#!/usr/bin/env python3
"""Build the OCR quality audit's own word lists (ja-ocr/wordlists/<code>.txt, the 5,000 most frequent
words, most frequent first) for languages wordfreq has none for. See ja-ocr/wordlists/README.md.

    scripts/ja-ocr/build-wordlists.py haw   # Hawaiian Corpus Project frequency list (CC0)
    scripts/ja-ocr/build-wordlists.py yi    # Yiddish Wikipedia and Wikisource articles (CC BY-SA 4.0)
    scripts/ja-ocr/build-wordlists.py ger-check --dta DTA.zip   # German: which 19th-century words the
                                                                # audit would count as damage
                                                                # (Deutsches Textarchiv)

Downloads go to a temporary directory and send DNT: 1. Words are folded the way quality.py folds a page's
words (case, combining marks; Hawaiian ʻokina and kahakō, which 19th-century papers didn't print;
Yiddish ligatures as their two letters).
"""

import argparse
import bz2
import collections
import os
import re
import sys
import tempfile
import unicodedata
import urllib.request
import xml.etree.ElementTree as ET

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OUT = os.path.join(ROOT, "ja-ocr", "wordlists")
TOP = 5000
HAW_URL = "https://raw.githubusercontent.com/dohliam/hawaiian-corpus/master/data/freqlist_haw.txt"
YI_URLS = ["https://dumps.wikimedia.org/yiwiki/latest/yiwiki-latest-pages-articles.xml.bz2",
           "https://dumps.wikimedia.org/yiwikisource/latest/yiwikisource-latest-pages-articles.xml.bz2"]


def fetch(url: str, path: str) -> str:
    req = urllib.request.Request(url, headers={"DNT": "1", "User-Agent": "usnewsmap-wordlists"})
    with urllib.request.urlopen(req) as r, open(path, "wb") as f:
        while chunk := r.read(1 << 20):
            f.write(chunk)
    return path


def strip_marks(w: str) -> str:
    return "".join(c for c in unicodedata.normalize("NFD", w) if not unicodedata.combining(c))


def write(code: str, counts: collections.Counter) -> None:
    ranked = [w for w, _ in counts.most_common(TOP)]
    with open(os.path.join(OUT, f"{code}.txt"), "w", encoding="utf-8") as f:
        f.write("\n".join(ranked) + "\n")
    print(f"wrote {code}.txt: {len(ranked)} of {len(counts)} words; top: {' '.join(ranked[:20])}")


def hawaiian(tmp: str) -> None:
    # Native Hawaiian words are (consonant)-vowel syllables; loan letters too. This drops English noise.
    cv = re.compile(r"^(?:[hklmnpwbdfgrstvz]?[aeiou])+$")
    counts: collections.Counter = collections.Counter()
    for line in open(fetch(HAW_URL, os.path.join(tmp, "haw.txt")), encoding="utf-8"):
        n, _, w = line.rstrip("\n").partition("\t")
        w = strip_marks(w.casefold())
        for mark in "ʻ‘'’":
            w = w.replace(mark, "")
        if cv.match(w):
            counts[w] += int(n)
    write("haw", counts)


LIGATURES = str.maketrans({"װ": "וו", "ױ": "וי", "ײ": "יי"})
MARKUP = [(re.compile(r"\[\[[^\]|]*:[^\]]*\]\]"), " "), (re.compile(r"<ref[^>]*/>|<ref.*?</ref>", re.S), " "),
          (re.compile(r"\{\{[^{}]*\}\}"), " "), (re.compile(r"\[\[(?:[^|\]]*\|)?([^\]]*)\]\]"), r"\1"),
          (re.compile(r"https?://\S+"), " "), (re.compile(r"<[^>]+>"), " "), (re.compile(r"'''?|==+"), " ")]
HEBREW_WORD = re.compile(r"[֐-׿יִ-ﭏ]+")


def yiddish(tmp: str) -> None:
    counts: collections.Counter = collections.Counter()
    for url in YI_URLS:
        ns = None
        for _, el in ET.iterparse(bz2.open(fetch(url, os.path.join(tmp, os.path.basename(url))))):
            tag = el.tag.rsplit("}", 1)[-1]
            if tag == "ns":
                ns = el.text
            elif tag == "text" and el.text and ns == "0" and not el.text.lstrip().startswith("#"):  # articles
                text = el.text
                for rx, sub in MARKUP:
                    for _ in range(2):
                        text = rx.sub(sub, text)
                for tok in HEBREW_WORD.findall(text):
                    w = strip_marks(unicodedata.normalize("NFC", tok)).translate(LIGATURES)
                    w = w.replace("־", "").replace("׳", "").replace("״", "")
                    if w.isalpha():
                        counts[w] += 1
            if tag == "page":
                el.clear()
    write("yi", counts)


def german_check(zip_path: str) -> None:
    """The near-misses of German's top 20 the DTA's 1800-1899 texts use most: real old spellings would show
    up here (none do; the top ones are OCR's own errors, older spellings and other languages)."""
    import zipfile

    sys.path.insert(0, os.path.join(ROOT, "ja-ocr"))
    import quality

    m = quality.Models().get("ger")
    counts: collections.Counter = collections.Counter()
    total = 0
    with zipfile.ZipFile(zip_path) as z:
        for name in z.namelist():
            if name.endswith(".txt"):
                words = quality.text_words(z.read(name).decode("utf-8", "replace").replace("ſ", "s"))[1]
                total += len(words)
                counts.update(w for w in words if w in m.near)
    print(f"{total:,} words")
    for w, c in counts.most_common(40):
        print(f"{w}\t{m.near[w]}\t{c}\t{c / total * 1e6:.1f} per million")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0].replace("\n", " "))
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("haw", help="Hawaiian")
    sub.add_parser("yi", help="Yiddish")
    g = sub.add_parser("ger-check", help="German: which 19th-century words would count as damage")
    g.add_argument("--dta", help="the Deutsches Textarchiv's 1800-1899 texts, as a zip (required)")
    # The positional form (`ger-check DTA.zip`) works for one release, with a
    # warning (#277).
    g.add_argument("legacy_dta", nargs="?", help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.cmd == "ger-check":
        if a.legacy_dta is not None:
            if a.dta is not None:
                g.error("give the zip once, as --dta")
            print("warning: a positional DTA.zip is deprecated; use --dta (#277)", file=sys.stderr)
            a.dta = a.legacy_dta
        if a.dta is None:
            g.error("--dta is required")
        german_check(a.dta)
        return
    with tempfile.TemporaryDirectory() as tmp:
        (hawaiian if a.cmd == "haw" else yiddish)(tmp)


if __name__ == "__main__":
    main()
