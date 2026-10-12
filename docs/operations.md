# Operations

Running the ingest pipeline in Azure: the backfill, releases, full rebuilds, and how to watch them. The pipeline is two Container Apps jobs, `caj-usnm-ingest-{env}` (`usnm-ingest run`: new LoC batches, the title catalog and a release) and `caj-usnm-backfill-{env}` (parallel `curate` workers for the initial corpus). Both run inside the VNet as `id-usnm-ingest`; the storage and Cosmos accounts stay private throughout. How they are deployed is in [`infra/README.md`](../infra/README.md); the design is in [08](design/08-azure-infrastructure.md) and [04](design/04-data-sources-and-ingestion.md).

Every command below uses your own `az` sign-in. CI can only roll the API onto new images, so starting jobs and changing settings is always a person's action.

## Settings

An environment's settings live in three places ([`infra/README.md`](../infra/README.md#settings-and-the-deployment-stack), #295), and [`infra/env/schema.json`](../infra/env/schema.json) says which setting goes where:

- **Public choices** (flags, sizes, caps, schedules, most `USNM_*` settings) are committed in `infra/env/<env>.json`. A change to a shared environment goes through a pull request like code, and `scripts/provision.sh` deploys only committed settings:

  ```sh
  scripts/settings.sh --env prod --key USNM_JA_LATIN --value true   # edits infra/env/prod.json
  git switch -c settings/prod-ja-latin && git commit -am "prod: USNM_JA_LATIN on" && git push -u origin HEAD
  # open the pull request; once it's merged:
  git switch main && git pull
  scripts/provision.sh --env prod
  ```

  CI's "stack change needs a provision" warning on the merge stays until you run that provision. Every `scripts/settings.sh` example below that sets a public setting works this way. In dev, an experiment's settings go in its own pull request; to try one before committing it, `scripts/provision.sh --env dev --allow-dirty`.

- **Private settings** (`USNM_ALERT_EMAILS`, `USNM_SEARCH_LOG_READERS`, `USNM_OCR_USERS`, `USNM_OCR_NODE_IPS`, `USNM_OCR_NODE_GROUP`, `USNM_OCR_KEYVAULT_ADMINS`, `USNM_ARCHIVE_ACCOUNT`, `USNM_VNET_ADDRESS_PREFIX`, the certificate ids and `USNM_GITHUB_REPO_IDS`) are in the **settings store**, one Azure App Configuration store for every environment in the subscription, `usnm-config-appcs-<suffix>` in `usnm-config-rg`, each environment's under its own label (`dev`, `prod`, ...). The key is the setting's name. Only the operators can reach it, with Entra ID (the store has no access keys); CI never reads it. `scripts/settings.sh` reads and sets them with your sign-in (in the store, removing any local copy), or `az` does directly:

  ```sh
  scripts/settings.sh --env prod --key USNM_ALERT_EMAILS --value ops@example.org
  # or: store=$(scripts/settings.sh --env prod --key SETTINGS_STORE_ENDPOINT)
  #     az appconfig kv set --endpoint "$store" --auth-mode login --label prod --key USNM_ALERT_EMAILS --value ops@example.org --yes
  scripts/provision.sh --env prod
  ```

  `scripts/settings.sh --env prod --key K --unset` clears a private setting everywhere, so it takes its default: it deletes the key from the store and any local copy. `--unset-local` removes only the local file's entry, so the store's value applies again (provision suggests it when a key is in both places). Until the store exists, private settings stay in the local file; once it does, `scripts/settings.sh --env <env> --migrate` moves them in (it stops, moving none, if the store already has a different value for one), and every provision until then warns. A script that sets a private setting (`bootstrap.sh` sets the repository's ids every run, and the certificate ids and alert addresses; `group.sh` the nodes' group; `settings.sh` the one you give it) writes it to the store, only if the value changed, and removes any local copy, so a re-run never leaves it in two places. An empty value stays a local `""`, which hides the store's value (`teardown.sh` detaches the archive that way); `--migrate` leaves those.

  **The store's lifecycle.** It sits outside every environment's stack, in its own stack `usnm-config` (`infra/settings/main.bicep`), so no provision or teardown of an environment deletes it: `teardown.sh` keeps the environment's private settings under its label, and the next stand-up reads them again. The first provision in a subscription creates it (`scripts/settings-store.sh --deploy`, in that environment's region, registering the `Microsoft.AppConfiguration` provider first if the subscription lacks it), and so does `bootstrap.sh` through its provision; it then waits up to 15 minutes for its new role on the store to apply; `scripts/settings-store.sh --what-if` shows what a deployment would change. It is on the Free tier: $0, 10 MB, 1,000 requests a day. A provision reads an environment's settings in one request and each script's single read is one more, so the cap is far off. The Free tier has no soft delete: a deleted key is gone.

  **Who can read the store.** App Configuration Data Owner goes to the Entra object ids in the store stack's admin list and to the account that last deployed it. The role covers every label: one maintainer operates every environment, so per-environment access isn't worth a store each. A provision reads the store before it deploys anything, and an operator without access stops there with the commands that fix it. To add someone, an operator who has access adds their object id (`az ad signed-in-user show --query id -o tsv`, run by them); `--admins` replaces the list, so give everyone's:

  ```sh
  scripts/settings-store.sh --deploy --admins "<id>,<id>"
  ```

  Without `--admins`, `--deploy` keeps the list it has. With no operator left, an Owner of the subscription grants the role to themselves on the store's group (not on the store, where it would collide with the stack's assignment), adds themselves to the list as above, then deletes that grant: `az role assignment create --assignee-object-id "$(az ad signed-in-user show --query id -o tsv)" --assignee-principal-type User --role "App Configuration Data Owner" --scope "/subscriptions/<subscription id>/resourceGroups/usnm-config-rg"`. A local file without `SETTINGS_STORE_ENDPOINT` (a new machine) takes the endpoint from the store's stack and records it, for every script that reads or writes a private setting, not only provision, so none takes a store it can't find for an unset setting.

  **Why App Configuration, not Key Vault.** These are configuration, not credentials (ADR-0009: there are none), and the Free tier costs nothing. #295 first built a Key Vault per environment; prod's never got past preflight, because the public-network guard-rail policy covers Key Vaults and its exemption was in the same deployment as the vault. No guard-rail definition covers App Configuration, so the store needs no exemption.

- **Everything else** stays in the local file (`~/.config/usnewsmap/<env>/.env` or `.azure/<env>/.env`): the stack's outputs, which every provision rewrites, the subscription and tenant, and what the scripts keep (`USNM_EXPERIMENT`, `USNM_IMAGE_TAG`).

A setting in the wrong place, or in two, stops a provision before it deploys anything. **Moving an environment from before #295** (its public settings are still in the local file) is one command, then a pull request:

```sh
scripts/settings.sh --env prod --migrate   # moves the public settings into infra/env/prod.json
git diff infra/env/prod.json               # check, commit, pull request, merge
scripts/provision.sh --env prod            # creates the settings store if the subscription has none
scripts/settings.sh --env prod --migrate   # moves the private settings into it
```

`scripts/provision.sh --env <env> --what-if` deploys nothing: it reads the settings as a provision would, runs Azure's preflight validation of the stack (a policy denial fails it) and prints the template's changes. In a subscription without the guard-rail stack yet, it prints that stack's changes instead: the environment's preflight needs its policy definitions.

## Deploy the jobs

`scripts/bootstrap.sh --env <env> --ingest` deploys both jobs (or `scripts/settings.sh --env <env> --key USNM_INGEST_JOBS --value true` and `scripts/provision.sh --env <env>` on an environment already on the registry). `USNM_BACKFILL_WORKERS` sets the backfill's workers (default 4), and `USNM_BACKFILL_CRON` an optional schedule for it (a UTC cron such as `"0 9 2-4 10 *"`; empty is manual), which every deployment keeps. The ingest job's Quickwit writer works on the replica's own disk, which holds a weekly delta; a full rebuild adds an NFS share for it, `USNM_INGEST_SCRATCH_GIB` in size (default 0, none; 128 for a rebuild, 08 §8.4), only while the rebuild runs ([Full rebuild](#full-rebuild)). The weekly schedule comes after the first release ([Weekly updates](#weekly-updates)).

## The title catalog

The catalog (`reference/catalog/titles.json`, `places.json`) is built by `titles-sync`, which `run` calls before every release. A batch is released only once its titles are in the catalog. LoC rate limits title records, often well under its published 20 a minute; titles-sync then sends nothing for 65 minutes and resumes more slowly, until 8 h into the run (`--titles-max-runtime-secs`), and the next run continues from where it stopped. `/v1/status` shows `titles.pipeline.awaiting_sync` (titles still to fetch) and `batches_waiting_for_titles`. Place corrections live in git and ship in the ingest image; the next run applies them (04 §4.6):

- **A misspelled or renamed town** that should share a place with the town's other titles: add `{"added": "2026-10-06", "from": "Skaguay", "reason": "old spelling", "ref": 190, "source": "…", "state": "AK", "to": "Skagway"}` to `catalog/overrides/place-aliases.json`. Every override entry carries its audit trail: `added` (the date), `reason`, `source` (what shows the change is right: a URL or a precise citation) and `ref` (the issue or PR), plus `updated` and `history` when it changes later (`catalog/overrides/README.md`). `from` is the city as LoC spells it (spelling variants such as "St."/"Saint" or a trailing state abbreviation already match); `to` is the town's name, which becomes the place's name. Keep the entries sorted by state, then `from`, and each entry's keys in alphabetical order; `cargo test -p usnm-ingest places` checks both, and refuses an alias whose `to` is another alias's `from`. Only merge towns that are one place on the map; a town that was separate (East Providence, Winston before 1913) keeps its own place.
- **Wrong coordinates or name**: add `{"added": "…", "city": "Lusk", "lat": 42.7625, "lon": -104.4522, "reason": "…", "ref": 123, "source": "…", "state": "WY"}` to `catalog/overrides/places.json`, sorted by state, then city, keys in alphabetical order, with `"name"` to change the place's name and `"precision"` (`city`, `county` or `state`) if needed. `source` says where the coordinates come from, `reason` why the override exists. Take the coordinates from the gazetteer (`catalog/gazetteer/us-places.csv`) or another public source, not from LoC, unless the gazetteer lacks the town and one of the place's own LoC records has its point (New Echota, #223). The `geocode` log lists places where LoC's point and the gazetteer's are more than 25 km apart and LoC's was kept (`furthest` in its warning, `disagreement_examples` in the report): start there. A LoC point more than 100 km from the gazetteer's town of that name (when the name isn't ambiguous there) is replaced by the gazetteer's already (`gazetteer_over_loc_places`); check that list too, since the gazetteer can have the name for a different town than the paper's. It also lists places at the median of LoC points more than 25 km apart (`loc_apart_examples`, with the two furthest points and their LCCNs): one of those records likely has another town's point. Each place in `catalog/places.json` records the rule its coordinates came from (`coordinates_from`) and the other names its titles use with the rule that merged each (`variants`).
- **A title whose record lists several places** (a paper that moved): add `{"added": "…", "city": "Chicago", "lccn": "sn84024055", "reason": "…", "ref": 190, "source": "…", "state": "IL"}` to `catalog/overrides/title-places.json`, sorted by `lccn`, keys in alphabetical order.
- **Check the effect first**: run `curl -s https://api.usnewsmap.com/v1/places > /tmp/places.json`, then `cargo run -p usnm-ingest --example place-report -- --places /tmp/places.json`; the report lists the merges, renamed places and places that move.

A corrected point or name ships with the next release, delta or full. A change that moves a published title to another place (most aliases, a title place, new merging rules) waits for the next full release: incremental releases keep published titles where they were published, and log `N published titles' place changes wait for the next full release`. Run a [full rebuild](#full-rebuild) on the E4 profile to apply them; the weekly run never starts one for this, since on the Consumption profile a full release runs out of memory (#172). To refresh the gazetteer for a newer Census year, run `scripts/build-gazetteer.py --year YYYY` and commit `catalog/gazetteer/us-places.csv`.

## Backfill and the first version

Backfill, then publish the first version:

```sh
RG=$(scripts/settings.sh --env prod --key AZURE_RESOURCE_GROUP)
# Queue every batch from LoC's listing and curate it. Downloads are paced to
# LoC's limit (10 bulk requests per 10 minutes per IP), one every 75 s across
# all workers, so this takes about 2.5 days. Each worker stops claiming 22 h
# after it starts and exits 0 once its current batch is done (by about
# 22 h 45 min). Start the job again when an execution has ended, until the
# queue is empty (it resumes where it left off).
az containerapp job start -n "$(scripts/settings.sh --env prod --key BACKFILL_JOB)" -g "$RG"
# When that has finished: curate any leftovers, build the base index, publish.
az containerapp job start -n "$(scripts/settings.sh --env prod --key INGEST_JOB)" -g "$RG"
```

Watch with `az containerapp job execution list -n <job> -g "$RG" -o table` and the saved log queries:

```sh
scripts/logs.sh --list                                                  # the saved queries (ops/queries/)
scripts/logs.sh --env prod --query-name release-progress --span 6h      # docs sent, rate, retries, free disk, memory, merges
scripts/logs.sh --env prod --query-name index-layout --span 2d          # splits, docs and size of each index a release published
scripts/logs.sh --env prod --query-name curation-throughput --span 1d   # batches and pages per hour
scripts/logs.sh --env prod --query-name curate-replicas --span 6h       # each worker's last line, batch and stage
scripts/logs.sh --env prod --query-name errors-by-batch --span 1d
scripts/logs.sh --env prod --query-name job-executions --span 2d
```

The workspace id is the stack output `LOG_ANALYTICS_WORKSPACE_ID`; the queries run with your own sign-in (Log Analytics Reader or more). Traces (`release`, `curate`) and metrics are in Application Insights `appi-usnm-<env>`. With alert emails set, a failed job, a stalled release, a stalled backfill or a backfill replica silent for 15 minutes sends an email (08 §8.1.2). So does a full Log Analytics daily cap (08 §8.1.1): until the cap resets nothing is logged, so a job that looks stalled may still be working; check `az containerapp job execution list` before stopping it. The same views, as charts: the "usnewsmap pipeline" workbook under Application Insights `appi-usnm-<env>` → Workbooks.
The last lines of each worker say why it stopped: `max runtime reached; claiming no more batches` (with the number of batches still queued) when the execution should be started again. `curation finished` on its own means the worker found nothing more it could claim. A batch that failed in this execution is queued again but skipped by that worker, so check `scripts/logs.sh --env prod --query-name errors-by-batch --span 1d` and start the job once more if it lists batches that weren't curated later.

## Switch the API to the published indexes

Once a version is published (`current.json` exists in `reference/`): `scripts/settings.sh --env <env> --key USNM_SEARCH_BACKEND --value quickwit`, then `scripts/provision.sh --env <env>`. Until then the API would have nothing to serve ([`infra/README.md`](../infra/README.md#what-this-slice-runs)).

## Weekly updates

Poll LoC weekly for new batches: `scripts/settings.sh --env <env> --key USNM_INGEST_CRON --value "17 3 * * 1"` (Mondays 03:17 UTC), then `scripts/provision.sh --env <env>` while no ingest execution is running. The schedule is committed in `infra/env/<env>.json` ([Settings](#settings)). Each run fetches LoC's listing once, curates and releases what is new, and exits 0 when there is nothing (08 §8.4). It stops curating 6 h in and releases what it has; if its log says `max runtime reached; claiming no more batches` with batches still queued, start the backfill job to curate the rest. A scheduled job can still be started by hand.

A delta's writer works on the replica's own disk (about 13 GiB free at the start), with no scratch share (#308). That holds a weekly delta: the delta of 10 October 2026 (5,647 pages) used about 0.1 GB, and the full rebuilds of 6 to 9 October (23.7M pages each) peaked at 11 to 14 GB on the share (sampled every 30 s). The writer's disk use grows with a delta's size only up to its caps (a 4 GiB write-ahead log, a 4 GiB split cache and one merge of about 30,000 pages at a time, up to about 9 GB with American Stories' text, 08 §8.4), and a weekly delta reaches none of them. The tightest case is a large delta after a backfill, hundreds of thousands of pages: if the writer reports a full disk, the release fails and publishes nothing (`current.json` stays where it was). Then turn the share on for one run (`USNM_INGEST_SCRATCH_GIB=128`, provision, start the job), and clear it afterwards as in [Full rebuild](#full-rebuild). `disk_free_mb` in the `release progress` lines shows how close a delta came.

## Check a release's splits

Before it publishes, a release waits for the writer's merges: `release merges` lines (every 30 s: step `settle` then `finalize`, splits, merges running and queued, free disk), then `merges settled; closing the index for its final merges`, then `merged; the index is closed to further writes` with the new index's split count. It then logs one `index layout` line per index of the new version and records the same under `indexes` in `reference/<version>/manifest.json`. `scripts/logs.sh --env prod --query-name index-layout --span 2d` lists them. A merged index has at most `pages / 30,000` (rounded down) `+ 7` splits (`split_num_docs_target` in `infra/quickwit/pages-index.yaml`); the release fails rather than publish more, or if merges don't finish within `USNM_MERGE_TIMEOUT_SECS` (default 14400, 4 hours; a setting, `scripts/settings.sh --env <env> --key USNM_MERGE_TIMEOUT_SECS --value <secs>` then `scripts/provision.sh --env <env>`, budgeted under the ingest job's 48-hour replica timeout (the backfill and Japanese OCR jobs keep 24 hours) in `infra/modules/ingestjobs.bicep`), or if the writer reports a full disk or a failed merge, or exits. A writer the kernel killed for memory shows as `the Quickwit writer was killed by signal 9 (SIGKILL): it ran out of memory (…)` in the `command failed` line; `quickwit_rss_mb` in `release progress` is its memory (08 §8.4).

## Searcher caches

The Quickwit sidecar keeps the split footers, fast fields and partial and predicate results in memory, with the capacities in `infra/quickwit/searcher.yaml` (08 §8.4). The API reads the sidecar's metrics every minute and reports them as `api.searcher_cache_*` (08 §8.1.2):

```sh
scripts/logs.sh --env prod --query-name searcher-caches --span 1d        # per replica and cache: MB and items held, hits, misses, evictions, hit rate
```

The same table, and charts of MB held and of misses and evictions, are in the "usnewsmap API" workbook. In Log Analytics:

```kusto
AppMetrics
| where AppRoleName == "usnm-api" and Name startswith "api.searcher_cache_"
| where tostring(Properties.cache) == "split_footer"
| summarize Count = sum(Sum), Held = max(Sum) by Name, Replica = AppRoleInstance
```

For `api.searcher_cache_bytes` and `api.searcher_cache_items` (gauges) read `Held`, the most the cache held in the span; for hits, misses and evictions read `Count`, their total in the span. Split footers are one item per split. A replica's first report counts everything since its sidecar started, the warm-up included.

**Sizing `split_footer_cache_capacity`.** Size it from the release's footer total, and check it against what the cache does after the warm-up. Evictions on their own don't show the footers don't fit: Quickwit 0.9.1 also counts replacing an entry as an eviction, and after a publish the cache drops the old version's footers. The API logs `split footer cache evictions seen` once per searcher run, for the record, without a cause. The footers don't fit when the total is above the capacity, or when footer misses and evictions keep coming hours after the warm-up, with no publish or restart in between. To size it:

1. Add up `footer_bytes` on the version's `index layout` lines (`scripts/logs.sh --env prod --query-name index-layout --span 2d`, column `FooterMb`), or under `indexes` in `reference/<version>/manifest.json`, over every index the version lists: the base, its deltas and the Japanese index.
2. Set `split_footer_cache_capacity` in `infra/quickwit/searcher.yaml` to that total plus about 25% for the deltas the next weekly releases add, rounded up. Quickwit reads `256MB` as 256,000,000 bytes, the unit `FooterMb` uses.
3. Check it fits the sidecar's 4 GiB next to the fast field cache (1 GB), the aggregation memory limit (768 MB) and the partial request and predicate caches (32 MB each). If it doesn't, the fast field cache is the one to shrink first; then the container's memory (`infra/modules/containerapp.bicep`).

The change goes live when it merges (CI applies `searcher.yaml` to the running app). Confirm with `searcher-caches` a day later: after the warm-up, footer misses and evictions stay near 0 (each replica start misses about once per split), and the footer cache's MB held is about the footer total.

## Searcher threads

The sidecar runs each split's search on its `search` thread pool, and downloads from Blob and opens the splits on its main Tokio runtime. The API reads both with the caches, every minute, and reports them as `api.searcher_pool_tasks` and `api.searcher_runtime_*` (08 §8.1.2):

```sh
scripts/logs.sh --env prod --query-name searcher-threads --span 2h       # per replica and minute: search pool ongoing and pending, main runtime threads and busy share
```

Read it while cold searches run (`scripts/load-cold-searches.py`):

- `SearchPending` above 0 with `SearchOngoing` at the pool's size: searches wait for cores. The pool has `RAYON_NUM_THREADS` threads (4, set on the sidecar in `infra/modules/containerapp.bicep`; without it, one per CPU Rust counts, which rounds the 3.75 vCPU quota down to 3). Quickwit doesn't report the size; it is the most `SearchOngoing` reaches.
- `MainBusyPercent` near 100 with the search pool below its size and nothing pending: searches wait on the main runtime. It has `QW_TOKIO_RUNTIME_NUM_THREADS` threads (4, same file; `MainThreads`).
- Neither: look at Blob reads, and at `max_num_concurrent_split_searches` in `infra/quickwit/searcher.yaml`, which limits the split searches downloading at once.

The pool numbers are a reading once a minute, not an average, so a short burst can fall between two. `MainBusyPercent` is busy time over the thread time the runtime had (`api.searcher_runtime_capacity_ms`), so it stays a share however the reports fall into the minutes; it is empty for a replica's first minute and after a searcher restart.

## Search capacity

A second searcher (a Quickwit cluster, #239) is deferred until either of these holds for a week (#238, 10 October 2026): the p90 of the wait for a computation slot (`api.search_queue_wait_seconds`) above 3 s, or a replica's CPU above 3 vCPU outside releases. With alert emails set, two severity 4 alerts watch them (08 §8.1.2):

- _Search capacity: searches waiting for a slot_ emails once a day while each of the last two days had more than 10% of its searches (at least 20) wait over 3 s. The first email covers two days and each later one adds a day, so six emails in a row are the week.
- `alert-usnm-api-replica-cpu-<env>` emails when the replicas' CPU averages over 3 vCPU for 6 hours, and again when it clears (prod only: where the app scales to zero, a window could hold only a start's samples). It averages the replicas together, so with a second replica sharing the load a hot one can sit under it: the workbook's per-replica chart is the record.

An alert can't look back a week (a log search alert reads at most 2 days), so judge the week in the "usnewsmap API" workbook's Search capacity section, which covers the last 14 days by day whatever the time range. Leave out days with a release or a new index version (every search is cold then), and check `scripts/logs.sh --env prod --query-name searcher-threads --span 24h` for whether the search pool or the main runtime is the one that's full. If it holds, the next step is moving Quickwit out of the API's replica (#267, [the searcher app](#searcher-app)), then the cluster in #239.

## Searcher app

By default the Quickwit searcher is a sidecar in each API replica, so every new API image, which also carries the site, starts a new searcher with empty caches and a warm-up (#267). `USNM_SEARCHER_APP` runs the searcher as its own Container App instead, `usnm-search-ca-<env>`, which API and web deploys never restart. It is off until it has been measured; whether to keep it is pending those numbers (below).

**What it deploys** (`infra/modules/containerapp.bicep`). The searcher app runs the sidecar's container: the same image, `infra/quickwit/searcher.yaml`, 3.75 vCPU / 7.5 GiB, thread counts and startup and liveness probes, plus a readiness probe on `/health/readyz`. It listens on all interfaces and advertises its replica's address, and still runs alone (its own cluster id, no peers). Its ingress is internal only: `http://usnm-search-ca-<env>` (port 80, to Quickwit's 7280) inside the environment. It runs one replica, always in prod (elsewhere it scales to zero with the API): a second would start cold and split the cache hits; more search capacity is a cluster (#239). Its system-assigned identity has Blob Data Reader on `qw-index` alone, as the sidecar's had, and the API's user-assigned identity pulls the image. The API app keeps the `api` container alone (0.25 vCPU / 0.5 GiB), and its `USNM_QUICKWIT_URL` names the searcher app. It also keeps its system-assigned identity and that role for now, which `api` doesn't use (below). The replica CPU alert and the workbook's replica CPU chart follow the searcher app.

**Turn it on:**

```sh
scripts/settings.sh --env prod --key USNM_SEARCHER_APP --value true
scripts/provision.sh --env prod
scripts/settings.sh --env prod --key SEARCHER_APP          # usnm-search-ca-prod
```

The provision creates the searcher app and its role first, then rolls the API onto a revision without the sidecar. The old revision, with its sidecar, serves until the new one has loaded and warmed up against the new searcher. The searcher's first revision can start before its role has reached Blob and restart once or twice.

**The API's identity stays.** The provision leaves the API app's system-assigned identity and its Blob Data Reader role on `qw-index` in place. An identity belongs to the app, not to a revision, so removing it in the same provision would cut the old revision's sidecar off from the index while it still serves, and the stack would delete the role; searches would fail until the new revision is ready. Kept, they also let a roll back's sidecar read the index straight away. They go in a follow-up change once the searcher app is kept (or with its other leftovers if it isn't): in `infra/modules/containerapp.bicep`, `apiSystemIdentity` becomes `sidecar`, and that change's provision removes them from an API that no longer has a sidecar. From then on a roll back gives the API a new identity whose role has to reach Blob first, as the searcher's first revision does.

**Deploys with it on.** CI's publish (`scripts/ci/roll-api.sh`) finds the searcher app from the API's `USNM_QUICKWIT_URL`. When `searcher.yaml` changed, it applies it to the searcher app, waits for its new revision, then rolls the API, whose start warms the new searcher; otherwise it leaves the searcher app alone. A new Quickwit image still moves with a provision, and restarts the searcher alone. The API sees the restart in the searcher's metrics within a minute (its counters start again from 0) and runs its start's warm-up again while it stays ready: `trigger` is `searcher_restart` in `scripts/logs.sh --env prod --query-name warm-up --span 2d`. When another warm-up is running then (a start's or a publish's), that run comes after it, and a publish in the meantime waits for it; the API's log says `the searcher restarted during a warm-up; warming it again after that one`. Like a start's, that run reads what the response caches hold and computes only the rest, so the searcher's own caches mostly fill on visitors' first searches.

**What to measure**, a week with it against a week without, leaving out days with a release or a new index version:

- **API to searcher latency:** `scripts/logs.sh --env prod --query-name backend-latency --span 14d`, `AvgMs` and `MinMs` per endpoint. The network hop should add a few milliseconds per call to searches of seconds.
- **Deploys:** after a merge that changes only the site, the API's log has no `the searcher restarted` line, and `scripts/logs.sh --env prod --query-name searcher-caches --span 1d` shows the caches kept their contents.
- **Idle billing of each app:** Cost Management, per resource and meter, daily: idle and active vCPU-seconds of `usnm-search-ca-prod` and of `ca-usnm-prod`, against about 180,000 idle and 14,000 to 32,000 active vCPU-seconds a day for the one app at 2.25 vCPU (3 to 8 October 2026, 09 §9.5). The searcher still polls the metastore every 30 s and answers the API's metrics read every minute; the API app now bills on its own between visitors.
- **Capacity:** with two API replicas, both send searches to the one searcher (each with its own computation slots, 06 §6.6), and both report its metrics, so read one replica's rows in `searcher-caches` and `searcher-threads`. Each replica also warms the searcher after it restarts, one warm-up at a time within a replica but not across them: while two replicas run (a rollout, or the HTTP rule's second replica), a searcher restart gets a warm-up from each, which compute the same misses side by side for up to the warm-up's budget (5 minutes). The searcher app runs one replica in prod and the API usually one, so that's a rollout's few minutes.

**Roll back:**

```sh
scripts/settings.sh --env prod --key USNM_SEARCHER_APP --unset
scripts/provision.sh --env prod
```

The API gets its sidecar back in a new revision, using the system-assigned identity and role on `qw-index` it kept, and the stack deletes the searcher app and its role. The stack deletes them when the deployment returns, which is before the API's new revision is ready, so the old revision has no searcher until then: searches fail, and once its readiness fails the site too, for the new revision's load and warm-up (up to about 13 minutes). Roll back at a quiet hour.

## The Quickwit image

The searcher sidecar, the ingest image (whose Quickwit is the release's writer) and the ingest job's init container run our build of Quickwit v0.9.1 ([ADR-0013](design/adr/0013-quickwit-from-our-fork.md)). `Dockerfile.quickwit-patched` builds it from our fork, [`tgoodyear/quickwit`](https://github.com/tgoodyear/quickwit), at a pinned commit. `infra/quickwit-image.json` records its digest for each environment, and `infra/main.bicep` and CI's publish read it there; an environment the file doesn't list runs upstream v0.9.1.

Build it in an environment's registry, which records the digest in the file:

```sh
scripts/build-quickwit.sh --env prod              # ACR Tasks, about an hour; the build log follows
scripts/build-quickwit.sh --env dev --from prod   # or import prod's build (same digest, no build)
```

If your `az` session expires during the build, ACR still finishes it. Once it has, run the same command again: it finds the tag and records its digest without building (`--rebuild` builds again). Then commit the file in a pull request. Once it merges, CI's publish checks that the registry has the digest (it fails if not) and rebuilds the ingest image on it, which the jobs pick up at their next execution. The sidecar and the init container move with `scripts/provision.sh --env <env>`, run once publish has finished.

**A new patch, or a Quickwit upgrade:**

1. On the fork, commit to the `usnm/v0.9.1-pool` branch (for an upgrade, rebase the commit onto the new upstream tag in a new branch, such as `usnm/v0.9.2-pool`) and tag it with a new `usnm-*` tag, such as `usnm-v0.9.1-pool2`. Names starting `usnm` trigger none of Quickwit's own workflows.
2. In `Dockerfile.quickwit-patched`, set `QW_COMMIT_HASH` to the tagged commit and `QW_COMMIT_TAGS` to the tag without `usnm-` (`v0.9.1-pool2`), which is also the image's tag. For an upgrade, also take the new version's Dockerfile changes (base images, packages) and its `QW_COMMIT_DATE`, and move the `upstream` pin in `infra/quickwit-image.json` and `Dockerfile.ingest`'s default to the new release (CI checks they match).
3. `scripts/build-quickwit.sh --env prod`, then commit, merge and provision, as above.
4. Re-run the cold-search test (`scripts/load-cold-searches.py`) and read [the searcher's threads](#searcher-threads).

**Back to upstream:** remove the environment from `environments` in `infra/quickwit-image.json` (or put its previous digest back), merge, and run `scripts/provision.sh --env <env>` once publish has finished. Publish copies upstream v0.9.1 into the registry if it isn't there and rebuilds the ingest image on it. To turn pooling off in the searcher alone, without a new image, set `QW_AZURE_POOL=false` on the sidecar (`infra/modules/containerapp.bicep`) and provision.

## Cache warm-up

Each API start, and each publish before it swaps the new version in, warms the caches with the home page's examples and the most frequent logged searches (06 §6.5), as does a searcher restart under a running API (`searcher_restart`, [the searcher app](#searcher-app)). They run one at a time: one that starts during another waits for its turn, and none is dropped. One line per run:

```sh
scripts/logs.sh --env prod --query-name warm-up --span 2d                # per run: examples warm, cached, computed, skipped, ms
```

A start isn't ready until its run ends, so in a rollout the old revision serves meanwhile and the new one takes visitors only once it's warm (06 §6.6); every prod start from 3 to 10 October 2026 used its whole 5-minute budget (`Ms` about 300000), but a start that finds everything cached is ready in seconds (below). A start still running at the readiness cap (6 minutes in prod, 1 minute where the app scales to zero) logs `warm-up still running at the readiness cap; reporting ready` and serves visitors while it finishes.

Read `Warm` against `Examples` first: equal means every example's search was in the in-process cache when the run ended (it is counted then, so evictions show). Then:

- **A start that read everything:** `Cached` is the examples plus the logged searches, `Computed` and `Skipped` are 0, and `Ms` is a few seconds. This is the usual start once a version and release have been warmed once.
- **A start that computed some:** `Computed` above 0 means searches no cache held: examples added since, or every search after a release that changed the response format (`RESPONSE_FORMAT` in `crates/usnm-api/src/routes/mod.rs`). Each takes about 11 s, so about 26 fit in the 5-minute budget. `Skipped` above 0 means the budget ran out with that many searches left; the next start reads what this one computed and goes on from there, and a visitor who opens a skipped example computes and persists it.
- **A publish:** `Cached` is 0 or close to it (nothing is cached for a new version yet, unless a visitor computed it meanwhile), and `Computed` is the searches reached in 15 minutes, about 75 of the 100 examples. The next start computes the rest.
- **`GaveWayMs`** is how long a start's computations waited for visitors' searches: a start computes a search only while no visitor's is running or queued. Large values mean visitors kept the searcher busy during the start.
- **`TimedOut` or `Failed`** above 0: the `slow warm-up query` and `warm-up query failed` lines of that run name the example (`api-errors` lists the failures).

Each computed search also logs `slow warm-up query` with its `ms` when it takes 1 s or more, and `api.prewarm_query_seconds` (by `endpoint` and `source`, `cache` or `computed`) has the time of every warm-up query.

## Full rebuild

Rebuild the indexes as one merged base (one-off, or a compaction). A release also builds a full base by itself, without `USNM_INGEST_FULL`, when the published version was built with another search backend, without common-word pairs or with a version of them newer than the release knows (05 §5.5.3), or when `USNM_AMERICAN_STORIES` is on and the published version doesn't have American Stories' text (below); the steps below force one. It is also how merged places and other place changes of published titles reach the site (04 §4.6). Indexes built before the merge settings (September 2026) keep their many small splits: their splits are past any maturation period, so nothing merges them in place, and they are sealed. A full release builds a new base from every curated batch, merged, and publishes a version with no deltas; the previous version stays in Blob for rollback (`current.json`). A full base is also what moves a version to the latest analyzer versions (05 §5.5.3): a delta folds its words as the published version's indexes do, so a change to the folding, such as vulgar fractions as one word (`½`, #168), reaches the site only with a full rebuild. Afterwards `current.json` has the latest `common_grams` and `ja.fold` (2 since #168), as does `build.features` in `reference/<version>/manifest.json`, and the API parses queries for them. It takes the writer lock like any release, so it never runs alongside another writer (08 §8.4.1). Run it when no backfill or ingest execution is running and the backfill has finished:

```sh
RG=$(scripts/settings.sh --env prod --key AZURE_RESOURCE_GROUP)
JOB=$(scripts/settings.sh --env prod --key INGEST_JOB)
scripts/settings.sh --env prod --key USNM_INGEST_FULL --value true
scripts/settings.sh --env prod --key USNM_MERGE_TIMEOUT_SECS --value 14400   # the default; set it only to change it
scripts/settings.sh --env prod --key USNM_DEDICATED_PROFILE --value true     # the E4 profile (#172)
scripts/settings.sh --env prod --key USNM_INGEST_ON_DEDICATED --value true   # and the ingest job on it
scripts/settings.sh --env prod --key USNM_INGEST_SCRATCH_GIB --value 128     # the writer's scratch share (08 §8.4)
scripts/provision.sh --env prod
curl -s https://api.usnewsmap.com/v1/status | jq .titles.pipeline   # awaiting_sync: titles to fetch first
az containerapp job start -n "$JOB" -g "$RG"
```

A full run releases only once titles-sync has fetched every title the listing and the curated batches name (titles LoC doesn't have excepted; a failed fetch counts as left); a base built without them would leave their batches out. With titles to fetch (about 4.5 s each, plus 65 minutes for each time LoC blocks), the first execution may spend its 8 hours on titles-sync and then fail with `titles-sync reached its deadline with N of M titles left … nothing was released` (or `LoC rate limited titles-sync …`). That is expected: start the job again, and it continues where it stopped. The job-failed alert leaves this stop out; the severity 3 _ingest not progressing_ alert fires if no execution follows within 3 hours, or if 3 stops in a row leave as many titles as the first. `scripts/logs.sh --env prod --query-name ingest-endings` lists each execution's ending, and the stop's `command failed` line carries `outcome: titles_left`. Once titles-sync finishes, the same execution builds the base. Watch it:

```sh
scripts/logs.sh --env prod --query-name release-progress --span 6h    # docs sent, rate, memory (quickwit_rss_mb), merges
az containerapp job execution list -n "$JOB" -g "$RG" -o table
```

- `title records` lines every 100 titles, and a `title records` report at the end (`left: 0` and no `throttled` or `out_of_time` when done).
- `release progress` every 30 s: about 750 pages a second before the common-word pairs (#154), so 23.8M pages took about 9 h; with the pairs (each page's index about 1.6× larger) the 2026-10-05 rebuild sent 230 to 460 pages a second, about 15 to 29 h for 23.7M pages (#172). `quickwit_rss_mb` should stay under about 5,500 on the Consumption profile (7,680 MiB container), or about 20,000 on the E4 profile (25 GiB).
- `release merges`: step `settle`, then `finalize`; `splits` falls by 9 every minute or two after ingest ends. About 1.7 h for 23.8M pages in October 2026; with the 60,000-page splits and the common-word pairs, expect about 2.7 h (#156), and at most `USNM_MERGE_TIMEOUT_SECS`. American Stories' text about doubles the index (step 4 of #218 measures the merge time).
- `merged; the index is closed to further writes`, `index layout` (about 800 splits for 23.8M pages at the 30,000-page target) and `released`. About 11 h from the start when the catalog was complete, in the October 2026 rebuild before #154. With the pairs, the 2026-10-05 rebuild sent only 230 to 460 pages a second, so plan for up to about 29 h of sending and about 41 h in all (up to 8 h of titles-sync, sending, up to 4 h of merges), inside the ingest job's 48 h replica timeout (#172, #173).

**Laying the base out by decade (#123, 05 §5.5.5).** To have the rebuild keep each decade in its own splits, so that a date-limited search skips the others, add the setting to the first provision above:

```sh
scripts/settings.sh --env prod --key USNM_PARTITION_DECADE --value true   # the job's --partition-decade
```

Only a full base takes it. The release then logs `decade layout` when it creates the index, a `sent a decade's pages` line each time a decade's pages go to the writer (at most every 30,000 pages of a decade, or the split target with `USNM_SPLIT_DOCS`, below; until then they wait in `/scratch/usnm/quickwit/decade-spill`, a few GB at most at 30,000), and `sent the pages a decade at a time` with the totals before the merges. The `index layout` line of the base has `splits_by_decade`. Check afterwards: `current.json` has `decades: 1`, and `reference/<version>/manifest.json` has `build.features.decades` and `splits_by_decade` under `indexes`. Leaving the setting on or off changes nothing until the next full rebuild: deltas follow the published version (with the field and decade tags on a version laid out by decade, without on one that isn't). To stop searches naming decades at once, without a rebuild, publish a `current.json` without the key.

**One searched text field (#283, 05 §5.5.7).** To have the rebuild search LoC's and American Stories' text in one field (`text_all`), so a search looks each word up once, add the setting to the first provision above, alongside `USNM_AMERICAN_STORIES`:

```sh
scripts/settings.sh --env prod --key USNM_SINGLE_TEXT_FIELD --value true   # the job's --single-text-field
```

Only a full base takes it. The release logs `one searched text field` when it creates the index, and `building index` with `text_layout: 2`. Check afterwards: `current.json` has `text_layout: 2`; `reference/<version>/manifest.json` has `build.features.text_layout` and, in `build.templates.pages`, the `text_all` and `text_all_cg` fields; `/v1/meta` says `"text_layout": 2`. The aggregate then has no `total.american_stories_only` (the site drops its page-count note), while hits keep `matched_in` and the American Stories badge. `USNM_AMERICAN_STORIES_SEARCH` has no effect on such a version (below). Leaving the setting on or off changes nothing until the next full rebuild: deltas follow the published version. To go back at once, publish a `current.json` of a version with a field per text (rollback); a full rebuild without the setting does it for good. Turning `USNM_AMERICAN_STORIES` off on such a version makes the next release a full base: the text can't be left out of the field it shares with LoC's in a delta.

**Page languages (#289, 05 §5.5.8).** To have the rebuild give each page its own languages, so `lang=` filters by them, add the setting to the first provision above:

```sh
scripts/settings.sh --env prod --key USNM_PAGE_LANGUAGE --value true   # the job's --page-language
```

Only a full base takes it. The release logs `building index` with `page_language: true`, and before the merges `pages whose own languages differ from their title's, in the version` with their count (expect about 1.5 to 2 million: mostly pages of multilingual titles). Check afterwards: `current.json` has `page_language: 1`; `reference/<version>/manifest.json` has `build.features.page_language`, the two fields in `build.templates.pages`, `page_languages.pages`, and lists `page_language_baselines.json` (perhaps 10% to 20% more entries than `language_baselines.json`) and `page_languages.json` (about 15 to 20 MB); `/v1/meta` says `"language_filter": "page"`. Compare the release's pages a second with the last rebuild's (`release progress`): detection runs on up to 4 threads beside the writer. Leaving the setting on or off changes nothing until the next full rebuild: deltas follow the published version and carry the moves forward. To go back to title languages at once, roll back to a version without page languages (publish its `current.json`); a full rebuild without the setting does it for good. Don't edit the key out of the serving version's `current.json`: the API keeps the language filter it loaded the version with, and its caches are keyed by the version (it logs `current.json changed page_language without a new version`).
**Larger splits (#283).** To have the rebuild's merges stop at more pages per split than the template's 30,000, so a cold search opens fewer splits, add the setting to the first provision above:

```sh
scripts/settings.sh --env prod --key USNM_SPLIT_DOCS --value 120000   # the job's --split-docs; the value comes from the dev measurement below
```

Only a full base takes it: a delta's few splits keep the template's target. The release logs `split target` when it creates the index and `building index` with `split_docs`. Check afterwards: the base's `index layout` line has about pages ÷ target splits (plus up to 7, per decade with `USNM_PARTITION_DECADE`) and a `largest_split_docs` near the target; `reference/<version>/manifest.json` has `build.features.split_docs` and `split_num_docs_target` at the target in `build.templates.pages`. The release holds the merged index to that target (`docs / target + 5 + 2`) before it publishes. `current.json` doesn't change, and the API needs nothing: it searches whatever splits the indexes have. What bounds the target is the writer (05 §5.5.1, 08 §8.4): a split holds min(its size, 10 GB) of the writer's memory while it uploads, and merged splits can reach about twice the target, at about 74 KB a page with American Stories' text; and the scratch share holds, at once, the largest merge (about twice the target's pages, about 18 GB at 120,000) and, with `USNM_PARTITION_DECADE`, the decade spill (under one target per decade, up to 15 × the target's pages compressed: a few GB at 30,000, about four times that at 120,000), inside 128 GiB. So a target above 30,000 needs the E4 profile. Clear the setting after the rebuild: a full base the release forces by itself (a new analyzer or American Stories version, say) runs on the Consumption profile, whose 7.5 GiB only the template's target fits.

**Settings checklist for the next rebuild (#283).** Every setting the rebuild reads, its value, and what to check afterwards. Set them all before the first provision above; the release reads them only when it builds the base.

| Setting                                                        | For the rebuild                                                                | What it does                                                                                                                                                  | Check afterwards                                                                                                                                                                                                                                                                 |
| -------------------------------------------------------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `USNM_INGEST_FULL`                                             | `true`, cleared afterwards                                                     | Every run builds a full base (`--full`)                                                                                                                       | `released` with `full: true`; `current.json` lists one main index                                                                                                                                                                                                                |
| `USNM_DEDICATED_PROFILE`, `USNM_INGEST_ON_DEDICATED`           | `true`, cleared afterwards in two provisions (below)                           | The E4 profile and the ingest job on it: 25 GiB for the container and the writer tuning (`USNM_WRITER_*`, set by the Bicep)                                   | The `writer tuning` line (`heap: 6GiB`, `commit_timeout_secs: 120`, `queue: 4GiB`); `quickwit_rss_mb` in `release progress` under about 20,000                                                                                                                                   |
| `USNM_AMERICAN_STORIES`                                        | `true` (as now)                                                                | American Stories' text with LoC's (04 §4.9); item 1 needs it                                                                                                  | `current.json` has `american_stories: 1`; the `American Stories` lines of the release ([below](#american-stories-text))                                                                                                                                                          |
| `USNM_SINGLE_TEXT_FIELD`                                       | `true`                                                                         | One searched text field, `text_layout: 2` (item 1, #285)                                                                                                      | As in [One searched text field](#full-rebuild) above: `text_layout: 2` in `current.json`, `build.features` and `/v1/meta`; no `total.american_stories_only`                                                                                                                      |
| `USNM_SPLIT_DOCS`                                              | The target the dev measurement picks; empty until then (30,000), cleared after | Pages per split of the base (item 2)                                                                                                                          | As in [Larger splits](#full-rebuild) above: `split target` line, `build.features.split_docs`, the `index layout` split count                                                                                                                                                     |
| `USNM_PARTITION_DECADE`                                        | `true`                                                                         | Decade partitions (item 3, #123)                                                                                                                              | `decades: 1` in `current.json`; `splits_by_decade` in the manifest                                                                                                                                                                                                               |
| (none)                                                         | Automatic                                                                      | A full base moves to the latest analyzer, version 2 (item 4, #168, #278; `½` is one word, #287's characters fold as themselves)                               | `current.json` has `common_grams: 2` and `ja.fold: 2`; `build.features` has `common_grams: 2` and `ja_fold: 2`                                                                                                                                                                   |
| `USNM_PAGE_LANGUAGE`                                           | `true` once measured on dev (below)                                            | Page-level language (item 5, #289): each page gets its own languages, found from its words, so `lang=` and the counts by language use them                    | As in [Page languages](#full-rebuild) above: `page_language: 1` in `current.json` and `build.features`, the `page_language` and `page_language_set` fields in `build.templates.pages`, `page_languages.json` listed in the manifest; `/v1/meta` says `"language_filter": "page"` |
| `USNM_OCR_LOCAL`                                               | `true`                                                                         | Our OCR in place of LoC's where rule v1 prefers it, on every batch the base reindexes (item 6, #296, [Our OCR in releases](#our-ocr-in-releases-296))         | The `our OCR in ocr-local` line; `reference/<version>/ocr_local.json` with the pages replaced; `build.features.ocr_local: 1`                                                                                                                                                     |
| `USNM_JA_LATIN`                                                | `true` if the published version has the Latin text (as now)                    | Our OCR's Latin text on pages LoC ships without text (#203); a full base without it drops those pages                                                         | `build.features.ja_latin: 1`; `reference/<version>/ja_latin.json` holds as many pages as before                                                                                                                                                                                  |
| `USNM_CONGRESS`                                                | `true` if the published version has a Congress set (none yet)                  | The Congress pages (#229); a release without it publishes a version without them (even with no new batch), and one that moves to a new analyzer rebuilds them | `build.features.congress: 1`; `current.json` has `congress`                                                                                                                                                                                                                      |
| (none)                                                         | Automatic                                                                      | The catalog on `main` (`catalog/overrides/title-places.json`), the 17 title moves of #281 included (item 7); only a full base moves published titles          | `/v1/titles/{lccn}` of a moved title (such as the Anti-Slavery Bugle) names its new place                                                                                                                                                                                        |
| `USNM_MERGE_TIMEOUT_SECS`                                      | `14400` (the default), unless dev shows larger splits merge for longer         | How long the release waits for the merges                                                                                                                     | `release merges` reaches `merges settled` well inside it                                                                                                                                                                                                                         |
| `USNM_INGEST_SCRATCH_GIB`                                      | `128`, cleared afterwards (below)                                              | The writer's scratch share, its account and private endpoint                                                                                                  | Free disk in `release merges` stays well above the largest merge (about twice the split target's pages), plus the decade spill's `peak_disk_mb` while pages are sent                                                                                                             |
| `USNM_AMERICAN_STORIES_SEARCH`                                 | `true` (as now)                                                                | No effect on a `text_layout: 2` version                                                                                                                       | No `USNM_AMERICAN_STORIES_SEARCH=false has no effect` warning in the API's log                                                                                                                                                                                                   |
| `split_footer_cache_capacity` (`infra/quickwit/searcher.yaml`) | Resized after the release if needed                                            | The searcher's footer cache must hold every footer of the version ([Searcher caches](#searcher-caches))                                                       | The new version's `footer_bytes` total against the capacity, then footer misses after the warm-up                                                                                                                                                                                |

Afterwards, benchmark with `scripts/bench-cold-searches.py` and compare with `pages-v20261008-1` (1,162 GB; median search 21.4 s with both texts' fields searched, #251), and note the index size, the send rate and the total time on #283.

**Before the go-ahead: what to measure on dev (#275).** The rebuild runs only once these are measured and the owner says go. Each needs the dev environment stood up with a 1% version (#275, the stand-up flow in PR #351) and American Stories' text:

- **One field and larger splits (items 1 and 2):** a full release on dev with `USNM_SINGLE_TEXT_FIELD`, `USNM_PARTITION_DECADE` and a few values of `USNM_SPLIT_DOCS` (30,000 and larger), each against a release without them: index size, pages per second, merge time, `quickwit_rss_mb` at its peak while the largest split uploads, the decade spill's `peak_disk_mb` (in `sent the pages a decade at a time`) beside the largest merge's scratch use, and cold search times. The search cluster's loader (`load --split-docs`, [below](#search-cluster-experiment)) sends documents with a field per text only, so it can measure the split target but not the one-field layout; a dev release measures both. Pick the rebuild's `USNM_SPLIT_DOCS` from the memory peak and the cold-search gain.
- **Time and profile (#233):** the last full rebuild took 35.8 h on E4. Confirm E4 (rather than Consumption, which can't take a larger target) from the dev send rate scaled to the corpus, inside the job's 48 h replica timeout; whether two writers are worth it is still open (#233), and the release has one writer.
- **Page-level language (item 5, #289):** a full release on dev with `USNM_PAGE_LANGUAGE` against one without: pages per second (detection runs on up to 4 threads beside the writer), the count of pages whose own languages differ from their title's, the sizes of `page_language_baselines.json` and `page_languages.json`, and the cold `/v1/aggregate` times of 05 §5.5.8 with `lang=` on the complement side and with `USNM_PAGE_LANGUAGE_FILTER=match`.

Then turn the full rebuild off, and the E4 profile and the scratch share with it, in two provisions: the job has to leave the profile before the profile can go. Run them only once the rebuild's execution has ended (`az containerapp job execution list`), since the first one takes the share away from the job.

```sh
scripts/settings.sh --env prod --key USNM_INGEST_FULL --unset
scripts/settings.sh --env prod --key USNM_SPLIT_DOCS --unset   # if it was set: a forced full base on Consumption takes only the template's target
scripts/settings.sh --env prod --key USNM_INGEST_ON_DEDICATED --unset
scripts/settings.sh --env prod --key USNM_INGEST_SCRATCH_GIB --unset    # deletes the share, its account, endpoint and DNS zone
scripts/provision.sh --env prod
scripts/settings.sh --env prod --key USNM_DEDICATED_PROFILE --unset
scripts/provision.sh --env prod
```

Don't leave `USNM_INGEST_FULL` set: every run, scheduled ones included, would rebuild the whole corpus. Don't leave `USNM_INGEST_SCRATCH_GIB` set either: the share, its private endpoint and its DNS zone cost about $25 a month (#308), and deltas don't need them. Its contents are scratch: a release empties the writer's data before it starts, so deleting the share loses nothing, and the next rebuild's provision creates it again, empty, under the same names. Don't leave `USNM_DEDICATED_PROFILE` set either: while the environment has the E4 profile it pays the Dedicated plan management fee every hour (about $0.10 an hour in East US 2 in October 2026), whether or not a node runs. An E4 node itself (4 vCPU, 32 GiB) costs about $0.39 an hour while the job runs.

**Why the E4 profile.** The first full rebuild with the common-word pairs (#154) was killed when the Quickwit writer ran out of memory at the Consumption profile's limit (7.5 GiB with the scratch share's init container), 6.5 h in, at 26.5% (#172). On the E4 profile the ingest container gets 3.0 vCPU and 25 GiB, and the writer a 6 GiB indexing heap, a 120 s commit timeout and a 4 GiB ingest queue (`USNM_WRITER_HEAP`, `USNM_WRITER_COMMIT_SECS`, `USNM_WRITER_QUEUE`, set by `infra/modules/ingestjobs.bicep`), so it writes splits at the 30,000-page target instead of cutting small ones every 30 s and merging them (#172). The `writer tuning` line at the start of the release shows them. A full run also takes longer than the October 2026 rebuild (about 230 to 460 pages a second instead of 750), which is why the ingest job's replica timeout is 48 h (#173). Benchmark afterwards with `scripts/bench-cold-searches.py`. The commit timeout was 600 s at first: the ingest queue's write-ahead log is only truncated when a commit is published, so it filled in about 4 minutes and Quickwit refused pages ("no shards available") for the rest of each 10 minutes, and the October 6 rebuild was stopped at 15% after averaging 132 pages/s. At 120 s the queue holds about 2 minutes of pages.

## Prune old index versions

Releases never delete an index, so `qw-index` keeps every version ever built and every rebuild that stopped part way (in October 2026, about 1.2 TB of the 2.8 TB of current index blobs; #245). `usnm-ingest prune` deletes the Quickwit indexes that no kept version names. It keeps:

- the serving version: every index `current.json` lists, its Japanese (`ja`) and Congress (`congress`) index sets included;
- its rollback, the version `current.json` names as `previous_version`;
- any published version newer than the serving one (there is one only after `current.json` was rolled back by hand, so rolling forward still works). A newer version whose snapshot has a `manifest.json` counts as published even if its run says `building`: a release writes the manifest, then `current.json`, then records the publish, and that last step can fail.

A kept version's indexes are all that its records name: `current.json`, its run in the pipeline state, the `indexes` of `reference/<version>/manifest.json`, and its Japanese and Congress index ids (`pages-ja-{date}-{n}`, `congress-{date}-{n}`). Every other `pages-…` or `congress-…` index is deleted: older versions, and rebuilds that were stopped or failed. Indexes of other names (such as Quickwit's own `otel-…`) are left alone. It deletes through Quickwit (`DELETE /api/v1/indexes/{id}`), which removes the split files and the index's metastore entry together, never with blob deletes.

Without `--delete`, it's a dry run: it lists each index it would delete with its splits, documents and size (its published splits' files), and the total, and changes nothing. It takes the writer lock like a release, so it fails with ``lock `quickwit-writer` is held by …`` while a release runs, and a release can't take the lock while prune holds it. It refuses to run when `current.json` can't be read or names no indexes, when the serving version has no index run, or when neither the rollback's run nor its manifest names its indexes (and when a manifest's `indexes` isn't a list of `index_id`s). With `--delete`, it reads `current.json` and the runs again before each delete and stops if what it keeps has changed. If the writer lock can't be renewed, it gives up the delete under way at once and stops its Quickwit node. So `--delete` needs `--quickwit-bin`, a node prune starts itself; `--quickwit-url` (a node already running, which Quickwit lets finish a delete its caller left) is for dry runs only.

Don't roll `current.json` back by hand while prune runs: a rollback doesn't take the writer lock, and prune checks what it keeps only before each delete, so a version made serving during a delete of its index loses it. Run it in the ingest job, as an execution with other arguments (`scripts/start-job.sh`), after a new version is published and checked, and while no ingest or backfill execution is running. An execution can't mount the scratch share, and prune doesn't need it: its Quickwit node only reads the metastore and deletes, with its data in a temporary directory. You need `az` signed in with access that can start the job (the owner's Contributor on the resource group) and read the logs. Dry run first:

```sh
RG=$(scripts/settings.sh --env prod --key AZURE_RESOURCE_GROUP)
JOB=$(scripts/settings.sh --env prod --key INGEST_JOB)
QW=(--quickwit-bin /usr/local/bin/quickwit --quickwit-metastore azure://qw-index --quickwit-index-root azure://qw-index)
scripts/start-job.sh --env prod --job INGEST_JOB -- prune "${QW[@]}"   # dry run: lists, deletes nothing
az containerapp job execution list -n "$JOB" -g "$RG" -o table  # until it has Succeeded
scripts/logs.sh --env prod --query-name prune --span 2h
```

`prune keeps` names the serving and rollback versions and every index they keep; check that the serving version is the one `/v1/status` reports and that the rollback is the one you want. Then one `prune candidate` line per index it would delete, with `bytes` and `gib`, and `prune plan` with the count and `total_gib` (the execution's console log ends with the whole report as JSON). If the list is what you expect, delete:

```sh
scripts/start-job.sh --env prod --job INGEST_JOB -- prune "${QW[@]}" --delete
scripts/logs.sh --env prod --query-name prune --span 2h    # a "deleted index" line for each, then "pruned"
```

A large index can take minutes to delete: Quickwit deletes each split file before it answers. A failed delete stops the run with the error; the indexes already deleted stay deleted, and running it again carries on. Afterwards the rollback is still there, but the versions before it are gone and can't be rolled back to. The bill drops later than the delete: the deleted split files become previous blob versions, which the `qw-index/` lifecycle rule deletes after a day, and then sit in the 3-day soft delete (08 §8.1).

## American Stories' text

The release indexes American Stories' text beside LoC's (04 §4.9, 05 §5.5.4, #218) only with `USNM_AMERICAN_STORIES` (the job's `--american-stories`), off by default. The first release with it builds a full base, so turn it on for a full rebuild, after step 4 of #218 has measured the index size and rebuild time, and with an explicit go-ahead:

1. Run the writer to the end: `scripts/ja-ocr/start-quality.sh --env prod --command american-stories-write` on the one-replica job. It marks each finished year with `curated/american-stories/years/{year}.json`; start it again until all 180 years have one (its log's `american stories write finished` line counts them). A release reads only the years with a marker, and a year finished after the base reaches the base's pages only at the next full rebuild.
2. Do the [full rebuild](#full-rebuild), with the setting added to its first provision:

   ```sh
   scripts/settings.sh --env prod --key USNM_AMERICAN_STORIES --value true
   scripts/settings.sh --env prod --key USNM_INGEST_FULL --value true          # optional: the setting forces a full base anyway
   ```

   With no year marked yet, the run fails at its start, before curation and titles-sync (`--american-stories (USNM_AMERICAN_STORIES) is set, but the curated store has no finished year of American Stories' text`). Its log has `American Stories' text` (years, parts) near the start, then `American Stories' text for the batch` per batch (pages, `text_mb`: what the release holds in memory for the batch), and `American Stories' text indexed` (documents with the text, and those with only it) before the merges.

3. Check the result: `current.json` has `american_stories: 1`; `reference/<version>/manifest.json` has `built_from.american_stories` and `build.features.american_stories`, and `american_stories.json` lists the parts read. On the site, a word from an American Stories headline finds pages, and a hit whose match is only there shows its snippet from that text.

Keep `USNM_AMERICAN_STORIES` set afterwards: weekly deltas then write the text for their pages too. Clearing it makes the next release publish a version without `american_stories`, so searches leave the text out; setting it again rebuilds in full.

To stop searching the text at once, without a release (to time cold searches without it, #251, or if it costs too much), switch the API's setting off, and on again the same way:

```sh
scripts/settings.sh --env prod --key USNM_AMERICAN_STORIES_SEARCH --value false   # on again: true, or --unset for the default
scripts/provision.sh --env prod
```

The API then searches LoC's text alone, whatever `current.json` says (05 §5.5.4): no `matched_in`, no American Stories snippets or badge, no `total.american_stories_only`. The indexes keep the text, so switching back needs no rebuild. The provision changes the API container's settings, so Container Apps starts a new replica: it loads the version, runs the startup warm-up and reports ready when it ends (about 5 minutes) or the readiness cap passes (6 minutes in prod, `USNM_READY_CAP_SECS`, 06 §6.6), and the old replica serves until then. Responses computed in one state are cached under keys of their own (`…|american_stories=off` when off), so the first switch off finds none cached: the warm-up computes what its budget allows and other searches start cold. Switching back on finds the responses persisted before (in Blob) still there. Check the result in the replica's start-up log line `American Stories' text search (USNM_AMERICAN_STORIES_SEARCH)` (`setting`, `in_version`, `searched`, `text_layout`) or in `/v1/meta` (`"american_stories"`). Browsers may keep a response they fetched in the other state for up to a day (06 §6.5).

On a version with one searched text field (`text_layout: 2`, [Full rebuild](#full-rebuild)) the setting can't switch the text off: it shares the field with LoC's. The API logs `USNM_AMERICAN_STORIES_SEARCH=false has no effect on this version` and searches both texts. To leave the text out there, roll `current.json` back to a version with a field per text, or rebuild in full without `USNM_AMERICAN_STORIES`.

## Japanese OCR: mixed pages and English text

**Mixed pages** (#204, 04 §4.8). The OCR job's targets (`targets-v3`) include the Japanese titles' pages whose LoC text is from 0.35 to under 0.65 word-like (`loc_text = mixed`, about 2,000 pages). List them once on the one-replica job, then start the OCR job as usual:

```sh
RG=$(scripts/settings.sh --env prod --key AZURE_RESOURCE_GROUP)
scripts/ja-ocr/start-quality.sh --env prod --command targets            # caj-usnm-jaone: writes ocr-ja/targets-v3.jsonl
# its log ends with `targets written` and the pages by loc_text (mixed: about 2,000); then
az containerapp job start -n caj-usnm-jaocr-prod -g "$RG"   # `run`: OCRs the targets not done yet
```

Listing streams the archive of every batch with a Japanese title again, as the first listing did, to find the pages LoC ships without text; pages already read are skipped by the run. Another cut: `start-quality.sh --env prod --command targets --mixed-below 0.8`, then `start-quality.sh --env prod --command run --mixed-below 0.8` (its own list and claims, `targets-v3-mixed80`); 0.35 or less lists no mixed pages. At the job's pace in October 2026 (a 2-page issue in about 21 s and a 4-page one in about 38 s per replica, mostly loc.gov's pacing) the 2,000 pages take about 5 hours on the two replicas, about $4.50 of Consumption time (2 × 4 vCPU and 8 GiB at about $0.43 an hour each), plus the listing. The next release publishes their Japanese text (an overlay-only release when nothing else is new). A mixed page joins the Japanese index only with Japanese on it: expect about 200 (Rocky Shimpo's front pages).

**English text of our OCR** (#203, 04 §4.8). Off by default. With `USNM_JA_LATIN` (the job's `--ja-latin`), a release gives the pages LoC ships without text a main-index document with the Latin-script text our OCR read on them (their English ads, mastheads and sections), and gives a page LoC read badly our text in place of LoC's where ours reads more words. No full rebuild is forced:

```sh
scripts/settings.sh --env prod --key USNM_JA_LATIN --value true
scripts/provision.sh --env prod
```

- The next release adds the pages LoC ships without text, a few thousand documents, in a new main index: the weekly delta, or, when LoC has published no new batch, a delta of just these pages (`no newly curated batches; releasing our OCR's Latin text of pages LoC ships without text in a delta of its own`). After that, a run with no new batch and no new OCR releases nothing. The log has `indexed our Japanese OCR's Latin text` with `missing` and `in_place_of_locs`; `reference/<version>/ocr_ja.json` has `latin: {added, pages, hidden, rule}`, `ja_latin.json` lists the pages, and the build record has `features.ja_latin`.
- Our text in place of LoC's reaches a batch when a release indexes it, so for the published Japanese batches at the next [full rebuild](#full-rebuild) (with the setting on). A delta of only the pages LoC ships without text indexes no batch, and its log line says so.
- Check: an English search for a word in a Colorado Times ad (for example `"Larimer Street"` over 1945) finds pages marked "Our OCR", whose viewer link has no highlight. The page totals don't change: these pages were counted since #139.
- Clearing the setting stops new documents; a delta keeps the ones already indexed, and the next full rebuild leaves them out.

## Check Japanese search

Our OCR of the Japanese pages LoC ships without text (04 §4.8) goes live only with a release: each release reads the OCR job's output once, near its start, and logs `Japanese OCR overlay` with the pages it took. Pages the job reads after that wait for the next release; when nothing else is new, a run releases them on their own ("releasing the new Japanese OCR on the same indexes"), or, with `USNM_JA_LATIN` and Latin text on pages LoC ships without text, in a delta of just those pages ([above](#japanese-ocr-mixed-pages-and-english-text)). After a release publishes, check from outside:

```sh
scripts/check-ja-search.py --expect <issue> … --late <issue> …
```

Issues are named as the OCR job logs them (`ocr issue done`, `<lccn>_<date>_ed-<n>`): `--expect` ones the job finished before the release's `Japanese OCR overlay` line, `--late` ones after it. The script checks that `/v1/meta` names a Japanese index and its pages, that common words (日本, 戦争, 米国, 真珠湾, 収容所) return pages (a version without the index answers 422), that hits are marked as our OCR with the engine, that each `--expect` issue has pages of our OCR (the paper's hits on that day for any of の, に, は, を, in the issue's edition), and that each `--late` issue has none yet. Every request is pinned to the version `/v1/meta` named and doesn't follow redirects, so a release that publishes mid-check fails it rather than mixing versions. It sends Do Not Track, so its searches stay out of the search log, and exits 1 if a check fails.

## Congress pages

The Congressional Globe (#229 phase 1, [04 §4.10](design/04-data-sources-and-ingestion.md#410-congress-pages-229)) is fetched from UNT's page OCR by `usnm-ingest congress-globe`, and the Serial Set (phase 2) from GovInfo by `usnm-ingest congress-serialset` ([below](#the-serial-set-from-govinfo)). A release with `USNM_CONGRESS` indexes both in one Congress set. All are off by default, and nothing has run yet.

1. **The archive first.** UNT's responses go to the raw store, so a fetch that stops resumes without asking UNT again. That is about 75 GB of OCR JSON before compression. Set `USNM_ARCHIVE_ACCOUNT` ([Archival storage](#archival-storage)) before the first fetch (decided on #229, 10 October); without a raw store the job logs a warning, keeps nothing and refetches a stopped item from its first page.
2. **The job.** Deploy `usnm-cong-caj-<env>` (saved as `CONGRESS_JOB`): manual, one replica of 0.5 vCPU and 1 GiB, on the ingest image as `id-usnm-ingest`.

   ```sh
   scripts/settings.sh --env prod --key USNM_CONGRESS_JOB --value true
   scripts/provision.sh --env prod
   RG=$(scripts/settings.sh --env prod --key AZURE_RESOURCE_GROUP)
   az containerapp job start -n "$(scripts/settings.sh --env prod --key CONGRESS_JOB)" -g "$RG"
   ```

   Each execution lists the collection, leaves out the items another holds, and fetches the items without a finished attempt. It goes one request every 3 seconds, as UNT's robots.txt asks, and stops between pages at 22 hours. Start it again after each execution until its report (JSON, at the end of the log) has `"left": 0`. The whole Globe is about 105,000 pages, about 4 days. The log has `Congressional Globe items` with the counts, `Globe item curated` per item with its pages by `date_from`, and `the server asked us to wait` (with `server=UNT`) on a 429 or 5xx. One item, or a fetch again: `scripts/start-job.sh --env prod --job CONGRESS_JOB -- congress-globe --items metadc29274 --max-runtime-secs 79200 [--refetch]` (the arguments after `--` replace the job's, so keep its `--max-runtime-secs`). The Cosmos lock `congress-globe` refuses a second execution while one runs.

3. **The release.** Turn on `USNM_CONGRESS` (the ingest job's `--congress`), then provision. The next ingest run builds `congress-<stamp>-<n>` from the finished items, even with no new LoC batch (`no newly curated batches; releasing the changed Congress pages on the same indexes`). Later runs keep it until the fetch writes more items. Check `reference/<version>/congress.json` (the items and pages indexed), `congress/baselines.json` and `current.json`'s `congress` block. The API serves the pages with `source=congress` while `USNM_CONGRESS_SEARCH` is on (the default; [configuration](configuration.md)).

Clearing `USNM_CONGRESS` makes the next release publish a version without the Congress set, even with no new LoC batch (`no newly curated batches; releasing the same indexes without the published Congress set`); `USNM_CONGRESS_SEARCH=false` hides it without a release. A full rebuild with `USNM_CONGRESS` on rebuilds the Congress index too when the analyzer changes.

### The Serial Set from GovInfo

`usnm-ingest congress-serialset` (#229 phase 2, [04 §4.10](design/04-data-sources-and-ingestion.md#410-congress-pages-229)) needs no API key: GovInfo's sitemaps, MODS and PDFs are public URLs. About 222,000 documents in about 11,900 volumes: 444,000 requests and about 4 TB of PDFs, each streamed to the replica's disk, read with `pdftotext` and deleted (about 5 days at a request a second).

1. **The archive first**, as for the Globe: the MODS and each document's page texts (`pages.json`, about 20 GB in all) go to the raw store under `congress/govinfo/{volume}/{package}/{lastmod}/`, so a stopped volume resumes without downloading its PDFs again. The PDFs are never kept. Without a raw store the job logs a warning and refetches a stopped volume's PDFs.
2. **The job.** Deploy `usnm-sset-caj-<env>` (saved as `SERIALSET_JOB`): manual, one replica of 1 vCPU and 2 GiB, on the ingest image (which carries poppler-utils) as `id-usnm-ingest`.

   ```sh
   scripts/settings.sh --env prod --key USNM_SERIALSET_JOB --value true
   scripts/provision.sh --env prod
   RG=$(scripts/settings.sh --env prod --key AZURE_RESOURCE_GROUP)
   az containerapp job start -n "$(scripts/settings.sh --env prod --key SERIALSET_JOB)" -g "$RG"
   ```

   Each execution reads the sitemap index and the 182 yearly sitemaps (all of them even with `--years`), then fetches the volumes without a finished or empty attempt, and those whose packages differ from the ones their newest attempt read (`source_packages`, a fingerprint of each package id and its `lastmod`: a document GPO changed, added or dropped, whatever its `lastmod`). An attempt written before the fingerprint was kept has none, so its volume is fetched once more (from the raw store's copies) and counted in `changed`. It goes about one request a second, backs off on 429 and 5xx (`the server asked us to wait`, `server=GovInfo`), and stops between documents at 22 hours. A sitemap or MODS request may take 120 seconds a try and a PDF 30 minutes, and every request ends by an hour after the deadline (a try that couldn't start with time left, and a retry past then, fail the request), so the job finishes and unlocks before its 24-hour timeout. Start it again after each execution until its report (JSON, at the end of the log) has `"left": 0` and `incomplete` empty. The log has `Serial Set volumes` with the counts, `Serial Set volume curated` per volume, and a warning per document not read. A volume is written only when every document is read or left out for good; otherwise it is in `incomplete` (with how many documents weren't read), nothing is written for it and the next execution fetches it again (what was read is in the raw store). The report lists the documents: `missing` (GovInfo answered 404 for the MODS, or for a PDF the MODS names; never kept, so asked for again) and `unreadable` (pdftotext failed, or MODS that isn't MODS), which keep their volume `incomplete`; `no_pdf` (the MODS names no PDF and GovInfo has none) and `undated` (no usable `dateIssued` or session year), left out for good; and `empty_volumes` (every document left out for good: an empty attempt, `empty.json` with the documents and why, is written in place of pages, so later executions skip the volume until GPO changes its packages or `--refetch`). A volume still `incomplete` after a few executions with the same document is a file GovInfo lacks or pdftotext can't read: look at the document on GovInfo before deciding what to do. `date_precision` counts documents dated to the day, month or year. Some volumes or years: `scripts/start-job.sh --env prod --job SERIALSET_JOB -- congress-serialset --years 1820,1821 --max-runtime-secs 79200 [--serials 33] [--refetch]` (the arguments after `--` replace the job's, so keep its `--max-runtime-secs`: without it the job has no deadline and a long run can hit the 24-hour timeout before it unlocks). `--years` picks the volumes with a package in those years' sitemaps; each is still made of its packages in every sitemap, so it is written whole and a later run of all years skips it. The Cosmos lock `congress-serialset` refuses a second execution while one runs; it is separate from the Globe's, so the two fetches can run together.

3. **Updates.** GPO is still adding volumes. Run the job again monthly: it rereads the sitemaps and fetches only new or changed volumes. The next release with `USNM_CONGRESS` rebuilds the Congress set.
4. **Check.** After the release, `current.json`'s `congress.series` has `serialset` with its pages and dates (1817 to 1998), and `source=congress&series=serialset` searches only its pages. A hit links to the page in the document's PDF (`…/content/pkg/{id}/pdf/{id}.pdf#page={seq}`).

### The Serial Set on-prem

The same fetch can run on an OCR node instead of as `usnm-sset-caj-<env>`, to spend no Azure compute ([ADR-0008, amendment of October 2026](design/adr/0008-private-networking.md#amendment-october-2026-the-on-prem-serial-set-runner-229), [08 §8.3.2](design/08-azure-infrastructure.md#832-on-prem-serial-set-runner)). Don't start the Azure job while the node runs it: the node's lock is in a local file, which the job's Cosmos lock doesn't see.

1. **Settings.** `USNM_SERIALSET_ONPREM=true` in `infra/env/<env>.json` (through a pull request), and the node's Arc identity's object id in the private setting `USNM_SERIALSET_RUNNER`. The lake's firewall takes its addresses from `USNM_OCR_NODE_IPS`, which the exchange account already uses; check it still holds the home IP (`curl -s https://api.ipify.org` on a node).

   ```sh
   az resource show -g rg-usnm-prod -n usnm-ocr-node3-arcs-prod --resource-type Microsoft.HybridCompute/machines --query identity.principalId -o tsv
   scripts/settings.sh --env prod --key USNM_SERIALSET_RUNNER --value <object id>
   scripts/provision.sh --env prod --what-if   # the policy step adds the lake; the stack adds the IP rule and the grant
   scripts/provision.sh --env prod
   ```

   A role assignment can take a few minutes to apply.

2. **The node.** Build `usnm-ingest` from source in a container, so the node gets no toolchain, and install it with poppler-utils and the units in `scripts/ocr-node/` (`usnm-sset.service`, `usnm-sset-status.service`, `usnm-sset-status.timer`, `sset-status.py` as `/usr/local/bin/usnm-sset-status`):

   ```sh
   mkdir -p src
   curl -sL https://codeload.github.com/tgoodyear/usnewsmap.com/tar.gz/<commit> | tar xz -C src --strip-components=1
   nice -n 19 podman run --rm --cpus 3 -v "$PWD/src:/src:Z" -w /src docker.io/library/rust:1-bookworm \
     cargo build --release --locked -p usnm-ingest --bin usnm-ingest
   sudo dnf install -y poppler-utils
   sudo install -m 0755 src/target/release/usnm-ingest /usr/local/bin/
   sudo install -m 0755 src/scripts/ocr-node/sset-status.py /usr/local/bin/usnm-sset-status
   sudo install -m 0644 src/scripts/ocr-node/usnm-sset.service src/scripts/ocr-node/usnm-sset-status.service \
     src/scripts/ocr-node/usnm-sset-status.timer /etc/systemd/system/
   sudo install -d -o usnm -g usnm -m 0750 /var/lib/usnm-sset /var/lib/usnm-sset/raw /var/lib/usnm-sset/reference
   sudo systemctl daemon-reload
   sudo systemctl enable --now usnm-sset-status.timer
   ```

   `/etc/usnm/sset.env` (mode 0640, group `usnm`) sets `USNM_CURATED_URL=https://<lake>.blob.core.windows.net/curated`, `USNM_RAW_URL=/var/lib/usnm-sset/raw`, `USNM_STATE_FILE=/var/lib/usnm-sset/state.json` and `USNM_REFERENCE_URL=/var/lib/usnm-sset/reference` (an empty directory; the fetch reads nothing there). The raw store holds the MODS and page texts, about 20 GB, so a stopped volume resumes without its PDFs. Before the lake is open, `USNM_CURATED_URL` can be a local directory: the fetch then fills the raw store, and a later run against the lake reads every volume back from it without asking GovInfo for the PDFs again.

3. **Run.** `sudo systemctl start usnm-sset` runs the whole fetch, at a request a second, until every volume is written or left (no deadline), and then stops. It runs as `usnm` with the Arc identity. Logs: `journalctl -u usnm-sset`; the report is the JSON at the end. `/var/lib/usnm-sset/status.json` (every 10 minutes) has the volumes to fetch, written, incomplete, the rate per hour and the hours left. Start it again until the report has `"left": 0` and `incomplete` empty, as for the job.
4. **Close.** When it's done, set `USNM_SERIALSET_ONPREM` to false and provision: the lake closes, the grant goes and the policy drops the exemption. Remove the private setting with `scripts/settings.sh --env prod --key USNM_SERIALSET_RUNNER --unset`.

## OCR quality audit

`jaocr.py quality` (`ja-ocr/quality.py`) measures how good LoC's OCR text is, by language and decade, to show where reading pages again would pay off, and how often a page's language differs from its title's first catalog language. It runs in the Japanese OCR job's image because the curated store is reachable only from the VNet. It reads every curated part of every batch in the published version once, keeps a fixed sample of pages (2% by default; a page is in it when a hash of its `doc_id` falls under the cut, so a rerun takes the same pages) and scores each sampled page's stored text. Word tokens are letters only (surrounding punctuation stripped, a word hyphenated across a line break joined), at least 2, case-folded.

The default metric, v2, first finds the page's language:

- Candidates are the title's catalog languages with a word list, plus English: wordfreq's, or for Hawaiian and Yiddish our own (`ja-ocr/wordlists/`, built by `scripts/ja-ocr/build-wordlists.py`; sources and licenses in its README). Each language's function words are its 100 most frequent words of 2 letters or more. The winner is the language whose function words are the largest share of the page's tokens, if that share is at least 0.10, at least 10 different function words appear, and among the function words only one of the two has, the winner has at least twice the runner-up's tokens. Otherwise the page is `und`: fewer than 20 word tokens, garbled, or in a language with no list (Dakota, Choctaw, Navajo, Latin, Welsh and others).
- A page whose words are mostly in another script takes the title's language in that script: Hebrew script is `yid` or `heb` (if the title lists both, the one whose function words clearly fit better), Cyrillic is Russian, Ukrainian, Bulgarian or Serbian. Serbian in Cyrillic is scored letter for letter in Latin script, against wordfreq's Serbo-Croatian list. Words are read with Yiddish ligatures (װ, ױ, ײ) as their two letters. A title that lists no language in that script gets `und`.
- `mixed`: a page of 200 words or more is cut into 50-word windows (a last, shorter window counts if it has at least 25 words; a shorter tail is left out), and each window gets a language if one clearly wins (the same test, with 5 different function words). Two languages that each win at least 20% of the decided windows, and at least 2 windows, make the page mixed. Each decided window is scored against its own language; windows no language wins (garbled, or too few function words) aren't scored. A second script holding at least 20% of the words in another of the title's languages also makes a page mixed (an English column on a Yiddish page): that part has no word minimum of its own, only the 5 different function words a window needs.

Then it scores the page against its language:

- `function_share`: the share of its word tokens that are the language's function words. Text in one language has a similar share from page to page, different for each language: on a handful of LoC pages checked while building it, about 0.41 to 0.46 for English, 0.25 to 0.31 for German, 0.36 to 0.39 for Spanish, and about 0.2 for Polish and Lithuanian, whose most common words are single letters and don't count. Compare a language's pages with its own median and read the low tail as damage.
- `damage_rate`: near-misses / (near-misses + tokens that are one of the language's 20 most frequent words). A near-miss is one OCR edit from one of those 20: a letter substituted or deleted, a thin letter (c f i j l n r t v) inserted, or one letter read as two thin ones ("h" as "li"). English "tbe", "tlie", "thc", "aud", "nnd", "ot", "iu"; German "bie", "unb", "ift", "fich", "dcr". Real words don't count: anything in the top 5,000 words of the language, of the title's other languages or of English ("she", "an", "or", "hand", "tho"). Danish and Norwegian also accept each other's words and the "aa" spelling of "å" ("paa", "aar"); Czech and Slovak each other's. A page with `damage_rate` over 0.1 counts as damaged, over 0.25 as badly damaged.
- `garbage_share`: the share of its tokens that rmgarbage-style rules flag: over 40 characters, mostly punctuation, the same letter 3 times in a row, 4 or more Latin letters with no vowel or only vowels, or a capital inside a lower-case word ("tHe").

`--metric v1` runs the first version: `dict_share`, the share of word tokens in wordfreq's top 200,000 words for the title's first catalog language. It is too lenient to rank pages: those lists hold common OCR errors ("tbe", "aud", "ift", "bie") and no frequency cut separates them from rare real words, so medians were about 0.95 for English and 0.86 for German. It also scored pages of multilingual titles against the first language listed, so Yiddish, Serbian and Polish papers catalogued as English first came out worst.

Start it as one execution of the job with `quality` and its options as the arguments; the script prints the execution's name:

```sh
scripts/ja-ocr/start-quality.sh --env prod --sample-pct 10 --min-pages 50
```

`az containerapp job start --args quality` alone doesn't work: when any container flag is given, the CLI sends a container override named after the job, with no image, settings or resources. The script sends the job's own template with only the arguments changed.

The job's replicas (`USNM_JA_OCR_REPLICAS`, 2) share the work. The parts are cut into chunks of whole batches (about 40 parts each); a replica claims a chunk under `audit/ocr-quality-v2-<version>-<pct>pct.run/<execution>/claims/`, scans it, and writes its sampled pages to `pages/<chunk>.jsonl.gz` and a `done/<chunk>.json` marker. A replica renews its claims every 5 minutes; a claim not renewed for `JAOCR_AUDIT_LOCK_MINUTES` (60) is taken over, and the replica that lost it drops its copy (a chunk's pages are the same whoever scans it). So a replica that dies costs only its chunks' time. When every chunk is done, the replica that creates `reduce.lock` tallies all the chunks, writes the outputs and the final status, renewing the lock as it goes, and then writes `finished.json`. The other waits for `finished.json` (taking over a reduce lock that goes stale), logs `ocr quality results written by another replica` and exits; a retried replica of a finished run exits at once. Progress writes stop once every chunk is done, and a waiting replica restores the final status if a late one replaced it. A new execution starts a new run directory, so a rerun redoes the work and overwrites the outputs. The outputs are shared by every execution with the same version and sample size, so the reducer publishes under `audit/ocr-quality-v2-<version>-<pct>pct.publish.lock` (released after, taken over when stale): two overlapping runs publish one after the other, and the later one's files win. Each replica scores parts in 6 worker processes (`JAOCR_QUALITY_WORKERS`; reading the parts is partly I/O, so more workers than the 4 vCPUs pays) and needs well under the job's 8 GiB: each worker holds a few small word lists and a cache of the last 262,144 distinct tokens' scores. The 2% run took 69 minutes on one replica before the work was shared and the token cache added (October 2026). The job's 24-hour replica timeout is its limit. `ja-ocr/bench_quality.py` times the per-page scoring on synthetic pages.

Output, in the curated container (the data account is reachable only through its private endpoint, like the search log below):

- `audit/ocr-quality-v2-<version>-<pct>pct.json`: every table, the summary (with the language agreement shares), the word lists and thresholds used.
- `audit/ocr-quality-v2-<version>-<pct>pct-<table>.csv`, one per table:
  - `by-language-decade`, `by-language`, `by-decade`, by detected language (`mixed`, `und`, and `no_text` for empty and short pages are rows of their own): pages sampled, shares empty and short, `text_pages`, shares `und` and `mixed`, mean, median and 10th, 25th, 75th and 90th percentiles of `function_share`, mean and median `damage_rate`, shares damaged (over 0.1) and badly damaged (over 0.25), mean `garbage_share`.
  - `language-agreement`: the crosstab of the title's first catalog language against the detected page language (`mixed:ger+eng` names both), with pages and the share of the row.
  - `agreement-summary`: for all pages with text, those of single-language titles, of multilingual titles, with no title, and of each first language: the shares detected as the first language, that differ, and detected as another language, `und` and `mixed`, each of the pages with text.
  - `worst-titles` (100) and `worst-batches` (50) by median `damage_rate`, with the detected-language mix (`ger:40 eng:3 und:2`); `undetermined-titles` (100), the titles with the highest `und` share.
- `audit/ocr-quality-v2-<version>-<pct>pct-pages.csv.gz`: every sampled page but Japanese titles', one row each: `doc_id`, title, date, batch, the title's languages, the detected language and decision, the winner's and runner-up's function-word shares, the script, `function_share`, `damage_rate`, `garbage_share` and word count. About 475,000 rows at 2%, for spot checks and for deciding how to tag page languages. It is compressed as the run goes; a sample of more than 3,000,000 pages (over about 12%) gets no page file (`ocr quality pages file dropped`), only the tables.

Titles and batches enter the worst lists with at least `--min-pages` pages with a `damage_rate`, and `undetermined-titles` with that many pages with text. The same rows are in Log Analytics, one `ocr quality` line each with `metric` and `table` fields, then `ocr quality finished` with the totals; `ocr quality progress` lines come about once a minute from each replica, with the chunks and batches done so far across the run. `scripts/ja-ocr/quality-rows.sh --env prod --execution <execution>` prints a run's rows as JSON lines:

```sh
scripts/logs.sh --env prod --query-name ocr-quality --span 2d
```

A v2 run also writes `status/ocr-quality.json` in the reference container for the status page: `{batches: {done, total}, finished_at, metric, pages_sampled, sample_pct, started_at, summary, updated_at, version}`, at the start, with each progress line and at the end. At the end `finished_at` and `summary` are set: `summary.agreement` (`differs_share`, `mixed_share`, `und_share` of all pages with text, `multilingual_differs_share` of multilingual titles' pages) and `summary.languages`, each detected language with at least 200 sampled pages (`pages`, `function_share_median`, `damage_rate_median`, `damaged_share`). Every field is present, and null when there is nothing to measure: the agreement shares when no sampled page has text, `multilingual_differs_share` when no page of a multilingual title does (a small sample), and a language's three scores when none of its pages was scored (a language with no word list, such as Cherokee). A run that fails leaves its last progress, with no `finished_at`. A failed write is logged and the audit goes on.

Read the numbers with these limits in mind:

- It is a sample. At 2% a small title or a single decade of a rare language has few pages; check `text_pages` and `damage_pages` before trusting a row.
- `damage_rate` has a floor: real words outside the top 5,000 that are one edit from a common word ("ana", "hen", "ion" in English) count as misses, so clean English scores about 0.04 to 0.05. Old spellings do too, so compare decades within a language more than languages with each other. Misreads that are real words ("tho" for "the", "ber" for "der") and misreads two edits away ("bcr", "beu" for "der", "den") don't count, so it undercounts damage.
- `und` mixes three things: pages too short to tell, pages so garbled no language fits, and pages in languages with no word list. `garbage_share` and the `undetermined-titles` list help tell them apart. Heavily damaged pages can fall to `und` instead of their language, so a language's damage shares leave out its worst pages.
- Detection only chooses among the title's catalog languages and English: a German page in a title catalogued as English only is `und`, not German. English is always a candidate, so a page counted as English in a title that doesn't list English is English text (ads, a column) by its function words.
- Pages of titles that list Japanese are left out (our own OCR covers them) and counted in the `japanese_skipped` row.

### Damage metric parity (#296)

The v2 metric's per-page code is `ja-ocr/damage_v2.py`, which `quality.py` imports. `usnm_core::damage` (cargo feature `damage`) is its Rust port, which the OCR pipeline uses (12 §12.5 Steps 5 and 6). Two checks keep them the same: the same language, decision, second language and script, equal token, word, head, near-miss and function-word counts, and rates within 1e-9.

- In CI, `cargo test -p usnm-core --features damage` scores the pages of `crates/usnm-core/tests/data/damage-parity.jsonl.zst` again: 66 pages of loc.gov's OCR text from 33 titles in 25 catalog languages, and 17 made-up pages for the traps between Python's string handling and Rust's. It fails when `damage_v2.py`'s sha256 isn't the one the file records. After a change to `damage_v2.py`, port the change and score the same pages again: `python3 ja-ocr/damage_parity.py --rescore <old file> --out <new file>` (wordfreq 3.1.1; no loc.gov requests). `--pages-from <file> --out <path> --synthetic` builds a new one from loc.gov, with jaocr.py's pacing.
- In the cloud, on curated text: `jaocr.py damage-parity` samples 400 pages of the 10% audit's per-page CSV, spread across languages, decisions and scripts, and writes `audit/damage-parity/<stamp>.jsonl.zst` in the curated container. It logs each page's doc_id and title languages (`damage parity page`), never text. `usnm-ingest ocr-damage --parity <that path>` scores them in Rust ([below](#ocr-damage-tables-296)).

```sh
scripts/ja-ocr/start-quality.sh --env prod --command damage-parity --pages 400
```

### OCR damage tables (#296)

`usnm-ingest ocr-damage` scores pages with the Rust port (12 §12.5 Step 6). It runs as one-off executions of the ingest job, which reads and writes the curated container. `scripts/start-job.sh` starts one with the job's image and settings and other arguments. An execution can't mount the job's scratch share, and this command needs none. It logs counts and doc_ids, never text. Run the three modes in this order: parity, the gate, then the full pass.

1. **Parity on curated text** (Step 5's cloud check). Write the pages with `damage-parity` ([above](#damage-metric-parity-296)), take the path from its `damage parity written` line, then:

   ```sh
   scripts/start-job.sh --env prod --job INGEST_JOB -- ocr-damage --parity audit/damage-parity/<stamp>.jsonl.zst
   ```

   It logs `ocr damage parity` with the pages that agree and up to 50 doc_ids that don't, and fails if any page differs.

2. **The gate.** The 10% audit's pages, picked by the audit's own sampler from every curated row of `pages-v20261003-1` (duplicate copies included), scored in Rust and compared with the audit's per-page CSV, joined on doc_id and batch. The CSV rounds every score to 4 decimals, so Rust's are formatted the same way before the comparison. A page agrees when the language, the decision and the three scores all match; the gate passes when at least 99.99% of the CSV's 2,368,911 rows agree. It writes no tables, only its result, `audit/damage/v2/gate/<version>-<pct>pct-<stamp>.json`, and the `ocr damage gate` line (counts by kind of difference, up to 50 doc_ids). It fails when the gate does. The result goes in a draft report PR.

   ```sh
   scripts/start-job.sh --env prod --job INGEST_JOB -- ocr-damage --version pages-v20261003-1 \
     --sample-pct 10 --compare-csv audit/ocr-quality-v2-pages-v20261003-1-10pct-pages.csv.gz
   ```

3. **The full pass**, on the live version (`index_version` in the reference container's `current.json`), once the gate has passed:

   ```sh
   scripts/start-job.sh --env prod --job INGEST_JOB -- ocr-damage --version <live version> --max-runtime-secs 165600
   ```

   Each batch version gets `audit/damage/v2/<batch>_ver<NN>.parquet`, one row per page: `doc_id`, `lccn`, `date`, `edition`, `seq`, the base `batch`, `batch_version`, `text_status`, `lang`, `decision` (`detected`, `mixed`, `und`, or `no_text` for pages without text), `mixed_with`, `script`, `damage_rate`, `function_share`, `garbage_share`, `words`, `head` and `near`. Beside it, `<batch>_ver<NN>.summary.json` holds per-title counts (pages, scored pages, und, mixed, damaged and badly damaged), the median damage rate and pages by language. The summary is written last and marks the batch version done, so a later run skips it and pays only for new batches. When every batch of the version has its tables, `audit/damage/v2/versions/<version>.json` lists them; the seeder and `ocr-sync` read that list. A second run on the same version writes nothing. `--batches a,b_ver02` scores only those batches and doesn't write the list. A page is scored against its title's languages in the version's `titles.json`, which the command requires (the working catalog only fills in titles it lacks). A later correction to a title's catalog languages doesn't rescore tables already written. Tables are create-only, so to rescore, delete both the `.parquet` and the `.summary.json` of each of that title's batch versions, then run the pass again.

   The ingest job has a 48-hour replica timeout. `--max-runtime-secs` stops the run from starting new batches (it finishes those it has started), and the next execution continues. `ocr damage progress` lines come once a minute with the pages per second. Batches are scored `--parallel` at a time, by default one per CPU. On one laptop core the scoring runs at about 1,650 pages a second, so the corpus's 23.7 million pages are a few hours of CPU on the job's 4 vCPUs, plus reading the curated parts. The Python audit's 10% sample took 5,725 s. Time the first full run and record it here. On the Consumption profile the job costs about $0.43 an hour, and reading curated parts that have moved to Cool adds about $0.01 per GB read.

**A dev-scale test** (dev holds nothing outside a focused experiment, so this needs the maintainer's approval first). Locally, from the repository root:

```sh
python3 scripts/fixture-batches.py --out-dir /tmp/usnm-dmg
cd /tmp/usnm-dmg
ingest="cargo run --release --manifest-path $OLDPWD/Cargo.toml -p usnm-ingest --"
$ingest --state-file state.json --curated curated --reference reference run --list batches.json --synthetic --index-dir reference/indexes
v=$(jq -r .index_version reference/current.json)
USNM_REFERENCE_URL=$PWD/reference USNM_CURATED_URL=$PWD/curated python3 $OLDPWD/ja-ocr/jaocr.py quality --sample-pct 100
$ingest --state-file state.json --curated curated --reference reference ocr-damage --version "$v" \
  --sample-pct 100 --compare-csv "audit/ocr-quality-v2-$v-100pct-pages.csv.gz"
$ingest --state-file state.json --curated curated --reference reference ocr-damage --version "$v"
$ingest --state-file state.json --curated curated --reference reference ocr-damage --version "$v"   # writes nothing
```

On 10 October 2026 that gate agreed on all 1,872 fixture pages, and the second full run skipped both batches. In dev, after `scripts/provision.sh --env dev`: curate and release a few small batches (`dlc_zurich_ver04,dlc_misctopsn83025894_ver01,dlc_misctopsn83021129_ver01`), run `scripts/ja-ocr/start-quality.sh --env dev --sample-pct 100`, then the three commands above with `--env dev`, the dev version and the dev audit's CSV, and `--command damage-parity --pages 50`. Check the tables, the gate result and the second run's `skipped`, then `scripts/teardown.sh --env dev`.

## Text the Japanese titles' pages hold that search can't reach

`jaocr.py mixed` (`ja-ocr/mixed.py`) measures two gaps the Japanese OCR leaves. The OCR job read a page only when LoC's text for it was missing, empty, short or under 35% word-like; a Japanese query searches only our OCR, any other query only LoC's text. Its October 2026 results led to the `mixed` targets and the English text of our OCR in the main index ([above](#japanese-ocr-mixed-pages-and-english-text)); the command still measures by the 35% test.

- Japanese on pages LoC read as words: a page with an English column and a Japanese one, read in English, can pass the 35% test, and its Japanese column is then garbled Latin. For every page with LoC text of a title that lists Japanese, in the batches that hold such titles, it counts pages by word-like share (bands from under 0.35 to 0.9 and over); the other titles in those batches, same scanning and OCR but English only, are the control.
- English on the pages we read: NDLOCR-Lite reads some English (mastheads, short lines), but only into the Japanese index. For every page of our OCR it counts the tokens that are among English's 5,000 most frequent words (3 letters or more).

```sh
scripts/ja-ocr/start-quality.sh --env prod --command mixed          # prints the execution's name
scripts/ja-ocr/quality-rows.sh --env prod --execution <execution>   # its "mixed pages" rows, once it has finished
```

It writes `audit/mixed-pages-<version>-<execution>.json` and a CSV per table in the curated container (named by execution, so two runs never write the same files): `loc-text-bands` (Japanese titles against the control), `loc-text-by-title`, `loc-text-samples` (15 page ids per band, with their loc.gov links, for looking at the scans), `our-ocr-english` (by why LoC's text wasn't used), `our-ocr-by-title` and `our-ocr-samples`. It runs on the one-replica audit job, `caj-usnm-jaone-<env>`, which `start-quality.sh` picks for it. Should a second replica run it anyway (the ja-ocr job), one does the work, renewing a lock; the other waits for its finished marker (`mixed pages written by another replica`) and takes over if the lock goes stale, and a retried replica of a finished execution exits at once. `our-ocr-english` and `our-ocr-by-title` also give the median Latin share of our OCR's letters and the share of pages over half Latin.

## American Stories against LoC's text

`jaocr.py american-stories --year Y [--year …] [--sample-pct 10]` (`ja-ocr/american_stories.py`, #205) compares [American Stories](https://huggingface.co/datasets/dell-research-harvard/AmericanStories) (Dell et al. 2023, CC BY 4.0), a re-OCR of Chronicling America, with LoC's text on our pages. It streams each year's tarball from Hugging Face (0.4 to 7.6 GB; nothing is written to disk), joins scans to our pages by their file names, read in a first pass (a scan named `<date>_p<page>_<lccn>_<reel>_<date><ed>_<image>.json` is our `<lccn>_<date>_ed-<ed>_seq-<page>`; one with no page, `pNone`, about 3% of 1865, takes its image's place in the issue, a rule that agrees with 92.8% of 1865's named pages; a second copy of a page is skipped), keeps the audit's fixed sample (`quality.sampled`), and reads LoC's text for the sampled pages of those years from the batches whose dates overlap them. For pages of titles whose first language is English it reports the join, words per page, the audit's English damage rate and function-word share for each text, pages matching a fixed term list in either text, and LoC's damage rate by the share of regions American Stories marked Illegible.

```sh
scripts/ja-ocr/start-quality.sh --env prod --command american-stories --year 1865 --year 1925
scripts/ja-ocr/quality-rows.sh --env prod --execution <execution>    # its "american stories" rows
```

It writes `audit/american-stories-<version>-<execution>.json`. It runs on the one-replica audit job, `caj-usnm-jaone-<env>` (`ja-ocr/solo.py` still keeps a second replica out, as for `jaocr.py mixed`). A failed run raises _Japanese OCR or audit job failed_ (severity 3), not the ingest alert.

### Indexing only where American Stories differs (`--diff`, #251)

`--diff` adds a measurement for indexing American Stories' text only where it differs from LoC's, to cut the second text field's cost to cold searches (#251). On the same pages it tokenizes both texts as the index does (`usnm_core::text`: hyphen rejoining, ligatures, split on what isn't a letter or digit, ASCII folding, tokens up to 40 characters), aligns them (a patience diff: tokens unique to both texts as anchors, `difflib.SequenceMatcher` with `autojunk=False` on small gaps without one), and builds each page's delta for k = 0, 2, 3 and 5: every region where American Stories' tokens differ from LoC's, with k of its tokens each side (where LoC has tokens American Stories hasn't, just the k each side), overlapping or touching windows merged. A term in American Stories' text is in LoC's or in the delta, and a phrase of n words that matches only in American Stories' text is inside one window once k is at least n - 1; the run checks both. New `american stories` tables:

- `diff_summary` (per year and k): American Stories', LoC's and the delta's tokens, the delta's share of American Stories' tokens (in total, and the median and 90th percentile per page), unique terms per page summed for each text and the delta (postings grow with them), `terms_lost` (should be 0), every page's distinct bigrams and trigrams that match only in American Stories' text and how many the delta keeps (`ngram2_loss`, `ngram3_loss`), the same for those whose words are all in LoC's text (`ngram*_edge*`: together only in American Stories'), n-grams that would match falsely if the windows ran together with no position gap (`ngram*_spurious_if_joined`), and `secs_per_1000_pages` for the diff.
- `diff_phrase` (per year, for the term list and 10 common phrases such as "new york" and "cross of gold") and `diff_phrase_group` (per year, for those two groups and the bigrams and trigrams of the run's `only_american_stories` contexts): pages matching in LoC's text, in American Stories', only in American Stories' (`only_words_all_in_loc` of those), and per k the ones the delta keeps (`kept_k*`, `loss_k*`, `edge_kept_k*`, `spurious_if_joined_k*`).
- `diff_edge_examples`: up to 10 pages a year with a phrase whose words are all in LoC's text but together only in American Stories', with loc.gov links.

```sh
scripts/ja-ocr/start-quality.sh --env prod --command american-stories --year 1865 --year 1925 --diff
```

The diff runs after the comparison and takes about 40 s per 1,000 pages of 8,000 tokens (measured on synthetic pages; the run logs its own figure in `american stories diff year`).

## American Stories' text for our pages

`jaocr.py american-stories-write [--year Y …]` (`ja-ocr/american_stories_write.py`, #218) writes American Stories' text (Dell et al. 2023, CC BY 4.0) for our pages to the curated container, for the release to index beside LoC's. It streams each year's tarball from Hugging Face once, places every scan on our page as `american-stories` does, and writes `american-stories/pages/<lccn>/<year>-<nnn>.parquet`: one row per page with `doc_id`, `lccn`, `date`, `text` (each article's headline, byline and text in American Stories' order, separated by blank lines), `articles` (JSON: each article's headline, byline, `start`/`end` in `text`, bounding boxes in scan pixels and the legibility of its regions), `legibility` (the page's text regions by legibility) and the scan's `width` and `height`. A page's second copy is skipped. Each year ends with a marker `american-stories/years/<year>.json` holding its counts; a rerun skips marked years, so an interrupted run resumes. A download that goes 5 minutes without data, or breaks off, fails and the year starts again from the top (up to 3 tries, a minute apart, each logged as `american stories year failed; retrying`); a retry writes the same parts under the same names. A year counts as read only when the gzip stream reached its end marker and, when Hugging Face sends one, the byte count matches `Content-Length`, so a download that stops at a file boundary inside the tarball is retried rather than marked. All 180 years are about 340 GB to stream.

```sh
scripts/ja-ocr/start-quality.sh --env prod --command american-stories-write          # every year
scripts/ja-ocr/start-quality.sh --env prod --command american-stories-write --year 1865
```

It runs on the one-replica job and logs `american stories year written` per year. `jaocr.py american-stories` also lists, per term and year, up to 8 pages that match only in American Stories' text with the words around the match (`only_american_stories`), for checking those matches by hand.

## Search log

The API keeps every search from the site, with only its filters, page count and UTC day, in the `searches` container ([ADR-0012](design/adr/0012-anonymous-search-log.md), 06 §6.8). Nothing expires `searches/days/` and `searches/import/`; staged batches are deleted after 7 days and stay in soft delete for 3 more. To read it:

```sh
scripts/settings.sh --env prod --key USNM_SEARCH_LOG_READERS \
  --value "$(az ad signed-in-user show --query id -o tsv)"   # private: the settings store (Settings, above)
scripts/provision.sh --env prod
scripts/searches.sh --env prod --days 30      # searches per day, top queries, queries that found nothing, date spans
```

The last two tables count searches by date span (the whole corpus range, over 10 years, 1–10 years, a year or less) and by the decade partitions they touch, bucketed as `usnm_core::decade` (pre-1820 is one). They hold counts and shares only, never queries. A search is whole range when it covers the corpus bounds the site serves (`/v1/meta`, read from the environment's `SITE_URL`); if those can't be read, the script says so and takes each version's widest search instead. They decide whether to build the next full base with `USNM_PARTITION_DECADE` (#123): only date-limited searches skip splits.

The data account is reachable only through its private endpoint, so run `searches.sh` from a network that reaches it. The first day file appears an hour after the first full UTC day with the log deployed.

## Archival storage

Production keeps no batch archives (ADR-0006): LoC is the source of record. The archival account keeps everything we download from outside Azure, so re-curation, benchmarks and other environments never download it again: LoC's batch archives and batch lists now, other sources later. Benchmarks are one use.

**The account** (`infra/archive/`) is its own deployment stack, `usnm-archive`, in its own group, `rg-usnm-archive`, outside every environment's stack. An environment's provision or teardown can't delete it. It is StorageV2, Entra only (no keys, no SAS), with no public network access, versioning, 14-day soft delete, a lifecycle rule that moves `raw/` and `sets/` to the Cold tier, and a `CanNotDelete` lock. Its stack detaches what leaves the template rather than deleting it, and denies deletes outside the stack except of the lock and of private endpoint connections.

**Policy keeps it that way.** The group, `rg-usnm-archive`, has the same guard-rail assignments as an environment's groups (`usnm-deny-*`, `usnm-audit-*`), with public network access denied and no public accounts, so policy denies turning shared keys or public network access back on (ADR-0009). The definitions come from the `usnm-guardrails` stack, which `deploy` creates or updates first, as an environment's provision does. While the account exists, the last environment's teardown keeps that stack. The diagnostic settings audit has an exemption for the account (`usnm-archive-no-diagnostics`, a waiver): its logs have nowhere to go, since every Log Analytics workspace belongs to an environment and goes with its teardown. Once a workspace that outlives the environments exists, the account should send its blob writes and deletes there, and the exemption goes.

Check what a deploy would change, then deploy:

```sh
scripts/archive-store.sh what-if --subscription <id>    # changes nothing
scripts/archive-store.sh deploy --subscription <id>     # prints the account's resource id
```

`what-if` shows the guard-rail definitions' changes, then the account's. Until the definitions exist, the account's what-if can fail on its assignments; `deploy` creates them first.

**An environment uses it** with `USNM_ARCHIVE_ACCOUNT` set to that resource id and a provision. That adds a private endpoint in the environment's VNet (`pe-usnm-archive-blob`, in its Blob private DNS zone) and, in the account's group, a custom role on `raw` for `id-usnm-ingest` that lists, reads and creates blobs but can't delete them (`usnm archive writer`). With the search cluster on, the bench identity (`usnm-qw-bench-id-{env}`) also gets read access on `raw` and the writer role on `sets`. The ingest and backfill jobs get `USNM_RAW_URL` pointing at `raw`. Clearing the setting removes only the environment's endpoint and roles. The account's lock can block removing an endpoint to it, so drop the setting this way. `scripts/teardown.sh` detaches an environment that has the setting on by itself first ([Dev environment lifecycle](#dev-environment-lifecycle)).

```sh
scripts/archive-store.sh unlock
scripts/settings.sh --env dev --key USNM_ARCHIVE_ACCOUNT --unset   # a private setting: deletes it from the settings store
scripts/provision.sh --env dev
scripts/archive-store.sh deploy        # the lock back
```

**What curation does with it:**

- **Every archive it downloads is kept** byte for byte as fetched, at `raw/{batch}/{archive file}`. It is written in 8 MiB blocks as it streams (never held whole) at the Cold tier. Once the archive's sha256 checks out, the upload is committed and `raw/{batch}/manifest.json` records the source URL, bytes, sha256, the time and LoC's response headers (`last-modified`, `etag`, `content-length`, `content-type`). Only then is the batch marked curated. A download that fails its checksum leaves nothing. The commit is create-only: an archive already at the path (another environment's, sharing the account) is never replaced, and the batch curates from its own download with a warning if the two differ.
- **A batch already kept is curated from the copy,** not LoC, when its manifest's sha256 is the one LoC lists (or LoC lists none). It needs no download slot, and the copy is checked again as it's read. The `archive source` line says which (`source: raw` or `loc`), and `archive retained` logs each new copy.
- **Batch lists are kept too:** each remote list an `enqueue` or `run` reads goes to `raw/listings/{time}-{sha}.json`.
- **So are the title records.** Titles-sync keeps the fields it uses from each title record in the environment's `reference/raw/titles.json`, and geocode builds the catalog (`reference/catalog/titles.json`, `places.json`) from them. After each titles-sync (on its own or in `run`), the ingest job saves all three as one create-only copy, `raw/titles/{time}.json` (UTC, as `listings/`), unless the newest copy there holds the same. If another run took the same second's name with different content, this copy fails (and the next titles-sync saves its own). The catalog goes with the records because titles-sync skips titles the catalog already has, and geocode keeps the catalog's ordinals and place ids. A copy that fails is a warning (`the title records weren't copied to the raw store`), not a failed sync, and the next titles-sync tries again; `usnm-ingest titles-archive` makes one by hand. A new environment starts from the newest copy ([Dev environment lifecycle](#dev-environment-lifecycle)). This uses the ingest identity's existing writer role on `raw` (list, read, create); nothing new is granted.

Nothing else is downloaded from outside Azure: the search cluster's sample, sets and bench read only Azure stores.

To keep the archives of batches curated before, queue them again with `--force` (only curated or failed batches at the listed version; one being curated is left alone), then run the backfill job. A re-curation writes a new attempt and replaces the batch's curation when it commits.

Stopping a curate run mid-batch leaves its batches leased for up to 2 hours (the curate worker's lease, set in `crates/usnm-ingest/src/main.rs`; the 45-minute `BATCH_LIMIT` is a running batch's own limit): `--force` leaves a leased batch alone and a new run skips it until the lease runs out. A batch whose runs are stopped repeatedly can reach the attempt cap (5) and be marked failed at its next claim; `enqueue --force` resets its attempts. Found on the dev 1% run (October 2026): after a stopped run, 8 batches needed two more passes.

```sh
scripts/start-job.sh --env dev --job INGEST_JOB -- enqueue --batches "$B" --force
scripts/start-job.sh --env dev --job BACKFILL_JOB -- curate --max-runtime-secs 14400
```

**An environment's own `raw` container** (`USNM_RETAIN_RAW true`) does the same inside the environment's data account. It goes with the environment, and turning the setting off deletes it, so it suits a short trial. `USNM_ARCHIVE_ACCOUNT` takes precedence when both are set. To move archives kept there into the archival account, with both settings on, run the copy in the ingest job. It streams each archive, checks it against its manifest, and skips those already there. A batch for which the destination holds another archive is left as it is and fails the copy at the end:

```sh
scripts/start-job.sh --env dev --job INGEST_JOB -- archive-copy \
  --from "$(az storage account show -n "$(scripts/settings.sh --env dev --key STORAGE_ACCOUNT)" --query primaryEndpoints.blob -o tsv)raw" \
  --batches "$B"
```

Then clear `USNM_RETAIN_RAW` and provision. That deletes the environment's own container, so check the copy's `archives copied` line first.

**Cost** (East US 2 list prices, October 2026): Cold storage is $0.0036 per GB-month, so the 1% set's 23.8 GB is about $0.09 a month. Reading it all back once costs about $0.71 ($0.03 per GB retrieved), and writing it about $0.05 (about 2,900 blocks at $0.18 per 10,000 writes). Cold has a 90-day early-deletion minimum. Each environment's endpoint is about $7.30 a month ($0.01 an hour).

### Sample sets

A set packages a sample for reuse in `sets/{name}/`, built once by the search cluster's bench job and never replaced (build `-v2` instead). A build first claims the name with `building.json`, so two builds never write the same objects. A build that fails keeps its name, so the retry is the next version:

- `raw.tar`: the set's LoC batch archives as kept in `raw/`, with their manifests. It's a plain tar, since the archives are bzip2 already. Curate it offline, or extract it into another raw store.
- `docs.ndjson.zst`: the sample's documents exactly as the release builds them (`text`, `text_cg`, every field; `text_as` and `text_as_cg` only when the manifest says `american_stories`). It's one zstd stream, so it can be loaded into any Quickwit 0.9 index created from `infra/quickwit/pages-index.yaml`.
- `manifest.json`, written last: name and version, how the batches were chosen, the batch list with each archive's bytes and sha256, page and document counts, each file's bytes and sha256, the corpus bounds, the common-word pairs' and American Stories' versions, the index config's sha256, the builder's commit and `created_at`.

```sh
J() { scripts/start-job.sh --env dev --job SEARCH_CLUSTER_JOB -- "$@"; }
J bundle --name loc-1pct-v1 --sample dev1pct \
  --selection "every 100th batch of LoC's listing sorted by name, from offset 39 (30 batches)"
J load --set loc-1pct-v1 --index s1ixset          # the cluster loads straight from the set
```

**From a container or a laptop with access** (Storage Blob Data Reader on `sets`, and a network path to the account: an environment's VNet, or a private endpoint of your own; public access is off):

```sh
az storage blob download --auth-mode login --account-name <archive account> -c sets -n loc-1pct-v1/docs.ndjson.zst -f docs.ndjson.zst
zstd -dc docs.ndjson.zst | split -C 8m - chunk-        # request bodies under Quickwit's 10 MiB limit
for f in chunk-*; do curl -sf -XPOST "$QW/api/v1/$INDEX/ingest" --data-binary @"$f"; done
```

Check each file against the manifest's sha256 (`shasum -a 256`). `tar -xf raw.tar` gives `{batch}/{archive}` and `{batch}/manifest.json`, the layout of `raw/`.

## Dev environment lifecycle

**The rule** (#275): `dev` (the stack `usnm-dev`, the group `usnm-core-rg-dev`) holds no resources unless a focused experiment is running. Stand it up for the experiment, and tear it down completely once the results are saved. Scaling to zero isn't enough: with no replicas running it still costs about $2 a day, about $60 a month. That's the 4 private endpoints ($0.24 a day each), the Container Apps environment's load balancer (about $0.45 to $0.60 a day) and public IP ($0.12), the registry (ACR Basic, about $0.17), the NFS share (64 GiB, about $0.21), 3 private DNS zones (about $0.05) and storage (Cost Management, October 2026).

**The marker.** Provisioning `dev` needs `USNM_EXPERIMENT`, the experiment's issue number. It tags dev's project group, `usnm-core-rg-dev`, with `experiment` and `started` (UTC), which stays the same while the experiment does. `scripts/dev-status.sh` lists the stack, what's in `usnm-core-rg-dev`, `usnm-core-infra-rg-dev` (the Container Apps environment's own) and `rg-usnm-dev-spot` (and the legacy `rg-usnm-dev` and `ME_cae-usnm-dev_*`, ADR-0017), the tag and its age, and what those groups have cost since the experiment started. It exits 1 if anything is there without the tag, and warns when the tag is older than 7 days. The guard-rail stack, which stays up between experiments, holds a budget on dev's groups (in both name forms), `budget-usnm-dev-groups`, that mails the alert recipients at $5 of actual spend in a month. It needs no `dev`, only an environment with `USNM_ALERT_EMAILS` provisioned since the budget was added. The legacy Container Apps group's name (`ME_cae-usnm-dev_rg-usnm-dev_<region>`) ends in dev's region: each provision of `dev` records its `AZURE_LOCATION` in the budget, and other environments' provisions keep it.

**Stand it up:**

```sh
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER --value true   # the experiment's own settings, if any, first
scripts/stand-up.sh --env dev --experiment 240 [--archive]
```

The experiment's public settings, and the ones `stand-up.sh` and `teardown.sh` write themselves (`USNM_USE_ACR`, `USNM_COSMOS_FREE_TIER`, `USNM_SEARCH_CLUSTER`), land in `infra/env/dev.json` in your checkout ([Settings](#settings)): commit the experiment's in its pull request. Both scripts provision without the uncommitted-changes check; on its own, use `scripts/provision.sh --env dev --allow-dirty`.

`scripts/stand-up.sh` starts from the settings the last teardown kept (`.env.deleted-<time>`, beside the settings file), or new ones. It provisions `dev` without images (registry, data, network, identities: about 7 minutes), copies the pinned Quickwit image into the registry, builds the checkout's API and ingest images there with `az acr build` (tag `exp-<commit>`; the Japanese OCR image too with `USNM_JA_OCR_JOB`), sets `USNM_IMAGE_TAG` and `USNM_USE_ACR`, and provisions again. `--archive` sets `USNM_ARCHIVE_ACCOUNT` from the `usnm-archive` stack. With the archival account and the ingest job on (`USNM_INGEST_JOBS`), it then seeds the title records and catalog from the account's newest `raw/titles/{time}.json` (`scripts/titles-seed.sh`, below). It never creates a GitHub Environment or adds `dev` to `USNM_DEPLOY_ENVIRONMENTS`, so no push to main rolls it: an experiment runs a branch's images. It refuses `prod`, and a checkout with uncommitted changes. To run a newer commit, run it again. `scripts/bootstrap.sh --env dev` would also work, but it wires `dev` to deploy from main.

**Data**, depending on the experiment:

- **Search experiments:** load a packaged set (`sets/loc-1pct-v1/docs.ndjson.zst`, [Sample sets](#sample-sets)) straight into Quickwit. No curation, no titles-sync, no LoC traffic.
- **Curation or release experiments:** enqueue the set's batches. With `USNM_ARCHIVE_ACCOUNT` set, curation reads them from `raw/` without LoC's download pacing. The 1% (30 batches, 23.8 GB) is roughly 2 worker-hours of bzip2, so under an hour on the backfill job's 4 workers. A release also needs the title catalog. Titles-sync is paced against LoC and can take up to 8 hours, and its output (`reference/raw/titles.json` and the catalog) is in dev's own data account, so it goes at teardown. With the archival account, each titles-sync also saves a copy to `raw/titles/{time}.json` ([Archival storage](#archival-storage)), and the next stand-up starts from the newest one, so the release's titles-sync fetches only titles the copy lacks.

**Seeding the titles.** `scripts/titles-seed.sh --env dev` (which `stand-up.sh` runs) starts one ingest job execution of `usnm-ingest titles-seed` and waits for it (`--wait-mins`, default 20). The job reads the newest copy through dev's endpoint with the ingest identity, checks that the pipeline can read it (every record, and the catalog as geocode loads it: both files or neither, unique ids and ordinals, every title's place listed; a copy that fails writes nothing), and writes the catalog files, then `raw/titles.json` last, each create-only. It writes only into an environment with no record cache: a cache dev already has, newer or older than the copy, is never replaced, and a seed stopped partway finishes when run again. A catalog file already there is kept only if it is the copy's; one that differs stops the seed before the cache is written, so dev never gets one copy's records with another catalog. It logs the copy it read (path, time, records, titles, places) and each file it wrote or kept:

```sh
scripts/titles-seed.sh --env dev                       # again, or after turning the ingest job on
scripts/logs.sh --env dev --query-name titles-archive --span 1h
```

**Cost of standing up:** two provisions and two or three `az acr build` runs, a few cents. Reading the 1% set back from Cold is about $0.71 ($0.03 per GB retrieved) plus about $0.24 of private endpoint processing. Then the idle floor (about $2 a day) and whatever the experiment runs. **Time:** about 40 to 60 minutes to a running environment with images. Data takes minutes for a set, about an hour to curate the 1% from the archive, and a few minutes to seed the titles from the archive (up to 8 hours for a titles-sync when there is no copy yet, or the catalog needs titles the copy lacks).

**Tear it down** once the results are saved: reports copied out of `qw-bench/runs/`, logs queried, and, if titles-sync ran with the archival account, its copy in the archive. Save that copy first, and don't start the teardown until the command below exits 0:

```sh
scripts/start-job.sh --env dev --job INGEST_JOB --wait-mins 20 -- titles-archive
```

Without `--wait-mins`, `start-job.sh` returns as soon as Azure accepts the execution, before anything is written, and a teardown started then would delete the record cache before the copy is made. With it, the script waits for the execution and exits 0 only once it has `Succeeded`, so the copy is in `raw/titles/` (or the newest copy there already held the same records, and nothing was written). It exits 1 if the execution ends `Failed`, `Stopped` or `Degraded` or is still running after 20 minutes: then don't tear down. Check the job's log (`scripts/logs.sh --env dev --query-name job-executions`; without `USNM_ARCHIVE_ACCOUNT` the job has no raw store, and the command fails with `titles-archive needs USNM_RAW_URL`), fix it and run it again. The command is create-only and skips a copy the archive already has, so running it when titles-sync's own copy succeeded costs one short execution. Then:

```sh
scripts/teardown.sh --env dev
scripts/dev-status.sh                                  # expect: dev holds nothing
```

With `USNM_ARCHIVE_ACCOUNT` set, `teardown.sh` first detaches `dev` from the archival account. It lifts the account's delete lock, clears `USNM_ARCHIVE_ACCOUNT` and `USNM_SEARCH_CLUSTER` (the settings that grant roles there) and provisions, so the stack deletes its endpoint and roles itself. It then checks that no role assignment in the account's group belongs to one of dev's identities and that no endpoint connection from dev's groups remains, and puts the lock back (`scripts/archive-store.sh deploy`) whatever happened. The account and its data are never changed. Run it again if it stops partway: it picks up where it stopped. Then it removes `dev` from `USNM_DEPLOY_ENVIRONMENTS` if it's there, waiting for any deploy (`ci` on main) that could still roll it; detaches the stack and deletes the groups it manages, `usnm-core-rg-dev` and `rg-usnm-dev-spot` (the Container Apps environment's `usnm-core-infra-rg-dev` goes with the environment), the custom roles and any role assignment the stack made outside them; keeps `usnm-guardrails` while another environment (or `usnm-archive`) uses it; deletes the GitHub Environment `dev` if there is one; and keeps the settings file as `.env.deleted-<time>`, without the experiment marker. `az deployment group delete -g rg-usnm-archive -n archive-grant-dev` removes the stale deployment record, if wanted.

Log Analytics keeps the workspace soft-deleted for 14 days at no cost, and a stand-up within that window recovers it. Storage account names are reusable straight away, and the resource names come back the same, since the suffix is `uniqueString(subscription, env)`. Dev's data goes with it: Cosmos batch state, curated parts, indexes and `qw-bench`. The archive keeps what was downloaded from LoC. With `USNM_OCR` on, the Document Intelligence account would need a purge before it could be recreated; it's off in dev.

**Cost of tearing down:** nothing billed. **Time:** about 25 to 40 minutes, mostly the group delete, plus about 10 minutes for the archive's provision.

**Settings.** Keep dev's settings in one place, `~/.config/usnewsmap/dev/.env` ([infra/README.md](../infra/README.md#settings-and-the-deployment-stack)), rather than in each worktree's `.azure/dev/.env`, where a stale copy can provision dev with old settings.

## Search cluster experiment

An experimental Quickwit cluster on Container Apps (#238, #239), measured on a 1% sample: indexing with 1 and 2 indexers, then searching with 1, 2 and 3 searchers. It runs in its own environment (`dev`), never in prod (the template ignores the setting there), and everything it adds sits behind `USNM_SEARCH_CLUSTER`. With the setting off, the stack is exactly what it was (the module is `if (searchCluster && useAcr && env != 'prod')`).

**What it deploys** (`infra/modules/searchcluster.bicep`):

- **Nodes** `usnm-qw-{i}-ca-{env}`, `USNM_SEARCH_CLUSTER_NODES` of them (0 to 4), each exactly one replica of `USNM_SEARCH_CLUSTER_NODE_VCPU` vCPU (default 2, the API sidecar's size) with 2 GiB per vCPU, on the Consumption profile. Node 0 is the only metastore (the single writer of the file-backed metastore in `qw-cluster`), the control plane, the janitor, an indexer, a searcher, and the root: searches enter there through its internal ingress, `http://usnm-qw-0-ca-{env}` inside the environment. Nodes 1 to `USNM_SEARCH_CLUSTER_INDEXERS - 1` also index; the rest only search. Their node config is `infra/quickwit/cluster-node.yaml`; its searcher settings are the sidecar's (`searcher.yaml`), which a test checks.
- **Addresses.** Quickwit gossips over UDP and calls leaves over gRPC, both on replica IPs, which change whenever a replica restarts (#239). Each node starts as `usnm-qwcluster node`. That writes its replica IP to `qw-bench/seeds/qw-{i}.json`, reads the other nodes' entries as `QW_PEER_SEEDS`, sets `QW_ADVERTISE_ADDRESS`, and then becomes Quickwit. Every node writes before it reads, so of two nodes starting together at least one sees the other. A node that restarts at a new IP joins through the others, and they learn its new address by gossip.
- **Bench job** `usnm-qw-bench-caj-{env}` (manual, 4 vCPU / 8 GiB, 6 h timeout), which runs `usnm-qwcluster` with each step's arguments (`scripts/start-job.sh`).
- **Storage, Entra only.** Containers `qw-cluster` (splits and metastore; Blob Data Contributor for each node's system-assigned identity, since Quickwit can use no other) and `qw-bench` (seeds, the sample, reports; Contributor for `usnm-qw-node-id-{env}` and `usnm-qw-bench-id-{env}`). The bench identity also reads `curated` and `reference`. Nothing can write `qw-index`, `reference` or `curated`. Turning the setting off deletes both containers (soft delete keeps them 14 days).

**The steps.** Each `usnm-qwcluster` step logs one `qwcluster report` line and stores the full report in `qw-bench/runs/`.

| Step      | Command                                                       | What it measures                                                                                                                                                                                                                                                                                                               |
| --------- | ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `sample`  | `sample --name N --pct P [--american-stories]`                | Takes `P`% of the published version's pages (SHA-256 of `doc_id`, mod 10,000, below `P` × 100, as the OCR audit samples) as the documents a full release builds, with duplicate copies settled as the release does. Reads every curated part once.                                                                             |
| `load`    | `load --sample N --index I [--split-docs 3000] [--senders 4]` | Creates `I` from `pages-index.yaml` with `split_num_docs_target` set and one ingest shard per indexer, sends the sample, and times sending, all documents committed, merges settled, and sealed. Also records documents by node, splits and footer sizes, and retries on 429 and 503.                                          |
| `bench`   | `bench --label L --index I --sample N --expect-searchers K`   | The 13 benchmark searches of `bench-cold-searches.py` and `load-cold-searches.py`, run as `/v1/aggregate` runs them (`plan::aggregate`) against node 0: `first`, `warm`, then concurrency 1, 2, 4 and 10, each with its windows a day further back. Reports median, p90, throughput, and each node's leaf searches and splits. |
| `members` | `members`                                                     | The cluster as node 0 sees it: members, addresses, generations, services.                                                                                                                                                                                                                                                      |
| `dump`    | `dump [--hold-secs 600]`                                      | Prints every stored report, for a log stream when Log Analytics is over its cap.                                                                                                                                                                                                                                               |

**Split target.** At the production 30,000 pages, a 1% sample makes about 8 splits, too few for a root to spread over three nodes the way production spreads about 800. The loader's default of 3,000 makes about 80 splits of a tenth the size. Per-split fixed costs then weigh more than in production, so a second index at 30,000 (`--split-docs 30000`) brackets the result.

**Run it** (in `dev`; prod's data account is private, so dev curates its own 1% from LoC):

```sh
scripts/archive-store.sh deploy                       # once per subscription; prints the account's id
ARCHIVE_ID=<the id it printed>
scripts/settings.sh --env dev --key USNM_INGEST_JOBS --value true
scripts/settings.sh --env dev --key USNM_INGEST_SCRATCH_GIB --value 0            # 1% fits the replica's disk
scripts/settings.sh --env dev --key USNM_LOG_DAILY_CAP_GB --value 1              # blob write logs; clear afterwards
scripts/settings.sh --env dev --key USNM_ARCHIVE_ACCOUNT --value "$ARCHIVE_ID"   # keep every LoC download (Archival storage, above; private: the settings store)
scripts/provision.sh --env dev --allow-dirty    # the experiment's settings, before they're committed
RG=$(scripts/settings.sh --env dev --key AZURE_RESOURCE_GROUP); ACR=$(scripts/settings.sh --env dev --key ACR_NAME)
TAG=qwc-$(git rev-parse --short HEAD)
az acr build -r "$ACR" -t usnewsmap-ingest:$TAG -f Dockerfile.ingest --build-arg USNM_GIT_SHA=$(git rev-parse HEAD) .
az acr build -r "$ACR" -t usnewsmap-api:$TAG -f Dockerfile .
scripts/settings.sh --env dev --key USNM_IMAGE_TAG --value $TAG
scripts/settings.sh --env dev --key USNM_USE_ACR --value true
scripts/provision.sh --env dev --allow-dirty

# The 1%: every 100th batch of LoC's listing by name, from the 40th (offset 39):
# 30 batches, 238,057 pages of 23,838,683 (0.999%), 26 awardees, 23.8 GB of archives.
B=arhi_beatles_ver01,az_fireant_ver01,ct_fairfield_ver01,curiv_plasse_ver01,dlc_alpha_ver03,dlc_debaptiste_ver01,dlc_goldenrod_ver01,dlc_leibovitz_ver01,dlc_saluki_ver01,gu_drteeth_ver01,iahi_hypno_ver01,in_irvington_ver01,khi_garwood_ver02,lu_juggernaut_ver01,me_calais_ver03,mnhi_dassel_ver01,mohi_berenice_ver01,mthi_goldeneye_ver01,ncu_cotton_ver04,nhd_lafayette_ver01,nn_keddy_ver01,ohi_himilco_ver01,oru_longspur_ver01,rp_hobgoblin_ver02,tu_eddie_ver02,uuml_anderson_ver01,vi_fezza_ver01,vnstcsc_duggan_ver01,whi_brie_ver01,wvu_jolie_ver02
scripts/start-job.sh --env dev --job INGEST_JOB -- enqueue --batches $B --force   # --force: batches curated before are curated again, now kept
scripts/start-job.sh --env dev --job BACKFILL_JOB -- curate --max-runtime-secs 14400
scripts/start-job.sh --env dev --job INGEST_JOB -- run --batches $B --full --curate-max-runtime-secs 21600 \
  --titles-max-runtime-secs 28800 --quickwit-bin /usr/local/bin/quickwit \
  --quickwit-metastore azure://qw-index --quickwit-index-root azure://qw-index
scripts/logs.sh --env dev --query-name release-progress --span 6h    # the single-writer baseline on the same pages

# One node, one indexer.
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER --value true
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_NODES --value 1
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_INDEXERS --value 1
scripts/provision.sh --env dev --allow-dirty
J() { scripts/start-job.sh --env dev --job SEARCH_CLUSTER_JOB -- "$@"; }
J members
J sample --name dev1pct --pct 100          # dev's whole version is the 1%
J bundle --name loc-1pct-v1 --sample dev1pct \
  --selection "every 100th batch of LoC's listing sorted by name, from offset 39 (30 batches)"
J load --sample dev1pct --index s1ix
J bench --label n1 --index s1ix --sample dev1pct --expect-searchers 1
# Two nodes, both indexing.
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_NODES --value 2
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_INDEXERS --value 2
scripts/provision.sh --env dev --allow-dirty
J load --sample dev1pct --index s2ix
J bench --label n2 --index s1ix --sample dev1pct --expect-searchers 2
# Three nodes; node 2 only searches. Then the restart test.
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_NODES --value 3
scripts/provision.sh --env dev --allow-dirty
J bench --label n3 --index s1ix --sample dev1pct --expect-searchers 3
J members
az containerapp revision restart -n usnm-qw-2-ca-dev -g "$RG" \
  --revision "$(az containerapp revision list -n usnm-qw-2-ca-dev -g "$RG" --query "[?properties.active].name | [0]" -o tsv)"
J bench --label n3-restart --index s1ix --sample dev1pct --expect-searchers 3 --levels 1,4 --levels-only
J members                                  # qw-2 has a new address and generation

# The report: Log Analytics, Azure Monitor per node app, list prices.
scripts/qwcluster-report.py --env dev --since 2d --out qwcluster-report.md
```

Start each step once the previous execution has ended (`az containerapp job execution list -n "$(scripts/settings.sh --env dev --key SEARCH_CLUSTER_JOB)" -g "$RG" -o table`). For a cold `first` pass, restart the node revisions before a bench. The first provision with the cluster may restart node 0 a few times while its identity's role on `qw-cluster` propagates. If Log Analytics is over its cap, stream the stored reports with `J dump --hold-secs 600` and `az containerapp job logs show -n <job> -g "$RG" --container qwbench --follow > lines.txt`, then run `scripts/qwcluster-report.py --env dev --from-file lines.txt`. Collect the report before scaling down: a deleted app's metrics can't be queried.

**Report lines.** Container Apps keeps about 16 KiB of a console line. A bench report therefore goes out as several `qwcluster report` lines (`bench`, one `bench_pass` per pass, `bench_probe`, `bench_profile`), which `scripts/qwcluster-report.py` puts back together by label. Images before this change logged a whole run on one line, and a run with `--probe` passed the limit (October 2026); the report salvages such a line's passes, and takes its Quickwit version from the run's `bench root` line, but its probe and profile are only in `qw-bench` (`runs/{label}/bench.json`). `J dump` with a current image logs those stored reports again, in parts.

**Idle and teardown.** `USNM_SEARCH_CLUSTER_NODES 0` and a provision remove the node apps but keep the containers and the bench job, so nothing is billed but storage. `USNM_SEARCH_CLUSTER ""` and a provision remove everything the setting added, both containers included. Clear `USNM_LOG_DAILY_CAP_GB` afterwards. Once the experiment's results are saved, tear `dev` down ([Dev environment lifecycle](#dev-environment-lifecycle)).

**Threads.** Every node runs one main-runtime thread and one search-pool thread per vCPU (`QW_TOKIO_RUNTIME_NUM_THREADS`, `RAYON_NUM_THREADS`), as the API's sidecar does with 4 and 4 at 3.75 vCPU (#251): 2 and 2 at the default 2 vCPU. Before October 2026 the nodes ran Quickwit's own default, a third of the CPUs rounded up for the main runtime (one thread at 2 vCPU), so the earlier benches had a single thread for downloads and split opening. `USNM_SEARCH_CLUSTER_RUNTIME_THREADS` and `USNM_SEARCH_CLUSTER_SEARCH_THREADS` override the counts (0: one per vCPU).

### Quickwit version comparison

Whether a pre-0.9 Quickwit searches our splits with less work than 0.9.1 (#251; upstream quickwit-oss/quickwit#6883 reports 0.9.1 1.25 to 2x slower than a 0.8 nightly on identical splits). The released 0.8.2 can't take part: it reads tantivy index formats 4 to 6 and 0.9.1 writes 7, and it authenticates to Azure only with an account key, which our accounts refuse (ADR-0009). The baseline is the `qw-azure-fix` nightly (`quickwit/quickwit:qw-azure-fix@sha256:d4f75ebc…`, main of 23 September 2025 plus the Azure multipart fix, which reports 0.8.0): it writes and reads format 7, uses managed identity, reads the 0.9 metastore, and takes every searcher key of `searcher.yaml` but `predicate_cache_capacity` and `leaf_request_timeout_secs`.

**What it deploys.** `USNM_SEARCH_COMPARE_TAGS`, ingest image tags separated by commas, gives one standalone searcher per tag, `usnm-qw-cmp-{i}-ca-{env}` (root `http://usnm-qw-cmp-{i}-ca-{env}` inside the environment), of `USNM_SEARCH_CLUSTER_NODE_VCPU` vCPU each. Each runs as the API sidecar does: searcher and metastore, polling the file-backed metastore in `qw-cluster` every 30 s, with Blob Data Reader on `qw-cluster` only, so it searches the cluster's indexes and can't change them. Each has a cluster id of its own and no seeds, so the two never join and a search never mixes versions. The node wrapper reads `quickwit --version` and, below 0.9, drops the two searcher keys and pins `warmup_single_split_initial_allocation` to 0.9.1's 300 MB (the nightly's default is 1 GB). The bench records the root's Quickwit version, each node's main-runtime busy time and Blob bytes downloaded per pass, and samples the search pool's running and queued split searches every 2 s.

**Images.** The second tag is the ingest image on the nightly's Quickwit (`Dockerfile.ingest`'s `QUICKWIT_IMAGE`; CI always builds the default, v0.9.1):

```sh
cd ~/code/usnewsmap.com-qwcluster
RG=$(scripts/settings.sh --env dev --key AZURE_RESOURCE_GROUP); ACR=$(scripts/settings.sh --env dev --key ACR_NAME)
TAG=qwc-$(git rev-parse --short HEAD); SHA=$(git rev-parse HEAD)
NIGHTLY=quickwit/quickwit:qw-azure-fix@sha256:d4f75ebc6d6ee7db4732b8a20be544ba3455bdc08ef0ccd7683f4aa854243ff6
az acr build -r "$ACR" -t usnewsmap-ingest:$TAG -f Dockerfile.ingest --build-arg USNM_GIT_SHA=$SHA .
az acr build -r "$ACR" -t usnewsmap-ingest:$TAG-qw2509 -f Dockerfile.ingest --build-arg USNM_GIT_SHA=$SHA --build-arg QUICKWIT_IMAGE=$NIGHTLY .
az acr build -r "$ACR" -t usnewsmap-api:$TAG -f Dockerfile .
```

**Smoke test** on an index the cluster already holds (`s1ixb`, the 1% without American Stories, 58 splits): cluster nodes off, so nothing writes while they search; both searchers at the sidecar's 4 vCPU.

```sh
scripts/settings.sh --env dev --key USNM_IMAGE_TAG --value $TAG
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER --value true
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_NODES --value 0
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_NODE_VCPU --value 4
scripts/settings.sh --env dev --key USNM_SEARCH_COMPARE_TAGS --value "$TAG,$TAG-qw2509"
scripts/provision.sh --env dev --allow-dirty
J() { scripts/start-job.sh --env dev --job SEARCH_CLUSTER_JOB -- "$@"; }
JOB=$(scripts/settings.sh --env dev --key SEARCH_CLUSTER_JOB)
wait_for() {  # until execution $1 has ended; prints how
  local s; while :; do
    s=$(az containerapp job execution show -n "$JOB" -g "$RG" --job-execution-name "$1" --query properties.status -o tsv)
    case "$s" in Running|Processing|Unknown|"") sleep 20 ;; *) echo "$1 $s"; return ;; esac
  done
}
restart() { for a in usnm-qw-cmp-0-ca-dev usnm-qw-cmp-1-ca-dev; do az containerapp revision restart -n $a -g "$RG" --revision "$(az containerapp revision list -n $a -g "$RG" --query "[?properties.active].name | [0]" -o tsv)"; done; sleep 90; }
wait_for "$(J members --cluster http://usnm-qw-cmp-0-ca-dev)"; wait_for "$(J members --cluster http://usnm-qw-cmp-1-ca-dev)"
bench() { wait_for "$(J bench --label "$1" --cluster "http://usnm-qw-cmp-$2-ca-dev" --index s1ixb --sample dev1pct --levels 1,4,10)"; }
restart; bench cmp-v091-r1 0; bench cmp-n2509-r1 1
restart; bench cmp-n2509-r2 1; bench cmp-v091-r2 0
restart; bench cmp-v091-r3 0; bench cmp-n2509-r3 1
scripts/qwcluster-report.py --env dev --since 6h --out qwcompare.md
```

The two `members` lines show each searcher alone in its cluster, with `main_threads` 4 in its counters (the nightly reports the same metric names as 0.9.1). The report's **Quickwit version comparison** section has, per pass, the main runtime's busy seconds per search and per GB downloaded, its busy share, and the search pool's peaks, then each version's averages with ratios to 0.9.1, then whether both versions found the same pages for every search. If they didn't, the timings don't compare. On dev's 1% the searches wait on Blob, not CPU (0.1 to 0.4 vCPU in October 2026), so the latency ratios don't carry to production; busy seconds per GB do, since production's cold searches wait on the main runtime (about 95% busy on 4 threads with an idle search pool, #251). A nightly ratio well under 1 (0.8 or less) on busy seconds per GB is worth the full run; near 1, the regression of #6883 isn't what holds production back.

**Full run**, if the smoke test points that way: indexes shaped like production's, built by 0.9.1 (node 0) from the 1% with `text_as` and `text_as_cg` filled from `text` and `text_cg` (`load --as-from-text`; LoC's OCR twice, the shape of production's work but not its matches), at the loader's 3,000 pages per split (production's fan-out per search) and at production's 30,000 (production's split size):

```sh
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_NODES --value 1
scripts/provision.sh --env dev --allow-dirty
wait_for "$(J load --sample dev1pct --index asdup3k --as-from-text)"
wait_for "$(J load --sample dev1pct --index asdup30k --split-docs 30000 --as-from-text)"
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_NODES --value 0
scripts/provision.sh --env dev --allow-dirty
bench() { wait_for "$(J bench --label "$1" --cluster "http://usnm-qw-cmp-$2-ca-dev" --index "$3" --sample dev1pct --american-stories --levels 1,4,10)"; }
# then the same three rounds as above for asdup3k and asdup30k
```

**Afterwards** `USNM_SEARCH_COMPARE_TAGS ""` and a provision remove the searchers (collect the report first). Two 4 vCPU searchers cost about $0.86 an hour while they exist.

### Local-disk test

Whether reading splits from somewhere other than Blob removes the main runtime's CPU cost (#251): the same index and bench on Quickwit 0.9.1 with the sidecar's 4 and 4 threads, one searcher, `usnm-qw-local-ca-dev`, in one mode at a time (`USNM_SEARCH_LOCAL_MODE`):

- `blob`: splits read from Blob, as the sidecar reads them.
- `nfs`: the index (`USNM_SEARCH_LOCAL_INDEX`) copied once onto an NFS Azure Files share and served from there as `file://`. The share, `qw-search` (`USNM_SEARCH_NFS_GIB`, default 64), is a second share in the ingest scratch share's provisioned v2 SSD account, reached through the same private endpoint; it exists while the mode is `nfs` (or the scratch share is on). The copy is made at the first start (`crates/usnm-ingest/src/cluster/local.rs`) and kept: a restart, or another index, copies only what isn't there.
- `cache`: Quickwit's split cache (`searcher.split_cache`, `USNM_SEARCH_LOCAL_CACHE_GIB`, default 40) on the replica's disk. A search reads from Blob and marks its splits; the cache downloads them whole in the background (4 at a time) and later searches read the local files.
- `copy`: the index copied to the replica's disk before Quickwit starts and served as `file://`, so no Blob client runs at all.

**Where it runs.** `blob` and `nfs` run on Consumption at the sidecar's 3.75 vCPU / 7.5 GiB (with `nfs`'s 0.25 vCPU init container that hands the share's directory to the node's user, 4 / 8 in all), the shape production searches with. A Consumption replica can mount an NFS share: the environment has its own VNet, and the ingest job mounts its scratch share on Consumption the same way ([storage mounts](https://learn.microsoft.com/azure/container-apps/storage-mounts#azure-files-volume)). `cache` and `copy` need more disk than a Consumption replica's 8 GiB ([ephemeral storage](https://learn.microsoft.com/azure/container-apps/storage-mounts#ephemeral-storage)), and a VM is out (`usnm-deny-iaas-compute` denies them in dev's groups), so they run on the E4 dedicated profile (`USNM_DEDICATED_PROFILE`), where Microsoft reported about 80 GiB of ephemeral storage per replica in September 2026 (microsoft/azure-container-apps#1779; the node logs `data directory disk` with `df` at start): 3.25 vCPU / 24 GiB, an E4 replica's most, its disk emptied by a restart. The E4 node costs about $0.39 an hour plus the $0.10 Dedicated plan fee while the profile exists; the share about $0.10 per GiB a month while it exists.

**One E4 mode at a time, with a provision in between.** The E4 profile has one node. A new revision there can't start while the old one holds it, and the old one keeps the traffic: on the evening of 9 October 2026 (ET) the `cache` and `copy` runs were all served by the `blob` revision (`AssigningReplicaFailed: Insufficient capacity on workload profile 'ingest-e4'`, then `Deployment Progress Deadline Exceeded`), and repeated the `blob` runs' windows, which Quickwit's partial request cache answered. Their results don't stand. To switch between `cache` and `copy`, clear the mode (`--unset`), provision, then set the new one and provision. The bench's `--variant blob|nfs|cache|copy` now reads the root's node config (`/api/v1/config`) and stops if the searcher isn't in that mode. Give each run its own `--offset` too, so no two runs share windows.

**What the bench adds.** `--probe` sends each search's summary request once more over Quickwit's gRPC API, the only place 0.9.1 reports its per-split resource stats (`SearchResponse.resource_stats`, quickwit #6416: not in the REST response, not in its metrics, in its logs only for memory-hungry queries): warm-up (the main runtime's downloading and decoding), waiting for the search pool, CPU search, waiting for a search permit, and bytes downloaded (`crates/usnm-ingest/src/cluster/grpc.rs`). `--profile-secs` runs searches with fresh windows under Quickwit's own CPU profiler (`/api/developer/pprof`, in its release builds) and stores the flame graph in `qw-bench` at `runs/{label}/flamegraph.svg`; the report lists samples by thread and the main runtime's busiest functions. `--wait-split-cache-secs` waits for the split cache to hold every split before the passes, and `--variant` names the mode in the report.

With `RG`, `J`, `JOB` and `wait_for` as in the version comparison above, and the image tag that has these flags in `USNM_IMAGE_TAG`:

```sh
lbench() { wait_for "$(J bench --label "$1" --cluster http://usnm-qw-local-ca-dev --index "$IX" --sample dev1pct --variant "$2" --levels 1,4,10 --probe --profile-secs 120 "${@:3}")"; }
# Until the searcher's newest revision is the ready one (a first nfs copy takes minutes).
ready() { until [ "$(az containerapp show -n usnm-qw-local-ca-dev -g "$RG" --query "properties.latestReadyRevisionName == properties.latestRevisionName" -o tsv)" = true ]; do sleep 30; done; }
scripts/settings.sh --env dev --key USNM_SEARCH_CLUSTER_NODES --value 0
scripts/settings.sh --env dev --key USNM_SEARCH_COMPARE_TAGS --unset
IX=s1ixb; scripts/settings.sh --env dev --key USNM_SEARCH_LOCAL_INDEX --value $IX
scripts/settings.sh --env dev --key USNM_SEARCH_LOCAL_MODE --value blob; scripts/provision.sh --env dev --allow-dirty; ready
lbench c-blob-r1 blob --offset 200; lbench c-blob-r2 blob --levels-only --offset 210
scripts/settings.sh --env dev --key USNM_SEARCH_LOCAL_MODE --value nfs; scripts/provision.sh --env dev --allow-dirty; ready
lbench c-nfs-r1 nfs --offset 220; lbench c-nfs-r2 nfs --levels-only --offset 230
az containerapp logs show -n usnm-qw-local-ca-dev -g "$RG" --container qwnode --tail 300 | grep -E 'data directory disk|local copy disk|index copied'
scripts/qwcluster-report.py --env dev --since 12h --out qwlocal.md
scripts/settings.sh --env dev --key USNM_SEARCH_LOCAL_MODE --unset; scripts/provision.sh --env dev --allow-dirty   # removes the searcher and the share
```

The report's comparison tables group the runs as `0.9.1 blob` and `0.9.1 nfs` (and `cache`, `copy`) with ratios to blob, and its bench rows name each run's mode. Compare the `r2` runs: if `nfs` cuts the main runtime's busy seconds per search and the probe's warm-up seconds as `copy` was meant to show, the Blob path (HTTP, TLS, the Azure client) is the main runtime's cost and an NFS share is a way around it in production; if it barely moves them, the cost is in decoding what a split needs. The flame graphs show which functions those seconds went to.

## On-prem OCR nodes

Local machines re-read badly OCR'd pages (#296, [ADR-0015](design/adr/0015-onprem-ocr-workers.md)). Each node is an Azure Arc-enabled server running RHEL 10 (AlmaLinux 9 is also accepted), with a managed identity and no stored secret. It takes pages from Storage queues in the exchange account `stusnmo<suffix>` and writes its output to that account's `ocr-out` container, which a cloud job copies into `curated`. This section sets up the account, connects and monitors a node, and starts the worker ([`crates/usnm-ocr-worker`](../crates/usnm-ocr-worker/README.md)).

**The exchange account.** Set the node IPs (the home connection's public IPv4 address; a comma-separated list or CIDR ranges also work) and turn it on. The IP is a private setting, in the settings store, never in the repository ([Settings](#settings)). The provision also adds a Microsoft.Storage service endpoint to `snet-cae`, which closes open connections from the subnet to storage public endpoints (the data lake is reached through its private endpoint and isn't affected), so run it while no ingest job is running.

```sh
scripts/settings.sh --env prod --key USNM_OCR_NODE_IPS --value 203.0.113.7   # the home IP, private: the settings store
scripts/settings.sh --env prod --key USNM_OCR_NODES --value true   # committed: pull request, merge, then provision from main
scripts/ocr-node/group.sh --env prod --dry-run
scripts/ocr-node/group.sh --env prod   # writes USNM_OCR_NODE_GROUP to the settings store (the local file until the store exists)
scripts/provision.sh --env prod        # writes OCR_EXCHANGE_ACCOUNT, OCR_NODE_DCR_ID and OCR_NODE_APPINSIGHTS, grants the group
```

**The nodes' Key Vault.** With `USNM_OCR_NODES` on, the provision also creates `usnmocrkv<env><suffix>` (saved as `OCR_KEYVAULT`) for the secrets you hold for setting up nodes: `rhel-activation-key` and `rhel-org-id`, the Red Hat activation key and organization id a RHEL node registers with, and `rhel-offline-token`, a Red Hat API offline token with which `scripts/ocr-node/rhel-image.sh` fetches RHEL images onto the Proxmox hosts through Red Hat's image API (below). Its firewall allows only `USNM_OCR_NODE_IPS`, so run `az` against it from home. Access is Azure RBAC only, and only the object ids in `USNM_OCR_KEYVAULT_ADMINS` get Key Vault Secrets Officer (nodes get nothing). Prod's vault has purge protection: a deleted secret or vault is kept for 90 days and can't be purged. Never paste the secrets into a chat, an issue or a commit, and keep them out of your shell history. The leading space on the `secret set` lines below keeps zsh and bash from saving them when `HIST_IGNORE_SPACE` or `HISTCONTROL=ignorespace` is set; or use `--file` with a file you delete straight after. Grant yourself access, provision, then set the secrets in your own terminal:

```sh
scripts/settings.sh --env prod --key USNM_OCR_KEYVAULT_ADMINS \
  --value "$(az ad signed-in-user show --query id -o tsv)"   # private: the settings store
scripts/provision.sh --env prod                              # writes OCR_KEYVAULT
vault=$(scripts/settings.sh --env prod --key OCR_KEYVAULT)   # usnmocrkvprodffapzy
 az keyvault secret set --vault-name "$vault" --name rhel-activation-key --value '<activation key>' -o none
 az keyvault secret set --vault-name "$vault" --name rhel-org-id --value '<organization id>' -o none
 az keyvault secret set --vault-name "$vault" --name rhel-offline-token --value '<offline token>' -o none
az keyvault secret list --vault-name "$vault" --query '[].name' -o tsv   # names only, never values
```

A new role assignment can take a few minutes to apply; a `Forbidden` right after the provision clears on its own. The vault's audit log (`AuditEvent`, every read, write and refused request) goes to `log-usnm-<env>`. Turning `USNM_OCR_NODES` off deletes the vault with the rest; prod's then stays soft-deleted for 90 days, holding its name, (7 days outside prod). `provision.sh` recovers it, with its secrets, before deploying when the nodes are turned back on within that time; it only recovers a vault deleted from this environment's resource group. Elsewhere a deleted vault is kept 7 days and `teardown.sh` purges it.

**The nodes' group.** The nodes get their roles through an Entra security group, `usnm-ocr-nodes-<env>`: the provision grants the group Message Processor on `ocr-priority` and `ocr-bulk`, Message Sender on `ocr-deadletter`, a custom writer role on `ocr-out` (list, read and create blobs, no delete) and Monitoring Metrics Publisher on the nodes' own Application Insights component, `usnm-ocr-appi-<env>` (the worker's telemetry), and nothing else; nothing on the environment's `appi-usnm-<env>`. Each node's Arc identity is a member. `group.sh` creates the group once per environment, saves its object id in `USNM_OCR_NODE_GROUP` and makes you an owner so you can add and remove members. Run again, it uses the saved id and lists the group's owners. It never adopts a group by name: if one named `usnm-ocr-nodes-<env>` exists and no id is saved, it stops, so you can check that group's members and owners (`az ad group member list --group <id>`, `az ad group owner list --group <id>`) before saving its id by hand. The provision grants every member the node roles, and owners can add members. Creating it needs the right to create security groups (the default user setting, or Groups Administrator). The deployment stack can't create it (ADR-0015), and teardown leaves it: delete it by its object id, the value of `USNM_OCR_NODE_GROUP` (`az ad group delete --group <id>`; teardown keeps the settings file, renamed), not by its name.

**Create the VM.** On a Proxmox host, `scripts/ocr-node/proxmox-create-vm.sh` (copy it to the host, run it as root) creates a VM from a qcow2 image already on the host, with `--image-file` and `--image-sha256`, such as Red Hat's RHEL 10 KVM Guest Image, or, without them, an AlmaLinux 9 VM from AlmaLinux's GenericCloud image, checking its published sha256. `--mac` gives the VM a fixed MAC address, so a DHCP reservation (a fixed IP on the router) still applies after a rebuild. The VM boots UEFI with Secure Boot (Microsoft's keys pre-enrolled; AlmaLinux's and Red Hat's shims are signed) and has a vTPM, CPU type `host` (so the OCR engines can use AVX2), the QEMU guest agent, DHCP and start at boot. cloud-init creates an admin user `claude` with key-only SSH and passwordless sudo. On the first try cloud-init left the hostname as `localhost`; the script now sets `fqdn` and runs `hostnamectl set-hostname`. The first DHCP request still goes out as `localhost`, so the gateway's local DNS learned the name only at the next renewal; cloud-init now brings the wired connection up again after setting the hostname, which sends it at once. The script prints the VM's address once the guest agent reports one.

`scripts/ocr-node/rhel-image.sh` puts the RHEL image on the host (#339). From your machine, it reads `rhel-offline-token` from the nodes' Key Vault with your sign-in and sends it to the host on ssh's stdin. On the host it exchanges the token at Red Hat's SSO for an access token, asks the image API for the download link of the image with that SHA-256 (Red Hat's download page lists it), downloads the image and keeps it only if the SHA-256 matches. The tokens and the signed link go to curl on its stdin, so none of them is on a command line or on the host's disk. An image already there with the right SHA-256 is left alone. Red Hat lets an offline token lapse after 30 days unused.

A rebuilt node gets a new name with the next number (`usnm-ocr-node3-arcs-prod` replaced node 2, `usnm-ocr-node4-arcs-prod` node 1), never a reused one. The name is also the worker's node id, which the ledger, `ocr-sync --report`'s pages by node and revocations by `node` all key on, so a reused name would mix two machines' output. The VMID is the host's choice; the nodes use 203 on pve-big and 124 on the NUC.

```sh
# from your machine: the RHEL 10.2 KVM Guest Image onto the host
scripts/ocr-node/rhel-image.sh --env prod --host root@pve.tjg.fyi --file-name rhel-10.2-x86_64-kvm.qcow2 \
  --image-sha256 caaacd0b11bbc4206f8c7fec8594b82b4366b3f88ff974dbbf373832ae676f4f [--dest /var/lib/vz/template/cache] [--dry-run]
# on the host, as root
bash proxmox-create-vm.sh --vmid 124 --name usnm-ocr-node4-arcs-prod --ssh-key "$(cat ~/.ssh/id_ed25519.pub)" \
  --cores 4 --memory-mib 8192 --disk-gib 64 \
  --image-file /var/lib/vz/template/cache/rhel-10.2-x86_64-kvm.qcow2 \
  --image-sha256 caaacd0b11bbc4206f8c7fec8594b82b4366b3f88ff974dbbf373832ae676f4f
# AlmaLinux 9 instead: leave out --image-file and --image-sha256
# optional: --storage local-lvm --bridge vmbr0 --snippets-storage local --mac 52:54:00:12:34:56
```

A RHEL image has no package repositories until it's registered with Red Hat, which `onboard.sh` does, so with `--image-file` cloud-init installs nothing: it creates the user, sets the hostname and starts the guest agent if the image has it (Red Hat's KVM Guest Image ships it). If the image has no guest agent, the script can't print the address; it prints the VM's MAC instead, to find in the router's DHCP leases, and `node-setup.sh` installs the agent after registration.

**The Proxmox host's network card.** The NUC's Intel e1000e NIC logs `Detected Hardware Unit Hang` and drops off the network under load. Turning off its segmentation and receive offloads fixes it; make it permanent with a `post-up` line on the interface in `/etc/network/interfaces` (the NIC is `nic0` on this host):

```text
iface nic0 inet manual
        post-up /sbin/ethtool -K nic0 tso off gso off gro off
```

**Connect a node.** The node needs AlmaLinux 9 or RHEL 10 and an admin account with key-based SSH and passwordless sudo. Its name (`--name`, also the worker's `--node-id`) is 1 to 32 letters, digits or hyphens. Run it from your machine, signed in to `az` as an account with the right to create app registrations in the tenant (the Application Developer role, or the default user setting that lets users register applications), to assign roles on the environment's resource group and to write its Arc machines, their extensions and their data collection rule associations (Owner; or User Access Administrator together with Contributor, or with Azure Connected Machine Resource Administrator and Monitoring Contributor: turning on automatic agent upgrade writes the machine, `Microsoft.HybridCompute/machines/write`, and the monitoring agent is an extension with a data collection rule association) and to add members to the nodes' group (its owner, which `group.sh` made you). The first onboarding in a subscription also registers three resource providers, a subscription-level action (Contributor or Owner on the subscription); with only resource-group-level rights, have them registered first:

```sh
scripts/ocr-node/onboard.sh --env prod --host claude@node1.example.org --name usnm-ocr-node1 --dry-run
scripts/ocr-node/onboard.sh --env prod --host claude@node1.example.org --name usnm-ocr-node1
```

`onboard.sh` registers the `Microsoft.HybridCompute`, `Microsoft.GuestConfiguration` and `Microsoft.HybridConnectivity` providers. Unless the node is already connected, it then creates a throwaway app registration and service principal, `usnm-arc-onboard-<name>-<UTC time>`, with a client secret valid for 2 hours and only Azure Connected Machine Onboarding on the environment's resource group, and waits for the role assignment to show up. Then it runs `scripts/ocr-node/node-setup.sh` on the node with sudo. On a RHEL node that isn't registered with Red Hat, `onboard.sh` first reads `rhel-org-id` and `rhel-activation-key` from the nodes' Key Vault (`OCR_KEYVAULT`) with your sign-in, so you need Key Vault Secrets Officer (`USNM_OCR_KEYVAULT_ADMINS`) and to run it from home (the vault's firewall allows `USNM_OCR_NODE_IPS` only), and sends them with the onboarding credential on `node-setup.sh`'s stdin; they're never written to disk on your machine, the node or the Proxmox host. `node-setup.sh` registers first (`subscription-manager register --org ... --activationkey ...`, which takes them as arguments, so they're in the process list for the few seconds it runs, before the `usnm` user exists; a registered node is skipped and needs no secrets) and enables BaseOS, AppStream and CodeReady Builder, since nothing can be installed before that. That script installs python3, podman, chrony, cron (for the agent's automatic upgrades), rsyslog and which, and initscripts where the distribution has it (the monitoring agent's prerequisites), the QEMU guest agent on a VM if the image lacks it, plus htop, iftop and screen from EPEL for watching the node (no OCR engine: EL9 has only Tesseract 4.1.1 and EPEL 9 none, so the engines come as pinned container images; `install-worker.sh` loads the worker's); creates the `usnm` user; sets up the engine runtime of [ADR-0016](design/adr/0016-pluggable-ocr-engines.md) ([13 §13.3](design/13-ocr-engines.md#133-component-view)): a second system user, `usnmeng`, with its own subordinate ids, lingering and the cpu, memory and pids cgroup controllers delegated to its user manager, not in `himds`; its Podman API socket at `/run/usnm-ocr/podman.sock` (mode 0660, group `usnmocr`, which `usnm` shares), through which the worker runs engine containers with `podman --remote`; and the engines' scratch, a tmpfs at `/run/usnm-ocr/scratch` capped at `--scratch-size` (default 2G) and owned by `usnm`, so an engine's files count against its own memory limit; installs the Connected Machine agent; sets its extension allow list to the Azure Monitor Linux Agent alone (with extensions off while a different list is replaced), reads it back, stops if any other extension is installed in `/var/lib/waagent` (the allow list doesn't remove one installed before it, and turning the extension manager on would run it again), and only then turns the extension manager on (an empty list allows every extension), and turns off guest configuration and incoming connections; keeps the journal on disk (`/etc/systemd/journald.conf.d/50-usnm.conf`: `Storage=persistent`, `SystemMaxUse=1G`; the cloud image keeps it in memory only, so a reboot lost the logs); sets sshd to offer post-quantum key exchange first where its OpenSSH supports it (a drop-in, `/etc/ssh/sshd_config.d/40-pq-kex.conf`; AlmaLinux 9.8's default crypto policy leaves it out); connects with that service principal (the tenant, app id and secret go over ssh's stdin, never on a command line; the secret waits in a root-only file under `/run` while `azcmagent connect --config` reads it, and a refused connect is retried for a few minutes while the new principal propagates); writes the worker's settings to `/etc/usnm/ocr-worker.env` unless `install-worker.sh` has written it (the exchange account's queue and `ocr-out` URLs, the node id, one page in flight per CPU and the connection string of the nodes' Application Insights component, which `onboard.sh` passes from `OCR_NODE_APPINSIGHTS`; root:usnm, mode 0640, nothing secret: with local auth off, ingestion needs an Entra token too); and installs `usnm-ocr-worker.service` (copied from `scripts/ocr-node/`) disabled, which starts only once `/usr/local/bin/usnm-ocr-worker` exists. It ends by checking that `usnm` can get a token (for Resource Manager, not storage: see below), and prints `azcmagent show` and `azcmagent config list`. Run again, it skips what's done and applies the lockdown, the settings and the unit as they are now (restarting a running worker if they changed). `onboard.sh` then waits for the machine's system-assigned identity and adds it to the nodes' group unless it's already a member, which gives the node its roles; no provision is needed. The machine identity's private key can live in the node's TPM (`--identity-key-store tpm`, the agent's TPM-backed identity, in preview; `proxmox-create-vm.sh` gives VMs a vTPM 2.0), so it can't be copied off the disk. Azure accepts that only once the subscription's `Microsoft.HybridCompute/HybridComputePreview` feature is Registered, which needs Microsoft's approval: `az feature register --namespace Microsoft.HybridCompute --name HybridComputePreview` leaves it Pending until then (requested 2026-10-10). `onboard.sh` checks the feature and asks `node-setup.sh` for the TPM only when it's Registered; otherwise the key stays in the agent's default store on disk. The key store is fixed at connect: moving an existing node to the TPM means `offboard.sh` and `onboard.sh` again, which makes a new machine and identity. A vTPM's state is a file on the Proxmox host, so this protects against the VM's disk or a backup of it, not against the host. It also turns on Microsoft's automatic agent upgrade for the machine (in preview: the agent upgrades itself to within one version of the latest, in batches at off-peak hours, scheduled with cron), which `node-setup.sh` already asks for at connect with `--enable-automatic-upgrade`; the property is checked so a machine connected earlier gets it too. Last, it stops if the machine has any extension in Azure other than the Azure Monitor Linux Agent (delete it, then run again), associates the machine with the nodes' data collection rule (`OCR_NODE_DCR_ID`), waits until the machine reports the allow list to Azure (a few minutes after a change), and installs the Azure Monitor Linux Agent extension with automatic upgrades on, waiting until it's installed (an agent already installed with automatic upgrades off gets them turned on). As soon as `node-setup.sh` has finished, since the connect was its only use, or when `onboard.sh` stops before that, on failure or Ctrl-C too, it deletes the role assignment, the service principal and the app registration, purges them from Entra's deleted items and prints what it deleted; a warning names anything it couldn't delete, to remove by hand.

**A node connected before monitoring** (connected with extensions off): provision once, so the nodes' workspace, component and rule exist and the group has Monitoring Metrics Publisher, then run `onboard.sh` again. It needs no onboarding credential for a connected node, re-runs `node-setup.sh` (which applies the allow list) and installs the agent.

**Monitoring.** The nodes have a workspace of their own, `usnm-ocr-log-<env>`, with its own daily cap of 0.1 GB, and an Application Insights component on it, `usnm-ocr-appi-<env>` (`infra/modules/ocr-node-monitoring.bicep`). The nodes' group can write telemetry to that component only, so a node, or anyone with a node's token, can neither write rows the API alerts read nor fill the environment's 1 GB cap; at worst it fills the nodes' cap, which stops the nodes' monitoring until the reset (ADR-0015). The rule, `usnm-ocr-dcr-<env>`, collects into the nodes' workspace: syslog at Warning and above from the system's facilities (kernel OOM kills, systemd, sshd, chronyd, podman containers' stderr); the worker's own lines at Info and above, from `local0` (its unit's `SyslogFacility`), since journald records every line the worker writes at info, warnings and errors included; and every 60 s, CPU per core, memory used, free space and read and write bytes per file system, and network bytes per interface; and every 60 s, VM insights' log-based metrics (`InsightsMetrics`), which make the Arc machine's **Insights** pane show as configured and fill its **Performance** tab (no Map: no Dependency agent). The agent also sends a heartbeat a minute (free). The worker's traces and metrics go to the nodes' component as role `usnm-ocr-worker`, instance the node name (see the worker's README). The exchange account's queue logs (`StorageQueueLogs`, a row for every queue request) and the rule's errors (`DCRLogErrors`) go to the nodes' workspace too. The estimate is 20 to 45 MB a day per node before the worker's telemetry, which isn't measured yet (Perf measured 7 to 10 MB; VM insights estimated 10 to 20 MB, `InsightsMB` in `ocr-node-health`), so two working nodes come close to the cap; measure it after a day and raise the cap if needed (`ocrNodeDailyCapGb` in `infra/main.bicep`, and `cap_gb` in `ops/queries/ocr-node-health.kql`, then provision). With alert emails set:

- _OCR node log volume high_ (severity 2) fires when a node sends more than a tenth of the cap (10 MB) in an hour, through the agent (Syslog, Perf and InsightsMetrics) and its Application Insights telemetry together, so a crash loop or debug logging is caught before one node fills the cap.
- _OCR node: loc.gov asked to wait over an hour_ (severity 2, hourly) fires when a node's worker counts a `Retry-After` over its one-hour cap (`ocr.loc_slowdowns` with `capped`). The worker holds that server for the hour and takes no pages meanwhile; if loc.gov keeps asking, stop the workers and raise their gaps.
- _OCR nodes' daily cap reached_ (severity 2) fires on the nodes' workspace's `OverQuota` event, as the environment's cap alert does (#279). Until the reset, the node alerts and queries see nothing. In the nodes' workspace, `Usage | where IsBillable | summarize sum(Quantity) by DataType, bin(TimeGenerated, 1h)` shows what filled it.

`scripts/logs.sh` runs the `ocr-node-*` queries against the nodes' workspace (`OCR_NODE_LOG_ANALYTICS_WORKSPACE_ID`):

```sh
scripts/logs.sh --env prod --query-name ocr-node-health             # per node: heartbeat, counters, CPU, memory, free space on /, worker lines, PerfMB, SyslogMB, InsightsMB and WorkerMB; DayMB and CapPct for the workspace
scripts/logs.sh --env prod --query-name ocr-node-errors --span 2d   # worker warnings and errors, panics, failed units, OOM kills, other errors
```

The same as charts: the "usnm OCR nodes (<env>)" workbook under Application Insights `usnm-ocr-appi-<env>` → Workbooks, with a time range and a node picker: CPU, memory, free disk space by mount, disk and network bytes per second, heartbeat gaps over 5 minutes (a table and a chart), the worker's warnings and errors (from Syslog and, once it sends telemetry, `AppTraces` and `AppExceptions`), pages done, failed and abandoned per hour (from its metrics; empty until it sends them), and the workspace's billed MB a day against its cap.

`no heartbeat` means the node is down or off the network, or has lost its agent or its association with the rule (run `onboard.sh` again): the agent sends heartbeats only to the workspaces of the rules it's associated with. `no counters` with heartbeats means the agent is associated but its counters aren't arriving: check the rule's counters and the agent's logs under `/var/opt/microsoft/azuremonitoragent/log`. A node with no heartbeat in the whole span isn't listed at all, so widen the span (`scripts/logs.sh --env prod --query-name ocr-node-health --span 7d`) or check the Arc machine's status.

**Install the worker.** CI builds `usnm-ocr-worker` on AlmaLinux 9 whenever the worker, a crate it uses or its Tesseract image changes on `main`, checks that it needs no glibc newer than EL9's 2.34, runs it on AlmaLinux 10, reads a rendered line with it through the Tesseract image, and publishes a GitHub release, `ocr-worker-<yyyymmdd>-<sha7>`: the binary, the Tesseract image as an OCI archive (`ocr/engines/tesseract/`: the engine survey's Debian base and Tesseract 5.5.0 package, with tessdata_best's `eng`, `deu`, `deu_latf`, `srp`, `srp_latn`, `ces`, `pol` and `rus`), the image's id and `SHA256SUMS`, each with a build provenance attestation. Install it from your machine:

```sh
scripts/ocr-node/install-worker.sh --env prod --name <arc machine> --host claude@<node> \
  [--ssh-key ~/.ssh/<key>] [--release ocr-worker-<yyyymmdd>-<sha7>] [--concurrency N] [--dry-run]
scripts/ocr-node/install-worker.sh --env prod --name <arc machine> --host claude@<node> --rollback [--release <tag>]
```

It takes the newest release unless `--release` names one, downloads the binary, the image id and `SHA256SUMS` with `gh`, runs `gh attestation verify` on each (built by this repository's `ci.yml` on `main`) and checks their sha256. The image archive (a few hundred MB) never crosses your machine: the node downloads it from the release and checks it against the verified `SHA256SUMS`, then loads it into the engine account's (`usnmeng`'s) podman storage through the Podman API socket. The binary goes to `/usr/local/lib/usnm-ocr-worker/<release>/`, and `/usr/local/bin/usnm-ocr-worker` points at it; `--rollback` points it back at the release installed before (or at `--release`) with that release's image. It writes `/etc/usnm/ocr-worker.env` (root:usnm, 0640) from the deployment outputs: `OCR_QUEUE_ENDPOINT`, `OCR_BLOB_ENDPOINT` with `ocr-out`, the Arc machine name as the node id, `--concurrency` (default one page per CPU), the image id, `OCR_NODE_APPINSIGHTS` (left out when empty) and LoC gaps of 3,500 × N and 1,000 × N ms, where N is the number of members of the nodes' group (`--nodes` must equal it). `node-setup.sh` leaves that file alone once `install-worker.sh` has written it. Local changes go in `/etc/usnm/ocr-worker.local.env`, read after it, for example `RUST_LOG=info,usnm_ocr_worker=debug` (the worker's own debug lines; a bare `RUST_LOG=debug` turns on every library's too, and every line goes to the nodes' workspace twice, through syslog and Application Insights, against its cap, so only for a while); then `systemctl restart usnm-ocr-worker`. It also installs the unit, `scripts/ocr-node/usnm-ocr-worker.service`, checks it with `systemd-analyze verify` and restarts the worker if it was running. `--dry-run` verifies the release, reads the node's state and prints the settings file without changing the node.

The unit stays disabled until `--enable`, which first reads a rendered line through the image as `usnm` and the engine runtime (`usnm-ocr-worker check-engine`), then enables the unit and disables it again if it doesn't stay up. The unit runs `usnm-ocr-worker run` as `usnm` with the agent's identity, its settings from the environment file, with `NoNewPrivileges=yes`, `RestrictNamespaces=yes`, no capabilities and `SystemCallFilter=@system-service`. It never runs podman itself: each page's Tesseract container runs under `usnmeng` through the socket (`USNM_OCR_PODMAN_URL`), with no network, a read-only root, all capabilities dropped, the image's user 10001 (a subordinate id of `usnmeng`), 1,536 MiB of memory and no swap, 1 CPU and 64 processes (`USNM_OCR_ENGINE_MEMORY_MIB`, `USNM_OCR_ENGINE_CPUS`, `USNM_OCR_ENGINE_PIDS` in `ocr-worker.local.env` change them). The page's scratch directory is on the scratch tmpfs, labelled for the container's own SELinux MCS level, with an ACL for the container user. A container killed for memory is reported as such. On stop, the worker gets 120 s: in-flight pages get 60 s, telemetry up to 30 s more. `ocr-out` deletes blobs after 30 days, so the worker shouldn't run on real queues before `ocr-sync` ([below](#ocr-pickup-ocr-sync-296)) is deployed.

**The engine runtime spike** (#307 item 0): `scripts/ocr-node/engine-spike.sh --host claude@<node> [--ssh-key FILE]` copies itself and the worker's unit to a node that `onboard.sh` has set up and checks, with sudo, every point ADR-0016 rests on: SELinux enforcing; `usnmeng`, its lingering, subordinate ids and cgroup delegation; the socket and the scratch tmpfs; and, as `usnm` in a transient unit with the worker's hardening, `podman --remote run --interactive` passing JSON lines both ways with the log driver off, the slot's MCS label and ACL scheme, the effective `memory.max` (no swap), `pids.max` and `cpu.max`, `OOMKilled` read before `podman rm`, and tmpfs writes counted against the container's memory. It prints PASS or FAIL for each, exits 1 on any failure, and removes what it made. Post the output on #296.

**When the home IP changes,** the nodes lose the exchange account until the firewall has the new address. Hidden messages come back on their own once their time runs out.

```sh
scripts/settings.sh --env prod --key USNM_OCR_NODE_IPS --value <new IP>
scripts/provision.sh --env prod
```

**Group changes take a while.** Membership is a claim in the identity's access token, and managed identity tokens are cached per resource for around 24 hours; Microsoft says a membership change can take several hours to apply, and a refresh can't be forced ([Microsoft](https://learn.microsoft.com/en-us/entra/identity/managed-identities-azure-resources/managed-identity-best-practice-recommendations#limitation-of-using-managed-identities-for-authorization)). So a node that fetched a storage token before it joined the group is refused by the exchange account until that token expires (that's why `node-setup.sh` checks with a Resource Manager token), and a removed node keeps its access for up to about a day on a token it already holds.

**Remove a node** with no provision:

```sh
scripts/ocr-node/offboard.sh --env prod --name usnm-ocr-node1 --host claude@node1.example.org --dry-run
scripts/ocr-node/offboard.sh --env prod --name usnm-ocr-node1 --host claude@node1.example.org
```

It removes the machine's identity from the nodes' group and deletes its data collection rule associations. With `--host`, it deletes the Azure Monitor Agent extension and waits while the node uninstalls it, then on the node stops and disables `usnm-ocr-worker.service`, removes any extension left (`azcmagent extension remove --all`) and runs `azcmagent disconnect --force-local-only`, which clears the agent's local state without an Azure sign-in, so no onboarding credential is needed. A RHEL node is also unregistered from Red Hat (`subscription-manager unregister`). Last, it deletes the Arc machine with your `az` sign-in, which deletes its identity so it can get no new token. Without `--host` (a node that's gone or unreachable) it leaves the extension to go with the machine and prints the node commands to run later. It needs ownership of the nodes' group and the right to delete resources in the environment's resource group. To cut a node off at once, rather than when its tokens expire, also take its address out of `USNM_OCR_NODE_IPS` and provision; with both nodes behind the same home IP, that cuts off every node. Offboard every node before turning `USNM_OCR_NODES` off. The provision then deletes the exchange account with its queues and `ocr-out`, and the nodes' monitoring: their workspace with its data, their Application Insights component with the group's Monitoring Metrics Publisher grant, the data collection rule and the three node alerts. Azure won't delete a data collection rule that still has associations: with a node still associated, the provision fails until its association is gone. `offboard.sh` deletes the machine's associations. The provision stops before it starts if the rule in `OCR_NODE_DCR_ID` still has any. The Arc machines stay in the resource group, and their identities in the nodes' group, until offboarded.

### Prefetched scans on pve-big (#368)

The scans of the damaged pages are fetched into pve-big's `tank/ocr/scans` by the scan-fetch container, CT 211 `usnm-scanfetch`, and served from there to the nodes over HTTP (12 §12.2.4). Everything runs on pve-big as root, from the repository's `scripts/ocr-node/`:

```sh
pve-ocr-storage.sh                                   # datasets, ocrdata, no NFS export
scanfetch-ct.sh --create                             # CT 211 with scans and manifests mounted
scanfetch-ct.sh --install --release ocr-worker-<yyyymmdd>-<sha7>
scanfetch-ct.sh --serve                              # the scan server, port 8380, for the OCR nodes by name
```

To get a manifest in, build it in Azure (`scripts/start-job.sh --env prod --job INGEST_JOB -- ocr-prefetch-manifest --version <v> --exchange-url <OCR_BLOB_ENDPOINT>ocr-out`, with no other ingest or backfill execution running), then on a node: `sudo pull-manifest.sh --account <OCR_EXCHANGE_ACCOUNT> --index-version <v> --scan-server http://<CT 211>:8380`. Start or move the fetch with `scanfetch-ct.sh --configure --manifest /tank/ocr/manifests/<v>/manifest.parquet [--buffer-cap BYTES] [--buffer-resolution ...] --enable`; it goes on where it stopped.

Check it with `pct exec 211 -- cat /tank/ocr/scans/status-full.json` and `pct exec 211 -- journalctl -u usnm-scanfetch -n 20`. `waiting` means the buffer is at its cap until workers read and delete buffer scans; `failed` after 20 pages from 3 titles in a row means LoC is refusing, so look before starting it again. `cooling` means every page left is in an issue whose item loc.gov couldn't serve (12 §12.2.4, #414); it asks again when the cool-off ends. `/tank/ocr/scans/item-errors.json` lists those issues, and `failures-full.jsonl` has a `loc_item_error` line for each page skipped. Point the workers at the server with `install-worker.sh --scan-cache-url http://<CT 211>:8380`. The nodes mount nothing; `node-setup.sh` removes an old `/srv/ocr-data` mount.

## OCR pickup: ocr-sync (#296)

`usnm-ingest ocr-sync` takes the nodes' output into the lake ([12 §12.5 Step 8](design/12-onprem-ocr-pipeline.md)). It runs hourly at minute 17 (`USNM_OCR_SYNC_CRON`; `manual` turns the schedule off) as `usnm-ocrs-caj-<env>` (saved as `OCR_SYNC_JOB`; `usnm-ocrsync-caj-<env>` before the October 2026 rename, #309), on the ingest image as `id-usnm-ingest`, 0.5 vCPU and 1 GiB. The provision creates the job only when `USNM_OCR_NODES` and the ingest jobs are on, together with the exchange account's `ocr-quarantine` container, the identity's grant on it and two alerts. Runs take the Cosmos lock `ops/ocr-sync`, so two never overlap. Logs carry page ids, paths and reason codes, never text.

Each run:

1. Deletes sources that an earlier run imported or quarantined but stopped before deleting. Its import record (`curated/ocr-local/import/<run>.json`) lists them with their ETags.
2. Drains `ocr-deadletter`. Valid dead letters are written to `curated/ocr-local/deadletter/<day>/<run>.jsonl`, re-encoded from their checked fields, with a `dead_lettered` outcome for each page. The rest go to quarantine. A run that stops between writing and deleting them drains them again after 10 minutes, so a dead letter can appear twice.
3. Checks each part in `ocr-out`, oldest first, at most `--max-parts` (2,000) per run in imports of `--chunk` (200). A part passes when its name and size are right, the strict reader takes it, the page is outstanding in the ledger with the same batch and reason, its text matches its hash and length, the engine, batch, title and image check out, and its ALTO passes the bounded reader. Its ALTO goes to `curated/ocr-local/alto/`, its row to one merged part per title, `curated/ocr-local/pages/<lccn>/<run>.parquet`, with both texts' scores, and an `imported` outcome goes to the ledger. A part that fails moves to `ocr-quarantine` with its ALTO. Then the sources are deleted with If-Match.
4. With a seed plan, tops `ocr-bulk` up (below).
5. Writes `status/ocr-local.json` to the reference container: the last run's and the running totals of imports, rejects by reason and dead letters by cause, the queue depths, pages outstanding, the parts left with the oldest one's age, and the run's seeding.

**Queue pages for the pilot.** Nobody but `id-usnm-ingest` can add messages, so `ocr-enqueue` runs as an execution of the pickup job, which has the exchange account's endpoints. It finds each page in the published version (Step 6's damage tables, or the curated text where they lack the batch). It then writes a ledger row, `curated/ocr-local/queued/<day>/<run>.parquet`, and puts the message: the base batch, the page's detected language in ISO 639-3 (its title's first language for `und`), and LoC's damage rate as the score. It skips pages already outstanding and Japanese titles. `--dry-run` only counts. The ledger rows are written before the messages. If a send fails, the pages after it get a `not_sent` outcome, so running the same command again queues them; the page whose send failed may have gone out, and `--include-outstanding` queues it again. Outside a dry run, `ocr-enqueue` and `ocr-requeue` hold the `ops/ocr-sync` lock, so they fail while a pickup run is going; start them again once it ends.

```sh
scripts/start-job.sh --env prod --job OCR_SYNC_JOB -- ocr-enqueue --pages sn83045462_1890-05-03_ed-1_seq-7,... \
  --reason bulk --plan pilot-german [--batch nbu_gage] [--dry-run]
```

**Queue dead-lettered pages again,** by cause (`dequeue_count`, `invalid`, `permanent`, `unroutable`) and, for `invalid`, kind. Pages queued since their dead letter, imported since, or whose latest dead letter has another cause, are skipped.

```sh
scripts/start-job.sh --env prod --job OCR_SYNC_JOB -- ocr-requeue --cause dequeue_count [--since 2026-10-20] [--dry-run]
```

**Seeding** ([12 §12.5 Step 10](design/12-onprem-ocr-pipeline.md)) is off while `USNM_OCR_SEED_PLAN` is empty. With a plan, each run ranks (batch, title) units by their summed damage over 0.1 from Step 6's tables for the published version, and queues the pages at or above the minimum damage (`USNM_OCR_SEED_MIN_DAMAGE`, default 0.25), unit by unit in issue order. Run `ocr-damage` on the version first: the seeder refuses to run without its tables. It keeps at most `USNM_OCR_BULK_WINDOW` bulk pages outstanding: pages in the ledger with no result yet, or the queue's message count if that is larger. It tops up only when `ocr-bulk` holds fewer than `USNM_OCR_BULK_REFILL_BELOW` messages (default half the window). A window of 0 (the default) is two days of pages at the rate `ocr-sync` imported them over the last week, at least 500. Bulk pages that `ocr-enqueue` and `ocr-requeue` queue count toward the window, but the window doesn't limit them. Other settings: `USNM_OCR_SEED_LANGUAGES` (ISO 639-3, default the Tesseract pilot's `ces,deu,eng,pol,rus,srp`), `USNM_OCR_SEED_BATCHES`, `USNM_OCR_SEED_YEARS` (`1890` or `1850-1919`), `USNM_OCR_SEED_MAX_PAGES` (the plan's budget), `USNM_OCR_SEED_PILOT` (one language or a batch list, and a budget of at most 10,000) and `USNM_OCR_SEED_SKIP_AMERICAN_STORIES` (off; also skips pages whose American Stories text scores under the minimum). Set them with `scripts/settings.sh` and provision.

It skips Japanese titles and pages already in the ledger, and copies of a page that another batch's copy wins (the release's duplicate rule). A page of the plan outstanding for 14 days, or dead-lettered with cause `dequeue_count`, is queued again at most 3 times. One dead-lettered as `unroutable` is queued again after 6 hours, doubling per cycle up to 48. A page whose message a failed run never sent (`not_sent`) goes out in the next run. Each batch's ranked units are stored under `curated/ocr-local/seed/rank/`, and the units a plan has finished under `curated/ocr-local/seed/plans/<plan>/done/`. A run that reaches its time limit before every batch is ranked queues no new pages; the next run continues from there.

**Seeding from fetched scans** (Step 11, #368). With `USNM_OCR_SEED_SOURCE=fetched` the plan's new pages come from the prefetch manifest instead of the ranked units: the keep set of `audit/damage/v2/prefetch/<version>/manifest.parquet` in the curated store (`USNM_OCR_SEED_MANIFEST`, default the published version), in the order the fetcher takes it (German before the 1920s first, each issue together), in the languages, years and damage asked for, and only pages on the newest fetched list in `ocr-out/fetched/<version>/`. A node publishes that list from the scan server's index (`usnm-ocr-worker publish-fetched`), because the job can't reach the LAN. The list only narrows the manifest's pages, so a node can't add a page. The window, budget, ledger, retries and skips are the same as above. A MARC code in `USNM_OCR_SEED_LANGUAGES` (`ger`) is read as its ISO code (`deu`).

`ocr-seed` is the same step on its own. Use `--dry-run` to print what it would queue as JSON, page ids per unit included, without writing anything. Its flags override the settings. For the pilot (Step 11), preview it, then set the plan:

```sh
scripts/start-job.sh --env prod --job OCR_SYNC_JOB -- ocr-seed --dry-run --seed-plan pilot-deu --seed-pilot \
  --seed-languages deu --seed-years 1850-1919 --seed-max-pages 3000 --bulk-window 1000
scripts/settings.sh --env prod --key USNM_OCR_SEED_PLAN --value pilot-deu
scripts/settings.sh --env prod --key USNM_OCR_SEED_PILOT --value true
scripts/settings.sh --env prod --key USNM_OCR_SEED_LANGUAGES --value deu
scripts/settings.sh --env prod --key USNM_OCR_SEED_YEARS --value 1850-1919
scripts/settings.sh --env prod --key USNM_OCR_SEED_MAX_PAGES --value 3000
scripts/settings.sh --env prod --key USNM_OCR_BULK_WINDOW --value 1000
scripts/provision.sh --env prod
```

**Quarantine.** Rejects are kept for 30 days as `rejects/<run>/<source path>` (a dead letter as `rejects/<run>/deadletter/<message id>.txt`), each with a `.reason.json` record: the reason code, the source, its ETag, size and creation time, never its content. A source over its size limit isn't read, so it has a record only. Read them with your own sign-in from home (the firewall allows the node IPs and the subnet). After a fix, put one reason's rejects back where they came from; the same run checks them again:

```sh
scripts/start-job.sh --env prod --job OCR_SYNC_JOB -- ocr-sync --from-quarantine --reason not_requested
```

Reason codes: `name`, `part_too_large`, `part_unreadable`, `rows`, `issue_mismatch`, `alto_missing`, `alto_too_large` (checks 1 and 2); `not_requested`, `batch_mismatch`, `reason_mismatch` (3); `text_null`, `text_too_large`, `text_control`, `text_mismatch` (4); `engine`, `engine_tag` (an `ocr_engine` with `image=local`, or not printable), `reason`, `batch_unpublished`, `title_unknown`, `title_japanese`, `image_url` (5); `alto_*` from the ALTO reader (6); `deadletter_*` for dead letters.

**The gain estimate.** `ocr-sync --report` applies rule v1 (Step 9) to the scores stored in every merged part, without picking anything up. It writes `curated/ocr-local/report/<run>.json` with pages considered and replaced, the reasons LoC's text stays, and counts by language and by engine.

```sh
scripts/start-job.sh --env prod --job OCR_SYNC_JOB -- ocr-sync --report
```

**Alerts** (with alert emails): a failed execution is reported by the pipeline's job-failed rule, `alert-usnm-ingest-job-failed-<env>` (severity 2, its `Execution` names the job; #308), and `usnm-ocrs-backlog-alert-<env>` (severity 3) fires when the last run left a part in `ocr-out` older than 72 hours. `ocr-out` deletes blobs 30 days after their last change, so a backlog left that long is lost.

## Our OCR in releases (#296)

With `USNM_OCR_LOCAL` (the ingest job's `--ocr-local`), a release weighs the text `ocr-sync` took into `curated/ocr-local/` against LoC's on the pages of the batches it indexes, and uses ours where rule v1 prefers it ([12 §12.5 Step 9](design/12-onprem-ocr-pipeline.md)). It is off by default, and with it off a release reads nothing from `ocr-local` and its documents and snapshot files don't change. `id-usnm-ingest` already has Blob Data Contributor on `curated`, so the setting needs no new grant.

```sh
scripts/settings.sh --env prod --key USNM_OCR_LOCAL --value true
scripts/provision.sh --env prod
```

- The weekly delta applies it to the batches it indexes. Published batches take it only at a [full rebuild](#full-rebuild) with the setting on. Each release that builds a main index logs `our OCR in ocr-local` (parts, titles, revocations) and, when the version has published batches, `pages of ocr-local that would replace LoC's text in published batches`, an estimate from the scores `ocr-sync` stored. A release of only the Japanese overlay reads nothing from `ocr-local` and logs neither line. `ocr-sync --report` ([above](#ocr-pickup-ocr-sync-296)) gives the same kind of estimate without a release.
- Records: `reference/<version>/ocr_local.json` (the rule and metric versions, a parts summary, pages considered, replaced and kept by reason, language and engine, and the revocations applied), `ocr_local_pages.bin` (the pages that hold our text), the manifest's `built_from.ocr_local` and the build record's `features.ocr_local`. The API marks hits on those pages "Our OCR", with a viewer link that has no highlight. On a page the Japanese overlay also has, the Japanese overlay wins.
- To take a node's or engine's output out, write a create-only `curated/ocr-local/revoked/<stamp>.json` with `reason`, `reference` and at least one of `node`, `engine`, `run`, `from` and `to`; every field given must match. Matching rows are left out from the next release that indexes their batches, so text already published stays until a full rebuild. A file the release can't read stops the release.
- Clearing the setting stops new replacements. A delta carries the published version's `ocr_local_pages.bin` forward either way; the next full rebuild without the setting goes back to LoC's text everywhere.

## The OCR dashboard on the LAN (#296)

`usnm-ocrdash` shows the scan fetch, the OCR nodes and the Proxmox hosts on one page that refreshes itself, at <https://ocr.tjg.fyi/>, from the home LAN only ([12 §12.2.5](design/12-onprem-ocr-pipeline.md#1225-the-lan-dashboard)). The same data is at `/api/view.json`, and per panel at `/api/hosts.json`, `/api/fetch.json`, `/api/nodes.json`, `/api/next.json` and `/api/ocred.json`. `ocr.tjg.fyi` is a static record in the LAN gateway's local DNS, with a DHCP reservation for CT 212; it has no public A record, and internal names never go in the public `tjg.fyi` zone. The commands below name hosts by their LAN DNS names (`pve.tjg.fyi`, `usnm-ocr-node3-arcs-prod.tjg.fyi`), which resolve only through the gateway's local DNS.

**Host stats.** As root on each Proxmox host, from a checkout of the scripts:

```sh
# pve-big: its file is bind-mounted into the dashboard's container
scripts/ocr-node/hoststats.sh --install --out /var/lib/usnm-hoststats/pve-big.json \
  --pool tank --dataset tank/ocr --dataset tank/ocr/scans --dataset tank/ocr/text
# the NUC host: pushes its stats to the dashboard
scripts/ocr-node/hoststats.sh --install --out /var/lib/usnm-hoststats/pve.json \
  --push-url http://ocr.tjg.fyi/api/host/pve
```

Each installs `usnm-hoststats.timer`, which runs every minute.

**The container.** As root on pve-big:

```sh
scripts/ocr-node/ocrdash-ct.sh --create
scripts/ocr-node/ocrdash-ct.sh --install --release ocr-worker-<yyyymmdd>-<sha7>
scripts/ocr-node/ocrdash-ct.sh --configure --allow lan \
  --worker usnm-ocr-node3=http://usnm-ocr-node3-arcs-prod.tjg.fyi:8381 \
  --worker usnm-ocr-node4=http://usnm-ocr-node4-arcs-prod.tjg.fyi:8381 \
  --host-push pve=pve.tjg.fyi --workbook-url '<the OCR nodes workbook link>' \
  --https-name ocr.tjg.fyi
```

`--create` makes CT 212 (unprivileged Debian 12, 1 core, 768 MB, onboot) with `tank/ocr/scans`, `tank/ocr/manifests` and `/var/lib/usnm-hoststats` bind-mounted read-only. `--install` takes `usnm-ocrdash` from a worker release; every release since the dashboard was merged has it. `--configure` sets the nftables input policy to drop everything but `--allow` (`lan`: the container's subnet), writes `/etc/usnm/ocrdash.env` and starts `usnm-ocrdash.service`. The worker link is the OCR nodes workbook's portal link, from `az resource list --resource-group rg-usnm-prod --resource-type microsoft.insights/workbooks`.

**HTTPS.** The certificate for `ocr.tjg.fyi` is a Let's Encrypt one that pve-big gets and renews itself, with home-infra's `home-acme` (lego over the DNS-01 challenge, signed in as pve-big's Arc managed identity; [home-infra README, "Let's Encrypt with DNS-01"](https://github.com/tgoodyear/home-infra#lets-encrypt-with-dns-01)). CT 212 holds no Azure credentials and never talks to Let's Encrypt.

- `home-acme@ocr.tjg.fyi.timer` on pve-big runs daily and renews when fewer than 30 days are left. It writes `fullchain.pem` and `privkey.pem` to `/var/lib/usnm-certs/ocr.tjg.fyi/` on the host (the key 0640, group 102001, which is ocrdash's gid 2001 in the unprivileged container), then runs `pct exec 212 -- systemctl reload-or-restart usnm-ocrdash.service`.
- `--configure --https-name ocr.tjg.fyi` bind-mounts that directory read-only at `/etc/usnm/tls` (`mp3`) and sets `USNM_OCRDASH_TLS_CERT`, `_TLS_KEY`, `_TLS_LISTEN` (`0.0.0.0:443`) and `_HTTPS_NAME`. The dashboard then serves the page on 443; port 80 answers every request with a 308 to `https://ocr.tjg.fyi/` and the same path, except the NUC's `PUT /api/host/pve`, which stays plain HTTP (it carries no secret, it's checked by address, and `curl` wouldn't follow a redirect). `systemctl reload` (SIGHUP) makes it read the pair again; a pair it can't read is logged and the old one kept.
- The nftables rule accepts any port from `--allow`, so 443 needs nothing new.
- The certificate is in the public Certificate Transparency logs, so the name `ocr.tjg.fyi` is public. Its address isn't: only the TXT record `_acme-challenge.ocr.tjg.fyi` is in public DNS, and it holds a placeholder except for the seconds of a challenge.
- Check: `curl -sv https://ocr.tjg.fyi/healthz` from a LAN machine shows `issuer: C=US; O=Let's Encrypt` and `SSL certificate verify ok`; `journalctl -u home-acme@ocr.tjg.fyi` on pve-big shows each run, with the issuer and expiry.

**When it shows something odd.**

- _A node says "no status"._ The worker isn't answering on port 8381: it is down, or it was installed without `--status-allow ocr.tjg.fyi`, or the name points somewhere else now (the worker resolves it each time its status server starts, the node's nftables rule when `install-worker.sh` runs). `scripts/ocr-node/install-worker.sh --status-allow <address>` sets the worker's settings and the node's nftables rule (`usnm-ocr-status-firewall.service`).
- _The stats are stale._ `systemctl status usnm-hoststats.timer` on the host. On the NUC, a failed push logs `can't push` in `journalctl -u usnm-hoststats`.
- _A new manifest._ The dashboard reads the manifest the fetcher's status names, and reads it again when its sha256 changes.
- _The browser warns about the certificate._ `journalctl -u home-acme@ocr.tjg.fyi -n 50` on pve-big, and `openssl x509 -in /var/lib/usnm-certs/ocr.tjg.fyi/fullchain.pem -noout -enddate`. `systemctl start home-acme@ocr.tjg.fyi` tries again now.
- _Logs:_ `pct exec 212 -- journalctl -u usnm-ocrdash`. The copies of the workers' journals are in `/var/lib/usnm-ocrdash` in the container; deleting them makes the dashboard copy each worker's journal again from the start.
