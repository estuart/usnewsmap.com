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
    # The gold check (#462): same worker image. `usnm-boinc gold submit` writes the input
    # template, which depends on the bundle.
    "usnm_gold": ["usnm_gold_out"],
    # The Surya 2 test (#516): its own worker image, a CPU and a GPU version.
    "usnm_surya": ["usnm_surya_in", "usnm_surya_out"],
    # The OCR quality estimate (#522): the CPU app runs in the worker image under the docker
    # class; the Qwen app has its own image, a CPU and a GPU version.
    "usnm_quality": ["usnm_quality_in", "usnm_quality_out"],
    "usnm_qwen": ["usnm_qwen_in", "usnm_qwen_out"],
}

# The plan classes an app version can be installed for (contrib-compute/boinc/plan_class_spec.xml),
# and the job.toml each app uses under each.
JOB_TOML = {
    ("usnm_ocr", "docker"): "ocr/boinc/job.toml",
    ("usnm_assess", "docker"): "ocr/boinc/job.toml",
    ("usnm_gold", "docker"): "ocr/boinc/job.toml",
    ("usnm_surya", "docker_mt"): "ocr/boinc/surya/job-cpu.toml",
    ("usnm_surya", "docker_nvidia_cuda"): "ocr/boinc/surya/job-gpu.toml",
    ("usnm_quality", "docker"): "ocr/boinc/job.toml",
    ("usnm_qwen", "docker_mt"): "ocr/boinc/qwen/job-cpu.toml",
    ("usnm_qwen", "docker_nvidia_cuda"): "ocr/boinc/qwen/job-gpu.toml",
}

PLATFORMS = ("x86_64-pc-linux-gnu", "arm64-apple-darwin", "x86_64-apple-darwin")


def install(project: Path, wrapper: Path, image: str, version: str, app: str = "usnm_ocr",
            plan_class: str = "docker", platform: str = "x86_64-pc-linux-gnu") -> Path:
    if app not in TEMPLATES:
        raise ValueError("--app must be one of " + ", ".join(sorted(TEMPLATES)))
    if platform not in PLATFORMS:
        raise ValueError("unsupported --platform")
    if (app, plan_class) not in JOB_TOML:
        raise ValueError(f"{app} has no version for plan class {plan_class}: one of "
                         + ", ".join(sorted(p for a, p in JOB_TOML if a == app)))
    if platform != "x86_64-pc-linux-gnu" and plan_class != "docker":
        raise ValueError(f"{platform} supports only plan class docker")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/-]*@sha256:[0-9a-f]{64}", image):
        raise ValueError("--image must be a pullable registry reference pinned by full digest")
    if not re.fullmatch(r"[1-9][0-9]*\.[0-9]+", version):
        raise ValueError("invalid app version")
    if not (project / "config.xml").is_file():
        raise ValueError("create a stock BOINC project first")
    if not wrapper.is_file():
        raise ValueError("missing trusted docker_wrapper for the selected platform")
    root = Path(__file__).resolve().parents[2]
    target = project / "apps" / app / version / f"{platform}__{plan_class}"
    target.mkdir(parents=True, exist_ok=False)
    suffix = f"{app}_{version}_{platform}_{plan_class}"
    wrapper_name = f"docker_wrapper_{suffix}"
    dockerfile_name = f"Dockerfile_{suffix}"
    job_name = f"job_{suffix}.toml"
    shutil.copyfile(wrapper, target / wrapper_name)
    (target / wrapper_name).chmod(0o755)
    (target / dockerfile_name).write_text(f"FROM {image}\n")
    shutil.copyfile(root / JOB_TOML[(app, plan_class)], target / job_name)
    spec = ET.Element("version")
    wrapper_file = ET.SubElement(spec, "file")
    ET.SubElement(wrapper_file, "physical_name").text = wrapper_name
    ET.SubElement(wrapper_file, "main_program")
    for physical, logical in [(dockerfile_name, "Dockerfile"), (job_name, "job.toml")]:
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
    parser.add_argument("--plan-class", default="docker", choices=sorted({p for _, p in JOB_TOML}))
    parser.add_argument("--platform", default="x86_64-pc-linux-gnu", choices=PLATFORMS)
    args = parser.parse_args()
    print(install(args.project.resolve(), args.wrapper.resolve(), args.image, args.version, args.app,
                  args.plan_class, args.platform))
    print(f"Register {args.app}, sign/publish the app, and configure stock daemons; see docs/boinc.md.")


if __name__ == "__main__":
    main()
