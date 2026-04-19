//! Integration test: walk the `asian-commercials-are-weird.flv` sample
//! end-to-end.
//!
//! This sample ships in the oxideav workspace (at the sibling path
//! `../samples/`) but may not be present on a fresh clone of this
//! crate — in that case the test is skipped with a message rather
//! than failing. Same pattern as oxideav-av1's `reference_clips.rs`.

use std::path::PathBuf;

use oxideav_container::ReadSeek;
use oxideav_core::{Error, NullCodecResolver};
use oxideav_flv::open_demuxer;

fn sample_path() -> Option<PathBuf> {
    // Allow an env override for crates.io CI and for anyone running
    // outside the oxideav monorepo. Otherwise search upwards from
    // `CARGO_MANIFEST_DIR`.
    if let Ok(p) = std::env::var("OXIDEAV_FLV_SAMPLE") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
        return None;
    }
    let mut here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..4 {
        let candidate = here.join("samples/asian-commercials-are-weird.flv");
        if candidate.exists() {
            return Some(candidate);
        }
        if !here.pop() {
            break;
        }
    }
    None
}

#[test]
fn walk_real_flv_sample() {
    let Some(path) = sample_path() else {
        eprintln!(
            "sample asian-commercials-are-weird.flv missing — skipping. \
             Set OXIDEAV_FLV_SAMPLE=<path> to override."
        );
        return;
    };
    let bytes = std::fs::read(&path).expect("read sample file");
    let input: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = open_demuxer(input, &NullCodecResolver).expect("open flv");

    // The sample is MP3 audio + VP6-flash video.
    let codecs: Vec<&str> = dmx
        .streams()
        .iter()
        .map(|s| s.params.codec_id.as_str())
        .collect();
    assert!(
        codecs.contains(&"mp3"),
        "expected mp3 audio, got {codecs:?}"
    );
    assert!(
        codecs.contains(&"vp6f"),
        "expected vp6f video, got {codecs:?}"
    );

    // Duration from the script tag — the sample is a short commercial.
    let dur = dmx.duration_micros().expect("duration_micros present");
    assert!(dur > 0 && dur < 120_000_000, "implausible duration {dur}");

    let audio_idx = dmx
        .streams()
        .iter()
        .position(|s| s.params.codec_id.as_str() == "mp3")
        .unwrap() as u32;
    let video_idx = dmx
        .streams()
        .iter()
        .position(|s| s.params.codec_id.as_str() == "vp6f")
        .unwrap() as u32;

    let mut audio_count = 0u32;
    let mut video_count = 0u32;
    let mut keyframe_count = 0u32;
    let mut first_keyframe_bytes: Option<Vec<u8>> = None;
    let mut first_audio_bytes: Option<Vec<u8>> = None;
    let mut last_pts: i64 = 0;
    loop {
        match dmx.next_packet() {
            Ok(pkt) => {
                if pkt.stream_index == audio_idx {
                    audio_count += 1;
                    if first_audio_bytes.is_none() {
                        first_audio_bytes = Some(pkt.data.clone());
                    }
                } else if pkt.stream_index == video_idx {
                    video_count += 1;
                    if pkt.flags.keyframe {
                        keyframe_count += 1;
                        if first_keyframe_bytes.is_none() {
                            first_keyframe_bytes = Some(pkt.data.clone());
                        }
                    }
                }
                if let Some(p) = pkt.pts {
                    if p > last_pts {
                        last_pts = p;
                    }
                }
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }

    // --- MP3 sanity: the first audio packet should start with the 11-bit
    // MPEG audio syncword (0xFFE).
    let mp3 = first_audio_bytes.expect("captured first audio");
    assert!(mp3.len() >= 4, "MP3 packet too short");
    let sync = ((mp3[0] as u16) << 3) | ((mp3[1] as u16) >> 5);
    assert_eq!(sync & 0x07FF, 0x07FF, "MP3 syncword missing");
    // --- VP6 sanity: the first keyframe body should parse as a keyframe
    // with a non-zero mb_width / mb_height. Manually decode enough of the
    // VP6 frame header to verify without pulling in oxideav-vp6 as a
    // cross-crate dev-dep (that would be a circular dep for the
    // standalone release of these crates).
    let vp6 = first_keyframe_bytes.expect("captured first keyframe");
    // vp6[0] is the FLV 1-byte VP6 adjustment byte; vp6[1..] is the
    // actual VP6 bitstream.
    assert!(vp6.len() > 8, "VP6 keyframe too short");
    let stream = &vp6[1..];
    let frame_mode = stream[0] >> 7;
    assert_eq!(frame_mode, 0, "first VP6 frame should be a keyframe");
    let separated_coeff = stream[0] & 0x01;
    let offset = if separated_coeff != 0 { 4 } else { 2 };
    assert!(stream.len() > offset + 3, "VP6 header too short");
    let mb_height = stream[offset] as u32;
    let mb_width = stream[offset + 1] as u32;
    assert!(mb_width > 0 && mb_width < 256);
    assert!(mb_height > 0 && mb_height < 256);
    eprintln!(
        "VP6 keyframe[0]: {}x{}px (mb_w={mb_width} mb_h={mb_height})",
        mb_width * 16,
        mb_height * 16
    );
    eprintln!(
        "sample: audio={audio_count} video={video_count} keyframes={keyframe_count} \
         last_pts={last_pts}ms duration={}us",
        dur
    );
    assert!(audio_count > 0, "no audio packets");
    assert!(video_count > 0, "no video packets");
    assert!(keyframe_count > 0, "no video keyframes");
    // Spot-check the first keyframe was a plausible size.
    assert!(
        vp6.len() > 16,
        "first keyframe suspiciously short ({} bytes)",
        vp6.len()
    );
}
