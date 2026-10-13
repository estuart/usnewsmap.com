# Images and deployment steps

Status: written, not built or run. The CI job `contrib-compute (server and bridge images)` builds the server image and smoke-tests it on every change to this directory; nothing here has started a project, reached a database, or served a volunteer. Everything not checked by that job is UNVERIFIED.

## What is built

| Image  | Source                              | Contents                                                                                                                                                                                                  |
| ------ | ----------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| server | `images/server/Containerfile`       | Debian 12 slim, pinned by digest. Stock BOINC server built from a pinned commit with no patches. Apache with PHP (mod_php), the scheduler and upload CGIs, all daemons under `supervisord`, one container |
| bridge | `images/bridge/Containerfile`       | The server image plus the `usnm-boinc` binary from PR #441's crate. Entry point `/usr/local/bin/bridge-job`                                                                                               |
| worker | `ocr/boinc/Containerfile` (PR #441) | Tesseract and `usnm-boinc work`; volunteers' Docker pulls it. Not built here. Where it is published: the addendum in ADR-0019                                                                             |

One container holds Apache and the daemons because the feeder and the scheduler share a System V shared-memory segment (design.md, section 2.1).

### Why the BOINC commit is master, not the 1.6.2 tag

`batch_collect_assimilator`, which PR #441's `daemons.xml` runs, is not in the `server_release/1.6/1.6.2` tree (searched on 2026-10-13) and is in master. The Containerfile pins master at `22bbebe08e2eaac5d5e88a6d93b88e800cb6341c` (2026-10-13). Move to the first release tag that contains it when one exists. The image is still unmodified upstream source.

### Facts about the stock tools that shaped the entry point

- `make_project` creates the schema, the project tree, and a code-signing key pair and an upload key pair in its key directory (`py/Boinc/setup_project.py`). The `--init-project` mode points the key directory at tmpfs and deletes it afterwards, so no code-signing private key exists on the server. The upload keys that run the project come from Key Vault.
- `make_project` refuses an existing project root, so the project lives in a subdirectory of the share: `/project/usnmocr`. `/project/handoff` is its sibling.
- BOINC's key generator is `crypt_prog -genkey 1024`, built with the server. `entrypoint.sh --make-upload-keys --dir DIR` wraps it, and `bootstrap.sh --apply` runs the image locally to make the upload keys, so the private key is generated on the operator's machine and written only to Key Vault.
- The `docker_wrapper` that `install.py` copies into the app version is built from the same source (`samples/docker_wrapper`) and left at `/usr/local/lib/usnm-boinc/docker_wrapper`. UNVERIFIED that it builds in this configuration, and that a binary linked on Debian 12 runs on volunteers' older glibc.

## Build

The server image's context is `contrib-compute/`:

```sh
docker build -f contrib-compute/images/server/Containerfile -t usnm-boinc-server:local contrib-compute
scripts/ci/check-boinc-server-image.sh --image usnm-boinc-server:local
```

The bridge image needs PR #441's `crates/usnm-boinc`, so it builds from the repository root after #441 is merged:

```sh
docker build -f contrib-compute/images/bridge/Containerfile \
  --build-arg SERVER_IMAGE=REGISTRY/NAME@sha256:SERVER_DIGEST .
```

To pin by digest, push each image to the project's registry and use the digest the registry reports, not the local image id. `bootstrap.sh` accepts only `name@sha256:<64 hex>`. The Containerfiles pin their base images by digest, and CI fails on a base without one.

Nothing pushes. CI builds without a registry. Pushing to the project's Azure Container Registry is the operator's step:

```sh
az acr login --name REGISTRY
docker tag usnm-boinc-server:local REGISTRY.azurecr.io/usnm-boinc-server:TAG
docker push REGISTRY.azurecr.io/usnm-boinc-server:TAG
az acr repository show --name REGISTRY --image usnm-boinc-server:TAG --query digest -o tsv
```

## Deploy, in order

Each step is the operator's. `bootstrap.sh` without `--apply` prints this plan with the values filled in and changes nothing.

1. Build and push the server image, and the bridge image once #441 is merged (above). Build and publish the worker image (ADR-0019 addendum).
2. Create the resource group. The script does not.
3. `scripts/boinc/bootstrap.sh --apply --subscription GUID --yes-deploy ...` (same options as `--what-if`). It checks the subscription, runs the what-if, deploys `infra/boinc/main.bicep`, generates the secrets, and writes them to Key Vault: `mysql-admin-password`, `mysql-runtime-password`, `upload-private-key`, `upload-public-key`, and the public half of the code-signing key, `code-sign-public-key`. It never reads `code_sign_private`, never prints a secret, and passes secrets to `az` only through mode-0600 files in a temporary directory that is removed on exit. A rerun redeploys, rotates the MySQL admin password and rewrites it; it keeps an existing run-time password and upload keys.
4. DNS and the managed certificate for the project host (bootstrap step 1b).
5. `az containerapp exec ... entrypoint.sh --init-project` (bootstrap step 3). Until it has run, the app answers the health probe with a placeholder.
6. App version, offline signing, `update_versions` (bootstrap step 4).
7. `curl` the project config and submit one reserved job (bootstrap step 5).

## Known blockers and risks

- **Guardrails.** The stack needs a storage account with shared-key access and a Log Analytics workspace with local auth (ADR-0019, decision 5). Until `infra/guardrails.bicep` has an exception, the deployment is denied in a guarded group. The what-if in step 3 shows the denial before anything is created.
- **Network path.** Key Vault and the Files share are private-endpoint only. Step 3's secret writes, and the copy of the signed app version to the share in step 6, need a machine with a path into the VNet (design.md, open question 11). If the vault is unreachable, the script stops after the deployment with that message.
- **Untested against a real server:** `make_project` against the flexible server; the grants for the run-time user; MySQL TLS with BOINC; Azure Files permissions under `boincadm` and `www-data` (the daemons run as `boincadm`, the share may be root-owned); the managed identity endpoint inside a Container Apps Job; per-run `JOB_ID` override for the job; shared-memory limits in Container Apps; image size (the whole build tree is kept, as upstream's images do).
