#!/usr/bin/env python3
"""Pure observer contracts: no display, socket, X11 library or native app."""
import ctypes
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import struct
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import zlib

spec = importlib.util.spec_from_file_location("interaction_pixel_observer", Path(__file__).with_name("interaction-pixel-observer.py"))
observer = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = observer
# Enforce import purity, including indirect native-library initialization.
with patch.object(ctypes, "CDLL", side_effect=AssertionError("native library forbidden in pure tests")):
    spec.loader.exec_module(observer)


def command(**changes):
    value = dict(kind="marker_wait", name="key-0", rect=[0, 0, 32, 12],
                 pane_id=123, sequence=1, cell_width=2, cell_height=4)
    value.update(changes)
    return value


def pixels(width, height, fn):
    return bytes(channel for y in range(height) for x in range(width) for channel in fn(x, y))


class Protocol(unittest.TestCase):
    def test_valid_marker(self):
        self.assertEqual(observer.validate_command(command()), command())
        self.assertEqual(observer.read_command(io.BytesIO(json.dumps(command()).encode() + b"\n")), command())
        self.assertIsNone(observer.read_command(io.BytesIO()))

    def test_safe_names_only(self):
        for name in ("", "../key", "A", "has space", "x" * 65, None, 12, "line\n"):
            with self.subTest(name=name), self.assertRaises(observer.ProtocolError):
                observer.validate_command(command(name=name))

    def test_bounded_rectangle_and_strict_integer_types(self):
        for rect in ([0, 0, 641, 20], [0, 0, 20, 257], [1599, 0, 2, 1],
                     [0, 999, 1, 2], [-1, 0, 1, 1], [True, 0, 1, 1],
                     [0, 0, 1.0, 1], [0, 0, 0, 1], [0, 0, 1], "0,0,1,1"):
            with self.subTest(rect=rect), self.assertRaises(observer.ProtocolError):
                observer.validate_command(command(rect=rect))
        observer.validate_command(command(rect=[960, 744, 640, 256]))

    def test_unknown_fields_and_identity_rejected(self):
        for update in (dict(pid=42), dict(pane_id=0), dict(pane_id=True),
                       dict(sequence=-1), dict(sequence=2 ** 32), dict(cell_width=0),
                       dict(cell_height=65), dict(text="forbidden"), dict(kind="probe")):
            with self.subTest(update=update), self.assertRaises(observer.ProtocolError):
                observer.validate_command(command(**update))
        missing = command(); del missing["pane_id"]
        with self.assertRaises(observer.ProtocolError): observer.validate_command(missing)

    def test_framing_and_malformed_json_fail_closed(self):
        for line in (b"{}", b"x\n", b"\xff\n", b"{}" + b" " * 4094 + b"\n",
                     b'{"kind":"marker_wait","kind":"divider"}\n',
                     b'{"kind":NaN}\n', b'[]\n', b'\n'):
            with self.subTest(line=line[:60]), self.assertRaises(observer.ProtocolError):
                observer.read_command(io.BytesIO(line))

    def test_read_does_not_consume_following_command(self):
        stream = io.BytesIO((json.dumps(command()) + "\n").encode() * 2)
        self.assertEqual(observer.read_command(stream)["name"], "key-0")
        self.assertEqual(observer.read_command(stream)["name"], "key-0")

    def test_divider_gtk_axis_validation(self):
        value = dict(kind="divider", name="drag-0", rect=[0, 0, 32, 12],
                     horizontal=True, expected=20, radius=10)
        self.assertEqual(observer.validate_command(value), value)
        with self.assertRaises(observer.ProtocolError):
            observer.validate_command(dict(value, horizontal=False))
        with self.assertRaises(observer.ProtocolError):
            observer.validate_command(dict(value, horizontal=1))

    def test_main_guard_cannot_open_display(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(ctypes, "CDLL") as library, patch("sys.stderr", io.StringIO()):
            self.assertEqual(observer.main(["unused"]), 2)
            library.assert_not_called()
        with patch.dict(os.environ, {"ANVIL_BUDGET_ISOLATED_DISPLAY": "1", "DISPLAY": "remote:0"}, clear=True), patch.object(ctypes, "CDLL") as library, patch("sys.stderr", io.StringIO()):
            self.assertEqual(observer.main(["unused"]), 2)
            library.assert_not_called()


class RawPixels(unittest.TestCase):
    def test_bgrx_conversion_and_row_padding(self):
        # Two pixels, two rows, and deliberately nonzero ignored padding.
        source = bytes([30, 20, 10, 99, 60, 50, 40, 98, 1, 2, 3, 4,
                        90, 80, 70, 97, 120, 110, 100, 96, 5, 6, 7, 8])
        self.assertEqual(observer.bgrx_to_rgb(source, 2, 2, 12),
                         bytes([10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120]))
        with self.assertRaises(ValueError): observer.bgrx_to_rgb(source[:-1], 2, 2, 12)
        with self.assertRaises(ValueError): observer.bgrx_to_rgb(source, 2, 2, 7)

    def test_png_exact_rgb_roundtrip_and_chunk_crc(self):
        rgb = bytes(range(18))
        data = observer.encode_png_rgb(rgb, 3, 2)
        self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
        offset, kinds, compressed = 8, [], b""
        while offset < len(data):
            size = struct.unpack(">I", data[offset:offset + 4])[0]
            kind = data[offset + 4:offset + 8]
            payload = data[offset + 8:offset + 8 + size]
            crc = struct.unpack(">I", data[offset + 8 + size:offset + 12 + size])[0]
            self.assertEqual(crc, zlib.crc32(kind + payload) & 0xffffffff)
            kinds.append(kind)
            if kind == b"IHDR":
                self.assertEqual(struct.unpack(">IIBBBBB", payload), (3, 2, 8, 2, 0, 0, 0))
            if kind == b"IDAT": compressed += payload
            offset += size + 12
        self.assertEqual(offset, len(data))
        self.assertEqual(kinds, [b"IHDR", b"IDAT", b"IEND"])
        self.assertEqual(zlib.decompress(compressed), b"\x00" + rgb[:9] + b"\x00" + rgb[9:])

    def test_invalid_rgb_lengths(self):
        for rgb in (b"", b"1234", "123"):
            with self.assertRaises(ValueError): observer.encode_png_rgb(rgb, 1, 1)

    def test_owned_directory_png_digest_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = observer.EvidenceDirectory(directory)
            try:
                cmd = command(rect=[0, 0, 1, 1], kind="marker_before")
                path, digest = evidence.save(cmd, b"\x00\x11\x22")
                self.assertEqual(Path(path).name, "key-0-before.png")
                self.assertEqual(digest, hashlib.sha256((Path(directory) / path).read_bytes()).hexdigest())
                self.assertEqual(stat.S_IMODE((Path(directory) / path).stat().st_mode), 0o600)
                with self.assertRaises(FileExistsError): evidence.save(cmd, b"\x00\x11\x22")
            finally:
                evidence.close()

    def test_output_directory_requires_private_permissions_and_no_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            loose = root / "loose"; loose.mkdir(mode=0o755)
            with self.assertRaises(ValueError): observer.EvidenceDirectory(loose)
            link = root / "link"; link.symlink_to(root, target_is_directory=True)
            with self.assertRaises(OSError): observer.EvidenceDirectory(link)

    def test_output_file_symlink_is_never_followed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "target"; target.write_bytes(b"unchanged")
            (root / "key-0-final.png").symlink_to(target)
            evidence = observer.EvidenceDirectory(root)
            try:
                with self.assertRaises(FileExistsError):
                    evidence.save(command(rect=[0, 0, 1, 1]), b"123")
                self.assertEqual(target.read_bytes(), b"unchanged")
            finally:
                evidence.close()


class Divider(unittest.TestCase):
    def test_side_by_side_horizontal_gtk_means_x(self):
        rgb = pixels(30, 8, lambda x, y: (10, 20, 30) if x < 16 else (200, 210, 220))
        found = observer.detect_divider(rgb, 30, 8, horizontal=True, expected=15, radius=6)
        self.assertTrue(found["matched"])
        self.assertEqual(found["position"], 16)
        self.assertEqual(found["contrast"], 8 * 3 * 190)

    def test_stacked_gtk_means_y(self):
        rgb = pixels(12, 30, lambda x, y: (0, 0, 0) if y < 11 else (255, 255, 255))
        found = observer.detect_divider(rgb, 12, 30, horizontal=False, expected=12, radius=4)
        self.assertTrue(found["matched"])
        self.assertEqual(found["position"], 11)

    def test_low_contrast_rejected(self):
        rgb = pixels(30, 8, lambda x, y: (0, 0, 0) if x < 15 else (4, 4, 4))
        found = observer.detect_divider(rgb, 30, 8, horizontal=True, expected=15, radius=8)
        self.assertFalse(found["matched"])
        self.assertEqual(found["reason"], "low-contrast")
        self.assertIsNone(found["position"])

    def test_ambiguous_separated_peaks_rejected(self):
        rgb = pixels(30, 8, lambda x, y: (240, 240, 240) if 10 <= x < 20 else (0, 0, 0))
        found = observer.detect_divider(rgb, 30, 8, horizontal=True, expected=15, radius=8)
        self.assertFalse(found["matched"])
        self.assertEqual(found["reason"], "ambiguous-peaks")
        self.assertIsNone(found["position"])

    def test_two_pixel_strip_counts_as_one_edge(self):
        rgb = pixels(30, 8, lambda x, y: (240, 240, 240) if 14 <= x < 16 else (0, 0, 0))
        found = observer.detect_divider(rgb, 30, 8, horizontal=True, expected=14, radius=8)
        self.assertTrue(found["matched"])
        self.assertEqual(found["position"], 14)

    def test_edge_outside_search_region_not_invented(self):
        rgb = pixels(30, 8, lambda x, y: (240, 240, 240) if x >= 25 else (0, 0, 0))
        found = observer.detect_divider(rgb, 30, 8, horizontal=True, expected=10, radius=3)
        self.assertFalse(found["matched"])


class FakeClock:
    def __init__(self): self.now = 1000; self.sleeps = []
    def __call__(self): return self.now
    def sleep(self, seconds): self.sleeps.append(seconds); self.now += round(seconds * 1e9)


class FakeScreen:
    def __init__(self, clock): self.clock = clock; self.count = 0
    def capture(self, rect):
        self.count += 1
        start = self.clock(); self.clock.now += 100
        return observer.Capture(bytes([self.count % 256, 0, 0]) * (rect[2] * rect[3]), start, self.clock())


class FakeEvidence:
    def __init__(self): self.saved = []
    def save(self, cmd, rgb):
        self.saved.append((cmd, rgb))
        return "/evidence/final.png", hashlib.sha256(rgb).hexdigest()


class Execution(unittest.TestCase):
    def make(self, match_at=3):
        clock, evidence = FakeClock(), FakeEvidence()
        screen = FakeScreen(clock)
        calls = []
        def decode(rgb, width, height, **kwargs):
            calls.append((rgb, width, height, kwargs))
            matched = screen.count >= match_at
            return SimpleNamespace(matched=matched, reason="matched" if matched else "absent", markers=[])
        runner = observer.Observer(screen, evidence, decode, clock=clock,
                                   cpu_clock=clock, sleep=clock.sleep)
        return runner, screen, evidence, clock, calls

    def test_wait_reports_intervals_and_last_absent_and_saves_only_final(self):
        runner, screen, evidence, clock, calls = self.make()
        result = runner.execute(command())
        self.assertTrue(result["matched"])
        self.assertEqual(result["capture_count"], 3)
        self.assertEqual(len(result["samples"]), 3)
        self.assertEqual(result["last_absent"], result["samples"][1])
        for sample in result["samples"]:
            self.assertIs(type(sample["started_ns"]), int)
            self.assertGreater(sample["finished_ns"], sample["started_ns"])
        self.assertEqual(len(evidence.saved), 1)
        self.assertEqual(len(clock.sleeps), 2)
        self.assertEqual(calls[-1][3], dict(cell_width=2, cell_height=4, expected_pane_id=123, expected_sequence=1))
        self.assertEqual(result["final_png_sha256"], hashlib.sha256(evidence.saved[0][1]).hexdigest())
        self.assertGreater(result["elapsed_ns"], 0)
        self.assertGreater(result["cpu_ns"], 0)

    def test_before_exactly_one_sample_even_when_absent(self):
        runner, screen, evidence, clock, calls = self.make()
        result = runner.execute(command(kind="marker_before"))
        self.assertFalse(result["matched"])
        self.assertEqual(screen.count, 1)
        self.assertEqual(clock.sleeps, [])
        self.assertEqual(result["last_absent"], result["samples"][0])

    def test_immediate_match_has_no_absent_and_no_sleep(self):
        runner, screen, evidence, clock, calls = self.make(match_at=1)
        result = runner.execute(command())
        self.assertTrue(result["matched"])
        self.assertIsNone(result["last_absent"])
        self.assertEqual(clock.sleeps, [])

    def test_wait_stops_at_deadline_with_no_capture_after_it(self):
        runner, screen, evidence, clock, calls = self.make(match_at=999)
        result = runner.execute(command())
        self.assertFalse(result["matched"])
        self.assertLessEqual(result["capture_count"], observer.MAX_SAMPLES)
        self.assertLessEqual(result["elapsed_ns"], observer.WAIT_NS)
        self.assertTrue(all(sample["started_ns"] < 1000 + observer.WAIT_NS for sample in result["samples"]))
        self.assertEqual(len(evidence.saved), 1)

    def test_sample_cap_independent_of_clock_progress(self):
        runner, screen, evidence, clock, calls = self.make(match_at=999)
        runner.sleep = lambda seconds: None
        result = runner.execute(command())
        self.assertEqual(result["capture_count"], 64)
        self.assertFalse(result["matched"])

    def test_divider_never_calls_marker_decoder(self):
        runner, screen, evidence, clock, calls = self.make()
        screen.capture = lambda rect: observer.Capture(pixels(32, 12, lambda x, y: (0, 0, 0) if x < 16 else (255, 255, 255)), 100, 101)
        result = runner.execute(dict(kind="divider", name="drag-0-before", rect=[0, 0, 32, 12],
                                     horizontal=True, expected=16, radius=5))
        self.assertEqual(calls, [])
        self.assertTrue(result["matched"])
        self.assertEqual(result["position"], 16)
        self.assertEqual(result["capture_count"], 1)

    def test_real_codec_accepts_only_the_captured_rgb_marker(self):
        decoder = observer.load_decoder()
        codec = sys.modules["interaction_pixel_codec"]
        width, height, x0, y0, cw, ch = 70, 12, 3, 3, 2, 4
        rgb = bytearray(bytes([37, 41, 43]) * (width * height))
        for cell, symbol in enumerate(codec.marker_symbols(123, 1)):
            color = bytes(codec.PALETTE[symbol])
            for y in range(y0, y0 + ch):
                for x in range(x0 + cell * cw, x0 + (cell + 1) * cw):
                    offset = (y * width + x) * 3
                    rgb[offset:offset + 3] = color
        clock, evidence = FakeClock(), FakeEvidence()
        screen = SimpleNamespace(capture=lambda rect: observer.Capture(bytes(rgb), 100, 101))
        runner = observer.Observer(screen, evidence, decoder, clock=clock,
                                   cpu_clock=clock, sleep=clock.sleep)
        result = runner.execute(command(rect=[0, 0, width, height]))
        self.assertTrue(result["matched"], result["reason"])
        self.assertEqual(result["markers"][0]["pane_id"], 123)
        self.assertEqual(result["markers"][0]["sequence"], 1)
        self.assertEqual(result["markers"][0]["x"], x0)
        self.assertEqual(result["markers"][0]["y"], y0)
        self.assertEqual(result["capture_count"], 1)

    def test_serve_emits_one_numeric_json_record_per_valid_command(self):
        runner, screen, evidence, clock, calls = self.make(match_at=1)
        output = io.StringIO()
        observer.serve(runner, io.BytesIO((json.dumps(command()) + "\n").encode()), output)
        rows = output.getvalue().splitlines()
        self.assertEqual(len(rows), 1)
        self.assertTrue(json.loads(rows[0])["matched"])

    def test_serve_invalid_command_never_captures_or_continues(self):
        runner, screen, evidence, clock, calls = self.make(match_at=1)
        stream = io.BytesIO(b'{}\n' + (json.dumps(command()) + "\n").encode())
        output = io.StringIO()
        with self.assertRaises(observer.ProtocolError): observer.serve(runner, stream, output)
        self.assertEqual(screen.count, 0)
        self.assertEqual(output.getvalue(), "")


class NativeLayoutWithoutNativeExecution(unittest.TestCase):
    def fake(self, *, valid=True):
        data = ctypes.create_string_buffer(bytes([3, 2, 1, 0]) * 2)
        image = observer.XImage(width=2, height=1, xoffset=0, format=2,
                                data=ctypes.addressof(data), byte_order=0, depth=24,
                                bytes_per_line=8, bits_per_pixel=32 if valid else 16,
                                red_mask=0xff0000, green_mask=0xff00, blue_mask=0xff)
        class Library:
            destroyed = 0
            def XGetImage(self, *args): return ctypes.pointer(image)
            def XDestroyImage(self, pointer): self.destroyed += 1
        screen = object.__new__(observer.Screen)
        screen.library, screen.display, screen.root = Library(), object(), 1
        screen.width, screen.height = 100, 100
        return screen, data

    def test_destroyed_after_success(self):
        screen, data = self.fake()
        result = screen.capture([0, 0, 2, 1])
        self.assertEqual(result.rgb, b"\x01\x02\x03" * 2)
        self.assertEqual(screen.library.destroyed, 1)

    def test_destroyed_after_layout_rejection(self):
        screen, data = self.fake(valid=False)
        with self.assertRaises(RuntimeError): screen.capture([0, 0, 2, 1])
        self.assertEqual(screen.library.destroyed, 1)

    def test_destroyed_after_conversion_failure(self):
        screen, data = self.fake()
        with patch.object(observer, "bgrx_to_rgb", side_effect=ValueError("injected")):
            with self.assertRaises(ValueError): screen.capture([0, 0, 2, 1])
        self.assertEqual(screen.library.destroyed, 1)

    def test_roi_rejected_before_capture(self):
        screen, data = self.fake()
        with self.assertRaises(observer.ProtocolError): screen.capture([99, 0, 2, 1])
        self.assertEqual(screen.library.destroyed, 0)



class DividerMotion(unittest.TestCase):
    def image(self, old, new=None, first=220, second=20):
        return bytes(v for y in range(12) for x in range(64)
                     for v in ([first]*3 if x >= old and (new is None or x < new) else [second]*3))
    def test_shared_union_moving_edge(self):
        result=observer.divider_motion(self.image(32),self.image(44),64,12,horizontal=True,expected=32,delta=12)
        self.assertEqual(result['shift'],12);self.assertTrue(result['old_edge_disappeared'])
    def test_stationary_stripe_is_ambiguous(self):
        image=self.image(32,44)
        with self.assertRaises(ValueError):observer.divider_motion(image,image,64,12,horizontal=True,expected=32,delta=12)
    def test_static_edges_with_changing_strength_are_not_movement(self):
        # Dominant edge swaps, but both edges remain above low-contrast threshold.
        def image(a,b,c):
            return bytes(v for y in range(12) for x in range(64) for v in ([a]*3 if x<32 else [b]*3 if x<44 else [c]*3))
        with self.assertRaises(ValueError):observer.divider_motion(image(0,200,160),image(0,40,240),64,12,horizontal=True,expected=32,delta=12)

if __name__ == "__main__":
    unittest.main()
