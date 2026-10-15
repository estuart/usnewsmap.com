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

# kv_get_optional NAME FILE: as kv_get, but a secret that does not exist (404) is skipped with
# a log line and returns 1; any other failure stops, as kv_get does.
kv_get_optional() {
  local name=$1 out=$2 token code
  token="$(kv_token)" || fail "cannot get a Key Vault token"
  code="$(umask 077; curl -sS -o "$out.json" -w '%{http_code}' -H "Authorization: Bearer $token" \
    "https://$KEY_VAULT_NAME.vault.azure.net/secrets/$name?api-version=7.4")" || { rm -f "$out.json"; fail "cannot read Key Vault secret $name"; }
  case "$code" in
    200) (umask 077; jq -er .value "$out.json" > "$out") || { rm -f "$out.json"; fail "cannot read Key Vault secret $name"; } ;;
    404) rm -f "$out.json"; log "no $name in Key Vault yet"; return 1 ;;
    *) rm -f "$out.json"; fail "cannot read Key Vault secret $name (HTTP $code)" ;;
  esac
  rm -f "$out.json"
}

fetch_secrets() {
  mkdir -p "$SECRETS/keys"
  chmod 700 "$SECRETS" "$SECRETS/keys"
  kv_get mysql-runtime-password "$SECRETS/db-runtime"
  kv_get upload-private-key "$SECRETS/keys/upload_private"
  kv_get upload-public-key "$SECRETS/keys/upload_public"
  # The public half of the offline code-signing key arrives with the first signed app version
  # (provision-boinc.sh, step 4); until then the project serves without it.
  kv_get_optional code-sign-public-key "$SECRETS/keys/code_sign_public" || true
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
  daemons='<daemon><cmd>sample_trivial_validator --app usnm_ocr</cmd><output>sample_trivial_validator_usnm_ocr.log</output><pid_file>sample_trivial_validator_usnm_ocr.pid</pid_file></daemon>
        <daemon><cmd>batch_collect_assimilator --app usnm_ocr</cmd><output>batch_collect_assimilator_usnm_ocr.log</output><pid_file>batch_collect_assimilator_usnm_ocr.pid</pid_file></daemon>'
  # The assessment app (#456), the gold check (#462) and the Surya test (#516) validate with usnm-boinc, which only the bridge image carries.
  # A server run from that image adds its daemons and links the validator scripts into bin/.
  # Script names only: script_validator then adds "files" (and "files2") itself. Spelled out,
  # the macros never expand at BOINC 22bbebe0, whose split() keeps the delimiter on each token.
  if command -v usnm-boinc >/dev/null 2>&1; then
    daemons="$daemons
        <daemon><cmd>script_validator --app usnm_assess --init_script usnm_assess_check --compare_script usnm_assess_compare</cmd><output>script_validator_usnm_assess.log</output><pid_file>script_validator_usnm_assess.pid</pid_file></daemon>
        <daemon><cmd>batch_collect_assimilator --app usnm_assess</cmd><output>batch_collect_assimilator_usnm_assess.log</output><pid_file>batch_collect_assimilator_usnm_assess.pid</pid_file></daemon>
        <daemon><cmd>script_validator --app usnm_gold --init_script usnm_gold_check</cmd><output>script_validator_usnm_gold.log</output><pid_file>script_validator_usnm_gold.pid</pid_file></daemon>
        <daemon><cmd>batch_collect_assimilator --app usnm_gold</cmd><output>batch_collect_assimilator_usnm_gold.log</output><pid_file>batch_collect_assimilator_usnm_gold.pid</pid_file></daemon>
        <daemon><cmd>script_validator --app usnm_surya --init_script usnm_surya_check --compare_script usnm_surya_compare</cmd><output>script_validator_usnm_surya.log</output><pid_file>script_validator_usnm_surya.pid</pid_file></daemon>
        <daemon><cmd>batch_collect_assimilator --app usnm_surya</cmd><output>batch_collect_assimilator_usnm_surya.log</output><pid_file>batch_collect_assimilator_usnm_surya.pid</pid_file></daemon>
        <daemon><cmd>script_validator --app usnm_quality --init_script usnm_quality_check --compare_script usnm_quality_compare</cmd><output>script_validator_usnm_quality.log</output><pid_file>script_validator_usnm_quality.pid</pid_file></daemon>
        <daemon><cmd>batch_collect_assimilator --app usnm_quality</cmd><output>batch_collect_assimilator_usnm_quality.log</output><pid_file>batch_collect_assimilator_usnm_quality.pid</pid_file></daemon>
        <daemon><cmd>script_validator --app usnm_qwen --init_script usnm_qwen_check --compare_script usnm_qwen_compare</cmd><output>script_validator_usnm_qwen.log</output><pid_file>script_validator_usnm_qwen.pid</pid_file></daemon>
        <daemon><cmd>batch_collect_assimilator --app usnm_qwen</cmd><output>batch_collect_assimilator_usnm_qwen.log</output><pid_file>batch_collect_assimilator_usnm_qwen.pid</pid_file></daemon>"
    if [ -d "$PROJ/bin" ]; then
      ln -sfn "$TEMPLATES/usnm_assess_check" "$PROJ/bin/usnm_assess_check"
      ln -sfn "$TEMPLATES/usnm_assess_compare" "$PROJ/bin/usnm_assess_compare"
      ln -sfn "$TEMPLATES/usnm_gold_check" "$PROJ/bin/usnm_gold_check"
      ln -sfn "$TEMPLATES/usnm_surya_check" "$PROJ/bin/usnm_surya_check"
      ln -sfn "$TEMPLATES/usnm_surya_compare" "$PROJ/bin/usnm_surya_compare"
      ln -sfn "$TEMPLATES/usnm_quality_check" "$PROJ/bin/usnm_quality_check"
      ln -sfn "$TEMPLATES/usnm_quality_compare" "$PROJ/bin/usnm_quality_compare"
      ln -sfn "$TEMPLATES/usnm_qwen_check" "$PROJ/bin/usnm_qwen_check"
      ln -sfn "$TEMPLATES/usnm_qwen_compare" "$PROJ/bin/usnm_qwen_compare"
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
