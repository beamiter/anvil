#!/usr/bin/env python3
"""Verify or explicitly refresh byte-exact production extraction; no native I/O."""
import hashlib
import json
import sys
from pathlib import Path
ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[1]


def outputs(repo=REPO):
    source = repo / 'src/terminal'
    text = (source/'url.rs').read_text()
    start = text.index('fn launch_failure(')
    end = text.index('/// One recovery prompt', start)
    boundary = text[start:end]
    if boundary.count('gio::AppInfo::launch_default_for_uri_async(') != 1:
        raise RuntimeError('production async boundary changed')
    recovery = (source/'url_recovery.rs').read_bytes()
    return {'src/extracted.rs': boundary.encode(), 'src/url_recovery.rs': recovery,
            'src/recovery_body.rs': recovery.split(b'\n', 1)[1]}


def identities(root=ROOT, repo=REPO):
    paths = ['src/terminal/url.rs', 'src/terminal/url_recovery.rs']
    paths += [str((root/name).relative_to(repo)) for name in outputs(repo)]
    return {name: hashlib.sha256((repo/name).read_bytes()).hexdigest() for name in paths}


def verify(root=ROOT, repo=REPO):
    for name, content in outputs(repo).items():
        if (root/name).read_bytes() != content:
            raise RuntimeError('stale production extraction')
    if identities(root, repo) != json.loads((root/'extraction.json').read_text()):
        raise RuntimeError('production extraction identity mismatch')


if __name__ == '__main__':
    if sys.argv[1:] == ['--write']:
        for name, content in outputs().items():
            (ROOT/name).write_bytes(content)
        (ROOT/'extraction.json').write_text(json.dumps(identities(), indent=2)+'\n')
    elif sys.argv[1:] == ['--check']:
        verify()
    else:
        raise SystemExit('Use --check or explicitly --write')
    print('Production extraction verified; no native execution.')
