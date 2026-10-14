#!/usr/bin/env python3
"""Put the signatures beside a staged app version and check it, in the appver image build
(Containerfile). Any failure fails the build, so no publish image exists for a bad version.

  check_appver.py --version-dir DIR --sigs DIR --public-key FILE [--crypt-prog PATH]

Refuses:
- a signature in --sigs for no staged file (an orphan .sig), or any other file there;
- a staged file (other than version.xml) without a signature;
- a signature that BOINC's crypt_prog -verify rejects against the public key;
- a version.xml that names a physical_name that isn't staged;
- a version directory that holds anything but plain files.
"""
import argparse
from pathlib import Path
import re
import shutil
import subprocess
import sys
import xml.etree.ElementTree as ET

NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")


class Refused(Exception):
    pass


def check(version_dir: Path, sigs: Path, public_key: Path, crypt_prog: str) -> list[str]:
    """Copies each NAME.sig from sigs into version_dir and checks the result. Returns the signed files."""
    if not public_key.is_file() or public_key.stat().st_size == 0:
        raise Refused(f"no code-signing public key at {public_key}")
    for p in version_dir.iterdir():
        if not p.is_file() or p.is_symlink() or not NAME.fullmatch(p.name):
            raise Refused(f"{p.name!r} in the version directory is not a plain file")
        if p.name.endswith(".sig"):
            raise Refused(f"{p.name} was staged, not signed: signatures come only from --sigs")
    files = sorted(p.name for p in version_dir.iterdir() if p.name != "version.xml")
    if "version.xml" not in {p.name for p in version_dir.iterdir()}:
        raise Refused("no version.xml")
    if not files:
        raise Refused("no files besides version.xml")
    given = sorted(p.name for p in sigs.iterdir())
    bad = [n for n in given if not n.endswith(".sig") or not NAME.fullmatch(n)]
    if bad:
        raise Refused("not a signature: " + ", ".join(bad))
    signed = {n[: -len(".sig")] for n in given}
    orphans = sorted(signed - set(files))
    if orphans:
        raise Refused("a .sig with no staged file: " + ", ".join(o + ".sig" for o in orphans))
    unsigned = [n for n in files if n not in signed]
    if unsigned:
        raise Refused("no .sig for " + ", ".join(unsigned))
    try:
        spec = ET.parse(version_dir / "version.xml").getroot()
    except ET.ParseError as e:
        raise Refused(f"version.xml is not XML: {e}") from e
    missing = [e.text or "" for e in spec.iter("physical_name") if (e.text or "") not in files]
    if missing:
        raise Refused("version.xml names files that are not staged: " + ", ".join(missing))
    for n in files:
        shutil.copyfile(sigs / (n + ".sig"), version_dir / (n + ".sig"))
        r = subprocess.run([crypt_prog, "-verify", str(version_dir / n), str(version_dir / (n + ".sig")),
                            str(public_key)], capture_output=True, text=True)
        if r.returncode != 0:
            raise Refused(f"{n}.sig does not verify against the public key: {(r.stdout + r.stderr).strip()}")
    return files


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--version-dir", required=True, type=Path)
    ap.add_argument("--sigs", required=True, type=Path)
    ap.add_argument("--public-key", required=True, type=Path)
    ap.add_argument("--crypt-prog", default="/usr/local/boinc/lib/crypt_prog")
    a = ap.parse_args(argv)
    try:
        files = check(a.version_dir, a.sigs, a.public_key, a.crypt_prog)
    except Refused as e:
        print(f"check_appver: refused {a.version_dir}: {e}", file=sys.stderr)
        return 1
    print(f"check_appver: {len(files)} files signed and verified: {', '.join(files)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
