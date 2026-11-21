//! Long-file throughput bench (plain `std::time`, no harness crate).
//!
//! Synthesises a one-hour FLV (30 fps AVC-tagged video with a keyframe
//! every 2 s + 43 audio tags per second, 263 k media tags, ~120 MB),
//! then measures:
//!
//! * mux packets/s through the registry [`oxideav_core::Muxer`] path
//!   (and through the bare tag writers for comparison);
//! * open time + the number of bytes / read calls / seek calls `open`
//!   issues against the underlying file;
//! * demux packets/s (full drain) from an unbuffered `File`;
//! * seek latency (median / worst of 32 spread targets) with and
//!   without an `onMetaData` keyframe table;
//! * peak heap during open + drain (counting global allocator).
//!
//! Every figure is the median of `RUNS` runs. Run with
//! `cargo bench --bench long_file`; the scratch files land in
//! `$TMPDIR/oxideav-flv-bench-<pid>/` and are removed at exit.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, NullCodecResolver, Packet, ReadSeek,
    StreamInfo, TimeBase, WriteSeek,
};
use oxideav_flv::script::MetadataBag;
use oxideav_flv::{header, open_demuxer, open_muxer, script, tag};

const RUNS: usize = 5;
const DURATION_S: u32 = 3600;
const FPS: u32 = 30;
const GOP: u32 = 60;
const AUDIO_TAG_MS_NUM: u64 = 1024 * 1000;
const AUDIO_RATE: u64 = 44_100;

// ---- counting allocator -----------------------------------------------------

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let live = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let live = LIVE.fetch_add(new - l.size(), Ordering::Relaxed) + new - l.size();
                PEAK.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn heap_mark() -> (usize, u64) {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    (live, ALLOCS.load(Ordering::Relaxed))
}

fn heap_since(mark: (usize, u64)) -> (usize, u64) {
    (
        PEAK.load(Ordering::Relaxed).saturating_sub(mark.0),
        ALLOCS.load(Ordering::Relaxed) - mark.1,
    )
}

// ---- I/O accounting ---------------------------------------------------------

#[derive(Default)]
struct IoStats {
    reads: AtomicU64,
    bytes: AtomicU64,
    seeks: AtomicU64,
}

struct CountingFile {
    f: File,
    stats: Arc<IoStats>,
}

impl Read for CountingFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.f.read(buf)?;
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for CountingFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.stats.seeks.fetch_add(1, Ordering::Relaxed);
        self.f.seek(pos)
    }
}

// ---- synthetic stream -------------------------------------------------------

/// One synthetic media unit in mux order.
struct Unit {
    video: bool,
    key: bool,
    ts_ms: u32,
    len: usize,
}

fn units() -> Vec<Unit> {
    let mut out = Vec::new();
    let total_ms = DURATION_S as u64 * 1000;
    let (mut vi, mut ai) = (0u64, 0u64);
    loop {
        let vt = vi * 1000 / FPS as u64;
        let at = ai * AUDIO_TAG_MS_NUM / AUDIO_RATE;
        if vt >= total_ms && at >= total_ms {
            break;
        }
        if vt <= at {
            let key = vi % GOP as u64 == 0;
            out.push(Unit {
                video: true,
                key,
                ts_ms: vt as u32,
                len: if key { 4096 } else { 520 },
            });
            vi += 1;
        } else {
            out.push(Unit {
                video: false,
                key: true,
                ts_ms: at as u32,
                len: 180,
            });
            ai += 1;
        }
    }
    out
}

const AVC_CONFIG: &[u8] = &[
    0x01, 0x64, 0x00, 0x1F, 0xFF, 0xE1, 0x00, 0x04, 0x67, 0x64, 0x00, 0x1F, 0x01, 0x00, 0x02, 0x68,
    0xEE,
];
const AAC_CONFIG: &[u8] = &[0x12, 0x10];

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + len) as u8).collect()
}

/// Write the stream with the bare tag writers. With `index`, the
/// `onMetaData` tag carries the `keyframes` table.
fn write_with_tag_writers<W: Write>(w: &mut W, units: &[Unit], index: bool) {
    let small = payload(520);
    let big = payload(4096);
    let aud = payload(180);
    let bag_for = |pos: Vec<u64>, times: Vec<f64>| {
        let bag = MetadataBag::new()
            .number("duration", DURATION_S as f64)
            .number("width", 1280.0)
            .number("height", 720.0)
            .number("framerate", FPS as f64)
            .number("videocodecid", 7.0)
            .number("audiocodecid", 10.0)
            .number("audiosamplerate", 44100.0)
            .boolean("stereo", true);
        if index {
            bag.keyframes(pos, times)
        } else {
            bag
        }
    };
    // Keyframe offsets depend on the onMetaData size, which depends only
    // on the keyframe *count* (AMF numbers are fixed-width): size the
    // tag with placeholders first.
    let kf_times: Vec<f64> = units
        .iter()
        .filter(|u| u.video && u.key)
        .map(|u| u.ts_ms as f64 / 1000.0)
        .collect();
    let mut sizing = Vec::new();
    script::write_on_metadata(
        &mut sizing,
        &bag_for(vec![0; kf_times.len()], kf_times.clone()),
    )
    .unwrap();
    let mut pos = 9 + 4 + sizing.len() as u64;
    pos += (11 + 5 + AVC_CONFIG.len() + 4) as u64;
    pos += (11 + 2 + AAC_CONFIG.len() + 4) as u64;
    let mut kf_pos = Vec::new();
    for u in units {
        if u.video && u.key {
            kf_pos.push(pos);
        }
        pos += (11 + u.len + if u.video { 5 } else { 2 } + 4) as u64;
    }
    header::write(w, true, true).unwrap();
    tag::write_first_previous_tag_size(w).unwrap();
    script::write_on_metadata(w, &bag_for(kf_pos, kf_times)).unwrap();
    tag::write_avc_sequence_header(w, 0, AVC_CONFIG).unwrap();
    tag::write_aac_sequence_header(w, 0, AAC_CONFIG).unwrap();
    for u in units {
        if u.video {
            let p = if u.key { &big } else { &small };
            tag::write_avc_nalu_tag(w, u.ts_ms, u.key, 0, p).unwrap();
        } else {
            tag::write_aac_raw_tag(w, u.ts_ms, &aud).unwrap();
        }
    }
}

fn mux_streams() -> Vec<StreamInfo> {
    let tb = TimeBase::new(1, 1000);
    let mut a = CodecParameters::audio(CodecId::new("aac"));
    a.sample_rate = Some(44_100);
    a.channels = Some(2);
    a.extradata = AAC_CONFIG.to_vec();
    let mut v = CodecParameters::video(CodecId::new("h264"));
    v.width = Some(1280);
    v.height = Some(720);
    v.extradata = AVC_CONFIG.to_vec();
    vec![
        StreamInfo {
            index: 0,
            time_base: tb,
            duration: None,
            start_time: Some(0),
            params: a,
        },
        StreamInfo {
            index: 1,
            time_base: tb,
            duration: None,
            start_time: Some(0),
            params: v,
        },
    ]
}

fn mux_packets(units: &[Unit]) -> Vec<Packet> {
    let tb = TimeBase::new(1, 1000);
    units
        .iter()
        .map(|u| {
            let mut p = Packet::new(u32::from(u.video), tb, payload(u.len));
            p.pts = Some(u.ts_ms as i64);
            p.dts = Some(u.ts_ms as i64);
            p.flags.keyframe = u.key;
            p
        })
        .collect()
}

fn median<T: Copy + PartialOrd>(mut v: Vec<T>) -> T {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn open_counted(path: &Path) -> (Box<dyn Demuxer>, Arc<IoStats>, Duration) {
    let stats = Arc::new(IoStats::default());
    let input: Box<dyn ReadSeek> = Box::new(CountingFile {
        f: File::open(path).unwrap(),
        stats: stats.clone(),
    });
    let t = Instant::now();
    let d = open_demuxer(input, &NullCodecResolver).unwrap();
    (d, stats, t.elapsed())
}

fn drain(d: &mut dyn Demuxer) -> u64 {
    let mut n = 0u64;
    loop {
        match d.next_packet() {
            Ok(p) => {
                n += 1;
                std::hint::black_box(&p.data);
            }
            Err(Error::Eof) => return n,
            Err(e) => panic!("demux error after {n} packets: {e}"),
        }
    }
}

fn bench_read(label: &str, path: &Path) {
    // open
    let mut open_us = Vec::new();
    let mut io = (0, 0, 0);
    for _ in 0..RUNS {
        let (_d, s, t) = open_counted(path);
        open_us.push(t.as_secs_f64() * 1e6);
        io = (
            s.reads.load(Ordering::Relaxed),
            s.bytes.load(Ordering::Relaxed),
            s.seeks.load(Ordering::Relaxed),
        );
    }
    println!(
        "[{label}] open: {:.0} us (median of {RUNS}); {} read calls, {} bytes, {} seek calls",
        median(open_us),
        io.0,
        io.1,
        io.2
    );
    // drain
    let mut pps = Vec::new();
    let mut peak = 0usize;
    let mut allocs = 0u64;
    let mut count = 0u64;
    let mut io = (0, 0);
    for _ in 0..RUNS {
        let mark = heap_mark();
        let (mut d, s, _) = open_counted(path);
        let t = Instant::now();
        count = drain(&mut *d);
        let dt = t.elapsed().as_secs_f64();
        let (p, a) = heap_since(mark);
        peak = p;
        allocs = a;
        pps.push(count as f64 / dt);
        io = (
            s.reads.load(Ordering::Relaxed),
            s.seeks.load(Ordering::Relaxed),
        );
    }
    println!(
        "[{label}] demux: {count} packets, {:.0} packets/s (median of {RUNS}); peak heap {} KiB, {:.2} allocations/packet, {:.2} read calls/packet, {:.2} seek calls/packet",
        median(pps),
        peak / 1024,
        allocs as f64 / count as f64,
        io.0 as f64 / count as f64,
        io.1 as f64 / count as f64,
    );
    // seek: 32 targets spread over the hour, visited in a scrambled order.
    let (mut d, _, _) = open_counted(path);
    let video = d
        .streams()
        .iter()
        .find(|s| s.params.codec_id.as_str() == "h264")
        .map(|s| s.index)
        .unwrap();
    let mut lat = Vec::new();
    let mut first = 0.0;
    for i in 0..32u64 {
        let target = ((i * 13) % 32) * (DURATION_S as u64 * 1000) / 32 + 777;
        let t = Instant::now();
        let landed = d.seek_to(video, target as i64).unwrap();
        let p = loop {
            let p = d.next_packet().unwrap();
            if !p.flags.header {
                break p;
            }
        };
        let us = t.elapsed().as_secs_f64() * 1e6;
        if i == 1 {
            first = us;
        }
        lat.push(us);
        std::hint::black_box((landed, p));
    }
    let worst = lat.iter().cloned().fold(0.0f64, f64::max);
    println!(
        "[{label}] seek + first packet: median {:.0} us, worst {:.0} us, first far seek {:.0} us (32 targets)",
        median(lat),
        worst,
        first
    );
}

fn main() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("oxideav-flv-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let units = units();
    println!(
        "synthetic stream: {} s, {} media tags ({} video, {} audio)",
        DURATION_S,
        units.len(),
        units.iter().filter(|u| u.video).count(),
        units.iter().filter(|u| !u.video).count()
    );

    // ---- mux ----------------------------------------------------------------
    let plain = dir.join("plain.flv");
    let indexed = dir.join("indexed.flv");
    let muxed = dir.join("muxed.flv");
    let mut tw = Vec::new();
    for _ in 0..RUNS {
        let t = Instant::now();
        let mut w = BufWriter::new(File::create(&plain).unwrap());
        write_with_tag_writers(&mut w, &units, false);
        w.flush().unwrap();
        tw.push((units.len() + 2) as f64 / t.elapsed().as_secs_f64());
    }
    println!(
        "mux (bare tag writers -> BufWriter<File>): {:.0} tags/s (median of {RUNS})",
        median(tw)
    );
    {
        let mut w = BufWriter::new(File::create(&indexed).unwrap());
        write_with_tag_writers(&mut w, &units, true);
        w.flush().unwrap();
    }
    let packets = mux_packets(&units);
    let streams = mux_streams();
    let mut mx = Vec::new();
    let mut peak = 0;
    let mut allocs = 0;
    for _ in 0..RUNS {
        let mark = heap_mark();
        let t = Instant::now();
        let out: Box<dyn WriteSeek> = Box::new(File::create(&muxed).unwrap());
        let mut m: Box<dyn Muxer> = open_muxer(out, &streams).unwrap();
        m.write_header().unwrap();
        for p in &packets {
            m.write_packet(p).unwrap();
        }
        m.write_trailer().unwrap();
        drop(m);
        mx.push(packets.len() as f64 / t.elapsed().as_secs_f64());
        let (p, a) = heap_since(mark);
        peak = p;
        allocs = a;
    }
    println!(
        "mux (Muxer -> unbuffered File): {:.0} packets/s (median of {RUNS}); peak heap {} KiB, {:.3} allocations/packet; file {} bytes",
        median(mx),
        peak / 1024,
        allocs as f64 / packets.len() as f64,
        std::fs::metadata(&muxed).unwrap().len()
    );
    drop(packets);

    // ---- read ---------------------------------------------------------------
    println!(
        "file size: {} bytes",
        std::fs::metadata(&plain).unwrap().len()
    );
    bench_read("no index", &plain);
    bench_read("onMetaData keyframes", &indexed);
    bench_read("muxer output", &muxed);

    let _ = std::fs::remove_dir_all(&dir);
}
