# shellcheck shell=bash
# The secrets job's step (entrypoint.sh --ensure-secrets). Sourced, not run.
#
# Runs inside the BOINC VNet with the job's managed identity (Key Vault Secrets Officer) and
# creates the secrets that must not rotate, each only if it is absent:
#   mysql-runtime-password            random, for BOINC's run-time MySQL user
#   upload-private-key, upload-public-key
#                                     BOINC's upload key pair, from crypt_prog -genkey
# Absence is known only from a successful listing of the vault: a failed lookup (throttling,
# network, permission) stops the job, so an existing password or key is never overwritten.
# Values are generated under /dev/shm (tmpfs), mode 0600, sent to Key Vault in a request body
# read from a file, never put on a command line and never printed. The vault's other secrets
# come from the deployment (mysql-admin-password, code-sign-public-key).

: "${KEY_VAULT_NAME:?}" "${AZURE_CLIENT_ID:?}"
KV_API=7.4

log() { echo "usnm-boinc: $*" >&2; }
fail() { log "$*"; exit 1; }

# The token goes to curl in a header file, not on its command line.
kv_auth_header() {
  local out=$1 token
  [ -n "${IDENTITY_ENDPOINT:-}" ] && [ -n "${IDENTITY_HEADER:-}" ] || fail "no managed identity endpoint in the environment"
  token="$(curl -fsS -H @- \
    "$IDENTITY_ENDPOINT?api-version=2019-08-01&resource=https://vault.azure.net&client_id=$AZURE_CLIENT_ID" \
    <<< "X-IDENTITY-HEADER: $IDENTITY_HEADER" | jq -er .access_token)" || fail "cannot get a Key Vault token"
  (umask 077; printf 'Authorization: Bearer %s\n' "$token" > "$out")
}

# kv_names HEADER: every secret name in the vault, one per line, following nextLink.
# Fails on any error; an empty vault is an empty, successful listing.
kv_names() {
  local hdr=$1 url="https://$KEY_VAULT_NAME.vault.azure.net/secrets?api-version=$KV_API&maxresults=25" page code
  while [ -n "$url" ]; do
    page="$(curl -sS -w '\n%{http_code}' -H @"$hdr" "$url")" || return 1
    code="${page##*$'\n'}"; page="${page%$'\n'*}"
    if [ "$code" != 200 ]; then
      # Key Vault's error code and message name the cause (network, RBAC); they hold no secret.
      echo "Key Vault answered $code: $(jq -c '.error | {code, message, inner: .innererror.code}' <<< "$page" 2>/dev/null || echo "${page:0:300}")" >&2
      return 1
    fi
    jq -er '.value | map(.id | split("/") | last) | .[]' <<< "$page" 2>/dev/null || jq -e '.value == []' <<< "$page" > /dev/null || return 1
    url="$(jq -r '.nextLink // empty' <<< "$page")" || return 1
  done
}

# kv_put HEADER NAME FILE: sets secret NAME to FILE's contents. The body is built in a file.
kv_put() {
  local hdr=$1 name=$2 file=$3 body resp code
  body="$(dirname "$file")/body.json" resp="$(dirname "$file")/response.json"
  (umask 077; jq -n --rawfile v "$file" '{value: $v, contentType: "text/plain"}' > "$body")
  # The success response echoes the value, so it stays in the tmpfs file and is never printed;
  # on an error only Key Vault's error fields are.
  code="$(umask 077; curl -sS -o "$resp" -w '%{http_code}' -X PUT -H @"$hdr" -H 'Content-Type: application/json' \
    --data-binary @"$body" "https://$KEY_VAULT_NAME.vault.azure.net/secrets/$name?api-version=$KV_API")" || code=000
  rm -f "$body"
  if [ "$code" != 200 ]; then
    echo "Key Vault answered $code: $(jq -c '.error | {code, message, inner: .innererror.code}' "$resp" 2>/dev/null || echo unreadable)" >&2
    rm -f "$resp"; fail "cannot write Key Vault secret $name"
  fi
  rm -f "$resp"
  log "created $name"
}

# kv_same HEADER NAME FILE: true when secret NAME holds exactly FILE's contents. Compared in tmpfs;
# nothing is printed.
kv_same() {
  local hdr=$1 name=$2 file=$3 got
  got="$(dirname "$file")/readback"
  (umask 077; curl -fsS -H @"$hdr" "https://$KEY_VAULT_NAME.vault.azure.net/secrets/$name?api-version=$KV_API" |
    jq -j .value > "$got") || { rm -f "$got"; return 1; }
  cmp -s "$got" "$file"; local rc=$?
  rm -f "$got"; return $rc
}

ensure_secrets() {
  local work hdr names
  work="$(mktemp -d /dev/shm/secrets.XXXXXX)"; chmod 700 "$work"
  # shellcheck disable=SC2064
  trap "rm -rf '$work'" EXIT
  hdr="$work/auth"
  kv_auth_header "$hdr"
  names="$(kv_names "$hdr")" || fail "cannot list the secrets in Key Vault $KEY_VAULT_NAME; changing nothing"
  have() { grep -qxF -- "$1" <<< "$names"; }

  if have mysql-runtime-password; then
    log "mysql-runtime-password exists; kept"
  else
    (umask 077; printf 'Aa1%s' "$(openssl rand -hex 20)" > "$work/runtime")
    kv_put "$hdr" mysql-runtime-password "$work/runtime"
  fi

  if have upload-private-key && have upload-public-key; then
    log "the upload key pair exists; kept"
  elif have upload-private-key || have upload-public-key; then
    fail "only one of upload-private-key and upload-public-key exists in $KEY_VAULT_NAME; refusing to overwrite it. Resolve by hand"
  else
    (umask 077; /usr/local/boinc/lib/crypt_prog -genkey 1024 "$work/upload_private" "$work/upload_public" > /dev/null) ||
      fail "crypt_prog could not make the upload keys"
    kv_put "$hdr" upload-private-key "$work/upload_private"
    kv_put "$hdr" upload-public-key "$work/upload_public"
    # Key Vault has no create-if-absent, so an overlapping run could interleave the two PUTs
    # (provision-boinc.sh refuses to start this job while an execution runs). Read both back:
    # a pair that isn't the one just written fails here instead of serving mismatched keys.
    kv_same "$hdr" upload-private-key "$work/upload_private" && kv_same "$hdr" upload-public-key "$work/upload_public" ||
      fail "the upload key pair in $KEY_VAULT_NAME is not the pair this run wrote (an overlapping run?). Delete both secrets and run again"
  fi
  log "secrets in $KEY_VAULT_NAME are complete"
}
