# BOINC server hosting: design

Status: draft. Nothing here is deployed. Facts about BOINC and Azure are in
[verified-facts.md](verified-facts.md), each with a fetch date (2026-10-13);
UNVERIFIED marks a claim that was not confirmed.

## 1. What this covers

PR #441 (`docs/boinc.md`, `crates/usnm-boinc`, `ocr/boinc/`) is the client side and the
bridge. Its model, which this design does not change:

- a Docker-wrapper app, `usnm_ocr`, whose worker image runs Tesseract directly; BOINC's
  `docker_wrapper` controls the outer container, so volunteers need Docker;
- one page per task, in an explicit cohort of 1 to 100 already-cached pages;
- a central bridge, `usnm-boinc`, that reserves pages under the OCR lock and ledger,
  journals each submission before `create_work`, reads the stock `batch_collect_assimilator`
  spool and publishes native ALTO and Parquet to `ocr-out` and `ocr-sync`;
- the default validator (`sample_trivial_validator`), for trusted friends;
- a stock, self-hosted BOINC server that the repository does not provision.

This directory is that last item: Azure hosting for the stock server, a script that sets it
up, and the notes on how the bridge reaches it. It contains no BOINC code, no work
generator, no validator, no assimilator and no app. The interface to #441 is in
[interface.md](interface.md).

## 2. Hosting on Azure

### 2.1 One Container Apps app

One Container Apps app, one container, `minReplicas = maxReplicas = 1`. The container runs
what `boinc-server-docker`'s `apache` image runs: `supervisord` starting cron, Apache with
PHP, and `bin/start`, which starts the feeder, transitioner, file_deleter, db_purge and the two app daemons from #441's `ocr/boinc/daemons.xml` (`sample_trivial_validator` and `batch_collect_assimilator`). Reasons:

- The feeder and the scheduler share a System V shared-memory segment (at least 32 MB), so
  they must share a host. Two containers in one replica share the network namespace, but
  whether they share the IPC namespace is UNVERIFIED. One container removes the question.
- Upstream splits the stack into `mysql`, `makeproject` and `apache` images; the daemons are
  already inside `apache`. We copy that split and replace `mysql` with the managed server.
- A sidecar is useful for one job: a log or metrics shipper. It is not needed in the first
  version. If added later it can share a replica-scoped volume with the main container.
- A second replica is not allowed: two schedulers with two feeders would each have a job cache
  and would both run the transitioner and validators against the same rows. BOINC supports
  multi-host projects, but only with the daemons each assigned to one host
  (`wiki:MultiHost`). We do not need it at this size.

Sizing for the first deployment, a pilot with a few trusted clients: 1 vCPU and 2 GiB (the Consumption combination). Apache with
PHP-FPM or mod_php, the scheduler CGI and about eight daemons fit in 2 GiB. UNVERIFIED until
measured.

### 2.2 Scale to zero is not viable

The daemons are long-running processes that poll the database every few seconds. The feeder
holds the job cache in memory. Scale to zero would stop all of them. A volunteer's scheduler
RPC arriving at a cold replica waits for a start that includes shared-memory warm-up and the
feeder's first fill. BOINC clients back off and retry, so it would function, but results being
uploaded and validated would stall for the cold period, and the daemons would not run when no
HTTP request has arrived.

So: `minReplicas = 1`. What we can do cheaply is a pause switch. Setting `minReplicas = 0`
(and not running the bridge job) parks the whole project. The clients back off with growing
delay. That parks the project and costs only the database and storage.

Container Apps bills a replica at the idle rate (lower) when it is not busy and at the active
rate when it is. The daemons use little CPU when no work flows, so the bill falls between
the two. Section 6 gives the unit prices.

### 2.3 Storage inside the app

The BOINC project tree needs persistence: `upload/`, `download/` (the staged scans, one per task, and the app files), `results/` (the assimilator's spool), `keys/` (public code-signing key, upload key pair), logs. On
Container Apps the container disk is ephemeral and the persistent option is an Azure Files
mount; Blob storage cannot be mounted. So:

- an Azure Files share holds `upload/`, `download/`, `results/`, `apps/` and a `handoff/` folder (section 4); mounted at the project path;
- `keys/` is not on the share. The container start script writes the upload key pair from
  Key Vault into a tmpfs-backed path and links it into the tree;
- the code-signing private key is not anywhere in Azure (section 5.1).

Whether SMB semantics (setgid, umask 0002, file locking) hold under the mount is UNVERIFIED.
If they do not, the fallback is a one-user container (web and daemons both as `boincadm`),
which gives up the web-user/daemon-user split BOINC recommends but keeps one process user.
That is a deployment-time test (section 9, question 1).

### 2.4 Database: Azure Database for MySQL flexible server, burstable

`Standard_B1ms` to start (1 vCore, 2 GiB, 341 connections), MySQL 8.4, 20 GiB storage, private
access in a VNet shared with the Container Apps environment, which cannot be changed later, so
choose it now. Notes:

- Authentication. BOINC reads a fixed password from `config.xml` and cannot refresh a token.
  Entra tokens last 5 to 60 minutes. So the daemons use a normal MySQL user whose password is
  generated at deploy time and stored as a Key Vault secret. Container Apps reads it through
  the app's user-assigned managed identity as a Key Vault secret reference. The password
  appears in the running container's environment and in the rendered `config.xml`, so
  `config.xml` is rendered into tmpfs at start, never into an image layer or the repo. The Entra
  admin for human access is a separate bootstrap step after deployment (bootstrap.sh 1a), because
  the server's identity needs Directory Readers first, which a template cannot grant. This is the one place where the "no keys in config"
  rule is not fully met, and it is documented as a gap.
- MyISAM. `post`, `thread` and `profile` are MyISAM in `schema.sql`. We turn off forums and
  profiles. Whether the server rejects MyISAM is UNVERIFIED; the `schema.sql` we apply is
  tested on a throwaway server first.
- TLS. Whether the BOINC C++ and PHP clients connect with `require_secure_transport = ON` is
  UNVERIFIED. If they do not, set it `OFF`. That is acceptable only because the server has
  private access only.
- CPU credits. B1ms has 20% baseline CPU. The transitioner and feeder poll constantly. If the
  credit balance drains, MySQL throttles to 20%. We alarm on the CPU-credit metric and move to
  `Standard_B2s` (4 GiB RAM variants cost more; section 6) when it does.
- Privileges. A setup user with `CREATE`/`ALTER`/`INDEX`/`DROP` runs `make_project` once from a
  one-off job; the run-time user gets `SELECT`/`INSERT`/`UPDATE`/`DELETE`. UNVERIFIED that the
  run-time user needs nothing more.

## 3. Static frontend: what can be static

Scope: very little.

- The client requests `<master URL>get_project_config.php` and the account RPCs
  (`create_account.php`, `lookup_account.php`) at the master URL. All are PHP. So the master
  URL must be served by the dynamic app. A static master page can carry the scheduler URLs
  (HTML comment plus `<link rel="boinc_scheduler">`) but cannot answer
  `get_project_config.php`.
- Static Web Apps can link a Container Apps backend, but this needs the Standard plan and is
  for `/api` routes (UNVERIFIED for the root path). A redirect rule from the static host to the
  dynamic one could work because the client follows redirects (`CURLOPT_FOLLOWLOCATION`), but
  that is UNVERIFIED end to end.

Decision:

1. The master URL is `https://boinc.usnewsmap.com/` for production and `https://boinc-dev.usnewsmap.com/` for dev and experiments, both custom domains on the Container App in the usnewsmap.com zone.
   A changed master URL forces every volunteer to reattach, so it must not change. The domain is ours, so we can move the app behind it.
2. Static Web Apps, Free plan, hosts the human-facing pages: what the project is, how to
   join, the privacy notice, the terms of use. These are plain HTML built from files in the
   repo. Nothing the BOINC client needs lives there.
3. File transfer: both directions go through the dynamic app, which reads and writes the
   Files share. Downloads are the cached scans that the bridge stages with `create_work`;
   uploads go to `file_upload_handler`, which cannot be redirected to blob storage.
4. Scheduler RPC goes to the dynamic app at `https://boinc.usnewsmap.com/<project>_cgi/cgi`.

Custom-domain steps (nothing here is done yet; `bootstrap.sh` prints them as a plan):

1. DNS `CNAME boinc` to the Container App's FQDN (`boinc-dev` for dev).
2. DNS `TXT asuid.boinc` holding the environment's custom-domain verification id.
3. Bind the domain to the app with a Container Apps managed certificate. UNVERIFIED that a
   managed certificate works with this environment; the fallback is a certificate from Key Vault.
4. The master URL cannot change after volunteers attach; every client would have to reattach.
5. `boinc-dev` follows the repo's dev rule: it holds nothing unless a focused experiment is
   running, and is torn down fully afterwards.

This differs from a "static frontend" as first asked for.

## 4. Data flow and the bridge

```mermaid
flowchart LR
  subgraph home [Maintainer side]
    Cache[Scan cache and OCR ledger] --> Export["usnm-boinc export"]
    Collect["usnm-boinc collect"] --> Out[ocr-out and ocr-sync]
  end
  Export -->|"jobs/ID copied to the share"| Share[(Azure Files share)]
  subgraph azure [Azure]
    Share --> Job["Container Apps Job: usnm-boinc submit"]
    Job -->|"bin/create_work"| DB[(MySQL)]
    Sched[Scheduler and feeder] --> Vol[Volunteer client + Docker]
    Vol -->|"file_upload_handler"| Share
    Share --> Asm["batch_collect_assimilator"]
    Asm -->|"results/BATCH/WU__file_NAME"| Share
  end
  Share -->|"spool pulled with Entra"| Collect
```

### 4.1 Who runs what

| Step                                  | Where                        | Why there                                                                                |
| ------------------------------------- | ---------------------------- | ---------------------------------------------------------------------------------------- |
| `export` (reserve, journal, snapshot) | Maintainer side              | Needs the scan cache, the OCR lock and ledger, and the existing Entra credential path    |
| `submit`, `reconcile` (`create_work`) | Container Apps Job, in Azure | Needs the project tree, `config.xml` and the database; none is reachable from outside    |
| Scheduler, feeder, transitioner, etc. | Container App                | Stock daemons; one replica (section 2)                                                   |
| `batch_collect_assimilator`           | Container App                | Writes the spool under the project tree, on the share                                    |
| `collect`, `status`                   | Maintainer side              | Writes `ocr-out`; holds the ledger and journal; the Job gets no storage write credential |

### 4.2 Where `create_work` runs

`usnm-boinc submit` runs `bin/dir_hier_path` and `bin/create_work` from the project
directory (`crates/usnm-boinc/src/submit.rs`). Both read `config.xml` and write to the
database, which is on a private VNet with no route from on-prem (ADR-0015 has no VPN). The
choices were:

- **Inside the server container** (`az containerapp exec`). Works, but it gives an
  interactive path into the public-facing replica and puts a second writer in its process
  space. Rejected.
- **A separate Container Apps Job in the same environment.** Same image (server tools plus
  the `usnm-boinc` binary), same share mounted at the project path, same identity and
  Key Vault path to render `config.xml` into tmpfs, manual trigger only, no platform
  retries (`modules/bridge-job.bicep`). Chosen. It has no role on `ocr-out` or `curated`.
- **A VM or a Cloud Shell session.** Rejected: ADR-0005 keeps IaaS out, and the shell would
  need the database credential on a person's machine.

The job's image is the server image plus the bridge binary. Building it is not done here
(`bridge-job` runs `/usr/local/bin/bridge-job`, a wrapper that renders `config.xml` and
execs `usnm-boinc`; it is part of the server image work, section 9).

### 4.3 The hand-off and the spool

- **Jobs go in.** `export` writes `<root>/jobs/<id>/` (the page, `request.json`, the record).
  The operator copies that folder to `handoff/jobs/<id>/` on the share. The Job runs
  `usnm-boinc submit --root <share>/handoff --project <share project dir> --job <id>`. The
  submission record, with its stage, is then updated on the share, so the operator copies it
  back before `collect` runs. #441's bridge assumes one `--root` holding the journal for
  every step; with two hosts, that file has to be moved by hand until the bridge supports a
  remote submit (a gap, section 7).
- **Results come out.** `batch_collect_assimilator` writes
  `results/<batch>/<workunit>__file_<logical name>` under the project directory, which is
  the Files share. The operator pulls that folder to the maintainer's side with Entra
  authorisation and points `collect --spool` at the local copy. The share's copy stays until
  `collect` reports `published`, as #441 requires. Entra authorisation for Azure Files data
  access from outside Azure is UNVERIFIED here; the fallback is the account key, which is
  already an ADR-0009 exception for the mount.
- **A job can also run `collect`.** That would need the ledger and write access to `ocr-out`
  from an Azure identity, a new grant under ADR-0015. Not proposed.

## 5. Signing, security and privacy

### 5.1 Code signing

- The code-signing key pair is generated on an offline machine (BOINC says disconnected and
  physically secure). The private key is never in the repo, the image, Key Vault or any Azure
  resource. The public key goes into the project's `keys/` directory.
- Release flow: `scripts/boinc/install.py` (#441) writes the app version directory. A human
  takes its files to the offline machine, runs `sign_executable FILE private_key > FILE.sig`
  for each, and brings the `.sig` files back. They go beside the files and `update_versions`
  runs on the server with no private key present, which picks up the `.sig` files
  (verified-facts.md (g)). The Dockerfile and `job.toml` are app files and are signed too.
- Rotation: BOINC supports an old-key signature chain. A rotation runbook is still to do.
- The upload key pair is separate, lives in Key Vault, and is loaded into tmpfs at start.

### 5.2 Security

Attack surface: the public scheduler CGI, `file_upload_handler` and the PHP web UI. The MySQL
server has private access only. The Key Vault and the Files account have public network access
off and are reached through private endpoints in the project's VNet; nothing else in the project is public. The public name is
`boinc.usnewsmap.com` with a managed certificate (section 3, UNVERIFIED); the master URL is part of
every volunteer's attachment and cannot change.

- Keep the PHP surface small: disable forums, profiles, private messages and teams in
  `html/project/project.inc`. Account creation can use `INVITE_CODES` so the project starts
  effectively private (#441's pilot is a few friends).
- BOINC's per-host quotas are set tightly in `config.xml` (`max_wus_in_progress`,
  `daily_result_quota`, `max_wus_to_send`); the cohort size (at most 100 tasks per batch) is
  the effective ceiling. How the project behaves under a flood has not been tested (UNVERIFIED);
  the pause switch (2.2) is the backstop.
- Secrets: Key Vault (RBAC mode) and a user-assigned managed identity shared by the app and the
  bridge job. Exceptions to the repository's Entra-only rule (ADR-0009), all forced by the
  platform or by BOINC: the MySQL password (2.4); the key of the Azure Files account, because
  the Container Apps mount takes it (the template reads it from the account it creates, so no
  key is passed in or committed); and the Log Analytics shared key that the Container Apps
  environment is configured with. No connection strings in Bicep outputs.
- Registry and operator access: both Container App resources pull the image with the shared
  identity, which has AcrPull scoped to the one registry named by the image (the registry has no
  admin user and no anonymous pull). The operator named by `--operator-object-id` gets Key Vault
  Secrets Officer, to create the four secrets, and is made the MySQL Entra administrator in the separate step above. Because the
  vault and the Files account are private, the operator needs a network path into the VNet to
  create secrets or copy to the share; none is provisioned here (open question 11).
- Policy: the repository's guardrails deny a storage account with shared key access and a
  Log Analytics workspace with local auth. This stack needs both (the three ADR-0009 exceptions
  in decision 5 of ADR-0019). `infra/main.bicep` waives exactly those two policies for the
  dedicated BOINC resource group when `USNM_BOINC_RESOURCE_GROUP` is set. The group must hold only
  BOINC resources, and provisioning runs before this stack is deployed.
- Images: our own build from the BOINC `server_release/1.6/1.6.2` tag; the Docker Hub images
  are stale.

Privacy:

- BOINC stores volunteer email, name, IP address and the account key. `config.xml` sets
  `enable_privacy_by_default`, `enable_delete_account` is on, and the terms of use are shown.
- The files on the share are scans the cache already holds, volunteers' results and the spool.
  No search text, user data or credentials of ours.
- Web server and scheduler logs include IP addresses. Log Analytics retention 30 days.
- #441's provenance carries its own route and version fields; nothing here adds a volunteer's
  email or IP to what is published.
- #441 notes that its result fields establish transport consistency, not proof that a
  volunteer ran the declared image. Trusted friends are a prerequisite.

## 6. Cost

Unit prices come from the Azure retail prices API (`https://prices.azure.com/api/retail/prices`),
queried 2026-10-13, East US, pay-as-you-go. They are retail list prices, not an invoice. No
run-rate total is given: the volumes depend on how many pages the maintainer chooses to send.

| Item                        | Meter                                          | Unit price                             |
| --------------------------- | ---------------------------------------------- | -------------------------------------- |
| Container Apps vCPU, idle   | Standard vCPU Idle Usage                       | $0.000003 / s                          |
| Container Apps vCPU, active | Standard vCPU Active Usage                     | $0.000024 / s                          |
| Container Apps memory       | Standard Memory (idle or active)               | $0.000003 / GiB-s                      |
| Container Apps requests     | Standard Requests                              | $0.40 / 1M after 2M free               |
| MySQL B1ms compute          | `B1MS`                                         | $0.017 / h                             |
| MySQL B2s compute           | `B2S`                                          | $0.068 / h                             |
| MySQL storage               | Storage Data Stored                            | $0.115 / GB-month                      |
| Azure Files                 | Files v2 Hot LRS data                          | $0.0287 / GB-month, plus operations    |
| Static Web Apps, Free plan  | none in the API                                | not listed (list price $0, UNVERIFIED) |
| Key Vault operations        | Standard operations                            | $0.03 / 10K                            |
| Log Analytics ingest        | Analytics Logs                                 | $2.30 / GB (first tier $0)             |
| Data transfer out           | Routing Preference Internet, Data Transfer Out | first 100 GB $0, then $0.08 / GB       |

Container Apps: the first 180,000 vCPU-seconds, 360,000 GiB-seconds and 2 million requests per
subscription per month are free (Microsoft Learn billing page). That is not subtracted here,
because the subscription may host other things. Whether a workload-profiles environment on the
Consumption profile has its own fee is UNVERIFIED; `infra/modules/containerapps-env.bicep`
says there is none without a Dedicated profile. That the Container Apps replica's traffic is
billed on the data-transfer-out meter is also UNVERIFIED.

### 6.1 Volumes from #441's contracts

The bridge sends one page per task, one result per task (`target_nresults` 1, quorum 1;
a task can be retried up to five times, `max_total_results` in `submit.rs`).

| Quantity per task    | Typical                                       | Contract limit (`usnm-ocr-contract/src/boinc.rs`) |
| -------------------- | --------------------------------------------- | ------------------------------------------------- |
| Download: scan       | about 4.5 MB (one JP2, measured on one page)  | 128 MiB (`MAX_SCAN`)                              |
| Download: request    | a few KB                                      | 16 KiB (`MAX_JSON`)                               |
| Upload: ALTO         | not measured (UNVERIFIED)                     | 32 MiB (`MAX_ALTO`)                               |
| Upload: text, result | not measured (UNVERIFIED)                     | 512 KiB (`MAX_TEXT`) and 16 KiB                   |
| Worker image pull    | once per host, from the registry, not from us |                                                   |

| Quantity per batch (at most 100 tasks) | Typical (4.5 MB scan) | At the contract limits |
| -------------------------------------- | --------------------- | ---------------------- |
| Staged on the share until deleted      | about 450 MB          | about 12.5 GiB         |
| Download from the server               | about 450 MB          | about 12.5 GiB         |
| Upload to the server                   | not measured          | about 3.1 GiB for ALTO |

Per-unit consequences, using the unit prices above:

- Download at 4.5 MB a task costs $0.08 / GB x 0.0045 GB per task above the free 100 GB a
  month, which is $0.00036 per task, if that traffic is billed on the meter in the table.
  Uploads to the server are inbound and have no transfer charge in the retail table (UNVERIFIED).
- Task rate: the batch sets it, not the server. A volunteer core needs the OCR time per page,
  which the project's own nodes measure at about 160 to 330 s (UNVERIFIED on volunteer
  hardware), so one core completes roughly 11 to 22 tasks an hour, and downloads about 50 to
  100 MB an hour. Scheduler contacts are about one per task.
- Files holds the staged scans for `delete_delay_hours` (168 in the template) after
  assimilation. The share quota is a parameter (`shareQuotaGb`, 200).
- The bridge job runs for seconds to minutes per cohort at 0.5 vCPU and 1 GiB, billed at the
  active rate for that time only.

The fixed blocks (Container App at the idle or active rate, MySQL, Files, Key Vault, Log
Analytics) are billed whether or not a batch is running. Pausing (2.2) saves the replica.

Knobs: the cohort size and how often batches go out; the MySQL tier (B1ms to B2s if CPU
credits drain); the retention of staged scans.

## 7. Where #441 and this hosting may not fit

1. **Docker on the server: not needed.** The plan class `docker` only filters hosts by the
   Docker version they report (verified-facts.md (d)). The Container App and the Job run no
   containers. Volunteers need Docker; the host routes that cannot run it (ACI, an
   unprivileged LXC, a plain container without the host socket) cannot take this app.
2. **The bridge assumes one host.** `submit.rs` and `lib.rs` take a single `--root` that holds
   the jobs, the journal and the scan cache, and run `create_work` from a local project
   directory. With the server in Azure and the cache on-prem, the journal has to be copied
   across by hand (4.3). A `submit` that works from a remote hand-off, or journals on the
   server side, would remove that.
3. **`--cap-drop=ALL` and directory permissions.** `ocr/boinc/job.toml` drops all
   capabilities, and `ocr/boinc/Containerfile` sets the user. BOINC creates slot and project
   directories with mode 0771 (`boinc_mkdir`), so the container user may not be able to write
   its outputs unless the user ids match. #441's acceptance exercise covers this; we cannot
   check it from here.
4. **`workdir` and `create_args` are source-only.** The wiki does not document them; the
   wrapper source does (verified-facts.md (d)). A released wrapper binary is needed that has
   them.
5. **`install.py` needs a `docker` plan class** that the repository did not provide.
   `boinc/plan_class_spec.xml` here supplies it.
6. **No registry is named for the worker image.** Volunteers' Docker must pull it by digest;
   where it lives is open (section 9).
7. **`daemons.xml` has no standard daemons.** It lists only the validator and the assimilator;
   the template here adds the feeder, transitioner, file_deleter and db_purge.
8. **Azure Files semantics.** BOINC wants setgid directories, umask 0002 and file locking;
   whether Azure Files over SMB under Container Apps gives them is UNVERIFIED (2.3).
9. **Resource bounds.** `submit.rs` sets `rsc_memory_bound` to 1.5 GiB and `job.toml` caps the
   container at 1536 MiB. Whether Tesseract plus JP2 decoding of a 128 MiB scan fits is
   UNVERIFIED; #441 says to calibrate on real scans.

## 8. Later, for anonymous volunteers

#441 trusts its friends. If the project ever opens to anonymous volunteers, the server side
would add replication with a validator that compares the text of replicas (BOINC's
`compare_results` hook or `script_validator`) and adaptive replication, so trusted hosts are
sampled. That belongs with the bridge's own validation, not with this hosting, and is not
proposed here.

A job type where a volunteer's machine fetches the scan from the Library of Congress instead
of receiving it from us is deferred. If it were ever built, each client would run at a low
request rate, it would not be a way around the source's limits, and a better place to get
the images would be preferred if one exists. It is not planned until the maintainer decides.

## 9. Open questions

1. Does Azure Files under Container Apps keep setgid, umask and locking semantics that BOINC
   needs? Test with a throwaway app before choosing it.
2. Do the BOINC C++ and PHP clients connect to the flexible server with TLS required? Does it
   accept the MyISAM tables (forums off)? Does BOINC's code work against MySQL 8.4?
3. Does Entra authorisation work for pulling and pushing Azure Files data from the
   maintainer's side, or does the hand-off use the account key?
4. Does a Container Apps Job accept per-run argument overrides at start (the job id to
   submit), or does each run need a new execution template?
5. Does the BOINC client follow a redirect for `get_project_config.php`?
6. Which registry holds the worker image, and who may pull from it?
7. Code-signing key custody: which offline machine, and who holds the backup?
8. The project's name, domain and terms of use. The master URL, once volunteers attach, cannot
   change.
9. Who builds the server image and `bridge-job` wrapper, and where does the build live?
10. Should `submit` get a remote mode in #441, or is the manual hand-off enough for the pilot?
11. What network path does the operator use into the VNet to create the Key Vault secrets and to
    reach the Files share (for the hand-off in section 4)? Candidates: `az containerapp exec`
    into the bridge job's environment, a point-to-site VPN, or a jump host. None is chosen.
12. ~~How are the three ADR-0009 exceptions expressed in the guardrail policies?~~ Resolved
    (ADR-0019, decision 5): group-scoped Waiver exemptions in `infra/modules/boinc-exemptions.bicep`;
    the definitions are unchanged.
13. Does the maintainer accept `require_secure_transport = OFF` inside the private network if
    the BOINC clients cannot use TLS?

## 10. First deployment step

Not without the maintainer's go-ahead: create a throwaway resource group with the MySQL
server only, run BOINC's `schema.sql` against it from a developer machine, and record whether
MyISAM is accepted and whether the BOINC daemons connect with TLS required. It costs a few
cents for an hour and answers question 2 before anything else is built on the answer.
