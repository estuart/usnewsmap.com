# BOINC and Azure facts, checked against sources

The facts the hosting design for the stock BOINC server depends on. The client side, the bridge and the contracts are in PR #441 (`docs/boinc.md`) and are not repeated here.

Every fact here has a source and a fetch date. All sources were read on 2026-10-13 (US Eastern).
A fact marked UNVERIFIED was not confirmed from a source and must be tested before design
depends on it. A fact marked CORRECTED differs from what the request assumed.

Source snapshots:

- BOINC wiki, cloned from `https://github.com/BOINC/boinc.wiki.git`, commit `7e16379`
  (2026-10-10). Pages are cited as `wiki:<Page>`, meaning
  `https://github.com/BOINC/boinc/wiki/<Page>`.
- BOINC source, `https://github.com/BOINC/boinc`, master at commit `0f611c6` (2026-10-12).
  Files are cited as `src:<path>`, meaning `https://github.com/BOINC/boinc/blob/master/<path>`.
- boinc-server-docker, `https://github.com/BOINC/boinc-server-docker`, cited as `bsd:<path>`.
- Latest server release tag: `server_release/1.6/1.6.2`, 2026-06-12
  (`https://api.github.com/repos/BOINC/boinc/releases/latest`; also `wiki:Server-release-notes`).

Summary:

| Claim                                           | Result                                                                                 |
| ----------------------------------------------- | -------------------------------------------------------------------------------------- |
| (a) No central service, you run your own server | CONFIRMED                                                                              |
| (b) Server stack and daemons                    | CONFIRMED, with four CORRECTIONS below (shared memory, shared filesystem, MyISAM, TLS) |
| (c) Client auth is a per-project account key    | CONFIRMED, one detail added (weak key)                                                 |
| (d) Docker-wrapper apps and the plan class      | CONFIRMED; the server needs no Docker; `workdir` and `create_args` are source-only     |
| (e) Validation and assimilation                 | CONFIRMED for the stock daemons #441 uses                                              |
| (f) Credit, leaderboards, teams                 | CONFIRMED                                                                              |
| (g) File serving and signing                    | CONFIRMED for download; CORRECTED for upload (cannot be blob storage)                  |
| (h) Server container images                     | CONFIRMED, but the published images are stale                                          |
| (i) Official list, attaching to an unlisted URL | CONFIRMED optional; attach by URL CONFIRMED                                            |

## (a) There is no central service

- CONFIRMED. A project is a server you run. The cookbook builds a project with
  `make_project`, which creates the DB, the project directory, the Apache config and the
  daemon config. Source: `wiki:Create-a-BOINC-server-(cookbook)`, `wiki:The-make_project-script`,
  `wiki:ServerIntro`.
- The client learns of a project from its master URL. Source: `wiki:Master-URL`.
- Account managers (BOINC Manager's "Add project" list, Science United) are optional indexes,
  not hosts for the project. See (j).

## (b) Server stack and daemons

Components, all CONFIRMED from `wiki:ServerIntro`, `wiki:Daemons`, `wiki:ProjectDaemons`,
`wiki:Create-a-BOINC-server-(cookbook)`:

| Component      | What it is                                                                   |
| -------------- | ---------------------------------------------------------------------------- |
| Apache + PHP   | Web UI, account RPCs, `get_project_config.php`, forums, leaderboards         |
| Scheduler      | CGI binary `cgi-bin/cgi` (fcgi supported), called by clients on each RPC     |
| feeder         | Fills a shared-memory cache of sendable jobs for the scheduler               |
| transitioner   | Moves workunits and results between states, creates replicas, sets deadlines |
| validator      | One per app. Checks results, picks the canonical one, grants credit          |
| assimilator    | One per app. Handles the canonical result of each finished workunit          |
| file_deleter   | Deletes input and output files after assimilation                            |
| db_purge       | Archives and removes old workunit and result rows                            |
| work generator | Project code that creates workunits (`bin/create_work` or `create_work2`)    |
| periodic tasks | Stats, `update_stats`, team/user cleanup, via `bin/start --cron`             |

Version and platform facts:

- Database: MySQL or MariaDB. The cookbook lists MySQL 5.x/8 or MariaDB;
  `bsd:images/mysql` uses `mariadb:lts`. Source: `wiki:SoftwarePrereqsUnix`,
  `bsd:images/mysql/Dockerfile`. A specific supported-version matrix is not published on the
  wiki: UNVERIFIED whether MySQL 8.4 is exercised by BOINC's own CI.
- Charset: `utf8mb4` / `utf8mb4_unicode_ci` in `src:py/Boinc/database.py` (`create_database`).
- DB privileges: `create_database` issues `CREATE DATABASE` and then DDL and DML on it, so
  the DB user needs `CREATE`, `ALTER`, `INDEX`, `DROP` during setup and `SELECT`,
  `INSERT`, `UPDATE`, `DELETE` at run time. No triggers, stored procedures or events are
  created (`src:db/schema.sql`). Sessions use `READ UNCOMMITTED`
  (`src:db/db_base.cpp`, `src:html/inc/db_conn.inc`). Splitting setup and run-time users is
  our inference: UNVERIFIED.
- Password handling: the DB password is in plain text in `config.xml`
  (`<db_passwd>`). Source: `wiki:Project-options`. This is the reason the design keeps
  `config.xml` out of the repo and renders it at container start from Key Vault.

CORRECTIONS and gaps, in order of how much they affect the design:

1. CORRECTED, shared memory. The feeder and the scheduler share a System V shared-memory
   segment (the job cache, default at least 32 MB). They must run on the same host, and so in
   the same container group. A scheduler in one replica and a feeder in another does not work.
   Source: `wiki:MultiHost`, `wiki:Daemons`. This is what rules out scale-to-zero and
   multiple replicas for the scheduler plus daemons (design.md section 2).
2. CORRECTED, shared filesystem. Scheduler, daemons, upload handler, file_deleter and the
   validators all read and write one project tree: `upload/`, `download/`, `keys/`, `log_*`,
   with a fan-out directory hierarchy (`uldl_dir_fanout`). The web user and the daemon user
   share a group; directories are setgid and umask is 0002. Source: `wiki:DirHierarchy`,
   `wiki:Create-a-BOINC-server-(cookbook)`, `wiki:Server-security`. On Container Apps the
   container disk is ephemeral and Azure Files is the persistent mount, and blob storage
   cannot be mounted (see the Azure notes below).
3. CORRECTED, MyISAM. Most tables are InnoDB, but `post`, `thread` and `profile` are MyISAM
   because of full-text indexes. Source: `src:db/schema.sql` lines 33 to 35. Azure Database
   for MySQL "uses the InnoDB storage engine"
   (`https://learn.microsoft.com/en-us/azure/mysql/concepts-version-policy`, ms.date 2026-05-13).
   UNVERIFIED: whether `ENGINE=MyISAM` is rejected or silently converted on the flexible
   server. Plan: disable forums and profiles (they are not needed), and test `schema.sql`
   against a throwaway server before any real deployment.
4. UNVERIFIED, TLS to the DB. The C++ daemons connect with `mysql_real_connect`
   (`CLIENT_FOUND_ROWS`) and set only `MYSQL_OPT_RECONNECT`; PHP uses `new mysqli(...)`. No
   SSL option is configured anywhere (`src:db/db_base.cpp`, `src:html/inc/db_conn.inc`).
   Whether they negotiate TLS depends on the client library's default (Oracle libmysqlclient
   defaults to "preferred"; `bsd:images/apache/Dockerfile` installs Debian's
   `default-mysql-client` and PHP `mysqli`, and MariaDB Connector defaults differ). The
   flexible server's default is `require_secure_transport = ON` with TLS 1.2
   (`https://learn.microsoft.com/en-us/azure/mysql/flexible-server/how-to-connect-tls-ssl`).
   If the BOINC clients do not negotiate TLS, set the server parameter to `OFF` inside a
   private-access VNet. This must be tested.

Other notes for (b):

- Azure auth to MySQL with Entra ID: the access token is the password, the client needs the
  cleartext plugin, and tokens live 5 to 60 minutes. Source:
  `https://learn.microsoft.com/en-us/azure/mysql/security/security-how-to-entra`
  (page dated 2026-07-17). BOINC reads a fixed password from `config.xml` and has no
  refresh path, so Entra auth is not usable for BOINC daemons. The password goes in Key
  Vault. UNVERIFIED: that no wrapper (sidecar token refresher) is worth the complexity.
- The flexible server burstable tier: `Standard_B1ms` has 1 vCore, 2 GiB, 341 max
  connections, 640 IOPS; base CPU 20%. Storage 20 GiB minimum. Source:
  `https://learn.microsoft.com/en-us/azure/mysql/flexible-server/concepts-compute-storage`.
- MySQL major versions on Azure: 8.0 (standard support ends 2027-01-31) and 8.4 are GA;
  5.7 is retired; 9.5 is a preview innovation release. Source: version-policy page above. Use 8.4.
- Private access (VNet integration) cannot be switched to or from public access after
  creation. Source: `https://learn.microsoft.com/en-us/azure/mysql/flexible-server/concepts-networking`.

## (c) Client authentication is a per-project account key

- CONFIRMED. The client stores an `authenticator` (account key) per project in
  `account_<project>.xml`, sent on each scheduler request. Account creation goes through
  `create_account.php` / `lookup_account.php`. Source: `wiki:AccountManagement`,
  `wiki:Weak account key`.
- Detail: BOINC has a "weak account key" (the key without the password hash) that can be used
  for read-only and attach actions. Source: `wiki:Weak account key`.
- There is no per-project credential for the cloud. Volunteers never hold an Azure credential.

## (d) Docker-wrapper apps and the plan class

All read on 2026-10-13 (US Eastern): wiki commit `7e16379`, source commit `0f611c6`.

- `docker_wrapper` runs a container described by `job.toml` and built from a `Dockerfile`
  (or pulled from an image), in an app version directory. Source: `wiki:Docker-apps`,
  `wiki:Docker-app-cookbook`, `wiki:Docker-app-implementation`.
- The wiki says the client must be BOINC 9.0 or later (see below: 8.2.13 is what exists) and the host needs Docker (Windows needs WSL). The
  image must contain `ps`. The cookbook asks for a server of November 2024 or later and tells
  you to copy `plan_class_spec.xml.sample` into the project directory. Source:
  `wiki:Docker-app-cookbook`.
- `workdir` and `create_args` in `job.toml`: the wiki does NOT document them. The source does:
  `samples/docker_wrapper/docker_wrapper.cpp` reads `workdir` (default `/app`) and appends
  `create_args` to the `docker create` command line; `--cpus` is added only when the task has
  more than one CPU. UNVERIFIED on a running client: that a given released wrapper binary has
  this code (#441 asks for it too, and has an acceptance exercise for it).
- The server needs no Docker. A plan class named `docker` in `<project>/plan_class_spec.xml`
  makes the scheduler send the app version only to hosts that report Docker: the check reads
  the client-reported `host.docker_version` and the client version
  (`src:sched/plan_class_spec.cpp`, lines 584 to 631). This answers the question for the
  Azure hosting: the Container App never runs a container itself.
- Element names are CONFIRMED in `src:sched/plan_class_spec.xml.sample` (the `docker` class is
  `<plan_class><name>docker</name><docker/></plan_class>`) and in the parser
  (`src:sched/plan_class_spec.cpp`, lines 1258 to 1266: `min_core_client_version`,
  `docker`). `boinc/plan_class_spec.xml` here is that class plus
  `<min_core_client_version>80213</min_core_client_version>`, so older clients are not sent
  the app. The server's own Docker check applies the 8.2.6 minimum to macOS clients only
  (line 625). The wiki asks for 9.0, but no 9.x client had been released as of 2026-10-14
  (newest Linux packages: 8.2.13 stable, 8.2.15 alpha), so a 9.0 minimum blocked every host.
  8.2.13 on Debian 13 detects Docker (`Docker: version ...` in its event log).
- `scripts/boinc/install.py` (#441) installs the version under
  `apps/usnm_ocr/<version>/x86_64-pc-linux-gnu__docker`. The `__docker` suffix is the plan
  class name, so the plan class must be called `docker`.
- Files for Docker apps (`Dockerfile`, `job.toml`, `version.xml`) are app files, signed like any
  other (g).

## (e) Validation and assimilation

- Replication: `min_quorum`, `target_nresults`, `max_error_results`, `max_total_results`
  (`wiki:Job-replication`, `wiki:Validators`). #441 submits with a quorum of 1 and one result.
- `sample_trivial_validator` accepts any result that has output files
  (`src:sched/sample_trivial_validator.cpp`). It checks nothing about the text, which #441
  accepts for trusted friends.
- `batch_collect_assimilator` moves each canonical output file into `results/<batch>/` as
  `<workunit>__file_<logical name>` with `boinc_rename`, and writes an error marker when the
  workunit has no canonical result (`src:sched/batch_collect_assimilator.cpp`). The spool is
  under the project directory, so on Azure it is on the Files share.
- `bin/create_work` and `bin/dir_hier_path` read `config.xml` and `bin/` from the project
  directory and `create_work` writes to the database (`src:tools/create_work.cpp`). They
  cannot run on a machine without both.
- `boinc_mkdir` creates directories with mode 0771 (`src:lib/filesys.cpp`); upload slot
  directories are therefore not world-writable. Whether the Docker task, run with
  `--cap-drop=ALL`, can write its slot directory depends on the user ids; this is
  UNVERIFIED and #441's acceptance exercise covers it.
- Later, for anonymous volunteers: replication plus a comparing validator (the C++
  `compare_results` hook, or `script_validator`, which passes only result ids and files) and
  adaptive replication (`wiki:Adaptive-Replication`). Not part of this change.

## (f) Credit, leaderboards, teams

- CONFIRMED built in (`src:sched/credit.cpp`, `wiki:CreditNew`). Teams and leaderboards can be
  disabled with `DISABLE_TEAMS` in `html/project/project.inc` (`wiki:Web-configuration-file`).
  `enable_privacy_by_default` exists (`wiki:GdprCompliance`).

## (g) File serving and signing

- Download: input files are described by `<file_info>` with one or more `<url>`; clients do
  plain HTTP GET (with Range). URLs can point to any HTTP server. `bin/create_work` takes
  `--remote_file URL nbytes MD5`, and remote files get the physical name `jf_<md5>`. Source:
  `wiki:Remote-input-file-management`, `wiki:Staging-input-files`, `wiki:Input-file-handling`.
  CONFIRMED: downloads can be served from blob storage.
- #441 does not use remote files: `usnm-boinc submit` writes each cached scan into the
  project's `download/` tree with `dir_hier_path` and passes it to `create_work` as a local
  input file. So the scans are served by Apache from the Files share, and no SAS or second
  storage account is involved.
- CORRECTED, upload: upload goes through the `file_upload_handler` CGI program, which checks
  an RSA-signed upload certificate in the job's file_info and writes to the local `upload/`
  tree. It cannot be redirected to blob storage. Source: `wiki:FileUpload`,
  `src:sched/file_upload_handler.cpp`. Persistent storage for it on Container Apps is an
  Azure Files mount (Azure NetApp Files and Blob mounts are not supported). Source:
  `https://learn.microsoft.com/en-us/azure/container-apps/storage-mounts`.
- Code signing: executables and app files are signed with the project's code signing key
  (`crypt_prog -genkey`, `sign_executable`, and `update_versions` signs when it installs app
  versions). The private key is meant to be kept offline on a separate machine; the public key
  ships in the client at attach time. Source: `wiki:Code-signing`,
  `wiki:File-signing-utilities`. Key rotation uses `old_key_i` with signatures from the old
  key over the new one.
- Offline signing is supported: if `update_versions` finds no private key in `keys/`, you
  produce `FILENAME.sig` for each app file on the separate machine and place them beside the
  files before running `update_versions`. Source: `wiki:Adding-app-versions` (section "Code
  signing"), `wiki:Code-signing` ("Using a separate code signing machine").
- Upload signing is a separate key pair (`upload_private`, `upload_public`) that stays on the
  server (`wiki:Code-signing`). `dont_generate_upload_certificates` exists for jobs whose
  uploads are not certified (`wiki:Project-options`); we do not use it.
- Files for Docker apps: the `Dockerfile` and `job.toml` are app files, signed like any other.

## (h) Server container images

- `boinc-server-docker` has three images: `mysql` (MariaDB LTS), `makeproject` (runs
  `make_project` once) and `apache` (PHP on Apache, `supervisord` running cron, rsyslog,
  Apache, and `bin/start` as user `boincadm`). All BOINC daemons and the scheduler run in the
  `apache` container, which is what the shared-memory constraint needs. Source:
  `bsd:docker-compose.yml`, `bsd:images/*/Dockerfile`.
- CORRECTED: the published Docker Hub tags (`boinc/server_apache`) are old (about 2019)
  (`https://hub.docker.com/v2/repositories/boinc/server_apache/tags`, read 2026-10-13). We
  would build our own image from the Dockerfiles, pinned to the `server_release/1.6/1.6.2`
  tag.

## (i) Official list, and attaching to an unlisted project

- CONFIRMED optional. The project list in BOINC Manager and the site's list are maintained
  by the BOINC team; a project asks to be listed through BOINC's contact process. Science
  United is a separate service that matches volunteers to projects. Source: `wiki:Going-public`,
  `wiki:ProjectLaunch`, `wiki:Choosing and joining projects`. We do not contact anyone.
- CONFIRMED attach by URL. BOINC Manager's "Add project" accepts a project URL; the client
  fetches `<master URL>get_project_config.php` (`src:client/acct_setup.cpp` line 72). It
  follows HTTP redirects (`CURLOPT_FOLLOWLOCATION` in `src:client/http_curl.cpp` line 527), but
  that redirects on `get_project_config.php` work end to end is UNVERIFIED.
- Master URL stability: changing the master URL forces volunteers to reattach. The master
  page must carry the scheduler URLs in an HTML comment (`<scheduler>...</scheduler>`) and
  `<link rel="boinc_scheduler" href="...">`. Source: `wiki:Master-URL`. Because the client
  requests `get_project_config.php` at the master URL, a purely static master host is not
  enough (design.md section 3).
- Privacy gates: `INVITE_CODES`, `INVITE_CODES_RPC` (regex for account-creation codes) and
  `disable_account_creation` keep the project effectively private. Source:
  `wiki:Web-configuration-file`, `wiki:Project-options`.
- GDPR: `enable_privacy_by_default`, `enable_delete_account`, terms-of-use consent,
  `delete_expired_*` tasks. BOINC stores volunteer email addresses and IP addresses. Source:
  `wiki:GdprCompliance`, `wiki:RightToErasure`.

## Azure facts used in the design

| Fact                                                                                                                | Source and date                                                                                                                                                 |
| ------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Container Apps free grant per subscription per month: 180,000 vCPU-seconds, 360,000 GiB-seconds, 2 million requests | `https://learn.microsoft.com/en-us/azure/container-apps/billing`, read 2026-10-13                                                                               |
| Multiple containers per replica share a replica-scoped volume; `minReplicas` 0 or higher                            | `.../container-apps/storage-mounts`, `.../container-apps/scale-app`, read 2026-10-13                                                                            |
| Azure Files can be mounted; Blob mount is not supported                                                             | `.../container-apps/storage-mounts`                                                                                                                             |
| Ephemeral storage 1 GiB at 0.25 vCPU up to 8 GiB over 1 vCPU                                                        | `.../container-apps/storage-mounts`                                                                                                                             |
| Static Web Apps linking a Container Apps backend needs the Standard plan                                            | `https://learn.microsoft.com/en-us/azure/static-web-apps/apis-container-apps`, read 2026-10-13. Proxying is for `/api` routes; root-path proxying is UNVERIFIED |
| Retail prices                                                                                                       | `https://prices.azure.com/api/retail/prices`, queried 2026-10-13 (see design.md section 6)                                                                      |

UNVERIFIED items that gate real work, in one list:

1. Whether `ENGINE=MyISAM` tables are accepted on the flexible server.
2. Whether BOINC's C++ and PHP clients connect when `require_secure_transport = ON`.
3. Whether Azure Files (SMB) preserves BOINC's setgid, umask and file-lock assumptions under a
   Container Apps mount, and its latency under the upload handler.
4. Whether the BOINC client follows a redirect for `get_project_config.php`.
5. That a plan class file copied from the sample works with the server build that is deployed (names are confirmed in source; no server was run).
6. Whether a released `docker_wrapper` honours `workdir` and `create_args`.
7. Whether the Container Apps Job accepts per-run argument overrides, and whether Entra
   authorisation works for data-plane access to Azure Files from the maintainer's side.
