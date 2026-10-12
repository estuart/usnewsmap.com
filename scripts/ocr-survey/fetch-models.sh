#!/usr/bin/env bash
# Download the survey's model files (models.lock) into a directory, one at a time and rate-limited,
# and check each sha256. A file already there with the right hash is kept.
#
#   fetch-models.sh --dest /srv/ocr-survey/models [--limit-rate 4M] [--only tessdata/]
set -euo pipefail
dest="" rate="4M" only=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dest) dest="$2"; shift 2 ;;
    --limit-rate) rate="$2"; shift 2 ;;
    --only) only="$2"; shift 2 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done
[ -n "$dest" ] || { echo "--dest is required" >&2; exit 2; }
here="$(cd "$(dirname "$0")" && pwd)"
grep -v '^#' "$here/models.lock" | while read -r name url sha; do
  [ -n "$name" ] || continue
  case "$name" in "$only"*) ;; *) continue ;; esac
  f="$dest/$name"
  mkdir -p "$(dirname "$f")"
  if [ -f "$f" ] && echo "$sha  $f" | sha256sum --check --status; then continue; fi
  echo "fetching $name"
  curl -fsSL --limit-rate "$rate" -A "usnewsmap-ocr-survey/1.0 (+https://usnewsmap.com)" -o "$f.part" "$url"
  echo "$sha  $f.part" | sha256sum --check --quiet
  mv "$f.part" "$f"
  sleep 2
done
