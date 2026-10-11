#!/usr/bin/env python3
"""Capture what a fixed set of searches returns on the live index version,
and compare two captures (#161).

    scripts/compare-versions.py capture [--base-url URL] > ops/version-snapshots/<version>.json
    scripts/compare-versions.py diff --before A.json --after B.json

A capture's format is ops/version-snapshots/schema.json; CI checks committed
captures against it (scripts/check-version-snapshots.py).

The API serves one index version at a time, so versions are compared through
captures: run `capture` while a version is live (before a release replaces
it), keep the file, and `diff` it with a capture of the next version.

The searches: the ten benchmark searches of scripts/load-cold-searches.py
(05 §5.8), the home page's examples (web/src/examples.json), and a few
Japanese ones (a version without a Japanese index answers those 422). Each
is asked as the web app asks: `202 Accepted` and `503 busy` are waited out,
up to 150 s. Run it at a quiet hour: a search no cache holds is computed in
full, one at a time.

A capture records, per search: the HTTP status, `total` (hits, places,
papers, days, baseline pages, first and last day), the per-bucket hits, the
hits by language, the backend's timing (`timing_ms`, from when the result
was computed, which may predate the capture if a cache answered) and the
wall time. `diff` reports hits and timing side by side and flags searches
whose hits changed by more than --threshold (default 1%), or whose status or
query changed (a search that kept its name but changed its query isn't
compared). A capture stops if the API stops serving its version partway
(redirects aren't followed); 202, 503 busy and 429 are waited out, and a
dropped or cut-off response is asked again, all within the 150 s.
"""

import argparse
import http.client
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
USER_AGENT = "usnm-compare/1 (scripts/compare-versions.py)"
MAX_WAIT_S = 150
REQUEST_WAIT_S = 12

# The benchmark searches (scripts/load-cold-searches.py), with their windows.
BENCH = [
    ("bench: cross of gold 1896", "q=cross+of+gold&mode=phrase&from=1896-01-01&to=1896-12-31"),
    ("bench: yellow jack 1878", "q=yellow+jack&mode=phrase&from=1878-01-01&to=1878-12-31"),
    ("bench: scalawag", "q=scalawag&mode=phrase"),
    ("bench: miscegenation", "q=miscegenation&mode=phrase"),
    ("bench: influenza 1918", "q=influenza&mode=phrase&from=1918-01-01&to=1918-12-31"),
    ("bench: railroad", "q=railroad&mode=phrase"),
    ("bench: lincoln", "q=lincoln&mode=phrase"),
    ("bench: gold near silver", "q=gold+silver&mode=near&near=5"),
    ("bench: influenza GA front", "q=influenza&mode=phrase&state=GA&front=true"),
    ("bench: yellow fever by month", "q=yellow+fever&mode=phrase&bucket=month"),
]
JAPANESE = [
    ("ja: 日本", "q=" + urllib.parse.quote("日本")),
    ("ja: 真珠湾", "q=" + urllib.parse.quote("真珠湾")),
    ("ja: 収容所", "q=" + urllib.parse.quote("収容所")),
]


def searches():
    examples = json.loads((ROOT / "web/src/examples.json").read_text())
    return BENCH + [(f"example: {e['id']}", e["aggregate"]) for e in examples] + JAPANESE


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Don't follow redirects: the API redirects a request for a version it no
    longer serves to the new one (307), which would mix versions in a capture."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = urllib.request.build_opener(NoRedirect)


def get(url, timeout):
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
    try:
        with OPENER.open(req, timeout=timeout) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers, e.read()


def ask(base, path):
    """(status, body, seconds), waiting out 202 and 503 busy as the web app does."""
    started = time.monotonic()
    while True:
        left = MAX_WAIT_S - (time.monotonic() - started)
        try:
            status, headers, body = get(base + path, max(left, 1))
        except (TimeoutError, ConnectionError, urllib.error.URLError, http.client.HTTPException):
            # A dropped connection: the search may still be computing (and
            # cached when done), so ask again while there's time left.
            if time.monotonic() - started + 2 + REQUEST_WAIT_S > MAX_WAIT_S:
                return "gave up", None, time.monotonic() - started
            time.sleep(2)
            continue
        try:
            doc = json.loads(body)
        except ValueError:
            doc = None
        busy = status == 503 and isinstance(doc, dict) and doc.get("type") == "/errors/busy"
        # A rate limit is waited out too: it says nothing about the version.
        if status not in (202, 429) and not busy:
            return status, doc, time.monotonic() - started
        wait = int(headers.get("Retry-After") or 2)
        if time.monotonic() - started + wait + REQUEST_WAIT_S > MAX_WAIT_S:
            return "gave up", None, time.monotonic() - started
        time.sleep(wait)


def capture(base):
    with urllib.request.urlopen(urllib.request.Request(f"{base}/v1/meta", headers={"User-Agent": USER_AGENT}),
                                timeout=30) as r:
        meta = json.load(r)
    version = meta["index_version"]
    out = {"base": base, "captured_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
           "index_version": version, "searches": {}}
    for name, query in searches():
        status, doc, secs = ask(base, f"/v1/aggregate?{query}&v={urllib.parse.quote(version)}")
        if status in (301, 302, 303, 307, 308):
            sys.exit(f"the API no longer serves {version} (it redirected `{name}`); start again")
        rec = {"query": query, "status": status, "wall_s": round(secs, 2)}
        if status == 200 and isinstance(doc, dict):
            if doc.get("index_version") not in (None, version):
                sys.exit(f"the API moved to {doc['index_version']} during the capture; start again")
            rec.update({
                "total": doc.get("total", {}) | {"first": None, "last": None},
                "bucket": doc.get("bucket"),
                "series_hits": (doc.get("series") or {}).get("hits"),
                "languages": dict(zip((doc.get("languages") or {}).get("code", []),
                                      (doc.get("languages") or {}).get("hits", []))),
                "timing_ms": doc.get("timing_ms"),
            })
            rec["total"] = {k: v for k, v in rec["total"].items() if v is not None}
        elif isinstance(doc, dict):
            rec["error"] = {k: doc.get(k) for k in ("type", "title", "detail") if doc.get(k)}
        out["searches"][name] = rec
        print(f"{name}: {status} {rec.get('total', {}).get('hits', '')} ({secs:.1f} s)", file=sys.stderr)
    json.dump(out, sys.stdout, indent=1, ensure_ascii=False, sort_keys=True)
    print()


def diff(a_path, b_path, threshold):
    a, b = (json.loads(Path(p).read_text()) for p in (a_path, b_path))
    print(f"A: {a['index_version']} (captured {a['captured_at']})")
    print(f"B: {b['index_version']} (captured {b['captured_at']})\n")
    print(f"{'search':<44} {'hits A':>12} {'hits B':>12} {'change':>8}  {'backend ms A':>12} {'B':>8}")
    flagged = 0
    for name in list(a["searches"]) + [n for n in b["searches"] if n not in a["searches"]]:
        ra, rb = a["searches"].get(name, {}), b["searches"].get(name, {})
        if ra and rb and ra.get("query") != rb.get("query"):
            # Same name, different search (an example was edited): not comparable.
            flagged += 1
            print(f"*{name[:43]:<43} {'query changed between captures; not compared':>50}")
            continue
        ha, hb = (r.get("total", {}).get("hits") for r in (ra, rb))
        ta, tb = ((r.get("timing_ms") or {}).get("backend") for r in (ra, rb))
        if ha is None or hb is None:
            change = f"{ra.get('status', '-')}→{rb.get('status', '-')}"
            mark = "*" if ra.get("status") != rb.get("status") else " "
        else:
            pct = (hb - ha) / ha * 100 if ha else (0.0 if hb == 0 else float("inf"))
            change = f"{pct:+.1f}%"
            mark = "*" if abs(pct) > threshold else " "
        flagged += mark == "*"
        fmt = lambda x: "-" if x is None else f"{x:,}"
        print(f"{mark}{name[:43]:<43} {fmt(ha):>12} {fmt(hb):>12} {change:>8}  {fmt(ta):>12} {fmt(tb):>8}")
    print(f"\n{flagged} flagged (* hits changed by more than {threshold}%, or the status or query changed).")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("capture")
    c.add_argument("--base-url", help="the site to capture (default https://usnewsmap.com)")
    d = sub.add_parser("diff")
    d.add_argument("--before", help="the earlier capture")
    d.add_argument("--after", help="the later capture")
    d.add_argument("--threshold", type=float, default=1.0)
    # The positional forms (`capture URL`, `diff A B`) work for one release,
    # with a warning (#277).
    c.add_argument("legacy", nargs="?", help=argparse.SUPPRESS)
    d.add_argument("legacy", nargs="*", help=argparse.SUPPRESS)
    a = ap.parse_args()
    if a.cmd == "capture":
        if a.legacy is not None:
            if a.base_url is not None:
                c.error("give the base URL once, as --base-url")
            print("warning: a positional base URL is deprecated; use --base-url (#277)", file=sys.stderr)
            a.base_url = a.legacy
        capture((a.base_url or "https://usnewsmap.com").rstrip("/"))
        return
    if a.legacy:
        if len(a.legacy) != 2 or a.before is not None or a.after is not None:
            d.error("name the captures as --before A --after B")
        print("warning: `diff A B` is deprecated; use --before A --after B (#277)", file=sys.stderr)
        a.before, a.after = a.legacy
    if a.before is None or a.after is None:
        d.error("--before and --after are required")
    diff(a.before, a.after, a.threshold)


if __name__ == "__main__":
    main()
