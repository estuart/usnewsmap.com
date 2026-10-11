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
# One parameter of a stack, by name in any case (a stack can change it). $1
# is the stack's "[keys(parameters), values(parameters)[].value]" as tsv:
# two rows.
stack_param() {
  awk -F'\t' -v want="$2" 'NR == 1 { n = split($0, k, "\t") }
    NR == 2 { split($0, v, "\t"); for (i = 1; i <= n; i++) if (tolower(k[i]) == tolower(want)) print v[i] }' <<< "$1"
}
# Create or update it:  deploy_guardrails LOCATION [EMAILS [DEV_LOCATION]]
# LOCATION counts only when the stack doesn't exist yet: a stack's location
# is fixed when it's created, and later callers in other regions reuse it.
# The stack also holds the budget on dev's groups (#275): it mails EMAILS,
# or keeps the recipients it has when EMAILS is empty, so a caller without
# recipients (an environment without them, the archival account) doesn't
# delete it. A budget's start date can't change and can't be in a past
# month: this month for a new budget, the stack's own value after that.
# DEV_LOCATION, dev's region, names the Container Apps environment's own
# group; empty keeps the stack's.
deploy_guardrails() {
  local location params="" emails=${2:-} dev_location=${3:-} kept start=""
  # Only "not found" means a first deployment; other errors stop here.
  if ! location=$(az stack sub show -n "$GUARDRAILS_STACK" --query location -o tsv 2>&1); then
    grep -qiE 'NotFound|could not be found' <<< "$location" || {
      echo "error: can't check $GUARDRAILS_STACK: $location" >&2
      return 1
    }
    location=$1
  else
    params=$(az stack sub show -n "$GUARDRAILS_STACK" \
      --query "[keys(parameters || \`{}\`), values(parameters || \`{}\`)[].value]" -o tsv) || {
      echo "error: can't read the parameters of $GUARDRAILS_STACK" >&2
      return 1
    }
  fi
  kept=$(stack_param "$params" devBudgetEmails)
  [ -z "$kept" ] || start=$(stack_param "$params" devBudgetStart)
  [ -n "$emails" ] || emails=$kept
  [ -n "$start" ] || start=$(date -u +%Y-%m-01)
  [ -n "$dev_location" ] || dev_location=$(stack_param "$params" devBudgetLocation)
  az stack sub create --name "$GUARDRAILS_STACK" --location "$location" \
    --template-file infra/guardrails.bicep \
    --parameters devBudgetEmails="$emails" devBudgetStart="$start" \
    devBudgetLocation="${dev_location:-eastus2}" \
    --action-on-unmanage deleteResources \
    --deny-settings-mode denyDelete \
    --description "usnewsmap guard-rail policy definitions, shared by every environment in the subscription and the archival account" \
    --yes --only-show-errors -o none
}
