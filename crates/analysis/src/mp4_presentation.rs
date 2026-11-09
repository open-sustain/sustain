// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Which of an MP4 audio track's decoded frames are the track's audio.
//!
//! An AAC encoder starts its stream with priming frames, 1,024 for
//! FFmpeg's encoder and 2,112 for Apple's, and pads the last packet. The
//! container records which frames to present, in two ways:
//!
//! * the **edit list** (`moov/trak/edts/elst`, ISO/IEC 14496-12 §8.6.6):
//!   the first frame and the length of each segment of the media to play;
//! * the **`iTunSMPB` tag** (`moov/udta/meta/ilst/----`), which iTunes
//!   writes instead of, or alongside, an edit list: the priming, the
//!   padding and the original length in frames.
//!
//! Symphonia's MP4 reader hands the decoder every frame, and its AAC
//! decoder trims none. A decode that ignored these records would put every
//! beat and waveform entry of an AAC file 23 to 48 ms late. GStreamer,
//! which plays the library, and FFmpeg honor either record. So does
//! rekordbox: on 354 AAC files of an XDJ-XZ export, its detail waveforms
//! lined up with a FLAC copy of the same recording to within a millisecond,
//! every one of them. The decoder therefore presents an MP4 track as those
//! players do (see `decode`).
//!
//! When a file carries both records, `iTunSMPB` is honored, as FFmpeg
//! does: it is the encoder's own account of its priming. (GStreamer 1.28
//! skips the two added together, even when they agree.) Only the first
//! edit that plays media counts: music files carry one. An empty edit
//! before it, which would delay the media's start, is not honored.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Bound on the movie box Sustain reads into memory. Its sample tables
/// grow with the track, by about 1.2 MB for two hours of AAC; a size far
/// past this is a damaged file, not a long one.
const MAX_MOVIE_BOX_BYTES: u64 = 64 * 1024 * 1024;

/// The presented span of an MP4 track's decoded frames, in the track's
/// media timescale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Presentation {
    /// Decoded units before the first presented one.
    start: u64,
    /// Presented units, when the container records how many.
    length: Option<u64>,
    /// Units per second.
    timescale: u32,
}

impl Presentation {
    /// Decoded frames at `sample_rate` before the first presented one.
    pub(crate) fn lead_frames(&self, sample_rate: u32) -> usize {
        frames(rescale(self.start, self.timescale, sample_rate))
    }

    /// Presented frames at `sample_rate`, when the container records how
    /// many.
    pub(crate) fn frame_count(&self, sample_rate: u32) -> Option<usize> {
        self.length
            .map(|length| frames(rescale(length, self.timescale, sample_rate)))
    }
}

fn frames(count: u64) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}

/// Convert `value` units of `from` per second to the nearest whole number
/// of units of `to` per second.
fn rescale(value: u64, from: u32, to: u32) -> u64 {
    let (from, to) = (u128::from(from), u128::from(to));
    let scaled = (u128::from(value) * to + from / 2) / from;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// The presentation the MP4 file at `path` records for the track whose
/// header carries `track_id` (Symphonia's track id), or `None` when it
/// records none and every decoded frame is presented.
pub(crate) fn read(path: &Path, track_id: u32) -> io::Result<Option<Presentation>> {
    read_from(&mut BufReader::new(File::open(path)?), track_id)
}

fn read_from<R: Read + Seek>(source: &mut R, track_id: u32) -> io::Result<Option<Presentation>> {
    let movie = read_movie_box(source)?;
    let mut movie_timescale = None;
    let mut track = None;
    let mut smpb = None;
    for child in Boxes::new(&movie) {
        let (kind, body) = child?;
        match &kind {
            b"mvhd" => movie_timescale = Some(header_timescale(body)?),
            b"trak" => {
                let candidate = read_track(body)?;
                if candidate.id == track_id {
                    track = Some(candidate);
                }
            }
            b"udta" => {
                for grandchild in Boxes::new(body) {
                    let (kind, body) = grandchild?;
                    if &kind == b"meta" {
                        smpb = smpb.or(itunes_smpb(body)?);
                    }
                }
            }
            b"meta" => smpb = smpb.or(itunes_smpb(body)?),
            _ => {}
        }
    }
    let track = track.ok_or_else(|| invalid("no track carries the decoded track's id"))?;
    let movie_timescale = movie_timescale.ok_or_else(|| invalid("no movie header"))?;
    let media_timescale = track.media_timescale;

    let edit = track.first_edit.map(|edit| Presentation {
        start: edit.media_time,
        length: (edit.segment_duration > 0)
            .then(|| rescale(edit.segment_duration, movie_timescale, media_timescale)),
        timescale: media_timescale,
    });
    let smpb = smpb.map(|smpb| Presentation {
        start: u64::from(smpb.priming),
        length: smpb.length,
        timescale: media_timescale,
    });
    Ok(smpb.or(edit))
}

/// Read the body of the top-level movie box, skipping the boxes around
/// it (the media data often comes first).
fn read_movie_box<R: Read + Seek>(source: &mut R) -> io::Result<Vec<u8>> {
    loop {
        let mut header = [0_u8; 8];
        source.read_exact(&mut header).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                invalid("no movie box")
            } else {
                error
            }
        })?;
        let declared = u64::from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]));
        let kind = [header[4], header[5], header[6], header[7]];
        let body_len = match declared {
            0 => {
                let here = source.stream_position()?;
                let end = source.seek(SeekFrom::End(0))?;
                source.seek(SeekFrom::Start(here))?;
                end - here
            }
            1 => {
                let mut large = [0_u8; 8];
                source.read_exact(&mut large)?;
                u64::from_be_bytes(large)
                    .checked_sub(16)
                    .ok_or_else(|| invalid("box shorter than its header"))?
            }
            size => size
                .checked_sub(8)
                .ok_or_else(|| invalid("box shorter than its header"))?,
        };
        if &kind == b"moov" {
            if body_len > MAX_MOVIE_BOX_BYTES {
                return Err(invalid("movie box too large"));
            }
            let mut body =
                vec![0_u8; usize::try_from(body_len).map_err(|_| invalid("movie box too large"))?];
            source.read_exact(&mut body)?;
            return Ok(body);
        }
        let skip = i64::try_from(body_len).map_err(|_| invalid("box too large"))?;
        source.seek(SeekFrom::Current(skip))?;
    }
}

/// What a track box records about its presentation.
struct TrackBox {
    id: u32,
    media_timescale: u32,
    first_edit: Option<Edit>,
}

/// An edit that plays media: where in the media it starts, in the media
/// timescale, and how long it lasts, in the movie timescale (0: unknown).
#[derive(Clone, Copy)]
struct Edit {
    media_time: u64,
    segment_duration: u64,
}

fn read_track(body: &[u8]) -> io::Result<TrackBox> {
    let mut id = None;
    let mut media_timescale = None;
    let mut first_edit = None;
    for child in Boxes::new(body) {
        let (kind, body) = child?;
        match &kind {
            b"tkhd" => id = Some(track_id(body)?),
            b"edts" => {
                for grandchild in Boxes::new(body) {
                    let (kind, body) = grandchild?;
                    if &kind == b"elst" {
                        first_edit = first_media_edit(body)?;
                    }
                }
            }
            b"mdia" => {
                for grandchild in Boxes::new(body) {
                    let (kind, body) = grandchild?;
                    if &kind == b"mdhd" {
                        media_timescale = Some(header_timescale(body)?);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(TrackBox {
        id: id.ok_or_else(|| invalid("track without a header"))?,
        media_timescale: media_timescale.ok_or_else(|| invalid("track without a media header"))?,
        first_edit,
    })
}

/// The track id in a track header (`tkhd`).
fn track_id(body: &[u8]) -> io::Result<u32> {
    // Version and flags, then creation and modification times: 32-bit in
    // version 0, 64-bit in version 1.
    let offset = if version(body)? == 1 { 4 + 16 } else { 4 + 8 };
    read_u32(body, offset)
}

/// The timescale in a movie (`mvhd`) or media (`mdhd`) header: both put
/// it right after the creation and modification times.
fn header_timescale(body: &[u8]) -> io::Result<u32> {
    let offset = if version(body)? == 1 { 4 + 16 } else { 4 + 8 };
    let timescale = read_u32(body, offset)?;
    if timescale == 0 {
        return Err(invalid("zero timescale"));
    }
    Ok(timescale)
}

/// The first edit in an edit list (`elst`) that plays media. An empty
/// edit (media time −1) plays none.
fn first_media_edit(body: &[u8]) -> io::Result<Option<Edit>> {
    let wide = version(body)? == 1;
    let count = read_u32(body, 4)?;
    let entry_len = if wide { 20 } else { 12 };
    for index in 0..count as usize {
        let offset = 8 + index * entry_len;
        let (segment_duration, media_time) = if wide {
            (read_u64(body, offset)?, read_u64(body, offset + 8)? as i64)
        } else {
            (
                u64::from(read_u32(body, offset)?),
                i64::from(read_u32(body, offset + 4)? as i32),
            )
        };
        if media_time == -1 {
            continue;
        }
        let media_time =
            u64::try_from(media_time).map_err(|_| invalid("negative edit media time"))?;
        return Ok(Some(Edit {
            media_time,
            segment_duration,
        }));
    }
    Ok(None)
}

/// What `iTunSMPB` records: the priming frames and, when known, the
/// original length in frames.
#[derive(Clone, Copy)]
struct Smpb {
    priming: u32,
    length: Option<u64>,
}

/// The `iTunSMPB` record in a metadata box (`meta`), if it holds one that
/// parses. A record that does not parse is ignored, as players ignore it.
fn itunes_smpb(meta: &[u8]) -> io::Result<Option<Smpb>> {
    // ISO metadata boxes carry a version and flags before their children;
    // QuickTime ones start with the handler box directly.
    let children = if meta.get(4..8) == Some(b"hdlr".as_slice()) {
        meta
    } else {
        meta.get(4..)
            .ok_or_else(|| invalid("truncated metadata box"))?
    };
    for child in Boxes::new(children) {
        let (kind, body) = child?;
        if &kind != b"ilst" {
            continue;
        }
        for item in Boxes::new(body) {
            let (kind, body) = item?;
            if &kind == b"----"
                && let Some(text) = freeform_value(body, "com.apple.iTunes", "iTunSMPB")?
            {
                return Ok(parse_smpb(text));
            }
        }
    }
    Ok(None)
}

/// The value of a freeform metadata item (`----`) when its namespace and
/// name are `mean` and `name`.
fn freeform_value<'a>(item: &'a [u8], mean: &str, name: &str) -> io::Result<Option<&'a [u8]>> {
    let (mut item_mean, mut item_name, mut value) = (None, None, None);
    for child in Boxes::new(item) {
        let (kind, body) = child?;
        // Each child starts with a version and flags; a value also carries
        // its type and locale before the data.
        match &kind {
            b"mean" => item_mean = body.get(4..),
            b"name" => item_name = body.get(4..),
            b"data" => value = body.get(8..),
            _ => {}
        }
    }
    let matches = item_mean == Some(mean.as_bytes()) && item_name == Some(name.as_bytes());
    Ok(value.filter(|_| matches))
}

/// Parse an `iTunSMPB` value: hexadecimal fields, the second holding the
/// priming frames and the fourth the original length (0 when unknown).
fn parse_smpb(text: &[u8]) -> Option<Smpb> {
    let text = std::str::from_utf8(text).ok()?;
    let mut fields = text.split_whitespace();
    let priming = u32::from_str_radix(fields.nth(1)?, 16).ok()?;
    let length = u64::from_str_radix(fields.nth(1)?, 16).ok()?;
    Some(Smpb {
        priming,
        length: (length > 0).then_some(length),
    })
}

/// The child boxes of a box body, as their type and body.
struct Boxes<'a> {
    rest: &'a [u8],
}

impl<'a> Boxes<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { rest: body }
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = io::Result<([u8; 4], &'a [u8])>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let parsed = split_box(self.rest);
        match parsed {
            Ok((kind, body, rest)) => {
                self.rest = rest;
                Some(Ok((kind, body)))
            }
            Err(error) => {
                self.rest = &[];
                Some(Err(error))
            }
        }
    }
}

/// Split the first box off `bytes`: its type, its body and what follows.
fn split_box(bytes: &[u8]) -> io::Result<([u8; 4], &[u8], &[u8])> {
    let declared = read_u32(bytes, 0)?;
    let kind = bytes
        .get(4..8)
        .and_then(|kind| <[u8; 4]>::try_from(kind).ok())
        .ok_or_else(|| invalid("truncated box header"))?;
    let (len, header_len) = match declared {
        0 => (bytes.len(), 8),
        1 => {
            let large =
                usize::try_from(read_u64(bytes, 8)?).map_err(|_| invalid("box too large"))?;
            (large, 16)
        }
        size => (size as usize, 8),
    };
    if len < header_len || len > bytes.len() {
        return Err(invalid("box overruns its parent"));
    }
    Ok((kind, &bytes[header_len..len], &bytes[len..]))
}

fn version(body: &[u8]) -> io::Result<u8> {
    body.first()
        .copied()
        .ok_or_else(|| invalid("truncated full box"))
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    bytes
        .get(offset..offset + 4)
        .and_then(|field| <[u8; 4]>::try_from(field).ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| invalid("truncated box"))
}

fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    bytes
        .get(offset..offset + 8)
        .and_then(|field| <[u8; 8]>::try_from(field).ok())
        .map(u64::from_be_bytes)
        .ok_or_else(|| invalid("truncated box"))
}

fn invalid(problem: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, problem)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{Presentation, read_from};

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(body.len() + 8)
            .expect("test box fits")
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    }

    fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    /// A movie or media header: version, flags, two times, the timescale.
    fn timescale_header(kind: &[u8; 4], timescale: u32, wide: bool) -> Vec<u8> {
        let mut body = vec![u8::from(wide), 0, 0, 0];
        body.extend(vec![0; if wide { 16 } else { 8 }]);
        body.extend(timescale.to_be_bytes());
        body.extend(vec![0; if wide { 8 } else { 4 }]);
        mp4_box(kind, &body)
    }

    fn track_header(id: u32, wide: bool) -> Vec<u8> {
        let mut body = vec![u8::from(wide), 0, 0, 0];
        body.extend(vec![0; if wide { 16 } else { 8 }]);
        body.extend(id.to_be_bytes());
        body.extend([0; 4]);
        mp4_box(b"tkhd", &body)
    }

    /// An edit list of `(segment duration, media time)` entries.
    fn edit_list(entries: &[(u64, i64)], wide: bool) -> Vec<u8> {
        let mut body = vec![u8::from(wide), 0, 0, 0];
        body.extend(
            u32::try_from(entries.len())
                .expect("few edits")
                .to_be_bytes(),
        );
        for &(duration, media_time) in entries {
            if wide {
                body.extend(duration.to_be_bytes());
                body.extend(media_time.to_be_bytes());
            } else {
                body.extend(u32::try_from(duration).expect("narrow").to_be_bytes());
                body.extend(i32::try_from(media_time).expect("narrow").to_be_bytes());
            }
            body.extend([0, 1, 0, 0]);
        }
        mp4_box(b"edts", &mp4_box(b"elst", &body))
    }

    fn track(id: u32, edits: Option<Vec<u8>>, media_timescale: u32, wide: bool) -> Vec<u8> {
        let mut body = track_header(id, wide);
        body.extend(edits.unwrap_or_default());
        body.extend(mp4_box(
            b"mdia",
            &timescale_header(b"mdhd", media_timescale, wide),
        ));
        mp4_box(b"trak", &body)
    }

    fn smpb_metadata(value: &str) -> Vec<u8> {
        let full = |kind: &[u8; 4], payload: &[u8]| {
            let mut body = vec![0; 4];
            body.extend_from_slice(payload);
            mp4_box(kind, &body)
        };
        let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0];
        data.extend_from_slice(value.as_bytes());
        let item = concat(&[
            full(b"mean", b"com.apple.iTunes"),
            full(b"name", b"iTunSMPB"),
            mp4_box(b"data", &data),
        ]);
        let mut meta = vec![0; 4];
        meta.extend(mp4_box(b"hdlr", &[0; 25]));
        meta.extend(mp4_box(b"ilst", &mp4_box(b"----", &item)));
        mp4_box(b"udta", &mp4_box(b"meta", &meta))
    }

    fn file(movie_children: &[Vec<u8>]) -> Cursor<Vec<u8>> {
        Cursor::new(concat(&[
            mp4_box(b"ftyp", b"M4A \0\0\0\0"),
            mp4_box(b"mdat", &[0; 64]),
            mp4_box(b"moov", &concat(movie_children)),
        ]))
    }

    const SMPB: &str = " 00000000 00000840 000001C0 0000000000015888 00000000 00000000";

    fn presentation(movie_children: &[Vec<u8>]) -> Option<Presentation> {
        read_from(&mut file(movie_children), 1).expect("parses")
    }

    #[test]
    fn the_edit_list_sets_the_first_frame_and_the_length() {
        for wide in [false, true] {
            let found = presentation(&[
                timescale_header(b"mvhd", 1_000, wide),
                track(1, Some(edit_list(&[(1_000, 1_024)], wide)), 44_100, wide),
            ])
            .expect("an edit list");
            assert_eq!(found.lead_frames(44_100), 1_024, "wide boxes: {wide}");
            assert_eq!(
                found.frame_count(44_100),
                Some(44_100),
                "wide boxes: {wide}"
            );
        }
    }

    #[test]
    fn frames_are_counted_at_the_decoded_rate() {
        let found = presentation(&[
            timescale_header(b"mvhd", 1_000, false),
            track(1, Some(edit_list(&[(500, 2_112)], false)), 44_100, false),
        ])
        .expect("an edit list");
        assert_eq!(found.lead_frames(22_050), 1_056);
        assert_eq!(found.frame_count(22_050), Some(11_025));
    }

    #[test]
    fn itunes_records_the_priming_and_the_length() {
        let found = presentation(&[
            timescale_header(b"mvhd", 44_100, false),
            track(1, None, 44_100, false),
            smpb_metadata(SMPB),
        ])
        .expect("an iTunSMPB record");
        assert_eq!(found.lead_frames(44_100), 2_112);
        assert_eq!(found.frame_count(44_100), Some(88_200));
    }

    #[test]
    fn itunes_comes_before_the_edit_list() {
        for edit_start in [0, 1_024] {
            let found = presentation(&[
                timescale_header(b"mvhd", 1_000, false),
                track(
                    1,
                    Some(edit_list(&[(1_000, edit_start)], false)),
                    44_100,
                    false,
                ),
                smpb_metadata(SMPB),
            ])
            .expect("a presentation");
            assert_eq!(found.lead_frames(44_100), 2_112, "edit from {edit_start}");
            assert_eq!(
                found.frame_count(44_100),
                Some(88_200),
                "edit from {edit_start}"
            );
        }
    }

    #[test]
    fn an_empty_edit_plays_no_media() {
        let found = presentation(&[
            timescale_header(b"mvhd", 1_000, false),
            track(
                1,
                Some(edit_list(&[(20, -1), (1_000, 1_024)], false)),
                44_100,
                false,
            ),
        ])
        .expect("an edit list");
        assert_eq!(found.lead_frames(44_100), 1_024);
    }

    #[test]
    fn the_decoded_track_is_found_by_its_id() {
        let movie = [
            timescale_header(b"mvhd", 1_000, false),
            track(1, Some(edit_list(&[(1_000, 1_024)], false)), 44_100, false),
            track(2, Some(edit_list(&[(1_000, 2_048)], false)), 48_000, false),
        ];
        let second = read_from(&mut file(&movie), 2)
            .expect("parses")
            .expect("an edit list");
        assert_eq!(second.lead_frames(48_000), 2_048);
        assert!(read_from(&mut file(&movie), 3).is_err());
    }

    #[test]
    fn a_file_without_records_presents_every_frame() {
        assert_eq!(
            presentation(&[
                timescale_header(b"mvhd", 1_000, false),
                track(1, None, 44_100, false)
            ]),
            None
        );
        let unparsable = presentation(&[
            timescale_header(b"mvhd", 1_000, false),
            track(1, None, 44_100, false),
            smpb_metadata("not a record"),
        ]);
        assert_eq!(unparsable, None);
    }

    #[test]
    fn a_damaged_box_is_an_error() {
        let mut bytes = file(&[
            timescale_header(b"mvhd", 1_000, false),
            track(1, Some(edit_list(&[(1_000, 1_024)], false)), 44_100, false),
        ])
        .into_inner();
        let len = bytes.len();
        // Make the edit list overrun the box holding it.
        let size_at = bytes
            .windows(4)
            .position(|window| window == b"elst")
            .expect("an edit list")
            - 4;
        bytes[size_at + 3] += 1;
        assert!(read_from(&mut Cursor::new(bytes.clone()), 1).is_err());
        bytes.truncate(len - 4);
        assert!(read_from(&mut Cursor::new(bytes), 1).is_err());
        assert!(read_from(&mut Cursor::new(b"\0\0\0\x10ftypM4A \0\0\0\0".to_vec()), 1).is_err());
    }
}
