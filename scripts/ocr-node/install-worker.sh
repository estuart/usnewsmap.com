#!/usr/bin/env bash
# Install the OCR worker on an onboarded node (#296, 12 Step 2), from the
# operator's machine, from a GitHub release that CI built and attested.
#
#   scripts/ocr-node/install-worker.sh --env prod --name usnm-ocr-node1 \
#     --host claude@node1.example.org [--ssh-key ~/.ssh/id_ed25519] \
#     [--release ocr-worker-YYYYMMDD-SHA7] [--nodes N] [--concurrency N] \
#     [--scan-cache-url http://HOST:8380] [--pacing-multiplier M] \
#     [--page-timeout-secs N] [--publish-fetched VERSION] \
#     [--status-allow DASHBOARD] [--enable] [--rollback] [--dry-run]
#
# Steps:
#   1. picks the release (--release, or the newest ocr-worker-* release) and
#      downloads its small files with gh: the worker binary, the Tesseract
#      image's id and SHA256SUMS. `gh attestation verify` checks each one
#      was built by this repository's CI on main, and the binary and image
#      id must match SHA256SUMS. The Tesseract image archive is never
#      downloaded here: the node fetches it from the release itself and
#      checks it against the verified SHA256SUMS.
#   2. writes the worker's settings, /etc/usnm/ocr-worker.env on the node
#      (root:usnm, 0640), from the environment's deployment outputs: the
#      queue endpoint, the blob endpoint with ocr-out, --name as the node
#      id, --concurrency (default: the node's CPUs), the Tesseract image,
#      LoC gaps of 5000 x M and 1000 x M ms (M is --pacing-multiplier,
#      default N, the number of nodes, and never under it), and the nodes'
#      Application Insights connection string (OCR_NODE_APPINSIGHTS; left
#      out when empty, so the worker sends no telemetry). N is the number
#      of members in the nodes' Entra group; --nodes must match it. With
#      --scan-cache-url, the worker reads prefetched scans from the scan
#      server in the scan-fetch container first (ADR-0015 decision 9).
#      --page-timeout-secs sets USNM_OCR_PAGE_TIMEOUT_SECS (the worker's
#      default is 600). --publish-fetched VERSION makes this node also
#      publish the scan server's fetched list for that prefetch manifest
#      to ocr-out/fetched/ for the seeder (12 Step 11); set it on one node.
#      With --status-allow (the dashboard's host name, or its address),
#      the worker serves its status to the LAN dashboard on port 8381 (12 §12.2.5), with a journal
#      of finished pages in /var/lib/usnm-ocr, and the node's nftables
#      drops the port from anyone else.
#   3. over ssh with sudo (scripts/ocr-node/install-worker-node.sh): puts
#      the binary in /usr/local/lib/usnm-ocr-worker/<release>/, loads the
#      Tesseract image into the engine account's (usnmeng's) podman
#      storage through its Podman API socket, installs the env file
#      and the unit, points /usr/local/bin/usnm-ocr-worker at the release
#      and restarts the unit if it was running.
#   4. the unit stays disabled unless --enable: then the node reads a
#      rendered line with the engine as usnm first, and the unit is
#      disabled again if it doesn't stay up. Don't enable it on real queues
#      before ocr-sync (12 Step 8) is deployed: ocr-out deletes blobs after
#      30 days.
# --rollback points the node back at the release installed before the
# current one (or at --release, if installed), with its Tesseract image;
# it downloads nothing.
#
# --dry-run downloads and verifies the release, reads the node's state
# over ssh and prints the settings file and the commands; it changes
# nothing on the node.
#
# Needs gh (signed in; the attestation API needs a token), az signed in to
# the environment's tenant (it reads the Arc machine and counts the group's
# members), and ssh with passwordless sudo for --host. Works with macOS's
# bash 3.2.
set -euo pipefail
usage() { sed -n '5,10s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

REPO=tgoodyear/usnewsmap.com
WORKFLOW="$REPO/.github/workflows/ci.yml"
ENV_NAME="" HOST="" NAME="" SSH_KEY="" RELEASE="" NODES="" CONCURRENCY="" SCAN_CACHE_URL=""
PACING="" PAGE_TIMEOUT="" PUBLISH_FETCHED="" STATUS_ALLOW=""
ENABLE=false ROLLBACK=false DRY_RUN=false
while [ $# -gt 0 ]; do
  case "$1" in
    --env) ENV_NAME=${2:-}; shift 2 ;;
    --name) NAME=${2:-}; shift 2 ;;
    --host) HOST=${2:-}; shift 2 ;;
    --ssh-key) SSH_KEY=${2:-}; shift 2 ;;
    --release) RELEASE=${2:-}; shift 2 ;;
    --nodes) NODES=${2:-}; shift 2 ;;
    --concurrency) CONCURRENCY=${2:-}; shift 2 ;;
    --scan-cache-url) SCAN_CACHE_URL=${2:-}; shift 2 ;;
    --pacing-multiplier) PACING=${2:-}; shift 2 ;;
    --page-timeout-secs) PAGE_TIMEOUT=${2:-}; shift 2 ;;
    --publish-fetched) PUBLISH_FETCHED=${2:-}; shift 2 ;;
    --status-allow) STATUS_ALLOW=${2:-}; shift 2 ;;
    --enable) ENABLE=true; shift ;;
    --rollback) ROLLBACK=true; shift ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || { echo "error: missing --env" >&2; usage; }
[ -n "$NAME" ] || { echo "error: missing --name" >&2; usage; }
[ -n "$HOST" ] || { echo "error: missing --host" >&2; usage; }
[[ $NAME =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,31}$ ]] || die "--name: 1 to 32 letters, digits or hyphens (the worker's node id)"
[ -z "$SSH_KEY" ] || [ -r "$SSH_KEY" ] || die "--ssh-key: can't read $SSH_KEY"
[ -z "$RELEASE" ] || [[ $RELEASE =~ ^ocr-worker-[0-9]{8}-[0-9a-f]{7}$ ]] || die "--release: ocr-worker-<yyyymmdd>-<sha7>"
[ -z "$NODES" ] || [[ $NODES =~ ^[1-9][0-9]?$ ]] || die "--nodes: 1 to 99"
[ -z "$CONCURRENCY" ] || [[ $CONCURRENCY =~ ^[1-9][0-9]?$ ]] || die "--concurrency: 1 to 99"
[ -z "$SCAN_CACHE_URL" ] || [[ $SCAN_CACHE_URL =~ ^https?://[A-Za-z0-9.-]+(:[0-9]+)?/?$ ]] || die "--scan-cache-url: http://HOST:PORT"
[ -z "$PACING" ] || [[ $PACING =~ ^[1-9][0-9]?$ ]] || die "--pacing-multiplier: 1 to 99"
[ -z "$PAGE_TIMEOUT" ] || [[ $PAGE_TIMEOUT =~ ^[1-9][0-9]{1,4}$ ]] || die "--page-timeout-secs: 10 to 99999"
[ -z "$PUBLISH_FETCHED" ] || [[ $PUBLISH_FETCHED =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || die "--publish-fetched: a manifest version (pages-vYYYYMMDD-N)"
[ -z "$PUBLISH_FETCHED" ] || [ -n "$SCAN_CACHE_URL" ] || die "--publish-fetched needs --scan-cache-url"
[ -z "$STATUS_ALLOW" ] || [[ $STATUS_ALLOW =~ ^[A-Za-z0-9.-]+$ ]] || die "--status-allow: the dashboard's host name or IPv4 address"
[ "$ROLLBACK" = false ] || [ "$ENABLE" = false ] || die "--rollback and --enable don't go together"

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# shellcheck source=scripts/lib/env.sh
. scripts/lib/env.sh
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME; run scripts/bootstrap.sh --env $ENV_NAME first"

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=20)
[ -z "$SSH_KEY" ] || SSH_OPTS+=(-i "$SSH_KEY" -o IdentitiesOnly=yes)
# Runs install-worker-node.sh on the node as root, with its arguments
# quoted for the node's shell.
node() {
  local args
  args=$(printf '%q ' "$@")
  # shellcheck disable=SC2029 # the arguments are meant to expand here
  ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash -s -- $args" < scripts/ocr-node/install-worker-node.sh
}

step "The node"
facts=$(node --facts) || die "can't run on $HOST over ssh with sudo"
fact() { printf '%s\n' "$facts" | sed -n "s/^$1=//p"; }
CURRENT=$(fact current)
echo "$HOST: $(fact nproc) CPUs; current release: ${CURRENT:-none}; installed: $(fact installed)"
echo "usnm-ocr-worker.service: $(fact unit)"
[ -n "$(fact usnm)" ] && [ -n "$(fact podman)" ] || die "no usnm user or no podman on $HOST; run scripts/ocr-node/onboard.sh first"
[ "$(fact engine_socket)" = yes ] || die "no engine runtime (/run/usnm-ocr/podman.sock) on $HOST; run scripts/ocr-node/onboard.sh again"

if [ "$ROLLBACK" = true ]; then
  step "Rollback"
  if [ "$DRY_RUN" = true ]; then
    echo "dry run: would point /usr/local/bin/usnm-ocr-worker at ${RELEASE:-the release installed before ${CURRENT:-the current one}} and restart the unit if it runs"
    exit 0
  fi
  if [ -n "$RELEASE" ]; then node --rollback --release "$RELEASE"; else node --rollback; fi
  exit 0
fi

QUEUE=$(aget OCR_QUEUE_ENDPOINT)
BLOB=$(aget OCR_BLOB_ENDPOINT)
CONN=$(aget OCR_NODE_APPINSIGHTS)
NODE_GROUP=$(aget USNM_OCR_NODE_GROUP)
GROUP=$(aget AZURE_RESOURCE_GROUP)
SUBSCRIPTION=$(aget AZURE_SUBSCRIPTION_ID)
TENANT=$(aget AZURE_TENANT_ID)
[ "$(aget USNM_OCR_NODES)" = true ] || die "USNM_OCR_NODES isn't true for $ENV_NAME"
[[ $QUEUE =~ ^https://[a-z0-9]+\.queue\.core\.windows\.net/?$ ]] ||
  die "OCR_QUEUE_ENDPOINT isn't a queue endpoint (${QUEUE:-empty}); run scripts/provision.sh --env $ENV_NAME"
[[ $BLOB =~ ^https://[a-z0-9]+\.blob\.core\.windows\.net/?$ ]] ||
  die "OCR_BLOB_ENDPOINT isn't a blob endpoint (${BLOB:-empty}); run scripts/provision.sh --env $ENV_NAME"
[ -n "$NODE_GROUP" ] || die "no USNM_OCR_NODE_GROUP for $ENV_NAME"
[[ $CONN != *'"'* ]] || die "OCR_NODE_APPINSIGHTS contains a double quote"
[ -n "$CONN" ] || echo "warning: OCR_NODE_APPINSIGHTS is empty, so the worker will send no telemetry"

step "Azure"
az account show -o none 2> /dev/null || die "run: az login --tenant $TENANT"
current_tenant=$(az account show --query tenantId -o tsv 2> /dev/null || true)
[ "$current_tenant" = "$TENANT" ] || die "az is signed in to tenant ${current_tenant:-unknown}, not $TENANT"
status=$(az resource show -g "$GROUP" -n "$NAME" --resource-type Microsoft.HybridCompute/machines \
  --subscription "$SUBSCRIPTION" --query properties.status -o tsv 2> /dev/null) ||
  die "no Arc machine $NAME in $GROUP; onboard the node first (scripts/ocr-node/onboard.sh)"
echo "Arc machine $NAME: $status"
# Graph's member count: `az ad group member list` can come back empty for
# members (the machines' service principals) the operator's account can't
# read in full.
members=$(az rest --method get --url "https://graph.microsoft.com/v1.0/groups/$NODE_GROUP/members/\$count" \
  --headers ConsistencyLevel=eventual 2> /dev/null | tr -dc '0-9') ||
  die "can't count the members of $NODE_GROUP"
[ -n "$members" ] && [ "$members" -ge 1 ] || die "the nodes' group $NODE_GROUP has no members"
# Every node paces itself alone, so N nodes each wait N times as long to
# keep the pair at the Japanese job's rate (12 §12.2.2).
[ -z "$NODES" ] || [ "$NODES" = "$members" ] ||
  die "--nodes $NODES, but the nodes' group has $members members; every node runs with the group's count"
NODES=$members
echo "nodes in the group: $NODES"
# The scan fetcher in the scan-fetch container also asks LoC, at the full
# gaps, so the operator may slow the workers' LoC fallback further
# (12 §12.2.2, "LoC pacing"). Never faster than one share per node.
[ -n "$PACING" ] || PACING=$NODES
[ "$PACING" -ge "$NODES" ] || die "--pacing-multiplier $PACING is under the $NODES nodes; every node waits at least N times as long"
echo "LoC gaps: x$PACING"

step "The release"
command -v gh > /dev/null || die "needs gh"
if [ -z "$RELEASE" ]; then
  RELEASE=$(gh release list --repo "$REPO" --limit 100 --json tagName,createdAt \
    --jq '[.[] | select(.tagName | test("^ocr-worker-[0-9]{8}-[0-9a-f]{7}$"))] | sort_by(.createdAt) | last | .tagName // empty')
  [ -n "$RELEASE" ] || die "no ocr-worker release in $REPO yet"
fi
echo "release $RELEASE"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
gh release download "$RELEASE" --repo "$REPO" --dir "$TMP" \
  --pattern usnm-ocr-worker --pattern usnm-ocr-tesseract.image-id --pattern SHA256SUMS
for f in usnm-ocr-worker usnm-ocr-tesseract.image-id SHA256SUMS; do
  gh attestation verify "$TMP/$f" --repo "$REPO" --signer-workflow "$WORKFLOW" \
    --source-ref refs/heads/main > /dev/null ||
    die "$f from $RELEASE has no valid attestation from $WORKFLOW on main"
  echo "$f: attestation verified"
done
sum_of() { awk -v f="$1" '$2 == f || $2 == "*" f { print $1 }' "$TMP/SHA256SUMS"; }
sha256() { shasum -a 256 "$1" | awk '{ print $1 }'; }
BIN_SUM=$(sum_of usnm-ocr-worker)
IMAGE_SUM=$(sum_of usnm-ocr-tesseract.oci.tar.gz)
[[ $BIN_SUM =~ ^[0-9a-f]{64}$ ]] && [[ $IMAGE_SUM =~ ^[0-9a-f]{64}$ ]] || die "SHA256SUMS lacks the binary or the image archive"
[ "$(sha256 "$TMP/usnm-ocr-worker")" = "$BIN_SUM" ] || die "the binary's sha256 differs from SHA256SUMS"
[ "$(sha256 "$TMP/usnm-ocr-tesseract.image-id")" = "$(sum_of usnm-ocr-tesseract.image-id)" ] ||
  die "the image id's sha256 differs from SHA256SUMS"
IMAGE_ID=$(cat "$TMP/usnm-ocr-tesseract.image-id")
[[ $IMAGE_ID =~ ^sha256:[0-9a-f]{64}$ ]] || die "the release's image id isn't sha256:<64 hex digits>"
IMAGE_URL="https://github.com/$REPO/releases/download/$RELEASE/usnm-ocr-tesseract.oci.tar.gz"
echo "binary sha256 $BIN_SUM; Tesseract image $IMAGE_ID"

[ -n "$CONCURRENCY" ] || CONCURRENCY=$(fact nproc)
[[ $CONCURRENCY =~ ^[1-9][0-9]*$ ]] || die "can't tell the node's CPUs; pass --concurrency"
{
  echo "# Written by scripts/ocr-node/install-worker.sh for $RELEASE on $(date -u +%Y-%m-%dT%H:%M:%SZ);"
  echo "# run it again to change these. Local changes go in /etc/usnm/ocr-worker.local.env."
  echo "USNM_OCR_QUEUE_URL=$QUEUE"
  echo "USNM_OCR_OUT_URL=${BLOB%/}/ocr-out"
  echo "USNM_OCR_NODE_ID=$NAME"
  echo "USNM_OCR_CONCURRENCY=$CONCURRENCY"
  echo "# LoC's pacing for $NODES node(s), multiplier $PACING: every node waits M times as long."
  # 5 s: www.loc.gov's Crawl-Delay.
  echo "USNM_OCR_API_GAP_MS=$((5000 * PACING))"
  echo "USNM_OCR_TILE_GAP_MS=$((1000 * PACING))"
  [ -z "$PAGE_TIMEOUT" ] || echo "USNM_OCR_PAGE_TIMEOUT_SECS=$PAGE_TIMEOUT"
  [ -z "$PUBLISH_FETCHED" ] || echo "USNM_OCR_PUBLISH_FETCHED=$PUBLISH_FETCHED"
  echo "USNM_OCR_TESSERACT_IMAGE=$IMAGE_ID"
  [ -z "$SCAN_CACHE_URL" ] || echo "USNM_OCR_SCAN_CACHE_URL=$SCAN_CACHE_URL"
  if [ -n "$STATUS_ALLOW" ]; then
    echo "USNM_OCR_STATUS_LISTEN=0.0.0.0:8381"
    echo "USNM_OCR_STATUS_ALLOW=$STATUS_ALLOW"
    echo "USNM_OCR_STATUS_JOURNAL=/var/lib/usnm-ocr/journal.jsonl"
  fi
  [ -z "$CONN" ] || echo "APPLICATIONINSIGHTS_CONNECTION_STRING=\"$CONN\""
} > "$TMP/ocr-worker.env"

step "/etc/usnm/ocr-worker.env"
# The connection string isn't a secret (ingestion needs an Entra token as
# well), but it needn't be in a terminal's scrollback either.
sed -E 's/^(APPLICATIONINSIGHTS_CONNECTION_STRING=).*/\1"<OCR_NODE_APPINSIGHTS>"/' "$TMP/ocr-worker.env"

if [ "$DRY_RUN" = true ]; then
  step "Dry run"
  echo "would copy the binary, the env file and scripts/ocr-node/usnm-ocr-worker.service to $HOST, then run as root:"
  echo "  install-worker-node.sh --release $RELEASE --dir <tmp> --binary-sha256 $BIN_SUM \\"
  echo "    --image-id $IMAGE_ID --image-url $IMAGE_URL --image-sha256 $IMAGE_SUM$([ "$ENABLE" = false ] || echo ' --enable')"
  if [ "$ENABLE" = true ]; then
    echo "then check the engine as usnm and enable usnm-ocr-worker.service"
  else
    echo "usnm-ocr-worker.service stays $(fact unit | cut -d, -f1)"
  fi
  exit 0
fi

step "Installing on $HOST"
remote=$(ssh "${SSH_OPTS[@]}" "$HOST" 'mktemp -d /tmp/usnm-install.XXXXXX') || die "can't reach $HOST over ssh"
# shellcheck disable=SC2029 # $remote is meant to expand here
{
  ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/usnm-ocr-worker" < "$TMP/usnm-ocr-worker" &&
    ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/ocr-worker.env" < "$TMP/ocr-worker.env" &&
    ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/usnm-ocr-worker.service" < scripts/ocr-node/usnm-ocr-worker.service
} || die "copying to $HOST failed"
status=0
if [ "$ENABLE" = true ]; then
  node --release "$RELEASE" --dir "$remote" --binary-sha256 "$BIN_SUM" --image-id "$IMAGE_ID" \
    --image-url "$IMAGE_URL" --image-sha256 "$IMAGE_SUM" --enable || status=$?
else
  node --release "$RELEASE" --dir "$remote" --binary-sha256 "$BIN_SUM" --image-id "$IMAGE_ID" \
    --image-url "$IMAGE_URL" --image-sha256 "$IMAGE_SUM" || status=$?
fi
# shellcheck disable=SC2029
ssh "${SSH_OPTS[@]}" "$HOST" "rm -rf $remote" || true
[ "$status" -eq 0 ] || die "installing on $HOST failed ($status)"
echo
echo "installed $RELEASE on $NAME ($HOST)"
