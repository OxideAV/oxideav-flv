//! FLV 9-byte file header.
//!
//! Layout (Adobe FLV File Format Spec v10 §E.2):
//!
//! ```text
//!  offset  size  field
//!       0     3  Signature  ('F', 'L', 'V')
//!       3     1  Version    (u8 — 1 for all versions spec'd so far)
//!       4     1  TypeFlags  (bit 0x04 = audio, 0x01 = video, rest reserved)
//!       5     4  DataOffset (u32 big-endian — total header size, = 9)
//! ```

use std::io::Read;

use oxideav_core::{Error, Result};

/// Three-byte FLV file magic (`FLV`).
pub const FLV_SIGNATURE: &[u8; 3] = b"FLV";

/// Decoded FLV file header.
#[derive(Clone, Copy, Debug)]
pub struct FlvHeader {
    pub version: u8,
    /// True when bit 0x04 is set in the flags byte.
    pub has_audio: bool,
    /// True when bit 0x01 is set in the flags byte.
    pub has_video: bool,
    /// Byte offset from start-of-file to the first byte after the header.
    /// Per spec this is always 9 for version 1.
    pub data_offset: u32,
}

impl FlvHeader {
    /// Parse a 9-byte FLV header from a reader, validating the magic
    /// bytes and `DataOffset`. Returns `Error::InvalidData` on any
    /// violation.
    pub fn read<R: Read + ?Sized>(r: &mut R) -> Result<Self> {
        let mut buf = [0u8; 9];
        r.read_exact(&mut buf)?;
        if &buf[0..3] != FLV_SIGNATURE {
            return Err(Error::invalid("FLV: signature mismatch"));
        }
        let version = buf[3];
        if version != 1 {
            return Err(Error::invalid(format!(
                "FLV: unsupported version {version}"
            )));
        }
        let flags = buf[4];
        let data_offset = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);
        if data_offset != 9 {
            return Err(Error::invalid(format!(
                "FLV: DataOffset must be 9 for version 1, got {data_offset}"
            )));
        }
        Ok(Self {
            version,
            has_audio: (flags & 0x04) != 0,
            has_video: (flags & 0x01) != 0,
            data_offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_minimal_header() {
        let bytes = [b'F', b'L', b'V', 0x01, 0x05, 0x00, 0x00, 0x00, 0x09];
        let h = FlvHeader::read(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(h.version, 1);
        assert!(h.has_audio);
        assert!(h.has_video);
        assert_eq!(h.data_offset, 9);
    }

    #[test]
    fn rejects_bad_signature() {
        let bytes = [b'F', b'L', b'X', 0x01, 0x05, 0x00, 0x00, 0x00, 0x09];
        assert!(matches!(
            FlvHeader::read(&mut Cursor::new(bytes)),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn rejects_bad_offset() {
        let bytes = [b'F', b'L', b'V', 0x01, 0x05, 0x00, 0x00, 0x00, 0x08];
        assert!(matches!(
            FlvHeader::read(&mut Cursor::new(bytes)),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn audio_only_flag() {
        let bytes = [b'F', b'L', b'V', 0x01, 0x04, 0x00, 0x00, 0x00, 0x09];
        let h = FlvHeader::read(&mut Cursor::new(bytes)).unwrap();
        assert!(h.has_audio);
        assert!(!h.has_video);
    }
}
