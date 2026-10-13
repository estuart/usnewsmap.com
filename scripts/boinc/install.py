#!/usr/bin/env python3
"""Install a signed-app candidate into an existing stock BOINC project.

Does not edit project config, publish/sign files, or install server packages.
"""
import argparse
from pathlib import Path
import re
import shutil
import xml.etree.ElementTree as ET


TEMPLATES = {
    "usnm_ocr": ["usnm_ocr_in", "usnm_ocr_out"],
    # The JP2 against IIIF JPEG assessment (#456): same worker image, same job.toml.
    "usnm_assess": ["usnm_assess_in", "usnm_assess_crash_in", "usnm_assess_out"],
}


def install(project: Path, wrapper: Path, image: str, version: str, app: str = "usnm_ocr") -> Path:
    if app not in TEMPLATES:
        raise ValueError("--app must be one of " + ", ".join(sorted(TEMPLATES)))
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/-]*@sha256:[0-9a-f]{64}", image):
        raise ValueError("--image must be a pullable registry reference pinned by full digest")
    if not re.fullmatch(r"[1-9][0-9]*\.[0-9]+", version):
        raise ValueError("invalid app version")
    if not (project / "config.xml").is_file():
        raise ValueError("create a stock BOINC project first")
    if not wrapper.is_file():
        raise ValueError("missing trusted Linux x86-64 docker_wrapper")
    root = Path(__file__).resolve().parents[2]
    target = project / "apps" / app / version / "x86_64-pc-linux-gnu__docker"
    target.mkdir(parents=True, exist_ok=False)
    shutil.copyfile(wrapper, target / "docker_wrapper_1")
    (target / "docker_wrapper_1").chmod(0o755)
    (target / "Dockerfile_1").write_text(f"FROM {image}\n")
    shutil.copyfile(root / "ocr/boinc/job.toml", target / "job_1.toml")
    spec = ET.Element("version")
    wrapper_file = ET.SubElement(spec, "file")
    ET.SubElement(wrapper_file, "physical_name").text = "docker_wrapper_1"
    ET.SubElement(wrapper_file, "main_program")
    for physical, logical in [("Dockerfile_1", "Dockerfile"), ("job_1.toml", "job.toml")]:
        item = ET.SubElement(spec, "file")
        ET.SubElement(item, "physical_name").text = physical
        ET.SubElement(item, "logical_name").text = logical
        ET.SubElement(item, "copy_file")
    ET.SubElement(spec, "is_wrapper")
    ET.indent(spec)
    ET.ElementTree(spec).write(target / "version.xml", encoding="unicode")
    (project / "templates").mkdir(exist_ok=True)
    for name in TEMPLATES[app]:
        shutil.copyfile(root / "ocr/boinc" / name, project / "templates" / name)
    return target


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", required=True, type=Path)
    parser.add_argument("--wrapper", required=True, type=Path)
    parser.add_argument("--image", required=True)
    parser.add_argument("--version", default="1.0")
    parser.add_argument("--app", default="usnm_ocr", choices=sorted(TEMPLATES))
    args = parser.parse_args()
    print(install(args.project.resolve(), args.wrapper.resolve(), args.image, args.version, args.app))
    print(f"Register {args.app}, sign/publish the app, and configure stock daemons; see docs/boinc.md.")


if __name__ == "__main__":
    main()
