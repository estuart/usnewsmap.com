#!/usr/bin/env bash
# Build the BOINC server and bridge images in Azure (ACR Tasks) into the app's shared
# registry, and write their registry digests to contrib-compute/infra/boinc/env/<env>.json:
#
#   scripts/build-boinc-images.sh --env prod
#   # commit the settings file through a pull request, then:
#   scripts/provision-boinc.sh --env prod
#
# Nothing is built on this machine: az acr build uploads the source (contrib-compute/ for the
# server; the repository for the bridge, which compiles crates/usnm-boinc) and the registry
# builds and pushes it. Images are tagged with the commit and pinned by digest. The worker
# image volunteers pull is not built here: it is a public GHCR package, published by
# .github/workflows/boinc-worker.yml (ADR-0019, addendum).
#
# It builds the committed HEAD: it refuses while the working tree has uncommitted changes, so
# the tag names what was built (--allow-dirty: dev only). --what-if prints the builds and
# changes nothing. The registry comes from the stack's output, so the base resources must be
# deployed first (scripts/provision-boinc.sh with no images in the settings).
set -euo pipefail
usage() { echo "usage: scripts/build-boinc-images.sh --env <prod|dev> [--allow-dirty] [--what-if]" >&2; exit 2; }
ENV_NAME="" ALLOW_DIRTY=false WHAT_IF=false
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --allow-dirty) ALLOW_DIRTY=true; shift ;;
    --what-if) WHAT_IF=true; shift ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/boinc.sh
. scripts/lib/boinc.sh
boinc_load_settings
boinc_az_check
if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
  $ALLOW_DIRTY && [ "$ENV_NAME" != prod ] || die "the working tree has uncommitted or untracked files; the images are built from it and tagged with HEAD, so commit first"
  echo "warning: building uncommitted changes (--allow-dirty)" >&2
fi
[ -f crates/usnm-boinc/Cargo.toml ] || die "no crates/usnm-boinc: the bridge image needs it"
sha=$(git rev-parse HEAD)
registry=$(jq -er .registry "$(dirname "$0")/../infra/names/$ENV_NAME.json") || die "no registry in infra/names/$ENV_NAME.json"
server="$registry.azurecr.io/usnm-boinc-server"
bridge="$registry.azurecr.io/usnm-boinc-bridge"

if $WHAT_IF; then
  cat << PLAN
would build, in registry $registry (ACR Tasks; nothing is built here):
  usnm-boinc-base, usnm-boinc-bridge-bin   only when their key (a hash of their inputs) is new
  $server:$sha    from contrib-compute/images/server/Containerfile, on the base image
  $bridge:$sha    from contrib-compute/images/bridge/Containerfile, the server image plus the binary
and write both digests to $BOINC_SETTINGS (images.server, images.bridge).
PLAN
  exit 0
fi

# digest REPOSITORY TAG: the registry digest of an image it holds.
digest() {
  local d
  d=$(baz acr repository show --name "$registry" --image "$1:$2" --query digest -o tsv) || die "no $1:$2 in $registry"
  [[ $d =~ ^sha256:[0-9a-f]{64}$ ]] || die "unexpected digest for $1:$2"
  printf '%s' "$d"
}

# have REPOSITORY TAG: true when the registry already holds REPOSITORY:TAG.
have() { baz acr repository show --name "$registry" --image "$1:$2" -o none 2> /dev/null; }

# The slow layers are keyed on their inputs and built only when that key is new:
# - usnm-boinc-base: BOINC compiled from the pinned commit plus the runtime packages, keyed on
#   images/base/Containerfile (which pins both);
# - usnm-boinc-bridge-bin: the usnm-boinc binary, keyed on the Rust sources, .dockerignore (the context is the repository) and its Containerfile.
# Keys are git object hashes of the committed tree, so they name exactly what was built.
base_key=$(git rev-parse "HEAD:contrib-compute/images/base/Containerfile" | cut -c1-16)
bin_inputs=(HEAD:crates HEAD:Cargo.toml HEAD:Cargo.lock HEAD:.dockerignore HEAD:contrib-compute/images/bridge-bin/Containerfile)
if git rev-parse -q --verify HEAD:rust-toolchain.toml > /dev/null; then bin_inputs+=(HEAD:rust-toolchain.toml); fi
bin_key=$(git rev-parse "${bin_inputs[@]}" | git hash-object --stdin | cut -c1-16)

if have usnm-boinc-base "$base_key"; then
  echo "usnm-boinc-base:$base_key is in $registry; not rebuilding BOINC"
else
  echo "building usnm-boinc-base:$base_key in $registry (BOINC from source: about 20 minutes)"
  baz acr build --registry "$registry" --image "usnm-boinc-base:$base_key" --platform linux/amd64 \
    --timeout 7200 --file contrib-compute/images/base/Containerfile contrib-compute/images/base ||
    die "the base image build failed"
fi
base_image="$registry.azurecr.io/usnm-boinc-base@$(digest usnm-boinc-base "$base_key")"

if have usnm-boinc-bridge-bin "$bin_key"; then
  echo "usnm-boinc-bridge-bin:$bin_key is in $registry; not recompiling usnm-boinc"
else
  echo "building usnm-boinc-bridge-bin:$bin_key in $registry (release build: about 10 minutes)"
  baz acr build --registry "$registry" --image "usnm-boinc-bridge-bin:$bin_key" --platform linux/amd64 \
    --timeout 7200 --file contrib-compute/images/bridge-bin/Containerfile . ||
    die "the bridge binary build failed"
fi
bin_image="$registry.azurecr.io/usnm-boinc-bridge-bin@$(digest usnm-boinc-bridge-bin "$bin_key")"

echo "building usnm-boinc-server:$sha in $registry"
baz acr build --registry "$registry" --image "usnm-boinc-server:$sha" --platform linux/amd64 \
  --timeout 1800 --build-arg "BASE_IMAGE=$base_image" --file contrib-compute/images/server/Containerfile contrib-compute ||
  die "the server image build failed"
server_image="$server@$(digest usnm-boinc-server "$sha")"
echo "server image $server_image"

echo "building usnm-boinc-bridge:$sha in $registry"
baz acr build --registry "$registry" --image "usnm-boinc-bridge:$sha" --platform linux/amd64 \
  --timeout 1800 --build-arg "SERVER_IMAGE=$server_image" --build-arg "BIN_IMAGE=$bin_image" \
  --file contrib-compute/images/bridge/Containerfile . ||
  die "the bridge image build failed"
bridge_image="$bridge@$(digest usnm-boinc-bridge "$sha")"
echo "bridge image $bridge_image"

tmp=$(mktemp)
jq --indent 2 --arg s "$server_image" --arg b "$bridge_image" '.images.server = $s | .images.bridge = $b' \
  "$BOINC_SETTINGS" > "$tmp" && mv "$tmp" "$BOINC_SETTINGS"
echo "wrote both digests to $BOINC_SETTINGS; commit it, then run scripts/provision-boinc.sh --env $ENV_NAME"
