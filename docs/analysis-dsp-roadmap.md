# Analysis DSP: literature extraction and roadmap

A design memo, not an implementation. It mines the research work in the
`stratum-dsp` fork Sustain's DSP was vendored from (`@5f4b416`,
`wip/dsp-rework`) and turns it into a small, testable roadmap for Sustain's
key/tempo/loudness quality — **without copying code blindly and without tuning
DSP**. The companion records are
[`analysis-dsp-ingest.md`](analysis-dsp-ingest.md) (what was vendored and why),
[`analysis-benchmark-corpora.md`](analysis-benchmark-corpora.md) (the
validation plan), and [`analysis-benchmark-results.md`](analysis-benchmark-results.md)
(recorded runs).

## Why this memo exists

The prior project's strongest asset is its **research**: a curated literature
base (`docs/literature/`, 20 paper notes + a 2026 refresh audit), a Python
validation suite with failure-mode analysers, and one internal engineering
note that documents a measured key-accuracy recipe. Its **code** is a separate
question — uneven quality, magic constants, corpus-fitted priors, and accuracy
numbers asserted against a single tiny private corpus. The split we work to:

- **Keep / mine:** bibliography, algorithm choices, corpus notes, failure-mode
  observations, evaluation metrics, provenance.
- **Reimplement / verify:** DSP code paths, defaults, scoring rules,
  dependencies, determinism, benchmark claims.
- **Do not inherit blindly:** genre profiles, hidden priors, magic constants,
  broad dependency graph, unverified "accuracy" numbers.

### The one finding that frames everything

Sustain vendored the **template-matching skeleton** (canonical Krumhansl–Kessler
templates + naive STFT chroma + a cosine match) but **not the real-world upgrade
stack** the fork's own note credits for taking key accuracy from near-zero to
72.1%. The fork's note
(`docs/literature/stratum_2025_key_detection_real_world.md`) states plainly that
"the template-matching key detector collapsed to a single key when using naive
full-track chroma averaging" — which is exactly Sustain's current chroma path,
and exactly the collapse #192 has been chasing. So Sustain's ~30–35%
MIREX-weighted key score (recorded in `analysis-benchmark-results.md`) is not a
broken detector; it is a detector running **without its front-end**. That makes
the front-end, not the scoring rule, the highest-value roadmap item.

Two caveats travel with that 72.1% number, and with every percentage in the
literature notes:

- It was measured on **n = 68** private DJ tracks ("hllmr"). The refresh audit's
  own standing principle is *"Do not tune against one corpus."* Treat 72.1% as a
  direction, never a target or a validated figure.
- Several literature notes (Gomtsyan, Müller, Ellis–Poliner) carry genre-by-genre
  accuracy tables and "+5–10%" claims with **no traceable provenance** in the
  repo. The *techniques* are real and well-cited; the *numbers* in those notes
  are not evidence.

## Bibliography → status

What the literature base actually contains, and where each item stands relative
to Sustain's vendored `crates/dsp` + `crates/analysis`.

| Paper / source (note file) | Idea | Status in Sustain |
| --- | --- | --- |
| Krumhansl & Kessler 1982 (`krumhansl_kessler_1982…`) | 24 empirical key profiles, template match | **Implemented** — canonical KK values, L2-normalised (`key/templates.rs`) |
| Temperley 1999 (cited in `templates.rs`) | Alternative key-finding profiles | Mentioned, **not implemented** |
| Gomtsyan 2019 (`gomtsyan_2019…`) | KK still competitive vs ML; clarity = max/mean | Reference; clarity **not implemented** |
| Müller & Ewert 2010 (`mueller_2010…`) | STFT chroma, freq→semitone, octave sum, L2 | **Implemented (base only)** — `chroma/extractor.rs` |
| Ellis & Poliner 2007 (`ellis_poliner_2007…`) | Temporal chroma smoothing | **Not vendored** |
| Driedger & Müller 2014 (`driedger_mueller_2014_hpss`) | Median-filter HPSS harmonic/percussive split | **Not vendored** |
| Grosche 2012 (`grosche_2012_tempogram`) | Fourier tempogram over a global novelty curve | **Implemented** — `period/tempogram_fft.rs` |
| Ellis & Pikrakis 2006 (`ellis_pikrakis_2006…`) | Autocorrelation tempo | **Implemented** — `period/tempogram_autocorr.rs` |
| Klapuri 2006 (`klapuri_2006_meter…`) | Spectral-flux novelty > energy flux | **Implemented** — `period/novelty.rs` |
| Gkiokas 2012 (`gkiokas_2012_bpm…`) | Comb filterbank, hypothesis tempos | **Not vendored** (folding scorer substitutes) |
| Schreiber & Müller 2018 (`schreiber_2018_blstm…`) | Multi-resolution tempo aggregation | **Not vendored** |
| Ellis 2007 (`ellis_2007_beat_tracking_dp`) | Global > local; DP beat tracking | Reference; no beat grid in Sustain |
| Böck 2016 (`boeck_2016…`) | HMM/Viterbi joint beat+downbeat | **Not vendored** (ML/DBN — Phase 2) |
| Bello 2005 / Bello&Sandler 2003 / McFee&Ellis 2014 / Pecan 2017 (onset notes) | Onset methods + consensus voting | **Partially** — only spectral-flux + superflux (`onset/`) |
| Schlüter & Böck 2014 (`schlueter_boeck_2014_cnn_onset`) | CNN onset | **Not vendored** — Phase 2/ML |
| ITU-R BS.1770-4 (`itu_bs1770_4_loudness`) | LUFS: K-weighting + gating | **Implemented but non-compliant** — `preprocessing/normalization.rs` |
| Stratum internal 2025 (`stratum_2025_key_detection_real_world`) | The 72.1% key recipe (n=68) | **Not vendored** — the missing stack |
| Refresh audit 2026 (`REFRESH_AUDIT_2026`) | Candidate literature + standing principles | Roadmap input (below) |

**Candidate literature flagged by the refresh audit, not yet acted on:**
Gómez 2006 (HPCP tonal description) and the MTG HPCP Vamp parameterisation;
Faraldo et al. 2017 *multi-profile EDM key estimation* (flagged "high-priority
key roadmap item"); `mir_eval` (Raffel 2014), JAMS, and `mirdata` (Bittner 2019)
for evaluation methodology; Beat This (Foscarin 2024) and Böck HMM as Phase-2/ML
beat references; FMAK / GiantSteps / Freesound Loop / Ballroom / Harmonix as
corpora.

---

## Area 1 — Key scoring and profile choice

**Literature.** Krumhansl–Kessler templates matched by dot product; because both
chroma and templates are L2-normalised, that dot product *is* cosine similarity,
which is the right cross-mode comparison. Gomtsyan reports KK remains
competitive with ML and frames a clarity metric (`max/mean`).

**What the fork measured (n=68, do not treat as validated).** On top of its
front-end, three scoring steps: separate major/minor score normalisation
(+3.0 pp), circle-of-fifths neighbour weighting (+5.9 pp), and a 3rd-scale-degree
mode heuristic. Note the **contradiction with Sustain**: the fork credits
separate major/minor normalisation as a *gain*, whereas in Sustain that same
per-mode normalisation was the #192 *bug* — it tied the top major and minor
candidate (both → 1.0) and the deterministic tie-break always chose major, so
minor was unreachable. The reconciliation is that the fork's tie was broken
downstream (mode heuristic + a different front-end), while Sustain's was not.

**What Sustain has.** Canonical KK templates (`key/templates.rs`), cosine
scoring summed over frames, the #192 fix (single shared-max normalisation,
`key/detector.rs:134`), and circle-of-fifths bonus weighting
(`key/detector.rs:155`, magic `circle_bonus_weight = 0.20`). It **lacks** the
mode heuristic and any clarity output, and carries **dead code**: `detector.rs`
computes `use_weighted_voting` (lines 240–243) and never uses it
(`final_key = best_key` always).

**Cross-corpus evidence (recorded).** GiantSteps (≈99% predicted minor) and the
FMAK/fma_medium partial baseline (98% predicted minor against a 43/57 major/minor
ground truth; major tracks score 9.8% MIREX vs minor 45.1%, dominated by
*parallel* — right-tonic-wrong-mode — errors) **agree**. The residual error is a
generic mode-discrimination weakness, not corpus noise.

**Decision.**
- *Keep:* canonical KK profiles; cosine scoring; the shared-max fix.
- *Reimplement / verify:* mode discrimination. Candidates, in increasing
  ambition — (a) **mean-centred (Pearson) correlation**, the actual
  Krumhansl–Schmuckler formulation, which removes the need for any ad-hoc
  per-mode rescaling and is the principled answer to the per-mode-norm
  contradiction; (b) alternative profiles (Temperley, Albrecht–Shanahan); (c)
  Faraldo **multi-profile** scoring (audit's high-priority item). Re-derive, do
  not port the fork's mode-heuristic constants.
- *Clean up:* remove the dead `use_weighted_voting` block.
- *Do not inherit:* the `0.20` circle-of-fifths weight, the `0.95/0.90` voting
  thresholds, the n=68 per-step deltas.
- *Success criterion:* predicted major/minor ratio within ~10 pp of ground truth
  on **both** the FMAK partial and GiantSteps, with MIREX-weighted not
  regressing on either.

## Area 2 — Chroma / HPCP front-end

**Literature.** Müller: standard STFT chroma (2048/512, A=440, octave sum, L2,
optional soft mapping). Ellis–Poliner: temporal smoothing (median, ~5 frames).
Driedger: median-filter HPSS to suppress percussion before tonal analysis. The
fork's internal note: a **key-only 8192-point STFT** (low-frequency semitone
spacing needs the resolution), an HPSS-style **harmonic mask**, and
**HPCP-style** peak+harmonic pitch-class profiles — together its single biggest
measured jump (45.6% → 63.2%). Gómez/MTG HPCP and Faraldo are the named
next-level references.

**What Sustain has.** The **base path only** (`chroma/extractor.rs`): the *same*
2048-point STFT shared with the tempogram (`crates/analysis/src/lib.rs:243`,
`STFT_FRAME_SIZE = 2048`), band-limited 100–5000 Hz, magnitude compressed
`^0.6`, soft-mapped (σ=0.5), L2-normalised **per frame**, with the per-frame
template matches then summed flat over the window. No temporal smoothing, no
HPSS, no HPCP, no tuning estimation, no key-specific STFT — the module doc says
so explicitly. This is the "naive full-track chroma
averaging" the fork's note identifies as the collapse trigger.

**Decision.**
- *Highest-value, lowest-risk roadmap item:* **decouple the key STFT from the
  tempo STFT.** The Analyzer already slices a dedicated key/BPM window; give key
  its own larger-FFT (≈8192) transform instead of reusing the 2048 tempogram
  STFT (whose size is a tempo/key compromise, per the comment at `lib.rs:239`).
  Layer in, incrementally and each verified on a benchmark: HPSS-style harmonic
  emphasis, then HPCP-style profiles, then temporal smoothing.
- *Verify, don't trust:* re-measure each increment on FMAK + GiantSteps; the
  fork's per-step deltas are n=68.
- *Do not inherit:* the `100`/`5000` Hz band-limit, the `^0.6` compression, the
  `σ=0.5` soft-mapping width — all "real-world DJ mix" priors fitted to one
  corpus; re-justify or expose as tested parameters (the audit's exact
  prescription: "explicit testable hypotheses … rather than more opaque tuning
  knobs").

## Area 3 — Tempo candidate generation and octave handling

**Literature.** Grosche (Fourier tempogram on a global novelty curve),
Ellis–Pikrakis (autocorrelation), Klapuri (spectral-flux novelty), Gkiokas
(comb-filterbank hypothesis tempos), Schreiber (multi-resolution), Ellis
(global > local). The recurring theme is that the dominant error is **metrical
level** (octave: ½×, 2×, ⅔×, 3/2×), not gross misdetection.

**What Sustain has — the best-ingested area.** `period/tempogram.rs` runs a
combined novelty curve (superflux + energy + HFC) through **both** FFT and
autocorrelation tempograms, then scores **tempo-folded candidates**
`{1, ½, 2, ⅓, 3, ⅔, 3/2}` with a blended FFT/autocorr score, a mild metrical
prior, and an explicit >180 BPM octave-fold correction. This is genuine,
faithfully-vendored prior art (Grosche-cited) and it directly targets the octave
failure mode.

**The catch.** The Analyzer calls it clamped to the user's BPM range
(**76–155**), and `octave_normalize` (`lib.rs:578`) folds again afterward. Two
consequences: the candidate scorer's `PREFERRED_MAX = 180` prior and its
`>180 → /2` correction are **inert** (no candidate above 155 is ever generated),
and genuinely fast material (GiantSteps DnB/hardcore, 160–185 BPM true) is
*unrepresentable* — it can only surface at half-tempo. The recorded GiantSteps
half-tempo cluster is therefore a **search-range** artefact, not a fold bug.

**Decision.**
- *Keep:* the dual-tempogram folding scorer as-is — it is the right design.
- *Flag, do not act (deferred by the maintainer):* the search-range vs
  display-range tension. The principled shape is to search wide (≈60–200) at the
  true metrical level and fold to the display window only at the end, so fast
  tracks are found correctly then presented in range. This is the `#192`
  range-sensitivity question; **do not widen the shipped 76–155 range** here.
- *Do not inherit without a benchmark:* multi-resolution (Schreiber) and the
  percussive-tempogram fallback (the fork's diagnostics flags hint at both) —
  Schreiber-grade complexity with no current evidence it helps Sustain's corpus.
- *Do not inherit:* the `0.55/0.45` blend, `0.80/0.90` priors, `>2.0` fold
  ratio, `60/180` preferred bounds — all unvalidated for Sustain's library.

## Area 4 — Confidence / unknown behaviour

**Literature.** The refresh audit's standing principle is explicit: *"Record
uncertainty explicitly. Arbitrary labels for ambiguous BPM/key/grid estimates
are product risk."* KK/Gomtsyan supply the mechanics: key clarity `= max/mean`,
confidence `= (best − second)/best`, with clarity correlated to accuracy.

**What the fork had (not vendored).** `analysis/confidence.rs` aggregates
per-feature confidence (BPM 40% / key 30% / grid 30%) plus a key-clarity gate
into an overall score and a level (High/Medium/Low), and emits typed
`AnalysisFlag`s (`MultimodalBpm`, `WeakTonality`, `TempoVariation`). The *shape*
is sound; the weights and thresholds (0.7/0.5 cutoffs, 0.3/0.2 flag triggers,
0.6/0.85 clarity penalties) are unvalidated magic.

**What Sustain has.** Nothing surfaced. `Analyzer::key()` discards the
confidence the detector computes and returns a bare `MusicalKey`;
`Analyzer::bpm()` returns a bare `f32`. There is no clarity output and no
"uncertain/unknown" state anywhere in `TrackAnalysis`. By the audit's own
principle, always emitting a definite key/BPM for ambiguous material is a
product risk Sustain currently carries.

**Decision.**
- *Reuse the idea:* thread the detector's confidence and a key-clarity value
  (`max/mean`) through to `TrackAnalysis`, and define an explicit *ambiguous*
  presentation when they fall below a benchmarked threshold (rather than
  printing a confident wrong key).
- *Re-derive, do not inherit:* every threshold and weight. Drop the
  `grid_stability` term entirely — Sustain has no beat grid.
- *Success criterion:* on FMAK, low-clarity tracks must measurably correlate with
  higher MIREX error before any threshold is exposed to users (verify the
  Gomtsyan/audit claim on our own data first).

## Area 5 — Loudness standard compliance

**Literature.** ITU-R BS.1770-4: K-weighting is a **two-stage** filter (a
high-shelf pre-filter ≈+4 dB above ~1.5 kHz, then an RLB high-pass ≈38 Hz);
integrated loudness uses **two** gates (absolute −70 LUFS *and* a relative
−10 LU gate), 400 ms blocks at **75% overlap**, a channel-weighted sum, and
`LUFS = −0.691 + 10·log10(mean square)`.

**What Sustain has — implemented but not compliant.** `normalization.rs`
implements a LUFS path labelled "ITU-R BS.1770-4" that is **consumed** by the
acoustics/Smart-Shuffle pass (`measure_integrated_lufs`, `lib.rs:606`). But: the
K-weighting is a **single generic RBJ high-pass biquad** (the code comment admits
it omits the shelf stage); it applies **only the −70 LUFS absolute gate** (no
relative gate); it uses **non-overlapping** 400 ms blocks; and it is mono-only.
Sustain *re-implements* the short-term / loudness-range / relative-gate layer
itself (`lib.rs`, 3 s/1 s windows, 20 LU gate, p95−p10) — reasonable EBU-R128-
style additions — but those sit on the non-compliant core.

**Decision.**
- *Reimplement or relabel:* either implement the real two-stage K-weighting (with
  the standard's tabulated coefficients), the relative gate, and 75% block
  overlap; **or** rename the measurement honestly as "approximate K-weighted
  loudness, not BS.1770-compliant." The current label is a correctness/honesty
  defect. For Smart Shuffle's *relative* ranking the systematic error largely
  cancels, so this is not urgent — but it is wrong the moment loudness is shown
  as an absolute LUFS value or used for ReplayGain-style tagging.
- *Verify:* against a reference implementation (e.g. a known-LUFS calibration
  signal or `libebur128`) once reimplemented.
- *Do not inherit:* the −14/−12 LUFS normalisation targets and the destructive
  `normalize()` gain path — Sustain *measures*, it does not re-render audio. The
  `measure-on-a-scratch-copy` dance in `measure_integrated_lufs` is a workaround
  for an API that conflates measurement with gain; a measurement-only entry point
  is the clean shape.

---

## Evaluation methodology

Already aligned, and worth protecting. `crates/analysis_bench` ports the fork's
metrics: MIREX key categories (correct/fifth/relative/parallel/other at
1.0/0.5/0.3/0.2/0.0) and BPM ±2 with metrical-ratio buckets. The refresh audit
recommends `mir_eval` / JAMS / `mirdata` *conventions* (not a Python dependency)
as the model for corpus manifests and standard metrics — a reference, not a task.
Every roadmap item above is gated on a recorded before/after run in
`analysis-benchmark-results.md`, from a clean tree, on FMAK + GiantSteps (and the
private reference corpus when it lands).

### Product key-quality target

Exact key is *not* the product goal. Sustain exports to Pioneer
XDJ/CDJ/Rekordbox-style workflows, where the operative question is whether the
predicted key lands in a **harmonically compatible set** for filtering and
mixing — not whether it nails the exact tonic+mode. So the metrics split into a
research tier and a product tier:

- **Exact key** — still reported. A useful *secondary* goal (≈40–50 %+ on
  private goldish would be good), but **not** the primary gate.
- **MIREX-weighted** — still reported, as the research/regression metric
  comparable to prior MIR work.
- **Strict harmonic-compatible rate** = `correct + fifth + relative` — the
  **product headline**. These three are adjacent on the Camelot wheel DJ
  software filters by, so a prediction in this set is harmonically usable.
- **Loose-compatible** = `correct + fifth + relative + parallel` — **diagnostic
  only**, until we confirm whether Pioneer treats the parallel key (same tonic,
  opposite mode) as compatible. `parallel` stays broken out and visible
  precisely because its usefulness in practice is unsettled — it may belong in
  the headline set later, or not.
- **`other`** = product fail.

The harness computes both compatible rates from the MIREX category histogram
(`KeySummary::strict_compatible_pct` / `loose_compatible_pct`; `run` prints them
as the key headline and `compare` shows the strict delta).

#### Two bars: minimum acceptance gate vs product ambition

The thresholds come in two tiers that must **not** be conflated — the floor a
change has to clear, and the bar the product actually aims at.

**External calibration.** On a 50-track dance/electronic set, commercial key
tools score, by *exact* key:

| Tool | Exact key (50 dance/electronic tracks) |
| --- | --- |
| Mixed In Key 12 | 94 % (47/50) |
| KeyFinder | 90 % (45/50) |
| Beatport | 88 % (44/50) |
| Rekordbox 7 | 82 % (41/50) |

These are *exact*-key figures, and Sustain's headline is the looser
*strict-compatible* rate (compatible ⊇ exact). So pegging a compatible target to
these numbers is a **conservative** reading, not an aggressive one: if
commercial tools clear 82–94 % *exact* on the product's core material, then
Sustain aiming for 85–90 % *compatible* there is simply the right bar.
Pioneer/XDJ/CDJ/Rekordbox compatibility is the real user workflow, so **Rekordbox
parity is a meaningful product benchmark**, not a stretch fantasy.

**Minimum acceptance gate** — the floor a key change must clear to be worth
keeping at all. This is the gate, **not the ambition**. (Parenthetical = current
v7 strict-compatible, all still below even the gate — this is the starting
point.)

| Corpus | Min acceptance gate (strict-compatible) | Current (v7) |
| --- | --- | --- |
| Private goldish | ≥ 75 % | 61.1 % |
| Private all-core | ≥ 65–70 % | 50.0 % |
| FMAK (`fma_medium`) | ≥ 55–60 % | 47.9 % |
| GiantSteps Key | ≥ 60 % | 51.5 % |

**Product ambition / DJ-tool parity** — the bar that actually matters, set
against the commercial tools on the core use case:

| Target set | Ambition (strict-compatible) |
| --- | --- |
| Private goldish / DJ-relevant | **≥ 85 %**, stretch ≥ 90 % |
| Dedicated dance/electronic calibration set (if built) | **≥ 90 %**, exact key tracked separately |

Public corpora (GiantSteps, FMAK) stay as **regression guards**: broader, noisier,
and harder than the product's core material (FMAK spans 17 genres; GiantSteps is
EDM- and minor-skewed). They must clear the acceptance gate and they catch
regressions, but they **do not cap ambition** — a change that lifts them *and*
holds the DJ-relevant set is good; one tuned to them at the expense of the
dance/electronic core is not. Do not overfit GiantSteps' minor skew.

Two guardrails alongside the headline:

- **Exact key** — tracked separately, never the primary gate. A useful secondary
  signal (≈ 40–50 % on private goldish is healthy today); on a dedicated
  dance/electronic set, *exact*-key parity with the tools above (~80 %+) becomes
  a real longer-term goal in its own right.
- **Mode mix must not collapse** — keep the predicted major/minor split within
  roughly 15–20 percentage points of each corpus's ground-truth mix. A change
  that lifts the compatible rate by flattening to one mode is a regression, not
  a win (the failure mode #192 has cycled through twice).

## Prioritised, testable roadmap

Ordered by expected impact against risk. **None of this is implemented in this
pass.** Each item is a hypothesis to be confirmed by a recorded benchmark
before and after.

1. **Key front-end decoupling** (Area 2) — give key its own ≈8192-point STFT +
   harmonic emphasis + HPCP, independent of the 2048 tempogram STFT. The fork's
   largest measured win and architecturally clean. *Gate:* MIREX-weighted on
   FMAK + GiantSteps.
2. **Mode discrimination** (Area 1) — mean-centred (Pearson) correlation and/or
   alternative/multi-profile templates; resolve the per-mode-normalisation
   contradiction principledly. *Gate:* predicted major/minor balance + MIREX.
3. **Confidence / unknown surface** (Area 4) — expose clarity + confidence and an
   ambiguous state; re-derive thresholds. *Gate:* clarity↔error correlation on
   FMAK. Addresses a standing product risk.
4. **Loudness compliance or honest relabel** (Area 5) — fix the K-weighting +
   gating, or stop calling it BS.1770-4. *Gate:* reference-LUFS check.
5. **(Deferred)** Tempo search-range vs display-range (Area 3). Respect the
   maintainer's deferral; do not widen 76–155 here.

## Non-goals and deferrals

- **No DSP tuning in this pass.** This memo is the plan; the next *change* waits
  on the private pop/reference corpus the side agent is building.
- **ML systems** (Beat This, Böck HMM, CNN onset) are Phase-2/v2 and conflict
  with `sustain-dsp`'s dependency hygiene (`rustfft` + `log` only). Out of scope.
- **Beat grid / downbeat** needs real corpora (Ballroom/Harmonix) before any
  claim, and is not currently a Sustain output. Out of scope.
- **The 72.1% figure** is one private corpus of 68 tracks. It is a direction, not
  a target or a validated benchmark.
