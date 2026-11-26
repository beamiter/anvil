#!/usr/bin/env python3
"""Run the historical VTE comparison fixture outside production Cargo.

The source overlay has its own manifest and frozen lock; neither production
file is modified. A stable path reuses compilation for per-test GTK processes.
"""
import fcntl
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
target = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')).resolve()
overlay = target / 'anvil-vte-oracle' / 'source'
overlay.mkdir(parents=True, exist_ok=True)
with (overlay.parent / 'build.lock').open('w') as guard:
    fcntl.flock(guard, fcntl.LOCK_EX)
    for entry in ROOT.iterdir():
        if entry.name not in {'Cargo.toml', 'Cargo.lock', 'target', '.git'}:
            link = overlay / entry.name
            if not link.is_symlink():
                link.symlink_to(entry, target_is_directory=entry.is_dir())
    manifest = (ROOT / 'Cargo.toml').read_text()
    manifest = manifest.replace('[dependencies]\n', '[dependencies]\nvte4 = { version = "=0.10.0", features = ["v0_76"] }\n', 1)
    for name, content in [('Cargo.toml', manifest.encode()), ('Cargo.lock', (ROOT / 'tools/vte-oracle/Cargo.lock').read_bytes())]:
        path = overlay / name
        if not path.exists() or path.read_bytes() != content:
            path.write_bytes(content)
    environment = dict(os.environ, JTERM_ANVIL_VTE_ORACLE='1', CARGO_TARGET_DIR=str(target))
    arguments = [argument for argument in sys.argv[1:] if argument != '--locked'] or ['test', '--all-targets']
    result = subprocess.run(['cargo', arguments[0], '--manifest-path', str(overlay / 'Cargo.toml'), '--locked', *arguments[1:]], env=environment)
    sys.exit(result.returncode)
