#!/usr/bin/env bash
# Summarize the anonymous search log (the searches container, ADR-0012):
# top queries, searches per day, the searches that found nothing, and the
# share of searches by date span and by decade partitions touched (#123).
#
#   scripts/searches.sh --env prod             # the last 30 days
#   scripts/searches.sh --env prod --days 7    # the last 7 days
#
# Reads with the operator's Entra sign-in (az storage ... --auth-mode login,
# ADR-0009), which needs Storage Blob Data Reader on the container: add your
# object id to USNM_SEARCH_LOG_READERS and provision. The data account takes
# connections only through its private endpoint, so run this from a network
# that reaches it. The raw queries stay on this machine and are deleted at exit.
set -euo pipefail
usage() { sed -n '2,13s/^# \{0,1\}//p' "$0" >&2; exit 2; }
# The positional form (`searches.sh prod [DAYS]`) works for one release,
# with a warning (#277).
if [ $# -ge 1 ] && [ $# -le 2 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/searches.sh ENV [DAYS]\` is deprecated; use --env and --days (#277)" >&2
  set -- --env "$1" --days "${2:-30}"
fi
ENV_NAME="" DAYS=30
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --days) [ $# -ge 2 ] || usage; DAYS=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
. scripts/lib/env.sh

[[ $DAYS =~ ^[1-9][0-9]{0,3}$ ]] || die "days must be a whole number from 1 to 9999"
command -v jq > /dev/null || die "jq is needed to summarize the records"
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME"
account=$(aget STORAGE_ACCOUNT)
[ -n "$account" ] ||
  die "STORAGE_ACCOUNT isn't set for $ENV_NAME; run scripts/provision.sh --env $ENV_NAME to save the stack outputs"

# The first day to include, UTC (GNU date, then BSD date).
since=$(date -u -d "-$((DAYS - 1)) days" +%F 2> /dev/null || date -u -v "-$((DAYS - 1))d" +%F)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

blobs() {
  az storage blob list --auth-mode login --account-name "$account" \
    --container-name searches --prefix "$1" --query '[].name' -o tsv 2> "$work/err" || {
    sed 's/^/  /' "$work/err" >&2
    die "can't list the searches container. It needs Storage Blob Data Reader on the container (USNM_SEARCH_LOG_READERS) and a network path to the data account's private endpoint"
  }
}

# A day's records: days/{day}.jsonl once the day has closed, otherwise its
# staged batches (staging/{day}/*.jsonl); import/{day}.jsonl as well.
fetch() {
  local name=$1 day=$2
  [[ $day =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] && [[ ! $day < $since ]] || return 0
  az storage blob download --auth-mode login --account-name "$account" \
    --container-name searches --name "$name" --file "$work/$(tr / _ <<< "$name")" \
    --no-progress -o none 2> "$work/err" || { sed 's/^/  /' "$work/err" >&2; die "can't download $name"; }
}
closed=$(blobs days/)
for name in $closed; do fetch "$name" "$(basename "$name" .jsonl)"; done
for name in $(blobs staging/); do
  day=$(basename "$(dirname "$name")")
  grep -qxF "days/$day.jsonl" <<< "$closed" || fetch "$name" "$day"
done
for name in $(blobs import/); do fetch "$name" "$(basename "$name" .jsonl)"; done

shopt -s nullglob
files=("$work"/*.jsonl)
[ ${#files[@]} -gt 0 ] || { echo "no searches since $since" >&2; exit 0; }
records="$work/all.json"
cat "${files[@]}" | jq -c 'select(type == "object" and .v == 1)' | jq -s . > "$records"

table() { column -t -s "$(printf '\t')"; }

echo "Searches per day (UTC), since $since"
jq -r '["day", "api", "import", "found nothing"],
  (group_by(.day)[] | [.[0].day,
    (map(select(.source == "api")) | length),
    (map(select(.source == "import")) | length),
    (map(select(.pages == 0)) | length)])
  | map(tostring) | @tsv' "$records" | table
echo
echo "Top 50 queries"
jq -r '["searches", "days", "pages", "query"],
  (group_by(.query) | map({n: length, days: (map(.day) | unique | length),
    pages: (map(.pages | select(. != null)) | max // "-"), query: .[0].query})
   | sort_by(-.n, .query)[:50][] | [.n, .days, .pages, .query])
  | map(tostring | gsub("[\t\r\n]+"; " ")) | @tsv' "$records" | table
echo
echo "Queries that found nothing (top 50)"
jq -r '["searches", "query"],
  (map(select(.pages == 0)) | group_by(.query) | map({n: length, query: .[0].query})
   | sort_by(-.n, .query)[:50][] | [.n, .query])
  | map(tostring | gsub("[\t\r\n]+"; " ")) | @tsv' "$records" | table
echo
# Aggregates only (scripts/lib/date-spans.jq): whether date-limited searches
# are common enough to build the next full base by decade (USNM_PARTITION_DECADE).
# "Whole range" compares each search with the corpus bounds the site serves
# (/v1/meta); without them it falls back to the widest search of each version.
bounds='{}'
site=$(aget SITE_URL)
if [ -n "$site" ] && meta=$(curl -fsS --max-time 20 "${site%/}/v1/meta" 2> /dev/null) &&
  bounds=$(jq -c '{news: .bounds}
    + (if (.congress.series // []) | length > 0
       then {congress: {from: (.congress.series | map(.from) | min),
                        to: (.congress.series | map(.to) | max)}} else {} end)' <<< "$meta"); then
  echo "Whole range: the bounds $site/v1/meta serves now ($(jq -r '[.news.from, .news.to] | join(" to ")' <<< "$bounds") for newspapers)"
else
  bounds='{}'
  echo "warning: couldn't read the corpus bounds from ${site:-the site}/v1/meta; \"whole range\" is each version's widest search, which overcounts it when no one searched the whole range" >&2
fi
echo
echo "Searches by date span"
jq -r --arg by span --argjson bounds "$bounds" -f scripts/lib/date-spans.jq "$records" | table
echo
echo "Searches by decade partitions touched (pre-1820 is one)"
jq -r --arg by decades --argjson bounds "$bounds" -f scripts/lib/date-spans.jq "$records" | table
echo "($(jq length "$records") searches, ${#files[@]} files)" >&2
