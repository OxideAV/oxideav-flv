//! FLV demuxer.
//!
//! * **Open** parses the 9-byte file header and then a bounded prefix
//!   of the tag stream (at most 256 tags / 4 MiB) — enough to see the
//!   first audio tag, the first video tag, their configuration records
//!   and the script tags in front of them. It never measures the input
//!   and never scans for a duration or an index.
//! * **`next_packet`** walks the tag stream through a sliding window
//!   (the crate-private `reader` module); each media tag becomes one `Packet` whose
//!   payload is copied out of the window exactly once.
//! * **`seek_to`** lands on the last sync point at or before the
//!   target, through the `onMetaData` keyframe table when there is a
//!   valid one and through an index built during reading otherwise.
//!
//! For AAC / H.264 tags the "packet type" / "configuration record"
//! prefix byte (and, for H.264, the 3-byte CompositionTime) is
//! stripped from the packet body and routed separately:
//!
//! * Packet-type 0 (config) lands in `extradata` on the stream's
//!   `CodecParameters` and is also surfaced as a `header`-flagged
//!   packet — including when it arrives late or changes mid-stream.
//! * Packet-type 1 (data) → normal decoder input.
//! * Packet-type 2 (end of sequence) → skipped.
//!
//! Timestamps are carried from the 24 + 8-bit wire field onto a 64-bit
//! timeline (the private `Clock` type); stream-health observations are counted in
//! [`FlvDemuxStats`].

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Error, Packet, Rational, Result, SampleFormat,
    StreamInfo, TimeBase,
};
use oxideav_core::{Demuxer, ReadSeek};

use crate::amf0::{AmfValue, parse_amf0_value};
use crate::amf3::{Amf3Array, Amf3Object, Amf3Value};
use crate::ex_audio::{ExAudioPacketType, ExAudioTagHeader, fourcc_audio_codec_id_str};
use crate::ex_video::{
    ExFrameType, ExPacketType, ExVideoTagHeader, FOURCC_AVC1, FOURCC_HVC1, FOURCC_VVC1,
    fourcc_codec_id_str,
};
use crate::header::FlvHeader;
use crate::mod_ex::ModExEntry;
use crate::multichannel::{MultichannelConfig, mask_channel_labels};
use crate::multitrack::{MultitrackPacketTrack, split_tracks};
use crate::reader::TagReader;
use crate::tag::{
    AUDIO_CODEC_AAC, AUDIO_CODEC_NELLYMOSER_8K_MONO, AUDIO_CODEC_NELLYMOSER_16K_MONO,
    AUDIO_CODEC_SPEEX, AudioTagHeader, EncryptedTagPreamble, TagHeader, TagType, VIDEO_CODEC_H264,
    VIDEO_CODEC_VP6A, VideoTagHeader, audio_codec_id_str, video_codec_id_str,
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

/// Discovery stops after this many tags…
const MAX_DISCOVERY_TAGS: u32 = 256;
/// …or after this many bytes once at least one stream is known
/// (sixteen times as many while none is), whichever comes first. `open`
/// never reads further than this, so opening a multi-gigabyte or
/// still-growing file costs the same as opening a short one.
const MAX_DISCOVERY_BYTES: u64 = 4 << 20;
/// Most `(key, value)` entries the metadata bag will hold. Script tags
/// keep arriving for as long as a live stream runs; past this many
/// entries further ones are not recorded.
const MAX_METADATA_ENTRIES: usize = 65_536;
/// `PreviousTagSize` (UI32) + the 11-byte tag header: the fixed prefix
/// of one record of the file body (spec §E.3).
const RECORD_PREFIX: usize = 4 + 11;

/// Open factory used by the container registry. Returns the demuxer as a
/// trait object; use [`open_concrete`] when you need the concrete
/// [`FlvDemuxer`] accessors ([`FlvDemuxer::is_encrypted`] /
/// [`FlvDemuxer::last_multitrack_tracks`] / [`FlvDemuxer::stats`]).
pub fn open(input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(open_concrete(input, codecs)?))
}

/// Open factory returning the concrete [`FlvDemuxer`] so callers can
/// reach the FLV-specific accessors that the [`Demuxer`] trait does not
/// expose — [`FlvDemuxer::is_encrypted`] (Adobe FLV §Annex F encryption
/// flag), [`FlvDemuxer::last_multitrack_tracks`] (every track of the
/// last Enhanced-RTMP multitrack tag) and [`FlvDemuxer::stats`]
/// (stream-health counters). [`open`] wraps this in a
/// `Box<dyn Demuxer>` for the registry path.
///
/// Opening is bounded: it reads the file header and then at most
/// 256 tags / 4 MiB looking for the first audio tag, the first video tag
/// and their configuration records. It never asks the input for its
/// length and never scans for a duration or a seek table, so a live
/// capture or a file that is still being written opens as fast as a
/// finished one. [`Demuxer::duration_micros`] is `Some` only when
/// `onMetaData` declares a duration.
pub fn open_concrete(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<FlvDemuxer> {
    let hdr = FlvHeader::read(&mut *input)?;
    // The body starts with `PreviousTagSize0`; every record after that
    // is "PreviousTagSize + tag", so the cursor always rests on a
    // back-pointer.
    let first_record_pos = input.stream_position()?;
    let mut reader = TagReader::new(input, first_record_pos);

    // --- Discovery -----------------------------------------------------------
    let mut found: [Option<StreamInfo>; 2] = [None, None];
    let mut keys: [Option<u32>; 2] = [None, None];
    let mut awaiting_config = [false, false];
    let mut metadata: Vec<(String, String)> = Vec::new();
    let mut duration_micros: Option<i64> = None;
    let mut keyframe_index: Option<KeyframeIndex> = None;
    let mut encryption: Option<EncryptionHeadline> = None;
    let mut xmp_metadata: Option<String> = None;
    let mut embedded_images: Vec<EmbeddedImage> = Vec::new();
    let mut clock = Clock::default();
    let mut tags_scanned: u32 = 0;
    // Whether discovery saw the whole file (as far as it exists now).
    let mut reached_end = false;
    while tags_scanned < MAX_DISCOVERY_TAGS {
        let have_any = found[0].is_some() || found[1].is_some();
        if found[0].is_some() && found[1].is_some() && !awaiting_config[0] && !awaiting_config[1] {
            break;
        }
        let budget = if have_any {
            MAX_DISCOVERY_BYTES
        } else {
            16 * MAX_DISCOVERY_BYTES
        };
        if reader.position() - first_record_pos > budget {
            break;
        }
        let Some(header) = peek_record(&mut reader)? else {
            reached_end = true;
            break;
        };
        let total = RECORD_PREFIX + header.data_size as usize;
        let ts = clock.stamp(header.kind, header.timestamp_ms);
        let body = &reader.window()[RECORD_PREFIX..total];
        match header.kind {
            Some(TagType::ScriptData) => {
                parse_script_body(
                    body,
                    &mut metadata,
                    &mut duration_micros,
                    &mut keyframe_index,
                    &mut encryption,
                    &mut xmp_metadata,
                    &mut embedded_images,
                );
            }
            Some(TagType::Audio) if !body.is_empty() => {
                let slot = STREAM_AUDIO as usize;
                if found[slot].is_none() {
                    if let Ok(mut info) = build_audio_stream(STREAM_AUDIO, body, &metadata) {
                        info.start_time = Some(ts);
                        awaiting_config[slot] =
                            audio_wants_config(body) && info.params.extradata.is_empty();
                        keys[slot] = audio_codec_key(body);
                        found[slot] = Some(info);
                    }
                } else if awaiting_config[slot]
                    && let Some(cfg) = audio_config_record(body)
                    && let Some(info) = found[slot].as_mut()
                {
                    info.params.extradata = cfg;
                    awaiting_config[slot] = false;
                }
            }
            Some(TagType::Video) if !body.is_empty() => {
                let slot = STREAM_VIDEO as usize;
                if found[slot].is_none() {
                    if video_codec_key(body).is_some()
                        && let Ok(mut info) = build_video_stream(STREAM_VIDEO, body, &metadata)
                    {
                        info.start_time = Some(ts);
                        awaiting_config[slot] =
                            video_wants_config(body) && info.params.extradata.is_empty();
                        keys[slot] = video_codec_key(body);
                        found[slot] = Some(info);
                    }
                } else if awaiting_config[slot]
                    && let Some(cfg) = video_config_record(body)
                    && let Some(info) = found[slot].as_mut()
                {
                    info.params.extradata = cfg;
                    awaiting_config[slot] = false;
                }
            }
            _ => {}
        }
        reader.consume(total);
        tags_scanned += 1;
    }
    let script_parsed_to = reader.position();

    // A stream the file header and `onMetaData` both declare, but whose
    // first tag lies beyond the discovery window (late video after a
    // long audio-only lead-in, typically), still gets its slot now: the
    // stream list is fixed for the demuxer's lifetime. Its parameters
    // come from `onMetaData` and are replaced by the tag's own facts as
    // soon as the first tag arrives.
    if found[0].is_none()
        && hdr.has_audio
        && let Some(info) = placeholder_audio_stream(&metadata)
    {
        found[0] = Some(info);
    }
    if found[1].is_none()
        && hdr.has_video
        && let Some(info) = placeholder_video_stream(&metadata)
    {
        found[1] = Some(info);
    }

    // Where `onMetaData` and the tags disagree the tags win; say so.
    report_conflicts(&hdr, &found, keys, reached_end, &mut metadata);

    // Audio is stream 0 when present, video follows; a missing kind
    // leaves no gap.
    let mut streams: Vec<StreamInfo> = Vec::new();
    let mut audio_stream_index: Option<u32> = None;
    let mut video_stream_index: Option<u32> = None;
    let [audio, video] = found;
    if let Some(mut s) = audio {
        s.index = streams.len() as u32;
        audio_stream_index = Some(s.index);
        streams.push(s);
    }
    if let Some(mut s) = video {
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

    // Back to the first record (inside the window for ordinary files, so
    // no I/O).
    reader.seek(first_record_pos)?;

    let active = vec![true; streams.len()];
    Ok(FlvDemuxer {
        reader,
        streams,
        metadata,
        duration_micros,
        audio_stream_index,
        video_stream_index,
        codec_keys: keys,
        active,
        first_record_pos,
        script_parsed_to,
        clock: Clock::default(),
        max_ts: 0,
        expected_prev_size: Some(0),
        index: LazyIndex::new(first_record_pos),
        keyframe_index,
        is_encrypted,
        last_multitrack: None,
        last_mod_ex: None,
        embedded_images,
        stats: FlvDemuxStats::default(),
        duration_is_scanned: false,
    })
}

/// Bring the record at the reader's cursor fully into the window and
/// return its tag header, or `None` when the input ends before the
/// record is complete (clean end of file, a file cut mid-tag, or a file
/// that has not been written that far yet). The cursor is left on the
/// record's `PreviousTagSize` either way; on `Some` the window holds
/// `RECORD_PREFIX + DataSize` bytes.
fn peek_record(reader: &mut TagReader) -> Result<Option<TagHeader>> {
    if !reader.fill(RECORD_PREFIX)? {
        return Ok(None);
    }
    let header = parse_tag_header(&reader.window()[4..RECORD_PREFIX]);
    if !reader.fill(RECORD_PREFIX + header.data_size as usize)? {
        return Ok(None);
    }
    Ok(Some(header))
}

/// Decode the 11 tag-header bytes (spec §E.4.1).
fn parse_tag_header(b: &[u8]) -> TagHeader {
    let u24 = |i: usize| ((b[i] as u32) << 16) | ((b[i + 1] as u32) << 8) | b[i + 2] as u32;
    TagHeader {
        tag_type_raw: b[0],
        kind: TagType::from_u8(b[0]),
        data_size: u24(1),
        timestamp_ms: ((b[7] as u32) << 24) | u24(4),
        stream_id: u24(8),
        filter: (b[0] & 0x20) != 0,
    }
}

fn is_media(kind: Option<TagType>) -> bool {
    matches!(kind, Some(TagType::Audio | TagType::Video))
}

/// Whether a video tag's frame-type bits say "key frame". `first` is
/// the first body byte.
fn is_key_typed(first: u8) -> bool {
    if (first & crate::ex_video::EX_HEADER_FLAG) != 0 {
        ExFrameType::from_u8((first >> 4) & 0x07).is_keyframe()
    } else {
        VideoTagHeader::parse(first).is_keyframe()
    }
}

/// Whether a tag is a point playback can start from: a coded video key
/// frame when the file has video, any audio tag otherwise. `head` is
/// the first two body bytes (fewer if the body is shorter).
///
/// Sequence headers and end-of-sequence markers carry the key-frame
/// type bits too but hold no picture, so the packet-type field is
/// consulted where the first two bytes expose it: legacy AVC
/// (`AVCPacketType` must be 1) and single-track Ex tags (`CodedFrames`
/// / `CodedFramesX`). ModEx- or multitrack-wrapped Ex tags keep the
/// frame-type answer.
fn is_sync_tag(kind: Option<TagType>, head: &[u8], has_video: bool) -> bool {
    match (kind, head.first()) {
        (Some(TagType::Video), Some(&b)) if has_video => {
            if !is_key_typed(b) {
                return false;
            }
            if (b & crate::ex_video::EX_HEADER_FLAG) != 0 {
                // PacketType: 1 CodedFrames, 3 CodedFramesX, 6
                // Multitrack, 7 ModEx.
                matches!(b & 0x0F, 1 | 3 | 6 | 7)
            } else if (b & 0x0F) == VIDEO_CODEC_H264 {
                head.get(1) == Some(&1)
            } else {
                true
            }
        }
        (Some(TagType::Audio), Some(_)) => !has_video,
        _ => false,
    }
}

/// Identity of the codec an audio tag announces: the legacy
/// `SoundFormat` nibble, or the FourCC of an Ex tag (tagged so the two
/// spaces cannot collide). `None` for an empty body.
fn audio_codec_key(body: &[u8]) -> Option<u32> {
    let b = *body.first()?;
    if (b >> 4) == crate::ex_audio::SOUND_FORMAT_EX_HEADER {
        match ExAudioTagHeader::parse(body) {
            Ok(Some(ex)) => Some(ex.fourcc.unwrap_or(u32::MAX)),
            _ => None,
        }
    } else {
        Some((b >> 4) as u32)
    }
}

/// Video counterpart of [`audio_codec_key`]. `None` for an empty body
/// and for command frames (FrameType 5), which name no codec.
fn video_codec_key(body: &[u8]) -> Option<u32> {
    let b = *body.first()?;
    if (b & crate::ex_video::EX_HEADER_FLAG) != 0 {
        match ExVideoTagHeader::parse(body) {
            Ok(Some(ex)) if !matches!(ex.frame_type, ExFrameType::Command) => {
                Some(ex.fourcc.unwrap_or(u32::MAX))
            }
            _ => None,
        }
    } else if VideoTagHeader::parse(b).is_video_info() {
        None
    } else {
        Some((b & 0x0F) as u32)
    }
}

/// Does this audio tag's codec ship a separate configuration record?
fn audio_wants_config(body: &[u8]) -> bool {
    match ExAudioTagHeader::parse(body) {
        Ok(Some(ex)) => matches!(
            ex.fourcc,
            Some(crate::ex_audio::FOURCC_OPUS)
                | Some(crate::ex_audio::FOURCC_FLAC)
                | Some(crate::ex_audio::FOURCC_AAC)
        ),
        Ok(None) => AudioTagHeader::parse(body[0]).codec_id == AUDIO_CODEC_AAC,
        Err(_) => false,
    }
}

/// Does this video tag's codec ship a separate configuration record?
fn video_wants_config(body: &[u8]) -> bool {
    match ExVideoTagHeader::parse(body) {
        Ok(Some(ex)) => ex.fourcc.is_some_and(|f| f != crate::ex_video::FOURCC_VP08),
        Ok(None) => VideoTagHeader::parse(body[0]).codec_id == VIDEO_CODEC_H264,
        Err(_) => false,
    }
}

/// The configuration record an audio tag carries, when it is a
/// sequence header (legacy AAC `AACPacketType == 0`, or an Ex
/// `SequenceStart`).
fn audio_config_record(body: &[u8]) -> Option<Vec<u8>> {
    match ExAudioTagHeader::parse(body) {
        Ok(Some(ex)) => {
            if ex.packet_type != ExAudioPacketType::SequenceStart || body.len() <= ex.bytes_consumed
            {
                return None;
            }
            let tail = &body[ex.bytes_consumed..];
            Some(match ex.multitrack {
                Some(mt) => default_track_payload(mt, ex.fourcc, tail),
                None => tail.to_vec(),
            })
        }
        Ok(None) => {
            let ah = AudioTagHeader::parse(*body.first()?);
            (ah.codec_id == AUDIO_CODEC_AAC && body.len() > 2 && body[1] == 0)
                .then(|| body[2..].to_vec())
        }
        Err(_) => None,
    }
}

/// The configuration record a video tag carries, when it is a sequence
/// header (legacy AVC `AVCPacketType == 0`, or an Ex `SequenceStart`).
fn video_config_record(body: &[u8]) -> Option<Vec<u8>> {
    match ExVideoTagHeader::parse(body) {
        Ok(Some(ex)) => {
            if !matches!(ex.packet_type, ExPacketType::SequenceStart)
                || body.len() <= ex.bytes_consumed
            {
                return None;
            }
            let tail = &body[ex.bytes_consumed..];
            Some(match ex.multitrack {
                Some(mt) => default_track_payload(mt, ex.fourcc, tail),
                None => tail.to_vec(),
            })
        }
        Ok(None) => {
            let vh = VideoTagHeader::parse(*body.first()?);
            (vh.codec_id == VIDEO_CODEC_H264 && body.len() > 5 && body[1] == 0)
                .then(|| body[5..].to_vec())
        }
        Err(_) => None,
    }
}

/// Stream slot for audio that `onMetaData` declares (`audiocodecid`) but
/// whose first tag has not been seen yet.
fn placeholder_audio_stream(metadata: &[(String, String)]) -> Option<StreamInfo> {
    let id = metadata_lookup_u32(metadata, "audiocodecid")?;
    let mut params = CodecParameters::audio(CodecId::new(crate::tag::audio_codec_id_str_u32(id)));
    apply_audio_metadata(&mut params, metadata);
    Some(StreamInfo {
        index: STREAM_AUDIO,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: None,
        params,
    })
}

/// Stream slot for video that `onMetaData` declares (`videocodecid`) but
/// whose first tag has not been seen yet.
fn placeholder_video_stream(metadata: &[(String, String)]) -> Option<StreamInfo> {
    let id = metadata_lookup_u32(metadata, "videocodecid")?;
    let mut params = CodecParameters::video(CodecId::new(crate::tag::video_codec_id_str_u32(id)));
    apply_video_metadata(&mut params, metadata);
    Some(StreamInfo {
        index: STREAM_VIDEO,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: None,
        params,
    })
}

/// Record every place where `onMetaData` (or the file header) and the
/// tags themselves disagree. The tags are what the stream list was
/// built from; each disagreement lands in the metadata bag under
/// `flv.conflict.<what>` as `"declared=<x> stream=<y>"` so a caller can
/// see that the declaration was overruled.
fn report_conflicts(
    hdr: &FlvHeader,
    found: &[Option<StreamInfo>; 2],
    keys: [Option<u32>; 2],
    reached_end: bool,
    metadata: &mut Vec<(String, String)>,
) {
    let mut notes: Vec<(String, String)> = Vec::new();
    // Only streams built from a real tag (`keys` set) can contradict a
    // declaration; a placeholder *is* the declaration.
    if let (Some(s), Some(_)) = (&found[0], keys[0]) {
        if let Some(id) = metadata_lookup_u32(metadata, "audiocodecid") {
            let declared = crate::tag::audio_codec_id_str_u32(id);
            if declared != s.params.codec_id.as_str() {
                notes.push((
                    "flv.conflict.audiocodecid".into(),
                    format!("declared={declared} stream={}", s.params.codec_id.as_str()),
                ));
            }
        }
        if keys[0].is_some_and(audio_header_is_authoritative) {
            if let (Some(d), Some(a)) = (
                metadata_lookup_u32(metadata, "audiosamplerate"),
                s.params.sample_rate,
            ) && !sound_rate_matches(d, a)
            {
                notes.push((
                    "flv.conflict.audiosamplerate".into(),
                    format!("declared={d} stream={a}"),
                ));
            }
            if let (Some(d), Some(a)) =
                (metadata_lookup_bool(metadata, "stereo"), s.params.channels)
                && d != (a == 2)
            {
                notes.push((
                    "flv.conflict.stereo".into(),
                    format!("declared={d} stream={}", a == 2),
                ));
            }
            if let (Some(d), Some(a)) = (
                metadata_audio_sample_format(metadata),
                s.params.sample_format,
            ) && d != a
            {
                notes.push((
                    "flv.conflict.audiosamplesize".into(),
                    format!("declared={d:?} stream={a:?}"),
                ));
            }
        }
    }
    if let (Some(s), Some(_)) = (&found[1], keys[1])
        && let Some(id) = metadata_lookup_u32(metadata, "videocodecid")
    {
        let declared = crate::tag::video_codec_id_str_u32(id);
        if declared != s.params.codec_id.as_str() {
            notes.push((
                "flv.conflict.videocodecid".into(),
                format!("declared={declared} stream={}", s.params.codec_id.as_str()),
            ));
        }
    }
    let kinds = |a: bool, v: bool| match (a, v) {
        (true, true) => "audio+video",
        (true, false) => "audio",
        (false, true) => "video",
        (false, false) => "none",
    };
    let seen = (keys[0].is_some(), keys[1].is_some());
    // Tags of a kind the header denies are a contradiction as soon as
    // one is seen; a kind the header promises is only missing once the
    // whole file has been looked at.
    let undeclared = (seen.0 && !hdr.has_audio) || (seen.1 && !hdr.has_video);
    let absent = (hdr.has_audio && !seen.0) || (hdr.has_video && !seen.1);
    if undeclared || (absent && reached_end) {
        notes.push((
            "flv.conflict.header_flags".into(),
            format!(
                "declared={} stream={}",
                kinds(hdr.has_audio, hdr.has_video),
                kinds(seen.0, seen.1)
            ),
        ));
    }
    metadata.extend(notes);
}

/// `SoundRate` names its rates loosely ("5.5 kHz", "11 kHz", …); treat a
/// declared rate within 1 % of the tag-header rate as agreement.
fn sound_rate_matches(declared: u32, actual: u32) -> bool {
    (declared as i64 - actual as i64).abs() * 100 <= actual as i64
}

/// For the formats whose samples are laid out by the `SoundRate` /
/// `SoundSize` / `SoundType` bits themselves (linear PCM, ADPCM and
/// generic-rate Nellymoser) the tag header is the fact and `onMetaData`
/// cannot overrule it. (The G.711 formats are listed as reserved in
/// §E.4.2.1 and commonly run at 8 kHz, which the bits cannot say, so
/// for them the declaration is kept.) `key` is an
/// [`audio_codec_key`].
fn audio_header_is_authoritative(key: u32) -> bool {
    key <= 0x0F
        && matches!(
            key as u8,
            crate::tag::AUDIO_CODEC_LPCM_NATIVE
                | crate::tag::AUDIO_CODEC_ADPCM
                | crate::tag::AUDIO_CODEC_LPCM_LE
                | crate::tag::AUDIO_CODEC_NELLYMOSER
        )
}

/// Extends the wire timestamp onto a 64-bit timeline.
///
/// The tag header stores time as 24 + 8 bits (spec §E.4.1,
/// `Timestamp` + `TimestampExtended`), so a long-running stream wraps
/// after about 49.7 days — and a writer that never fills the extension
/// byte wraps its 24 bits after about 4.66 hours. Each timestamp is
/// placed at the position congruent to the wire value that is nearest
/// the previous tag, which keeps time monotonic across either wrap
/// while leaving genuine discontinuities (a jump that is not a wrap)
/// visible as jumps.
///
/// The first tag after open is taken at face value as an unsigned
/// 32-bit number.
#[derive(Clone, Copy, Debug, Default)]
enum Clock {
    /// Nothing read yet.
    #[default]
    Unanchored,
    /// The previous tag carried wire value `raw`, placed at `ext`.
    /// `wrap24` is set once the stream has been seen to wrap its low 24
    /// bits without carrying into the extension byte.
    At { raw: u32, ext: i64, wrap24: bool },
    /// No previous tag, but the next one is expected near `ext` (after
    /// a seek).
    Near { ext: i64 },
}

/// A 24-bit wrap is only recognised when one tag sits this close below
/// 2^24 ms and the other this close above zero.
const WRAP24_GUARD_MS: u32 = 60_000;
const WRAP24: i64 = 1 << 24;

impl Clock {
    /// Place `raw` on the timeline; also says whether the stream is in
    /// the "24-bit wrap without extension" regime afterwards.
    fn place(&self, raw: u32) -> (i64, bool) {
        match *self {
            Clock::Unanchored => (raw as i64, false),
            Clock::At {
                raw: prev,
                ext,
                wrap24,
            } => {
                let mut step = raw.wrapping_sub(prev) as i32 as i64;
                let mut wrap24 = wrap24;
                if (raw >> 24) == (prev >> 24) {
                    let (pl, rl) = (prev & 0x00FF_FFFF, raw & 0x00FF_FFFF);
                    let high = (1 << 24) - WRAP24_GUARD_MS;
                    if step < 0 && pl >= high && rl < WRAP24_GUARD_MS {
                        // Forward across a 24-bit wrap.
                        step += WRAP24;
                        wrap24 = true;
                    } else if step > 0 && wrap24 && pl < WRAP24_GUARD_MS && rl >= high {
                        // A straggler from just before that wrap.
                        step -= WRAP24;
                    }
                }
                (ext + step, wrap24)
            }
            Clock::Near { ext } => {
                let near32 = ext + (raw.wrapping_sub(ext as u32) as i32 as i64);
                if (near32 - ext).abs() >= WRAP24 / 2 && raw < (1 << 24) {
                    let low = (raw as i64 - (ext & (WRAP24 - 1)) + WRAP24 / 2).rem_euclid(WRAP24)
                        - WRAP24 / 2;
                    (ext + low, true)
                } else {
                    (near32, false)
                }
            }
        }
    }

    /// Timeline position for a tag of type `kind`. Only audio and video
    /// tags move the clock: script tags are conventionally stamped 0
    /// wherever they sit, and must not pull the timeline back there.
    fn stamp(&mut self, kind: Option<TagType>, raw: u32) -> i64 {
        if is_media(kind) {
            self.extend(raw)
        } else {
            self.place(raw).0
        }
    }

    /// Place `raw` and make it the reference for the next tag.
    fn extend(&mut self, raw: u32) -> i64 {
        let (ext, wrap24) = self.place(raw);
        *self = Clock::At { raw, ext, wrap24 };
        ext
    }
}

/// One seek landing point: a sync tag's record offset (the offset of the
/// `PreviousTagSize` in front of the tag) and its timeline position.
#[derive(Clone, Copy, Debug)]
struct IndexEntry {
    record_pos: u64,
    ts: i64,
}

/// Upper bound on the lazily built index (24 bytes per entry).
const MAX_INDEX_ENTRIES: usize = 1 << 16;

/// Sync-point index built as a side effect of reading.
///
/// It covers `[first record, scanned_to)` and grows only when the
/// demuxer (or a seek that has to look further ahead) walks tags
/// starting exactly at `scanned_to`, so it never costs a pass of its
/// own. Memory is bounded: once [`MAX_INDEX_ENTRIES`] is reached every
/// other entry is dropped and the minimum spacing doubles — a seek then
/// lands on the nearest kept entry and walks forward the short rest.
#[derive(Debug)]
struct LazyIndex {
    entries: Vec<IndexEntry>,
    /// Record offset of the first tag not yet examined.
    scanned_to: u64,
    /// Timeline state at `scanned_to`.
    clock: Clock,
    /// Minimum spacing between kept entries (0 = keep every sync tag).
    stride_ms: i64,
}

impl LazyIndex {
    fn new(first_record_pos: u64) -> Self {
        Self {
            entries: Vec::new(),
            scanned_to: first_record_pos,
            clock: Clock::default(),
            stride_ms: 0,
        }
    }

    /// Note the tag at `record_pos` (which must equal `scanned_to`).
    fn note(&mut self, record_pos: u64, total: usize, clock: Clock, ts: i64, sync: bool) {
        debug_assert_eq!(record_pos, self.scanned_to);
        self.scanned_to = record_pos + total as u64;
        self.clock = clock;
        if !sync {
            return;
        }
        // Keep the list strictly increasing in time so it can be
        // bisected; a sync tag that steps backwards (a discontinuity)
        // is not a target a time-based seek could name anyway.
        if let Some(last) = self.entries.last()
            && ts < last.ts + self.stride_ms.max(1)
        {
            return;
        }
        if self.entries.len() >= MAX_INDEX_ENTRIES {
            let span = self.entries.last().map_or(0, |l| l.ts) - self.entries[0].ts;
            let mut keep = false;
            self.entries.retain(|_| {
                keep = !keep;
                keep
            });
            self.stride_ms = (self.stride_ms * 2)
                .max(2 * span / MAX_INDEX_ENTRIES as i64)
                .max(1);
        }
        self.entries.push(IndexEntry { record_pos, ts });
    }

    /// Last entry at or before `target`.
    fn at_or_before(&self, target: i64) -> Option<IndexEntry> {
        let n = self.entries.partition_point(|e| e.ts <= target);
        n.checked_sub(1).map(|i| self.entries[i])
    }

    /// Whether the index already holds a sync point after `target`
    /// (i.e. the answer for `target` cannot change by scanning further).
    fn covers(&self, target: i64) -> bool {
        self.entries.last().is_some_and(|e| e.ts > target)
    }
}

/// Stream-health counters a [`FlvDemuxer`] keeps while reading — the
/// things a well-formed file never triggers. Read them through
/// [`FlvDemuxer::stats`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct FlvDemuxStats {
    /// Tags whose preceding `PreviousTagSize` was not the size of the
    /// tag before it (spec §E.3). The field is never used to walk the
    /// file, so a wrong value costs nothing but is counted here.
    pub previous_tag_size_mismatches: u64,
    /// Times the 24 + 8-bit wire timestamp wrapped and was carried onto
    /// the 64-bit packet timeline.
    pub timestamp_wraps: u64,
    /// Timestamp jumps that were not wraps: a step backwards of more
    /// than one second, or forwards of more than ten, from one tag to
    /// the next. Packet timestamps follow the stream across the jump.
    pub timestamp_discontinuities: u64,
    /// Tags of a type other than audio (8), video (9) or script (18),
    /// skipped by their `DataSize`.
    pub unknown_tags_skipped: u64,
    /// Audio or video tags dropped because the file gave no sign of that
    /// stream when it was opened (neither a tag in the discovery window
    /// nor a header flag plus `onMetaData` codec id).
    pub undeclared_stream_tags_dropped: u64,
    /// Configuration records (AAC / AVC sequence headers, Ex
    /// `SequenceStart`) that arrived after open or replaced an earlier
    /// one. Each also surfaces as a `header`-flagged packet and updates
    /// the stream's `extradata`.
    pub config_updates: u64,
    /// Times a stream switched codec mid-file (the tag header named a
    /// different codec than the stream had). The stream's parameters
    /// are rebuilt from the new tag.
    pub codec_changes: u64,
    /// `true` once reading stopped at a tag that was cut short (the file
    /// ends inside a tag header or body). Reading again retries from the
    /// same tag, so a file that is still growing simply continues.
    pub ended_mid_tag: bool,
    /// `true` when the `onMetaData` keyframe table pointed at something
    /// that was not a video key frame at the stated time and was
    /// therefore discarded; seeks then use the lazily built index.
    pub keyframe_table_rejected: bool,
}

/// Public [`Demuxer`] type, exported so the integration tests can
/// name it. Intentionally opaque — construction is via [`open`].
pub struct FlvDemuxer {
    reader: TagReader,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    duration_micros: Option<i64>,
    audio_stream_index: Option<u32>,
    video_stream_index: Option<u32>,
    /// Codec identity ([`audio_codec_key`] / [`video_codec_key`]) each
    /// stream's parameters were built from; `None` while a stream is
    /// still an `onMetaData` placeholder.
    codec_keys: [Option<u32>; 2],
    /// Per-stream "caller wants packets" flags
    /// ([`Demuxer::set_active_streams`]).
    active: Vec<bool>,
    /// Offset of `PreviousTagSize0`, the first record of the body.
    first_record_pos: u64,
    /// Script tags below this offset are already in the metadata bag.
    script_parsed_to: u64,
    /// Timeline state at the read cursor.
    clock: Clock,
    /// Highest timeline position handed out so far.
    max_ts: i64,
    /// What the next `PreviousTagSize` should read (`None` right after
    /// a seek, when the previous tag is unknown).
    expected_prev_size: Option<u32>,
    /// Sync points seen so far.
    index: LazyIndex,
    /// `keyframes` table from `onMetaData`, until proven wrong.
    keyframe_index: Option<KeyframeIndex>,
    /// `true` when discovery saw a `|AdditionalHeader` script tag (FLV
    /// encryption, spec Annex F.2). The demuxer still emits packets for
    /// every tag — encrypted media bodies surface with
    /// `flags.discard = true` so decoders skip past them rather than
    /// trying to interpret ciphertext as a frame.
    is_encrypted: bool,
    /// Every track of the most-recently-emitted multitrack Ex tag, in
    /// wire order (enhanced-rtmp-v2 §`ExAudioTagBody` / §`ExVideoTagBody`).
    /// `next_packet` emits a packet built from the *default* track only,
    /// so this side-channel preserves the non-default variants (different
    /// bitrate / resolution / codec / language / camera angle) that the
    /// stream model would otherwise drop. Reset to `None` each time
    /// `next_packet` returns a packet from a single-track tag, so the
    /// accessor never reports stale tracks from an earlier multitrack tag.
    last_multitrack: Option<Vec<MultitrackPacketTrack>>,
    /// ModEx state of the Ex tag the most-recently-emitted packet was
    /// built from (enhanced-rtmp-v2 §`ModEx`). `next_packet` folds the
    /// `TimestampOffsetNano` accumulator into a millisecond pts/dts but
    /// the sub-millisecond refinement (and any reserved-subtype ModEx
    /// blobs) are dropped from the `Packet` because the core `Packet`
    /// carries no nanosecond field. This side-channel preserves them so
    /// a remuxer can reconstruct the original `TimestampOffsetNano` ModEx
    /// (and the nano-refined presentation time). Reset to its empty state
    /// each time `next_packet` returns a packet from a tag that carried
    /// no ModEx prefix, so the accessor never reports stale offsets from
    /// an earlier tag.
    last_mod_ex: Option<LastModEx>,
    /// Embedded images harvested from `onImageData` script tags during
    /// discovery (Adobe FLV spec v10.1 Annex B.2). One entry per
    /// `onImageData` tag seen, in discovery order. Empty for the common
    /// case of a file with no embedded images.
    embedded_images: Vec<EmbeddedImage>,
    stats: FlvDemuxStats,
    /// The duration came from [`FlvDemuxer::scan_to_end`], not from the
    /// file, and is refreshed by the next scan.
    duration_is_scanned: bool,
}

/// An embedded image harvested from an `onImageData` script tag
/// (Adobe FLV spec v10.1 Annex B.2 "Image Metadata").
///
/// When an F4V/FLV sample is an image type (GIF, PNG, or JPEG) the
/// producer surfaces the original compressed image file through an
/// `onImageData` script-data tag carrying two properties: `data` (a
/// BYTEARRAY of the original GIF/PNG/JPEG bytes) and `trackid` (a
/// DOUBLE naming the track the sample belongs to). The image bytes are
/// binary and have no representation in the string `metadata()` bag, so
/// the demuxer collects each `onImageData` it sees during discovery
/// into [`FlvDemuxer::embedded_images`]; the bag additionally carries
/// the `onimagedata.<n>.trackid` / `.length` scalars so a caller
/// scanning only the string bag still learns an image is present.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EmbeddedImage {
    /// The `trackid` (Annex B.2 DOUBLE) the image sample belongs to.
    /// `None` when the producer omitted the property.
    pub track_id: Option<u32>,
    /// The original compressed image file bytes (the `data` BYTEARRAY) —
    /// a complete GIF, PNG, or JPEG file as the producer embedded it.
    pub data: Vec<u8>,
}

/// Captured ModEx state of the Ex tag a packet was built from.
///
/// Mirrors the `timestamp_offset_nano` + `mod_ex_entries` fields the
/// [`ExAudioTagHeader`] / [`ExVideoTagHeader`] parsers expose, lifted
/// onto the demuxer so a caller can recover the sub-millisecond timing
/// refinement that the millisecond-quantised [`Packet`] pts/dts cannot
/// carry.
#[derive(Clone, Debug, Default)]
struct LastModEx {
    /// Sum of `TimestampOffsetNano` ModEx offsets (ns) on the tag.
    offset_nano: u32,
    /// Every ModEx entry on the tag, in wire order.
    entries: Vec<ModExEntry>,
}

impl FlvDemuxer {
    /// `true` when this FLV file declared an [`Annex F`](Adobe FLV
    /// Spec v10.1) encryption header. Always `false` for plain FLVs.
    pub fn is_encrypted(&self) -> bool {
        self.is_encrypted
    }

    /// Every track of the most-recently-emitted Enhanced-RTMP multitrack
    /// Ex tag (audio or video), in wire order — or `None` when the last
    /// packet [`next_packet`] returned came from a single-track tag.
    ///
    /// [`next_packet`] surfaces the *default* track (trackId 0, or the
    /// first track in wire order) as the [`Packet`]; for a multitrack tag
    /// the other variants — different bitrate, resolution, codec,
    /// language, or camera angle (enhanced-rtmp-v2 §"Track Ordering") —
    /// are not part of the single-stream packet flow. This accessor lets
    /// a caller recover them: each [`MultitrackPacketTrack`] carries the
    /// `trackId`, the codec `fourcc` + resolved `codec_name`, and an
    /// *owned* copy of that track's coded payload, so a receiver
    /// implementing its own track-selection logic can pick a non-default
    /// variant without re-parsing the Ex header or re-running
    /// [`crate::split_tracks`].
    ///
    /// The slice is refreshed on every [`next_packet`] call: it holds the
    /// tracks of the tag the just-returned packet was built from, and is
    /// cleared to `None` as soon as a single-track tag is emitted, so it
    /// never reports stale tracks from an earlier multitrack tag.
    /// `SequenceEnd` and other tags that produce no packet do not disturb
    /// the previously-captured set.
    ///
    /// [`next_packet`]: oxideav_core::Demuxer::next_packet
    pub fn last_multitrack_tracks(&self) -> Option<&[MultitrackPacketTrack]> {
        self.last_multitrack.as_deref()
    }

    /// Every embedded image harvested from `onImageData` script tags
    /// during discovery (Adobe FLV spec v10.1 Annex B.2 "Image
    /// Metadata"), in discovery order. Empty for the common case of a
    /// file with no embedded images.
    ///
    /// When an F4V/FLV sample is an image type (GIF / PNG / JPEG) the
    /// producer surfaces the original compressed image file through an
    /// `onImageData` script tag carrying `data` (a BYTEARRAY of the file
    /// bytes) + `trackid` (a DOUBLE). The binary bytes have no place in
    /// the string `metadata()` bag, so each [`EmbeddedImage`] carries an
    /// owned copy of the original file bytes plus its `track_id`. The
    /// `metadata()` bag still gains an `onimagedata.<n>.trackid` /
    /// `.length` scalar pair per image so a caller scanning only the bag
    /// learns an image is present without reaching this accessor.
    pub fn embedded_images(&self) -> &[EmbeddedImage] {
        &self.embedded_images
    }

    /// Sum of `TimestampOffsetNano` ModEx offsets (in nanoseconds) on the
    /// Enhanced-RTMP Ex tag the most-recently-emitted [`next_packet`]
    /// packet was built from — or `None` when that tag carried no
    /// `TimestampOffsetNano` ModEx (every legacy tag, and every Ex tag
    /// without a ModEx prefix).
    ///
    /// The FLV wire timestamp is a 24-bit-plus-8 millisecond value;
    /// enhanced-rtmp-v2 §`ModEx` layers a `TimestampOffsetNano` refinement
    /// (0..=999_999 ns, "just over 1 ms" of sub-millisecond precision) on
    /// top so a receiver can recover the exact presentation time when
    /// re-targeting formats with finer timescales (MP4, M2TS, MSE).
    /// [`next_packet`] cannot fold this into the [`Packet`] pts/dts —
    /// those are integer milliseconds and the core [`Packet`] has no
    /// nanosecond field — so this accessor surfaces it for a remuxer that
    /// wants to preserve or re-emit the offset. The nano-refined
    /// presentation time of the last packet is
    /// `pts_ms * 1_000_000 + offset_ns`.
    ///
    /// Refreshed on every [`next_packet`] call and cleared to `None` as
    /// soon as a tag without a `TimestampOffsetNano` ModEx is emitted, so
    /// it never reports a stale offset from an earlier tag. Tags that
    /// produce no packet (`SequenceEnd`, script tags) do not disturb the
    /// captured value.
    ///
    /// [`next_packet`]: oxideav_core::Demuxer::next_packet
    pub fn last_timestamp_offset_nano(&self) -> Option<u32> {
        self.last_mod_ex
            .as_ref()
            .map(|m| m.offset_nano)
            .filter(|&n| n != 0)
    }

    /// Every ModEx entry parsed off the front of the Enhanced-RTMP Ex tag
    /// the most-recently-emitted [`next_packet`] packet was built from, in
    /// wire order — or `None` when that tag carried no ModEx prefix.
    ///
    /// Unlike [`last_timestamp_offset_nano`], which collapses to the
    /// single accumulated nanosecond value, this preserves the full ModEx
    /// chain including reserved/future subtypes (carried as
    /// [`crate::mod_ex::ModExPayload::Reserved`] with their raw bytes
    /// attached). A remuxer that wants byte-exact re-emission of the
    /// tag's ModEx prefix feeds these straight back into
    /// [`crate::mod_ex::emit`] (or the `mod_ex_entries` field of the
    /// [`ExAudioTagHeader`] / [`ExVideoTagHeader`] writers).
    ///
    /// Refreshed on every [`next_packet`] call and cleared to `None` for
    /// any tag without a ModEx prefix, so it never reports stale entries.
    ///
    /// [`last_timestamp_offset_nano`]: Self::last_timestamp_offset_nano
    /// [`next_packet`]: oxideav_core::Demuxer::next_packet
    pub fn last_mod_ex_entries(&self) -> Option<&[ModExEntry]> {
        self.last_mod_ex.as_ref().map(|m| m.entries.as_slice())
    }

    /// Stream-health counters gathered so far: wrong `PreviousTagSize`
    /// fields, timestamp wraps and discontinuities, skipped unknown tag
    /// types, late or changed configuration records, mid-stream codec
    /// switches, and whether reading stopped inside a cut-off tag.
    pub fn stats(&self) -> &FlvDemuxStats {
        &self.stats
    }

    /// Number of sync points (video key frames, or audio tags of an
    /// audio-only file) the demuxer has indexed so far while reading.
    /// The index is a by-product of [`Demuxer::next_packet`] and of
    /// seeks that had to look ahead; it is capped at 65 536 entries.
    pub fn indexed_sync_points(&self) -> usize {
        self.index.entries.len()
    }

    /// Walk the rest of the file once — tag headers only, payloads are
    /// skipped — to index every sync point and learn where the stream
    /// ends. `open` never does this on its own; call it when a complete
    /// index or a duration is worth one pass over the file (a capture
    /// that was never finalised, a file without `onMetaData`).
    ///
    /// Returns the timestamp of the last complete media tag in
    /// milliseconds (`None` when there is none). If the file declared
    /// no duration, [`Demuxer::duration_micros`] afterwards reports the
    /// span from the first media tag to that one. The read position is
    /// unchanged; on a file that is still growing the call can be
    /// repeated and only walks the new part.
    pub fn scan_to_end(&mut self) -> Result<Option<i64>> {
        let resume = self.reader.position();
        self.reader.seek(self.index.scanned_to)?;
        let walked = self.walk_to(self.index.clock, i64::MAX);
        self.reader.seek(resume)?;
        walked?;
        let Clock::At { ext: last, .. } = self.index.clock else {
            return Ok(None);
        };
        if self.duration_micros.is_none() || self.duration_is_scanned {
            let first = self
                .streams
                .iter()
                .filter_map(|s| s.start_time)
                .min()
                .unwrap_or(0);
            self.duration_micros = Some((last - first).max(0).saturating_mul(1000));
            self.duration_is_scanned = true;
        }
        Ok(Some(last))
    }

    /// Step the timeline for a tag read at the cursor and keep the
    /// wrap / discontinuity counters.
    fn advance_clock(&mut self, raw: u32) -> i64 {
        let before = self.clock;
        let ts = self.clock.extend(raw);
        if let Clock::At {
            raw: prev_raw,
            ext: prev,
            ..
        } = before
        {
            // (a step that is neither is ordinary playback)
            let step = ts - prev;
            if raw < prev_raw && step > 0 {
                // Count each roll-over once, however the tags around
                // it interleave.
                if (ts >> 24) > (self.max_ts >> 24) {
                    self.stats.timestamp_wraps += 1;
                }
            } else if !(-1_000..=10_000).contains(&step) {
                self.stats.timestamp_discontinuities += 1;
            }
        }
        self.max_ts = self.max_ts.max(ts);
        ts
    }

    /// Walk tag headers forward from the reader's cursor (timeline state
    /// `clock`), feeding the lazy index where the walk passes its
    /// frontier, until a tag later than `target` or the end of input.
    /// Returns the last sync point at or before `target` that the walk
    /// saw. Bodies are skipped, not read.
    fn walk_to(&mut self, mut clock: Clock, target: i64) -> Result<Option<IndexEntry>> {
        let has_video = self.video_stream_index.is_some();
        let mut best = None;
        loop {
            let record_pos = self.reader.position();
            if !self.reader.fill(RECORD_PREFIX)? {
                break;
            }
            let header = parse_tag_header(&self.reader.window()[4..RECORD_PREFIX]);
            let total = RECORD_PREFIX + header.data_size as usize;
            let head_len = (header.data_size as usize).min(2);
            if !self.reader.fill(RECORD_PREFIX + head_len)? {
                break;
            }
            let head = [
                self.reader
                    .window()
                    .get(RECORD_PREFIX)
                    .copied()
                    .unwrap_or(0),
                self.reader
                    .window()
                    .get(RECORD_PREFIX + 1)
                    .copied()
                    .unwrap_or(0),
            ];
            let head = &head[..head_len];
            if record_pos == self.index.scanned_to {
                clock = self.index.clock;
            }
            let ts = clock.stamp(header.kind, header.timestamp_ms);
            let sync = is_sync_tag(header.kind, head, has_video);
            if sync && ts > target {
                break;
            }
            // A tag only counts once the input holds all of it (a file
            // that is still growing may end inside this one).
            if !self.reader.reaches(record_pos + total as u64)? {
                self.reader.seek(record_pos)?;
                break;
            }
            if record_pos == self.index.scanned_to {
                self.index.note(record_pos, total, clock, ts, sync);
            }
            if sync {
                best = Some(IndexEntry { record_pos, ts });
            }
        }
        Ok(best)
    }

    /// Put the read cursor on `entry`.
    fn land(&mut self, entry: IndexEntry) -> Result<i64> {
        self.reader.seek(entry.record_pos)?;
        self.clock = Clock::Near { ext: entry.ts };
        self.expected_prev_size = None;
        Ok(entry.ts)
    }

    /// Verify that the keyframe table's claim "a key frame stamped
    /// `table_ts` starts at byte `tag_pos`" holds in the file. (A video
    /// tag with key-frame type bits is enough here: some writers list
    /// the sequence header that precedes the first picture.)
    fn check_table_entry(&mut self, tag_pos: u64, table_ts: i64) -> Result<Option<IndexEntry>> {
        // The table gives the offset of the tag; the record starts at
        // the `PreviousTagSize` four bytes earlier.
        let Some(record_pos) = tag_pos.checked_sub(4) else {
            return Ok(None);
        };
        if record_pos < self.first_record_pos {
            return Ok(None);
        }
        self.reader.seek(record_pos)?;
        if !self.reader.fill(RECORD_PREFIX + 1)? {
            return Ok(None);
        }
        let w = self.reader.window();
        let header = parse_tag_header(&w[4..RECORD_PREFIX]);
        if header.data_size == 0
            || header.stream_id != 0
            || header.kind != Some(TagType::Video)
            || !is_key_typed(w[RECORD_PREFIX])
        {
            return Ok(None);
        }
        let (ts, _) = Clock::Near { ext: table_ts }.place(header.timestamp_ms);
        // The table's time must describe this tag (allowing for
        // rounding and for a writer whose table times are offset from
        // its tag times by a few seconds).
        if (ts - table_ts).abs() > 5_000 {
            return Ok(None);
        }
        Ok(Some(IndexEntry { record_pos, ts }))
    }

    /// Seek through the `onMetaData` keyframe table. `Ok(None)` when
    /// there is no usable table (absent, or it pointed at something that
    /// is not a key frame — in which case it is dropped for good).
    fn seek_by_table(&mut self, target: i64) -> Result<Option<i64>> {
        let (Some(idx), Some(_)) = (&self.keyframe_index, self.video_stream_index) else {
            return Ok(None);
        };
        let target_seconds = target as f64 / 1000.0;
        let n = idx.times_seconds.partition_point(|t| *t <= target_seconds);
        let i = n.saturating_sub(1);
        let tag_pos = idx.file_positions[i];
        let table_ts = (idx.times_seconds[i] * 1000.0).round() as i64;
        let entry = self.check_table_entry(tag_pos, table_ts)?;
        let Some(entry) = entry else {
            self.keyframe_index = None;
            self.stats.keyframe_table_rejected = true;
            return Ok(None);
        };
        // A table may list only some key frames: walk on to the last
        // one at or before the target (at most one table interval).
        let mut best = entry;
        if entry.ts < target {
            self.reader.seek(entry.record_pos)?;
            if let Some(later) = self.walk_to(Clock::Near { ext: entry.ts }, target)? {
                best = later;
            }
        }
        self.land(best).map(Some)
    }

    /// Seek through the lazily built index, extending it up to the
    /// target first when reading has not got that far yet.
    fn seek_by_scan(&mut self, target: i64) -> Result<i64> {
        if !self.index.covers(target) {
            self.reader.seek(self.index.scanned_to)?;
            self.walk_to(self.index.clock, target)?;
        }
        let Some(mut entry) = self.index.at_or_before(target).or_else(|| {
            // Target lies before the first sync point: start there.
            self.index.entries.first().copied()
        }) else {
            // No sync point anywhere (yet): rewind to the first tag.
            self.reader.seek(self.first_record_pos)?;
            self.clock = Clock::default();
            self.expected_prev_size = Some(0);
            return Ok(0);
        };
        if self.index.stride_ms > 0 && entry.ts < target {
            // The index was thinned: the exact sync point may sit
            // between two kept entries.
            self.reader.seek(entry.record_pos)?;
            if let Some(later) = self.walk_to(Clock::Near { ext: entry.ts }, target)? {
                entry = later;
            }
        }
        self.land(entry)
    }
}

/// Captured ModEx state of an Ex tag, or `None` when it carried no
/// ModEx prefix.
fn mod_ex_state(offset_nano: u32, entries: Vec<ModExEntry>) -> Option<LastModEx> {
    if entries.is_empty() {
        return None;
    }
    Some(LastModEx {
        offset_nano,
        entries,
    })
}

/// Resolve every track of a multitrack Ex video tag body to an owned
/// [`MultitrackPacketTrack`], or `None` when the tag is single-track or
/// its multitrack body does not split. The returned payloads are the
/// per-track Ex bodies exactly as they sit on the wire (the leading
/// `SI24` CompositionTimeOffset for AVC/HEVC/VVC `CodedFrames` is
/// preserved).
fn video_multitrack_tracks(
    body: &[u8],
    ex: &ExVideoTagHeader,
) -> Option<Vec<MultitrackPacketTrack>> {
    multitrack_tracks(body, ex.multitrack?, ex.fourcc, ex.bytes_consumed, true)
}

/// Audio counterpart of [`video_multitrack_tracks`].
fn audio_multitrack_tracks(
    body: &[u8],
    ex: &ExAudioTagHeader,
) -> Option<Vec<MultitrackPacketTrack>> {
    multitrack_tracks(body, ex.multitrack?, ex.fourcc, ex.bytes_consumed, false)
}

fn multitrack_tracks(
    body: &[u8],
    mt_type: crate::multitrack::AvMultitrackType,
    default_fourcc: Option<u32>,
    bytes_consumed: usize,
    is_video: bool,
) -> Option<Vec<MultitrackPacketTrack>> {
    let tail = &body[bytes_consumed.min(body.len())..];
    let tracks = split_tracks(mt_type, default_fourcc.unwrap_or(0), tail).ok()?;
    Some(
        tracks
            .into_iter()
            .map(|t| {
                let codec_name = if is_video {
                    fourcc_codec_id_str(t.fourcc)
                } else {
                    fourcc_audio_codec_id_str(t.fourcc)
                };
                MultitrackPacketTrack {
                    track_id: t.track_id,
                    fourcc: t.fourcc,
                    codec_name,
                    payload: tail[t.payload].to_vec(),
                }
            })
            .collect(),
    )
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
            .field("indexed_sync_points", &self.index.entries.len())
            .field("stats", &self.stats)
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

    /// Read the next packet.
    ///
    /// Returns [`Error::Eof`] when the input ends — at the end of the
    /// last complete tag even if the file was cut inside the next one
    /// ([`FlvDemuxStats::ended_mid_tag`] tells the two apart). The read
    /// position stays on the incomplete tag, so calling `next_packet`
    /// again after the file has grown continues from there.
    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            let record_pos = self.reader.position();
            let Some(header) = peek_record(&mut self.reader)? else {
                // Only the final `PreviousTagSize` (or nothing) may
                // remain after the last tag.
                self.stats.ended_mid_tag = self.reader.available() > 4;
                return Err(Error::Eof);
            };
            self.stats.ended_mid_tag = false;
            let total = RECORD_PREFIX + header.data_size as usize;

            let prev_size = {
                let w = self.reader.window();
                u32::from_be_bytes([w[0], w[1], w[2], w[3]])
            };
            if self.expected_prev_size.is_some_and(|e| e != prev_size) {
                self.stats.previous_tag_size_mismatches += 1;
            }
            self.expected_prev_size = Some(11 + header.data_size);

            let at_frontier = record_pos == self.index.scanned_to;
            if at_frontier {
                // One timeline for packets and seek results.
                if !matches!(self.index.clock, Clock::Unanchored) {
                    self.clock = self.index.clock;
                }
            }
            let ts = if is_media(header.kind) {
                self.advance_clock(header.timestamp_ms)
            } else {
                self.clock.place(header.timestamp_ms).0
            };
            let body = &self.reader.window()[RECORD_PREFIX..total];
            if at_frontier {
                let sync = is_sync_tag(
                    header.kind,
                    &body[..body.len().min(2)],
                    self.video_stream_index.is_some(),
                );
                self.index.note(record_pos, total, self.clock, ts, sync);
            }

            let mut out: Option<Packet> = None;
            if header.filter {
                // Filtered (encrypted) tags carry an EncryptionTagHeader
                // + FilterParams preamble in front of the codec body
                // (spec F.3). We carry no DRM client, so the encrypted
                // bytes go downstream with `flags.discard = true` and
                // decoders never see ciphertext as a frame.
                let slot = match header.kind {
                    Some(TagType::Audio) => self.audio_stream_index,
                    Some(TagType::Video) => self.video_stream_index,
                    _ => None,
                };
                if let Some(idx) = slot
                    && self.active[idx as usize]
                {
                    match build_encrypted_packet(&self.streams[idx as usize], ts, body) {
                        Ok(p) => out = p,
                        Err(e) => {
                            self.reader.consume(total);
                            return Err(e);
                        }
                    }
                }
            } else {
                match header.kind {
                    None => self.stats.unknown_tags_skipped += 1,
                    Some(TagType::ScriptData) => {
                        // Script tags past the discovery window (a late
                        // `onMetaData`, cue points, live captions) join
                        // the metadata bag when first read.
                        if record_pos >= self.script_parsed_to {
                            let mut encryption = None;
                            let mut xmp = None;
                            parse_script_body(
                                body,
                                &mut self.metadata,
                                &mut self.duration_micros,
                                &mut self.keyframe_index,
                                &mut encryption,
                                &mut xmp,
                                &mut self.embedded_images,
                            );
                            if let Some(x) = xmp {
                                self.metadata.retain(|(k, _)| k != "xmp");
                                self.metadata.push(("xmp".into(), x));
                            }
                            self.script_parsed_to = record_pos + total as u64;
                        }
                    }
                    Some(TagType::Audio) => match self.audio_stream_index {
                        None => self.stats.undeclared_stream_tags_dropped += 1,
                        Some(idx) if !self.active[idx as usize] => {}
                        Some(idx) => {
                            let ex = ExAudioTagHeader::parse(body).ok().flatten();
                            let stream = &mut self.streams[idx as usize];
                            if !body.is_empty() {
                                refresh_audio_stream(
                                    stream,
                                    &mut self.codec_keys[0],
                                    body,
                                    ts,
                                    &self.metadata,
                                    &mut self.stats,
                                );
                            }
                            // Enhanced-RTMP `MultichannelConfig` frames
                            // carry the stream's speaker layout (no
                            // audio data); a new one supersedes the
                            // last.
                            if let Some(ex) = &ex
                                && ex.packet_type == ExAudioPacketType::MultichannelConfig
                            {
                                let tail = &body[ex.bytes_consumed.min(body.len())..];
                                let payload = match ex.multitrack {
                                    Some(mt) => default_track_payload(mt, ex.fourcc, tail),
                                    None => tail.to_vec(),
                                };
                                harvest_multichannel_config(&payload, &mut self.metadata, stream);
                            }
                            out = build_audio_packet(stream, ts, body, ex.as_ref());
                            if out.is_some() {
                                self.last_multitrack =
                                    ex.as_ref().and_then(|ex| audio_multitrack_tracks(body, ex));
                                self.last_mod_ex = ex.and_then(|ex| {
                                    mod_ex_state(ex.timestamp_offset_nano, ex.mod_ex_entries)
                                });
                            }
                        }
                    },
                    Some(TagType::Video) => match self.video_stream_index {
                        None => self.stats.undeclared_stream_tags_dropped += 1,
                        Some(idx) if !self.active[idx as usize] => {}
                        Some(idx) => {
                            let ex = ExVideoTagHeader::parse(body).ok().flatten();
                            let stream = &mut self.streams[idx as usize];
                            if !body.is_empty() {
                                refresh_video_stream(
                                    stream,
                                    &mut self.codec_keys[1],
                                    body,
                                    ts,
                                    &self.metadata,
                                    &mut self.stats,
                                );
                            }
                            // Enhanced-RTMP `Metadata` frames carry
                            // AMF-encoded HDR `colorInfo` (no video
                            // data); a new one replaces the last.
                            if let Some(ex) = &ex
                                && matches!(ex.packet_type, ExPacketType::Metadata)
                            {
                                let start = ex.bytes_consumed.min(body.len());
                                harvest_video_metadata_frame(&body[start..], &mut self.metadata);
                            }
                            out = build_video_packet(stream, ts, body, ex.as_ref());
                            if out.is_some() {
                                self.last_multitrack =
                                    ex.as_ref().and_then(|ex| video_multitrack_tracks(body, ex));
                                self.last_mod_ex = ex.and_then(|ex| {
                                    mod_ex_state(ex.timestamp_offset_nano, ex.mod_ex_entries)
                                });
                            }
                        }
                    },
                }
            }
            self.reader.consume(total);
            // A frame tag that holds only its framing bytes carries
            // nothing to decode (some writers close a stream with one).
            if let Some(pkt) = out
                && (!pkt.data.is_empty() || pkt.flags.header)
            {
                return Ok(pkt);
            }
        }
    }

    /// Restrict packet delivery to the listed streams. Tags of the
    /// other streams are stepped over without their payload being
    /// copied.
    fn set_active_streams(&mut self, indices: &[u32]) {
        for (i, a) in self.active.iter_mut().enumerate() {
            *a = indices.contains(&(i as u32));
        }
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        self.duration_micros
    }

    /// Seek to the last sync point **at or before** `pts` (stream time
    /// base, always 1/1000 s) and return its timestamp. A sync point is
    /// a video key frame, or any audio tag in an audio-only file. The
    /// next packet read is that tag.
    ///
    /// * With an `onMetaData` `keyframes` table the nearest listed key
    ///   frame is found by bisection and checked against the file (a
    ///   table that points at anything but a key frame is discarded and
    ///   reported in [`FlvDemuxStats::keyframe_table_rejected`]); the
    ///   short stretch from there to the target is then walked so a
    ///   table that lists only some key frames still gives the exact
    ///   answer.
    /// * Without one, the demuxer uses the index it builds as tags are
    ///   read. A target inside the part already read costs a bisection;
    ///   a target beyond it walks tag headers forward from the furthest
    ///   point seen (payloads are skipped), remembering every sync point
    ///   so the walk is never repeated.
    ///
    /// A target before the first sync point lands on the first one; a
    /// target past the end lands on the last one. On a file that is
    /// still being written the answer reflects the tags present at the
    /// time of the call, and a later call sees the newer ones.
    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if (stream_index as usize) >= self.streams.len() {
            return Err(Error::invalid(format!(
                "FLV: stream index {stream_index} out of range (have {} stream(s))",
                self.streams.len()
            )));
        }
        let target = pts.max(0);
        if let Some(landed) = self.seek_by_table(target)? {
            return Ok(landed);
        }
        self.seek_by_scan(target)
    }
}

/// Bring a stream's parameters in line with the audio tag being read:
/// fill a placeholder, follow a codec switch, and pick up a late or
/// changed configuration record.
fn refresh_audio_stream(
    stream: &mut StreamInfo,
    key: &mut Option<u32>,
    body: &[u8],
    ts: i64,
    metadata: &[(String, String)],
    stats: &mut FlvDemuxStats,
) {
    let now = audio_codec_key(body);
    if now.is_some() && now != *key {
        if let Ok(info) = build_audio_stream(stream.index, body, metadata) {
            if key.is_some() {
                stats.codec_changes += 1;
            } else {
                stream.start_time = Some(ts);
            }
            stream.params = info.params;
            *key = now;
        }
        return;
    }
    if let Some(cfg) = audio_config_record(body) {
        if cfg != stream.params.extradata {
            stream.params.extradata = cfg;
            stats.config_updates += 1;
        }
    } else if now.is_some_and(audio_header_is_authoritative) {
        // The SoundRate / SoundSize / SoundType bits are the sample
        // layout for these formats; follow them if they change.
        let ah = AudioTagHeader::parse(body[0]);
        let fmt = if ah.is_16bit {
            SampleFormat::S16
        } else {
            SampleFormat::U8
        };
        let p = &mut stream.params;
        if p.sample_rate != Some(ah.sample_rate_hz())
            || p.channels != Some(ah.channels())
            || p.sample_format != Some(fmt)
        {
            p.sample_rate = Some(ah.sample_rate_hz());
            p.channels = Some(ah.channels());
            p.sample_format = Some(fmt);
            stats.config_updates += 1;
        }
    }
}

/// Video counterpart of [`refresh_audio_stream`].
fn refresh_video_stream(
    stream: &mut StreamInfo,
    key: &mut Option<u32>,
    body: &[u8],
    ts: i64,
    metadata: &[(String, String)],
    stats: &mut FlvDemuxStats,
) {
    let now = video_codec_key(body);
    if now.is_some() && now != *key {
        if let Ok(info) = build_video_stream(stream.index, body, metadata) {
            if key.is_some() {
                stats.codec_changes += 1;
            } else {
                stream.start_time = Some(ts);
            }
            stream.params = info.params;
            *key = now;
        }
        return;
    }
    if let Some(cfg) = video_config_record(body)
        && cfg != stream.params.extradata
    {
        stream.params.extradata = cfg;
        stats.config_updates += 1;
    }
}

/// Recover the default track's payload (trackId 0, or the first track in
/// wire order) from a multitrack Ex tag body tail. Returns the whole
/// tail unchanged if the split fails or yields no tracks, so a malformed
/// multitrack record degrades to "treat the body as opaque extradata"
/// rather than panicking.
fn default_track_payload(
    mt_type: crate::multitrack::AvMultitrackType,
    default_fourcc: Option<u32>,
    tail: &[u8],
) -> Vec<u8> {
    match split_tracks(mt_type, default_fourcc.unwrap_or(0), tail) {
        Ok(tracks) => match tracks
            .iter()
            .find(|t| t.track_id == 0)
            .or_else(|| tracks.first())
        {
            Some(track) => tail[track.payload.clone()].to_vec(),
            None => tail.to_vec(),
        },
        Err(_) => tail.to_vec(),
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
    // Enhanced RTMP (E-FLV) ExAudioTagHeader path: SoundFormat=9
    // (ExHeader) signals the codec is identified by FourCC rather than
    // the legacy 4-bit SoundFormat. Build the stream descriptor off the
    // FourCC codec id; the SequenceStart body (Opus ID header / FLAC
    // STREAMINFO / AAC AudioSpecificConfig) becomes extradata for the
    // decoder.
    if let Some(ex) = ExAudioTagHeader::parse(body)? {
        let codec_name = match ex.fourcc {
            Some(fcc) => fourcc_audio_codec_id_str(fcc),
            // ManyTracksManyCodecs case: per-track FourCc is inside the
            // body. We don't track-split, so surface a sentinel codec
            // id so the resolver gets a stable name to log.
            None => "flv:exaudio:multicodec".into(),
        };
        let codec = CodecId::new(codec_name);
        let mut params = CodecParameters::audio(codec);
        // Pull authoritative rate / channels from onMetaData when
        // present (the ExHeader body itself carries no SoundRate /
        // SoundSize / SoundType fields — those bits were repurposed as
        // AudioPacketType).
        if let Some(sr) = metadata_lookup_u32(metadata, "audiosamplerate")
            && sr > 0
        {
            params.sample_rate = Some(sr);
        }
        if let Some(b) = metadata_lookup_bool(metadata, "stereo") {
            params.channels = Some(if b { 2 } else { 1 });
        }
        if let Some(kbps) = metadata_lookup_f64(metadata, "audiodatarate")
            && kbps.is_finite()
            && kbps >= 0.0
        {
            params.bit_rate = Some((kbps * 1000.0) as u64);
        }
        // `audiosamplesize` (E.5) is the only spec-defined source of
        // per-sample resolution on the ExAudio side — the ExHeader byte
        // repurposes the legacy SoundSize bit as AudioPacketType, so
        // there is no header field to fall back on.
        if let Some(fmt) = metadata_audio_sample_format(metadata) {
            params.sample_format = Some(fmt);
        }
        // SequenceStart's body is the codec's decoder-configuration
        // record — route to extradata so consumers find it without
        // peeking at the first packet (consistent with the legacy AAC
        // path below and the ExVideo path above). In multitrack mode the
        // config record lives inside the default track's payload, so we
        // split first and lift trackId 0's bytes.
        if ex.packet_type == ExAudioPacketType::SequenceStart && body.len() > ex.bytes_consumed {
            let tail = &body[ex.bytes_consumed..];
            params.extradata = match ex.multitrack {
                Some(mt_type) => default_track_payload(mt_type, ex.fourcc, tail),
                None => tail.to_vec(),
            };
        }
        return Ok(StreamInfo {
            index,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params,
        });
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
    //
    // Not so for the formats whose sample layout *is* those header bits
    // (linear PCM, ADPCM, generic-rate Nellymoser): there the tag header
    // is the fact, and a disagreeing `onMetaData` is reported under
    // `flv.conflict.*` instead of being believed.
    let header_wins = audio_header_is_authoritative(ah.codec_id as u32);
    if !header_wins
        && let Some(sr) = metadata_lookup_u32(metadata, "audiosamplerate")
        && sr > 0
    {
        params.sample_rate = Some(sr);
    }
    // The `onMetaData` `stereo` boolean is normally the more
    // authoritative channel-count source. But E.4.2.1 pins Nellymoser
    // 8/16 kHz mono and Speex to a single channel regardless of any
    // signalling, so a producer that wrongly stamped `stereo = true` for
    // one of those formats must not upgrade them to 2 channels.
    let spec_mono = matches!(
        ah.codec_id,
        AUDIO_CODEC_NELLYMOSER_8K_MONO | AUDIO_CODEC_NELLYMOSER_16K_MONO | AUDIO_CODEC_SPEEX
    );
    if !spec_mono
        && !header_wins
        && let Some(b) = metadata_lookup_bool(metadata, "stereo")
    {
        params.channels = Some(if b { 2 } else { 1 });
    }
    // Likewise the `audiosamplerate` override must not move Speex /
    // Nellymoser 8/16 kHz off their spec-fixed rates (the SoundRate field
    // "cannot represent" those rates, so a producer's `audiosamplerate`
    // for these is advisory at best — the codec id is the truth).
    let spec_fixed_rate = matches!(
        ah.codec_id,
        AUDIO_CODEC_NELLYMOSER_8K_MONO | AUDIO_CODEC_NELLYMOSER_16K_MONO | AUDIO_CODEC_SPEEX
    );
    if spec_fixed_rate {
        params.sample_rate = Some(ah.sample_rate_hz());
    }
    // `audiodatarate` is in kilobits-per-second per E.5.
    if let Some(kbps) = metadata_lookup_f64(metadata, "audiodatarate")
        && kbps.is_finite()
        && kbps >= 0.0
    {
        params.bit_rate = Some((kbps * 1000.0) as u64);
    }
    // `audiosamplesize` (E.5) overrides the SoundSize-derived resolution
    // the same way `audiosamplerate` overrides the SoundRate field: the
    // 1-bit SoundSize "only pertains to uncompressed formats" (E.4.2.1),
    // so for AAC / MP3 / Speex the onMetaData value is the producer's
    // declared truth.
    if !header_wins && let Some(fmt) = metadata_audio_sample_format(metadata) {
        params.sample_format = Some(fmt);
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
        let codec_name = match ex.fourcc {
            Some(fcc) => fourcc_codec_id_str(fcc),
            // ManyTracksManyCodecs case: per-track FourCc is inside the
            // body. We don't split the stream model per track, so surface
            // a sentinel codec id so the resolver gets a stable name to
            // log (mirrors the ExAudio multicodec sentinel).
            None => "flv:exvideo:multicodec".into(),
        };
        let codec = CodecId::new(codec_name);
        let mut params = CodecParameters::video(codec);
        if let Some(w) = metadata_lookup_u32(metadata, "width") {
            params.width = Some(w);
        }
        if let Some(h) = metadata_lookup_u32(metadata, "height") {
            params.height = Some(h);
        }
        if let Some(fps) = metadata_lookup_f64(metadata, "videoframerate")
            .or_else(|| metadata_lookup_f64(metadata, "framerate"))
            && fps.is_finite()
            && fps > 0.0
        {
            params.frame_rate = Some(f64_to_rational(fps));
        }
        if let Some(kbps) = metadata_lookup_f64(metadata, "videodatarate")
            && kbps.is_finite()
            && kbps >= 0.0
        {
            params.bit_rate = Some((kbps * 1000.0) as u64);
        }
        // SequenceStart's body is the codec's decoder-configuration
        // record — route to extradata so consumers find it without
        // peeking at the first packet (consistent with the legacy AVC
        // path below). In multitrack mode the config record lives inside
        // the default track's payload, so we split first and lift
        // trackId 0's bytes.
        if matches!(ex.packet_type, ExPacketType::SequenceStart) && body.len() > ex.bytes_consumed {
            let tail = &body[ex.bytes_consumed..];
            params.extradata = match ex.multitrack {
                Some(mt_type) => default_track_payload(mt_type, ex.fourcc, tail),
                None => tail.to_vec(),
            };
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
        && fps.is_finite()
        && fps > 0.0
    {
        params.frame_rate = Some(f64_to_rational(fps));
    }
    // `videodatarate` is in kilobits-per-second per E.5.
    if let Some(kbps) = metadata_lookup_f64(metadata, "videodatarate")
        && kbps.is_finite()
        && kbps >= 0.0
    {
        params.bit_rate = Some((kbps * 1000.0) as u64);
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

/// `onMetaData` audio properties → parameters, for a stream that has
/// no tag of its own yet.
fn apply_audio_metadata(params: &mut CodecParameters, metadata: &[(String, String)]) {
    if let Some(sr) = metadata_lookup_u32(metadata, "audiosamplerate")
        && sr > 0
    {
        params.sample_rate = Some(sr);
    }
    if let Some(b) = metadata_lookup_bool(metadata, "stereo") {
        params.channels = Some(if b { 2 } else { 1 });
    }
    if let Some(kbps) = metadata_lookup_f64(metadata, "audiodatarate")
        && kbps.is_finite()
        && kbps >= 0.0
    {
        params.bit_rate = Some((kbps * 1000.0) as u64);
    }
    if let Some(fmt) = metadata_audio_sample_format(metadata) {
        params.sample_format = Some(fmt);
    }
}

/// `onMetaData` video properties → parameters, for a stream that has
/// no tag of its own yet.
fn apply_video_metadata(params: &mut CodecParameters, metadata: &[(String, String)]) {
    if let Some(w) = metadata_lookup_u32(metadata, "width") {
        params.width = Some(w);
    }
    if let Some(h) = metadata_lookup_u32(metadata, "height") {
        params.height = Some(h);
    }
    if let Some(fps) = metadata_lookup_f64(metadata, "videoframerate")
        .or_else(|| metadata_lookup_f64(metadata, "framerate"))
        && fps.is_finite()
        && fps > 0.0
    {
        params.frame_rate = Some(f64_to_rational(fps));
    }
    if let Some(kbps) = metadata_lookup_f64(metadata, "videodatarate")
        && kbps.is_finite()
        && kbps >= 0.0
    {
        params.bit_rate = Some((kbps * 1000.0) as u64);
    }
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
    ts: i64,
    body: &[u8],
    ex: Option<&ExAudioTagHeader>,
) -> Option<Packet> {
    if body.is_empty() {
        // Enhanced-RTMP-v2 "audio silence message" (§`AudioPacketType`,
        // SequenceStart/SequenceEnd comments): "This silence message is
        // identified when an audio message contains a zero-length
        // payload, or more precisely, an empty audio message without an
        // AudioTagHeader, indicating a period of silence." The spec
        // gives it explicit playback semantics — play out the buffered
        // audio, flush the audio decoder, and stop using the audio clock
        // as the A/V sync master for the silence period; it is declared
        // to have "no less than the same meaning as" `SequenceEnd`.
        //
        // Surface it as a zero-length header+discard packet at the tag's
        // timestamp so callers can react to the boundary (flush their
        // decoder / switch to wall-clock timing) instead of having the
        // signal silently dropped. `header` marks "this carries no
        // decodable media"; `discard` keeps the empty body from reaching
        // a decoder as a frame.
        let mut pkt = Packet::new(stream.index, stream.time_base, Vec::new());
        pkt.pts = Some(ts);
        pkt.dts = Some(ts);
        pkt.flags.keyframe = true;
        pkt.flags.header = true;
        pkt.flags.discard = true;
        return Some(pkt);
    }
    // Enhanced RTMP / E-FLV ExAudioTagHeader path (SoundFormat=9).
    // Map onto the existing Packet semantics:
    //   AudioPacketType=0 SequenceStart        → header packet (config)
    //   AudioPacketType=1 CodedFrames          → data packet
    //   AudioPacketType=2 SequenceEnd          → skip (no decoder input)
    //   AudioPacketType=4 MultichannelConfig   → header+discard (the
    //     parsed speaker layout is harvested into the metadata bag +
    //     stream channels by `next_packet` before this call)
    //   AudioPacketType=7 ModEx + reserved     → header+discard
    // The outer `Multitrack` (=5) value never appears on
    // `ex.packet_type`: the parser already resolves it down to the
    // inner per-track AudioPacketType and flags the wrapper on
    // `ex.multitrack` (mirrors the video header shape). The default
    // track's body is selected below via `split_tracks`.
    if let Some(ex) = ex {
        if ex.packet_type == ExAudioPacketType::SequenceEnd {
            return None;
        }
        let payload_start = ex.bytes_consumed.min(body.len());
        // Multitrack (enhanced-rtmp-v2 §`ExAudioTagBody`): surface the
        // default track (trackId 0, or the first track) so a multitrack
        // audio file stays decodable through its primary variant instead
        // of being dropped wholesale. `ex.packet_type` already holds the
        // inner per-track packet type (post-Multitrack unwrap).
        let data = if let Some(mt_type) = ex.multitrack {
            let tail = &body[payload_start..];
            let tracks = split_tracks(mt_type, ex.fourcc.unwrap_or(0), tail).ok()?;
            let track = tracks
                .iter()
                .find(|t| t.track_id == 0)
                .or_else(|| tracks.first())?;
            tail[track.payload.clone()].to_vec()
        } else {
            body[payload_start..].to_vec()
        };
        let mut pkt = Packet::new(stream.index, stream.time_base, data);
        pkt.pts = Some(ts);
        pkt.dts = Some(ts);
        // Audio: every packet is independently decodable.
        pkt.flags.keyframe = true;
        match ex.packet_type {
            ExAudioPacketType::SequenceStart => {
                pkt.flags.header = true;
            }
            ExAudioPacketType::CodedFrames => {
                // Data packet — keyframe flag set above.
            }
            ExAudioPacketType::MultichannelConfig
            | ExAudioPacketType::ModEx
            | ExAudioPacketType::Reserved3
            | ExAudioPacketType::Reserved6
            | ExAudioPacketType::Reserved(_) => {
                pkt.flags.header = true;
                pkt.flags.discard = true;
            }
            ExAudioPacketType::Multitrack => {
                // Parser resolves the outer Multitrack down to the
                // inner per-track AudioPacketType, so this variant
                // never reaches the match arm. Surface the
                // unreachable invariant loudly rather than silently
                // discarding any future malformed input.
                unreachable!(
                    "ExAudioTagHeader::parse never surfaces packet_type=Multitrack \
                     (the outer wrapper is captured on `multitrack` instead)"
                );
            }
            ExAudioPacketType::SequenceEnd => unreachable!(),
        }
        return Some(pkt);
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
    pkt.pts = Some(ts);
    pkt.dts = Some(ts);
    pkt.flags.keyframe = true; // audio: every packet is independently decodable
    pkt.flags.header = is_header;
    Some(pkt)
}

/// Build a packet from a Video tag. Same shape as `build_audio_packet`.
fn build_video_packet(
    stream: &StreamInfo,
    ts: i64,
    body: &[u8],
    ex: Option<&ExVideoTagHeader>,
) -> Option<Packet> {
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
    if let Some(ex) = ex {
        if matches!(ex.packet_type, ExPacketType::SequenceEnd) {
            return None;
        }
        let payload_start = ex.bytes_consumed.min(body.len());
        // Multitrack (enhanced-rtmp-v2 §`ExVideoTagBody`): the body after
        // the Ex header is a loop of per-track records. We don't expand
        // the stream model one-per-track; instead we surface the *default
        // track* (trackId 0, or the first track in wire order) so a
        // multitrack file stays decodable through its primary variant
        // rather than being dropped wholesale. `ex.packet_type` already
        // holds the inner per-track packet type (post-Multitrack unwrap).
        let mut mt_cto = ex.composition_time_offset_ms;
        let data = if let Some(cmd) = ex.video_command {
            vec![cmd.as_u8()]
        } else if let Some(mt_type) = ex.multitrack {
            let tail = &body[payload_start..];
            let tracks = split_tracks(mt_type, ex.fourcc.unwrap_or(0), tail).ok()?;
            let track = tracks
                .iter()
                .find(|t| t.track_id == 0)
                .or_else(|| tracks.first())?;
            let mut track_payload = &tail[track.payload.clone()];
            // For AVC/HEVC/VVC CodedFrames the per-track payload is
            // prefixed with the SI24 CompositionTimeOffset (the same
            // framing a single-track CodedFrames body carries).
            if matches!(ex.packet_type, ExPacketType::CodedFrames)
                && matches!(track.fourcc, FOURCC_HVC1 | FOURCC_VVC1 | FOURCC_AVC1)
                && track_payload.len() >= 3
            {
                let raw = ((track_payload[0] as u32) << 16)
                    | ((track_payload[1] as u32) << 8)
                    | (track_payload[2] as u32);
                let sext = if raw & 0x0080_0000 != 0 {
                    raw | 0xFF00_0000
                } else {
                    raw
                };
                mt_cto = Some(sext as i32);
                track_payload = &track_payload[3..];
            }
            track_payload.to_vec()
        } else {
            // Command-frame sentinel: when frame_type == Command the body
            // contains exactly one UI8 (VideoCommand) and no codec payload
            // (handled above). Otherwise the body tail is the codec
            // payload. Down-stream callers can match on
            // `VideoCommand::from_u8(pkt.data[0])` if they want to react
            // to the seek-sequence boundary.
            body[payload_start..].to_vec()
        };
        let dts = ts;
        let pts = dts + mt_cto.unwrap_or(0) as i64;
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
        return Some(pkt);
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
        pkt.pts = Some(ts);
        pkt.dts = Some(ts);
        pkt.flags.header = true;
        pkt.flags.discard = true;
        // FrameType=5 is not a random-access point.
        pkt.flags.keyframe = false;
        return Some(pkt);
    }

    let mut payload_offset: usize = 1;
    let mut pts = ts;
    let dts = ts;
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
    Some(pkt)
}

/// Build a packet from a filtered (encrypted) tag. The encrypted
/// payload follows the `EncryptionTagHeader` + `FilterParams` preamble
/// (spec F.3.1 / F.3.2). We don't decrypt — the body is forwarded with
/// `flags.discard = true` so downstream consumers know not to feed it to
/// a decoder. Returns `Ok(None)` if the tag is for a stream type we
/// don't have (e.g. an encrypted video tag in an audio-only file).
fn build_encrypted_packet(stream: &StreamInfo, ts: i64, body: &[u8]) -> Result<Option<Packet>> {
    let preamble = EncryptedTagPreamble::parse(body)?;
    if preamble.bytes_consumed > body.len() {
        return Ok(None);
    }
    let cipher = body[preamble.bytes_consumed..].to_vec();
    let mut pkt = Packet::new(stream.index, stream.time_base, cipher);
    pkt.pts = Some(ts);
    pkt.dts = Some(ts);
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
    embedded_images: &mut Vec<EmbeddedImage>,
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
    // A stream can carry script tags for as long as it runs (captions,
    // cue points, status); the bag stops growing at a fixed size.
    if metadata.len() >= MAX_METADATA_ENTRIES {
        return;
    }
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
            //
            // Cue points are appended in order, so the newest
            // `cuepoint.<n>.` entry (found from the back) carries the
            // highest index.
            let n = metadata
                .iter()
                .rev()
                .find_map(|(k, _)| {
                    let rest = k.strip_prefix("cuepoint.")?;
                    rest.split(['.', '[']).next()?.parse::<u32>().ok()
                })
                .map_or(0, |m| m.saturating_add(1));
            flatten_amf_value(&value, &format!("cuepoint.{n}"), metadata);
        }
        "onImageData" => {
            // Annex B.2 "Image Metadata": when an F4V/FLV sample is an
            // image type (GIF / PNG / JPEG), the producer surfaces the
            // original compressed image file through an `onImageData`
            // script tag carrying `data` (a BYTEARRAY of the file bytes)
            // and `trackid` (a DOUBLE naming the owning track). The image
            // bytes are binary and have no representation in the string
            // metadata bag, so they are collected into a dedicated
            // side-channel (`FlvDemuxer::embedded_images`). A `data`
            // BYTEARRAY only exists in AMF3, so the value is the `0x11`
            // AVM+ object wrapping an AMF3 Object/Array carrying the two
            // properties. We additionally lift the trackid + byte length
            // into the metadata bag under an `onimagedata.<n>.` prefix so
            // a caller scanning only the bag still sees that an image is
            // present.
            if let Some(img) = parse_on_image_data(&value) {
                let n = embedded_images.len();
                if let Some(tid) = img.track_id {
                    metadata.push((format!("onimagedata.{n}.trackid"), tid.to_string()));
                }
                metadata.push((
                    format!("onimagedata.{n}.length"),
                    img.data.len().to_string(),
                ));
                embedded_images.push(img);
            }
        }
        "|AdditionalHeader" => {
            *encryption = parse_additional_header(&value);
        }
        _ => {
            // Unknown script-tag name — preserve both the name AND the
            // value payload so callers see the full script body. FLV
            // spec v10.1 enumerates only four spec-defined names
            // (`onMetaData` E.5, `onXMPData` E.6, `onCuePoint` Annex A,
            // `|AdditionalHeader` F.2.1). Enhanced-RTMP-v2's
            // §"Enhancing onMetaData" describes the ScriptTagBody as
            // *encapsulating method invocations* (method name + single
            // argument), so producers in the wild legitimately emit
            // additional method names (live-caption tracks, producer
            // telemetry, RTMP-relayed status snapshots, …).
            //
            // The name is surfaced under the stable `scriptdata.name`
            // sentinel (unchanged from before). The argument value is
            // additionally lifted through `flatten_amf_value` under a
            // `scriptdata.<name>.<...>` prefix so its full structure
            // reaches the metadata bag — scalars land directly under
            // `scriptdata.<name>`, composite values fan out with
            // `.<subkey>` / `[i]` suffixes per the existing flatten
            // schema. Previously the value was silently dropped, which
            // was a data-loss bug for every producer-defined script tag
            // a non-spec FLV reader might want to inspect.
            metadata.push(("scriptdata.name".into(), name_str.clone()));
            flatten_amf_value(&value, &format!("scriptdata.{name_str}"), metadata);
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
    //
    // TypedObject (AMF0 §2.18) is also accepted because some servers
    // (FMS / Wowza relays) wrap the whole `onMetaData` payload in a
    // class-aliased object before forwarding; the property body is the
    // same as for an anonymous Object. The class alias itself is
    // surfaced under `scriptdata.class` so callers can see which
    // producer-registered type emitted the metadata.
    let entries = match value {
        AmfValue::Object(v) | AmfValue::EcmaArray(v) => v.as_slice(),
        AmfValue::TypedObject { class_name, body } => {
            metadata.push(("scriptdata.class".into(), class_name.clone()));
            body.as_slice()
        }
        _ => return,
    };
    for (k, v) in entries {
        if metadata.len() >= MAX_METADATA_ENTRIES {
            break;
        }
        match v {
            AmfValue::Number(n) => {
                // Duration is in seconds — convert to microseconds and
                // store both a string form (for metadata) and the numeric
                // form (for `duration_micros`).
                // (A duration of 0 is what a writer leaves behind when
                // it could not come back to fill the real one in.)
                if k == "duration" && duration_micros.is_none() && *n > 0.0 {
                    let micros = (*n * 1_000_000.0).round();
                    if micros.is_finite() && micros >= 0.0 && micros < i64::MAX as f64 {
                        *duration_micros = Some(micros as i64);
                    }
                }
                metadata.push((k.clone(), format_number(*n)));
            }
            AmfValue::Boolean(b) => metadata.push((k.clone(), b.to_string())),
            AmfValue::String(s) => metadata.push((k.clone(), s.clone())),
            AmfValue::Object(_) | AmfValue::EcmaArray(_) | AmfValue::TypedObject { .. }
                if k == "keyframes" && keyframe_index.is_none() =>
            {
                *keyframe_index = parse_keyframes_object(v);
            }
            // Enhanced-RTMP-v2 §"Enhancing onMetaData": the new
            // `audioTrackIdInfoMap` / `videoTrackIdInfoMap` properties carry
            // per-track metadata for additional (non-default) tracks in a
            // multitrack stream. Each is an object keyed by trackId (1, 2,
            // …; trackId 0 is the default track described by the top-level
            // onMetaData fields) whose value is a per-track property object
            // (width / height / videodatarate / channels / samplerate /
            // codec id / …). Flatten the whole map into the metadata bag
            // under a stable lowercased prefix so callers can read e.g.
            // `videotrackidinfomap.1.width` without an AMF model. The exact
            // per-track field set is producer-defined (delta-style or full),
            // so a structural flatten — rather than a fixed schema — is the
            // right model.
            AmfValue::Object(_) | AmfValue::EcmaArray(_) | AmfValue::TypedObject { .. }
                if k == "audioTrackIdInfoMap" || k == "videoTrackIdInfoMap" =>
            {
                flatten_amf_value(v, &k.to_ascii_lowercase(), metadata);
            }
            // Any other top-level property (Date, Null, Undefined,
            // Unsupported, Reference, Xml, StrictArray, AvmPlus, or a
            // nested Object/EcmaArray/TypedObject whose key isn't one
            // of the explicitly-handled ones above) used to be silently
            // dropped. Adobe's *Flash Video File Format Specification*
            // v10.1 Annex B "Stream Properties" enumerates a fixed
            // schema (avclevel / avcprofile / moovposition / ...) but
            // observed FLV producers in the wild also emit:
            //   * `metadatacreator`, `encoder`, `creationdate` as
            //     strings (handled) and as `Date` (was dropped);
            //   * HDR / color-config sub-objects at top level (not
            //     just under the E-FLV `colorInfo` envelope);
            //   * `filepositions` / `times` arrays outside the
            //     `keyframes` container (some Wowza variants);
            //   * AMF3 sub-trees behind the `0x11` AVM+ marker for
            //     producers that switch encodings mid-onMetaData.
            // Flatten every other variant through `flatten_amf_value`
            // under its original property name so callers reading the
            // metadata bag see the producer's actual data instead of
            // an empty key set. Scalars land directly under `k`;
            // composite values gain `k.<subkey>` or `k[i]` suffixes.
            _ => flatten_amf_value(v, k, metadata),
        }
    }
}

#[allow(non_snake_case)] // mirrors the spec property name verbatim
fn xmp_liveXML(value: &AmfValue) -> Option<String> {
    // Per E.6 the XMP object has exactly one property: liveXML.
    let entries: &[(String, AmfValue)] = match value {
        AmfValue::Object(b) | AmfValue::EcmaArray(b) => b,
        AmfValue::TypedObject { body, .. } => body,
        // Some producers nest the string directly (legitimate per E.6
        // because `liveXML` is "an XML string"); an AMF0 `XMLDocument`
        // marker (§2.17) is the same payload shape with a typed wire
        // marker — accept both.
        AmfValue::String(s) | AmfValue::Xml(s) => return Some(s.clone()),
        _ => return None,
    };
    for (k, v) in entries {
        if k == "liveXML"
            && let AmfValue::String(s) | AmfValue::Xml(s) = v
        {
            return Some(s.clone());
        }
    }
    None
}

/// Parse an `onImageData` argument value (Annex B.2) into an
/// [`EmbeddedImage`].
///
/// Annex B.2 lists two properties: `data` (a BYTEARRAY of the
/// compressed image file) and `trackid` (a DOUBLE). A BYTEARRAY only
/// exists in AMF3, so the argument is the AMF0 `0x11` AVM+ marker
/// wrapping an AMF3 Object (or ECMA-style Array) carrying the two
/// properties. Returns `None` when the value carries no recoverable
/// `data` byte payload — the trackid alone is not an image.
fn parse_on_image_data(value: &AmfValue) -> Option<EmbeddedImage> {
    let inner = match value {
        AmfValue::AvmPlus(inner) => inner.as_ref(),
        _ => return None,
    };
    let data = amf3_property(inner, "data").and_then(|v| match v {
        Amf3Value::ByteArray(b) => Some(b.clone()),
        _ => None,
    })?;
    let track_id = amf3_property(inner, "trackid")
        .and_then(Amf3Value::as_f64)
        .filter(|n| n.is_finite() && (0.0..=u32::MAX as f64).contains(n))
        .map(|n| n as u32);
    Some(EmbeddedImage { track_id, data })
}

/// Look a named property up in an AMF3 Object (sealed or dynamic
/// member) or in the associative portion of an AMF3 Array.
fn amf3_property<'a>(value: &'a Amf3Value, name: &str) -> Option<&'a Amf3Value> {
    match value {
        Amf3Value::Object(Amf3Object {
            sealed_names,
            sealed_values,
            dynamic_members,
            ..
        }) => sealed_names
            .iter()
            .position(|n| n == name)
            .map(|i| &sealed_values[i])
            .or_else(|| {
                dynamic_members
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v)
            }),
        Amf3Value::Array(Amf3Array { assoc, .. }) => {
            assoc.iter().find(|(k, _)| k == name).map(|(_, v)| v)
        }
        _ => None,
    }
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
    if out.len() >= MAX_METADATA_ENTRIES {
        return;
    }
    match value {
        AmfValue::Number(n) => out.push((prefix.into(), format_number(*n))),
        AmfValue::Boolean(b) => out.push((prefix.into(), b.to_string())),
        AmfValue::String(s) => out.push((prefix.into(), s.clone())),
        AmfValue::Null => out.push((prefix.into(), "null".into())),
        AmfValue::Undefined => out.push((prefix.into(), "undefined".into())),
        AmfValue::Unsupported => out.push((prefix.into(), "unsupported".into())),
        AmfValue::Reference(idx) => out.push((prefix.into(), format!("ref:{idx}"))),
        AmfValue::Xml(s) => out.push((prefix.into(), s.clone())),
        AmfValue::Date { time_ms, tz } => {
            out.push((prefix.into(), format!("date:{time_ms}tz:{tz}")));
        }
        AmfValue::Object(b) | AmfValue::EcmaArray(b) => {
            for (k, v) in b {
                flatten_amf_value(v, &format!("{prefix}.{k}"), out);
            }
        }
        AmfValue::TypedObject { class_name, body } => {
            // Surface the class alias under a `.class` sentinel so a
            // caller can identify the producer-registered type, then
            // flatten the property body like any other object.
            out.push((format!("{prefix}.class"), class_name.clone()));
            for (k, v) in body {
                flatten_amf_value(v, &format!("{prefix}.{k}"), out);
            }
        }
        AmfValue::StrictArray(items) => {
            for (i, v) in items.iter().enumerate() {
                flatten_amf_value(v, &format!("{prefix}[{i}]"), out);
            }
        }
        AmfValue::AvmPlus(inner) => {
            flatten_amf3_value(inner, prefix, out);
        }
    }
}

/// Flatten an AMF3 value into the same `(key, string)` shape as
/// [`flatten_amf_value`] uses for AMF0 — so callers consuming the
/// `metadata[...]` bag see AVM+ payloads under the same prefix scheme.
fn flatten_amf3_value(value: &Amf3Value, prefix: &str, out: &mut Vec<(String, String)>) {
    if out.len() >= MAX_METADATA_ENTRIES {
        return;
    }
    match value {
        Amf3Value::Undefined => out.push((prefix.into(), "undefined".into())),
        Amf3Value::Null => out.push((prefix.into(), "null".into())),
        Amf3Value::Boolean(b) => out.push((prefix.into(), b.to_string())),
        Amf3Value::Integer(i) => out.push((prefix.into(), i.to_string())),
        Amf3Value::Double(d) => out.push((prefix.into(), format_number(*d))),
        Amf3Value::String(s) => out.push((prefix.into(), s.clone())),
        Amf3Value::Xml(s) | Amf3Value::XmlDocument(s) => out.push((prefix.into(), s.clone())),
        Amf3Value::Date(ms) => out.push((prefix.into(), format!("date:{ms}"))),
        Amf3Value::ByteArray(bytes) => {
            out.push((prefix.into(), format!("bytearray:{}", bytes.len())));
        }
        Amf3Value::Array(Amf3Array { assoc, dense }) => {
            for (k, v) in assoc {
                flatten_amf3_value(v, &format!("{prefix}.{k}"), out);
            }
            for (i, v) in dense.iter().enumerate() {
                flatten_amf3_value(v, &format!("{prefix}[{i}]"), out);
            }
        }
        Amf3Value::Object(Amf3Object {
            class_name,
            externalizable,
            sealed_names,
            sealed_values,
            dynamic_members,
            ..
        }) => {
            if !class_name.is_empty() {
                out.push((format!("{prefix}.class"), class_name.clone()));
            }
            if *externalizable {
                out.push((format!("{prefix}.externalizable"), "true".into()));
            }
            for (name, val) in sealed_names.iter().zip(sealed_values.iter()) {
                flatten_amf3_value(val, &format!("{prefix}.{name}"), out);
            }
            for (k, v) in dynamic_members {
                flatten_amf3_value(v, &format!("{prefix}.{k}"), out);
            }
        }
    }
}

/// Harvest an Enhanced-RTMP `VideoPacketType.Metadata` body into the
/// metadata bag, per Veovera `enhanced-rtmp-v2` §"Metadata Frame".
///
/// The body that follows the Ex video tag header (when
/// `videoPacketType == VideoPacketType.Metadata`) carries **no video
/// data**: instead it is a series of AMF0 `[name, value]` pairs, encoded
/// exactly like a `SCRIPTDATA` payload (the spec defers to the FLV
/// `SCRIPTDATAVALUE` description for the wire layout). The only pair
/// currently defined is `["colorInfo", Object]`, whose nested
/// `colorConfig` / `hdrCll` / `hdrMdcv` sub-objects describe BT.2020 HDR
/// metadata.
///
/// Each pair is flattened under a `<name-lowercased>.…` prefix using the
/// shared [`flatten_amf_value`] walker — so `colorInfo.colorConfig.
/// transferCharacteristics` lands as `colorinfo.colorConfig.
/// transferCharacteristics`. Per spec each new `colorInfo` "invalidates
/// and replaces the current one", and a value of `Undefined` (or an
/// empty object) resets to the original color state; we model that by
/// dropping every prior entry under the same prefix before flattening the
/// new value. A reset therefore clears the bag (an `Undefined` value
/// flattens to a single `colorinfo = undefined` sentinel; an empty object
/// flattens to nothing).
///
/// Unknown pair names (none are spec-defined yet) are flattened verbatim
/// under their own lowercased prefix so a future-defined metadata name
/// surfaces rather than being silently dropped.
fn harvest_video_metadata_frame(body: &[u8], out: &mut Vec<(String, String)>) {
    let mut pos = 0usize;
    while pos < body.len() {
        // name = AMF0 value (always a String in practice, but parse it as
        // a value so a malformed name aborts the walk cleanly rather than
        // mis-slicing the body).
        let (name, np) = match parse_amf0_value(body, pos) {
            Ok(v) => v,
            Err(_) => return,
        };
        let name = match name.as_str() {
            Some(s) => s.to_string(),
            None => return,
        };
        let (value, vp) = match parse_amf0_value(body, np) {
            Ok(v) => v,
            Err(_) => return,
        };
        // Forward progress guard — a zero-advance parse would loop forever.
        if vp <= pos {
            return;
        }
        pos = vp;

        // `colorInfo` (the only spec-defined name) lands under a stable
        // lowercase `colorinfo` prefix; every other name keeps its own
        // lowercased prefix so future metadata names are visible.
        let prefix = name.to_ascii_lowercase();
        // A new value replaces the prior one for this name (spec:
        // "invalidates and replaces the current one"; Undefined / {} reset).
        out.retain(|(k, _)| k != &prefix && !k.starts_with(&format!("{prefix}.")));
        flatten_amf_value(&value, &prefix, out);
    }
}

/// Harvest an Enhanced-RTMP-v2 `AudioPacketType.MultichannelConfig`
/// payload (enhanced-rtmp-v2 §`ExAudioTagBody`) into the metadata bag
/// and onto the audio stream's `CodecParameters`.
///
/// Keys (latest config wins — a new signal supersedes the prior one, so
/// every prior `multichannelconfig.*` entry is dropped first):
///
/// * `multichannelconfig.order` — `"unspecified"` / `"native"` /
///   `"custom"` / `"reserved:<n>"`.
/// * `multichannelconfig.channelcount` — the UI8 channel count.
/// * `multichannelconfig.flags` (`Native` only) — the raw UI32
///   `audioChannelFlags` mask as `0x<8-hex-digits>`.
/// * `multichannelconfig.layout` (`Native` only) — comma-joined speaker
///   labels for every spec-defined mask bit set, in mask-bit order
///   (e.g. `"frontleft,frontright,frontcenter,lowfrequency1,backleft,
///   backright"` for 5.1).
/// * `multichannelconfig.mapping` (`Custom` only) — comma-joined
///   per-channel speaker labels in bitstream order (`"unused"` /
///   `"unknown"` / `"reserved:<n>"` for the special values).
///
/// The channel count also overrides `CodecParameters::channels` when
/// non-zero — the spec defines this signal as the channel-mapping truth
/// for codecs that are not self-describing (the `onMetaData` `stereo`
/// boolean can only express 1 or 2). A malformed body is skipped
/// silently: side-channel info must not kill the stream, and the tag is
/// still surfaced as a header+discard packet either way.
fn harvest_multichannel_config(
    payload: &[u8],
    out: &mut Vec<(String, String)>,
    stream: &mut StreamInfo,
) {
    let mcc = match MultichannelConfig::parse(payload) {
        Ok(m) => m,
        Err(_) => return,
    };
    out.retain(|(k, _)| k != "multichannelconfig" && !k.starts_with("multichannelconfig."));
    out.push(("multichannelconfig.order".into(), mcc.order.label()));
    out.push((
        "multichannelconfig.channelcount".into(),
        mcc.channel_count.to_string(),
    ));
    if let Some(flags) = mcc.channel_flags {
        out.push(("multichannelconfig.flags".into(), format!("0x{flags:08X}")));
        out.push((
            "multichannelconfig.layout".into(),
            mask_channel_labels(flags).join(","),
        ));
    }
    if let Some(mapping) = &mcc.mapping {
        out.push((
            "multichannelconfig.mapping".into(),
            mapping
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(","),
        ));
    }
    if mcc.channel_count > 0 {
        stream.params.channels = Some(mcc.channel_count as u16);
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
        if k == key
            && let Ok(n) = v.parse::<f64>()
            && n.is_finite()
            && n >= 0.0
            && n <= u32::MAX as f64
        {
            return Some(n as u32);
        }
    }
    None
}

fn metadata_lookup_f64(metadata: &[(String, String)], key: &str) -> Option<f64> {
    for (k, v) in metadata {
        if k == key
            && let Ok(n) = v.parse::<f64>()
            && n.is_finite()
        {
            return Some(n);
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

/// Map the spec E.5 `audiosamplesize` onMetaData field ("Resolution of a
/// single audio sample", in bits) to a [`SampleFormat`]. Adobe's encoders
/// only ever emit 8 or 16 here — the legacy `SoundSize` field can encode
/// only those two values (E.4.2.1) and `audiosamplesize` is its
/// onMetaData mirror — so we recognise exactly 8 (→ `U8`) and 16 (→
/// `S16`). Any other value is unrecognised and returns `None` rather than
/// inventing a format. Returns `None` for absent / non-numeric entries.
fn metadata_audio_sample_format(metadata: &[(String, String)]) -> Option<SampleFormat> {
    match metadata_lookup_u32(metadata, "audiosamplesize")? {
        8 => Some(SampleFormat::U8),
        16 => Some(SampleFormat::S16),
        _ => None,
    }
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
    fn empty_audio_tag_emits_silence_message() {
        // Enhanced-RTMP-v2 "audio silence message": an audio tag with a
        // zero-length payload (no AudioTagHeader byte) signals a period
        // of silence. The demuxer must surface it as a zero-length
        // header+discard packet at the tag's timestamp — not drop it,
        // and not abort the stream.
        let mp3_body = {
            let flags = (2u8 << 4) | (2 << 2) | 0x02 | 0x01; // mp3, 22 kHz, 16-bit, stereo
            let mut v = vec![flags];
            v.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
            v
        };
        let audio_tag = make_tag(0x08, 0, &mp3_body);
        // The silence message: an audio tag with a zero-length body.
        let silence_tag = make_tag(0x08, 120, &[]);
        // A normal audio frame after the silence period resumes playback.
        // Leading byte is the legacy AudioTagHeader (mp3, 22 kHz, 16-bit,
        // stereo); the remaining bytes are the coded payload.
        let resume_tag = make_tag(0x08, 240, &[mp3_body[0], 0xDE, 0xAD]);
        let flv = make_flv(&[&audio_tag, &silence_tag, &resume_tag]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.streams().len(), 1);
        assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mp3");

        // First the real audio frame.
        let p1 = dmx.next_packet().unwrap();
        assert_eq!(p1.stream_index, 0);
        assert_eq!(p1.pts, Some(0));
        assert_eq!(p1.data, vec![0xAA, 0xBB, 0xCC]);
        assert!(!p1.flags.discard);

        // Then the silence message: empty body, header+discard, correct ts.
        let silence = dmx.next_packet().unwrap();
        assert_eq!(silence.stream_index, 0);
        assert_eq!(silence.pts, Some(120));
        assert_eq!(silence.dts, Some(120));
        assert!(silence.data.is_empty(), "silence carries no media bytes");
        assert!(silence.flags.header, "silence is a non-media signal");
        assert!(
            silence.flags.discard,
            "silence must not reach a decoder as a frame"
        );

        // Playback resumes with the next real frame — the silence message
        // did not abort the stream.
        let p3 = dmx.next_packet().unwrap();
        assert_eq!(p3.pts, Some(240));
        assert_eq!(p3.data, vec![0xDE, 0xAD]);

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
    fn audiosamplesize_sets_sample_format_on_legacy_aac() {
        // AAC carries no usable SoundSize (the field "only pertains to
        // uncompressed formats"); audiosamplesize=16 (E.5) is the
        // producer's declared resolution → S16.
        let script_tag = make_tag(0x12, 0, &on_metadata_with_property("audiosamplesize", 16.0));
        // AAC tag: codec=10, rate idx=3, SoundSize bit=0 (8-bit) — the
        // metadata override must win over the header bit. Mono (Type=1
        // bit kept; SoundSize bit elided as it is 0).
        let flags = (10 << 4) | (3 << 2) | 0x01;
        let audio_body = vec![flags as u8, 0x01, 0xAA];
        let audio_tag = make_tag(0x08, 0, &audio_body);
        let flv = make_flv(&[&script_tag, &audio_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(
            dmx.streams()[0].params.sample_format,
            Some(SampleFormat::S16)
        );
    }

    #[test]
    fn audiosamplesize_8_overrides_soundsize_to_u8() {
        // MP3 tag with SoundSize=1 (16-bit) but onMetaData
        // audiosamplesize=8 → the declared 8-bit resolution wins (U8).
        let script_tag = make_tag(0x12, 0, &on_metadata_with_property("audiosamplesize", 8.0));
        let flags = (2 << 4) | (2 << 2) | 0x02 | 0x01;
        let audio_body = vec![flags as u8, 0xAA, 0xBB];
        let audio_tag = make_tag(0x08, 0, &audio_body);
        let flv = make_flv(&[&script_tag, &audio_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(
            dmx.streams()[0].params.sample_format,
            Some(SampleFormat::U8)
        );
    }

    #[test]
    fn audiosamplesize_unrecognised_leaves_header_format_intact() {
        // audiosamplesize=24 is not a value Adobe's SoundSize bit can
        // represent — the override is declined and the SoundSize-derived
        // S16 (16-bit bit set) is preserved rather than invented away.
        let script_tag = make_tag(0x12, 0, &on_metadata_with_property("audiosamplesize", 24.0));
        let flags = (2 << 4) | (2 << 2) | 0x02 | 0x01; // SoundSize bit = 16-bit
        let audio_body = vec![flags as u8, 0xAA, 0xBB];
        let audio_tag = make_tag(0x08, 0, &audio_body);
        let flv = make_flv(&[&script_tag, &audio_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(
            dmx.streams()[0].params.sample_format,
            Some(SampleFormat::S16)
        );
    }

    #[test]
    fn audiosamplesize_sets_sample_format_on_ex_stream() {
        // ExAudio repurposes the SoundSize bit as AudioPacketType, so
        // audiosamplesize is the only resolution source; 16 → S16.
        let script_tag = make_tag(0x12, 0, &on_metadata_with_property("audiosamplesize", 16.0));
        let mut audio_body = vec![0x90]; // SoundFormat=9 (ExHeader), SequenceStart
        audio_body.extend_from_slice(b"Opus");
        audio_body.extend_from_slice(b"OpusHead");
        let audio_tag = make_tag(0x08, 0, &audio_body);
        let flv = make_flv(&[&script_tag, &audio_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        let s = &dmx.streams()[0];
        assert_eq!(s.params.codec_id.as_str(), "opus");
        assert_eq!(s.params.sample_format, Some(SampleFormat::S16));
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
        assert!(
            md.iter()
                .any(|(k, v)| k == "encryption.algorithm" && v == "AES-CBC")
        );
        assert!(
            md.iter()
                .any(|(k, v)| k == "encryption.key_subtype" && v == "FlashAccessv2")
        );
        assert!(
            md.iter()
                .any(|(k, v)| k == "encryption.version" && v == "2")
        );
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
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "xmp" && v == live_xml)
        );
    }

    #[test]
    fn on_metadata_typed_object_wraps_payload_with_class_alias() {
        // Some servers (FMS / Wowza relays) re-encode `onMetaData` as a
        // TypedObject (AMF0 marker 0x10) carrying the producer's
        // registered class alias. The property body shape is identical
        // to an anonymous Object — the parser must look through the
        // alias and still extract `videodatarate`, etc., AND surface
        // the alias under `scriptdata.class` so callers can see it.
        let class = "flex.messaging.io.ArrayCollection";
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onMetaData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        // TypedObject(class, { videodatarate: 768.0 })
        body.push(0x10);
        body.extend_from_slice(&(class.len() as u16).to_be_bytes());
        body.extend_from_slice(class.as_bytes());
        body.extend_from_slice(&("videodatarate".len() as u16).to_be_bytes());
        body.extend_from_slice(b"videodatarate");
        body.push(0x00);
        body.extend_from_slice(&768.0_f64.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![(1u8 << 4) | 4, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        // bit_rate lifted out of the typed-object body.
        assert_eq!(dmx.streams()[0].params.bit_rate, Some(768_000));
        // Class alias surfaced for the caller.
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "scriptdata.class" && v == class)
        );
    }

    #[test]
    fn on_xmp_data_accepts_xmldocument_marker_payload() {
        // §2.17: a producer is free to encode the `liveXML` value as an
        // AMF0 XMLDocument (marker 0x0F + u32 length + UTF-8 bytes)
        // instead of an ordinary String. We accept both.
        let live_xml = "<x:xmpmeta>hello</x:xmpmeta>";
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onXMPData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onXMPData");
        body.push(0x03); // Object
        body.extend_from_slice(&("liveXML".len() as u16).to_be_bytes());
        body.extend_from_slice(b"liveXML");
        body.push(0x0F); // XMLDocument
        body.extend_from_slice(&(live_xml.len() as u32).to_be_bytes());
        body.extend_from_slice(live_xml.as_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![(1u8 << 4) | 4, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "xmp" && v == live_xml)
        );
    }

    #[test]
    fn on_metadata_unsupported_value_does_not_drop_neighbouring_fields() {
        // A property whose value the producer marked AMF0 Unsupported
        // (marker 0x0D, §2.15) must not poison the rest of the
        // metadata object. Build {a: <Unsupported>, videodatarate: 256.0}
        // and confirm `videodatarate` still lifts into `bit_rate`.
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onMetaData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        body.push(0x08); // ECMA array
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&("a".len() as u16).to_be_bytes());
        body.extend_from_slice(b"a");
        body.push(0x0D); // Unsupported
        body.extend_from_slice(&("videodatarate".len() as u16).to_be_bytes());
        body.extend_from_slice(b"videodatarate");
        body.push(0x00);
        body.extend_from_slice(&256.0_f64.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![(1u8 << 4) | 4, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.streams()[0].params.bit_rate, Some(256_000));
    }

    #[test]
    fn on_metadata_date_field_lands_in_metadata_bag() {
        // Adobe FLV Annex E.5 lets producers stamp `creationdate` /
        // `metadatacreationdate` as an AMF0 Date (marker 0x0B, §2.13:
        // double millis since epoch + i16 UTC-offset). Producers in
        // the wild (FMS, OBS post-2024) emit this. Prior versions of
        // `parse_on_metadata` matched only Number/Boolean/String at
        // top level and silently dropped Date, hiding the timestamp
        // from `Demuxer::metadata`. The fallthrough flatten now
        // surfaces it under the original property name.
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onMetaData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        body.push(0x08); // ECMA array
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&("creationdate".len() as u16).to_be_bytes());
        body.extend_from_slice(b"creationdate");
        body.push(0x0B); // Date marker
        body.extend_from_slice(&1_700_000_000_000.0_f64.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes()); // UTC offset
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![(1u8 << 4) | 4, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        // The Date string-form is `date:<millis>tz:<offset>` per
        // `flatten_amf_value` — assert presence under the original key.
        assert!(
            dmx.metadata().iter().any(|(k, v)| k == "creationdate"
                && v.starts_with("date:1700000000000")
                && v.ends_with("tz:0")),
            "creationdate not surfaced; metadata={:?}",
            dmx.metadata()
        );
    }

    #[test]
    fn on_metadata_nested_object_outside_known_schema_is_flattened() {
        // A producer-defined top-level sub-object (e.g. a colour-config
        // pack at the FLV-scriptdata level rather than under the E-FLV
        // `colorInfo` AMF envelope). It is neither `keyframes` nor an
        // Enhanced-RTMP track-info map, so the prior code dropped it.
        // The fallthrough flatten now lifts every leaf into the bag
        // under `<key>.<subkey>` paths.
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onMetaData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        body.push(0x08); // outer ECMA array
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&("producerInfo".len() as u16).to_be_bytes());
        body.extend_from_slice(b"producerInfo");
        body.push(0x03); // Object marker
        body.extend_from_slice(&("name".len() as u16).to_be_bytes());
        body.extend_from_slice(b"name");
        body.push(0x02);
        body.extend_from_slice(&("obs-32.0".len() as u16).to_be_bytes());
        body.extend_from_slice(b"obs-32.0");
        body.extend_from_slice(&("buildno".len() as u16).to_be_bytes());
        body.extend_from_slice(b"buildno");
        body.push(0x00);
        body.extend_from_slice(&42.0_f64.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x09]); // close inner Object
        body.extend_from_slice(&[0x00, 0x00, 0x09]); // close outer ECMA
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![(1u8 << 4) | 4, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        let md = dmx.metadata();
        assert!(
            md.iter()
                .any(|(k, v)| k == "producerInfo.name" && v == "obs-32.0"),
            "producerInfo.name missing; metadata={:?}",
            md
        );
        assert!(
            md.iter()
                .any(|(k, v)| k == "producerInfo.buildno" && v == "42"),
            "producerInfo.buildno missing; metadata={:?}",
            md
        );
    }

    #[test]
    fn on_metadata_null_and_undefined_sentinels_preserved() {
        // AMF0 Null (0x05, §2.7) and Undefined (0x06, §2.8) at top
        // level used to be silently dropped. Callers comparing two
        // FLVs need to see that the producer explicitly emitted a
        // null sentinel for a field rather than omitting it; surface
        // both under string-form sentinels.
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onMetaData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        body.push(0x08); // ECMA array
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&("subtitle".len() as u16).to_be_bytes());
        body.extend_from_slice(b"subtitle");
        body.push(0x05); // Null
        body.extend_from_slice(&("rating".len() as u16).to_be_bytes());
        body.extend_from_slice(b"rating");
        body.push(0x06); // Undefined
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![(1u8 << 4) | 4, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        let md = dmx.metadata();
        assert!(
            md.iter().any(|(k, v)| k == "subtitle" && v == "null"),
            "subtitle=null missing; metadata={:?}",
            md
        );
        assert!(
            md.iter().any(|(k, v)| k == "rating" && v == "undefined"),
            "rating=undefined missing; metadata={:?}",
            md
        );
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

    // --- AMF0 encode helpers for the colorInfo metadata-frame tests ----------

    fn amf_string(s: &str) -> Vec<u8> {
        let mut v = vec![0x02];
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn amf_number(n: f64) -> Vec<u8> {
        let mut v = vec![0x00];
        v.extend_from_slice(&n.to_be_bytes());
        v
    }

    /// AMF0 object: `(u16-len key, value)* 0x00 0x00 0x09`.
    fn amf_object(fields: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut v = vec![0x03];
        for (k, val) in fields {
            v.extend_from_slice(&(k.len() as u16).to_be_bytes());
            v.extend_from_slice(k.as_bytes());
            v.extend_from_slice(val);
        }
        v.extend_from_slice(&[0x00, 0x00, 0x09]);
        v
    }

    fn meta_lookup<'a>(md: &'a [(String, String)], key: &str) -> Option<&'a str> {
        md.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn ex_video_metadata_colorinfo_flattens_into_metadata() {
        // Real AMF colorInfo metadata frame per enhanced-rtmp-v2
        // §"Metadata Frame": a [name, value] pair ["colorInfo", Object]
        // with nested colorConfig / hdrCll / hdrMdcv sub-objects.
        let color_config = amf_object(&[
            ("bitDepth", amf_number(10.0)),
            ("colorPrimaries", amf_number(9.0)),
            ("transferCharacteristics", amf_number(16.0)),
            ("matrixCoefficients", amf_number(9.0)),
        ]);
        let hdr_cll = amf_object(&[
            ("maxFall", amf_number(400.0)),
            ("maxCLL", amf_number(1000.0)),
        ]);
        let color_info = amf_object(&[("colorConfig", color_config), ("hdrCll", hdr_cll)]);
        let mut amf = amf_string("colorInfo");
        amf.extend_from_slice(&color_info);

        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"hvc1");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x09, 0, &seq_body);

        let mut meta_body = vec![0x94]; // FrameType=key + PacketType=Metadata
        meta_body.extend_from_slice(b"hvc1");
        meta_body.extend_from_slice(&amf);
        let meta_tag = make_tag(0x09, 0, &meta_body);

        let flv = make_flv(&[&seq_tag, &meta_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _seq = dmx.next_packet().unwrap();
        // The Metadata frame is still surfaced as a header+discard packet.
        let m = dmx.next_packet().unwrap();
        assert!(m.flags.header && m.flags.discard);

        let md = dmx.metadata();
        assert_eq!(
            meta_lookup(md, "colorinfo.colorConfig.bitDepth"),
            Some("10")
        );
        assert_eq!(
            meta_lookup(md, "colorinfo.colorConfig.transferCharacteristics"),
            Some("16")
        );
        assert_eq!(
            meta_lookup(md, "colorinfo.colorConfig.matrixCoefficients"),
            Some("9")
        );
        assert_eq!(meta_lookup(md, "colorinfo.hdrCll.maxFall"), Some("400"));
        assert_eq!(meta_lookup(md, "colorinfo.hdrCll.maxCLL"), Some("1000"));
    }

    #[test]
    fn ex_video_metadata_colorinfo_replaces_prior_value() {
        // Two colorInfo frames: the second must replace the first per spec
        // ("each new colorInfo invalidates and replaces the current one").
        let info1 = {
            let mut a = amf_string("colorInfo");
            a.extend_from_slice(&amf_object(&[(
                "colorConfig",
                amf_object(&[("bitDepth", amf_number(8.0))]),
            )]));
            a
        };
        let info2 = {
            let mut a = amf_string("colorInfo");
            a.extend_from_slice(&amf_object(&[(
                "colorConfig",
                amf_object(&[("bitDepth", amf_number(12.0))]),
            )]));
            a
        };

        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"av01");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x09, 0, &seq_body);

        let mut m1 = vec![0x94];
        m1.extend_from_slice(b"av01");
        m1.extend_from_slice(&info1);
        let m1_tag = make_tag(0x09, 0, &m1);

        let mut m2 = vec![0x94];
        m2.extend_from_slice(b"av01");
        m2.extend_from_slice(&info2);
        let m2_tag = make_tag(0x09, 40, &m2);

        let flv = make_flv(&[&seq_tag, &m1_tag, &m2_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        while dmx.next_packet().is_ok() {}

        let md = dmx.metadata();
        // Only one entry for bitDepth, and it carries the second value.
        let hits: Vec<&str> = md
            .iter()
            .filter(|(k, _)| k == "colorinfo.colorConfig.bitDepth")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(hits, vec!["12"]);
    }

    #[test]
    fn ex_video_metadata_colorinfo_undefined_resets() {
        // colorInfo set, then reset to Undefined (the RECOMMENDED reset
        // per spec). After the reset the nested entries are gone and only
        // the sentinel remains.
        let set = {
            let mut a = amf_string("colorInfo");
            a.extend_from_slice(&amf_object(&[(
                "colorConfig",
                amf_object(&[("bitDepth", amf_number(10.0))]),
            )]));
            a
        };
        let reset = {
            let mut a = amf_string("colorInfo");
            a.push(0x06); // AMF0 Undefined
            a
        };

        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"av01");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x09, 0, &seq_body);

        let mut m1 = vec![0x94];
        m1.extend_from_slice(b"av01");
        m1.extend_from_slice(&set);
        let m1_tag = make_tag(0x09, 0, &m1);

        let mut m2 = vec![0x94];
        m2.extend_from_slice(b"av01");
        m2.extend_from_slice(&reset);
        let m2_tag = make_tag(0x09, 40, &m2);

        let flv = make_flv(&[&seq_tag, &m1_tag, &m2_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        while dmx.next_packet().is_ok() {}

        let md = dmx.metadata();
        assert!(meta_lookup(md, "colorinfo.colorConfig.bitDepth").is_none());
        assert_eq!(meta_lookup(md, "colorinfo"), Some("undefined"));
    }

    #[test]
    fn ex_video_metadata_malformed_amf_is_ignored() {
        // A non-AMF blob in a Metadata frame must not poison the parse —
        // no colorinfo.* metadata, and the packet is still surfaced.
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"hvc1");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x09, 0, &seq_body);

        let mut meta_body = vec![0x94];
        meta_body.extend_from_slice(b"hvc1");
        meta_body.extend_from_slice(b"not-amf");
        let meta_tag = make_tag(0x09, 0, &meta_body);

        let flv = make_flv(&[&seq_tag, &meta_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _seq = dmx.next_packet().unwrap();
        let m = dmx.next_packet().unwrap();
        assert!(m.flags.header && m.flags.discard);
        assert!(
            dmx.metadata()
                .iter()
                .all(|(k, _)| !k.starts_with("colorinfo"))
        );
    }

    /// Build an `onMetaData` script-tag body whose single ECMA-array
    /// argument carries the supplied top-level `(key, amf-value-bytes)`
    /// fields verbatim. Lets the track-map tests embed nested objects
    /// without the rigid single-number shape of `on_metadata_with_property`.
    fn on_metadata_with_fields(fields: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onMetaData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        // ECMA array argument.
        body.push(0x08);
        body.extend_from_slice(&(fields.len() as u32).to_be_bytes());
        for (k, val) in fields {
            body.extend_from_slice(&(k.len() as u16).to_be_bytes());
            body.extend_from_slice(k.as_bytes());
            body.extend_from_slice(val);
        }
        body.extend_from_slice(&[0x00, 0x00, 0x09]);
        body
    }

    #[test]
    fn on_metadata_video_track_id_info_map_flattens() {
        // Enhanced-RTMP-v2 §"Enhancing onMetaData": the new
        // `videoTrackIdInfoMap` carries per-track metadata for non-default
        // tracks, keyed by trackId (1, 2, …). Each value is a per-track
        // property object. The whole map flattens under a lowercased prefix.
        let track1 = amf_object(&[
            ("width", amf_number(1024.0)),
            ("height", amf_number(768.0)),
            ("videodatarate", amf_number(2000.0)),
            ("videocodecid", amf_number(1635135537.0)), // makeFourCc("av01")
        ]);
        let track2 = amf_object(&[
            ("width", amf_number(3840.0)),
            ("height", amf_number(2160.0)),
        ]);
        let map = amf_object(&[("1", track1), ("2", track2)]);
        let body = on_metadata_with_fields(&[("videoTrackIdInfoMap", map)]);
        let script_tag = make_tag(0x12, 0, &body);
        // A video keyframe so stream discovery still succeeds.
        let kf_body = vec![(1u8 << 4) | 2, 0xAA, 0xBB];
        let video_tag = make_tag(0x09, 0, &kf_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        let md = dmx.metadata();
        assert_eq!(meta_lookup(md, "videotrackidinfomap.1.width"), Some("1024"));
        assert_eq!(meta_lookup(md, "videotrackidinfomap.1.height"), Some("768"));
        assert_eq!(
            meta_lookup(md, "videotrackidinfomap.1.videodatarate"),
            Some("2000")
        );
        assert_eq!(
            meta_lookup(md, "videotrackidinfomap.1.videocodecid"),
            Some("1635135537")
        );
        assert_eq!(meta_lookup(md, "videotrackidinfomap.2.width"), Some("3840"));
        assert_eq!(
            meta_lookup(md, "videotrackidinfomap.2.height"),
            Some("2160")
        );
    }

    #[test]
    fn on_metadata_audio_track_id_info_map_flattens() {
        // The audio-side twin of the video map. Delta-style entries (only
        // the fields that differ from the default track) are valid per spec;
        // we flatten exactly what the producer sent.
        let track1 = amf_object(&[
            ("audiodatarate", amf_number(256.0)),
            ("channels", amf_number(2.0)),
            ("samplerate", amf_number(44100.0)),
        ]);
        let track2 = amf_object(&[
            ("audiodatarate", amf_number(320.0)),
            ("samplerate", amf_number(48000.0)),
        ]);
        let map = amf_object(&[("1", track1), ("2", track2)]);
        let body = on_metadata_with_fields(&[("audioTrackIdInfoMap", map)]);
        let script_tag = make_tag(0x12, 0, &body);
        let kf_body = vec![(1u8 << 4) | 2, 0xAA, 0xBB];
        let video_tag = make_tag(0x09, 0, &kf_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        let md = dmx.metadata();
        assert_eq!(
            meta_lookup(md, "audiotrackidinfomap.1.audiodatarate"),
            Some("256")
        );
        assert_eq!(meta_lookup(md, "audiotrackidinfomap.1.channels"), Some("2"));
        assert_eq!(
            meta_lookup(md, "audiotrackidinfomap.1.samplerate"),
            Some("44100")
        );
        assert_eq!(
            meta_lookup(md, "audiotrackidinfomap.2.audiodatarate"),
            Some("320")
        );
        assert_eq!(
            meta_lookup(md, "audiotrackidinfomap.2.samplerate"),
            Some("48000")
        );
        // track 2 sent no `channels` (delta-style) — must not appear.
        assert!(meta_lookup(md, "audiotrackidinfomap.2.channels").is_none());
    }

    #[test]
    fn on_metadata_without_track_maps_emits_no_map_keys() {
        // A plain onMetaData (no track maps) must not synthesise any
        // *trackidinfomap.* entries — the maps are opt-in per producer.
        let body = on_metadata_with_property("width", 640.0);
        let script_tag = make_tag(0x12, 0, &body);
        let kf_body = vec![(1u8 << 4) | 2, 0xAA, 0xBB];
        let video_tag = make_tag(0x09, 0, &kf_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        let md = dmx.metadata();
        assert_eq!(meta_lookup(md, "width"), Some("640"));
        assert!(md.iter().all(|(k, _)| !k.contains("trackidinfomap")));
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
    fn ex_video_command_frame_is_discardable_and_carries_command_byte() {
        // FrameType=Command (5) + PacketType=SequenceStart (any
        // non-Metadata, per enhanced-rtmp-v2 §`Extended VideoTagHeader`):
        // the spec mandates a UI8 VideoCommand after the FourCc, and
        // `processVideoBody = false` after that. The demuxer surfaces
        // the command byte as the packet payload so callers can react
        // to the client-side-seek boundary (parity with the legacy
        // FrameType=5 routing).
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"av01");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x09, 0, &seq_body);
        let mut cmd_body = vec![0xD0]; // 0x80 | (5<<4) | 0
        cmd_body.extend_from_slice(b"av01");
        cmd_body.push(0x01); // command byte = EndSeek
        let cmd_tag = make_tag(0x09, 100, &cmd_body);
        let flv = make_flv(&[&seq_tag, &cmd_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _ = dmx.next_packet().unwrap();
        let cmd = dmx.next_packet().unwrap();
        assert!(cmd.flags.header);
        assert!(cmd.flags.discard);
        assert!(!cmd.flags.keyframe);
        // The command byte (EndSeek = 1) is the entire packet body.
        // Down-stream callers can resolve it via
        // `oxideav_flv::VideoCommand::from_u8(pkt.data[0])`.
        assert_eq!(cmd.data, vec![0x01]);
        assert_eq!(
            crate::VideoCommand::from_u8(cmd.data[0]),
            crate::VideoCommand::EndSeek
        );
    }

    #[test]
    fn ex_audio_opus_sequence_start_routes_extradata() {
        // SoundFormat=9 (ExHeader) + AudioPacketType=0 (SequenceStart)
        // → 0x90. FourCc = "Opus"; body = dummy OpusHead.
        let mut tag_body = vec![0x90];
        tag_body.extend_from_slice(b"Opus");
        let opus_head = b"OpusHead\x01\x02\x38\x01\x80\xBB\x00\x00\x00\x00\x00";
        tag_body.extend_from_slice(opus_head);
        let audio_tag = make_tag(0x08, 0, &tag_body);
        // Follow with a CodedFrames packet so the data-path runs too.
        let mut frame_body = vec![0x91];
        frame_body.extend_from_slice(b"Opus");
        frame_body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let frame_tag = make_tag(0x08, 20, &frame_body);
        // Add a video tag so discovery sees both streams.
        let vp6_body = {
            let flags = (1u8 << 4) | 4;
            vec![flags, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&audio_tag, &frame_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        // Stream 0 should be "opus" (FourCC), not the legacy SoundFormat
        // table lookup.
        assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "opus");
        assert_eq!(dmx.streams()[0].params.extradata, opus_head.to_vec());
        // First packet is the SequenceStart config — emitted as a
        // header packet, not as decoder data.
        let p1 = dmx.next_packet().unwrap();
        assert!(p1.flags.header, "SequenceStart must be header-flagged");
        assert!(p1.flags.keyframe);
        assert_eq!(p1.data, opus_head.to_vec());
        // Second packet is the CodedFrames body.
        let p2 = dmx.next_packet().unwrap();
        assert!(!p2.flags.header);
        assert_eq!(p2.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(p2.pts, Some(20));
        assert_eq!(p2.dts, Some(20));
    }

    #[test]
    fn ex_audio_flac_fourcc_recognised() {
        let mut tag_body = vec![0x90];
        tag_body.extend_from_slice(b"fLaC");
        tag_body.extend_from_slice(b"fLaC\x80\x00\x00\x22"); // dummy STREAMINFO
        let audio_tag = make_tag(0x08, 0, &tag_body);
        let vp6_body = {
            let flags = (1u8 << 4) | 4;
            vec![flags, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&audio_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "flac");
    }

    #[test]
    fn ex_audio_ac3_coded_frames_passes_payload() {
        // AC-3 has no SequenceStart (the stream is self-synchronising) —
        // discovery should land directly on a CodedFrames packet and the
        // codec id should be "ac3".
        let mut tag_body = vec![0x91]; // ExHeader + CodedFrames
        tag_body.extend_from_slice(b"ac-3");
        // ATSC AC-3 sync word followed by some dummy bytes.
        let frame = b"\x0B\x77\x00\x00\xDE\xAD\xBE\xEF";
        tag_body.extend_from_slice(frame);
        let audio_tag = make_tag(0x08, 40, &tag_body);
        let vp6_body = {
            let flags = (1u8 << 4) | 4;
            vec![flags, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&audio_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "ac3");
        // No extradata: CodedFrames doesn't carry a config record.
        assert!(dmx.streams()[0].params.extradata.is_empty());
        let p = dmx.next_packet().unwrap();
        assert!(!p.flags.header);
        assert_eq!(p.data, frame.to_vec());
        assert_eq!(p.pts, Some(40));
    }

    #[test]
    fn ex_audio_sequence_end_yields_no_packet() {
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"Opus");
        seq_body.extend_from_slice(b"head");
        let seq_tag = make_tag(0x08, 0, &seq_body);
        let mut end_body = vec![0x92]; // SequenceEnd
        end_body.extend_from_slice(b"Opus");
        let end_tag = make_tag(0x08, 200, &end_body);
        // Video tag for discovery to succeed (audio-only seek path is
        // exercised elsewhere; here we just need discovery to land).
        let vp6_body = {
            let flags = (1u8 << 4) | 4;
            vec![flags, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&seq_tag, &end_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _seq = dmx.next_packet().unwrap();
        // SequenceEnd should be silently dropped → next is the video
        // tag (stream 1), not an audio packet.
        let next = dmx.next_packet().unwrap();
        assert_ne!(next.stream_index, 0, "SequenceEnd should not surface");
    }

    #[test]
    fn ex_audio_multichannel_config_is_discardable_header() {
        // Discovery tag (SequenceStart) sets the codec; then a
        // MultichannelConfig frame surfaces as header + discard so a
        // standard audio decoder skips the side-channel info.
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"Opus");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x08, 0, &seq_body);
        let mut mcc_body = vec![0x94]; // MultichannelConfig
        mcc_body.extend_from_slice(b"Opus");
        // Truncated config: order=Native promises a UI32 flags field
        // but only one byte follows — the harvest must skip silently.
        mcc_body.extend_from_slice(&[0x01, 0x02, 0x03]);
        let mcc_tag = make_tag(0x08, 10, &mcc_body);
        let vp6_body = {
            let flags = (1u8 << 4) | 4;
            vec![flags, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&seq_tag, &mcc_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _seq = dmx.next_packet().unwrap();
        let m = dmx.next_packet().unwrap();
        assert!(m.flags.header);
        assert!(m.flags.discard);
        assert_eq!(m.data, vec![0x01, 0x02, 0x03]);
        // Malformed side-channel info: no metadata keys, channel count
        // untouched.
        assert!(
            dmx.metadata()
                .iter()
                .all(|(k, _)| !k.starts_with("multichannelconfig"))
        );
        assert_ne!(dmx.streams()[0].params.channels, Some(2));
    }

    #[test]
    fn ex_audio_multichannel_config_in_multitrack_default_track_harvests() {
        // MultichannelConfig wrapped in a Multitrack OneTrack record:
        // the harvest must unwrap the default track's payload exactly
        // like the packet path does. Lead 0x95 = ExHeader + Multitrack;
        // 0x04 = OneTrack | inner MultichannelConfig; shared FourCc;
        // then the OneTrack record = trackId UI8 + payload (Unspecified
        // order, 4 channels).
        let mut seq_body = vec![0x90];
        seq_body.extend_from_slice(b"Opus");
        seq_body.push(0x00);
        let seq_tag = make_tag(0x08, 0, &seq_body);
        let mut mcc_body = vec![0x95];
        mcc_body.push(0x04); // OneTrack | MultichannelConfig
        mcc_body.extend_from_slice(b"Opus");
        mcc_body.push(0x00); // trackId 0
        mcc_body.extend_from_slice(&[0x00, 0x04]); // Unspecified, 4 ch
        let mcc_tag = make_tag(0x08, 10, &mcc_body);
        let vp6_body = {
            let flags = (1u8 << 4) | 4;
            vec![flags, 0x00, 0x42]
        };
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&seq_tag, &mcc_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        let _seq = dmx.next_packet().unwrap();
        let m = dmx.next_packet().unwrap();
        assert!(m.flags.header && m.flags.discard);
        assert_eq!(
            meta_lookup(dmx.metadata(), "multichannelconfig.order"),
            Some("unspecified")
        );
        assert_eq!(
            meta_lookup(dmx.metadata(), "multichannelconfig.channelcount"),
            Some("4")
        );
        assert!(meta_lookup(dmx.metadata(), "multichannelconfig.flags").is_none());
        assert!(meta_lookup(dmx.metadata(), "multichannelconfig.mapping").is_none());
        assert_eq!(dmx.streams()[0].params.channels, Some(4));
    }

    #[test]
    fn ex_audio_metadata_overrides_apply_to_ex_stream() {
        // onMetaData{audiosamplerate=48000, stereo=true,
        // audiodatarate=128} should land on an ExAudio Opus stream just
        // like it does on legacy streams.
        let mut script_body = Vec::new();
        script_body.push(0x02);
        script_body.extend_from_slice(&(10u16).to_be_bytes());
        script_body.extend_from_slice(b"onMetaData");
        script_body.push(0x08);
        script_body.extend_from_slice(&0u32.to_be_bytes());
        script_body.extend_from_slice(&(15u16).to_be_bytes());
        script_body.extend_from_slice(b"audiosamplerate");
        script_body.push(0x00);
        script_body.extend_from_slice(&48000.0_f64.to_be_bytes());
        script_body.extend_from_slice(&(6u16).to_be_bytes());
        script_body.extend_from_slice(b"stereo");
        script_body.push(0x01);
        script_body.push(0x01);
        script_body.extend_from_slice(&(13u16).to_be_bytes());
        script_body.extend_from_slice(b"audiodatarate");
        script_body.push(0x00);
        script_body.extend_from_slice(&128.0_f64.to_be_bytes());
        script_body.extend_from_slice(&[0x00, 0x00, 0x09]);
        let script_tag = make_tag(0x12, 0, &script_body);

        let mut audio_body = vec![0x90];
        audio_body.extend_from_slice(b"Opus");
        audio_body.extend_from_slice(b"OpusHead");
        let audio_tag = make_tag(0x08, 0, &audio_body);
        let flv = make_flv(&[&script_tag, &audio_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        let s = &dmx.streams()[0];
        assert_eq!(s.params.codec_id.as_str(), "opus");
        assert_eq!(s.params.sample_rate, Some(48000));
        assert_eq!(s.params.channels, Some(2));
        assert_eq!(s.params.bit_rate, Some(128_000));
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

    #[test]
    fn multitrack_video_emits_default_track_payload() {
        // ExVideo Multitrack ManyTracks with two av01 tracks. The demuxer
        // must surface the trackId-0 default track's coded data as the
        // packet payload, not the whole multitrack body.
        // Lead 0x96 = IsExHeader | FrameType=1 (key) | PacketType=6.
        // mt-byte 0x11 = ManyTracks (1) | inner CodedFrames (1).
        let mut vbody = vec![0x96, 0x11];
        vbody.extend_from_slice(b"av01"); // shared FourCc
        // Track 0: id=0, size=3, payload [0xDE,0xAD,0xBE].
        vbody.push(0x00);
        vbody.extend_from_slice(&[0x00, 0x00, 0x03]);
        vbody.extend_from_slice(&[0xDE, 0xAD, 0xBE]);
        // Track 1: id=1, size=2, payload [0x11,0x22].
        vbody.push(0x01);
        vbody.extend_from_slice(&[0x00, 0x00, 0x02]);
        vbody.extend_from_slice(&[0x11, 0x22]);

        // Audio tag so discovery completes (audio stream 0, video 1).
        let mp3_body = {
            let flags = (2 << 4) | (2 << 2) | 0x02 | 0x01;
            vec![flags as u8, 0xAA]
        };
        let audio_tag = make_tag(0x08, 0, &mp3_body);
        let video_tag = make_tag(0x09, 40, &vbody);
        let flv = make_flv(&[&audio_tag, &video_tag]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        // AV1 multitrack with a shared FourCc resolves to the av1 codec id.
        assert_eq!(dmx.streams()[1].params.codec_id.as_str(), "av1");

        let _audio = dmx.next_packet().unwrap();
        let vpkt = dmx.next_packet().unwrap();
        assert_eq!(vpkt.stream_index, 1);
        assert!(vpkt.flags.keyframe);
        assert!(!vpkt.flags.discard);
        // Only the default (trackId 0) track's payload, not track 1's.
        assert_eq!(vpkt.data, vec![0xDE, 0xAD, 0xBE]);
    }

    #[test]
    fn multitrack_audio_emits_default_track_payload() {
        // ExAudio Multitrack ManyTracksManyCodecs: per-track FourCc. The
        // default track is Opus CodedFrames; track 1 is a different codec.
        // Lead 0x95 = ExHeader(9) | AudioPacketType=5 (Multitrack).
        // mt-byte 0x21 = ManyTracksManyCodecs (2) | inner CodedFrames (1).
        let mut abody = vec![0x95, 0x21];
        // Track 0: FourCc "Opus", id=0, size=2, payload [0x4F,0x67].
        abody.extend_from_slice(b"Opus");
        abody.push(0x00);
        abody.extend_from_slice(&[0x00, 0x00, 0x02]);
        abody.extend_from_slice(&[0x4F, 0x67]);
        // Track 1: FourCc "mp4a", id=1, size=1, payload [0x99].
        abody.extend_from_slice(b"mp4a");
        abody.push(0x01);
        abody.extend_from_slice(&[0x00, 0x00, 0x01]);
        abody.push(0x99);

        let audio_tag = make_tag(0x08, 0, &abody);
        // A legacy video tag so discovery completes.
        let vp6_body = vec![((1 << 4) | 4) as u8, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&audio_tag, &video_tag]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open(input, &NullCodecResolver).unwrap();
        // ManyTracksManyCodecs has no shared FourCc → multicodec sentinel.
        assert_eq!(
            dmx.streams()[0].params.codec_id.as_str(),
            "flv:exaudio:multicodec"
        );

        let apkt = dmx.next_packet().unwrap();
        assert_eq!(apkt.stream_index, 0);
        // Default (trackId 0) Opus payload only.
        assert_eq!(apkt.data, vec![0x4F, 0x67]);
    }

    #[test]
    fn last_multitrack_tracks_exposes_video_non_default_variants() {
        // Same fixture shape as `multitrack_video_emits_default_track_payload`
        // (ManyTracks, two av01 tracks) — but here we assert the demuxer
        // also exposes the *non-default* track (id=1) through
        // `last_multitrack_tracks`, which the single-stream packet flow
        // would otherwise drop.
        let mut vbody = vec![0x96, 0x11];
        vbody.extend_from_slice(b"av01");
        vbody.push(0x00);
        vbody.extend_from_slice(&[0x00, 0x00, 0x03]);
        vbody.extend_from_slice(&[0xDE, 0xAD, 0xBE]);
        vbody.push(0x01);
        vbody.extend_from_slice(&[0x00, 0x00, 0x02]);
        vbody.extend_from_slice(&[0x11, 0x22]);

        let mp3_body = vec![(((2 << 4) | (2 << 2) | 0x02 | 0x01) as u8), 0xAA];
        let audio_tag = make_tag(0x08, 0, &mp3_body);
        let video_tag = make_tag(0x09, 40, &vbody);
        let flv = make_flv(&[&audio_tag, &video_tag]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open_concrete(input, &NullCodecResolver).unwrap();

        let fourcc_av01 = u32::from_be_bytes(*b"av01");

        // Before any packet is pulled, no multitrack tag has been seen.
        assert!(dmx.last_multitrack_tracks().is_none());

        // The audio tag is single-track → side-channel stays None.
        let _audio = dmx.next_packet().unwrap();
        assert!(dmx.last_multitrack_tracks().is_none());

        // The video tag is multitrack → both tracks surface.
        let _vpkt = dmx.next_packet().unwrap();
        let tracks = dmx.last_multitrack_tracks().expect("multitrack captured");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_id, 0);
        assert_eq!(tracks[0].fourcc, fourcc_av01);
        assert_eq!(tracks[0].codec_name, "av1");
        assert_eq!(tracks[0].payload, vec![0xDE, 0xAD, 0xBE]);
        assert_eq!(tracks[0].len(), 3);
        assert!(!tracks[0].is_empty());
        // The non-default variant — not reachable via the Packet flow.
        assert_eq!(tracks[1].track_id, 1);
        assert_eq!(tracks[1].fourcc, fourcc_av01);
        assert_eq!(tracks[1].codec_name, "av1");
        assert_eq!(tracks[1].payload, vec![0x11, 0x22]);
    }

    #[test]
    fn last_multitrack_tracks_audio_many_codecs_and_clears_on_single_track() {
        // ExAudio ManyTracksManyCodecs (per-track FourCc) followed by a
        // legacy single-track audio tag. The side-channel must (a) expose
        // both tracks with their distinct resolved codec names, then (b)
        // clear to None once a single-track tag is emitted, so it never
        // reports stale tracks.
        let mut abody = vec![0x95, 0x21];
        abody.extend_from_slice(b"Opus");
        abody.push(0x00);
        abody.extend_from_slice(&[0x00, 0x00, 0x02]);
        abody.extend_from_slice(&[0x4F, 0x67]);
        abody.extend_from_slice(b"mp4a");
        abody.push(0x01);
        abody.extend_from_slice(&[0x00, 0x00, 0x01]);
        abody.push(0x99);
        let mt_tag = make_tag(0x08, 0, &abody);

        // A second, legacy MP3 audio tag (single-track).
        let mp3_body = vec![(((2 << 4) | (2 << 2) | 0x02 | 0x01) as u8), 0xBB];
        let mp3_tag = make_tag(0x08, 20, &mp3_body);

        let vp6_body = vec![((1 << 4) | 4) as u8, 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&mt_tag, &mp3_tag, &video_tag]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open_concrete(input, &NullCodecResolver).unwrap();

        // First packet is the multitrack tag.
        let _a0 = dmx.next_packet().unwrap();
        let tracks = dmx.last_multitrack_tracks().expect("multitrack captured");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_id, 0);
        assert_eq!(tracks[0].codec_name, "opus");
        assert_eq!(tracks[0].payload, vec![0x4F, 0x67]);
        assert_eq!(tracks[1].track_id, 1);
        // Per-track FourCc "mp4a" resolves to the AAC audio name.
        assert_eq!(tracks[1].codec_name, "aac");
        assert_eq!(tracks[1].payload, vec![0x99]);

        // Second packet is a single-track legacy tag → side-channel clears.
        let _a1 = dmx.next_packet().unwrap();
        assert!(dmx.last_multitrack_tracks().is_none());
    }

    #[test]
    fn last_multitrack_tracks_one_track_mode() {
        // OneTrack mode: a single trackId-0 track whose payload runs to the
        // end of the body (no sizeOfTrack field). The accessor surfaces the
        // lone track, matching the Packet payload.
        // mt-byte 0x01 = OneTrack (0) | inner CodedFrames (1).
        let mut vbody = vec![0x96, 0x01];
        vbody.extend_from_slice(b"hvc1");
        vbody.push(0x00); // trackId 0
        // SI24 CTO (0) + coded bytes — payload runs to end.
        vbody.extend_from_slice(&[0x00, 0x00, 0x00, 0x77, 0x88]);

        let mp3_body = vec![(((2 << 4) | (2 << 2) | 0x02 | 0x01) as u8), 0xAA];
        let audio_tag = make_tag(0x08, 0, &mp3_body);
        let video_tag = make_tag(0x09, 0, &vbody);
        let flv = make_flv(&[&audio_tag, &video_tag]);

        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let mut dmx = open_concrete(input, &NullCodecResolver).unwrap();
        let _a = dmx.next_packet().unwrap();
        let _v = dmx.next_packet().unwrap();
        let tracks = dmx.last_multitrack_tracks().expect("one-track captured");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_id, 0);
        assert_eq!(tracks[0].fourcc, FOURCC_HVC1);
        assert_eq!(tracks[0].codec_name, "h265");
        // Whole tail (SI24 CTO preserved in the per-track payload).
        assert_eq!(tracks[0].payload, vec![0x00, 0x00, 0x00, 0x77, 0x88]);
    }

    /// Build a SCRIPTDATA tag body whose method name is the given
    /// (non-spec) string and whose argument is a String AMF0 value.
    /// Used by the `unknown_script_*` tests below.
    fn script_string_arg(name: &str, value: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&(name.len() as u16).to_be_bytes());
        body.extend_from_slice(name.as_bytes());
        body.push(0x02);
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        body.extend_from_slice(value.as_bytes());
        body
    }

    #[test]
    fn unknown_script_name_preserves_string_argument() {
        // FLV spec v10.1 names only `onMetaData` (E.5), `onXMPData`
        // (E.6), `onCuePoint` (Annex A), and `|AdditionalHeader`
        // (F.2.1). Enhanced-RTMP-v2 §"Enhancing onMetaData" treats
        // SCRIPTDATA as a generic "method-name + argument" carrier, so
        // producers in the wild emit other names (live captions,
        // telemetry, …). Previously the demuxer dropped the argument
        // value and only recorded the name; now both surface.
        let script_tag = make_tag(0x12, 0, &script_string_arg("onCaptionInfo", "hello"));
        let vp6_body = vec![((1u8 << 4) | 4), 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        // The legacy `scriptdata.name` sentinel still records the name.
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "scriptdata.name" && v == "onCaptionInfo")
        );
        // The argument value lands under the scoped prefix.
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "scriptdata.onCaptionInfo" && v == "hello")
        );
    }

    #[test]
    fn unknown_script_name_flattens_object_argument() {
        // Same coverage, with an Object argument exercising the
        // composite-flatten path. Producer-defined keys fan out under
        // the `scriptdata.<name>.<subkey>` prefix.
        let mut arg = Vec::new();
        arg.push(0x03); // Object
        arg.extend_from_slice(&("track".len() as u16).to_be_bytes());
        arg.extend_from_slice(b"track");
        arg.push(0x00);
        arg.extend_from_slice(&7.0_f64.to_be_bytes());
        arg.extend_from_slice(&("language".len() as u16).to_be_bytes());
        arg.extend_from_slice(b"language");
        arg.push(0x02);
        arg.extend_from_slice(&("eng".len() as u16).to_be_bytes());
        arg.extend_from_slice(b"eng");
        arg.extend_from_slice(&[0x00, 0x00, 0x09]);
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onTextData".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onTextData");
        body.extend_from_slice(&arg);
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![((1u8 << 4) | 4), 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "scriptdata.name" && v == "onTextData")
        );
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "scriptdata.onTextData.track" && v == "7")
        );
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "scriptdata.onTextData.language" && v == "eng")
        );
    }

    #[test]
    fn unknown_script_name_preserves_null_argument() {
        // A Null argument used to leave the metadata bag with only the
        // name sentinel; the flatten walker now lifts it under the
        // sentinel string `"null"` so the absence-of-value is itself
        // visible to callers.
        let mut body = Vec::new();
        body.push(0x02);
        body.extend_from_slice(&("onSomeEvent".len() as u16).to_be_bytes());
        body.extend_from_slice(b"onSomeEvent");
        body.push(0x05); // Null
        let script_tag = make_tag(0x12, 0, &body);
        let vp6_body = vec![((1u8 << 4) | 4), 0x00, 0x42];
        let video_tag = make_tag(0x09, 0, &vp6_body);
        let flv = make_flv(&[&script_tag, &video_tag]);
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(flv));
        let dmx = open(input, &NullCodecResolver).unwrap();
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "scriptdata.name" && v == "onSomeEvent")
        );
        assert!(
            dmx.metadata()
                .iter()
                .any(|(k, v)| k == "scriptdata.onSomeEvent" && v == "null")
        );
    }

    #[test]
    fn speex_stream_stays_mono_16k_despite_onmetadata_stereo() {
        // E.4.2.1: Speex is "compressed mono sampled at 16 kHz". A
        // producer that wrongly stamped `stereo = true` /
        // `audiosamplerate = 44100` must NOT upgrade the stream — the
        // codec id pins it to mono / 16 kHz.
        // SoundFormat 11 (Speex) high nibble, SoundType bit set (wrongly
        // claims stereo) — 0xB2.
        let speex_body = vec![0xB2u8, 0x00]; // header + 1 byte
        let meta = vec![
            ("stereo".to_string(), "true".to_string()),
            ("audiosamplerate".to_string(), "44100".to_string()),
        ];
        let info = build_audio_stream(STREAM_AUDIO, &speex_body, &meta).unwrap();
        assert_eq!(info.params.channels, Some(1), "Speex must stay mono");
        assert_eq!(
            info.params.sample_rate,
            Some(16_000),
            "Speex must stay 16 kHz"
        );
    }

    #[test]
    fn nellymoser_8k_stream_stays_mono_8k_despite_onmetadata() {
        // Nellymoser 8 kHz mono (codec 5).
        let body = vec![(5u8 << 4) | (3 << 2) | 0b11, 0x00];
        let meta = vec![
            ("stereo".to_string(), "true".to_string()),
            ("audiosamplerate".to_string(), "48000".to_string()),
        ];
        let info = build_audio_stream(STREAM_AUDIO, &body, &meta).unwrap();
        assert_eq!(info.params.channels, Some(1));
        assert_eq!(info.params.sample_rate, Some(8_000));
    }

    #[test]
    fn mp3_stream_still_honours_onmetadata_overrides() {
        // A non-pinned format must still pick up the authoritative
        // onMetaData stereo / samplerate values.
        let mp3_body = vec![(2u8 << 4) | (3 << 2) | 0b11, 0x00];
        let meta = vec![
            ("stereo".to_string(), "false".to_string()),
            ("audiosamplerate".to_string(), "48000".to_string()),
        ];
        let info = build_audio_stream(STREAM_AUDIO, &mp3_body, &meta).unwrap();
        assert_eq!(
            info.params.channels,
            Some(1),
            "onMetaData stereo=false wins"
        );
        assert_eq!(info.params.sample_rate, Some(48_000));
    }
}
