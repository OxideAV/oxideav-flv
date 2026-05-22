//! Enhanced RTMP / E-FLV `ModEx` (Modifier Extension) tag bodies, per
//! Veovera `enhanced-rtmp-v2` §`ExAudioTagHeader` (audio ModEx loop) and
//! §`ExVideoTagHeader` (video ModEx loop).
//!
//! # On-the-wire layout
//!
//! Both the audio and video Ex tag headers share the *same* ModEx loop
//! shape: it sits between the leading packet-type-bearing byte and the
//! FourCc (or `Multitrack` outer header). The walk runs while the
//! currently-decoded `AudioPacketType` / `VideoPacketType` equals
//! `ModEx = 7`:
//!
//! ```text
//!   ── loop while packet_type == ModEx:
//!     1   UI8       modExDataSize - 1                       (UI8)
//!     ?   ?         if modExDataSize == 256: UI16 + 1       (escape)
//!     N   bytes     modExData[modExDataSize]                (opaque)
//!     1   UI8       (modExType UB[4]) | (next packetType UB[4])
//!   ──
//! ```
//!
//! - `modExDataSize` is encoded as `UI8 + 1` so a single byte can
//!   express sizes 1..256. The escape (`0xFF` → `UI16 + 1`) extends the
//!   range to 257..65_536 — see `decode_size`.
//! - The trailer byte packs **two** UB[4]s: high nibble = the modifier
//!   subtype identifier (`AudioPacketModExType` / `VideoPacketModExType`)
//!   and low nibble = the next `AudioPacketType` / `VideoPacketType`
//!   (which may itself be `ModEx`, chaining the loop).
//!
//! # Currently-defined subtypes
//!
//! `enhanced-rtmp-v2` defines exactly **one** ModEx subtype per
//! direction:
//!
//! - `AudioPacketModExType::TimestampOffsetNano = 0`
//! - `VideoPacketModExType::TimestampOffsetNano = 0`
//!
//! Values 1..13 are reserved-for-future, 14 and 15 are explicitly
//! reserved. This parser surfaces them as
//! [`AudioPacketModExType::Reserved(u8)`] /
//! [`VideoPacketModExType::Reserved(u8)`] with the raw payload bytes
//! preserved on the side via [`ModExEntry::raw`] so future-defined
//! subtypes can be lifted without re-parsing the wire layer.
//!
//! ## `TimestampOffsetNano` payload
//!
//! Per the v2 spec: a 3-byte big-endian UI24 that refines the
//! millisecond RTMP timestamp on this packet by 0..999_999 ns. Multiple
//! TimestampOffsetNano ModEx packets chain additively (the decoder
//! sums them; the spec caps the total at one millisecond minus one
//! nanosecond per-message).

use oxideav_core::{Error, Result};

/// Audio-side ModEx subtype identifier (UB[4], high nibble of the
/// trailer byte). The spec only defines `TimestampOffsetNano = 0`;
/// every other value is captured as `Reserved(u8)` so future
/// subtype additions surface without parser changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioPacketModExType {
    /// `0` — `TimestampOffsetNano`. The ModEx data carries a UI24
    /// nanosecond timestamp refinement (0..999_999 ns).
    TimestampOffsetNano,
    /// Any other UB[4] value. Reserved (1..13) or formally reserved
    /// (14, 15) per the spec.
    Reserved(u8),
}

impl AudioPacketModExType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::TimestampOffsetNano,
            other => Self::Reserved(other),
        }
    }

    /// Raw UB[4] code stored on the wire (for round-trip / logging).
    pub fn raw(self) -> u8 {
        match self {
            Self::TimestampOffsetNano => 0,
            Self::Reserved(v) => v,
        }
    }
}

/// Video-side ModEx subtype identifier. Symmetric with
/// [`AudioPacketModExType`]; kept distinct so the audio and video
/// paths remain self-contained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoPacketModExType {
    /// `0` — `TimestampOffsetNano`. The ModEx data carries a UI24
    /// nanosecond timestamp refinement (0..999_999 ns).
    TimestampOffsetNano,
    /// Any other UB[4] value.
    Reserved(u8),
}

impl VideoPacketModExType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::TimestampOffsetNano,
            other => Self::Reserved(other),
        }
    }

    pub fn raw(self) -> u8 {
        match self {
            Self::TimestampOffsetNano => 0,
            Self::Reserved(v) => v,
        }
    }
}

/// Typed payload carried inside a ModEx data blob.
///
/// `enhanced-rtmp-v2` defines only one such payload today; the parser
/// still surfaces opaque reserved blobs (the raw bytes are reachable
/// via [`ModExEntry::raw`]) so consumers can react to future spec
/// extensions without re-parsing the size escape and trailer layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModExPayload {
    /// `TimestampOffsetNano` — UI24 BE nanosecond refinement
    /// (0..999_999 ns) decoded from a >= 3-byte payload. Spec note:
    /// "modExData MUST be at least 3 bytes, storing values up to
    /// 999,999 ns".
    TimestampOffsetNano { offset_ns: u32 },
    /// Modifier subtype is reserved (or future). The wire payload is
    /// preserved on [`ModExEntry::raw`]; this variant carries the raw
    /// UB[4] subtype code so callers can branch on it.
    Reserved { subtype_raw: u8 },
}

/// A single ModEx entry as parsed off the front of an ExAudio /
/// ExVideo tag body.
///
/// `raw` always points at the on-wire bytes of the modifier data
/// section (the size-prefixed blob, *not* the trailer byte). For
/// `TimestampOffsetNano` that's the 3+ bytes containing the UI24;
/// for reserved subtypes it's whatever opaque blob the producer
/// chose to embed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModExEntry {
    /// Subtype code as decoded from the high nibble of the ModEx
    /// trailer byte. `u8` so the audio and video paths can share the
    /// same `ModExEntry` shape — they translate it via
    /// [`AudioPacketModExType::from_u8`] /
    /// [`VideoPacketModExType::from_u8`] when surfaced.
    pub subtype_raw: u8,
    /// Typed payload (only `TimestampOffsetNano` today; reserved
    /// subtypes carry a `Reserved` marker + the raw payload below).
    pub payload: ModExPayload,
    /// Verbatim ModEx data bytes (size-prefix removed). Length matches
    /// the decoded `modExDataSize`.
    pub raw: Vec<u8>,
}

impl ModExEntry {
    /// View this entry as an audio-side typed subtype.
    pub fn audio_subtype(&self) -> AudioPacketModExType {
        AudioPacketModExType::from_u8(self.subtype_raw)
    }

    /// View this entry as a video-side typed subtype.
    pub fn video_subtype(&self) -> VideoPacketModExType {
        VideoPacketModExType::from_u8(self.subtype_raw)
    }

    /// If this entry is a `TimestampOffsetNano`, return its UI24
    /// nanosecond value. `None` for reserved subtypes.
    pub fn timestamp_offset_nano(&self) -> Option<u32> {
        match self.payload {
            ModExPayload::TimestampOffsetNano { offset_ns } => Some(offset_ns),
            ModExPayload::Reserved { .. } => None,
        }
    }
}

/// Decode a single ModEx data blob's size prefix.
///
/// Returns the data size and the number of header bytes consumed
/// (1 for UI8, 3 for the UI16 escape).
///
/// Per spec:
///
/// ```text
///   modExDataSize = UI8 + 1                    // covers 1..256
///   if modExDataSize == 256:
///     modExDataSize = UI16 + 1                // covers 1..65536, but
///                                             // only used >= 257
/// ```
fn decode_size(body: &[u8], cursor: usize) -> Result<(usize, usize)> {
    if cursor >= body.len() {
        return Err(Error::invalid("FLV ModEx: truncated size prefix"));
    }
    let small = (body[cursor] as usize) + 1;
    if small != 256 {
        return Ok((small, 1));
    }
    if cursor + 3 > body.len() {
        return Err(Error::invalid("FLV ModEx: truncated UI16 size escape"));
    }
    let big = (((body[cursor + 1] as usize) << 8) | (body[cursor + 2] as usize)) + 1;
    Ok((big, 3))
}

/// Run the ModEx walk off `body[cursor..]` while `packet_type_raw`
/// equals `MODEX_PACKET_TYPE`. Returns the updated cursor, the
/// post-ModEx `packet_type_raw` value, the sequence of parsed
/// ModEx entries (one per loop iteration), and the accumulated
/// `TimestampOffsetNano` sum (in ns).
///
/// `MODEX_PACKET_TYPE` is 7 for both audio and video — that's why
/// the helper takes it as a const argument: same wire format, two
/// independent enums on top.
pub fn walk<const MODEX_PACKET_TYPE: u8>(
    body: &[u8],
    mut cursor: usize,
    mut packet_type_raw: u8,
) -> Result<(usize, u8, Vec<ModExEntry>, u32)> {
    let mut entries: Vec<ModExEntry> = Vec::new();
    let mut total_offset_ns: u32 = 0;

    while packet_type_raw == MODEX_PACKET_TYPE {
        // Size prefix (UI8 + 1, with UI16 + 1 escape on 256).
        let (mod_ex_size, size_header_len) = decode_size(body, cursor)?;
        cursor += size_header_len;

        // Opaque data section.
        if cursor + mod_ex_size > body.len() {
            return Err(Error::invalid("FLV ModEx: truncated modExData"));
        }
        let raw = body[cursor..cursor + mod_ex_size].to_vec();
        cursor += mod_ex_size;

        // Trailer byte: (modExType UB[4]) | (next packetType UB[4]).
        if cursor >= body.len() {
            return Err(Error::invalid("FLV ModEx: truncated trailer byte"));
        }
        let trailer = body[cursor];
        cursor += 1;
        let subtype_raw = (trailer >> 4) & 0x0F;
        packet_type_raw = trailer & 0x0F;

        // Decode the typed payload.
        let payload = if subtype_raw == 0 {
            // `TimestampOffsetNano` — UI24 BE in the first 3 bytes.
            // Spec: "modExData MUST be at least 3 bytes". Anything
            // shorter is malformed.
            if raw.len() < 3 {
                return Err(Error::invalid(
                    "FLV ModEx: TimestampOffsetNano needs >= 3 bytes",
                ));
            }
            let offset_ns = ((raw[0] as u32) << 16) | ((raw[1] as u32) << 8) | (raw[2] as u32);
            // Saturating-add so two consecutive ModEx packets can't
            // overflow the u32 even though the spec caps each at
            // 999_999 ns.
            total_offset_ns = total_offset_ns.saturating_add(offset_ns);
            ModExPayload::TimestampOffsetNano { offset_ns }
        } else {
            // Reserved (1..13) or formally-reserved (14, 15).
            // Opaque payload preserved on `raw`; no semantic decode.
            ModExPayload::Reserved { subtype_raw }
        };

        entries.push(ModExEntry {
            subtype_raw,
            payload,
            raw,
        });
    }

    Ok((cursor, packet_type_raw, entries, total_offset_ns))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: encode one ModEx entry into a contiguous byte slab.
    ///
    /// `subtype` = high nibble of trailer; `next_packet_type` = low
    /// nibble (i.e. the next AudioPacketType / VideoPacketType the
    /// outer parser must continue with).
    fn encode_one(payload: &[u8], subtype: u8, next_packet_type: u8) -> Vec<u8> {
        let mut buf = Vec::new();
        // size prefix
        if payload.len() <= 256 {
            buf.push((payload.len() - 1) as u8);
        } else {
            // 256 escape
            buf.push(0xFF);
            let n16 = (payload.len() - 1) as u16;
            buf.extend_from_slice(&n16.to_be_bytes());
        }
        buf.extend_from_slice(payload);
        buf.push(((subtype & 0x0F) << 4) | (next_packet_type & 0x0F));
        buf
    }

    #[test]
    fn audio_subtype_lookup_round_trips() {
        assert_eq!(
            AudioPacketModExType::from_u8(0),
            AudioPacketModExType::TimestampOffsetNano
        );
        assert_eq!(AudioPacketModExType::TimestampOffsetNano.raw(), 0);
        for v in 1..=15u8 {
            let t = AudioPacketModExType::from_u8(v);
            assert_eq!(t, AudioPacketModExType::Reserved(v));
            assert_eq!(t.raw(), v);
        }
    }

    #[test]
    fn video_subtype_lookup_round_trips() {
        assert_eq!(
            VideoPacketModExType::from_u8(0),
            VideoPacketModExType::TimestampOffsetNano
        );
        assert_eq!(VideoPacketModExType::TimestampOffsetNano.raw(), 0);
        for v in 1..=15u8 {
            let t = VideoPacketModExType::from_u8(v);
            assert_eq!(t, VideoPacketModExType::Reserved(v));
            assert_eq!(t.raw(), v);
        }
    }

    #[test]
    fn decode_size_small_case() {
        // size byte 0x02 → modExDataSize = 3
        let body = [0x02, 0xAA, 0xBB, 0xCC];
        let (sz, hdr) = decode_size(&body, 0).unwrap();
        assert_eq!(sz, 3);
        assert_eq!(hdr, 1);
    }

    #[test]
    fn decode_size_escape_case() {
        // size byte 0xFF → escape; UI16 BE = 0x0100 (256) → +1 = 257
        let body = [0xFF, 0x01, 0x00];
        let (sz, hdr) = decode_size(&body, 0).unwrap();
        assert_eq!(sz, 257);
        assert_eq!(hdr, 3);
    }

    #[test]
    fn decode_size_truncated_small() {
        let body: &[u8] = &[];
        assert!(decode_size(body, 0).is_err());
    }

    #[test]
    fn decode_size_truncated_escape() {
        // escape byte present but UI16 missing
        let body = [0xFF, 0x01];
        assert!(decode_size(&body, 0).is_err());
    }

    #[test]
    fn walk_no_modex_returns_input_packet_type() {
        // packet_type_raw=1 (CodedFrames), no ModEx in the loop — no
        // bytes consumed.
        let body = [0xDE, 0xAD];
        let (cur, pt, entries, ns) = walk::<7>(&body, 0, 1).unwrap();
        assert_eq!(cur, 0);
        assert_eq!(pt, 1);
        assert!(entries.is_empty());
        assert_eq!(ns, 0);
    }

    #[test]
    fn walk_single_timestamp_offset_nano() {
        // 3-byte UI24 = 1000ns; subtype=0, next_packet_type=1 (CodedFrames).
        let body = encode_one(&[0x00, 0x03, 0xE8], 0, 1);
        let (cur, pt, entries, ns) = walk::<7>(&body, 0, 7).unwrap();
        assert_eq!(cur, body.len());
        assert_eq!(pt, 1); // CodedFrames now active.
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.subtype_raw, 0);
        assert_eq!(e.timestamp_offset_nano(), Some(1000));
        assert_eq!(e.raw, vec![0x00, 0x03, 0xE8]);
        assert_eq!(ns, 1000);
        assert_eq!(e.audio_subtype(), AudioPacketModExType::TimestampOffsetNano);
        assert_eq!(e.video_subtype(), VideoPacketModExType::TimestampOffsetNano);
    }

    #[test]
    fn walk_chains_two_timestamp_offsets() {
        // First entry: 100ns, trailer subtype=0, next=ModEx (chain).
        // Second entry: 200ns, trailer subtype=0, next=CodedFrames.
        let mut body = encode_one(&[0x00, 0x00, 0x64], 0, 7);
        body.extend(encode_one(&[0x00, 0x00, 0xC8], 0, 1));
        let (cur, pt, entries, ns) = walk::<7>(&body, 0, 7).unwrap();
        assert_eq!(cur, body.len());
        assert_eq!(pt, 1);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].timestamp_offset_nano(), Some(100));
        assert_eq!(entries[1].timestamp_offset_nano(), Some(200));
        assert_eq!(ns, 300);
    }

    #[test]
    fn walk_reserved_subtype_preserves_raw_payload() {
        // subtype=0x05 (reserved) with an opaque 4-byte payload.
        // trailer next=1 (CodedFrames).
        let body = encode_one(&[0xCA, 0xFE, 0xBA, 0xBE], 5, 1);
        let (cur, pt, entries, ns) = walk::<7>(&body, 0, 7).unwrap();
        assert_eq!(cur, body.len());
        assert_eq!(pt, 1);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.subtype_raw, 5);
        assert_eq!(e.audio_subtype(), AudioPacketModExType::Reserved(5));
        assert_eq!(e.video_subtype(), VideoPacketModExType::Reserved(5));
        assert!(matches!(
            e.payload,
            ModExPayload::Reserved { subtype_raw: 5 }
        ));
        assert_eq!(e.raw, vec![0xCA, 0xFE, 0xBA, 0xBE]);
        // Reserved subtypes never report a timestamp offset.
        assert_eq!(e.timestamp_offset_nano(), None);
        assert_eq!(ns, 0);
    }

    #[test]
    fn walk_handles_256_byte_escape_payload() {
        // 257-byte payload triggers the UI16 escape (size byte = 0xFF,
        // then UI16 BE = 256 → +1 = 257). subtype=0xF (reserved),
        // trailer next=1.
        let payload: Vec<u8> = (0..257).map(|i| (i & 0xFF) as u8).collect();
        let body = encode_one(&payload, 0xF, 1);
        let (cur, pt, entries, ns) = walk::<7>(&body, 0, 7).unwrap();
        assert_eq!(cur, body.len());
        assert_eq!(pt, 1);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.subtype_raw, 0xF);
        assert_eq!(e.raw.len(), 257);
        // No TimestampOffsetNano interpretation for non-zero subtype.
        assert_eq!(ns, 0);
    }

    #[test]
    fn walk_chains_timestamp_then_reserved() {
        // First: 500ns; next=ModEx. Second: reserved subtype=2, next=1.
        let mut body = encode_one(&[0x00, 0x01, 0xF4], 0, 7);
        body.extend(encode_one(&[0x01, 0x02], 2, 1));
        let (cur, pt, entries, ns) = walk::<7>(&body, 0, 7).unwrap();
        assert_eq!(cur, body.len());
        assert_eq!(pt, 1);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].timestamp_offset_nano(), Some(500));
        assert_eq!(entries[1].timestamp_offset_nano(), None);
        assert_eq!(entries[1].subtype_raw, 2);
        assert_eq!(ns, 500);
    }

    #[test]
    fn walk_truncated_size_errors() {
        // packet_type=ModEx but body has no size byte.
        let body: &[u8] = &[];
        assert!(walk::<7>(body, 0, 7).is_err());
    }

    #[test]
    fn walk_truncated_data_errors() {
        // size says 3 bytes, only 1 present.
        let body = [0x02, 0xAA];
        assert!(walk::<7>(&body, 0, 7).is_err());
    }

    #[test]
    fn walk_truncated_trailer_errors() {
        // size=1, 1 data byte, but trailer byte missing.
        let body = [0x00, 0xAA];
        assert!(walk::<7>(&body, 0, 7).is_err());
    }

    #[test]
    fn walk_short_timestamp_offset_nano_errors() {
        // subtype=0 (TimestampOffsetNano) but payload is only 2 bytes.
        let body = encode_one(&[0xAA, 0xBB], 0, 1);
        assert!(walk::<7>(&body, 0, 7).is_err());
    }

    #[test]
    fn walk_starts_already_past_modex() {
        // packet_type_raw=2 (SequenceEnd) → no ModEx loop, no bytes
        // consumed.
        let body = [0x99, 0x88];
        let (cur, pt, entries, ns) = walk::<7>(&body, 0, 2).unwrap();
        assert_eq!(cur, 0);
        assert_eq!(pt, 2);
        assert!(entries.is_empty());
        assert_eq!(ns, 0);
    }
}
