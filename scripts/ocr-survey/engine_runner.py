#!/usr/bin/env python3
"""Run one OCR engine on a list of page images, one page at a time, and measure each (#296).

    engine_runner.py --engine tesseract --jobs /work/jobs.jsonl --results /work/results.jsonl --out /work/out

This is the entry point of every survey container. Each line of --jobs is a
job: {"job_id", "page_id", "image", "variant", "scale", "timeout"}. For
`tesseract` the variant is the -l argument; for the others it names the
model or setting. scale < 1 shrinks the image (linear, Lanczos) before OCR.

The engine's models are loaded once in this process. Each page then runs in
it with a SIGALRM timeout (a page past it is stopped and recorded as a
timeout; Surya's partial text is kept). While a page runs, a thread samples
the process tree (this process, Tesseract, llama-server) from /proc every
0.25 s: peak_rss_mb is the largest summed RSS seen, cpu_seconds the tree's
CPU time over the page. Seconds are wall time for the page, model loading
excluded; cpu_seconds / seconds is the number of cores the page kept busy.

Writes OUT/<job_id>.txt (text), OUT/<job_id>.xml (ALTO, where the engine
makes it) and one result line per job to --results. Jobs that already have
a result line are skipped, so a run can be stopped and started again.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import time
import traceback
from pathlib import Path

TILE, OVERLAP = 1280, 256  # docTR and Surya: tiles of a large page, in pixels, and their overlap


def load_image(path: str, scale: float):
    from PIL import Image

    Image.MAX_IMAGE_PIXELS = None
    im = Image.open(path)
    im.load()
    if scale != 1.0:
        im = im.resize((max(1, round(im.width * scale)), max(1, round(im.height * scale))), Image.LANCZOS)
    return im.convert("RGB")


def tiles(w: int, h: int, size: int = TILE, overlap: int = OVERLAP):
    """(x0, y0, x1, y1, core box) for tiles covering the page. A word belongs to the tile whose core holds its
    centre. Cores split each overlap at its midpoint between the actual adjacent tile starts, so they partition
    the page and each word is kept once, even where the last tile overlaps its neighbour by more than `overlap`.

    Round 1 of the survey ran with cores that took half of `overlap` from each inner side instead. Where the
    last gap was shorter than a step, those cores overlapped, and words there were kept twice (see the report)."""
    step = size - overlap

    def starts(n):
        if n <= size:
            return [0]
        out = list(range(0, n - size, step)) + [n - size]
        return sorted(set(out))

    def cuts(ss, n):
        # core edges: 0, the midpoint of each adjacent pair's overlap, n
        return [0] + [(b + a + size) // 2 for a, b in zip(ss, ss[1:])] + [n]

    xs, ys = starts(w), starts(h)
    cxs, cys = cuts(xs, w), cuts(ys, h)
    for yi, y in enumerate(ys):
        for xi, x in enumerate(xs):
            yield x, y, min(x + size, w), min(y + size, h), (cxs[xi], cys[yi], cxs[xi + 1], cys[yi + 1])


def inside(core, cx: float, cy: float) -> bool:
    return core[0] <= cx < core[2] and core[1] <= cy < core[3]


# ------------------------------------------------------------------ engines


class Tesseract:
    name = "tesseract"

    def __init__(self):
        import subprocess

        self.version = subprocess.run(["tesseract", "--version"], capture_output=True, text=True).stdout.split("\n")[0]
        self.threads = int(os.environ.get("OMP_THREAD_LIMIT", "0") or 0) or None

    def run(self, job: dict, out: Path) -> None:
        import subprocess
        import tempfile

        img = job["image"]
        with tempfile.TemporaryDirectory() as tmp:
            if job.get("scale", 1.0) != 1.0:
                p = os.path.join(tmp, "scaled.png")
                load_image(img, job["scale"]).save(p)
                img = p
            base = out / job["job_id"]
            psm = str(job.get("psm", 3))
            p = subprocess.Popen(["tesseract", img, str(base), "-l", job["variant"], "--psm", psm, "txt", "alto"],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                _, stderr = p.communicate()
            except BaseException:
                p.kill()
                p.wait()
                raise
            if p.returncode != 0:
                raise RuntimeError(stderr[-2000:])
            if not (out / f"{job['job_id']}.xml").exists():
                raise RuntimeError("no ALTO written: " + stderr[-1000:])


class Doctr:
    name = "doctr"

    def __init__(self):
        import doctr
        import torch
        from doctr.models import ocr_predictor

        self.version = f"python-doctr {doctr.__version__}, torch {torch.__version__}"
        self.threads = torch.get_num_threads()
        self.predictors = {}
        self.ocr_predictor = ocr_predictor

    def predictor(self, variant: str):
        # variant: "<det_arch>+<reco_arch>"
        if variant not in self.predictors:
            det, reco = variant.split("+")
            if reco == "parseq-multilingual":  # the hub model, loaded from its pinned files as from_hub would
                from doctr.models import parseq

                d = os.environ["DOCTR_MULTILINGUAL"]
                cfg = json.loads(Path(d, "config.json").read_text())
                reco = parseq(pretrained=False, vocab=cfg["vocab"], input_shape=cfg["input_shape"])
                reco.cfg = {k: v for k, v in cfg.items() if k not in ("arch", "task")}
                reco.from_pretrained(str(Path(d, "pytorch_model.bin")))
            self.predictors[variant] = self.ocr_predictor(det_arch=det, reco_arch=reco, pretrained=True,
                                                          assume_straight_pages=True)
        return self.predictors[variant]

    def preload(self, variants):
        for v in variants:
            self.predictor(v)

    def run(self, job: dict, out: Path) -> None:
        import numpy as np

        pred = self.predictor(job["variant"])
        im = load_image(job["image"], job.get("scale", 1.0))
        arr = np.asarray(im)
        h, w = arr.shape[:2]
        lines_out, words_out = [], []
        batch = list(tiles(w, h))
        for i in range(0, len(batch), 4):
            chunk = batch[i:i + 4]
            res = pred([arr[y0:y1, x0:x1] for x0, y0, x1, y1, _ in chunk])
            for (x0, y0, x1, y1, core), page in zip(chunk, res.pages):
                tw, th = x1 - x0, y1 - y0
                for block in page.blocks:
                    for line in block.lines:
                        kept = []
                        for word in line.words:
                            (a, b), (c, d) = word.geometry
                            wx0, wy0, wx1, wy1 = x0 + a * tw, y0 + b * th, x0 + c * tw, y0 + d * th
                            if inside(core, (wx0 + wx1) / 2, (wy0 + wy1) / 2):
                                kept.append(word.value)
                                words_out.append([round(wx0), round(wy0), round(wx1), round(wy1), word.value,
                                                  round(float(word.confidence), 3)])
                        if kept:
                            lines_out.append(" ".join(kept))
        (out / f"{job['job_id']}.txt").write_text("\n".join(lines_out), encoding="utf-8")
        (out / f"{job['job_id']}.words.json").write_text(json.dumps(words_out, ensure_ascii=False))


class Surya:
    """Surya 2 (surya-ocr 0.22): a vision-language model served by llama-server, full-page mode per tile.
    The model shrinks what it is given, so a newspaper page is read in TILE-pixel tiles, not whole."""

    name = "surya"

    def __init__(self):
        import importlib.metadata

        from surya.inference import SuryaInferenceManager
        from surya.recognition import RecognitionPredictor

        self.version = f"surya-ocr {importlib.metadata.version('surya-ocr')}, llama.cpp server"
        self.threads = os.cpu_count()
        self.manager = SuryaInferenceManager(lazy=False)  # starts llama-server and loads the model now
        self.rec = RecognitionPredictor(self.manager)

    @staticmethod
    def block_text(html: str) -> str:
        from bs4 import BeautifulSoup

        return BeautifulSoup(html, "html.parser").get_text("\n").strip()

    LOG = Path("~/.cache/datalab/surya/llamacpp_server.log").expanduser()

    def run(self, job: dict, out: Path) -> None:
        try:
            self.read(job, out)
        except BaseException:
            # a request cut off by the page timeout would keep llama-server busy into the next page
            signal.setitimer(signal.ITIMER_REAL, 0)
            self.manager.stop()
            self.manager.start()
            raise
        finally:
            # llama-server's log for this page (prompt and generation speed in tokens per second)
            if self.LOG.exists():
                (out / f"{job['job_id']}.llama.log").write_bytes(self.LOG.read_bytes())
                self.LOG.write_bytes(b"")

    def read(self, job: dict, out: Path) -> None:
        im = load_image(job["image"], job.get("scale", 1.0))
        w, h = im.size
        lines_out = []
        for x0, y0, x1, y1, core in tiles(w, h):
            page = self.rec([im.crop((x0, y0, x1, y1))], full_page=True)[0]
            for b in page.blocks:
                if b.skipped or b.error:
                    continue
                bx0, by0, bx1, by1 = b.bbox
                text = self.block_text(b.html)
                if text and inside(core, x0 + (bx0 + bx1) / 2, y0 + (by0 + by1) / 2):
                    lines_out.append(text)
            # partial output, so a page that times out still shows how far it got
            (out / f"{job['job_id']}.txt").write_text("\n".join(lines_out), encoding="utf-8")


class NdnpOpenOcr:
    name = "ndnp-open-ocr"

    def __init__(self):
        import pytesseract

        self.version = f"ndnp-open-ocr {os.environ.get('NDNP_OPEN_OCR_COMMIT', '?')}, tesseract " \
                       f"{pytesseract.get_tesseract_version()}"
        self.threads = int(os.environ.get("OMP_THREAD_LIMIT", "0") or 0) or None
        from ndnp_open_ocr import processors  # imports the segmenter, if its assets are there

        if not processors._SEGMENTER_AVAILABLE:  # it would silently fall back to plain Tesseract
            raise RuntimeError("NDNP Open OCR's segmenter is not available")
        self.OCRProcessor, self.Pre = processors.OCRProcessor, processors.PreprocessingMethod

    def run(self, job: dict, out: Path) -> None:
        import shutil
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            src = os.path.join(tmp, job["job_id"] + ".jpg")
            shutil.copy(job["image"], src)
            o = os.path.join(tmp, "out")
            os.makedirs(o)
            p = self.OCRProcessor(src, o, preprocessing_method=self.Pre.ORIGINAL,
                                  use_segmenter=job["variant"] == "segmentation")
            p.generate_alto()  # ALTO only: the PDF it also makes is not needed here
            x = os.path.join(o, job["job_id"] + ".xml")
            if not os.path.isfile(x):
                raise RuntimeError("no ALTO written")
            shutil.copy(x, out / f"{job['job_id']}.xml")
        import altotext

        text, _ = altotext.parse((out / f"{job['job_id']}.xml").read_bytes())
        (out / f"{job['job_id']}.txt").write_text(text, encoding="utf-8")


class AmericanStories:
    """American Stories' pipeline (layout, legibility, lines, EfficientOCR words and characters), run as its
    own script on one page at a time with the settings of its process_scans.sh. The script loads its models on
    every run; init times one run on a blank image, so that share of each page's seconds is known."""

    name = "americanstories"

    def __init__(self):
        import tempfile

        from PIL import Image

        self.dir, self.models = os.environ["AMERICANSTORIES_DIR"], os.environ["AMERICANSTORIES_MODELS"]
        self.version = f"AmericanStories {os.environ.get('AMERICANSTORIES_COMMIT', '?')}"
        self.threads = os.cpu_count()
        with tempfile.TemporaryDirectory() as tmp:
            Image.new("RGB", (640, 640), "white").save(os.path.join(tmp, "blank.jpg"))
            t0 = time.time()
            try:
                self.pipeline(os.path.join(tmp, "blank.jpg"), tmp)
            except RuntimeError as e:  # timed all the same: the models were loaded before the page failed
                print(f"americanstories: blank page: {str(e)[-300:]}", flush=True)
            self.blank_seconds = round(time.time() - t0, 1)
        print(f"americanstories: a blank page takes {self.blank_seconds}s (model loading)", flush=True)

    def pipeline(self, image: str, tmp: str) -> dict:
        import shutil
        import subprocess

        scans, outdir = os.path.join(tmp, "scans"), os.path.join(tmp, "out")
        os.makedirs(scans, exist_ok=True)
        os.makedirs(outdir, exist_ok=True)
        name = os.path.basename(image)
        shutil.copy(image, os.path.join(scans, name))
        m = self.models
        cmd = ["python3", os.path.join(self.dir, "src", "run_img2txt_yolo_pipeline.py"), "--manifest_path", scans,
               "--checkpoint_path_layout", f"{m}/layout_model_new.onnx",
               "--checkpoint_path_line", f"{m}/line_model_new.onnx",
               "--label_map_path_layout", f"{self.dir}/src/label_maps/label_map_layout.json",
               "--label_map_path_line", f"{self.dir}/src/label_maps/label_map_line.json",
               "--effocr_recognizer_dir", f"{m}/word_recognizer_new",
               "--effocr_char_recognizer_dir", f"{m}/char_recognizer",
               "--effocr_localizer_dir", f"{m}/", "--output_save_path", outdir,
               "--legibility-classifier", f"{m}/legibility_model_new.onnx",
               "--localizer_model_backend", "yolov8", "--line_model_backend", "yolov8",
               "--layout_model_backend", "yolov8", "--recognizer_word_thresh", "0.85", "--word-level-effocr"]
        # run from the directory above the checkout, as process_scans.sh does: the code opens files by
        # paths relative to it (./AmericanStories/src/utils/...)
        p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                             cwd=os.path.dirname(self.dir))
        try:
            log, _ = p.communicate()
        except BaseException:
            p.kill()
            p.wait()
            raise
        # The script names its output after the input file, swapping "jp2" for "json" only
        res = os.path.join(outdir, name.replace("jp2", "json"))
        if p.returncode != 0 or not os.path.isfile(res):
            # the script logs to a file in its output directory, and records a failed stage in error_table.csv
            for extra in ("error_table.csv", "ca_transcription.txt"):
                if os.path.isfile(os.path.join(outdir, extra)):
                    log += f"\n--- {extra}\n" + open(os.path.join(outdir, extra)).read()[-1500:]
            raise RuntimeError(f"exit {p.returncode}, no output: {log[-3000:]}")
        with open(res) as f:
            return json.load(f)

    def run(self, job: dict, out: Path) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            img = job["image"]
            if job.get("scale", 1.0) != 1.0:
                img = os.path.join(tmp, "scaled.jpg")
                load_image(job["image"], job["scale"]).save(img, quality=95)
            res = self.pipeline(img, tmp)
        regions = [{"bbox": b["bbox"], "class": b["class"], "legibility": b.get("legibility"),
                    "text": b.get("raw_text", "")} for b in res.get("bboxes", [])]
        (out / f"{job['job_id']}.regions.json").write_text(json.dumps(regions, ensure_ascii=False))
        (out / f"{job['job_id']}.txt").write_text("\n\n".join(r["text"] for r in regions if r["text"].strip()),
                                                  encoding="utf-8")


ENGINES = {"americanstories": AmericanStories, "tesseract": Tesseract, "doctr": Doctr, "surya": Surya, "ndnp-open-ocr": NdnpOpenOcr}


# ------------------------------------------------------------------ the loop


class PageTimeout(BaseException):
    """A BaseException, so that an engine's own `except Exception` (NDNP Open OCR's generate_alto has one)
    can't swallow it."""


def _on_alarm(signum, frame):
    raise PageTimeout()


def tree() -> list[int]:
    """This process and its descendants (Tesseract, llama-server), from /proc."""
    parents = {}
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                with open(f"/proc/{d}/stat") as f:
                    parents[int(d)] = int(f.read().rsplit(")", 1)[1].split()[1])
            except OSError:
                pass
    out, todo = [], [os.getpid()]
    while todo:
        p = todo.pop()
        out.append(p)
        todo.extend(c for c, pp in parents.items() if pp == p)
    return out


def proc_stats(pid: int) -> tuple[float, float]:
    """(RSS in MB, user + system CPU seconds) of one process, 0 if it is gone."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()
        tick = os.sysconf("SC_CLK_TCK")
        cpu = (int(fields[11]) + int(fields[12])) / tick
        rss = int(fields[21]) * os.sysconf("SC_PAGE_SIZE") / 2**20
        return rss, cpu
    except (OSError, IndexError, ValueError):
        return 0.0, 0.0


class Meter:
    """Samples the process tree every 0.25 s while a page runs: peak RSS (sum over the tree) and CPU seconds
    (the tree's CPU at the end less at the start, plus children that exited meanwhile)."""

    def __init__(self):
        import threading

        self.peak = 0.0
        self.cpu_seen: dict[int, float] = {}
        self.cpu_start: dict[int, float] = {p: proc_stats(p)[1] for p in tree()}
        self.stop = threading.Event()
        self.t = threading.Thread(target=self.loop, daemon=True)
        self.t.start()

    def sample(self):
        total = 0.0
        for p in tree():
            rss, cpu = proc_stats(p)
            total += rss
            self.cpu_seen[p] = max(self.cpu_seen.get(p, 0.0), cpu)
        self.peak = max(self.peak, total)

    def loop(self):
        while not self.stop.wait(0.25):
            self.sample()

    def finish(self) -> tuple[float, float]:
        self.stop.set()
        self.t.join()
        self.sample()
        cpu = sum(c - self.cpu_start.get(p, 0.0) for p, c in self.cpu_seen.items())
        return self.peak, cpu


def run_one(engine, job: dict, out: Path) -> dict:
    """One page in this process, with a SIGALRM timeout. (Forking per page crashed torch's thread pool.)"""
    err = out / f"{job['job_id']}.err"
    err.unlink(missing_ok=True)  # from an earlier failed try of this job
    timeout = job.get("timeout") or 3600
    status = "ok"
    meter = Meter()
    t0 = time.time()
    signal.signal(signal.SIGALRM, _on_alarm)
    signal.setitimer(signal.ITIMER_REAL, timeout)
    try:
        engine.run(job, out)
    except PageTimeout:
        status = "timeout"
    except Exception:  # noqa: BLE001 - recorded, the run goes on
        status = "error"
        err.write_text(traceback.format_exc()[-4000:])
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
    wall = time.time() - t0
    peak, cpu = meter.finish()
    return {
        "job_id": job["job_id"], "page_id": job["page_id"], "engine": engine.name, "variant": job["variant"],
        "scale": job.get("scale", 1.0), "status": status, "seconds": round(wall, 2), "cpu_seconds": round(cpu, 2),
        "busy_cores": round(cpu / wall, 2) if wall else None, "peak_rss_mb": round(peak, 1),
        "threads_setting": engine.threads, "concurrency": job.get("concurrency", 1), "timeout": timeout, "engine_version": engine.version,
        "error": err.read_text()[-1500:] if err.exists() else None,
        "finished": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--engine", required=True, choices=sorted(ENGINES))
    ap.add_argument("--jobs", type=Path, required=True)
    ap.add_argument("--results", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    jobs = [json.loads(x) for x in a.jobs.read_text().splitlines() if x.strip()]
    done = set()
    if a.results.exists():
        done = {json.loads(x)["job_id"] for x in a.results.read_text().splitlines() if x.strip()}
    todo = [j for j in jobs if j["job_id"] not in done]
    if not todo:
        return
    t0 = time.time()
    engine = ENGINES[a.engine]()
    if hasattr(engine, "preload"):
        engine.preload(sorted({j["variant"] for j in todo}))
    load = round(time.time() - t0, 1)
    print(f"{a.engine}: {engine.version}; models loaded in {load}s; {len(todo)} jobs", flush=True)
    for job in todo:
        r = run_one(engine, job, a.out)
        r["model_load_seconds"] = load
        if hasattr(engine, "blank_seconds"):
            r["blank_page_seconds"] = engine.blank_seconds
        with a.results.open("a") as f:
            f.write(json.dumps(r) + "\n")
        print(f"{r['job_id']}: {r['status']} {r['seconds']}s rss {r['peak_rss_mb']} MB", flush=True)


if __name__ == "__main__":
    main()
