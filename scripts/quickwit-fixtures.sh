#!/usr/bin/env bash
# Load the synthetic fixture indexes into a local Quickwit node, then serve
# them the way production does (08 §8.4.1): the writer stops and a read-only
# searcher polls the file-backed metastore. For the parity test
# (crates/usnm-search/tests/quickwit_parity.rs) and for running the API with
# USNM_BACKEND=quickwit locally.
#
#   scripts/quickwit-fixtures.sh [--workdir DIR]    # prints the searcher URL
#   kill "$(cat DIR/quickwit.pid)"                  # stops it
#
# Without --workdir it works in a new temporary directory.
#
# QUICKWIT_BIN   path to the quickwit binary (default: quickwit on PATH)
# QUICKWIT_PORT  REST port (default 7280)
set -euo pipefail

usage() { sed -n '2,14s/^# \{0,1\}//p' "$0" >&2; exit 2; }
# The positional form (`quickwit-fixtures.sh DIR`) works for one release,
# with a warning (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/quickwit-fixtures.sh DIR\` is deprecated; use --workdir (#277)" >&2
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
root="$(cd "$(dirname "$0")/.." && pwd)"
[ -n "$work" ] || work=$(mktemp -d)
bin="${QUICKWIT_BIN:-quickwit}"
port="${QUICKWIT_PORT:-7280}"
url="http://127.0.0.1:${port}"

mkdir -p "$work/data" "$work/meta" "$work/indexes"

# node.yaml for one role: `writer` (default services) or `searcher`.
node_config() {
  cat <<YAML
version: 0.8
cluster_id: usnm-$1
node_id: $1
listen_address: 127.0.0.1
rest:
  listen_port: ${port}
data_dir: ${work}/data
default_index_root_uri: file://${work}/indexes
YAML
  if [ "$1" = searcher ]; then
    # Same shape as the sidecar in infra/modules/containerapp.bicep.
    printf 'enabled_services: [searcher, metastore]\nmetastore_uri: file://%s/meta#polling_interval=5s\n' "$work"
  else
    printf 'metastore_uri: file://%s/meta\n' "$work"
  fi
}

start() {
  node_config "$1" > "$work/$1.yaml"
  "$bin" run --config "$work/$1.yaml" > "$work/$1.log" 2>&1 &
  echo $! > "$work/quickwit.pid"
  for _ in $(seq 1 60); do
    curl -sf "$url/api/v1/version" > /dev/null && return 0
    sleep 1
  done
  cat "$work/$1.log" >&2
  exit 1
}

stop() {
  local pid
  pid="$(cat "$work/quickwit.pid")"
  kill "$pid"
  for _ in $(seq 1 60); do
    kill -0 "$pid" 2> /dev/null || return 0
    sleep 1
  done
  echo "quickwit writer did not stop" >&2
  exit 1
}

start writer
for index in pages-base-fixture pages-delta-fixture-1 pages-ja-fixture congress-fixture; do
  # The Japanese pages (#139) and the Congress pages (#229) have their own
  # mappings.
  template=pages-index.yaml
  [ "$index" = pages-ja-fixture ] && template=pages-ja-index.yaml
  [ "$index" = congress-fixture ] && template=congress-index.yaml
  sed -e "s|\${INDEX_ID}|${index}|" -e "s|\${INDEX_URI}|file://${work}/indexes/${index}|" \
    "$root/infra/quickwit/$template" |
    curl -sf -XPOST -H 'content-type: application/yaml' --data-binary @- \
      "$url/api/v1/indexes" > /dev/null
  # The main pages get `text_cg` (05 §5.5.3), and `text_as_cg` where they have
  # American Stories' `text_as` (05 §5.5.4), from the same code as a release.
  docs="$root/fixtures/data/indexes/${index}.jsonl"
  if [ "$template" = pages-index.yaml ] || [ "$template" = congress-index.yaml ]; then
    cargo run -q --manifest-path "$root/Cargo.toml" -p usnm-core --example add_text_cg \
      < "$docs" > "$work/${index}.jsonl"
    docs="$work/${index}.jsonl"
  fi
  curl -sf -XPOST "$url/api/v1/${index}/ingest?commit=force" \
    --data-binary @"$docs" |
    grep -q '"num_rejected_docs": 0' || { echo "ingest of ${index} rejected documents" >&2; exit 1; }
done
# The same base and delta with their common-word pairs at version 1 (05
# §5.5.3), as the indexes built before #168 have them: the parity test
# searches them with queries parsed for that version.
for index in pages-base-fixture pages-delta-fixture-1; do
  copy="${index}-cg1"
  sed -e "s|\${INDEX_ID}|${copy}|" -e "s|\${INDEX_URI}|file://${work}/indexes/${copy}|" \
    "$root/infra/quickwit/pages-index.yaml" |
    curl -sf -XPOST -H 'content-type: application/yaml' --data-binary @- \
      "$url/api/v1/indexes" > /dev/null
  cargo run -q --manifest-path "$root/Cargo.toml" -p usnm-core --example add_text_cg -- --common-grams-version 1 \
    < "$root/fixtures/data/indexes/${index}.jsonl" > "$work/${copy}.jsonl"
  curl -sf -XPOST "$url/api/v1/${copy}/ingest?commit=force" \
    --data-binary @"$work/${copy}.jsonl" |
    grep -q '"num_rejected_docs": 0' || { echo "ingest of ${copy} rejected documents" >&2; exit 1; }
done
# The same base and delta laid out by decade (05 §5.5.5): their pages moved
# over four decades (fixtures/decades.rs), the base partitioned and the
# delta tagged, as a release builds them.
for pair in pages-base-fixture:partitioned pages-delta-fixture-1:tagged; do
  index="${pair%%:*}-decades"
  layout="${pair##*:}"
  cargo run -q --manifest-path "$root/Cargo.toml" -p usnm-ingest --example decade_fixture -- \
    --template "$layout" < "$root/infra/quickwit/pages-index.yaml" |
    sed -e "s|\${INDEX_ID}|${index}|" -e "s|\${INDEX_URI}|file://${work}/indexes/${index}|" |
    curl -sf -XPOST -H 'content-type: application/yaml' --data-binary @- \
      "$url/api/v1/indexes" > /dev/null
  cargo run -q --manifest-path "$root/Cargo.toml" -p usnm-ingest --example decade_fixture -- \
    --docs < "$root/fixtures/data/indexes/${pair%%:*}.jsonl" > "$work/${index}.jsonl"
  curl -sf -XPOST "$url/api/v1/${index}/ingest?commit=force" \
    --data-binary @"$work/${index}.jsonl" |
    grep -q '"num_rejected_docs": 0' || { echo "ingest of ${index} rejected documents" >&2; exit 1; }
done
# The same base and delta with one searched field for both texts (05
# §5.5.7, #283): the mapping and the `text_all` fields as a release writes
# them.
for from in pages-base-fixture pages-delta-fixture-1; do
  index="${from}-single"
  cargo run -q --manifest-path "$root/Cargo.toml" -p usnm-ingest --example single_field_fixture -- \
    --template < "$root/infra/quickwit/pages-index.yaml" |
    sed -e "s|\${INDEX_ID}|${index}|" -e "s|\${INDEX_URI}|file://${work}/indexes/${index}|" |
    curl -sf -XPOST -H 'content-type: application/yaml' --data-binary @- \
      "$url/api/v1/indexes" > /dev/null
  cargo run -q --manifest-path "$root/Cargo.toml" -p usnm-ingest --example single_field_fixture -- \
    --docs < "$root/fixtures/data/indexes/${from}.jsonl" > "$work/${index}.jsonl"
  curl -sf -XPOST "$url/api/v1/${index}/ingest?commit=force" \
    --data-binary @"$work/${index}.jsonl" |
    grep -q '"num_rejected_docs": 0' || { echo "ingest of ${index} rejected documents" >&2; exit 1; }
done
stop
start searcher
echo "$url"
