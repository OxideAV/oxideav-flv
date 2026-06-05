//! Pure-Rust Flash Video (FLV) container demuxer.
//!
//! Reference: Adobe *Video File Format Specification* version 10
//! (2008). The crate parses the 9-byte FLV header and the `FLV tag`
//! stream that follows, and emits one [`oxideav_core::Packet`] per
//! media tag. Script tags (AMF0 `onMetaData`) are parsed for container
//! metadata (duration, width, height, codec ids) and consumed
//! internally.
//!
//! See the crate README for the supported codec map. A muxer slice
//! is also provided: [`header::write`] emits the file header,
//! [`tag::write_tag`] / [`tag::write_mp3_tag`] / [`tag::write_aac_raw_tag`]
//! write audio tags, [`tag::write_h263_tag`] / [`tag::write_vp6_tag`] /
//! [`tag::write_vp6a_tag`] / [`tag::write_avc_sequence_header`] /
//! [`tag::write_avc_nalu_tag`] write the legacy video tags, and
//! [`script::write_on_metadata`] serialises an `onMetaData` script tag
//! from a [`script::MetadataBag`]. The output round-trips bit-exactly
//! through [`FlvDemuxer`].

#![deny(missing_debug_implementations)]

pub mod amf0;
pub mod amf3;
pub mod color_info;
pub mod demuxer;
pub mod ex_audio;
pub mod ex_video;
pub mod header;
pub mod mod_ex;
pub mod multitrack;
pub mod script;
pub mod tag;
pub mod typed_meta;

use oxideav_core::ContainerRegistry;

pub use amf0::{parse_amf0_value, AmfValue};
pub use amf3::{parse_amf3_value, Amf3Array, Amf3Object, Amf3Value};
pub use color_info::{ColorConfig, ColorInfo, HdrCll, HdrMdcv};
pub use demuxer::{open as open_demuxer, FlvDemuxer};
pub use ex_audio::{
    fourcc_audio_codec_id_str, AudioPacketModExType, AvMultitrackType, ExAudioPacketType,
    ExAudioTagHeader, FOURCC_AAC as FOURCC_AUDIO_AAC, FOURCC_AC3, FOURCC_EAC3, FOURCC_FLAC,
    FOURCC_MP3 as FOURCC_AUDIO_MP3, FOURCC_OPUS, SOUND_FORMAT_EX_HEADER,
};
pub use ex_video::{
    fourcc_codec_id_str, ExFrameType, ExPacketType, ExVideoTagHeader, VideoCommand,
    VideoPacketModExType, EX_HEADER_FLAG, FOURCC_AV01, FOURCC_AVC1, FOURCC_HVC1, FOURCC_VP08,
    FOURCC_VP09, FOURCC_VVC1,
};
pub use header::{FlvHeader, FLV_SIGNATURE};
pub use mod_ex::{ModExEntry, ModExPayload};
pub use multitrack::{split_tracks, MultitrackTrack};
pub use script::{write_on_metadata, write_on_metadata_body, MetaValue, MetadataBag};
pub use tag::{
    audio_codec_id_str, video_codec_id_str, write_aac_ex_coded_frames, write_aac_ex_sequence_start,
    write_aac_raw_tag, write_ac3_coded_frames, write_audio_tag, write_av1_coded_frames,
    write_av1_sequence_start, write_avc_end_of_sequence, write_avc_nalu_tag,
    write_avc_sequence_header, write_eac3_coded_frames, write_ex_audio_sequence_end,
    write_ex_audio_tag, write_ex_video_color_info, write_ex_video_color_info_reset,
    write_ex_video_metadata, write_ex_video_sequence_end, write_ex_video_tag,
    write_first_previous_tag_size, write_flac_coded_frames, write_flac_sequence_start,
    write_h263_tag, write_hevc_coded_frames, write_hevc_coded_frames_x, write_hevc_sequence_start,
    write_mp3_ex_coded_frames, write_mp3_tag, write_opus_coded_frames, write_opus_sequence_start,
    write_tag, write_video_info_command_tag, write_video_tag, write_vp6_tag, write_vp6a_tag,
    write_vp9_coded_frames, write_vp9_sequence_start, write_vvc_coded_frames,
    write_vvc_sequence_start, AudioTagHeader, EncryptedTagPreamble, FrameType, TagHeader, TagType,
    VideoInfoCommand, VideoTagHeader, AUDIO_CODEC_AAC, AUDIO_CODEC_MP3, AUDIO_CODEC_MP3_8K,
    VIDEO_CODEC_FLV1, VIDEO_CODEC_H264, VIDEO_CODEC_VP6A, VIDEO_CODEC_VP6F,
};
pub use typed_meta::TypedMetadata;

/// Register the demuxer, its probe, and the `.flv` extension with a
/// [`ContainerRegistry`].
pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("flv", demuxer::open);
    reg.register_extension("flv", "flv");
    reg.register_probe("flv", probe);
}

/// Install the FLV container into a [`oxideav_core::RuntimeContext`].
///
/// Convenience wrapper around [`register_containers`] that matches the
/// uniform `register(&mut RuntimeContext)` entry point every sibling
/// crate exposes.
///
/// Also wired into [`oxideav_meta::register_all`] via the
/// [`oxideav_core::register!`] macro below.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("flv", register);

/// Content probe — returns `100` for a well-formed FLV signature, else
/// `0`. The four-byte magic is `FLV\x01` followed by a flags byte and a
/// 4-byte big-endian `DataOffset` of 9. The check is deliberately tight
/// so we don't grab random files that happen to start with `FL`.
fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() < 9 {
        return 0;
    }
    if &p.buf[0..3] != FLV_SIGNATURE {
        return 0;
    }
    // Version byte must be 1 (all spec-defined versions so far).
    if p.buf[3] != 1 {
        return 0;
    }
    // DataOffset is a 4-byte BE field at offset 5. The spec requires it to
    // equal 9 for FLV 1; anything else is a forgery.
    let offset = u32::from_be_bytes([p.buf[5], p.buf[6], p.buf[7], p.buf[8]]);
    if offset != 9 {
        return 0;
    }
    100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_via_runtime_context_installs_container() {
        let mut ctx = oxideav_core::RuntimeContext::new();
        register(&mut ctx);
        assert_eq!(ctx.containers.container_for_extension("flv"), Some("flv"));
    }
}
