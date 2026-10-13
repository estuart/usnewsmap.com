"""Run the worker's `ocr-file` on both formats of each fetched page (#296).

One page at a time, at the lowest CPU and IO priority, as the `usnm` user with
the worker's own engine settings (the engine image, the Podman socket and the
scratch tmpfs from /etc/usnm/ocr-worker.env), so the profile is the one the
worker runs: `frak2021` for German, `--psm 3 --dpi 300`, one thread, 1536 MiB.
The order of the two formats alternates from page to page, so neither always
runs first. `ocr-file` reports the seconds of its engine call, which is what
`secs` holds. `wall` is the whole process, including starting the container.

A file named STOP in <dir> ends the run after the page in progress.
"""

import argparse
import json
import os
import subprocess
import time

WORKER = "/usr/local/bin/usnm-ocr-worker"


def read_env(path):
    env = {}
    text = subprocess.run(["sudo", "-n", "cat", path], capture_output=True, text=True, check=True).stdout
    for line in text.splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            k, v = line.split("=", 1)
            env[k] = v.strip('"')
    return env


def tesseract_pids():
    out = subprocess.run(["pgrep", "-x", "tesseract"], capture_output=True, text=True).stdout
    return {int(x) for x in out.split()}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sample", required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--env-file", default="/etc/usnm/ocr-worker.env")
    ap.add_argument("--node", required=True)
    ap.add_argument("--part", type=int, default=0, help="with --parts: do pages where (n %% parts) == part")
    ap.add_argument("--parts", type=int, default=1)
    ap.add_argument("--results", required=True)
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.sample)]
    env = read_env(a.env_file)
    keep = {k: env[k] for k in ("USNM_OCR_TESSERACT_IMAGE", "USNM_OCR_PAGE_TIMEOUT_SECS") if k in env}
    keep.update({
        "USNM_OCR_PODMAN_URL": "unix:///run/usnm-ocr/podman.sock",
        "USNM_OCR_SCRATCH_DIR": "/run/usnm-ocr/scratch",
        "HOME": "/var/lib/usnm-ocr",
        "XDG_RUNTIME_DIR": "/run/usnm-ocr-worker",
    })
    done = set()
    if os.path.exists(a.results):
        for l in open(a.results):
            r = json.loads(l)
            done.add((r["n"], r["fmt"]))
    out = open(a.results, "a")
    for n, r in enumerate(rows, 1):
        if n % a.parts != a.part:
            continue
        d = os.path.join(a.dir, str(n))
        while not os.path.exists(os.path.join(d, "ready")):
            if os.path.exists(os.path.join(a.dir, "STOP")) or os.path.exists(os.path.join(a.dir, "HOLD")):
                return
            time.sleep(10)
        fmts = ["jp2", "jpg"] if n % 2 else ["jpg", "jp2"]
        for fmt in fmts:
            img = os.path.join(d, f"page.{fmt}")
            if (n, fmt) in done or not os.path.exists(img):
                continue
            if os.path.exists(os.path.join(a.dir, "STOP")):
                return
            od = os.path.join(d, f"out-{fmt}")
            os.makedirs(od, exist_ok=True)
            subprocess.run(["chmod", "-R", "a+rwX", d], check=False)
            cmd = ["sudo", "-n", "-u", "usnm", "nice", "-n", "19", "ionice", "-c", "3", "env"]
            cmd += [f"{k}={v}" for k, v in keep.items()]
            cmd += [WORKER, "ocr-file", "--image", img, "--language", r["lang"], "--out", od]
            load = open("/proc/loadavg").read().split()[0]
            before = tesseract_pids()
            t0 = time.time()
            proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            # The engine runs in a container under another account, so `nice` on
            # the client doesn't reach it: lower the new tesseract process too.
            size = os.path.getsize(img)
            lowered = False
            peak_kib = 0
            tpid = None
            for _ in range(90):
                time.sleep(1)
                for pid in tesseract_pids() - before:
                    # Only the process that has this scan open as its input (the
                    # workers' own pages are other files).
                    st = subprocess.run(["sudo", "-n", "sh", "-c", f"stat -c %s /proc/{pid}/root/work/input.*"],
                                        capture_output=True, text=True).stdout.split()
                    if str(size) in st:
                        subprocess.run(["sudo", "-n", "renice", "-n", "19", "-p", str(pid)], capture_output=True)
                        subprocess.run(["sudo", "-n", "ionice", "-c", "3", "-p", str(pid)], capture_output=True)
                        lowered = True
                        tpid = pid
                if lowered or proc.poll() is not None:
                    break
            # Peak resident memory: the tesseract process's high-water mark, read
            # every few seconds until the worker returns (the last read stands).
            while tpid and proc.poll() is None:
                st = subprocess.run(["sudo", "-n", "grep", "VmHWM", f"/proc/{tpid}/status"],
                                    capture_output=True, text=True).stdout.split()
                if len(st) >= 2 and st[1].isdigit():
                    peak_kib = max(peak_kib, int(st[1]))
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
            stdout, stderr = proc.communicate()
            wall = time.time() - t0
            p = subprocess.CompletedProcess(cmd, proc.returncode, stdout, stderr)
            rec = {"n": n, "doc_id": r["doc_id"], "fmt": fmt, "lang": r["lang"], "node": a.node,
                   "wall": round(wall, 2), "load_before": float(load), "rc": p.returncode,
                   "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t0)),
                   "image_bytes": os.path.getsize(img), "peak_rss_kib": peak_kib or None}
            for line in p.stdout.splitlines():
                if line.startswith("{"):
                    try:
                        rec.update({k: v for k, v in json.loads(line).items() if k in ("secs", "chars", "status", "engine")})
                    except ValueError:
                        pass
            if p.returncode:
                rec["stderr"] = p.stderr[-400:]
                rec["stdout_tail"] = p.stdout[-600:]
            out.write(json.dumps(rec) + "\n")
            out.flush()


if __name__ == "__main__":
    main()
