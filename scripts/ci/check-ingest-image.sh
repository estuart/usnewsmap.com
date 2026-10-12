#!/usr/bin/env bash
# The ingest image (--image) runs, and carries the Quickwit binary.
set -euo pipefail
usage() { echo "usage: scripts/ci/check-ingest-image.sh --image IMAGE" >&2; exit 2; }
# The positional form (`check-ingest-image.sh IMAGE`) works for one
# release, with a warning (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/ci/check-ingest-image.sh IMAGE\` is deprecated; use --image (#277)" >&2
  set -- --image "$1"
fi
image=""
while [ $# -gt 0 ]; do
  case $1 in
    --image) [ $# -ge 2 ] || usage; image=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$image" ] || usage
docker run --rm "$image" --version
docker run --rm --entrypoint quickwit "$image" --version
# The search cluster's commands (#239).
docker run --rm --entrypoint usnm-qwcluster "$image" --version
# pdftotext, which congress-serialset reads GovInfo's PDFs with (#229).
docker run --rm --entrypoint pdftotext "$image" -v
