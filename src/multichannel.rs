//! Enhanced-RTMP-v2 `AudioPacketType.MultichannelConfig` body
//! (enhanced-rtmp-v2 §"Enhanced Audio" / §`ExAudioTagBody`).
//!
//! When an Ex audio tag carries `AudioPacketType = MultichannelConfig`
//! (4), the body — after the [`crate::ex_audio::ExAudioTagHeader`], and
//! inside the per-track payload in multitrack mode — describes the
//! stream's speaker layout. The spec defines it for codecs that are not
//! self-describing for channel mapping, and calls it out explicitly for
//! Opus streams whose `SequenceStart` payload is empty ("use the
//! `AudioPacketType.MultichannelConfig` signal for channel mapping when
//! present; otherwise, default to mono/stereo mode").
//!
//! Wire layout (all integers big-endian):
//!
//! ```text
//!   audioChannelOrder = UI8        (AudioChannelOrder)
//!   channelCount      = UI8
//!   if audioChannelOrder == Custom:
//!     audioChannelMapping = UI8[channelCount]   (one AudioChannel each)
//!   if audioChannelOrder == Native:
//!     audioChannelFlags   = UI32                (AudioChannelMask bits)
//! ```
//!
//! * `Unspecified` (0) — only the channel count is signalled.
//! * `Native` (1) — the channels appear in the same order as the
//!   `AudioChannel` enum; `audioChannelFlags` is a bitmask saying which
//!   are present (bit *i* of the mask corresponds to `AudioChannel`
//!   value *i*).
//! * `Custom` (2) — an explicit per-channel speaker map: entry 0 is the
//!   speaker for the first channel in the bitstream, entry 1 for the
//!   second, and so on.
//!
//! The parser is lenient (reserved order values and unknown trailing
//! bytes are tolerated so future spec revisions still parse); the
//! writer is strict (order/field consistency and mask invariants are
//! validated before any bytes are emitted), mirroring the crate's
//! keyframes-toc and `colorInfo` conventions.

use oxideav_core::{Error, Result};

/// `AudioChannelOrder` (UI8) — how the channels in the bitstream relate
/// to speaker positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioChannelOrder {
    /// `0` — only the channel count is specified, without any further
    /// information about the channel order.
    Unspecified,
    /// `1` — native channel order (the channels are in the same order
    /// as defined in the [`AudioChannel`] enum); presence is signalled
    /// via the `audioChannelFlags` bitmask.
    Native,
    /// `2` — the channel order does not correspond to any predefined
    /// order and is stored as an explicit map.
    Custom,
    /// `3..` — reserved for future spec revisions.
    Reserved(u8),
}

impl AudioChannelOrder {
    /// Decode the UI8 wire value.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Unspecified,
            1 => Self::Native,
            2 => Self::Custom,
            other => Self::Reserved(other),
        }
    }

    /// UI8 wire value — inverse of [`Self::from_u8`].
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Unspecified => 0,
            Self::Native => 1,
            Self::Custom => 2,
            Self::Reserved(n) => n,
        }
    }

    /// Stable lowercase label for metadata surfacing
    /// (`"unspecified"` / `"native"` / `"custom"` / `"reserved:<n>"`).
    pub fn label(self) -> String {
        match self {
            Self::Unspecified => "unspecified".into(),
            Self::Native => "native".into(),
            Self::Custom => "custom".into(),
            Self::Reserved(n) => format!("reserved:{n}"),
        }
    }
}

/// `AudioChannel` (UI8) — a speaker position for one bitstream channel.
///
/// Values `0..=17` are the commonly-used surround speaker positions;
/// `18..=23` complete 22.2 multichannel audio as standardized in
/// SMPTE ST 2036-2-2008. `0xFE` marks a channel that is empty and can
/// be safely skipped; `0xFF` marks a channel that contains data but
/// whose speaker configuration is unknown. `24..=0xFD` are reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioChannel {
    /// `0`
    FrontLeft,
    /// `1`
    FrontRight,
    /// `2`
    FrontCenter,
    /// `3`
    LowFrequency1,
    /// `4`
    BackLeft,
    /// `5`
    BackRight,
    /// `6`
    FrontLeftCenter,
    /// `7`
    FrontRightCenter,
    /// `8`
    BackCenter,
    /// `9`
    SideLeft,
    /// `10`
    SideRight,
    /// `11`
    TopCenter,
    /// `12`
    TopFrontLeft,
    /// `13`
    TopFrontCenter,
    /// `14`
    TopFrontRight,
    /// `15`
    TopBackLeft,
    /// `16`
    TopBackCenter,
    /// `17`
    TopBackRight,
    /// `18`
    LowFrequency2,
    /// `19`
    TopSideLeft,
    /// `20`
    TopSideRight,
    /// `21`
    BottomFrontCenter,
    /// `22`
    BottomFrontLeft,
    /// `23`
    BottomFrontRight,
    /// `24..=0xFD` — reserved.
    Reserved(u8),
    /// `0xFE` — channel is empty and can be safely skipped.
    Unused,
    /// `0xFF` — channel contains data, but its speaker configuration
    /// is unknown.
    Unknown,
}

impl AudioChannel {
    /// Decode the UI8 wire value.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::FrontLeft,
            1 => Self::FrontRight,
            2 => Self::FrontCenter,
            3 => Self::LowFrequency1,
            4 => Self::BackLeft,
            5 => Self::BackRight,
            6 => Self::FrontLeftCenter,
            7 => Self::FrontRightCenter,
            8 => Self::BackCenter,
            9 => Self::SideLeft,
            10 => Self::SideRight,
            11 => Self::TopCenter,
            12 => Self::TopFrontLeft,
            13 => Self::TopFrontCenter,
            14 => Self::TopFrontRight,
            15 => Self::TopBackLeft,
            16 => Self::TopBackCenter,
            17 => Self::TopBackRight,
            18 => Self::LowFrequency2,
            19 => Self::TopSideLeft,
            20 => Self::TopSideRight,
            21 => Self::BottomFrontCenter,
            22 => Self::BottomFrontLeft,
            23 => Self::BottomFrontRight,
            0xFE => Self::Unused,
            0xFF => Self::Unknown,
            other => Self::Reserved(other),
        }
    }

    /// UI8 wire value — inverse of [`Self::from_u8`].
    pub fn to_u8(self) -> u8 {
        match self {
            Self::FrontLeft => 0,
            Self::FrontRight => 1,
            Self::FrontCenter => 2,
            Self::LowFrequency1 => 3,
            Self::BackLeft => 4,
            Self::BackRight => 5,
            Self::FrontLeftCenter => 6,
            Self::FrontRightCenter => 7,
            Self::BackCenter => 8,
            Self::SideLeft => 9,
            Self::SideRight => 10,
            Self::TopCenter => 11,
            Self::TopFrontLeft => 12,
            Self::TopFrontCenter => 13,
            Self::TopFrontRight => 14,
            Self::TopBackLeft => 15,
            Self::TopBackCenter => 16,
            Self::TopBackRight => 17,
            Self::LowFrequency2 => 18,
            Self::TopSideLeft => 19,
            Self::TopSideRight => 20,
            Self::BottomFrontCenter => 21,
            Self::BottomFrontLeft => 22,
            Self::BottomFrontRight => 23,
            Self::Reserved(n) => n,
            Self::Unused => 0xFE,
            Self::Unknown => 0xFF,
        }
    }

    /// Stable lowercase label for metadata surfacing (`"frontleft"`,
    /// `"lowfrequency1"`, …, `"unused"`, `"unknown"`,
    /// `"reserved:<n>"`).
    pub fn label(self) -> String {
        match self {
            Self::FrontLeft => "frontleft".into(),
            Self::FrontRight => "frontright".into(),
            Self::FrontCenter => "frontcenter".into(),
            Self::LowFrequency1 => "lowfrequency1".into(),
            Self::BackLeft => "backleft".into(),
            Self::BackRight => "backright".into(),
            Self::FrontLeftCenter => "frontleftcenter".into(),
            Self::FrontRightCenter => "frontrightcenter".into(),
            Self::BackCenter => "backcenter".into(),
            Self::SideLeft => "sideleft".into(),
            Self::SideRight => "sideright".into(),
            Self::TopCenter => "topcenter".into(),
            Self::TopFrontLeft => "topfrontleft".into(),
            Self::TopFrontCenter => "topfrontcenter".into(),
            Self::TopFrontRight => "topfrontright".into(),
            Self::TopBackLeft => "topbackleft".into(),
            Self::TopBackCenter => "topbackcenter".into(),
            Self::TopBackRight => "topbackright".into(),
            Self::LowFrequency2 => "lowfrequency2".into(),
            Self::TopSideLeft => "topsideleft".into(),
            Self::TopSideRight => "topsideright".into(),
            Self::BottomFrontCenter => "bottomfrontcenter".into(),
            Self::BottomFrontLeft => "bottomfrontleft".into(),
            Self::BottomFrontRight => "bottomfrontright".into(),
            Self::Reserved(n) => format!("reserved:{n}"),
            Self::Unused => "unused".into(),
            Self::Unknown => "unknown".into(),
        }
    }
}

/// Number of `AudioChannelMask` bits the spec defines (bits `0..24`;
/// bit *i* corresponds to [`AudioChannel`] value *i*).
pub const CHANNEL_MASK_BITS: u32 = 24;

/// Every spec-defined `AudioChannelMask` bit set
/// (`FrontLeft ..= BottomFrontRight`).
pub const CHANNEL_MASK_ALL: u32 = (1 << CHANNEL_MASK_BITS) - 1;

/// Speaker labels for every spec-defined bit set in a `Native`-order
/// `audioChannelFlags` mask, in mask-bit (= [`AudioChannel`] enum)
/// order. Bits outside the spec-defined range (`>= 2^24`) are ignored —
/// they stay visible through the raw flags value instead.
pub fn mask_channel_labels(flags: u32) -> Vec<String> {
    (0..CHANNEL_MASK_BITS)
        .filter(|bit| flags & (1 << bit) != 0)
        .map(|bit| AudioChannel::from_u8(bit as u8).label())
        .collect()
}

/// Parsed `AudioPacketType.MultichannelConfig` body.
///
/// Invariants on the write side ([`Self::to_bytes`]):
///
/// * `Custom` order requires `mapping` with exactly `channel_count`
///   entries and no `channel_flags`.
/// * `Native` order requires `channel_flags` whose set bits all fall in
///   the spec-defined mask range and whose population count equals
///   `channel_count`, and no `mapping`.
/// * `Unspecified` order requires neither.
/// * `Reserved(_)` orders have no spec-defined wire recipe and are
///   rejected.
///
/// The parser is lenient: it accepts reserved order values (leaving the
/// unparseable tail opaque), `Native` masks with reserved high bits,
/// and population counts that disagree with `channel_count` — the
/// caller sees exactly what the producer signalled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultichannelConfig {
    /// How the bitstream channels relate to speaker positions.
    pub order: AudioChannelOrder,
    /// Number of channels in the bitstream.
    pub channel_count: u8,
    /// Explicit per-channel speaker map — `Some` iff
    /// `order == Custom`. Entry 0 is the speaker for the first channel
    /// in the bitstream, entry 1 for the second, and so on.
    pub mapping: Option<Vec<AudioChannel>>,
    /// `audioChannelFlags` presence bitmask — `Some` iff
    /// `order == Native`. AND with `1 << AudioChannel::to_u8(..)` (for
    /// values below [`CHANNEL_MASK_BITS`]) to test whether a specific
    /// speaker is present.
    pub channel_flags: Option<u32>,
}

impl MultichannelConfig {
    /// Parse a `MultichannelConfig` body (the bytes following the
    /// `ExAudioTagHeader`, or the per-track payload in multitrack
    /// mode).
    ///
    /// Trailing bytes beyond the parsed structure are ignored
    /// (forward-compat with future spec additions); a body too short
    /// for the structure its order byte promises errors with
    /// [`Error::InvalidData`].
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 2 {
            return Err(Error::invalid(
                "FLV MultichannelConfig: body shorter than order + count bytes",
            ));
        }
        let order = AudioChannelOrder::from_u8(body[0]);
        let channel_count = body[1];
        let mut cursor = 2usize;
        let mapping = if order == AudioChannelOrder::Custom {
            let n = channel_count as usize;
            if body.len() < cursor + n {
                return Err(Error::invalid(
                    "FLV MultichannelConfig: truncated Custom channel mapping",
                ));
            }
            let map = body[cursor..cursor + n]
                .iter()
                .map(|&b| AudioChannel::from_u8(b))
                .collect();
            cursor += n;
            Some(map)
        } else {
            None
        };
        let channel_flags = if order == AudioChannelOrder::Native {
            if body.len() < cursor + 4 {
                return Err(Error::invalid(
                    "FLV MultichannelConfig: truncated Native channel flags",
                ));
            }
            let flags = u32::from_be_bytes([
                body[cursor],
                body[cursor + 1],
                body[cursor + 2],
                body[cursor + 3],
            ]);
            Some(flags)
        } else {
            None
        };
        Ok(Self {
            order,
            channel_count,
            mapping,
            channel_flags,
        })
    }

    /// Serialise this config to the wire bytes that follow the
    /// `ExAudioTagHeader` — the inverse of [`Self::parse`]. Appends to
    /// `out`; on error nothing is written.
    pub fn to_bytes(&self, out: &mut Vec<u8>) -> Result<()> {
        match self.order {
            AudioChannelOrder::Unspecified => {
                if self.mapping.is_some() || self.channel_flags.is_some() {
                    return Err(Error::invalid(
                        "FLV MultichannelConfig: Unspecified order carries no \
                         mapping or channel flags",
                    ));
                }
            }
            AudioChannelOrder::Custom => {
                if self.channel_flags.is_some() {
                    return Err(Error::invalid(
                        "FLV MultichannelConfig: Custom order carries a mapping, \
                         not channel flags",
                    ));
                }
                let mapping = self.mapping.as_ref().ok_or_else(|| {
                    Error::invalid("FLV MultichannelConfig: Custom order needs a channel mapping")
                })?;
                if mapping.len() != self.channel_count as usize {
                    return Err(Error::invalid(format!(
                        "FLV MultichannelConfig: mapping has {} entries but \
                         channelCount is {}",
                        mapping.len(),
                        self.channel_count
                    )));
                }
            }
            AudioChannelOrder::Native => {
                if self.mapping.is_some() {
                    return Err(Error::invalid(
                        "FLV MultichannelConfig: Native order carries channel \
                         flags, not a mapping",
                    ));
                }
                let flags = self.channel_flags.ok_or_else(|| {
                    Error::invalid("FLV MultichannelConfig: Native order needs channel flags")
                })?;
                if flags & !CHANNEL_MASK_ALL != 0 {
                    return Err(Error::invalid(format!(
                        "FLV MultichannelConfig: channel flags 0x{flags:08X} set \
                         bits outside the spec-defined AudioChannelMask range"
                    )));
                }
                if flags.count_ones() != self.channel_count as u32 {
                    return Err(Error::invalid(format!(
                        "FLV MultichannelConfig: channel flags 0x{flags:08X} mark \
                         {} channel(s) present but channelCount is {}",
                        flags.count_ones(),
                        self.channel_count
                    )));
                }
            }
            AudioChannelOrder::Reserved(n) => {
                return Err(Error::invalid(format!(
                    "FLV MultichannelConfig: reserved AudioChannelOrder {n} has \
                     no spec-defined wire layout"
                )));
            }
        }
        out.push(self.order.to_u8());
        out.push(self.channel_count);
        if let Some(mapping) = &self.mapping {
            out.extend(mapping.iter().map(|c| c.to_u8()));
        }
        if let Some(flags) = self.channel_flags {
            out.extend_from_slice(&flags.to_be_bytes());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_native_5_1() {
        // 5.1: FrontLeft | FrontRight | FrontCenter | LowFrequency1 |
        // BackLeft | BackRight = 0x3F.
        let body = [0x01, 6, 0x00, 0x00, 0x00, 0x3F];
        let mcc = MultichannelConfig::parse(&body).unwrap();
        assert_eq!(mcc.order, AudioChannelOrder::Native);
        assert_eq!(mcc.channel_count, 6);
        assert_eq!(mcc.channel_flags, Some(0x3F));
        assert_eq!(mcc.mapping, None);
        assert_eq!(
            mask_channel_labels(0x3F),
            vec![
                "frontleft",
                "frontright",
                "frontcenter",
                "lowfrequency1",
                "backleft",
                "backright"
            ]
        );
    }

    #[test]
    fn parse_custom_mapping() {
        let body = [0x02, 3, 0x01, 0x00, 0xFE];
        let mcc = MultichannelConfig::parse(&body).unwrap();
        assert_eq!(mcc.order, AudioChannelOrder::Custom);
        assert_eq!(mcc.channel_count, 3);
        assert_eq!(
            mcc.mapping,
            Some(vec![
                AudioChannel::FrontRight,
                AudioChannel::FrontLeft,
                AudioChannel::Unused
            ])
        );
        assert_eq!(mcc.channel_flags, None);
    }

    #[test]
    fn parse_unspecified_count_only() {
        let body = [0x00, 2];
        let mcc = MultichannelConfig::parse(&body).unwrap();
        assert_eq!(mcc.order, AudioChannelOrder::Unspecified);
        assert_eq!(mcc.channel_count, 2);
        assert_eq!(mcc.mapping, None);
        assert_eq!(mcc.channel_flags, None);
    }

    #[test]
    fn parse_reserved_order_is_lenient() {
        // Reserved order: count still surfaces; the opaque tail is
        // ignored so a future spec revision degrades gracefully.
        let body = [0x07, 4, 0xDE, 0xAD];
        let mcc = MultichannelConfig::parse(&body).unwrap();
        assert_eq!(mcc.order, AudioChannelOrder::Reserved(7));
        assert_eq!(mcc.channel_count, 4);
        assert_eq!(mcc.mapping, None);
        assert_eq!(mcc.channel_flags, None);
    }

    #[test]
    fn parse_native_reserved_high_bits_are_lenient() {
        // Bit 31 is outside the spec-defined mask range — the parser
        // preserves it verbatim; only the writer rejects it.
        let body = [0x01, 1, 0x80, 0x00, 0x00, 0x01];
        let mcc = MultichannelConfig::parse(&body).unwrap();
        assert_eq!(mcc.channel_flags, Some(0x8000_0001));
        assert_eq!(mask_channel_labels(0x8000_0001), vec!["frontleft"]);
    }

    #[test]
    fn parse_trailing_bytes_ignored() {
        let body = [0x00, 2, 0xAA, 0xBB];
        let mcc = MultichannelConfig::parse(&body).unwrap();
        assert_eq!(mcc.channel_count, 2);
    }

    #[test]
    fn parse_truncation_errors() {
        for body in [
            &[][..],
            &[0x01][..],                      // missing count
            &[0x02, 3, 0x00, 0x01][..],       // Custom: 2 of 3 mapping bytes
            &[0x01, 2, 0x00, 0x00, 0x03][..], // Native: 3 of 4 flag bytes
        ] {
            assert!(
                matches!(MultichannelConfig::parse(body), Err(Error::InvalidData(_))),
                "body {body:02X?} should error"
            );
        }
    }

    #[test]
    fn to_bytes_round_trips_all_orders() {
        let configs = [
            MultichannelConfig {
                order: AudioChannelOrder::Unspecified,
                channel_count: 2,
                mapping: None,
                channel_flags: None,
            },
            MultichannelConfig {
                order: AudioChannelOrder::Native,
                channel_count: 6,
                mapping: None,
                channel_flags: Some(0x3F),
            },
            MultichannelConfig {
                order: AudioChannelOrder::Custom,
                channel_count: 4,
                mapping: Some(vec![
                    AudioChannel::FrontLeft,
                    AudioChannel::FrontRight,
                    AudioChannel::Unknown,
                    AudioChannel::Unused,
                ]),
                channel_flags: None,
            },
        ];
        for mcc in configs {
            let mut wire = Vec::new();
            mcc.to_bytes(&mut wire).unwrap();
            let back = MultichannelConfig::parse(&wire).unwrap();
            assert_eq!(back, mcc);
        }
    }

    #[test]
    fn to_bytes_rejects_inconsistent_shapes() {
        let cases = [
            // Custom without a mapping.
            MultichannelConfig {
                order: AudioChannelOrder::Custom,
                channel_count: 2,
                mapping: None,
                channel_flags: None,
            },
            // Custom mapping length != channel_count.
            MultichannelConfig {
                order: AudioChannelOrder::Custom,
                channel_count: 3,
                mapping: Some(vec![AudioChannel::FrontLeft]),
                channel_flags: None,
            },
            // Custom with channel flags.
            MultichannelConfig {
                order: AudioChannelOrder::Custom,
                channel_count: 1,
                mapping: Some(vec![AudioChannel::FrontLeft]),
                channel_flags: Some(1),
            },
            // Native without flags.
            MultichannelConfig {
                order: AudioChannelOrder::Native,
                channel_count: 2,
                mapping: None,
                channel_flags: None,
            },
            // Native with a mapping.
            MultichannelConfig {
                order: AudioChannelOrder::Native,
                channel_count: 1,
                mapping: Some(vec![AudioChannel::FrontLeft]),
                channel_flags: Some(1),
            },
            // Native flags with reserved high bits.
            MultichannelConfig {
                order: AudioChannelOrder::Native,
                channel_count: 1,
                mapping: None,
                channel_flags: Some(0x0100_0000),
            },
            // Native popcount != channel_count.
            MultichannelConfig {
                order: AudioChannelOrder::Native,
                channel_count: 3,
                mapping: None,
                channel_flags: Some(0x03),
            },
            // Unspecified with extras.
            MultichannelConfig {
                order: AudioChannelOrder::Unspecified,
                channel_count: 2,
                mapping: None,
                channel_flags: Some(0x03),
            },
            // Reserved order has no wire layout.
            MultichannelConfig {
                order: AudioChannelOrder::Reserved(9),
                channel_count: 2,
                mapping: None,
                channel_flags: None,
            },
        ];
        for mcc in cases {
            let mut wire = Vec::new();
            assert!(
                matches!(mcc.to_bytes(&mut wire), Err(Error::InvalidData(_))),
                "{mcc:?} should be rejected"
            );
            assert!(wire.is_empty(), "failed write must not emit bytes");
        }
    }

    #[test]
    fn audio_channel_round_trips_through_to_u8() {
        for v in 0u8..=255 {
            assert_eq!(AudioChannel::from_u8(v).to_u8(), v);
        }
    }

    #[test]
    fn audio_channel_order_round_trips_through_to_u8() {
        for v in 0u8..=255 {
            assert_eq!(AudioChannelOrder::from_u8(v).to_u8(), v);
        }
    }

    #[test]
    fn channel_22_2_mask_completes_the_enum() {
        // All 24 spec-defined positions present (22.2 superset mask).
        let labels = mask_channel_labels(CHANNEL_MASK_ALL);
        assert_eq!(labels.len(), 24);
        assert_eq!(labels[0], "frontleft");
        assert_eq!(labels[18], "lowfrequency2");
        assert_eq!(labels[23], "bottomfrontright");
    }
}
