#!/usr/bin/env bash
# Start local stand-ins for the Azure data services, so usnm-ingest and
# usnm-api run their Blob and Cosmos code paths (Entra tokens included)
# without an Azure account:
#
#   Azurite (Blob, HTTPS + OAuth)  127.0.0.1:10000
#   shim.py: Blob over HTTP        127.0.0.1:10010   -> Azurite
#            managed identity      127.0.0.1:10020
#   cosmos.py: Cosmos DB           127.0.0.1:10030
#
# Creates the curated, reference and cache containers, then writes
# DIR/env with the variables the binaries read.
#
#   scripts/local-azure/up.sh [--workdir DIR]     # prints the env file's path
#   . DIR/env
#   scripts/local-azure/down.sh --workdir DIR
#
# Without --workdir it works in a new temporary directory.
#
# AZURITE_BIN   azurite-blob binary (default: azurite-blob on PATH;
#               `npm install -g azurite`)
# COSMOS_PAGE, COSMOS_429, COSMOS_SEED   passed to cosmos.py
set -euo pipefail

usage() { sed -n '2,22s/^# \{0,1\}//p' "$0" >&2; exit 2; }
# The positional form (`up.sh DIR`) works for one release, with a warning
# (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/local-azure/up.sh DIR\` is deprecated; use --workdir (#277)" >&2
  set -- --workdir "$1"
fi
work=""
while [ $# -gt 0 ]; do
  case $1 in
    --workdir) [ $# -ge 2 ] || usage; work=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
here="$(cd "$(dirname "$0")" && pwd)"
[ -n "$work" ] || work=$(mktemp -d)
mkdir -p "$work"
work="$(cd "$work" && pwd)"
azurite="${AZURITE_BIN:-azurite-blob}"
header="local"

for port in 10000 10010 10020 10030; do
  if (exec 3<> "/dev/tcp/127.0.0.1/${port}") 2> /dev/null; then
    echo "port ${port} is in use; stop the other instance (down.sh) first" >&2
    exit 1
  fi
done

mkdir -p "$work/azurite"
: > "$work/pids"

# If any step fails, stop what already started so the ports are free again.
started=false
stop_on_failure() {
  $started && return
  while read -r pid; do
    kill "$pid" 2> /dev/null || true
  done < "$work/pids"
  rm -f "$work/pids"
}
trap stop_on_failure EXIT

wait_for() { # url, log
  for _ in $(seq 1 60); do
    curl -sk -o /dev/null "$1" && return 0
    sleep 0.5
  done
  cat "$2" >&2
  exit 1
}

if [ ! -f "$work/cert.pem" ]; then
  openssl req -x509 -newkey rsa:2048 -nodes -days 365 -subj /CN=127.0.0.1 \
    -addext subjectAltName=IP:127.0.0.1 \
    -keyout "$work/key.pem" -out "$work/cert.pem" 2> /dev/null
fi

"$azurite" --location "$work/azurite" --blobHost 127.0.0.1 --blobPort 10000 \
  --oauth basic --cert "$work/cert.pem" --key "$work/key.pem" \
  --skipApiVersionCheck > "$work/azurite.log" 2>&1 &
echo $! >> "$work/pids"
wait_for https://127.0.0.1:10000/ "$work/azurite.log"

SHIM_IDENTITY_HEADER="$header" python3 "$here/shim.py" > "$work/shim.log" 2>&1 &
echo $! >> "$work/pids"
wait_for http://127.0.0.1:10010/ "$work/shim.log"

COSMOS_FILE="$work/cosmos.json" python3 "$here/cosmos.py" > "$work/cosmos.log" 2>&1 &
echo $! >> "$work/pids"
wait_for http://127.0.0.1:10030/ "$work/cosmos.log"

token="$(curl -sf -H "X-IDENTITY-HEADER: ${header}" \
  'http://127.0.0.1:10020/msi/token?api-version=2019-08-01&resource=https://storage.azure.com/' |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["access_token"])')"
for container in curated reference cache; do
  status="$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
    -H "Authorization: Bearer ${token}" -H 'x-ms-version: 2023-11-03' \
    -H "x-ms-date: $(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S GMT')" \
    "http://127.0.0.1:10010/devstoreaccount1/${container}?restype=container")"
  case "$status" in
    201 | 409) ;; # 409: kept from an earlier run in this workdir
    *) echo "creating container ${container}: HTTP ${status}" >&2; exit 1 ;;
  esac
done

cat > "$work/env" << ENV
export IDENTITY_ENDPOINT=http://127.0.0.1:10020/msi/token
export IDENTITY_HEADER=${header}
export USNM_COSMOS_ENDPOINT=http://127.0.0.1:10030/
export USNM_CURATED_URL=http://127.0.0.1:10010/devstoreaccount1/curated
export USNM_REFERENCE_URL=http://127.0.0.1:10010/devstoreaccount1/reference
export USNM_RESPONSE_CACHE_URL=http://127.0.0.1:10010/devstoreaccount1/cache
ENV
started=true
echo "$work/env"
