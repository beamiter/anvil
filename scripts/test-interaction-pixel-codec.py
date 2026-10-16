#!/usr/bin/env python3
"""Offline synthetic RGB tests only: no display, socket, PTY, or native process.

The independent reference packer and tiny public-SGR renderer produce RGB
fixtures. Decoder calls receive only RGB bytes, dimensions and expected values.
These tests do not establish that VTE/X11 renders the fixture correctly; a later
authorized screenshot gate must establish that separately.
"""

import importlib.util
import pathlib
import random
import re
import sys
import unittest


sys.dont_write_bytecode = True
PATH = pathlib.Path(__file__).with_name("interaction-pixel-codec.py")
SPEC = importlib.util.spec_from_file_location("interaction_pixel_codec", PATH)
CODEC = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CODEC
SPEC.loader.exec_module(CODEC)

# Deliberately independent from the module's palette, packing and CRC helpers.
COLORS = ((0, 0, 0), (255, 255, 255), (255, 0, 255), (0, 255, 255))
BACKGROUND = (37, 53, 71)


def reference_crc(data):
    # Polynomial long division, rather than the production shift-register loop.
    polynomial = int.from_bytes(data, "big") << 8
    while polynomial.bit_length() >= 9:
        polynomial ^= 0x107 << (polynomial.bit_length() - 9)
    return polynomial


def reference_symbols(pane, sequence):
    identity = (pane << 20) | sequence
    check = reference_crc(b"A04P\x01" + identity.to_bytes(6, "big"))
    bits = format(pane, "022b") + format(sequence, "020b") + format(check, "08b")
    return [1, 2, 3] + [int(bits[i:i + 2], 2) for i in range(0, 50, 2)] + [3, 1, 2]


def ansi_cells(ansi):
    """Reject every escape other than exact public truecolor SGR and reset."""
    cell = re.compile(r"\x1b\[48;2;(\d+);(\d+);(\d+)m ")
    output = []
    if not ansi.startswith("\x1b[0m"):
        raise AssertionError("missing inherited-attribute reset")
    cursor = 4
    while cursor < len(ansi) and ansi[cursor:] != "\x1b[0m":
        match = cell.match(ansi, cursor)
        if match is None:
            raise AssertionError("unexpected control sequence or printable character")
        color = tuple(int(value) for value in match.groups())
        if color not in COLORS:
            raise AssertionError("non-canonical color")
        output.append(color)
        cursor = match.end()
    if ansi[cursor:] != "\x1b[0m":
        raise AssertionError("missing final reset")
    return output


def canvas(width, height, padding=0):
    stride = width * 3 + padding
    pixels = bytearray()
    for _ in range(height):
        pixels.extend(bytes(BACKGROUND) * width)
        pixels.extend(b"\xa5" * padding)
    return pixels, stride


def paint(pixels, stride, x, y, cw, ch, cells):
    for index, color in enumerate(cells):
        for row in range(y, y + ch):
            start = row * stride + (x + index * cw) * 3
            pixels[start:start + cw * 3] = bytes(color) * cw


def fixture(pane=200123, sequence=19, cw=10, ch=20, x=7, y=11, padding=0, symbols=None):
    width, height = x + 31 * cw + 9, y + ch + 13
    pixels, stride = canvas(width, height, padding)
    if symbols is None:
        symbols = reference_symbols(pane, sequence)
    paint(pixels, stride, x, y, cw, ch, [COLORS[value] for value in symbols])
    return pixels, width, height, stride


def crop(pixels, width, height, stride, left=0, right=0, top=0, bottom=0):
    new_width, new_height = width - left - right, height - top - bottom
    cropped = bytearray()
    for y in range(top, height - bottom):
        start = y * stride + left * 3
        cropped.extend(pixels[start:start + new_width * 3])
    return cropped, new_width, new_height, new_width * 3


class CodecTests(unittest.TestCase):
    def decode(self, sample, pane=200123, sequence=19, cw=10, ch=20, **kwargs):
        pixels, width, height, stride = sample
        return CODEC.decode_rgb(pixels, width, height, expected_pane_id=pane,
                                expected_sequence=sequence, cell_width=cw,
                                cell_height=ch, stride=stride, **kwargs)

    def test_crc_known_check_vector_and_independent_reference(self):
        self.assertEqual(CODEC.crc8_atm(b"123456789"), 0xF4)
        for data in (b"", b"\x00", b"\xff", b"A04P\x01\x00\x01\x02\x03\x04\x05"):
            self.assertEqual(CODEC.crc8_atm(data), reference_crc(data))

    def test_encoding_is_fixed_width_public_ansi_only(self):
        for pane, sequence in ((0, 0), (200123, 19), (4194303, 1048575)):
            ansi = CODEC.encode_marker(pane, sequence)
            cells = ansi_cells(ansi)
            self.assertEqual(len(cells), 31)
            self.assertEqual(cells, [COLORS[value] for value in reference_symbols(pane, sequence)])
            self.assertNotIn("\x1b]", ansi)
            self.assertNotIn("\n", ansi)
            self.assertNotIn("\r", ansi)

    def test_encoded_ansi_renders_and_decodes_without_text_side_channel(self):
        width, height, cw, ch, x, y = 350, 60, 10, 20, 13, 17
        pixels, stride = canvas(width, height)
        paint(pixels, stride, x, y, cw, ch, ansi_cells(CODEC.encode_marker(200123, 19)))
        result = self.decode((pixels, width, height, stride))
        self.assertTrue(result.matched)
        self.assertEqual(result.reason, "matched")
        marker = result.markers[0]
        self.assertEqual((marker.x, marker.y, marker.width, marker.height, marker.cell_width), (13, 17, 310, 20, 10))
        self.assertEqual((marker.pane_id, marker.sequence), (200123, 19))

    def test_arbitrary_origins_and_supported_scales(self):
        for cw, ch in ((1, 1), (8, 16), (9, 19), (10, 20), (12, 24), (20, 40)):
            for x, y in ((0, 0), (3, 5), (14, 29)):
                with self.subTest(cw=cw, ch=ch, x=x, y=y):
                    result = self.decode(fixture(cw=cw, ch=ch, x=x, y=y), cw=cw, ch=ch)
                    self.assertTrue(result.matched, result.reason)
                    self.assertEqual((result.markers[0].x, result.markers[0].y), (x, y))

    def test_candidate_width_inference_is_bounded(self):
        sample = fixture(cw=12, ch=24)
        result = self.decode(sample, cw=None, ch=24, candidate_cell_widths=(8, 10, 12))
        self.assertTrue(result.matched)
        self.assertEqual(result.markers[0].cell_width, 12)
        self.assertFalse(self.decode(sample, cw=None, ch=24, candidate_cell_widths=(8, 10)).matched)

    def test_zero_and_maximum_identity_and_sequence(self):
        for pane, seq in ((0, 0), (4194303, 0), (0, 1048575), (4194303, 1048575)):
            self.assertTrue(self.decode(fixture(pane=pane, sequence=seq), pane=pane, sequence=seq).matched)

    def test_seeded_reference_packets_round_trip(self):
        random_source = random.Random(1042026)
        for _ in range(100):
            pane = random_source.randrange(4194304)
            sequence = random_source.randrange(1048576)
            result = self.decode(fixture(pane=pane, sequence=sequence, cw=2, ch=3),
                                 pane=pane, sequence=sequence, cw=2, ch=3)
            self.assertTrue(result.matched, result.reason)
            marker = result.markers[0]
            self.assertEqual((marker.pane_id, marker.sequence), (pane, sequence))

    def test_wrong_pane_wrong_sequence_and_stale(self):
        sample = fixture()
        self.assertEqual(self.decode(sample, pane=200124).reason, "wrong_pane")
        self.assertEqual(self.decode(sample, sequence=18).reason, "wrong_sequence")
        result = self.decode(sample, sequence=20)
        self.assertFalse(result.matched)
        self.assertEqual(result.reason, "stale_sequence")
        self.assertEqual(result.markers[0].sequence, 19)

    def test_expected_absent_before_and_present_after(self):
        self.assertFalse(self.decode(fixture(sequence=18)).matched)
        self.assertTrue(self.decode(fixture(sequence=19)).matched)

    def test_all_four_edge_clippings_fail(self):
        sample = fixture(x=0, y=0)
        pixels, width, height, stride = sample
        # Trim ordinary margins first so each remaining cut clips a guard/row.
        exact = crop(pixels, width, height, stride, right=9, bottom=13)
        self.assertTrue(self.decode(exact).matched)
        for edge in ("left", "right", "top", "bottom"):
            with self.subTest(edge=edge):
                self.assertFalse(self.decode(crop(*exact, **{edge: 1})).matched)

    def test_wrong_scale_or_cell_height_fails(self):
        sample = fixture()
        for cw, ch in ((9, 20), (11, 20), (10, 19), (10, 21), (5, 10), (20, 40)):
            with self.subTest(cw=cw, ch=ch):
                self.assertFalse(self.decode(sample, cw=cw, ch=ch).matched)

    def test_each_single_payload_bit_flip_fails_crc(self):
        original = reference_symbols(200123, 19)
        for cell in range(3, 28):
            for bit in (1, 2):
                symbols = original[:]
                symbols[cell] ^= bit
                result = self.decode(fixture(symbols=symbols))
                self.assertFalse(result.matched)
                self.assertEqual(result.reason, "checksum_mismatch")

    def test_each_guard_cell_corruption_fails(self):
        for cell in (0, 1, 2, 28, 29, 30):
            symbols = reference_symbols(200123, 19)
            symbols[cell] = (symbols[cell] + 1) % 4
            self.assertFalse(self.decode(fixture(symbols=symbols)).matched)

    def test_one_off_center_pixel_corruption_fails(self):
        sample = fixture()
        pixels, _, _, stride = sample
        offset = (11 + 3) * stride + (7 + 7 * 10 + 1) * 3
        pixels[offset:offset + 3] = bytes(BACKGROUND)
        result = self.decode(sample)
        self.assertFalse(result.matched)
        self.assertEqual(result.reason, "non_uniform_cells")

    def test_padded_stride_is_respected(self):
        self.assertTrue(self.decode(fixture(padding=17)).matched)

    def test_moderate_rgb_jitter_within_explicit_tolerance(self):
        sample = fixture()
        pixels, width, _, stride = sample
        for y in range(11, 31):
            for x in range(7, 317):
                for channel in range(3):
                    offset = y * stride + x * 3 + channel
                    pixels[offset] = 12 if pixels[offset] == 0 else 243
        self.assertTrue(self.decode(sample, color_tolerance=12).matched)
        self.assertFalse(self.decode(sample, color_tolerance=11).matched)

    def test_partial_ansi_zeroed_or_missing_guard_fails(self):
        cells = ansi_cells(CODEC.encode_marker(200123, 19))
        for emitted in (0, 1, 2, 3, 14, 28, 29, 30):
            pixels, stride = canvas(340, 50)
            paint(pixels, stride, 7, 11, 10, 20, cells[:emitted])
            self.assertFalse(self.decode((pixels, 340, 50, stride)).matched)
        sample = fixture()
        pixels, width, height, stride = sample
        for y in range(11, 31):
            start = y * stride + (7 + 28 * 10) * 3
            pixels[start:start + 30 * 3] = b"\x00" * (30 * 3)
        self.assertFalse(self.decode(sample).matched)

    def test_no_marker_and_random_nonterminal_pixels(self):
        pixels, stride = canvas(350, 60)
        self.assertEqual(self.decode((pixels, 350, 60, stride)).reason, "marker_not_found")
        random_source = random.Random(104)
        pixels = bytes(random_source.randrange(256) for _ in range(350 * 60 * 3))
        self.assertFalse(self.decode((pixels, 350, 60, 1050)).matched)

    def test_multiple_marker_coordinates_and_duplicate_expected_failure(self):
        pixels, stride = canvas(350, 100)
        paint(pixels, stride, 7, 5, 10, 20, [COLORS[i] for i in reference_symbols(200123, 18)])
        paint(pixels, stride, 7, 51, 10, 20, [COLORS[i] for i in reference_symbols(200123, 19)])
        sample = (pixels, 350, 100, stride)
        result = self.decode(sample)
        self.assertTrue(result.matched)
        self.assertEqual([(marker.y, marker.sequence) for marker in result.markers], [(5, 18), (51, 19)])
        paint(pixels, stride, 7, 5, 10, 20, [COLORS[i] for i in reference_symbols(200123, 19)])
        result = self.decode(sample)
        self.assertFalse(result.matched)
        self.assertEqual(result.reason, "ambiguous_expected_marker")

    def test_guard_can_touch_same_color_surrounding_pixels(self):
        pixels, width, height, stride = fixture()
        for y in range(11, 31):
            pixels[y * stride:y * stride + 7 * 3] = bytes(COLORS[1]) * 7
            start = y * stride + 317 * 3
            pixels[start:y * stride + width * 3] = bytes(COLORS[2]) * (width - 317)
        self.assertTrue(self.decode((pixels, width, height, stride)).matched)

    def test_candidate_limit_fails_closed(self):
        width, height = 32 * 40, 2 * 30
        pixels, stride = canvas(width, height)
        cells = [COLORS[value] for value in reference_symbols(200123, 19)]
        for y in range(0, height, 2):
            for x in range(0, width, 32):
                paint(pixels, stride, x, y, 1, 1, cells)
        result = self.decode((pixels, width, height, stride), cw=1, ch=1)
        self.assertFalse(result.matched)
        self.assertEqual(result.reason, "candidate_limit_exceeded")
        self.assertLessEqual(len(result.markers), 1024)

    def test_identity_and_sequence_never_wrap_or_truncate(self):
        for pane, sequence in ((-1, 0), (4194304, 0), (0, -1), (0, 1048576), (True, 0), (1.5, 0)):
            with self.assertRaises(ValueError):
                CODEC.encode_marker(pane, sequence)

    def test_invalid_rgb_and_geometry_contracts_are_rejected(self):
        sample = fixture()
        pixels, width, height, stride = sample
        kwargs = dict(expected_pane_id=200123, expected_sequence=19, cell_width=10, cell_height=20)
        with self.assertRaises(TypeError):
            CODEC.decode_rgb("A04K:19", width, height, **kwargs)
        with self.assertRaises(ValueError):
            CODEC.decode_rgb(pixels[:-1], width, height, **kwargs)
        with self.assertRaises(ValueError):
            CODEC.decode_rgb(pixels, width, height, stride=stride - 1, **kwargs)
        for key, value in (("cell_width", 0), ("cell_height", 0), ("cell_height", 129), ("color_tolerance", 49), ("expected_sequence", -1)):
            bad = dict(kwargs)
            bad[key] = value
            with self.assertRaises(ValueError):
                CODEC.decode_rgb(pixels, width, height, **bad)
        with self.assertRaises(ValueError):
            CODEC.decode_rgb(b"", 4096, 4096, **kwargs)
        for widths in ((), tuple(range(1, 34)), (0,), (65,)):
            bad = dict(kwargs)
            bad["cell_width"] = None
            bad["candidate_cell_widths"] = widths
            with self.assertRaises(ValueError):
                CODEC.decode_rgb(pixels, width, height, **bad)


if __name__ == "__main__":
    unittest.main(verbosity=2)
