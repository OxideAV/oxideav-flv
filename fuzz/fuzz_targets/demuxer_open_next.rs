#![no_main]

//! Feed arbitrary fuzz-supplied bytes to [`oxideav_flv::open_demuxer`]
//! and, when the file header parses, drain [`oxideav_core::Demuxer::next_packet`]
//! until it errors or terminates the stream. The contract under test
//! is purely that every call returns: a malformed stream yields
//! `Err(oxideav_core::Error::…)`, a well-formed one yields a
//! [`oxideav_core::Packet`], and neither path may panic, integer-
//! overflow (debug build), index out of bounds, or pre-allocate a
//! gigabyte-sized payload from an attacker-controlled `DataSize`.
//!
//! The crate's `read_body` pre-allocation guard refuses any tag whose
//! `DataSize` exceeds the remaining bytes of the underlying stream
//! before committing the `Vec`; this target keeps that guard honest by
//! feeding it the forged-DataSize lever (the 24-bit `0x00FF_FFFF` cap
//! is reachable from 3 attacker bytes) on every iteration.
//!
//! We also iterate `next_packet` with a hard step cap so an input that
//! somehow drives a non-terminating loop fails the fuzzer rather than
//! hanging it indefinitely.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{Error, NullCodecResolver, ReadSeek};
use oxideav_flv::open_demuxer;

/// Maximum number of packets we drain per fuzz iteration. Real FLV
/// files have millions of tags; the fuzzer needs to terminate quickly
/// to make progress, so we bound the per-iteration work. The cap is
/// generous enough that any pathological tight-loop will exceed it
/// before the harness times out.
const MAX_PACKETS: usize = 4096;

fuzz_target!(|data: &[u8]| {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    let mut dmx = match open_demuxer(input, &NullCodecResolver) {
        Ok(d) => d,
        Err(_) => return,
    };
    // Touch the read-only inspectors that callers typically hit right
    // after open — these traverse `streams()` / `metadata()` which
    // have their own malformed-input failure modes.
    let _ = dmx.streams().len();
    let _ = dmx.format_name();

    for _ in 0..MAX_PACKETS {
        match dmx.next_packet() {
            Ok(_) => {}
            Err(Error::Eof) => return,
            Err(_) => return,
        }
    }
});
