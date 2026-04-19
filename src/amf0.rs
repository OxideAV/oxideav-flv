//! Minimal AMF0 decoder — enough to parse FLV `onMetaData` script
//! tags.
//!
//! AMF0 wire format is described in the ActionScript spec; each value
//! begins with a one-byte type marker:
//!
//! * `0x00` Number — 8-byte IEEE-754 big-endian double.
//! * `0x01` Boolean — one byte (0 = false, nonzero = true).
//! * `0x02` String — u16 BE length + UTF-8 bytes.
//! * `0x03` Object — (u16-length-prefixed key, value)* followed by an
//!   empty key + `0x09` object-end marker.
//! * `0x05` Null.
//! * `0x06` Undefined.
//! * `0x08` ECMA array — u32 BE length (hint, ignored) + same body as
//!   an Object.
//! * `0x09` Object end — only valid as a terminator.
//! * `0x0A` Strict array — u32 BE length + that many values.
//! * `0x0B` Date — 8-byte double (ms since epoch) + i16 BE timezone.
//! * `0x0C` Long string — u32 BE length + UTF-8 bytes.
//!
//! Types not listed above (`MovieClip`, `TypedObject`, `XML Document`,
//! `Reference`, …) are not expected inside `onMetaData` — they surface
//! as [`Error::InvalidData`] so callers can log the anomaly rather than
//! silently drop metadata.

use oxideav_core::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum AmfValue {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, AmfValue)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, AmfValue)>),
    StrictArray(Vec<AmfValue>),
    Date { time_ms: f64, tz: i16 },
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

    /// Look up a field by name from an Object or EcmaArray value;
    /// returns `None` for every other variant.
    pub fn get(&self, key: &str) -> Option<&AmfValue> {
        let body = match self {
            Self::Object(v) | Self::EcmaArray(v) => v,
            _ => return None,
        };
        body.iter().find(|(k, _)| k == key).map(|(_, v)| v)
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
}
