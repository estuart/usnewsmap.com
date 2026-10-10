#!/usr/bin/env bash
# Create the OCR nodes' Entra security group for an environment (#296,
# ADR-0015) and save its object id in USNM_OCR_NODE_GROUP, so the next
# provision grants the group the node roles on the exchange account.
#
#   scripts/ocr-node/group.sh --env prod [--dry-run]
#
# The group is usnm-ocr-nodes-<env>: a plain security group (not
# mail-enabled, not role-assignable) whose members are the nodes' Arc
# machine identities. onboard.sh adds each node, offboard.sh removes it.
# Deployment stacks can't hold Microsoft Graph resources, so the Bicep
# doesn't create it. Run once per environment; run again, it finds the group
# by name, makes you an owner if you aren't one (owners can add and remove
# members) and saves its id. It never deletes the group.
#
# --dry-run checks your sign-in and prints the plan; it creates nothing and
# changes no setting.
#
# Needs az signed in to the environment's tenant with the right to create
# security groups (the default user setting, or Groups Administrator).
# Works with macOS's bash 3.2.
set -euo pipefail
usage() { sed -n '6s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }

ENV_NAME="" DRY_RUN=false
while [ $# -gt 0 ]; do
  case "$1" in
    --env) ENV_NAME=${2:-}; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || { echo "error: missing --env" >&2; usage; }

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# shellcheck source=scripts/lib/env.sh
. scripts/lib/env.sh
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME; run scripts/bootstrap.sh $ENV_NAME first"

TENANT=$(aget AZURE_TENANT_ID)
SAVED=$(aget USNM_OCR_NODE_GROUP)
[ -n "$TENANT" ] || die "$ENV_FILE has no AZURE_TENANT_ID; run scripts/provision.sh $ENV_NAME"
az account show -o none 2> /dev/null || die "run: az login --tenant $TENANT"
# az ad works in the current account's tenant.
current_tenant=$(az account show --query tenantId -o tsv 2> /dev/null || true)
[ "$current_tenant" = "$TENANT" ] ||
  die "az's current account is in tenant ${current_tenant:-unknown}, not $TENANT: az login --tenant $TENANT"

GROUP_NAME="usnm-ocr-nodes-$ENV_NAME"
[ "$DRY_RUN" = false ] || echo "dry run: nothing is changed"

# Display names aren't unique in Entra; more than one match is left to a
# person to sort out.
ids=$(az ad group list --filter "displayName eq '$GROUP_NAME'" --query '[].id' -o tsv)
count=$(printf '%s' "$ids" | grep -c . || true)
[ "$count" -le 1 ] || die "$count groups are named $GROUP_NAME ($(echo "$ids" | tr '\n' ' ')); delete the extras"
GROUP_ID=$ids
if [ -n "$GROUP_ID" ]; then
  echo "$GROUP_NAME exists: $GROUP_ID"
elif [ "$DRY_RUN" = true ]; then
  echo "would create security group $GROUP_NAME"
else
  GROUP_ID=$(az ad group create --display-name "$GROUP_NAME" --mail-nickname "$GROUP_NAME" \
    --description "usnewsmap $ENV_NAME on-prem OCR nodes' Arc identities (ADR-0015); roles on the exchange account come from the provision" \
    --query id -o tsv) || die "can't create $GROUP_NAME (you need the right to create security groups)"
  echo "created $GROUP_NAME: $GROUP_ID"
fi

me=$(az ad signed-in-user show --query id -o tsv 2> /dev/null || true)
if [ -z "$me" ]; then
  echo "warning: not signed in as a user, so not checking group ownership" >&2
elif [ -z "$GROUP_ID" ]; then
  echo "would make you ($me) an owner of $GROUP_NAME"
elif [ -n "$(az ad group owner list --group "$GROUP_ID" --query "[?id=='$me'].id" -o tsv)" ]; then
  echo "you ($me) own $GROUP_NAME"
elif [ "$DRY_RUN" = true ]; then
  echo "would make you ($me) an owner of $GROUP_NAME"
else
  az ad group owner add --group "$GROUP_ID" --owner-object-id "$me" -o none ||
    die "can't add you as an owner of $GROUP_NAME"
  echo "made you ($me) an owner of $GROUP_NAME"
fi

if [ -z "$GROUP_ID" ]; then
  echo "would save its id in USNM_OCR_NODE_GROUP (now: ${SAVED:-empty})"
elif [ "$SAVED" = "$GROUP_ID" ]; then
  echo "USNM_OCR_NODE_GROUP is already $GROUP_ID"
elif [ "$DRY_RUN" = true ]; then
  echo "would set USNM_OCR_NODE_GROUP to $GROUP_ID (now: ${SAVED:-empty})"
else
  scripts/settings.sh "$ENV_NAME" USNM_OCR_NODE_GROUP "$GROUP_ID"
  echo "set USNM_OCR_NODE_GROUP to $GROUP_ID${SAVED:+ (was $SAVED)}"
fi
echo "Grant the group the node roles by provisioning (needs Owner on the subscription):"
echo "  scripts/provision.sh $ENV_NAME"
