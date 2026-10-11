#!/usr/bin/env bash
# The scan-fetch container on pve-big (#368, 12 §12.2.4): an unprivileged
# Debian 12 LXC whose only job is `usnm-ocr-worker prefetch`, fetching
# scans from LoC into the host's tank/ocr/scans. It holds no Azure
# credentials; the manifest reaches /tank/ocr/manifests from a node
# (pull-manifest.sh). Run on the Proxmox host as root, after
# pve-ocr-storage.sh; safe to run again.
#
#   scanfetch-ct.sh --create [--ctid 211] [--hostname usnm-scanfetch]
#     [--storage ssd] [--bridge vmbr0] [--cores 2] [--memory 1024]
#     [--template local:vztmpl/debian-12-standard_12.12-1_amd64.tar.zst]
#     [--data /tank/ocr]
#   scanfetch-ct.sh --install --release ocr-worker-<yyyymmdd>-<sha7> [--ctid 211]
#   scanfetch-ct.sh --configure --manifest /tank/ocr/manifests/<file>.parquet
#     [--resolution full] [--order priority] [--max-total-bytes N]
#     [--concurrency 2] [--enable] [--ctid 211]
#
# --create makes the container (onboot, one NIC on --bridge with DHCP,
# nesting for systemd's sandboxing) with <data>/scans bind-mounted
# read-write at /tank/ocr/scans (mp0) and <data>/manifests read-only at
# /tank/ocr/manifests (mp1), and inside it the user scanfetch, uid and gid
# 2000, which the unprivileged mapping makes the host's 102000 (ocrdata).
# --install downloads the release's usnm-ocr-worker and SHA256SUMS inside
# the container, straight from GitHub, checks the sum, and points
# /usr/local/bin/usnm-ocr-worker at /usr/local/lib/usnm-ocr-worker/<release>/.
# --configure writes /etc/usnm/scanfetch.env and the unit
# (usnm-scanfetch.service, beside this script), and with --enable starts it
# (restarts it if it was running, which goes on where it stopped).
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,27s/^# \{0,1\}//p' "$0" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd)

CTID=211 NAME=usnm-scanfetch STORAGE=ssd BRIDGE=vmbr0 CORES=2 MEMORY=1024
TEMPLATE=local:vztmpl/debian-12-standard_12.12-1_amd64.tar.zst DATA=/tank/ocr
CREATE=false INSTALL=false CONFIGURE=false ENABLE=false RELEASE=""
MANIFEST="" RESOLUTION=full ORDER=priority MAX_BYTES="" CONCURRENCY=2
REPO=tgoodyear/usnewsmap.com
FETCH_UID=2000
while [ $# -gt 0 ]; do
  case "$1" in
    --create) CREATE=true; shift ;;
    --install) INSTALL=true; shift ;;
    --configure) CONFIGURE=true; shift ;;
    --enable) ENABLE=true; shift ;;
    --ctid) CTID=${2:-}; shift 2 ;;
    --hostname) NAME=${2:-}; shift 2 ;;
    --storage) STORAGE=${2:-}; shift 2 ;;
    --bridge) BRIDGE=${2:-}; shift 2 ;;
    --cores) CORES=${2:-}; shift 2 ;;
    --memory) MEMORY=${2:-}; shift 2 ;;
    --template) TEMPLATE=${2:-}; shift 2 ;;
    --data) DATA=${2:-}; shift 2 ;;
    --release) RELEASE=${2:-}; shift 2 ;;
    --manifest) MANIFEST=${2:-}; shift 2 ;;
    --resolution) RESOLUTION=${2:-}; shift 2 ;;
    --order) ORDER=${2:-}; shift 2 ;;
    --max-total-bytes) MAX_BYTES=${2:-}; shift 2 ;;
    --concurrency) CONCURRENCY=${2:-}; shift 2 ;;
    -h|--help) usage ;;
    *) die "unknown option $1" ;;
  esac
done
$CREATE || $INSTALL || $CONFIGURE || usage
[[ $CTID =~ ^[0-9]+$ ]] || die "--ctid: a number"
[ "$(id -u)" = 0 ] || die "run as root on the Proxmox host"
command -v pct > /dev/null || die "no pct here: run on the Proxmox host"
in_ct() { pct exec "$CTID" -- "$@"; }

if $CREATE; then
  [ -d "$DATA/scans" ] && [ -d "$DATA/manifests" ] ||
    die "$DATA/scans and $DATA/manifests must exist: run pve-ocr-storage.sh first"
  if ! pct status "$CTID" > /dev/null 2>&1; then
    pct create "$CTID" "$TEMPLATE" \
      --hostname "$NAME" --unprivileged 1 --features nesting=1 \
      --cores "$CORES" --memory "$MEMORY" --swap 0 \
      --rootfs "$STORAGE:4" --onboot 1 --ostype debian \
      --net0 "name=eth0,bridge=$BRIDGE,ip=dhcp,firewall=0" \
      --mp0 "$DATA/scans,mp=/tank/ocr/scans" \
      --mp1 "$DATA/manifests,mp=/tank/ocr/manifests,ro=1" \
      --description "usnewsmap scan prefetch (#368): the only LoC fetcher while it runs. scripts/ocr-node/scanfetch-ct.sh"
  else
    echo "CT $CTID exists; leaving its configuration as it is"
  fi
  pct status "$CTID" | grep -q running || pct start "$CTID"
  for _ in $(seq 30); do
    in_ct getent hosts deb.debian.org > /dev/null 2>&1 && break
    sleep 2
  done
  in_ct bash -c 'DEBIAN_FRONTEND=noninteractive apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get install -y -q curl ca-certificates'
  in_ct bash -c "getent group scanfetch > /dev/null || groupadd --gid $FETCH_UID scanfetch
    id scanfetch > /dev/null 2>&1 || useradd --uid $FETCH_UID --gid $FETCH_UID --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin scanfetch
    mkdir -p /etc/usnm"
  in_ct su -s /bin/sh scanfetch -c 'touch /tank/ocr/scans/.write-check && rm /tank/ocr/scans/.write-check' ||
    die "scanfetch can't write /tank/ocr/scans: is it owned by 102000 on the host?"
fi

if $INSTALL; then
  [[ $RELEASE =~ ^ocr-worker-[0-9]{8}-[0-9a-f]{7}$ ]] || die "--release: ocr-worker-<yyyymmdd>-<sha7>"
  url="https://github.com/$REPO/releases/download/$RELEASE"
  in_ct bash -euo pipefail -c "
    dir=/usr/local/lib/usnm-ocr-worker/$RELEASE
    tmp=\$(mktemp -d)
    trap 'rm -rf \$tmp' EXIT
    curl -fsSL --proto =https -o \$tmp/usnm-ocr-worker '$url/usnm-ocr-worker'
    curl -fsSL --proto =https -o \$tmp/SHA256SUMS '$url/SHA256SUMS'
    (cd \$tmp && grep ' usnm-ocr-worker\$' SHA256SUMS | sha256sum -c -)
    mkdir -p \$dir
    install -m 0755 \$tmp/usnm-ocr-worker \$dir/usnm-ocr-worker
    ln -sfn \$dir/usnm-ocr-worker /usr/local/bin/usnm-ocr-worker
    /usr/local/bin/usnm-ocr-worker --version"
fi

if $CONFIGURE; then
  [ -n "$MANIFEST" ] || die "--configure needs --manifest"
  [[ $MANIFEST == /tank/ocr/manifests/* ]] || die "--manifest must be under /tank/ocr/manifests"
  [[ $RESOLUTION == full || $RESOLUTION =~ ^scaled:[1-9][0-9]?$ ]] || die "--resolution: full or scaled:<1-99>"
  [[ $ORDER == priority || $ORDER == issue ]] || die "--order: priority or issue"
  [ -z "$MAX_BYTES" ] || [[ $MAX_BYTES =~ ^[0-9]+$ ]] || die "--max-total-bytes: a number of bytes"
  [[ $CONCURRENCY =~ ^[1-4]$ ]] || die "--concurrency: 1 to 4"
  in_ct test -r "$MANIFEST" || die "$MANIFEST isn't readable in CT $CTID"
  env="# Written by scripts/ocr-node/scanfetch-ct.sh --configure.
USNM_PREFETCH_MANIFEST=$MANIFEST
USNM_PREFETCH_OUT=/tank/ocr/scans
USNM_PREFETCH_LOCK=/tank/ocr/scans/.prefetch.lock
USNM_PREFETCH_RESOLUTION=$RESOLUTION
USNM_PREFETCH_ORDER=$ORDER
USNM_PREFETCH_CONCURRENCY=$CONCURRENCY
USNM_OCR_API_GAP_MS=3500
USNM_OCR_TILE_GAP_MS=1000
RUST_LOG=info"
  [ -z "$MAX_BYTES" ] || env+="
USNM_PREFETCH_MAX_TOTAL_BYTES=$MAX_BYTES"
  printf '%s\n' "$env" | in_ct bash -c 'cat > /etc/usnm/scanfetch.env.tmp && chmod 0644 /etc/usnm/scanfetch.env.tmp && mv /etc/usnm/scanfetch.env.tmp /etc/usnm/scanfetch.env'
  pct push "$CTID" "$HERE/usnm-scanfetch.service" /etc/systemd/system/usnm-scanfetch.service
  in_ct systemctl daemon-reload
  if $ENABLE; then
    in_ct systemctl enable usnm-scanfetch.service
    in_ct systemctl restart usnm-scanfetch.service
    sleep 5
    in_ct systemctl --no-pager status usnm-scanfetch.service | head -5
  fi
fi
