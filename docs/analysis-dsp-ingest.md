# Analysis DSP ingest — gate artifacts (#192)

> **Status: the ingest has landed (§5).** This document is the review gate that
> preceded it — the exact analysis-output contract, the *measured* transitive
> call graph of the DSP Sustain uses, and the vendoring/provenance shape — and
> now doubles as the reference for what was vendored. The ingest itself (vendor
> the minimal core into `crates/dsp`, drop the published `stratum-dsp`, collapse
> to a single symphonia) is recorded in §5; per-file detail is in
> `crates/dsp/PROVENANCE.md`.

`crates/analysis` reached into the published `stratum-dsp` 1.0 crate for BPM,
key, the STFT, spectral-flux onsets, and ITU-R BS.1770 loudness; the ingest
replaced that with the vendored `sustain-dsp` (`crates/dsp`). The ingest source
was the fork `open-sustain/stratum-dsp@wip/dsp-rework`
(HEAD `5f4b4160441725cdd8a6e4355b6908d41b0451db`), **not** crates.io — the fork
already carries fixes (notably deterministic key selection, see §1.3) that the
published crate lacks. Everything measured below was read directly from that
fork checkout and from `crates/analysis/src/lib.rs` at the current `main`.

---

## 1. Analysis-output contract

This is the exact set of values `sustain_analysis::Analyzer` produces, their
types, their semantics, and which of them depend on third-party DSP. The
contract is the invariant the ingest must preserve: the vendored core may be
restructured freely as long as these outputs stay within the tolerances in
§1.4.

### 1.1 The four capability-gated outputs

`Analyzer` exposes four independent band methods (`crates/analysis/src/lib.rs`).
A method never called does zero work, so the contract is per-band.

| Method | Return type | Meaning |
| --- | --- | --- |
| `Analyzer::bpm()` | `Option<f32>` | Tempo in BPM, octave-normalized into `[options.min_bpm, options.max_bpm]`. `None` on decode failure, too-short audio, or no confident estimate. |
| `Analyzer::key()` | `Option<MusicalKey>` | One of Sustain's 24 canonical `MusicalKey` variants. `None` on decode failure, empty chroma, or a label outside the 24. |
| `Analyzer::acoustics()` | `Option<AcousticFeatures>` | Nine perceptual features (below). `None` on decode failure, too-short audio, or effectively-silent material. |
| `Analyzer::waveform()` | `Option<WaveformTiers>` | Preview + detail envelope tiers. `None` only on decode failure; a silent-but-valid track yields `Some` with empty segments. |

`AcousticFeatures` (re-exported from `sustain_domain`) carries exactly nine
`f32` fields, all consumed by Smart Shuffle:

- `integrated_lufs` — gated BS.1770-4 integrated loudness of the measured region.
- `short_term_lufs_max` — max of the sliding 3 s / 1 s short-term loudness curve.
- `loudness_range_lu` — 95th−10th percentile spread of gated short-term windows.
- `onset_rate_hz` — spectral-flux onset count ÷ region duration.
- `low_band_ratio`, `mid_band_ratio`, `high_band_ratio` — fraction of STFT power
  below 250 Hz / 250–4000 Hz / above 4000 Hz.
- `low_band_variation` — coefficient of variation of the per-frame low-band
  fraction.
- `tonalness` — mean `1 − spectral_flatness` across non-silent frames.

`WaveformTiers` is two `WaveformSegments` (preview at `PREVIEW_SEGMENT_COUNT`,
detail at `DETAIL_SEGMENTS_PER_SECOND` on the Pioneer half-frame timeline).
The waveform is **skipped entirely** for long tracks (`is_long_track()`).

### 1.2 Which outputs depend on third-party DSP

This is the boundary the ingest moves. Only the values in the "DSP" column
change provenance; Sustain's own math is untouched.

| Output | Third-party DSP it consumes | Sustain-owned math on top |
| --- | --- | --- |
| `bpm` | `estimate_bpm_tempogram` → `BpmEstimate.bpm` | `octave_normalize` into the range |
| `key` | `compute_stft` → `extract_chroma_from_spectrogram_with_options` → `detect_key` → `KeyDetectionResult.key` (a `stratum_dsp::Key`) | `stratum_key_label` + `map_stratum_key` → `MusicalKey` |
| `acoustics.integrated_lufs` / `short_term_lufs_max` / `loudness_range_lu` | `normalize` (BS.1770-4) → `LoudnessMetadata.measured_lufs` | sliding-window sampling, percentile/gate range |
| `acoustics.onset_rate_hz` | `detect_spectral_flux_onsets` (count) | ÷ duration |
| `acoustics.*_band_ratio` / `low_band_variation` / `tonalness` | `compute_stft` (the spectrogram only) | band power split, CoV, spectral flatness — **all Sustain code** |
| `waveform` | none | `decode` + `bands` + `waveform` — **all Sustain code** |

The STFT (`compute_stft`) is the shared front-end: BPM, key, and every spectral
acoustic feature slice off it. The waveform tiers depend on nothing in
`stratum-dsp` — they are pure Sustain (`decode.rs`, `bands.rs`, `waveform.rs`).

### 1.3 Determinism contract

Sustain stores these values once and treats them as authoritative
(`ANALYZER_VERSION` gating, library-wins policy). They must therefore be a
deterministic function of the decoded samples + `AnalysisOptions`:

- **Same file + same options → identical `bpm`, `key`, `acoustics`, `waveform`.**
  Verified by `crates/analysis_bench`: the determinism test asserts all four,
  and the committed synthetic baseline regenerates byte-for-byte.
- **Key determinism — resolved by the ingest.** Phase 1 surfaced that the
  **published** `stratum-dsp` 1.0 selected the winning key via a `max_by` over a
  `HashMap`, so ties resolved in hash-iteration order and flipped between runs;
  the synthetic corpus excluded `key` to avoid a churning baseline. The vendored
  `sustain-dsp` (from the fork) ranks with `detect_key_weighted` →
  `sort_key_scores_desc` → `compare_key_scores_desc`, a `total_cmp` with a
  `key_sort_index` secondary key — fully deterministic, no map iteration. So
  `key` and the triad fixtures with key ground truth are back in the synthetic
  corpus and the determinism assertions.

### 1.4 Equivalence tolerance (what the ingest must preserve)

The ingest restructures code; it must not silently move outputs. "Preserved"
means:

- **Synthetic baseline:** `bpm`, `acoustics`, and `waveform` stay **byte-stable**
  against `crates/analysis_bench/baselines/synthetic.json` (PCM/WAV decode is
  bit-identical, so the only variable is the DSP math). A change here is either
  a bug or a conscious algorithm change that bumps `ANALYZER_VERSION`.
- **Real-audio corpora:** BPM within the existing MIR tolerance (±2 BPM and the
  metrical-ratio buckets), MIREX-weighted key category not regressing, loudness
  within a small LU tolerance, onset-rate within a small relative tolerance —
  measured by the harness against the private manifests.
- **`ANALYZER_VERSION` (currently 5)** is bumped *only* if the ingest
  deliberately changes a stored value's meaning (e.g. key now deterministic
  where it was not). A pure refactor that holds the synthetic baseline does not
  bump it.

---

## 2. Measured transitive call graph

Traced from the eight — and only eight — symbols `crates/analysis/src/lib.rs`
imports from `stratum_dsp`. Sustain deliberately bypasses
`stratum_dsp::analyze_audio` (the compute-everything orchestrator), so the
reachable set is far smaller than the crate. **Reachability here means "reached
from Sustain's entry points," not "reached from the crate's own `analyze_audio`."**
That distinction matters: many `stratum-dsp` functions are reachable only from
`analyze_audio` and are dead weight for us.

### 2.1 Entry points

```
features::chroma::extractor::compute_stft
features::chroma::extractor::extract_chroma_from_spectrogram_with_options
features::key::detect_key
features::key::templates::KeyTemplates            (::new)
features::onset::spectral_flux::detect_spectral_flux_onsets
features::period::tempogram::estimate_bpm_tempogram
preprocessing::normalization::{normalize, NormalizationConfig, NormalizationMethod}
Key                                                (enum, via detect_key's result)
```

### 2.2 Reachable closure — per file, with the symbols that come along

| File | Reachable symbols | External crate | Notes |
| --- | --- | --- | --- |
| `error.rs` | `AnalysisError` (whole enum) | — | Hand-rolled `Display`/`Error`, **no `thiserror`**. Only `InvalidInput`/`ProcessingError`/`NumericalError` are constructed on reachable paths. |
| `analysis/result.rs` | `Key` enum **only** (+ its `name`/`numerical`/`from_numerical` methods) | `serde` (derive) | Sustain pattern-matches `Key::Major/Minor(u32)` and never serializes it → the `Serialize`/`Deserialize` derive can be **dropped** in the vendored copy, killing the serde dep. |
| `features/chroma/extractor.rs` | `compute_stft`, `extract_chroma_from_spectrogram_with_options`, `frame_to_chroma`, `frame_to_chroma_tuned`, consts `EPSILON`/`A4_FREQ`/`SEMITONE_OFFSET`/`DEFAULT_CHROMA_FMIN_HZ`/`DEFAULT_CHROMA_FMAX_HZ` | `rustfft` (only `compute_stft`) | All HPCP/tuning/log-freq/beat-sync/mask/`_and_energy` variants are **not** reached. |
| `features/key/detector.rs` | `detect_key`, `detect_key_weighted`, `weighted_sum_dot`, `dot_product` | — | `_mode_heuristic`/`_multi_scale`/`_median`/`_ensemble` not reached. |
| `features/key/mod.rs` | `KeyDetectionResult`, `sort_key_scores_desc`, `compare_key_scores_desc`, `key_sort_index` | — | `key_sort_index` is reached transitively (tiebreak inside `compare_key_scores_desc`). |
| `features/key/templates.rs` | `TemplateSet`, `KeyTemplates` + `new`, `new_with_template_set`, `new_krumhansl_kessler`, `new_temperley`, `get_major_template`, `get_minor_template`, `Default` | — | `new_temperley` is reached only as a static match arm of `new_with_template_set`; Sustain calls `new()` (Krumhansl-Kessler). `get_template` is unused. |
| `features/onset/spectral_flux.rs` | `detect_spectral_flux_onsets` (+ `EPSILON`) | — | Self-contained leaf. |
| `features/period/tempogram.rs` | `estimate_bpm_tempogram`, `estimate_bpm_tempogram_impl`, `TempogramCandidateDebug`, `TempogramBandFusionConfig` | — | Sustain calls the plain entry → `_impl(None)`. The `*_band_fusion`/`*_with_candidates` entry wrappers are **not** reached. `_impl` always builds a `Vec<TempogramCandidateDebug>` (discarded) and takes `Option<TempogramBandFusionConfig>` (always `None`) — both structs come along; the band-fusion *body* is dead. |
| `features/period/novelty.rs` | `superflux_novelty`, `energy_flux_novelty`, `hfc_novelty`, `combined_novelty`, `combined_novelty_with_params`, `validate_spectrogram`, `normalize_in_place`, `local_mean_subtract`, `smooth_moving_average_in_place` (+ `EPSILON`) | — | `spectral_flux_novelty`, all `*_band` variants, `mel_superflux_novelty`, `MelFilterbank`, `mel`, `inv_mel` are **not** reached (they need a `Some(band_cfg)`). |
| `features/period/tempogram_fft.rs` | `fft_tempogram`, `find_best_bpm_fft`, `FftTempogramResult` (+ `EPSILON`) | `rustfft` | Whole module reachable. |
| `features/period/tempogram_autocorr.rs` | `autocorrelation_tempogram`, `find_best_bpm_autocorr`, `AutocorrTempogramResult` (+ `EPSILON`) | — | Pure; no FFT. |
| `features/period/mod.rs` | `BpmEstimate` struct **only** | — | `BpmCandidate`, `LegacyBpmGuardrails`, `estimate_bpm*`, and the legacy/multi-resolution submodule declarations + re-exports are **not** reached. |
| `preprocessing/normalization.rs` | whole module: `normalize`, `NormalizationConfig`, `NormalizationMethod`, `LoudnessMetadata`, `KWeightingFilter`, `calculate_lufs`, `normalize_peak`, `normalize_lufs`, `normalize_rms` (+ consts) | — | Self-contained (`AnalysisError` + `log`). Sustain selects `Loudness`; `normalize_peak` is also the quiet-fallback; `normalize_rms` survives only as a static match arm. |

**13 source files** carry reachable code. The module-declaration files
(`lib.rs`, `features/mod.rs`, `features/chroma/mod.rs`, `features/key/mod.rs`,
`features/onset/mod.rs`, `features/period/mod.rs`, `preprocessing/mod.rs`,
`analysis/mod.rs`) are trimmed to declare only the reachable submodules.

### 2.3 Not reachable — the negative space (what we do **not** vendor)

Whole files, dead from Sustain's entry points:

- `analysis/confidence.rs`, `analysis/metadata.rs`, `config.rs`, `waveform.rs`
- `features/beat_tracking/*` (5 files)
- `features/chroma/normalization.rs` (`sharpen_chroma`), `features/chroma/smoothing.rs` (`smooth_chroma`)
- `features/key/key_changes.rs`, `features/key/key_clarity.rs` (`compute_key_clarity`)
- `features/onset/{consensus,energy_flux,hfc,hpss,threshold}.rs`
- `features/period/{autocorrelation,candidate_filter,comb_filter,multi_resolution,peak_picking}.rs`
- `preprocessing/{channel_mixer,silence}.rs`
- `ml/*` (4 files, feature-gated off anyway)

> Note: `config.rs` (`AnalysisConfig`) is **not** required — Sustain passes
> explicit parameters to every DSP function and never constructs an
> `AnalysisConfig`. `analysis/confidence.rs`, `chroma/{normalization,smoothing}`,
> and `key/key_clarity` are reachable only from `analyze_audio` /
> `detect_key_multi_scale`, which Sustain does not call. (These four were
> over-reported by an automated first-pass trace that followed the crate's own
> orchestrator; the hand trace above is the authoritative split.)

### 2.4 External dependencies after the cut

| Crate | Currently | After vendoring the reachable set |
| --- | --- | --- |
| `rustfft` (6.x) | transitive via `stratum-dsp` | **direct** dep of `crates/analysis` (used by `compute_stft` + `fft_tempogram`) |
| `log` (0.4) | transitive via `stratum-dsp` | **direct** dep, or strip the `log::debug!/warn!` calls from the vendored code |
| `serde`, `serde_json` | pulled by `stratum-dsp` | **dead** once `Key`'s derive is dropped — not vendored |
| `rayon` | pulled by `stratum-dsp` | **dead** — no reachable function uses it (it lives in beat-tracking / multi-resolution) |

So the vendored core needs **`rustfft` + `log` only**. Neither is a direct
workspace dependency today.

---

## 3. Vendoring & provenance plan

### 3.1 Where the source lands

**Decision (landed): a sibling crate `crates/dsp` (`sustain-dsp`)** that
`crates/analysis` depends on — chosen over an inline module because the fork's
per-module `#[cfg(test)]` suites come along, the DSP boundary is explicit and
independently testable, and a crate boundary documents the "domain stays off
GTK/symphonia/storage" rule. The crate is pure DSP: its only dependencies are
`rustfft` and `log`. The vendored files keep their upstream module shape so the
algorithms stay legible and diffable against the fork:

```
crates/dsp/                         <- vendored from stratum-dsp@5f4b416
  Cargo.toml                        (edition 2021; license = "MIT OR Apache-2.0"; deps: rustfft, log)
  PROVENANCE.md, LICENSE-MIT, LICENSE-APACHE
  src/lib.rs                        (Sustain-authored: curated public API + module decls)
  src/error.rs                      (AnalysisError)
  src/analysis/result.rs            (Key)
  src/features/chroma/extractor.rs  (compute_stft + extract_chroma_from_spectrogram_with_options)
  src/features/key/{mod,detector,templates}.rs
  src/features/onset/spectral_flux.rs
  src/features/period/{mod,tempogram,novelty,tempogram_fft,tempogram_autocorr}.rs
  src/preprocessing/normalization.rs
```

The public surface is exactly the primitives the analyzer composes (re-exported
from the crate root); the module tree is crate-private. No orchestration API and
no genre/profile layer were added. Per-file trims and adaptations are recorded
in `crates/dsp/PROVENANCE.md`.

### 3.2 License retention (the obligation)

`stratum-dsp` is **`MIT OR Apache-2.0`** (`Cargo.toml: license = "MIT OR
Apache-2.0"`; both `LICENSE-MIT` and `LICENSE-APACHE` present at the fork root).
Both are GPL-3.0-or-later compatible, so vendoring the source into Sustain's
GPL-3.0-or-later tree is sound: the combined work is GPL-3.0-or-later, and the
permissive notices must travel with it.

Concrete obligations:

1. **Copy both license files** (`LICENSE-MIT`, `LICENSE-APACHE`) into the
   vendored directory, verbatim, retaining the upstream copyright line
   (`authors = ["HLLMR"]`).
2. **Add a `PROVENANCE.md`** in the vendored directory recording: upstream repo
   `github.com/HLLMR/stratum-dsp` (via the `open-sustain/stratum-dsp` fork),
   branch `wip/dsp-rework`, exact commit `5f4b4160441725cdd8a6e4355b6908d41b0451db`,
   the dual license, the list of files taken, and the fact that the copy is a
   **subset** (the reachable closure of §2). Sustain's own
   `// SPDX-License-Identifier: GPL-3.0-or-later` header is *not* added to the
   vendored files — they keep their `MIT OR Apache-2.0` identity.
3. **`docs/licensing.md`**: the vendored code leaves the Cargo crate graph, so
   `cargo about` will no longer harvest its notice into
   `THIRD-PARTY-LICENSES.md` (which reads `Cargo.lock`). Add an entry to
   licensing.md's "Non-Cargo components" section — parallel to the Pioneer
   interoperability constants — recording the vendored `MIT OR Apache-2.0` DSP
   and pointing at its retained `LICENSE-*` files. This is the new obligation
   carrier in place of the (now-absent) cargo-about entry.
4. **`deny.toml` / `about.toml`**: removing the `stratum-dsp` crate dep also
   removes whatever inbound licenses it dragged in (e.g. `serde`/`rayon` if not
   otherwise present). Regenerate `THIRD-PARTY-LICENSES.md` and let CI's
   `cargo-about` + `cargo-deny` jobs confirm the new set. `rustfft` (already in
   the graph transitively) becomes a direct dep — its license is already in the
   inventory, so no policy change is expected; verify, don't assume.

### 3.3 Invariants the ingest must hold (carried from #192)

- **Single symphonia.** After the ingest, the workspace resolves **one**
  symphonia (0.6). The fork's `Cargo.toml` lists symphonia only as a
  *dev-dependency* (for its example CLIs); vendoring source, not the crate,
  means none of that comes along — `crates/analysis` keeps its existing
  symphonia 0.6 (from #172) and nothing else pulls 0.5.
- **Capability gating preserved.** The whole point of bypassing `analyze_audio`
  is that "BPM only" must not pay for chroma/key/loudness/waveform. The vendored
  functions stay free-standing (no orchestrator), so `Analyzer`'s per-band
  laziness is unchanged.
- **No genre profiles.** The only tunable is the honest BPM range
  (`AnalysisOptions { min_bpm, max_bpm }`) feeding `octave_normalize` and the
  tempogram. The ingest adds **no** per-genre template/profile switching. A
  user-facing BPM-range knob in the Analysis preferences (generalist preset
  76–155, matching the synthetic corpus) is the only new surface, and it maps
  straight onto the existing `AnalysisOptions` — no new DSP concept.
- **Determinism over arbitrary tie-breaks.** Keep the fork's deterministic key
  ranking (§1.3); do not reintroduce hash-order selection. Where the DSP must
  break a tie, it breaks it deterministically and the uncertainty is surfaced
  (confidence), not hidden behind a coin-flip.
- **No hacks.** If a vendored function turns out to need something from a
  not-reachable file, that dependency is added to the closure deliberately and
  recorded here — not stubbed or worked around.

---

## 4. Baseline status

Phase 1 (`crates/analysis_bench`, committed) already established the
measurement substrate and the *current-implementation* baseline that the ingest
will be validated against:

- **Synthetic tier (committed, CI-safe):** `corpora/synthetic.toml` +
  `baselines/synthetic.json` — byte-stable `bpm`/`acoustics`/`waveform` over
  generated silence/tone/ramp/metronome fixtures. This is the regression gate
  the ingest must hold (§1.4). `key` and triad fixtures are intentionally held
  back until §1.3 lands.
- **Real-audio tier (private, never committed):** manifest-driven over the
  maintainer's local corpus, scored with the MIR metrics (BPM ±tolerance +
  metrical buckets, MIREX-weighted key). The harness and metrics are in place;
  the **real-audio quality baseline of the current implementation still needs to
  be recorded** from the private manifests before Phase 4 tuning, so "before vs
  after" is measurable on real material and not just synthetics.

Outstanding for Phase 4 entry: capture that real-audio baseline, then proceed.

---

## 5. Phase 4 (ingest) — landed

1. **Vendored** the §2.2 closure into `crates/dsp` (§3.1) with the §3.2 notices
   retained (`PROVENANCE.md`, `LICENSE-MIT`, `LICENSE-APACHE`). Pure DSP:
   `rustfft` + `log` only.
2. **Repointed** `crates/analysis` at `sustain_dsp`; dropped the `stratum-dsp`
   dependency. `rustfft`/`log` are now direct deps of the vendored crate;
   `serde`/`serde_json`/`rayon` left the analysis dependency path.
3. **Re-enabled deterministic `key`** in the synthetic corpus (triad fixtures
   restored) and asserted it in the determinism test (§1.3).
4. **Validated**: the vendored full-band path reproduces the published-1.0
   BPM/acoustics/waveform on the synthetic corpus **byte-for-byte**, so
   `ANALYZER_VERSION` stays at 5 (no change to stored values' meaning; key only
   becomes deterministic on ties, which `FILL_IF_NULL` never re-clobbers). The
   real-audio MIR baseline (§4) remains the deeper validation and is still owed.
5. Regenerated `THIRD-PARTY-LICENSES.md` (drops `stratum-dsp`); `cargo-deny`
   licenses/bans/sources ok; added the `docs/licensing.md` vendored-source
   record (with the open binary-packaging item for the MIT/Apache notice).
   Full workspace gate green.

### Still owed (follow-ups, not blockers)

- Record the **real-audio** quality baseline of the vendored implementation
  from the private manifests, so before/after BPM/key/loudness quality is
  measured on real material, not only synthetics.
- Resolve the binary-packaging attribution placement for `sustain-dsp`'s
  MIT/Apache notice (see `docs/licensing.md`).
