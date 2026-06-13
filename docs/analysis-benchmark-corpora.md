# Analysis benchmark corpora

The reference registry for the audio material Sustain uses to validate
BPM, key, and loudness/acoustics quality. It is the planning companion to
the `sustain-analysis-bench` harness (`crates/analysis_bench/`, whose
`README.md` documents the manifest format, the metrics, and the commands).

**This file is a plan, not a result.** A corpus is only usable for a
quality claim after its audio and annotations are actually present locally,
a file-level manifest with checksums is recorded, and a benchmark run is
captured. Recorded runs live in
[`analysis-benchmark-results.md`](analysis-benchmark-results.md) (aggregate
metrics only — never audio, local paths, or generated manifests). The
committed `baselines/synthetic.json` is a determinism and
constructed-ground-truth baseline only; it deliberately makes no accuracy
claim.

GiantSteps Tempo and Key have a shipped, reproducible adapter
(`analysis-bench adapt-giantsteps`) and a verified-live audio source (the
JKU mirror). A first full baseline was recorded 2026-06-13 — see
[`analysis-benchmark-results.md`](analysis-benchmark-results.md). To
reproduce or extend it, see
[Acquiring and adapting GiantSteps](#acquiring-and-adapting-giantsteps-tempo--key).

FMAK / FMAKv2 is the second key corpus with a shipped adapter
(`analysis-bench adapt-fmak`) — an independent, genre-broad cross-check on
GiantSteps Key. Its audio is the Free Music Archive, downloaded separately;
the baseline is pending a local FMA download + run. See
[Acquiring and adapting FMAK](#acquiring-and-adapting-fmak--fmakv2).

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
| GiantSteps Tempo | 2 | `bpm` | [giantsteps-tempo-dataset](https://github.com/GiantSteps/giantsteps-tempo-dataset) @ `d51ab24` | annotations in-repo (per-repo terms; contact TU Wien) | JKU mirror **live** (`cp.jku.at/datasets/giantsteps/backup/<id>.LOFI.mp3`, the primary in the repo's `audio_dl.sh`); Beatport `geo-samples` CDN **dead**; mirdata ships annotations only. Audio external, never committed. | **Adapter shipped** (`adapt-giantsteps --dataset tempo`): maps `annotations_v2/tempo/*.bpm` → per-track `bpm`, skips `0.0` sentinels, and by default requires + md5-verifies each track's `md5/` digest (`--no-md5` opts out). Baseline **recorded 2026-06-13** ([results](analysis-benchmark-results.md)). |
| GiantSteps Key | 2 | `key` | [giantsteps-key-dataset](https://github.com/GiantSteps/giantsteps-key-dataset) @ `6bcd492` | **CC BY-SA 4.0** | JKU mirror **live** (same backup path + `audio_dl.sh`); Beatport CDN dead; mirdata annotations-only. Audio external, never committed. | **Adapter shipped** (`adapt-giantsteps --dataset key`): maps `annotations/key/*.key` → per-track `key` (the harness scorer parses the label); same default-on, strict `md5/` verification as Tempo (`--no-md5` opts out). Baseline **recorded 2026-06-13** ([results](analysis-benchmark-results.md)); key result governed by a detector bug under fix (#192). |
| FMAK / FMAKv2 | 1 | `key` | FMAKv2 [Zenodo `12759100`](https://zenodo.org/records/12759100) (`fmakv2.csv`, md5 `3b2d16784ffbda850c8ddf0519478bfd`); orig. FMAK [Zenodo `10719859`](https://zenodo.org/records/10719859) | **CC BY 4.0** (annotations) | FMA audio, **per-track CC licenses** — not redistributed; downloaded as an `fma_*` archive from [mdeff/fma](https://github.com/mdeff/fma) (per-archive SHA1), kept external | **Adapter shipped** (`adapt-fmak`): maps `fmakv2.csv` (`track_id`,`key_and_mode`) → per-track `key` against an FMA audio root (`<id6[..3]>/<id6>.mp3`); annotated tracks absent from the chosen archive subset are reported and excluded. Expert song-level key/mode, 5,489 tracks / 17 genres / 24 keys — an independent second key corpus to cross-check GiantSteps. **Partial baseline recorded** (`fma_medium` subset, 1,723/5,489 covered) — see [results](analysis-benchmark-results.md); full FMAK (`fma_large`) run still pending. |
| FMA Small | 1 | `bpm` (sanity) | <https://github.com/mdeff/fma> | metadata CC BY 4.0; code MIT | per-track artist licenses | Echonest tempo labels are weak → development/tuning only, **not** final accuracy claims. Keep tuning runs separate from validation runs. |
| Freesound Loop (FSL10K) | 4 | `bpm`, `key` | <https://zenodo.org/records/3967852> | per-sound CC (see `FSL10K/metadata.json`) | downloadable from Zenodo | 9,455 loops with tempo/key/genre. Short-audio robustness; user/tag-derived BPM needs caveat-aware scoring. |
| Ballroom | 3 | — (beat grid) | <https://github.com/CPJKU/BallroomAnnotations> | annotation repo only | audio archive **external** | **Deferred**: no beat-grid metric yet. Revisit when a beat task lands; pin repo tag + archive checksum. |
| Harmonix Set | 3 | — (beat grid) | <https://github.com/urinieto/harmonixset> | annotations MIT | audio **indirect/external** | **Deferred**: beat/downbeat/segment value, no harness metric yet. |
| Beat This | research | — | <https://zenodo.org/records/13922116> | Zenodo package | mel spectrograms, **no audio for many sets** | Literature comparison only; not a sample-in audio corpus. |
| Sustain private | 5 | `bpm`, `key` | local library only | n/a | local, **never committed** | **Adapter shipped** (`adapt-private`): maps a private `reference.toml` (rich hand-curated key/BPM ground truth) + an audio root → a gitignored manifest pinned to the shipped BPM range; provenance tiers (goldish/silver) reported, not embedded. Reality-check baseline **recorded 2026-06-13** (26 tracks, 18 goldish + 8 silver) — see [results](analysis-benchmark-results.md). See "Tier 5" below. |

## Evaluation policy

- **BPM headline**: accuracy within ±2 BPM, plus mean absolute error and the
  metrical-ratio buckets.
- **Key headline (product-facing)**: the **strict harmonic-compatible rate**
  (`correct + fifth + relative` — the keys adjacent on the Camelot wheel that
  DJ/Rekordbox-style filtering treats as mixable), with a diagnostic *loose*
  rate (`+ parallel`) alongside. Sustain's product goal is harmonic
  compatibility for export/filtering, not exact key, so this is what a
  key-detection change is judged by. **Exact match** and **MIREX-weighted**
  related-key categories stay reported as research/regression metrics, and the
  predicted major/minor **mode mix** is watched so a compatible-rate gain that
  collapses mode is caught. See the "Product key-quality target" in
  [`analysis-dsp-roadmap.md`](analysis-dsp-roadmap.md) for the thresholds.
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
headline metrics alongside the result, per the policy above — the recorded
runs live in
[`analysis-benchmark-results.md`](analysis-benchmark-results.md).

## Acquiring and adapting FMAK / FMAKv2

FMAK is the second key corpus with a shipped adapter, and it is deliberately
independent of GiantSteps: different audio (Free Music Archive, not Beatport),
a far broader genre spread (17 genres), and expert song-level labels. Running
both lets a key-detection change be judged against agreement *across* corpora
rather than one corpus's idiosyncrasies.

The annotations are tiny and CC BY 4.0; the audio is the FMA, whose tracks
carry **per-track** Creative Commons licenses and are not redistributed by
Sustain. Download an FMA archive yourself and keep it outside the working tree.

1. Get the FMAKv2 annotations and verify the digest:

   ```bash
   curl -L -o fmakv2.csv 'https://zenodo.org/records/12759100/files/fmakv2.csv?download=1'
   echo '3b2d16784ffbda850c8ddf0519478bfd  fmakv2.csv' | md5sum -c -
   ```

2. Download an FMA audio archive and verify + extract it. `fma_large`
   (93 GB, 30 s clips of **all** 106,574 tracks) covers every FMAK id;
   `fma_medium` (22 GB) and `fma_small` (7.2 GB) are genre-biased subsets that
   cover only part of FMAK (the adapter reports how many annotated tracks it
   excludes for missing audio). FMA's integrity model is a per-archive SHA1:

   ```bash
   curl -O https://os.unil.cloud.switch.ch/fma/fma_large.zip
   echo '497109f4dd721066b5ce5e5f250ec604dc78939e  fma_large.zip' | sha1sum -c -
   unzip fma_large.zip          # → fma_large/<id6[..3]>/<id6>.mp3
   ```

   The clips are 30 s where FMAK keys are song-level; key is a global property,
   so a 30 s excerpt is standard MIR practice for key scoring, but record it as
   a caveat alongside any result.

3. Adapt to a (gitignored) harness manifest. The adapter is header-driven, so
   either `fmakv2.csv` or the original `keys.csv` works; it never downloads:

   ```bash
   cargo run -p sustain-analysis-bench --release -- adapt-fmak \
     --annotations fmakv2.csv \
     --audio       fma_large \
     --out         validation-data/fma/corpora/fmak_key.toml
   ```

4. Run the harness and record the result **outside** git (the repo's
   gitignored `validation-data/` workspace is the maintainer's home for this):

   ```bash
   cargo run -p sustain-analysis-bench --release -- run \
     --manifest validation-data/fma/corpora/fmak_key.toml \
     --out      validation-data/fma/results/fmak_key.json
   ```

The generated manifest is gitignored (it names local audio paths); audio and
results stay out of git. Record the harness commit,
`ANALYZER_VERSION`, `corpus_id`, the FMA archive used (and how many annotated
tracks were excluded for missing audio), and the headline metrics alongside the
result, per the policy above — recorded runs live in
[`analysis-benchmark-results.md`](analysis-benchmark-results.md).

## Tier 5 — private reality check (adapter shipped, baseline recorded)

The maintainer's own audio with hand-curated key/BPM ground truth — the best
product sanity check, and the cross-genre complement to the electronic
GiantSteps and the CC-licensed FMA. A first **reality-check baseline was
recorded 2026-06-13** (26 tracks; see
[`analysis-benchmark-results.md`](analysis-benchmark-results.md)). Everything
private stays out of git: audio, source URLs, the generated manifest, and the
per-track results all live under the gitignored `validation-data/` workspace,
and only aggregate metrics reach the results file.

The corpus's canonical form is a private `reference.toml` — one `[[tracks]]`
entry per track with the authoritative `key`, `bpm` where known, a
`confidence` tier (`goldish` = musical analysis + ≥2 independent databases
agree; `silver` = ≥3 secondary databases, the weaker label), a
`duration_seconds` hint, and rich provenance (artist/title/source URLs/notes
the harness ignores). The `adapt-private` command turns that file plus the
audio root into a gitignored harness manifest.

1. Adapt the private reference into a (gitignored) manifest. The manifest
   `id` is each file's stem; provenance tiers are reported in the summary but
   never embedded in the manifest, and the run is pinned to the analyzer's
   shipped BPM range:

   ```bash
   cargo run -p sustain-analysis-bench --release -- adapt-private \
     --reference validation-data/private/reference.toml \
     --audio     validation-data/private \
     --out       validation-data/private/private_pop_core.toml
   ```

2. Run a full report to a location **outside** the repo (the gitignored
   `validation-data/` workspace):

   ```bash
   cargo run -p sustain-analysis-bench --release -- run \
     --manifest validation-data/private/private_pop_core.toml \
     --out      validation-data/private/results/private_pop_core.json
   ```

3. Quote only the aggregate metrics and the provenance (harness commit,
   `ANALYZER_VERSION`, tier split) when reporting — never the per-track paths,
   titles, source URLs, or the manifest. The gold/silver split is recovered at
   analysis time by joining result `id`s back to the reference's `confidence`
   column. Recorded runs live in
   [`analysis-benchmark-results.md`](analysis-benchmark-results.md).
