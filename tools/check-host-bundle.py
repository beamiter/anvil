#!/usr/bin/env python3
"""Verify this frontend's frozen host bundle and its immutable public bases."""
import hashlib
import json
from pathlib import Path
import tomllib

root=Path(__file__).resolve().parents[1]
p=root/'vendor/host-adapter-bundle.json'
manifest=json.loads(p.read_text())
assert manifest['schema']==1 and manifest['bundle']=='withdrawal-resume-v1'
remote='https://github.com/beamiter/jcompanion.git'
allowed={'tools/check-host-bundle.py'}
cargo=tomllib.loads((root/'Cargo.toml').read_text())
if cargo['package']['name'] in ['forge','anvil']:
 allowed.update({'src/gtk_close.rs', 'src/gtk_presentation.rs', 'src/gtk_clock_fixture.rs'})
for name in ['companion-host','companion-protocol','companion-client']:
 for item in (root/'vendor'/name).rglob('*'):
  if item.is_file() and not any(part in ['target','__pycache__'] for part in item.relative_to(root).parts):
   allowed.add(str(item.relative_to(root)))
assert allowed==set(manifest['files_sha256']),('unexpected bundle files',allowed^set(manifest['files_sha256']))
for name,want in manifest['files_sha256'].items():
 path=root/name
 assert not Path(name).is_absolute() and '..' not in Path(name).parts and not path.is_symlink()
 assert path.is_file() and hashlib.sha256(path.read_bytes()).hexdigest()==want,name
assert cargo['dependencies']['companion-host']=={'version':'0.1.0','path':'vendor/companion-host'}
patch=cargo['patch'][remote]
assert patch=={name:{'path':'vendor/'+name} for name in ['companion-protocol','companion-client']}
for table in ['dependencies','dev-dependencies']:
 core=cargo.get(table,{}).get('jterm_core')
 if core:
  assert core.get('git')=='https://github.com/beamiter/jterm_core.git'
  assert core.get('rev')==manifest['public_core_base'] and 'path' not in core
for package in ['companion-host','companion-client','companion-protocol']:
 data=tomllib.loads((root/'vendor'/package/'Cargo.toml').read_text())
 assert data['package']['name']==package and data['package']['version']==manifest['version']
 for name,dependency in data['dependencies'].items():
  if name in ['companion-protocol','companion-client','companion-core','eva-character-atlas']:
   assert dependency.get('git')==remote and dependency.get('rev')==manifest['public_companion_base'] and 'path' not in dependency
print(json.dumps({'passed':True,'files':len(allowed),'bundle':manifest['bundle'],'public_companion_base':manifest['public_companion_base'],'public_core_base':manifest['public_core_base'],'bundle_manifest_sha256':hashlib.sha256(p.read_bytes()).hexdigest()}))
