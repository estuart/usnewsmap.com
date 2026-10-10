#!/usr/bin/env bash
# Onboard an on-prem OCR node (#296, ADR-0015) from the operator's machine:
# connect it to Azure Arc in the environment's resource group and record its
# identity, so the next provision grants it the exchange account's queues
# and output container.
#
#   scripts/ocr-node/onboard.sh --env prod --host claude@node1.example.org \
#     --name usnm-ocr-node1 [--ssh-key ~/.ssh/id_ed25519] [--dry-run]
#
# The node runs AlmaLinux 9 or RHEL 10 and takes key-based SSH with passwordless sudo
# for --host. Run with USNM_OCR_NODES=true already provisioned (the exchange
# account exists). Steps:
#   1. registers Microsoft.HybridCompute, Microsoft.GuestConfiguration and
#      Microsoft.HybridConnectivity in the subscription;
#   2. copies scripts/ocr-node/node-setup.sh to the node and runs it with
#      sudo, sending a short-lived ARM access token from your az sign-in on
#      its stdin (never on a command line, never on the node's disk);
#   3. reads the machine's system-assigned identity and adds it to
#      USNM_OCR_NODE_PRINCIPALS.
# It doesn't provision: it prints the command for that.
#
# --dry-run checks the settings and your sign-in and prints the plan; it
# registers nothing, doesn't connect to the node and changes no setting.
#
# Needs az signed in with Owner on the environment's subscription, and ssh.
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
PRINCIPALS=$(aget USNM_OCR_NODE_PRINCIPALS)
[ -n "$SUBSCRIPTION" ] && [ -n "$TENANT" ] && [ -n "$GROUP" ] ||
  die "$ENV_FILE has no AZURE_SUBSCRIPTION_ID, AZURE_TENANT_ID or AZURE_RESOURCE_GROUP; run scripts/provision.sh $ENV_NAME"
[ "$NODES" = true ] ||
  die "USNM_OCR_NODES isn't true for $ENV_NAME: scripts/settings.sh $ENV_NAME USNM_OCR_NODES true, then scripts/provision.sh $ENV_NAME"
[ -n "$EXCHANGE" ] ||
  die "no OCR_EXCHANGE_ACCOUNT for $ENV_NAME yet: run scripts/provision.sh $ENV_NAME first"
[ -n "$NODE_IPS" ] ||
  echo "warning: USNM_OCR_NODE_IPS is empty, so the exchange account's firewall lets no node in; set it to the node's public IP and provision"

# Every az call names the subscription, so a dry run doesn't change your
# default subscription either.
az account show -o none 2> /dev/null || die "run: az login --tenant $TENANT"
az group show -n "$GROUP" --subscription "$SUBSCRIPTION" -o none 2> /dev/null ||
  die "can't read $GROUP in subscription $SUBSCRIPTION; sign in to tenant $TENANT with access to it"
echo "environment $ENV_NAME: subscription $SUBSCRIPTION, $GROUP, $LOCATION, exchange account $EXCHANGE"
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
# Quoted for the node's shell, which ssh hands the command line to.
SETUP_ARGS=$(printf '%q ' --tenant "$TENANT" --subscription "$SUBSCRIPTION" --resource-group "$GROUP" \
  --location "$LOCATION" --name "$NAME" --tags "$TAGS")

step "Node setup"
if [ "$DRY_RUN" = true ]; then
  echo "would copy scripts/ocr-node/node-setup.sh to $HOST and run:"
  echo "  sudo bash node-setup.sh $SETUP_ARGS < (ARM access token)"
else
  remote=$(ssh "${SSH_OPTS[@]}" "$HOST" 'mktemp -d /tmp/usnm-onboard.XXXXXX') ||
    die "can't reach $HOST over ssh"
  # shellcheck disable=SC2029 # $remote is meant to expand here
  ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/node-setup.sh" < scripts/ocr-node/node-setup.sh
  # The token lives in this shell's memory and goes to the node on ssh's
  # stdin; node-setup.sh reads it there. It's valid for about an hour and
  # carries your rights (Owner), so root on the node could copy it while
  # it's there: onboard only a node you have just built (ADR-0015). A
  # service principal with only Azure Connected Machine Onboarding would
  # avoid that, at the price of a credential and an app registration.
  token=$(az account get-access-token --resource https://management.azure.com --tenant "$TENANT" \
    --query accessToken -o tsv) || die "can't get an access token"
  status=0
  # shellcheck disable=SC2029 # the arguments are meant to expand here
  printf '%s\n' "$token" |
    ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash $remote/node-setup.sh $SETUP_ARGS" || status=$?
  token=""
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "$HOST" "rm -rf $remote" || true
  [ "$status" -eq 0 ] || die "node-setup.sh failed on $HOST ($status)"
fi

step "Machine identity"
if [ "$DRY_RUN" = true ]; then
  if principal=$(az resource show -g "$GROUP" -n "$NAME" --resource-type Microsoft.HybridCompute/machines \
    --subscription "$SUBSCRIPTION" --query identity.principalId -o tsv 2> /dev/null); then
    echo "$NAME exists, identity $principal"
  else
    echo "$NAME isn't connected yet"
  fi
  echo "would add its principal id to USNM_OCR_NODE_PRINCIPALS (now: ${PRINCIPALS:-empty})"
  exit 0
fi
principal=""
for attempt in 1 2 3 4 5 6; do
  principal=$(az resource show -g "$GROUP" -n "$NAME" --resource-type Microsoft.HybridCompute/machines \
    --subscription "$SUBSCRIPTION" --query identity.principalId -o tsv 2> /dev/null || true)
  [ -z "$principal" ] || break
  sleep 10
done
[ -n "$principal" ] || die "can't read $NAME's identity in $GROUP (tried $attempt times)"
echo "$NAME: identity $principal"

# Add it once; keep the others.
case ",$PRINCIPALS," in
  *",$principal,"*)
    echo "already in USNM_OCR_NODE_PRINCIPALS" ;;
  *)
    scripts/settings.sh "$ENV_NAME" USNM_OCR_NODE_PRINCIPALS "${PRINCIPALS:+$PRINCIPALS,}$principal"
    echo "added to USNM_OCR_NODE_PRINCIPALS" ;;
esac

step "Next"
echo "Grant the node its roles on $EXCHANGE by provisioning (needs Owner on the subscription):"
echo "  scripts/provision.sh $ENV_NAME"
