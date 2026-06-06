#![no_main]

//! Feed arbitrary fuzz-supplied bytes to [`oxideav_flv::parse_amf3_value`]
//! starting at offset 0. The decoder must always return a `Result` and
//! never panic / abort / OOM.
//!
//! AMF3 (Adobe AMF 3 Specification, Dec 2007) has 13 type markers.
//! The interesting failure modes:
//!
//! U29 (§1.3.1) — variable-length unsigned 29-bit integer. Values that
//! hit `2^29 - 1` exactly must succeed; the 4-byte form must not
//! overflow into a 30-bit count. UTF-8-vr (§1.3.2) reuses U29 with the
//! low bit as a literal/reference flag — a reference into an as-yet-
//! empty table is a wire error, not a panic.
//!
//! Traits (§3.12) — three forms: inline (sealed count + class name +
//! sealed property names), traits-ref (back-reference), and traits-ext
//! (externalizable — zero body bytes consumed). A traits-ref to an
//! unpopulated slot must error cleanly.
//!
//! Complex-object references (§2.2) — circular Array / Object graphs
//! are valid AMF3 and the decoder reserves table slots before
//! descending; a forged reference past the end must error rather than
//! recurse infinitely.

use libfuzzer_sys::fuzz_target;
use oxideav_flv::parse_amf3_value;

fuzz_target!(|data: &[u8]| {
    let _ = parse_amf3_value(data, 0);
});
