# shellcheck shell=bash
# Shared by the environment scripts; source it with ENV_NAME set and the
# repository root as the working directory. Needs az signed in to the
# environment's subscription for the stack functions, and a die() function.
#
# An environment's settings live in three places (#295), and
# infra/env/schema.json says which setting goes where:
#
# - public choices (flags, sizes, caps, schedules) in infra/env/<env>.json,
#   committed, so a change to them goes through a pull request;
# - private ones (addresses, principals, resource ids) in the settings
#   store, one App Configuration store for every environment in the
#   subscription (scripts/settings-store.sh; its endpoint is the local
#   SETTINGS_STORE_ENDPOINT), under the environment's label, which provision
#   reads with the operator's sign-in (load_store); until it exists, they
#   stay in the local file, and --migrate then moves them;
# - everything else in the local file: the stack's outputs, the
#   subscription and tenant, and what the scripts keep (local in the
#   schema). KEY="value" lines (the layout azd used, so older environments
#   carry over) in $USNM_SETTINGS_HOME/<env>/.env (default
#   ~/.config/usnewsmap/<env>/.env) when that directory exists, so every
#   checkout reads the same file, otherwise the checkout's own
#   .azure/<env>/.env (#275).
#
# A setting in the wrong place, or in two, stops provision (settings_check).
#
# The environment name: prod, dev or x plus 1-4 lowercase letters or digits
# (ADR-0017; valid_env_name).
# shellcheck source=scripts/lib/env-name.sh
. scripts/lib/env-name.sh
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
# The committed settings, infra/env/<env>.json (USNM_ENV_DIR replaces
# infra/env in the tests). scripts/lib/env_settings.py checks the file
# against the schema and prints SETTINGS_PUBLIC, SETTINGS_PRIVATE and
# SETTINGS_LOCAL (the schema's settings by where they live) and
# COMMITTED_SETTINGS (KEY="value" lines). A file that doesn't check out
# stops every script up front, like a local file that doesn't parse.
SETTINGS_DIR=${USNM_ENV_DIR:-infra/env}
COMMITTED_FILE="$SETTINGS_DIR/$ENV_NAME.json"
SETTINGS_PUBLIC="" SETTINGS_PRIVATE="" COMMITTED_SETTINGS=""
load_committed() {
  local out
  out=$(python3 scripts/lib/env_settings.py --env "$ENV_NAME" --shell) ||
    { echo "error: fix $COMMITTED_FILE (or $SETTINGS_DIR/schema.json) before running anything" >&2; return 1; }
  eval "$out"
}
load_committed || exit 1
# The settings store's private settings for this environment as
# KEY="value" lines, once load_store has read them (STORE_LOADED), and their
# names. The store's own stack (infra/settings/main.bicep).
STORE_SETTINGS="" STORE_KEYS="" STORE_LOADED=false
SETTINGS_STACK=usnm-config
in_list() { [[ " $2 " == *" $1 "* ]]; }
is_public() { in_list "$1" "$SETTINGS_PUBLIC"; }
is_private() { in_list "$1" "$SETTINGS_PRIVATE"; }
# A value quoted for a KEY="value" line, which is sourced.
quote_value() {
  local v=$1
  v=${v//\\/\\\\}; v=${v//\"/\\\"}; v=${v//\$/\\\$}; v=${v//\`/\\\`}
  printf '%s' "$v"
}
# A VNet address space: an IPv4 network in CIDR with no host bits, /26 or
# larger, inside an RFC 1918 block. The blocks are built from their numbers:
# private addresses aren't written out (scripts/ci/no-private-addresses.sh).
valid_vnet_prefix() {
  python3 - "$1" << 'PY' || { echo "error: \"$1\" isn't a VNet address space: an IPv4 network in CIDR (no host bits), /26 or larger, in an RFC 1918 block" >&2; return 1; }
import ipaddress, sys
try:
    n = ipaddress.IPv4Network(sys.argv[1])
except ValueError:
    sys.exit(1)
blocks = [ipaddress.IPv4Network((10 << 24, 8)),
          ipaddress.IPv4Network(((172 << 24) | (16 << 16), 12)),
          ipaddress.IPv4Network(((192 << 24) | (168 << 16), 16))]
sys.exit(0 if n.prefixlen <= 26 and any(n.subnet_of(b) for b in blocks) else 1)
PY
}
# One setting from the local file (empty when unset).
# shellcheck source=/dev/null
# Only the file counts: the caller's variable of the same name is cleared.
# A settings file that can't be read or parsed fails the read.
local_get() {
  valid_key "$1"
  (set +u; unset "$1"; set -a
   if [ -f "$ENV_FILE" ]; then
     . "$ENV_FILE" || { echo "error: can't read $ENV_FILE" >&2; exit 1; }
   fi
   printf '%s\n' "${!1:-}")
}
# The names the local file sets, one per line.
local_keys() {
  [ -f "$ENV_FILE" ] || return 0
  { grep -oE '^[A-Za-z_][A-Za-z0-9_]*=' "$ENV_FILE" || [ $? -eq 1 ]; } | tr -d =
}
# Whether the local file sets $1, even to "".
local_has() { local_keys | grep -qx "$1"; }
committed_get() {
  (set +u; unset "$1"; eval "$COMMITTED_SETTINGS"; printf '%s\n' "${!1:-}")
}
# Read one setting (empty when unset), wherever it lives: a public one from
# the committed file; a private one from the local file, else the store (a
# local entry wins, even "": teardown clears USNM_ARCHIVE_ACCOUNT that way);
# anything else from the local file. A public setting still only in the
# local file (an environment from before #295) is read from there with a
# warning, so the scripts keep working until it moves; provision refuses.
aget() {
  valid_key "$1"
  local v
  if is_public "$1"; then
    v=$(committed_get "$1")
    if [ -z "$v" ] && local_has "$1"; then
      v=$(local_get "$1") || return 1
      [ -z "$v" ] ||
        echo "warning: $1 is in $ENV_FILE; it belongs in $COMMITTED_FILE: scripts/settings.sh --env $ENV_NAME --migrate" >&2
    fi
    printf '%s\n' "$v"
    return 0
  fi
  if is_private "$1" && ! local_has "$1"; then
    if $STORE_LOADED; then
      (set +u; unset "$1"; eval "$STORE_SETTINGS"; printf '%s\n' "${!1:-}")
    else
      store_get "$1"
    fi
    return
  fi
  local_get "$1"
}
# One private setting from the settings store, when load_store hasn't read
# them all (the scripts but provision): empty when there's no store or no
# such key, a failure when the store can't be read, so a script never
# takes an unreadable setting for an unset one. A list of one key reads as
# empty when it's missing, where `kv show` fails.
store_get() {
  local endpoint out
  endpoint=$(settings_store_endpoint) || return 1
  [ -n "$endpoint" ] || { echo; return 0; }
  if ! out=$(az appconfig kv list --endpoint "$endpoint" --auth-mode login --label "$ENV_NAME" --key "$1" --query "[0].value" -o tsv 2>&1); then
    echo "error: can't read $1 from the settings store $endpoint: $out" >&2
    store_access_hint
    return 1
  fi
  printf '%s\n' "$out"
}
# How an operator without access to the settings store gets it, after a
# refused read or write. App Configuration Data Owner goes to the operators
# in the store's admin list (its stack's parameter) and to the account that
# deployed it last, so another operator needs one of them to add their
# object id to the list; or, as an Owner of the subscription, a grant of
# their own on the store's group (one on the store would collide with the
# assignment the stack then makes), removed once the list has them.
store_access_hint() {
  cat >&2 << EOF
The settings store gives App Configuration Data Owner to the operators in its admin list and to whoever deployed it last. Either:
  - ask one of them to add your object id (az ad signed-in-user show --query id -o tsv) to that list, comma separated:
      scripts/settings-store.sh --deploy --admins "<ids>"
  - or, as an Owner of the subscription, grant it to yourself on usnm-config-rg, add yourself to the list as above, then delete the grant (a new role takes a few minutes to apply):
      az role assignment create --assignee-object-id "\$(az ad signed-in-user show --query id -o tsv)" --assignee-principal-type User --role "App Configuration Data Owner" --scope "/subscriptions/\$(az account show --query id -o tsv)/resourceGroups/usnm-config-rg"
EOF
}
# Write one setting: a public one to the committed file (and out of the
# local one, where an environment from before #295 kept it); a private one,
# once the settings store exists, to the store (and out of the local file,
# so a script that sets it again, like bootstrap.sh, never leaves a second
# copy); anything else to the local file, quoted so it can be sourced. An
# empty value removes a public setting, which then takes its default, as an
# empty one always has; an empty private one is a local "", which shadows
# the store (teardown.sh detaches the archive that way).
aset() {
  valid_key "$1"
  # One setting, one line: the file is edited line by line.
  [[ $2 != *$'\n'* ]] || { echo "error: $1: values can't contain newlines" >&2; return 1; }
  if is_public "$1"; then
    python3 scripts/lib/env_settings.py --env "$ENV_NAME" --set "$1" --value "$2" || return 1
    load_committed || return 1
    local_del "$1"
    return
  fi
  if is_private "$1" && [ -n "$2" ]; then
    local endpoint
    endpoint=$(settings_store_endpoint) || return 1
    if [ -n "$endpoint" ]; then
      store_set "$endpoint" "$1" "$2" || return 1
      local_del "$1"
      return
    fi
  fi
  local_set "$1" "$2"
}
# One private setting into the settings store at $1, under this
# environment's label, with the operator's sign-in (Entra ID, App
# Configuration Data Owner), unless it already holds that value.
store_set() {
  local cur out
  if $STORE_LOADED; then
    cur=$(set +u; unset "$2"; eval "$STORE_SETTINGS"; printf '%s\n' "${!2:-}")
  else
    cur=$(store_get "$2") || return 1
  fi
  [ "$cur" != "$3" ] || return 0
  if ! out=$(az appconfig kv set --endpoint "$1" --auth-mode login --label "$ENV_NAME" --key "$2" --value "$3" --yes -o none 2>&1); then
    echo "error: can't set $2 in the settings store $1: $out" >&2
    store_access_hint
    return 1
  fi
  echo "set $2 for $ENV_NAME in the settings store $1"
  # Read from the store from now on, not from what load_store read.
  STORE_LOADED=false
}
# One setting into the local file, quoted so it can be sourced.
local_set() {
  local v tmp
  mkdir -p "$(dirname "$ENV_FILE")"
  touch "$ENV_FILE"
  v=$(quote_value "$2")
  tmp=$(mktemp)
  # grep's 1 means "no other lines"; anything above is a read error, and
  # writing the file back would lose every other setting.
  grep -v "^$1=" "$ENV_FILE" > "$tmp" || [ $? -eq 1 ] ||
    { rm -f "$tmp"; echo "error: can't read $ENV_FILE" >&2; return 1; }
  printf '%s="%s"\n' "$1" "$v" >> "$tmp"
  mv "$tmp" "$ENV_FILE"
}
# Remove one setting, so it takes its default: a public one from the
# committed file (and the local one); a private one from the settings
# store, once it exists, and from the local file; anything else from the
# local file. local_del removes only the local entry (a private setting's
# store value then applies).
adel() {
  valid_key "$1"
  if is_public "$1"; then
    python3 scripts/lib/env_settings.py --env "$ENV_NAME" --unset "$1" || return 1
    load_committed || return 1
  elif is_private "$1"; then
    local endpoint
    endpoint=$(settings_store_endpoint) || return 1
    [ -z "$endpoint" ] || store_delete "$endpoint" "$1" || return 1
  fi
  local_del "$1"
}
# Delete one private setting from the settings store at $1 (nothing to do
# when it has none: the delete matches no key). The Free tier keeps no
# deleted keys, so setting it again is a plain set.
store_delete() {
  local out
  if ! out=$(az appconfig kv delete --endpoint "$1" --auth-mode login --label "$ENV_NAME" --key "$2" --yes -o none 2>&1); then
    grep -qiE 'no matching|not found|does not exist' <<< "$out" && return 0
    echo "error: can't delete $2 from the settings store $1: $out" >&2
    store_access_hint
    return 1
  fi
  echo "deleted $2 for $ENV_NAME from the settings store $1"
  STORE_LOADED=false
}
local_del() {
  [ -f "$ENV_FILE" ] || return 0
  local tmp
  tmp=$(mktemp)
  grep -v "^$1=" "$ENV_FILE" > "$tmp" || [ $? -eq 1 ] ||
    { rm -f "$tmp"; echo "error: can't read $ENV_FILE" >&2; return 1; }
  mv "$tmp" "$ENV_FILE"
}
# Read this environment's settings from the store into STORE_SETTINGS, in
# one request (a page holds 100; the Free tier allows 1,000 a day), with
# the operator's sign-in. Before the store exists there's nothing to read.
# A key under this label that isn't a private setting stops here, like an
# unknown key in a committed file.
load_store() {
  local endpoint out lines
  STORE_SETTINGS="" STORE_KEYS=""
  endpoint=$(settings_store_endpoint) || return 1
  [ -n "$endpoint" ] || { STORE_LOADED=true; return 0; }
  out=$(az appconfig kv list --endpoint "$endpoint" --auth-mode login --label "$ENV_NAME" --all --query "[].{key: key, value: value}" -o json) ||
    { echo "error: can't list $ENV_NAME's settings in the settings store $endpoint" >&2; store_access_hint; return 1; }
  # KEY="value" lines, quoted as quote_value does, then the keys.
  lines=$(python3 -c 'import json, sys
private = set(sys.argv[2].split())
for kv in json.loads(sys.argv[1] or "[]"):
    k, v = kv.get("key") or "", kv.get("value") or ""
    if k not in private:
        sys.exit(f"error: the settings store holds {k} for {sys.argv[3]}, which isn'"'"'t a private setting (infra/env/schema.json)")
    if "\n" in v:
        sys.exit(f"error: the settings store'"'"'s {k} for {sys.argv[3]} has more than one line")
    for a, b in (("\\", "\\\\"), ("\"", "\\\""), ("$", "\\$"), ("`", "\\`")):
        v = v.replace(a, b)
    print(f"{k}=\"{v}\"")' "$out" "$SETTINGS_PRIVATE" "$ENV_NAME") || return 1
  STORE_SETTINGS=$lines
  [ -z "$lines" ] || STORE_KEYS=$(cut -d= -f1 <<< "$lines" | tr '\n' ' ')
  STORE_LOADED=true
}
# The settings store's endpoint: the local file's SETTINGS_STORE_ENDPOINT,
# or, when the local file lacks it (another operator's machine, a new
# checkout), the store stack's output, which is then recorded there. Empty
# before the store exists. Every read and write of a private setting goes
# through it, so none takes a store it can't name for no store.
# SETTINGS_READ_ONLY (the what-if) records nothing.
SETTINGS_READ_ONLY=false
settings_store_endpoint() {
  local endpoint
  endpoint=$(local_get SETTINGS_STORE_ENDPOINT) || return 1
  if [ -z "$endpoint" ]; then
    endpoint=$(stack_settings_store) || return 1
    [ -z "$endpoint" ] || $SETTINGS_READ_ONLY || local_set SETTINGS_STORE_ENDPOINT "$endpoint" || return 1
  fi
  printf '%s\n' "$endpoint"
}
# The settings store's endpoint from its stack's outputs
# (SETTINGS_STORE_ENDPOINT, whose case the stack may change), in the
# environment's recorded subscription, or the selected one before bootstrap
# records it: empty without the stack or that output.
stack_settings_store() {
  local out err errf sub
  local scope=()
  sub=$(local_get AZURE_SUBSCRIPTION_ID) || return 1
  [ -z "$sub" ] || scope=(--subscription "$sub")
  errf=$(mktemp)
  if ! out=$(az stack sub show -n "$SETTINGS_STACK" --query outputs -o json ${scope[@]+"${scope[@]}"} 2> "$errf"); then
    err=$(cat "$errf")
    rm -f "$errf"
    # az says ResourceNotFound for a stack that doesn't exist.
    grep -qE 'DeploymentStackNotFound|ResourceNotFound' <<< "$err" && return 0
    echo "error: can't read the stack $SETTINGS_STACK's outputs: $err" >&2
    return 1
  fi
  rm -f "$errf"
  python3 -c 'import json, sys
for k, v in (json.loads(sys.argv[1] or "{}") or {}).items():
    if k.upper() == "SETTINGS_STORE_ENDPOINT":
        print(v.get("value") or "")' "$out"
}
# Moving an environment to another subscription (bootstrap.sh --move)
# rebuilds it there, with that subscription's settings store: the old
# store's private settings for this environment come back to the local
# file, where they stay until the new store exists (then --migrate moves
# them), and the old store is forgotten. A setting the local file already
# has is kept. The old store keeps its copy.
store_detach() {
  local k v endpoint
  endpoint=$(settings_store_endpoint) || return 1
  [ -n "$endpoint" ] || return 0
  load_store || return 1
  for k in $STORE_KEYS; do
    local_has "$k" && continue
    v=$(aget "$k") || return 1
    local_set "$k" "$v" || return 1
  done
  local_del SETTINGS_STORE_ENDPOINT || return 1
  STORE_SETTINGS="" STORE_KEYS="" STORE_LOADED=false
  echo "$ENV_NAME's private settings from $endpoint are in $ENV_FILE until the new subscription's settings store exists"
}
# store_detach for a move from subscription $1 to $2. The old store's
# endpoint comes from the local file or, when that lacks it (another
# operator's machine), from the old subscription's store stack, so the old
# subscription is selected first (in another tenant, az must be signed in
# there too; az login keeps the other sign-ins), and az takes a token in its
# tenant; then $2 is selected again, whatever happened. A local
# SETTINGS_STORE_ENDPOINT="" moves without the old store's settings (for an
# old subscription that's gone).
store_detach_from() {
  local old=$1 target=$2 tenant rc=0
  if local_has SETTINGS_STORE_ENDPOINT && [ -z "$(local_get SETTINGS_STORE_ENDPOINT)" ]; then
    echo "SETTINGS_STORE_ENDPOINT is \"\" in $ENV_FILE: moving without the old settings store's private settings"
    local_del SETTINGS_STORE_ENDPOINT
    return
  fi
  tenant=$(local_get AZURE_TENANT_ID) || return 1
  if ! az account set --subscription "$old" 2> /dev/null; then
    echo "error: az can't select the old subscription $old, whose settings store holds the private settings: az login --tenant ${tenant:-<its tenant>} (az keeps your other sign-ins), then run this again with --subscription $target" >&2
    return 1
  fi
  store_detach || rc=1
  az account set --subscription "$target" || return 1
  return $rc
}
# Each setting in its place, before a deployment reads them: a public one
# only in the committed file, a private one in the store or the local file
# but not both. A private one still in the local file once the store exists
# is a warning, with the command that moves it. load_store first.
settings_check() {
  local k errors="" moves=""
  for k in $(local_keys); do
    if is_public "$k"; then
      errors+="  $k is in $ENV_FILE; public settings are committed, in $COMMITTED_FILE"$'\n'
    elif is_private "$k" && [ -n "$(local_get "$k")" ]; then
      if in_list "$k" "$STORE_KEYS"; then
        errors+="  $k is in both $ENV_FILE and the settings store; keep the store's: scripts/settings.sh --env $ENV_NAME --key $k --unset-local; or the local one: scripts/settings.sh --env $ENV_NAME --key $k --value \"\$(scripts/settings.sh --env $ENV_NAME --key $k)\""$'\n'
      elif [ -n "$(local_get SETTINGS_STORE_ENDPOINT)" ]; then
        moves+=" $k"
      fi
    fi
  done
  if [ -n "$moves" ]; then
    echo "warning: private settings still in $ENV_FILE:$moves; move them to the settings store: scripts/settings.sh --env $ENV_NAME --migrate" >&2
  fi
  [ -n "$errors" ] || return 0
  printf 'error: settings in the wrong place:\n%s' "$errors" >&2
  echo "scripts/settings.sh --env $ENV_NAME --migrate moves the public ones into $COMMITTED_FILE, to commit" >&2
  return 1
}
# Move settings to where they belong. First every public setting in the
# local file into the committed file, unless the committed file already has
# another value (then nothing moves). Then, once the settings store exists,
# every private one with a value in the local file into the store, under
# this environment's label, unless the store already has another value
# (then no private one moves); until the store exists they stay local. An
# empty local private one ("") stays: it shadows the store on purpose.
migrate_settings() {
  local k v c conflicts="" moved=0 endpoint
  for k in $(local_keys); do
    is_public "$k" || continue
    v=$(local_get "$k") || return 1
    c=$(committed_get "$k")
    if [ -n "$v" ] && [ -n "$c" ] && [ "$v" != "$c" ]; then
      conflicts+="  $k: $COMMITTED_FILE has \"$c\", $ENV_FILE \"$v\""$'\n'
    fi
  done
  if [ -n "$conflicts" ]; then
    printf 'error: these differ; settle each in one place first (scripts/settings.sh --env %s --key K --value V):\n%s' "$ENV_NAME" "$conflicts" >&2
    return 1
  fi
  for k in $(local_keys); do
    is_public "$k" || continue
    v=$(local_get "$k") || return 1
    c=$(committed_get "$k")
    if [ -n "$v" ] && [ -z "$c" ]; then
      python3 scripts/lib/env_settings.py --env "$ENV_NAME" --set "$k" --value "$v" || return 1
      load_committed || return 1
    fi
    local_del "$k" || return 1
    moved=$((moved + 1))
  done
  echo "moved $moved public settings from $ENV_FILE to $COMMITTED_FILE"
  # The settings vault this replaced (#295) gave its role to a list of its
  # own; the store's list is its stack's (scripts/settings-store.sh).
  if local_has USNM_SETTINGS_VAULT_ADMINS; then
    echo "note: USNM_SETTINGS_VAULT_ADMINS in $ENV_FILE is now the settings store's admin list: pass its value to scripts/settings-store.sh --deploy --admins, then remove it from $ENV_FILE"
  fi
  endpoint=$(settings_store_endpoint) || return 1
  if [ -z "$endpoint" ]; then
    for k in $(local_keys); do
      is_private "$k" && echo "private, stays in $ENV_FILE until the settings store exists: $k"
    done
    return 0
  fi
  load_store || return 1
  for k in $(local_keys); do
    if ! is_private "$k" || ! in_list "$k" "$STORE_KEYS"; then continue; fi
    v=$(local_get "$k") || return 1
    [ -n "$v" ] || continue
    c=$(set +u; unset "$k"; eval "$STORE_SETTINGS"; printf '%s\n' "${!k:-}")
    [ "$v" = "$c" ] || conflicts+="  $k"$'\n'
  done
  if [ -n "$conflicts" ]; then
    printf 'error: these differ between %s and the settings store; keep one of each (scripts/settings.sh --env %s --key K --unset-local keeps the store'"'"'s):\n%s' "$ENV_FILE" "$ENV_NAME" "$conflicts" >&2
    return 1
  fi
  moved=0
  for k in $(local_keys); do
    is_private "$k" || continue
    v=$(local_get "$k") || return 1
    [ -n "$v" ] || { echo "kept $k=\"\" in $ENV_FILE: it hides the settings store's value (--unset clears it)"; continue; }
    store_set "$endpoint" "$k" "$v" > /dev/null || return 1
    local_del "$k" || return 1
    moved=$((moved + 1))
  done
  echo "moved $moved private settings from $ENV_FILE to the settings store $endpoint (label $ENV_NAME)"
}
# Whether the committed file (and the schema) match the checkout's commit:
# scripts/provision.sh deploys only committed settings, unless --allow-dirty.
committed_clean() {
  local dir out
  dir=$(dirname "$COMMITTED_FILE")
  out=$(git -C "$dir" status --porcelain -- "$(basename "$COMMITTED_FILE")" schema.json 2>&1) ||
    { echo "error: can't check $COMMITTED_FILE with git: $out" >&2; return 2; }
  [ -z "$out" ] || { printf '%s\n' "$out" >&2; return 1; }
}

# The environment's resource groups by name (ADR-0017; infra/naming.bicep's
# rg, containerEnvInfraRg and spotRg), in both forms: dev and experiments
# use ADR-0017's, and the current prod keeps its legacy names until its
# rebuild. The project group, which carries the experiment tag, comes first
# in each form. A legacy Container Apps environment's own group is Azure's
# ME_<environment>_<group>_<region>, which starts with LEGACY_INFRA_GROUP.
PROJECT_GROUPS="usnm-core-rg-$ENV_NAME rg-usnm-$ENV_NAME"
# shellcheck disable=SC2034 # used by scripts/dev-status.sh
ENV_GROUPS="$PROJECT_GROUPS usnm-core-infra-rg-$ENV_NAME rg-usnm-$ENV_NAME-spot"
# shellcheck disable=SC2034 # used by scripts/dev-status.sh
LEGACY_INFRA_GROUP="ME_cae-usnm-${ENV_NAME}_"
# One deployment stack per environment, at subscription scope (it holds the
# resource groups). Resources dropped from the template are deleted; deny
# settings stop anyone deleting a managed resource outside the stack.
STACK="usnm-$ENV_NAME"
# The region, with the same default as infra/main.bicepparam.
stack_location() { local l; l=$(aget AZURE_LOCATION) || return 1; echo "${l:-eastus2}"; }
# Export exactly the settings infra/main.bicepparam reads, when they have
# a value; an unset one takes its default there. Each is read on its own
# (aget), so the settings file never sets this script's own variables
# (STACK, ENV_NAME, ...), and a variable left in the caller's shell can't
# stand in for a setting the file doesn't have. infra/policy-exemptions.bicepparam
# reads a few of the same. Call it in a subshell: it exports.
export_stack_settings() {
  local k v
  for k in $(grep -o "readEnvironmentVariable('[A-Za-z0-9_]*'" infra/main.bicepparam | cut -d"'" -f2); do
    unset "$k"
    v=$(aget "$k") || return 1
    [ -z "$v" ] || export "$k=$v"
  done
  # The stack's name comes from the environment's; the template's must match.
  export AZURE_ENV_NAME=$ENV_NAME
}
# The public-network policy's assignment, written on its own before the
# stack (#336): preflight checks the stack against the assignment as it is
# then, so an account the stack both opens and exempts would be refused.
# infra/policy-exemptions.bicep writes the same publicAccountIds the stack
# then writes (infra/public-accounts.bicep). Nothing to do before the first
# provision, when no assignment exists yet to refuse anything.
POLICY_EXEMPTIONS=usnm-audit-data-public-network
# The project group that holds the assignment, in either name form; nothing
# when none does.
policy_exemptions_group() {
  local g out sub
  sub=$(aget AZURE_SUBSCRIPTION_ID) || return 1
  for g in $PROJECT_GROUPS; do
    if out=$(az policy assignment show --name "$POLICY_EXEMPTIONS" \
      --scope "/subscriptions/$sub/resourceGroups/$g" --query name -o tsv 2>&1); then
      echo "$g"
      return 0
    elif ! grep -qiE 'NotFound|could not be found|does not exist' <<< "$out"; then
      echo "error: can't read the policy assignment $POLICY_EXEMPTIONS in $g: $out" >&2
      return 1
    fi
  done
}
# deploy_policy_exemptions [--what-if]: write the assignment's next
# publicAccountIds, or with --what-if show the change and, on stdout, the
# ids it adds (one per line), writing nothing.
deploy_policy_exemptions() (
  local g current
  export_stack_settings || exit 1
  g=$(policy_exemptions_group) || exit 1
  if [ -z "$g" ]; then
    echo "no policy assignment $POLICY_EXEMPTIONS yet: the stack creates it" >&2
    exit 0
  fi
  local args=(--resource-group "$g" --name "usnm-$ENV_NAME-policy-exemptions"
    --parameters infra/policy-exemptions.bicepparam --only-show-errors)
  if [ "${1:-}" = --what-if ]; then
    current=$(az policy assignment show --name "$POLICY_EXEMPTIONS" \
      --scope "/subscriptions/$(aget AZURE_SUBSCRIPTION_ID)/resourceGroups/$g" \
      --query "parameters.publicAccountIds.value" -o json) || exit 1
    local out
    az deployment group what-if "${args[@]}" >&2 || exit 1
    out=$(az deployment group what-if "${args[@]}" --no-pretty-print -o json) || exit 1
    # The ids the next value has and the current one doesn't.
    python3 -c '
import json, sys
current = {i.lower() for i in json.loads(sys.argv[1] or "[]")}
for c in json.loads(sys.stdin.read()).get("changes", []):
    after = (c.get("after") or {}).get("properties", {}).get("parameters", {})
    for i in after.get("publicAccountIds", {}).get("value", []):
        if i.lower() not in current:
            print(i)
' "$current" <<< "$out"
    exit
  fi
  echo "writing the exemptions of $POLICY_EXEMPTIONS in $g (#336)"
  az deployment group create "${args[@]}" -o none
)
# deploy_stack [--what-if [--exempting IDS]]: create or update the
# environment's stack, or with --what-if validate it and show its changes,
# deploying nothing. IDS (newline separated) are the resources the policy
# step (deploy_policy_exemptions) is about to exempt: a what-if can't write
# that step first, so preflight refuses them, and those refusals alone are
# expected.
deploy_stack() (
  export_stack_settings || exit 1
  # The VNet's address space has no default anywhere: private addresses
  # aren't committed (scripts/ci/no-private-addresses.sh).
  [ -n "${USNM_VNET_ADDRESS_PREFIX:-}" ] || {
    echo "error: USNM_VNET_ADDRESS_PREFIX isn't set for $ENV_NAME: the VNet's address space, a private IPv4 block in CIDR, /26 or larger. Set it with scripts/settings.sh --env $ENV_NAME --key USNM_VNET_ADDRESS_PREFIX --value <cidr> (an existing environment: its VNet's current block, az network vnet show)" >&2
    exit 1
  }
  valid_vnet_prefix "$USNM_VNET_ADDRESS_PREFIX" || exit 1
  [ -z "$EXPERIMENT_STARTED_DRY_RUN" ] || export USNM_EXPERIMENT_STARTED=$EXPERIMENT_STARTED_DRY_RUN
  local location
  location=$(stack_location) || exit 1
  local stack_args=(--name "$STACK" --location "$location" --parameters infra/main.bicepparam
    --action-on-unmanage deleteResources --deny-settings-mode denyDelete
    --description "usnewsmap $ENV_NAME (scripts/bootstrap.sh, scripts/provision.sh)")
  if [ "${1:-}" = --what-if ]; then
    # Azure's preflight validation of the stack, as a create runs it first
    # (policy denials included), then the template's changes: stacks have no
    # what-if, a deployment's shows the same template's.
    local exempting="" err what_if_args=()
    [ "${2:-}" != --exempting ] || exempting=${3:-}
    echo "validating stack $STACK (preflight)"
    if ! err=$(az stack sub validate "${stack_args[@]}" --only-show-errors -o none 2>&1); then
      only_pending_refusals "$err" "$exempting" || { echo "$err" >&2; exit 1; }
      echo "preflight refuses only what the policy step exempts first (#336), which a provision writes before the stack:"
      while IFS= read -r err; do echo "  $err"; done <<< "$exempting"
      # The what-if's own preflight would refuse the same resources, so it
      # checks the template alone; the validation above checked the rest.
      what_if_args=(--validation-level Template)
    fi
    echo "stack $STACK passes preflight; its changes (a deployment's what-if: it doesn't list what the stack deletes because the template dropped it):"
    az deployment sub what-if --location "$location" --parameters infra/main.bicepparam ${what_if_args[@]+"${what_if_args[@]}"} --only-show-errors
    exit
  fi
  az stack sub create "${stack_args[@]}" --yes --only-show-errors -o none
)
# only_pending_refusals ERR IDS: whether ERR, a failed preflight, is
# policy refusals alone (RequestDisallowedByPolicy), each of a resource
# named in IDS, the ids the policy step is about to exempt.
only_pending_refusals() {
  [ -n "$2" ] || return 1
  python3 -c '
import re, sys
err, ids = sys.argv[1], sys.argv[2].split()
names = {i.rstrip("/").rsplit("/", 1)[-1].lower() for i in ids}
refused = re.findall(r"Resource \x27([^\x27]+)\x27 was disallowed by policy", err)
codes = (set(re.findall(r"\"code\":\s*\"([A-Za-z]+)\"", err)) | set(re.findall(r"\(([A-Za-z]+)\)", err))
         | set(re.findall(r"^(?:ERROR: )?([A-Za-z]+) - ", err, re.M)))
wrappers = {"InvalidTemplateDeployment", "DeploymentStackValidationFailed", "RequestDisallowedByPolicy"}
ok = refused and all(r.lower() in names for r in refused) and codes <= wrappers
sys.exit(0 if ok else 1)
' "$1" "$2"
}
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
# A deleted Key Vault stays soft-deleted, holding its name, for its
# retention (7 days outside prod), and the stack can't create one under a
# reserved name, so recover a soft-deleted vault of this environment first;
# the stack takes it back over with its secrets. The OCR nodes' vault
# (usnmocrkv<env><suffix>, ADR-0017) goes when USNM_OCR_NODES goes off, so
# it's recovered when they're on (prod's has purge protection and is
# recovered the same way). It's in the environment's project group, in
# either of its name forms (PROJECT_GROUPS; infra/naming.bicep's rg:
# usnm-core-rg-<env>, or the current prod's legacy rg-usnm-<env>), so this
# doesn't need the local file's AZURE_RESOURCE_GROUP: a rebuild after a
# teardown starts from a local file without it. A vault is recovered into
# its old group, the one its deleted vault id names, so that group, if the
# teardown deleted it, is created first, as the stack would (the stack then
# takes it over, with its tags).
vaults_recover() {
  local nodes out v id lid g rg exists location in_groups="" checked=" "
  nodes=$(aget USNM_OCR_NODES) || return 1
  [ "$nodes" = true ] || return 0
  # Only a vault deleted from this environment's own group: the name prefix
  # alone could match another environment's vault (the suffix comes from the
  # full environment name), so the original vault id must name this group.
  for g in $PROJECT_GROUPS; do
    in_groups="${in_groups:+$in_groups || }contains(to_lower(properties.vaultId), to_lower('/resourceGroups/$g/providers/'))"
  done
  out=$(az keyvault list-deleted --resource-type vault \
    --query "[?starts_with(name, 'usnmocrkv') && ($in_groups)].[name, properties.vaultId]" -o tsv 2>&1) ||
    { echo "error: can't list deleted Key Vaults: $out" >&2; return 1; }
  [ -n "$out" ] || return 0
  while IFS=$'\t' read -r v id; do
    [ -n "$v" ] || continue
    rg=""
    # Case-insensitive, without bash 4's ${x,,} (macOS has bash 3.2).
    lid=$(tr '[:upper:]' '[:lower:]' <<< "$id")
    for g in $PROJECT_GROUPS; do
      if [[ $lid == *"/resourcegroups/$(tr '[:upper:]' '[:lower:]' <<< "$g")/providers/"* ]]; then rg=$g; break; fi
    done
    [ -n "$rg" ] || { echo "error: can't tell the resource group of the deleted Key Vault $v from its id $id" >&2; return 1; }
    if [[ $checked != *" $rg "* ]]; then
      exists=$(az group exists -n "$rg" -o tsv) || { echo "error: can't check whether $rg exists" >&2; return 1; }
      if [ "$exists" != true ]; then
        location=$(stack_location) || return 1
        echo "creating $rg to recover its soft-deleted Key Vaults into"
        az group create -n "$rg" -l "$location" --tags project=usnewsmap "environment=$ENV_NAME" -o none || return 1
      fi
      checked="$checked$rg "
    fi
    echo "recovering soft-deleted Key Vault $v into $rg before the deployment re-creates it"
    az keyvault recover --name "$v" -o none || return 1
  done <<< "$out"
}
# The settings store is shared by every environment in the subscription and
# outside their stacks (infra/settings/main.bicep): the first provision in a
# subscription creates it, in the environment's region. Its private
# settings then move in with scripts/settings.sh --migrate (settings_check
# says so). A local SETTINGS_KEYVAULT, the name of the settings vault the
# store replaced (#295), goes, unless that vault still exists: its secrets
# would be lost with it (the stack no longer declares it), so the provision
# stops until they're copied.
settings_store_script() { scripts/settings-store.sh "$@"; }
# Whether settings_store_ensure just created the store, so its new role may
# not apply yet (load_store_after_create).
STORE_CREATED=false
settings_store_ensure() {
  local endpoint location vault out
  vault=$(local_get SETTINGS_KEYVAULT) || return 1
  if [ -n "$vault" ]; then
    if out=$(az keyvault show -n "$vault" --query name -o tsv 2>&1); then
      echo "error: the settings vault $vault still exists, and this provision would delete it with its secrets: copy each to the settings store first (scripts/settings.sh --env $ENV_NAME --key KEY --value \"\$(az keyvault secret show --vault-name $vault --name KEY-WITH-HYPHENS --query value -o tsv)\"), then remove SETTINGS_KEYVAULT from $ENV_FILE" >&2
      return 1
    fi
    grep -qiE 'NotFound|not found' <<< "$out" || { echo "error: can't check the settings vault $vault: $out" >&2; return 1; }
    local_del SETTINGS_KEYVAULT || return 1
  fi
  endpoint=$(settings_store_endpoint) || return 1
  [ -z "$endpoint" ] || return 0
  location=$(stack_location) || return 1
  echo "creating the settings store (stack $SETTINGS_STACK), shared by every environment in this subscription"
  settings_store_script --deploy --location "$location" || return 1
  endpoint=$(settings_store_endpoint) || return 1
  [ -n "$endpoint" ] || { echo "error: the settings store's stack $SETTINGS_STACK names no endpoint" >&2; return 1; }
  STORE_CREATED=true
}
# load_store, which right after settings_store_ensure created the store can
# be refused while the new App Configuration Data Owner assignment
# propagates (up to about 15 minutes): then it's tried once a minute for 15
# minutes. On an existing store a refusal fails at once.
STORE_RETRY_SECS=${STORE_RETRY_SECS:-60}
load_store_after_create() {
  local try
  $STORE_CREATED || { load_store; return; }
  for try in $(seq 1 15); do
    load_store 2> /dev/null && return 0
    echo "the settings store's new role may not apply yet; trying again in ${STORE_RETRY_SECS}s ($try of 15)"
    sleep "$STORE_RETRY_SECS"
  done
  load_store
}
# dev exists only while an experiment runs (#275; docs/operations.md, "Dev
# environment lifecycle"), so its provision needs USNM_EXPERIMENT, the
# experiment's issue number. The template tags the project group
# (usnm-core-rg-<env>) with it and with USNM_EXPERIMENT_STARTED, set here:
# the group's own `started` while its `experiment` is the same, otherwise
# now. teardown.sh's provision sets NO_EXPERIMENT_CHECK, since it only takes
# the environment apart.
# experiment_marker [--dry-run]: --dry-run (the what-if) keeps the start in
# EXPERIMENT_STARTED_DRY_RUN, which deploy_stack exports, and writes
# nothing.
EXPERIMENT_STARTED_DRY_RUN=""
experiment_marker() {
  local exp out g tagged started=""
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
  # The project group in either name form: the first one that exists.
  for g in $PROJECT_GROUPS; do
    if out=$(az group show -n "$g" --query "[tags.experiment, tags.started]" -o tsv 2>&1); then
      IFS=$'\t' read -r tagged started <<< "$out"
      [ "$tagged" = "$exp" ] || started=""
      break
    elif ! grep -qiE 'ResourceGroupNotFound|could not be found' <<< "$out"; then
      echo "error: can't read the tags of $g: $out" >&2
      return 1
    fi
  done
  [ -n "$started" ] || started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  if [ "${1:-}" = --dry-run ]; then EXPERIMENT_STARTED_DRY_RUN=$started; return 0; fi
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
  # A soft-deleted vault can't be taken over: recover it first.
  vaults_recover || die "not provisioning $ENV_NAME"
  settings_store_ensure || die "not provisioning $ENV_NAME"
  load_store_after_create || die "not provisioning $ENV_NAME"
  settings_check || die "not provisioning $ENV_NAME"
  experiment_marker || die "not provisioning $ENV_NAME"
  ocr_nodes_off_check || die "not provisioning $ENV_NAME"
  for attempt in 1 2 3; do
    deploy_env_guardrails && deploy_policy_exemptions && deploy_stack && save_outputs && return 0
    [ "$attempt" = 3 ] && die "provisioning failed three times"
    echo "retrying in 60s"
    sleep 60
  done
}
# What a provision would deploy, changing nothing (provision.sh --what-if):
# the settings as provision reads them (from the store when it exists) and
# checks them, dev's experiment marker included, then Azure's preflight
# validation of the stack, policy included, and the template's changes.
# Without a store yet, its own what-if first, and the private settings come
# from the local file. The stack assigns the guard-rail definitions, so in a
# subscription without them (no usnm-guardrails stack, which a provision
# deploys first) only their what-if is shown: the stack's preflight needs
# them.
provision_what_if() {
  local endpoint location out
  SETTINGS_READ_ONLY=true
  endpoint=$(settings_store_endpoint) || die "no what-if for $ENV_NAME"
  location=$(stack_location) || die "no what-if for $ENV_NAME"
  if [ -z "$endpoint" ]; then
    echo "no settings store yet: the provision creates it (scripts/settings-store.sh):"
    settings_store_script --what-if --location "$location" || die "no what-if for $ENV_NAME"
  fi
  load_store || die "no what-if for $ENV_NAME"
  settings_check || die "no what-if for $ENV_NAME"
  experiment_marker --dry-run || die "no what-if for $ENV_NAME"
  ocr_nodes_off_check || die "no what-if for $ENV_NAME"
  if ! out=$(az stack sub show -n "$GUARDRAILS_STACK" --query name -o tsv 2>&1); then
    grep -qiE 'NotFound|could not be found' <<< "$out" || die "can't check $GUARDRAILS_STACK: $out"
    echo "no guard-rail stack $GUARDRAILS_STACK yet: the provision deploys it first; its changes:"
    az deployment sub what-if --location "$location" --template-file infra/guardrails.bicep --only-show-errors ||
      die "the what-if of $GUARDRAILS_STACK failed"
    echo "the stack $STACK assigns its definitions, so its preflight runs once they exist"
    return 0
  fi
  local exempting
  echo "the policy step's changes (it runs before the stack, #336):"
  exempting=$(deploy_policy_exemptions --what-if) || die "the what-if of the policy step failed"
  deploy_stack --what-if --exempting "$exempting" || die "the what-if of $STACK failed"
}
