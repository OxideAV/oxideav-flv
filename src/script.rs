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
//! [`MetaValue`]'s type matrix mirrors the matrix the demuxer parses:
//! the AMF0 scalars ([`MetaValue::Number`] / [`MetaValue::Boolean`] /
//! [`MetaValue::String`] / [`MetaValue::Date`] / [`MetaValue::Null`] /
//! [`MetaValue::Undefined`] / [`MetaValue::Xml`]) plus the composites
//! [`MetaValue::Object`] / [`MetaValue::EcmaArray`] /
//! [`MetaValue::StrictArray`] (round-tripped flattened under
//! `metadata["<key>.<subkey>"]` / `metadata["<key>[i]"]`) and the
//! `onMetaData.keyframes` seek-table via [`MetaValue::Keyframes`] — an
//! anonymous AMF0 Object carrying two parallel SCRIPTDATASTRICTARRAY
//! properties (`filepositions[]` and `times[]`) the demuxer harvests for
//! the O(log n) seek-by-pts bisect path (see [`crate::FlvDemuxer`]
//! `seek_to`). The keyframes wire layout follows §E.4.4 / §E.4.4.7 /
//! §E.4.4.9; the property name `"keyframes"` and the inner field names
//! `"filepositions"` / `"times"` are the de-facto convention every
//! keyframe-indexed FLV producer follows and which `FlvDemuxer` parses.
//! The Enhanced-RTMP-v2 §"Enhancing onMetaData" per-track info maps
//! ([`TrackInfoMap`] / [`TrackInfo`]) build on [`MetaValue::Object`].

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
    /// AMF0 Date (SCRIPTDATADATE, spec §E.4.4.3) — e.g. a `creationdate`
    /// the producer stamps as a typed date rather than a free-form
    /// string. `time_ms` is the DOUBLE `DateTime` (milliseconds since
    /// Jan 1 1970 UTC); `tz` is the SI16 `LocalDateTimeOffset` (local
    /// time offset in minutes from UTC — negative west of Greenwich,
    /// positive east). The demuxer flattens this on the read side into
    /// the `"date:<ms>tz:<offset>"` carrier string surfaced under the
    /// property name, which [`crate::TypedMetadata::creationdate_as_date`]
    /// decodes back into the `(time_ms, tz)` pair.
    Date {
        /// Milliseconds since Jan 1 1970 UTC.
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
    /// An anonymous AMF0 Object (`0x03`) carrying nested
    /// `(property-name, value)` pairs in insertion order, terminated by
    /// the `0x00 0x00 0x09` object-end marker. This is the muxer mirror
    /// of the demuxer's structural `flatten_amf_value` walk: a producer
    /// `onMetaData` property whose value is a sub-object (HDR config,
    /// producer telemetry, the Enhanced-RTMP-v2 `videoTrackIdInfoMap` /
    /// `audioTrackIdInfoMap` per-track maps, …) round-trips back through
    /// `FlvDemuxer` flattened under `metadata["<key>.<subkey>"]`. Values
    /// nest recursively, so a map-of-objects (each trackId keying a
    /// per-track descriptor) is expressible directly.
    Object(Vec<(String, MetaValue)>),
    /// An AMF0 ECMA array (`0x08`) — the same `(name, value)*` body as an
    /// Object but with the type marker the demuxer reads back identically
    /// (it flattens both under `metadata["<key>.<subkey>"]`). The wire
    /// difference is the `0x08` marker + a UI32 associative-count hint
    /// (emitted as the true pair count). Use this when a producer's
    /// schema specifically calls for the ECMA-array marker.
    EcmaArray(Vec<(String, MetaValue)>),
    /// An AMF0 strict array (`0x0A`) of mixed-type, dense-indexed values
    /// (spec §2.12) — the demuxer flattens it under `metadata["<key>[i]"]`.
    /// Unlike Object / EcmaArray there are no property names and no
    /// object-end terminator; a UI32 length prefix delimits the run.
    StrictArray(Vec<MetaValue>),
    /// An AMF0 Null value (`0x05`, §2.7). The demuxer flattens it to the
    /// `"null"` sentinel string.
    Null,
    /// An AMF0 Undefined value (`0x06`, §2.8). The demuxer flattens it to
    /// the `"undefined"` sentinel string.
    Undefined,
    /// An AMF0 XMLDocument value (`0x0F`, §2.17). Carries a UTF-8 XML
    /// string; the demuxer surfaces it verbatim under its property name.
    Xml(String),
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

    /// Append an AMF0 Date property (SCRIPTDATADATE, spec §E.4.4.3) and
    /// return `self` for chaining. `time_ms` is the milliseconds-since-
    /// epoch `DateTime`; `tz` is the `LocalDateTimeOffset` in minutes
    /// from UTC (negative west of Greenwich, positive east). Producers
    /// use this for a typed `creationdate` — the demuxer surfaces it as
    /// the `"date:<ms>tz:<offset>"` carrier the regular
    /// [`crate::TypedMetadata::creationdate`] accessor returns and
    /// [`crate::TypedMetadata::creationdate_as_date`] decodes into the
    /// `(time_ms, tz)` pair.
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

    /// Append a nested AMF0 Object property and return `self` for
    /// chaining. The `value` is a [`MetaValue::Object`] carrying its own
    /// ordered `(name, MetaValue)` pairs (themselves possibly nested),
    /// so a producer-defined sub-object — an HDR config block, a
    /// telemetry struct, or the Enhanced-RTMP-v2 per-track info maps —
    /// round-trips back through [`crate::FlvDemuxer`] flattened under
    /// `metadata["<key>.<subkey>"]`. Use [`ObjectBuilder`] to build the
    /// nested value ergonomically:
    ///
    /// ```
    /// use oxideav_flv::script::{MetadataBag, ObjectBuilder};
    /// let bag = MetadataBag::new().object(
    ///     "producerInfo",
    ///     ObjectBuilder::new()
    ///         .string("name", "oxideav")
    ///         .number("buildno", 42.0)
    ///         .build(),
    /// );
    /// assert_eq!(bag.len(), 1);
    /// ```
    pub fn object(mut self, key: &str, value: MetaValue) -> Self {
        self.entries.push((key.to_string(), value));
        self
    }

    /// Append an AMF0 Null property (`0x05`) and return `self`. The
    /// demuxer flattens it to the `"null"` sentinel string.
    pub fn null(mut self, key: &str) -> Self {
        self.entries.push((key.to_string(), MetaValue::Null));
        self
    }

    /// Append an AMF0 Undefined property (`0x06`) and return `self`. The
    /// demuxer flattens it to the `"undefined"` sentinel string.
    pub fn undefined(mut self, key: &str) -> Self {
        self.entries.push((key.to_string(), MetaValue::Undefined));
        self
    }

    /// Append an AMF0 XMLDocument property (`0x0F`) and return `self`.
    /// The demuxer surfaces the XML string verbatim under `key`.
    pub fn xml(mut self, key: &str, s: &str) -> Self {
        self.entries
            .push((key.to_string(), MetaValue::Xml(s.to_string())));
        self
    }

    /// Append an AMF0 strict-array property (`0x0A`) of mixed-type
    /// `items` and return `self`. The demuxer flattens each element under
    /// `metadata["<key>[i]"]`.
    pub fn strict_array(mut self, key: &str, items: Vec<MetaValue>) -> Self {
        self.entries
            .push((key.to_string(), MetaValue::StrictArray(items)));
        self
    }

    /// Append the Enhanced-RTMP-v2 `videoTrackIdInfoMap` property — the
    /// per-track metadata map for the additional (non-default) video
    /// tracks of a multitrack stream (§"Enhancing onMetaData"). The
    /// default track (trackId 0) is described by the top-level fields;
    /// this map keys the variants by trackId 1, 2, …. Each entry's typed
    /// scalars (`width` / `height` / `videodatarate` / `framerate` /
    /// `videocodecid`) are written under the spec property names the
    /// demuxer flattens to `metadata["videotrackidinfomap.<id>.<field>"]`
    /// and [`crate::TypedMetadata::video_track_info_map`] reads back.
    /// trackId 0 is rejected (it is the default track, never a map key).
    pub fn video_track_info_map(mut self, map: &TrackInfoMap) -> Self {
        self.entries.push((
            "videoTrackIdInfoMap".to_string(),
            map.to_meta_value(MediaKind::Video),
        ));
        self
    }

    /// Append the Enhanced-RTMP-v2 `audioTrackIdInfoMap` property — the
    /// audio-side twin of [`Self::video_track_info_map`]. Each entry's
    /// typed scalars (`audiodatarate` / `channels` / `samplerate` /
    /// `audiocodecid`) are written under the spec property names the
    /// demuxer flattens to `metadata["audiotrackidinfomap.<id>.<field>"]`
    /// and [`crate::TypedMetadata::audio_track_info_map`] reads back.
    pub fn audio_track_info_map(mut self, map: &TrackInfoMap) -> Self {
        self.entries.push((
            "audioTrackIdInfoMap".to_string(),
            map.to_meta_value(MediaKind::Audio),
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

/// Ergonomic builder for a [`MetaValue::Object`] — an ordered set of
/// nested `(name, MetaValue)` pairs. Use it to construct producer-defined
/// `onMetaData` sub-objects (HDR config, telemetry, per-track maps) that
/// round-trip back through [`crate::FlvDemuxer`] flattened under
/// `metadata["<prefix>.<key>"]`.
///
/// ```
/// use oxideav_flv::script::ObjectBuilder;
/// let obj = ObjectBuilder::new()
///     .number("width", 1024.0)
///     .number("height", 768.0)
///     .build();
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObjectBuilder {
    entries: Vec<(String, MetaValue)>,
}

impl ObjectBuilder {
    /// An empty object builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a Number child property.
    pub fn number(mut self, key: &str, n: f64) -> Self {
        self.entries.push((key.to_string(), MetaValue::Number(n)));
        self
    }

    /// Append a Boolean child property.
    pub fn boolean(mut self, key: &str, b: bool) -> Self {
        self.entries.push((key.to_string(), MetaValue::Boolean(b)));
        self
    }

    /// Append a String child property.
    pub fn string(mut self, key: &str, s: &str) -> Self {
        self.entries
            .push((key.to_string(), MetaValue::String(s.to_string())));
        self
    }

    /// Append a Date child property (SCRIPTDATADATE, spec §E.4.4.3).
    pub fn date(mut self, key: &str, time_ms: f64, tz: i16) -> Self {
        self.entries
            .push((key.to_string(), MetaValue::Date { time_ms, tz }));
        self
    }

    /// Append a nested-Object child property.
    pub fn object(mut self, key: &str, value: MetaValue) -> Self {
        self.entries.push((key.to_string(), value));
        self
    }

    /// Append an AMF0 Null child property.
    pub fn null(mut self, key: &str) -> Self {
        self.entries.push((key.to_string(), MetaValue::Null));
        self
    }

    /// Append an AMF0 Undefined child property.
    pub fn undefined(mut self, key: &str) -> Self {
        self.entries.push((key.to_string(), MetaValue::Undefined));
        self
    }

    /// Append an AMF0 XMLDocument child property.
    pub fn xml(mut self, key: &str, s: &str) -> Self {
        self.entries
            .push((key.to_string(), MetaValue::Xml(s.to_string())));
        self
    }

    /// Finish the builder into a [`MetaValue::Object`].
    pub fn build(self) -> MetaValue {
        MetaValue::Object(self.entries)
    }
}

/// Which media side a [`TrackInfoMap`] describes — chooses the spec
/// property names (`videodatarate` vs `audiodatarate`, etc.) emitted for
/// each track entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaKind {
    Video,
    Audio,
}

/// One non-default track's metadata for an Enhanced-RTMP-v2
/// `videoTrackIdInfoMap` / `audioTrackIdInfoMap` entry (§"Enhancing
/// onMetaData"). Every field is `Option`, so a producer may emit a
/// delta-style entry (only the fields that differ from the default
/// track) or a full per-track descriptor — both are spec-valid. Build
/// one with the chained setters; the relevant fields are written under
/// the spec property names the demuxer flattens and
/// [`crate::TypedVideoTrackInfo`] / [`crate::TypedAudioTrackInfo`] read
/// back. Setting a field that belongs to the other media kind is a
/// no-op on emit (a video-only `channels`, say, is simply not written).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackInfo {
    /// Video: pixel width (`width`).
    pub width: Option<u32>,
    /// Video: pixel height (`height`).
    pub height: Option<u32>,
    /// Video: frames per second (`framerate`).
    pub framerate: Option<f64>,
    /// Video: bitrate in kbps (`videodatarate`).
    pub video_data_rate_kbps: Option<f64>,
    /// Video: codec id — a legacy CodecID nibble or a FourCc UI32
    /// (`videocodecid`).
    pub video_codec_id: Option<u32>,
    /// Audio: bitrate in kbps (`audiodatarate`).
    pub audio_data_rate_kbps: Option<f64>,
    /// Audio: channel count (`channels`).
    pub channels: Option<u32>,
    /// Audio: sample rate in Hz (`samplerate` — the per-track spelling,
    /// distinct from the top-level `audiosamplerate`).
    pub audio_sample_rate: Option<f64>,
    /// Audio: codec id — a legacy CodecID nibble or a FourCc UI32
    /// (`audiocodecid`).
    pub audio_codec_id: Option<u32>,
}

impl TrackInfo {
    /// An all-absent (delta-style empty) track entry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the video pixel width.
    pub fn width(mut self, w: u32) -> Self {
        self.width = Some(w);
        self
    }

    /// Set the video pixel height.
    pub fn height(mut self, h: u32) -> Self {
        self.height = Some(h);
        self
    }

    /// Set the video frame rate (fps).
    pub fn framerate(mut self, fps: f64) -> Self {
        self.framerate = Some(fps);
        self
    }

    /// Set the video bitrate (kbps).
    pub fn video_data_rate_kbps(mut self, kbps: f64) -> Self {
        self.video_data_rate_kbps = Some(kbps);
        self
    }

    /// Set the video codec id (legacy nibble or FourCc UI32).
    pub fn video_codec_id(mut self, id: u32) -> Self {
        self.video_codec_id = Some(id);
        self
    }

    /// Set the audio bitrate (kbps).
    pub fn audio_data_rate_kbps(mut self, kbps: f64) -> Self {
        self.audio_data_rate_kbps = Some(kbps);
        self
    }

    /// Set the audio channel count.
    pub fn channels(mut self, c: u32) -> Self {
        self.channels = Some(c);
        self
    }

    /// Set the audio sample rate (Hz).
    pub fn audio_sample_rate(mut self, hz: f64) -> Self {
        self.audio_sample_rate = Some(hz);
        self
    }

    /// Set the audio codec id (legacy nibble or FourCc UI32).
    pub fn audio_codec_id(mut self, id: u32) -> Self {
        self.audio_codec_id = Some(id);
        self
    }

    /// Lower this entry into the ordered `(name, MetaValue)` pairs for
    /// the chosen media kind, in the spec example's field order. Codec
    /// ids and other integer-valued scalars all serialise as AMF0
    /// Numbers (doubles) per the spec.
    fn to_pairs(&self, kind: MediaKind) -> Vec<(String, MetaValue)> {
        let mut out: Vec<(String, MetaValue)> = Vec::new();
        match kind {
            MediaKind::Video => {
                if let Some(v) = self.width {
                    out.push(("width".into(), MetaValue::Number(v as f64)));
                }
                if let Some(v) = self.height {
                    out.push(("height".into(), MetaValue::Number(v as f64)));
                }
                if let Some(v) = self.video_data_rate_kbps {
                    out.push(("videodatarate".into(), MetaValue::Number(v)));
                }
                if let Some(v) = self.framerate {
                    out.push(("framerate".into(), MetaValue::Number(v)));
                }
                if let Some(v) = self.video_codec_id {
                    out.push(("videocodecid".into(), MetaValue::Number(v as f64)));
                }
            }
            MediaKind::Audio => {
                if let Some(v) = self.audio_data_rate_kbps {
                    out.push(("audiodatarate".into(), MetaValue::Number(v)));
                }
                if let Some(v) = self.channels {
                    out.push(("channels".into(), MetaValue::Number(v as f64)));
                }
                if let Some(v) = self.audio_sample_rate {
                    out.push(("samplerate".into(), MetaValue::Number(v)));
                }
                if let Some(v) = self.audio_codec_id {
                    out.push(("audiocodecid".into(), MetaValue::Number(v as f64)));
                }
            }
        }
        out
    }
}

/// The Enhanced-RTMP-v2 per-track info map (`videoTrackIdInfoMap` /
/// `audioTrackIdInfoMap`, §"Enhancing onMetaData"): an ordered set of
/// `(trackId, TrackInfo)` entries keyed by the non-default trackIds
/// (1, 2, …). The wire shape is an anonymous AMF0 Object whose property
/// names are the decimal trackId strings and whose values are the
/// per-track descriptor sub-objects. Insertion order is preserved on the
/// wire so the round-trip bytes are deterministic.
///
/// ```
/// use oxideav_flv::script::{MetadataBag, TrackInfo, TrackInfoMap};
/// let map = TrackInfoMap::new()
///     .track(1, TrackInfo::new().width(1024).height(768))
///     .track(2, TrackInfo::new().width(3840).height(2160));
/// let bag = MetadataBag::new().video_track_info_map(&map);
/// assert_eq!(bag.len(), 1);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackInfoMap {
    entries: Vec<(u32, TrackInfo)>,
}

impl TrackInfoMap {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or, if `track_id` is already present, append a duplicate
    /// entry for) a non-default track. trackId 0 is the default track —
    /// described by the top-level `onMetaData` fields — and is never a
    /// map key; passing it makes [`MetadataBag::video_track_info_map`] /
    /// [`MetadataBag::audio_track_info_map`] emit a `"0"` key the
    /// demuxer's `TypedMetadata` track iterators deliberately skip, so
    /// callers should start at 1.
    pub fn track(mut self, track_id: u32, info: TrackInfo) -> Self {
        self.entries.push((track_id, info));
        self
    }

    /// Number of track entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no track entries have been added.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Lower the whole map into a [`MetaValue::Object`] keyed by the
    /// decimal trackId strings, each value the per-track descriptor for
    /// the chosen media kind.
    fn to_meta_value(&self, kind: MediaKind) -> MetaValue {
        let pairs = self
            .entries
            .iter()
            .map(|(id, info)| (id.to_string(), MetaValue::Object(info.to_pairs(kind))))
            .collect();
        MetaValue::Object(pairs)
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
        write_meta_value(out, key, value)?;
    }
    amf0::write_object_end(out)?;
    Ok(())
}

/// Serialise a single [`MetaValue`] into `out`. `key` is the property
/// name it was bound to (used only for error context). Recurses for
/// [`MetaValue::Object`] so arbitrarily nested producer sub-objects —
/// including the per-track info maps — serialise to the AMF0 anonymous
/// Object shape the demuxer's `flatten_amf_value` walk reads back.
fn write_meta_value(out: &mut Vec<u8>, key: &str, value: &MetaValue) -> Result<()> {
    match value {
        MetaValue::Number(n) => amf0::write_number(out, *n)?,
        MetaValue::Boolean(b) => amf0::write_boolean(out, *b)?,
        MetaValue::String(s) => amf0::write_string(out, s)?,
        MetaValue::Date { time_ms, tz } => {
            if !time_ms.is_finite() {
                return Err(Error::invalid(format!(
                    "FLV onMetaData Date property {key:?}: \
                     time_ms must be finite (saw {time_ms})"
                )));
            }
            amf0::write_date(out, *time_ms, *tz)?;
        }
        MetaValue::Keyframes {
            file_positions,
            times_seconds,
        } => write_keyframes_object(out, file_positions, times_seconds)?,
        MetaValue::Object(pairs) => {
            amf0::write_object_start(out)?;
            for (k, v) in pairs {
                amf0::write_property_name(out, k)?;
                write_meta_value(out, k, v)?;
            }
            amf0::write_object_end(out)?;
        }
        MetaValue::EcmaArray(pairs) => {
            amf0::write_ecma_array_start(out, pairs.len() as u32)?;
            for (k, v) in pairs {
                amf0::write_property_name(out, k)?;
                write_meta_value(out, k, v)?;
            }
            amf0::write_object_end(out)?;
        }
        MetaValue::StrictArray(items) => {
            if items.len() > u32::MAX as usize {
                return Err(Error::invalid(format!(
                    "FLV onMetaData strict-array {key:?}: length {} exceeds UI32 max",
                    items.len()
                )));
            }
            amf0::write_strict_array_start(out, items.len() as u32)?;
            for v in items {
                write_meta_value(out, key, v)?;
            }
        }
        MetaValue::Null => amf0::write_null(out)?,
        MetaValue::Undefined => amf0::write_undefined(out)?,
        MetaValue::Xml(s) => amf0::write_xml(out, s)?,
    }
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
/// * `parameters` — an anonymous Object of user-defined name → value
///   pairs (Annex A models cue parameters as a free-form bag; the
///   demuxer surfaces them under `cuepoint.<n>.parameters.<key>` via
///   `flatten_amf_value`, which handles any AMF0 value type).
///
/// String parameters are the common case (chapter markers, ad-insertion
/// triggers, captions) and have a dedicated `parameter(name, &str)`
/// fast-path setter. Producers needing typed parameters (a Number
/// duration, a Boolean flag, a nested Object) use the
/// [`Self::parameter_typed`] setter with any [`MetaValue`]; both kinds
/// emit into the same `parameters` Object in insertion order, and the
/// demuxer's flatten walk round-trips them under
/// `metadata["cuepoint.<n>.parameters.<key>"]` regardless of type.
#[derive(Clone, Debug, PartialEq)]
pub struct CuePointParams {
    /// Producer-assigned identifier (`name` property).
    pub name: String,
    /// Cue time in seconds (`time` property).
    pub time_seconds: f64,
    /// Event / Navigation (`type` property).
    pub kind: CuePointType,
    /// User-defined parameter pairs (`parameters` property), each a
    /// `(name, MetaValue)`. A string parameter is stored as
    /// [`MetaValue::String`]; richer types ride the same vec. Empty when
    /// the cue carries no extra parameters. Insertion order is preserved
    /// on the wire so the round-trip is deterministic.
    pub parameters: Vec<(String, MetaValue)>,
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

    /// Append a user String parameter `(name, value)` pair — the common
    /// case. Equivalent to `parameter_typed(name, MetaValue::String(..))`.
    pub fn parameter(mut self, name: &str, value: &str) -> Self {
        self.parameters
            .push((name.to_string(), MetaValue::String(value.to_string())));
        self
    }

    /// Append a typed user parameter `(name, value)` pair, where `value`
    /// is any [`MetaValue`] (Number / Boolean / Date / nested Object /
    /// …). The demuxer flattens it under
    /// `metadata["cuepoint.<n>.parameters.<name>"]` (composite values
    /// fan out with `.<subkey>` / `[i]` suffixes).
    pub fn parameter_typed(mut self, name: &str, value: MetaValue) -> Self {
        self.parameters.push((name.to_string(), value));
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
        write_meta_value(out, k, v)?;
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
    fn date_property_round_trips_through_parser() {
        // creationdate stamped as an AMF0 Date (2025-01-01T00:00:00Z,
        // JST +540 min) rather than a free-form string.
        let bag = MetadataBag::new().number("duration", 2.0).date(
            "creationdate",
            1_735_689_600_000.0,
            540,
        );
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();

        let (name, p) = parse_amf0_value(&body, 0).unwrap();
        assert_eq!(name, AmfValue::String("onMetaData".into()));
        let (value, np) = parse_amf0_value(&body, p).unwrap();
        assert_eq!(np, body.len());
        match value {
            AmfValue::EcmaArray(props) => {
                assert_eq!(props.len(), 2);
                assert_eq!(props[0], ("duration".into(), AmfValue::Number(2.0)));
                assert_eq!(
                    props[1],
                    (
                        "creationdate".into(),
                        AmfValue::Date {
                            time_ms: 1_735_689_600_000.0,
                            tz: 540
                        }
                    )
                );
            }
            other => panic!("expected ecma array, got {other:?}"),
        }
    }

    #[test]
    fn date_property_rejects_non_finite_time() {
        let bag = MetadataBag::new().date("creationdate", f64::NAN, 0);
        let mut body = Vec::new();
        assert!(matches!(
            write_on_metadata_body(&mut body, &bag),
            Err(Error::InvalidData(_))
        ));
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
    fn on_cue_point_typed_parameters_round_trip_through_parser() {
        // A mix of typed parameters: a Number duration, a Boolean flag,
        // and a nested Object, alongside the legacy string fast-path.
        let cue = CuePointParams::new("ad-break", 30.0, CuePointType::Event)
            .parameter("label", "midroll")
            .parameter_typed("duration", MetaValue::Number(15.0))
            .parameter_typed("skippable", MetaValue::Boolean(true))
            .parameter_typed(
                "meta",
                ObjectBuilder::new().string("campaign", "summer").build(),
            );
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
            other => panic!("parameters: expected object, got {other:?}"),
        };
        assert_eq!(params.len(), 4);
        assert_eq!(
            params[0],
            ("label".into(), AmfValue::String("midroll".into()))
        );
        assert_eq!(params[1], ("duration".into(), AmfValue::Number(15.0)));
        assert_eq!(params[2], ("skippable".into(), AmfValue::Boolean(true)));
        match &params[3].1 {
            AmfValue::Object(inner) => {
                assert_eq!(
                    inner[0],
                    ("campaign".into(), AmfValue::String("summer".into()))
                );
            }
            other => panic!("meta: expected object, got {other:?}"),
        }
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

    // Helper: parse the `onMetaData` body and return the ECMA-array
    // properties (skipping the name string).
    fn parse_meta_props(body: &[u8]) -> Vec<(String, AmfValue)> {
        let (name, p) = parse_amf0_value(body, 0).unwrap();
        assert_eq!(name, AmfValue::String("onMetaData".into()));
        let (value, np) = parse_amf0_value(body, p).unwrap();
        assert_eq!(np, body.len(), "ECMA array must consume the body exactly");
        match value {
            AmfValue::EcmaArray(props) => props,
            other => panic!("expected ecma array, got {other:?}"),
        }
    }

    #[test]
    fn nested_object_property_round_trips_through_parser() {
        let bag = MetadataBag::new().number("duration", 2.0).object(
            "producerInfo",
            ObjectBuilder::new()
                .string("name", "oxideav")
                .number("buildno", 42.0)
                .build(),
        );
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();

        let props = parse_meta_props(&body);
        assert_eq!(props.len(), 2);
        assert_eq!(props[0], ("duration".into(), AmfValue::Number(2.0)));
        match &props[1] {
            (k, AmfValue::Object(inner)) if k == "producerInfo" => {
                assert_eq!(
                    inner[0],
                    ("name".into(), AmfValue::String("oxideav".into()))
                );
                assert_eq!(inner[1], ("buildno".into(), AmfValue::Number(42.0)));
            }
            other => panic!("expected producerInfo object, got {other:?}"),
        }
    }

    #[test]
    fn nested_object_recurses() {
        let bag = MetadataBag::new().object(
            "outer",
            ObjectBuilder::new()
                .object("inner", ObjectBuilder::new().number("leaf", 7.0).build())
                .build(),
        );
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();
        let props = parse_meta_props(&body);
        let outer = match &props[0] {
            (k, AmfValue::Object(b)) if k == "outer" => b,
            other => panic!("expected outer, got {other:?}"),
        };
        let inner = match &outer[0] {
            (k, AmfValue::Object(b)) if k == "inner" => b,
            other => panic!("expected inner, got {other:?}"),
        };
        assert_eq!(inner[0], ("leaf".into(), AmfValue::Number(7.0)));
    }

    #[test]
    fn video_track_info_map_emits_spec_property_names_and_keys() {
        // Mirrors the spec example (§"Enhancing onMetaData"): trackId 1
        // is a full descriptor, trackId 2 a delta-style entry.
        let map = TrackInfoMap::new()
            .track(
                1,
                TrackInfo::new()
                    .width(1024)
                    .height(768)
                    .video_data_rate_kbps(2000.0)
                    .video_codec_id(1_635_135_537), // makeFourCc("av01")
            )
            .track(2, TrackInfo::new().width(3840).height(2160));
        let bag = MetadataBag::new().video_track_info_map(&map);
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();

        let props = parse_meta_props(&body);
        assert_eq!(props.len(), 1);
        let outer = match &props[0] {
            (k, AmfValue::Object(b)) if k == "videoTrackIdInfoMap" => b,
            other => panic!("expected videoTrackIdInfoMap, got {other:?}"),
        };
        // Keys are decimal trackId strings in insertion order.
        assert_eq!(outer[0].0, "1");
        assert_eq!(outer[1].0, "2");
        let t1 = match &outer[0].1 {
            AmfValue::Object(b) => b,
            other => panic!("expected track1 object, got {other:?}"),
        };
        assert_eq!(t1[0], ("width".into(), AmfValue::Number(1024.0)));
        assert_eq!(t1[1], ("height".into(), AmfValue::Number(768.0)));
        assert_eq!(t1[2], ("videodatarate".into(), AmfValue::Number(2000.0)));
        assert_eq!(
            t1[3],
            ("videocodecid".into(), AmfValue::Number(1_635_135_537.0))
        );
        // Delta-style track 2 carries only width / height.
        let t2 = match &outer[1].1 {
            AmfValue::Object(b) => b,
            other => panic!("expected track2 object, got {other:?}"),
        };
        assert_eq!(t2.len(), 2);
        assert_eq!(t2[0], ("width".into(), AmfValue::Number(3840.0)));
        assert_eq!(t2[1], ("height".into(), AmfValue::Number(2160.0)));
    }

    #[test]
    fn audio_track_info_map_emits_audio_property_names() {
        let map = TrackInfoMap::new().track(
            1,
            TrackInfo::new()
                .audio_data_rate_kbps(256.0)
                .channels(2)
                .audio_sample_rate(44_100.0)
                .audio_codec_id(1_297_377_380), // makeFourCc("Mp4a")-style UI32
        );
        let bag = MetadataBag::new().audio_track_info_map(&map);
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();
        let props = parse_meta_props(&body);
        let outer = match &props[0] {
            (k, AmfValue::Object(b)) if k == "audioTrackIdInfoMap" => b,
            other => panic!("expected audioTrackIdInfoMap, got {other:?}"),
        };
        let t1 = match &outer[0].1 {
            AmfValue::Object(b) => b,
            other => panic!("expected track1 object, got {other:?}"),
        };
        assert_eq!(t1[0], ("audiodatarate".into(), AmfValue::Number(256.0)));
        assert_eq!(t1[1], ("channels".into(), AmfValue::Number(2.0)));
        assert_eq!(t1[2], ("samplerate".into(), AmfValue::Number(44_100.0)));
        assert_eq!(t1[3].0, "audiocodecid");
    }

    #[test]
    fn track_info_map_ignores_cross_kind_fields() {
        // A `channels` set on a *video* entry must not be written
        // (it belongs to the audio side); and vice-versa.
        let map = TrackInfoMap::new().track(1, TrackInfo::new().width(640).channels(6));
        let bag = MetadataBag::new().video_track_info_map(&map);
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();
        let props = parse_meta_props(&body);
        let outer = match &props[0] {
            (_, AmfValue::Object(b)) => b,
            other => panic!("expected object, got {other:?}"),
        };
        let t1 = match &outer[0].1 {
            AmfValue::Object(b) => b,
            other => panic!("expected track object, got {other:?}"),
        };
        // Only `width` survives — `channels` is an audio field.
        assert_eq!(t1.len(), 1);
        assert_eq!(t1[0].0, "width");
    }

    #[test]
    fn empty_track_info_map_emits_empty_object() {
        let bag = MetadataBag::new().video_track_info_map(&TrackInfoMap::new());
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();
        let props = parse_meta_props(&body);
        match &props[0] {
            (k, AmfValue::Object(b)) if k == "videoTrackIdInfoMap" => assert!(b.is_empty()),
            other => panic!("expected empty videoTrackIdInfoMap, got {other:?}"),
        }
    }

    #[test]
    fn null_undefined_xml_round_trip_through_parser() {
        let bag = MetadataBag::new()
            .null("a")
            .undefined("b")
            .xml("c", "<x>hi</x>");
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();
        let props = parse_meta_props(&body);
        assert_eq!(props[0], ("a".into(), AmfValue::Null));
        assert_eq!(props[1], ("b".into(), AmfValue::Undefined));
        assert_eq!(props[2], ("c".into(), AmfValue::Xml("<x>hi</x>".into())));
    }

    #[test]
    fn ecma_array_value_round_trips_through_parser() {
        let bag = MetadataBag::new().object(
            "wrapped",
            MetaValue::EcmaArray(vec![
                ("k1".into(), MetaValue::Number(1.0)),
                ("k2".into(), MetaValue::String("v".into())),
            ]),
        );
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();
        let props = parse_meta_props(&body);
        match &props[0] {
            (k, AmfValue::EcmaArray(inner)) if k == "wrapped" => {
                assert_eq!(inner[0], ("k1".into(), AmfValue::Number(1.0)));
                assert_eq!(inner[1], ("k2".into(), AmfValue::String("v".into())));
            }
            other => panic!("expected ecma array, got {other:?}"),
        }
    }

    #[test]
    fn strict_array_mixed_values_round_trips_through_parser() {
        let bag = MetadataBag::new().strict_array(
            "list",
            vec![
                MetaValue::Number(3.0),
                MetaValue::String("x".into()),
                MetaValue::Boolean(true),
                MetaValue::Null,
            ],
        );
        let mut body = Vec::new();
        write_on_metadata_body(&mut body, &bag).unwrap();
        let props = parse_meta_props(&body);
        match &props[0] {
            (k, AmfValue::StrictArray(items)) if k == "list" => {
                assert_eq!(items.len(), 4);
                assert_eq!(items[0], AmfValue::Number(3.0));
                assert_eq!(items[1], AmfValue::String("x".into()));
                assert_eq!(items[2], AmfValue::Boolean(true));
                assert_eq!(items[3], AmfValue::Null);
            }
            other => panic!("expected strict array, got {other:?}"),
        }
    }
}
