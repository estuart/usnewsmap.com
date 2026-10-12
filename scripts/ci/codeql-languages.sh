#!/usr/bin/env bash
# Which CodeQL languages a change touches, as the step output
# languages=<JSON array> for codeql.yml's analysis matrix. A change to HTML,
# CSS, Markdown, images, Bicep or JSON data alone gives [], and no analysis
# runs (package.json, the lock files and tsconfig*.json are JavaScript's).
# The changed files are worked out as scripts/ci/changes.sh does.
# Every language runs for a schedule or workflow_dispatch, for a change to
# codeql.yml or this script, and whenever the changed files can't be worked
# out.
set -euo pipefail
out=${GITHUB_OUTPUT:-/dev/stdout}
everything='["actions","javascript-typescript","python","rust"]'
all() {
  echo "languages=$everything" >> "$out"
  echo "analysing every language: $1"
  exit 0
}
case "${GITHUB_EVENT_NAME:-}" in
  pull_request) range="origin/$GITHUB_BASE_REF...HEAD" ;;
  push)
    before=${BEFORE_SHA:-}
    [ -n "$before" ] && ! [[ $before =~ ^0+$ ]] || all "no previous commit"
    range="$before..$GITHUB_SHA"
    ;;
  *) all "event ${GITHUB_EVENT_NAME:-unknown}" ;;
esac
files=$(git diff --name-only --no-renames "$range") || all "git diff failed for $range"
[ -n "$files" ] || all "no changed files found"
echo "$files" | sed 's/^/changed: /'
grep -Eq '^(\.github/workflows/codeql\.yml|scripts/ci/codeql-languages\.sh)$' <<< "$files" &&
  all "the CodeQL setup changed"
# What each language's analysis reads: its sources and the manifests and
# lock files that pin its dependencies.
pattern() {
  case $1 in
    actions) echo '^\.github/(workflows|actions)/' ;;
    javascript-typescript) echo '\.(js|jsx|ts|tsx|mjs|cjs|mts|cts)$|(^|/)(package\.json|tsconfig[^/]*\.json|jsconfig\.json|package-lock\.json|npm-shrinkwrap\.json|yarn\.lock|pnpm-lock\.yaml)$' ;;
    python) echo '\.py$|(^|/)requirements[^/]*\.txt$|(^|/)pyproject\.toml$' ;;
    rust) echo '\.rs$|(^|/)Cargo\.(toml|lock)$|(^|/)rust-toolchain(\.toml)?$' ;;
  esac
}
langs=()
for lang in actions javascript-typescript python rust; do
  if grep -Eq "$(pattern "$lang")" <<< "$files"; then langs+=("$lang"); fi
done
json=$(printf '%s\n' "${langs[@]+"${langs[@]}"}" | jq -Rnc '[inputs | select(. != "")]')
echo "languages=$json" >> "$out"
echo "analysing: $json"
