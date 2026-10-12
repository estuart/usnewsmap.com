# usnm-ocr-worker

The on-prem OCR worker for [#296](https://github.com/tgoodyear/usnewsmap.com/issues/296). It runs on a local machine, takes one page at a time from a queue, fetches the page's scan from the Library of Congress, reads it with Tesseract, and writes ALTO and an overlay Parquet part to the exchange storage account. It keeps no LoC text and doesn't decide whether its text replaces LoC's. A cloud job checks the parts, and the release decides.

## Commands

All flags are named.

```sh
# Work until stopped (SIGTERM or Ctrl-C).
usnm-ocr-worker run \
  --queue-url https://<account>.queue.core.windows.net \
  --out-url https://<account>.blob.core.windows.net/ocr-out \
  --node-id nuc1 [--concurrency 2] [--once] [--engine tesseract]

# Queue one page (testing).
usnm-ocr-worker enqueue --queue-url <url|dir> \
  --page sn83045462/1890-05-03/ed-1/seq-7 --batch nbu_gage \
  --priority bulk --language deu

# OCR one image file and print the time taken (benchmarking on a node).
usnm-ocr-worker ocr-file --image page.jpg --language deu --out /tmp/ocr

# Read a rendered line of English with the engine; fails unless it comes back.
usnm-ocr-worker check-engine --tesseract-image sha256:<image id>

# The version and the commit CI built it from.
usnm-ocr-worker --version
```

## Prefetching scans

`prefetch` fetches the scans a manifest lists (`usnm-ingest ocr-prefetch-manifest`, #368) onto local disk with the same LoC client and pacing as `run`, so a node's engines can read them later without asking LoC again. It runs in the scan-fetch container on pve-big (`scripts/ocr-node/scanfetch-ct.sh`, 12 §12.2.4).

```sh
# Fetch every page, German before the 1920s first, stopping at 2.5 TB on disk.
usnm-ocr-worker prefetch --manifest /tank/ocr/manifests/<version>/manifest.parquet \
  --out /tank/ocr/scans --resolution full --max-total-bytes 2500000000000

# About 300 pages stratified by language and decade, scaled to 50%, then a projection.
usnm-ocr-worker prefetch --manifest <manifest> --out <dir> --sample 300 --resolution scaled:50 --buffer-resolution scaled:50
usnm-ocr-worker prefetch-estimate --manifest <manifest> --index <dir>/index-pct50.jsonl
```

Pages are in two sets. The keep set holds every page not detected as English and the worst English batches' pages, and is kept for good. The buffer set holds the rest of English, capped by `--buffer-cap` (1 TB): the fetcher waits at the cap until workers have emptied some of it. `--resolution` applies to the keep set and `--buffer-resolution` to the buffer set. `--sets keep|buffer|both` picks the sets. Scans go to `<out>/<set>/<batch>/<lccn>/<date>/<edition>/<seq>.jpg` (`<seq>.pct<n>.jpg` when scaled), each with a sidecar `<file>.json` holding the tile.loc.gov URL, the fetch time, the size and the sha256. A run skips scans that match their sidecar, so it resumes where it stopped and can be pointed at a newer manifest. Other flags: `--limit`, `--order priority|issue`, `--concurrency` (4; the pages share the gaps), `--lock-file` (default `<out>/.prefetch.lock`; one fetching process at a time), `--progress-every` (100), `--api-gap-ms` and `--tile-gap-ms`. It writes `status-<res>.json`, `index-<res>.jsonl` and `failures-<res>.jsonl` in `<out>`, logs progress as JSON lines, and stops after 20 pages fail in a row.

`run --scan-cache-url http://<host>:8380` asks the scan server (`serve-scans`) for a page's full scan, in `keep/` and then `buffer/`, before asking LoC, with a 2 s connect timeout and a 30 s total. It uses the scan when its sidecar names the page and the message's batch at full size, points at a tile.loc.gov IIIF image, and matches the bytes' size and sha256; a miss, a timeout, a 5xx or a mismatch goes to LoC at once. Every result's ALTO records the image URL in `fileName` and the sha256 of the bytes read in `fileIdentifier` (ADR-0015 decision 9). Once the output is uploaded, a buffer-set scan is deleted on the server, best effort.

`run --publish-fetched <manifest version>` (`USNM_OCR_PUBLISH_FETCHED`, set on one node) also relays the scan server's index to the seeder every `--publish-every-secs` (1800): it reads `/index/full.jsonl` as a stream, keeps the full-size scans' batch and page id, and writes them create-only to `ocr-out/fetched/<version>/<node>.<stamp>.parquet` (`usnm_ocr_contract::fetched`). The seeder's `fetched` source queues only pages on the newest list (12 Step 11). A failed publish is logged and tried again at the next turn. `publish-fetched --scan-cache-url --out-url --node-id --manifest` does it once.

`serve-scans --allow <node IPs> [--scans /tank/ocr/scans] [--manifests /tank/ocr/manifests] [--listen 0.0.0.0:8380]` serves `GET /scans/...` and the sidecars, `DELETE /scans/buffer/...` (renaming the sidecar to `<scan>.done`; `keep/` gets 403) and `PUT /manifests/<version>/{manifest.parquet,summary.json}`, to the allowed addresses only, with a log line per request.

`--queue-url` and `--out-url` also take a directory, so the whole loop runs locally with no Azure at all. Other `run` flags: `--api-gap-ms` (3500) and `--tile-gap-ms` (1000) set the gaps between loc.gov API and image requests, `--scan-cache-url` names the scan server to ask before LoC (see Prefetching scans), `--visibility-secs` (1800) sets how long a taken message stays hidden, and `--shutdown-grace-secs` (60) sets how long in-flight pages get after SIGTERM. The Tesseract flags are `--tesseract-bin` or `--tesseract-image` (with `--podman-bin`, and `--podman-url` with `--scratch-dir`, `--engine-memory-mib` (1536), `--engine-cpus` (1) and `--engine-pids` (64)), `--psm` (3), `--dpi` (300), `--threads-per-page` (1) and `--page-timeout-secs` (600, or `USNM_OCR_PAGE_TIMEOUT_SECS`).

These `run` flags can also come from the environment, which is how the node's unit sets them (`/etc/usnm/ocr-worker.env`, written by `scripts/ocr-node/install-worker.sh`): `USNM_OCR_QUEUE_URL`, `USNM_OCR_OUT_URL`, `USNM_OCR_NODE_ID`, `USNM_OCR_CONCURRENCY`, `USNM_OCR_API_GAP_MS`, `USNM_OCR_TILE_GAP_MS`, `USNM_OCR_SCAN_CACHE_URL`, `USNM_OCR_VISIBILITY_SECS`, and for the engine `USNM_OCR_TESSERACT_IMAGE`, `USNM_OCR_PODMAN_URL`, `USNM_OCR_SCRATCH_DIR`, `USNM_OCR_ENGINE_MEMORY_MIB`, `USNM_OCR_ENGINE_CPUS` and `USNM_OCR_ENGINE_PIDS` (the unit sets the socket and the scratch directory). A flag on the command line wins. `--shutdown-grace-secs` and `--once` are command-line only: the unit's `TimeoutStopSec` is set for the default grace.

## Queues

There are three Azure Storage Queues: `ocr-priority`, `ocr-bulk` and `ocr-deadletter`. The worker polls `ocr-priority` first. When both are empty it waits before polling again, doubling the wait from 1 s up to 60 s.

- A received message is hidden for `--visibility-secs` (1800 s), set when it is taken. The nodes can take, peek and delete messages but not update them ([ADR-0015](../../docs/design/adr/0015-onprem-ocr-workers.md)), so the hide is never renewed. A page not done by 80% of that time is abandoned with nothing written, so it reappears cleanly for another node. Keep `--page-timeout-secs` under that share; the worker warns at startup if it isn't.
- A message received more than 5 times goes to `ocr-deadletter` and is deleted from its queue. So does a message that can never succeed: one that fails its checks (bad JSON, say), or a page loc.gov lists no image for. The dead letter (version 2, `message::DeadLetter`) keeps the original queue, the message id, a `cause` (`dequeue_count`, `invalid`, `permanent`, or `unroutable` once the engine contract moves pages whose engines are down, with their ids in `engines`), for `invalid` the `kind` of check that failed, the error (at most 200 characters, no control characters), the dequeue count, the node and the time. It keeps the message itself only if the message passed its checks, re-encoded from the checked fields. A message that failed them might carry anything, so the dead letter records only the kind. Any node can put anything on `ocr-deadletter`, so `message::decode_dead_letter` reads one strictly (version 1 or 2, unknown fields refused, every field checked) for the cloud job that drains it. A `dequeue_count` dead letter still can't say why the earlier attempts failed; that is in `ocr.pages_failed` by `error` and in the `ocr_page` requests by message id.
- A temporary failure (LoC or storage errors, a Tesseract crash or timeout, or a language with no model on this node, which another node may have) leaves the message, which reappears when its time runs out. The worker asks the engine whether it can read the page's language before it fetches the scan, so a page with no model here costs loc.gov nothing.
- While loc.gov holds this node's requests for more than half a new page's budget (80% of `--visibility-secs`), the worker takes no messages: a page taken then would mostly wait in the pacer, and could be abandoned and spend one of its 5 receives.
- The output uploads are cut off at the same 80% mark, so a node never writes a page after its message may have gone to another node. A cut between the two uploads leaves an ALTO without its part, which does no harm because parts are only read with their ALTO.
- On SIGTERM the worker stops taking messages and gives in-flight pages the grace period. Pages still unfinished after that are abandoned and reappear when their time runs out. The systemd unit's `TimeoutStopSec` should be longer than the grace period plus the telemetry flush (up to 30 s); the node's unit (`scripts/ocr-node/usnm-ocr-worker.service`) keeps the default 60 s grace, sets `TimeoutStopSec=120` and `KillMode=mixed`, so only the worker gets the SIGTERM and Tesseract keeps running through the grace period.

The REST calls use API version 2023-11-03 and an Entra bearer token for `https://storage.azure.com/`. No keys or SAS tokens are used.

In directory mode, each queue is a directory with `ready/` and `leased/` subdirectories. A receive claims a message by renaming it, which is atomic, so two workers sharing the directory never take the same message.

### Message

```json
{"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-7","batch":"nbu_gage","language":"deu","reason":"bulk","score":0.62,"queued_at":"2026-10-10T12:00:00Z"}
```

`page` must be a canonical `PageKey`, `batch` has the shape of LoC's base batch names (the awardee's code, `_`, the batch's name, as in `nbu_gage`; no `_verNN`, since curated rows and the release use the base name and a versioned one would be skipped at release), `language` an ISO 639-3 code that `usnm_core::lang` knows (`deu`, not the catalog's `ger`), `reason` is `user` or `bulk`, and `score` is optional (0 to 1). Unknown fields are refused, so a search query can't be carried in a message. On the queue the JSON is base64-encoded, the convention the portal, Storage Explorer and Functions follow. Raw JSON is accepted too.

## Identity

`usnm_store::credential::from_env` picks the Azure Arc agent's managed identity when `IDENTITY_ENDPOINT` and `IMDS_ENDPOINT` are set and `IDENTITY_HEADER` isn't, which is what the Connected Machine agent sets. The agent answers the first token request with 401 and `WWW-Authenticate: Basic realm=<file>`. The worker reads that file and sends its contents back as `Authorization: Basic <secret>`. It reads the file only if it is under `/var/opt/azcmagent/tokens/`, is named `*.key` and is at most 4 KiB, and it sends the secret only to a loopback endpoint. The service user must be in the `himds` group to read the file. Tokens are cached until five minutes before they expire, and tokens are never logged. Container Apps keeps its own identity path, which is unchanged.

## Fetching scans

The worker fetches scans the way `ja-ocr/jaocr.py` does. It makes one request per issue to `https://www.loc.gov/item/{lccn}/{date}/ed-{n}/?fo=json` and caches the result (256 issues, one hour). The page's JP2 URL gives its IIIF service on `tile.loc.gov`, and the scan is `<service>/full/full/0/default.jpg`. The API and image gaps are shared by every page in flight on the node. With two nodes, double both so the pair stays at the Japanese job's rate. A 429, a 503 or a 403 (how a block looks) holds every request to that server for its `Retry-After`, or for 60 s times the attempt number if that is longer or none came, at most an hour; a longer `Retry-After` is cut to the hour and counted with `capped`, which a log alert on the nodes' workspace watches. A 408 is retried like a 5xx; other 4xx answers are permanent. Only an issue's answer is cached: a 4xx on its item JSON fails that page and is asked again for the next one. The User-Agent is `usnewsmap-ocr-worker/<version> (+https://usnewsmap.com)`. The scan is held in memory and in a temporary directory that is removed after the page is done.

## Tesseract

On a node, Tesseract runs from a pinned container image, `ocr/engines/tesseract/` (Debian's Tesseract 5.5.0 package on the engine survey's base, with tessdata_best's `eng`, `deu`, `deu_latf`, `srp`, `srp_latn`, `ces`, `pol` and `rus`, all Apache-2.0, and UB Mannheim's `frak2021` 0.905 from Zenodo record 10125246, CC0 1.0). `--tesseract-image` names it by image id or digest, never by tag, and the worker runs one container per page with podman: no network, a read-only root, all capabilities dropped, no privilege gain, and the page's scratch directory as `/work`. On a node, `--podman-url` points at the engine runtime of [ADR-0016](../../docs/design/adr/0016-pluggable-ocr-engines.md) (#307): the engine account `usnmeng`'s Podman API socket, which `scripts/ocr-node/node-setup.sh` sets up. The worker then never runs podman itself. Each container runs as the image's user 10001, a subordinate id of `usnmeng`, with memory equal to memory plus swap and CPU, pid, open-file and core limits, and its log driver off. The page's scratch directory goes on the tmpfs `--scratch-dir` names (the worker's own `/tmp` is private to its unit), with an ACL entry for that user and, under SELinux, a `container_file_t` label at an MCS level the container runs at. Containers aren't run with `--rm`: after one exits, the worker reads `OOMKilled`, so a page that ran out of memory says so, then removes it. A page that times out or is abandoned has its container removed, and the worker removes exited containers it left behind when it starts. The engine contract (#307) replaces the container per page with a long-lived instance per engine on the same socket. Without `--tesseract-image`, `--tesseract-bin` runs a Tesseract on the host (a developer machine).

Tesseract runs with `--psm 3 --dpi 300` and the `txt` and `alto` configs, so one pass writes both outputs. `OMP_THREAD_LIMIT` is set to `--threads-per-page`. The worker lists the installed models once at startup and routes each language to the first candidate whose models are all installed:

| Language            | Candidates                                      |
| ------------------- | ----------------------------------------------- |
| `deu`               | `frak2021`, `deu_latf+deu`, `deu`               |
| `srp`               | `srp+srp_latn`, `srp`                           |
| `hrv`, `bos`        | `hrv`, `srp_latn`                               |
| `nob`, `nno`, `nor` | `nor`                                           |
| others              | the code itself (`eng`, `rus`, `ces`, `pol`, …) |

`ocr_engine` is the tag of 13 §13.8.1 (`usnm_core::ocr_engine`): engine and version, the models read (`profile`), the first 12 hex digits of the image's id and of the models' sha256 (from `ocr/engines/tesseract/models.lock`, built into the worker), the route and the contract, for example `tesseract/5.5.0 profile=frak2021 image=sha256:3f9a0c1d2e4b models=sha256:8c2d71aa09fe route=minimal@r1 contract=0`. A host binary writes `image=local models=local`, which `ocr-sync` refuses. The route is hard-coded (`engine::candidates`): German to `frak2021`, else `deu_latf+deu`; other languages to their tessdata_best model; a language with no model isn't read. `engines.toml` and its gate (#371) replace it. Other engines can be added behind the `Engine` trait if the engine survey (#296) picks them.

## Output

Each page writes two objects to `--out-url`:

1. `alto/<doc_id>.<node>.<UTC stamp>.xml`: Tesseract's ALTO, with `fileName` set to the scan's URL. It is written first.
2. `pages/<lccn>_<date>_ed-<n>.<node>.<UTC stamp>.parquet`: an overlay part with one row, with the same stamp as its ALTO. Its columns and types are the ones `ja-ocr/jaocr.py write_part` writes (`usnm_overlay::schema`), zstd-compressed, and the release's overlay reader (`usnm_ingest::ocr_ja::read_part`) reads it.

Values specific to this worker:

- `ocr_source` is `usnm-tesseract`, named after the engine as the Japanese job's `usnm-ndlocr-lite` is. The node appears in the part name.
- `loc_text` is the queue reason (`bulk` or `user`). The worker never sees LoC's text, so it can't say how that text failed. These values never match `missing`, so a release won't count the page as one curation never had.
- `text` is normalized with `usnm_core::text::normalize_ocr`, as LoC's text is at curation (04 §4.5). `text_status`, `text_chars` and `text_sha256` are computed from the normalized text.
- `image_url` is the IIIF service base, as in the Japanese overlay.

Both writes are create-only, and the names are unique per node and moment: the nodes' role on `ocr-out` can create blobs but not replace or delete them. Each page gets its own part, so a node writes many small files. The cloud copy-in job should merge them into larger parts as it copies them to `curated/ocr-local/pages/`.

## Logs and telemetry

Logs are JSON lines on stdout (journald under systemd), with the level set by `RUST_LOG` (default `info`). Each finished page logs one line with the page, queue, reason, language, engine, status, characters, the ALTO and part names, and seconds taken.

When `APPLICATIONINSIGHTS_CONNECTION_STRING` is set, the worker also sends telemetry to Application Insights through `usnm-telemetry`, as `usnm-api` does. Unset or empty, it runs with stdout logs only. The cloud role is `usnm-ocr-worker` and the role instance is `--node-id`. Application Insights has local auth disabled, so every upload carries an Entra token for `https://monitor.azure.com/.default` from the node's Arc identity through HIMDS, as the queue and blob calls get theirs. The node's identity needs Monitoring Metrics Publisher on the component. On a node that is the nodes' own component, `usnm-ocr-appi-<env>`, on their own workspace, `usnm-ocr-log-<env>`: the template grants the nodes' Entra group the role there and nowhere else (`infra/modules/ocr-node-monitoring.bicep`, ADR-0015 decision 8), so a node can't write the environment's telemetry. `scripts/ocr-node/onboard.sh` passes that component's connection string (the setting `OCR_NODE_APPINSIGHTS`) to `scripts/ocr-node/node-setup.sh`, which writes it to `/etc/usnm/ocr-worker.env` for the systemd unit.

Each message is a request (`ocr_page`) carrying its page id, queue, reason and language, with an `outcome` of `done`, `failed`, `abandoned` or `dead_lettered`. The lines logged while it runs, at the `RUST_LOG` level, go with it as traces. Message bodies, search text and tokens are never recorded. A page abandoned at shutdown is counted, and its request has no `outcome`. The metrics:

| Metric                    | Attributes                                                       |
| ------------------------- | ---------------------------------------------------------------- |
| `ocr.pages_done`          | `engine`, `language`, `reason`, `queue`                          |
| `ocr.pages_failed`        | the same, and `error` (fetch, engine, no_model, output, scratch) |
| `ocr.pages_abandoned`     | `cause`: `budget` (with the page's attributes) or `shutdown`     |
| `ocr.dead_lettered`       | `queue`, `cause` (dequeue_count, invalid, permanent, unroutable) |
| `ocr.empty_polls`         | none: a poll that found both queues empty                        |
| `ocr.loc_slowdowns`       | `status` (429, 503, 403), `server`, `retry_after`, `capped`      |
| `ocr.ocr_seconds`         | histogram: `engine`, `models`, `language`                        |
| `ocr.image_fetch_seconds` | histogram: `outcome` (ok, error), `source` (loc, cache)          |

Metrics are sent every 30 s and once more at shutdown.

## Building for AlmaLinux 9

CI builds the worker on AlmaLinux 9 (`ocr-worker-el9` in `.github/workflows/ci.yml`), fails if it needs a glibc symbol newer than EL9's 2.34, runs it on AlmaLinux 10, and reads a rendered line through the Tesseract image with `check-engine`. On `main` it publishes the binary and the image as an attested GitHub release, `ocr-worker-<yyyymmdd>-<sha7>`, which `scripts/ocr-node/install-worker.sh` verifies and installs ([operations](../../docs/operations.md#on-prem-ocr-nodes)). The binary uses rustls and has no OpenSSL dependency. To build one by hand, use an EL9 container, not a newer distribution:

```sh
podman run --rm -v "$PWD":/src:Z -w /src docker.io/library/almalinux:9 bash -c '
  dnf -y install gcc && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal &&
  ~/.cargo/bin/cargo build --release --locked -p usnm-ocr-worker'
```

## Tests

`cargo test -p usnm-ocr-worker` covers message parsing (a catalog code such as `ger` is refused), each dead-letter cause and the strict dead-letter reader, pacing, Retry-After and 408 retries, a 403 that holds rather than dead-letters, a refused issue that isn't cached, a page with no model that makes no image request, a long hold that takes no messages, an end-to-end page whose part the ingest reader reads, dead-lettering (and that a refused message's content isn't kept), abandoning a page over its time, and shutdown. One test runs the real Tesseract on a rendered line when Tesseract and `eng` are installed, and skips otherwise. With no connection string, telemetry stays off. The queue client (local and Azure, with the approximate message count) is in `usnm-store` (`usnm_store::queue`) and tested there against a stand-in service. The Arc challenge, and its token for Azure Monitor, are tested in `usnm-store` against a stand-in agent.
