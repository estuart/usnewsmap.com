"""Print the headers of a JP2 file (boxes, SIZ, COD) as one JSON line, with no libraries (#296)."""
import argparse, json, struct


def boxes(b, off=0, end=None):
    end = len(b) if end is None else end
    while off + 8 <= end:
        ln, typ = struct.unpack(">I4s", b[off:off + 8])
        hdr = 8
        if ln == 1:
            ln = struct.unpack(">Q", b[off + 8:off + 16])[0]
            hdr = 16
        elif ln == 0:
            ln = end - off
        yield typ.decode("latin1"), off + hdr, off + ln
        off += ln


def info(path):
    b = open(path, "rb").read()
    out = {"file": path, "bytes": len(b), "boxes": []}
    for typ, s, e in boxes(b):
        out["boxes"].append(typ)
        if typ == "jp2h":
            for t2, s2, e2 in boxes(b, s, e):
                if t2 == "ihdr":
                    h, w, nc, bpc = struct.unpack(">IIHB", b[s2:s2 + 11])
                    out.update(height=h, width=w, comps=nc, bpc=bpc + 1)
                if t2 == "colr":
                    meth = b[s2]
                    out["colr"] = ("enum%d" % struct.unpack(">I", b[s2 + 3:s2 + 7])[0]) if meth == 1 else "icc%d" % (e2 - s2 - 3)
                if t2 == "pclr":
                    out["palette"] = True
                if t2 == "cdef":
                    out["cdef"] = True
        if typ == "jp2c":
            c = b[s:e]
            i = 2  # after SOC
            while i + 4 <= len(c):
                m, = struct.unpack(">H", c[i:i + 2])
                if m == 0xFF90:  # SOT: main header over
                    break
                ln, = struct.unpack(">H", c[i + 2:i + 4])
                seg = c[i + 4:i + 2 + ln]
                if m == 0xFF51:
                    rs, xs, ys, xo, yo, xt, yt, xto, yto, cs = struct.unpack(">HIIIIIIIIH", seg[:36])
                    out.update(siz=[xs, ys], tile=[xt, yt], ntiles=-(-xs // xt) * -(-ys // yt),
                               ssiz=seg[36], profile=rs)
                if m == 0xFF52:
                    sc, prog, layers, mct, nlev, xcb, ycb, cbs, tr = struct.unpack(">BBHBBBBBB", seg[:10])
                    out.update(cod=dict(prog=prog, layers=layers, mct=mct, levels=nlev, cb=[1 << (xcb + 2), 1 << (ycb + 2)],
                                        cbstyle=cbs, wavelet="5/3" if tr else "9/7", precincts=bool(sc & 1), sop=bool(sc & 2), eph=bool(sc & 4)))
                if m == 0xFF5C:
                    out["qcd_style"] = seg[0] & 0x1f
                if m == 0xFF55:
                    out["tlm"] = True
                if m == 0xFF57:
                    out["plm"] = True
                i += 2 + ln
    return out


ap = argparse.ArgumentParser()
ap.add_argument("--files", nargs="+", required=True)
a = ap.parse_args()
for f in a.files:
    print(json.dumps(info(f)))
