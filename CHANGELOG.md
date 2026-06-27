# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

- ModEx timestamp-offset / entry-chain demuxer side-channel (`FlvDemuxer::last_timestamp_offset_nano` / `last_mod_ex_entries`) — closes a demux↔mux asymmetry on the Enhanced-RTMP-v2 §`ModEx` prefix. The `ExAudioTagHeader` / `ExVideoTagHeader` parsers already accumulate the `TimestampOffsetNano` sub-millisecond refinement (0..=999_999 ns, "just over 1 ms" of precision) and preserve every ModEx entry (including reserved/future subtypes with their raw bytes), but `next_packet` could only fold the integer-millisecond RTMP timestamp into `Packet::pts`/`dts` — the core `Packet` carries no nanosecond field, so the nano refinement and the full ModEx chain were dropped. Two new `FlvDemuxer` accessors mirror the existing `last_multitrack_tracks` side-channel: `last_timestamp_offset_nano()` returns the accumulated nanosecond offset of the tag the most-recently-emitted packet was built from (the nano-refined presentation time is `pts_ms * 1_000_000 + offset_ns`), and `last_mod_ex_entries()` returns the full `&[ModExEntry]` chain so a remuxer can feed them straight back into `mod_ex::emit` / the `mod_ex_entries` writer field for byte-exact re-emission. Both refresh on every `next_packet` and clear for any tag without a ModEx prefix (no stale bleed onto later packets); tags that produce no packet (`SequenceEnd`, script tags) leave the captured value undisturbed. 3 new `tests/roundtrip_muxer.rs` integration tests drive the mux → demux side-channel loop (video nano offset + clear-on-header, chained audio 100+200 ns + clear-on-plain-tag, reserved-subtype passthrough). `open_demuxer_concrete` / `FlvDemuxer` re-exported for the concrete accessors
- `onCuePoint` muxer typed parameters — the cue-point writer modelled the `parameters` Object as `(name, String)` pairs only, but the demuxer flattens cue parameters through `flatten_amf_value`, which handles any AMF0 value type. `CuePointParams::parameters` is now a `Vec<(String, MetaValue)>`; the existing `.parameter(name, &str)` string fast-path is preserved (stores a `MetaValue::String`), and a new `.parameter_typed(name, MetaValue)` setter accepts a Number / Boolean / Date / nested Object / array parameter. The writer routes parameters through the shared recursive `write_meta_value` serialiser, so a typed cue parameter round-trips under `metadata["cuepoint.<n>.parameters.<key>"]` (composite values fan out with `.<subkey>` / `[i]`). 1 new `script::tests::*` unit test + 1 new `tests/roundtrip_muxer.rs` integration test
- `onMetaData` muxer AMF0 value-type completeness — the muxer's `MetaValue` could emit Number / Boolean / String / Date / Object / EcmaArray-map / Keyframes, but not the AMF0 Null (`0x05`) / Undefined (`0x06`) / XMLDocument (`0x0F`) scalars or a mixed-type EcmaArray (`0x08`) / StrictArray (`0x0A`) value the demuxer's `flatten_amf_value` walk reads back (Null → `"null"`, Undefined → `"undefined"`, Xml verbatim, StrictArray → `metadata["<key>[i]"]`). New `MetaValue::{Null, Undefined, Xml, EcmaArray, StrictArray}` variants wired into the recursive `write_meta_value` serialiser, plus `MetadataBag::{null, undefined, xml, strict_array}` + `ObjectBuilder::{null, undefined, xml}` builders. New AMF0 writer primitives `amf0::write_undefined` / `write_xml` / `write_strict_array_start` (mixed-type, no terminator — UI32 length is the sole delimiter). 3 new `script::tests::*` unit tests + 1 new `tests/roundtrip_muxer.rs` integration test drive the full mux → demux flatten loop. With the Object/track-map slice this closes the AMF0 value-type matrix the muxer can emit against the matrix the demuxer parses
- `onMetaData` muxer parity for nested objects + Enhanced-RTMP-v2 per-track info maps — closes a demux↔mux asymmetry where the demuxer flattened producer sub-objects (HDR config, telemetry) and the §"Enhancing onMetaData" `videoTrackIdInfoMap` / `audioTrackIdInfoMap` per-track maps, but the muxer's `MetadataBag` could only emit scalars + the `keyframes` composite. New `MetaValue::Object(Vec<(String, MetaValue)>)` variant + recursive `write_meta_value` serialiser emit an anonymous AMF0 Object the demuxer's `flatten_amf_value` walk reads back under `metadata["<key>.<subkey>"]`. New `MetadataBag::object` builder + `ObjectBuilder` ergonomic nested-object constructor. New typed `TrackInfoMap` / `TrackInfo` builders + `MetadataBag::video_track_info_map` / `audio_track_info_map` emit the spec-example map shape (trackId-keyed sub-objects with `width` / `height` / `videodatarate` / `framerate` / `videocodecid` on the video side; `audiodatarate` / `channels` / `samplerate` / `audiocodecid` on the audio side) under the spec property names the demuxer flattens and `TypedMetadata::video_track_info_map` / `audio_track_info_map` re-type. Delta-style entries (only the differing fields) are first-class; cross-kind fields (an audio `channels` on a video entry) are dropped on emit; trackIds serialise as the decimal string keys. 7 new `script::tests::*` unit tests + 3 new `tests/roundtrip_muxer.rs` integration tests drive the full mux → demux → `TypedMetadata` loop (FourCc-packed `videocodecid: makeFourCc("av01")` round-trips to `"av1"`, `audiocodecid: makeFourCc("Opus")` to `"opus"`). `MetaValue::Object`, `ObjectBuilder`, `TrackInfo`, `TrackInfoMap` re-exported from the crate root
- `amf3_roundtrip` fuzz target (fifth fuzz target) — differential test of the AMF3 encoder against the decoder: decode arbitrary bytes, re-encode any accepted value via `write_amf3_value`, decode again, and assert the value is unchanged and the canonical encoding is a fixed point (`encode == encode ∘ decode ∘ encode`). A deterministic sibling (`encode_is_a_fixed_point_for_a_value_corpus`) runs the same invariant over a 12-shape corpus in CI
- AMF0 AVM+ switch writer (`amf0::write_avm_plus`) — the write-side inverse of the `AmfValue::AvmPlus` parse arm (AMF0 spec §3.1): emits the `0x11` AVM+ switch marker then serialises an `Amf3Value` via the new `amf3::write_amf3_value`, so a producer can round-trip an AMF3-encoded `onMetaData` / `onCuePoint` payload back through `parse_amf0_value` (which the demuxer already lifts + flattens). 2 new amf0 unit tests (object round-trip through the switch + exact-bytes scalar)
- AMF3 encoder (`amf3::write_amf3_value`) — the inverse of `parse_amf3_value`, closing the AMF3 read↔write loop (AMF 3 Specification, Dec 2007). Serialises all 13 type markers (undefined / null / false / true / integer / double / string / xml-doc / date / array / object / xml / byte-array) from an `Amf3Value` tree. Literal-first: every complex value is emitted as a fresh literal (the tree model has no shared identity to dedup), but non-empty string literals (§3.8) are deduped through a write-side string table that mirrors the decoder's encounter order, so `parse ∘ write` and `write ∘ parse` are both identity (incl. the canonical-bytes test where a decoded reference-using stream re-encodes to its original bytes). U29 (§1.3.1) emitter matches the decoder's `read_u29` byte-for-byte; the signed-29-bit `integer-type` (§3.6) round-trips its full `-2^28 ..= 2^28-1` range (`-1` → `U29_MAX`) and rejects out-of-range ints (would be a Double); inline-traits Object headers carry sealed-count + dynamic/externalizable flags; externalizable objects emit the flag + class name with zero body bytes (matching the no-recipe decode stance); empty assoc / dynamic keys are rejected (they would be read as the run terminator). 21 new encoder unit tests
- AMF0 Date / SCRIPTDATADATE muxer support (spec §E.4.4.3): new `amf0::write_date(w, time_ms, tz)` writer — marker `0x0B` + 8-byte BE `DateTime` (ms since 1970 UTC) + SI16 BE `LocalDateTimeOffset` (minutes from UTC, negative west of Greenwich) — and a `MetadataBag::date(key, time_ms, tz)` builder + `MetaValue::Date { time_ms, tz }` variant, closing the read↔write loop for the demuxer's existing AMF0 Date parse path. A `creationdate` stamped through `.date(..)` surfaces under the demuxer's `"date:<ms>tz:<offset>"` carrier and decodes back via `TypedMetadata::creationdate_as_date`; a non-finite `time_ms` is rejected with `Error::InvalidData` before any bytes are emitted. 2 amf0 unit tests + 2 script unit tests + 2 mux→demux→typed-accessor integration tests (positive + negative offset)
- Enhanced-RTMP-v2 `connect` per-codec capability queries (`ConnectCommandObject`): `video_codec_caps` / `audio_codec_caps` resolve the effective `FourCcInfoMask` for a FourCC honouring the §"Enhancing NetConnection connect Command" wildcard-override rule (a `"*"` key OR-folds its flags into every codec — e.g. `"*": CanForward` reports `CanForward` for all codecs regardless of their individual entries), with `can_decode_video` / `can_encode_video` / `can_forward_video` + audio equivalents answering the spec's per-codec capability question directly. Adds `supports_mod_ex` / `supports_timestamp_nano_offset` to complete the `CapsExMask` query set alongside `supports_reconnect` / `supports_multitrack`
- Legacy AAC sequence-header muxer writer (`tag::write_aac_sequence_header`): the read-side inverse of the demuxer's `AACPacketType == 0` AudioSpecificConfig extraction and the legacy-codec sibling of `write_aac_raw_tag` (raw-AU writer) / `write_aac_ex_sequence_start` (Enhanced-RTMP FourCc-mode). Lays out the spec §E.4.2.2 `SoundFormat=10` `0xAF` header + `AACPacketType=0` + the ISO 14496-3 `AudioSpecificConfig` verbatim; round-trips back into `params.extradata` through `FlvDemuxer`
- Enhanced-RTMP-v2 NetConnection `connect` command (`connect` module): typed `ConnectCommandObject` builder for the §"Enhancing NetConnection connect Command" capability declaration — `fourCcList` (legacy + `["*"]` wildcard), `videoFourCcInfoMap`/`audioFourCcInfoMap` (`FourCcInfoMask` CanDecode/CanEncode/CanForward), and `capsEx` (`CapsExMask` Reconnect/Multitrack/ModEx/TimestampNanoOffset), plus extra-property preservation, AMF0 command serialiser, and `parse_connect_command` read↔write loop; adds `amf0::write_strict_array_string`
- Enhanced-RTMP-v2 NetConnection `onStatus` command + *Reconnect Request* (`on_status` module): typed `OnStatusInfo` builder, `reconnect_request` constructor (spec-pinned `code`/`level`, optional `tcUrl`/`description`), AMF0 command serialiser, and `parse_on_status_command` read↔write loop; adds `amf0::write_null`
- `join_tracks` multitrack body serialiser — the write-side inverse of `split_tracks` (`OneTrack` / `ManyTracks` / `ManyTracksManyCodecs`), with `JoinTrack` input and `split↔join` round-trip tests

## [0.0.5](https://github.com/OxideAV/oxideav-flv/compare/v0.0.4...v0.0.5) - 2026-06-14

### Other

- order-agnostic speaker-presence queries
- resolve FourCc-packed videocodecid / audiocodecid in TypedMetadata
- expose non-default multitrack tracks via FlvDemuxer::last_multitrack_tracks
- surface Enhanced-RTMP-v2 audio silence message as a discard packet
- parse + emit Enhanced-RTMP-v2 MultichannelConfig speaker layout
- TypedColorInfo::to_color_info read↔write rebuild for Enhanced-RTMP-v2 HDR colorInfo
- TypedColorInfo read-side mirror for Enhanced-RTMP-v2 HDR colorInfo
- TypedMetadata accessors for Enhanced-RTMP-v2 per-track info maps
- TypedMetadata videoframerate alias + effective_framerate accessor
- drop release-plz.toml — use release-plz defaults across the workspace
- onCuePoint + onXMPData script-data tag writers
- fuzz/ sub-crate — demuxer / AMF0 / AMF3 / script-metadata roundtrip
- typed onMetaData accessors for the Annex E.5 fifteen properties
- HDR colorInfo encode-side wiring (Enhanced-RTMP v2 Metadata Frame)
- ExAudio multitrack emission + parser inner-AudioPacketType surfacing
- ExVideo / ExAudio ModEx prefix emission
- onMetaData.keyframes seek-table writer
- Enhanced-RTMP ExVideo + ExAudio muxer slice (FourCc mode)
- legacy video tag muxer — H.263 / VP6 / VP6A / AVC writers
- drop stale src/writer.rs swept into prior commit

### Added

- `MultichannelConfig::present_channels()` / `has_channel()` /
  `present_channel_labels()` — order-agnostic speaker-presence queries
  for the Enhanced-RTMP-v2 `AudioPacketType.MultichannelConfig` body.
  The spec poses a single question — "to see if a specific audio channel
  is present" — but answers it two ways: `Native` order via the
  `audioChannelFlags & AudioChannelMask.xxx` bitmask test, `Custom`
  order via the explicit per-channel mapping. These helpers unify both
  into one set of present speakers (`Custom` excludes
  `AudioChannel::Unused` — the spec's "empty, can be safely skipped"
  channel; reserved high mask bits outside the 24-bit `AudioChannelMask`
  range carry no speaker). `Unspecified` / reserved orders report no
  present channels. Complements the existing `mask_channel_labels`
  (which only covered the `Native` bitmask form).
- `tag::video_codec_id_str_u32` / `tag::audio_codec_id_str_u32` —
  codec-id resolvers that accept both `onMetaData` `videocodecid` /
  `audiocodecid` encodings the Enhanced-RTMP-v2 §"Enhancing onMetaData"
  extension allows: a legacy 4-bit CodecID (E.4.3.1 / E.4.2.1) **or** a
  packed FourCc UI32 stamped via `makeFourCc()`. A FourCc-packed value
  (e.g. `"av01" == 0x61763031 == 1_635_135_537`, `"Opus" == 0x4F707573`)
  is decoded as a big-endian FourCc and routed through the same resolver
  the wire-side ExVideo / ExAudio path uses, so it surfaces as `"av1"` /
  `"opus"` / `"h265"` / … instead of the prior useless
  `flv:video:1635135537` raw-integer carrier.
- `TypedVideoTrackInfo::video_codec_id_str()` /
  `TypedAudioTrackInfo::audio_codec_id_str()` — per-track string forms of
  the codec id, resolving the same legacy-or-FourCc encoding so the
  Enhanced-RTMP-v2 per-track info-map example
  (`videocodecid: makeFourCc("av01")`) reads back as `"av1"`.

### Changed

- `TypedMetadata::video_codec_id_str` / `audio_codec_id_str` now resolve
  a FourCc-packed `videocodecid` / `audiocodecid` to the canonical codec
  string instead of emitting a `flv:video:<N>` / `flv:audio:<N>`
  raw-integer carrier.

### Added (multitrack)

- Enhanced-RTMP **non-default multitrack track** read access
  (§"Track Ordering", §`ExAudioTagBody` / §`ExVideoTagBody`).
  `next_packet` emits only the default track (trackId 0 / first in wire
  order) of a multitrack Ex tag, so the other variants — different
  bitrate, resolution, codec, language, or camera angle — used to be
  dropped from the single-stream packet flow. `FlvDemuxer` now keeps
  every track of the most-recently-emitted multitrack tag and exposes
  them via `last_multitrack_tracks() -> Option<&[MultitrackPacketTrack]>`.
  Each `MultitrackPacketTrack` carries `track_id`, the codec `fourcc` +
  resolved `codec_name`, and an *owned* copy of that track's coded
  payload (the SI24 CompositionTimeOffset is preserved for AVC/HEVC/VVC
  `CodedFrames`), so a receiver implementing its own track-selection
  logic recovers a non-default variant without re-parsing the Ex header
  or re-running `split_tracks`. The side-channel refreshes on every
  `next_packet` and clears to `None` the moment a single-track tag is
  emitted, so it never reports stale tracks. To reach the concrete
  accessor, `demuxer::open_concrete` (re-exported as
  `open_demuxer_concrete`) returns the concrete `FlvDemuxer`; the
  registry `open` wraps it in a `Box<dyn Demuxer>` unchanged. New public
  type `MultitrackPacketTrack`. 3 new unit tests (video ManyTracks +
  audio ManyTracksManyCodecs with single-track clear + OneTrack).
- Enhanced-RTMP-v2 **audio silence message** handling (§`AudioPacketType`).
  An audio tag with a zero-length payload (an empty audio message
  without an `AudioTagHeader`) signals a period of silence whose
  spec-defined playback semantics are to drain buffered audio, flush
  the audio decoder, and stop using the audio clock as the A/V-sync
  master for the silence period (declared to have "no less than the
  same meaning as" `SequenceEnd`). Previously the demuxer dropped such
  a tag silently in `build_audio_packet`; it now surfaces it — when an
  audio stream is already established — as a zero-length
  `header = true` + `discard = true` packet at the tag's timestamp, so
  callers can react to the silence boundary (flush their decoder /
  switch to wall-clock timing) instead of seeing nothing. The empty
  body never reaches a decoder as a frame, and playback resumes on the
  next real audio tag. A silence tag preceding any real audio tag still
  mints no stream (silence carries no codec) and is skipped during
  discovery, unchanged. 1 new unit test; the existing
  `zero_length_audio_tag_is_skipped_not_panic` /
  `flood_of_zero_size_tags_terminates` robustness tests remain green
  (the silence packet only emits once an audio stream exists).
- Enhanced-RTMP-v2 `AudioPacketType.MultichannelConfig` body parsing +
  emission (new `multichannel` module). The previously-opaque speaker
  layout signal (§`ExAudioTagBody`) now decodes into a typed
  `MultichannelConfig` struct — `AudioChannelOrder`
  (`Unspecified` / `Native` / `Custom` / reserved-preserving),
  `channel_count`, the `Custom` per-channel `AudioChannel` speaker map
  (all 24 spec positions through 22.2 / SMPTE ST 2036-2-2008, plus
  `Unused` 0xFE / `Unknown` 0xFF / reserved), and the `Native`
  `audioChannelFlags` UI32 presence mask. The demuxer harvests the
  parsed config (multitrack-aware: the default track's payload is
  unwrapped first) into `metadata["multichannelconfig.order" /
  ".channelcount" / ".flags" / ".layout" / ".mapping"]` — latest signal
  supersedes the prior one — and lifts the channel count into
  `CodecParameters::channels` (the spec's channel-mapping truth for
  codecs that are not self-describing; the `onMetaData` `stereo`
  boolean can only express 1 or 2). The tag itself still surfaces as a
  header+discard packet. Write side:
  `tag::write_ex_audio_multichannel_config(w, ts, fourcc, &config)` +
  `MultichannelConfig::to_bytes` validate order/field consistency
  (Custom mapping length == channelCount, Native mask popcount ==
  channelCount with no reserved high bits, reserved orders rejected)
  before any bytes are emitted; the parser stays lenient on all three
  so callers see exactly what a producer signalled. 12 unit tests +
  2 demuxer tests + 2 mux→demux round-trip integration tests.
- `TypedColorInfo::to_color_info()` — reconstructs the encode-side
  [`color_info::ColorInfo`] struct from the read view in one call,
  closing the read↔write symmetry loop for the Enhanced-RTMP-v2
  §"Metadata Frame" HDR `colorInfo` signalling. The struct it returns,
  fed back through `ColorInfo::encode_amf` /
  `tag::write_ex_video_color_info`, re-emits the same
  `["colorInfo", Object]` AMF body the demuxer parsed (modulo fields the
  producer stamped out-of-range, which the read view drops to `None` and
  so do not survive the rebuild). Each of the three groups
  (`colorConfig` / `hdrCll` / `hdrMdcv`) is populated to `Some(..)` only
  when at least one of its fields survives as a finite, in-range value —
  mirroring the encode-side convention that an all-`None` group is
  omitted from the AMF object entirely. A reset sentinel
  (`is_reset_sentinel() == true`) and an all-fields-malformed frame both
  rebuild to `ColorInfo::default()`, which the encoder emits as the
  spec's empty-object reset shape. Four `typed_meta::tests::*` unit
  tests cover the full-payload rebuild, the absent-group omission, the
  reset-sentinel default rebuild, and the out-of-range field drop; two
  new `tests/roundtrip_muxer.rs` integration tests drive the full
  mux → demux → `to_color_info()` loop (fully-populated equality + the
  Undefined reset default rebuild).
- `TypedMetadata::color_info()` accessor returning
  `Option<TypedColorInfo>` — a borrowed read-side mirror of the
  encode-side [`color_info::ColorInfo`] struct that re-types every
  populated field of the Enhanced-RTMP-v2 §"Metadata Frame" /
  §`ColorInfo` HDR signalling back into its spec-declared AMF Number
  shape. `bit_depth` / `color_primaries` / `transfer_characteristics`
  / `matrix_coefficients` cover the `colorConfig` ISO 23091-4 / H.273
  index fields as `u8`; `max_fall` / `max_cll` cover the
  content-light-level pair as `f64` cd/m^2; `red_x` / `red_y` /
  `green_x` / `green_y` / `blue_x` / `blue_y` / `white_point_x` /
  `white_point_y` / `max_luminance` / `min_luminance` cover the SMPTE
  ST 2086:2018 mastering-display primaries / white point / luminance
  as `f64`. `TypedColorInfo::is_reset_sentinel()` distinguishes the
  RECOMMENDED `["colorInfo", Undefined]` reset shape (the demuxer's
  `colorinfo = "undefined"` sentinel) from a regular populated frame
  — both surface the view as `Some`, but the reset case reports
  `is_reset_sentinel() == true` with every field accessor returning
  `None`. Producers stamp only the metadata they actually have;
  absent fields, out-of-range values, and non-finite numbers all
  flow through as `None`. Eight unit tests cover round-tripping
  against the demuxer's `ex_video_metadata_colorinfo_flattens_into_metadata`
  / `..._undefined_resets` fixtures, the full BT.2020 + D65 hdrMdcv
  group, the reset-vs-no-colorInfo distinction, malformed field
  rejects, and the orthogonality of `colorInfo` against the per-track
  info maps.
- `TypedMetadata::video_track_info_map()` /
  `TypedMetadata::audio_track_info_map()` iterators and
  `TypedMetadata::video_track_info(track_id)` /
  `TypedMetadata::audio_track_info(track_id)` lookups for the
  Enhanced-RTMP-v2 §"Enhancing onMetaData" per-track property maps
  (`videoTrackIdInfoMap` / `audioTrackIdInfoMap`). Each entry parses
  back into a typed view — `TypedVideoTrackInfo` exposes `width` /
  `height` / `video_data_rate_kbps` / `video_codec_id` / `framerate`
  (alias-preference: `videoframerate` first, falling back to
  `framerate`); `TypedAudioTrackInfo` exposes `audio_data_rate_kbps` /
  `audio_sample_rate` (the spec's per-track shortened `samplerate`
  field, not the top-level `audiosamplerate`) / `channels` (modern
  count, not the legacy `stereo` boolean) / `audio_codec_id`. The
  iterator deduplicates per-trackId, preserves bag-insertion order,
  skips trackId 0 (the default track — its fields belong at the top
  level of `onMetaData` and are surfaced by the regular accessors),
  and gracefully skips malformed trackId strings instead of wedging.
  Delta-style per-track entries (only the fields that differ from the
  default track) are first-class: absent fields return `None` rather
  than synthesising from the top-level value, so callers can
  distinguish "producer signalled no per-track override" from
  "producer signalled the same value as the default track". Eleven
  unit tests cover the round-trip against the demuxer's flatten
  fixtures, the trackId-0 / malformed-id / malformed-value rejects,
  the alias-preference for per-track `framerate`, and the
  empty-map case where no track keys exist.
- `TypedMetadata::videoframerate()` + `TypedMetadata::effective_framerate()`
  accessors for the Annex B.1 `videoframerate` alias of the Annex E.5
  `framerate` property. `videoframerate()` returns the de-facto
  property name emitted by every post-2008 Flash-era producer (the
  bag carries it under the same `Vec<(String, String)>` shape, so the
  accessor is a finite-`f64` re-type identical to the spec-named
  `framerate` accessor). `effective_framerate()` mirrors the demuxer's
  alias-preference order — `videoframerate` first, falling back to
  `framerate` — so callers wanting the same value the demuxer lifted
  into `CodecParameters::frame_rate` can read it back through the
  typed view without re-implementing the preference logic. Three
  unit tests cover the alias-only / spec-only / both-present cases
  plus the malformed-alias fallthrough.
- `onCuePoint` (Annex A) and `onXMPData` (§E.6) script-data tag
  writers — completing the muxer's coverage of the four spec-defined
  script names (the other two being `onMetaData` E.5 and the
  encryption `|AdditionalHeader` F.2). New
  `script::write_on_cue_point(w, ts_ms, &CuePointParams)` lays down
  the AMF0 method-name + cue-object pair under the four
  long-standing Flash-runtime property keys: `name` (producer
  identifier, AMF0 String), `time` (Number; validated finite —
  NaN / ±∞ rejected as the demuxer treats `time` as seconds and a
  non-finite value would corrupt the metadata bag), `type` (String;
  `"event"` for playhead-pass dispatch, `"navigation"` for cues that
  additionally surface as seek targets — exposed through
  `CuePointType::Event` / `CuePointType::Navigation`), and
  `parameters` (anonymous Object of user-defined `(name, String)`
  pairs the demuxer flattens under
  `metadata["cuepoint.<n>.parameters.<key>"]`). `timestamp_ms` is
  the cue's playback alignment timestamp — Annex A.4 specifies the
  AMF data track is interleaved at the right time alongside audio
  and video so the runtime dispatches the cue when the playhead
  reaches it. `script::write_on_xmp_data(w, ts_ms, live_xml)` emits
  the §E.6 anonymous Object carrying the single `liveXML` String
  property — exactly the shape the demuxer's `xmp_liveXML` accessor
  walks to surface the payload under `metadata["xmp"]`. Both
  writers round-trip bit-exactly through `FlvDemuxer` and may be
  interleaved freely between media tags. Three new
  `tests/roundtrip_muxer.rs` tests cover both writers
  end-to-end (XMP packet body surfaces verbatim under
  `metadata["xmp"]`; per-cue `name` / `time` / `type` / `parameters`
  round-trip under `metadata["cuepoint.<n>.<key>"]`; interleaving
  cuepoint / XMP tags between MP3 audio tags leaves the audio
  packet stream undisturbed) and eight new `script::tests::*` unit
  tests cover the body-layout, `event` vs `navigation` wire
  spelling, empty-parameters Object, non-finite-time rejection, and
  full-tag header layout (TagType `0x12`, UI24 + UI8 timestamp,
  matching DataSize) for each writer. `CuePointParams`,
  `CuePointType`, `write_on_cue_point` / `write_on_cue_point_body`,
  and `write_on_xmp_data` / `write_on_xmp_data_body` are re-exported
  from the crate root.

- Fuzz-target crate `fuzz/` exercising the parser surface against
  libfuzzer. Four targets land: `demuxer_open_next` (feed arbitrary
  bytes to `open_demuxer`, drain `next_packet` until error / EOF,
  bound the per-iteration step count so a forged input cannot wedge
  the harness — the 24-bit `DataSize` lever and the `read_body`
  remaining-bytes guard are exercised here); `amf0_parse` and
  `amf3_parse` (feed arbitrary bytes through the AMF0 / AMF3 entry
  points — covers `LongString` `u32::MAX`-length, unterminated
  Object body, AVM+ switch into AMF3, U29 4-byte form, UTF-8-vr
  reference tables, traits chains, and circular complex-object
  references); and `script_metadata_roundtrip` (synthesise a
  minimal FLV from fuzz-controlled scalar `onMetaData` properties
  using the muxer slice, re-parse with `open_demuxer`, assert every
  property the muxer emitted survives in `metadata()` — surfaces
  any writer/parser disagreement where the writer happily emits
  bytes the parser refuses or silently drops). The fuzz crate uses
  an isolated `[workspace]` table so it does not pull into the
  umbrella; its `Cargo.lock` is gitignored per workspace policy.

- Typed read-side accessor for the spec-defined fifteen `onMetaData`
  properties of Annex E.5 (`duration`, `filesize`, `width`, `height`,
  `framerate`, `videodatarate`, `audiodatarate`, `audiosamplerate`,
  `audiosamplesize`, `audiodelay`, `videocodecid`, `audiocodecid`,
  `stereo`, `canSeekToEnd`, `creationdate`). New `crate::typed_meta`
  module exposes `TypedMetadata`, a borrowed view over
  `Demuxer::metadata()` that re-types each property back into its
  declared AMF type (Number / Boolean / String) so callers don't
  have to parse strings out of the bag themselves. Missing or
  malformed entries return `None`; the accessor never panics. A
  structured `creationdate_as_date` accessor decodes the
  `"date:<ms>tz:<offset>"` carrier the demuxer uses when the
  producer stamped the field as an AMF0 `Date` rather than a
  free-form `String`. Convenience helpers
  `video_codec_id_str` / `audio_codec_id_str` lower the integer
  codec id through the stable per-id string table
  (`"h264"`, `"vp6f"`, `"aac"`, `"speex"`, …).

- HDR `colorInfo` encode-side wiring (Veovera `enhanced-rtmp-v2`
  §"Metadata Frame" / §`ColorInfo` type block). New
  `crate::color_info` module exposes typed
  `ColorInfo` / `ColorConfig` / `HdrCll` / `HdrMdcv` structs whose
  fields mirror the spec's AMF object shape one-for-one
  (`bitDepth` + ISO 23091-4 `colorPrimaries` /
  `transferCharacteristics` / `matrixCoefficients` for the BT.2020
  config; `maxFall` / `maxCLL` for content light level; `redX..blueY`
  + `whitePointX` / `whitePointY` + `maxLuminance` / `minLuminance`
  for the SMPTE ST 2086:2018 mastering-display volume). Every group
  is optional so producers emit only what they actually signal —
  absent fields are omitted from the AMF object and the player falls
  back to codec-bitstream signalling.
  - `ColorInfo::encode_amf` lays down the AMF0
    `["colorInfo", Object]` pair that follows the Ex video tag
    header on a `videoPacketType = Metadata` tag. Bounds-checks every
    populated field against the spec ranges
    (`hdrCll.*` in `[0.0001, 10_000]` cd/m^2; chromaticities in
    `[0.0001, 0.7400]` for X / `[0.0001, 0.8400]` for Y;
    `maxLuminance` in `[5, 10_000]`, `minLuminance` in
    `[0.0001, 5]`; `bitDepth` in `[8, 16]`) and returns
    `Error::invalid` on the first out-of-range value so callers
    can't silently mux a malformed blob.
  - `color_info::encode_amf_into(out, &ci)` appends the pair to an
    existing buffer for producers that want to emit several pairs in
    one Metadata tag (the spec leaves room for future pair names
    alongside `colorInfo`).
  - `color_info::encode_amf_reset()` builds the spec-recommended
    reset payload `["colorInfo", Undefined]` (Veovera v2: "To reset
    to the original color state you can send colorInfo with a value
    of Undefined (the RECOMMENDED approach) or an empty object").
  - `tag::write_ex_video_color_info(w, ts, fourcc, &ci)` is the
    one-call convenience writer — encodes the typed struct, packages
    it in a `videoPacketType = Metadata` tag with the supplied
    FourCC, and emits the trailing `PreviousTagSize` back-pointer.
    Validation errors surface to the caller before any bytes reach
    the writer, so an out-of-range value leaves the output buffer
    untouched.
  - `tag::write_ex_video_color_info_reset(w, ts, fourcc)` emits the
    reset shape via the same single call.
  - **Round-trip symmetry with the parser.** The encoded body is
    consumed verbatim by `FlvDemuxer`'s existing
    `harvest_video_metadata_frame` walker: every populated field
    surfaces as `metadata["colorinfo.<group>.<key>"]` (bitDepth,
    colorPrimaries, transferCharacteristics, matrixCoefficients,
    maxFall, maxCLL, redX..blueY, whitePointX/Y, maxLuminance,
    minLuminance) and the spec's invalidate-and-replace semantics
    fall out for free — a follow-up `write_ex_video_color_info`
    replaces the prior `colorinfo.*` entries; a
    `write_ex_video_color_info_reset` drops them and leaves the
    `metadata["colorinfo"] = "undefined"` sentinel. Covered by
    four new `tests/roundtrip_muxer.rs` tests
    (`ex_video_color_info_writer_round_trip_full_payload`,
    `_omits_absent_groups`, `_reset_clears_prior_signal`,
    `_rejects_out_of_range_at_writer`) plus nine unit tests in
    `src/color_info.rs` covering empty / populated / reset shapes
    and each spec-range guard.

- ExAudio multitrack emission + parser-level inner-AudioPacketType
  surfacing (Veovera `enhanced-rtmp-v2` §`ExAudioTagHeader` /
  §`ExAudioTagBody`). The audio-side `ExAudioTagHeader` parser used
  to set `packet_type = Multitrack` and silently drop the inner
  per-track `AudioPacketType` byte; that left no way for the inverse
  `to_bytes` writer to recover the wire shape, so multitrack
  emission was rejected with `Error::InvalidData`. The asymmetry
  also forced the demuxer's match arm to discard the wrapper
  altogether (and the discarded default-track payload never
  reached callers). The parser now decodes the multitrack outer
  byte's two nibbles into `multitrack: Some(AvMultitrackType)` and
  `packet_type: ExAudioPacketType` — the inner per-track packet
  type — mirroring the shape the video-side `ExVideoTagHeader`
  parser has carried since the multitrack slice landed.
  - **Parser.** When the leading byte's low nibble is `Multitrack`
    (5), the next byte packs `AvMultitrackType UB[4] | inner
    AudioPacketType UB[4]`. The inner type must NOT itself be
    `Multitrack` (rejected with `Error::InvalidData`). The shared
    FourCc is read for `OneTrack` / `ManyTracks` and skipped for
    `ManyTracksManyCodecs` (where the per-track FourCc rides
    inside each body record). `bytes_consumed` advances past the
    multitrack outer byte and optional shared FourCc so the body
    splitter (`crate::multitrack::split_tracks`) can walk the
    per-track records off `body[bytes_consumed..]`.
  - **Writer.** `ExAudioTagHeader::to_bytes` mirrors the video
    writer: when `multitrack.is_some()` the leading byte's low
    nibble is `Multitrack` (5), the multitrack outer byte packs
    `AvMultitrackType.to_u8() << 4 | inner_packet_type.to_u8()`,
    and the shared FourCc is emitted exactly when the variant
    isn't `ManyTracksManyCodecs`. Nested multitrack
    (`packet_type == Multitrack` with `multitrack.is_some()`)
    is rejected, mirroring the parser's read-side rejection.
    `ManyTracksManyCodecs` with a `Some(fourcc)` is rejected
    (the spec leaves no slot for a shared FourCc in that mode);
    `OneTrack` / `ManyTracks` with `None` is rejected (the spec
    requires the shared FourCc on those variants). ModEx prefix
    emission stacks with multitrack mode: the lead byte
    advertises the ModEx sentinel `7` and the final ModEx
    trailer's low nibble carries the resolved outer `Multitrack`
    value so the parser's `walk` exits on `Multitrack` and the
    multitrack outer byte is read next.
  - **Demuxer routing.** The match arm that classified ExAudio
    packets dropped `ExAudioPacketType::Multitrack` into the
    `header + discard` bucket; that path silently discarded the
    default-track audio body even when the multitrack outer was
    wrapping a perfectly decodable inner `CodedFrames`. With the
    parser now resolving the outer wrapper, the match arm's
    `Multitrack` variant is unreachable and is replaced with an
    explicit `unreachable!()` that documents the invariant. The
    default-track selection (`split_tracks` → trackId 0 → first
    track in wire order) already lived inside the same code path
    and continues to drive the packet body for both video and
    audio multitrack tags.
  - **Tests.** Five new `src/ex_audio.rs` unit tests exercise the
    OneTrack / ManyTracks / ManyTracksManyCodecs `to_bytes`
    round-trips, the multitrack-with-ModEx stack, and the writer
    rejections (nested multitrack, missing shared FourCc on
    `OneTrack` / `ManyTracks`, present shared FourCc on
    `ManyTracksManyCodecs`). One new parser test asserts the inner
    `SequenceStart` is surfaced through a `ManyTracks` wrapper.
    Two new `tests/roundtrip_muxer.rs` integration tests build
    full FLVs through `write_ex_audio_tag` (OneTrack-Opus +
    ManyTracks-AAC) and assert the demuxer surfaces the
    default-track payload verbatim with the right pts and codec
    id, with no `discard` flag set on the resolved `CodedFrames`
    routing.

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
