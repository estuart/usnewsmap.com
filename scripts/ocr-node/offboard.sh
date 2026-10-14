#!/usr/bin/env bash
# Remove an on-prem OCR node (#296, ADR-0015) from the operator's machine:
# take its identity out of the nodes' Entra group, remove the Azure Monitor
# Agent and its data collection rule association, stop and disconnect the
# agent on the node, and delete its Arc machine. No provision needed.
#
#   scripts/ocr-node/offboard.sh --env prod --name usnm-ocr-node1 \
#     [--host claude@node1.example.org] [--ssh-key ~/.ssh/id_ed25519] [--dry-run]
#
# Steps:
#   1. removes the Arc machine's system-assigned identity from
#      USNM_OCR_NODE_GROUP, which takes away the node roles;
#   2. deletes the machine's data collection rule associations, so it
#      sends no more logs or counters, and, with --host, deletes the Azure
#      Monitor Linux Agent extension and waits while the node uninstalls
#      it (without --host, deleting the machine in step 4 removes the
#      extension's resource; the node is gone or unreachable);
#   3. with --host, over ssh with sudo: stops and disables
#      the weekly reboot window first (usnm-ocr-reboot.timer and a
#      usnm-ocr-reboot.service already running, so a window can't reboot
#      the node mid-offboard; their units and script are removed), the
#      nightly worker update (usnm-ocr-update.timer, its service and
#      scripts, #471) and dnf-automatic.timer, then usnm-ocr-worker.service; removes the
#      engine runtime (ADR-0016: the
#      engine account usnmeng with its containers, images and subordinate
#      ids, its Podman API units, the cgroup delegation, the scratch tmpfs
#      and the group usnmocr), stops the extension manager and removes any
#      extension still installed (`azcmagent extension remove --all`) and,
#      if the agent is configured (connected or not), runs
#      `azcmagent disconnect --force-local-only`,
#      which clears the agent's local state without signing in to Azure
#      (so no onboarding credential is needed), and on RHEL unregisters
#      it from Red Hat (`subscription-manager unregister`); without --host,
#      prints those commands instead;
#   4. deletes the Arc machine with your own az sign-in, which deletes its
#      identity.
# The on-prem Serial Set fetch (usnm-sset.service, #229) is left alone;
# stop and disable it first on a node that runs it (docs/operations.md).
# Tokens the node already holds stay valid until they expire: up to about
# 24 hours for a managed identity (ADR-0015). Run again, it skips what's done.
#
# --dry-run prints the plan; it changes nothing and doesn't connect to the
# node.
#
# Needs az signed in to the environment's tenant with the right to remove
# members from the nodes' group (an owner of it) and to delete resources in
# the environment's group (including the machine's extensions and data
# collection rule associations), and ssh for --host. Works with macOS's
# bash 3.2.
set -euo pipefail
usage() { sed -n '7,8s/^# \{0,1\}//p' "$0" >&2; exit 2; }
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

step "Monitoring"
# Resource Manager calls through az rest, as in onboard.sh.
ARM=https://management.azure.com
HC_API=2025-01-13
DCRA_API=2022-06-01
EXT=AzureMonitorLinuxAgent
# A GET; the query's result goes in ARM_OUT. Returns 1 only when the
# resource doesn't exist, and stops on any other failure.
ARM_OUT=""
# Hybrid Compute answers a missing extension with HCRP404 rather than
# ResourceNotFound, and now and then with HCRP500, so a 5xx is tried again
# up to three times.
arm_get() {
  local out err errf try
  ARM_OUT=""
  errf=$(mktemp)
  for try in 1 2 3; do
    if out=$(az rest --method get --url "$1" --query "$2" -o tsv 2> "$errf"); then
      rm -f "$errf"
      ARM_OUT=$out
      return 0
    fi
    err=$(cat "$errf")
    if grep -qE '"code": ?"(ResourceNotFound|HCRP404)"' <<< "$err"; then rm -f "$errf"; return 1; fi
    grep -qE 'Internal Server Error|Service Unavailable|Bad Gateway|Gateway Timeout|"code": ?"HCRP5[0-9][0-9]"' <<< "$err" || break
    [ "$try" = 3 ] || sleep 10
  done
  rm -f "$errf"
  die "can't read ${1%%\?*}: $err"
}
if [ -z "$MACHINE_ID" ]; then
  echo "no machine; nothing to remove"
else
  # Every association on the machine: its own rule's, and any other, since
  # the machine is going.
  dcras=""
  if arm_get "$ARM$MACHINE_ID/providers/Microsoft.Insights/dataCollectionRuleAssociations?api-version=$DCRA_API" \
    'value[].id'; then
    dcras=$ARM_OUT
  fi
  [ -n "$dcras" ] || echo "no data collection rule associations"
  for id in $dcras; do
    if [ "$DRY_RUN" = true ]; then
      echo "would delete ${id#"$MACHINE_ID"/providers/}"
    else
      az rest --method delete --url "$ARM$id?api-version=$DCRA_API" -o none ||
        die "can't delete $id"
      echo "deleted ${id#"$MACHINE_ID"/providers/}"
    fi
  done
  ext_url="$ARM$MACHINE_ID/extensions/$EXT?api-version=$HC_API"
  state=""
  if arm_get "$ext_url" properties.provisioningState; then state=${ARM_OUT:-unknown}; fi
  if [ -z "$state" ]; then
    echo "$EXT isn't installed"
  elif [ -z "$HOST" ]; then
    echo "$EXT ($state) goes with the machine; on a node that still exists, the node commands below remove it"
  elif [ "$DRY_RUN" = true ]; then
    echo "would delete $EXT ($state) and wait while the node uninstalls it"
  else
    # The node's agent uninstalls it and reports back; then the resource
    # is gone. Deletes are always allowed, whatever the allow list says.
    az rest --method delete --url "$ext_url" -o none || die "can't delete $EXT on $NAME"
    waited=0
    while arm_get "$ext_url" properties.provisioningState; do
      if [ "$ARM_OUT" = Failed ]; then
        echo "warning: deleting $EXT failed on the node; the node step removes it locally"
        break
      fi
      if [ "$waited" -ge 600 ]; then
        echo "warning: $EXT is still ${ARM_OUT:-being deleted} after 10 minutes; the node step removes it locally"
        break
      fi
      sleep 20
      waited=$((waited + 20))
    done
    [ "$waited" -ge 600 ] || [ "$ARM_OUT" = Failed ] || echo "deleted $EXT"
  fi
fi

step "Node"
# --force-local-only: the agent forgets its connection without asking Azure,
# so the node needs no credential; this script deletes the resource itself.
# The agent is cleared whenever it's configured (it has a resource name),
# whether it shows Connected or not: one that can't reach Azure shows
# Disconnected but still holds the machine's settings.
# The engine runtime that node-setup.sh sets up (ADR-0016), removed on the
# node.
# shellcheck disable=SC2016 # expanded on the node
ENGINE_REMOVE='if id usnmeng > /dev/null 2>&1; then
  uid=$(id -u usnmeng)
  systemctl --user -M usnmeng@ stop usnm-podman.socket usnm-podman.service 2> /dev/null || true
  loginctl disable-linger usnmeng || true
  loginctl terminate-user usnmeng 2> /dev/null || true
  systemctl stop "user@$uid.service" 2> /dev/null || true
  rm -f "/etc/systemd/system/user@$uid.service.d/50-usnm-delegate.conf"
  rmdir "/etc/systemd/system/user@$uid.service.d" 2> /dev/null || true
  semanage fcontext -d -e /var/lib/containers /var/lib/usnmeng/.local/share/containers 2> /dev/null || true
  userdel --remove usnmeng 2> /dev/null || userdel usnmeng
  rm -rf /var/lib/usnmeng
  sed -i "/^usnmeng:/d" /etc/subuid /etc/subgid
  echo "removed usnmeng"
fi
systemctl --global disable usnm-podman.socket 2> /dev/null || true
rm -f /etc/systemd/user/usnm-podman.socket /etc/systemd/user/usnm-podman.service
scratch=$(systemd-escape --path --suffix=mount /run/usnm-ocr/scratch)
systemctl disable --now "$scratch" 2> /dev/null || true
rm -f "/etc/systemd/system/$scratch" /etc/tmpfiles.d/usnm-ocr.conf
rm -rf /run/usnm-ocr
systemctl daemon-reload
if getent group usnmocr > /dev/null; then groupdel usnmocr && echo "removed group usnmocr"; fi'
# Automatic updates and the weekly reboot window that node-setup.sh sets
# up, and the worker's nightly update from install-worker.sh: a node kept or reused after offboarding neither updates nor reboots
# by itself. dnf-automatic stays installed.
UPDATES_REMOVE='systemctl disable --now usnm-ocr-reboot.timer usnm-ocr-update.timer dnf-automatic.timer 2> /dev/null || true
systemctl stop usnm-ocr-reboot.service usnm-ocr-update.service 2> /dev/null || true
systemctl disable usnm-ocr-reboot-resume.service 2> /dev/null || true
rm -f /etc/systemd/system/usnm-ocr-reboot.timer /etc/systemd/system/usnm-ocr-reboot.service \
  /etc/systemd/system/usnm-ocr-reboot-resume.service /usr/local/sbin/usnm-ocr-reboot-if-needed
rm -f /etc/systemd/system/usnm-ocr-update.timer /etc/systemd/system/usnm-ocr-update.service \
  /usr/local/sbin/usnm-ocr-update-worker
rm -rf /var/lib/usnm-ocr-reboot /var/lib/usnm-ocr-update /usr/local/libexec/usnm-ocr
systemctl daemon-reload'
# shellcheck disable=SC2016 # expanded on the node
REMOTE=''"$UPDATES_REMOVE"'
systemctl disable --now usnm-ocr-worker.service 2> /dev/null || true
'"$ENGINE_REMOVE"'
if ! command -v azcmagent > /dev/null; then
  echo "no Connected Machine agent"
else
  # Microsoft: stop the extension manager before azcmagent extension.
  systemctl stop extd 2> /dev/null || true
  azcmagent extension remove --all || echo "warning: azcmagent extension remove --all failed"
  if azcmagent show 2> /dev/null | grep -Eqi "^ *resource name *: *[^ ]"; then
    azcmagent disconnect --force-local-only
  else
    echo "agent not configured"
  fi
fi
# A RHEL node leaves Red Hat too; onboard.sh registers it again.
if [ "$(. /etc/os-release && printf %s "${ID:-}")" = rhel ] && command -v subscription-manager > /dev/null; then
  if command -v insights-client > /dev/null && insights-client --status > /dev/null 2>&1; then
    insights-client --unregister || echo "warning: insights-client --unregister failed"
  fi
  # Stop the daily upload too, registered or not, so a node kept or reused
  # after offboarding does not run an unregistered client every day.
  if systemctl list-unit-files insights-client.timer > /dev/null 2>&1; then
    systemctl disable --now insights-client.timer || echo "warning: could not disable insights-client.timer"
  fi
  if subscription-manager identity > /dev/null 2>&1; then
    subscription-manager unregister || echo "warning: subscription-manager unregister failed"
  else
    echo "not registered with Red Hat"
  fi
fi'
if [ -z "$HOST" ]; then
  echo "no --host; if the node still exists, run on it as root:"
  printf '%s\n' "$UPDATES_REMOVE" | while IFS= read -r line; do echo "  $line"; done
  echo "  systemctl disable --now usnm-ocr-worker.service"
  printf '%s\n' "$ENGINE_REMOVE" | while IFS= read -r line; do echo "  $line"; done
  echo "  systemctl stop extd"
  echo "  azcmagent extension remove --all"
  echo "  azcmagent disconnect --force-local-only"
  echo "  insights-client --unregister      # RHEL only"
  echo "  systemctl disable --now insights-client.timer   # RHEL only"
  echo "  subscription-manager unregister   # RHEL only"
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
