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
filesystem paths, or the full timing-heavy result JSON. Those stay external
(the maintainer keeps them under `~/sustain-validation-data/`).

Each run is recorded as its own dated section, so a baseline taken before a
DSP change sits next to the baseline taken after it and the two can be
compared directly.

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
