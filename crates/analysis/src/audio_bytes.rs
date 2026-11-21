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
//! A container that records its own length (RIFF, AIFF, Matroska, Core
//! Audio Format) ends where it says, and its last 128 bytes may well be
//! audio that happens to start with `TAG`: such a file is read whole.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

/// Length of an ID3v1 tag, which sits at the very end of a file.
const ID3V1_LEN: u64 = 128;
/// What an ID3v1 tag starts with.
const ID3V1_MAGIC: &[u8; 3] = b"TAG";
/// What the containers that record their own length start with.
const LENGTH_RECORDING_MAGICS: [&[u8; 4]; 5] = [
    b"RIFF",
    b"RF64",
    b"FORM",
    b"caff",
    &[0x1a, 0x45, 0xdf, 0xa3],
];

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
    /// it has one and its container does not record its own length.
    pub(crate) fn new(mut file: File) -> io::Result<Self> {
        let file_len = file.seek(SeekFrom::End(0))?;
        let mut len = file_len;
        let mut head = [0_u8; 4];
        file.seek(SeekFrom::Start(0))?;
        let records_its_length = read_up_to(&mut file, &mut head)? == head.len()
            && LENGTH_RECORDING_MAGICS.contains(&&head);
        if let Some(tag_start) = file_len.checked_sub(ID3V1_LEN)
            && !records_its_length
        {
            let mut magic = [0_u8; ID3V1_MAGIC.len()];
            file.seek(SeekFrom::Start(tag_start))?;
            file.read_exact(&mut magic)?;
            if &magic == ID3V1_MAGIC {
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
