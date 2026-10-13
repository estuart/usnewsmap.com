"""Side-by-side reading of the two texts on chosen pages (#296).

For each --pages entry prints the word-level differences between the JP2 and
JPEG texts (difflib on the normalised word sequences): how many differing
words each side has that LoC's OCR also has, and a few differing stretches.
"""
import argparse, difflib, json, os
from score import alto_text, words

ap = argparse.ArgumentParser()
ap.add_argument("--dir", required=True)
ap.add_argument("--pages", nargs="+", type=int, required=True)
ap.add_argument("--stretches", type=int, default=6)
a = ap.parse_args()
for n in a.pages:
    d = os.path.join(a.dir, str(n))
    ref = set(words(alto_text(os.path.join(d, "loc.xml"))))
    t = {f: open(os.path.join(d, f"out-{f}", "page.txt"), encoding="utf-8").read() for f in ("jp2", "jpg")}
    w = {f: words(t[f]) for f in t}
    sm = difflib.SequenceMatcher(None, w["jp2"], w["jpg"], autojunk=False)
    only = {"jp2": [], "jpg": []}
    stretches = []
    for op, i1, i2, j1, j2 in sm.get_opcodes():
        if op == "equal":
            continue
        only["jp2"] += w["jp2"][i1:i2]
        only["jpg"] += w["jpg"][j1:j2]
        if op == "replace" and len(stretches) < 400:
            stretches.append((" ".join(w["jp2"][i1:i2]), " ".join(w["jpg"][j1:j2])))
    inref = {f: sum(1 for x in only[f] if x in ref) for f in only}
    print(f"== page {n}: JP2 {len(w['jp2'])} words, JPEG {len(w['jpg'])} words; differing words JP2 {len(only['jp2'])} "
          f"(in LoC {inref['jp2']}), JPEG {len(only['jpg'])} (in LoC {inref['jpg']})")
    step = max(1, len(stretches) // a.stretches)
    for x, y in stretches[::step][: a.stretches]:
        print(f"   JP2 : {x[:90]}\n   JPEG: {y[:90]}")
