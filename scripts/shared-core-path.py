#!/usr/bin/env python3
"""Resolve the exact production jterm_core source, never an unrelated sibling/archive."""
import json
from pathlib import Path
import subprocess
import sys
import tomllib

root = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
dep = tomllib.loads((root / 'Cargo.toml').read_text())['dependencies']['jterm_core']
if 'path' in dep:
    print((root / dep['path']).resolve(strict=True))
else:
    revision = dep.get('rev', '')
    if len(revision) != 40 or not dep.get('git'):
        raise SystemExit('jterm_core must have an exact Git revision')
    host = next(line[6:] for line in subprocess.check_output(['rustc', '-vV'], text=True).splitlines() if line.startswith('host: '))
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1', '--filter-platform', host], cwd=root))
    expected = f"git+{dep['git']}?rev={revision}#{revision}"
    matches = [p for p in metadata['packages'] if p['name'] == 'jterm_core' and p.get('source') == expected]
    if len(matches) != 1:
        raise SystemExit('resolved jterm_core source differs from the production exact pin')
    print(Path(matches[0]['manifest_path']).parent)
