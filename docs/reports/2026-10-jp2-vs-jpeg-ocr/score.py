"""Score the JP2 and JPEG OCR texts against LoC's OCR of the same pages (#296).

Reads the files fetch.py and run_ocr.py left in --dir and writes one JSON line
per page to --out.

LoC's OCR is machine output too, so every number here is agreement with it,
not accuracy against the printed page.

Normalisation, the same for all three texts: Unicode NFKC, lower case, long s
to s, a word broken with a hyphen at a line end joined, every character that
isn't a letter or a digit read as a space. A "word" is what is left between
spaces.

Order-free metrics. The engines read a page's columns in different orders, so
a character-by-character comparison would measure the layout, not the
reading. Instead the words of each text are counted as a multiset:

  word_recall      matched reference words / reference words
  word_precision   matched words / words in the OCR text
  wer_bag          (max(reference words, OCR words) - matched) / reference
                   words. This is a floor on the true WER: it counts a
                   substitution as one error but cannot see a word in the
                   wrong place.
  cer_bag          the unmatched reference words are paired with unmatched
                   OCR words of the same first letter and near length, best
                   first; each pair costs its edit distance, each word left
                   over costs its length; divided by the reference's
                   characters. A heuristic, not a bound on the true CER:
                   first-letter pairing and greedy matching can cost more
                   than the true edit distance.
  seq_agree        the two OCR texts aligned as word sequences
                   (difflib, no junk heuristic): 2 * matched / (words of both)
  line_agree       the same on the texts' non-empty lines, compared whole
  dice_jp2_jpeg    2 * shared words / (words of the JP2 text + words of the
                   JPEG text), the two OCR texts against each other.

plain_share is the share of the whitespace-separated tokens of the OCR text
(as the worker stores it) that, with leading and trailing punctuation
removed, are two or more letters long and hold only letters, with an
apostrophe or a hyphen between letters at most.
"""

import argparse
import collections
import difflib
import json
import os
import re
import unicodedata
import xml.etree.ElementTree as ET


def alto_text(path):
    root = ET.parse(path).getroot()
    lines = []
    for el in root.iter():
        if el.tag.rsplit("}", 1)[-1] != "TextLine":
            continue
        parts = []
        for s in el:
            tag = s.tag.rsplit("}", 1)[-1]
            if tag == "String":
                parts.append(s.get("CONTENT", ""))
            elif tag == "HYP":
                parts.append(s.get("CONTENT", "-"))
        # a HYP is attached to the word before it
        line = ""
        for p in parts:
            line += p if p == "-" and line else (" " + p if line else p)
        lines.append(line)
    return "\n".join(lines)


def words(text):
    t = unicodedata.normalize("NFKC", text).casefold().replace("ſ", "s")
    t = re.sub(r"[-­‐‑]\s*\n\s*", "", t)
    t = "".join(c if c.isalnum() else " " for c in t)
    return t.split()


def edit(a, b, cap):
    if abs(len(a) - len(b)) > cap:
        return cap + 1
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def cer_bag(ref, hyp):
    rc, hc = collections.Counter(ref), collections.Counter(hyp)
    ur = list((rc - hc).elements())
    uh = list((hc - rc).elements())
    ref_chars = sum(len(w) for w in ref)
    if not ref_chars:
        return None
    buckets = collections.defaultdict(list)
    for w in uh:
        buckets[w[0]].append(w)
    cost = 0
    for w in sorted(ur, key=len, reverse=True):
        best, bi = len(w), None
        cands = buckets.get(w[0], [])
        for i, h in enumerate(cands):
            if abs(len(h) - len(w)) > max(2, len(w) // 2):
                continue
            d = edit(w, h, best)
            if d < best:
                best, bi = d, i
                if d <= 1:
                    break
        if bi is not None:
            cands.pop(bi)
        cost += best
    # OCR words nobody was paired with are insertions
    cost += sum(len(h) for c in buckets.values() for h in c)
    return cost / ref_chars


def plain_share(text):
    toks = text.split()
    if not toks:
        return None
    ok = 0
    for t in toks:
        t = re.sub(r"^[^\w]+|[^\w]+$", "", t)
        if len(t) >= 2 and re.fullmatch(r"[^\W\d_]+(?:['’-][^\W\d_]+)*", t):
            ok += 1
    return ok / len(toks)


def metrics(ref, hyp, raw):
    rc, hc = collections.Counter(ref), collections.Counter(hyp)
    m = sum((rc & hc).values())
    return {
        "words": len(hyp),
        "chars": sum(len(w) for w in hyp),
        "word_recall": m / len(ref) if ref else None,
        "word_precision": m / len(hyp) if hyp else None,
        "wer_bag": (max(len(ref), len(hyp)) - m) / len(ref) if ref else None,
        "cer_bag": cer_bag(ref, hyp),
        "plain_share": plain_share(raw),
    }


def seq_ratio(a, b):
    if not a and not b:
        return None
    return difflib.SequenceMatcher(None, a, b, autojunk=False).ratio()


def lines_of(text):
    out = []
    for l in text.splitlines():
        w = " ".join(words(l))
        if w:
            out.append(w)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sample", required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--results", nargs="+", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.sample)]
    run = {}
    for f in a.results:
        for l in open(f):
            r = json.loads(l)
            run[(r["n"], r["fmt"])] = r
    out = open(a.out, "w")
    for n, s in enumerate(rows, 1):
        d = os.path.join(a.dir, str(n))
        have = {f: run.get((n, f)) for f in ("jp2", "jpg")}
        if not all(have.values()) or not os.path.exists(os.path.join(d, "loc.xml")):
            continue
        bad = {f: have[f]["rc"] != 0 or not have[f].get("chars") for f in have}
        if any(bad.values()):
            out.write(json.dumps({"n": n, "doc_id": s["doc_id"], "lang": s["lang"], "decade": s["decade"],
                                  "failed": {f: {"rc": have[f]["rc"], "chars": have[f].get("chars"),
                                                 "status": have[f].get("status"), "wall": have[f]["wall"],
                                                 "stderr": (have[f].get("stderr") or "")[-120:]}
                                             for f in have if bad[f]}}) + "\n")
            continue
        ref = words(alto_text(os.path.join(d, "loc.xml")))
        raw, wd = {}, {}
        for f in ("jp2", "jpg"):
            raw[f] = open(os.path.join(d, f"out-{f}", "page.txt"), encoding="utf-8").read()
            wd[f] = words(raw[f])
        rec = {k: s[k] for k in ("doc_id", "lang", "year", "decade", "damage", "batch")}
        rec.update(n=n, ref_words=len(ref), ref_chars=sum(len(w) for w in ref))
        for f in ("jp2", "jpg"):
            rec[f] = metrics(ref, wd[f], raw[f])
            rec[f].update(secs=run[(n, f)].get("secs"), wall=run[(n, f)]["wall"], node=run[(n, f)]["node"],
                          load_before=run[(n, f)]["load_before"], bytes=run[(n, f)]["image_bytes"],
                          peak_rss_kib=run[(n, f)].get("peak_rss_kib"))
        c1, c2 = collections.Counter(wd["jp2"]), collections.Counter(wd["jpg"])
        shared = sum((c1 & c2).values())
        rec["dice_jp2_jpeg"] = 2 * shared / (len(wd["jp2"]) + len(wd["jpg"])) if wd["jp2"] or wd["jpg"] else None
        rec["seq_agree"] = seq_ratio(wd["jp2"], wd["jpg"])
        rec["line_agree"] = seq_ratio(lines_of(raw["jp2"]), lines_of(raw["jpg"]))
        rec["identical_text"] = raw["jp2"] == raw["jpg"]
        out.write(json.dumps(rec) + "\n")


if __name__ == "__main__":
    main()
