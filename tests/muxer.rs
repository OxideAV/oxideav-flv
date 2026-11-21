//! [`FlvMuxer`] through the registry and directly: every codec
//! carriage, Enhanced-RTMP FourCC tags both ways, in-place `onMetaData`
//! finalisation, the optional keyframe table, sinks that cannot seek,
//! configuration changes, and timestamp handling.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, ContainerRegistry, Demuxer, Error, Muxer, NullCodecResolver, Packet,
    Rational, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_flv::{
    FlvDemuxer, FlvMuxerOptions, open_demuxer_concrete, open_muxer_with_options,
    register_containers,
};

/// In-memory sink whose contents outlive the muxer.
#[derive(Clone, Default)]
struct Shared {
    data: Arc<Mutex<Vec<u8>>>,
    pos: usize,
    seekable: bool,
}

impl Shared {
    fn new(seekable: bool) -> Self {
        Self {
            seekable,
            ..Self::default()
        }
    }
    fn bytes(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }
}

impl Write for Shared {
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

impl Seek for Shared {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        if !self.seekable {
            return Err(std::io::Error::other("not seekable"));
        }
        self.pos = match pos {
            SeekFrom::Start(p) => p as usize,
            SeekFrom::Current(o) => (self.pos as i64 + o) as usize,
            SeekFrom::End(o) => (self.data.lock().unwrap().len() as i64 + o) as usize,
        };
        Ok(self.pos as u64)
    }
}

const MS: TimeBase = TimeBase(Rational { num: 1, den: 1000 });

fn audio(codec: &str, rate: u32, channels: u16) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new(codec));
    p.sample_rate = Some(rate);
    p.channels = Some(channels);
    p.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: MS,
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn video(codec: &str, w: u32, h: u32) -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new(codec));
    p.width = Some(w);
    p.height = Some(h);
    p.frame_rate = Some(Rational::new(25, 1));
    StreamInfo {
        index: 0,
        time_base: MS,
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn pkt(stream: u32, dts: i64, pts: i64, key: bool, data: Vec<u8>) -> Packet {
    let mut p = Packet::new(stream, MS, data);
    p.dts = Some(dts);
    p.pts = Some(pts);
    p.flags.keyframe = key;
    p
}

fn mux(streams: &[StreamInfo], options: FlvMuxerOptions, packets: &[Packet]) -> Vec<u8> {
    let sink = Shared::new(true);
    let mut m = open_muxer_with_options(Box::new(sink.clone()), streams, options).unwrap();
    m.write_header().unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    sink.bytes()
}

fn demux(bytes: Vec<u8>) -> (FlvDemuxer, Vec<Packet>) {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    (d, out)
}

fn meta<'a>(d: &'a FlvDemuxer, key: &str) -> Option<&'a str> {
    d.metadata()
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn body(seed: usize, n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + seed) as u8).collect()
}

#[test]
fn h264_aac_round_trip_through_the_registry() {
    let mut reg = ContainerRegistry::new();
    register_containers(&mut reg);
    assert!(reg.muxer_names().any(|n| n == "flv"));

    let mut a = audio("aac", 48_000, 2);
    a.params.extradata = vec![0x11, 0x90];
    let mut v = video("h264", 1280, 720);
    v.params.extradata = vec![1, 100, 0, 31, 0xFF, 0xE1, 0, 2, 0x67, 0x64];
    let streams = [a, v];

    let mut packets = Vec::new();
    for i in 0..100i64 {
        // Video with a reordering delay: pts = dts + 80 on B-pictures.
        let cto = if i % 3 == 1 { 80 } else { 0 };
        packets.push(pkt(
            1,
            i * 40,
            i * 40 + cto,
            i % 25 == 0,
            body(i as usize, 300),
        ));
        packets.push(pkt(0, i * 40 + 3, i * 40 + 3, true, body(i as usize, 90)));
    }
    let sink = Shared::new(true);
    let mut m = reg
        .open_muxer("flv", Box::new(sink.clone()), &streams)
        .unwrap();
    assert_eq!(m.format_name(), "flv");
    m.write_header().unwrap();
    for p in &packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = sink.bytes();

    let (d, got) = demux(bytes.clone());
    let s = d.streams();
    assert_eq!(s[0].params.codec_id.as_str(), "aac");
    assert_eq!(s[0].params.sample_rate, Some(48_000));
    assert_eq!(s[0].params.channels, Some(2));
    assert_eq!(s[0].params.extradata, [0x11, 0x90]);
    assert_eq!(s[1].params.codec_id.as_str(), "h264");
    assert_eq!(
        (s[1].params.width, s[1].params.height),
        (Some(1280), Some(720))
    );
    assert_eq!(s[1].params.frame_rate, Some(Rational::new(25, 1)));
    assert_eq!(s[1].params.extradata, streams[1].params.extradata);
    // Finalised in place: duration = last pts (3 960 ms + 0), file size.
    assert_eq!(d.duration_micros(), Some(3_963_000));
    assert_eq!(meta(&d, "filesize"), Some(bytes.len().to_string().as_str()));
    assert!(meta(&d, "videodatarate").unwrap().parse::<f64>().unwrap() > 50.0);
    assert!(
        !d.metadata()
            .iter()
            .any(|(k, _)| k.starts_with("flv.conflict"))
    );
    assert_eq!(d.stats(), &Default::default());

    let data: Vec<&Packet> = got.iter().filter(|p| !p.flags.header).collect();
    assert_eq!(got.iter().filter(|p| p.flags.header).count(), 2);
    assert_eq!(data.len(), packets.len());
    for (g, w) in data.iter().zip(&packets) {
        assert_eq!(
            (g.stream_index, g.dts, g.pts, g.flags.keyframe, &g.data),
            (w.stream_index, w.dts, w.pts, w.flags.keyframe, &w.data)
        );
    }
}

#[test]
fn every_legacy_codec_carriage_round_trips() {
    for (codec, rate, ch) in [
        ("mp3", 44_100, 2),
        ("mp3", 48_000, 2),
        ("mp3", 8_000, 1),
        ("aac", 44_100, 2),
        ("pcm_s16le", 22_050, 1),
        ("adpcm_swf", 11_025, 2),
        ("nellymoser", 8_000, 1),
        ("nellymoser", 16_000, 1),
        ("nellymoser", 22_050, 1),
        ("pcm_alaw", 8_000, 1),
        ("pcm_mulaw", 8_000, 1),
        ("speex", 16_000, 1),
    ] {
        let streams = [audio(codec, rate, ch)];
        let packets: Vec<Packet> = (0..5)
            .map(|i| pkt(0, i * 20, i * 20, true, body(i as usize, 40)))
            .collect();
        let (d, got) = demux(mux(&streams, FlvMuxerOptions::new(), &packets));
        let p = &d.streams()[0].params;
        assert_eq!(p.codec_id.as_str(), codec, "{codec} {rate}");
        assert_eq!(p.sample_rate, Some(rate), "{codec} {rate}");
        assert_eq!(p.channels, Some(ch), "{codec} {rate}");
        assert_eq!(got.len(), 5);
        for (g, w) in got.iter().zip(&packets) {
            assert_eq!((g.pts, &g.data), (w.pts, &w.data), "{codec}");
        }
        assert!(
            !d.metadata()
                .iter()
                .any(|(k, _)| k.starts_with("flv.conflict")),
            "{codec} {rate}: {:?}",
            d.metadata()
        );
    }
    for codec in ["flv1", "flashsv", "vp6f", "vp6a", "flashsv2", "h264"] {
        let streams = [video(codec, 320, 240)];
        let packets: Vec<Packet> = (0..6)
            .map(|i| pkt(0, i * 40, i * 40, i % 3 == 0, body(i as usize, 64)))
            .collect();
        let (d, got) = demux(mux(&streams, FlvMuxerOptions::new(), &packets));
        assert_eq!(d.streams()[0].params.codec_id.as_str(), codec);
        assert_eq!(got.len(), 6, "{codec}");
        for (g, w) in got.iter().zip(&packets) {
            assert_eq!(
                (g.pts, g.flags.keyframe, &g.data),
                (w.pts, w.flags.keyframe, &w.data),
                "{codec}"
            );
        }
    }
}

#[test]
fn pcm_u8_and_unrepresentable_parameters() {
    let mut s = audio("pcm_s16le", 44_100, 2);
    s.params.sample_format = Some(SampleFormat::U8);
    let (d, _) = demux(mux(
        &[s],
        FlvMuxerOptions::new(),
        &[pkt(0, 0, 0, true, vec![1, 2])],
    ));
    assert_eq!(d.streams()[0].params.sample_format, Some(SampleFormat::U8));

    let sink = || -> Box<dyn WriteSeek> { Box::new(Shared::new(true)) };
    let opts = FlvMuxerOptions::new;
    // 48 kHz PCM cannot be said in two SoundRate bits.
    let e = open_muxer_with_options(sink(), &[audio("pcm_s16le", 48_000, 2)], opts()).unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)));
    // Neither can 5.1 PCM.
    let e = open_muxer_with_options(sink(), &[audio("pcm_s16le", 44_100, 6)], opts()).unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)));
    // A codec with no FLV tag form.
    let e = open_muxer_with_options(sink(), &[video("theora", 16, 16)], opts()).unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)));
    // Two video streams.
    let e = open_muxer_with_options(
        sink(),
        &[video("h264", 16, 16), video("h264", 16, 16)],
        opts(),
    )
    .unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)));
    assert!(open_muxer_with_options(sink(), &[], opts()).is_err());
}

#[test]
fn enhanced_rtmp_fourcc_codecs_round_trip() {
    for (codec, has_config, has_cto) in [
        ("h265", true, true),
        ("h266", true, true),
        ("av1", true, false),
        ("vp9", true, false),
        ("vp8", false, false),
    ] {
        let mut v = video(codec, 1920, 1080);
        if has_config {
            v.params.extradata = body(9, 23);
        }
        let mut a = audio("opus", 48_000, 2);
        a.params.extradata = b"OpusHead\x01\x02\x38\x01\x80\xbb\0\0\0\0\0".to_vec();
        a.index = 1;
        let streams = [v, a];
        let mut packets = Vec::new();
        for i in 0..12i64 {
            let cto = if has_cto && i % 4 == 2 { 120 } else { 0 };
            packets.push(pkt(
                0,
                i * 40,
                i * 40 + cto,
                i % 6 == 0,
                body(i as usize, 200),
            ));
            packets.push(pkt(1, i * 40, i * 40, true, body(i as usize + 50, 60)));
        }
        let (d, got) = demux(mux(&streams, FlvMuxerOptions::new(), &packets));
        // The demuxer lists audio first.
        let (sa, sv) = (&d.streams()[0], &d.streams()[1]);
        assert_eq!(sv.params.codec_id.as_str(), codec);
        assert_eq!(sa.params.codec_id.as_str(), "opus");
        assert_eq!(
            (sv.params.width, sv.params.height),
            (Some(1920), Some(1080))
        );
        assert_eq!(sa.params.sample_rate, Some(48_000));
        assert_eq!(sa.params.channels, Some(2));
        assert_eq!(sa.params.extradata, streams[1].params.extradata);
        assert_eq!(sv.params.extradata, streams[0].params.extradata, "{codec}");
        assert!(
            !d.metadata()
                .iter()
                .any(|(k, _)| k.starts_with("flv.conflict")),
            "{codec}: {:?}",
            d.metadata()
        );
        let data: Vec<&Packet> = got.iter().filter(|p| !p.flags.header).collect();
        assert_eq!(data.len(), packets.len(), "{codec}");
        for (g, w) in data.iter().zip(&packets) {
            // Stream order differs (demuxer: audio = 0).
            assert_eq!(g.stream_index, 1 - w.stream_index, "{codec}");
            assert_eq!(
                (g.dts, g.pts, g.flags.keyframe, &g.data),
                (w.dts, w.pts, w.flags.keyframe, &w.data),
                "{codec}"
            );
        }
    }
    for (codec, config) in [("flac", true), ("ac3", false), ("eac3", false)] {
        let mut a = audio(codec, 48_000, 2);
        if config {
            a.params.extradata = body(3, 38);
        }
        let packets: Vec<Packet> = (0..4)
            .map(|i| pkt(0, i * 32, i * 32, true, body(i as usize, 70)))
            .collect();
        let (d, got) = demux(mux(
            std::slice::from_ref(&a),
            FlvMuxerOptions::new(),
            &packets,
        ));
        assert_eq!(d.streams()[0].params.codec_id.as_str(), codec);
        assert_eq!(d.streams()[0].params.extradata, a.params.extradata);
        assert_eq!(got.iter().filter(|p| !p.flags.header).count(), 4, "{codec}");
    }
}

fn long_video(n: i64, gop: i64) -> Vec<Packet> {
    (0..n)
        .map(|i| pkt(0, i * 40, i * 40, i % gop == 0, body(i as usize, 150)))
        .collect()
}

#[test]
fn keyframe_table_is_written_in_place_and_used_for_seeking() {
    let streams = [video("flv1", 640, 360)];
    let packets = long_video(3_000, 50); // 60 key frames
    let plain = mux(&streams, FlvMuxerOptions::new(), &packets);
    let tabled = mux(
        &streams,
        FlvMuxerOptions::new().with_keyframe_index(100),
        &packets,
    );
    // Reserving the table costs a fixed amount, known up front.
    assert_eq!(tabled.len() - plain.len(), 100 * 18 + 60);

    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(tabled));
    let mut d = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
    assert!(
        meta(&d, "filler").is_none(),
        "reserved space is not metadata"
    );
    for target in [119_999i64, 0, 61_234, 2_000, 1_999, 100_000] {
        let want = target / 2_000 * 2_000;
        assert_eq!(d.seek_to(0, target).unwrap(), want);
        let p = d.next_packet().unwrap();
        assert!(p.flags.keyframe);
        assert_eq!(p.pts, Some(want));
    }
    assert!(!d.stats().keyframe_table_rejected);
    // The table did the work: the far seeks indexed almost nothing.
    assert!(d.indexed_sync_points() <= 2, "{}", d.indexed_sync_points());
}

#[test]
fn keyframe_table_smaller_than_the_stream_is_thinned_evenly() {
    let streams = [video("flv1", 640, 360)];
    let packets = long_video(4_000, 4); // 1 000 key frames, room for 16
    let bytes = mux(
        &streams,
        FlvMuxerOptions::new().with_keyframe_index(16),
        &packets,
    );
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
    for target in [159_999i64, 3, 80_085, 12_345] {
        let want = target / 160 * 160;
        assert_eq!(d.seek_to(0, target).unwrap(), want);
        assert_eq!(d.next_packet().unwrap().pts, Some(want));
    }
    assert!(!d.stats().keyframe_table_rejected);
}

#[test]
fn sink_that_cannot_seek_still_gets_a_valid_stream() {
    let streams = [video("flv1", 640, 360)];
    let packets = long_video(200, 10);
    let sink = Shared::new(false);
    let mut m = open_muxer_with_options(
        Box::new(sink.clone()),
        &streams,
        FlvMuxerOptions::new().with_keyframe_index(8),
    )
    .unwrap();
    m.write_header().unwrap();
    for p in &packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    m.write_trailer().unwrap(); // idempotent
    let (d, got) = demux(sink.bytes());
    assert_eq!(got.len(), 200);
    // Placeholders stay: duration unknown, empty table (ignored).
    assert_eq!(d.duration_micros(), None);
    let mut d = d;
    assert_eq!(d.seek_to(0, 5_000).unwrap(), 4_800);

    // `with_finalize(false)` gives the same bytes on a seekable sink.
    let again = mux(
        &streams,
        FlvMuxerOptions::new()
            .with_keyframe_index(8)
            .with_finalize(false),
        &packets,
    );
    assert_eq!(again, sink.bytes());
}

#[test]
fn writes_are_streamed_not_accumulated() {
    // The sink sees data long before the trailer, in bounded pieces.
    struct Probe {
        inner: Shared,
        largest: Arc<Mutex<usize>>,
    }
    impl Write for Probe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut l = self.largest.lock().unwrap();
            *l = (*l).max(buf.len());
            self.inner.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Probe {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }
    let sink = Shared::new(true);
    let largest = Arc::new(Mutex::new(0));
    let mut m = open_muxer_with_options(
        Box::new(Probe {
            inner: sink.clone(),
            largest: largest.clone(),
        }),
        &[video("flv1", 640, 360)],
        FlvMuxerOptions::new(),
    )
    .unwrap();
    m.write_header().unwrap();
    for (i, p) in long_video(5_000, 50).iter().enumerate() {
        m.write_packet(p).unwrap();
        if i == 2_500 {
            assert!(sink.bytes().len() > 300_000, "half the stream is out");
        }
    }
    // One large frame goes straight through.
    m.write_packet(&pkt(0, 200_000, 200_000, true, body(1, 500_000)))
        .unwrap();
    m.write_trailer().unwrap();
    assert_eq!(*largest.lock().unwrap(), 500_000);
    let (_, got) = demux(sink.bytes());
    assert_eq!(got.len(), 5_001);
    assert_eq!(got.last().unwrap().data, body(1, 500_000));
}

#[test]
fn configuration_change_mid_stream_round_trips() {
    let mut v = video("h264", 640, 360);
    v.params.extradata = vec![1, 66, 0, 30];
    let mut header = pkt(0, 400, 400, true, vec![1, 100, 0, 40, 0xAA]);
    header.flags.header = true;
    let mut same_again = header.clone();
    same_again.dts = Some(480);
    let packets = vec![
        pkt(0, 0, 0, true, body(1, 50)),
        pkt(0, 40, 40, false, body(2, 50)),
        header.clone(),
        pkt(0, 400, 400, true, body(3, 50)),
        same_again,
        pkt(0, 480, 480, false, body(4, 50)),
    ];
    let (d, got) = demux(mux(&[v], FlvMuxerOptions::new(), &packets));
    let headers: Vec<(Option<i64>, Vec<u8>)> = got
        .iter()
        .filter(|p| p.flags.header)
        .map(|p| (p.pts, p.data.clone()))
        .collect();
    // The repeated identical record was not written twice.
    assert_eq!(
        headers,
        vec![
            (Some(0), vec![1, 66, 0, 30]),
            (Some(400), vec![1, 100, 0, 40, 0xAA])
        ]
    );
    assert_eq!(d.streams()[0].params.extradata, [1, 100, 0, 40, 0xAA]);
    assert_eq!(d.stats().config_updates, 1);
    assert_eq!(got.iter().filter(|p| !p.flags.header).count(), 4);
}

#[test]
fn timestamps_negative_start_other_time_bases_and_wrap() {
    // 90 kHz time base, first dts below zero (encoder delay).
    let tb = TimeBase::new(1, 90_000);
    let mut v = video("h264", 640, 360);
    v.time_base = tb;
    let mk = |dts: i64, pts: i64, key: bool| {
        let mut p = Packet::new(0, tb, vec![1, 2, 3]);
        p.dts = Some(dts);
        p.pts = Some(pts);
        p.flags.keyframe = key;
        p
    };
    let packets = [
        mk(-7_200, 0, true),
        mk(-3_600, 7_200, false),
        mk(0, 3_600, false),
    ];
    let (_, got) = demux(mux(&[v], FlvMuxerOptions::new(), &packets));
    let times: Vec<(Option<i64>, Option<i64>)> = got.iter().map(|p| (p.dts, p.pts)).collect();
    assert_eq!(
        times,
        vec![
            (Some(0), Some(80)),
            (Some(40), Some(160)),
            (Some(80), Some(120))
        ]
    );

    // Past 2^32 ms the wire value wraps; the reader carries it on.
    let start = (1i64 << 32) - 200;
    let packets: Vec<Packet> = (0..10)
        .map(|i| pkt(0, start + i * 40, start + i * 40, i == 0, vec![i as u8]))
        .collect();
    let (d, got) = demux(mux(
        &[video("flv1", 16, 16)],
        FlvMuxerOptions::new(),
        &packets,
    ));
    let pts: Vec<i64> = got.iter().map(|p| p.pts.unwrap()).collect();
    assert_eq!(pts, (0..10).map(|i| start + i * 40).collect::<Vec<_>>());
    assert_eq!(d.stats().timestamp_wraps, 1);
}

#[test]
fn text_metadata_is_carried_and_structural_keys_are_derived() {
    let sink = Shared::new(true);
    let mut m = open_muxer_with_options(
        Box::new(sink.clone()),
        &[video("flv1", 640, 360)],
        FlvMuxerOptions::new(),
    )
    .unwrap();
    assert!(m.carries_metadata_key("title"));
    assert!(!m.carries_metadata_key("duration"));
    assert!(!m.carries_metadata_key("keyframes.times[0]"));
    m.set_metadata(&[
        ("title".into(), "Ünïcode title".into()),
        ("encoder".into(), "oxideav".into()),
        ("duration".into(), "999".into()),
        ("width".into(), "1".into()),
        ("long".into(), "x".repeat(70_000)),
    ])
    .unwrap();
    m.write_header().unwrap();
    assert!(m.set_metadata(&[]).is_err());
    m.write_packet(&pkt(0, 0, 0, true, vec![0; 10])).unwrap();
    let mut last = pkt(0, 960, 960, false, vec![0; 10]);
    last.duration = Some(40);
    m.write_packet(&last).unwrap();
    m.write_trailer().unwrap();
    let (d, _) = demux(sink.bytes());
    assert_eq!(meta(&d, "title"), Some("Ünïcode title"));
    assert_eq!(meta(&d, "encoder"), Some("oxideav"));
    assert_eq!(meta(&d, "long").map(str::len), Some(70_000));
    assert_eq!(meta(&d, "width"), Some("640"));
    // 960 ms + the last packet's 40 ms.
    assert_eq!(d.duration_micros(), Some(1_000_000));
}

#[test]
fn misuse_is_an_error_not_a_corrupt_file() {
    let sink = Shared::new(true);
    let mut m = open_muxer_with_options(
        Box::new(sink.clone()),
        &[video("flv1", 16, 16)],
        FlvMuxerOptions::new(),
    )
    .unwrap();
    assert!(m.write_packet(&pkt(0, 0, 0, true, vec![1])).is_err());
    assert!(m.write_trailer().is_err());
    m.write_header().unwrap();
    assert!(m.write_header().is_err());
    assert!(m.write_packet(&pkt(3, 0, 0, true, vec![1])).is_err());
    // A payload beyond the 24-bit DataSize is refused, and the stream
    // stays intact for the packets around it.
    m.write_packet(&pkt(0, 0, 0, true, vec![7])).unwrap();
    assert!(
        m.write_packet(&pkt(0, 40, 40, false, vec![0; 1 << 24]))
            .is_err()
    );
    m.write_packet(&pkt(0, 80, 80, false, vec![8])).unwrap();
    m.write_trailer().unwrap();
    assert!(m.write_packet(&pkt(0, 120, 120, false, vec![1])).is_err());
    let (d, got) = demux(sink.bytes());
    assert_eq!(got.len(), 2);
    assert_eq!(d.stats(), &Default::default());
}
