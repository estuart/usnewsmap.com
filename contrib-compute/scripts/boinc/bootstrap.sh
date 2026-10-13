#!/usr/bin/env bash
# Operator script for the BOINC server: plans the Bicep deployment and the steps that follow
# it. In this version it can only preview. Nothing is deployed and no secret is created;
# --what-if is required, and the script refuses to run without it.
#
#   bootstrap.sh --what-if --resource-group RG --image REGISTRY/NAME@sha256:... \
#                --worker-image REGISTRY/NAME@sha256:... --signing-key-dir DIR \
#                --operator-object-id GUID --operator-login NAME \
#                [--registry-resource-group RG] [--location eastus] [--name boinc]
#
# It sets up the STOCK BOINC server that PR #441's usnm-boinc bridge submits to; it builds no
# BOINC code. What a real run will do, in order (printed below as the plan):
#   1. az deployment group what-if / create for infra/boinc/main.bicep
#   2. generate the MySQL and upload-key secrets and store them in Key Vault
#   3. create the stock project (make_project on first run, `upgrade` on later runs), register
#      the app usnm_ocr, install the plan class, and merge #441's daemons.xml
#   4. install the docker_wrapper app version with #441's scripts/boinc/install.py, sign it
#      OFFLINE, and register it with update_versions
#   5. check that the project's get_project_config.php answers
#
# The code-signing PRIVATE key never comes to this script or to Azure. --signing-key-dir
# names a directory on your offline machine (for example a mounted USB stick). The script
# only checks that it holds code_sign_private and prints the signing commands for you to run
# there; the signed files (*.sig) come back to this machine.
set -euo pipefail

SCRIPT_VERSION="0.3.0"
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
Usage: bootstrap.sh --what-if --resource-group RG --image IMAGE --worker-image IMAGE --signing-key-dir DIR --project-url URL --operator-object-id GUID --operator-login NAME [options]

Required:
  --what-if              preview only. This version cannot deploy, so the flag is mandatory
  --resource-group RG    Azure resource group for the project
  --image IMAGE          server image, by digest: registry/name@sha256:<64 hex>
  --worker-image IMAGE   the Tesseract worker image from #441's ocr/boinc/Containerfile, by
                         digest; volunteers' Docker pulls it, so it must be in a registry
                         they can reach
  --signing-key-dir DIR  offline directory that holds code_sign_private (it is only checked
                         for, never read or copied)

  --project-url URL      master URL: https://boinc.usnewsmap.com/ (prod) or
                         https://boinc-dev.usnewsmap.com/ (dev/experiment). It cannot change
                         once volunteers attach. Its host is passed to the template as masterHost
  --operator-object-id GUID
                         Entra object id of the operator (or group) who gets Key Vault Secrets
                         Officer and is the MySQL Entra administrator
  --operator-login NAME  that administrator's Entra sign-in name

Options:
  --registry-resource-group RG
                         resource group of the image's registry, if not --resource-group
                         (the identity gets AcrPull on that one registry)
  --location NAME        Azure region (default eastus)
  --name NAME            short resource name prefix, 3 to 12 lower-case letters/digits (default boinc)
  --help                 this text
  --version              print the script version
USAGE
}

what_if=0 rg="" image="" worker_image="" key_dir="" location="eastus" name="boinc" project_url="" operator_oid="" operator_login="" registry_rg=""

while [ $# -gt 0 ]; do
  case "$1" in
    --what-if) what_if=1; shift ;;
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

[ "$what_if" -eq 1 ] || die "this version only previews: pass --what-if. Nothing is deployed."
[ -n "$rg" ] || die "--resource-group is required"
[ -n "$image" ] || die "--image is required"
[ -n "$worker_image" ] || die "--worker-image is required"
[ -n "$key_dir" ] || die "--signing-key-dir is required"
for img in "$image" "$worker_image"; do
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

# The signing key must be offline: refuse a path inside this repository.
# Compare real paths: the directory with symlinks resolved, and the key file itself (a symlink
# that points back into the repository is refused too).
key_abs="$(cd -P "$key_dir" 2>/dev/null && pwd -P)" || die "--signing-key-dir $key_dir does not exist"
[ -f "$key_abs/code_sign_private" ] || die "no code_sign_private in $key_abs"
key_real="$(realpath "$key_abs/code_sign_private")" || die "cannot resolve $key_abs/code_sign_private"
for p in "$key_abs" "$key_real"; do
  case "$p/" in "$repo_root"/*) die "--signing-key-dir is inside this repository ($repo_root); use an offline location" ;; esac
done
if [ -n "$(find "$key_abs/code_sign_private" -perm -004 2>/dev/null)" ]; then
  echo "bootstrap.sh: warning: code_sign_private is readable by everyone; chmod 600 it" >&2
fi

note "plan only; nothing below is executed"
echo
note "1. Preview the Azure deployment (read-only; run it yourself)"
cat <<PLAN
az deployment group what-if \\
  --resource-group $rg \\
  --template-file $contrib_root/infra/boinc/main.bicep \\
  --parameters name=$name location=$location image=$image \\
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
note "2. Secrets (real run): generated locally, written straight to Key Vault, never printed"
echo "   mysql-admin-password, mysql-runtime-password, and the upload key pair (make_keys output)"
echo "   Key Vault name comes from the deployment output keyVaultName."
echo
note "3. The stock project (real run, inside the Container App via az containerapp exec)"
cat <<PLAN
   first run:  make_project --no_query --db_host <mysql host> ... usnmocr   (creates the schema)
   later runs: bin/upgrade   (schema updates from a newer BOINC release)
   then:       bin/xadd < boinc/project.xml          (registers app usnm_ocr; target_nresults 1)
               cp boinc/plan_class_spec.xml <project>/plan_class_spec.xml   (the 'docker' class)
   daemons:    paste the <daemon> entries of ocr/boinc/daemons.xml (PR #441) into config.xml
               in place of @APP_DAEMONS@: sample_trivial_validator and batch_collect_assimilator
   The spool of finished results appears under <project>/results/<batch>/ on the Files share.
PLAN
echo
note "4. App version and code signing (real run; the private key stays offline)"
cat <<PLAN
   In the container, with the docker_wrapper binary of a trusted BOINC release at WRAPPER:
       python3 scripts/boinc/install.py --project <project dir> --wrapper WRAPPER \\
         --image $worker_image --version 1.0
   (that script is PR #441's; it refuses an existing version directory and writes
   apps/usnm_ocr/1.0/x86_64-pc-linux-gnu__docker). On the OFFLINE machine ($key_abs),
   for each file F in that directory:
       bin/sign_executable F $key_abs/code_sign_private > F.sig
   Copy the .sig files back beside the files, then in the container:
       bin/update_versions
   If update_versions finds no code_sign_private, it uses the .sig files. That is the intended path.
PLAN
echo
note "5. Check (real run)"
url="$project_url"
echo "   curl -fsS ${url}usnmocr/get_project_config.php"
echo "   then submit one reserved job with the bridge job (docs/design.md section 4) and watch it finish"
echo
note "done: previewed only. Deploy with az yourself when the open questions in docs/design.md (section 9) are settled."
