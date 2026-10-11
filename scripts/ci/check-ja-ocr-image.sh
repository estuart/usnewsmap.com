#!/usr/bin/env bash
# The Japanese OCR image (--image) runs NDLOCR-Lite as its non-root user and reads
# a known headline (Rocky Shimpo, 1945-01-01, page 4) correctly, as text and as ALTO,
# and has the word lists the OCR quality audit (quality.py) reads.
set -euo pipefail
usage() { echo "usage: scripts/ci/check-ja-ocr-image.sh --image IMAGE" >&2; exit 2; }
# The positional form (`check-ja-ocr-image.sh IMAGE`) works for one
# release, with a warning (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/ci/check-ja-ocr-image.sh IMAGE\` is deprecated; use --image (#277)" >&2
  set -- --image "$1"
fi
image=""
while [ $# -gt 0 ]; do
  case $1 in
    --image) [ $# -ge 2 ] || usage; image=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$image" ] || usage
docker run --rm --entrypoint python3 "$image" -c \
  'import onnxruntime, cv2, pyarrow, azure.identity, azure.storage.blob, wordfreq; print("imports ok")'
# Every language the audit maps has its own wordfreq list in the image (no
# fallback to a "nearest" language, no download at run time).
docker run --rm -w /opt/jaocr --entrypoint python3 "$image" -c '
import quality
for lang, code in sorted(quality.WORDFREQ.items()):
    words = quality.wordfreq_words(code)
    assert words and len(words) > 20000, (lang, code, words and len(words))
s = quality.score("The council met on Tuesday and voted for the new school", quality.wordfreq_words("en"), "eng")
assert s.dict_share == 1.0, s
models = quality.Models()
for lang in sorted(quality.WORDFREQ):
    m = models.get(lang)
    assert m and len(m.top) >= 100 and len(m.head) >= 20 and m.near, lang
assert models.get("eng").near.get("tbe") == "the" and "she" not in models.get("eng").near
for lang in sorted(quality.LOCAL_WORDLISTS):  # our own lists, wordlists/<code>.txt
    m = models.get(lang)
    assert m and len(m.real) == 5000 and len(m.top) >= 100 and m.near, lang
print("word lists ok")'
work=$(mktemp -d)
mkdir -p "$work/img" "$work/out"
chmod 777 "$work/out"
cp ja-ocr/testdata/rocky-shimpo-1945-01-01-seq-4-headline.jpg "$work/img/"
docker run --rm -v "$work/img:/img:ro" -v "$work/out:/out" -w /opt/ndlocr-lite/src \
  --entrypoint python3 "$image" ocr.py --sourcedir /img --output /out > /dev/null
grep -q '去年の大記事' "$work/out/rocky-shimpo-1945-01-01-seq-4-headline.txt" ||
  { echo "NDLOCR-Lite didn't read the headline:" >&2; head -5 "$work"/out/*.txt >&2; exit 1; }
# The job turns NDLOCR-Lite's XML into ALTO with the module the image ships (#151).
docker run --rm -v "$work/out:/out:ro" -w /opt/jaocr --entrypoint python3 "$image" -c '
import alto, xml.etree.ElementTree as ET
from datetime import datetime, timezone
x = alto.ndlocr_to_alto(open("/out/rocky-shimpo-1945-01-01-seq-4-headline.xml", "rb").read(), alto.Geometry(),
                        image_url="u", engine="e", seq=4, ocred_at=datetime.now(timezone.utc))
s = [e.get("CONTENT") for e in ET.fromstring(x).iter("{%s}String" % alto.NS)]
assert any("去年の大記事" in c for c in s), s
print("alto ok")'
echo "headline read"
