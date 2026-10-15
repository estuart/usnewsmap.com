"""Exercise the publish transaction on a temporary share, without a database."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class PublishTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.project = self.root / 'project'
        (self.project / 'bin').mkdir(parents=True)
        (self.project / '.usnm-initialised').touch()
        for name in ('xadd', 'update_versions'):
            command = self.project / 'bin' / name
            command.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> calls\n')
            command.chmod(0o755)
        self.source = self.root / 'appver'
        (self.source / 'templates').mkdir(parents=True)
        (self.source / 'templates' / 'usnm_ocr_1.0.xml').write_text('template')
        (self.source / 'project.xml').write_text('<project/>')
        lib = self.root / 'lib.sh'
        lib.write_text('fail() { echo "$*" >&2; exit 1; }\nlog() { :; }\nfetch_secrets() { :; }\nrender_config() { :; }\n')
        script = Path(__file__).with_name('publish-appver.sh').read_text()
        script = script.replace('. /usr/local/lib/usnm-boinc/lib.sh', f'. "{lib}"')
        script = script.replace('src=/usr/local/lib/usnm-boinc/appver', f'src="{self.source}"')
        self.script = self.root / 'publish.sh'
        self.script.write_text(script)

    def publish(self, platform, payload='wrapper', version='1.0'):
        src = self.source / f'apps/usnm_ocr/{version}' / f'{platform}__docker'
        src.mkdir(parents=True, exist_ok=True)
        (src / 'docker_wrapper').write_text(payload)
        return subprocess.run(['bash', str(self.script)], capture_output=True, text=True,
                              env={**os.environ, 'PROJ': str(self.project), 'APPVER_APP': 'usnm_ocr',
                                   'APPVER_VERSION': version, 'APPVER_PLATFORM': platform})

    def test_sibling_platform_publish_and_replay_preserve_linux(self):
        self.assertEqual(self.publish('x86_64-pc-linux-gnu', 'linux').returncode, 0)
        linux = self.project / 'apps/usnm_ocr/1.0/x86_64-pc-linux-gnu__docker/docker_wrapper'
        template = self.project / 'templates/usnm_ocr_1.0.xml'
        os.utime(template, (1_000_000_000, 1_000_000_000))
        before = linux.stat().st_mtime_ns
        template_before = (template.stat().st_ino, template.stat().st_mtime_ns)
        self.assertEqual(self.publish('arm64-apple-darwin', 'mac').returncode, 0)
        self.assertEqual(self.publish('arm64-apple-darwin', 'mac').returncode, 0)
        self.assertEqual(linux.read_text(), 'linux')
        self.assertEqual(linux.stat().st_mtime_ns, before)
        self.assertEqual((template.stat().st_ino, template.stat().st_mtime_ns), template_before)
        self.assertEqual(len((self.project / 'calls').read_text().splitlines()), 6)
        self.assertFalse((self.project / '.usnm-publish.lock').exists())

    def test_changed_platform_or_template_is_refused_before_registration(self):
        self.assertEqual(self.publish('x86_64-pc-linux-gnu').returncode, 0)
        calls = (self.project / 'calls').read_bytes()
        self.assertNotEqual(self.publish('x86_64-pc-linux-gnu', 'changed').returncode, 0)
        (self.source / 'templates' / 'usnm_ocr_1.0.xml').write_text('changed template')
        self.assertNotEqual(self.publish('arm64-apple-darwin').returncode, 0)
        self.assertFalse((self.project / 'apps/usnm_ocr/1.0/arm64-apple-darwin__docker').exists())
        self.assertEqual((self.project / 'calls').read_bytes(), calls)
        self.assertFalse((self.project / '.usnm-publish.lock').exists())

    def test_new_version_atomically_updates_changed_template(self):
        self.assertEqual(self.publish('x86_64-pc-linux-gnu', 'v1.0', version='1.0').returncode, 0)
        template = self.project / 'templates/usnm_ocr_1.0.xml'
        old_ino = template.stat().st_ino
        (self.source / 'templates' / 'usnm_ocr_1.0.xml').write_text('v1.1 template')
        self.assertEqual(self.publish('x86_64-pc-linux-gnu', 'v1.1', version='1.1').returncode, 0)
        self.assertEqual(template.read_text(), 'v1.1 template')
        self.assertNotEqual(template.stat().st_ino, old_ino)
        (self.source / 'templates' / 'usnm_ocr_1.0.xml').write_text('drifted sibling template')
        self.assertNotEqual(self.publish('arm64-apple-darwin', 'v1.1-mac', version='1.1').returncode, 0)


if __name__ == '__main__':
    unittest.main()
