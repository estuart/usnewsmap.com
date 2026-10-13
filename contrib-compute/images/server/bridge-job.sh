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
    # Submission publishes readable download inputs before create_work. The
    # hand-off journal stays private; no permission repair happens afterwards.
    exec usnm-boinc submit --root "$HANDOFF_DIR" --project "$PROJ" --job "$JOB_ID" ;;
  reconcile)
    case "${BRIDGE_FOUND:-}" in present|absent) ;; *) fail "BRIDGE_FOUND must be present or absent" ;; esac
    exec usnm-boinc reconcile --root "$HANDOFF_DIR" --job "$JOB_ID" --found "$BRIDGE_FOUND" ;;
  *) fail "BRIDGE_STEP must be submit or reconcile" ;;
esac
