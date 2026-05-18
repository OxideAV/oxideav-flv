//! FLV tag header + codec-id mappings.
//!
//! Tag layout (spec §E.4):
//!
//! ```text
//!   0   1    TagType   (0x08 audio, 0x09 video, 0x12 script)
//!   1   3    DataSize  (payload length, u24 BE)
//!   4   3    Timestamp (lower 24 bits, u24 BE, milliseconds)
//!   7   1    TimestampExtended (top 8 bits, prepend to u32)
//!   8   3    StreamID  (u24 BE — reserved, always 0)
//!  11   N    payload (DataSize bytes)
//! ```

use std::io::Read;

use oxideav_core::{Error, Result};

/// Tag header length in bytes (not including the payload or the
/// 4-byte `PreviousTagSize` prefix).
pub const TAG_HEADER_LEN: u32 = 11;

/// Tag-type byte values defined by the FLV spec. Other values are
/// reserved and the demuxer surfaces them as a decoder-free `Packet`
/// only if the caller asks us to (we currently skip unknown tag types).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagType {
    Audio,
    Video,
    ScriptData,
}

impl TagType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v & 0x1F {
            0x08 => Some(Self::Audio),
            0x09 => Some(Self::Video),
            0x12 => Some(Self::ScriptData),
            _ => None,
        }
    }
}

/// Parsed 11-byte tag header.
#[derive(Clone, Copy, Debug)]
pub struct TagHeader {
    pub tag_type_raw: u8,
    pub kind: Option<TagType>,
    pub data_size: u32,
    /// Full 32-bit timestamp (milliseconds). The upper 8 bits are the
    /// "TimestampExtended" byte.
    pub timestamp_ms: u32,
    pub stream_id: u32,
    /// True when bit 0x20 of the tag-type byte is set (the "Filter"
    /// flag — encryption hint). We surface it but otherwise treat the
    /// payload as cleartext; a filtered tag needs the consumer to
    /// resolve the filter descriptor themselves.
    pub filter: bool,
}

impl TagHeader {
    /// Read an 11-byte tag header from `r`. Returns `Error::Eof` if
    /// `r` is already at end-of-file on entry (distinct from a
    /// truncated / partial read, which surfaces as `Io(UnexpectedEof)`).
    pub fn read<R: Read + ?Sized>(r: &mut R) -> Result<Self> {
        let mut buf = [0u8; TAG_HEADER_LEN as usize];
        // Read first byte with a distinct EOF path so callers can
        // cleanly stop iterating at the end of the tag stream.
        let mut first = [0u8; 1];
        match r.read(&mut first) {
            Ok(0) => return Err(Error::Eof),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                r.read_exact(&mut first)?;
            }
            Err(e) => return Err(e.into()),
        }
        buf[0] = first[0];
        r.read_exact(&mut buf[1..])?;
        let tag_type_raw = buf[0];
        let filter = (tag_type_raw & 0x20) != 0;
        let kind = TagType::from_u8(tag_type_raw);
        let data_size = u24_be(&buf[1..4]);
        let ts_low = u24_be(&buf[4..7]);
        let ts_high = buf[7] as u32;
        let timestamp_ms = (ts_high << 24) | ts_low;
        let stream_id = u24_be(&buf[8..11]);
        Ok(Self {
            tag_type_raw,
            kind,
            data_size,
            timestamp_ms,
            stream_id,
            filter,
        })
    }
}

fn u24_be(b: &[u8]) -> u32 {
    debug_assert!(b.len() >= 3);
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32)
}

// ---- audio codec map -------------------------------------------------------

pub const AUDIO_CODEC_LPCM_NATIVE: u8 = 0;
pub const AUDIO_CODEC_ADPCM: u8 = 1;
pub const AUDIO_CODEC_MP3: u8 = 2;
pub const AUDIO_CODEC_LPCM_LE: u8 = 3;
pub const AUDIO_CODEC_NELLYMOSER_16K_MONO: u8 = 4;
pub const AUDIO_CODEC_NELLYMOSER_8K_MONO: u8 = 5;
pub const AUDIO_CODEC_NELLYMOSER: u8 = 6;
pub const AUDIO_CODEC_ALAW: u8 = 7;
pub const AUDIO_CODEC_MULAW: u8 = 8;
pub const AUDIO_CODEC_AAC: u8 = 10;
pub const AUDIO_CODEC_SPEEX: u8 = 11;
pub const AUDIO_CODEC_MP3_8K: u8 = 14;
pub const AUDIO_CODEC_DEVICE_SPECIFIC: u8 = 15;

/// Decoded audio tag header — the first byte of every audio payload.
#[derive(Clone, Copy, Debug)]
pub struct AudioTagHeader {
    pub codec_id: u8,
    /// 0=5.5 kHz, 1=11 kHz, 2=22 kHz, 3=44 kHz (spec rate index).
    pub sample_rate_idx: u8,
    /// False = 8-bit, True = 16-bit.
    pub is_16bit: bool,
    /// False = mono, True = stereo.
    pub is_stereo: bool,
}

impl AudioTagHeader {
    pub fn parse(b: u8) -> Self {
        Self {
            codec_id: b >> 4,
            sample_rate_idx: (b >> 2) & 0x03,
            is_16bit: (b & 0x02) != 0,
            is_stereo: (b & 0x01) != 0,
        }
    }

    pub fn sample_rate_hz(self) -> u32 {
        match self.codec_id {
            AUDIO_CODEC_MP3_8K => 8_000,
            AUDIO_CODEC_AAC => 44_100, // real rate comes from AudioSpecificConfig
            AUDIO_CODEC_NELLYMOSER_8K_MONO => 8_000,
            AUDIO_CODEC_NELLYMOSER_16K_MONO => 16_000,
            _ => match self.sample_rate_idx {
                0 => 5_512,
                1 => 11_025,
                2 => 22_050,
                _ => 44_100,
            },
        }
    }

    pub fn channels(self) -> u16 {
        if self.is_stereo {
            2
        } else {
            1
        }
    }
}

/// Short stable id string for the audio codec. Matches the strings
/// oxideav-codec uses elsewhere (`"mp3"`, `"aac"`, `"pcm_s16le"`, …).
/// Unknown ids fall back to `flv:audio:<N>`.
pub fn audio_codec_id_str(id: u8) -> String {
    match id {
        AUDIO_CODEC_LPCM_NATIVE => "pcm_s16le".into(),
        AUDIO_CODEC_ADPCM => "adpcm_swf".into(),
        AUDIO_CODEC_MP3 | AUDIO_CODEC_MP3_8K => "mp3".into(),
        AUDIO_CODEC_LPCM_LE => "pcm_s16le".into(),
        AUDIO_CODEC_NELLYMOSER_8K_MONO
        | AUDIO_CODEC_NELLYMOSER_16K_MONO
        | AUDIO_CODEC_NELLYMOSER => "nellymoser".into(),
        AUDIO_CODEC_ALAW => "pcm_alaw".into(),
        AUDIO_CODEC_MULAW => "pcm_mulaw".into(),
        AUDIO_CODEC_AAC => "aac".into(),
        AUDIO_CODEC_SPEEX => "speex".into(),
        AUDIO_CODEC_DEVICE_SPECIFIC => "flv:audio:device".into(),
        other => format!("flv:audio:{other}"),
    }
}

// ---- video codec map -------------------------------------------------------

pub const VIDEO_CODEC_JPEG: u8 = 1;
pub const VIDEO_CODEC_FLV1: u8 = 2;
pub const VIDEO_CODEC_SCREEN_V1: u8 = 3;
pub const VIDEO_CODEC_VP6F: u8 = 4;
pub const VIDEO_CODEC_VP6A: u8 = 5;
pub const VIDEO_CODEC_SCREEN_V2: u8 = 6;
pub const VIDEO_CODEC_H264: u8 = 7;

/// FrameType field (bits 7..4 of the first video byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    Key,
    Inter,
    DisposableInter,
    GeneratedKey,
    /// Per E.4.3.1 "video info/command frame" — body[1] is a UI8 command
    /// byte, not codec data. Commands defined: `0` = start of client-side
    /// seeking video sequence, `1` = end. The flag is surfaced so the
    /// demuxer can route the tag away from the decoder (no codec on
    /// earth wants to parse it as a frame).
    VideoInfo,
    Unknown(u8),
}

impl FrameType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Key,
            2 => Self::Inter,
            3 => Self::DisposableInter,
            4 => Self::GeneratedKey,
            5 => Self::VideoInfo,
            other => Self::Unknown(other),
        }
    }
}

/// Body-byte command values for [`FrameType::VideoInfo`] tags
/// (spec E.4.3.1 / E.4.3 VideoTagBody, IF FrameType == 5 branch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoInfoCommand {
    /// `0` — start of client-side-seeking video sequence.
    StartClientSeek,
    /// `1` — end of client-side-seeking video sequence.
    EndClientSeek,
    /// Any other UI8 — spec-unknown but preserved verbatim.
    Unknown(u8),
}

impl VideoInfoCommand {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::StartClientSeek,
            1 => Self::EndClientSeek,
            other => Self::Unknown(other),
        }
    }
}

/// Decoded video tag header — the first byte of every video payload.
#[derive(Clone, Copy, Debug)]
pub struct VideoTagHeader {
    pub frame_type: FrameType,
    pub codec_id: u8,
}

impl VideoTagHeader {
    pub fn parse(b: u8) -> Self {
        Self {
            frame_type: FrameType::from_u8(b >> 4),
            codec_id: b & 0x0F,
        }
    }

    pub fn is_keyframe(self) -> bool {
        matches!(self.frame_type, FrameType::Key | FrameType::GeneratedKey)
    }

    /// True when this is a video info / command frame (spec
    /// FrameType == 5). The body of such tags is **not** codec data —
    /// the demuxer surfaces them with `flags.discard` set so decoders
    /// skip them.
    pub fn is_video_info(self) -> bool {
        matches!(self.frame_type, FrameType::VideoInfo)
    }
}

// ---- encrypted-tag (Annex F) preamble parser -----------------------------

/// Parsed [`EncryptionTagHeader`] + [`FilterParams`] preamble of an
/// encrypted FLV tag (spec Annex F.3.1 / F.3.2).
///
/// Layout when the `Filter` bit of the [`TagHeader`] is set:
///
/// ```text
///   0   1   NumFilters (UI8, shall be 1)
///   1   N   FilterName (UTF-8, NUL-terminated STRING)
///   N+1 3   Length     (UI24 BE — bytes of FilterParams that follow)
///   N+4 L   FilterParams
/// ```
///
/// Two FilterName values are spec-defined:
/// `"Encryption"` (FLV encryption version 1) and `"SE"` (Selective
/// Encryption, version 2). Both wrap an `EncryptionFilterParams` or
/// `SelectiveEncryptionFilterParams` body whose first member is the
/// 16-byte AES-CBC initialisation vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedTagPreamble {
    pub filter_name: String,
    /// `true` for non-selective `"Encryption"`, `false` for `"SE"`.
    /// When `"SE"`, [`Self::is_encrypted`] tells whether *this* tag is
    /// actually ciphertext (vs in-the-clear with the SE wrapper).
    pub full_encryption: bool,
    /// Selective-encryption per-tag indicator. Always `true` for
    /// non-selective encryption; for `"SE"`, the UB[1] EncryptedAU bit.
    pub is_encrypted: bool,
    /// AES-CBC IV — present when [`Self::is_encrypted`] is true.
    pub iv: Option<[u8; 16]>,
    /// Number of bytes consumed from the tag body. The remaining
    /// `tag_data_size - bytes_consumed` bytes are the (possibly
    /// encrypted) ciphertext body.
    pub bytes_consumed: usize,
}

impl EncryptedTagPreamble {
    /// Parse the EncryptionTagHeader + FilterParams from the start of a
    /// filtered tag body. Returns `Err(Error::InvalidData)` on a
    /// malformed preamble (truncated string, NumFilters != 1, unknown
    /// FilterName, missing IV).
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.is_empty() {
            return Err(Error::invalid("FLV encrypted tag: empty body"));
        }
        let num_filters = body[0];
        if num_filters != 1 {
            return Err(Error::invalid(format!(
                "FLV encrypted tag: NumFilters {num_filters} != 1"
            )));
        }
        // STRING (UTF-8, NUL-terminated, per E.4.4 "STRING" type).
        let name_start = 1;
        let nul = body[name_start..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::invalid("FLV encrypted tag: unterminated FilterName"))?;
        let filter_name = std::str::from_utf8(&body[name_start..name_start + nul])
            .map_err(|_| Error::invalid("FLV encrypted tag: non-UTF-8 FilterName"))?
            .to_string();
        let name_end = name_start + nul + 1; // skip NUL
        if body.len() < name_end + 3 {
            return Err(Error::invalid("FLV encrypted tag: truncated Length"));
        }
        let params_len = ((body[name_end] as usize) << 16)
            | ((body[name_end + 1] as usize) << 8)
            | (body[name_end + 2] as usize);
        let params_start = name_end + 3;
        if body.len() < params_start + params_len {
            return Err(Error::invalid(
                "FLV encrypted tag: truncated FilterParams body",
            ));
        }
        let params = &body[params_start..params_start + params_len];

        let (full_encryption, is_encrypted, iv) = match filter_name.as_str() {
            "Encryption" => {
                // EncryptionFilterParams: UI8[16] IV.
                if params.len() < 16 {
                    return Err(Error::invalid(
                        "FLV Encryption FilterParams: IV must be 16 bytes",
                    ));
                }
                let mut iv = [0u8; 16];
                iv.copy_from_slice(&params[..16]);
                (true, true, Some(iv))
            }
            "SE" => {
                // SelectiveEncryptionFilterParams: UB[1] EncryptedAU +
                // UB[7] Reserved + IF EncryptedAU UI8[16] IV.
                if params.is_empty() {
                    return Err(Error::invalid("FLV SE FilterParams: empty"));
                }
                let encrypted_au = (params[0] >> 7) & 0x01 == 1;
                let iv = if encrypted_au {
                    if params.len() < 17 {
                        return Err(Error::invalid(
                            "FLV SE FilterParams: truncated IV (EncryptedAU=1 needs 1+16 bytes)",
                        ));
                    }
                    let mut iv = [0u8; 16];
                    iv.copy_from_slice(&params[1..17]);
                    Some(iv)
                } else {
                    None
                };
                (false, encrypted_au, iv)
            }
            other => {
                return Err(Error::invalid(format!(
                    "FLV encrypted tag: unknown FilterName {other:?}"
                )));
            }
        };
        Ok(Self {
            filter_name,
            full_encryption,
            is_encrypted,
            iv,
            bytes_consumed: params_start + params_len,
        })
    }
}

pub fn video_codec_id_str(id: u8) -> String {
    match id {
        VIDEO_CODEC_JPEG => "mjpeg".into(),
        VIDEO_CODEC_FLV1 => "flv1".into(),
        VIDEO_CODEC_SCREEN_V1 => "flashsv".into(),
        VIDEO_CODEC_VP6F => "vp6f".into(),
        VIDEO_CODEC_VP6A => "vp6a".into(),
        VIDEO_CODEC_SCREEN_V2 => "flashsv2".into(),
        VIDEO_CODEC_H264 => "h264".into(),
        other => format!("flv:video:{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn tag_header_roundtrip() {
        // audio tag, 7 bytes payload, ts 0x00000003, stream 0
        let bytes = [
            0x08, 0x00, 0x00, 0x07, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00,
        ];
        let h = TagHeader::read(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(h.kind, Some(TagType::Audio));
        assert_eq!(h.data_size, 7);
        assert_eq!(h.timestamp_ms, 3);
        assert_eq!(h.stream_id, 0);
        assert!(!h.filter);
    }

    #[test]
    fn tag_header_extended_timestamp() {
        // video tag, ts extended by 0x01 in high byte -> 0x01_00_00_FF.
        let bytes = [
            0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x01, 0x00, 0x00, 0x00,
        ];
        let h = TagHeader::read(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(h.kind, Some(TagType::Video));
        assert_eq!(h.timestamp_ms, 0x0100_00FF);
    }

    #[test]
    fn audio_header_decode() {
        // codec=10 (AAC), rate=3 (44kHz), 16-bit, stereo -> 0xAF
        let h = AudioTagHeader::parse(0xAF);
        assert_eq!(h.codec_id, 10);
        assert_eq!(h.sample_rate_idx, 3);
        assert!(h.is_16bit);
        assert!(h.is_stereo);
    }

    #[test]
    fn video_header_decode() {
        // frame_type=1 (key), codec_id=4 (vp6f) -> 0x14
        let h = VideoTagHeader::parse(0x14);
        assert_eq!(h.codec_id, VIDEO_CODEC_VP6F);
        assert!(h.is_keyframe());
    }

    #[test]
    fn eof_read_on_empty() {
        let mut c = Cursor::new(&[] as &[u8]);
        assert!(matches!(TagHeader::read(&mut c), Err(Error::Eof)));
    }

    #[test]
    fn encryption_preamble_v1_full_encryption() {
        // NumFilters=1, FilterName="Encryption\0", Length=16, IV bytes 0x10..0x1F.
        let mut body = vec![1u8];
        body.extend_from_slice(b"Encryption\0");
        body.push(0); // length hi
        body.push(0); // length mid
        body.push(16); // length lo (16 bytes IV)
        let iv: [u8; 16] = std::array::from_fn(|i| 0x10 + i as u8);
        body.extend_from_slice(&iv);
        body.extend_from_slice(b"cipherbytes"); // trailing ciphertext

        let p = EncryptedTagPreamble::parse(&body).unwrap();
        assert_eq!(p.filter_name, "Encryption");
        assert!(p.full_encryption);
        assert!(p.is_encrypted);
        assert_eq!(p.iv, Some(iv));
        // 1 + len("Encryption\0")=11 + 3 + 16 = 31
        assert_eq!(p.bytes_consumed, 31);
        assert_eq!(&body[p.bytes_consumed..], b"cipherbytes");
    }

    #[test]
    fn encryption_preamble_v2_selective_unencrypted() {
        // SE with EncryptedAU=0: just one byte, no IV.
        let mut body = vec![1u8];
        body.extend_from_slice(b"SE\0");
        body.push(0);
        body.push(0);
        body.push(1); // length = 1
        body.push(0x00); // EncryptedAU=0
        body.extend_from_slice(b"plaintext");

        let p = EncryptedTagPreamble::parse(&body).unwrap();
        assert_eq!(p.filter_name, "SE");
        assert!(!p.full_encryption);
        assert!(!p.is_encrypted);
        assert_eq!(p.iv, None);
        // 1 + 3 + 3 + 1 = 8
        assert_eq!(p.bytes_consumed, 8);
        assert_eq!(&body[p.bytes_consumed..], b"plaintext");
    }

    #[test]
    fn encryption_preamble_v2_selective_encrypted_with_iv() {
        let mut body = vec![1u8];
        body.extend_from_slice(b"SE\0");
        body.push(0);
        body.push(0);
        body.push(17); // length = 1 + 16
        body.push(0x80); // EncryptedAU=1
        let iv: [u8; 16] = [0xAA; 16];
        body.extend_from_slice(&iv);

        let p = EncryptedTagPreamble::parse(&body).unwrap();
        assert!(p.is_encrypted);
        assert_eq!(p.iv, Some(iv));
    }

    #[test]
    fn encryption_preamble_rejects_unknown_filter_name() {
        let mut body = vec![1u8];
        body.extend_from_slice(b"Bogus\0");
        body.push(0);
        body.push(0);
        body.push(0); // length 0
        assert!(matches!(
            EncryptedTagPreamble::parse(&body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn encryption_preamble_rejects_truncated_length() {
        // FilterName terminator present but no length follows.
        let body = b"\x01Encryption\0\x00";
        assert!(matches!(
            EncryptedTagPreamble::parse(body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn video_info_command_decode() {
        // FrameType=5, codec_id=2 (Sorenson) -> 0x52
        let h = VideoTagHeader::parse(0x52);
        assert!(h.is_video_info());
        assert!(!h.is_keyframe());
        assert_eq!(
            VideoInfoCommand::from_u8(0),
            VideoInfoCommand::StartClientSeek
        );
        assert_eq!(
            VideoInfoCommand::from_u8(1),
            VideoInfoCommand::EndClientSeek
        );
        assert_eq!(VideoInfoCommand::from_u8(7), VideoInfoCommand::Unknown(7));
    }
}
