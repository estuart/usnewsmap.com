#!/usr/bin/env python3
"""Run cold searches at rising concurrency and report how the API copes.

    scripts/load-cold-searches.py [--base-url URL] [--levels 1,2,4,10] [--pause 30] [-v]

The default base URL is http://localhost:8080. Run it against staging; run it
against production only at a quiet hour, once, since every search it sends
is a cold one that the searcher must compute in full.

The same ten searches, a mix of the benchmark classes in 05 §5.8 (rare
phrase, mid-frequency term, high-frequency term, proximity, filtered), run
once per concurrency level: a pool of N workers drains the ten. Each request
waits out `202 Accepted` and `503 busy` as the web app does (06 §6.3.5), so
the time to a result includes any time queued for a computation slot.

"Cold" means cold for the API: every pass shifts each search's date window
back by one more day, so the canonical request, and with it the cache key,
is new each time, and neither the in-process nor the persistent cache can
answer it. The shift keeps a window's length (a whole-corpus window loses
its last days, since the API clamps dates to the corpus), so every pass
searches the same workload. The whole run starts from a random offset of up
to 90 days (`--offset` sets it), so a second run doesn't meet the first
run's cached results. Quickwit's own caches
are a different matter: split footers and fast fields stay warm across
passes. So the first pass, at concurrency 1, is reported apart as "first"
(the searcher's cold start); the levels after it, including another pass at
concurrency 1, share the same warm-searcher conditions and compare fairly.
Each search the script runs leaves an entry in the persistent response cache
for this index version.

How to read the result: `speedup` is a level's throughput (searches per
second) over the concurrency-1 pass. When a cold search mostly waits on Blob
reads, running more at once overlaps that waiting, and speedup grows with
concurrency up to the API's slots (`USNM_COMPUTE_CONCURRENCY`); raising
the slot count would then help. When it mostly computes, speedup stops near
the searcher's vCPU count (2), and only more CPU, or cheaper searches,
helps. `ahead` is the most searches the API reported queued ahead of one of
ours; `busy` counts `503 busy` answers (a full queue), which the script
waits out like the web app.

The API rate-limits each client address (120 requests a minute, bursts of
40, by default); the script pauses between levels. Like the web app, it
waits out a `429` met while waiting on a search (counted in `429s`), and
counts one on a search's first request as that search failing. Its requests are not in the search log: they send
no Origin or Sec-Fetch-Site header and a script's user agent (`admit` in
crates/usnm-api/src/searchlog.rs). They do show in request telemetry.
"""

import argparse
import concurrent.futures
import datetime
import json
import random
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

# The benchmark classes of 05 §5.8, without the pathological ones. `window`
# is (from, to) or None for the whole corpus.
SEARCHES = [
    ({"q": "cross of gold", "mode": "phrase"}, ("1896-01-01", "1896-12-31")),
    ({"q": "yellow jack", "mode": "phrase"}, ("1878-01-01", "1878-12-31")),
    ({"q": "scalawag", "mode": "phrase"}, None),
    ({"q": "miscegenation", "mode": "phrase"}, None),
    ({"q": "influenza", "mode": "phrase"}, ("1918-01-01", "1918-12-31")),
    ({"q": "railroad", "mode": "phrase"}, None),
    ({"q": "lincoln", "mode": "phrase"}, None),
    ({"q": "gold silver", "mode": "near", "near": "5"}, None),
    ({"q": "influenza", "mode": "phrase", "state": "GA", "front": "true"}, None),
    ({"q": "yellow fever", "mode": "phrase", "bucket": "month"}, None),
]

# Give up on one search after this long, as the web app does (150 s).
MAX_WAIT_S = 150
# The longest one request waits on the API before a `202` (10 s, plus 2 s
# for its persistent cache), as in web/src/api/client.ts.
REQUEST_WAIT_S = 12
USER_AGENT = "usnm-load/1 (scripts/load-cold-searches.py)"


def get(url, timeout=MAX_WAIT_S):
    """(status, headers, body) for one GET, without raising on 4xx/5xx."""
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers, e.read()


def json_of(body):
    try:
        return json.loads(body)
    except ValueError:
        return {}


class Result:
    def __init__(self, name):
        self.name = name
        self.status = None
        self.seconds = 0.0
        self.accepted = 0
        self.busy = 0
        self.limited = 0
        self.ahead = None
        self.backend_ms = None


def run(base, path, name):
    """Ask as the web app does until there is an answer or the wait runs out:
    each request is held to what is left of the wait, and no request is
    started that could outlast it."""
    r = Result(name)
    started = time.monotonic()
    while True:
        try:
            status, headers, body = get(base + path, max(MAX_WAIT_S - (time.monotonic() - started), 1))
        except (TimeoutError, urllib.error.URLError) as e:
            if isinstance(e, urllib.error.URLError) and not isinstance(e.reason, TimeoutError):
                raise
            r.status = "gave up"
            r.seconds = time.monotonic() - started
            return r
        doc = json_of(body)
        if status == 202:
            r.accepted += 1
            if doc.get("status") == "queued":
                r.ahead = max(r.ahead or 0, doc.get("ahead", 0))
        elif status == 503 and doc.get("type") == "/errors/busy":
            r.busy += 1
        elif status == 429 and (r.accepted or r.busy):
            # Like the web app, a rate limit is waited out only once the
            # search is under way; before that it is the answer.
            r.limited += 1
        else:
            r.status = status
            r.seconds = time.monotonic() - started
            r.backend_ms = (doc.get("timing_ms") or {}).get("backend")
            return r
        wait = int(headers.get("Retry-After") or 2)
        # The next request may itself wait up to REQUEST_WAIT_S for an answer.
        if time.monotonic() - started + wait + REQUEST_WAIT_S > MAX_WAIT_S:
            r.status = "gave up"
            r.seconds = time.monotonic() - started
            return r
        time.sleep(wait)


class Paths:
    """A new canonical request for each search on each pass."""

    def __init__(self, bounds, version, offset):
        self.lo = datetime.date.fromisoformat(bounds["from"])
        self.hi = datetime.date.fromisoformat(bounds["to"])
        self.version = version
        self.offset = offset
        self.passes = 0

    def next_pass(self):
        """Requests for every search, shifted one day further than the last pass."""
        self.passes += 1
        shift = datetime.timedelta(days=self.offset + self.passes)
        return [self.make(p, w, shift) for p, w in SEARCHES]

    def make(self, params, window, shift):
        start, end = (self.lo, self.hi) if window is None else map(datetime.date.fromisoformat, window)
        start, end = max(start, self.lo), min(end, self.hi)
        if end - start < datetime.timedelta(days=30):
            # The window is outside this corpus (the fixtures cover 1895-1897).
            start, end = self.lo, self.hi
        start, end = max(start - shift, self.lo), end - shift
        query = {**params, "from": start.isoformat(), "to": end.isoformat(), "v": self.version}
        return "/v1/aggregate?" + urllib.parse.urlencode(query)


def name_of(params, window):
    extra = "".join(f" {k}={v}" for k, v in params.items() if k not in ("q", "mode"))
    return f"{params['q']}{extra}" + (f" ({window[0][:4]})" if window else "")


def level(base, paths, concurrency, verbose):
    jobs = list(zip(paths.next_pass(), (name_of(p, w) for p, w in SEARCHES)))
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        results = list(pool.map(lambda j: run(base, *j), jobs))
    wall = time.monotonic() - started
    if verbose:
        for r in results:
            backend = f"{r.backend_ms / 1000:.2f}" if r.backend_ms is not None else "-"
            print(
                f"    {r.name:<36} {r.status!s:>7} {r.seconds:7.2f} s  backend {backend:>6} s  "
                f"202s {r.accepted}  ahead {r.ahead if r.ahead is not None else '-'}  busy {r.busy}"
            )
    return wall, results


def summary(label, wall, results, baseline):
    ok = [r for r in results if r.status == 200]
    times = sorted(r.seconds for r in results)
    backend = [r.backend_ms / 1000 for r in ok if r.backend_ms is not None]
    throughput = len(ok) / wall if wall else 0.0
    speedup = f"{throughput / baseline:.2f}" if baseline else "-"
    ahead = max((r.ahead for r in results if r.ahead is not None), default=None)
    print(
        f"{label:<7} {wall:7.1f} {throughput:8.3f} {speedup:>7} "
        f"{statistics.median(times):7.2f} {times[-1]:7.2f} "
        f"{statistics.mean(backend) if backend else 0:9.2f} "
        f"{sum(r.accepted for r in results):5} {ahead if ahead is not None else '-':>5} "
        f"{sum(r.busy for r in results):5} {sum(r.limited for r in results):5} "
        f"{len(results) - len(ok):6}"
    )
    return throughput


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--base-url", help="the API to load (default http://localhost:8080)")
    # The positional form (`load-cold-searches.py URL`) works for one
    # release, with a warning (#277).
    ap.add_argument("legacy_base_url", nargs="?", help=argparse.SUPPRESS)
    ap.add_argument("--levels", default="1,2,4,10", help="concurrency levels, comma-separated")
    ap.add_argument("--pause", type=float, default=30, help="seconds between levels (rate limit)")
    ap.add_argument("--offset", type=int, help="days every window starts shifted back (default: random, 0-90)")
    ap.add_argument("-v", "--verbose", action="store_true", help="print every search")
    args = ap.parse_args()
    if args.legacy_base_url is not None:
        if args.base_url is not None:
            ap.error("give the base URL once, as --base-url")
        print("warning: a positional base URL is deprecated; use --base-url (#277)", file=sys.stderr)
        args.base_url = args.legacy_base_url
    base = (args.base_url or "http://localhost:8080").rstrip("/")
    levels = [int(n) for n in args.levels.split(",")]

    status, _, body = get(base + "/v1/meta")
    if status != 200:
        raise SystemExit(f"{base}/v1/meta answered {status}")
    meta = json.loads(body)
    offset = args.offset if args.offset is not None else random.randint(0, 90)
    paths = Paths(meta["bounds"], meta["index_version"], offset)
    print(
        f"{base}, index version {meta['index_version']}, {len(SEARCHES)} cold searches per level, "
        f"windows shifted back {offset} days plus one per pass"
    )
    print(
        f"{'level':<7} {'wall s':>7} {'per s':>8} {'speedup':>7} {'p50 s':>7} {'max s':>7} "
        f"{'backend s':>9} {'202s':>5} {'ahead':>5} {'busy':>5} {'429s':>5} {'failed':>6}"
    )
    # The searcher's cold start, reported apart (see the module doc).
    wall, results = level(base, paths, 1, args.verbose)
    summary("first", wall, results, None)
    baseline = None
    for n in levels:
        time.sleep(args.pause)
        wall, results = level(base, paths, n, args.verbose)
        throughput = summary(str(n), wall, results, baseline)
        if n == 1:
            baseline = throughput
    if baseline is None:
        print("(add 1 to --levels for the speedup column)")


if __name__ == "__main__":
    main()
