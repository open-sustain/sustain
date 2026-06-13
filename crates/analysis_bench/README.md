# sustain-analysis-bench

Benchmark and validation harness for Sustain's audio-analysis pipeline
(`sustain-analysis`). It runs the real [`Analyzer`] over a manifest of
tracks, records the outputs and per-band timings, and — where the manifest
carries ground truth — scores them with standard MIR metrics. This is the
measurement substrate behind the "best in class" BPM/key/loudness goal:
**we never change the DSP blind, we baseline first.**

It is developer/validation tooling — not linked into the `sustain` binary,
so it has no bearing on the cold-start budget — but it is a full workspace
member, so `cargo fmt`/`clippy`/`test` cover it like everything else.

## Current recorded status

The current stop point for #192 is `ANALYZER_VERSION` **12**: dedicated
8192-point key STFT (v8), HPSS harmonic emphasis (v9), a Sustain-authored
core HPCP front-end (v10), HPCP harmonic summation (v11), and HPCP global
tuning estimation (v12). The detailed, aggregate-only record is
[`docs/analysis-benchmark-results.md`](../../docs/analysis-benchmark-results.md);
this README keeps the short operational snapshot for the next DSP pass.

Key quality is judged by **strict harmonic-compatible rate** (`correct + fifth + relative`),
because Sustain's product target is DJ/Pioneer/Rekordbox-style compatible
filtering, not exact key alone. At v12:

| Corpus | n | strict-compatible | exact | notes |
| --- | ---: | ---: | ---: | --- |
| Private goldish | 18 | **100.0 %** | 77.8 % | trusted product-tier labels; 0 `other`; preserved through v12 |
| Private all-core | 26 | **96.2 %** | 65.4 % | 18 goldish + 8 silver; exact −3.8 pp vs v11 (one silver track) |
| FMAK (`fma_medium`) | 1,723 | **72.6 %** | 49.8 % | broad public key regression corpus |
| GiantSteps Key | 604 | **64.7 %** | 44.5 % | strict +1.8 pp vs v11; third straight front-end gain |

Aggregate over the 2,353 scored real key tracks: strict-compatible **70.8 %**
(+0.8 pp vs v11), exact **48.6 %** (+0.4 pp), MIREX +0.5 pp; both public corpora
up on strict/exact/MIREX and `other` drops on each. BPM is byte-identical to v11.
Runtime cost is light (key time/track +4-7 %, background analysis). The one
caveat: private all-core exact −3.8 pp from a single silver track moving
`correct → fifth` (strict and goldish untouched). The open issue is unchanged:
the GiantSteps mode-mix gap (predicted-minor 53.5 % against 84.6 % minor truth,
~31 pp) is moved by neither v11 nor v12. Future key work should address that
mode/parallel boundary without broad constant sweeps.

BPM quality at the shipped 76-155 range is currently limited more by corpus
range than by gross detection: the private corpus has every in-range track
exact within +/-2 BPM (22/22; overall 84.6 % because four labels sit at or
below the 76 BPM floor), while GiantSteps Tempo records 61.3 % within +/-2 BPM
because fast 160-185 BPM material is unrepresentable under the current range
and lands at half tempo. Do not read those range artefacts as a tempogram
regression.

## Fixture tiers

- **Tier 0 — synthetic** (`corpora/synthetic.toml`): silence, tones, a
  ramp, metronomes, and triads, generated deterministically from a recipe.
  Committable (we commit the recipe, never audio), CI-safe, and the backbone
  of the determinism guarantee. Ground truth is exact by construction, so it
  also yields a coarse accuracy figure — but **this tier does not make
  accuracy claims.**
- **Tiers 1–4 — public reference corpora**: GiantSteps Tempo/Key, FMAK,
  Freesound Loop, and friends — the real external quality signal. These are
  acquired locally (audio never committed) and scored per the registry in
  [`docs/analysis-benchmark-corpora.md`](../../docs/analysis-benchmark-corpora.md),
  which records each corpus's source, licenses, checksums, and the tasks it
  supports. GiantSteps has a ready adapter — `adapt-giantsteps` (below) —
  that turns a pinned checkout plus a downloaded-audio directory into a
  scored manifest. No accuracy claim until a corpus is present and a run
  recorded.
- **Tier 5 — private real-audio**: corpora such as the maintainer's local
  `test-library/`, referenced by `path` from a **gitignored** manifest.
  These carry the real BPM/key/loudness quality signal. Audio is never
  committed; only anonymized manifests (by `id`, never a path) and aggregate
  results are.

## Commands

```bash
# Record the committed synthetic baseline (byte-stable; no env/timings):
cargo run -p sustain-analysis-bench --release -- run \
  --manifest crates/analysis_bench/corpora/synthetic.toml \
  --out      crates/analysis_bench/baselines/synthetic.json \
  --reproducible

# Run a private real-audio corpus (full report, with timings):
cargo run -p sustain-analysis-bench --release -- run \
  --manifest crates/analysis_bench/corpora/private.toml \
  --out /tmp/private-results.json

# Adapt a local GiantSteps Tempo/Key checkout (annotations + audio fetched
# separately with the dataset's own audio_dl.sh) into a gitignored manifest.
# MD5 verification is required by default: every emitted track must have an
# upstream md5/<stem>.md5, and a missing digest is a hard error (--no-md5
# opts out of verification entirely):
cargo run -p sustain-analysis-bench --release -- adapt-giantsteps \
  --dataset key \
  --repo  ../giantsteps-key-dataset \
  --audio ../giantsteps-key-dataset/audio \
  --out   crates/analysis_bench/corpora/giantsteps_key.toml

# Adapt the FMAK / FMAKv2 key annotations (a single CSV keyed by FMA
# track_id) against an extracted Free Music Archive audio root. The audio is
# downloaded separately as an fma_* archive (fma_large covers all FMAK ids);
# annotated tracks absent from the chosen archive subset are reported and
# excluded, never silently dropped:
cargo run -p sustain-analysis-bench --release -- adapt-fmak \
  --annotations ../fmakv2.csv \
  --audio       ../fma_large \
  --out         crates/analysis_bench/corpora/fmak_key.toml

# Adapt the maintainer's private key/BPM reference set (a rich reference.toml
# of hand-curated ground truth, with audio alongside it) into a gitignored
# manifest pinned to the shipped BPM range. Provenance tiers (goldish/silver)
# are reported but not embedded; reference rows with no audio are reported and
# excluded. Everything here stays outside git — audio, manifest, and results:
cargo run -p sustain-analysis-bench --release -- adapt-private \
  --reference validation-data/private/reference.toml \
  --audio     validation-data/private \
  --out       validation-data/private/private_pop_core.toml

# Before/after a DSP or decoder change (e.g. #172):
cargo run -p sustain-analysis-bench --release -- compare \
  --baseline /tmp/before.json --candidate /tmp/after.json

# Just write the synthetic fixtures somewhere (inspection/manual decode):
cargo run -p sustain-analysis-bench -- gen \
  --manifest crates/analysis_bench/corpora/synthetic.toml --out /tmp/fixtures
```

A full `run` captures an `env` block recording the build profile, the
`ANALYZER_VERSION`, the host, and — for provenance — the short git commit and
a `git_dirty` flag. **Record reference baselines only from a clean working
tree** (`git status --porcelain` empty): the commit hash alone names HEAD and
cannot reveal uncommitted edits, so a dirty-tree run would label its outputs
with a commit that did not produce them. `git_dirty` makes such a run
self-evident.

`--reproducible` drops the machine-specific environment capture and **all
wall-clock timings**, leaving only the deterministic outputs and scores, so
the file diffs meaningfully. Timing baselines are inherently machine-specific
(the maintainer's reference machines are a Ryzen AI Max+ 395 and a Ryzen
7900) and are produced per-machine with a plain `run`, then `compare`d — they
are not committed.

## Manifest format

```toml
[meta]
corpus_id = "my_corpus"
description = "..."

[options]
min_bpm = 76.0          # optional; defaults to the analyzer's own range
max_bpm = 155.0
tasks   = ["bpm", "key"]  # optional; default is every capability

# A generated fixture:
[[track]]
id = "click_120"
synthetic = { kind = "click_train", bpm = 120.0, secs = 20.0 }
bpm = 120.0             # optional ground truth

# Real audio (gitignored manifest only):
[[track]]
id = "track_0001"       # opaque id — appears in results, the path never does
path = "/home/me/music/a.flac"
duration_secs = 251.0   # optional; makes window placement deterministic
bpm = 128.0             # optional ground truth
key = "Am"              # optional ground truth (any common spelling)
tasks = ["bpm"]         # optional per-track capability override
```

Synthetic `kind`s: `silence`, `tone` (`freq_hz`), `ramp`, `click_train`
(`bpm`), `triad` (`root_pc` 0–11, `minor`). All take `secs`, `sample_rate`
(default 44100), and — where meaningful — `channels`.

## Metrics

Ported from the stratum-dsp validation suite (`validation/_metrics.py`,
`_keys.py`) so figures are comparable to that prior work:

- **BPM** — absolute error, ±2 BPM accuracy, and metrical ratio buckets
  (`1x`, `2x`, `1/2x`, `3/2x`, `2/3x`, `4/3x`, `3/4x`, `other`) that surface
  octave/harmonic confusions instead of scoring them as plain misses.
- **Key** — MIREX categories (`correct`, `fifth`, `relative`, `parallel`,
  `other`) with the standard weights (1.0 / 0.5 / 0.3 / 0.2 / 0.0), compared
  on pitch classes so enharmonic spellings never matter. From the category
  histogram the run also reports the **product-facing key headline**: the
  *strict harmonic-compatible* rate (`correct + fifth + relative` — exact, a
  fifth away, or the relative major/minor, all adjacent on the Camelot wheel
  that DJ/Rekordbox-style filtering uses) and a diagnostic *loose* rate (strict
  `+ parallel`). Exact-match and MIREX-weighted stay as research/regression
  metrics; the compatible rate is what a key-detection change is judged by. See
  the "Product key-quality target" in
  [`docs/analysis-dsp-roadmap.md`](../../docs/analysis-dsp-roadmap.md).

[`Analyzer`]: https://docs.rs/sustain-analysis
