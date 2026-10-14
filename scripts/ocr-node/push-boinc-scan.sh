#!/usr/bin/env bash
# Push scans the fetcher already holds to the BOINC hand-off container (#469,
# docs/boinc.md#one-page-from-the-scan-cache), for the hand-off job's export. Runs in the
# scan-fetch container (CT 211), signed in as its Arc machine identity
# (az login --identity --allow-no-subscriptions), which has the hand-off writer role on the
# container and nothing else there. It reads /tank/ocr and asks LoC for nothing.
#
#   push-boinc-scan.sh --account <exchange account> --scan /tank/ocr/scans/keep/<batch>/.../<seq>.jp2 \
#     [--scan ...] [--manifest /tank/ocr/manifests/<version>/manifest.parquet]
#     [--scans-root /tank/ocr/scans] [--manifests-root /tank/ocr/manifests] [--what-if]
#
# Each --scan goes up with its sidecar (<scan>.json) as scans/<path under --scans-root>, and
# --manifest as manifests/<path under --manifests-root> (the hand-off job's HANDOFF_MANIFEST),
# so each manifest snapshot keeps its own name. A blob already there with the same size is
# left as it is (the fetcher never rewrites a scan or a snapshot), so a second run is safe;
# one with another size stops the run. --what-if prints the uploads.
set -euo pipefail
usage() { awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "$0" >&2; exit 2; }
ACCOUNT="" MANIFEST="" SCANS_ROOT=/tank/ocr/scans MANIFESTS_ROOT=/tank/ocr/manifests WHAT_IF=false
SCANS=()
while [ $# -gt 0 ]; do
  case $1 in
    --account) [ $# -ge 2 ] || usage; ACCOUNT=$2; shift 2 ;;
    --scan) [ $# -ge 2 ] || usage; SCANS+=("$2"); shift 2 ;;
    --manifest) [ $# -ge 2 ] || usage; MANIFEST=$2; shift 2 ;;
    --scans-root) [ $# -ge 2 ] || usage; SCANS_ROOT=${2%/}; shift 2 ;;
    --manifests-root) [ $# -ge 2 ] || usage; MANIFESTS_ROOT=${2%/}; shift 2 ;;
    --what-if) WHAT_IF=true; shift ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
die() { echo "error: $*" >&2; exit 1; }
[ -n "$ACCOUNT" ] && { [ ${#SCANS[@]} -gt 0 ] || [ -n "$MANIFEST" ]; } || usage
[[ $ACCOUNT =~ ^[a-z0-9]{3,24}$ ]] || die "--account is a storage account name"
CONTAINER=boinc-handoff
# Blob names are /-separated segments of [A-Za-z0-9._-], starting with a letter or digit
# (usnm_store::validate_path), so the hand-off job can copy them.
segments='^[A-Za-z0-9][A-Za-z0-9._-]*(/[A-Za-z0-9][A-Za-z0-9._-]*)*$'
uploads=()
for s in ${SCANS[@]+"${SCANS[@]}"}; do
  case $s in "$SCANS_ROOT"/*) ;; *) die "--scan $s is not under $SCANS_ROOT" ;; esac
  [ -f "$s" ] && [ ! -L "$s" ] || die "--scan $s is not a regular file"
  [ -f "$s.json" ] || die "--scan $s has no sidecar $s.json"
  rel=${s#"$SCANS_ROOT"/}
  [[ $rel =~ $segments ]] && [[ $rel != *..* ]] || die "--scan $s: its path has characters a blob name can't take"
  uploads+=("$s" "scans/$rel" "$s.json" "scans/$rel.json")
done
if [ -n "$MANIFEST" ]; then
  case $MANIFEST in "$MANIFESTS_ROOT"/*) ;; *) die "--manifest $MANIFEST is not under $MANIFESTS_ROOT" ;; esac
  [ -f "$MANIFEST" ] && [ ! -L "$MANIFEST" ] || die "--manifest $MANIFEST is not a regular file"
  rel=${MANIFEST#"$MANIFESTS_ROOT"/}
  [[ $rel =~ $segments ]] && [[ $rel != *..* ]] || die "--manifest $MANIFEST: its path has characters a blob name can't take"
  uploads+=("$MANIFEST" "manifests/$rel")
fi
command -v az > /dev/null || die "az is not installed"
for ((i = 0; i < ${#uploads[@]}; i += 2)); do
  file=${uploads[i]} blob=${uploads[i + 1]}
  if $WHAT_IF; then
    echo "would upload $file to $CONTAINER/$blob"
    continue
  fi
  size=$(az storage blob show --auth-mode login --account-name "$ACCOUNT" --container-name "$CONTAINER" \
    --name "$blob" --query properties.contentLength -o tsv --only-show-errors 2> /dev/null) || size=""
  if [ -n "$size" ]; then
    [ "$size" = "$(wc -c < "$file" | tr -d ' ')" ] || die "$CONTAINER/$blob is already there with another size ($size bytes)"
    echo "already there: $CONTAINER/$blob" >&2
    continue
  fi
  az storage blob upload --auth-mode login --account-name "$ACCOUNT" --container-name "$CONTAINER" \
    --name "$blob" --file "$file" --no-progress --only-show-errors -o none || die "can't upload $file"
  echo "uploaded $CONTAINER/$blob" >&2
done
