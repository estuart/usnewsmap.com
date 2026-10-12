#!/usr/bin/env python3
"""Score LoC's text and each engine's text for the survey pages, and summarise (#296).

    score.py --sample DIR --work DIR --out docs/reports/2026-10-ocr-engine-survey/data
    score.py --sample DIR --select-control

Scores come from ja-ocr/quality.py, imported, with its word lists (wordfreq
3.1.1). Every text of a page is scored against the page's group language
(not a language detected per text, so a garbled text can't escape its
language by being undetermined):

- damage_rate: quality.damage over the text's words: near-misses of the
  language's 20 most frequent words over near-misses plus those words. As
  in the audit, a near-miss that is a real English word (top 5,000) doesn't
  count on a non-English page. Serbian Cyrillic is transliterated letter
  for letter first (quality.SERBIAN_LATIN), as the audit does.
- known_share: the share of the text's words (letters only, 2 or more,
  case-folded: quality.text_words) in the language's top 5,000 list
  (LangModel.real); known_words: how many of them there are. Against
  LoC's text, known_words_ratio is the one closest to what search gains:
  above 1, the engine read more words a search could match.
- tokens and words: whitespace tokens and word tokens.

LoC's text also gets quality.score_v2's detected language, to flag sampled
pages that aren't in their group's language.

--select-control keeps the control pages: in draw order, the first
CONTROL_PER_SOURCE candidates of each control title whose LoC text is
detected as English, has at least 1,000 words and damage at most
CONTROL_MAX_DAMAGE. It writes
manifest-final.jsonl with `in_sample` set on every row.

Writes OUT/scores.jsonl (a row per page and text), OUT/summary.json (per
group and engine variant) and OUT/alto.jsonl (ALTO checks).

ALTO checks: well-formed XML, the ALTO namespace, and validity against
LoC's schema for that version (xsd/: alto-2-0, alto-3-0 and alto-4-2 as
published at loc.gov/standards/alto, with their xlink import pointed at a
local copy of LoC's xlink.xsd from loc.gov/standards/mets). ALTO in the
pre-standard CCS namespace (schema.ccs-gmbh.com/ALTO) has no public schema
and is recorded as not checked. Validation needs lxml.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "ja-ocr"))
sys.path.insert(0, HERE)

import altotext  # noqa: E402
import quality  # noqa: E402

CONTROL_PER_SOURCE = 2
CONTROL_MAX_DAMAGE = 0.08
CONTROL_MIN_WORDS = 1000
CLEAR_MARGIN = 0.05  # deu_latf beats deu "clearly": damage lower and known share higher, each by this much
MODELS = quality.Models()


def score_text(text: str, lang: str) -> dict:
    model = MODELS.get(lang)
    n_tokens, words, _, _ = quality.text_words(text)
    if lang == "srp":
        words = [w.translate(quality.SERBIAN_LATIN) for w in words]
    others = () if lang == "eng" else (MODELS.get("eng"),)
    d = quality.damage(model, words, others)
    known = sum(1 for w in words if w in model.real)
    return {
        "tokens": n_tokens, "words": len(words), "head": d.head, "near": d.near,
        "damage_rate": round(d.damage_rate(), 4) if d.damage_rate() is not None else None,
        "known_share": round(known / len(words), 4) if words else None,
        "known_words": known,
    }


def detected(text: str, lang: str) -> str:
    langs = (lang,) if lang == "eng" else (lang, "eng")
    return quality.score_v2(text, langs, MODELS)["lang"]


def load_jsonl(p: Path) -> list[dict]:
    return [json.loads(x) for x in p.read_text().splitlines() if x.strip()] if p.exists() else []


def select_control(sample: Path) -> None:
    rows = load_jsonl(sample / "manifest.jsonl")
    kept: dict[str, int] = {}
    for r in rows:
        if r["group"] != "control":
            r["in_sample"] = True
            continue
        text = (sample / "loctext" / f"{r['page_id']}.txt").read_text(encoding="utf-8")
        s = score_text(text, "eng")
        det = detected(text, "eng")
        ok = (kept.get(r["source"], 0) < CONTROL_PER_SOURCE and det == "eng" and s["words"] >= CONTROL_MIN_WORDS
              and s["damage_rate"] is not None and s["damage_rate"] <= CONTROL_MAX_DAMAGE)
        r["in_sample"] = ok
        r["control_check"] = {"detected": det, "words": s["words"], "damage_rate": s["damage_rate"],
                              "rule": f"first {CONTROL_PER_SOURCE} per title in draw order: detected eng, >= {CONTROL_MIN_WORDS} "
                                      f"words, damage <= {CONTROL_MAX_DAMAGE}"}
        kept[r["source"]] = kept.get(r["source"], 0) + ok
    (sample / "manifest-final.jsonl").write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows))
    print(f"control pages kept: {sum(kept.values())} {kept}")


SCHEMAS = {"v2": "alto-2-0.xsd", "v3": "alto-3-0.xsd", "v4": "alto-4-2.xsd"}
_schemas: dict = {}


def schema(version: str):
    from lxml import etree

    if version not in _schemas:
        _schemas[version] = etree.XMLSchema(etree.parse(os.path.join(HERE, "xsd", SCHEMAS[version])))
    return _schemas[version]


def alto_check(path: Path) -> dict:
    from lxml import etree

    data = path.read_bytes()
    try:
        ns = altotext.namespace(data)
    except Exception as e:  # noqa: BLE001
        return {"emitted": True, "well_formed": False, "error": str(e)[:200]}
    version = altotext.ALTO_NAMESPACES.get(ns)
    out = {"emitted": True, "well_formed": True, "namespace": ns, "alto_version": version, "schema_valid": None}
    if version in SCHEMAS:
        s = schema(version)
        out["schema_valid"] = s.validate(etree.fromstring(data, etree.XMLParser(huge_tree=True)))
        if not out["schema_valid"]:
            out["error"] = str(s.error_log.last_error)[:300]
    return out


def med(xs):
    xs = [x for x in xs if x is not None]
    return round(statistics.median(xs), 4) if xs else None


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--sample", type=Path, required=True)
    ap.add_argument("--work", type=Path)
    ap.add_argument("--out", type=Path)
    ap.add_argument("--select-control", action="store_true")
    ap.add_argument("--timing", type=Path, action="append", default=[],
                    help="a work directory from a run with one container at a time (clean seconds per page)")
    a = ap.parse_args()
    if a.select_control:
        select_control(a.sample)
        return
    manifest = [r for r in load_jsonl(a.sample / "manifest-final.jsonl") if r.get("in_sample")]
    pages = {r["page_id"]: r for r in manifest}
    a.out.mkdir(parents=True, exist_ok=True)
    rows, alto_rows = [], []
    loc = {}
    for pid, r in pages.items():
        text = (a.sample / "loctext" / f"{pid}.txt").read_text(encoding="utf-8")
        s = score_text(text, r["lang"])
        s.update({"page_id": pid, "group": r["group"], "lang": r["lang"], "engine": "loc", "variant": "loc",
                  "scale": 1.0, "status": "ok", "detected_lang": detected(text, r["lang"])})
        loc[pid] = s
        rows.append(s)
        alto_rows.append({"page_id": pid, "engine": "loc", **alto_check(a.sample / "alto" / f"{pid}.xml")})
    for res in sorted((a.work / "results").glob("*.jsonl")):
        if res.stem.startswith("throughput"):
            continue
        for j in load_jsonl(res):
            pid = j["page_id"]
            if pid not in pages:
                continue
            r = pages[pid]
            txt = a.work / "out" / j["engine"] / f"{j['job_id']}.txt"
            text = txt.read_text(encoding="utf-8") if txt.exists() else ""
            s = score_text(text, r["lang"])  # an empty text scores 0 words, no damage rate
            row = {"page_id": pid, "group": r["group"], "lang": r["lang"], **{k: j.get(k) for k in (
                "engine", "variant", "scale", "status", "seconds", "cpu_seconds", "busy_cores", "peak_rss_mb",
                "threads_setting", "engine_version", "model_load_seconds")}, **s}
            row["partial"] = j["status"] == "timeout" and bool(text)
            if s.get("damage_rate") is not None and loc[pid]["damage_rate"] is not None:
                row["damage_change"] = round(s["damage_rate"] - loc[pid]["damage_rate"], 4)
            if s.get("known_share") is not None and loc[pid]["known_share"] is not None:
                row["known_change"] = round(s["known_share"] - loc[pid]["known_share"], 4)
            if s.get("words") is not None and loc[pid]["words"]:
                row["words_ratio"] = round(s["words"] / loc[pid]["words"], 3)
            if s.get("known_words") is not None and loc[pid]["known_words"]:
                row["known_words_ratio"] = round(s["known_words"] / loc[pid]["known_words"], 3)
            rows.append(row)
            x = a.work / "out" / j["engine"] / f"{j['job_id']}.xml"
            alto_rows.append({"page_id": pid, "engine": j["engine"], "variant": j["variant"], "scale": j["scale"],
                              **(alto_check(x) if x.exists() else {"emitted": False})})
    rows.sort(key=lambda x: (x["group"], x["page_id"], x["engine"] != "loc", x["engine"], x["variant"],
                             -x["scale"]))
    (a.out / "scores.jsonl").write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows))
    (a.out / "alto.jsonl").write_text("".join(json.dumps(r) + "\n" for r in alto_rows))

    summary = []
    keys = sorted({(r["group"], r["engine"], r["variant"], r["scale"]) for r in rows})
    for g, e, v, sc in keys:
        rs = [r for r in rows if (r["group"], r["engine"], r["variant"], r["scale"]) == (g, e, v, sc)]
        ok = [r for r in rs if r["status"] == "ok"]
        out = {"group": g, "engine": e, "variant": v, "scale": sc, "pages": len(rs), "ok": len(ok),
               "timeouts": sum(r["status"] == "timeout" for r in rs), "errors": sum(r["status"] == "error" for r in rs),
               "median_damage": med(r.get("damage_rate") for r in ok),
               "median_known_share": med(r.get("known_share") for r in ok),
               "median_words": med(r.get("words") for r in ok),
               "empty": sum(1 for r in ok if not r.get("words"))}
        if e != "loc":
            ch = [r["damage_change"] for r in ok if r.get("damage_change") is not None]
            out.update({
                "median_damage_change": med(ch),
                "pages_damage_lower": sum(c < 0 for c in ch), "pages_compared": len(ch),
                "median_known_change": med(r.get("known_change") for r in ok),
                "median_words_ratio": med(r.get("words_ratio") for r in ok),
                "median_known_words_ratio": med(r.get("known_words_ratio") for r in ok),
                "pages_more_known_words": sum(1 for r in ok if (r.get("known_words_ratio") or 0) > 1),
                # over the pages where the engine's own metric exists, as for its median above
                "loc_median_damage_same_pages": med(loc[r["page_id"]]["damage_rate"] for r in ok
                                                    if r.get("damage_rate") is not None),
                "loc_median_known_share_same_pages": med(loc[r["page_id"]]["known_share"] for r in ok
                                                         if r.get("known_share") is not None),
                "median_seconds": med(r.get("seconds") for r in ok),
                "median_peak_rss_mb": med(r.get("peak_rss_mb") for r in ok),
                "max_peak_rss_mb": max((r["peak_rss_mb"] for r in rs if r.get("peak_rss_mb")), default=None),
                "median_busy_cores": med(r.get("busy_cores") for r in ok),
            })
        summary.append(out)

    # Seconds per page measured with the engine alone on the node (--timing runs)
    timing = []
    for d in a.timing:
        for res in sorted((d / "results").glob("*.jsonl")):
            for j in load_jsonl(res):
                if j["page_id"] in pages and j.get("concurrency", 1) == 1:
                    timing.append({**{k: j.get(k) for k in ("page_id", "engine", "variant", "scale", "status",
                                                             "seconds", "cpu_seconds", "busy_cores", "peak_rss_mb",
                                                             "threads_setting", "engine_version")},
                                   "group": pages[j["page_id"]]["group"], "timing_dir": d.name})
    (a.out / "timing.jsonl").write_text("".join(json.dumps(r) + "\n" for r in timing))
    single = []
    for e, v, sc in sorted({(r["engine"], r["variant"], r["scale"]) for r in timing}):
        rs = [r for r in timing if (r["engine"], r["variant"], r["scale"]) == (e, v, sc) and r["status"] == "ok"]
        if rs:
            secs = [r["seconds"] for r in rs]
            single.append({"engine": e, "variant": v, "scale": sc, "pages": len(rs),
                           "median_seconds": med(secs), "min_seconds": min(secs), "max_seconds": max(secs),
                           "median_busy_cores": med(r["busy_cores"] for r in rs),
                           "max_peak_rss_mb": max(r["peak_rss_mb"] for r in rs)})
    throughput = load_jsonl(a.work / "results" / "throughput.jsonl")

    # German: deu_latf against deu, page by page (#246's typeface question)
    typeface = []
    for pid, r in pages.items():
        if r["group"] != "german":
            continue
        by = {r2["variant"]: r2 for r2 in rows if r2["page_id"] == pid and r2["engine"] == "tesseract"
              and r2["scale"] == 1.0 and r2["status"] == "ok"}
        if "deu" in by and "deu_latf" in by:
            f, d = by["deu_latf"], by["deu"]
            dd = (d.get("damage_rate") or 0) - (f.get("damage_rate") or 0)
            dk = (f.get("known_share") or 0) - (d.get("known_share") or 0)
            verdict = ("deu_latf" if dd >= CLEAR_MARGIN and dk >= CLEAR_MARGIN else
                       "deu" if -dd >= CLEAR_MARGIN and -dk >= CLEAR_MARGIN else "unclear")
            typeface.append({"page_id": pid, "date": r["date"], "title": r["source_label"],
                             "deu_damage": d.get("damage_rate"), "deu_latf_damage": f.get("damage_rate"),
                             "deu_known": d.get("known_share"), "deu_latf_known": f.get("known_share"),
                             "better": verdict})
    (a.out / "summary.json").write_text(json.dumps({
        "by_group_engine": summary, "german_typeface": typeface, "clear_margin": CLEAR_MARGIN,
        "single_run_seconds": single, "throughput": throughput}, indent=1, ensure_ascii=False) + "\n")
    for s in summary:
        print(json.dumps(s, ensure_ascii=False))
    for t in typeface:
        print(json.dumps(t, ensure_ascii=False))


if __name__ == "__main__":
    main()
