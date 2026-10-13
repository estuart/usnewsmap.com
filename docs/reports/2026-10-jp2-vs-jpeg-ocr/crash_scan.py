"""Count how many of the fetched JP2s the production engine cannot read (#296).

Runs the worker's `ocr-file` on each page's JP2 with a 20 s page timeout. A
JP2 the engine cannot read fails in a few seconds with "tesseract exited with
exit status: 136"; one it can read runs on until the 20 s limit kills it. The
two are told apart by the message, so no page is OCRed to the end. Same user and
engine settings as run_ocr.py. Unlike run_ocr.py it does not renice the
Tesseract process inside the container, so the engine ran at normal priority.
"""
import argparse
import json
import os
import subprocess
import time

from run_ocr import WORKER, read_env


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sample", required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--results", required=True)
    ap.add_argument("--timeout-secs", type=int, default=20)
    ap.add_argument("--env-file", default="/etc/usnm/ocr-worker.env")
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.sample)]
    env = read_env(a.env_file)
    keep = {k: env[k] for k in ("USNM_OCR_TESSERACT_IMAGE",) if k in env}
    keep.update({"USNM_OCR_PODMAN_URL": "unix:///run/usnm-ocr/podman.sock",
                 "USNM_OCR_SCRATCH_DIR": "/run/usnm-ocr/scratch", "HOME": "/var/lib/usnm-ocr",
                 "XDG_RUNTIME_DIR": "/run/usnm-ocr-worker"})
    done = {json.loads(l)["n"] for l in open(a.results)} if os.path.exists(a.results) else set()
    out = open(a.results, "a")
    for n, r in enumerate(rows, 1):
        img = os.path.join(a.dir, str(n), "page.jp2")
        if n in done or not os.path.exists(img):
            continue
        od = os.path.join(a.dir, str(n), "out-scan")
        os.makedirs(od, exist_ok=True)
        subprocess.run(["chmod", "-R", "a+rwX", os.path.join(a.dir, str(n))], capture_output=True)
        cmd = ["sudo", "-n", "-u", "usnm", "nice", "-n", "19", "ionice", "-c", "3", "env"]
        cmd += [f"{k}={v}" for k, v in keep.items()]
        cmd += [WORKER, "ocr-file", "--image", img, "--language", r["lang"], "--out", od,
                "--page-timeout-secs", str(a.timeout_secs)]
        t0 = time.time()
        p = subprocess.run(cmd, capture_output=True, text=True)
        text = p.stdout + p.stderr
        verdict = "ok" if p.returncode == 0 else "crash_136" if "exit status: 136" in text else "timeout" if time.time() - t0 >= a.timeout_secs - 1 else "other"
        out.write(json.dumps({"n": n, "doc_id": r["doc_id"], "lang": r["lang"], "decade": r["decade"], "batch": r["batch"],
                              "bytes": os.path.getsize(img), "wall": round(time.time() - t0, 1), "rc": p.returncode,
                              "verdict": verdict, "tail": text[-200:] if verdict == "other" else None}) + "\n")
        out.flush()


if __name__ == "__main__":
    main()
