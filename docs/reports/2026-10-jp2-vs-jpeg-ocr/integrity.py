"""Check each fetched JP2: the jp2c box fits the file and the codestream ends with EOC; md5 where the bag's value is known."""
import argparse, hashlib, json, os, struct
ap = argparse.ArgumentParser()
ap.add_argument("--dir", required=True); ap.add_argument("--crash", required=True)
ap.add_argument("--expected", required=True); ap.add_argument("--out", required=True)
a = ap.parse_args()
exp = {json.loads(l)["n"]: json.loads(l)["md5"] for l in open(a.expected)}
out = open(a.out, "w")
for l in open(a.crash):
    c = json.loads(l); n = c["n"]
    p = os.path.join(a.dir, str(n), "page.jp2")
    b = open(p, "rb").read()
    pos, boxes, jp2c_end, jp2c_len = 0, [], None, None
    while pos + 8 <= len(b):
        ln, ty = struct.unpack(">I4s", b[pos:pos+8])
        hdr = 8
        if ln == 1:
            ln = struct.unpack(">Q", b[pos+8:pos+16])[0]; hdr = 16
        if ln == 0: ln = len(b) - pos
        boxes.append(ty.decode("latin1"))
        if ty == b"jp2c": jp2c_end, jp2c_len = pos + ln, ln - hdr
        pos += ln
        if ln < 8: break
    rec = {"n": n, "verdict": c["verdict"], "bytes": len(b), "boxes": boxes, "box_fits": pos == len(b) and jp2c_end == len(b),
           "eoc": b[-2:] == b"\xff\xd9", "md5_ok": (hashlib.md5(b).hexdigest() == exp[n]) if n in exp else None}
    out.write(json.dumps(rec) + "\n")
