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
}
