//! Minimal AMF0 decoder — enough to parse FLV `onMetaData` script
//! tags.
//!
//! AMF0 wire format is described in the ActionScript spec; each value
//! begins with a one-byte type marker (FLV spec Annex E.4.4.2 enumerates
//! the FLV-relevant subset):
//!
//! * `0x00` Number — 8-byte IEEE-754 big-endian double.
//! * `0x01` Boolean — one byte (0 = false, nonzero = true).
//! * `0x02` String — u16 BE length + UTF-8 bytes.
//! * `0x03` Object — (u16-length-prefixed key, value)* followed by an
//!   empty key + `0x09` object-end marker.
//! * `0x05` Null.
//! * `0x06` Undefined.
//! * `0x07` Reference — UI16 BE index into a prior-object table (E.4.4.2
//!   type 7). FLV producers in the wild rarely emit this, and we don't
//!   maintain a reference table, but the marker is preserved so an
//!   unexpected occurrence doesn't poison the whole script tag.
//! * `0x08` ECMA array — u32 BE length (hint, ignored) + same body as
//!   an Object.
//! * `0x09` Object end — only valid as a terminator.
//! * `0x0A` Strict array — u32 BE length + that many values.
//! * `0x0B` Date — 8-byte double (ms since epoch) + i16 BE timezone.
//! * `0x0C` Long string — u32 BE length + UTF-8 bytes.
//! * `0x0D` Unsupported — no payload. The spec (§2.15) lets a producer
//!   emit this marker for a value it cannot serialise; some endpoints
//!   raise an error on encountering it, others treat it as
//!   `Undefined`. We surface it as a distinct variant so callers can
//!   tell which behaviour they're seeing.
//! * `0x0F` XML Document — encoded "always" as a long UTF-8 string
//!   (u32 BE length + UTF-8 bytes per §2.17). Carries the serialised
//!   DOM body of an `XMLDocument`; we keep the raw payload so callers
//!   can pipe it into their own XML parser if they need one.
//! * `0x10` Typed Object — `class-name (UTF-8) + object-property*`
//!   per §2.18. Producers that register a class alias on a typed
//!   object emit this in place of the anonymous `0x03` object; FMS /
//!   Wowza relays do pass these through in `onMetaData` payloads. The
//!   class name is preserved alongside the property body.
//!
//! Type `0x04` MovieClip is reserved-not-supported per E.4.4.2 and
//! surfaces as [`Error::InvalidData`] — the spec explicitly bans
//! producers from emitting it.
//!
//! Type `0x0E` RecordSet is reserved-not-supported per AMF0 §2.16 (the
//! spec mirrors the MovieClip status); it surfaces as
//! [`Error::InvalidData`].
//!
//! The `0x11` AVM+ object marker (AMF3 switch, AMF0 spec §3.1) lifts
//! the decode into the AMF3 grammar; the following bytes are parsed
//! through [`crate::amf3::parse_amf3_value`] and surfaced as
//! [`AmfValue::AvmPlus`].

use std::io::Write;

use oxideav_core::{Error, Result};

use crate::amf3::{parse_amf3_value, Amf3Value};

#[derive(Clone, Debug, PartialEq)]
pub enum AmfValue {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, AmfValue)>),
    Null,
    Undefined,
    /// AMF0 Reference (marker `0x07`) — UI16 index into the implicit
    /// per-message reference table. We don't resolve references (FLV
    /// `onMetaData` payloads rarely re-use objects in practice); the
    /// index is preserved verbatim so the caller can log it or skip.
    Reference(u16),
    EcmaArray(Vec<(String, AmfValue)>),
    StrictArray(Vec<AmfValue>),
    Date {
        time_ms: f64,
        tz: i16,
    },
    /// AMF0 Unsupported (marker `0x0D`, spec §2.15). The producer used
    /// this sentinel for a value its serializer could not encode. No
    /// payload — the marker stands on its own.
    Unsupported,
    /// AMF0 XMLDocument (marker `0x0F`, spec §2.17). Carries the
    /// serialized form of an `XMLDocument` as a long UTF-8 string. The
    /// raw body is preserved so callers can feed it into their own XML
    /// parser if they need to.
    Xml(String),
    /// AMF0 Typed Object (marker `0x10`, spec §2.18). A complex value
    /// with a registered class alias. `class_name` is the alias the
    /// producer attached; `body` is the same `(key, value)*` sequence
    /// as for an anonymous Object.
    TypedObject {
        class_name: String,
        body: Vec<(String, AmfValue)>,
    },
    /// AMF0 AVM+ Object (marker `0x11`, AMF0 spec §3.1). The byte
    /// stream switches to AMF3 (a.k.a. "ActionScript 3.0 serialisation
    /// format") for exactly one value; the inner value is fully
    /// decoded via the AMF3 grammar.
    AvmPlus(Box<Amf3Value>),
}

impl AmfValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            Self::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Look up a field by name from an Object, EcmaArray, or
    /// TypedObject value; returns `None` for every other variant.
    pub fn get(&self, key: &str) -> Option<&AmfValue> {
        let body = match self {
            Self::Object(v) | Self::EcmaArray(v) => v,
            Self::TypedObject { body, .. } => body,
            _ => return None,
        };
        body.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// For a `TypedObject`, the producer-registered class alias.
    /// `None` for every other variant (including anonymous `Object`).
    pub fn class_name(&self) -> Option<&str> {
        match self {
            Self::TypedObject { class_name, .. } => Some(class_name.as_str()),
            _ => None,
        }
    }
}

/// Parse a single AMF0 value starting at `pos`. On success the new
/// position is returned so the caller can walk a sequence of values.
pub fn parse_amf0_value(data: &[u8], pos: usize) -> Result<(AmfValue, usize)> {
    let mut p = pos;
    let marker = peek_byte(data, p)?;
    p += 1;
    let value = match marker {
        0x00 => {
            // Number — 8-byte BE IEEE-754 double.
            let n = read_f64_be(data, p)?;
            p += 8;
            AmfValue::Number(n)
        }
        0x01 => {
            // Boolean.
            let b = peek_byte(data, p)?;
            p += 1;
            AmfValue::Boolean(b != 0)
        }
        0x02 => {
            // String — u16 BE length + UTF-8 bytes.
            let len = read_u16_be(data, p)? as usize;
            p += 2;
            let s = read_utf8(data, p, len)?;
            p += len;
            AmfValue::String(s)
        }
        0x03 => {
            // Anonymous object.
            let (body, np) = parse_object_body(data, p)?;
            p = np;
            AmfValue::Object(body)
        }
        0x05 => AmfValue::Null,
        0x06 => AmfValue::Undefined,
        0x07 => {
            // Reference — UI16 BE pointing into the implicit object
            // table. We don't resolve it, just surface the index so
            // callers can log + skip.
            let idx = read_u16_be(data, p)?;
            p += 2;
            AmfValue::Reference(idx)
        }
        0x08 => {
            // ECMA array — u32 BE count (hint) + object body.
            p = p
                .checked_add(4)
                .ok_or_else(|| Error::invalid("AMF0 overflow"))?;
            let (body, np) = parse_object_body(data, p)?;
            p = np;
            AmfValue::EcmaArray(body)
        }
        0x0A => {
            // Strict array — u32 BE count + values.
            let count = read_u32_be(data, p)? as usize;
            p += 4;
            let mut out = Vec::with_capacity(count.min(256));
            for _ in 0..count {
                let (v, np) = parse_amf0_value(data, p)?;
                out.push(v);
                p = np;
            }
            AmfValue::StrictArray(out)
        }
        0x0B => {
            let time_ms = read_f64_be(data, p)?;
            p += 8;
            let tz = read_i16_be(data, p)?;
            p += 2;
            AmfValue::Date { time_ms, tz }
        }
        0x0C => {
            let len = read_u32_be(data, p)? as usize;
            p += 4;
            let s = read_utf8(data, p, len)?;
            p += len;
            AmfValue::String(s)
        }
        0x0D => {
            // Unsupported — no payload, the marker stands on its own
            // (spec §2.15).
            AmfValue::Unsupported
        }
        0x0F => {
            // XMLDocument — encoded as a long UTF-8 string (u32 BE
            // length + UTF-8 bytes, spec §2.17 references the long
            // string form).
            let len = read_u32_be(data, p)? as usize;
            p += 4;
            let s = read_utf8(data, p, len)?;
            p += len;
            AmfValue::Xml(s)
        }
        0x10 => {
            // Typed Object — `UTF-8 class-name + *(object-property)`
            // (spec §2.18). The class-name is a plain u16-length-prefixed
            // UTF-8 string (the leading byte of an anonymous-object body
            // terminator does NOT apply here — that terminator only
            // belongs to the property body).
            let class_name_len = read_u16_be(data, p)? as usize;
            p += 2;
            let class_name = read_utf8(data, p, class_name_len)?;
            p += class_name_len;
            let (body, np) = parse_object_body(data, p)?;
            p = np;
            AmfValue::TypedObject { class_name, body }
        }
        0x11 => {
            // AVM+ switch marker (AMF0 spec §3.1) — the next value is
            // encoded with the AMF3 grammar.
            let (inner, np) = parse_amf3_value(data, p)?;
            p = np;
            AmfValue::AvmPlus(Box::new(inner))
        }
        other => {
            return Err(Error::invalid(format!(
                "AMF0: unsupported type marker 0x{other:02X}"
            )));
        }
    };
    Ok((value, p))
}

/// Parse the "(key, value)* end-marker" body shared by Object (`0x03`)
/// and EcmaArray (`0x08`). The terminator is a zero-length key
/// followed by the object-end marker byte `0x09`.
fn parse_object_body(data: &[u8], start: usize) -> Result<(Vec<(String, AmfValue)>, usize)> {
    let mut p = start;
    let mut out: Vec<(String, AmfValue)> = Vec::new();
    loop {
        if p + 3 > data.len() {
            return Err(Error::invalid("AMF0: truncated object body"));
        }
        let key_len = u16::from_be_bytes([data[p], data[p + 1]]) as usize;
        // Empty key + 0x09 object-end marker is the terminator.
        if key_len == 0 && data[p + 2] == 0x09 {
            return Ok((out, p + 3));
        }
        p += 2;
        let key = read_utf8(data, p, key_len)?;
        p += key_len;
        let (value, np) = parse_amf0_value(data, p)?;
        p = np;
        out.push((key, value));
    }
}

// ---- AMF0 writers ----------------------------------------------------------
//
// The minimal serialiser surface needed to emit an FLV `onMetaData`
// script tag (spec Annex E.4.4 / AMF0 §2). Each `write_*` for a typed
// value emits the one-byte type marker followed by the payload, mirroring
// the `parse_amf0_value` grammar above. Property *names* inside an object
// or ECMA array are bare length-prefixed strings with **no** type marker
// (spec SCRIPTDATASTRING, E.4.4.10) — [`write_property_name`] emits that
// form, while [`write_string`] emits a full String *value* (marker
// `0x02`).

/// Write an AMF0 Number value (marker `0x00` + 8-byte BE IEEE-754
/// double, §2.2).
pub fn write_number<W: Write + ?Sized>(w: &mut W, n: f64) -> Result<()> {
    w.write_all(&[0x00])?;
    w.write_all(&n.to_be_bytes())?;
    Ok(())
}

/// Write an AMF0 Boolean value (marker `0x01` + one byte `0`/`1`, §2.3).
pub fn write_boolean<W: Write + ?Sized>(w: &mut W, b: bool) -> Result<()> {
    w.write_all(&[0x01, u8::from(b)])?;
    Ok(())
}

/// Write an AMF0 String value (marker `0x02` + UI16 BE length + UTF-8
/// bytes, §2.4). Errors if `s` exceeds 65535 bytes — a longer payload
/// requires the Long String type (`0x0C`), which `onMetaData` property
/// values do not need.
pub fn write_string<W: Write + ?Sized>(w: &mut W, s: &str) -> Result<()> {
    w.write_all(&[0x02])?;
    write_utf8_u16(w, s)
}

/// Write a bare AMF0 property name — the UI16-length-prefixed UTF-8
/// string that precedes each value inside an Object / ECMA array body
/// (SCRIPTDATASTRING, spec E.4.4.10). No type marker.
pub fn write_property_name<W: Write + ?Sized>(w: &mut W, name: &str) -> Result<()> {
    write_utf8_u16(w, name)
}

/// Write the AMF0 anonymous-Object start marker (`0x03`, §2.5). Property
/// pairs follow (each a [`write_property_name`] + a value), terminated by
/// [`write_object_end`].
pub fn write_object_start<W: Write + ?Sized>(w: &mut W) -> Result<()> {
    w.write_all(&[0x03])?;
    Ok(())
}

/// Write the AMF0 ECMA-array start marker and its `count` hint: marker
/// `0x08` followed by the UI32 BE associative-count (§2.10). The body is
/// the same `(name, value)*` sequence as an Object, terminated by
/// [`write_object_end`]. `count` is an approximate-length hint; decoders
/// (including this crate's) ignore the exact value, but emitting the
/// true property count is the convention FLV producers follow.
pub fn write_ecma_array_start<W: Write + ?Sized>(w: &mut W, count: u32) -> Result<()> {
    w.write_all(&[0x08])?;
    w.write_all(&count.to_be_bytes())?;
    Ok(())
}

/// Write the AMF0 object-end terminator — an empty property name plus
/// the object-end marker (`0x00 0x00 0x09`, SCRIPTDATAOBJECTEND, spec
/// E.4.4.7). Closes both Object and ECMA-array bodies.
pub fn write_object_end<W: Write + ?Sized>(w: &mut W) -> Result<()> {
    w.write_all(&[0x00, 0x00, 0x09])?;
    Ok(())
}

/// Write an AMF0 Strict-array value of Numbers — marker `0x0A`,
/// UI32 BE `StrictArrayLength`, then `len(values)` Number values each
/// emitted with the standard Number marker (spec §E.4.4.9
/// SCRIPTDATASTRICTARRAY + §E.4.4.2 type 10 + Number §2.2). No
/// terminator follows the list per the spec. Convenience helper for
/// the `onMetaData.keyframes` toc, whose `filepositions[]` and
/// `times[]` are both StrictArrays of Numbers.
pub fn write_strict_array_number<W: Write + ?Sized>(w: &mut W, values: &[f64]) -> Result<()> {
    if values.len() > u32::MAX as usize {
        return Err(Error::invalid(format!(
            "AMF0: strict-array length {} exceeds UI32 max",
            values.len()
        )));
    }
    w.write_all(&[0x0A])?;
    w.write_all(&(values.len() as u32).to_be_bytes())?;
    for n in values {
        write_number(w, *n)?;
    }
    Ok(())
}

/// Shared helper: UI16 BE length + UTF-8 bytes.
fn write_utf8_u16<W: Write + ?Sized>(w: &mut W, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    if bytes.len() > u16::MAX as usize {
        return Err(Error::invalid(format!(
            "AMF0: string length {} exceeds UI16 max (use a long string)",
            bytes.len()
        )));
    }
    w.write_all(&(bytes.len() as u16).to_be_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

fn peek_byte(data: &[u8], pos: usize) -> Result<u8> {
    data.get(pos)
        .copied()
        .ok_or_else(|| Error::invalid("AMF0: truncated value"))
}

fn read_u16_be(data: &[u8], pos: usize) -> Result<u16> {
    if pos + 2 > data.len() {
        return Err(Error::invalid("AMF0: truncated u16"));
    }
    Ok(u16::from_be_bytes([data[pos], data[pos + 1]]))
}

fn read_i16_be(data: &[u8], pos: usize) -> Result<i16> {
    if pos + 2 > data.len() {
        return Err(Error::invalid("AMF0: truncated i16"));
    }
    Ok(i16::from_be_bytes([data[pos], data[pos + 1]]))
}

fn read_u32_be(data: &[u8], pos: usize) -> Result<u32> {
    if pos + 4 > data.len() {
        return Err(Error::invalid("AMF0: truncated u32"));
    }
    Ok(u32::from_be_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

fn read_f64_be(data: &[u8], pos: usize) -> Result<f64> {
    if pos + 8 > data.len() {
        return Err(Error::invalid("AMF0: truncated f64"));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[pos..pos + 8]);
    Ok(f64::from_be_bytes(b))
}

fn read_utf8(data: &[u8], pos: usize, len: usize) -> Result<String> {
    if pos.saturating_add(len) > data.len() {
        return Err(Error::invalid("AMF0: truncated string"));
    }
    String::from_utf8(data[pos..pos + len].to_vec())
        .map_err(|_| Error::invalid("AMF0: non-UTF-8 string"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number() {
        let mut bytes = vec![0x00];
        bytes.extend_from_slice(&(1234.5_f64).to_be_bytes());
        let (v, p) = parse_amf0_value(&bytes, 0).unwrap();
        assert_eq!(v, AmfValue::Number(1234.5));
        assert_eq!(p, bytes.len());
    }

    #[test]
    fn string() {
        let s = "hello";
        let mut bytes = vec![0x02];
        bytes.extend_from_slice(&(s.len() as u16).to_be_bytes());
        bytes.extend_from_slice(s.as_bytes());
        let (v, p) = parse_amf0_value(&bytes, 0).unwrap();
        assert_eq!(v, AmfValue::String(s.into()));
        assert_eq!(p, bytes.len());
    }

    #[test]
    fn object_with_number_and_string() {
        // {"a": 1.0, "b": "x"}
        let mut b = vec![0x03];
        // key "a"
        b.extend_from_slice(&[0x00, 0x01, b'a']);
        b.push(0x00);
        b.extend_from_slice(&1.0_f64.to_be_bytes());
        // key "b"
        b.extend_from_slice(&[0x00, 0x01, b'b']);
        b.push(0x02);
        b.extend_from_slice(&(1u16).to_be_bytes());
        b.push(b'x');
        // terminator
        b.extend_from_slice(&[0x00, 0x00, 0x09]);
        let (v, p) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(p, b.len());
        match v {
            AmfValue::Object(body) => {
                assert_eq!(body.len(), 2);
                assert_eq!(body[0].0, "a");
                assert_eq!(body[0].1, AmfValue::Number(1.0));
                assert_eq!(body[1].0, "b");
                assert_eq!(body[1].1, AmfValue::String("x".into()));
            }
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn rejects_unknown_marker() {
        let bytes = [0xFF];
        assert!(matches!(
            parse_amf0_value(&bytes, 0),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn reference_marker() {
        // Marker 0x07, idx 0x0102.
        let bytes = [0x07, 0x01, 0x02];
        let (v, p) = parse_amf0_value(&bytes, 0).unwrap();
        assert_eq!(v, AmfValue::Reference(0x0102));
        assert_eq!(p, 3);
    }

    #[test]
    fn long_string_via_type_0c() {
        let s = "hi";
        let mut b = vec![0x0C];
        b.extend_from_slice(&(s.len() as u32).to_be_bytes());
        b.extend_from_slice(s.as_bytes());
        let (v, p) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(v, AmfValue::String(s.into()));
        assert_eq!(p, b.len());
    }

    #[test]
    fn unsupported_marker_stands_alone() {
        // §2.15: marker 0x0D, no payload.
        let bytes = [0x0D];
        let (v, p) = parse_amf0_value(&bytes, 0).unwrap();
        assert_eq!(v, AmfValue::Unsupported);
        assert_eq!(p, 1);
    }

    #[test]
    fn xml_document_round_trips_as_long_utf8() {
        // §2.17: marker 0x0F + u32 BE length + UTF-8 bytes.
        let xml = "<x>hi</x>";
        let mut b = vec![0x0F];
        b.extend_from_slice(&(xml.len() as u32).to_be_bytes());
        b.extend_from_slice(xml.as_bytes());
        let (v, p) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(v, AmfValue::Xml(xml.into()));
        assert_eq!(p, b.len());
    }

    #[test]
    fn typed_object_carries_class_name_and_body() {
        // §2.18: marker 0x10 + UTF-8 class name + (UTF-8 key, value)*
        // + terminator 0x00 0x00 0x09.
        // Build: TypedObject("Foo", { "a": 1.0 })
        let class = "Foo";
        let mut b = vec![0x10];
        b.extend_from_slice(&(class.len() as u16).to_be_bytes());
        b.extend_from_slice(class.as_bytes());
        // property "a" -> Number(1.0)
        b.extend_from_slice(&[0x00, 0x01, b'a']);
        b.push(0x00);
        b.extend_from_slice(&1.0_f64.to_be_bytes());
        // terminator
        b.extend_from_slice(&[0x00, 0x00, 0x09]);
        let (v, p) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(p, b.len());
        match &v {
            AmfValue::TypedObject { class_name, body } => {
                assert_eq!(class_name, "Foo");
                assert_eq!(body.len(), 1);
                assert_eq!(body[0].0, "a");
                assert_eq!(body[0].1, AmfValue::Number(1.0));
            }
            other => panic!("expected typed object, got {other:?}"),
        }
        // `get` looks into TypedObject bodies, and `class_name` exposes
        // the alias — both are part of the lookup contract.
        assert_eq!(v.get("a"), Some(&AmfValue::Number(1.0)));
        assert_eq!(v.class_name(), Some("Foo"));
    }

    #[test]
    fn typed_object_with_empty_class_name() {
        // §2.18 allows a zero-length class name (anonymous typed
        // object — degenerates to an object whose alias is "").
        let mut b = vec![0x10, 0x00, 0x00];
        b.extend_from_slice(&[0x00, 0x00, 0x09]);
        let (v, p) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(p, b.len());
        match v {
            AmfValue::TypedObject { class_name, body } => {
                assert_eq!(class_name, "");
                assert!(body.is_empty());
            }
            other => panic!("expected typed object, got {other:?}"),
        }
    }

    #[test]
    fn amf3_switch_marker_decodes_inner_value() {
        // §3.1 AVM+ object marker 0x11: the next byte is an AMF3
        // marker. `0x03` = AMF3 true-marker, which carries no body.
        let bytes = [0x11, 0x03];
        let (v, p) = parse_amf0_value(&bytes, 0).unwrap();
        assert_eq!(p, 2);
        match v {
            AmfValue::AvmPlus(inner) => {
                assert_eq!(*inner, Amf3Value::Boolean(true));
            }
            other => panic!("expected AvmPlus, got {other:?}"),
        }
    }

    #[test]
    fn amf3_switch_marker_with_truncated_inner_errors() {
        // 0x11 by itself: the next byte should be an AMF3 marker but
        // the input ends.
        let bytes = [0x11];
        assert!(matches!(
            parse_amf0_value(&bytes, 0),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn write_number_round_trips_through_parse() {
        let mut b = Vec::new();
        write_number(&mut b, 1234.5).unwrap();
        let (v, p) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(v, AmfValue::Number(1234.5));
        assert_eq!(p, b.len());
    }

    #[test]
    fn write_boolean_round_trips() {
        for tv in [true, false] {
            let mut b = Vec::new();
            write_boolean(&mut b, tv).unwrap();
            let (v, _) = parse_amf0_value(&b, 0).unwrap();
            assert_eq!(v, AmfValue::Boolean(tv));
        }
    }

    #[test]
    fn write_string_round_trips() {
        let mut b = Vec::new();
        write_string(&mut b, "onMetaData").unwrap();
        // marker 0x02 + u16 len(10) + bytes.
        assert_eq!(b[0], 0x02);
        assert_eq!(u16::from_be_bytes([b[1], b[2]]), 10);
        let (v, p) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(v, AmfValue::String("onMetaData".into()));
        assert_eq!(p, b.len());
    }

    #[test]
    fn write_ecma_array_round_trips_with_object_parser() {
        // Emit {"a": 1.0, "ok": true, "s": "x"} as an ECMA array and
        // parse it back via the value parser.
        let mut b = Vec::new();
        write_ecma_array_start(&mut b, 3).unwrap();
        write_property_name(&mut b, "a").unwrap();
        write_number(&mut b, 1.0).unwrap();
        write_property_name(&mut b, "ok").unwrap();
        write_boolean(&mut b, true).unwrap();
        write_property_name(&mut b, "s").unwrap();
        write_string(&mut b, "x").unwrap();
        write_object_end(&mut b).unwrap();
        let (v, p) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(p, b.len());
        match v {
            AmfValue::EcmaArray(body) => {
                assert_eq!(body.len(), 3);
                assert_eq!(body[0], ("a".into(), AmfValue::Number(1.0)));
                assert_eq!(body[1], ("ok".into(), AmfValue::Boolean(true)));
                assert_eq!(body[2], ("s".into(), AmfValue::String("x".into())));
            }
            other => panic!("expected ecma array, got {other:?}"),
        }
    }

    #[test]
    fn write_object_round_trips_with_parser() {
        let mut b = Vec::new();
        write_object_start(&mut b).unwrap();
        write_property_name(&mut b, "k").unwrap();
        write_number(&mut b, 7.0).unwrap();
        write_object_end(&mut b).unwrap();
        let (v, _) = parse_amf0_value(&b, 0).unwrap();
        assert_eq!(v.get("k"), Some(&AmfValue::Number(7.0)));
    }

    #[test]
    fn write_string_rejects_oversize() {
        let big = "x".repeat(70_000);
        let mut b = Vec::new();
        assert!(matches!(
            write_string(&mut b, &big),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn strict_array_holds_ordered_values() {
        // [1.0, "x"]
        let mut b = vec![0x0A];
        b.extend_from_slice(&(2u32).to_be_bytes());
        b.push(0x00);
        b.extend_from_slice(&1.0_f64.to_be_bytes());
        b.push(0x02);
        b.extend_from_slice(&(1u16).to_be_bytes());
        b.push(b'x');
        let (v, _) = parse_amf0_value(&b, 0).unwrap();
        match v {
            AmfValue::StrictArray(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], AmfValue::Number(1.0));
                assert_eq!(items[1], AmfValue::String("x".into()));
            }
            other => panic!("expected strict array, got {other:?}"),
        }
    }
}
