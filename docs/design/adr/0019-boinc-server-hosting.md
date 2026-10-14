# ADR-0019: Hosting for the stock BOINC server

- **Status:** Proposed
- **Date:** 2026-10
- **Extends:** ADR-0015 (on-prem OCR workers) and ADR-0016 (pluggable OCR engines)
- **Amends:** ADR-0009 for the BOINC project server only, in three places (decision 5)
- **Code and docs:** [`contrib-compute/`](../../../contrib-compute/README.md)

## Context

PR #441 adds a BOINC batch pilot to the OCR pipeline. Its worker is a Docker-wrapper app, `usnm_ocr`, that runs Tesseract on one cached page per task. Its bridge, `usnm-boinc`, reserves pages, submits them to a stock BOINC server with `create_work`, reads the stock `batch_collect_assimilator` spool, and publishes native ALTO and Parquet. The pilot leaves the server itself out: it says to create a stock Linux BOINC server and does not provision one.

This ADR covers that missing piece, the Azure hosting of a stock server, so the pilot runs on a reproducible server instead of a hand-built machine. It adds no BOINC code and does not replace anything in #441.

## Decision

1. **The server is stock BOINC, hosted as one Container App with a MySQL flexible server**, in Bicep (`contrib-compute/infra/boinc/`). It is one replica, never scaled to zero, because the feeder and the scheduler share memory and the daemons poll continuously. The daemons are the stock ones from #441's `ocr/boinc/daemons.xml`, plus the standard feeder, transitioner, file_deleter and db_purge.
2. **The project directory is an Azure Files share**, behind a private endpoint, mounted into the app and into a bridge job. The share holds `download/`, `upload/`, `results/` (the assimilator spool), `apps/` and hand-off folders.
3. **`usnm-boinc submit` and `reconcile` run in a manual-trigger Container Apps Job in the same environment**, from the server image plus the `usnm-boinc` binary. Stock `create_work` and `dir_hier_path` need the project tree, `config.xml` and the database, which exist only there. `export` and `collect` stay on the maintainer's side, where the scan cache, the ledger and the Entra credential for `ocr-out` are. The job holds no storage credential for `ocr-out` or `curated`.
4. **The master URL is a custom domain on the Container App.** Static Web Apps (Free) hosts only human-readable pages, because `get_project_config.php` and the account RPCs are PHP. The master URL is `https://boinc.usnewsmap.com/` (prod) or `https://boinc-dev.usnewsmap.com/` (dev, which holds no resources unless a focused experiment is running). Setup is a `boinc` CNAME to the app's FQDN, an `asuid` TXT record, and a Container Apps managed certificate (UNVERIFIED that one works with this environment). The master URL never changes once volunteers attach.
5. **Secrets.** Everything uses Entra identities except three things, each forced by the platform or by BOINC, and each an exception to ADR-0009:
   - the MySQL password, held in Key Vault and read by the app's managed identity, because BOINC reads a fixed password from `config.xml` and cannot use Entra tokens;
   - the key of the Azure Files account, because the Container Apps Azure Files mount takes an account key;
   - the Log Analytics shared key that the Container Apps environment configuration takes.

   **Exemptions.** The guardrail definitions (`infra/guardrails.bicep`) deny a storage account with shared key access and a workspace with local auth, and both stay unchanged. The BOINC stack deploys into its own resource group, which `infra/main.bicep` creates when the setting is true and which holds only BOINC resources. `infra/main.bicep` takes `deployBoincGroup` (setting `USNM_BOINC_GROUP`, false by default, so existing deployments do not change), and the group's name, `usnm-boinc-rg-<env>`, comes from `infra/naming.bicep`. When true, `infra/main.bicep` assigns the same guardrail definitions to that group, and `infra/modules/boinc-exemptions.bicep` writes two Waiver exemptions on it, one for `usnm-deny-storage-shared-key` and one for `usnm-deny-monitor-local-auth`, and nothing else is waived. Each waiver is group-wide: it covers every resource in the group for that one policy, which is why the group must hold only BOINC resources. The scope is the group, not the two resources, because a resource-scoped exemption cannot exist before its target and the policy denies the target without it; so provision first, then deploy the BOINC stack. The keys these two features require live only in the platform configuration (the Container Apps environment's storage and log definitions), never in app code, the repo or a script. The Key Vault and the Files account need no public-access exception: both are private-endpoint only.

   The upload-signing key pair is kept apart from the code-signing key, which stays offline. `scripts/ci/no-shared-keys.sh` doesn't scan `contrib-compute/`; these are the places it would flag.

6. **The images are built from `contrib-compute/images/`** (the server and the bridge job; [docs/images.md](../../../contrib-compute/docs/images.md)), from a pinned BOINC commit, because the tag the design first named lacks `batch_collect_assimilator`. The worker image's registry is in the addendum.
7. **Where #441 and this hosting may not fit are recorded, not worked around**: the Docker wrapper on the client side needs Docker only on volunteers' machines, not on the server (`docs/verified-facts.md`); the bridge's single `--root`, the permissions of the Docker job under `--cap-drop=ALL`, and Azure Files semantics are open (`docs/design.md`, section 7).

## Alternatives

- **A virtual machine.** Rejected: ADR-0005 keeps IaaS out of the project's subscription.
- **Running `submit` inside the web app container.** Rejected: it would give the public-facing replica an exec path and a second writer to the project tree.
- **Running `usnm-boinc` end to end on-prem.** Not possible as written: there is no VPN from on-prem to the VNet (ADR-0015), and the database is private.
- **Scale-to-zero for the server.** Not viable: the daemons and the shared memory segment run continuously. A pause switch (`minReplicas = 0`) saves the replica's cost, while the database and storage continue.

## Consequences

- The project gains a public scheduler, an upload handler and a web UI, and the work to keep them patched. The design's section 5 covers the controls.
- #441's pilot trusts its friends: the default validator checks nothing. Anything open to anonymous volunteers needs more validation, which is out of scope here.
- Nothing is deployed. The design lists what is unverified: the MySQL flexible server's fit with BOINC's tables, Azure Files semantics under Container Apps, and the Docker-wrapper plan class on a current server build.

## Addendum (2026-10-13): where the worker image is published

Implemented by `.github/workflows/boinc-worker.yml` (docs/boinc.md, "Publish the worker image").

**Problem.** Each volunteer's Docker pulls the worker image: the app version's `Dockerfile` is `FROM <image>` (`scripts/boinc/install.py` in PR #441). Volunteers have no credentials for us. The project's Azure Container Registry has no anonymous pull and no admin user (design.md, 5.2), and that is right for the server images, so it cannot serve volunteers.

**Decision.** Publish the worker image as a public GHCR package under this repository: `ghcr.io/tgoodyear/usnm-boinc-worker`, linked to the repository. It is built the way #441 describes (`docs/boinc.md`, "Build the worker image"): first the engine image from `ocr/engines/tesseract/Containerfile`, then `ocr/boinc/Containerfile` with `--build-arg TESSERACT_IMAGE=sha256:<engine image id>`. The app version records the registry digest of the pushed worker image (`ghcr.io/tgoodyear/usnm-boinc-worker@sha256:<64 hex>`), not the local image id and not the engine's pin.

- The image holds Tesseract, its pinned models, and the `usnm-boinc` binary. It holds no credential, no data and no scan, so a public package discloses nothing that the repository does not already publish.
- A GHCR package is private by default. Its visibility is set once in the package settings by the maintainer; a workflow cannot do it. Before the first task is sent, verify an anonymous pull from a logged-out Docker on a host that is not the maintainer's (#441 asks for the same check from the volunteer runtime).
- Publishing is a workflow started by hand (`workflow_dispatch`), using the repository token, with permission to write packages. A pull request that changes the image's inputs builds it without pushing. A push to main publishes nothing: each image needs its own signed app version. Rolling the image means a new app version and a drained cohort (#441, "Build the worker image").

**Alternatives.**

- _Azure Container Registry with anonymous pull._ A registry-wide setting that exposes every repository in it, and its availability depends on the registry's SKU (UNVERIFIED). Rejected: the server images share that registry.
- _A second registry in Azure just for the worker._ Adds a fixed monthly cost for one image, against a free public package.
- _Docker Hub._ Anonymous pulls are rate limited per client address (UNVERIFIED for current limits), which fails for volunteers behind shared addresses.
- _Ship the image from the project server as a file._ BOINC's Docker wrapper does not do that.

**Consequences.** The registry digest exists only after the first push, so `bootstrap.sh --worker-image` cannot be run for real until then. GHCR's terms and retention for public packages are the maintainer's to check (UNVERIFIED).
