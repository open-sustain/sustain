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
# separately with the dataset's own audio_dl.sh) into a gitignored manifest,
# md5-verifying the audio against the upstream digests (--no-md5 opts out):
cargo run -p sustain-analysis-bench --release -- adapt-giantsteps \
  --dataset key \
  --repo  ../giantsteps-key-dataset \
  --audio ../giantsteps-key-dataset/audio \
  --out   crates/analysis_bench/corpora/giantsteps_key.toml

# Before/after a DSP or decoder change (e.g. #172):
cargo run -p sustain-analysis-bench --release -- compare \
  --baseline /tmp/before.json --candidate /tmp/after.json

# Just write the synthetic fixtures somewhere (inspection/manual decode):
cargo run -p sustain-analysis-bench -- gen \
  --manifest crates/analysis_bench/corpora/synthetic.toml --out /tmp/fixtures
```

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
  on pitch classes so enharmonic spellings never matter.

[`Analyzer`]: https://docs.rs/sustain-analysis
