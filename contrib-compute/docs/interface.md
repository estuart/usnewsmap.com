# Interface to PR #441

What the Azure-hosted server and the files in this directory assume about #441's client and bridge, and what #441 can assume about the server. Design reasons are in [design.md](design.md). Nothing is deployed; the project URL is a placeholder that the maintainer fills in when the server exists.

Where this page and #441 differ, #441 wins; it is the client base.

## Names (all from #441)

| Item             | Value                                                                                                                                                |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| App name         | `usnm_ocr` (`boinc/project.xml` registers it with `target_nresults` 1)                                                                               |
| App type         | Docker wrapper; the worker image is built from `ocr/boinc/Containerfile` and runs `usnm-boinc work`                                                  |
| App version      | installed by `scripts/boinc/install.py --project --wrapper --image --version` into `apps/usnm_ocr/<version>/x86_64-pc-linux-gnu__docker`             |
| Plan class       | `docker`, from `boinc/plan_class_spec.xml` here (minimum client 9.0)                                                                                 |
| Workunit shape   | one page per task; templates `usnm_ocr_in` and `usnm_ocr_out` in `ocr/boinc/`; inputs `page` and `request.json`                                      |
| Daemons          | `sample_trivial_validator` and `batch_collect_assimilator`, from `ocr/boinc/daemons.xml`, in place of `@APP_DAEMONS@` in `boinc/config.xml.template` |
| Spool            | `<project>/results/<batch>/<workunit>__file_<logical name>`, written by `batch_collect_assimilator`                                                  |
| Submission       | `usnm-boinc submit --root --project --job`, which runs `bin/dir_hier_path` and `bin/create_work`                                                     |
| Publication      | `usnm-boinc collect --root --spool --job --curated-url --out-url`                                                                                    |
| Task limits      | `delay_bound` 259200 s; `rsc_memory_bound` 1.5 GiB; `rsc_disk_bound` 1 GiB; container capped at 1536 MiB and one CPU by `job.toml`                   |
| File size limits | scan 128 MiB, ALTO 32 MiB, text 512 KiB, JSON 16 KiB (`usnm-ocr-contract/src/boinc.rs`)                                                              |
| Client needs     | BOINC 9.0 or later, Docker, outbound HTTPS to the project URL, and to the registry holding the worker image. No inbound ports                        |

## What this directory provides to #441

| Item                               | Where                                                                                     |
| ---------------------------------- | ----------------------------------------------------------------------------------------- |
| A stock server in Azure            | `infra/boinc/` (Container App, MySQL, Files share, Key Vault, Log Analytics, static site) |
| The `docker` plan class            | `boinc/plan_class_spec.xml`                                                               |
| Project and daemon configuration   | `boinc/config.xml.template` and `boinc/project.xml`                                       |
| Where `submit` and `reconcile` run | the manual-trigger Container Apps Job in `infra/boinc/modules/bridge-job.bicep`           |
| The set-up plan and deployment     | `scripts/boinc/bootstrap.sh` (`--what-if` default; `--apply` deploys)                     |
| The server and bridge images       | `images/server`, `images/bridge` ([images.md](images.md))                                 |

## Paths on the share

| Path                                  | Content                                                              |
| ------------------------------------- | -------------------------------------------------------------------- |
| `/project/usnmocr/download`, `upload` | BOINC's fan-out trees: staged scans and volunteers' uploads          |
| `/project/usnmocr/results/<batch>`    | the assimilator's spool; keep until `collect` reports `published`    |
| `/project/usnmocr/apps/usnm_ocr`      | app versions written by `install.py`, then signed                    |
| `/project/handoff/jobs/<id>`          | an exported job, copied here for `submit`; its record is copied back |

## Account creation and privacy

`boinccmd --create_account URL email password name`, or attach with an existing key: `boinccmd --project_attach URL key`. Whether account creation is open or needs an invite code is the maintainer's choice (design.md 5.2). Do not share one account key across unrelated deployments.
