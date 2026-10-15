# OCR quality on BOINC

Issue: [#522](https://github.com/tgoodyear/usnewsmap.com/issues/522). Two apps estimate, for every page and without anyone transcribing it, how accurate and how complete each text we hold of it is, how much a careful reader could recover, and what the scan looks like. `usnm_quality` (CPU) compares the texts; `usnm_qwen` (NVIDIA GPU, with a CPU fallback) adds a vision-language reading of a sample of crops, as one more reader. The other apps are described in [boinc.md](boinc.md), [boinc-assessment.md](boinc-assessment.md) and [boinc-surya.md](boinc-surya.md).

Status: implemented in code and tested locally; UNVERIFIED on the live BOINC server and volunteer hosts. Every number below is a limit the code enforces, not a live measurement.

## What a unit is

### usnm_quality

One page per workunit. Inputs, in template order: `page.jpg` (the full-size IIIF JPEG), `loc.xml` (LoC's ALTO), `texts.json`, `places.txt` and `quality.json`. `places.txt` is the campaign's, the same physical file for every unit (its name is its checksum: `usnm_quality_places_<sha256>`), marked sticky in the input template so a host downloads it once.

`quality.json` is `usnm_ocr_contract::quality::Job`: `v` (1), `id` (the sha256 of the job's fields), `doc_id`, `language`, `group`, `inputs` (`page`, `alto`, `texts`, `places`, each `{bytes, sha256, source}`), `engine {image}`, `readers` (`[{name, source}]`, in report order), `function_words`, `crop_seed` and `k_crops`. The host checks every input against its checksum before anything runs.

`texts.json` is `quality::Texts`: `{"page": {"width", "height"}, "readers": [{"name", "source", "text", "regions": [{"box": [x0, y0, x1, y1], "kind", "legibility"}], "alto": false}]}`. Every reader the job names must be in it once with the same source. LoC's text is carried here too, for one code path; LoC's words with boxes come from `loc.xml`.

Outputs, in template order: `result.json` (`quality::ResultManifest`, at most 256 KiB), `confusions.json` (a map from reader name to its top 50 confusion pairs, at most 256 KiB) and `diag.txt` (at most 64 KiB). The unit always writes all three. `result.json` holds `v`, `job_id`, `engine`, `outcome` (`ok`, `timeout`, `failed`, `unit_error`), `unit_error`, `page {image, layout}`, `readers`, `hat`, `legibility`, `timings {decode_ms, features_ms, text_ms, total_ms, cpu_ms, peak_rss_kib}`, `host`, the checksums and sizes of the other two files, and `completed_at`.

Per reader: `name`, `source`, `chars`, `words`, `est_cer` (the hat's estimate, clipped at 0), `est_cer_raw`, `est_cer_low_bound_note`, `completeness`, `precision`, `findable_content`, `findable_places` (with the counts `content_found`, `content_total`, `places_found`, `places_total`), and `pairs`: against every other reader by name, `cer_a` (this reader as the hypothesis), `cer_b`, `cer_sym` (character edits over the mean length), `char_edits`, `chars_a`, `chars_b`, `wer_a`, `wer_b`, `word_edits`, `words_a`, `words_b` and `dice`. Rates that have no denominator (an empty consensus, an empty reader) are null.

Every float in the record is rounded to six decimals; the computation is single-threaded and uses ordered maps throughout, so two hosts' records are the same bytes.

### usnm_qwen

One page per workunit. Inputs: `page.jpg` and `qwen.json` (`usnm_ocr_contract::qwen::Job`): `v`, `id`, `doc_id`, `language`, `group`, `input` (the page), `crops` (`[{id, box, stratum, loc_text}]`, at most 64), `engine {image, llama, models: [{name, file, mmproj, min_gpu_mb}], weights}`, `prompt_version` (1), `gpu_timeout_secs`, `cpu_timeout_secs` and `max_tokens_per_1000px2`. The crops are the same ones the quality unit of the page names (the same `crop_seed` and `k_crops`), with LoC's words in each box in visual line order.

Outputs: `readings.json` (`qwen::Readings`: `v`, `job_id`, `outcome`, `device`, `model`, `prompt_version`, `readings: [{id, text, tokens, ms}]`, `diag`), `result.json` (`qwen::ResultManifest`: the outcome, device, model, times, crops read, host, and the checksums of the other two) and `diag.txt`. Outcomes: `ok` (every crop read), `timeout` (the crops read so far are in the readings), `failed`, `no_gpu` (the GPU version ran without a visible GPU) and `unit_error`.

## The estimator

Everything is in `crates/usnm-ocr-quality`, a library with no BOINC in it.

- `align`: the edit script between two sequences with a backtrace, in linear space (Hirschberg over the last-row computation, a full table under 2^18 cells), with ties broken in a fixed order. The pairwise metrics normalise both texts as the gold set does (`usnm_ocr_survey::text::normalize`), count character edits with the bit-parallel distance, word edits over whitespace-split words, and word-bag Dice over lower-case alphanumeric tokens.
- `hat`: `d_ij = e_i + e_j` solved by least squares over the pairs (normal equations, Gaussian elimination with partial pivoting). Three readers with all three pairs give the exact solution `e_A = (d_AB + d_AC - d_BC) / 2` and a zero residual. Estimates are clipped at 0 with the raw value kept; the residual is the root mean square of `d_ij - e_i - e_j`; fewer than three readers, or too few pairs, leaves the system undetermined and every estimate null. The disagreement solved on is `cer_sym`.
- `consensus`: readers' texts as tokens (normalised, split on whitespace, leading and trailing punctuation stripped, matched case-folded). Every pair of readers is aligned at word level; matches link occurrences; the connected components with members from at least two readers are the consensus words. Completeness is the share of consensus words a reader has; precision the share of its tokens that are consensus occurrences. A reader's token aligned as a substitution against a consensus word it lacks is kept for the confusion counts.
- `findability`: content words are tokens with at least four letters that are not in the job's function-word list; place names are `places.txt` tokenised the same way and matched as consecutive token sequences. Each reader's rate is the share of the consensus sequence's distinct content words, and separately its place names, that the reader's tokens contain.
- `confusion`: the character alignment of each disagreeing word pair, consecutive non-matching steps merged into one pair (`rn` for `m` is one substitution), counted per reader, the top 50 by count.
- `features`: on a grey copy downscaled by an integer factor to a long side of at most 1,024 px: skew (the angle in -3 to 3 degrees, 0.1 degree steps, at which the ink's horizontal projection profile has the highest variance; positive when lines descend to the right), blur (variance of the 3x3 Laplacian), contrast (95th less 5th percentile of grey), noise (mean absolute 3x3 median residual), bleed-through (share of pixels in the middle third of grey between the ink and background modes), ink density (share below the Otsu threshold). From the ALTO: text blocks, lines, words, median line height, columns (blocks clustered by x-centre with a gap of half the median block width), mean word confidence.
- `crops`: the gold set's recipe (`scripts/ocr-survey/crops.py`): a line with at least 4 words whose word boxes span at least 600 px; the box is the line's left and top less 15 px, to its right (at most 1,600 px from the left) plus 15 px, down to 6% of the page height below the line's top. Six strata (terciles of the block's mean word confidence by top or bottom half of the page); `k` crops (12 by default) round-robin over the strata, each stratum shuffled by SplitMix64 from the seed.
- `report`: the record above, its JSON Schema (`report::schema()`), and the rounding.

Legibility in the unit is `1 - min est_cer` over the page's readers (`basis: page-hat`). The crop-level estimate with the vision-language reader, and the join with `usnm_qwen`'s readings, is done on the server by `quality collect`, not in the unit: the two apps run on different hosts. UNVERIFIED: how far the page-level and the crop-level estimates agree.

Text from `texts.json` is used as the readers' text for every reader, LoC included; `loc.xml` is used for the features and the crops.

## The image

`usnm_quality` runs in the worker image (`ocr/boinc/Containerfile`), the one `usnm_ocr`, `usnm_assess` and `usnm_gold` use, under plan class `docker` and `ocr/boinc/job.toml` (1 CPU, 1.5 GiB). It needs nothing but `usnm-boinc`; the host does not check an engine build for it.

`usnm_qwen`'s image (`ocr/boinc/qwen/Containerfile`, `ocr/boinc/qwen/weights.lock`, `ocr/boinc/qwen/NOTICE`) is built on `ghcr.io/ggml-org/llama.cpp:server-cuda` at digest `sha256:b0baccbe2e00b5d74e63f85f9f023c93d61d0f329475269e1e7002e87eb875ea` (llama.cpp build 11610, `0.6.0-dev`), which runs Qwen3-VL natively with no Python stack. It bakes Qwen3-VL-8B-Instruct at `Q4_K_M` (`Qwen3VL-8B-Instruct-Q4_K_M.gguf` and `mmproj-Qwen3VL-8B-Instruct-F16.gguf`) and Qwen3-VL-2B-Instruct at `Q8_0` (`Qwen3VL-2B-Instruct-Q8_0.gguf` and `mmproj-Qwen3VL-2B-Instruct-F16.gguf`) from Hugging Face at pinned revisions, checked against `ocr/boinc/qwen/weights.lock`, into `/opt/qwen/models` (`usnm_boinc::qwen::MODEL_DIR`), alongside Apache-2.0 `LICENSE` and `NOTICE`. The image records `USNM_QWEN_LLAMA_IMAGE` and `USNM_QWEN_WEIGHTS`, and the host refuses a unit staged for another build (`qwen::work::prepare`). The host picks the first model whose `min_gpu_mb` the GPU has free (7,000 MB for the 8B model), else the last (the 2B model), and the 2B model on CPU. Inside the unit (`qwen::work`), `usnm-boinc` starts `/app/llama-server` on loopback (`--no-webui --ctx-size 4096 --seed 522 --temp 0`, with `--n-gpu-layers 99` when a GPU is visible and `0` on CPU) and sends each crop as a PNG data URL to `/v1/chat/completions` with prompt version 1.

`.github/workflows/boinc-qwen.yml` (`.github/actions/boinc-qwen-build/action.yml`) builds the image on the runner's scratch volume, runs `usnm-boinc qwen probe` and a synthetic CPU unit (`usnm-boinc qwen local-unit --synthetic --dir /work --timeout-secs 600`, `usnm-boinc work --dir /work`, `usnm-boinc qwen check`), and on `main` pushes `ghcr.io/tgoodyear/usnm-boinc-worker:qwen-<commit>` and writes its `sha256:...` digest to the step summary.

## App versions and plan classes

| App            | Version | Plan class           | Container                                                                     |
| -------------- | ------- | -------------------- | ----------------------------------------------------------------------------- |
| `usnm_quality` | 1.0     | `docker`             | `ocr/boinc/job.toml`: 1 CPU, 1.5 GiB, no swap                                 |
| `usnm_qwen`    | 1.0     | `docker_mt`          | `ocr/boinc/qwen/job-cpu.toml`: `--cpus` from the thread count, 6 GiB, no swap |
| `usnm_qwen`    | 1.1     | `docker_nvidia_cuda` | `ocr/boinc/qwen/job-gpu.toml`: `--gpus all`, 2 CPUs, 6 GiB, no swap           |

The two `usnm_qwen` classes are the Surya app's (`contrib-compute/boinc/plan_class_spec.xml`); `docker_nvidia_cuda` asks for 3,000 MB of GPU memory, enough for the 2B model. `scripts/boinc/install.py` knows all three (`TEMPLATES`, `JOB_TOML`). `create_work` bounds: `usnm_quality` 1e12 flops estimated, 1e14 bound, 1.5 GiB; `usnm_qwen` 1e13 estimated, 1e16 bound, 6 GiB. UNVERIFIED until the first units return.

Register the apps on the server (`scripts/start-boinc-bridge.sh --env prod --step register-apps --wait-mins 15`), then release `usnm_quality` (which uses `images.worker` from `contrib-compute/infra/boinc/env/<env>.json`) and the two `usnm_qwen` plan classes (which take `--worker-image` with the digest from `.github/workflows/boinc-qwen.yml`):

```sh
scripts/boinc-app-release.sh --env prod --app usnm_quality --version 1.0 --signing-host <host>
scripts/boinc-app-release.sh --env prod --app usnm_qwen --version 1.0 --plan-class docker_mt \
  --worker-image ghcr.io/tgoodyear/usnm-boinc-worker@sha256:<qwen-digest> --signing-host <host>
scripts/boinc-app-release.sh --env prod --app usnm_qwen --version 1.1 --plan-class docker_nvidia_cuda \
  --worker-image ghcr.io/tgoodyear/usnm-boinc-worker@sha256:<qwen-digest> --signing-host <host>
```

## Diagnostics

Both runners write `diag.txt` as the Surya worker does: from `init_data.xml` only the client version, app version, plan class, GPU type and device number, CPU count and memory bound (never the account key or host name); the container runtime and whether it is rootless; the cgroup limits and memory. `usnm_qwen` adds the NVIDIA device nodes, variables, driver and `nvidia-smi`'s output, the model chosen and llama-server's lines. `usnm_quality` adds the job's readers and the timings. A unit that cannot run (a checksum that does not match, a missing or different engine build) returns `unit_error` with the reason and no record.

## Redundancy and validation

Both apps: `min_quorum` 2, `target_nresults` 2, at most 4 results. `script_validator` runs:

- `usnm_quality_check` (`usnm-boinc quality check`): shape, sizes, UTF-8, the checksums of `confusions.json` and `diag.txt`, the record's consistency with its outcome. Every result is copied to `<project>/quality/seen/<result>/`.
- `usnm_quality_compare` (`usnm-boinc quality compare`): the same job and engine, the same outcome, and the canonical records equal: keys sorted, floats to six decimals, without the host, the timings, the time and the diagnostics. Every pair is appended to `<project>/quality/pairs.jsonl` with the verdict and, when they differ, which part differs.
- `usnm_qwen_check` (`usnm-boinc qwen check`): shape, sizes, the checksums of `readings.json` and `diag.txt`, the readings against the result (count, device, model, outcome). Every result is copied to `<project>/qwen/seen/<result>/`.
- `usnm_qwen_compare` (`usnm-boinc qwen compare`): the same job and engine, both read the page or neither, and per crop both read or neither and a word-bag Dice of at least the threshold. Until the probe has measured it, the threshold is 0.5 (`PROBE_MIN_DICE`), which only rejects unrelated text. Every pair is appended to `<project>/qwen/pairs.jsonl` with the Dice per crop and both hosts' devices and models.

The daemons are in `ocr/boinc/daemons.xml` and `contrib-compute/images/server/lib.sh`; the wrappers in `contrib-compute/images/bridge/`.

## CLI subcommands

### usnm-boinc quality

- `sample`: draws the campaign's stratified sample (`sample.jsonl`) from the prefetch manifest (`--manifest`, `--out`, `--pages` default 500, `--min-per-stratum` default 2, `--seed`, optional `--title-languages` JSON mapping LCCN to MARC language codes, optional `--with-as` listing doc ids with American Stories text, optional `--pages-file` restricting the draw to specific doc ids).
- `function-words`: writes `<out-dir>/<lang>.txt` with the top `N` (default 100) alphabetic words of at least four letters from the embedded wordfreq word list (`usnm_core::lang::wordlist`) for each `--lang` (`--lang` repeatable, `--top`, `--out-dir`).
- `texts-export`: reads the American Stories page Parquet shards (`curated/american-stories/pages/<lccn>/<year>-<nnn>.parquet`) and the newest published (`curated/ocr-local`) or pending (`--out-url`) overlay for each selected page (`--pages`/`--doc-id` or `--sample`), and writes `<out-dir>/<doc_id>.json` (`texts.json` without LoC's text, which `stage` fills from `loc.xml`) and `<out-dir>/<doc_id>.ours.xml` when our ALTO overlay exists (`--curated-url`, optional `--out-url`, `--out-dir`).
- `stage`: stages `usnm_quality` units under `<root>/units/<doc_id>/` and `<root>/places.txt`. Takes repeatable `--page group:lang:doc_id:batch[+batch...]` or `--sample` (a `sample.jsonl` from `quality sample`, with `--group` defaulting to `sample`), `--root`, `--engine-image`, `--texts-dir`, optional `--scans-dir`, `--places` (either a line-per-entry `.txt` or the catalog `places.json`, deduplicated into `<root>/places.txt`), `--function-words` or `--function-words-dir`, `--readers` (comma-separated, default `loc,as`), `--crop-seed` (default 522), `--k-crops` (default 12), `--gap-secs` (at least 5, default 5), `--max-pages` (default 20), optional `--batch-cache`, and optional `--scan-archive`. When `--scans-dir` has `<doc_id>.jpg` and `<doc_id>.xml` from the hand-off container, `stage` reads those local files without touching the network. Otherwise it uses `crates/usnm-boinc/src/scans.rs`: it checks the scan archive first when `--scan-archive` is set (`keep/<batch>/<lccn>/<date>/<edition>/<seq>.jpg`, verified against its stored `sha256`), and falls back to `tile.loc.gov` through the page's batch (`--batch-cache` caches the batch manifest and issue METS on the storage service), paced at one request at a time with at least `--gap-secs` between requests. It never calls `www.loc.gov`'s item API. It parses `loc.xml` for the page dimensions and LoC's text, merges `<texts-dir>/<doc_id>.json` (and `<doc_id>.ours.xml` when present), and writes `page.jpg`, `loc.xml`, `texts.json`, and `quality.json`.
- `submit`: hands staged units in `<root>/units/` to `create_work` (`--root`, `--project`, `--max-units` default 100), copying `places.txt` once to `download/usnm_quality_places_<sha256>` and each unit's inputs into the project's download hierarchy with write-ahead `submitting` and `submitted` markers.
- `local-unit`: writes a unit directory from local files (`--page`, `--alto`, `--texts`, `--places`, `--function-words`, `--dir`, `--engine-image`, `--crop-seed` default 522, `--k-crops` default 12) so `usnm-boinc work --dir <dir>` can run without LoC or BOINC.
- `check`: `script_validator`'s `--init_script` (`--files <result.json> <confusions.json> <diag.txt>`, optional `--keep <dir>`).
- `compare`: `script_validator`'s `--compare_script` (`--files <r1...> --files2 <r2...>`, optional `--log <pairs.jsonl>`).
- `collect`: joins the assimilated canonical results of `usnm_quality` (`--root`, `--spool`) and `usnm_qwen` (`--qwen-root`, `--qwen-spool`) for `--campaign`. Recomputes the crop-level three-cornered hat per page when a `usnm_qwen` reading is present (`legibility.basis` becomes `crop-hat+qwen`), and writes `pages.parquet`, `crops.parquet`, and `summary.md` either to `--out-dir` or to `--curated-url` under `audit/quality/<campaign>/`.
- `calibrate`: runs the estimator on gold crops with human transcriptions (`--crops <crops.jsonl>`, repeatable `--hyp ENGINE=FILE.jsonl`, optional `--readings <readings.jsonl>`, `--out <dir>`) and writes `calibration.csv`, `calibration.md`, and `by-crop.csv`.
- `estimate`: runs the quality computation offline over a directory of pages (`--dir`, `--out`, `--places`, `--function-words`, `--crop-seed` default 522, `--k-crops` default 12, repeatable `--reader NAME=PATTERN` for the OCR survey layout `<dir>/sample/{images,alto,loctext}`), verifying the originating `qwen.json` (or `readings/<page>.qwen.json`) and joining on its recorded crops when `readings.json` is present, and writing per-page `result.json`, `confusions.json`, and `crops.jsonl` plus aggregate `pages.jsonl` and `crops.jsonl`.
- `summary`: reads `<root>/units/`, `<project>/quality/seen` (`--seen`), and `<project>/quality/pairs.jsonl` (`--pairs`) and prints a Markdown summary table of kept results and compared pairs.

### usnm-boinc qwen

- `stage`: reads staged `usnm_quality` units from `--quality-root` (`--root`, `--engine-image`, `--engine-llama`, `--engine-weights`, `--gpu-timeout-secs` default 1800, `--cpu-timeout-secs` default 7200, `--max-tokens-per-1000px2` default 4, `--max-pages` default 20), cuts the same deterministic crops from each unit's `page.jpg` and `loc.xml` (`crop_seed` and `k_crops` from `quality.json`) without contacting LoC again, and writes `<root>/units/<doc_id>/{page.jpg, qwen.json}`.
- `submit`: hands staged `usnm_qwen` units in `<root>/units/` to `create_work` (`--root`, `--project`, `--max-units` default 20) with write-ahead `submitting` and `submitted` markers.
- `local-unit`: writes a unit directory from `--image` and `--alto` (or `--synthetic`, which generates a built-in two-line block-capitals page and ALTO) into `--dir` (`--engine-image`, `--engine-llama` defaulting to `USNM_QWEN_LLAMA_IMAGE`, `--engine-weights` defaulting to `USNM_QWEN_WEIGHTS`, `--crop-seed` default 522, `--k-crops` default 12, `--timeout-secs` default 1800, `--max-tokens-per-1000px2` default 4).
- `probe`: runs `/app/llama-server --version` and checks that every model and multimodal projector in `usnm_boinc::qwen::default_models()` exists under `/opt/qwen/models`, printing their sizes without loading weights into memory.
- `check`: `script_validator`'s `--init_script` (`--files <readings.json> <result.json> <diag.txt>`, optional `--keep <dir>`).
- `compare`: `script_validator`'s `--compare_script` (`--files <r1...> --files2 <r2...>`, `--min-dice` default `0.5`, optional `--log <pairs.jsonl>`).
- `summary`: reads `<root>/units/`, `<project>/qwen/seen` (`--seen`), and `<project>/qwen/pairs.jsonl` (`--pairs`) and prints a Markdown summary table of kept results and compared pairs.

## Running a campaign

### Local smoke test without LoC or BOINC

```sh
# A quality unit from local files, then the unit, then the validator's check.
usnm-boinc quality local-unit --page page.jpg --alto loc.xml --texts texts.json --places places.txt \
  --function-words function-words.txt --dir u
usnm-boinc work --dir u
usnm-boinc quality check --files u/result.json u/confusions.json u/diag.txt
# The same computation over a directory of pages, without staging.
usnm-boinc quality estimate --dir pages --out out --places places.txt --function-words function-words.txt
# A qwen unit from a page and its ALTO (the crops are cut here); inside the image, work then reads it.
usnm-boinc qwen local-unit --image page.jpg --alto loc.xml --dir q --engine-llama sha256:... --engine-weights sha256:...
```

### Server campaign through handoff-job.sh and bridge-job.sh

On the server (`contrib-compute/images/server/bridge-job.sh`) and the hand-off job in the main Container Apps environment (`contrib-compute/images/bridge/handoff-job.sh`), a quality campaign lives under `$SHARE_DIR/quality/<QUALITY_CAMPAIGN>` and a Qwen campaign under `$SHARE_DIR/qwen/<QWEN_CAMPAIGN>` (each campaign name is 1 to 41 characters of `[a-z0-9-]`). UNVERIFIED against a live server.

1. **Export page texts (`handoff-job.sh`, `HANDOFF_STEP=texts-export`).** The hand-off job reaches `CURATED_URL` and `OUT_URL` privately in the main environment, runs `usnm-boinc quality texts-export` for `HANDOFF_PAGES` (comma-separated doc ids) or `HANDOFF_SAMPLE` (a sample file under `quality-samples/` in `HANDOFF_URL`), and uploads `<doc_id>.json` (and `<doc_id>.ours.xml` when present) to `HANDOFF_URL` under `texts/`:

   ```sh
   scripts/start-boinc-bridge.sh --env prod --target handoff --step texts-export \
     --var HANDOFF_SAMPLE=v1.jsonl --wait-mins 30
   ```

2. **Stage quality units (`bridge-job.sh`, `BRIDGE_STEP=quality-stage`).** When `HANDOFF_URL` is set, pulls `texts/` into `$SHARE_DIR/quality/<QUALITY_CAMPAIGN>/texts`; when `QUALITY_LANGS` is set (comma-separated ISO 639-3 codes), writes `<campaign>/function-words` with `usnm-boinc quality function-words`. Runs `usnm-boinc quality stage` for `QUALITY_PAGES` (`group:lang:doc_id:batch[+batch...],...`) or `QUALITY_SAMPLE` (default `<campaign>/sample.jsonl`, with `QUALITY_GROUP` default `sample`), `QUALITY_READERS` (default `loc,as,ours-tesseract`), `QUALITY_PLACES` (default `<campaign>/places.txt`), `QUALITY_ENGINE_IMAGE` (`sha256:<worker-digest>`), and `QUALITY_MAX` (default 100), reading scans from `SCAN_ARCHIVE_URL` (`--scan-archive`) first and then `tile.loc.gov` through the batch (`--batch-cache`):

   ```sh
   scripts/start-boinc-bridge.sh --env prod --step quality-stage \
     --var QUALITY_CAMPAIGN=v1-probe --var QUALITY_ENGINE_IMAGE=sha256:<worker-digest> \
     --var QUALITY_LANGS=eng --var QUALITY_PAGES=probe:eng:sn83030214-1900-01-01-ed-1-seq-1:nypl_einstein_ver01 \
     --var QUALITY_MAX=20 --wait-mins 30
   ```

3. **Stage Qwen units (`bridge-job.sh`, `BRIDGE_STEP=qwen-stage`).** Cuts the deterministic crops from the staged `usnm_quality` units in `$SHARE_DIR/quality/<QUALITY_CAMPAIGN>` into `$SHARE_DIR/qwen/<QWEN_CAMPAIGN>` without fetching from LoC again:

   ```sh
   scripts/start-boinc-bridge.sh --env prod --step qwen-stage \
     --var QUALITY_CAMPAIGN=v1-probe --var QWEN_CAMPAIGN=v1-probe \
     --var QWEN_ENGINE_IMAGE=sha256:<qwen-digest> \
     --var QWEN_ENGINE_LLAMA=sha256:b0baccbe2e00b5d74e63f85f9f023c93d61d0f329475269e1e7002e87eb875ea \
     --var QWEN_ENGINE_WEIGHTS=sha256:884ccbc56c47ac7c4e74df187fa9a0ac3a3b6423d28f6132b00ca82d67ce8406 \
     --var QWEN_MAX=20 --wait-mins 15
   ```

4. **Submit both apps' workunits (`bridge-job.sh`, `BRIDGE_STEP=quality-submit` and `BRIDGE_STEP=qwen-submit`).** Hands the staged units to `create_work` with two results per workunit (`QUALITY_MAX` default 500, `QWEN_MAX` default 20):

   ```sh
   scripts/start-boinc-bridge.sh --env prod --step quality-submit \
     --var QUALITY_CAMPAIGN=v1-probe --var QUALITY_MAX=500 --wait-mins 15
   scripts/start-boinc-bridge.sh --env prod --step qwen-submit \
     --var QWEN_CAMPAIGN=v1-probe --var QWEN_MAX=20 --wait-mins 15
   ```

5. **Inspect validator summaries (`bridge-job.sh`, `BRIDGE_STEP=quality-summary` and `BRIDGE_STEP=qwen-summary`).** Reads `<project>/quality/{seen,pairs.jsonl}` and `<project>/qwen/{seen,pairs.jsonl}` and writes `<campaign>/summary.md`:

   ```sh
   scripts/start-boinc-bridge.sh --env prod --step quality-summary --var QUALITY_CAMPAIGN=v1-probe
   scripts/start-boinc-bridge.sh --env prod --step qwen-summary --var QWEN_CAMPAIGN=v1-probe
   ```

6. **Collect canonical results (`bridge-job.sh`, `BRIDGE_STEP=quality-collect`).** Joins the assimilated `usnm_quality` (`QUALITY_SPOOL`) and optional `usnm_qwen` (`QWEN_CAMPAIGN` and `QWEN_SPOOL`) results into `<campaign>/out/{pages.parquet,crops.parquet,summary.md}`, or uploads them to `QUALITY_CURATED_URL` under `audit/quality/<QUALITY_CAMPAIGN>/`:

   ```sh
   scripts/start-boinc-bridge.sh --env prod --step quality-collect \
     --var QUALITY_CAMPAIGN=v1-probe --var QUALITY_SPOOL=/project/usnmocr/results/0 \
     --var QWEN_CAMPAIGN=v1-probe --var QWEN_SPOOL=/project/usnmocr/results/0 --wait-mins 15
   ```

## Calibration

`usnm-boinc quality calibrate` runs the three-cornered hat per gold crop over `--crops <crops.jsonl>` (gold crops with human transcriptions), two or more `--hyp ENGINE=FILE.jsonl` OCR hypothesis files, and optional `--readings <readings.jsonl>` vision-language readings, and writes `calibration.csv`, `calibration.md` and `by-crop.csv` to `--out`: per reader, true CER, estimated CER, bias, MAE, Pearson and Spearman, both without (`three-ocr`) and with (`three-ocr+vlm`) the vision-language reader. UNVERIFIED on the full multi-engine gold crop campaign.

## Measured

Nothing yet. This section gets the first live campaign's and the full calibration's numbers.

## Unconfirmed

- How much the hat's estimates are biased low on real pages, where readers fail on the same smudges (the calibration's question).
- Whether `cer_sym` (edits over the mean length) is the right disagreement to solve on when one reader drops much of the page; the calibration compares it with the alternatives.
- The crop-level estimate's agreement with the page-level one.
- Time and memory per unit on volunteers' hosts, and so the `create_work` bounds and the Qwen containers' limits.
- The Dice threshold for `usnm_qwen`, to be measured from the logged pairs.
