#!/usr/bin/env bash
# A Proxmox host's stats for the OCR dashboard (usnm-ocrdash, 12 §12.2.5):
# the node's CPU, memory and load and its guests (from pvesh), and the ZFS
# pools and datasets named, as one JSON file (usnm_ocr_contract::status::
# HostStats). Run as root on the host; nothing here changes the host.
#
#   hoststats.sh --collect --out FILE [--pool NAME]... [--dataset NAME]...
#     [--push-url http://<dashboard>/api/host/<host>]
#   hoststats.sh --install --out FILE [--pool NAME]... [--dataset NAME]...
#     [--push-url URL]
#
# --collect writes FILE (through a temporary file and a rename, mode 0644),
# and with --push-url also PUTs it to the dashboard, which takes it only
# from this host's address. --install copies this script to
# /usr/local/sbin/usnm-hoststats and writes and starts a systemd timer that
# runs --collect with the same options every minute
# (usnm-hoststats.service and .timer).
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,17s/^# \{0,1\}//p' "$0" >&2; exit 2; }

COLLECT=false INSTALL=false OUT="" PUSH_URL=""
POOLS=() DATASETS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --collect) COLLECT=true; shift ;;
    --install) INSTALL=true; shift ;;
    --out) OUT=${2:-}; shift 2 ;;
    --pool) POOLS+=("${2:-}"); shift 2 ;;
    --dataset) DATASETS+=("${2:-}"); shift 2 ;;
    --push-url) PUSH_URL=${2:-}; shift 2 ;;
    -h|--help) usage ;;
    *) die "unknown option $1" ;;
  esac
done
$COLLECT || $INSTALL || usage
[ -n "$OUT" ] || die "--out is needed"
[[ $OUT == /* ]] || die "--out: an absolute path"
[ -z "$PUSH_URL" ] || [[ $PUSH_URL =~ ^http://[A-Za-z0-9.-]+(:[0-9]+)?/api/host/[A-Za-z0-9-]+$ ]] ||
  die "--push-url: http://<dashboard>[:port]/api/host/<host>"
for n in "${POOLS[@]}" "${DATASETS[@]}"; do
  [[ $n =~ ^[A-Za-z0-9_./-]+$ ]] || die "not a pool or dataset name: $n"
done
[ "$(id -u)" = 0 ] || die "run as root on the Proxmox host"
command -v pvesh > /dev/null || die "no pvesh here: run on the Proxmox host"

# A JSON string from text: backslashes, quotes and control characters escaped.
jstr() {
  local s
  s=$(printf '%s' "$1" | tr -d '\000-\010\013\014\016-\037')
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\t'/ }
  s=${s//$'\r'/ }
  s=${s//$'\n'/ }
  printf '"%s"' "$s"
}
# A number, or 0.
num() { [[ $1 =~ ^[0-9]+(\.[0-9]+)?$ ]] && printf '%s' "$1" || printf 0; }

collect() {
  local host node resources pools="" datasets="" sep
  host=$(hostname)
  node=$(pvesh get "/nodes/$host/status" --output-format json)
  resources=$(pvesh get /cluster/resources --type vm --output-format json)
  sep=""
  for p in "${POOLS[@]}"; do
    local name size alloc free frag cap health statusx scan errors
    read -r name size alloc free frag cap health < <(zpool list -Hp -o name,size,alloc,free,frag,cap,health "$p")
    statusx=$(zpool status -x "$p" 2> /dev/null | head -n 1)
    scan=$(zpool status "$p" | sed -n 's/^ *scan: //p' | head -n 1)
    errors=$(zpool status "$p" | sed -n 's/^errors: //p' | head -n 1)
    pools+="$sep{\"name\":$(jstr "$name"),\"size\":$(num "$size"),\"alloc\":$(num "$alloc"),\"free\":$(num "$free"),\"frag_pct\":$(num "${frag%\%}"),\"cap_pct\":$(num "${cap%\%}"),\"health\":$(jstr "$health"),\"status_x\":$(jstr "$statusx"),\"scan\":$(jstr "$scan"),\"errors\":$(jstr "$errors")}"
    sep=","
  done
  sep=""
  for d in "${DATASETS[@]}"; do
    local used avail quota logical ratio
    used=$(zfs get -Hp -o value used "$d")
    avail=$(zfs get -Hp -o value available "$d")
    quota=$(zfs get -Hp -o value quota "$d")
    logical=$(zfs get -Hp -o value logicalused "$d")
    ratio=$(zfs get -Hp -o value compressratio "$d")
    datasets+="$sep{\"name\":$(jstr "$d"),\"used\":$(num "$used"),\"available\":$(num "$avail"),\"quota\":$(num "$quota"),\"logicalused\":$(num "$logical"),\"compressratio\":$(num "${ratio%x}")}"
    sep=","
  done
  printf '{"schema":1,"host":%s,"collected_at":"%s","node":%s,"resources":%s,"pools":[%s],"datasets":[%s]}\n' \
    "$(jstr "$host")" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$node" "$resources" "$pools" "$datasets"
}

if $COLLECT; then
  mkdir -p "$(dirname "$OUT")"
  tmp=$(mktemp "$OUT.XXXXXX")
  trap 'rm -f "$tmp"' EXIT
  collect > "$tmp"
  chmod 0644 "$tmp"
  mv "$tmp" "$OUT"
  trap - EXIT
  if [ -n "$PUSH_URL" ]; then
    curl -fsS --max-time 10 -X PUT -H 'Content-Type: application/json' --data-binary "@$OUT" "$PUSH_URL" ||
      echo "warning: can't push to $PUSH_URL" >&2
  fi
fi

if $INSTALL; then
  install -m 0755 "$0" /usr/local/sbin/usnm-hoststats
  args="--collect --out $OUT"
  for p in "${POOLS[@]}"; do args+=" --pool $p"; done
  for d in "${DATASETS[@]}"; do args+=" --dataset $d"; done
  [ -z "$PUSH_URL" ] || args+=" --push-url $PUSH_URL"
  cat > /etc/systemd/system/usnm-hoststats.service <<UNIT
# Written by scripts/ocr-node/hoststats.sh --install: this host's stats for
# the OCR dashboard (usnm-ocrdash).
[Unit]
Description=usnewsmap OCR dashboard host stats

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/usnm-hoststats $args
Nice=10
UNIT
  cat > /etc/systemd/system/usnm-hoststats.timer <<UNIT
# Written by scripts/ocr-node/hoststats.sh --install.
[Unit]
Description=usnewsmap OCR dashboard host stats, every minute

[Timer]
OnBootSec=30s
OnUnitActiveSec=60s
AccuracySec=5s

[Install]
WantedBy=timers.target
UNIT
  systemctl daemon-reload
  systemctl enable --now usnm-hoststats.timer
  systemctl start usnm-hoststats.service
  echo "usnm-hoststats.timer enabled; $OUT:"
  head -c 300 "$OUT"
  echo
fi
