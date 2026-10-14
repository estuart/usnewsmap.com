#!/usr/bin/env python3
"""Read one page with Surya 2 in one input mode, inside the usnm_surya worker image (#516).

    run.py --image page.jpg --mode tiled|whole|capped --out text.txt --stats stats.json
    run.py --probe

`usnm-boinc work` runs this once per mode, in its own process group, with the
deadline, memory sampling and diagnostics around it. Surya starts llama-server
(the image's llama.cpp build, CUDA when a GPU is visible, else CPU) and reads
the image through it. The modes:

- tiled: 1,280 px tiles overlapping by 256 px, as round 1 of the engine survey
  (#370) read pages, with each overlap split at its midpoint so a block is kept
  from one tile only (the fixed version of round 1's cores);
- whole: the whole page in one request;
- capped: the whole page scaled (Lanczos) so its long side is at most 2,048 px.

--probe prints one JSON line: Python, torch and its CUDA check, surya-ocr and
the inference settings, without loading a model.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time

TILE, OVERLAP = 1280, 256
CAPPED_LONG_SIDE = 2048


def tiles(w: int, h: int, size: int = TILE, overlap: int = OVERLAP):
    """(x0, y0, x1, y1, core) for tiles covering the page; cores partition it (engine_runner.py, #370)."""
    step = size - overlap

    def starts(n):
        if n <= size:
            return [0]
        out = list(range(0, n - size, step)) + [n - size]
        return sorted(set(out))

    def cuts(ss, n):
        return [0] + [(b + a + size) // 2 for a, b in zip(ss, ss[1:])] + [n]

    xs, ys = starts(w), starts(h)
    cxs, cys = cuts(xs, w), cuts(ys, h)
    for yi, y in enumerate(ys):
        for xi, x in enumerate(xs):
            yield x, y, min(x + size, w), min(y + size, h), (cxs[xi], cys[yi], cxs[xi + 1], cys[yi + 1])


def inside(core, cx: float, cy: float) -> bool:
    return core[0] <= cx < core[2] and core[1] <= cy < core[3]


def probe() -> int:
    info = {"python": sys.version.split()[0]}
    try:
        import torch

        info["torch"] = torch.__version__
        info["torch_cuda_build"] = torch.version.cuda
        info["torch_cuda"] = bool(torch.cuda.is_available())
        info["torch_cuda_devices"] = torch.cuda.device_count() if info["torch_cuda"] else 0
    except Exception as e:  # noqa: BLE001 - the probe reports, never stops
        info["torch_error"] = repr(e)[:300]
    try:
        import importlib.metadata

        from surya.settings import settings

        info["surya"] = importlib.metadata.version("surya-ocr")
        info["surya_backend"] = settings.SURYA_INFERENCE_BACKEND
        info["surya_torch_device"] = str(getattr(settings, "TORCH_DEVICE_MODEL", None))
        info["llama_ngl"] = settings.LLAMA_CPP_NGL
        info["llama_extra_args"] = settings.LLAMA_CPP_EXTRA_ARGS
    except Exception as e:  # noqa: BLE001
        info["surya_error"] = repr(e)[:300]
    print(json.dumps(info, sort_keys=True))
    print("torch reads only Surya's layout fallback here; the OCR model runs in llama-server, "
          "whose log says which device it used.")
    return 0


def block_text(html: str) -> str:
    from bs4 import BeautifulSoup

    return BeautifulSoup(html, "html.parser").get_text("\n").strip()


class BlockErrors(Exception):
    """Surya reported blocks it could not read: the run failed, whatever text the rest gave."""


def read(image: str, mode: str) -> tuple[list[str], int]:
    from PIL import Image

    from surya.inference import SuryaInferenceManager
    from surya.recognition import RecognitionPredictor

    Image.MAX_IMAGE_PIXELS = None
    im = Image.open(image)
    im.load()
    im = im.convert("RGB")
    if mode == "capped":
        long_side = max(im.size)
        if long_side > CAPPED_LONG_SIDE:
            f = CAPPED_LONG_SIDE / long_side
            im = im.resize((max(1, round(im.width * f)), max(1, round(im.height * f))), Image.LANCZOS)
    manager = SuryaInferenceManager(lazy=False)
    try:
        rec = RecognitionPredictor(manager)
        lines: list[str] = []
        errors: list[str] = []
        if mode == "tiled":
            boxes = list(tiles(*im.size))
            for x0, y0, x1, y1, core in boxes:
                page = rec([im.crop((x0, y0, x1, y1))], full_page=True)[0]
                for b in page.blocks:
                    if b.error:
                        errors.append(f"tile {x0},{y0}: {b.error}")
                        continue
                    if b.skipped:
                        continue
                    bx0, by0, bx1, by1 = b.bbox
                    text = block_text(b.html)
                    if text and inside(core, x0 + (bx0 + bx1) / 2, y0 + (by0 + by1) / 2):
                        lines.append(text)
            if errors:
                raise BlockErrors(errors)
            return lines, len(boxes)
        page = rec([im], full_page=True)[0]
        for b in page.blocks:
            if b.error:
                errors.append(str(b.error))
                continue
            if b.skipped:
                continue
            text = block_text(b.html)
            if text:
                lines.append(text)
        if errors:
            raise BlockErrors(errors)
        return lines, 1
    finally:
        manager.stop()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--probe", action="store_true")
    ap.add_argument("--image")
    ap.add_argument("--mode", choices=["tiled", "whole", "capped"])
    ap.add_argument("--out")
    ap.add_argument("--stats")
    a = ap.parse_args()
    if a.probe:
        return probe()
    if not (a.image and a.mode and a.out and a.stats):
        ap.error("--image, --mode, --out and --stats are required")
    started = time.monotonic()
    try:
        lines, n = read(a.image, a.mode)
    except BlockErrors as e:
        errs = e.args[0]
        print(f"surya could not read {len(errs)} block(s); the first: {'; '.join(map(str, errs[:5]))}"[:2000],
              file=sys.stderr)
        return 3
    tmp = a.out + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    os.replace(tmp, a.out)
    with open(a.stats, "w") as f:
        json.dump({"blocks": len(lines), "seconds": round(time.monotonic() - started, 2), "tiles": n}, f)
    return 0


if __name__ == "__main__":
    sys.exit(main())
