#!/usr/bin/env bash
# Operator script for the BOINC server. Two modes:
#
#   --what-if (the default): prints the plan and the commands; changes nothing and creates
#             no secret. Safe to run anywhere.
#   --apply:  deploys infra/boinc/main.bicep into an EXISTING resource group, generates the
#             secrets locally and writes them to Key Vault. It refuses to run unless the
#             resource group, --subscription and --yes-deploy are all given.
#
#   bootstrap.sh [--what-if] --resource-group RG --image REGISTRY/NAME@sha256:... \
#                --worker-image REGISTRY/NAME@sha256:... --signing-key-dir DIR \
#                --project-url URL --operator-object-id GUID --operator-login NAME \
#                --bridge-image REGISTRY/NAME@sha256:... [--registry-resource-group RG] \
#                [--location eastus] [--name boinc]
#   bootstrap.sh --apply --subscription GUID --yes-deploy  (same options)
#
# It sets up the STOCK BOINC server that PR #441's usnm-boinc bridge submits to; the images
# are built from contrib-compute/images (docs/images.md). What a real run does, in order:
#   1. az deployment group what-if, then create, for infra/boinc/main.bicep (apply does these)
#   2. generate the MySQL and upload-key secrets and store them in Key Vault (apply does this)
#   3. create the stock project: `entrypoint.sh --init-project` inside the app (printed, run by hand)
#   4. install the docker_wrapper app version with #441's scripts/boinc/install.py, sign it
#      OFFLINE, and register it with update_versions (printed, run by hand)
#   5. check that the project's get_project_config.php answers (printed)
#
# Secrets: generated here, kept in a mode-0700 temporary directory that is removed on exit,
# passed to az through mode-0600 files, never put on a command line and never printed. Apply is
# safe to run again: the MySQL admin password is rotated by the redeployment and rewritten to
# Key Vault; the run-time password and the upload keys are created only if absent.
#
# The code-signing PRIVATE key never comes to this script or to Azure. --signing-key-dir
# names a directory on your offline machine (for example a mounted USB stick). The script only
# checks that it holds code_sign_private (existence, never opened) and, in apply, reads the
# PUBLIC half, code_sign_public, into Key Vault. It prints the signing commands for you to run
# there; the signed files (*.sig) come back to this machine.
set -euo pipefail

SCRIPT_VERSION="0.4.0"
# contrib_root is contrib-compute/ (where infra/boinc lives). repo_root is the Git repository
# root, found with physical paths (symlinks resolved) so the offline-key check below compares
# real locations.
contrib_root="$(cd -P "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
repo_root="$(git -C "$contrib_root" rev-parse --show-toplevel 2>/dev/null)" || repo_root="$(cd -P "$contrib_root/.." && pwd -P)"
repo_root="$(cd -P "$repo_root" && pwd -P)"

die() { echo "bootstrap.sh: $*" >&2; exit 1; }
note() { echo "== $*"; }

usage() {
  cat <<'USAGE'
Usage: bootstrap.sh [--what-if | --apply] --resource-group RG --image IMAGE --worker-image IMAGE --signing-key-dir DIR --project-url URL --operator-object-id GUID --operator-login NAME [options]

Mode (default --what-if):
  --what-if              print the plan; change nothing
  --apply                deploy and write secrets. Also needs --subscription and --yes-deploy,
                         and the resource group must already exist

Required:
  --resource-group RG    Azure resource group for the project
  --image IMAGE          server image (images/server), by digest: registry/name@sha256:<64 hex>
  --worker-image IMAGE   the Tesseract worker image from #441's ocr/boinc/Containerfile, by
                         digest; volunteers' Docker pulls it, so it must be in a registry
                         they can reach without credentials (docs/images.md)
  --signing-key-dir DIR  offline directory that holds code_sign_private (checked for, never
                         opened) and code_sign_public (apply copies this one to Key Vault)
  --project-url URL      master URL: https://boinc.usnewsmap.com/ (prod) or
                         https://boinc-dev.usnewsmap.com/ (dev/experiment). It cannot change
                         once volunteers attach. Its host is passed to the template as masterHost
  --operator-object-id GUID
                         Entra object id of the operator (or group) who gets Key Vault Secrets
                         Officer and is the MySQL Entra administrator
  --operator-login NAME  that administrator's Entra sign-in name

Apply only:
  --subscription GUID    subscription to deploy into. Every az call passes it; the default
                         subscription is never used. The script refuses the subscription that
                         holds other production resources (8d356797...)
  --yes-deploy           confirms that you mean to create resources and secrets

Options:
  --bridge-image IMAGE   (required) bridge job image (images/bridge), by digest, same registry as --image
                         (default: --image, which has no usnm-boinc binary)
  --registry-resource-group RG
                         resource group of the image's registry, if not --resource-group
                         (the identity gets AcrPull on that one registry)
  --container-runtime R  docker or podman, for generating the upload keys from the server image
                         (default: whichever is installed)
  --location NAME        Azure region (default eastus)
  --name NAME            short resource name prefix, 3 to 12 lower-case letters/digits (default boinc)
  --help                 this text
  --version              print the script version
USAGE
}

what_if=0 apply=0 confirm=0 subscription="" bridge_image="" runtime="" rg="" image="" worker_image="" key_dir="" location="eastus" name="boinc" project_url="" operator_oid="" operator_login="" registry_rg=""

while [ $# -gt 0 ]; do
  case "$1" in
    --what-if) what_if=1; shift ;;
    --apply) apply=1; shift ;;
    --yes-deploy) confirm=1; shift ;;
    --subscription) [ $# -ge 2 ] || die "$1 needs a value"; subscription="$2"; shift 2 ;;
    --bridge-image) [ $# -ge 2 ] || die "$1 needs a value"; bridge_image="$2"; shift 2 ;;
    --container-runtime) [ $# -ge 2 ] || die "$1 needs a value"; runtime="$2"; shift 2 ;;
    --resource-group) [ $# -ge 2 ] || die "$1 needs a value"; rg="$2"; shift 2 ;;
    --image) [ $# -ge 2 ] || die "$1 needs a value"; image="$2"; shift 2 ;;
    --worker-image) [ $# -ge 2 ] || die "$1 needs a value"; worker_image="$2"; shift 2 ;;
    --signing-key-dir) [ $# -ge 2 ] || die "$1 needs a value"; key_dir="$2"; shift 2 ;;
    --location) [ $# -ge 2 ] || die "$1 needs a value"; location="$2"; shift 2 ;;
    --name) [ $# -ge 2 ] || die "$1 needs a value"; name="$2"; shift 2 ;;
    --project-url) [ $# -ge 2 ] || die "$1 needs a value"; project_url="$2"; shift 2 ;;
    --operator-object-id) [ $# -ge 2 ] || die "$1 needs a value"; operator_oid="$2"; shift 2 ;;
    --operator-login) [ $# -ge 2 ] || die "$1 needs a value"; operator_login="$2"; shift 2 ;;
    --registry-resource-group) [ $# -ge 2 ] || die "$1 needs a value"; registry_rg="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    --version) echo "$SCRIPT_VERSION"; exit 0 ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
done

[ "$what_if" -eq 0 ] || [ "$apply" -eq 0 ] || die "--what-if and --apply are exclusive"
if [ "$apply" -eq 1 ]; then
  [ -n "$rg" ] || die "--apply needs --resource-group"
  [ -n "$subscription" ] || die "--apply needs --subscription GUID (the default subscription is never used)"
  [ "$confirm" -eq 1 ] || die "--apply needs --yes-deploy, to confirm that resources and secrets will be created"
  [[ "$subscription" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]] || die "--subscription must be a GUID"
  # Compare in lower case, so 8D356797-... cannot slip past the guard. The pattern above already
  # excludes braces, URNs and padding, so a GUID in any other spelling is rejected, not matched.
  subscription="$(printf '%s' "$subscription" | tr '[:upper:]' '[:lower:]')"
  case "$subscription" in 8d356797-*) die "refusing subscription 8d356797...: it holds other production resources" ;; esac
else
  [ -z "$subscription" ] && [ "$confirm" -eq 0 ] || die "--subscription and --yes-deploy belong to --apply"
fi
[ -n "$rg" ] || die "--resource-group is required"
[ -n "$image" ] || die "--image is required"
[ -n "$worker_image" ] || die "--worker-image is required"
[ -n "$key_dir" ] || die "--signing-key-dir is required"
[ -n "$bridge_image" ] || die "--bridge-image is required: the server image has no usnm-boinc binary, so the bridge job would fail (build images/bridge from crates/usnm-boinc)"
for img in "$image" "$worker_image" "$bridge_image"; do
  # exactly "<repository>@sha256:" and 64 lower-case hex characters, nothing after
  [[ "$img" =~ ^[^@[:space:]]+@sha256:[0-9a-f]{64}$ ]] || die "images must be pinned by digest: name@sha256:<64 lower-case hex>"
done
case "$name" in *[!a-z0-9]*|'') die "--name must be lower-case letters and digits" ;; esac
[ "${#name}" -ge 3 ] && [ "${#name}" -le 12 ] || die "--name must be 3 to 12 characters"
[ -n "$project_url" ] || die "--project-url is required"
case "$project_url" in
  https://boinc.usnewsmap.com/) domain_env=prod ;;
  https://boinc-dev.usnewsmap.com/) domain_env=dev ;;
  *) die "--project-url must be https://boinc.usnewsmap.com/ or https://boinc-dev.usnewsmap.com/ (https, trailing slash)" ;;
esac
domain="${project_url#https://}"; domain="${domain%/}"
[ -n "$operator_oid" ] || die "--operator-object-id is required"
[[ "$operator_oid" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]] || die "--operator-object-id must be a GUID"
[ -n "$operator_login" ] || die "--operator-login is required"
[[ "$operator_login" =~ ^[[:print:]]+$ ]] || die "--operator-login must be a single-line Entra user name or group display name"
[ -n "$registry_rg" ] || registry_rg="$rg"
# The registry is named by the image: <registry>.azurecr.io/<repository>@sha256:...
registry_host="${image%%/*}"
case "$registry_host" in *.azurecr.io) ;; *) die "--image must be in an Azure Container Registry (<name>.azurecr.io/...): the app pulls with its identity" ;; esac
[ "${bridge_image%%/*}" = "$registry_host" ] || die "--bridge-image must be in the same registry as --image ($registry_host)"

# The signing key must be offline: refuse a path inside this repository.
# Compare real paths: the directory with symlinks resolved, and the key file itself (a symlink
# that points back into the repository is refused too).
key_abs="$(cd -P "$key_dir" 2>/dev/null && pwd -P)" || die "--signing-key-dir $key_dir does not exist"
[ -f "$key_abs/code_sign_private" ] || die "no code_sign_private in $key_abs"
key_real="$(realpath "$key_abs/code_sign_private")" || die "cannot resolve $key_abs/code_sign_private"
for p in "$key_abs" "$key_real"; do
  case "$p/" in "$repo_root"/*) die "--signing-key-dir is inside this repository ($repo_root); use an offline location" ;; esac
done
[ "$apply" -eq 0 ] || [ -f "$key_abs/code_sign_public" ] || die "no code_sign_public in $key_abs"
if [ -n "$(find "$key_abs/code_sign_private" -perm -004 2>/dev/null)" ]; then
  echo "bootstrap.sh: warning: code_sign_private is readable by everyone; chmod 600 it" >&2
fi

az_() { az "$@" --subscription "$subscription" --only-show-errors; }

apply_all() {
  command -v az >/dev/null || die "az is not installed"
  command -v openssl >/dev/null || die "openssl is not installed"
  command -v jq >/dev/null || die "jq is not installed"
  [ "$(az account show --subscription "$subscription" --query id -o tsv --only-show-errors 2>/dev/null)" = "$subscription" ] ||
    die "az cannot see subscription $subscription (az login first)"
  az_ group show --name "$rg" --query name -o none 2>/dev/null || die "resource group $rg does not exist in that subscription; create it first (this script does not)"
  if [ -z "$runtime" ]; then
    for r in docker podman; do command -v "$r" >/dev/null && { runtime="$r"; break; }; done
  fi
  [ -n "$runtime" ] || die "no container runtime found (docker or podman); needed to generate the upload keys"

  work="$(mktemp -d)"; chmod 700 "$work"
  trap 'rm -rf "$work"' EXIT
  umask 077
  genpw() { printf 'Aa1%s' "$(openssl rand -hex 20)"; }

  note "A. Deployment: what-if, then create (resource group $rg)"
  genpw > "$work/admin-pw"
  jq -n --rawfile pw "$work/admin-pw" --arg n "$name" --arg l "$location" --arg i "$image" --arg b "$bridge_image" \
        --arg h "$domain" --arg r "$registry_rg" --arg o "$operator_oid" '{
    "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentParameters.json#",
    contentVersion: "1.0.0.0",
    parameters: {
      name: {value: $n}, location: {value: $l}, image: {value: $i}, bridgeImage: {value: $b},
      masterHost: {value: $h}, registryResourceGroup: {value: $r}, operatorPrincipalIds: {value: [$o]},
      mysqlAdminPassword: {value: $pw}
    }}' > "$work/params.json"
  az_ deployment group what-if --resource-group "$rg" --template-file "$contrib_root/infra/boinc/main.bicep" \
      --parameters "@$work/params.json" --no-pretty-print --query 'properties.changes[].{op:changeType,id:resourceId}' -o tsv ||
    die "what-if failed (a policy denial from the guardrails shows here; see ADR-0019 decision 5)"
  deployment="usnm-$name-$(date -u +%Y%m%d%H%M%S)"
  az_ deployment group create --resource-group "$rg" --name "$deployment" \
      --template-file "$contrib_root/infra/boinc/main.bicep" --parameters "@$work/params.json" -o none
  vault="$(az_ deployment group show --resource-group "$rg" --name "$deployment" --query properties.outputs.keyVaultName.value -o tsv)"
  fqdn="$(az_ deployment group show --resource-group "$rg" --name "$deployment" --query properties.outputs.appFqdn.value -o tsv)"
  [ -n "$vault" ] || die "the deployment returned no keyVaultName"

  note "B. Secrets into Key Vault $vault (values are never printed)"
  # One successful listing is the only evidence of absence: a failed lookup (throttling, a
  # network or permission error) must never read as "missing", or an existing runtime password
  # or upload key pair would be overwritten.
  existing="$(az_ keyvault secret list --vault-name "$vault" --query '[].name' -o tsv)" ||
    die "cannot list secrets in Key Vault $vault: it is private-endpoint only. The deployment is done and the MySQL admin password was set by it; run this script again from a machine with a network path into the VNet (it redeploys and rotates the admin password)"
  put() { az_ keyvault secret set --vault-name "$vault" --name "$1" --file "$2" --query id -o none; }
  have() { printf '%s\n' "$existing" | grep -qxF -- "$1"; }
  put mysql-admin-password "$work/admin-pw"
  have mysql-runtime-password || { genpw > "$work/runtime-pw"; put mysql-runtime-password "$work/runtime-pw"; }
  if have upload-private-key || have upload-public-key; then
    have upload-private-key && have upload-public-key ||
      die "only one of upload-private-key and upload-public-key exists in $vault; refusing to overwrite it. Resolve by hand"
  else
    az_ acr login --name "${registry_host%%.*}" -o none || die "az acr login failed for ${registry_host%%.*}"
    mkdir "$work/keys"
    # Files must belong to the invoking user, or az cannot read them below. Docker: --user.
    # Podman rootless: --userns=keep-id maps the invoking user in (a plain --user would be a
    # subordinate uid on the host). UNVERIFIED for Podman.
    local own=(--user "$(id -u):$(id -g)")
    case "$runtime" in podman) own=(--userns=keep-id --user "$(id -u):$(id -g)") ;; esac
    "$runtime" run --rm "${own[@]}" -v "$work/keys:/out" "$image" --make-upload-keys --dir /out > /dev/null ||
      die "could not run $image to generate the upload keys"
    put upload-private-key "$work/keys/upload_private"
    put upload-public-key "$work/keys/upload_public"
  fi
  put code-sign-public-key "$key_abs/code_sign_public"
  echo "   done"

  note "C. Next, by hand (the rest of the plan below; nothing else is automatic)"
  echo "   App FQDN for the DNS record: $fqdn"
  echo "   Project init, DNS, app version and signing: steps 1b, 3, 4, 5 below."
  echo
}

if [ "$apply" -eq 1 ]; then
  apply_all
fi

if [ "$apply" -eq 1 ]; then note "the plan, for the steps that stay by hand"; else note "plan only; nothing below is executed"; fi
echo
note "1. Preview the Azure deployment (read-only; run it yourself)"
cat <<PLAN
az deployment group what-if \\
  --resource-group $rg \\
  --template-file $contrib_root/infra/boinc/main.bicep \\
  --parameters name=$name location=$location image=$image bridgeImage=$bridge_image \\
               masterHost=$domain registryResourceGroup=$registry_rg \\
               operatorPrincipalIds='["$operator_oid"]' \\
               mysqlAdminPassword=<generated, from Key Vault>
   The registry is ${registry_host%%.*} (the identity gets AcrPull on it only). Key Vault and the
   Files account are private-endpoint only; run step 2 from a machine with a network path into the VNet.
PLAN
echo
note "1a. MySQL Entra administrator (real run, after the deployment; not in the template)"
printf '   The server needs a user-assigned identity that holds Directory Readers (or Graph\n   permissions) first; a template cannot grant that. Then:\n'
printf '       az mysql flexible-server ad-admin create --resource-group %q --server-name <mysql server> \\\n         --display-name %q --object-id %q --identity <user-assigned identity resource id>\n' "$rg" "$operator_login" "$operator_oid"
echo
note "1b. Custom domain $domain (real run; this script touches no DNS)"
label="${domain%%.*}"
cat <<PLAN
   In the usnewsmap.com zone:
       CNAME  $label  ->  <Container App FQDN, from the deployment output>
       TXT    asuid.$label  ->  <customDomainVerificationId of the Container Apps environment>
   Then bind $domain to the app with a Container Apps managed certificate.
   UNVERIFIED that a managed certificate works with this environment (it needs the CNAME to
   resolve publicly before validation). If it does not, bring a certificate from Key Vault.
   The master URL cannot change after volunteers attach, so settle it before the first volunteer.
PLAN
if [ "$domain_env" = dev ]; then
  echo "   dev rule: $domain holds no resources unless a focused experiment is running; tear it all down after."
fi
echo
note "2. Secrets (apply does this): generated locally, written straight to Key Vault, never printed"
echo "   mysql-admin-password, mysql-runtime-password, upload-private-key and upload-public-key (made by"
echo "   the server image's crypt_prog, run locally), and code-sign-public-key (the PUBLIC half, copied from"
echo "   $key_abs/code_sign_public). Key Vault is private-endpoint only: run apply from a machine with a"
echo "   network path into the VNet (design.md, open question 11), or the writes fail and you re-run apply."
echo
note "3. The stock project (real run, by hand, inside the Container App)"
cat <<PLAN
   az containerapp exec --subscription <sub> --resource-group $rg --name usnm-$name-app \\
     --command "/usr/local/lib/usnm-boinc/entrypoint.sh --init-project"
   It runs the stock make_project (creates the schema and the project tree on the share, with its
   key directory on tmpfs so no code-signing key is kept), creates the run-time MySQL user, renders
   config.xml from the template with the daemons from ocr/boinc/daemons.xml, runs bin/xadd with
   boinc/project.xml (app usnm_ocr) and installs the 'docker' plan class. It refuses to run twice.
   The spool of finished results appears under <project>/results/<batch>/ on the Files share.
   UNVERIFIED end to end; see docs/images.md.
PLAN
echo
note "4. App version and code signing (real run; the private key stays offline)"
cat <<PLAN
   install.py and ocr/boinc/ are PR #441's, so run them from a checkout of that code on a trusted
   machine, against a scratch project directory that holds an empty config.xml (install.py only checks
   that it exists). The docker_wrapper is the one the server image was built with:
       docker create --name w $image && docker cp w:/usr/local/lib/usnm-boinc/docker_wrapper ./docker_wrapper
       mkdir -p scratch && touch scratch/config.xml
       python3 scripts/boinc/install.py --project scratch --wrapper ./docker_wrapper \\
         --image $worker_image --version 1.0
   It writes scratch/apps/usnm_ocr/1.0/x86_64-pc-linux-gnu__docker and scratch/templates/. It refuses
   an existing version directory. On the OFFLINE machine ($key_abs),
   for each file F in that version directory:
       sign_executable F $key_abs/code_sign_private > F.sig
   (sign_executable is bin/sign_executable from the BOINC build; the server image has it at
   /usr/local/boinc/tools/sign_executable.) Copy scratch/apps/usnm_ocr and scratch/templates, with the
   .sig files, to <project>/apps/usnm_ocr and <project>/templates on the Files share, then in the app:
       cd <project> && bin/update_versions
   If update_versions finds no code_sign_private, it uses the .sig files. That is the intended path.
   UNVERIFIED: the copy to the share needs the same network path as the Key Vault writes.
PLAN
echo
note "5. Check (real run)"
url="$project_url"
echo "   curl -fsS ${url}usnmocr/get_project_config.php"
echo "   then submit one reserved job with the bridge job (docs/design.md section 4) and watch it finish"
echo
if [ "$apply" -eq 1 ]; then
  note "done: deployed and secrets written. Do the manual steps above; do not re-run --apply (it rotates the MySQL admin password)."
else
  note "done: previewed only. Re-run with --apply --subscription GUID --yes-deploy to deploy."
fi
