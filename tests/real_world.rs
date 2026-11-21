//! Behaviour on the streams that turn up in practice: truncated and
//! still-growing files, live captures without duration or index, wrong
//! back-pointers, timestamp wraps and jumps, `onMetaData` that
//! contradicts the tags, late or changing configuration records,
//! empty and unknown tags — and seeking on all of them.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oxideav_core::{Demuxer, Error, NullCodecResolver, Packet, ReadSeek, SampleFormat};
use oxideav_flv::script::MetadataBag;
use oxideav_flv::{FlvDemuxer, open_demuxer_concrete, script};

// ---- file builder -----------------------------------------------------------

struct Flv {
    bytes: Vec<u8>,
    prev: u32,
}

impl Flv {
    fn new(audio: bool, video: bool) -> Self {
        let flags = (u8::from(audio) << 2) | u8::from(video);
        Self {
            bytes: vec![b'F', b'L', b'V', 1, flags, 0, 0, 0, 9],
            prev: 0,
        }
    }

    /// Append one record; returns the offset of the tag itself.
    fn tag(&mut self, kind: u8, ts: u32, body: &[u8]) -> u64 {
        self.bytes.extend_from_slice(&self.prev.to_be_bytes());
        let at = self.bytes.len() as u64;
        self.bytes.push(kind);
        self.bytes
            .extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        self.bytes.extend_from_slice(&ts.to_be_bytes()[1..]);
        self.bytes.push((ts >> 24) as u8);
        self.bytes.extend_from_slice(&[0, 0, 0]);
        self.bytes.extend_from_slice(body);
        self.prev = 11 + body.len() as u32;
        at
    }

    fn meta(&mut self, bag: &MetadataBag) {
        let mut body = Vec::new();
        script::write_on_metadata_body(&mut body, bag).unwrap();
        self.tag(0x12, 0, &body);
    }

    fn finish(mut self) -> Vec<u8> {
        self.bytes.extend_from_slice(&self.prev.to_be_bytes());
        self.bytes
    }
}

/// Sorenson-tagged video body: key or inter frame, `n` payload bytes.
fn video(key: bool, n: usize) -> Vec<u8> {
    let mut b = vec![if key { 0x12 } else { 0x22 }];
    b.resize(1 + n, 0x5A);
    b
}

/// MP3-tagged audio body (44 kHz, 16-bit, stereo).
fn mp3(n: usize) -> Vec<u8> {
    let mut b = vec![0x2F];
    b.resize(1 + n, 0xA5);
    b
}

fn open(bytes: Vec<u8>) -> FlvDemuxer {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    open_demuxer_concrete(input, &NullCodecResolver).expect("open")
}

fn drain(d: &mut FlvDemuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux error: {e}"),
        }
    }
}

/// 30 fps video with a key frame every `gop` frames plus an audio tag
/// every 26 ms, `seconds` long. Returns the file and the key-frame
/// (tag offset, ms) list.
fn av_file(seconds: u32, gop: u32, meta: Option<MetadataBag>) -> (Vec<u8>, Vec<(u64, u32)>) {
    let mut f = Flv::new(true, true);
    if let Some(m) = meta {
        f.meta(&m);
    }
    let mut kfs = Vec::new();
    let (mut vi, mut ai) = (0u32, 0u32);
    loop {
        let (vt, at) = (vi * 1000 / 30, ai * 26);
        if vt >= seconds * 1000 && at >= seconds * 1000 {
            break;
        }
        if vt <= at {
            let key = vi % gop == 0;
            let pos = f.tag(0x09, vt, &video(key, if key { 900 } else { 120 }));
            if key {
                kfs.push((pos, vt));
            }
            vi += 1;
        } else {
            f.tag(0x08, at, &mp3(96));
            ai += 1;
        }
    }
    (f.finish(), kfs)
}

// ---- open is bounded --------------------------------------------------------

struct Counted {
    inner: Cursor<Vec<u8>>,
    bytes: Arc<AtomicU64>,
    end_seeks: Arc<AtomicU64>,
}

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Counted {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        if matches!(pos, SeekFrom::End(_)) {
            self.end_seeks.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.seek(pos)
    }
}

#[test]
fn open_reads_a_bounded_prefix_and_never_measures_the_input() {
    // ~10 minutes, no duration, no index: a live capture.
    let (bytes, _) = av_file(600, 60, None);
    assert!(bytes.len() > 4_000_000);
    let read = Arc::new(AtomicU64::new(0));
    let end_seeks = Arc::new(AtomicU64::new(0));
    let input: Box<dyn ReadSeek> = Box::new(Counted {
        inner: Cursor::new(bytes),
        bytes: read.clone(),
        end_seeks: end_seeks.clone(),
    });
    let mut d = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
    assert_eq!(d.streams().len(), 2);
    assert_eq!(d.duration_micros(), None, "no onMetaData → unknown");
    assert!(
        read.load(Ordering::Relaxed) <= 128 * 1024,
        "open read {} bytes",
        read.load(Ordering::Relaxed)
    );
    // Reading and seeking never ask where the input ends either.
    for _ in 0..100 {
        d.next_packet().unwrap();
    }
    d.seek_to(1, 300_000).unwrap();
    d.next_packet().unwrap();
    assert_eq!(end_seeks.load(Ordering::Relaxed), 0);
}

// ---- truncation -------------------------------------------------------------

#[test]
fn every_cut_point_ends_cleanly_at_the_last_complete_tag() {
    let (bytes, _) = av_file(1, 10, None);
    let full = drain(&mut open(bytes.clone())).len();
    assert!(full > 40);
    // Cut anywhere inside the last three records.
    let tail = 3 * (11 + 4 + 121);
    let mut seen_partial = false;
    for cut in bytes.len() - tail..bytes.len() {
        let mut d = open(bytes[..cut].to_vec());
        let got = drain(&mut d).len();
        assert!(got <= full && got + 4 >= full, "cut {cut}: {got} of {full}");
        seen_partial |= d.stats().ended_mid_tag;
        // And again: still a clean end, no panic, no duplicate packet.
        assert!(matches!(d.next_packet(), Err(Error::Eof)));
    }
    assert!(seen_partial);
    assert!(!open(bytes).stats().ended_mid_tag);
}

// ---- growing file -----------------------------------------------------------

#[derive(Clone)]
struct Growing {
    data: Arc<Mutex<Vec<u8>>>,
    pos: u64,
}

impl Read for Growing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let d = self.data.lock().unwrap();
        let start = (self.pos as usize).min(d.len());
        let n = buf.len().min(d.len() - start);
        buf[..n].copy_from_slice(&d[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Growing {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(o) => (self.pos as i64 + o) as u64,
            SeekFrom::End(o) => (self.data.lock().unwrap().len() as i64 + o) as u64,
        };
        Ok(self.pos)
    }
}

#[test]
fn growing_file_continues_where_it_stopped() {
    let (bytes, kfs) = av_file(20, 30, None);
    let total = drain(&mut open(bytes.clone())).len();
    let shared = Arc::new(Mutex::new(Vec::new()));
    // First instalment ends in the middle of a tag.
    let first = bytes.len() / 3 + 7;
    shared.lock().unwrap().extend_from_slice(&bytes[..first]);
    let input: Box<dyn ReadSeek> = Box::new(Growing {
        data: shared.clone(),
        pos: 0,
    });
    let mut d = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
    let mut got = drain(&mut d).len();
    assert!(d.stats().ended_mid_tag);
    assert!(got > 0 && got < total);

    // A seek past what exists lands on the last key frame present.
    let last_present = kfs
        .iter()
        .filter(|(pos, _)| (*pos as usize) + 11 + 901 <= first)
        .map(|(_, t)| *t as i64)
        .max()
        .unwrap();
    assert_eq!(d.seek_to(1, 19_000).unwrap(), last_present);

    // The writer catches up; the same demuxer sees the rest.
    let mid = 2 * bytes.len() / 3;
    shared.lock().unwrap().extend_from_slice(&bytes[first..mid]);
    let landed = d.seek_to(1, 19_000).unwrap();
    assert!(landed > last_present);
    shared.lock().unwrap().extend_from_slice(&bytes[mid..]);
    assert_eq!(d.seek_to(1, 19_000).unwrap(), 19_000);

    // Sequential reading across the growth points loses nothing.
    d.seek_to(1, 0).unwrap();
    got = drain(&mut d).len();
    assert_eq!(got, total);
    assert!(!d.stats().ended_mid_tag);
}

#[test]
fn sequential_read_resumes_mid_tag_without_loss() {
    let (bytes, _) = av_file(3, 30, None);
    let want: Vec<(u32, i64, usize)> = drain(&mut open(bytes.clone()))
        .iter()
        .map(|p| (p.stream_index, p.pts.unwrap(), p.data.len()))
        .collect();
    let shared = Arc::new(Mutex::new(bytes[..4_000].to_vec()));
    let input: Box<dyn ReadSeek> = Box::new(Growing {
        data: shared.clone(),
        pos: 0,
    });
    let mut d = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
    let mut got = Vec::new();
    let mut fed = 4_000;
    loop {
        match d.next_packet() {
            Ok(p) => got.push((p.stream_index, p.pts.unwrap(), p.data.len())),
            Err(Error::Eof) if fed < bytes.len() => {
                // Feed an awkward amount so tags straddle the boundary.
                let next = (fed + 777).min(bytes.len());
                shared.lock().unwrap().extend_from_slice(&bytes[fed..next]);
                fed = next;
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(got, want);
}

// ---- PreviousTagSize --------------------------------------------------------

#[test]
fn wrong_previous_tag_size_is_counted_not_trusted() {
    let (good, _) = av_file(2, 15, None);
    let want = drain(&mut open(good.clone())).len();
    let mut d = open(good.clone());
    drain(&mut d);
    assert_eq!(d.stats().previous_tag_size_mismatches, 0);

    // Rewrite every back-pointer with garbage.
    let mut bad = good;
    let mut pos = 9;
    let mut records = 0;
    while pos + 15 <= bad.len() {
        let size = u32::from_be_bytes([0, bad[pos + 5], bad[pos + 6], bad[pos + 7]]) as usize;
        bad[pos..pos + 4].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        pos += 15 + size;
        records += 1;
    }
    let mut d = open(bad);
    assert_eq!(drain(&mut d).len(), want);
    assert_eq!(d.stats().previous_tag_size_mismatches, records);
    // Seeking does not depend on them either.
    assert_eq!(d.seek_to(1, 1_400).unwrap(), 1_000);
}

// ---- timestamps -------------------------------------------------------------

#[test]
fn timestamp_wrap_at_32_bits_stays_monotonic() {
    let mut f = Flv::new(false, true);
    let start = 0xFFFF_FE00u32;
    for i in 0..40u32 {
        f.tag(0x09, start.wrapping_add(i * 40), &video(i % 10 == 0, 50));
    }
    let mut d = open(f.finish());
    let pts: Vec<i64> = drain(&mut d).iter().map(|p| p.pts.unwrap()).collect();
    assert_eq!(pts[0], start as i64);
    assert!(pts.windows(2).all(|w| w[1] - w[0] == 40), "{pts:?}");
    assert!(*pts.last().unwrap() > u32::MAX as i64);
    assert_eq!(d.stats().timestamp_wraps, 1);
    assert_eq!(d.stats().timestamp_discontinuities, 0);
    // Seek to a time beyond the wrap, on the extended timeline.
    let target = start as i64 + 30 * 40 + 5;
    assert_eq!(d.seek_to(0, target).unwrap(), start as i64 + 30 * 40);
    assert_eq!(d.next_packet().unwrap().pts, Some(start as i64 + 30 * 40));
}

#[test]
fn timestamp_wrap_at_24_bits_without_extension_stays_monotonic() {
    // A writer that never fills TimestampExtended: the low 24 bits
    // roll over on their own. Audio and video interleave slightly out
    // of order across the roll-over.
    let mut f = Flv::new(true, true);
    let start = (1u32 << 24) - 400;
    let mut expect = Vec::new();
    for i in 0..30u32 {
        let v = start as i64 + (i * 40) as i64;
        f.tag(0x09, (v as u32) & 0x00FF_FFFF, &video(i % 5 == 0, 50));
        expect.push(v);
        // The audio tag trails the video tag by 15 ms.
        let a = v - 15;
        f.tag(0x08, (a as u32) & 0x00FF_FFFF, &mp3(20));
        expect.push(a);
    }
    let mut d = open(f.finish());
    let pts: Vec<i64> = drain(&mut d).iter().map(|p| p.pts.unwrap()).collect();
    assert_eq!(pts, expect);
    assert_eq!(d.stats().timestamp_wraps, 1);
    // After a seek the timeline is the same one.
    let target = start as i64 + 20 * 40;
    assert_eq!(d.seek_to(1, target + 7).unwrap(), target);
    assert_eq!(d.next_packet().unwrap().pts, Some(target));
}

#[test]
fn timestamp_discontinuity_is_reported_and_followed() {
    let mut f = Flv::new(false, true);
    for i in 0..10u32 {
        f.tag(0x09, 50_000 + i * 40, &video(i == 0, 50));
    }
    // The source restarted: time jumps back to zero…
    for i in 0..10u32 {
        f.tag(0x09, i * 40, &video(i == 0, 50));
    }
    // …and later leaps forward by an hour.
    for i in 0..10u32 {
        f.tag(0x09, 3_600_000 + i * 40, &video(i == 0, 50));
    }
    let mut d = open(f.finish());
    assert_eq!(d.streams()[0].start_time, Some(50_000));
    let pts: Vec<i64> = drain(&mut d).iter().map(|p| p.pts.unwrap()).collect();
    assert_eq!(pts[9], 50_360);
    assert_eq!(pts[10], 0);
    assert_eq!(pts[20], 3_600_000);
    assert_eq!(d.stats().timestamp_discontinuities, 2);
    assert_eq!(d.stats().timestamp_wraps, 0);
}

// ---- onMetaData vs the tags -------------------------------------------------

#[test]
fn tag_header_wins_over_contradicting_onmetadata() {
    let mut f = Flv::new(true, true);
    f.meta(
        &MetadataBag::new()
            .number("audiocodecid", 2.0) // says MP3
            .number("audiosamplerate", 44_100.0)
            .number("audiosamplesize", 8.0)
            .boolean("stereo", true)
            .number("videocodecid", 7.0), // says AVC
    );
    // Linear PCM little-endian, 22 kHz, 16-bit, mono.
    let pcm = [(3u8 << 4) | (2 << 2) | 0x02, 1, 2, 3, 4];
    f.tag(0x08, 0, &pcm);
    f.tag(0x09, 0, &video(true, 30)); // Sorenson, not AVC
    let d = open(f.finish());
    let a = &d.streams()[0].params;
    assert_eq!(a.codec_id.as_str(), "pcm_s16le");
    assert_eq!(a.sample_rate, Some(22_050));
    assert_eq!(a.channels, Some(1));
    assert_eq!(a.sample_format, Some(SampleFormat::S16));
    assert_eq!(d.streams()[1].params.codec_id.as_str(), "flv1");
    let md = d.metadata();
    let get = |k: &str| {
        md.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("missing {k} in {md:?}"))
    };
    assert_eq!(
        get("flv.conflict.audiocodecid"),
        "declared=mp3 stream=pcm_s16le"
    );
    assert_eq!(
        get("flv.conflict.audiosamplerate"),
        "declared=44100 stream=22050"
    );
    assert_eq!(get("flv.conflict.stereo"), "declared=true stream=false");
    assert_eq!(
        get("flv.conflict.audiosamplesize"),
        "declared=U8 stream=S16"
    );
    assert_eq!(
        get("flv.conflict.videocodecid"),
        "declared=h264 stream=flv1"
    );
}

#[test]
fn header_flags_that_disagree_with_the_tags_are_reported() {
    // Header says video only; the file has audio only.
    let mut f = Flv::new(false, true);
    for i in 0..5 {
        f.tag(0x08, i * 26, &mp3(40));
    }
    let d = open(f.finish());
    assert_eq!(d.streams().len(), 1);
    assert!(
        d.metadata()
            .iter()
            .any(|(k, v)| k == "flv.conflict.header_flags" && v == "declared=video stream=audio")
    );
    // An honest file reports nothing.
    let (bytes, _) = av_file(1, 10, None);
    assert!(
        !open(bytes)
            .metadata()
            .iter()
            .any(|(k, _)| k.starts_with("flv.conflict."))
    );
}

// ---- configuration records --------------------------------------------------

fn aac_config(cfg: &[u8]) -> Vec<u8> {
    let mut b = vec![0xAF, 0x00];
    b.extend_from_slice(cfg);
    b
}

fn aac_raw(n: usize) -> Vec<u8> {
    let mut b = vec![0xAF, 0x01];
    b.resize(2 + n, 0x33);
    b
}

fn avc_config(cfg: &[u8]) -> Vec<u8> {
    let mut b = vec![0x17, 0x00, 0, 0, 0];
    b.extend_from_slice(cfg);
    b
}

fn avc_frame(key: bool, n: usize) -> Vec<u8> {
    let mut b = vec![if key { 0x17 } else { 0x27 }, 0x01, 0, 0, 0];
    b.resize(5 + n, 0x44);
    b
}

#[test]
fn sequence_header_after_the_first_frames_is_found_at_open() {
    let mut f = Flv::new(true, true);
    for i in 0..4 {
        f.tag(0x08, i * 23, &aac_raw(30));
        f.tag(0x09, i * 33, &avc_frame(i == 0, 60));
    }
    f.tag(0x08, 92, &aac_config(&[0x12, 0x10]));
    f.tag(0x09, 132, &avc_config(&[1, 100, 0, 31, 0xFF]));
    f.tag(0x08, 115, &aac_raw(30));
    let d = open(f.finish());
    assert_eq!(d.streams()[0].params.extradata, [0x12, 0x10]);
    assert_eq!(d.streams()[1].params.extradata, [1, 100, 0, 31, 0xFF]);
}

#[test]
fn late_and_changing_sequence_headers_update_the_stream() {
    let mut f = Flv::new(true, true);
    f.tag(0x09, 0, &avc_config(&[1, 66, 0, 30]));
    // Far more raw AAC than discovery looks at, then the config.
    for i in 0..400u32 {
        f.tag(0x08, i * 23, &aac_raw(20));
        if i % 8 == 0 {
            f.tag(0x09, i * 23, &avc_frame(i % 80 == 0, 40));
        }
    }
    f.tag(0x08, 9_200, &aac_config(&[0x11, 0x90]));
    f.tag(0x08, 9_223, &aac_raw(20));
    // The encoder is reconfigured: new AVC and AAC records.
    f.tag(0x09, 9_240, &avc_config(&[1, 100, 0, 40, 0xFF, 0xE1]));
    f.tag(0x09, 9_240, &avc_frame(true, 40));
    f.tag(0x08, 9_246, &aac_config(&[0x12, 0x10]));
    f.tag(0x08, 9_269, &aac_raw(20));
    // The same record again changes nothing.
    f.tag(0x08, 9_292, &aac_config(&[0x12, 0x10]));
    let mut d = open(f.finish());
    assert!(d.streams()[0].params.extradata.is_empty());
    assert_eq!(d.streams()[1].params.extradata, [1, 66, 0, 30]);

    let mut headers = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) if p.flags.header => {
                // The stream already reflects the record the packet
                // announces.
                assert_eq!(
                    d.streams()[p.stream_index as usize].params.extradata,
                    p.data
                );
                headers.push((p.stream_index, p.pts.unwrap(), p.data));
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(
        headers,
        vec![
            (1, 0, vec![1, 66, 0, 30]),
            (0, 9_200, vec![0x11, 0x90]),
            (1, 9_240, vec![1, 100, 0, 40, 0xFF, 0xE1]),
            (0, 9_246, vec![0x12, 0x10]),
            (0, 9_292, vec![0x12, 0x10]),
        ]
    );
    assert_eq!(d.stats().config_updates, 3);
    assert_eq!(d.stats().codec_changes, 0);
}

#[test]
fn codec_switch_mid_stream_rebuilds_the_stream_parameters() {
    let mut f = Flv::new(true, false);
    for i in 0..5 {
        f.tag(0x08, i * 26, &mp3(40));
    }
    f.tag(0x08, 130, &aac_config(&[0x12, 0x10]));
    f.tag(0x08, 130, &aac_raw(40));
    // Enhanced-RTMP Opus: SoundFormat 9, SequenceStart, FourCC.
    let mut opus = vec![0x90];
    opus.extend_from_slice(b"Opus");
    opus.extend_from_slice(b"OpusHead\x01\x02");
    f.tag(0x08, 160, &opus);
    let mut d = open(f.finish());
    assert_eq!(d.streams()[0].params.codec_id.as_str(), "mp3");
    let mut ids = Vec::new();
    while let Ok(p) = d.next_packet() {
        ids.push((
            d.streams()[0].params.codec_id.as_str().to_string(),
            p.flags.header,
        ));
    }
    assert_eq!(ids.len(), 8);
    assert_eq!(ids[4].0, "mp3");
    assert_eq!(ids[5], ("aac".to_string(), true));
    assert_eq!(ids[7], ("opus".to_string(), true));
    assert_eq!(d.streams()[0].params.extradata, b"OpusHead\x01\x02");
    assert_eq!(d.stats().codec_changes, 2);
}

#[test]
fn pcm_layout_change_follows_the_tag_header() {
    let mut f = Flv::new(true, false);
    f.tag(0x08, 0, &[(3 << 4) | (3 << 2) | 0x02 | 0x01, 0, 0, 0, 0]);
    f.tag(0x08, 10, &[(3 << 4) | (1 << 2), 0, 0]); // 11 kHz, 8-bit, mono
    let mut d = open(f.finish());
    assert_eq!(d.streams()[0].params.sample_rate, Some(44_100));
    d.next_packet().unwrap();
    d.next_packet().unwrap();
    let p = &d.streams()[0].params;
    assert_eq!(
        (p.sample_rate, p.channels, p.sample_format),
        (Some(11_025), Some(1), Some(SampleFormat::U8))
    );
    assert_eq!(d.stats().config_updates, 1);
}

// ---- odd tags ---------------------------------------------------------------

#[test]
fn zero_length_and_unknown_tags_are_stepped_over() {
    let mut f = Flv::new(true, true);
    f.tag(0x09, 0, &video(true, 20));
    f.tag(0x08, 0, &mp3(20));
    f.tag(0x09, 33, &[]); // empty video tag
    f.tag(0x0F, 40, &[1, 2, 3, 4, 5, 6]); // undefined tag type
    f.tag(0x1F, 41, &[]); // undefined, empty
    f.tag(0x12, 45, &[]); // empty script tag
    f.tag(0x12, 46, &[0xFF, 0xFF, 0xFF]); // script tag that is not AMF
    f.tag(0x08, 52, &[]); // empty audio tag: the silence signal
    f.tag(0x09, 66, &video(false, 20));
    let mut d = open(f.finish());
    let pkts = drain(&mut d);
    let shape: Vec<(u32, i64, usize, bool)> = pkts
        .iter()
        .map(|p| {
            (
                p.stream_index,
                p.pts.unwrap(),
                p.data.len(),
                p.flags.discard,
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (1, 0, 20, false),
            (0, 0, 20, false),
            (0, 52, 0, true),
            (1, 66, 20, false)
        ]
    );
    assert_eq!(d.stats().unknown_tags_skipped, 2);
}

#[test]
fn stream_beyond_the_discovery_window_needs_a_declaration() {
    let build = |declare: bool| {
        let mut f = Flv::new(true, declare);
        if declare {
            f.meta(
                &MetadataBag::new()
                    .number("videocodecid", 2.0)
                    .number("width", 320.0)
                    .number("height", 240.0),
            );
        }
        for i in 0..600u32 {
            f.tag(0x08, i * 26, &mp3(30));
        }
        f.tag(0x09, 15_600, &video(true, 50));
        f.finish()
    };
    // Declared (header flag + videocodecid): the slot exists from open
    // and fills in when the first tag arrives.
    let mut d = open(build(true));
    assert_eq!(d.streams().len(), 2);
    let v = &d.streams()[1].params;
    assert_eq!(
        (v.codec_id.as_str(), v.width, v.height),
        ("flv1", Some(320), Some(240))
    );
    assert_eq!(d.streams()[1].start_time, None);
    let pkts = drain(&mut d);
    assert_eq!(pkts.last().unwrap().stream_index, 1);
    assert_eq!(d.streams()[1].start_time, Some(15_600));
    assert_eq!(d.stats().undeclared_stream_tags_dropped, 0);
    assert_eq!(d.stats().codec_changes, 0);

    // Undeclared: the stream list cannot grow after open, so the tag
    // is dropped — and counted.
    let mut d = open(build(false));
    assert_eq!(d.streams().len(), 1);
    assert_eq!(drain(&mut d).len(), 600);
    assert_eq!(d.stats().undeclared_stream_tags_dropped, 1);
}

#[test]
fn late_script_tags_join_the_metadata() {
    let mut f = Flv::new(true, false);
    for i in 0..300u32 {
        f.tag(0x08, i * 26, &mp3(30));
    }
    let mut body = Vec::new();
    script::write_on_metadata_body(
        &mut body,
        &MetadataBag::new()
            .number("duration", 12.5)
            .string("title", "late"),
    )
    .unwrap();
    f.tag(0x12, 7_800, &body);
    f.tag(0x08, 7_800, &mp3(30));
    let mut d = open(f.finish());
    assert_eq!(d.duration_micros(), None);
    drain(&mut d);
    assert_eq!(d.duration_micros(), Some(12_500_000));
    assert!(
        d.metadata()
            .iter()
            .any(|(k, v)| k == "title" && v == "late")
    );
    // Reading it a second time does not duplicate it.
    d.seek_to(0, 0).unwrap();
    drain(&mut d);
    assert_eq!(d.metadata().iter().filter(|(k, _)| k == "title").count(), 1);
}

// ---- seeking ----------------------------------------------------------------

fn at_or_before(kfs: &[(u64, u32)], target: i64) -> i64 {
    kfs.iter()
        .map(|(_, t)| *t as i64)
        .filter(|t| *t <= target)
        .max()
        .unwrap_or(kfs[0].1 as i64)
}

fn check_seeks(d: &mut FlvDemuxer, kfs: &[(u64, u32)], targets: &[i64]) {
    for &t in targets {
        let want = at_or_before(kfs, t);
        // Seek on either stream: both land on the video key frame.
        let stream = t.rem_euclid(2) as u32;
        assert_eq!(d.seek_to(stream, t).unwrap(), want, "target {t}");
        let p = loop {
            let p = d.next_packet().unwrap();
            if p.stream_index == 1 {
                break p;
            }
        };
        assert!(p.flags.keyframe);
        assert_eq!(p.pts, Some(want), "target {t}");
    }
}

const TARGETS: &[i64] = &[
    0,
    1,
    999,
    1_000,
    1_001,
    59_999,
    31_337,
    4_000,
    4_033,
    58_000,
    12_345,
    2_999,
    60_000,
    1 << 40,
    -5,
    30_000,
];

#[test]
fn seek_without_a_table_is_exact_and_builds_the_index_once() {
    let (bytes, kfs) = av_file(60, 30, None);
    let mut d = open(bytes);
    assert_eq!(d.indexed_sync_points(), 0);
    check_seeks(&mut d, &kfs, TARGETS);
    // Every key frame was indexed exactly once on the way.
    assert_eq!(d.indexed_sync_points(), kfs.len());
    check_seeks(&mut d, &kfs, TARGETS);
    assert_eq!(d.indexed_sync_points(), kfs.len());
    assert!(!d.stats().keyframe_table_rejected);
}

#[test]
fn index_grows_as_a_by_product_of_reading() {
    let (bytes, kfs) = av_file(10, 30, None);
    let mut d = open(bytes);
    let mut n = 0;
    while d.next_packet().is_ok() {
        n += 1;
        if n == 200 {
            assert!(d.indexed_sync_points() > 0 && d.indexed_sync_points() < kfs.len());
        }
    }
    assert_eq!(d.indexed_sync_points(), kfs.len());
}

fn with_table(
    seconds: u32,
    gop: u32,
    pick: impl Fn(&[(u64, u32)]) -> (Vec<u64>, Vec<f64>),
) -> (Vec<u8>, Vec<(u64, u32)>) {
    // The table's size depends only on its length, so build once to
    // learn the offsets and again with them filled in.
    let (_, raw) = av_file(seconds, gop, None);
    let (p0, t0) = pick(&raw);
    let probe = MetadataBag::new()
        .number("duration", seconds as f64)
        .keyframes(vec![0; p0.len()], t0.clone());
    let (_, shifted) = av_file(seconds, gop, Some(probe));
    let (p1, t1) = pick(&shifted);
    let bag = MetadataBag::new()
        .number("duration", seconds as f64)
        .keyframes(p1, t1);
    av_file(seconds, gop, Some(bag))
}

#[test]
fn seek_with_a_complete_table_is_exact() {
    let (bytes, kfs) = with_table(60, 30, |k| {
        (
            k.iter().map(|(p, _)| *p).collect(),
            k.iter().map(|(_, t)| *t as f64 / 1000.0).collect(),
        )
    });
    let mut d = open(bytes);
    assert_eq!(d.duration_micros(), Some(60_000_000));
    check_seeks(&mut d, &kfs, TARGETS);
    assert!(!d.stats().keyframe_table_rejected);
    // The table answered: nothing had to be indexed for a far seek.
    let mut d2 = open(drain_source(&d));
    d2.seek_to(1, 55_000).unwrap();
    assert!(d2.indexed_sync_points() < kfs.len() / 2);
}

/// A fresh copy of the same file for a second demuxer.
fn drain_source(_d: &FlvDemuxer) -> Vec<u8> {
    with_table(60, 30, |k| {
        (
            k.iter().map(|(p, _)| *p).collect(),
            k.iter().map(|(_, t)| *t as f64 / 1000.0).collect(),
        )
    })
    .0
}

#[test]
fn seek_with_a_sparse_table_still_finds_the_exact_key_frame() {
    // Only every seventh key frame is listed.
    let (bytes, kfs) = with_table(60, 30, |k| {
        (
            k.iter().step_by(7).map(|(p, _)| *p).collect(),
            k.iter()
                .step_by(7)
                .map(|(_, t)| *t as f64 / 1000.0)
                .collect(),
        )
    });
    let mut d = open(bytes);
    check_seeks(&mut d, &kfs, TARGETS);
    assert!(!d.stats().keyframe_table_rejected);
}

#[test]
fn seek_with_a_wrong_table_falls_back_to_the_stream() {
    // Offsets that point into the middle of tags.
    let (bytes, kfs) = with_table(30, 30, |k| {
        (
            k.iter().map(|(p, _)| *p + 3).collect(),
            k.iter().map(|(_, t)| *t as f64 / 1000.0).collect(),
        )
    });
    let mut d = open(bytes);
    let targets: Vec<i64> = TARGETS.iter().map(|t| (*t).min(29_999)).collect();
    check_seeks(&mut d, &kfs, &targets);
    assert!(d.stats().keyframe_table_rejected);

    // Right offsets, times that describe some other file.
    let (bytes, kfs) = with_table(30, 30, |k| {
        (
            k.iter().map(|(p, _)| *p).collect(),
            k.iter().map(|(_, t)| *t as f64 / 1000.0 + 500.0).collect(),
        )
    });
    let mut d = open(bytes);
    check_seeks(&mut d, &kfs, &targets);
    assert!(d.stats().keyframe_table_rejected);

    // Offsets beyond the end of the file.
    let (bytes, kfs) = with_table(30, 30, |k| {
        (
            k.iter().map(|(p, _)| *p + (1 << 33)).collect(),
            k.iter().map(|(_, t)| *t as f64 / 1000.0).collect(),
        )
    });
    let mut d = open(bytes);
    check_seeks(&mut d, &kfs, &targets);
    assert!(d.stats().keyframe_table_rejected);
}

#[test]
fn audio_only_index_is_capped_and_seeks_stay_exact() {
    // 70 000 audio tags: more sync points than the index keeps.
    let mut f = Flv::new(true, false);
    for i in 0..70_000u32 {
        f.tag(0x08, i * 10, &[0x2F, i as u8]);
    }
    let mut d = open(f.finish());
    for target in [699_995i64, 5, 123_456, 350_001, 699_990, 10] {
        let want = target / 10 * 10;
        assert_eq!(d.seek_to(0, target).unwrap(), want);
        let p = d.next_packet().unwrap();
        assert_eq!(p.pts, Some(want));
        assert_eq!(p.data, [(want / 10) as u8]);
    }
    assert!(d.indexed_sync_points() <= 65_536);
    assert!(d.indexed_sync_points() >= 30_000);
}

#[test]
fn inactive_streams_are_skipped() {
    let (bytes, _) = av_file(2, 15, None);
    let all = drain(&mut open(bytes.clone()));
    let mut d = open(bytes);
    d.set_active_streams(&[1]);
    let v = drain(&mut d);
    assert!(v.iter().all(|p| p.stream_index == 1));
    assert_eq!(v.len(), all.iter().filter(|p| p.stream_index == 1).count());
    d.set_active_streams(&[0, 1]);
    d.seek_to(0, 0).unwrap();
    assert_eq!(drain(&mut d).len(), all.len());
}

// ---- hostile script data ----------------------------------------------------

#[test]
fn script_tags_cannot_overflow_grow_without_bound_or_exhaust_the_stack() {
    let mut f = Flv::new(true, false);
    // A property that looks like the last possible cue-point index…
    f.meta(&MetadataBag::new().string("cuepoint.4294967295.name", "x"));
    // …followed by a real cue point (index arithmetic must not wrap),
    let mut cue = vec![0x02, 0, 10];
    cue.extend_from_slice(b"onCuePoint");
    cue.extend_from_slice(&[0x03, 0, 1, b'k', 0x05, 0, 0, 0x09]);
    f.tag(0x12, 0, &cue);
    // a script tag nested 100 000 arrays deep,
    let mut deep = vec![0x02, 0, 1, b'd'];
    for _ in 0..100_000 {
        deep.extend_from_slice(&[0x0A, 0, 0, 0, 1]);
    }
    deep.push(0x05);
    f.tag(0x12, 0, &deep);
    f.tag(0x08, 0, &mp3(10));
    // and a caption-style script tag on every frame of a long stream.
    let mut text = vec![0x02, 0, 10];
    text.extend_from_slice(b"onTextData");
    text.extend_from_slice(&[0x02, 0, 2, b'h', b'i']);
    for i in 0..40_000u32 {
        f.tag(0x12, i * 20, &text);
        f.tag(0x08, i * 20, &[0x2F, 1]);
    }
    let mut d = open(f.finish());
    assert_eq!(drain(&mut d).len(), 40_001);
    assert!(d.metadata().len() <= 65_536, "{}", d.metadata().len());
    assert!(d.metadata().len() > 60_000);
    assert!(
        d.metadata()
            .iter()
            .any(|(k, _)| k == "cuepoint.4294967295.k")
    );
}
