#!/usr/bin/env python3
"""Bounded, independent X11 RGB observer for an explicitly isolated display.

Importing this module does not load X11 or open a display. The executable owns
one display connection; its collector owns the process lifetime. Only sampled
RGB pixels are passed to the marker codec, never VTE, PTY, text or probe state.
JSONL commands are fail-closed, <=4096 bytes including their newline. Each
command saves just its before/final ROI as a lossless PNG in an existing private
persistent directory. A marker wait samples at most 64 times over one second.
"""
import ctypes
from dataclasses import asdict, dataclass, is_dataclass
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import struct
import sys
import time
import zlib

MAX_COMMAND_BYTES = 4096
MAX_WIDTH = 640
MAX_HEIGHT = 256
MAX_SCREEN_WIDTH = 1600
MAX_SCREEN_HEIGHT = 1000
MAX_SAMPLES = 64
WAIT_NS = 1_000_000_000
POLL_NS = 16_000_000
NAME = re.compile(r"[a-z0-9-]{1,64}\Z")


class ProtocolError(ValueError):
    """Invalid input terminates the observer rather than guessing intent."""


def integer(value, label, low, high):
    if type(value) is not int or not low <= value <= high:
        raise ProtocolError(f"{label} must be an integer in {low}..{high}")
    return value


def validate_command(command):
    if type(command) is not dict:
        raise ProtocolError("command must be an object")
    kind = command.get("kind")
    if kind not in ("marker_before", "marker_wait", "divider"):
        raise ProtocolError("unknown command kind")
    name = command.get("name")
    if not isinstance(name, str) or NAME.fullmatch(name) is None:
        raise ProtocolError("unsafe evidence name")
    rect = command.get("rect")
    if type(rect) is not list or len(rect) != 4:
        raise ProtocolError("rect must be [x,y,width,height]")
    x = integer(rect[0], "x", 0, MAX_SCREEN_WIDTH - 1)
    y = integer(rect[1], "y", 0, MAX_SCREEN_HEIGHT - 1)
    width = integer(rect[2], "width", 1, MAX_WIDTH)
    height = integer(rect[3], "height", 1, MAX_HEIGHT)
    if x + width > MAX_SCREEN_WIDTH or y + height > MAX_SCREEN_HEIGHT:
        raise ProtocolError("rect exceeds bounded display")
    common = {"kind", "name", "rect"}
    normalized = dict(command)
    if kind == "divider":
        allowed = common | {"horizontal", "expected", "radius"}
        if type(command.get("horizontal")) is not bool:
            raise ProtocolError("horizontal must be a boolean")
        axis = width if command["horizontal"] else height
        integer(command.get("expected"), "expected", 1, axis - 1)
        integer(command.get("radius"), "radius", 0, min(128, axis - 1))
    else:
        allowed = common | {"pane_id", "sequence", "cell_width", "cell_height"}
        normalized["pane_id"] = integer(command.get("pane_id"),
                                        "pane_id", 1, 0xffffffff)
        integer(command.get("sequence"), "sequence", 0, 0xffffffff)
        integer(command.get("cell_width"), "cell_width", 1, 64)
        integer(command.get("cell_height"), "cell_height", 1, 64)
    if set(command) - allowed:
        raise ProtocolError("unknown command fields")
    return normalized


def no_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ProtocolError("duplicate JSON key")
        result[key] = value
    return result


def read_command(stream):
    line = stream.readline(MAX_COMMAND_BYTES + 1)
    if line == b"":
        return None
    if len(line) > MAX_COMMAND_BYTES or not line.endswith(b"\n"):
        raise ProtocolError("command exceeds byte bound or lacks newline")
    try:
        command = json.loads(line.decode("utf-8"), object_pairs_hook=no_duplicate_keys,
                             parse_constant=lambda value: (_ for _ in ()).throw(
                                 ProtocolError("nonfinite JSON value")))
    except (ValueError, UnicodeDecodeError, RecursionError) as error:
        raise ProtocolError("invalid JSON command") from error
    return validate_command(command)


def check_rgb(rgb, width, height):
    integer(width, "width", 1, MAX_WIDTH)
    integer(height, "height", 1, MAX_HEIGHT)
    if not isinstance(rgb, (bytes, bytearray)) or len(rgb) != width * height * 3:
        raise ValueError("RGB payload length mismatch")


def bgrx_to_rgb(data, width, height, stride):
    """Convert bounded little-endian BGRX rows using C-level slice copies."""
    integer(width, "width", 1, MAX_WIDTH)
    integer(height, "height", 1, MAX_HEIGHT)
    integer(stride, "stride", width * 4, width * 4 + 256)
    if not isinstance(data, bytes) or len(data) != stride * height:
        raise ValueError("BGRX payload length mismatch")
    result = bytearray(width * height * 3)
    for y in range(height):
        row = data[y * stride:y * stride + width * 4]
        start = y * width * 3
        end = start + width * 3
        result[start:end:3] = row[2::4]
        result[start + 1:end:3] = row[1::4]
        result[start + 2:end:3] = row[0::4]
    return bytes(result)


def encode_png_rgb(rgb, width, height):
    """Encode plain 8-bit RGB PNG with no optional or identifying metadata."""
    check_rgb(rgb, width, height)
    def chunk(kind, payload):
        return (struct.pack(">I", len(payload)) + kind + payload +
                struct.pack(">I", zlib.crc32(kind + payload) & 0xffffffff))
    row = width * 3
    raw = b"".join(b"\x00" + rgb[offset:offset + row]
                   for offset in range(0, len(rgb), row))
    return (b"\x89PNG\r\n\x1a\n" +
            chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)) +
            chunk(b"IDAT", zlib.compress(raw, 3)) + chunk(b"IEND", b""))


class EvidenceDirectory:
    """Pin the private output directory and never follow/overwrite a file."""
    def __init__(self, path):
        supplied = Path(path).absolute()
        self.fd = os.open(supplied, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            info = os.fstat(self.fd)
            if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700:
                raise ValueError("evidence directory must be owned by observer and mode 0700")
            self.path = supplied.resolve(strict=True)
            if os.stat(self.path).st_ino != info.st_ino or os.stat(self.path).st_dev != info.st_dev:
                raise ValueError("evidence directory changed during opening")
        except BaseException:
            os.close(self.fd)
            raise

    def save(self, command, rgb):
        name = command["name"]
        if NAME.fullmatch(name) is None:
            raise ProtocolError("unsafe evidence name")
        phase = "before" if command["kind"] == "marker_before" else "final"
        filename = f"{name}-{phase}.png"
        data = encode_png_rgb(rgb, command["rect"][2], command["rect"][3])
        fd = os.open(filename, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=self.fd)
        with os.fdopen(fd, "wb") as output:
            output.write(data)
        return filename, hashlib.sha256(data).hexdigest()

    def close(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None


@dataclass(frozen=True)
class Capture:
    rgb: bytes
    started_ns: int
    finished_ns: int


class XImage(ctypes.Structure):
    # The public prefix of Xlib's XImage; no ownership is transferred to Python.
    _fields_ = [("width", ctypes.c_int), ("height", ctypes.c_int),
                ("xoffset", ctypes.c_int), ("format", ctypes.c_int),
                ("data", ctypes.c_void_p), ("byte_order", ctypes.c_int),
                ("bitmap_unit", ctypes.c_int), ("bitmap_bit_order", ctypes.c_int),
                ("bitmap_pad", ctypes.c_int), ("depth", ctypes.c_int),
                ("bytes_per_line", ctypes.c_int), ("bits_per_pixel", ctypes.c_int),
                ("red_mask", ctypes.c_ulong), ("green_mask", ctypes.c_ulong),
                ("blue_mask", ctypes.c_ulong), ("obdata", ctypes.c_void_p)]


def configure_xlib(library):
    signatures = {
        "XOpenDisplay": ([ctypes.c_char_p], ctypes.c_void_p),
        "XCloseDisplay": ([ctypes.c_void_p], ctypes.c_int),
        "XDefaultScreen": ([ctypes.c_void_p], ctypes.c_int),
        "XDefaultRootWindow": ([ctypes.c_void_p], ctypes.c_ulong),
        "XDisplayWidth": ([ctypes.c_void_p, ctypes.c_int], ctypes.c_int),
        "XDisplayHeight": ([ctypes.c_void_p, ctypes.c_int], ctypes.c_int),
        "XDefaultDepth": ([ctypes.c_void_p, ctypes.c_int], ctypes.c_int),
        "XGetImage": ([ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                       ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_int],
                      ctypes.POINTER(XImage)),
        "XDestroyImage": ([ctypes.POINTER(XImage)], ctypes.c_int),
    }
    for name, (arguments, returns) in signatures.items():
        function = getattr(library, name)
        function.argtypes = arguments
        function.restype = returns


class Screen:
    def __init__(self, library, display):
        self.library, self.display = library, display
        screen = library.XDefaultScreen(display)
        self.width = library.XDisplayWidth(display, screen)
        self.height = library.XDisplayHeight(display, screen)
        if not (0 < self.width <= MAX_SCREEN_WIDTH and 0 < self.height <= MAX_SCREEN_HEIGHT):
            raise RuntimeError("isolated display exceeds 1600x1000 bounds")
        if library.XDefaultDepth(display, screen) != 24 or sys.byteorder != "little":
            raise RuntimeError("observer requires little-endian 24-depth display")
        self.root = library.XDefaultRootWindow(display)

    def capture(self, rect):
        x, y, width, height = rect
        if not (0 <= x and 0 <= y and 1 <= width <= MAX_WIDTH and 1 <= height <= MAX_HEIGHT
                and x + width <= self.width and y + height <= self.height):
            raise ProtocolError("ROI outside isolated display")
        started = time.monotonic_ns()
        pointer = self.library.XGetImage(self.display, self.root, x, y, width, height,
                                        ctypes.c_ulong(-1).value, 2)  # AllPlanes, ZPixmap
        finished = time.monotonic_ns()
        if not pointer:
            raise RuntimeError("XGetImage failed")
        try:
            image = pointer.contents
            if (image.width != width or image.height != height or image.xoffset != 0
                    or image.format != 2 or image.byte_order != 0 or image.depth != 24
                    or image.bits_per_pixel != 32 or image.bytes_per_line != width * 4
                    or (image.red_mask, image.green_mask, image.blue_mask) !=
                       (0xff0000, 0x00ff00, 0x0000ff) or not image.data):
                raise RuntimeError("unsupported XImage layout; require 24-depth little-endian BGRX")
            raw = ctypes.string_at(image.data, image.bytes_per_line * height)
            return Capture(bgrx_to_rgb(raw, width, height, image.bytes_per_line), started, finished)
        finally:
            self.library.XDestroyImage(pointer)


def detect_divider(rgb, width, height, *, horizontal, expected, radius):
    """Find a distinct RGB-gradient edge, independent of widget/probe state.

    Coordinates are local to the ROI. GTK horizontal=True means side-by-side
    panes, so the boundary varies along x; False means stacked panes and y. Contrast is the summed RGB absolute difference
    across the other axis. Adjacent peaks within two pixels are one edge.
    """
    check_rgb(rgb, width, height)
    if type(horizontal) is not bool:
        raise ProtocolError("horizontal must be a boolean")
    axis, cross = (width, height) if horizontal else (height, width)
    integer(expected, "expected", 1, axis - 1)
    integer(radius, "radius", 0, min(128, axis - 1))
    lo, hi = max(1, expected - radius), min(axis - 1, expected + radius)
    scores = {}
    for position in range(lo, hi + 1):
        total = 0
        if not horizontal:
            first, second = (position - 1) * width * 3, position * width * 3
            total = sum(abs(a - b) for a, b in zip(rgb[first:first + width * 3],
                                                   rgb[second:second + width * 3]))
        else:
            for y in range(height):
                offset = (y * width + position) * 3
                total += sum(abs(rgb[offset + c] - rgb[offset - 3 + c]) for c in range(3))
        scores[position] = total
    # Deterministic representative for a narrow strip: closest to expected,
    # then lowest coordinate. This must not resolve separated ambiguous peaks.
    position = max(scores, key=lambda p: (scores[p], -abs(p - expected), -p))
    strongest = scores[position]
    second = max((score for p, score in scores.items() if abs(p - position) > 2), default=0)
    result = dict(position=None, contrast=strongest, cross_axis_pixels=cross,
                  normalized_contrast=strongest / cross, competing_contrast=second)
    if strongest < 24 * cross:
        return dict(result, matched=False, reason="low-contrast")
    if second * 5 >= strongest * 4:
        return dict(result, matched=False, reason="ambiguous-peaks")
    return dict(result, position=position, matched=True, reason="distinct-edge")


def paired_strip_motion(before, after, width, height, *, horizontal, first, second,
                        expected, delta):
    """Conservative alternative when unrelated content leaves an RGB edge.

    Learn a unique, cross-axis-constant 1–2 pixel strip ONLY from the before
    capture. Two pixels is the existing edge-cluster radius, not an artifact
    fit. Require exact fill RGB and boundary-sign transport, opposite signed boundaries,
    and reject even one matching residual row. Unsupported styles fail closed.
    This observes a visual signature, not semantic widget identity in isolation.
    """
    axis, cross = (width, height) if horizontal else (height, width)
    lo = max(1, min(expected, expected + delta) - 5)
    hi = min(axis - 1, max(expected, expected + delta) + 5)

    def pixel(rgb, position, row):
        x, y = (position, row) if horizontal else (row, position)
        offset = (y * width + x) * 3
        return tuple(rgb[offset:offset + 3])

    def signature(rgb, start, size, row):
        return tuple(pixel(rgb, p, row) for p in range(start - 1, start + size + 1))

    def valid(sig):
        left, fill, right = sig[0], sig[1], sig[-1]
        if any(color != fill for color in sig[1:-1]): return False
        entering = tuple(b-a for a,b in zip(left,fill))
        leaving = tuple(b-a for a,b in zip(fill,right))
        return (sum(abs(v) for v in entering) >= 24 and
                sum(abs(v) for v in leaving) >= 24 and
                all(a*b <= 0 for a,b in zip(entering,leaving)) and
                any(a*b < 0 for a,b in zip(entering,leaving)))

    def shape(sig):
        # Background color is not part of the strip: preserve exact fill color
        # and both signed boundaries, with >=24 contrast independently per row.
        if not valid(sig): return None
        sign=lambda value: (value>0)-(value<0)
        return (sig[1],tuple(sign(b-a) for a,b in zip(sig[0],sig[1])),
                tuple(sign(b-a) for a,b in zip(sig[-2],sig[-1])))

    def complete(rgb):
        found=[]
        for start in range(lo, hi + 1):
            for size in (1,2):
                if start + size >= axis: continue
                sig=shape(signature(rgb,start,size,0))
                if sig is not None and all(shape(signature(rgb,start,size,row))==sig for row in range(1,cross)):
                    found.append((start,size,sig))
        return found

    originals=[item for item in complete(before)
               if min(abs(first["position"]-item[0]),abs(first["position"]-item[0]-item[1]))<=2]
    if len(originals)!=1: raise ValueError('before image has no unique complete narrow strip')
    old,size,sig=originals[0]
    if sum(item[1:]==(size,sig) for item in complete(before))!=1:
        raise ValueError("preinput signature is duplicated")
    targets=[item for item in complete(after) if item[1:]==(size,sig)]
    if len(targets)!=1 or targets[0][1:]!=(size,sig):
        raise ValueError('after image does not contain the same unique complete strip')
    new=targets[0][0]
    if min(abs(first['position']-old),abs(first['position']-old-size))>2 or min(abs(second['position']-new),abs(second['position']-new-size))>2:
        raise ValueError('strip does not correspond to dominant edge')
    if abs(old-expected)>5 or abs(new-expected-delta)>5 or abs(new-old-delta)>2:
        raise ValueError('strip is outside expected motion bounds')
    for rgb,site in ((after,old),(before,new)):
        for start in range(max(1,site-2),min(axis-size-1,site+2)+1):
            if any(shape(signature(rgb,start,size,row))==sig for row in range(cross)):
                raise ValueError('matching strip preexists or partially remains')
    return dict(method='unique-preinput-paired-strip-v1',before_start=old,
                after_start=new,width=size,fill_and_signed_boundaries=[list(c) for c in sig],
                complete_cross_axis_pixels=cross,partial_residual_rows=0)


def divider_motion(before, after, width, height, *, horizontal, expected, delta):
    """Match one moving edge over the SAME union, then prove disappearance/appearance."""
    integer(delta, "delta", -18, 12)
    if delta not in (-18, -12, 6, 12): raise ProtocolError("unsupported drag delta")
    center, radius = expected + delta // 2, abs(delta) // 2 + 5
    first = detect_divider(before, width, height, horizontal=horizontal, expected=center, radius=radius)
    second = detect_divider(after, width, height, horizontal=horizontal, expected=center, radius=radius)
    if not first['matched'] or not second['matched']:
        raise ValueError('divider union does not contain one unambiguous edge in each capture')
    if abs(first['position'] - expected) > 5 or abs(second['position'] - expected - delta) > 5:
        raise ValueError('divider edge is outside expected motion locations')
    shift = second['position'] - first['position']
    if abs(shift - delta) > 2: raise ValueError('divider edge did not follow drag')
    old_after = detect_divider(after, width, height, horizontal=horizontal, expected=first['position'], radius=2)
    new_before = detect_divider(before, width, height, horizontal=horizontal, expected=second['position'], radius=2)
    proof = dict(method='strict-low-contrast-disappearance')
    if old_after['reason'] != 'low-contrast' or new_before['reason'] != 'low-contrast':
        proof = paired_strip_motion(before, after, width, height, horizontal=horizontal,
                                    first=first, second=second, expected=expected, delta=delta)
    return dict(before=first, after=second, shift=shift, old_edge_disappeared=True, new_edge_appeared=True, disappearance_proof=proof)


def json_value(value):
    if is_dataclass(value):
        return json_value(asdict(value))
    if isinstance(value, dict):
        return {key: json_value(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [json_value(item) for item in value]
    return value


class Observer:
    def __init__(self, screen, evidence, decoder, *, clock=time.monotonic_ns,
                 cpu_clock=time.process_time_ns, sleep=time.sleep):
        self.screen, self.evidence, self.decoder = screen, evidence, decoder
        self.clock, self.cpu_clock, self.sleep = clock, cpu_clock, sleep

    def execute(self, command):
        command = validate_command(command)
        started, cpu_started = self.clock(), self.cpu_clock()
        deadline = started + WAIT_NS
        samples, last_absent = [], None
        while True:
            captured = self.screen.capture(command["rect"])
            width, height = command["rect"][2:]
            if command["kind"] == "divider":
                decoded = detect_divider(captured.rgb, width, height,
                                         horizontal=command["horizontal"],
                                         expected=command["expected"], radius=command["radius"])
            else:
                result = self.decoder(captured.rgb, width, height,
                                      cell_width=command["cell_width"], cell_height=command["cell_height"],
                                      expected_pane_id=command["pane_id"], expected_sequence=command["sequence"])
                decoded = dict(matched=result.matched, reason=result.reason, markers=json_value(result.markers))
            interval = dict(started_ns=captured.started_ns, finished_ns=captured.finished_ns,
                            matched=decoded["matched"], reason=decoded["reason"])
            samples.append(interval)
            if not decoded["matched"]:
                last_absent = dict(interval)
            now = self.clock()
            if (command["kind"] != "marker_wait" or decoded["matched"] or
                    len(samples) >= MAX_SAMPLES or now >= deadline):
                break
            # Pause after each decode; decoding overhead never causes a
            # catch-up burst. No capture starts at/after the deadline.
            delay = min(POLL_NS, deadline - now)
            self.sleep(delay / 1_000_000_000)
            if self.clock() >= deadline:
                break
        path, digest = self.evidence.save(command, captured.rgb)
        return dict(kind=command["kind"], name=command["name"], rect=command["rect"],
                    **decoded, samples=samples, last_absent=last_absent,
                    capture_count=len(samples), elapsed_ns=self.clock() - started,
                    cpu_ns=self.cpu_clock() - cpu_started,
                    final_png_path=path, final_png_sha256=digest)


def load_decoder():
    path = Path(__file__).with_name("interaction-pixel-codec.py")
    spec = importlib.util.spec_from_file_location("interaction_pixel_codec", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module.decode_rgb


def serve(observer, input_stream, output_stream):
    while True:
        command = read_command(input_stream)
        if command is None:
            return
        response = observer.execute(command)
        output_stream.write(json.dumps(response, separators=(",", ":"), allow_nan=False) + "\n")
        output_stream.flush()


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 1:
        print("usage: interaction-pixel-observer.py PRIVATE_EVIDENCE_DIRECTORY", file=sys.stderr)
        return 2
    if os.environ.get("ANVIL_BUDGET_ISOLATED_DISPLAY") != "1":
        print("pixel observer requires ANVIL_BUDGET_ISOLATED_DISPLAY=1", file=sys.stderr)
        return 2
    # Reject network display names: the collector must assign a private local X
    # server and retain its lifecycle. The env guard alone cannot prove ownership.
    display_name = os.environ.get("DISPLAY", "")
    if re.fullmatch(r":[0-9]{1,5}(?:\.0)?", display_name) is None:
        print("pixel observer requires an explicit local isolated DISPLAY", file=sys.stderr)
        return 2
    evidence, display, library = None, None, None
    try:
        evidence = EvidenceDirectory(argv[0])
        decoder = load_decoder()
        library = ctypes.CDLL("libX11.so.6")
        configure_xlib(library)
        display = library.XOpenDisplay(display_name.encode("ascii"))
        if not display:
            raise RuntimeError("cannot open isolated X11 display")
        observer = Observer(Screen(library, display), evidence, decoder)
        serve(observer, sys.stdin.buffer, sys.stdout)
        return 0
    except (OSError, ValueError, RuntimeError) as error:
        print(f"pixel observer failed closed: {error}", file=sys.stderr)
        return 2
    finally:
        if display:
            library.XCloseDisplay(display)
        if evidence is not None:
            evidence.close()


if __name__ == "__main__":
    raise SystemExit(main())
