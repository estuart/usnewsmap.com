#!/usr/bin/env python3
"""Score the OCR engines' output on a sample (#128).

    scripts/ja-ocr/score.py --sample-dir SAMPLE_DIR [--engines ndlocr-lite azure tesseract-jpn_vert]

Needs rapidfuzz (pip install rapidfuzz). Prints markdown:

- Per engine: pages with output, median seconds per page, characters per
  page, and the share that are Japanese script (kana and kanji). A page
  where LoC found no text is Japanese, so a low share means noise.
- Agreement: between each pair of engines, on the Japanese characters only
  (punctuation, spaces and Latin dropped). Two good engines agree; this
  doesn't say which is right.
- Accuracy, when SAMPLE_DIR/refs/<page_id>.txt exist (transcriptions of
  crops; see crops.py): each engine against them.

Two measures throughout. CER (character error rate, edit distance over the
reference length) also counts reading-order differences, which vertical
pages have plenty of. Bigram F1 (overlap of adjacent character pairs) ignores
order beyond pairs, so it measures what search sees: whether the words are
there. Old character forms are folded to modern ones on both sides first
(戰 → 戦), since engines differ on that and searchers type modern forms.
"""

import argparse
import csv
import re
import statistics
import sys
from pathlib import Path

from rapidfuzz.distance import Levenshtein

OLD = (
    "戰戦國国會会學学與与爲為來来傳伝權権氣気靜静發発驛駅觸触歐欧經経縣県團団體体實実樂楽舊旧當当對対數数擧挙圖図廣広澤沢"
    "濟済號号變変證証讀読賣売錢銭鐵鉄關関寫写應応參参囘回區区單単嚴厳壽寿將将從従價価歸帰屆届戲戯擔担據拠收収晝昼榮栄歲歳"
    "歷歴殘残滿満爭争獨独竝並粹粋絲糸總総繪絵聲声臺台舍舎藥薬處処衞衛裝装覺覚觀観譯訳豐豊轉転辭辞遲遅邊辺醫医鎭鎮雜雑顯顕"
    "驗験黨党齡齢圓円惡悪戀恋拂払條条檢検淺浅灣湾燈灯狀状畫画盜盗眞真碎砕稱称穩穏縱縦聽聴肅粛腦脳臟臓莊荘萬万蟲虫覽覧"
    "謠謡譽誉賴頼踐践輕軽辯弁遞逓鄕郷釋釈鑛鉱陷陥險険隨随雙双靈霊顏顔餘余髮髪鬪闘鷄鶏黑黒默黙齒歯廳庁縣県聯連壯壮狹狭"
    "產産亞亜惠恵擴拡瀨瀬繼継效効礙碍廢廃燒焼")
FOLD = str.maketrans(OLD[0::2], OLD[1::2])
JA = re.compile(r"[぀-ヿ㐀-䶿一-鿿豈-﫿々〆ヶ]")


def ja_only(s: str) -> str:
    return "".join(JA.findall(s)).translate(FOLD)


def bigram_f1(hyp: str, ref: str) -> float:
    from collections import Counter

    h = Counter(hyp[i:i + 2] for i in range(len(hyp) - 1))
    r = Counter(ref[i:i + 2] for i in range(len(ref) - 1))
    common = sum((h & r).values())
    if not common:
        return 0.0
    p, rc = common / sum(h.values()), common / sum(r.values())
    return 2 * p * rc / (p + rc)


def cer(hyp: str, ref: str) -> float:
    return Levenshtein.distance(hyp, ref) / max(len(ref), 1)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--sample-dir", dest="sample", type=Path, help="the sample to score (required)")
    # The positional form (`score.py SAMPLE_DIR`) works for one release, with a
    # warning (#277).
    ap.add_argument("legacy_sample", nargs="?", type=Path, help=argparse.SUPPRESS)
    ap.add_argument("--engines", nargs="+", default=["ndlocr-lite", "azure", "tesseract-jpn_vert"])
    a = ap.parse_args()
    if a.legacy_sample is not None:
        if a.sample is not None:
            ap.error("give SAMPLE_DIR once, as --sample-dir")
        print("warning: a positional SAMPLE_DIR is deprecated; use --sample-dir (#277)", file=sys.stderr)
        a.sample = a.legacy_sample
    if a.sample is None:
        ap.error("--sample-dir is required")
    rows = list(csv.DictReader((a.sample / "sample.csv").open()))
    engines = [e for e in a.engines if (a.sample / "out" / e).is_dir()]
    text = {e: {} for e in engines}
    secs = {e: {} for e in engines}
    for e in engines:
        d = a.sample / "out" / e
        for r in rows:
            f = d / f"{r['page_id']}.txt"
            if f.exists():
                text[e][r["page_id"]] = f.read_text()
        t = d / "times.csv"
        if t.exists():
            secs[e] = {x["page_id"]: float(x["seconds"]) for x in csv.DictReader(t.open())}

    print(f"## {a.sample.name}: {len(rows)} pages\n")
    print("| engine | pages | s/page (median) | chars/page (median) | Japanese share (median) |")
    print("|---|---|---|---|---|")
    for e in engines:
        t = text[e]
        if not t:
            continue
        chars = [len(re.sub(r"\s", "", s)) for s in t.values()]
        share = [len(ja_only(s)) / max(len(re.sub(r"\s", "", s)), 1) for s in t.values()]
        s = statistics.median(secs[e].values()) if secs[e] else float("nan")
        print(f"| {e} | {len(t)} | {s:.1f} | {statistics.median(chars):.0f} | {statistics.median(share):.0%} |")

    print("\n### Agreement between engines (Japanese characters only)\n")
    print("| pair | pages | bigram F1 median (higher is closer) | CER median |")
    print("|---|---|---|---|")
    for i, x in enumerate(engines):
        for y in engines[i + 1:]:
            both = [p for p in text[x] if p in text[y]]
            if not both:
                continue
            f1 = [bigram_f1(ja_only(text[x][p]), ja_only(text[y][p])) for p in both]
            v = [cer(ja_only(text[x][p]), ja_only(text[y][p])) for p in both]
            print(f"| {x} vs {y} | {len(both)} | {statistics.median(f1):.1%} | {statistics.median(v):.1%} |")

    refs = a.sample / "refs"
    if refs.is_dir():
        print("\n### Accuracy against reference transcriptions (Japanese characters only)\n")
        print("| engine | crops | bigram F1 mean | CER mean | by group (F1 / CER) |")
        print("|---|---|---|---|---|")
        group = {r["page_id"]: r.get("group", "") for r in rows}
        for e in engines:
            v, by = [], {}
            for f in sorted(refs.glob("*.txt")):
                pid, ref = f.stem, ja_only(f.read_text())
                if pid not in text[e] or len(ref) < 10:
                    continue
                hyp = ja_only(text[e][pid])
                pair = (bigram_f1(hyp, ref), cer(hyp, ref))
                v.append(pair)
                by.setdefault(group.get(pid, ""), []).append(pair)
            if v:
                g = ", ".join(
                    f"{k or '-'} {statistics.mean(x[0] for x in xs):.0%} / {statistics.mean(x[1] for x in xs):.0%} ({len(xs)})"
                    for k, xs in sorted(by.items()))
                print(f"| {e} | {len(v)} | {statistics.mean(x[0] for x in v):.1%} | {statistics.mean(x[1] for x in v):.1%} | {g} |")


if __name__ == "__main__":
    main()
