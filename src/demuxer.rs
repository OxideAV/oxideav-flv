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
    CodecId, CodecParameters, CodecResolver, Error, Packet, Rational, Result, SampleFormat,
    StreamInfo, TimeBase,
};
use oxideav_core::{Demuxer, ReadSeek};

use crate::amf0::{parse_amf0_value, AmfValue};
use crate::ex_video::{fourcc_codec_id_str, ExFrameType, ExPacketType, ExVideoTagHeader};
use crate::header::FlvHeader;
use crate::tag::{
    audio_codec_id_str, video_codec_id_str, AudioTagHeader, EncryptedTagPreamble, TagHeader,
    TagType, VideoTagHeader, AUDIO_CODEC_AAC, VIDEO_CODEC_H264, VIDEO_CODEC_VP6A,
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

/// Encryption headline parsed from a `|AdditionalHeader` script tag
/// (spec Annex F.2). When present, every media tag whose `Filter` bit
/// is set carries an `EncryptionTagHeader` + `FilterParams` body.
/// We don't decrypt — this struct exists so the demuxer can surface
/// "the file is DRM-protected, here's the metadata" to callers via the
/// `metadata()` bag and the `is_encrypted()` helper.
#[derive(Clone, Debug, Default)]
struct EncryptionHeadline {
    /// `1` (FMRMS v1) or `2` (Flash Access v2) per F.2.2.
    version: Option<f64>,
    /// `"Standard"` per F.2.2.
    method: Option<String>,
    /// Per F.2.3 — for `"Standard"`, always `"AES-CBC"`.
    algorithm: Option<String>,
    /// Per F.2.4 — for AES-CBC, always 16 bytes (128 bits).
    key_length: Option<f64>,
    /// Per F.2.5 — `"APS"` (v1) or `"FlashAccessv2"` (v2).
    key_subtype: Option<String>,
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
    let mut encryption: Option<EncryptionHeadline> = None;
    let mut xmp_metadata: Option<String> = None;
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
                    &mut encryption,
                    &mut xmp_metadata,
                );
            }
            TagType::Audio => {
                if streams_by_type[STREAM_AUDIO as usize].is_none() && !body.is_empty() {
                    let info = build_audio_stream(STREAM_AUDIO, &body, &metadata)?;
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

    // Flatten the encryption + XMP discoveries into the metadata bag so
    // callers see "the file is DRM-protected, with such-and-such
    // algorithm" through the standard `metadata()` accessor.
    let is_encrypted = encryption.is_some();
    if let Some(e) = &encryption {
        metadata.push(("encryption".into(), "true".into()));
        if let Some(v) = e.version {
            metadata.push(("encryption.version".into(), format_number(v)));
        }
        if let Some(m) = &e.method {
            metadata.push(("encryption.method".into(), m.clone()));
        }
        if let Some(a) = &e.algorithm {
            metadata.push(("encryption.algorithm".into(), a.clone()));
        }
        if let Some(k) = e.key_length {
            metadata.push(("encryption.key_length".into(), format_number(k)));
        }
        if let Some(s) = &e.key_subtype {
            metadata.push(("encryption.key_subtype".into(), s.clone()));
        }
    }
    if let Some(xmp) = &xmp_metadata {
        metadata.push(("xmp".into(), xmp.clone()));
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
        is_encrypted,
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
    /// `true` when discovery saw a `|AdditionalHeader` script tag (FLV
    /// encryption, spec Annex F.2). The demuxer still emits packets for
    /// every tag — encrypted media bodies surface with
    /// `flags.discard = true` so decoders skip past them rather than
    /// trying to interpret ciphertext as a frame.
    is_encrypted: bool,
}

impl FlvDemuxer {
    /// `true` when this FLV file declared an [`Annex F`](Adobe FLV
    /// Spec v10.1) encryption header. Always `false` for plain FLVs.
    pub fn is_encrypted(&self) -> bool {
        self.is_encrypted
    }
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
            .field("is_encrypted", &self.is_encrypted)
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

            // Filtered (encrypted) tags carry an EncryptionTagHeader +
            // FilterParams preamble in front of the codec body. Per F.1
            // "non-compliant players will ignore tags with the filter
            // flag set". We're a compliant *demuxer* but we don't carry
            // a DRM client, so the most we can do is route the encrypted
            // bytes downstream with `flags.discard = true` so decoders
            // don't try to parse ciphertext.
            if header.filter {
                if let Some(pkt) = build_encrypted_packet(
                    &header,
                    &body,
                    self.audio_stream_index,
                    self.video_stream_index,
                    &self.streams,
                )? {
                    return Ok(pkt);
                }
                continue;
            }

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
                    // flag without consuming the rest of the tag. The
                    // Ex header (IsExHeader=1) reuses bits 6..4 as
                    // FrameType so the same is_keyframe test works for
                    // both legacy and enhanced video tags.
                    if header.data_size == 0 {
                        false
                    } else {
                        let mut b = [0u8; 1];
                        self.input.read_exact(&mut b)?;
                        let is_kf = if (b[0] & crate::ex_video::EX_HEADER_FLAG) != 0 {
                            let ft = ExFrameType::from_u8((b[0] >> 4) & 0x07);
                            ft.is_keyframe()
                        } else {
                            VideoTagHeader::parse(b[0]).is_keyframe()
                        };
                        // Rewind past the byte we just peeked so the
                        // tag is fully unread when we either jump out
                        // or skip past it below.
                        self.input.seek(SeekFrom::Current(-1))?;
                        is_kf && (header.timestamp_ms as i64) >= target_pts_ms
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

fn build_audio_stream(
    index: u32,
    body: &[u8],
    metadata: &[(String, String)],
) -> Result<StreamInfo> {
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
    // Override sample-rate / channels with the more authoritative
    // onMetaData values when present (Annex E.5 / Annex B.1).
    // AudioTagHeader's SoundRate field can only encode 5.5/11/22/44 kHz,
    // and AAC always reports 44 kHz regardless of the true rate carried
    // in AudioSpecificConfig — `audiosamplerate` is the producer's
    // declared truth.
    if let Some(sr) = metadata_lookup_u32(metadata, "audiosamplerate") {
        if sr > 0 {
            params.sample_rate = Some(sr);
        }
    }
    if let Some(b) = metadata_lookup_bool(metadata, "stereo") {
        params.channels = Some(if b { 2 } else { 1 });
    }
    // `audiodatarate` is in kilobits-per-second per E.5.
    if let Some(kbps) = metadata_lookup_f64(metadata, "audiodatarate") {
        if kbps.is_finite() && kbps >= 0.0 {
            params.bit_rate = Some((kbps * 1000.0) as u64);
        }
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
    // Enhanced RTMP (E-FLV) ExVideoTagHeader path: the high bit
    // (IsExHeader) of the leading byte means the codec is identified
    // by FourCC rather than the legacy 4-bit CodecID. Build the stream
    // descriptor off the FourCC codec id; the config record (when
    // present) becomes extradata for the decoder.
    if let Some(ex) = ExVideoTagHeader::parse(body)? {
        let codec = CodecId::new(fourcc_codec_id_str(ex.fourcc));
        let mut params = CodecParameters::video(codec);
        if let Some(w) = metadata_lookup_u32(metadata, "width") {
            params.width = Some(w);
        }
        if let Some(h) = metadata_lookup_u32(metadata, "height") {
            params.height = Some(h);
        }
        if let Some(fps) = metadata_lookup_f64(metadata, "videoframerate")
            .or_else(|| metadata_lookup_f64(metadata, "framerate"))
        {
            if fps.is_finite() && fps > 0.0 {
                params.frame_rate = Some(f64_to_rational(fps));
            }
        }
        if let Some(kbps) = metadata_lookup_f64(metadata, "videodatarate") {
            if kbps.is_finite() && kbps >= 0.0 {
                params.bit_rate = Some((kbps * 1000.0) as u64);
            }
        }
        // SequenceStart's body is the codec's decoder-configuration
        // record — route to extradata so consumers find it without
        // peeking at the first packet (consistent with the legacy AVC
        // path below).
        if matches!(ex.packet_type, ExPacketType::SequenceStart) && body.len() > ex.bytes_consumed {
            params.extradata = body[ex.bytes_consumed..].to_vec();
        }
        return Ok(StreamInfo {
            index,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params,
        });
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
    // `videoframerate` (preferred per Annex B.1) or `framerate` per E.5.
    // Convert to a Rational using a small denominator scale so producers
    // emitting 29.97 / 23.976 round-trip cleanly.
    if let Some(fps) = metadata_lookup_f64(metadata, "videoframerate")
        .or_else(|| metadata_lookup_f64(metadata, "framerate"))
    {
        if fps.is_finite() && fps > 0.0 {
            params.frame_rate = Some(f64_to_rational(fps));
        }
    }
    // `videodatarate` is in kilobits-per-second per E.5.
    if let Some(kbps) = metadata_lookup_f64(metadata, "videodatarate") {
        if kbps.is_finite() && kbps >= 0.0 {
            params.bit_rate = Some((kbps * 1000.0) as u64);
        }
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

/// Map an AMF Number frame-rate (29.97, 23.976, 60, ...) to a
/// best-effort Rational. We snap a small set of NTSC-family rates to
/// their canonical 1001-denominator forms; everything else uses a
/// 1000-denominator approximation.
fn f64_to_rational(fps: f64) -> Rational {
    // Producers typically emit 23.976023..., 29.97002997..., 59.94005994... .
    // Snap with a 0.005 tolerance.
    const NTSC: &[(u32, u32, f64)] = &[
        (24000, 1001, 23.976_023),
        (30000, 1001, 29.970_03),
        (60000, 1001, 59.940_06),
        (48000, 1001, 47.952_05),
        (120_000, 1001, 119.880_12),
    ];
    for (num, den, target) in NTSC {
        if (fps - target).abs() < 0.005 {
            return Rational::new(*num as i64, *den as i64).reduced();
        }
    }
    // Otherwise round to milli-fps and reduce trailing-zero noise.
    let scaled = (fps * 1000.0).round() as i64;
    Rational::new(scaled.max(1), 1000).reduced()
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
    // Enhanced RTMP / E-FLV ExVideoTagHeader path. The high bit
    // (IsExHeader) signals the leading byte carries (FrameType,
    // PacketType) and is followed by a 4-byte FourCC. Map onto the
    // existing Packet semantics:
    //   PacketType=0 SequenceStart  → header packet (config record)
    //   PacketType=1 CodedFrames    → data packet (+ CTO for AVC/HEVC/VVC)
    //   PacketType=2 SequenceEnd    → skip (no decoder input)
    //   PacketType=3 CodedFramesX   → data packet, implicit CTO=0
    //   PacketType=4 Metadata       → header+discard (HDR colorInfo AMF)
    //   PacketType=5 MPEG2TS start  → header packet
    //   PacketType=6 Multitrack     → header+discard (we don't track-split)
    //   PacketType=7 ModEx          → header+discard
    //   FrameType=Command           → discardable command (legacy parity)
    if let Ok(Some(ex)) = ExVideoTagHeader::parse(body) {
        if matches!(ex.packet_type, ExPacketType::SequenceEnd) {
            return None;
        }
        let payload_start = ex.bytes_consumed.min(body.len());
        let data = body[payload_start..].to_vec();
        let dts = hdr.timestamp_ms as i64;
        let pts = dts + ex.composition_time_offset_ms.unwrap_or(0) as i64;
        let mut pkt = Packet::new(stream.index, stream.time_base, data);
        pkt.pts = Some(pts);
        pkt.dts = Some(dts);
        // Keyframe-ness comes from the FrameType bits in the Ex header,
        // matching legacy semantics (Key + GeneratedKey both random-
        // access points).
        pkt.flags.keyframe = ex.frame_type.is_keyframe();
        match ex.packet_type {
            ExPacketType::SequenceStart | ExPacketType::Mpeg2TsSequenceStart => {
                pkt.flags.header = true;
            }
            ExPacketType::Metadata
            | ExPacketType::Multitrack
            | ExPacketType::ModEx
            | ExPacketType::Reserved(_) => {
                pkt.flags.header = true;
                pkt.flags.discard = true;
            }
            ExPacketType::CodedFrames | ExPacketType::CodedFramesX => {
                // Data packet — keyframe flag set above.
            }
            ExPacketType::SequenceEnd => unreachable!(),
        }
        // Command-frame sentinel: video info / command frames still
        // mean "this isn't a video frame, route accordingly". The Ex
        // header keeps FrameType=Command (5) for backward compat.
        if matches!(ex.frame_type, ExFrameType::Command) {
            pkt.flags.header = true;
            pkt.flags.discard = true;
            pkt.flags.keyframe = false;
        }
        return Some((pkt, None));
    }
    let vh = VideoTagHeader::parse(body[0]);

    // FrameType == 5 (video info / command) — body[1] is a UI8
    // command, not codec data. Surface it as a packet with `discard`
    // set so decoders skip it, but `header` set so callers can react
    // to the client-side-seeking boundary if they want.
    if vh.is_video_info() {
        if body.len() < 2 {
            return None;
        }
        let cmd_byte = body[1];
        let mut pkt = Packet::new(stream.index, stream.time_base, vec![cmd_byte]);
        pkt.pts = Some(hdr.timestamp_ms as i64);
        pkt.dts = Some(hdr.timestamp_ms as i64);
        pkt.flags.header = true;
        pkt.flags.discard = true;
        // FrameType=5 is not a random-access point.
        pkt.flags.keyframe = false;
        return Some((pkt, None));
    }

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

/// Build a packet from a filtered (encrypted) tag. The encrypted
/// payload follows the `EncryptionTagHeader` + `FilterParams` preamble
/// (spec F.3.1 / F.3.2). We don't decrypt — the body is forwarded with
/// `flags.discard = true` so downstream consumers know not to feed it to
/// a decoder. Returns `Ok(None)` if the tag is for a stream type we
/// don't have (e.g. an encrypted video tag in an audio-only file).
fn build_encrypted_packet(
    hdr: &TagHeader,
    body: &[u8],
    audio_idx: Option<u32>,
    video_idx: Option<u32>,
    streams: &[StreamInfo],
) -> Result<Option<Packet>> {
    let target_idx = match hdr.kind {
        Some(TagType::Audio) => audio_idx,
        Some(TagType::Video) => video_idx,
        // Script tags are required to be in-clear (F.1 §2.c / F.4) so
        // an encrypted script tag is itself a spec violation — but we
        // forward it via the audio stream slot if present, otherwise
        // skip. This is conservative; the more common interpretation
        // would be to drop, but losing a "weird" tag silently makes
        // forensic debugging harder.
        Some(TagType::ScriptData) | None => return Ok(None),
    };
    let Some(idx) = target_idx else {
        return Ok(None);
    };
    let preamble = EncryptedTagPreamble::parse(body)?;
    if preamble.bytes_consumed > body.len() {
        return Ok(None);
    }
    let cipher = body[preamble.bytes_consumed..].to_vec();
    let stream = &streams[idx as usize];
    let mut pkt = Packet::new(stream.index, stream.time_base, cipher);
    pkt.pts = Some(hdr.timestamp_ms as i64);
    pkt.dts = Some(hdr.timestamp_ms as i64);
    pkt.flags.discard = true;
    // For selective encryption with EncryptedAU=0 the bytes are
    // actually plaintext — but we still set discard, because the
    // caller doesn't have a way to differentiate per-packet inside the
    // current Packet API and the safer default is "don't feed unknown
    // bytes to a decoder". A future DRM-aware caller can clear the
    // flag after running its filter chain.
    let _ = preamble.is_encrypted;
    Ok(Some(pkt))
}

fn parse_script_body(
    body: &[u8],
    metadata: &mut Vec<(String, String)>,
    duration_micros: &mut Option<i64>,
    keyframe_index: &mut Option<KeyframeIndex>,
    encryption: &mut Option<EncryptionHeadline>,
    xmp_metadata: &mut Option<String>,
) {
    // Script tag body = (AMF0 name, AMF0 value). FLV producers in the
    // wild emit several spec-defined names; we recognise:
    //
    //   onMetaData         (E.5)  — duration / width / height / codec
    //                                 ids / `keyframes` toc / ...
    //   onXMPData          (E.6)  — XMP livexml string
    //   onCuePoint                 — embedded cue points (Annex A);
    //                                 preserved verbatim in metadata as
    //                                 cuepoint.<key> entries
    //   |AdditionalHeader  (F.2.1) — FLV encryption headline
    let (name, p) = match parse_amf0_value(body, 0) {
        Ok(v) => v,
        Err(_) => return,
    };
    let name_str = match name.as_str() {
        Some(s) => s.to_string(),
        None => return,
    };
    let (value, _np) = match parse_amf0_value(body, p) {
        Ok(v) => v,
        Err(_) => return,
    };
    match name_str.as_str() {
        "onMetaData" => parse_on_metadata(&value, metadata, duration_micros, keyframe_index),
        "onXMPData" => {
            // Per E.6 the payload is an object with a single "liveXML"
            // string-or-longstring property.
            if let Some(s) = xmp_liveXML(&value) {
                *xmp_metadata = Some(s);
            }
        }
        "onCuePoint" => {
            // Annex A: per-cue parameter pack. Surface the fields under
            // a `cuepoint.<n>.<key>` prefix so callers see them without
            // pulling in a full AMF cue model.
            let n = metadata
                .iter()
                .filter(|(k, _)| k.starts_with("cuepoint."))
                .map(|(k, _)| {
                    // extract integer index between the two dots
                    k.split('.')
                        .nth(1)
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0)
                })
                .max()
                .map(|m| m + 1)
                .unwrap_or(0);
            flatten_amf_value(&value, &format!("cuepoint.{n}"), metadata);
        }
        "|AdditionalHeader" => {
            *encryption = parse_additional_header(&value);
        }
        _ => {
            // Unknown name — preserve the name itself so callers can see
            // the file had a non-spec data tag.
            metadata.push(("scriptdata.name".into(), name_str));
        }
    }
}

fn parse_on_metadata(
    value: &AmfValue,
    metadata: &mut Vec<(String, String)>,
    duration_micros: &mut Option<i64>,
    keyframe_index: &mut Option<KeyframeIndex>,
) {
    // Walk top-level object/ecma-array keys and pull them into the
    // metadata bag. Numbers become their displayed form, strings pass
    // through, the `keyframes` object is harvested for the seek toc.
    let entries = match value {
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

#[allow(non_snake_case)] // mirrors the spec property name verbatim
fn xmp_liveXML(value: &AmfValue) -> Option<String> {
    // Per E.6 the XMP object has exactly one property: liveXML.
    let entries: &[(String, AmfValue)] = match value {
        AmfValue::Object(b) | AmfValue::EcmaArray(b) => b,
        // Some producers nest the string directly.
        AmfValue::String(s) => return Some(s.clone()),
        _ => return None,
    };
    for (k, v) in entries {
        if k == "liveXML" {
            if let AmfValue::String(s) = v {
                return Some(s.clone());
            }
        }
    }
    None
}

fn parse_additional_header(value: &AmfValue) -> Option<EncryptionHeadline> {
    // AdditionalHeader = {Encryption: {Version, Method, Flags, Params,
    //                                  ...}}
    let enc_obj = value.get("Encryption")?;
    let mut h = EncryptionHeadline::default();
    if let Some(AmfValue::Number(v)) = enc_obj.get("Version") {
        h.version = Some(*v);
    }
    if let Some(AmfValue::String(s)) = enc_obj.get("Method") {
        h.method = Some(s.clone());
    }
    let params = enc_obj.get("Params");
    if let Some(p) = params {
        if let Some(AmfValue::String(s)) = p.get("EncryptionAlgorithm") {
            h.algorithm = Some(s.clone());
        }
        if let Some(AmfValue::Number(n)) =
            p.get("EncryptionParams").and_then(|q| q.get("KeyLength"))
        {
            h.key_length = Some(*n);
        }
        if let Some(AmfValue::String(s)) = p.get("KeyInfo").and_then(|q| q.get("SubType")) {
            h.key_subtype = Some(s.clone());
        }
    }
    Some(h)
}

/// Flatten an AMF value into a prefix-keyed list of (key, string-form)
/// pairs and append them to `out`. Used for `onCuePoint` payloads where
/// the spec doesn't fix the property layout.
fn flatten_amf_value(value: &AmfValue, prefix: &str, out: &mut Vec<(String, String)>) {
    match value {
        AmfValue::Number(n) => out.push((prefix.into(), format_number(*n))),
        AmfValue::Boolean(b) => out.push((prefix.into(), b.to_string())),
        AmfValue::String(s) => out.push((prefix.into(), s.clone())),
        AmfValue::Null => out.push((prefix.into(), "null".into())),
        AmfValue::Undefined => out.push((prefix.into(), "undefined".into())),
        AmfValue::Reference(idx) => out.push((prefix.into(), format!("ref:{idx}"))),
        AmfValue::Date { time_ms, tz } => {
            out.push((prefix.into(), format!("date:{time_ms}tz:{tz}")));
        }
        AmfValue::Object(b) | AmfValue::EcmaArray(b) => {
            for (k, v) in b {
                flatten_amf_value(v, &format!("{prefix}.{k}"), out);
            }
        }
        AmfValue::StrictArray(items) => {
            for (i, v) in items.iter().enumerate() {
                flatten_amf_value(v, &format!("{prefix}[{i}]"), out);
            }
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

fn metadata_lookup_f64(metadata: &[(String, String)], key: &str) -> Option<f64> {
    for (k, v) in metadata {
        if k == key {
            if let Ok(n) = v.parse::<f64>() {
                if n.is_finite() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn metadata_lookup_bool(metadata: &[(String, String)], key: &str) -> Option<bool> {
    for (k, v) in metadata {
        if k == key {
            return match v.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
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

    /// Build an `onMetaData` script-tag body with a single property of
    /// the given (string-)key and (number-)value. Used by the unit
    /// tests below to keep the byte-level construction terse.
    fn on_metadata_with_property(key: &str, value: f64) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onMetaData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        // ECMA array { key: value }
        body.push(0x08);
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(key.len() as u16).to_be_bytes());
        body.extend_from_slice(key.as_bytes());
        body.push(0x00);
        body.extend_from_slice(&value.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        body
    }

    #[test]
    fn videodatarate_lifts_into_bit_rate() {
        // Producer-declared video bit-rate = 768 kbps → 768_000 bit_rate.
        let script_tag = make_tag(0x12, 0, &on_metadata_with_property("videodatarate", 768.0));
        let vp6_body = {
            let flags = (1 << 4) | 4;
            vec![flags as u8, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.streams()[0].params.bit_rate, Some(768_000));
    }

    #[test]
    fn videoframerate_snaps_to_ntsc_fraction() {
        let script_tag = make_tag(0x12, 0, &on_metadata_with_property("videoframerate", 29.97));
        let vp6_body = {
            let flags = (1 << 4) | 4;
            vec![flags as u8, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        let fr = dmx.streams()[0].params.frame_rate.expect("frame_rate set");
        assert_eq!((fr.num, fr.den), (30_000, 1001));
    }

    #[test]
    fn audiosamplerate_overrides_soundrate_field() {
        // Build an onMetaData with audiosamplerate=48000.
        let script_tag = make_tag(
            0x12,
            0,
            &on_metadata_with_property("audiosamplerate", 48_000.0),
        );
        // AAC tag — would otherwise report 44100. Body: codec=10, rate
        // idx=3 (44k), 16-bit, stereo + AAC packet type 1 + raw byte.
        let flags = (10 << 4) | (3 << 2) | 0x02 | 0x01;
        let audio_body = vec![flags as u8, 0x01, 0xAA];
        let audio_tag = make_tag(0x08, 0, &audio_body);
        let flv = make_flv(&[&script_tag, &audio_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.streams()[0].params.sample_rate, Some(48_000));
    }

    #[test]
    fn video_info_command_packet_is_discardable_header() {
        // FrameType=5, codec=2; command byte = 0 (start of seek seq).
        let cmd_body = vec![(5u8 << 4) | 2, 0x00];
        let video_tag_cmd = make_tag(0x09, 100, &cmd_body);
        // Follow with a keyframe so discovery still succeeds.
        let kf_body = vec![(1u8 << 4) | 2, 0xAA, 0xBB];
        let video_tag_kf = make_tag(0x09, 0, &kf_body);
        let flv = make_flv(&[&video_tag_kf, &video_tag_cmd]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let p1 = dmx.next_packet().unwrap();
        assert!(p1.flags.keyframe, "first packet should be the keyframe");
        let p2 = dmx.next_packet().unwrap();
        assert!(p2.flags.header, "video-info command must be header-flagged");
        assert!(
            p2.flags.discard,
            "video-info command must be discard-flagged (not codec data)"
        );
        assert_eq!(p2.data, vec![0x00]);
        assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    }

    #[test]
    fn additional_header_marks_demuxer_encrypted() {
        // |AdditionalHeader carrying {Encryption: {Version: 2, Method: "Standard",
        //   Params: {EncryptionAlgorithm: "AES-CBC",
        //            EncryptionParams: {KeyLength: 16},
        //            KeyInfo: {SubType: "FlashAccessv2"}}}}
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&17u16.to_be_bytes());
        body.extend_from_slice(b"|AdditionalHeader");
        // Outer object
        body.push(0x03);
        body.extend_from_slice(&10u16.to_be_bytes());
        body.extend_from_slice(b"Encryption");
        // Inner object
        body.push(0x03);
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(b"Version");
        body.push(0x00);
        body.extend_from_slice(&2.0f64.to_be_bytes());
        body.extend_from_slice(&6u16.to_be_bytes());
        body.extend_from_slice(b"Method");
        body.push(0x02);
        body.extend_from_slice(&8u16.to_be_bytes());
        body.extend_from_slice(b"Standard");
        // Params object
        body.extend_from_slice(&6u16.to_be_bytes());
        body.extend_from_slice(b"Params");
        body.push(0x03);
        body.extend_from_slice(&19u16.to_be_bytes());
        body.extend_from_slice(b"EncryptionAlgorithm");
        body.push(0x02);
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(b"AES-CBC");
        body.extend_from_slice(&16u16.to_be_bytes());
        body.extend_from_slice(b"EncryptionParams");
        body.push(0x03);
        body.extend_from_slice(&9u16.to_be_bytes());
        body.extend_from_slice(b"KeyLength");
        body.push(0x00);
        body.extend_from_slice(&16.0f64.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(b"KeyInfo");
        body.push(0x03);
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(b"SubType");
        body.push(0x02);
        body.extend_from_slice(&13u16.to_be_bytes());
        body.extend_from_slice(b"FlashAccessv2");
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        body.extend_from_slice(&[0x00, 0x00, 0x09]);

        let script_tag = make_tag(0x12, 0, &body);
        // Plain video tag to satisfy stream discovery.
        let vp6_body = vec![(1u8 << 4) | 4, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        // Downcast back to FlvDemuxer so we can call the inherent
        // `is_encrypted` method.
        let md = dmx.metadata();
        assert!(md.iter().any(|(k, v)| k == "encryption" && v == "true"));
        assert!(md
            .iter()
            .any(|(k, v)| k == "encryption.algorithm" && v == "AES-CBC"));
        assert!(md
            .iter()
            .any(|(k, v)| k == "encryption.key_subtype" && v == "FlashAccessv2"));
        assert!(md
            .iter()
            .any(|(k, v)| k == "encryption.version" && v == "2"));
    }

    #[test]
    fn filtered_audio_tag_emits_discardable_packet() {
        // Discovery first sees a normal MP3 audio tag so the audio
        // stream is built; then a filter-flagged tag with an Encryption
        // preamble + 16-byte ciphertext body.
        let mp3_body = {
            let flags = (2u8 << 4) | (2 << 2) | 0x02 | 0x01;
            vec![flags, 0xAA, 0xBB, 0xCC]
        };
        let plain_audio = make_tag(0x08, 0, &mp3_body);

        // Filter-flagged audio tag: tag-type byte 0x08 | 0x20 = 0x28.
        let mut filtered_body = vec![1u8];
        filtered_body.extend_from_slice(b"Encryption\0");
        filtered_body.push(0);
        filtered_body.push(0);
        filtered_body.push(16); // params length = 16 bytes IV
        filtered_body.extend_from_slice(&[0u8; 16]); // IV
        filtered_body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // ciphertext
        let filtered_audio = make_tag(0x28, 100, &filtered_body);
        let flv = make_flv(&[&plain_audio, &filtered_audio]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        // First packet — the plain MP3.
        let p1 = dmx.next_packet().unwrap();
        assert!(!p1.flags.discard, "plain audio should not be discardable");
        // Second packet — the encrypted body.
        let p2 = dmx.next_packet().unwrap();
        assert!(
            p2.flags.discard,
            "filtered audio body must surface with discard=true"
        );
        assert_eq!(p2.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn on_xmp_data_surfaces_xmp_metadata_string() {
        let live_xml = "<x:xmpmeta>hi</x:xmpmeta>";
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onXMPData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onXMPData");
        body.push(0x03); // Object
        body.extend_from_slice(&("liveXML".len() as u16).to_be_bytes());
        body.extend_from_slice(b"liveXML");
        body.push(0x02);
        body.extend_from_slice(&(live_xml.len() as u16).to_be_bytes());
        body.extend_from_slice(live_xml.as_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![(1u8 << 4) | 4, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert!(dmx
            .metadata()
            .iter()
            .any(|(k, v)| k == "xmp" && v == live_xml));
    }

    #[test]
    fn ex_video_av1_sequence_start_landed_in_extradata() {
        // E-RTMP enhanced video tag: IsExHeader=1, FrameType=key (1),
        // PacketType=SequenceStart (0) → 0x90; FourCc = "av01"; config
        // record body = 6 bytes.
        let mut tag_body = vec![0x90];
        tag_body.extend_from_slice(b"av01");
        let config = [0x81, 0x05, 0x0C, 0x00, 0x0A, 0x0B]; // dummy AV1CC
        tag_body.extend_from_slice(&config);
        let video_tag = make_tag(0x09, 0, &tag_body);
        // Follow with a CodedFrames packet so discovery sees one media
        // tag and we can exercise the data-packet path.
        let mut frame_body = vec![0xA1]; // inter + CodedFrames
        frame_body.extend_from_slice(b"av01");
        frame_body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let frame_tag = make_tag(0x09, 33, &frame_body);
        let flv = make_flv(&[&video_tag, &frame_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        // Codec id should be "av1", not the legacy CodecID lookup.
        assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "av1");
        assert_eq!(dmx.streams()[0].params.extradata, config.to_vec());
        // First packet is the SequenceStart config — emitted as a
        // header packet, not as decoder data.
        let p1 = dmx.next_packet().unwrap();
        assert!(p1.flags.header, "SequenceStart must be header-flagged");
        assert!(p1.flags.keyframe);
        assert_eq!(p1.data, config.to_vec());
        // Second packet is the CodedFrames body.
        let p2 = dmx.next_packet().unwrap();
        assert!(!p2.flags.header);
        assert_eq!(p2.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(p2.pts, Some(33));
        assert_eq!(p2.dts, Some(33));
    }

    #[test]
    fn ex_video_hevc_coded_frames_applies_composition_time_offset() {
        // Discovery tag (SequenceStart) is required to set the codec
        // id; then a CodedFrames tag at dts=100 with CTO=+50 → pts=150.
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"hvc1");
        seq_body.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]); // dummy hvcC
        let seq_tag = make_tag(0x09, 0, &seq_body);
        let mut frame_body = vec![0xA1]; // inter + CodedFrames
        frame_body.extend_from_slice(b"hvc1");
        // SI24 CTO = +50.
        frame_body.extend_from_slice(&[0x00, 0x00, 0x32]);
        frame_body.extend_from_slice(&[0xCA, 0xFE]);
        let frame_tag = make_tag(0x09, 100, &frame_body);
        let flv = make_flv(&[&seq_tag, &frame_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "h265");
        // Skip the header packet from SequenceStart.
        let _h = dmx.next_packet().unwrap();
        let p = dmx.next_packet().unwrap();
        assert_eq!(p.dts, Some(100));
        assert_eq!(p.pts, Some(150));
        assert_eq!(p.data, vec![0xCA, 0xFE]);
    }

    #[test]
    fn ex_video_metadata_packet_is_discardable_header() {
        // HDR colorInfo metadata frame — header + discard so a
        // standard video decoder skips it.
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"hvc1");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x09, 0, &seq_body);
        let mut meta_body = vec![0x94]; // FrameType=key + PacketType=Metadata
        meta_body.extend_from_slice(b"hvc1");
        meta_body.extend_from_slice(b"amf-color-info-blob");
        let meta_tag = make_tag(0x09, 0, &meta_body);
        let flv = make_flv(&[&seq_tag, &meta_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _seq = dmx.next_packet().unwrap();
        let m = dmx.next_packet().unwrap();
        assert!(m.flags.header);
        assert!(m.flags.discard);
        assert_eq!(m.data, b"amf-color-info-blob".to_vec());
    }

    #[test]
    fn ex_video_sequence_end_yields_no_packet() {
        // SequenceStart for discovery, then SequenceEnd which should be
        // swallowed by the demuxer (no decoder input), then EOF.
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"av01");
        seq_body.push(0x42);
        let seq_tag = make_tag(0x09, 0, &seq_body);
        let mut end_body = vec![0x92]; // SequenceEnd
        end_body.extend_from_slice(b"av01");
        let end_tag = make_tag(0x09, 200, &end_body);
        let flv = make_flv(&[&seq_tag, &end_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _seq = dmx.next_packet().unwrap();
        // SequenceEnd should be silently dropped → next call is EOF.
        assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    }

    #[test]
    fn ex_video_command_frame_is_discardable() {
        // FrameType=Command (5) + PacketType=SequenceStart (any) — the
        // command-frame sentinel keeps the legacy "discardable header"
        // semantics from FrameType=5 routing.
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"av01");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x09, 0, &seq_body);
        let mut cmd_body = vec![0xD0]; // 0x80 | (5<<4) | 0
        cmd_body.extend_from_slice(b"av01");
        cmd_body.push(0x00); // command byte
        let cmd_tag = make_tag(0x09, 100, &cmd_body);
        let flv = make_flv(&[&seq_tag, &cmd_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _ = dmx.next_packet().unwrap();
        let cmd = dmx.next_packet().unwrap();
        assert!(cmd.flags.header);
        assert!(cmd.flags.discard);
        assert!(!cmd.flags.keyframe);
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
