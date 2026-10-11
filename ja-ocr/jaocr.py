#!/usr/bin/env python3
"""OCR the Japanese pages LoC has no text for, with NDLOCR-Lite (#128).

    python3 jaocr.py targets [--mixed-below 0.65]
                                      list the pages to OCR (writes ocr-ja/targets-v3.jsonl)
    python3 jaocr.py run [--limit N] [--mixed-below 0.65]
                                      OCR the targets not done yet
    python3 jaocr.py audit            titles LoC ships pages without text for (audit.py)
    python3 jaocr.py quality [--sample-pct 2] [--min-pages 50] [--metric v2]
                                      how good LoC's OCR is, by language and decade (quality.py)
    python3 jaocr.py damage-parity [--pages 400] [--audit-version pages-v20261003-1]
                                      pages scored by damage_v2.py, to check the Rust port against (damage_parity.py)
    python3 jaocr.py mixed            text on the Japanese titles' pages search can't reach yet (mixed.py)
    python3 jaocr.py american-stories --year 1865 [--year ...] [--sample-pct 10] [--diff]
                                      American Stories' text against LoC's on our pages (american_stories.py)
    python3 jaocr.py american-stories-write [--year ...]
                                      write American Stories' text for our pages (american_stories_write.py)

Stores are blob container URLs (Entra auth via the managed identity) or local
directories, so the job runs the same way against a copy on disk:

    USNM_REFERENCE_URL  reference store: current.json, {version}/batches.json,
                        {version}/titles.json
    USNM_CURATED_URL    curated store: the batches' Parquet parts (read) and
                        ocr-ja/ (written)

Targets are pages of titles whose catalog languages include Japanese whose
LoC text is missing, empty, short or mostly not words: LoC's OCR has no
Japanese script, so those are the Japanese pages. Pages whose LoC text is
word-like enough to keep but under --mixed-below (JAOCR_MIXED_BELOW, 0.65)
are targets too (`mixed`, #204): a page with an English column and a
Japanese one that LoC read in English. Two sources:

- missing: LoC's bulk archives ship a Japanese page as an empty ALTO
  ocr.xml with no ocr.txt, so curation (which reads ocr.txt) never saw it.
  The job lists the archives of every batch LoC's datasets listing gives
  for a Japanese title, and takes each page with ocr.xml and no ocr.txt.
- empty, short, garbled, mixed: pages curation did see, in the published
  version's Parquet parts, whose text is empty, short, garbled Latin, or
  partly so.

`run` claims one issue at a time by creating ocr-ja/claims/<targets>/<issue>.json
(a create-only write, so replicas never take the same issue; a claim older
than JAOCR_CLAIM_HOURS is taken over with an ETag compare-and-swap). For each issue it asks loc.gov for the
page images (one API request per issue), downloads each target page's
full-resolution JPEG from LoC's IIIF service, runs NDLOCR-Lite on the issue's
pages together, and writes each page's ALTO to ocr-ja/alto/<doc_id>.xml
(alto.py, #151) and then ocr-ja/pages/<issue>[.<n>].parquet, an overlay keyed
by doc_id (see write_part; not the full curated schema). A part is written
only when every page it was asked for has text and ALTO. A page is done when
its doc_id is in a part and it has ALTO, so an issue done before new targets
appeared, or before the job wrote ALTO, gets another part for those pages. loc.gov's API is paced JAOCR_API_GAP seconds
per request and the image server JAOCR_TILE_GAP seconds, each multiplied by
JAOCR_REPLICAS so replicas together stay under LoC's limits.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

import alto

UA = "usnewsmap-ja-ocr/1.0 (+https://usnewsmap.com)"
OCR_SOURCE = "usnm-ndlocr-lite"
PREFIX = "ocr-ja"
# Bumped when the way targets are chosen changes, so a run lists them again
# (v3: the `mixed` pages, #204). A --mixed-below other than the default
# names its own list (targets_name).
TARGETS = "targets-v3"
# Of pages with LoC text, those under this word-like share are targets
# (#204). Under 0.35 they are `garbled`; from 0.35 to here, `mixed`. 0.65 is
# where the Japanese titles' excess over the control stops: 0.35-0.5 holds
# 2.1% of their pages against 1.3% (1.6 times), 0.5-0.65 6.3% against 2.5%
# (2.5 times), 0.65-0.8 27.4% against 22.7% (1.2 times, while 0.8-0.9 has
# 6.4 points fewer: a shift of the whole distribution, not extra pages).
# Read with NDLOCR-Lite, 3 of 13 sampled pages from 0.5 to 0.65 had Japanese
# (Rocky Shimpo front pages) and none of 28 from 0.35 to 0.5 or 0.65 to 0.8;
# the English ones it read better than LoC up to 0.65 and about as well
# above (04 §4.8).
MIXED_BELOW = 0.65
GARBLED_BELOW = 0.35
COLLECTION = "https://www.loc.gov/collections/chronicling-america/?fo=json&c=1&at=datasets"
MIN_PAGE_CHARS = 20  # crates/usnm-core/src/text.rs
# A word: Capitalized or lower case, or ALL CAPS (headlines); mixed case is OCR noise.
WORDLIKE = re.compile(r"^(?:[A-Z]?[a-z]{2,}|[A-Z]{3,15})$")
# usnm_store::is_safe_segment: alphanumeric first, then [A-Za-z0-9._-], at most 128.
SAFE_SEGMENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
PAGE_COLUMNS = ["doc_id", "page_key", "lccn", "date", "edition", "seq", "batch", "text_status", "text"]


def log(msg: str, **fields) -> None:
    print(json.dumps({"ts": datetime.now(timezone.utc).isoformat(), "message": msg, **fields},
                     ensure_ascii=False), flush=True)


# ---------------------------------------------------------------- stores


TMP_PREFIX = ".tmp-"  # LocalStore's files being written


class LocalStore:
    def __init__(self, root: str):
        self.root = Path(root)

    def read(self, path: str) -> bytes:
        return (self.root / path).read_bytes()

    def _staged(self, p: Path, data: bytes) -> Path:
        """`data` in a temporary file beside `p`, so readers never see a partly written file."""
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.parent / f"{TMP_PREFIX}{p.name}.{os.getpid()}.{threading.get_ident()}"
        tmp.write_bytes(data)
        return tmp

    def write(self, path: str, data: bytes) -> None:
        p = self.root / path
        os.replace(self._staged(p, data), p)

    def create(self, path: str, data: bytes) -> bool:
        """Write only if absent; False if it exists. Atomic: the file appears whole or not at all."""
        p = self.root / path
        tmp = self._staged(p, data)
        try:
            os.link(tmp, p)
            return True
        except FileExistsError:
            return False
        finally:
            tmp.unlink()

    def list(self, prefix: str) -> list[str]:
        base = self.root / prefix
        if not base.exists():
            return []
        return [str(p.relative_to(self.root)) for p in base.rglob("*")
                if p.is_file() and not p.name.startswith(TMP_PREFIX)]

    def modified(self, path: str) -> datetime:
        return datetime.fromtimestamp((self.root / path).stat().st_mtime, timezone.utc)

    def exists(self, path: str) -> bool:
        return (self.root / path).is_file()

    def renew(self, path: str, data: bytes, owner: str) -> bool:
        """Rewrite a lock only if `owner` still holds it. Not atomic: the local store is for one process."""
        try:
            if json.loads(self.read(path)).get("owner") != owner:
                return False
        except (OSError, ValueError):
            return False
        self.write(path, data)
        return True

    def take_over(self, path: str, data: bytes, stale: timedelta) -> bool:
        """Replace a blob older than `stale`. Not atomic: the local store is for one process."""
        if datetime.now(timezone.utc) - self.modified(path) <= stale:
            return False
        self.write(path, data)
        return True


class BlobStore:
    def __init__(self, url: str):
        from azure.identity import DefaultAzureCredential
        from azure.storage.blob import ContainerClient

        self.url = url
        self.client = ContainerClient.from_container_url(url, credential=DefaultAzureCredential())

    def __reduce__(self):
        # A worker process (quality.py) opens its own client from the URL.
        return (BlobStore, (self.url,))

    def read(self, path: str) -> bytes:
        return self.client.download_blob(path).readall()

    def write(self, path: str, data: bytes) -> None:
        self.client.upload_blob(path, data, overwrite=True)

    def create(self, path: str, data: bytes) -> bool:
        from azure.core.exceptions import ResourceExistsError

        try:
            self.client.upload_blob(path, data, overwrite=False)
            return True
        except ResourceExistsError:
            return False

    def list(self, prefix: str) -> list[str]:
        return [b.name for b in self.client.list_blobs(name_starts_with=prefix)]

    def modified(self, path: str) -> datetime:
        return self.client.get_blob_client(path).get_blob_properties().last_modified

    def exists(self, path: str) -> bool:
        return self.client.get_blob_client(path).exists()

    def renew(self, path: str, data: bytes, owner: str) -> bool:
        """Rewrite a lock only if `owner` still holds it, and nobody wrote it since we read it (ETag)."""
        from azure.core import MatchConditions
        from azure.core.exceptions import ResourceModifiedError, ResourceNotFoundError

        blob = self.client.get_blob_client(path)
        try:
            got = blob.download_blob()
            if json.loads(got.readall()).get("owner") != owner:
                return False
            blob.upload_blob(data, overwrite=True, etag=got.properties.etag,
                             match_condition=MatchConditions.IfNotModified)
            return True
        except (ResourceModifiedError, ResourceNotFoundError, ValueError):
            return False

    def take_over(self, path: str, data: bytes, stale: timedelta) -> bool:
        """Replace a blob older than `stale`, only if nobody replaced it since we looked (ETag)."""
        from azure.core import MatchConditions
        from azure.core.exceptions import ResourceModifiedError

        blob = self.client.get_blob_client(path)
        props = blob.get_blob_properties()
        if datetime.now(timezone.utc) - props.last_modified <= stale:
            return False
        try:
            blob.upload_blob(data, overwrite=True, etag=props.etag, match_condition=MatchConditions.IfNotModified)
            return True
        except ResourceModifiedError:
            return False


def read_all(store, paths: list[str], workers: int = 16):
    """Each path's bytes, in order, read `workers` at a time: at most that many are in memory at once
    (a plain executor map reads them all ahead and held a whole year's parts, killing the job)."""
    from concurrent.futures import ThreadPoolExecutor

    with ThreadPoolExecutor(workers) as ex:
        for i in range(0, len(paths), workers):
            yield from ex.map(store.read, paths[i:i + workers])


def store(url: str):
    return BlobStore(url) if url.startswith("https://") else LocalStore(url)


# ---------------------------------------------------------------- targets


def japanese_lccns(titles: list[dict]) -> set[str]:
    return {t["lccn"] for t in titles if "jpn" in (t.get("languages") or [])}


def wordlike_share(text: str) -> float:
    words = text.split()
    if not words:
        return 0.0
    return sum(1 for w in words if WORDLIKE.match(w.strip(".,;:!?'\"()"))) / len(words)


def needs_ocr(text_status: str, text: str | None, mixed_below: float = MIXED_BELOW) -> str | None:
    """Why a Japanese title's page needs our OCR, or None if LoC's text is English."""
    if text_status != "ok":
        return text_status  # empty or short: LoC found no text
    share = wordlike_share(text or "")
    if share < GARBLED_BELOW:
        return "garbled"
    if share < mixed_below:
        return "mixed"  # partly words: an English column beside one LoC couldn't read
    return None


def targets_name(mixed_below: float = MIXED_BELOW) -> str:
    """The targets list's name (and its claims'): TARGETS, or with its own
    --mixed-below, a whole percentage (mixed_below_setting)."""
    pct = round(mixed_below * 100)
    if abs(mixed_below * 100 - pct) > 1e-9:
        raise ValueError(f"--mixed-below must be a whole percentage, not {mixed_below}")
    if mixed_below == MIXED_BELOW:
        return TARGETS
    return f"{TARGETS}-mixed{pct:02d}"


def mixed_below_setting(arg: float | None) -> float:
    """--mixed-below, else JAOCR_MIXED_BELOW, else MIXED_BELOW: a whole
    percentage from 0 to 1 (0.65, not 0.649), so each cut has its own list
    and claims (targets_name); at most GARBLED_BELOW turns `mixed` off."""
    v = arg if arg is not None else float(os.environ.get("JAOCR_MIXED_BELOW", MIXED_BELOW))
    pct = round(v * 100)
    if not 0 <= v <= 1 or abs(v * 100 - pct) > 1e-9:
        raise ValueError(f"--mixed-below must be a whole percentage from 0 to 1 (such as 0.65), not {v}")
    return pct / 100


def issue_key(lccn: str, date: str, edition: int) -> str:
    return f"{lccn}_{date}_ed-{edition}"


DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
YMD = re.compile(r"^\d{4}$|^\d{2}$")
LCCN = re.compile(r"^[a-z0-9]{1,16}$")


def page_from_path(path: str) -> tuple[str, str] | None:
    """(page_key, file name) for a page file, in either archive layout
    (crates/usnm-ingest/src/archive.rs page_key_from_path): lccn/yyyy/mm/dd/ed-n/seq-n/file
    or lccn/yyyy-mm-dd/ed-n/seq-n/file, under any prefix."""
    parts = path.strip().removeprefix("./").strip("/").split("/")
    n = len(parts)
    if n < 5 or not parts[n - 2].startswith("seq-") or not parts[n - 3].startswith("ed-"):
        return None
    # crates/usnm-core/src/ids.rs: ed and seq from 1 (a few archives hold a seq-0
    # page, which curation skips too), a real calendar date, lowercase LCCN. The
    # curated and overlay columns are int16, so at most 32767.
    ed, seq = number(parts[n - 3], "ed-"), number(parts[n - 2], "seq-")
    if ed is None or seq is None:
        return None
    if DATE.match(parts[n - 4]):
        date, lccn = parts[n - 4], parts[n - 5]
    elif n >= 7 and all(YMD.match(x) for x in parts[n - 6:n - 3]):
        date, lccn = "-".join(parts[n - 6:n - 3]), parts[n - 7]
    else:
        return None
    try:
        datetime.strptime(date, "%Y-%m-%d")
    except ValueError:
        return None
    if not LCCN.match(lccn):
        return None
    return f"{lccn}/{date}/ed-{ed}/seq-{seq}", parts[-1]


def number(part: str, prefix: str) -> int | None:
    v = part[len(prefix):] if part.startswith(prefix) else ""
    return int(v) if v.isdigit() and 0 < int(v) <= 32767 else None


def missing_pages(names, jpn: set[str], batch: str) -> list[dict]:
    """Pages of Japanese titles with an ALTO file but no ocr.txt."""
    files: dict[str, set[str]] = {}
    for name in names:
        hit = page_from_path(name)
        if hit:
            files.setdefault(hit[0], set()).add(hit[1])
    out = []
    for key, fs in files.items():
        lccn, date, ed, seq = key.split("/")
        if lccn not in jpn or "ocr.txt" in fs or "ocr.xml" not in fs:
            continue
        out.append({"doc_id": key.replace("/", "_"), "page_key": key, "lccn": lccn, "date": date,
                    "edition": int(ed[3:]), "seq": int(seq[4:]), "batch": batch, "loc_text": "missing"})
    return out


def list_archive(url: str, pacer: "Pacer") -> list[str]:
    """Member names of a .tar.bz2 on LoC's server, streamed (nothing is kept)."""
    import tarfile

    for attempt in range(4):
        pacer.wait()
        try:
            req = urllib.request.Request(url, headers={"User-Agent": UA})
            with urllib.request.urlopen(req, timeout=600) as r, tarfile.open(fileobj=r, mode="r|bz2") as tar:
                return [m.name for m in tar]
        except (urllib.error.URLError, http.client.HTTPException, OSError, EOFError, tarfile.TarError) as e:
            log("retrying archive", url=url, error=str(e)[:200])
            time.sleep(120 * (attempt + 1))
    raise RuntimeError(f"giving up on {url}")


def targets(reference, curated, datasets: list[dict] | None = None,
            mixed_below: float = MIXED_BELOW) -> list[dict]:
    current = json.loads(reference.read("current.json"))
    version = current["reference"]
    # The same rule as the API and the store: one path segment, no traversal.
    if not SAFE_SEGMENT.match(version):
        raise ValueError(f"current.json names an unsafe reference version: {version!r}")
    titles = json.loads(reference.read(f"{version}/titles.json"))
    if reference.exists("catalog/titles.json"):
        # The working catalog has titles with no published pages too (all-Japanese titles).
        titles = titles + json.loads(reference.read("catalog/titles.json"))
    jpn = japanese_lccns(titles)
    by_id: dict[str, dict] = {}

    # Pages curation saw, with empty, short or garbled text.
    batches = json.loads(reference.read(f"{version}/batches.json"))
    picked = [b for b in batches if jpn & set(b["curated"]["lccns"])]
    log("listing curated targets", version=version, japanese_titles=len(jpn), batches=len(picked))
    import pyarrow.parquet as pq

    for b in picked:
        for part in b["curated"]["parts"]:
            t = pq.read_table(io.BytesIO(curated.read(part)), columns=PAGE_COLUMNS)
            for r in t.to_pylist():
                if r["lccn"] not in jpn:
                    continue
                why = needs_ocr(r["text_status"], r["text"], mixed_below)
                if why:
                    by_id[r["doc_id"]] = {
                        "doc_id": r["doc_id"], "page_key": r["page_key"], "lccn": r["lccn"],
                        "date": str(r["date"]), "edition": int(r["edition"]), "seq": int(r["seq"]),
                        "batch": r["batch"], "loc_text": why,
                    }

    # Pages curation never saw: in the archives, with ALTO and no ocr.txt.
    if datasets is None:
        datasets = json.loads(fetch(COLLECTION, Pacer(float(os.environ.get("JAOCR_API_GAP", "3.5")))))["datasets"]
    archives = [d for d in datasets if jpn & set(d.get("lccns") or [])]
    log("listing archives", archives=len(archives))
    bulk = Pacer(float(os.environ.get("JAOCR_ARCHIVE_GAP", "60")))
    for d in archives:
        batch = re.sub(r"_ver\d+$", "", d["batch"])
        found = missing_pages(list_archive(d["url"], bulk), jpn, batch)
        for r in found:
            by_id.setdefault(r["doc_id"], r)
        log("archive listed", batch=d["batch"], missing=len(found))

    out = sorted(by_id.values(), key=lambda r: (r["lccn"], r["date"], r["edition"], r["seq"]))
    return out


# ---------------------------------------------------------------- loc.gov


@dataclass
class Pacer:
    gap: float
    last: float = 0.0

    def wait(self) -> None:
        d = self.gap - (time.time() - self.last)
        if d > 0:
            time.sleep(d)
        self.last = time.time()


def fetch(url: str, pacer: Pacer, attempts: int = 5) -> bytes:
    for i in range(attempts):
        pacer.wait()
        try:
            req = urllib.request.Request(url, headers={"User-Agent": UA})
            with urllib.request.urlopen(req, timeout=180) as r:
                return r.read()
        # A dropped connection mid-body is http.client.IncompleteRead (an
        # HTTPException, not a URLError); sockets raise OSError.
        except (urllib.error.URLError, http.client.HTTPException, OSError) as e:
            if isinstance(e, urllib.error.HTTPError) and e.code == 404:
                raise
            wait = 60 * (i + 1)
            if isinstance(e, urllib.error.HTTPError) and e.code == 429:
                wait = max(wait, int(e.headers.get("Retry-After") or 0))
            log("retrying", url=url[:120], error=str(e)[:200], wait=wait)
            time.sleep(wait)
    raise RuntimeError(f"giving up on {url}")


def iiif_base(jp2_url: str) -> str:
    path = jp2_url.split("/storage-services/service/", 1)[1].removesuffix(".jp2")
    return "https://tile.loc.gov/image-services/iiif/service:" + path.replace("/", ":")


def page_images(item: dict) -> dict[int, str]:
    """seq -> IIIF base URL, from loc.gov's issue JSON.

    `files` is in page (seq) order, which is not the order of the image file
    names: microfilm frames can run 0364, 0368, 0369, 0365, ... for pages 1-4.
    """
    files = (item.get("resources") or [{}])[0].get("files") or []
    out = {}
    for seq, page in enumerate(files, 1):
        jp2 = next((f.get("url", "") for f in page if f.get("mimetype") == "image/jp2"), "")
        if jp2:
            out[seq] = iiif_base(jp2)
    return out


def page_altos(item: dict) -> dict[int, str]:
    """seq -> LoC's ALTO (ocr.xml) URL, from the same issue JSON."""
    files = (item.get("resources") or [{}])[0].get("files") or []
    out = {}
    for seq, page in enumerate(files, 1):
        xml = next((f.get("url", "") for f in page if f.get("mimetype") == "text/xml"), "")
        if xml:
            out[seq] = xml
    return out


def loc_geometry(url: str | None, pacer: "Pacer", page_key: str) -> alto.Geometry:
    """The page's dpi and size from LoC's ALTO; 300 dpi and our image's size without it."""
    if not url:
        return alto.Geometry()
    try:
        return alto.loc_geometry(fetch(url, pacer))
    except Exception as e:  # noqa: BLE001 - the defaults are right for every page checked
        log("no LoC ALTO for page", page=page_key, error=f"{type(e).__name__}: {e}"[:200])
        return alto.Geometry()


# ---------------------------------------------------------------- OCR


def normalize(text: str) -> str:
    """NFC, no control characters, trailing spaces trimmed; newlines (columns) kept."""
    text = unicodedata.normalize("NFC", text)
    text = "".join(c for c in text if c in "\n\t " or not unicodedata.category(c).startswith("C"))
    lines = [re.sub(r"[ \t　]+", " ", ln).strip() for ln in text.split("\n")]
    return "\n".join(ln for ln in lines if ln)


def text_status(text: str) -> str:
    n = sum(1 for c in text if not c.isspace())
    return "empty" if n == 0 else "short" if len(text.strip()) < MIN_PAGE_CHARS else "ok"


def ndlocr(root: Path, images: Path, out: Path) -> None:
    # It exits 0 with "Output Directory is not found." if out doesn't exist.
    out.mkdir(parents=True, exist_ok=True)
    r = subprocess.run([sys.executable, "ocr.py", "--sourcedir", str(images), "--output", str(out)],
                       cwd=root / "src", capture_output=True, text=True)
    if r.returncode != 0 or not any(out.glob("*.txt")):
        raise RuntimeError(f"NDLOCR-Lite produced no text (exit {r.returncode}): {(r.stdout + r.stderr)[-500:]}")


def ndlocr_version(root: Path) -> str:
    """The NDLOCR-Lite commit (NDLOCR_COMMIT, set by the image), else the checkout's HEAD."""
    if os.environ.get("NDLOCR_COMMIT"):
        return os.environ["NDLOCR_COMMIT"]
    r = subprocess.run(["git", "-C", str(root), "rev-parse", "--short", "HEAD"], capture_output=True, text=True)
    return r.stdout.strip() or "unknown"


def write_part(curated, issue: str, rows: list[dict]) -> None:
    """The overlay schema: identity (doc_id, page_key, lccn, date, edition, seq, batch),
    provenance (ocr_source, ocr_engine, loc_text, image_url, ocred_at) and the text
    (text_status, text, text_chars, text_sha256)."""
    import pyarrow as pa
    import pyarrow.parquet as pq

    schema = pa.schema([
        ("doc_id", pa.string()), ("page_key", pa.string()), ("lccn", pa.string()),
        ("date", pa.date32()), ("edition", pa.int16()), ("seq", pa.int16()),
        ("batch", pa.string()), ("ocr_source", pa.string()), ("ocr_engine", pa.string()),
        ("loc_text", pa.string()), ("text_status", pa.string()), ("text", pa.large_string()),
        ("text_chars", pa.int32()), ("text_sha256", pa.binary(32)), ("image_url", pa.string()),
        ("ocred_at", pa.timestamp("us", tz="UTC")),
    ])
    for r in rows:
        r["date"] = datetime.strptime(r["date"], "%Y-%m-%d").date() if isinstance(r["date"], str) else r["date"]
    buf = io.BytesIO()
    pq.write_table(pa.Table.from_pylist(rows, schema=schema), buf, compression="zstd")
    curated.write(f"{PREFIX}/pages/{issue}.parquet", buf.getvalue())


def done_pages(curated, parts: list[str]) -> set[str]:
    import pyarrow.parquet as pq

    done: set[str] = set()
    for p in parts:
        if p.endswith(".parquet"):
            done.update(pq.read_table(io.BytesIO(curated.read(p)), columns=["doc_id"]).column("doc_id").to_pylist())
    return done


def part_name(curated, issue: str) -> str:
    """<issue>, or <issue>.<n> when the issue already has parts (pages added to the targets later)."""
    if not curated.exists(f"{PREFIX}/pages/{issue}.parquet"):
        return issue
    n = 2
    while curated.exists(f"{PREFIX}/pages/{issue}.{n}.parquet"):
        n += 1
    return f"{issue}.{n}"


def claim(curated, issue: str, owner: str, stale: timedelta) -> bool:
    path = f"{PREFIX}/claims/{issue}.json"
    body = json.dumps({"owner": owner, "at": datetime.now(timezone.utc).isoformat()}).encode()
    if curated.create(path, body):
        return True
    if curated.take_over(path, body, stale):
        log("took over a stale claim", issue=issue)
        return True
    return False


# Progress for the status page (#139), in the reference store, which the
# API reads: the totals come from the part listing, so every replica writes
# the same numbers.
STATUS = "status/ocr-ja.json"
STATUS_EVERY = timedelta(minutes=2)


def alto_pages(curated) -> set[str]:
    """doc_ids with ALTO, from the listing alone."""
    return {Path(n).name.removesuffix(".xml") for n in curated.list(f"{PREFIX}/alto/") if n.endswith(".xml")}


def progress(curated, rows: list[dict]) -> dict:
    """Target and done pages and issues, from the ALTO listing alone: a page's
    ALTO is written just before its part, so a page with ALTO is done but for
    a part write that failed, which is close enough for a progress bar. An
    issue is done when all its target pages are."""
    with_alto = alto_pages(curated)
    per_issue: dict[str, list[bool]] = {}
    for r in rows:
        per_issue.setdefault(issue_key(r["lccn"], r["date"], r["edition"]), []).append(r["doc_id"] in with_alto)
    return {
        "targets": {"pages": len(rows), "issues": len(per_issue)},
        "done": {"pages": sum(sum(v) for v in per_issue.values()),
                 "issues": sum(all(v) for v in per_issue.values())},
    }


def write_status(reference, curated, rows: list[dict], started: datetime, done_at_start: int,
                 engine: str) -> dict:
    """Write the progress record, with a finish estimate from this run's pace."""
    p = progress(curated, rows)
    now = datetime.now(timezone.utc)
    gained = p["done"]["pages"] - done_at_start
    remaining = p["targets"]["pages"] - p["done"]["pages"]
    eta = None
    if gained > 0 and remaining > 0:
        eta = (now + (now - started) * (remaining / gained)).isoformat()
    body = {"schema": 1, "updated_at": now.isoformat(), "started_at": started.isoformat(),
            "engine": engine, **p, "eta": eta}
    try:
        reference.write(STATUS, json.dumps(body).encode())
    except Exception as e:  # the status page is not worth stopping the OCR for
        log("status write failed", error=f"{type(e).__name__}: {e}")
    return body


def run(reference, curated, ndl_root: Path, limit: int | None, owner: str,
        mixed_below: float = MIXED_BELOW) -> None:
    replicas = max(1, int(os.environ.get("JAOCR_REPLICAS", "1")))
    api = Pacer(float(os.environ.get("JAOCR_API_GAP", "3.5")) * replicas)
    tile = Pacer(float(os.environ.get("JAOCR_TILE_GAP", "1.0")) * replicas)
    stale = timedelta(hours=float(os.environ.get("JAOCR_CLAIM_HOURS", "6")))
    name = targets_name(mixed_below)
    tfile = f"{PREFIX}/{name}.jsonl"
    if curated.exists(tfile):
        rows = [json.loads(x) for x in curated.read(tfile).decode().splitlines() if x]
    else:
        rows = targets(reference, curated, mixed_below=mixed_below)
        curated.write(tfile, "\n".join(json.dumps(r) for r in rows).encode())
    parts = curated.list(f"{PREFIX}/pages/")
    done = done_pages(curated, parts) & alto_pages(curated)
    by_issue: dict[str, list[dict]] = {}
    for r in rows:
        if r["doc_id"] not in done:
            by_issue.setdefault(issue_key(r["lccn"], r["date"], r["edition"]), []).append(r)
    todo = sorted(by_issue)
    log("ocr starting", targets=len(rows), targets_file=tfile, done_pages=len(done), todo_issues=len(todo),
        todo_pages=sum(len(v) for v in by_issue.values()), replicas=replicas)
    engine = f"ndlocr-lite {ndlocr_version(ndl_root)}"
    started = datetime.now(timezone.utc)
    # Pages done before this run don't count toward its pace.
    done_at_start = progress(curated, rows)["done"]["pages"]
    write_status(reference, curated, rows, started, done_at_start, engine)
    last_status = started
    pages_done = 0
    for issue in todo:
        if datetime.now(timezone.utc) - last_status >= STATUS_EVERY:
            write_status(reference, curated, rows, started, done_at_start, engine)
            last_status = datetime.now(timezone.utc)
        if limit is not None and pages_done >= limit:
            break
        # Claims are per targets list: a new list's issues are claimed afresh.
        if not claim(curated, f"{name}/{issue}", owner, stale):
            continue
        try:
            pages_done += ocr_issue(curated, ndl_root, issue, by_issue[issue], api, tile, engine)
        except Exception as e:  # noqa: BLE001 - one issue's failure mustn't end the replica
            # No part: the issue is retried once its claim goes stale.
            log("issue failed", issue=issue, error=f"{type(e).__name__}: {e}"[:500])
    write_status(reference, curated, rows, started, done_at_start, engine)
    log("ocr finished", pages=pages_done)


def ocr_issue(curated, ndl_root: Path, issue: str, pages: list[dict], api: "Pacer", tile: "Pacer",
              engine: str) -> int:
    """OCR one claimed issue's target pages and write its part; the number of pages written.

    Writes nothing unless every page has text and ALTO: the issue is then retried once its claim goes stale.
    """
    p0 = pages[0]
    item_url = f"https://www.loc.gov/item/{p0['lccn']}/{p0['date']}/ed-{p0['edition']}/?fo=json"
    t0 = time.time()
    try:
        item = json.loads(fetch(item_url, api))
    except urllib.error.HTTPError as e:
        log("issue not found on loc.gov", issue=issue, status=e.code)
        return 0
    images, altos = page_images(item), page_altos(item)
    rows_out = []
    altos_out: dict[str, bytes] = {}
    with tempfile.TemporaryDirectory() as tmp:
        src, out = Path(tmp, "img"), Path(tmp, "out")
        src.mkdir()
        urls = {}
        for p in pages:
            base = images.get(p["seq"])
            if not base:
                log("no image for page", page=p["page_key"])
                continue
            urls[p["doc_id"]] = base
            (src / f"{p['doc_id']}.jpg").write_bytes(fetch(base + "/full/full/0/default.jpg", tile))
        if urls:
            ndlocr(ndl_root, src, out)
        now = datetime.now(timezone.utc)
        for p in pages:
            f, x = out / f"{p['doc_id']}.txt", out / f"{p['doc_id']}.xml"
            if p["doc_id"] not in urls or not f.exists() or not x.exists():
                continue
            text = normalize(f.read_text())
            geometry = loc_geometry(altos.get(p["seq"]), tile, p["page_key"])
            altos_out[p["doc_id"]] = alto.ndlocr_to_alto(
                x.read_bytes(), geometry, image_url=urls[p["doc_id"]] + "/full/full/0/default.jpg",
                engine=engine, seq=p["seq"], ocred_at=now)
            rows_out.append({
                **{k: p[k] for k in ("doc_id", "page_key", "lccn", "date", "edition", "seq", "batch", "loc_text")},
                "ocr_source": OCR_SOURCE, "ocr_engine": engine,
                "text_status": text_status(text), "text": text, "text_chars": len(text),
                "text_sha256": hashlib.sha256(text.encode()).digest(),
                "image_url": urls[p["doc_id"]], "ocred_at": now,
            })
    got = {r["doc_id"] for r in rows_out}
    missing = [p["page_key"] for p in pages if p["doc_id"] not in got]
    if missing:
        log("ocr incomplete; issue left for a later run", issue=issue, missing=missing[:10],
            missing_count=len(missing))
        return 0
    # ALTO first: a page counts as done only once both are written.
    for doc_id, xml in altos_out.items():
        curated.write(f"{PREFIX}/alto/{doc_id}.xml", xml)
    write_part(curated, part_name(curated, issue), rows_out)
    log("ocr issue done", issue=issue, pages=len(rows_out), secs=round(time.time() - t0, 1),
        chars=sum(r["text_chars"] for r in rows_out))
    return len(rows_out)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("command", choices=["targets", "run", "audit", "quality", "mixed", "american-stories", "american-stories-write",
                                        "damage-parity"])
    ap.add_argument("--limit", type=int, help="stop after about this many pages")
    ap.add_argument("--mixed-below", type=float,
                    help=f"targets, run: OCR pages whose LoC text is under this word-like share, a whole "
                         f"percentage (default JAOCR_MIXED_BELOW or {MIXED_BELOW}; {GARBLED_BELOW} or less: "
                         f"no mixed pages)")
    ap.add_argument("--all-languages", action="store_true", help="audit: every title, not just non-English ones")
    ap.add_argument("--sample-pct", type=float,
                    help="quality: percent of pages to score (default 2); american-stories: to compare (default 10)")
    ap.add_argument("--min-pages", type=int, default=50,
                    help="quality: scored pages a title or batch needs for the worst lists (default 50)")
    ap.add_argument("--year", type=int, action="append", help="american-stories: a year to compare (repeatable)")
    ap.add_argument("--diff", action="store_true",
                    help="american-stories: also measure indexing its text only where it differs from LoC's (#251)")
    ap.add_argument("--metric", choices=["v1", "v2"], default="v2",
                    help="quality: v2 (page language, function words, damage rate; default) or v1 (dict_share)")
    ap.add_argument("--pages", type=int, default=400, help="damage-parity: pages to sample (default 400)")
    ap.add_argument("--audit-version", default="pages-v20261003-1",
                    help="damage-parity: the index version of the 10%% audit to sample from")
    a = ap.parse_args()
    reference = store(os.environ["USNM_REFERENCE_URL"])
    curated = store(os.environ["USNM_CURATED_URL"])
    if a.command == "audit":
        import audit

        audit.audit(reference, curated, a.all_languages)
    elif a.command == "american-stories-write":
        import american_stories_write

        american_stories_write.write(reference, curated, a.year or None)
    elif a.command == "american-stories":
        import american_stories

        pct = 10.0 if a.sample_pct is None else a.sample_pct
        american_stories.american_stories(reference, curated, a.year or [], pct, diff=a.diff)
    elif a.command == "damage-parity":
        import damage_parity

        damage_parity.damage_parity(reference, curated, a.pages, a.audit_version)
    elif a.command == "mixed":
        import mixed

        mixed.mixed(reference, curated)
    elif a.command == "quality":
        import quality

        quality.quality(reference, curated, 2.0 if a.sample_pct is None else a.sample_pct, a.min_pages, metric=a.metric)
    elif a.command == "targets":
        below = mixed_below_setting(a.mixed_below)
        rows = targets(reference, curated, mixed_below=below)
        curated.write(f"{PREFIX}/{targets_name(below)}.jsonl", "\n".join(json.dumps(r) for r in rows).encode())
        issues = {issue_key(r["lccn"], r["date"], r["edition"]) for r in rows}
        why: dict[str, int] = {}
        for r in rows:
            why[r["loc_text"]] = why.get(r["loc_text"], 0) + 1
        log("targets written", pages=len(rows), issues=len(issues), by_loc_text=why)
    else:
        ndl = Path(os.environ.get("NDLOCR_ROOT", "/opt/ndlocr-lite"))
        owner = os.environ.get("CONTAINER_APP_REPLICA_NAME") or os.environ.get("HOSTNAME") or "local"
        run(reference, curated, ndl, a.limit, owner, mixed_below_setting(a.mixed_below))


if __name__ == "__main__":
    main()
