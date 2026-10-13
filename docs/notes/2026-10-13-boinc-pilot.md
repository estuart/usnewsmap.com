# Stock BOINC batch pilot

[Design issue #438](https://github.com/tgoodyear/usnewsmap.com/issues/438) adds a
file-based batch transport alongside the existing on-prem queue workers. See the
[setup and recovery guide](../boinc.md).

The first pilot shares the Tesseract 5.5.0 Containerfile, model lock, and routing.
The central bridge checks the published reference snapshot and reserves native
work under the OCR lock. It submits one cached scan per task, imports stock
BOINC's result spool, and publishes native ALTO/overlay-Parquet pairs for
`ocr-sync`. It does not change which text the release chooses.

Verification during implementation:

- Built the pinned engine image, including SHA-256 verification of its models,
  then built the BOINC worker Containerfile.
- Ran the worker with networking disabled, a read-only root, dropped capabilities,
  and its configured CPU/memory/PID limits. The synthetic rendered page returned
  `HELLO WORLD` using Tesseract 5.5.0 and the locked `eng` profile.
- Collected that output through the central CLI twice; the journal remained
  `published` with one native output pair.
- Integration tests exercise native `ocr-sync` pickup, reservation replay,
  ambiguous submission, terminal spool errors, malformed/mismatched output,
  publishing crash recovery, and duplicate collection after pickup.
- Review follow-up adds filtered manifest decoding, native title-language fallback
  for `und`/missing detection, and one cohort reservation under a renewable lease.
  Regression tests count snapshot/ledger reads, exercise a lost reservation
  response and partial journal updates, and refuse conflicting cohorts or a
  changed published snapshot without writing new requests.
- A second review follow-up validates version IDs before constructing object
  paths and loads title languages only from the published snapshot. Regression
  tests reject unsafe IDs before reference-store reads and refuse catalog-only
  titles in both export and reservation.
- An OCI permission check used a synthetic slot with mode 0771 owned by a different
  container UID and input files mode 0600. Dropping all capabilities failed with
  `Permission denied`; retaining only `DAC_OVERRIDE` returned `HELLO WORLD` and
  mode-0644 outputs/progress. This used the previously built worker image (worker
  execution code unchanged) and rootless Podman, not a BOINC client. The job
  configuration now retains that capability; live wrapper verification remains
  required.

The synthetic page establishes execution and format compatibility, not historical
OCR accuracy or general throughput. No live BOINC server/client task, OS idle
behavior, or Azure publication was exercised here. Those are distinct acceptance
steps in the guide. Cohorts must be excluded from overlapping seeder plans; this
pilot does not extend the seeder's long-lived ownership policy.
