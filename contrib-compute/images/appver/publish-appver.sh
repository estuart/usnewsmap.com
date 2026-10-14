#!/usr/bin/env bash
# The publish job's command (infra/boinc/main.bicep, publishJob): copies the signed app version
# baked into this image (Containerfile) onto the project share, installs its templates, and runs
# the stock bin/update_versions, which reads the .sig files and adds the app_version rows.
#
# Versions are immutable: if apps/<app>/<version>/ is already on the share it must be identical,
# and then only update_versions runs again (it skips versions it already has).
set -euo pipefail
# shellcheck source=SCRIPTDIR/../server/lib.sh
. /usr/local/lib/usnm-boinc/lib.sh
: "${APPVER_APP:?}" "${APPVER_VERSION:?}"
[ -e "$PROJ/.usnm-initialised" ] || fail "project is not initialised"
src=/usr/local/lib/usnm-boinc/appver
dst="$PROJ/apps/$APPVER_APP/$APPVER_VERSION"
# One publish at a time: executions of a manual job can overlap, and they share apps/,
# templates/ and update_versions. mkdir is atomic on the share; a lock older than the job's
# timeout is from a run that was killed, and is taken over.
lock="$PROJ/.usnm-publish.lock"
if ! mkdir "$lock" 2> /dev/null; then
  if [ -n "$(find "$lock" -maxdepth 0 -mmin +120 2> /dev/null)" ]; then
    # Rename is atomic: of two runs that both see the stale lock, only one moves it away, and
    # only that one may make the new lock (which mkdir again claims atomically).
    stale="$lock.stale.$$.$(date +%s)"
    mv "$lock" "$stale" 2> /dev/null || fail "another publish took over $lock first; wait for it to end"
    rm -rf "$stale"
    mkdir "$lock" 2> /dev/null || fail "another publish took $lock first; wait for it to end"
    log "took over $lock, left by a run more than 2 hours ago"
  else
    fail "another publish holds $lock; wait for it to end"
  fi
fi
trap 'rm -rf "$lock"' EXIT
mkdir -p "$PROJ/apps/$APPVER_APP" "$PROJ/templates"
if [ -e "$dst" ]; then
  diff -r "$src/apps/$APPVER_APP/$APPVER_VERSION" "$dst" > /dev/null ||
    fail "$dst is already on the share and differs from this image: versions are immutable, release a new version"
  # A published version keeps its templates: a differing one is refused, a missing one restored.
  for t in "$src"/templates/*; do
    n=$(basename "$t")
    if [ -e "$PROJ/templates/$n" ]; then
      cmp -s "$t" "$PROJ/templates/$n" ||
        fail "templates/$n on the share differs from this image's; a changed template is a new version"
    else
      cp "$t" "$PROJ/templates/$n"
      log "restored the missing templates/$n"
    fi
  done
  log "$dst is already on the share and identical"
else
  stage=$(mktemp -d "$PROJ/apps/$APPVER_APP/.$APPVER_VERSION.XXXXXX")
  cp -r "$src/apps/$APPVER_APP/$APPVER_VERSION/." "$stage/"
  chmod 0755 "$stage"
  mv "$stage" "$dst" || { rm -rf "$stage"; fail "can't move the version into $dst"; }
  log "copied $APPVER_APP $APPVER_VERSION to $dst"
  cp "$src"/templates/* "$PROJ/templates/"
fi
fetch_secrets
render_config
(cd "$PROJ" && bin/update_versions --noconfirm)
chown -R boincadm:boincadm "$dst" "$PROJ/templates" "$PROJ/download" 2> /dev/null ||
  log "chown on the share failed (UNVERIFIED semantics, design.md 2.3)"
log "published $APPVER_APP $APPVER_VERSION"
