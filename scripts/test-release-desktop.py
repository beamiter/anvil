#!/usr/bin/env python3
"""Launch installed desktop entries via GIO using a recording executable.

This verifies launcher parsing and process spawning, not a GTK/AppModel session.
"""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio


BUNDLE = Path(sys.argv.pop(1)).resolve()
ROOT = Path(__file__).resolve().parents[1]


class DesktopInstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="anvil-desktop-launch-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def install(self, kind, home):
        env = dict(os.environ, HOME=str(home), XDG_CONFIG_HOME=str(home / "config"),
                   XDG_DATA_HOME=str(home / "data"), PATH="/usr/bin:/bin", DESTDIR="")
        command = (["bash", str(BUNDLE / "install.sh"), "--no-config"] if kind == "release"
                   else ["bash", str(ROOT / "scripts/install.sh"), "--no-config",
                         "--binary", str(BUNDLE / "bin/anvil")])
        return subprocess.run(command, env=env, text=True, capture_output=True, timeout=30)

    def check_launches(self, home, arguments=()):
        desktop = home / "data/applications/io.github.beamiter.anvil.desktop"
        info = Gio.DesktopAppInfo.new_from_filename(str(desktop))
        self.assertIsNotNone(info, desktop.read_text())
        for action in (None, "NewWindow"):
            record = self.root / "launch-record"
            record.unlink(missing_ok=True)
            context = Gio.AppLaunchContext()
            context.setenv("ANVIL_TEST_LAUNCH_RECORD", str(record))
            if action is None:
                self.assertTrue(info.launch([], context))
            else:
                info.launch_action(action, context)
            deadline = time.monotonic() + 3
            while not record.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(record.exists(), f"launch failed: {home}/{action}")
            self.assertEqual(record.read_text().splitlines(),
                             [str(home / ".local/bin/anvil"), *arguments])

    def test_installed_main_and_action_launch_exact_executable(self):
        for kind in ("release", "source"):
            for name in ("space home", r"back\slash", 'quote"home', "dollar$home",
                         "tick`home", "single'home", "中文 home"):
                with self.subTest(installer=kind, home=name):
                    home = self.root / kind / name
                    result = self.install(kind, home)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.check_launches(home)

    def test_release_keeps_argument_separator_and_quoted_arguments(self):
        template = BUNDLE / "share/applications/io.github.beamiter.anvil.desktop"
        original = template.read_text()
        try:
            template.write_text(original.replace('\nExec=anvil\n',
                                '\nExec=anvil --fixture "two words"\n'))
            home = self.root / "arguments home"
            result = self.install("release", home)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.check_launches(home, ("--fixture", "two words"))
        finally:
            template.write_text(original)

    def test_unrepresentable_home_is_rejected_before_binary_replacement(self):
        for name in ("equal=home", "percent%home", "newline\nhome"):
            with self.subTest(home=name):
                home = self.root / name
                binary = home / ".local/bin/anvil"
                binary.parent.mkdir(parents=True)
                binary.write_text("previous binary")
                result = self.install("release", home)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(binary.read_text(), "previous binary")


if __name__ == "__main__":
    unittest.main()
