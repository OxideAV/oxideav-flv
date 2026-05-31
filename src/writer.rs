//! FLV muxer: write an FLV byte stream from headers, script metadata, and
//! audio/video tags.
//!
//! The output layout is the exact inverse of what [`crate::FlvDemuxer`] reads:
//!
//! ```text
//! FlvHeader (9 bytes)
//! PreviousTagSize0 (4 bytes, always 0)
//! Tag1 (11-byte header + body)   PreviousTagSize1 (4 bytes)
//! Tag2 (11-byte header + body)   PreviousTagSize2 (4 bytes)
//! ...
//! ```
//!
//! [`FlvWriter`] tracks the running stream so each tag's `PreviousTagSize`
//! trailer is emitted automatically. The low-level building blocks live in
//! [`crate::header`], [`crate::tag`], and [`crate::amf`] for callers that want
//! to drive the byte stream directly.

use crate::amf::Amf0Value;
use crate::error::Result;
use crate::header;
use crate::tag::{self, AudioTag, ScriptTag, TagType, VideoTag};
use std::io::Write;

/// The AAC codec ID used in the FLV audio tag header (`SoundFormat` 10).
pub const AAC_CODEC_ID: u8 = 10;
/// The MP3 codec ID used in the FLV audio tag header (`SoundFormat` 2).
pub const MP3_CODEC_ID: u8 = 2;
/// `AACPacketType` value for a raw AAC access unit (1); 0 is the sequence
/// header carrying the `AudioSpecificConfig`.
pub const AAC_PACKET_TYPE_RAW: u8 = 1;

/// A small typed bag of common `onMetaData` properties.
///
/// Only the fields that are `Some` are emitted, so a default bag produces an
/// empty ECMA array. The spec does not mandate property ordering; entries are
/// written in declaration order. Each field maps to the AMF0 type the FLV
/// `onMetaData` property list specifies (numbers as AMF0 numbers, `stereo` as
/// an AMF0 boolean, `encoder` as an AMF0 string).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataBag {
    /// Total duration in seconds.
    pub duration: Option<f64>,
    /// Video frame width in pixels.
    pub width: Option<f64>,
    /// Video frame height in pixels.
    pub height: Option<f64>,
    /// Video frame rate in frames per second.
    pub framerate: Option<f64>,
    /// Video codec ID.
    pub videocodecid: Option<f64>,
    /// Video bit rate in kilobits per second.
    pub videodatarate: Option<f64>,
    /// Audio codec ID.
    pub audiocodecid: Option<f64>,
    /// Audio bit rate in kilobits per second.
    pub audiodatarate: Option<f64>,
    /// Audio sample rate in Hz.
    pub audiosamplerate: Option<f64>,
    /// Audio sample size in bits.
    pub audiosamplesize: Option<f64>,
    /// Whether the audio is stereo.
    pub stereo: Option<bool>,
    /// Total file size in bytes.
    pub filesize: Option<f64>,
    /// Name of the encoder that produced the file.
    pub encoder: Option<String>,
}

impl MetadataBag {
    /// Builds the AMF0 [`Amf0Value::EcmaArray`] carrying the set properties.
    pub fn to_ecma_array(&self) -> Amf0Value {
        let mut props: Vec<(String, Amf0Value)> = Vec::new();
        let mut num = |key: &str, v: Option<f64>| {
            if let Some(v) = v {
                props.push((key.to_string(), Amf0Value::Number(v)));
            }
        };
        num("duration", self.duration);
        num("width", self.width);
        num("height", self.height);
        num("framerate", self.framerate);
        num("videocodecid", self.videocodecid);
        num("videodatarate", self.videodatarate);
        num("audiocodecid", self.audiocodecid);
        num("audiodatarate", self.audiodatarate);
        num("audiosamplerate", self.audiosamplerate);
        num("audiosamplesize", self.audiosamplesize);
        if let Some(stereo) = self.stereo {
            props.push(("stereo".to_string(), Amf0Value::Boolean(stereo)));
        }
        num("filesize", self.filesize);
        if let Some(encoder) = &self.encoder {
            props.push(("encoder".to_string(), Amf0Value::String(encoder.clone())));
        }
        Amf0Value::EcmaArray(props)
    }
}

/// Streaming FLV muxer wrapping any [`Write`] sink.
pub struct FlvWriter<W: Write> {
    inner: W,
    /// Size in bytes of the most recently written tag (header + body), or 0
    /// before any tag has been emitted.
    last_tag_size: u32,
}

impl<W: Write> FlvWriter<W> {
    /// Creates a writer, immediately emitting the 9-byte FLV header (version 1)
    /// and the 4-byte `PreviousTagSize0` field.
    pub fn new(mut inner: W, has_audio: bool, has_video: bool) -> Result<Self> {
        header::write(&mut inner, has_audio, has_video)?;
        tag::write_previous_tag_size(&mut inner, 0)?;
        Ok(Self {
            inner,
            last_tag_size: 0,
        })
    }

    /// Writes a tag of `tag_type` with the given timestamp and pre-encoded
    /// body, followed by its `PreviousTagSize` trailer. Returns the tag size
    /// (header + body) that was recorded in the trailer.
    pub fn write_tag(&mut self, tag_type: TagType, timestamp: u32, body: &[u8]) -> Result<u32> {
        let size = tag::write_tag(&mut self.inner, tag_type, timestamp, 0, body)?;
        tag::write_previous_tag_size(&mut self.inner, size)?;
        self.last_tag_size = size;
        Ok(size)
    }

    /// Writes a script-data tag carrying `("onMetaData", <ECMA array>)`.
    pub fn write_on_metadata(&mut self, meta: &MetadataBag) -> Result<u32> {
        let script = ScriptTag {
            name: "onMetaData".to_string(),
            value: meta.to_ecma_array(),
        };
        self.write_script(0, &script)
    }

    /// Writes an arbitrary script-data tag (type 18) at `timestamp`.
    pub fn write_script(&mut self, timestamp: u32, script: &ScriptTag) -> Result<u32> {
        let body = script.to_body()?;
        self.write_tag(TagType::Script, timestamp, &body)
    }

    /// Writes an audio tag (type 8) at `timestamp` from a typed [`AudioTag`].
    pub fn write_audio(&mut self, timestamp: u32, audio: &AudioTag) -> Result<u32> {
        let body = audio.to_body();
        self.write_tag(TagType::Audio, timestamp, &body)
    }

    /// Writes a video tag (type 9) at `timestamp` from a typed [`VideoTag`].
    pub fn write_video(&mut self, timestamp: u32, video: &VideoTag) -> Result<u32> {
        let body = video.to_body();
        self.write_tag(TagType::Video, timestamp, &body)
    }

    /// Writes an MP3 audio tag (`SoundFormat` 2). `sound_rate`,
    /// `sound_size_16bit`, and `sound_type_stereo` map to the packed audio
    /// header bits; `frame` is the raw MP3 frame payload.
    pub fn write_mp3_tag(
        &mut self,
        timestamp: u32,
        sound_rate: u8,
        sound_size_16bit: bool,
        sound_type_stereo: bool,
        frame: &[u8],
    ) -> Result<u32> {
        let audio = AudioTag {
            codec_id: MP3_CODEC_ID,
            sound_rate,
            sound_size_16bit,
            sound_type_stereo,
            data: frame.to_vec(),
        };
        self.write_audio(timestamp, &audio)
    }

    /// Writes a raw-AU AAC audio tag (`SoundFormat` 10, `AACPacketType` 1).
    ///
    /// The audio-header rate/size/type bits are fixed at 3/16-bit/stereo as
    /// the FLV spec requires for AAC (the real configuration lives in the
    /// `AudioSpecificConfig` sequence header, written separately). The tag
    /// body is the `AACPacketType` byte followed by `raw_au`.
    pub fn write_aac_raw_tag(&mut self, timestamp: u32, raw_au: &[u8]) -> Result<u32> {
        let mut data = Vec::with_capacity(1 + raw_au.len());
        data.push(AAC_PACKET_TYPE_RAW);
        data.extend_from_slice(raw_au);
        let audio = AudioTag {
            codec_id: AAC_CODEC_ID,
            sound_rate: 3,
            sound_size_16bit: true,
            sound_type_stereo: true,
            data,
        };
        self.write_audio(timestamp, &audio)
    }

    /// Size in bytes of the most recently written tag, or 0 if none.
    pub fn last_tag_size(&self) -> u32 {
        self.last_tag_size
    }

    /// Flushes and returns the underlying writer.
    pub fn into_inner(mut self) -> Result<W> {
        self.inner.flush().map_err(crate::error::Error::Io)?;
        Ok(self.inner)
    }

    /// Borrows the underlying writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }
}
