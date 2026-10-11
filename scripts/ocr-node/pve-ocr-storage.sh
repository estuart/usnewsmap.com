#!/usr/bin/env bash
# The OCR data store on the Proxmox host pve-big (#368, 12 §12.2.4): ZFS
# datasets under <pool>/ocr for the scans fetched ahead of the workers and
# for ALTO and text later, and their owner. Run on the host as root; safe
# to run again.
#
#   pve-ocr-storage.sh [--pool tank] [--quota 3T] [--dry-run]
#
# Creates, or sets the properties of:
#   <pool>/ocr        /<pool>/ocr        lz4, atime=off, quota --quota
#   <pool>/ocr/scans  /<pool>/ocr/scans  lz4, recordsize=1M (JPEGs, written once)
#   <pool>/ocr/text   /<pool>/ocr/text   zstd, recordsize=128K
# and a plain directory /<pool>/ocr/manifests for the prefetch manifests.
#
# Everything is owned by ocrdata (uid and gid 102000), mode 2775. 102000 is
# what uid 2000 in an unprivileged container maps to, so the scan-fetch
# container (scanfetch-ct.sh) writes as its own uid 2000.
#
# Nothing is exported. The nodes read scans from the container's scan
# server over HTTP, not NFS (ADR-0015 decision 9), so an export file an
# earlier version wrote, /etc/exports.d/usnm-ocr.exports, is removed.
# nfs-kernel-server stays installed, with no usnm exports.
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,22s/^# \{0,1\}//p' "$0" >&2; exit 2; }

POOL=tank QUOTA=3T DRY=false
OWNER=ocrdata OWNER_ID=102000
EXPORTS=/etc/exports.d/usnm-ocr.exports
while [ $# -gt 0 ]; do
  case "$1" in
    --pool) POOL=${2:-}; shift 2 ;;
    --quota) QUOTA=${2:-}; shift 2 ;;
    --dry-run) DRY=true; shift ;;
    -h|--help) usage ;;
    *) die "unknown option $1" ;;
  esac
done
[[ $POOL =~ ^[A-Za-z0-9_.-]+$ ]] || die "--pool: a pool name"
[[ $QUOTA =~ ^[0-9]+[KMGTP]?$ ]] || die "--quota: a ZFS size such as 3T"
[ "$(id -u)" = 0 ] || die "run as root on the Proxmox host"
command -v zfs > /dev/null || die "no zfs here"

run() { if $DRY; then echo "+ $*"; else "$@"; fi; }

# A dataset with these properties, created or brought up to date.
dataset() {
  local name=$1; shift
  if zfs list -H -o name "$name" > /dev/null 2>&1; then
    for p in "$@"; do run zfs set "$p" "$name"; done
  else
    local opts=()
    for p in "$@"; do opts+=(-o "$p"); done
    run zfs create "${opts[@]}" "$name"
  fi
}

dataset "$POOL/ocr" compression=lz4 atime=off "quota=$QUOTA"
dataset "$POOL/ocr/scans" compression=lz4 recordsize=1M
dataset "$POOL/ocr/text" compression=zstd recordsize=128K

if ! getent group "$OWNER" > /dev/null; then
  run groupadd --system --gid "$OWNER_ID" "$OWNER"
fi
if ! id "$OWNER" > /dev/null 2>&1; then
  run useradd --system --uid "$OWNER_ID" --gid "$OWNER_ID" --no-create-home \
    --home-dir /nonexistent --shell /usr/sbin/nologin "$OWNER"
fi
[ "$(id -u "$OWNER" 2> /dev/null || echo "$OWNER_ID")" = "$OWNER_ID" ] ||
  $DRY || die "$OWNER exists with another uid"

ROOT=$(zfs get -H -o value mountpoint "$POOL/ocr" 2> /dev/null || echo "/$POOL/ocr")
run mkdir -p "$ROOT/manifests"
for d in "$ROOT" "$ROOT/scans" "$ROOT/text" "$ROOT/manifests"; do
  run chown "$OWNER_ID:$OWNER_ID" "$d"
  run chmod 2775 "$d"
done

# No NFS export: remove the one an earlier version wrote.
[ ! -f "$EXPORTS" ] || run rm -f "$EXPORTS"
# Reloaded every run, so an export left active by a failed earlier run goes.
if command -v exportfs > /dev/null; then run exportfs -ra; fi
run zfs list -o name,mountpoint,quota,compression,recordsize,atime -r "$POOL/ocr"
