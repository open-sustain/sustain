// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Optical-disc probing and audio-CD track encoding for Sustain.
//!
//! This crate owns three things and nothing else:
//!
//! * **Probing** — enumerating optical drives and reading a disc's TOC into
//!   an owned, thread-safe [`TocSnapshot`] (device path, MusicBrainz Disc
//!   ID, TOC fingerprint, and per-track numbers/offsets/durations).
//! * **Encoding** — extracting one physical track via GStreamer's
//!   `cdparanoiasrc` and encoding it with `flacenc` / `lamemp3enc` per a
//!   [`sustain_domain::CdEncodingProfile`].
//! * **The seams** — the [`OpticalProbe`] and [`CdEncoder`] traits, so the
//!   import state machine in the runtime can be tested with fakes, without a
//!   physical drive.
//!
//! It deliberately does **not** own GTK, network metadata lookup, SQLite,
//! notifications, or the live application model. The MusicBrainz disc-id
//! lookup lives in `sustain-metadata-remote`; the per-track tag/publish/row
//! pipeline lives in `sustain-app-runtime`.
//!
//! libdiscid is reached through the optional, default-on `optical` feature
//! (see `Cargo.toml`). Built without it, [`SystemOpticalProbe`] reports no
//! optical backend and the rest of the application stays usable.

#![forbid(unsafe_code)]

mod encode;
mod error;
mod probe;
mod toc;

pub use encode::{
    CdEncoder, EncodeProgress, EncodeRequest, EncoderElement, EncoderProperty, GStreamerCdEncoder,
    SINK_FACTORY, SOURCE_FACTORY, encoder_element, first_missing_element, required_elements,
};
pub use error::{CdImportError, CdImportResult};
pub use probe::{OpticalProbe, SystemOpticalProbe};
pub use toc::{CD_FRAMES_PER_SECOND, DiscIdentity, RawTocTrack, TocSnapshot, TocTrack};
