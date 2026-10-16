"""Bounded pipe-only startup and cleanup helpers; importing opens no bus/socket."""
import os
import re
import select
import subprocess
import time


def read_bus_address(process, expected_path, timeout=5.0):
    """Read one bounded ASCII address line, never a blocking buffered readline."""
    deadline = time.monotonic() + timeout
    fd = process.stdout.fileno()
    os.set_blocking(fd, False)
    pending = bytearray()
    while True:
        if process.poll() is not None:
            raise RuntimeError('private bus exited before ready')
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('private bus readiness timeout')
        ready, _, _ = select.select([fd], [], [], min(remaining, 0.05))
        if not ready:
            continue
        try:
            chunk = os.read(fd, 4096)
        except BlockingIOError:
            continue
        if not chunk:
            raise RuntimeError('private bus closed its address pipe')
        pending.extend(chunk)
        if len(pending) > 4096:
            raise RuntimeError('private bus address exceeds limit')
        if b'\n' not in pending:
            continue
        line, extra = bytes(pending).split(b'\n', 1)
        if extra:
            raise RuntimeError('private bus published extra address data')
        try:
            address = line.decode('ascii')
        except UnicodeDecodeError:
            raise RuntimeError('private bus published a non-ASCII address') from None
        pattern = re.escape('unix:path=' + str(expected_path)) + r'(?:,guid=[0-9a-f]{32})?'
        if re.fullmatch(pattern, address) is None:
            raise RuntimeError('unexpected private bus address')
        if process.poll() is not None:
            raise RuntimeError('private bus exited during startup')
        return address


def stop_children(children, timeout=3.0):
    """Attempt every owned child, with bounded terminate/kill/reap waits."""
    failed = False
    for child in children:
        if child is None:
            continue
        try:
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=timeout)
        except (OSError, subprocess.TimeoutExpired):
            failed = True
        finally:
            if child.stdout is not None:
                try:
                    child.stdout.close()
                except OSError:
                    failed = True
    if failed:
        raise RuntimeError('owned child cleanup failed')
