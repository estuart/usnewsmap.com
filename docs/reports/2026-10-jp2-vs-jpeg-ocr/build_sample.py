"""Pick the pages for the JP2 vs JPEG run from what pve-big already holds (#296, #368).

English and the other languages come from the scan fetcher's earlier
stratified sample (scans/sample/, full-size IIIF JPEGs); German comes from the
keep set, whose scans are served by the scan server in either format. Years
1840-1929 only. Every German page of the JPEG pool with a decade and the JP2
pool are drawn per decade; the six JPEG-pool pages with the highest damage
rate are added as poor-condition pages.
"""
import argparse, collections, json, os, random
import pyarrow.parquet as pq

ap = argparse.ArgumentParser()
ap.add_argument("--manifest", required=True)
ap.add_argument("--scans", required=True)
ap.add_argument("--out", required=True)
ap.add_argument("--seed", type=int, default=296)
a = ap.parse_args()
rng = random.Random(a.seed)
t = pq.read_table(a.manifest)
d = t.to_pydict()
idx = {x: i for i, x in enumerate(d["doc_id"])}
L = {"eng": "eng", "ger": "deu", "cze": "ces", "pol": "pol", "rus": "rus", "srp": "srp"}


def rec(side, src):
    i = idx.get(side["doc_id"])
    if i is None or d["lang"][i] not in L:
        return None
    yr = d["date"][i].year
    if not (1840 <= yr <= 1929):
        return None
    base = side["iiif_base"]
    return dict(doc_id=side["doc_id"], batch=side["batch"], batch_version=side["batch_version"], lccn=d["lccn"][i],
                date=str(d["date"][i]), edition=d["edition"][i], seq=d["seq"][i], lang=L[d["lang"][i]],
                lang_manifest=d["lang"][i], year=yr, decade=yr // 10 * 10, damage=d["damage_rate"][i],
                decision=d["decision"][i], iiif_base=base,
                storage_base="https://tile.loc.gov/storage-services/" + base.split("/iiif/")[1].replace(":", "/"),
                have=src, have_format=side.get("format", "jpeg"))


sample_idx = [json.loads(l) for l in open(os.path.join(a.scans, "sample/index-full.jsonl"))]
main_idx = [json.loads(l) for l in open(os.path.join(a.scans, "index-full.jsonl"))]
eng, oth = [], []
for s in sample_idx:
    r = rec(s, "sample")
    if r:
        (eng if r["lang"] == "eng" else oth).append(r)
ger_jpeg = [r for s in main_idx if s.get("format", "jpeg") == "jpeg" for r in [rec(s, "keep")] if r and r["decade"] >= 1850]
ger_jp2 = [r for s in main_idx if s.get("format") == "jp2" for r in [rec(s, "keep")] if r]


def strat(pool, per):
    g = collections.defaultdict(list)
    for r in pool:
        g[r["decade"]].append(r)
    out = []
    for k in sorted(g):
        rng.shuffle(g[k])
        out += g[k][:per]
    return out


chosen = sorted(eng, key=lambda r: r["doc_id"]) + sorted(oth, key=lambda r: r["doc_id"])
chosen += strat(ger_jpeg, 5) + strat(ger_jp2, 5)
taken = {c["doc_id"] for c in chosen}
chosen += sorted([r for r in ger_jpeg if r["doc_id"] not in taken], key=lambda r: -r["damage"])[:6]
seen, out = set(), []
for r in chosen:
    if r["doc_id"] not in seen:
        seen.add(r["doc_id"])
        out.append(r)
open(a.out, "w").write("".join(json.dumps(r) + "\n" for r in out))
print(len(out), collections.Counter(r["lang"] for r in out))
