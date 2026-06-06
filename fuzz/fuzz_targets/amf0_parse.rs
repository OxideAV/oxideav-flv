#![no_main]

//! Feed arbitrary fuzz-supplied bytes to [`oxideav_flv::parse_amf0_value`]
//! starting at offset 0. The decoder must always return a `Result` and
//! never panic / abort / OOM, regardless of how malformed the input is.
//!
//! AMF0 has 16 type markers (Adobe AMF0 Specification §2.1), and the
//! interesting failure modes cluster around three:
//!
//!  * `LongString` (`0x0C`) — claims a `u32`-length body. The decoder
//!    must reject lengths that exceed the remaining buffer rather than
//!    allocate gigabytes.
//!  * `Object` (`0x03`) — keys are length-prefixed `UTF-8` blobs
//!    terminated by the `0x00 0x00 0x09` `ObjectEnd` marker. A missing
//!    terminator or a forged key length must not spin the parser.
//!  * `TypedObject` (`0x10`) — same as `Object` but prefixed with a
//!    class alias. A missing terminator in the alias body is the same
//!    failure shape.
//!  * `0x11` AVM+ switch — lifts the byte stream into the AMF3 decoder.
//!    The AMF3 module has its own fuzz target, but exercising the
//!    AMF0→AMF3 transition here keeps the boundary honest.

use libfuzzer_sys::fuzz_target;
use oxideav_flv::parse_amf0_value;

fuzz_target!(|data: &[u8]| {
    let _ = parse_amf0_value(data, 0);
});
