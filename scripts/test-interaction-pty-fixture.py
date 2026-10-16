#!/usr/bin/env python3
"""Pure fixture checks; intentionally do not launch a PTY or signal a process."""
import importlib.util
from pathlib import Path
import unittest
spec = importlib.util.spec_from_file_location("fixture", Path(__file__).with_name("interaction-pty-fixture.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
class Fixture(unittest.TestCase):
    def test_wallclock_budget(self):
        self.assertEqual(fixture.scheduled_bytes(1_000_000_000, 2_000_000_000), 256 * 1024)
        self.assertEqual(fixture.scheduled_bytes(2, 1), 0)
    def test_fixed_numeric_plaintext(self):
        block = fixture.block(123)
        self.assertEqual(len(block), 4096)
        self.assertIn(b"A04K000123", block)
        self.assertNotIn(b"\x1b", block)
        self.assertTrue(all(32 <= v <= 126 or v in (10, 13) for v in block))
if __name__ == "__main__": unittest.main()
