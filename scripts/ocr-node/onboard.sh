#!/usr/bin/env bash
# Onboard an on-prem OCR node (#296, ADR-0015) from the operator's machine:
# connect it to Azure Arc in the environment's resource group, add its
# identity to the nodes' Entra group, which the provision has already
# granted the exchange account's queues and output container (and the
# nodes' own Application Insights component, usnm-ocr-appi-<env>, for the
# worker's telemetry), and install the Azure Monitor Agent with the nodes'
# data collection rule, which sends to the nodes' own workspace. No
# provision is needed afterwards.
#
#   scripts/ocr-node/onboard.sh --env prod --host claude@node1.example.org \
#     --name usnm-ocr-node1 [--reboot-day Sun] [--ssh-key ~/.ssh/id_ed25519] [--dry-run]
#
# The node runs AlmaLinux 9 or RHEL 10 and takes key-based SSH with passwordless sudo
# for --host. Run with USNM_OCR_NODES=true already provisioned (the exchange
# account exists) and the group made (scripts/ocr-node/group.sh, which saves
# USNM_OCR_NODE_GROUP; provision after it). Steps:
#   1. registers Microsoft.HybridCompute, Microsoft.GuestConfiguration and
#      Microsoft.HybridConnectivity in the subscription;
#   2. unless the node is already connected, creates a throwaway app
#      registration and service principal with a 2-hour secret and only
#      Azure Connected Machine Onboarding on the environment's group;
#   3. copies scripts/ocr-node/node-setup.sh, the worker's unit,
#      usnm-ocr-worker.service, and reboot-if-needed.sh (the weekly reboot
#      window's script) to the node and runs node-setup.sh with
#      sudo, sending the tenant, app id and secret on its stdin (never on a
#      command line, never on the node's disk); on a RHEL node that isn't
#      registered with Red Hat yet, also the organization id and activation
#      key, which it reads from the nodes' Key Vault (OCR_KEYVAULT, secrets
#      rhel-org-id and rhel-activation-key) with your sign-in, the same way
#      (docs/operations.md); and the exchange account
#      and the nodes' Application Insights connection string
#      (OCR_NODE_APPINSIGHTS) as arguments (not secrets) for the worker's
#      unit, and --reboot-day, the node's day for its weekly reboot window
#      (a new node needs one no other node has; without it, a node keeps
#      the day it has);
#   4. deletes the role assignment, the service principal and the app
#      registration, and purges them from Entra's deleted items, as soon as
#      node-setup.sh has finished (the connect was their only use), or when
#      the script exits before that, on failure or Ctrl-C too;
#   5. waits for the Arc machine's system-assigned identity and adds it to
#      the group (once);
#   6. associates the machine with the nodes' data collection rule
#      (OCR_NODE_DCR_ID) and installs the Azure Monitor Linux Agent
#      extension, once the machine reports the allow list node-setup.sh set.
#      It stops if the machine has any other extension in Azure.
# Remove a node with scripts/ocr-node/offboard.sh.
#
# --dry-run checks the settings and your sign-in and prints the plan; it
# registers nothing, creates nothing in Entra, doesn't connect to the node,
# adds no group member and installs nothing. It lists the Key Vault's secret
# names (not their values) to check the Red Hat ones are there.
#
# Needs az signed in with the right to register applications in the tenant,
# to assign roles on the environment's group and to write its Arc machines,
# their extensions and their data collection rule associations (Owner; or
# User Access Administrator plus Contributor, or plus Azure Connected
# Machine Resource Administrator and Monitoring Contributor), and to add
# members to the nodes' group (an owner of it, as group.sh makes you), and
# ssh. For a RHEL node, also Key Vault Secrets Officer on the nodes' Key
# Vault (USNM_OCR_KEYVAULT_ADMINS), from an address its firewall allows
# (USNM_OCR_NODE_IPS). Registering the three providers is a subscription-level action
# (Microsoft.<namespace>/register/action, in Contributor or Owner on the
# subscription); without it they have to be registered already.
# Works with macOS's bash 3.2.
set -euo pipefail
usage() { sed -n '11,12s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

ENV_NAME="" HOST="" NAME="" SSH_KEY="" DRY_RUN=false REBOOT_DAY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --env) ENV_NAME=${2:-}; shift 2 ;;
    --host) HOST=${2:-}; shift 2 ;;
    --name) NAME=${2:-}; shift 2 ;;
    --reboot-day) REBOOT_DAY=${2:-}; shift 2 ;;
    --ssh-key) SSH_KEY=${2:-}; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || { echo "error: missing --env" >&2; usage; }
[ -n "$HOST" ] || { echo "error: missing --host" >&2; usage; }
[ -n "$NAME" ] || { echo "error: missing --name" >&2; usage; }
# Arc resource names: letters, digits, hyphens, periods and underscores; we
# keep to letters, digits and hyphens, and to 32, since the name is also
# the worker's --node-id.
[[ $NAME =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,31}$ ]] || die "--name: 1 to 32 letters, digits or hyphens, starting with a letter or digit"
[ -z "$SSH_KEY" ] || [ -r "$SSH_KEY" ] || die "--ssh-key: can't read $SSH_KEY"
case "$REBOOT_DAY" in ""|Mon|Tue|Wed|Thu|Fri|Sat|Sun) ;; *) die "--reboot-day: Mon, Tue, Wed, Thu, Fri, Sat or Sun" ;; esac

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# shellcheck source=scripts/lib/env.sh
. scripts/lib/env.sh
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME; run scripts/bootstrap.sh --env $ENV_NAME first"

SUBSCRIPTION=$(aget AZURE_SUBSCRIPTION_ID)
TENANT=$(aget AZURE_TENANT_ID)
GROUP=$(aget AZURE_RESOURCE_GROUP)
LOCATION=$(stack_location)
EXCHANGE=$(aget OCR_EXCHANGE_ACCOUNT)
NODES=$(aget USNM_OCR_NODES)
NODE_IPS=$(aget USNM_OCR_NODE_IPS)
NODE_GROUP=$(aget USNM_OCR_NODE_GROUP)
# The nodes' own component, never the environment's (ADR-0015 decision 8).
CONN=$(aget OCR_NODE_APPINSIGHTS)
APPI_ID=$(aget OCR_NODE_APPINSIGHTS_ID)
DCR_ID=$(aget OCR_NODE_DCR_ID)
# The nodes' Key Vault, for a RHEL node's Red Hat registration.
KEYVAULT=$(aget OCR_KEYVAULT)
[ -n "$SUBSCRIPTION" ] && [ -n "$TENANT" ] && [ -n "$GROUP" ] ||
  die "$ENV_FILE has no AZURE_SUBSCRIPTION_ID, AZURE_TENANT_ID or AZURE_RESOURCE_GROUP; run scripts/provision.sh --env $ENV_NAME"
[ "$NODES" = true ] ||
  die "USNM_OCR_NODES isn't true for $ENV_NAME: scripts/settings.sh --env $ENV_NAME --key USNM_OCR_NODES --value true, then scripts/provision.sh --env $ENV_NAME"
[ -n "$EXCHANGE" ] ||
  die "no OCR_EXCHANGE_ACCOUNT for $ENV_NAME yet: run scripts/provision.sh --env $ENV_NAME first"
[ -n "$NODE_GROUP" ] ||
  die "no USNM_OCR_NODE_GROUP for $ENV_NAME: scripts/ocr-node/group.sh --env $ENV_NAME, then scripts/provision.sh --env $ENV_NAME"
[ -n "$DCR_ID" ] ||
  die "no OCR_NODE_DCR_ID for $ENV_NAME (the nodes' data collection rule): run scripts/provision.sh --env $ENV_NAME first"
[ -n "$CONN" ] ||
  echo "warning: OCR_NODE_APPINSIGHTS (the nodes' Application Insights) is empty, so the worker will send no telemetry; run scripts/provision.sh --env $ENV_NAME"
[ -n "$NODE_IPS" ] ||
  echo "warning: USNM_OCR_NODE_IPS is empty, so the exchange account's firewall lets no node in; set it to the node's public IP and provision"

# Every az call names the subscription, so a dry run doesn't change your
# default subscription either.
az account show -o none 2> /dev/null || die "run: az login --tenant $TENANT"
# The az ad commands work in the current account's tenant, whatever
# --subscription says, so the app registration would land in the wrong
# tenant otherwise.
current_tenant=$(az account show --query tenantId -o tsv 2> /dev/null || true)
[ "$current_tenant" = "$TENANT" ] ||
  die "az's current account is in tenant ${current_tenant:-unknown}, not $TENANT: az login --tenant $TENANT, then az account set --subscription $SUBSCRIPTION"
az group show -n "$GROUP" --subscription "$SUBSCRIPTION" -o none 2> /dev/null ||
  die "can't read $GROUP in subscription $SUBSCRIPTION; sign in to tenant $TENANT with access to it"
# The nodes' Entra group; the provision grants it the node roles.
NODE_GROUP_NAME=$(az ad group show --group "$NODE_GROUP" --query displayName -o tsv 2> /dev/null) ||
  die "can't read group $NODE_GROUP (USNM_OCR_NODE_GROUP); run scripts/ocr-node/group.sh --env $ENV_NAME"
echo "environment $ENV_NAME: subscription $SUBSCRIPTION, $GROUP, $LOCATION, exchange account $EXCHANGE"
echo "node group $NODE_GROUP_NAME ($NODE_GROUP)"
appi=${APPI_ID##*/}
echo "data collection rule ${DCR_ID##*/}; worker telemetry to ${appi:-none}"
echo "node $HOST, Arc resource $NAME"
echo "Red Hat secrets, for a RHEL node: ${KEYVAULT:-no OCR_KEYVAULT}"
[ "$DRY_RUN" = false ] || echo "dry run: nothing is changed"

TAGS="project=usnewsmap,environment=$ENV_NAME,role=ocr-node"

step "Resource providers"
for ns in Microsoft.HybridCompute Microsoft.GuestConfiguration Microsoft.HybridConnectivity; do
  state=$(az provider show -n "$ns" --subscription "$SUBSCRIPTION" --query registrationState -o tsv 2> /dev/null || true)
  if [ "$state" = Registered ]; then
    echo "$ns: registered"
  elif [ "$DRY_RUN" = true ]; then
    echo "$ns: ${state:-unknown}; would register"
  else
    echo "$ns: ${state:-unknown}; registering"
    az provider register -n "$ns" --subscription "$SUBSCRIPTION" --wait -o none
  fi
done

# Always non-empty, so "${SSH_OPTS[@]}" is safe under set -u in bash 3.2.
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=20)
[ -z "$SSH_KEY" ] || SSH_OPTS+=(-i "$SSH_KEY" -o IdentitiesOnly=yes)
# Quoted for the node's shell, which ssh hands the command line to. No
# credential here: the tenant, app id and secret go on ssh's stdin.
# The TPM-backed identity (the key in the node's TPM) is in preview: Azure
# refuses it unless the subscription's Microsoft.HybridCompute/
# HybridComputePreview feature is Registered, and that feature needs
# Microsoft's approval (az feature register leaves it Pending until then).
# Ask for the TPM only when it's registered; otherwise the key stays in the
# agent's default store, and the node moves to the TPM later by offboarding
# and onboarding again.
tpm_feature=$(az feature show --namespace Microsoft.HybridCompute --name HybridComputePreview \
  --subscription "$SUBSCRIPTION" --query properties.state -o tsv 2> /dev/null || echo unknown)
if [ "$tpm_feature" = Registered ]; then
  KEY_STORE=tpm
else
  KEY_STORE=default
fi
SETUP_ARGS=$(printf '%q ' --subscription "$SUBSCRIPTION" --resource-group "$GROUP" \
  --location "$LOCATION" --name "$NAME" --tags "$TAGS" --identity-key-store "$KEY_STORE" \
  --exchange-account "$EXCHANGE" --app-insights "$CONN")
[ -z "$REBOOT_DAY" ] || SETUP_ARGS+=$(printf '%q ' --reboot-day "$REBOOT_DAY")
echo "identity key store: $KEY_STORE (HybridComputePreview feature: ${tpm_feature:-unknown})"

# The onboarding credential is a throwaway service principal with only Azure
# Connected Machine Onboarding on the group, made for this run and deleted
# once node-setup.sh has connected the node, or when the script exits before
# that, however it exits (ADR-0015). Your own token would carry all your
# rights for about an hour.
ONBOARD_ROLE=b64e21ea-ac4e-4cdf-9dc9-5b892992bee7 # Azure Connected Machine Onboarding
SCOPE="/subscriptions/$SUBSCRIPTION/resourceGroups/$GROUP"
SP_NAME="usnm-arc-onboard-$NAME-$(date -u +%Y%m%dT%H%M%SZ)"
APP_OBJ="" APP_ID="" SP_OBJ="" SECRET="" RA_ID="" SP_CREATED=false

# Deletes the role assignment, the service principal and the app
# registration, then purges both from Entra's deleted items. Looks each one
# up by name if the script stopped before it recorded its id.
cleanup() {
  # The status the script is exiting with, kept for the EXIT trap; 0 when
  # called once node-setup.sh has finished.
  # shellcheck disable=SC2319
  local status=$? ids id deleted="" ok attempt
  SECRET="" RH_ORG="" RH_KEY=""
  [ "$SP_CREATED" = true ] || return "$status"
  set +e
  step "Deleting the onboarding service principal"
  # The assignment this run made, by the id its create returned; the list
  # below, which can lag behind a new assignment, catches any other.
  if [ -n "$RA_ID" ]; then
    if az role assignment delete --ids "$RA_ID" --subscription "$SUBSCRIPTION" -o none; then
      echo "deleted role assignment $RA_ID"
    else
      echo "warning: couldn't delete role assignment $RA_ID; delete it by hand" >&2
    fi
  fi
  [ -n "$APP_OBJ" ] || APP_OBJ=$(az ad app list --display-name "$SP_NAME" --query '[0].id' -o tsv 2> /dev/null)
  [ -n "$APP_ID" ] || [ -z "$APP_OBJ" ] || APP_ID=$(az ad app show --id "$APP_OBJ" --query appId -o tsv 2> /dev/null)
  [ -n "$SP_OBJ" ] || [ -z "$APP_ID" ] ||
    SP_OBJ=$(az ad sp list --filter "appId eq '$APP_ID'" --query '[0].id' -o tsv 2> /dev/null)
  if [ -n "$SP_OBJ" ]; then
    ids=$(az role assignment list --assignee "$SP_OBJ" --scope "$SCOPE" --subscription "$SUBSCRIPTION" \
      --query '[].id' -o tsv 2> /dev/null)
    for id in $ids; do
      [ "$id" != "$RA_ID" ] || continue
      if az role assignment delete --ids "$id" --subscription "$SUBSCRIPTION" -o none; then
        echo "deleted role assignment $id"
      else
        echo "warning: couldn't delete role assignment $id; delete it by hand" >&2
      fi
    done
    if az ad sp delete --id "$SP_OBJ" -o none 2> /dev/null; then
      echo "deleted service principal $SP_OBJ"
      deleted="$SP_OBJ"
    else
      # Deleting the app registration below removes it too.
      echo "service principal $SP_OBJ goes with its app registration"
    fi
  fi
  if [ -n "$APP_OBJ" ]; then
    if az ad app delete --id "$APP_OBJ" -o none; then
      echo "deleted app registration $SP_NAME ($APP_OBJ, app id $APP_ID)"
      deleted="$deleted $APP_OBJ"
    else
      echo "warning: couldn't delete app registration $SP_NAME ($APP_OBJ); delete it by hand" >&2
    fi
  else
    echo "no app registration named $SP_NAME found"
  fi
  # Deleted objects stay restorable for 30 days; purge them. A soft-deleted
  # object can take a moment to show up in deleted items.
  for id in $deleted; do
    ok=false
    for attempt in 1 2 3 4 5; do
      if az rest --method delete --url "https://graph.microsoft.com/v1.0/directory/deletedItems/$id" \
        -o none 2> /dev/null; then
        ok=true
        break
      fi
      sleep 5
    done
    if [ "$ok" = true ]; then
      echo "purged $id from deleted items"
    else
      echo "warning: $id is still in Entra's deleted items (it goes by itself after 30 days)" >&2
    fi
  done
  return "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# Looks up the Arc machine and puts the query's result in MACHINE_OUT.
# Returns 1 only when the machine doesn't exist, and stops the script on any
# other failure (sign-in, throttling, network), so an API error is never
# taken for "not connected". Called in this shell, not in $(...), so its
# die ends the script.
MACHINE_OUT=""
machine_get() {
  local out err errf
  MACHINE_OUT=""
  # stderr apart, so a warning can't end up in the result.
  errf=$(mktemp)
  if out=$(az resource show -g "$GROUP" -n "$NAME" --resource-type Microsoft.HybridCompute/machines \
    --subscription "$SUBSCRIPTION" --query "$1" -o tsv 2> "$errf"); then
    rm -f "$errf"
    MACHINE_OUT=$out
    return 0
  fi
  err=$(cat "$errf")
  rm -f "$errf"
  if grep -qE '\((ResourceNotFound|ResourceGroupNotFound)\)' <<< "$err"; then return 1; fi
  die "can't look up $NAME in $GROUP: $err"
}

step "Node setup"
connected=""
if machine_get properties.status; then connected=$MACHINE_OUT; fi
if [ "$connected" = Connected ]; then
  echo "$NAME is already connected; no onboarding credential needed"
elif [ "$DRY_RUN" = true ]; then
  echo "would create app registration and service principal $SP_NAME with a secret valid for 2 hours,"
  echo "assign it Azure Connected Machine Onboarding ($ONBOARD_ROLE) on $SCOPE,"
  echo "and delete all three (purging the Entra objects) when this script exits"
fi
# The Red Hat organization id and activation key: read from the nodes' Key
# Vault only for a RHEL node that isn't registered, kept in this shell's
# memory, sent on node-setup.sh's stdin and cleared. Never printed.
RH_ORG="" RH_KEY=""
RH_SECRETS="rhel-org-id rhel-activation-key"
read_rh_secrets() {
  [ -n "$KEYVAULT" ] ||
    die "$HOST runs RHEL and isn't registered with Red Hat, and there's no OCR_KEYVAULT for $ENV_NAME: run scripts/provision.sh --env $ENV_NAME, then set the secrets (docs/operations.md)"
  local why="(you need Key Vault Secrets Officer, from USNM_OCR_KEYVAULT_ADMINS, and to be at an address in USNM_OCR_NODE_IPS)"
  RH_ORG=$(az keyvault secret show --vault-name "$KEYVAULT" --name rhel-org-id --query value -o tsv) ||
    die "can't read rhel-org-id from $KEYVAULT $why"
  RH_KEY=$(az keyvault secret show --vault-name "$KEYVAULT" --name rhel-activation-key --query value -o tsv) ||
    die "can't read rhel-activation-key from $KEYVAULT $why"
  [ -n "$RH_ORG" ] && [ -n "$RH_KEY" ] || die "rhel-org-id or rhel-activation-key in $KEYVAULT is empty"
  echo "read rhel-org-id and rhel-activation-key from $KEYVAULT"
}
if [ "$DRY_RUN" = true ]; then
  if [ -z "$KEYVAULT" ]; then
    echo "warning: no OCR_KEYVAULT, so a RHEL node that isn't registered with Red Hat can't be onboarded; run scripts/provision.sh --env $ENV_NAME"
  elif names=$(az keyvault secret list --vault-name "$KEYVAULT" --query '[].name' -o tsv 2> /dev/null); then
    for n in $RH_SECRETS; do
      if grep -qx "$n" <<< "$names"; then
        echo "$KEYVAULT has $n"
      else
        echo "warning: $KEYVAULT has no $n; a RHEL node that isn't registered can't be onboarded (docs/operations.md)"
      fi
    done
  else
    echo "warning: can't list $KEYVAULT's secrets (Key Vault Secrets Officer from USNM_OCR_KEYVAULT_ADMINS, at an address in USNM_OCR_NODE_IPS?)"
  fi
  echo "would copy scripts/ocr-node/node-setup.sh, usnm-ocr-worker.service and reboot-if-needed.sh to $HOST and run:"
  echo "  sudo bash node-setup.sh $SETUP_ARGS < (tenant, app id and secret; on RHEL not yet registered, the Red Hat organization id and activation key from ${KEYVAULT:-OCR_KEYVAULT})"
else
  if [ "$connected" != Connected ]; then
    # macOS date, then GNU date.
    end=$(date -u -v+2H +%Y-%m-%dT%H:%M:%SZ 2> /dev/null || date -u -d '+2 hours' +%Y-%m-%dT%H:%M:%SZ)
    echo "creating $SP_NAME (secret valid until $end)"
    SP_CREATED=true
    APP_OBJ=$(az ad app create --display-name "$SP_NAME" --sign-in-audience AzureADMyOrg --query id -o tsv) ||
      die "can't create an app registration (you need the right to register applications)"
    APP_ID=$(az ad app show --id "$APP_OBJ" --query appId -o tsv)
    SP_OBJ=$(az ad sp create --id "$APP_ID" --query id -o tsv) || die "can't create the service principal"
    # Replaces the app's (no) credentials with one secret; it stays in this
    # shell's memory and is never printed.
    SECRET=$(az ad app credential reset --id "$APP_OBJ" --end-date "$end" --display-name onboard \
      --query password -o tsv 2> /dev/null) || die "can't add a secret to $SP_NAME"
    RA_ID=$(az role assignment create --assignee-object-id "$SP_OBJ" --assignee-principal-type ServicePrincipal \
      --role "$ONBOARD_ROLE" --scope "$SCOPE" --subscription "$SUBSCRIPTION" --query id -o tsv) ||
      die "can't assign Azure Connected Machine Onboarding on $SCOPE"
    echo "assigned Azure Connected Machine Onboarding on $SCOPE"
    # The assignment and the secret take a little while to reach every
    # region; node-setup.sh retries the connect too.
    seen=""
    for attempt in 1 2 3 4 5 6 7 8 9 10 11 12; do
      seen=$(az role assignment list --assignee "$SP_OBJ" --scope "$SCOPE" --subscription "$SUBSCRIPTION" \
        --role "$ONBOARD_ROLE" --query '[0].id' -o tsv 2> /dev/null || true)
      [ -z "$seen" ] || break
      sleep 5
    done
    [ -n "$seen" ] || die "the role assignment for $SP_NAME didn't show up"
    sleep 30
  fi
  remote=$(ssh "${SSH_OPTS[@]}" "$HOST" 'mktemp -d /tmp/usnm-onboard.XXXXXX') ||
    die "can't reach $HOST over ssh"
  # shellcheck disable=SC2029 # $remote is meant to expand here
  ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/node-setup.sh" < scripts/ocr-node/node-setup.sh
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/usnm-ocr-worker.service" < scripts/ocr-node/usnm-ocr-worker.service
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/reboot-if-needed.sh" < scripts/ocr-node/reboot-if-needed.sh
  # A RHEL node registers with Red Hat before node-setup.sh installs
  # anything; one that's registered already needs no secrets.
  # shellcheck disable=SC2016 # expanded on the node
  rh=$(ssh "${SSH_OPTS[@]}" "$HOST" 'id=$(. /etc/os-release && printf %s "${ID:-}")
    if [ "$id" != rhel ]; then echo other
    elif sudo -n subscription-manager identity > /dev/null 2>&1; then echo registered
    else echo unregistered; fi' < /dev/null) || die "can't check $HOST's OS over ssh"
  case "$rh" in
    other) ;;
    registered) echo "$HOST runs RHEL and is registered with Red Hat" ;;
    unregistered) read_rh_secrets ;;
    *) die "unexpected answer from $HOST about its OS: $rh" ;;
  esac
  # The tenant, app id and secret, then the Red Hat organization id and
  # activation key, go to the node on ssh's stdin, never on a command line;
  # node-setup.sh reads them there. printf is a shell builtin, so they
  # aren't in this machine's process list either. On a connected,
  # registered node, nothing is sent.
  status=0
  if [ -n "$SECRET$RH_KEY" ]; then
    # shellcheck disable=SC2029 # the arguments are meant to expand here
    printf '%s\n%s\n%s\n%s\n%s\n' "${SECRET:+$TENANT}" "${SECRET:+$APP_ID}" "$SECRET" "$RH_ORG" "$RH_KEY" |
      ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash $remote/node-setup.sh $SETUP_ARGS" || status=$?
  else
    # shellcheck disable=SC2029
    ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash $remote/node-setup.sh $SETUP_ARGS" < /dev/null || status=$?
  fi
  SECRET="" RH_ORG="" RH_KEY=""
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "$HOST" "rm -rf $remote" || true
  [ "$status" -eq 0 ] || die "node-setup.sh failed on $HOST ($status)"
  # The node is connected; nothing below needs the principal, and the
  # monitoring step can wait up to 25 minutes. Delete it now (cleanup does
  # nothing unless this run created it), in a subshell, because cleanup
  # turns off set -e. The EXIT trap still covers the failures above, and
  # runs cleanup again if this one is interrupted.
  (cleanup) || true
  SP_CREATED=false
fi

# The Azure Monitor Agent and the nodes' data collection rule (ADR-0015
# decision 8). The template can't attach them, because it doesn't declare
# the machines. The extension is a child of the machine; offboard.sh
# deletes it and the association. Resource Manager calls go through az
# rest, so no az extension (connectedmachine, monitor-control-service) is
# needed.
ARM=https://management.azure.com
HC_API=2025-01-13
DCRA_API=2022-06-01
EXT=AzureMonitorLinuxAgent
EXT_TYPE=Microsoft.Azure.Monitor/$EXT
DCRA=${DCR_ID##*/}
lower() { tr '[:upper:]' '[:lower:]'; }

# A Resource Manager GET; the query's result goes in ARM_OUT. Like
# machine_get, returns 1 only when the resource doesn't exist and stops on
# any other failure. Called in this shell.
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
# What the machine last reported to Azure, which refuses an extension the
# machine's agent would refuse: "<extensionsEnabled> <allow list>" in CFG.
agent_config() {
  machine_get "join(' ', [to_string(properties.agentConfiguration.extensionsEnabled), join(',', \
    (properties.agentConfiguration.extensionsAllowList || \`[]\`)[].join('/', [publisher, type]))])" ||
    die "$NAME is no longer in $GROUP"
  CFG=$MACHINE_OUT
}
# The extension's provisioning state in STATE, empty if it isn't there, and
# UPGRADES: on if its automatic upgrade is on, else off. (Minor versions are
# picked at install or update, which the PUT below asks for anyway.)
ext_state() {
  STATE="" UPGRADES=off
  if arm_get "$ARM$1/extensions/$EXT?api-version=$HC_API" \
    "join(' ', [properties.provisioningState || '-', to_string(properties.enableAutomaticUpgrade)])"; then
    STATE=${ARM_OUT%% *}
    [ "$STATE" != - ] || STATE=""
    [ "${ARM_OUT#* }" != true ] || UPGRADES=on
  fi
}
# The rule the machine's association names, lowercase, in RULE; empty if
# there's no association.
dcra_rule() {
  RULE=""
  if arm_get "$ARM$1/providers/Microsoft.Insights/dataCollectionRuleAssociations/$DCRA?api-version=$DCRA_API" \
    properties.dataCollectionRuleId; then
    RULE=$(lower <<< "$ARM_OUT")
  fi
}

# Associates machine $1 with the rule and installs the agent; with
# --dry-run, says what it would do. Safe to run again.
CFG="" STATE="" UPGRADES=off RULE=""
monitoring() {
  local machine=$1 want_rule location waited
  step "Azure Monitor Agent"
  if [ -z "$machine" ]; then
    [ "$DRY_RUN" = true ] || die "can't find $NAME in $GROUP; run this again"
    echo "would associate $NAME with $DCRA and install $EXT (automatic upgrades on) once it's connected"
    return 0
  fi
  want_rule=$(lower <<< "$DCR_ID")
  agent_config
  dcra_rule "$machine"
  ext_state "$machine"
  echo "$NAME reports extensions enabled: ${CFG%% *}; allowed: ${CFG#* }"
  # The allow list stops new installs, not an extension installed before
  # it: that one stays, and runs again once the extension manager is on,
  # until it's deleted from Azure (Microsoft). So any other extension stops
  # this, in a dry run too. node-setup.sh checks the node's side.
  if arm_get "$ARM$machine/extensions?api-version=$HC_API" "value[?name!='$EXT'].name" && [ -n "$ARM_OUT" ]; then
    die "$NAME has other extensions in Azure: $(paste -sd, - <<< "$ARM_OUT"); delete each one" \
      "(az rest --method delete --url \"$ARM$machine/extensions/<name>?api-version=$HC_API\";" \
      "deletes are accepted whatever the allow list says) and run this again"
  fi
  if [ "$DRY_RUN" = true ]; then
    if [ "$RULE" = "$want_rule" ]; then
      echo "already associated with $DCRA"
    else
      echo "would associate it with $DCRA"
    fi
    if [ "$STATE" = Succeeded ] && [ "$UPGRADES" = on ]; then
      echo "$EXT is installed, automatic upgrades on"
    elif [ "$STATE" = Succeeded ]; then
      echo "$EXT is installed with automatic upgrades off; would turn them on"
    else
      echo "$EXT: ${STATE:-not installed}; would wait for the machine to report extensions enabled with"
      echo "only $EXT_TYPE allowed (node-setup.sh sets that), then install it (automatic upgrades on)"
    fi
    return 0
  fi

  if [ "$RULE" = "$want_rule" ]; then
    echo "already associated with $DCRA"
  else
    az rest --method put \
      --url "$ARM$machine/providers/Microsoft.Insights/dataCollectionRuleAssociations/$DCRA?api-version=$DCRA_API" \
      --body "{\"properties\": {\"dataCollectionRuleId\": \"$DCR_ID\", \"description\": \"usnewsmap OCR node (onboard.sh)\"}}" \
      -o none || die "can't associate $NAME with $DCRA"
    echo "associated $NAME with $DCRA"
  fi

  # Installed some other way (by hand, by policy), it may have automatic
  # upgrades off; the PUT below turns them on.
  if [ "$STATE" = Succeeded ] && [ "$UPGRADES" = on ]; then
    echo "$EXT is installed, automatic upgrades on"
    return 0
  fi
  # The machine reports a configuration change within a few minutes.
  waited=0
  while [ "$(lower <<< "$CFG")" != "$(lower <<< "true $EXT_TYPE")" ]; do
    [ "$waited" -lt 600 ] ||
      die "$NAME still reports extensions enabled: ${CFG%% *}; allowed: ${CFG#* } (want true and only $EXT_TYPE); check \`azcmagent config list\` on the node, then run this again"
    echo "waiting for $NAME to report the extension allow list ($waited s)"
    sleep 20
    waited=$((waited + 20))
    agent_config
  done
  case "$STATE" in
    Creating | Updating) echo "$EXT: $STATE" ;;
    *)
      machine_get location || die "$NAME is no longer in $GROUP"
      location=$MACHINE_OUT
      az rest --method put --url "$ARM$machine/extensions/$EXT?api-version=$HC_API" \
        --body "{\"location\": \"$location\", \"properties\": {\"publisher\": \"Microsoft.Azure.Monitor\", \"type\": \"$EXT\", \"enableAutomaticUpgrade\": true, \"autoUpgradeMinorVersion\": true}}" \
        -o none || die "can't install $EXT on $NAME"
      if [ "$STATE" = Succeeded ]; then
        echo "turning on automatic upgrades for $EXT"
      else
        echo "installing $EXT${STATE:+ (was $STATE)}"
      fi
      ;;
  esac
  # The node downloads and starts it; a few minutes.
  waited=0
  while :; do
    sleep 20
    waited=$((waited + 20))
    ext_state "$machine"
    case "$STATE" in
      Succeeded) break ;;
      Failed | Canceled)
        arm_get "$ARM$machine/extensions/$EXT?api-version=$HC_API" properties.instanceView.status.message || true
        die "$EXT on $NAME: $STATE${ARM_OUT:+ ($ARM_OUT)}; fix the cause and run this again"
        ;;
    esac
    [ "$waited" -lt 900 ] || die "$EXT on $NAME is still ${STATE:-unknown} after 15 minutes; run this again later"
  done
  if [ "$UPGRADES" = on ]; then
    echo "$EXT is installed, automatic upgrades on"
  else
    echo "warning: $EXT is installed but reports automatic upgrades off; run this again in a few minutes"
  fi
}

step "Machine"
# The provision grants the nodes' group the node roles
# (infra/modules/ocr-exchange.bicep); a node gets them by being a member.
# 0 if a member, 1 if not; stops if Graph can't say.
is_member() {
  local out
  out=$(az ad group member check --group "$NODE_GROUP" --member-id "$1" --query value -o tsv 2>&1) ||
    die "can't check whether $1 is in $NODE_GROUP_NAME: $out"
  case "$out" in
    true) return 0 ;;
    false) return 1 ;;
    *) die "can't check whether $1 is in $NODE_GROUP_NAME: $out" ;;
  esac
}
if [ "$DRY_RUN" = true ]; then
  principal=""
  if machine_get identity.principalId; then principal=$MACHINE_OUT; fi
  if [ -z "$principal" ]; then
    echo "$NAME isn't connected yet, or has no identity yet"
    echo "would add its identity to $NODE_GROUP_NAME"
  elif is_member "$principal"; then
    echo "$NAME (identity $principal) is already in $NODE_GROUP_NAME"
  else
    echo "would add $NAME (identity $principal) to $NODE_GROUP_NAME"
  fi
  if machine_get properties.agentUpgrade.enableAutomaticUpgrade && [ "$MACHINE_OUT" = true ]; then
    echo "automatic agent upgrade is on"
  else
    echo "would turn on automatic agent upgrade"
  fi
  machine=""
  if machine_get id; then machine=$MACHINE_OUT; fi
  monitoring "$machine"
  exit 0
fi
principal=""
for attempt in 1 2 3 4 5 6; do
  if machine_get identity.principalId; then principal=$MACHINE_OUT; fi
  [ -z "$principal" ] || break
  sleep 10
done
[ -n "$principal" ] || die "$NAME in $GROUP has no system-assigned identity yet (tried $attempt times); run this again"
echo "$NAME: identity $principal"

# A new identity's service principal can take a moment to reach Graph.
if is_member "$principal"; then
  echo "already in $NODE_GROUP_NAME"
else
  added=false
  for attempt in 1 2 3 4 5 6; do
    if az ad group member add --group "$NODE_GROUP" --member-id "$principal" -o none 2> /dev/null || is_member "$principal"; then
      added=true
      break
    fi
    sleep 10
  done
  [ "$added" = true ] || die "can't add $principal to $NODE_GROUP_NAME (you need to own the group); run this again"
  echo "added $NAME to $NODE_GROUP_NAME"
fi

# Microsoft's automatic agent upgrade (in preview; agent 1.57 or later, and
# cron on Linux, which node-setup.sh installs): the agent upgrades itself to
# within one version of the latest, in batches at off-peak hours. A machine
# connected before node-setup.sh passed --enable-automatic-upgrade gets it
# here, by setting the machine's agentUpgrade property.
if machine_get properties.agentUpgrade.enableAutomaticUpgrade && [ "$MACHINE_OUT" = true ]; then
  echo "automatic agent upgrade is already on"
else
  az rest --method patch --url "https://management.azure.com/subscriptions/$SUBSCRIPTION/resourceGroups/$GROUP/providers/Microsoft.HybridCompute/machines/$NAME?api-version=2024-05-20-preview" \
    --headers "Content-Type=application/json" \
    --body '{"properties":{"agentUpgrade":{"enableAutomaticUpgrade":true}}}' -o none ||
    die "can't turn on automatic agent upgrade for $NAME; run this again"
  echo "turned on automatic agent upgrade"
fi

machine_get id || die "$NAME is no longer in $GROUP"
monitoring "$MACHINE_OUT"

step "Next"
echo "$NAME has the node roles on $EXCHANGE through $NODE_GROUP_NAME; no provision needed."
echo "A token the node fetched for storage before it joined the group lacks the membership"
echo "until it expires (up to about 24 hours for a managed identity; ADR-0015)."
echo "Its heartbeat and counters reach the nodes' workspace within about 10 minutes:"
echo "  scripts/logs.sh --env $ENV_NAME --query-name ocr-node-health"
echo "The worker starts once its binary is installed on the node, as root:"
echo "  install -m 0755 usnm-ocr-worker /usr/local/bin/ && systemctl enable --now usnm-ocr-worker"
