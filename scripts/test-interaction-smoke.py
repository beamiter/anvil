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
                setup_snapshot=dict(tabs=tabs), fixtures=[{'pid': 100 + n} for n in range(panes)],
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


if __name__ == '__main__': unittest.main()
