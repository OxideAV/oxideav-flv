//! Muxer → demuxer bit-exact round-trip.
//!
//! The first FLV muxer slice writes a file header, an `onMetaData`
//! script tag, and a run of MP3 audio tags. This test hands the muxed
//! bytes straight to the existing [`FlvDemuxer`] and asserts that the
//! container survives the round-trip:
//!
//! * the file header flags parse back (audio present, video absent);
//! * the leading `PreviousTagSize0` is `0`;
//! * the `onMetaData` scalar properties read back identically through
//!   `metadata()` / `duration_micros()`;
//! * every MP3 frame body survives byte-for-byte, in order, with the
//!   timestamp it was written with.

use std::io::Cursor;

use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};
use oxideav_flv::{header, open_demuxer, script, script::MetadataBag, tag, FlvHeader};

/// Three distinct synthetic MP3 frame payloads. The demuxer treats an
/// MP3 tag body after the one-byte AudioTagHeader as opaque `SoundData`,
/// so the exact bytes are irrelevant to parsing — distinct patterns let
/// us assert each frame survives in the right order without aliasing.
fn mp3_frames() -> Vec<Vec<u8>> {
    vec![
        vec![0xFF, 0xFB, 0x90, 0x00, 0x11, 0x22, 0x33],
        vec![0xFF, 0xFB, 0x90, 0x44, 0x55, 0x66, 0x77, 0x88],
        vec![0xFF, 0xFB, 0x91, 0x99, 0xAA, 0xBB],
    ]
}

/// One MP3 frame at 44.1 kHz is 1152 samples ≈ 26.122 ms; integer-ms
/// stamps of 0/26/52 are what a real muxer would emit.
const FRAME_TS_MS: [u32; 3] = [0, 26, 52];

fn build_flv() -> (Vec<u8>, Vec<Vec<u8>>) {
    let frames = mp3_frames();
    let mut buf = Vec::new();
    // Audio-only file.
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let bag = MetadataBag::new()
        .number("duration", 2.0)
        .number("audiosamplerate", 44_100.0)
        .number("audiodatarate", 128.0)
        .boolean("stereo", true)
        .string("encoder", "oxideav-flv muxer");
    script::write_on_metadata(&mut buf, &bag).unwrap();

    for (frame, &ts) in frames.iter().zip(FRAME_TS_MS.iter()) {
        // SoundRate idx 3 = 44 kHz, 16-bit, stereo.
        tag::write_mp3_tag(&mut buf, ts, 3, true, true, frame).unwrap();
    }
    (buf, frames)
}

fn open(bytes: Vec<u8>) -> Box<dyn Demuxer> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    open_demuxer(input, &NullCodecResolver).expect("open muxed flv")
}

#[test]
fn file_header_flags_survive_round_trip() {
    let (bytes, _) = build_flv();
    // Parse the 9-byte header back with the demuxer-side reader.
    let h = FlvHeader::read(&mut Cursor::new(&bytes[..9])).unwrap();
    assert_eq!(h.version, 1);
    assert!(h.has_audio, "audio flag must survive");
    assert!(!h.has_video, "video flag must survive");
    assert_eq!(h.data_offset, 9);
    // Leading PreviousTagSize0 is always zero (spec E.3).
    assert_eq!(&bytes[9..13], &[0, 0, 0, 0]);
}

#[test]
fn on_metadata_keys_read_back_identically() {
    let (bytes, _) = build_flv();
    let dmx = open(bytes);
    let md = dmx.metadata();

    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    // Integral-valued numbers serialise without a trailing ".0".
    assert_eq!(lookup("duration"), Some("2"));
    assert_eq!(lookup("audiosamplerate"), Some("44100"));
    assert_eq!(lookup("audiodatarate"), Some("128"));
    assert_eq!(lookup("stereo"), Some("true"));
    assert_eq!(lookup("encoder"), Some("oxideav-flv muxer"));

    // duration (2.0 s) flows into the numeric accessor as microseconds.
    assert_eq!(dmx.duration_micros(), Some(2_000_000));
}

#[test]
fn single_audio_stream_is_mp3() {
    let (bytes, _) = build_flv();
    let dmx = open(bytes);
    assert_eq!(dmx.streams().len(), 1, "audio-only file → one stream");
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mp3");
}

#[test]
fn audio_packet_bodies_survive_byte_for_byte() {
    let (bytes, frames) = build_flv();
    let mut dmx = open(bytes);

    let mut got: Vec<(i64, Vec<u8>)> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) if p.flags.header => continue, // MP3 has no config packet, but be safe.
            Ok(p) => got.push((p.pts.unwrap_or(-1), p.data.clone())),
            Err(_) => break,
        }
    }

    assert_eq!(got.len(), frames.len(), "every MP3 tag must surface");
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(
            got[i].0, FRAME_TS_MS[i] as i64,
            "frame {i} timestamp must round-trip"
        );
        assert_eq!(
            &got[i].1, frame,
            "frame {i} body must survive byte-for-byte"
        );
    }
}
