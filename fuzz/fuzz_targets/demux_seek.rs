#![no_main]

//! Tag-structured demuxer target.
//!
//! Arbitrary bytes rarely get past the 9-byte file header, so this
//! target builds the file from a recipe instead: the fuzz input is read
//! as a list of records, each emitted as a well-formed tag (type, time,
//! length and back-pointer all consistent) whose *body* is fuzz bytes —
//! which puts the AMF, Ex-header, ModEx, multitrack and encryption
//! parsers directly in the line of fire. Recipe opcodes also inject raw
//! bytes, forge the DataSize, corrupt the back-pointer and truncate the
//! file, covering the framing errors as well.
//!
//! The file is then driven through `open` and an interleaving of
//! `next_packet`, `seek_to` and `set_active_streams` calls taken from
//! the rest of the input. Nothing may panic, overflow, allocate
//! without bound or fail to terminate; a seek that succeeds must leave
//! the demuxer in a state where reading continues or ends cleanly.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};
use oxideav_flv::open_demuxer_concrete;

struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn u8(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }
    fn take(&mut self, n: usize) -> &'a [u8] {
        let n = n.min(self.0.len());
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        a
    }
}

const MAX_TAGS: usize = 600;
const MAX_ACTIONS: usize = 64;

fuzz_target!(|data: &[u8]| {
    let mut r = Bytes(data);
    let Some(flags) = r.u8() else { return };
    let Some(n_actions) = r.u8() else { return };
    let actions = r.take((n_actions as usize).min(MAX_ACTIONS) * 4);

    let mut file = vec![b'F', b'L', b'V', 1, flags & 0x05, 0, 0, 0, 9];
    let mut prev = 0u32;
    let mut ts = 0u32;
    for _ in 0..MAX_TAGS {
        let Some(op) = r.u8() else { break };
        let kind = match op & 0x07 {
            0 | 1 => 0x08,
            2 | 3 => 0x09,
            4 => 0x12,
            5 => 0x28 | (op >> 7), // filtered audio / video
            6 => op >> 3,          // any tag type
            _ => {
                // Raw injection: bytes that follow no framing at all.
                let n = r.u8().unwrap_or(0) as usize;
                file.extend_from_slice(r.take(n));
                continue;
            }
        };
        // Time: small steps mostly, an arbitrary jump now and then.
        let step = r.u8().unwrap_or(0);
        if step == 0xFF {
            let j = r.take(4);
            let mut b = [0u8; 4];
            b[..j.len()].copy_from_slice(j);
            ts = u32::from_be_bytes(b);
        } else {
            ts = ts.wrapping_add(step as u32);
        }
        let len = match r.u8().unwrap_or(0) {
            n @ 0..=0xF7 => n as usize,
            n => ((n as usize - 0xF7) << 8) | r.u8().unwrap_or(0) as usize,
        };
        let body = r.take(len);
        let quirk = (op >> 3) & 0x0F;
        // Back-pointer: right, or wrong when the recipe says so.
        let back = if quirk == 1 { prev ^ 0x0101 } else { prev };
        file.extend_from_slice(&back.to_be_bytes());
        // DataSize: right, or forged.
        let size = match quirk {
            2 => body.len() as u32 + 1,
            3 => 0x00FF_FFFF,
            _ => body.len() as u32,
        };
        file.push(kind);
        file.extend_from_slice(&size.to_be_bytes()[1..]);
        file.extend_from_slice(&ts.to_be_bytes()[1..]);
        file.push((ts >> 24) as u8);
        file.extend_from_slice(&[0, 0, if quirk == 4 { 1 } else { 0 }]);
        file.extend_from_slice(body);
        prev = 11 + body.len() as u32;
    }
    file.extend_from_slice(&prev.to_be_bytes());
    if flags & 0x80 != 0 && !file.is_empty() {
        // Cut the file somewhere.
        let cut = (flags as usize * 131) % file.len();
        file.truncate(cut.max(9));
    }

    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let Ok(mut d) = open_demuxer_concrete(input, &NullCodecResolver) else {
        return;
    };
    let streams = d.streams().len() as u32;
    let mut budget = 4 * MAX_TAGS;
    for a in actions.chunks(4) {
        match a[0] & 0x03 {
            0 => {
                let mut t = [0u8; 4];
                t[..a.len() - 1].copy_from_slice(&a[1..]);
                let pts = i64::from(u32::from_be_bytes(t)) >> (a[0] >> 3);
                let stream = u32::from(a[0] >> 2) & 1;
                match d.seek_to(stream, pts) {
                    Ok(_) => {}
                    Err(_) => assert!(stream >= streams, "seek failed on a valid stream"),
                }
            }
            1 => d.set_active_streams(&[u32::from(a[0] >> 2) & 1]),
            2 => d.set_active_streams(&[0, 1]),
            _ => {}
        }
        for _ in 0..=(a[0] >> 4) {
            if budget == 0 || d.next_packet().is_err() {
                break;
            }
            budget -= 1;
        }
        let _ = d.stats();
    }
    while budget > 0 && d.next_packet().is_ok() {
        budget -= 1;
    }
    let _ = d.metadata().len();
    let _ = d.duration_micros();
});
