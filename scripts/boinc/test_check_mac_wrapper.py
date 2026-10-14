"""Reject altered, wrong-platform and wrong-source Mac build artifacts before signing."""
import hashlib
import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("mac_wrapper", Path(__file__).with_name("check-mac-wrapper.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class CheckMacWrapper(unittest.TestCase):
    def test_architecture_checksum_and_source_are_all_required(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            wrapper = root / "wrapper"
            wrapper.write_bytes(b"\xcf\xfa\xed\xfe" + struct.pack("<I", module.CPU["arm64-apple-darwin"]) + b"payload")
            digest = hashlib.sha256(wrapper.read_bytes()).hexdigest()
            pin = root / "BOINC_COMMIT"
            repo = Path(__file__).resolve().parents[2]
            pin.write_text(next(l.split("=", 1)[1] for l in
                                (repo / "contrib-compute/images/base/Containerfile").read_text().splitlines()
                                if l.startswith("ARG BOINC_COMMIT=")))
            module.check(wrapper, "arm64-apple-darwin", digest, pin)
            with self.assertRaisesRegex(ValueError, "architecture"):
                module.check(wrapper, "x86_64-apple-darwin", digest, pin)
            with self.assertRaisesRegex(ValueError, "SHA-256"):
                module.check(wrapper, "arm64-apple-darwin", "0" * 64, pin)
            pin.write_text("0" * 40)
            with self.assertRaisesRegex(ValueError, "source pin"):
                module.check(wrapper, "arm64-apple-darwin", digest, pin)

    def test_universal_and_truncated_headers(self):
        cpu = list(module.CPU.values())
        data = b"\xca\xfe\xba\xbe" + struct.pack(">I", 2)
        data += b"".join(struct.pack(">IIIII", c, 0, 48, 1, 0) for c in cpu)
        self.assertEqual(module.architectures(data), set(cpu))
        with self.assertRaisesRegex(ValueError, "header"):
            module.architectures(data[:-1])


if __name__ == "__main__":
    unittest.main()
