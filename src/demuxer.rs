//! FLV demuxer.
//!
//! Two-pass initialisation:
//!
//! 1. Parse the 9-byte file header. Skip the first `PreviousTagSize`
//!    (always zero in spec-conformant files).
//! 2. Walk tags until we have discovered at least one audio and one
//!    video stream (or we hit the end of the file). For each new
//!    media kind, synthesise a `StreamInfo` from the first tag's
//!    header. Script tags are consumed into `metadata` along the way.
//! 3. Return to the start-of-tags position. `next_packet` re-walks
//!    the stream producing one `Packet` per audio/video tag.
//!
//! For AAC / H.264 tags the "packet type" / "configuration record"
//! prefix byte (and, for H.264, the 3-byte CompositionTime) is
//! stripped from the packet body and routed separately:
//!
//! * Packet-type 0 (config) lands in `extradata` on the stream's
//!   `CodecParameters` and is also surfaced as a `header`-flagged
//!   packet to match behaviours callers expect (MP4 demuxer / MKV
//!   demuxer pattern).
//! * Packet-type 1 (data) → normal decoder input.
//! * Packet-type 2 (end of sequence) → skipped.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Error, Packet, Result, SampleFormat, StreamInfo,
    TimeBase,
};
use oxideav_core::{Demuxer, ReadSeek};

use crate::amf0::{parse_amf0_value, AmfValue};
use crate::header::FlvHeader;
use crate::tag::{
    audio_codec_id_str, video_codec_id_str, AudioTagHeader, TagHeader, TagType, VideoTagHeader,
    AUDIO_CODEC_AAC, VIDEO_CODEC_H264, VIDEO_CODEC_VP6A,
};

/// Parsed `keyframes` toc from the `onMetaData` script tag — the
/// `filepositions[]` / `times[]` arrays most FLV encoders write near
/// the start of the file. Parallel arrays of equal length; entries
/// are sorted by `times` ascending. When present, seeks are O(log n)
/// bisects on `times`.
#[derive(Clone, Debug, Default)]
struct KeyframeIndex {
    /// Absolute byte offsets of each video keyframe tag (the TagType
    /// byte, *not* the preceding PreviousTagSize prefix).
    file_positions: Vec<u64>,
    /// Wall-clock times in **seconds** of each keyframe (`f64` as
    /// stored in AMF0). Same length as `file_positions`.
    times_seconds: Vec<f64>,
}

const STREAM_AUDIO: u32 = 0;
const STREAM_VIDEO: u32 = 1;

/// Open factory used by the container registry.
pub fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let _hdr = FlvHeader::read(&mut *input)?;
    // The four bytes immediately after the header are the first
    // `PreviousTagSize` — per spec always 0x00000000.
    let _ = read_u32_be(&mut *input)?;
    let first_tag_pos = input.stream_position()?;

    // --- Pass 1: discovery ---------------------------------------------------
    let mut streams_by_type: [Option<StreamInfo>; 2] = [None, None];
    let mut metadata: Vec<(String, String)> = Vec::new();
    let mut duration_micros: Option<i64> = None;
    let mut keyframe_index: Option<KeyframeIndex> = None;
    // Scan up to a reasonable cap — we only need one audio + one video tag
    // plus the script tag. Keep a hard limit so pathological files can't
    // force us to pre-read the whole input here.
    let mut tags_scanned: u32 = 0;
    const MAX_DISCOVERY_TAGS: u32 = 256;
    while tags_scanned < MAX_DISCOVERY_TAGS {
        if streams_by_type[0].is_some() && streams_by_type[1].is_some() {
            break;
        }
        let pos = input.stream_position()?;
        let header = match TagHeader::read(&mut *input) {
            Ok(h) => h,
            Err(Error::Eof) => break,
            Err(e) => return Err(e),
        };
        let kind = match header.kind {
            Some(k) => k,
            None => {
                // Unknown tag type — skip the body + trailing size.
                skip_bytes(&mut *input, header.data_size as u64 + 4)?;
                tags_scanned += 1;
                continue;
            }
        };
        // Read the full payload.
        let body = read_body(&mut *input, header.data_size)?;
        // Trailing PreviousTagSize (u32 BE).
        let _ = read_u32_be(&mut *input)?;

        match kind {
            TagType::ScriptData => {
                parse_script_body(
                    &body,
                    &mut metadata,
                    &mut duration_micros,
                    &mut keyframe_index,
                );
            }
            TagType::Audio => {
                if streams_by_type[STREAM_AUDIO as usize].is_none() && !body.is_empty() {
                    let info = build_audio_stream(STREAM_AUDIO, &body)?;
                    streams_by_type[STREAM_AUDIO as usize] = Some(info);
                }
            }
            TagType::Video => {
                if streams_by_type[STREAM_VIDEO as usize].is_none() && !body.is_empty() {
                    let info = build_video_stream(STREAM_VIDEO, &body, &metadata)?;
                    streams_by_type[STREAM_VIDEO as usize] = Some(info);
                }
            }
        }
        let _ = pos;
        tags_scanned += 1;
    }

    // Preserve discovery order. Audio is stream 0 when present, video 1.
    // If one of them is missing we renumber so there's no gap.
    let mut streams: Vec<StreamInfo> = Vec::new();
    let mut audio_stream_index: Option<u32> = None;
    let mut video_stream_index: Option<u32> = None;
    if let Some(mut s) = streams_by_type[0].take() {
        s.index = streams.len() as u32;
        audio_stream_index = Some(s.index);
        streams.push(s);
    }
    if let Some(mut s) = streams_by_type[1].take() {
        s.index = streams.len() as u32;
        video_stream_index = Some(s.index);
        streams.push(s);
    }
    if streams.is_empty() {
        return Err(Error::invalid("FLV: no audio or video tags discovered"));
    }

    // --- Rewind for packet emission -----------------------------------------
    input.seek(SeekFrom::Start(first_tag_pos))?;

    Ok(Box::new(FlvDemuxer {
        input,
        streams,
        metadata,
        duration_micros,
        audio_stream_index,
        video_stream_index,
        // Pending header-flagged "config" packet for AVC / AAC — queued so
        // we surface exactly one config packet before the first data packet
        // for each of those codecs.
        pending_packet: None,
        first_tag_pos,
        keyframe_index,
    }))
}

/// Public [`Demuxer`] type, exported so the integration tests can
/// name it. Intentionally opaque — construction is via [`open`].
pub struct FlvDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    duration_micros: Option<i64>,
    audio_stream_index: Option<u32>,
    video_stream_index: Option<u32>,
    pending_packet: Option<Packet>,
    /// Byte offset of the first FLV tag (immediately after the file
    /// header + leading PreviousTagSize). Used as the "rewind to
    /// start" landing point for `seek_to(0)` and as the lower bound
    /// for scan-forward seeks.
    first_tag_pos: u64,
    /// Cached `keyframes` table parsed out of `onMetaData`. `None` when
    /// no script tag was present, when the tag did not carry a
    /// `keyframes` object, or when the arrays were mismatched /
    /// truncated. Callers fall back to scan-forward.
    keyframe_index: Option<KeyframeIndex>,
}

impl std::fmt::Debug for FlvDemuxer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlvDemuxer")
            .field("streams", &self.streams.len())
            .field("duration_micros", &self.duration_micros)
            .field("audio_stream_index", &self.audio_stream_index)
            .field("video_stream_index", &self.video_stream_index)
            .field(
                "keyframe_index_entries",
                &self.keyframe_index.as_ref().map(|i| i.file_positions.len()),
            )
            .finish()
    }
}

impl Demuxer for FlvDemuxer {
    fn format_name(&self) -> &str {
        "flv"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.pending_packet.take() {
            return Ok(p);
        }
        loop {
            let header = match TagHeader::read(&mut *self.input) {
                Ok(h) => h,
                Err(Error::Eof) => return Err(Error::Eof),
                Err(e) => return Err(e),
            };
            let body = read_body(&mut *self.input, header.data_size)?;
            // Trailing PreviousTagSize.
            let _ = read_u32_be(&mut *self.input)?;

            match header.kind {
                Some(TagType::ScriptData) | None => continue,
                Some(TagType::Audio) => {
                    let idx = match self.audio_stream_index {
                        Some(i) => i,
                        None => continue,
                    };
                    if let Some((pkt, pending)) =
                        build_audio_packet(&self.streams[idx as usize], &header, &body)
                    {
                        if let Some(p) = pending {
                            self.pending_packet = Some(pkt);
                            return Ok(p);
                        }
                        return Ok(pkt);
                    }
                }
                Some(TagType::Video) => {
                    let idx = match self.video_stream_index {
                        Some(i) => i,
                        None => continue,
                    };
                    if let Some((pkt, pending)) =
                        build_video_packet(&self.streams[idx as usize], &header, &body)
                    {
                        if let Some(p) = pending {
                            self.pending_packet = Some(pkt);
                            return Ok(p);
                        }
                        return Ok(pkt);
                    }
                }
            }
        }
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        self.duration_micros
    }

    /// Seek to the nearest video keyframe at or before `pts` (in the
    /// stream's time base — always 1/1000 s for FLV).
    ///
    /// Two paths:
    ///
    /// 1. **Toc path** — when `onMetaData.keyframes` carries parallel
    ///    `filepositions[]` / `times[]` arrays, binary-search `times`
    ///    for the largest entry ≤ target_seconds and jump to the
    ///    corresponding byte offset. O(log n).
    /// 2. **Scan path** — rewind to the start of the tag stream and
    ///    walk tags forward, returning the first video keyframe tag
    ///    whose timestamp is ≥ target_pts. (We can't reliably scan
    ///    *backwards* in a flat tag stream without an index, so for
    ///    target_pts > 0 we land "at or after" rather than "at or
    ///    before". `seek_to(0)` always lands at the start.)
    ///
    /// Audio-only streams have no notion of "keyframe" (every audio
    /// packet is independently decodable) — for the audio stream the
    /// scan path stops at the first audio tag ≥ target_pts.
    ///
    /// `pts` is clamped at zero; values past the end of the file land
    /// at the last keyframe (toc path) or fall through to EOF on the
    /// next `next_packet()` call (scan path).
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        // Validate the requested stream index.
        if (stream_index as usize) >= self.streams.len() {
            return Err(Error::invalid(format!(
                "FLV: stream index {stream_index} out of range (have {} stream(s))",
                self.streams.len()
            )));
        }
        let target_pts_ms = pts.max(0);

        // Reset transient state so callers don't see a stale pending
        // packet from before the seek.
        self.pending_packet = None;

        // Toc path — only meaningful when video is present (the toc
        // describes video keyframes).
        if let (Some(idx), Some(_video_stream_index)) =
            (self.keyframe_index.clone(), self.video_stream_index)
        {
            // Defensive: empty / mismatched arrays should have been
            // rejected at parse time, but guard anyway.
            if !idx.times_seconds.is_empty() && idx.times_seconds.len() == idx.file_positions.len()
            {
                let target_seconds = (target_pts_ms as f64) / 1000.0;
                let pos = match idx.times_seconds.binary_search_by(|t| {
                    t.partial_cmp(&target_seconds)
                        .unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    Ok(i) => i,
                    // bisect-left: `i` is the first entry strictly
                    // greater than target. Step back to the entry at
                    // or before target (clamping to 0).
                    Err(i) => i.saturating_sub(1),
                };
                let dest = idx.file_positions[pos];
                // `dest` is the absolute byte offset of the keyframe
                // tag's TagType byte. Seek there so `next_packet` can
                // re-read the tag header from that point.
                self.input.seek(SeekFrom::Start(dest))?;
                let landed_ms = (idx.times_seconds[pos] * 1000.0).round() as i64;
                return Ok(landed_ms);
            }
        }

        // Scan path — walk tags from the start looking for a video
        // keyframe ≥ target_pts (or, for audio-only files, any audio
        // tag ≥ target_pts).
        self.input.seek(SeekFrom::Start(self.first_tag_pos))?;
        // Are we seeking on the audio or video stream?
        let seeking_video = match self.video_stream_index {
            Some(i) => i == stream_index,
            None => false,
        };
        loop {
            let tag_pos = self.input.stream_position()?;
            let header = match TagHeader::read(&mut *self.input) {
                Ok(h) => h,
                Err(Error::Eof) => {
                    // Past EOF — land at the end. `next_packet` will
                    // surface EOF on the next call.
                    return Ok(target_pts_ms);
                }
                Err(e) => return Err(e),
            };
            let kind = match header.kind {
                Some(k) => k,
                None => {
                    skip_bytes(&mut *self.input, header.data_size as u64 + 4)?;
                    continue;
                }
            };
            let want_match = match kind {
                TagType::Video if seeking_video || self.video_stream_index.is_some() => {
                    // Peek the first byte of the body for the keyframe
                    // flag without consuming the rest of the tag.
                    if header.data_size == 0 {
                        false
                    } else {
                        let mut b = [0u8; 1];
                        self.input.read_exact(&mut b)?;
                        let vh = VideoTagHeader::parse(b[0]);
                        // Rewind past the byte we just peeked so the
                        // tag is fully unread when we either jump out
                        // or skip past it below.
                        self.input.seek(SeekFrom::Current(-1))?;
                        vh.is_keyframe() && (header.timestamp_ms as i64) >= target_pts_ms
                    }
                }
                TagType::Audio if !seeking_video && self.video_stream_index.is_none() => {
                    // Audio-only: every audio packet is a keyframe.
                    (header.timestamp_ms as i64) >= target_pts_ms
                }
                _ => false,
            };
            if want_match {
                // Rewind to the start of this tag so `next_packet`
                // re-reads its header on the next call.
                self.input.seek(SeekFrom::Start(tag_pos))?;
                return Ok(header.timestamp_ms as i64);
            }
            // Skip body + trailing PreviousTagSize and try the next.
            skip_bytes(&mut *self.input, header.data_size as u64 + 4)?;
        }
    }
}

fn build_audio_stream(index: u32, body: &[u8]) -> Result<StreamInfo> {
    if body.is_empty() {
        return Err(Error::invalid("FLV: empty audio tag"));
    }
    let ah = AudioTagHeader::parse(body[0]);
    let codec = CodecId::new(audio_codec_id_str(ah.codec_id));
    let mut params = CodecParameters::audio(codec);
    params.sample_rate = Some(ah.sample_rate_hz());
    params.channels = Some(ah.channels());
    params.sample_format = if ah.is_16bit {
        Some(SampleFormat::S16)
    } else {
        Some(SampleFormat::U8)
    };
    // AAC: byte 1 is AACPacketType. Type 0 = AudioSpecificConfig — the
    // decoder extradata. We copy it in here so consumers can find it
    // without requiring them to peek at the first packet.
    if ah.codec_id == AUDIO_CODEC_AAC && body.len() >= 2 && body[1] == 0x00 {
        params.extradata = body[2..].to_vec();
    }
    Ok(StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params,
    })
}

fn build_video_stream(
    index: u32,
    body: &[u8],
    metadata: &[(String, String)],
) -> Result<StreamInfo> {
    if body.is_empty() {
        return Err(Error::invalid("FLV: empty video tag"));
    }
    let vh = VideoTagHeader::parse(body[0]);
    let codec = CodecId::new(video_codec_id_str(vh.codec_id));
    let mut params = CodecParameters::video(codec);
    // Pull width/height from metadata if the script tag supplied them —
    // otherwise leave as None and let the decoder figure it out from the
    // keyframe header.
    if let Some(w) = metadata_lookup_u32(metadata, "width") {
        params.width = Some(w);
    }
    if let Some(h) = metadata_lookup_u32(metadata, "height") {
        params.height = Some(h);
    }
    // H.264: body[1] = AVCPacketType, body[2..5] = CompositionTime offset.
    // Type 0 = AVCDecoderConfigurationRecord. Route it to extradata.
    if vh.codec_id == VIDEO_CODEC_H264 && body.len() >= 5 && body[1] == 0x00 {
        params.extradata = body[5..].to_vec();
    } else if vh.codec_id == VIDEO_CODEC_VP6A && body.len() >= 2 {
        // VP6-with-alpha header has an extra byte giving the byte offset
        // to the alpha data — surface it in extradata for the decoder.
        params.extradata = vec![body[1]];
    }
    Ok(StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params,
    })
}

/// Build a packet from an Audio tag. Returns a `(data_pkt, maybe_header_pkt)`
/// tuple — when the tag is an AAC config record, the header-flagged packet
/// is yielded first (via the demuxer's `pending_packet` slot) and the
/// corresponding data packet is empty, so we return `None` in that slot to
/// mean "emit the header packet now and continue the loop after".
fn build_audio_packet(
    stream: &StreamInfo,
    hdr: &TagHeader,
    body: &[u8],
) -> Option<(Packet, Option<Packet>)> {
    if body.is_empty() {
        return None;
    }
    let ah = AudioTagHeader::parse(body[0]);
    let payload_offset: usize;
    let is_header;
    if ah.codec_id == AUDIO_CODEC_AAC {
        if body.len() < 2 {
            return None;
        }
        let packet_type = body[1];
        match packet_type {
            0x00 => {
                // Config record — emit as header packet only.
                payload_offset = 2;
                is_header = true;
            }
            0x01 => {
                payload_offset = 2;
                is_header = false;
            }
            _ => return None,
        }
    } else {
        payload_offset = 1;
        is_header = false;
    }
    if body.len() < payload_offset {
        return None;
    }
    let data = body[payload_offset..].to_vec();
    let mut pkt = Packet::new(stream.index, stream.time_base, data);
    pkt.pts = Some(hdr.timestamp_ms as i64);
    pkt.dts = Some(hdr.timestamp_ms as i64);
    pkt.flags.keyframe = true; // audio: every packet is independently decodable
    pkt.flags.header = is_header;
    Some((pkt, None))
}

/// Build a packet from a Video tag. Same shape as `build_audio_packet`.
fn build_video_packet(
    stream: &StreamInfo,
    hdr: &TagHeader,
    body: &[u8],
) -> Option<(Packet, Option<Packet>)> {
    if body.is_empty() {
        return None;
    }
    let vh = VideoTagHeader::parse(body[0]);
    let mut payload_offset: usize = 1;
    let mut pts = hdr.timestamp_ms as i64;
    let dts = hdr.timestamp_ms as i64;
    let mut is_header = false;

    if vh.codec_id == VIDEO_CODEC_H264 {
        if body.len() < 5 {
            return None;
        }
        let packet_type = body[1];
        // CompositionTime — i24 BE, signed. Adjusts pts relative to dts.
        let comp = {
            let raw = ((body[2] as u32) << 16) | ((body[3] as u32) << 8) | (body[4] as u32);
            // Sign-extend 24-bit.
            let sext = if raw & 0x0080_0000 != 0 {
                raw | 0xFF00_0000
            } else {
                raw
            };
            sext as i32 as i64
        };
        match packet_type {
            0x00 => {
                is_header = true;
                payload_offset = 5;
            }
            0x01 => {
                payload_offset = 5;
                pts = dts + comp;
            }
            0x02 => {
                // End-of-sequence marker — skip.
                return None;
            }
            _ => return None,
        }
    }

    if body.len() < payload_offset {
        return None;
    }
    let data = body[payload_offset..].to_vec();
    let mut pkt = Packet::new(stream.index, stream.time_base, data);
    pkt.pts = Some(pts);
    pkt.dts = Some(dts);
    pkt.flags.keyframe = vh.is_keyframe();
    pkt.flags.header = is_header;
    Some((pkt, None))
}

fn parse_script_body(
    body: &[u8],
    metadata: &mut Vec<(String, String)>,
    duration_micros: &mut Option<i64>,
    keyframe_index: &mut Option<KeyframeIndex>,
) {
    // Script tag body = (AMF0 name, AMF0 value). We only care when the
    // name is "onMetaData" — other variants are rarer and safely ignored.
    let (name, p) = match parse_amf0_value(body, 0) {
        Ok(v) => v,
        Err(_) => return,
    };
    let name_str = match name.as_str() {
        Some(s) => s.to_string(),
        None => return,
    };
    if name_str != "onMetaData" {
        return;
    }
    let (value, _np) = match parse_amf0_value(body, p) {
        Ok(v) => v,
        Err(_) => return,
    };
    // Walk top-level object/ecma-array keys and pull them into the
    // metadata bag. Numbers become their displayed form, strings pass
    // through, the `keyframes` object is harvested for the seek toc.
    let entries = match &value {
        AmfValue::Object(v) | AmfValue::EcmaArray(v) => v.as_slice(),
        _ => return,
    };
    for (k, v) in entries {
        match v {
            AmfValue::Number(n) => {
                // Duration is in seconds — convert to microseconds and
                // store both a string form (for metadata) and the numeric
                // form (for `duration_micros`).
                if k == "duration" && duration_micros.is_none() && *n >= 0.0 {
                    let micros = (*n * 1_000_000.0).round();
                    if micros.is_finite() && micros >= 0.0 && micros < i64::MAX as f64 {
                        *duration_micros = Some(micros as i64);
                    }
                }
                metadata.push((k.clone(), format_number(*n)));
            }
            AmfValue::Boolean(b) => metadata.push((k.clone(), b.to_string())),
            AmfValue::String(s) => metadata.push((k.clone(), s.clone())),
            AmfValue::Object(_) | AmfValue::EcmaArray(_)
                if k == "keyframes" && keyframe_index.is_none() =>
            {
                *keyframe_index = parse_keyframes_object(v);
            }
            _ => {}
        }
    }
}

/// Pull the parallel `filepositions[]` / `times[]` arrays out of a
/// `keyframes` AMF0 object. Returns `None` when either array is
/// missing, of the wrong type, or has a mismatched length — callers
/// fall back to scan-forward seeking on `None`.
///
/// Per spec the entries are sorted ascending by `times`. We don't
/// re-sort: a producer that emits an out-of-order toc is malformed.
fn parse_keyframes_object(v: &AmfValue) -> Option<KeyframeIndex> {
    let entries: &[(String, AmfValue)] = match v {
        AmfValue::Object(b) | AmfValue::EcmaArray(b) => b,
        _ => return None,
    };
    let mut file_positions: Option<Vec<u64>> = None;
    let mut times_seconds: Option<Vec<f64>> = None;
    for (k, val) in entries {
        let arr = match val {
            AmfValue::StrictArray(a) => a,
            _ => continue,
        };
        let mut nums: Vec<f64> = Vec::with_capacity(arr.len());
        for av in arr {
            if let AmfValue::Number(n) = av {
                if !n.is_finite() {
                    return None;
                }
                nums.push(*n);
            } else {
                return None;
            }
        }
        match k.as_str() {
            "filepositions" => {
                let mut out = Vec::with_capacity(nums.len());
                for n in nums {
                    if n < 0.0 || n > u64::MAX as f64 {
                        return None;
                    }
                    out.push(n as u64);
                }
                file_positions = Some(out);
            }
            "times" => times_seconds = Some(nums),
            _ => {}
        }
    }
    let fp = file_positions?;
    let ts = times_seconds?;
    if fp.is_empty() || fp.len() != ts.len() {
        return None;
    }
    // Reject NaNs / non-monotonic timing — those would break the
    // bisect later. We allow exact-duplicate timestamps (rare but
    // legal when two keyframes share a millisecond).
    for w in ts.windows(2) {
        if w[1] < w[0] {
            return None;
        }
    }
    Some(KeyframeIndex {
        file_positions: fp,
        times_seconds: ts,
    })
}

fn metadata_lookup_u32(metadata: &[(String, String)], key: &str) -> Option<u32> {
    for (k, v) in metadata {
        if k == key {
            if let Ok(n) = v.parse::<f64>() {
                if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 {
                    return Some(n as u32);
                }
            }
        }
    }
    None
}

fn format_number(n: f64) -> String {
    // Integral-valued floats become "42"; everything else uses the
    // default rust formatter. Avoids "42.0" noise in common cases.
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e18 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn read_u32_be<R: Read + ?Sized>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn read_body<R: Read + ?Sized>(r: &mut R, size: u32) -> Result<Vec<u8>> {
    let mut out = vec![0u8; size as usize];
    r.read_exact(&mut out)?;
    Ok(out)
}

fn skip_bytes<R: Seek + ?Sized>(r: &mut R, n: u64) -> Result<()> {
    r.seek(SeekFrom::Current(n as i64))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::NullCodecResolver;
    use std::io::Cursor;

    fn make_flv(tags: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        // header — audio+video flags, data offset 9.
        out.extend_from_slice(b"FLV\x01");
        out.push(0x05);
        out.extend_from_slice(&9u32.to_be_bytes());
        // first PreviousTagSize
        out.extend_from_slice(&0u32.to_be_bytes());
        for t in tags {
            let len = t.len() as u32;
            out.extend_from_slice(t);
            // PreviousTagSize = len + 11
            out.extend_from_slice(&(len + 11).to_be_bytes());
        }
        out
    }

    fn make_tag(kind: u8, timestamp_ms: u32, body: &[u8]) -> Vec<u8> {
        let mut t = Vec::with_capacity(11 + body.len());
        t.push(kind);
        // data size (u24 BE)
        let sz = body.len() as u32;
        t.push((sz >> 16) as u8);
        t.push((sz >> 8) as u8);
        t.push(sz as u8);
        // timestamp low 24 + extended
        t.push((timestamp_ms >> 16) as u8);
        t.push((timestamp_ms >> 8) as u8);
        t.push(timestamp_ms as u8);
        t.push((timestamp_ms >> 24) as u8);
        // stream id (always 0)
        t.extend_from_slice(&[0, 0, 0]);
        t.extend_from_slice(body);
        t
    }

    #[test]
    fn open_empty_fails() {
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(Vec::new()));
        assert!(open(input, &NullCodecResolver).is_err());
    }

    #[test]
    fn open_synth_flv_mp3_vp6f_roundtrip() {
        let mp3_body = {
            // codec id 2 (MP3), 22 kHz (idx 2), 16-bit, stereo
            let flags = (2 << 4) | (2 << 2) | 0x02 | 0x01;
            let mut v = vec![flags as u8];
            v.extend_from_slice(&[0xAA, 0xBB, 0xCC]); // dummy audio bytes
            v
        };
        let vp6_body = {
            // frame_type=1 (key), codec_id=4 (vp6f)
            let flags = (1 << 4) | 4;
            let mut v = vec![flags as u8];
            // VP6 adjustment byte, then dummy coded bytes.
            v.extend_from_slice(&[0x00, 0xDE, 0xAD, 0xBE, 0xEF]);
            v
        };

        let audio_tag = make_tag(0x08, 0, &mp3_body);
        let video_tag = make_tag(0x09, 33, &vp6_body);
        let flv = make_flv(&[&audio_tag, &video_tag]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.format_name(), "flv");
        assert_eq!(dmx.streams().len(), 2);
        // Stream 0 should be mp3 audio; stream 1 should be vp6f video.
        assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mp3");
        assert_eq!(dmx.streams()[1].params.codec_id.as_str(), "vp6f");

        let p1 = dmx.next_packet().unwrap();
        assert_eq!(p1.stream_index, 0);
        assert_eq!(p1.pts, Some(0));
        assert_eq!(p1.data, vec![0xAA, 0xBB, 0xCC]);

        let p2 = dmx.next_packet().unwrap();
        assert_eq!(p2.stream_index, 1);
        assert_eq!(p2.pts, Some(33));
        assert!(p2.flags.keyframe);
        assert_eq!(p2.data, vec![0x00, 0xDE, 0xAD, 0xBE, 0xEF]);

        assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    }

    #[test]
    fn script_metadata_surfaces() {
        // Build an onMetaData tag with duration=1.5 and width=640.
        let mut body = Vec::new();
        // "onMetaData"
        body.push(0x02);
        body.extend_from_slice(&(10u16).to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        // object {"duration": 1.5, "width": 640}
        body.push(0x08);
        body.extend_from_slice(&0u32.to_be_bytes()); // ecma array count hint
        body.extend_from_slice(&(8u16).to_be_bytes());
        body.extend_from_slice(b"duration");
        body.push(0x00);
        body.extend_from_slice(&1.5_f64.to_be_bytes());
        body.extend_from_slice(&(5u16).to_be_bytes());
        body.extend_from_slice(b"width");
        body.push(0x00);
        body.extend_from_slice(&640.0_f64.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x09]);

        let script_tag = make_tag(0x12, 0, &body);
        // Follow it with one video tag so discovery succeeds.
        let vp6_body = {
            let flags = (1 << 4) | 4;
            vec![flags as u8, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.duration_micros(), Some(1_500_000));
        let md = dmx.metadata();
        assert!(md.iter().any(|(k, v)| k == "duration" && v == "1.5"));
        assert!(md.iter().any(|(k, v)| k == "width" && v == "640"));
        // The video stream should have picked up width=640 from metadata.
        assert_eq!(dmx.streams()[0].params.width, Some(640));
    }
}
