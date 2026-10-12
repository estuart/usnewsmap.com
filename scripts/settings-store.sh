#!/usr/bin/env bash
# The settings store (infra/settings/main.bicep, docs/operations.md,
# "Settings"): one App Configuration store for every environment in the
# subscription, each environment's private settings under its own label.
# Its own deployment stack, usnm-config, outside every environment's, so no
# provision or teardown of an environment deletes it.
#
#   scripts/settings-store.sh --what-if [--subscription ID] [--location REGION] [--admins IDS]
#   scripts/settings-store.sh --deploy [--subscription ID] [--location REGION] [--admins IDS]
#
# --what-if shows what --deploy would change, and changes nothing. --deploy
# creates or updates the store and prints its endpoint; scripts/lib/env.sh's
# provision runs it when the subscription has no store yet. --admins takes
# the Entra object ids, comma separated, given App Configuration Data Owner
# on the store besides the account deploying it: every operator who
# provisions an environment (--admins '' empties the list). Without it the
# stack keeps the list it has. The role covers every label: one maintainer
# operates every environment.
#
# Resources dropped from the template are deleted (an admin taken off the
# list loses the role), and the stack denies deletes outside it.
#
# Needs az 2.61+, signed in with Owner on the subscription.
set -euo pipefail
usage() { sed -n '2,23s/^# \{0,1\}//p' "$0" >&2; exit 2; }
ACTION="" SUBSCRIPTION="" LOCATION=eastus2 ADMINS="" ADMINS_SET=false
while [ $# -gt 0 ]; do
  case "$1" in
    --what-if) [ -z "$ACTION" ] || usage; ACTION=what-if; shift ;;
    --deploy) [ -z "$ACTION" ] || usage; ACTION=deploy; shift ;;
    --subscription) [ $# -ge 2 ] || usage; SUBSCRIPTION=$2; shift 2 ;;
    --location) [ $# -ge 2 ] || usage; LOCATION=$2; shift 2 ;;
    --admins) [ $# -ge 2 ] || usage; ADMINS=$2; ADMINS_SET=true; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ACTION" ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
# need_stack_az and stack_param.
# shellcheck source=scripts/lib/guardrails.sh
. scripts/lib/guardrails.sh
STACK=usnm-config
az account show -o none 2> /dev/null || die "run: az login [--tenant TENANT]"
[ -z "$SUBSCRIPTION" ] || az account set --subscription "$SUBSCRIPTION"
echo "subscription $(az account show --query '[name, id]' -o tsv | paste -sd' ' -)"
# A stack's location is fixed once created, and the admins it has are kept
# unless --admins is given. Only "not found" means a first deployment; any
# other error stops here.
if location=$(az stack sub show -n "$STACK" --query location -o tsv 2>&1); then
  LOCATION=$location
  if ! $ADMINS_SET; then
    params=$(az stack sub show -n "$STACK" \
      --query "[keys(parameters || \`{}\`), values(parameters || \`{}\`)[].value]" -o tsv) ||
      die "can't read the parameters of $STACK"
    ADMINS=$(stack_param "$params" admins)
  fi
else
  grep -qiE 'NotFound|could not be found' <<< "$location" || die "can't check $STACK: $location"
fi

case "$ACTION" in
  what-if)
    # Stacks have no what-if; a deployment's shows the same template's
    # changes, but not what the stack deletes because the template dropped
    # it (an admin taken off the list loses the role).
    echo "the store's changes (a deployment's what-if: it doesn't list role assignments the stack deletes for admins taken off the list):"
    az deployment sub what-if --location "$LOCATION" \
      --template-file infra/settings/main.bicep \
      --parameters location="$LOCATION" admins="$ADMINS" --only-show-errors
    ;;
  deploy)
    need_stack_az
    # A subscription that has never had a store doesn't have the provider
    # registered, and the deployment would fail with MissingSubscriptionRegistration.
    [ "$(az provider show -n Microsoft.AppConfiguration --query registrationState -o tsv 2> /dev/null)" = Registered ] ||
      az provider register -n Microsoft.AppConfiguration --wait -o none
    az stack sub create --name "$STACK" --location "$LOCATION" \
      --template-file infra/settings/main.bicep \
      --parameters location="$LOCATION" admins="$ADMINS" \
      --action-on-unmanage deleteResources \
      --deny-settings-mode denyDelete \
      --description "usnewsmap settings store, every environment's private settings by label (scripts/settings-store.sh)" \
      --yes --only-show-errors -o none
    # Stacks can return output names with their case changed (scripts/lib/env.sh).
    endpoint=$(az stack sub show -n "$STACK" --query outputs -o json |
      python3 -c 'import json, sys
for k, v in (json.load(sys.stdin) or {}).items():
    if k.upper() == "SETTINGS_STORE_ENDPOINT":
        print(v.get("value") or "")')
    [ -n "$endpoint" ] || die "the stack has no SETTINGS_STORE_ENDPOINT output"
    echo "settings store: $endpoint"
    ;;
  *) usage ;;
esac
