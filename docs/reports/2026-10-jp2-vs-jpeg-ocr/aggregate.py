"""Turn score.py's per-page lines into the report's tables (#296).

Reads --scores (score.py's output) and --results (run_ocr.py's lines) and
prints Markdown tables and a Mermaid chart to stdout. Nothing here reads the
scans or the text.
"""
import argparse
import json
import statistics as st


def mean(xs):
    xs = [x for x in xs if x is not None]
    return sum(xs) / len(xs) if xs else None


def med(xs):
    xs = [x for x in xs if x is not None]
    return st.median(xs) if xs else None


def f(x, p=3):
    return "-" if x is None else f"{x:.{p}f}"


def group(r):
    return "English" if r["lang"] == "eng" else "German" if r["lang"] == "deu" else "Other"


def era(r):
    y = r["year"]
    return "1840-1879" if y < 1880 else "1880-1909" if y < 1910 else "1910-1929"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--scores", required=True)
    ap.add_argument("--results", nargs="+", required=True)
    ap.add_argument("--crash", help="crash_scan.py's output")
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.scores)]
    ok = [r for r in rows if "failed" not in r]
    bad = [r for r in rows if "failed" in r]
    res = [json.loads(l) for fn in a.results for l in open(fn)]

    if a.crash:
        cs = [json.loads(l) for l in open(a.crash)]
        print("### JP2s the engine cannot read (crash scan, every fetched page)\n")
        print("| Group | JP2s | Exit 136 within seconds | Read (ran to the 20 s limit or finished) | Crash share |")
        print("|---|---|---|---|---|")
        def crow(name, g):
            c = sum(1 for x in g if x["verdict"] == "crash_136")
            print(f"| {name} | {len(g)} | {c} | {len(g) - c} | {c / len(g):.0%} |" if g else "")
        for k in ("English", "German", "Other"):
            crow(k, [x for x in cs if group(x) == k])
        for lo, hi in ((1840, 1879), (1880, 1909), (1910, 1929)):
            crow(f"{lo}-{hi}", [x for x in cs if lo <= x["decade"] <= hi])
        crow("Batches starting mdu_", [x for x in cs if x["batch"].startswith("mdu_")])
        crow("All other batches", [x for x in cs if not x["batch"].startswith("mdu_")])
        crow("All", cs)
        print()

    print("### Failures and empty outputs\n")
    print("| Format | Runs | Failed (non-zero exit) | Empty text | Timed out |")
    print("|---|---|---|---|---|")
    for fmt, name in (("jp2", "JP2"), ("jpg", "IIIF JPEG")):
        rr = [r for r in res if r["fmt"] == fmt]
        fail = [r for r in rr if r["rc"]]
        empty = [r for r in rr if not r["rc"] and not r.get("chars")]
        to = [r for r in rr if r["rc"] and r["wall"] >= 1150]  # the page timeout is 1200 s
        print(f"| {name} | {len(rr)} | {len(fail)} | {len(empty)} | {len(to)} |")
    print()
    for r in bad:
        print(f"- page {r['n']} ({r['lang']}, {r['decade']}s): {json.dumps(r['failed'])}")
    print()

    def table(title, keyf, keys):
        print(f"### {title}\n")
        print("| Group | Pages | JP2 WER (bag) | JPEG WER (bag) | JP2 CER (bag) | JPEG CER (bag) | JP2 recall | JPEG recall | JP2 better / equal / worse (WER) |")
        print("|---|---|---|---|---|---|---|---|---|")
        for k in keys + ["All"]:
            g = [r for r in ok if k == "All" or keyf(r) == k]
            if not g:
                continue
            d = [r["jpg"]["wer_bag"] - r["jp2"]["wer_bag"] for r in g if r["jp2"]["wer_bag"] is not None]
            eq = sum(1 for x in d if abs(x) < 0.0005)
            better = sum(1 for x in d if x >= 0.0005)
            print(f"| {k} | {len(g)} | {f(mean([r['jp2']['wer_bag'] for r in g]))} | {f(mean([r['jpg']['wer_bag'] for r in g]))} | "
                  f"{f(mean([r['jp2']['cer_bag'] for r in g]))} | {f(mean([r['jpg']['cer_bag'] for r in g]))} | "
                  f"{f(mean([r['jp2']['word_recall'] for r in g]))} | {f(mean([r['jpg']['word_recall'] for r in g]))} | "
                  f"{better} / {eq} / {len(d) - better - eq} |")
        print()

    table("Agreement with LoC's OCR, by language profile", group, ["English", "German", "Other"])
    table("By era", era, ["1840-1879", "1880-1909", "1910-1929"])
    ok_poor = [dict(r, cond="poor (damage >= 0.5)" if (r.get("damage") or 0) >= 0.5 else "ordinary") for r in ok]
    ok[:] = ok_poor
    table("By condition", lambda r: r["cond"], ["ordinary", "poor (damage >= 0.5)"])

    print("### The two Tesseract outputs against each other\n")
    print("| Group | Pages | Identical text | Word-sequence agreement (mean) | Line agreement (mean) | Bag-of-words Dice (mean) |")
    print("|---|---|---|---|---|---|")
    for k in ["English", "German", "Other", "All"]:
        g = [r for r in ok if k == "All" or group(r) == k]
        if g:
            print(f"| {k} | {len(g)} | {sum(r['identical_text'] for r in g)} | {f(mean([r['seq_agree'] for r in g]))} | "
                  f"{f(mean([r['line_agree'] for r in g]))} | {f(mean([r['dice_jp2_jpeg'] for r in g]))} |")
    print()

    print("### Time and memory\n")
    print("| Group | Pages | JP2 engine secs (mean) | JPEG engine secs (mean) | JP2 / JPEG | JP2 file MB | JPEG file MB | JP2 peak RSS MiB (median / max) | JPEG peak RSS MiB (median / max) |")
    print("|---|---|---|---|---|---|---|---|---|")
    for k in ["English", "German", "Other", "All"]:
        g = [r for r in ok if k == "All" or group(r) == k]
        if not g:
            continue
        a2, b2 = mean([r["jp2"]["secs"] for r in g]), mean([r["jpg"]["secs"] for r in g])
        rss = lambda fm: [r[fm]["peak_rss_kib"] / 1024 for r in g if r[fm].get("peak_rss_kib")]
        print(f"| {k} | {len(g)} | {f(a2, 0)} | {f(b2, 0)} | {f(a2 / b2 if a2 and b2 else None, 2)} | "
              f"{f(mean([r['jp2']['bytes'] / 1e6 for r in g]), 1)} | {f(mean([r['jpg']['bytes'] / 1e6 for r in g]), 1)} | "
              f"{f(med(rss('jp2')), 0)} / {f(max(rss('jp2'), default=None), 0)} | {f(med(rss('jpg')), 0)} / {f(max(rss('jpg'), default=None), 0)} |")
    print()

    names = ["English", "German", "Other"]
    def m(fmt, k):
        return round(mean([r[fmt]["wer_bag"] for r in ok if group(r) == k]) or 0, 3)
    print("```mermaid")
    print("xychart-beta")
    print('    title "Mean bag-of-words WER against LoC OCR (lower is closer)"')
    print("    x-axis [" + ", ".join(names) + "]")
    print('    y-axis "WER (bag)" 0 --> 1')
    print("    bar [" + ", ".join(str(m("jp2", k)) for k in names) + "]")
    print("    bar [" + ", ".join(str(m("jpg", k)) for k in names) + "]")
    print("```")
    print("\nFirst bar series is JP2, second is IIIF JPEG.")


if __name__ == "__main__":
    main()
