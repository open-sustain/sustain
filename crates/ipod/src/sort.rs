// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The orders of the iPod's browse menus.
//!
//! The master playlist carries one index per browse menu — the tracks by
//! title, artist, album, genre and composer — and, beside each, a jump
//! table: runs of the index sharing a first letter, which the click wheel
//! skips between. The orders are Sustain's own collation
//! ([`CollationKey`]), over the "sort as" fields when set, so the iPod
//! files tracks where Sustain's library does; ties break as libgpod's
//! writer breaks them.

use std::cmp::Ordering;

use sustain_domain::{CollationKey, normalize_search_text};

use crate::model::IpodTrack;

/// A browse menu, with the sort-type code its index and jump table carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Menu {
    Title = 0x03,
    Album = 0x04,
    Artist = 0x05,
    Genre = 0x07,
    Composer = 0x12,
}

impl Menu {
    /// In the order libgpod writes them.
    pub(crate) const ALL: [Self; 5] = [
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::Genre,
        Self::Composer,
    ];

    pub(crate) const fn code(self) -> u32 {
        self as u32
    }
}

/// One run of a jump table: `count` index entries from `start` share
/// `letter`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JumpRun {
    pub(crate) letter: u16,
    pub(crate) start: u32,
    pub(crate) count: u32,
}

/// One track's sort keys and jump letters, built once and compared by
/// every menu.
struct Keys {
    title: CollationKey,
    album: CollationKey,
    artist: CollationKey,
    genre: CollationKey,
    composer: CollationKey,
    disc: u32,
    track: u32,
    letters: [u16; 5],
}

impl Keys {
    fn new(track: &IpodTrack) -> Self {
        let title = sort_text(track.sort_title.as_deref(), track.title.as_deref());
        let album = sort_text(track.sort_album.as_deref(), track.album.as_deref());
        let artist = sort_text(track.sort_artist.as_deref(), track.artist.as_deref());
        let genre = sort_text(None, track.genre.as_deref());
        let composer = sort_text(track.sort_composer.as_deref(), track.composer.as_deref());
        Self {
            letters: [
                jump_letter(title),
                jump_letter(artist),
                jump_letter(album),
                jump_letter(genre),
                jump_letter(composer),
            ],
            title: CollationKey::new(title),
            album: CollationKey::new(album),
            artist: CollationKey::new(artist),
            genre: CollationKey::new(genre),
            composer: CollationKey::new(composer),
            disc: track.disc_number.unwrap_or(0),
            track: track.track_number.unwrap_or(0),
        }
    }

    fn letter(&self, menu: Menu) -> u16 {
        let slot = Menu::ALL
            .iter()
            .position(|&candidate| candidate == menu)
            .expect("every menu is listed");
        self.letters[slot]
    }

    fn compare(&self, other: &Self, menu: Menu) -> Ordering {
        let album_order = || {
            self.album
                .cmp(&other.album)
                .then(self.disc.cmp(&other.disc))
                .then(self.track.cmp(&other.track))
                .then_with(|| self.title.cmp(&other.title))
        };
        match menu {
            Menu::Title => self.title.cmp(&other.title),
            Menu::Album => album_order(),
            Menu::Artist => self.artist.cmp(&other.artist).then_with(album_order),
            Menu::Genre => self
                .genre
                .cmp(&other.genre)
                .then_with(|| self.artist.cmp(&other.artist))
                .then_with(album_order),
            Menu::Composer => self.composer.cmp(&other.composer).then_with(album_order),
        }
    }
}

/// The text a track sorts by in one menu: the "sort as" value when set,
/// else the shown value, else nothing.
fn sort_text<'a>(sort_as: Option<&'a str>, shown: Option<&'a str>) -> &'a str {
    sort_as
        .filter(|text| !text.trim().is_empty())
        .or(shown)
        .unwrap_or_default()
}

/// The letter a value files under in a jump table: its first letter or
/// digit, accents dropped and upper-cased — the fold the collation sorts by,
/// so one letter's entries form a single run. Digits, and text without a
/// letter or digit, file under `0`, as on an iPod written by iTunes.
fn jump_letter(text: &str) -> u16 {
    let folded = normalize_search_text(text);
    let Some(first) = folded.chars().find(|c| c.is_alphanumeric()) else {
        return u16::from(b'0');
    };
    if first.is_numeric() {
        return u16::from(b'0');
    }
    let mut upper = first.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(letter), None) => u16::try_from(u32::from(letter)).unwrap_or(u16::from(b'0')),
        _ => u16::try_from(u32::from(first)).unwrap_or(u16::from(b'0')),
    }
}

/// The browse indexes of a track list: for each menu, the tracks'
/// positions in menu order, and the jump table over it.
pub(crate) struct BrowseIndexes {
    keys: Vec<Keys>,
}

impl BrowseIndexes {
    pub(crate) fn new(tracks: &[IpodTrack]) -> Self {
        Self {
            keys: tracks.iter().map(Keys::new).collect(),
        }
    }

    /// The positions of the tracks in `menu`'s order, and its jump table.
    /// The sort is stable, so equal tracks keep their library order.
    pub(crate) fn index(&self, menu: Menu) -> (Vec<u32>, Vec<JumpRun>) {
        let mut order: Vec<usize> = (0..self.keys.len()).collect();
        order.sort_by(|&a, &b| self.keys[a].compare(&self.keys[b], menu));
        let mut runs: Vec<JumpRun> = Vec::new();
        for (rank, &position) in order.iter().enumerate() {
            let letter = self.keys[position].letter(menu);
            let rank = u32::try_from(rank).expect("track counts are bounded by the writer");
            match runs.last_mut() {
                Some(run) if run.letter == letter => run.count += 1,
                _ => runs.push(JumpRun {
                    letter,
                    start: rank,
                    count: 1,
                }),
            }
        }
        let positions = order
            .into_iter()
            .map(|position| u32::try_from(position).expect("track counts are bounded"))
            .collect();
        (positions, runs)
    }
}

#[cfg(test)]
mod tests {
    use super::{BrowseIndexes, JumpRun, Menu, jump_letter};
    use crate::model::{FileType, IpodTrack};

    fn track(title: &str, artist: &str, album: &str, number: u32) -> IpodTrack {
        IpodTrack {
            dbid: 1,
            location: String::new(),
            file_type: FileType::Mp3,
            title: Some(title.to_owned()),
            artist: Some(artist.to_owned()),
            album: Some(album.to_owned()),
            album_artist: None,
            genre: None,
            composer: None,
            comment: None,
            sort_title: None,
            sort_artist: None,
            sort_album: None,
            sort_album_artist: None,
            sort_composer: None,
            compilation: false,
            rating_stars: 0,
            track_number: Some(number),
            track_total: None,
            disc_number: None,
            disc_total: None,
            year: None,
            duration_ms: 0,
            size_bytes: 0,
            bitrate_kbps: None,
            sample_rate_hz: 44_100,
            date_added: None,
        }
    }

    #[test]
    fn jump_letters_fold_accents_and_file_digits_under_zero() {
        assert_eq!(jump_letter("édith"), u16::from(b'E'));
        assert_eq!(jump_letter("  ...and justice"), u16::from(b'A'));
        assert_eq!(jump_letter("1999"), u16::from(b'0'));
        assert_eq!(jump_letter("!!!"), u16::from(b'0'));
        assert_eq!(jump_letter(""), u16::from(b'0'));
    }

    #[test]
    fn artists_order_by_album_then_track_and_share_jump_runs() {
        let tracks = vec![
            track("Zed", "Björk", "Post", 2),
            track("Army", "Air", "Moon Safari", 1),
            track("Hyper", "Björk", "Post", 1),
            track("Édith", "Beck", "Odelay", 1),
        ];
        let (positions, runs) = BrowseIndexes::new(&tracks).index(Menu::Artist);
        assert_eq!(positions, vec![1, 3, 2, 0]);
        assert_eq!(
            runs,
            vec![
                JumpRun {
                    letter: u16::from(b'A'),
                    start: 0,
                    count: 1
                },
                JumpRun {
                    letter: u16::from(b'B'),
                    start: 1,
                    count: 3
                },
            ]
        );
    }

    #[test]
    fn a_sort_as_value_wins_over_the_shown_one() {
        let mut beatles = track("Help", "The Beatles", "Help!", 1);
        beatles.sort_artist = Some("Beatles".to_owned());
        let tracks = vec![track("Song", "Tears", "Album", 1), beatles];
        let (positions, runs) = BrowseIndexes::new(&tracks).index(Menu::Artist);
        assert_eq!(positions, vec![1, 0]);
        assert_eq!(runs[0].letter, u16::from(b'B'));
    }
}
