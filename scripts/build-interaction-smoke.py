#!/usr/bin/env python3
"""Build each observed revision in a distinct new target with rustc provenance."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def record_source(source, observed):
    source = source.resolve(strict=True); observed = observed.resolve(strict=True)
    git = lambda *args: subprocess.check_output(['git', '-C', str(source), *args], text=True).strip()
    entries = []
    for line in git('ls-tree', '-r', '--full-tree', 'HEAD').splitlines():
        metadata, path = line.split('\t', 1)
        mode, kind, blob = metadata.split()
        if kind != 'blob' or mode not in ('100644', '100755'):
            raise ValueError('unsupported source tree entry')
        data = (source / path).read_bytes()
        if hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest() != blob:
            raise ValueError('checkout differs from immutable source tree')
        if ('100755' if (source / path).stat().st_mode & 0o111 else '100644') != mode:
            raise ValueError('checkout executable mode differs from source tree')
        entries.append(dict(path=path, mode=mode, blob=blob))
    tracked = {e['path'] for e in entries}
    expected = tracked | {'scripts/interaction_budget_probe.rs', 'interaction-overlay.json'}
    actual = {str(p.relative_to(observed)) for p in observed.rglob('*') if p.is_file()}
    if actual != expected:
        raise ValueError('prepared tree has missing or untracked build inputs')
    files = {path: digest(observed / path) for path in sorted(expected)}
    delta = {path: value for path, value in files.items()
             if path not in tracked or value != digest(source / path)}
    allowed = {'src/main.rs', 'src/app_msg.rs', 'src/pty.rs', 'src/block_view/mod.rs',
               'src/terminal/vte.rs', 'scripts/interaction_budget_probe.rs', 'interaction-overlay.json'}
    if not set(delta) <= allowed:
        raise ValueError('unexpected instrumentation delta')
    record = dict(source_commit=git('rev-parse', 'HEAD'), source_tree=git('rev-parse', 'HEAD^{tree}'),
                  tracked_entries=entries, overlay_delta=delta, prepared_files=files)
    (observed / 'interaction-source.json').write_text(json.dumps(record, indent=2) + '\n')


def rustc_wrapper():
    compiler, *args = sys.argv[2:]
    is_app = '--crate-name' in args and args[args.index('--crate-name') + 1] == 'anvil'
    record = None
    if is_app:
        root = Path(os.environ['A04_EXPECTED_SOURCE']).resolve(strict=True)
        cwd = Path.cwd().resolve()
        source = next((Path(a).resolve() for a in args if a.endswith('.rs')), None)
        if cwd != root or source != root / 'src/main.rs':
            raise SystemExit('refusing wrong-source anvil compiler invocation')
        record = dict(cwd=str(cwd), source=str(source), source_sha256=digest(source),
                      compiler=str(Path(compiler).resolve()), started_ns=time.time_ns(),
                      arguments_sha256=hashlib.sha256(json.dumps(args).encode()).hexdigest())
    result = subprocess.run([compiler, *args])
    if record is not None:
        record.update(ended_ns=time.time_ns(), returncode=result.returncode)
        with open(os.environ['A04_RUSTC_RECORD'], 'a') as output:
            output.write(json.dumps(record) + '\n')
    raise SystemExit(result.returncode)


def validate_artifact(messages, root, target):
    app = [m for m in messages if m.get('reason') == 'compiler-artifact'
           and m.get('target', {}).get('name') == 'anvil' and m.get('executable')]
    if len(app) != 1 or app[0].get('fresh') is not False:
        raise ValueError('require exactly one freshly compiled anvil artifact')
    artifact = app[0]
    if Path(artifact['manifest_path']).resolve() != root / 'Cargo.toml':
        raise ValueError('artifact manifest differs from expected source')
    executable = Path(artifact['executable']).resolve(strict=True)
    if executable != target / 'release/anvil':
        raise ValueError('artifact executable differs from independent target')
    return executable


def build(root, target, binary, evidence, role):
    root = root.resolve(strict=True); target = target.resolve()
    if target.exists():
        raise ValueError('target must be absent: compiled artifacts cannot be shared or reused')
    evidence.mkdir(parents=True, exist_ok=True)
    wrapper = evidence / (role + '-rustc-wrapper')
    wrapper.write_text('#!/bin/sh\nexec ' + __import__('shlex').join(
        [sys.executable, '-I', str(Path(__file__).resolve()), '--rustc-wrapper']) + ' "$@"\n')
    wrapper.chmod(0o700)
    compiler_log = evidence / (role + '-rustc.jsonl')
    forbidden = ('RUSTC', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
                 'CARGO_BUILD_RUSTC', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER',
                 'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTFLAGS')
    if any(os.environ.get(key) for key in forbidden) or any(
            key.startswith('CARGO_TARGET_') and key.endswith('_RUSTFLAGS') and value
            for key, value in os.environ.items()):
        raise ValueError('compiler/flag override environment is not supported')
    clean_env = {key: value for key, value in os.environ.items() if not key.startswith('PYTHON')}
    env = dict(clean_env, CARGO_TARGET_DIR=str(target), CARGO_INCREMENTAL='0', CARGO_BUILD_JOBS='1',
               RUSTC_WRAPPER=str(wrapper), A04_EXPECTED_SOURCE=str(root), A04_RUSTC_RECORD=str(compiler_log))
    provenance = json.loads((root / 'interaction-source.json').read_text())
    files = provenance['prepared_files']
    actual = {str(p.relative_to(root)) for p in root.rglob('*') if p.is_file()}
    if actual != set(files) | {'interaction-source.json'} or any(digest(root / p) != h for p, h in files.items()):
        raise ValueError('prepared source no longer matches tracked tree plus overlay')
    record = dict(role=role, source_root=str(root), manifest_path=str(root / 'Cargo.toml'),
                  target_dir=str(target), overlay=json.loads((root / 'interaction-overlay.json').read_text()),
                  source_provenance=provenance, source_files=files, started_ns=time.time_ns(),
                  compiler_overrides='rejected', python_overrides='cleared; isolated wrapper')
    destination = evidence / (role + '-build.json')
    try:
        command = ['cargo', 'build', '--manifest-path', str(root / 'Cargo.toml'), '--release', '--locked',
                   '--message-format=json-render-diagnostics']
        record['command'] = command
        with (evidence / (role + '-cargo.jsonl')).open('w') as output, (evidence / (role + '-build.log')).open('w') as errors:
            subprocess.run(command, env=env, stdout=output, stderr=errors, check=True)
        messages = [json.loads(line) for line in (evidence / (role + '-cargo.jsonl')).read_text().splitlines()]
        executable = validate_artifact(messages, root, target)
        invocations = [json.loads(line) for line in compiler_log.read_text().splitlines()]
        if len(invocations) != 1 or invocations[0]['returncode'] != 0:
            raise ValueError('missing unique successful application rustc invocation')
        if invocations[0]['source_sha256'] != record['source_files']['src/main.rs']:
            raise ValueError('compiled source digest differs from build record')
        if any(digest(root / path) != value for path, value in record['source_files'].items()):
            raise ValueError('source changed during compilation')
        if executable.stat().st_mtime_ns < record['started_ns']:
            raise ValueError('artifact predates fresh build')
        binary.parent.mkdir(parents=True, exist_ok=True)
        __import__('shutil').copy2(executable, binary)
        record.update(binary_sha256=digest(binary), binary_size=binary.stat().st_size,
                      binary_mtime_ns=binary.stat().st_mtime_ns, compiler_invocations=invocations, verified=True)
    finally:
        record['ended_ns'] = time.time_ns()
        destination.write_text(json.dumps(record, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('source', 'target', 'binary', 'evidence'): parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--role', choices=('baseline', 'candidate'), required=True)
    args = parser.parse_args()
    build(args.source, args.target, args.binary, args.evidence.resolve(), args.role)


if __name__ == '__main__':
    if len(sys.argv) == 4 and sys.argv[1] == '--record-source': record_source(Path(sys.argv[2]), Path(sys.argv[3]))
    elif len(sys.argv) > 1 and sys.argv[1] == '--rustc-wrapper': rustc_wrapper()
    else: main()
