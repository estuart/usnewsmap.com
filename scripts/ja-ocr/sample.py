#!/usr/bin/env python3
"""Pick a sample of Japanese pages from loc.gov and download their scans (#128).

    scripts/ja-ocr/sample.py --out-dir OUT_DIR [--pages 100] [--seed 128]

Walks the titles loc.gov lists as Japanese, spreads issues across each
title's years, and reads LoC's own OCR (ALTO) for every page of a chosen
issue. A page whose OCR is empty, or mostly not words, is a Japanese page:
LoC's OCR captured no Japanese script. Mostly-English pages are skipped.
For each sampled page it writes the full-resolution JPEG to OUT_DIR/images
and a row to OUT_DIR/sample.csv; LoC's ALTO goes to OUT_DIR/alto for later
comparison.

loc.gov's JSON API allows about 20 requests a minute, so API calls are
paced 3.5 s apart; tile.loc.gov (images, ALTO) 1 s apart. Rerunning skips
pages already in sample.csv.
"""

import argparse
import csv
import json
import random
import re
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

UA = "usnewsmap-ja-ocr/1.0 (+https://usnewsmap.com)"
API = "https://www.loc.gov"
API_GAP, TILE_GAP = 3.5, 1.0
_last = {"api": 0.0, "tile": 0.0}


def get(url: str, kind: str, attempts: int = 4) -> bytes:
    for i in range(attempts):
        wait = (API_GAP if kind == "api" else TILE_GAP) - (time.time() - _last[kind])
        if wait > 0:
            time.sleep(wait)
        _last[kind] = time.time()
        try:
            req = urllib.request.Request(url, headers={"User-Agent": UA})
            with urllib.request.urlopen(req, timeout=120) as r:
                return r.read()
        except Exception as e:  # 429/5xx: back off and retry
            print(f"  retry {i + 1} {url[:100]}: {e}", file=sys.stderr)
            time.sleep(30 * (i + 1))
    raise RuntimeError(f"giving up on {url}")


def api(path: str, **params) -> dict:
    q = urllib.parse.urlencode({"fo": "json", **params})
    return json.loads(get(f"{API}{path}?{q}", "api"))


def japanese_titles() -> list[str]:
    d = api("/collections/chronicling-america/", fa="language:japanese", c=100, at="results")
    out = []
    for r in d["results"]:
        parts = r["id"].rstrip("/").split("/")
        if parts[-2] == "item" and parts[-1].startswith("sn"):
            out.append(parts[-1])
    return out


def issues(lccn: str) -> list[str]:
    """Issue item URLs for a title (up to 1,000, enough to spread a sample)."""
    urls, page = [], 1
    while page <= 10:
        d = api("/collections/chronicling-america/", fa=f"number_lccn:{lccn}", c=100, sp=page,
                at="results,pagination")
        for r in d["results"]:
            if re.search(r"/\d{4}-\d\d-\d\d/ed-\d+/$", r["id"]):
                urls.append(r["id"].replace("http://", "https://"))
        if not d["pagination"].get("next"):
            break
        page += 1
    return sorted(set(urls))


# A word: Capitalized or lower case, or ALL CAPS (headlines); mixed case is OCR noise.
WORD = re.compile(r"^(?:[A-Z]?[a-z]{2,}|[A-Z]{3,15})$")


def classify(alto: str) -> tuple[str, int, float]:
    """('empty'|'garbled'|'english', words, share of word-like tokens)."""
    words = re.findall(r'CONTENT="([^"]*)"', alto)
    if len("".join(words)) < 20:
        return "empty", len(words), 0.0
    wordlike = sum(1 for w in words if WORD.match(w.strip(".,;:!?'\"()")))
    share = wordlike / len(words)
    return ("garbled" if share < 0.35 else "english"), len(words), round(share, 3)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", dest="out", type=Path, help="where to write the sample (required)")
    # The positional form (`sample.py OUT_DIR`) works for one release, with a
    # warning (#277).
    ap.add_argument("legacy_out", nargs="?", type=Path, help=argparse.SUPPRESS)
    ap.add_argument("--pages", type=int, default=100)
    ap.add_argument("--seed", type=int, default=128)
    ap.add_argument("--issues-per-title", type=int, default=4)
    ap.add_argument("--pages-per-issue", type=int, default=2)
    ap.add_argument("--titles", nargs="+", help="only these LCCNs (default: every Japanese title)")
    a = ap.parse_args()
    if a.legacy_out is not None:
        if a.out is not None:
            ap.error("give OUT_DIR once, as --out-dir")
        print("warning: a positional OUT_DIR is deprecated; use --out-dir (#277)", file=sys.stderr)
        a.out = a.legacy_out
    if a.out is None:
        ap.error("--out-dir is required")
    rnd = random.Random(a.seed)
    for d in ("images", "alto"):
        (a.out / d).mkdir(parents=True, exist_ok=True)
    sheet = a.out / "sample.csv"
    fields = ["page_id", "lccn", "title", "date", "seq", "loc_ocr", "loc_words", "loc_wordlike",
              "width", "height", "image", "jp2", "alto"]
    done = {r["page_id"] for r in csv.DictReader(sheet.open())} if sheet.exists() else set()
    new = not sheet.exists()
    out = sheet.open("a", newline="")
    w = csv.DictWriter(out, fieldnames=fields)
    if new:
        w.writeheader()
    count = len(done)

    titles = a.titles or japanese_titles()
    rnd.shuffle(titles)
    print(f"{len(titles)} Japanese titles; {count} pages already sampled", file=sys.stderr)
    for lccn in titles:
        if count >= a.pages:
            break
        iss = issues(lccn)
        if not iss:
            continue
        # Spread across the run: one issue from each slice of the sorted list.
        k = min(a.issues_per_title, len(iss))
        picks = [iss[rnd.randrange(i * len(iss) // k, (i + 1) * len(iss) // k)] for i in range(k)]
        for url in picks:
            if count >= a.pages:
                break
            item = json.loads(get(url + "?fo=json", "api"))
            title = item.get("item", {}).get("title", "")
            files = (item.get("resources") or [{}])[0].get("files") or []
            m = re.search(r"/(\d{4}-\d\d-\d\d)/", url)
            date = m.group(1) if m else ""
            issue_prefix = f"{lccn}_{date}_{url.rstrip('/').split('/')[-1]}_seq-"
            found = sum(1 for d in done if d.startswith(issue_prefix))
            for seq, page in enumerate(files, 1):
                if found >= a.pages_per_issue or count >= a.pages:
                    break
                by = {f.get("mimetype"): f.get("url", "") for f in page}
                alto_url, jp2 = by.get("text/xml", ""), by.get("image/jp2", "")
                if not alto_url or not jp2:
                    continue
                page_id = f"{lccn}_{date}_{url.rstrip('/').split('/')[-1]}_seq-{seq}"
                if page_id in done:
                    continue
                alto = get(alto_url, "tile").decode("utf-8", "replace")
                kind, nwords, share = classify(alto)
                if kind == "english":
                    continue
                path = jp2.split("/storage-services/service/", 1)[1].removesuffix(".jp2")
                iiif = "https://tile.loc.gov/image-services/iiif/service:" + path.replace("/", ":")
                info = json.loads(get(iiif + "/info.json", "tile"))
                img = a.out / "images" / f"{page_id}.jpg"
                img.write_bytes(get(iiif + "/full/full/0/default.jpg", "tile"))
                (a.out / "alto" / f"{page_id}.xml").write_text(alto)
                w.writerow({"page_id": page_id, "lccn": lccn, "title": title, "date": date, "seq": seq,
                            "loc_ocr": kind, "loc_words": nwords, "loc_wordlike": share,
                            "width": info.get("width"), "height": info.get("height"),
                            "image": iiif, "jp2": jp2, "alto": alto_url})
                out.flush()
                found += 1
                count += 1
                print(f"{count:3} {page_id} {kind} {title[:40]}", file=sys.stderr)
    print(f"done: {count} pages in {sheet}", file=sys.stderr)


if __name__ == "__main__":
    main()
