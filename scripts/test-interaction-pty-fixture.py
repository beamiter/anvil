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
    def test_barcode_chunk_ends_with_complete_frame_not_wrapping_padding(self):
        import sys
        spec = importlib.util.spec_from_file_location("fixture_codec", Path(__file__).with_name("interaction-pixel-codec.py"))
        codec = importlib.util.module_from_spec(spec); sys.modules[spec.name] = codec; spec.loader.exec_module(codec)
        marker = codec.encode_marker(123, 7)
        data = fixture.block(7, marker)
        self.assertEqual(len(data), 4096)
        self.assertTrue(data.endswith(marker.encode("ascii") + b"\r\n"))
        self.assertNotIn(b"\x1b]", data)
        self.assertGreater(data.count(marker.encode("ascii")), 1)
    def test_barcode_frame_cannot_exceed_chunk(self):
        with self.assertRaises(ValueError): fixture.block(1, 'x' * 4096)
if __name__ == "__main__": unittest.main()

