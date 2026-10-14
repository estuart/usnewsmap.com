#!/usr/bin/env bash
# The OCR worker's nightly self-update on a node (#471).
# install-worker.sh --auto-update-delay installs this as
# /usr/local/sbin/usnm-ocr-update-worker and runs it from
# usnm-ocr-update.service, which usnm-ocr-update.timer starts at 03:15 US
# Eastern every day.
#
#   usnm-ocr-update-worker --delay-hours N [--leader-status URL] [--leader-up-hours N] [--stay-up-secs N] [--dry-run]
#
# It picks the newest ocr-worker-* release published at least --delay-hours
# ago that this node hasn't rejected, and does nothing if the node runs it
# already. With --leader-status (another node's worker status, for example
# http://<leader>:8381/status), it also waits until that node's worker
# runs the release and has been up for --leader-up-hours (default 2): one
# node takes a release first, the others follow once it has run it.
#
# Verification needs no GitHub token. The repository is public, so its
# attestations API answers without one: for each file it downloads (the
# binary, the image id and SHA256SUMS) it fetches the Sigstore bundles for
# the file's sha256 and runs `gh attestation verify --bundle` with the same
# policy as install-worker.sh: signed by this repository's ci.yml on main.
# gh checks the bundle against Sigstore's public trust root; it is never
# signed in on the node. The binary and the image id must also match
# SHA256SUMS, and the image archive is checked against it when the node
# downloads it.
#
# The update itself is install-worker-node.sh --update (installed beside
# this as /usr/local/libexec/usnm-ocr/install-worker-node.sh): it keeps
# /etc/usnm/ocr-worker.env as it is apart from the Tesseract image, runs
# `usnm-ocr-worker check-engine` with the new binary and image before
# switching, restarts the worker and rolls back to the release before if
# the unit doesn't stay up for --stay-up-secs (default 300). A release
# that fails the check or is rolled back is added to
# /var/lib/usnm-ocr-update/rejected and never tried again;
# `install-worker.sh --rollback` adds the release it leaves there too.
#
# It does nothing while usnm-ocr-worker.service isn't enabled, while
# dnf-automatic or the weekly reboot window runs, or while another install
# holds the maintenance lock (/run/lock/usnm-ocr-maintenance.lock, which
# the reboot window waits for). Its lines go to the journal under
# usnm-ocr-update; an update applied, a rollback and any problem are logged
# at Warning, which the Azure Monitor Agent collects from the node's
# syslog. --dry-run says what it would do and changes nothing.
set -euo pipefail
usage() { sed -n '8s/^# \{0,1\}//p' "$0" >&2; exit 2; }
# Under systemd, a <N> prefix sets a line's syslog priority.
pri() { if [ -n "${JOURNAL_STREAM:-}" ]; then printf '<%s>%s\n' "$1" "$2"; else printf '%s\n' "$2"; fi; }
log() { pri 5 "$*"; }
warn() { pri 4 "$*"; }

DELAY="" LEADER="" LEADER_UP=2 STAY_UP=300 DRY_RUN=false
while [ $# -gt 0 ]; do
  case "$1" in
    --delay-hours) DELAY=${2:-}; shift 2 ;;
    --leader-status) LEADER=${2:-}; shift 2 ;;
    --leader-up-hours) LEADER_UP=${2:-}; shift 2 ;;
    --stay-up-secs) STAY_UP=${2:-}; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[[ $DELAY =~ ^[0-9]{1,3}$ ]] || { echo "error: --delay-hours: 0 to 999" >&2; usage; }
DELAY=$((10#$DELAY))
[ -z "$LEADER" ] || [[ $LEADER =~ ^https?://[A-Za-z0-9.-]+(:[0-9]+)?/[A-Za-z0-9/_-]*$ ]] ||
  { echo "error: --leader-status: http://HOST:PORT/status" >&2; usage; }
[[ $LEADER_UP =~ ^[0-9]{1,3}$ ]] || { echo "error: --leader-up-hours: 0 to 999" >&2; usage; }
LEADER_UP=$((10#$LEADER_UP))
[[ $STAY_UP =~ ^[1-9][0-9]{0,4}$ ]] || { echo "error: --stay-up-secs: 1 to 99999" >&2; usage; }
[ "$DRY_RUN" = true ] || [ "$(id -u)" -eq 0 ] || { echo "error: run as root" >&2; exit 1; }

REPO=tgoodyear/usnewsmap.com
WORKFLOW="$REPO/.github/workflows/ci.yml"
API="https://api.github.com/repos/$REPO"
TAG_RE='^ocr-worker-[0-9]{8}-[0-9a-f]{7}$'
# USNM_OCR_TEST_ROOT: the tests' stand-in for /, with --dry-run only.
R=""
[ "$DRY_RUN" = false ] || R=${USNM_OCR_TEST_ROOT:-}
LIB=$R/usr/local/lib/usnm-ocr-worker
LINK=$R/usr/local/bin/usnm-ocr-worker
ENVFILE=$R/etc/usnm/ocr-worker.env
UNIT=$R/etc/systemd/system/usnm-ocr-worker.service
NODE_SCRIPT=$R/usr/local/libexec/usnm-ocr/install-worker-node.sh
REJECTED=$R/var/lib/usnm-ocr-update/rejected
LOCK=$R/run/lock/usnm-ocr-maintenance.lock

if ! systemctl is-enabled --quiet usnm-ocr-worker.service 2> /dev/null; then
  log "usnm-ocr-worker.service isn't enabled; nothing to update"
  exit 0
fi
for unit in dnf-automatic.service usnm-ocr-reboot.service; do
  if systemctl is-active --quiet "$unit" 2> /dev/null; then
    log "$unit is running; no update tonight"
    exit 0
  fi
done
if [ "$DRY_RUN" = false ]; then
  mkdir -p "$(dirname "$LOCK")" "$(dirname "$REJECTED")"
  exec 9> "$LOCK"
  if ! flock -n 9; then
    log "another install or the reboot window holds $LOCK; no update tonight"
    exit 0
  fi
  # install-worker-node.sh, run below, takes the lock only when it isn't
  # held for it.
  export USNM_OCR_MAINTENANCE_LOCKED=1
fi

current=$(readlink "$LINK" 2> /dev/null || true)
current=${current#"$LIB"/}
current=${current%/usnm-ocr-worker}
[[ $current =~ $TAG_RE ]] || current=""

# Published releases, oldest first: "tag published_at" lines.
releases=$(curl -fsSL --retry 3 --max-time 60 -H 'Accept: application/vnd.github+json' "$API/releases?per_page=100" |
  jq -r --arg re "$TAG_RE" '.[] | select(.draft == false and .prerelease == false and (.tag_name | test($re)))
    | "\(.published_at) \(.tag_name)"' | LC_ALL=C sort | awk '{ print $2, $1 }') ||
  { warn "warning: can't list the releases of $REPO"; exit 1; }
[ -n "$releases" ] || { warn "warning: no ocr-worker release in $REPO"; exit 1; }

now=$(date -u +%s)
target=""
while read -r tag published; do
  [ -n "$tag" ] || continue
  t=$(jq -rn --arg t "$published" '$t | fromdateiso8601') || continue
  [ $((now - t)) -ge $((DELAY * 3600)) ] || continue
  if [ -s "$REJECTED" ] && grep -qxF "$tag" "$REJECTED"; then continue; fi
  target=$tag
done <<< "$releases"
if [ -z "$target" ]; then
  log "no release older than $DELAY h that this node hasn't rejected"
  exit 0
fi
if [ "$target" = "$current" ]; then
  log "up to date: $current"
  exit 0
fi
# A release published after the target (installed by hand) stays.
if [ -n "$current" ]; then
  pos() { awk -v t="$1" '$1 == t { print NR }' <<< "$releases"; }
  cur_pos=$(pos "$current")
  if [ -n "$cur_pos" ] && [ "$cur_pos" -gt "$(pos "$target")" ]; then
    log "$current is newer than $target; leaving it"
    exit 0
  fi
fi

if [ -n "$LEADER" ]; then
  # The leader's worker must run the target (its version names the commit,
  # the tag's last 7 characters) and have stayed up a while.
  if ! status=$(curl -fsS --max-time 20 "$LEADER"); then
    warn "warning: can't read the leader's status at $LEADER; no update to $target tonight"
    exit 1
  fi
  sha7=${target##*-}
  leader_ok=$(jq -r --arg c "commit $sha7" --argjson up "$((LEADER_UP * 3600))" '
    (.version // "") as $v | ((.started_at // "") | sub("\\.[0-9]+"; "") | try fromdateiso8601 catch null) as $s
    | if ($v | contains($c)) | not then "the leader runs \($v)"
      elif $s == null then "the leader reports no start time"
      elif (now - $s) < $up then "the leader has run it for under \($up / 3600) h"
      else "ok" end' <<< "$status") || leader_ok="the leader's status isn't JSON"
  if [ "$leader_ok" != ok ]; then
    log "waiting for the leader before $target: $leader_ok"
    exit 0
  fi
fi

if [ "$DRY_RUN" = true ]; then
  log "dry run: would update from ${current:-nothing} to $target"
  exit 0
fi
[ -x "$NODE_SCRIPT" ] || { warn "warning: no $NODE_SCRIPT; run install-worker.sh again"; exit 1; }
[ -s "$ENVFILE" ] && [ -s "$UNIT" ] || { warn "warning: no $ENVFILE or $UNIT"; exit 1; }
log "updating from ${current:-nothing} to $target"

TMP=$(mktemp -d /var/tmp/usnm-ocr-update.XXXXXX)
trap 'rm -rf "$TMP"' EXIT
base="https://github.com/$REPO/releases/download/$target"
for f in usnm-ocr-worker usnm-ocr-tesseract.image-id SHA256SUMS; do
  curl -fsSL --retry 3 --proto '=https' -o "$TMP/$f" "$base/$f" ||
    { warn "warning: can't download $f of $target"; exit 1; }
done

# gh with an empty configuration of its own: never signed in, no token.
mkdir -p "$TMP/gh"
verify() {
  local f=$1 digest
  digest=$(sha256sum "$TMP/$f" | awk '{ print $1 }')
  curl -fsSL --retry 3 --max-time 60 -H 'Accept: application/vnd.github+json' "$API/attestations/sha256:$digest" |
    jq -c '.attestations[].bundle' > "$TMP/$f.bundle.jsonl" && [ -s "$TMP/$f.bundle.jsonl" ] || return 1
  env -u GH_TOKEN -u GITHUB_TOKEN GH_CONFIG_DIR="$TMP/gh" GH_NO_UPDATE_NOTIFIER=1 GH_PROMPT_DISABLED=1 \
    gh attestation verify "$TMP/$f" --bundle "$TMP/$f.bundle.jsonl" --repo "$REPO" \
    --signer-workflow "$WORKFLOW" --source-ref refs/heads/main > /dev/null
}
for f in usnm-ocr-worker usnm-ocr-tesseract.image-id SHA256SUMS; do
  verify "$f" || { warn "warning: $f of $target has no valid attestation from $WORKFLOW on main; not updating"; exit 1; }
done
sum_of() { awk -v f="$1" '$2 == f || $2 == "*" f { print $1 }' "$TMP/SHA256SUMS"; }
BIN_SUM=$(sum_of usnm-ocr-worker)
IMAGE_SUM=$(sum_of usnm-ocr-tesseract.oci.tar.gz)
[[ $BIN_SUM =~ ^[0-9a-f]{64}$ ]] && [[ $IMAGE_SUM =~ ^[0-9a-f]{64}$ ]] ||
  { warn "warning: SHA256SUMS of $target lacks the binary or the image archive"; exit 1; }
echo "$BIN_SUM  $TMP/usnm-ocr-worker" | sha256sum --check --quiet ||
  { warn "warning: the binary of $target differs from its SHA256SUMS"; exit 1; }
echo "$(sum_of usnm-ocr-tesseract.image-id)  $TMP/usnm-ocr-tesseract.image-id" | sha256sum --check --quiet ||
  { warn "warning: the image id of $target differs from its SHA256SUMS"; exit 1; }
IMAGE_ID=$(cat "$TMP/usnm-ocr-tesseract.image-id")
[[ $IMAGE_ID =~ ^sha256:[0-9a-f]{64}$ ]] || { warn "warning: the image id of $target isn't sha256:<64 hex digits>"; exit 1; }
log "$target: attestations and checksums verified"

# The node's own settings and unit, with only the image changed.
grep -q '^USNM_OCR_TESSERACT_IMAGE=' "$ENVFILE" || { warn "warning: $ENVFILE has no USNM_OCR_TESSERACT_IMAGE line"; exit 1; }
sed "s|^USNM_OCR_TESSERACT_IMAGE=.*|USNM_OCR_TESSERACT_IMAGE=$IMAGE_ID|" "$ENVFILE" > "$TMP/ocr-worker.env"
cp "$UNIT" "$TMP/usnm-ocr-worker.service"

status=0
"$NODE_SCRIPT" --update --release "$target" --dir "$TMP" --binary-sha256 "$BIN_SUM" --image-id "$IMAGE_ID" \
  --image-url "$base/usnm-ocr-tesseract.oci.tar.gz" --image-sha256 "$IMAGE_SUM" --stay-up-secs "$STAY_UP" || status=$?
case "$status" in
  0) warn "usnm-ocr-worker update applied: ${current:-nothing} -> $target" ;;
  3) warn "usnm-ocr-worker update rolled back: $target -> ${current:-nothing} (the unit didn't stay up); $target is rejected"; exit 1 ;;
  4) warn "usnm-ocr-worker update to $target refused: check-engine failed; $target is rejected"; exit 1 ;;
  *) warn "warning: usnm-ocr-worker update to $target failed ($status); see the journal"; exit 1 ;;
esac
