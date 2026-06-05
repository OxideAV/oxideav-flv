//! End-to-end test for [`oxideav_flv::TypedMetadata`].
//!
//! Round-trips the Annex E.5 fifteen well-known `onMetaData` properties
//! through the muxer + demuxer pair and asserts every typed accessor
//! reads back the value the producer stamped. Specifically: the
//! [`oxideav_flv::script::MetadataBag`] (write side) and
//! [`oxideav_flv::FlvDemuxer`] (read side) preserve the
//! Number / Boolean / String shape declared by Annex E.5 across the
//! string-bag flatten in between, so the typed accessor sees the
//! original AMF type even though the bag is `Vec<(String, String)>`.

use std::io::Cursor;

use oxideav_core::{NullCodecResolver, ReadSeek};
use oxideav_flv::{
    header, open_demuxer,
    script::{self, MetadataBag},
    tag, TypedMetadata,
};

/// Build a minimal audio-only FLV (header + onMetaData + one MP3 tag)
/// whose `onMetaData` carries the full Annex E.5 fifteen-property set
/// plus the `videocodecid` Number (legacy H.264 id 7) so the
/// string-codec helper can be verified through the typed view as well.
fn build_typed_meta_flv() -> Vec<u8> {
    let mut buf = Vec::new();
    header::write(&mut buf, true, true).unwrap(); // has both
    tag::write_first_previous_tag_size(&mut buf).unwrap();

    let bag = MetadataBag::new()
        // Number properties.
        .number("duration", 12.5)
        .number("filesize", 12_345_678.0)
        .number("width", 1920.0)
        .number("height", 1080.0)
        .number("framerate", 29.97)
        .number("videodatarate", 2500.0)
        .number("audiodatarate", 192.0)
        .number("audiosamplerate", 48_000.0)
        .number("audiosamplesize", 16.0)
        .number("audiodelay", 0.038)
        .number("videocodecid", 7.0)
        .number("audiocodecid", 10.0)
        // Boolean properties.
        .boolean("stereo", true)
        .boolean("canSeekToEnd", true)
        // String property.
        .string("creationdate", "Wed, 01 Jan 2025 00:00:00 GMT");
    script::write_on_metadata(&mut buf, &bag).unwrap();

    // One tiny MP3 tag so the demuxer's stream-discovery has
    // something to do — TypedMetadata depends on the script tag
    // landing in the bag, which happens during `open` regardless of
    // whether any media tags follow.
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &[0xFF, 0xFB, 0x00, 0x00]).unwrap();
    buf
}

#[test]
fn typed_metadata_reads_back_all_e5_properties() {
    let bytes = build_typed_meta_flv();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = open_demuxer(input, &NullCodecResolver).unwrap();

    // The opaque `Demuxer` trait carries the bag through
    // `Demuxer::metadata`; [`TypedMetadata::new`] wraps it without
    // copying.
    let typed: TypedMetadata<'_> = TypedMetadata::new(dmx.metadata());

    // Numbers.
    assert_eq!(typed.duration(), Some(12.5));
    assert_eq!(typed.filesize(), Some(12_345_678));
    assert_eq!(typed.width(), Some(1920));
    assert_eq!(typed.height(), Some(1080));
    assert_eq!(typed.framerate(), Some(29.97));
    assert_eq!(typed.video_data_rate_kbps(), Some(2500.0));
    assert_eq!(typed.audio_data_rate_kbps(), Some(192.0));
    assert_eq!(typed.audio_sample_rate(), Some(48_000.0));
    assert_eq!(typed.audio_sample_size(), Some(16));
    assert_eq!(typed.audio_delay_seconds(), Some(0.038));
    assert_eq!(typed.video_codec_id(), Some(7));
    assert_eq!(typed.audio_codec_id(), Some(10));
    assert_eq!(typed.video_codec_id_str().as_deref(), Some("h264"));
    assert_eq!(typed.audio_codec_id_str().as_deref(), Some("aac"));

    // Booleans.
    assert_eq!(typed.stereo(), Some(true));
    assert_eq!(typed.can_seek_to_end(), Some(true));

    // String.
    assert_eq!(typed.creationdate(), Some("Wed, 01 Jan 2025 00:00:00 GMT"));
    // No Date carrier when the bag holds a free-form string.
    assert_eq!(typed.creationdate_as_date(), None);
}

#[test]
fn typed_metadata_absent_properties_return_none() {
    // A bag with only duration set — every other typed accessor
    // returns None.
    let mut buf = Vec::new();
    header::write(&mut buf, true, false).unwrap();
    tag::write_first_previous_tag_size(&mut buf).unwrap();
    let bag = MetadataBag::new().number("duration", 1.0);
    script::write_on_metadata(&mut buf, &bag).unwrap();
    tag::write_mp3_tag(&mut buf, 0, 3, true, true, &[0xFF, 0xFB, 0x00, 0x00]).unwrap();

    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(buf));
    let dmx = open_demuxer(input, &NullCodecResolver).unwrap();
    let typed = TypedMetadata::new(dmx.metadata());

    assert_eq!(typed.duration(), Some(1.0));
    assert_eq!(typed.filesize(), None);
    assert_eq!(typed.width(), None);
    assert_eq!(typed.height(), None);
    assert_eq!(typed.framerate(), None);
    assert_eq!(typed.video_data_rate_kbps(), None);
    assert_eq!(typed.audio_data_rate_kbps(), None);
    assert_eq!(typed.audio_sample_rate(), None);
    assert_eq!(typed.audio_sample_size(), None);
    assert_eq!(typed.audio_delay_seconds(), None);
    assert_eq!(typed.video_codec_id(), None);
    assert_eq!(typed.audio_codec_id(), None);
    assert_eq!(typed.video_codec_id_str(), None);
    assert_eq!(typed.audio_codec_id_str(), None);
    assert_eq!(typed.stereo(), None);
    assert_eq!(typed.can_seek_to_end(), None);
    assert_eq!(typed.creationdate(), None);
    assert_eq!(typed.creationdate_as_date(), None);
}
