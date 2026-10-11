#!/usr/bin/env bash
# Read or change an environment's settings (scripts/lib/env.sh says where).
#
#   scripts/settings.sh prod                 # all of them
#   scripts/settings.sh prod API_URL         # one
#   scripts/settings.sh prod KEY VALUE       # set one ("" clears it)
#
# A change takes effect in Azure on the next scripts/provision.sh (or
# bootstrap) run. The template's outputs (API_URL, ACR_NAME, ...) are
# rewritten by every deployment. Where the environment lives (subscription,
# region, repository) is bootstrap's to set: see its --move.
set -euo pipefail
[ $# -ge 1 ] && [ $# -le 3 ] || { sed -n '2,11s/^# \{0,1\}//p' "$0" >&2; exit 2; }
ENV_NAME=$1
cd "$(dirname "$0")/.."
. scripts/lib/env.sh
[ -e "$ENV_FILE" ] || [ $# -eq 3 ] ||
  { echo "error: no settings for $ENV_NAME" >&2; exit 1; }
case $# in
  1) cat "$ENV_FILE" ;;
  2) aget "$2" ;;
  3)
    # Changing these here would deploy a second copy elsewhere (or rebuild
    # this one in another region), past bootstrap's move checks.
    case "$2" in
      AZURE_ENV_NAME|AZURE_SUBSCRIPTION_ID|AZURE_LOCATION|USNM_GITHUB_REPO|USNM_GITHUB_REPO_IDS)
        if [ "$(aget "$2")" != "$3" ] && [ -n "$(aget "$2")" ]; then
          echo "error: $2 is set by bootstrap; to move the environment, run scripts/bootstrap.sh $ENV_NAME --move (see its header)" >&2
          exit 1
        fi ;;
    esac
    aset "$2" "$3" ;;
esac
