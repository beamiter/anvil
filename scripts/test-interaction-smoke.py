#!/usr/bin/env python3
"""Pure smoke-gate negatives. No display, process launch or sockets."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('smoke', Path(__file__).with_name('run-interaction-smoke.py'))
smoke = importlib.util.module_from_spec(spec); spec.loader.exec_module(smoke)


def valid(panes):
    tabs = [{'panes': [{'pid': 100 + n, 'block': True, 'mapped': n < 16}
                       for n in range(start, stop)]}
            for start, stop in ([(0, 16), (16, 32)] if panes == 32 else [(0, panes)])]
    for i, tab in enumerate(tabs): tab['tab_id'] = 10 + i
    keys = [{'pid': 116 if panes == 32 and n == 1 else 100, 'sequence': ([1, 1, 2][n] if panes == 32 else n + 1)} for n in range(3)]
    snapshots = []
    for n, key in enumerate(keys):
        target = 1 if panes == 32 and n == 1 else 0
        observed = copy.deepcopy(tabs)
        for i, tab in enumerate(observed):
            tab['tab_id'] = 10 + i
            for pane in tab['panes']: pane['mapped'] = i == target
        snapshots.append(dict(request=n, monotonic_ns=n + 10, tabs=observed, active_tab_index=target))
        key.update(id=n, scheduled_ns=n + 10, scheduled_tab_id=10 + target, active_tab_id=10 + target,
                   target_snapshot_request=n, target_snapshot_ns=n + 10, injected_ns=n + 11)
    return dict(workload=dict(tab_ids=[t['tab_id'] for t in tabs], kind='short_smoke', tab_switch_seconds=.5, key_count=3, active_seconds=2),
                snapshots=snapshots, schema='raw-native-block-v1', smoke=True, budget_acceptance=False, panes=panes,
                binary_sha256='a' * 64, raw_capture_completed=True, evidence_write_succeeded=True,
                cancellation_count=0, cleanup=dict(verified_empty=True, remaining_pids=[], errors=[],
                owned_identities=[{'pid': 100 + n} for n in range(panes)]),
                setup_snapshot=dict(tabs=tabs, fixture_program_matches=True), fixtures=[{'pid': 100 + n} for n in range(panes)],
                keys=keys, drags=[] if panes == 1 else [{}, {}, {}],
                raw_probe=dict(vte_spawn_count=0, timer_fallback=False, overflow=False,
                dispatch_ids=[0, 1], dispatch_ms=[.01, .02], consumer_dispatch_ids=[1, 1],
                consumer_ms=[.003, .004], consumed_bytes=100,
                painted_markers=[(k['pid'], k['sequence'], k['injected_ns'] + 1) for k in keys]))


class SmokeGate(unittest.TestCase):
    def test_supported_topologies_remain_proxy_only(self):
        for count in (1, 8, 32):
            result = smoke.validate(valid(count), count, 'a' * 64)
            self.assertFalse(result['budget_acceptance'])
            self.assertEqual(len(result['missing_gates']), 4)

    def test_rejects_unverified_cleanup_and_cancellation(self):
        for change in ('cleanup', 'cancel', 'write', 'capture'):
            row = valid(1)
            if change == 'cleanup': row['cleanup']['verified_empty'] = False
            elif change == 'cancel': row['cancellation_count'] = 1
            elif change == 'write': row['evidence_write_succeeded'] = False
            else: row['raw_capture_completed'] = False
            with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)

    def test_rejects_vte_substitution_or_unattributed_consumer(self):
        for value in (1, False):
            row = valid(1); row['raw_probe']['vte_spawn_count'] = value
            with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)
        row = valid(1); row['raw_probe']['consumer_dispatch_ids'] = [99]
        with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)

    def test_rejects_missing_echo_or_second_tab(self):
        row = valid(32); row['raw_probe']['painted_markers'] = []
        with self.assertRaises(ValueError): smoke.validate(row, 32, 'a' * 64)
        row = valid(32)
        for k in row['keys']: k['pid'] = 100
        row['raw_probe']['painted_markers'] = [(100, k['sequence'], 1) for k in row['keys']]
        with self.assertRaises(ValueError): smoke.validate(row, 32, 'a' * 64)

    def test_rejects_unproven_tab_or_pane_identity(self):
        for field in ('scheduled_tab_id', 'pid', 'target_snapshot_ns'):
            row = valid(32); row['keys'][1][field] = -1
            with self.assertRaises(ValueError): smoke.validate(row, 32, 'a' * 64)

    def test_rejects_wrong_binary_topology_and_one_pane_drag(self):
        row = valid(1)
        with self.assertRaises(ValueError): smoke.validate(row, 1, 'b' * 64)
        row['drags'] = [{}]
        with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)
        row = valid(32); row['setup_snapshot']['tabs'][1]['panes'][0]['mapped'] = True
        with self.assertRaises(ValueError): smoke.validate(row, 32, 'a' * 64)


    def test_actual_collector_schedule_uses_fractional_time_and_integer_indices(self):
        spec = importlib.util.spec_from_file_location('collector', Path(__file__).with_name('collect-interaction-budget.py'))
        collector = importlib.util.module_from_spec(spec); spec.loader.exec_module(collector)
        for interval, times, expected in ((.5, [.1, .7, 1.3], [0, 1, 0]),
                                           (10, [.1, .7, 1.3, 9.9, 10, 19.9, 20], [0, 0, 0, 0, 1, 1, 0])):
            actual = [collector.scheduled_tab_index(t, interval, 2) for t in times]
            self.assertEqual(actual, expected)
            self.assertTrue(all(type(value) is int for value in actual))
            self.assertTrue(all(collector.scheduled_tab_index(t, interval, 1) == 0 for t in times))

    def test_rejects_two_distinct_panes_in_same_tab(self):
        row = valid(32); middle = row['keys'][1]
        middle.update(pid=101, active_tab_id=10, scheduled_tab_id=10)
        snapshot = row['snapshots'][1]; snapshot['active_tab_index'] = 0
        for i, tab in enumerate(snapshot['tabs']):
            for pane in tab['panes']: pane['mapped'] = i == 0
        row['raw_probe']['painted_markers'][1] = (101, 1, middle['injected_ns'] + 1)
        with self.assertRaisesRegex(ValueError, 'every topology tab'): smoke.validate(row, 32, 'a' * 64)

    def test_rejects_workload_tab_list_mismatch(self):
        for ids in ([10], [11, 10], [10, 10], [10, 12]):
            row = valid(32); row['workload']['tab_ids'] = ids
            with self.assertRaises(ValueError): smoke.validate(row, 32, 'a' * 64)

    def test_rejects_triplicated_key_and_single_early_marker_witness(self):
        row = valid(1); row['keys'] = [copy.deepcopy(row['keys'][0]) for _ in range(3)]
        row['raw_probe']['painted_markers'] = [(100, 1, 1)]
        with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)

    def test_rejects_duplicate_ids_sequences_and_paint_markers(self):
        for field in ('id', 'sequence', 'injected_ns'):
            row = valid(1); row['keys'][1][field] = row['keys'][0][field]
            with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)
        row = valid(1); row['keys'][1]['sequence'] = 3
        with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)
        row = valid(1); row['raw_probe']['painted_markers'].append(row['raw_probe']['painted_markers'][0])
        with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)

    def test_rejects_pre_injection_paint_with_otherwise_valid_record(self):
        row = valid(1); key = row['keys'][1]
        row['raw_probe']['painted_markers'][1] = (key['pid'], key['sequence'], key['injected_ns'] - 1)
        with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)

    def test_rejects_duplicate_or_reordered_snapshots(self):
        for kind in ('duplicate', 'request_order', 'timestamp_order'):
            row = valid(1)
            if kind == 'duplicate': row['snapshots'].append(copy.deepcopy(row['snapshots'][-1]))
            elif kind == 'request_order': row['snapshots'].reverse()
            else: row['snapshots'][1]['monotonic_ns'] = 0
            with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)

    def test_rejects_production_config_fallback(self):
        row = valid(1); row['setup_snapshot']['fixture_program_matches'] = False
        with self.assertRaises(ValueError): smoke.validate(row, 1, 'a' * 64)

    def test_build_provenance_rejects_reused_or_wrong_source_artifact(self):
        import tempfile
        spec = importlib.util.spec_from_file_location('build', Path(__file__).with_name('build-interaction-smoke.py'))
        build = importlib.util.module_from_spec(spec); spec.loader.exec_module(build)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / 'source'; root.mkdir()
            target = Path(directory) / 'target'; (target / 'release').mkdir(parents=True)
            executable = target / 'release/anvil'; executable.write_bytes(b'binary')
            message = dict(reason='compiler-artifact', target=dict(name='anvil'), executable=str(executable),
                           manifest_path=str(root / 'Cargo.toml'), fresh=False)
            self.assertEqual(build.validate_artifact([message], root, target), executable)
            for changes in (dict(fresh=True), dict(manifest_path=str(root / 'wrong.toml')),
                            dict(executable=str(root))):
                bad = dict(message, **changes)
                with self.assertRaises(ValueError): build.validate_artifact([bad], root, target)
            with self.assertRaises(ValueError): build.validate_artifact([message, message], root, target)
            with self.assertRaises(ValueError): build.build(root, target, executable, root, 'candidate')

    def test_wrapper_records_exact_source_and_rejects_wrong_cwd(self):
        import tempfile, os, sys, subprocess, json
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / 'source'; (root / 'src').mkdir(parents=True)
            (root / 'src/main.rs').write_text('fn main() {}')
            fake = Path(directory) / 'rustc'; fake.write_text('#!' + sys.executable + '\n'); fake.chmod(0o700)
            record = Path(directory) / 'rustc.jsonl'
            env = dict(os.environ, A04_EXPECTED_SOURCE=str(root), A04_RUSTC_RECORD=str(record))
            command = [sys.executable, str(Path(__file__).with_name('build-interaction-smoke.py').resolve()),
                       '--rustc-wrapper', str(fake), '--crate-name', 'anvil', 'src/main.rs']
            subprocess.run(command, cwd=root, env=env, check=True)
            row = json.loads(record.read_text()); self.assertEqual(row['source'], str(root / 'src/main.rs'))
            self.assertEqual(row['returncode'], 0)
            failed = subprocess.run(command, cwd=directory, env=env, capture_output=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(len(record.read_text().splitlines()), 1)

    def test_complete_git_inputs_include_assets_and_reject_untracked_delta(self):
        import tempfile, subprocess, shutil, json
        spec = importlib.util.spec_from_file_location('build', Path(__file__).with_name('build-interaction-smoke.py'))
        build = importlib.util.module_from_spec(spec); spec.loader.exec_module(build)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / 'source'; root.mkdir()
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            for name in ['src/main.rs', 'build.rs', 'Cargo.lock', 'config.toml.example', 'scripts/shell-integration/test']:
                path = root / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_text('fixture')
            subprocess.run(['git', '-C', str(root), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(root), '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                            'commit', '-qm', 'fixture'], check=True)
            observed = Path(directory) / 'observed'
            shutil.copytree(root, observed, ignore=shutil.ignore_patterns('.git'))
            (observed / 'scripts/interaction_budget_probe.rs').write_text('probe')
            (observed / 'interaction-overlay.json').write_text('{}')
            build.record_source(root, observed)
            record = json.loads((observed / 'interaction-source.json').read_text())
            self.assertIn('config.toml.example', record['prepared_files'])
            self.assertIn('scripts/shell-integration/test', record['prepared_files'])
            self.assertEqual(len(record['tracked_entries']), 5)
            (observed / 'interaction-source.json').unlink()
            (observed / 'config.toml.example').write_text('changed embedded asset')
            with self.assertRaises(ValueError): build.record_source(root, observed)
            (observed / 'config.toml.example').write_text('fixture')
            (root / 'build.rs').write_text('dirty checkout')
            with self.assertRaises(ValueError): build.record_source(root, observed)

    def test_rejects_cargo_compiler_and_flags_environment_aliases(self):
        import os, tempfile
        from unittest import mock
        spec = importlib.util.spec_from_file_location('build', Path(__file__).with_name('build-interaction-smoke.py'))
        build = importlib.util.module_from_spec(spec); spec.loader.exec_module(build)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); target = root / 'target'
            for key in ('RUSTC', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
                        'CARGO_BUILD_RUSTC', 'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER',
                        'CARGO_BUILD_RUSTC_WRAPPER', 'CARGO_BUILD_RUSTFLAGS',
                        'CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS'):
                with self.subTest(key=key), mock.patch.dict(os.environ, {key: 'override'}):
                    with self.assertRaisesRegex(ValueError, 'override environment'):
                        build.build(root, target, root / 'binary', root, 'candidate')


if __name__ == '__main__': unittest.main()
