#!/usr/bin/env bash
# The Container Apps Job's command (infra/boinc/modules/bridge-job.bicep). Renders config.xml
# into tmpfs like the server does, then runs one usnm-boinc step against the hand-off root.
#
#   JOB_ID=<id>              the exported job (required)
#   BRIDGE_STEP=submit       or: reconcile (then BRIDGE_FOUND=present|absent is required)
#
# Settled by this script, open in design.md section 9 (question 4): the job id is read from
# the environment, so each run needs a start with an environment override. UNVERIFIED.
set -euo pipefail
# shellcheck source=lib.sh
. /usr/local/lib/usnm-boinc/lib.sh
: "${JOB_ID:?set JOB_ID}" "${HANDOFF_DIR:?}"
[[ "$JOB_ID" =~ ^[A-Za-z0-9_-]+$ ]] || fail "JOB_ID has characters outside [A-Za-z0-9_-]"
[ -e "$PROJ/.usnm-initialised" ] || fail "project is not initialised"
fetch_secrets
render_config
umask 0002
case "${BRIDGE_STEP:-submit}" in
  submit)
    # usnm-boinc stages downloads with NamedTempFile (mode 0600, root-owned here), which Apache
    # (www-data) cannot serve. Give what the step wrote the BOINC shared-group modes afterwards.
    # UNVERIFIED: this is after create_work, so a client could ask in the gap. Upstream follow-up
    # in the usnm-boinc crate (#441): chmod g+r before create_work, then drop this loop.
    stamp="$(mktemp)"
    rc=0
    usnm-boinc submit --root "$HANDOFF_DIR" --project "$PROJ" --job "$JOB_ID" || rc=$?
    for d in "$PROJ/download" "$HANDOFF_DIR"; do
      [ -d "$d" ] || continue
      find "$d" -newer "$stamp" -exec chgrp boincadm {} + -exec chmod g+rX,o+rX {} + 2>/dev/null || true
    done
    rm -f "$stamp"
    exit "$rc" ;;
  reconcile)
    case "${BRIDGE_FOUND:-}" in present|absent) ;; *) fail "BRIDGE_FOUND must be present or absent" ;; esac
    exec usnm-boinc reconcile --root "$HANDOFF_DIR" --job "$JOB_ID" --found "$BRIDGE_FOUND" ;;
  *) fail "BRIDGE_STEP must be submit or reconcile" ;;
esac
