//! Seek integration tests.
//!
//! Fixtures (committed under `tests/fixtures/`):
//!
//! * `flv1_3s_keyframe_idx.flv` — Sorenson FLV1, 2 s, with the optional
//!   `onMetaData.keyframes` toc (`-flvflags add_keyframe_index`). Used
//!   to exercise the O(log n) bisect path.
//! * `flv1_3s_with_metadata.flv` — same source, but no `keyframes`
//!   table. Used to exercise the scan-forward fallback.
//! * `audio_only_2s.flv` — MP3 audio only. Used to exercise the
//!   audio-only branch of the scan path.
//!
//! Time base on every FLV stream is 1/1000, so `pts` and milliseconds
//! coincide.

use std::path::PathBuf;

use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};
use oxideav_flv::open_demuxer;

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    p
}

fn open(name: &str) -> Box<dyn Demuxer> {
    let bytes = std::fs::read(fixture(name)).expect("read fixture");
    let input: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    open_demuxer(input, &NullCodecResolver).expect("open flv")
}

/// Walk packets after a seek and return `(stream_index, pts, keyframe)`
/// for the first non-header packet emitted.
fn first_data_packet(dmx: &mut dyn Demuxer) -> Option<(u32, i64, bool)> {
    loop {
        match dmx.next_packet() {
            Ok(p) if p.flags.header => continue,
            Ok(p) => return Some((p.stream_index, p.pts.unwrap_or(0), p.flags.keyframe)),
            Err(_) => return None,
        }
    }
}

#[test]
fn seek_with_keyframes_table_lands_exact_at_toc_entry() {
    // The `keyframes` toc in the keyframe-idx fixture has two entries
    // at t=0 and t=1.0 s. Seeking to 1000 ms should land exactly at
    // the second entry.
    let mut dmx = open("flv1_3s_keyframe_idx.flv");
    // Find the video stream — fixture has only video.
    let video_idx = dmx
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == "flv1")
        .map(|s| s.index)
        .expect("flv1 video stream");
    let landed = dmx.seek_to(video_idx, 1000).expect("seek_to 1000");
    // Toc path returns the exact times[] entry rounded to ms; ffmpeg
    // writes times=[0.0, 1.0] for `-g 5 -r 5 -duration 2`.
    assert_eq!(
        landed, 1000,
        "expected to land exactly at toc entry t=1000 ms"
    );
    // The next non-header packet should be a video keyframe at >= 1000 ms.
    let (idx, pts, key) = first_data_packet(&mut *dmx).expect("packet after seek");
    assert_eq!(idx, video_idx);
    assert!(key, "first packet after toc seek must be a keyframe");
    assert!(
        pts >= 1000,
        "first packet pts {pts} should be >= seek target 1000"
    );
}

#[test]
fn seek_to_zero_resets_to_first_keyframe() {
    let mut dmx = open("flv1_3s_keyframe_idx.flv");
    let video_idx = dmx
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == "flv1")
        .map(|s| s.index)
        .expect("flv1 video stream");
    // Read a packet first, so we're definitely past the start.
    let _ = dmx.next_packet();
    let landed = dmx.seek_to(video_idx, 0).expect("seek_to 0");
    assert_eq!(landed, 0, "seek_to(0) must land at the first keyframe");
    let (_idx, pts, key) = first_data_packet(&mut *dmx).expect("packet after seek");
    assert!(key, "first packet after seek_to(0) must be a keyframe");
    assert_eq!(pts, 0, "first packet pts after seek_to(0) must be 0");
}

#[test]
fn seek_past_end_clamps_or_returns_eof_on_next() {
    let mut dmx = open("flv1_3s_keyframe_idx.flv");
    let video_idx = dmx
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == "flv1")
        .map(|s| s.index)
        .expect("flv1 video stream");
    // 60 seconds past the end of a 2 s clip.
    let _landed = dmx.seek_to(video_idx, 60_000).expect("seek_to past end");
    // Toc path lands at the last keyframe; scan path would land at
    // EOF. Either is acceptable — what matters is that the next
    // packet either comes from at-or-after the last keyframe or is
    // EOF, never a stale packet from earlier in the file.
    match dmx.next_packet() {
        Ok(p) => {
            // Must be from the second half of the file.
            assert!(
                p.pts.unwrap_or(0) >= 1000,
                "packet pts after seek-past-end should be >= last toc entry, got {:?}",
                p.pts
            );
        }
        Err(oxideav_core::Error::Eof) => {
            // Also acceptable — landed at end of file.
        }
        Err(e) => panic!("unexpected error after seek-past-end: {e:?}"),
    }
}

#[test]
fn seek_without_keyframes_table_scans_forward() {
    // The "with_metadata" fixture has duration / width / height in
    // onMetaData but does NOT have a `keyframes` table. seek_to must
    // fall through to the scan-forward path and still find a
    // keyframe.
    let mut dmx = open("flv1_3s_with_metadata.flv");
    let video_idx = dmx
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == "flv1")
        .map(|s| s.index)
        .expect("flv1 video stream");
    let landed = dmx.seek_to(video_idx, 500).expect("seek_to 500");
    // Scan path lands at the first keyframe >= target. With -g 5 -r 5
    // the keyframes are at 0, 1000, ... — so 500 lands at 1000 ms.
    assert!(
        landed >= 500,
        "scan-forward seek should land at-or-after target, got {landed}"
    );
    let (_idx, pts, key) = first_data_packet(&mut *dmx).expect("packet after seek");
    assert!(
        key,
        "first packet after scan-forward seek must be a keyframe"
    );
    assert_eq!(pts, landed, "first packet pts must match landed pts");
}

#[test]
fn seek_in_audio_only_flv_returns_unsupported_or_audio_position() {
    let mut dmx = open("audio_only_2s.flv");
    // Audio-only fixture — should have exactly one stream.
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mp3");
    let audio_idx = dmx.streams()[0].index;
    // Seek 500 ms in. The scan path will land at the first audio
    // packet ≥ 500 ms (every audio packet is independently decodable).
    let landed = dmx
        .seek_to(audio_idx, 500)
        .expect("audio seek should succeed via scan path");
    assert!(
        landed >= 500,
        "audio seek should land at-or-after target 500, got {landed}"
    );
    let (idx, pts, _key) = first_data_packet(&mut *dmx).expect("audio packet after seek");
    assert_eq!(idx, audio_idx);
    assert!(pts >= 500, "first audio packet pts {pts} should be >= 500");
}

#[test]
fn seek_invalid_stream_index_errors() {
    let mut dmx = open("audio_only_2s.flv");
    assert!(matches!(
        dmx.seek_to(99, 0),
        Err(oxideav_core::Error::InvalidData(_))
    ));
}
