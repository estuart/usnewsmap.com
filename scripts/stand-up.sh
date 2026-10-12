#!/usr/bin/env bash
# Stand an environment up for one experiment (#275; docs/operations.md, "Dev
# environment lifecycle"), running this checkout's code. Unlike bootstrap it
# creates no GitHub Environment and leaves USNM_DEPLOY_ENVIRONMENTS alone,
# so no push to main rolls it. It provisions the environment, builds the
# checkout's images in its registry with ACR Tasks (az acr build), points
# the environment at them (USNM_IMAGE_TAG exp-<commit>, USNM_USE_ACR true)
# and provisions again. With the archival account and the ingest job, it
# then starts the title records and catalog from the account's newest copy
# (scripts/titles-seed.sh). scripts/teardown.sh takes it down afterwards.
#
#   scripts/stand-up.sh --env dev --experiment 275 [--archive]
#
#   --env NAME           the environment; never prod
#   --experiment N       the experiment's issue number (USNM_EXPERIMENT)
#   --archive            use the archival account: USNM_ARCHIVE_ACCOUNT from
#                        the usnm-archive stack (scripts/archive-store.sh)
#   --subscription ID    for an environment with no settings yet (default:
#                        az's current subscription)
#   --location REGION    the same (default eastus2)
#
# Settings: the environment's settings file (scripts/lib/env.sh) if there is
# one; otherwise the newest one a teardown kept beside it (.env.deleted-<time>),
# copied back; otherwise new ones. The experiment's own settings
# (USNM_SEARCH_CLUSTER, USNM_INGEST_JOBS, ...) can be set before this runs,
# or afterwards followed by scripts/provision.sh. Running it again builds the
# checkout's current commit and provisions again.
#
# Needs az 2.61+ signed in with Owner on the subscription, git and jq. The
# checkout must have no uncommitted changes to tracked files, so the image
# tag names what was built. A first stand-up takes about 40 to 60 minutes.
set -euo pipefail
usage() { awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "$0" >&2; exit 2; }
TARGET="" EXPERIMENT="" ARCHIVE=false SUBSCRIPTION="" LOCATION=eastus2
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; TARGET=$2; shift 2 ;;
    --experiment) [ $# -ge 2 ] || usage; EXPERIMENT=$2; shift 2 ;;
    --archive) ARCHIVE=true; shift ;;
    --subscription) [ $# -ge 2 ] || usage; SUBSCRIPTION=$2; shift 2 ;;
    --location) [ $# -ge 2 ] || usage; LOCATION=$2; shift 2 ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$TARGET" ] && [ -n "$EXPERIMENT" ] || usage
[[ $EXPERIMENT =~ ^[0-9]+$ ]] || { echo "error: --experiment is an issue number" >&2; exit 2; }
[ "$TARGET" != prod ] || { echo "error: prod is never an experiment" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }
cd "$(dirname "$0")/.."
ENV_NAME=$TARGET
. scripts/lib/env.sh
need_stack_az
for tool in az git jq; do command -v "$tool" > /dev/null || die "$tool is not installed"; done
az account show -o none 2> /dev/null || die "run: az login [--tenant TENANT]"
# Untracked files too: the image builds send the checkout as their context.
[ -z "$(git status --porcelain)" ] ||
  die "commit, stash or remove the checkout's changes and untracked files first: the image tag names the commit"
SHA=$(git rev-parse HEAD)
TAG=exp-$(git rev-parse --short HEAD)

step "Settings for $ENV_NAME"
if [ ! -s "$ENV_FILE" ]; then
  # A glob sorts by name, and the names end in UTC times: the last is newest.
  kept=""
  for f in "$ENV_FILE".deleted-*; do [ -e "$f" ] && kept=$f; done
  if [ -n "$kept" ]; then
    echo "starting from $kept"
    mkdir -p "$(dirname "$ENV_FILE")"
    cp "$kept" "$ENV_FILE"
  else
    echo "no settings yet: new ones"
    [ -z "$SUBSCRIPTION" ] || az account set --subscription "$SUBSCRIPTION"
    aset AZURE_ENV_NAME "$ENV_NAME"
    aset AZURE_SUBSCRIPTION_ID "$(az account show --query id -o tsv)"
    aset AZURE_LOCATION "$LOCATION"
  fi
fi
recorded=$(aget AZURE_SUBSCRIPTION_ID)
[ -n "$recorded" ] || die "no AZURE_SUBSCRIPTION_ID in $ENV_FILE"
[ -z "$SUBSCRIPTION" ] || [ "$SUBSCRIPTION" = "$recorded" ] ||
  die "$ENV_NAME's settings are for subscription $recorded; moving is bootstrap's (--move)"
az account set --subscription "$recorded" ||
  die "can't select subscription $recorded (az login to its tenant?)"
aset USNM_EXPERIMENT "$EXPERIMENT"
# One Cosmos DB free-tier account per subscription, as bootstrap decides.
if [ -z "$(aget USNM_COSMOS_FREE_TIER)" ]; then
  others=$(az cosmosdb list --query "[?enableFreeTier && !(tags.project == 'usnewsmap' && tags.environment == '$ENV_NAME')].name" -o tsv)
  if [ -n "$others" ]; then aset USNM_COSMOS_FREE_TIER false; else aset USNM_COSMOS_FREE_TIER true; fi
fi

# The images come from the environment's own registry, which the first
# provision creates. Until then the stack runs without them: no API app, no
# jobs (they all need USNM_USE_ACR).
ACR=$(aget ACR_NAME)
have=0
if [ -n "$ACR" ]; then
  have=$(az acr list --query "length([?name=='$ACR'])" -o tsv) || die "can't list the registries"
fi
if [ "$have" = 0 ]; then
  step "Provisioning $ENV_NAME without images (registry, data, network, identities)"
  aset USNM_USE_ACR false
  aset USNM_IMAGE_TAG ""
  aset USNM_API_IMAGE ""
  provision
  ACR=$(aget ACR_NAME)
fi
[ -n "$ACR" ] || die "no ACR_NAME after provisioning"
SERVER=$(aget ACR_LOGIN_SERVER)
[ -n "$SERVER" ] || SERVER=$(az acr show -n "$ACR" --query loginServer -o tsv)

# The Quickwit image the API's sidecar and the jobs run, pinned in
# infra/quickwit-image.json, copied in by digest as CI's publish does
# (scripts/ci/quickwit-image.sh). An environment the file lists runs our own
# build, which only scripts/build-quickwit.sh puts there.
step "Quickwit image in $ACR"
pins=infra/quickwit-image.json
[ -z "$(jq -r --arg env "$ENV_NAME" '.environments[$env] // empty' "$pins")" ] ||
  die "$pins lists $ENV_NAME: build its Quickwit first (scripts/build-quickwit.sh --env $ENV_NAME)"
qw_image=$(jq -r .upstream.image "$pins")
qw_tag=$(jq -r .upstream.tag "$pins")
qw_digest=$(jq -r .upstream.digest "$pins")
[[ $qw_digest =~ ^sha256:[0-9a-f]{64}$ ]] || die "bad digest in $pins: $qw_digest"
got=$(az acr repository show -n "$ACR" --image "$qw_image@$qw_digest" --query digest -o tsv 2> /dev/null || true)
if [ "$got" != "$qw_digest" ]; then
  # Google's Docker Hub mirror first, as CI does: Docker Hub's rate limit.
  for src in "mirror.gcr.io/$qw_image@$qw_digest" "docker.io/$qw_image@$qw_digest"; do
    if az acr import -n "$ACR" --source "$src" --image "$qw_image:$qw_tag" --force -o none; then break; fi
    echo "could not import from ${src%%/*}"
  done
  got=$(az acr repository show -n "$ACR" --image "$qw_image@$qw_digest" --query digest -o tsv 2> /dev/null || true)
  [ "$got" = "$qw_digest" ] || die "$ACR has no $qw_image@$qw_digest after the import"
fi
echo "$ACR has $qw_image:$qw_tag@$qw_digest"

# The images CI would publish, from this checkout (an existing tag is kept:
# the same commit builds the same image). The Japanese OCR image only when
# its job is on.
step "Building $TAG in $ACR"
build() {
  local repo=$1; shift
  if [ -n "$(az acr repository show-tags -n "$ACR" --repository "$repo" --query "[?@=='$TAG']" -o tsv 2> /dev/null)" ]; then
    echo "$repo:$TAG is there already"
    return 0
  fi
  az acr build -r "$ACR" -t "$repo:$TAG" "$@" .
}
build usnewsmap-api -f Dockerfile
build usnewsmap-ingest -f Dockerfile.ingest --build-arg "USNM_GIT_SHA=$SHA" \
  --build-arg "QUICKWIT_IMAGE=$SERVER/$qw_image@$qw_digest"
[ "$(aget USNM_JA_OCR_JOB)" != true ] || build usnewsmap-ja-ocr -f Dockerfile.ja-ocr

step "Provisioning $ENV_NAME on $TAG"
aset USNM_IMAGE_TAG "$TAG"
aset USNM_USE_ACR true
if [ "$ARCHIVE" = true ]; then
  # Stacks can return output names with their case changed (scripts/lib/env.sh).
  archive=$(az stack sub show -n usnm-archive --query outputs -o json |
    jq -r 'to_entries[] | select(.key | ascii_upcase == "ARCHIVE_ACCOUNT_ID") | .value.value')
  [ -n "$archive" ] || die "no archival account: the usnm-archive stack has no ARCHIVE_ACCOUNT_ID output"
  aset USNM_ARCHIVE_ACCOUNT "$archive"
fi
provision

# The title records and catalog from the archival account's newest copy, so
# a release here needs no titles-sync paced against LoC. Never replaces a
# cache the environment has.
if [ -n "$(aget USNM_ARCHIVE_ACCOUNT)" ]; then
  step "Title records from the archival account"
  if [ -z "$(aget INGEST_JOB)" ]; then
    echo "no ingest job (USNM_INGEST_JOBS is off): not seeded. With the job on, run scripts/titles-seed.sh --env $ENV_NAME"
  elif ! scripts/titles-seed.sh --env "$ENV_NAME"; then
    echo "warning: the title records weren't seeded; $ENV_NAME is up without them. Again: scripts/titles-seed.sh --env $ENV_NAME" >&2
  fi
fi

step "Done"
echo "$ENV_NAME runs $TAG for experiment #$EXPERIMENT. API: $(aget API_URL)"
echo "Check it with scripts/dev-status.sh; tear it down when the results are saved: scripts/teardown.sh --env $ENV_NAME"
