#!/usr/bin/env bash
# The Playwright + axe suite against the API started by start-api-fixtures.sh,
# which serves the build as production does. The tests intercept the default
# basemap style and serve a local one, so no third-party tile service is
# needed.
#
#   web-e2e.sh --step build   install, build the site and precompress it
#   web-e2e.sh --step test    run the suite (the API must be up)
#
# CI runs them as separate steps so the API's build (started in the
# background beforehand) overlaps the site's; without --step, both run.
# Chromium and its system packages install in the background while the site
# builds (playwright-install.log).
set -euo pipefail
usage() { echo "usage: scripts/ci/web-e2e.sh [--step build|test]" >&2; exit 2; }
# The positional form (`web-e2e.sh build`) works for one release, with a
# warning (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/ci/web-e2e.sh STEP\` is deprecated; use --step (#277)" >&2
  set -- --step "$1"
fi
step=all
while [ $# -gt 0 ]; do
  case $1 in
    --step) [ $# -ge 2 ] || usage; step=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
case "$step" in
  build | test | all) ;;
  *) usage ;;
esac
cd web
if [ "$step" = build ] || [ "$step" = all ]; then
  npm ci
  npx playwright install --with-deps chromium > ../playwright-install.log 2>&1 &
  install=$!
  # Always reap the installer, even when the site's build fails, so it
  # isn't still writing its log when the step ends.
  site=0
  # The fixture corpus has 6 places: the relative rate's tests need it on with fewer than the site's 10.
  { VITE_SKEW_MIN_PLACES=5 npm run build && node scripts/precompress.mjs dist; } || site=$?
  installed=0
  wait "$install" || installed=$?
  if [ "$installed" -ne 0 ]; then
    cat ../playwright-install.log
    echo "::error::installing Playwright's browser failed"
  fi
  [ "$site" -eq 0 ] && [ "$installed" -eq 0 ] || exit 1
fi
if [ "$step" = test ] || [ "$step" = all ]; then
  PW_BASE_URL=http://127.0.0.1:8080 npm run e2e
fi
