#!/usr/bin/env bash
# Remove an on-prem OCR node (#296, ADR-0015) from the operator's machine:
# take its identity out of the nodes' Entra group, stop and disconnect the
# agent on the node, and delete its Arc machine. No provision needed.
#
#   scripts/ocr-node/offboard.sh --env prod --name usnm-ocr-node1 \
#     [--host claude@node1.example.org] [--ssh-key ~/.ssh/id_ed25519] [--dry-run]
#
# Steps:
#   1. removes the Arc machine's system-assigned identity from
#      USNM_OCR_NODE_GROUP, which takes away the node roles;
#   2. with --host, over ssh with sudo: stops and disables
#      usnm-ocr-worker.service and, if the agent is configured (connected
#      or not), runs `azcmagent disconnect --force-local-only`, which
#      clears the agent's local state without signing in to Azure (so no
#      onboarding credential is needed); without
#      --host (a node that's gone), prints those commands instead;
#   3. deletes the Arc machine with your own az sign-in, which deletes its
#      identity.
# Tokens the node already holds stay valid until they expire: up to about
# 24 hours for a managed identity (ADR-0015). Run again, it skips what's done.
#
# --dry-run prints the plan; it changes nothing and doesn't connect to the
# node.
#
# Needs az signed in to the environment's tenant with the right to remove
# members from the nodes' group (an owner of it) and to delete resources in
# the environment's group, and ssh for --host. Works with macOS's bash 3.2.
set -euo pipefail
usage() { sed -n '6,7s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

ENV_NAME="" HOST="" NAME="" SSH_KEY="" DRY_RUN=false
while [ $# -gt 0 ]; do
  case "$1" in
    --env) ENV_NAME=${2:-}; shift 2 ;;
    --host) HOST=${2:-}; shift 2 ;;
    --name) NAME=${2:-}; shift 2 ;;
    --ssh-key) SSH_KEY=${2:-}; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || { echo "error: missing --env" >&2; usage; }
[ -n "$NAME" ] || { echo "error: missing --name" >&2; usage; }
[[ $NAME =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,53}$ ]] || die "--name: 1 to 54 letters, digits or hyphens, starting with a letter or digit"
[ -z "$SSH_KEY" ] || [ -r "$SSH_KEY" ] || die "--ssh-key: can't read $SSH_KEY"

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# shellcheck source=scripts/lib/env.sh
. scripts/lib/env.sh
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME"

SUBSCRIPTION=$(aget AZURE_SUBSCRIPTION_ID)
TENANT=$(aget AZURE_TENANT_ID)
GROUP=$(aget AZURE_RESOURCE_GROUP)
NODE_GROUP=$(aget USNM_OCR_NODE_GROUP)
[ -n "$SUBSCRIPTION" ] && [ -n "$TENANT" ] && [ -n "$GROUP" ] ||
  die "$ENV_FILE has no AZURE_SUBSCRIPTION_ID, AZURE_TENANT_ID or AZURE_RESOURCE_GROUP"
az account show -o none 2> /dev/null || die "run: az login --tenant $TENANT"
current_tenant=$(az account show --query tenantId -o tsv 2> /dev/null || true)
[ "$current_tenant" = "$TENANT" ] ||
  die "az's current account is in tenant ${current_tenant:-unknown}, not $TENANT: az login --tenant $TENANT, then az account set --subscription $SUBSCRIPTION"
echo "environment $ENV_NAME: subscription $SUBSCRIPTION, $GROUP; Arc resource $NAME${HOST:+ on $HOST}"
[ "$DRY_RUN" = false ] || echo "dry run: nothing is changed"

MACHINE_ID="" principal=""
# stderr apart, so a warning can't end up in the result.
errf=$(mktemp)
if machine=$(az resource show -g "$GROUP" -n "$NAME" --resource-type Microsoft.HybridCompute/machines \
  --subscription "$SUBSCRIPTION" --query '[id, identity.principalId]' -o tsv 2> "$errf"); then
  rm -f "$errf"
  MACHINE_ID=$(echo "$machine" | sed -n 1p)
  principal=$(echo "$machine" | sed -n 2p)
  echo "$NAME: $MACHINE_ID, identity ${principal:-none}"
else
  # Only a machine that isn't there counts as gone; any other failure stops.
  err=$(cat "$errf")
  rm -f "$errf"
  grep -qE '\((ResourceNotFound|ResourceGroupNotFound)\)' <<< "$err" || die "can't look up $NAME in $GROUP: $err"
  echo "$NAME isn't in $GROUP"
fi

step "Group membership"
if [ -z "$NODE_GROUP" ]; then
  echo "no USNM_OCR_NODE_GROUP for $ENV_NAME; nothing to remove"
elif [ -z "$principal" ]; then
  echo "no identity to remove (a deleted identity leaves its groups by itself)"
elif ! member=$(az ad group member check --group "$NODE_GROUP" --member-id "$principal" --query value -o tsv) ||
  { [ "$member" != true ] && [ "$member" != false ]; }; then
  die "can't check whether $principal is in $NODE_GROUP"
elif [ "$member" = false ]; then
  echo "$principal isn't in $NODE_GROUP"
elif [ "$DRY_RUN" = true ]; then
  echo "would remove $principal from $NODE_GROUP"
else
  az ad group member remove --group "$NODE_GROUP" --member-id "$principal" ||
    die "can't remove $principal from $NODE_GROUP (you need to own the group)"
  echo "removed $principal from $NODE_GROUP"
fi

step "Node"
# --force-local-only: the agent forgets its connection without asking Azure,
# so the node needs no credential; this script deletes the resource itself.
# The agent is cleared whenever it's configured (it has a resource name),
# whether it shows Connected or not: one that can't reach Azure shows
# Disconnected but still holds the machine's settings.
REMOTE='systemctl disable --now usnm-ocr-worker.service 2> /dev/null || true
if ! command -v azcmagent > /dev/null; then
  echo "no Connected Machine agent"
elif azcmagent show 2> /dev/null | grep -Eqi "^ *resource name *: *[^ ]"; then
  azcmagent disconnect --force-local-only
else
  echo "agent not configured"
fi'
if [ -z "$HOST" ]; then
  echo "no --host; if the node still exists, run on it as root:"
  echo "  systemctl disable --now usnm-ocr-worker.service"
  echo "  azcmagent disconnect --force-local-only"
elif [ "$DRY_RUN" = true ]; then
  echo "would run on $HOST with sudo:"
  printf '%s\n' "$REMOTE" | while IFS= read -r line; do echo "  $line"; done
else
  SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=20)
  [ -z "$SSH_KEY" ] || SSH_OPTS+=(-i "$SSH_KEY" -o IdentitiesOnly=yes)
  printf '%s\n' "$REMOTE" | ssh "${SSH_OPTS[@]}" "$HOST" 'sudo bash -s' ||
    die "couldn't disconnect the agent on $HOST; fix that, or run again without --host to skip it"
fi

step "Arc machine"
if [ -z "$MACHINE_ID" ]; then
  echo "nothing to delete"
elif [ "$DRY_RUN" = true ]; then
  echo "would delete $MACHINE_ID (and with it its identity)"
else
  az resource delete --ids "$MACHINE_ID" -o none || die "can't delete $MACHINE_ID"
  echo "deleted $MACHINE_ID"
fi
