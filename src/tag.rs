//! FLV tag header + codec-id mappings.
//!
//! Tag layout (spec §E.4):
//!
//! ```text
//!   0   1    TagType   (0x08 audio, 0x09 video, 0x12 script)
//!   1   3    DataSize  (payload length, u24 BE)
//!   4   3    Timestamp (lower 24 bits, u24 BE, milliseconds)
//!   7   1    TimestampExtended (top 8 bits, prepend to u32)
//!   8   3    StreamID  (u24 BE — reserved, always 0)
//!  11   N    payload (DataSize bytes)
//! ```

use std::io::{Read, Write};

use oxideav_core::{Error, Result};

/// Tag header length in bytes (not including the payload or the
/// 4-byte `PreviousTagSize` prefix).
pub const TAG_HEADER_LEN: u32 = 11;

/// Maximum value of a `UI24` field — the `DataSize`, `Timestamp` (low
/// 24 bits), and `StreamID` fields are all 24-bit (spec §E.4.1).
const UI24_MAX: u32 = 0x00FF_FFFF;

/// Tag-type byte values defined by the FLV spec. Other values are
/// reserved and the demuxer surfaces them as a decoder-free `Packet`
/// only if the caller asks us to (we currently skip unknown tag types).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagType {
    Audio,
    Video,
    ScriptData,
}

impl TagType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v & 0x1F {
            0x08 => Some(Self::Audio),
            0x09 => Some(Self::Video),
            0x12 => Some(Self::ScriptData),
            _ => None,
        }
    }

    /// The `TagType` UB[5] wire value (spec §E.4.1): `8` audio, `9`
    /// video, `18` script data. Inverse of [`TagType::from_u8`].
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Audio => 0x08,
            Self::Video => 0x09,
            Self::ScriptData => 0x12,
        }
    }
}

// ---- tag stream writers ----------------------------------------------------

/// Write the leading `PreviousTagSize0` field that opens an FLV file
/// body (spec §E.3 — "Always 0"). Call this once, immediately after the
/// 9-byte file header and before the first tag.
pub fn write_first_previous_tag_size<W: Write + ?Sized>(w: &mut W) -> Result<()> {
    w.write_all(&0u32.to_be_bytes())?;
    Ok(())
}

/// Write one complete FLV tag: the 11-byte tag header, the `body`
/// payload, and the trailing 4-byte `PreviousTagSize` back-pointer
/// (spec §E.3 / §E.4.1).
///
/// * `tag_type` — audio / video / script.
/// * `timestamp_ms` — full 32-bit presentation time; the low 24 bits go
///   in `Timestamp`, the top 8 in `TimestampExtended` (E.4.1).
/// * `stream_id` — the `StreamID` UI24, always `0` per spec.
/// * `body` — the tag payload (`DataSize` is its length).
///
/// The trailing `PreviousTagSize` is `11 + body.len()`, i.e. the size of
/// this tag including its header, exactly as a demuxer's reverse walk
/// expects.
///
/// Returns the total number of bytes written for this tag
/// (`11 + body.len() + 4`) so a caller chaining tags can track the file
/// offset. Errors with [`Error::InvalidData`] if `body` exceeds the
/// `UI24` `DataSize` limit or `stream_id` exceeds `UI24`.
pub fn write_tag<W: Write + ?Sized>(
    w: &mut W,
    tag_type: TagType,
    timestamp_ms: u32,
    stream_id: u32,
    body: &[u8],
) -> Result<u32> {
    let data_size = body.len();
    if data_size as u64 > UI24_MAX as u64 {
        return Err(Error::invalid(format!(
            "FLV tag: DataSize {data_size} exceeds UI24 max {UI24_MAX}"
        )));
    }
    if stream_id > UI24_MAX {
        return Err(Error::invalid(format!(
            "FLV tag: StreamID {stream_id} exceeds UI24 max {UI24_MAX}"
        )));
    }
    let data_size = data_size as u32;
    let ts_low = timestamp_ms & UI24_MAX;
    let ts_ext = (timestamp_ms >> 24) as u8;
    let mut header = [0u8; TAG_HEADER_LEN as usize];
    // Reserved UB[2]=0, Filter UB[1]=0, TagType UB[5] — we never emit
    // filtered (encrypted) tags here, so the leading three bits are 0.
    header[0] = tag_type.to_u8();
    header[1..4].copy_from_slice(&u24_to_be(data_size));
    header[4..7].copy_from_slice(&u24_to_be(ts_low));
    header[7] = ts_ext;
    header[8..11].copy_from_slice(&u24_to_be(stream_id));
    w.write_all(&header)?;
    w.write_all(body)?;
    // PreviousTagSize = 11 + DataSize (E.3).
    let prev_tag_size = TAG_HEADER_LEN + data_size;
    w.write_all(&prev_tag_size.to_be_bytes())?;
    Ok(TAG_HEADER_LEN + data_size + 4)
}

/// Write an audio tag: a one-byte [`AudioTagHeader`] followed by
/// `payload`, wrapped in a full FLV tag via [`write_tag`].
///
/// For AAC (`codec_id` 10) the caller is responsible for prepending the
/// `AACPacketType` byte to `payload` (or use [`write_aac_raw_tag`]).
pub fn write_audio_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    header: AudioTagHeader,
    payload: &[u8],
) -> Result<u32> {
    let mut body = Vec::with_capacity(1 + payload.len());
    body.push(header.to_byte());
    body.extend_from_slice(payload);
    write_tag(w, TagType::Audio, timestamp_ms, 0, &body)
}

/// Write an MP3 audio tag (`SoundFormat = 2`, spec §E.4.2.1).
///
/// `mp3_frame` is one raw MPEG-1/2 Audio Layer III frame; it becomes the
/// `SoundData` body verbatim. `sample_rate_idx` is the 2-bit `SoundRate`
/// code (`0`=5.5k, `1`=11k, `2`=22k, `3`=44k), `is_16bit` the `SoundSize`
/// bit, `is_stereo` the `SoundType` bit.
pub fn write_mp3_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    sample_rate_idx: u8,
    is_16bit: bool,
    is_stereo: bool,
    mp3_frame: &[u8],
) -> Result<u32> {
    let header = AudioTagHeader {
        codec_id: AUDIO_CODEC_MP3,
        sample_rate_idx: sample_rate_idx & 0x03,
        is_16bit,
        is_stereo,
    };
    write_audio_tag(w, timestamp_ms, header, mp3_frame)
}

/// Write a raw AAC audio tag (`SoundFormat = 10`, `AACPacketType = 1`,
/// spec §E.4.2.1 / §E.4.2.2).
///
/// `raw_au` is one raw AAC access unit. Per spec the `SoundRate` /
/// `SoundSize` / `SoundType` bits for AAC are fixed at `3` (44 kHz) /
/// 16-bit / stereo and ignored by the player (the real parameters come
/// from the `AudioSpecificConfig`), so the header byte is `0xAF`.
pub fn write_aac_raw_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    raw_au: &[u8],
) -> Result<u32> {
    let header = AudioTagHeader {
        codec_id: AUDIO_CODEC_AAC,
        sample_rate_idx: 3,
        is_16bit: true,
        is_stereo: true,
    };
    // AACPacketType UI8 = 1 (raw) precedes the access unit (E.4.2.1).
    let mut payload = Vec::with_capacity(1 + raw_au.len());
    payload.push(0x01);
    payload.extend_from_slice(raw_au);
    write_audio_tag(w, timestamp_ms, header, &payload)
}

/// Write a video tag: a one-byte [`VideoTagHeader`] followed by
/// `payload`, wrapped in a full FLV tag via [`write_tag`] (spec
/// §E.4.3 / §E.4.3.1).
///
/// `payload` is the codec-specific `VIDEODATA` body that follows the
/// `FrameType | CodecID` byte. For AVC the caller is responsible for
/// prepending the `AVCPacketType` + `CompositionTime` bytes (or use the
/// dedicated [`write_avc_sequence_header`] / [`write_avc_nalu_tag`]
/// helpers). For VP6 with alpha (codec_id 5) the first payload byte is
/// the spec-defined alpha-data offset — see [`write_vp6a_tag`] for the
/// canonical builder.
pub fn write_video_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    header: VideoTagHeader,
    payload: &[u8],
) -> Result<u32> {
    let mut body = Vec::with_capacity(1 + payload.len());
    body.push(header.to_byte());
    body.extend_from_slice(payload);
    write_tag(w, TagType::Video, timestamp_ms, 0, &body)
}

/// Write a Sorenson H.263 (`flv1`, codec_id 2) video tag, spec §E.4.3.1.
///
/// `is_keyframe` picks [`FrameType::Key`] vs [`FrameType::Inter`];
/// `frame` is one raw H.263 bitstream frame, written verbatim as
/// `VIDEODATA`.
pub fn write_h263_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    frame: &[u8],
) -> Result<u32> {
    let header = VideoTagHeader {
        frame_type: if is_keyframe {
            FrameType::Key
        } else {
            FrameType::Inter
        },
        codec_id: VIDEO_CODEC_FLV1,
    };
    write_video_tag(w, timestamp_ms, header, frame)
}

/// Write a VP6 (`vp6f`, codec_id 4) video tag, spec §E.4.3.1.
///
/// `frame` is one raw VP6 bitstream frame, written verbatim as
/// `VIDEODATA`. VP6 does not have the alpha-offset byte the VP6A
/// variant carries.
pub fn write_vp6_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    frame: &[u8],
) -> Result<u32> {
    let header = VideoTagHeader {
        frame_type: if is_keyframe {
            FrameType::Key
        } else {
            FrameType::Inter
        },
        codec_id: VIDEO_CODEC_VP6F,
    };
    write_video_tag(w, timestamp_ms, header, frame)
}

/// Write a VP6-with-alpha (`vp6a`, codec_id 5) video tag, spec §E.4.3.1.
///
/// The VIDEODATA body for VP6A is:
///
/// ```text
///   1   AlphaOffset (UI8 — byte offset to the alpha-channel sub-stream)
///   N   VP6 video data (the BGR sub-stream)
///   M   VP6 alpha data
/// ```
///
/// `alpha_offset` is the UI8 byte offset from the *start of the
/// VP6-video sub-stream* to the start of the alpha sub-stream (spec
/// E.4.3.1 IF CodecID == 5). `frame` is the concatenated
/// `vp6_video | vp6_alpha` payload. The demuxer surfaces the alpha
/// offset through extradata; this writer preserves the spec layout
/// byte-exactly.
pub fn write_vp6a_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    alpha_offset: u8,
    frame: &[u8],
) -> Result<u32> {
    let header = VideoTagHeader {
        frame_type: if is_keyframe {
            FrameType::Key
        } else {
            FrameType::Inter
        },
        codec_id: VIDEO_CODEC_VP6A,
    };
    let mut payload = Vec::with_capacity(1 + frame.len());
    payload.push(alpha_offset);
    payload.extend_from_slice(frame);
    write_video_tag(w, timestamp_ms, header, &payload)
}

/// Write an AVC sequence-header video tag (`AVCPacketType = 0`, spec
/// §E.4.3.1 IF CodecID == 7). The body layout is:
///
/// ```text
///   0   VideoTagHeader byte  (FrameType=Key | CodecID=7 = 0x17)
///   1   AVCPacketType UI8    (0 — AVCDecoderConfigurationRecord)
///   2   CompositionTime SI24 (0 for the sequence header per spec)
///   5   AVCDecoderConfigurationRecord (the `extradata` blob)
/// ```
///
/// `config_record` is the
/// `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15) verbatim — the
/// same byte sequence the demuxer surfaces to `params.extradata`.
/// The frame type is forced to [`FrameType::Key`] per spec convention
/// (a sequence header opens a configuration window and is always
/// emitted on a key frame).
pub fn write_avc_sequence_header<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    config_record: &[u8],
) -> Result<u32> {
    let header = VideoTagHeader {
        frame_type: FrameType::Key,
        codec_id: VIDEO_CODEC_H264,
    };
    let mut payload = Vec::with_capacity(4 + config_record.len());
    payload.push(0x00); // AVCPacketType = 0 (sequence header)
    payload.extend_from_slice(&u24_to_be(0)); // CompositionTime = 0
    payload.extend_from_slice(config_record);
    write_video_tag(w, timestamp_ms, header, &payload)
}

/// Write an AVC NALU access-unit video tag (`AVCPacketType = 1`, spec
/// §E.4.3.1 IF CodecID == 7). The body layout is:
///
/// ```text
///   0   VideoTagHeader byte
///   1   AVCPacketType UI8    (1 — one or more length-prefixed NALUs)
///   2   CompositionTime SI24 (pts - dts, in milliseconds, signed 24-bit)
///   5   NALU access unit     (length-prefixed per the sequence header's
///                             lengthSizeMinusOne, conventionally 4-byte BE)
/// ```
///
/// `composition_time_ms` is the signed 24-bit composition-time offset
/// (`pts - dts`) expressed in milliseconds; it ranges
/// `-8_388_608..=8_388_607` and is sign-truncated into the wire SI24 on
/// overflow with [`Error::InvalidData`]. `access_unit` is one or more
/// length-prefixed NALUs concatenated, exactly as the spec's
/// "NALU access unit" requires.
pub fn write_avc_nalu_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    composition_time_ms: i32,
    access_unit: &[u8],
) -> Result<u32> {
    // Reject out-of-SI24 composition times rather than silently truncating;
    // signed 24-bit range is [-2^23, 2^23 - 1].
    if !(-(1 << 23)..(1 << 23)).contains(&composition_time_ms) {
        return Err(Error::invalid(format!(
            "AVC tag: CompositionTime {composition_time_ms} ms outside SI24 range"
        )));
    }
    let header = VideoTagHeader {
        frame_type: if is_keyframe {
            FrameType::Key
        } else {
            FrameType::Inter
        },
        codec_id: VIDEO_CODEC_H264,
    };
    // SI24 BE — preserve the low 24 bits of the two's-complement encoding.
    let cts_bits = composition_time_ms as u32 & 0x00FF_FFFF;
    let mut payload = Vec::with_capacity(4 + access_unit.len());
    payload.push(0x01); // AVCPacketType = 1 (NALU)
    payload.extend_from_slice(&u24_to_be(cts_bits));
    payload.extend_from_slice(access_unit);
    write_video_tag(w, timestamp_ms, header, &payload)
}

/// Write an AVC end-of-sequence video tag (`AVCPacketType = 2`, spec
/// §E.4.3.1 IF CodecID == 7) — a one-byte body terminating the AVC
/// sub-stream. `CompositionTime` is `0` and the body has no NALU data.
pub fn write_avc_end_of_sequence<W: Write + ?Sized>(w: &mut W, timestamp_ms: u32) -> Result<u32> {
    let header = VideoTagHeader {
        // End-of-sequence is conventionally emitted on a non-keyframe slot;
        // spec doesn't constrain the FrameType bits but DisposableInter
        // is the closest "this is not codec data" marker available in the
        // legacy field. Decoders won't try to decode it (AVCPacketType=2
        // is the signal).
        frame_type: FrameType::DisposableInter,
        codec_id: VIDEO_CODEC_H264,
    };
    let mut payload = Vec::with_capacity(4);
    payload.push(0x02); // AVCPacketType = 2 (end of sequence)
    payload.extend_from_slice(&u24_to_be(0));
    write_video_tag(w, timestamp_ms, header, &payload)
}

/// Write a FrameType=5 "video info / command" tag (spec §E.4.3.1 IF
/// FrameType == 5). The body is a one-byte UI8 command (`0` =
/// StartClientSeek, `1` = EndClientSeek; other codes are reserved but
/// passed through verbatim via [`VideoInfoCommand::Unknown`]).
///
/// CodecID in the wire byte is set to `0` — the spec gives no
/// codec-id meaning for FrameType=5, and a real-world parser ignores
/// the low nibble entirely. The demuxer surfaces such tags with
/// `flags.header = true` + `flags.discard = true`.
pub fn write_video_info_command_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    command: VideoInfoCommand,
) -> Result<u32> {
    let header = VideoTagHeader {
        frame_type: FrameType::VideoInfo,
        codec_id: 0,
    };
    write_video_tag(w, timestamp_ms, header, &[command.to_u8()])
}

// ---- Enhanced-RTMP / E-FLV ExVideo + ExAudio writers --------------------
//
// These mirror the parsers in `ex_video.rs` / `ex_audio.rs`. Each writer
// emits a complete FLV tag: TAG_HEADER + ExHeader bytes (via
// `ExVideoTagHeader::to_bytes` / `ExAudioTagHeader::to_bytes`) + the
// codec-specific payload + trailing PreviousTagSize.
//
// The legacy `write_video_tag` / `write_audio_tag` family above is the
// pre-2023 entry point. The Ex family below is the FourCC-mode entry
// point used by enhanced-rtmp-v1 (FourCC av01 / vp09 / vp08 / hvc1 /
// avc1 / vvc1 video and Opus / fLaC / ac-3 / ec-3 / .mp3 / mp4a audio).

/// Write a fully-formed Enhanced-RTMP video tag from an
/// [`ExVideoTagHeader`] and a codec-specific payload.
///
/// `payload` is the bytes that follow the ExHeader on the wire:
///
/// * For `SequenceStart` — the codec's configuration record (e.g. an
///   `AV1CodecConfigurationRecord`, `HEVCDecoderConfigurationRecord`, …).
/// * For `CodedFrames` — one or more coded frames. NOTE: the SI24
///   `CompositionTimeOffset` for HEVC / VVC / AVC is emitted by
///   [`ExVideoTagHeader::to_bytes`] from `header.composition_time_offset_ms`,
///   so `payload` must *not* include it.
/// * For `CodedFramesX` — coded frames with no CTO byte slot.
/// * For `Metadata` — AMF-encoded `["colorInfo", Object]` (or future
///   metadata frame variants).
/// * For `SequenceEnd` — empty.
///
/// The dedicated `write_av1_*` / `write_hevc_*` / `write_opus_*` helpers
/// below cover the common cases without the caller having to assemble
/// the header struct by hand.
pub fn write_ex_video_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    header: &crate::ex_video::ExVideoTagHeader,
    payload: &[u8],
) -> Result<u32> {
    let mut body = Vec::with_capacity(8 + payload.len());
    header.to_bytes(&mut body)?;
    body.extend_from_slice(payload);
    write_tag(w, TagType::Video, timestamp_ms, 0, &body)
}

/// Write a fully-formed Enhanced-RTMP audio tag from an
/// [`ExAudioTagHeader`] and a codec-specific payload.
///
/// `payload` follows the ExHeader on the wire:
///
/// * For `SequenceStart` — the codec's configuration record (Opus RFC
///   7845 ID header, `fLaC` + STREAMINFO, AAC AudioSpecificConfig).
/// * For `CodedFrames` — one coded frame (ATSC AC-3 sync frame, FLAC
///   frame, raw AAC AU, MP3 frame, Opus packet).
/// * For `MultichannelConfig` — multichannel-config blob.
/// * For `SequenceEnd` — empty.
pub fn write_ex_audio_tag<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    header: &crate::ex_audio::ExAudioTagHeader,
    payload: &[u8],
) -> Result<u32> {
    let mut body = Vec::with_capacity(8 + payload.len());
    header.to_bytes(&mut body)?;
    body.extend_from_slice(payload);
    write_tag(w, TagType::Audio, timestamp_ms, 0, &body)
}

// ---- Ex-video single-track convenience writers ----------------------------

fn single_track_ex_video_header(
    frame_type: crate::ex_video::ExFrameType,
    packet_type: crate::ex_video::ExPacketType,
    fourcc: u32,
    composition_time_offset_ms: Option<i32>,
) -> crate::ex_video::ExVideoTagHeader {
    crate::ex_video::ExVideoTagHeader {
        frame_type,
        packet_type,
        fourcc: Some(fourcc),
        multitrack: None,
        bytes_consumed: 0,
        composition_time_offset_ms,
        timestamp_offset_nano: 0,
        mod_ex_entries: Vec::new(),
        video_command: None,
    }
}

/// Write an AV1 (`av01`) Enhanced-RTMP SequenceStart tag carrying the
/// `AV1CodecConfigurationRecord` verbatim.
///
/// Frame type is forced to [`crate::ex_video::ExFrameType::KeyFrame`]
/// per spec convention (the sequence header opens a fresh decoding
/// window).
pub fn write_av1_sequence_start<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    config_record: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        crate::ex_video::ExFrameType::KeyFrame,
        crate::ex_video::ExPacketType::SequenceStart,
        crate::ex_video::FOURCC_AV01,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, config_record)
}

/// Write an AV1 (`av01`) Enhanced-RTMP CodedFrames tag.
///
/// AV1 carries no SI24 `CompositionTimeOffset` (spec: CTO is only for
/// HEVC / VVC / AVC `CodedFrames`); `payload` is one or more coded
/// frames verbatim.
pub fn write_av1_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    payload: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        if is_keyframe {
            crate::ex_video::ExFrameType::KeyFrame
        } else {
            crate::ex_video::ExFrameType::InterFrame
        },
        crate::ex_video::ExPacketType::CodedFrames,
        crate::ex_video::FOURCC_AV01,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, payload)
}

/// Write a VP9 (`vp09`) Enhanced-RTMP SequenceStart tag carrying the
/// `VPCodecConfigurationRecord`.
pub fn write_vp9_sequence_start<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    config_record: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        crate::ex_video::ExFrameType::KeyFrame,
        crate::ex_video::ExPacketType::SequenceStart,
        crate::ex_video::FOURCC_VP09,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, config_record)
}

/// Write a VP9 (`vp09`) Enhanced-RTMP CodedFrames tag. No CTO byte
/// slot per spec.
pub fn write_vp9_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    payload: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        if is_keyframe {
            crate::ex_video::ExFrameType::KeyFrame
        } else {
            crate::ex_video::ExFrameType::InterFrame
        },
        crate::ex_video::ExPacketType::CodedFrames,
        crate::ex_video::FOURCC_VP09,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, payload)
}

/// Write an HEVC (`hvc1`) Enhanced-RTMP SequenceStart tag carrying the
/// `HEVCDecoderConfigurationRecord`.
pub fn write_hevc_sequence_start<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    config_record: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        crate::ex_video::ExFrameType::KeyFrame,
        crate::ex_video::ExPacketType::SequenceStart,
        crate::ex_video::FOURCC_HVC1,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, config_record)
}

/// Write an HEVC (`hvc1`) Enhanced-RTMP CodedFrames tag.
///
/// `composition_time_offset_ms` is the signed 24-bit `CTS` (pts − dts)
/// in milliseconds; the `to_bytes` step emits it as a SI24 between the
/// FourCc and the NALU payload (mirroring legacy AVC). Use
/// [`write_hevc_coded_frames_x`] for the no-CTO 3-byte-savings variant.
pub fn write_hevc_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    composition_time_offset_ms: i32,
    payload: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        if is_keyframe {
            crate::ex_video::ExFrameType::KeyFrame
        } else {
            crate::ex_video::ExFrameType::InterFrame
        },
        crate::ex_video::ExPacketType::CodedFrames,
        crate::ex_video::FOURCC_HVC1,
        Some(composition_time_offset_ms),
    );
    write_ex_video_tag(w, timestamp_ms, &header, payload)
}

/// Write an HEVC (`hvc1`) Enhanced-RTMP `CodedFramesX` tag (implicit
/// zero CTO; 3 bytes saved vs `CodedFrames`).
pub fn write_hevc_coded_frames_x<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    payload: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        if is_keyframe {
            crate::ex_video::ExFrameType::KeyFrame
        } else {
            crate::ex_video::ExFrameType::InterFrame
        },
        crate::ex_video::ExPacketType::CodedFramesX,
        crate::ex_video::FOURCC_HVC1,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, payload)
}

/// Write a VVC (`vvc1`) Enhanced-RTMP SequenceStart tag carrying the
/// `VVCDecoderConfigurationRecord`.
pub fn write_vvc_sequence_start<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    config_record: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        crate::ex_video::ExFrameType::KeyFrame,
        crate::ex_video::ExPacketType::SequenceStart,
        crate::ex_video::FOURCC_VVC1,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, config_record)
}

/// Write a VVC (`vvc1`) Enhanced-RTMP CodedFrames tag with SI24 CTO.
pub fn write_vvc_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    is_keyframe: bool,
    composition_time_offset_ms: i32,
    payload: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        if is_keyframe {
            crate::ex_video::ExFrameType::KeyFrame
        } else {
            crate::ex_video::ExFrameType::InterFrame
        },
        crate::ex_video::ExPacketType::CodedFrames,
        crate::ex_video::FOURCC_VVC1,
        Some(composition_time_offset_ms),
    );
    write_ex_video_tag(w, timestamp_ms, &header, payload)
}

/// Write an Ex-video `SequenceEnd` tag for the given FourCc (no payload).
pub fn write_ex_video_sequence_end<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    fourcc: u32,
) -> Result<u32> {
    let header = single_track_ex_video_header(
        crate::ex_video::ExFrameType::KeyFrame,
        crate::ex_video::ExPacketType::SequenceEnd,
        fourcc,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, &[])
}

/// Write an Ex-video `Metadata` tag (HDR colorInfo / future extensions).
/// `amf_payload` is the AMF-encoded `["colorInfo", Object]` body.
pub fn write_ex_video_metadata<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    fourcc: u32,
    amf_payload: &[u8],
) -> Result<u32> {
    let header = single_track_ex_video_header(
        crate::ex_video::ExFrameType::KeyFrame,
        crate::ex_video::ExPacketType::Metadata,
        fourcc,
        None,
    );
    write_ex_video_tag(w, timestamp_ms, &header, amf_payload)
}

/// Write an Ex-video `Metadata` tag carrying an HDR `colorInfo` object
/// (Veovera `enhanced-rtmp-v2` §"Metadata Frame"). Encodes the supplied
/// [`crate::color_info::ColorInfo`] via the AMF0 grammar described in
/// the spec's `ColorInfo` type block and packages it in a
/// `videoPacketType = Metadata` tag for the given FourCC. The output
/// is symmetric with [`crate::FlvDemuxer`]'s
/// `harvest_video_metadata_frame` walker — every populated field
/// surfaces under `metadata["colorinfo.<group>.<key>"]` after the
/// round-trip.
pub fn write_ex_video_color_info<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    fourcc: u32,
    color_info: &crate::color_info::ColorInfo,
) -> Result<u32> {
    let amf = color_info.encode_amf()?;
    write_ex_video_metadata(w, timestamp_ms, fourcc, &amf)
}

/// Write the spec-recommended colorInfo reset — an Ex-video `Metadata`
/// tag whose payload is `["colorInfo", Undefined]` (Veovera
/// `enhanced-rtmp-v2` §"Metadata Frame": "To reset to the original
/// color state you can send colorInfo with a value of Undefined (the
/// RECOMMENDED approach) or an empty object."). The demuxer drops
/// every prior `colorinfo.*` metadata entry and leaves the
/// `metadata["colorinfo"] = "undefined"` sentinel.
pub fn write_ex_video_color_info_reset<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    fourcc: u32,
) -> Result<u32> {
    let amf = crate::color_info::encode_amf_reset();
    write_ex_video_metadata(w, timestamp_ms, fourcc, &amf)
}

// ---- Ex-audio single-track convenience writers ----------------------------

fn single_track_ex_audio_header(
    packet_type: crate::ex_audio::ExAudioPacketType,
    fourcc: u32,
) -> crate::ex_audio::ExAudioTagHeader {
    crate::ex_audio::ExAudioTagHeader {
        packet_type,
        fourcc: Some(fourcc),
        multitrack: None,
        timestamp_offset_nano: 0,
        mod_ex_entries: Vec::new(),
        bytes_consumed: 0,
    }
}

/// Write an Opus Enhanced-RTMP SequenceStart tag carrying the RFC 7845
/// OpusHead ID header.
pub fn write_opus_sequence_start<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    opus_head: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::SequenceStart,
        crate::ex_audio::FOURCC_OPUS,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, opus_head)
}

/// Write an Opus Enhanced-RTMP CodedFrames tag carrying one Opus packet.
pub fn write_opus_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    packet: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::CodedFrames,
        crate::ex_audio::FOURCC_OPUS,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, packet)
}

/// Write a FLAC (`fLaC`) Enhanced-RTMP SequenceStart tag carrying the
/// Xiph `fLaC` marker + STREAMINFO metadata block.
pub fn write_flac_sequence_start<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    flac_marker_and_streaminfo: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::SequenceStart,
        crate::ex_audio::FOURCC_FLAC,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, flac_marker_and_streaminfo)
}

/// Write a FLAC (`fLaC`) Enhanced-RTMP CodedFrames tag carrying one
/// FLAC frame.
pub fn write_flac_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    flac_frame: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::CodedFrames,
        crate::ex_audio::FOURCC_FLAC,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, flac_frame)
}

/// Write an AC-3 (`ac-3`) Enhanced-RTMP CodedFrames tag carrying one
/// ATSC AC-3 sync frame. AC-3 is self-synchronising so a
/// `SequenceStart` is not required.
pub fn write_ac3_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    ac3_sync_frame: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::CodedFrames,
        crate::ex_audio::FOURCC_AC3,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, ac3_sync_frame)
}

/// Write an E-AC-3 (`ec-3`) Enhanced-RTMP CodedFrames tag carrying one
/// ATSC E-AC-3 sync frame.
pub fn write_eac3_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    eac3_sync_frame: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::CodedFrames,
        crate::ex_audio::FOURCC_EAC3,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, eac3_sync_frame)
}

/// Write an MP3 Enhanced-RTMP CodedFrames tag (`.mp3` FourCc — distinct
/// from the legacy `SoundFormat=2` MP3 path that still uses
/// [`write_mp3_tag`]).
pub fn write_mp3_ex_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    mp3_frame: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::CodedFrames,
        crate::ex_audio::FOURCC_MP3,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, mp3_frame)
}

/// Write an AAC (`mp4a`) Enhanced-RTMP SequenceStart tag carrying the
/// ISO/IEC 14496-3 `AudioSpecificConfig`.
///
/// This is the FourCc-mode counterpart to legacy
/// [`write_aac_raw_tag`] — the same AAC stream can be expressed either
/// via `SoundFormat=10` (legacy) or FourCc `mp4a` (Ex). The decoder
/// resolves the codec id identically (`"aac"`).
pub fn write_aac_ex_sequence_start<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    audio_specific_config: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::SequenceStart,
        crate::ex_audio::FOURCC_AAC,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, audio_specific_config)
}

/// Write an AAC (`mp4a`) Enhanced-RTMP CodedFrames tag carrying one raw
/// AAC access unit.
pub fn write_aac_ex_coded_frames<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    raw_au: &[u8],
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::CodedFrames,
        crate::ex_audio::FOURCC_AAC,
    );
    write_ex_audio_tag(w, timestamp_ms, &header, raw_au)
}

/// Write an Ex-audio `SequenceEnd` tag for the given FourCc (no payload).
pub fn write_ex_audio_sequence_end<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    fourcc: u32,
) -> Result<u32> {
    let header =
        single_track_ex_audio_header(crate::ex_audio::ExAudioPacketType::SequenceEnd, fourcc);
    write_ex_audio_tag(w, timestamp_ms, &header, &[])
}

/// Write an Ex-audio `MultichannelConfig` tag for the given FourCc
/// (enhanced-rtmp-v2 §`ExAudioTagBody`): the typed
/// [`crate::multichannel::MultichannelConfig`] is validated and
/// serialised as the tag payload.
///
/// The demuxer parses the same body back into
/// `metadata["multichannelconfig.*"]` entries and lifts the channel
/// count into `CodecParameters::channels`, so the writer and parser
/// share one source of truth. Invalid configs (see
/// [`crate::multichannel::MultichannelConfig::to_bytes`]) error before
/// any bytes reach `w`.
pub fn write_ex_audio_multichannel_config<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    fourcc: u32,
    config: &crate::multichannel::MultichannelConfig,
) -> Result<u32> {
    let header = single_track_ex_audio_header(
        crate::ex_audio::ExAudioPacketType::MultichannelConfig,
        fourcc,
    );
    let mut payload = Vec::with_capacity(2 + 4 + config.mapping.as_ref().map_or(0, Vec::len));
    config.to_bytes(&mut payload)?;
    write_ex_audio_tag(w, timestamp_ms, &header, &payload)
}

fn u24_to_be(v: u32) -> [u8; 3] {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// Parsed 11-byte tag header.
#[derive(Clone, Copy, Debug)]
pub struct TagHeader {
    pub tag_type_raw: u8,
    pub kind: Option<TagType>,
    pub data_size: u32,
    /// Full 32-bit timestamp (milliseconds). The upper 8 bits are the
    /// "TimestampExtended" byte.
    pub timestamp_ms: u32,
    pub stream_id: u32,
    /// True when bit 0x20 of the tag-type byte is set (the "Filter"
    /// flag — encryption hint). We surface it but otherwise treat the
    /// payload as cleartext; a filtered tag needs the consumer to
    /// resolve the filter descriptor themselves.
    pub filter: bool,
}

impl TagHeader {
    /// Read an 11-byte tag header from `r`. Returns `Error::Eof` if
    /// `r` is already at end-of-file on entry (distinct from a
    /// truncated / partial read, which surfaces as `Io(UnexpectedEof)`).
    pub fn read<R: Read + ?Sized>(r: &mut R) -> Result<Self> {
        let mut buf = [0u8; TAG_HEADER_LEN as usize];
        // Read first byte with a distinct EOF path so callers can
        // cleanly stop iterating at the end of the tag stream.
        let mut first = [0u8; 1];
        match r.read(&mut first) {
            Ok(0) => return Err(Error::Eof),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                r.read_exact(&mut first)?;
            }
            Err(e) => return Err(e.into()),
        }
        buf[0] = first[0];
        r.read_exact(&mut buf[1..])?;
        let tag_type_raw = buf[0];
        let filter = (tag_type_raw & 0x20) != 0;
        let kind = TagType::from_u8(tag_type_raw);
        let data_size = u24_be(&buf[1..4]);
        let ts_low = u24_be(&buf[4..7]);
        let ts_high = buf[7] as u32;
        let timestamp_ms = (ts_high << 24) | ts_low;
        let stream_id = u24_be(&buf[8..11]);
        Ok(Self {
            tag_type_raw,
            kind,
            data_size,
            timestamp_ms,
            stream_id,
            filter,
        })
    }
}

fn u24_be(b: &[u8]) -> u32 {
    debug_assert!(b.len() >= 3);
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32)
}

// ---- audio codec map -------------------------------------------------------

pub const AUDIO_CODEC_LPCM_NATIVE: u8 = 0;
pub const AUDIO_CODEC_ADPCM: u8 = 1;
pub const AUDIO_CODEC_MP3: u8 = 2;
pub const AUDIO_CODEC_LPCM_LE: u8 = 3;
pub const AUDIO_CODEC_NELLYMOSER_16K_MONO: u8 = 4;
pub const AUDIO_CODEC_NELLYMOSER_8K_MONO: u8 = 5;
pub const AUDIO_CODEC_NELLYMOSER: u8 = 6;
pub const AUDIO_CODEC_ALAW: u8 = 7;
pub const AUDIO_CODEC_MULAW: u8 = 8;
pub const AUDIO_CODEC_AAC: u8 = 10;
pub const AUDIO_CODEC_SPEEX: u8 = 11;
pub const AUDIO_CODEC_MP3_8K: u8 = 14;
pub const AUDIO_CODEC_DEVICE_SPECIFIC: u8 = 15;

/// Decoded audio tag header — the first byte of every audio payload.
#[derive(Clone, Copy, Debug)]
pub struct AudioTagHeader {
    pub codec_id: u8,
    /// 0=5.5 kHz, 1=11 kHz, 2=22 kHz, 3=44 kHz (spec rate index).
    pub sample_rate_idx: u8,
    /// False = 8-bit, True = 16-bit.
    pub is_16bit: bool,
    /// False = mono, True = stereo.
    pub is_stereo: bool,
}

impl AudioTagHeader {
    pub fn parse(b: u8) -> Self {
        Self {
            codec_id: b >> 4,
            sample_rate_idx: (b >> 2) & 0x03,
            is_16bit: (b & 0x02) != 0,
            is_stereo: (b & 0x01) != 0,
        }
    }

    /// Pack the header back into its wire byte (spec §E.4.2.1):
    /// `SoundFormat UB[4] | SoundRate UB[2] | SoundSize UB[1] |
    /// SoundType UB[1]`. Inverse of [`AudioTagHeader::parse`].
    pub fn to_byte(self) -> u8 {
        ((self.codec_id & 0x0F) << 4)
            | ((self.sample_rate_idx & 0x03) << 2)
            | (u8::from(self.is_16bit) << 1)
            | u8::from(self.is_stereo)
    }

    pub fn sample_rate_hz(self) -> u32 {
        match self.codec_id {
            AUDIO_CODEC_MP3_8K => 8_000,
            AUDIO_CODEC_AAC => 44_100, // real rate comes from AudioSpecificConfig
            AUDIO_CODEC_NELLYMOSER_8K_MONO => 8_000,
            AUDIO_CODEC_NELLYMOSER_16K_MONO => 16_000,
            _ => match self.sample_rate_idx {
                0 => 5_512,
                1 => 11_025,
                2 => 22_050,
                _ => 44_100,
            },
        }
    }

    pub fn channels(self) -> u16 {
        if self.is_stereo {
            2
        } else {
            1
        }
    }
}

/// Short stable id string for the audio codec. Matches the strings
/// oxideav-codec uses elsewhere (`"mp3"`, `"aac"`, `"pcm_s16le"`, …).
/// Unknown ids fall back to `flv:audio:<N>`.
pub fn audio_codec_id_str(id: u8) -> String {
    match id {
        AUDIO_CODEC_LPCM_NATIVE => "pcm_s16le".into(),
        AUDIO_CODEC_ADPCM => "adpcm_swf".into(),
        AUDIO_CODEC_MP3 | AUDIO_CODEC_MP3_8K => "mp3".into(),
        AUDIO_CODEC_LPCM_LE => "pcm_s16le".into(),
        AUDIO_CODEC_NELLYMOSER_8K_MONO
        | AUDIO_CODEC_NELLYMOSER_16K_MONO
        | AUDIO_CODEC_NELLYMOSER => "nellymoser".into(),
        AUDIO_CODEC_ALAW => "pcm_alaw".into(),
        AUDIO_CODEC_MULAW => "pcm_mulaw".into(),
        AUDIO_CODEC_AAC => "aac".into(),
        AUDIO_CODEC_SPEEX => "speex".into(),
        AUDIO_CODEC_DEVICE_SPECIFIC => "flv:audio:device".into(),
        other => format!("flv:audio:{other}"),
    }
}

/// Resolve an `onMetaData` `audiocodecid` value into its stable codec
/// string, accepting both encodings the Enhanced-RTMP-v2 spec allows
/// for that property (§"Enhancing onMetaData"):
///
/// * a legacy 4-bit `SoundFormat` value (E.4.2.1) — routed through
///   [`audio_codec_id_str`];
/// * a packed [FourCC] UI32 stamped via `makeFourCc()` — e.g.
///   `"Opus" == 0x4F707573 == 1_332_770_163` — routed through
///   [`crate::ex_audio::fourcc_audio_codec_id_str`] so it resolves to
///   the same `"opus"` / `"flac"` / `"ac3"` / … string the wire-side
///   ExAudio path produces.
///
/// The discriminator is the same one the spec example relies on: a
/// FourCC packs four printable ASCII bytes, so every spec-defined
/// FourCC is `> 0xFF`. A value in `0..=0xFF` is treated as the legacy
/// id. A value `> 0xFF` whose four bytes are not all printable ASCII
/// is neither a legacy id nor a well-formed FourCC; it falls through
/// the FourCC resolver's own `flv:exaudio:0x…` hex carrier so the
/// caller still sees the raw value rather than `None`.
pub fn audio_codec_id_str_u32(id: u32) -> String {
    if id <= u8::MAX as u32 {
        audio_codec_id_str(id as u8)
    } else {
        crate::ex_audio::fourcc_audio_codec_id_str(id)
    }
}

// ---- video codec map -------------------------------------------------------

pub const VIDEO_CODEC_JPEG: u8 = 1;
pub const VIDEO_CODEC_FLV1: u8 = 2;
pub const VIDEO_CODEC_SCREEN_V1: u8 = 3;
pub const VIDEO_CODEC_VP6F: u8 = 4;
pub const VIDEO_CODEC_VP6A: u8 = 5;
pub const VIDEO_CODEC_SCREEN_V2: u8 = 6;
pub const VIDEO_CODEC_H264: u8 = 7;

/// FrameType field (bits 7..4 of the first video byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    Key,
    Inter,
    DisposableInter,
    GeneratedKey,
    /// Per E.4.3.1 "video info/command frame" — body[1] is a UI8 command
    /// byte, not codec data. Commands defined: `0` = start of client-side
    /// seeking video sequence, `1` = end. The flag is surfaced so the
    /// demuxer can route the tag away from the decoder (no codec on
    /// earth wants to parse it as a frame).
    VideoInfo,
    Unknown(u8),
}

impl FrameType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Key,
            2 => Self::Inter,
            3 => Self::DisposableInter,
            4 => Self::GeneratedKey,
            5 => Self::VideoInfo,
            other => Self::Unknown(other),
        }
    }

    /// The 4-bit FrameType nibble that goes in the high bits of the
    /// video tag's first byte (spec §E.4.3.1). Inverse of
    /// [`FrameType::from_u8`].
    ///
    /// `Unknown(n)` is masked to 4 bits — values outside `0..=15` are
    /// not representable in the wire field and the caller's choice to
    /// build them is preserved modulo 16.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Key => 1,
            Self::Inter => 2,
            Self::DisposableInter => 3,
            Self::GeneratedKey => 4,
            Self::VideoInfo => 5,
            Self::Unknown(n) => n & 0x0F,
        }
    }
}

/// Body-byte command values for [`FrameType::VideoInfo`] tags
/// (spec E.4.3.1 / E.4.3 VideoTagBody, IF FrameType == 5 branch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoInfoCommand {
    /// `0` — start of client-side-seeking video sequence.
    StartClientSeek,
    /// `1` — end of client-side-seeking video sequence.
    EndClientSeek,
    /// Any other UI8 — spec-unknown but preserved verbatim.
    Unknown(u8),
}

impl VideoInfoCommand {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::StartClientSeek,
            1 => Self::EndClientSeek,
            other => Self::Unknown(other),
        }
    }

    /// Wire UI8 value for this command (spec §E.4.3.1 IF FrameType==5).
    /// Inverse of [`VideoInfoCommand::from_u8`].
    pub fn to_u8(self) -> u8 {
        match self {
            Self::StartClientSeek => 0,
            Self::EndClientSeek => 1,
            Self::Unknown(n) => n,
        }
    }
}

/// Decoded video tag header — the first byte of every video payload.
#[derive(Clone, Copy, Debug)]
pub struct VideoTagHeader {
    pub frame_type: FrameType,
    pub codec_id: u8,
}

impl VideoTagHeader {
    pub fn parse(b: u8) -> Self {
        Self {
            frame_type: FrameType::from_u8(b >> 4),
            codec_id: b & 0x0F,
        }
    }

    /// Pack the header back into its wire byte (spec §E.4.3.1):
    /// `FrameType UB[4] | CodecID UB[4]`. Inverse of
    /// [`VideoTagHeader::parse`].
    pub fn to_byte(self) -> u8 {
        (self.frame_type.to_u8() << 4) | (self.codec_id & 0x0F)
    }

    pub fn is_keyframe(self) -> bool {
        matches!(self.frame_type, FrameType::Key | FrameType::GeneratedKey)
    }

    /// True when this is a video info / command frame (spec
    /// FrameType == 5). The body of such tags is **not** codec data —
    /// the demuxer surfaces them with `flags.discard` set so decoders
    /// skip them.
    pub fn is_video_info(self) -> bool {
        matches!(self.frame_type, FrameType::VideoInfo)
    }
}

// ---- encrypted-tag (Annex F) preamble parser -----------------------------

/// Parsed [`EncryptionTagHeader`] + [`FilterParams`] preamble of an
/// encrypted FLV tag (spec Annex F.3.1 / F.3.2).
///
/// Layout when the `Filter` bit of the [`TagHeader`] is set:
///
/// ```text
///   0   1   NumFilters (UI8, shall be 1)
///   1   N   FilterName (UTF-8, NUL-terminated STRING)
///   N+1 3   Length     (UI24 BE — bytes of FilterParams that follow)
///   N+4 L   FilterParams
/// ```
///
/// Two FilterName values are spec-defined:
/// `"Encryption"` (FLV encryption version 1) and `"SE"` (Selective
/// Encryption, version 2). Both wrap an `EncryptionFilterParams` or
/// `SelectiveEncryptionFilterParams` body whose first member is the
/// 16-byte AES-CBC initialisation vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedTagPreamble {
    pub filter_name: String,
    /// `true` for non-selective `"Encryption"`, `false` for `"SE"`.
    /// When `"SE"`, [`Self::is_encrypted`] tells whether *this* tag is
    /// actually ciphertext (vs in-the-clear with the SE wrapper).
    pub full_encryption: bool,
    /// Selective-encryption per-tag indicator. Always `true` for
    /// non-selective encryption; for `"SE"`, the UB[1] EncryptedAU bit.
    pub is_encrypted: bool,
    /// AES-CBC IV — present when [`Self::is_encrypted`] is true.
    pub iv: Option<[u8; 16]>,
    /// Number of bytes consumed from the tag body. The remaining
    /// `tag_data_size - bytes_consumed` bytes are the (possibly
    /// encrypted) ciphertext body.
    pub bytes_consumed: usize,
}

impl EncryptedTagPreamble {
    /// Parse the EncryptionTagHeader + FilterParams from the start of a
    /// filtered tag body. Returns `Err(Error::InvalidData)` on a
    /// malformed preamble (truncated string, NumFilters != 1, unknown
    /// FilterName, missing IV).
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.is_empty() {
            return Err(Error::invalid("FLV encrypted tag: empty body"));
        }
        let num_filters = body[0];
        if num_filters != 1 {
            return Err(Error::invalid(format!(
                "FLV encrypted tag: NumFilters {num_filters} != 1"
            )));
        }
        // STRING (UTF-8, NUL-terminated, per E.4.4 "STRING" type).
        let name_start = 1;
        let nul = body[name_start..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::invalid("FLV encrypted tag: unterminated FilterName"))?;
        let filter_name = std::str::from_utf8(&body[name_start..name_start + nul])
            .map_err(|_| Error::invalid("FLV encrypted tag: non-UTF-8 FilterName"))?
            .to_string();
        let name_end = name_start + nul + 1; // skip NUL
        if body.len() < name_end + 3 {
            return Err(Error::invalid("FLV encrypted tag: truncated Length"));
        }
        let params_len = ((body[name_end] as usize) << 16)
            | ((body[name_end + 1] as usize) << 8)
            | (body[name_end + 2] as usize);
        let params_start = name_end + 3;
        if body.len() < params_start + params_len {
            return Err(Error::invalid(
                "FLV encrypted tag: truncated FilterParams body",
            ));
        }
        let params = &body[params_start..params_start + params_len];

        let (full_encryption, is_encrypted, iv) = match filter_name.as_str() {
            "Encryption" => {
                // EncryptionFilterParams: UI8[16] IV.
                if params.len() < 16 {
                    return Err(Error::invalid(
                        "FLV Encryption FilterParams: IV must be 16 bytes",
                    ));
                }
                let mut iv = [0u8; 16];
                iv.copy_from_slice(&params[..16]);
                (true, true, Some(iv))
            }
            "SE" => {
                // SelectiveEncryptionFilterParams: UB[1] EncryptedAU +
                // UB[7] Reserved + IF EncryptedAU UI8[16] IV.
                if params.is_empty() {
                    return Err(Error::invalid("FLV SE FilterParams: empty"));
                }
                let encrypted_au = (params[0] >> 7) & 0x01 == 1;
                let iv = if encrypted_au {
                    if params.len() < 17 {
                        return Err(Error::invalid(
                            "FLV SE FilterParams: truncated IV (EncryptedAU=1 needs 1+16 bytes)",
                        ));
                    }
                    let mut iv = [0u8; 16];
                    iv.copy_from_slice(&params[1..17]);
                    Some(iv)
                } else {
                    None
                };
                (false, encrypted_au, iv)
            }
            other => {
                return Err(Error::invalid(format!(
                    "FLV encrypted tag: unknown FilterName {other:?}"
                )));
            }
        };
        Ok(Self {
            filter_name,
            full_encryption,
            is_encrypted,
            iv,
            bytes_consumed: params_start + params_len,
        })
    }
}

pub fn video_codec_id_str(id: u8) -> String {
    match id {
        VIDEO_CODEC_JPEG => "mjpeg".into(),
        VIDEO_CODEC_FLV1 => "flv1".into(),
        VIDEO_CODEC_SCREEN_V1 => "flashsv".into(),
        VIDEO_CODEC_VP6F => "vp6f".into(),
        VIDEO_CODEC_VP6A => "vp6a".into(),
        VIDEO_CODEC_SCREEN_V2 => "flashsv2".into(),
        VIDEO_CODEC_H264 => "h264".into(),
        other => format!("flv:video:{other}"),
    }
}

/// Resolve an `onMetaData` `videocodecid` value into its stable codec
/// string, accepting both encodings the Enhanced-RTMP-v2 spec allows
/// for that property (§"Enhancing onMetaData"):
///
/// * a legacy 4-bit `CodecID` value (E.4.3.1) — routed through
///   [`video_codec_id_str`];
/// * a packed [FourCC] UI32 stamped via `makeFourCc()` — e.g.
///   `"av01" == 0x61763031 == 1_635_135_537` — routed through
///   [`crate::ex_video::fourcc_codec_id_str`] so it resolves to the
///   same `"av1"` / `"vp9"` / `"h265"` / … string the wire-side
///   ExVideo path produces.
///
/// See [`audio_codec_id_str_u32`] for the legacy-vs-FourCC
/// discriminator rationale (a FourCC packs four printable ASCII bytes,
/// so it is always `> 0xFF`).
pub fn video_codec_id_str_u32(id: u32) -> String {
    if id <= u8::MAX as u32 {
        video_codec_id_str(id as u8)
    } else {
        crate::ex_video::fourcc_codec_id_str(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn tag_header_roundtrip() {
        // audio tag, 7 bytes payload, ts 0x00000003, stream 0
        let bytes = [
            0x08, 0x00, 0x00, 0x07, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00,
        ];
        let h = TagHeader::read(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(h.kind, Some(TagType::Audio));
        assert_eq!(h.data_size, 7);
        assert_eq!(h.timestamp_ms, 3);
        assert_eq!(h.stream_id, 0);
        assert!(!h.filter);
    }

    #[test]
    fn tag_header_extended_timestamp() {
        // video tag, ts extended by 0x01 in high byte -> 0x01_00_00_FF.
        let bytes = [
            0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x01, 0x00, 0x00, 0x00,
        ];
        let h = TagHeader::read(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(h.kind, Some(TagType::Video));
        assert_eq!(h.timestamp_ms, 0x0100_00FF);
    }

    #[test]
    fn audio_header_decode() {
        // codec=10 (AAC), rate=3 (44kHz), 16-bit, stereo -> 0xAF
        let h = AudioTagHeader::parse(0xAF);
        assert_eq!(h.codec_id, 10);
        assert_eq!(h.sample_rate_idx, 3);
        assert!(h.is_16bit);
        assert!(h.is_stereo);
    }

    #[test]
    fn video_header_decode() {
        // frame_type=1 (key), codec_id=4 (vp6f) -> 0x14
        let h = VideoTagHeader::parse(0x14);
        assert_eq!(h.codec_id, VIDEO_CODEC_VP6F);
        assert!(h.is_keyframe());
    }

    #[test]
    fn eof_read_on_empty() {
        let mut c = Cursor::new(&[] as &[u8]);
        assert!(matches!(TagHeader::read(&mut c), Err(Error::Eof)));
    }

    #[test]
    fn encryption_preamble_v1_full_encryption() {
        // NumFilters=1, FilterName="Encryption\0", Length=16, IV bytes 0x10..0x1F.
        let mut body = vec![1u8];
        body.extend_from_slice(b"Encryption\0");
        body.push(0); // length hi
        body.push(0); // length mid
        body.push(16); // length lo (16 bytes IV)
        let iv: [u8; 16] = std::array::from_fn(|i| 0x10 + i as u8);
        body.extend_from_slice(&iv);
        body.extend_from_slice(b"cipherbytes"); // trailing ciphertext

        let p = EncryptedTagPreamble::parse(&body).unwrap();
        assert_eq!(p.filter_name, "Encryption");
        assert!(p.full_encryption);
        assert!(p.is_encrypted);
        assert_eq!(p.iv, Some(iv));
        // 1 + len("Encryption\0")=11 + 3 + 16 = 31
        assert_eq!(p.bytes_consumed, 31);
        assert_eq!(&body[p.bytes_consumed..], b"cipherbytes");
    }

    #[test]
    fn encryption_preamble_v2_selective_unencrypted() {
        // SE with EncryptedAU=0: just one byte, no IV.
        let mut body = vec![1u8];
        body.extend_from_slice(b"SE\0");
        body.push(0);
        body.push(0);
        body.push(1); // length = 1
        body.push(0x00); // EncryptedAU=0
        body.extend_from_slice(b"plaintext");

        let p = EncryptedTagPreamble::parse(&body).unwrap();
        assert_eq!(p.filter_name, "SE");
        assert!(!p.full_encryption);
        assert!(!p.is_encrypted);
        assert_eq!(p.iv, None);
        // 1 + 3 + 3 + 1 = 8
        assert_eq!(p.bytes_consumed, 8);
        assert_eq!(&body[p.bytes_consumed..], b"plaintext");
    }

    #[test]
    fn encryption_preamble_v2_selective_encrypted_with_iv() {
        let mut body = vec![1u8];
        body.extend_from_slice(b"SE\0");
        body.push(0);
        body.push(0);
        body.push(17); // length = 1 + 16
        body.push(0x80); // EncryptedAU=1
        let iv: [u8; 16] = [0xAA; 16];
        body.extend_from_slice(&iv);

        let p = EncryptedTagPreamble::parse(&body).unwrap();
        assert!(p.is_encrypted);
        assert_eq!(p.iv, Some(iv));
    }

    #[test]
    fn encryption_preamble_rejects_unknown_filter_name() {
        let mut body = vec![1u8];
        body.extend_from_slice(b"Bogus\0");
        body.push(0);
        body.push(0);
        body.push(0); // length 0
        assert!(matches!(
            EncryptedTagPreamble::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn encryption_preamble_rejects_truncated_length() {
        // FilterName terminator present but no length follows.
        let body = b"\x01Encryption\0\x00";
        assert!(matches!(
            EncryptedTagPreamble::parse(body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn audio_header_byte_round_trips() {
        // Every legal 4-bit codec / 2-bit rate / size / type combination
        // packs and unpacks losslessly.
        for codec_id in 0u8..16 {
            for rate in 0u8..4 {
                for &is_16bit in &[false, true] {
                    for &is_stereo in &[false, true] {
                        let h = AudioTagHeader {
                            codec_id,
                            sample_rate_idx: rate,
                            is_16bit,
                            is_stereo,
                        };
                        let back = AudioTagHeader::parse(h.to_byte());
                        assert_eq!(back.codec_id, codec_id);
                        assert_eq!(back.sample_rate_idx, rate);
                        assert_eq!(back.is_16bit, is_16bit);
                        assert_eq!(back.is_stereo, is_stereo);
                    }
                }
            }
        }
        // Worked example from `audio_header_decode`: AAC/44k/16-bit/stereo
        // packs to 0xAF.
        let h = AudioTagHeader {
            codec_id: 10,
            sample_rate_idx: 3,
            is_16bit: true,
            is_stereo: true,
        };
        assert_eq!(h.to_byte(), 0xAF);
    }

    #[test]
    fn write_tag_emits_header_body_and_trailer() {
        // audio tag, 3-byte body, ts 0x01020304 (exercises the extended
        // timestamp byte), stream 0.
        let mut out = Vec::new();
        let body = [0xAA, 0xBB, 0xCC];
        let total = write_tag(&mut out, TagType::Audio, 0x0102_0304, 0, &body).unwrap();
        // 11 header + 3 body + 4 trailer = 18.
        assert_eq!(total, 18);
        assert_eq!(out.len(), 18);
        // TagType byte.
        assert_eq!(out[0], 0x08);
        // DataSize UI24 = 3.
        assert_eq!(&out[1..4], &[0x00, 0x00, 0x03]);
        // Timestamp low 24 bits = 0x020304, extended = 0x01.
        assert_eq!(&out[4..7], &[0x02, 0x03, 0x04]);
        assert_eq!(out[7], 0x01);
        // StreamID UI24 = 0.
        assert_eq!(&out[8..11], &[0x00, 0x00, 0x00]);
        // Body.
        assert_eq!(&out[11..14], &body);
        // PreviousTagSize = 11 + 3 = 14.
        assert_eq!(&out[14..18], &14u32.to_be_bytes());
        // The header we just wrote parses back identically.
        let h = TagHeader::read(&mut Cursor::new(&out)).unwrap();
        assert_eq!(h.kind, Some(TagType::Audio));
        assert_eq!(h.data_size, 3);
        assert_eq!(h.timestamp_ms, 0x0102_0304);
        assert_eq!(h.stream_id, 0);
    }

    #[test]
    fn write_first_previous_tag_size_is_four_zero_bytes() {
        let mut out = Vec::new();
        write_first_previous_tag_size(&mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 0]);
    }

    #[test]
    fn write_mp3_tag_body_layout() {
        let mut out = Vec::new();
        let frame = [0xFF, 0xFB, 0x90, 0x00]; // MPEG-1 L3 sync + bytes
        write_mp3_tag(&mut out, 26, 3, true, true, &frame).unwrap();
        // After the 11-byte tag header: AudioTagHeader byte then frame.
        let ah = AudioTagHeader::parse(out[11]);
        assert_eq!(ah.codec_id, AUDIO_CODEC_MP3);
        assert_eq!(ah.sample_rate_idx, 3);
        assert!(ah.is_16bit && ah.is_stereo);
        assert_eq!(&out[12..16], &frame);
    }

    #[test]
    fn write_aac_raw_tag_prefixes_packet_type() {
        let mut out = Vec::new();
        let au = [0x21, 0x00, 0x03];
        write_aac_raw_tag(&mut out, 0, &au).unwrap();
        // header byte 0xAF, AACPacketType 0x01, then the access unit.
        assert_eq!(out[11], 0xAF);
        assert_eq!(out[12], 0x01);
        assert_eq!(&out[13..16], &au);
    }

    #[test]
    fn write_tag_rejects_oversized_body() {
        // A body claiming > UI24 bytes can't be expressed; the writer
        // rejects it rather than truncating the DataSize field. Use a
        // cheap zero-filled buffer one byte past the limit.
        let big = vec![0u8; (UI24_MAX as usize) + 1];
        let mut out = Vec::new();
        assert!(matches!(
            write_tag(&mut out, TagType::Audio, 0, 0, &big),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn video_header_byte_round_trips() {
        // Every legal FrameType (0..=15 including Unknown) packs with
        // every 4-bit codec_id lossless-ly.
        for ft in [
            FrameType::Key,
            FrameType::Inter,
            FrameType::DisposableInter,
            FrameType::GeneratedKey,
            FrameType::VideoInfo,
            FrameType::Unknown(7),
            FrameType::Unknown(0),
            FrameType::Unknown(15),
        ] {
            for codec in 0u8..16 {
                let h = VideoTagHeader {
                    frame_type: ft,
                    codec_id: codec,
                };
                let back = VideoTagHeader::parse(h.to_byte());
                assert_eq!(back.codec_id, codec);
                // FrameType round-trips exactly because the wire field
                // is 4 bits and we masked Unknown(n) to 4 bits.
                assert_eq!(back.frame_type.to_u8(), ft.to_u8());
            }
        }
        // Worked example: keyframe + AVC (codec 7) packs to 0x17.
        let h = VideoTagHeader {
            frame_type: FrameType::Key,
            codec_id: VIDEO_CODEC_H264,
        };
        assert_eq!(h.to_byte(), 0x17);
    }

    #[test]
    fn write_h263_keyframe_emits_video_tag_with_flv1_id() {
        let mut out = Vec::new();
        let frame = [0x00, 0x00, 0x84, 0x00, 0x07]; // arbitrary H.263 bits
        write_h263_tag(&mut out, 42, true, &frame).unwrap();
        // 11-byte tag header + 1 video header byte + 5 frame bytes + 4 trailer = 21
        assert_eq!(out.len(), 21);
        assert_eq!(out[0], 0x09); // TagType = Video
                                  // VideoTagHeader: FrameType=Key(1) | CodecID=FLV1(2) = 0x12.
        let vh = VideoTagHeader::parse(out[11]);
        assert_eq!(vh.codec_id, VIDEO_CODEC_FLV1);
        assert!(vh.is_keyframe());
        assert_eq!(&out[12..17], &frame);
        // PreviousTagSize = 11 + DataSize(6) = 17.
        assert_eq!(&out[17..21], &17u32.to_be_bytes());
    }

    #[test]
    fn write_vp6_inter_marks_inter_frame() {
        let mut out = Vec::new();
        write_vp6_tag(&mut out, 33, false, &[0xAA, 0xBB, 0xCC]).unwrap();
        let vh = VideoTagHeader::parse(out[11]);
        assert_eq!(vh.codec_id, VIDEO_CODEC_VP6F);
        assert!(!vh.is_keyframe());
        // The frame bytes follow the single header byte.
        assert_eq!(&out[12..15], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn write_vp6a_prefixes_alpha_offset_byte() {
        let mut out = Vec::new();
        let frame = [0x11, 0x22, 0x33, 0x44];
        write_vp6a_tag(&mut out, 0, true, 0x07, &frame).unwrap();
        let vh = VideoTagHeader::parse(out[11]);
        assert_eq!(vh.codec_id, VIDEO_CODEC_VP6A);
        assert!(vh.is_keyframe());
        // VIDEODATA: AlphaOffset(0x07) || frame.
        assert_eq!(out[12], 0x07);
        assert_eq!(&out[13..17], &frame);
    }

    #[test]
    fn write_avc_sequence_header_lays_out_packet_type_and_cto() {
        let mut out = Vec::new();
        // Synthetic AVCDecoderConfigurationRecord: just a few opaque bytes
        // — the writer does not parse it.
        let config = [0x01, 0x42, 0xC0, 0x1F, 0xFF, 0xE1];
        write_avc_sequence_header(&mut out, 0, &config).unwrap();
        // VideoTagHeader byte: Key(1)|H264(7) = 0x17.
        assert_eq!(out[11], 0x17);
        // AVCPacketType = 0.
        assert_eq!(out[12], 0x00);
        // CompositionTime = 0 (SI24 BE).
        assert_eq!(&out[13..16], &[0, 0, 0]);
        // The config record follows verbatim.
        assert_eq!(&out[16..22], &config);
    }

    #[test]
    fn write_avc_nalu_round_trips_negative_composition_time() {
        let mut out = Vec::new();
        let au = [0x00, 0x00, 0x00, 0x05, 0x65, 0x88, 0x84, 0x00, 0x20]; // length-prefixed IDR fragment
                                                                         // CTS = -42 ms (B-frame reordering).
        write_avc_nalu_tag(&mut out, 100, true, -42, &au).unwrap();
        assert_eq!(out[11], 0x17); // Key|H264
        assert_eq!(out[12], 0x01); // AVCPacketType = 1 (NALU)
                                   // SI24 of -42 = two's-complement low 24 bits = 0xFFFFD6.
        let raw = ((out[13] as u32) << 16) | ((out[14] as u32) << 8) | (out[15] as u32);
        let sext = if raw & 0x0080_0000 != 0 {
            raw | 0xFF00_0000
        } else {
            raw
        };
        assert_eq!(sext as i32, -42);
        assert_eq!(&out[16..16 + au.len()], &au);
    }

    #[test]
    fn write_avc_nalu_rejects_out_of_range_composition_time() {
        let mut out = Vec::new();
        // 2^23 is outside SI24 (max is 2^23 - 1).
        assert!(matches!(
            write_avc_nalu_tag(&mut out, 0, false, 1 << 23, &[]),
            Err(Error::InvalidData(_))
        ));
        assert!(matches!(
            write_avc_nalu_tag(&mut out, 0, false, -(1 << 23) - 1, &[]),
            Err(Error::InvalidData(_))
        ));
        // Boundary values are accepted.
        assert!(write_avc_nalu_tag(&mut out, 0, false, (1 << 23) - 1, &[]).is_ok());
        let mut out2 = Vec::new();
        assert!(write_avc_nalu_tag(&mut out2, 0, false, -(1 << 23), &[]).is_ok());
    }

    #[test]
    fn write_avc_end_of_sequence_carries_packet_type_two() {
        let mut out = Vec::new();
        write_avc_end_of_sequence(&mut out, 1000).unwrap();
        assert_eq!(out[12], 0x02); // AVCPacketType = 2 (EOS)
        assert_eq!(&out[13..16], &[0, 0, 0]); // CompositionTime = 0
                                              // Body is exactly 5 bytes (1 video header + 1 packet-type + 3 SI24).
                                              // Total = 11 + 5 + 4 = 20.
        assert_eq!(out.len(), 20);
    }

    #[test]
    fn write_video_info_command_emits_one_byte_body() {
        let mut out = Vec::new();
        write_video_info_command_tag(&mut out, 5, VideoInfoCommand::StartClientSeek).unwrap();
        // FrameType=VideoInfo(5)|CodecID=0 = 0x50.
        assert_eq!(out[11], 0x50);
        assert_eq!(out[12], 0x00); // StartClientSeek command byte
                                   // 11 + 2 + 4 = 17.
        assert_eq!(out.len(), 17);

        let mut out = Vec::new();
        write_video_info_command_tag(&mut out, 5, VideoInfoCommand::EndClientSeek).unwrap();
        assert_eq!(out[12], 0x01);

        let mut out = Vec::new();
        write_video_info_command_tag(&mut out, 5, VideoInfoCommand::Unknown(0xAB)).unwrap();
        assert_eq!(out[12], 0xAB);
    }

    #[test]
    fn video_info_command_to_u8_round_trips() {
        for v in [0u8, 1, 2, 0x7F, 0xAB, 0xFF] {
            assert_eq!(VideoInfoCommand::from_u8(v).to_u8(), v);
        }
    }

    #[test]
    fn video_info_command_decode() {
        // FrameType=5, codec_id=2 (Sorenson) -> 0x52
        let h = VideoTagHeader::parse(0x52);
        assert!(h.is_video_info());
        assert!(!h.is_keyframe());
        assert_eq!(
            VideoInfoCommand::from_u8(0),
            VideoInfoCommand::StartClientSeek
        );
        assert_eq!(
            VideoInfoCommand::from_u8(1),
            VideoInfoCommand::EndClientSeek
        );
        assert_eq!(VideoInfoCommand::from_u8(7), VideoInfoCommand::Unknown(7));
    }
}
