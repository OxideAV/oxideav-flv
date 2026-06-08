//! Typed read-side accessor for the fifteen spec-defined `onMetaData`
//! properties of Annex E.5.
//!
//! The Adobe *Video File Format Specification* v10.1, Annex E.5
//! ("onMetaData") enumerates a fixed table of property names with
//! known AMF types:
//!
//! | Property         | AMF type | Comment                                         |
//! |------------------|----------|-------------------------------------------------|
//! | `audiocodecid`   | Number   | Audio codec ID (E.4.2.1 SoundFormat value)      |
//! | `audiodatarate`  | Number   | Audio bit rate in kbit/s                        |
//! | `audiodelay`     | Number   | Audio codec delay, in seconds                   |
//! | `audiosamplerate`| Number   | Audio sample rate in Hz                         |
//! | `audiosamplesize`| Number   | Audio bit depth (bits per sample)               |
//! | `canSeekToEnd`   | Boolean  | True when the last video frame is a key frame   |
//! | `creationdate`   | String   | Creation date and time (free-form string)       |
//! | `duration`       | Number   | Total presentation duration in seconds          |
//! | `filesize`       | Number   | Total file size in bytes                        |
//! | `framerate`      | Number   | Frames per second                               |
//! | `height`         | Number   | Video height in pixels                          |
//! | `stereo`         | Boolean  | True when the audio is stereo                   |
//! | `videocodecid`   | Number   | Video codec ID (E.4.3.1 CodecID value)          |
//! | `videodatarate`  | Number   | Video bit rate in kbit/s                        |
//! | `width`          | Number   | Video width in pixels                           |
//!
//! [`FlvDemuxer`](crate::FlvDemuxer) flattens the AMF0 `onMetaData`
//! payload into a `Vec<(String, String)>` bag — every value becomes a
//! string in a known format ([`crate::demuxer`] internals: Number →
//! `format_number`, Boolean → `"true"`/`"false"`, String → the raw
//! string, Date → `"date:<ms>tz:<offset>"`). The string bag is the
//! framework's `Demuxer::metadata` shape so it surfaces uniformly
//! across every container; this module re-types the Annex E.5 fifteen
//! back into their declared AMF types so callers don't have to
//! re-parse the strings.
//!
//! [`TypedMetadata`] holds a borrow of the bag and exposes each
//! property as an accessor returning `Option<T>` (or
//! `Result<Option<T>, _>` where the spec's declared type admits a
//! non-trivial sub-format, e.g. `creationdate`'s mixed
//! string-or-Date storage). Missing or malformed entries return
//! `None` — the accessor never panics on bag contents, no matter
//! how the producer wrote them.
//!
//! ```
//! use oxideav_flv::typed_meta::TypedMetadata;
//! let bag = vec![
//!     ("duration".to_string(), "12.5".to_string()),
//!     ("width".to_string(), "1920".to_string()),
//!     ("height".to_string(), "1080".to_string()),
//!     ("stereo".to_string(), "true".to_string()),
//!     ("audiosamplerate".to_string(), "48000".to_string()),
//! ];
//! let meta = TypedMetadata::new(&bag);
//! assert_eq!(meta.duration(), Some(12.5));
//! assert_eq!(meta.width(), Some(1920));
//! assert_eq!(meta.height(), Some(1080));
//! assert_eq!(meta.stereo(), Some(true));
//! assert_eq!(meta.audio_sample_rate(), Some(48_000.0));
//! ```

use crate::tag::{audio_codec_id_str, video_codec_id_str};

/// Borrowed view over the `Demuxer::metadata` bag that re-types the
/// Annex E.5 fifteen well-known properties back into their declared
/// AMF types.
///
/// Construction is just `TypedMetadata::new(bag)` — every accessor is
/// an O(n) linear scan of the bag for `n` typically below 30, which
/// is well under the bag-rebuild cost. Callers that hit the same
/// property repeatedly should cache the returned value.
///
/// All accessors return `Option<T>`:
///
/// * `None` when the property is absent from the bag.
/// * `None` when the value's string form doesn't parse back into the
///   declared AMF type (e.g. `width` stored as a non-numeric string,
///   `stereo` stored as something other than `"true"` / `"false"`).
///
/// The non-fallible `Option` shape matches the framework convention
/// that container metadata is advisory: producers may omit any
/// property, write garbage, or signal a contradictory value, and the
/// caller is expected to fall back to bitstream-level signals.
#[derive(Clone, Copy, Debug)]
pub struct TypedMetadata<'a> {
    bag: &'a [(String, String)],
}

impl<'a> TypedMetadata<'a> {
    /// Wrap a borrowed bag in a typed view. Zero-cost — the bag is
    /// not copied or scanned at construction time.
    pub fn new(bag: &'a [(String, String)]) -> Self {
        Self { bag }
    }

    /// The underlying bag the view was created from. Provided for
    /// callers that already have a [`TypedMetadata`] in hand and want
    /// the raw key/value pairs without re-fetching from the demuxer.
    pub fn as_pairs(&self) -> &'a [(String, String)] {
        self.bag
    }

    // ------------------------------------------------------------ Number

    /// `duration` — total presentation length in seconds (Annex E.5).
    ///
    /// AMF type: Number. Negative values are out of range per the
    /// spec ("total duration") and return `None`; non-finite (NaN /
    /// infinity) values likewise return `None`.
    pub fn duration(&self) -> Option<f64> {
        self.lookup_finite_f64("duration")
            .filter(|n| n.is_sign_positive() || *n == 0.0)
    }

    /// `filesize` — total file size in bytes (Annex E.5).
    ///
    /// AMF type: Number. Returns the value as `u64` after the same
    /// finite + non-negative gating as [`Self::duration`]. Producers
    /// who write a sentinel `0` (the size isn't known yet at mux
    /// time) flow through as `Some(0)`; consumers can treat that as
    /// "unknown" if they prefer.
    pub fn filesize(&self) -> Option<u64> {
        let n = self.lookup_finite_f64("filesize")?;
        if !(0.0..=u64::MAX as f64).contains(&n) {
            return None;
        }
        Some(n as u64)
    }

    /// `width` — video width in pixels (Annex E.5).
    ///
    /// AMF type: Number. The spec lists this on the same line as
    /// `height` so the same range gating applies: finite,
    /// non-negative, and below `u32::MAX`. Producers occasionally
    /// write fractional values (1920.0); the accessor accepts those
    /// and truncates via the same path as the demuxer's internal
    /// `metadata_lookup_u32`.
    pub fn width(&self) -> Option<u32> {
        self.lookup_u32("width")
    }

    /// `height` — video height in pixels (Annex E.5).
    pub fn height(&self) -> Option<u32> {
        self.lookup_u32("height")
    }

    /// `framerate` — frames per second (Annex E.5).
    ///
    /// AMF type: Number. The spec doesn't say whether this is
    /// `videoframerate` or the older `framerate` — both are observed
    /// in the wild — so this accessor returns the spec-named
    /// `framerate` property only. Callers wanting the producer's
    /// effective frame rate should reach for
    /// [`Self::effective_framerate`], which consults
    /// [`Self::videoframerate`] first (the Annex B.1 alias the
    /// demuxer prefers when both keys are present) and falls back to
    /// the spec-named `framerate`.
    pub fn framerate(&self) -> Option<f64> {
        self.lookup_finite_f64("framerate")
    }

    /// `videoframerate` — the producer-stamped frame rate under the
    /// Annex B.1 alias (the de-facto property name emitted by every
    /// post-2008 Flash-era producer).
    ///
    /// AMF type: Number. The spec's Annex E.5 fixed table lists only
    /// `framerate`, but Annex B.1 of the Adobe *Video File Format
    /// Specification* (the "Metadata Tags" appendix that catalogues
    /// the conventional property names extant on the wire) declares
    /// `videoframerate` as the preferred carrier; the demuxer reads
    /// it first and falls back to `framerate` when absent. This
    /// accessor surfaces the raw `videoframerate` value verbatim so
    /// callers can choose their own preference order; reach for
    /// [`Self::effective_framerate`] when the demuxer's
    /// alias-first-then-fallback shape is wanted instead.
    pub fn videoframerate(&self) -> Option<f64> {
        self.lookup_finite_f64("videoframerate")
    }

    /// Effective frame rate — [`Self::videoframerate`] when present,
    /// otherwise [`Self::framerate`].
    ///
    /// Mirrors the alias preference the demuxer uses when lifting
    /// `frame_rate` into [`oxideav_core::CodecParameters`]: the
    /// Annex B.1 `videoframerate` alias is preferred (and is what
    /// every modern producer stamps), with the legacy Annex E.5
    /// `framerate` as the spec-named fallback. Returns `None` when
    /// neither key is present in the bag or when both fail finite
    /// parsing.
    pub fn effective_framerate(&self) -> Option<f64> {
        self.videoframerate().or_else(|| self.framerate())
    }

    /// `videodatarate` — video bit rate in kbit/s (Annex E.5).
    pub fn video_data_rate_kbps(&self) -> Option<f64> {
        self.lookup_finite_f64("videodatarate")
    }

    /// `audiodatarate` — audio bit rate in kbit/s (Annex E.5).
    pub fn audio_data_rate_kbps(&self) -> Option<f64> {
        self.lookup_finite_f64("audiodatarate")
    }

    /// `audiosamplerate` — audio sample rate in Hz (Annex E.5).
    ///
    /// AMF type: Number. Producers nearly always write a discrete
    /// value (44_100, 48_000, …) but the spec does not constrain
    /// the range, so any finite non-negative value flows through.
    pub fn audio_sample_rate(&self) -> Option<f64> {
        self.lookup_finite_f64("audiosamplerate")
            .filter(|n| *n >= 0.0)
    }

    /// `audiosamplesize` — audio bit depth (bits per sample) per
    /// Annex E.5.
    ///
    /// AMF type: Number. Adobe's encoders only ever emit `8` or `16`
    /// here (the legacy `SoundSize` field in E.4.2.1 encodes only
    /// those two values), but the accessor passes the raw integer
    /// through — callers that need the stricter
    /// `8` / `16`-only behaviour should match on the returned
    /// value.
    pub fn audio_sample_size(&self) -> Option<u32> {
        self.lookup_u32("audiosamplesize")
    }

    /// `audiodelay` — audio codec delay, in seconds (Annex E.5).
    ///
    /// AMF type: Number. Maps the spec's "Delay introduced by the
    /// audio codec in seconds" verbatim — typical Flash-era values
    /// are around `0.038` s for AAC priming.
    pub fn audio_delay_seconds(&self) -> Option<f64> {
        self.lookup_finite_f64("audiodelay")
    }

    /// `videocodecid` — the E.4.3.1 video CodecID stamped by the
    /// producer (Annex E.5).
    ///
    /// AMF type: Number. Returns the raw integer; callers wanting
    /// the stable string form ("flv1", "vp6f", "h264", …) should
    /// pass the result through [`Self::video_codec_id_str`].
    pub fn video_codec_id(&self) -> Option<u32> {
        self.lookup_u32("videocodecid")
    }

    /// `audiocodecid` — the E.4.2.1 audio SoundFormat stamped by the
    /// producer (Annex E.5).
    pub fn audio_codec_id(&self) -> Option<u32> {
        self.lookup_u32("audiocodecid")
    }

    /// Convenience: [`Self::video_codec_id`] mapped through the
    /// stable [`crate::tag::video_codec_id_str`] table — `"flv1"`,
    /// `"vp6f"`, `"h264"`, …
    ///
    /// Returns `None` when `videocodecid` is absent or malformed.
    /// Unknown legacy ids (0..=15) flow through as
    /// `flv:video:<N>`; out-of-range producer-stamped values
    /// (anything `> 0x0F`) likewise flow through the
    /// `flv:video:<N>` fallback so the caller still sees the raw
    /// number rather than `None`.
    pub fn video_codec_id_str(&self) -> Option<String> {
        let id = self.video_codec_id()?;
        // Producers occasionally stamp the FourCc as an integer via
        // `makeFourCc()` (e.g. 1635135537 == "av01"); the helper's
        // u8 input mask trims those down to the low byte. Forward
        // the raw value through the formatter so the caller sees
        // exactly what the bag held.
        if id > u8::MAX as u32 {
            return Some(format!("flv:video:{id}"));
        }
        Some(video_codec_id_str(id as u8))
    }

    /// Convenience: [`Self::audio_codec_id`] mapped through the
    /// stable [`crate::tag::audio_codec_id_str`] table — `"mp3"`,
    /// `"aac"`, `"speex"`, …
    pub fn audio_codec_id_str(&self) -> Option<String> {
        let id = self.audio_codec_id()?;
        if id > u8::MAX as u32 {
            return Some(format!("flv:audio:{id}"));
        }
        Some(audio_codec_id_str(id as u8))
    }

    // ----------------------------------------------------------- Boolean

    /// `stereo` — true when the audio stream is stereo (Annex E.5).
    ///
    /// AMF type: Boolean. The bag stores `"true"` / `"false"`; any
    /// other string (the property is absent, or a producer wrote
    /// e.g. `"1"`) returns `None`.
    pub fn stereo(&self) -> Option<bool> {
        self.lookup_bool("stereo")
    }

    /// `canSeekToEnd` — true when the last video frame in the file
    /// is a keyframe (Annex E.5).
    ///
    /// AMF type: Boolean. The spec gives the property in
    /// camelCase; the demuxer preserves whatever the producer
    /// wrote, so this accessor accepts the spec form
    /// (`canSeekToEnd`) only. Producers who write
    /// `canseektoend` (all-lowercase) will not be matched here —
    /// add an alias scan if a fixture demands it.
    pub fn can_seek_to_end(&self) -> Option<bool> {
        self.lookup_bool("canSeekToEnd")
    }

    // ------------------------------------------------------------ String

    /// `creationdate` — free-form creation date and time string
    /// (Annex E.5).
    ///
    /// AMF type: String. Annex E.5 specifies String here, but in
    /// the wild some producers stamp the field as an AMF0 `Date`
    /// (marker 0x0B, §2.13: `DOUBLE` ms-since-epoch + `INT16` UTC
    /// offset minutes); the demuxer surfaces a Date as
    /// `"date:<ms>tz:<offset>"` to preserve the timestamp losslessly.
    /// This accessor returns the raw string form — callers can
    /// match on the `"date:"` prefix to detect the Date carrier and
    /// reach for [`Self::creationdate_as_date`] for a structured
    /// view.
    pub fn creationdate(&self) -> Option<&'a str> {
        self.lookup_str("creationdate")
    }

    /// Structured view of `creationdate` when the producer stamped
    /// it as an AMF0 `Date` rather than a free-form `String`.
    ///
    /// Returns `Some((ms_since_epoch, tz_offset_minutes))` when the
    /// bag carries the `"date:<ms>tz:<offset>"` form the demuxer
    /// uses to encode AMF0 `Date` values into the string bag.
    /// Returns `None` when the property is absent, when it was
    /// stored as a free-form string (no `"date:"` prefix), or when
    /// the encoded form fails to parse — defensive against
    /// downstream callers that may have written into the bag
    /// outside the demuxer's discipline.
    pub fn creationdate_as_date(&self) -> Option<(f64, i16)> {
        let s = self.lookup_str("creationdate")?;
        parse_date_carrier(s)
    }

    // ----------------------------------------------------------- helpers

    fn lookup_str(&self, key: &str) -> Option<&'a str> {
        self.bag
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn lookup_finite_f64(&self, key: &str) -> Option<f64> {
        let v = self.lookup_str(key)?;
        let n: f64 = v.parse().ok()?;
        if !n.is_finite() {
            return None;
        }
        Some(n)
    }

    fn lookup_u32(&self, key: &str) -> Option<u32> {
        let n = self.lookup_finite_f64(key)?;
        if !(0.0..=u32::MAX as f64).contains(&n) {
            return None;
        }
        Some(n as u32)
    }

    fn lookup_bool(&self, key: &str) -> Option<bool> {
        match self.lookup_str(key)? {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    // ----------------------- Enhanced-RTMP-v2 per-track maps -----------------
    //
    // The `videoTrackIdInfoMap` / `audioTrackIdInfoMap` properties (Enhanced
    // RTMP v2 §"Enhancing onMetaData") carry per-track metadata for the
    // additional (non-default) tracks of a multitrack stream. trackId 0 is
    // the default track — its fields live at the top level of onMetaData
    // and are surfaced by the regular accessors above. trackId 1, 2, … are
    // the extras; the demuxer flattens each entry under the prefix
    // `videotrackidinfomap.<N>.` / `audiotrackidinfomap.<N>.` so callers
    // can read the producer's per-track values without an AMF model.
    //
    // The methods below give a typed read of that same flatten — one
    // [`TypedVideoTrackInfo`] / [`TypedAudioTrackInfo`] view per non-zero
    // trackId, with the same `Option<T>` shape as the top-level
    // accessors. Producers may emit delta-style entries (only the fields
    // that differ from the default track); absent fields return `None`.

    /// Borrowed view over the per-track entries of
    /// `videoTrackIdInfoMap`, one [`TypedVideoTrackInfo`] per non-zero
    /// trackId the producer stamped. Iteration order matches the
    /// insertion order of the bag (which mirrors the producer's AMF
    /// object key order). Returns an empty iterator when the producer
    /// didn't emit a `videoTrackIdInfoMap`.
    pub fn video_track_info_map(&self) -> TrackInfoIter<'a, TypedVideoTrackInfo<'a>> {
        TrackInfoIter::new(self.bag, "videotrackidinfomap.", TypedVideoTrackInfo::new)
    }

    /// Look up a single video per-track entry by `trackId`. trackId 0
    /// is the default track (its fields live at the top level of
    /// onMetaData and are surfaced by the regular accessors above); the
    /// per-track map carries trackId 1, 2, …
    ///
    /// Returns `None` when the producer didn't emit a
    /// `videoTrackIdInfoMap` at all OR when the requested trackId is
    /// absent from the map. trackId 0 always returns `None` here — it
    /// would shadow the top-level fields; callers should read those
    /// directly via [`Self::width`] / [`Self::height`] / etc.
    pub fn video_track_info(&self, track_id: u32) -> Option<TypedVideoTrackInfo<'a>> {
        if track_id == 0 {
            return None;
        }
        let prefix = format!("videotrackidinfomap.{track_id}.");
        if self.bag.iter().any(|(k, _)| k.starts_with(&prefix)) {
            Some(TypedVideoTrackInfo::new(self.bag, track_id))
        } else {
            None
        }
    }

    /// Borrowed view over the per-track entries of
    /// `audioTrackIdInfoMap`. Twin of [`Self::video_track_info_map`].
    pub fn audio_track_info_map(&self) -> TrackInfoIter<'a, TypedAudioTrackInfo<'a>> {
        TrackInfoIter::new(self.bag, "audiotrackidinfomap.", TypedAudioTrackInfo::new)
    }

    /// Look up a single audio per-track entry by `trackId`. Twin of
    /// [`Self::video_track_info`].
    pub fn audio_track_info(&self, track_id: u32) -> Option<TypedAudioTrackInfo<'a>> {
        if track_id == 0 {
            return None;
        }
        let prefix = format!("audiotrackidinfomap.{track_id}.");
        if self.bag.iter().any(|(k, _)| k.starts_with(&prefix)) {
            Some(TypedAudioTrackInfo::new(self.bag, track_id))
        } else {
            None
        }
    }
}

/// Iterator over the per-track entries of `videoTrackIdInfoMap` /
/// `audioTrackIdInfoMap`. Yields one [`TypedVideoTrackInfo`] /
/// [`TypedAudioTrackInfo`] per non-zero trackId seen in the bag, in
/// bag-insertion order, deduplicated so each trackId is yielded at most
/// once.
#[derive(Debug)]
pub struct TrackInfoIter<'a, T> {
    bag: &'a [(String, String)],
    prefix: &'static str,
    ctor: fn(&'a [(String, String)], u32) -> T,
    seen: Vec<u32>,
    cursor: usize,
}

impl<'a, T> TrackInfoIter<'a, T> {
    fn new(
        bag: &'a [(String, String)],
        prefix: &'static str,
        ctor: fn(&'a [(String, String)], u32) -> T,
    ) -> Self {
        Self {
            bag,
            prefix,
            ctor,
            seen: Vec::new(),
            cursor: 0,
        }
    }
}

impl<'a, T> Iterator for TrackInfoIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        while self.cursor < self.bag.len() {
            let (k, _) = &self.bag[self.cursor];
            self.cursor += 1;
            let rest = match k.strip_prefix(self.prefix) {
                Some(r) => r,
                None => continue,
            };
            // The flattened key is `<prefix><track_id>.<field>`. The
            // demuxer's `flatten_amf_value` walker uses the AMF object's
            // key verbatim as the next path segment, so the trackId is
            // an ASCII decimal between the prefix and the first `.`.
            let id_str = match rest.split_once('.') {
                Some((id, _)) => id,
                None => continue,
            };
            let id: u32 = match id_str.parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            // trackId 0 is the default track; its fields belong at the
            // top level. A producer who nests them under the per-track
            // map anyway shadows the top-level accessors — skip it.
            if id == 0 {
                continue;
            }
            if self.seen.contains(&id) {
                continue;
            }
            self.seen.push(id);
            return Some((self.ctor)(self.bag, id));
        }
        None
    }
}

/// Borrowed view over one video per-track entry inside
/// `videoTrackIdInfoMap`. Each accessor reads the matching
/// `videotrackidinfomap.<track_id>.<field>` key out of the bag and
/// parses it back into the field's spec-declared AMF type. Missing or
/// malformed entries return `None`.
///
/// Producers commonly emit delta-style entries — only the fields that
/// differ from the default track — so an `Option<T>` shape is the
/// honest model. The default track's fields are at the top level of
/// `onMetaData` and are read via the [`TypedMetadata`] accessors
/// above; this view never falls back to the top level.
#[derive(Clone, Copy, Debug)]
pub struct TypedVideoTrackInfo<'a> {
    bag: &'a [(String, String)],
    track_id: u32,
}

impl<'a> TypedVideoTrackInfo<'a> {
    fn new(bag: &'a [(String, String)], track_id: u32) -> Self {
        Self { bag, track_id }
    }

    /// The trackId this view is keyed under (1, 2, …; non-zero by
    /// construction).
    pub fn track_id(&self) -> u32 {
        self.track_id
    }

    fn key(&self, field: &str) -> String {
        format!("videotrackidinfomap.{}.{}", self.track_id, field)
    }

    fn lookup_str(&self, field: &str) -> Option<&'a str> {
        let k = self.key(field);
        self.bag
            .iter()
            .find(|(bk, _)| bk == &k)
            .map(|(_, v)| v.as_str())
    }

    fn lookup_finite_f64(&self, field: &str) -> Option<f64> {
        let n: f64 = self.lookup_str(field)?.parse().ok()?;
        if !n.is_finite() {
            return None;
        }
        Some(n)
    }

    fn lookup_u32(&self, field: &str) -> Option<u32> {
        let n = self.lookup_finite_f64(field)?;
        if !(0.0..=u32::MAX as f64).contains(&n) {
            return None;
        }
        Some(n as u32)
    }

    /// `width` — video width in pixels for this track.
    pub fn width(&self) -> Option<u32> {
        self.lookup_u32("width")
    }

    /// `height` — video height in pixels for this track.
    pub fn height(&self) -> Option<u32> {
        self.lookup_u32("height")
    }

    /// `videodatarate` — video bit rate in kbit/s for this track.
    pub fn video_data_rate_kbps(&self) -> Option<f64> {
        self.lookup_finite_f64("videodatarate")
    }

    /// `videocodecid` — the codec id for this track. The Enhanced-RTMP
    /// spec lets producers stamp this as either the legacy 4-bit
    /// CodecID nibble (E.4.3.1) or the packed FourCc form via
    /// `makeFourCc()` (e.g. `'a''v''0''1' == 0x61763031 == 1_635_135_537`).
    /// Returns the raw integer; callers wanting the string form should
    /// pass the value through the same fallback path as
    /// [`TypedMetadata::video_codec_id_str`].
    pub fn video_codec_id(&self) -> Option<u32> {
        self.lookup_u32("videocodecid")
    }

    /// `framerate` — frames per second for this track. Producers
    /// emitting per-track maps tend to stamp the modern
    /// `videoframerate` alias instead; both are read here, with the
    /// alias preferred (mirroring the demuxer's top-level alias
    /// preference for [`TypedMetadata::effective_framerate`]).
    pub fn framerate(&self) -> Option<f64> {
        self.lookup_finite_f64("videoframerate")
            .or_else(|| self.lookup_finite_f64("framerate"))
    }
}

/// Borrowed view over one audio per-track entry inside
/// `audioTrackIdInfoMap`. Twin of [`TypedVideoTrackInfo`].
#[derive(Clone, Copy, Debug)]
pub struct TypedAudioTrackInfo<'a> {
    bag: &'a [(String, String)],
    track_id: u32,
}

impl<'a> TypedAudioTrackInfo<'a> {
    fn new(bag: &'a [(String, String)], track_id: u32) -> Self {
        Self { bag, track_id }
    }

    /// The trackId this view is keyed under (1, 2, …; non-zero by
    /// construction).
    pub fn track_id(&self) -> u32 {
        self.track_id
    }

    fn key(&self, field: &str) -> String {
        format!("audiotrackidinfomap.{}.{}", self.track_id, field)
    }

    fn lookup_str(&self, field: &str) -> Option<&'a str> {
        let k = self.key(field);
        self.bag
            .iter()
            .find(|(bk, _)| bk == &k)
            .map(|(_, v)| v.as_str())
    }

    fn lookup_finite_f64(&self, field: &str) -> Option<f64> {
        let n: f64 = self.lookup_str(field)?.parse().ok()?;
        if !n.is_finite() {
            return None;
        }
        Some(n)
    }

    fn lookup_u32(&self, field: &str) -> Option<u32> {
        let n = self.lookup_finite_f64(field)?;
        if !(0.0..=u32::MAX as f64).contains(&n) {
            return None;
        }
        Some(n as u32)
    }

    /// `audiodatarate` — audio bit rate in kbit/s for this track.
    pub fn audio_data_rate_kbps(&self) -> Option<f64> {
        self.lookup_finite_f64("audiodatarate")
    }

    /// `samplerate` — audio sample rate in Hz for this track.
    ///
    /// Note that the per-track map uses the spec-shortened name
    /// `samplerate` (rather than the top-level `audiosamplerate`); the
    /// demuxer flattens both verbatim so the accessor matches what the
    /// producer wrote. A `0` or negative value is rejected as
    /// nonsense.
    pub fn audio_sample_rate(&self) -> Option<f64> {
        self.lookup_finite_f64("samplerate").filter(|n| *n >= 0.0)
    }

    /// `channels` — number of audio channels for this track. The
    /// per-track map uses the modern `channels` count directly rather
    /// than the legacy `stereo` boolean from Annex E.5.
    pub fn channels(&self) -> Option<u32> {
        self.lookup_u32("channels")
    }

    /// `audiocodecid` — the codec id for this track. As with
    /// [`TypedVideoTrackInfo::video_codec_id`], either the legacy
    /// 4-bit SoundFormat (E.4.2.1) or a packed FourCc may be stamped.
    pub fn audio_codec_id(&self) -> Option<u32> {
        self.lookup_u32("audiocodecid")
    }
}

/// Parse the `"date:<ms>tz:<offset>"` carrier form used by the demuxer
/// when an AMF0 `Date` value is stored in the string-bag metadata.
/// Returns `None` when the prefix doesn't match or the two numeric
/// fields don't parse cleanly.
fn parse_date_carrier(s: &str) -> Option<(f64, i16)> {
    let rest = s.strip_prefix("date:")?;
    let (ms_str, tz_str) = rest.split_once("tz:")?;
    let ms: f64 = ms_str.parse().ok()?;
    let tz: i16 = tz_str.parse().ok()?;
    if !ms.is_finite() {
        return None;
    }
    Some((ms, tz))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bag(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn number_accessors_decode_back_from_string_bag() {
        let b = bag(&[
            ("duration", "12.5"),
            ("filesize", "12345678"),
            ("width", "1920"),
            ("height", "1080"),
            ("framerate", "29.97"),
            ("videodatarate", "2500"),
            ("audiodatarate", "192"),
            ("audiosamplerate", "48000"),
            ("audiosamplesize", "16"),
            ("audiodelay", "0.038"),
            ("videocodecid", "7"),
            ("audiocodecid", "10"),
        ]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.duration(), Some(12.5));
        assert_eq!(m.filesize(), Some(12_345_678));
        assert_eq!(m.width(), Some(1920));
        assert_eq!(m.height(), Some(1080));
        assert_eq!(m.framerate(), Some(29.97));
        assert_eq!(m.video_data_rate_kbps(), Some(2500.0));
        assert_eq!(m.audio_data_rate_kbps(), Some(192.0));
        assert_eq!(m.audio_sample_rate(), Some(48_000.0));
        assert_eq!(m.audio_sample_size(), Some(16));
        assert_eq!(m.audio_delay_seconds(), Some(0.038));
        assert_eq!(m.video_codec_id(), Some(7));
        assert_eq!(m.audio_codec_id(), Some(10));
        assert_eq!(m.video_codec_id_str().as_deref(), Some("h264"));
        assert_eq!(m.audio_codec_id_str().as_deref(), Some("aac"));
    }

    #[test]
    fn boolean_accessors_decode_true_and_false() {
        let b = bag(&[("stereo", "true"), ("canSeekToEnd", "false")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.stereo(), Some(true));
        assert_eq!(m.can_seek_to_end(), Some(false));
    }

    #[test]
    fn missing_properties_return_none() {
        let b = bag(&[("duration", "1.0")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.width(), None);
        assert_eq!(m.height(), None);
        assert_eq!(m.framerate(), None);
        assert_eq!(m.stereo(), None);
        assert_eq!(m.can_seek_to_end(), None);
        assert_eq!(m.creationdate(), None);
        assert_eq!(m.creationdate_as_date(), None);
    }

    #[test]
    fn malformed_numbers_return_none() {
        let b = bag(&[
            ("duration", "twelve"),
            ("width", "wide"),
            ("height", "-12"),
            ("filesize", "-1"),
            ("framerate", "NaN"),
        ]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.duration(), None);
        assert_eq!(m.width(), None);
        // height is u32 in the spec; a negative number is rejected.
        assert_eq!(m.height(), None);
        assert_eq!(m.filesize(), None);
        // NaN is non-finite — rejected.
        assert_eq!(m.framerate(), None);
    }

    #[test]
    fn duration_rejects_negative_values() {
        let b = bag(&[("duration", "-1.5")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.duration(), None);
    }

    #[test]
    fn malformed_booleans_return_none() {
        // The bag is string-formed, so the canonical encodings are
        // exactly "true" / "false". Anything else returns None — this
        // matches the demuxer's internal `metadata_lookup_bool` so
        // callers get the same answer through both paths.
        let b = bag(&[("stereo", "1"), ("canSeekToEnd", "yes")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.stereo(), None);
        assert_eq!(m.can_seek_to_end(), None);
    }

    #[test]
    fn creationdate_string_form_passes_through() {
        let b = bag(&[("creationdate", "Wed, 01 Jan 2025 00:00:00 GMT")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.creationdate(), Some("Wed, 01 Jan 2025 00:00:00 GMT"));
        // The free-form String form has no Date carrier — structured
        // view is None.
        assert_eq!(m.creationdate_as_date(), None);
    }

    #[test]
    fn creationdate_date_carrier_form_decodes() {
        // `"date:<ms>tz:<offset>"` is the demuxer's carrier for an
        // AMF0 Date value stamped on the creationdate property.
        let b = bag(&[("creationdate", "date:1735689600000tz:540")]);
        let m = TypedMetadata::new(&b);
        // The raw string is still surfaced through .creationdate().
        assert_eq!(m.creationdate(), Some("date:1735689600000tz:540"));
        // And the structured accessor decodes both halves.
        assert_eq!(m.creationdate_as_date(), Some((1_735_689_600_000.0, 540)));
    }

    #[test]
    fn creationdate_malformed_date_carrier_returns_none() {
        let b = bag(&[("creationdate", "date:not-a-numbertz:0")]);
        let m = TypedMetadata::new(&b);
        // The string form still surfaces (the bag holds whatever is
        // there) but the structured view rejects the bad number.
        assert!(m.creationdate().is_some());
        assert_eq!(m.creationdate_as_date(), None);
    }

    #[test]
    fn codec_id_str_passes_unknown_ids_through_fallback() {
        // The legacy CodecID nibble is 4 bits, but the spec text
        // ("CodecID values, see E.4.3.1 / E.4.2.1") does not bar a
        // producer from stamping an unknown id. The string helpers
        // route any unrecognised value through the
        // `flv:video:<N>` / `flv:audio:<N>` fallback so callers
        // still see what was on the wire.
        let b = bag(&[("videocodecid", "100"), ("audiocodecid", "200")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.video_codec_id(), Some(100));
        assert_eq!(m.audio_codec_id(), Some(200));
        assert_eq!(m.video_codec_id_str().as_deref(), Some("flv:video:100"));
        assert_eq!(m.audio_codec_id_str().as_deref(), Some("flv:audio:200"));
    }

    #[test]
    fn codec_id_str_handles_fourcc_packed_ids() {
        // Enhanced-RTMP encoders sometimes stamp `videocodecid` as a
        // packed FourCc via `makeFourCc()` ('a' 'v' '0' '1' →
        // 0x61763031 == 1_635_135_537). The helper trims those down
        // to a `flv:video:<N>` carrier so the caller still sees
        // the raw integer; the FourCc surfaces separately via the
        // Ex video tag header.
        let b = bag(&[("videocodecid", "1635135537")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.video_codec_id(), Some(1_635_135_537));
        assert_eq!(
            m.video_codec_id_str().as_deref(),
            Some("flv:video:1635135537")
        );
    }

    #[test]
    fn as_pairs_returns_the_original_bag() {
        let b = bag(&[("duration", "1.0")]);
        let m = TypedMetadata::new(&b);
        let pairs = m.as_pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "duration");
        assert_eq!(pairs[0].1, "1.0");
    }

    #[test]
    fn videoframerate_alias_is_surfaced_independently_of_framerate() {
        // Annex B.1's `videoframerate` and Annex E.5's `framerate` are
        // two distinct bag keys; the typed view exposes each via its
        // own accessor so callers that want one specifically can read
        // only that.
        let b = bag(&[("videoframerate", "59.94"), ("framerate", "30")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.videoframerate(), Some(59.94));
        assert_eq!(m.framerate(), Some(30.0));
    }

    #[test]
    fn effective_framerate_prefers_videoframerate_then_framerate() {
        // Producer emits both: the alias wins (mirroring the demuxer's
        // own `frame_rate` lift order).
        let both = bag(&[("videoframerate", "29.97"), ("framerate", "30")]);
        assert_eq!(TypedMetadata::new(&both).effective_framerate(), Some(29.97));
        // Alias only.
        let alias_only = bag(&[("videoframerate", "23.976")]);
        assert_eq!(
            TypedMetadata::new(&alias_only).effective_framerate(),
            Some(23.976)
        );
        // Legacy `framerate` only: the fallback fires.
        let legacy_only = bag(&[("framerate", "24")]);
        assert_eq!(
            TypedMetadata::new(&legacy_only).effective_framerate(),
            Some(24.0)
        );
        // Neither: None.
        let none = bag(&[("width", "1920")]);
        assert_eq!(TypedMetadata::new(&none).effective_framerate(), None);
    }

    #[test]
    fn effective_framerate_falls_through_when_alias_is_malformed() {
        // A malformed `videoframerate` (non-finite / unparseable) is
        // treated as absent by the alias accessor, so the fallback to
        // `framerate` kicks in. This matches the
        // `lookup_finite_f64` filter inside each accessor — the
        // typed view is forgiving of producer garbage.
        let nan_alias = bag(&[("videoframerate", "NaN"), ("framerate", "25")]);
        let m = TypedMetadata::new(&nan_alias);
        assert_eq!(m.videoframerate(), None);
        assert_eq!(m.effective_framerate(), Some(25.0));
    }

    #[test]
    fn first_occurrence_wins_on_duplicate_keys() {
        // The bag is an insertion-ordered Vec, not a map. A producer
        // who stamps the same property twice gets the first value
        // (matching the demuxer's `find()`-style scan).
        let b = bag(&[("duration", "1.0"), ("duration", "2.0")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.duration(), Some(1.0));
    }

    #[test]
    fn video_track_info_map_round_trips_demuxer_flatten() {
        // Mirrors the demuxer's `on_metadata_video_track_id_info_map_flattens`
        // fixture: trackId 1 carries width/height/videodatarate/videocodecid,
        // trackId 2 carries width/height only (delta-style entry).
        let b = bag(&[
            ("videotrackidinfomap.1.width", "1024"),
            ("videotrackidinfomap.1.height", "768"),
            ("videotrackidinfomap.1.videodatarate", "2000"),
            ("videotrackidinfomap.1.videocodecid", "1635135537"),
            ("videotrackidinfomap.2.width", "3840"),
            ("videotrackidinfomap.2.height", "2160"),
        ]);
        let m = TypedMetadata::new(&b);
        let tracks: Vec<_> = m.video_track_info_map().collect();
        assert_eq!(tracks.len(), 2);
        // trackId 1 — full entry.
        let t1 = &tracks[0];
        assert_eq!(t1.track_id(), 1);
        assert_eq!(t1.width(), Some(1024));
        assert_eq!(t1.height(), Some(768));
        assert_eq!(t1.video_data_rate_kbps(), Some(2000.0));
        assert_eq!(t1.video_codec_id(), Some(1_635_135_537));
        // trackId 2 — delta entry: width/height only.
        let t2 = &tracks[1];
        assert_eq!(t2.track_id(), 2);
        assert_eq!(t2.width(), Some(3840));
        assert_eq!(t2.height(), Some(2160));
        assert_eq!(t2.video_data_rate_kbps(), None);
        assert_eq!(t2.video_codec_id(), None);
    }

    #[test]
    fn audio_track_info_map_round_trips_demuxer_flatten() {
        // Mirrors the demuxer's `on_metadata_audio_track_id_info_map_flattens`
        // fixture.
        let b = bag(&[
            ("audiotrackidinfomap.1.audiodatarate", "256"),
            ("audiotrackidinfomap.1.channels", "2"),
            ("audiotrackidinfomap.1.samplerate", "44100"),
            ("audiotrackidinfomap.2.audiodatarate", "320"),
            ("audiotrackidinfomap.2.samplerate", "48000"),
        ]);
        let m = TypedMetadata::new(&b);
        let tracks: Vec<_> = m.audio_track_info_map().collect();
        assert_eq!(tracks.len(), 2);
        let t1 = &tracks[0];
        assert_eq!(t1.track_id(), 1);
        assert_eq!(t1.audio_data_rate_kbps(), Some(256.0));
        assert_eq!(t1.channels(), Some(2));
        assert_eq!(t1.audio_sample_rate(), Some(44_100.0));
        let t2 = &tracks[1];
        assert_eq!(t2.track_id(), 2);
        assert_eq!(t2.audio_data_rate_kbps(), Some(320.0));
        assert_eq!(t2.audio_sample_rate(), Some(48_000.0));
        // trackId 2 had no `channels` field — delta-style.
        assert_eq!(t2.channels(), None);
    }

    #[test]
    fn track_info_lookup_by_id_returns_some_only_when_present() {
        let b = bag(&[
            ("videotrackidinfomap.1.width", "1024"),
            ("videotrackidinfomap.1.height", "768"),
        ]);
        let m = TypedMetadata::new(&b);
        // Present.
        let t1 = m.video_track_info(1).expect("trackId 1 should be present");
        assert_eq!(t1.width(), Some(1024));
        // Absent.
        assert!(m.video_track_info(2).is_none());
        // trackId 0 is the default track — never surfaced through the per-track map.
        assert!(m.video_track_info(0).is_none());
    }

    #[test]
    fn track_info_map_empty_when_no_track_keys_in_bag() {
        // A plain onMetaData (no track maps) — the iterators are empty
        // and per-id lookups all return None.
        let b = bag(&[("width", "640"), ("height", "360")]);
        let m = TypedMetadata::new(&b);
        assert_eq!(m.video_track_info_map().count(), 0);
        assert_eq!(m.audio_track_info_map().count(), 0);
        assert!(m.video_track_info(1).is_none());
        assert!(m.audio_track_info(1).is_none());
    }

    #[test]
    fn track_info_iter_deduplicates_per_track_id() {
        // Every per-track entry contributes multiple bag rows (one per
        // field). The iterator must surface each trackId once even
        // though its keys appear multiple times in the bag.
        let b = bag(&[
            ("videotrackidinfomap.1.width", "640"),
            ("videotrackidinfomap.1.height", "360"),
            ("videotrackidinfomap.1.videodatarate", "500"),
            ("videotrackidinfomap.2.width", "1920"),
            ("videotrackidinfomap.2.height", "1080"),
        ]);
        let m = TypedMetadata::new(&b);
        let ids: Vec<u32> = m.video_track_info_map().map(|t| t.track_id()).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn track_info_iter_skips_track_id_zero() {
        // trackId 0 is the default track — its fields belong at the
        // top level of onMetaData, not the per-track map. A producer
        // who stamps it under the map anyway is surfacing redundant
        // data; skip it so the iterator yields only the "extra"
        // tracks.
        let b = bag(&[
            ("videotrackidinfomap.0.width", "999"),
            ("videotrackidinfomap.1.width", "1024"),
        ]);
        let m = TypedMetadata::new(&b);
        let ids: Vec<u32> = m.video_track_info_map().map(|t| t.track_id()).collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn track_info_iter_skips_malformed_track_ids() {
        // A producer who somehow stamps a non-integer trackId would
        // otherwise wedge the iterator; the parse-fail path simply
        // skips those rows and continues.
        let b = bag(&[
            ("videotrackidinfomap.abc.width", "1024"),
            ("videotrackidinfomap.1.width", "640"),
        ]);
        let m = TypedMetadata::new(&b);
        let ids: Vec<u32> = m.video_track_info_map().map(|t| t.track_id()).collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn track_info_framerate_prefers_videoframerate_alias() {
        // Per-track video framerate alias preference mirrors the
        // top-level `effective_framerate` behaviour.
        let alias = bag(&[
            ("videotrackidinfomap.1.videoframerate", "29.97"),
            ("videotrackidinfomap.1.framerate", "30"),
        ]);
        let m = TypedMetadata::new(&alias);
        let t = m.video_track_info(1).unwrap();
        assert_eq!(t.framerate(), Some(29.97));
        // Legacy `framerate` only — fallback fires.
        let legacy = bag(&[("videotrackidinfomap.1.framerate", "24")]);
        assert_eq!(
            TypedMetadata::new(&legacy)
                .video_track_info(1)
                .unwrap()
                .framerate(),
            Some(24.0)
        );
    }

    #[test]
    fn track_info_rejects_malformed_field_values() {
        let b = bag(&[
            ("videotrackidinfomap.1.width", "wide"),
            ("videotrackidinfomap.1.height", "-1"),
            ("videotrackidinfomap.1.videodatarate", "NaN"),
            ("audiotrackidinfomap.1.samplerate", "-44100"),
            ("audiotrackidinfomap.1.channels", "twelve"),
        ]);
        let m = TypedMetadata::new(&b);
        let v = m.video_track_info(1).unwrap();
        assert_eq!(v.width(), None);
        assert_eq!(v.height(), None);
        assert_eq!(v.video_data_rate_kbps(), None);
        let a = m.audio_track_info(1).unwrap();
        assert_eq!(a.audio_sample_rate(), None);
        assert_eq!(a.channels(), None);
    }
}
