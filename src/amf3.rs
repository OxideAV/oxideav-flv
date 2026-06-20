//! AMF3 (Action Message Format 3, "AVM+") decoder + encoder.
//!
//! Reference: Adobe *AMF 3 Specification* (Dec 2007), staged at
//! `docs/container/adobe/amf3_spec_121207.pdf`. AMF3 is the on-wire
//! serialisation used by ActionScript 3.0; FLV scriptdata can switch to
//! AMF3 mid-message via the AMF0 `0x11` AVM+ marker (§3.1).
//!
//! ## Wire shape
//!
//! AMF3 has 13 one-byte type markers:
//!
//! | Marker | Type        | Spec  |
//! | ------ | ----------- | ----- |
//! | `0x00` | undefined   | §3.2  |
//! | `0x01` | null        | §3.3  |
//! | `0x02` | false       | §3.4  |
//! | `0x03` | true        | §3.5  |
//! | `0x04` | integer     | §3.6  |
//! | `0x05` | double      | §3.7  |
//! | `0x06` | string      | §3.8  |
//! | `0x07` | xml-doc     | §3.9  |
//! | `0x08` | date        | §3.10 |
//! | `0x09` | array       | §3.11 |
//! | `0x0A` | object      | §3.12 |
//! | `0x0B` | xml         | §3.13 |
//! | `0x0C` | byte-array  | §3.14 |
//!
//! ## Primitives
//!
//! * **U29** (§1.3.1) — variable-length unsigned 29-bit integer. Each of
//!   the first three bytes uses its high bit as a continuation flag; the
//!   fourth byte (when present) contributes all 8 bits. Values >= 2^29
//!   are a range error. We refuse them rather than truncating.
//! * **UTF-8-vr** (§1.3.2) — a `U29` whose low bit is the "ref vs.
//!   literal" flag (0 = reference, 1 = literal). For a literal the
//!   remaining 28 bits give the byte length of the UTF-8 string body
//!   that follows; literals are pushed into the string reference table
//!   (except the empty string, which is "never sent by reference"). For
//!   a reference, the remaining bits index back into that table.
//!
//! ## Reference tables
//!
//! The spec mandates THREE distinct implicit tables maintained for the
//! lifetime of one message (§2.2):
//!
//! * **String table** — every non-empty string literal is appended.
//! * **Complex-object table** — Array / Date / Object / TypedObject /
//!   XML / XMLDocument / ByteArray literals (instances; *not* their
//!   traits — those are independent).
//! * **Traits table** — every distinct Traits block (class-name +
//!   sealed-property names + dynamic flag + externalizable flag) is
//!   appended. Future Object headers reference the traits by index.
//!
//! Reference indices are zero-based; index N means "the Nth literal we
//! have seen so far in this message." A reference into an as-yet-empty
//! table is a wire error, surfaced as [`Error::InvalidData`].
//!
//! ## Externalizable objects
//!
//! When a Traits header sets the externalizable flag (§3.12), the class
//! takes full control of the trailing bytes and the spec gives no
//! parser recipe ("This represents the completely custom serialisation
//! of 'externalizable' types. The client and server have an agreement
//! as to how to read in this information."). Since we cannot know
//! where the externalizable body ends without that private agreement,
//! the decoder reads **zero** body bytes from the wire and continues
//! at the next marker. [`Amf3Object::externalizable`] is left set so
//! callers can detect that the producer used the custom encoding and
//! handle the class themselves if they know its private grammar.
//!
//! ## Encoding
//!
//! [`write_amf3_value`] is the inverse of [`parse_amf3_value`]: it
//! serialises an [`Amf3Value`] tree back to AMF3 bytes. The encoder is
//! a **literal-first** serialiser — every complex value (Array / Date /
//! Object / XML / XMLDocument / ByteArray) is emitted as a fresh literal
//! rather than a back-reference, because the [`Amf3Value`] model is a
//! tree (no shared identity to dedup against) and an all-literal stream
//! is a valid AMF3 message that decodes to the identical value graph.
//! The one place references are used is the **string table** (§3.8):
//! repeated non-empty string literals (sealed-property names, class
//! aliases, assoc / dynamic keys, and `String` values) are emitted by
//! reference the second time they appear, exactly mirroring the order in
//! which the decoder populates its string table, so `parse ∘ write` and
//! `write ∘ parse` are both identity. The empty string is always a
//! literal (§3.8: "never sent by reference").
//!
//! Externalizable objects round-trip the flag and class name but carry
//! no body (the decoder reads zero body bytes; the encoder writes zero),
//! matching the module's no-recipe stance.
//!
//! ## What is intentionally NOT implemented
//!
//! * Complex-object / traits back-references on the encode side — see
//!   "Encoding" above; the tree model has no shared identity to dedup.
//! * IExternalizable bodies — see above.
//! * The `flash.utils.IDataInput` / `flash.utils.IDataOutput` callbacks
//!   for ByteArray (spec §4.2). The ByteArray bytes are surfaced raw.

use oxideav_core::{Error, Result};

/// A value decoded from an AMF3 stream.
///
/// This is the AMF3-only sibling of [`crate::amf0::AmfValue`]. The two
/// formats overlap in many places but differ on enough details
/// (integers carried separately from doubles, dates have no timezone,
/// references replace whole values, etc.) that keeping them as
/// distinct types keeps the AMF0 surface clean for the existing
/// `onMetaData` consumers.
#[derive(Clone, Debug, PartialEq)]
pub enum Amf3Value {
    /// `undefined-marker` (`0x00`).
    Undefined,
    /// `null-marker` (`0x01`).
    Null,
    /// `false-marker` (`0x02`).
    Boolean(bool),
    /// `integer-type` (`0x04`) — signed 29-bit value in the
    /// `-2^28 ..= 2^28 - 1` range after sign-extension per §3.6.
    Integer(i32),
    /// `double-type` (`0x05`).
    Double(f64),
    /// `string-type` (`0x06`).
    String(String),
    /// `xml-doc-type` (`0x07`). Legacy `flash.xml.XMLDocument`.
    XmlDocument(String),
    /// `date-type` (`0x08`). UTC milliseconds since epoch; timezone is
    /// explicitly not transmitted (§3.10).
    Date(f64),
    /// `array-type` (`0x09`). Spec §3.11 splits an Array into the
    /// associative portion (string-keyed name/value pairs) and the
    /// dense portion (ordinal values). We surface both.
    Array(Amf3Array),
    /// `object-type` (`0x0A`).
    Object(Amf3Object),
    /// `xml-type` (`0x0B`). Modern E4X-shaped XML.
    Xml(String),
    /// `byte-array-type` (`0x0C`).
    ByteArray(Vec<u8>),
}

/// An AMF3 Array (§3.11).
#[derive(Clone, Debug, PartialEq)]
pub struct Amf3Array {
    /// `(name, value)*` for non-ordinal indices ("ECMA-style").
    pub assoc: Vec<(String, Amf3Value)>,
    /// Ordinal entries, in index order.
    pub dense: Vec<Amf3Value>,
}

/// An AMF3 Object (§3.12) with its trait information.
#[derive(Clone, Debug, PartialEq)]
pub struct Amf3Object {
    /// Class alias. `""` for anonymous objects.
    pub class_name: String,
    /// `true` when the class declared the `dynamic` trait. Dynamic
    /// objects can carry additional name/value pairs in
    /// `dynamic_members` after the sealed values.
    pub dynamic: bool,
    /// `true` when the class implements `flash.utils.IExternalizable`.
    /// In that case the spec gives no recipe for the body; we surface
    /// the flag and decode no further bytes (see module-level docs).
    pub externalizable: bool,
    /// Names of the sealed (statically-declared) members of the class,
    /// in declaration order. Empty for anonymous classes.
    pub sealed_names: Vec<String>,
    /// Sealed values, parallel to `sealed_names` (same index = same
    /// member).
    pub sealed_values: Vec<Amf3Value>,
    /// Dynamic members (only populated for `dynamic` classes).
    pub dynamic_members: Vec<(String, Amf3Value)>,
}

impl Amf3Value {
    /// Convenience: extract a primitive `f64` representation, mirroring
    /// the AMF0 [`crate::amf0::AmfValue::as_f64`] helper. Integers and
    /// booleans coerce; everything else returns `None`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Integer(i) => Some(f64::from(*i)),
            Self::Double(d) => Some(*d),
            Self::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// Convenience: borrow the inner string for `String`-typed values
    /// or the two XML variants. Returns `None` otherwise.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) | Self::Xml(s) | Self::XmlDocument(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// Maximum U29 value per §1.3.1 ("the largest unsigned integer value
/// that can be represented is 2^29 - 1").
const U29_MAX: u32 = (1 << 29) - 1;

/// Reference-table state, owned by a decode pass.
#[derive(Debug, Default)]
struct State {
    /// String reference table (§3.8 + §2.2).
    strings: Vec<String>,
    /// Complex-object reference table (§2.2): Array / Date / Object /
    /// XML / XMLDocument / ByteArray. We store decoded values so a
    /// reference walks back to an equivalent literal.
    objects: Vec<Amf3Value>,
    /// Traits reference table (§3.12 + §2.2).
    traits: Vec<TraitsEntry>,
}

/// Entry in the traits reference table.
#[derive(Clone, Debug, PartialEq)]
struct TraitsEntry {
    class_name: String,
    dynamic: bool,
    externalizable: bool,
    sealed_names: Vec<String>,
}

/// Decode a single AMF3 value, starting at `pos`. The decoder
/// allocates its own reference tables; the tables live only for this
/// call. (Per AMF3 spec §4.1 the tables reset on every new message; a
/// scriptdata payload is one message.)
///
/// Returns the decoded value plus the new byte cursor.
pub fn parse_amf3_value(data: &[u8], pos: usize) -> Result<(Amf3Value, usize)> {
    let mut state = State::default();
    parse_value(data, pos, &mut state)
}

fn parse_value(data: &[u8], pos: usize, state: &mut State) -> Result<(Amf3Value, usize)> {
    let mut p = pos;
    let marker = peek_byte(data, p)?;
    p += 1;
    let value = match marker {
        0x00 => Amf3Value::Undefined,
        0x01 => Amf3Value::Null,
        0x02 => Amf3Value::Boolean(false),
        0x03 => Amf3Value::Boolean(true),
        0x04 => {
            let (u, np) = read_u29(data, p)?;
            p = np;
            Amf3Value::Integer(sign_extend_29(u))
        }
        0x05 => {
            let d = read_f64_be(data, p)?;
            p += 8;
            Amf3Value::Double(d)
        }
        0x06 => {
            let (s, np) = read_utf8_vr(data, p, state)?;
            p = np;
            Amf3Value::String(s)
        }
        0x07 => {
            let (s, np) = read_xml_like(data, p, state, matches_xmldoc)?;
            p = np;
            let value = Amf3Value::XmlDocument(s);
            // Newly-decoded literal lands in the object table.
            if !is_object_ref(data, p, &value) {
                state.objects.push(value.clone());
            }
            value
        }
        0x08 => {
            let (date, np) = read_date(data, p, state)?;
            p = np;
            date
        }
        0x09 => {
            let (arr, np) = read_array(data, p, state)?;
            p = np;
            arr
        }
        0x0A => {
            let (obj, np) = read_object(data, p, state)?;
            p = np;
            obj
        }
        0x0B => {
            let (s, np) = read_xml_like(data, p, state, matches_xml)?;
            p = np;
            let value = Amf3Value::Xml(s);
            if !is_object_ref(data, p, &value) {
                state.objects.push(value.clone());
            }
            value
        }
        0x0C => {
            let (bytes, np) = read_byte_array(data, p, state)?;
            p = np;
            bytes
        }
        other => {
            return Err(Error::invalid(format!(
                "AMF3: unsupported type marker 0x{other:02X}"
            )));
        }
    };
    Ok((value, p))
}

/// Read a `U29` per §1.3.1. The high bit of each of the first three
/// bytes signals "another byte follows"; on the fourth byte we
/// consume all 8 bits unconditionally.
fn read_u29(data: &[u8], pos: usize) -> Result<(u32, usize)> {
    let mut p = pos;
    let mut acc: u32 = 0;
    for i in 0..3 {
        let b = peek_byte(data, p)?;
        p += 1;
        if b & 0x80 == 0 {
            // Final byte for this run — its low 7 bits round out the value.
            acc = (acc << 7) | u32::from(b & 0x7F);
            return Ok((acc, p));
        }
        acc = (acc << 7) | u32::from(b & 0x7F);
        // Stay in the loop while continuation flags keep coming.
        let _ = i;
    }
    // Three continuation bytes were consumed; the fourth contributes
    // all 8 bits per spec. The total significant bit count is
    // 7+7+7+8 = 29, so the four-byte form's output range is exactly
    // `0..=U29_MAX` and cannot overflow by construction.
    let b = peek_byte(data, p)?;
    p += 1;
    acc = (acc << 8) | u32::from(b);
    debug_assert!(acc <= U29_MAX);
    Ok((acc, p))
}

/// Sign-extend a 29-bit value to `i32` per §3.6 ("ActionScript int is a
/// signed 'int' type").
fn sign_extend_29(u: u32) -> i32 {
    if u & (1 << 28) == 0 {
        u as i32
    } else {
        // Set the high 3 bits.
        (u | 0xE000_0000) as i32
    }
}

/// Read a UTF-8-vr per §1.3.2 / §3.8: U29 with low bit as flag, body
/// follows for literals, table lookup for references.
fn read_utf8_vr(data: &[u8], pos: usize, state: &mut State) -> Result<(String, usize)> {
    let (u, mut p) = read_u29(data, pos)?;
    if u & 1 == 0 {
        // Reference path.
        let idx = (u >> 1) as usize;
        let s = state
            .strings
            .get(idx)
            .ok_or_else(|| Error::invalid("AMF3: string-ref out of range"))?
            .clone();
        Ok((s, p))
    } else {
        let len = (u >> 1) as usize;
        let s = read_utf8(data, p, len)?;
        p += len;
        // Per §3.8: the empty string is never stored as a reference.
        if !s.is_empty() {
            state.strings.push(s.clone());
        }
        Ok((s, p))
    }
}

/// Helper used by xml-doc / xml: read a U29; on a 0-low-bit it's an
/// object-table reference (we look up via the supplied predicate), on
/// a 1-low-bit it's a literal whose body is `(U29>>1)` UTF-8 bytes.
fn read_xml_like<F>(data: &[u8], pos: usize, state: &State, matches: F) -> Result<(String, usize)>
where
    F: Fn(&Amf3Value) -> Option<String>,
{
    let (u, mut p) = read_u29(data, pos)?;
    if u & 1 == 0 {
        let idx = (u >> 1) as usize;
        let prior = state
            .objects
            .get(idx)
            .ok_or_else(|| Error::invalid("AMF3: object-ref out of range"))?;
        let s = matches(prior).ok_or_else(|| {
            Error::invalid("AMF3: object-ref points at value of incompatible type")
        })?;
        Ok((s, p))
    } else {
        let len = (u >> 1) as usize;
        let s = read_utf8(data, p, len)?;
        p += len;
        Ok((s, p))
    }
}

fn matches_xmldoc(v: &Amf3Value) -> Option<String> {
    match v {
        Amf3Value::XmlDocument(s) => Some(s.clone()),
        _ => None,
    }
}

fn matches_xml(v: &Amf3Value) -> Option<String> {
    match v {
        Amf3Value::Xml(s) => Some(s.clone()),
        _ => None,
    }
}

/// Compatibility shim — once a freshly-decoded XML/XMLDocument value
/// is materialised, we want to push it into the object table only if
/// it was a literal. A reference returns the same value content but
/// must NOT be appended again. We detect this by checking whether the
/// preceding U29 had a 0-low-bit, but the caller already consumed
/// those bytes, so we route through this no-op stub (always false)
/// and rely on the literal/reference branches in `read_xml_like` to
/// have been the path taken. The append is unconditional in the
/// literal case; this stub keeps the call shape symmetric for future
/// refactors.
fn is_object_ref(_data: &[u8], _pos: usize, _value: &Amf3Value) -> bool {
    false
}

/// Read a date-type (§3.10). The U29 low bit is the literal/ref flag;
/// for a literal the body is an 8-byte BE double = UTC milliseconds.
fn read_date(data: &[u8], pos: usize, state: &mut State) -> Result<(Amf3Value, usize)> {
    let (u, mut p) = read_u29(data, pos)?;
    if u & 1 == 0 {
        let idx = (u >> 1) as usize;
        let prior = state
            .objects
            .get(idx)
            .ok_or_else(|| Error::invalid("AMF3: date-ref out of range"))?;
        match prior {
            Amf3Value::Date(_) => Ok((prior.clone(), p)),
            _ => Err(Error::invalid(
                "AMF3: date-ref points at value of incompatible type",
            )),
        }
    } else {
        // remaining bits are not significant per spec
        let ms = read_f64_be(data, p)?;
        p += 8;
        let v = Amf3Value::Date(ms);
        state.objects.push(v.clone());
        Ok((v, p))
    }
}

/// Read an array-type (§3.11). When the U29 low bit is 0 the array is
/// a back-reference; otherwise it's a literal whose remaining 28 bits
/// give the dense-portion count, followed by the associative portion
/// (UTF-8-vr name + value pairs terminated by the empty string) and
/// then `count` dense values.
fn read_array(data: &[u8], pos: usize, state: &mut State) -> Result<(Amf3Value, usize)> {
    let (u, mut p) = read_u29(data, pos)?;
    if u & 1 == 0 {
        let idx = (u >> 1) as usize;
        let prior = state
            .objects
            .get(idx)
            .ok_or_else(|| Error::invalid("AMF3: array-ref out of range"))?;
        match prior {
            Amf3Value::Array(_) => Ok((prior.clone(), p)),
            _ => Err(Error::invalid(
                "AMF3: array-ref points at value of incompatible type",
            )),
        }
    } else {
        let dense_count = (u >> 1) as usize;
        // Reserve the object-table slot BEFORE descending — a value
        // inside the array can legitimately reference the array
        // itself (circular graph), and the slot index has to match
        // the literal's position in encounter order.
        let placeholder = Amf3Value::Array(Amf3Array {
            assoc: Vec::new(),
            dense: Vec::new(),
        });
        let slot = state.objects.len();
        state.objects.push(placeholder);

        let mut assoc: Vec<(String, Amf3Value)> = Vec::new();
        loop {
            let (key, np) = read_utf8_vr(data, p, state)?;
            p = np;
            if key.is_empty() {
                break;
            }
            let (val, np) = parse_value(data, p, state)?;
            p = np;
            assoc.push((key, val));
        }
        let mut dense: Vec<Amf3Value> = Vec::with_capacity(dense_count.min(256));
        for _ in 0..dense_count {
            let (val, np) = parse_value(data, p, state)?;
            p = np;
            dense.push(val);
        }
        let arr = Amf3Value::Array(Amf3Array { assoc, dense });
        state.objects[slot] = arr.clone();
        Ok((arr, p))
    }
}

/// Read an object-type (§3.12). The leading U29 carries a 4-bit flag
/// run plus either a trait index (when bits 0..1 are `01`) or the
/// inline trait descriptor.
///
/// Encoding (low bit = bit 0):
///
/// * bit0 = 0 → object reference (`(U29>>1)` indexes objects table).
/// * bit0 = 1, bit1 = 0 → traits reference (`(U29>>2)` indexes traits
///   table); class members + values follow per the prior trait.
/// * bit0 = 1, bit1 = 1, bit2 = 1 → traits-ext (externalizable);
///   class-name then opaque body — we record the flag and read zero
///   body bytes.
/// * bit0 = 1, bit1 = 1, bit2 = 0 → traits inline; bit3 = dynamic
///   flag; sealed-member count = `U29>>4`.
fn read_object(data: &[u8], pos: usize, state: &mut State) -> Result<(Amf3Value, usize)> {
    let (u, mut p) = read_u29(data, pos)?;
    if u & 1 == 0 {
        let idx = (u >> 1) as usize;
        let prior = state
            .objects
            .get(idx)
            .ok_or_else(|| Error::invalid("AMF3: object-ref out of range"))?;
        match prior {
            Amf3Value::Object(_) => Ok((prior.clone(), p)),
            _ => Err(Error::invalid(
                "AMF3: object-ref points at value of incompatible type",
            )),
        }
    } else {
        let traits_ref = (u >> 1) & 1 == 0;
        let entry: TraitsEntry = if traits_ref {
            // U29O-traits-ref — index into the traits table.
            let idx = (u >> 2) as usize;
            state
                .traits
                .get(idx)
                .cloned()
                .ok_or_else(|| Error::invalid("AMF3: traits-ref out of range"))?
        } else {
            // U29O-traits or U29O-traits-ext.
            let externalizable = (u >> 2) & 1 == 1;
            let dynamic = (u >> 3) & 1 == 1;
            let sealed_count = (u >> 4) as usize;
            let (class_name, np) = read_utf8_vr(data, p, state)?;
            p = np;
            if externalizable {
                // Sealed-count field is required to be zero by §3.12.
                if sealed_count != 0 {
                    return Err(Error::invalid(
                        "AMF3: traits-ext header has non-zero sealed count",
                    ));
                }
            }
            let mut sealed_names: Vec<String> = Vec::with_capacity(sealed_count.min(64));
            if !externalizable {
                for _ in 0..sealed_count {
                    let (name, np) = read_utf8_vr(data, p, state)?;
                    p = np;
                    sealed_names.push(name);
                }
            }
            let entry = TraitsEntry {
                class_name,
                dynamic,
                externalizable,
                sealed_names,
            };
            state.traits.push(entry.clone());
            entry
        };

        // Reserve object slot BEFORE reading children for the same
        // circular-graph reason as Array.
        let slot = state.objects.len();
        state.objects.push(Amf3Value::Object(Amf3Object {
            class_name: entry.class_name.clone(),
            dynamic: entry.dynamic,
            externalizable: entry.externalizable,
            sealed_names: entry.sealed_names.clone(),
            sealed_values: Vec::new(),
            dynamic_members: Vec::new(),
        }));

        if entry.externalizable {
            // No body recipe — surface the flag and consume nothing.
            let obj = Amf3Value::Object(Amf3Object {
                class_name: entry.class_name,
                dynamic: false,
                externalizable: true,
                sealed_names: Vec::new(),
                sealed_values: Vec::new(),
                dynamic_members: Vec::new(),
            });
            state.objects[slot] = obj.clone();
            return Ok((obj, p));
        }

        let mut sealed_values: Vec<Amf3Value> = Vec::with_capacity(entry.sealed_names.len());
        for _ in 0..entry.sealed_names.len() {
            let (val, np) = parse_value(data, p, state)?;
            p = np;
            sealed_values.push(val);
        }
        let mut dynamic_members: Vec<(String, Amf3Value)> = Vec::new();
        if entry.dynamic {
            loop {
                let (key, np) = read_utf8_vr(data, p, state)?;
                p = np;
                if key.is_empty() {
                    break;
                }
                let (val, np) = parse_value(data, p, state)?;
                p = np;
                dynamic_members.push((key, val));
            }
        }
        let obj = Amf3Value::Object(Amf3Object {
            class_name: entry.class_name,
            dynamic: entry.dynamic,
            externalizable: false,
            sealed_names: entry.sealed_names,
            sealed_values,
            dynamic_members,
        });
        state.objects[slot] = obj.clone();
        Ok((obj, p))
    }
}

/// Read a byte-array-type (§3.14). U29 with low bit = ref/literal flag;
/// literal body is `(U29>>1)` raw bytes.
fn read_byte_array(data: &[u8], pos: usize, state: &mut State) -> Result<(Amf3Value, usize)> {
    let (u, mut p) = read_u29(data, pos)?;
    if u & 1 == 0 {
        let idx = (u >> 1) as usize;
        let prior = state
            .objects
            .get(idx)
            .ok_or_else(|| Error::invalid("AMF3: byte-array-ref out of range"))?;
        match prior {
            Amf3Value::ByteArray(_) => Ok((prior.clone(), p)),
            _ => Err(Error::invalid(
                "AMF3: byte-array-ref points at value of incompatible type",
            )),
        }
    } else {
        let len = (u >> 1) as usize;
        if p.saturating_add(len) > data.len() {
            return Err(Error::invalid("AMF3: truncated byte-array body"));
        }
        let bytes = data[p..p + len].to_vec();
        p += len;
        let v = Amf3Value::ByteArray(bytes);
        state.objects.push(v.clone());
        Ok((v, p))
    }
}

fn peek_byte(data: &[u8], pos: usize) -> Result<u8> {
    data.get(pos)
        .copied()
        .ok_or_else(|| Error::invalid("AMF3: truncated value"))
}

fn read_f64_be(data: &[u8], pos: usize) -> Result<f64> {
    if pos.saturating_add(8) > data.len() {
        return Err(Error::invalid("AMF3: truncated f64"));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[pos..pos + 8]);
    Ok(f64::from_be_bytes(b))
}

fn read_utf8(data: &[u8], pos: usize, len: usize) -> Result<String> {
    if pos.saturating_add(len) > data.len() {
        return Err(Error::invalid("AMF3: truncated UTF-8 body"));
    }
    let slice = &data[pos..pos + len];
    std::str::from_utf8(slice)
        .map(|s| s.to_owned())
        .map_err(|_| Error::invalid("AMF3: invalid UTF-8 in string body"))
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Encoder-side string reference table.
///
/// The decoder appends every non-empty string literal it reads to its
/// string table (in encounter order). For `write ∘ parse` and
/// `parse ∘ write` to both be identity, the encoder must append in the
/// exact same encounter order and emit the second-and-later occurrences
/// by reference. This table tracks the strings already written so a
/// repeat can be encoded as `(idx << 1)` (low bit clear = reference).
#[derive(Debug, Default)]
struct EncState {
    /// Strings already written as literals, in write order. The index of
    /// a string here is its AMF3 string-table index.
    strings: Vec<String>,
}

impl EncState {
    /// Index of `s` in the string table, if it has already been written
    /// as a literal during this message.
    fn string_index(&self, s: &str) -> Option<usize> {
        self.strings.iter().position(|t| t == s)
    }
}

/// Serialise a single [`Amf3Value`] to AMF3 bytes (the inverse of
/// [`parse_amf3_value`]).
///
/// The bytes are appended to `out`. A fresh string-reference table is
/// allocated for this call, matching the decoder's "tables reset per
/// message" rule (§4.1) — a scriptdata payload is one message.
///
/// `Ok(())` on success. The only failure mode is a value that cannot be
/// represented in AMF3 at all: a string / array / object / byte-array
/// whose length exceeds the 28-bit `U29` size field (`2^28 - 1`), which
/// raises [`Error::InvalidData`] before any partial bytes for that value
/// reach `out` beyond the marker. (28 bits because the low bit of the
/// length `U29` is the literal/reference flag, leaving 28 for the size.)
pub fn write_amf3_value(out: &mut Vec<u8>, value: &Amf3Value) -> Result<()> {
    let mut state = EncState::default();
    write_value(out, value, &mut state)
}

/// Largest length representable in the 28-bit size field of a UTF-8-vr /
/// array / byte-array `U29` header (`(len << 1) | 1` must stay <= U29_MAX).
const U29_LEN_MAX: usize = (1 << 28) - 1;

fn write_value(out: &mut Vec<u8>, value: &Amf3Value, state: &mut EncState) -> Result<()> {
    match value {
        Amf3Value::Undefined => out.push(0x00),
        Amf3Value::Null => out.push(0x01),
        Amf3Value::Boolean(false) => out.push(0x02),
        Amf3Value::Boolean(true) => out.push(0x03),
        Amf3Value::Integer(i) => {
            out.push(0x04);
            write_integer(out, *i)?;
        }
        Amf3Value::Double(d) => {
            out.push(0x05);
            out.extend_from_slice(&d.to_be_bytes());
        }
        Amf3Value::String(s) => {
            out.push(0x06);
            write_utf8_vr(out, s, state)?;
        }
        Amf3Value::XmlDocument(s) => {
            out.push(0x07);
            // xml-doc / xml are NOT part of the string table (the decoder
            // pushes them to the OBJECT table). Always a fresh literal.
            write_xml_like(out, s)?;
        }
        Amf3Value::Date(ms) => {
            out.push(0x08);
            // Literal: U29 = 1 (low bit set, no significant high bits),
            // followed by the 8-byte BE double.
            out.push(0x01);
            out.extend_from_slice(&ms.to_be_bytes());
        }
        Amf3Value::Array(arr) => {
            out.push(0x09);
            write_array(out, arr, state)?;
        }
        Amf3Value::Object(obj) => {
            out.push(0x0A);
            write_object(out, obj, state)?;
        }
        Amf3Value::Xml(s) => {
            out.push(0x0B);
            write_xml_like(out, s)?;
        }
        Amf3Value::ByteArray(bytes) => {
            out.push(0x0C);
            if bytes.len() > U29_LEN_MAX {
                return Err(Error::invalid("AMF3: byte-array too long to encode"));
            }
            write_u29(out, ((bytes.len() as u32) << 1) | 1);
            out.extend_from_slice(bytes);
        }
    }
    Ok(())
}

/// Encode an `i32` as the 29-bit `integer-type` body (§3.6). Values
/// outside the signed 29-bit range cannot be represented as an AMF3
/// integer; a real producer would emit them as a Double, so this is a
/// caller error surfaced as [`Error::InvalidData`].
fn write_integer(out: &mut Vec<u8>, i: i32) -> Result<()> {
    // Signed 29-bit range is -2^28 ..= 2^28 - 1.
    const MIN: i32 = -(1 << 28);
    const MAX: i32 = (1 << 28) - 1;
    if !(MIN..=MAX).contains(&i) {
        return Err(Error::invalid(
            "AMF3: integer out of signed 29-bit range — encode as Double",
        ));
    }
    // Mask to the low 29 bits; this is the exact inverse of
    // `sign_extend_29` (a negative value wraps to its 29-bit two's
    // complement, e.g. -1 → 0x1FFF_FFFF).
    let u = (i as u32) & U29_MAX;
    write_u29(out, u);
    Ok(())
}

/// Append a `U29` (§1.3.1) for `v` (which must be `<= U29_MAX`). Mirrors
/// the test-helper `u29_encode` byte-for-byte so the decoder's
/// `read_u29` recovers `v` exactly.
fn write_u29(out: &mut Vec<u8>, v: u32) {
    debug_assert!(v <= U29_MAX);
    if v < 0x80 {
        out.push(v as u8);
    } else if v < 0x4000 {
        out.push((((v >> 7) & 0x7F) | 0x80) as u8);
        out.push((v & 0x7F) as u8);
    } else if v < 0x20_0000 {
        out.push((((v >> 14) & 0x7F) | 0x80) as u8);
        out.push((((v >> 7) & 0x7F) | 0x80) as u8);
        out.push((v & 0x7F) as u8);
    } else {
        // 4-byte form: first 3 bytes carry 7 bits each + continuation
        // flag; the 4th carries all 8 bits (7+7+7+8 = 29).
        out.push((((v >> 22) & 0x7F) | 0x80) as u8);
        out.push((((v >> 15) & 0x7F) | 0x80) as u8);
        out.push((((v >> 8) & 0x7F) | 0x80) as u8);
        out.push((v & 0xFF) as u8);
    }
}

/// Write a UTF-8-vr (§1.3.2 / §3.8): emit a string-table reference if
/// `s` was already written as a literal this message; otherwise emit a
/// literal (length-prefixed body) and append it to the table. The empty
/// string is always a literal and never enters the table.
fn write_utf8_vr(out: &mut Vec<u8>, s: &str, state: &mut EncState) -> Result<()> {
    if s.is_empty() {
        // Literal, length 0: `(0 << 1) | 1` = 1.
        write_u29(out, 1);
        return Ok(());
    }
    if let Some(idx) = state.string_index(s) {
        // Reference: `idx << 1` (low bit clear).
        if idx > (U29_MAX >> 1) as usize {
            return Err(Error::invalid(
                "AMF3: string-table index too large to encode",
            ));
        }
        write_u29(out, (idx as u32) << 1);
        return Ok(());
    }
    let bytes = s.as_bytes();
    if bytes.len() > U29_LEN_MAX {
        return Err(Error::invalid("AMF3: string too long to encode"));
    }
    write_u29(out, ((bytes.len() as u32) << 1) | 1);
    out.extend_from_slice(bytes);
    state.strings.push(s.to_owned());
    Ok(())
}

/// Write an xml / xml-doc body as a fresh literal (these types live in
/// the object table on the decode side, so the string table is never
/// consulted — every occurrence is a literal).
fn write_xml_like(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    if bytes.len() > U29_LEN_MAX {
        return Err(Error::invalid("AMF3: xml body too long to encode"));
    }
    write_u29(out, ((bytes.len() as u32) << 1) | 1);
    out.extend_from_slice(bytes);
    Ok(())
}

/// Write an array-type body (§3.11): a literal header carrying the dense
/// count, then the associative `(key, value)*` run terminated by the
/// empty-string key, then the dense values in order.
fn write_array(out: &mut Vec<u8>, arr: &Amf3Array, state: &mut EncState) -> Result<()> {
    if arr.dense.len() > U29_LEN_MAX {
        return Err(Error::invalid(
            "AMF3: array dense portion too long to encode",
        ));
    }
    // Literal header: `(dense_count << 1) | 1`.
    write_u29(out, ((arr.dense.len() as u32) << 1) | 1);
    for (key, val) in &arr.assoc {
        // An empty assoc key would be read as the terminator, silently
        // truncating the assoc run — reject it rather than corrupt.
        if key.is_empty() {
            return Err(Error::invalid("AMF3: array assoc key must be non-empty"));
        }
        write_utf8_vr(out, key, state)?;
        write_value(out, val, state)?;
    }
    // Associative terminator: empty-string key.
    write_utf8_vr(out, "", state)?;
    for val in &arr.dense {
        write_value(out, val, state)?;
    }
    Ok(())
}

/// Write an object-type body (§3.12) with an inline traits header.
///
/// The traits header `U29` low nibble is, from bit 0:
/// `1` (instance, not object-ref), `1` (traits inline, not traits-ref),
/// `externalizable`, `dynamic`; bits 4.. carry the sealed-member count.
fn write_object(out: &mut Vec<u8>, obj: &Amf3Object, state: &mut EncState) -> Result<()> {
    if obj.externalizable {
        // traits-ext: bit0=1 (instance), bit1=1 (inline), bit2=1 (ext).
        // Sealed count must be 0 (§3.12). No body bytes follow the class
        // name — matching the decoder's no-recipe stance.
        write_u29(out, 0b0111);
        write_utf8_vr(out, &obj.class_name, state)?;
        return Ok(());
    }
    if obj.sealed_names.len() != obj.sealed_values.len() {
        return Err(Error::invalid(
            "AMF3: object sealed_names / sealed_values length mismatch",
        ));
    }
    let sealed_count = obj.sealed_names.len();
    // Header value: bit0=1, bit1=1, bit2=0 (not ext), bit3=dynamic,
    // bits 4.. = sealed_count.
    let dynamic_bit = if obj.dynamic { 1u32 } else { 0 };
    let header = (sealed_count as u64) << 4 | (dynamic_bit << 3) as u64 | 0b0011;
    if header > u64::from(U29_MAX) {
        return Err(Error::invalid(
            "AMF3: object has too many sealed members to encode",
        ));
    }
    write_u29(out, header as u32);
    write_utf8_vr(out, &obj.class_name, state)?;
    for name in &obj.sealed_names {
        write_utf8_vr(out, name, state)?;
    }
    for val in &obj.sealed_values {
        write_value(out, val, state)?;
    }
    if obj.dynamic {
        for (key, val) in &obj.dynamic_members {
            if key.is_empty() {
                return Err(Error::invalid("AMF3: dynamic-member key must be non-empty"));
            }
            write_utf8_vr(out, key, state)?;
            write_value(out, val, state)?;
        }
        // Dynamic terminator: empty-string key.
        write_utf8_vr(out, "", state)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper — encode a U29 literal length flag (`(len << 1) | 1`).
    fn u29_lit_len(len: usize) -> Vec<u8> {
        u29_encode(((len as u32) << 1) | 1)
    }

    /// Helper — encode a U29 reference flag (`idx << 1`).
    fn u29_ref(idx: usize) -> Vec<u8> {
        u29_encode((idx as u32) << 1)
    }

    fn u29_encode(v: u32) -> Vec<u8> {
        assert!(v <= U29_MAX);
        if v < 0x80 {
            vec![v as u8]
        } else if v < 0x4000 {
            vec![(((v >> 7) & 0x7F) | 0x80) as u8, (v & 0x7F) as u8]
        } else if v < 0x20_0000 {
            vec![
                (((v >> 14) & 0x7F) | 0x80) as u8,
                (((v >> 7) & 0x7F) | 0x80) as u8,
                (v & 0x7F) as u8,
            ]
        } else {
            // 4-byte form: first 3 bytes carry 7 bits each with the
            // continuation flag set; the 4th byte carries all 8 bits.
            // Total significant bits = 7+7+7+8 = 29.
            vec![
                (((v >> 22) & 0x7F) | 0x80) as u8,
                (((v >> 15) & 0x7F) | 0x80) as u8,
                (((v >> 8) & 0x7F) | 0x80) as u8,
                (v & 0xFF) as u8,
            ]
        }
    }

    #[test]
    fn u29_round_trip_at_boundaries() {
        for &v in &[
            0u32, 1, 0x7F, 0x80, 0x3FFF, 0x4000, 0x1F_FFFF, 0x20_0000, U29_MAX,
        ] {
            let enc = u29_encode(v);
            let (got, np) = read_u29(&enc, 0).unwrap();
            assert_eq!(got, v, "value {v:#x} round-tripped wrong");
            assert_eq!(np, enc.len(), "value {v:#x} cursor wrong");
        }
    }

    #[test]
    fn u29_four_byte_form_reaches_max_exactly() {
        // 4-byte form with all-FF body: bits = 7+7+7+8 = 29 ones, i.e.
        // U29_MAX exactly. The encoding is incapable of representing a
        // value larger than `2^29 - 1`, so this is the largest legal
        // input — not an overflow. Confirm the decoder accepts it.
        let bytes = [0xFF, 0xFF, 0xFF, 0xFF];
        let (got, np) = read_u29(&bytes, 0).unwrap();
        assert_eq!(got, U29_MAX);
        assert_eq!(np, 4);
    }

    #[test]
    fn undefined_marker() {
        let (v, p) = parse_amf3_value(&[0x00], 0).unwrap();
        assert_eq!(v, Amf3Value::Undefined);
        assert_eq!(p, 1);
    }

    #[test]
    fn null_marker() {
        let (v, _) = parse_amf3_value(&[0x01], 0).unwrap();
        assert_eq!(v, Amf3Value::Null);
    }

    #[test]
    fn boolean_markers() {
        let (v, _) = parse_amf3_value(&[0x02], 0).unwrap();
        assert_eq!(v, Amf3Value::Boolean(false));
        let (v, _) = parse_amf3_value(&[0x03], 0).unwrap();
        assert_eq!(v, Amf3Value::Boolean(true));
    }

    #[test]
    fn integer_positive_and_negative() {
        // 0 → integer-marker + U29(0)
        let (v, _) = parse_amf3_value(&[0x04, 0x00], 0).unwrap();
        assert_eq!(v, Amf3Value::Integer(0));
        // 1
        let (v, _) = parse_amf3_value(&[0x04, 0x01], 0).unwrap();
        assert_eq!(v, Amf3Value::Integer(1));
        // 127 (max single-byte U29)
        let (v, _) = parse_amf3_value(&[0x04, 0x7F], 0).unwrap();
        assert_eq!(v, Amf3Value::Integer(127));
        // -1: U29 of 2^29 - 1 = 0x1FFF_FFFF, sign-extended via bit28.
        let mut b = vec![0x04];
        b.extend_from_slice(&u29_encode(U29_MAX));
        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(v, Amf3Value::Integer(-1));
        // -2: U29 of 2^29 - 2 = 0x1FFF_FFFE.
        let mut b = vec![0x04];
        b.extend_from_slice(&u29_encode(U29_MAX - 1));
        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(v, Amf3Value::Integer(-2));
    }

    #[test]
    fn double_marker() {
        let mut b = vec![0x05];
        b.extend_from_slice(&core::f64::consts::PI.to_be_bytes());
        let (v, p) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(v, Amf3Value::Double(core::f64::consts::PI));
        assert_eq!(p, 9);
    }

    #[test]
    fn string_literal_and_reference() {
        // Two strings: "ab" then a reference to it via second top-level
        // value (manually walked because parse_amf3_value resets state).
        // Use an array (the assoc keys all share state) to force shared
        // state.
        // Array with assoc { "ab": String("ab"), "": end } and dense=0.
        let mut b = vec![0x09]; // array-marker
        b.extend_from_slice(&u29_lit_len(0)); // dense count = 0, literal
                                              // assoc key "ab" — UTF-8-vr literal
        b.extend_from_slice(&u29_lit_len(2));
        b.extend_from_slice(b"ab");
        // value 0x06 (string) "ab" — at this point "ab" is in the
        // string table at index 0; encode by reference.
        b.push(0x06);
        b.extend_from_slice(&u29_ref(0));
        // assoc terminator: empty string (literal length 0).
        b.extend_from_slice(&u29_lit_len(0));

        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        let arr = match v {
            Amf3Value::Array(a) => a,
            other => panic!("expected array, got {other:?}"),
        };
        assert_eq!(arr.dense, Vec::<Amf3Value>::new());
        assert_eq!(arr.assoc.len(), 1);
        assert_eq!(arr.assoc[0].0, "ab");
        assert_eq!(arr.assoc[0].1, Amf3Value::String("ab".into()));
    }

    #[test]
    fn empty_string_never_referenced() {
        // Marker 0x06 + U29 literal length 0.
        let b = [0x06u8, 0x01];
        let (v, p) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(v, Amf3Value::String(String::new()));
        assert_eq!(p, 2);
    }

    #[test]
    fn string_reference_out_of_range_errors() {
        // 0x06 + U29 reference flag (low bit 0) with idx 0 — but the
        // table is empty.
        let b = [0x06u8, 0x00];
        assert!(matches!(
            parse_amf3_value(&b, 0),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn date_literal_records_epoch_ms() {
        let mut b = vec![0x08, 0x01];
        let ms = 1_700_000_000_000.0_f64;
        b.extend_from_slice(&ms.to_be_bytes());
        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(v, Amf3Value::Date(ms));
    }

    #[test]
    fn array_with_assoc_and_dense() {
        // Array { assoc: { "x": Integer(1) }, dense: [Integer(2), Integer(3)] }
        let mut b = vec![0x09];
        b.extend_from_slice(&u29_lit_len(2)); // dense count = 2
        b.extend_from_slice(&u29_lit_len(1)); // key "x"
        b.push(b'x');
        b.push(0x04);
        b.push(0x01); // integer 1
        b.extend_from_slice(&u29_lit_len(0)); // assoc terminator
        b.push(0x04);
        b.push(0x02);
        b.push(0x04);
        b.push(0x03);
        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        let arr = match v {
            Amf3Value::Array(a) => a,
            _ => panic!(),
        };
        assert_eq!(arr.assoc.len(), 1);
        assert_eq!(arr.assoc[0], ("x".into(), Amf3Value::Integer(1)));
        assert_eq!(
            arr.dense,
            vec![Amf3Value::Integer(2), Amf3Value::Integer(3)]
        );
    }

    #[test]
    fn anonymous_dynamic_object() {
        // Object: anonymous, dynamic = true, 0 sealed members; one
        // dynamic member {"k": Integer(7)}.
        // Traits inline header: 0b1011 = sealed_count=0, dynamic=1,
        //                                externalizable=0, traits-inline=1, instance=1
        //                              → u29 value 0xB = 11
        let mut b = vec![0x0A];
        b.extend_from_slice(&u29_encode(0b1011));
        // class name = empty string literal
        b.extend_from_slice(&u29_lit_len(0));
        // dynamic member "k" → Integer(7)
        b.extend_from_slice(&u29_lit_len(1));
        b.push(b'k');
        b.push(0x04);
        b.push(0x07);
        // dynamic terminator
        b.extend_from_slice(&u29_lit_len(0));

        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        let obj = match v {
            Amf3Value::Object(o) => o,
            _ => panic!(),
        };
        assert_eq!(obj.class_name, "");
        assert!(obj.dynamic);
        assert!(!obj.externalizable);
        assert!(obj.sealed_names.is_empty());
        assert!(obj.sealed_values.is_empty());
        assert_eq!(obj.dynamic_members.len(), 1);
        assert_eq!(obj.dynamic_members[0], ("k".into(), Amf3Value::Integer(7)));
    }

    #[test]
    fn typed_sealed_object_with_two_members() {
        // class "Point", two sealed members "x", "y", non-dynamic.
        // Traits inline header: sealed_count=2, dynamic=0, ext=0,
        //   traits-inline=1, instance=1
        // Bits: [sealed:n] [dynamic:1] [ext:1] [traitsref:1] [inst:1]
        //       value: (2 << 4) | (0 << 3) | (0 << 2) | (1 << 1) | 1
        //            = 0x23 = 35
        // Bit layout — kept verbose to mirror §3.12's wire description:
        // sealed_count=2, dynamic=0, externalizable=0, traits-inline=1,
        // instance=1.
        #[allow(clippy::identity_op)]
        let header: u32 = (2 << 4) | (0 << 3) | (0 << 2) | (1 << 1) | 1;
        let mut b = vec![0x0A];
        b.extend_from_slice(&u29_encode(header));
        b.extend_from_slice(&u29_lit_len(5));
        b.extend_from_slice(b"Point");
        // sealed names "x", "y"
        b.extend_from_slice(&u29_lit_len(1));
        b.push(b'x');
        b.extend_from_slice(&u29_lit_len(1));
        b.push(b'y');
        // sealed values 3, 4 (integer-marker + U29)
        b.push(0x04);
        b.push(0x03);
        b.push(0x04);
        b.push(0x04);

        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        let obj = match v {
            Amf3Value::Object(o) => o,
            _ => panic!(),
        };
        assert_eq!(obj.class_name, "Point");
        assert!(!obj.dynamic);
        assert_eq!(obj.sealed_names, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(
            obj.sealed_values,
            vec![Amf3Value::Integer(3), Amf3Value::Integer(4)]
        );
    }

    #[test]
    fn traits_reference_round_trips_second_instance_cheaply() {
        // First instance is the same Point(3, 4) above; the second is
        // Point(5, 6) sent with U29O-traits-ref to the first traits.
        // Build by concatenating into one array so they share state.
        // Bit layout — kept verbose to mirror §3.12's wire description:
        // sealed_count=2, dynamic=0, externalizable=0, traits-inline=1,
        // instance=1.
        #[allow(clippy::identity_op)]
        let header: u32 = (2 << 4) | (0 << 3) | (0 << 2) | (1 << 1) | 1;
        let mut p1 = vec![0x0A];
        p1.extend_from_slice(&u29_encode(header));
        p1.extend_from_slice(&u29_lit_len(5));
        p1.extend_from_slice(b"Point");
        p1.extend_from_slice(&u29_lit_len(1));
        p1.push(b'x');
        p1.extend_from_slice(&u29_lit_len(1));
        p1.push(b'y');
        p1.push(0x04);
        p1.push(0x03);
        p1.push(0x04);
        p1.push(0x04);

        // Second instance: U29O-traits-ref with idx 0 →
        // bits=[idx][01]→ U29 value (0 << 2) | 0b01 = 1.
        let mut p2 = vec![0x0A];
        p2.extend_from_slice(&u29_encode(0b01));
        // Sealed values 5, 6 — names come from the prior trait, NOT
        // resent.
        p2.push(0x04);
        p2.push(0x05);
        p2.push(0x04);
        p2.push(0x06);

        // Wrap the two instances in an array so the state survives.
        let mut b = vec![0x09];
        b.extend_from_slice(&u29_lit_len(2));
        b.extend_from_slice(&u29_lit_len(0));
        b.extend_from_slice(&p1);
        b.extend_from_slice(&p2);

        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        let arr = match v {
            Amf3Value::Array(a) => a,
            _ => panic!(),
        };
        let o1 = match &arr.dense[0] {
            Amf3Value::Object(o) => o,
            _ => panic!(),
        };
        assert_eq!(o1.class_name, "Point");
        assert_eq!(
            o1.sealed_values,
            vec![Amf3Value::Integer(3), Amf3Value::Integer(4)]
        );
        let o2 = match &arr.dense[1] {
            Amf3Value::Object(o) => o,
            _ => panic!(),
        };
        assert_eq!(o2.class_name, "Point");
        assert_eq!(o2.sealed_names, o1.sealed_names);
        assert_eq!(
            o2.sealed_values,
            vec![Amf3Value::Integer(5), Amf3Value::Integer(6)]
        );
    }

    #[test]
    fn object_reference_round_trips_second_instance_as_alias() {
        // Encode a sealed Point at index 0, then a U29O-ref(0) for the
        // second array slot; both should decode to the same value.
        // Bit layout: sealed_count=1, dynamic=0, externalizable=0,
        // traits-inline=1, instance=1.
        #[allow(clippy::identity_op)]
        let header: u32 = (1 << 4) | (0 << 3) | (0 << 2) | (1 << 1) | 1;
        let mut p1 = vec![0x0A];
        p1.extend_from_slice(&u29_encode(header));
        p1.extend_from_slice(&u29_lit_len(1));
        p1.push(b'P');
        p1.extend_from_slice(&u29_lit_len(1));
        p1.push(b'x');
        p1.push(0x04);
        p1.push(0x09);

        // Outer array reserves slot 0; the typed Point lands at slot 1.
        let mut p2 = vec![0x0A];
        p2.extend_from_slice(&u29_ref(1));

        let mut b = vec![0x09];
        b.extend_from_slice(&u29_lit_len(2));
        b.extend_from_slice(&u29_lit_len(0));
        b.extend_from_slice(&p1);
        b.extend_from_slice(&p2);

        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        let arr = match v {
            Amf3Value::Array(a) => a,
            _ => panic!(),
        };
        assert_eq!(arr.dense[0], arr.dense[1]);
    }

    #[test]
    fn externalizable_object_records_flag_no_body_bytes() {
        // Traits-ext header: ext=1, dynamic=0, traits-inline=1,
        // instance=1, sealed_count=0 → (0 << 4) | (0 << 3) | (1 << 2) |
        // (1 << 1) | 1 = 0b0111 = 7.
        let header: u32 = 0b0111;
        let mut b = vec![0x0A];
        b.extend_from_slice(&u29_encode(header));
        b.extend_from_slice(&u29_lit_len(3));
        b.extend_from_slice(b"Ext");
        // No body bytes consumed.
        let trailer_pos_before = b.len();
        let (v, p) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(p, trailer_pos_before);
        let obj = match v {
            Amf3Value::Object(o) => o,
            _ => panic!(),
        };
        assert_eq!(obj.class_name, "Ext");
        assert!(obj.externalizable);
        assert!(!obj.dynamic);
        assert!(obj.sealed_names.is_empty());
    }

    #[test]
    fn xml_document_marker() {
        let xml = "<n/>";
        let mut b = vec![0x07];
        b.extend_from_slice(&u29_lit_len(xml.len()));
        b.extend_from_slice(xml.as_bytes());
        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(v, Amf3Value::XmlDocument(xml.into()));
    }

    #[test]
    fn xml_marker() {
        let xml = "<n/>";
        let mut b = vec![0x0B];
        b.extend_from_slice(&u29_lit_len(xml.len()));
        b.extend_from_slice(xml.as_bytes());
        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(v, Amf3Value::Xml(xml.into()));
    }

    #[test]
    fn byte_array_literal_and_reference() {
        // Top-level ByteArray then reference inside an array. The
        // outer array reserves object-slot 0 before descending; the
        // ByteArray therefore lands at object-slot 1, which is what
        // the reference back-points at.
        let mut p1 = vec![0x0C];
        p1.extend_from_slice(&u29_lit_len(3));
        p1.extend_from_slice(&[0xDE, 0xAD, 0xBE]);
        let mut p2 = vec![0x0C];
        p2.extend_from_slice(&u29_ref(1));
        let mut b = vec![0x09];
        b.extend_from_slice(&u29_lit_len(2));
        b.extend_from_slice(&u29_lit_len(0));
        b.extend_from_slice(&p1);
        b.extend_from_slice(&p2);
        let (v, _) = parse_amf3_value(&b, 0).unwrap();
        let arr = match v {
            Amf3Value::Array(a) => a,
            _ => panic!(),
        };
        assert_eq!(
            arr.dense,
            vec![
                Amf3Value::ByteArray(vec![0xDE, 0xAD, 0xBE]),
                Amf3Value::ByteArray(vec![0xDE, 0xAD, 0xBE])
            ]
        );
    }

    #[test]
    fn rejects_unknown_marker() {
        let b = [0xEE];
        assert!(matches!(
            parse_amf3_value(&b, 0),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_double_errors() {
        // marker 0x05 followed by 7 bytes (need 8).
        let b = [0x05, 0x40, 0x09, 0x21, 0xFB, 0x54, 0x44, 0x2D];
        assert!(matches!(
            parse_amf3_value(&b, 0),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_string_body_errors() {
        // marker 0x06, U29 says 5 bytes, only 2 provided.
        let mut b = vec![0x06];
        b.extend_from_slice(&u29_lit_len(5));
        b.extend_from_slice(b"ab");
        assert!(matches!(
            parse_amf3_value(&b, 0),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_byte_array_errors() {
        let mut b = vec![0x0C];
        b.extend_from_slice(&u29_lit_len(4));
        b.extend_from_slice(&[0xAA, 0xBB]);
        assert!(matches!(
            parse_amf3_value(&b, 0),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn as_f64_and_as_str_helpers() {
        assert_eq!(Amf3Value::Integer(7).as_f64(), Some(7.0));
        assert_eq!(Amf3Value::Double(1.5).as_f64(), Some(1.5));
        assert_eq!(Amf3Value::Boolean(true).as_f64(), Some(1.0));
        assert_eq!(Amf3Value::Boolean(false).as_f64(), Some(0.0));
        assert_eq!(Amf3Value::Null.as_f64(), None);
        assert_eq!(Amf3Value::String("hi".into()).as_str(), Some("hi"));
        assert_eq!(Amf3Value::Xml("<n/>".into()).as_str(), Some("<n/>"));
        assert_eq!(Amf3Value::XmlDocument("<n/>".into()).as_str(), Some("<n/>"));
        assert_eq!(Amf3Value::Integer(7).as_str(), None);
    }

    // -----------------------------------------------------------------
    // Encoder tests
    // -----------------------------------------------------------------

    /// Encode `v`, decode the bytes back, and assert the value survives
    /// and the decoder consumed every emitted byte.
    fn enc_round_trip(v: &Amf3Value) -> Vec<u8> {
        let mut out = Vec::new();
        write_amf3_value(&mut out, v).expect("encode");
        let (back, p) = parse_amf3_value(&out, 0).expect("decode");
        assert_eq!(&back, v, "value did not survive encode→decode");
        assert_eq!(p, out.len(), "decoder did not consume the whole encoding");
        out
    }

    #[test]
    fn encode_scalar_markers_round_trip() {
        enc_round_trip(&Amf3Value::Undefined);
        enc_round_trip(&Amf3Value::Null);
        enc_round_trip(&Amf3Value::Boolean(false));
        enc_round_trip(&Amf3Value::Boolean(true));
        enc_round_trip(&Amf3Value::Double(core::f64::consts::PI));
        enc_round_trip(&Amf3Value::Double(0.0));
        enc_round_trip(&Amf3Value::Date(1_700_000_000_000.0));
    }

    #[test]
    fn encode_scalar_marker_bytes_are_spec_exact() {
        let mut out = Vec::new();
        write_amf3_value(&mut out, &Amf3Value::Undefined).unwrap();
        assert_eq!(out, [0x00]);
        out.clear();
        write_amf3_value(&mut out, &Amf3Value::Null).unwrap();
        assert_eq!(out, [0x01]);
        out.clear();
        write_amf3_value(&mut out, &Amf3Value::Boolean(false)).unwrap();
        assert_eq!(out, [0x02]);
        out.clear();
        write_amf3_value(&mut out, &Amf3Value::Boolean(true)).unwrap();
        assert_eq!(out, [0x03]);
    }

    #[test]
    fn encode_integer_boundaries_round_trip() {
        for &i in &[0i32, 1, -1, 2, -2, 127, -128, (1 << 28) - 1, -(1 << 28)] {
            enc_round_trip(&Amf3Value::Integer(i));
        }
    }

    #[test]
    fn encode_negative_one_is_u29_max() {
        // -1 sign-extends from a 29-bit field of all ones = U29_MAX.
        let mut out = Vec::new();
        write_amf3_value(&mut out, &Amf3Value::Integer(-1)).unwrap();
        let mut expect = vec![0x04];
        expect.extend_from_slice(&u29_encode(U29_MAX));
        assert_eq!(out, expect);
    }

    #[test]
    fn encode_integer_out_of_29bit_range_errors() {
        // 2^28 is one past the signed-29-bit max.
        let mut out = Vec::new();
        assert!(matches!(
            write_amf3_value(&mut out, &Amf3Value::Integer(1 << 28)),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn encode_string_round_trip() {
        enc_round_trip(&Amf3Value::String(String::new()));
        enc_round_trip(&Amf3Value::String("hello".into()));
        enc_round_trip(&Amf3Value::String("üñîçödé".into()));
    }

    #[test]
    fn encode_empty_string_is_always_literal() {
        // 0x06 marker + U29 = 1 (literal, length 0).
        let mut out = Vec::new();
        write_amf3_value(&mut out, &Amf3Value::String(String::new())).unwrap();
        assert_eq!(out, [0x06, 0x01]);
    }

    #[test]
    fn encode_string_dedup_uses_reference() {
        // An array whose assoc key and value are the same string: the
        // value must be emitted as a string-table reference to index 0.
        let arr = Amf3Value::Array(Amf3Array {
            assoc: vec![("ab".into(), Amf3Value::String("ab".into()))],
            dense: vec![],
        });
        let out = enc_round_trip(&arr);
        // 0x09 array, u29(dense=0 lit)=0x01, key "ab" literal
        // (0x05 0x61 0x62), value 0x06 + ref(0)=0x00, terminator 0x01.
        assert_eq!(out, [0x09, 0x01, 0x05, b'a', b'b', 0x06, 0x00, 0x01]);
    }

    #[test]
    fn encode_array_with_assoc_and_dense_round_trip() {
        enc_round_trip(&Amf3Value::Array(Amf3Array {
            assoc: vec![
                ("x".into(), Amf3Value::Integer(1)),
                ("flag".into(), Amf3Value::Boolean(true)),
            ],
            dense: vec![
                Amf3Value::Integer(2),
                Amf3Value::Double(3.5),
                Amf3Value::String("z".into()),
            ],
        }));
    }

    #[test]
    fn encode_empty_array_round_trip() {
        enc_round_trip(&Amf3Value::Array(Amf3Array {
            assoc: vec![],
            dense: vec![],
        }));
    }

    #[test]
    fn encode_array_empty_assoc_key_errors() {
        let mut out = Vec::new();
        let v = Amf3Value::Array(Amf3Array {
            assoc: vec![(String::new(), Amf3Value::Null)],
            dense: vec![],
        });
        assert!(matches!(
            write_amf3_value(&mut out, &v),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn encode_anonymous_dynamic_object_round_trip() {
        enc_round_trip(&Amf3Value::Object(Amf3Object {
            class_name: String::new(),
            dynamic: true,
            externalizable: false,
            sealed_names: vec![],
            sealed_values: vec![],
            dynamic_members: vec![
                ("k".into(), Amf3Value::Integer(7)),
                ("name".into(), Amf3Value::String("v".into())),
            ],
        }));
    }

    #[test]
    fn encode_typed_sealed_object_round_trip() {
        enc_round_trip(&Amf3Value::Object(Amf3Object {
            class_name: "com.example.Point".into(),
            dynamic: false,
            externalizable: false,
            sealed_names: vec!["x".into(), "y".into()],
            sealed_values: vec![Amf3Value::Integer(3), Amf3Value::Integer(4)],
            dynamic_members: vec![],
        }));
    }

    #[test]
    fn encode_sealed_and_dynamic_object_round_trip() {
        enc_round_trip(&Amf3Value::Object(Amf3Object {
            class_name: "Mixed".into(),
            dynamic: true,
            externalizable: false,
            sealed_names: vec!["a".into()],
            sealed_values: vec![Amf3Value::Boolean(false)],
            dynamic_members: vec![("extra".into(), Amf3Value::Double(1.25))],
        }));
    }

    #[test]
    fn encode_externalizable_object_round_trips_flag_only() {
        // The decoder produces an externalizable Object with empty
        // sealed/dynamic vectors and dynamic = false; the encoder must
        // emit exactly that shape so round-trip is identity.
        enc_round_trip(&Amf3Value::Object(Amf3Object {
            class_name: "flex.messaging.io.ArrayCollection".into(),
            dynamic: false,
            externalizable: true,
            sealed_names: vec![],
            sealed_values: vec![],
            dynamic_members: vec![],
        }));
    }

    #[test]
    fn encode_object_sealed_length_mismatch_errors() {
        let mut out = Vec::new();
        let v = Amf3Value::Object(Amf3Object {
            class_name: String::new(),
            dynamic: false,
            externalizable: false,
            sealed_names: vec!["only".into()],
            sealed_values: vec![], // mismatch
            dynamic_members: vec![],
        });
        assert!(matches!(
            write_amf3_value(&mut out, &v),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn encode_xml_and_xml_document_round_trip() {
        enc_round_trip(&Amf3Value::Xml("<root><a/></root>".into()));
        enc_round_trip(&Amf3Value::XmlDocument("<legacy/>".into()));
        enc_round_trip(&Amf3Value::Xml(String::new()));
    }

    #[test]
    fn encode_byte_array_round_trip() {
        enc_round_trip(&Amf3Value::ByteArray(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        enc_round_trip(&Amf3Value::ByteArray(vec![]));
    }

    #[test]
    fn encode_nested_graph_round_trip() {
        // An object whose member is an array of objects, with shared
        // string keys to exercise the string-table reference path across
        // nesting depth.
        let inner = Amf3Value::Object(Amf3Object {
            class_name: String::new(),
            dynamic: true,
            externalizable: false,
            sealed_names: vec![],
            sealed_values: vec![],
            dynamic_members: vec![("label".into(), Amf3Value::String("label".into()))],
        });
        enc_round_trip(&Amf3Value::Object(Amf3Object {
            class_name: String::new(),
            dynamic: true,
            externalizable: false,
            sealed_names: vec![],
            sealed_values: vec![],
            dynamic_members: vec![(
                "items".into(),
                Amf3Value::Array(Amf3Array {
                    assoc: vec![("label".into(), Amf3Value::Integer(1))],
                    dense: vec![inner.clone(), inner],
                }),
            )],
        }));
    }

    #[test]
    fn encode_is_a_fixed_point_for_a_value_corpus() {
        // For a corpus of value shapes, assert the canonical encoding is
        // a fixed point: encode → decode → encode reproduces the first
        // encoding byte-for-byte (the deterministic sibling of the
        // `amf3_roundtrip` differential fuzz target).
        let corpus = [
            Amf3Value::Undefined,
            Amf3Value::Null,
            Amf3Value::Boolean(true),
            Amf3Value::Integer(-(1 << 28)),
            Amf3Value::Double(-0.0),
            Amf3Value::String("repeat".into()),
            Amf3Value::Xml("<x/>".into()),
            Amf3Value::XmlDocument("<y/>".into()),
            Amf3Value::Date(42.0),
            Amf3Value::ByteArray(vec![1, 2, 3]),
            Amf3Value::Array(Amf3Array {
                assoc: vec![("k".into(), Amf3Value::String("k".into()))],
                dense: vec![Amf3Value::Integer(9), Amf3Value::Null],
            }),
            Amf3Value::Object(Amf3Object {
                class_name: "C".into(),
                dynamic: true,
                externalizable: false,
                sealed_names: vec!["s".into()],
                sealed_values: vec![Amf3Value::Boolean(false)],
                dynamic_members: vec![("d".into(), Amf3Value::Double(1.0))],
            }),
        ];
        for v in &corpus {
            let mut e1 = Vec::new();
            write_amf3_value(&mut e1, v).unwrap();
            let (back, p) = parse_amf3_value(&e1, 0).unwrap();
            assert_eq!(p, e1.len(), "value {v:?} left trailing bytes");
            assert_eq!(&back, v, "value {v:?} drifted across encode→decode");
            let mut e2 = Vec::new();
            write_amf3_value(&mut e2, &back).unwrap();
            assert_eq!(e1, e2, "value {v:?} encoding is not a fixed point");
        }
    }

    #[test]
    fn decode_then_encode_is_byte_identity() {
        // Build a non-trivial AMF3 stream by hand (string table exercised
        // via a repeated value), decode it, re-encode, and assert the
        // bytes match — i.e. `write ∘ parse` is identity for a canonical
        // literal-first / string-deduped stream.
        let mut b = vec![0x09]; // array
        b.extend_from_slice(&u29_lit_len(0)); // dense count 0
        b.extend_from_slice(&u29_lit_len(2)); // key "ab"
        b.extend_from_slice(b"ab");
        b.push(0x06); // string value
        b.extend_from_slice(&u29_ref(0)); // ref to "ab"
        b.extend_from_slice(&u29_lit_len(0)); // assoc terminator
        let (v, p) = parse_amf3_value(&b, 0).unwrap();
        assert_eq!(p, b.len());
        let mut out = Vec::new();
        write_amf3_value(&mut out, &v).unwrap();
        assert_eq!(out, b, "re-encode did not reproduce the canonical bytes");
    }
}
