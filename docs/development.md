# Development

How to run and test US News Map locally. Nothing here needs an Azure account or the real newspaper data: the API serves a small **synthetic** corpus from [`fixtures/`](../fixtures/README.md), and local stand-ins cover the Azure code paths.

## Run the API and the site

Requires a stable Rust toolchain (see `rust-toolchain.toml`). The web app also needs Node.js 24.

```sh
cargo test --workspace                 # unit + API integration tests against the fixtures
cargo run -p usnm-api                  # serves the synthetic fixture corpus on :8080
curl 'localhost:8080/v1/aggregate?q=%22cross+of+gold%22&from=1896-06-01&to=1896-12-31'
curl 'localhost:8080/v1/hits?q=%22cross+of+gold%22&place=P00001&limit=5'
curl 'localhost:8080/v1/days?q=%22cross+of+gold%22&place=P00001,P00006'
```

With the API running, `cd web && npm ci && npm run dev` serves the site on `http://localhost:5173` (see [`web/README.md`](../web/README.md)).

## Against Quickwit

The same fixtures can be served by [Quickwit 0.9.1](https://github.com/quickwit-oss/quickwit/releases/tag/v0.9.1), the pinned version, set up the way production runs it. `scripts/quickwit-fixtures.sh` loads the fixture indexes with a writer node, stops it, and starts a read-only searcher that polls the metastore:

```sh
url=$(QUICKWIT_BIN=/path/to/quickwit scripts/quickwit-fixtures.sh --workdir /tmp/qw)
QUICKWIT_URL=$url cargo test -p usnm-search --test quickwit_parity   # Quickwit vs the reference backend
USNM_BACKEND=quickwit USNM_QUICKWIT_URL=$url cargo run -p usnm-api
kill "$(cat /tmp/qw/quickwit.pid)"
```

Without `QUICKWIT_URL`, the parity tests skip; CI runs them in the `quickwit` job. Quickwit has no fuzzy term queries, so on this backend `/v1/meta` reports `"fuzzy": false` and fuzzy queries return 422 (05 §5.5.1).

## Ingest pipeline

`usnm-ingest` turns LoC batch archives into a published version: `enqueue` records a batch list, `curate` claims queued batches and writes curated Parquet, and `release` builds a new index (a delta, or a base with `--full`) and its reference snapshot, then writes `current.json` last. `run` does all three. To try it on the synthetic corpus, rebuilt as two batch archives:

```sh
repo=$PWD                                                       # run from the repository root
python3 scripts/fixture-batches.py --out-dir /tmp/usnm-ingest   # archives, batches.json, reference/catalog/
cd /tmp/usnm-ingest
cargo run --manifest-path "$repo/Cargo.toml" -p usnm-ingest -- \
  --state-file state.json --curated curated --reference reference \
  run --list batches.json --synthetic --index-dir reference/indexes
USNM_DATA_DIR=/tmp/usnm-ingest/reference USNM_STATE_FILE=/tmp/usnm-ingest/state.json \
  cargo run --manifest-path "$repo/Cargo.toml" -p usnm-api
```

With `USNM_STATE_FILE`, the API's `/v1/status` (and the site's `/datasets` page) reads the local pipeline state; in Azure it reads Cosmos (`USNM_COSMOS_ENDPOINT`). To index into Quickwit instead, replace `--index-dir …` with `--quickwit-bin /path/to/quickwit --quickwit-metastore file:///tmp/usnm-ingest/qw --quickwit-index-root file:///tmp/usnm-ingest/qw`; the pipeline runs its own writer node for the release.

In Azure the same binary runs from the `usnewsmap-ingest` image with `--cosmos https://{account}.documents.azure.com/`, Blob URLs for `--curated` and `--reference`, and `--quickwit-metastore azure://qw-index --quickwit-index-root azure://qw-index`. The store, state and index-target options also read environment variables (`USNM_CURATED_URL`, `USNM_REFERENCE_URL`, `USNM_COSMOS_ENDPOINT`, `USNM_QUICKWIT_*`, …; see `usnm-ingest --help`). `enqueue` with no `--list` reads LoC's own batch listing; add `--batches name_ver01,…` to take only some.

## Against local Azure stand-ins

`scripts/local-azure/` runs the Blob and Cosmos code paths without an Azure account. That includes Entra tokens, conditional Cosmos writes, and several `curate` workers sharing one queue, which the `--state-file` store can't do because it serves one process at a time. It starts:

- [Azurite](https://github.com/Azure/Azurite) for Blob Storage, in OAuth mode, so it checks each token's audience, issuer and expiry
- `shim.py`: a managed identity endpoint, and a plain-HTTP proxy in front of Azurite's HTTPS port (the Rust clients only trust public CAs)
- `cosmos.py`: the Cosmos DB REST calls `CosmosDocs` makes, with etags, 409/412, query paging and session tokens. It only accepts Entra tokens for Cosmos. It follows our reading of the REST API, so the first run against a real account is still the real test.

It needs `azurite-blob` on `PATH` (`npm install -g azurite`), Python 3 and `openssl`.

```sh
. "$(scripts/local-azure/up.sh --workdir /tmp/usnm-azure)"     # starts everything, exports the USNM_* and IDENTITY_* variables
cargo build --release -p usnm-ingest -p usnm-api
ingest=target/release/usnm-ingest
$ingest enqueue --batches dlc_zurich_ver04,dlc_misctopsn83025894_ver01,dlc_misctopsn83021129_ver01
$ingest curate & $ingest curate & wait                  # two workers, one queue
$ingest titles-sync --lccns sn85042252,sn83025894,sn83021129
$ingest release --index-dir /tmp/usnm-azure/indexes
USNM_DATA_DIR=/tmp/usnm-azure target/release/usnm-api   # reads the snapshot from Azurite, caches responses there
scripts/local-azure/down.sh --workdir /tmp/usnm-azure   # data stays; up.sh on the same directory resumes
```

`$ingest duplicates` reports the pages that ship in more than one curated batch (JSON on stdout; changes nothing). `titles-sync` with no `--lccns` or `--list` fetches every title in LoC's listing (hours); `run --batches …` syncs only the titles for those batches. Set these on `up.sh` to change the Cosmos stand-in: `COSMOS_PAGE=2` exercises query continuation, `COSMOS_429=0.1` throttles a tenth of requests, and `COSMOS_SEED=state.json` starts from a `--state-file` run.

## Checks

The formatting, lint, test and build commands CI runs are listed in [CONTRIBUTING.md](../CONTRIBUTING.md). The web app's end-to-end tests are described in [`web/README.md`](../web/README.md).

Run `npm run format` in `web/` before pushing. It runs Prettier (pinned in `web/package.json`) from the repository root, with the root's `.prettierrc.json` and `.prettierignore`: it formats the web app's TypeScript, JavaScript, CSS, HTML and JSON (not `package-lock.json`, build output or `web/src/examples.json`, which keeps each example's view on one line) and every Markdown file in the repository but the reports under `docs/reports/` (pandoc builds their PDFs, and sizes table columns from the Markdown's layout). In Markdown it keeps prose lines and code blocks as written. It leaves data JSON outside `web/`, YAML, Bicep, Rust and Python alone. `npm run format:check` is the check CI runs. Where Prettier's layout would change how a Markdown block renders, wrap the block in `<!-- prettier-ignore-start -->` and `<!-- prettier-ignore-end -->`.

### JSON data files

Each JSON data file in the repository has a spec and a check that CI runs:

| File                                             | Spec and check                                                                                                                                                                                                                                                           |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `catalog/overrides/*.json`                       | Parsed into `deny_unknown_fields` structs in usnm-ingest, with per-field audit checks and sorted keys (`cargo test -p usnm-ingest`)                                                                                                                                      |
| `ops/version-snapshots/*.json`                   | `ops/version-snapshots/schema.json`, checked by `scripts/check-version-snapshots.py` (CI job `ops`)                                                                                                                                                                      |
| `ops/index-history.json`                         | `ops/index-history.schema.json` and sorted keys at every level, checked by `scripts/check-index-history.py` (CI job `ops`); run it on `scripts/reconstruct-index-history.py`'s output before replacing the file                                                          |
| `web/src/examples.json`                          | `examples.test.ts` checks every entry has exactly the fields of `Example` (`web/src/examples.ts`) and a `view` of `ViewState` fields (`npm test`); the API's `prewarm::Example` rejects unknown fields (`cargo test -p usnm-api prewarm`)                                |
| `infra/env/*.json`                               | `infra/env/schema.json`, checked by `scripts/lib/env_settings.py --check` (CI job `infra`): only public settings the schema lists, typed, keys sorted, laid out as `json.dumps(indent=2)` writes it; the schema lists exactly the settings `infra/main.bicepparam` reads |
| `infra/workbooks/*.json`                         | `scripts/ci/check-workbooks.py` (CI job `infra`): each file `infra/modules/workbooks.bicep` loads parses, has `version` and `items`, and names the module's workspace placeholder instead of a resource id                                                               |
| `fixtures/data/**`, `fixtures/skew-vectors.json` | Regenerated and compared (`python3 fixtures/generate.py`, `cargo test -p usnm-core --test skew_vectors`)                                                                                                                                                                 |

`check-version-snapshots.py` and `check-index-history.py` need `fastjsonschema` (`scripts/ci/requirements-ops.txt`); `check-workbooks.py` and `scripts/lib/env_settings.py` use only the standard library, so `python3 scripts/ci/check-workbooks.py` and `python3 scripts/lib/env_settings.py --check` run as they are:

```sh
python3 -m venv /tmp/ops && /tmp/ops/bin/pip install --require-hashes --no-deps -r scripts/ci/requirements-ops.txt
/tmp/ops/bin/python scripts/check-version-snapshots.py
/tmp/ops/bin/python scripts/check-index-history.py
/tmp/ops/bin/python -m unittest scripts/test_check_version_snapshots.py scripts/test_check_index_history.py
```
