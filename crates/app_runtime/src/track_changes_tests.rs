// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::{TrackChange, TrackChangeOutbox};
use crate::{TrackFields, TrackId};

fn track_id(id: i64) -> TrackId {
    TrackId::new(id).expect("positive track id")
}

fn fields(fields: TrackFields) -> TrackChange {
    TrackChange {
        fields,
        analysis: false,
    }
}

#[test]
fn only_the_first_pending_change_wakes_the_reader() {
    let mut outbox = TrackChangeOutbox::default();

    assert!(outbox.record(track_id(1), fields(TrackFields::TITLE)));
    assert!(!outbox.record(track_id(2), fields(TrackFields::RATING)));
    assert!(!outbox.record(track_id(1), fields(TrackFields::ARTIST)));
}

#[test]
fn a_change_recorded_after_a_take_wakes_the_reader_again() {
    let mut outbox = TrackChangeOutbox::default();
    assert!(outbox.record(track_id(1), fields(TrackFields::TITLE)));

    assert_eq!(outbox.take().len(), 1);

    assert!(outbox.is_empty());
    assert!(outbox.record(track_id(1), fields(TrackFields::RATING)));
}

#[test]
fn changes_to_one_track_merge() {
    let mut outbox = TrackChangeOutbox::default();
    outbox.record(track_id(1), fields(TrackFields::TITLE));
    outbox.record(
        track_id(1),
        TrackChange {
            fields: TrackFields::BPM,
            analysis: true,
        },
    );

    let changes = outbox.take();

    assert_eq!(
        changes.get(&track_id(1)),
        Some(&TrackChange {
            fields: TrackFields::TITLE | TrackFields::BPM,
            analysis: true,
        })
    );
}

#[test]
fn an_empty_change_records_nothing() {
    let mut outbox = TrackChangeOutbox::default();

    assert!(!outbox.record(track_id(1), TrackChange::default()));
    assert!(outbox.is_empty());
}

#[test]
fn an_analysis_change_alone_is_recorded() {
    let mut outbox = TrackChangeOutbox::default();

    assert!(outbox.record(
        track_id(1),
        TrackChange {
            fields: TrackFields::NONE,
            analysis: true,
        }
    ));
}
