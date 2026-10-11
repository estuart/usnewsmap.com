#!/usr/bin/env bash
# The ingest image ($1) runs, and carries the Quickwit binary.
set -euo pipefail
docker run --rm "$1" --version
docker run --rm --entrypoint quickwit "$1" --version
# The search cluster's commands (#239).
docker run --rm --entrypoint usnm-qwcluster "$1" --version
