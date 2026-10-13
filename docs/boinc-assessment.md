# JP2 against IIIF JPEG on BOINC

Issue: [#456](https://github.com/tgoodyear/usnewsmap.com/issues/456). Background: the [#435 report](reports/2026-10-jp2-vs-jpeg-ocr/report.md), which compared the two formats on 80 pages that were not a random sample. This page covers the second BOINC app, `usnm_assess`, which runs the same comparison on a stratified random sample of the corpus on volunteer hosts, and the server steps around it. The OCR pilot (`usnm_ocr`) is described in [boinc.md](boinc.md); the server hosting is in [contrib-compute/docs](../contrib-compute/docs/design.md).

Status: written and unit-tested. No campaign has run, and nothing here has been deployed.

## What a unit is

One page per workunit, in one of two modes:

| Mode    | Inputs                                    | What the host runs                                           | Page deadline |
| ------- | ----------------------------------------- | ------------------------------------------------------------ | ------------- |
| `ocr`   | the JP2, the full-size IIIF JPEG, the job | Tesseract on the JP2, then on the JPEG, to the end           | 1200 s        |
| `crash` | the JP2, the job                          | Tesseract on the JP2 until it crashes or the deadline passes | 20 s          |

Every result has the same three files: `text-jp2.txt`, `text-jpeg.txt` (empty for a crash unit) and `result.json`. For each format, `result.json` records the outcome (`ok`, `crash_136`, `timeout`, `other`), the exit code or signal, engine milliseconds, peak memory (the highest `VmHWM` of the Tesseract process, sampled every 100 ms) and the text's SHA-256. The types are in `crates/usnm-ocr-contract/src/assess.rs`.

The engine's exit is a result, not an error. A JP2 that makes Tesseract exit with status 136, or die from signal 8 when no shell or container runtime sits in between (the #455 crash), is recorded as `crash_136` and validated like any other outcome. The unit fails only when the unit itself is wrong: an input checksum, a model file, or an engine version or language route that differs from the job.

## Engine and profile

The host runs `usnm-boinc work` in the same worker image as the OCR pilot (`ocr/boinc/Containerfile`); the presence of `assess.json` in the slot selects the assessment. Tesseract runs with the production worker's arguments: `<image> page -l <profile> --psm 3 --dpi 300 txt alto`, one thread, the language profile the worker's router picks (`frak2021` for German), inside the Docker wrapper's 1536 MiB, one-CPU container (`ocr/boinc/job.toml`).

The engine build is pinned and can be swapped. Each job carries the worker image's full digest, the digest of the Tesseract image it was built on, the Tesseract version, the profile and the model digest. The worker image records its Tesseract image as `USNM_TESSERACT_IMAGE` at build time; the host refuses a unit staged for another one, or whose installed version or model files differ, and the validator refuses to pair results from different engines. The worker image cannot read its own digest, so that field is the operator's record of which app version the units were staged for. To test another engine build (for example a fix from #455):

1. Build the worker image `FROM` the new Tesseract image (`--build-arg TESSERACT_IMAGE=...`).
2. Install it as a new `usnm_assess` app version (`scripts/boinc/install.py --app usnm_assess --version 2.0 --image ...`), sign it, and run `bin/update_versions`.
3. Stage with `--engine-image` and `--engine-base` set to the new digests (and `--engine-version` if the version changed) into a fresh `--root` (a new `ASSESS_CAMPAIGN` on the bridge job). Job ids include the engine, so the two campaigns never mix.

## Sample

`usnm-boinc assess sample` reads a prefetch manifest (`manifest.parquet`) and writes two JSON Lines files:

- the OCR sample (`--ocr-pages`, default 3000), and
- the crash-only list (`--crash-pages`, default 20000), drawn from pages not in the OCR sample.

Strata are language (ISO 639-3, as the production message carries it) × decade × batch prefix (the batch name up to its first `_`, such as `mdu`) × LoC damage rate in three bins (`low` under 0.1, `mid` under 0.5, `high`). Each stratum gets pages in proportion to its size, with a floor of `--min-per-stratum` (default 2) or the whole stratum if it is smaller. Each row carries the stratum's size (`stratum_pages`) and two weights: `weight`, the stratum's pages divided by the OCR pages drawn from it, for quality and speed; and `crash_weight`, the stratum's pages divided by all pages drawn from it (OCR and crash), for crash rates, since every unit reads the JP2. The draw orders each stratum by SHA-256 of `--seed` and the doc id, so the same manifest and seed give the same sample. Output is in that hash order, so any prefix of a run is mixed.

The usual prefetch manifest holds only pages above a damage rate of 0.25, which is not the corpus. Build one for the sample with `usnm-ingest ocr-prefetch-manifest --min-damage 0`; the sampler refuses a manifest whose lowest damage rate is 0.1 or more. That tool keeps pages whose rate is above the minimum, so pages with a rate of exactly 0 are still left out, and it keeps one copy of each duplicated page. Estimates cover that population.

Pages without a detected language use the title's first language if `--title-languages` (a JSON object, LCCN to MARC codes) is given; otherwise they are left out. Pages whose language has no locked model are left out too, as production leaves them. The counts go to stderr.

## Fetching from LoC

`usnm-boinc assess stage` fetches the inputs on the server side, never on hosts, with the rules of the #435 report's `fetch.py`:

- one request at a time, at least 5 s apart (`--gap-secs`, which cannot go lower), on the storage service and the image server, which share one pacer;
- no retries; a 403, 429 or 5xx, or a network failure, stops the run and writes `HOLD` under the staging root, and no later run starts until a person has looked and removed it;
- a 404 marks the page `missing` and moves on.

It resolves the JP2 through the batch's bag manifest and checks its md5, then (OCR units) fetches the full-size IIIF JPEG and LoC's ALTO next to the JP2. The ALTO stays on the server for scoring and is never sent to hosts. Each run stages at most `--max-pages` (default 100) and resumes where the last stopped. At about four requests per OCR page, that is about 20 s per page: 3000 OCR pages take about 17 hours of fetching, and 20000 crash pages (one or two requests each, plus batch manifests and METS once per issue) about 1 to 2 days.

## Redundancy and validation

`create_work` makes each unit with `min_quorum` 2 and `target_nresults` 2, so two hosts read every page, and allows up to 6 results in all when the first two disagree or fail. Stock `script_validator` runs two scripts from the bridge image:

- `usnm_assess_check` (`usnm-boinc assess check`) on each result: shape, sizes, UTF-8, text checksums, and an outcome that matches the exit status.
- `usnm_assess_compare` (`usnm-boinc assess compare`) on each pair: same job and engine; the same outcome per format (for a crash unit, crashed or not: a run that read past the 20 s deadline counts the same as one that finished); and where both read a format, the same text, or a word-bag Dice of at least 0.98 (`MIN_TEXT_AGREEMENT`). One thread on one build is deterministic, but the LSTM can take different SIMD paths on different CPUs.

`batch_collect_assimilator --app usnm_assess` copies the canonical result into the spool.

The validator needs `usnm-boinc` in the server container. The server image does not carry it, so a server that runs the assessment uses the bridge image: deploy with `bootstrap.sh --assessment` (Bicep parameter `assessment=true`). `images/server/lib.sh` then adds the two daemons and links the scripts into the project's `bin/`.

## Scoring

Campaigns are kept apart by `ASSESS_CAMPAIGN`: the bridge job stages, submits and collects under `<share>/assess/<campaign>/`, so a campaign per engine build never shares units or output with another.

`usnm-boinc assess collect` reads the canonical results and writes, under `--out`, the files the #435 report's scripts read: `sample.jsonl`, `results.jsonl`, `crash.jsonl`, and `<n>/loc.xml` with `<n>/out-jp2/page.txt` and `<n>/out-jpg/page.txt`. `score.py` and `aggregate.py` then run unchanged against LoC's ALTO. A JP2 run that ended in `other` (neither a crash nor a read, such as an out-of-memory kill) is left out of `crash.jsonl`, because `aggregate.py` would count it as read. It also writes `nonresponse.jsonl`, the sampled pages with no result (LoC had no file, BOINC gave up, or still pending), and `units.jsonl`: every unit with its stratum, weights (as drawn, and `realized_weight` / `realized_crash_weight` over the units that came back), engine and per-format outcome, time and memory, for weighted corpus estimates and confidence intervals in the follow-up report.

## Running a campaign

Nothing here starts on its own. With the server deployed with `assessment=true`, the `usnm_assess` app version installed and signed, and the sample files on the share:

```sh
# Anywhere with the manifest.
usnm-boinc assess sample --manifest manifest.parquet --seed 456 \
  --out-ocr sample-ocr.jsonl --out-crash sample-crash.jsonl
# Copy both to <share>/assess/<campaign>/ on the project share.

# Bridge job runs, repeated until stage reports nothing left (each run is bounded).
az containerapp job start ... --env-vars BRIDGE_STEP=assess-stage ASSESS_MODE=crash ASSESS_CAMPAIGN=tess550 \
  ASSESS_ENGINE_IMAGE=sha256:... ASSESS_ENGINE_BASE=sha256:...
az containerapp job start ... --env-vars BRIDGE_STEP=assess-submit ASSESS_MODE=crash ASSESS_CAMPAIGN=tess550
# The same for ASSESS_MODE=ocr.

# When the units have been assimilated:
az containerapp job start ... --env-vars BRIDGE_STEP=assess-collect ASSESS_CAMPAIGN=tess550 ASSESS_SPOOL=/project/usnmocr/results/<batch>
# Tables: <share>/assess/tess550/out/tables.md
```

If `create_work` fails, the unit keeps a `submitting` marker and later runs skip it. Look the workunit up on the server by its name (`usnm_assess_<job id>`); if it is there, rename the marker to `submitted`, otherwise delete it.

## Not verified

- `script_validator`'s macros (`files`, `files2`) and that it finds the scripts in the project's `bin/`, at the pinned BOINC commit.
- Outbound HTTPS from the bridge job to tile.loc.gov.
- The spool's batch directory name for units made without `--batch`.
- That the crash reproduces in the BOINC worker image on volunteer CPUs (#455). The campaign records it either way.
