# contrib-compute

Azure hosting for the stock BOINC server that PR #441 (the BOINC batch OCR pilot, `usnm-boinc`) submits to. #441 is the client base and the bridge: a Docker-wrapper app, one cached page per task, a bridge that reserves and journals pages and publishes native output. It does not provision a server. This directory does. The decision is in [ADR-0019](../docs/design/adr/0019-boinc-server-hosting.md); the on-prem workers it sits beside are in [ADR-0015](../docs/design/adr/0015-onprem-ocr-workers.md).

Status: a draft. Nothing here is deployed and no server exists. Checked so far: Bicep compile, shellcheck and XML well-formedness. Each document lists what was not tested, marked UNVERIFIED.

## Contents

- `infra/boinc/`: Bicep for the stock server: a Container App (Apache, PHP and the stock daemons, one replica, no scale to zero), a MySQL flexible server, an Azure Files share, Key Vault, Log Analytics, a static site for the human pages, and a manual-trigger Container Apps Job that runs `usnm-boinc submit`. Lint and compile only.
- `boinc/`: `config.xml.template` (daemons come from #441's `ocr/boinc/daemons.xml`), `project.xml` and `plan_class_spec.xml` (the `docker` plan class).
- `scripts/boinc/bootstrap.sh`: the operator's plan for standing the server up; it only prints and what-ifs.
- `docs/`: [design](docs/design.md), [interface](docs/interface.md) (what is assumed of #441), and [verified facts](docs/verified-facts.md) (each with its source and fetch date).

This directory has no Rust, app or work generator; those are #441's. Later options for anonymous volunteers, and a deferred fetch idea, are one short section each in the design.

## Checks

From this directory:

```sh
for f in boinc/project.xml boinc/plan_class_spec.xml boinc/config.xml.template; do
  python3 -c 'import sys, xml.dom.minidom as m; m.parse(sys.argv[1])' "$f"
done
shellcheck -x -S warning scripts/boinc/*.sh
for f in $(git ls-files 'infra/*.bicep'); do bicep build "$f" --stdout > /dev/null; done
```

CI runs the same when a pull request touches this directory (`scripts/ci/changes.sh`). `bicep build` only compiles on the local machine; nothing in this repository is deployed by these commands.

The repository's rules in [CONTRIBUTING](../CONTRIBUTING.md) apply here too: no private addresses, no secrets, and scripts take `--flags`, never positional arguments. The server template has three exceptions to the Entra-only rule, listed in ADR-0019, including the database password in its Key Vault.
