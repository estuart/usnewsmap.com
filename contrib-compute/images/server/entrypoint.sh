#!/usr/bin/env bash
# Container entrypoint for the stock BOINC server image.
#
#   --serve              (default) wait for the project, render config.xml into tmpfs, start
#                        Apache and the daemons under supervisord
#   --init-project       once, by hand (az containerapp exec): create the schema and the project
#                        tree on the share with the stock make_project, register the app
#   --make-upload-keys --dir DIR
#                        write upload_private and upload_public into DIR
#   --ensure-secrets     the secrets job (infra/boinc/modules/secrets-job.bicep): create
#                        mysql-runtime-password and the upload key pair in Key Vault, each only
#                        if absent. A failed lookup stops the job; it is never read as absence
#
# UNVERIFIED: none of this has run; see contrib-compute/docs/images.md.
set -euo pipefail
mode=serve keydir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --serve) mode=serve; shift ;;
    --init-project) mode=init-project; shift ;;
    --make-upload-keys) mode=make-upload-keys; shift ;;
    --ensure-secrets) mode=ensure-secrets; shift ;;
    --dir) [ $# -ge 2 ] || { echo "--dir needs a value" >&2; exit 2; }; keydir="$2"; shift 2 ;;
    *) echo "entrypoint.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ "$mode" = make-upload-keys ]; then
  [ -n "$keydir" ] || { echo "--make-upload-keys needs --dir DIR" >&2; exit 2; }
  umask 077
  exec /usr/local/boinc/lib/crypt_prog -genkey 1024 "$keydir/upload_private" "$keydir/upload_public"
fi

if [ "$mode" = ensure-secrets ]; then
  # shellcheck source=secrets.sh
  . /usr/local/lib/usnm-boinc/secrets.sh
  ensure_secrets
  exit 0
fi

# shellcheck source=lib.sh
. /usr/local/lib/usnm-boinc/lib.sh

init_project() {
  local marker="$PROJ/.usnm-initialised" mycnf="$SECRETS/mysql-admin.cnf" scratch="$SECRETS/scratch"
  [ ! -e "$marker" ] || fail "$PROJ is already initialised ($marker exists); refusing to run make_project again"
  mkdir -p "$SECRETS"; chmod 700 "$SECRETS"
  kv_get mysql-admin-password "$SECRETS/db-admin"
  kv_get mysql-runtime-password "$SECRETS/db-runtime"
  (umask 077; { echo '[client]'; echo "host=$DB_HOST"; echo "user=$DB_ADMIN_USER"; echo "password=$(cat "$SECRETS/db-admin")"; } > "$mycnf")
  # make_project creates the database and the schema, and (as a side effect of its library)
  # a code-signing key pair and an upload key pair. Both go to a scratch directory that is
  # deleted below: the real upload keys come from Key Vault and the code-signing private key
  # must never exist on this server.
  mkdir -p "$scratch"
  # Removed on any exit, including a make_project failure under set -e.
  # Expanded now: the locals are gone by the time an EXIT trap runs under set -u.
  # shellcheck disable=SC2064 # expanded on purpose (see above)
  trap "rm -rf '$scratch' '$PROJ/keys'" EXIT
  # No marker means no finished init: a failed run's tree and database are started afresh.
  # make_project reads $USER, which a container is not given.
  (cd "$BOINC_SRC" && USER="${USER:-$(id -un)}" tools/make_project --no_query --delete_prev_inst --drop_db_first --srcdir "$BOINC_SRC" \
      --project_root "$PROJ" --key_dir "$scratch" \
      --url_base "https://$MASTER_HOST/" --html_user_url "https://$MASTER_HOST/$PROJECT/" \
      --html_ops_url "https://$MASTER_HOST/${PROJECT}_ops/" --project_host "$MASTER_HOST" \
      --db_host "$DB_HOST" --db_name "$DB_NAME" --db_user "$DB_ADMIN_USER" \
      --db_passwd "$(cat "$SECRETS/db-admin")" --user_name boincadm \
      "$PROJECT" 'usnewsmap OCR')
  rm -rf "$scratch" "$PROJ/config.xml" "$PROJ/keys"
  trap - EXIT
  # The run-time user needs no schema rights. UNVERIFIED that these grants are enough.
  local rt; rt="$(cat "$SECRETS/db-runtime")"
  mysql --defaults-extra-file="$mycnf" <<SQL
CREATE USER IF NOT EXISTS '$DB_RUNTIME_USER'@'%' IDENTIFIED BY '$rt';
ALTER USER '$DB_RUNTIME_USER'@'%' IDENTIFIED BY '$rt';
GRANT SELECT, INSERT, UPDATE, DELETE ON \`$DB_NAME\`.* TO '$DB_RUNTIME_USER'@'%';
SQL
  fetch_secrets
  render_config
  cp "$TEMPLATES/project.xml" "$PROJ/project.xml"
  cp "$TEMPLATES/plan_class_spec.xml" "$PROJ/plan_class_spec.xml"
  (cd "$PROJ" && bin/xadd)
  mkdir -p "$SHARE_DIR/handoff"
  chown -R boincadm:boincadm "$PROJ" "$SHARE_DIR/handoff" 2>/dev/null || log "chown on the share failed (UNVERIFIED semantics, design.md 2.3)"
  rm -f "$SECRETS/db-admin" "$mycnf"
  touch "$marker"
  log "project $PROJECT initialised; next: app version and signing (bootstrap.sh step 4)"
}

# project_tree: fixes to the project tree on the share, safe to repeat at every start.
# - make_project made only cache/00..80 of the 256 the pages hash into (seen 2026-10-13);
# - the stock project.inc names the project "REPLACE WITH PROJECT NAME".
project_tree() {
  local i inc="$PROJ/html/project/project.inc"
  for i in $(seq 0 255); do mkdir -p "$PROJ/html/cache/$(printf %02x "$i")"; done
  sed -i -e 's|"REPLACE WITH PROJECT NAME"|"usnewsmap OCR"|' \
    -e 's|"REPLACE WITH COPYRIGHT HOLDER"|"Trevor Goodyear"|' "$inc"
}

serve() {
  local marker="$PROJ/.usnm-initialised" placeholder=""
  if [ ! -e "$marker" ]; then
    log "project not initialised; answering the probe until init-project has run"
    python3 - <<'PY' &
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b"not initialised\n")
http.server.HTTPServer(("", 80), H).serve_forever()
PY
    placeholder=$!
    until [ -e "$marker" ]; do sleep 10; done
    kill "$placeholder"; wait "$placeholder" 2>/dev/null || true
  fi
  fetch_secrets
  render_config
  project_tree
  sed -e "s|@PROJ@|$PROJ|g" -e "s|@PROJECT@|$PROJECT|g" -e "s|@MASTER_HOST@|$MASTER_HOST|g" \
    "$TEMPLATES/apache-project.conf.template" > /etc/apache2/conf-enabled/boinc-project.conf
  exec /usr/bin/supervisord -n -c "$TEMPLATES/supervisord.conf"
}

case "$mode" in
  init-project) init_project ;;
  serve) serve ;;
esac
