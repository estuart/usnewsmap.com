#!/usr/bin/env bash
# Which parts of CI a change needs, as step outputs rust/web/image/fixtures/
# ops/code/markdown/ocr_worker/page_language=true|false (ocr_worker: the OCR
# worker's EL9 build, its Tesseract image and, on main, its release;
# page_language: the page-language detector's test on text of known
# language; code: anything but
# documentation changed, so the infrastructure checks and the shared-key scan run; markdown: a Markdown
# file changed, so Prettier checks its format), and provision=true|false:
# whether it changes the Azure stack beyond what a deploy applies (the API
# image and infra/quickwit/searcher.yaml, which scripts/ci/roll-api.sh
# applies), so scripts/provision.sh must run. Documentation (Markdown
# anywhere, anything under docs/) counts for no output but markdown: a change
# to it alone runs only the Markdown format check.
# contrib: the hosting for the stock BOINC server (contrib-compute/: shell,
# XML and Bicep; ADR-0019), which no other job builds.
# Everything runs for a workflow_dispatch (scripts/bootstrap.sh dispatches ci
# to publish images), for a change to the workflows themselves, and whenever
# the changed files can't be worked out; everything but ops, which runs only
# when its own files change (the snapshots, the index history, their schemas,
# checkers and writers, its requirements, or ci.yml, which defines the job).
# The infra job (code) checks the workbook definitions, so any change to
# infra/workbooks/ runs it.
set -euo pipefail
out=${GITHUB_OUTPUT:-/dev/stdout}
# Known only once the changed files are; "run everything" keeps them.
provision=false
ops=false
forced=
all() {
  printf 'rust=true\nquickwit=true\nweb=true\nimage=true\nfixtures=true\nops=%s\ncode=true\nmarkdown=true\nprovision=%s\nocr_worker=true\npage_language=true\ncontrib=true\n' "$ops" "$provision" >> "$out"
  echo "running everything: $1"
  exit 0
}
case "${GITHUB_EVENT_NAME:-}" in
  pull_request) range="origin/$GITHUB_BASE_REF...HEAD" ;;
  push)
    before=${BEFORE_SHA:-}
    [ -n "$before" ] && ! [[ $before =~ ^0+$ ]] || all "no previous commit"
    # Two dots: the trees before and after the push (a force push included).
    range="$before..$GITHUB_SHA"
    ;;
  *) all "event ${GITHUB_EVENT_NAME:-unknown}" ;;
esac
# Both sides of a rename: a source file moved under docs/ is still a removal
# the checks must see.
files=$(git diff --name-only --no-renames "$range") || all "git diff failed for $range"
[ -n "$files" ] || all "no changed files found"
echo "$files" | sed 's/^/changed: /'
# Prettier formats Markdown anywhere (.prettierrc.json, .prettierignore).
if grep -Eq '\.md$' <<< "$files"; then markdown=true; else markdown=false; fi
# Documentation is only read: nothing below counts it. No README is compiled
# into a binary or an image.
files=$(grep -Ev '\.md$|^docs/' <<< "$files" || true)
matches() { [ -n "$files" ] && grep -Eq "$1" <<< "$files"; }
# infra/quickwit/ holds the index configs (read by the ingest image) and the
# searcher config (applied on deploy). Everything else under infra/ needs a
# provision, an environment's committed settings (infra/env/) included.
if [ -n "$files" ] && grep -E '^infra/' <<< "$files" | grep -Evq '^infra/quickwit/'; then
  provision=true
fi
# The index version snapshots and the index history must match their schemas
# (and what writes them); searches.sh's date-span summary has its tests there too.
matches '^(ops/version-snapshots/|ops/index-history(\.schema)?\.json$|scripts/(check-version-snapshots|test_check_version_snapshots|compare-versions|check-index-history|test_check_index_history|reconstruct-index-history|test_date_spans)\.py$|scripts/lib/date-spans\.jq$|scripts/ci/requirements-ops\.txt$|\.github/workflows/ci\.yml$)' && ops=true
# Another workflow or dependabot.yml defines no job here. An edit to ci.yml
# outside a job (triggers, env) or to the changes job runs everything; an edit
# inside other jobs forces just the flags those jobs' `if:` lines read.
matches '^\.github/workflows/ci\.yml$' && {
  lines=$(mktemp)
  forced=$(git diff -U0 "$range" -- .github/workflows/ci.yml | awk '
    /^@@/ { split($3, a, ","); start = substr(a[1], 2) + 0; n = (a[2] == "" ? 1 : a[2] + 0)
            for (i = 0; i < n; i++) changed[start + i] = 1; if (n == 0) changed[start] = 1; next }
    END { for (l in changed) print l }' | sort -n > "$lines"
    awk -v list="$lines" '
      BEGIN { while ((getline l < list) > 0) hit[l] = 1 }
      /^jobs:/ { injobs = 1; next }
      !injobs { if (hit[NR]) print "ALL"; next }
      /^  [a-z0-9_-]+:$/ { job = $1; if (job == "changes:") job = "ALL"; hitjob[job] = hitjob[job] || hit[NR]; cur = job; next }
      { if (hit[NR]) hitjob[cur] = 1
        while (match($0, /outputs\.[a-z_]+/)) { flags[cur] = flags[cur] " " substr($0, RSTART + 8, RLENGTH - 8); $0 = substr($0, RSTART + RLENGTH) } }
      END { for (j in hitjob) if (hitjob[j]) { if (j == "ALL") print "ALL"; else print flags[j] } }' .github/workflows/ci.yml | tr -s ' \n' ' ')
  rm -f "$lines"
  case " $forced " in *" ALL "*) all "workflow changed" ;; esac
  [[ " $forced " == *" markdown "* ]] && markdown=true
  echo "workflow jobs changed, forcing:$forced"
}
# The Rust build and tests: the crates, the files compiled into binaries
# (index config, place overrides, the example searches the API warms, the
# reconstructed index history), the fixtures the tests read, and the Python
# metric the damage port is checked against (its tables, its word lists and
# their sha256).
rust='^(crates/|Cargo\.(toml|lock)$|rust-toolchain\.toml$|fixtures/|infra/quickwit/|catalog/|web/src/examples\.json$|ops/index-history\.json$|ja-ocr/damage_v2\.py$|ja-ocr/wordlists/|scripts/(ci/|quickwit-fixtures\.sh))'
# The Quickwit job (parity against a real searcher, a release into a real
# writer): the code those tests run and what they load. Not catalog/,
# web/src/examples.json, ops/index-history.json or other CI scripts, which
# the rust job's own tests cover; editing them doesn't change what Quickwit
# is asked to do.
quickwit='^(crates/|Cargo\.(toml|lock)$|rust-toolchain\.toml$|fixtures/|infra/quickwit/|scripts/quickwit-fixtures\.sh$|scripts/ci/install-quickwit\.sh$)'
# The web job also checks the format, so Prettier's config counts.
web='^(web/|fixtures/|scripts/ci/|\.prettier(rc\.json|ignore)$)'
# The Quickwit pin is the ingest image's base, so a new one republishes it.
image="$rust|^(web/|ja-ocr/|Dockerfile|\.dockerignore$|infra/quickwit-image\.json$|scripts/build-page-language-words\.py$)"
# The synthetic corpus must match its generator.
fixtures='^fixtures/'
# The OCR worker's release: the worker and the LAN dashboard, the crates they use, the workspace's
# build files, its Tesseract image and the release script.
ocr_worker='^(crates/(usnm-ocr-worker|usnm-ocr-dash|usnm-ocr-contract|usnm-core|usnm-overlay|usnm-store|usnm-telemetry)/|Cargo\.(toml|lock)$|rust-toolchain\.toml$|ocr/engines/tesseract/|scripts/ci/ocr-worker-release\.sh$)'
# The page-language detector's test on text of known language (05 §5.5.8):
# the detector, its word lists and the language table it reads them by, its
# crate's manifest and the lock file, the samples and their test, and the
# scripts that write the lists and the samples.
page_language='^(crates/usnm-core/src/page_language/|crates/usnm-core/src/lang\.rs$|crates/usnm-core/data/page-language-words\.txt$|crates/usnm-core/Cargo\.toml$|Cargo\.lock$|crates/usnm-core/tests/page_language_samples\.rs$|fixtures/page-language/|scripts/(page-language-samples|build-page-language-words)\.py$)'
# The BOINC server hosting: its scripts, templates and Bicep (its Markdown counts as
# documentation, above), and the workflow, which matches earlier.
contrib='^(contrib-compute/|crates/usnm-boinc/)'
for part in rust quickwit web image fixtures ocr_worker page_language contrib; do
  if matches "${!part}" || [[ " $forced " == *" $part "* ]]; then echo "$part=true" >> "$out"; else echo "$part=false" >> "$out"; fi
done
echo "ops=$ops" >> "$out"
echo "markdown=$markdown" >> "$out"
if [ -n "$files" ]; then echo "code=true" >> "$out"; else echo "code=false" >> "$out"; fi
echo "provision=$provision" >> "$out"
