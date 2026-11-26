#!/usr/bin/env python3
"""Enforce the production Cargo and optional linked-ELF no-libvte boundary."""
import json
from pathlib import Path
import re
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
def forbidden(name):
    return name in {'vte4', 'vte4-sys', 'vte', 'vte-sys'}
for name in re.findall(r'^name = "([^"]+)"$', (root / 'Cargo.lock').read_text(), re.M):
    if forbidden(name):
        raise SystemExit(f'production Cargo.lock contains {name}')
metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--offline', '--format-version=1'], cwd=root))
for package in metadata['packages']:
    if forbidden(package['name']):
        raise SystemExit(f"production Cargo graph contains {package['name']}")
if len(sys.argv) > 1:
    binary = Path(sys.argv[1]).resolve()
    dynamic = subprocess.check_output(['readelf', '--dynamic', str(binary)], text=True)
    imports = subprocess.check_output(['nm', '--dynamic', '--undefined-only', str(binary)], text=True)
    if re.search(r'libvte|\bvte_', dynamic + imports, re.I):
        raise SystemExit('production ELF still links or imports libvte')
print('Production manifest, lock and resolved graph contain no VTE' + ('; ELF has no VTE linkage/imports' if len(sys.argv)>1 else ''))
