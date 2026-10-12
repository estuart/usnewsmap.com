#!/usr/bin/env bash
# Publishes the OCR worker's GitHub release from CI (12 Step 2): the EL9
# binary, the LAN dashboard's binary (12 §12.2.5), the Tesseract image the
# worker runs (an OCI archive and its image id) and SHA256SUMS, as
# `ocr-worker-<yyyymmdd>-<sha7>`. The ocr-worker-release job attests every
# file first; scripts/ocr-node/install-worker.sh verifies them. A release
# that exists already (a re-run) is left as it is.
#   scripts/ci/ocr-worker-release.sh --dist DIR
#
# Needs GH_TOKEN with contents: write, GITHUB_REPOSITORY and GITHUB_SHA.
set -euo pipefail
usage() { sed -n '8s/^# \{0,1\}//p' "$0" >&2; exit 2; }
DIST=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dist) DIST=${2:-}; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[ -n "$DIST" ] || { echo "error: missing --dist" >&2; usage; }
files="usnm-ocr-worker usnm-ocrdash usnm-ocr-tesseract.oci.tar.gz usnm-ocr-tesseract.image-id SHA256SUMS"
for f in $files; do
  [ -s "$DIST/$f" ] || { echo "error: $DIST/$f is missing" >&2; exit 1; }
done
(cd "$DIST" && sha256sum --check --quiet SHA256SUMS)

tag="ocr-worker-$(date -u +%Y%m%d)-${GITHUB_SHA:0:7}"
# A release is created as a draft, gets its files, and is published only
# then, so an interrupted run never leaves a published release without all
# of them. A rerun finishes a draft it finds; a published one must be
# complete.
if state=$(gh release view "$tag" --repo "$GITHUB_REPOSITORY" --json isDraft,assets \
  --jq '"\(.isDraft) \([.assets[].name] | sort | join(","))"' 2> /dev/null); then
  want=$(echo "$files" | tr ' ' '\n' | LC_ALL=C sort | paste -sd, -)
  if [ "$state" = "false $want" ]; then
    echo "::notice::release $tag exists with every file; leaving it"
    exit 0
  fi
  [ "${state%% *}" = true ] || { echo "error: release $tag is published without every file ($state)" >&2; exit 1; }
  for f in $files; do
    gh release upload "$tag" "$DIST/$f" --repo "$GITHUB_REPOSITORY" --clobber
  done
  gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false --latest=false
  echo "finished and published $tag"
  exit 0
fi
image=$(cat "$DIST/usnm-ocr-tesseract.image-id")
notes=$(mktemp)
cat > "$notes" <<NOTES
The on-prem OCR worker (#296) for AlmaLinux 9 and RHEL 10 (x86_64, glibc 2.34 or later), built from $GITHUB_SHA.

- \`usnm-ocr-worker\`: the worker binary.
- \`usnm-ocrdash\`: the LAN dashboard (\`scripts/ocr-node/ocrdash-ct.sh --install\`).
- \`usnm-ocr-tesseract.oci.tar.gz\`: the Tesseract image it runs (\`ocr/engines/tesseract/\`), image id \`$image\`.
- \`usnm-ocr-tesseract.image-id\`: that id.
- \`SHA256SUMS\`: the sha256 of each file.

Each file has a build provenance attestation. Install with \`scripts/ocr-node/install-worker.sh --release $tag\`, which verifies them.
NOTES
args=()
for f in $files; do args+=("$DIST/$f"); done
gh release create "$tag" "${args[@]}" --repo "$GITHUB_REPOSITORY" --target "$GITHUB_SHA" \
  --title "$tag" --notes-file "$notes" --draft
gh release edit "$tag" --repo "$GITHUB_REPOSITORY" --draft=false --latest=false
echo "published $tag"
