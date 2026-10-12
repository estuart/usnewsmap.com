#!/usr/bin/env bash
# Deploy an existing environment's stack from its current settings, without
# the rest of bootstrap (no CI run, no GitHub or domain steps). Use it once
# a settings change is merged (docs/operations.md, "Settings"):
#
#   scripts/settings.sh --env prod --key USNM_SEARCH_BACKEND --value quickwit
#   # commit infra/env/prod.json, open a pull request, merge, pull
#   scripts/provision.sh --env prod
#
# It deploys the committed settings (infra/env/<env>.json), the private
# ones in the settings store (under the environment's label) and the local
# file's (scripts/lib/env.sh), and refuses while infra/env/<env>.json or the
# schema has uncommitted changes; --allow-dirty deploys them anyway (an
# experiment in dev). The first provision in a subscription creates the
# settings store (scripts/settings-store.sh).
#
# --what-if changes nothing: it reads the settings the same way, runs
# Azure's preflight validation of the stack (a policy denial fails it) and
# shows the template's changes.
#
# Needs az 2.61+, signed in to the environment's subscription with Owner.
set -euo pipefail
usage() { echo "usage: scripts/provision.sh --env <env> [--allow-dirty] [--what-if]" >&2; exit 2; }
# The positional form (`provision.sh prod`) works for one release, with a
# warning (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/provision.sh ENV\` is deprecated; use --env (#277)" >&2
  set -- --env "$1"
fi
ENV_NAME="" ALLOW_DIRTY=false WHAT_IF=false
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --allow-dirty) ALLOW_DIRTY=true; shift ;;
    --what-if) WHAT_IF=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
. scripts/lib/env.sh
if ! $ALLOW_DIRTY; then
  rc=0
  committed_clean || rc=$?
  [ "$rc" -ne 1 ] ||
    die "$COMMITTED_FILE has uncommitted changes (above): commit them (through a pull request for a shared environment), or pass --allow-dirty for an experiment"
  [ "$rc" -eq 0 ] || exit 1
fi
need_stack_az
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME; run scripts/bootstrap.sh --env $ENV_NAME first"
az account set --subscription "$(aget AZURE_SUBSCRIPTION_ID)" 2> /dev/null ||
  die "run: az login (the environment is in subscription $(aget AZURE_SUBSCRIPTION_ID))"
if $WHAT_IF; then
  provision_what_if
  exit 0
fi
provision
echo "deployed stack $STACK"
