#!/usr/bin/env bash
# Copy a scan prefetch manifest (#368) from the exchange account's ocr-out
# container to pve-big, on an OCR node, as root. The node's Arc identity can
# already read ocr-out (ADR-0015 decision 5), so this needs no new role,
# and the file goes from Azure to the node to the scan server in the
# scan-fetch container, never through anyone's laptop.
#
#   pull-manifest.sh --account stusnmoffapzy --index-version pages-v20261010-2
#     --scan-server http://HOST:8380 [--partial-stamp 20261011T201500Z]
#
# Reads manifests/<version>[/partial-<stamp>]/{manifest.parquet,summary.json},
# checks the manifest's sha256 against the summary's manifest_sha256, and
# PUTs both to the scan server's /manifests/<version>[-partial-<stamp>]/,
# which lands in /tank/ocr/manifests.
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,14s/^# \{0,1\}//p' "$0" >&2; exit 2; }

ACCOUNT="" VERSION="" STAMP="" SERVER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --account) ACCOUNT=${2:-}; shift 2 ;;
    --index-version) VERSION=${2:-}; shift 2 ;;
    --partial-stamp) STAMP=${2:-}; shift 2 ;;
    --scan-server) SERVER=${2:-}; shift 2 ;;
    -h|--help) usage ;;
    *) die "unknown option $1" ;;
  esac
done
[[ $ACCOUNT =~ ^[a-z0-9]{3,24}$ ]] || usage
[[ $SERVER =~ ^https?://[A-Za-z0-9.-]+(:[0-9]+)?$ ]] || die "--scan-server: http://HOST:PORT"
[[ $VERSION =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] || die "--index-version: a version id"
[ -z "$STAMP" ] || [[ $STAMP =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || die "--partial-stamp: yyyymmddThhmmssZ"
[ "$(id -u)" = 0 ] || die "run as root (the Arc agent's token challenge is readable by root and himds)"
command -v jq > /dev/null || die "jq is needed"

rel="manifests/$VERSION${STAMP:+/partial-$STAMP}"
dest="$VERSION${STAMP:+-partial-$STAMP}"

# A storage token from the Arc agent: its first answer is 401 with a
# challenge file, whose contents go back as Basic auth.
ep="http://localhost:40342/metadata/identity/oauth2/token?api-version=2020-06-01&resource=https%3A%2F%2Fstorage.azure.com%2F"
challenge=$(curl -s -o /dev/null -D - -H Metadata:true "$ep" | tr -d '\r' |
  sed -n 's/^[Ww][Ww][Ww]-[Aa]uthenticate: Basic realm=//p')
[[ $challenge == /var/opt/azcmagent/tokens/*.key ]] || die "no Arc token challenge (is this an Arc-connected node?)"
token=$(curl -sf -H Metadata:true -H "Authorization: Basic $(cat "$challenge")" "$ep" | jq -r .access_token)
[ -n "$token" ] && [ "$token" != null ] || die "no storage token from the Arc agent"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
get() {
  curl -sf --proto =https -H "Authorization: Bearer $token" -H "x-ms-version: 2023-11-03" \
    -o "$tmp/$1" "https://$ACCOUNT.blob.core.windows.net/ocr-out/$rel/$1" ||
    die "can't read ocr-out/$rel/$1"
}
get summary.json
get manifest.parquet
want=$(jq -r .manifest_sha256 "$tmp/summary.json")
got=$(sha256sum "$tmp/manifest.parquet" | cut -d' ' -f1)
[ "$want" = "$got" ] || die "manifest sha256 $got isn't the summary's $want"
for f in manifest.parquet summary.json; do
  curl -sf --max-time 600 -T "$tmp/$f" "$SERVER/manifests/$dest/$f" > /dev/null ||
    die "the scan server didn't take $f"
done
jq -c '{index_version, partial, pages, manifest_sha256, by_language}' "$tmp/summary.json"
echo "/tank/ocr/manifests/$dest/manifest.parquet"
