#!/usr/bin/env python3
"""Verify a Mac artifact's checksum, upstream source pin and required Mach-O slice."""
import argparse
import hashlib
from pathlib import Path
import struct

CPU = {"arm64-apple-darwin": 0x0100000C, "x86_64-apple-darwin": 0x01000007}


def architectures(data: bytes) -> set[int]:
    if len(data) < 8:
        raise ValueError("truncated Mach-O header")
    magic = data[:4]
    if magic in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
        return {struct.unpack_from("<I" if magic[0] == 0xcf else ">I", data, 4)[0]}
    formats = {b"\xca\xfe\xba\xbe": (">", 20), b"\xbe\xba\xfe\xca": ("<", 20),
               b"\xca\xfe\xba\xbf": (">", 32), b"\xbf\xba\xfe\xca": ("<", 32)}
    if magic not in formats:
        raise ValueError("wrapper is not a supported Mach-O executable")
    endian, stride = formats[magic]
    count = struct.unpack_from(endian + "I", data, 4)[0]
    if count > 16 or len(data) < 8 + count * stride:
        raise ValueError("invalid fat Mach-O header")
    return {struct.unpack_from(endian + "I", data, 8 + i * stride)[0] for i in range(count)}


def check(wrapper: Path, platform: str, digest: str, source_pin: Path) -> None:
    data = wrapper.read_bytes()
    if hashlib.sha256(data).hexdigest() != digest:
        raise ValueError("wrapper SHA-256 mismatch")
    if CPU[platform] not in architectures(data):
        raise ValueError("wrapper lacks the selected Mac architecture")
    root = Path(__file__).resolve().parents[2]
    pinned = [line.removeprefix("ARG BOINC_COMMIT=") for line in
              (root / "contrib-compute/images/base/Containerfile").read_text().splitlines()
              if line.startswith("ARG BOINC_COMMIT=")]
    if len(pinned) != 1 or source_pin.read_text().strip() != pinned[0]:
        raise ValueError("wrapper BOINC_COMMIT differs from the server source pin")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--wrapper", type=Path, required=True)
    p.add_argument("--platform", choices=sorted(CPU), required=True)
    p.add_argument("--sha256", required=True)
    p.add_argument("--source-pin", type=Path, required=True)
    a = p.parse_args()
    check(a.wrapper, a.platform, a.sha256, a.source_pin)


if __name__ == "__main__":
    main()
