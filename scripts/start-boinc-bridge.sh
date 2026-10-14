#!/usr/bin/env bash
# Start one run of the BOINC bridge job (contrib-compute/images/server/bridge-job.sh) with its
# step and that step's settings, and print the execution's name:
#
#   scripts/start-boinc-bridge.sh --env prod --step register-apps
#   scripts/start-boinc-bridge.sh --env prod --step gold-prepare --var GOLD_CAMPAIGN=v1-tess550
#   scripts/start-boinc-bridge.sh --env prod --step submit --var JOB_ID=<id> --wait-mins 30
#
# Why not `az containerapp job start --env-vars`: with any container flag the CLI sends an
# execution template holding one container named after the job (this one is "bridge"), with
# no image, and with only the variables given, so the job's own settings (KEY_VAULT_NAME,
# DB_HOST, PROJECT_DIR, ...) are gone. The API's execution template also has no volumes or
# mounts (scripts/start-job.sh), and every step needs the project share at /project. So this
# script writes the run's settings into the job itself, with
# `az containerapp job update --replace-env-vars`: the job's settings that the template
# defines (BASE_VARS below, kept as they are) plus BRIDGE_STEP and each --var. Settings a
# previous run left are dropped. Then it starts the job with no override, so the image, the
# command, the resources and the share mount are the deployed ones. The next
# scripts/provision-boinc.sh puts the template's settings back.
#
# --var NAME=VALUE is repeatable; NAME is upper case and can't be one of BASE_VARS or
# BRIDGE_STEP. The step and the values are checked by bridge-job.sh, not here. --wait-mins N
# waits up to N minutes for the execution to succeed and exits 1 if it ends Failed, Stopped
# or Degraded or is still running then. --what-if prints the settings and changes nothing.
# One step at a time: a manual job can run several executions at once, and they would share
# the project tree and the database. So it refuses while any execution of the job is still
# running, and after the start it reads the new execution's settings back and fails if they
# are not this run's (another caller updated the job in between). That narrows the window
# but is no lock: one operator runs these steps.
# Needs az signed in to the settings' subscription, and jq.
set -euo pipefail
usage() { awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "$0" >&2; exit 2; }
ENV_NAME="" STEP="" WAIT_MINS="" WHAT_IF=false
VARS=()
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --step) [ $# -ge 2 ] || usage; STEP=$2; shift 2 ;;
    --var) [ $# -ge 2 ] || usage; VARS+=("$2"); shift 2 ;;
    --wait-mins)
      [ $# -ge 2 ] || usage
      [[ $2 =~ ^[0-9]+$ ]] || { echo "error: --wait-mins is a number of minutes" >&2; exit 2; }
      WAIT_MINS=$((10#$2)); shift 2 ;;
    --what-if) WHAT_IF=true; shift ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] && [ -n "$STEP" ] || usage
die() { echo "error: $*" >&2; exit 1; }
# The settings contrib-compute/infra/boinc/modules/bridge-job.bicep gives the container
# (scripts/test_boinc_scripts.py checks the two lists agree).
BASE_VARS=(AZURE_CLIENT_ID KEY_VAULT_NAME DB_HOST DB_NAME MASTER_HOST PROJECT_DIR HANDOFF_DIR)
[[ $STEP =~ ^[a-z][a-z-]{0,40}$ ]] || die "--step is a bridge step name, such as register-apps or gold-prepare"
seen=" "
for v in ${VARS[@]+"${VARS[@]}"}; do
  name=${v%%=*}
  [[ $v == *=* ]] || die "--var wants NAME=VALUE, got \"$v\""
  [[ $name =~ ^[A-Z][A-Z0-9_]{0,63}$ ]] || die "--var $name: names are upper case letters, digits and _"
  [ "$name" != BRIDGE_STEP ] || die "--var BRIDGE_STEP: use --step"
  for b in "${BASE_VARS[@]}"; do [ "$name" != "$b" ] || die "--var $name: the job's own setting can't be changed per run"; done
  [[ $seen != *" $name "* ]] || die "--var $name is given twice"
  seen="$seen$name "
done
cd "$(dirname "$0")/.."
ALLOW_DIRTY=false
# shellcheck source=scripts/lib/boinc.sh
. scripts/lib/boinc.sh
boinc_load_settings
boinc_az_check
job=$(boinc_output bridgeJobName)
[ -n "$job" ] || die "the stack $BOINC_STACK has no bridge job (are the images in $BOINC_SETTINGS deployed? scripts/provision-boinc.sh --env $ENV_NAME)"
template=$(baz containerapp job show --name "$job" --resource-group "$BOINC_RG" --query properties.template -o json) ||
  die "can't read $job"
container=$(jq -er '.containers[0].name' <<< "$template") || die "$job has no container"
# The job's own settings, as deployed; each must be there.
base=()
for b in "${BASE_VARS[@]}"; do
  val=$(jq -er --arg n "$b" '.containers[0].env // [] | map(select(.name == $n and (.value // "") != "")) | if length == 1 then .[0].value else error end' <<< "$template") ||
    die "$job has no $b setting; run scripts/provision-boinc.sh --env $ENV_NAME first"
  base+=("$b=$val")
done
run=("BRIDGE_STEP=$STEP" ${VARS[@]+"${VARS[@]}"})
if $WHAT_IF; then
  echo "would set the settings of $job (container $container) to:"
  printf '  %s\n' "${base[@]}" "${run[@]}"
  echo "then start one execution of it with no override"
  exit 0
fi
# running_executions: the job's executions that haven't ended.
running_executions() {
  baz containerapp job execution list --name "$job" --resource-group "$BOINC_RG" \
    --query "[?properties.status=='Running' || properties.status=='Processing'].name" -o tsv
}
running=$(running_executions) || die "can't list the executions of $job"
[ -z "$running" ] || die "$job is still running $(echo "$running" | tr '\n' ' ')- wait for it to end first"
baz containerapp job update --name "$job" --resource-group "$BOINC_RG" --container-name "$container" \
  --replace-env-vars "${base[@]}" "${run[@]}" -o none || die "can't set the run's settings on $job"
execution=$(baz containerapp job start --name "$job" --resource-group "$BOINC_RG" --query name -o tsv) ||
  die "can't start $job"
[ -n "$execution" ] || die "$job started no execution"
# The execution records the template it started with: it must hold exactly this run's settings.
want=$(printf '%s\n' "${base[@]}" "${run[@]}" | sort)
got=$(baz containerapp job execution show --name "$job" --resource-group "$BOINC_RG" --job-execution-name "$execution" \
  --query properties.template.containers[0].env -o json | jq -r '.[] | "\(.name)=\(.value // "")"' | sort) ||
  die "can't read $execution's settings; stop it if it isn't this run's: az containerapp job stop --name $job --resource-group $BOINC_RG --job-execution-name $execution"
[ "$got" = "$want" ] ||
  die "$execution did not start with this run's settings (another caller changed $job); stop it: az containerapp job stop --name $job --resource-group $BOINC_RG --job-execution-name $execution"
if [ -z "$WAIT_MINS" ]; then
  echo "$execution"
  exit 0
fi
echo "waiting up to $WAIT_MINS minutes for $job execution $execution" >&2
deadline=$(($(date +%s) + WAIT_MINS * 60))
while :; do
  status=$(baz containerapp job execution show --name "$job" --resource-group "$BOINC_RG" \
    --job-execution-name "$execution" --query properties.status -o tsv) || die "can't read $execution's status"
  case $status in
    Succeeded) break ;;
    Failed | Stopped | Degraded) die "$execution ended $status; its log is in the stack's Log Analytics workspace (ContainerAppConsoleLogs_CL)" ;;
  esac
  [ "$(date +%s)" -lt "$deadline" ] || die "$execution is still ${status:-starting} after $WAIT_MINS minutes"
  sleep 15
done
echo "$execution succeeded" >&2
echo "$execution"
