//! Typed encode-side wiring for the Enhanced-RTMP-v2
//! `VideoPacketType.Metadata` `colorInfo` HDR object (Veovera
//! `enhanced-rtmp-v2` §"Metadata Frame", §"colorInfo" type block).
//!
//! Producers that mux Enhanced-RTMP video (`hvc1` / `vvc1` / `av01` /
//! `vp09` / `avc1`) need to deliver HDR signalling to the player ahead
//! of the first frame it applies to. The spec gives one carriage: a
//! `videoPacketType = 4` (`Metadata`) Ex video tag whose body is an
//! AMF-encoded series of `[name, value]` pairs, with `["colorInfo",
//! Object]` the only currently-defined pair. The nested object holds a
//! `colorConfig` group (bitDepth + ISO 23091-4 indices), an `hdrCll`
//! group (MaxFALL / MaxCLL), and an `hdrMdcv` group (mastering-display
//! primaries / white-point / luminance per SMPTE ST 2086:2018).
//!
//! The demuxer already parses these via the shared AMF0 flatten walker
//! and exposes them under stable `metadata["colorinfo.<group>.<key>"]`
//! keys (see `crate::demuxer::harvest_video_metadata_frame`). This
//! module is the writer-side mirror: callers populate a typed
//! [`ColorInfo`] struct, then either feed [`ColorInfo::encode_amf`] to
//! [`crate::tag::write_ex_video_metadata`] directly or use the
//! [`crate::tag::write_ex_video_color_info`] convenience writer.
//!
//! The encoded body is symmetric with the parser: feeding the output
//! of [`ColorInfo::encode_amf`] through [`crate::tag::write_ex_video_tag`]
//! and back through [`crate::FlvDemuxer`] recovers every populated
//! field as a `colorinfo.*` metadata entry.

use std::io::Write;

use oxideav_core::{Error, Result};

use crate::amf0;

/// Top-level `colorInfo` object (Veovera `enhanced-rtmp-v2`
/// §"Metadata Frame"). Each group is optional so producers can emit
/// only the metadata they actually have — a player that finds a missing
/// group falls back to the codec-bitstream signalling.
///
/// An empty [`ColorInfo`] (`ColorInfo::default()`) encodes to an empty
/// AMF0 object, which per spec resets the player's color state — the
/// same semantics as [`write_ex_video_color_info_reset`] below
/// (Undefined value).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColorInfo {
    /// `colorConfig` — bit depth + ISO 23091-4 / ITU-T H.273 colour
    /// indices (primaries, transfer, matrix).
    pub color_config: Option<ColorConfig>,
    /// `hdrCll` — MaxFALL / MaxCLL content-light-level metadata
    /// (CTA-861.3 / CEA-861.3, surfaced via the spec's hdrCll group).
    pub hdr_cll: Option<HdrCll>,
    /// `hdrMdcv` — mastering-display colour-volume metadata
    /// (SMPTE ST 2086:2018).
    pub hdr_mdcv: Option<HdrMdcv>,
}

/// `colorConfig` group. Every field is optional; an `Option::None`
/// means "producer did not signal it" and the property is omitted from
/// the AMF object so the player can fall through to its codec-bitstream
/// signalling. A `Some(v)` whose value falls outside the spec range is
/// rejected by [`ColorInfo::encode_amf`] with [`Error::invalid`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ColorConfig {
    /// `bitDepth`. Spec: "SHOULD be 8, 10 or 12." We don't enforce that
    /// SHOULD — only `u8` so callers can't emit a NaN — but anything
    /// outside `[8, 16]` is rejected as out-of-band.
    pub bit_depth: Option<u8>,
    /// `colorPrimaries` — ISO 23091-4 / H.273 §8.1 enumeration `[0, 255]`.
    pub color_primaries: Option<u8>,
    /// `transferCharacteristics` — ISO 23091-4 / H.273 §8.2 enumeration
    /// `[0, 255]`.
    pub transfer_characteristics: Option<u8>,
    /// `matrixCoefficients` — ISO 23091-4 / H.273 §8.3 enumeration
    /// `[0, 255]`.
    pub matrix_coefficients: Option<u8>,
}

/// `hdrCll` group — content light level (MaxFALL / MaxCLL). Spec range
/// for both is `[0.0001, 10_000]` cd/m^2. Out-of-range values are
/// rejected as [`Error::invalid`]; `None` omits the property.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HdrCll {
    /// `maxFall` — maximum frame-average light level of the entire
    /// playback sequence, in cd/m^2.
    pub max_fall: Option<f64>,
    /// `maxCLL` — maximum light level of any single pixel of the
    /// entire playback sequence, in cd/m^2.
    pub max_cll: Option<f64>,
}

/// `hdrMdcv` group — mastering-display colour-volume metadata
/// (SMPTE ST 2086:2018). Chromaticity coordinates land in CIE 1931 XY
/// space with the spec-required four-decimal-place precision; `redX` /
/// `greenX` / `blueX` / `whitePointX` are in `[0.0001, 0.7400]` and the
/// `*Y` counterparts in `[0.0001, 0.8400]`. Luminance is in cd/m^2
/// (`maxLuminance` in `[5, 10_000]`, `minLuminance` in `[0.0001, 5]`).
/// `None` omits the property; out-of-range values raise
/// [`Error::invalid`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HdrMdcv {
    pub red_x: Option<f64>,
    pub red_y: Option<f64>,
    pub green_x: Option<f64>,
    pub green_y: Option<f64>,
    pub blue_x: Option<f64>,
    pub blue_y: Option<f64>,
    pub white_point_x: Option<f64>,
    pub white_point_y: Option<f64>,
    /// `maxLuminance` — peak mastering-display luminance, cd/m^2.
    pub max_luminance: Option<f64>,
    /// `minLuminance` — minimum mastering-display luminance, cd/m^2.
    pub min_luminance: Option<f64>,
}

// X chromaticity in `[0.0001, 0.7400]`, Y chromaticity in
// `[0.0001, 0.8400]` per Veovera v2 hdrMdcv ranges (which follow
// SMPTE ST 2086:2018 with units of 1 cd/m^2 for luminance).
const CHROMA_X_MIN: f64 = 0.0001;
const CHROMA_X_MAX: f64 = 0.7400;
const CHROMA_Y_MIN: f64 = 0.0001;
const CHROMA_Y_MAX: f64 = 0.8400;
const CLL_MIN: f64 = 0.0001;
const CLL_MAX: f64 = 10_000.0;
const MAX_LUM_MIN: f64 = 5.0;
const MAX_LUM_MAX: f64 = 10_000.0;
const MIN_LUM_MIN: f64 = 0.0001;
const MIN_LUM_MAX: f64 = 5.0;
// bitDepth allowed range — spec SHOULD says 8 / 10 / 12, but the field
// is a free `number` so we accept [8, 16] (any other producer-specific
// width like 14 bits still survives). Anything outside that band is
// almost certainly a bug.
const BIT_DEPTH_MIN: u8 = 8;
const BIT_DEPTH_MAX: u8 = 16;

impl ColorInfo {
    /// Encode the colorInfo into the AMF body that follows the Ex
    /// video tag header on a `VideoPacketType.Metadata` tag (spec
    /// §"Metadata Frame"). The body shape is one AMF0 `[name, value]`
    /// pair: an AMF0 String `"colorInfo"` followed by an anonymous
    /// AMF0 Object carrying the populated groups.
    ///
    /// An empty [`ColorInfo`] encodes the spec-recommended "empty
    /// object" reset shape (`["colorInfo", {}]`); use
    /// [`encode_amf_reset`] for the alternative `Undefined` reset.
    ///
    /// Bounds-checks every populated field against the spec ranges;
    /// returns [`Error::invalid`] on the first out-of-range value.
    pub fn encode_amf(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(256);
        encode_amf_into(&mut out, self)?;
        Ok(out)
    }
}

/// Append the `["colorInfo", Object]` pair to `out`. Useful when a
/// producer wants to emit several pairs in one Metadata tag (the spec
/// allows future pair names alongside `colorInfo`).
pub fn encode_amf_into<W: Write + ?Sized>(out: &mut W, ci: &ColorInfo) -> Result<()> {
    amf0::write_string(out, "colorInfo")?;
    // Anonymous object — { colorConfig?, hdrCll?, hdrMdcv? }.
    amf0::write_object_start(out)?;
    if let Some(cc) = &ci.color_config {
        amf0::write_property_name(out, "colorConfig")?;
        write_color_config(out, cc)?;
    }
    if let Some(cll) = &ci.hdr_cll {
        amf0::write_property_name(out, "hdrCll")?;
        write_hdr_cll(out, cll)?;
    }
    if let Some(mdcv) = &ci.hdr_mdcv {
        amf0::write_property_name(out, "hdrMdcv")?;
        write_hdr_mdcv(out, mdcv)?;
    }
    amf0::write_object_end(out)?;
    Ok(())
}

/// Encode the spec-recommended reset shape — an AMF0 `[name, value]`
/// pair `["colorInfo", Undefined]`. Per Veovera `enhanced-rtmp-v2`
/// §"Metadata Frame": "To reset to the original color state you can
/// send colorInfo with a value of Undefined (the RECOMMENDED approach)
/// or an empty object." The demuxer surfaces this as a single
/// `metadata["colorinfo"] = "undefined"` sentinel and drops every
/// prior `colorinfo.*` entry.
pub fn encode_amf_reset() -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    amf0::write_string(&mut out, "colorInfo").expect("Vec write");
    // AMF0 Undefined marker (0x06) — no payload bytes.
    out.push(0x06);
    out
}

fn write_color_config<W: Write + ?Sized>(out: &mut W, cc: &ColorConfig) -> Result<()> {
    amf0::write_object_start(out)?;
    if let Some(bd) = cc.bit_depth {
        if !(BIT_DEPTH_MIN..=BIT_DEPTH_MAX).contains(&bd) {
            return Err(Error::invalid(format!(
                "colorInfo: bitDepth {bd} outside expected [{BIT_DEPTH_MIN}, {BIT_DEPTH_MAX}]"
            )));
        }
        amf0::write_property_name(out, "bitDepth")?;
        amf0::write_number(out, f64::from(bd))?;
    }
    if let Some(p) = cc.color_primaries {
        amf0::write_property_name(out, "colorPrimaries")?;
        amf0::write_number(out, f64::from(p))?;
    }
    if let Some(t) = cc.transfer_characteristics {
        amf0::write_property_name(out, "transferCharacteristics")?;
        amf0::write_number(out, f64::from(t))?;
    }
    if let Some(m) = cc.matrix_coefficients {
        amf0::write_property_name(out, "matrixCoefficients")?;
        amf0::write_number(out, f64::from(m))?;
    }
    amf0::write_object_end(out)?;
    Ok(())
}

fn write_hdr_cll<W: Write + ?Sized>(out: &mut W, cll: &HdrCll) -> Result<()> {
    amf0::write_object_start(out)?;
    if let Some(v) = cll.max_fall {
        check_range("hdrCll.maxFall", v, CLL_MIN, CLL_MAX)?;
        amf0::write_property_name(out, "maxFall")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = cll.max_cll {
        check_range("hdrCll.maxCLL", v, CLL_MIN, CLL_MAX)?;
        amf0::write_property_name(out, "maxCLL")?;
        amf0::write_number(out, v)?;
    }
    amf0::write_object_end(out)?;
    Ok(())
}

fn write_hdr_mdcv<W: Write + ?Sized>(out: &mut W, m: &HdrMdcv) -> Result<()> {
    amf0::write_object_start(out)?;
    if let Some(v) = m.red_x {
        check_range("hdrMdcv.redX", v, CHROMA_X_MIN, CHROMA_X_MAX)?;
        amf0::write_property_name(out, "redX")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.red_y {
        check_range("hdrMdcv.redY", v, CHROMA_Y_MIN, CHROMA_Y_MAX)?;
        amf0::write_property_name(out, "redY")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.green_x {
        check_range("hdrMdcv.greenX", v, CHROMA_X_MIN, CHROMA_X_MAX)?;
        amf0::write_property_name(out, "greenX")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.green_y {
        check_range("hdrMdcv.greenY", v, CHROMA_Y_MIN, CHROMA_Y_MAX)?;
        amf0::write_property_name(out, "greenY")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.blue_x {
        check_range("hdrMdcv.blueX", v, CHROMA_X_MIN, CHROMA_X_MAX)?;
        amf0::write_property_name(out, "blueX")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.blue_y {
        check_range("hdrMdcv.blueY", v, CHROMA_Y_MIN, CHROMA_Y_MAX)?;
        amf0::write_property_name(out, "blueY")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.white_point_x {
        check_range("hdrMdcv.whitePointX", v, CHROMA_X_MIN, CHROMA_X_MAX)?;
        amf0::write_property_name(out, "whitePointX")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.white_point_y {
        check_range("hdrMdcv.whitePointY", v, CHROMA_Y_MIN, CHROMA_Y_MAX)?;
        amf0::write_property_name(out, "whitePointY")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.max_luminance {
        check_range("hdrMdcv.maxLuminance", v, MAX_LUM_MIN, MAX_LUM_MAX)?;
        amf0::write_property_name(out, "maxLuminance")?;
        amf0::write_number(out, v)?;
    }
    if let Some(v) = m.min_luminance {
        check_range("hdrMdcv.minLuminance", v, MIN_LUM_MIN, MIN_LUM_MAX)?;
        amf0::write_property_name(out, "minLuminance")?;
        amf0::write_number(out, v)?;
    }
    amf0::write_object_end(out)?;
    Ok(())
}

fn check_range(name: &str, value: f64, lo: f64, hi: f64) -> Result<()> {
    if !value.is_finite() {
        return Err(Error::invalid(format!(
            "colorInfo: {name} = {value} is not finite"
        )));
    }
    if value < lo || value > hi {
        return Err(Error::invalid(format!(
            "colorInfo: {name} = {value} outside spec range [{lo}, {hi}]"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u16(b: &[u8], p: usize) -> u16 {
        u16::from_be_bytes([b[p], b[p + 1]])
    }

    /// Walk AMF0 bytes from `pos`, expecting a property-name UI16 + UTF-8.
    fn read_prop_name(b: &[u8], pos: usize) -> (String, usize) {
        let len = read_u16(b, pos) as usize;
        let s = std::str::from_utf8(&b[pos + 2..pos + 2 + len])
            .unwrap()
            .to_string();
        (s, pos + 2 + len)
    }

    #[test]
    fn empty_color_info_encodes_to_empty_object_pair() {
        let ci = ColorInfo::default();
        let bytes = ci.encode_amf().unwrap();
        // "colorInfo" string (0x02 + UI16 9 + 9 bytes) + Object start
        // (0x03) + object-end (0x00 0x00 0x09) = 12 + 4 = 16 bytes.
        assert_eq!(bytes.len(), 1 + 2 + 9 + 1 + 3);
        assert_eq!(bytes[0], 0x02);
        assert_eq!(read_u16(&bytes, 1), 9);
        assert_eq!(&bytes[3..12], b"colorInfo");
        assert_eq!(bytes[12], 0x03);
        assert_eq!(&bytes[13..16], &[0x00, 0x00, 0x09]);
    }

    #[test]
    fn populated_color_info_emits_expected_properties() {
        let ci = ColorInfo {
            color_config: Some(ColorConfig {
                bit_depth: Some(10),
                color_primaries: Some(9),
                transfer_characteristics: Some(16),
                matrix_coefficients: Some(9),
            }),
            hdr_cll: Some(HdrCll {
                max_fall: Some(400.0),
                max_cll: Some(1000.0),
            }),
            hdr_mdcv: Some(HdrMdcv {
                red_x: Some(0.708),
                red_y: Some(0.292),
                green_x: Some(0.170),
                green_y: Some(0.797),
                blue_x: Some(0.131),
                blue_y: Some(0.046),
                white_point_x: Some(0.3127),
                white_point_y: Some(0.3290),
                max_luminance: Some(1000.0),
                min_luminance: Some(0.01),
            }),
        };
        let bytes = ci.encode_amf().unwrap();
        // Sanity: pair starts with the "colorInfo" String marker.
        assert_eq!(bytes[0], 0x02);
        assert_eq!(&bytes[3..12], b"colorInfo");
        // The body starts at byte 12 with the outer Object-start marker.
        assert_eq!(bytes[12], 0x03);
        // Walk the outer body and confirm the three group names show up in order.
        let mut p = 13;
        let (name, np) = read_prop_name(&bytes, p);
        assert_eq!(name, "colorConfig");
        // Object value
        assert_eq!(bytes[np], 0x03);
        // Skip the inner object cheaply by scanning for the 0x00 0x00 0x09 marker.
        let mut q = np + 1;
        while !(bytes[q] == 0x00 && bytes[q + 1] == 0x00 && bytes[q + 2] == 0x09) {
            q += 1;
        }
        q += 3;
        p = q;
        let (name2, np2) = read_prop_name(&bytes, p);
        assert_eq!(name2, "hdrCll");
        assert_eq!(bytes[np2], 0x03);
        let mut q = np2 + 1;
        while !(bytes[q] == 0x00 && bytes[q + 1] == 0x00 && bytes[q + 2] == 0x09) {
            q += 1;
        }
        q += 3;
        let (name3, _) = read_prop_name(&bytes, q);
        assert_eq!(name3, "hdrMdcv");
    }

    #[test]
    fn reset_payload_is_undefined_value() {
        let bytes = encode_amf_reset();
        // "colorInfo" String + AMF0 Undefined marker (0x06).
        assert_eq!(bytes.len(), 1 + 2 + 9 + 1);
        assert_eq!(bytes[0], 0x02);
        assert_eq!(&bytes[3..12], b"colorInfo");
        assert_eq!(bytes[12], 0x06);
    }

    #[test]
    fn out_of_range_max_cll_rejected() {
        let ci = ColorInfo {
            hdr_cll: Some(HdrCll {
                max_cll: Some(20_000.0),
                ..HdrCll::default()
            }),
            ..ColorInfo::default()
        };
        let err = ci.encode_amf().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("maxCLL"), "got: {msg}");
        assert!(msg.contains("outside spec range"), "got: {msg}");
    }

    #[test]
    fn nan_max_fall_rejected() {
        let ci = ColorInfo {
            hdr_cll: Some(HdrCll {
                max_fall: Some(f64::NAN),
                ..HdrCll::default()
            }),
            ..ColorInfo::default()
        };
        let err = ci.encode_amf().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("maxFall"));
        assert!(msg.contains("not finite"));
    }

    #[test]
    fn out_of_range_bit_depth_rejected() {
        let ci = ColorInfo {
            color_config: Some(ColorConfig {
                bit_depth: Some(4),
                ..ColorConfig::default()
            }),
            ..ColorInfo::default()
        };
        let err = ci.encode_amf().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bitDepth"));
    }

    #[test]
    fn out_of_range_red_x_rejected() {
        let ci = ColorInfo {
            hdr_mdcv: Some(HdrMdcv {
                red_x: Some(0.9),
                ..HdrMdcv::default()
            }),
            ..ColorInfo::default()
        };
        let err = ci.encode_amf().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("redX"));
    }

    #[test]
    fn out_of_range_min_luminance_rejected() {
        let ci = ColorInfo {
            hdr_mdcv: Some(HdrMdcv {
                min_luminance: Some(7.5),
                ..HdrMdcv::default()
            }),
            ..ColorInfo::default()
        };
        let err = ci.encode_amf().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("minLuminance"));
    }

    #[test]
    fn encode_amf_into_appends_to_existing_buffer() {
        let mut buf = vec![0xAA, 0xBB];
        let ci = ColorInfo::default();
        encode_amf_into(&mut buf, &ci).unwrap();
        // Original bytes preserved.
        assert_eq!(&buf[..2], &[0xAA, 0xBB]);
        // Pair body lands after them.
        assert_eq!(buf[2], 0x02);
    }
}
