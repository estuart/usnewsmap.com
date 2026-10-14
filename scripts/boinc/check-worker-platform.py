#!/usr/bin/env python3
"""Require a native Linux child in the worker index before publishing a Mac app."""
import argparse
import json
import sys

ARCH = {"arm64-apple-darwin": "arm64", "x86_64-apple-darwin": "amd64"}


def check(manifest: dict, platform: str) -> None:
    # Existing Linux releases may use a single amd64 manifest. Mac releases must
    # use the new index; reading a manifest alone cannot prove its CPU architecture.
    if platform == "x86_64-pc-linux-gnu":
        return
    required = {"os": "linux", "architecture": ARCH[platform]}
    if not any(all(child.get("platform", {}).get(k) == v for k, v in required.items())
               for child in manifest.get("manifests", [])):
        raise ValueError(f"worker index lacks linux/{ARCH[platform]}; emulation is unsupported")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--platform", choices=["x86_64-pc-linux-gnu", *ARCH], required=True)
    a = p.parse_args()
    check(json.load(sys.stdin), a.platform)


if __name__ == "__main__":
    main()
