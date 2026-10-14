import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("worker_platform", Path(__file__).with_name("check-worker-platform.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class WorkerPlatformTests(unittest.TestCase):
    def test_mac_refuses_old_single_manifest_and_wrong_architecture(self):
        for manifest in ({"config": {}}, {"manifests": [{"platform": {"os": "linux", "architecture": "amd64"}}]},
                         {"manifests": [{"platform": {"os": "darwin", "architecture": "arm64"}}]}):
            with self.assertRaises(ValueError):
                module.check(manifest, "arm64-apple-darwin")

    def test_native_index_and_linux_compatibility(self):
        index = {"manifests": [{"platform": {"os": "linux", "architecture": arch}} for arch in ("amd64", "arm64")]}
        for platform in module.ARCH:
            module.check(index, platform)
        module.check({"config": {}}, "x86_64-pc-linux-gnu")


if __name__ == "__main__":
    unittest.main()
