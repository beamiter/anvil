#!/usr/bin/env python3
"""Run the real support collector with isolated diagnostic/failure fixtures."""

import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
NAME = "anvil-support-20261114T000000Z.tar.gz"


class SupportBundleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="anvil-support-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.output = self.root / "output with spaces"
        self.output.mkdir()
        self.archive = self.output / NAME
        self.tool("date", "printf '20261114T000000Z\\n'\n")
        self.tool("anvil", '''case "$*" in
    --version) printf 'anvil 0.2.0\\n' ;;
    --doctor) printf 'redacted diagnostic\\n'; exit 1 ;;
    '--doctor --json') printf '{"redacted":true}\\n'; exit 1 ;;
    --check-config*) exit 2 ;;
    --config-path) printf '%s/config/anvil/config.toml\\n' "$HOME" ;;
esac
''')
        self.env = dict(os.environ, PATH=f"{self.bin}:/usr/bin:/bin",
                        ANVIL_BIN=str(self.bin / "anvil"), HOME=str(self.root),
                        XDG_CONFIG_HOME=str(self.root / "config"),
                        XDG_STATE_HOME=str(self.root / "state"),
                        TEST_ARCHIVE=str(self.archive))

    def tool(self, name, body):
        path = self.bin / name
        path.write_text("#!/bin/sh\nset -eu\n" + body)
        path.chmod(0o755)

    def collect(self, output=None, cwd=None):
        return subprocess.run(["bash", str(ROOT / "scripts/support-bundle.sh"),
                               str(self.output if output is None else output)],
                              cwd=cwd, env=self.env, capture_output=True,
                              text=True, timeout=15)

    def assert_clean(self, expected):
        self.assertEqual(sorted(p.name for p in self.output.iterdir()), expected)

    def test_success_keeps_diagnostic_failure_evidence(self):
        result = self.collect()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.archive.stat().st_mode & 0o777, 0o600)
        with tarfile.open(self.archive) as archive:
            system = next(m for m in archive.getmembers() if m.name.endswith('/system.txt'))
            data = archive.extractfile(system).read().decode()
            self.assertIn("doctor_exit=1\n", data)
            self.assertIn("config_check_exit=2\n", data)
        self.assert_clean([NAME])

    def test_relative_output_directory(self):
        result = self.collect(output="output with spaces", cwd=self.root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(tarfile.is_tarfile(self.archive))
        self.assert_clean([NAME])

    def test_failed_producers_never_publish_partial_archive(self):
        for producer in ("tar", "gzip"):
            with self.subTest(producer=producer):
                self.tool(producer, "printf 'partial archive bytes'; exit 73\n")
                result = self.collect()
                self.assertNotEqual(result.returncode, 0)
                self.assert_clean([])
                (self.bin / producer).unlink()

    def test_final_name_is_absent_during_compression(self):
        self.tool("gzip", '[ ! -e "$TEST_ARCHIVE" ] || exit 74\nexec /usr/bin/gzip "$@"\n')
        result = self.collect()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_clean([NAME])

    def test_existing_file_and_dangling_link_are_preserved(self):
        self.archive.write_bytes(b"previous archive")
        self.assertNotEqual(self.collect().returncode, 0)
        self.assertEqual(self.archive.read_bytes(), b"previous archive")
        self.archive.unlink()
        self.archive.symlink_to(self.root / "missing")
        self.assertNotEqual(self.collect().returncode, 0)
        self.assertTrue(self.archive.is_symlink())
        self.assertFalse((self.root / "missing").exists())
        self.assert_clean([NAME])

    def test_concurrent_file_or_directory_link_wins_publication(self):
        victim = self.root / "victim"
        victim.mkdir()
        for kind in ("file", "directory"):
            with self.subTest(kind=kind):
                body = ('printf "concurrent archive" >"$TEST_ARCHIVE"\n' if kind == "file"
                        else '/usr/bin/ln -s "$HOME/victim" "$TEST_ARCHIVE"\n')
                self.tool("ln", body + 'exec /usr/bin/ln "$@"\n')
                self.assertNotEqual(self.collect().returncode, 0)
                if kind == "file":
                    self.assertEqual(self.archive.read_bytes(), b"concurrent archive")
                else:
                    self.assertTrue(self.archive.is_symlink())
                    self.assertEqual(list(victim.iterdir()), [])
                self.assert_clean([NAME])
                self.archive.unlink()


if __name__ == "__main__":
    unittest.main()
