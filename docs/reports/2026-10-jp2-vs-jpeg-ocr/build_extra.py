"""Pick extra pages for the JP2 vs JPEG run from the sample's own batches (#296)."""
import argparse, json, random, collections
import pyarrow as pa, pyarrow.parquet as pq
ap = argparse.ArgumentParser()
ap.add_argument("--manifest", required=True)
ap.add_argument("--sample", required=True)
ap.add_argument("--out-prefix", required=True)
ap.add_argument("--seed", type=int, default=297)
a = ap.parse_args()
rng = random.Random(a.seed)
t = pq.read_table(a.manifest)
d = t.to_pydict()
have = {json.loads(l)["doc_id"] for l in open(a.sample)}
rows = [json.loads(l) for l in open(a.sample)]
L = {"eng": "eng", "ger": "deu", "cze": "ces", "pol": "pol", "rus": "rus", "srp": "srp"}
batches = {r["batch"] for r in rows if r["lang"] in ("eng", "pol", "ces", "srp", "rus")}
by = collections.defaultdict(list)
for i in range(t.num_rows):
    if d["batch"][i] in batches and d["lang"][i] in L and d["doc_id"][i] not in have and 1840 <= d["date"][i].year <= 1929 and d["seq"][i] <= 16:
        by[(d["batch"][i], d["lang"][i])].append(i)
pick = []
for k in sorted(by):
    pick.append(rng.choice(by[k]))   # one extra per batch and language
chosen = set(pick)
# poor condition: highest damage among English pages of those batches
poor = [i for k, v in by.items() if k[1] == "eng" for i in v if d["damage_rate"][i] >= 0.5 and i not in chosen]
rng.shuffle(poor)
pick += poor[:14]
meta = []
for i in pick:
    yr = d["date"][i].year
    meta.append(dict(doc_id=d["doc_id"][i], batch=d["batch"][i], batch_version=d["batch_version"][i], lccn=d["lccn"][i], date=str(d["date"][i]),
                     edition=d["edition"][i], seq=d["seq"][i], lang=L[d["lang"][i]], lang_manifest=d["lang"][i], year=yr, decade=yr // 10 * 10,
                     damage=d["damage_rate"][i], decision=d["decision"][i], have="prefetch"))
pq.write_table(t.take(pick), a.out_prefix + ".parquet")
open(a.out_prefix + ".jsonl", "w").write("".join(json.dumps(m) + "\n" for m in meta))
print(len(meta), collections.Counter(m["lang"] for m in meta), sorted(collections.Counter(m["decade"] for m in meta).items()))
