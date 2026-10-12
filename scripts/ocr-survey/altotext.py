"""Text and word boxes from an ALTO file (LoC's or an engine's), for the engine survey (#296).

Text: a TextLine's Strings joined by spaces (a HYP adds "-" at the line's
end), one line per TextLine, a blank line between TextBlocks. That is how
LoC's ocr.txt reads, near enough for the damage metric, which only looks at
words and joins a word broken with a hyphen at a line end.

Boxes are in the file's units, scaled to image pixels with `scale`: LoC's
ALTO is in inch1200 at the scan's dpi (pixels = units * dpi / 1200);
Tesseract's and NDNP Open OCR's are in pixels.
"""

from __future__ import annotations

import re
import xml.etree.ElementTree as ET

ALTO_NAMESPACES = {
    "http://www.loc.gov/standards/alto/ns-v2#": "v2",
    "http://www.loc.gov/standards/alto/ns-v3#": "v3",
    "http://www.loc.gov/standards/alto/ns-v4#": "v4",
    "http://schema.ccs-gmbh.com/ALTO": "ccs",
}


def _local(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def namespace(xml: bytes) -> str | None:
    root = ET.fromstring(xml)
    return root.tag[1:].split("}", 1)[0] if root.tag.startswith("{") else None


def pixel_scale(xml: bytes) -> float:
    """Units to pixels: dpi/1200 when the file is in inch1200 (LoC's), else 1."""
    text = xml[:20000].decode("utf-8", "replace")
    unit = re.search(r"<(?:\w+:)?MeasurementUnit>\s*(\w+)", text)
    if unit and unit.group(1) == "inch1200":
        dpi = re.search(r"xdpi:\s*(\d+)", xml.decode("utf-8", "replace"))
        d = int(dpi.group(1)) if dpi and int(dpi.group(1)) > 0 else 300
        return d / 1200
    if unit and unit.group(1) == "mm10":
        return 300 / 254
    return 1.0


def parse(xml: bytes, scale: float | None = None) -> tuple[str, list[tuple[float, float, float, float, str, int]]]:
    """(text, words): words as (x, y, w, h, content, line number) in pixels."""
    if scale is None:
        scale = pixel_scale(xml)
    root = ET.fromstring(xml)
    blocks_out: list[str] = []
    words = []
    line_no = 0
    for block in root.iter():
        if _local(block.tag) != "TextBlock":
            continue
        lines_out = []
        for line in block:
            if _local(line.tag) != "TextLine":
                continue
            line_no += 1
            parts = []
            for el in line:
                t = _local(el.tag)
                if t == "String":
                    c = el.get("CONTENT", "")
                    parts.append(c)
                    try:
                        words.append((float(el.get("HPOS", 0)) * scale, float(el.get("VPOS", 0)) * scale,
                                      float(el.get("WIDTH", 0)) * scale, float(el.get("HEIGHT", 0)) * scale,
                                      c, line_no))
                    except ValueError:
                        pass
                elif t == "HYP":
                    parts.append("-")
            s = " ".join(p for p in parts if p).replace(" -", "-") if parts and parts[-1] == "-" else " ".join(
                p for p in parts if p)
            if s:
                lines_out.append(s)
        if lines_out:
            blocks_out.append("\n".join(lines_out))
    return "\n\n".join(blocks_out), words


def text_in_box(words, box: tuple[int, int, int, int]) -> str:
    """The words whose centre falls inside box (x0, y0, x1, y1), one line per line of the source."""
    x0, y0, x1, y1 = box
    lines: dict[int, list[str]] = {}
    for x, y, w, h, c, ln in words:
        cx, cy = x + w / 2, y + h / 2
        if x0 <= cx < x1 and y0 <= cy < y1:
            lines.setdefault(ln, []).append(c)
    return "\n".join(" ".join(ws) for _, ws in sorted(lines.items()))
