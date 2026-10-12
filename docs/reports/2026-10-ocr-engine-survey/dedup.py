#!/usr/bin/env python3
"""docTR's round-1 scores with the words that the tiling kept twice removed (#296, #370).

    dedup.py --sample SAMPLE --work WORK --data docs/reports/2026-10-ocr-engine-survey/data

Round 1 cut each page into 1,280 px tiles overlapping by 256 px, and kept a
word from the tile whose core held its centre. The cores took 128 px from
each inner side, so where a page's last tile started less than a full step
after the one before it, two cores overlapped and a word there could be kept
from both tiles. engine_runner.py now splits each overlap at its midpoint.

docTR's words.json gives each kept word's box on the page, but not its tile.
So this keeps a word whose centre is in one of those double strips unless a
word kept before it in the same strip has a box that overlaps it by at least
half (intersection over the smaller box), whatever either word reads. That
is the copy from the earlier tile. The text is the engine's own .txt with
those words taken out of their lines (words.json lists the words of the
.txt's lines in order, which is checked), scored as score.py does against
LoC's text. It prints the per-group medians and writes
DATA/doctr-dedup.jsonl.

Run it where score.py runs (it imports score.py, so ja-ocr/quality.py and
wordfreq), on the round-1 work directory.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "scripts" / "ocr-survey"))

from PIL import Image  # noqa: E402

import score  # noqa: E402

TILE, OVERLAP = 1280, 256


def double_strips(n: int) -> list[tuple[int, int]]:
    """Where round 1's cores overlapped along one axis."""
    step = TILE - OVERLAP
    if n <= TILE:
        return []
    ss = sorted(set(list(range(0, n - TILE, step)) + [n - TILE]))
    return [(b + OVERLAP // 2, a + TILE - OVERLAP // 2) for a, b in zip(ss, ss[1:])
            if a + TILE - OVERLAP // 2 > b + OVERLAP // 2]


def overlap_share(p, q) -> float:
    iw = min(p[2], q[2]) - max(p[0], q[0])
    ih = min(p[3], q[3]) - max(p[1], q[1])
    if iw <= 0 or ih <= 0:
        return 0.0
    smaller = min((p[2] - p[0]) * (p[3] - p[1]), (q[2] - q[0]) * (q[3] - q[1])) or 1
    return iw * ih / smaller


def dedup(words: list, w: int, h: int) -> list[bool]:
    """For each word, whether to keep it."""
    xs, ys = double_strips(w), double_strips(h)
    keep, strip_kept = [], []
    for wd in words:
        cx, cy = (wd[0] + wd[2]) / 2, (wd[1] + wd[3]) / 2
        if any(a <= cx < b for a, b in xs) or any(a <= cy < b for a, b in ys):
            if any(overlap_share(wd, k) >= 0.5 for k in strip_kept):
                keep.append(False)
                continue
            strip_kept.append(wd)
        keep.append(True)
    return keep


def rebuild(text: str, words: list, keep: list[bool]) -> str:
    """The .txt with the dropped words taken out of their lines."""
    out, i = [], 0
    for line in text.split("\n"):
        n = len(line.split(" "))
        assert [x[4] for x in words[i:i + n]] == line.split(" "), "words.json doesn't match the .txt"
        kept = [x[4] for x, k in zip(words[i:i + n], keep[i:i + n]) if k]
        if kept:
            out.append(" ".join(kept))
        i += n
    assert i == len(words), "words.json has words the .txt hasn't"
    return "\n".join(out)


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--sample", type=Path, required=True)
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--data", type=Path, required=True)
    a = ap.parse_args()
    rows = [json.loads(x) for x in (a.data / "scores.jsonl").read_text().splitlines() if x.strip()]
    loc = {r["page_id"]: r for r in rows if r["engine"] == "loc"}
    out = []
    for r in rows:
        if r["engine"] != "doctr" or r["status"] != "ok":
            continue
        jid = f"{r['page_id']}__{r['variant']}@1"
        words = json.loads((a.work / "out" / "doctr" / f"{jid}.words.json").read_text())
        with Image.open(a.sample / "images" / f"{r['page_id']}.jpg") as im:
            w, h = im.size
        text = (a.work / "out" / "doctr" / f"{jid}.txt").read_text(encoding="utf-8")
        keep = dedup(words, w, h)
        before = score.score_text(rebuild(text, words, [True] * len(words)), r["lang"])
        s = score.score_text(rebuild(text, words, keep), r["lang"])
        lp = loc[r["page_id"]]
        out.append({
            "page_id": r["page_id"], "group": r["group"], "boxes": len(words), "boxes_removed": keep.count(False),
            "words_before": before["words"], "damage_before": before["damage_rate"],
            "words": s["words"], "known_words": s["known_words"], "damage_rate": s["damage_rate"],
            "known_share": s["known_share"], "loc_damage": lp["damage_rate"],
            "damage_change": None if s["damage_rate"] is None or lp["damage_rate"] is None
            else round(s["damage_rate"] - lp["damage_rate"], 4),
            "words_ratio": round(s["words"] / lp["words"], 3) if lp["words"] else None,
            "known_words_ratio": round(s["known_words"] / lp["known_words"], 3) if lp["known_words"] else None,
        })
    (a.data / "doctr-dedup.jsonl").write_text("".join(json.dumps(x) + "\n" for x in out))
    print("| Group | Pages | Boxes removed (median share) | Damage (median) | LoC, same pages | Change (median) "
          "| Pages better | Words vs LoC | Known words vs LoC; pages above 1 |")
    print("|---|---|---|---|---|---|---|---|---|")
    for g in sorted({x["group"] for x in out}):
        xs = [x for x in out if x["group"] == g]
        ch = [x["damage_change"] for x in xs if x["damage_change"] is not None]
        print(f"| {g} | {len(xs)} | {med(x['boxes_removed'] / x['boxes'] for x in xs if x['boxes']):.3f} | "
              f"{med(x['damage_rate'] for x in xs):.3f} | "
              f"{med(x['loc_damage'] for x in xs if x['damage_rate'] is not None):.3f} | {med(ch):.3f} | "
              f"{sum(c < 0 for c in ch)} of {len(ch)} | {med(x['words_ratio'] for x in xs):.2f} | "
              f"{med(x['known_words_ratio'] for x in xs):.2f}; "
              f"{sum(1 for x in xs if (x['known_words_ratio'] or 0) > 1)} |")


if __name__ == "__main__":
    main()
