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
//! * `PacketType=6 Multitrack` (v2) — the body is a loop of per-track
//!   records (`crate::multitrack::split_tracks`); the inner per-track
//!   packet type is re-read off the multitrack outer header.
//! * `PacketType=7 ModEx` (v2 extension; parsed but TimestampOffsetNano
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

use crate::mod_ex::{emit as mod_ex_emit, walk as mod_ex_walk, ModExEntry};
use crate::multitrack::AvMultitrackType;

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

    /// 3-bit wire value (bits 6..4 of the ExVideo leading byte, spec
    /// enhanced-rtmp-v2 §`Extended VideoTagHeader`). Inverse of
    /// [`Self::from_u8`]. `Reserved(n)` is masked to 3 bits.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::KeyFrame => 1,
            Self::InterFrame => 2,
            Self::DisposableInterFrame => 3,
            Self::GeneratedKeyFrame => 4,
            Self::Command => 5,
            Self::Reserved(n) => n & 0x07,
        }
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

    /// 4-bit wire value (bits 3..0 of the ExVideo leading byte, spec
    /// enhanced-rtmp-v2 §`Extended VideoTagHeader`). Inverse of
    /// [`Self::from_u8`]. `Reserved(n)` is masked to 4 bits.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::SequenceStart => 0,
            Self::CodedFrames => 1,
            Self::SequenceEnd => 2,
            Self::CodedFramesX => 3,
            Self::Metadata => 4,
            Self::Mpeg2TsSequenceStart => 5,
            Self::Multitrack => 6,
            Self::ModEx => 7,
            Self::Reserved(n) => n & 0x0F,
        }
    }
}

/// `VideoCommand` UI8 read off the body when
/// `videoFrameType == VideoFrameType.Command` and
/// `videoPacketType != VideoPacketType.Metadata` (Veovera
/// enhanced-rtmp-v2 §`Extended VideoTagHeader`).
///
/// The spec assigns `0 = StartSeek` (start of client-side seeking video
/// sequence) and `1 = EndSeek` (end of the same), reserving `2..=0xFF`
/// for future use. Unknown values are preserved verbatim so a future
/// command extension lands without parser changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCommand {
    /// `0` — start of client-side seeking video frame sequence.
    StartSeek,
    /// `1` — end of client-side seeking video frame sequence.
    EndSeek,
    /// Any other UI8 — spec-reserved but preserved opaquely so callers
    /// can log future extensions.
    Reserved(u8),
}

impl VideoCommand {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::StartSeek,
            1 => Self::EndSeek,
            other => Self::Reserved(other),
        }
    }

    /// The wire-level UI8 this command was decoded from. Round-trips
    /// the [`Self::Reserved`] payload byte; for `StartSeek` / `EndSeek`
    /// the canonical 0 / 1 are returned.
    pub fn as_u8(self) -> u8 {
        match self {
            Self::StartSeek => 0,
            Self::EndSeek => 1,
            Self::Reserved(v) => v,
        }
    }
}

/// `VideoPacketModExType` (UB[4] following the ModEx blob).
///
/// Re-exported from [`crate::mod_ex`] so the video-side public API
/// keeps a name local to ex_video while sharing the actual enum with
/// the shared ModEx walker.
pub use crate::mod_ex::VideoPacketModExType;

/// Result of parsing an Enhanced RTMP ExVideoTagHeader off the start of
/// a filter-clear video tag body.
///
/// Layout when the leading-byte IsExHeader flag is set:
///
/// ```text
///   0   1   IsExHeader|FrameType|PacketType   UI8
///   ┌── loop while PacketType == ModEx (= 7):
///   │  1   1   modExDataSize = UI8 + 1                 (UI8)
///   │  ?   ?   if modExDataSize == 256, UI16 + 1       (escape)
///   │  ?   N   modExData[modExDataSize]                (opaque)
///   │  ?   1   VideoPacketModExType(UB[4]) | next PacketType(UB[4])
///   └──
///   ?   4   VideoFourCc                       UI32 BE
///   ?   ?   PacketType-specific payload (e.g. SI24 composition-time
///           offset for HEVC CodedFrames, AV1CodecConfigurationRecord
///           for SequenceStart on FourCC=av01, …)
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExVideoTagHeader {
    pub frame_type: ExFrameType,
    pub packet_type: ExPacketType,
    /// FourCc identifying the codec. `None` only when the spec's
    /// multitrack `ManyTracksManyCodecs` mode is in effect; in that case
    /// the per-track FourCc is carried inside the body and must be parsed
    /// via [`crate::multitrack::split_tracks`].
    pub fourcc: Option<u32>,
    /// Multitrack outer descriptor when `videoPacketType == Multitrack`
    /// (`6`), `None` for the common single-track case. The per-track body
    /// loop is walked with [`crate::multitrack::split_tracks`] given this
    /// type and (for `OneTrack` / `ManyTracks`) the shared [`Self::fourcc`].
    pub multitrack: Option<AvMultitrackType>,
    /// Number of bytes consumed at the front of the tag body. Callers
    /// slice `body[bytes_consumed..]` to recover the payload.
    pub bytes_consumed: usize,
    /// Decoded SI24 composition-time offset (ms) — present only for
    /// HEVC / VVC / AVC `CodedFrames`. `None` otherwise (the spec
    /// implies a zero offset).
    pub composition_time_offset_ms: Option<i32>,
    /// Sum of TimestampOffsetNano ModEx offsets read from the header
    /// stack, in nanoseconds (0..999_999 per ModEx). The spec defines
    /// only this ModEx subtype today (`VideoPacketModExType` = 0).
    pub timestamp_offset_nano: u32,
    /// All ModEx entries parsed off the front of the body, in wire
    /// order. Each entry carries the typed subtype + payload (see
    /// [`crate::mod_ex::ModExEntry`]) so reserved subtypes survive the
    /// header walk with their raw bytes attached.
    pub mod_ex_entries: Vec<ModExEntry>,
    /// Decoded `VideoCommand` (UI8) read off the body when
    /// `frame_type == Command && packet_type != Metadata`. Per
    /// enhanced-rtmp-v2 §`Extended VideoTagHeader` the body carries
    /// exactly one UI8 in that case and no further codec payload —
    /// [`bytes_consumed`] is advanced past it so callers see an empty
    /// `body[bytes_consumed..]`. `None` for every non-command tag.
    pub video_command: Option<VideoCommand>,
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

        // Walk all ModEx (VideoPacketType = 7) entries off the front of
        // the body. Shared with the audio path via `crate::mod_ex`.
        // Returns the cursor past the ModEx run, the post-ModEx
        // VideoPacketType byte, every parsed entry (typed subtype +
        // raw payload), and the total `TimestampOffsetNano` sum (ns).
        let (cursor, packet_type_raw, mod_ex_entries, timestamp_offset_nano) =
            mod_ex_walk::<7>(body, 1, packet_type_raw)?;
        let packet_type = ExPacketType::from_u8(packet_type_raw);

        // Multitrack outer header (enhanced-rtmp-v2 §`ExVideoTagHeader`,
        // `videoPacketType == VideoPacketType.Multitrack` branch). When
        // present, the next byte packs `videoMultitrackType (UB[4]) |
        // inner videoPacketType (UB[4])`; the inner type must NOT itself
        // be Multitrack. For OneTrack / ManyTracks a single shared
        // `videoFourCc` follows; ManyTracksManyCodecs carries the FourCc
        // per-track inside the body (recovered via
        // `crate::multitrack::split_tracks`), so the shared FourCc is
        // absent here.
        let (multitrack, packet_type, fourcc, mut cursor) =
            if matches!(packet_type, ExPacketType::Multitrack) {
                if cursor >= body.len() {
                    return Err(Error::invalid(
                        "FLV Ex video tag: truncated multitrack header byte",
                    ));
                }
                let mt_byte = body[cursor];
                let mut c = cursor + 1;
                let mt_type = AvMultitrackType::from_u8((mt_byte >> 4) & 0x0F);
                let inner_pt = ExPacketType::from_u8(mt_byte & 0x0F);
                if matches!(inner_pt, ExPacketType::Multitrack) {
                    return Err(Error::invalid(
                        "FLV Ex video tag: nested Multitrack packet type",
                    ));
                }
                let fourcc = if matches!(mt_type, AvMultitrackType::ManyTracksManyCodecs) {
                    None
                } else {
                    if c + 4 > body.len() {
                        return Err(Error::invalid(
                            "FLV Ex video tag: truncated multitrack FourCc",
                        ));
                    }
                    let fcc = u32::from_be_bytes([body[c], body[c + 1], body[c + 2], body[c + 3]]);
                    c += 4;
                    Some(fcc)
                };
                (Some(mt_type), inner_pt, fourcc, c)
            } else {
                // Single-track: FourCc — 4 bytes immediately after the
                // leading byte (or immediately after the last ModEx
                // trailer byte when a ModEx run was consumed).
                if cursor + 4 > body.len() {
                    return Err(Error::invalid("FLV Ex video tag: truncated FourCc"));
                }
                let fcc = u32::from_be_bytes([
                    body[cursor],
                    body[cursor + 1],
                    body[cursor + 2],
                    body[cursor + 3],
                ]);
                (None, packet_type, Some(fcc), cursor + 4)
            };
        let mut composition_time_offset_ms = None;

        // SI24 CompositionTimeOffset is present only for HEVC / VVC /
        // AVC `CodedFrames` (PacketType=1) in the *single-track* case.
        // PacketType=3 (CodedFramesX) explicitly drops it; PacketType=0
        // (SequenceStart) and PacketType=2 (SequenceEnd) don't carry one
        // either. In multitrack mode the per-track CTO lives inside each
        // track payload (after `split_tracks` slicing), so it is not read
        // here.
        if multitrack.is_none()
            && matches!(packet_type, ExPacketType::CodedFrames)
            && matches!(fourcc, Some(FOURCC_HVC1 | FOURCC_VVC1 | FOURCC_AVC1))
        {
            if body.len() < cursor + 3 {
                return Err(Error::invalid(
                    "FLV Ex video tag: truncated CompositionTimeOffset",
                ));
            }
            let raw = ((body[cursor] as u32) << 16)
                | ((body[cursor + 1] as u32) << 8)
                | (body[cursor + 2] as u32);
            // Sign-extend 24 bits.
            let sext = if raw & 0x0080_0000 != 0 {
                raw | 0xFF00_0000
            } else {
                raw
            };
            composition_time_offset_ms = Some(sext as i32);
            cursor += 3;
        }

        // VideoCommand UI8 — present when frame_type == Command and the
        // packet_type isn't Metadata (enhanced-rtmp-v2 §`Extended
        // VideoTagHeader`, lines "if (videoPacketType !=
        // VideoPacketType.Metadata && videoFrameType ==
        // VideoFrameType.Command) videoCommand = UI8 as VideoCommand").
        // The spec then sets `processVideoBody = false` so no further
        // payload bytes follow — bytes_consumed is advanced past the
        // command byte so callers see an empty `body[bytes_consumed..]`.
        let video_command = if matches!(frame_type, ExFrameType::Command)
            && !matches!(packet_type, ExPacketType::Metadata)
        {
            if body.len() < cursor + 1 {
                return Err(Error::invalid("FLV Ex video tag: truncated VideoCommand"));
            }
            let cmd = VideoCommand::from_u8(body[cursor]);
            cursor += 1;
            Some(cmd)
        } else {
            None
        };

        Ok(Some(Self {
            frame_type,
            packet_type,
            fourcc,
            multitrack,
            bytes_consumed: cursor,
            composition_time_offset_ms,
            timestamp_offset_nano,
            mod_ex_entries,
            video_command,
        }))
    }

    /// Serialise this header to the wire bytes that opens an
    /// `ExVideoTagBody` — the inverse of [`Self::parse`].
    ///
    /// The output is appended to `out`. After return the caller appends
    /// the codec-specific payload (e.g. AV1CodecConfigurationRecord for
    /// SequenceStart, NALU access-unit for HEVC CodedFrames after the
    /// CTO bytes are emitted here, etc.).
    ///
    /// Coverage and limitations for this slice:
    ///
    /// * Single-track headers are fully supported.
    /// * Multitrack `OneTrack` / `ManyTracks` are supported (the shared
    ///   FourCc is emitted once). `ManyTracksManyCodecs` is supported,
    ///   the shared FourCc is omitted per spec.
    /// * SI24 `CompositionTimeOffset` is emitted for HEVC / VVC / AVC
    ///   `CodedFrames` (single-track only) when
    ///   `composition_time_offset_ms` is `Some(_)`; for any other
    ///   FourCc / packet-type combination a non-`None` CTO is rejected
    ///   with [`Error::InvalidData`] since the spec leaves no slot for it.
    /// * The trailing `VideoCommand` UI8 is emitted when
    ///   `frame_type == Command` and `packet_type != Metadata`, mirroring
    ///   the parser's spec-aligned guard. `video_command = None` on that
    ///   combination is rejected (the spec requires the byte).
    /// * ModEx prefix emission **is** supported via the shared
    ///   [`crate::mod_ex::emit`] writer. When `mod_ex_entries` is
    ///   non-empty the lead byte's low nibble is `7` (ModEx) and the
    ///   entries are chained off the front of the body, each
    ///   contributing its size prefix + raw payload + trailer byte;
    ///   the final trailer's low nibble carries the resolved
    ///   `packet_type` (or `Multitrack` when multitrack mode is in
    ///   effect) so the parser's `walk` exits with the correct
    ///   AudioPacketType / VideoPacketType. Per-entry contracts (size
    ///   in `1..=65_536`, `TimestampOffsetNano` payload `>= 3` bytes,
    ///   raw UI24 matches the typed `offset_ns`, `offset_ns
    ///   <= 999_999`) match the parser's invariants. A
    ///   `timestamp_offset_nano != 0` with no `mod_ex_entries` is
    ///   rejected as internally inconsistent.
    ///
    /// Returns `Err(Error::InvalidData)` for any combination that doesn't
    /// have a defined wire representation.
    pub fn to_bytes(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.mod_ex_entries.is_empty() && self.timestamp_offset_nano != 0 {
            return Err(Error::invalid(
                "ExVideoTagHeader::to_bytes: nonzero timestamp_offset_nano without mod_ex_entries",
            ));
        }
        let frame_bits = self.frame_type.to_u8() & 0x07;
        // Single-track: leading-byte PacketType is the final packet type.
        // Multitrack: leading-byte PacketType is `Multitrack`; the inner
        // type goes into the multitrack header byte that follows the
        // (shared) FourCc-or-not.
        let (resolved_pt, inner_pt_for_mt) = if self.multitrack.is_some() {
            (ExPacketType::Multitrack.to_u8(), Some(self.packet_type))
        } else {
            (self.packet_type.to_u8() & 0x0F, None)
        };
        // When ModEx entries are present the lead byte advertises the
        // ModEx sentinel `7`; the resolved packet type rides on the last
        // ModEx trailer instead. Match the parser's `walk`-then-resolve
        // layout exactly.
        let lead_pt = if self.mod_ex_entries.is_empty() {
            resolved_pt
        } else {
            7
        };
        out.push(EX_HEADER_FLAG | (frame_bits << 4) | lead_pt);

        if !self.mod_ex_entries.is_empty() {
            mod_ex_emit::<7>(out, &self.mod_ex_entries, resolved_pt)?;
        }

        if let Some(mt_type) = self.multitrack {
            // Spec layout: UB[4] AvMultitrackType | UB[4] inner PacketType.
            let inner_pt = inner_pt_for_mt.expect("set when multitrack is Some");
            if matches!(inner_pt, ExPacketType::Multitrack) {
                return Err(Error::invalid(
                    "ExVideoTagHeader::to_bytes: nested Multitrack packet type",
                ));
            }
            let mt_byte = (mt_type.to_u8() << 4) | (inner_pt.to_u8() & 0x0F);
            out.push(mt_byte);
            // FourCc is shared except in ManyTracksManyCodecs.
            if matches!(mt_type, AvMultitrackType::ManyTracksManyCodecs) {
                if self.fourcc.is_some() {
                    return Err(Error::invalid(
                        "ExVideoTagHeader::to_bytes: ManyTracksManyCodecs must not carry a shared FourCc",
                    ));
                }
            } else {
                let fcc = self.fourcc.ok_or_else(|| {
                    Error::invalid(
                        "ExVideoTagHeader::to_bytes: multitrack OneTrack/ManyTracks needs a shared FourCc",
                    )
                })?;
                out.extend_from_slice(&fcc.to_be_bytes());
            }
            // No single-track CTO read on multitrack mode; the per-track
            // CTO is part of each track's body payload.
            if self.composition_time_offset_ms.is_some() {
                return Err(Error::invalid(
                    "ExVideoTagHeader::to_bytes: composition_time_offset_ms must be None in multitrack mode (per-track CTO lives in the payload)",
                ));
            }
        } else {
            // Single-track: FourCc immediately after the lead byte.
            let fcc = self.fourcc.ok_or_else(|| {
                Error::invalid("ExVideoTagHeader::to_bytes: single-track header needs a FourCc")
            })?;
            out.extend_from_slice(&fcc.to_be_bytes());

            // SI24 CompositionTimeOffset — emitted only for HEVC / VVC /
            // AVC `CodedFrames` (single-track), mirroring the parser.
            let cto_slot = matches!(self.packet_type, ExPacketType::CodedFrames)
                && matches!(fcc, FOURCC_HVC1 | FOURCC_VVC1 | FOURCC_AVC1);
            match (cto_slot, self.composition_time_offset_ms) {
                (true, Some(cto)) => {
                    if !(-(1 << 23)..(1 << 23)).contains(&cto) {
                        return Err(Error::invalid(format!(
                            "ExVideoTagHeader::to_bytes: CompositionTimeOffset {cto} ms outside SI24 range"
                        )));
                    }
                    let bits = (cto as u32) & 0x00FF_FFFF;
                    out.push((bits >> 16) as u8);
                    out.push((bits >> 8) as u8);
                    out.push(bits as u8);
                }
                (true, None) => {
                    return Err(Error::invalid(
                        "ExVideoTagHeader::to_bytes: HEVC/VVC/AVC CodedFrames requires composition_time_offset_ms (use 0 for no reorder)",
                    ));
                }
                (false, Some(_)) => {
                    return Err(Error::invalid(
                        "ExVideoTagHeader::to_bytes: composition_time_offset_ms is only defined for HEVC/VVC/AVC CodedFrames",
                    ));
                }
                (false, None) => {}
            }
        }

        // VideoCommand UI8: written iff frame_type == Command && packet_type != Metadata.
        // Mirrors the parser guard.
        let needs_command = matches!(self.frame_type, ExFrameType::Command)
            && !matches!(self.packet_type, ExPacketType::Metadata);
        match (needs_command, self.video_command) {
            (true, Some(cmd)) => out.push(cmd.as_u8()),
            (true, None) => {
                return Err(Error::invalid(
                    "ExVideoTagHeader::to_bytes: frame_type=Command with non-Metadata packet_type requires video_command",
                ));
            }
            (false, Some(_)) => {
                return Err(Error::invalid(
                    "ExVideoTagHeader::to_bytes: video_command set without frame_type=Command (or with Metadata packet_type)",
                ));
            }
            (false, None) => {}
        }

        Ok(())
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
        assert_eq!(h.fourcc, Some(FOURCC_AV01));
        assert_eq!(h.bytes_consumed, 5);
        assert_eq!(h.composition_time_offset_ms, None);
        assert_eq!(fourcc_codec_id_str(h.fourcc.unwrap()), "av1");
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
        assert_eq!(h.fourcc, Some(FOURCC_HVC1));
        assert_eq!(h.bytes_consumed, 8);
        assert_eq!(h.composition_time_offset_ms, Some(33));
        assert_eq!(fourcc_codec_id_str(h.fourcc.unwrap()), "h265");
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
        assert_eq!(h.fourcc, Some(FOURCC_AV01));
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
    fn multitrack_one_track_with_fourcc() {
        // 0x96 = IsExHeader | FrameType=1 (key) | PacketType=6 (Multitrack).
        // Inner byte 0x01 = OneTrack (0) << 4 | inner CodedFrames (1).
        let mut body = vec![0x96];
        body.push(0x01);
        body.extend_from_slice(b"av01");
        body.extend_from_slice(&[0x00, 0xDE, 0xAD]); // trackId + payload (caller splits)
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::KeyFrame);
        assert_eq!(h.multitrack, Some(AvMultitrackType::OneTrack));
        // Inner packet type is what the header now reports.
        assert_eq!(h.packet_type, ExPacketType::CodedFrames);
        assert_eq!(h.fourcc, Some(FOURCC_AV01));
        // No single-track CTO read in multitrack mode (it lives per-track).
        assert_eq!(h.composition_time_offset_ms, None);
        assert_eq!(h.bytes_consumed, 6); // lead + mt-byte + 4 FourCc
        assert_eq!(&body[h.bytes_consumed..], &[0x00, 0xDE, 0xAD]);
    }

    #[test]
    fn multitrack_many_tracks_many_codecs_omits_fourcc() {
        // 0x96 + (0x21 = ManyTracksManyCodecs (2) << 4 | CodedFrames (1)).
        let mut body = vec![0x96];
        body.push(0x21);
        body.extend_from_slice(&[0xCA, 0xFE]); // per-track structure (caller splits)
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.multitrack, Some(AvMultitrackType::ManyTracksManyCodecs));
        assert_eq!(h.packet_type, ExPacketType::CodedFrames);
        assert_eq!(h.fourcc, None);
        // No shared FourCc consumed: lead (1) + mt-byte (1) = 2.
        assert_eq!(h.bytes_consumed, 2);
        assert_eq!(&body[h.bytes_consumed..], &[0xCA, 0xFE]);
    }

    #[test]
    fn multitrack_does_not_read_single_track_cto() {
        // hvc1 ManyTracks CodedFrames: the SI24 CTO lives inside each
        // track payload, so the header parser must NOT consume one here.
        let mut body = vec![0x96]; // key + Multitrack
        body.push(0x11); // ManyTracks (1) << 4 | CodedFrames (1)
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x21]); // trackId + (CTO bytes belong to track payload)
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.multitrack, Some(AvMultitrackType::ManyTracks));
        assert_eq!(h.composition_time_offset_ms, None);
        assert_eq!(h.bytes_consumed, 6); // lead + mt-byte + 4 FourCc
    }

    #[test]
    fn truncated_multitrack_header_byte_errors() {
        // PacketType=Multitrack but no following AvMultitrackType byte.
        let body = [0x96];
        assert!(matches!(
            ExVideoTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn nested_multitrack_rejected() {
        // 0x96 = Multitrack. Inner byte 0x06 = OneTrack | inner
        // PacketType=6 (Multitrack again — illegal).
        let body = [0x96, 0x06];
        assert!(matches!(
            ExVideoTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_multitrack_fourcc_errors() {
        // OneTrack but only 2 of 4 shared-FourCc bytes.
        let body = [0x96, 0x01, b'a', b'v'];
        assert!(matches!(
            ExVideoTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
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
        assert_eq!(h.fourcc, Some(u32::from_be_bytes(*b"zzzz")));
        assert_eq!(fourcc_codec_id_str(h.fourcc.unwrap()), "flv:exvideo:zzzz");
    }

    #[test]
    fn unknown_fourcc_non_ascii_falls_through_as_hex() {
        let mut body = vec![0x90];
        body.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        body.push(0x00);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(
            fourcc_codec_id_str(h.fourcc.unwrap()),
            "flv:exvideo:0x01020304"
        );
    }

    #[test]
    fn command_frame_type_recognised_and_command_byte_decoded() {
        // FrameType=5 (Command) with IsExHeader=1, PacketType=0
        // (SequenceStart, non-Metadata so spec mandates a UI8
        // VideoCommand follows the FourCc).
        // 0xD0 = 0x80 | 0x50 | 0x00.
        let mut body = vec![0xD0];
        body.extend_from_slice(b"av01");
        body.push(0x00); // VideoCommand::StartSeek
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::Command);
        assert!(!h.frame_type.is_keyframe());
        assert_eq!(h.video_command, Some(VideoCommand::StartSeek));
        // bytes_consumed must point past the command byte so the body
        // tail is empty (spec: ExVideoTagBody has no payload when
        // videoCommand has been set).
        assert_eq!(h.bytes_consumed, body.len());
        assert!(body[h.bytes_consumed..].is_empty());
    }

    #[test]
    fn command_end_seek_decoded() {
        // Command=1 → EndSeek.
        let mut body = vec![0xD0];
        body.extend_from_slice(b"av01");
        body.push(0x01);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.video_command, Some(VideoCommand::EndSeek));
    }

    #[test]
    fn command_reserved_value_preserved() {
        // Command=0x07 → Reserved(7) so future spec extensions land
        // without parser changes.
        let mut body = vec![0xD0];
        body.extend_from_slice(b"av01");
        body.push(0x07);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.video_command, Some(VideoCommand::Reserved(7)));
        assert_eq!(h.video_command.unwrap().as_u8(), 7);
    }

    #[test]
    fn command_with_metadata_packet_type_has_no_command_byte() {
        // FrameType=Command + PacketType=Metadata → spec says
        // `if (videoPacketType != VideoPacketType.Metadata &&
        //     videoFrameType == VideoFrameType.Command)` so the
        // command byte is NOT read; the trailing bytes are the AMF
        // metadata payload. Also: "frameType is ignored if
        // videoPacketType is VideoPacketType.MetaData".
        let mut body = vec![0xD4]; // 0x80 | 0x50 | 0x04
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(b"colorInfo-amf");
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::Command);
        assert_eq!(h.packet_type, ExPacketType::Metadata);
        assert_eq!(h.video_command, None);
        // bytes_consumed must stop at the AMF payload boundary (FourCc
        // end), not eat into the Metadata body.
        assert_eq!(h.bytes_consumed, 5);
        assert_eq!(&body[h.bytes_consumed..], b"colorInfo-amf");
    }

    #[test]
    fn command_byte_truncated_errors() {
        // FrameType=Command + non-Metadata packet_type but no trailing
        // command byte → spec violation.
        let mut body = vec![0xD0]; // 0x80 | 0x50 | 0x00 (SeqStart, not Metadata)
        body.extend_from_slice(b"av01");
        // (no command byte)
        assert!(matches!(
            ExVideoTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn command_after_hevc_cto_when_packet_type_is_coded_frames() {
        // Pathological-but-spec-legal: frame_type=Command (5) with
        // packet_type=CodedFrames (1) on hvc1. CodedFrames+HEVC still
        // requires SI24 CompositionTimeOffset; the command byte follows
        // it (per spec the videoCommand UI8 is read after the rest of
        // the ExVideoTagHeader has been parsed).
        let mut body = vec![0xD1]; // 0x80 | 0x50 | 0x01
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0x00, 0x00, 0x21]); // CTO = 33 (SI24)
        body.push(0x00); // VideoCommand::StartSeek
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::Command);
        assert_eq!(h.packet_type, ExPacketType::CodedFrames);
        assert_eq!(h.composition_time_offset_ms, Some(33));
        assert_eq!(h.video_command, Some(VideoCommand::StartSeek));
        assert_eq!(h.bytes_consumed, body.len());
    }

    #[test]
    fn video_command_roundtrips_as_u8() {
        assert_eq!(VideoCommand::from_u8(0), VideoCommand::StartSeek);
        assert_eq!(VideoCommand::from_u8(1), VideoCommand::EndSeek);
        assert_eq!(VideoCommand::from_u8(42), VideoCommand::Reserved(42));
        assert_eq!(VideoCommand::StartSeek.as_u8(), 0);
        assert_eq!(VideoCommand::EndSeek.as_u8(), 1);
        assert_eq!(VideoCommand::Reserved(255).as_u8(), 255);
    }

    // ----- ModEx walk on the video path (Enhanced RTMP v2 §
    // ExVideoTagHeader, while VideoPacketType == ModEx) -----

    #[test]
    fn modex_timestamp_offset_nano_decoded_on_video() {
        // 0x97 = IsExHeader (0x80) | FrameType=1 (0x10) | PacketType=7
        // (ModEx). ModEx data: size-1=2, UI24 BE = 1500 ns, trailer
        // (TSNano << 4) | inner CodedFrames=1 = 0x01. Then FourCc av01
        // (AV1 → no CTO) and 2 payload bytes.
        let mut body = vec![0x97];
        body.push(0x02); // size-1 = 2 → 3-byte payload
        body.extend_from_slice(&[0x00, 0x05, 0xDC]); // 1500 ns
        body.push(0x01); // trailer: TSNano (0) << 4 | CodedFrames
        body.extend_from_slice(b"av01");
        body.extend_from_slice(&[0xDE, 0xAD]);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::KeyFrame);
        assert_eq!(h.packet_type, ExPacketType::CodedFrames);
        assert_eq!(h.fourcc, Some(FOURCC_AV01));
        assert_eq!(h.timestamp_offset_nano, 1500);
        assert_eq!(h.mod_ex_entries.len(), 1);
        assert_eq!(h.mod_ex_entries[0].timestamp_offset_nano(), Some(1500));
        assert_eq!(
            h.mod_ex_entries[0].video_subtype(),
            VideoPacketModExType::TimestampOffsetNano
        );
        // bytes_consumed = 1 (lead) + 1 (size) + 3 (UI24) + 1 (trailer)
        //                + 4 (FourCc) = 10. AV1 CodedFrames has no CTO.
        assert_eq!(h.bytes_consumed, 10);
        assert_eq!(&body[h.bytes_consumed..], &[0xDE, 0xAD]);
    }

    #[test]
    fn modex_chains_then_resolves_to_hevc_coded_frames_with_cto() {
        // Two TSNano ModEx packets (100 + 250 ns) then HEVC CodedFrames
        // with CTO = 33 ms. Validates that the CTO parser anchors off
        // the cursor advanced by the ModEx walk, not a hardcoded 5.
        let mut body = vec![0xA7]; // 0x80 | FrameType=2 (Inter) | ModEx
        body.push(0x02);
        body.extend_from_slice(&[0x00, 0x00, 0x64]); // 100 ns
        body.push(0x07); // trailer: TSNano | ModEx (chain)
        body.push(0x02);
        body.extend_from_slice(&[0x00, 0x00, 0xFA]); // 250 ns
        body.push(0x01); // trailer: TSNano | CodedFrames
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0x00, 0x00, 0x21]); // CTO = 33 ms (SI24)
        body.extend_from_slice(&[0xCA, 0xFE]);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.frame_type, ExFrameType::InterFrame);
        assert_eq!(h.packet_type, ExPacketType::CodedFrames);
        assert_eq!(h.fourcc, Some(FOURCC_HVC1));
        assert_eq!(h.timestamp_offset_nano, 350);
        assert_eq!(h.mod_ex_entries.len(), 2);
        assert_eq!(h.composition_time_offset_ms, Some(33));
        assert_eq!(&body[h.bytes_consumed..], &[0xCA, 0xFE]);
    }

    #[test]
    fn modex_reserved_subtype_tolerated_on_video() {
        // Reserved subtype 0x5 carrying a 4-byte opaque blob, then
        // resolves to SequenceStart on av01.
        let mut body = vec![0x97];
        body.push(0x03); // size-1=3 → 4-byte payload
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        body.push(0x50); // trailer: (0x5 << 4) | SequenceStart(0)
        body.extend_from_slice(b"av01");
        body.extend_from_slice(&[0x42]);
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExPacketType::SequenceStart);
        assert_eq!(h.fourcc, Some(FOURCC_AV01));
        assert_eq!(h.timestamp_offset_nano, 0); // reserved subtype: no TS sum
        assert_eq!(h.mod_ex_entries.len(), 1);
        let entry = &h.mod_ex_entries[0];
        assert_eq!(entry.subtype_raw, 5);
        assert_eq!(entry.video_subtype(), VideoPacketModExType::Reserved(5));
        assert_eq!(entry.timestamp_offset_nano(), None);
        assert_eq!(entry.raw, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(&body[h.bytes_consumed..], &[0x42]);
    }

    #[test]
    fn modex_size_escape_handles_257_byte_payload_on_video() {
        // 0xFF size byte → escape; UI16 BE = 256 → +1 = 257. Inner
        // packet type = SequenceStart on av01.
        let mut body = vec![0x97];
        body.push(0xFF);
        body.extend_from_slice(&[0x01, 0x00]); // 257
        body.extend(std::iter::repeat(0xAB).take(257));
        body.push(0xF0); // trailer: reserved 0xF | SequenceStart
        body.extend_from_slice(b"av01");
        let h = ExVideoTagHeader::parse(&body).unwrap().unwrap();
        assert_eq!(h.packet_type, ExPacketType::SequenceStart);
        assert_eq!(h.fourcc, Some(FOURCC_AV01));
        assert_eq!(h.mod_ex_entries.len(), 1);
        assert_eq!(h.mod_ex_entries[0].raw.len(), 257);
    }

    #[test]
    fn modex_truncated_size_errors_on_video() {
        // PacketType=ModEx but no size byte.
        let body = [0x97];
        assert!(matches!(
            ExVideoTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn modex_short_timestamp_offset_nano_errors_on_video() {
        // Subtype 0 (TSNano) but payload is only 2 bytes → reject.
        let mut body = vec![0x97];
        body.push(0x01); // size-1=1 → 2-byte payload
        body.extend_from_slice(&[0xAA, 0xBB]);
        body.push(0x01); // trailer: TSNano | CodedFrames
        body.extend_from_slice(b"av01");
        assert!(matches!(
            ExVideoTagHeader::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn video_packet_modex_type_round_trips() {
        assert_eq!(
            VideoPacketModExType::from_u8(0),
            VideoPacketModExType::TimestampOffsetNano
        );
        for v in 1u8..=15 {
            assert_eq!(
                VideoPacketModExType::from_u8(v),
                VideoPacketModExType::Reserved(v)
            );
        }
    }

    // ---- to_bytes: parse-emit-parse round-trips ---------------------------

    /// Helper: parse `body`, emit via `to_bytes`, and check that the
    /// emitted bytes parse back into the same header. Returns the emitted
    /// bytes so the caller can also check the byte-level layout when
    /// useful.
    fn assert_round_trips(body: &[u8]) -> Vec<u8> {
        let h = ExVideoTagHeader::parse(body).unwrap().unwrap();
        let payload_tail = &body[h.bytes_consumed..];
        let mut out = Vec::new();
        h.to_bytes(&mut out).unwrap();
        out.extend_from_slice(payload_tail);
        let h2 = ExVideoTagHeader::parse(&out).unwrap().unwrap();
        assert_eq!(h.frame_type, h2.frame_type);
        assert_eq!(h.packet_type, h2.packet_type);
        assert_eq!(h.fourcc, h2.fourcc);
        assert_eq!(h.multitrack, h2.multitrack);
        assert_eq!(h.composition_time_offset_ms, h2.composition_time_offset_ms);
        assert_eq!(h.video_command, h2.video_command);
        assert_eq!(h.bytes_consumed, h2.bytes_consumed);
        assert_eq!(&out[h2.bytes_consumed..], payload_tail);
        out
    }

    #[test]
    fn to_bytes_av1_sequence_start_round_trip() {
        let mut body = vec![0x90];
        body.extend_from_slice(b"av01");
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let out = assert_round_trips(&body);
        // For the no-CTO single-track case the emitted bytes must match
        // the original verbatim.
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_hevc_coded_frames_round_trip_with_cto() {
        let mut body = vec![0xA1];
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0x00, 0x00, 0x21]); // CTO = 33
        body.extend_from_slice(&[0xCA, 0xFE]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_hevc_coded_frames_round_trip_with_negative_cto() {
        let mut body = vec![0xA1];
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // CTO = -1
        body.extend_from_slice(&[0x00]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_coded_frames_x_round_trip() {
        let mut body = vec![0xA3];
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0xBE, 0xEF]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_sequence_end_round_trip() {
        let mut body = vec![0x92];
        body.extend_from_slice(b"av01");
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_metadata_round_trip() {
        let mut body = vec![0x94];
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(b"colorInfo-blob");
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_multitrack_one_track_round_trip() {
        let mut body = vec![0x96];
        body.push(0x01); // OneTrack | CodedFrames
        body.extend_from_slice(b"av01");
        body.extend_from_slice(&[0x00, 0xDE, 0xAD]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_multitrack_many_tracks_many_codecs_round_trip() {
        let mut body = vec![0x96];
        body.push(0x21); // ManyTracksManyCodecs | CodedFrames
        body.extend_from_slice(&[0xCA, 0xFE]);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_command_start_seek_round_trip() {
        let mut body = vec![0xD0];
        body.extend_from_slice(b"av01");
        body.push(0x00);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_command_end_seek_round_trip() {
        let mut body = vec![0xD0];
        body.extend_from_slice(b"av01");
        body.push(0x01);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_command_after_hevc_cto_round_trip() {
        let mut body = vec![0xD1];
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(&[0x00, 0x00, 0x21]);
        body.push(0x00);
        let out = assert_round_trips(&body);
        assert_eq!(out, body);
    }

    #[test]
    fn to_bytes_rejects_cto_on_non_hevc_codec() {
        let h = ExVideoTagHeader {
            frame_type: ExFrameType::InterFrame,
            packet_type: ExPacketType::CodedFrames,
            fourcc: Some(FOURCC_AV01),
            multitrack: None,
            bytes_consumed: 0,
            composition_time_offset_ms: Some(33),
            timestamp_offset_nano: 0,
            mod_ex_entries: Vec::new(),
            video_command: None,
        };
        let mut out = Vec::new();
        assert!(matches!(h.to_bytes(&mut out), Err(Error::InvalidData(_))));
    }

    #[test]
    fn to_bytes_rejects_missing_cto_for_hevc_coded_frames() {
        let h = ExVideoTagHeader {
            frame_type: ExFrameType::InterFrame,
            packet_type: ExPacketType::CodedFrames,
            fourcc: Some(FOURCC_HVC1),
            multitrack: None,
            bytes_consumed: 0,
            composition_time_offset_ms: None,
            timestamp_offset_nano: 0,
            mod_ex_entries: Vec::new(),
            video_command: None,
        };
        let mut out = Vec::new();
        assert!(matches!(h.to_bytes(&mut out), Err(Error::InvalidData(_))));
    }

    #[test]
    fn to_bytes_rejects_cto_overflow() {
        let h = ExVideoTagHeader {
            frame_type: ExFrameType::InterFrame,
            packet_type: ExPacketType::CodedFrames,
            fourcc: Some(FOURCC_HVC1),
            multitrack: None,
            bytes_consumed: 0,
            composition_time_offset_ms: Some(1 << 23),
            timestamp_offset_nano: 0,
            mod_ex_entries: Vec::new(),
            video_command: None,
        };
        let mut out = Vec::new();
        assert!(matches!(h.to_bytes(&mut out), Err(Error::InvalidData(_))));
    }

    #[test]
    fn to_bytes_emits_modex_timestamp_offset_nano_then_resolves_to_packet_type() {
        // One ModEx entry (TimestampOffsetNano = 100 ns), then the
        // resolved VideoPacketType = SequenceStart (0) carries an AV1
        // FourCc and a 2-byte payload (dummy config record). Lead byte
        // low nibble must be `7` (ModEx); the last trailer carries the
        // resolved type (`SequenceStart`).
        let h = ExVideoTagHeader {
            frame_type: ExFrameType::KeyFrame,
            packet_type: ExPacketType::SequenceStart,
            fourcc: Some(FOURCC_AV01),
            multitrack: None,
            bytes_consumed: 0,
            composition_time_offset_ms: None,
            timestamp_offset_nano: 100,
            mod_ex_entries: vec![ModExEntry {
                subtype_raw: 0,
                payload: crate::mod_ex::ModExPayload::TimestampOffsetNano { offset_ns: 100 },
                raw: vec![0, 0, 0x64],
            }],
            video_command: None,
        };
        let mut out = Vec::new();
        h.to_bytes(&mut out).unwrap();
        // FrameType=KeyFrame (1) on bits 6..4, ModEx (7) on bits 3..0,
        // plus EX_HEADER_FLAG → 0x80 | 0x10 | 0x07 = 0x97.
        assert_eq!(out[0], 0x80 | (1 << 4) | 7);
        out.extend_from_slice(&[0xDE, 0xAD]);
        let h2 = ExVideoTagHeader::parse(&out).unwrap().unwrap();
        assert_eq!(h2.frame_type, ExFrameType::KeyFrame);
        assert_eq!(h2.packet_type, ExPacketType::SequenceStart);
        assert_eq!(h2.fourcc, Some(FOURCC_AV01));
        assert_eq!(h2.timestamp_offset_nano, 100);
        assert_eq!(h2.mod_ex_entries.len(), 1);
        assert_eq!(h2.mod_ex_entries[0].timestamp_offset_nano(), Some(100));
        assert_eq!(&out[h2.bytes_consumed..], &[0xDE, 0xAD]);
    }

    #[test]
    fn to_bytes_emits_modex_resolves_to_hevc_coded_frames_carrying_cto() {
        // ModEx run followed by HEVC CodedFrames (single-track) — the
        // CTO bytes follow the FourCc, matching the parser's
        // walk → FourCc → SI24 CTO order.
        let h = ExVideoTagHeader {
            frame_type: ExFrameType::InterFrame,
            packet_type: ExPacketType::CodedFrames,
            fourcc: Some(FOURCC_HVC1),
            multitrack: None,
            bytes_consumed: 0,
            composition_time_offset_ms: Some(42),
            timestamp_offset_nano: 500,
            mod_ex_entries: vec![ModExEntry {
                subtype_raw: 0,
                payload: crate::mod_ex::ModExPayload::TimestampOffsetNano { offset_ns: 500 },
                raw: vec![0, 0x01, 0xF4],
            }],
            video_command: None,
        };
        let mut out = Vec::new();
        h.to_bytes(&mut out).unwrap();
        out.extend_from_slice(&[0x00, 0x00, 0x01, 0x40]); // dummy NALU
        let h2 = ExVideoTagHeader::parse(&out).unwrap().unwrap();
        assert_eq!(h2.frame_type, ExFrameType::InterFrame);
        assert_eq!(h2.packet_type, ExPacketType::CodedFrames);
        assert_eq!(h2.fourcc, Some(FOURCC_HVC1));
        assert_eq!(h2.composition_time_offset_ms, Some(42));
        assert_eq!(h2.timestamp_offset_nano, 500);
        assert_eq!(h2.mod_ex_entries.len(), 1);
    }

    #[test]
    fn to_bytes_emits_modex_resolves_to_multitrack_one_track() {
        // ModEx → resolved packet_type = Multitrack(OneTrack) shared
        // FourCc = AV1; per spec the per-track payload follows the
        // multitrack header byte and shared FourCc.
        let h = ExVideoTagHeader {
            frame_type: ExFrameType::KeyFrame,
            packet_type: ExPacketType::SequenceStart, // inner type
            fourcc: Some(FOURCC_AV01),
            multitrack: Some(AvMultitrackType::OneTrack),
            bytes_consumed: 0,
            composition_time_offset_ms: None,
            timestamp_offset_nano: 250,
            mod_ex_entries: vec![ModExEntry {
                subtype_raw: 0,
                payload: crate::mod_ex::ModExPayload::TimestampOffsetNano { offset_ns: 250 },
                raw: vec![0, 0, 0xFA],
            }],
            video_command: None,
        };
        let mut out = Vec::new();
        h.to_bytes(&mut out).unwrap();
        out.extend_from_slice(&[0xCA, 0xFE]); // dummy track payload
        let h2 = ExVideoTagHeader::parse(&out).unwrap().unwrap();
        assert_eq!(h2.multitrack, Some(AvMultitrackType::OneTrack));
        assert_eq!(h2.packet_type, ExPacketType::SequenceStart);
        assert_eq!(h2.fourcc, Some(FOURCC_AV01));
        assert_eq!(h2.timestamp_offset_nano, 250);
        assert_eq!(h2.mod_ex_entries.len(), 1);
    }

    #[test]
    fn to_bytes_rejects_internally_inconsistent_timestamp_offset_nano() {
        // No entries but timestamp_offset_nano != 0 — internally
        // inconsistent (the parser would always populate the field
        // from the entries sum), so the writer must refuse.
        let h = ExVideoTagHeader {
            frame_type: ExFrameType::KeyFrame,
            packet_type: ExPacketType::CodedFrames,
            fourcc: Some(FOURCC_AV01),
            multitrack: None,
            bytes_consumed: 0,
            composition_time_offset_ms: None,
            timestamp_offset_nano: 100,
            mod_ex_entries: Vec::new(),
            video_command: None,
        };
        let mut out = Vec::new();
        assert!(matches!(h.to_bytes(&mut out), Err(Error::InvalidData(_))));
    }

    #[test]
    fn to_bytes_rejects_modex_with_wrong_raw_ui24() {
        // Internally inconsistent: typed offset_ns says 200 but the raw
        // bytes encode 100. The writer's per-entry validation catches
        // this so the wire output never disagrees with the model.
        let h = ExVideoTagHeader {
            frame_type: ExFrameType::KeyFrame,
            packet_type: ExPacketType::CodedFrames,
            fourcc: Some(FOURCC_AV01),
            multitrack: None,
            bytes_consumed: 0,
            composition_time_offset_ms: None,
            timestamp_offset_nano: 200,
            mod_ex_entries: vec![ModExEntry {
                subtype_raw: 0,
                payload: crate::mod_ex::ModExPayload::TimestampOffsetNano { offset_ns: 200 },
                raw: vec![0, 0, 0x64], // encodes 100, not 200
            }],
            video_command: None,
        };
        let mut out = Vec::new();
        assert!(matches!(h.to_bytes(&mut out), Err(Error::InvalidData(_))));
    }

    #[test]
    fn frame_type_round_trips_through_to_u8() {
        for v in 0u8..8 {
            assert_eq!(ExFrameType::from_u8(v).to_u8(), v);
        }
    }

    #[test]
    fn packet_type_round_trips_through_to_u8() {
        for v in 0u8..16 {
            assert_eq!(ExPacketType::from_u8(v).to_u8(), v);
        }
    }
}
