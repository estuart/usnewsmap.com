#!/usr/bin/env bash
# Start one execution of an environment's Container Apps job with other
# arguments, keeping the job's image, settings and replicas, and print the
# execution's name.
#
#   scripts/start-job.sh --env dev --job INGEST_JOB -- enqueue --batches a_ver01,b_ver01
#   scripts/start-job.sh --env dev --job BACKFILL_JOB -- curate --max-runtime-secs 14400
#   scripts/start-job.sh --env prod --job INGEST_JOB -- ocr-damage --version pages-v20261003-1
#   scripts/start-job.sh --env dev --job INGEST_JOB --wait-mins 20 -- titles-archive
#
# --job names the setting that names the job (INGEST_JOB, BACKFILL_JOB,
# ...: the stack's outputs). Everything after `--` replaces the first
# container's arguments; its command stays. `az containerapp job start
# --args` sends a container without the image or settings, so this copies
# the job's template and swaps only the arguments (as
# scripts/ja-ocr/start-quality.sh does). An execution's template has no
# volumes (the API's JobExecutionTemplate takes containers and init
# containers with image, command, args, env and resources only), so it
# can't name the job's volumes or mounts: use it for commands that don't
# need the ingest job's scratch share, such as enqueue, curate,
# archive-copy or ocr-damage. Needs az signed in to the environment's
# subscription and jq.
#
# It returns once Azure accepts the execution, not when the command is done.
# --wait-mins N waits up to N minutes for the execution to succeed, printing
# its name only then, and exits 1 if it ends Failed, Stopped or Degraded or
# is still running at the deadline. Use it when the next step needs what
# the command writes, as a teardown needs titles-archive's copy.
set -euo pipefail
usage() { awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "$0" >&2; exit 2; }
ENV_NAME=""
SETTING=""
WAIT_MINS=""
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --job) [ $# -ge 2 ] || usage; SETTING=$2; shift 2 ;;
    --wait-mins)
      # Checked here, not later: an empty value (an unset variable) must not
      # read as "no wait" and let a teardown go ahead.
      [ $# -ge 2 ] || usage
      [[ $2 =~ ^[0-9]+$ ]] || { echo "error: --wait-mins is a number of minutes" >&2; exit 2; }
      # In decimal: Bash arithmetic reads a leading zero (`08`) as octal.
      WAIT_MINS=$((10#$2)); shift 2 ;;
    --) shift; break ;;
    -h|--help) usage ;;
    *) echo "error: unexpected \"$1\" (the job's arguments go after --)" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] && [ -n "$SETTING" ] && [ $# -ge 1 ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
. scripts/lib/env.sh
command -v jq > /dev/null || die "jq is needed to build the request"
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME"
[[ $SETTING =~ _JOB$ ]] || die "\"$SETTING\" isn't a job setting (INGEST_JOB, BACKFILL_JOB, ...)"
sub=$(aget AZURE_SUBSCRIPTION_ID)
rg=$(aget AZURE_RESOURCE_GROUP)
job=$(aget "$SETTING")
[ -n "$job" ] || die "$SETTING isn't set for $ENV_NAME (is the job deployed? run scripts/provision.sh --env $ENV_NAME)"
template=$(az containerapp job show -n "$job" -g "$rg" --subscription "$sub" --query properties.template -o json) ||
  die "can't read $job (is az signed in to the environment's tenant?)"
volumes=$(jq -r '[.volumes[]?.name] | join(", ")' <<< "$template")
[ -z "$volumes" ] ||
  echo "note: $job mounts $volumes, which an execution can't name; don't rely on them in this one" >&2
# Every container as the API takes it for an execution, the first with the
# new arguments. `--args --`: they are positional strings, not jq options.
body=$(jq -c 'def exec: {name, image, command, args, env, resources} | with_entries(select(.value != null));
  {containers: ([.containers[0] | .args = $ARGS.positional] + .containers[1:] | map(exec)),
   initContainers: ((.initContainers // []) | map(exec))}' --args -- "$@" <<< "$template")
execution=$(az rest --method post \
  --url "https://management.azure.com/subscriptions/$sub/resourceGroups/$rg/providers/Microsoft.App/jobs/$job/start?api-version=2025-01-01" \
  --body "$body" --query name -o tsv)
if [ -z "$WAIT_MINS" ]; then
  echo "$execution"
  exit 0
fi
[ -n "$execution" ] || die "$job started no execution"
echo "waiting up to $WAIT_MINS minutes for $job execution $execution" >&2
deadline=$(($(date +%s) + WAIT_MINS * 60))
while :; do
  status=$(az containerapp job execution show -n "$job" -g "$rg" --subscription "$sub" \
    --job-execution-name "$execution" --query properties.status -o tsv) ||
    die "can't read $execution's status"
  case $status in
    Succeeded) break ;;
    Failed | Stopped | Degraded)
      die "$execution ended $status. Its log: scripts/logs.sh --env $ENV_NAME --query-name job-executions" ;;
  esac
  [ "$(date +%s)" -lt "$deadline" ] ||
    die "$execution is still ${status:-starting} after $WAIT_MINS minutes; wait for it before relying on what it writes (scripts/logs.sh --env $ENV_NAME --query-name job-executions)"
  sleep 15
done
echo "$execution succeeded" >&2
echo "$execution"
