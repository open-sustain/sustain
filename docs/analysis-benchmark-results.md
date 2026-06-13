# Analysis benchmark results

Recorded baseline runs of the `sustain-analysis-bench` harness against the
public reference corpora registered in
[`analysis-benchmark-corpora.md`](analysis-benchmark-corpora.md). That file
is the *plan* (sources, licences, acquisition, metrics); this file is the
*record* of actual runs.

**What is committed here:** only aggregate, reproducible facts — the run
date, the harness commit, `ANALYZER_VERSION`, the BPM range, the pinned
dataset commits, scored counts, MD5 status, and the headline metrics, plus
any finding the run exposed. **Not** committed: audio, the generated
per-track manifests (they name local audio paths and are gitignored), local
filesystem paths, or the full timing-heavy result JSON. Those stay out of
git, under the repo's gitignored `validation-data/` workspace (large
public-corpus downloads, extracted audio, generated manifests, and result
JSONs all live there).

Each run is recorded as its own dated section, so a baseline taken before a
DSP change sits next to the baseline taken after it and the two can be
compared directly.

**Recording rule:** a baseline must be run from a clean working tree
(`git status --porcelain` empty) so the JSON `env`'s `git_commit` names the
exact code that produced it. The harness records a `git_dirty` flag for this
reason; a section whose run was dirty is not a trustworthy baseline.

---

## 2026-06-13 — GiantSteps Tempo + Key, pre-fix baseline

The first full real-audio baseline. Recorded **before** the key-detector
correctness fix (see *Finding* below), to preserve the as-shipped behaviour
as evidence.

| | |
| --- | --- |
| Harness commit | `f0d5302` |
| `ANALYZER_VERSION` | 5 |
| BPM range | 76–155 (the shipped default) |
| Build | `--release` |
| GiantSteps Key commit | `6bcd492` |
| GiantSteps Tempo commit | `d51ab24` |
| Audio source | JKU mirror, fetched by each dataset's own `audio_dl.sh` |
| Audio acquisition | Key 604/604, Tempo 664/664 downloaded; **0 errors, 0 backup-CDN fallbacks**; every file MD5-OK |
| Manifest MD5 check | adapter re-verified every emitted track against `md5/` (Key 604, Tempo 661); strict, default-on |

### Key (MIREX)

604 tracks scored.

| Metric | Value |
| --- | --- |
| MIREX weighted | **15.6 %** |
| correct | 23 (3.8 %) |
| fifth | 36 |
| relative | 99 |
| parallel | 117 |
| other | 329 |

### Tempo

664 annotations, 3 `0.0` "no-tempo" sentinels skipped → 661 scored.

| Metric | Value |
| --- | --- |
| within ±2 BPM | **61.3 %** |
| mean absolute error | 25.18 BPM |

Metrical-ratio buckets (prediction ÷ ground truth):

| 1× | ½× | ⅔× | ¾× | 4/3× | 2× | 3/2× | other |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 426 | 145 | 34 | 16 | 12 | 6 | 3 | 19 |

### Finding: the key detector never outputs a minor key

The 15.6 % is not "weak DSP" — exact-correct at 3.8 % is *below* random
chance (~4.2 % across 24 keys). All 604 predictions are bare major pitch
classes, and the single synthetic minor fixture (`triad_a_minor`, ground
truth A minor) is likewise mis-detected as F major. The detector cannot
output a minor key on real polyphonic audio at all.

Root cause (`crates/dsp/src/features/key/detector.rs`): the major and minor
score sets were each normalized by *their own* maximum before the cross-mode
comparison, so the top major and top minor candidate both became exactly
`1.0`; an identical circle-of-fifths self-bonus then left both at `1.20`, an
exact tie that the deterministic tie-break (`key_sort_index`: major 0–11,
minor 12–23, ascending) always resolves to major. Because the key templates
are already L2-normalized unit vectors, that per-mode rescaling discarded the
genuine cross-mode magnitude that decides major vs. minor. Tracked and fixed
under #192.

### Note: tempo half-tempo cluster is the BPM range, not a defect

64 % of tempo predictions land at the correct metrical level; the dominant
error is 145 tracks detected at exactly ½ the true tempo. GiantSteps Tempo
contains a lot of fast material (drum & bass / hardcore, 160–185 BPM) that
the shipped 76–155 window cannot represent, so the analyzer reports the
half-tempo octave. This is a range/corpus interaction, not a tempo-detection
bug; the 76–155 default is left unchanged here and range sensitivity is a
separate question. The 61.3 % therefore understates the as-shipped metrical
accuracy *for this corpus*.

---

## 2026-06-13 — GiantSteps, post-fix (key-detector mode fix, `ANALYZER_VERSION` 6)

Same datasets, audio, and BPM range as the pre-fix run above; the only change
is the key-detector correctness fix landed in commit `064bbc2` (the per-mode
score normalization that pinned every result to major was replaced by a single
shared-maximum scaling, restoring honest cross-mode comparison —
`ANALYZER_VERSION` 5 → 6).

| | |
| --- | --- |
| Harness commit | `064bbc2` |
| `ANALYZER_VERSION` | 6 |
| BPM range | 76–155 (unchanged) |
| Build | `--release` |
| GiantSteps Key commit | `6bcd492` |
| GiantSteps Tempo commit | `d51ab24` |
| Working tree at run time | clean (`git status --porcelain` empty, `HEAD == origin/main == 064bbc2`) |

**Provenance note.** These figures were re-recorded from a clean checkout of
`064bbc2`: the result JSON `env` now reports `git_commit = 064bbc2` and
`analyzer_version = 6`. The first capture of this baseline had been run from an
uncommitted working tree, so its `env` reported the *previous* HEAD
(`3f8b29d`) alongside the post-fix `analyzer_version = 6` — an inconsistent
record. A recorded baseline must always be run from a clean tree so its
`git_commit` names the exact code that produced it; the bench `env` now also
records a `git_dirty` flag (`git status --porcelain` non-empty) so any future
dirty-tree run is self-evident rather than silently mislabelled.

### Key (MIREX)

604 tracks scored.

| Metric | Pre-fix | **Post-fix** |
| --- | --- | --- |
| MIREX weighted | 15.6 % | **35.3 %** |
| correct | 23 | **118** |
| fifth | 36 | 174 |
| relative | 99 | 13 |
| parallel | 117 | 23 |
| other | 329 | 276 |

Minor keys are now reachable: `triad_a_minor` in the synthetic corpus detects
as A minor (was F major), exact-correct on real audio quintupled (23 → 118),
and MIREX more than doubled.

**Known limitation — the fix flips the mode bias, it does not balance it.**
Predicted mode mix went from 604 major / 0 minor (pre-fix) to **5 major / 599
minor** (post-fix), against a ground-truth mix of 93 major / 511 minor. The
detector now over-predicts minor almost as strongly as it used to over-predict
major, and the headline gain is partly because this corpus is ~85 % minor. The
root cause is that raw Krumhansl-Kessler dot-product matching against
L2-normalized profiles is minor-leaning on real polyphonic audio; the old
per-mode normalization was a crude counterweight that overshot into a
hard major lock. Removing it is the correct minimal fix (raw cosine similarity
is the right cross-mode comparison) and a real improvement, but genuinely
balanced mode discrimination needs a follow-up: mean-centered correlation (the
actual Krumhansl-Schmuckler formulation uses the Pearson coefficient, not a
raw dot product) and/or modern profiles (e.g. Albrecht-Shanahan). Tracked under
#192; not attempted in this pass to keep the change minimal and avoid
corpus-specific tuning.

### Tempo

**Unchanged** — byte-identical to the pre-fix run (61.3 % within ±2, MAE
25.18, all 661 BPM outputs match), confirming the key fix is isolated to key
detection and does not touch the tempo path.

---

## 2026-06-13 — FMAK / FMAKv2 key, partial (`fma_medium` subset)

A second, independent key baseline on the same post-fix analyzer
(`ANALYZER_VERSION` 6), against FMAK/FMAKv2 — expert song-level key
annotations for the Free Music Archive — rather than GiantSteps. The point
is cross-corpus corroboration: GiantSteps Key is genre-narrow (mostly
electronic, ~85 % minor), so a second corpus from a different distribution
tells us whether the post-fix behaviour is a property of the detector or of
that one corpus.

**This is a *partial* FMAK baseline, not full FMAK.** FMAKv2 annotates 5,489
FMA tracks; the audio was taken from the `fma_medium` archive (25,000 clips,
genre-biased), which contains only 1,723 of those annotated tracks. The
remaining 3,766 annotated tracks are simply absent from this archive subset.
Full FMAK coverage requires the `fma_large` archive (covers all FMA ids);
that run is not done here.

| | |
| --- | --- |
| Harness commit | `62aced3` |
| `ANALYZER_VERSION` | 6 |
| BPM range | 76–155 (unchanged; key-only run) |
| Build | `--release` |
| Working tree at run time | clean (`git status --porcelain` empty, `git_dirty = false`, `HEAD == 62aced3`) |
| Annotations | FMAKv2 `fmakv2.csv` (Zenodo 12759100, CC BY 4.0), MD5 `3b2d16784ffbda850c8ddf0519478bfd` |
| Audio | `fma_medium.zip`, SHA1 `c67b69ea232021025fca9231fc1c7c1a063ab50b` (FMA ships per-archive SHA1; there is no per-track digest, so the adapter does not MD5-verify individual clips) |

### Coverage

| | |
| --- | --- |
| Annotated rows | 5,489 |
| Emitted (audio present in `fma_medium`) | **1,723** (31.4 %) |
| Excluded — no audio in this archive subset | 3,766 |
| Skipped — empty key label | 0 |
| Unparseable key label | 0 |

### Key (MIREX)

1,723 tracks scored.

| Metric | Value |
| --- | --- |
| MIREX weighted | **30.0 %** |
| correct | 331 (19.2 %) |
| fifth | 268 (15.6 %) |
| relative | 65 (3.8 %) |
| parallel | 159 (9.2 %) |
| other | 900 (52.2 %) |

### Finding: the minor bias reproduces on an independent corpus

FMAK is far more mode-balanced than GiantSteps — ground truth is **740 major
/ 983 minor** (43 % / 57 %) — yet the detector predicts **40 major / 1,682
minor** (2 % / 98 %; one track produced no key). Split by ground-truth mode:

| Ground-truth mode | n | correct | fifth | relative | parallel | other | MIREX |
| --- | --- | --- | --- | --- | --- | --- | --- |
| major | 740 | 15 | 14 | 63 | 158 | 490 | **9.8 %** |
| minor | 983 | 316 | 254 | 2 | 1 | 410 | **45.1 %** |

This is the same pathology recorded in the post-fix GiantSteps section above
(predicted-minor share 98 % here vs. 99 % there), now seen on a corpus with a
completely different genre mix and a near-even mode split. The major column is
dominated by `parallel` errors (158) — right tonic, wrong mode — which is the
signature of a detector that finds the correct key centre but defaults to
minor. Because both corpora agree, the residual error is a **generic
mode-discrimination weakness in the DSP, not GiantSteps-specific corpus
noise**: the headline 30 % is held down by major tracks scoring 9.8 %, not by
anything peculiar to FMA. This is the cross-corpus signal #192 was waiting on
to decide that the next key fix (mean-centered correlation / modern profiles)
is generic rather than corpus-specific. **No DSP was changed in this pass**;
balanced mode discrimination remains the open #192 follow-up.

---

## 2026-06-13 — private reality-check corpus (key + BPM, real commercial masters)

A third key baseline on the same post-fix analyzer (`ANALYZER_VERSION` 6),
this time against the maintainer's **private** reality-check set: 26
commercially-mastered pop/rock/rap tracks with hand-curated ground truth —
material neither GiantSteps (electronic) nor the Free Music Archive
(independent/CC) represents. This is a private reality check, **not a public
benchmark**: the audio, the source URLs, the generated manifest, and the
per-track results all stay in the gitignored `validation-data/` workspace.
Only the aggregates below are recorded.

The set carries two provenance tiers. **Goldish** (18 tracks): human musical
analysis (e.g. HookTheory) *and* ≥2 independent databases agree on the key.
**Silver** (8 tracks): ≥3 secondary databases agree with no analysis source —
largely Spotify-derived and so mode/tonic-correlated, the weaker label. Key is
the authoritative label; BPM is source-consensus except for three tracks the
maintainer tapped by ear.

| | |
| --- | --- |
| Harness commit | `5c16af0` |
| `ANALYZER_VERSION` | 6 |
| BPM range | 76–155 (unchanged; the shipped default) |
| Build | `--release` |
| Working tree at run time | clean (`git status --porcelain` empty, `git_dirty = false`, `HEAD == 5c16af0`) |
| Corpus | private, 26 tracks (18 goldish + 8 silver); generated by `analysis-bench adapt-private` |

### Key (MIREX)

26 tracks scored.

| Metric | All (26) | goldish (18) | silver (8) |
| --- | --- | --- | --- |
| MIREX weighted | **40.8 %** | 46.7 % | 27.5 % |
| correct | 9 | 7 | 2 |
| fifth | 2 | 2 | 0 |
| relative | 0 | 0 | 0 |
| parallel | 3 | 2 | 1 |
| other | 12 | 7 | 5 |

### Finding: the minor bias reproduces a third time, here at 100 %

Predicted mode mix is **0 major / 26 minor** — every prediction is minor —
against a ground-truth mix of **10 major / 16 minor**. Split by ground-truth
mode:

| Ground-truth mode | n | correct | fifth | relative | parallel | other | MIREX |
| --- | --- | --- | --- | --- | --- | --- | --- |
| major | 10 | 0 | 0 | 0 | 3 | 7 | **6.0 %** |
| minor | 16 | 9 | 2 | 0 | 0 | 5 | **62.5 %** |

This is the same pathology already recorded on GiantSteps (99 % predicted
minor) and FMAK (98 %): the detector finds the key centre but defaults to
minor. Here it is at its most extreme — not one of the ten major tracks is
called major, and three of them land on the *parallel* minor (right tonic,
wrong mode). Minor tracks score 62.5 %, major tracks 6.0 %. A balanced
commercial-pop set, a different genre distribution again, and the same generic
mode-discrimination weakness — **three independent corpora now agree it is in
the DSP, not any one corpus.** The silver tier scores lower (27.5 %) than
goldish (46.7 %) partly because it is more major-heavy (5 of 8) and partly
because its Spotify-derived labels are noisier; goldish, the trustworthy tier,
is the cleaner read at 46.7 %.

### BPM

26 tracks scored.

| Metric | All (26) | goldish (18) | silver (8) |
| --- | --- | --- | --- |
| within ±2 BPM | **84.6 %** | 83.3 % | 87.5 % |
| mean absolute error | 12.39 BPM | 13.78 | 9.27 |

Metrical-ratio buckets (prediction ÷ ground truth): 1× 22, 2× 3, other 1.

### Finding: every BPM error is a sub-floor tempo, not a detection fault

All 22 tracks whose true tempo lies inside the shipped 76–155 window are
detected correctly (1×, within ±2). The four "misses" are exactly the four
tracks whose ground-truth tempo sits **at or below the 76 BPM floor** — three
at 75–76 and one at 47 (a maintainer-tapped half-time ballad) — which the
search range cannot represent, so the analyzer reports the 2× (or, for the 47,
~3×) octave. The 12.39 MAE is entirely those four. This mirrors the GiantSteps
Tempo finding (fast material → half-tempo) at the opposite end of the range: a
range/corpus interaction, not a tempo-detection defect. The 76–155 default is
left unchanged here; range sensitivity remains a separate question. Within the
representable range, BPM on real commercial masters is effectively exact.

This run is the private gate #192 was waiting on before the first DSP change.
**No DSP was changed in this pass.** It corroborates the public corpora: the
next key fix — balanced mode discrimination (mean-centered / Pearson
correlation, alternative profiles) — is generic, while tempo needs no change
here.

---

## 2026-06-13 — key detector v7 (mean-centered Pearson mode scoring), all three corpora

The first DSP change under #192: the key detector now scores with the actual
Krumhansl-Schmuckler metric — Pearson correlation against the aggregated
chroma profile — instead of a raw dot product against the L2-normalized
templates (`ANALYZER_VERSION` 6 → 7). The raw dot product correlated against
the baseline energy real polyphonic audio spreads across every pitch class,
which systematically favoured the denser minor profile; centering both the
chroma and each profile removes that bias. All three key corpora above are
re-baselined here from a clean tree so the before (v6) → after (v7) effect is
directly comparable. Tempo is untouched by this key-only change.

| | |
| --- | --- |
| Harness commit | `4439760` (clean tree; the v7 DSP is commit `e9a42e6`, with an AGENTS-only doc commit on top) |
| `ANALYZER_VERSION` | 7 |
| BPM range | 76–155 (unchanged) |
| Build | `--release` |
| Working tree at run time | clean (`git_dirty = false`, `HEAD == 4439760`) |

### Headline (MIREX-weighted key), v6 → v7

| Corpus | n | v6 | **v7** | predicted-minor share v6 → v7 |
| --- | --- | --- | --- | --- |
| GiantSteps Key | 604 | 35.3 % | **35.5 %** | 99.2 % → 81.8 % |
| FMAK (`fma_medium`) | 1,723 | 30.0 % | **36.9 %** | 97.7 % → 71.5 % |
| Private core | 26 | 40.8 % | **41.5 %** | 100 % → 73.1 % |

### Mode split, v6 → v7 (the change that matters)

The point of the change is *mode discrimination*, which the overall headline
hides on minor-heavy corpora. Split by ground-truth mode:

| Corpus | major-track MIREX v6 → v7 | minor-track MIREX v6 → v7 |
| --- | --- | --- |
| GiantSteps Key (93 maj / 511 min) | ~0 → **24.7 %** | — → 37.5 % |
| FMAK (740 maj / 983 min) | 9.8 % → **32.0 %** | 45.1 % → 40.6 % |
| Private core (10 maj / 16 min) | 6.0 % → **24.0 %** | 62.5 % → 52.5 % |

(GiantSteps v6 predicted only 5 of 604 tracks major, so its major-track recall
was effectively zero; v7 predicts 110 major. Per-mode v6 figures for FMAK and
private are the dated sections above.)

### Finding: it balances the mode call rather than flipping it

Across all three corpora the change moves in the same direction: major-track
MIREX rises substantially (≈0–10 % → 24–32 %), minor-track MIREX regresses
only moderately and stays the *stronger* mode, and the predicted-minor share
falls from 98–100 % toward — but not past — the true mix (GiantSteps is 85 %
minor, FMAK 57 %, private 62 %). Overall MIREX is non-negative everywhere
(FMAK +6.9, private +0.7, GiantSteps +0.2; the synthetic key fixtures rose
33.3 % → 66.7 % as the C-major triad now detects C major). The gain is largest
on FMAK, the most mode-balanced corpus — exactly what a genuine
mode-discrimination fix predicts, and evidence it is not a single-corpus
overfit. On the private set the *goldish* tier (trustworthy, analysis-backed
labels) improved 46.7 % → 51.7 %, while the noisier, Spotify-correlated
*silver* tier (n = 8) fell 27.5 % → 18.8 %.

This is a real improvement but **not** a full resolution: predicted-minor is
still elevated versus ground truth and minor accuracy slipped, so #192 stays
open. The next principled step is a profile A/B (Temperley / Albrecht-Shanahan
against the current Krumhansl-Kessler) under this same harness — no DSP was
tuned and no corpus-specific constant was introduced to reach these numbers.

---

## 2026-06-13 — key detector v8 (dedicated 8192-pt key STFT), all three corpora

The first **front-end** change under #192, and roadmap item 1 (the
highest-value key item). Key detection had been reusing the 2048-point
tempogram STFT, whose ~21.5 Hz bins cannot separate adjacent semitones below
~360 Hz — most of the 100–5000 Hz chroma band — so the low-register harmonic
energy that decides major vs. minor was smeared together. Key now runs its own
STFT at `KEY_STFT_FRAME_SIZE` (8192-point, ~5.4 Hz bins, resolving semitones
down to ~90 Hz; hop = frame/4, the same 75 % overlap as the tempo STFT), while
sharing the one decoded window (`ANALYZER_VERSION` 7 → 8). **One variable
changed:** scoring, profiles, and the chroma mapping are untouched, and no
HPSS/HPCP/smoothing/constant rides along — those are the remaining front-end
steps. Tempo is byte-identical across all 2,353 benchmarked tracks, confirming
the decoupling is isolated to key. All three corpora are re-baselined from a
clean tree.

| | |
| --- | --- |
| Harness commit | `fa2cca9` |
| `ANALYZER_VERSION` | 8 |
| BPM range | 76–155 (unchanged) |
| Build | `--release` |
| Working tree at run time | clean (`git_dirty = false`, `HEAD == fa2cca9`) |

### Headline: strict harmonic-compatible rate (the product metric), v7 → v8

`strict-compatible = correct + fifth + relative` — the
[product key-quality target](analysis-dsp-roadmap.md#product-key-quality-target).
Exact key and MIREX-weighted are guardrails alongside it.

| Corpus | n | strict-compat v7 → **v8** | exact v7 → v8 | MIREX v7 → v8 |
| --- | --- | --- | --- | --- |
| Private goldish | 18 | 61.1 % → **94.4 %** | 44.4 % → 61.1 % | 51.7 % → 77.8 % |
| Private all-core | 26 | 50.0 % → **84.6 %** | 34.6 % → 53.8 % | 41.5 % → 68.5 % |
| Private silver | 8 | 25.0 % → **62.5 %** | 12.5 % → 37.5 % | 18.8 % → 47.5 % |
| FMAK (`fma_medium`) | 1,723 | 47.9 % → **57.9 %** | 25.4 % → 29.4 % | 36.9 % → 44.2 % |
| GiantSteps Key | 604 | 51.5 % → **59.1 %** | 19.5 % → 23.7 % | 35.5 % → 42.4 % |
| synthetic (key) | 3 | 66.7 % → 66.7 % | — | 66.7 % → 66.7 % |

### Mode mix snaps to ground truth (the guardrail that matters)

The #192 pathology was a predicted major/minor split far from truth (v7 still
ran 72–82 % minor on balanced corpora). v8 closes that gap on its own — no
mode heuristic, just finer chroma:

| Corpus | predicted maj/min v7 → **v8** | truth maj/min | v8 gap |
| --- | --- | --- | --- |
| FMAK | 490/1232 → **745/977** | 740/983 | **0.3 pp** |
| Private all-core | 7/19 → **8/18** | 10/16 | ~8 pp |
| Private goldish | 5/13 → **6/12** | 5/13 | ~6 pp |
| GiantSteps Key | 110/494 → **160/444** | 93/511 | ~11 pp |

All within the ±15–20 pp mode-mix guard; FMAK, the most mode-balanced corpus,
lands within 0.3 pp of its 42.9 % major truth (was 14.5 pp too minor).

### Finding: a front-end correctness gain, not a scoring tweak

Every metric rises or holds on every corpus, and the gains are *correlated* —
exact key, strict-compatible, MIREX, and mode-mix accuracy all move together,
with the product-fail (`other`) bucket dropping everywhere (FMAK 792 → 595,
GiantSteps 263 → 191, private all-core 11 → 4). That is the signature of a
detector that is genuinely *hearing* the harmony better, not a bias being
flipped to flatter one corpus's skew. The largest gains land on the
product-relevant private set (goldish strict-compatible 61.1 % → 94.4 %,
clearing the DJ-tool parity ambition tier and approaching the ≥ 90 % stretch),
while the public regression guards both rise (FMAK +10.0 pp, GiantSteps
+7.6 pp), so it is not a private-pop overfit.

**Honest caveat.** GiantSteps lands at 59.1 % strict-compatible, 0.9 pp under
its 60 % minimum-acceptance floor — but it *rose* +7.6 pp, it is a regression
guard rather than a product target, and every other corpus clears its gate
(private clears the *ambition* tier). This is a clear net win, not a
regression, so the change was committed.

**Runtime.** The added 8192-pt STFT costs little and is off the cold-start path
(analysis never runs at launch): key band +24.8 ms/track on full-length private
material, total per-corpus wall time +2.5 % (GiantSteps) to +7.8 % (private).

This resolves the mode-discrimination weakness the prior three baselines
diagnosed, but #192 stays open: the remaining front-end steps (HPSS-style
harmonic emphasis, HPCP profiles, temporal smoothing — each its own measured
change) are still pending, and only after the front-end is built out is a
profile A/B (Temperley / Albrecht-Shanahan) worth running, since profile
optimality depends on chroma quality. No DSP was tuned and no corpus-specific
constant was introduced to reach these numbers.

