#!/usr/bin/env bash
# Deploy an existing environment's stack from its current settings, without
# the rest of bootstrap (no CI run, no GitHub or domain steps). Use it after
# changing a setting:
#
#   scripts/settings.sh --env prod --key USNM_SEARCH_BACKEND --value quickwit
#   scripts/provision.sh --env prod
#
# Needs az 2.61+, signed in to the environment's subscription with Owner.
set -euo pipefail
usage() { echo "usage: scripts/provision.sh --env <env>" >&2; exit 2; }
# The positional form (`provision.sh prod`) works for one release, with a
# warning (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/provision.sh ENV\` is deprecated; use --env (#277)" >&2
  set -- --env "$1"
fi
ENV_NAME=""
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
. scripts/lib/env.sh
need_stack_az
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME; run scripts/bootstrap.sh --env $ENV_NAME first"
az account set --subscription "$(aget AZURE_SUBSCRIPTION_ID)" 2> /dev/null ||
  die "run: az login (the environment is in subscription $(aget AZURE_SUBSCRIPTION_ID))"
provision
echo "deployed stack $STACK"
