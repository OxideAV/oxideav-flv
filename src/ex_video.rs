//! Enhanced RTMP / E-FLV ExVideoTagHeader (Veovera enhanced-rtmp-v1 §
//! "Defining Additional Video Codecs" and enhanced-rtmp-v2 §"Enhanced
//! Video").
//!
//! Legacy FLV `VideoTagHeader` uses the high bit of its first byte
//! (`FrameType UB[4]`) for `FrameType`. Pre-Y2023 producers never
//! emitted `FrameType` values ≥ 8, so the top bit was always zero —
//! enhanced-RTMP repurposes that bit as the **IsExHeader** flag:
//!
//! ```text
//!   bit 7 (0x80) : IsExHeader (1 = Ex header follows)
//!   bits 6..4    : FrameType (1=key, 2=inter, 3=disposable, 4=gen-key,
//!                              5=command, 6/7=reserved)
//!   bits 3..0    : When IsExHeader=0, CodecID (legacy semantics)
//!                  When IsExHeader=1, PacketType (Ex semantics)
//! ```
//!
//! When IsExHeader=1, the next four bytes are the VideoFourCc (UI32 BE,
//! ASCII), and the body interpretation depends on the (PacketType,
//! FourCc) pair:
//!
//! * `PacketType=0 SequenceStart` — body is the codec's configuration
//!   record (AV1CodecConfigurationRecord / VPCodecConfigurationRecord /
//!   HEVCDecoderConfigurationRecord / etc.).
//! * `PacketType=1 CodedFrames` — body is one (or more) coded frames.
//!   For HEVC + VVC + AVC the body is `SI24 CompositionTimeOffset` then
//!   the frame; for AV1 / VP9 / VP8 the offset is absent.
//! * `PacketType=2 SequenceEnd` — empty body.
//! * `PacketType=3 CodedFramesX` — like CodedFrames but
//!   CompositionTimeOffset is implicitly 0 (3-byte savings).
//! * `PacketType=4 Metadata` — body is AMF-encoded `["colorInfo",
//!   Object]` HDR metadata.
//! * `PacketType=5 MPEG2TSSequenceStart` — bitstream wrapped in MPEG-2
//!   TS (for AV1; mutually exclusive with PacketType=0 per spec).
//! * `PacketType=6` ModEx (v2 extension; parsed but TimestampOffsetNano
//!   is the only currently-defined kind).
//!
//! FourCc values currently spec-defined (enhanced-rtmp-v2 §
//! "Enhanced Video"):
//!
//! ```text
//!   "av01" — AV1
//!   "vp09" — VP9
//!   "vp08" — VP8
//!   "hvc1" — HEVC / H.265
//!   "avc1" — AVC / H.264 (FourCC signaling alternative to CodecID=7)
//!   "vvc1" — VVC / H.266
//! ```
//!
//! Unknown FourCc values fall through to `flv:exvideo:<fourcc>` —
//! the spec requires unrecognised values to fail gracefully, so the
//! demuxer reports them as unknown rather than rejecting the file.

use oxideav_core::{Error, Result};

/// Mask of the IsExHeader bit in the VideoTagHeader's first byte.
pub const EX_HEADER_FLAG: u8 = 0x80;

/// Spec-defined FourCc value for AV1.
pub const FOURCC_AV01: u32 = u32::from_be_bytes(*b"av01");
/// Spec-defined FourCc value for VP9.
pub const FOURCC_VP09: u32 = u32::from_be_bytes(*b"vp09");
/// Spec-defined FourCc value for VP8.
pub const FOURCC_VP08: u32 = u32::from_be_bytes(*b"vp08");
/// Spec-defined FourCc value for HEVC / H.265.
pub const FOURCC_HVC1: u32 = u32::from_be_bytes(*b"hvc1");
/// Spec-defined FourCc value for AVC / H.264 (FourCC signaling).
pub const FOURCC_AVC1: u32 = u32::from_be_bytes(*b"avc1");
/// Spec-defined FourCc value for VVC / H.266.
pub const FOURCC_VVC1: u32 = u32::from_be_bytes(*b"vvc1");

/// Ex-video FrameType (bits 6..4 of the leading byte when IsExHeader=1).
/// Per enhanced-rtmp-v2 these mirror the legacy 1..5 enumeration; we
/// keep the explicit Command variant so callers can recognise it
/// without consulting PacketType.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExFrameType {
    KeyFrame,
    InterFrame,
    DisposableInterFrame,
    GeneratedKeyFrame,
    Command,
    Reserved(u8),
}

impl ExFrameType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::KeyFrame,
            2 => Self::InterFrame,
            3 => Self::DisposableInterFrame,
            4 => Self::GeneratedKeyFrame,
            5 => Self::Command,
            other => Self::Reserved(other),
        }
    }

    pub fn is_keyframe(self) -> bool {
        matches!(self, Self::KeyFrame | Self::GeneratedKeyFrame)
    }
}

/// Ex-video PacketType (bits 3..0 of the leading byte when IsExHeader=1).
/// Values >= 6 are v2 reserved/extension territory; ModEx (7) is the
/// only defined extension so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExPacketType {
    /// `0` — codec configuration record (AV1CodecConfigurationRecord,
    /// HEVCDecoderConfigurationRecord, VPCodecConfigurationRecord, ...).
    SequenceStart,
    /// `1` — coded frames. For HEVC/VVC/AVC the body is prefixed with a
    /// 3-byte SI24 CompositionTimeOffset.
    CodedFrames,
    /// `2` — end of sequence; payload is empty.
    SequenceEnd,
    /// `3` — coded frames with implicit zero CompositionTimeOffset.
    CodedFramesX,
    /// `4` — AMF-encoded metadata frame (HDR colorInfo + future
    /// extensions).
    Metadata,
    /// `5` — bitstream wrapped in MPEG-2 TS framing (AV1).
    Mpeg2TsSequenceStart,
    /// `6` — multitrack mode (v2 only). Followed by a UB[4]
    /// AvMultitrackType + per-track FourCc.
    Multitrack,
    /// `7` — ModEx (v2 only). Followed by a length-prefixed modifier
    /// payload.
    ModEx,
    /// `>= 8` — reserved for future extensions.
    Reserved(u8),
}

impl ExPacketType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::SequenceStart,
            1 => Self::CodedFrames,
            2 => Self::SequenceEnd,
            3 => Self::CodedFramesX,
            4 => Self::Metadata,
            5 => Self::Mpeg2TsSequenceStart,
            6 => Self::Multitrack,
            7 => Self::ModEx,
            other => Self::Reserved(other),
        }
    }
}

/// Result of parsing an Enhanced RTMP ExVideoTagHeader off the start of
/// a filter-clear video tag body.
///
/// Layout when the leading-byte IsExHeader flag is set:
///
/// ```text
///   0   1   IsExHeader|FrameType|PacketType   UI8
///   1   4   VideoFourCc                       UI32 BE
///   5   ?   PacketType-specific payload (e.g. SI24 composition-time
///           offset for HEVC CodedFrames, AV1CodecConfigurationRecord
///           for SequenceStart on FourCC=av01, …)
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExVideoTagHeader {
    pub frame_type: ExFrameType,
    pub packet_type: ExPacketType,
    pub fourcc: u32,
    /// Number of bytes consumed at the front of the tag body. Callers
    /// slice `body[bytes_consumed..]` to recover the payload.
    pub bytes_consumed: usize,
    /// Decoded SI24 composition-time offset (ms) — present only for
    /// HEVC / VVC / AVC `CodedFrames`. `None` otherwise (the spec
    /// implies a zero offset).
    pub composition_time_offset_ms: Option<i32>,
}

impl ExVideoTagHeader {
    /// Try to parse an ExVideoTagHeader from the start of a video tag
    /// body. Returns `Ok(None)` if the IsExHeader flag is not set
    /// (legacy VideoTagHeader path); `Err(...)` on a malformed Ex
    /// header.
    pub fn parse(body: &[u8]) -> Result<Option<Self>> {
        if body.is_empty() {
            return Ok(None);
        }
        let lead = body[0];
        if (lead & EX_HEADER_FLAG) == 0 {
            return Ok(None);
        }
        // Layout per enhanced-rtmp-v1 Table 4 / v2 §Enhanced Video:
        //   bits 6..4 = FrameType, bits 3..0 = PacketType.
        let frame_type = ExFrameType::from_u8((lead >> 4) & 0x07);
        let packet_type_raw = lead & 0x0F;
        let packet_type = ExPacketType::from_u8(packet_type_raw);

        // FourCc — 4 bytes immediately after the leading byte.
        if body.len() < 5 {
            return Err(Error::invalid("FLV Ex video tag: truncated FourCc"));
        }
        let fourcc = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
        let mut bytes_consumed = 5usize;
        let mut composition_time_offset_ms = None;

        // SI24 CompositionTimeOffset is present only for HEVC / VVC /
        // AVC `CodedFrames` (PacketType=1). PacketType=3 (CodedFramesX)
        // explicitly drops it; PacketType=0 (SequenceStart) and
        // PacketType=2 (SequenceEnd) don't carry one either.
        if matches!(packet_type, ExPacketType::CodedFrames)
            && matches!(fourcc, FOURCC_HVC1 | FOURCC_VVC1 | FOURCC_AVC1)
        {
            if body.len() < 8 {
                return Err(Error::invalid(
                    "FLV Ex video tag: truncated CompositionTimeOffset",
                ));
            }
            let raw = ((body[5] as u32) << 16) | ((body[6] as u32) << 8) | (body[7] as u32);
            // Sign-extend 24 bits.
            let sext = if raw & 0x0080_0000 != 0 {
                raw | 0xFF00_0000
            } else {
                raw
            };
            composition_time_offset_ms = Some(sext as i32);
            bytes_consumed = 8;
        }

        Ok(Some(Self {
            frame_type,
            packet_type,
            fourcc,
            bytes_consumed,
            composition_time_offset_ms,
        }))
    }
}

/// Stable codec-id string for an Ex-video FourCc. Matches the strings
/// the registry uses elsewhere (`"h264"`, `"h265"`, `"vp9"`, `"av1"`,
/// …). Unknown FourCcs surface as `flv:exvideo:<fourcc-ascii>` so the
/// caller can log the anomaly.
pub fn fourcc_codec_id_str(fourcc: u32) -> String {
    match fourcc {
        FOURCC_AV01 => "av1".into(),
        FOURCC_VP09 => "vp9".into(),
        FOURCC_VP08 => "vp8".into(),
        FOURCC_HVC1 => "h265".into(),
        FOURCC_AVC1 => "h264".into(),
        FOURCC_VVC1 => "h266".into(),
        other => {
            let bytes = other.to_be_bytes();
            // Show the FourCc as ASCII when printable, hex otherwise.
            if bytes.iter().all(|b| (0x20..=0x7E).contains(b)) {
                format!(
                    "flv:exvideo:{}",
                    std::str::from_utf8(&bytes).unwrap_or("????")
                )
            } else {
                format!("flv:exvideo:0x{other:08X}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_legacy_video_tag_header() {
        // 0x17 = legacy keyframe (FrameType=1) + AVC (CodecID=7), no Ex.
        let body = [0x17, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(ExVideoTagHeader::parse(&body).unwrap(), None);
    }

    #[test]
    fn rejects_empty_body() {
        assert_eq!(ExVideoTagHeader::parse(&[]).unwrap(), None);
    }

    #[test]
    fn av1_sequence_start() {
        // IsExHeader=1, FrameType=1 (key), PacketType=0 (SequenceStart)
        // → 0x10 | 0x80 = 0x90. FourCc = "av01".
        let mut body = vec![0x90];
        body.extend_from_slice(b"av01");
        body.extend_from_slice(&[0xDE, 0xAD]); // dummy config record bytes
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::KeyFrame);
        assert!(h.frame_type.is_keyframe());
        assert_eq!(h.packet_type, ExPacketType::SequenceStart);
        assert_eq!(h.fourcc, FOURCC_AV01);
        assert_eq!(h.bytes_consumed, 5);
        assert_eq!(h.composition_time_offset_ms, None);
        assert_eq!(fourcc_codec_id_str(h.fourcc), "av1");
        assert_eq!(&body[h.bytes_consumed..], &[0xDE, 0xAD]);
    }

    #[test]
    fn hevc_coded_frames_carries_composition_time_offset() {
        // IsExHeader=1, FrameType=2 (inter), PacketType=1 (CodedFrames)
        // → 0x20 | 0x80 | 0x01 = 0xA1. FourCc = "hvc1". CTO = 33.
        let mut body = vec![0xA1];
        body.extend_from_slice(b"hvc1");
        // SI24 BE = 33.
        body.extend_from_slice(&[0x00, 0x00, 0x21]);
        body.extend_from_slice(&[0xCA, 0xFE]); // dummy NALU bytes
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::InterFrame);
        assert!(!h.frame_type.is_keyframe());
        assert_eq!(h.packet_type, ExPacketType::CodedFrames);
        assert_eq!(h.fourcc, FOURCC_HVC1);
        assert_eq!(h.bytes_consumed, 8);
        assert_eq!(h.composition_time_offset_ms, Some(33));
        assert_eq!(fourcc_codec_id_str(h.fourcc), "h265");
        assert_eq!(&body[h.bytes_consumed..], &[0xCA, 0xFE]);
    }

    #[test]
    fn hevc_coded_frames_negative_composition_time_offset() {
        // Negative CTO (-1) → SI24 0xFFFFFF.
        let mut body = vec![0xA1];
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
        body.push(0x00);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.composition_time_offset_ms, Some(-1));
    }

    #[test]
    fn av1_coded_frames_skips_composition_time_offset() {
        // AV1 CodedFrames carries no CTO, so bytes_consumed stays at 5.
        let mut body = vec![0xA1]; // inter + CodedFrames
        body.extend_from_slice(b"av01");
        body.extend_from_slice(&[0xFE, 0xED]);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExPacketType::CodedFrames);
        assert_eq!(h.fourcc, FOURCC_AV01);
        assert_eq!(h.bytes_consumed, 5);
        assert_eq!(h.composition_time_offset_ms, None);
        assert_eq!(&body[h.bytes_consumed..], &[0xFE, 0xED]);
    }

    #[test]
    fn coded_frames_x_optimisation_skips_cto() {
        // PacketType=3 (CodedFramesX) explicitly drops the CTO even on
        // HEVC — the spec note says it's a 3-byte optimisation.
        let mut body = vec![0xA3]; // inter + CodedFramesX
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0xBE, 0xEF]);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExPacketType::CodedFramesX);
        assert_eq!(h.bytes_consumed, 5);
        assert_eq!(h.composition_time_offset_ms, None);
    }

    #[test]
    fn sequence_end_recognised() {
        let mut body = vec![0x92]; // key + SequenceEnd (PacketType=2)
        body.extend_from_slice(b"av01");
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExPacketType::SequenceEnd);
        assert_eq!(h.bytes_consumed, 5);
    }

    #[test]
    fn metadata_packet_type_recognised() {
        let mut body = vec![0x94]; // key + Metadata
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(b"colorInfo-amf-blob");
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExPacketType::Metadata);
        // Metadata has no CTO even on HEVC.
        assert_eq!(h.bytes_consumed, 5);
    }

    #[test]
    fn truncated_fourcc_errors() {
        let body = [0x90, b'a', b'v']; // only 2 of 4 FourCc bytes
        assert!(matches!(
            ExVideoTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_cto_errors() {
        let mut body = vec![0xA1]; // CodedFrames + hvc1 → expects CTO
        body.extend_from_slice(b"hvc1");
        body.push(0x00); // only 1 of 3 CTO bytes
        assert!(matches!(
            ExVideoTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn unknown_fourcc_falls_through() {
        let mut body = vec![0x90];
        body.extend_from_slice(b"zzzz");
        body.push(0x00);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.fourcc, u32::from_be_bytes(*b"zzzz"));
        assert_eq!(fourcc_codec_id_str(h.fourcc), "flv:exvideo:zzzz");
    }

    #[test]
    fn unknown_fourcc_non_ascii_falls_through_as_hex() {
        let mut body = vec![0x90];
        body.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        body.push(0x00);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(fourcc_codec_id_str(h.fourcc), "flv:exvideo:0x01020304");
    }

    #[test]
    fn command_frame_type_recognised() {
        // FrameType=5 with IsExHeader=1 — enhanced-rtmp keeps the
        // command sentinel.
        let mut body = vec![0xD0]; // 0x80 | 0x50 | 0x00 → key=5 + SeqStart
        body.extend_from_slice(b"av01");
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::Command);
        assert!(!h.frame_type.is_keyframe());
    }
}
