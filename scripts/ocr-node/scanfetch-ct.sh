#!/usr/bin/env bash
# The scan-fetch container on pve-big (#368, 12 §12.2.4): an unprivileged
# Debian 12 LXC whose only job is `usnm-ocr-worker prefetch`, fetching
# scans from LoC into the host's tank/ocr/scans, and serving them to the
# OCR nodes over HTTP (serve-scans). It holds no Azure credentials; a node
# PUTs the manifest to the scan server (pull-manifest.sh). Run on the
# Proxmox host as root, after pve-ocr-storage.sh; safe to run again.
#
#   scanfetch-ct.sh --create [--ctid 211] [--hostname usnm-scanfetch]
#     [--storage ssd] [--bridge vmbr0] [--cores 2] [--memory 4096] [--swap 512]
#     [--template local:vztmpl/debian-12-standard_12.12-1_amd64.tar.zst]
#     [--data /tank/ocr]
#   scanfetch-ct.sh --install --release ocr-worker-<yyyymmdd>-<sha7> [--ctid 211]
#   scanfetch-ct.sh --configure --manifest /tank/ocr/manifests/<file>.parquet
#     [--resolution full] [--buffer-resolution full] [--sets both]
#     [--buffer-cap BYTES] [--order priority] [--max-total-bytes N]
#     [--concurrency 4] [--enable] [--ctid 211]
#   scanfetch-ct.sh --serve [--allow HOST,HOST] [--port 8380]
#     [--ctid 211]
#
# --create makes the container (onboot, one NIC on --bridge with DHCP,
# nesting for systemd's sandboxing) with <data>/scans bind-mounted
# read-write at /tank/ocr/scans (mp0) and <data>/manifests read-write at
# /tank/ocr/manifests (mp1), and inside it the user scanfetch, uid and gid
# 2000, which the unprivileged mapping makes the host's 102000 (ocrdata).
# --install downloads the release's usnm-ocr-worker and SHA256SUMS inside
# the container, straight from GitHub, checks the sum, and points
# /usr/local/bin/usnm-ocr-worker at /usr/local/lib/usnm-ocr-worker/<release>/.
# --configure writes /etc/usnm/scanfetch.env and the unit
# (usnm-scanfetch.service, beside this script), and with --enable starts it
# (restarts it if it was running, which goes on where it stopped).
# --serve installs nftables in the container with a rule that drops the
# scan server's port from anyone outside the container's own LAN (its eth0
# subnet, read when it runs), writes /etc/usnm/scanserve.env and the unit
# usnm-scanserve.service, and starts it. The server itself answers only the
# --allow hosts (default: the two OCR nodes), by name: it resolves them at
# startup and every 5 minutes. The names resolve only through the UniFi
# gateway's local DNS. No address is written into this script. The
# manifests mount must be read-write for the server's PUT; a container made
# before that is switched over and rebooted.
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,40s/^# \{0,1\}//p' "$0" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd)

CTID=211 NAME=usnm-scanfetch STORAGE=ssd BRIDGE=vmbr0 CORES=2 MEMORY=4096 SWAP=512
TEMPLATE=local:vztmpl/debian-12-standard_12.12-1_amd64.tar.zst DATA=/tank/ocr
CREATE=false INSTALL=false CONFIGURE=false ENABLE=false RELEASE=""
MANIFEST="" RESOLUTION=full ORDER=priority MAX_BYTES="" CONCURRENCY=4
BUFFER_RESOLUTION=full SETS=both BUFFER_CAP=1000000000000
SERVE=false PORT=8380
# The scan server's clients, the OCR nodes, by host name (LAN DNS only).
OCR_NODES=usnm-ocr-node3-arcs-prod.tjg.fyi,usnm-ocr-node4-arcs-prod.tjg.fyi
ALLOW=$OCR_NODES
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
    --swap) SWAP=${2:-}; shift 2 ;;
    --template) TEMPLATE=${2:-}; shift 2 ;;
    --data) DATA=${2:-}; shift 2 ;;
    --release) RELEASE=${2:-}; shift 2 ;;
    --manifest) MANIFEST=${2:-}; shift 2 ;;
    --resolution) RESOLUTION=${2:-}; shift 2 ;;
    --order) ORDER=${2:-}; shift 2 ;;
    --max-total-bytes) MAX_BYTES=${2:-}; shift 2 ;;
    --buffer-resolution) BUFFER_RESOLUTION=${2:-}; shift 2 ;;
    --sets) SETS=${2:-}; shift 2 ;;
    --buffer-cap) BUFFER_CAP=${2:-}; shift 2 ;;
    --concurrency) CONCURRENCY=${2:-}; shift 2 ;;
    --serve) SERVE=true; shift ;;
    --allow) ALLOW=${2:-}; shift 2 ;;
    --port) PORT=${2:-}; shift 2 ;;
    -h|--help) usage ;;
    *) die "unknown option $1" ;;
  esac
done
$CREATE || $INSTALL || $CONFIGURE || $SERVE || usage
[[ $CTID =~ ^[0-9]+$ ]] || die "--ctid: a number"
[[ $MEMORY =~ ^[0-9]+$ && $SWAP =~ ^[0-9]+$ ]] || die "--memory and --swap: MB"
[ "$(id -u)" = 0 ] || die "run as root on the Proxmox host"
command -v pct > /dev/null || die "no pct here: run on the Proxmox host"
in_ct() { pct exec "$CTID" -- "$@"; }

if $CREATE; then
  [ -d "$DATA/scans" ] && [ -d "$DATA/manifests" ] ||
    die "$DATA/scans and $DATA/manifests must exist: run pve-ocr-storage.sh first"
  if ! pct status "$CTID" > /dev/null 2>&1; then
    pct create "$CTID" "$TEMPLATE" \
      --hostname "$NAME" --unprivileged 1 --features nesting=1 \
      --cores "$CORES" --memory "$MEMORY" --swap "$SWAP" \
      --rootfs "$STORAGE:4" --onboot 1 --ostype debian \
      --net0 "name=eth0,bridge=$BRIDGE,ip=dhcp,firewall=0" \
      --mp0 "$DATA/scans,mp=/tank/ocr/scans" \
      --mp1 "$DATA/manifests,mp=/tank/ocr/manifests" \
      --description "usnewsmap scan prefetch (#368): the only LoC fetcher while it runs. scripts/ocr-node/scanfetch-ct.sh"
  else
    # Memory and swap apply live; the rest of an existing CT is left as it is.
    pct set "$CTID" --memory "$MEMORY" --swap "$SWAP"
    echo "CT $CTID exists; memory $MEMORY MB, swap $SWAP MB, the rest left as it is"
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
  [[ $BUFFER_RESOLUTION == full || $BUFFER_RESOLUTION =~ ^scaled:[1-9][0-9]?$ ]] || die "--buffer-resolution: full or scaled:<1-99>"
  [[ $SETS == keep || $SETS == buffer || $SETS == both ]] || die "--sets: keep, buffer or both"
  [[ $BUFFER_CAP =~ ^[0-9]+$ ]] || die "--buffer-cap: a number of bytes"
  [[ $CONCURRENCY =~ ^[1-4]$ ]] || die "--concurrency: 1 to 4"
  in_ct test -r "$MANIFEST" || die "$MANIFEST isn't readable in CT $CTID"
  env="# Written by scripts/ocr-node/scanfetch-ct.sh --configure.
USNM_PREFETCH_MANIFEST=$MANIFEST
USNM_PREFETCH_OUT=/tank/ocr/scans
USNM_PREFETCH_LOCK=/tank/ocr/scans/.prefetch.lock
USNM_PREFETCH_RESOLUTION=$RESOLUTION
USNM_PREFETCH_BUFFER_RESOLUTION=$BUFFER_RESOLUTION
USNM_PREFETCH_SETS=$SETS
USNM_PREFETCH_BUFFER_CAP=$BUFFER_CAP
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

if $SERVE; then
  [[ $PORT =~ ^[0-9]{2,5}$ ]] && [ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || die "--port: 1 to 65535"
  IFS=, read -r -a HOSTS <<< "$ALLOW"
  [ "${#HOSTS[@]}" -gt 0 ] || die "--allow: no hosts"
  for h in "${HOSTS[@]}"; do
    [[ $h =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ ]] || die "--allow: $h isn't a host name"
    in_ct getent ahostsv4 "$h" > /dev/null || die "--allow: $h doesn't resolve in CT $CTID"
  done
  # A release from before host names took addresses only.
  in_ct /usr/local/bin/usnm-ocr-worker serve-scans --help | grep -q -- --allow-refresh-secs ||
    die "CT $CTID's usnm-ocr-worker takes addresses only in --allow: --install a newer release first"
  # The container's LAN, for the nftables rule: its eth0 subnet.
  LAN=$(in_ct ip -4 route show dev eth0 scope link proto kernel | awk 'NR == 1 { print $1 }')
  [[ $LAN =~ ^[0-9.]+/[0-9]+$ ]] || die "CT $CTID: no IPv4 subnet on eth0"
  # The manifests mount read-write, for PUT /manifests.
  # The mount changes only when the container starts again, so it is
  # rebooted here (a fetch in it stops and goes on where it stopped).
  if pct config "$CTID" | grep -q '^mp1:.*ro=1'; then
    pct set "$CTID" --mp1 "$DATA/manifests,mp=/tank/ocr/manifests"
    echo "mp1 is read-write now; rebooting CT $CTID to apply it"
    pct reboot "$CTID"
    for _ in $(seq 60); do
      pct status "$CTID" | grep -q running && in_ct true 2> /dev/null && break
      sleep 2
    done
    in_ct su -s /bin/sh scanfetch -c 'touch /tank/ocr/manifests/.write-check && rm /tank/ocr/manifests/.write-check' ||
      die "the manifests mount still isn't writable in CT $CTID"
  fi
  allow_list=$(IFS=,; echo "${HOSTS[*]}")
  in_ct bash -c 'DEBIAN_FRONTEND=noninteractive apt-get install -y -q nftables > /dev/null'
  in_ct bash -c "cat > /etc/nftables.conf.tmp <<NFT
#!/usr/sbin/nft -f
# Written by scripts/ocr-node/scanfetch-ct.sh --serve: the scan server's
# port only from this container's LAN; the server answers only the nodes.
flush ruleset
table inet usnm {
  chain input {
    type filter hook input priority 0; policy accept;
    tcp dport $PORT ip saddr != $LAN drop
    tcp dport $PORT ip6 saddr ::/0 drop
  }
}
NFT
mv /etc/nftables.conf.tmp /etc/nftables.conf && nft -f /etc/nftables.conf && systemctl enable nftables > /dev/null"
  printf '%s\n' "# Written by scripts/ocr-node/scanfetch-ct.sh --serve." \
    "USNM_SCANSERVE_LISTEN=0.0.0.0:$PORT" "USNM_SCANSERVE_ALLOW=$allow_list" "RUST_LOG=info" |
    in_ct bash -c 'cat > /etc/usnm/scanserve.env.tmp && mv /etc/usnm/scanserve.env.tmp /etc/usnm/scanserve.env'
  in_ct install -d -o scanfetch -g scanfetch -m 2775 /tank/ocr/scans/buffer /tank/ocr/scans/keep
  pct push "$CTID" "$HERE/usnm-scanserve.service" /etc/systemd/system/usnm-scanserve.service
  in_ct systemctl daemon-reload
  in_ct systemctl enable usnm-scanserve.service
  in_ct systemctl restart usnm-scanserve.service
  sleep 2
  in_ct systemctl --no-pager status usnm-scanserve.service | head -5
  echo "scan server: http://$(in_ct hostname -I | cut -d' ' -f1):$PORT"
fi
