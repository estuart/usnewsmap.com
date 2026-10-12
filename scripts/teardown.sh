#!/usr/bin/env bash
# Delete one usnewsmap environment: its deployment stack with every resource
# and resource group it manages, and its GitHub Environment (08 §8.9). The
# last environment in a subscription also takes the guard-rail stack, unless
# the archival account (usnm-archive) still assigns it. The data goes with
# it (a new environment rebuilds it from LoC). Its settings file
# (scripts/lib/env.sh) is kept, renamed .env.deleted-<time>, without its
# experiment marker, for scripts/stand-up.sh to start from, and so are its
# private settings in the settings store (under its label), which the
# store's own stack (usnm-config) keeps.
#
# An environment that uses the archival account (USNM_ARCHIVE_ACCOUNT) is
# first detached from it: the account's delete lock is lifted, the
# environment is provisioned without the setting and without the search
# cluster (the settings that grant it roles there), the account is checked
# for anything of the environment's left behind, and the lock is put back
# (scripts/archive-store.sh deploy), whether or not those steps succeed. The
# account and its data are never changed (#275).
#
#   scripts/teardown.sh --env NAME [--subscription ID] [--repo OWNER/NAME]
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
# The positional form (`teardown.sh prod [options]`) works for one release,
# with a warning (#277).
if [ $# -ge 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/teardown.sh ENV [options]\` is deprecated; use --env (#277)" >&2
  set -- --env "$@"
fi
ENV_NAME="" SUBSCRIPTION="" REPO=""
while [ $# -gt 0 ]; do
  case "$1" in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --subscription) [ $# -ge 2 ] || usage; SUBSCRIPTION=$2; shift 2 ;;
    --repo) [ $# -ge 2 ] || usage; REPO=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
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
# which its delete lock can keep from being deleted with the group, and
# roles in its group; detached below. An old copy can't be: provisioning
# applies the live copy's settings. A detach that was interrupted after it
# cleared the setting is recorded, so running this again finishes it.
archive_mark="$(dirname "$ENV_FILE")/teardown-$SUBSCRIPTION.archive"
archive=$(aget USNM_ARCHIVE_ACCOUNT)
resuming=false
if [ -z "$archive" ] && [ -s "$archive_mark" ]; then
  archive=$(cat "$archive_mark")
  resuming=true
fi
if [ -n "$archive" ]; then
  [ "$old_copy" = false ] ||
    die "$ENV_NAME's settings use the archival account (USNM_ARCHIVE_ACCOUNT), and this is an old copy, which they don't describe; remove its endpoint and roles there by hand first"
  [[ $archive =~ ^/subscriptions/([^/]+)/resourceGroups/([^/]+)/providers/Microsoft.Storage/storageAccounts/([^/]+)$ ]] ||
    die "USNM_ARCHIVE_ACCOUNT isn't a storage account's resource id: $archive"
  archive_sub=${BASH_REMATCH[1]} archive_group=${BASH_REMATCH[2]} archive_name=${BASH_REMATCH[3]}
fi
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
# Provisioning recreates the stack: never after it's been detached.
[ -z "$archive" ] || [ "$stack" = true ] ||
  die "$ENV_NAME's stack is already detached but its settings still use the archival account; remove its endpoint and roles there by hand, clear USNM_ARCHIVE_ACCOUNT and run this again"
# Sorts what the stack manages ($ids) into what's deleted below.
sort_ids() {
  groups=$(grep -Ei '^/subscriptions/[^/]+/resourceGroups/[^/]+$' <<< "$ids" | sed 's|.*/||' || true)
  # The environment's custom role outlives its resource group: it's declared
  # in the group but stored with the subscription.
  roles=$(grep -i '/providers/Microsoft.Authorization/roleDefinitions/' <<< "$ids" || true)
  # Key Vaults are soft-deleted with their group; purged below where allowed.
  vaults=$(grep -iE '/providers/Microsoft.KeyVault/vaults/[^/]+$' <<< "$ids" | sed 's|.*/||' || true)
  # Role assignments outside the environment's groups don't go with them:
  # deleted one by one, so none is left pointing at a deleted identity.
  local id g inside
  outside=""
  for id in $(grep -i '/providers/Microsoft.Authorization/roleAssignments/' <<< "$ids" || true); do
    inside=false
    for g in $groups; do
      if grep -qi "/resourceGroups/$g/" <<< "$id"; then inside=true; break; fi
    done
    [ "$inside" = true ] || outside="$outside$id"$'\n'
  done
  outside=${outside%$'\n'}
}
sort_ids
# The guard-rail definitions go with the last environment in the subscription.
# Any other usnm- stack keeps them, the archival account's (usnm-archive)
# too: its group assigns them. The settings store's (usnm-config) assigns
# none, so it doesn't count.
others=$(az stack sub list --query "[?starts_with(name, 'usnm-') && name != '$STACK' && name != '$GUARDRAILS_STACK' && name != '$SETTINGS_STACK'].name" -o tsv)
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
if [ -n "$archive" ]; then
  echo "  first its endpoint and roles on the archival account $archive_name (lock lifted, a provision"
  echo "  without USNM_ARCHIVE_ACCOUNT and USNM_SEARCH_CLUSTER, lock put back; the account and its data stay)"
fi
sed 's/^/  resource group (everything in it) /' <<< "$groups"
[ -n "$roles" ] && sed 's/^/  /' <<< "$roles"
[ -n "$outside" ] && sed 's/^/  /' <<< "$outside"
[ -n "$vaults" ] && sed 's/^/  Key Vault (then purged, unless purge protection holds it) /' <<< "$vaults"
if [ "$guardrails" = true ]; then
  echo "  the guard-rail stack $GUARDRAILS_STACK and its policy definitions"
elif [ -n "$others" ]; then
  echo "  (the guard-rail stack $GUARDRAILS_STACK stays: other stacks use it: $others)"
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
  # Only ci runs on main from a push or a dispatch deploy (its publish job);
  # pull request runs and other workflows never roll an environment, so
  # they aren't waited for (#275). total_count covers every matching run,
  # however many there are.
  unfinished() {
    local s e n total=0
    for s in requested queued waiting pending in_progress; do
      for e in push workflow_dispatch; do
        n=$(gh api "repos/$REPO/actions/workflows/ci.yml/runs?branch=main&event=$e&status=$s&per_page=1" \
          -q .total_count) || return 1
        total=$((total + n))
      done
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
      echo "waiting for queued and running deploys (ci on main) in $REPO to finish"
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

# Detach the environment from the archival account (#275). Deleting its
# group would also delete its endpoint's connection on the account, which
# the account's delete lock refuses, and leave its roles in the account's
# group. A provision without the setting has the stack delete both itself,
# with the lock lifted for as long as that takes and put back on any exit.
if [ -n "$archive" ]; then
  restore_lock() {
    echo "putting the archival account's lock back"
    scripts/archive-store.sh deploy --subscription "$archive_sub" > /dev/null && return 0
    echo "error: the lock on $archive_name may still be off: run scripts/archive-store.sh deploy --subscription $archive_sub" >&2
    return 1
  }
  trap 'restore_lock || true' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  # The identities whose roles must be gone afterwards, read before the
  # provision deletes the search cluster's, and kept beside the marker so a
  # run that picks up an interrupted detach checks all of them. Read afresh
  # while the setting is still on: the detach hasn't started, and the
  # environment's identities may have changed since an earlier attempt.
  principals_file="${archive_mark%.archive}.principals"
  if [ "$resuming" = true ] && [ -s "$principals_file" ]; then
    principals=$(cat "$principals_file")
  else
    principals=""
    for g in $groups; do
      p=$(az identity list -g "$g" --query "[].principalId" -o tsv) || die "can't list the identities in $g"
      principals="$principals$p"$'\n'
    done
    mkdir -p "$(dirname "$principals_file")"
    printf '%s' "$principals" > "$principals_file"
  fi
  echo "detaching $ENV_NAME from the archival account $archive_name (USNM_ARCHIVE_ACCOUNT was $archive)"
  scripts/archive-store.sh unlock --subscription "$archive_sub"
  # archive-store.sh selects its own subscription.
  az account set --subscription "$SUBSCRIPTION"
  mkdir -p "$(dirname "$archive_mark")"
  printf '%s\n' "$archive" > "$archive_mark"
  aset USNM_ARCHIVE_ACCOUNT ""
  aset USNM_SEARCH_CLUSTER false
  NO_EXPERIMENT_CHECK=true provision
  # Nothing of the environment's may be left in the account's group: no
  # role assignment for one of its identities, and no endpoint connection
  # from one of its groups. Read only: the account isn't changed.
  # JMESPath in az has no lower-case function: the scopes are compared here.
  prefix=$(lower "/subscriptions/$archive_sub/resourceGroups/$archive_group/")
  assigned=$(az role assignment list --all --subscription "$archive_sub" \
    --query "[].[principalId, scope]" -o tsv) ||
    die "can't list the role assignments in $archive_group"
  stray=""
  while IFS=$'\t' read -r p scope; do
    [ -n "$p" ] && [[ $(lower "$scope") == "$prefix"* ]] && grep -Fqx "$p" <<< "$principals" &&
      stray="$stray  $scope ($p)"$'\n'
  done <<< "$assigned"
  [ -z "$stray" ] || die "$ENV_NAME's identities still have roles in $archive_group:"$'\n'"$stray"
  conns=$(az storage account show -n "$archive_name" -g "$archive_group" --subscription "$archive_sub" \
    --query "privateEndpointConnections[].privateEndpoint.id" -o tsv) ||
    die "can't read the endpoint connections of $archive_name"
  for g in $groups; do
    ! grep -qi "/resourceGroups/$g/" <<< "$conns" ||
      die "$archive_name still has an endpoint connection from $g:"$'\n'"$conns"
  done
  # The traps stay until the lock is back, so an interrupt meanwhile still
  # puts it back on.
  restore_lock || die "detached from $archive_name, but its lock isn't back on (see above); then run this again"
  trap - EXIT INT TERM
  rm -f "$archive_mark" "$principals_file"
  az account set --subscription "$SUBSCRIPTION"
  # The stack no longer manages the endpoint, the roles or the archive's
  # writer role definition.
  ids=$(az stack sub show -n "$STACK" --query "resources[].id" -o tsv)
  sort_ids
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
for id in $outside; do
  if ! out=$(az resource show --ids "$id" -o none 2>&1); then
    grep -qiE 'NotFound|could not be found|RoleAssignmentNotFound' <<< "$out" || die "can't check $id: $out"
    continue
  fi
  echo "deleting role assignment $id"
  az role assignment delete --ids "$id" -o none
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
  others=$(az stack sub list --query "[?starts_with(name, 'usnm-') && name != '$GUARDRAILS_STACK' && name != '$SETTINGS_STACK'].name" -o tsv) ||
    die "can't list deployment stacks; kept $GUARDRAILS_STACK (delete it when no environment uses it)"
  if [ -n "$others" ]; then
    echo "keeping $GUARDRAILS_STACK: another stack was created meanwhile ($others)"
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

# The next stand-up names its own experiment. The settings store stays,
# with the environment's private settings, so its endpoint stays too.
if [ -f "$ENV_FILE" ]; then
  adel USNM_EXPERIMENT
  adel USNM_EXPERIMENT_STARTED
  mv "$ENV_FILE" "$ENV_FILE.deleted-$(date -u +%Y%m%dT%H%M%SZ)"
fi
echo "deleted environment $ENV_NAME"
