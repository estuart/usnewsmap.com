#!/usr/bin/env python3
"""Generate the SYNTHETIC fixture corpus used for local development and tests.

Everything here is invented: place and title names are labelled "Fixture",
LCCNs use the unassigned `sn99…` range, and page text is templated. It must
never be presented as real Chronicling America data.

Usage: python3 fixtures/generate.py   (writes fixtures/data/…)
"""
import hashlib
import json
import random
from datetime import date, timedelta
from pathlib import Path

OUT = Path(__file__).parent / "data"
VERSION = "fixture-v1"
EPOCH = date(1700, 1, 1)

PLACES = [
    # id, ordinal, name, state, lat, lon, precision
    ("P00001", 1, "Fixture City A (Chicago area)", "IL", 41.88, -87.63, "city"),
    ("P00002", 2, "Fixture City B (New York area)", "NY", 40.71, -74.01, "city"),
    ("P00003", 3, "Fixture City C (San Francisco area)", "CA", 37.77, -122.42, "city"),
    ("P00004", 4, "Fixture City D (Atlanta area)", "GA", 33.75, -84.39, "city"),
    ("P00005", 5, "Fixture City E (Charleston area)", "SC", 32.78, -79.93, "city"),
    ("P00006", 6, "Fixture County F (Nebraska)", "NE", 41.26, -95.94, "county"),
]

FILLER = (
    "the council met on tuesday and the price of cotton rose while the railroad "
    "announced a new schedule for the northern line and the weather remained fair"
).split()

TOPICS = {
    "gold": "you shall not crucify mankind upon a cross of gold said the speaker to the convention",
    "silver": "the friends of free silver gathered to hear the orator on the money question",
    "standard": "bankers defended the gold standard against the silver movement in the east",
    "fever": "reports of yellow fever reached the port and the board of health met",
}

# Days after 1896-07-09 when each place first carries the speech (east first, then west).
SPREAD = {"P00001": 1, "P00002": 4, "P00004": 9, "P00005": 12, "P00003": 20, "P00006": 30}

# Catalog languages of each place's one title, for the language filter. Most
# are English; one title lists two languages (it counts in both), and two
# list one other language. The page text stays English: it is templated.
LANGUAGES = {"P00002": ["eng", "ger"], "P00003": ["spa"], "P00006": ["ger"]}


# A synthetic Japanese title (#139): its pages are the Japanese pages we OCR
# ourselves, in their own index (pages-ja-fixture), with the printed text in
# `printed` and its search tokens in `text`. The printed text uses old forms
# (戰爭, 米國, 選擧), small kana and voiced kana, so folding is exercised.
# lccn, name, place, state, title ordinal (the place's ordinal sets place_shard)
JA_TITLE = ("sn99000901", "Fixture Shimpo (Japanese)", "P00003", "CA", 7)
JA_WORDS = ["日本", "東京", "米國", "戰爭", "平和", "選擧", "ニュース", "ロッキー", "新報",
            "學校", "ガス", "收容所", "デンバー", "がっこう"]
JA_TOPICS = {"war": "米國と日本の戰爭", "election": "選擧のニュース"}
JA_PARTICLES = ["の", "は", "に", "を", "と", "で", "から"]
# usnm_core::ja's folding, for the characters above only; a Rust test
# (crates/usnm-search/tests/ja_fixture.rs) checks every document against it.
JA_FOLD = {"國": "国", "戰": "戦", "爭": "争", "擧": "挙", "學": "学", "收": "収",
           "ュ": "ユ", "ッ": "ツ", "っ": "つ"}


def is_ja(c):
    o = ord(c)
    return 0x3041 <= o <= 0x30FF or 0x4E00 <= o <= 0x9FFF


def ja_index_text(printed):
    tokens, latin = [], ""
    for c in printed + " ":
        if c.isascii() and c.isalnum():
            latin += c
            continue
        if latin:
            tokens.append(latin.lower())
            latin = ""
        if is_ja(c):
            tokens.append(JA_FOLD.get(c, c))
    return " ".join(tokens)


def ja_page_text(rng, topic):
    lines = []
    for i in range(4):
        words = rng.sample(JA_WORDS, 5)
        line = "".join(w + rng.choice(JA_PARTICLES) for w in words)
        if topic and i == 1:
            line += JA_TOPICS[topic]
        lines.append(line + "。")
    if rng.random() < 0.3:
        lines.append("Denver")
    return "\n".join(lines)


def ja_docs():
    """The Japanese fixture pages, from their own random stream."""
    rng = random.Random(901)
    lccn, _, pid, state, ordinal = JA_TITLE
    place_ordinal = next(p[1] for p in PLACES if p[0] == pid)
    docs, d = [], date(1896, 1, 4)
    while d <= date(1897, 12, 25):
        topic = "war" if date(1896, 7, 1) <= d <= date(1896, 9, 30) else (
            "election" if date(1896, 10, 1) <= d <= date(1896, 11, 30) else None)
        printed = ja_page_text(rng, topic)
        docs.append({
            "doc_id": f"{lccn}_{d.isoformat()}_ed-1_seq-1",
            "day": day_number(d), "ym": d.year * 12 + d.month - 1, "year": d.year,
            "place_id": pid, "place_shard": place_ordinal % 8, "lccn": lccn, "state": state,
            "language": ["eng", "jpn"], "front_page": True, "edition": 1, "seq": 1,
            "sort_key": sort_key(ordinal, 1, 1), "date": d.isoformat(),
            "text": ja_index_text(printed), "printed": printed, "ocr_source": "usnm-ndlocr-lite",
            "ocr_engine": "ndlocr-lite 636d1cf",
        })
        d += timedelta(days=14)
    return docs


# Congress fixture pages (#229): one invented page stream per series, in
# its own era, all at the reserved place PC001 (Washington, DC). Titles are
# labelled "Fixture" and links go to example.org: none of this is real.
CONGRESS_SERIES = ["annals", "register", "globe", "serialset"]  # same-day hit order
CONGRESS_STREAMS = [
    # series, first day, last day, step in days, pages a day, congress, session, topic words
    ("annals", date(1820, 2, 1), date(1820, 3, 31), 3, 1, 16, "1",
     "the senate debated the admission of missouri and the line of the compromise"),
    ("register", date(1832, 12, 10), date(1833, 3, 2), 4, 1, 22, "2",
     "the house heard the ordinance of nullification and the tariff of duties"),
    ("globe", date(1850, 1, 7), date(1850, 9, 30), 7, 2, 31, "1",
     "the senate took up the fugitive slave bill and the admission of california"),
    ("serialset", date(1896, 2, 3), date(1897, 11, 29), 28, 3, 54, "1",
     "the report on free silver and the gold standard was ordered printed"),
]
CONGRESS_FILLER = (
    "mr fixture of the committee moved that the further consideration of the "
    "resolution be postponed and the yeas and nays were ordered on the motion"
).split()
CONGRESS_STATES = ["MO", "SC", "CA", "IL", "NY", "GA"]


def ordinal(n):
    """1st, 2nd, 3rd, 4th, 11th, 12th, 13th, 21st, 22nd, …"""
    suffix = "th" if 11 <= n % 100 <= 13 else {1: "st", 2: "nd", 3: "rd"}.get(n % 10, "th")
    return f"{n}{suffix}"


def congress_docs():
    """The Congress fixture pages, from their own random stream."""
    rng = random.Random(229)
    docs = []
    for series, first, last, step, per_day, congress, session, topic in CONGRESS_STREAMS:
        d, unit = first, 0
        while d <= last:
            unit += 1
            for seq in range(1, per_day + 1):
                words = [rng.choice(CONGRESS_FILLER) for _ in range(30)]
                if rng.random() < 0.6:
                    at = rng.randrange(len(words))
                    words[at:at] = topic.split()
                text = " ".join(words)
                chamber = rng.choice(["senate", "house"])
                state = rng.choice(CONGRESS_STATES)
                doc_id = f"fixture-{series}-{unit:04d}_p{seq}"
                docs.append({
                    "doc_id": doc_id,
                    "day": day_number(d), "ym": d.year * 12 + d.month - 1, "year": d.year,
                    "date": d.isoformat(),
                    "place_id": "PC001", "place_shard": 0, "lccn": "", "state": "DC",
                    "language": ["eng"], "front_page": False, "edition": 1, "seq": seq,
                    "sort_key": (CONGRESS_SERIES.index(series) << 56) | (unit << 24) | seq,
                    "text": text,
                    "series": series,
                    "title": f"Fixture {series.title()}, {ordinal(congress)} Congress, session {session}",
                    "citation": f"Fixture {series} {congress}-{session} p. {unit * 10 + seq}",
                    "congress": congress, "session": session, "chamber": chamber,
                    "printed_page": str(unit * 10 + seq),
                    "package_id": f"fixture-{series}-{unit:04d}",
                    "link": f"https://example.org/fixture/congress/{series}/{unit:04d}#page={seq}",
                    "ocr_source": "fixture",
                    "states": [state], "state_via": [f"{state}:title"], "speakers": [],
                })
            d += timedelta(days=step)
    return docs


def day_number(d):
    return (d - EPOCH).days


def page_text(rng, topics):
    words = []
    for _ in range(4):
        words += rng.sample(FILLER, 12)
        if topics:
            words += TOPICS[topics.pop(0)].split()
    words += rng.sample(FILLER, 10)
    return " ".join(words).capitalize() + "."


# A market report on some of one place's pages, in LoC's text only and
# without drawing from either random stream, so no other text changes. Its
# fractions are each one word in the index (`½`, `61¼`), and `1/2` is two
# (#168); the two forms are on different pages. The circled ideographs are
# kept as they are, and `B㊁` is one word, not `b` and `二` (#241).
#
# In June, a garbled telegraph item with two OCR runs (#286): one of 254
# bytes (130 characters, `é` being two bytes), which Quickwit's `usnm_text`
# keeps as a word of `text`, and one of 255 bytes, which its `remove_long`
# drops. Both keep their positions, and the one searched field (`text_all`)
# drops both, as it drops every word over 40 characters.
#
# In July, a grain report whose three grains are two words apart (#243): a
# NEAR search for them with two words of slop is counted and not listed by
# relevance, as Quickwit's phrase scorer has it, and swapped they are found
# with enough slop.
LONG_RUN_KEPT = "Qzxwy" + "é" * 124 + "x"
LONG_RUN_DROPPED = "Vyxwz" + "é" * 125
MARKET = {3: "Wheat closed ½ higher at 61¼ and oats steady.",
          4: "Oats closed 1/2 lower at 20¾ and wheat steady.",
          5: "Auction lots ㊀ and B㊁ withdrawn.",
          6: f"Cable garbled {LONG_RUN_KEPT} wharfage tolls; signal {LONG_RUN_DROPPED} semaphore lamps.",
          7: "Corn held firm, barley was quiet, rye dull."}
assert len(LONG_RUN_KEPT.encode()) == 254 and len(LONG_RUN_DROPPED.encode()) == 255


def market(pid, d, seq):
    """The market report on this page, or None."""
    if pid == "P00001" and seq == 2 and d.year == 1896:
        return MARKET.get(d.month)
    return None


# American Stories' second OCR of a page (05 §5.5.4, #218), from its own
# random stream so LoC's text above doesn't change with it. It covers some
# pages; on those it reads headlines LoC's text lacks (so their words match
# only there), fixes some of LoC's misread words, and misreads others itself.
AS_HEADLINES = {
    "gold": ["BRYAN AT CHICAGO", "THE BOY ORATOR OF THE PLATTE"],
    "silver": ["BIMETALLISM DEBATED"],
    "standard": ["BIMETALLISM DEBATED"],
    "fever": ["QUARANTINE AT THE PORT"],
    None: ["LOCAL NEWS"],
}
# Words LoC's OCR misread on a covered page, which American Stories reads right.
LOC_MISREAD = {"gold": "goid", "silver": "sllver", "convention": "conventlon",
               "orator": "orat0r", "fever": "fevcr", "standard": "standarcl"}
# Words American Stories misread where LoC's text has them right.
AS_MISREAD = {"gold": "golcl", "silver": "silvcr", "convention": "convent1on",
              "orator": "oratar", "fever": "faver", "cotton": "cottan"}


def misread(text, table, rng):
    """`text` with one word in `table` replaced by its misreading, if it has one."""
    words = text.split(" ")
    spots = [i for i, w in enumerate(words) if w.lower().rstrip(".") in table]
    if not spots:
        return text
    i = rng.choice(spots)
    w = words[i]
    bare = w.rstrip(".")
    wrong = table[bare.lower()]
    words[i] = (wrong.capitalize() if bare[0].isupper() else wrong) + w[len(bare):]
    return " ".join(words)


def american_stories(rng, text, topics):
    """(LoC's text, American Stories' text or None) for a page with LoC's `text`
    (empty when LoC has none) and the topics it was written from."""
    if not text:
        # A page LoC has no text for: American Stories has some for half of them.
        if rng.random() < 0.5:
            return text, page_text(rng, list(topics))
        return text, None
    if rng.random() >= 0.4:
        return text, None
    text_as = text
    if rng.random() < 0.5:
        heads = AS_HEADLINES[topics[0] if topics else None]
        text_as = rng.choice(heads) + "\n" + text_as
    roll = rng.random()
    if roll < 0.25:
        text = misread(text, LOC_MISREAD, rng)
    elif roll < 0.45:
        text_as = misread(text_as, AS_MISREAD, rng)
    return text, text_as


def sort_key(title_ordinal, edition, seq):
    """Numeric same-day tiebreak: Quickwit 0.9 can't sort on text fields."""
    return (title_ordinal << 32) | (edition << 16) | seq


def main():
    rng = random.Random(1896)
    as_rng = random.Random(1865)
    OUT.mkdir(parents=True, exist_ok=True)
    titles, docs_base, docs_delta, baselines, title_pages, by_language = [], [], [], {}, {}, {}
    for pid, ordinal, name, state, *_ in PLACES:
        lccn = f"sn99{ordinal:06d}"
        languages = LANGUAGES.get(pid, ["eng"])
        # Years of publication as geocode derives them from LoC's `dates`
        # (04 §4.6); the last paper's record is open-ended ("1896-current").
        first_year = 1890 + ordinal
        dates = f"{first_year}-current" if ordinal == len(PLACES) else f"{first_year}-1897"
        title = {
            "lccn": lccn, "name": f"The Fixture Gazette {ordinal}", "ordinal": ordinal, "place_id": pid,
            "state": state, "languages": languages, "dates": dates, "first_year": first_year,
        }
        if ordinal != len(PLACES):
            title["last_year"] = 1897
        titles.append(title)
        counts = {}
        d = date(1895, 1, 5)
        while d <= date(1897, 12, 31):
            for seq in (1, 2):
                topics = []
                if date(1896, 7, 9) + timedelta(days=SPREAD[pid]) <= d <= date(1896, 12, 31) and rng.random() < 0.8:
                    topics.append("gold")
                if date(1896, 1, 1) <= d <= date(1896, 11, 30) and rng.random() < 0.3:
                    topics.append("silver")
                if rng.random() < 0.1:
                    topics.append("standard")
                if state in ("GA", "SC") and date(1897, 8, 1) <= d <= date(1897, 10, 31) and rng.random() < 0.6:
                    topics.append("fever")
                written = list(topics)
                text = "" if rng.random() < 0.02 else page_text(rng, topics)
                text, text_as = american_stories(as_rng, text, written)
                report = market(pid, d, seq)
                if text and report:
                    text += " " + report
                dn = day_number(d)
                counts[dn] = counts.get(dn, 0) + 1
                if not text and not text_as:
                    continue  # no text at all: counted in baselines, excluded from the index
                doc = {
                    "doc_id": f"{lccn}_{d.isoformat()}_ed-1_seq-{seq}",
                    "day": dn, "ym": d.year * 12 + d.month - 1, "year": d.year,
                    "place_id": pid, "place_shard": ordinal % 8, "lccn": lccn,
                    "state": state, "language": languages, "front_page": seq == 1,
                    "edition": 1, "seq": seq,
                    # Hits order within a day: title, then edition, then page (05 §5.5).
                    "sort_key": sort_key(ordinal, 1, seq),
                    # Quickwit's timestamp field; the API itself uses `day`.
                    "date": d.isoformat(),
                    "text": text,
                }
                if text_as is not None:
                    doc["text_as"] = text_as
                (docs_delta if d >= date(1897, 7, 1) else docs_base).append(doc)
            d += timedelta(days=7)
        baselines[pid] = sorted(counts.items())
        by_language.setdefault(tuple(sorted(set(languages))), {})[pid] = sorted(counts.items())
        title_pages[lccn] = sum(counts.values())

    snap = OUT / VERSION
    snap.mkdir(exist_ok=True)
    (OUT / "indexes").mkdir(exist_ok=True)
    for index_id, docs in (("pages-base-fixture", docs_base), ("pages-delta-fixture-1", docs_delta),
                           ("pages-ja-fixture", ja_docs()), ("congress-fixture", congress_docs())):
        with open(OUT / "indexes" / f"{index_id}.jsonl", "w") as f:
            for doc in docs:
                f.write(json.dumps(doc) + "\n")
    places = [
        {"id": p[0], "ordinal": p[1], "name": p[2], "state": p[3], "lat": p[4], "lon": p[5], "precision": p[6]}
        for p in PLACES
    ]
    (snap / "places.json").write_text(json.dumps(places, indent=1) + "\n")
    (snap / "titles.json").write_text(json.dumps(titles, indent=1) + "\n")
    (snap / "baselines.json").write_text(json.dumps(baselines) + "\n")
    # The same pages split by the languages their title lists (one entry per
    # distinct set, sets in sorted order), for exact baselines under `lang`.
    language_baselines = {"sets": [
        {"languages": list(langs), "baselines": by_language[langs]} for langs in sorted(by_language)
    ]}
    (snap / "language_baselines.json").write_text(json.dumps(language_baselines) + "\n")
    # Pages per title (the status page's pages by language), keys sorted.
    (snap / "title_pages.json").write_text(json.dumps(title_pages, indent=1, sort_keys=True) + "\n")
    # The API verifies every reference file against this manifest (04 §4.3).
    files = []
    for name in ("baselines.json", "language_baselines.json", "places.json", "title_pages.json", "titles.json"):
        data = (snap / name).read_bytes()
        files.append({"path": name, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)})
    manifest = {"index_version": VERSION, "files": files}
    (snap / "manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")
    current = {
        "index_version": VERSION,
        "backend": "memory",
        "indexes": ["pages-base-fixture", "pages-delta-fixture-1"],
        "reference": VERSION,
        "bounds": {"from": "1895-01-01", "to": "1897-12-31"},
        "published_at": "2026-09-27T00:00:00Z",
        "synthetic": True,
    }
    (OUT / "current.json").write_text(json.dumps(current, indent=1) + "\n")
    print(f"{len(docs_base)} base + {len(docs_delta)} delta docs, {len(titles)} titles")


if __name__ == "__main__":
    main()
