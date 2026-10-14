#!/usr/bin/env bash
# Which parts of CI a change needs, as step outputs rust/web/image/e2e/fixtures/
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
# ocr_gold: the OCR gold set (scripts/ocr-survey/, fixtures/ocr-gold/, and the
# Rust scorer's golden files that gold.py writes), whose unit tests and golden
# check run in the infra job. fixtures/ocr-gold/ is read by no build, test
# or generator, so it counts for none of rust/quickwit/web/image/fixtures.
# contrib: the hosting for the stock BOINC server (contrib-compute/: shell,
# XML and Bicep; ADR-0019), which no other job builds.
# Everything runs for a workflow_dispatch (scripts/bootstrap.sh dispatches ci
# to publish images), for a change to the workflows themselves, and whenever
# the changed files can't be worked out; everything but ops, which runs only
# when its own files change (the snapshots, the index history, their schemas,
# checkers and writers, its requirements, or ci.yml, which defines the job).
# quickwit: what the Quickwit job's two tests build or load: the workspace
# crates usnm-search and usnm-ingest depend on (from cargo metadata), the
# workspace manifest and lock file only when the packages Cargo resolves for
# those crates change, and the fixtures, index configs and Quickwit scripts.
# image and e2e are narrowed the same way: image for the workspace crates
# usnm-api (Dockerfile) and usnm-ingest (Dockerfile.ingest) build, e2e (the web
# end-to-end job) for those usnm-api builds, the API that
# scripts/ci/start-api-fixtures.sh starts. A crate outside a closure (the BOINC
# or OCR survey crates, say) runs neither.
# The infra job (code) checks the workbook definitions, so any change to
# infra/workbooks/ runs it.
set -euo pipefail
out=${GITHUB_OUTPUT:-/dev/stdout}
# Known only once the changed files are; "run everything" keeps them.
provision=false
ops=false
forced=
all() {
  printf 'rust=true\nquickwit=true\nweb=true\nimage=true\ne2e=true\nfixtures=true\nops=%s\ncode=true\nmarkdown=true\nprovision=%s\nocr_worker=true\npage_language=true\ncontrib=true\nocr_gold=true\n' "$ops" "$provision" >> "$out"
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
# The gold set's crops and selection: nothing builds, tests or generates them.
built=$(grep -Ev '^fixtures/ocr-gold/' <<< "$files" || true)
matches_built() { [ -n "$built" ] && grep -Eq "$1" <<< "$built"; }
# infra/quickwit/ holds the index configs (read by the ingest image) and the
# searcher config (applied on deploy). Everything else under infra/ needs a
# provision, an environment's committed settings (infra/env/) included.
if [ -n "$files" ] && grep -E '^infra/' <<< "$files" | grep -Evq '^infra/quickwit/'; then
  provision=true
fi
# The index version snapshots and the index history must match their schemas
# (and what writes them); searches.sh's date-span summary has its tests there too.
matches '^(ops/version-snapshots/|ops/index-history(\.schema)?\.json$|scripts/(check-version-snapshots|test_check_version_snapshots|compare-versions|check-index-history|test_check_index_history|reconstruct-index-history|test_date_spans)\.py$|scripts/lib/date-spans\.jq$|scripts/ci/requirements-ops\.txt$)' && ops=true
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
  case " $forced " in *" ALL "*) ops=true; all "workflow changed" ;; esac
  for flag in markdown ops provision; do
    if [[ " $forced " == *" $flag "* ]]; then printf -v "$flag" true; fi
  done
  echo "workflow jobs changed, forcing:$forced"
}
# Workflow files are settled above; they are not code for the file rules below.
files=$(grep -Ev '^\.github/' <<< "$files" || true)
# The Rust build and tests: the crates, the files compiled into binaries
# (index config, place overrides, the example searches the API warms, the
# reconstructed index history), the fixtures the tests read, and the Python
# metric the damage port is checked against (its tables, its word lists and
# their sha256).
rust_files='^(rust-toolchain\.toml$|fixtures/|ocr/contract/|infra/quickwit/|catalog/|web/src/examples\.json$|ops/index-history\.json$|ja-ocr/damage_v2\.py$|ja-ocr/wordlists/|scripts/quickwit-fixtures\.sh)'
rust="^(crates/|Cargo\.(toml|lock)$)|$rust_files"
# The Quickwit job (parity against a real searcher, a release into a real
# writer) runs `cargo test -p usnm-search --test quickwit_parity` and
# `cargo test -p usnm-ingest --test pipeline`: the files those tests load
# here, plus (below) the workspace crates they build and the dependencies
# Cargo resolves for them. Not catalog/, web/src/examples.json,
# ops/index-history.json or other CI scripts, which the rust job's own tests
# cover; editing them doesn't change what Quickwit is asked to do.
quickwit='^(rust-toolchain\.toml$|fixtures/|infra/quickwit/|scripts/quickwit-fixtures\.sh$|scripts/ci/install-quickwit\.sh$)'
# The web job also checks the format, so Prettier's config counts.
web='^(web/|fixtures/|scripts/ci/(web-e2e|start-api-fixtures)\.sh$|\.prettier(rc\.json|ignore)$)'
# The Quickwit pin is the ingest image's base, so a new one republishes it.
# Crates count for these two below, by closure.
e2e="$rust_files|$web"
image="$rust_files|^(web/|ja-ocr/|Dockerfile|\.dockerignore$|infra/quickwit-image\.json$|scripts/build-page-language-words\.py$|scripts/ci/(check-(api|ingest|ja-ocr)-image|quickwit-image|roll-api|head-of-main|indexnow)\.sh$)"
# The BOINC integration tests execute this wrapper at run time. Its changes
# need the Rust tests, but it is not an input to the serving/ingest images above.
rust="$rust|^contrib-compute/images/server/bridge-job\.sh$"
# The synthetic corpus must match its generator.
fixtures='^fixtures/'
# The OCR worker's release: the worker and the LAN dashboard, the crates they use, the workspace's
# build files, its Tesseract image and the release script.
ocr_worker='^(crates/(usnm-ocr-worker|usnm-ocr-dash|usnm-ocr-contract|usnm-alto|usnm-core|usnm-overlay|usnm-store|usnm-telemetry)/|Cargo\.(toml|lock)$|rust-toolchain\.toml$|ocr/engines/tesseract/|scripts/ci/ocr-worker-release\.sh$)'
# The page-language detector's test on text of known language (05 §5.5.8):
# the detector, its word lists and the language table it reads them by, its
# crate's manifest and the lock file, the samples and their test, and the
# scripts that write the lists and the samples.
page_language='^(crates/usnm-core/src/page_language/|crates/usnm-core/src/lang\.rs$|crates/usnm-core/data/page-language-words\.txt$|crates/usnm-core/Cargo\.toml$|Cargo\.lock$|crates/usnm-core/tests/page_language_samples\.rs$|fixtures/page-language/|scripts/(page-language-samples|build-page-language-words)\.py$)'
# The BOINC server hosting: its scripts, templates and Bicep (its Markdown counts as
# documentation, above), and the workflow, which matches earlier.
contrib='^(contrib-compute/|crates/usnm-boinc/|scripts/ci/check-boinc-server-image\.sh$)'
ocr_gold='^(scripts/ocr-survey/|fixtures/ocr-gold/|crates/usnm-ocr-survey/tests/golden/)'
# closure_crates ROOT...: the directories of the workspace crates the roots
# depend on, directly or not (normal, build and dev dependencies: path
# dependencies in their manifests), from cargo metadata, so a new dependency
# can't leave the list stale.
closure_crates() {
  cargo metadata --format-version 1 --no-deps --offline | jq -r --arg root "$PWD/" --args '
    (.packages | map({key: .name, value: .}) | from_entries) as $p
    | def close: . as $s | ([$s[], ($s[] | $p[.].dependencies[] | select(.path) | .name)] | unique) as $n
        | if $n == $s then $s else $n | close end;
    $ARGS.positional | close | .[] | $p[.].manifest_path | ltrimstr($root) | rtrimstr("Cargo.toml")' "$@"
}
# closure_tree DIR ROOT...: every package (with its features) Cargo builds for
# the roots' tests, in the checkout at DIR, without the workspace crates' local
# paths (a git source keeps its revision).
closure_tree() {
  local dir=$1 p args=(); shift
  for p in "$@"; do args+=(-p "$p"); done
  (cd "$dir" && cargo tree --locked "${args[@]}" -e normal,build,dev --prefix none -f '{p} {f}' | sed 's| (/[^)]*)||' | sort -u)
}
# root_settings: the workspace manifest at revision $1 without its member list
# and [workspace.dependencies], whose effect on these crates the package tree
# shows. Anything else there (profiles, workspace.package, lints, patches)
# can change how the tests build without changing the tree.
root_settings() {
  git show "$1:Cargo.toml" | awk '
    /^members = \[/ { skip = 1 } skip { if (/^\]/) skip = 0; next }
    /^\[/ { deps = ($0 == "[workspace.dependencies]") } !deps'
}
# lock_changed ROOT...: whether the packages resolved for the roots differ
# between the base and this revision (a crate added outside them changes
# neither). True if that can't be worked out.
lock_changed() {
  local base dir before after
  case "$range" in *...*) base=$(git merge-base "${range%%...*}" "${range##*...}") ;; *) base=${range%%..*} ;; esac || return 0
  [ "$(root_settings "$base")" = "$(root_settings HEAD)" ] || return 0
  dir=$(mktemp -d)
  git worktree add -q --detach "$dir" "$base" || { rm -rf "$dir"; return 0; }
  before=$(closure_tree "$dir" "$@") || before=
  git worktree remove --force "$dir"
  after=$(closure_tree . "$@") || after=
  [ -z "$before" ] || [ -z "$after" ] || [ "$before" != "$after" ]
}
# narrow PART ROOT...: run PART when a crate in the roots' closure changed, or
# the workspace manifest or lock file changed what Cargo resolves for them.
# Fails open: when cargo can't say, PART runs.
narrow() {
  local part=$1 dirs; shift
  matches_built "${!part}" && return 0
  if ! dirs=$(closure_crates "$@") || [ -z "$dirs" ]; then
    echo "cargo metadata failed: running $part"; printf -v "$part" '.'
  elif matches_built "^($(paste -sd'|' - <<< "$dirs"))"; then
    printf -v "$part" '.'
  elif matches_built '^Cargo\.(toml|lock)$' && lock_changed "$@"; then
    echo "the packages resolved for $* changed: running $part"; printf -v "$part" '.'
  fi
}
narrow quickwit usnm-search usnm-ingest
narrow image usnm-api usnm-ingest
narrow e2e usnm-api
for part in rust quickwit web image e2e fixtures; do
  if matches_built "${!part}" || [[ " $forced " == *" $part "* ]]; then echo "$part=true" >> "$out"; else echo "$part=false" >> "$out"; fi
done
for part in ocr_worker page_language contrib ocr_gold; do
  if matches "${!part}" || [[ " $forced " == *" $part "* ]]; then echo "$part=true" >> "$out"; else echo "$part=false" >> "$out"; fi
done
echo "ops=$ops" >> "$out"
echo "markdown=$markdown" >> "$out"
if [ -n "$files" ] || [[ " $forced " == *" code "* ]]; then echo "code=true" >> "$out"; else echo "code=false" >> "$out"; fi
echo "provision=$provision" >> "$out"
