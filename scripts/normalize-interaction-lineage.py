#!/usr/bin/env python3
"""Pure, non-accepting full-workload lineage staging. Never run native code.

The metadata envelope binds exact raw JSON bytes to source/binary/environment
identities. It is a consistency contract, not a signature or authenticity proof.
Current collectors lack warmup timestamps and complete callback attribution;
missing evidence must be collected, never backfilled from configured durations.
"""
import argparse
import copy
import hashlib
import importlib.util
import json
from pathlib import Path

spec = importlib.util.spec_from_file_location('lineage_budget', Path(__file__).with_name('compare-interaction-budget.py'))
budget = importlib.util.module_from_spec(spec)
spec.loader.exec_module(budget)
require = budget.require
MAX_BYTES = 16 * 1024 * 1024
BLOCKERS = ['independent full-workload pixel subset', 'event-linked rendered drag geometry',
            'complete idle source attribution', 'phase-bounded throughput and resource normalization',
            'paired full-budget matrix']


def integer(value, label, minimum=0):
    require(type(value) is int and value >= minimum, label + ': invalid integer')
    return value


def bounded_list(value, label, maximum=100000):
    require(type(value) is list and len(value) <= maximum, label + ': invalid or oversized list')
    return value


def strict_json(data):
    require(type(data) is bytes and 0 < len(data) <= MAX_BYTES, 'invalid JSON size')
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, 'duplicate JSON key: ' + key)
            result[key] = value
        return result
    def constant(value):
        raise ValueError('nonfinite JSON constant: ' + value)
    return json.loads(data, object_pairs_hook=pairs, parse_constant=constant)


def topology(snapshot, count):
    require(type(snapshot) is dict and snapshot['fixture_program_matches'] is True, 'fixture identity missing')
    tabs = bounded_list(snapshot['tabs'], 'tabs', 2)
    require(len(tabs) == (2 if count == 32 else 1), 'wrong tab count')
    result = {}; all_pids = set(); mapped = 0
    for tab in tabs:
        tid = integer(tab['tab_id'], 'tab ID')
        require(tid not in result, 'duplicate tab ID')
        panes = bounded_list(tab['panes'], 'panes', 16)
        require(len(panes) == (16 if count == 32 else count), 'wrong pane count')
        ids = set()
        for pane in panes:
            pid = integer(pane['pid'], 'pane PID', 1)
            require(pid not in all_pids and pane['block'] is True and type(pane['mapped']) is bool,
                    'duplicate/non-Block/unknown pane')
            if pane['mapped']:
                require(integer(pane['columns'], 'columns') >= 16 and integer(pane['rows'], 'rows') >= 6,
                        'unreadable mapped pane')
                mapped += 1
            ids.add(pid); all_pids.add(pid)
        result[tid] = ids
    require(mapped == min(count, 16), 'wrong mapped pane count')
    active = integer(snapshot['active_tab_index'], 'active tab')
    require(active < len(tabs), 'invalid active tab')
    require(all(p['mapped'] for p in tabs[active]['panes']), 'active tab contains unmapped pane')
    return result


def setup_subset(snapshot, final):
    """Validate an incremental, owned Block tree without demanding final size."""
    require(type(snapshot) is dict and snapshot['fixture_program_matches'] is True, 'setup fixture identity missing')
    tabs = bounded_list(snapshot['tabs'], 'setup tabs', 2)
    require(0 < len(tabs) <= len(final), 'invalid incremental tab count')
    result = {}; seen = set()
    for tab in tabs:
        tid = integer(tab['tab_id'], 'setup tab ID')
        require(tid in final and tid not in result, 'unknown/duplicate setup tab')
        panes = bounded_list(tab['panes'], 'setup panes', 16)
        require(panes, 'empty setup tab')
        ids = set()
        for pane in panes:
            pid = integer(pane['pid'], 'setup pane PID', 1)
            require(pid in final[tid] and pid not in seen and pane['block'] is True and
                    type(pane['mapped']) is bool, 'invalid incremental pane identity')
            ids.add(pid); seen.add(pid)
        result[tid] = ids
    active = integer(snapshot['active_tab_index'], 'setup active tab')
    require(active < len(tabs), 'invalid setup active tab')
    return result


def stage(raw_bytes, metadata):
    """Normalize key/paint and callback relations; always budget_acceptance=False."""
    try:
        return _stage(raw_bytes, metadata)
    except (KeyError, TypeError, IndexError, OverflowError) as error:
        raise ValueError('missing or malformed evidence: ' + str(error)) from error


def _stage(raw_bytes, metadata):
    raw = strict_json(raw_bytes)
    require(type(raw) is dict and type(metadata) is dict, 'expected evidence objects')
    require(metadata['schema'] == 'a04-lineage-binding-v1', 'unsupported binding')
    require(metadata['raw_sha256'] == hashlib.sha256(raw_bytes).hexdigest(), 'raw bytes not bound')
    for name, size in [('source_sha', 40), ('source_tree', 40), ('binary_sha256', 64)]:
        budget.digest(metadata[name], size, name)
    require(type(metadata['pair_id']) is str and bool(metadata['pair_id'].strip()), 'missing pair ID')
    require(metadata['role'] in ('baseline', 'candidate'), 'invalid role')
    require(integer(metadata['repetition'], 'repetition', 1) <= 2, 'unsupported repetition')
    env = metadata['environment']
    require(type(env) is dict, 'environment object missing')
    for key in budget.ENV_KEYS:
        require(key in env and env[key] is not None and env[key] != '', 'missing environment ' + key)
        require(type(env[key]) in (str, int, float, list, dict), 'invalid environment ' + key)
    # Reject nonfinite numbers anywhere, including a direct Python API envelope.
    try: json.dumps(metadata, allow_nan=False)
    except (ValueError, TypeError) as error: raise ValueError('invalid metadata JSON') from error
    for key in budget.ENV_DIGEST_KEYS:
        budget.digest(env[key], 64, key)
    require(env['build_profile'] == 'release' and env['backend'] == 'x11' and env['renderer'] == 'cairo',
            'unsupported build/backend/renderer')
    require(raw['schema'] == 'raw-native-block-v1' and raw['smoke'] is False and raw['budget_acceptance'] is False,
            'not non-accepting full raw evidence')
    require(raw['binary_sha256'] == metadata['binary_sha256'], 'binary identity mismatch')
    require(raw['raw_capture_completed'] is True and raw['evidence_write_succeeded'] is True, 'incomplete capture')
    require(integer(raw['cancellation_count'], 'cancellation count') == 0 and not raw.get('error') and
            raw['harvest_errors'] == [] and raw['finalization_errors'] == [], 'capture failed or cancelled')
    count = integer(raw['panes'], 'panes'); require(count in (1, 8, 32), 'unsupported pane count')
    lifecycle_start = integer(raw['lifecycle_started_ns'], 'lifecycle start', 1)
    cleanup_start = integer(raw['cleanup_started_ns'], 'cleanup start', 1)
    cleanup_end = integer(raw['cleanup_ended_ns'], 'cleanup end', 1)
    stamps = [integer(raw[k], k, 1) for k in ('warmup_started_ns', 'warmup_ended_ns', 'active_started_ns',
              'active_stopped_ns', 'idle_started_ns', 'idle_ended_ns')]
    ws, we, start, stop, idle_start, idle_stop = stamps
    require(lifecycle_start <= ws < we <= start < stop <= idle_start < idle_stop < cleanup_start <= cleanup_end,
            'nonchronological phase/lifecycle windows')
    require(we-ws >= 5_000_000_000 and stop-start >= 60_000_000_000 and
            idle_start-stop >= 10_000_000_000 and idle_stop-idle_start >= 1_200_000_000_000, 'incomplete phase window')
    topo = topology(raw['setup_snapshot'], count); pids = set().union(*topo.values())
    cleanup = raw['cleanup']
    require(cleanup['verified_empty'] is True and cleanup['remaining_pids'] == [] and cleanup['errors'] == [],
            'owned cleanup not proven')
    owned = {}
    for identity in bounded_list(cleanup['owned_identities'], 'owned identities', 10000):
        pid = integer(identity['pid'], 'owned PID', 1)
        require(pid not in owned, 'duplicate owned PID')
        owned[pid] = integer(identity['start_ticks'], 'owned start ticks', 1)
    require(pids <= owned.keys(), 'fixture ownership incomplete')
    work = raw['workload']
    require(work['kind'] == 'full_budget' and type(work['key_count']) is int and work['key_count'] == 300 and
            type(work['active_seconds']) is int and work['active_seconds'] == 60 and
            type(work['tab_switch_seconds']) is int and work['tab_switch_seconds'] == 10 and
            type(work['tab_ids']) is list and all(type(t) is int for t in work['tab_ids']) and
            work['tab_ids'] == list(topo), 'wrong fixed workload')
    probe = raw['raw_probe']
    require(probe['metric'] == 'after_paint_proxy_not_display' and probe['overflow'] is False and
            probe['timer_fallback'] is False and type(probe['vte_spawn_count']) is int and
            probe['vte_spawn_count'] == 0, 'invalid production probe path')
    callbacks = {k: copy.deepcopy(bounded_list(probe[k], k)) for k in
                 ('dispatch_ids', 'consumer_dispatch_ids', 'dispatch_ms', 'consumer_ms')}
    require(callbacks['dispatch_ids'] and callbacks['consumer_dispatch_ids'], 'empty callback evidence')
    for key in ('dispatch_ms', 'consumer_ms'):
        for value in callbacks[key]: budget.number(value, key)
    callbacks.update(owned_pty_dispatch_count=len(callbacks['dispatch_ids']),
                     reader_ctx_consumer_count=len(callbacks['consumer_dispatch_ids']))
    budget.validate_callbacks(callbacks, 'raw probe')
    # Counts above describe retained arrays, not independent lifetime counters.
    # Retain every control response. Setup grows the pane tree; finish is a
    # distinct finalization observation after the idle window, before cleanup.
    snapshots = {}; classified = []; last_req = -1; last_stamp = lifecycle_start
    setup_topology = {}; saw_measured = False; finish_count = 0
    for snap in bounded_list(raw['snapshots'], 'snapshots', 10000):
        req = integer(snap['request'], 'snapshot request'); stamp = integer(snap['monotonic_ns'], 'snapshot time', 1)
        require(req > last_req and stamp >= last_stamp and stamp < cleanup_start, 'snapshot order/lifecycle invalid')
        if stamp < ws:
            require(not saw_measured, 'setup response after measured phase')
            current = setup_subset(snap, topo)
            require(all(t in current and ids <= current[t] for t, ids in setup_topology.items()),
                    'setup removed or moved established pane identities')
            setup_topology = current
            phase = 'setup'
        else:
            require(setup_topology == topo, 'setup did not establish final topology before warmup')
            if not saw_measured:
                require(topology(classified[-1]['snapshot'], count) == topo, 'final setup geometry not ready')
            require(topology(snap, count) == topo, 'measurement/finalization topology changed')
            saw_measured = True
            if stamp > idle_stop:
                require(snap == probe and type(snap['request']) is int, 'unknown post-idle control response')
                finish_count += 1; phase = 'finalization'
            elif stamp < start: phase = 'warmup'
            elif stamp < stop: phase = 'active'
            elif stamp < idle_start: phase = 'drain'
            else: phase = 'idle'
        classified.append(dict(phase=phase, snapshot=copy.deepcopy(snap)))
        snapshots[req] = snap; last_req = req; last_stamp = stamp
    require(finish_count == 1 and classified[-1]['phase'] == 'finalization', 'exact terminal finish response missing')
    require(raw['setup_snapshot']['request'] in snapshots and
            raw['setup_snapshot'] == snapshots[raw['setup_snapshot']['request']], 'legacy setup snapshot not retained')
    keys = bounded_list(raw['keys'], 'keys', 300); require(len(keys) == 300, 'incomplete key schedule')
    expected = {}; sequences = {}; rows = []; previous = start-1; previous_request = -1; seen_tabs = set()
    for i, key in enumerate(keys):
        require(type(key['id']) is int and key['id'] == i, 'duplicate/reordered key ID')
        pid = integer(key['pid'], 'key PID', 1); seq = integer(key['sequence'], 'key sequence', 1)
        require(pid in pids and seq == sequences.get(pid, 0)+1, 'invalid per-pane key sequence')
        injected = integer(key['injected_ns'], 'injection', 1); scheduled = integer(key['scheduled_ns'], 'schedule', 1)
        # Match the existing wall-clock collector formula, not observed completion.
        planned = start + int((.1 + i * (60 - .2) / 300) * 1e9)
        require(scheduled == planned and start <= scheduled <= injected < stop and injected > previous,
                'wrong schedule or injection window/order')
        req = integer(key['target_snapshot_request'], 'target request')
        require(req in snapshots and req >= previous_request, 'target snapshot missing/reordered')
        snap = snapshots[req]; active = snap['tabs'][snap['active_tab_index']]
        tid = integer(key['active_tab_id'], 'active tab ID'); wanted = integer(key['scheduled_tab_id'], 'scheduled tab ID')
        require(tid == wanted == active['tab_id'] and pid in topo[tid], 'target pane/tab mismatch')
        require(we <= integer(key['target_snapshot_ns'], 'target timestamp', 1) == snap['monotonic_ns'] <= injected,
                'target timestamp outside post-warmup measurement context')
        require(any(p['pid'] == pid and p['mapped'] is True for p in active['panes']), 'target pane not mapped')
        expected[(pid, seq)] = i; sequences[pid] = seq; seen_tabs.add(tid)
        rows.append(dict(id=i,pid=pid,sequence=seq,tab_id=tid,scheduled_ns=scheduled,injected_ns=injected,
                         lateness_ns=injected-scheduled))
        previous=injected; previous_request=req
    require(seen_tabs == set(topo), 'missing workload tab coverage')
    painted = set(); last_paint = -1
    for marker in bounded_list(probe['painted_markers'], 'painted markers', 10000):
        require(type(marker) is list and len(marker) == 3, 'malformed painted marker')
        pid, seq, stamp = [integer(v, 'paint marker', 1) for v in marker]
        identity=(pid,seq)
        require(identity in expected and identity not in painted, 'unexpected/duplicate painted marker')
        row=rows[expected[identity]]
        require(row['injected_ns'] < stamp <= idle_start and stamp >= last_paint, 'paint order/window invalid')
        row.update(painted_ns=stamp,input_to_after_paint_ms=(stamp-row['injected_ns'])/1e6)
        painted.add(identity); last_paint=stamp
    require(painted == expected.keys(), 'lost key paint')
    # Active fixture snapshots and final harvested fixture reports retain the
    # same synthetic child identities. Neither may substitute unrelated PIDs.
    fixtures = bounded_list(raw['fixtures'], 'fixtures', 32)
    require(len(fixtures) == count, 'fixture report count mismatch')
    fixture_by_pid = {}
    for report in fixtures:
        pid = integer(report['pid'], 'fixture report PID', 1)
        require(pid in pids and pid not in fixture_by_pid, 'unknown/duplicate fixture report PID')
        fixture_by_pid[pid] = report
    require(set(fixture_by_pid) == pids, 'fixture report identity set mismatch')
    final_reports = raw['fixture_reports']
    require(type(final_reports) is dict and set(final_reports) == {str(pid) for pid in pids},
            'final fixture report identity set mismatch')
    for pid in pids:
        for report in (fixture_by_pid[pid], final_reports[str(pid)]):
            require(integer(report['pid'], 'fixture PID', 1) == pid, 'fixture report key/PID mismatch')
            require(integer(report['input_count'], 'fixture input count') == sequences.get(pid, 0) and
                    integer(report['marker_sequence'], 'fixture marker sequence') == sequences.get(pid, 0),
                    'fixture report/key lineage mismatch')
    drags=bounded_list(raw['drags'], 'drags', 300)
    require(len(drags) == (0 if count == 1 else 300), 'wrong drag injection count')
    last_drag=start-1
    for i, drag in enumerate(drags):
        require(type(drag['id']) is int and drag['id'] == i, 'duplicate/reordered drag ID')
        stamp=integer(drag['injected_ns'], 'drag injection', 1)
        require(last_drag < stamp < stop and stamp >= rows[i]['injected_ns'], 'drag injection window/order invalid')
        require(type(drag['delta']) is int and drag['delta'] == (12 if i%2==0 else -12), 'wrong drag input schedule')
        last_drag=stamp
    return dict(schema='a04-full-lineage-stage-v1',budget_acceptance=False,physical_scanout_proven=False,
                comparator_compatible=False,identity=copy.deepcopy(metadata),panes=count,keys=rows,
                callbacks=callbacks,callback_counts_scope='retained_arrays_only',
                drag_injections=copy.deepcopy(drags),drag_geometry_linked=False,
                control_responses=classified,cleanup=copy.deepcopy(cleanup),fixtures=copy.deepcopy(fixtures),fixture_reports=copy.deepcopy(final_reports),
                lifecycle_windows_ns=dict(start=lifecycle_start,cleanup_start=cleanup_start,cleanup_end=cleanup_end),
                phase_windows_ns=dict(zip(('warmup_start','warmup_end','active_start','active_end','idle_start','idle_end'),stamps)),
                missing_gates=list(BLOCKERS))


def for_budget(*_args, **_kwargs):
    """No accepting conversion exists until independently reviewed gates land."""
    raise ValueError('full normalization unavailable: ' + '; '.join(BLOCKERS))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('raw',type=Path);parser.add_argument('binding',type=Path)
    args=parser.parse_args()
    try:
        for p in (args.raw,args.binding):require(p.stat().st_size <= MAX_BYTES, 'file too large')
        result=stage(args.raw.read_bytes(),strict_json(args.binding.read_bytes()))
        print(json.dumps(result,indent=2,allow_nan=False))
    except (ValueError,OSError) as error:
        parser.exit(2,str(error)+'\n')

if __name__ == '__main__':main()
