#!/usr/bin/env bash
# Run a saved query (ops/queries/<name>.kql) against an environment's Log
# Analytics workspace and print the result as a table.
#
#   scripts/logs.sh prod release-progress          # the last day
#   scripts/logs.sh prod curation-throughput 6h    # 30m, 6h, 2d or ISO 8601 (PT6H)
#   scripts/logs.sh prod list                      # the saved queries
#
# Needs az signed in with an account that can read the workspace (Log
# Analytics Reader or more) and jq. The workspace accepts Entra tokens only
# (ADR-0009); its id comes from the stack output LOG_ANALYTICS_WORKSPACE_ID.
# The ocr-node-* queries run against the OCR nodes' own workspace instead
# (usnm-ocr-log-<env>, OCR_NODE_LOG_ANALYTICS_WORKSPACE_ID; ADR-0015).
set -euo pipefail
[ $# -ge 2 ] && [ $# -le 3 ] || { sed -n '2,13s/^# \{0,1\}//p' "$0" >&2; exit 2; }
ENV_NAME=$1
NAME=$2
SPAN=${3:-1d}
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
. scripts/lib/env.sh

if [ "$NAME" = list ]; then
  for f in ops/queries/*.kql; do
    printf '%-22s %s\n' "$(basename "$f" .kql)" "$(sed -n '1s#^// *##p' "$f")"
  done
  exit 0
fi
[[ $NAME =~ ^[a-z0-9-]+$ ]] && [ -f "ops/queries/$NAME.kql" ] ||
  die "no saved query \"$NAME\"; see: scripts/logs.sh $ENV_NAME list"
command -v jq > /dev/null || die "jq is needed to build the request and print the result"

# 30m / 6h / 2d → ISO 8601; anything starting with P passes through.
case $SPAN in
  P*) ;;
  *[0-9]m) SPAN="PT${SPAN%m}M" ;;
  *[0-9]h) SPAN="PT${SPAN%h}H" ;;
  *[0-9]d) SPAN="P${SPAN%d}D" ;;
  *) die "timespan \"$SPAN\": use 30m, 6h, 2d or ISO 8601 (PT6H)" ;;
esac
[[ $SPAN =~ ^P([0-9]+D)?(T([0-9]+H)?([0-9]+M)?)?$ ]] && [ "$SPAN" != P ] && [ "$SPAN" != PT ] ||
  die "timespan \"$SPAN\" isn't an ISO 8601 duration"

[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME"
# The nodes' data is only in their workspace.
case $NAME in
  ocr-node-*) ws_key=OCR_NODE_LOG_ANALYTICS_WORKSPACE_ID ;;
  *) ws_key=LOG_ANALYTICS_WORKSPACE_ID ;;
esac
workspace=$(aget "$ws_key")
[ -n "$workspace" ] ||
  die "$ws_key isn't set for $ENV_NAME; run scripts/provision.sh $ENV_NAME to save the stack outputs"

body=$(mktemp)
trap 'rm -f "$body"' EXIT
jq -n --rawfile q "ops/queries/$NAME.kql" --arg t "$SPAN" '{query: $q, timespan: $t}' > "$body"
result=$(az rest --method post \
  --resource https://api.loganalytics.io \
  --url "https://api.loganalytics.io/v1/workspaces/$workspace/query" \
  --headers Content-Type=application/json \
  --body "@$body" -o json) || die "the query failed (is az signed in to the environment's tenant?)"

# One tab-separated line per row, header first; tabs and newlines inside
# values become spaces, and empty values "-" (column merges empty fields).
jq -r '.tables[0] as $t
  | ($t.columns | map(.name)), ($t.rows[] | map(if . == null then "" else tostring end))
  | map(gsub("[\t\r\n]+"; " ") | if . == "" then "-" else . end) | @tsv' <<< "$result" |
  column -t -s "$(printf '\t')"
rows=$(jq '.tables[0].rows | length' <<< "$result")
echo "($rows rows, $SPAN)" >&2
