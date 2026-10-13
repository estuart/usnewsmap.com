"""Join the base sample and the extra pages the worker's `prefetch` placed (#296).

The extra pages' JP2s were fetched by `usnm-ocr-worker prefetch` (md5-checked
against the bag), which also wrote index-full.jsonl with each page's IIIF base.
This adds the IIIF base, the storage base and the JP2's path to each extra row
so fetch.py needs only the JPEG and the ALTO from LoC. Pages the prefetch did
not place are left out.
"""
import argparse
import json
import os

ap = argparse.ArgumentParser()
ap.add_argument("--base", required=True)
ap.add_argument("--extra", required=True)
ap.add_argument("--prefetch-dir", required=True)
ap.add_argument("--out", required=True)
a = ap.parse_args()
idx = {}
for l in open(os.path.join(a.prefetch_dir, "index-full.jsonl")):
    r = json.loads(l)
    idx[r["doc_id"]] = r
rows = [json.loads(l) for l in open(a.base)]
missing = 0
for l in open(a.extra):
    r = json.loads(l)
    i = idx.get(r["doc_id"])
    p = None
    if i:
        for kind in ("keep", "buffer"):
            c = os.path.join(a.prefetch_dir, kind, i["batch"], r["lccn"], r["date"], str(r["edition"]), f"{r['seq']}.jp2")
            if os.path.exists(c):
                p = c
    if not p:
        missing += 1
        continue
    r.update(iiif_base=i["iiif_base"], storage_base=i["loc_url"], local_jp2=os.path.abspath(p),
             have="prefetch", have_format="jp2", year=int(r["date"][:4]), decade=int(r["date"][:4]) // 10 * 10)
    r.setdefault("damage", r.get("damage"))
    rows.append(r)
open(a.out, "w").write("".join(json.dumps(r) + "\n" for r in rows))
print(len(rows), "pages;", missing, "extra pages not placed")
