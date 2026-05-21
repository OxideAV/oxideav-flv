# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Enhanced RTMP / E-FLV `ExAudioTagHeader` parsing (Veovera
  `enhanced-rtmp-v2` "Enhanced Audio"): SoundFormat=9 (ExHeader)
  switches the audio tag into FourCC + AudioPacketType semantics
  (legacy SoundRate / SoundSize / SoundType bits are repurposed
  as the AudioPacketType UB[4]). FourCCs `Opus` / `fLaC` / `ac-3`
  / `ec-3` / `.mp3` / `mp4a` map onto stable codec ids
  (`opus` / `flac` / `ac3` / `eac3` / `mp3` / `aac`); unknown
  FourCCs surface as `flv:exaudio:<ascii>`. AudioPacketTypes
  `SequenceStart` (Opus ID header / FLAC `fLaC + STREAMINFO` /
  AAC `AudioSpecificConfig` → extradata + header packet),
  `CodedFrames` (data), `SequenceEnd` (dropped),
  `MultichannelConfig` / `Multitrack` / `ModEx` (header+discard)
  are routed onto existing Packet semantics. ModEx headers chain
  off the front of the body; `TimestampOffsetNano` ModEx values
  accumulate into a single nanosecond offset on the parsed
  header (the 8-bit / 16-bit size escape is supported).
  Multitrack outer header parsed for `OneTrack` / `ManyTracks` /
  `ManyTracksManyCodecs`. `audiosamplerate` / `stereo` /
  `audiodatarate` from `onMetaData` apply to ExAudio streams the
  same way they do to legacy ones. New public types:
  `ExAudioTagHeader`, `ExAudioPacketType`, `AudioPacketModExType`,
  `AvMultitrackType` + the spec-defined `FOURCC_*` /
  `SOUND_FORMAT_EX_HEADER` constants.
- Enhanced RTMP / E-FLV `ExVideoTagHeader` parsing (Veovera
  `enhanced-rtmp-v1` Table 4 + `enhanced-rtmp-v2` "Enhanced Video"):
  the IsExHeader bit (top bit of the leading VideoTagHeader byte)
  switches into FourCC + PacketType semantics. FourCCs `av01` / `vp09`
  / `vp08` / `hvc1` / `avc1` / `vvc1` map onto stable codec ids
  (`av1` / `vp9` / `vp8` / `h265` / `h264` / `h266`); unknown
  FourCCs surface as `flv:exvideo:<ascii>`. PacketTypes
  `SequenceStart` (config record → extradata + header packet),
  `CodedFrames` (data; HEVC/VVC/AVC parse a 3-byte SI24 CTO so
  pts/dts split correctly), `CodedFramesX` (data, implicit CTO=0),
  `SequenceEnd` (dropped), `Metadata` (HDR colorInfo →
  header+discard), `Mpeg2TsSequenceStart` (header), `Multitrack` /
  `ModEx` (header+discard) are all routed onto existing Packet
  semantics. Seek scan-forward recognises Ex keyframes via the same
  FrameType field. New public types: `ExFrameType`, `ExPacketType`,
  `ExVideoTagHeader` + the spec-defined `FOURCC_*` constants.
- AMF0 `Reference` marker (type 7, UI16 BE) decoded into
  `AmfValue::Reference(u16)` — FLV `onMetaData` payloads rarely emit
  it, but unexpected occurrences no longer poison the parse.
- `onXMPData` (FLV spec Annex E.6) — the `liveXML` body surfaces via
  `metadata["xmp"]`.
- `onCuePoint` (Annex A) — payload flattened into
  `metadata["cuepoint.N.<key>"]` entries so callers see every field.
- `|AdditionalHeader` (Annex F.2 FLV encryption headline) — parsed
  into `encryption.version` / `encryption.method` /
  `encryption.algorithm` / `encryption.key_length` /
  `encryption.key_subtype` metadata + `FlvDemuxer::is_encrypted()`
  helper.
- Filtered tag preamble (`EncryptionTagHeader` + `FilterParams`, spec
  F.3.1 / F.3.2) parsed for both spec-defined FilterNames
  (`"Encryption"` v1 and `"SE"` v2 selective); ciphertext bodies are
  forwarded with `flags.discard = true` so decoders skip past
  encrypted samples instead of trying to interpret them.
- FrameType 5 video info / command tags (E.4.3.1) surface as packets
  with `flags.header = true` + `flags.discard = true` and a 1-byte
  body carrying the command (0 = start of client-side-seeking
  sequence, 1 = end), instead of being routed to the decoder.
- `onMetaData` enrichment: `videoframerate` / `framerate` lift into
  `CodecParameters::frame_rate` (NTSC values 23.976 / 29.97 / 59.94 /
  47.952 / 119.88 snap to canonical 1001-denominator `Rational`s).
  `videodatarate` / `audiodatarate` (kbps) lift into
  `CodecParameters::bit_rate`. `audiosamplerate` overrides the
  audio-tag header's 5.5/11/22/44 kHz `SoundRate` quantisation;
  `stereo` overrides its `SoundType` bit.
- New public types: `EncryptedTagPreamble`, `FrameType`,
  `VideoInfoCommand`.

### Added (previously)

- `Demuxer::seek_to` — O(log n) bisect on the `onMetaData.keyframes`
  toc (`filepositions[]` / `times[]`) when present, scan-forward
  fallback otherwise. Audio-only files use the scan path against
  every audio tag (each is independently decodable).

## [0.0.3](https://github.com/OxideAV/oxideav-flv/compare/v0.0.2...v0.0.3) - 2026-05-06

### Other

- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-flv/pull/502))
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- pin release-plz to patch-only bumps

## [0.0.2](https://github.com/OxideAV/oxideav-flv/compare/v0.0.1...v0.0.2) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- drop Cargo.lock — this crate is a library
