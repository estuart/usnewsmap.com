#!/usr/bin/env bash
# Read or change an environment's settings (scripts/lib/env.sh says where).
#
#   scripts/settings.sh --env prod                            # all of them
#   scripts/settings.sh --env prod --key API_URL              # one
#   scripts/settings.sh --env prod --key KEY --value VALUE    # set one
#   scripts/settings.sh --env prod --key KEY --unset          # clear one
#   scripts/settings.sh --env prod --key KEY --unset-local    # drop a local copy
#   scripts/settings.sh --env prod --migrate                  # move to infra/env
#
# A public setting (infra/env/schema.json) is set in infra/env/<env>.json:
# commit it, merge it through a pull request, then scripts/provision.sh
# deploys it. A private one goes in the settings vault, with your az
# sign-in, and out of the local file; until the vault exists, in the local
# file. A cleared setting takes its default (infra/main.bicepparam): --unset
# removes it everywhere, a private one from the vault too (deleted, kept
# soft-deleted; setting it again recovers it first). --unset-local removes
# only the local file's entry, so a private setting's vault value applies.
# The template's outputs (API_URL, ACR_NAME, ...) are rewritten by every
# deployment. Where the environment lives (subscription, region,
# repository) is bootstrap's to set: see its --move. --migrate moves an
# environment's public settings from the local file into
# infra/env/<env>.json (#295).
set -euo pipefail
usage() { sed -n '2,24s/^# \{0,1\}//p' "$0" >&2; exit 2; }
# The positional form (`settings.sh prod KEY [VALUE]`, "" clearing) works
# for one release, with a warning (#277).
if [ $# -ge 1 ] && [ $# -le 3 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/settings.sh ENV [KEY [VALUE]]\` is deprecated; use --env, --key and --value or --unset (#277)" >&2
  legacy=(--env "$1")
  [ $# -lt 2 ] || legacy+=(--key "$2")
  if [ $# -eq 3 ] && [ -z "$3" ]; then legacy+=(--unset); elif [ $# -eq 3 ]; then legacy+=(--value "$3"); fi
  set -- "${legacy[@]}"
fi
ENV_NAME="" KEY="" VALUE="" SET=false UNSET=false UNSET_LOCAL=false MIGRATE=false
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --key) [ $# -ge 2 ] || usage; KEY=$2; shift 2 ;;
    --value) [ $# -ge 2 ] || usage; VALUE=$2; SET=true; shift 2 ;;
    --unset) UNSET=true; shift ;;
    --unset-local) UNSET_LOCAL=true; shift ;;
    --migrate) MIGRATE=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
if { $SET || $UNSET || $UNSET_LOCAL; } && [ -z "$KEY" ]; then echo "error: --value, --unset and --unset-local need --key" >&2; usage; fi
n=0; for f in $SET $UNSET $UNSET_LOCAL; do ! $f || n=$((n + 1)); done
[ $n -le 1 ] || { echo "error: one of --value, --unset and --unset-local" >&2; usage; }
if $MIGRATE && { [ -n "$KEY" ] || [ $n -gt 0 ]; }; then echo "error: --migrate takes only --env" >&2; usage; fi
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
. scripts/lib/env.sh
next_steps() {
  echo "next: commit $COMMITTED_FILE, open a pull request and merge it; then, from main, scripts/provision.sh --env $ENV_NAME"
}
if $MIGRATE; then
  migrate_settings || exit 1
  next_steps
  exit 0
fi
[ -e "$ENV_FILE" ] || [ -e "$COMMITTED_FILE" ] || $SET || $UNSET ||
  { echo "error: no settings for $ENV_NAME" >&2; exit 1; }
if [ -z "$KEY" ]; then
  vault=$(local_get SETTINGS_KEYVAULT)
  echo "# committed: $COMMITTED_FILE"
  [ -z "$COMMITTED_SETTINGS" ] || printf '%s\n' "$COMMITTED_SETTINGS"
  echo "# local: $ENV_FILE"
  [ ! -f "$ENV_FILE" ] || cat "$ENV_FILE"
  [ -z "$vault" ] ||
    echo "# private settings in the settings vault: az keyvault secret list --vault-name $vault --query '[].name' -o tsv"
  exit 0
fi
# A private setting the local file doesn't have is read from the vault.
if ! $SET && ! $UNSET && ! $UNSET_LOCAL; then aget "$KEY"; exit 0; fi
# Changing these here would deploy a second copy elsewhere (or rebuild this
# one in another region), past bootstrap's move checks.
# (Dropping a private one's local copy leaves the vault's value: no move.)
if ! { $UNSET_LOCAL && is_private "$KEY"; }; then
  case "$KEY" in
    AZURE_ENV_NAME|AZURE_SUBSCRIPTION_ID|AZURE_LOCATION|USNM_GITHUB_REPO|USNM_GITHUB_REPO_IDS)
      if [ "$(aget "$KEY")" != "$VALUE" ] && [ -n "$(aget "$KEY")" ]; then
        echo "error: $KEY is set by bootstrap; to move the environment, run scripts/bootstrap.sh --env $ENV_NAME --move (see its header)" >&2
        exit 1
      fi ;;
  esac
fi
if $UNSET_LOCAL; then
  local_del "$KEY"
  echo "removed $KEY from $ENV_FILE"
  if is_private "$KEY"; then
    v=$(aget "$KEY")
    if [ -n "$v" ]; then echo "$KEY now comes from the settings vault"; else echo "$KEY is unset: it takes its default"; fi
  fi
  exit 0
fi
# Where a private setting goes, resolved before the change, so the message
# after it says where it went: the settings vault (named by the local file
# or, on a machine without that, the stack's output, which this records),
# or the local file until the vault exists.
vault=""
! is_private "$KEY" || vault=$(settings_vault_name)
# Once the vault exists, aset writes a private setting there, with the
# operator's own sign-in, and removes any local copy, which would otherwise
# hide the vault's value (an empty value stays a local "", which does);
# adel deletes it from the vault and the local file.
if $UNSET; then adel "$KEY"; else aset "$KEY" "$VALUE"; fi
if is_public "$KEY"; then
  next_steps
elif is_private "$KEY" && $UNSET && [ -n "$vault" ]; then
  echo "cleared $KEY in the settings vault $vault and $ENV_FILE; it takes its default. next: scripts/provision.sh --env $ENV_NAME"
elif is_private "$KEY" && $UNSET; then
  echo "cleared $KEY in $ENV_FILE; it takes its default"
elif is_private "$KEY" && [ -n "$vault" ] && [ -n "$VALUE" ]; then
  echo "$KEY is in the settings vault $vault. next: scripts/provision.sh --env $ENV_NAME"
elif is_private "$KEY" && [ -n "$vault" ]; then
  echo "note: an empty $KEY stays in $ENV_FILE as \"\", which hides the settings vault's value; --unset clears it everywhere"
elif is_private "$KEY"; then
  echo "note: $KEY is private; it's in $ENV_FILE until the settings vault exists (the next provision creates it)"
fi
