"""Fetch the files for the JP2 vs JPEG comparison, on an OCR node (#296, #368).

For each sampled page it puts three files in <dir>/<n>/ (n = line number in
the sample file):

  page.jpg   the full IIIF JPEG
  page.jp2   the original JP2 from the batch bag
  loc.xml    LoC's own ALTO for the page, from the same bag

A file the scan server in CT 211 already holds (the keep set) comes from
there. Everything else comes from LoC's tile.loc.gov, one request at a time,
GAP seconds apart (default 5, the loc.gov crawl delay; the storage service's
own gap is 1 s). Any 403, 429 or 5xx stops the run at once: nothing retries
past a block. A 404 for the ALTO is recorded and the page is left without
reference text.
"""

import argparse
import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

UA = "usnewsmap-jp2-comparison/0.1 (+https://usnewsmap.com; #296)"


class Blocked(Exception):
    pass


def get(url, headers=None):
    """GET with two retries on a dropped connection (never on an HTTP status)."""
    for attempt in range(3):
        req = urllib.request.Request(url, headers={"User-Agent": UA, **(headers or {})})
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, b""
        except Exception:  # IncompleteRead, timeout, reset
            time.sleep(10)
    return 0, b""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sample", required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--scan-server", required=True)
    ap.add_argument("--gap-secs", type=float, default=5.0)
    ap.add_argument("--limit", type=int, default=0)
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.sample)]
    last = [0.0]
    stats = {"loc_requests": 0, "scan_server": 0, "bytes_loc": 0}

    def loc(url):
        wait = last[0] + a.gap_secs - time.time()
        if wait > 0:
            time.sleep(wait)
        status, body = get(url)
        last[0] = time.time()
        stats["loc_requests"] += 1
        stats["bytes_loc"] += len(body)
        if status in (403, 429) or status >= 500:
            raise Blocked(f"{status} for {url}")
        return status, body

    log = open(os.path.join(a.dir, "fetch.log"), "a")
    try:
        for n, r in enumerate(rows, 1):
            if a.limit and n > a.limit:
                break
            d = os.path.join(a.dir, str(n))
            os.makedirs(d, exist_ok=True)
            frame = r["iiif_base"].rsplit(":", 1)[1]
            base = r["storage_base"].rsplit("/", 1)[0]  # the issue directory
            sub = f"{r['batch']}/{r['lccn']}/{r['date']}/{r['edition']}/{r['seq']}"
            want = {
                "page.jpg": ("local", r["local_jpg"]) if r.get("local_jpg") else ("scan" if r["have"] == "keep" and r["have_format"] == "jpeg" else "loc",
                             r["iiif_base"] + "/full/full/0/default.jpg"),
                "page.jp2": ("local", r["local_jp2"]) if r.get("local_jp2") else ("scan" if r["have"] == "keep" and r["have_format"] == "jp2" else "loc",
                             f"{base}/{frame}.jp2"),
                "loc.xml": ("loc", f"{base}/{frame}.xml"),
            }
            for name, (src, url) in want.items():
                path = os.path.join(d, name)
                if os.path.exists(path):
                    continue
                if src == "local":
                    status, body = (200, open(url, "rb").read()) if os.path.exists(url) else (404, b"")
                elif src == "scan":
                    status, body = get(f"{a.scan_server}/scans/keep/{sub}.{name.split('.')[1]}")
                    stats["scan_server"] += 1
                else:
                    status, body = loc(url)
                ok = status == 200 and body
                if ok and name == "page.jp2" and body[:12] != b"\x00\x00\x00\x0cjP  \r\n\x87\n":
                    ok = False
                if ok and name == "page.jpg" and body[:2] != b"\xff\xd8":
                    ok = False
                rec = {"n": n, "doc_id": r["doc_id"], "file": name, "source": src, "status": status,
                       "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest() if ok else None,
                       "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
                log.write(json.dumps(rec) + "\n")
                log.flush()
                if ok:
                    with open(path + ".part", "wb") as f:
                        f.write(body)
                    os.rename(path + ".part", path)
                else:
                    open(path + ".missing", "w").write(str(status))
            open(os.path.join(d, "ready"), "w").write("1")
    except Blocked as e:
        print("STOPPED: " + str(e), file=sys.stderr)
        open(os.path.join(a.dir, "HOLD"), "w").write(str(e))
        sys.exit(3)
    print(json.dumps(stats))


if __name__ == "__main__":
    main()
