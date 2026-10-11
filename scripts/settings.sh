#!/usr/bin/env bash
# Read or change an environment's settings (scripts/lib/env.sh says where).
#
#   scripts/settings.sh --env prod                            # all of them
#   scripts/settings.sh --env prod --key API_URL              # one
#   scripts/settings.sh --env prod --key KEY --value VALUE    # set one
#   scripts/settings.sh --env prod --key KEY --unset          # clear one
#
# A change takes effect in Azure on the next scripts/provision.sh (or
# bootstrap) run. The template's outputs (API_URL, ACR_NAME, ...) are
# rewritten by every deployment. Where the environment lives (subscription,
# region, repository) is bootstrap's to set: see its --move. A cleared
# setting takes its default (infra/main.bicepparam).
set -euo pipefail
usage() { sed -n '2,13s/^# \{0,1\}//p' "$0" >&2; exit 2; }
# The positional form (`settings.sh prod KEY [VALUE]`, "" clearing) works
# for one release, with a warning (#277).
if [ $# -ge 1 ] && [ $# -le 3 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/settings.sh ENV [KEY [VALUE]]\` is deprecated; use --env, --key and --value or --unset (#277)" >&2
  legacy=(--env "$1")
  [ $# -lt 2 ] || legacy+=(--key "$2")
  if [ $# -eq 3 ] && [ -z "$3" ]; then legacy+=(--unset); elif [ $# -eq 3 ]; then legacy+=(--value "$3"); fi
  set -- "${legacy[@]}"
fi
ENV_NAME="" KEY="" VALUE="" SET=false UNSET=false
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --key) [ $# -ge 2 ] || usage; KEY=$2; shift 2 ;;
    --value) [ $# -ge 2 ] || usage; VALUE=$2; SET=true; shift 2 ;;
    --unset) UNSET=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
if { $SET || $UNSET; } && [ -z "$KEY" ]; then echo "error: --value and --unset need --key" >&2; usage; fi
if $SET && $UNSET; then echo "error: --value or --unset, not both" >&2; usage; fi
cd "$(dirname "$0")/.."
. scripts/lib/env.sh
[ -e "$ENV_FILE" ] || $SET || $UNSET ||
  { echo "error: no settings for $ENV_NAME" >&2; exit 1; }
if [ -z "$KEY" ]; then
  cat "$ENV_FILE"
  exit 0
fi
$SET || $UNSET || { aget "$KEY"; exit 0; }
# Changing these here would deploy a second copy elsewhere (or rebuild this
# one in another region), past bootstrap's move checks.
case "$KEY" in
  AZURE_ENV_NAME|AZURE_SUBSCRIPTION_ID|AZURE_LOCATION|USNM_GITHUB_REPO|USNM_GITHUB_REPO_IDS)
    if [ "$(aget "$KEY")" != "$VALUE" ] && [ -n "$(aget "$KEY")" ]; then
      echo "error: $KEY is set by bootstrap; to move the environment, run scripts/bootstrap.sh --env $ENV_NAME --move (see its header)" >&2
      exit 1
    fi ;;
esac
if $UNSET; then adel "$KEY"; else aset "$KEY" "$VALUE"; fi
