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
    `audiodatarate`, `videodatarate`, `audiosamplerate`,
    `audiosamplesize`, `stereo`, `creationdate`, etc. NTSC-family
    `videoframerate` values
    (29.97/23.976/59.94/47.952/119.88) snap to canonical 1001-denominator
    `Rational`s; non-canonical rates use a 1/1000 fallback.
    `videodatarate` / `audiodatarate` (kbps) lift into
    `CodecParameters::bit_rate`. `audiosamplerate` overrides the
    `SoundRate` field's 5.5/11/22/44 kHz quantisation.
    `audiosamplesize` (8 / 16) sets `CodecParameters::sample_format`
    (U8 / S16) — the only resolution source on ExAudio (where the
    SoundSize bit was repurposed as AudioPacketType) and the
    spec-defined override for legacy compressed formats whose 1-bit
    SoundSize "only pertains to uncompressed formats" (E.4.2.1). The optional
    `keyframes` toc (`filepositions[]` / `times[]`) is harvested for
    O(log n) seeks. Enhanced-RTMP-v2's new `audioTrackIdInfoMap` /
    `videoTrackIdInfoMap` per-track metadata maps (keyed by trackId
    1, 2, … for non-default multitrack variants; trackId 0 is the
    top-level fields) flatten under
    `metadata["videotrackidinfomap.1.width"]`,
    `metadata["audiotrackidinfomap.2.samplerate"]`, … so callers read
    per-track bitrate / resolution / codec without an AMF model. Any
    top-level property whose value doesn't fit those buckets — Null,
    Undefined, Date (e.g. `creationdate`), Reference, Unsupported,
    XMLDocument, StrictArray, an `0x11` AVM+ AMF3 sub-tree, or a
    producer-defined nested Object outside the known schema — is
    lifted through the AMF flatten walker under its original property
    name, so the metadata bag preserves the producer's full
    `onMetaData` surface (Date → `"date:<millis>tz:<offset>"`,
    nested objects → `<key>.<subkey>` paths, Null/Undefined → the
    string sentinels `"null"` / `"undefined"`).
  - `onXMPData` (Annex E.6) — `liveXML` body surfaced via
    `metadata["xmp"]`.
  - `onCuePoint` (Annex A) — payload flattened into
    `metadata["cuepoint.N.<key>"]` so callers see every field without
    pulling in a separate cue model.
  - `|AdditionalHeader` (Annex F.2) — FLV-encryption header.
    `EncryptionHeader.Version`, `Method`, `EncryptionAlgorithm`,
    `KeyLength`, and `KeyInfo.SubType` surface as `encryption.*`
    metadata; `FlvDemuxer::is_encrypted()` returns `true`.
  - **Any other script name** (Enhanced-RTMP-v2 §"Enhancing onMetaData"
    treats SCRIPTDATA as a generic "method-name + argument" carrier so
    producers may emit names beyond the four spec-defined ones —
    live-caption tracks, producer telemetry, RTMP-relayed status
    snapshots, etc.) lifts the method name under
    `metadata["scriptdata.name"]` (legacy sentinel) AND flattens the
    argument payload under `scriptdata.<name>.<...>` via the same
    walker that handles `onCuePoint` payloads. Scalars land directly
    under `scriptdata.<name>`, composite values fan out with
    `.<subkey>` / `[i]` suffixes, so the producer's full argument
    structure reaches callers instead of being silently dropped.
- `Demuxer::seek_to` — bisects the `keyframes` toc when present,
  otherwise scans tags forward to the first video keyframe (or audio
  packet, for audio-only files) at-or-after the requested pts.
- AMF0 `Reference` (marker `0x07`, spec E.4.4.2 type 7) preserved as
  `AmfValue::Reference(u16)`. AMF0 `Unsupported` (`0x0D`, spec §2.15) /
  `XMLDocument` (`0x0F`, §2.17) / `TypedObject` (`0x10`, §2.18) parse
  into dedicated variants; an `onMetaData` payload wrapped in a typed
  object (FMS / Wowza relays do this) walks through to the same
  property extraction path, and the producer's class alias surfaces
  under `metadata["scriptdata.class"]`. `liveXML` accepts both the
  ordinary string and the `XMLDocument` marker. `MovieClip` /
  `RecordSet` (reserved-not-supported) still error so contamination is
  loud.
- AMF3 (Adobe AMF 3 Specification, Dec 2007) — the AMF0 `0x11` AVM+
  switch marker (AMF0 §3.1) now lifts the byte stream into a full AMF3
  decoder, surfaced as `AmfValue::AvmPlus(Box<Amf3Value>)`. The AMF3
  module covers all 13 type markers (`undefined` / `null` / `false` /
  `true` / `integer` / `double` / `string` / `xml-doc` / `date` /
  `array` / `object` / `xml` / `byte-array`) with the three implicit
  reference tables (strings, complex-objects, traits) per AMF 3
  §2.2 + §3.8 + §3.12. The U29 variable-length unsigned 29-bit integer
  primitive (§1.3.1) decodes the 1-byte through 4-byte forms (with
  exact `2^29 - 1` cap proof in tests), and UTF-8-vr (§1.3.2)
  preserves the empty-string-never-sent-by-reference rule. Trait
  blocks support inline (sealed_count + dynamic + class name + sealed
  property names), traits-ref (back-reference to a prior trait), and
  traits-ext (externalizable — the flag is set and zero body bytes
  consumed since the spec gives no parser recipe; callers that know
  the class's private grammar can decode the trailing bytes
  themselves). Object back-references decode to an alias of the
  prior instance; circular graphs are handled by reserving the
  object-table slot before descending into Array / Object children.
  Top-level demuxer flattening: an `AvmPlus` value reached via
  `onCuePoint` or `VideoPacketType.Metadata` lowers through a
  symmetric `flatten_amf3_value` walker so callers see AVM+ payloads
  under the same `metadata["prefix.key"]` shape as their AMF0
  counterparts.
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
  - **Audio silence message** (enhanced-rtmp-v2 §`AudioPacketType`): an
    audio tag whose payload is zero-length (an empty audio message
    carrying no `AudioTagHeader`) signals a period of silence with
    spec-defined playback semantics — drain buffered audio, flush the
    audio decoder, and stop using the audio clock as the A/V-sync master
    until media resumes (declared to have "no less than the same meaning
    as" `SequenceEnd`). Once an audio stream is established, the demuxer
    surfaces the silence message as a zero-length `header = true` +
    `discard = true` packet at the tag's timestamp, so callers can react
    to the boundary rather than have the signal silently dropped; the
    empty body never reaches a decoder as a frame, and the stream
    resumes cleanly on the next real audio tag. (A silence tag that
    precedes any real audio tag mints no stream — silence carries no
    codec — and is skipped during discovery, unchanged.)
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
    discard so HDR `colorInfo` AMF blobs do not reach video decoders,
    **and** the AMF body is parsed: each `[name, value]` pair is
    flattened under a lowercased prefix (`colorInfo` →
    `metadata["colorinfo.colorConfig.bitDepth"]`,
    `["colorinfo.hdrCll.maxCLL"]`, `["colorinfo.hdrMdcv.redX"]`, …) so
    callers see the BT.2020 HDR parameters without an AMF model. A new
    `colorInfo` replaces the prior one (spec: "invalidates and replaces");
    a `colorInfo = Undefined` reset clears the nested keys and leaves a
    `colorinfo = undefined` sentinel; `Mpeg2TsSequenceStart` → header packet;
    `Multitrack` → outer header parsed (`AvMultitrackType` +
    shared/per-track FourCc), body split per-track via `split_tracks`,
    and the default track (trackId 0, or first in wire order) emitted as
    the packet — its inner packet type drives routing and, for
    AVC/HEVC/VVC `CodedFrames`, the per-track SI24 CTO is read inside the
    track payload;
    `ModEx` → ModEx-loop walked off the front (TimestampOffsetNano
    decoded; reserved subtypes preserved opaquely) so the resolved
    inner packet type drives the routing.
  - `FrameType=Command` (5): per enhanced-rtmp-v2 §`Extended
    VideoTagHeader`, when `videoFrameType == Command` and
    `videoPacketType != Metadata` the body carries a single UI8
    `VideoCommand` (0 = `StartSeek`, 1 = `EndSeek`, 2..=0xFF reserved)
    and no further codec payload. The Ex header parser advances
    `bytes_consumed` past the command byte and surfaces it on
    `ExVideoTagHeader::video_command`; the demuxer emits a
    header+discard packet whose 1-byte body is the decoded command,
    matching the legacy FrameType=5 routing. Reserved command codes
    are preserved verbatim via `VideoCommand::Reserved(u8)` so future
    spec additions don't need parser changes. When `packet_type ==
    Metadata` the spec says `frameType` is ignored — no command byte
    is read and the trailing bytes remain the AMF metadata payload.
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
    `MultichannelConfig` → header + discard, **and** the speaker-layout
    body is parsed (enhanced-rtmp-v2 §`ExAudioTagBody`):
    `AudioChannelOrder` + channelCount + (`Custom`) the per-channel
    `AudioChannel` speaker map / (`Native`) the `audioChannelFlags`
    UI32 presence mask land under
    `metadata["multichannelconfig.order" / ".channelcount" / ".flags"
    / ".layout" / ".mapping"]` (latest signal supersedes), and the
    channel count lifts into `CodecParameters::channels` — the spec's
    channel-mapping truth for codecs that are not self-describing
    (called out for Opus streams with an empty SequenceStart payload);
    multitrack-wrapped configs unwrap the default track's payload
    first; `ModEx` → header + discard
    (parsed but not consumed); `Multitrack` → outer header parsed, body
    split per-track via `split_tracks`, default track (trackId 0, or
    first) emitted via its inner packet type (extradata lifted from the
    default track's `SequenceStart` payload). `audiosamplerate` /
    `stereo` / `audiodatarate` from `onMetaData` apply to ExAudio
    streams the same way they do to legacy ones.
  - ModEx headers chain on both the audio AND video sides (the v2
    spec defines the same loop shape for both): zero or more
    length-prefixed modifier blobs are consumed off the front of the
    body, each emitted as a `ModExEntry { subtype_raw, payload, raw }`
    on the parsed header. The 8-bit / 16-bit size escape (UI8+1 →
    UI16+1 when the first byte is `0xFF`) covers payloads of 1..65_536
    bytes. The only currently-defined subtype is
    `TimestampOffsetNano` (subtype 0 — UI24 BE nanosecond refinement
    0..999_999 ns); spec-reserved subtypes (1..15) are surfaced via
    `ModExPayload::Reserved { subtype_raw }` with the opaque payload
    on `ModExEntry::raw` so future spec additions land without parser
    changes. The per-tag accumulator (`timestamp_offset_nano` on
    `ExAudioTagHeader` / `ExVideoTagHeader`) sums every
    TimestampOffsetNano in the chain.
  - Multitrack body splitter (`split_tracks`, shared audio/video, per
    enhanced-rtmp-v2 §`ExAudioTagBody` / §`ExVideoTagBody`): `OneTrack`
    runs the single track's payload to the end of the body; `ManyTracks`
    / `ManyTracksManyCodecs` walk a loop of `[FourCc?] trackId UI8
    [sizeOfTrack UI24] payload` records into a `Vec<MultitrackTrack>`
    (`track_id` / `fourcc` / payload byte-range). The stream model stays
    single-stream; the demuxer surfaces the default track per tag.
    `ManyTracksManyCodecs` (no shared FourCc) maps the stream to the
    `flv:exaudio:multicodec` / `flv:exvideo:multicodec` sentinel.

### Typed `onMetaData` accessors

[`oxideav_flv::TypedMetadata`] (module `typed_meta`) is a borrowed view
over `Demuxer::metadata()` that re-types the spec-defined fifteen
`onMetaData` properties of Annex E.5 back into their declared AMF
types — Number / Boolean / String — so callers don't have to parse
strings out of the bag themselves:

```rust
use oxideav_flv::TypedMetadata;
let typed = TypedMetadata::new(dmx.metadata());
let dur = typed.duration();                 // Option<f64>     — seconds
let w   = typed.width();                    // Option<u32>     — pixels
let h   = typed.height();                   // Option<u32>     — pixels
let fps = typed.framerate();                // Option<f64>     — fps
let st  = typed.stereo();                   // Option<bool>
let cse = typed.can_seek_to_end();          // Option<bool>
let cd  = typed.creationdate();             // Option<&str>    — free-form
let cdd = typed.creationdate_as_date();     // Option<(f64, i16)> — ms + tz min
let vid = typed.video_codec_id_str();       // Option<String>  — "h264", "vp6f", …
```

Per-property accessors: `duration` / `filesize` / `width` / `height` /
`framerate` / `videoframerate` / `effective_framerate` (alias-aware:
`videoframerate` first, falls back to `framerate` — mirrors the order
the demuxer uses when lifting `CodecParameters::frame_rate`) /
`video_data_rate_kbps` / `audio_data_rate_kbps` /
`audio_sample_rate` / `audio_sample_size` / `audio_delay_seconds` /
`video_codec_id` / `audio_codec_id` (+ string forms via
[`tag::video_codec_id_str`] / [`tag::audio_codec_id_str`]) / `stereo` /
`can_seek_to_end` / `creationdate` (+ a structured
`creationdate_as_date` accessor that decodes the
`"date:<ms>tz:<offset>"` carrier the demuxer uses when the producer
stamped the field as an AMF0 `Date`). Missing or malformed entries
return `None`; the accessor never panics on bag contents.

The Enhanced-RTMP-v2 per-track property maps (`videoTrackIdInfoMap` /
`audioTrackIdInfoMap` from the §"Enhancing onMetaData" extension) get
their own typed views — `TypedMetadata::video_track_info_map()` /
`audio_track_info_map()` iterate one
[`TypedVideoTrackInfo`] / [`TypedAudioTrackInfo`] per non-zero
trackId the producer signalled (trackId 0 is the default track — its
fields live at the top level of `onMetaData` and are read via the
regular accessors above; the iterator skips it). Each entry re-types
the per-track scalars: video tracks expose `width` / `height` /
`video_data_rate_kbps` / `video_codec_id` / `framerate`
(alias-preference: `videoframerate` first, falling back to
`framerate`); audio tracks expose `audio_data_rate_kbps` /
`audio_sample_rate` (the spec's per-track shortened `samplerate`, not
the top-level `audiosamplerate`) / `channels` (modern count, not the
legacy `stereo` boolean) / `audio_codec_id`. Delta-style entries
(only the fields that differ from the default track) are first-class
— absent fields return `None` rather than synthesising from the
default-track value, so callers can distinguish "producer signalled
no per-track override" from "producer signalled the same value as
the default". `TypedMetadata::video_track_info(id)` /
`audio_track_info(id)` look up a specific trackId without iterating.

The Enhanced-RTMP-v2 `colorInfo` HDR Metadata Frame (§"Metadata Frame")
gets its own typed read view: `TypedMetadata::color_info()` returns
`Option<TypedColorInfo>` — `Some` when the producer ever stamped a
`colorInfo` (populated or reset), `None` when the producer never sent
one. Field accessors mirror the encode-side
[`color_info::ColorInfo`] / `ColorConfig` / `HdrCll` / `HdrMdcv` groups:
`bit_depth` / `color_primaries` / `transfer_characteristics` /
`matrix_coefficients` for the `colorConfig` ISO 23091-4 / H.273
indices; `max_fall` / `max_cll` for the content-light-level pair;
`red_x` / `red_y` / `green_x` / `green_y` / `blue_x` / `blue_y` /
`white_point_x` / `white_point_y` / `max_luminance` / `min_luminance`
for the SMPTE ST 2086:2018 mastering-display primaries. Producers
stamp only the metadata they actually have; absent fields return
`None`. The RECOMMENDED `["colorInfo", Undefined]` reset shape is
surfaced via `TypedColorInfo::is_reset_sentinel()` — `true` after the
producer cleared HDR state (every field accessor still returns `None`
in that case), `false` for a regular populated frame or the
empty-object reset form. The two shapes are otherwise indistinguishable
through the field accessors; the sentinel is the only way to tell
"producer reset HDR state" apart from "producer stamped a populated
frame whose fields the reader happened not to ask for".
`TypedColorInfo::to_color_info()` reconstructs the encode-side
[`color_info::ColorInfo`] struct from the read view in one call, closing
the read↔write loop: the struct it returns, fed back through
`ColorInfo::encode_amf` / `tag::write_ex_video_color_info`, re-emits the
same `["colorInfo", Object]` AMF body the demuxer parsed. Each of the
three groups (`colorConfig` / `hdrCll` / `hdrMdcv`) is `Some` only when
at least one of its fields survives as a finite, in-range value —
mirroring the encoder's "omit an all-absent group" convention — so a
reset sentinel and an all-malformed frame both rebuild to
`ColorInfo::default()`.

### Robustness

The demuxer is fuzz-shaped by a hand-crafted adversarial-input suite
(`tests/injection_robustness.rs`, 18 blobs) that exercises every parser
lever — empty / truncated header, forged oversize `DataSize` (the 16 MB
OOM lever), missing `PreviousTagSize` trailer, unknown AMF0 markers,
`LongString` with `u32::MAX` length, unterminated Object body,
non-object `onMetaData`, unknown `TagType`, forged Filter-flag with
truncated preamble, zero-length tags, and mid-stream truncation. The
guarantee is "never panics, never allocates a gigabyte, never spins
forever; either errors cleanly with `Error::InvalidData` / `Eof` / `Io`,
or degrades to a stream that terminates on the first `next_packet()`."
A pre-allocation guard in `read_body` rejects any tag whose `DataSize`
exceeds the remaining bytes of the underlying `Read + Seek` stream
*before* committing the `Vec`.

A `fuzz/` sub-crate (cargo-fuzz / libfuzzer) backs the hand-crafted
suite with four targets:

* `demuxer_open_next` — arbitrary bytes through `open_demuxer` and
  drain `next_packet` until error or EOF, with a per-iteration step
  cap so a forged input cannot wedge the harness.
* `amf0_parse` / `amf3_parse` — arbitrary bytes through
  `parse_amf0_value` / `parse_amf3_value` directly, exercising the
  `LongString` `u32::MAX` lever, unterminated Object bodies, the
  AMF0→AMF3 `0x11` AVM+ switch, U29 4-byte form, UTF-8-vr reference
  tables, traits chains, and circular complex-object references.
* `script_metadata_roundtrip` — synthesise a minimal FLV from
  fuzz-controlled scalar `onMetaData` properties using the muxer
  slice, re-parse with `open_demuxer`, assert every property the
  muxer emitted survives in `metadata()`; surfaces any
  writer/parser disagreement.

The fuzz crate carries its own `[workspace]` table so the umbrella
build is unaffected; `Cargo.lock` is gitignored.

### Muxer

A first muxer slice is implemented: enough to write a playable
audio-only FLV that round-trips bit-exactly back through `FlvDemuxer`.

| Primitive | Function | Spec |
| --- | --- | --- |
| File header | `header::write(w, has_audio, has_video)` | §E.2 |
| Leading `PreviousTagSize0` | `tag::write_first_previous_tag_size(w)` | §E.3 |
| Generic tag | `tag::write_tag(w, type, ts_ms, stream_id, body)` | §E.4.1 |
| MP3 audio tag | `tag::write_mp3_tag(w, ts_ms, rate_idx, is_16bit, is_stereo, frame)` | §E.4.2.1 |
| Raw AAC audio tag | `tag::write_aac_raw_tag(w, ts_ms, raw_au)` | §E.4.2.1/2 |
| Generic video tag | `tag::write_video_tag(w, ts_ms, VideoTagHeader, payload)` | §E.4.3.1 |
| Sorenson H.263 (`flv1`) tag | `tag::write_h263_tag(w, ts_ms, is_keyframe, frame)` | §E.4.3.1 |
| VP6 (`vp6f`) tag | `tag::write_vp6_tag(w, ts_ms, is_keyframe, frame)` | §E.4.3.1 |
| VP6-with-alpha (`vp6a`) tag | `tag::write_vp6a_tag(w, ts_ms, is_keyframe, alpha_offset, frame)` | §E.4.3.1 |
| AVC sequence header | `tag::write_avc_sequence_header(w, ts_ms, config_record)` | §E.4.3.1 |
| AVC NALU access unit | `tag::write_avc_nalu_tag(w, ts_ms, is_keyframe, composition_time_ms, au)` | §E.4.3.1 |
| AVC end-of-sequence | `tag::write_avc_end_of_sequence(w, ts_ms)` | §E.4.3.1 |
| Video info / command tag | `tag::write_video_info_command_tag(w, ts_ms, VideoInfoCommand)` | §E.4.3.1 |
| Generic Ex-video tag | `tag::write_ex_video_tag(w, ts_ms, &ExVideoTagHeader, payload)` | enhanced-rtmp v1/v2 |
| AV1 sequence start / coded frames | `tag::write_av1_sequence_start` / `tag::write_av1_coded_frames` | FourCc `av01` |
| VP9 sequence start / coded frames | `tag::write_vp9_sequence_start` / `tag::write_vp9_coded_frames` | FourCc `vp09` |
| HEVC sequence start / coded frames (SI24 CTO) / coded-frames-x | `tag::write_hevc_sequence_start` / `tag::write_hevc_coded_frames` / `tag::write_hevc_coded_frames_x` | FourCc `hvc1` |
| VVC sequence start / coded frames | `tag::write_vvc_sequence_start` / `tag::write_vvc_coded_frames` | FourCc `vvc1` |
| Ex-video sequence end / metadata | `tag::write_ex_video_sequence_end` / `tag::write_ex_video_metadata` | enhanced-rtmp v1/v2 |
| Generic Ex-audio tag | `tag::write_ex_audio_tag(w, ts_ms, &ExAudioTagHeader, payload)` | enhanced-rtmp v2 |
| Opus sequence start / coded frames | `tag::write_opus_sequence_start` / `tag::write_opus_coded_frames` | FourCc `Opus` |
| FLAC sequence start / coded frames | `tag::write_flac_sequence_start` / `tag::write_flac_coded_frames` | FourCc `fLaC` |
| AC-3 / E-AC-3 coded frames | `tag::write_ac3_coded_frames` / `tag::write_eac3_coded_frames` | FourCc `ac-3` / `ec-3` |
| MP3 (FourCc-mode) coded frames | `tag::write_mp3_ex_coded_frames` | FourCc `.mp3` |
| AAC (FourCc-mode) sequence start / coded frames | `tag::write_aac_ex_sequence_start` / `tag::write_aac_ex_coded_frames` | FourCc `mp4a` |
| Ex-audio sequence end | `tag::write_ex_audio_sequence_end` | enhanced-rtmp v2 |
| Ex-audio multichannel config | `tag::write_ex_audio_multichannel_config(w, ts_ms, fourcc, &MultichannelConfig)` | enhanced-rtmp v2 §`ExAudioTagBody` |
| Ex-video HDR `colorInfo` metadata tag | `tag::write_ex_video_color_info(w, ts_ms, fourcc, &ColorInfo)` | enhanced-rtmp v2 §"Metadata Frame" |
| Ex-video HDR `colorInfo` reset (`Undefined`) | `tag::write_ex_video_color_info_reset(w, ts_ms, fourcc)` | enhanced-rtmp v2 §"Metadata Frame" |
| Typed `colorInfo` AMF body encoder | `color_info::{ColorInfo, ColorConfig, HdrCll, HdrMdcv}::encode_amf()` / `encode_amf_into` / `encode_amf_reset` | enhanced-rtmp v2 §`ColorInfo` |
| AMF0 writers | `amf0::{write_number, write_boolean, write_string, write_property_name, write_object_start, write_ecma_array_start, write_object_end}` | AMF0 §2 |
| `onMetaData` script tag | `script::write_on_metadata(w, &MetadataBag)` | §E.4.4 / §E.5 |
| `onMetaData.keyframes` seek-table composite | `MetadataBag::keyframes(file_positions, times_seconds)` + AMF0 `write_strict_array_number` | §E.4.4.7 / §E.4.4.9 |
| `onCuePoint` script tag (Annex A embedded cue point) | `script::write_on_cue_point(w, ts_ms, &CuePointParams)` | Annex A.2 |
| `onXMPData` script tag (XMP metadata) | `script::write_on_xmp_data(w, ts_ms, live_xml)` | §E.6 |

`write_tag` returns the total bytes written (`11 + body.len() + 4`) and
emits the trailing `PreviousTagSize = 11 + DataSize` back-pointer.
`MetadataBag` is an ordered bag of the three AMF0 scalar property types
(Number / Boolean / String) plus the `keyframes` seek-table composite
(`MetaValue::Keyframes { file_positions: Vec<u64>, times_seconds:
Vec<f64> }`, emitted as an anonymous AMF0 Object carrying two parallel
SCRIPTDATASTRICTARRAY properties `filepositions` and `times`); insertion
order is preserved on the wire so the output is deterministic. The
`keyframes` writer validates the toc invariants the demuxer enforces on
the read side (non-empty, parallel-length, ascending finite `times`,
`filepositions` ≤ `2^53` so they survive the AMF0 Number round-trip)
and round-trips through `FlvDemuxer::seek_to` via the O(log n) bisect
path rather than the scan-forward fallback. Producers that need
correct offsets in the toc typically reserve a fixed-size `onMetaData`
slot up front, mux the body to learn the keyframe positions, and
rewrite the slot in-place with the populated toc — the writer is
agnostic of that strategy.

`script::write_on_cue_point(w, ts_ms, &CuePointParams)` emits an
Annex A embedded cue point. The typed [`CuePointParams`] pack carries
the four spec-conventional properties — `name` (producer identifier),
`time_seconds` (Number, validated finite), `kind` (
[`CuePointType::Event`] / [`CuePointType::Navigation`], wire-spelled
`"event"` / `"navigation"`), and a `parameters` Object of `(name,
string)` pairs the demuxer surfaces under
`metadata["cuepoint.<n>.parameters.<key>"]`. `timestamp_ms` is the
playback alignment timestamp the runtime dispatches on (Annex A.4:
the AMF data track is interleaved at the right time alongside audio
and video). `script::write_on_xmp_data(w, ts_ms, live_xml)` emits an
§E.6 `onXMPData` script tag carrying the `liveXML` String the
demuxer surfaces under `metadata["xmp"]`. Both writers round-trip
bit-exactly through `FlvDemuxer` and may be interleaved freely
between media tags.

The video tag writers all share the same `VideoTagHeader` / `FrameType`
model the demuxer uses; `write_avc_nalu_tag` packs `pts - dts` as the
SI24 `CompositionTime` (rejecting out-of-range deltas with
`Error::InvalidData`), and `write_avc_sequence_header` carries the
`AVCDecoderConfigurationRecord` verbatim so that
`extradata == config_record` after the round-trip. Tests in
`tests/roundtrip_muxer.rs` write video-only FLVs for H.263 / VP6 / VP6A /
AVC and assert the demuxer recovers the codec id, keyframe flag,
extradata, and per-packet pts/dts including the B-frame reorder case.

The Enhanced-RTMP `ExVideoTagHeader` / `ExAudioTagHeader` writers share
the same encoding source of truth as the parser via the new `to_bytes`
inverse: `ExFrameType::to_u8` / `ExPacketType::to_u8` /
`ExAudioPacketType::to_u8` / `AvMultitrackType::to_u8` round-trip every
nibble; `write_hevc_coded_frames` emits the SI24
`CompositionTimeOffset` between the FourCc and the NALU payload
(positive + negative + zero deltas all round-trip), and FourCc-mode
`SequenceStart` config records (`AV1CodecConfigurationRecord`,
`HEVCDecoderConfigurationRecord`, Opus RFC 7845 OpusHead, FLAC
STREAMINFO, AAC `AudioSpecificConfig`) reach `params.extradata`
verbatim. Multitrack emission is supported on the video side
(`OneTrack` / `ManyTracks` / `ManyTracksManyCodecs`). ModEx prefix
emission is supported on both the audio and video sides via the
shared `mod_ex::emit` writer: the lead byte's low nibble flips to `7`
(ModEx), each `mod_ex_entries[i]` lays down a size prefix + raw
payload + trailer (`subtype << 4 | next_packet_type`), and the last
entry's trailer carries the resolved `packet_type` — `walk ∘ emit`
recovers the same entries + accumulator (`TimestampOffsetNano` sums
match between writer and parser). Per-entry validation matches the
parser's invariants (raw size in `1..=65_536`, `TimestampOffsetNano`
raw `>= 3` bytes with UI24 BE matching `offset_ns`, `offset_ns
<= 999_999`). Multitrack emission is now supported on both the audio
and video sides for the spec's three `AvMultitrackType` variants
(`OneTrack` / `ManyTracks` / `ManyTracksManyCodecs`); the audio
parser surfaces the **inner** per-track `AudioPacketType` on
`ExAudioTagHeader::packet_type` (the outer `Multitrack` wrapper rides
on `multitrack` instead, mirroring the video header's shape), and
`to_bytes` lays the inner type into the multitrack outer byte. ModEx
prefix emission stacks cleanly with multitrack mode: the lead byte
advertises the ModEx sentinel and the final ModEx trailer carries
the resolved outer `Multitrack` value so `walk` exits on
`Multitrack` and the multitrack outer byte is read next.

The Enhanced-RTMP-v2 HDR `colorInfo` metadata frame has a typed
encode-side mirror of the demuxer's parser: a `color_info` module
exposes `ColorInfo` / `ColorConfig` / `HdrCll` / `HdrMdcv` structs
matching the spec's AMF object one-for-one (`bitDepth` + ISO 23091-4
indices; `maxFall` / `maxCLL` content light level; chromaticity
primaries + white point + mastering luminance per SMPTE ST 2086:2018).
Every field is `Option<…>` so producers emit only what they signal; a
populated struct passed to `tag::write_ex_video_color_info(w, ts,
fourcc, &ci)` lays down a `videoPacketType = Metadata` Ex video tag
whose AMF body matches the spec's `["colorInfo", Object]` pair shape.
Bounds checks (`hdrCll.*` in `[0.0001, 10_000]` cd/m^2; chromaticities
in `[0.0001, 0.7400]` for X / `[0.0001, 0.8400]` for Y; `maxLuminance`
in `[5, 10_000]`, `minLuminance` in `[0.0001, 5]`; `bitDepth` in
`[8, 16]`) run before any bytes reach the writer so out-of-range
values raise `Error::invalid` and the output buffer stays untouched.
`tag::write_ex_video_color_info_reset(w, ts, fourcc)` emits the
spec-recommended reset shape `["colorInfo", Undefined]`. The encoded
body is symmetric with the parser: a follow-up
`write_ex_video_color_info` replaces the prior `colorinfo.*` metadata
entries; a `write_ex_video_color_info_reset` drops them and leaves the
`metadata["colorinfo"] = "undefined"` sentinel — same shape the parser
observes when an external producer sends the same payload.

```rust
use oxideav_flv::{header, script, script::MetadataBag, tag};

# let mp3_frame: Vec<u8> = vec![];
let mut flv = Vec::new();
header::write(&mut flv, true, false)?; // audio-only
tag::write_first_previous_tag_size(&mut flv)?;
let meta = MetadataBag::new()
    .number("duration", 2.0)
    .number("audiosamplerate", 44_100.0)
    .boolean("stereo", true)
    .string("encoder", "oxideav-flv");
script::write_on_metadata(&mut flv, &meta)?;
tag::write_mp3_tag(&mut flv, 0, 3, true, true, &mp3_frame)?;
# Ok::<(), oxideav_core::Error>(())
```

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
