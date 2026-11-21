//! Cross-check against a black-box reader / writer.
//!
//! When `ffmpeg` and `ffprobe` are on the PATH (the tests return early
//! otherwise), each case:
//!
//! 1. has the black-box writer produce a short FLV for a codec pair;
//! 2. opens it here and compares stream facts and per-stream packet
//!    counts with what the black-box reader reports for the same file;
//! 3. remuxes the packets through [`oxideav_flv::FlvMuxer`] and asks the
//!    black-box reader about *our* file: same codecs, same sizes and
//!    rates, same packet counts, and a duration within a frame of the
//!    original;
//! 4. checks that the black-box reader can decode from a seek point in
//!    our file (with and without our keyframe table).
//!
//! A codec pair whose encoder this build of the tool lacks is skipped
//! at run time.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{
    Demuxer, Error, MediaType, Muxer, NullCodecResolver, Packet, ReadSeek, WriteSeek,
};
use oxideav_flv::{FlvMuxerOptions, open_demuxer_concrete, open_muxer_with_options};

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxideav-flv-crosscheck-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// What the black-box reader says about one stream.
#[derive(Debug, Default, Clone, PartialEq)]
struct Probe {
    codec: String,
    kind: String,
    width: u32,
    height: u32,
    sample_rate: u32,
    channels: u32,
    packets: u64,
}

fn probe(path: &Path) -> Option<(Vec<Probe>, f64)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-count_packets", "-show_entries"])
        .arg("stream=codec_type,codec_name,width,height,sample_rate,channels,nb_read_packets:format=duration")
        .args(["-of", "default=noprint_wrappers=0"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("ffprobe: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut streams = Vec::new();
    let mut cur: Option<Probe> = None;
    let mut duration = 0.0;
    for line in text.lines() {
        match line.trim() {
            "[STREAM]" => cur = Some(Probe::default()),
            "[/STREAM]" => streams.extend(cur.take()),
            l => {
                let Some((k, v)) = l.split_once('=') else {
                    continue;
                };
                if k == "duration" && cur.is_none() {
                    duration = v.parse().unwrap_or(0.0);
                }
                if let Some(p) = cur.as_mut() {
                    match k {
                        "codec_name" => p.codec = v.to_string(),
                        "codec_type" => p.kind = v.to_string(),
                        "width" => p.width = v.parse().unwrap_or(0),
                        "height" => p.height = v.parse().unwrap_or(0),
                        "sample_rate" => p.sample_rate = v.parse().unwrap_or(0),
                        "channels" => p.channels = v.parse().unwrap_or(0),
                        "nb_read_packets" => p.packets = v.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
        }
    }
    Some((streams, duration))
}

/// Our codec id → the name the black-box reader prints.
fn their_name(ours: &str) -> &str {
    match ours {
        "h265" => "hevc",
        "h266" => "vvc",
        other => other,
    }
}

struct Case {
    name: &'static str,
    video: Option<&'static [&'static str]>,
    audio: Option<&'static [&'static str]>,
}

fn generate(case: &Case, path: &Path) -> bool {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-y"]);
    if case.video.is_some() {
        cmd.args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25:duration=4",
        ]);
    }
    if case.audio.is_some() {
        cmd.args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4"]);
    }
    match case.video {
        Some(v) => cmd.args(v),
        None => cmd.arg("-vn"),
    };
    match case.audio {
        Some(a) => cmd.args(a),
        None => cmd.arg("-an"),
    };
    cmd.args(["-f", "flv"]).arg(path);
    match cmd.output() {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            eprintln!(
                "skip {}: writer refused: {}",
                case.name,
                String::from_utf8_lossy(&o.stderr)
                    .lines()
                    .next()
                    .unwrap_or("")
            );
            false
        }
        Err(_) => false,
    }
}

fn run_case(case: &Case) {
    let theirs = scratch(&format!("{}-theirs.flv", case.name));
    if !generate(case, &theirs) {
        return;
    }
    let Some((their_streams, their_duration)) = probe(&theirs) else {
        eprintln!("skip {}: reader could not open its own file", case.name);
        return;
    };

    // ---- theirs, opened here ------------------------------------------------
    let input: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&theirs).unwrap());
    let mut d = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
    assert_eq!(d.streams().len(), their_streams.len(), "{}", case.name);
    let streams = d.streams().to_vec();
    for s in &streams {
        let video = s.params.media_type == MediaType::Video;
        let t = their_streams
            .iter()
            .find(|t| t.kind == if video { "video" } else { "audio" })
            .unwrap_or_else(|| panic!("{}: reader lists no such stream", case.name));
        assert_eq!(
            their_name(s.params.codec_id.as_str()),
            t.codec,
            "{}",
            case.name
        );
        if video {
            assert_eq!(
                (s.params.width, s.params.height),
                (Some(t.width), Some(t.height)),
                "{}",
                case.name
            );
        } else {
            assert_eq!(s.params.sample_rate, Some(t.sample_rate), "{}", case.name);
            assert_eq!(
                s.params.channels.map(u32::from),
                Some(t.channels),
                "{}",
                case.name
            );
        }
    }
    let mut packets: Vec<Packet> = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => packets.push(p),
            Err(Error::Eof) => break,
            Err(e) => panic!("{}: {e}", case.name),
        }
    }
    for s in &streams {
        let video = s.params.media_type == MediaType::Video;
        let t = their_streams
            .iter()
            .find(|t| t.kind == if video { "video" } else { "audio" })
            .unwrap();
        let ours = packets
            .iter()
            .filter(|p| p.stream_index == s.index && !p.flags.header && !p.flags.discard)
            .count() as u64;
        assert_eq!(ours, t.packets, "{}: {} packet count", case.name, t.kind);
    }
    assert_eq!(d.stats().previous_tag_size_mismatches, 0, "{}", case.name);
    assert_eq!(d.stats().unknown_tags_skipped, 0, "{}", case.name);
    assert!(!d.stats().keyframe_table_rejected, "{}", case.name);
    let our_duration = d.duration_micros().map(|u| u as f64 / 1e6);
    if let Some(dur) = our_duration {
        assert!(
            (dur - their_duration).abs() < 0.1,
            "{}: {dur} vs {their_duration}",
            case.name
        );
    }

    // ---- ours, opened there -------------------------------------------------
    for (label, options) in [
        ("plain", FlvMuxerOptions::new()),
        ("indexed", FlvMuxerOptions::new().with_keyframe_index(64)),
    ] {
        let ours = scratch(&format!("{}-ours-{label}.flv", case.name));
        let out: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&ours).unwrap());
        let mut m = open_muxer_with_options(out, &streams, options).unwrap();
        m.set_metadata(d.metadata()).unwrap();
        m.write_header().unwrap();
        for p in &packets {
            m.write_packet(p).unwrap();
        }
        m.write_trailer().unwrap();
        drop(m);

        let (our_streams, duration) = probe(&ours)
            .unwrap_or_else(|| panic!("{}: reader rejected our {label} file", case.name));
        let mut a = their_streams.clone();
        let mut b = our_streams.clone();
        a.sort_by(|x, y| x.kind.cmp(&y.kind));
        b.sort_by(|x, y| x.kind.cmp(&y.kind));
        assert_eq!(
            a, b,
            "{}: stream facts / packet counts ({label})",
            case.name
        );
        assert!(
            (duration - their_duration).abs() < 0.08,
            "{}: duration {duration} vs {their_duration} ({label})",
            case.name
        );

        // Decode one second from a seek point in our file.
        let seek = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", "2.5", "-i"])
            .arg(&ours)
            .args(["-t", "1", "-f", "null", "-"])
            .output()
            .unwrap();
        assert!(
            seek.status.success() && seek.stderr.is_empty(),
            "{}: reader failed after seeking in our {label} file: {}",
            case.name,
            String::from_utf8_lossy(&seek.stderr)
        );

        // And our own reader agrees with itself on the remux.
        let input: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&ours).unwrap());
        let mut d2 = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
        let mut n = 0;
        while let Ok(p) = d2.next_packet() {
            if !p.flags.header && !p.flags.discard {
                n += 1;
            }
        }
        let want = packets
            .iter()
            .filter(|p| !p.flags.header && !p.flags.discard)
            .count();
        assert_eq!(n, want, "{}", case.name);
        // Nothing to complain about — a configuration record that the
        // source itself revised at the end of the stream aside.
        let mut stats = d2.stats().clone();
        assert_eq!(
            stats.config_updates,
            d.stats().config_updates,
            "{}",
            case.name
        );
        stats.config_updates = 0;
        assert_eq!(stats, Default::default(), "{}", case.name);
        if let Some(v) = d2
            .streams()
            .iter()
            .find(|s| s.params.media_type == MediaType::Video)
            .map(|s| s.index)
        {
            let landed = d2.seek_to(v, 2_500).unwrap();
            assert!(landed <= 2_500, "{}", case.name);
            let p = loop {
                let p = d2.next_packet().unwrap();
                if p.stream_index == v && !p.flags.header {
                    break p;
                }
            };
            assert!(p.flags.keyframe, "{}", case.name);
            assert_eq!(p.dts, Some(landed), "{}", case.name);
        }
        let _ = std::fs::remove_file(&ours);
    }
    let _ = std::fs::remove_file(&theirs);
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "flv1-mp3",
            video: Some(&["-c:v", "flv", "-g", "25"]),
            audio: Some(&["-c:a", "libmp3lame", "-ar", "44100", "-ac", "2"]),
        },
        Case {
            name: "h264-aac",
            video: Some(&[
                "-c:v", "libx264", "-g", "25", "-bf", "2", "-pix_fmt", "yuv420p",
            ]),
            audio: Some(&["-c:a", "aac", "-ar", "48000", "-ac", "2"]),
        },
        Case {
            name: "h264-only",
            video: Some(&[
                "-c:v", "libx264", "-g", "25", "-bf", "0", "-pix_fmt", "yuv420p",
            ]),
            audio: None,
        },
        Case {
            name: "mp3-only",
            video: None,
            audio: Some(&["-c:a", "libmp3lame", "-ar", "22050", "-ac", "1"]),
        },
        Case {
            name: "flv1-adpcm",
            video: Some(&["-c:v", "flv", "-g", "25"]),
            audio: Some(&["-c:a", "adpcm_swf", "-ar", "22050", "-ac", "1"]),
        },
        Case {
            name: "flv1-pcm",
            video: Some(&["-c:v", "flv", "-g", "25"]),
            audio: Some(&["-c:a", "pcm_s16le", "-ar", "44100", "-ac", "2"]),
        },
        Case {
            name: "flv1-nellymoser",
            video: Some(&["-c:v", "flv", "-g", "25"]),
            audio: Some(&["-c:a", "nellymoser", "-ar", "22050", "-ac", "1"]),
        },
        // Enhanced-RTMP FourCC carriage.
        Case {
            name: "hevc-opus",
            video: Some(&[
                "-c:v", "libx265", "-g", "25", "-bf", "2", "-pix_fmt", "yuv420p",
            ]),
            audio: Some(&["-c:a", "libopus", "-ar", "48000", "-ac", "2"]),
        },
        Case {
            name: "av1-flac",
            video: Some(&["-c:v", "libsvtav1", "-g", "25", "-pix_fmt", "yuv420p"]),
            audio: Some(&["-c:a", "flac", "-ar", "44100", "-ac", "2"]),
        },
        Case {
            name: "vp9-ac3",
            video: Some(&["-c:v", "libvpx-vp9", "-g", "25", "-pix_fmt", "yuv420p"]),
            audio: Some(&["-c:a", "ac3", "-ar", "48000", "-ac", "2"]),
        },
    ]
}

#[test]
fn black_box_reader_and_writer_agree_with_ours() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skip: ffmpeg / ffprobe not installed");
        return;
    }
    for case in cases() {
        run_case(&case);
    }
    let _ = std::fs::remove_dir_all(scratch("x").parent().unwrap());
}
