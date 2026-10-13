#!/usr/bin/env bash
# Container entrypoint for the stock BOINC server image.
#
#   --serve              (default) wait for the project, render config.xml into tmpfs, start
#                        Apache and the daemons under supervisord
#   --init-project       once, by hand (az containerapp exec): create the schema and the project
#                        tree on the share with the stock make_project, register the app
#   --make-upload-keys --dir DIR
#                        write upload_private and upload_public into DIR (operator machine)
#
# UNVERIFIED: none of this has run; see contrib-compute/docs/images.md.
set -euo pipefail
mode=serve keydir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --serve) mode=serve; shift ;;
    --init-project) mode=init-project; shift ;;
    --make-upload-keys) mode=make-upload-keys; shift ;;
    --dir) [ $# -ge 2 ] || { echo "--dir needs a value" >&2; exit 2; }; keydir="$2"; shift 2 ;;
    *) echo "entrypoint.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ "$mode" = make-upload-keys ]; then
  [ -n "$keydir" ] || { echo "--make-upload-keys needs --dir DIR" >&2; exit 2; }
  umask 077
  exec /usr/local/boinc/lib/crypt_prog -genkey 1024 "$keydir/upload_private" "$keydir/upload_public"
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
  trap 'rm -rf "$scratch" "$PROJ/keys"' EXIT
  (cd "$BOINC_SRC" && tools/make_project --no_query --srcdir "$BOINC_SRC" \
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
  sed -e "s|@PROJ@|$PROJ|g" -e "s|@PROJECT@|$PROJECT|g" -e "s|@MASTER_HOST@|$MASTER_HOST|g" \
    "$TEMPLATES/apache-project.conf.template" > /etc/apache2/conf-enabled/boinc-project.conf
  exec /usr/bin/supervisord -n -c "$TEMPLATES/supervisord.conf"
}

case "$mode" in
  init-project) init_project ;;
  serve) serve ;;
esac
