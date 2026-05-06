//! Pure-Rust Flash Video (FLV) container demuxer.
//!
//! Reference: Adobe *Video File Format Specification* version 10
//! (2008). The crate parses the 9-byte FLV header and the `FLV tag`
//! stream that follows, and emits one [`oxideav_core::Packet`] per
//! media tag. Script tags (AMF0 `onMetaData`) are parsed for container
//! metadata (duration, width, height, codec ids) and consumed
//! internally.
//!
//! See the crate README for the supported codec map. Muxing is not
//! implemented.

#![deny(missing_debug_implementations)]

pub mod amf0;
pub mod demuxer;
pub mod header;
pub mod tag;

use oxideav_core::ContainerRegistry;

pub use amf0::{parse_amf0_value, AmfValue};
pub use demuxer::{open as open_demuxer, FlvDemuxer};
pub use header::{FlvHeader, FLV_SIGNATURE};
pub use tag::{
    audio_codec_id_str, video_codec_id_str, AudioTagHeader, TagHeader, TagType, VideoTagHeader,
    AUDIO_CODEC_AAC, AUDIO_CODEC_MP3, AUDIO_CODEC_MP3_8K, VIDEO_CODEC_H264, VIDEO_CODEC_VP6A,
    VIDEO_CODEC_VP6F,
};

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
