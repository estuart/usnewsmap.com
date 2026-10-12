#!/usr/bin/env python3
"""Markdown tables and Mermaid charts for the survey report, from score.py's output (#296).

    tables.py --data docs/reports/2026-10-ocr-engine-survey/data

Prints to stdout; the report's tables are pasted from it, so each number
in them comes from data/summary.json (and data/scores.jsonl for the sample
table). Pages per day for one engine alone: 86,400 / median single-run
seconds per page, rounded down to the nearest 10.
"""

from __future__ import annotations

import argparse
import json
import statistics
from pathlib import Path

GROUP_ORDER = ["german", "worst-english", "control", "serbian", "russian", "czech", "polish"]
ENGINE_NAMES = {"loc": "LoC", "tesseract": "Tesseract", "doctr": "docTR", "surya": "Surya",
                "ndnp-open-ocr": "NDNP Open OCR", "americanstories": "American Stories"}


def f(x, nd=3):
    if x is None:
        return ""
    if isinstance(x, float):
        return f"{x:.{nd}f}"
    return str(x)


def label(r: dict) -> str:
    e = ENGINE_NAMES.get(r["engine"], r["engine"])
    if r["engine"] == "loc":
        return e
    v = r["variant"]
    s = "" if r["scale"] == 1.0 else f", {int(r['scale'] * 100)}% scale"
    if r["engine"] == "tesseract":
        return f"{e} `{v}`{s}"
    return f"{e}{s}"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--data", type=Path, required=True)
    a = ap.parse_args()
    summary = json.loads((a.data / "summary.json").read_text())
    scores = [json.loads(x) for x in (a.data / "scores.jsonl").read_text().splitlines() if x.strip()]
    rows = summary["by_group_engine"]
    single = {(r["engine"], r["variant"], r["scale"]): r for r in summary["single_run_seconds"]}

    print("## Sample\n")
    print("| Group | Pages | Years | LoC damage (median) | LoC known share (median) | LoC words (median) |")
    print("|---|---|---|---|---|---|")
    for g in GROUP_ORDER:
        loc = [r for r in scores if r["group"] == g and r["engine"] == "loc"]
        if not loc:
            continue
        years = sorted(int(r["page_id"].split("_")[1][:4]) for r in loc)
        print(f"| {g} | {len(loc)} | {years[0]}-{years[-1]} | {f(statistics.median(r['damage_rate'] for r in loc))} | "
              f"{f(statistics.median(r['known_share'] for r in loc))} | {int(statistics.median(r['words'] for r in loc))} |")

    for g in GROUP_ORDER:
        rs = [r for r in rows if r["group"] == g and r["engine"] != "loc"]
        if not rs:
            continue
        print(f"\n## {g}\n")
        print("| Engine | Pages read | Damage (median) | LoC damage, same pages | Change (median) | Pages better "
              "| Known share change (median) | Words vs LoC (median) | Known words vs LoC (median); pages above 1 | Seconds/page alone | Seconds/page, 4 at once "
              "| Peak RSS MB (max) |")
        print("|---|---|---|---|---|---|---|---|---|---|---|---|")
        for r in sorted(rs, key=lambda r: (r["engine"], r["variant"], -r["scale"])):
            s = single.get((r["engine"], r["variant"], r["scale"]))
            loaded = r["engine"] == "tesseract"
            bits = [f"{r['timeouts']} timed out" if r["timeouts"] else "", f"{r['errors']} failed" if r["errors"] else "",
                    f"{r['empty']} with no text" if r.get("empty") else ""]
            note = f" ({', '.join(b for b in bits if b)})" if any(bits) else ""
            print(f"| {label(r)} | {r['ok']} of {r['pages']}{note} | {f(r['median_damage'])} | "
                  f"{f(r.get('loc_median_damage_same_pages'))} | {f(r.get('median_damage_change'))} | "
                  f"{r.get('pages_damage_lower')} of {r.get('pages_compared')} | {f(r.get('median_known_change'))} | "
                  f"{f(r.get('median_words_ratio'), 2)} | "
                  f"{f(r.get('median_known_words_ratio'), 2)}; {r.get('pages_more_known_words')} | "
                  f"{f(s['median_seconds'], 0) if s else ('' if loaded else f(r.get('median_seconds'), 0))} | "
                  f"{f(r.get('median_seconds'), 0) if loaded else ''} | {f(r.get('max_peak_rss_mb'), 0)} |")

    print("\n## Single-run seconds per page\n")
    print("| Engine | Pages | Median s | Min s | Max s | Busy cores (median) | Pages/day, one at a time |")
    print("|---|---|---|---|---|---|---|")
    for r in summary["single_run_seconds"]:
        ppd = int(86400 / r["median_seconds"] // 10 * 10) if r["median_seconds"] else ""
        print(f"| {label(r)} | {r['pages']} | {f(r['median_seconds'], 0)} | {f(r['min_seconds'], 0)} | "
              f"{f(r['max_seconds'], 0)} | {f(r['median_busy_cores'], 2)} | {ppd} |")

    if summary.get("throughput"):
        print("\n## Throughput\n")
        print("| Containers at once | Pages | Wall seconds | Pages/day |")
        print("|---|---|---|---|")
        for r in sorted(summary["throughput"], key=lambda r: r["containers"]):
            print(f"| {r['containers']} | {r['pages']} | {f(r['wall_seconds'], 0)} | {r['pages_per_day']} |")

    if summary.get("german_typeface"):
        print("\n## German typeface\n")
        print("| Page | Title | deu damage | deu_latf damage | deu known | deu_latf known | Reads better |")
        print("|---|---|---|---|---|---|---|")
        for r in sorted(summary["german_typeface"], key=lambda r: r["date"]):
            print(f"| {r['page_id']} | {r['title']} | {f(r['deu_damage'])} | {f(r['deu_latf_damage'])} | "
                  f"{f(r['deu_known'])} | {f(r['deu_latf_known'])} | {r['better']} |")

    print("\n## Chart: median damage by group, LoC against the best engine\n")
    groups, loc_vals, best_vals = [], [], []
    for g in GROUP_ORDER:
        rs = [r for r in rows if r["group"] == g and r["engine"] != "loc" and r["scale"] == 1.0
              and r["median_damage"] is not None and r["ok"] >= max(2, r["pages"] // 2)]
        if not rs:
            continue
        b = min(rs, key=lambda r: r["median_damage"])
        groups.append(g)
        loc_vals.append(b["loc_median_damage_same_pages"])
        best_vals.append(b["median_damage"])
        print(f"%% {g}: best = {label(b)}")
    print("```mermaid\nxychart-beta\n    title \"Median damage rate: LoC (bars) and the best engine (line)\"")
    print("    x-axis [" + ", ".join(f'"{g}"' for g in groups) + "]")
    print("    y-axis \"Damage rate\" 0 --> 0.6")
    print("    bar [" + ", ".join(f"{v:.3f}" for v in loc_vals) + "]")
    print("    line [" + ", ".join(f"{v:.3f}" for v in best_vals) + "]\n```")


if __name__ == "__main__":
    main()
