// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::PlayableId;

pub(super) fn shuffled_track_ids(track_ids: &[PlayableId], shuffle_seed: u64) -> Vec<PlayableId> {
    let mut shuffled = track_ids.to_vec();
    let mut random = SplitMix64::new(shuffle_seed);

    for index in (1..shuffled.len()).rev() {
        let swap_index = random.next_index(index + 1);
        shuffled.swap(index, swap_index);
    }

    shuffled
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D049BB133111EB);
        value ^ (value >> 31)
    }

    fn next_index(&mut self, upper_bound: usize) -> usize {
        let upper_bound = upper_bound as u64;
        let rejection_threshold = u64::MAX - (u64::MAX % upper_bound);

        loop {
            let value = self.next_u64();
            if value < rejection_threshold {
                return (value % upper_bound) as usize;
            }
        }
    }
}
