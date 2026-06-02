//! Enhanced RTMP / E-FLV ExAudioTagHeader (Veovera enhanced-rtmp-v2 §
//! "Enhanced Audio").
//!
//! Legacy FLV `AudioTagHeader` (spec §E.4.2.1) packs the codec into the
//! high four bits of the first byte (`SoundFormat UB[4]`) and uses the
//! low four bits for `SoundRate UB[2] | SoundSize UB[1] | SoundType
//! UB[1]`. The pre-Y2023 spec reserves `SoundFormat` values 12 and 13;
//! enhanced-rtmp-v2 repurposes `SoundFormat = 9` (formerly reserved) as
//! the **ExHeader** sentinel:
//!
//! ```text
//!   bits 7..4 : SoundFormat (9 = ExHeader → FourCC mode)
//!   bits 3..0 : When SoundFormat != 9, legacy
//!               (SoundRate/SoundSize/SoundType)
//!               When SoundFormat == 9, AudioPacketType UB[4]
//! ```
//!
//! When `SoundFormat == ExHeader` the spec walks (in order):
//!
//! 1. Zero or more `ModEx` (`AudioPacketType = 7`) headers — each is a
//!    length-prefixed modifier blob followed by a fresh `AudioPacketType`
//!    UB[4]. The only currently-defined modifier is `TimestampOffsetNano`
//!    (`AudioPacketModExType = 0`) which carries a 3-byte UI24 storing
//!    a sub-millisecond offset (0..999_999 ns).
//! 2. If the final `AudioPacketType` is `Multitrack` (5): a `UB[4]
//!    AvMultitrackType` + (if not `ManyTracksManyCodecs`) a single
//!    `audioFourCc` (FOURCC, 4 bytes). For `ManyTracksManyCodecs` the
//!    per-track FourCC is fetched inside the body loop instead — this
//!    parser exposes the multitrack outer header and leaves body-side
//!    parsing to the caller (we do not track-split).
//! 3. Otherwise: `audioFourCc` (FOURCC, 4 bytes).
//!
//! Per the spec's `enum AudioFourCc`, the spec-defined values are:
//!
//! ```text
//!   "ac-3"  — AC-3 (ATSC sync frames)
//!   "ec-3"  — E-AC-3
//!   "Opus"  — Opus (RFC 7845 ID header on SequenceStart, Opus packets
//!             on CodedFrames per Appendix B / Section 3 of RFC 6716)
//!   ".mp3"  — MPEG-1/2 Layer III frames
//!   "fLaC"  — FLAC (Xiph fLaC + STREAMINFO on SequenceStart, frames
//!             on CodedFrames)
//!   "mp4a"  — AAC (ISO/IEC 14496-3 AudioSpecificConfig on
//!             SequenceStart, raw access units on CodedFrames)
//! ```
//!
//! Unknown FourCC values fall through to `flv:exaudio:<fourcc>` so the
//! producer can be logged rather than silently dropped (mirroring the
//! ExVideo unknown-FourCC fallback).

use oxideav_core::{Error, Result};

use crate::mod_ex::{walk as mod_ex_walk, ModExEntry};

/// `SoundFormat` value that signals the ExAudio FourCC path.
/// Pre-2023 FLV reserved `SoundFormat = 9`; enhanced-rtmp-v2 reuses it.
pub const SOUND_FORMAT_EX_HEADER: u8 = 9;

/// Spec-defined FourCc value for AC-3.
pub const FOURCC_AC3: u32 = u32::from_be_bytes(*b"ac-3");
/// Spec-defined FourCc value for E-AC-3.
pub const FOURCC_EAC3: u32 = u32::from_be_bytes(*b"ec-3");
/// Spec-defined FourCc value for Opus.
pub const FOURCC_OPUS: u32 = u32::from_be_bytes(*b"Opus");
/// Spec-defined FourCc value for MPEG-1/2 Layer III. Note the leading
/// dot (`".mp3"`) — the spec spells it that way to avoid colliding with
/// any hypothetical 3-byte ASCII identifier.
pub const FOURCC_MP3: u32 = u32::from_be_bytes(*b".mp3");
/// Spec-defined FourCc value for FLAC.
pub const FOURCC_FLAC: u32 = u32::from_be_bytes(*b"fLaC");
/// Spec-defined FourCc value for AAC (FOURCC-mode; legacy codec-id 10
/// covers AAC in pre-2023 FLV).
pub const FOURCC_AAC: u32 = u32::from_be_bytes(*b"mp4a");

/// Ex-audio `AudioPacketType` (bits 3..0 of the leading byte when
/// SoundFormat == ExHeader). Values >= 8 are reserved for future
/// extensions; ModEx (7) is the only currently-defined extension hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExAudioPacketType {
    /// `0` — codec-config blob (Opus ID header / FLAC STREAMINFO /
    /// AAC AudioSpecificConfig). AC-3 / E-AC-3 / MP3 ignore this
    /// packet type per spec (their stream is self-synchronising).
    SequenceStart,
    /// `1` — coded audio frames. Body interpretation depends on
    /// FourCc (ATSC sync frame for AC-3 / E-AC-3, Opus packets for
    /// Opus, MP3 frame for `.mp3`, FLAC frame for `fLaC`, raw AAC
    /// AU for `mp4a`).
    CodedFrames,
    /// `2` — end of audio sequence; payload is empty.
    SequenceEnd,
    /// `3` — reserved.
    Reserved3,
    /// `4` — multichannel configuration. Body encodes
    /// `AudioChannelOrder` + channel count + (per order) speaker
    /// mapping / channel-flags bitmask.
    MultichannelConfig,
    /// `5` — multitrack mode. Followed by `UB[4] AvMultitrackType` +
    /// per-track FourCc (for non-`ManyTracksManyCodecs`).
    Multitrack,
    /// `6` — reserved.
    Reserved6,
    /// `7` — ModEx. Followed by a length-prefixed modifier payload
    /// then a fresh `AudioPacketType` UB[4]. ModEx can chain.
    ModEx,
    /// `>= 8` — reserved for future extensions.
    Reserved(u8),
}

impl ExAudioPacketType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::SequenceStart,
            1 => Self::CodedFrames,
            2 => Self::SequenceEnd,
            3 => Self::Reserved3,
            4 => Self::MultichannelConfig,
            5 => Self::Multitrack,
            6 => Self::Reserved6,
            7 => Self::ModEx,
            other => Self::Reserved(other),
        }
    }

    /// 4-bit wire value (the low nibble of the ExAudio leading byte,
    /// spec enhanced-rtmp-v2 §`ExAudioTagHeader`). Inverse of
    /// [`Self::from_u8`]. `Reserved(n)` is masked to 4 bits.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::SequenceStart => 0,
            Self::CodedFrames => 1,
            Self::SequenceEnd => 2,
            Self::Reserved3 => 3,
            Self::MultichannelConfig => 4,
            Self::Multitrack => 5,
            Self::Reserved6 => 6,
            Self::ModEx => 7,
            Self::Reserved(n) => n & 0x0F,
        }
    }
}

/// `AudioPacketModExType` (UB[4] following the ModEx blob).
///
/// Re-exported from [`crate::mod_ex`] so the audio-side public API
/// keeps its historical name while the actual enum lives next to the
/// shared ModEx walker.
pub use crate::mod_ex::AudioPacketModExType;

/// `AvMultitrackType` (UB[4]). Shared with ExVideo (same enum on the
/// wire); the canonical definition + per-track body splitter live in
/// [`crate::multitrack`]. Re-exported here so the audio-side public API
/// keeps its historical path.
pub use crate::multitrack::AvMultitrackType;

/// Result of parsing an Enhanced RTMP ExAudioTagHeader off the start of
/// a filter-clear audio tag body.
///
/// Layout when `SoundFormat == ExHeader`:
///
/// ```text
///   0   1   SoundFormat(9) | AudioPacketType(UB[4])   UI8
///   ┌── loop while AudioPacketType == ModEx:
///   │  1   1   modExDataSize = UI8 + 1                 (UI8)
///   │  ?   ?   if modExDataSize == 256, UI16 + 1 = 16-bit size
///   │  ?   N   modExData[modExDataSize]                (opaque)
///   │  ?   1   AudioPacketModExType(UB[4]) | next AudioPacketType(UB[4])
///   └──
///   ?   ?   if AudioPacketType == Multitrack:
///           1  AvMultitrackType(UB[4]) | next AudioPacketType(UB[4])
///           4  audioFourCc (FOURCC, except for ManyTracksManyCodecs
///              where FourCc is per-track inside the body)
///         else:
///           4  audioFourCc (FOURCC)
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExAudioTagHeader {
    /// `AudioPacketType` after ModEx unwrapping (and after the
    /// `Multitrack` UB[4] re-read when applicable).
    pub packet_type: ExAudioPacketType,
    /// FourCc identifying the codec. `None` only when the spec's
    /// multitrack `ManyTracksManyCodecs` mode is in effect; in that
    /// case the per-track FourCc is carried inside the body and must
    /// be parsed by the caller.
    pub fourcc: Option<u32>,
    /// Multitrack outer descriptor when `AudioPacketType == Multitrack`,
    /// `None` for the common single-track case.
    pub multitrack: Option<AvMultitrackType>,
    /// Sum of TimestampOffsetNano ModEx offsets read from the header
    /// stack, in nanoseconds (0..999_999 per ModEx). Reported in
    /// nanoseconds so the caller can decide whether to round to the
    /// millisecond timestamp or expose it verbatim.
    pub timestamp_offset_nano: u32,
    /// All ModEx entries parsed off the front of the body, in wire
    /// order. Each entry carries the typed subtype + payload (see
    /// [`crate::mod_ex::ModExEntry`]) so reserved subtypes survive the
    /// header walk with their raw bytes attached.
    pub mod_ex_entries: Vec<ModExEntry>,
    /// Number of bytes consumed at the front of the tag body. The
    /// caller slices `body[bytes_consumed..]` to recover the payload.
    pub bytes_consumed: usize,
}

impl ExAudioTagHeader {
    /// Try to parse an ExAudioTagHeader from the start of an audio tag
    /// body. Returns `Ok(None)` if `SoundFormat != ExHeader` (legacy
    /// AudioTagHeader path); `Err(...)` on a malformed Ex header.
    pub fn parse(body: &[u8]) -> Result<Option<Self>> {
        if body.is_empty() {
            return Ok(None);
        }
        let lead = body[0];
        if (lead >> 4) != SOUND_FORMAT_EX_HEADER {
            return Ok(None);
        }
        let cursor = 1usize;
        let packet_type_raw = lead & 0x0F;

        // Walk all ModEx (AudioPacketType = 7) entries off the front of
        // the body. Shared with the video path via `crate::mod_ex`.
        // Returns the new cursor, the post-ModEx AudioPacketType byte,
        // every parsed entry (typed subtype + raw payload), and the
        // total `TimestampOffsetNano` accumulator.
        let (cursor, packet_type_raw, mod_ex_entries, timestamp_offset_nano) =
            mod_ex_walk::<7>(body, cursor, packet_type_raw)?;
        let mut cursor = cursor;

        let packet_type = ExAudioPacketType::from_u8(packet_type_raw);

        let (multitrack, fourcc) = if packet_type == ExAudioPacketType::Multitrack {
            // Multitrack outer header: UB[4] AvMultitrackType +
            // UB[4] inner AudioPacketType (must NOT itself be
            // Multitrack — that would be malformed).
            if cursor >= body.len() {
                return Err(Error::invalid(
                    "FLV Ex audio tag: truncated multitrack header byte",
                ));
            }
            let mt_byte = body[cursor];
            cursor += 1;
            let mt_type = AvMultitrackType::from_u8((mt_byte >> 4) & 0x0F);
            let inner_pt_raw = mt_byte & 0x0F;
            if ExAudioPacketType::from_u8(inner_pt_raw) == ExAudioPacketType::Multitrack {
                return Err(Error::invalid(
                    "FLV Ex audio tag: nested Multitrack packet type",
                ));
            }
            // For ManyTracksManyCodecs, the per-track FourCc is
            // carried inside the body loop (one FourCc per track),
            // so we leave `fourcc` as None and let the caller walk
            // the per-track structure.
            let fourcc = if mt_type == AvMultitrackType::ManyTracksManyCodecs {
                None
            } else {
                if cursor + 4 > body.len() {
                    return Err(Error::invalid(
                        "FLV Ex audio tag: truncated multitrack FourCc",
                    ));
                }
                let fcc = u32::from_be_bytes([
                    body[cursor],
                    body[cursor + 1],
                    body[cursor + 2],
                    body[cursor + 3],
                ]);
                cursor += 4;
                Some(fcc)
            };
            (Some(mt_type), fourcc)
        } else {
            // Single-track: FourCc directly after the packet-type byte
            // (and any ModEx headers we already consumed).
            if cursor + 4 > body.len() {
                return Err(Error::invalid("FLV Ex audio tag: truncated FourCc"));
            }
            let fcc = u32::from_be_bytes([
                body[cursor],
                body[cursor + 1],
                body[cursor + 2],
                body[cursor + 3],
            ]);
            cursor += 4;
            (None, Some(fcc))
        };

        Ok(Some(Self {
            packet_type,
            fourcc,
            multitrack,
            timestamp_offset_nano,
            mod_ex_entries,
            bytes_consumed: cursor,
        }))
    }

    /// Serialise this header to the wire bytes that opens an
    /// `ExAudioTagBody` — the inverse of [`Self::parse`].
    ///
    /// The output is appended to `out`. After return the caller appends
    /// the codec-specific payload (Opus ID header / FLAC STREAMINFO /
    /// AAC AudioSpecificConfig for `SequenceStart`, AC-3 sync frames for
    /// `CodedFrames`, etc.).
    ///
    /// Coverage and limitations for this slice:
    ///
    /// * Single-track headers are fully supported (the common case for
    ///   every Enhanced-RTMP audio FourCc — Opus / fLaC / ac-3 / ec-3 /
    ///   .mp3 / mp4a).
    /// * ModEx prefix emission is **not** implemented in this slice —
    ///   `mod_ex_entries` is required to be empty and
    ///   `timestamp_offset_nano` must be `0`. Round-tripping the parsed
    ///   ModEx-bearing wire shape will land in a follow-up round.
    /// * Multitrack emission is **not** implemented in this slice. The
    ///   audio parser stores `packet_type = Multitrack` and discards the
    ///   inner `AudioPacketType` (vs the video parser which surfaces the
    ///   inner type), so faithful inversion requires a model addition
    ///   (e.g. `inner_packet_type: Option<ExAudioPacketType>`). Deferred
    ///   to keep this round on the wire-byte writer only.
    ///
    /// Returns `Err(Error::InvalidData)` on combinations the spec leaves
    /// no wire representation for.
    pub fn to_bytes(&self, out: &mut Vec<u8>) -> Result<()> {
        if !self.mod_ex_entries.is_empty() {
            return Err(Error::invalid(
                "ExAudioTagHeader::to_bytes: ModEx emission not yet implemented",
            ));
        }
        if self.timestamp_offset_nano != 0 {
            return Err(Error::invalid(
                "ExAudioTagHeader::to_bytes: nonzero timestamp_offset_nano without mod_ex_entries",
            ));
        }
        if self.multitrack.is_some() {
            return Err(Error::invalid(
                "ExAudioTagHeader::to_bytes: multitrack emission not yet implemented (inner AudioPacketType is not surfaced by the parser)",
            ));
        }
        // SoundFormat=9 (ExHeader) high nibble + AudioPacketType low nibble.
        out.push((SOUND_FORMAT_EX_HEADER << 4) | (self.packet_type.to_u8() & 0x0F));
        let fcc = self.fourcc.ok_or_else(|| {
            Error::invalid("ExAudioTagHeader::to_bytes: single-track header needs a FourCc")
        })?;
        out.extend_from_slice(&fcc.to_be_bytes());
        Ok(())
    }
}

/// Stable codec-id string for an ExAudio FourCc. Matches strings the
/// registry uses elsewhere (`"aac"`, `"mp3"`, `"opus"`, `"flac"`,
/// `"ac3"`, `"eac3"`). Unknown FourCcs surface as
/// `flv:exaudio:<fourcc-ascii>` so the caller can log the anomaly.
pub fn fourcc_audio_codec_id_str(fourcc: u32) -> String {
    match fourcc {
        FOURCC_AC3 => "ac3".into(),
        FOURCC_EAC3 => "eac3".into(),
        FOURCC_OPUS => "opus".into(),
        FOURCC_MP3 => "mp3".into(),
        FOURCC_FLAC => "flac".into(),
        FOURCC_AAC => "aac".into(),
        other => {
            let bytes = other.to_be_bytes();
            if bytes.iter().all(|b| (0x20..=0x7E).contains(b)) {
                format!(
                    "flv:exaudio:{}",
                    std::str::from_utf8(&bytes).unwrap_or("????")
                )
            } else {
                format!("flv:exaudio:0x{other:08X}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_legacy_audio_tag_header() {
        // 0xAF = legacy AAC (SoundFormat=10) + 44 kHz + 16-bit + stereo.
        let body = [0xAF, 0x01, 0xDE, 0xAD];
        assert_eq!(ExAudioTagHeader::parse(&body).unwrap(), None);
    }

    #[test]
    fn rejects_empty_body() {
        assert_eq!(ExAudioTagHeader::parse(&[]).unwrap(), None);
    }

    #[test]
    fn opus_sequence_start() {
        // SoundFormat=9 (ExHeader) + AudioPacketType=0 (SequenceStart)
        // → 0x90. FourCc = "Opus".
        let mut body = vec![0x90];
        body.extend_from_slice(b"Opus");
        body.extend_from_slice(&[0x4F, 0x70]); // dummy OpusHead bytes
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExAudioPacketType::SequenceStart);
        assert_eq!(h.fourcc, Some(FOURCC_OPUS));
        assert_eq!(h.multitrack, None);
        assert_eq!(h.timestamp_offset_nano, 0);
        assert_eq!(h.bytes_consumed, 5);
        assert_eq!(fourcc_audio_codec_id_str(h.fourcc.unwrap()), "opus");
        assert_eq!(&body[h.bytes_consumed..], &[0x4F, 0x70]);
    }

    #[test]
    fn flac_sequence_start_id_string_matches() {
        let mut body = vec![0x90];
        body.extend_from_slice(b"fLaC");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.fourcc, Some(FOURCC_FLAC));
        assert_eq!(fourcc_audio_codec_id_str(h.fourcc.unwrap()), "flac");
    }

    #[test]
    fn ac3_coded_frames() {
        // SoundFormat=9 + AudioPacketType=1 (CodedFrames) → 0x91.
        let mut body = vec![0x91];
        body.extend_from_slice(b"ac-3");
        body.extend_from_slice(&[0x0B, 0x77]); // ATSC AC-3 sync word
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExAudioPacketType::CodedFrames);
        assert_eq!(h.fourcc, Some(FOURCC_AC3));
        assert_eq!(fourcc_audio_codec_id_str(h.fourcc.unwrap()), "ac3");
        assert_eq!(&body[h.bytes_consumed..], &[0x0B, 0x77]);
    }

    #[test]
    fn eac3_recognised() {
        let mut body = vec![0x91];
        body.extend_from_slice(b"ec-3");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(fourcc_audio_codec_id_str(h.fourcc.unwrap()), "eac3");
    }

    #[test]
    fn mp3_fourcc_decoded() {
        let mut body = vec![0x91];
        body.extend_from_slice(b".mp3");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.fourcc, Some(FOURCC_MP3));
        assert_eq!(fourcc_audio_codec_id_str(h.fourcc.unwrap()), "mp3");
    }

    #[test]
    fn aac_fourcc_decoded() {
        let mut body = vec![0x90];
        body.extend_from_slice(b"mp4a");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.fourcc, Some(FOURCC_AAC));
        assert_eq!(fourcc_audio_codec_id_str(h.fourcc.unwrap()), "aac");
    }

    #[test]
    fn sequence_end_recognised() {
        // SoundFormat=9 + AudioPacketType=2 (SequenceEnd) → 0x92.
        let mut body = vec![0x92];
        body.extend_from_slice(b"Opus");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExAudioPacketType::SequenceEnd);
        assert_eq!(h.fourcc, Some(FOURCC_OPUS));
        assert_eq!(h.bytes_consumed, 5);
    }

    #[test]
    fn multichannel_config_recognised() {
        // 0x94 = ExHeader + MultichannelConfig.
        let mut body = vec![0x94];
        body.extend_from_slice(b"Opus");
        body.push(0x01); // dummy AudioChannelOrder
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExAudioPacketType::MultichannelConfig);
        assert_eq!(h.fourcc, Some(FOURCC_OPUS));
        assert_eq!(&body[h.bytes_consumed..], &[0x01]);
    }

    #[test]
    fn multitrack_one_track_with_fourcc() {
        // 0x95 = ExHeader + Multitrack. Inner byte 0x01 = OneTrack +
        // inner AudioPacketType=1 (CodedFrames).
        let mut body = vec![0x95];
        body.push(0x01);
        body.extend_from_slice(b"Opus");
        body.extend_from_slice(&[0xAA, 0xBB]);
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExAudioPacketType::Multitrack);
        assert_eq!(h.multitrack, Some(AvMultitrackType::OneTrack));
        assert_eq!(h.fourcc, Some(FOURCC_OPUS));
        assert_eq!(&body[h.bytes_consumed..], &[0xAA, 0xBB]);
    }

    #[test]
    fn multitrack_many_tracks_many_codecs_omits_fourcc() {
        // 0x95 + (0x21 = ManyTracksManyCodecs + CodedFrames).
        let mut body = vec![0x95];
        body.push(0x21);
        body.extend_from_slice(&[0xCA, 0xFE]); // per-track stuff (caller parses)
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.multitrack, Some(AvMultitrackType::ManyTracksManyCodecs));
        assert_eq!(h.fourcc, None);
        // No FourCc consumed: header is just lead (1) + mt (1) = 2.
        assert_eq!(h.bytes_consumed, 2);
    }

    #[test]
    fn modex_timestamp_offset_nano_accumulates() {
        // 0x97 = ExHeader + ModEx. Inside ModEx:
        //   UI8 modExSize-1 = 0x02 → 3-byte payload
        //   3-byte UI24 = 0x000_3E8 = 1000 ns
        //   trailer byte = (modExType=0 << 4) | inner packetType=1
        //     = 0x00 | 0x01 = 0x01
        // FourCc then = "Opus", and inner packet type = CodedFrames.
        let mut body = vec![0x97];
        body.push(0x02); // modExSize-1: payload is 3 bytes
        body.extend_from_slice(&[0x00, 0x03, 0xE8]); // 1000 ns
        body.push(0x01); // trailer: (TimestampOffsetNano << 4) | CodedFrames
        body.extend_from_slice(b"Opus");
        body.extend_from_slice(&[0xDE, 0xAD]);
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExAudioPacketType::CodedFrames);
        assert_eq!(h.fourcc, Some(FOURCC_OPUS));
        assert_eq!(h.timestamp_offset_nano, 1000);
        assert_eq!(&body[h.bytes_consumed..], &[0xDE, 0xAD]);
    }

    #[test]
    fn modex_chains_two_offsets() {
        // Two ModEx packets in a row, then CodedFrames.
        // Lead 0x97 (ExHeader + ModEx).
        // ModEx #1: size-1=2, 3-byte 100 ns, trailer 0x07 (TSNano + ModEx)
        // ModEx #2: size-1=2, 3-byte 200 ns, trailer 0x01 (TSNano + CodedFrames)
        let mut body = vec![0x97];
        body.push(0x02);
        body.extend_from_slice(&[0x00, 0x00, 0x64]); // 100
        body.push(0x07);
        body.push(0x02);
        body.extend_from_slice(&[0x00, 0x00, 0xC8]); // 200
        body.push(0x01);
        body.extend_from_slice(b"Opus");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.timestamp_offset_nano, 300);
        assert_eq!(h.packet_type, ExAudioPacketType::CodedFrames);
        assert_eq!(h.fourcc, Some(FOURCC_OPUS));
    }

    #[test]
    fn modex_entries_field_exposes_typed_payloads() {
        // Two ModEx entries: first a TimestampOffsetNano (subtype 0,
        // 500 ns), second a reserved subtype 0x3 with a 2-byte opaque
        // payload, then inner packet type=CodedFrames=1.
        let mut body = vec![0x97]; // ExHeader + ModEx
        body.push(0x02); // size-1=2 → 3-byte payload
        body.extend_from_slice(&[0x00, 0x01, 0xF4]); // 500 ns
        body.push(0x07); // trailer: (TSNano << 4) | ModEx (chain)
        body.push(0x01); // size-1=1 → 2-byte payload
        body.extend_from_slice(&[0xAA, 0xBB]);
        body.push(0x31); // trailer: (reserved 0x3 << 4) | CodedFrames
        body.extend_from_slice(b"Opus");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExAudioPacketType::CodedFrames);
        assert_eq!(h.timestamp_offset_nano, 500);
        assert_eq!(h.mod_ex_entries.len(), 2);

        let first = &h.mod_ex_entries[0];
        assert_eq!(first.subtype_raw, 0);
        assert_eq!(first.timestamp_offset_nano(), Some(500));
        assert_eq!(
            first.audio_subtype(),
            AudioPacketModExType::TimestampOffsetNano
        );
        assert_eq!(first.raw, vec![0x00, 0x01, 0xF4]);

        let second = &h.mod_ex_entries[1];
        assert_eq!(second.subtype_raw, 3);
        assert_eq!(second.audio_subtype(), AudioPacketModExType::Reserved(3));
        assert_eq!(second.timestamp_offset_nano(), None);
        assert_eq!(second.raw, vec![0xAA, 0xBB]);
    }

    #[test]
    fn modex_size_escape_handles_256_byte_payload() {
        // size-1=0xFF → triggers escape; UI16+1 then carries true size.
        // We pick a 257-byte ModEx payload so the escape path runs.
        let mut body = vec![0x97];
        body.push(0xFF); // forces escape
        body.extend_from_slice(&[0x01, 0x00]); // UI16 BE = 256 → +1 = 257
        body.extend(std::iter::repeat(0xAB).take(257));
        // trailer: any modExType (here 0xF reserved) + CodedFrames
        body.push(0xF1);
        body.extend_from_slice(b"Opus");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExAudioPacketType::CodedFrames);
        assert_eq!(h.fourcc, Some(FOURCC_OPUS));
        // Unrecognised modExType: no timestamp accumulation.
        assert_eq!(h.timestamp_offset_nano, 0);
    }

    #[test]
    fn truncated_fourcc_errors() {
        let body = [0x90, b'O', b'p']; // only 2 of 4 FourCc bytes
        assert!(matches!(
            ExAudioTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_modex_errors() {
        // ModEx but no size byte.
        let body = [0x97];
        assert!(matches!(
            ExAudioTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_multitrack_errors() {
        // Multitrack but no following AvMultitrackType byte.
        let body = [0x95];
        assert!(matches!(
            ExAudioTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn nested_multitrack_rejected() {
        // 0x95 = ExHeader + Multitrack. Inner byte 0x05 = OneTrack +
        // inner AudioPacketType=5 (Multitrack again — illegal).
        let body = [0x95, 0x05];
        assert!(matches!(
            ExAudioTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn unknown_fourcc_falls_through() {
        let mut body = vec![0x90];
        body.extend_from_slice(b"zzzz");
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.fourcc, Some(u32::from_be_bytes(*b"zzzz")));
        assert_eq!(
            fourcc_audio_codec_id_str(h.fourcc.unwrap()),
            "flv:exaudio:zzzz"
        );
    }

    #[test]
    fn unknown_fourcc_non_ascii_falls_through_as_hex() {
        let mut body = vec![0x90];
        body.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        let h = ExAudioTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(
            fourcc_audio_codec_id_str(h.fourcc.unwrap()),
            "flv:exaudio:0x01020304"
        );
    }

    // ---- to_bytes: parse-emit-parse round-trips ---------------------------

    fn assert_round_trips(body: &[u8]) -> Vec<u8> {
        let h = ExAudioTagHeader::parse(body).unwrap().unwrap();
        let payload_tail = &body[h.bytes_consumed..];
        let mut out = Vec::new();
        h.to_bytes(&mut out).unwrap();
        out.extend_from_slice(payload_tail);
        let h2 = ExAudioTagHeader::parse(&out).unwrap().unwrap();
        assert_eq!(h.packet_type, h2.packet_type);
        assert_eq!(h.fourcc, h2.fourcc);
        assert_eq!(h.multitrack, h2.multitrack);
        assert_eq!(h.bytes_consumed, h2.bytes_consumed);
        assert_eq!(&out[h2.bytes_consumed..], payload_tail);
        out
    }

    #[test]
    fn to_bytes_opus_sequence_start_round_trip() {
        let mut body = vec![0x90];
        body.extend_from_slice(b"Opus");
        body.extend_from_slice(&[0x4F, 0x70, 0x75, 0x73]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_flac_round_trip() {
        let mut body = vec![0x91];
        body.extend_from_slice(b"fLaC");
        body.extend_from_slice(&[0xFF, 0xF8]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_ac3_coded_frames_round_trip() {
        let mut body = vec![0x91];
        body.extend_from_slice(b"ac-3");
        body.extend_from_slice(&[0x0B, 0x77, 0x00]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_mp3_round_trip() {
        let mut body = vec![0x91];
        body.extend_from_slice(b".mp3");
        body.extend_from_slice(&[0xFF, 0xFB, 0x90]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_aac_mp4a_sequence_start_round_trip() {
        let mut body = vec![0x90];
        body.extend_from_slice(b"mp4a");
        body.extend_from_slice(&[0x12, 0x10]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_sequence_end_round_trip() {
        let mut body = vec![0x92];
        body.extend_from_slice(b"Opus");
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_multichannel_config_round_trip() {
        let mut body = vec![0x94];
        body.extend_from_slice(b"Opus");
        body.push(0x01);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_rejects_multitrack_emission() {
        // Multitrack emission is deferred (the parser doesn't surface
        // the inner AudioPacketType, so faithful inversion needs a
        // model field). Until then the writer must refuse rather than
        // silently produce wrong bytes.
        let h = ExAudioTagHeader {
            packet_type: ExAudioPacketType::Multitrack,
            fourcc: Some(FOURCC_OPUS),
            multitrack: Some(AvMultitrackType::OneTrack),
            timestamp_offset_nano: 0,
            mod_ex_entries: Vec::new(),
            bytes_consumed: 0,
        };
        let mut out = Vec::new();
        assert!(matches!(h.to_bytes(&mut out), Err(Error::InvalidData(_))));
    }

    #[test]
    fn to_bytes_rejects_modex_emission() {
        let h = ExAudioTagHeader {
            packet_type: ExAudioPacketType::CodedFrames,
            fourcc: Some(FOURCC_OPUS),
            multitrack: None,
            timestamp_offset_nano: 0,
            mod_ex_entries: vec![ModExEntry {
                subtype_raw: 0,
                payload: crate::mod_ex::ModExPayload::TimestampOffsetNano { offset_ns: 100 },
                raw: vec![0, 0, 0x64],
            }],
            bytes_consumed: 0,
        };
        let mut out = Vec::new();
        assert!(matches!(h.to_bytes(&mut out), Err(Error::InvalidData(_))));
    }

    #[test]
    fn to_bytes_rejects_missing_fourcc_single_track() {
        let h = ExAudioTagHeader {
            packet_type: ExAudioPacketType::CodedFrames,
            fourcc: None,
            multitrack: None,
            timestamp_offset_nano: 0,
            mod_ex_entries: Vec::new(),
            bytes_consumed: 0,
        };
        let mut out = Vec::new();
        assert!(matches!(h.to_bytes(&mut out), Err(Error::InvalidData(_))));
    }

    #[test]
    fn audio_packet_type_round_trips_through_to_u8() {
        for v in 0u8..16 {
            assert_eq!(ExAudioPacketType::from_u8(v).to_u8(), v);
        }
    }
}
