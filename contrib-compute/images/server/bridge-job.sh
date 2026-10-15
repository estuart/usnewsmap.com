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
#   BRIDGE_STEP=gold-trust      [GOLD_MAX_CER=0.10]; writes $SHARE_DIR/gold/trust.json from this
#                               campaign's out/reports.jsonl, for `usnm-boinc collect --trust`
#   BRIDGE_STEP=gold-bundle     builds the bundle v1 from its publishers onto the share, checks it
#                               against the committed lock and writes <campaign>/bundle.tar
#                               (gold-bundle.sh; the gold bundle job, bridge image only)
#
# The Surya 2 test (#516, docs/boinc-surya.md), under $SHARE_DIR/surya/<SURYA_CAMPAIGN> (same
# name rule as ASSESS_CAMPAIGN):
#   BRIDGE_STEP=surya-stage     SURYA_PAGES=group:lang:doc_id:batch[+batch...],... (at most
#                               SURYA_MAX, default 20; batches as <name>_ver<NN>)
#                               SURYA_ENGINE_IMAGE=sha256:... (the usnm_surya worker image)
#                               SURYA_ENGINE_LLAMA=sha256:... SURYA_ENGINE_WEIGHTS=sha256:...
#                               (its USNM_SURYA_LLAMA_IMAGE and USNM_SURYA_WEIGHTS)
#                               gets each page's IIIF JPEG from the scan archive, else from
#                               tile.loc.gov through its batch, one request at a time, 5 s apart
#
# assess-stage, surya-stage and quality-stage read the scan archive first when SCAN_ARCHIVE_URL
# is set (https://<scansAccount>.blob.core.windows.net/scans, with the job's identity); none of
# them asks www.loc.gov's item API.
#   BRIDGE_STEP=surya-submit    [SURYA_MAX=20]
#   BRIDGE_STEP=surya-summary   prints the kept results and compared pairs (<project>/surya) as
#                               a Markdown table, and writes it to <campaign>/summary.md
#
# The OCR quality estimate (#522, docs/boinc-quality.md), under $SHARE_DIR/quality/<QUALITY_CAMPAIGN>
# and $SHARE_DIR/qwen/<QWEN_CAMPAIGN> (same name rule as ASSESS_CAMPAIGN):
#   BRIDGE_STEP=quality-stage   [HANDOFF_URL=...] (pulls texts/ into <campaign>/texts)
#                               [QUALITY_LANGS=eng,deu,...] (writes <campaign>/function-words)
#                               [QUALITY_PAGES=group:lang:doc_id:batch[+batch...],...]
#                               [QUALITY_SAMPLE=<path, default <campaign>/sample.jsonl>]
#                               [QUALITY_GROUP=sample] [QUALITY_READERS=loc,as,ours-tesseract]
#                               [QUALITY_PLACES=<campaign>/places.txt]
#                               QUALITY_ENGINE_IMAGE=sha256:... [QUALITY_MAX=100]
#   BRIDGE_STEP=quality-submit  [QUALITY_MAX=500]
#   BRIDGE_STEP=quality-collect QUALITY_SPOOL=<assimilator results directory>
#                               [QWEN_CAMPAIGN=... QWEN_SPOOL=...]
#                               [QUALITY_CURATED_URL=...] (else writes <campaign>/out)
#   BRIDGE_STEP=quality-summary writes <campaign>/summary.md from <project>/quality
#   BRIDGE_STEP=qwen-stage      QUALITY_CAMPAIGN=<quality campaign>
#                               QWEN_ENGINE_IMAGE=sha256:... QWEN_ENGINE_LLAMA=sha256:...
#                               QWEN_ENGINE_WEIGHTS=sha256:... [QWEN_MAX=20]
#   BRIDGE_STEP=qwen-submit     [QWEN_MAX=20]
#   BRIDGE_STEP=qwen-summary    writes <campaign>/summary.md from <project>/qwen
#
# The remote hand-off (#469, docs/boinc.md#one-page-from-the-scan-cache), through the
# hand-off container HANDOFF_URL=https://<account>.blob.core.windows.net/boinc-handoff, read
# and written with this job's identity:
#   BRIDGE_STEP=submit-remote   JOB_ID; copies the exported job from the container's
#                               dispatch/jobs/<id>/ into $HANDOFF_DIR (checking the page's
#                               size and sha256 against the request) and submits it. A job
#                               already past reserved is left. LoC is never asked.
#   BRIDGE_STEP=spool-out       JOB_ID HANDOFF_SPOOL=<assimilator results directory under
#                               $PROJ/results>; writes the job's spool files and the canonical
#                               result's host id to spool/<id>/, and $SHARE_DIR/gold/trust.json
#                               (when there) to gold/trust.json
#
# Registering the apps after init (docs/boinc.md#register-the-apps):
#   BRIDGE_STEP=register-apps   copies this image's project.xml into the project and runs
#                               bin/xadd, which adds the apps and platforms the database lacks
#                               and leaves the others (safe to repeat)
#
# Start a step with scripts/start-boinc-bridge.sh, which sets these on the job: an
# `az containerapp job start --env-vars` override drops the job's own settings and the share.
#
# The step and its settings (JOB_ID included) are read from the environment. Each run's are
# written into the job's own template by scripts/start-boinc-bridge.sh, which then starts it
# with no execution override (design.md section 9, question 4). UNVERIFIED against a live job.
set -euo pipefail
# shellcheck source=lib.sh disable=SC1091
. /usr/local/lib/usnm-boinc/lib.sh
[ -e "$PROJ/.usnm-initialised" ] || fail "project is not initialised"
step="${BRIDGE_STEP:-submit}"
ASSESS=""
if [[ "$step" == assess-* ]]; then
  [[ "${ASSESS_CAMPAIGN:-}" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || fail "ASSESS_CAMPAIGN must be 1-41 of [a-z0-9-]"
  ASSESS="$SHARE_DIR/assess/$ASSESS_CAMPAIGN"
fi
SURYA=""
if [[ "$step" == surya-* ]]; then
  [[ "${SURYA_CAMPAIGN:-}" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || fail "SURYA_CAMPAIGN must be 1-41 of [a-z0-9-]"
  SURYA="$SHARE_DIR/surya/$SURYA_CAMPAIGN"
fi
QUALITY=""
if [[ "$step" == quality-* ]]; then
  [[ "${QUALITY_CAMPAIGN:-}" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || fail "QUALITY_CAMPAIGN must be 1-41 of [a-z0-9-]"
  QUALITY="$SHARE_DIR/quality/$QUALITY_CAMPAIGN"
fi
QWEN=""
if [[ "$step" == qwen-* ]]; then
  [[ "${QWEN_CAMPAIGN:-}" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || fail "QWEN_CAMPAIGN must be 1-41 of [a-z0-9-]"
  QWEN="$SHARE_DIR/qwen/$QWEN_CAMPAIGN"
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
    # No database or secrets: this step only reads the scan archive and LoC and writes the share.
    assess_mode
    exec usnm-boinc assess stage --sample "$ASSESS/sample-$ASSESS_MODE.jsonl" --root "$ASSESS" \
      --mode "$ASSESS_MODE" --engine-image "${ASSESS_ENGINE_IMAGE:?set ASSESS_ENGINE_IMAGE}" \
      --engine-base "${ASSESS_ENGINE_BASE:?set ASSESS_ENGINE_BASE}" \
      --engine-version "${ASSESS_ENGINE_VERSION:-5.5.0}" \
      --max-pages "${ASSESS_MAX:-100}" --batch-cache "$ASSESS/batch-cache" \
      ${SCAN_ARCHIVE_URL:+--scan-archive "$SCAN_ARCHIVE_URL"} ;;
  assess-collect)
    usnm-boinc assess collect --root "$ASSESS" --spool "${ASSESS_SPOOL:?set ASSESS_SPOOL}" --out "$ASSESS/out"
    python3 /usr/local/lib/usnm-boinc/assess/score.py --sample "$ASSESS/out/sample.jsonl" \
      --dir "$ASSESS/out" --results "$ASSESS/out/results.jsonl" --out "$ASSESS/out/scores.jsonl"
    python3 /usr/local/lib/usnm-boinc/assess/aggregate.py --scores "$ASSESS/out/scores.jsonl" \
      --results "$ASSESS/out/results.jsonl" --crash "$ASSESS/out/crash.jsonl" > "$ASSESS/out/tables.md"
    exit 0 ;;
  surya-stage)
    # No database or secrets: this step only reads the scan archive and LoC and writes the share.
    [[ "${SURYA_PAGES:-}" =~ ^[a-z0-9:_,+-]+$ ]] || fail "SURYA_PAGES must be group:lang:doc_id:batch[+batch...],..."
    page_args=()
    IFS=, read -r -a pages <<< "$SURYA_PAGES"
    for p in "${pages[@]}"; do page_args+=(--page "$p"); done
    exec usnm-boinc surya stage "${page_args[@]}" --root "$SURYA" \
      --engine-image "${SURYA_ENGINE_IMAGE:?set SURYA_ENGINE_IMAGE}" \
      --engine-llama "${SURYA_ENGINE_LLAMA:?set SURYA_ENGINE_LLAMA}" \
      --engine-weights "${SURYA_ENGINE_WEIGHTS:?set SURYA_ENGINE_WEIGHTS}" \
      --max-pages "${SURYA_MAX:-20}" --batch-cache "$SURYA/batch-cache" \
      ${SCAN_ARCHIVE_URL:+--scan-archive "$SCAN_ARCHIVE_URL"} ;;
  surya-summary)
    # No database or secrets: reads what the validator kept and writes the campaign's summary.
    usnm-boinc surya summary --root "$SURYA" --seen "$PROJ/surya/seen" --pairs "$PROJ/surya/pairs.jsonl" \
      | tee "$SURYA/summary.md"
    exit 0 ;;
  quality-stage)
    # No database or secrets: reads the hand-off texts, scan archive and LoC, writes the share.
    mkdir -p "$QUALITY/texts" "$QUALITY/function-words"
    if [ -n "${HANDOFF_URL:-}" ]; then
      [[ "$HANDOFF_URL" =~ ^https://[a-z0-9]{3,24}\.blob\.core\.windows\.net/boinc-handoff$ ]] ||
        fail "HANDOFF_URL must be https://<account>.blob.core.windows.net/boinc-handoff"
      usnm-boinc handoff get --from-url "$HANDOFF_URL" --prefix texts --to "$QUALITY/texts"
    fi
    if [ -n "${QUALITY_LANGS:-}" ]; then
      [[ "$QUALITY_LANGS" =~ ^[a-z,]+$ ]] || fail "QUALITY_LANGS must be comma-separated ISO 639-3 codes"
      lang_args=()
      IFS=, read -r -a langs <<< "$QUALITY_LANGS"
      for l in "${langs[@]}"; do lang_args+=(--lang "$l"); done
      usnm-boinc quality function-words "${lang_args[@]}" --out-dir "$QUALITY/function-words"
    fi
    page_args=()
    if [ -n "${QUALITY_PAGES:-}" ]; then
      [[ "$QUALITY_PAGES" =~ ^[a-z0-9:_,+-]+$ ]] || fail "QUALITY_PAGES must be group:lang:doc_id:batch[+batch...],..."
      IFS=, read -r -a pages <<< "$QUALITY_PAGES"
      for p in "${pages[@]}"; do page_args+=(--page "$p"); done
    fi
    sample="${QUALITY_SAMPLE:-}"
    if [ -z "$sample" ] && [ -f "$QUALITY/sample.jsonl" ]; then
      sample="$QUALITY/sample.jsonl"
    fi
    if [ -n "$sample" ]; then
      [[ "${QUALITY_GROUP:-sample}" =~ ^[a-z0-9_-]+$ ]] || fail "QUALITY_GROUP must be [a-z0-9_-]+"
      page_args+=(--sample "$sample" --group "${QUALITY_GROUP:-sample}")
    fi
    [ "${#page_args[@]}" -gt 0 ] || fail "set QUALITY_PAGES or QUALITY_SAMPLE (or place $QUALITY/sample.jsonl)"
    [[ "${QUALITY_READERS:-loc,as,ours-tesseract}" =~ ^[a-z0-9_,-]+$ ]] ||
      fail "QUALITY_READERS must be comma-separated reader names"
    exec usnm-boinc quality stage "${page_args[@]}" --root "$QUALITY" \
      --texts-dir "$QUALITY/texts" --places "${QUALITY_PLACES:-$QUALITY/places.txt}" \
      --function-words-dir "$QUALITY/function-words" \
      --readers "${QUALITY_READERS:-loc,as,ours-tesseract}" \
      --engine-image "${QUALITY_ENGINE_IMAGE:?set QUALITY_ENGINE_IMAGE}" \
      --max-pages "${QUALITY_MAX:-100}" --batch-cache "$QUALITY/batch-cache" \
      ${SCAN_ARCHIVE_URL:+--scan-archive "$SCAN_ARCHIVE_URL"} ;;
  quality-collect)
    qwen_root="$QUALITY/no-qwen"
    qwen_spool="${QWEN_SPOOL:-$QUALITY/no-qwen}"
    if [ -n "${QWEN_CAMPAIGN:-}" ]; then
      [[ "$QWEN_CAMPAIGN" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || fail "QWEN_CAMPAIGN must be 1-41 of [a-z0-9-]"
      qwen_root="$SHARE_DIR/qwen/$QWEN_CAMPAIGN"
      qwen_spool="${QWEN_SPOOL:?set QWEN_SPOOL}"
    fi
    out_args=(--out-dir "$QUALITY/out")
    [ -n "${QUALITY_CURATED_URL:-}" ] && out_args=(--curated-url "$QUALITY_CURATED_URL")
    exec usnm-boinc quality collect --campaign "$QUALITY_CAMPAIGN" --root "$QUALITY" \
      --spool "${QUALITY_SPOOL:?set QUALITY_SPOOL}" --qwen-root "$qwen_root" --qwen-spool "$qwen_spool" \
      "${out_args[@]}" ;;
  quality-summary)
    usnm-boinc quality summary --root "$QUALITY" --seen "$PROJ/quality/seen" --pairs "$PROJ/quality/pairs.jsonl" \
      | tee "$QUALITY/summary.md"
    exit 0 ;;
  qwen-stage)
    [[ "${QUALITY_CAMPAIGN:-}" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || fail "QUALITY_CAMPAIGN must be 1-41 of [a-z0-9-]"
    exec usnm-boinc qwen stage --quality-root "$SHARE_DIR/quality/$QUALITY_CAMPAIGN" --root "$QWEN" \
      --engine-image "${QWEN_ENGINE_IMAGE:?set QWEN_ENGINE_IMAGE}" \
      --engine-llama "${QWEN_ENGINE_LLAMA:?set QWEN_ENGINE_LLAMA}" \
      --engine-weights "${QWEN_ENGINE_WEIGHTS:?set QWEN_ENGINE_WEIGHTS}" \
      --max-pages "${QWEN_MAX:-20}" ;;
  qwen-summary)
    usnm-boinc qwen summary --root "$QWEN" --seen "$PROJ/qwen/seen" --pairs "$PROJ/qwen/pairs.jsonl" \
      | tee "$QWEN/summary.md"
    exit 0 ;;
  gold-prepare)
    # No database or secrets: this step only talks to the publishers and writes the share.
    exec usnm-boinc gold prepare --bundle "$GOLD/bundle.tar" --root "$GOLD" ;;
  gold-bundle)
    # No database or secrets: this step only talks to the datasets' publishers and writes the share.
    [ -f /usr/local/lib/usnm-boinc/gold-bundle.sh ] || fail "gold-bundle needs the bridge image"
    exec bash /usr/local/lib/usnm-boinc/gold-bundle.sh --share "$SHARE_DIR" --campaign "$GOLD_CAMPAIGN" ;;
  gold-collect)
    exec usnm-boinc gold collect --root "$GOLD" --project "$PROJ" \
      --spool "${GOLD_SPOOL:?set GOLD_SPOOL}" --out "$GOLD/out" ;;
  gold-trust)
    # No database or secrets: reads this campaign's reports and writes the share's trust list.
    exec usnm-boinc gold trust --reports "$GOLD/out/reports.jsonl" \
      --max-cer "${GOLD_MAX_CER:-0.10}" --out "$SHARE_DIR/gold/trust.json" ;;
esac
if [ "$step" != assess-submit ] && [ "$step" != gold-submit ] && [ "$step" != surya-submit ] &&
  [ "$step" != quality-submit ] && [ "$step" != qwen-submit ] && [ "$step" != register-apps ]; then
  : "${JOB_ID:?set JOB_ID}" "${HANDOFF_DIR:?}"
  [[ "$JOB_ID" =~ ^[A-Za-z0-9_-]+$ ]] || fail "JOB_ID has characters outside [A-Za-z0-9_-]"
fi
if [ "$step" = submit-remote ] || [ "$step" = spool-out ]; then
  [[ "${HANDOFF_URL:-}" =~ ^https://[a-z0-9]{3,24}\.blob\.core\.windows\.net/boinc-handoff$ ]] ||
    fail "HANDOFF_URL must be https://<account>.blob.core.windows.net/boinc-handoff"
fi
if [ "$step" = spool-out ]; then
  # Reads the spool and, for the host id, the database; writes only the hand-off container.
  spool="${HANDOFF_SPOOL:?set HANDOFF_SPOOL}"
  case "$spool" in "$PROJ"/results/*) ;; *) fail "HANDOFF_SPOOL must be under $PROJ/results" ;; esac
  [[ "$spool" != *..* ]] || fail "HANDOFF_SPOOL must not contain .."
  host_args=()
  mkdir -p "$SECRETS"; chmod 700 "$SECRETS"
  mycnf="$SECRETS/mysql-runtime.cnf"
  trap 'rm -f "$mycnf"' EXIT
  kv_get mysql-runtime-password "$SECRETS/db-runtime"
  (umask 077; { echo '[client]'; echo "host=$DB_HOST"; echo "user=$DB_RUNTIME_USER"; echo "password=$(cat "$SECRETS/db-runtime")"; } > "$mycnf")
  # The canonical result's host (BOINC's result.hostid); JOB_ID is [A-Za-z0-9_-] (above).
  # UNVERIFIED against a live database.
  if host=$(mysql --defaults-extra-file="$mycnf" --batch --skip-column-names "$DB_NAME" \
      -e "SELECT r.hostid FROM workunit w JOIN result r ON r.id = w.canonical_resultid WHERE w.name = 'usnm_$JOB_ID'"); then
    if [[ "$host" =~ ^[0-9]+$ ]]; then host_args=(--host-id "$host"); else log "no canonical result host for usnm_$JOB_ID yet"; fi
  else
    log "could not read the canonical result's host id; the node must find it another way"
  fi
  trust_args=()
  [ -f "$SHARE_DIR/gold/trust.json" ] && trust_args=(--trust "$SHARE_DIR/gold/trust.json")
  usnm-boinc handoff put-spool --to-url "$HANDOFF_URL" --spool "$spool" --job "$JOB_ID" \
    ${host_args[@]+"${host_args[@]}"} ${trust_args[@]+"${trust_args[@]}"}
  exit 0
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
  surya-submit)
    stamp="$(mktemp)"
    rc=0
    usnm-boinc surya submit --root "$SURYA" --project "$PROJ" --max-units "${SURYA_MAX:-20}" || rc=$?
    for d in "$PROJ/download" "$PROJ/templates"; do
      find "$d" -newer "$stamp" -exec chgrp boincadm {} + -exec chmod g+rX,o+rX {} + 2>/dev/null || true
    done
    rm -f "$stamp"
    exit "$rc" ;;
  quality-submit)
    stamp="$(mktemp)"
    rc=0
    usnm-boinc quality submit --root "$QUALITY" --project "$PROJ" --max-units "${QUALITY_MAX:-500}" || rc=$?
    for d in "$PROJ/download" "$PROJ/templates"; do
      find "$d" -newer "$stamp" -exec chgrp boincadm {} + -exec chmod g+rX,o+rX {} + 2>/dev/null || true
    done
    rm -f "$stamp"
    exit "$rc" ;;
  qwen-submit)
    stamp="$(mktemp)"
    rc=0
    usnm-boinc qwen submit --root "$QWEN" --project "$PROJ" --max-units "${QWEN_MAX:-20}" || rc=$?
    for d in "$PROJ/download" "$PROJ/templates"; do
      find "$d" -newer "$stamp" -exec chgrp boincadm {} + -exec chmod g+rX,o+rX {} + 2>/dev/null || true
    done
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
  register-apps)
    # BOINC's tools/xadd looks up each <platform> and <app> of project.xml by name: it inserts
    # the ones the database lacks, updates a changed user_friendly_name or min_version, and
    # leaves the rest; it never deletes. So a rerun changes nothing (BOINC 22bbebe, tools/xadd).
    cp "$TEMPLATES/project.xml" "$PROJ/project.xml"
    cd "$PROJ"
    exec bin/xadd ;;
  submit-remote)
    # The page comes from the hand-off container, which the scan fetcher filled; never from
    # LoC. Submission then runs as for submit, which checks the page again.
    state=$(usnm-boinc handoff pull --from-url "$HANDOFF_URL" --root "$HANDOFF_DIR" --job "$JOB_ID")
    if [ "$state" != reserved ]; then
      log "job $JOB_ID is already $state on the server; nothing submitted"
      exit 0
    fi
    exec usnm-boinc submit --root "$HANDOFF_DIR" --project "$PROJ" --job "$JOB_ID" ;;
  submit)
    # Submission publishes readable download inputs before create_work. The
    # hand-off journal stays private; no permission repair happens afterwards.
    exec usnm-boinc submit --root "$HANDOFF_DIR" --project "$PROJ" --job "$JOB_ID" ;;
  reconcile)
    case "${BRIDGE_FOUND:-}" in present|absent) ;; *) fail "BRIDGE_FOUND must be present or absent" ;; esac
    exec usnm-boinc reconcile --root "$HANDOFF_DIR" --job "$JOB_ID" --found "$BRIDGE_FOUND" ;;
  *) fail "BRIDGE_STEP must be submit, submit-remote, spool-out, reconcile, register-apps, assess-stage, assess-submit, assess-collect, surya-stage, surya-submit, surya-summary, quality-stage, quality-submit, quality-collect, quality-summary, qwen-stage, qwen-submit, qwen-summary, gold-prepare, gold-submit, gold-collect, gold-trust or gold-bundle" ;;
esac
