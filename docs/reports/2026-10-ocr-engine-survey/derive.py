#!/usr/bin/env python3
"""Per-engine speed, memory, failures, ALTO and typeface counts for the survey report (#296).

    derive.py --data docs/reports/2026-10-ocr-engine-survey/data

Reads only score.py's output in DATA (scores.jsonl, timing.jsonl, alto.jsonl,
summary.json) and prints Markdown tables; tables.py prints the per-group
tables. Pages per minute per core is 60 / median CPU-seconds per page, so an
engine that keeps 8 cores busy for 100 s scores the same as one that keeps 1
core busy for 800 s. Speed comes from runs with one container on the node:
the timing passes (Tesseract on node 1; docTR and American Stories on node 2)
and the main runs of NDNP Open OCR (node 1) and Surya (node 2), which ran one
container at a time.
"""

from __future__ import annotations

import argparse
import collections
import json
import statistics
from pathlib import Path

NODE = {"tesseract": "node 1", "ndnp-open-ocr": "node 1", "doctr": "node 2", "americanstories": "node 2",
        "surya": "node 2"}
NAMES = {"tesseract": "Tesseract", "ndnp-open-ocr": "NDNP Open OCR", "doctr": "docTR",
         "americanstories": "American Stories", "surya": "Surya"}


def load(p: Path) -> list[dict]:
    return [json.loads(x) for x in p.read_text().splitlines() if x.strip()]


def label(e: str, v: str, sc: float) -> str:
    s = "" if sc == 1.0 else ", 50% scale"
    return f"{NAMES[e]} `{v}`{s}" if e == "tesseract" else f"{NAMES[e]}{s}"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--data", type=Path, required=True)
    a = ap.parse_args()
    scores = load(a.data / "scores.jsonl")
    timing = load(a.data / "timing.jsonl")
    alto = load(a.data / "alto.jsonl")
    summary = json.loads((a.data / "summary.json").read_text())

    print("## Speed and memory, one container on the node\n")
    print("| Engine | Node | Pages | Seconds/page (median) | CPU-seconds/page (median) | Busy cores (median) "
          "| Pages/min/core | Peak RSS MB (max) | Source |")
    print("|---|---|---|---|---|---|---|---|---|")
    groups = collections.defaultdict(list)
    for r in timing:
        if r["status"] == "ok":
            groups[(r["engine"], r["variant"], r["scale"], "timing pass")].append(r)
    for r in scores:
        if r["engine"] in ("ndnp-open-ocr", "surya") and r["status"] == "ok":
            groups[(r["engine"], r["variant"], r["scale"], "main run")].append(r)
    speed_chart = []
    for (e, v, sc, src), rs in sorted(groups.items(), key=lambda kv: (kv[0][0] != "tesseract", kv[0])):
        cpu = statistics.median(r["cpu_seconds"] for r in rs)
        ppmc = 60 / cpu
        print(f"| {label(e, v, sc)} | {NODE[e]} | {len(rs)} | {statistics.median(r['seconds'] for r in rs):.0f} | "
              f"{cpu:.0f} | {statistics.median(r['busy_cores'] for r in rs):.2f} | {ppmc:.3f} | "
              f"{max(r['peak_rss_mb'] for r in rs):.0f} | {src} |")
        if sc == 1.0 and (e != "tesseract" or v in ("eng", "deu_latf")):
            speed_chart.append((label(e, v, sc).replace("`", ""), ppmc))

    print("\n## Pages, failures and memory per engine (all runs)\n")
    print("| Engine | Jobs | ok | Timed out | ok with no text | Partial text kept | Peak RSS MB (max) |")
    print("|---|---|---|---|---|---|---|")
    for e in NAMES:
        rs = [r for r in scores if r["engine"] == e]
        print(f"| {NAMES[e]} | {len(rs)} | {sum(r['status'] == 'ok' for r in rs)} | "
              f"{sum(r['status'] == 'timeout' for r in rs)} | "
              f"{sum(r['status'] == 'ok' and not r['words'] for r in rs)} | {sum(bool(r.get('partial')) for r in rs)} | "
              f"{max(r['peak_rss_mb'] for r in rs):.0f} |")

    print("\n## ALTO from the engine\n")
    print("| Engine | Files checked | Emitted | ALTO version | Valid against LoC's schema |")
    print("|---|---|---|---|---|")
    for e in ["loc", *NAMES]:
        rs = [r for r in alto if r["engine"] == e]
        vers = collections.Counter(r.get("alto_version") for r in rs if r.get("emitted"))
        print(f"| {NAMES.get(e, 'LoC')} | {len(rs)} | {sum(bool(r.get('emitted')) for r in rs)} | "
              f"{', '.join(f'{k}: {n}' for k, n in sorted(vers.items(), key=str)) or '-'} | "
              f"{sum(r.get('schema_valid') is True for r in rs)} |")

    t = collections.Counter(r["better"] for r in summary["german_typeface"])
    no_text = sum(r["deu_damage"] is None and r["deu_latf_damage"] is None for r in summary["german_typeface"])
    print(f"\nGerman typeface, {len(summary['german_typeface'])} pages: deu_latf clearly better on {t['deu_latf']}, "
          f"deu on {t['deu']}, unclear on {t['unclear']} (of which {no_text} have no text from either model); "
          f"margin {summary['clear_margin']} on both damage and known share.")

    print("\n```mermaid\nxychart-beta\n    title \"Pages per minute per core (one container on the node)\"")
    print("    x-axis [" + ", ".join(f'"{n}"' for n, _ in speed_chart) + "]")
    print(f"    y-axis \"Pages/min/core\" 0 --> {max(v for _, v in speed_chart) * 1.2:.2f}")
    print("    bar [" + ", ".join(f"{v:.3f}" for _, v in speed_chart) + "]\n```")


if __name__ == "__main__":
    main()
