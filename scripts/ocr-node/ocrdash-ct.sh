#!/usr/bin/env bash
# The OCR dashboard's container on pve-big (12 §12.2.5): an unprivileged
# Debian 12 LXC running usnm-ocrdash, a read-only page of the scan fetch,
# the OCR nodes and the Proxmox hosts, for the LAN only. It holds no Azure
# credentials. Run on the Proxmox host as root; safe to run again.
#
#   ocrdash-ct.sh --create [--ctid 212] [--hostname usnm-ocrdash]
#     [--storage ssd] [--bridge vmbr0] [--cores 1] [--memory 768] [--swap 256]
#     [--template local:vztmpl/debian-12-standard_12.12-1_amd64.tar.zst]
#     [--data /tank/ocr] [--hoststats /var/lib/usnm-hoststats]
#   ocrdash-ct.sh --install --release ocr-worker-<yyyymmdd>-<sha7> [--ctid 212]
#   ocrdash-ct.sh --configure [--allow lan|<range>]
#     [--worker <name>=http://<host>:<port>]... [--host-push <host>=<its name>]...
#     [--workbook-url URL] [--port 80]
#     [--https-name ocr.tjg.fyi [--certs /var/lib/usnm-certs]] [--ctid 212]
#
# --create makes the container (onboot, one NIC on --bridge with DHCP,
# nesting for systemd's sandboxing) with three read-only bind mounts:
# <data>/scans and <data>/manifests at the same paths (the fetcher's status,
# index and failures, and the manifest its status names), and --hoststats
# at /srv/hoststats (this host's stats, from hoststats.sh). Inside it, the
# user ocrdash (uid and gid 2001) runs the dashboard.
# --install downloads the release's usnm-ocrdash and SHA256SUMS inside the
# container, straight from GitHub, checks the sum, and points
# /usr/local/bin/usnm-ocrdash at /usr/local/lib/usnm-ocrdash/<release>/.
# --configure installs nftables with an input policy that drops everything
# but --allow (and loopback, replies and IPv6 neighbour discovery), writes
# /etc/usnm/ocrdash.env and the unit usnm-ocrdash.service (beside this
# script), and starts it. With --https-name, it bind-mounts
# <certs>/<name> read-only at /etc/usnm/tls (home-acme on this host keeps
# fullchain.pem and privkey.pem there, the key readable by ocrdash's group
# only) and the dashboard serves HTTPS on 443 with them; port 80 then only
# redirects to https://<name>/ and takes the hosts' pushes. home-acme runs
# `systemctl reload usnm-ocrdash` after a renewal. The nftables rule takes
# any port from --allow, so 443 needs no rule of its own. --allow lan (the
# default) is the subnet the container is on, read when it runs; the
# dashboard reads it too. A
# --host-push host may PUT its own stats; the dashboard resolves its name
# and checks the address. No address goes into the repository.
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,39s/^# \{0,1\}//p' "$0" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd)

CTID=212 NAME=usnm-ocrdash STORAGE=ssd BRIDGE=vmbr0 CORES=1 MEMORY=768 SWAP=256
TEMPLATE=local:vztmpl/debian-12-standard_12.12-1_amd64.tar.zst DATA=/tank/ocr
HOSTSTATS=/var/lib/usnm-hoststats
CREATE=false INSTALL=false CONFIGURE=false RELEASE=""
ALLOW=lan PORT=80 WORKBOOK_URL="" HTTPS_NAME="" CERTS=/var/lib/usnm-certs
WORKERS=() PUSHERS=()
REPO=tgoodyear/usnewsmap.com
DASH_UID=2001
while [ $# -gt 0 ]; do
  case "$1" in
    --create) CREATE=true; shift ;;
    --install) INSTALL=true; shift ;;
    --configure) CONFIGURE=true; shift ;;
    --ctid) CTID=${2:-}; shift 2 ;;
    --hostname) NAME=${2:-}; shift 2 ;;
    --storage) STORAGE=${2:-}; shift 2 ;;
    --bridge) BRIDGE=${2:-}; shift 2 ;;
    --cores) CORES=${2:-}; shift 2 ;;
    --memory) MEMORY=${2:-}; shift 2 ;;
    --swap) SWAP=${2:-}; shift 2 ;;
    --template) TEMPLATE=${2:-}; shift 2 ;;
    --data) DATA=${2:-}; shift 2 ;;
    --hoststats) HOSTSTATS=${2:-}; shift 2 ;;
    --release) RELEASE=${2:-}; shift 2 ;;
    --allow) ALLOW=${2:-}; shift 2 ;;
    --port) PORT=${2:-}; shift 2 ;;
    --worker) WORKERS+=("${2:-}"); shift 2 ;;
    --host-push) PUSHERS+=("${2:-}"); shift 2 ;;
    --workbook-url) WORKBOOK_URL=${2:-}; shift 2 ;;
    --https-name) HTTPS_NAME=${2:-}; shift 2 ;;
    --certs) CERTS=${2:-}; shift 2 ;;
    -h|--help) usage ;;
    *) die "unknown option $1" ;;
  esac
done
$CREATE || $INSTALL || $CONFIGURE || usage
[[ $CTID =~ ^[0-9]+$ ]] || die "--ctid: a number"
[[ $MEMORY =~ ^[0-9]+$ && $SWAP =~ ^[0-9]+$ ]] || die "--memory and --swap: MB"
[ "$(id -u)" = 0 ] || die "run as root on the Proxmox host"
command -v pct > /dev/null || die "no pct here: run on the Proxmox host"
in_ct() { pct exec "$CTID" -- "$@"; }

if $CREATE; then
  [ -d "$DATA/scans" ] && [ -d "$DATA/manifests" ] ||
    die "$DATA/scans and $DATA/manifests must exist: run pve-ocr-storage.sh first"
  mkdir -p "$HOSTSTATS"
  if ! pct status "$CTID" > /dev/null 2>&1; then
    pct create "$CTID" "$TEMPLATE" \
      --hostname "$NAME" --unprivileged 1 --features nesting=1 \
      --cores "$CORES" --memory "$MEMORY" --swap "$SWAP" \
      --rootfs "$STORAGE:4" --onboot 1 --ostype debian \
      --net0 "name=eth0,bridge=$BRIDGE,ip=dhcp,firewall=0" \
      --mp0 "$DATA/scans,mp=/tank/ocr/scans,ro=1" \
      --mp1 "$DATA/manifests,mp=/tank/ocr/manifests,ro=1" \
      --mp2 "$HOSTSTATS,mp=/srv/hoststats,ro=1" \
      --description "usnewsmap OCR dashboard (12 §12.2.5): read-only, LAN only. scripts/ocr-node/ocrdash-ct.sh"
  else
    pct set "$CTID" --memory "$MEMORY" --swap "$SWAP"
    echo "CT $CTID exists; memory $MEMORY MB, swap $SWAP MB, the rest left as it is"
  fi
  pct status "$CTID" | grep -q running || pct start "$CTID"
  for _ in $(seq 30); do
    in_ct getent hosts deb.debian.org > /dev/null 2>&1 && break
    sleep 2
  done
  in_ct bash -c 'DEBIAN_FRONTEND=noninteractive apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get install -y -q curl ca-certificates nftables'
  in_ct bash -c "getent group ocrdash > /dev/null || groupadd --gid $DASH_UID ocrdash
    id ocrdash > /dev/null 2>&1 || useradd --uid $DASH_UID --gid $DASH_UID --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin ocrdash
    mkdir -p /etc/usnm"
  in_ct su -s /bin/sh ocrdash -c 'test -r /tank/ocr/scans/status-full.json || ls /tank/ocr/scans > /dev/null' ||
    die "ocrdash can't read /tank/ocr/scans"
fi

if $INSTALL; then
  [[ $RELEASE =~ ^ocr-worker-[0-9]{8}-[0-9a-f]{7}$ ]] || die "--release: ocr-worker-<yyyymmdd>-<sha7>"
  url="https://github.com/$REPO/releases/download/$RELEASE"
  in_ct bash -euo pipefail -c "
    dir=/usr/local/lib/usnm-ocrdash/$RELEASE
    tmp=\$(mktemp -d)
    trap 'rm -rf \$tmp' EXIT
    curl -fsSL --proto =https -o \$tmp/usnm-ocrdash '$url/usnm-ocrdash'
    curl -fsSL --proto =https -o \$tmp/SHA256SUMS '$url/SHA256SUMS'
    (cd \$tmp && grep ' usnm-ocrdash\$' SHA256SUMS | sha256sum -c -)
    mkdir -p \$dir
    install -m 0755 \$tmp/usnm-ocrdash \$dir/usnm-ocrdash
    ln -sfn \$dir/usnm-ocrdash /usr/local/bin/usnm-ocrdash
    /usr/local/bin/usnm-ocrdash --version"
  if in_ct systemctl is-active --quiet usnm-ocrdash.service; then
    in_ct systemctl restart usnm-ocrdash.service
  fi
fi

if $CONFIGURE; then
  if [ "$ALLOW" = lan ]; then
    NFT_ALLOW=$(in_ct ip -4 -o route show scope link proto kernel dev eth0 | awk '{print $1}' | head -n 1)
  else
    NFT_ALLOW=$ALLOW
  fi
  [[ $NFT_ALLOW =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+/[0-9]+$ ]] || die "--allow: lan, or an IPv4 range (<address>/<bits>)"
  [[ $PORT =~ ^[0-9]+$ ]] || die "--port: a number"
  for w in "${WORKERS[@]}"; do
    [[ $w =~ ^[A-Za-z0-9-]+=http://[A-Za-z0-9.-]+:[0-9]+$ ]] || die "--worker: <name>=http://<host>:<port>, not $w"
  done
  for p in "${PUSHERS[@]}"; do
    [[ $p =~ ^[A-Za-z0-9-]+=[A-Za-z0-9.-]+$ ]] || die "--host-push: <host>=<its name>, not $p"
  done
  [ -z "$WORKBOOK_URL" ] || [[ $WORKBOOK_URL =~ ^https://portal\.azure\.com/[^[:space:]\"\'\<\>]+$ ]] ||
    die "--workbook-url: an https://portal.azure.com/ link"
  if [ -n "$HTTPS_NAME" ]; then
    [[ $HTTPS_NAME =~ ^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$ && $HTTPS_NAME != *..* && ${#HTTPS_NAME} -le 253 ]] ||
      die "--https-name: a host name"
    [ -s "$CERTS/$HTTPS_NAME/fullchain.pem" ] && [ -s "$CERTS/$HTTPS_NAME/privkey.pem" ] ||
      die "no certificate in $CERTS/$HTTPS_NAME: run home-infra's scripts/acme-setup.sh first"
    mp="$CERTS/$HTTPS_NAME,mp=/etc/usnm/tls,ro=1"
    # A changed mount point applies only when the container starts again,
    # so it is rebooted here.
    if [ "$(pct config "$CTID" | sed -n 's/^mp3: //p')" != "$mp" ]; then
      pct set "$CTID" --mp3 "$mp"
      echo "mp3 is $mp now; rebooting CT $CTID to apply it"
      pct reboot "$CTID"
      for _ in $(seq 60); do
        pct status "$CTID" | grep -q running && in_ct test -f /etc/usnm/tls/fullchain.pem 2> /dev/null && break
        sleep 2
      done
    fi
    in_ct su -s /bin/sh ocrdash -c 'test -r /etc/usnm/tls/privkey.pem' ||
      die "ocrdash can't read /etc/usnm/tls/privkey.pem (group of the files on the host: ocrdash's mapped gid)"
  fi
  in_ct bash -c "cat > /etc/nftables.conf.tmp <<NFT
#!/usr/sbin/nft -f
# Written by scripts/ocr-node/ocrdash-ct.sh --configure: the LAN only.
flush ruleset
table inet usnm {
  chain input {
    type filter hook input priority 0; policy drop;
    iif lo accept
    ct state established,related accept
    ip saddr $NFT_ALLOW accept
    meta l4proto ipv6-icmp accept
  }
}
NFT
mv /etc/nftables.conf.tmp /etc/nftables.conf && nft -f /etc/nftables.conf && systemctl enable nftables > /dev/null"
  join() { local IFS=,; echo "$*"; }
  env="# Written by scripts/ocr-node/ocrdash-ct.sh --configure.
USNM_OCRDASH_LISTEN=0.0.0.0:$PORT
USNM_OCRDASH_ALLOW=$ALLOW
USNM_OCRDASH_SCANS=/tank/ocr/scans
USNM_OCRDASH_HOST_FILE=/srv/hoststats/$(hostname).json
USNM_OCRDASH_STATE_DIR=/var/lib/usnm-ocrdash
RUST_LOG=info"
  [ ${#WORKERS[@]} -eq 0 ] || env+="
USNM_OCRDASH_WORKER=$(join "${WORKERS[@]}")"
  [ ${#PUSHERS[@]} -eq 0 ] || env+="
USNM_OCRDASH_HOST_PUSH=$(join "${PUSHERS[@]}")"
  [ -z "$WORKBOOK_URL" ] || env+="
USNM_OCRDASH_WORKBOOK_URL=$WORKBOOK_URL"
  [ -z "$HTTPS_NAME" ] || env+="
USNM_OCRDASH_TLS_CERT=/etc/usnm/tls/fullchain.pem
USNM_OCRDASH_TLS_KEY=/etc/usnm/tls/privkey.pem
USNM_OCRDASH_TLS_LISTEN=0.0.0.0:443
USNM_OCRDASH_HTTPS_NAME=$HTTPS_NAME"
  printf '%s\n' "$env" | in_ct bash -c 'cat > /etc/usnm/ocrdash.env.tmp && chmod 0644 /etc/usnm/ocrdash.env.tmp && mv /etc/usnm/ocrdash.env.tmp /etc/usnm/ocrdash.env'
  pct push "$CTID" "$HERE/usnm-ocrdash.service" /etc/systemd/system/usnm-ocrdash.service
  in_ct systemctl daemon-reload
  in_ct systemctl enable usnm-ocrdash.service
  in_ct systemctl restart usnm-ocrdash.service
  sleep 3
  in_ct systemctl --no-pager status usnm-ocrdash.service | head -5
  if [ -n "$HTTPS_NAME" ]; then
    echo "dashboard: https://$HTTPS_NAME/"
  else
    echo "dashboard: http://$(in_ct hostname -I | cut -d' ' -f1)$([ "$PORT" = 80 ] || echo ":$PORT")/"
  fi
fi
