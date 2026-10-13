#!/usr/bin/env python3
"""The OCR gold set: select pages, cut crops, validate transcriptions, score CER (#296).

Design and rules are in docs/ocr-gold-set.md. Every option is a named flag.

    gold.py select   --scores docs/reports/2026-10-ocr-engine-survey/data/scores.jsonl --out selection.jsonl
    gold.py select   --manifest SAMPLE/manifest-final.jsonl --out selection.jsonl [--target 60] [--seed 296]
    gold.py cut      --selection selection.jsonl --sample SAMPLE --out DIR [--work WORK]
    gold.py fetch    --manifest DIR/manifest.jsonl --out DIR/images
    gold.py validate --crops DIR/crops.jsonl [--manifest DIR/manifest.jsonl --images DIR/images]
    gold.py score    --crops DIR/crops.jsonl [--hyp ENGINE=FILE.jsonl ...] [--hints] [--out DIR]
    gold.py agree    --crops DIR/crops.jsonl [--out DIR]
    gold.py public-fetch   --dataset NAME --src DIR --confirm-download [--pages N]
    gold.py public-convert --dataset NAME --src DIR --out DIR [--count N] [--per-page N] [--seed N] [--not-for-scoring]

select  Seeded stratified choice of pages, one crop per page: the target is shared equally between the
        survey's groups (a group with fewer pages gives all of them and the rest goes to the others),
        and inside a group the picks cycle through the decades in a seeded order. With --scores the
        page's date, LCCN, edition and sequence come from its page_id; --manifest also carries the
        image and ALTO URLs and the image checksum, which `cut` and `fetch` need.
cut     Cuts one crop per selected page (the box is crops.pick_box on LoC's ALTO, seeded per page),
        writes the manifest (URLs, checksums, box) and a transcription template, crops.jsonl, with
        `transcription` null. Each engine's text for the box goes in `hints`, marked as not truth.
fetch   Re-creates the crops from the manifest: downloads each page image (paced as locfetch does),
        checks its sha256, cuts the box.
validate  Checks a filled-in crops.jsonl. Exit status 1 on any error.
score   CER and WER of hypotheses against the transcriptions (normalization: see NORMALIZATION).
        Hypotheses are `--hyp ENGINE=FILE.jsonl` (rows {"crop_id", "text"}), and/or `--hints`, which uses
        the engines' texts stored in crops.jsonl. Crops that fail validation are left out and counted.
agree   For crops with no transcription: the engines' texts for the same box compared with each other
        (pairwise CER, and each engine's mean CER against the others). This is agreement, not accuracy:
        two engines can agree and both be wrong.
public-fetch    Downloads one public ground-truth dataset (docs/ocr-gold-set.md, "Public ground truth") into
        --src. Some are gigabytes, so --confirm-download is required; run it where the data is to be kept.
public-convert  Turns the downloaded dataset into the layout `score` reads: images/ (crops), manifest.jsonl
        and crops.jsonl, with the dataset's own human transcription filled in. NAME is one of
        reichsanzeiger or austrian, the two that meet the source policy (docs/ocr-gold-set.md). The
        readers for americanstories and ndl-pdm stay for comparison only: they need --not-for-scoring,
        and their rows carry for_scoring false.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import io
import json
import random
import re
import sys
import unicodedata
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

PAGE_ID = re.compile(r"^(?P<lccn>[^_]+)_(?P<date>\d{4}-\d{2}-\d{2})_ed-?(?P<ed>\d+)_seq-?(?P<seq>\d+)$")
DEFAULT_TARGET = 60
DEFAULT_SEED = 296
HINT_NOTE = "HINT, NOT TRUTH: each engine's current text for this box. Do not copy it; read the image."
UNKNOWN = "[?]"
SKIPPED = "[...]"  # not allowed: validation rejects crops that skip printed text
GUESS = re.compile(r"\[\?([^\[\]\n]+)\]")
MAX_UNKNOWN_SHARE = 0.02


def read_jsonl(p: Path) -> list[dict]:
    return [json.loads(x) for x in p.read_text(encoding="utf-8").splitlines() if x.strip()]


def write_jsonl(p: Path, rows: list[dict]) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows), encoding="utf-8")


# ---------------------------------------------------------------- select


def parse_page_id(pid: str) -> dict:
    m = PAGE_ID.match(pid)
    if not m:
        raise ValueError(f"unexpected page_id {pid!r}")
    return {"lccn": m["lccn"], "date": m["date"], "edition": int(m["ed"]), "seq": int(m["seq"])}


def loc_page_url(r: dict) -> str:
    return f"https://www.loc.gov/resource/{r['lccn']}/{r['date']}/ed-{r['edition']}/?sp={r['seq']}"


def sample_pages(scores: Path | None, manifest: Path | None) -> list[dict]:
    """One row per survey page: page_id, group, lang, lccn, date, edition, seq (+ URLs and checksum with a manifest)."""
    pages: dict[str, dict] = {}
    if manifest:
        for r in read_jsonl(manifest):
            if r.get("in_sample", True):
                pages[r["page_id"]] = {**parse_page_id(r["page_id"]), **{k: r.get(k) for k in (
                    "page_id", "group", "lang", "title", "source_label", "image_url", "image_sha256", "alto_url")}}
    else:
        for r in read_jsonl(scores):
            pid = r["page_id"]
            pages.setdefault(pid, {**parse_page_id(pid), "page_id": pid, "group": r["group"], "lang": r["lang"]})
    return sorted(pages.values(), key=lambda p: p["page_id"])


def quotas(sizes: dict[str, int], target: int) -> dict[str, int]:
    """Equal shares; a group smaller than its share gives all its pages and the rest is shared by the others."""
    left, todo, out = target, sorted(sizes), {}
    while todo:
        share = left // len(todo)
        small = [g for g in todo if sizes[g] <= share]
        if not small:
            extra = left - share * len(todo)
            for i, g in enumerate(todo):  # groups in name order take the remainder
                out[g] = share + (1 if i < extra else 0)
            break
        for g in small:
            out[g] = sizes[g]
            left -= sizes[g]
            todo.remove(g)
    return out


def decade(date: str) -> str:
    return f"{int(date[:4]) // 10 * 10}s"


def select(pages: list[dict], target: int, seed: int) -> list[dict]:
    by_group: dict[str, list[dict]] = {}
    for p in pages:
        by_group.setdefault(p["group"], []).append(p)
    q = quotas({g: len(ps) for g, ps in by_group.items()}, target)
    out = []
    for g in sorted(by_group):
        rnd = random.Random(f"{seed}:gold:{g}")
        eras: dict[str, list[dict]] = {}
        for p in by_group[g]:
            eras.setdefault(decade(p["date"]), []).append(p)
        for ps in eras.values():
            ps.sort(key=lambda p: p["page_id"])
            rnd.shuffle(ps)
        order = sorted(eras)
        rnd.shuffle(order)
        picked: list[dict] = []
        while len(picked) < q[g]:  # cycle through the eras, one page each time round
            for e in order:
                if eras[e] and len(picked) < q[g]:
                    picked.append(eras[e].pop())
        for p in sorted(picked, key=lambda p: p["page_id"]):
            out.append({**p, "era": decade(p["date"]), "stratum": f"{g}/{decade(p['date'])}"})
    return out


def cmd_select(a: argparse.Namespace) -> int:
    if bool(a.scores) == bool(a.manifest):
        print("give exactly one of --scores and --manifest", file=sys.stderr)
        return 2
    rows = select(sample_pages(a.scores, a.manifest), a.target, a.seed)
    write_jsonl(a.out, rows)
    tally: dict[str, int] = {}
    for r in rows:
        tally[r["group"]] = tally.get(r["group"], 0) + 1
    print(f"{len(rows)} pages selected (seed {a.seed}, target {a.target}): {tally}")
    return 0


# ---------------------------------------------------------------- cut and fetch


def sha256(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def encode_crop(img, box) -> bytes:
    import io

    buf = io.BytesIO()
    img.crop(tuple(box)).convert("L").save(buf, format="JPEG", quality=85)
    return buf.getvalue()


def cmd_cut(a: argparse.Namespace) -> int:
    import PIL
    from PIL import Image

    import crops

    Image.MAX_IMAGE_PIXELS = None
    sel = read_jsonl(a.selection)
    sample = {r["page_id"]: r for r in read_jsonl(a.sample / "manifest-final.jsonl")}
    results = []
    if a.work:
        for res in sorted((a.work / "results").glob("*.jsonl")):
            if not res.stem.startswith("throughput"):  # aggregate rows, no page_id (as score.py skips them)
                results += read_jsonl(res)
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "images").mkdir(exist_ok=True)
    manifest, template, counters = [], [], {}
    for s in sel:
        pid = s["page_id"]
        sm = sample.get(pid)
        if sm is None:
            print(f"skipped, not in the sample: {pid}", file=sys.stderr)
            continue
        page_bytes = (a.sample / "images" / f"{pid}.jpg").read_bytes()
        if sm.get("image_sha256") and sha256(page_bytes) != sm["image_sha256"]:
            print(f"skipped, local page image doesn't match the sample checksum: {pid}", file=sys.stderr)
            continue
        img = Image.open(io.BytesIO(page_bytes))
        loc = crops.alto_words(a.sample / "alto" / f"{pid}.xml", img.width)
        box = crops.pick_box(loc, img.size, random.Random(f"{a.seed}:gold-box:{pid}"))
        if box is None:
            print(f"skipped, no text region of 600+ pixels: {pid}", file=sys.stderr)
            continue
        counters[s["group"]] = counters.get(s["group"], 0) + 1
        crop_id = f"{s['group']}-{counters[s['group']]:02d}"
        data = encode_crop(img, box)
        (a.out / "images" / f"{crop_id}.jpg").write_bytes(data)
        row = {
            "crop_id": crop_id, "page_id": pid, "group": s["group"], "lang": s["lang"], "era": s["era"],
            "stratum": s["stratum"], "lccn": s["lccn"], "date": s["date"], "edition": s["edition"], "seq": s["seq"],
            "title": sm.get("title"), "box": list(box), "image": f"images/{crop_id}.jpg",
            "crop_sha256": sha256(data), "crop_bytes": len(data), "encoder": f"Pillow {PIL.__version__}",
            "page_url": loc_page_url(s), "page_image_url": sm["image_url"],
            "page_image_sha256": sm.get("image_sha256") or sha256(page_bytes), "alto_url": sm["alto_url"],
        }
        manifest.append(row)
        texts = crops.engine_texts(a.work, results, pid, loc, box, img.width)
        template.append({
            "crop_id": crop_id, "group": s["group"], "lang": s["lang"], "date": s["date"], "title": sm.get("title"),
            "image": row["image"],
            "hints": {"is_truth": False, "note": HINT_NOTE, "texts": dict(sorted(texts.items()))},
            "transcription": None, "transcriber": None, "reviewer": None, "hint_copy_reviewed": False,
            "notes": "",
        })
    write_jsonl(a.out / "manifest.jsonl", manifest)
    write_jsonl(a.out / "crops.jsonl", template)
    total = sum(r["crop_bytes"] for r in manifest)
    print(f"{len(manifest)} crops, {total / 1e6:.1f} MB "
          f"({'under' if total < 10e6 else 'over'} the 10 MB limit for committing the images)")
    return 0


def cmd_fetch(a: argparse.Namespace) -> int:
    import PIL
    from PIL import Image

    import locfetch

    Image.MAX_IMAGE_PIXELS = None
    import io

    a.out.mkdir(parents=True, exist_ok=True)
    bad = 0
    pages: dict[str, bytes] = {}
    for r in read_jsonl(a.manifest):
        dest = a.out / Path(r["image"]).name
        if dest.exists() and sha256(dest.read_bytes()) == r["crop_sha256"]:
            continue
        data = pages.get(r["page_id"]) or locfetch.fetch(r["page_image_url"], locfetch.TILE)
        pages = {r["page_id"]: data}  # one page at a time
        if sha256(data) != r["page_image_sha256"]:
            print(f"page image checksum differs: {r['page_id']} (LoC changed the file?)", file=sys.stderr)
            bad += 1
            continue
        crop = encode_crop(Image.open(io.BytesIO(data)), r["box"])
        dest.write_bytes(crop)
        if sha256(crop) != r["crop_sha256"]:
            # JPEG bytes can differ between Pillow versions; the page checksum above is what pins the pixels.
            print(f"crop bytes differ from the manifest's: {r['crop_id']} (encoder {PIL.__version__}, "
                  f"manifest {r.get('encoder')})", file=sys.stderr)
    return 1 if bad else 0


# ---------------------------------------------------------------- normalization and CER

# NORMALIZATION, applied to the transcription and to every hypothesis before comparing:
#  1. Unicode NFC. Nothing else is folded: case, punctuation, diacritics, long s and ligatures all count.
#  2. A hyphen at a line's end followed by a line break is removed and the line joined (the survey's
#     text does the same, altotext.py), so a word broken across lines counts as one word.
#  3. Line breaks and runs of whitespace become one space; the ends are stripped.
# In the transcription only: `[?x]` (a guessed character or characters) counts as x, and `[?]` (an
# unreadable character) is dropped. Crops with more than MAX_UNKNOWN_SHARE unknown characters fail validation.


def normalize(text: str, truth: bool = False) -> str:
    t = unicodedata.normalize("NFC", text)
    if truth:
        t = GUESS.sub(lambda m: m.group(1), t).replace(UNKNOWN, "")
    t = re.sub(r"-[ \t]*\r?\n[ \t]*", "", t)
    return re.sub(r"\s+", " ", t).strip()


def edit_distance(a: list | str, b: list | str) -> int:
    if len(a) < len(b):
        a, b = b, a
    prev = list(range(len(b) + 1))
    for i, x in enumerate(a, 1):
        cur = [i]
        for j, y in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (x != y)))
        prev = cur
    return prev[-1]


def cer_wer(truth: str, hyp: str) -> dict:
    t, h = normalize(truth, truth=True), normalize(hyp)
    tw, hw = t.split(), h.split()
    return {"ref_chars": len(t), "char_edits": edit_distance(t, h), "ref_words": len(tw),
            "word_edits": edit_distance(tw, hw)}


# ---------------------------------------------------------------- validate


def unknown_share(text: str) -> float:
    """Share of the resolved text's characters that are unknown or guessed; [?abc] counts 3."""
    guessed = sum(len(g) for g in GUESS.findall(text))
    unknown = GUESS.sub("", text).count(UNKNOWN)
    resolved = GUESS.sub(lambda m: m.group(1), text).replace(UNKNOWN, "?").replace("\n", "")
    return (unknown + guessed) / (len(resolved) or 1)


def validate_rows(rows: list[dict], manifest: list[dict] | None = None, images: Path | None = None,
                  require_review: bool = True) -> tuple[list[str], list[str]]:
    errors, warnings = [], []
    seen: set[str] = set()
    by_id = {m["crop_id"]: m for m in manifest or []}
    for r in rows:
        cid = r.get("crop_id", "?")
        if cid in seen:
            errors.append(f"{cid}: duplicate crop_id")
        seen.add(cid)
        t = r.get("transcription")
        if not isinstance(t, str) or not t.strip():
            errors.append(f"{cid}: transcription is empty")
            continue
        if not r.get("transcriber"):
            errors.append(f"{cid}: no transcriber")
        if require_review and (not r.get("reviewer") or r.get("reviewer") == r.get("transcriber")):
            errors.append(f"{cid}: needs a second-pass reviewer other than the transcriber")
        stripped = GUESS.sub("", t).replace(UNKNOWN, "")
        if "[?" in stripped:
            errors.append(f"{cid}: unbalanced uncertain-character marker")
        if SKIPPED in t:
            errors.append(f"{cid}: skipped-span marker {SKIPPED}; the scorer can't align around it, drop the crop")
        if unknown_share(t) > MAX_UNKNOWN_SHARE:
            errors.append(f"{cid}: more than {MAX_UNKNOWN_SHARE:.0%} unknown characters; drop the crop")
        nt = normalize(t, truth=True)
        for engine, text in sorted(((r.get("hints") or {}).get("texts") or {}).items()):
            if text and normalize(text) == nt and not r.get("hint_copy_reviewed"):
                errors.append(f"{cid}: transcription equals {engine!r} hint verbatim; set hint_copy_reviewed "
                              "after checking the image, or re-read it")
            elif text and len(nt) >= 30 and not r.get("hint_copy_reviewed") \
                    and edit_distance(nt, normalize(text)) / len(nt) < 0.01:
                warnings.append(f"{cid}: within 1% of {engine!r} hint; check it was read from the image")
        m = by_id.get(cid)
        if manifest is not None and m is None:
            errors.append(f"{cid}: not in the manifest")
        if m is not None and r.get("image") is not None and r["image"] != m["image"]:
            errors.append(f"{cid}: image {r['image']!r} differs from the manifest's {m['image']!r}")
        if m is not None and images is not None:
            p = images / Path(m["image"]).name
            if not p.exists():
                errors.append(f"{cid}: image missing ({p.name})")
            elif sha256(p.read_bytes()) != m["crop_sha256"]:
                warnings.append(f"{cid}: image bytes differ from the manifest's checksum")
    if manifest is not None:
        for cid in sorted(set(by_id) - seen):
            errors.append(f"{cid}: in the manifest, not in crops.jsonl")
    return errors, warnings


def cmd_validate(a: argparse.Namespace) -> int:
    manifest = read_jsonl(a.manifest) if a.manifest else None
    errors, warnings = validate_rows(read_jsonl(a.crops), manifest, a.images, not a.allow_unreviewed)
    for w in warnings:
        print(f"warning: {w}")
    for e in errors:
        print(f"error: {e}")
    print(f"{len(errors)} errors, {len(warnings)} warnings")
    return 1 if errors else 0


# ---------------------------------------------------------------- score


def cmd_score(a: argparse.Namespace) -> int:
    rows = read_jsonl(a.crops)
    errors, _ = validate_rows(rows, require_review=not a.allow_unreviewed)
    bad = {e.split(":", 1)[0] for e in errors}
    hyps: dict[str, dict[str, str]] = {}
    for spec in a.hyp:
        name, _, path = spec.partition("=")
        if not path:
            print(f"--hyp wants ENGINE=FILE.jsonl, got {spec!r}", file=sys.stderr)
            return 2
        hyps[name] = {r["crop_id"]: r["text"] for r in read_jsonl(Path(path))}
    if a.hints:
        for r in rows:
            for engine, text in ((r.get("hints") or {}).get("texts") or {}).items():
                hyps.setdefault(engine, {})[r["crop_id"]] = text
    used = [r for r in rows if r["crop_id"] not in bad]
    print(f"{len(used)} of {len(rows)} crops usable; {len(rows) - len(used)} left out (failed validation)",
          file=sys.stderr)
    out_rows = []
    for engine in sorted(hyps):
        for r in used:
            if r["crop_id"] in hyps[engine]:
                out_rows.append({"engine": engine, "crop_id": r["crop_id"], "group": r["group"],
                                 **cer_wer(r["transcription"], hyps[engine][r["crop_id"]])})
    summary = []
    for engine in sorted(hyps):
        for group in [None, *sorted({r["group"] for r in used})]:
            rs = [x for x in out_rows if x["engine"] == engine and (group is None or x["group"] == group)]
            if not rs:
                continue
            c, w = sum(x["ref_chars"] for x in rs), sum(x["ref_words"] for x in rs)
            summary.append({"engine": engine, "group": group or "all", "crops": len(rs), "ref_chars": c,
                            "cer": round(sum(x["char_edits"] for x in rs) / c, 4) if c else None,
                            "wer": round(sum(x["word_edits"] for x in rs) / w, 4) if w else None})
    if a.out:
        a.out.mkdir(parents=True, exist_ok=True)
        for name, data in (("cer-by-crop.csv", out_rows), ("cer-summary.csv", summary)):
            if data:
                with (a.out / name).open("w", newline="") as f:
                    w = csv.DictWriter(f, fieldnames=list(data[0]))
                    w.writeheader()
                    w.writerows(data)
    print("| engine | group | crops | reference chars | CER | WER |\n|---|---|---|---|---|---|")
    for s in summary:
        print(f"| {s['engine']} | {s['group']} | {s['crops']} | {s['ref_chars']} | {s['cer']} | {s['wer']} |")
    return 0


# ---------------------------------------------------------------- agreement (untranscribed crops)


def agreement(rows: list[dict]) -> tuple[list[dict], list[dict]]:
    """Pairwise CER between engines' hint texts per crop, and each engine's mean against the others.

    Agreement is not accuracy. The CER of A against B is edits over B's length, so it is reported both ways.
    """
    pairs: list[dict] = []
    for r in rows:
        texts = {e: t for e, t in sorted(((r.get("hints") or {}).get("texts") or {}).items()) if t and t.strip()}
        for a in texts:
            for b in texts:
                if a != b:
                    x = cer_wer(texts[b], texts[a])
                    pairs.append({"crop_id": r["crop_id"], "group": r.get("group"), "engine": a, "other": b,
                                  "ref_chars": x["ref_chars"], "char_edits": x["char_edits"]})
    summary = []
    for engine in sorted({p["engine"] for p in pairs}):
        for group in [None, *sorted({p["group"] for p in pairs})]:
            ps = [p for p in pairs if p["engine"] == engine and (group is None or p["group"] == group)]
            c = sum(p["ref_chars"] for p in ps)
            if ps and c:
                summary.append({"engine": engine, "group": group or "all", "pairs": len(ps),
                                "disagreement_cer": round(sum(p["char_edits"] for p in ps) / c, 4)})
    return pairs, summary


def cmd_agree(a: argparse.Namespace) -> int:
    pairs, summary = agreement(read_jsonl(a.crops))
    if a.out:
        a.out.mkdir(parents=True, exist_ok=True)
        for name, data in (("agreement-by-pair.csv", pairs), ("agreement-summary.csv", summary)):
            if data:
                with (a.out / name).open("w", newline="") as f:
                    w = csv.DictWriter(f, fieldnames=list(data[0]))
                    w.writeheader()
                    w.writerows(data)
    print("AGREEMENT BETWEEN ENGINES, NOT ACCURACY: no transcription is used. Lower means the engine's text is "
          "closer to the others', not closer to the page.")
    print("| engine | group | pairs | mean CER against the other engines |\n|---|---|---|---|")
    for s in summary:
        print(f"| {s['engine']} | {s['group']} | {s['pairs']} | {s['disagreement_cer']} |")
    return 0


# ---------------------------------------------------------------- public ground truth

HF_AS_TRAIN = "https://huggingface.co/datasets/dell-research-harvard/AmericanStoriesTraining/resolve/main/"
ZENODO_FILE = "https://zenodo.org/api/records/{rec}/files/{name}/content"
# The Mannheim image server, as the dataset's own download_images.sh builds it (imageurls.list holds the rest).
REICHSANZEIGER_IMAGES = "https://digi.bib.uni-mannheim.de/reichsanzeiger.fcgi?FIF=/reichsanzeiger/film/"
NDL_PDM_JSON = "https://lab.ndl.go.jp/dataset/pdm_ocr_dataset/line/tosho_all_linejson.zip"

# What each source says about itself, as read on 2026-10-13 (docs/ocr-gold-set.md has the sources and caveats).
PUBLIC: dict[str, dict] = {
    "americanstories": {
        "group": "pub-americanstories", "lang": "eng",
        "license": "Apache-2.0 (Hugging Face repository metadata)",
        "url": "https://huggingface.co/datasets/dell-research-harvard/AmericanStoriesTraining",
        "chars": (200, 2500), "scoring": False,
        "policy": "fails: card empty, source and years of the 10 pages undocumented",
    },
    "reichsanzeiger": {
        "group": "pub-reichsanzeiger", "lang": "deu",
        "license": "CC0-1.0 (GitHub LICENSE file); CC-BY-4.0 (Zenodo record); images Public Domain Mark 1.0",
        "url": "https://doi.org/10.5281/zenodo.10144094",
        "chars": (200, 2500), "scoring": True, "policy": "passes",
    },
    "austrian": {
        "group": "pub-austrian", "lang": "deu", "license": "CC-BY-4.0 (Zenodo record)",
        "url": "https://doi.org/10.5281/zenodo.3387369",
        "chars": (200, 2500), "scoring": True, "policy": "passes",
    },
    "ndl-pdm": {
        "group": "pub-ndl-pdm", "lang": "jpn", "license": "Public Domain Mark 1.0 (repository README)",
        "url": "https://github.com/ndl-lab/pdmocrdataset-part1",
        "chars": (12, 300), "scoring": False,
        "policy": "fails: README does not say the transcriptions were made or checked by people",
    },
}


def box_of(points: list[tuple[float, float]]) -> list[int]:
    xs, ys = [p[0] for p in points], [p[1] for p in points]
    return [int(min(xs)), int(min(ys)), int(max(xs) + 0.999), int(max(ys) + 0.999)]


def parse_coco_gold(json_path: Path, kinds: tuple[int, ...] = (1,)) -> list[dict]:
    """AmericanStoriesTraining gold_hand_transcription.json: COCO, text on each region (category 1 is an article)."""
    d = json.loads(json_path.read_text(encoding="utf-8"))
    files = {i["id"]: i["file_name"] for i in d["images"]}
    out = []
    for a in d["annotations"]:
        if a.get("category_id") in kinds and (a.get("text") or "").strip() and a["image_id"] in files:
            x, y, w, h = a["bbox"]
            out.append({"page": files[a["image_id"]], "region": str(a["id"]), "box": [int(x), int(y), int(x + w + 0.999),
                        int(y + h + 0.999)], "text": a["text"].strip("\n")})
    return out


def parse_page_xml(xml_path: Path) -> tuple[str | None, list[dict]]:
    """PAGE XML (any schema year): the image name, and one region per TextRegion that holds TextLines.

    The region's text is its lines' TextEquiv/Unicode, one per line, in file order.
    """
    import xml.etree.ElementTree as ET

    root = ET.parse(xml_path).getroot()

    def local(e):
        return e.tag.rsplit("}", 1)[-1]

    page = next((e for e in root.iter() if local(e) == "Page"), None)
    name = page.get("imageFilename") if page is not None else None
    regions = []
    for reg in (e for e in root.iter() if local(e) == "TextRegion"):
        lines, pts = [], []
        for ln in (c for c in reg if local(c) == "TextLine"):
            u = next((u for te in ln if local(te) == "TextEquiv" for u in te if local(u) == "Unicode"), None)
            if u is not None and (u.text or "").strip():
                lines.append(u.text.strip())
        c = next((c for c in reg if local(c) == "Coords"), None)
        if c is not None and c.get("points"):
            pts = [tuple(float(v) for v in p.split(",")) for p in c.get("points").split()]
        if lines and pts:
            regions.append({"region": reg.get("id") or str(len(regions)), "box": box_of(pts), "text": "\n".join(lines)})
    return name, regions


def parse_ndl_json(json_path: Path) -> list[dict]:
    """NDL public-domain OCR dataset, JSON form: one text line per box, `quad` x1..y4, orientation vertical or horizontal."""
    d = json.loads(json_path.read_text(encoding="utf-8"))
    out = []
    for b in d.get("boxes", []):
        q = b["quad"]
        if (b.get("text") or "").strip():
            out.append({"region": str(b.get("box_id")), "text": b["text"].strip(),
                        "box": box_of([(q[f"x{i}"], q[f"y{i}"]) for i in range(1, 5)]),
                        "orientation": b.get("orientation")})
    return out


def public_regions(dataset: str, src: Path) -> list[dict]:
    """Every transcribed region in a downloaded dataset: page (image path), page_id, region, box, text."""
    out: list[dict] = []
    index: dict[str, Path] = {}
    for p in sorted(src.rglob("*")):
        if p.suffix.lower() in (".jpg", ".jpeg", ".png", ".tif", ".tiff"):
            index.setdefault(p.name, p)
    if dataset == "americanstories":
        gold_json = next(iter(sorted(src.rglob("gold_hand_transcription.json"))), None)
        for r in parse_coco_gold(gold_json) if gold_json else []:
            if r["page"] in index:
                out.append({**r, "page_id": f"americanstories-gold-{Path(r['page']).stem}", "image": index[r["page"]]})
    elif dataset in ("reichsanzeiger", "austrian"):
        for x in sorted(src.rglob("*.xml")):
            try:
                name, regs = parse_page_xml(x)
            except Exception:  # noqa: BLE001 -- not a PAGE file (METS, metadata)
                continue
            if name and name in index and regs:
                out += [{**r, "page_id": f"{dataset}-{Path(name).stem}", "image": index[name]} for r in regs]
    elif dataset == "ndl-pdm":
        # json/<dir>/<name>.json with its image at img/<dir>/<name>.jpg (layout from the README; unchecked).
        for j in sorted(src.rglob("*.json")):
            for ext in (".jpg", ".jpeg", ".png"):
                img = index.get(j.stem + ext)
                if img is not None:
                    out += [{**r, "page_id": f"ndl-pdm-{j.parent.name}-{j.stem}", "image": img}
                            for r in parse_ndl_json(j)]
                    break
    else:
        raise ValueError(f"unknown dataset {dataset!r}")
    return out


def pick_public(regions: list[dict], count: int, per_page: int, seed: int, min_chars: int, max_chars: int) -> list[dict]:
    """Seeded: regions of min_chars to max_chars; at most per_page per page; pages in a seeded order, round robin."""
    by_page: dict[str, list[dict]] = {}
    for r in regions:
        if min_chars <= len(normalize(r["text"])) <= max_chars:
            by_page.setdefault(r["page_id"], []).append(r)
    rnd = random.Random(f"{seed}:public")
    pages = sorted(by_page)
    rnd.shuffle(pages)
    for p in pages:
        by_page[p].sort(key=lambda r: r["region"])
        rnd.shuffle(by_page[p])
    picked: list[dict] = []
    for rank in range(per_page):
        for p in pages:
            if rank < len(by_page[p]) and len(picked) < count:
                picked.append(by_page[p][rank])
    return picked


def public_rows(dataset: str, pick: dict, n: int, crop: bytes, box: list[int], page_sha: str, encoder: str) -> tuple[dict, dict]:
    meta = PUBLIC[dataset]
    crop_id = f"{meta['group']}-{n:03d}"
    who = f"{dataset}: published ground truth"
    m = {"crop_id": crop_id, "page_id": pick["page_id"], "group": meta["group"], "lang": meta["lang"], "era": None,
         "stratum": meta["group"], "box": box, "image": f"images/{crop_id}.jpg", "crop_sha256": sha256(crop),
         "crop_bytes": len(crop), "encoder": encoder, "page_image_sha256": page_sha, "source_image": pick["image"].name,
         "region": pick["region"], "source_dataset": dataset, "source_url": meta["url"], "source_license": meta["license"],
         "for_scoring": meta["scoring"]}
    t = {"crop_id": crop_id, "group": meta["group"], "lang": meta["lang"], "date": None, "title": pick["page_id"],
         "image": m["image"], "hints": {"is_truth": False, "note": "none: the truth is the dataset's own", "texts": {}},
         "transcription": pick["text"], "transcriber": who, "reviewer": "not applicable: published ground truth",
         "hint_copy_reviewed": False, "truth_source": dataset, "for_scoring": meta["scoring"],
         "notes": "" if meta["scoring"] else f"not for scoring: source policy {meta['policy']}"}
    return m, t


def cmd_public_convert(a: argparse.Namespace) -> int:
    if not PUBLIC[a.dataset]["scoring"] and not a.not_for_scoring:
        print(f"{a.dataset} does not meet the source policy ({PUBLIC[a.dataset]['policy']}); "
              "pass --not-for-scoring to convert it for comparison only", file=sys.stderr)
        return 2
    import PIL
    from PIL import Image

    Image.MAX_IMAGE_PIXELS = None
    regions = public_regions(a.dataset, a.src)
    if not regions:
        print(f"error: no transcribed regions found under {a.src} for {a.dataset}", file=sys.stderr)
        return 1
    lo, hi = PUBLIC[a.dataset]["chars"]
    pick = pick_public(regions, a.count, a.per_page, a.seed, a.min_chars or lo, a.max_chars or hi)
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "images").mkdir(exist_ok=True)
    manifest, template, page_sha = [], [], {}
    for n, p in enumerate(pick, 1):
        img = Image.open(p["image"])
        x0, y0, x1, y1 = p["box"]
        box = [max(0, x0), max(0, y0), min(img.width, x1), min(img.height, y1)]
        if box[2] <= box[0] or box[3] <= box[1]:
            print(f"skipped, box outside the image: {p['page_id']} {p['region']}", file=sys.stderr)
            continue
        data = encode_crop(img, box)
        if p["image"] not in page_sha:
            page_sha[p["image"]] = sha256(p["image"].read_bytes())
        m, t = public_rows(a.dataset, p, len(manifest) + 1, data, box, page_sha[p["image"]], f"Pillow {PIL.__version__}")
        (a.out / m["image"]).write_bytes(data)
        manifest.append(m)
        template.append(t)
    write_jsonl(a.out / "manifest.jsonl", manifest)
    write_jsonl(a.out / "crops.jsonl", template)
    print(f"{len(manifest)} crops from {len({m['page_id'] for m in manifest})} pages "
          f"({len(regions)} transcribed regions in the download), {sum(m['crop_bytes'] for m in manifest) / 1e6:.1f} MB")
    return 0


def download(url: str, dest: Path, gap: float = 1.0) -> None:
    """Streams url to dest (no whole-file buffer: some of these are gigabytes). Skips a file already there."""
    import time
    import urllib.request

    if dest.exists() and dest.stat().st_size:
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    time.sleep(gap)
    req = urllib.request.Request(url, headers={"User-Agent": "usnewsmap-ocr-survey/1.0 (+https://usnewsmap.com)"})
    tmp = dest.with_suffix(dest.suffix + ".part")
    with urllib.request.urlopen(req, timeout=300) as r, tmp.open("wb") as f:
        while chunk := r.read(1 << 20):
            f.write(chunk)
    tmp.rename(dest)


def cmd_public_fetch(a: argparse.Namespace) -> int:
    import zipfile

    if not a.confirm_download:
        print("public-fetch downloads up to several GB; pass --confirm-download", file=sys.stderr)
        return 2
    a.src.mkdir(parents=True, exist_ok=True)
    if a.dataset == "americanstories":
        j = a.src / "gold_hand_transcription.json"
        download(HF_AS_TRAIN + "gold_data/gold_hand_transcription.json", j)
        for img in json.loads(j.read_text(encoding="utf-8"))["images"][: a.pages]:
            download(HF_AS_TRAIN + "gold_data/gold_images/" + img["file_name"], a.src / img["file_name"])
    elif a.dataset == "reichsanzeiger":
        z = a.src / "reichsanzeiger-gt-1.0.0.zip"
        download(ZENODO_FILE.format(rec=10144428, name="UB-Mannheim/reichsanzeiger-gt-1.0.0.zip"), z)
        zipfile.ZipFile(z).extractall(a.src / "gt")
        lists = sorted((a.src / "gt").rglob("imageurls.list"))
        if not lists:
            print("imageurls.list not found in the archive", file=sys.stderr)
            return 1
        lines = [x.split() for x in lists[0].read_text().splitlines() if x.strip()]
        random.Random("public-fetch").shuffle(lines)  # a fixed order, so --pages N is the same pages every time
        for url, name in lines[: a.pages]:
            download(REICHSANZEIGER_IMAGES + url, a.src / "images" / name)
    elif a.dataset == "austrian":
        for name in ("TrainingSet_ONB_Newseye_GT_M1+.zip", "ValidationSet_ONB_Newseye_GT_M1+.zip"):
            z = a.src / name
            download(ZENODO_FILE.format(rec=3387369, name=name.replace("+", "%2B")), z)
            zipfile.ZipFile(z).extractall(a.src / name.removesuffix(".zip"))
    elif a.dataset == "ndl-pdm":
        z = a.src / "tosho_all_linejson.zip"
        download(NDL_PDM_JSON, z)  # 5.2 GB
        zipfile.ZipFile(z).extractall(a.src / "tosho_all_linejson")
    else:
        print(f"unknown dataset {a.dataset!r}", file=sys.stderr)
        return 2
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("select")
    s.add_argument("--scores", type=Path, help="the survey's data/scores.jsonl (page_id, group, lang)")
    s.add_argument("--manifest", type=Path, help="the sample's manifest-final.jsonl")
    s.add_argument("--out", type=Path, required=True)
    s.add_argument("--target", type=int, default=DEFAULT_TARGET)
    s.add_argument("--seed", type=int, default=DEFAULT_SEED)
    s.set_defaults(fn=cmd_select)

    c = sub.add_parser("cut")
    c.add_argument("--selection", type=Path, required=True)
    c.add_argument("--sample", type=Path, required=True)
    c.add_argument("--work", type=Path, help="survey work dir, for the engines' hint texts")
    c.add_argument("--out", type=Path, required=True)
    c.add_argument("--seed", type=int, default=DEFAULT_SEED)
    c.set_defaults(fn=cmd_cut)

    f = sub.add_parser("fetch")
    f.add_argument("--manifest", type=Path, required=True)
    f.add_argument("--out", type=Path, required=True)
    f.set_defaults(fn=cmd_fetch)

    v = sub.add_parser("validate")
    v.add_argument("--crops", type=Path, required=True)
    v.add_argument("--manifest", type=Path)
    v.add_argument("--images", type=Path)
    v.add_argument("--allow-unreviewed", action="store_true", help="skip the second-pass reviewer check")
    v.set_defaults(fn=cmd_validate)

    k = sub.add_parser("score")
    k.add_argument("--crops", type=Path, required=True)
    k.add_argument("--hyp", action="append", default=[], metavar="ENGINE=FILE.jsonl")
    k.add_argument("--hints", action="store_true", help="score the engine texts stored in crops.jsonl")
    k.add_argument("--allow-unreviewed", action="store_true")
    k.add_argument("--out", type=Path)
    k.set_defaults(fn=cmd_score)

    g = sub.add_parser("agree")
    g.add_argument("--crops", type=Path, required=True)
    g.add_argument("--out", type=Path)
    g.set_defaults(fn=cmd_agree)

    for name, fn in (("public-fetch", cmd_public_fetch), ("public-convert", cmd_public_convert)):
        q = sub.add_parser(name)
        q.add_argument("--dataset", required=True, choices=sorted(PUBLIC))
        q.add_argument("--src", type=Path, required=True, help="where the dataset is (or goes)")
        if name == "public-fetch":
            q.add_argument("--confirm-download", action="store_true", help="agree to a download of up to several GB")
            q.add_argument("--pages", type=int, default=20, help="page images to download (datasets with separate images)")
        else:
            q.add_argument("--out", type=Path, required=True)
            q.add_argument("--count", type=int, default=60)
            q.add_argument("--per-page", type=int, default=3)
            q.add_argument("--seed", type=int, default=DEFAULT_SEED)
            q.add_argument("--min-chars", type=int, help="shortest region, in normalized characters (default per dataset)")
            q.add_argument("--max-chars", type=int, help="longest region (default per dataset)")
            q.add_argument("--not-for-scoring", action="store_true",
                           help="allow a dataset that fails the source policy, for comparison only")
        q.set_defaults(fn=fn)

    a = ap.parse_args(argv)
    return a.fn(a)


if __name__ == "__main__":
    sys.exit(main())
