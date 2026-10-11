#!/usr/bin/env bash
# Stop what scripts/local-azure/up.sh started in DIR. The data (Azurite
# blobs, cosmos.json) stays, so up.sh on the same DIR picks it back up.
#
#   scripts/local-azure/down.sh --workdir DIR
set -euo pipefail
usage() { echo "usage: scripts/local-azure/down.sh --workdir DIR" >&2; exit 2; }
# The positional form (`down.sh DIR`) works for one release, with a warning
# (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/local-azure/down.sh DIR\` is deprecated; use --workdir (#277)" >&2
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
[ -n "$work" ] || usage
[ -f "$work/pids" ] || { echo "no $work/pids" >&2; exit 1; }
while read -r pid; do
  kill "$pid" 2> /dev/null || true
done < "$work/pids"
# Wait until they have exited, so up.sh can take the ports straight away.
while read -r pid; do
  for _ in $(seq 1 50); do
    kill -0 "$pid" 2> /dev/null || break
    sleep 0.2
  done
done < "$work/pids"
rm "$work/pids"
