// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Manifest-driven benchmark and validation harness for Sustain's audio
//! analysis pipeline.
//!
//! This crate is the measurement substrate behind the "best in class"
//! BPM/key/loudness goal: it runs the public
//! [`sustain_analysis::Analyzer`] over a manifest of tracks, records the
//! outputs and per-band timings, and (where the manifest carries ground
//! truth) scores them with the standard MIR metrics — BPM ±tolerance and
//! metrical ratio buckets, MIREX-weighted key categories. Recording a
//! baseline of the *current* implementation is the precondition for
//! reworking the DSP: we never change the analyzer blind.
//!
//! ## Two fixture tiers
//!
//! * **Synthetic** ([`fixtures`]) — deterministically generated WAVs
//!   (silence, tones, ramps, click trains, triads) described inline in a
//!   manifest. These are CI-safe: they commit no audio, regenerate
//!   bit-for-bit, and back the decoder/seek/mono/waveform *determinism*
//!   guarantee (same input → identical output). Their ground-truth BPM/key
//!   is exact by construction, so they double as a coarse accuracy
//!   smoke-check.
//! * **Real-audio** — private corpora (e.g. the maintainer's local
//!   `test-library/`) referenced by path from a *gitignored* manifest.
//!   These carry the real BPM/key/loudness quality signal. Audio is never
//!   committed; only anonymized manifests and aggregate results are.
//!
//! ## Module map
//!
//! * [`manifest`] — the TOML manifest model (tracks, options, ground truth).
//! * [`fixtures`] — deterministic synthetic-WAV synthesis.
//! * [`metrics`] — BPM and MIREX-key scoring, ported from the stratum-dsp
//!   validation suite's `_metrics.py` / `_keys.py`.
//! * [`run`] — drive the analyzer over a manifest and assemble the report.
//! * [`giantsteps`] — adapt a local GiantSteps Tempo/Key checkout into a
//!   manifest (the first public reference corpus; audio stays external).
//! * [`fmak`] — adapt the FMAK / FMAKv2 key annotations + a Free Music
//!   Archive audio root into a manifest (a second, independent key corpus;
//!   audio stays external).
//! * [`private`] — adapt the maintainer's private key/BPM `reference.toml` +
//!   an audio root into a manifest (a real-audio reality check; audio,
//!   source URLs, manifest, and results all stay external).

pub mod fixtures;
pub mod fmak;
pub mod giantsteps;
pub mod manifest;
pub mod metrics;
pub mod private;
pub mod run;
