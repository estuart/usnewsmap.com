#!/usr/bin/env python3
"""Run the OCR engines on the sampled pages (#128).

    scripts/ja-ocr/run.py --sample-dir SAMPLE_DIR --engine ndlocr-lite --ndlocr ~/code/ndlocr-lite
    scripts/ja-ocr/run.py --sample-dir SAMPLE_DIR --engine tesseract --tessdata SAMPLE_DIR/tessdata
    scripts/ja-ocr/run.py --sample-dir SAMPLE_DIR --engine azure --endpoint "$OCR_ENDPOINT"

The azure engine calls Document Intelligence's prebuilt-read model with an
Entra token from az (the account has no keys; the caller needs Cognitive
Services User). F0 takes images up to 4 MB, so larger scans are re-encoded
as JPEG at full resolution (needs Pillow; the NDLOCR-Lite venv has it).

Reads SAMPLE_DIR/sample.csv (from sample.py) and writes each page's text to
SAMPLE_DIR/out/<engine>/<page_id>.txt, with seconds per page in
SAMPLE_DIR/out/<engine>/times.csv. Pages that already have output are
skipped, so it can be stopped and rerun.
"""

import argparse
import csv
import io
import json
import urllib.error
import urllib.request
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def ndlocr(root: Path, img: Path, out: Path) -> None:
    with tempfile.TemporaryDirectory() as tmp:
        subprocess.run(
            [str(root / ".venv/bin/python"), "ocr.py", "--sourceimg", str(img), "--output", tmp],
            cwd=root / "src", check=True, capture_output=True,
        )
        shutil.copy(Path(tmp) / f"{img.stem}.txt", out)


def tesseract(tessdata: Path, lang: str, img: Path, out: Path) -> None:
    subprocess.run(
        ["tesseract", str(img), str(out.with_suffix("")), "-l", lang, "--psm", "1"],
        env={"TESSDATA_PREFIX": str(tessdata), "PATH": "/opt/homebrew/bin:/usr/bin:/bin"},
        check=True, capture_output=True,
    )


AZURE_LIMIT = 4 * 1024 * 1024
_token = {"value": "", "expires": 0.0}


def azure_token() -> str:
    if time.time() > _token["expires"] - 300:
        out = subprocess.run(
            ["az", "account", "get-access-token", "--resource", "https://cognitiveservices.azure.com",
             "--query", "[accessToken,expires_on]", "-o", "tsv"],
            check=True, capture_output=True, text=True,
        ).stdout.split()
        _token.update(value=out[0], expires=float(out[1]))
    return _token["value"]


def fit(img: Path) -> bytes:
    data = img.read_bytes()
    if len(data) <= AZURE_LIMIT:
        return data
    from PIL import Image

    im = Image.open(img).convert("L")
    for scale in (1.0, 0.85, 0.7):
        sized = im if scale == 1.0 else im.resize((int(im.width * scale), int(im.height * scale)), Image.LANCZOS)
        for q in (85, 75, 65):
            buf = io.BytesIO()
            sized.save(buf, "JPEG", quality=q)
            if buf.tell() <= AZURE_LIMIT:
                if scale < 1.0:
                    print(f"{img.name}: scaled to {scale:.0%} to fit F0's 4 MB", file=sys.stderr)
                return buf.getvalue()
    raise ValueError(f"{img.name}: can't fit under 4 MB")


def open_retrying(req: urllib.request.Request, timeout: int):
    for attempt in range(8):
        try:
            return urllib.request.urlopen(req, timeout=timeout)
        except urllib.error.HTTPError as e:
            if e.code != 429 or attempt == 7:
                raise
            time.sleep(int(e.headers.get("Retry-After") or 10))


def azure(endpoint: str, img: Path, out: Path) -> None:
    url = endpoint.rstrip("/") + "/documentintelligence/documentModels/prebuilt-read:analyze?api-version=2024-11-30"
    req = urllib.request.Request(url, data=fit(img), method="POST", headers={
        "Authorization": f"Bearer {azure_token()}", "Content-Type": "application/octet-stream"})
    with open_retrying(req, 120) as r:
        op = r.headers["Operation-Location"]
    deadline = time.monotonic() + 300
    while True:
        if time.monotonic() > deadline:
            raise RuntimeError("still running after 5 minutes")
        time.sleep(2)
        poll = urllib.request.Request(op, headers={"Authorization": f"Bearer {azure_token()}"})
        with open_retrying(poll, 60) as r:
            body = json.load(r)
        if body["status"] in ("succeeded", "failed"):
            break
    if body["status"] == "failed":
        raise RuntimeError(json.dumps(body.get("error"))[:300])
    out.write_text(body["analyzeResult"]["content"])
    (out.with_suffix(".json")).write_text(json.dumps(body["analyzeResult"], ensure_ascii=False))


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--sample-dir", dest="sample", type=Path, help="the sample to OCR (required)")
    # The positional form (`run.py SAMPLE_DIR`) works for one release, with a
    # warning (#277).
    ap.add_argument("legacy_sample", nargs="?", type=Path, help=argparse.SUPPRESS)
    ap.add_argument("--engine", required=True, choices=["ndlocr-lite", "tesseract", "azure"])
    ap.add_argument("--endpoint", help="Document Intelligence endpoint (azure)")
    ap.add_argument("--ndlocr", type=Path, default=Path.home() / "code/ndlocr-lite")
    ap.add_argument("--tessdata", type=Path)
    ap.add_argument("--lang", default="jpn_vert")
    a = ap.parse_args()
    if a.legacy_sample is not None:
        if a.sample is not None:
            ap.error("give SAMPLE_DIR once, as --sample-dir")
        print("warning: a positional SAMPLE_DIR is deprecated; use --sample-dir (#277)", file=sys.stderr)
        a.sample = a.legacy_sample
    if a.sample is None:
        ap.error("--sample-dir is required")
    if a.engine == "azure" and not a.endpoint:
        ap.error("--engine azure needs --endpoint")
    if a.engine == "tesseract" and not a.tessdata:
        ap.error("--engine tesseract needs --tessdata")
    name = a.engine if a.engine != "tesseract" else f"tesseract-{a.lang}"
    out_dir = a.sample / "out" / name
    out_dir.mkdir(parents=True, exist_ok=True)
    times = out_dir / "times.csv"
    new = not times.exists()
    with times.open("a", newline="") as tf:
        tw = csv.writer(tf)
        if new:
            tw.writerow(["page_id", "seconds"])
        for row in csv.DictReader((a.sample / "sample.csv").open()):
            pid = row["page_id"]
            out = out_dir / f"{pid}.txt"
            if out.exists():
                continue
            img = a.sample / "images" / f"{pid}.jpg"
            t0 = time.time()
            try:
                if a.engine == "ndlocr-lite":
                    ndlocr(a.ndlocr, img, out)
                elif a.engine == "azure":
                    azure(a.endpoint, img, out)
                else:
                    tesseract(a.tessdata, a.lang, img, out)
            except subprocess.CalledProcessError as e:
                print(f"{pid}: failed: {e.stderr[-300:]!r}", file=sys.stderr)
                continue
            except (urllib.error.URLError, RuntimeError, ValueError) as e:
                print(f"{pid}: failed: {e}", file=sys.stderr)
                continue
            secs = round(time.time() - t0, 1)
            tw.writerow([pid, secs])
            tf.flush()
            print(f"{name} {pid} {secs}s", file=sys.stderr)


if __name__ == "__main__":
    main()
