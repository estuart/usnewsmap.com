# BOINC batch pilot

An opt-in worker pool for trusted friends, alongside the on-prem OCR nodes.
Design issue: [#438](https://github.com/tgoodyear/usnewsmap.com/issues/438).
The pilot uses stock BOINC, one cached page per task, and the repository's
Tesseract 5.5.0/model lock and language routing. It publishes native ALTO and
Parquet to `ocr-out`; `ocr-sync` and the release keep their existing decisions.

## Boundaries

- Linux x86-64 first, with compatible recent stock BOINC and Docker. The stock
  Docker wrapper is the only app type in this pilot. Volunteers need permission
  to run Docker containers; hosts without that capability (including restricted
  or unprivileged containers) cannot participate. A native app is future work.
- Explicit cohorts of 1-100 pages from one batch, selected from the prefetch
  manifest. No automatic consumption of Azure queues or LoC fetching.
- Central bridge: native work reservations, BOINC staging/submission, validation
  and publishing. Only it can access Azure through the existing Entra credential
  path. Volunteers get images and processing metadata, never credentials.
- Separate job/result file transport, version 1, from the planned ADR-0016
  long-lived engine protocol. Engine provenance remains `contract=0` with
  `route=boinc@r1`; models and routing are shared with the current worker.
- A task's container runs Tesseract directly. There is no nested Podman/Docker
  daemon. BOINC's Docker wrapper controls the outer container.

```
explicit page selection + cached scans
  -> native requested-work reservation + local dispatch journal
  -> stock BOINC -> pinned worker container
  -> stock batch_collect_assimilator spool
  -> usnm-boinc collect -> native ALTO/Parquet in ocr-out
  -> existing ocr-sync -> existing release/index
```

## Build the worker image

First build the existing engine image. Keep its pinned base, package snapshot,
and `models.lock`, including `frak2021`; the BOINC worker verifies the selected
models' bytes at execution. Then build the BOINC image using that engine image's
immutable digest (these examples run from the repository root):

```sh
docker build -f ocr/engines/tesseract/Containerfile \
  -t usnm-tesseract:pilot ocr/engines/tesseract
docker image inspect usnm-tesseract:pilot --format '{{.Id}}'

docker build -f ocr/boinc/Containerfile \
  --build-arg TESSERACT_IMAGE=sha256:ENGINE_IMAGE_ID \
  -t usnm-boinc:pilot .
```

Replace the placeholder with the full engine image ID. For remote volunteers,
publish the BOINC image to a registry they can pull, and use its registry digest:
`registry.example.org/usnm/boinc@sha256:FULL_DIGEST`. Record the full worker-image
pin, not the engine base's pin. Rebuild/sign a new BOINC app version when changing
code, models, image, or runtime configuration. The Rust build uses the repository's
locked dependencies; capture the build revision and compiler in your release.

Before changing the installed worker image, stop creating new cohorts. Finish
exporting/submitting already allocated cohorts with their original image, and
drain all existing workunits through collection. Resolve uncertain and failed tasks before
rolling the app version; do not send their existing requests to a different image.
The worker checks the exact Tesseract version, profile and model bytes, and the
collector checks the result against the job's immutable engine metadata. Installing
a new app version does not rewrite existing requests or bind `create_work` to an
image automatically. Resume with fresh cohorts and the new image pin after the
old cohort is drained. Retain old signed files and journals for recovery.

## Local synthetic demonstration

Requires Rust and the worker image above. `pilot_fixture` creates a rendered
`HELLO WORLD` page plus native manifest, sidecar, curated/reference stores, and
file-backed state. It makes no network requests and contains no production data.
Use a fresh directory:

```sh
cargo run --locked -p usnm-boinc --example pilot_fixture -- --dir /tmp/usnm-boinc-demo
cargo build --locked -p usnm-boinc

target/debug/usnm-boinc export \
  --manifest /tmp/usnm-boinc-demo/manifest.parquet \
  --pages /tmp/usnm-boinc-demo/pages.txt --batch test_pilot \
  --scans /tmp/usnm-boinc-demo/scans --root /tmp/usnm-boinc-demo/dispatch \
  --plan pilot-demo --image sha256:WORKER_IMAGE_ID \
  --curated-url /tmp/usnm-boinc-demo/curated \
  --reference-url /tmp/usnm-boinc-demo/reference \
  --state-file /tmp/usnm-boinc-demo/state.json
```

The exporter prints a job ID. Run its directory through the worker image:

```sh
docker run --rm --network=none --read-only --cap-drop=ALL \
  --security-opt=no-new-privileges --cpus=1 --memory=1536m \
  --memory-swap=1536m --pids-limit=64 --tmpfs=/tmp:rw,size=512m \
  -v /tmp/usnm-boinc-demo/dispatch/jobs/JOB_ID:/work usnm-boinc:pilot
```

On SELinux hosts, label only this scratch directory appropriately (e.g. a `:Z`
volume). The stock spool names are `<workunit>__file_<logical_name>`; for this
local test copy the three output files into a scratch spool with that naming,
where the workunit is `usnm_JOB_ID`, then run:

```sh
target/debug/usnm-boinc collect --root /tmp/usnm-boinc-demo/dispatch \
  --spool /tmp/usnm-boinc-demo/spool --job JOB_ID \
  --curated-url /tmp/usnm-boinc-demo/curated --out-url /tmp/usnm-boinc-demo/out
```

Run it twice: publication remains one ALTO/Parquet pair. The Rust integration
suite also runs these outputs through the real `ocr-sync` in directory mode:

```sh
cargo test --locked -p usnm-boinc
python3 -m unittest discover -s scripts/boinc -p 'test_*.py'
```

## Connect a stock BOINC project

Create your own Linux BOINC project server using the upstream setup documentation.
Stock means unmodified upstream BOINC software: BOINC Central hosting or public
project registration is not required. Friends attach manually to your server's
project URL. Registering `usnm_ocr` below means registering it on your own server.
This repository does not provision its web server/database or ship BOINC binaries.
Use BOINC 9.0+ clients and a recent Linux x86-64 `docker_wrapper` supporting
`workdir` and `create_args`. Install/merge upstream's Docker plan class and test
that the requested runtime limits, pause/resume, and output permissions work.

Register the application `usnm_ocr` in the BOINC admin interface. Install an app
candidate using a trusted stock wrapper and the pullable worker image digest:

```sh
python3 scripts/boinc/install.py --project /srv/boinc/newspapers \
  --wrapper /path/to/docker_wrapper \
  --image registry.example.org/usnm/boinc@sha256:FULL_DIGEST --version 1.0
```

The installer refuses an existing version directory. It does not alter running
server configuration. Sign/publish its files using BOINC's normal code-signing
process and `bin/update_versions`. Merge `ocr/boinc/daemons.xml` into the existing
`config.xml` daemons section and restart the project daemons.

`sample_trivial_validator` accepts results for this trusted-friends pilot; it
is not an accuracy/security check. `batch_collect_assimilator` moves canonical
outputs to `results/0/<workunit>__file_<logical_name>` and records terminal errors
there. Our central collector validates every result before publishing. Keep the
spool until central publication succeeds. A partial/missing spool is refused;
inspect BOINC logs and recover its files rather than accepting a partial result.

Export a real allocated cohort using its native prefetch manifest, canonical
page-key list, cached scan root, and curated/reference stores. For Azure state use `--cosmos`
in place of `--state-file`, with the same Entra identity/store settings as ingest.
The image flag is the full `sha256:...` digest corresponding to the installed app.
Then submit each exported job:

```sh
target/debug/usnm-boinc submit --root /srv/usnm/boinc-state \
  --project /srv/boinc/newspapers --job JOB_ID
```

Attach one friend's stock BOINC client to the project. Configure idle-only
processing and conservative preferences, and update the project. After the task
uploads and the assimilator spools it:

```sh
target/debug/usnm-boinc collect --root /srv/usnm/boinc-state \
  --spool /srv/boinc/newspapers/results/0 --job JOB_ID \
  --curated-url CURATED_STORE --out-url OCR_OUT_STORE
```

A `published` bridge entry means its native output pair is in `ocr-out`, not that
it has replaced searchable text. Continue with the existing `ocr-sync` and release.

## Allocation and recovery

Export filters the manifest while decoding, retaining only selected batch/page
rows (the native decoder still validates all rows). Undetected page languages
use the title's first catalog language from the published reference snapshot,
with the same catalog fallback and MARC-to-ISO mapping as native enqueue.

Export stages all selected jobs locally before reserving the cohort under one
renewable `ocr-sync` lock. It reads the published metadata and native ledger once,
validates the whole cohort, and writes one create-only queued part for fresh
reservations. If any row conflicts, no new reservations are written. It verifies
that the published version has not changed since staging, checks batch/version
and title against that snapshot, and leaves Japanese titles to their
existing pipeline. It refuses pages with prior native work, except an
exact replay of its own reservation. "Fresh cohort" means different page keys
without prior native work. Both outstanding and completed native work make a
page ineligible; changing the plan, image pin, timeout or job ID does not bypass
this check. The ledger remains
outstanding across BOINC processing; no Azure queue lease is held or renewed. Allocate a fresh small cohort,
and exclude it from overlapping seeder plans: the existing seeder may reconsider
old outstanding work under its own policy. This pilot does not add a global
long-lived BOINC ownership field to that policy.

The local journal has `prepared`, `reserved`, `submitting`, `submitted`,
`published`, and `failed` states. Keep it on persistent local storage and back it
up. An advisory lock serializes bridge processes sharing that directory.

- Repeating export completes a reservation interrupted by a crash, without writing
  another request. A lost cohort-write response or partially updated local
  journals replay the exact existing rows and reserve only still-fresh pages.
  The original request time is retained at native microsecond precision.
- Before `create_work`, submission records `submitting`. After an error or crash
  it never automatically submits again. Inspect BOINC for `usnm_JOB_ID`. Only
  after verifying existence/absence run `reconcile --root DIR --job ID --found
present` (mark submitted) or `--found absent` (permit submission again).
- The importer snapshots the first accepted bytes before external publication.
  Fixed object names and byte comparisons recover a lost upload response or a
  crash between ALTO and Parquet. Conflicting preexisting bytes stop publication.
  Native import outcomes recover a lost local commit after `ocr-sync` consumed
  the objects. A completed import is never republished from another callback.
- A terminal BOINC error sets `failed`. Preserve the native reservation and
  diagnose it with BOINC's server tools. This first pilot has no automatic
  reallocation. After recovering the same workunit on the server, preserve then
  remove its stale spool error marker and use `reconcile --root DIR --job ID
--found present` to permit collection of its recovered output. A failed job
  cannot be marked absent to create a duplicate workunit. Alternatively arrange
  a new native allocation through the existing pipeline. Do not reset a
  submitted/failed task blindly. There is no supported "re-export this failed
  page with a new BOINC job ID" command. To move the page back to the native pool,
  first ensure BOINC can no longer return or collect it, retain its journal and
  spool for audit, then use the existing
  [native enqueue/requeue procedure](operations.md#ocr-pickup-ocr-sync-296) with an
  operator-approved `ocr-enqueue --include-outstanding` retry for that page.
  `ocr-requeue` targets native dead letters, not BOINC spool error markers. Never
  delete ledger history to make the page appear fresh.
- Keep `keep/` scans. Do not delete `buffer/` scans from a volunteer claim or task
  completion. The exporter snapshots inputs centrally and never deletes originals.
- No in-page checkpointing: restart can repeat a page. BOINC resource estimates
  are initial bounds; calibrate them on real scans. One-hour maximum configurable
  OCR deadline, default 600 seconds, one Tesseract thread, PSM 3/DPI 300. The
  fixed `rsc_fpops_est=1e12` is provisional, so client ETA and credit are rough;
  use `elapsed_ms` from returned manifests across real JPEG/JP2 scans to calibrate
  it. `target_nresults=1` requests one result. `max_success_results=3` is a failure
  ceiling, not a request for three copies; stock BOINC cancels unsent copies once
  it finds a canonical result. These retry ceilings remain unchanged pending
  live recovery measurements.

Result identity/provenance fields and checksums establish transport consistency,
not proof that a volunteer actually ran the declared image. Trusted friends are
a prerequisite; public enrollment requires stronger validation. Raising quorum
alone with `sample_trivial_validator` does not compare OCR output. A future audit
or comparative validator could rerun sampled pages and compare text, after
measuring determinism across supported hosts, or check a trusted reference set.
No hand-transcribed historical accuracy measurement is implied by the synthetic smoke test.

## Live acceptance exercise

Complete one real stock-client/server task, then test keyboard activity pause,
sleep/reboot, a missing result file, a wrong checksum, an importer restart and
repeat collection. Confirm the wrapper applies `create_args` and all three outputs
are readable by the host client. Record server/client/wrapper/image versions and
which platforms were tested. Local native-pipeline tests do not establish BOINC
scheduler or OS-specific idle behavior.

Upstream references:

- [Server setup](https://github.com/BOINC/boinc/wiki/ServerIntro)
- [Docker app cookbook](https://github.com/BOINC/boinc/wiki/Docker-app-cookbook)
- [Code signing](https://github.com/BOINC/boinc/wiki/Code-signing)
- [Batch collector](https://github.com/BOINC/boinc/blob/master/sched/batch_collect_assimilator.cpp)
- [Stock validator result and retry handling](https://github.com/BOINC/boinc/blob/master/sched/validator.cpp)
