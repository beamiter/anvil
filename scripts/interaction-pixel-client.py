#!/usr/bin/env python3
"""Private process-pipe client and bounded geometry for the short pixel gate."""
import json
import math
import os
from pathlib import Path
import select
import subprocess
import sys
import time


def finite(value):
    if type(value) not in (int, float) or not math.isfinite(value):
        raise ValueError('geometry must be finite numeric values')
    return value


def coordinates(values, count):
    if not isinstance(values, (list, tuple)) or len(values) != count:
        raise ValueError('invalid geometry dimensions')
    return [finite(value) for value in values]


def active_tab(snapshot):
    index = snapshot['active_tab_index']; tabs = snapshot['tabs']
    if type(index) is not int or not 0 <= index < len(tabs):
        raise ValueError('invalid active tab index')
    return tabs[index]


def marker_rect(pane, origin):
    geometry = pane['pixel_geometry']; cell_width = geometry['cell_width']; cell_height = geometry['cell_height']
    if type(cell_width) is not int or type(cell_height) is not int or not (1 <= cell_width <= 20 and 1 <= cell_height <= 64):
        raise ValueError('unsupported fixed-font cell geometry')
    origin = coordinates(origin, 2)
    a = coordinates(pane['bounds'], 4); b = coordinates(geometry['bounds'], 4)
    if min(a[2:] + b[2:]) <= 0: raise ValueError('nonpositive geometry')
    left = max(a[0], b[0]); top = max(a[1], b[1]); right = min(a[0] + a[2], b[0] + b[2]); bottom = min(a[1] + a[3], b[1] + b[3])
    left = math.ceil(origin[0] + left); right = math.floor(origin[0] + right)
    top = math.ceil(origin[1] + top); bottom = math.floor(origin[1] + bottom)
    width = min(right - left, 33 * cell_width)
    height = min(bottom - top, 4 * cell_height)
    if width < 31 * cell_width or height < cell_height * 2:
        raise ValueError('visible pane cannot retain complete pixel barcode')
    rect = [left, bottom - height, width, height]
    if rect[0] < 0 or rect[1] < 0 or rect[0] + width > 1600 or rect[1] + height > 1000:
        raise ValueError('marker ROI outside isolated screen')
    return rect, cell_width, cell_height


def divider_rect(divider, origin):
    horizontal = divider['horizontal']
    if type(horizontal) is not bool: raise ValueError('invalid divider orientation')
    origin = coordinates(origin, 2)
    finite(divider['x']); finite(divider['y'])
    width, height = (64, 48) if horizontal else (48, 64)
    rect = [round(origin[0] + divider['x']) - width // 2,
            round(origin[1] + divider['y']) - height // 2, width, height]
    if rect[0] < 0 or rect[1] < 0 or rect[0] + width > 1600 or rect[1] + height > 1000:
        raise ValueError('divider ROI outside isolated screen')
    return rect, 32


def allocation_shift(before, after, delta):
    finite(delta)
    if delta == 0: raise ValueError('zero drag delta')
    a = active_tab(before); b = active_tab(after)
    if a['tab_id'] != b['tab_id'] or [p['pid'] for p in a['panes']] != [p['pid'] for p in b['panes']]:
        raise ValueError('drag changed active tab or pane identity')
    x, y = before['dividers'][0], after['dividers'][0]
    if type(x['horizontal']) is not bool or type(y['horizontal']) is not bool or x['horizontal'] != y['horizontal']:
        raise ValueError('drag changed divider orientation')
    shift = finite(y['position']) - finite(x['position'])
    for pane in a['panes'] + b['panes']: coordinates(pane['bounds'], 4)
    if shift * delta <= 0 or abs(shift - delta) > 4:
        raise ValueError('real divider allocation did not follow drag')
    axis = (0, 2) if x['horizontal'] else (1, 3)
    other = (1, 3) if x['horizontal'] else (0, 2)
    if any(any(abs(p['bounds'][i] - q['bounds'][i]) > 1 for i in other) for p, q in zip(a['panes'], b['panes'])):
        raise ValueError('drag changed unrelated allocation axis')
    if all(all(p['bounds'][i] == q['bounds'][i] for i in axis) for p, q in zip(a['panes'], b['panes'])):
        raise ValueError('divider position changed without pane allocation')
    return shift


class PixelClient:
    def __init__(self, directory, env, check_cancelled):
        directory.mkdir(mode=0o700)
        self.check_cancelled = check_cancelled
        self.buffer = b''
        self.process = subprocess.Popen([sys.executable, '-I', str(Path(__file__).with_name('interaction-pixel-observer.py')),
                                         str(directory)], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        os.set_blocking(self.process.stdout.fileno(), False)

    def request(self, **command):
        encoded = (json.dumps(command) + '\n').encode()
        if len(encoded) > 4096: raise ValueError('oversized pixel observer command')
        self.process.stdin.write(encoded); self.process.stdin.flush()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            self.check_cancelled()
            if b'\n' in self.buffer:
                line, self.buffer = self.buffer.split(b'\n', 1)
                value = json.loads(line)
                if value.get('kind') != command['kind'] or value.get('name') != command['name']:
                    raise ValueError('pixel observer response identity mismatch')
                return value
            if self.process.poll() is not None: raise RuntimeError('owned pixel observer exited')
            if select.select([self.process.stdout], [], [], .025)[0]:
                self.buffer += os.read(self.process.stdout.fileno(), 32768)
                if len(self.buffer) > 65536: raise ValueError('pixel observer response exceeded bound')
        raise TimeoutError('owned pixel observer response deadline exceeded')

    def close_pipes(self):
        # OwnedProcesses, not this pipe helper, is the signal/reaping authority.
        for stream in (self.process.stdin, self.process.stdout):
            try: stream.close()
            except (BrokenPipeError, OSError): pass
