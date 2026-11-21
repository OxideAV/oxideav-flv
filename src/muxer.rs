//! FLV muxer.
//!
//! A streaming writer: every packet goes to the sink as one tag the
//! moment it arrives (through a 64 KiB coalescing buffer, so small tags
//! do not cost a system call each and large payloads are handed to the
//! sink without being copied). Nothing about the stream is held back —
//! memory use is independent of the file length.
//!
//! The file starts with an `onMetaData` tag (spec §E.5) whose
//! length-independent fields are fixed-width AMF numbers. When the
//! stream ends, [`Muxer::write_trailer`] seeks back and overwrites the
//! ones that could not be known up front — `duration`, `filesize`, the
//! measured data rates and, when asked for, the `keyframes` table (plus
//! the header's audio / video flags, should a stream have stayed
//! empty) — and returns to the end of the file. If the sink cannot seek, the trailer
//! leaves the placeholders (a duration of 0, i.e. "unknown / live") and
//! still succeeds: the file is a valid stream either way.
//!
//! Codec carriage (chosen from each stream's codec id):
//!
//! | codec id | tag form |
//! |---|---|
//! | `mp3`, `aac`, `pcm_s16le` / `pcm_u8`, `adpcm_swf`, `nellymoser`, `pcm_alaw`, `pcm_mulaw`, `speex` | legacy `SoundFormat` (§E.4.2.1) |
//! | `opus`, `flac`, `ac3`, `eac3` | Enhanced-RTMP FourCC audio |
//! | `flv1`, `flashsv`, `vp6f`, `vp6a`, `flashsv2`, `h264` | legacy `CodecID` (§E.4.3.1) |
//! | `h265`, `h266`, `av1`, `vp9`, `vp8` | Enhanced-RTMP FourCC video |
//!
//! Configuration records (`extradata`, or a `header`-flagged packet at
//! any point in the stream) become sequence-header tags; a record equal
//! to the one last written for that stream is not repeated.

use std::io::{SeekFrom, Write};

use oxideav_core::{
    Error, MediaType, Muxer, Packet, Result, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use crate::ex_audio::{FOURCC_AC3, FOURCC_EAC3, FOURCC_FLAC, FOURCC_OPUS};
use crate::ex_video::{
    EX_HEADER_FLAG, FOURCC_AV01, FOURCC_HVC1, FOURCC_VP08, FOURCC_VP09, FOURCC_VVC1,
};
use crate::tag::{
    AUDIO_CODEC_AAC, AUDIO_CODEC_ADPCM, AUDIO_CODEC_ALAW, AUDIO_CODEC_LPCM_LE, AUDIO_CODEC_MP3,
    AUDIO_CODEC_MP3_8K, AUDIO_CODEC_MULAW, AUDIO_CODEC_NELLYMOSER, AUDIO_CODEC_NELLYMOSER_8K_MONO,
    AUDIO_CODEC_NELLYMOSER_16K_MONO, AUDIO_CODEC_SPEEX, VIDEO_CODEC_FLV1, VIDEO_CODEC_H264,
    VIDEO_CODEC_SCREEN_V1, VIDEO_CODEC_SCREEN_V2, VIDEO_CODEC_VP6A, VIDEO_CODEC_VP6F,
};

/// Coalescing buffer size; payloads of at least half this go straight
/// to the sink.
const BUFFER: usize = 64 * 1024;

/// Upper bound on the reserved keyframe table (65 KiB of file).
const MAX_KEYFRAME_ENTRIES: usize = 3_600;

/// Largest tag payload (`DataSize` is a UI24).
const MAX_DATA_SIZE: usize = 0x00FF_FFFF;

/// Options for [`open_with_options`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FlvMuxerOptions {
    /// Number of `onMetaData.keyframes` entries to reserve room for
    /// (18 bytes each in the file header area). `0` (the default)
    /// writes no table.
    pub keyframe_index_capacity: usize,
    /// Rewrite `duration` / `filesize` / data rates / `keyframes` in
    /// place when the stream ends (default `true`). Turn off for a
    /// sink that must only ever be appended to.
    pub finalize: bool,
}

impl Default for FlvMuxerOptions {
    fn default() -> Self {
        Self {
            keyframe_index_capacity: 0,
            finalize: true,
        }
    }
}

impl FlvMuxerOptions {
    /// Defaults: no keyframe table, finalize on.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve room for `capacity` keyframe-table entries (capped at
    /// 3 600 — two hours at one key frame every two seconds). When the
    /// stream has more key frames than that, an
    /// evenly thinned subset is written; readers that walk forward from
    /// the nearest listed key frame (as [`crate::FlvDemuxer`] does)
    /// still seek exactly.
    pub fn with_keyframe_index(mut self, capacity: usize) -> Self {
        self.keyframe_index_capacity = capacity.min(MAX_KEYFRAME_ENTRIES);
        self
    }

    /// Enable or disable the in-place rewrite at the end of the stream.
    pub fn with_finalize(mut self, finalize: bool) -> Self {
        self.finalize = finalize;
        self
    }
}

/// How a stream's packets are framed.
#[derive(Clone, Copy, Debug)]
enum Carriage {
    /// Legacy audio: the complete AudioTagHeader byte; `aac` adds the
    /// `AACPacketType` byte.
    LegacyAudio { header: u8, aac: bool },
    /// Enhanced-RTMP audio by FourCC; `config` when the codec has a
    /// `SequenceStart` record.
    ExAudio { fourcc: u32, config: bool },
    /// Legacy video: the CodecID nibble; `avc` adds `AVCPacketType` +
    /// CompositionTime.
    LegacyVideo { codec_id: u8, avc: bool },
    /// Enhanced-RTMP video by FourCC; `cto` when `CodedFrames` carries a
    /// CompositionTimeOffset; `config` when the codec has a
    /// `SequenceStart` record.
    ExVideo {
        fourcc: u32,
        cto: bool,
        config: bool,
    },
}

impl Carriage {
    fn is_video(&self) -> bool {
        matches!(self, Self::LegacyVideo { .. } | Self::ExVideo { .. })
    }

    fn has_config(&self) -> bool {
        match *self {
            Self::LegacyAudio { aac, .. } => aac,
            Self::LegacyVideo { avc, .. } => avc,
            Self::ExAudio { config, .. } | Self::ExVideo { config, .. } => config,
        }
    }

    /// Value for `audiocodecid` / `videocodecid`: the legacy id, or the
    /// FourCC as a number for the Enhanced-RTMP codecs.
    fn metadata_codec_id(&self) -> f64 {
        match *self {
            Self::LegacyAudio { header, .. } => (header >> 4) as f64,
            Self::LegacyVideo { codec_id, .. } => codec_id as f64,
            Self::ExAudio { fourcc, .. } | Self::ExVideo { fourcc, .. } => fourcc as f64,
        }
    }
}

/// `SoundRate` index for a sampling rate, when the two-bit field can
/// say it (within 1 %).
fn sound_rate_index(rate: u32) -> Option<u8> {
    [5_512u32, 11_025, 22_050, 44_100]
        .iter()
        .position(|&r| (rate as i64 - r as i64).abs() * 100 <= r as i64)
        .map(|i| i as u8)
}

fn audio_carriage(s: &StreamInfo) -> Result<Carriage> {
    let p = &s.params;
    let id = p.codec_id.as_str();
    let rate = p.sample_rate.unwrap_or(44_100);
    let channels = p.channels.unwrap_or(2);
    let sixteen = !matches!(p.sample_format, Some(SampleFormat::U8 | SampleFormat::U8P));
    // SoundRate / SoundSize / SoundType bits for the formats that use
    // them as written. `exact` demands a representable rate.
    let bits = |format: u8, exact: bool, sixteen: bool| -> Result<Carriage> {
        if channels > 2 {
            return Err(Error::unsupported(format!(
                "FLV muxer: {id} with {channels} channels has no legacy tag form (mono / stereo only)"
            )));
        }
        let idx = match sound_rate_index(rate) {
            Some(i) => i,
            None if exact => {
                return Err(Error::unsupported(format!(
                    "FLV muxer: {id} at {rate} Hz cannot be signalled (SoundRate is 5.5 / 11 / 22 / 44 kHz)"
                )));
            }
            // Nearest slot at or above; the true rate goes into
            // `audiosamplerate`.
            None => match rate {
                0..=8_000 => 0,
                8_001..=12_000 => 1,
                12_001..=24_000 => 2,
                _ => 3,
            },
        };
        Ok(Carriage::LegacyAudio {
            header: (format << 4) | (idx << 2) | (u8::from(sixteen) << 1) | u8::from(channels == 2),
            aac: false,
        })
    };
    match id {
        "mp3" if rate == 8_000 => bits(AUDIO_CODEC_MP3_8K, false, true),
        "mp3" => bits(AUDIO_CODEC_MP3, false, true),
        // AAC: the bits are fixed at 44 kHz / 16-bit / stereo and
        // ignored by readers (§E.4.2.1).
        "aac" => Ok(Carriage::LegacyAudio {
            header: (AUDIO_CODEC_AAC << 4) | 0x0F,
            aac: true,
        }),
        "pcm_s16le" => bits(AUDIO_CODEC_LPCM_LE, true, sixteen),
        "pcm_u8" => bits(AUDIO_CODEC_LPCM_LE, true, false),
        "adpcm_swf" => bits(AUDIO_CODEC_ADPCM, true, true),
        "nellymoser" if rate == 8_000 && channels == 1 => Ok(Carriage::LegacyAudio {
            header: (AUDIO_CODEC_NELLYMOSER_8K_MONO << 4) | 0x02,
            aac: false,
        }),
        "nellymoser" if rate == 16_000 && channels == 1 => Ok(Carriage::LegacyAudio {
            header: (AUDIO_CODEC_NELLYMOSER_16K_MONO << 4) | 0x02,
            aac: false,
        }),
        "nellymoser" => bits(AUDIO_CODEC_NELLYMOSER, true, true),
        "pcm_alaw" => bits(AUDIO_CODEC_ALAW, false, true),
        "pcm_mulaw" => bits(AUDIO_CODEC_MULAW, false, true),
        // Speex: SoundRate 0, SoundSize 1, SoundType 0 (§E.4.2.1).
        "speex" => Ok(Carriage::LegacyAudio {
            header: (AUDIO_CODEC_SPEEX << 4) | 0x02,
            aac: false,
        }),
        "opus" => Ok(Carriage::ExAudio {
            fourcc: FOURCC_OPUS,
            config: true,
        }),
        "flac" => Ok(Carriage::ExAudio {
            fourcc: FOURCC_FLAC,
            config: true,
        }),
        "ac3" => Ok(Carriage::ExAudio {
            fourcc: FOURCC_AC3,
            config: false,
        }),
        "eac3" => Ok(Carriage::ExAudio {
            fourcc: FOURCC_EAC3,
            config: false,
        }),
        other => Err(Error::unsupported(format!(
            "FLV muxer: no tag form for audio codec {other}"
        ))),
    }
}

fn video_carriage(s: &StreamInfo) -> Result<Carriage> {
    let legacy = |codec_id: u8| {
        Ok(Carriage::LegacyVideo {
            codec_id,
            avc: codec_id == VIDEO_CODEC_H264,
        })
    };
    let ex = |fourcc: u32, cto: bool, config: bool| {
        Ok(Carriage::ExVideo {
            fourcc,
            cto,
            config,
        })
    };
    match s.params.codec_id.as_str() {
        "flv1" => legacy(VIDEO_CODEC_FLV1),
        "flashsv" => legacy(VIDEO_CODEC_SCREEN_V1),
        "vp6f" => legacy(VIDEO_CODEC_VP6F),
        "vp6a" => legacy(VIDEO_CODEC_VP6A),
        "flashsv2" => legacy(VIDEO_CODEC_SCREEN_V2),
        "h264" => legacy(VIDEO_CODEC_H264),
        "h265" => ex(FOURCC_HVC1, true, true),
        "h266" => ex(FOURCC_VVC1, true, true),
        "av1" => ex(FOURCC_AV01, false, true),
        "vp9" => ex(FOURCC_VP09, false, true),
        "vp8" => ex(FOURCC_VP08, false, false),
        other => Err(Error::unsupported(format!(
            "FLV muxer: no tag form for video codec {other}"
        ))),
    }
}

/// Per-stream writer state.
#[derive(Debug)]
struct Track {
    carriage: Carriage,
    /// Configuration record last written as a sequence header.
    config: Vec<u8>,
    /// Payload bytes written (for the measured data rate).
    bytes: u64,
    /// Tags written for this stream (frames and sequence headers).
    tags: u64,
    /// Declared bit rate, when the stream came with one.
    declared_bit_rate: Option<u64>,
}

/// Buffered sink that knows its position.
struct Sink {
    out: Box<dyn WriteSeek>,
    buf: Vec<u8>,
    /// Bytes handed to `put` so far (= file position of the next tag).
    pos: u64,
}

impl Sink {
    fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.out.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Append `head`, `payload` and `tail` as one unit.
    fn put(&mut self, head: &[u8], payload: &[u8], tail: &[u8]) -> Result<()> {
        let total = head.len() + payload.len() + tail.len();
        if self.buf.len() + total > BUFFER {
            self.flush()?;
        }
        if payload.len() >= BUFFER / 2 {
            self.buf.extend_from_slice(head);
            self.flush()?;
            self.out.write_all(payload)?;
            self.buf.extend_from_slice(tail);
        } else {
            self.buf.extend_from_slice(head);
            self.buf.extend_from_slice(payload);
            self.buf.extend_from_slice(tail);
        }
        self.pos += total as u64;
        Ok(())
    }
}

/// Where the patchable `onMetaData` numbers live in the file.
#[derive(Clone, Copy, Debug, Default)]
struct Patch {
    duration: u64,
    filesize: u64,
    video_rate: Option<u64>,
    audio_rate: Option<u64>,
    /// Offset and length of the reserved `keyframes` + filler region.
    keyframes: Option<(u64, usize)>,
}

/// FLV [`Muxer`]. Construct with [`open`], [`open_concrete`] or
/// [`open_with_options`].
pub struct FlvMuxer {
    sink: Sink,
    options: FlvMuxerOptions,
    /// Indexed by `Packet::stream_index`.
    tracks: Vec<Track>,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    patch: Patch,
    header_written: bool,
    trailer_written: bool,
    /// Subtracted from every timestamp so the first tag sits at or
    /// after 0 (set from the first packet when it starts below zero).
    origin_ms: Option<i64>,
    /// Latest end time seen, in milliseconds.
    end_ms: i64,
    /// Key frames seen: (tag offset, time in ms). Thinned to stay
    /// within twice the reserved capacity.
    keyframes: Vec<(u64, i64)>,
    /// Keep one key frame in `keyframe_stride` once thinning started.
    keyframe_stride: u64,
    keyframes_seen: u64,
}

impl std::fmt::Debug for FlvMuxer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlvMuxer")
            .field("streams", &self.streams.len())
            .field("position", &self.sink.pos)
            .field("end_ms", &self.end_ms)
            .field("options", &self.options)
            .finish()
    }
}

/// Registry factory: a muxer with default options.
pub fn open(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    Ok(Box::new(open_concrete(output, streams)?))
}

/// Like [`open`] but returns the concrete type.
pub fn open_concrete(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<FlvMuxer> {
    open_with_options(output, streams, FlvMuxerOptions::default())
}

/// Open a muxer with explicit [`FlvMuxerOptions`].
///
/// `streams` may hold at most one audio and one video stream (the FLV
/// stream model); packets are matched to them by position in the slice.
/// Fails with [`Error::Unsupported`] for a codec that has no FLV tag
/// form, or parameters the tag header cannot express.
pub fn open_with_options(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    options: FlvMuxerOptions,
) -> Result<FlvMuxer> {
    let mut tracks = Vec::with_capacity(streams.len());
    let (mut audio, mut video) = (0, 0);
    for s in streams {
        let carriage = match s.params.media_type {
            MediaType::Audio => {
                audio += 1;
                audio_carriage(s)?
            }
            MediaType::Video => {
                video += 1;
                video_carriage(s)?
            }
            other => {
                return Err(Error::unsupported(format!(
                    "FLV muxer: {other:?} streams are not carried"
                )));
            }
        };
        tracks.push(Track {
            carriage,
            config: Vec::new(),
            bytes: 0,
            tags: 0,
            declared_bit_rate: s.params.bit_rate,
        });
    }
    if tracks.is_empty() {
        return Err(Error::invalid("FLV muxer: no streams"));
    }
    if audio > 1 || video > 1 {
        return Err(Error::unsupported(
            "FLV muxer: at most one audio and one video stream",
        ));
    }
    Ok(FlvMuxer {
        sink: Sink {
            out: output,
            buf: Vec::with_capacity(BUFFER),
            pos: 0,
        },
        options,
        tracks,
        streams: streams.to_vec(),
        metadata: Vec::new(),
        patch: Patch::default(),
        header_written: false,
        trailer_written: false,
        origin_ms: None,
        end_ms: 0,
        keyframes: Vec::new(),
        keyframe_stride: 1,
        keyframes_seen: 0,
    })
}

// ---- AMF0 pieces of the onMetaData tag --------------------------------------

fn amf_key(out: &mut Vec<u8>, key: &str) {
    let k = &key.as_bytes()[..key.len().min(u16::MAX as usize)];
    out.extend_from_slice(&(k.len() as u16).to_be_bytes());
    out.extend_from_slice(k);
}

/// `key: Number`; returns the offset of the 8 value bytes within `out`.
fn amf_number(out: &mut Vec<u8>, key: &str, v: f64) -> usize {
    amf_key(out, key);
    out.push(0x00);
    out.extend_from_slice(&v.to_be_bytes());
    out.len() - 8
}

fn amf_bool(out: &mut Vec<u8>, key: &str, v: bool) {
    amf_key(out, key);
    out.extend_from_slice(&[0x01, u8::from(v)]);
}

fn amf_string(out: &mut Vec<u8>, key: &str, v: &str) {
    amf_key(out, key);
    if v.len() <= u16::MAX as usize {
        out.push(0x02);
        out.extend_from_slice(&(v.len() as u16).to_be_bytes());
    } else {
        out.push(0x0C);
        out.extend_from_slice(&(v.len() as u32).to_be_bytes());
    }
    out.extend_from_slice(v.as_bytes());
}

/// Bytes of the reserved region for `n` entries: the `keyframes`
/// object plus the end marker of the enclosing array.
fn keyframes_region_len(n: usize) -> usize {
    // key + Object marker, two (key + StrictArray marker + count + n
    // Numbers), object end, array end.
    (2 + 9 + 1) + (2 + 13 + 1 + 4 + 9 * n) + (2 + 5 + 1 + 4 + 9 * n) + 3 + 3
}

/// The reserved tail of the `onMetaData` body: the `keyframes` object
/// for `entries`, the end marker of the enclosing array, and zero bytes
/// up to exactly `region` bytes.
///
/// The unused part sits *after* the value's end marker, inside the
/// tag's `DataSize`: a reader that parses the name and the value never
/// looks at it. It is a multiple of nine zero bytes, i.e. a run of AMF
/// Numbers `0.0`, so a reader that keeps decoding values to the end of
/// the tag finds well-formed ones. (Padding with a String property was
/// tried first; a black-box reader gave up on `onMetaData` at a
/// kilobyte-long string, and at AMF type 12.)
fn keyframes_region(entries: &[(u64, i64)], region: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(region);
    amf_key(&mut out, "keyframes");
    out.push(0x03);
    amf_key(&mut out, "filepositions");
    out.push(0x0A);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (pos, _) in entries {
        out.push(0x00);
        out.extend_from_slice(&(*pos as f64).to_be_bytes());
    }
    amf_key(&mut out, "times");
    out.push(0x0A);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (_, ms) in entries {
        out.push(0x00);
        out.extend_from_slice(&(*ms as f64 / 1000.0).to_be_bytes());
    }
    // End of the `keyframes` object, end of the onMetaData array.
    out.extend_from_slice(&[0x00, 0x00, 0x09, 0x00, 0x00, 0x09]);
    debug_assert!(out.len() <= region && (region - out.len()).is_multiple_of(9));
    out.resize(region, 0);
    out
}

/// `onMetaData` keys the muxer writes itself (or that describe another
/// file's structure) and therefore does not take from `set_metadata`.
fn is_structural_key(key: &str) -> bool {
    matches!(
        key,
        "duration"
            | "width"
            | "height"
            | "framerate"
            | "videoframerate"
            | "videodatarate"
            | "videocodecid"
            | "audiodatarate"
            | "audiosamplerate"
            | "audiosamplesize"
            | "audiocodecid"
            | "stereo"
            | "filesize"
            | "keyframes"
            | "canSeekToEnd"
            | "encryption"
            | "xmp"
    ) || key.contains(['.', '['])
}

/// Milliseconds for a timestamp in `tb` ticks.
fn to_ms(v: i64, tb: TimeBase) -> i64 {
    let ms = TimeBase::new(1, 1000);
    if tb == ms { v } else { tb.rescale(v, ms) }
}

impl FlvMuxer {
    /// File offset the next tag will be written at.
    pub fn position(&self) -> u64 {
        self.sink.pos
    }

    /// Write one tag: 11-byte header, `prefix` (the codec framing
    /// bytes), `payload`, and the trailing `PreviousTagSize`.
    fn put_tag(&mut self, tag_type: u8, ts_ms: i64, prefix: &[u8], payload: &[u8]) -> Result<()> {
        let data_size = prefix.len() + payload.len();
        if data_size > MAX_DATA_SIZE {
            return Err(Error::invalid(format!(
                "FLV muxer: tag payload of {data_size} bytes exceeds the 24-bit DataSize"
            )));
        }
        // SI32 on the wire: the low 24 bits, then the extension byte.
        let ts = ts_ms as u32;
        let mut head = [0u8; 11 + 16];
        head[0] = tag_type;
        head[1..4].copy_from_slice(&(data_size as u32).to_be_bytes()[1..]);
        head[4..7].copy_from_slice(&ts.to_be_bytes()[1..]);
        head[7] = (ts >> 24) as u8;
        // StreamID stays 0.
        head[11..11 + prefix.len()].copy_from_slice(prefix);
        let tail = (11 + data_size as u32).to_be_bytes();
        self.sink.put(&head[..11 + prefix.len()], payload, &tail)
    }

    /// Write `config` as the sequence header of stream `idx`, unless it
    /// is the record already in force.
    fn put_config(&mut self, idx: usize, ts_ms: i64, config: &[u8]) -> Result<()> {
        if config.is_empty() || self.tracks[idx].config == config {
            return Ok(());
        }
        match self.tracks[idx].carriage {
            Carriage::LegacyAudio { header, aac: true } => {
                self.put_tag(0x08, ts_ms, &[header, 0x00], config)?;
            }
            Carriage::LegacyVideo {
                codec_id,
                avc: true,
            } => {
                // Key frame | CodecID, AVCPacketType 0, CompositionTime 0.
                self.put_tag(0x09, ts_ms, &[0x10 | codec_id, 0, 0, 0, 0], config)?;
            }
            Carriage::ExAudio {
                fourcc,
                config: true,
            } => {
                // SoundFormat 9 | AudioPacketType SequenceStart (0).
                let f = fourcc.to_be_bytes();
                self.put_tag(0x08, ts_ms, &[0x90, f[0], f[1], f[2], f[3]], config)?;
            }
            Carriage::ExVideo {
                fourcc,
                config: true,
                ..
            } => {
                // IsExHeader | key frame | PacketType SequenceStart (0).
                let f = fourcc.to_be_bytes();
                self.put_tag(
                    0x09,
                    ts_ms,
                    &[EX_HEADER_FLAG | 0x10, f[0], f[1], f[2], f[3]],
                    config,
                )?;
            }
            _ => return Ok(()),
        }
        let t = &mut self.tracks[idx];
        t.config.clear();
        t.config.extend_from_slice(config);
        t.tags += 1;
        Ok(())
    }

    fn note_keyframe(&mut self, pos: u64, ts_ms: i64) {
        let cap = self.options.keyframe_index_capacity;
        if cap == 0 {
            return;
        }
        let n = self.keyframes_seen;
        self.keyframes_seen += 1;
        if !n.is_multiple_of(self.keyframe_stride) {
            return;
        }
        if self.keyframes.len() >= 2 * cap {
            let mut keep = false;
            self.keyframes.retain(|_| {
                keep = !keep;
                keep
            });
            self.keyframe_stride *= 2;
            if !n.is_multiple_of(self.keyframe_stride) {
                return;
            }
        }
        self.keyframes.push((pos, ts_ms));
    }

    /// The (at most `capacity`) entries that go into the file.
    fn keyframes_for_file(&self) -> Vec<(u64, i64)> {
        let cap = self.options.keyframe_index_capacity;
        let n = self.keyframes.len();
        if n <= cap {
            return self.keyframes.clone();
        }
        let step = n.div_ceil(cap);
        self.keyframes.iter().copied().step_by(step).collect()
    }
}

impl Muxer for FlvMuxer {
    fn format_name(&self) -> &str {
        "flv"
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Err(Error::invalid("FLV muxer: header already written"));
        }
        let audio = self.tracks.iter().position(|t| !t.carriage.is_video());
        let video = self.tracks.iter().position(|t| t.carriage.is_video());

        // --- onMetaData body ---
        let mut body = Vec::with_capacity(512);
        body.push(0x02);
        body.extend_from_slice(&10u16.to_be_bytes());
        body.extend_from_slice(b"onMetaData");
        body.push(0x08);
        let count_at = body.len();
        body.extend_from_slice(&0u32.to_be_bytes());
        let mut count = 0u32;
        let mut patch = Patch {
            duration: amf_number(&mut body, "duration", 0.0) as u64,
            ..Patch::default()
        };
        count += 1;
        if let Some(i) = video {
            let p = &self.streams[i].params;
            if let Some(w) = p.width {
                amf_number(&mut body, "width", w as f64);
                count += 1;
            }
            if let Some(h) = p.height {
                amf_number(&mut body, "height", h as f64);
                count += 1;
            }
            if let Some(fr) = p.frame_rate
                && fr.den != 0
            {
                amf_number(&mut body, "framerate", fr.num as f64 / fr.den as f64);
                count += 1;
            }
            let at = amf_number(
                &mut body,
                "videodatarate",
                p.bit_rate.map_or(0.0, |b| b as f64 / 1000.0),
            );
            patch.video_rate = Some(at as u64);
            amf_number(
                &mut body,
                "videocodecid",
                self.tracks[i].carriage.metadata_codec_id(),
            );
            count += 2;
        }
        if let Some(i) = audio {
            let p = &self.streams[i].params;
            let at = amf_number(
                &mut body,
                "audiodatarate",
                p.bit_rate.map_or(0.0, |b| b as f64 / 1000.0),
            );
            patch.audio_rate = Some(at as u64);
            count += 1;
            if let Some(sr) = p.sample_rate {
                amf_number(&mut body, "audiosamplerate", sr as f64);
                count += 1;
            }
            let bits = match p.sample_format {
                Some(SampleFormat::U8 | SampleFormat::U8P) => Some(8.0),
                Some(SampleFormat::S16 | SampleFormat::S16P) => Some(16.0),
                _ => None,
            };
            if let Some(b) = bits {
                amf_number(&mut body, "audiosamplesize", b);
                count += 1;
            }
            if let Some(ch) = p.channels {
                amf_bool(&mut body, "stereo", ch == 2);
                count += 1;
            }
            amf_number(
                &mut body,
                "audiocodecid",
                self.tracks[i].carriage.metadata_codec_id(),
            );
            count += 1;
        }
        for (k, v) in &self.metadata {
            amf_string(&mut body, k, v);
            count += 1;
        }
        patch.filesize = amf_number(&mut body, "filesize", 0.0) as u64;
        count += 1;
        let cap = self.options.keyframe_index_capacity;
        if cap > 0 && video.is_some() {
            // The region includes the array's end marker.
            let region = keyframes_region_len(cap);
            patch.keyframes = Some((body.len() as u64, region));
            body.extend_from_slice(&keyframes_region(&[], region));
            count += 1;
        } else {
            body.extend_from_slice(&[0x00, 0x00, 0x09]);
        }
        body[count_at..count_at + 4].copy_from_slice(&count.to_be_bytes());

        // --- file header, PreviousTagSize0, the script tag ---
        let mut lead = Vec::with_capacity(13);
        crate::header::write(&mut lead, audio.is_some(), video.is_some())?;
        lead.extend_from_slice(&0u32.to_be_bytes());
        self.sink.put(&lead, &[], &[])?;
        // Body offsets → file offsets (the tag header is 11 bytes).
        let base = self.sink.pos + 11;
        patch.duration += base;
        patch.filesize += base;
        patch.video_rate = patch.video_rate.map(|o| o + base);
        patch.audio_rate = patch.audio_rate.map(|o| o + base);
        patch.keyframes = patch.keyframes.map(|(o, n)| (o + base, n));
        self.patch = patch;
        self.put_tag(0x12, 0, &[], &body)?;

        // --- configuration records that came with the streams ---
        for i in 0..self.tracks.len() {
            if self.tracks[i].carriage.has_config() {
                let cfg = std::mem::take(&mut self.streams[i].params.extradata);
                self.put_config(i, 0, &cfg)?;
            }
        }
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written || self.trailer_written {
            return Err(Error::invalid(
                "FLV muxer: write_packet outside write_header..write_trailer",
            ));
        }
        let idx = packet.stream_index as usize;
        let Some(track) = self.tracks.get(idx) else {
            return Err(Error::invalid(format!(
                "FLV muxer: packet for unknown stream {idx}"
            )));
        };
        let carriage = track.carriage;
        let dts = packet.dts.or(packet.pts).unwrap_or(0);
        let pts = packet.pts.unwrap_or(dts);
        let dts_ms = to_ms(dts, packet.time_base);
        let pts_ms = to_ms(pts, packet.time_base);
        let origin = *self.origin_ms.get_or_insert(dts_ms.min(0));
        let ts = (dts_ms - origin).max(0);
        let cto = (pts_ms - dts_ms).clamp(-(1 << 23), (1 << 23) - 1) as i32;

        if packet.flags.header {
            // A configuration record (possibly a changed one). Other
            // header-flagged packets (commands, in-band metadata) have
            // no generic tag form and are not written.
            if carriage.has_config() && !packet.flags.discard {
                self.put_config(idx, ts, &packet.data)?;
            }
            return Ok(());
        }
        if packet.flags.discard {
            return Ok(());
        }

        let pos = self.sink.pos;
        let key = packet.flags.keyframe;
        let data = packet.data.as_slice();
        // FrameType: 1 = key frame, 2 = inter frame (§E.4.3.1).
        let frame = if key { 0x10 } else { 0x20 };
        let c = (cto as u32).to_be_bytes();
        match carriage {
            Carriage::LegacyAudio { header, aac } => {
                if aac {
                    self.put_tag(0x08, ts, &[header, 0x01], data)?;
                } else {
                    self.put_tag(0x08, ts, &[header], data)?;
                }
            }
            Carriage::ExAudio { fourcc, .. } => {
                // SoundFormat 9 | AudioPacketType CodedFrames (1).
                let f = fourcc.to_be_bytes();
                self.put_tag(0x08, ts, &[0x91, f[0], f[1], f[2], f[3]], data)?;
            }
            Carriage::LegacyVideo { codec_id, avc } => {
                if avc {
                    self.put_tag(0x09, ts, &[frame | codec_id, 0x01, c[1], c[2], c[3]], data)?;
                } else {
                    self.put_tag(0x09, ts, &[frame | codec_id], data)?;
                }
            }
            Carriage::ExVideo {
                fourcc,
                cto: has_cto,
                ..
            } => {
                let f = fourcc.to_be_bytes();
                let lead = EX_HEADER_FLAG | frame;
                if has_cto && cto != 0 {
                    // PacketType CodedFrames (1) + SI24 offset.
                    self.put_tag(
                        0x09,
                        ts,
                        &[lead | 1, f[0], f[1], f[2], f[3], c[1], c[2], c[3]],
                        data,
                    )?;
                } else if has_cto {
                    // PacketType CodedFramesX (3): offset implied 0.
                    self.put_tag(0x09, ts, &[lead | 3, f[0], f[1], f[2], f[3]], data)?;
                } else {
                    self.put_tag(0x09, ts, &[lead | 1, f[0], f[1], f[2], f[3]], data)?;
                }
            }
        }
        if carriage.is_video() && key {
            // The table stores the offset of the tag itself.
            self.note_keyframe(pos, ts);
        }
        self.tracks[idx].bytes += data.len() as u64;
        self.tracks[idx].tags += 1;
        let dur_ms = packet.duration.map_or(0, |d| to_ms(d, packet.time_base));
        self.end_ms = self
            .end_ms
            .max(ts + (pts_ms - dts_ms).max(0) + dur_ms.max(0));
        Ok(())
    }

    /// Flush, then (unless disabled or the sink cannot seek) rewrite
    /// the `onMetaData` placeholders: `duration`, `filesize`, data
    /// rates that were not declared, and the `keyframes` table.
    fn write_trailer(&mut self) -> Result<()> {
        if !self.header_written {
            return Err(Error::invalid("FLV muxer: trailer before header"));
        }
        if self.trailer_written {
            return Ok(());
        }
        self.trailer_written = true;
        self.sink.flush()?;
        if self.options.finalize {
            let end = self.sink.pos;
            let seconds = self.end_ms as f64 / 1000.0;
            let mut patches: Vec<(u64, Vec<u8>)> = vec![
                (self.patch.duration, seconds.to_be_bytes().to_vec()),
                (self.patch.filesize, (end as f64).to_be_bytes().to_vec()),
            ];
            for t in &self.tracks {
                let at = if t.carriage.is_video() {
                    self.patch.video_rate
                } else {
                    self.patch.audio_rate
                };
                if let Some(at) = at
                    && t.declared_bit_rate.is_none()
                    && seconds > 0.0
                {
                    let kbps = t.bytes as f64 * 8.0 / seconds / 1000.0;
                    patches.push((at, kbps.to_be_bytes().to_vec()));
                }
            }
            // The header's TypeFlags promised every stream handed to
            // `open`; correct the promise if one never produced a tag.
            let wrote = |video: bool| {
                self.tracks
                    .iter()
                    .any(|t| t.carriage.is_video() == video && t.tags > 0)
            };
            if self.tracks.iter().any(|t| t.tags == 0) {
                let flags = (u8::from(wrote(false)) << 2) | u8::from(wrote(true));
                patches.push((4, vec![flags]));
            }
            if let Some((at, region)) = self.patch.keyframes {
                patches.push((at, keyframes_region(&self.keyframes_for_file(), region)));
            }
            // A sink that cannot seek keeps its placeholders.
            let out = &mut self.sink.out;
            let patched = (|| -> std::io::Result<()> {
                for (at, bytes) in &patches {
                    out.seek(SeekFrom::Start(*at))?;
                    out.write_all(bytes)?;
                }
                out.seek(SeekFrom::Start(end))?;
                Ok(())
            })();
            let _ = patched;
        }
        self.sink.out.flush()?;
        Ok(())
    }

    /// Stores every pair whose key is a plain name (no `.` / `[`) and
    /// is not one of the fields the muxer derives itself (`duration`,
    /// `width`, `height`, `framerate`, `videodatarate`, `videocodecid`,
    /// `audiodatarate`, `audiosamplerate`, `audiosamplesize`,
    /// `audiocodecid`, `stereo`, `filesize`, `keyframes`, …) as an AMF
    /// string property of `onMetaData`.
    fn set_metadata(&mut self, metadata: &[(String, String)]) -> Result<()> {
        if self.header_written {
            return Err(Error::invalid("FLV muxer: set_metadata after write_header"));
        }
        self.metadata = metadata
            .iter()
            .filter(|(k, _)| self.carries_metadata_key(k))
            .cloned()
            .collect();
        Ok(())
    }

    fn carries_metadata_key(&self, key: &str) -> bool {
        !key.is_empty() && key.len() <= u16::MAX as usize && !is_structural_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_region_is_exact_for_any_fill() {
        let cap = MAX_KEYFRAME_ENTRIES;
        let region = keyframes_region_len(cap);
        for n in [0, 1, 7, cap - 1, cap] {
            let e: Vec<(u64, i64)> = (0..n as u64).map(|i| (i * 100, i as i64 * 2000)).collect();
            let r = keyframes_region(&e, region);
            assert_eq!(r.len(), region);
            assert_eq!(keyframes_region_len(n), region - 18 * (cap - n));
            assert!(r[keyframes_region_len(n)..].iter().all(|b| *b == 0));
        }
    }

    #[test]
    fn sound_rate_index_accepts_nominal_rates() {
        assert_eq!(sound_rate_index(44_100), Some(3));
        assert_eq!(sound_rate_index(22_050), Some(2));
        assert_eq!(sound_rate_index(11_025), Some(1));
        assert_eq!(sound_rate_index(5_512), Some(0));
        assert_eq!(sound_rate_index(5_500), Some(0));
        assert_eq!(sound_rate_index(48_000), None);
    }
}
