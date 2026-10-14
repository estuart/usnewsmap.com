# shellcheck shell=bash disable=SC2034
# Shared by scripts/provision-boinc.sh and scripts/build-boinc-images.sh. Sourced from the
# repository root after ENV_NAME and ALLOW_DIRTY are set; needs a die() function.
#
# The BOINC server's settings are committed in contrib-compute/infra/boinc/env/<env>.json
# (schema alongside). Its resources are the deployment stack usnm-boinc-<env> in the group
# usnm-boinc-rg-<env>, which the main stack creates (infra/main.bicep, deployBoincGroup).

BOINC_DIR=contrib-compute/infra/boinc
BOINC_SETTINGS="$BOINC_DIR/env/$ENV_NAME.json"
BOINC_RG="usnm-boinc-rg-$ENV_NAME"
BOINC_STACK="usnm-boinc-$ENV_NAME"
# Never touched by these scripts (other production resources).
FORBIDDEN_SUBSCRIPTION_PREFIX=8d356797

case "$ENV_NAME" in prod | dev) ;; *) die "--env must be prod or dev (the BOINC server has no experiment environments)" ;; esac
[ -f "$BOINC_SETTINGS" ] || die "no settings file $BOINC_SETTINGS"

# The settings file and its schema must be committed, as scripts/provision.sh requires of
# infra/env/<env>.json, unless --allow-dirty (dev only).
boinc_clean_check() {
  local out
  out=$(git status --porcelain -- "$BOINC_SETTINGS" "$BOINC_DIR/env/schema.json" 2>&1) ||
    die "can't check $BOINC_SETTINGS with git: $out"
  [ -z "$out" ] && return 0
  printf '%s\n' "$out" >&2
  $ALLOW_DIRTY || die "$BOINC_SETTINGS has uncommitted changes (above): commit them through a pull request, or pass --allow-dirty for an experiment in dev"
  [ "$ENV_NAME" != prod ] || die "--allow-dirty is refused for prod: commit $BOINC_SETTINGS first"
  echo "warning: deploying uncommitted settings (--allow-dirty)" >&2
}

# Checks the settings against the rules in schema.json, with jq, and reads them into
# ASSESSMENT, SUBSCRIPTION, LOCATION, PROJECT_URL, MASTER_HOST, OPERATOR_ID, OPERATOR_LOGIN,
# SERVER_IMAGE, BRIDGE_IMAGE, WORKER_IMAGE and APPVER_IMAGE.
boinc_load_settings() {
  local err
  err=$(jq -r '
    def guid: test("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$");
    def acr: test("^$|^[a-z0-9.-]+\\.azurecr\\.io/[a-z0-9/_.-]+@sha256:[0-9a-f]{64}$");
    [ (keys - ["$schema", "assessment", "codeSignPublicKey", "images", "location", "operator", "projectUrl", "subscriptionId"]
        | if length > 0 then "unknown keys: \(join(", "))" else empty end),
      (if keys_unsorted != keys then "keys are not sorted" else empty end),
      (if (.assessment | type) != "boolean" then "assessment must be true or false" else empty end),
      (if (.codeSignPublicKey | type) != "string" then "codeSignPublicKey must be a string" else empty end),
      (if (.images | type) != "object" or (.images | keys) != ["appver", "bridge", "server", "worker"] then "images must have exactly appver, bridge, server and worker" else empty end),
      (if (.images.server // "" | acr) and (.images.bridge // "" | acr) then empty else "images.server and images.bridge must be empty or <registry>.azurecr.io/<name>@sha256:<64 hex>" end),
      (if (.images.appver // "") | test("^$|^[a-z0-9.-]+\\.azurecr\\.io/usnm-boinc-appver@sha256:[0-9a-f]{64}$") then empty else "images.appver must be empty or <registry>.azurecr.io/usnm-boinc-appver@sha256:<64 hex>" end),
      (if (.images.appver // "" | length > 0) and (.images.server // "" | length == 0) then "images.appver needs images.server (the publish job runs in the app environment)" else empty end),
      (if (.images.worker // "") | test("^$|^ghcr\\.io/tgoodyear/usnm-boinc-worker@sha256:[0-9a-f]{64}$") then empty else "images.worker must be empty or ghcr.io/tgoodyear/usnm-boinc-worker@sha256:<64 hex>" end),
      (if (.images.server // "" | length > 0) != (.images.bridge // "" | length > 0) then "images.server and images.bridge are set together (the bridge image is built on the server image)" else empty end),
      (if (.location // "") | test("^[a-z0-9]+$") then empty else "location must be an Azure region name" end),
      (if (.operator.objectId // "") | guid then empty else "operator.objectId must be a lower-case GUID" end),
      (if (.operator.login // "") | test("^[[:print:]]+$") then empty else "operator.login must be one printable line" end),
      (if .projectUrl == "https://boinc.usnewsmap.com/" or .projectUrl == "https://boinc-dev.usnewsmap.com/" then empty else "projectUrl must be https://boinc.usnewsmap.com/ or https://boinc-dev.usnewsmap.com/" end),
      (if (.subscriptionId // "") | guid then empty else "subscriptionId must be a lower-case GUID" end)
    ] | .[]' "$BOINC_SETTINGS") || die "$BOINC_SETTINGS is not valid JSON"
  [ -z "$err" ] || die "$BOINC_SETTINGS: $(printf '%s; ' "$err" | sed 's/; $//')"
  ASSESSMENT=$(jq -r .assessment "$BOINC_SETTINGS")
  SUBSCRIPTION=$(jq -r .subscriptionId "$BOINC_SETTINGS")
  LOCATION=$(jq -r .location "$BOINC_SETTINGS")
  PROJECT_URL=$(jq -r .projectUrl "$BOINC_SETTINGS")
  OPERATOR_ID=$(jq -r .operator.objectId "$BOINC_SETTINGS")
  OPERATOR_LOGIN=$(jq -r .operator.login "$BOINC_SETTINGS")
  SERVER_IMAGE=$(jq -r .images.server "$BOINC_SETTINGS")
  BRIDGE_IMAGE=$(jq -r .images.bridge "$BOINC_SETTINGS")
  WORKER_IMAGE=$(jq -r .images.worker "$BOINC_SETTINGS")
  APPVER_IMAGE=$(jq -r .images.appver "$BOINC_SETTINGS")
  MASTER_HOST=${PROJECT_URL#https://}
  MASTER_HOST=${MASTER_HOST%/}
  case "$ENV_NAME:$MASTER_HOST" in
    prod:boinc.usnewsmap.com | dev:boinc-dev.usnewsmap.com) ;;
    *) die "projectUrl $PROJECT_URL is not $ENV_NAME's master URL" ;;
  esac
  case "$SUBSCRIPTION" in "$FORBIDDEN_SUBSCRIPTION_PREFIX"-*) die "refusing subscription $SUBSCRIPTION: it holds other production resources" ;; esac
}

# Every az call names the subscription; the default one is never used.
baz() { az "$@" --subscription "$SUBSCRIPTION" --only-show-errors; }

boinc_az_check() {
  command -v az > /dev/null || die "az is not installed"
  command -v jq > /dev/null || die "jq is not installed"
  [ "$(az account show --subscription "$SUBSCRIPTION" --query id -o tsv 2> /dev/null)" = "$SUBSCRIPTION" ] ||
    die "az cannot see subscription $SUBSCRIPTION (az login --tenant <its tenant> first)"
  baz group show --name "$BOINC_RG" --query name -o none 2> /dev/null ||
    die "no resource group $BOINC_RG: it comes from the main stack (USNM_BOINC_GROUP true in infra/env/$ENV_NAME.json, then scripts/provision.sh --env $ENV_NAME)"
}

# boinc_output NAME: one output of the BOINC stack, empty if the stack or the output is
# missing. Matched without case: a stack can return output names with their case changed
# (scripts/lib/env.sh, save_outputs).
boinc_output() {
  local out
  if ! out=$(baz stack group show --name "$BOINC_STACK" --resource-group "$BOINC_RG" --query outputs -o json 2>&1); then
    grep -qiE 'NotFound|could not be found' <<< "$out" && return 0
    die "can't read the stack $BOINC_STACK: $out"
  fi
  jq -r --arg n "$1" '(. // {}) | to_entries[] | select((.key | ascii_downcase) == ($n | ascii_downcase)) | .value.value // empty' <<< "$out"
}
