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

- 9-byte `FLV\x01` header (Adobe FLV Spec v10.1, Annex E.2).
- Tag stream: 4-byte `PreviousTagSize` prefix + 11-byte tag header +
  payload + close.
- Script tags (Annex E.4.4):
  - `onMetaData` — parsed for `duration`, `width`, `height`,
    `videocodecid`, `audiocodecid`, `framerate` / `videoframerate`,
    `audiodatarate`, `videodatarate`, `audiosamplerate`, `stereo`,
    `creationdate`, etc. NTSC-family `videoframerate` values
    (29.97/23.976/59.94/47.952/119.88) snap to canonical 1001-denominator
    `Rational`s; non-canonical rates use a 1/1000 fallback.
    `videodatarate` / `audiodatarate` (kbps) lift into
    `CodecParameters::bit_rate`. `audiosamplerate` overrides the
    `SoundRate` field's 5.5/11/22/44 kHz quantisation. The optional
    `keyframes` toc (`filepositions[]` / `times[]`) is harvested for
    O(log n) seeks.
  - `onXMPData` (Annex E.6) — `liveXML` body surfaced via
    `metadata["xmp"]`.
  - `onCuePoint` (Annex A) — payload flattened into
    `metadata["cuepoint.N.<key>"]` so callers see every field without
    pulling in a separate cue model.
  - `|AdditionalHeader` (Annex F.2) — FLV-encryption header.
    `EncryptionHeader.Version`, `Method`, `EncryptionAlgorithm`,
    `KeyLength`, and `KeyInfo.SubType` surface as `encryption.*`
    metadata; `FlvDemuxer::is_encrypted()` returns `true`.
- `Demuxer::seek_to` — bisects the `keyframes` toc when present,
  otherwise scans tags forward to the first video keyframe (or audio
  packet, for audio-only files) at-or-after the requested pts.
- AMF0 `Reference` (marker `0x07`, spec E.4.4.2 type 7) preserved as
  `AmfValue::Reference(u16)`. `MovieClip` (reserved-not-supported) +
  every unenumerated marker still error out so contamination is loud.
- FrameType 5 "video info / command" tags (spec E.4.3.1) are surfaced
  as packets with `flags.header = true` + `flags.discard = true` and a
  1-byte body carrying the command (0 = start of client-side-seeking
  sequence, 1 = end). Decoders skip them via the discard flag;
  callers can react to the seek-sequence boundary.
- Filter-flag (Annex F.3.1): tags whose tag-type byte has bit `0x20`
  set carry an `EncryptionTagHeader` (NumFilters + FilterName +
  Length) + `FilterParams` preamble. Both spec-defined FilterNames
  (`"Encryption"` v1 = always-on AES-CBC with IV; `"SE"` v2 =
  per-packet selective encryption with EncryptedAU + optional IV)
  are parsed; the ciphertext body is forwarded with
  `flags.discard = true`. No decryption — the demuxer surfaces
  "this file is DRM-protected, here's the metadata" rather than
  silently dropping the data.
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
- Enhanced RTMP (E-FLV, Veovera `enhanced-rtmp-v1` / `v2`):
  - IsExHeader-flagged video tags (high bit of the leading byte) are
    routed through the ExVideoTagHeader path: FourCC codec identifier
    + PacketType replace the legacy 4-bit CodecID.
  - FourCCs decoded: `av01` (AV1), `vp09` (VP9), `vp08` (VP8), `hvc1`
    (HEVC), `avc1` (FourCC-signaled AVC), `vvc1` (VVC). Unknown
    FourCCs fall through to `flv:exvideo:<ascii>` so producers can be
    logged rather than silently dropped.
  - PacketTypes: `SequenceStart` → header packet with the decoder
    config record (AV1CC / VPCC / HEVCDecoderConfigurationRecord) in
    extradata; `CodedFrames` → data packet (HEVC / VVC / AVC pull a
    3-byte SI24 CompositionTimeOffset so pts/dts split correctly);
    `CodedFramesX` → CodedFrames with implicit CTO=0;
    `SequenceEnd` → dropped (no decoder input); `Metadata` → header +
    discard so HDR `colorInfo` AMF blobs reach metadata observers but
    not video decoders; `Mpeg2TsSequenceStart` → header packet;
    `Multitrack` / `ModEx` → header + discard (parsed but not
    track-split). FrameType=Command keeps the legacy "discardable
    header" semantics.
  - Seek scan-forward path recognises Ex keyframes via the same
    FrameType bit field.
- Enhanced RTMP audio (E-FLV ExAudioTagHeader, Veovera
  `enhanced-rtmp-v2` §"Enhanced Audio"):
  - SoundFormat=9 (ExHeader) flags an audio tag as FourCC-coded. The
    low 4 bits of the leading byte become `AudioPacketType` (legacy
    SoundRate/SoundSize/SoundType bits are repurposed).
  - FourCCs decoded: `Opus` (Opus), `fLaC` (FLAC), `ac-3` (AC-3),
    `ec-3` (E-AC-3), `.mp3` (MPEG Layer III), `mp4a` (FourCC-signaled
    AAC). Unknown FourCCs fall through to `flv:exaudio:<ascii>` so
    producers can be logged rather than silently dropped.
  - AudioPacketTypes: `SequenceStart` → header packet with the codec's
    config blob (Opus ID header / FLAC `fLaC + STREAMINFO` / AAC
    `AudioSpecificConfig`) routed to extradata; `CodedFrames` → data
    packet; `SequenceEnd` → dropped (no decoder input);
    `MultichannelConfig` / `Multitrack` / `ModEx` → header + discard
    (parsed but not consumed). `audiosamplerate` / `stereo` /
    `audiodatarate` from `onMetaData` apply to ExAudio streams the
    same way they do to legacy ones.
  - ModEx headers chain: zero or more length-prefixed modifier blobs
    are consumed off the front of the body; `TimestampOffsetNano`
    modifiers accumulate into a single nanosecond offset on the
    parsed header. The 8-bit / 16-bit size escape (UI8+1 → UI16+1
    when the first byte is `0xFF`) is supported.
  - Multitrack outer header parsed: `OneTrack` / `ManyTracks` carry
    a single FourCc; `ManyTracksManyCodecs` leaves the FourCc empty
    (per-track FourCc lives in the body — not yet track-split).

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

When the IsExHeader flag is set the FourCC table below applies instead
of the FLV id column (legacy ids 0..15 remain reserved on that side):

| FourCC | Media | CodecId | Notes                              |
| ------ | ----- | ------- | ---------------------------------- |
| `av01` | video | `av1`   | AV1CodecConfigurationRecord        |
| `vp09` | video | `vp9`   | VPCodecConfigurationRecord         |
| `vp08` | video | `vp8`   |                                    |
| `hvc1` | video | `h265`  | HEVCDecoderConfigurationRecord     |
| `avc1` | video | `h264`  | FourCC-signaled AVC                |
| `vvc1` | video | `h266`  | VVCDecoderConfigurationRecord      |

When SoundFormat=9 (ExHeader) is set on an audio tag the FourCC table
below applies (legacy SoundFormat ids 0..15 remain reserved on that
side; FourCC mode also disables the SoundRate / SoundSize / SoundType
header bits, which are reused for `AudioPacketType`):

| FourCC | Media | CodecId | Notes                                       |
| ------ | ----- | ------- | ------------------------------------------- |
| `Opus` | audio | `opus`  | RFC 7845 OpusHead on SequenceStart          |
| `fLaC` | audio | `flac`  | `fLaC` + STREAMINFO on SequenceStart        |
| `ac-3` | audio | `ac3`   | ATSC AC-3 sync frame on CodedFrames         |
| `ec-3` | audio | `eac3`  | ATSC E-AC-3 sync frame on CodedFrames       |
| `.mp3` | audio | `mp3`   | MPEG-1/2 Layer III frame on CodedFrames     |
| `mp4a` | audio | `aac`   | AudioSpecificConfig on SequenceStart        |

## License

MIT — see [LICENSE](LICENSE).
