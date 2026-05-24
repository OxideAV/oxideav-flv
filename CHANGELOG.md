# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `audiosamplesize` onMetaData field (Adobe FLV Spec v10.1, Annex E.5 —
  "Resolution of a single audio sample", in bits) lifts into
  `CodecParameters::sample_format`. A value of `8` maps to `U8`, `16`
  maps to `S16`; any other value is unrecognised and leaves the
  header-derived format intact rather than inventing one. On the
  ExAudio (FourCC) path this is the *only* spec-defined resolution
  source — the leading byte of the ExHeader audio tag repurposes the
  legacy `SoundSize` bit as `AudioPacketType` (Veovera
  `enhanced-rtmp-v2`), so previously no `sample_format` was set at all.
  On the legacy path it overrides the 1-bit `SoundSize`-derived format,
  consistent with how `audiosamplerate` already overrides the 2-bit
  `SoundRate` field; per E.4.2.1 the SoundSize bit "only pertains to
  uncompressed formats" so for AAC / MP3 / Speex the onMetaData value
  is the producer's declared truth.
- Enhanced RTMP / E-FLV `onMetaData` per-track metadata maps
  (`audioTrackIdInfoMap` / `videoTrackIdInfoMap`, Veovera
  `enhanced-rtmp-v2` §"Enhancing onMetaData"). These NEW properties
  describe additional (non-default) tracks in a multitrack stream: each
  is an object keyed by trackId (`1`, `2`, … — trackId `0` is the
  default track, described by the top-level `onMetaData` fields), whose
  value is a per-track property object (width / height / videodatarate /
  channels / samplerate / codec id / …). Producers may send delta-style
  entries (only the fields that differ from the default) or full
  descriptors — both are valid. Previously the demuxer dropped both maps
  silently (they fell into the catch-all arm of `parse_on_metadata`).
  Now each map is flattened into the `metadata()` bag under a lowercased
  prefix via the existing `flatten_amf_value` walker —
  `videoTrackIdInfoMap[1].width` → `metadata["videotrackidinfomap.1.width"]`,
  `audioTrackIdInfoMap[2].samplerate` →
  `metadata["audiotrackidinfomap.2.samplerate"]`, etc. — so callers can
  read per-track bitrate / resolution / codec without an AMF model. A
  structural flatten (rather than a fixed schema) is used because the
  spec leaves the per-track field set producer-defined. A plain
  `onMetaData` with no track maps synthesises no `*trackidinfomap.*`
  keys.

- Enhanced RTMP / E-FLV `VideoPacketType.Metadata` HDR `colorInfo`
  parsing (Veovera `enhanced-rtmp-v2` §"Metadata Frame"). The Metadata
  frame's body carries no video data — it is a series of AMF0
  `[name, value]` pairs (encoded like `SCRIPTDATA`), the only defined
  pair being `["colorInfo", Object]` with nested `colorConfig` /
  `hdrCll` / `hdrMdcv` BT.2020 HDR sub-objects. Previously the demuxer
  forwarded the AMF blob as a header+discard packet without parsing it.
  Now the demuxer also harvests it into the `metadata()` bag:
  - New `harvest_video_metadata_frame` walks the AMF0 `[name, value]`
    pairs at `next_packet` time and flattens each value under a
    lowercased prefix via the existing `flatten_amf_value` helper —
    `colorInfo.colorConfig.transferCharacteristics` →
    `metadata["colorinfo.colorConfig.transferCharacteristics"]`,
    `colorInfo.hdrMdcv.redX` → `metadata["colorinfo.hdrMdcv.redX"]`, etc.
  - Per spec each new `colorInfo` "invalidates and replaces the current
    one": before flattening a new value the prior `colorinfo.*` entries
    are dropped. A reset (`colorInfo = Undefined`, the RECOMMENDED form,
    or an empty object) clears the nested keys — `Undefined` leaves a
    single `colorinfo = undefined` sentinel; `{}` leaves nothing.
  - Malformed / non-AMF Metadata bodies are ignored (no `colorinfo.*`
    entries) rather than poisoning the parse; the discard packet is
    still surfaced unchanged, so existing routing is untouched.

- Enhanced RTMP / E-FLV multitrack body splitter (Veovera
  `enhanced-rtmp-v2` §`ExAudioTagBody` / §`ExVideoTagBody`). The
  `Multitrack` packet type (audio `5` / video `6`) batches several
  tracks into one tag; previously the demuxer parsed the outer header
  (audio side only) and discarded the whole body. Now:

  - New `multitrack` module: `AvMultitrackType` (moved here from
    `ex_audio`; re-exported for back-compat), `MultitrackTrack`
    (`track_id` / `fourcc` / payload byte-range), and `split_tracks`
    walking the per-track loop `[FourCc?] trackId UI8 [sizeOfTrack
    UI24] payload`. `OneTrack` runs the single track to the end of the
    body; `ManyTracks` / `ManyTracksManyCodecs` slice each track by its
    `sizeOfTrack` UI24.
  - `ExVideoTagHeader` reaches parity with `ExAudioTagHeader`: a new
    `multitrack: Option<AvMultitrackType>` field plus outer-header parse
    (`videoMultitrackType` UB[4] + inner `videoPacketType` + shared
    FourCc, rejecting nested Multitrack). `ExVideoTagHeader::fourcc` is
    now `Option<u32>` (`None` for `ManyTracksManyCodecs`, matching the
    audio side); the single-track CTO is no longer read in multitrack
    mode (it lives inside each track payload).
  - The demuxer surfaces the **default track** (trackId 0, or first in
    wire order) per multitrack tag: its inner packet type drives
    routing, AVC/HEVC/VVC `CodedFrames` read the per-track SI24 CTO from
    inside the track payload, and `SequenceStart` extradata is lifted
    from the default track's payload. `ManyTracksManyCodecs` (no shared
    FourCc) maps to a `flv:exaudio:multicodec` / `flv:exvideo:multicodec`
    sentinel codec id.

- Enhanced RTMP / E-FLV `VideoCommand` plumbing on `ExVideoTagHeader`
  (Veovera `enhanced-rtmp-v2` §`Extended VideoTagHeader`). When the
  Ex video tag carries `videoFrameType == VideoFrameType.Command` and
  `videoPacketType != VideoPacketType.Metadata`, the spec mandates a
  UI8 `VideoCommand` byte after the FourCc (and after the
  CompositionTimeOffset on AVC / HEVC / VVC `CodedFrames`) with
  `processVideoBody = false` afterwards. Previously the demuxer
  dropped the byte entirely on the Ex path; the legacy FrameType=5
  routing surfaced it as a 1-byte discard packet. Both paths now
  agree:

  - New `VideoCommand` enum (`StartSeek = 0`, `EndSeek = 1`,
    `Reserved(u8)` for `2..=0xFF`) with `from_u8` / `as_u8`
    round-trip helpers. Reserved codes are preserved verbatim so
    future spec extensions land without parser changes.
  - `ExVideoTagHeader::video_command: Option<VideoCommand>` carries
    the decoded command; `None` for every non-command tag (and for
    `Metadata` command tags where the spec says `frameType` is
    ignored). `bytes_consumed` advances past the command byte so the
    body tail is empty per spec.
  - The demuxer's Ex video path emits a header+discard packet whose
    1-byte body is the command byte — parity with the legacy
    FrameType=5 routing so downstream callers can resolve the
    seek-sequence boundary via
    `oxideav_flv::VideoCommand::from_u8(pkt.data[0])`.
  - Truncated-command-byte body errors out (spec violation) rather
    than emitting a malformed packet.

- Enhanced RTMP / E-FLV `ModEx` (Modifier Extension) tag bodies
  (Veovera `enhanced-rtmp-v2` §`ExAudioTagHeader` / §`ExVideoTagHeader`,
  while packet_type == ModEx = 7). The audio and video Ex headers
  now share a `crate::mod_ex` walker that:
  - Decodes the size prefix (UI8 + 1, with `0xFF → UI16 + 1` escape
    for payloads ≥ 257 bytes, max 65_536 bytes).
  - Emits one `ModExEntry { subtype_raw, payload, raw }` per loop
    iteration, with a typed `ModExPayload::TimestampOffsetNano
    { offset_ns }` for subtype 0 (UI24 BE nanosecond refinement
    0..999_999 ns) and a `Reserved { subtype_raw }` placeholder for
    every other UB[4] code (1..15) that keeps the opaque payload on
    `ModExEntry::raw` so future-defined subtypes don't need parser
    changes.
  - Re-reads the next AudioPacketType / VideoPacketType from the low
    nibble of the trailer byte, chaining ModEx packets transparently.
  - Sums all TimestampOffsetNano values into a per-tag
    `timestamp_offset_nano: u32` (already in place on
    `ExAudioTagHeader`; **new** on `ExVideoTagHeader` — round 87/89
    surfaced video ModEx as routed-but-opaque).
  - Rejects malformed inputs (truncated size prefix / data / trailer,
    TimestampOffsetNano payload shorter than 3 bytes).

  New public types: `mod_ex::{ModExEntry, ModExPayload,
  AudioPacketModExType, VideoPacketModExType}` (the
  `AudioPacketModExType` name is now re-exported from `mod_ex` —
  consumers who imported it via `oxideav_flv::AudioPacketModExType`
  see no surface change). `ExAudioTagHeader` gains
  `mod_ex_entries: Vec<ModExEntry>`; `ExVideoTagHeader` gains
  `mod_ex_entries: Vec<ModExEntry>` + `timestamp_offset_nano: u32`.

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
