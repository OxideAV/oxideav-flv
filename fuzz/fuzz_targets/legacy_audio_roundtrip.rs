#![no_main]

//! Differential mux -> demux test of the legacy audio tag writers
//! (`write_pcm_tag` / `write_adpcm_tag` / `write_alaw_tag` /
//! `write_mulaw_tag` / `write_nellymoser_tag` /
//! `write_nellymoser_8k_mono_tag` / `write_nellymoser_16k_mono_tag` /
//! `write_speex_tag` / `write_mp3_8k_tag`).
//!
//! The fuzzer picks one codec (all tags in a file must share a codec —
//! the demuxer establishes a single audio stream from the first audio
//! tag) and a sequence of fuzz-controlled `SoundData` payloads, muxes an
//! audio-only FLV, and re-parses it through
//! [`oxideav_flv::open_demuxer`]. The contract under test:
//!
//!  * the muxed file always re-opens (writer/parser byte agreement);
//!  * the resolved `params.codec_id` matches the writer's declared
//!    SoundFormat;
//!  * every non-empty payload survives verbatim, in order, on the
//!    packet stream (the demuxer strips exactly the one-byte
//!    AudioTagHeader for every legacy non-AAC format).
//!
//! A payload of zero length is a special case: the demuxer treats an
//! empty audio message as an enhanced-rtmp-v2 "silence" signal, so those
//! tags are excluded from the survival assertion (they surface as a
//! discardable header packet rather than a data packet). The harness
//! caps the tag count so a forged input cannot wedge it.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{NullCodecResolver, ReadSeek};
use oxideav_flv::{
    header, open_demuxer, tag, write_adpcm_tag, write_alaw_tag, write_mp3_8k_tag, write_mulaw_tag,
    write_nellymoser_16k_mono_tag, write_nellymoser_8k_mono_tag, write_nellymoser_tag,
    write_pcm_tag, write_speex_tag,
};

const MAX_TAGS: usize = 64;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    // Pick one codec for the whole file (mixed-codec audio is not valid
    // FLV; the demuxer keeps a single audio stream).
    let selector = data[0] % 9;
    // Header bits derived from the second byte so PCM (the only family
    // where SoundSize / SoundType are authoritative) exercises both.
    let rate_idx = data[1] & 0x03;
    let is_16bit = (data[1] & 0x04) != 0;
    let is_stereo = (data[1] & 0x08) != 0;

    // Expected codec string per selector.
    let expected_codec = match selector {
        0 | 1 => "pcm_s16le",
        2 => "adpcm_swf",
        3 => "pcm_alaw",
        4 => "pcm_mulaw",
        5 | 6 | 7 => "nellymoser",
        _ => "mp3",
    };
    // selector 8 would be Speex, but we fold that below explicitly; keep
    // the codec map exhaustive: remap 8 to Speex here.
    let (expected_codec, is_speex) = if selector == 8 {
        ("speex", true)
    } else {
        (expected_codec, false)
    };

    // Slice the remaining bytes into a series of payloads. Each payload
    // is length-prefixed by one byte (mod 24 so payloads stay small and
    // the harness stays fast).
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    let mut cur = &data[2..];
    while payloads.len() < MAX_TAGS {
        if cur.is_empty() {
            break;
        }
        let len = (cur[0] as usize) % 24;
        cur = &cur[1..];
        if cur.len() < len {
            break;
        }
        payloads.push(cur[..len].to_vec());
        cur = &cur[len..];
    }
    if payloads.is_empty() {
        return;
    }

    // Mux.
    let mut buf = Vec::new();
    if header::write(&mut buf, true, false).is_err() {
        return;
    }
    if tag::write_first_previous_tag_size(&mut buf).is_err() {
        return;
    }
    for (i, p) in payloads.iter().enumerate() {
        let ts = (i as u32) * 20;
        let r = match selector {
            0 => write_pcm_tag(&mut buf, ts, false, rate_idx, is_16bit, is_stereo, p),
            1 => write_pcm_tag(&mut buf, ts, true, rate_idx, is_16bit, is_stereo, p),
            2 => write_adpcm_tag(&mut buf, ts, rate_idx, is_16bit, is_stereo, p),
            3 => write_alaw_tag(&mut buf, ts, rate_idx, is_stereo, p),
            4 => write_mulaw_tag(&mut buf, ts, rate_idx, is_stereo, p),
            5 => write_nellymoser_tag(&mut buf, ts, rate_idx, is_stereo, p),
            6 => write_nellymoser_8k_mono_tag(&mut buf, ts, p),
            7 => write_nellymoser_16k_mono_tag(&mut buf, ts, p),
            _ if is_speex => write_speex_tag(&mut buf, ts, p),
            _ => write_mp3_8k_tag(&mut buf, ts, is_16bit, is_stereo, p),
        };
        if r.is_err() {
            return;
        }
    }

    // The demuxer only mints an audio stream once it sees a non-empty
    // audio tag. If every payload is empty there is no stream to inspect.
    let any_non_empty = payloads.iter().any(|p| !p.is_empty());

    // Demux.
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let mut dmx = match open_demuxer(input, &NullCodecResolver) {
        Ok(d) => d,
        Err(_) => panic!("muxed legacy audio failed to re-open"),
    };

    if any_non_empty {
        let streams = dmx.streams();
        assert_eq!(streams.len(), 1, "expected exactly one audio stream");
        assert_eq!(
            streams[0].params.codec_id.as_str(),
            expected_codec,
            "codec id mismatch for selector {selector}"
        );
    }

    // Collect the data packets (skip discardable silence-signal packets
    // produced by empty payloads) and assert the non-empty payloads
    // survive in order.
    let expected: Vec<&Vec<u8>> = payloads.iter().filter(|p| !p.is_empty()).collect();
    let mut got: Vec<Vec<u8>> = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        if p.flags.discard {
            continue;
        }
        got.push(p.data.clone());
    }
    assert_eq!(
        got.len(),
        expected.len(),
        "packet count mismatch for selector {selector}"
    );
    for (a, b) in got.iter().zip(expected.iter()) {
        assert_eq!(a, *b, "payload body mismatch for selector {selector}");
    }
});
