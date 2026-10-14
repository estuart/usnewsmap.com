#!/usr/bin/env bash
# Deploy the BOINC server (ADR-0019) from its committed settings, as scripts/provision.sh
# deploys the app:
#
#   # edit contrib-compute/infra/boinc/env/prod.json, open a pull request, merge, pull
#   scripts/provision-boinc.sh --env prod --what-if
#   scripts/provision-boinc.sh --env prod
#
# It deploys contrib-compute/infra/boinc/main.bicep as the deployment stack usnm-boinc-<env>
# into the existing group usnm-boinc-rg-<env> (the main stack creates it, with the guardrails
# and the two waivers of ADR-0019 decision 5), what-if first. It refuses while the settings
# file or its schema has uncommitted changes; --allow-dirty deploys them anyway, in dev only.
#
# First run: with no images in the settings it deploys the base resources (registry, network,
# vault, storage, database, monitoring). Then scripts/build-boinc-images.sh --env <env> builds
# the server and bridge images in Azure and writes their digests to the settings; commit them
# and run this again to deploy the app and the jobs.
#
# Secrets, none of which pass through this machine:
# - mysql-admin-password: a new random value made inside each deployment (main.bicep's
#   parameter default) and written to Key Vault by the same deployment;
# - mysql-runtime-password and the upload key pair: created inside the VNet by the secrets job
#   (infra/boinc/modules/secrets-job.bicep), which this script starts after each deployment.
#   Each is created only if absent, and a failed lookup stops the job;
# - code-sign-public-key: the PUBLIC half of the offline code-signing key, from the settings.
#   The private half never comes here or to Azure.
# The steps that stay by hand (project init, DNS and certificate, the app version and its
# offline signing, the check) are printed at the end.
#
# --what-if changes nothing: Azure's preflight validation of the stack, then the template's
# changes. Needs az 2.61+ and jq, signed in to the settings' subscription with Owner on the
# BOINC group and on the shared registry and DNS zone's group (infra/names/<env>.json rg): the
# stack writes role assignments in both, and the custom domain's records in the zone.
set -euo pipefail
usage() { echo "usage: scripts/provision-boinc.sh --env <prod|dev> [--publish-image REF] [--allow-dirty] [--what-if]" >&2; exit 2; }
ENV_NAME="" ALLOW_DIRTY=false WHAT_IF=false PUBLISH_IMAGE=""
PUBLISH_OVERRIDE=""
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --allow-dirty) ALLOW_DIRTY=true; shift ;;
    --what-if) WHAT_IF=true; shift ;;
    # Override images.appver (the publish job's image) for this deployment only.
    --publish-image) [ $# -ge 2 ] || usage; PUBLISH_OVERRIDE=$2; shift 2 ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || usage
die() { echo "error: $*" >&2; exit 1; }
cd "$(dirname "$0")/.."
# shellcheck source=scripts/lib/boinc.sh
. scripts/lib/boinc.sh
boinc_clean_check
boinc_load_settings
boinc_az_check
# The publish job (main.bicep) runs the last published app version, images.appver.
PUBLISH_IMAGE=${PUBLISH_OVERRIDE:-$APPVER_IMAGE}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# No secret goes in the parameters: the template makes the admin password itself.
names_file="$(dirname "$0")/../infra/names/$ENV_NAME.json"
REGISTRY_NAME=$(jq -er .registry "$names_file") || die "no registry in $names_file"
REGISTRY_RG=$(jq -er .rg "$names_file") || die "no rg in $names_file"
# The custom domain (MASTER_HOST in the app's zone, infra/env/<env>.json USNM_DNS_ZONE) goes on
# in stages, because each needs the last: 0, the DNS records (CNAME and asuid TXT, written by
# this stack into the zone's group); 1, the name on the app unbound and its managed
# certificate; 2, the name bound with the certificate. domain_stage reads where it is.
DNS_ZONE=$(jq -r '.USNM_DNS_ZONE // empty' "$(dirname "$0")/../infra/env/$ENV_NAME.json")
[ -z "$DNS_ZONE" ] || [[ $MASTER_HOST == *".$DNS_ZONE" ]] || die "$MASTER_HOST is not in the zone $DNS_ZONE"
CERT_NAME="cert-${MASTER_HOST//./-}"
domain_stage() {
  local record=${MASTER_HOST%".$DNS_ZONE"} state names
  [ -n "$DNS_ZONE" ] && [ -n "$SERVER_IMAGE" ] || { echo 0; return; }
  # No app in the stack yet (a base-only deploy): no environment, so no certificate to read.
  [ -n "$(boinc_output appFqdn)" ] || { echo 0; return; }
  # A failed read must stop the run: guessing a lower stage would unbind a working domain.
  state=$(baz containerapp env certificate list --name "usnm-boinc-cae-$ENV_NAME" --resource-group "$BOINC_RG" \
    --managed-certificates-only --query "[?name=='$CERT_NAME'].properties.provisioningState | [0]" -o tsv) ||
    die "could not read the managed certificates of usnm-boinc-cae-$ENV_NAME"
  if [ "$state" = Succeeded ]; then echo 2; return; fi
  names=$(baz network dns record-set cname list --zone-name "$DNS_ZONE" --resource-group "$REGISTRY_RG" --query '[].name' -o tsv) ||
    die "could not read the CNAME records of $DNS_ZONE"
  if grep -qx "$record" <<< "$names"; then echo 1; else echo 0; fi
}
# write_params STAGE: the deployment parameters, with the custom domain stage.
write_params() {
jq -n --argjson assess "$ASSESSMENT" --arg zone "$DNS_ZONE" --arg zrg "$REGISTRY_RG" --argjson stage "$1" --arg rn "$REGISTRY_NAME" --arg rg "$REGISTRY_RG" --arg env "$ENV_NAME" --arg l "$LOCATION" --arg i "$SERVER_IMAGE" --arg b "$BRIDGE_IMAGE" --arg p "$PUBLISH_IMAGE" \
  --arg h "$MASTER_HOST" --arg o "$OPERATOR_ID" --rawfile s "$BOINC_SETTINGS" '{
  "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentParameters.json#",
  contentVersion: "1.0.0.0",
  parameters: {
    env: {value: $env}, assessment: {value: $assess}, registryName: {value: $rn}, registryResourceGroup: {value: $rg}, location: {value: $l}, image: {value: $i}, bridgeImage: {value: $b}, publishImage: {value: $p},
    masterHost: {value: $h}, operatorPrincipalIds: {value: [$o]},
    codeSignPublicKey: {value: ($s | fromjson | .codeSignPublicKey)},
    dnsZoneName: {value: $zone}, dnsZoneResourceGroup: {value: $zrg}, customDomainStage: {value: $stage}
  }}' > "$work/params.json"
}
STAGE=$(domain_stage) || exit 1
write_params "$STAGE"

# The images must be in the shared registry: the app and the jobs pull with their identities,
# which this stack gives AcrPull there only.
registry="$REGISTRY_NAME.azurecr.io"
[ -z "$PUBLISH_IMAGE" ] || [ -n "$SERVER_IMAGE" ] || die "the publish image needs the app: set the server and bridge images first"
[ -z "$PUBLISH_IMAGE" ] || [[ $PUBLISH_IMAGE =~ ^[a-z0-9.-]+\.azurecr\.io/usnm-boinc-appver@sha256:[0-9a-f]{64}$ ]] ||
  die "the publish image must be <registry>.azurecr.io/usnm-boinc-appver@sha256:<64 hex>"
if [ -n "$SERVER_IMAGE" ]; then
  for img in "$SERVER_IMAGE" "$BRIDGE_IMAGE" ${PUBLISH_IMAGE:+"$PUBLISH_IMAGE"}; do
    [ "${img%%/*}" = "$registry" ] || die "$img is not in the shared registry $registry"
  done
fi

stack_args=(--name "$BOINC_STACK" --resource-group "$BOINC_RG"
  --template-file "$BOINC_DIR/main.bicep" --parameters "@$work/params.json"
  --action-on-unmanage deleteResources --deny-settings-mode denyDelete
  # Private endpoints manage their own A records in the stack's DNS zones; denyDelete
  # would otherwise block that and leave a zone group with no record.
  --deny-settings-excluded-actions
  "Microsoft.Network/privateDnsZones/A/delete Microsoft.Network/privateEndpoints/privateDnsZoneGroups/delete"
  --description "usnewsmap BOINC server $ENV_NAME (scripts/provision-boinc.sh)")

echo "validating stack $BOINC_STACK (preflight; a guardrail denial fails here)"
baz stack group validate "${stack_args[@]}" -o none || die "the stack $BOINC_STACK fails preflight"
echo "stack $BOINC_STACK passes preflight; its changes (a deployment's what-if: it doesn't list what the stack deletes because the template dropped it):"
baz deployment group what-if --resource-group "$BOINC_RG" --template-file "$BOINC_DIR/main.bicep" \
  --parameters "@$work/params.json" || die "the what-if of $BOINC_STACK failed"
if $WHAT_IF; then
  [ -n "$SERVER_IMAGE" ] || echo "no images in $BOINC_SETTINGS yet: this deploys the base resources only; then scripts/build-boinc-images.sh --env $ENV_NAME"
  exit 0
fi

echo "deploying stack $BOINC_STACK (custom domain stage $STAGE)"
baz stack group create "${stack_args[@]}" --yes -o none || die "the stack $BOINC_STACK failed"
echo "deployed stack $BOINC_STACK"
# Step the custom domain on: each pass enables the next stage, at most twice more.
for _ in 1 2; do
  next=$(domain_stage) || exit 1
  [ "$next" -gt "$STAGE" ] || break
  STAGE=$next
  write_params "$STAGE"
  echo "deploying stack $BOINC_STACK again for custom domain stage $STAGE"
  baz stack group create "${stack_args[@]}" --yes -o none || die "the stack $BOINC_STACK failed at custom domain stage $STAGE"
done
[ -z "$DNS_ZONE" ] || [ -z "$SERVER_IMAGE" ] || echo "custom domain $MASTER_HOST: stage $STAGE of 2"

if [ -z "$SERVER_IMAGE" ]; then
  echo "base resources deployed. Next: scripts/build-boinc-images.sh --env $ENV_NAME, commit $BOINC_SETTINGS, then run this again."
  exit 0
fi

# The secrets job, inside the VNet. It changes nothing when every secret exists.
secrets_job=$(boinc_output secretsJobName)
[ -n "$secrets_job" ] || die "the stack has no secretsJobName output"
running=$(baz containerapp job execution list --name "$secrets_job" --resource-group "$BOINC_RG" \
  --query "[?properties.status=='Running'].name" -o tsv) || die "could not list the executions of $secrets_job"
[ -z "$running" ] || die "$secrets_job is already running ($running); wait for it, so two runs can't interleave the upload key pair"
echo "starting $secrets_job (creates the absent secrets inside the VNet; values never leave it)"
execution=$(baz containerapp job start --name "$secrets_job" --resource-group "$BOINC_RG" --query name -o tsv) ||
  die "could not start $secrets_job"
status=""
for _ in $(seq 1 60); do
  status=$(baz containerapp job execution show --name "$secrets_job" --resource-group "$BOINC_RG" \
    --job-execution-name "$execution" --query properties.status -o tsv) || die "could not read $execution"
  case "$status" in Succeeded | Failed | Stopped | Degraded) break ;; esac
  sleep 10
done
[ "$status" = Succeeded ] ||
  die "$secrets_job execution $execution ended $status: see its logs in Log Analytics (ContainerAppConsoleLogs_CL, ContainerJobName_s == \"$secrets_job\")"
echo "secrets complete ($execution)"

fqdn=$(boinc_output appFqdn)
cat << STEPS

== Done by this script: the stack, the secrets. Next, by hand, once each:

1. Project init (creates the schema and the project tree on the share; refuses to run twice).
   It runs the server image's entrypoint in its own job, with the bridge's share, database
   and vault settings:
       az containerapp job start --subscription $SUBSCRIPTION --resource-group $BOINC_RG --name usnm-boinc-init-caj-$ENV_NAME
   Until it has run, the app answers its probe with a placeholder.

2. Custom domain $MASTER_HOST: this script writes its DNS records into the app's zone and binds
   it with a managed certificate (stage $STAGE of 2 above; run it again if short of 2). The master
   URL cannot change after volunteers attach.

3. App version and code signing (the private key stays on the signing host), one command:
       scripts/boinc-app-release.sh --env $ENV_NAME --app usnm_ocr --version 1.0 --signing-host <host>
   It needs the worker image in images.worker of $BOINC_SETTINGS and the PUBLIC key in
   codeSignPublicKey (contrib-compute/docs/app-release.md).

4. Check:
       curl -fsS https://$fqdn/usnmocr/get_project_config.php
       curl -fsS ${PROJECT_URL}usnmocr/get_project_config.php    (after step 2)
   then submit one reserved job with the bridge job (contrib-compute/docs/design.md, section 4).

5. Optional: the MySQL Entra administrator ($OPERATOR_LOGIN, $OPERATOR_ID) needs a server
   identity with Directory Readers first, which a template cannot grant (design.md, 5.1).
STEPS
