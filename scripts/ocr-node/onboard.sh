#!/usr/bin/env bash
# Onboard an on-prem OCR node (#296, ADR-0015) from the operator's machine:
# connect it to Azure Arc in the environment's resource group and add its
# identity to the nodes' Entra group, which the provision has already
# granted the exchange account's queues and output container. No provision
# is needed afterwards.
#
#   scripts/ocr-node/onboard.sh --env prod --host claude@node1.example.org \
#     --name usnm-ocr-node1 [--ssh-key ~/.ssh/id_ed25519] [--dry-run]
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
#   3. copies scripts/ocr-node/node-setup.sh to the node and runs it with
#      sudo, sending the tenant, app id and secret on its stdin (never on a
#      command line, never on the node's disk);
#   4. deletes the role assignment, the service principal and the app
#      registration, and purges them from Entra's deleted items, when the
#      script exits, on failure or Ctrl-C too;
#   5. waits for the Arc machine's system-assigned identity and adds it to
#      the group (once).
# Remove a node with scripts/ocr-node/offboard.sh.
#
# --dry-run checks the settings and your sign-in and prints the plan; it
# registers nothing, creates nothing in Entra, doesn't connect to the node
# and adds no group member.
#
# Needs az signed in with the right to register applications in the tenant,
# to assign roles on the environment's group and to add members to the
# nodes' group (an owner of it, as group.sh makes you), and ssh. Registering the
# three providers is a subscription-level action
# (Microsoft.<namespace>/register/action, in Contributor or Owner on the
# subscription); without it they have to be registered already.
# Works with macOS's bash 3.2.
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
[ -n "$HOST" ] || { echo "error: missing --host" >&2; usage; }
[ -n "$NAME" ] || { echo "error: missing --name" >&2; usage; }
# Arc resource names: letters, digits, hyphens, periods and underscores; we
# keep to letters, digits and hyphens.
[[ $NAME =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,53}$ ]] || die "--name: 1 to 54 letters, digits or hyphens, starting with a letter or digit"
[ -z "$SSH_KEY" ] || [ -r "$SSH_KEY" ] || die "--ssh-key: can't read $SSH_KEY"

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# shellcheck source=scripts/lib/env.sh
. scripts/lib/env.sh
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME; run scripts/bootstrap.sh $ENV_NAME first"

SUBSCRIPTION=$(aget AZURE_SUBSCRIPTION_ID)
TENANT=$(aget AZURE_TENANT_ID)
GROUP=$(aget AZURE_RESOURCE_GROUP)
LOCATION=$(stack_location)
EXCHANGE=$(aget OCR_EXCHANGE_ACCOUNT)
NODES=$(aget USNM_OCR_NODES)
NODE_IPS=$(aget USNM_OCR_NODE_IPS)
NODE_GROUP=$(aget USNM_OCR_NODE_GROUP)
[ -n "$SUBSCRIPTION" ] && [ -n "$TENANT" ] && [ -n "$GROUP" ] ||
  die "$ENV_FILE has no AZURE_SUBSCRIPTION_ID, AZURE_TENANT_ID or AZURE_RESOURCE_GROUP; run scripts/provision.sh $ENV_NAME"
[ "$NODES" = true ] ||
  die "USNM_OCR_NODES isn't true for $ENV_NAME: scripts/settings.sh $ENV_NAME USNM_OCR_NODES true, then scripts/provision.sh $ENV_NAME"
[ -n "$EXCHANGE" ] ||
  die "no OCR_EXCHANGE_ACCOUNT for $ENV_NAME yet: run scripts/provision.sh $ENV_NAME first"
[ -n "$NODE_GROUP" ] ||
  die "no USNM_OCR_NODE_GROUP for $ENV_NAME: scripts/ocr-node/group.sh --env $ENV_NAME, then scripts/provision.sh $ENV_NAME"
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
echo "node $HOST, Arc resource $NAME"
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
SETUP_ARGS=$(printf '%q ' --subscription "$SUBSCRIPTION" --resource-group "$GROUP" \
  --location "$LOCATION" --name "$NAME" --tags "$TAGS")

# The onboarding credential is a throwaway service principal with only Azure
# Connected Machine Onboarding on the group, made for this run and deleted
# when the script exits, however it exits (ADR-0015). Your own token would
# carry all your rights for about an hour.
ONBOARD_ROLE=b64e21ea-ac4e-4cdf-9dc9-5b892992bee7 # Azure Connected Machine Onboarding
SCOPE="/subscriptions/$SUBSCRIPTION/resourceGroups/$GROUP"
SP_NAME="usnm-arc-onboard-$NAME-$(date -u +%Y%m%dT%H%M%SZ)"
APP_OBJ="" APP_ID="" SP_OBJ="" SECRET="" RA_ID="" SP_CREATED=false

# Deletes the role assignment, the service principal and the app
# registration, then purges both from Entra's deleted items. Looks each one
# up by name if the script stopped before it recorded its id.
cleanup() {
  local status=$? ids id deleted="" ok attempt
  SECRET=""
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

step "Node setup"
connected=$(az resource show -g "$GROUP" -n "$NAME" --resource-type Microsoft.HybridCompute/machines \
  --subscription "$SUBSCRIPTION" --query properties.status -o tsv 2> /dev/null || true)
if [ "$connected" = Connected ]; then
  echo "$NAME is already connected; no onboarding credential needed"
elif [ "$DRY_RUN" = true ]; then
  echo "would create app registration and service principal $SP_NAME with a secret valid for 2 hours,"
  echo "assign it Azure Connected Machine Onboarding ($ONBOARD_ROLE) on $SCOPE,"
  echo "and delete all three (purging the Entra objects) when this script exits"
fi
if [ "$DRY_RUN" = true ]; then
  echo "would copy scripts/ocr-node/node-setup.sh to $HOST and run:"
  echo "  sudo bash node-setup.sh $SETUP_ARGS < (tenant, app id and secret)"
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
  # The tenant, app id and secret go to the node on ssh's stdin, never on a
  # command line; node-setup.sh reads them there. On a connected node,
  # nothing is sent.
  status=0
  if [ -n "$SECRET" ]; then
    # shellcheck disable=SC2029 # the arguments are meant to expand here
    printf '%s\n%s\n%s\n' "$TENANT" "$APP_ID" "$SECRET" |
      ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash $remote/node-setup.sh $SETUP_ARGS" || status=$?
  else
    # shellcheck disable=SC2029
    ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash $remote/node-setup.sh $SETUP_ARGS" < /dev/null || status=$?
  fi
  SECRET=""
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "$HOST" "rm -rf $remote" || true
  [ "$status" -eq 0 ] || die "node-setup.sh failed on $HOST ($status)"
fi

step "Machine"
# The provision grants the nodes' group the node roles
# (infra/modules/ocr-exchange.bicep); a node gets them by being a member.
is_member() {
  [ "$(az ad group member check --group "$NODE_GROUP" --member-id "$1" --query value -o tsv 2> /dev/null)" = true ]
}
if [ "$DRY_RUN" = true ]; then
  principal=$(az resource show -g "$GROUP" -n "$NAME" --resource-type Microsoft.HybridCompute/machines \
    --subscription "$SUBSCRIPTION" --query identity.principalId -o tsv 2> /dev/null || true)
  if [ -z "$principal" ]; then
    echo "$NAME isn't connected yet, or has no identity yet"
    echo "would add its identity to $NODE_GROUP_NAME"
  elif is_member "$principal"; then
    echo "$NAME (identity $principal) is already in $NODE_GROUP_NAME"
  else
    echo "would add $NAME (identity $principal) to $NODE_GROUP_NAME"
  fi
  exit 0
fi
principal=""
for attempt in 1 2 3 4 5 6; do
  principal=$(az resource show -g "$GROUP" -n "$NAME" --resource-type Microsoft.HybridCompute/machines \
    --subscription "$SUBSCRIPTION" --query identity.principalId -o tsv 2> /dev/null || true)
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

step "Next"
echo "$NAME has the node roles on $EXCHANGE through $NODE_GROUP_NAME; no provision needed."
echo "A token the node fetched for storage before it joined the group lacks the membership"
echo "until it expires (up to about 24 hours for a managed identity; ADR-0015)."
