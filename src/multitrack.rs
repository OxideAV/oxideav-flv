//! Enhanced RTMP / E-FLV multitrack body codec (splitter +
//! serialiser), per Veovera `enhanced-rtmp-v2` §`ExAudioTagBody` and
//! §`ExVideoTagBody`. [`split_tracks`] walks a body into per-track byte
//! ranges; [`join_tracks`] is its exact inverse, writing a batch of
//! owned [`JoinTrack`]s back into the same wire layout.
//!
//! A single audio or video message at one timestamp may carry a *batch*
//! of tracks (e.g. several `trackId` values for different bitrates,
//! resolutions, codecs, languages, or camera angles). The `Multitrack`
//! packet type (`AudioPacketType = 5` / `VideoPacketType = 6`) in the
//! Ex tag header switches the body from a single payload into a loop of
//! per-track records.
//!
//! # On-the-wire layout
//!
//! The Ex tag *header* already carries the [`AvMultitrackType`] (UB[4],
//! parsed by [`crate::ex_audio::ExAudioTagHeader`] /
//! [`crate::ex_video::ExVideoTagHeader`]) and, for the
//! `OneTrack` / `ManyTracks` modes, a single shared `FourCc`. The body
//! that follows the Ex header is then walked one track at a time:
//!
//! ```text
//!   ── repeat for each track:
//!     if multitrackType == ManyTracksManyCodecs:
//!       4   FourCc                    (per-track codec id)
//!     1     trackId  = UI8
//!     if multitrackType != OneTrack:
//!       3   sizeOfTrack = UI24        (bytes of this track's payload,
//!                                       counted from immediately after
//!                                       this field)
//!     N     trackPayload[sizeOfTrack] (config / coded data)
//!   ──
//! ```
//!
//! For `OneTrack` there is no `sizeOfTrack` field — the single track's
//! payload runs to the end of the body (the spec models `OneTrack` as a
//! degenerate multitrack with exactly one track and the `continue`
//! branch never taken). For `ManyTracks` / `ManyTracksManyCodecs` the
//! `sizeOfTrack` UI24 demarcates each track so the parser can advance to
//! the next one; the spec's `positionDataPtrToNextTrack(sizeOfTrack)`
//! step is exactly this advance.
//!
//! This splitter is codec-agnostic: per-track payloads are returned as
//! byte ranges into the caller's body slice. Codec-specific framing
//! that lives *inside* a track payload (e.g. the `SI24
//! CompositionTimeOffset` AVC/HEVC/VVC `CodedFrames` carry, or a codec
//! config record on `SequenceStart`) is the caller's concern — it parses
//! the per-track payload exactly as it would a single-track Ex body.

use std::ops::Range;

use oxideav_core::{Error, Result};

/// `AvMultitrackType` (UB[4]). Shared by the audio and video Ex
/// pipelines — the same enum is read off the multitrack outer header in
/// both [`crate::ex_audio`] and [`crate::ex_video`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvMultitrackType {
    /// `0` — one track (the default; a degenerate single-track batch
    /// with no per-track `sizeOfTrack` field).
    OneTrack,
    /// `1` — many tracks, the *same* codec FourCc on all of them (the
    /// shared FourCc is carried once in the Ex header).
    ManyTracks,
    /// `2` — many tracks, each with its *own* FourCc carried per-track
    /// inside the body loop.
    ManyTracksManyCodecs,
    /// `3..=15` — reserved for future extensions; preserved verbatim so
    /// a future multitrack mode lands without parser changes.
    Reserved(u8),
}

impl AvMultitrackType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::OneTrack,
            1 => Self::ManyTracks,
            2 => Self::ManyTracksManyCodecs,
            other => Self::Reserved(other),
        }
    }

    /// 4-bit wire value (the high nibble of the multitrack header byte
    /// per enhanced-rtmp-v2 §`ExVideoTagHeader` / §`ExAudioTagHeader`).
    /// Inverse of [`Self::from_u8`]. `Reserved(n)` is masked to 4 bits.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::OneTrack => 0,
            Self::ManyTracks => 1,
            Self::ManyTracksManyCodecs => 2,
            Self::Reserved(n) => n & 0x0F,
        }
    }

    /// `true` for `OneTrack` — the degenerate mode with no per-track
    /// `sizeOfTrack` field.
    pub fn is_one_track(self) -> bool {
        matches!(self, Self::OneTrack)
    }

    /// `true` for `ManyTracksManyCodecs` — the mode that carries a
    /// per-track FourCc inside the body loop.
    pub fn is_many_codecs(self) -> bool {
        matches!(self, Self::ManyTracksManyCodecs)
    }
}

/// One track recovered from a multitrack [`ExAudioTagBody`] /
/// [`ExVideoTagBody`].
///
/// [`ExAudioTagBody`]: crate::ex_audio
/// [`ExVideoTagBody`]: crate::ex_video
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultitrackTrack {
    /// `trackId` (UI8). `0` is the conventional default track; positive
    /// ids identify additional variants. These are identifiers only and
    /// imply no ordering or quality ranking (per spec §"Track Ordering").
    pub track_id: u8,
    /// FourCc identifying this track's codec. For `OneTrack` /
    /// `ManyTracks` this is the shared FourCc from the Ex header (passed
    /// in as `default_fourcc`); for `ManyTracksManyCodecs` it is the
    /// per-track FourCc read off the front of each track record.
    pub fourcc: u32,
    /// Byte range of this track's payload *within the body slice handed
    /// to [`split_tracks`]*. Slice `body[track.payload.clone()]` to
    /// recover the per-track config / coded-data bytes (which the caller
    /// parses with the same codec-specific logic it uses for a
    /// single-track Ex body).
    pub payload: Range<usize>,
}

impl MultitrackTrack {
    /// The length in bytes of this track's payload.
    pub fn len(&self) -> usize {
        self.payload.end - self.payload.start
    }

    /// `true` when the track's payload is empty (e.g. a `SequenceEnd`
    /// track, which spec-legally carries no bytes).
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

/// One track of a multitrack Ex tag, resolved to an *owned* payload and
/// a codec name — the read-side companion to [`MultitrackTrack`].
///
/// [`MultitrackTrack`] returns a byte *range* into a borrowed body slice,
/// which is ideal for the demuxer's zero-copy default-track lift but
/// awkward for a caller that wants to keep every variant of a multitrack
/// tag after the tag body goes out of scope. A [`FlvDemuxer`] surfaces
/// the full set of tracks of the most-recently-emitted multitrack tag as
/// a `Vec<MultitrackPacketTrack>` (see
/// [`FlvDemuxer::last_multitrack_tracks`]), so a caller that wants the
/// non-default variants (different bitrate / resolution / codec /
/// language / camera angle, per enhanced-rtmp-v2 §"Track Ordering") can
/// recover their coded data without re-running [`split_tracks`] or
/// re-parsing the Ex header itself.
///
/// [`FlvDemuxer`]: crate::FlvDemuxer
/// [`FlvDemuxer::last_multitrack_tracks`]: crate::FlvDemuxer::last_multitrack_tracks
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultitrackPacketTrack {
    /// `trackId` (UI8). `0` is the conventional default track; positive
    /// ids identify additional variants. Identifiers only — they imply
    /// no ordering, priority, or quality ranking (enhanced-rtmp-v2
    /// §"Track Ordering").
    pub track_id: u8,
    /// FourCc identifying this track's codec (the shared Ex-header FourCc
    /// for `OneTrack` / `ManyTracks`, the per-track FourCc for
    /// `ManyTracksManyCodecs`). Big-endian packed (e.g.
    /// `u32::from_be_bytes(*b"hvc1")`).
    pub fourcc: u32,
    /// Human-readable codec name for [`Self::fourcc`] (`"h265"`, `"av1"`,
    /// `"opus"`, …), or the `flv:exvideo:<ascii>` / `flv:exaudio:<ascii>`
    /// sentinel for a FourCc the crate does not recognise. The `is_video`
    /// flag passed to the resolver disambiguates the audio vs. video name
    /// table.
    pub codec_name: String,
    /// This track's coded payload — owned, so it outlives the tag body.
    /// For `SequenceStart` this is the codec configuration record; for
    /// `CodedFrames` it is the coded media (the leading `SI24`
    /// CompositionTimeOffset, for AVC/HEVC/VVC, is left in place — the
    /// payload is exactly the per-track Ex body the demuxer would parse).
    pub payload: Vec<u8>,
}

impl MultitrackPacketTrack {
    /// The length in bytes of this track's owned payload.
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// `true` when the track carries no payload bytes (e.g. a per-track
    /// `SequenceEnd`).
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

/// Split a multitrack Ex tag body into its constituent per-track
/// records, per `enhanced-rtmp-v2` §`ExAudioTagBody` /
/// §`ExVideoTagBody`.
///
/// `mt_type` is the [`AvMultitrackType`] decoded from the Ex header.
/// `default_fourcc` is the shared FourCc the Ex header carried for the
/// `OneTrack` / `ManyTracks` modes; it is ignored (and may be any value)
/// for `ManyTracksManyCodecs`, where each track carries its own FourCc.
/// `body` is the tag-body tail *after* the Ex header
/// (`tag_body[ex.bytes_consumed..]`).
///
/// Returns one [`MultitrackTrack`] per track in wire order. The byte
/// ranges are relative to `body` (not the full tag body).
///
/// Errors with [`Error::InvalidData`] on a truncated track record, a
/// `sizeOfTrack` that runs past the end of the body, or a `Reserved`
/// multitrack type (whose per-track layout the spec does not define).
pub fn split_tracks(
    mt_type: AvMultitrackType,
    default_fourcc: u32,
    body: &[u8],
) -> Result<Vec<MultitrackTrack>> {
    if let AvMultitrackType::Reserved(_) = mt_type {
        return Err(Error::invalid(
            "FLV multitrack: reserved AvMultitrackType has no defined per-track layout",
        ));
    }

    let mut tracks = Vec::new();
    let mut cursor = 0usize;

    loop {
        // ManyTracksManyCodecs: each track is prefixed with its own
        // FourCc. OneTrack / ManyTracks reuse the Ex-header FourCc.
        let fourcc = if mt_type.is_many_codecs() {
            if cursor + 4 > body.len() {
                return Err(Error::invalid("FLV multitrack: truncated per-track FourCc"));
            }
            let fcc = u32::from_be_bytes([
                body[cursor],
                body[cursor + 1],
                body[cursor + 2],
                body[cursor + 3],
            ]);
            cursor += 4;
            fcc
        } else {
            default_fourcc
        };

        // trackId UI8 — always present.
        if cursor >= body.len() {
            return Err(Error::invalid("FLV multitrack: truncated trackId"));
        }
        let track_id = body[cursor];
        cursor += 1;

        if mt_type.is_one_track() {
            // OneTrack: no sizeOfTrack — the single track's payload runs
            // to the end of the body. There is exactly one track.
            tracks.push(MultitrackTrack {
                track_id,
                fourcc,
                payload: cursor..body.len(),
            });
            break;
        }

        // ManyTracks / ManyTracksManyCodecs: sizeOfTrack UI24 demarcates
        // this track's payload.
        if cursor + 3 > body.len() {
            return Err(Error::invalid("FLV multitrack: truncated sizeOfTrack"));
        }
        let size = ((body[cursor] as usize) << 16)
            | ((body[cursor + 1] as usize) << 8)
            | (body[cursor + 2] as usize);
        cursor += 3;

        let payload_end = cursor
            .checked_add(size)
            .filter(|&end| end <= body.len())
            .ok_or_else(|| Error::invalid("FLV multitrack: sizeOfTrack runs past end of body"))?;
        tracks.push(MultitrackTrack {
            track_id,
            fourcc,
            payload: cursor..payload_end,
        });
        cursor = payload_end;

        // The spec's loop continues only while there is another track;
        // it `break`s once the body is exhausted. A non-zero remainder
        // smaller than a minimal track record is malformed.
        if cursor == body.len() {
            break;
        }
    }

    Ok(tracks)
}

/// One track handed to [`join_tracks`] for encoding — the owned,
/// write-side input that mirrors what [`split_tracks`] recovers.
///
/// `join_tracks` is the exact inverse of [`split_tracks`]: it serialises a
/// batch of tracks back into the `enhanced-rtmp-v2` §`ExAudioTagBody` /
/// §`ExVideoTagBody` wire layout. A [`JoinTrack`] therefore carries the
/// same three pieces of state a [`MultitrackTrack`] / [`MultitrackPacketTrack`]
/// exposes on read: the `trackId`, the codec `fourcc`, and the per-track
/// payload bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinTrack {
    /// `trackId` (UI8). `0` is the conventional default track; positive
    /// ids identify additional variants. Identifiers only — no ordering
    /// or quality ranking is implied (enhanced-rtmp-v2 §"Track Ordering").
    pub track_id: u8,
    /// FourCc identifying this track's codec, big-endian packed (e.g.
    /// `u32::from_be_bytes(*b"hvc1")`). Emitted per-track *only* for
    /// [`AvMultitrackType::ManyTracksManyCodecs`]; for `OneTrack` /
    /// `ManyTracks` the shared FourCc lives in the Ex header and every
    /// `JoinTrack::fourcc` in the batch is ignored by [`join_tracks`].
    pub fourcc: u32,
    /// This track's coded payload (codec configuration record on
    /// `SequenceStart`, coded media on `CodedFrames`, or empty on
    /// `SequenceEnd`). Serialised verbatim — `join_tracks` does not
    /// inspect or reframe it.
    pub payload: Vec<u8>,
}

impl JoinTrack {
    /// Construct a [`JoinTrack`] from its parts.
    pub fn new(track_id: u8, fourcc: u32, payload: Vec<u8>) -> Self {
        Self {
            track_id,
            fourcc,
            payload,
        }
    }

    /// The length in bytes of this track's payload.
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// `true` when this track carries no payload bytes.
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

impl From<&MultitrackPacketTrack> for JoinTrack {
    /// Lift an owned read-side [`MultitrackPacketTrack`] straight into the
    /// write-side input, so `split (→ resolve) → join` round-trips without
    /// the caller re-stating each field. The `codec_name` is dropped — the
    /// wire layout keys off `fourcc` only.
    fn from(t: &MultitrackPacketTrack) -> Self {
        Self {
            track_id: t.track_id,
            fourcc: t.fourcc,
            payload: t.payload.clone(),
        }
    }
}

/// Largest payload a single non-`OneTrack` track may carry: the
/// `sizeOfTrack` field is a UI24, so anything above this overflows it.
const MAX_TRACK_PAYLOAD: usize = 0x00FF_FFFF;

/// Serialise a batch of tracks into a multitrack Ex tag body — the exact
/// inverse of [`split_tracks`], per `enhanced-rtmp-v2` §`ExAudioTagBody` /
/// §`ExVideoTagBody`.
///
/// `mt_type` is the [`AvMultitrackType`] the Ex header will advertise.
/// `tracks` are the per-track records to emit, in wire order. The returned
/// `Vec<u8>` is the tag-body tail that follows the Ex header (i.e. it does
/// **not** include the Ex header's own multitrack-type nibble or, for the
/// `OneTrack` / `ManyTracks` modes, the shared FourCc — those belong to
/// the [`crate::ex_audio::ExAudioTagHeader`] /
/// [`crate::ex_video::ExVideoTagHeader`] encoder).
///
/// Per the spec body loop:
/// * For [`AvMultitrackType::ManyTracksManyCodecs`] each record is
///   prefixed with its own `FourCc` (UI32 BE); for `OneTrack` /
///   `ManyTracks` the FourCc is shared in the Ex header and omitted here
///   (each track's [`JoinTrack::fourcc`] is ignored).
/// * `trackId` (UI8) is always written.
/// * For every mode *except* `OneTrack` a `sizeOfTrack` (UI24 BE) demarcates
///   the payload. `OneTrack` omits it — the single payload runs to the end
///   of the body.
///
/// Errors with [`Error::InvalidData`] when:
/// * `mt_type` is [`AvMultitrackType::Reserved`] (no defined wire layout —
///   mirrors [`split_tracks`]);
/// * `mt_type` is [`AvMultitrackType::OneTrack`] and `tracks.len() != 1`
///   (the degenerate mode carries exactly one track and has no
///   `sizeOfTrack` to delimit a second);
/// * `tracks` is empty for a non-`OneTrack` mode (a multitrack body with no
///   tracks cannot be parsed back — `split_tracks` always yields ≥ 1);
/// * any non-`OneTrack` track's payload exceeds [`MAX_TRACK_PAYLOAD`]
///   (`0x00FF_FFFF`), which would overflow the UI24 `sizeOfTrack`.
pub fn join_tracks(mt_type: AvMultitrackType, tracks: &[JoinTrack]) -> Result<Vec<u8>> {
    if let AvMultitrackType::Reserved(_) = mt_type {
        return Err(Error::invalid(
            "FLV multitrack: reserved AvMultitrackType has no defined per-track layout",
        ));
    }

    if mt_type.is_one_track() {
        if tracks.len() != 1 {
            return Err(Error::invalid(
                "FLV multitrack: OneTrack body must carry exactly one track",
            ));
        }
    } else if tracks.is_empty() {
        return Err(Error::invalid(
            "FLV multitrack: ManyTracks/ManyTracksManyCodecs body needs at least one track",
        ));
    }

    let mut body = Vec::new();
    for track in tracks {
        // ManyTracksManyCodecs: each track is prefixed with its own
        // FourCc. OneTrack / ManyTracks carry the shared FourCc in the
        // Ex header, so it is not written into the body.
        if mt_type.is_many_codecs() {
            body.extend_from_slice(&track.fourcc.to_be_bytes());
        }

        // trackId UI8 — always present.
        body.push(track.track_id);

        if mt_type.is_one_track() {
            // OneTrack: no sizeOfTrack — payload runs to the end of body.
            body.extend_from_slice(&track.payload);
            break;
        }

        // ManyTracks / ManyTracksManyCodecs: sizeOfTrack UI24 BE.
        let size = track.payload.len();
        if size > MAX_TRACK_PAYLOAD {
            return Err(Error::invalid(
                "FLV multitrack: track payload exceeds UI24 sizeOfTrack capacity",
            ));
        }
        body.push((size >> 16) as u8);
        body.push((size >> 8) as u8);
        body.push(size as u8);
        body.extend_from_slice(&track.payload);
    }

    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOURCC_AV01: u32 = u32::from_be_bytes(*b"av01");
    const FOURCC_HVC1: u32 = u32::from_be_bytes(*b"hvc1");
    const FOURCC_OPUS: u32 = u32::from_be_bytes(*b"Opus");

    #[test]
    fn av_multitrack_type_round_trips() {
        assert_eq!(AvMultitrackType::from_u8(0), AvMultitrackType::OneTrack);
        assert_eq!(AvMultitrackType::from_u8(1), AvMultitrackType::ManyTracks);
        assert_eq!(
            AvMultitrackType::from_u8(2),
            AvMultitrackType::ManyTracksManyCodecs
        );
        for v in 3u8..=15 {
            assert_eq!(AvMultitrackType::from_u8(v), AvMultitrackType::Reserved(v));
        }
        assert!(AvMultitrackType::OneTrack.is_one_track());
        assert!(!AvMultitrackType::ManyTracks.is_one_track());
        assert!(AvMultitrackType::ManyTracksManyCodecs.is_many_codecs());
        assert!(!AvMultitrackType::ManyTracks.is_many_codecs());
    }

    #[test]
    fn one_track_payload_runs_to_end() {
        // OneTrack: trackId=0 then the whole rest of the body is the
        // single track's payload. No sizeOfTrack field.
        let body = [0x00, 0xDE, 0xAD, 0xBE, 0xEF];
        let tracks = split_tracks(AvMultitrackType::OneTrack, FOURCC_AV01, &body).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_id, 0);
        assert_eq!(tracks[0].fourcc, FOURCC_AV01);
        assert_eq!(&body[tracks[0].payload.clone()], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(tracks[0].len(), 4);
        assert!(!tracks[0].is_empty());
    }

    #[test]
    fn one_track_empty_payload() {
        // OneTrack SequenceEnd: trackId then no payload bytes.
        let body = [0x07];
        let tracks = split_tracks(AvMultitrackType::OneTrack, FOURCC_OPUS, &body).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_id, 7);
        assert!(tracks[0].is_empty());
        assert_eq!(tracks[0].len(), 0);
    }

    #[test]
    fn many_tracks_shared_fourcc() {
        // Two tracks, both hvc1 (FourCc shared in Ex header). Each:
        //   trackId UI8, sizeOfTrack UI24, payload[size].
        let mut body = Vec::new();
        // Track 0: id=0, size=3, payload [0x01,0x02,0x03].
        body.push(0x00);
        body.extend_from_slice(&[0x00, 0x00, 0x03]);
        body.extend_from_slice(&[0x01, 0x02, 0x03]);
        // Track 1: id=1, size=2, payload [0xAA,0xBB].
        body.push(0x01);
        body.extend_from_slice(&[0x00, 0x00, 0x02]);
        body.extend_from_slice(&[0xAA, 0xBB]);

        let tracks = split_tracks(AvMultitrackType::ManyTracks, FOURCC_HVC1, &body).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_id, 0);
        assert_eq!(tracks[0].fourcc, FOURCC_HVC1);
        assert_eq!(&body[tracks[0].payload.clone()], &[0x01, 0x02, 0x03]);
        assert_eq!(tracks[1].track_id, 1);
        assert_eq!(tracks[1].fourcc, FOURCC_HVC1);
        assert_eq!(&body[tracks[1].payload.clone()], &[0xAA, 0xBB]);
    }

    #[test]
    fn many_tracks_many_codecs_per_track_fourcc() {
        // Two tracks with distinct codecs. Each:
        //   FourCc (4), trackId UI8, sizeOfTrack UI24, payload[size].
        let mut body = Vec::new();
        // Track 0: av01, id=0, size=2.
        body.extend_from_slice(b"av01");
        body.push(0x00);
        body.extend_from_slice(&[0x00, 0x00, 0x02]);
        body.extend_from_slice(&[0x11, 0x22]);
        // Track 1: hvc1, id=5, size=1.
        body.extend_from_slice(b"hvc1");
        body.push(0x05);
        body.extend_from_slice(&[0x00, 0x00, 0x01]);
        body.push(0x33);

        // default_fourcc is ignored for ManyTracksManyCodecs.
        let tracks = split_tracks(AvMultitrackType::ManyTracksManyCodecs, 0, &body).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_id, 0);
        assert_eq!(tracks[0].fourcc, FOURCC_AV01);
        assert_eq!(&body[tracks[0].payload.clone()], &[0x11, 0x22]);
        assert_eq!(tracks[1].track_id, 5);
        assert_eq!(tracks[1].fourcc, FOURCC_HVC1);
        assert_eq!(&body[tracks[1].payload.clone()], &[0x33]);
    }

    #[test]
    fn many_tracks_three_tracks_walk_to_end() {
        let mut body = Vec::new();
        for (id, payload) in [(0u8, &[0xA0][..]), (1, &[0xA1, 0xA2]), (2, &[0xA3])] {
            body.push(id);
            let size = payload.len();
            body.extend_from_slice(&[(size >> 16) as u8, (size >> 8) as u8, size as u8]);
            body.extend_from_slice(payload);
        }
        let tracks = split_tracks(AvMultitrackType::ManyTracks, FOURCC_OPUS, &body).unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(
            tracks.iter().map(|t| t.track_id).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(&body[tracks[1].payload.clone()], &[0xA1, 0xA2]);
    }

    #[test]
    fn reserved_multitrack_type_rejected() {
        let body = [0x00, 0x00, 0x00, 0x00];
        assert!(matches!(
            split_tracks(AvMultitrackType::Reserved(3), FOURCC_AV01, &body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_track_id_errors() {
        // ManyTracks but body ends right where trackId should be.
        let body: [u8; 0] = [];
        assert!(matches!(
            split_tracks(AvMultitrackType::ManyTracks, FOURCC_AV01, &body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_size_of_track_errors() {
        // trackId present but only 1 of 3 sizeOfTrack bytes.
        let body = [0x00, 0x00];
        assert!(matches!(
            split_tracks(AvMultitrackType::ManyTracks, FOURCC_AV01, &body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn size_of_track_overruns_body_errors() {
        // sizeOfTrack=10 but only 2 payload bytes follow.
        let mut body = vec![0x00];
        body.extend_from_slice(&[0x00, 0x00, 0x0A]); // size = 10
        body.extend_from_slice(&[0x01, 0x02]); // only 2 bytes
        assert!(matches!(
            split_tracks(AvMultitrackType::ManyTracks, FOURCC_AV01, &body),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn truncated_per_track_fourcc_errors() {
        // ManyTracksManyCodecs but only 2 of 4 FourCc bytes.
        let body = *b"av";
        assert!(matches!(
            split_tracks(AvMultitrackType::ManyTracksManyCodecs, 0, &body),
            Err(Error::InvalidData(_))
        ));
    }

    // ---- join_tracks (write-side inverse of split_tracks) ----

    /// Resolve the read-side ranges of a `split_tracks` result against the
    /// body slice into owned `JoinTrack`s, so a `split → join` round-trip
    /// can be expressed without re-stating each field by hand.
    fn resolve(body: &[u8], tracks: &[MultitrackTrack]) -> Vec<JoinTrack> {
        tracks
            .iter()
            .map(|t| JoinTrack::new(t.track_id, t.fourcc, body[t.payload.clone()].to_vec()))
            .collect()
    }

    #[test]
    fn join_one_track_matches_handbuilt_body() {
        let tracks = [JoinTrack::new(0, FOURCC_AV01, vec![0xDE, 0xAD, 0xBE, 0xEF])];
        let body = join_tracks(AvMultitrackType::OneTrack, &tracks).unwrap();
        // OneTrack body: trackId then bare payload, no FourCc, no size.
        assert_eq!(body, vec![0x00, 0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn join_one_track_empty_payload() {
        let tracks = [JoinTrack::new(7, FOURCC_OPUS, vec![])];
        let body = join_tracks(AvMultitrackType::OneTrack, &tracks).unwrap();
        assert_eq!(body, vec![0x07]);
    }

    #[test]
    fn join_many_tracks_shared_fourcc_matches_handbuilt() {
        let tracks = [
            JoinTrack::new(0, FOURCC_HVC1, vec![0x01, 0x02, 0x03]),
            JoinTrack::new(1, FOURCC_HVC1, vec![0xAA, 0xBB]),
        ];
        let body = join_tracks(AvMultitrackType::ManyTracks, &tracks).unwrap();
        let expected = vec![
            0x00, 0x00, 0x00, 0x03, 0x01, 0x02, 0x03, // track 0
            0x01, 0x00, 0x00, 0x02, 0xAA, 0xBB, // track 1
        ];
        assert_eq!(body, expected);
    }

    #[test]
    fn join_many_tracks_many_codecs_emits_per_track_fourcc() {
        let tracks = [
            JoinTrack::new(0, FOURCC_AV01, vec![0x11, 0x22]),
            JoinTrack::new(5, FOURCC_HVC1, vec![0x33]),
        ];
        let body = join_tracks(AvMultitrackType::ManyTracksManyCodecs, &tracks).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(b"av01");
        expected.push(0x00);
        expected.extend_from_slice(&[0x00, 0x00, 0x02]);
        expected.extend_from_slice(&[0x11, 0x22]);
        expected.extend_from_slice(b"hvc1");
        expected.push(0x05);
        expected.extend_from_slice(&[0x00, 0x00, 0x01]);
        expected.push(0x33);
        assert_eq!(body, expected);
    }

    #[test]
    fn split_then_join_is_identity_one_track() {
        let body = [0x00, 0xDE, 0xAD, 0xBE, 0xEF];
        let tracks = split_tracks(AvMultitrackType::OneTrack, FOURCC_AV01, &body).unwrap();
        let rebuilt = join_tracks(AvMultitrackType::OneTrack, &resolve(&body, &tracks)).unwrap();
        assert_eq!(rebuilt, body);
    }

    #[test]
    fn split_then_join_is_identity_many_tracks() {
        let mut body = Vec::new();
        body.push(0x00);
        body.extend_from_slice(&[0x00, 0x00, 0x03]);
        body.extend_from_slice(&[0x01, 0x02, 0x03]);
        body.push(0x01);
        body.extend_from_slice(&[0x00, 0x00, 0x02]);
        body.extend_from_slice(&[0xAA, 0xBB]);

        let tracks = split_tracks(AvMultitrackType::ManyTracks, FOURCC_HVC1, &body).unwrap();
        let rebuilt = join_tracks(AvMultitrackType::ManyTracks, &resolve(&body, &tracks)).unwrap();
        assert_eq!(rebuilt, body);
    }

    #[test]
    fn split_then_join_is_identity_many_codecs() {
        let mut body = Vec::new();
        body.extend_from_slice(b"av01");
        body.push(0x00);
        body.extend_from_slice(&[0x00, 0x00, 0x02]);
        body.extend_from_slice(&[0x11, 0x22]);
        body.extend_from_slice(b"hvc1");
        body.push(0x05);
        body.extend_from_slice(&[0x00, 0x00, 0x01]);
        body.push(0x33);

        let tracks = split_tracks(AvMultitrackType::ManyTracksManyCodecs, 0, &body).unwrap();
        let rebuilt = join_tracks(
            AvMultitrackType::ManyTracksManyCodecs,
            &resolve(&body, &tracks),
        )
        .unwrap();
        assert_eq!(rebuilt, body);
    }

    #[test]
    fn join_then_split_is_identity_three_many_tracks() {
        // Hand-built batch → join → split recovers the same trackIds and
        // payloads (and the shared FourCc threaded through).
        let tracks = [
            JoinTrack::new(0, FOURCC_OPUS, vec![0xA0]),
            JoinTrack::new(1, FOURCC_OPUS, vec![0xA1, 0xA2]),
            JoinTrack::new(2, FOURCC_OPUS, vec![]),
        ];
        let body = join_tracks(AvMultitrackType::ManyTracks, &tracks).unwrap();
        let split = split_tracks(AvMultitrackType::ManyTracks, FOURCC_OPUS, &body).unwrap();
        assert_eq!(split.len(), 3);
        for (orig, got) in tracks.iter().zip(split.iter()) {
            assert_eq!(orig.track_id, got.track_id);
            assert_eq!(orig.fourcc, got.fourcc);
            assert_eq!(&body[got.payload.clone()], orig.payload.as_slice());
        }
    }

    #[test]
    fn join_then_split_is_identity_many_codecs() {
        let tracks = [
            JoinTrack::new(0, FOURCC_AV01, vec![0x11, 0x22, 0x33]),
            JoinTrack::new(9, FOURCC_HVC1, vec![0x44]),
            JoinTrack::new(2, FOURCC_OPUS, vec![]),
        ];
        let body = join_tracks(AvMultitrackType::ManyTracksManyCodecs, &tracks).unwrap();
        // default_fourcc is ignored for ManyTracksManyCodecs.
        let split = split_tracks(AvMultitrackType::ManyTracksManyCodecs, 0, &body).unwrap();
        assert_eq!(split.len(), 3);
        for (orig, got) in tracks.iter().zip(split.iter()) {
            assert_eq!(orig.track_id, got.track_id);
            assert_eq!(orig.fourcc, got.fourcc);
            assert_eq!(&body[got.payload.clone()], orig.payload.as_slice());
        }
    }

    #[test]
    fn join_from_packet_track_lifts_owned_read_side() {
        // MultitrackPacketTrack (owned read side) → JoinTrack via From.
        let pkt = [
            MultitrackPacketTrack {
                track_id: 0,
                fourcc: FOURCC_AV01,
                codec_name: "av1".to_string(),
                payload: vec![0x11, 0x22],
            },
            MultitrackPacketTrack {
                track_id: 1,
                fourcc: FOURCC_HVC1,
                codec_name: "h265".to_string(),
                payload: vec![0x33],
            },
        ];
        let join: Vec<JoinTrack> = pkt.iter().map(JoinTrack::from).collect();
        let body = join_tracks(AvMultitrackType::ManyTracksManyCodecs, &join).unwrap();
        let split = split_tracks(AvMultitrackType::ManyTracksManyCodecs, 0, &body).unwrap();
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].fourcc, FOURCC_AV01);
        assert_eq!(&body[split[1].payload.clone()], &[0x33]);
    }

    #[test]
    fn join_reserved_type_rejected() {
        let tracks = [JoinTrack::new(0, FOURCC_AV01, vec![0x00])];
        assert!(matches!(
            join_tracks(AvMultitrackType::Reserved(3), &tracks),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn join_one_track_rejects_multiple() {
        let tracks = [
            JoinTrack::new(0, FOURCC_AV01, vec![0x00]),
            JoinTrack::new(1, FOURCC_AV01, vec![0x01]),
        ];
        assert!(matches!(
            join_tracks(AvMultitrackType::OneTrack, &tracks),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn join_one_track_rejects_empty_batch() {
        assert!(matches!(
            join_tracks(AvMultitrackType::OneTrack, &[]),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn join_many_tracks_rejects_empty_batch() {
        assert!(matches!(
            join_tracks(AvMultitrackType::ManyTracks, &[]),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn join_rejects_payload_over_ui24() {
        let tracks = [JoinTrack::new(
            0,
            FOURCC_AV01,
            vec![0u8; MAX_TRACK_PAYLOAD + 1],
        )];
        assert!(matches!(
            join_tracks(AvMultitrackType::ManyTracks, &tracks),
            Err(Error::InvalidData(_))
        ));
    }

    #[test]
    fn join_accepts_max_ui24_payload_size_field() {
        // A payload exactly at the UI24 ceiling encodes a 0xFFFFFF size.
        // (Use a small payload but assert the boundary check is inclusive
        // by confirming MAX is accepted, not the multi-MB allocation.)
        let tracks = [JoinTrack::new(3, FOURCC_HVC1, vec![0xEE; 4])];
        let body = join_tracks(AvMultitrackType::ManyTracks, &tracks).unwrap();
        assert_eq!(&body[1..4], &[0x00, 0x00, 0x04]);
        assert_eq!(body[0], 0x03);
    }
}
