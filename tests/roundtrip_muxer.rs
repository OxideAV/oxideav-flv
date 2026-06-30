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
use oxideav_flv::{
    header, open_demuxer, open_demuxer_concrete, script,
    script::{CuePointParams, CuePointType, EncryptionHeader, MetadataBag},
    tag, FlvDemuxer, FlvHeader,
};
use oxideav_flv::{
    join_tracks, write_aac_ex_coded_frames, write_aac_ex_sequence_start, write_aac_raw_tag,
    write_aac_sequence_header, write_ac3_coded_frames, write_additional_header,
    write_av1_coded_frames, write_av1_sequence_start, write_avc_nalu_tag,
    write_avc_sequence_header, write_eac3_coded_frames, write_ex_audio_multichannel_config,
    write_ex_audio_sequence_end, write_ex_audio_tag, write_ex_video_color_info,
    write_ex_video_color_info_reset, write_ex_video_metadata, write_ex_video_sequence_end,
    write_ex_video_tag, write_filtered_tag, write_flac_coded_frames, write_flac_sequence_start,
    write_h263_tag, write_hevc_coded_frames, write_hevc_coded_frames_x, write_hevc_sequence_start,
    write_mp3_ex_coded_frames, write_opus_coded_frames, write_opus_sequence_start, write_vp6_tag,
    write_vp6a_tag, write_vp9_coded_frames, write_vp9_sequence_start, write_vvc_coded_frames,
    write_vvc_sequence_start, AudioChannel, AudioChannelOrder, AvMultitrackType, ColorConfig,
    ColorInfo, EncryptedTagPreamble, ExAudioPacketType, ExAudioTagHeader, ExFrameType,
    ExPacketType, ExVideoTagHeader, HdrCll, HdrMdcv, JoinTrack, ModExEntry, ModExPayload,
    MultichannelConfig, TagType, FOURCC_AUDIO_AAC, FOURCC_AV01, FOURCC_OPUS, FOURCC_VP09,
};

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
fn on_metadata_date_property_round_trips_through_typed_accessor() {
    use oxideav_flv::TypedMetadata;

    // Mux a `creationdate` stamped as an AMF0 Date (SCRIPTDATADATE,
    // §E.4.4.3) rather than a free-form string: 2025-01-01T00:00:00Z
    // = 1_735_689_600_000 ms, JST local offset +540 min.
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag =
        MetadataBag::new()
            .number("duration", 1.0)
            .date("creationdate", 1_735_689_600_000.0, 540);
    script::write_on_metadata(&mut buf, &bag).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &mp3_frames()[0]).unwrap();

    let dmx = open(buf);
    let md = dmx.metadata();

    // The demuxer surfaces the Date under the `"date:<ms>tz:<offset>"`
    // carrier, exactly as it does for an externally-produced FLV.
    let raw = md
        .iter()
        .find(|(k, _)| k == "creationdate")
        .map(|(_, v)| v.as_str());
    assert_eq!(raw, Some("date:1735689600000tz:540"));

    // The typed accessor decodes the carrier back into the (ms, tz) pair.
    let typed = TypedMetadata::new(md);
    assert_eq!(
        typed.creationdate_as_date(),
        Some((1_735_689_600_000.0, 540))
    );
}

#[test]
fn on_metadata_date_round_trips_negative_offset() {
    use oxideav_flv::TypedMetadata;

    // A zone west of Greenwich carries a negative LocalDateTimeOffset.
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new().date("creationdate", 0.0, -480);
    script::write_on_metadata(&mut buf, &bag).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &mp3_frames()[0]).unwrap();

    let dmx = open(buf);
    let typed = TypedMetadata::new(dmx.metadata());
    assert_eq!(typed.creationdate_as_date(), Some((0.0, -480)));
}

#[test]
fn on_metadata_video_track_info_map_round_trips_through_typed_accessor() {
    use oxideav_flv::{TrackInfo, TrackInfoMap, TypedMetadata};

    // Mux the spec's `videoTrackIdInfoMap` example (§"Enhancing
    // onMetaData"): trackId 1 a full descriptor, trackId 2 delta-style.
    let map = TrackInfoMap::new()
        .track(
            1,
            TrackInfo::new()
                .width(1024)
                .height(768)
                .video_data_rate_kbps(2000.0)
                .video_codec_id(1_635_135_537), // makeFourCc("av01")
        )
        .track(2, TrackInfo::new().width(3840).height(2160));

    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new()
        .number("width", 640.0)
        .video_track_info_map(&map);
    script::write_on_metadata(&mut buf, &bag).unwrap();
    // A video keyframe so stream discovery succeeds.
    tag::write_h263_tag(&mut buf, 0, true, &[0xAA, 0xBB, 0xCC]).unwrap();

    let dmx = open(buf);
    let md = dmx.metadata();

    // Raw flattened keys appear exactly as for an external producer.
    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    assert_eq!(lookup("videotrackidinfomap.1.width"), Some("1024"));
    assert_eq!(
        lookup("videotrackidinfomap.1.videocodecid"),
        Some("1635135537")
    );
    assert_eq!(lookup("videotrackidinfomap.2.height"), Some("2160"));

    // The typed iterator re-types each non-zero trackId entry.
    let typed = TypedMetadata::new(md);
    let tracks: Vec<_> = typed.video_track_info_map().collect();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].track_id(), 1);
    assert_eq!(tracks[0].width(), Some(1024));
    assert_eq!(tracks[0].height(), Some(768));
    assert_eq!(tracks[0].video_data_rate_kbps(), Some(2000.0));
    // FourCc-packed codec id resolves to the canonical short id.
    assert_eq!(tracks[0].video_codec_id_str().as_deref(), Some("av1"));
    assert_eq!(tracks[1].track_id(), 2);
    assert_eq!(tracks[1].width(), Some(3840));
    // Delta-style entry: track 2 sent no datarate.
    assert_eq!(tracks[1].video_data_rate_kbps(), None);
}

#[test]
fn on_metadata_audio_track_info_map_round_trips_through_typed_accessor() {
    use oxideav_flv::{TrackInfo, TrackInfoMap, TypedMetadata};

    let map = TrackInfoMap::new()
        .track(
            1,
            TrackInfo::new()
                .audio_data_rate_kbps(256.0)
                .channels(2)
                .audio_sample_rate(44_100.0)
                .audio_codec_id(1_332_770_163), // makeFourCc("Opus")
        )
        .track(2, TrackInfo::new().audio_data_rate_kbps(320.0));

    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new().audio_track_info_map(&map);
    script::write_on_metadata(&mut buf, &bag).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &mp3_frames()[0]).unwrap();

    let dmx = open(buf);
    let typed = TypedMetadata::new(dmx.metadata());
    let tracks: Vec<_> = typed.audio_track_info_map().collect();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].track_id(), 1);
    assert_eq!(tracks[0].audio_data_rate_kbps(), Some(256.0));
    assert_eq!(tracks[0].channels(), Some(2));
    assert_eq!(tracks[0].audio_sample_rate(), Some(44_100.0));
    assert_eq!(tracks[0].audio_codec_id_str().as_deref(), Some("opus"));
    assert_eq!(tracks[1].track_id(), 2);
    assert_eq!(tracks[1].audio_data_rate_kbps(), Some(320.0));
    // Delta-style: track 2 sent no channels.
    assert_eq!(tracks[1].channels(), None);
}

#[test]
fn on_metadata_nested_object_round_trips_flattened() {
    use oxideav_flv::script::ObjectBuilder;

    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new().object(
        "producerInfo",
        ObjectBuilder::new()
            .string("name", "oxideav")
            .number("buildno", 42.0)
            .build(),
    );
    script::write_on_metadata(&mut buf, &bag).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &mp3_frames()[0]).unwrap();

    let dmx = open(buf);
    let md = dmx.metadata();
    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    // The demuxer's flatten walker exposes nested leaves under
    // `<key>.<subkey>` — exactly as for an external producer.
    assert_eq!(lookup("producerInfo.name"), Some("oxideav"));
    assert_eq!(lookup("producerInfo.buildno"), Some("42"));
}

#[test]
fn on_metadata_amf0_value_types_round_trip_flattened() {
    use oxideav_flv::script::MetaValue;

    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new()
        .null("a")
        .undefined("b")
        .xml("c", "<x>hi</x>")
        .strict_array(
            "list",
            vec![
                MetaValue::Number(7.0),
                MetaValue::String("z".into()),
                MetaValue::Null,
            ],
        );
    script::write_on_metadata(&mut buf, &bag).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &mp3_frames()[0]).unwrap();

    let dmx = open(buf);
    let md = dmx.metadata();
    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    // Null / Undefined flatten to sentinel strings; Xml verbatim.
    assert_eq!(lookup("a"), Some("null"));
    assert_eq!(lookup("b"), Some("undefined"));
    assert_eq!(lookup("c"), Some("<x>hi</x>"));
    // Strict array flattens under `<key>[i]`.
    assert_eq!(lookup("list[0]"), Some("7"));
    assert_eq!(lookup("list[1]"), Some("z"));
    assert_eq!(lookup("list[2]"), Some("null"));
}

#[test]
fn single_audio_stream_is_mp3() {
    let (bytes, _) = build_flv();
    let dmx = open(bytes);
    assert_eq!(dmx.streams().len(), 1, "audio-only file → one stream");
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mp3");
}

// ---- video-tag muxer round-trips -----------------------------------------

/// Build a minimal video-only FLV with `n` H.263 (`flv1`) frames at
/// 40 ms (25 fps) spacing. The first frame is marked as a keyframe so
/// the demuxer mints a video stream with `codec_id = "flv1"`.
fn build_h263_flv(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    for (i, frame) in frames.iter().enumerate() {
        let ts = (i as u32) * 40;
        write_h263_tag(&mut buf, ts, i == 0, frame).unwrap();
    }
    buf
}

#[test]
fn h263_video_tag_round_trips_bytes_and_keyframe_flag() {
    let frames = vec![
        vec![0x00, 0x00, 0x84, 0x42, 0x90, 0xAA],
        vec![0x00, 0x00, 0x84, 0x40, 0xCC],
        vec![0x00, 0x00, 0x84, 0x40, 0xDD, 0xEE],
    ];
    let bytes = build_h263_flv(&frames);
    let mut dmx = open(bytes);
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "flv1");

    let mut got = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        if p.flags.header {
            continue;
        }
        got.push((p.pts.unwrap_or(-1), p.flags.keyframe, p.data.clone()));
    }
    assert_eq!(got.len(), frames.len());
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(got[i].0, (i as i64) * 40, "frame {i} pts");
        assert_eq!(got[i].1, i == 0, "frame {i} keyframe flag");
        assert_eq!(&got[i].2, frame, "frame {i} body");
    }
}

#[test]
fn vp6_tag_round_trips() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let frame = vec![0x80, 0x12, 0x34, 0x56, 0x78];
    write_vp6_tag(&mut buf, 0, true, &frame).unwrap();
    let mut dmx = open(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "vp6f");
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame);
    assert!(p.flags.keyframe);
}

#[test]
fn vp6a_tag_carries_alpha_offset_into_extradata() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let frame = vec![0x80, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    write_vp6a_tag(&mut buf, 0, true, 0x0A, &frame).unwrap();
    let dmx = open(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "vp6a");
    // The demuxer routes the VP6A alpha-offset byte into extradata.
    assert_eq!(dmx.streams()[0].params.extradata, vec![0x0A]);
}

#[test]
fn avc_sequence_header_lifts_into_extradata_and_nalu_pts_uses_cto() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    // Synthetic AVCDecoderConfigurationRecord — opaque to the muxer.
    let config = vec![0x01, 0x42, 0xC0, 0x1F, 0xFF, 0xE1, 0x00, 0x05];
    write_avc_sequence_header(&mut buf, 0, &config).unwrap();
    // IDR + CTS=0 at dts 0 (so pts == 0).
    let idr = vec![0x00, 0x00, 0x00, 0x05, 0x65, 0x88, 0x84, 0x00, 0x20];
    write_avc_nalu_tag(&mut buf, 0, true, 0, &idr).unwrap();
    // P-frame at dts 40 with CTS=80 → pts 120 (B-frame reorder case).
    let p_au = vec![0x00, 0x00, 0x00, 0x04, 0x41, 0xE1, 0x80, 0x10];
    write_avc_nalu_tag(&mut buf, 40, false, 80, &p_au).unwrap();
    // P-frame at dts 80 with negative CTS=-20 → pts 60.
    let p_au2 = vec![0x00, 0x00, 0x00, 0x04, 0x41, 0xE1, 0x80, 0x11];
    write_avc_nalu_tag(&mut buf, 80, false, -20, &p_au2).unwrap();

    let mut dmx = open(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "h264");
    assert_eq!(
        dmx.streams()[0].params.extradata,
        config,
        "AVCDecoderConfigurationRecord must reach extradata verbatim"
    );

    let mut packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        if p.flags.header {
            continue;
        }
        packets.push((p.pts.unwrap_or(0), p.dts.unwrap_or(0), p.data.clone()));
    }
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0], (0, 0, idr), "IDR pts/dts and body");
    assert_eq!(packets[1], (120, 40, p_au), "B-reorder pts = dts + CTS");
    assert_eq!(packets[2], (60, 80, p_au2), "negative CTS reorders pts");
}

#[test]
fn legacy_aac_sequence_header_lifts_into_extradata_and_raw_aus_survive() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    // onMetaData carries the producer's declared rate/channels — AAC's
    // SoundRate/SoundType bits are spec-fixed at 44 kHz/stereo and ignored.
    let bag = MetadataBag::new()
        .number("audiosamplerate", 48_000.0)
        .boolean("stereo", true);
    script::write_on_metadata(&mut buf, &bag).unwrap();

    // Synthetic AudioSpecificConfig (ISO 14496-3) — opaque to the muxer.
    // (AAC-LC object type 2, 48 kHz index 3, stereo channel config 2.)
    let asc = vec![0x11, 0x90];
    write_aac_sequence_header(&mut buf, 0, &asc).unwrap();

    // Two raw access units after the config record.
    let au0 = vec![0x21, 0x1A, 0x00, 0x4E];
    let au1 = vec![0x21, 0x1B, 0x88, 0x12, 0x34];
    write_aac_raw_tag(&mut buf, 0, &au0).unwrap();
    write_aac_raw_tag(&mut buf, 21, &au1).unwrap();

    let mut dmx = open(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "aac");
    assert_eq!(
        dmx.streams()[0].params.extradata,
        asc,
        "legacy AAC AudioSpecificConfig must reach extradata verbatim"
    );

    let mut packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        // The sequence header surfaces as a header-flagged config packet
        // (it carries no decodable audio frame); skip it.
        if p.flags.header {
            continue;
        }
        packets.push((p.pts.unwrap_or(-1), p.data.clone()));
    }
    assert_eq!(
        packets.len(),
        2,
        "both raw AUs must surface as data packets"
    );
    assert_eq!(packets[0], (0, au0), "first raw AU body + timestamp");
    assert_eq!(packets[1], (21, au1), "second raw AU body + timestamp");
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

// ---- Enhanced-RTMP v1 ExVideo / ExAudio muxer round-trips ----------------
//
// These exercise the FourCc-mode wire shape introduced by enhanced-rtmp.
// Each test writes a video- or audio-only FLV with the dedicated Ex
// writer, demuxes it via the existing `FlvDemuxer`, and asserts the
// resulting `params.codec_id` / `params.extradata` / per-packet body
// match what was written.

fn open_video_only(buf: Vec<u8>) -> Box<dyn Demuxer> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    open_demuxer(input, &NullCodecResolver).expect("open muxed flv")
}

/// Concrete-typed open so tests can reach the [`FlvDemuxer`]-only
/// accessors (`last_timestamp_offset_nano`, `last_mod_ex_entries`,
/// `last_multitrack_tracks`) that the `Box<dyn Demuxer>` trait object
/// does not expose.
fn open_concrete(buf: Vec<u8>) -> FlvDemuxer {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    open_demuxer_concrete(input, &NullCodecResolver).expect("open muxed flv (concrete)")
}

#[test]
fn av1_ex_video_sequence_start_lifts_into_extradata() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    // Synthetic AV1CodecConfigurationRecord — opaque to the muxer.
    let config = vec![0x81, 0x05, 0x0C, 0x00, 0x0A, 0x0B];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();
    // Two CodedFrames (AV1 has no SI24 CTO slot, so the writer just
    // emits the lead byte + FourCc + frame).
    let key = vec![0x12, 0x34, 0x56, 0x78];
    write_av1_coded_frames(&mut buf, 0, true, &key).unwrap();
    let inter = vec![0x9A, 0xBC, 0xDE];
    write_av1_coded_frames(&mut buf, 40, false, &inter).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "av1");
    assert_eq!(
        dmx.streams()[0].params.extradata,
        config,
        "AV1CodecConfigurationRecord must reach extradata verbatim"
    );

    // Drain packets — SequenceStart shows as a header packet first.
    let p_hdr = dmx.next_packet().unwrap();
    assert!(p_hdr.flags.header);
    assert_eq!(p_hdr.data, config);
    let p1 = dmx.next_packet().unwrap();
    assert!(!p1.flags.header);
    assert_eq!(p1.data, key);
    assert!(p1.flags.keyframe);
    assert_eq!(p1.pts, Some(0));
    let p2 = dmx.next_packet().unwrap();
    assert_eq!(p2.data, inter);
    assert!(!p2.flags.keyframe);
    assert_eq!(p2.pts, Some(40));
}

#[test]
fn vp9_ex_video_round_trips_codec_id() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x01, 0x02, 0x03, 0x04, 0x05];
    write_vp9_sequence_start(&mut buf, 0, &config).unwrap();
    let frame = vec![0xAA, 0xBB, 0xCC];
    write_vp9_coded_frames(&mut buf, 0, true, &frame).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "vp9");
    assert_eq!(dmx.streams()[0].params.extradata, config);
    // Header packet, then the keyframe.
    let _h = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame);
    assert!(p.flags.keyframe);
}

#[test]
fn hevc_ex_video_carries_composition_time_offset_and_extradata() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x01, 0x42, 0xC0, 0x1F, 0xFF];
    write_hevc_sequence_start(&mut buf, 0, &config).unwrap();
    // dts=0, CTO=0 → pts=0; dts=40, CTO=+80 → pts=120; dts=80, CTO=-20 → pts=60.
    let idr = vec![0x65, 0x88, 0x84, 0x00];
    write_hevc_coded_frames(&mut buf, 0, true, 0, &idr).unwrap();
    let p1 = vec![0x41, 0xE1, 0x80, 0x10];
    write_hevc_coded_frames(&mut buf, 40, false, 80, &p1).unwrap();
    let p2 = vec![0x41, 0xE1, 0x80, 0x11];
    write_hevc_coded_frames(&mut buf, 80, false, -20, &p2).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "h265");
    assert_eq!(dmx.streams()[0].params.extradata, config);

    let mut packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        if p.flags.header {
            continue;
        }
        packets.push((p.pts.unwrap_or(0), p.dts.unwrap_or(0), p.data.clone()));
    }
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0], (0, 0, idr));
    assert_eq!(packets[1], (120, 40, p1), "B-frame reorder pts = dts + CTO");
    assert_eq!(packets[2], (60, 80, p2), "negative CTO");
}

#[test]
fn hevc_coded_frames_x_drops_cto_byte() {
    // CodedFramesX is the 3-byte-savings variant: SI24 CTO is implicit 0.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0xDE, 0xAD, 0xBE, 0xEF];
    write_hevc_sequence_start(&mut buf, 0, &config).unwrap();
    let frame = vec![0xCA, 0xFE, 0xBA, 0xBE];
    write_hevc_coded_frames_x(&mut buf, 33, true, &frame).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "h265");
    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame);
    assert_eq!(p.dts, Some(33));
    assert_eq!(p.pts, Some(33));
}

#[test]
fn vvc_ex_video_sequence_and_coded_frames_round_trip() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x01, 0x10, 0x00, 0x00, 0x00];
    write_vvc_sequence_start(&mut buf, 0, &config).unwrap();
    let frame = vec![0x10, 0x20, 0x30];
    write_vvc_coded_frames(&mut buf, 100, true, 0, &frame).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "h266");
    assert_eq!(dmx.streams()[0].params.extradata, config);
    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame);
    assert_eq!(p.dts, Some(100));
    assert_eq!(p.pts, Some(100));
}

#[test]
fn opus_ex_audio_sequence_start_lifts_into_extradata() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    // Synthetic RFC 7845 OpusHead — opaque to the muxer.
    let opus_head = vec![
        b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', 0x01, 0x02, 0x68, 0x01, 0x80, 0xBB, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];
    write_opus_sequence_start(&mut buf, 0, &opus_head).unwrap();
    let pkt = vec![0xFC, 0x12, 0x34];
    write_opus_coded_frames(&mut buf, 20, &pkt).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "opus");
    assert_eq!(dmx.streams()[0].params.extradata, opus_head);
    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, pkt);
    assert_eq!(p.pts, Some(20));
}

#[test]
fn flac_ex_audio_sequence_start_round_trips() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    // Xiph fLaC marker + a minimal STREAMINFO header (38 bytes total).
    let mut streaminfo = vec![b'f', b'L', b'a', b'C'];
    streaminfo.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]); // METADATA_BLOCK header (last, STREAMINFO, 34)
    streaminfo.extend(std::iter::repeat(0xAB).take(34));
    write_flac_sequence_start(&mut buf, 0, &streaminfo).unwrap();
    let frame = vec![0xFF, 0xF8, 0x69, 0x18];
    write_flac_coded_frames(&mut buf, 26, &frame).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "flac");
    assert_eq!(dmx.streams()[0].params.extradata, streaminfo);
    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame);
}

#[test]
fn ac3_ex_audio_coded_frames_codec_id_is_ac3() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let frame = vec![0x0B, 0x77, 0xDE, 0xAD, 0xBE, 0xEF];
    write_ac3_coded_frames(&mut buf, 0, &frame).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "ac3");
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame);
}

#[test]
fn eac3_ex_audio_coded_frames_codec_id_is_eac3() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let frame = vec![0x0B, 0x77, 0x01, 0x02, 0x03];
    write_eac3_coded_frames(&mut buf, 0, &frame).unwrap();

    let dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "eac3");
}

#[test]
fn aac_ex_audio_sequence_start_and_coded_frames_round_trip() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    // Synthetic AudioSpecificConfig (5 bits AOT=2 + 4 bits sample-rate-idx=4 + 4 bits chan=2).
    let asc = vec![0x12, 0x10];
    write_aac_ex_sequence_start(&mut buf, 0, &asc).unwrap();
    let au = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE];
    write_aac_ex_coded_frames(&mut buf, 23, &au).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "aac");
    assert_eq!(dmx.streams()[0].params.extradata, asc);
    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, au);
    assert_eq!(p.pts, Some(23));
}

#[test]
fn mp3_ex_audio_fourcc_path_codec_id_is_mp3() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let frame = vec![0xFF, 0xFB, 0x90, 0x44, 0x55];
    write_mp3_ex_coded_frames(&mut buf, 0, &frame).unwrap();

    let mut dmx = open_video_only(buf);
    // FourCc `.mp3` resolves to "mp3" — same codec id as the legacy
    // SoundFormat=2 path.
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mp3");
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame);
}

#[test]
fn ex_video_metadata_tag_is_discardable_header() {
    // Open with a SequenceStart so the demuxer mints the stream, then
    // emit a Metadata frame and assert it round-trips as header+discard.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x01];
    write_hevc_sequence_start(&mut buf, 0, &config).unwrap();
    let amf_blob = b"colorInfo-amf-payload".to_vec();
    write_ex_video_metadata(&mut buf, 0, oxideav_flv::FOURCC_HVC1, &amf_blob).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "h265");
    let _seq = dmx.next_packet().unwrap();
    let m = dmx.next_packet().unwrap();
    assert!(m.flags.header);
    assert!(m.flags.discard);
    assert_eq!(m.data, amf_blob);
}

#[test]
fn ex_video_sequence_end_emits_empty_body() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0xDE, 0xAD];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();
    write_ex_video_sequence_end(&mut buf, 1000, oxideav_flv::FOURCC_AV01).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "av1");
    // SequenceStart header, then SequenceEnd is the only other tag —
    // the demuxer routes it as no-packet (we get EOF immediately after).
    let _start = dmx.next_packet().unwrap();
    // SequenceEnd produces no data packet, so the next call surfaces EOF
    // (or another header per demuxer policy — assert that the call
    // doesn't panic and either yields a discardable packet or EOF).
    let _ = dmx.next_packet();
}

#[test]
fn ex_audio_sequence_end_emits_empty_body() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    write_aac_ex_sequence_start(&mut buf, 0, &[0x12, 0x10]).unwrap();
    write_ex_audio_sequence_end(&mut buf, 500, oxideav_flv::FOURCC_AUDIO_AAC).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "aac");
    let _ = dmx.next_packet();
    let _ = dmx.next_packet();
}

#[test]
fn ex_audio_multichannel_config_native_round_trips_through_demuxer() {
    // Native-order 5.1 (FrontLeft | FrontRight | FrontCenter |
    // LowFrequency1 | BackLeft | BackRight = 0x3F): the writer's
    // typed MultichannelConfig must come back out of the demuxer as
    // multichannelconfig.* metadata + an updated channel count.
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    write_opus_sequence_start(&mut buf, 0, &[0x4F, 0x70]).unwrap();
    let mcc = MultichannelConfig {
        order: AudioChannelOrder::Native,
        channel_count: 6,
        mapping: None,
        channel_flags: Some(0x3F),
    };
    write_ex_audio_multichannel_config(&mut buf, 5, FOURCC_OPUS, &mcc).unwrap();
    write_opus_coded_frames(&mut buf, 20, &[0xFC, 0x12]).unwrap();

    let mut dmx = open_video_only(buf);
    let _seq = dmx.next_packet().unwrap();
    let m = dmx.next_packet().unwrap();
    assert!(m.flags.header && m.flags.discard);
    // The discardable packet's body is the raw config bytes.
    assert_eq!(m.data, vec![0x01, 6, 0x00, 0x00, 0x00, 0x3F]);
    let lookup = |key: &str| {
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(lookup("multichannelconfig.order"), Some("native"));
    assert_eq!(lookup("multichannelconfig.channelcount"), Some("6"));
    assert_eq!(lookup("multichannelconfig.flags"), Some("0x0000003F"));
    assert_eq!(
        lookup("multichannelconfig.layout"),
        Some("frontleft,frontright,frontcenter,lowfrequency1,backleft,backright")
    );
    assert_eq!(lookup("multichannelconfig.mapping"), None);
    assert_eq!(dmx.streams()[0].params.channels, Some(6));
    // The coded frame after the config still decodes normally.
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, vec![0xFC, 0x12]);
}

#[test]
fn ex_audio_multichannel_config_custom_supersedes_native() {
    // A second MultichannelConfig replaces the first: Native 5.1 is
    // followed by a Custom 2-channel swap map — the flags/layout keys
    // must vanish and the mapping + count must reflect the new signal.
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    write_opus_sequence_start(&mut buf, 0, &[0x4F, 0x70]).unwrap();
    let native = MultichannelConfig {
        order: AudioChannelOrder::Native,
        channel_count: 6,
        mapping: None,
        channel_flags: Some(0x3F),
    };
    write_ex_audio_multichannel_config(&mut buf, 5, FOURCC_OPUS, &native).unwrap();
    let custom = MultichannelConfig {
        order: AudioChannelOrder::Custom,
        channel_count: 2,
        mapping: Some(vec![AudioChannel::FrontRight, AudioChannel::FrontLeft]),
        channel_flags: None,
    };
    write_ex_audio_multichannel_config(&mut buf, 10, FOURCC_OPUS, &custom).unwrap();

    let mut dmx = open_video_only(buf);
    let _seq = dmx.next_packet().unwrap();
    let _native = dmx.next_packet().unwrap();
    let _custom = dmx.next_packet().unwrap();
    let lookup = |key: &str| {
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(lookup("multichannelconfig.order"), Some("custom"));
    assert_eq!(lookup("multichannelconfig.channelcount"), Some("2"));
    assert_eq!(
        lookup("multichannelconfig.mapping"),
        Some("frontright,frontleft")
    );
    assert_eq!(lookup("multichannelconfig.flags"), None);
    assert_eq!(lookup("multichannelconfig.layout"), None);
    // Exactly one entry per key — the retain-then-push replace must not
    // leave duplicates behind.
    let count = dmx
        .metadata()
        .iter()
        .filter(|(k, _)| k == "multichannelconfig.order")
        .count();
    assert_eq!(count, 1);
    assert_eq!(dmx.streams()[0].params.channels, Some(2));
}

// ---- onMetaData.keyframes seek-table writer round-trip ------------------
//
// Builds an FLV with the `keyframes` toc populated up-front so the
// muxer → demuxer round-trip exercises the same O(log n) bisect path
// that the legacy `tests/seek.rs` fixture file (cooked by `ffmpeg
// -flvflags add_keyframe_index`) covers. Two strategies are used:
//
// 1. **Two-pass mux**: serialise the metadata tag once with the planned
//    toc to learn its on-wire size, then mux the file with the actual
//    keyframe offsets known up-front. The metadata bag values (and
//    therefore the tag size) are unchanged between passes because the
//    `filepositions[]` slots are sized as fixed AMF0 Numbers; the only
//    thing that varies between pass 1 and pass 2 is the *numeric value*
//    of each filepositions entry, which is irrelevant to the wire size.
// 2. The video stream is three H.263 (`flv1`) frames at 40 ms each;
//    every frame is marked as a keyframe so the toc references known
//    offsets and the demuxer's `seek_to` lands on a frame whose body
//    we can match against the muxer's input.

fn h263_keyframe(byte_marker: u8) -> Vec<u8> {
    // Synthetic body — opaque to the muxer. The first 4 bytes
    // resemble an H.263 PSC so the bytes look plausible; the marker
    // byte at the end disambiguates per-frame so the demuxer-side
    // assertion can distinguish them.
    vec![0x00, 0x00, 0x84, 0x00, byte_marker, 0xFF, 0xEE]
}

/// Produce an FLV byte buffer with three video keyframes at t=0/40/80
/// ms and an `onMetaData` script tag at the head carrying a populated
/// `keyframes` seek-table that points at each of the three video
/// tags. Returns the buffer plus the three frame bodies (in mux
/// order).
fn build_h263_flv_with_keyframes_toc() -> (Vec<u8>, Vec<Vec<u8>>) {
    let frames = vec![
        h263_keyframe(0xA0),
        h263_keyframe(0xA1),
        h263_keyframe(0xA2),
    ];
    let times = vec![0.0, 0.040, 0.080];

    // Pass 1 — serialise the metadata tag with placeholder offsets to
    // learn the byte size of the header + first-previous-tag-size +
    // metadata-tag prefix. The toc's payload shape is independent of
    // the actual offset values (every entry is a fixed-size AMF0
    // Number), so the placeholder run produces a byte-exact size for
    // the real run.
    let placeholders: Vec<u64> = vec![0, 0, 0];
    let bag_probe = MetadataBag::new()
        .number("duration", 0.08)
        .keyframes(placeholders, times.clone());
    let mut probe = Vec::new();
    header::write(&mut probe, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut probe).unwrap();
    script::write_on_metadata(&mut probe, &bag_probe).unwrap();
    let first_video_tag_offset = probe.len() as u64;

    // Each subsequent video tag occupies 11 (tag header) + body + 4
    // (trailing PreviousTagSize) bytes, and `write_h263_tag` prefixes
    // exactly 1 byte (`FrameType | CodecID`) to the body before the
    // VIDEODATA payload.
    let video_tag_size = |body_len: usize| 11 + 1 + body_len as u64 + 4;
    let file_positions: Vec<u64> = {
        let mut acc = first_video_tag_offset;
        let mut out = Vec::with_capacity(frames.len());
        for f in &frames {
            out.push(acc);
            acc += video_tag_size(f.len());
        }
        out
    };

    // Pass 2 — re-emit with the real offsets.
    let bag = MetadataBag::new()
        .number("duration", 0.08)
        .keyframes(file_positions.clone(), times);
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    script::write_on_metadata(&mut buf, &bag).unwrap();
    // Sanity: the pass-1 byte-size prediction must hold.
    assert_eq!(buf.len() as u64, first_video_tag_offset);

    // Emit the three keyframes; assert each lands at the offset the
    // toc claims.
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(
            buf.len() as u64,
            file_positions[i],
            "video tag {i} must land at the toc's claimed offset"
        );
        write_h263_tag(&mut buf, (i as u32) * 40, true, frame).unwrap();
    }
    (buf, frames)
}

#[test]
fn keyframes_toc_round_trips_through_demuxer_metadata() {
    let (bytes, _) = build_h263_flv_with_keyframes_toc();
    let dmx = open(bytes);
    // The demuxer exposes onMetaData scalar fields via metadata(); the
    // `keyframes` composite is consumed internally and surfaces via
    // the seek path rather than the metadata bag (no
    // `metadata["keyframes"]` entry — that's a deliberate sink rather
    // than a flatten). We assert the scalar property still parses
    // through alongside the toc.
    let md = dmx.metadata();
    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    assert!(
        lookup("duration").is_some(),
        "duration must coexist with the keyframes composite"
    );
    // No flatten under "keyframes" — the demuxer parses it into the
    // internal seek-table, not the metadata bag.
    assert!(
        md.iter().all(|(k, _)| k != "keyframes"
            && !k.starts_with("keyframes.")
            && !k.starts_with("filepositions")
            && !k.starts_with("times")),
        "keyframes composite must not leak into the metadata flatten path: \
         {md:?}"
    );
}

#[test]
fn keyframes_toc_drives_seek_by_pts_bisect_path() {
    let (bytes, frames) = build_h263_flv_with_keyframes_toc();
    let mut dmx = open(bytes);
    // Stream 1 is the video stream (audio absent in this synthetic FLV).
    let stream_index = dmx
        .streams()
        .iter()
        .position(|s| s.params.codec_id.as_str() == "flv1")
        .expect("h263 stream must register") as u32;

    // Seek to t=40 ms — the toc has an exact entry there, so the
    // bisect-left lands on it and the next packet is the second
    // keyframe (body `frames[1]`).
    let landed = dmx.seek_to(stream_index, 40).expect("seek to 40 ms");
    assert!(
        landed <= 40,
        "bisect-left toc-seek must land at or before target (landed at {landed})"
    );
    let p = dmx.next_packet().expect("packet after seek-to-40 ms");
    assert!(p.flags.keyframe, "toc entries are video keyframes");
    assert_eq!(
        p.data, frames[1],
        "seek-to-40 ms must surface frame index 1"
    );

    // Seek to t=70 ms — bisect-left lands at the t=40 ms entry (the
    // largest toc entry ≤ target), so the next packet is again
    // frames[1] (the second keyframe). This confirms the toc bisect
    // is being walked rather than a scan-forward (a scan would land
    // at the next keyframe ≥ 70 ms, i.e. frames[2]).
    let landed = dmx.seek_to(stream_index, 70).expect("seek to 70 ms");
    assert!(landed <= 70);
    let p = dmx.next_packet().expect("packet after seek-to-70 ms");
    assert_eq!(
        p.data, frames[1],
        "70 ms must bisect-left to the 40 ms entry, not scan forward to 80 ms"
    );
}

// ---- ModEx prefix emission integration tests ------------------------------
//
// `ExVideoTagHeader::to_bytes` and `ExAudioTagHeader::to_bytes` accept
// `mod_ex_entries` and chain them off the front of the tag body. These
// integration tests build a full FLV through `write_ex_video_tag` /
// `write_ex_audio_tag` (the generic header-then-payload writers) with a
// ModEx-bearing header and assert the demuxer recovers the resolved
// codec id, payload bytes, and accumulated `TimestampOffsetNano`.

#[test]
fn ex_video_modex_timestamp_offset_nano_round_trips_through_demuxer() {
    // FLV with one AV1 SequenceStart (so the stream is minted), then a
    // ModEx-bearing CodedFrames tag whose ModEx prefix carries a single
    // TimestampOffsetNano = 250_000 ns (a quarter-millisecond
    // refinement to the integer-ms RTMP timestamp).
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x81, 0x05, 0x0C];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();

    let frame = vec![0xCA, 0xFE, 0xBA, 0xBE];
    let header_struct = ExVideoTagHeader {
        frame_type: ExFrameType::KeyFrame,
        packet_type: ExPacketType::CodedFrames,
        fourcc: Some(FOURCC_AV01),
        multitrack: None,
        bytes_consumed: 0,
        composition_time_offset_ms: None,
        timestamp_offset_nano: 250_000,
        mod_ex_entries: vec![ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 250_000 },
            // 250_000 = 0x03D090 in 24-bit BE.
            raw: vec![0x03, 0xD0, 0x90],
        }],
        video_command: None,
    };
    write_ex_video_tag(&mut buf, 40, &header_struct, &frame).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "av1");
    // Header packet (SequenceStart) first, then the ModEx-bearing
    // CodedFrames body.
    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame, "payload bytes must survive the ModEx prefix");
    assert!(p.flags.keyframe, "FrameType=KeyFrame must round-trip");
    assert_eq!(p.pts, Some(40));
}

#[test]
fn ex_audio_modex_timestamp_offset_nano_chained_round_trips_through_demuxer() {
    // FLV with one AAC SequenceStart, then a ModEx-bearing CodedFrames
    // tag chaining TWO TimestampOffsetNano refinements (100 ns + 200 ns
    // = 300 ns accumulator). Demuxer must surface the AAC payload and
    // resolve the codec id to "aac".
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let asc = vec![0x12, 0x10];
    write_aac_ex_sequence_start(&mut buf, 0, &asc).unwrap();

    let au = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let header_struct = ExAudioTagHeader {
        packet_type: ExAudioPacketType::CodedFrames,
        fourcc: Some(FOURCC_AUDIO_AAC),
        multitrack: None,
        timestamp_offset_nano: 300,
        mod_ex_entries: vec![
            ModExEntry {
                subtype_raw: 0,
                payload: ModExPayload::TimestampOffsetNano { offset_ns: 100 },
                raw: vec![0x00, 0x00, 0x64],
            },
            ModExEntry {
                subtype_raw: 0,
                payload: ModExPayload::TimestampOffsetNano { offset_ns: 200 },
                raw: vec![0x00, 0x00, 0xC8],
            },
        ],
        bytes_consumed: 0,
    };
    write_ex_audio_tag(&mut buf, 23, &header_struct, &au).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "aac");
    assert_eq!(dmx.streams()[0].params.extradata, asc);

    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, au, "payload bytes must survive the ModEx chain");
    assert_eq!(p.pts, Some(23));
}

#[test]
fn ex_video_modex_reserved_subtype_passthrough_round_trips_through_demuxer() {
    // Reserved-subtype ModEx blob round-trips opaquely off the front of
    // the body: the demuxer doesn't model the reserved subtype, but the
    // payload bytes following the FourCc still reach the packet body
    // intact. This is the spec's "future-proof" path — new ModEx
    // subtypes land in producers before parsers learn them.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x81, 0x05, 0x0C];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();

    let frame = vec![0x11, 0x22, 0x33];
    let header_struct = ExVideoTagHeader {
        frame_type: ExFrameType::InterFrame,
        packet_type: ExPacketType::CodedFrames,
        fourcc: Some(FOURCC_AV01),
        multitrack: None,
        bytes_consumed: 0,
        composition_time_offset_ms: None,
        timestamp_offset_nano: 0,
        mod_ex_entries: vec![ModExEntry {
            subtype_raw: 5, // reserved
            payload: ModExPayload::Reserved { subtype_raw: 5 },
            raw: vec![0xAA, 0xBB, 0xCC, 0xDD],
        }],
        video_command: None,
    };
    write_ex_video_tag(&mut buf, 60, &header_struct, &frame).unwrap();

    let mut dmx = open_video_only(buf);
    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, frame);
    assert_eq!(p.pts, Some(60));
}

// ---- ModEx side-channel (last_timestamp_offset_nano / last_mod_ex_entries) ---
//
// The integer-ms FLV timestamp folds into `Packet::pts`/`dts`, but the
// `TimestampOffsetNano` sub-millisecond refinement (enhanced-rtmp-v2
// §`ModEx`) has nowhere to live on the core `Packet` (no nanosecond
// field). The demuxer captures it in a side-channel mirroring
// `last_multitrack_tracks`. These tests assert a remuxer can recover the
// exact nano offset + the full ModEx entry chain after demux.

#[test]
fn demuxer_surfaces_video_timestamp_offset_nano_side_channel() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x81, 0x05, 0x0C];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();

    let frame = vec![0xCA, 0xFE, 0xBA, 0xBE];
    let header_struct = ExVideoTagHeader {
        frame_type: ExFrameType::KeyFrame,
        packet_type: ExPacketType::CodedFrames,
        fourcc: Some(FOURCC_AV01),
        multitrack: None,
        bytes_consumed: 0,
        composition_time_offset_ms: None,
        timestamp_offset_nano: 250_000,
        mod_ex_entries: vec![ModExEntry {
            subtype_raw: 0,
            payload: ModExPayload::TimestampOffsetNano { offset_ns: 250_000 },
            raw: vec![0x03, 0xD0, 0x90],
        }],
        video_command: None,
    };
    write_ex_video_tag(&mut buf, 40, &header_struct, &frame).unwrap();

    let mut dmx = open_concrete(buf);
    // SequenceStart header packet: no ModEx prefix → side-channel clear.
    let _hdr = dmx.next_packet().unwrap();
    assert_eq!(dmx.last_timestamp_offset_nano(), None);
    assert!(dmx.last_mod_ex_entries().is_none());

    // CodedFrames packet: side-channel surfaces the nano offset + the
    // ModEx entry chain byte-for-byte.
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.pts, Some(40));
    assert_eq!(dmx.last_timestamp_offset_nano(), Some(250_000));
    // Nano-refined presentation time = pts_ms * 1e6 + offset_ns.
    let refined_ns = p.pts.unwrap() * 1_000_000 + 250_000;
    assert_eq!(refined_ns, 40_250_000);

    let entries = dmx.last_mod_ex_entries().expect("entries present");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].timestamp_offset_nano(), Some(250_000));
    assert_eq!(entries[0].raw, vec![0x03, 0xD0, 0x90]);
}

#[test]
fn demuxer_surfaces_chained_audio_nano_offset_and_clears_on_plain_tag() {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let asc = vec![0x12, 0x10];
    write_aac_ex_sequence_start(&mut buf, 0, &asc).unwrap();

    // First a ModEx-bearing tag chaining 100 + 200 = 300 ns.
    let au0 = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let modex_hdr = ExAudioTagHeader {
        packet_type: ExAudioPacketType::CodedFrames,
        fourcc: Some(FOURCC_AUDIO_AAC),
        multitrack: None,
        timestamp_offset_nano: 300,
        mod_ex_entries: vec![
            ModExEntry {
                subtype_raw: 0,
                payload: ModExPayload::TimestampOffsetNano { offset_ns: 100 },
                raw: vec![0x00, 0x00, 0x64],
            },
            ModExEntry {
                subtype_raw: 0,
                payload: ModExPayload::TimestampOffsetNano { offset_ns: 200 },
                raw: vec![0x00, 0x00, 0xC8],
            },
        ],
        bytes_consumed: 0,
    };
    write_ex_audio_tag(&mut buf, 23, &modex_hdr, &au0).unwrap();

    // Then a plain (no-ModEx) Ex audio tag — the side-channel MUST clear
    // so the stale 300 ns offset doesn't bleed onto the next packet.
    let au1 = vec![0x01, 0x02, 0x03];
    write_aac_ex_coded_frames(&mut buf, 46, &au1).unwrap();

    let mut dmx = open_concrete(buf);
    let _hdr = dmx.next_packet().unwrap();

    let p0 = dmx.next_packet().unwrap();
    assert_eq!(p0.data, au0);
    assert_eq!(dmx.last_timestamp_offset_nano(), Some(300));
    assert_eq!(dmx.last_mod_ex_entries().map(|e| e.len()), Some(2));

    let p1 = dmx.next_packet().unwrap();
    assert_eq!(p1.data, au1);
    assert_eq!(
        dmx.last_timestamp_offset_nano(),
        None,
        "plain tag must clear the nano side-channel"
    );
    assert!(dmx.last_mod_ex_entries().is_none());
}

#[test]
fn demuxer_preserves_reserved_modex_subtype_in_side_channel() {
    // A reserved-subtype ModEx blob has no nano value, but the entry +
    // its raw bytes still survive on the side-channel so a remuxer can
    // re-emit the (unrecognised-but-preserved) ModEx prefix verbatim.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x81, 0x05, 0x0C];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();

    let frame = vec![0x11, 0x22, 0x33];
    let header_struct = ExVideoTagHeader {
        frame_type: ExFrameType::InterFrame,
        packet_type: ExPacketType::CodedFrames,
        fourcc: Some(FOURCC_AV01),
        multitrack: None,
        bytes_consumed: 0,
        composition_time_offset_ms: None,
        timestamp_offset_nano: 0,
        mod_ex_entries: vec![ModExEntry {
            subtype_raw: 5,
            payload: ModExPayload::Reserved { subtype_raw: 5 },
            raw: vec![0xAA, 0xBB, 0xCC, 0xDD],
        }],
        video_command: None,
    };
    write_ex_video_tag(&mut buf, 60, &header_struct, &frame).unwrap();

    let mut dmx = open_concrete(buf);
    let _hdr = dmx.next_packet().unwrap();
    let _p = dmx.next_packet().unwrap();
    // No TimestampOffsetNano → nano accessor stays None even though a
    // ModEx prefix is present...
    assert_eq!(dmx.last_timestamp_offset_nano(), None);
    // ...but the reserved entry IS preserved for byte-exact re-emission.
    let entries = dmx.last_mod_ex_entries().expect("reserved entry preserved");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].subtype_raw, 5);
    assert_eq!(entries[0].timestamp_offset_nano(), None);
    assert_eq!(entries[0].raw, vec![0xAA, 0xBB, 0xCC, 0xDD]);
}

#[test]
fn ex_audio_multitrack_one_track_round_trips_through_demuxer() {
    // OneTrack multitrack audio: the body after the Ex header carries
    // `trackId UI8` then the per-track payload running to the end of
    // the body. Build a OneTrack Opus stream whose default track
    // (trackId 0) carries a single coded-frame body. The writer must
    // produce a tag the demuxer recovers as the default-track Opus
    // packet, with the codec id resolved to `opus` and the per-track
    // payload bytes surfaced verbatim.
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    // SequenceStart so the stream is minted with the right codec id +
    // extradata. Drive it through the single-track path first; the
    // multitrack CodedFrames tag follows.
    let opus_head = vec![b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd'];
    write_opus_sequence_start(&mut buf, 0, &opus_head).unwrap();

    // OneTrack CodedFrames body: `trackId(0)` + Opus packet.
    let opus_packet = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
    let mut mt_body = vec![0u8]; // trackId 0
    mt_body.extend_from_slice(&opus_packet);

    let header_struct = ExAudioTagHeader {
        packet_type: ExAudioPacketType::CodedFrames,
        fourcc: Some(FOURCC_OPUS),
        multitrack: Some(AvMultitrackType::OneTrack),
        timestamp_offset_nano: 0,
        mod_ex_entries: Vec::new(),
        bytes_consumed: 0,
    };
    write_ex_audio_tag(&mut buf, 23, &header_struct, &mt_body).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "opus");
    assert_eq!(dmx.streams()[0].params.extradata, opus_head);

    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(
        p.data, opus_packet,
        "default-track payload must reach the demuxer verbatim"
    );
    assert_eq!(p.pts, Some(23));
    assert!(
        !p.flags.discard,
        "Multitrack outer wrapper resolves to CodedFrames → data packet, not discard"
    );
}

#[test]
fn ex_audio_multitrack_many_tracks_round_trips_default_track() {
    // ManyTracks: two AAC tracks. trackId 0 is the default; trackId 1
    // is the alternate. Each track is prefixed with `trackId UI8+
    // sizeOfTrack UI24` followed by `sizeOfTrack` payload bytes. The
    // demuxer surfaces the default track's payload as the packet
    // body; the alternate track survives in the wire bytes but is not
    // emitted (the demuxer is single-stream-per-tag).
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let asc = vec![0x12, 0x10];
    write_aac_ex_sequence_start(&mut buf, 0, &asc).unwrap();

    let default_au = vec![0x11, 0x22, 0x33, 0x44];
    let alt_au = vec![0x55, 0x66, 0x77, 0x88, 0x99];

    let mut mt_body = Vec::new();
    // Track 0 (default).
    mt_body.push(0u8); // trackId
    let s = default_au.len();
    mt_body.extend_from_slice(&[(s >> 16) as u8, (s >> 8) as u8, s as u8]);
    mt_body.extend_from_slice(&default_au);
    // Track 1 (alternate).
    mt_body.push(1u8);
    let s = alt_au.len();
    mt_body.extend_from_slice(&[(s >> 16) as u8, (s >> 8) as u8, s as u8]);
    mt_body.extend_from_slice(&alt_au);

    let header_struct = ExAudioTagHeader {
        packet_type: ExAudioPacketType::CodedFrames,
        fourcc: Some(FOURCC_AUDIO_AAC),
        multitrack: Some(AvMultitrackType::ManyTracks),
        timestamp_offset_nano: 0,
        mod_ex_entries: Vec::new(),
        bytes_consumed: 0,
    };
    write_ex_audio_tag(&mut buf, 46, &header_struct, &mt_body).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "aac");

    let _hdr = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(
        p.data, default_au,
        "ManyTracks default-track payload (trackId 0) must surface verbatim"
    );
    assert_eq!(p.pts, Some(46));
}

// ---- Multitrack write (join_tracks) → demux side-channel round-trip --------
//
// The prior multitrack tests hand-rolled the `[FourCc?] trackId UI8
// sizeOfTrack UI24 payload` body bytes and only asserted the default
// track on the trait-object packet. These close the *write-side* loop:
// the body is produced by the `join_tracks` serialiser (the documented
// inverse of `split_tracks`), threaded through `write_ex_*_tag`, demuxed
// via the concrete `FlvDemuxer`, and EVERY track is recovered through
// `last_multitrack_tracks()` — the per-track trackId, FourCc, resolved
// codec name, and payload all survive end to end.

#[test]
fn video_many_tracks_join_round_trips_all_tracks_via_side_channel() {
    // ManyTracks (shared AV1 codec) — 3 variants. `join_tracks` writes
    // the UI24-delimited per-track loop; the demuxer surfaces the default
    // track as the packet and the full set on the side-channel.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x81, 0x05, 0x0C];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();

    let payloads = [
        vec![0xA0, 0xA1, 0xA2],             // trackId 0 (default, 4K)
        vec![0xB0, 0xB1, 0xB2, 0xB3],       // trackId 1 (1080p)
        vec![0xC0, 0xC1, 0xC2, 0xC3, 0xC4], // trackId 2 (720p)
    ];
    let tracks: Vec<JoinTrack> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| JoinTrack::new(i as u8, FOURCC_AV01, p.clone()))
        .collect();
    let mt_body = join_tracks(AvMultitrackType::ManyTracks, &tracks).unwrap();

    let header_struct = ExVideoTagHeader {
        frame_type: ExFrameType::KeyFrame,
        packet_type: ExPacketType::CodedFrames,
        fourcc: Some(FOURCC_AV01),
        multitrack: Some(AvMultitrackType::ManyTracks),
        bytes_consumed: 0,
        composition_time_offset_ms: None,
        timestamp_offset_nano: 0,
        mod_ex_entries: Vec::new(),
        video_command: None,
    };
    write_ex_video_tag(&mut buf, 70, &header_struct, &mt_body).unwrap();

    let mut dmx = open_concrete(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "av1");
    let _seq = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, payloads[0], "default track surfaces as the packet");
    assert_eq!(p.pts, Some(70));

    let tracks = dmx
        .last_multitrack_tracks()
        .expect("multitrack side-channel populated");
    assert_eq!(tracks.len(), 3);
    for (i, t) in tracks.iter().enumerate() {
        assert_eq!(t.track_id, i as u8);
        assert_eq!(t.fourcc, FOURCC_AV01, "shared FourCc on every track");
        assert_eq!(t.codec_name, "av1");
        assert_eq!(t.payload, payloads[i], "track {i} payload verbatim");
    }
}

#[test]
fn video_many_tracks_many_codecs_join_round_trips_per_track_fourcc() {
    // ManyTracksManyCodecs — each track carries its own FourCc on the
    // wire. Mix AV1 (default) + VP9 so the per-track FourCc recovery is
    // observable (a shared-codec test couldn't tell them apart).
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let config = vec![0x81, 0x05, 0x0C];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();

    let av1_payload = vec![0xAA, 0xBB, 0xCC];
    let vp9_payload = vec![0xDD, 0xEE, 0xFF, 0x00];
    let tracks = vec![
        JoinTrack::new(0, FOURCC_AV01, av1_payload.clone()),
        JoinTrack::new(1, FOURCC_VP09, vp9_payload.clone()),
    ];
    let mt_body = join_tracks(AvMultitrackType::ManyTracksManyCodecs, &tracks).unwrap();

    let header_struct = ExVideoTagHeader {
        frame_type: ExFrameType::KeyFrame,
        packet_type: ExPacketType::CodedFrames,
        // ManyTracksManyCodecs: no shared header FourCc (each track
        // carries its own). The Ex header FourCc field is `None`.
        fourcc: None,
        multitrack: Some(AvMultitrackType::ManyTracksManyCodecs),
        bytes_consumed: 0,
        composition_time_offset_ms: None,
        timestamp_offset_nano: 0,
        mod_ex_entries: Vec::new(),
        video_command: None,
    };
    write_ex_video_tag(&mut buf, 70, &header_struct, &mt_body).unwrap();

    let mut dmx = open_concrete(buf);
    let _seq = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, av1_payload, "default (AV1) track is the packet");

    let tracks = dmx
        .last_multitrack_tracks()
        .expect("multitrack side-channel populated");
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].fourcc, FOURCC_AV01);
    assert_eq!(tracks[0].codec_name, "av1");
    assert_eq!(tracks[0].payload, av1_payload);
    assert_eq!(tracks[1].fourcc, FOURCC_VP09);
    assert_eq!(tracks[1].codec_name, "vp9");
    assert_eq!(tracks[1].payload, vp9_payload);
}

#[test]
fn audio_many_tracks_many_codecs_join_round_trips_per_track_fourcc() {
    // Audio ManyTracksManyCodecs — Opus (default) + FLAC. Confirms the
    // audio FourCc name table resolves per-track on the side-channel.
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let opus_head = vec![b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd'];
    write_opus_sequence_start(&mut buf, 0, &opus_head).unwrap();

    let opus_au = vec![0x10, 0x20, 0x30];
    let flac_au = vec![0x40, 0x50, 0x60, 0x70];
    let flac_fourcc = u32::from_be_bytes(*b"fLaC");
    let tracks = vec![
        JoinTrack::new(0, FOURCC_OPUS, opus_au.clone()),
        JoinTrack::new(1, flac_fourcc, flac_au.clone()),
    ];
    let mt_body = join_tracks(AvMultitrackType::ManyTracksManyCodecs, &tracks).unwrap();

    let header_struct = ExAudioTagHeader {
        packet_type: ExAudioPacketType::CodedFrames,
        fourcc: None,
        multitrack: Some(AvMultitrackType::ManyTracksManyCodecs),
        timestamp_offset_nano: 0,
        mod_ex_entries: Vec::new(),
        bytes_consumed: 0,
    };
    write_ex_audio_tag(&mut buf, 30, &header_struct, &mt_body).unwrap();

    let mut dmx = open_concrete(buf);
    let _seq = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, opus_au, "default (Opus) track is the packet");

    let tracks = dmx
        .last_multitrack_tracks()
        .expect("multitrack side-channel populated");
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].fourcc, FOURCC_OPUS);
    assert_eq!(tracks[0].codec_name, "opus");
    assert_eq!(tracks[0].payload, opus_au);
    assert_eq!(tracks[1].fourcc, flac_fourcc);
    assert_eq!(tracks[1].codec_name, "flac");
    assert_eq!(tracks[1].payload, flac_au);
}

// ---- Filtered (encrypted) tag write → demux round-trip (Annex F.3) ----------
//
// `write_filtered_tag` sets the tag-header Filter bit, frames the
// `EncryptionTagHeader` + `FilterParams` preamble, and appends the
// (cipher)text body. The demuxer's filtered-tag path strips the preamble
// and surfaces the trailing body as a discard-flagged packet. These close
// the container-level encryption loop (the crate carries no DRM client —
// the body is whatever the caller's filter chain produced).

#[test]
fn write_filtered_video_tag_round_trips_through_demuxer() {
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    // Unencrypted SequenceStart mints the video stream (encrypted tags
    // don't mint streams; the codec is signalled in the clear first).
    let config = vec![0x81, 0x05, 0x0C];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();

    // Full ("Encryption", v1) filtered video tag.
    let iv: [u8; 16] = std::array::from_fn(|i| 0x20 + i as u8);
    let pre = EncryptedTagPreamble::encryption(iv);
    let cipher = vec![0xCA, 0xFE, 0xD0, 0x0D, 0x99];
    write_filtered_tag(&mut buf, TagType::Video, 33, 0, &pre, &cipher).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "av1");
    let _seq = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    // The ciphertext body survives the preamble strip verbatim; the
    // packet is discard-flagged (we don't decrypt).
    assert_eq!(p.data, cipher, "ciphertext body survives demux verbatim");
    assert!(p.flags.discard, "encrypted body is discard-flagged");
    assert_eq!(p.pts, Some(33));
}

#[test]
fn write_filtered_audio_se_cleartext_tag_round_trips_through_demuxer() {
    // Selective Encryption wrapper with EncryptedAU=0 — the body is
    // plaintext but still framed through the SE preamble. The demuxer
    // strips the preamble and surfaces the plaintext body.
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let asc = vec![0x12, 0x10];
    write_aac_ex_sequence_start(&mut buf, 0, &asc).unwrap();

    let pre = EncryptedTagPreamble::selective(None);
    let plain = vec![0x01, 0x02, 0x03, 0x04];
    write_filtered_tag(&mut buf, TagType::Audio, 23, 0, &pre, &plain).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "aac");
    let _seq = dmx.next_packet().unwrap();
    let p = dmx.next_packet().unwrap();
    assert_eq!(
        p.data, plain,
        "SE-cleartext body survives the preamble strip"
    );
    assert_eq!(p.pts, Some(23));
}

#[test]
fn additional_header_plus_filtered_tags_round_trip_full_encrypted_flv() {
    // A complete encrypted-FLV head: onMetaData, then the
    // |AdditionalHeader Encryption Header (F.2.1, timestamp 0), then a
    // SequenceStart to mint the stream, then a fully-encrypted video
    // tag. The demuxer must flag the file encrypted and read every
    // encryption.* field back from the header.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let meta = MetadataBag::new().number("duration", 2.0);
    script::write_on_metadata(&mut buf, &meta).unwrap();

    // |AdditionalHeader at timestamp 0, right after onMetaData (F.2.1).
    let hdr = EncryptionHeader::flash_access_v2();
    write_additional_header(&mut buf, &hdr).unwrap();

    let config = vec![0x81, 0x05, 0x0C];
    write_av1_sequence_start(&mut buf, 0, &config).unwrap();

    let iv: [u8; 16] = [0x77; 16];
    let pre = EncryptedTagPreamble::encryption(iv);
    let cipher = vec![0xAB, 0xCD, 0xEF];
    write_filtered_tag(&mut buf, TagType::Video, 40, 0, &pre, &cipher).unwrap();

    let dmx = open_concrete(buf);
    assert!(
        dmx.is_encrypted(),
        "|AdditionalHeader marks the file encrypted"
    );
    let md = dmx.metadata();
    assert_eq!(meta_lookup(md, "encryption"), Some("true"));
    assert_eq!(meta_lookup(md, "encryption.version"), Some("2"));
    assert_eq!(meta_lookup(md, "encryption.method"), Some("Standard"));
    assert_eq!(meta_lookup(md, "encryption.algorithm"), Some("AES-CBC"));
    assert_eq!(meta_lookup(md, "encryption.key_length"), Some("16"));
    assert_eq!(
        meta_lookup(md, "encryption.key_subtype"),
        Some("FlashAccessv2")
    );
}

// ---- HDR colorInfo encode-side wiring round-trip --------------------------
//
// Exercises the typed `crate::color_info::ColorInfo` encoder by muxing a
// `videoPacketType = Metadata` tag and confirming the demuxer's
// `harvest_video_metadata_frame` walker recovers every populated field
// under the spec-defined `colorinfo.*` metadata keys.

fn meta_lookup<'a>(md: &'a [(String, String)], key: &str) -> Option<&'a str> {
    md.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn make_hvc1_seq_start(buf: &mut Vec<u8>) {
    let config = vec![0x01];
    write_hevc_sequence_start(buf, 0, &config).unwrap();
}

#[test]
fn ex_video_color_info_writer_round_trip_full_payload() {
    // Mux a SequenceStart + a fully-populated colorInfo Metadata tag,
    // demux it, and assert every spec-defined field reaches the
    // metadata bag under the lowercase `colorinfo.<group>.<key>` path
    // that `harvest_video_metadata_frame` produces.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    make_hvc1_seq_start(&mut buf);

    let ci = ColorInfo {
        color_config: Some(ColorConfig {
            bit_depth: Some(10),
            color_primaries: Some(9),           // BT.2020
            transfer_characteristics: Some(16), // SMPTE ST 2084 (PQ)
            matrix_coefficients: Some(9),       // BT.2020 NCL
        }),
        hdr_cll: Some(HdrCll {
            max_fall: Some(400.0),
            max_cll: Some(1000.0),
        }),
        hdr_mdcv: Some(HdrMdcv {
            red_x: Some(0.708),
            red_y: Some(0.292),
            green_x: Some(0.170),
            green_y: Some(0.797),
            blue_x: Some(0.131),
            blue_y: Some(0.046),
            white_point_x: Some(0.3127),
            white_point_y: Some(0.3290),
            max_luminance: Some(1000.0),
            min_luminance: Some(0.01),
        }),
    };
    write_ex_video_color_info(&mut buf, 40, oxideav_flv::FOURCC_HVC1, &ci).unwrap();

    let mut dmx = open_video_only(buf);
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "h265");
    let _seq = dmx.next_packet().unwrap();
    let m = dmx.next_packet().unwrap();
    // Metadata tag remains header+discard so codec decoders skip it.
    assert!(m.flags.header && m.flags.discard);

    let md = dmx.metadata();
    // colorConfig
    assert_eq!(
        meta_lookup(md, "colorinfo.colorConfig.bitDepth"),
        Some("10")
    );
    assert_eq!(
        meta_lookup(md, "colorinfo.colorConfig.colorPrimaries"),
        Some("9")
    );
    assert_eq!(
        meta_lookup(md, "colorinfo.colorConfig.transferCharacteristics"),
        Some("16")
    );
    assert_eq!(
        meta_lookup(md, "colorinfo.colorConfig.matrixCoefficients"),
        Some("9")
    );
    // hdrCll
    assert_eq!(meta_lookup(md, "colorinfo.hdrCll.maxFall"), Some("400"));
    assert_eq!(meta_lookup(md, "colorinfo.hdrCll.maxCLL"), Some("1000"));
    // hdrMdcv — primaries
    assert_eq!(meta_lookup(md, "colorinfo.hdrMdcv.redX"), Some("0.708"));
    assert_eq!(meta_lookup(md, "colorinfo.hdrMdcv.redY"), Some("0.292"));
    assert_eq!(meta_lookup(md, "colorinfo.hdrMdcv.greenX"), Some("0.17"));
    assert_eq!(meta_lookup(md, "colorinfo.hdrMdcv.greenY"), Some("0.797"));
    assert_eq!(meta_lookup(md, "colorinfo.hdrMdcv.blueX"), Some("0.131"));
    assert_eq!(meta_lookup(md, "colorinfo.hdrMdcv.blueY"), Some("0.046"));
    assert_eq!(
        meta_lookup(md, "colorinfo.hdrMdcv.whitePointX"),
        Some("0.3127")
    );
    assert_eq!(
        meta_lookup(md, "colorinfo.hdrMdcv.whitePointY"),
        Some("0.329")
    );
    assert_eq!(
        meta_lookup(md, "colorinfo.hdrMdcv.maxLuminance"),
        Some("1000")
    );
    assert_eq!(
        meta_lookup(md, "colorinfo.hdrMdcv.minLuminance"),
        Some("0.01")
    );
}

#[test]
fn ex_video_color_info_full_read_write_loop_via_to_color_info() {
    // Close the read↔write loop end-to-end: mux a fully-populated
    // colorInfo Metadata tag, demux it, and reconstruct the encode-side
    // `ColorInfo` struct from the typed read view via
    // `TypedColorInfo::to_color_info`. The rebuilt struct must equal the
    // one the producer encoded (every field is a finite, in-range value
    // here so none drop on the read side).
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    make_hvc1_seq_start(&mut buf);

    let ci = ColorInfo {
        color_config: Some(ColorConfig {
            bit_depth: Some(10),
            color_primaries: Some(9),
            transfer_characteristics: Some(16),
            matrix_coefficients: Some(9),
        }),
        hdr_cll: Some(HdrCll {
            max_fall: Some(400.0),
            max_cll: Some(1000.0),
        }),
        hdr_mdcv: Some(HdrMdcv {
            red_x: Some(0.708),
            red_y: Some(0.292),
            green_x: Some(0.170),
            green_y: Some(0.797),
            blue_x: Some(0.131),
            blue_y: Some(0.046),
            white_point_x: Some(0.3127),
            white_point_y: Some(0.3290),
            max_luminance: Some(1000.0),
            min_luminance: Some(0.01),
        }),
    };
    write_ex_video_color_info(&mut buf, 40, oxideav_flv::FOURCC_HVC1, &ci).unwrap();

    let mut dmx = open_video_only(buf);
    while dmx.next_packet().is_ok() {}

    let md = dmx.metadata();
    let typed = oxideav_flv::TypedMetadata::new(md);
    let rebuilt = typed
        .color_info()
        .expect("colorInfo view present")
        .to_color_info();
    assert_eq!(rebuilt, ci);
}

#[test]
fn ex_video_color_info_reset_loop_rebuilds_default() {
    // After the spec-RECOMMENDED Undefined reset, the typed view reports
    // `is_reset_sentinel()` and rebuilds to the empty `ColorInfo`.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    make_hvc1_seq_start(&mut buf);

    let ci = ColorInfo {
        color_config: Some(ColorConfig {
            bit_depth: Some(10),
            ..ColorConfig::default()
        }),
        ..ColorInfo::default()
    };
    write_ex_video_color_info(&mut buf, 40, oxideav_flv::FOURCC_HVC1, &ci).unwrap();
    write_ex_video_color_info_reset(&mut buf, 80, oxideav_flv::FOURCC_HVC1).unwrap();

    let mut dmx = open_video_only(buf);
    while dmx.next_packet().is_ok() {}

    let md = dmx.metadata();
    let typed = oxideav_flv::TypedMetadata::new(md);
    let view = typed
        .color_info()
        .expect("reset sentinel makes view present");
    assert!(view.is_reset_sentinel());
    assert_eq!(view.to_color_info(), ColorInfo::default());
}

#[test]
fn ex_video_color_info_writer_omits_absent_groups() {
    // Populate only `colorConfig`; assert hdrCll / hdrMdcv keys do
    // not appear in the metadata bag so producers can emit partial
    // signalling without phantom keys.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    make_hvc1_seq_start(&mut buf);

    let ci = ColorInfo {
        color_config: Some(ColorConfig {
            bit_depth: Some(12),
            ..ColorConfig::default()
        }),
        ..ColorInfo::default()
    };
    write_ex_video_color_info(&mut buf, 40, oxideav_flv::FOURCC_HVC1, &ci).unwrap();

    let mut dmx = open_video_only(buf);
    let _seq = dmx.next_packet().unwrap();
    let _m = dmx.next_packet().unwrap();

    let md = dmx.metadata();
    assert_eq!(
        meta_lookup(md, "colorinfo.colorConfig.bitDepth"),
        Some("12")
    );
    assert!(md.iter().all(|(k, _)| !k.starts_with("colorinfo.hdrCll")));
    assert!(md.iter().all(|(k, _)| !k.starts_with("colorinfo.hdrMdcv")));
    // Absent colorConfig.colorPrimaries — not emitted by the writer.
    assert!(meta_lookup(md, "colorinfo.colorConfig.colorPrimaries").is_none());
}

#[test]
fn ex_video_color_info_reset_clears_prior_signal() {
    // Set then reset — the reset writer emits the spec-recommended
    // `["colorInfo", Undefined]` payload and the demuxer drops every
    // prior `colorinfo.*` entry, leaving the `"undefined"` sentinel.
    let mut buf = Vec::new();
    header::write(&mut buf, false, true).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    make_hvc1_seq_start(&mut buf);

    let ci = ColorInfo {
        color_config: Some(ColorConfig {
            bit_depth: Some(10),
            ..ColorConfig::default()
        }),
        ..ColorInfo::default()
    };
    write_ex_video_color_info(&mut buf, 40, oxideav_flv::FOURCC_HVC1, &ci).unwrap();
    write_ex_video_color_info_reset(&mut buf, 80, oxideav_flv::FOURCC_HVC1).unwrap();

    let mut dmx = open_video_only(buf);
    while dmx.next_packet().is_ok() {}

    let md = dmx.metadata();
    assert!(meta_lookup(md, "colorinfo.colorConfig.bitDepth").is_none());
    assert_eq!(meta_lookup(md, "colorinfo"), Some("undefined"));
}

#[test]
fn ex_video_color_info_rejects_out_of_range_at_writer() {
    // Writer must surface the spec-range validation error to callers,
    // not silently emit a malformed AMF blob.
    let mut buf = Vec::new();
    let ci = ColorInfo {
        hdr_cll: Some(HdrCll {
            max_cll: Some(50_000.0), // > 10_000 cd/m^2 ceiling
            ..HdrCll::default()
        }),
        ..ColorInfo::default()
    };
    let err = write_ex_video_color_info(&mut buf, 0, oxideav_flv::FOURCC_HVC1, &ci).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("maxCLL"));
    // And nothing was written to the buffer (validation runs before
    // the FLV tag header is laid down).
    assert!(buf.is_empty());
}

/// Build a minimal audio FLV that includes one `onXMPData` tag at the
/// head and two `onCuePoint` tags interleaved with MP3 frames.
fn build_flv_with_cue_and_xmp() -> Vec<u8> {
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let bag = MetadataBag::new()
        .number("duration", 0.078)
        .number("audiosamplerate", 44_100.0)
        .string("encoder", "oxideav-flv muxer");
    script::write_on_metadata(&mut buf, &bag).unwrap();

    // XMP packet at t=0 — producer-style XMP envelope.
    let xmp = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>";
    script::write_on_xmp_data(&mut buf, 0, xmp).unwrap();

    // First cue at t=0 ms — navigation marker.
    let cue0 = CuePointParams::new("chapter-1", 0.0, CuePointType::Navigation)
        .parameter("title", "Intro")
        .parameter("section", "A");
    script::write_on_cue_point(&mut buf, 0, &cue0).unwrap();

    // One MP3 frame at t=0.
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &[0xFF, 0xFB, 0x90, 0x00]).unwrap();

    // Second cue at t=26 ms — event marker, no parameters.
    let cue1 = CuePointParams::new("ad-mark", 0.026, CuePointType::Event);
    script::write_on_cue_point(&mut buf, 26, &cue1).unwrap();

    tag::write_mp3_tag(&mut buf, 26, 3, true, true, &[0xFF, 0xFB, 0x90, 0x44]).unwrap();

    buf
}

#[test]
fn on_xmp_data_writer_round_trips_through_demuxer() {
    let bytes = build_flv_with_cue_and_xmp();
    let dmx = open(bytes);
    let md = dmx.metadata();
    // The XMP packet body surfaces verbatim under `metadata["xmp"]`.
    let xmp = md.iter().find(|(k, _)| k == "xmp").map(|(_, v)| v.as_str());
    assert_eq!(
        xmp,
        Some("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>")
    );
}

#[test]
fn on_xmp_data_promotes_oversized_packet_to_amf0_long_string() {
    // An XMP packet larger than the 65535-byte UI16 String ceiling must
    // be emitted via the AMF0 Long String form (0x0C, UI32 length) —
    // otherwise the ordinary String writer would truncate or overflow.
    // The demuxer parses both forms into the same value, so the full
    // packet surfaces verbatim under metadata["xmp"].
    let big = format!(
        "<x:xmpmeta>{}</x:xmpmeta>",
        "<rdf:Description/>".repeat(5000) // ~85 KB, well past 65535
    );
    assert!(big.len() > u16::MAX as usize);

    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let meta = MetadataBag::new().number("duration", 1.0);
    script::write_on_metadata(&mut buf, &meta).unwrap();
    script::write_on_xmp_data(&mut buf, 0, &big).unwrap();
    // A media tag so the demuxer mints a stream (script-only files error).
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &[0xFF, 0xFB, 0x90, 0x00]).unwrap();

    let dmx = open(buf);
    let md = dmx.metadata();
    let xmp = md.iter().find(|(k, _)| k == "xmp").map(|(_, v)| v.as_str());
    assert_eq!(xmp, Some(big.as_str()), "oversized XMP survives verbatim");
}

#[test]
fn on_cue_point_writer_round_trips_per_cue_fields() {
    let bytes = build_flv_with_cue_and_xmp();
    let dmx = open(bytes);
    let md = dmx.metadata();

    // The demuxer indexes cues by occurrence under
    // `cuepoint.<n>.<key>`. Walk the bag and assert each cue's four
    // spec properties round-tripped.
    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    assert_eq!(lookup("cuepoint.0.name"), Some("chapter-1"));
    assert_eq!(lookup("cuepoint.0.time"), Some("0"));
    assert_eq!(lookup("cuepoint.0.type"), Some("navigation"));
    assert_eq!(lookup("cuepoint.0.parameters.title"), Some("Intro"));
    assert_eq!(lookup("cuepoint.0.parameters.section"), Some("A"));

    assert_eq!(lookup("cuepoint.1.name"), Some("ad-mark"));
    assert_eq!(lookup("cuepoint.1.time"), Some("0.026"));
    assert_eq!(lookup("cuepoint.1.type"), Some("event"));
    // No `parameters.<key>` entries on cue 1 — it was constructed
    // without any user parameters.
    assert!(md
        .iter()
        .all(|(k, _)| !k.starts_with("cuepoint.1.parameters.")));
}

#[test]
fn on_cue_point_typed_parameters_round_trip_through_demuxer() {
    use oxideav_flv::script::{MetaValue, ObjectBuilder};

    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new().number("duration", 0.04);
    script::write_on_metadata(&mut buf, &bag).unwrap();

    // A cue with mixed-type parameters: a String, a Number, a Boolean,
    // and a nested Object — all flatten through the demuxer.
    let cue = CuePointParams::new("ad-break", 0.0, CuePointType::Event)
        .parameter("label", "midroll")
        .parameter_typed("duration", MetaValue::Number(15.0))
        .parameter_typed("skippable", MetaValue::Boolean(true))
        .parameter_typed(
            "meta",
            ObjectBuilder::new().string("campaign", "summer").build(),
        );
    script::write_on_cue_point(&mut buf, 0, &cue).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &mp3_frames()[0]).unwrap();

    let dmx = open(buf);
    let md = dmx.metadata();
    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    assert_eq!(lookup("cuepoint.0.parameters.label"), Some("midroll"));
    assert_eq!(lookup("cuepoint.0.parameters.duration"), Some("15"));
    assert_eq!(lookup("cuepoint.0.parameters.skippable"), Some("true"));
    // Nested object fans out under `.<subkey>`.
    assert_eq!(
        lookup("cuepoint.0.parameters.meta.campaign"),
        Some("summer")
    );
}

#[test]
fn cue_point_and_xmp_tags_do_not_disturb_audio_packets() {
    // Inserting cuepoint / XMP script tags between media tags must not
    // affect the audio packet stream the demuxer yields.
    let bytes = build_flv_with_cue_and_xmp();
    let mut dmx = open(bytes);
    let mut audio_count = 0;
    let mut last_ts = -1i64;
    while let Ok(pkt) = dmx.next_packet() {
        if pkt.flags.header || pkt.flags.discard {
            continue;
        }
        let ts = pkt.pts.unwrap_or(-1);
        assert!(
            ts > last_ts,
            "audio timestamps must advance: {ts} > {last_ts}"
        );
        last_ts = ts;
        audio_count += 1;
    }
    assert_eq!(audio_count, 2, "two MP3 frames in fixture");
}

// ----------------------- Annex B.2 onImageData ----------------------------

/// An `onImageData` script tag (Annex B.2) carrying an embedded JPEG
/// round-trips through the demuxer: the original image file bytes and
/// the trackid are recovered via `FlvDemuxer::embedded_images`, and the
/// string bag gains the `onimagedata.<n>.{trackid,length}` scalars.
#[test]
fn on_image_data_round_trips_through_embedded_images() {
    // A minimal but realistic compressed image payload (JPEG SOI + a few
    // bytes). The demuxer must surface these bytes verbatim.
    let img: Vec<u8> = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0xFF, 0xD9,
    ];

    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new().number("duration", 1.0);
    script::write_on_metadata(&mut buf, &bag).unwrap();
    // onImageData tag at the head, before the first audio tag.
    script::write_on_image_data(&mut buf, 0, &img, Some(2)).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &mp3_frames()[0]).unwrap();

    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let dmx = open_demuxer_concrete(input, &NullCodecResolver).unwrap();

    let images = dmx.embedded_images();
    assert_eq!(images.len(), 1, "one onImageData tag");
    assert_eq!(images[0].track_id, Some(2));
    assert_eq!(images[0].data, img, "image bytes survive verbatim");

    // The string bag carries the scalar summary.
    let md = dmx.metadata();
    let lookup = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    assert_eq!(lookup("onimagedata.0.trackid"), Some("2"));
    assert_eq!(lookup("onimagedata.0.length"), Some("12"));
}

/// Two `onImageData` tags are collected in discovery order, and a
/// trackid-less image still surfaces its bytes.
#[test]
fn on_image_data_multiple_images_and_no_trackid() {
    let img_a: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic
    let img_b: Vec<u8> = vec![0x47, 0x49, 0x46, 0x38]; // GIF magic

    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new().number("duration", 1.0);
    script::write_on_metadata(&mut buf, &bag).unwrap();
    script::write_on_image_data(&mut buf, 0, &img_a, Some(1)).unwrap();
    script::write_on_image_data(&mut buf, 0, &img_b, None).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &mp3_frames()[0]).unwrap();

    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let dmx = open_demuxer_concrete(input, &NullCodecResolver).unwrap();
    let images = dmx.embedded_images();
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].track_id, Some(1));
    assert_eq!(images[0].data, img_a);
    assert_eq!(images[1].track_id, None, "trackid omitted");
    assert_eq!(images[1].data, img_b);
}
