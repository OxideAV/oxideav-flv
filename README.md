# oxideav-flv

Pure-Rust **Flash Video (FLV)** container demuxer for oxideav. Zero C
dependencies, no FFI, no `*-sys` crates.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-container = "0.1"
oxideav-flv = "0.0"
```

## Status

### Demuxer

- 9-byte `FLV\x01` header (Adobe FLV Spec v10).
- Tag stream: 4-byte `PreviousTagSize` prefix + 11-byte tag header +
  payload + close.
- Script tag (type 0x12, AMF0 `onMetaData`) — parsed for `duration`,
  `width`, `height`, `videocodecid`, `audiocodecid`, `framerate`,
  `audiodatarate`, `videodatarate`, `creationdate`, etc.
- Audio tag (0x08):
  - Codec id 2 = MP3, 14 = MP3 8 kHz.
  - Codec id 10 = AAC. First packet-type byte distinguishes
    `AACSequenceHeader` (`header = true`) from `AACRaw`.
  - Codec id 0, 3 = Linear PCM (native, little-endian).
  - Codec id 1 = ADPCM.
  - Codec id 7 = G.711 A-law, 8 = G.711 mu-law.
  - Codec id 11 = Speex.
- Video tag (0x09):
  - Codec id 2 = Sorenson H.263 (flv1).
  - Codec id 3 = Screen video.
  - Codec id 4 = VP6 (vp6f).
  - Codec id 5 = VP6 with alpha (vp6a) — the first payload byte gives
    the alpha offset, which is stripped and surfaced to the decoder
    via extradata.
  - Codec id 7 = AVC / H.264 — AVCPacketType + CompositionTime header
    bytes are parsed; `AVCDecoderConfigurationRecord` lands in
    extradata, subsequent NALU packets carry the 4-byte length prefix.

### Muxer

Not implemented — out of scope for the initial import. FLV muxing is
rare and easy to add later when a user actually needs it.

## Quick use

```rust
use std::io::Cursor;
use oxideav_core::NullCodecResolver;
use oxideav_container::{Demuxer, ReadSeek};

let bytes = std::fs::read("clip.flv")?;
let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
let mut dmx = oxideav_flv::open_demuxer(input, &NullCodecResolver)?;
while let Ok(pkt) = dmx.next_packet() {
    // hand pkt.data + stream index to the matching decoder.
    let _ = pkt;
}
# Ok::<(), oxideav_core::Error>(())
```

### Container / codec IDs

Container name: `"flv"` (extension `.flv`, magic `FLV\x01`).

Codec ids produced by the demuxer (stable strings so downstream code
can resolve them through `oxideav-codec`'s registry):

| FLV id | Media | CodecId    | Notes                        |
| ------ | ----- | ---------- | ---------------------------- |
| 0      | audio | `pcm_s16le`| endian-native per spec       |
| 1      | audio | `adpcm_swf`| Flash ADPCM                  |
| 2      | audio | `mp3`      |                              |
| 3      | audio | `pcm_s16le`| little-endian                |
| 7      | audio | `pcm_alaw` |                              |
| 8      | audio | `pcm_mulaw`|                              |
| 10     | audio | `aac`      | MP4-style config + raw AUs   |
| 11     | audio | `speex`    |                              |
| 14     | audio | `mp3`      | 8 kHz subvariant             |
| 2      | video | `flv1`     | Sorenson H.263               |
| 3      | video | `flashsv`  | Screen video v1              |
| 4      | video | `vp6f`     | VP6 FLV-flavour              |
| 5      | video | `vp6a`     | VP6 + alpha plane            |
| 6      | video | `flashsv2` | Screen video v2              |
| 7      | video | `h264`     | AVC: configuration + NALUs   |

## License

MIT — see [LICENSE](LICENSE).
