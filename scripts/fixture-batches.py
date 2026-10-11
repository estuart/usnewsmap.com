#!/usr/bin/env python3
"""Turn the SYNTHETIC fixture corpus back into LoC-style batch archives, for
trying the ingest pipeline locally (see README "Ingest pipeline").

Writes to OUT_DIR:
  batch_fx_early_ver01.tar.gz   pages before 1897-07-01 ({lccn}/{yyyy}/{mm}/{dd}/ed-1/seq-N/ocr.txt)
  batch_fx_late_ver01.tar.gz    the rest ({lccn}/{yyyy-mm-dd}/ed-1/seq-N/ocr.txt)
  batches.json                  the batch list for `usnm-ingest enqueue`
  reference/catalog/            titles.json and places.json

Usage: python3 scripts/fixture-batches.py --out-dir OUT_DIR
"""
import argparse
import hashlib
import io
import json
import sys
import tarfile
from datetime import date, timedelta
from pathlib import Path

DATA = Path(__file__).resolve().parent.parent / "fixtures" / "data"
EPOCH = date(1700, 1, 1)
SPLIT = date(1897, 7, 1)


def main(out):
    out.mkdir(parents=True, exist_ok=True)
    titles = json.loads((DATA / "fixture-v1/titles.json").read_text())
    lccn_of = {t["place_id"]: t["lccn"] for t in titles}
    text = {}
    for f in ("pages-base-fixture", "pages-delta-fixture-1"):
        for line in (DATA / f"indexes/{f}.jsonl").read_text().splitlines():
            d = json.loads(line)
            text[d["doc_id"]] = d["text"]
    pages = {"early": [], "late": []}
    for place, series in json.loads((DATA / "fixture-v1/baselines.json").read_text()).items():
        for day, n in series:
            d = EPOCH + timedelta(days=day)
            for seq in range(1, n + 1):
                doc_id = f"{lccn_of[place]}_{d.isoformat()}_ed-1_seq-{seq}"
                pages["early" if d < SPLIT else "late"].append((lccn_of[place], d, seq, text.get(doc_id, "")))
    listing = []
    for part, historical in (("early", True), ("late", False)):
        name = f"batch_fx_{part}_ver01"
        path = out / f"{name}.tar.gz"
        with tarfile.open(path, "w:gz") as tar:
            for lccn, d, seq, body in pages[part]:
                folder = d.strftime("%Y/%m/%d") if historical else d.isoformat()
                data = body.encode()
                info = tarfile.TarInfo(f"{lccn}/{folder}/ed-1/seq-{seq}/ocr.txt")
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
        sha = hashlib.sha256(path.read_bytes()).hexdigest()
        listing.append({"name": name, "url": str(path.resolve()), "sha256": sha})
    (out / "batches.json").write_text(json.dumps(listing, indent=1) + "\n")
    catalog = out / "reference" / "catalog"
    catalog.mkdir(parents=True, exist_ok=True)
    for f in ("titles.json", "places.json"):
        (catalog / f).write_bytes((DATA / "fixture-v1" / f).read_bytes())
    print(f"{len(pages['early'])} + {len(pages['late'])} pages in 2 batches under {out}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0].replace("\n", " "))
    ap.add_argument("--out-dir", type=Path, help="where to write the archives (required)")
    # The positional form (`fixture-batches.py OUT_DIR`) works for one
    # release, with a warning (#277).
    ap.add_argument("legacy_out_dir", nargs="?", type=Path, help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.legacy_out_dir is not None:
        if a.out_dir is not None:
            ap.error("give the directory once, as --out-dir")
        print("warning: a positional OUT_DIR is deprecated; use --out-dir (#277)", file=sys.stderr)
        a.out_dir = a.legacy_out_dir
    if a.out_dir is None:
        ap.error("--out-dir is required")
    main(a.out_dir)
