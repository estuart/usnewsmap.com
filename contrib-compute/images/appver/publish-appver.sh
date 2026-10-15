#!/usr/bin/env bash
# The publish job's command (infra/boinc/main.bicep, publishJob): copies the signed app version
# baked into this image (Containerfile) onto the project share, installs its templates, and runs
# the stock bin/update_versions, which reads the .sig files and adds the app_version rows.
#
# Platform versions are immutable: an existing platform directory must be identical,
# and then only update_versions runs again (it skips versions it already has).
set -euo pipefail
# shellcheck source=SCRIPTDIR/../server/lib.sh
. /usr/local/lib/usnm-boinc/lib.sh
: "${APPVER_APP:?}" "${APPVER_VERSION:?}"
APPVER_PLATFORM=${APPVER_PLATFORM:-x86_64-pc-linux-gnu}
APPVER_PLAN_CLASS=${APPVER_PLAN_CLASS:-docker}
case "$APPVER_PLATFORM" in x86_64-pc-linux-gnu|arm64-apple-darwin|x86_64-apple-darwin) ;; *) fail "unsupported app platform" ;; esac
case "$APPVER_PLAN_CLASS" in docker|docker_mt|docker_nvidia_cuda) ;; *) fail "unsupported plan class" ;; esac
[ "$APPVER_PLATFORM" = x86_64-pc-linux-gnu ] || [ "$APPVER_PLAN_CLASS" = docker ] || fail "$APPVER_PLATFORM supports only plan class docker"
[ -e "$PROJ/.usnm-initialised" ] || fail "project is not initialised"
src=/usr/local/lib/usnm-boinc/appver
version_dir="$PROJ/apps/$APPVER_APP/$APPVER_VERSION"
subdir="${APPVER_PLATFORM}__${APPVER_PLAN_CLASS}"
dst="$version_dir/$subdir"
source_dir="$src/apps/$APPVER_APP/$APPVER_VERSION/$subdir"
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
has_existing_version=false
if [ -d "$version_dir" ] && [ -n "$(find "$version_dir" -mindepth 1 -maxdepth 1 -type d ! -name '.*' -print -quit)" ]; then
  has_existing_version=true
fi
# Adding a sibling platform or re-running a published version must not rewrite existing shared templates.
if [ "$has_existing_version" = true ]; then
  for t in "$src"/templates/*; do
    n=$(basename "$t")
    if [ -e "$PROJ/templates/$n" ]; then
      cmp -s "$t" "$PROJ/templates/$n" ||
        fail "templates/$n on the share differs from this image's; a changed template requires a new version"
    fi
  done
fi
if [ -e "$dst" ]; then
  diff -r "$source_dir" "$dst" > /dev/null ||
    fail "$dst is already on the share and differs from this image: versions are immutable, release a new version"
  log "$dst is already on the share and identical"
else
  mkdir -p "$version_dir"
  stage=$(mktemp -d "$version_dir/.$subdir.XXXXXX")
  cp -r "$source_dir/." "$stage/"
  chmod 0755 "$stage"
  mv "$stage" "$dst" || { rm -rf "$stage"; fail "can't move the version into $dst"; }
  log "copied $APPVER_APP $APPVER_VERSION to $dst"
fi
for t in "$src"/templates/*; do
  n=$(basename "$t")
  if [ -e "$PROJ/templates/$n" ] && cmp -s "$t" "$PROJ/templates/$n"; then
    continue
  fi
  tmp_tpl=$(mktemp "$PROJ/templates/.$n.XXXXXX")
  cp "$t" "$tmp_tpl"
  chmod 0644 "$tmp_tpl"
  mv -f "$tmp_tpl" "$PROJ/templates/$n" || { rm -f "$tmp_tpl"; fail "can't install templates/$n"; }
  log "installed templates/$n"
done
fetch_secrets
render_config
(cd "$PROJ" && cp "$src/project.xml" project.xml && bin/xadd)
(cd "$PROJ" && bin/update_versions --noconfirm)
chown -R boincadm:boincadm "$dst" "$PROJ/templates" "$PROJ/download" 2> /dev/null ||
  log "chown on the share failed (UNVERIFIED semantics, design.md 2.3)"
log "published $APPVER_APP $APPVER_VERSION"
