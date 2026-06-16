//! Typed wiring for the Enhanced-RTMP-v2 *Enhancing NetConnection
//! `connect` Command* (Veovera `enhanced-rtmp-v2` §"Enhancing
//! NetConnection connect Command").
//!
//! When a client connects to an E-RTMP server it sends a `connect`
//! command whose **Command Object** is an AMF0 object of name/value
//! pairs. E-RTMP extends that object with new properties so the client
//! can declare which enhanced codecs and protocol capabilities it
//! supports:
//!
//! | Property            | Type                     | Purpose                                              |
//! | ------------------- | ------------------------ | ---------------------------------------------------- |
//! | `fourCcList`        | Strict Array of strings  | enhanced codec list (legacy; `"*"` wildcard allowed) |
//! | `videoFourCcInfoMap`| Object (FourCC → number) | per-video-codec [`FourCcInfoMask`] capability flags  |
//! | `audioFourCcInfoMap`| Object (FourCC → number) | per-audio-codec [`FourCcInfoMask`] capability flags  |
//! | `capsEx`            | number                   | extended [`CapsExMask`] protocol capability flags    |
//!
//! The spec notes `fourCcList` is the original E-RTMP mechanism and
//! RECOMMENDS clients switch to the `[audio|video]FourCcInfoMap`
//! properties going forward, while servers SHOULD accept both. The
//! server's `_result` response echoes its own support via the same
//! `videoFourCcInfoMap` / `capsEx` shape, so the encoder/parser here is
//! symmetric for both directions.
//!
//! Like [`crate::on_status`], `connect` is an RTMP **command message**,
//! not an FLV `SCRIPTDATA` tag: the full command is the AMF0 sequence
//! `"connect"`, a transaction id (Number, conventionally `1`), then the
//! Command Object. This module provides a typed [`ConnectCommandObject`]
//! builder, an AMF0 serialiser for both the bare Command Object
//! ([`write_command_object`]) and the full command sequence
//! ([`write_connect_command`]), and a parser
//! ([`parse_connect_command`] / [`parse_command_object`]) that recovers
//! the typed view — closing the read↔write loop the same way
//! `colorInfo` and `onStatus` do.

use std::io::Write;

use oxideav_core::{Error, Result};

use crate::amf0::{self, parse_amf0_value, AmfValue};

/// AMF0 command name for the NetConnection `connect` command.
pub const COMMAND_NAME: &str = "connect";

/// Wildcard FourCC key — a catch-all that, when present in a
/// `[audio|video]FourCcInfoMap` or `fourCcList`, applies to any codec.
/// Per spec: a `"*"` key "overrides the flags set on properties for
/// specific codecs".
pub const FOURCC_WILDCARD: &str = "*";

/// Per-codec capability flags carried in the values of
/// `videoFourCcInfoMap` / `audioFourCcInfoMap`. Combine with bitwise OR.
///
/// Per the E-RTMP-v2 `enum FourCcInfoMask` in the §"Enhancing
/// NetConnection connect Command" table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FourCcInfoMask;

impl FourCcInfoMask {
    /// The peer can decode this codec.
    pub const CAN_DECODE: u32 = 0x01;
    /// The peer can encode this codec.
    pub const CAN_ENCODE: u32 = 0x02;
    /// The peer can forward (relay without transcoding) this codec.
    pub const CAN_FORWARD: u32 = 0x04;

    /// Bitmask of all currently-defined flags. Bits outside this set are
    /// spec-reserved and preserved verbatim on round-trip.
    pub const ALL: u32 = Self::CAN_DECODE | Self::CAN_ENCODE | Self::CAN_FORWARD;
}

/// Extended protocol capability flags carried in the `capsEx` Number.
/// Combine with bitwise OR. Per the E-RTMP-v2 `enum CapsExMask`.
///
/// "If the extended capabilities are expressed elsewhere they will not
/// appear here (e.g., FourCC, HDR or `VideoPacketType.Metadata` support
/// is not expressed in this property)."
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapsExMask;

impl CapsExMask {
    /// Support for the reconnection feature (pairs with
    /// [`crate::on_status::CODE_RECONNECT_REQUEST`]).
    pub const RECONNECT: u32 = 0x01;
    /// Support for audio/video multitrack.
    pub const MULTITRACK: u32 = 0x02;
    /// Can parse the ModEx signal.
    pub const MOD_EX: u32 = 0x04;
    /// Support for the nanosecond timestamp offset (`TimestampOffsetNano`).
    pub const TIMESTAMP_NANO_OFFSET: u32 = 0x08;

    /// Bitmask of all currently-defined flags. Bits outside this set are
    /// spec-reserved and preserved verbatim on round-trip.
    pub const ALL: u32 =
        Self::RECONNECT | Self::MULTITRACK | Self::MOD_EX | Self::TIMESTAMP_NANO_OFFSET;
}

/// Typed view of the E-RTMP-v2 `connect` Command Object.
///
/// The four E-RTMP-new properties get first-class fields; any other
/// Command Object properties a producer set (`app`, `tcUrl`,
/// `flashVer`, `objectEncoding`, …) are preserved in [`extra`] so the
/// object round-trips byte-for-byte the values it carried. Only
/// string-valued and number-valued extras are retained — the legacy
/// `connect` Command Object properties are all strings or numbers.
///
/// [`extra`]: ConnectCommandObject::extra
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectCommandObject {
    /// `fourCcList` — the legacy enhanced-codec declaration: a strict
    /// array of FourCC strings (`"av01"`, `"hvc1"`, …) or the single
    /// wildcard `["*"]`. `None` when the producer did not send it.
    pub four_cc_list: Option<Vec<String>>,
    /// `videoFourCcInfoMap` — per-video-codec capability flags keyed by
    /// FourCC string (insertion order preserved). `None` when absent.
    pub video_four_cc_info_map: Option<Vec<(String, u32)>>,
    /// `audioFourCcInfoMap` — per-audio-codec capability flags keyed by
    /// FourCC string (insertion order preserved). `None` when absent.
    pub audio_four_cc_info_map: Option<Vec<(String, u32)>>,
    /// `capsEx` — extended [`CapsExMask`] capability flags. `None` when
    /// the producer did not send it.
    pub caps_ex: Option<u32>,
    /// Other Command Object properties, preserved in insertion order so
    /// the object round-trips. Each value is either a String or a Number
    /// (`AmfNum`) — the two scalar types the legacy `connect` Command
    /// Object uses (`app` / `tcUrl` / `flashVer` strings,
    /// `objectEncoding` / `audioCodecs` / `videoCodecs` numbers).
    pub extra: Vec<(String, ScalarValue)>,
}

/// A scalar AMF0 value preserved on the [`ConnectCommandObject::extra`]
/// list — the subset (String / Number) the `connect` Command Object's
/// non-E-RTMP properties use.
#[derive(Clone, Debug, PartialEq)]
pub enum ScalarValue {
    /// AMF0 String.
    Str(String),
    /// AMF0 Number.
    Num(f64),
    /// AMF0 Boolean.
    Bool(bool),
}

impl ConnectCommandObject {
    /// An empty Command Object (no E-RTMP properties, no extras).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the `fourCcList` legacy enhanced-codec declaration.
    #[must_use]
    pub fn four_cc_list<I, S>(mut self, codecs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.four_cc_list = Some(codecs.into_iter().map(Into::into).collect());
        self
    }

    /// Set the `fourCcList` to the single wildcard `["*"]` — a client
    /// (recorder / forwarder) that can receive any codec.
    #[must_use]
    pub fn four_cc_list_wildcard(mut self) -> Self {
        self.four_cc_list = Some(vec![FOURCC_WILDCARD.to_string()]);
        self
    }

    /// Add one `videoFourCcInfoMap` entry: a FourCC string key and its
    /// [`FourCcInfoMask`] flags. Creates the map if absent.
    #[must_use]
    pub fn video_codec(mut self, fourcc: &str, mask: u32) -> Self {
        self.video_four_cc_info_map
            .get_or_insert_with(Vec::new)
            .push((fourcc.to_string(), mask));
        self
    }

    /// Add one `audioFourCcInfoMap` entry: a FourCC string key and its
    /// [`FourCcInfoMask`] flags. Creates the map if absent.
    #[must_use]
    pub fn audio_codec(mut self, fourcc: &str, mask: u32) -> Self {
        self.audio_four_cc_info_map
            .get_or_insert_with(Vec::new)
            .push((fourcc.to_string(), mask));
        self
    }

    /// Set the `capsEx` extended-capability flags.
    #[must_use]
    pub fn caps_ex(mut self, mask: u32) -> Self {
        self.caps_ex = Some(mask);
        self
    }

    /// Append a String extra property (e.g. `app`, `tcUrl`, `flashVer`).
    #[must_use]
    pub fn string(mut self, name: &str, value: &str) -> Self {
        self.extra
            .push((name.to_string(), ScalarValue::Str(value.to_string())));
        self
    }

    /// Append a Number extra property (e.g. `objectEncoding`).
    #[must_use]
    pub fn number(mut self, name: &str, value: f64) -> Self {
        self.extra.push((name.to_string(), ScalarValue::Num(value)));
        self
    }

    /// Append a Boolean extra property (e.g. `fpad`).
    #[must_use]
    pub fn boolean(mut self, name: &str, value: bool) -> Self {
        self.extra
            .push((name.to_string(), ScalarValue::Bool(value)));
        self
    }

    /// `true` when this Command Object advertises reconnection support
    /// via `capsEx & CapsExMask.Reconnect` — the capability a server
    /// checks before sending a `NetConnection.Connect.ReconnectRequest`.
    pub fn supports_reconnect(&self) -> bool {
        self.caps_ex.is_some_and(|c| c & CapsExMask::RECONNECT != 0)
    }

    /// `true` when this Command Object advertises multitrack support
    /// via `capsEx & CapsExMask.Multitrack`.
    pub fn supports_multitrack(&self) -> bool {
        self.caps_ex
            .is_some_and(|c| c & CapsExMask::MULTITRACK != 0)
    }
}

/// Serialise just the `connect` Command Object (the anonymous AMF0
/// Object value) into `out`. Properties are emitted in the order:
/// the [`ConnectCommandObject::extra`] pairs first (preserving the
/// producer's leading `app` / `tcUrl` / … fields), then the E-RTMP
/// properties `fourCcList`, `videoFourCcInfoMap`, `audioFourCcInfoMap`,
/// `capsEx` (each only when present), then the object-end terminator.
pub fn write_command_object<W: Write + ?Sized>(
    out: &mut W,
    obj: &ConnectCommandObject,
) -> Result<()> {
    amf0::write_object_start(out)?;
    for (name, value) in &obj.extra {
        amf0::write_property_name(out, name)?;
        match value {
            ScalarValue::Str(s) => amf0::write_string(out, s)?,
            ScalarValue::Num(n) => amf0::write_number(out, *n)?,
            ScalarValue::Bool(b) => amf0::write_boolean(out, *b)?,
        }
    }
    if let Some(list) = &obj.four_cc_list {
        amf0::write_property_name(out, "fourCcList")?;
        let refs: Vec<&str> = list.iter().map(String::as_str).collect();
        amf0::write_strict_array_string(out, &refs)?;
    }
    if let Some(map) = &obj.video_four_cc_info_map {
        amf0::write_property_name(out, "videoFourCcInfoMap")?;
        write_info_map(out, map)?;
    }
    if let Some(map) = &obj.audio_four_cc_info_map {
        amf0::write_property_name(out, "audioFourCcInfoMap")?;
        write_info_map(out, map)?;
    }
    if let Some(caps) = obj.caps_ex {
        amf0::write_property_name(out, "capsEx")?;
        amf0::write_number(out, f64::from(caps))?;
    }
    amf0::write_object_end(out)?;
    Ok(())
}

/// Write a `[audio|video]FourCcInfoMap` value — an anonymous AMF0 Object
/// whose property names are FourCC strings and whose values are the
/// capability-flag Numbers.
fn write_info_map<W: Write + ?Sized>(out: &mut W, map: &[(String, u32)]) -> Result<()> {
    amf0::write_object_start(out)?;
    for (fourcc, mask) in map {
        amf0::write_property_name(out, fourcc)?;
        amf0::write_number(out, f64::from(*mask))?;
    }
    amf0::write_object_end(out)?;
    Ok(())
}

/// Serialise the full `connect` command sequence into `out` (no RTMP
/// chunk framing): command name `"connect"`, the transaction id
/// (Number, conventionally `1`), then the Command Object.
pub fn write_connect_command_body<W: Write + ?Sized>(
    out: &mut W,
    transaction_id: f64,
    obj: &ConnectCommandObject,
) -> Result<()> {
    amf0::write_string(out, COMMAND_NAME)?;
    amf0::write_number(out, transaction_id)?;
    write_command_object(out, obj)?;
    Ok(())
}

/// Serialise the full `connect` command sequence to a fresh `Vec<u8>`
/// (transaction id conventionally `1`).
pub fn write_connect_command(transaction_id: f64, obj: &ConnectCommandObject) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(256);
    write_connect_command_body(&mut out, transaction_id, obj)?;
    Ok(out)
}

/// Parse a bare `connect` Command Object (an AMF0 Object value) back
/// into a typed [`ConnectCommandObject`]. The E-RTMP properties
/// (`fourCcList`, `videoFourCcInfoMap`, `audioFourCcInfoMap`, `capsEx`)
/// route into their typed fields; every other String / Number / Boolean
/// property lands in [`ConnectCommandObject::extra`] in bitstream order.
/// Other property value types are ignored.
///
/// Returns [`Error::invalid`] if the value is not an AMF0 Object /
/// ECMA-array.
pub fn parse_command_object(buf: &[u8], pos: usize) -> Result<(ConnectCommandObject, usize)> {
    let (val, next) = parse_amf0_value(buf, pos)?;
    let props = match val {
        AmfValue::Object(props) | AmfValue::EcmaArray(props) => props,
        other => {
            return Err(Error::invalid(format!(
                "FLV connect: Command Object must be an AMF0 Object, got {other:?}"
            )));
        }
    };

    let mut obj = ConnectCommandObject::new();
    for (key, value) in props {
        match key.as_str() {
            "fourCcList" => {
                if let AmfValue::StrictArray(items) = value {
                    let list = items
                        .into_iter()
                        .filter_map(|v| match v {
                            AmfValue::String(s) => Some(s),
                            _ => None,
                        })
                        .collect();
                    obj.four_cc_list = Some(list);
                }
            }
            "videoFourCcInfoMap" => {
                obj.video_four_cc_info_map = Some(parse_info_map(value));
            }
            "audioFourCcInfoMap" => {
                obj.audio_four_cc_info_map = Some(parse_info_map(value));
            }
            "capsEx" => {
                if let AmfValue::Number(n) = value {
                    obj.caps_ex = Some(num_to_flags(n));
                }
            }
            _ => match value {
                AmfValue::String(s) => obj.extra.push((key, ScalarValue::Str(s))),
                AmfValue::Number(n) => obj.extra.push((key, ScalarValue::Num(n))),
                AmfValue::Boolean(b) => obj.extra.push((key, ScalarValue::Bool(b))),
                _ => {}
            },
        }
    }
    Ok((obj, next))
}

/// Lower a parsed AMF0 Object/ECMA-array into a FourCC→flags map,
/// keeping only Number-valued entries (insertion order preserved).
fn parse_info_map(value: AmfValue) -> Vec<(String, u32)> {
    let props = match value {
        AmfValue::Object(props) | AmfValue::EcmaArray(props) => props,
        _ => return Vec::new(),
    };
    props
        .into_iter()
        .filter_map(|(k, v)| match v {
            AmfValue::Number(n) => Some((k, num_to_flags(n))),
            _ => None,
        })
        .collect()
}

/// Convert an AMF0 Number capability mask to `u32`, clamping a
/// non-finite or out-of-range value to `0` (a forged stream cannot
/// produce a panicking cast).
fn num_to_flags(n: f64) -> u32 {
    if n.is_finite() && n >= 0.0 && n <= f64::from(u32::MAX) {
        n as u32
    } else {
        0
    }
}

/// Parse a full `connect` command sequence back into a typed
/// [`ConnectCommandObject`], closing the read↔write loop. Validates the
/// command name (`"connect"`), skips the transaction id, and parses the
/// Command Object.
///
/// Returns [`Error::invalid`] if the byte stream does not begin with the
/// `"connect"` command name or does not carry an Object Command Object.
pub fn parse_connect_command(buf: &[u8]) -> Result<ConnectCommandObject> {
    let (name, p) = parse_amf0_value(buf, 0)?;
    match name {
        AmfValue::String(s) if s == COMMAND_NAME => {}
        other => {
            return Err(Error::invalid(format!(
                "FLV connect: expected command name \"{COMMAND_NAME}\", got {other:?}"
            )));
        }
    }
    // Transaction id (Number) — skip.
    let (_txn, p) = parse_amf0_value(buf, p)?;
    let (obj, _p) = parse_command_object(buf, p)?;
    Ok(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_match_spec_bit_values() {
        assert_eq!(FourCcInfoMask::CAN_DECODE, 0x01);
        assert_eq!(FourCcInfoMask::CAN_ENCODE, 0x02);
        assert_eq!(FourCcInfoMask::CAN_FORWARD, 0x04);
        assert_eq!(CapsExMask::RECONNECT, 0x01);
        assert_eq!(CapsExMask::MULTITRACK, 0x02);
        assert_eq!(CapsExMask::MOD_EX, 0x04);
        assert_eq!(CapsExMask::TIMESTAMP_NANO_OFFSET, 0x08);
    }

    #[test]
    fn command_body_starts_with_name_and_txn() {
        let obj = ConnectCommandObject::new().caps_ex(CapsExMask::RECONNECT);
        let bytes = write_connect_command(1.0, &obj).unwrap();
        // String marker + UI16 len(7) + "connect".
        assert_eq!(bytes[0], 0x02);
        assert_eq!(u16::from_be_bytes([bytes[1], bytes[2]]), 7);
        assert_eq!(&bytes[3..10], b"connect");
        // Number marker (transaction id 1.0).
        assert_eq!(bytes[10], 0x00);
        assert_eq!(&bytes[11..19], &1.0f64.to_be_bytes());
        // Command Object marker.
        assert_eq!(bytes[19], 0x03);
    }

    #[test]
    fn full_connect_round_trips() {
        let obj = ConnectCommandObject::new()
            .string("app", "live")
            .string("tcUrl", "rtmp://127.0.0.1/live")
            .number("objectEncoding", 0.0)
            .four_cc_list(["av01", "hvc1", "Opus"])
            .video_codec(
                "vp09",
                FourCcInfoMask::CAN_DECODE | FourCcInfoMask::CAN_ENCODE,
            )
            .audio_codec("Opus", FourCcInfoMask::CAN_DECODE)
            .caps_ex(CapsExMask::RECONNECT | CapsExMask::MULTITRACK);
        let bytes = write_connect_command(1.0, &obj).unwrap();
        let back = parse_connect_command(&bytes).unwrap();
        assert_eq!(back, obj);
        assert!(back.supports_reconnect());
        assert!(back.supports_multitrack());
    }

    #[test]
    fn wildcard_four_cc_list_round_trips() {
        let obj = ConnectCommandObject::new().four_cc_list_wildcard();
        let bytes = write_connect_command(1.0, &obj).unwrap();
        let back = parse_connect_command(&bytes).unwrap();
        assert_eq!(
            back.four_cc_list.as_deref(),
            Some(&[FOURCC_WILDCARD.to_string()][..])
        );
    }

    #[test]
    fn info_map_preserves_wildcard_and_order() {
        let obj = ConnectCommandObject::new()
            .video_codec(FOURCC_WILDCARD, FourCcInfoMask::CAN_FORWARD)
            .video_codec(
                "vp09",
                FourCcInfoMask::CAN_DECODE | FourCcInfoMask::CAN_ENCODE,
            );
        let bytes = write_connect_command(1.0, &obj).unwrap();
        let back = parse_connect_command(&bytes).unwrap();
        let map = back.video_four_cc_info_map.unwrap();
        assert_eq!(map[0], ("*".to_string(), FourCcInfoMask::CAN_FORWARD));
        assert_eq!(
            map[1],
            (
                "vp09".to_string(),
                FourCcInfoMask::CAN_DECODE | FourCcInfoMask::CAN_ENCODE
            )
        );
    }

    #[test]
    fn extra_properties_survive_in_order() {
        let obj = ConnectCommandObject::new()
            .string("app", "myapp")
            .number("audioCodecs", 4071.0)
            .number("videoCodecs", 252.0)
            .boolean("fpad", false);
        let bytes = write_connect_command(1.0, &obj).unwrap();
        let back = parse_connect_command(&bytes).unwrap();
        assert_eq!(
            back.extra,
            vec![
                ("app".to_string(), ScalarValue::Str("myapp".to_string())),
                ("audioCodecs".to_string(), ScalarValue::Num(4071.0)),
                ("videoCodecs".to_string(), ScalarValue::Num(252.0)),
                ("fpad".to_string(), ScalarValue::Bool(false)),
            ]
        );
    }

    #[test]
    fn caps_ex_all_bits_round_trip() {
        let obj = ConnectCommandObject::new().caps_ex(CapsExMask::ALL);
        let bytes = write_connect_command(1.0, &obj).unwrap();
        let back = parse_connect_command(&bytes).unwrap();
        assert_eq!(back.caps_ex, Some(CapsExMask::ALL));
        assert!(back.supports_reconnect());
        assert!(back.supports_multitrack());
    }

    #[test]
    fn reserved_caps_ex_bits_preserved() {
        // A future reserved bit (0x10) must survive the round-trip
        // verbatim so an updated peer can act on it.
        let obj = ConnectCommandObject::new().caps_ex(CapsExMask::RECONNECT | 0x10);
        let bytes = write_connect_command(1.0, &obj).unwrap();
        let back = parse_connect_command(&bytes).unwrap();
        assert_eq!(back.caps_ex, Some(CapsExMask::RECONNECT | 0x10));
    }

    #[test]
    fn absent_properties_stay_none() {
        let obj = ConnectCommandObject::new().string("app", "live");
        let bytes = write_connect_command(1.0, &obj).unwrap();
        let back = parse_connect_command(&bytes).unwrap();
        assert!(back.four_cc_list.is_none());
        assert!(back.video_four_cc_info_map.is_none());
        assert!(back.audio_four_cc_info_map.is_none());
        assert!(back.caps_ex.is_none());
        assert!(!back.supports_reconnect());
    }

    #[test]
    fn parse_rejects_wrong_command_name() {
        let mut bytes = Vec::new();
        amf0::write_string(&mut bytes, "onStatus").unwrap();
        amf0::write_number(&mut bytes, 1.0).unwrap();
        write_command_object(&mut bytes, &ConnectCommandObject::new()).unwrap();
        let err = parse_connect_command(&bytes).unwrap_err();
        assert!(format!("{err}").contains("connect"));
    }

    #[test]
    fn parse_ignores_non_scalar_info_map_entries() {
        // A FourCC map whose value is a String (not a Number) is dropped.
        let mut bytes = Vec::new();
        amf0::write_string(&mut bytes, COMMAND_NAME).unwrap();
        amf0::write_number(&mut bytes, 1.0).unwrap();
        amf0::write_object_start(&mut bytes).unwrap();
        amf0::write_property_name(&mut bytes, "videoFourCcInfoMap").unwrap();
        amf0::write_object_start(&mut bytes).unwrap();
        amf0::write_property_name(&mut bytes, "vp09").unwrap();
        amf0::write_number(&mut bytes, f64::from(FourCcInfoMask::CAN_DECODE)).unwrap();
        amf0::write_property_name(&mut bytes, "bogus").unwrap();
        amf0::write_string(&mut bytes, "not-a-number").unwrap();
        amf0::write_object_end(&mut bytes).unwrap();
        amf0::write_object_end(&mut bytes).unwrap();

        let back = parse_connect_command(&bytes).unwrap();
        let map = back.video_four_cc_info_map.unwrap();
        assert_eq!(map, vec![("vp09".to_string(), FourCcInfoMask::CAN_DECODE)]);
    }

    #[test]
    fn out_of_range_caps_ex_clamps_to_zero() {
        // A forged non-finite / out-of-range capsEx Number must not panic
        // the cast; it clamps to 0.
        let mut bytes = Vec::new();
        amf0::write_string(&mut bytes, COMMAND_NAME).unwrap();
        amf0::write_number(&mut bytes, 1.0).unwrap();
        amf0::write_object_start(&mut bytes).unwrap();
        amf0::write_property_name(&mut bytes, "capsEx").unwrap();
        amf0::write_number(&mut bytes, f64::INFINITY).unwrap();
        amf0::write_object_end(&mut bytes).unwrap();

        let back = parse_connect_command(&bytes).unwrap();
        assert_eq!(back.caps_ex, Some(0));
    }
}
