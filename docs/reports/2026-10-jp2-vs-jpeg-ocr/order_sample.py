"""Thin and order the sampled pages so any prefix of the run is representative (#296).

Reads --in (merge_sample.py's output), keeps 40 German pages at most (the 6 with
the highest damage, then 3 per decade), and interleaves
English, German and the other languages in proportion to their counts.
"""
import argparse, collections, json, random

ap = argparse.ArgumentParser()
ap.add_argument("--in", dest="inp", required=True)
ap.add_argument("--out", required=True)
ap.add_argument("--seed", type=int, default=296)
ap.add_argument("--german", type=int, default=40)
a = ap.parse_args()
rows = [json.loads(l) for l in open(a.inp)]
rng = random.Random(a.seed)
g = collections.defaultdict(list)
for r in rows:
    g[r["lang"]].append(r)
deu = sorted(g["deu"], key=lambda r: -r["damage"])
poor, rest = deu[:6], deu[6:]
by = collections.defaultdict(list)
for r in rest:
    by[r["decade"]].append(r)
pick = []
for k in sorted(by):
    rng.shuffle(by[k])
    pick += by[k][:3]
g["deu"] = (poor + pick)[: a.german]
oth = [r for k in ("pol", "ces", "srp", "rus") for r in g[k]]
groups = {"eng": g["eng"], "deu": g["deu"], "oth": oth}
for v in groups.values():
    rng.shuffle(v)
w = {k: len(v) for k, v in groups.items()}
tot = sum(w.values())
acc = {k: 0 for k in w}
order = []
while any(groups[k] for k in w):
    for k in w:
        acc[k] += w[k]
    k = max((k for k in w if groups[k]), key=lambda k: acc[k])
    acc[k] -= tot
    order.append(groups[k].pop(0))
open(a.out, "w").write("".join(json.dumps(r) + "\n" for r in order))
print(len(order), dict(collections.Counter(r["lang"] for r in order)))
