#!/usr/bin/env bash
# The archival storage account (infra/archive/, docs/operations.md,
# "Archival storage"): its own deployment stack, usnm-archive, outside every
# environment's, so no provision or teardown of an environment deletes it.
#
#   scripts/archive-store.sh what-if [--subscription ID] [--location REGION]
#   scripts/archive-store.sh deploy [--subscription ID] [--location REGION]
#   scripts/archive-store.sh unlock [--subscription ID]   # lift the delete lock
#
# `what-if` shows what `deploy` would change, and changes nothing. `deploy`
# first creates or updates the guard-rail stack, usnm-guardrails, whose
# policy definitions the account's group assigns (as an environment's
# provision does), then the account (putting the delete lock back), and
# prints the account's resource id, for an environment's USNM_ARCHIVE_ACCOUNT.
# Resources dropped from the template are detached, never deleted, and the
# stack denies deletes outside it except of the lock and of private endpoint
# connections. A delete lock on the account can block removing an
# environment's private endpoint to it, so before an environment drops
# USNM_ARCHIVE_ACCOUNT (or is torn down): `unlock`, provision the
# environment without the setting, then `deploy` again.
#
# Needs az 2.61+, signed in with Owner on the subscription.
set -euo pipefail
usage() { sed -n '2,21s/^# \{0,1\}//p' "$0" >&2; exit 2; }
[ $# -ge 1 ] || usage
ACTION=$1; shift
SUBSCRIPTION="" LOCATION=eastus2
while [ $# -gt 0 ]; do
  case "$1" in
    --subscription) SUBSCRIPTION=$2; shift 2 ;;
    --location) LOCATION=$2; shift 2 ;;
    *) usage ;;
  esac
done
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/guardrails.sh
. scripts/lib/guardrails.sh
STACK=usnm-archive
GROUP=rg-usnm-archive
LOCK=usnm-archive-keep
command -v jq > /dev/null || die "jq is needed"
az account show -o none 2> /dev/null || die "run: az login [--tenant TENANT]"
[ -z "$SUBSCRIPTION" ] || az account set --subscription "$SUBSCRIPTION"
echo "subscription $(az account show --query '[name, id]' -o tsv | paste -sd' ' -)"
# A stack's location is fixed once created. Only "not found" means a first
# deployment; any other error stops here rather than fall back to LOCATION.
keep_stack_location() {
  local location
  if location=$(az stack sub show -n "$STACK" --query location -o tsv 2>&1); then
    LOCATION=$location
  else
    grep -qiE 'NotFound|could not be found' <<< "$location" ||
      die "can't check $STACK: $location"
  fi
}

case "$ACTION" in
  what-if)
    keep_stack_location
    # Stacks have no what-if; a deployment's shows the same template's
    # changes. The guard-rail definitions come first: the account's group
    # assigns them, so its what-if can fail until they exist (deploy creates
    # them first).
    echo "guard-rail definitions ($GUARDRAILS_STACK):"
    az deployment sub what-if --location "$LOCATION" \
      --template-file infra/guardrails.bicep --only-show-errors
    echo "archival account ($STACK):"
    az deployment sub what-if --location "$LOCATION" \
      --template-file infra/archive/main.bicep \
      --parameters location="$LOCATION" --only-show-errors
    ;;
  deploy)
    need_stack_az
    keep_stack_location
    # The account's group assigns the guard-rail definitions.
    deploy_guardrails "$LOCATION" || die "can't deploy $GUARDRAILS_STACK"
    az stack sub create --name "$STACK" --location "$LOCATION" \
      --template-file infra/archive/main.bicep \
      --parameters location="$LOCATION" \
      --action-on-unmanage detachAll \
      --deny-settings-mode denyDelete \
      --deny-settings-excluded-actions "Microsoft.Authorization/locks/delete Microsoft.Storage/storageAccounts/privateEndpointConnections/delete Microsoft.Storage/storageAccounts/privateEndpointConnectionProxies/delete" \
      --description "usnewsmap archival storage, shared by every environment (scripts/archive-store.sh)" \
      --yes --only-show-errors -o none
    # Stacks can return output names with their case changed (scripts/lib/env.sh).
    id=$(az stack sub show -n "$STACK" --query outputs -o json |
      jq -r 'to_entries[] | select(.key | ascii_upcase == "ARCHIVE_ACCOUNT_ID") | .value.value')
    [ -n "$id" ] || die "the stack has no ARCHIVE_ACCOUNT_ID output"
    echo "archival account: $id"
    echo "an environment uses it with: scripts/settings.sh <env> USNM_ARCHIVE_ACCOUNT $id && scripts/provision.sh <env>"
    ;;
  unlock)
    account=$(az storage account list -g "$GROUP" --query "[0].name" -o tsv)
    [ -n "$account" ] || die "no account in $GROUP"
    az lock delete --name "$LOCK" -g "$GROUP" --resource "$account" \
      --resource-type Microsoft.Storage/storageAccounts -o none
    echo "lock $LOCK lifted from $account; run: scripts/archive-store.sh deploy to put it back"
    ;;
  *) usage ;;
esac
