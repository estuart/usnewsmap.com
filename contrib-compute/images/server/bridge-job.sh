#!/usr/bin/env bash
# The Container Apps Job's command (infra/boinc/modules/bridge-job.bicep). Renders config.xml
# into tmpfs like the server does, then runs one usnm-boinc step against the hand-off root.
#
#   JOB_ID=<id>              the exported job (required for submit and reconcile)
#   BRIDGE_STEP=submit       or: reconcile (then BRIDGE_FOUND=present|absent is required)
#
# The JP2 against IIIF JPEG assessment (#456, docs/boinc-assessment.md), under
# $PROJECT_DIR/assess/<ASSESS_CAMPAIGN> on the share (one campaign per engine build;
# ASSESS_CAMPAIGN is required, lower-case letters, digits and hyphens):
#   BRIDGE_STEP=assess-stage    ASSESS_MODE=ocr|crash ASSESS_ENGINE_IMAGE=sha256:... (worker image)
#                               ASSESS_ENGINE_BASE=<its TESSERACT_IMAGE> [ASSESS_ENGINE_VERSION=5.5.0]
#                               [ASSESS_MAX=100]
#                               fetches from LoC, one request at a time, 5 s apart
#   BRIDGE_STEP=assess-submit   ASSESS_MODE=ocr|crash [ASSESS_MAX=500]
#   BRIDGE_STEP=assess-collect  ASSESS_SPOOL=<assimilator results directory>; runs score.py
#                               and aggregate.py into <campaign>/out
#
# The gold check (#462, docs/boinc.md#gold-check), under $SHARE_DIR/gold/<GOLD_CAMPAIGN> (one
# campaign per bundle and engine build; same name rule as ASSESS_CAMPAIGN). Copy the bundle tar
# to <campaign>/bundle.tar first.
#   BRIDGE_STEP=gold-prepare    fetches the original files the bundle names, one request at a
#                               time, 5 s apart, to record their sizes, md5s and the crops'
#                               reference counts (prepared.json); serves none of them
#   BRIDGE_STEP=gold-submit     GOLD_ENGINE_IMAGE=sha256:... GOLD_ENGINE_BASE=<its TESSERACT_IMAGE>
#                               [GOLD_ENGINE_VERSION=5.5.0] [GOLD_UNITS=20]
#   BRIDGE_STEP=gold-collect    GOLD_SPOOL=<assimilator results directory>; writes <campaign>/out
#
# Settled by this script, open in design.md section 9 (question 4): the job id is read from
# the environment, so each run needs a start with an environment override. UNVERIFIED.
set -euo pipefail
# shellcheck source=lib.sh
. /usr/local/lib/usnm-boinc/lib.sh
[ -e "$PROJ/.usnm-initialised" ] || fail "project is not initialised"
step="${BRIDGE_STEP:-submit}"
ASSESS=""
if [[ "$step" == assess-* ]]; then
  [[ "${ASSESS_CAMPAIGN:-}" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || fail "ASSESS_CAMPAIGN must be 1-41 of [a-z0-9-]"
  ASSESS="$SHARE_DIR/assess/$ASSESS_CAMPAIGN"
fi
GOLD=""
if [[ "$step" == gold-* ]]; then
  [[ "${GOLD_CAMPAIGN:-}" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || fail "GOLD_CAMPAIGN must be 1-41 of [a-z0-9-]"
  GOLD="$SHARE_DIR/gold/$GOLD_CAMPAIGN"
fi
assess_mode() {
  case "${ASSESS_MODE:-}" in ocr|crash) ;; *) fail "ASSESS_MODE must be ocr or crash" ;; esac
}
case "$step" in
  assess-stage)
    # No database or secrets: this step only talks to LoC and writes the share.
    assess_mode
    exec usnm-boinc assess stage --sample "$ASSESS/sample-$ASSESS_MODE.jsonl" --root "$ASSESS" \
      --mode "$ASSESS_MODE" --engine-image "${ASSESS_ENGINE_IMAGE:?set ASSESS_ENGINE_IMAGE}" \
      --engine-base "${ASSESS_ENGINE_BASE:?set ASSESS_ENGINE_BASE}" \
      --engine-version "${ASSESS_ENGINE_VERSION:-5.5.0}" \
      --max-pages "${ASSESS_MAX:-100}" --batch-cache "$ASSESS/batch-cache" ;;
  assess-collect)
    usnm-boinc assess collect --root "$ASSESS" --spool "${ASSESS_SPOOL:?set ASSESS_SPOOL}" --out "$ASSESS/out"
    python3 /usr/local/lib/usnm-boinc/assess/score.py --sample "$ASSESS/out/sample.jsonl" \
      --dir "$ASSESS/out" --results "$ASSESS/out/results.jsonl" --out "$ASSESS/out/scores.jsonl"
    python3 /usr/local/lib/usnm-boinc/assess/aggregate.py --scores "$ASSESS/out/scores.jsonl" \
      --results "$ASSESS/out/results.jsonl" --crash "$ASSESS/out/crash.jsonl" > "$ASSESS/out/tables.md"
    exit 0 ;;
  gold-prepare)
    # No database or secrets: this step only talks to the publishers and writes the share.
    exec usnm-boinc gold prepare --bundle "$GOLD/bundle.tar" --root "$GOLD" ;;
  gold-collect)
    exec usnm-boinc gold collect --root "$GOLD" --project "$PROJ" \
      --spool "${GOLD_SPOOL:?set GOLD_SPOOL}" --out "$GOLD/out" ;;
esac
if [ "$step" != assess-submit ] && [ "$step" != gold-submit ]; then
  : "${JOB_ID:?set JOB_ID}" "${HANDOFF_DIR:?}"
  [[ "$JOB_ID" =~ ^[A-Za-z0-9_-]+$ ]] || fail "JOB_ID has characters outside [A-Za-z0-9_-]"
fi
fetch_secrets
render_config
umask 0002
case "$step" in
  assess-submit)
    assess_mode
    stamp="$(mktemp)"
    rc=0
    usnm-boinc assess submit --root "$ASSESS" --project "$PROJ" --mode "$ASSESS_MODE" \
      --max-units "${ASSESS_MAX:-500}" || rc=$?
    find "$PROJ/download" -newer "$stamp" -exec chgrp boincadm {} + -exec chmod g+rX,o+rX {} + 2>/dev/null || true
    rm -f "$stamp"
    exit "$rc" ;;
  gold-submit)
    stamp="$(mktemp)"
    rc=0
    usnm-boinc gold submit --root "$GOLD" --project "$PROJ" \
      --engine-image "${GOLD_ENGINE_IMAGE:?set GOLD_ENGINE_IMAGE}" \
      --engine-base "${GOLD_ENGINE_BASE:?set GOLD_ENGINE_BASE}" \
      --engine-version "${GOLD_ENGINE_VERSION:-5.5.0}" --units "${GOLD_UNITS:-20}" || rc=$?
    for d in "$PROJ/download" "$PROJ/templates" "$PROJ/gold"; do
      [ -d "$d" ] || continue
      find "$d" -newer "$stamp" -exec chgrp boincadm {} + -exec chmod g+rX,o+rX {} + 2>/dev/null || true
    done
    rm -f "$stamp"
    exit "$rc" ;;
  submit)
    # Submission publishes readable download inputs before create_work. The
    # hand-off journal stays private; no permission repair happens afterwards.
    exec usnm-boinc submit --root "$HANDOFF_DIR" --project "$PROJ" --job "$JOB_ID" ;;
  reconcile)
    case "${BRIDGE_FOUND:-}" in present|absent) ;; *) fail "BRIDGE_FOUND must be present or absent" ;; esac
    exec usnm-boinc reconcile --root "$HANDOFF_DIR" --job "$JOB_ID" --found "$BRIDGE_FOUND" ;;
  *) fail "BRIDGE_STEP must be submit, reconcile, assess-stage, assess-submit, assess-collect, gold-prepare, gold-submit or gold-collect" ;;
esac
