//! Injection-robustness tests: hand-crafted adversarial FLV blobs that
//! exercise the demuxer's parser paths against truncation, oversize
//! length fields, malformed AMF0 script data, and forged tag-type
//! bytes. The guarantee under test is **not** "extracts media" — most
//! of these inputs *cannot* yield media — it is "never panics, never
//! allocates a gigabyte, never spins forever; either errors cleanly
//! with [`Error::InvalidData`] / [`Error::Eof`] / [`Error::Io`], or
//! degrades to a stream the consumer can stop on the first
//! `next_packet()` call."
//!
//! The blob constructors below mirror the `make_flv` / `make_tag`
//! helpers in `src/demuxer.rs#tests` but live in an integration test
//! file so they exercise the publicly exported API surface, not the
//! private one. No fixtures on disk — every adversarial input is
//! generated in-process so the test stays hermetic.
//!
//! Spec references:
//! * Adobe `video_file_format_spec_v10_1.pdf`, §E.4.1 (`TagType`),
//!   §E.4 (`PreviousTagSize`), §E.4.4 (`SCRIPTDATA`), §E.5
//!   (`onMetaData` properties).
//! * AMF0 §2 (type markers 0x00..0x10).

use std::io::Cursor;

use oxideav_core::{Error, NullCodecResolver, ReadSeek};
use oxideav_flv::open_demuxer;

// ---- blob builders --------------------------------------------------------

/// Build a minimal 13-byte FLV preamble (9-byte header + 4-byte first
/// `PreviousTagSize` zero). Per spec §E.2 every well-formed file starts
/// with this prefix; the `audio | video` flags byte is producer-chosen.
fn flv_preamble(flags: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(13);
    out.extend_from_slice(b"FLV\x01");
    out.push(flags);
    out.extend_from_slice(&9u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

/// Assemble an 11-byte tag header in front of `body` using the given
/// `tag_type` byte (low 5 bits = TagType, bit 0x20 = Filter flag).
/// `data_size_override` lets the caller forge a UI24 length that does
/// **not** match `body.len()` (this is the injection lever).
fn tag_with_forged_size(
    tag_type: u8,
    timestamp_ms: u32,
    body: &[u8],
    data_size_override: u32,
) -> Vec<u8> {
    let mut t = Vec::with_capacity(11 + body.len());
    t.push(tag_type);
    t.push((data_size_override >> 16) as u8);
    t.push((data_size_override >> 8) as u8);
    t.push(data_size_override as u8);
    t.push((timestamp_ms >> 16) as u8);
    t.push((timestamp_ms >> 8) as u8);
    t.push(timestamp_ms as u8);
    t.push((timestamp_ms >> 24) as u8);
    t.extend_from_slice(&[0, 0, 0]);
    t.extend_from_slice(body);
    t
}

/// Wrap a single forged tag in the FLV preamble + trailing
/// `PreviousTagSize` (the trailer can also be omitted to exercise the
/// "trailing-prev-size truncation" branch — that's `wrap_no_trailer`).
fn wrap(tag_bytes: &[u8]) -> Vec<u8> {
    let mut out = flv_preamble(0x05);
    out.extend_from_slice(tag_bytes);
    // PreviousTagSize for this tag (claimed length is the wire length).
    out.extend_from_slice(&(tag_bytes.len() as u32).to_be_bytes());
    out
}

fn wrap_no_trailer(tag_bytes: &[u8]) -> Vec<u8> {
    let mut out = flv_preamble(0x05);
    out.extend_from_slice(tag_bytes);
    out
}

/// Try to open `bytes` as FLV and walk every packet until EOF or
/// error. Returns the count of packets that came out cleanly plus the
/// terminal `Result`. Never panics — that's the contract under test.
fn drain(bytes: Vec<u8>) -> (usize, Result<(), Error>) {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = match open_demuxer(input, &NullCodecResolver) {
        Ok(d) => d,
        Err(e) => return (0, Err(e)),
    };
    let mut count = 0usize;
    loop {
        match dmx.next_packet() {
            Ok(_) => {
                count += 1;
                // Hard cap so a hypothetical infinite-loop regression
                // doesn't wedge the test runner.
                if count > 10_000 {
                    return (count, Err(Error::invalid("packet flood")));
                }
            }
            Err(Error::Eof) => return (count, Ok(())),
            Err(e) => return (count, Err(e)),
        }
    }
}

// ---- header / preamble injection -----------------------------------------

#[test]
fn empty_input_errors_cleanly() {
    let (_, r) = drain(Vec::new());
    assert!(r.is_err(), "empty input must not yield a working demuxer");
}

#[test]
fn header_only_errors_cleanly() {
    // FLV preamble + trailer zero, no tags at all → no audio/video
    // discovered, open() must error.
    let (_, r) = drain(flv_preamble(0x05));
    assert!(
        matches!(r, Err(Error::InvalidData(_))),
        "header-only file must yield InvalidData, got {r:?}"
    );
}

#[test]
fn truncated_header_errors_cleanly() {
    // First 4 bytes only — `FLV\x01`. Header reader must reject.
    let (_, r) = drain(b"FLV\x01".to_vec());
    assert!(r.is_err(), "5-byte input must not open");
}

#[test]
fn bad_signature_errors_cleanly() {
    // 9 bytes that pass length but fail the magic gate.
    let bytes = b"XYZ\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
    let (_, r) = drain(bytes);
    assert!(r.is_err(), "non-FLV magic must not open");
}

// ---- DataSize injection (the OOM lever) ----------------------------------

#[test]
fn forged_oversize_data_size_rejected_before_alloc() {
    // Forge a tag header that claims 16 MB of payload but the file is
    // 30 bytes. The pre-allocation guard in `read_body` must reject
    // *before* committing a 16 MB Vec.
    let forged = tag_with_forged_size(0x09, 0, &[0x12], 0x00FF_FFFF);
    let bytes = wrap(&forged);
    let (_, r) = drain(bytes);
    assert!(
        matches!(r, Err(Error::InvalidData(_)) | Err(Error::Io(_))),
        "forged-oversize tag must error, got {r:?}"
    );
}

#[test]
fn forged_oversize_audio_tag_first_position_rejected() {
    // Same lever, but on the first tag (so the *open()* path is the
    // one that has to refuse — discovery, not next_packet()).
    let forged = tag_with_forged_size(0x08, 0, &[0xAF], 0x00FF_FFFF);
    let bytes = wrap(&forged);
    let (_, r) = drain(bytes);
    assert!(
        matches!(r, Err(Error::InvalidData(_)) | Err(Error::Io(_))),
        "forged-oversize first-tag must abort open() cleanly, got {r:?}"
    );
}

#[test]
fn data_size_one_byte_past_eof_rejected() {
    // Body is one byte short of the claimed size — guard must fire
    // because remaining = body.len(), but DataSize = body.len() + 1.
    let body = [0x14u8, 0x00, 0x42];
    let forged = tag_with_forged_size(0x09, 0, &body, body.len() as u32 + 1);
    let bytes = wrap_no_trailer(&forged); // no trailer → minimum stream length
    let (_, r) = drain(bytes);
    assert!(
        matches!(r, Err(Error::InvalidData(_)) | Err(Error::Io(_))),
        "off-by-one truncation must error, got {r:?}"
    );
}

// ---- truncated trailer ----------------------------------------------------

#[test]
fn truncated_previous_tag_size_errors_cleanly() {
    // Valid tag, valid body, but the trailing 4-byte `PreviousTagSize`
    // is missing. `read_u32_be` on the trailer must surface
    // `UnexpectedEof` (mapped to `Error::Io`), not panic.
    let body = [0x14u8, 0x00, 0x42];
    let tag = tag_with_forged_size(0x09, 0, &body, body.len() as u32);
    let mut bytes = flv_preamble(0x05);
    bytes.extend_from_slice(&tag);
    // intentionally no trailer
    let (_, r) = drain(bytes);
    assert!(r.is_err(), "missing trailer must error cleanly");
}

// ---- malformed script data ------------------------------------------------

fn script_tag_with_body(body: &[u8]) -> Vec<u8> {
    tag_with_forged_size(0x12, 0, body, body.len() as u32)
}

#[test]
fn script_tag_with_unknown_amf_marker_does_not_crash() {
    // 0xFF is not a defined AMF0 marker (§2 only covers 0x00..=0x10).
    // `parse_script_body` swallows the parse error and the demuxer
    // continues — `open()` then fails with InvalidData because no
    // audio/video tag was discovered, which is acceptable.
    let bytes = wrap(&script_tag_with_body(&[0xFFu8]));
    let (_, r) = drain(bytes);
    assert!(r.is_err(), "script-only file still has no media");
}

#[test]
fn script_tag_with_truncated_string_does_not_crash() {
    // AMF0 String marker 0x02 + claimed length 100 + only 4 bytes
    // follow. `read_utf8` must reject without slicing past `data`.
    let mut body = vec![0x02];
    body.extend_from_slice(&(100u16).to_be_bytes());
    body.extend_from_slice(b"abcd");
    let bytes = wrap(&script_tag_with_body(&body));
    let (_, r) = drain(bytes);
    assert!(r.is_err());
}

#[test]
fn script_tag_with_huge_long_string_length_does_not_crash() {
    // AMF0 LongString marker 0x0C + claimed u32 length 0xFFFF_FFFF +
    // a few real bytes. `read_utf8` uses `pos.saturating_add(len)` so
    // it must not panic on the addition and must reject because
    // pos+len > body.len().
    let mut body = vec![0x0C];
    body.extend_from_slice(&u32::MAX.to_be_bytes());
    body.extend_from_slice(b"abcd");
    let bytes = wrap(&script_tag_with_body(&body));
    let (_, r) = drain(bytes);
    assert!(r.is_err());
}

#[test]
fn script_tag_with_negative_terminator_does_not_loop() {
    // Anonymous-object marker 0x03 with no terminator bytes inside the
    // tag body. `parse_object_body` must hit the truncation guard, not
    // spin reading past `data.len()`.
    let body = vec![0x03];
    let bytes = wrap(&script_tag_with_body(&body));
    let (_, r) = drain(bytes);
    assert!(r.is_err());
}

#[test]
fn on_metadata_with_non_object_value_does_not_crash() {
    // "onMetaData" name + bare Number (not Object). `parse_on_metadata`
    // bails on the `_ => return` branch — the demuxer should not error
    // open()ing on this alone, but it has no media tags so the
    // outcome is still InvalidData.
    let mut body = Vec::new();
    body.push(0x02);
    body.extend_from_slice(&("onMetaData".len() as u16).to_be_bytes());
    body.extend_from_slice(b"onMetaData");
    body.push(0x00); // Number marker
    body.extend_from_slice(&42.0_f64.to_be_bytes());
    let bytes = wrap(&script_tag_with_body(&body));
    let (_, r) = drain(bytes);
    assert!(r.is_err(), "no media after malformed onMetaData");
}

// ---- forged tag-type byte -------------------------------------------------

#[test]
fn unknown_tag_type_skipped_until_eof() {
    // Tag type 0x05 is not defined (audio=0x08, video=0x09, script=0x12).
    // Discovery skips it via `header.data_size as u64 + 4` and then
    // falls off the end. `open()` must error because no media was found.
    let tag = tag_with_forged_size(0x05, 0, &[0xDE, 0xAD], 2);
    let bytes = wrap(&tag);
    let (_, r) = drain(bytes);
    assert!(
        matches!(r, Err(Error::InvalidData(_))),
        "unknown tag types alone must not yield a working demuxer, got {r:?}"
    );
}

#[test]
fn forged_filter_flag_truncated_preamble_does_not_crash() {
    // Filter bit set (0x20) on a video tag, but the body is too short
    // to contain a NumFilters + FilterName + Length preamble.
    // `build_encrypted_packet` must surface InvalidData / Io rather
    // than panic. With only an audio tag below it, discovery may
    // succeed; the filtered tag walk happens during `next_packet`.
    let audio = tag_with_forged_size(0x08, 0, &[0x2F, 0x00], 2);
    let filtered = tag_with_forged_size(0x09 | 0x20, 0, &[0x01], 1);
    let mut bytes = flv_preamble(0x05);
    bytes.extend_from_slice(&audio);
    bytes.extend_from_slice(&(audio.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&filtered);
    bytes.extend_from_slice(&(filtered.len() as u32).to_be_bytes());
    let (_, _r) = drain(bytes);
    // The exact error / packet shape depends on discovery ordering;
    // what matters is "no panic, no OOM, no infinite loop" — drain()
    // would have hit its cap or panicked otherwise.
}

// ---- zero-length payload --------------------------------------------------

#[test]
fn zero_length_audio_tag_is_skipped_not_panic() {
    // Audio tag with DataSize=0 — discovery checks `!body.is_empty()`
    // and refuses to mint a stream. The video tag is real, so the
    // demuxer should still produce one video stream + one packet.
    let empty_audio = tag_with_forged_size(0x08, 0, &[], 0);
    let video = tag_with_forged_size(0x09, 0, &[0x14, 0x00, 0x42], 3);
    let mut bytes = flv_preamble(0x05);
    bytes.extend_from_slice(&empty_audio);
    bytes.extend_from_slice(&(empty_audio.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&video);
    bytes.extend_from_slice(&(video.len() as u32).to_be_bytes());
    let (count, r) = drain(bytes);
    assert!(r.is_ok(), "zero-length audio must not abort, got {r:?}");
    assert_eq!(count, 1, "expected exactly one video packet");
}

// ---- mid-stream truncation -----------------------------------------------

#[test]
fn mid_tag_truncation_after_discovery_errors_cleanly() {
    // Build a file with two valid tags + a third whose 11-byte header
    // is fully present but whose body is truncated to one byte.
    let audio = tag_with_forged_size(0x08, 0, &[0x2F, 0x00, 0x00], 3);
    let video = tag_with_forged_size(0x09, 0, &[0x14, 0x00, 0x42], 3);
    let forged_third = tag_with_forged_size(0x09, 10, &[0x14], 5); // claims 5, only 1 follows

    let mut bytes = flv_preamble(0x05);
    bytes.extend_from_slice(&audio);
    bytes.extend_from_slice(&(audio.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&video);
    bytes.extend_from_slice(&(video.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&forged_third);
    // no trailer — body itself is already truncated
    let (_count, r) = drain(bytes);
    assert!(r.is_err(), "mid-stream truncation must error on read");
}

// ---- repeated tags-with-zero-size do not loop forever ---------------------

#[test]
fn flood_of_zero_size_tags_terminates() {
    // 256 audio tags with DataSize=0 — each iteration of discovery
    // advances by 11+4 = 15 bytes (the trailer is a UI32). The
    // discovery cap (MAX_DISCOVERY_TAGS) is 256; this stresses it.
    let mut bytes = flv_preamble(0x05);
    let empty = tag_with_forged_size(0x08, 0, &[], 0);
    for _ in 0..512 {
        bytes.extend_from_slice(&empty);
        bytes.extend_from_slice(&0u32.to_be_bytes());
    }
    let (_count, r) = drain(bytes);
    // Discovery walks up to 256 tags then stops; no audio body was
    // ever non-empty, so open() errors. Either path is fine — what
    // matters is termination.
    let _ = r;
}
