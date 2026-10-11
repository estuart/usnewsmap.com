#!/usr/bin/env bash
# Build our Quickwit image (Dockerfile.quickwit-patched, ADR-0013) in an
# environment's registry with ACR Tasks, and record its digest for that
# environment in infra/quickwit-image.json, where infra/main.bicep (the
# searcher sidecar, the ingest job's init container) and CI's publish (the
# ingest image's base) read it. Commit that change; after it merges and
# publish has run, `scripts/provision.sh --env <env>` moves the sidecar.
#
#   scripts/build-quickwit.sh --env prod
#   scripts/build-quickwit.sh --env dev --from prod
#
# --env NAME   the environment whose registry gets the image (ACR_NAME in
#              .azure/NAME/.env).
# --from NAME  copy the image recorded for environment NAME from its
#              registry instead of building (az acr import, same digest),
#              so two environments run the same bits. Both registries must
#              be in a subscription az is signed in to.
# --rebuild    build even if the registry already has this tag; without it
#              an existing tag's digest is recorded as it is (a build that
#              outlived its az session is recorded by running this again).
#
# The image is quickwit/quickwit-usnm, tagged with QW_COMMIT_TAGS from the
# Dockerfile (v0.9.1-pool1). A build takes about an hour on ACR Tasks'
# default 2 vCPU agent (54 minutes in October 2026). Needs az signed in to
# the environment's subscription and jq.
set -euo pipefail
usage() { sed -n '2,27s/^# \{0,1\}//p' "$0" >&2; exit 2; }
TARGET="" FROM_ENV="" REBUILD=false
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; TARGET=$2; shift 2 ;;
    --from) [ $# -ge 2 ] || usage; FROM_ENV=$2; shift 2 ;;
    --rebuild) REBUILD=true; shift ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$TARGET" ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
command -v jq > /dev/null || die "jq is needed to record the digest"
PINS=infra/quickwit-image.json
DOCKERFILE=Dockerfile.quickwit-patched
REPO=quickwit/quickwit-usnm

# One environment's registry and subscription, from its settings. A
# subshell, so reading another environment's settings changes nothing here.
registry_of() (
  ENV_NAME=$1
  # shellcheck source=scripts/lib/env.sh
  . scripts/lib/env.sh
  [ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME"
  acr=$(aget ACR_NAME) sub=$(aget AZURE_SUBSCRIPTION_ID)
  [ -n "$acr" ] && [ -n "$sub" ] || die "$ENV_NAME has no ACR_NAME or AZURE_SUBSCRIPTION_ID; run scripts/provision.sh --env $ENV_NAME"
  printf '%s %s\n' "$acr" "$sub"
)
reg=$(registry_of "$TARGET")
read -r ACR SUB <<< "$reg"
az account show --subscription "$SUB" -o none 2> /dev/null ||
  die "run: az login (the environment is in subscription $SUB)"

# Record the target environment's pin, keys sorted.
record() {
  local tmp
  tmp=$(mktemp)
  jq -S --indent 2 --arg env "$TARGET" --arg image "$1" --arg tag "$2" --arg digest "$3" \
    '.environments[$env] = {digest: $digest, image: $image, tag: $tag}' "$PINS" > "$tmp"
  mv "$tmp" "$PINS"
  echo "recorded $TARGET: $1:$2@$3 in $PINS"
  echo "next: commit $PINS, merge, let CI publish, then scripts/provision.sh --env $TARGET"
}

if [ -n "$FROM_ENV" ]; then
  [ "$FROM_ENV" != "$TARGET" ] || die "--from names the same environment"
  pin=$(jq -c --arg env "$FROM_ENV" '.environments[$env] // empty' "$PINS")
  [ -n "$pin" ] || die "$PINS records no image for $FROM_ENV"
  image=$(jq -r .image <<< "$pin") tag=$(jq -r .tag <<< "$pin") digest=$(jq -r .digest <<< "$pin")
  reg=$(registry_of "$FROM_ENV")
  read -r src_acr src_sub <<< "$reg"
  src_id=$(az acr show -n "$src_acr" --subscription "$src_sub" --query id -o tsv) ||
    die "can't read $FROM_ENV's registry $src_acr"
  echo "importing $image@$digest from $src_acr into $ACR"
  az acr import -n "$ACR" --subscription "$SUB" --registry "$src_id" \
    --source "$image@$digest" --image "$image:$tag" --force -o none
  got=$(az acr repository show -n "$ACR" --subscription "$SUB" --image "$image@$digest" --query digest -o tsv) ||
    die "$ACR doesn't have $image@$digest after the import"
  [ "$got" = "$digest" ] || die "imported digest $got != $digest"
  record "$image" "$tag" "$digest"
  exit 0
fi

tag=$(sed -n 's/^ARG QW_COMMIT_TAGS=//p' "$DOCKERFILE")
[[ $tag =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] || die "no valid QW_COMMIT_TAGS in $DOCKERFILE: '$tag'"

existing=$(az acr repository show -n "$ACR" --subscription "$SUB" --image "$REPO:$tag" --query digest -o tsv 2> /dev/null || true)
if [ -n "$existing" ] && [ "$REBUILD" = false ]; then
  echo "$ACR already has $REPO:$tag ($existing); not building (--rebuild to build again)"
else
  # The Dockerfile fetches the source from the fork, so the context is the
  # Dockerfile alone: nothing of the repository is uploaded.
  ctx=$(mktemp -d)
  trap 'rm -rf "$ctx"' EXIT
  cp "$DOCKERFILE" "$ctx/Dockerfile"
  echo "building $REPO:$tag in $ACR (about an hour; the log follows)"
  az acr build -r "$ACR" --subscription "$SUB" -t "$REPO:$tag" \
    --platform linux/amd64 --timeout 28800 "$ctx"
fi
digest=$(az acr repository show -n "$ACR" --subscription "$SUB" --image "$REPO:$tag" --query digest -o tsv)
[[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || die "no digest for $REPO:$tag in $ACR"
echo "digest: $digest"
record "$REPO" "$tag" "$digest"
