# shellcheck shell=bash
# The guard-rail policy stack, shared by scripts/lib/env.sh (every
# environment) and scripts/archive-store.sh (the archival account). Source it
# with the repository root as the working directory. Needs az signed in to
# the subscription, and a die() function.

# The stack commands need az 2.61+ (--action-on-unmanage); older versions
# stop at the first deployment with "unrecognized arguments".
# grep reads the help to the end: with -q it could quit early, and pipefail
# would count az's SIGPIPE as a failure.
need_stack_az() {
  az stack sub create --help 2> /dev/null | grep -- --action-on-unmanage > /dev/null ||
    die "az $(az version --query '"azure-cli"' -o tsv 2> /dev/null) is too old for deployment stacks; upgrade to 2.61 or later"
}
# The guard-rail policy definitions that every environment in the
# subscription, and the archival account's group, assign: one stack of their
# own, so no two stacks manage the same resource.
GUARDRAILS_STACK=usnm-guardrails
# Create or update it:  deploy_guardrails LOCATION
# LOCATION counts only when the stack doesn't exist yet: a stack's location
# is fixed when it's created, and later callers in other regions reuse it.
deploy_guardrails() {
  local location
  # Only "not found" means a first deployment; other errors stop here.
  if ! location=$(az stack sub show -n "$GUARDRAILS_STACK" --query location -o tsv 2>&1); then
    grep -qiE 'NotFound|could not be found' <<< "$location" || {
      echo "error: can't check $GUARDRAILS_STACK: $location" >&2
      return 1
    }
    location=$1
  fi
  az stack sub create --name "$GUARDRAILS_STACK" --location "$location" \
    --template-file infra/guardrails.bicep \
    --action-on-unmanage deleteResources \
    --deny-settings-mode denyDelete \
    --description "usnewsmap guard-rail policy definitions, shared by every environment in the subscription and the archival account" \
    --yes --only-show-errors -o none
}
