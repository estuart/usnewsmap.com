# Operations

Running the ingest pipeline in Azure: the backfill, releases, full rebuilds, and how to watch them. The pipeline is two Container Apps jobs, `caj-usnm-ingest-{env}` (`usnm-ingest run`: new LoC batches, the title catalog and a release) and `caj-usnm-backfill-{env}` (parallel `curate` workers for the initial corpus). Both run inside the VNet as `id-usnm-ingest`; the storage and Cosmos accounts stay private throughout. How they are deployed is in [`infra/README.md`](../infra/README.md); the design is in [08](design/08-azure-infrastructure.md) and [04](design/04-data-sources-and-ingestion.md).

Every command below uses your own `az` sign-in. CI can only roll the API onto new images, so starting jobs and changing settings is always a person's action.

## Deploy the jobs

`scripts/bootstrap.sh <env> --ingest` deploys both jobs (or `scripts/settings.sh <env> USNM_INGEST_JOBS true` and `scripts/provision.sh <env>` on an environment already on the registry). `USNM_BACKFILL_WORKERS` sets the backfill's workers (default 4), and `USNM_BACKFILL_CRON` an optional schedule for it (a UTC cron such as `"0 9 2-4 10 *"`; empty is manual), which every deployment keeps. The ingest job's Quickwit writer works on an NFS share, `USNM_INGEST_SCRATCH_GIB` in size (default 128; 0 removes it, 08 §8.4). The weekly schedule comes after the first release ([Weekly updates](#weekly-updates)).

## The title catalog

The catalog (`reference/catalog/titles.json`, `places.json`) is built by `titles-sync`, which `run` calls before every release. A batch is released only once its titles are in the catalog. LoC rate limits title records, often well under its published 20 a minute; titles-sync then sends nothing for 65 minutes and resumes more slowly, until 8 h into the run (`--titles-max-runtime-secs`), and the next run continues from where it stopped. `/v1/status` shows `titles.pipeline.awaiting_sync` (titles still to fetch) and `batches_waiting_for_titles`. Place corrections live in git and ship in the ingest image; the next run applies them (04 §4.6):

- **A misspelled or renamed town** that should share a place with the town's other titles: add `{"added": "2026-10-06", "from": "Skaguay", "reason": "old spelling", "ref": 190, "source": "…", "state": "AK", "to": "Skagway"}` to `catalog/overrides/place-aliases.json`. Every override entry carries its audit trail: `added` (the date), `reason`, `source` (what shows the change is right: a URL or a precise citation) and `ref` (the issue or PR), plus `updated` and `history` when it changes later (`catalog/overrides/README.md`). `from` is the city as LoC spells it (spelling variants such as "St."/"Saint" or a trailing state abbreviation already match); `to` is the town's name, which becomes the place's name. Keep the entries sorted by state, then `from`, and each entry's keys in alphabetical order; `cargo test -p usnm-ingest places` checks both, and refuses an alias whose `to` is another alias's `from`. Only merge towns that are one place on the map; a town that was separate (East Providence, Winston before 1913) keeps its own place.
- **Wrong coordinates or name**: add `{"added": "…", "city": "Lusk", "lat": 42.7625, "lon": -104.4522, "reason": "…", "ref": 123, "source": "…", "state": "WY"}` to `catalog/overrides/places.json`, sorted by state, then city, keys in alphabetical order, with `"name"` to change the place's name and `"precision"` (`city`, `county` or `state`) if needed. `source` says where the coordinates come from, `reason` why the override exists. Take the coordinates from the gazetteer (`catalog/gazetteer/us-places.csv`) or another public source, not from LoC, unless the gazetteer lacks the town and one of the place's own LoC records has its point (New Echota, #223). The `geocode` log lists places where LoC's point and the gazetteer's are more than 25 km apart and LoC's was kept (`furthest` in its warning, `disagreement_examples` in the report): start there. A LoC point more than 100 km from the gazetteer's town of that name (when the name isn't ambiguous there) is replaced by the gazetteer's already (`gazetteer_over_loc_places`); check that list too, since the gazetteer can have the name for a different town than the paper's. It also lists places at the median of LoC points more than 25 km apart (`loc_apart_examples`, with the two furthest points and their LCCNs): one of those records likely has another town's point. Each place in `catalog/places.json` records the rule its coordinates came from (`coordinates_from`) and the other names its titles use with the rule that merged each (`variants`).
- **A title whose record lists several places** (a paper that moved): add `{"added": "…", "city": "Chicago", "lccn": "sn84024055", "reason": "…", "ref": 190, "source": "…", "state": "IL"}` to `catalog/overrides/title-places.json`, sorted by `lccn`, keys in alphabetical order.
- **Check the effect first**: run `curl -s https://api.usnewsmap.com/v1/places > /tmp/places.json`, then `cargo run -p usnm-ingest --example place-report -- /tmp/places.json`; the report lists the merges, renamed places and places that move.

A corrected point or name ships with the next release, delta or full. A change that moves a published title to another place (most aliases, a title place, new merging rules) waits for the next full release: incremental releases keep published titles where they were published, and log `N published titles' place changes wait for the next full release`. Run a [full rebuild](#full-rebuild) on the E4 profile to apply them; the weekly run never starts one for this, since on the Consumption profile a full release runs out of memory (#172). To refresh the gazetteer for a newer Census year, run `scripts/build-gazetteer.py --year YYYY` and commit `catalog/gazetteer/us-places.csv`.

## Backfill and the first version

Backfill, then publish the first version:

```sh
RG=$(scripts/settings.sh prod AZURE_RESOURCE_GROUP)
# Queue every batch from LoC's listing and curate it. Downloads are paced to
# LoC's limit (10 bulk requests per 10 minutes per IP), one every 75 s across
# all workers, so this takes about 2.5 days. Each worker stops claiming 22 h
# after it starts and exits 0 once its current batch is done (by about
# 22 h 45 min). Start the job again when an execution has ended, until the
# queue is empty (it resumes where it left off).
az containerapp job start -n "$(scripts/settings.sh prod BACKFILL_JOB)" -g "$RG"
# When that has finished: curate any leftovers, build the base index, publish.
az containerapp job start -n "$(scripts/settings.sh prod INGEST_JOB)" -g "$RG"
```

Watch with `az containerapp job execution list -n <job> -g "$RG" -o table` and the saved log queries:

```sh
scripts/logs.sh prod list                      # the saved queries (ops/queries/)
scripts/logs.sh prod release-progress 6h       # docs sent, rate, retries, free disk, memory, merges
scripts/logs.sh prod index-layout 2d           # splits, docs and size of each index a release published
scripts/logs.sh prod curation-throughput 1d    # batches and pages per hour
scripts/logs.sh prod curate-replicas 6h        # each worker's last line, batch and stage
scripts/logs.sh prod errors-by-batch 1d
scripts/logs.sh prod job-executions 2d
```

The workspace id is the stack output `LOG_ANALYTICS_WORKSPACE_ID`; the queries run with your own sign-in (Log Analytics Reader or more). Traces (`release`, `curate`) and metrics are in Application Insights `appi-usnm-<env>`. With alert emails set, a failed job, a stalled release, a stalled backfill or a backfill replica silent for 15 minutes sends an email (08 §8.1.2). So does a full Log Analytics daily cap (08 §8.1.1): until the cap resets nothing is logged, so a job that looks stalled may still be working; check `az containerapp job execution list` before stopping it. The same views, as charts: the "usnewsmap pipeline" workbook under Application Insights `appi-usnm-<env>` → Workbooks.
The last lines of each worker say why it stopped: `max runtime reached; claiming no more batches` (with the number of batches still queued) when the execution should be started again. `curation finished` on its own means the worker found nothing more it could claim. A batch that failed in this execution is queued again but skipped by that worker, so check `scripts/logs.sh prod errors-by-batch 1d` and start the job once more if it lists batches that weren't curated later.

## Switch the API to the published indexes

Once a version is published (`current.json` exists in `reference/`): `scripts/settings.sh <env> USNM_SEARCH_BACKEND quickwit`, then `scripts/provision.sh <env>`. Until then the API would have nothing to serve ([`infra/README.md`](../infra/README.md#what-this-slice-runs)).

## Weekly updates

Poll LoC weekly for new batches: `scripts/settings.sh <env> USNM_INGEST_CRON "17 3 * * 1"` (Mondays 03:17 UTC), then `scripts/provision.sh <env>` while no ingest execution is running. The setting lives only in `.azure/<env>/.env`. Each run fetches LoC's listing once, curates and releases what is new, and exits 0 when there is nothing (08 §8.4). It stops curating 6 h in and releases what it has; if its log says `max runtime reached; claiming no more batches` with batches still queued, start the backfill job to curate the rest. A scheduled job can still be started by hand.

## Check a release's splits

Before it publishes, a release waits for the writer's merges: `release merges` lines (every 30 s: step `settle` then `finalize`, splits, merges running and queued, free disk), then `merges settled; closing the index for its final merges`, then `merged; the index is closed to further writes` with the new index's split count. It then logs one `index layout` line per index of the new version and records the same under `indexes` in `reference/<version>/manifest.json`. `scripts/logs.sh prod index-layout 2d` lists them. A merged index has at most `pages / 30,000` (rounded down) `+ 7` splits (`split_num_docs_target` in `infra/quickwit/pages-index.yaml`); the release fails rather than publish more, or if merges don't finish within `USNM_MERGE_TIMEOUT_SECS` (default 14400, 4 hours; a setting, `scripts/settings.sh <env> USNM_MERGE_TIMEOUT_SECS <secs>` then `scripts/provision.sh <env>`, budgeted under the ingest job's 48-hour replica timeout (the backfill and Japanese OCR jobs keep 24 hours) in `infra/modules/ingestjobs.bicep`), or if the writer reports a full disk or a failed merge, or exits. A writer the kernel killed for memory shows as `the Quickwit writer was killed by signal 9 (SIGKILL): it ran out of memory (…)` in the `command failed` line; `quickwit_rss_mb` in `release progress` is its memory (08 §8.4).

## Searcher caches

The Quickwit sidecar keeps the split footers, fast fields and partial and predicate results in memory, with the capacities in `infra/quickwit/searcher.yaml` (08 §8.4). The API reads the sidecar's metrics every minute and reports them as `api.searcher_cache_*` (08 §8.1.2):

```sh
scripts/logs.sh prod searcher-caches 1d        # per replica and cache: MB and items held, hits, misses, evictions, hit rate
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

1. Add up `footer_bytes` on the version's `index layout` lines (`scripts/logs.sh prod index-layout 2d`, column `FooterMb`), or under `indexes` in `reference/<version>/manifest.json`, over every index the version lists: the base, its deltas and the Japanese index.
2. Set `split_footer_cache_capacity` in `infra/quickwit/searcher.yaml` to that total plus about 25% for the deltas the next weekly releases add, rounded up. Quickwit reads `256MB` as 256,000,000 bytes, the unit `FooterMb` uses.
3. Check it fits the sidecar's 4 GiB next to the fast field cache (1 GB), the aggregation memory limit (768 MB) and the partial request and predicate caches (32 MB each). If it doesn't, the fast field cache is the one to shrink first; then the container's memory (`infra/modules/containerapp.bicep`).

The change goes live when it merges (CI applies `searcher.yaml` to the running app). Confirm with `searcher-caches` a day later: after the warm-up, footer misses and evictions stay near 0 (each replica start misses about once per split), and the footer cache's MB held is about the footer total.

## Searcher threads

The sidecar runs each split's search on its `search` thread pool, and downloads from Blob and opens the splits on its main Tokio runtime. The API reads both with the caches, every minute, and reports them as `api.searcher_pool_tasks` and `api.searcher_runtime_*` (08 §8.1.2):

```sh
scripts/logs.sh prod searcher-threads 2h       # per replica and minute: search pool ongoing and pending, main runtime threads and busy share
```

Read it while cold searches run (`scripts/load-cold-searches.py`):

- `SearchPending` above 0 with `SearchOngoing` at the pool's size: searches wait for cores. The pool has `RAYON_NUM_THREADS` threads (4, set on the sidecar in `infra/modules/containerapp.bicep`; without it, one per CPU Rust counts, which rounds the 3.75 vCPU quota down to 3). Quickwit doesn't report the size; it is the most `SearchOngoing` reaches.
- `MainBusyPercent` near 100 with the search pool below its size and nothing pending: searches wait on the main runtime. It has `QW_TOKIO_RUNTIME_NUM_THREADS` threads (4, same file; `MainThreads`).
- Neither: look at Blob reads, and at `max_num_concurrent_split_searches` in `infra/quickwit/searcher.yaml`, which limits the split searches downloading at once.

The pool numbers are a reading once a minute, not an average, so a short burst can fall between two. `MainBusyPercent` is busy time over the thread time the runtime had (`api.searcher_runtime_capacity_ms`), so it stays a share however the reports fall into the minutes; it is empty for a replica's first minute and after a searcher restart.

## The Quickwit image

The searcher sidecar, the ingest image (whose Quickwit is the release's writer) and the ingest job's init container run our build of Quickwit v0.9.1 ([ADR-0013](design/adr/0013-quickwit-from-our-fork.md)). `Dockerfile.quickwit-patched` builds it from our fork, [`tgoodyear/quickwit`](https://github.com/tgoodyear/quickwit), at a pinned commit. `infra/quickwit-image.json` records its digest for each environment, and `infra/main.bicep` and CI's publish read it there; an environment the file doesn't list runs upstream v0.9.1.

Build it in an environment's registry, which records the digest in the file:

```sh
scripts/build-quickwit.sh --env prod              # ACR Tasks, about an hour; the build log follows
scripts/build-quickwit.sh --env dev --from prod   # or import prod's build (same digest, no build)
```

If your `az` session expires during the build, ACR still finishes it. Once it has, run the same command again: it finds the tag and records its digest without building (`--rebuild` builds again). Then commit the file in a pull request. Once it merges, CI's publish checks that the registry has the digest (it fails if not) and rebuilds the ingest image on it, which the jobs pick up at their next execution. The sidecar and the init container move with `scripts/provision.sh <env>`, run once publish has finished.

**A new patch, or a Quickwit upgrade:**

1. On the fork, commit to the `usnm/v0.9.1-pool` branch (for an upgrade, rebase the commit onto the new upstream tag in a new branch, such as `usnm/v0.9.2-pool`) and tag it with a new `usnm-*` tag, such as `usnm-v0.9.1-pool2`. Names starting `usnm` trigger none of Quickwit's own workflows.
2. In `Dockerfile.quickwit-patched`, set `QW_COMMIT_HASH` to the tagged commit and `QW_COMMIT_TAGS` to the tag without `usnm-` (`v0.9.1-pool2`), which is also the image's tag. For an upgrade, also take the new version's Dockerfile changes (base images, packages) and its `QW_COMMIT_DATE`, and move the `upstream` pin in `infra/quickwit-image.json` and `Dockerfile.ingest`'s default to the new release (CI checks they match).
3. `scripts/build-quickwit.sh --env prod`, then commit, merge and provision, as above.
4. Re-run the cold-search test (`scripts/load-cold-searches.py`) and read [the searcher's threads](#searcher-threads).

**Back to upstream:** remove the environment from `environments` in `infra/quickwit-image.json` (or put its previous digest back), merge, and run `scripts/provision.sh <env>` once publish has finished. Publish copies upstream v0.9.1 into the registry if it isn't there and rebuilds the ingest image on it. To turn pooling off in the searcher alone, without a new image, set `QW_AZURE_POOL=false` on the sidecar (`infra/modules/containerapp.bicep`) and provision.

## Cache warm-up

Each API start, and each publish before it swaps the new version in, warms the caches with the home page's examples and the most frequent logged searches (06 §6.5). One line per run:

```sh
scripts/logs.sh prod warm-up 2d                # per run: examples warm, cached, computed, skipped, ms
```

A start isn't ready until its run ends, so in a rollout the old revision serves meanwhile and the new one takes visitors only once it's warm (06 §6.6); every prod start from 3 to 10 October 2026 used its whole 5-minute budget (`Ms` about 300000), but a start that finds everything cached is ready in seconds (below). A start still running at the readiness cap (6 minutes in prod, 1 minute where the app scales to zero) logs `warm-up still running at the readiness cap; reporting ready` and serves visitors while it finishes.

Read `Warm` against `Examples` first: equal means every example's search was in the in-process cache when the run ended (it is counted then, so evictions show). Then:

- **A start that read everything:** `Cached` is the examples plus the logged searches, `Computed` and `Skipped` are 0, and `Ms` is a few seconds. This is the usual start once a version and release have been warmed once.
- **A start that computed some:** `Computed` above 0 means searches no cache held: examples added since, or every search after a release that changed the response format (`RESPONSE_FORMAT` in `crates/usnm-api/src/routes/mod.rs`). Each takes about 11 s, so about 26 fit in the 5-minute budget. `Skipped` above 0 means the budget ran out with that many searches left; the next start reads what this one computed and goes on from there, and a visitor who opens a skipped example computes and persists it.
- **A publish:** `Cached` is 0 or close to it (nothing is cached for a new version yet, unless a visitor computed it meanwhile), and `Computed` is the searches reached in 15 minutes, about 75 of the 100 examples. The next start computes the rest.
- **`GaveWayMs`** is how long a start's computations waited because every visitor slot was taken. Large values mean visitors kept the searcher busy during the start.
- **`TimedOut` or `Failed`** above 0: the `slow warm-up query` and `warm-up query failed` lines of that run name the example (`api-errors` lists the failures).

Each computed search also logs `slow warm-up query` with its `ms` when it takes 1 s or more, and `api.prewarm_query_seconds` (by `endpoint` and `source`, `cache` or `computed`) has the time of every warm-up query.

## Full rebuild

Rebuild the indexes as one merged base (one-off, or a compaction). A release also builds a full base by itself, without `USNM_INGEST_FULL`, when the published version was built with another search backend, without common-word pairs or with a version of them newer than the release knows (05 §5.5.3), or when `USNM_AMERICAN_STORIES` is on and the published version doesn't have American Stories' text (below); the steps below force one. It is also how merged places and other place changes of published titles reach the site (04 §4.6). Indexes built before the merge settings (September 2026) keep their many small splits: their splits are past any maturation period, so nothing merges them in place, and they are sealed. A full release builds a new base from every curated batch, merged, and publishes a version with no deltas; the previous version stays in Blob for rollback (`current.json`). A full base is also what moves a version to the latest analyzer versions (05 §5.5.3): a delta folds its words as the published version's indexes do, so a change to the folding, such as vulgar fractions as one word (`½`, #168), reaches the site only with a full rebuild. Afterwards `current.json` has the latest `common_grams` and `ja.fold` (2 since #168), as does `build.features` in `reference/<version>/manifest.json`, and the API parses queries for them. It takes the writer lock like any release, so it never runs alongside another writer (08 §8.4.1). Run it when no backfill or ingest execution is running and the backfill has finished:

```sh
RG=$(scripts/settings.sh prod AZURE_RESOURCE_GROUP)
JOB=$(scripts/settings.sh prod INGEST_JOB)
scripts/settings.sh prod USNM_INGEST_FULL true
scripts/settings.sh prod USNM_MERGE_TIMEOUT_SECS 14400   # the default; set it only to change it
scripts/settings.sh prod USNM_DEDICATED_PROFILE true     # the E4 profile (#172)
scripts/settings.sh prod USNM_INGEST_ON_DEDICATED true   # and the ingest job on it
scripts/provision.sh prod
curl -s https://api.usnewsmap.com/v1/status | jq .titles.pipeline   # awaiting_sync: titles to fetch first
az containerapp job start -n "$JOB" -g "$RG"
```

A full run releases only once titles-sync has fetched every title the listing and the curated batches name (titles LoC doesn't have excepted; a failed fetch counts as left); a base built without them would leave their batches out. With titles to fetch (about 4.5 s each, plus 65 minutes for each time LoC blocks), the first execution may spend its 8 hours on titles-sync and then fail with `titles-sync reached its deadline with N of M titles left … nothing was released` (or `LoC rate limited titles-sync …`). That is expected: start the job again, and it continues where it stopped. The job-failed alert leaves this stop out; the severity 3 _ingest not progressing_ alert fires if no execution follows within 3 hours, or if 3 stops in a row leave as many titles as the first. `scripts/logs.sh prod ingest-endings` lists each execution's ending, and the stop's `command failed` line carries `outcome: titles_left`. Once titles-sync finishes, the same execution builds the base. Watch it:

```sh
scripts/logs.sh prod release-progress 6h    # docs sent, rate, memory (quickwit_rss_mb), merges
az containerapp job execution list -n "$JOB" -g "$RG" -o table
```

- `title records` lines every 100 titles, and a `title records` report at the end (`left: 0` and no `throttled` or `out_of_time` when done).
- `release progress` every 30 s: about 750 pages a second before the common-word pairs (#154), so 23.8M pages took about 9 h; with the pairs (each page's index about 1.6× larger) the 2026-10-05 rebuild sent 230 to 460 pages a second, about 15 to 29 h for 23.7M pages (#172). `quickwit_rss_mb` should stay under about 5,500 on the Consumption profile (7,680 MiB container), or about 20,000 on the E4 profile (25 GiB).
- `release merges`: step `settle`, then `finalize`; `splits` falls by 9 every minute or two after ingest ends. About 1.7 h for 23.8M pages in October 2026; with the 60,000-page splits and the common-word pairs, expect about 2.7 h (#156), and at most `USNM_MERGE_TIMEOUT_SECS`. American Stories' text about doubles the index (step 4 of #218 measures the merge time).
- `merged; the index is closed to further writes`, `index layout` (about 800 splits for 23.8M pages at the 30,000-page target) and `released`. About 11 h from the start when the catalog was complete, in the October 2026 rebuild before #154. With the pairs, the 2026-10-05 rebuild sent only 230 to 460 pages a second, so plan for up to about 29 h of sending and about 41 h in all (up to 8 h of titles-sync, sending, up to 4 h of merges), inside the ingest job's 48 h replica timeout (#172, #173).

**Laying the base out by decade (#123, 05 §5.5.5).** To have the rebuild keep each decade in its own splits, so that a date-limited search skips the others, add the setting to the first provision above:

```sh
scripts/settings.sh prod USNM_PARTITION_DECADE true   # the job's --partition-decade
```

Only a full base takes it. The release then logs `decade layout` when it creates the index, a `sent a decade's pages` line each time a decade's pages go to the writer (at most every 30,000 pages of a decade; until then they wait in `/scratch/usnm/quickwit/decade-spill`, a few GB at most), and `sent the pages a decade at a time` with the totals before the merges. The `index layout` line of the base has `splits_by_decade`. Check afterwards: `current.json` has `decades: 1`, and `reference/<version>/manifest.json` has `build.features.decades` and `splits_by_decade` under `indexes`. Leaving the setting on or off changes nothing until the next full rebuild: deltas follow the published version (with the field and decade tags on a version laid out by decade, without on one that isn't). To stop searches naming decades at once, without a rebuild, publish a `current.json` without the key.

**One searched text field (#283, 05 §5.5.7).** To have the rebuild search LoC's and American Stories' text in one field (`text_all`), so a search looks each word up once, add the setting to the first provision above, alongside `USNM_AMERICAN_STORIES`:

```sh
scripts/settings.sh prod USNM_SINGLE_TEXT_FIELD true   # the job's --single-text-field
```

Only a full base takes it. The release logs `one searched text field` when it creates the index, and `building index` with `text_layout: 2`. Check afterwards: `current.json` has `text_layout: 2`; `reference/<version>/manifest.json` has `build.features.text_layout` and, in `build.templates.pages`, the `text_all` and `text_all_cg` fields; `/v1/meta` says `"text_layout": 2`. The aggregate then has no `total.american_stories_only` (the site drops its page-count note), while hits keep `matched_in` and the American Stories badge. `USNM_AMERICAN_STORIES_SEARCH` has no effect on such a version (below). Leaving the setting on or off changes nothing until the next full rebuild: deltas follow the published version. To go back at once, publish a `current.json` of a version with a field per text (rollback); a full rebuild without the setting does it for good. Turning `USNM_AMERICAN_STORIES` off on such a version makes the next release a full base: the text can't be left out of the field it shares with LoC's in a delta.

Then turn the full rebuild off, and the E4 profile with it, in two provisions: the job has to leave the profile before the profile can go.

```sh
scripts/settings.sh prod USNM_INGEST_FULL ""
scripts/settings.sh prod USNM_INGEST_ON_DEDICATED ""
scripts/provision.sh prod
scripts/settings.sh prod USNM_DEDICATED_PROFILE ""
scripts/provision.sh prod
```

Don't leave `USNM_INGEST_FULL` set: every run, scheduled ones included, would rebuild the whole corpus. Don't leave `USNM_DEDICATED_PROFILE` set either: while the environment has the E4 profile it pays the Dedicated plan management fee every hour (about $0.10 an hour in East US 2 in October 2026), whether or not a node runs. An E4 node itself (4 vCPU, 32 GiB) costs about $0.39 an hour while the job runs.

**Why the E4 profile.** The first full rebuild with the common-word pairs (#154) was killed when the Quickwit writer ran out of memory at the Consumption profile's limit (7.5 GiB with the scratch share's init container), 6.5 h in, at 26.5% (#172). On the E4 profile the ingest container gets 3.0 vCPU and 25 GiB, and the writer a 6 GiB indexing heap, a 120 s commit timeout and a 4 GiB ingest queue (`USNM_WRITER_HEAP`, `USNM_WRITER_COMMIT_SECS`, `USNM_WRITER_QUEUE`, set by `infra/modules/ingestjobs.bicep`), so it writes splits at the 30,000-page target instead of cutting small ones every 30 s and merging them (#172). The `writer tuning` line at the start of the release shows them. A full run also takes longer than the October 2026 rebuild (about 230 to 460 pages a second instead of 750), which is why the ingest job's replica timeout is 48 h (#173). Benchmark afterwards with `scripts/bench-cold-searches.py`. The commit timeout was 600 s at first: the ingest queue's write-ahead log is only truncated when a commit is published, so it filled in about 4 minutes and Quickwit refused pages ("no shards available") for the rest of each 10 minutes, and the October 6 rebuild was stopped at 15% after averaging 132 pages/s. At 120 s the queue holds about 2 minutes of pages.

## American Stories' text

The release indexes American Stories' text beside LoC's (04 §4.9, 05 §5.5.4, #218) only with `USNM_AMERICAN_STORIES` (the job's `--american-stories`), off by default. The first release with it builds a full base, so turn it on for a full rebuild, after step 4 of #218 has measured the index size and rebuild time, and with an explicit go-ahead:

1. Run the writer to the end: `scripts/ja-ocr/start-quality.sh prod american-stories-write` on the one-replica job. It marks each finished year with `curated/american-stories/years/{year}.json`; start it again until all 180 years have one (its log's `american stories write finished` line counts them). A release reads only the years with a marker, and a year finished after the base reaches the base's pages only at the next full rebuild.
2. Do the [full rebuild](#full-rebuild), with the setting added to its first provision:

   ```sh
   scripts/settings.sh prod USNM_AMERICAN_STORIES true
   scripts/settings.sh prod USNM_INGEST_FULL true          # optional: the setting forces a full base anyway
   ```

   With no year marked yet, the run fails at its start, before curation and titles-sync (`--american-stories (USNM_AMERICAN_STORIES) is set, but the curated store has no finished year of American Stories' text`). Its log has `American Stories' text` (years, parts) near the start, then `American Stories' text for the batch` per batch (pages, `text_mb`: what the release holds in memory for the batch), and `American Stories' text indexed` (documents with the text, and those with only it) before the merges.

3. Check the result: `current.json` has `american_stories: 1`; `reference/<version>/manifest.json` has `built_from.american_stories` and `build.features.american_stories`, and `american_stories.json` lists the parts read. On the site, a word from an American Stories headline finds pages, and a hit whose match is only there shows its snippet from that text.

Keep `USNM_AMERICAN_STORIES` set afterwards: weekly deltas then write the text for their pages too. Clearing it makes the next release publish a version without `american_stories`, so searches leave the text out; setting it again rebuilds in full.

To stop searching the text at once, without a release (to time cold searches without it, #251, or if it costs too much), switch the API's setting off, and on again the same way:

```sh
scripts/settings.sh prod USNM_AMERICAN_STORIES_SEARCH false   # on again: true, or "" for the default
scripts/provision.sh prod
```

The API then searches LoC's text alone, whatever `current.json` says (05 §5.5.4): no `matched_in`, no American Stories snippets or badge, no `total.american_stories_only`. The indexes keep the text, so switching back needs no rebuild. The provision changes the API container's settings, so Container Apps starts a new replica: it loads the version, runs the startup warm-up and reports ready when it ends (about 5 minutes) or the readiness cap passes (6 minutes in prod, `USNM_READY_CAP_SECS`, 06 §6.6), and the old replica serves until then. Responses computed in one state are cached under keys of their own (`…|american_stories=off` when off), so the first switch off finds none cached: the warm-up computes what its budget allows and other searches start cold. Switching back on finds the responses persisted before (in Blob) still there. Check the result in the replica's start-up log line `American Stories' text search (USNM_AMERICAN_STORIES_SEARCH)` (`setting`, `in_version`, `searched`, `text_layout`) or in `/v1/meta` (`"american_stories"`). Browsers may keep a response they fetched in the other state for up to a day (06 §6.5).

On a version with one searched text field (`text_layout: 2`, [Full rebuild](#full-rebuild)) the setting can't switch the text off: it shares the field with LoC's. The API logs `USNM_AMERICAN_STORIES_SEARCH=false has no effect on this version` and searches both texts. To leave the text out there, roll `current.json` back to a version with a field per text, or rebuild in full without `USNM_AMERICAN_STORIES`.

## Japanese OCR: mixed pages and English text

**Mixed pages** (#204, 04 §4.8). The OCR job's targets (`targets-v3`) include the Japanese titles' pages whose LoC text is from 0.35 to under 0.65 word-like (`loc_text = mixed`, about 2,000 pages). List them once on the one-replica job, then start the OCR job as usual:

```sh
RG=$(scripts/settings.sh prod AZURE_RESOURCE_GROUP)
scripts/ja-ocr/start-quality.sh prod targets            # caj-usnm-jaone: writes ocr-ja/targets-v3.jsonl
# its log ends with `targets written` and the pages by loc_text (mixed: about 2,000); then
az containerapp job start -n caj-usnm-jaocr-prod -g "$RG"   # `run`: OCRs the targets not done yet
```

Listing streams the archive of every batch with a Japanese title again, as the first listing did, to find the pages LoC ships without text; pages already read are skipped by the run. Another cut: `start-quality.sh prod targets --mixed-below 0.8`, then `start-quality.sh prod run --mixed-below 0.8` (its own list and claims, `targets-v3-mixed80`); 0.35 or less lists no mixed pages. At the job's pace in October 2026 (a 2-page issue in about 21 s and a 4-page one in about 38 s per replica, mostly loc.gov's pacing) the 2,000 pages take about 5 hours on the two replicas, about $4.50 of Consumption time (2 × 4 vCPU and 8 GiB at about $0.43 an hour each), plus the listing. The next release publishes their Japanese text (an overlay-only release when nothing else is new). A mixed page joins the Japanese index only with Japanese on it: expect about 200 (Rocky Shimpo's front pages).

**English text of our OCR** (#203, 04 §4.8). Off by default. With `USNM_JA_LATIN` (the job's `--ja-latin`), a release gives the pages LoC ships without text a main-index document with the Latin-script text our OCR read on them (their English ads, mastheads and sections), and gives a page LoC read badly our text in place of LoC's where ours reads more words. No full rebuild is forced:

```sh
scripts/settings.sh prod USNM_JA_LATIN true
scripts/provision.sh prod
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
scripts/ja-ocr/start-quality.sh prod --sample-pct 10 --min-pages 50
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

Titles and batches enter the worst lists with at least `--min-pages` pages with a `damage_rate`, and `undetermined-titles` with that many pages with text. The same rows are in Log Analytics, one `ocr quality` line each with `metric` and `table` fields, then `ocr quality finished` with the totals; `ocr quality progress` lines come about once a minute from each replica, with the chunks and batches done so far across the run. `scripts/ja-ocr/quality-rows.sh prod <execution>` prints a run's rows as JSON lines:

```sh
scripts/logs.sh prod ocr-quality 2d
```

A v2 run also writes `status/ocr-quality.json` in the reference container for the status page: `{batches: {done, total}, finished_at, metric, pages_sampled, sample_pct, started_at, summary, updated_at, version}`, at the start, with each progress line and at the end. At the end `finished_at` and `summary` are set: `summary.agreement` (`differs_share`, `mixed_share`, `und_share` of all pages with text, `multilingual_differs_share` of multilingual titles' pages) and `summary.languages`, each detected language with at least 200 sampled pages (`pages`, `function_share_median`, `damage_rate_median`, `damaged_share`). Every field is present, and null when there is nothing to measure: the agreement shares when no sampled page has text, `multilingual_differs_share` when no page of a multilingual title does (a small sample), and a language's three scores when none of its pages was scored (a language with no word list, such as Cherokee). A run that fails leaves its last progress, with no `finished_at`. A failed write is logged and the audit goes on.

Read the numbers with these limits in mind:

- It is a sample. At 2% a small title or a single decade of a rare language has few pages; check `text_pages` and `damage_pages` before trusting a row.
- `damage_rate` has a floor: real words outside the top 5,000 that are one edit from a common word ("ana", "hen", "ion" in English) count as misses, so clean English scores about 0.04 to 0.05. Old spellings do too, so compare decades within a language more than languages with each other. Misreads that are real words ("tho" for "the", "ber" for "der") and misreads two edits away ("bcr", "beu" for "der", "den") don't count, so it undercounts damage.
- `und` mixes three things: pages too short to tell, pages so garbled no language fits, and pages in languages with no word list. `garbage_share` and the `undetermined-titles` list help tell them apart. Heavily damaged pages can fall to `und` instead of their language, so a language's damage shares leave out its worst pages.
- Detection only chooses among the title's catalog languages and English: a German page in a title catalogued as English only is `und`, not German. English is always a candidate, so a page counted as English in a title that doesn't list English is English text (ads, a column) by its function words.
- Pages of titles that list Japanese are left out (our own OCR covers them) and counted in the `japanese_skipped` row.

## Text the Japanese titles' pages hold that search can't reach

`jaocr.py mixed` (`ja-ocr/mixed.py`) measures two gaps the Japanese OCR leaves. The OCR job read a page only when LoC's text for it was missing, empty, short or under 35% word-like; a Japanese query searches only our OCR, any other query only LoC's text. Its October 2026 results led to the `mixed` targets and the English text of our OCR in the main index ([above](#japanese-ocr-mixed-pages-and-english-text)); the command still measures by the 35% test.

- Japanese on pages LoC read as words: a page with an English column and a Japanese one, read in English, can pass the 35% test, and its Japanese column is then garbled Latin. For every page with LoC text of a title that lists Japanese, in the batches that hold such titles, it counts pages by word-like share (bands from under 0.35 to 0.9 and over); the other titles in those batches, same scanning and OCR but English only, are the control.
- English on the pages we read: NDLOCR-Lite reads some English (mastheads, short lines), but only into the Japanese index. For every page of our OCR it counts the tokens that are among English's 5,000 most frequent words (3 letters or more).

```sh
scripts/ja-ocr/start-quality.sh prod mixed          # prints the execution's name
scripts/ja-ocr/quality-rows.sh prod <execution>     # its "mixed pages" rows, once it has finished
```

It writes `audit/mixed-pages-<version>-<execution>.json` and a CSV per table in the curated container (named by execution, so two runs never write the same files): `loc-text-bands` (Japanese titles against the control), `loc-text-by-title`, `loc-text-samples` (15 page ids per band, with their loc.gov links, for looking at the scans), `our-ocr-english` (by why LoC's text wasn't used), `our-ocr-by-title` and `our-ocr-samples`. It runs on the one-replica audit job, `caj-usnm-jaone-<env>`, which `start-quality.sh` picks for it. Should a second replica run it anyway (the ja-ocr job), one does the work, renewing a lock; the other waits for its finished marker (`mixed pages written by another replica`) and takes over if the lock goes stale, and a retried replica of a finished execution exits at once. `our-ocr-english` and `our-ocr-by-title` also give the median Latin share of our OCR's letters and the share of pages over half Latin.

## American Stories against LoC's text

`jaocr.py american-stories --year Y [--year …] [--sample-pct 10]` (`ja-ocr/american_stories.py`, #205) compares [American Stories](https://huggingface.co/datasets/dell-research-harvard/AmericanStories) (Dell et al. 2023, CC BY 4.0), a re-OCR of Chronicling America, with LoC's text on our pages. It streams each year's tarball from Hugging Face (0.4 to 7.6 GB; nothing is written to disk), joins scans to our pages by their file names, read in a first pass (a scan named `<date>_p<page>_<lccn>_<reel>_<date><ed>_<image>.json` is our `<lccn>_<date>_ed-<ed>_seq-<page>`; one with no page, `pNone`, about 3% of 1865, takes its image's place in the issue, a rule that agrees with 92.8% of 1865's named pages; a second copy of a page is skipped), keeps the audit's fixed sample (`quality.sampled`), and reads LoC's text for the sampled pages of those years from the batches whose dates overlap them. For pages of titles whose first language is English it reports the join, words per page, the audit's English damage rate and function-word share for each text, pages matching a fixed term list in either text, and LoC's damage rate by the share of regions American Stories marked Illegible.

```sh
scripts/ja-ocr/start-quality.sh prod american-stories --year 1865 --year 1925
scripts/ja-ocr/quality-rows.sh prod <execution>    # its "american stories" rows
```

It writes `audit/american-stories-<version>-<execution>.json`. It runs on the one-replica audit job, `caj-usnm-jaone-<env>` (`ja-ocr/solo.py` still keeps a second replica out, as for `jaocr.py mixed`). A failed run raises _Japanese OCR or audit job failed_ (severity 3), not the ingest alert.

### Indexing only where American Stories differs (`--diff`, #251)

`--diff` adds a measurement for indexing American Stories' text only where it differs from LoC's, to cut the second text field's cost to cold searches (#251). On the same pages it tokenizes both texts as the index does (`usnm_core::text`: hyphen rejoining, ligatures, split on what isn't a letter or digit, ASCII folding, tokens up to 40 characters), aligns them (a patience diff: tokens unique to both texts as anchors, `difflib.SequenceMatcher` with `autojunk=False` on small gaps without one), and builds each page's delta for k = 0, 2, 3 and 5: every region where American Stories' tokens differ from LoC's, with k of its tokens each side (where LoC has tokens American Stories hasn't, just the k each side), overlapping or touching windows merged. A term in American Stories' text is in LoC's or in the delta, and a phrase of n words that matches only in American Stories' text is inside one window once k is at least n - 1; the run checks both. New `american stories` tables:

- `diff_summary` (per year and k): American Stories', LoC's and the delta's tokens, the delta's share of American Stories' tokens (in total, and the median and 90th percentile per page), unique terms per page summed for each text and the delta (postings grow with them), `terms_lost` (should be 0), every page's distinct bigrams and trigrams that match only in American Stories' text and how many the delta keeps (`ngram2_loss`, `ngram3_loss`), the same for those whose words are all in LoC's text (`ngram*_edge*`: together only in American Stories'), n-grams that would match falsely if the windows ran together with no position gap (`ngram*_spurious_if_joined`), and `secs_per_1000_pages` for the diff.
- `diff_phrase` (per year, for the term list and 10 common phrases such as "new york" and "cross of gold") and `diff_phrase_group` (per year, for those two groups and the bigrams and trigrams of the run's `only_american_stories` contexts): pages matching in LoC's text, in American Stories', only in American Stories' (`only_words_all_in_loc` of those), and per k the ones the delta keeps (`kept_k*`, `loss_k*`, `edge_kept_k*`, `spurious_if_joined_k*`).
- `diff_edge_examples`: up to 10 pages a year with a phrase whose words are all in LoC's text but together only in American Stories', with loc.gov links.

```sh
scripts/ja-ocr/start-quality.sh prod american-stories --year 1865 --year 1925 --diff
```

The diff runs after the comparison and takes about 40 s per 1,000 pages of 8,000 tokens (measured on synthetic pages; the run logs its own figure in `american stories diff year`).

## American Stories' text for our pages

`jaocr.py american-stories-write [--year Y …]` (`ja-ocr/american_stories_write.py`, #218) writes American Stories' text (Dell et al. 2023, CC BY 4.0) for our pages to the curated container, for the release to index beside LoC's. It streams each year's tarball from Hugging Face once, places every scan on our page as `american-stories` does, and writes `american-stories/pages/<lccn>/<year>-<nnn>.parquet`: one row per page with `doc_id`, `lccn`, `date`, `text` (each article's headline, byline and text in American Stories' order, separated by blank lines), `articles` (JSON: each article's headline, byline, `start`/`end` in `text`, bounding boxes in scan pixels and the legibility of its regions), `legibility` (the page's text regions by legibility) and the scan's `width` and `height`. A page's second copy is skipped. Each year ends with a marker `american-stories/years/<year>.json` holding its counts; a rerun skips marked years, so an interrupted run resumes. A download that goes 5 minutes without data, or breaks off, fails and the year starts again from the top (up to 3 tries, a minute apart, each logged as `american stories year failed; retrying`); a retry writes the same parts under the same names. A year counts as read only when the gzip stream reached its end marker and, when Hugging Face sends one, the byte count matches `Content-Length`, so a download that stops at a file boundary inside the tarball is retried rather than marked. All 180 years are about 340 GB to stream.

```sh
scripts/ja-ocr/start-quality.sh prod american-stories-write          # every year
scripts/ja-ocr/start-quality.sh prod american-stories-write --year 1865
```

It runs on the one-replica job and logs `american stories year written` per year. `jaocr.py american-stories` also lists, per term and year, up to 8 pages that match only in American Stories' text with the words around the match (`only_american_stories`), for checking those matches by hand.

## Search log

The API keeps every search from the site, with only its filters, page count and UTC day, in the `searches` container ([ADR-0012](design/adr/0012-anonymous-search-log.md), 06 §6.8). Nothing expires `searches/days/` and `searches/import/`; staged batches are deleted after 7 days and stay in soft delete for 14 more. To read it:

```sh
scripts/settings.sh prod USNM_SEARCH_LOG_READERS "$(az ad signed-in-user show --query id -o tsv)"
scripts/provision.sh prod
scripts/searches.sh prod 30      # searches per day, top queries, queries that found nothing
```

The data account is reachable only through its private endpoint, so run `searches.sh` from a network that reaches it. The first day file appears an hour after the first full UTC day with the log deployed.

## Archival storage

Production keeps no batch archives (ADR-0006): LoC is the source of record. The archival account keeps everything we download from outside Azure, so re-curation, benchmarks and other environments never download it again: LoC's batch archives and batch lists now, other sources later. Benchmarks are one use.

**The account** (`infra/archive/`) is its own deployment stack, `usnm-archive`, in its own group, `rg-usnm-archive`, outside every environment's stack. An environment's provision or teardown can't delete it. It is StorageV2, Entra only (no keys, no SAS), with no public network access, versioning, 14-day soft delete, a lifecycle rule that moves `raw/` and `sets/` to the Cold tier, and a `CanNotDelete` lock. Its stack detaches what leaves the template rather than deleting it, and denies deletes outside the stack except of the lock and of private endpoint connections. Deploy it once:

```sh
scripts/archive-store.sh deploy --subscription <id>     # prints the account's resource id
```

**An environment uses it** with `USNM_ARCHIVE_ACCOUNT` set to that resource id and a provision. That adds a private endpoint in the environment's VNet (`pe-usnm-archive-blob`, in its Blob private DNS zone) and, in the account's group, a custom role on `raw` for `id-usnm-ingest` that lists, reads and creates blobs but can't delete them (`usnm archive writer`). The ingest and backfill jobs get `USNM_RAW_URL` pointing at `raw`. Clearing the setting removes only the environment's endpoint and roles. The account's lock can block removing an endpoint to it, so drop the setting (or tear the environment down) this way. `scripts/teardown.sh` refuses while the setting is on.

```sh
scripts/archive-store.sh unlock
scripts/settings.sh dev USNM_ARCHIVE_ACCOUNT ""
scripts/provision.sh dev
scripts/archive-store.sh deploy        # the lock back
```

**What curation does with it:**

- **Every archive it downloads is kept** byte for byte as fetched, at `raw/{batch}/{archive file}`. It is written in 8 MiB blocks as it streams (never held whole) at the Cold tier. Once the archive's sha256 checks out, the upload is committed and `raw/{batch}/manifest.json` records the source URL, bytes, sha256, the time and LoC's response headers (`last-modified`, `etag`, `content-length`, `content-type`). Only then is the batch marked curated. A download that fails its checksum leaves nothing. The commit is create-only: an archive already at the path (another environment's, sharing the account) is never replaced, and the batch curates from its own download with a warning if the two differ.
- **A batch already kept is curated from the copy,** not LoC, when its manifest's sha256 is the one LoC lists (or LoC lists none). It needs no download slot, and the copy is checked again as it's read. The `archive source` line says which (`source: raw` or `loc`), and `archive retained` logs each new copy.
- **Batch lists are kept too:** each remote list an `enqueue` or `run` reads goes to `raw/listings/{time}-{sha}.json`. Titles-sync keeps the fields it uses from each title record in the environment's `reference/raw/titles.json`. Nothing else is downloaded from outside Azure.

To keep the archives of batches curated before, queue them again with `--force` (only curated or failed batches at the listed version; one being curated is left alone), then run the backfill job. A re-curation writes a new attempt and replaces the batch's curation when it commits.

Stopping a curate run mid-batch leaves its batches leased for up to 2 hours (the curate worker's lease, set in `crates/usnm-ingest/src/main.rs`; the 45-minute `BATCH_LIMIT` is a running batch's own limit): `--force` leaves a leased batch alone and a new run skips it until the lease runs out. A batch whose runs are stopped repeatedly can reach the attempt cap (5) and be marked failed at its next claim; `enqueue --force` resets its attempts. Found on the dev 1% run (October 2026): after a stopped run, 8 batches needed two more passes.

```sh
scripts/start-job.sh dev INGEST_JOB enqueue --batches "$B" --force
scripts/start-job.sh dev BACKFILL_JOB curate --max-runtime-secs 14400
```

**An environment's own `raw` container** (`USNM_RETAIN_RAW true`) does the same inside the environment's data account. It goes with the environment, and turning the setting off deletes it, so it suits a short trial. `USNM_ARCHIVE_ACCOUNT` takes precedence when both are set. To move archives kept there into the archival account, with both settings on, run the copy in the ingest job. It streams each archive, checks it against its manifest, and skips those already there. A batch for which the destination holds another archive is left as it is and fails the copy at the end:

```sh
scripts/start-job.sh dev INGEST_JOB archive-copy \
  --from "$(az storage account show -n "$(scripts/settings.sh dev STORAGE_ACCOUNT)" --query primaryEndpoints.blob -o tsv)raw" \
  --batches "$B"
```

Then clear `USNM_RETAIN_RAW` and provision. That deletes the environment's own container, so check the copy's `archives copied` line first.

**Cost** (East US 2 list prices, October 2026): Cold storage is $0.0036 per GB-month, so the 1% set's 23.8 GB is about $0.09 a month. Reading it all back once costs about $0.71 ($0.03 per GB retrieved), and writing it about $0.05 (about 2,900 blocks at $0.18 per 10,000 writes). Cold has a 90-day early-deletion minimum. Each environment's endpoint is about $7.30 a month ($0.01 an hour).

### Sample sets

A set packages a sample of the corpus for reuse in `sets/{name}/`, built once and never replaced (build `-v2` instead). The builder comes with the search cluster experiment's tooling (#240); this account holds the container and the layout below is what it writes. A build first claims the name with `building.json`, so two builds never write the same objects. A build that fails keeps its name, so the retry is the next version:

- `raw.tar`: the set's LoC batch archives as kept in `raw/`, with their manifests. It's a plain tar, since the archives are bzip2 already. Curate it offline, or extract it into another raw store.
- `docs.ndjson.zst`: the sample's documents exactly as the release builds them (`text`, `text_cg`, every field; `text_as` and `text_as_cg` only when the manifest says `american_stories`). It's one zstd stream, so it can be loaded into any Quickwit 0.9 index created from `infra/quickwit/pages-index.yaml`.
- `manifest.json`, written last: name and version, how the batches were chosen, the batch list with each archive's bytes and sha256, page and document counts, each file's bytes and sha256, the corpus bounds, the common-word pairs' and American Stories' versions, the index config's sha256, the builder's commit and `created_at`.

**From a container or a laptop with access** (Storage Blob Data Reader on `sets`, and a network path to the account: an environment's VNet, or a private endpoint of your own; public access is off):

```sh
az storage blob download --auth-mode login --account-name <archive account> -c sets -n loc-1pct-v1/docs.ndjson.zst -f docs.ndjson.zst
zstd -dc docs.ndjson.zst | split -C 8m - chunk-        # request bodies under Quickwit's 10 MiB limit
for f in chunk-*; do curl -sf -XPOST "$QW/api/v1/$INDEX/ingest" --data-binary @"$f"; done
```

Check each file against the manifest's sha256 (`shasum -a 256`). `tar -xf raw.tar` gives `{batch}/{archive}` and `{batch}/manifest.json`, the layout of `raw/`.

## On-prem OCR nodes

Local machines re-read badly OCR'd pages (#296, [ADR-0015](design/adr/0015-onprem-ocr-workers.md)). Each node is an Azure Arc-enabled server running AlmaLinux 9 (RHEL 10 is also accepted), with a managed identity and no stored secret. It takes pages from Storage queues in the exchange account `stusnmo<suffix>` and writes its output to that account's `ocr-out` container, which a cloud job copies into `curated`. This section sets up the account, connects and monitors a node, and starts the worker ([`crates/usnm-ocr-worker`](../crates/usnm-ocr-worker/README.md)).

**The exchange account.** Set the node IPs (the home connection's public IPv4 address; a comma-separated list or CIDR ranges also work) and turn it on. The IP lives only in the environment's settings, never in the repository. The provision also adds a Microsoft.Storage service endpoint to `snet-cae`, which closes open connections from the subnet to storage public endpoints (the data lake is reached through its private endpoint and isn't affected), so run it while no ingest job is running.

```sh
scripts/settings.sh prod USNM_OCR_NODE_IPS 203.0.113.7    # the home IP
scripts/settings.sh prod USNM_OCR_NODES true
scripts/ocr-node/group.sh --env prod --dry-run
scripts/ocr-node/group.sh --env prod                      # writes USNM_OCR_NODE_GROUP
scripts/provision.sh prod                                 # writes OCR_EXCHANGE_ACCOUNT, OCR_NODE_DCR_ID and OCR_NODE_APPINSIGHTS, grants the group
```

**The nodes' group.** The nodes get their roles through an Entra security group, `usnm-ocr-nodes-<env>`: the provision grants the group Message Processor on `ocr-priority` and `ocr-bulk`, Message Sender on `ocr-deadletter`, a custom writer role on `ocr-out` (list, read and create blobs, no delete) and Monitoring Metrics Publisher on the nodes' own Application Insights component, `usnm-ocr-appi-<env>` (the worker's telemetry), and nothing else; nothing on the environment's `appi-usnm-<env>`. Each node's Arc identity is a member. `group.sh` creates the group once per environment, saves its object id in `USNM_OCR_NODE_GROUP` and makes you an owner so you can add and remove members. Run again, it uses the saved id and lists the group's owners. It never adopts a group by name: if one named `usnm-ocr-nodes-<env>` exists and no id is saved, it stops, so you can check that group's members and owners (`az ad group member list --group <id>`, `az ad group owner list --group <id>`) before saving its id by hand. The provision grants every member the node roles, and owners can add members. Creating it needs the right to create security groups (the default user setting, or Groups Administrator). The deployment stack can't create it (ADR-0015), and teardown leaves it: delete it by its object id, the value of `USNM_OCR_NODE_GROUP` (`az ad group delete --group <id>`; teardown keeps the settings file, renamed), not by its name.

**Create the VM.** On a Proxmox host, `scripts/ocr-node/proxmox-create-vm.sh` (copy it to the host, run it as root) creates an AlmaLinux 9 VM from AlmaLinux's GenericCloud image, checking its published sha256. The VM boots UEFI with Secure Boot (Microsoft's keys pre-enrolled; AlmaLinux's shim is signed) and has a vTPM, CPU type `host` (so the OCR engines can use AVX2), the QEMU guest agent, DHCP and start at boot. cloud-init creates an admin user `claude` with key-only SSH and passwordless sudo. On the first try cloud-init left the hostname as `localhost`; the script now sets `fqdn` and runs `hostnamectl set-hostname`. It prints the VM's address once the guest agent reports one.

```sh
bash proxmox-create-vm.sh --vmid 120 --name usnm-ocr-node1 --ssh-key "$(cat ~/.ssh/id_ed25519.pub)"
# optional: --cores 4 --memory-mib 8192 --disk-gib 64 --storage local-lvm --bridge vmbr0 --snippets-storage local
```

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

`onboard.sh` registers the `Microsoft.HybridCompute`, `Microsoft.GuestConfiguration` and `Microsoft.HybridConnectivity` providers. Unless the node is already connected, it then creates a throwaway app registration and service principal, `usnm-arc-onboard-<name>-<UTC time>`, with a client secret valid for 2 hours and only Azure Connected Machine Onboarding on the environment's resource group, and waits for the role assignment to show up. Then it runs `scripts/ocr-node/node-setup.sh` on the node with sudo. That script installs python3, podman, chrony, cron (for the agent's automatic upgrades), rsyslog and which, and initscripts where the distribution has it (the monitoring agent's prerequisites), plus htop, iftop and screen from EPEL for watching the node (no OCR engine: EL9 has only Tesseract 4.1.1 and EPEL 9 none, so the engines come later as pinned container images or a source build); creates the `usnm` user; installs the Connected Machine agent; sets its extension allow list to the Azure Monitor Linux Agent alone (with extensions off while a different list is replaced), reads it back, stops if any other extension is installed in `/var/lib/waagent` (the allow list doesn't remove one installed before it, and turning the extension manager on would run it again), and only then turns the extension manager on (an empty list allows every extension), and turns off guest configuration and incoming connections; keeps the journal on disk (`/etc/systemd/journald.conf.d/50-usnm.conf`: `Storage=persistent`, `SystemMaxUse=1G`; the cloud image keeps it in memory only, so a reboot lost the logs); sets sshd to offer post-quantum key exchange first where its OpenSSH supports it (a drop-in, `/etc/ssh/sshd_config.d/40-pq-kex.conf`; AlmaLinux 9.8's default crypto policy leaves it out); connects with that service principal (the tenant, app id and secret go over ssh's stdin, never on a command line; the secret waits in a root-only file under `/run` while `azcmagent connect --config` reads it, and a refused connect is retried for a few minutes while the new principal propagates); writes the worker's settings to `/etc/usnm/ocr-worker.env` (the exchange account's queue and `ocr-out` URLs, the node id, `--concurrency` of one page per CPU and the connection string of the nodes' Application Insights component, which `onboard.sh` passes from `OCR_NODE_APPINSIGHTS`; root's, mode 0644, nothing secret: with local auth off, ingestion needs an Entra token too); and installs `usnm-ocr-worker.service`, which starts only once `/usr/local/bin/usnm-ocr-worker` exists. It ends by checking that `usnm` can get a token (for Resource Manager, not storage: see below), and prints `azcmagent show` and `azcmagent config list`. Run again, it skips what's done and applies the lockdown, the settings and the unit as they are now (restarting a running worker if they changed). `onboard.sh` then waits for the machine's system-assigned identity and adds it to the nodes' group unless it's already a member, which gives the node its roles; no provision is needed. The machine identity's private key can live in the node's TPM (`--identity-key-store tpm`, the agent's TPM-backed identity, in preview; `proxmox-create-vm.sh` gives VMs a vTPM 2.0), so it can't be copied off the disk. Azure accepts that only once the subscription's `Microsoft.HybridCompute/HybridComputePreview` feature is Registered, which needs Microsoft's approval: `az feature register --namespace Microsoft.HybridCompute --name HybridComputePreview` leaves it Pending until then (requested 2026-10-10). `onboard.sh` checks the feature and asks `node-setup.sh` for the TPM only when it's Registered; otherwise the key stays in the agent's default store on disk. The key store is fixed at connect: moving an existing node to the TPM means `offboard.sh` and `onboard.sh` again, which makes a new machine and identity. A vTPM's state is a file on the Proxmox host, so this protects against the VM's disk or a backup of it, not against the host. It also turns on Microsoft's automatic agent upgrade for the machine (in preview: the agent upgrades itself to within one version of the latest, in batches at off-peak hours, scheduled with cron), which `node-setup.sh` already asks for at connect with `--enable-automatic-upgrade`; the property is checked so a machine connected earlier gets it too. Last, it stops if the machine has any extension in Azure other than the Azure Monitor Linux Agent (delete it, then run again), associates the machine with the nodes' data collection rule (`OCR_NODE_DCR_ID`), waits until the machine reports the allow list to Azure (a few minutes after a change), and installs the Azure Monitor Linux Agent extension with automatic upgrades on, waiting until it's installed (an agent already installed with automatic upgrades off gets them turned on). As soon as `node-setup.sh` has finished, since the connect was its only use, or when `onboard.sh` stops before that, on failure or Ctrl-C too, it deletes the role assignment, the service principal and the app registration, purges them from Entra's deleted items and prints what it deleted; a warning names anything it couldn't delete, to remove by hand.

**A node connected before monitoring** (connected with extensions off): provision once, so the nodes' workspace, component and rule exist and the group has Monitoring Metrics Publisher, then run `onboard.sh` again. It needs no onboarding credential for a connected node, re-runs `node-setup.sh` (which applies the allow list) and installs the agent.

**Monitoring.** The nodes have a workspace of their own, `usnm-ocr-log-<env>`, with its own daily cap of 0.1 GB, and an Application Insights component on it, `usnm-ocr-appi-<env>` (`infra/modules/ocr-node-monitoring.bicep`). The nodes' group can write telemetry to that component only, so a node, or anyone with a node's token, can neither write rows the API alerts read nor fill the environment's 1 GB cap; at worst it fills the nodes' cap, which stops the nodes' monitoring until the reset (ADR-0015). The rule, `usnm-ocr-dcr-<env>`, collects into the nodes' workspace: syslog at Warning and above from the system's facilities (kernel OOM kills, systemd, sshd, chronyd, podman containers' stderr); the worker's own lines at Info and above, from `local0` (its unit's `SyslogFacility`), since journald records every line the worker writes at info, warnings and errors included; and every 60 s, CPU per core, memory used, free space and read and write bytes per file system, and network bytes per interface. The agent also sends a heartbeat a minute (free). The worker's traces and metrics go to the nodes' component as role `usnm-ocr-worker`, instance the node name (see the worker's README). The exchange account's queue logs (`StorageQueueLogs`, a row for every queue request) and the rule's errors (`DCRLogErrors`) go to the nodes' workspace too. The estimate is 8 to 25 MB a day per node before the worker's telemetry, which isn't measured yet; measure it after a day and raise the cap if needed (`dailyCapGb` of the `ocr-node-monitoring` module in `infra/main.bicep`, and `cap_gb` in `ops/queries/ocr-node-health.kql`, then provision). With alert emails set:

- _OCR node log volume high_ (severity 2) fires when a node sends more than a tenth of the cap (10 MB) in an hour, through the agent (Syslog and Perf) and its Application Insights telemetry together, so a crash loop or debug logging is caught before one node fills the cap.
- _OCR nodes' daily cap reached_ (severity 2) fires on the nodes' workspace's `OverQuota` event, as the environment's cap alert does (#279). Until the reset, the node alerts and queries see nothing. In the nodes' workspace, `Usage | where IsBillable | summarize sum(Quantity) by DataType, bin(TimeGenerated, 1h)` shows what filled it.

`scripts/logs.sh` runs the `ocr-node-*` queries against the nodes' workspace (`OCR_NODE_LOG_ANALYTICS_WORKSPACE_ID`):

```sh
scripts/logs.sh prod ocr-node-health      # per node: heartbeat, counters, CPU, memory, free space on /, worker lines, PerfMB, SyslogMB and WorkerMB; DayMB and CapPct for the workspace
scripts/logs.sh prod ocr-node-errors 2d   # worker warnings and errors, panics, failed units, OOM kills, other errors
```

`no heartbeat` means the node is down or off the network, or has lost its agent or its association with the rule (run `onboard.sh` again): the agent sends heartbeats only to the workspaces of the rules it's associated with. `no counters` with heartbeats means the agent is associated but its counters aren't arriving: check the rule's counters and the agent's logs under `/var/opt/microsoft/azuremonitoragent/log`. A node with no heartbeat in the whole span isn't listed at all, so widen the span (`scripts/logs.sh prod ocr-node-health 7d`) or check the Arc machine's status.

**Start the worker.** Build `usnm-ocr-worker` for EL9 ([its README](../crates/usnm-ocr-worker/README.md#building-for-almalinux-9)), copy it to the node and, as root:

```sh
install -m 0755 usnm-ocr-worker /usr/local/bin/
systemctl enable --now usnm-ocr-worker
journalctl -u usnm-ocr-worker -f
```

The unit runs `usnm-ocr-worker run --queue-url https://<exchange>.queue.core.windows.net --out-url https://<exchange>.blob.core.windows.net/ocr-out --node-id <name> --shutdown-grace-secs 60` plus `USNM_OCR_ARGS` (default `--concurrency <CPUs>`), as `usnm`, with the agent's identity. On stop, the worker gets 120 s: in-flight pages get 60 s, telemetry up to 30 s more. `node-setup.sh` rewrites `/etc/usnm/ocr-worker.env` on every run, so put local changes in `/etc/usnm/ocr-worker.local.env`, read after it, for example `USNM_OCR_ARGS="--concurrency 3 --api-gap-ms 7000 --tile-gap-ms 2000"` with two nodes sharing loc.gov's rate, or `RUST_LOG=info,usnm_ocr_worker=debug` (the worker's own debug lines; a bare `RUST_LOG=debug` turns on every library's too, and every line goes to the nodes' workspace twice, through syslog and Application Insights, against its cap, so only for a while); then `systemctl restart usnm-ocr-worker`.

**When the home IP changes,** the nodes lose the exchange account until the firewall has the new address. Hidden messages come back on their own once their time runs out.

```sh
scripts/settings.sh prod USNM_OCR_NODE_IPS <new IP>
scripts/provision.sh prod
```

**Group changes take a while.** Membership is a claim in the identity's access token, and managed identity tokens are cached per resource for around 24 hours; Microsoft says a membership change can take several hours to apply, and a refresh can't be forced ([Microsoft](https://learn.microsoft.com/en-us/entra/identity/managed-identities-azure-resources/managed-identity-best-practice-recommendations#limitation-of-using-managed-identities-for-authorization)). So a node that fetched a storage token before it joined the group is refused by the exchange account until that token expires (that's why `node-setup.sh` checks with a Resource Manager token), and a removed node keeps its access for up to about a day on a token it already holds.

**Remove a node** with no provision:

```sh
scripts/ocr-node/offboard.sh --env prod --name usnm-ocr-node1 --host claude@node1.example.org --dry-run
scripts/ocr-node/offboard.sh --env prod --name usnm-ocr-node1 --host claude@node1.example.org
```

It removes the machine's identity from the nodes' group and deletes its data collection rule associations. With `--host`, it deletes the Azure Monitor Agent extension and waits while the node uninstalls it, then on the node stops and disables `usnm-ocr-worker.service`, removes any extension left (`azcmagent extension remove --all`) and runs `azcmagent disconnect --force-local-only`, which clears the agent's local state without an Azure sign-in, so no onboarding credential is needed. Last, it deletes the Arc machine with your `az` sign-in, which deletes its identity so it can get no new token. Without `--host` (a node that's gone or unreachable) it leaves the extension to go with the machine and prints the node commands to run later. It needs ownership of the nodes' group and the right to delete resources in the environment's resource group. To cut a node off at once, rather than when its tokens expire, also take its address out of `USNM_OCR_NODE_IPS` and provision; with both nodes behind the same home IP, that cuts off every node. Offboard every node before turning `USNM_OCR_NODES` off. The provision then deletes the exchange account with its queues and `ocr-out`, and the nodes' monitoring: their workspace with its data, their Application Insights component with the group's Monitoring Metrics Publisher grant, the data collection rule and the two node alerts. Azure won't delete a data collection rule that still has associations: with a node still associated, the provision fails until its association is gone. `offboard.sh` deletes the machine's associations. The provision stops before it starts if the rule in `OCR_NODE_DCR_ID` still has any. The Arc machines stay in the resource group, and their identities in the nodes' group, until offboarded.
