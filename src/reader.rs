//! Windowed reader over the demuxer's input.
//!
//! The FLV body is a flat run of small records (most tags are a few
//! hundred bytes), so issuing one `read` per field costs several system
//! calls per packet on an unbuffered file. [`TagReader`] keeps one
//! sliding window over the input instead: a tag is parsed in place from
//! the window and its payload is copied out exactly once, into the
//! packet.
//!
//! Three properties the demuxer relies on:
//!
//! * **No length probe.** The reader never asks the input how long it
//!   is. A record that extends past the bytes currently available is
//!   reported as "not there yet" (`fill` returns `false`) with the
//!   cursor untouched, so a truncated file ends cleanly and a file that
//!   is still being written can be retried later from the same spot.
//! * **Allocation follows real bytes.** The window grows only when it is
//!   already full of bytes that were actually read, so a forged 16 MiB
//!   `DataSize` in a 30-byte file allocates nothing beyond the initial
//!   window.
//! * **Seeks inside the window are free.** Repositioning to an offset
//!   the window already covers only moves the cursor.

use std::io::{ErrorKind, SeekFrom};

use oxideav_core::{ReadSeek, Result};

/// Initial (and steady-state) window size.
const WINDOW: usize = 64 * 1024;

/// Above this capacity the window is released once it drains, so one
/// oversized tag does not pin its buffer for the rest of the stream.
const SHRINK_ABOVE: usize = 256 * 1024;

pub(crate) struct TagReader {
    input: Box<dyn ReadSeek>,
    /// Backing storage; `buf[start..end]` is the unread window.
    buf: Vec<u8>,
    start: usize,
    end: usize,
    /// Absolute input offset of `buf[0]`.
    base: u64,
}

impl TagReader {
    /// Wrap `input`, whose cursor currently sits at absolute offset
    /// `pos`.
    pub(crate) fn new(input: Box<dyn ReadSeek>, pos: u64) -> Self {
        Self {
            input,
            buf: vec![0u8; WINDOW],
            start: 0,
            end: 0,
            base: pos,
        }
    }

    /// Absolute offset of the next unread byte.
    pub(crate) fn position(&self) -> u64 {
        self.base + self.start as u64
    }

    /// Bytes available without touching the input.
    pub(crate) fn available(&self) -> usize {
        self.end - self.start
    }

    /// The unread window (use after a successful [`fill`](Self::fill)).
    pub(crate) fn window(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    /// Make at least `n` bytes available. Returns `Ok(false)` when the
    /// input ends first; the bytes that did arrive stay in the window
    /// and the cursor does not move, so the call can be repeated once
    /// the input has grown.
    pub(crate) fn fill(&mut self, n: usize) -> Result<bool> {
        if self.available() >= n {
            return Ok(true);
        }
        let avail = self.available();
        if self.buf.len() > SHRINK_ABOVE && n <= WINDOW && avail <= WINDOW {
            // An oversized tag has passed: go back to the small window.
            let mut small = vec![0u8; WINDOW];
            small[..avail].copy_from_slice(&self.buf[self.start..self.end]);
            self.buf = small;
            self.base += self.start as u64;
            self.start = 0;
            self.end = avail;
        } else if self.start > 0 && self.start + n > self.buf.len() {
            // Slide the window to the front when the tail has no room.
            self.buf.copy_within(self.start..self.end, 0);
            self.base += self.start as u64;
            self.end -= self.start;
            self.start = 0;
        }
        while self.available() < n {
            if self.end == self.buf.len() {
                // Full of real bytes and still short: double, capped at
                // what this request needs.
                let want = (self.start + n).max(WINDOW);
                let grown = (self.buf.len() * 2).min(want.next_multiple_of(WINDOW));
                self.buf.resize(grown.max(self.buf.len() + 1), 0);
            }
            match self.input.read(&mut self.buf[self.end..]) {
                Ok(0) => return Ok(false),
                Ok(got) => self.end += got,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(true)
    }

    /// Advance the cursor by `n` bytes that are in the window.
    pub(crate) fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.available());
        self.start += n.min(self.available());
        if self.start == self.end {
            self.base += self.start as u64;
            self.start = 0;
            self.end = 0;
            if self.buf.len() > SHRINK_ABOVE {
                self.buf = vec![0u8; WINDOW];
            }
        }
    }

    /// Move the cursor to absolute offset `pos`. Free when the window
    /// already covers `pos`; otherwise one seek on the input.
    pub(crate) fn seek(&mut self, pos: u64) -> Result<()> {
        if pos >= self.base && pos <= self.base + self.end as u64 {
            self.start = (pos - self.base) as usize;
            return Ok(());
        }
        self.input.seek(SeekFrom::Start(pos))?;
        self.base = pos;
        self.start = 0;
        self.end = 0;
        if self.buf.len() > SHRINK_ABOVE {
            self.buf = vec![0u8; WINDOW];
        }
        Ok(())
    }

    /// Move the cursor forward to absolute offset `end` if the input
    /// extends that far (`end` must lie beyond the cursor). Bytes in
    /// between are stepped over, not kept. On `false` the cursor is
    /// unspecified and the caller repositions.
    pub(crate) fn reaches(&mut self, end: u64) -> Result<bool> {
        if end >= self.base && end <= self.base + self.end as u64 {
            self.start = (end - self.base) as usize;
            return Ok(true);
        }
        self.seek(end - 1)?;
        if !self.fill(1)? {
            return Ok(false);
        }
        self.consume(1);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn reader(bytes: Vec<u8>) -> TagReader {
        TagReader::new(Box::new(Cursor::new(bytes)), 0)
    }

    #[test]
    fn fill_reports_short_input_without_moving() {
        let mut r = reader(vec![1, 2, 3]);
        assert!(!r.fill(4).unwrap());
        assert_eq!(r.position(), 0);
        assert!(r.fill(3).unwrap());
        assert_eq!(r.window(), &[1, 2, 3]);
    }

    #[test]
    fn window_grows_only_with_real_bytes() {
        let mut r = reader(vec![7u8; 100]);
        // A 16 MiB request against 100 real bytes must not allocate.
        assert!(!r.fill(16 << 20).unwrap());
        assert_eq!(r.buf.len(), WINDOW);
        assert_eq!(r.available(), 100);
    }

    #[test]
    fn large_record_then_shrink() {
        let n = WINDOW * 5 + 123;
        let data: Vec<u8> = (0..n).map(|i| i as u8).collect();
        let mut r = reader(data.clone());
        r.fill(10).unwrap();
        r.consume(10);
        assert!(r.fill(n - 10).unwrap());
        assert_eq!(r.window()[..n - 10], data[10..]);
        r.consume(n - 10);
        assert_eq!(r.buf.len(), WINDOW);
        assert_eq!(r.position(), n as u64);
    }

    #[test]
    fn seek_inside_window_keeps_bytes() {
        let data: Vec<u8> = (0..200u8).collect();
        let mut r = reader(data);
        r.fill(200).unwrap();
        r.consume(150);
        r.seek(20).unwrap();
        assert_eq!(r.position(), 20);
        assert_eq!(r.window()[0], 20);
        assert!(r.reaches(120).unwrap());
        assert_eq!(r.window()[0], 120);
        assert!(!r.reaches(201).unwrap());
        r.seek(20).unwrap();
        assert!(r.reaches(200).unwrap());
        r.seek(5000).unwrap();
        assert!(!r.fill(1).unwrap());
    }
}
