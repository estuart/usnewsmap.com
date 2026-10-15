#!/usr/bin/env bash
# Create or delete the ephemeral BOINC test client VM (#484) in its own resource group
# usnm-boinc-test-rg-<env>.
#
# Usage:
#   scripts/boinc-test-client.sh --env <prod|dev> --create [--sku Standard_NC4as_T4_v3] [--spot|--paygo]
#     [--authenticator-file <PATH>] [--what-if]
#   scripts/boinc-test-client.sh --env <prod|dev> --delete [--what-if]
#
# The BOINC Key Vault disables public network access and template deployment, so the test VM
# either creates a disposable volunteer account on the server via `boinccmd --create_account`
# (the default) or reads an existing account's authenticator from `--authenticator-file <PATH>`
# into a mode-0600 `@params` file passed to the `@secure()` Bicep parameter.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/boinc-test-client.sh --env <prod|dev> (--create | --delete) [options]

  --env <prod|dev>                 Environment to target (required)
  --create                         Create usnm-boinc-test-rg-<env> and deploy the test client VM
  --delete                         Delete usnm-boinc-test-rg-<env> and everything in it
  --sku <SKU>                      Azure VM size (default: Standard_NC4as_T4_v3)
  --spot                           Deploy as an Azure Spot VM (default)
  --paygo                          Deploy as a regular pay-as-you-go VM
  --authenticator-file <PATH>      Optional file holding an existing BOINC authenticator (default: create a disposable account on the server)
  --what-if                        Check settings and print the plan without creating or deleting resources
  -h, --help                       Show this help
EOF
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

ENV_NAME=""
MODE=""
SKU="Standard_NC4as_T4_v3"
SPOT=true
AUTH_FILE=""
WHAT_IF=false

while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --create) [ -z "$MODE" ] || usage; MODE=create; shift ;;
    --delete) [ -z "$MODE" ] || usage; MODE=delete; shift ;;
    --sku) [ $# -ge 2 ] || usage; SKU=$2; shift 2 ;;
    --spot) SPOT=true; shift ;;
    --paygo) SPOT=false; shift ;;
    --authenticator-file) [ $# -ge 2 ] || usage; AUTH_FILE=$2; shift 2 ;;
    --what-if) WHAT_IF=true; shift ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done

[ -n "$ENV_NAME" ] && [ -n "$MODE" ] || usage
[[ $SKU =~ ^[A-Za-z0-9_]+$ ]] || die "--sku must be an Azure VM size name"

auth_val=""
if [ -n "$AUTH_FILE" ]; then
  [ -f "$AUTH_FILE" ] || die "--authenticator-file $AUTH_FILE does not exist"
  auth_val=$(tr -d '[:space:]' < "$AUTH_FILE")
  [[ -n $auth_val && $auth_val =~ ^[0-9a-fA-F_]+$ ]] ||
    die "--authenticator-file $AUTH_FILE must contain a non-empty BOINC authenticator"
fi

cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/boinc.sh disable=SC1091
. scripts/lib/boinc.sh
boinc_load_settings
boinc_az_check

TEST_RG="usnm-boinc-test-rg-$ENV_NAME"

if [ "$MODE" = delete ]; then
  if $WHAT_IF; then
    echo "would delete resource group $TEST_RG"
    exit 0
  fi
  if [ "$(baz group exists --name "$TEST_RG")" = true ]; then
    baz group delete --name "$TEST_RG" --yes --no-wait
    echo "deleting $TEST_RG"
  else
    echo "$TEST_RG does not exist"
  fi
  exit 0
fi

vault=$(boinc_output keyVaultName)
[ -n "$vault" ] || die "stack $BOINC_STACK has no keyVaultName output: run scripts/provision-boinc.sh --env $ENV_NAME first"

master_url="${PROJECT_URL}usnmocr/"

params=$(umask 077 && mktemp)
trap 'rm -f "$params"' EXIT
printf '%s' "$auth_val" | jq -Rs \
  --arg env "$ENV_NAME" \
  --arg location "$LOCATION" \
  --arg vmSize "$SKU" \
  --argjson spot "$SPOT" \
  --arg projectUrl "$master_url" \
  '{
    "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentParameters.json#",
    contentVersion: "1.0.0.0",
    parameters: {
      env: { value: $env },
      location: { value: $location },
      vmSize: { value: $vmSize },
      spot: { value: $spot },
      projectUrl: { value: $projectUrl },
      projectAuthenticator: { value: . }
    }
  }' > "$params"

if $WHAT_IF; then
  auth_plan="create disposable account via boinccmd --create_account"
  [ -z "$AUTH_FILE" ] || auth_plan="from $AUTH_FILE"
  cat <<PLAN
would create $TEST_RG in $LOCATION and deploy contrib-compute/infra/boinc/test-client.bicep:
  vmSize: $SKU (spot=$SPOT)
  projectUrl: $master_url
  projectAuthenticator: $auth_plan
PLAN
  exit 0
fi

baz group create --name "$TEST_RG" --location "$LOCATION" -o none
baz deployment group create \
  --name "usnm-boinc-test-$ENV_NAME" \
  --resource-group "$TEST_RG" \
  --template-file contrib-compute/infra/boinc/test-client.bicep \
  --parameters "@$params"
