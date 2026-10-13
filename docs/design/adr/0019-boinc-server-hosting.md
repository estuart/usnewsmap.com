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

   **Exemptions.** The guardrail definitions (`infra/guardrails.bicep`) deny a storage account with shared key access and a workspace with local auth, and both stay unchanged. The BOINC stack deploys into its own resource group, which must already exist and hold only BOINC resources. `infra/main.bicep` takes `boincResourceGroup` (setting `USNM_BOINC_RESOURCE_GROUP`, empty by default, so existing deployments do not change). When set, `infra/main.bicep` assigns the same guardrail definitions to that group, and `infra/modules/boinc-exemptions.bicep` writes two Waiver exemptions on it, one for `usnm-deny-storage-shared-key` and one for `usnm-deny-monitor-local-auth`, and nothing else is waived. Each waiver is group-wide: it covers every resource in the group for that one policy, which is why the group must hold only BOINC resources. The scope is the group, not the two resources, because a resource-scoped exemption cannot exist before its target and the policy denies the target without it; so provision first, then deploy the BOINC stack. The keys these two features require live only in the platform configuration (the Container Apps environment's storage and log definitions), never in app code, the repo or a script. The Key Vault and the Files account need no public-access exception: both are private-endpoint only.

   The upload-signing key pair is kept apart from the code-signing key, which stays offline. `scripts/ci/no-shared-keys.sh` doesn't scan `contrib-compute/`; these are the places it would flag.

6. **Where #441 and this hosting may not fit are recorded, not worked around**: the Docker wrapper on the client side needs Docker only on volunteers' machines, not on the server (`docs/verified-facts.md`); the bridge's single `--root`, the permissions of the Docker job under `--cap-drop=ALL`, and Azure Files semantics are open (`docs/design.md`, section 7).

## Alternatives

- **A virtual machine.** Rejected: ADR-0005 keeps IaaS out of the project's subscription.
- **Running `submit` inside the web app container.** Rejected: it would give the public-facing replica an exec path and a second writer to the project tree.
- **Running `usnm-boinc` end to end on-prem.** Not possible as written: there is no VPN from on-prem to the VNet (ADR-0015), and the database is private.
- **Scale-to-zero for the server.** Not viable: the daemons and the shared memory segment run continuously. A pause switch (`minReplicas = 0`) saves the replica's cost, while the database and storage continue.

## Consequences

- The project gains a public scheduler, an upload handler and a web UI, and the work to keep them patched. The design's section 5 covers the controls.
- #441's pilot trusts its friends: the default validator checks nothing. Anything open to anonymous volunteers needs more validation, which is out of scope here.
- Nothing is deployed. The design lists what is unverified: the MySQL flexible server's fit with BOINC's tables, Azure Files semantics under Container Apps, and the Docker-wrapper plan class on a current server build.
