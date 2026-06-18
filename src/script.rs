//! FLV script-data tag writers (spec §E.4.1 / §E.4.4 / §E.5 / §E.6 /
//! Annex A).
//!
//! A `ScriptTagBody` is two AMF0 values: a String *name* and a value
//! payload. For `onMetaData` the name is the literal `"onMetaData"` and
//! the value is an ECMA array of the file's metadata properties
//! (duration, dimensions, codec ids, data rates, …). This module owns
//! the small typed bag of scalar properties and serialises it into a
//! complete script tag (TagType `0x12`) via the AMF0 writers in
//! [`crate::amf0`] and the tag framing in [`crate::tag`].
//!
//! Two further script-data tag flavours land here:
//! [`write_on_cue_point`] emits an Annex A embedded cue point from a
//! [`CuePointParams`] (the four conventional `name` / `time` / `type` /
//! `parameters` properties), and [`write_on_xmp_data`] emits an
//! §E.6 XMP metadata tag carrying the `liveXML` string. Both
//! round-trip bit-exactly through [`crate::FlvDemuxer`] under the
//! existing `metadata["cuepoint.<n>.<key>"]` / `metadata["xmp"]`
//! bag layouts.
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
/// String (`0x02`), [`MetaValue::Date`] → Date (`0x0B`),
/// [`MetaValue::Keyframes`] → an anonymous Object
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
    /// AMF0 Date (SCRIPTDATADATE, §E.4.4.3) — `time_ms` milliseconds
    /// since the Unix epoch (1 Jan 1970 UTC) plus a `tz`
    /// `LocalDateTimeOffset` in minutes from UTC (negative west of
    /// Greenwich, positive east). The spec types `creationdate` as a
    /// String, but Flash-era producers also stamp it as an AMF0 Date;
    /// the demuxer reads both forms (Date flattens to the
    /// `"date:<ms>tz:<offset>"` carrier the typed `creationdate_as_date`
    /// accessor decodes), so the muxer mirrors both.
    Date {
        /// Milliseconds since the Unix epoch (1 Jan 1970 UTC).
        time_ms: f64,
        /// Local time offset in minutes from UTC.
        tz: i16,
    },
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

    /// Append an AMF0 Date property (SCRIPTDATADATE, §E.4.4.3) and
    /// return `self` for chaining. `time_ms` is milliseconds since the
    /// Unix epoch (1 Jan 1970 UTC); `tz` is the `LocalDateTimeOffset`
    /// in minutes from UTC (negative west of Greenwich, positive east).
    ///
    /// Used for `creationdate` when a producer stamps it as an AMF0 Date
    /// rather than a free-form String. The demuxer reads it back through
    /// the `"date:<ms>tz:<offset>"` carrier that
    /// [`crate::TypedMetadata::creationdate_as_date`] decodes into the
    /// same `(time_ms, tz)` pair.
    pub fn date(mut self, key: &str, time_ms: f64, tz: i16) -> Self {
        self.entries
            .push((key.to_string(), MetaValue::Date { time_ms, tz }));
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
            MetaValue::Date { time_ms, tz } => {
                if !time_ms.is_finite() {
                    return Err(Error::invalid(
                        "FLV onMetaData: Date time_ms must be finite",
                    ));
                }
                amf0::write_date(out, *time_ms, *tz)?;
            }
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

/// Kind tag for an embedded cue point — wire-serialised under the
/// conventional `type` property (Annex A.2: "The first value shall be a
/// string that represents the name of the AMF sample"; the per-cue
/// properties layout is fixed by long-standing Flash convention).
///
/// `Event` cue points are dispatched whenever the playhead passes the
/// cue timestamp during normal playback. `Navigation` cue points
/// additionally land at the closest preceding video keyframe so
/// the runtime can offer them as seek targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CuePointType {
    /// Dispatched on playhead pass-through. Wire form: `"event"`.
    Event,
    /// Dispatched on playhead pass-through; additionally surfaced as a
    /// seek target on the closest preceding video keyframe. Wire form:
    /// `"navigation"`.
    Navigation,
}

impl CuePointType {
    /// Lower-case wire spelling exactly matching the spec convention.
    pub fn as_str(self) -> &'static str {
        match self {
            CuePointType::Event => "event",
            CuePointType::Navigation => "navigation",
        }
    }
}

/// Typed cue-point parameter pack for [`write_on_cue_point`] (Annex A).
///
/// Each cue point is a single AMF0 object carrying four well-known
/// properties:
///
/// * `name` — opaque identifier the producer assigns to this cue.
/// * `time` — wall-clock time in **seconds**, as an AMF0 Number.
/// * `type` — `"event"` or `"navigation"` (see [`CuePointType`]).
/// * `parameters` — an anonymous Object of user-defined name → string
///   pairs (Annex A models cue parameters as a free-form bag; the
///   demuxer surfaces them under `cuepoint.<n>.parameters.<key>`).
///
/// The parameter bag is intentionally limited to `(name, String)`
/// pairs because that is what the existing demuxer
/// (`flatten_amf_value` → `metadata["cuepoint.N.parameters.<key>"]`)
/// already round-trips through. Producers needing richer typed
/// parameters can stitch them in at the AMF0 level themselves — this
/// helper covers the common case (chapter markers, ad-insertion
/// triggers, captions) without dragging an AMF builder API into the
/// public surface.
#[derive(Clone, Debug, PartialEq)]
pub struct CuePointParams {
    /// Producer-assigned identifier (`name` property).
    pub name: String,
    /// Cue time in seconds (`time` property).
    pub time_seconds: f64,
    /// Event / Navigation (`type` property).
    pub kind: CuePointType,
    /// User-defined name → string parameter pairs (`parameters`
    /// property). Empty when the cue carries no extra parameters.
    pub parameters: Vec<(String, String)>,
}

impl CuePointParams {
    /// Construct a parameter-less cue at `time_seconds`.
    pub fn new(name: &str, time_seconds: f64, kind: CuePointType) -> Self {
        Self {
            name: name.to_string(),
            time_seconds,
            kind,
            parameters: Vec::new(),
        }
    }

    /// Append a user parameter `(name, value)` pair.
    pub fn parameter(mut self, name: &str, value: &str) -> Self {
        self.parameters.push((name.to_string(), value.to_string()));
        self
    }
}

/// Serialise an `onCuePoint` `ScriptTagBody` (no tag framing) into
/// `out` — the AMF0 String name `"onCuePoint"` followed by the cue
/// object whose four properties (`name`, `time`, `type`,
/// `parameters`) the demuxer harvests under
/// `metadata["cuepoint.<n>.<key>"]`.
///
/// Validates that `time_seconds` is finite (the demuxer treats
/// per-cue `time` as a wall-clock seconds value; NaN / ±∞ would
/// silently corrupt the metadata bag).
pub fn write_on_cue_point_body(out: &mut Vec<u8>, params: &CuePointParams) -> Result<()> {
    if !params.time_seconds.is_finite() {
        return Err(Error::invalid(
            "FLV onCuePoint: time_seconds must be finite",
        ));
    }
    // Method name.
    amf0::write_string(out, "onCuePoint")?;
    // Cue object.
    amf0::write_object_start(out)?;
    amf0::write_property_name(out, "name")?;
    amf0::write_string(out, &params.name)?;
    amf0::write_property_name(out, "time")?;
    amf0::write_number(out, params.time_seconds)?;
    amf0::write_property_name(out, "type")?;
    amf0::write_string(out, params.kind.as_str())?;
    amf0::write_property_name(out, "parameters")?;
    // `parameters` is an anonymous Object — even when empty (the spec
    // makes no statement either way; the demuxer's flatten walker
    // handles either shape, but emitting an empty Object keeps the
    // wire payload self-describing).
    amf0::write_object_start(out)?;
    for (k, v) in &params.parameters {
        amf0::write_property_name(out, k)?;
        amf0::write_string(out, v)?;
    }
    amf0::write_object_end(out)?;
    amf0::write_object_end(out)?;
    Ok(())
}

/// Write a complete `onCuePoint` script tag — tag header (TagType
/// `0x12`, `timestamp_ms`, StreamID `0`), the serialised
/// `ScriptTagBody`, and the trailing `PreviousTagSize` (spec §E.3 /
/// §E.4.1, Annex A.2 — cue-point sample format).
///
/// Unlike `onMetaData`, an `onCuePoint` tag carries a non-zero
/// timestamp aligned with the cue's playback time — the Flash runtime
/// dispatches the cue when the playhead passes the tag (Annex A.4
/// "AMF content should be interleaved at the right time along with the
/// audio and video content"). Returns the total number of bytes
/// written.
pub fn write_on_cue_point<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    params: &CuePointParams,
) -> Result<u32> {
    let mut body = Vec::new();
    write_on_cue_point_body(&mut body, params)?;
    tag::write_tag(w, TagType::ScriptData, timestamp_ms, 0, &body)
}

/// Serialise an `onXMPData` `ScriptTagBody` (no tag framing) into
/// `out` — the AMF0 String name `"onXMPData"` followed by an
/// anonymous Object carrying the single `liveXML` String property
/// (spec §E.6). The demuxer's `xmp_liveXML` accessor parses this
/// exact shape and surfaces the payload under `metadata["xmp"]`.
pub fn write_on_xmp_data_body(out: &mut Vec<u8>, live_xml: &str) -> Result<()> {
    amf0::write_string(out, "onXMPData")?;
    amf0::write_object_start(out)?;
    amf0::write_property_name(out, "liveXML")?;
    amf0::write_string(out, live_xml)?;
    amf0::write_object_end(out)?;
    Ok(())
}

/// Write a complete `onXMPData` script tag — tag header (TagType
/// `0x12`, `timestamp_ms`, StreamID `0`), the serialised
/// `ScriptTagBody`, and the trailing `PreviousTagSize` (spec §E.3 /
/// §E.4.1 / §E.6). `live_xml` is the XMP packet bytes (typically the
/// `<x:xmpmeta ...>` envelope a producer hands to the runtime); the
/// demuxer round-trips it byte-for-byte under `metadata["xmp"]`.
///
/// The AMF0 String wire form caps each value at 65535 bytes; XMP
/// packets near that ceiling should be emitted as their own tag, or
/// the AMF0 LongString writer (`0x0C`) reused — that path is left to
/// callers who need it.
///
/// `timestamp_ms` is the cue-style alignment timestamp. Producers
/// typically place an XMP header tag at the head of the file with
/// `timestamp_ms = 0`.
pub fn write_on_xmp_data<W: Write + ?Sized>(
    w: &mut W,
    timestamp_ms: u32,
    live_xml: &str,
) -> Result<u32> {
    let mut body = Vec::new();
    write_on_xmp_data_body(&mut body, live_xml)?;
    tag::write_tag(w, TagType::ScriptData, timestamp_ms, 0, &body)
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
    fn date_property_round_trips_through_parser() {
        // 14 Nov 2023 22:13:20 UTC, +120 min (UTC+2) offset.
        let bag = MetadataBag::new().number("duration", 2.0).date(
            "creationdate",
            1_700_000_000_000.0,
            120,
        );
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();

        let (name, p) = parse_amf0_value(&body, 0).unwrap();
        assert_eq!(name, AmfValue::String("onMetaData".into()));
        let (value, np) = parse_amf0_value(&body, p).unwrap();
        assert_eq!(np, body.len());
        let props = match value {
            AmfValue::EcmaArray(props) => props,
            other => panic!("expected ecma array, got {other:?}"),
        };
        assert_eq!(props.len(), 2);
        assert_eq!(props[0], ("duration".into(), AmfValue::Number(2.0)));
        assert_eq!(
            props[1],
            (
                "creationdate".into(),
                AmfValue::Date {
                    time_ms: 1_700_000_000_000.0,
                    tz: 120
                }
            )
        );
    }

    #[test]
    fn date_property_accepts_negative_tz() {
        // -300 min (UTC-5) offset, west of Greenwich.
        let bag = MetadataBag::new().date("creationdate", 0.0, -300);
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();
        let (_name, p) = parse_amf0_value(&body, 0).unwrap();
        let (value, _np) = parse_amf0_value(&body, p).unwrap();
        let props = match value {
            AmfValue::EcmaArray(props) => props,
            other => panic!("expected ecma array, got {other:?}"),
        };
        assert_eq!(
            props[0],
            (
                "creationdate".into(),
                AmfValue::Date {
                    time_ms: 0.0,
                    tz: -300
                }
            )
        );
    }

    #[test]
    fn date_property_rejects_non_finite_time() {
        let bag = MetadataBag::new().date("creationdate", f64::NAN, 0);
        let err = write_on_metadata_body(&mut Vec::new(), &bag).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("finite"), "got {msg}");
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

    #[test]
    fn on_cue_point_body_round_trips_through_parser() {
        let cue = CuePointParams::new("chapter-1", 12.5, CuePointType::Navigation)
            .parameter("title", "Intro")
            .parameter("section", "A");
        let mut body = Vec::new();
        write_on_cue_point_body(&mut body, &cue).unwrap();

        // First value: the method name string.
        let (name, p) = parse_amf0_value(&body, 0).unwrap();
        assert_eq!(name, AmfValue::String("onCuePoint".into()));
        // Second value: the cue object, consuming the rest exactly.
        let (value, np) = parse_amf0_value(&body, p).unwrap();
        assert_eq!(np, body.len());
        let entries = match value {
            AmfValue::Object(b) => b,
            other => panic!("expected anonymous object, got {other:?}"),
        };
        // Four spec-conventional properties in canonical order.
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries[0],
            ("name".into(), AmfValue::String("chapter-1".into()))
        );
        assert_eq!(entries[1], ("time".into(), AmfValue::Number(12.5)));
        assert_eq!(
            entries[2],
            ("type".into(), AmfValue::String("navigation".into()))
        );
        let params = match &entries[3].1 {
            AmfValue::Object(b) => b,
            other => panic!("parameters: expected anonymous object, got {other:?}"),
        };
        assert_eq!(entries[3].0, "parameters");
        assert_eq!(params.len(), 2);
        assert_eq!(
            params[0],
            ("title".into(), AmfValue::String("Intro".into()))
        );
        assert_eq!(params[1], ("section".into(), AmfValue::String("A".into())));
    }

    #[test]
    fn on_cue_point_event_emits_event_string() {
        let cue = CuePointParams::new("ad-1", 30.0, CuePointType::Event);
        let mut body = Vec::new();
        write_on_cue_point_body(&mut body, &cue).unwrap();
        let (_, p) = parse_amf0_value(&body, 0).unwrap();
        let (value, _) = parse_amf0_value(&body, p).unwrap();
        let entries = match value {
            AmfValue::Object(b) => b,
            other => panic!("expected object, got {other:?}"),
        };
        assert_eq!(entries[2].0, "type");
        assert_eq!(entries[2].1, AmfValue::String("event".into()));
    }

    #[test]
    fn on_cue_point_empty_parameters_round_trip() {
        let cue = CuePointParams::new("mark", 0.0, CuePointType::Event);
        let mut body = Vec::new();
        write_on_cue_point_body(&mut body, &cue).unwrap();
        let (_, p) = parse_amf0_value(&body, 0).unwrap();
        let (value, np) = parse_amf0_value(&body, p).unwrap();
        assert_eq!(np, body.len());
        let entries = match value {
            AmfValue::Object(b) => b,
            other => panic!("expected object, got {other:?}"),
        };
        let params = match &entries[3].1 {
            AmfValue::Object(b) => b,
            other => panic!("expected object, got {other:?}"),
        };
        assert!(params.is_empty());
    }

    #[test]
    fn on_cue_point_rejects_non_finite_time() {
        let cue = CuePointParams::new("nan", f64::NAN, CuePointType::Event);
        let err = write_on_cue_point_body(&mut Vec::new(), &cue).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("finite"), "got {msg}");
    }

    #[test]
    fn on_cue_point_full_tag_has_correct_timestamp_and_size() {
        let cue = CuePointParams::new("c", 1.0, CuePointType::Event);
        let mut out = Vec::new();
        let total = write_on_cue_point(&mut out, 1_000, &cue).unwrap();
        assert_eq!(total as usize, out.len());
        // TagType byte = 0x12 (script data).
        assert_eq!(out[0], 0x12);
        // Timestamp UI24 + TimestampExtended UI8 = 1000.
        let ts = ((out[4] as u32) << 16) | ((out[5] as u32) << 8) | (out[6] as u32);
        assert_eq!(ts, 1_000);
        assert_eq!(out[7], 0);
        // DataSize matches body length.
        let data_size = ((out[1] as u32) << 16) | ((out[2] as u32) << 8) | (out[3] as u32);
        assert_eq!(data_size, total - 15);
    }

    #[test]
    fn on_xmp_data_body_round_trips_through_parser() {
        let xml = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">hi</x:xmpmeta>";
        let mut body = Vec::new();
        write_on_xmp_data_body(&mut body, xml).unwrap();

        let (name, p) = parse_amf0_value(&body, 0).unwrap();
        assert_eq!(name, AmfValue::String("onXMPData".into()));
        let (value, np) = parse_amf0_value(&body, p).unwrap();
        assert_eq!(np, body.len());
        let entries = match value {
            AmfValue::Object(b) => b,
            other => panic!("expected anonymous object, got {other:?}"),
        };
        // Exactly one property: liveXML String.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], ("liveXML".into(), AmfValue::String(xml.into())));
    }

    #[test]
    fn on_xmp_data_full_tag_has_script_tagtype() {
        let mut out = Vec::new();
        write_on_xmp_data(&mut out, 0, "<x>x</x>").unwrap();
        // TagType byte = 0x12 (script data).
        assert_eq!(out[0], 0x12);
    }

    #[test]
    fn cue_point_type_wire_strings_match_demuxer_expectation() {
        assert_eq!(CuePointType::Event.as_str(), "event");
        assert_eq!(CuePointType::Navigation.as_str(), "navigation");
    }
}
