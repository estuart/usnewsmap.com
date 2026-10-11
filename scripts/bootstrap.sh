#!/usr/bin/env bash
# Stand up (or bring up to date) one usnewsmap environment in any Azure
# subscription or tenant, and wire this GitHub repository to deploy it
# (08 §8.9). Safe to re-run: every step is idempotent.
#
#   scripts/bootstrap.sh --env NAME [options]
#
#   --env NAME            the environment (1-16 lowercase letters and digits)
#   --subscription ID     Azure subscription (default: az's current one)
#   --location REGION     default eastus2 (new environments only)
#   --repo OWNER/NAME     GitHub repository that deploys it (default: this clone's)
#   --domain NAME         create the public DNS zone for NAME (e.g. usnewsmap.com)
#   --alert-email ADDR    alerts and the $80 budget go to ADDR
#   --ingest              deploy the ingest and backfill jobs
#   --cleanup-legacy      remove the repository-level variables and the SWA token
#                         secret used before per-environment deployment
#   --move                allow an existing environment to move to another
#                         subscription (it starts there with no data)
#
# Needs: az 2.61+ (this adds its containerapp extension), gh and curl, signed
# in (`az login --tenant …`, `gh auth login`), Owner on the subscription and
# admin on the GitHub repository.
#
# The environment's settings live in .azure/<env>/.env (KEY="value" lines,
# the layout azd used, so older environments carry over). Everything in Azure
# is one deployment stack, usnm-<env>: resources removed from the template
# are deleted, and nothing the stack manages can be deleted outside it
# (08 §8.5). scripts/teardown.sh removes an environment.
set -euo pipefail

usage() { awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "$0"; exit 2; }
# The positional form (`bootstrap.sh prod [options]`) works for one
# release, with a warning (#277).
if [ $# -ge 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/bootstrap.sh ENV [options]\` is deprecated; use --env (#277)" >&2
  set -- --env "$@"
fi
ENV_NAME="" SUBSCRIPTION="" LOCATION="eastus2" REPO="" DOMAIN="" EMAIL="" INGEST=false CLEANUP=false MOVE=false
while [ $# -gt 0 ]; do
  case "$1" in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --subscription) [ $# -ge 2 ] || usage; SUBSCRIPTION=$2; shift 2 ;;
    --location) [ $# -ge 2 ] || usage; LOCATION=$2; shift 2 ;;
    --repo) [ $# -ge 2 ] || usage; REPO=$2; shift 2 ;;
    --domain) [ $# -ge 2 ] || usage; DOMAIN=$2; shift 2 ;;
    --alert-email) [ $# -ge 2 ] || usage; EMAIL=$2; shift 2 ;;
    --ingest) INGEST=true; shift ;;
    --cleanup-legacy) CLEANUP=true; shift ;;
    --move) MOVE=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
# It becomes part of resource names (the registry allows only lowercase
# letters and digits) and of the CI identity's OIDC subject.
if ! [[ $ENV_NAME =~ ^[a-z][a-z0-9]{0,15}$ ]] || [ "$ENV_NAME" = guardrails ]; then
  echo "error: the environment name must be 1-16 lowercase letters and digits, starting with a letter (not \"guardrails\", the shared policy stack)" >&2
  exit 2
fi

step() { printf '\n==> %s\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }
for tool in az gh curl; do command -v "$tool" > /dev/null || die "$tool is not installed"; done
# Binding a managed certificate (`hostname bind --validation-method`) needs
# az 2.90's built-in commands, or the containerapp extension on older ones.
az containerapp hostname bind --help 2> /dev/null | grep -- --validation-method > /dev/null ||
  az extension add -n containerapp --upgrade --only-show-errors -o none
az containerapp hostname bind --help 2> /dev/null | grep -- --validation-method > /dev/null ||
  die "az containerapp hostname bind has no --validation-method; update az"
cd "$(dirname "$0")/.."

step "Checking sign-ins"
az account show -o none 2> /dev/null || die "run: az login [--tenant TENANT]"
[ -n "$SUBSCRIPTION" ] && az account set --subscription "$SUBSCRIPTION"
SUBSCRIPTION=$(az account show --query id -o tsv)
TENANT=$(az account show --query tenantId -o tsv)
gh auth status > /dev/null 2>&1 || die "run: gh auth login"
[ -n "$REPO" ] || REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
echo "subscription $SUBSCRIPTION (tenant $TENANT), repository $REPO, environment $ENV_NAME"

step "Registering resource providers"
for ns in Microsoft.App Microsoft.ContainerRegistry Microsoft.DocumentDB Microsoft.Web \
  Microsoft.Network Microsoft.Storage Microsoft.OperationalInsights Microsoft.Insights \
  Microsoft.ManagedIdentity Microsoft.Consumption Microsoft.PolicyInsights; do
  [ "$(az provider show -n "$ns" --query registrationState -o tsv 2> /dev/null)" = Registered ] ||
    az provider register -n "$ns" --wait -o none
done

step "Configuring environment settings"
. scripts/lib/env.sh
need_stack_az
aset AZURE_ENV_NAME "$ENV_NAME"
# Settings that describe what exists in one subscription can't carry over to
# another: moving starts again from an empty registry, with no certificate
# and a fresh look at the Cosmos DB free tier.
MOVING=false
previous=$(aget AZURE_SUBSCRIPTION_ID)
if [ -n "$previous" ] && [ "$previous" != "$SUBSCRIPTION" ]; then
  [ "$MOVE" = true ] ||
    die "environment $ENV_NAME is in subscription $previous; pass --move to rebuild it in $SUBSCRIPTION (it starts with no data)"
  MOVING=true
  aset USNM_USE_ACR false
  aset USNM_API_IMAGE ""
  aset USNM_API_CERT_ID ""
  aset USNM_SITE_CERT_ID ""
  aset USNM_WWW_CERT_ID ""
  [ -z "$(aget USNM_BUDGET_START)" ] || aset USNM_BUDGET_START "$(date -u +%Y-%m-01)"
fi
aset AZURE_SUBSCRIPTION_ID "$SUBSCRIPTION"
[ -n "$(aget AZURE_LOCATION)" ] || aset AZURE_LOCATION "$LOCATION"
aset USNM_GITHUB_REPO "$REPO"
aset USNM_GITHUB_REPO_IDS \
  "$(gh api "repos/$REPO" --jq '"\(.owner.login)@\(.owner.id)/\(.name)@\(.id)"')"
[ -n "$DOMAIN" ] && aset USNM_DNS_ZONE "$DOMAIN"
if [ -n "$EMAIL" ]; then
  aset USNM_ALERT_EMAILS "$EMAIL"
  # A budget's start date can never change: set it once.
  [ -n "$(aget USNM_BUDGET_START)" ] || aset USNM_BUDGET_START "$(date -u +%Y-%m-01)"
fi
[ "$INGEST" = true ] && aset USNM_INGEST_JOBS true
# One Cosmos DB free-tier account per subscription: use it only if it's free.
if [ -z "$(aget USNM_COSMOS_FREE_TIER)" ] || [ "$MOVING" = true ]; then
  others=$(az cosmosdb list --query "[?enableFreeTier && !starts_with(name, 'cosmos-usnm-$ENV_NAME-')].name" -o tsv)
  if [ -n "$others" ]; then
    echo "the subscription's free tier is taken by: $others"
    aset USNM_COSMOS_FREE_TIER false
  else
    aset USNM_COSMOS_FREE_TIER true
  fi
fi
USE_ACR=$(aget USNM_USE_ACR)
[ -n "$USE_ACR" ] || { USE_ACR=false; aset USNM_USE_ACR false; }

step "Provisioning"
provision

step "Wiring GitHub Environment '$ENV_NAME' in $REPO"
# Only main may deploy: the CI identity trusts this environment's jobs.
gh api -X PUT "repos/$REPO/environments/$ENV_NAME" --input - > /dev/null << 'EOF'
{"deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}}
EOF
# Exactly one policy may exist: the branch main. Anything else (another
# branch pattern, or a tag named main) would let other refs use the identity.
policies="repos/$REPO/environments/$ENV_NAME/deployment-branch-policies"
has_main=false
while read -r id name type; do
  [ -n "$id" ] || continue
  if [ "$name" = main ] && [ "$type" = branch ] && [ "$has_main" = false ]; then
    has_main=true
  else
    echo "removing deployment policy $type $name"
    gh api -X DELETE "$policies/$id" > /dev/null
  fi
done <<< "$(gh api "$policies" --paginate --jq '.branch_policies[] | "\(.id) \(.name) \(.type // "branch")"')"
[ "$has_main" = true ] || gh api -X POST "$policies" -f name=main -f type=branch > /dev/null
evar() { gh variable set "$1" --env "$ENV_NAME" -R "$REPO" --body "$2"; }
evar USNM_ACR_LOGIN_SERVER "$(aget ACR_LOGIN_SERVER)"
evar USNM_CI_CLIENT_ID "$(aget CI_CLIENT_ID)"
evar AZURE_TENANT_ID "$(aget AZURE_TENANT_ID)"
evar AZURE_SUBSCRIPTION_ID "$(aget AZURE_SUBSCRIPTION_ID)"
evar USNM_RESOURCE_GROUP "$(aget AZURE_RESOURCE_GROUP)"
# List the environment for the deploy workflows.
# Unset means none yet; any other error stops here, since writing the list
# back without the other environments would stop their deploys.
if ! envs=$(gh variable get USNM_DEPLOY_ENVIRONMENTS -R "$REPO" 2>&1); then
  grep -qiE 'not found|HTTP 404' <<< "$envs" ||
    die "can't read USNM_DEPLOY_ENVIRONMENTS in $REPO: $envs"
  envs=""
fi
if ! grep -q "\"$ENV_NAME\"" <<< "$envs"; then
  # A JSON array of names: ["dev","prod"].
  names=$( (tr -d '[]" ' <<< "$envs" | tr ',' '\n'; echo "$ENV_NAME") | grep -v '^$' | sort -u)
  envs="[$(sed 's/.*/"&"/' <<< "$names" | paste -sd, -)]"
  gh variable set USNM_DEPLOY_ENVIRONMENTS -R "$REPO" --body "$envs"
fi
echo "deploy environments: $envs"

# Start a workflow on main and wait for that run to finish. The run is found
# by a unique correlation id, which the workflows put in their run name.
run_and_wait() {
  local workflow=$1; shift
  local correlation
  correlation="bootstrap-$ENV_NAME-$(date -u +%Y%m%dT%H%M%S)-$$"
  gh workflow run "$workflow" -R "$REPO" --ref main -f correlation="$correlation" "$@"
  local id=""
  for _ in $(seq 60); do
    sleep 5
    id=$(gh run list -R "$REPO" --workflow "$workflow" --event workflow_dispatch \
      --limit 20 --json databaseId,displayTitle \
      --jq "[.[] | select(.displayTitle | contains(\"$correlation\"))][0].databaseId // empty")
    [ -n "$id" ] && break
  done
  [ -n "$id" ] || die "the $workflow run did not start"
  gh run watch "$id" -R "$REPO" --exit-status > /dev/null || die "the $workflow run failed: gh run view $id -R $REPO --log-failed"
}

# Always: the registry must hold the current main, not just some earlier build.
step "Publishing main's images to $(aget ACR_NAME) (ci on main)"
run_and_wait ci

if [ "$USE_ACR" != true ]; then
  step "Switching to the private registry"
  aset USNM_USE_ACR true
  aset USNM_API_IMAGE ""
  provision
fi

# The Static Web App that used to serve the site deployed with a shared token
# (ADR-0009). Provisioning never deletes, so remove it here, in every
# environment, whether or not it has a domain.
SWA="swa-usnm-$ENV_NAME"
if az staticwebapp show -n "$SWA" -g "$(aget AZURE_RESOURCE_GROUP)" -o none 2> /dev/null; then
  echo "removing the Static Web App $SWA: the API app serves the site now"
  az staticwebapp delete -n "$SWA" -g "$(aget AZURE_RESOURCE_GROUP)" --yes -o none
fi

# Names on the app (the site and the API), once the registrar delegates the
# domain to the zone.
DOMAIN=$(aget USNM_DNS_ZONE)
if [ -n "$DOMAIN" ]; then
  step "Custom domains for $DOMAIN"
  if ! curl -fsS -H 'accept: application/dns-json' "https://cloudflare-dns.com/dns-query?name=$DOMAIN&type=NS" |
    grep -q azure-dns; then
    echo "$DOMAIN is not delegated to Azure DNS yet. Set its name servers to:"
    echo "  $(aget NAME_SERVERS)"
    echo "then re-run this script to bind the names."
  else
    RG=$(aget AZURE_RESOURCE_GROUP) APP=$(aget API_APP) CAE=$(aget CONTAINER_ENV_NAME)
    names() { tr '\t' '\n' | grep -Fqx "$1"; }
    # The TXT record that validated the Static Web App's apex.
    az network dns record-set txt delete -g "$RG" -z "$DOMAIN" -n _dnsauth --yes -o none 2> /dev/null || true
    # Each name: add it to the app, bind a managed certificate, and record the
    # certificate so provisioning keeps the binding. www and api validate by
    # their CNAMEs; the apex over HTTP, through its A record.
    bound=false
    bind() {
      local host=$1 how=$2 var=$3 cert
      [ -z "$(aget "$var")" ] || return 0
      az containerapp hostname list -n "$APP" -g "$RG" --query "[].name" -o tsv | names "$host" ||
        az containerapp hostname add -n "$APP" -g "$RG" --hostname "$host" -o none
      az containerapp hostname bind -n "$APP" -g "$RG" --hostname "$host" \
        --environment "$CAE" --validation-method "$how" -o none ||
        die "binding $host failed. If its DNS record changed in the last hour, resolvers may still cache the old one; re-run later."
      cert=$(az containerapp show -n "$APP" -g "$RG" \
        --query "properties.configuration.ingress.customDomains[?name=='$host'].certificateId | [0]" -o tsv)
      [ -n "$cert" ] || die "$host has no certificate after binding"
      aset "$var" "$cert"
      bound=true
    }
    bind "api.$DOMAIN" CNAME USNM_API_CERT_ID
    bind "www.$DOMAIN" CNAME USNM_WWW_CERT_ID
    bind "$DOMAIN" HTTP USNM_SITE_CERT_ID
    [ "$bound" = false ] || provision
  fi
fi

evar USNM_API_APP "$(aget API_APP)"
# Variables of the retired web deploy workflow.
for v in USNM_SWA_NAME USNM_API_URL; do
  gh variable delete "$v" --env "$ENV_NAME" -R "$REPO" 2> /dev/null && echo "deleted variable $v" || true
done

step "Checking"
API_URL=$(aget API_URL) SITE=$(aget SITE_URL)
# A replica that has just started is not ready until its startup warm-up
# ends (up to 360 s after loading where min replicas is above 0; 06 §6.6).
curl -fsS --retry 90 --retry-delay 10 --retry-all-errors "$API_URL/readyz" > /dev/null
echo "API  $API_URL  ready"
curl -fsS --retry 10 --retry-delay 6 --retry-all-errors -o /dev/null "$SITE"
echo "site $SITE  up"

if [ "$CLEANUP" = true ]; then
  step "Removing the pre-environment repository settings"
  for v in USNM_ACR_LOGIN_SERVER USNM_CI_CLIENT_ID AZURE_TENANT_ID AZURE_SUBSCRIPTION_ID \
    USNM_API_APP USNM_RESOURCE_GROUP USNM_API_URL; do
    gh variable delete "$v" -R "$REPO" 2> /dev/null && echo "deleted variable $v" || true
  done
  gh secret delete AZURE_STATIC_WEB_APPS_API_TOKEN -R "$REPO" 2> /dev/null &&
    echo "deleted secret AZURE_STATIC_WEB_APPS_API_TOKEN" || true
fi

step "Done"
if [ "$(aget USNM_INGEST_JOBS)" = true ]; then
  echo "Load the corpus (08 §8.9): az containerapp job start -n $(aget BACKFILL_JOB) -g $(aget AZURE_RESOURCE_GROUP)"
fi
