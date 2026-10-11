#!/usr/bin/env python3
"""Cut reference crops from the sampled pages for accuracy scoring (#128).

    scripts/ja-ocr/crops.py --sample-dir SAMPLE_DIR [--count 24] [--seed 128]

Picks pages evenly from the typeset papers (Rocky Shimpo, Colorado Times)
and the camp papers (mostly hand-lettered mimeograph), cuts one region of
each (a quarter of the width, a fifth of the height, at a seeded random
spot), and writes them as their own sample in SAMPLE_DIR/crops (sample.csv
with a `group` column, images/). Run the engines on that directory with
run.py, transcribe each crop into SAMPLE_DIR/crops/refs/<page_id>.txt, then
score.py --sample-dir SAMPLE_DIR/crops. Needs Pillow.
"""

import argparse
import csv
import random
import sys
from pathlib import Path

from PIL import Image

TYPESET = {"sn83025517", "sn83025518"}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--sample-dir", dest="sample", type=Path, help="the sample to cut crops from (required)")
    # The positional form (`crops.py SAMPLE_DIR`) works for one release, with a
    # warning (#277).
    ap.add_argument("legacy_sample", nargs="?", type=Path, help=argparse.SUPPRESS)
    ap.add_argument("--count", type=int, default=24)
    ap.add_argument("--seed", type=int, default=128)
    a = ap.parse_args()
    if a.legacy_sample is not None:
        if a.sample is not None:
            ap.error("give SAMPLE_DIR once, as --sample-dir")
        print("warning: a positional SAMPLE_DIR is deprecated; use --sample-dir (#277)", file=sys.stderr)
        a.sample = a.legacy_sample
    if a.sample is None:
        ap.error("--sample-dir is required")
    rnd = random.Random(a.seed)
    rows = list(csv.DictReader((a.sample / "sample.csv").open()))
    groups = {"typeset": [r for r in rows if r["lccn"] in TYPESET],
              "camp": [r for r in rows if r["lccn"] not in TYPESET]}
    out = a.sample / "crops"
    (out / "images").mkdir(parents=True, exist_ok=True)
    (out / "refs").mkdir(exist_ok=True)
    picked = []
    for g, rs in groups.items():
        rnd.shuffle(rs)
        picked += [(g, r) for r in rs[: a.count // 2]]
    with (out / "sample.csv").open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["page_id", "lccn", "title", "date", "group", "box"])
        w.writeheader()
        for g, r in picked:
            im = Image.open(a.sample / "images" / f"{r['page_id']}.jpg")
            W, H = im.size
            cw, ch = W // 4, H // 5
            x, y = rnd.randrange(W // 20, W - cw - W // 20), rnd.randrange(H // 20, H - ch - H // 20)
            pid = f"{r['page_id']}__crop"
            im.crop((x, y, x + cw, y + ch)).save(out / "images" / f"{pid}.jpg", quality=92)
            w.writerow({"page_id": pid, "lccn": r["lccn"], "title": r["title"], "date": r["date"],
                        "group": g, "box": f"{x},{y},{cw},{ch}"})
    print(f"{len(picked)} crops in {out}")


if __name__ == "__main__":
    main()
