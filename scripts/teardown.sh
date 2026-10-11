#!/usr/bin/env bash
# Delete one usnewsmap environment: its deployment stack with every resource
# and resource group it manages, and its GitHub Environment (08 §8.9). The
# last environment in a subscription also takes the guard-rail stack. The
# data goes with it (a new environment rebuilds it from LoC). Its local
# settings (.azure/<env>/.env) are kept, renamed, in case they're needed.
#
#   scripts/teardown.sh <env> [--subscription ID] [--repo OWNER/NAME]
#
# The subscription defaults to the environment's own (from its settings).
# Passing another --subscription removes the copy left there after a move:
# only its Azure resources, since the GitHub Environment, the deploy list
# and the settings belong to the live copy. Don't run bootstrap for the same
# environment while this runs.
#
# Needs: az 2.61+ and gh, signed in, Owner on the subscription and admin on
# the repository. Asks for the environment's name before deleting anything.
set -euo pipefail

usage() { awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "$0"; exit 2; }
[ $# -ge 1 ] || usage
ENV_NAME=$1; shift
case "$ENV_NAME" in -*|"") usage ;; esac
SUBSCRIPTION="" REPO=""
while [ $# -gt 0 ]; do
  case "$1" in
    --subscription) SUBSCRIPTION=$2; shift 2 ;;
    --repo) REPO=$2; shift 2 ;;
    *) usage ;;
  esac
done
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
. scripts/lib/env.sh
need_stack_az

az account show -o none 2> /dev/null || die "run: az login [--tenant TENANT]"
# The environment's recorded subscription, unless another is named. Without
# a record there's no telling the live copy from an old one: stop.
recorded=$(aget AZURE_SUBSCRIPTION_ID)
[ -n "$recorded" ] ||
  die "no AZURE_SUBSCRIPTION_ID in $ENV_FILE; can't tell which subscription holds $ENV_NAME"
[ -n "$SUBSCRIPTION" ] || SUBSCRIPTION=$recorded
az account set --subscription "$SUBSCRIPTION" ||
  die "can't select subscription $SUBSCRIPTION (az login to its tenant?)"
SUBSCRIPTION=$(az account show --query id -o tsv)
# Another subscription than the recorded one: an old copy after a move.
old_copy=false
# Subscription ids compare case-insensitively; tr rather than ${x,,}, which
# macOS's bash 3.2 doesn't have.
lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }
[ "$(lower "$SUBSCRIPTION")" = "$(lower "$recorded")" ] || old_copy=true
gh auth status > /dev/null 2>&1 || die "run: gh auth login"
# The repository whose workflows deploy this environment: the one bootstrap
# recorded. --repo, or the checkout's own, must be the same one.
configured=$(aget USNM_GITHUB_REPO)
[ -n "$REPO" ] || REPO=${configured:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}
[ -z "$configured" ] || [ "$REPO" = "$configured" ] ||
  die "environment $ENV_NAME is deployed from $configured, not $REPO"
# An environment using the archival account has a private endpoint to it,
# which its delete lock can keep from being deleted with the group.
[ -z "$(aget USNM_ARCHIVE_ACCOUNT)" ] ||
  die "$ENV_NAME uses the archival account (USNM_ARCHIVE_ACCOUNT). First: scripts/archive-store.sh unlock; scripts/settings.sh $ENV_NAME USNM_ARCHIVE_ACCOUNT \"\"; scripts/provision.sh $ENV_NAME; scripts/archive-store.sh deploy"
# What the stack manages, saved before it's detached so an interrupted
# teardown can be run again and pick up where it stopped.
resume="$(dirname "$ENV_FILE")/teardown-$SUBSCRIPTION.ids"
stack=true
if out=$(az stack sub show -n "$STACK" -o none 2>&1); then
  ids=$(az stack sub show -n "$STACK" --query "resources[].id" -o tsv)
elif ! grep -qiE 'NotFound|could not be found' <<< "$out"; then
  # Only a stack that's really gone means "already detached".
  die "can't check the deployment stack $STACK: $out"
elif [ -s "$resume" ]; then
  echo "resuming an interrupted teardown of $ENV_NAME (stack already detached)"
  ids=$(cat "$resume") stack=false
else
  die "no deployment stack $STACK in subscription $SUBSCRIPTION"
fi
groups=$(grep -Ei '^/subscriptions/[^/]+/resourceGroups/[^/]+$' <<< "$ids" | sed 's|.*/||' || true)
# The environment's custom role outlives its resource group: it's declared
# in the group but stored with the subscription.
roles=$(grep -i '/providers/Microsoft.Authorization/roleDefinitions/' <<< "$ids" || true)
# Key Vaults are soft-deleted with their group; purged below where allowed.
vaults=$(grep -iE '/providers/Microsoft.KeyVault/vaults/[^/]+$' <<< "$ids" | sed 's|.*/||' || true)
# The guard-rail definitions go with the last environment in the subscription.
others=$(az stack sub list --query "[?starts_with(name, 'usnm-') && name != '$STACK' && name != '$GUARDRAILS_STACK'].name" -o tsv)
guardrails=false
if [ -z "$others" ]; then
  if out=$(az stack sub show -n "$GUARDRAILS_STACK" -o none 2>&1); then
    guardrails=true
  else
    grep -qiE 'NotFound|could not be found' <<< "$out" ||
      die "can't check the guard-rail stack $GUARDRAILS_STACK: $out"
  fi
fi

echo "This deletes environment $ENV_NAME from subscription $SUBSCRIPTION:"
sed 's/^/  resource group (everything in it) /' <<< "$groups"
[ -n "$roles" ] && sed 's/^/  /' <<< "$roles"
[ -n "$vaults" ] && sed 's/^/  Key Vault (then purged, unless purge protection holds it) /' <<< "$vaults"
if [ "$guardrails" = true ]; then
  echo "  the guard-rail stack $GUARDRAILS_STACK and its policy definitions"
elif [ -n "$others" ]; then
  echo "  (the guard-rail stack $GUARDRAILS_STACK stays: other environments use it: $others)"
fi
if [ "$old_copy" = true ]; then
  echo "This is an old copy: $ENV_NAME now lives in subscription $recorded, whose"
  echo "GitHub Environment, deploy list entry and settings stay as they are."
else
  echo "and the GitHub Environment $ENV_NAME in $REPO."
fi
read -r -p "Type the environment name to confirm: " answer
[ "$answer" = "$ENV_NAME" ] || die "not confirmed"

# The GitHub side belongs to the live copy; an old copy skips it.
if [ "$old_copy" = false ]; then
  # First stop the workflows deploying to it, so no new rollout targets what's
  # being deleted.
  # Unset is fine (nothing deploys anywhere); any other error stops teardown.
  if ! envs=$(gh variable get USNM_DEPLOY_ENVIRONMENTS -R "$REPO" 2>&1); then
    grep -qiE 'not found|HTTP 404' <<< "$envs" ||
      die "can't read USNM_DEPLOY_ENVIRONMENTS in $REPO: $envs"
    envs=""
  fi
  # Taking the environment off the list is recorded first, so a teardown
  # interrupted during the wait below still waits when it's run again,
  # although the list no longer names it.
  waiting="$(dirname "$ENV_FILE")/teardown-$SUBSCRIPTION.wait"
  was_listed=false
  [ -e "$waiting" ] && was_listed=true
  if grep -q "\"$ENV_NAME\"" <<< "$envs"; then
    was_listed=true
    mkdir -p "$(dirname "$waiting")"
    : > "$waiting"
    names=$(tr -d '[]" ' <<< "$envs" | tr ',' '\n' | grep -vx "$ENV_NAME" | grep -v '^$' || true)
    envs="[$(sed 's/.*/"&"/' <<< "$names" | paste -sd, -)]"
    [ "$envs" = '[""]' ] && envs='[]'
    gh variable set USNM_DEPLOY_ENVIRONMENTS -R "$REPO" --body "$envs"
  fi
  # A run that already started, or was queued, still holds the old list; wait
  # until none is left. An environment that wasn't on the list can't be
  # deployed by any run, so there's nothing to wait for (dev, #275).
  # total_count covers every matching run, however many there are.
  unfinished() {
    local s n total=0
    for s in requested queued waiting pending in_progress; do
      n=$(gh api "repos/$REPO/actions/runs?status=$s&per_page=1" -q .total_count) || return 1
      total=$((total + n))
    done
    echo "$total"
  }
  if $was_listed; then
    failures=0
    while :; do
      if ! runs=$(unfinished); then
        # A transient GitHub error (a TLS timeout) shouldn't end the wait.
        failures=$((failures + 1))
        [ "$failures" -lt 10 ] || die "can't list workflow runs in $REPO"
        sleep 30
        continue
      fi
      failures=0
      [ "$runs" -gt 0 ] || break
      echo "waiting for queued and running workflows in $REPO to finish"
      sleep 30
    done
    rm -f "$waiting"
  else
    echo "$ENV_NAME isn't on USNM_DEPLOY_ENVIRONMENTS, so no workflow run can deploy it; not waiting"
  fi

  # Last check before anything is deleted: stop if a bootstrap put the
  # environment back on the deploy list while this waited.
  now=$(gh variable get USNM_DEPLOY_ENVIRONMENTS -R "$REPO" 2>&1) ||
    grep -qiE 'not found|HTTP 404' <<< "$now" ||
    die "can't read USNM_DEPLOY_ENVIRONMENTS in $REPO: $now"
  if grep -q "\"$ENV_NAME\"" <<< "$now"; then
    die "$ENV_NAME is on USNM_DEPLOY_ENVIRONMENTS again (a bootstrap ran meanwhile?); nothing deleted"
  fi
fi

# Detach, then delete explicitly: the stack's deleteAll would stop at a
# resource group that also holds something it doesn't manage.
mkdir -p "$(dirname "$resume")"
printf '%s\n' "$ids" > "$resume"
if [ "$stack" = true ]; then
  az stack sub delete -n "$STACK" --action-on-unmanage detachAll --yes --only-show-errors
fi
# Each step skips what an earlier, interrupted run already deleted.
# A lookup that fails for any other reason than "not found" stops here. A
# role scoped to a deleted group answers RoleDefinitionDoesNotExist.
for g in $groups; do
  exists=$(az group exists -n "$g") || die "can't check resource group $g"
  [ "$exists" = true ] || continue
  echo "deleting resource group $g"
  az group delete -n "$g" --yes -o none
done
# A deleted Key Vault stays soft-deleted, holding its name, for its retention
# (7 days outside prod). Purge it so a rebuilt environment can use the name
# (ADR-0017); prod's has purge protection, which refuses, so say so instead.
# The deleted-vault list is eventually consistent: a vault deleted with its
# group can take minutes to appear, so look for a while before concluding
# it's gone (purged by an earlier run).
for v in $vaults; do
  found=false
  for attempt in 1 2 3 4 5 6 7 8 9 10; do
    if out=$(az keyvault show-deleted --name "$v" --query properties.purgeProtectionEnabled -o tsv 2>&1); then
      found=true
      break
    fi
    grep -qiE 'NotFound|not found' <<< "$out" || die "can't check deleted Key Vault $v: $out"
    [ "$attempt" -lt 10 ] && sleep 30
  done
  if [ "$found" = false ]; then
    echo "Key Vault $v isn't in the deleted list after 5 minutes; if it shows up later: az keyvault purge --name $v"
    continue
  fi
  if [ "$out" = true ]; then
    echo "keeping soft-deleted Key Vault $v: purge protection holds it, and its name, until its retention ends"
    continue
  fi
  echo "purging soft-deleted Key Vault $v"
  az keyvault purge --name "$v" -o none
done
for id in $roles; do
  if ! out=$(az resource show --ids "$id" -o none 2>&1); then
    grep -qiE 'NotFound|could not be found|RoleDefinitionDoesNotExist' <<< "$out" || die "can't check $id: $out"
    continue
  fi
  az resource delete --ids "$id" -o none
done
# The assignments went with the resource groups, so nothing uses the
# definitions any more, unless an environment was created while this ran:
# check again just before deleting.
if [ "$guardrails" = true ]; then
  # Fail closed: if the list can't be read, keep the stack.
  others=$(az stack sub list --query "[?starts_with(name, 'usnm-') && name != '$GUARDRAILS_STACK'].name" -o tsv) ||
    die "can't list deployment stacks; kept $GUARDRAILS_STACK (delete it when no environment uses it)"
  if [ -n "$others" ]; then
    echo "keeping $GUARDRAILS_STACK: another environment was created meanwhile ($others)"
    guardrails=false
  fi
fi
# Stacks aren't the whole story: an environment whose teardown stopped after
# its detach has no stack but may still assign the definitions. Ask Azure
# for any assignment, anywhere in the subscription, that still uses them.
if [ "$guardrails" = true ]; then
  used=$(az policy assignment list --disable-scope-strict-match \
    --query "[].[id, policyDefinitionId]" -o tsv) ||
    die "can't list policy assignments; kept $GUARDRAILS_STACK (delete it when no environment uses it)"
  used=$(grep -i '/providers/Microsoft.Authorization/policyDefinitions/usnm-' <<< "$used" | cut -f1 || true)
  if [ -n "$used" ]; then
    echo "keeping $GUARDRAILS_STACK: its definitions are still assigned (an unfinished teardown?):"
    sed 's/^/  /' <<< "$used"
    guardrails=false
  fi
fi
if [ "$guardrails" = true ]; then
  az stack sub delete -n "$GUARDRAILS_STACK" --action-on-unmanage deleteAll --yes --only-show-errors
fi

if [ "$old_copy" = true ]; then
  rm -f "$resume"
  echo "deleted the old copy of $ENV_NAME from subscription $SUBSCRIPTION"
  exit 0
fi

# Already gone is fine; anything else is an error to see.
if ! out=$(gh api -X DELETE "repos/$REPO/environments/$ENV_NAME" 2>&1); then
  grep -q 'HTTP 404' <<< "$out" || die "deleting the GitHub Environment $ENV_NAME: $out"
fi
# Only now is everything gone; until here a re-run resumes from the list.
rm -f "$resume"

if [ -f "$ENV_FILE" ]; then mv "$ENV_FILE" "$ENV_FILE.deleted-$(date -u +%Y%m%dT%H%M%SZ)"; fi
echo "deleted environment $ENV_NAME"
