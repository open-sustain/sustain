// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The bytes of an audio file that are its audio container: the whole
//! file, less an ID3v1 tag after it.
//!
//! An ID3v1 tag is 128 bytes starting with `TAG`, appended to the file
//! whatever its format. MP3 readers expect it. Older taggers appended it
//! to FLAC files too, where nothing does: Symphonia's FLAC reader looks
//! for the next frame to know where the last one ends, runs into the tag
//! and fails the read, so not one sample of an intact file came back.
//! libFLAC and players read such a file whole. Handing the decoder the
//! file without the tag gives it the end of file it expects.
//!
//! Only a stream that runs to the end of its file — MPEG audio, ADTS,
//! FLAC — can mistake a tag for audio. A container that records its own
//! length (RIFF, AIFF, Matroska, Core Audio Format) ends where it says,
//! and its last 128 bytes may well be audio that happens to start with
//! `TAG`: such a file is read whole. MP4 and Ogg sit between the two.
//! An MP4 file is a run of boxes that each record their length, read to
//! the end of the file, so a tag after the last one is read as a box;
//! Ogg pages run to the end of the file too, and its reader looks for
//! the last one within a page's length of the end, which a tag after it
//! can push out of reach. There, 128 bytes starting with `TAG` are a tag
//! when no box or page reaches into them.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

/// Length of an ID3v1 tag, which sits at the very end of a file.
const ID3V1_LEN: u64 = 128;
/// What an ID3v1 tag starts with.
const ID3V1_MAGIC: &[u8; 3] = b"TAG";
/// How many leading bytes tell a container apart: an MP4 box type sits
/// after the box's 4-byte size.
const CONTAINER_HEAD_LEN: usize = 8;
/// Length of an MP4 box header: its 32-bit size, then its type.
const MP4_BOX_HEADER_LEN: u64 = 8;
/// Length of an MP4 box header whose size is 1: its 64-bit size follows.
const MP4_LARGE_BOX_HEADER_LEN: u64 = 16;
/// What an Ogg page starts with.
const OGG_CAPTURE_PATTERN: &[u8; 4] = b"OggS";
/// Length of an Ogg page header before its segment table.
const OGG_HEADER_LEN: usize = 27;
/// The longest an Ogg page can be: its header, 255 segment lengths and
/// 255 segments of 255 bytes.
const OGG_MAX_PAGE_LEN: usize = OGG_HEADER_LEN + 255 + 255 * 255;

/// How a container ends, which tells whether 128 bytes starting with
/// `TAG` at the end of its file are an ID3v1 tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContainerEnd {
    /// The container records its own length: nothing after it is read.
    Recorded,
    /// MP4 boxes, which end where a box ends.
    Mp4Box,
    /// Ogg pages, which end where a page ends.
    OggPage,
    /// A stream of frames that runs to the end of the file.
    EndOfFile,
}

impl ContainerEnd {
    /// How the container of a file starting with `head` ends.
    fn of(head: &[u8]) -> Self {
        const RECORDING_MAGICS: [&[u8]; 5] = [
            b"RIFF",
            b"RF64",
            b"FORM",
            b"caff",
            &[0x1a, 0x45, 0xdf, 0xa3],
        ];
        if RECORDING_MAGICS.iter().any(|magic| head.starts_with(magic)) {
            Self::Recorded
        } else if head.get(4..8) == Some(b"ftyp".as_slice()) {
            Self::Mp4Box
        } else if head.starts_with(OGG_CAPTURE_PATTERN) {
            Self::OggPage
        } else {
            Self::EndOfFile
        }
    }
}

/// Where the CRC of an Ogg page sits in its header.
const OGG_CRC_RANGE: std::ops::Range<usize> = 22..26;

/// Where the version of an Ogg page sits in its header: 0, the only one.
const OGG_VERSION_AT: usize = 4;

/// Where the header type of an Ogg page sits in its header.
const OGG_HEADER_TYPE_AT: usize = 5;

/// The header-type flags Ogg defines: continued, first, last page.
const OGG_HEADER_TYPE_FLAGS: u8 = 0b111;

/// The length the Ogg page header `bytes` start with gives its page —
/// header, segment table and segments —, if they start with a header
/// the format allows. The page itself may not be all there.
fn ogg_page_len(bytes: &[u8]) -> Option<usize> {
    let header = bytes.get(..OGG_HEADER_LEN)?;
    if !header.starts_with(OGG_CAPTURE_PATTERN)
        || header[OGG_VERSION_AT] != 0
        || header[OGG_HEADER_TYPE_AT] & !OGG_HEADER_TYPE_FLAGS != 0
    {
        return None;
    }
    let segment_count = usize::from(header[OGG_HEADER_LEN - 1]);
    let segment_table = bytes.get(OGG_HEADER_LEN..OGG_HEADER_LEN + segment_count)?;
    let body_len: usize = segment_table.iter().copied().map(usize::from).sum();
    Some(OGG_HEADER_LEN + segment_count + body_len)
}

/// Whether `page`, one whole Ogg page, carries its own checksum.
fn ogg_page_checks_out(page: &[u8]) -> bool {
    page.get(OGG_CRC_RANGE)
        .and_then(|stored| stored.try_into().ok())
        .is_some_and(|stored| ogg_crc(page) == u32::from_le_bytes(stored))
}

/// The Ogg checksum of `page`, its own CRC field read as zero: CRC-32
/// with polynomial 0x04c11db7, unreflected, from zero.
fn ogg_crc(page: &[u8]) -> u32 {
    page.iter().enumerate().fold(0_u32, |crc, (index, &byte)| {
        let byte = if OGG_CRC_RANGE.contains(&index) {
            0
        } else {
            byte
        };
        (crc << 8) ^ OGG_CRC_TABLE[usize::from((crc >> 24) as u8 ^ byte)]
    })
}

/// [`ogg_crc`] a byte at a time: the remainder of each byte value.
const OGG_CRC_TABLE: [u32; 256] = {
    let mut table = [0_u32; 256];
    let mut value = 0;
    while value < 256 {
        let mut crc = (value as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x04c1_1db7
            };
            bit += 1;
        }
        table[value] = crc;
        value += 1;
    }
    table
};

/// Whether the bytes of `tail` from `candidate` on lie past every Ogg
/// page found in `tail`: no page reaches into them, so they are not part
/// of the stream. Only a header whose page would reach them is checked
/// against its checksum: one that ends before them cannot matter.
fn past_every_ogg_page(tail: &[u8], candidate: usize) -> bool {
    !(0..tail.len()).any(|start| {
        ogg_page_len(&tail[start..]).is_some_and(|len| {
            start + len > candidate
                && tail
                    .get(start..start + len)
                    .is_some_and(ogg_page_checks_out)
        })
    })
}

/// A file read up to where its audio container ends.
pub(crate) struct AudioBytes {
    file: File,
    /// Bytes of the file that are not a trailing tag.
    len: u64,
    /// Where the next read starts.
    position: u64,
}

impl AudioBytes {
    /// `file`, from its start, without the ID3v1 tag after its audio if
    /// it has one and is a stream that runs to the end of the file.
    pub(crate) fn new(mut file: File) -> io::Result<Self> {
        let file_len = file.seek(SeekFrom::End(0))?;
        let mut len = file_len;
        let mut head = [0_u8; CONTAINER_HEAD_LEN];
        file.seek(SeekFrom::Start(0))?;
        let head_len = read_up_to(&mut file, &mut head)?;
        let container_end = ContainerEnd::of(&head[..head_len]);
        if let Some(tag_start) = file_len.checked_sub(ID3V1_LEN)
            && container_end != ContainerEnd::Recorded
        {
            let mut magic = [0_u8; ID3V1_MAGIC.len()];
            file.seek(SeekFrom::Start(tag_start))?;
            file.read_exact(&mut magic)?;
            let is_tag = &magic == ID3V1_MAGIC
                && match container_end {
                    ContainerEnd::Recorded => false,
                    ContainerEnd::Mp4Box => past_every_mp4_box_of(&mut file, tag_start)?,
                    ContainerEnd::OggPage => {
                        past_every_ogg_page_of(&mut file, tag_start, file_len)?
                    }
                    ContainerEnd::EndOfFile => true,
                };
            if is_tag {
                len = tag_start;
            }
        }
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            file,
            len,
            position: 0,
        })
    }
}

/// Whether `offset` of `file` lies past every top-level MP4 box: the
/// boxes from the start of the file, each where the one before it ends,
/// end at or before it. Fewer bytes before it than a box header holds
/// are no box. A box that runs to the end of the file, or a size no box
/// can have, reaches it.
fn past_every_mp4_box_of(file: &mut File, offset: u64) -> io::Result<bool> {
    let mut box_start = 0_u64;
    while offset - box_start >= MP4_BOX_HEADER_LEN {
        let mut header = [0_u8; MP4_BOX_HEADER_LEN as usize];
        file.seek(SeekFrom::Start(box_start))?;
        file.read_exact(&mut header)?;
        let (size, header_len) =
            match u32::from_be_bytes([header[0], header[1], header[2], header[3]]) {
                0 => return Ok(false),
                1 if offset - box_start >= MP4_LARGE_BOX_HEADER_LEN => {
                    let mut large = [0_u8; 8];
                    file.read_exact(&mut large)?;
                    (u64::from_be_bytes(large), MP4_LARGE_BOX_HEADER_LEN)
                }
                1 => return Ok(false),
                size => (u64::from(size), MP4_BOX_HEADER_LEN),
            };
        match box_start.checked_add(size) {
            Some(box_end) if size >= header_len && box_end <= offset => box_start = box_end,
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Whether `offset` of `file` lies past every Ogg page that could reach
/// it: the pages that start within a page's length before it. Only the
/// bytes before `end`, the length the file was found to have, are read:
/// what a growing file gains since is not part of the audio read.
fn past_every_ogg_page_of(file: &mut File, offset: u64, end: u64) -> io::Result<bool> {
    let tail_start = offset.saturating_sub(OGG_MAX_PAGE_LEN as u64);
    // At most a page's length and a tag's: both fit in memory.
    let mut tail = vec![0_u8; usize::try_from(end - tail_start).unwrap_or(usize::MAX)];
    file.seek(SeekFrom::Start(tail_start))?;
    let tail_len = read_up_to(file, &mut tail)?;
    tail.truncate(tail_len);
    let candidate = usize::try_from(offset - tail_start).unwrap_or(usize::MAX);
    Ok(past_every_ogg_page(&tail, candidate))
}

/// Read into `buffer` until it is full or the file ends; how many bytes
/// were read.
fn read_up_to(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

impl Read for AudioBytes {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let left = self.len.saturating_sub(self.position);
        let wanted = buffer
            .len()
            .min(usize::try_from(left).unwrap_or(usize::MAX));
        let read = self.file.read(&mut buffer[..wanted])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for AudioBytes {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => self.len.checked_add_signed(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the file",
            )
        })?;
        self.position = self.file.seek(SeekFrom::Start(target))?;
        Ok(self.position)
    }
}

impl MediaSource for AudioBytes {
    fn is_seekable(&self) -> bool {
        self.file.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioBytes, ID3V1_LEN};
    use std::io::{Read, Seek, SeekFrom, Write};
    use symphonia::core::io::MediaSource;

    fn audio_bytes(content: &[u8]) -> AudioBytes {
        let mut file = tempfile::tempfile().expect("temp file");
        file.write_all(content).expect("write");
        AudioBytes::new(file).expect("audio bytes")
    }

    fn tagged(audio: &[u8]) -> Vec<u8> {
        let mut content = audio.to_vec();
        content.extend_from_slice(b"TAG");
        content.resize(audio.len() + ID3V1_LEN as usize, 0);
        content
    }

    #[test]
    fn a_file_without_a_tag_is_read_whole() {
        let content: Vec<u8> = (0..=255).cycle().take(1_000).collect();
        let mut source = audio_bytes(&content);
        assert_eq!(source.byte_len(), Some(1_000));
        let mut read = Vec::new();
        source.read_to_end(&mut read).expect("read");
        assert_eq!(read, content);
    }

    #[test]
    fn an_id3v1_tag_after_the_audio_is_left_out() {
        let audio: Vec<u8> = (0..=255).cycle().take(1_000).collect();
        let mut source = audio_bytes(&tagged(&audio));
        assert_eq!(source.byte_len(), Some(1_000));
        let mut read = Vec::new();
        source.read_to_end(&mut read).expect("read");
        assert_eq!(read, audio);
    }

    #[test]
    fn seeking_from_the_end_counts_from_the_end_of_the_audio() {
        let audio: Vec<u8> = (0..=255).cycle().take(1_000).collect();
        let mut source = audio_bytes(&tagged(&audio));
        assert_eq!(source.seek(SeekFrom::End(-10)).expect("seek"), 990);
        let mut read = Vec::new();
        source.read_to_end(&mut read).expect("read");
        assert_eq!(read, audio[990..]);
        assert_eq!(source.seek(SeekFrom::Current(-5)).expect("seek"), 995);
        assert!(source.seek(SeekFrom::Current(-2_000)).is_err());
    }

    /// A RIFF file's last 128 bytes may be samples that spell `TAG`: the
    /// container says where its data ends, so the file is read whole
    /// (#346, A7-24).
    #[test]
    fn a_self_delimiting_container_ending_in_tag_is_read_whole() {
        let mut content = b"RIFF\x00\x00\x00\x00WAVE".to_vec();
        content.extend((0..=255).cycle().take(1_000));
        let content = tagged(&content);
        let mut source = audio_bytes(&content);
        assert_eq!(source.byte_len(), Some(content.len() as u64));
        let mut read = Vec::new();
        source.read_to_end(&mut read).expect("read");
        assert_eq!(read, content);
    }

    /// An MP4 box of `box_type` holding `body`.
    fn mp4_box(box_type: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut content = u32::try_from(body.len() + 8)
            .expect("box size")
            .to_be_bytes()
            .to_vec();
        content.extend_from_slice(box_type);
        content.extend_from_slice(body);
        content
    }

    /// An MP4 file whose `mdat` holds `media`, its `moov` after it.
    fn mp4_file(media: &[u8]) -> Vec<u8> {
        let mut content = mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00");
        content.extend(mp4_box(b"mdat", media));
        content.extend(mp4_box(b"moov", &[0; 64]));
        content
    }

    /// Media whose last 128 bytes start with `TAG`, inside the box that
    /// holds them, is read whole (#346, A7-24).
    #[test]
    fn an_mp4_box_ending_in_tag_is_read_whole() {
        let mut content = mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00");
        content.extend(mp4_box(b"mdat", &tagged(&[0x21; 1_000])));
        assert_eq!(audio_bytes(&content).byte_len(), Some(content.len() as u64));
    }

    #[test]
    fn an_mp4_file_loses_the_tag_after_its_last_box() {
        let content = mp4_file(&[0x21; 1_000]);
        assert_eq!(
            audio_bytes(&tagged(&content)).byte_len(),
            Some(content.len() as u64)
        );
    }

    /// Fewer stray bytes after the last box than a box header holds are
    /// no box: the tag after them is still one.
    #[test]
    fn an_mp4_file_loses_a_tag_after_stray_bytes() {
        let mut content = mp4_file(&[0x21; 1_000]);
        content.extend_from_slice(&[0; 4]);
        assert_eq!(
            audio_bytes(&tagged(&content)).byte_len(),
            Some(content.len() as u64)
        );
    }

    /// A box of size 0 runs to the end of the file, its last 128 bytes
    /// with it.
    #[test]
    fn an_mp4_box_running_to_the_end_of_the_file_keeps_its_tail() {
        let mut content = mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00");
        content.extend_from_slice(b"\x00\x00\x00\x00mdat");
        let content = tagged(&content);
        assert_eq!(audio_bytes(&content).byte_len(), Some(content.len() as u64));
    }

    /// A box whose size is 1 gives its size in the 64 bits after its
    /// type.
    #[test]
    fn an_mp4_file_with_a_large_box_loses_the_tag_after_it() {
        let mut content = mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00");
        content.extend_from_slice(b"\x00\x00\x00\x01mdat");
        content.extend_from_slice(&(16_u64 + 1_000).to_be_bytes());
        content.extend_from_slice(&[0x21; 1_000]);
        assert_eq!(
            audio_bytes(&tagged(&content)).byte_len(),
            Some(content.len() as u64)
        );
    }

    /// An Ogg page of `body`, in one segment table, with its checksum.
    fn ogg_page(body: &[u8]) -> Vec<u8> {
        let mut page = b"OggS".to_vec();
        page.resize(26, 0);
        let segments: Vec<u8> = body
            .chunks(255)
            .map(|chunk| u8::try_from(chunk.len()).expect("a segment fits a byte"))
            .collect();
        page.push(u8::try_from(segments.len()).expect("a page has at most 255 segments"));
        page.extend(segments);
        page.extend_from_slice(body);
        let crc = super::ogg_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        page
    }

    #[test]
    fn the_ogg_checksum_is_the_one_ogg_defines() {
        // Polynomial 0x04c11db7, unreflected, from zero: CRC-32/CKSUM
        // without its final inversion, whose check value is 0x765e7680.
        assert_eq!(super::ogg_crc(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn an_ogg_stream_loses_a_tag_after_padding() {
        let body: Vec<u8> = (0..=255).cycle().take(1_000).collect();
        let mut audio = ogg_page(&body);
        audio.push(0);
        assert_eq!(
            audio_bytes(&tagged(&audio)).byte_len(),
            Some(audio.len() as u64)
        );
    }

    #[test]
    fn a_capture_pattern_inside_ogg_audio_is_no_page() {
        // `OggS` inside a page's audio, then `TAG` inside the same page:
        // only a page whose checksum holds counts.
        let mut body: Vec<u8> = (0..=255).cycle().take(1_000).collect();
        body[700..704].copy_from_slice(b"OggS");
        let tag_start = body.len() - ID3V1_LEN as usize;
        body[tag_start..tag_start + 3].copy_from_slice(b"TAG");
        let content = ogg_page(&body);
        assert_eq!(audio_bytes(&content).byte_len(), Some(content.len() as u64));
    }

    #[test]
    fn an_ogg_stream_loses_the_tag_after_its_last_page() {
        let body: Vec<u8> = (0..=255).cycle().take(1_000).collect();
        let mut audio = ogg_page(&body[..400]);
        audio.extend(ogg_page(&body[400..]));
        assert_eq!(
            audio_bytes(&tagged(&audio)).byte_len(),
            Some(audio.len() as u64)
        );
    }

    #[test]
    fn an_ogg_page_ending_in_tag_is_read_whole() {
        let mut body: Vec<u8> = (0..=255).cycle().take(1_000).collect();
        let tag_start = body.len() - ID3V1_LEN as usize;
        body[tag_start..tag_start + 3].copy_from_slice(b"TAG");
        let content = ogg_page(&body);
        assert_eq!(audio_bytes(&content).byte_len(), Some(content.len() as u64));
    }

    #[test]
    fn a_flac_stream_loses_the_tag_after_it() {
        let mut audio = b"fLaC".to_vec();
        audio.extend((0..=255).cycle().take(1_000));
        assert_eq!(
            audio_bytes(&tagged(&audio)).byte_len(),
            Some(audio.len() as u64)
        );
    }

    #[test]
    fn a_file_shorter_than_a_tag_is_read_whole() {
        let mut source = audio_bytes(b"TAG");
        assert_eq!(source.byte_len(), Some(3));
        let mut read = Vec::new();
        source.read_to_end(&mut read).expect("read");
        assert_eq!(read, b"TAG");
    }
}
