#!/usr/bin/env bash
# Fetch a RHEL image onto a Proxmox host through Red Hat's image API (#339),
# from the operator's machine, for proxmox-create-vm.sh --image-file.
#
#   scripts/ocr-node/rhel-image.sh --env prod --host root@pve.example.org \
#     --image-sha256 <SHA-256 from Red Hat's download page> \
#     --file-name rhel-10.2-x86_64-kvm.qcow2 \
#     [--dest /var/lib/vz/template/cache] [--ssh-key FILE] [--dry-run]
#
# Reads rhel-offline-token from the nodes' Key Vault (OCR_KEYVAULT) with
# your sign-in (Key Vault Secrets Officer from USNM_OCR_KEYVAULT_ADMINS,
# from an address in USNM_OCR_NODE_IPS) and sends it to the host on ssh's
# stdin. On the host, as root: exchanges it at Red Hat's SSO for a
# short-lived access token, asks
# https://api.access.redhat.com/management/v1/images/<sha256>/download for
# the image's download link, downloads it to <dest>/<file-name>.part and
# moves it into place only if its SHA-256 matches --image-sha256. The
# offline token, the access token and the signed link go to curl on its
# stdin, so none of them is on a command line or on the host's disk. A file
# already there with the right SHA-256 is left alone.
#
# --dry-run checks the settings, that the vault has the secret (by name),
# and that the host is reachable, and prints what it would do; it reads no
# secret.
#
# Needs az signed in to the environment's tenant and ssh as root to the
# host (or a user with passwordless sudo). Works with macOS's bash 3.2.
set -euo pipefail
usage() { sed -n '5,8s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }

ENV_NAME="" HOST="" SHA="" FILE_NAME="" DEST=/var/lib/vz/template/cache SSH_KEY="" DRY_RUN=false
while [ $# -gt 0 ]; do
  case "$1" in
    --env) ENV_NAME=${2:-}; shift 2 ;;
    --host) HOST=${2:-}; shift 2 ;;
    --image-sha256) SHA=${2:-}; shift 2 ;;
    --file-name) FILE_NAME=${2:-}; shift 2 ;;
    --dest) DEST=${2:-}; shift 2 ;;
    --ssh-key) SSH_KEY=${2:-}; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] || { echo "error: missing --env" >&2; usage; }
[ -n "$HOST" ] || { echo "error: missing --host" >&2; usage; }
SHA=$(printf '%s' "$SHA" | tr '[:upper:]' '[:lower:]')
[[ $SHA =~ ^[0-9a-f]{64}$ ]] || die "--image-sha256: 64 hex digits, from Red Hat's download page"
[[ $FILE_NAME =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] || die "--file-name: a plain file name, such as rhel-10.2-x86_64-kvm.qcow2"
[[ $DEST =~ ^/[A-Za-z0-9._/-]*$ ]] || die "--dest: an absolute path"
[ -z "$SSH_KEY" ] || [ -r "$SSH_KEY" ] || die "--ssh-key: can't read $SSH_KEY"

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
# shellcheck source=scripts/lib/env.sh
. scripts/lib/env.sh
[ -s "$ENV_FILE" ] || die "no settings for $ENV_NAME; run scripts/bootstrap.sh --env $ENV_NAME first"
KEYVAULT=$(aget OCR_KEYVAULT)
[ -n "$KEYVAULT" ] || die "no OCR_KEYVAULT for $ENV_NAME: provision with USNM_OCR_NODES on (docs/operations.md)"

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=20)
[ -z "$SSH_KEY" ] || SSH_OPTS+=(-i "$SSH_KEY" -o IdentitiesOnly=yes)

why="(you need Key Vault Secrets Officer, from USNM_OCR_KEYVAULT_ADMINS, and to be at an address in USNM_OCR_NODE_IPS)"
echo "image $FILE_NAME ($SHA) to $HOST:$DEST, with rhel-offline-token from $KEYVAULT"

# Runs on the host as root. Arguments: sha256, dest, file name; the
# offline token on stdin.
# shellcheck disable=SC2016 # expands on the host
REMOTE='set -euo pipefail
sha=$1 dest=$2 name=$3
die() { echo "error: $*" >&2; exit 1; }
for c in curl python3 sha256sum; do command -v $c > /dev/null || die "no $c on $(hostname)"; done
out=$dest/$name
if [ -f "$out" ] && [ "$(sha256sum "$out" | cut -d" " -f1)" = "$sha" ]; then
  echo "$(hostname): $out is already there with that SHA-256"
  exit 0
fi
IFS= read -r offline || true
[ -n "$offline" ] || die "no offline token on stdin"
mkdir -p "$dest"
json() { python3 -c "import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1].split(\".\"):
    d=d.get(k) if isinstance(d,dict) else None
print(d or \"\")" "$1"; }
# The offline token for an access token: curl reads the form field from stdin.
access=$(printf "%s" "$offline" | curl -fsS --proto =https \
  -d grant_type=refresh_token -d client_id=rhsm-api --data-urlencode refresh_token@- \
  https://sso.redhat.com/auth/realms/redhat-external/protocol/openid-connect/token | json access_token) ||
  die "Red Hat SSO refused the offline token (expired? offline tokens lapse after 30 days unused)"
unset offline
[ -n "$access" ] || die "no access token from Red Hat SSO"
# The download link: a redirect, or JSON with body.href. The header from
# stdin; the answer, headers then body then the status code, stays in memory.
resp=$(printf "Authorization: Bearer %s\n" "$access" | curl -sS --proto =https -H @- \
  -D - -w "\n%{http_code}" "https://api.access.redhat.com/management/v1/images/$sha/download")
unset access
code=$(printf "%s\n" "$resp" | tail -n 1)
resp=$(printf "%s\n" "$resp" | sed "\$d")
case "$code" in
  307|302|301) href=$(printf "%s\n" "$resp" | sed -n "s/^[Ll]ocation: *//p" | tr -d "\r" | head -1) ;;
  200) href=$(printf "%s" "$resp" | python3 -c "import sys
print(sys.stdin.read().split(\"\r\n\r\n\")[-1])" | json body.href) ;;
  404) die "Red Hat has no image with SHA-256 $sha" ;;
  *) die "the image API answered HTTP $code" ;;
esac
[ -n "$href" ] || die "the image API gave no download link (HTTP $code)"
case "$href" in https://*) ;; *) die "the download link isn'"'"'t HTTPS" ;; esac
echo "$(hostname): downloading $name"
# The signed link from a curl config on stdin.
printf "url = \"%s\"\n" "$href" | curl -fL --proto =https --proto-redir =https -sS -K - -o "$out.part"
got=$(sha256sum "$out.part" | cut -d" " -f1)
[ "$got" = "$sha" ] || { rm -f "$out.part"; die "SHA-256 mismatch: got $got"; }
mv "$out.part" "$out"
echo "$(hostname): $out verified ($sha, $(stat -c %s "$out") bytes)"'

remote_cmd="bash -c $(printf '%q' "$REMOTE") rhel-image $(printf '%q ' "$SHA" "$DEST" "$FILE_NAME")"
[ "${HOST%%@*}" = root ] || remote_cmd="sudo $remote_cmd"

if [ "$DRY_RUN" = true ]; then
  names=$(az keyvault secret list --vault-name "$KEYVAULT" --query '[].name' -o tsv) || die "can't list $KEYVAULT's secrets $why"
  printf '%s\n' "$names" | grep -qx rhel-offline-token ||
    die "$KEYVAULT has no rhel-offline-token (docs/operations.md)"
  echo "$KEYVAULT has rhel-offline-token"
  # shellcheck disable=SC2029 # $DEST is meant to expand here
  ssh "${SSH_OPTS[@]}" "$HOST" "test -d $(printf '%q' "$DEST") && echo \"\$(hostname): $DEST exists\" || echo \"\$(hostname): $DEST will be created\"" ||
    die "can't reach $HOST over ssh"
  echo "dry run: would read rhel-offline-token, send it to $HOST on ssh's stdin and fetch the image there"
  exit 0
fi

TOKEN=$(az keyvault secret show --vault-name "$KEYVAULT" --name rhel-offline-token --query value -o tsv) ||
  die "can't read rhel-offline-token from $KEYVAULT $why"
[ -n "$TOKEN" ] || die "rhel-offline-token in $KEYVAULT is empty"
# shellcheck disable=SC2029 # the arguments are meant to expand here
printf '%s\n' "$TOKEN" | ssh "${SSH_OPTS[@]}" "$HOST" "$remote_cmd"
unset TOKEN
