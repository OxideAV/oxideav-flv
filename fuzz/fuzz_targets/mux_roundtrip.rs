#![no_main]

//! Muxer → demuxer differential.
//!
//! The fuzz input picks an audio codec, a video codec, the muxer
//! options and a run of packets (stream, time step, composition
//! offset, key flag, payload, configuration changes). The packets go
//! through [`oxideav_flv::FlvMuxer`] into memory and come back through
//! [`oxideav_flv::FlvDemuxer`]; every data packet must return with the
//! same stream, timestamps, key flag and payload, the stream facts must
//! match what was declared, the demuxer must find nothing to complain
//! about, and a seek must land on a key frame at or before its target.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, NullCodecResolver, Packet, ReadSeek,
    StreamInfo, TimeBase,
};
use oxideav_flv::{FlvMuxerOptions, open_demuxer_concrete, open_muxer_with_options};

#[derive(Clone, Default)]
struct Sink {
    data: Arc<Mutex<Vec<u8>>>,
    pos: usize,
    seekable: bool,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut d = self.data.lock().unwrap();
        let end = self.pos + buf.len();
        if d.len() < end {
            d.resize(end, 0);
        }
        d[self.pos..end].copy_from_slice(buf);
        self.pos = end;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Sink {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        if !self.seekable {
            return Err(std::io::Error::other("not seekable"));
        }
        if let SeekFrom::Start(p) = pos {
            self.pos = p as usize;
        }
        Ok(self.pos as u64)
    }
}

/// (codec id, has a configuration record, composition offsets allowed)
const VIDEO: &[(&str, bool, bool)] = &[
    ("flv1", false, false),
    ("vp6f", false, false),
    ("flashsv", false, false),
    ("h264", true, true),
    ("h265", true, true),
    ("h266", true, true),
    ("av1", true, false),
    ("vp9", true, false),
    ("vp8", false, false),
];

/// (codec id, rate, channels, has a configuration record)
const AUDIO: &[(&str, u32, u16, bool)] = &[
    ("mp3", 44_100, 2, false),
    ("mp3", 48_000, 1, false),
    ("aac", 48_000, 2, true),
    ("pcm_s16le", 22_050, 1, false),
    ("adpcm_swf", 11_025, 2, false),
    ("nellymoser", 16_000, 1, false),
    ("speex", 16_000, 1, false),
    ("pcm_mulaw", 8_000, 1, false),
    ("opus", 48_000, 2, true),
    ("flac", 44_100, 2, true),
    ("ac3", 48_000, 2, false),
];

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let ms = TimeBase::new(1, 1000);
    let (vsel, asel, opt) = (data[0], data[1], data[2]);
    let use_video = vsel & 0x80 == 0 || asel & 0x80 != 0;
    let use_audio = asel & 0x80 == 0;
    let v = VIDEO[(vsel & 0x7F) as usize % VIDEO.len()];
    let a = AUDIO[(asel & 0x7F) as usize % AUDIO.len()];
    let mut streams = Vec::new();
    let mut video_idx = None;
    let mut audio_idx = None;
    if use_video {
        let mut p = CodecParameters::video(CodecId::new(v.0));
        p.width = Some(640);
        p.height = Some(360);
        if v.1 && opt & 0x01 != 0 {
            p.extradata = vec![1, 2, 3, vsel];
        }
        video_idx = Some(streams.len() as u32);
        streams.push(StreamInfo {
            index: streams.len() as u32,
            time_base: ms,
            duration: None,
            start_time: Some(0),
            params: p,
        });
    }
    if use_audio {
        let mut p = CodecParameters::audio(CodecId::new(a.0));
        p.sample_rate = Some(a.1);
        p.channels = Some(a.2);
        if a.3 && opt & 0x02 != 0 {
            p.extradata = vec![9, 8, asel];
        }
        audio_idx = Some(streams.len() as u32);
        streams.push(StreamInfo {
            index: streams.len() as u32,
            time_base: ms,
            duration: None,
            start_time: Some(0),
            params: p,
        });
    }

    let sink = Sink {
        seekable: opt & 0x04 == 0,
        ..Sink::default()
    };
    let options = FlvMuxerOptions::new()
        .with_keyframe_index(((opt >> 4) as usize) * 3)
        .with_finalize(opt & 0x08 == 0);
    let mut m = open_muxer_with_options(Box::new(sink.clone()), &streams, options)
        .expect("listed codecs are muxable");
    m.write_header().unwrap();

    // Expected demuxer view: (is_video, dts, pts, key, header, payload).
    let mut want: Vec<(bool, i64, i64, bool, bool, Vec<u8>)> = Vec::new();
    let mut configs: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    for (i, s) in streams.iter().enumerate() {
        if !s.params.extradata.is_empty() {
            let is_video = Some(i as u32) == video_idx;
            configs[usize::from(is_video)] = s.params.extradata.clone();
            want.push((is_video, 0, 0, true, true, s.params.extradata.clone()));
        }
    }
    let mut dts = 0i64;
    let mut first_video = true;
    let mut rest = &data[3..];
    while rest.len() >= 3 && want.len() < 400 {
        let (c, step, len) = (rest[0], rest[1], rest[2] as usize);
        rest = &rest[3..];
        let n = len.min(rest.len());
        let payload = rest[..n].to_vec();
        rest = &rest[n..];
        let is_video = match (video_idx, audio_idx) {
            (Some(_), Some(_)) => c & 0x01 != 0,
            (Some(_), None) => true,
            _ => false,
        };
        let idx = if is_video { video_idx } else { audio_idx }.unwrap();
        dts += step as i64;
        let has_config = if is_video { v.1 } else { a.3 };
        if c & 0x0E == 0x0E && has_config && !payload.is_empty() {
            // A configuration record mid-stream.
            let mut p = Packet::new(idx, ms, payload.clone());
            p.dts = Some(dts);
            p.pts = Some(dts);
            p.flags.header = true;
            m.write_packet(&p).unwrap();
            let slot = &mut configs[usize::from(is_video)];
            if *slot != payload {
                *slot = payload.clone();
                want.push((is_video, dts, dts, true, true, payload));
            }
            continue;
        }
        let cto = if is_video && v.2 { ((c >> 4) as i64) * 7 } else { 0 };
        // Start on a key frame so the stream is seekable from the top.
        let key = !is_video || c & 0x02 != 0 || first_video;
        if is_video {
            first_video = false;
        }
        let mut p = Packet::new(idx, ms, payload.clone());
        p.dts = Some(dts);
        p.pts = Some(dts + cto);
        p.flags.keyframe = key;
        m.write_packet(&p).unwrap();
        want.push((is_video, dts, dts + cto, key, false, payload));
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = sink.data.lock().unwrap().clone();

    let has_media = !want.is_empty();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = match open_demuxer_concrete(input, &NullCodecResolver) {
        Ok(d) => d,
        Err(_) => {
            assert!(!has_media, "a file with media tags must open");
            return;
        }
    };
    // Which demuxer stream is video?
    let dv = d
        .streams()
        .iter()
        .position(|s| s.params.media_type == oxideav_core::MediaType::Video)
        .map(|i| i as u32);
    for s in d.streams() {
        let is_video = Some(s.index) == dv;
        let declared = if is_video { v.0 } else { a.0 };
        assert_eq!(s.params.codec_id.as_str(), declared);
        if !is_video {
            assert_eq!(s.params.sample_rate, Some(a.1));
            assert_eq!(s.params.channels, Some(a.2));
        } else {
            assert_eq!((s.params.width, s.params.height), (Some(640), Some(360)));
        }
    }
    // The one disagreement a muxer cannot avoid without seeking back:
    // a stream it announced in the header flags that never got a tag.
    let finalized = opt & 0x0C == 0;
    assert!(
        !d.metadata().iter().any(|(k, _)| k.starts_with("flv.conflict.")
            && (finalized || k != "flv.conflict.header_flags")),
        "{:?}",
        d.metadata()
    );

    // (A frame with an empty payload still makes a tag — the codec
    // framing bytes — but the demuxer does not hand out empty frames.)
    let mut got = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => got.push(p),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error on muxer output: {e}"),
        }
    }
    let streams_seen: [bool; 2] = [
        d.streams().iter().any(|s| Some(s.index) != dv),
        dv.is_some(),
    ];
    let expect: Vec<_> = want
        .iter()
        .filter(|w| streams_seen[usize::from(w.0)])
        .filter(|w| w.4 || !w.5.is_empty())
        .collect();
    assert_eq!(got.len(), expect.len(), "packet count");
    for (g, w) in got.iter().zip(&expect) {
        assert_eq!(Some(g.stream_index) == dv, w.0);
        assert_eq!(g.dts, Some(w.1));
        assert_eq!(g.data, w.5);
        if w.4 {
            assert!(g.flags.header);
        } else {
            assert_eq!(g.pts, Some(w.2));
            assert_eq!(g.flags.keyframe, w.3);
            assert!(!g.flags.header);
        }
    }
    let st = d.stats();
    assert_eq!(st.previous_tag_size_mismatches, 0);
    assert_eq!(st.unknown_tags_skipped, 0);
    assert_eq!(st.codec_changes, 0);
    assert!(!st.ended_mid_tag);
    assert!(!st.keyframe_table_rejected);

    // Seek: lands on a sync point at or before the target (or on the
    // first one when the target precedes it).
    if let Some(vi) = dv {
        // (An empty key-frame tag is still a sync point in the file.)
        let keys: Vec<i64> = want
            .iter()
            .filter(|w| w.0 && !w.4 && w.3)
            .map(|w| w.1)
            .collect();
        if let Some(&first) = keys.first() {
            for target in [dts / 2, dts, 0] {
                let landed = d.seek_to(vi, target).unwrap();
                let best = keys.iter().copied().filter(|k| *k <= target).max();
                assert_eq!(landed, best.unwrap_or(first), "target {target}");
            }
        }
    }
});
