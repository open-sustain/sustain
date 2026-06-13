# Provenance — vendored DSP + Sustain-authored extensions

This crate (`sustain-dsp`) began as a **vendored, trimmed** copy of selected
modules of **stratum-dsp**, and now also carries a small set of **original
Sustain-authored DSP extensions** (re-derived from the literature, not copied
from upstream — see "Sustain-authored additions" below). The bulk is vendored;
the extensions are individually called out.

| | |
| --- | --- |
| Upstream project | `stratum-dsp` — https://github.com/HLLMR/stratum-dsp |
| Vendored via | the `open-sustain/stratum-dsp` fork, branch `wip/dsp-rework` |
| Exact commit | `5f4b4160441725cdd8a6e4355b6908d41b0451db` |
| Upstream author | HLLMR |
| Upstream license | **MIT OR Apache-2.0** (retained — see `LICENSE-MIT`, `LICENSE-APACHE`) |

Both upstream licenses are GPL-3.0-or-later compatible. Sustain ships as
GPL-3.0-or-later overall; this vendored code keeps its permissive dual license
and its notices travel with it. Because the code is vendored source rather than
a crates.io dependency, it does **not** appear in the `cargo about`-generated
`THIRD-PARTY-LICENSES.md` (which reads `Cargo.lock`); the attribution is
recorded in `docs/licensing.md` instead, with the `LICENSE-*` files here as the
canonical text.

## Why vendored

`sustain-analysis` reached into the published `stratum-dsp` 1.0 crate for five
primitives. Vendoring the reachable subset lets Sustain (a) drop a large
dependency whose `analyze_audio` orchestration and ML/beat-tracking modules it
never used, (b) collapse the crate graph to a single symphonia and the minimal
`rustfft` + `log`, and (c) own the DSP it depends on so determinism and tuning
are under its control. The selection is the measured transitive closure of the
eight symbols the analyzer imports; the method and negative space are recorded
in `docs/analysis-dsp-ingest.md`.

## Files taken (and from where)

| This crate | Upstream path |
| --- | --- |
Every vendored `.rs` file gained a leading SPDX header (see Adaptations); the
column below describes the change *beyond* that header.

| `src/error.rs` | `src/error.rs` (SPDX header only; otherwise byte-verbatim) |
| `src/analysis/result.rs` | `src/analysis/result.rs` (trimmed to `Key`) |
| `src/features/chroma/extractor.rs` | `src/features/chroma/extractor.rs` (trimmed) |
| `src/features/key/{mod,detector,templates}.rs` | `src/features/key/{mod,detector,templates}.rs` (trimmed) |
| `src/features/onset/spectral_flux.rs` | `src/features/onset/spectral_flux.rs` (`no_run`→`ignore` doc fences; otherwise verbatim) |
| `src/features/period/{tempogram,novelty}.rs` | trimmed (`tempogram` folded to full-band; `novelty` reduced to the full-band curves) |
| `src/features/period/{tempogram_fft,tempogram_autocorr}.rs` | near-verbatim: `no_run`→`ignore` doc fence + one `#[allow(dead_code)]` diagnostic field each |
| `src/features/period/mod.rs` | `src/features/period/mod.rs` (trimmed to `BpmEstimate`) |
| `src/preprocessing/normalization.rs` | `no_run`→`ignore` doc fences; otherwise verbatim |
| `src/lib.rs`, the `mod.rs` files | Sustain-authored: curated public surface + trimmed module declarations |

## Adaptations (and why)

The vendored code is kept as close to upstream as possible. The deliberate
deviations, all recorded so a future upstream change can be re-merged:

- **Trimmed to the reachable subset.** Functions not reachable from the eight
  entry points were removed (e.g. HPCP/tuning/log-frequency/beat-synchronous
  chroma, the ensemble/multi-scale/median/mode-heuristic key detectors,
  `key_changes`/`key_clarity`, the band/mel novelty variants and
  `spectral_flux_novelty`, the legacy onset-list BPM estimators), along with
  their tests. The remaining tests are upstream's, kept verbatim where the
  function survived (one chroma test was re-pointed from the removed
  `extract_chroma` helper to the kept `extract_chroma_from_spectrogram_with_options`).
- **`estimate_bpm_tempogram` folded to the full-band path.** Upstream's
  `estimate_bpm_tempogram_impl` took an optional `TempogramBandFusionConfig`
  (multi-band + log-mel candidate generation) and returned a diagnostics
  candidate buffer; Sustain only ever called the plain entry (which passed
  `None` and discarded the diagnostics). The vendored copy constant-folds that
  `None` path: identical full-band novelty → FFT + autocorrelation tempograms →
  tempo-folded scoring with the metrical prior and the >180 BPM octave-fold
  correction, minus the unused config, mel/band novelty, and diagnostics.
- **`Key` lost its `serde` derive.** Sustain maps `Key` onto its own
  `MusicalKey` and never serializes it; dropping the derive removes the `serde`
  dependency. The `KeyType` alias (unused) was also dropped.
- **Doc-comment examples marked `ignore`.** Upstream's `no_run` examples
  `use`d deep `stratum_dsp::…` module paths that are crate-private here; they
  are illustrative only (the unit tests carry the real coverage), so their
  fences were changed to `ignore`. A runnable example lives on the crate root.
- **Error type kept as `AnalysisError`.** Retained verbatim (distinct from
  `sustain_analysis::AnalysisError`; the two never meet at a call site because
  the analyzer converts DSP results to `Option` immediately).
- **Public surface narrowed.** The module tree is crate-private; only the
  primitives the analyzer needs are re-exported from the crate root.
- **Per-file SPDX headers.** Every vendored `.rs` gained a leading
  `// SPDX-License-Identifier: MIT OR Apache-2.0` so each file states its
  license inside the otherwise GPL-3.0-or-later workspace.
- **Two `#[allow(dead_code)]` diagnostic fields.** `FftTempogramResult.power`
  and `AutocorrTempogramResult.strength` are computed and asserted by their own
  modules' tests but read by neither Sustain's consumer nor (post-trim)
  non-test code, so they carry a documented allow rather than being amputated.

## Sustain-authored additions (not vendored)

Original code written for Sustain, living in this crate because it is pure DSP
that belongs next to the primitives it composes. It was **re-derived from the
cited literature, with no upstream code copied**, so it has no upstream path to
re-merge against — when reconciling with a future `stratum-dsp`, treat these
files as Sustain's own.

| This crate | Origin |
| --- | --- |
| `src/features/chroma/hpss.rs` | Sustain-authored. Median-filter harmonic-percussive separation (soft Wiener mask), re-derived from Fitzgerald (2010) and Driedger & Müller (2014). The vendoring deliberately left HPSS out (it was trimmed as unreachable); this is a fresh implementation, not the fork's HPSS, and inherits none of the fork's corpus-fitted constants. Used by `sustain-analysis` on the key path before chroma. |
| `src/features/chroma/hpcp.rs` | Sustain-authored. Core Harmonic Pitch Class Profile — spectral peak picking with quadratic (QIFFT) interpolation, energy weighting, and a squared-cosine pitch-class window — re-derived from Gómez (2006), *Tonal Description of Music Audio Signals*. **No code was read or copied from MTG's Essentia, the AGPL-3.0 reference implementation**, and none of the upstream fork's HPCP (which was trimmed as unreachable, see Adaptations) was used. It implements only the core front-end; harmonic summation, tuning estimation, sub-semitone resolution, and max-normalization are deliberately omitted. Used by `sustain-analysis` on the key path in place of the vendored band-summed chroma. |

## License policy for this crate

**Every file in `sustain-dsp` — vendored or Sustain-authored — uses the
`// SPDX-License-Identifier: MIT OR Apache-2.0` header, matching the crate's
declared `license = "MIT OR Apache-2.0"`.** This is deliberate and overrides
the workspace-wide "new `.rs` files are GPL-3.0-or-later" rule in the root
`AGENTS.md`/`CLAUDE.md`:

- The vendored code keeps its upstream permissive license (the attribution
  must travel with it).
- New Sustain-authored DSP added here stays permissive so the crate remains
  **uniformly MIT/Apache** — a single-license, reusable, mixed-license-free DSP
  core. A GPL file would make the whole crate effectively GPL on distribution
  (GPLv3 is one-directional: it can absorb Apache-2.0/MIT, not the reverse),
  defeating the point of keeping it permissive.
- The GPL-3.0-or-later application consumes this crate freely; that direction
  is GPLv3-compatible (see <https://www.apache.org/licenses/GPL-compatibility.html>).

Do **not** "fix" a header in this crate to GPL. New files here are
`MIT OR Apache-2.0` on purpose.

## Re-merging upstream changes

Every vendored file differs from `5f4b416` by at least the added SPDX header.
Beyond that, `error.rs` is byte-identical; `spectral_flux.rs`,
`tempogram_fft.rs`, `tempogram_autocorr.rs`, and `normalization.rs` differ only
by the mechanical `no_run`→`ignore` doc-fence change (plus the one dead-code
allow in each of the two tempogram backends), so they diff to a small, obvious
delta. The trimmed files (`extractor.rs`, `detector.rs`, `templates.rs`,
`features/key/mod.rs`, `tempogram.rs`, `novelty.rs`, `analysis/result.rs`,
`features/period/mod.rs`, and the module-declaration files) require reconciling
the adaptations listed above.
