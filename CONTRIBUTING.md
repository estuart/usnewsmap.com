# Contributing

Issues and pull requests are welcome. For anything bigger than a small fix, open an issue first so the approach can be agreed before you write the code.

The [design documents](docs/design/README.md) explain how the system works and why. If a change alters a decision recorded there, update the document or the [ADR](docs/design/adr/README.md) in the same pull request. Work worth a write-up, a measurement, an experiment, a change whose numbers someone will want later, gets a [technical note](docs/notes/README.md).

## Run the checks

[`contrib-compute/`](contrib-compute/README.md) holds the shell, XML and Bicep that host a stock BOINC server; CI checks them when a pull request touches it (its README lists the checks), and it has no Rust. CI runs the Rust checks when a pull request touches Rust code and the web checks when it touches the web app (`scripts/ci/changes.sh`). A pull request that only changes documentation (Markdown anywhere, or anything under `docs/`) runs none of this workflow's checks beyond the `changes` job itself and, when it changes Markdown, the format check (`npm run format:check`); CodeQL (`.github/workflows/codeql.yml`) analyses only the languages a pull request touches (`scripts/ci/codeql-languages.sh`), so a change to only HTML, CSS, Markdown, Bicep or JSON data runs no analysis (`package.json`, lock files and `tsconfig*.json` count as JavaScript), and every language runs on main weekly. Run the ones that apply before you push:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

cd web
npm ci
npm audit --audit-level=high
npm run lint
npm run format:check
npm run typecheck
npm test
npm run build
```

The infrastructure checks, and the scan for shared keys, run on every pull request that changes anything other than Markdown or `docs/` (from the repository root):

```sh
scripts/ci/lint-bicep.sh
scripts/ci/no-shared-keys.sh
scripts/ci/no-positional-args.py
python3 -m unittest scripts/ci/test_no_positional_args.py
scripts/ci/check-workbooks.py
python3 -m unittest scripts/ci/test_check_workbooks.py
python3 -m unittest scripts/test_env_scripts.py
scripts/lib/env_settings.py --check
scripts/ci/check-names.py --base origin/main
python3 -m unittest scripts/ci/test_check_names.py
bicep build infra/main.bicep --stdout > /dev/null
bicep build infra/guardrails.bicep --stdout > /dev/null
AZURE_ENV_NAME=dev USNM_VNET_ADDRESS_PREFIX=192.0.2.0/24 bicep build-params infra/main.bicepparam --stdout > /dev/null
```

`scripts/ci/no-private-addresses.sh` runs on every pull request, documentation included.

`scripts/ci/install-bicep.sh` shows the Bicep version CI uses. The checks on the JSON data files are listed in the [development guide](docs/development.md#json-data-files). The end-to-end tests are described in [`web/README.md`](web/README.md), and the Quickwit parity tests in the [development guide](docs/development.md#against-quickwit).

## Rules

- **Entra ID only.** Every service signs in with a managed identity. Don't add account keys, SAS tokens, connection strings with keys or instrumentation keys ([ADR-0009](docs/design/adr/0009-entra-identity-only.md)). `scripts/ci/no-shared-keys.sh` fails the build if one appears.
- **No real data from production.** Never commit search log content or real search queries ([ADR-0012](docs/design/adr/0012-anonymous-search-log.md)), production logs, traces or data dumps. Tests use the synthetic corpus in `fixtures/`. `python3 fixtures/generate.py` rebuilds it, and CI checks that the result matches what is committed.
- **No secrets.** The repository holds none, and pull requests never get Azure access. Only `main` deploys, through its GitHub Environment.
- **Settings through pull requests.** An environment's public settings are committed in `infra/env/<env>.json`; change them with `scripts/settings.sh` and a pull request like any other change, and add a new setting to `infra/env/schema.json` with the line in `infra/main.bicepparam` that reads it, marked public or private. Nothing that identifies a subscription, tenant, identity or person (ids, addresses, resource ids) goes in the repository. The settings the schema marks private (principals, addresses, resource ids) are in the settings store, under the environment's label ([operations](docs/operations.md#settings)); the subscription and tenant ids (`AZURE_SUBSCRIPTION_ID`, `AZURE_TENANT_ID`), which the scripts need before they can reach the store, stay in the local settings file with the stack's outputs. `scripts/lib/env_settings.py --check` fails the build on a private or unknown setting in a committed file.
- **No private addresses.** Never commit an RFC 1918 address (the 10/8, 172.16/12 and 192.168/16 blocks), in code, scripts, Bicep, docs or tests. Refer to a private host by its host name, by a constant or environment variable, or best a constant that holds the host name, resolved at run time. Tests use the RFC 5737 documentation ranges (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`), or build a private address from its octets when the test is about private addresses. A private value an environment needs, such as its VNet block, is a setting. `scripts/ci/no-private-addresses.sh` fails the build on one, with no exceptions.
- **Named arguments only.** Scripts and command-line tools take named arguments (`scripts/settings.sh --env prod --key USNM_X --value true`), never positional ones: a positional interface breaks silently when its order changes, a named one fails loudly. Each parses its flags, fails on an unknown or missing one and prints its usage. A subcommand (`usnm-ingest enqueue`, `jaocr.py run`) is a name too. `scripts/ci/no-positional-args.py` fails the build on a script that reads `$1` or `sys.argv` outside its flag parser, a positional argparse argument, or a clap field that isn't `long` (#277).

## Pull requests

Keep each pull request to one change, and say what it changes and how you tested it. CI skips the jobs a change can't affect, so an infrastructure-only fix doesn't wait on the Rust and web builds.
