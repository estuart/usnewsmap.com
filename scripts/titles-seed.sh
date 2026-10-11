#!/usr/bin/env bash
# Start an environment's title records and catalog from the newest copy in
# the archival account (`raw/titles/{time}.json`, which titles-sync saves
# there after each run), so a release in a new environment doesn't wait for
# a titles-sync paced against LoC (up to 8 hours). #275; docs/operations.md,
# "Dev environment lifecycle". scripts/stand-up.sh runs it.
#
#   scripts/titles-seed.sh --env dev [--wait-mins 20]
#
#   --env NAME         the environment; never prod
#   --wait-mins N      how long to wait for the job execution (default 20)
#
# It starts one execution of the environment's ingest job running
# `usnm-ingest titles-seed` (scripts/start-job.sh) and waits for it. The job
# reaches the archival account through the environment's private endpoint,
# as curation does, with the ingest identity's existing role there (list,
# read and create in `raw`); nothing here reads the account directly. The
# seed writes only into an environment with no record cache
# (`reference/raw/titles.json`), each file create-only: a cache the
# environment already has, newer or older, is never replaced. The job logs
# the copy it read and the files it wrote or kept:
# scripts/logs.sh --env NAME --query-name titles-archive.
#
# Needs USNM_ARCHIVE_ACCOUNT and the ingest job (USNM_INGEST_JOBS with
# USNM_USE_ACR) in the environment's settings, az signed in to its
# subscription, and jq.
set -euo pipefail
usage() { awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "$0" >&2; exit 2; }
ENV_NAME="" WAIT_MINS=20
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --wait-mins) [ $# -ge 2 ] || usage; WAIT_MINS=$2; shift 2 ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
[[ $WAIT_MINS =~ ^[0-9]+$ ]] || { echo "error: --wait-mins is a number of minutes" >&2; exit 2; }
# In decimal: Bash arithmetic reads a leading zero (`08`) as octal.
WAIT_MINS=$((10#$WAIT_MINS))
# Prod's catalog numbers are in its published versions: it keeps its own.
[ "$ENV_NAME" != prod ] || { echo "error: prod keeps its own title records" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
. scripts/lib/env.sh
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME"
[ -n "$(aget USNM_ARCHIVE_ACCOUNT)" ] ||
  die "$ENV_NAME doesn't use the archival account (USNM_ARCHIVE_ACCOUNT): nothing to seed from"
job=$(aget INGEST_JOB)
[ -n "$job" ] ||
  die "$ENV_NAME has no ingest job (INGEST_JOB; USNM_INGEST_JOBS and USNM_USE_ACR on, then scripts/provision.sh --env $ENV_NAME)"
sub=$(aget AZURE_SUBSCRIPTION_ID)
rg=$(aget AZURE_RESOURCE_GROUP)

execution=$(scripts/start-job.sh --env "$ENV_NAME" --job INGEST_JOB -- titles-seed) ||
  die "couldn't start $job"
[ -n "$execution" ] || die "$job started no execution"
echo "seeding $ENV_NAME's title records: $job execution $execution"

deadline=$(($(date +%s) + WAIT_MINS * 60))
while :; do
  status=$(az containerapp job execution show -n "$job" -g "$rg" --subscription "$sub" \
    --job-execution-name "$execution" --query properties.status -o tsv) ||
    die "can't read $execution's status"
  case $status in
    Succeeded) break ;;
    Failed | Stopped | Degraded)
      die "$execution ended $status; nothing was replaced. Its log: scripts/logs.sh --env $ENV_NAME --query-name job-executions" ;;
  esac
  [ "$(date +%s)" -lt "$deadline" ] ||
    die "$execution is still ${status:-starting} after $WAIT_MINS minutes; check it with scripts/logs.sh --env $ENV_NAME --query-name titles-archive"
  sleep 15
done
echo "$execution succeeded. What it copied, or kept (a few minutes after, once the logs arrive):"
echo "  scripts/logs.sh --env $ENV_NAME --query-name titles-archive --span 1h"
