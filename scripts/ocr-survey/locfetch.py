"""Polite fetches from loc.gov and tile.loc.gov for the engine survey (#296).

loc.gov's API is paced API_GAP seconds per request and the image server
TILE_GAP seconds, as the Japanese job does (ja-ocr/jaocr.py). A 429 waits
for its Retry-After (or a minute, longer on each try). A 403 stops the run:
it is usually a challenge page, and the survey does not work around one.
"""

from __future__ import annotations

import http.client
import os
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "ja-ocr"))

from jaocr import Pacer, iiif_base, page_altos, page_images  # noqa: E402,F401

UA = "usnewsmap-ocr-survey/1.0 (+https://usnewsmap.com)"
API_GAP = 3.5
TILE_GAP = 1.0
API = Pacer(API_GAP)
TILE = Pacer(TILE_GAP)


class Blocked(RuntimeError):
    """loc.gov answered 403: stop and report, don't retry."""


def fetch(url: str, pacer: Pacer, attempts: int = 4) -> bytes:
    for i in range(attempts):
        pacer.wait()
        try:
            req = urllib.request.Request(url, headers={"User-Agent": UA, "DNT": "1"})
            with urllib.request.urlopen(req, timeout=180) as r:
                return r.read()
        except urllib.error.HTTPError as e:
            if e.code == 403:
                raise Blocked(f"403 from {url}") from e
            if e.code == 404:
                raise
            wait = 60 * (i + 1)
            if e.code in (429, 503):
                wait = max(wait, int(e.headers.get("Retry-After") or 0))
            print(f"retry {url[:120]}: HTTP {e.code}, waiting {wait}s", file=sys.stderr, flush=True)
            time.sleep(wait)
        except (urllib.error.URLError, http.client.HTTPException, OSError) as e:
            wait = 60 * (i + 1)
            print(f"retry {url[:120]}: {e}, waiting {wait}s", file=sys.stderr, flush=True)
            time.sleep(wait)
    raise RuntimeError(f"giving up on {url}")
