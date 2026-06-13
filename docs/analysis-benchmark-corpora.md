# Analysis benchmark corpora

The reference registry for the audio material Sustain uses to validate
BPM, key, and loudness/acoustics quality. It is the planning companion to
the `sustain-analysis-bench` harness (`crates/analysis_bench/`, whose
`README.md` documents the manifest format, the metrics, and the commands).

**This file is a plan, not a result.** A corpus is only usable for a
quality claim after its audio and annotations are actually present locally,
a file-level manifest with checksums is recorded, and a benchmark run is
captured. Until then every real-audio row below is **pending a recorded
run** — no BPM/key accuracy number in this repository is backed by real
audio yet. The committed `baselines/synthetic.json` is a determinism and
constructed-ground-truth baseline only; it deliberately makes no accuracy
claim.

GiantSteps Tempo and Key now have a shipped, reproducible adapter
(`analysis-bench adapt-giantsteps`) and a verified-live audio source (the
JKU mirror), so their acquisition is no longer hypothetical. What remains
before any GiantSteps claim is simply running the harness over the adapted
manifest and recording the result — see
[Acquiring and adapting GiantSteps](#acquiring-and-adapting-giantsteps-tempo--key).

Registry transcribed 2026-06-13, adapted for the Sustain harness from the
prior corpus research in the `stratum-dsp` validation suite
(`validation/benchmarks/corpora.md`). The metric definitions are the same
ones the harness ports from `validation/_metrics.py`, so figures stay
comparable to that work.

## What the Sustain harness can score today

The harness measures four bands and scores the two with a clean
ground-truth contract:

- **BPM** — ±2 BPM accuracy, mean absolute error, and metrical ratio
  buckets (`1x`, `2x`, `1/2x`, …) that surface octave confusions instead of
  scoring them as plain misses.
- **Key** — MIREX categories (`correct`, `fifth`, `relative`, `parallel`,
  `other`) on pitch classes, so enharmonic spellings never matter.
- **Acoustics** and **waveform** are recorded per track for stability/diff,
  but have no external ground-truth scoring.

It does **not** yet score a beat grid (beat/downbeat F-measure). Corpora
whose value is beat/downbeat annotation are therefore deferred until a
beat-grid task exists in the harness — listing them as "available" would
imply a metric we cannot compute.

## Validation tiers

- **Tier 0 — synthetic** (`corpora/synthetic.toml`): deterministic
  recipe-built audio. Committed, CI-safe, the backbone of the determinism
  guarantee. Coarse constructed accuracy only — makes no claim.
- **Tier 1 — broad open sanity**: FMA / FMA Small (BPM sanity, decoder
  breadth) and FMAK (expert key).
- **Tier 2 — DJ/EDM**: GiantSteps Tempo (BPM) and GiantSteps Key (key) —
  the closest match to Sustain's DJ-leaning workflows, and the highest
  priority once acquired.
- **Tier 3 — beat/downbeat/grid**: Ballroom, Harmonix Set, Beat This.
  **Deferred** — no beat-grid metric in the harness yet.
- **Tier 4 — loops / short audio**: Freesound Loop Dataset (FSL10K).
- **Tier 5 — private reality check**: the maintainer's own library, by
  `path` from a gitignored manifest. Best product sanity; never committed
  as audio or path, only by opaque `id` and aggregate metrics.

## Corpus matrix

| Corpus | Tier | Harness tasks | Source | Annotation license | Audio availability | Status / next action |
| --- | --- | --- | --- | --- | --- | --- |
| GiantSteps Tempo | 2 | `bpm` | [giantsteps-tempo-dataset](https://github.com/GiantSteps/giantsteps-tempo-dataset) @ `d51ab24` | annotations in-repo (per-repo terms; contact TU Wien) | JKU mirror **live** (`cp.jku.at/datasets/giantsteps/backup/<id>.LOFI.mp3`, the primary in the repo's `audio_dl.sh`); Beatport `geo-samples` CDN **dead**; mirdata ships annotations only. Audio external, never committed. | **Adapter shipped** (`adapt-giantsteps --dataset tempo`): maps `annotations_v2/tempo/*.bpm` → per-track `bpm`, skips `0.0` sentinels, and by default requires + md5-verifies each track's `md5/` digest (`--no-md5` opts out). Baseline **pending a recorded run**. |
| GiantSteps Key | 2 | `key` | [giantsteps-key-dataset](https://github.com/GiantSteps/giantsteps-key-dataset) @ `6bcd492` | **CC BY-SA 4.0** | JKU mirror **live** (same backup path + `audio_dl.sh`); Beatport CDN dead; mirdata annotations-only. Audio external, never committed. | **Adapter shipped** (`adapt-giantsteps --dataset key`): maps `annotations/key/*.key` → per-track `key` (the harness scorer parses the label); same default-on, strict `md5/` verification as Tempo (`--no-md5` opts out). Baseline **pending a recorded run**. |
| FMAK / FMA Keys | 1 | `key` | <https://zenodo.org/records/10719860> | CC BY 4.0 (annotations) | FMA audio, **per-track licenses** — check each | Expert song-level key/mode for 5,489 songs. Build an FMAK→manifest adapter; verify per-track audio licenses before any redistribution. |
| FMA Small | 1 | `bpm` (sanity) | <https://github.com/mdeff/fma> | metadata CC BY 4.0; code MIT | per-track artist licenses | Echonest tempo labels are weak → development/tuning only, **not** final accuracy claims. Keep tuning runs separate from validation runs. |
| Freesound Loop (FSL10K) | 4 | `bpm`, `key` | <https://zenodo.org/records/3967852> | per-sound CC (see `FSL10K/metadata.json`) | downloadable from Zenodo | 9,455 loops with tempo/key/genre. Short-audio robustness; user/tag-derived BPM needs caveat-aware scoring. |
| Ballroom | 3 | — (beat grid) | <https://github.com/CPJKU/BallroomAnnotations> | annotation repo only | audio archive **external** | **Deferred**: no beat-grid metric yet. Revisit when a beat task lands; pin repo tag + archive checksum. |
| Harmonix Set | 3 | — (beat grid) | <https://github.com/urinieto/harmonixset> | annotations MIT | audio **indirect/external** | **Deferred**: beat/downbeat/segment value, no harness metric yet. |
| Beat This | research | — | <https://zenodo.org/records/13922116> | Zenodo package | mel spectrograms, **no audio for many sets** | Literature comparison only; not a sample-in audio corpus. |
| Sustain private | 5 | `bpm`, `key`, `acoustics` | local library only | n/a | local, **never committed** | See "Tier 5" below. Pending maintainer-provided paths + ground truth. |

## Evaluation policy

- **BPM headline**: accuracy within ±2 BPM, plus mean absolute error and the
  metrical-ratio buckets.
- **Key headline**: exact match plus MIREX related-key categories.
- **Beat grid**: not computed yet — out of scope until the harness grows a
  beat-grid task; do not claim beat quality beyond the synthetic fixtures.
- **No silent label trust**: weak labels (Echonest tempo, loop tags) are for
  tuning, kept separate from final validation claims.

## Acquisition rules

- Keep benchmark **audio out of git** unless redistribution rights are
  explicit. Store corpora under a documented external path (e.g.
  `~/sustain-validation-data/<corpus_id>/`), never inside the repo.
- Record, per corpus, before any result is cited: source URL, access date,
  version/commit/DOI, local path, **separate** audio and annotation
  licenses, redistribution policy, file count after filters, per-file or
  per-archive checksums, observed codec/sample-rate/channels, the tasks it
  supports, the exact metrics, and known caveats.
- Annotation licensing and audio licensing are usually different — track
  them separately.
- Private tracks stay out of git entirely: only an anonymized manifest
  (by `id`) and aggregate metrics may be committed, and only if rights are
  explicit.

## Recording a public reference baseline

Once a corpus's audio + annotations are local and its manifest is built
(per-track `path` + ground-truth `bpm`/`key`), save the manifest under
`crates/analysis_bench/corpora/<corpus_id>.toml` and run a full report:

```bash
cargo run -p sustain-analysis-bench --release -- run \
  --manifest crates/analysis_bench/corpora/giantsteps_tempo.toml \
  --out      ~/sustain-validation-data/results/giantsteps_tempo.json
```

Record alongside the result: the harness commit (`git rev-parse HEAD`),
`ANALYZER_VERSION` (currently 5), the corpus `corpus_id` + checksums, the
machine, and the headline metrics. Compare across DSP changes with
`compare` (see the harness README). Results that reference real audio paths
are **not** committed.

## Acquiring and adapting GiantSteps (Tempo / Key)

GiantSteps is the first public corpus with a shipped, reproducible adapter.
Its audio is not redistributable, so it is fetched from the upstream mirror
and kept outside the repo; only the adapter and this registry are committed.
Clone the dataset **outside** the Sustain working tree.

1. Clone the dataset at its pinned commit (annotations + md5 only — small):

   ```bash
   git clone https://github.com/GiantSteps/giantsteps-key-dataset
   git -C giantsteps-key-dataset checkout 6bcd492
   # Tempo instead: giantsteps-tempo-dataset @ d51ab24
   ```

2. Download the audio with the dataset's own script — it pulls from the JKU
   mirror and md5-checks each file. Sustain deliberately does **not**
   reimplement this. The script writes into the checkout's `audio/`:

   ```bash
   (cd giantsteps-key-dataset && bash audio_dl.sh)   # → audio/<id>.LOFI.mp3
   ```

3. Adapt to a (gitignored) harness manifest. MD5 verification is on by
   default and strict: every emitted track must carry an upstream
   `md5/<stem>.md5` digest and is re-verified against it, so a missing
   digest is a hard error (`--no-md5` opts out of verification):

   ```bash
   cargo run -p sustain-analysis-bench --release -- adapt-giantsteps \
     --dataset key \
     --repo    giantsteps-key-dataset \
     --audio   giantsteps-key-dataset/audio \
     --out     crates/analysis_bench/corpora/giantsteps_key.toml
   ```

4. Run the harness and record the result **outside** the repo:

   ```bash
   cargo run -p sustain-analysis-bench --release -- run \
     --manifest crates/analysis_bench/corpora/giantsteps_key.toml \
     --out      ~/sustain-validation-data/results/giantsteps_key.json
   ```

Swap `--dataset key` and the key repo for `tempo` and the tempo repo to
build the BPM corpus (`annotations_v2/tempo/*.bpm`; tracks annotated `0.0`
are reported and skipped). The generated `corpora/giantsteps_*.toml` is
gitignored — it names local audio paths — and audio and results stay
external. Record the harness commit, `ANALYZER_VERSION`, `corpus_id`, and
headline metrics alongside the result, per the policy above. **Until such a
run is recorded, no GiantSteps accuracy number is claimed.**

## Tier 5 — private reality check (pending corpus acquisition)

**Status: no private corpus is recorded in this environment.** This item
stays open until the maintainer provides local audio with trustworthy
ground truth. To record it:

1. Copy the template below to `crates/analysis_bench/corpora/private.toml`
   — that path is **gitignored** (`corpora/private*.toml`), so the real
   paths and titles never reach git.
2. Fill in one `[[track]]` per known track, with curated `bpm`/`key` ground
   truth. The `id` is opaque and is the only track identifier that appears
   in results — the `path` never does.
3. Run a full report to a location **outside** the repo, e.g.
   `~/sustain-validation-data/results/private.json`:

   ```bash
   cargo run -p sustain-analysis-bench --release -- run \
     --manifest crates/analysis_bench/corpora/private.toml \
     --out      ~/sustain-validation-data/results/private.json
   ```

4. Quote only the aggregate metrics (and, if useful, the harness commit and
   `ANALYZER_VERSION`) when reporting — never the per-track paths.

Private manifest template (save as `corpora/private.toml`, gitignored):

```toml
[meta]
corpus_id = "private_reality_check"
description = "Maintainer-owned tracks with curated BPM/key ground truth."

[options]
# Leave unset to use the analyzer default (76–155, the shipped preset), or
# pin the window the run should use.
# min_bpm = 76.0
# max_bpm = 155.0
tasks = ["bpm", "key", "acoustics"]

[[track]]
id = "track_0001"                 # opaque; the path below never appears in results
path = "/home/me/music/example_a.flac"
duration_secs = 251.0             # optional; makes window placement deterministic
bpm = 128.0                       # curated ground truth
key = "Am"                        # curated ground truth (any common spelling)

[[track]]
id = "track_0002"
path = "/home/me/music/example_b.flac"
bpm = 92.0
key = "F#m"
```
