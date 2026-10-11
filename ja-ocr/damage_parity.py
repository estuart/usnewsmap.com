"""Pages scored by damage_v2.py, for checking the Rust port of the damage metric (usnm_core::damage,
12 section 12.5 Step 5) against it.

    jaocr.py damage-parity [--pages 400] [--audit-version pages-v20261003-1]
    python3 damage_parity.py --pages-from FILE --out PATH [--synthetic]
    python3 damage_parity.py --rescore PATH --out PATH

In the cloud (`jaocr.py damage-parity`, on the one-replica audit job, since curated text is reachable only
inside the VNet), it samples --pages pages from the 10% audit's per-page CSV
(audit/ocr-quality-v2-<audit-version>-10pct-pages.csv.gz), spread across languages, decisions and scripts:
the pages of each (language, decision, script) are ordered by a hash of their doc_id and the groups take
turns. It reads their curated text and writes audit/damage-parity/<stamp>.jsonl.zst to the curated store.
It logs each sampled doc_id and its title's languages, never text.

On a laptop (--pages-from), it reads loc.gov's OCR text for the pages in FILE (JSON lines with doc_id and
languages, the title's catalog languages), with jaocr.py's pacing, and with --synthetic adds made-up pages
for the known traps between Python and Rust (casefold, combining marks, separators that str.split takes as
whitespace). That builds the small fixture CI checks: crates/usnm-core/tests/data/damage-parity.jsonl.zst.
After a change to damage_v2.py, --rescore scores a file's pages again from the text it holds, with no
loc.gov requests.

Each file is JSON lines, zstd-compressed. The first line is {"header": {...}} with the sha256 of
damage_v2.py and of our own word lists (wordlists/haw.txt and yi.txt), the wordfreq and Unicode versions and
the page count. Each other line is a page: doc_id, the title's
languages, the text, score_v2's output ("score", rates at full precision), detect()'s damage counts
("damage": words, function, head, near; score_v2 doesn't return them) and the forced-language score of each
of the title's languages and English ("forced", by MARC code; null without a word list).
"""

from __future__ import annotations

import argparse
import csv
import gzip
import hashlib
import io
import json
import os
import sys
import unicodedata
from datetime import datetime, timezone

import damage_v2

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_AUDIT_VERSION = "pages-v20261003-1"
API_GAP, TILE_GAP = 3.5, 1.0  # jaocr.py's loc.gov pacing, in seconds


def damage_v2_sha256() -> str:
    with open(os.path.join(HERE, "damage_v2.py"), "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def wordlists_sha256() -> dict[str, str]:
    """sha256 of our own word lists (damage_v2.LOCAL_WORDLISTS), which the models read besides wordfreq's."""
    out = {}
    for code in sorted(damage_v2.LOCAL_WORDLISTS.values()):
        with open(os.path.join(damage_v2.WORDLIST_DIR, f"{code}.txt"), "rb") as f:
            out[code] = hashlib.sha256(f.read()).hexdigest()
    return out


def wordfreq_version() -> str | None:
    try:
        from importlib.metadata import version

        return version("wordfreq")
    except Exception:  # noqa: BLE001 - only for the header
        return None


def record(doc_id: str, text: str, languages: tuple, models: damage_v2.Models) -> dict:
    """One page as the parity check reads it."""
    s = damage_v2.score_v2(text, languages, models)
    _, words, _, _ = damage_v2.text_words(text)
    d = damage_v2.detect(words, languages, models).damage
    forced = {lang: damage_v2.forced(text, lang, models) for lang in dict.fromkeys([*languages, damage_v2.ENGLISH])}
    return {"doc_id": doc_id, "languages": list(languages), "text": text, "score": s,
            "damage": None if d is None else {"words": d.words, "function": d.function, "head": d.head,
                                              "near": d.near},
            "forced": forced}


def header(source: str, pages: int) -> dict:
    return {"header": {"damage_v2_sha256": damage_v2_sha256(), "wordlists_sha256": wordlists_sha256(),
                       "wordfreq": wordfreq_version(),
                       "python": sys.version.split()[0], "unicode": unicodedata.unidata_version,
                       "source": source, "pages": pages,
                       "created_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")}}


def zstd_compress(data: bytes) -> bytes:
    """A zstd frame: the standard library's on Python 3.14, else pyarrow's (the job's image)."""
    try:
        from compression import zstd

        return zstd.compress(data, level=19)
    except ImportError:
        import pyarrow as pa

        sink = pa.BufferOutputStream()
        with pa.output_stream(sink, compression="zstd") as out:
            out.write(data)
        return sink.getvalue().to_pybytes()


def zstd_decompress(data: bytes) -> bytes:
    try:
        from compression import zstd

        return zstd.decompress(data)
    except ImportError:
        import pyarrow as pa

        return pa.input_stream(pa.BufferReader(data), compression="zstd").read()


def encode(source: str, records: list[dict]) -> bytes:
    lines = [header(source, len(records)), *records]
    return zstd_compress("".join(json.dumps(r, ensure_ascii=False, separators=(",", ":")) + "\n"
                                 for r in lines).encode())


# ---------------------------------------------------------------- sampling (cloud)


def order_key(doc_id: str) -> bytes:
    return hashlib.blake2b(doc_id.encode(), digest_size=8).digest()


def spread(rows: list[dict], n: int) -> list[dict]:
    """n rows spread across (language, decision, script): each group's rows in hash order, groups taking
    turns in sorted order."""
    groups: dict[tuple, list[dict]] = {}
    for r in rows:
        groups.setdefault((r["language"], r["decision"], r["script"]), []).append(r)
    queues = [sorted(g, key=lambda r: order_key(r["doc_id"])) for _, g in sorted(groups.items())]
    out: list[dict] = []
    i = 0
    while len(out) < n and any(queues):
        q = queues[i % len(queues)]
        if q:
            out.append(q.pop(0))
        i += 1
    return out


def damage_parity(reference, curated, pages: int = 400, audit_version: str = DEFAULT_AUDIT_VERSION) -> str:
    """Sample pages from the 10% audit, score their curated text, write the parity file; its path."""
    import pyarrow.parquet as pq

    import jaocr
    import quality

    if pages < 1:
        raise ValueError(f"--pages must be at least 1, not {pages}")
    if not jaocr.SAFE_SEGMENT.match(audit_version):
        raise ValueError(f"unsafe audit version: {audit_version!r}")
    csv_path = f"audit/ocr-quality-v2-{audit_version}-10pct-pages.csv.gz"
    rows = [r for r in csv.DictReader(io.StringIO(gzip.decompress(curated.read(csv_path)).decode()))
            if r["decision"] != damage_v2.NO_TEXT]
    chosen = spread(rows, pages)
    for r in chosen:
        jaocr.log("damage parity page", doc_id=r["doc_id"], languages=r["title_languages"], batch=r["batch"])
    batches_path = f"{audit_version}/batches.json"
    if not reference.exists(batches_path):  # an old version's files can be gone: the current one's
        current = json.loads(reference.read("current.json"))["reference"]
        if not jaocr.SAFE_SEGMENT.match(current):
            raise ValueError(f"current.json names an unsafe reference version: {current!r}")
        batches_path = f"{current}/batches.json"
    parts = {quality.batch_name(b): (b.get("curated") or {}).get("parts") or []
             for b in json.loads(reference.read(batches_path))}
    wanted: dict[str, dict[str, dict]] = {}
    for r in chosen:
        wanted.setdefault(r["batch"], {})[r["doc_id"]] = r
    texts: dict[str, str] = {}
    for batch, by_id in sorted(wanted.items()):
        for part in parts.get(batch, []):
            f = pq.ParquetFile(io.BytesIO(curated.read(part)))
            for rb in f.iter_batches(batch_size=4096, columns=["doc_id", "text"]):
                for doc_id, text in zip(rb.column("doc_id").to_pylist(), rb.column("text").to_pylist()):
                    if doc_id in by_id and text:
                        texts[doc_id] = text
    models = damage_v2.Models()
    records = [record(r["doc_id"], texts[r["doc_id"]], tuple(r["title_languages"].split()), models)
               for r in chosen if r["doc_id"] in texts]
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = f"audit/damage-parity/{stamp}.jsonl.zst"
    curated.write(path, encode("curated", records))
    jaocr.log("damage parity written", path=path, pages=len(records), sampled=len(chosen),
              missing=len(chosen) - len(records), audit=csv_path, damage_v2_sha256=damage_v2_sha256())
    return path


# ---------------------------------------------------------------- loc.gov pages and traps (laptop)


def loc_text(doc_id: str, api, tile, items: dict) -> str:
    """loc.gov's OCR text of a page, by doc_id (lccn_date_ed-N_seq-M)."""
    import jaocr

    lccn, date, ed, seq = doc_id.split("_")
    url = f"https://www.loc.gov/item/{lccn}/{date}/{ed}/?fo=json"
    if url not in items:
        items[url] = json.loads(jaocr.fetch(url, api))
    files = items[url]["resources"][0]["files"][int(seq.removeprefix("seq-")) - 1]
    service = next(f["fulltext_service"] for f in files if f.get("mimetype") == "text/plain")
    # {"<ALTO path>": {"full_text": "...", "height": ..., "width": ...}}
    (page,) = json.loads(jaocr.fetch(service, tile)).values()
    return page["full_text"]


ENGLISH = ("The council of the city met on Tuesday evening at the court house, and after a long debate it was "
           "voted that the new school should be built on the lot which the board had bought last year. ")
GERMAN = ("Der Stadtrath hat am Dienstag beschlossen, dass die neue Schule auf dem Grundstück gebaut werden "
          "soll, welches die Behörde im vorigen Jahre gekauft hat. Es ist noch nicht bekannt, wann sie beginnt. ")
SERBIAN = ("Општински одбор састао се у уторак увече у судници и после дуге расправе решено је да се нова школа "
           "сагради на плацу који је управа купила прошле године. ")
RUSSIAN = ("Городской совет собрался во вторник вечером в здании суда и после долгого спора было решено, что "
           "новая школа будет построена на участке, который управление купило в прошлом году. ")
YIDDISH = ("דער שטאט ראט האט זיך פֿארזאמלט דינסטיק אין אוונט און נאך א לאנגע וויכוח האט מען באשלאסן אז די "
           "נייע שול וועט געבויט ווערן אויף דעם פלאץ וואס די קאמיסיע האט געקויפט פאר א יאר. ")


def synthetic() -> list[tuple[str, str, tuple]]:
    """(doc_id, text, languages) for the traps between Python's string handling and Rust's."""
    return [
        # casefold: ß and ẞ to "ss", ſ to "s", final sigma; "STRASSE" and "Straße" are one word.
        ("synthetic-casefold", (GERMAN + "Die Straße und die STRASSE, der Fuß, daß, ẞ, ſich, Maß. ") * 4,
         ("ger", "eng")),
        # Combining marks: Python's isalpha and \w don't take them, Rust's is_alphabetic does.
        ("synthetic-combining", (GERMAN.replace("ü", "ü").replace("ö", "ö") + "café ") * 4,
         ("ger",)),
        ("synthetic-hebrew-points", YIDDISH * 4 + "פֿון אַ װאָך ײִדיש ױך", ("yid", "eng")),
        # str.split() takes \x1c to \x1f as whitespace; split_whitespace doesn't.
        ("synthetic-separators", ENGLISH.replace(" ", "\x1c", 5).replace(" ", "\x1f", 5) * 4, ("eng",)),
        ("synthetic-unicode-spaces", ENGLISH.replace(" ", " ", 8).replace(" ", "　", 4) * 4, ("eng",)),
        # Hyphenated line breaks, with spaces before the next line.
        ("synthetic-dehyphenate", ("The rail-\n   road was built by the coun-\ncil and the peo- \nple. " + ENGLISH) * 3,
         ("eng",)),
        ("synthetic-serbian-mixed", (SERBIAN + ENGLISH) * 3, ("srp", "eng")),
        ("synthetic-serbian-cyrillic", SERBIAN * 4, ("srp", "eng")),
        ("synthetic-russian", RUSSIAN * 4, ("rus", "srp")),
        ("synthetic-turkish-dotted", ("İSTANBUL İzmir ığdır " + ENGLISH) * 4, ("eng",)),
        ("synthetic-garbage", ("tHe aaaa bcdfg %%%a xviii MCMXLV McKay USA ThE HeLLoS iii xxx "
                               + ENGLISH.replace("the ", "tbe ").replace("and ", "aud ")) * 3, ("eng",)),
        ("synthetic-czech-no-vowels", ("Smrt prst vlk krk " + ENGLISH) * 3, ("cze", "eng")),
        ("synthetic-numbers", ("½ ² ³ Ⅻ 1905 ١٢٣ " + ENGLISH) * 3, ("eng",)),
        ("synthetic-greek", "Ο ΣΙΣΥΦΟΣ και η πόλις της Ελλάδος. " * 12, ("gre", "eng")),
        ("synthetic-dano-norwegian", ("Han kom paa Mandag til Byen, og der var mange Folk af alle Slags. " * 6),
         ("nor", "dan", "eng")),
        ("synthetic-short", "The council met.", ("eng",)),
        ("synthetic-empty", "", ("eng",)),
    ]


def main() -> int:
    import jaocr

    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--pages-from",
                     help="JSON lines, one page each: {\"doc_id\": ..., \"languages\": [MARC codes]}")
    src.add_argument("--rescore", help="a parity file whose pages to score again (after a change to damage_v2.py), "
                                       "with no loc.gov requests")
    ap.add_argument("--out", required=True, help="the .jsonl.zst file to write")
    ap.add_argument("--synthetic", action="store_true", help="also the made-up pages for the known traps")
    a = ap.parse_args()
    models = damage_v2.Models()
    if a.rescore:
        with open(a.rescore, "rb") as f:
            old = [json.loads(line) for line in zstd_decompress(f.read()).decode().splitlines() if line.strip()]
        head, pages = old[0]["header"], old[1:]
        records = [record(p["doc_id"], p["text"], tuple(p["languages"]), models) for p in pages]
        with open(a.out, "wb") as f:
            f.write(encode(head["source"], records))
        print(f"{a.out}: {len(records)} pages scored again", file=sys.stderr)
        return 0
    with open(a.pages_from, encoding="utf-8") as f:
        pages = [json.loads(line) for line in f if line.strip()]
    api, tile, items = jaocr.Pacer(API_GAP), jaocr.Pacer(TILE_GAP), {}
    records = []
    for p in pages:
        text = loc_text(p["doc_id"], api, tile, items)
        records.append(record(p["doc_id"], text, tuple(p["languages"]), models))
        print(f"{p['doc_id']}: {records[-1]['score']['lang']} {records[-1]['score']['decision']}", file=sys.stderr)
    if a.synthetic:
        records += [record(d, t, langs, models) for d, t, langs in synthetic()]
    with open(a.out, "wb") as f:
        f.write(encode("loc.gov" + (" and synthetic" if a.synthetic else ""), records))
    print(f"{a.out}: {len(records)} pages", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
