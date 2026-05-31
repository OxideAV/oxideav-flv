//! FLV `onMetaData` script-data tag writer (spec §E.4.1 / §E.4.4 /
//! §E.5).
//!
//! A `ScriptTagBody` is two AMF0 values: a String *name* and a value
//! payload. For `onMetaData` the name is the literal `"onMetaData"` and
//! the value is an ECMA array of the file's metadata properties
//! (duration, dimensions, codec ids, data rates, …). This module owns
//! the small typed bag of scalar properties and serialises it into a
//! complete script tag (TagType `0x12`) via the AMF0 writers in
//! [`crate::amf0`] and the tag framing in [`crate::tag`].
//!
//! The spec does not mandate property ordering; [`MetadataBag`]
//! preserves insertion order so the emitted bytes are deterministic and
//! a byte-exact round-trip through [`crate::FlvDemuxer`] is reproducible.
//! Composite properties (the `keyframes` seek table, per-track info
//! maps) are out of scope for this first muxer slice — only the three
//! AMF0 scalar types a basic `onMetaData` needs are modelled.

use std::io::Write;

use oxideav_core::Result;

use crate::amf0;
use crate::tag::{self, TagType};

/// One typed `onMetaData` property value. Maps to exactly one AMF0
/// scalar type on the wire: [`MetaValue::Number`] → Number (`0x00`),
/// [`MetaValue::Boolean`] → Boolean (`0x01`), [`MetaValue::String`] →
/// String (`0x02`).
#[derive(Clone, Debug, PartialEq)]
pub enum MetaValue {
    /// AMF0 Number — every numeric `onMetaData` field (duration, width,
    /// height, framerate, *datarate, audiosamplerate, filesize, …) is a
    /// double on the wire even when integer-valued.
    Number(f64),
    /// AMF0 Boolean — e.g. `stereo`, `canSeekToEnd`.
    Boolean(bool),
    /// AMF0 String — e.g. `encoder`, `metadatacreator`.
    String(String),
}

/// Ordered set of `onMetaData` properties to serialise.
///
/// Build one with the chained setters and hand it to
/// [`write_on_metadata`]:
///
/// ```
/// use oxideav_flv::script::MetadataBag;
/// let bag = MetadataBag::new()
///     .number("duration", 2.0)
///     .number("audiosamplerate", 44_100.0)
///     .boolean("stereo", true)
///     .string("encoder", "oxideav-flv");
/// assert_eq!(bag.len(), 4);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetadataBag {
    entries: Vec<(String, MetaValue)>,
}

impl MetadataBag {
    /// An empty bag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a Number property and return `self` for chaining.
    pub fn number(mut self, key: &str, n: f64) -> Self {
        self.entries.push((key.to_string(), MetaValue::Number(n)));
        self
    }

    /// Append a Boolean property and return `self` for chaining.
    pub fn boolean(mut self, key: &str, b: bool) -> Self {
        self.entries.push((key.to_string(), MetaValue::Boolean(b)));
        self
    }

    /// Append a String property and return `self` for chaining.
    pub fn string(mut self, key: &str, s: &str) -> Self {
        self.entries
            .push((key.to_string(), MetaValue::String(s.to_string())));
        self
    }

    /// Number of properties in the bag.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no properties have been added.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The properties in insertion order.
    pub fn entries(&self) -> &[(String, MetaValue)] {
        &self.entries
    }
}

/// Serialise just the `onMetaData` `ScriptTagBody` (no tag framing) into
/// `out`: the String name `"onMetaData"` followed by an ECMA array of
/// the bag's properties (spec §E.4.1 ScriptTagBody, §E.5).
pub fn write_on_metadata_body(out: &mut Vec<u8>, bag: &MetadataBag) -> Result<()> {
    // Name: SCRIPTDATAVALUE of String type (E.4.1 — "Method or object
    // name", Type = 2).
    amf0::write_string(out, "onMetaData")?;
    // Value: SCRIPTDATAVALUE of ECMA-array type (Type = 8). The length
    // hint is the true property count.
    amf0::write_ecma_array_start(out, bag.len() as u32)?;
    for (key, value) in bag.entries() {
        amf0::write_property_name(out, key)?;
        match value {
            MetaValue::Number(n) => amf0::write_number(out, *n)?,
            MetaValue::Boolean(b) => amf0::write_boolean(out, *b)?,
            MetaValue::String(s) => amf0::write_string(out, s)?,
        }
    }
    amf0::write_object_end(out)?;
    Ok(())
}

/// Write a complete `onMetaData` script tag — tag header (TagType
/// `0x12`, timestamp `0`, StreamID `0`), the serialised `ScriptTagBody`,
/// and the trailing `PreviousTagSize` (spec §E.3 / §E.4.1).
///
/// `onMetaData` is conventionally the first tag in the file with a
/// timestamp of `0`. Returns the total number of bytes written
/// (`11 + body.len() + 4`).
pub fn write_on_metadata<W: Write + ?Sized>(w: &mut W, bag: &MetadataBag) -> Result<u32> {
    let mut body = Vec::new();
    write_on_metadata_body(&mut body, bag)?;
    tag::write_tag(w, TagType::ScriptData, 0, 0, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amf0::{parse_amf0_value, AmfValue};

    #[test]
    fn bag_preserves_insertion_order() {
        let bag = MetadataBag::new()
            .number("duration", 2.0)
            .boolean("stereo", true)
            .string("encoder", "oxideav-flv");
        let keys: Vec<&str> = bag.entries().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["duration", "stereo", "encoder"]);
    }

    #[test]
    fn body_parses_back_as_name_then_ecma_array() {
        let bag = MetadataBag::new()
            .number("duration", 2.0)
            .number("width", 320.0)
            .boolean("stereo", true)
            .string("encoder", "oxideav-flv");
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();

        // First value: the name string.
        let (name, p) = parse_amf0_value(&body, 0).unwrap();
        assert_eq!(name, AmfValue::String("onMetaData".into()));
        // Second value: the ECMA array, consuming the rest exactly.
        let (value, np) = parse_amf0_value(&body, p).unwrap();
        assert_eq!(np, body.len());
        match value {
            AmfValue::EcmaArray(props) => {
                assert_eq!(props.len(), 4);
                assert_eq!(props[0], ("duration".into(), AmfValue::Number(2.0)));
                assert_eq!(props[1], ("width".into(), AmfValue::Number(320.0)));
                assert_eq!(props[2], ("stereo".into(), AmfValue::Boolean(true)));
                assert_eq!(
                    props[3],
                    ("encoder".into(), AmfValue::String("oxideav-flv".into()))
                );
            }
            other => panic!("expected ecma array, got {other:?}"),
        }
    }

    #[test]
    fn full_tag_has_script_tagtype_and_consistent_size() {
        let bag = MetadataBag::new().number("duration", 1.0);
        let mut out = Vec::new();
        let total = write_on_metadata(&mut out, &bag).unwrap();
        assert_eq!(total as usize, out.len());
        // TagType byte = 0x12 (script data).
        assert_eq!(out[0], 0x12);
        // DataSize (UI24) matches body length = total - 11 - 4.
        let data_size = ((out[1] as u32) << 16) | ((out[2] as u32) << 8) | (out[3] as u32);
        assert_eq!(data_size, total - 15);
        // Trailing PreviousTagSize = 11 + DataSize.
        let trailer = u32::from_be_bytes([
            out[out.len() - 4],
            out[out.len() - 3],
            out[out.len() - 2],
            out[out.len() - 1],
        ]);
        assert_eq!(trailer, 11 + data_size);
    }
}
