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
//! In addition to the three AMF0 scalar types ([`MetaValue::Number`] /
//! [`MetaValue::Boolean`] / [`MetaValue::String`]), the bag also models
//! the `onMetaData.keyframes` seek-table composite via
//! [`MetaValue::Keyframes`] — an anonymous AMF0 Object carrying two
//! parallel SCRIPTDATASTRICTARRAY properties (`filepositions[]` and
//! `times[]`) that the demuxer harvests for the O(log n) seek-by-pts
//! bisect path (see [`crate::FlvDemuxer`] `seek_to`). The wire layout
//! follows §E.4.4 / §E.4.4.7 / §E.4.4.9 (StrictArray); the property
//! name `"keyframes"` and the inner field names `"filepositions"` /
//! `"times"` are the de-facto convention every keyframe-indexed FLV
//! producer follows and which `FlvDemuxer` parses on the read side.
//! Per-track info maps remain out of scope for this slice.

use std::io::Write;

use oxideav_core::{Error, Result};

use crate::amf0;
use crate::tag::{self, TagType};

/// Largest integer that round-trips losslessly through an IEEE-754
/// `f64` (i.e. `2^53`). AMF0 numbers are doubles on the wire, so a
/// `keyframes` `filepositions` entry above this cap would silently
/// quantise on serialisation — we reject it instead.
const F64_LOSSLESS_INTEGER_MAX: u64 = 1u64 << 53;

/// One typed `onMetaData` property value. Maps to exactly one AMF0
/// type on the wire: [`MetaValue::Number`] → Number (`0x00`),
/// [`MetaValue::Boolean`] → Boolean (`0x01`), [`MetaValue::String`] →
/// String (`0x02`), [`MetaValue::Keyframes`] → an anonymous Object
/// (`0x03`) carrying two parallel SCRIPTDATASTRICTARRAY (`0x0A`)
/// properties (`filepositions` + `times`).
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
    /// `onMetaData.keyframes` seek-table — two parallel arrays of equal
    /// length. `file_positions[i]` is the absolute byte offset of the
    /// i-th video keyframe tag (the TagType byte, *not* the preceding
    /// PreviousTagSize prefix); `times_seconds[i]` is its wall-clock
    /// time in seconds. Entries are sorted ascending by
    /// `times_seconds`; the demuxer bisects `times_seconds` for the
    /// O(log n) seek-by-pts path and follows the matching
    /// `file_positions` offset.
    Keyframes {
        /// Absolute byte offsets of each video keyframe tag.
        file_positions: Vec<u64>,
        /// Wall-clock time of each keyframe in seconds.
        times_seconds: Vec<f64>,
    },
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

    /// Append the `onMetaData.keyframes` seek-table — the parallel
    /// `filepositions[]` / `times[]` arrays the demuxer harvests for
    /// the O(log n) bisect-seek path. The property name is fixed at
    /// the conventional `"keyframes"`; both arrays must be equal in
    /// length and `times_seconds` must be sorted ascending
    /// (non-decreasing; duplicate timestamps are legal when two
    /// keyframes share a millisecond).
    ///
    /// The caller writes the FLV body first, records each keyframe
    /// tag's absolute byte offset (the offset of the `TagType` byte,
    /// not the preceding `PreviousTagSize` prefix), and finally emits
    /// an `onMetaData` carrying this composite at the head of the
    /// file. Producers typically reserve a fixed-size `onMetaData`
    /// slot up front, mux the body to learn the offsets, then rewrite
    /// the slot in-place with the populated toc; this writer is
    /// agnostic of that strategy and just emits the bytes the demuxer
    /// reads back.
    pub fn keyframes(mut self, file_positions: Vec<u64>, times_seconds: Vec<f64>) -> Self {
        self.entries.push((
            "keyframes".to_string(),
            MetaValue::Keyframes {
                file_positions,
                times_seconds,
            },
        ));
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
            MetaValue::Keyframes {
                file_positions,
                times_seconds,
            } => write_keyframes_object(out, file_positions, times_seconds)?,
        }
    }
    amf0::write_object_end(out)?;
    Ok(())
}

/// Emit the AMF0 anonymous-Object value for an `onMetaData.keyframes`
/// composite — `0x03` start marker, then the two SCRIPTDATASTRICTARRAY
/// properties `filepositions` and `times` (each a UI32 BE length
/// followed by Number values per §E.4.4.9 / §E.4.4.2 type 10), then
/// the `0x00 0x00 0x09` object-end terminator. The property emission
/// order (`filepositions` first, then `times`) matches the convention
/// every observed FLV producer follows; the AMF0 decoder is
/// order-insensitive so the demuxer parses either ordering, but
/// emitting in the conventional order keeps the round-trip bytes
/// stable.
///
/// Validates the toc invariants the demuxer enforces on the read
/// side:
///
/// * Both arrays non-empty and equal in length.
/// * `times_seconds` finite and sorted ascending (non-decreasing).
/// * `file_positions` representable losslessly as IEEE-754 doubles
///   (`≤ 2^53`); a larger value would round when serialised to AMF0
///   Number and the demuxer would read back a different offset.
fn write_keyframes_object(
    out: &mut Vec<u8>,
    file_positions: &[u64],
    times_seconds: &[f64],
) -> Result<()> {
    if file_positions.is_empty() {
        return Err(Error::invalid(
            "FLV onMetaData.keyframes: filepositions / times must be non-empty",
        ));
    }
    if file_positions.len() != times_seconds.len() {
        return Err(Error::invalid(format!(
            "FLV onMetaData.keyframes: filepositions ({}) / times ({}) length mismatch",
            file_positions.len(),
            times_seconds.len()
        )));
    }
    for &pos in file_positions {
        if pos > F64_LOSSLESS_INTEGER_MAX {
            return Err(Error::invalid(format!(
                "FLV onMetaData.keyframes: filepositions entry {pos} \
                 exceeds 2^53 (would round when serialised as AMF0 Number)"
            )));
        }
    }
    for w in times_seconds.windows(2) {
        if !w[0].is_finite() || !w[1].is_finite() {
            return Err(Error::invalid(
                "FLV onMetaData.keyframes: times entries must be finite",
            ));
        }
        if w[1] < w[0] {
            return Err(Error::invalid(format!(
                "FLV onMetaData.keyframes: times must be sorted ascending \
                 (saw {} after {})",
                w[1], w[0]
            )));
        }
    }
    // Even single-entry tocs need the finite check.
    if times_seconds.len() == 1 && !times_seconds[0].is_finite() {
        return Err(Error::invalid(
            "FLV onMetaData.keyframes: times entries must be finite",
        ));
    }

    let positions_as_f64: Vec<f64> = file_positions.iter().map(|&p| p as f64).collect();
    amf0::write_object_start(out)?;
    amf0::write_property_name(out, "filepositions")?;
    amf0::write_strict_array_number(out, &positions_as_f64)?;
    amf0::write_property_name(out, "times")?;
    amf0::write_strict_array_number(out, times_seconds)?;
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
    fn keyframes_property_round_trips_through_parser() {
        // Two video keyframes at t=0 and t=1.0 s, at synthetic file
        // offsets the demuxer would have recorded while muxing.
        let bag = MetadataBag::new()
            .number("duration", 2.0)
            .keyframes(vec![13, 8_192], vec![0.0, 1.0]);
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();

        // Skip the name string ("onMetaData").
        let (name, p) = parse_amf0_value(&body, 0).unwrap();
        assert_eq!(name, AmfValue::String("onMetaData".into()));
        // Parse the ECMA array.
        let (value, np) = parse_amf0_value(&body, p).unwrap();
        assert_eq!(np, body.len());
        let props = match value {
            AmfValue::EcmaArray(props) => props,
            other => panic!("expected ecma array, got {other:?}"),
        };
        assert_eq!(props.len(), 2);
        assert_eq!(props[0].0, "duration");
        assert_eq!(props[1].0, "keyframes");
        // The keyframes property is an anonymous Object with two
        // StrictArray<Number> properties in spec-conventional order.
        let kf_entries = match &props[1].1 {
            AmfValue::Object(entries) => entries,
            other => panic!("expected anonymous object, got {other:?}"),
        };
        assert_eq!(kf_entries.len(), 2);
        assert_eq!(kf_entries[0].0, "filepositions");
        assert_eq!(kf_entries[1].0, "times");
        match &kf_entries[0].1 {
            AmfValue::StrictArray(vs) => {
                assert_eq!(vs, &vec![AmfValue::Number(13.0), AmfValue::Number(8_192.0)]);
            }
            other => panic!("filepositions: expected strict array, got {other:?}"),
        }
        match &kf_entries[1].1 {
            AmfValue::StrictArray(vs) => {
                assert_eq!(vs, &vec![AmfValue::Number(0.0), AmfValue::Number(1.0)]);
            }
            other => panic!("times: expected strict array, got {other:?}"),
        }
    }

    #[test]
    fn keyframes_rejects_length_mismatch() {
        let bag = MetadataBag::new().keyframes(vec![13, 8_192], vec![0.0]);
        let err = write_on_metadata_body(&mut Vec::new(), &bag).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("length mismatch"), "got {msg}");
    }

    #[test]
    fn keyframes_rejects_empty_arrays() {
        let bag = MetadataBag::new().keyframes(Vec::new(), Vec::new());
        let err = write_on_metadata_body(&mut Vec::new(), &bag).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("non-empty"), "got {msg}");
    }

    #[test]
    fn keyframes_rejects_non_monotonic_times() {
        let bag = MetadataBag::new().keyframes(vec![13, 8_192], vec![1.0, 0.5]);
        let err = write_on_metadata_body(&mut Vec::new(), &bag).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("ascending"), "got {msg}");
    }

    #[test]
    fn keyframes_accepts_equal_timestamps() {
        // Two keyframes at the same millisecond is rare but legal —
        // the demuxer's bisect picks the bisect-left entry. The
        // writer must not reject it.
        let bag = MetadataBag::new().keyframes(vec![13, 14], vec![0.500, 0.500]);
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).expect("equal timestamps allowed");
        assert!(!body.is_empty());
    }

    #[test]
    fn keyframes_rejects_non_finite_time() {
        let bag = MetadataBag::new().keyframes(vec![13, 8_192], vec![0.0, f64::NAN]);
        let err = write_on_metadata_body(&mut Vec::new(), &bag).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("finite"), "got {msg}");
    }

    #[test]
    fn keyframes_rejects_position_above_2pow53() {
        // 2^53 fits losslessly; 2^53 + 1 does not.
        let just_too_big = (1u64 << 53) + 1;
        let bag = MetadataBag::new().keyframes(vec![13, just_too_big], vec![0.0, 1.0]);
        let err = write_on_metadata_body(&mut Vec::new(), &bag).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("2^53") || msg.contains("would round"),
            "got {msg}"
        );
    }

    #[test]
    fn keyframes_accepts_position_at_2pow53_boundary() {
        let bag = MetadataBag::new().keyframes(vec![13, 1u64 << 53], vec![0.0, 1.0]);
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).expect("2^53 is the boundary, allowed");
        assert!(!body.is_empty());
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
