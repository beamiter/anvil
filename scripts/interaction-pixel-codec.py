#!/usr/bin/env python3
"""Bounded public-ANSI marker and an independent, RGB-only pixel decoder.

This is short-gate instrumentation, not a production protocol. A marker is one
terminal row of 31 background-colored spaces. It contains a 22-bit pane identity,
20-bit sequence, and CRC-8/ATM. A child can use its own stable os.getpid() as its
identity; callers must reject out-of-range IDs and sequences, never truncate or
wrap them. Explicit per-child indices are also allowed if the caller guarantees
uniqueness. The codec does not read process state, terminal text, or app reports.

Layout, left to right (most significant dibit first):
  white/magenta/cyan | 25 payload cells | cyan/white/magenta
  payload = ((pane_id << 20) | sequence) << 8 | crc
  crc = CRC-8/ATM(b"A04P\\x01" + identity_and_sequence.to_bytes(6, "big"))

Each payload cell carries two bits using black, white, magenta, cyan in that
order. ANSI contains only SGR truecolor backgrounds, spaces, and final reset.
The encoder resets inherited SGR first, then resets again at the end. The caller
controls placement and line breaks; no OSC or trust control exists.

decode_rgb accepts a tightly packed or padded RGB buffer and explicit terminal
cell height. Width is explicit or selected from a bounded list. It searches
arbitrary pixel origins, verifies every pixel of every cell, both sync guards,
the exact vertical extent, checksum, and expected identity/sequence. Coordinates
are ROI-relative. No text/OCR/VTE/probe-output input is accepted. Caller must
separately verify that this ROI belongs to the visible, owned pane, that the
expected marker was absent before input, and preserve capture timing/bounds.

CRC8 is an accidental-corruption check, not authentication or a cryptographic
proof. The strict colors and guards add structure, but do not improve CRC8's
collision guarantee. A matched offline RGB test does not prove real rendering.
"""

from dataclasses import dataclass
from typing import Optional, Sequence, Tuple


PALETTE = ((0, 0, 0), (255, 255, 255), (255, 0, 255), (0, 255, 255))
LEFT_GUARD = (1, 2, 3)
RIGHT_GUARD = (3, 1, 2)
MARKER_COLUMNS = 31
MARKER_ROWS = 1
MAX_PANE_ID = (1 << 22) - 1
MAX_SEQUENCE = (1 << 20) - 1
CRC_DOMAIN = b"A04P\x01"
MAX_ROI_PIXELS = 2 * 1024 * 1024
MAX_ROI_DIMENSION = 4096
MAX_CANDIDATES = 1024
UNKNOWN_COLOR = 255


@dataclass(frozen=True)
class DecodedMarker:
    pane_id: int
    sequence: int
    checksum: int
    x: int
    y: int
    width: int
    height: int
    cell_width: int


@dataclass(frozen=True)
class DecodeResult:
    matched: bool
    reason: str
    markers: Tuple[DecodedMarker, ...]


def _integer(value: int, name: str, minimum: int, maximum: int) -> int:
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError("%s must be an integer in [%d, %d]" % (name, minimum, maximum))
    return value


def crc8_atm(data: bytes) -> int:
    """CRC-8/ATM: poly=0x07, init=0, refin=false, refout=false, xorout=0."""
    crc = 0
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = ((crc << 1) ^ (0x07 if crc & 0x80 else 0)) & 0xFF
    return crc


def marker_symbols(pane_id: int, sequence: int) -> Tuple[int, ...]:
    """Return the documented 31 cell symbols, principally for encoder review."""
    _integer(pane_id, "pane_id", 0, MAX_PANE_ID)
    _integer(sequence, "sequence", 0, MAX_SEQUENCE)
    identity = (pane_id << 20) | sequence
    crc = crc8_atm(CRC_DOMAIN + identity.to_bytes(6, "big"))
    payload = (identity << 8) | crc
    return LEFT_GUARD + tuple((payload >> shift) & 3 for shift in range(48, -1, -2)) + RIGHT_GUARD


def encode_marker(pane_id: int, sequence: int) -> str:
    """Return exactly 31 background-colored spaces, with no cursor movement."""
    # Clear inherited reverse/dim/conceal attributes before choosing colors.
    chunks = ["\x1b[0m"]
    for symbol in marker_symbols(pane_id, sequence):
        red, green, blue = PALETTE[symbol]
        chunks.append("\x1b[48;2;%d;%d;%dm " % (red, green, blue))
    chunks.append("\x1b[0m")
    return "".join(chunks)


def _classify_rgb(rgb: memoryview, width: int, height: int, stride: int, tolerance: int) -> bytearray:
    """Classify only the supplied RGB bytes; there is no external input path."""
    colors = bytearray([UNKNOWN_COLOR]) * (width * height)
    high = 255 - tolerance
    for y in range(height):
        source = y * stride
        target = y * width
        for x in range(width):
            p = source + x * 3
            red, green, blue = rgb[p], rgb[p + 1], rgb[p + 2]
            if red <= tolerance and green <= tolerance and blue <= tolerance:
                colors[target + x] = 0
            elif red >= high and green >= high and blue >= high:
                colors[target + x] = 1
            elif red >= high and green <= tolerance and blue >= high:
                colors[target + x] = 2
            elif red <= tolerance and green >= high and blue >= high:
                colors[target + x] = 3
    return colors


def decode_rgb(
    rgb: bytes,
    width: int,
    height: int,
    *,
    expected_pane_id: int,
    expected_sequence: int,
    cell_height: int,
    cell_width: Optional[int] = None,
    candidate_cell_widths: Sequence[int] = (8, 9, 10, 11, 12),
    stride: Optional[int] = None,
    color_tolerance: int = 16,
) -> DecodeResult:
    """Decode RGB pixels, returning all valid markers and an exact-match verdict.

    An invalid input contract raises ValueError/TypeError. A well-formed image
    with absent, clipped, damaged, stale, or wrong markers returns matched=False.
    Duplicate expected markers are ambiguous and fail closed. Diagnostic reason
    strings describe observed evidence; absence alone cannot distinguish clipping
    from missing content. Geometry/extent verification requires the full marker.
    Bounds cap allocation and scanning work, including adversarial marker count.
    """
    _integer(width, "width", 1, MAX_ROI_DIMENSION)
    _integer(height, "height", 1, MAX_ROI_DIMENSION)
    if width * height > MAX_ROI_PIXELS:
        raise ValueError("RGB ROI exceeds pixel bound")
    _integer(cell_height, "cell_height", 1, 128)
    _integer(color_tolerance, "color_tolerance", 0, 48)
    _integer(expected_pane_id, "expected_pane_id", 0, MAX_PANE_ID)
    _integer(expected_sequence, "expected_sequence", 0, MAX_SEQUENCE)
    if cell_width is not None:
        widths = {_integer(cell_width, "cell_width", 1, 64)}
    else:
        if not 1 <= len(candidate_cell_widths) <= 32:
            raise ValueError("provide between 1 and 32 candidate cell widths")
        widths = {_integer(value, "candidate_cell_width", 1, 64) for value in candidate_cell_widths}
    if stride is None:
        stride = width * 3
    _integer(stride, "stride", width * 3, MAX_ROI_DIMENSION * 3 + 4096)
    try:
        source = memoryview(rgb)
        if not source.c_contiguous:
            raise ValueError("RGB buffer must be contiguous")
        source = source.cast("B")
    except TypeError as exc:
        raise TypeError("rgb must be a binary buffer, never terminal text") from exc
    if len(source) != stride * height:
        raise ValueError("RGB byte length must equal stride * height")
    colors = _classify_rgb(source, width, height, stride, color_tolerance)
    markers = []
    rejected = set()
    candidate_count = 0

    def row_matches(x: int, y: int, cw: int, symbols: Tuple[int, ...]) -> bool:
        if not 0 <= y < height:
            return False
        offset = y * width + x
        for symbol in symbols:
            for pixel in range(offset, offset + cw):
                if colors[pixel] != symbol:
                    return False
            offset += cw
        return True

    # The middle left-guard run is magenta of exactly one cell width. Unlike
    # center-only sampling, run boundaries make a clipped first/last cell fail.
    # The white run can extend into the surrounding background, so anchor its
    # final cell at the magenta transition instead of assuming a dark margin.
    for y in range(height):
        row_start = y * width
        runs = []
        start = 0
        while start < width:
            color = colors[row_start + start]
            end = start + 1
            while end < width and colors[row_start + end] == color:
                end += 1
            runs.append((color, start, end))
            start = end
        for index in range(1, len(runs) - 1):
            previous, middle, following = runs[index - 1:index + 2]
            if previous[0] != 1 or middle[0] != 2 or following[0] != 3:
                continue
            cw = middle[2] - middle[1]
            if cw not in widths or previous[2] - previous[1] < cw or following[2] - following[1] < cw:
                continue
            x = middle[1] - cw
            marker_width = MARKER_COLUMNS * cw
            if x + marker_width > width or y + cell_height > height:
                rejected.add("clipped_or_geometry_mismatch")
                continue
            symbols = tuple(colors[row_start + x + i * cw + cw // 2] for i in range(MARKER_COLUMNS))
            if symbols[:3] != LEFT_GUARD or symbols[-3:] != RIGHT_GUARD or UNKNOWN_COLOR in symbols:
                continue
            # Consider only the first scanline of a uniform band. This both
            # bounds repeated checksum work and prevents undersized cell-height
            # assumptions from accepting the interior of a taller marker.
            if row_matches(x, y - 1, cw, symbols):
                continue
            candidate_count += 1
            if candidate_count > MAX_CANDIDATES:
                return DecodeResult(False, "candidate_limit_exceeded", tuple(markers))
            if not all(row_matches(x, row, cw, symbols) for row in range(y, y + cell_height)):
                rejected.add("non_uniform_cells")
                continue
            if row_matches(x, y + cell_height, cw, symbols):
                rejected.add("clipped_or_geometry_mismatch")
                continue
            packet = 0
            for symbol in symbols[3:-3]:
                packet = (packet << 2) | symbol
            identity, checksum = packet >> 8, packet & 0xFF
            if checksum != crc8_atm(CRC_DOMAIN + identity.to_bytes(6, "big")):
                rejected.add("checksum_mismatch")
                continue
            markers.append(DecodedMarker(
                pane_id=identity >> 20,
                sequence=identity & MAX_SEQUENCE,
                checksum=checksum,
                x=x,
                y=y,
                width=marker_width,
                height=cell_height,
                cell_width=cw,
            ))
    expected = [marker for marker in markers if marker.pane_id == expected_pane_id and marker.sequence == expected_sequence]
    if len(expected) == 1:
        return DecodeResult(True, "matched", tuple(markers))
    if len(expected) > 1:
        return DecodeResult(False, "ambiguous_expected_marker", tuple(markers))
    same_pane = [marker for marker in markers if marker.pane_id == expected_pane_id]
    if same_pane:
        reason = "stale_sequence" if all(marker.sequence < expected_sequence for marker in same_pane) else "wrong_sequence"
    elif markers:
        reason = "wrong_pane"
    else:
        reason = next((value for value in (
            "checksum_mismatch", "non_uniform_cells", "clipped_or_geometry_mismatch"
        ) if value in rejected), "marker_not_found")
    return DecodeResult(False, reason, tuple(markers))
