#!/usr/bin/env bash
# The BOINC hand-off job's command (infra/modules/boinc-handoff-job.bicep, #469): the bridge
# image run in the main Container Apps environment, which reaches Cosmos, the lake and the
# exchange account privately. It never talks to the BOINC server: the hand-off container
# (HANDOFF_URL) sits between them. Every path is scratch in the replica; the dispatch root
# lives in the container under dispatch/.
#
# The job's own settings: HANDOFF_URL COSMOS_ENDPOINT CURATED_URL REFERENCE_URL OUT_URL
# AZURE_CLIENT_ID. Each run's (scripts/start-boinc-bridge.sh --target handoff):
#   HANDOFF_STEP=export   HANDOFF_PAGES=<page key>[,<page key>...] HANDOFF_BATCH=<batch>
#                         HANDOFF_PLAN=<plan id> HANDOFF_MANIFEST=<path under manifests/, as
#                         push-boinc-scan.sh names it: <version>/manifest.parquet>
#                         ENGINE_IMAGE=sha256:<the worker image's digest>
#                         copies scans/ and the manifest down, runs usnm-boinc export (the
#                         ocr-sync lease in Cosmos, the native ledger reservation) and writes
#                         back the journal entries it touched; prints the job ids
#   HANDOFF_STEP=collect  JOB_ID [HOST_ID=<the canonical result's host id>]
#                         copies the dispatch root, spool/<id>/ and gold/trust.json down, runs
#                         usnm-boinc collect --trust into ocr-out (a terminal BOINC failure:
#                         collect without a host or trust list, which marks the job failed) and
#                         writes back that job's entry
#
# Only the entries a run touched go back, so runs for different jobs don't undo each other.
# Run one execution at a time all the same (scripts/start-boinc-bridge.sh refuses to start
# while one runs). If writing an entry back keeps failing, its record is printed to the log
# (USNM-HANDOFF-RECORD lines) so it can be restored by hand: a lost reservation's record
# can't be made again (its queued_at is in the ledger).
set -euo pipefail
log() { echo "usnm-boinc-handoff: $*" >&2; }
fail() { log "$*"; exit 1; }
for v in HANDOFF_URL COSMOS_ENDPOINT CURATED_URL REFERENCE_URL OUT_URL; do
  [ -n "${!v:-}" ] || fail "$v is not set (the job's own setting)"
done
[[ $HANDOFF_URL =~ ^https://[a-z0-9]{3,24}\.blob\.core\.windows\.net/boinc-handoff$ ]] ||
  fail "HANDOFF_URL must be https://<account>.blob.core.windows.net/boinc-handoff"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root="$work/dispatch"
usnm-boinc handoff get --from-url "$HANDOFF_URL" --prefix dispatch --to "$root"
# put_back DIR...: writes each journal directory (relative to the root) back, retrying.
put_back() {
  local d try
  for d in "$@"; do
    [ -d "$root/$d" ] || continue
    for try in 1 2 3 4 5; do
      usnm-boinc handoff put --from "$root/$d" --to-url "$HANDOFF_URL" --prefix "dispatch/$d" && continue 2
      log "writing dispatch/$d back failed (attempt $try)"
      sleep $((try * 10))
    done
    find "$root/$d" -name record.json -exec sh -c 'printf "USNM-HANDOFF-RECORD %s %s\n" "$1" "$(tr -d "\n" < "$1")"' _ {} \;
    fail "could not write dispatch/$d back; its record is in this log"
  done
}
case "${HANDOFF_STEP:-}" in
  export)
    [[ ${HANDOFF_MANIFEST:-} =~ ^[A-Za-z0-9][A-Za-z0-9._-]*(/[A-Za-z0-9][A-Za-z0-9._-]*)*$ ]] &&
      [[ $HANDOFF_MANIFEST != *..* ]] || fail "HANDOFF_MANIFEST is a path under manifests/"
    [[ ${HANDOFF_PAGES:-} =~ ^[A-Za-z0-9/_,-]+$ ]] || fail "HANDOFF_PAGES is a comma-separated list of page keys"
    : "${HANDOFF_BATCH:?set HANDOFF_BATCH}" "${HANDOFF_PLAN:?set HANDOFF_PLAN}" "${ENGINE_IMAGE:?set ENGINE_IMAGE}"
    usnm-boinc handoff get --from-url "$HANDOFF_URL" --prefix scans --to "$work/scans"
    usnm-boinc handoff get --from-url "$HANDOFF_URL" --prefix manifests --to "$work/manifests"
    [ -f "$work/manifests/$HANDOFF_MANIFEST" ] || fail "no manifests/$HANDOFF_MANIFEST in the hand-off container"
    tr ',' '\n' <<< "$HANDOFF_PAGES" > "$work/pages.txt"
    stamp="$work/stamp"
    touch "$stamp"
    sleep 1
    rc=0
    usnm-boinc export --manifest "$work/manifests/$HANDOFF_MANIFEST" --pages "$work/pages.txt" \
      --batch "$HANDOFF_BATCH" --scans "$work/scans" --root "$root" --plan "$HANDOFF_PLAN" \
      --image "$ENGINE_IMAGE" --curated-url "$CURATED_URL" --reference-url "$REFERENCE_URL" \
      --cosmos "$COSMOS_ENDPOINT" || rc=$?
    # Whatever export got to (it journals each entry before it reserves) goes back.
    touched=()
    if [ -d "$root/jobs" ]; then
      while IFS= read -r f; do touched+=("jobs/$(basename "$(dirname "$f")")"); done \
        < <(find "$root/jobs" -mindepth 2 -maxdepth 2 -name record.json -newer "$stamp")
    fi
    put_back ${touched[@]+"${touched[@]}"}
    exit "$rc" ;;
  collect)
    [[ ${JOB_ID:-} =~ ^[0-9a-f]{64}$ ]] || fail "JOB_ID is a job id (64 hex)"
    host=$(usnm-boinc handoff get-spool --from-url "$HANDOFF_URL" --job "$JOB_ID" \
      --spool "$work/spool" --trust-out "$work/trust.json")
    rc=0
    if [ -e "$work/spool/usnm_${JOB_ID}_error" ] || [ -e "$work/spool/usnm_${JOB_ID}_no_output_files" ]; then
      # A terminal BOINC failure has no canonical host; collect marks the job failed.
      usnm-boinc collect --root "$root" --spool "$work/spool" --job "$JOB_ID" \
        --curated-url "$CURATED_URL" --out-url "$OUT_URL" || rc=$?
    else
      if [ -n "${HOST_ID:-}" ]; then
        [[ $HOST_ID =~ ^[0-9]+$ ]] || fail "HOST_ID is a number"
        [ "$host" = unknown ] || [ "$host" = "$HOST_ID" ] || fail "HOST_ID $HOST_ID is not the server's $host"
        host=$HOST_ID
      fi
      [ "$host" != unknown ] || fail "the server recorded no host id; read it from the ops pages and pass HOST_ID"
      [ -f "$work/trust.json" ] || fail "no gold/trust.json in the hand-off container (run spool-out after gold-trust)"
      usnm-boinc collect --root "$root" --spool "$work/spool" --job "$JOB_ID" \
        --curated-url "$CURATED_URL" --out-url "$OUT_URL" --trust "$work/trust.json" --host-id "$host" || rc=$?
    fi
    put_back "jobs/$JOB_ID" "quarantine/$JOB_ID"
    exit "$rc" ;;
  *) fail "HANDOFF_STEP must be export or collect" ;;
esac
