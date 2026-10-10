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
  --page sn83045462/1890-05-03/ed-1/seq-7 --batch nbu_gage_ver01 \
  --priority bulk --language deu

# OCR one image file and print the time taken (benchmarking on a node).
usnm-ocr-worker ocr-file --image page.jpg --language deu --out /tmp/ocr
```

`--queue-url` and `--out-url` also take a directory, so the whole loop runs locally with no Azure at all. Other `run` flags: `--api-gap-ms` (3500) and `--tile-gap-ms` (1000) set the gaps between loc.gov API and image requests, and `--shutdown-grace-secs` (60) sets how long in-flight pages get after SIGTERM. The Tesseract flags are `--tesseract-bin`, `--psm` (3), `--dpi` (300), `--threads-per-page` (1) and `--page-timeout-secs` (600).

## Queues

There are three Azure Storage Queues: `ocr-priority`, `ocr-bulk` and `ocr-deadletter`. The worker polls `ocr-priority` first. When both are empty it waits before polling again, doubling the wait from 1 s up to 60 s.

- A received message is hidden for 600 s and renewed every 180 s while its page is worked on. Each renewal (Update Message) returns a new pop receipt, and the final delete uses the latest one.
- A message received more than 5 times goes to `ocr-deadletter` and is deleted from its queue. So does a message that can never succeed: bad JSON, a page loc.gov lists no image for, or a language with no model on the node.
- A temporary failure (LoC or storage errors, a Tesseract crash or timeout) hides the message for 300 s and lets it come back.
- On SIGTERM the worker stops taking messages and gives in-flight pages the grace period. Pages still unfinished after that are made visible again (visibility 0).

The REST calls use API version 2023-11-03 and an Entra bearer token for `https://storage.azure.com/`. No keys or SAS tokens are used.

In directory mode, each queue is a directory with `ready/` and `leased/` subdirectories. A receive claims a message by renaming it, which is atomic, so two workers sharing the directory never take the same message.

### Message

```json
{"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-7","batch":"nbu_gage_ver01","language":"deu","reason":"bulk","score":0.62,"queued_at":"2026-10-10T12:00:00Z"}
```

`page` must be a canonical `PageKey`, `language` is ISO 639-3, `reason` is `user` or `bulk`, and `score` is optional (0 to 1). Unknown fields are refused, so a search query can't be carried in a message. On the queue the JSON is base64-encoded, the convention the portal, Storage Explorer and Functions follow. Raw JSON is accepted too.

## Identity

`usnm_store::credential::from_env` picks the Azure Arc agent's managed identity when `IDENTITY_ENDPOINT` and `IMDS_ENDPOINT` are set and `IDENTITY_HEADER` isn't, which is what the Connected Machine agent sets. The agent answers the first token request with 401 and `WWW-Authenticate: Basic realm=<file>`. The worker reads that file and sends its contents back as `Authorization: Basic <secret>`. It reads the file only if it is under `/var/opt/azcmagent/tokens/`, is named `*.key` and is at most 4 KiB, and it sends the secret only to a loopback endpoint. The service user must be in the `himds` group to read the file. Tokens are cached until five minutes before they expire, and tokens are never logged. Container Apps keeps its own identity path, which is unchanged.

## Fetching scans

The worker fetches scans the way `ja-ocr/jaocr.py` does. It makes one request per issue to `https://www.loc.gov/item/{lccn}/{date}/ed-{n}/?fo=json` and caches the result (256 issues, one hour). The page's JP2 URL gives its IIIF service on `tile.loc.gov`, and the scan is `<service>/full/full/0/default.jpg`. The API and image gaps are shared by every page in flight on the node. With two nodes, double both so the pair stays at the Japanese job's rate. A 429 or 503 holds every request for its `Retry-After` (at most an hour). The User-Agent is `usnewsmap-ocr-worker/<version> (+https://usnewsmap.com)`. The scan is held in memory and in a temporary directory that is removed after the page is done.

## Tesseract

Tesseract runs as a subprocess with `--psm 3 --dpi 300` and the `txt` and `alto` configs, so one pass writes both outputs. `OMP_THREAD_LIMIT` is set to `--threads-per-page`. The worker lists the installed models once at startup and routes each language to the first candidate whose models are all installed:

| Language            | Candidates                                      |
| ------------------- | ----------------------------------------------- |
| `deu`               | `deu_latf+deu`, `frk+deu`, `deu`                |
| `srp`               | `srp+srp_latn`, `srp`                           |
| `hrv`, `bos`        | `hrv`, `srp_latn`                               |
| `nob`, `nno`, `nor` | `nor`                                           |
| others              | the code itself (`eng`, `rus`, `ces`, `pol`, …) |

`ocr_engine` records the version and models used, for example `tesseract-5.5.1/deu_latf+deu`. Other engines can be added behind the `Engine` trait if the engine survey (#296) picks them.

## Output

Each page writes two objects to `--out-url`:

1. `alto/<doc_id>.xml`: Tesseract's ALTO, with `fileName` set to the scan's URL. It is written first and replaced if the page is read again.
2. `pages/<lccn>_<date>_ed-<n>.<node>.<UTC stamp>.parquet`: a create-only overlay part with one row. Its columns and types are the ones `ja-ocr/jaocr.py write_part` writes (`usnm_overlay::schema`), zstd-compressed, and the release's overlay reader (`usnm_ingest::ocr_ja::read_part`) reads it.

Values specific to this worker:

- `ocr_source` is `usnm-tesseract`, named after the engine as the Japanese job's `usnm-ndlocr-lite` is. The node appears in the part name.
- `loc_text` is the queue reason (`bulk` or `user`). The worker never sees LoC's text, so it can't say how that text failed. These values never match `missing`, so a release won't count the page as one curation never had.
- `text` is normalized with `usnm_core::text::normalize_ocr`, as LoC's text is at curation (04 §4.5). `text_status`, `text_chars` and `text_sha256` are computed from the normalized text.
- `image_url` is the IIIF service base, as in the Japanese overlay.

Each page gets its own part, so a node writes many small files. The cloud copy-in job should merge them into larger parts as it copies them to `curated/ocr-local/pages/`.

## Logs

Logs are JSON lines on stdout (journald under systemd), with the level set by `RUST_LOG` (default `info`). Each finished page logs one line with the page, queue, reason, engine, status, characters and seconds taken.

## Building for AlmaLinux 9

The binary uses rustls and has no OpenSSL dependency. It must be linked against a glibc no newer than EL9's 2.34, so don't build it on a newer distribution and copy it over. Either of these works:

```sh
# On the node, or in an EL9 container with podman:
podman run --rm -v "$PWD":/src:Z -w /src docker.io/library/almalinux:9 bash -c '
  dnf -y install gcc && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal &&
  ~/.cargo/bin/cargo build --release --locked -p usnm-ocr-worker'

# Or from a developer machine with cross (its default image has an older glibc):
cross build --release --locked --target x86_64-unknown-linux-gnu -p usnm-ocr-worker
```

The node also needs Tesseract and its language models (`dnf install tesseract tesseract-langpack-deu …`, or `.traineddata` files copied into the tessdata directory). EL9's packaged Tesseract may be older than 5. `ocr_engine` records whichever version runs.

## Tests

`cargo test -p usnm-ocr-worker` covers message parsing, the local queue, the Azure REST calls against a stand-in server, lease renewal, pacing and Retry-After, an end-to-end page whose part the ingest reader reads, dead-lettering, and shutdown. One test runs the real Tesseract on a rendered line when Tesseract and `eng` are installed, and skips otherwise. The Arc challenge is tested in `usnm-store` against a stand-in agent.
