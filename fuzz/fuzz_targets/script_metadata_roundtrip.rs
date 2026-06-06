#![no_main]

//! Synthesise a minimal FLV from fuzz-controlled scalar `onMetaData`
//! properties, then re-parse it through [`oxideav_flv::open_demuxer`]
//! and assert the demuxer at least opens it and yields a non-erroring
//! `metadata()` view. The contract under test is that the producer
//! side (the muxer slice — `header::write`,
//! `script::write_on_metadata`, `tag::write_first_previous_tag_size`)
//! and the consumer side agree on every byte the writer can emit, for
//! any combination of fuzz-controlled property values.
//!
//! The fuzzer derives:
//!
//!  * the leading `audio` / `video` header flag bits from the first
//!    fuzz byte;
//!  * a sequence of `(kind, key, value)` properties from the remaining
//!    bytes — kind 0 = `Number(f64)`, 1 = `Boolean(bool)`, 2 =
//!    `String(...)`; keys are short ASCII (length-prefixed by the next
//!    byte modulo 16) so the AMF0 key encoding stays in its UTF-8
//!    happy path. The bag accepts any `&str` key including the empty
//!    string and arbitrary UTF-8; we deliberately keep the fuzz under
//!    ASCII because the writer's UTF-8 surface is exercised by the
//!    `script_metadata_roundtrip` of the AMF0 unit tests already.
//!
//! Goal: surface any (writer, parser) disagreement where the writer
//! happily emits bytes the parser refuses to read back. A
//! disagreement is a panic, an `Err` from `open_demuxer`, or a
//! property the writer emitted that doesn't appear in the demuxer's
//! `metadata()` map.

use std::collections::HashMap;
use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{NullCodecResolver, ReadSeek};
use oxideav_flv::{header, open_demuxer, script, script::MetadataBag, tag};

/// Maximum number of `onMetaData` properties we emit per iteration.
/// Real producers emit ~15; we go a little higher to exercise the
/// duplicate-key path the writer permits (last-write wins on the
/// demuxer side).
const MAX_PROPS: usize = 32;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let has_audio = (data[0] & 0x01) != 0;
    let has_video = (data[0] & 0x02) != 0;

    // Build the metadata bag from the rest of the fuzz bytes.
    let mut bag = MetadataBag::new();
    let mut emitted: HashMap<String, ()> = HashMap::new();
    let mut cur = &data[1..];
    for _ in 0..MAX_PROPS {
        // Need at least 1 byte for the kind tag and 1 byte for the
        // key length nibble. If we run out, stop adding.
        if cur.len() < 2 {
            break;
        }
        let kind = cur[0] % 3;
        let key_len = ((cur[1] & 0x0F) as usize).max(1);
        cur = &cur[2..];
        if cur.len() < key_len {
            break;
        }
        // Force ASCII letters so the key is always valid AMF0
        // SCRIPTDATASTRING and we stay out of the UTF-8 boundary
        // testing the AMF parser unit tests already cover.
        let key: String = cur[..key_len]
            .iter()
            .map(|b| char::from(b'a' + (b % 26)))
            .collect();
        cur = &cur[key_len..];
        if emitted.contains_key(&key) {
            // Skip duplicate keys: the writer would happily emit
            // them but the demuxer's flatten walker last-write-wins,
            // and we want the per-key assertion below to be
            // deterministic.
            continue;
        }
        match kind {
            0 => {
                if cur.len() < 8 {
                    break;
                }
                let n = f64::from_be_bytes(cur[..8].try_into().unwrap());
                cur = &cur[8..];
                // NaN / infinities don't survive AMF0 → metadata
                // string conversion meaningfully — skip them, the
                // writer accepts but the demuxer normalises.
                if !n.is_finite() {
                    continue;
                }
                bag = bag.number(&key, n);
                emitted.insert(key, ());
            }
            1 => {
                if cur.is_empty() {
                    break;
                }
                let b = cur[0] & 1 != 0;
                cur = &cur[1..];
                bag = bag.boolean(&key, b);
                emitted.insert(key, ());
            }
            2 => {
                if cur.len() < 2 {
                    break;
                }
                let s_len = (cur[0] as usize) % 16;
                cur = &cur[1..];
                if cur.len() < s_len {
                    break;
                }
                let s: String = cur[..s_len]
                    .iter()
                    .map(|b| char::from(b'a' + (b % 26)))
                    .collect();
                cur = &cur[s_len..];
                bag = bag.string(&key, &s);
                emitted.insert(key, ());
            }
            _ => unreachable!(),
        }
    }

    // Mux it.
    let mut buf = Vec::new();
    if header::write(&mut buf, has_audio, has_video).is_err() {
        return;
    }
    if tag::write_first_previous_tag_size(&mut buf).is_err() {
        return;
    }
    if script::write_on_metadata(&mut buf, &bag).is_err() {
        return;
    }

    // Demux it.
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let dmx = match open_demuxer(input, &NullCodecResolver) {
        Ok(d) => d,
        Err(_) => {
            // The mux step succeeded but open refused — that's a
            // mux/demux disagreement. Panic so the fuzzer captures it.
            panic!("muxed onMetaData failed to re-open");
        }
    };
    let meta = dmx.metadata();
    // Every key we kept must appear. Numbers come back as their
    // f64 string via the metadata flatten walker, booleans as
    // "true"/"false", strings verbatim. We only assert presence
    // because the exact value-string surface is covered by the
    // crate's own unit tests; here we want to catch the
    // writer-emitted-but-parser-dropped failure shape.
    for k in emitted.keys() {
        assert!(
            meta.iter().any(|(mk, _)| mk == k),
            "key {:?} was muxed but is missing from metadata()",
            k
        );
    }
});
