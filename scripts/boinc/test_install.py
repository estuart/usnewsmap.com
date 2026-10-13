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
        self.assertEqual((target / "Dockerfile_1").read_text(), f"FROM {IMAGE}\n")
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


if __name__ == "__main__":
    unittest.main()
