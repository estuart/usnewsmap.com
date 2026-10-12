#!/usr/bin/env python3
"""Draw the survey's sample pages from loc.gov and cache their scans and LoC's OCR (#296).

    sample.py --out /srv/ocr-survey/sample --seed 296

For each group's sources (a title by LCCN, a batch, or every title in a
language), the draw is seeded and goes like this:

1. One API call lists the source's issues per decade (the `dates` facet).
   Decades outside the group's window, or with no issues, are dropped.
2. Pick k of n for the source cycles through those decades in a seeded
   shuffled order, so a source's picks come from as many decades as it has.
3. In that decade, a seeded random position among the source's items
   (`c=1&sp=<k>`); a title record instead of an issue, or an issue already
   picked, is drawn again (up to 6 times), and then the next decade is tried.
4. One issue JSON call (`/item/{lccn}/{date}/ed-{n}/?fo=json`) gives the
   page images and LoC's ALTO; a seeded random page of the issue is taken.

Then the page's full-resolution image (IIIF full/full/0/default.jpg, read
from the issue JSON as ja-ocr/jaocr.py does) and LoC's ALTO are fetched, and
LoC's text is written from the ALTO (altotext.py). The control group draws
four candidate pages from each of five titles; score.py --select-control
keeps two per title, the first in draw order that read well (see there). Control pages are drawn by month
instead (draw_issue_month): a seeded random month in the window, then a
seeded random issue of the title that month.

Writes OUT/manifest.jsonl (one row per page, with the selection details),
OUT/images/, OUT/alto/ and OUT/loctext/. A page already in the manifest is
not fetched again, so it can be stopped and rerun. Each pick has its own
seeded PRNG, so a rerun draws what an uninterrupted run would.

Round 1's sample was drawn by an earlier version with one PRNG per source,
whose rerun after an error could draw differently from an uninterrupted run. So
seed 296 doesn't reproduce it: the sample is the archived manifest.jsonl.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import sys
import urllib.parse
from pathlib import Path

import altotext
import locfetch

COLLECTION = "https://www.loc.gov/collections/chronicling-america/"

# group, language (MARC, as quality.py uses), window (first and last year), sources: (facet, value, label, picks)
GROUPS = [
    ("german", "ger", (1850, 1919), [
        ("number_lccn", "sn87067031", "Nebraska Staats-Zeitung", 4),
        ("number_lccn", "sn86071815", "Richmonder Anzeiger", 4),
        ("number_lccn", "sn86054053", "Hermanner Volksblatt", 4),
        ("number_lccn", "sn86053575", "Tennessee Staatszeitung", 3),
    ]),
    ("worst-english", "eng", (1800, 1963), [
        ("batch", "nn_kant_ver01", "nn_kant_ver01", 3),
        ("batch", "txdn_japan_ver01", "txdn_japan_ver01", 3),
        ("batch", "nn_bentham_ver01", "nn_bentham_ver01", 3),
        ("batch", "nn_carson_ver02", "nn_carson_ver02", 3),
        ("batch", "kyu_frenchie_ver02", "kyu_frenchie_ver02", 3),
    ]),
    ("serbian", "srp", (1906, 1963), [
        ("number_lccn", "sn85055101", "Amerikanski Srbobran", 15),
    ]),
    ("czech", "cze", (1870, 1963), [
        ("number_lccn", "sn83045348", "Pokrok Západu", 15),
    ]),
    ("russian", "rus", (1900, 1963), [
        ("language", "russian", "Russian-language titles", 5),
    ]),
    ("polish", "pol", (1880, 1963), [
        ("language", "polish", "Polish-language titles", 5),
    ]),
    # Control: well-read English dailies of the 1920s and 1930s, drawn a month at a time ("title_month").
    # A first draft drew from every English title through the collection search, whose deep result pages
    # (sp=100546) loc.gov refuses with HTTP 400.
    ("control", "eng", (1920, 1939), [
        ("title_month", "sn83045462", "Evening Star (Washington, D.C.)", 4),
        ("title_month", "sn84026749", "The Washington Times (Washington, D.C.)", 4),
        ("title_month", "sn85042243", "The Bismarck Tribune (Bismarck, N.D.)", 4),
        ("title_month", "sn83030214", "New-York Tribune (New York, N.Y.)", 4),
        ("title_month", "sn92070146", "Imperial Valley Press (El Centro, Calif.)", 4),
    ]),
]


def search_url(facet: str, value: str, extra: dict) -> str:
    q = {"fa": f"{facet}:{value}", "fo": "json", **extra}
    return COLLECTION + "?" + urllib.parse.urlencode(q)


def get_json(url: str) -> dict:
    return json.loads(locfetch.fetch(url, locfetch.API))


def decades(facet: str, value: str, window: tuple[int, int]) -> list[tuple[int, int, int]]:
    """(first year, last year, items) per decade of the source inside the window."""
    d = get_json(search_url(facet, value, {"c": 1, "at": "facets"}))
    out = []
    for f in d.get("facets", []):
        if f.get("type") != "dates":
            continue
        for x in f.get("filters", []):
            m = re.match(r"(\d{4}) to (\d{4})", x.get("title", ""))
            if not m:
                continue
            a, b = max(int(m.group(1)), window[0]), min(int(m.group(2)), window[1])
            if a <= b and x.get("count", 0) > 0:
                out.append((a, b, x["count"]))
    return sorted(out)


ISSUE = re.compile(r"/item/([^/]+)/(\d{4}-\d{2}-\d{2})/ed-(\d+)/?$")


def draw_issue(facet: str, value: str, dec: tuple[int, int, int], rnd: random.Random, seen: set) -> dict | None:
    a, b, count = dec
    for _ in range(min(6, max(count, 1))):
        k = rnd.randint(1, max(count, 1))
        d = get_json(search_url(facet, value, {"c": 1, "sp": k, "dates": f"{a}/{b}", "at": "results,pagination"}))
        of = (d.get("pagination") or {}).get("of") or 0
        if of and k > of:
            k = rnd.randint(1, of)
            d = get_json(search_url(facet, value, {"c": 1, "sp": k, "dates": f"{a}/{b}",
                                                   "at": "results,pagination"}))
        for r in d.get("results", []):
            m = ISSUE.search(r.get("id", "").rstrip("/") + "/")
            if m and m.group(0) not in seen:
                seen.add(m.group(0))
                return {"lccn": m.group(1), "date": m.group(2), "edition": int(m.group(3)), "decade": f"{a}/{b}",
                        "sp": k, "of": of}
    return None


def draw_issue_month(lccn: str, window: tuple[int, int], rnd: random.Random, seen: set) -> dict | None:
    """A seeded random month in the window, then a seeded random issue of the title in that month: one
    shallow search call per try (c=100, first result page), no deep paging."""
    for _ in range(6):
        y, m = rnd.randint(window[0], window[1]), rnd.randint(1, 12)
        month = f"{y}-{m:02d}"
        d = get_json(search_url("number_lccn", lccn, {"c": 100, "dates": f"{month}/{month}",
                                                       "at": "results,pagination"}))
        issues = []
        for r in d.get("results", []):
            mm = ISSUE.search(r.get("id", "").rstrip("/") + "/")
            if mm and mm.group(0) not in seen:
                issues.append(mm)
        if issues:
            mm = rnd.choice(issues)
            seen.add(mm.group(0))
            return {"lccn": mm.group(1), "date": mm.group(2), "edition": int(mm.group(3)), "decade": month,
                    "sp": 1, "of": (d.get("pagination") or {}).get("of") or 0}
    return None


def page_id(lccn: str, date: str, ed: int, seq: int) -> str:
    return f"{lccn}_{date}_ed{ed}_seq{seq}"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--seed", type=int, default=296)
    ap.add_argument("--groups", default="", help="comma-separated group names (default: all)")
    a = ap.parse_args()
    for sub in ("images", "alto", "loctext"):
        (a.out / sub).mkdir(parents=True, exist_ok=True)
    manifest = a.out / "manifest.jsonl"
    done = {}
    if manifest.exists():
        for line in manifest.read_text().splitlines():
            r = json.loads(line)
            done[(r["group"], r["source"], r["pick"])] = r
    seen = {f"/item/{r['lccn']}/{r['date']}/ed-{r['edition']}/" for r in done.values()}
    only = set(filter(None, a.groups.split(",")))
    for group, lang, window, sources in GROUPS:
        if only and group not in only:
            continue
        for facet, value, label, picks in sources:
            rnd = random.Random(f"{a.seed}:{group}:{facet}:{value}")
            if facet == "title_month":
                decs = [(window[0], window[1], 0)]
            else:
                decs = decades(facet, value, window)
            if not decs:
                print(f"no issues: {group} {facet}:{value}", file=sys.stderr)
                continue
            order = list(decs)
            rnd.shuffle(order)
            for pick in range(picks):
                if (group, value, pick) in done:
                    continue
                # each pick draws from its own PRNG, so a resumed run draws what an uninterrupted one would
                rnd = random.Random(f"{a.seed}:{group}:{facet}:{value}:{pick}")
                issue = None
                for j in range(len(order)):  # a decade with only the title record (or used up): the next one
                    if facet == "title_month":
                        issue = draw_issue_month(value, window, rnd, seen)
                    else:
                        issue = draw_issue(facet, value, order[(pick + j) % len(order)], rnd, seen)
                    if issue is not None:
                        break
                if issue is None:
                    print(f"no issue drawn: {group} {value} pick {pick}", file=sys.stderr)
                    continue
                item_url = f"https://www.loc.gov/item/{issue['lccn']}/{issue['date']}/ed-{issue['edition']}/?fo=json"
                item = get_json(item_url)
                images, altos = locfetch.page_images(item), locfetch.page_altos(item)
                seqs = sorted(s for s in images if s in altos)
                if not seqs:
                    print(f"no pages with image and ALTO: {item_url}", file=sys.stderr)
                    continue
                seq = rnd.choice(seqs)
                pid = page_id(issue["lccn"], issue["date"], issue["edition"], seq)
                image_url = images[seq] + "/full/full/0/default.jpg"
                img = locfetch.fetch(image_url, locfetch.TILE)
                alto_xml = locfetch.fetch(altos[seq], locfetch.TILE)
                (a.out / "images" / f"{pid}.jpg").write_bytes(img)
                (a.out / "alto" / f"{pid}.xml").write_bytes(alto_xml)
                text, _ = altotext.parse(alto_xml)
                (a.out / "loctext" / f"{pid}.txt").write_text(text, encoding="utf-8")
                it = item.get("item") or {}
                row = {
                    "page_id": pid, "group": group, "lang": lang, "source": value, "source_facet": facet,
                    "source_label": label, "pick": pick, "lccn": issue["lccn"], "date": issue["date"],
                    "edition": issue["edition"], "seq": seq, "pages_in_issue": len(images),
                    "title": it.get("title"), "batch": (it.get("batch") or [None])[0],
                    "item_languages": it.get("language"), "decade_window": issue["decade"],
                    "draw_position": issue["sp"], "draw_of": issue["of"], "issue_url": item_url,
                    "image_url": image_url, "image_bytes": len(img), "image_sha256": hashlib.sha256(img).hexdigest(),
                    "alto_url": altos[seq], "loc_text_chars": len(text),
                }
                with manifest.open("a") as f:
                    f.write(json.dumps(row, ensure_ascii=False) + "\n")
                print(f"{group} {value} {pick}: {pid} ({len(img) // 1024} KB)", flush=True)


if __name__ == "__main__":
    main()
