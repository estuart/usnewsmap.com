#!/usr/bin/env bash
# Print one OCR quality audit run's (or jaocr.py mixed or american-stories run's) table rows and summary as JSON lines, from
# the environment's Log Analytics workspace: the "ocr quality" rows the
# reducing replica logs (every table) and the "ocr quality finished" line,
# without timestamps. The audit's own outputs (audit/*.json, CSVs) are in the
# curated store, which the network rules keep private; the logs are not.
#
#   scripts/ja-ocr/quality-rows.sh --env prod --execution caj-usnm-jaocr-prod-a6wvy42 > rows.jsonl
#   scripts/ja-ocr/quality-rows.sh --env prod --execution caj-usnm-jaocr-prod-a6wvy42 --span 3d
#   scripts/ja-ocr/quality-rows.sh --env prod --execution caj-usnm-jaone-prod-x1y2z3a   # a mixed or american-stories run
#
# Needs az signed in with an account that can read the workspace and jq.
set -euo pipefail
usage() { sed -n '2,12s/^# \{0,1\}//p' "$0" >&2; exit 2; }
# The positional form (`quality-rows.sh prod EXECUTION [SPAN]`) works for
# one release, with a warning (#277).
if [ $# -ge 2 ] && [ $# -le 3 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/ja-ocr/quality-rows.sh ENV EXECUTION [SPAN]\` is deprecated; use --env, --execution and --span (#277)" >&2
  set -- --env "$1" --execution "$2" --span "${3:-2d}"
fi
ENV_NAME="" EXECUTION="" SPAN=2d
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --execution) [ $# -ge 2 ] || usage; EXECUTION=$2; shift 2 ;;
    --span) [ $# -ge 2 ] || usage; SPAN=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] && [ -n "$EXECUTION" ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/../.."
. scripts/lib/env.sh
command -v jq > /dev/null || die "jq is needed"
# Both name forms (ADR-0017): caj-usnm-jaocr-<env>-<id> until Phase 3,
# usnm-jaocr-caj-<env>-<id> after; the same for jaone.
[[ $EXECUTION =~ ^(caj-usnm-ja(ocr|one)|usnm-ja(ocr|one)-caj)-[a-z0-9]+-[a-z0-9]+$ ]] ||
  die "\"$EXECUTION\" isn't an execution of the ja-ocr job or its one-off job (caj-usnm-jaocr-…, usnm-jaocr-caj-… or the jaone forms)"
[[ $SPAN =~ ^[0-9]+[hd]$ ]] || die "timespan \"$SPAN\": use e.g. 12h or 3d"
case $SPAN in *h) iso="PT${SPAN%h}H" ;; *d) iso="P${SPAN%d}D" ;; esac
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME"
workspace=$(aget LOG_ANALYTICS_WORKSPACE_ID)
[ -n "$workspace" ] || die "LOG_ANALYTICS_WORKSPACE_ID isn't set for $ENV_NAME"

query="ContainerAppConsoleLogs
| where ContainerName == \"jaocr\" and ContainerGroupName startswith \"$EXECUTION-\"
| where (Log has \"ocr quality\" or Log has \"mixed pages\" or Log has \"american stories\") and Log !has \"IDENTITY_HEADER\" and Log !has \"MSI_SECRET\"
| extend j = parse_json(Log)
| where tostring(j.message) in (\"ocr quality\", \"ocr quality finished\", \"mixed pages\", \"mixed pages finished\", \"american stories\", \"american stories finished\", \"american stories year written\", \"american stories write finished\")
| project TimeGenerated, Log
| order by TimeGenerated asc"
body=$(mktemp)
trap 'rm -f "$body"' EXIT
jq -n --arg q "$query" --arg t "$iso" '{query: $q, timespan: $t}' > "$body"
az rest --method post --resource https://api.loganalytics.io \
  --url "https://api.loganalytics.io/v1/workspaces/$workspace/query" \
  --headers Content-Type=application/json --body "@$body" -o json |
  jq -c '.tables[0].rows[] | .[1] | fromjson | del(.ts)'
