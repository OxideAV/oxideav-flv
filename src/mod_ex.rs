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

/// Encode the size prefix for a single ModEx data blob.
///
/// Per spec the wire form is:
///
/// ```text
///   modExDataSize = UI8 + 1                    // covers 1..255 directly
///   if encoded UI8 byte == 0xFF:
///     modExDataSize = UI16 + 1                 // covers 1..65536 via escape
/// ```
///
/// A producer therefore picks the UI8 path for payloads of 1..=255
/// bytes (UI8 byte 0..=0xFE → size 1..=255), and the UI16 escape for
/// payloads of 256..=65_536 bytes (escape byte 0xFF followed by UI16 BE
/// 0x00FF..=0xFFFF → size 256..=65_536). Payloads outside `1..=65_536`
/// have no wire representation and are rejected with
/// [`Error::InvalidData`].
///
/// Note: the inclusive UI8 boundary at 255 is the only correct choice —
/// payload length 256 cannot be expressed as UI8 because `UI8 + 1 = 256`
/// requires UI8 byte `0xFF`, which the decoder interprets as the
/// escape sentinel rather than a literal 256.
fn encode_size(out: &mut Vec<u8>, size: usize) -> Result<()> {
    if size == 0 || size > 65_536 {
        return Err(Error::invalid(
            "FLV ModEx: modExData size must be in 1..=65_536",
        ));
    }
    if size <= 255 {
        out.push((size - 1) as u8);
    } else {
        // size in 256..=65_536 → escape sentinel + UI16 BE (size - 1).
        out.push(0xFF);
        let n16 = (size - 1) as u16;
        out.extend_from_slice(&n16.to_be_bytes());
    }
    Ok(())
}

/// Emit a ModEx run as the prefix of an Ex audio / Ex video tag body.
///
/// `entries` is the in-order list of ModEx entries to chain (each
/// becomes one iteration of the spec's `while packetType == ModEx`
/// loop). `final_packet_type` is the low nibble of the trailer byte on
/// the **last** entry — the `AudioPacketType` / `VideoPacketType` the
/// outer parser must observe **after** the ModEx run finishes. Every
/// non-final entry's trailer chains by writing `MODEX_PACKET_TYPE` as
/// the next-packet-type so the parser keeps looping.
///
/// `MODEX_PACKET_TYPE` is `7` for both audio and video, matching
/// [`walk`]. `final_packet_type` must not itself be the ModEx sentinel
/// (the writer's contract is that the caller already aggregated every
/// ModEx packet into `entries`); a `MODEX_PACKET_TYPE` final value is
/// rejected with [`Error::InvalidData`].
///
/// Per-entry validation matches the parser's invariants in [`walk`]:
///
/// * `entries` MUST be non-empty (the caller should only invoke
///   `emit` when emitting a ModEx prefix at all).
/// * Each entry's `raw` size MUST be in 1..=65_536 (the wire size
///   field range).
/// * For `subtype_raw == 0` (`TimestampOffsetNano`), `raw` MUST be at
///   least 3 bytes, AND the encoded UI24 BE in `raw[0..3]` MUST equal
///   the typed payload's `offset_ns` (otherwise the entry is internally
///   inconsistent — typical bug-detector for hand-constructed
///   round-trips). The spec also caps `offset_ns` at 999_999 (one
///   millisecond minus one nanosecond per-message); values outside
///   `0..=999_999` are rejected.
/// * `subtype_raw` MUST be in 0..=15 (the wire UB[4] range).
///
/// Round-tripping `walk` ∘ `emit` recovers the same entries +
/// `total_offset_ns` accumulator, modulo the saturating-add behaviour
/// `walk` applies to the running sum (which `emit` does not need to
/// replicate — the spec already caps each entry at < 1 ms).
pub fn emit<const MODEX_PACKET_TYPE: u8>(
    out: &mut Vec<u8>,
    entries: &[ModExEntry],
    final_packet_type: u8,
) -> Result<()> {
    if entries.is_empty() {
        return Err(Error::invalid(
            "FLV ModEx: emit called with no entries (use the lead byte directly)",
        ));
    }
    if final_packet_type & 0x0F != final_packet_type {
        return Err(Error::invalid(
            "FLV ModEx: final_packet_type must fit in UB[4]",
        ));
    }
    if final_packet_type == MODEX_PACKET_TYPE {
        return Err(Error::invalid(
            "FLV ModEx: final_packet_type cannot itself be the ModEx sentinel",
        ));
    }

    let last_index = entries.len() - 1;
    for (i, entry) in entries.iter().enumerate() {
        if entry.subtype_raw & 0x0F != entry.subtype_raw {
            return Err(Error::invalid("FLV ModEx: subtype_raw must fit in UB[4]"));
        }
        // TimestampOffsetNano validation: payload >= 3 bytes, UI24 BE
        // matches `offset_ns`, and offset_ns within spec bound.
        if let ModExPayload::TimestampOffsetNano { offset_ns } = entry.payload {
            if entry.raw.len() < 3 {
                return Err(Error::invalid(
                    "FLV ModEx: TimestampOffsetNano needs >= 3 bytes",
                ));
            }
            if offset_ns > 999_999 {
                return Err(Error::invalid(
                    "FLV ModEx: TimestampOffsetNano offset_ns exceeds 999_999 ns",
                ));
            }
            let raw_ui24 = ((entry.raw[0] as u32) << 16)
                | ((entry.raw[1] as u32) << 8)
                | (entry.raw[2] as u32);
            if raw_ui24 != offset_ns {
                return Err(Error::invalid(
                    "FLV ModEx: TimestampOffsetNano raw[0..3] does not match offset_ns",
                ));
            }
            if entry.subtype_raw != 0 {
                return Err(Error::invalid(
                    "FLV ModEx: TimestampOffsetNano payload requires subtype_raw == 0",
                ));
            }
        }
        encode_size(out, entry.raw.len())?;
        out.extend_from_slice(&entry.raw);
        let next_pt = if i == last_index {
            final_packet_type
        } else {
            MODEX_PACKET_TYPE
        };
        out.push(((entry.subtype_raw & 0x0F) << 4) | (next_pt & 0x0F));
    }
    Ok(())
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
        // size prefix: UI8 path covers 1..=255 (UI8 byte 0..=0xFE);
        // UI16 escape covers 256..=65_536. Encoding payload.len() == 256
        // as UI8 would write 0xFF which the decoder treats as the escape
        // sentinel, so the boundary is strictly `<= 255` on the UI8 side.
        if payload.len() <= 255 {
            buf.push((payload.len() - 1) as u8);
        } else {
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

    // ---- emit tests ---------------------------------------------------

    #[test]
    fn encode_size_ui8_path() {
        let mut out = Vec::new();
        encode_size(&mut out, 1).unwrap();
        assert_eq!(out, vec![0x00]);
        out.clear();
        encode_size(&mut out, 255).unwrap();
        // 255 = 254 + 1 → UI8 byte = 0xFE (max non-escape).
        assert_eq!(out, vec![0xFE]);
    }

    #[test]
    fn encode_size_ui16_escape_path() {
        let mut out = Vec::new();
        encode_size(&mut out, 256).unwrap();
        // 256 cannot be encoded as UI8 (would write 0xFF, the escape
        // sentinel). UI16 path: 0xFF + UI16 BE (256 - 1 = 0x00FF).
        assert_eq!(out, vec![0xFF, 0x00, 0xFF]);
        out.clear();
        encode_size(&mut out, 65_536).unwrap();
        // UI16 BE = 0xFFFF.
        assert_eq!(out, vec![0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn encode_size_rejects_out_of_range() {
        let mut out = Vec::new();
        assert!(encode_size(&mut out, 0).is_err());
        assert!(encode_size(&mut out, 65_537).is_err());
    }

    #[test]
    fn emit_single_entry_round_trips_through_walk() {
        let entry = ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 1000 },
            raw: vec![0x00, 0x03, 0xE8],
        };
        let mut out = Vec::new();
        emit::<7>(&mut out, std::slice::from_ref(&entry), 1).unwrap();
        // Round-trip: walk should recover the same entry, with the
        // final packet type reported as 1.
        let (cur, pt, entries, ns) = walk::<7>(&out, 0, 7).unwrap();
        assert_eq!(cur, out.len());
        assert_eq!(pt, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], entry);
        assert_eq!(ns, 1000);
    }

    #[test]
    fn emit_chain_of_two_round_trips_through_walk() {
        let entries = vec![
            ModExEntry {
                subtype_raw: 0,
                payload: ModExPayload::TimestampOffsetNano { offset_ns: 100 },
                raw: vec![0x00, 0x00, 0x64],
            },
            ModExEntry {
                subtype_raw: 0,
                payload: ModExPayload::TimestampOffsetNano { offset_ns: 200 },
                raw: vec![0x00, 0x00, 0xC8],
            },
        ];
        let mut out = Vec::new();
        emit::<7>(&mut out, &entries, 2).unwrap();
        let (cur, pt, parsed, ns) = walk::<7>(&out, 0, 7).unwrap();
        assert_eq!(cur, out.len());
        assert_eq!(pt, 2);
        assert_eq!(parsed, entries);
        assert_eq!(ns, 300);
    }

    #[test]
    fn emit_chain_then_resolves_to_modex_in_caller_would_continue() {
        // Sanity: the final trailer carries whatever low nibble we ask
        // for. `walk` exits as soon as the low nibble != 7, so emitting
        // resolved=ModEx (7) is rejected (callers should aggregate
        // every ModEx into entries before emit).
        let entry = ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 100 },
            raw: vec![0x00, 0x00, 0x64],
        };
        let mut out = Vec::new();
        assert!(emit::<7>(&mut out, std::slice::from_ref(&entry), 7).is_err());
    }

    #[test]
    fn emit_reserved_subtype_round_trips() {
        let entry = ModExEntry {
            subtype_raw: 5,
            payload: ModExPayload::Reserved { subtype_raw: 5 },
            raw: vec![0xCA, 0xFE, 0xBA, 0xBE],
        };
        let mut out = Vec::new();
        emit::<7>(&mut out, std::slice::from_ref(&entry), 1).unwrap();
        let (cur, pt, parsed, ns) = walk::<7>(&out, 0, 7).unwrap();
        assert_eq!(cur, out.len());
        assert_eq!(pt, 1);
        assert_eq!(parsed, vec![entry]);
        assert_eq!(ns, 0);
    }

    #[test]
    fn emit_handles_256_byte_payload_via_escape_round_trip() {
        // 256-byte payload triggers the UI16 escape (UI8 cannot express
        // 256 without colliding with the sentinel). Subtype is reserved
        // so the parser doesn't impose UI24 validation.
        let payload: Vec<u8> = (0..256).map(|i| (i & 0xFF) as u8).collect();
        let entry = ModExEntry {
            subtype_raw: 0xF,
            payload: ModExPayload::Reserved { subtype_raw: 0xF },
            raw: payload.clone(),
        };
        let mut out = Vec::new();
        emit::<7>(&mut out, std::slice::from_ref(&entry), 1).unwrap();
        // The first byte after the lead must be 0xFF (escape sentinel)
        // since 256 cannot be encoded as a literal UI8.
        assert_eq!(out[0], 0xFF);
        let (cur, pt, parsed, _) = walk::<7>(&out, 0, 7).unwrap();
        assert_eq!(cur, out.len());
        assert_eq!(pt, 1);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].raw.len(), 256);
        assert_eq!(parsed[0].raw, payload);
    }

    #[test]
    fn emit_rejects_empty_entries() {
        let mut out = Vec::new();
        assert!(emit::<7>(&mut out, &[], 1).is_err());
    }

    #[test]
    fn emit_rejects_final_packet_type_out_of_nibble_range() {
        let entry = ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 100 },
            raw: vec![0x00, 0x00, 0x64],
        };
        let mut out = Vec::new();
        assert!(emit::<7>(&mut out, std::slice::from_ref(&entry), 0x10).is_err());
    }

    #[test]
    fn emit_rejects_subtype_out_of_nibble_range() {
        let entry = ModExEntry {
            subtype_raw: 0x10,
            payload: ModExPayload::Reserved { subtype_raw: 0x10 },
            raw: vec![0xAA],
        };
        let mut out = Vec::new();
        assert!(emit::<7>(&mut out, std::slice::from_ref(&entry), 1).is_err());
    }

    #[test]
    fn emit_rejects_timestamp_offset_nano_with_short_raw() {
        let entry = ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 100 },
            raw: vec![0xAA, 0xBB], // < 3 bytes
        };
        let mut out = Vec::new();
        assert!(emit::<7>(&mut out, std::slice::from_ref(&entry), 1).is_err());
    }

    #[test]
    fn emit_rejects_timestamp_offset_nano_with_mismatched_raw_ui24() {
        let entry = ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 200 },
            raw: vec![0x00, 0x00, 0x64], // encodes 100, not 200
        };
        let mut out = Vec::new();
        assert!(emit::<7>(&mut out, std::slice::from_ref(&entry), 1).is_err());
    }

    #[test]
    fn emit_rejects_timestamp_offset_nano_above_one_million_ns() {
        let entry = ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano {
                offset_ns: 1_000_000,
            },
            raw: vec![0x0F, 0x42, 0x40], // encodes 1_000_000 BE
        };
        let mut out = Vec::new();
        assert!(emit::<7>(&mut out, std::slice::from_ref(&entry), 1).is_err());
    }

    #[test]
    fn emit_rejects_timestamp_offset_nano_subtype_not_zero() {
        let entry = ModExEntry {
            subtype_raw: 1,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 100 },
            raw: vec![0x00, 0x00, 0x64],
        };
        let mut out = Vec::new();
        assert!(emit::<7>(&mut out, std::slice::from_ref(&entry), 1).is_err());
    }

    #[test]
    fn emit_max_ns_999_999_round_trip() {
        // Boundary: spec cap is 999_999 ns; the writer accepts it.
        let entry = ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 999_999 },
            raw: vec![0x0F, 0x42, 0x3F], // 999_999 = 0x0F423F
        };
        let mut out = Vec::new();
        emit::<7>(&mut out, std::slice::from_ref(&entry), 1).unwrap();
        let (_, _, parsed, ns) = walk::<7>(&out, 0, 7).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].timestamp_offset_nano(), Some(999_999));
        assert_eq!(ns, 999_999);
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
