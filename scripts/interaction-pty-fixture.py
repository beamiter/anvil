#!/usr/bin/env python3
"""Nonexecuting raw PTY child. SIGUSR1=start/reset, SIGUSR2=drain/idle.

Only the benchmark runner launches this program on its owned PTYs. No shell,
OSC133, clipboard, authentication material, or network access is used.
"""
import argparse
import json
import importlib.util
import sys
import os
from pathlib import Path
import select
import signal
import time
import tty

RATE = 256 * 1024
CHUNK = 4096


def block(sequence, pixel_marker=None):
    if pixel_marker is not None:
        frame = (f"\r\nA04K{sequence:06d}\r\n" + pixel_marker + "\r\n").encode("ascii")
        if len(frame) > CHUNK: raise ValueError("barcode frame exceeds chunk")
        data = frame * (CHUNK // len(frame))
        return b" " * (CHUNK - len(data)) + data
    line = (f"A04K{sequence:06d} " + "." * 52 + "\r\n").encode("ascii")
    return (line * ((CHUNK + len(line) - 1) // len(line)))[:CHUNK]


def scheduled_bytes(start_ns, now_ns):
    return max(0, (now_ns - start_ns) * RATE // 1_000_000_000)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reports", type=Path, required=True)
    args = parser.parse_args()
    info = args.reports.lstat()
    if not args.reports.is_absolute() or not args.reports.is_dir() or info.st_uid != os.geteuid() or info.st_mode & 0o077:
        raise SystemExit("fixture reports require an owned absolute private directory")
    if not os.isatty(0) or not os.isatty(1):
        raise SystemExit("fixture requires its owned PTY")
    tty.setraw(0)
    os.set_blocking(0, False)
    os.set_blocking(1, False)
    read_signal, write_signal = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
    signal.set_wakeup_fd(write_signal)
    for sig in (signal.SIGUSR1, signal.SIGUSR2, signal.SIGTERM):
        signal.signal(sig, lambda *_: None)
    running = False
    stopping = False
    pending = b""
    sequence = emitted = inputs = unexpected = 0
    start = end = blocked_since = blocked_ns = max_lag_ns = 0

    def report():
        data = dict(pid=os.getpid(), started_ns=start, stopped_ns=end,
                    scheduled_bytes=scheduled_bytes(start, end) if start else 0,
                    emitted_bytes=emitted, input_count=inputs, unexpected_input_bytes=unexpected,
                    blocked_write_ms=blocked_ns / 1e6, max_write_lag_ms=max_lag_ns / 1e6,
                    pending_bytes=len(pending), marker_sequence=sequence)
        temp = args.reports / f"{os.getpid()}.tmp"
        fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as output:
            json.dump(data, output)
        os.replace(temp, args.reports / f"{os.getpid()}.json")

    pixel_codec = None
    if os.environ.get("ANVIL_INTERACTION_PIXEL_MARKERS") == "1":
        spec = importlib.util.spec_from_file_location("pixel_codec", Path(__file__).with_name("interaction-pixel-codec.py"))
        pixel_codec = importlib.util.module_from_spec(spec); sys.modules[spec.name] = pixel_codec; spec.loader.exec_module(pixel_codec)
        pixel_codec.encode_marker(os.getpid(), 0)  # Validate identity range before readiness.
    report()
    try:
        while True:
            now = time.monotonic_ns()
            if running and not pending and scheduled_bytes(start, now) >= emitted + CHUNK:
                pending = block(sequence, pixel_codec.encode_marker(os.getpid(), sequence) if pixel_codec else None)
                blocked_since = time.monotonic_ns()
            deadline = start + (emitted + CHUNK) * 1_000_000_000 // RATE
            timeout = max(0, (deadline - now) / 1e9) if running and not pending else None
            ready, writable, _ = select.select([0, read_signal], [1] if pending else [], [], timeout)
            if read_signal in ready:
                for sig in os.read(read_signal, 4096):
                    if sig == signal.SIGTERM:
                        end = time.monotonic_ns(); report(); return
                    if sig == signal.SIGUSR1:
                        if running or pending:
                            raise RuntimeError("start requires fully drained idle fixture")
                        running = True; stopping = False
                        sequence = emitted = inputs = unexpected = 0
                        blocked_since = blocked_ns = max_lag_ns = 0
                        start = time.monotonic_ns(); end = 0
                    if sig == signal.SIGUSR2:
                        running = False; stopping = True; end = time.monotonic_ns()
            if 0 in ready:
                try: data = os.read(0, 4096)
                except BlockingIOError: data = None
                if data == b"":
                    end = time.monotonic_ns(); report(); return
                if data is None: data = b""
                for byte in data:
                    if byte == ord("a"):
                        inputs += 1; sequence += 1
                        if sequence > 999999: raise RuntimeError("fixture event bound exceeded")
                    else: unexpected += 1
            if pending and 1 in writable:
                try:
                    written = os.write(1, pending)
                except BlockingIOError:
                    written = 0
                if written:
                    emitted += written; pending = pending[written:]
                    stamp = time.monotonic_ns()
                    max_lag_ns = max(max_lag_ns, stamp - deadline)
                    if blocked_since:
                        blocked_ns += stamp - blocked_since; blocked_since = 0
                elif not blocked_since:
                    blocked_since = time.monotonic_ns()
            elif pending and not blocked_since:
                blocked_since = time.monotonic_ns()
            if stopping and not pending:
                stopping = False; report()
    finally:
        signal.set_wakeup_fd(-1)
        os.close(read_signal); os.close(write_signal)


if __name__ == "__main__":
    main()
