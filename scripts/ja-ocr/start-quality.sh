#!/usr/bin/env bash
# Start the OCR quality audit (ja-ocr/quality.py) as an execution of the
# environment's ja-ocr job (or its one-replica audit job, below), with the job's own image, settings and replicas
# but the audit's command line, and print the execution's name.
#
#   scripts/ja-ocr/start-quality.sh --env prod --sample-pct 10
#   scripts/ja-ocr/start-quality.sh --env prod --sample-pct 2 --min-pages 50
#   scripts/ja-ocr/start-quality.sh --env prod --command mixed     # jaocr.py mixed instead (mixed.py)
#   scripts/ja-ocr/start-quality.sh --env prod --command american-stories --year 1865 --year 1925 [--diff]
#   scripts/ja-ocr/start-quality.sh --env prod --command american-stories-write [--year 1865 ...]
#   scripts/ja-ocr/start-quality.sh --env prod --command targets [--mixed-below 0.65]   (the OCR's page list)
#   scripts/ja-ocr/start-quality.sh --env prod --command run [--mixed-below 0.65]       (the OCR itself)
#   scripts/ja-ocr/start-quality.sh --env prod --command damage-parity [--pages 400] [--audit-version V]
#
# --command defaults to quality; the options after it are the command's. The
# quality audit runs on the ja-ocr job, whose replicas share its work
# (quality.py); mixed, targets, damage-parity and the american-stories
# commands, which one replica does, run on the one-replica audit job
# (JA_AUDIT_JOB), so a second replica can't fail the execution.
# `az containerapp job start --args` sends a container without the image or
# settings, so this copies the job's template and swaps only the arguments.
# Needs az signed in to the environment's subscription and jq. Results:
# scripts/ja-ocr/quality-rows.sh.
set -euo pipefail
usage() { sed -n '2,23s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
ENV_NAME=""
command=quality
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --command) [ $# -ge 2 ] || usage; command=$2; shift 2 ;;
    -h|--help) usage ;;
    *) break ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
cd "$(dirname "$0")/../.."
. scripts/lib/env.sh
command -v jq > /dev/null || die "jq is needed to build the request"
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME"
sub=$(aget AZURE_SUBSCRIPTION_ID)
rg=$(aget AZURE_RESOURCE_GROUP)
# The jobs' names from the stack's outputs (JA_OCR_JOB, JA_AUDIT_JOB), not
# rebuilt here (ADR-0017).
job=$(aget JA_OCR_JOB)
one=$(aget JA_AUDIT_JOB)
[ -n "$job" ] && [ -n "$one" ] ||
  die "JA_OCR_JOB or JA_AUDIT_JOB isn't set for $ENV_NAME (USNM_JA_OCR_JOB on and provisioned?)"
case $command in
  quality)
    allowed='^(--sample-pct|--min-pages|--metric|[0-9]+(\.[0-9]+)?|v[12])$' ;;
  mixed)
    job=$one
    [ $# -eq 0 ] || die "mixed takes no options" ;;
  targets|run)
    # targets lists the pages once, on one replica, before the OCR job's
    # replicas start (they would otherwise both list them, streaming every
    # Japanese title's archive). run is the OCR job's own command, for a
    # --mixed-below other than the default.
    [ "$command" = targets ] && job=$one
    allowed='^(--mixed-below|0(\.[0-9]{1,2})?|1(\.0+)?)$' ;;
  american-stories)
    job=$one
    allowed='^(--year|--sample-pct|--diff|[0-9]+(\.[0-9]+)?)$' ;;
  american-stories-write)
    # The writer reads every page of each year it's given: only --year narrows it.
    job=$one
    allowed='^(--year|[0-9]{4})$' ;;
  damage-parity)
    job=$one
    allowed='^(--pages|[0-9]+|--audit-version|pages-v[0-9]{8}-[0-9]+)$' ;;
  *) die "unknown command \"$command\"" ;;
esac
for a in "$@"; do
  [[ $a =~ ${allowed:-^$} ]] || die "unexpected argument \"$a\" for $command"
done

template=$(az containerapp job show -n "$job" -g "$rg" --subscription "$sub" --query properties.template -o json) ||
  die "can't read $job (is az signed in to the environment's tenant?)"
# `--args --`: the audit's options are positional strings, not jq options.
body=$(jq -c --arg command "$command" '{containers: [.containers[0] | .args = ([$command] + $ARGS.positional)],
  initContainers: (.initContainers // [])}' --args -- "$@" <<< "$template")
az rest --method post \
  --url "https://management.azure.com/subscriptions/$sub/resourceGroups/$rg/providers/Microsoft.App/jobs/$job/start?api-version=2025-01-01" \
  --body "$body" --query name -o tsv
