#!/usr/bin/env python3
"""Record exact tracked fixture/production identities before hosted execution."""
import hashlib
import json
import sys
import tomllib
from pathlib import Path
from extract import REPO, ROOT, verify


def record():
    verify()
    production = tomllib.loads((REPO/'Cargo.toml').read_text())
    fixture = tomllib.loads((ROOT/'Cargo.toml').read_text())
    for key in ['git', 'rev']:
        if production['dependencies']['jterm_core'][key] != fixture['dependencies']['jterm_core'][key]:
            raise RuntimeError('fixture core pin differs from production')
    prod_lock = tomllib.loads((REPO/'Cargo.lock').read_text())['package']
    fixture_lock = tomllib.loads((ROOT/'Cargo.lock').read_text())['package']
    for name in ['gio', 'glib', 'jterm_core']:
        select = lambda packages: [(p['version'], p.get('source')) for p in packages if p['name'] == name]
        if select(prod_lock) != select(fixture_lock):
            raise RuntimeError('fixture native binding/core identity differs from production')
    paths = [REPO/'Cargo.toml', REPO/'Cargo.lock', REPO/'src/terminal/url.rs',
             REPO/'src/terminal/url_recovery.rs', REPO/'.github/workflows/link-recovery.yml']
    paths += [p for p in ROOT.rglob('*') if p.is_file() and 'target' not in p.parts
              and '__pycache__' not in p.parts]
    return {'files': {str(p.relative_to(REPO)): hashlib.sha256(p.read_bytes()).hexdigest()
                      for p in sorted(paths)},
            'native_boundary_only': True, 'gtk_ui_acceptance': False,
            'isolation': 'fresh user and network namespaces, private non-activating DBus, private XDG roots'}


if __name__ == '__main__':
    Path(sys.argv[1]).write_text(json.dumps(record(), indent=2)+'\n')
