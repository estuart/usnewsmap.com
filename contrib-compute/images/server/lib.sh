# shellcheck shell=bash
# shellcheck disable=SC2034
# Shared by entrypoint.sh and bridge-job.sh. Sourced, not run.
# Secrets come from Key Vault with the container's managed identity and are written only
# under /dev/shm (tmpfs), mode 0600; nothing is printed and nothing goes into an image layer.

: "${KEY_VAULT_NAME:?}" "${DB_HOST:?}" "${DB_NAME:?}" "${MASTER_HOST:?}" "${PROJECT_DIR:?}"
PROJECT="${PROJECT:-usnmocr}"
DB_RUNTIME_USER="${DB_RUNTIME_USER:-usnmboincrun}"
DB_ADMIN_USER="${DB_ADMIN_USER:-usnmboincadmin}"
export PROJECT DB_RUNTIME_USER
BOINC_SRC=/usr/local/boinc
SHARE_DIR="$PROJECT_DIR"
PROJ="$PROJECT_DIR/$PROJECT"
SECRETS=/dev/shm/boinc
TEMPLATES=/usr/local/lib/usnm-boinc

log() { echo "usnm-boinc: $*" >&2; }
fail() { log "$*"; exit 1; }

kv_token() {
  [ -n "${IDENTITY_ENDPOINT:-}" ] && [ -n "${IDENTITY_HEADER:-}" ] || fail "no managed identity endpoint in the environment"
  curl -fsS -H "X-IDENTITY-HEADER: $IDENTITY_HEADER" \
    "$IDENTITY_ENDPOINT?api-version=2019-08-01&resource=https://vault.azure.net&client_id=${AZURE_CLIENT_ID:?}" |
    jq -er .access_token
}

# kv_get NAME FILE: writes the secret value to FILE (mode 0600). Never prints it.
kv_get() {
  local name=$1 out=$2 token
  token="$(kv_token)" || fail "cannot get a Key Vault token"
  (umask 077; curl -fsS -H "Authorization: Bearer $token" \
    "https://$KEY_VAULT_NAME.vault.azure.net/secrets/$name?api-version=7.4" | jq -er .value > "$out") ||
    fail "cannot read Key Vault secret $name"
}

fetch_secrets() {
  mkdir -p "$SECRETS/keys"
  chmod 700 "$SECRETS" "$SECRETS/keys"
  kv_get mysql-runtime-password "$SECRETS/db-runtime"
  kv_get upload-private-key "$SECRETS/keys/upload_private"
  kv_get upload-public-key "$SECRETS/keys/upload_public"
  kv_get code-sign-public-key "$SECRETS/keys/code_sign_public"
  chmod 640 "$SECRETS"/keys/*
  chown -R root:boincadm "$SECRETS"
  chmod 750 "$SECRETS" "$SECRETS/keys"
}

# render_config: fills config.xml.template from the environment and the fetched runtime
# password into tmpfs, and links it into the project tree. The code-signing PRIVATE key is not
# fetched, rendered or linked anywhere.
# <host> is the container's own short hostname, not the public name: sched/start (at the pinned
# BOINC commit, lines 662-663 and 787) starts a daemon only when its host equals
# socket.gethostname().split('.')[0]. config.xml is rendered on every start, so a replaced replica
# gets its new name. The public URL stays in <master_url>. UNVERIFIED: that a Container Apps
# replica's hostname is stable for the life of its process (it is re-rendered at each start).
render_config() {
  local daemons
  daemons='<daemon><cmd>sample_trivial_validator --app usnm_ocr</cmd></daemon>
        <daemon><cmd>batch_collect_assimilator --app usnm_ocr</cmd></daemon>'
  # The assessment app (#456) validates with usnm-boinc, which only the bridge image carries.
  # A server run from that image adds its daemons and links the validator scripts into bin/.
  if command -v usnm-boinc >/dev/null 2>&1; then
    daemons="$daemons
        <daemon><cmd>script_validator --app usnm_assess --init_script &quot;usnm_assess_check files&quot; --compare_script &quot;usnm_assess_compare files files2&quot;</cmd></daemon>
        <daemon><cmd>batch_collect_assimilator --app usnm_assess</cmd></daemon>"
    if [ -d "$PROJ/bin" ]; then
      ln -sfn "$TEMPLATES/usnm_assess_check" "$PROJ/bin/usnm_assess_check"
      ln -sfn "$TEMPLATES/usnm_assess_compare" "$PROJ/bin/usnm_assess_compare"
    fi
  fi
  DB_PASSWD="$(cat "$SECRETS/db-runtime")" python3 - "$TEMPLATES/config.xml.template" "$SECRETS/config.xml" "$daemons" <<'PY'
import os, socket, sys
from xml.sax.saxutils import escape
src, dst, daemons = sys.argv[1:4]
v = {
    "DB_HOST": os.environ["DB_HOST"], "DB_NAME": os.environ["DB_NAME"],
    "DB_USER": os.environ["DB_RUNTIME_USER"], "DB_PASSWD": os.environ["DB_PASSWD"],
    "MASTER_URL": "https://%s/" % os.environ["MASTER_HOST"], "PROJECT_HOST": socket.gethostname().split(".")[0],
    "PROJECT_DIR": os.path.join(os.environ["PROJECT_DIR"], os.environ["PROJECT"]), "PROJECT": os.environ["PROJECT"],
}
t = open(src).read()
for k, x in v.items():
    t = t.replace("@%s@" % k, escape(x))
t = t.replace("@APP_DAEMONS@", daemons)
fd = os.open(dst, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o640)
with os.fdopen(fd, "w") as f:
    f.write(t)
PY
  chown root:boincadm "$SECRETS/config.xml"
  ln -sfn "$SECRETS/config.xml" "$PROJ/config.xml"
  rm -rf "${PROJ:?}/keys"
  ln -sfn "$SECRETS/keys" "$PROJ/keys"
}
