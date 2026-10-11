#!/usr/bin/env python3
"""Time a few known-slow searches, cold then warm, one request at a time.

    scripts/bench-cold-searches.py [--base-url URL]    (default https://usnewsmap.com)

For each search it asks /v1/aggregate as the web app does, waits out each
`202 Accepted` for its Retry-After (06 §6.3.5), and records the time to the
result and the backend time the API reports (`timing_ms.backend`, which is
the time of the computation that produced the body, even when the body now
comes from a cache). Then it asks once more for the same search, which the
API now answers from its in-process cache. That is one pass. Run it once
against production, not in a loop; the numbers are for deciding whether
the searcher needs more compute or a split cache.

"Cold" means not in the API's caches when the pass starts. A search that a
visitor or an earlier pass already ran may be in the persistent cache
(kept for the index version's lifetime) or warm in Quickwit's own caches,
and then comes back fast: when the first answer took under 1 s with no 202,
the first column adds "cached" (a guess).

These requests are not in the search log: the API records a search only
when the request comes from the site's own pages (an Origin of the site or
Sec-Fetch-Site: same-origin) and from a browser, and this script sends
neither header and a script's user agent (`admit` in
crates/usnm-api/src/searchlog.rs). They do show in request telemetry.
"""

import argparse
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

# Whole corpus, by month.
SEARCHES = [
    {"q": "radio", "mode": "phrase", "bucket": "month"},
    {"q": "television", "mode": "phrase", "bucket": "month"},
    {"q": "yellow fever", "mode": "phrase", "bucket": "month"},
]

# Give up on one search after this long (the API's own limit is 120 s).
MAX_WAIT_S = 180
USER_AGENT = "usnm-bench/1 (scripts/bench-cold-searches.py)"


def get(url):
    """(status, headers, body) for one GET, without raising on 4xx/5xx."""
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=MAX_WAIT_S) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers, e.read()


def run(base, path):
    """Ask until the answer isn't a 202. Returns (status, seconds, 202 count, body)."""
    started = time.monotonic()
    accepted = 0
    while True:
        status, headers, body = get(base + path)
        if status != 202:
            return status, time.monotonic() - started, accepted, body
        accepted += 1
        wait = int(headers.get("Retry-After") or 2)
        if time.monotonic() - started + wait > MAX_WAIT_S:
            return 202, time.monotonic() - started, accepted, body
        time.sleep(wait)


def backend_ms(body):
    try:
        return json.loads(body)["timing_ms"]["backend"]
    except (ValueError, KeyError, TypeError):
        return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--base-url", help="the site to time (default https://usnewsmap.com)")
    # The positional form (`bench-cold-searches.py URL`) works for one
    # release, with a warning (#277).
    ap.add_argument("legacy_base_url", nargs="?", help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.legacy_base_url is not None:
        if a.base_url is not None:
            ap.error("give the base URL once, as --base-url")
        print("warning: a positional base URL is deprecated; use --base-url (#277)", file=sys.stderr)
        a.base_url = a.legacy_base_url
    base = (a.base_url or "https://usnewsmap.com").rstrip("/")
    _, _, meta = get(base + "/v1/meta")
    version = json.loads(meta)["index_version"]
    print(f"{base}, index version {version}")
    print(f"{'search':<28} {'first: status':>13} {'s':>7} {'202s':>5} {'backend ms':>11}   {'again: s':>8}")
    for s in SEARCHES:
        path = "/v1/aggregate?" + urllib.parse.urlencode({**s, "v": version})
        status, secs, accepted, body = run(base, path)
        cold_backend = backend_ms(body)
        # A fast first answer with no 202 came from a cache.
        label = f"{status}" if accepted or secs > 1 else f"{status} cached"
        again_status, again_secs, _, _ = run(base, path)
        again = f"{again_secs:8.2f}" if again_status == 200 else f"{again_status:>8}"
        name = f"{s['q']} ({s['bucket']})"
        print(
            f"{name:<28} {label:>13} {secs:7.2f} {accepted:>5} "
            f"{cold_backend if cold_backend is not None else '-':>11}   {again}"
        )
        # One search at a time, with a pause between them.
        time.sleep(2)


if __name__ == "__main__":
    main()
