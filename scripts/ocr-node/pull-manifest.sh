#!/usr/bin/env bash
# Copy a scan prefetch manifest (#368) from the exchange account's ocr-out
# container to the OCR data share, on an OCR node, as root. The node's Arc
# identity can already read ocr-out (ADR-0015 decision 5), so this needs
# no new role, and the file goes from Azure to the node to pve-big's tank
# over the node's NFS mount, never through anyone's laptop.
#
#   pull-manifest.sh --account stusnmoffapzy --index-version pages-v20261010-2
#     [--partial-stamp 20261011T201500Z] [--dest /srv/ocr-data/manifests]
#
# Reads manifests/<version>[/partial-<stamp>]/{manifest.parquet,summary.json}
# and checks the manifest's sha256 against the summary's manifest_sha256
# before it moves both into <dest>/<version>[/partial-<stamp>]/.
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,13s/^# \{0,1\}//p' "$0" >&2; exit 2; }

ACCOUNT="" VERSION="" STAMP="" DEST=/srv/ocr-data/manifests
while [ $# -gt 0 ]; do
  case "$1" in
    --account) ACCOUNT=${2:-}; shift 2 ;;
    --index-version) VERSION=${2:-}; shift 2 ;;
    --partial-stamp) STAMP=${2:-}; shift 2 ;;
    --dest) DEST=${2:-}; shift 2 ;;
    -h|--help) usage ;;
    *) die "unknown option $1" ;;
  esac
done
[[ $ACCOUNT =~ ^[a-z0-9]{3,24}$ ]] || usage
[[ $VERSION =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] || die "--index-version: a version id"
[ -z "$STAMP" ] || [[ $STAMP =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || die "--partial-stamp: yyyymmddThhmmssZ"
[ "$(id -u)" = 0 ] || die "run as root (the Arc agent's token challenge is readable by root and himds)"
command -v jq > /dev/null || die "jq is needed"

rel="manifests/$VERSION${STAMP:+/partial-$STAMP}"
out="$DEST/$VERSION${STAMP:+/partial-$STAMP}"

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
mkdir -p "$out"
for f in summary.json manifest.parquet; do
  cp "$tmp/$f" "$out/$f.tmp"
  mv "$out/$f.tmp" "$out/$f"
done
jq -c '{index_version, partial, pages, manifest_sha256, by_language}' "$out/summary.json"
echo "$out/manifest.parquet"
