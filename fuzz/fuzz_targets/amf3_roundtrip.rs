#![no_main]

//! Differential round-trip fuzz for the AMF3 encoder
//! [`oxideav_flv::write_amf3_value`] against the decoder
//! [`oxideav_flv::parse_amf3_value`].
//!
//! The invariant under test is **encode↔decode stability**: for any byte
//! string the decoder accepts, re-encoding the decoded value and
//! decoding it again must yield the identical value. That is, `decode`
//! followed by `encode` followed by `decode` is a fixed point. This
//! catches any asymmetry between the two halves — a marker the encoder
//! emits differently from how the decoder reads it, a U29 length the
//! encoder rounds wrong, a string-table index that drifts, an
//! externalizable object that loses its flag, etc.
//!
//! We do NOT require `encode(decode(x)) == x` byte-for-byte: the
//! decoder accepts non-canonical streams (e.g. a string sent as a
//! literal where a reference was possible), and the literal-first
//! encoder produces a canonical form. But re-decoding the canonical
//! form must reproduce the same value graph, and a third pass must be a
//! true fixed point (`encode(v) == encode(decode(encode(v)))`).

use libfuzzer_sys::fuzz_target;
use oxideav_flv::{parse_amf3_value, write_amf3_value};

fuzz_target!(|data: &[u8]| {
    // Phase 1 — decode whatever the fuzzer gave us. Most inputs error
    // cleanly; that path is already covered by `amf3_parse`. We only
    // care about inputs the decoder accepts.
    let Ok((value, _consumed)) = parse_amf3_value(data, 0) else {
        return;
    };

    // Phase 2 — re-encode the decoded value. The encoder may legitimately
    // reject a value it cannot represent (e.g. an `Integer` outside the
    // signed-29-bit range can never come from the decoder, so this should
    // not fire, but we tolerate it rather than assert).
    let mut enc = Vec::new();
    let Ok(()) = write_amf3_value(&mut enc, &value) else {
        return;
    };

    // Phase 3 — the canonical encoding must decode back to the SAME value
    // and consume every byte we wrote.
    let (value2, consumed2) = parse_amf3_value(&enc, 0)
        .expect("re-encoded AMF3 must decode");
    assert_eq!(consumed2, enc.len(), "decoder left trailing encoder bytes");
    assert_eq!(value, value2, "value drifted across encode→decode");

    // Phase 4 — fixed point: a second encode must be byte-identical to
    // the first (the canonical form is stable under repeated encoding).
    let mut enc2 = Vec::new();
    write_amf3_value(&mut enc2, &value2).expect("second encode");
    assert_eq!(enc, enc2, "canonical encoding is not a fixed point");
});
