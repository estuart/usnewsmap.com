"""Installation tests: no server, wrapper execution, or registry is required."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET

spec = importlib.util.spec_from_file_location("boinc_install", Path(__file__).with_name("install.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
IMAGE = "registry.example.org/usnm/boinc@sha256:" + "a" * 64


class InstallTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.project = self.root / "project"
        self.project.mkdir()
        (self.project / "config.xml").write_text("<boinc/>")
        self.wrapper = self.root / "docker_wrapper"
        self.wrapper.write_bytes(b"not-executed")

    def tearDown(self):
        self.tmp.cleanup()

    def test_app_files_templates_and_refusal_to_overwrite(self):
        target = module.install(self.project, self.wrapper, IMAGE, "1.0")
        self.assertEqual((target / "Dockerfile_usnm_ocr_1.0_x86_64-pc-linux-gnu_docker").read_text(), f"FROM {IMAGE}\n")
        tree = ET.parse(target / "version.xml")
        self.assertEqual(tree.find("file/main_program").tag, "main_program")
        self.assertTrue((self.project / "templates/usnm_ocr_out").is_file())
        with self.assertRaises(FileExistsError):
            module.install(self.project, self.wrapper, IMAGE, "1.0")

    def test_unpinned_and_injected_images_never_create_app(self):
        for image in ["image:latest", IMAGE + "\nRUN evil", "sha256:" + "a" * 64]:
            with self.assertRaises(ValueError):
                module.install(self.project, self.wrapper, image, "1.0")
        self.assertFalse((self.project / "apps").exists())

    def test_assessment_app_gets_its_templates(self):
        target = module.install(self.project, self.wrapper, IMAGE, "1.0", "usnm_assess")
        self.assertEqual(target.parts[-3], "usnm_assess")
        for name in ["usnm_assess_in", "usnm_assess_crash_in", "usnm_assess_out"]:
            self.assertTrue((self.project / "templates" / name).is_file())
        with self.assertRaises(ValueError):
            module.install(self.project, self.wrapper, IMAGE, "2.0", "other")

    def test_gold_app_gets_its_output_template(self):
        target = module.install(self.project, self.wrapper, IMAGE, "1.0", "usnm_gold")
        self.assertEqual(target.parts[-3], "usnm_gold")
        self.assertTrue((self.project / "templates" / "usnm_gold_out").is_file())

    def test_surya_versions_per_plan_class(self):
        cpu = module.install(self.project, self.wrapper, IMAGE, "1.0", "usnm_surya", "docker_mt")
        gpu = module.install(self.project, self.wrapper, IMAGE, "1.1", "usnm_surya", "docker_nvidia_cuda")
        self.assertEqual(cpu.name, "x86_64-pc-linux-gnu__docker_mt")
        self.assertEqual(gpu.name, "x86_64-pc-linux-gnu__docker_nvidia_cuda")
        self.assertIn("use_gpu = true", (gpu / "job_usnm_surya_1.1_x86_64-pc-linux-gnu_docker_nvidia_cuda.toml").read_text())
        self.assertNotIn("use_gpu", (cpu / "job_usnm_surya_1.0_x86_64-pc-linux-gnu_docker_mt.toml").read_text())
        for name in ["usnm_surya_in", "usnm_surya_out"]:
            self.assertTrue((self.project / "templates" / name).is_file())
        # Each app only has the plan classes it was made for.
        with self.assertRaises(ValueError):
            module.install(self.project, self.wrapper, IMAGE, "2.0", "usnm_surya", "docker")
        with self.assertRaises(ValueError):
            module.install(self.project, self.wrapper, IMAGE, "2.0", "usnm_ocr", "docker_nvidia_cuda")

    def test_quality_and_qwen_apps(self):
        quality = module.install(self.project, self.wrapper, IMAGE, "1.0", "usnm_quality", "docker")
        self.assertEqual(quality.name, "x86_64-pc-linux-gnu__docker")
        self.assertNotIn("use_gpu", (quality / "job_usnm_quality_1.0_x86_64-pc-linux-gnu_docker.toml").read_text())
        for name in ["usnm_quality_in", "usnm_quality_out"]:
            self.assertTrue((self.project / "templates" / name).is_file())
        self.assertIn("<sticky/>", (self.project / "templates" / "usnm_quality_in").read_text())
        cpu = module.install(self.project, self.wrapper, IMAGE, "1.0", "usnm_qwen", "docker_mt")
        gpu = module.install(self.project, self.wrapper, IMAGE, "1.1", "usnm_qwen", "docker_nvidia_cuda")
        self.assertIn("use_gpu = true", (gpu / "job_usnm_qwen_1.1_x86_64-pc-linux-gnu_docker_nvidia_cuda.toml").read_text())
        self.assertNotIn("use_gpu", (cpu / "job_usnm_qwen_1.0_x86_64-pc-linux-gnu_docker_mt.toml").read_text())
        for name in ["usnm_qwen_in", "usnm_qwen_out"]:
            self.assertTrue((self.project / "templates" / name).is_file())
        with self.assertRaises(ValueError):
            module.install(self.project, self.wrapper, IMAGE, "2.0", "usnm_quality", "docker_mt")
        with self.assertRaises(ValueError):
            module.install(self.project, self.wrapper, IMAGE, "2.0", "usnm_qwen", "docker")

    def test_platforms_coexist_at_one_version_without_shared_download_collisions(self):
        staged_downloads = {}
        for platform in module.PLATFORMS:
            wrapper = self.root / f"wrapper_{platform}"
            wrapper.write_bytes(f"wrapper-payload-{platform}".encode())
            target = module.install(self.project, wrapper, IMAGE, "1.0", platform=platform)
            self.assertEqual(target.name, f"{platform}__docker")
            spec = ET.parse(target / "version.xml").getroot()
            logicals = {f.findtext("logical_name"): f.findtext("physical_name") for f in spec.findall("file") if f.find("logical_name") is not None}
            self.assertEqual(set(logicals), {"Dockerfile", "job.toml"})
            for f in spec.findall("file"):
                phys = f.findtext("physical_name")
                payload = (target / phys).read_bytes()
                if phys in staged_downloads:
                    self.assertEqual(staged_downloads[phys], payload)
                staged_downloads[phys] = payload
            with self.assertRaises(FileExistsError):
                module.install(self.project, wrapper, IMAGE, "1.0", platform=platform)
        with self.assertRaises(ValueError):
            module.install(self.project, self.wrapper, IMAGE, "1.0", "usnm_surya", "docker_mt", platform="arm64-apple-darwin")

    def test_invalid_platform_does_not_create_files(self):
        with self.assertRaisesRegex(ValueError, "platform"):
            module.install(self.project, self.wrapper, IMAGE, "1.0", platform="../../other")
        self.assertFalse((self.project / "apps").exists())


if __name__ == "__main__":
    unittest.main()
