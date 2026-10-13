#!/usr/bin/env python3
"""Cut crops from the survey pages, with each engine's text for them, for hand transcription (#296).

    crops.py --sample DIR --work DIR --out docs/reports/2026-10-ocr-engine-survey/crops [--per-group 3] [--seed 296]

For each group, --per-group pages are picked (seeded), and on each a region
of text: a seeded random line of LoC's ALTO with at least 4 words, in a
block at least 600 pixels wide, and the region below it (up to 1,600 pixels
wide, 6% of the page high). The crop is written as OUT/<crop_id>.jpg.

OUT/crops.jsonl has a row per crop: the page, the box in image pixels, and
the text inside the box from every source with word boxes: LoC's ALTO,
each Tesseract and NDNP Open OCR ALTO at full scale, and docTR's word
boxes. Surya's and American Stories' output here has region boxes, not
word boxes, so they are not cut. A word is in the box when its centre is,
and the words are put in visual line order (lines_in_box). `transcription` is empty: a person fills it
in from the image, and only then can a character error rate be computed.
"""

from __future__ import annotations

import argparse
import json
import random
import re
from pathlib import Path

from PIL import Image

import altotext

Image.MAX_IMAGE_PIXELS = None


def page_width(xml: bytes) -> float | None:
    m = re.search(rb"<(?:\w+:)?Page\b[^>]*\bWIDTH=\"([\d.]+)\"", xml)
    return float(m.group(1)) if m else None


def alto_words(path: Path, image_width: int):
    """Word boxes in image pixels: the ALTO's units scaled by the image's width over the Page's WIDTH."""
    xml = path.read_bytes()
    w = page_width(xml)
    scale = image_width / w if w else altotext.pixel_scale(xml)
    return altotext.parse(xml, scale)[1]


def lines_in_box(words, box) -> str:
    """The words whose centre is in the box, as visual lines: sorted by their centre's height, a new line
    when a word's centre is more than half a median word height below the line's first word, then left to
    right. Engines order their own blocks differently, so this puts every source in the same reading order."""
    x0, y0, x1, y1 = box
    ws = [(x + w / 2, y + h / 2, h, c) for x, y, w, h, c, _ in words
          if x0 <= x + w / 2 < x1 and y0 <= y + h / 2 < y1 and c]
    if not ws:
        return ""
    tol = sorted(h for _, _, h, _ in ws)[len(ws) // 2] / 2
    lines: list[list] = []
    for w in sorted(ws, key=lambda w: w[1]):
        if lines and w[1] - lines[-1][0][1] <= tol:
            lines[-1].append(w)
        else:
            lines.append([w])
    return "\n".join(" ".join(c for _, _, _, c in sorted(line)) for line in lines)


def pick_box(words, size: tuple[int, int], rnd: random.Random):
    lines: dict[int, list] = {}
    for x, y, w, h, c, ln in words:
        lines.setdefault(ln, []).append((x, y, w, h))
    good = []
    for ln, ws in lines.items():
        if len(ws) < 4:
            continue
        x0 = min(x for x, _, _, _ in ws)
        x1 = max(x + w for x, _, w, _ in ws)
        if x1 - x0 >= 600:
            good.append((ln, x0, min(y for _, y, _, _ in ws), x1))
    if not good:
        return None
    _, x0, y0, x1 = rnd.choice(sorted(good))
    W, H = size
    return (max(0, int(x0) - 15), max(0, int(y0) - 15), min(W, int(min(x1, x0 + 1600)) + 15),
            min(H, int(y0 + 0.06 * H)))


def engine_texts(work: Path, results: list, pid: str, loc, box, image_width: int) -> dict:
    """The text inside the box from LoC's ALTO and from each engine run of the page that has word boxes."""
    texts = {"loc": lines_in_box(loc, box)}
    for j in results:
        if j["page_id"] != pid or j["status"] != "ok" or j.get("scale", 1.0) != 1.0:
            continue
        d = work / "out" / j["engine"]
        key = f"{j['engine']} {j['variant']}"
        if (d / f"{j['job_id']}.xml").exists():
            texts[key] = lines_in_box(alto_words(d / f"{j['job_id']}.xml", image_width), box)
        elif (d / f"{j['job_id']}.words.json").exists():
            ws = json.loads((d / f"{j['job_id']}.words.json").read_text())
            # docTR: [x0, y0, x1, y1, text, confidence]
            texts[key] = lines_in_box([(a, b, c - a, d - b, t, 0) for a, b, c, d, t, _ in ws], box)
    return texts


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--sample", type=Path, required=True)
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--per-group", type=int, default=3)
    ap.add_argument("--seed", type=int, default=296)
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    manifest = [json.loads(x) for x in (a.sample / "manifest-final.jsonl").read_text().splitlines() if x.strip()]
    manifest = [r for r in manifest if r.get("in_sample")]
    results = []
    for res in sorted((a.work / "results").glob("*.jsonl")):
        results += [json.loads(x) for x in res.read_text().splitlines() if x.strip()]
    rows = []
    for group in sorted({r["group"] for r in manifest}):
        rnd = random.Random(f"{a.seed}:{group}")
        pages = sorted((r for r in manifest if r["group"] == group), key=lambda r: r["page_id"])
        rnd.shuffle(pages)
        n = 0
        for r in pages:
            if n >= a.per_group:
                break
            pid = r["page_id"]
            img = Image.open(a.sample / "images" / f"{pid}.jpg")
            loc = alto_words(a.sample / "alto" / f"{pid}.xml", img.width)
            box = pick_box(loc, img.size, rnd)
            if box is None:
                continue
            n += 1
            crop_id = f"{group}-{n}"
            img.crop(box).convert("L").save(a.out / f"{crop_id}.jpg", quality=85)
            texts = engine_texts(a.work, results, pid, loc, box, img.width)
            rows.append({"crop_id": crop_id, "group": group, "page_id": pid, "lang": r["lang"],
                         "title": r.get("title"), "date": r["date"], "box": list(box), "image": f"{crop_id}.jpg",
                         "texts": dict(sorted(texts.items())), "transcription": None})
    (a.out / "crops.jsonl").write_text("".join(json.dumps(x, ensure_ascii=False) + "\n" for x in rows))
    print(f"{len(rows)} crops")


if __name__ == "__main__":
    main()
