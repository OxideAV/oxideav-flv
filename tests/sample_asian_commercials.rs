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
    let mut last_pts: i64 = 0;
    loop {
        match dmx.next_packet() {
            Ok(pkt) => {
                if pkt.stream_index == audio_idx {
                    audio_count += 1;
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
    eprintln!(
        "sample: audio={audio_count} video={video_count} keyframes={keyframe_count} \
         last_pts={last_pts}ms duration={}us",
        dur
    );
    assert!(audio_count > 0, "no audio packets");
    assert!(video_count > 0, "no video packets");
    assert!(keyframe_count > 0, "no video keyframes");
    // Spot-check the first keyframe: VP6 payloads have a 1-byte FLV
    // adjustment prefix, then the coded bitstream. Just make sure we
    // got some plausible amount of data.
    let kf = first_keyframe_bytes.expect("keyframe captured");
    assert!(
        kf.len() > 16,
        "first keyframe suspiciously short ({} bytes)",
        kf.len()
    );
}
