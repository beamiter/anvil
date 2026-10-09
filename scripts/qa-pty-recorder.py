#!/usr/bin/env python3
"""Record bytes from a test PTY without interpreting or executing them."""
import os
import pathlib
import select
import sys
import time
import tty

capture, ready, stop = map(pathlib.Path, sys.argv[1:4])
tty.setraw(0)
with capture.open('wb', buffering=0) as output:
    ready.write_text('raw nonexecuting PTY ready')
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline and not stop.exists():
        if select.select([0], [], [], .05)[0]:
            data = os.read(0, 4096)
            if not data:
                break
            output.write(data)
