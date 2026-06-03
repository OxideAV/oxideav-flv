# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- ExVideo / ExAudio ModEx prefix emission (Veovera `enhanced-rtmp-v2`
  §`ExVideoTagHeader` / §`ExAudioTagHeader`, while
  `packet_type == ModEx = 7`). The muxer slice landed last round with
  `mod_ex_entries` required to be empty on emit (`Error::InvalidData`)
  because the inverse-emission step had no canonical-ordering source of
  truth. Now both `ExVideoTagHeader::to_bytes` and
  `ExAudioTagHeader::to_bytes` route through a shared
  `crate::mod_ex::emit` writer that walks the entries in order, lays
  down each entry's size prefix + raw payload + trailer byte
  (`(subtype_raw & 0x0F) << 4 | next_packet_type & 0x0F`), and chains
  the next-packet-type as `7` (ModEx) on every non-final entry so the
  parser's `walk` keeps looping. The final entry carries the resolved
  `packet_type` in its trailer's low nibble, so the parser exits with
  the same value the writer started with.

  - **Lead byte.** When `mod_ex_entries.is_empty()` the lead byte's
    low nibble is the resolved packet type (existing path). When
    non-empty, the low nibble is the ModEx sentinel `7`; the resolved
    type rides on the last trailer instead. Symmetric with how the
    parser reads the lead byte then immediately invokes `walk`.
  - **Multitrack interaction.** On the video side, when `multitrack`
    is `Some`, the resolved packet type in the trailer is
    `ExPacketType::Multitrack`. The parser's read-then-walk-then-
    multitrack-header flow is preserved; the multitrack outer header
    follows the ModEx chain just as it does in the no-ModEx case.
  - **Size encoding.** `encode_size` writes the spec's UI8 path for
    payloads of 1..=255 bytes (UI8 byte = `size - 1`, range 0..=0xFE)
    and the UI16 escape for 256..=65_536 (sentinel `0xFF` followed by
    UI16 BE `(size - 1)`). Sizes outside `1..=65_536` are rejected
    with `Error::InvalidData`. Sentinel-collision corner: payload
    length 256 cannot be expressed with a literal UI8 (would write
    `0xFF` → escape), so the boundary is strictly `<= 255` on the UI8
    side. The existing decoder test helper (`encode_one`) was off by
    one at this boundary and now matches the spec.
  - **Per-entry validation.** `mod_ex::emit` enforces the parser's
    invariants on every entry so internally-inconsistent headers are
    caught at emit time rather than producing wrong bytes:
    `subtype_raw` MUST fit in UB[4]; `TimestampOffsetNano` payloads
    MUST be `>= 3` bytes; the raw `[0..3]` UI24 BE MUST equal the
    typed `offset_ns`; `offset_ns <= 999_999` ns (spec cap); subtype
    `0` MUST go with the `TimestampOffsetNano` payload variant.
    Reserved-subtype payloads are passed through opaquely (the
    producer's bytes survive a round-trip even when neither the
    writer nor the reader know the subtype's semantics).
  - **`final_packet_type` contract.** `emit::<7>` rejects
    `final_packet_type == 7` (the caller's job is to aggregate every
    ModEx packet into the entries vector before invoking emit; a
    final ModEx-sentinel value would leave the parser hanging in the
    walk loop with no trailing data). Also rejects values outside
    UB[4].
  - **Internal-consistency guard on the headers.**
    `ExAudioTagHeader::to_bytes` and `ExVideoTagHeader::to_bytes`
    refuse `mod_ex_entries.is_empty() && timestamp_offset_nano != 0`
    — a non-zero accumulator with no entries is impossible to produce
    via the parser path and would silently lose the offset on emit.
  - 17 new unit tests (`src/mod_ex.rs`) cover the
    `encode_size` UI8/UI16 boundary cases, single-entry/two-entry
    chains round-tripping through `walk`, the 256-byte payload UI16
    path, reserved-subtype passthrough, the 999_999 ns boundary, and
    every invalid-input rejection (zero / over-range size, empty
    entries, out-of-nibble subtype, out-of-nibble final type,
    final type == ModEx sentinel, mismatched raw UI24, over-cap
    `offset_ns`, subtype-`TimestampOffsetNano` mismatch, short raw
    payload). Two new audio-side and four new video-side unit tests
    in `src/ex_audio.rs` / `src/ex_video.rs` cover the lead-byte
    nibble, single-entry + chain emission, HEVC `CodedFrames` with
    ModEx + SI24 CTO, multitrack-OneTrack with ModEx, and both
    inconsistency-rejection paths. Three new `tests/roundtrip_muxer.rs`
    integration tests build full FLVs through `write_ex_video_tag` /
    `write_ex_audio_tag` with ModEx-bearing headers and assert the
    demuxer recovers the resolved codec id, payload bytes, and pts
    on the AV1 / AAC / reserved-subtype paths.

- `onMetaData.keyframes` seek-table writer (spec §E.4.4 / §E.4.4.7 /
  §E.4.4.9). The legacy muxer slice's `MetadataBag` only modelled the
  three AMF0 scalar property types (Number / Boolean / String), so a
  muxer could not emit the parallel `filepositions[]` / `times[]` toc
  the demuxer harvests for the O(log n) bisect-seek path; producers
  using the writer had to fall through to the scan-forward seek path.
  Now:
  - New `MetaValue::Keyframes { file_positions: Vec<u64>,
    times_seconds: Vec<f64> }` variant + a `MetadataBag::keyframes`
    builder. The wire layout matches what `FlvDemuxer` parses on the
    read side: an anonymous AMF0 Object (`0x03`) keyed
    `"keyframes"`, carrying two parallel SCRIPTDATASTRICTARRAY
    (`0x0A`) properties in the spec-conventional order
    `filepositions` then `times`, terminated by the AMF0 object-end
    `0x00 0x00 0x09`.
  - New `amf0::write_strict_array_number` AMF0 primitive (marker
    `0x0A` + UI32 BE `StrictArrayLength` + N Number values, per
    §E.4.4.9). No terminating record follows the list.
  - Validation matches the demuxer's read-side invariants: both
    arrays non-empty and equal length, `times` finite and sorted
    ascending (non-decreasing — duplicate millisecond timestamps are
    legal when two keyframes share a ms), `filepositions` ≤ `2^53`
    so they survive the AMF0 Number lossless-integer round-trip.
    Violations error with `Error::InvalidData` rather than emitting
    a quietly-malformed toc.
  - `tests/roundtrip_muxer.rs` — two new integration tests build a
    three-keyframe H.263 FLV with the toc populated up front (via a
    two-pass mux that learns the metadata tag size, then computes
    the keyframe offsets, then re-emits the metadata tag with the
    real offsets) and assert: (a) the demuxer parses the composite
    silently into its internal seek table (no flatten leakage under
    `metadata["keyframes.…"]`); (b) `Demuxer::seek_to(40)` /
    `seek_to(70)` walk the bisect-left path — 40 ms lands on the
    exact entry, 70 ms bisects-left to the 40 ms entry rather than
    scan-forwarding to 80 ms. Seven new unit tests in
    `src/script.rs` cover the AMF0 wire round-trip through
    `parse_amf0_value`, length-mismatch / empty-array /
    non-monotonic-times / non-finite-time / above-2^53-position
    rejection, the equal-timestamp acceptance case, and the exact
    `2^53` boundary acceptance.

- Enhanced-RTMP v1 ExVideo / ExAudio muxer slice (Veovera
  enhanced-rtmp-v1 §"Defining Additional Video Codecs" + enhanced-rtmp-v2
  §"Enhanced Video" / §"Enhanced Audio"). The FourCc-mode wire format
  now round-trips through writer → demuxer for every spec-defined codec:
  - `ExVideoTagHeader::to_bytes` / `ExAudioTagHeader::to_bytes` —
    wire-byte inverses of the existing `parse` constructors, sharing
    one encoding source of truth with the demuxer. ExVideo supports
    single-track + multitrack (`OneTrack` / `ManyTracks` /
    `ManyTracksManyCodecs`) including the SI24
    `CompositionTimeOffset` slot for HEVC / VVC / AVC `CodedFrames`
    and the trailing `VideoCommand` UI8 for FrameType=5 (Command)
    non-Metadata. ExAudio covers single-track for this slice.
    Multitrack on the audio side is deferred until the parser
    surfaces the inner `AudioPacketType` (today only the outer
    Multitrack marker survives). ModEx prefix emission is deferred
    on both sides — `to_bytes` returns `Error::InvalidData` rather
    than emit incorrect bytes.
  - `ExFrameType::to_u8` / `ExPacketType::to_u8` /
    `ExAudioPacketType::to_u8` / `AvMultitrackType::to_u8` — inverses
    of the existing `from_u8` constructors so the writer and the
    parser share one nibble-encoding source of truth.
  - `tag::write_ex_video_tag(w, ts_ms, &ExVideoTagHeader, payload)`
    and `tag::write_ex_audio_tag(w, ts_ms, &ExAudioTagHeader, payload)`
    — generic ExHeader-then-payload writers.
  - Per-codec convenience writers covering every Enhanced-RTMP
    FourCc the demuxer accepts:
    - Video: `write_av1_sequence_start`,
      `write_av1_coded_frames`, `write_vp9_sequence_start`,
      `write_vp9_coded_frames`, `write_hevc_sequence_start`,
      `write_hevc_coded_frames` (with SI24 CTO),
      `write_hevc_coded_frames_x` (implicit zero CTO),
      `write_vvc_sequence_start`, `write_vvc_coded_frames`,
      `write_ex_video_sequence_end`, `write_ex_video_metadata`
      (HDR `colorInfo` AMF blob).
    - Audio: `write_opus_sequence_start`, `write_opus_coded_frames`,
      `write_flac_sequence_start`, `write_flac_coded_frames`,
      `write_ac3_coded_frames`, `write_eac3_coded_frames`,
      `write_mp3_ex_coded_frames` (FourCc `.mp3` path; distinct from
      the legacy `SoundFormat=2` writer), `write_aac_ex_sequence_start`
      / `write_aac_ex_coded_frames` (FourCc `mp4a` path),
      `write_ex_audio_sequence_end`.
  - `tests/roundtrip_muxer.rs` — 14 new tests assert the writer →
    demuxer round-trip for `av1` / `vp9` / `h265` (CodedFrames +
    CodedFramesX) / `h266` / `opus` / `flac` / `ac3` / `eac3` /
    `aac` / `mp3` (FourCc) plus `Metadata` (header+discard) and
    `SequenceEnd` (empty body). Each test asserts the demuxer's
    `params.codec_id` resolves to the canonical short id and that
    `SequenceStart` config records land in `params.extradata`
    verbatim. HEVC `CodedFrames` validates the SI24 CTO carries
    positive + zero + negative composition-time offsets through to
    `pts = dts + CTO`.
  - Unit `to_bytes` tests in `src/ex_video.rs` + `src/ex_audio.rs`
    cover the "parse → emit → re-parse" path for every header
    combination supported by this slice and assert byte-for-byte
    equality on the canonical wire shapes (lead byte + FourCc +
    optional CTO + optional VideoCommand). Negative tests cover
    every spec-invalid combination the writer must reject
    (out-of-range CTO, missing CTO, CTO on non-HEVC codec,
    missing FourCc, mismatched multitrack codec slot, ModEx
    emission with a non-empty `mod_ex_entries`).

- Legacy video tag muxer slice (spec §E.4.3 / §E.4.3.1). The audio-only
  first muxer slice now extends to H.263 / VP6 / VP6A / AVC video, so a
  full audio+video FLV writes through the same writer family and the
  demuxer recovers it bit-exactly:
  - `tag::write_video_tag(w, ts_ms, VideoTagHeader, payload)` — generic
    one-byte `FrameType | CodecID` header followed by `VIDEODATA`,
    wrapped in a complete FLV tag.
  - `tag::write_h263_tag(w, ts_ms, is_keyframe, frame)` — Sorenson H.263
    (`flv1`, CodecID 2) writer; passes the bitstream frame verbatim as
    `VIDEODATA`.
  - `tag::write_vp6_tag(w, ts_ms, is_keyframe, frame)` — VP6 (`vp6f`,
    CodecID 4) writer.
  - `tag::write_vp6a_tag(w, ts_ms, is_keyframe, alpha_offset, frame)` —
    VP6-with-alpha (`vp6a`, CodecID 5) writer, prefixing the spec
    `AlphaOffset` UI8 to the BGR+alpha sub-stream. The demuxer route
    that lifts the alpha-offset byte into `params.extradata` round-trips
    through this writer.
  - `tag::write_avc_sequence_header(w, ts_ms, config_record)` — AVC
    (`h264`, CodecID 7) `AVCPacketType = 0` writer; the
    `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15) reaches
    `params.extradata` verbatim after the round-trip.
  - `tag::write_avc_nalu_tag(w, ts_ms, is_keyframe, composition_time_ms,
    access_unit)` — AVC `AVCPacketType = 1` writer; packs the signed
    24-bit `CompositionTime` (`pts - dts`, in milliseconds) and rejects
    deltas outside `-2^23..=2^23 - 1` with `Error::InvalidData` rather
    than truncating.
  - `tag::write_avc_end_of_sequence(w, ts_ms)` — AVC `AVCPacketType = 2`
    end-of-sequence sentinel; one-byte body, `CompositionTime = 0`.
  - `tag::write_video_info_command_tag(w, ts_ms, VideoInfoCommand)` —
    FrameType=5 video-info / command tag (spec E.4.3.1, IF
    FrameType==5); emits the UI8 command byte (StartClientSeek /
    EndClientSeek / `Unknown(u8)` passthrough).
  - `VideoTagHeader::to_byte` / `FrameType::to_u8` /
    `VideoInfoCommand::to_u8` — wire-byte inverses of the existing
    `parse` / `from_u8` constructors so the new writers share one
    encoding source-of-truth with the demuxer.
  - `tests/roundtrip_muxer.rs` — four new video tests write a
    video-only FLV with each codec and assert
    `streams[0].params.codec_id` (`flv1` / `vp6f` / `vp6a` / `h264`),
    keyframe flags, `extradata` (VP6A alpha offset, AVC config record),
    and per-packet pts/dts including the B-frame reorder case
    (positive + negative SI24 CTS).

- First muxer slice (spec Annex E, AMF0 §2). New write-side surface that
  round-trips bit-exactly through `FlvDemuxer`:
  - `header::write` — the 9-byte file header (signature / version /
    TypeFlags / DataOffset, §E.2).
  - `tag::write_first_previous_tag_size`, `tag::write_tag` — the leading
    `PreviousTagSize0` and the 11-byte tag header + body + trailing
    `PreviousTagSize` framing (§E.3 / §E.4.1). `write_tag` returns the
    total tag size for offset bookkeeping.
  - `tag::write_audio_tag` / `tag::write_mp3_tag` /
    `tag::write_aac_raw_tag` and `AudioTagHeader::to_byte` — audio tag
    emit for legacy MP3 (`SoundFormat 2`) and raw AAC
    (`SoundFormat 10`, `AACPacketType 1`), §E.4.2.1 / §E.4.2.2.
  - AMF0 writers (`amf0::write_number` / `write_boolean` /
    `write_string` / `write_property_name` / `write_object_start` /
    `write_ecma_array_start` / `write_object_end`).
  - `script::MetadataBag` + `script::write_on_metadata` — an ordered
    bag of Number / Boolean / String properties serialised as an
    `onMetaData` script tag (TagType `0x12`, §E.4.4 / §E.5).
  - `tests/roundtrip_muxer.rs` — writes header + `onMetaData` + N MP3
    tags, demuxes the buffer, and asserts the header flags, metadata
    keys, `duration_micros`, and every audio body survive byte-for-byte.

## [0.0.4](https://github.com/OxideAV/oxideav-flv/compare/v0.0.3...v0.0.4) - 2026-05-29

### Other

- preserve unknown script-tag argument payloads via flatten
- preserve top-level onMetaData fields outside the known schema
- full Adobe AMF 3 decoder + AMF0 0x11 AVM+ switch
- injection-robustness suite + read_body pre-alloc guard
- TypedObject / XMLDocument / Unsupported markers
- lift onMetaData audiosamplesize into sample_format
- parse onMetaData audioTrackIdInfoMap / videoTrackIdInfoMap
- parse E-FLV VideoPacketType.Metadata HDR colorInfo into metadata bag
- E-FLV multitrack body splitter + default-track routing
- Enhanced-RTMP VideoCommand UI8 on the Ex video path
- typed E-FLV ModEx walk (Enhanced RTMP v2 §ModEx)
- Enhanced RTMP / E-FLV ExAudioTagHeader (Opus/FLAC/AC-3/E-AC-3/MP3/AAC FourCC)
- Enhanced RTMP / E-FLV ExVideoTagHeader (AV1/VP9/HEVC/VVC/VP8 + AVC FourCC)
- FLV encryption (Annex F) + FrameType=5 + AMF0 Reference + onMetaData enrichment
- add Demuxer::seek_to with keyframes-toc + scan-forward paths

### Added

- Unknown-script-name argument preservation. The `dispatch_script_tag`
  arm that catches non-spec script-tag names used to record only the
  method name under `metadata["scriptdata.name"]` and silently drop the
  AMF0 argument payload. FLV spec v10.1 enumerates only four
  spec-defined names (`onMetaData` E.5, `onXMPData` E.6, `onCuePoint`
  Annex A, `|AdditionalHeader` F.2.1), but Enhanced-RTMP-v2 §"Enhancing
  onMetaData" describes the ScriptTagBody as encapsulating method
  invocations (method name + single argument), so producers in the
  wild legitimately emit additional method names (live-caption tracks,
  producer telemetry, RTMP-relayed status snapshots, …). The argument
  is now lifted through the existing `flatten_amf_value` walker under
  a `scriptdata.<name>.<...>` prefix — scalars (Number / Boolean /
  String / Null / Undefined / Unsupported / Reference / Xml / Date)
  land directly under `scriptdata.<name>`, composite values
  (Object / EcmaArray / TypedObject / StrictArray / AvmPlus) fan out
  with `.<subkey>` / `[i]` suffixes per the existing flatten schema.
  The legacy `metadata["scriptdata.name"] = <name>` sentinel is
  preserved so existing callers that only checked the name see no
  surface change. Three new unit tests cover the string-argument,
  nested-object-argument, and Null-argument variants; previously
  every non-spec script tag emitted exactly one metadata entry
  regardless of how rich its payload was.

- `parse_on_metadata` fall-through preservation. Previously, a top-level
  `onMetaData` property whose value didn't match Number / Boolean /
  String — or wasn't a recognised composite key (`keyframes`,
  `audioTrackIdInfoMap`, `videoTrackIdInfoMap`) — was silently dropped
  from `Demuxer::metadata`. AMF0 Null (§2.7), Undefined (§2.8), Date
  (§2.13), Reference (§2.14), Unsupported (§2.15), XMLDocument (§2.17),
  StrictArray (§2.12), the AVM+ AMF3 sub-tree (§3.1), and any
  producer-defined nested Object/EcmaArray/TypedObject outside the
  known schema went unrecorded. The fall-through arm now lifts every
  unmatched variant through `flatten_amf_value` under its original
  property name, so callers reading the metadata bag see the
  producer's full top-level surface. Worked examples: an Annex E.5
  `creationdate` Date lands as
  `metadata["creationdate"] = "date:<millis>tz:<offset>"`; a producer
  `producerInfo` sub-object lifts each leaf under
  `metadata["producerInfo.name"]` / `metadata["producerInfo.buildno"]`
  paths; explicit Null / Undefined sentinels surface as the strings
  `"null"` / `"undefined"`. Three new unit tests cover the Date,
  nested-object, and Null/Undefined variants. Existing
  `on_metadata_unsupported_value_does_not_drop_neighbouring_fields`
  test (the `0x0D` Unsupported case) still passes — the change is
  strictly additive on top of every previously-recognised path.

- AMF3 (Adobe AMF 3 Specification, December 2007) decoder. The AMF0
  `0x11` AVM+ switch marker (AMF0 spec §3.1) used to hard-error inside
  `parse_amf0_value`; it now decodes the following bytes through the
  full AMF3 grammar and surfaces them as
  `AmfValue::AvmPlus(Box<Amf3Value>)`. The new `src/amf3.rs` module
  covers all 13 type markers (§3.1 table: undefined / null / false /
  true / integer / double / string / xml-doc / date / array / object /
  xml / byte-array) with the three implicit reference tables (strings,
  complex-objects, traits) per §2.2 + §3.8 + §3.12. The U29 variable-
  length unsigned 29-bit integer primitive (§1.3.1) decodes all four
  byte-length forms and proves the `2^29 - 1` cap is exact (the
  four-byte form's 7+7+7+8 = 29 bits cannot overflow by construction).
  UTF-8-vr (§1.3.2) preserves the empty-string-never-sent-by-reference
  rule. Trait blocks support inline (sealed_count + dynamic + class
  name + sealed property names), traits-ref (back-reference to a prior
  trait entry), and traits-ext (externalizable — the flag is set and
  zero body bytes consumed since the spec gives no parser recipe;
  callers that know the class's private grammar can decode the
  trailing bytes themselves). Object back-references decode to an
  alias of the prior instance; circular graphs are handled by
  reserving the object-table slot before descending into Array /
  Object children. The demuxer's `flatten_amf_value` walker lowers
  through a symmetric `flatten_amf3_value` companion so an AVM+ value
  reached via `onCuePoint` or `VideoPacketType.Metadata` surfaces
  under the same `metadata["prefix.key"]` shape as its AMF0
  counterpart (numbers/strings/booleans flatten verbatim; Date as
  `date:<ms>`; ByteArray as `bytearray:<len>`; Object class alias
  under `.class`; externalizable under `.externalizable=true`; Array
  assoc keys under `.key` and dense slots under `[i]`). 26 new
  unit tests cover U29 boundary round-trips, integer sign-extension
  at 2^29-1 → -1, string literal/reference, empty-string-never-ref,
  date literal, array with both assoc and dense portions, anonymous
  dynamic object, typed sealed Point(x, y) object, traits-ref
  second-instance reuse, object-ref aliasing, externalizable opaque
  flag, both XML markers, byte-array literal/ref, and truncation-
  rejection of every variable-length primitive.
- Injection-robustness regression suite (`tests/injection_robustness.rs`,
  18 hand-crafted adversarial blobs). Each input forges a different
  parser lever — empty / truncated file header, bad signature, forged
  oversize `DataSize` (the 16 MB OOM lever), off-by-one body truncation,
  missing `PreviousTagSize` trailer, unknown AMF0 type markers, truncated
  AMF0 strings, `LongString` claiming `u32::MAX` length, unterminated
  Object body, non-object `onMetaData` value, unknown `TagType`, forged
  Filter-flag with truncated preamble, zero-length audio tag, mid-stream
  truncation after discovery, and a flood of zero-size tags — and asserts
  the parser either errors cleanly with `Error::InvalidData` / `Error::Eof`
  / `Error::Io`, or degrades to a stream that terminates on the first
  `next_packet()`. No panics, no OOM, no infinite loops. Hermetic — no
  on-disk fixtures.
- AMF0 Typed Object (marker `0x10`, AMF0 spec §2.18), XML Document
  (marker `0x0F`, §2.17), and Unsupported (marker `0x0D`, §2.15)
  parsing. The three markers used to hard-error inside `parse_amf0_value`,
  which would silently drop the entire `onMetaData` payload whenever a
  producer chose any of them. `AmfValue` grows three variants:
  `TypedObject { class_name, body }` carries both the registered class
  alias and the same property body shape as anonymous `Object`;
  `Xml(String)` preserves the raw XMLDocument bytes; `Unsupported`
  records the spec sentinel for "I cannot serialise this value." The
  `AmfValue::get` lookup now also looks into `TypedObject` bodies, and
  the new `AmfValue::class_name` accessor exposes the alias when
  present. `parse_on_metadata` and `xmp_liveXML` accept `TypedObject`
  in the same role as `Object` / `EcmaArray`, so an FMS / Wowza relay
  that wraps `onMetaData` in a class-aliased object no longer hides
  `videodatarate` / `width` / `keyframes` from the demuxer; the alias
  itself surfaces under `metadata["scriptdata.class"]`. `xmp_liveXML`
  also accepts the `Xml` marker form of `liveXML`. The flatten path
  used by `onCuePoint` / `colorInfo` learned to format all three new
  variants (`unsupported`, the raw XML body, and a `.class` sentinel
  followed by the typed body). AMF3 (marker `0x11`, §3.1) still
  errors loudly — no AMF3 decoder yet, see followups.
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

### Fixed

- Pre-allocation guard on `read_body` (the tag-payload reader). The FLV
  `DataSize` field is a UI24 (max 16 777 215), and the old code blindly
  committed `vec![0u8; size as usize]` before `read_exact` could surface
  `UnexpectedEof`. A forged tag header on a 30-byte file would therefore
  pre-allocate ~16 MB. The new path probes the underlying `Read + Seek`
  stream's length once and rejects any `DataSize` that exceeds remaining
  bytes with `Error::InvalidData("FLV tag: DataSize N exceeds remaining
  stream bytes M")` — turning the OOM vector into a cheap structural
  rejection before any allocation occurs. Falls back to "trust the size
  and let `read_exact` error" on streams that don't support seeking-to-end
  (none of our `Box<dyn ReadSeek>` inputs in practice).

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
