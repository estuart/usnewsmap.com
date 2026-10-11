# shellcheck shell=bash
# Shared by the environment scripts; source it with ENV_NAME set and the
# repository root as the working directory. Needs az signed in to the
# environment's subscription for the stack functions, and a die() function.
#
# An environment's settings are KEY="value" lines (the layout azd used, so
# older environments carry over) in one file: $USNM_SETTINGS_HOME/<env>/.env
# (default ~/.config/usnewsmap/<env>/.env) when that directory exists, so
# every checkout reads the same settings, otherwise the checkout's own
# .azure/<env>/.env (#275).
#
# The name becomes part of resource names (the registry allows only
# lowercase letters and digits), the stack's name and the settings path.
# "guardrails" is taken: usnm-guardrails is the shared policy stack.
valid_env_name() {
  [[ $1 =~ ^[a-z][a-z0-9]{0,15}$ ]] || {
    echo "error: the environment name must be 1-16 lowercase letters and digits, starting with a letter" >&2
    exit 2
  }
  # usnm-<name> would be another stack's name, and rg-usnm-<name> its group.
  case "$1" in
    guardrails | archive) echo "error: \"$1\" is reserved" >&2; exit 2 ;;
  esac
}
valid_env_name "$ENV_NAME"
# need_stack_az, GUARDRAILS_STACK, stack_param and deploy_guardrails.
# shellcheck source=scripts/lib/guardrails.sh
. scripts/lib/guardrails.sh
# A copy in a checkout can be stale: once the shared directory exists (a
# teardown leaves it, holding the kept file), the checkout's copy is ignored.
SETTINGS_HOME=${USNM_SETTINGS_HOME:-${XDG_CONFIG_HOME:-${HOME:-}/.config}/usnewsmap}
if [ -d "$SETTINGS_HOME/$ENV_NAME" ]; then
  ENV_FILE="$SETTINGS_HOME/$ENV_NAME/.env"
  [ ! -f ".azure/$ENV_NAME/.env" ] ||
    echo "note: $ENV_NAME's settings are $ENV_FILE; ignoring this checkout's .azure/$ENV_NAME/.env (delete it)" >&2
else
  ENV_FILE=".azure/$ENV_NAME/.env"
fi
# A settings file that doesn't parse stops every script up front, before
# anything reads a setting as empty and falls back to a default.
# shellcheck source=/dev/null
if [ -f "$ENV_FILE" ] && ! (set +u; . "$ENV_FILE") > /dev/null; then
  echo "error: $ENV_FILE doesn't parse; fix it before running anything" >&2
  exit 1
fi
# Setting names are shell variable names.
valid_key() {
  [[ $1 =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || { echo "error: invalid setting name: $1" >&2; exit 2; }
}
# Read one setting (empty when unset).
# shellcheck source=/dev/null
# Only the file counts: the caller's variable of the same name is cleared.
# A settings file that can't be read or parsed fails the read.
aget() {
  valid_key "$1"
  (set +u; unset "$1"; set -a
   if [ -f "$ENV_FILE" ]; then
     . "$ENV_FILE" || { echo "error: can't read $ENV_FILE" >&2; exit 1; }
   fi
   printf '%s\n' "${!1:-}")
}
# Write one setting, quoted so the file can be sourced.
aset() {
  valid_key "$1"
  # One setting, one line: the file is edited line by line.
  [[ $2 != *$'\n'* ]] || { echo "error: $1: values can't contain newlines" >&2; return 1; }
  local v=$2 tmp
  mkdir -p "$(dirname "$ENV_FILE")"
  touch "$ENV_FILE"
  v=${v//\\/\\\\}; v=${v//\"/\\\"}; v=${v//\$/\\\$}; v=${v//\`/\\\`}
  tmp=$(mktemp)
  # grep's 1 means "no other lines"; anything above is a read error, and
  # writing the file back would lose every other setting.
  grep -v "^$1=" "$ENV_FILE" > "$tmp" || [ $? -eq 1 ] ||
    { rm -f "$tmp"; echo "error: can't read $ENV_FILE" >&2; return 1; }
  printf '%s="%s"\n' "$1" "$v" >> "$tmp"
  mv "$tmp" "$ENV_FILE"
}
# Remove one setting.
adel() {
  valid_key "$1"
  [ -f "$ENV_FILE" ] || return 0
  local tmp
  tmp=$(mktemp)
  grep -v "^$1=" "$ENV_FILE" > "$tmp" || [ $? -eq 1 ] ||
    { rm -f "$tmp"; echo "error: can't read $ENV_FILE" >&2; return 1; }
  mv "$tmp" "$ENV_FILE"
}

# One deployment stack per environment, at subscription scope (it holds the
# resource groups). Resources dropped from the template are deleted; deny
# settings stop anyone deleting a managed resource outside the stack.
STACK="usnm-$ENV_NAME"
# The region, with the same default as infra/main.bicepparam.
stack_location() { local l; l=$(aget AZURE_LOCATION) || return 1; echo "${l:-eastus2}"; }
deploy_stack() (
  # Export exactly the settings infra/main.bicepparam reads, when they have
  # a value; an unset one takes its default there. Each is read on its own
  # (aget), so the settings file never sets this script's own variables
  # (STACK, ENV_NAME, ...), and a variable left in the caller's shell can't
  # stand in for a setting the file doesn't have.
  local k v
  for k in $(grep -o "readEnvironmentVariable('[A-Za-z0-9_]*'" infra/main.bicepparam | cut -d"'" -f2); do
    unset "$k"
    v=$(aget "$k") || exit 1
    [ -z "$v" ] || export "$k=$v"
  done
  # The stack's name comes from the environment's; the template's must match.
  export AZURE_ENV_NAME=$ENV_NAME
  local location
  location=$(stack_location) || exit 1
  az stack sub create --name "$STACK" --location "$location" \
    --parameters infra/main.bicepparam \
    --action-on-unmanage deleteResources \
    --deny-settings-mode denyDelete \
    --description "usnewsmap $ENV_NAME (scripts/bootstrap.sh, scripts/provision.sh)" \
    --yes --only-show-errors -o none
)
# The template's outputs become settings (API_URL, ACR_NAME, ...). One
# query reads the names and the values from the same object, so they pair
# up in order; tsv prints them as two rows, which awk splits keeping empty
# values. A failed query fails the step, so stale settings aren't kept.
# The stack returns output names with their case changed (BACKFILL_JOB comes
# back as backfilL_JOB); main.bicep declares them all in upper case, so that's
# the name to save. Earlier runs saved the returned names: drop those.
save_outputs() {
  local out k v raw
  out=$(az stack sub show -n "$STACK" \
    --query "[keys(outputs), values(outputs)[].value]" -o tsv) || return 1
  while IFS=$'\t' read -r raw k v; do
    [ -n "$k" ] || continue
    aset "$k" "$v" || return 1
    [ "$raw" = "$k" ] || adel "$raw" || return 1
  done < <(awk -F'\t' 'NR == 1 { n = split($0, k, "\t") }
      NR == 2 { split($0, v, "\t"); for (i = 1; i <= n; i++) print k[i] "\t" toupper(k[i]) "\t" v[i] }' <<< "$out")
}
# With USNM_OCR_NODES off, the stack deletes the nodes' monitoring (their
# workspace, Application Insights component and data collection rule), and
# Azure won't delete a rule that still has associations, so every try below
# would fail. Stops first, naming them; offboard.sh deletes them.
# Returns 0 when there's nothing to check or the rule is already gone.
ocr_nodes_off_check() {
  local nodes dcr out err errf
  nodes=$(aget USNM_OCR_NODES) || return 1
  dcr=$(aget OCR_NODE_DCR_ID) || return 1
  [ "$nodes" != true ] && [ -n "$dcr" ] || return 0
  errf=$(mktemp)
  if ! out=$(az rest --method get --url "https://management.azure.com$dcr/associations?api-version=2022-06-01" \
    --query 'value[].id' -o tsv 2> "$errf"); then
    err=$(cat "$errf")
    rm -f "$errf"
    grep -q ResourceNotFound <<< "$err" && return 0
    echo "error: can't list the associations of ${dcr##*/}: $err" >&2
    return 1
  fi
  rm -f "$errf"
  [ -n "$out" ] || return 0
  echo "error: USNM_OCR_NODES is off, so the provision would delete ${dcr##*/}, which these still use:" >&2
  while IFS= read -r err; do echo "  $err"; done <<< "$out" >&2
  echo "offboard those nodes first (scripts/ocr-node/offboard.sh), or turn USNM_OCR_NODES back on" >&2
  return 1
}
# The OCR nodes' Key Vault (usnmocrkv<env><suffix>, ADR-0017) is deleted by
# the stack when USNM_OCR_NODES goes off, and a deleted vault stays
# soft-deleted, holding its name, for its retention (7 days outside prod).
# Turning the nodes back on then fails on the reserved name, so recover a
# soft-deleted vault of this environment first; the stack takes it back over
# with its secrets. Prod's has purge protection and is recovered the same way.
ocr_vault_recover() {
  local nodes rg out v
  nodes=$(aget USNM_OCR_NODES) || return 1
  [ "$nodes" = true ] || return 0
  rg=$(aget AZURE_RESOURCE_GROUP) || return 1
  [ -n "$rg" ] || return 0
  # Only a vault deleted from this environment's own group: the name prefix
  # alone could match another environment's vault (the suffix comes from the
  # full environment name), so the original vault id must name this group.
  out=$(az keyvault list-deleted --resource-type vault \
    --query "[?starts_with(name, 'usnmocrkv') && contains(to_lower(properties.vaultId), to_lower('/resourceGroups/$rg/providers/'))].name" \
    -o tsv 2>&1) ||
    { echo "error: can't list deleted Key Vaults: $out" >&2; return 1; }
  [ -n "$out" ] || return 0
  while IFS= read -r v; do
    echo "recovering soft-deleted Key Vault $v before the deployment re-creates it"
    az keyvault recover --name "$v" -o none || return 1
  done <<< "$out"
}
# dev exists only while an experiment runs (#275; docs/operations.md, "Dev
# environment lifecycle"), so its provision needs USNM_EXPERIMENT, the
# experiment's issue number. The template tags rg-usnm-<env> with it and
# with USNM_EXPERIMENT_STARTED, set here: the group's own `started` while
# its `experiment` is the same, otherwise now. teardown.sh's provision sets
# NO_EXPERIMENT_CHECK, since it only takes the environment apart.
experiment_marker() {
  local exp out tagged started=""
  [ "$ENV_NAME" = dev ] || return 0
  # Teardown's provision reads and checks nothing: a bad marker or a tag it
  # can't read mustn't stop the detach.
  [ "${NO_EXPERIMENT_CHECK:-}" = true ] && return 0
  exp=$(aget USNM_EXPERIMENT) || return 1
  if [ -z "$exp" ]; then
    echo "error: dev exists only during an experiment: first scripts/settings.sh --env dev --key USNM_EXPERIMENT --value <issue number> (or scripts/stand-up.sh)" >&2
    return 1
  fi
  [[ $exp =~ ^[0-9]+$ ]] || { echo "error: USNM_EXPERIMENT is an issue number, not \"$exp\"" >&2; return 1; }
  if out=$(az group show -n "rg-usnm-$ENV_NAME" --query "[tags.experiment, tags.started]" -o tsv 2>&1); then
    IFS=$'\t' read -r tagged started <<< "$out"
    [ "$tagged" = "$exp" ] || started=""
  elif ! grep -qiE 'ResourceGroupNotFound|could not be found' <<< "$out"; then
    echo "error: can't read the tags of rg-usnm-$ENV_NAME: $out" >&2
    return 1
  fi
  [ -n "$started" ] || started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  aset USNM_EXPERIMENT_STARTED "$started"
}
# New roles and permissions take a minute or two to apply everywhere, so a
# deployment that depends on one can fail once; it's idempotent, so retry.
# The guard-rail stack for this environment: created in its region, with
# its alert recipients for the budget on dev's groups, and dev's region when
# dev provisions (scripts/lib/guardrails.sh).
deploy_env_guardrails() {
  local location emails dev_location=""
  location=$(stack_location) || return 1
  emails=$(aget USNM_ALERT_EMAILS) || return 1
  [ "$ENV_NAME" != dev ] || dev_location=$location
  deploy_guardrails "$location" "$emails" "$dev_location"
}
provision() {
  experiment_marker || die "not provisioning $ENV_NAME"
  ocr_nodes_off_check || die "not provisioning $ENV_NAME"
  ocr_vault_recover || die "not provisioning $ENV_NAME"
  for attempt in 1 2 3; do
    deploy_env_guardrails && deploy_stack && save_outputs && return 0
    [ "$attempt" = 3 ] && die "provisioning failed three times"
    echo "retrying in 60s"
    sleep 60
  done
}
