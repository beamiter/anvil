#!/usr/bin/env python3
"""Check a frontend's exact process package and unchanged public core pin."""
import hashlib
import json
from pathlib import Path
import tomllib

root = Path(__file__).resolve().parents[1]
path = root / 'vendor/process-adapter-bundle.json'
manifest = json.loads(path.read_text())
assert manifest['schema'] == 1 and manifest['bundle'] == 'pty-process-v1'
allowed = {'tools/check-process-bundle.py', 'src/gtk_process.rs', 'src/gtk_spawn.rs'}
for file in (root / 'vendor/companion-process').rglob('*'):
    if file.is_file() and not any(p in ['target', '__pycache__'] for p in file.relative_to(root).parts):
        allowed.add(str(file.relative_to(root)))
assert allowed == set(manifest['files_sha256']), allowed ^ set(manifest['files_sha256'])
for name, want in manifest['files_sha256'].items():
    file = root / name
    assert not Path(name).is_absolute() and '..' not in Path(name).parts and not file.is_symlink()
    assert hashlib.sha256(file.read_bytes()).hexdigest() == want, name
cargo = tomllib.loads((root / 'Cargo.toml').read_text())
assert cargo['dependencies']['companion-process'] == {'version': '0.1.0', 'path': 'vendor/companion-process'}
for table in ['dependencies', 'dev-dependencies']:
    core = cargo.get(table, {}).get('jterm_core')
    if core:
        assert core.get('git') == 'https://github.com/beamiter/jterm_core.git'
        assert core.get('rev') == manifest['public_core_base'] and 'path' not in core
print(json.dumps(dict(passed=True, files=len(allowed), bundle=manifest['bundle'],
                      bundle_manifest_sha256=hashlib.sha256(path.read_bytes()).hexdigest())))
