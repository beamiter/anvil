#!/usr/bin/env python3
"""Short paired virtual-display correctness and empirical observer cost only."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import sys


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(file))
    module = importlib.util.module_from_spec(spec); sys.modules[name] = module; spec.loader.exec_module(module)
    return module


def observer_cost(raw):
    """Recorded costs, not a statistical observer-error or full-budget guarantee."""
    samples = []
    responses = []
    for event in raw['pixel_events'] + raw['pixel_drags']:
        responses.extend((event['before'], event['after']))
    for response in responses: samples.extend(response['samples'])
    cost = dict(captures=len(samples), capture_max_ns=max((s['finished_ns'] - s['started_ns'] for s in samples), default=0),
                observer_cpu_ns=sum(r['cpu_ns'] for r in responses),
                observer_elapsed_ns=sum(r['elapsed_ns'] for r in responses),
                active_elapsed_ns=raw['active_stopped_ns'] - raw['active_started_ns'],
                collector_cpu_ns=raw['active_collector_cpu_ns'],
                app_cpu_ticks=raw['active_app_after']['cpu_ticks'] - raw['active_app_before']['cpu_ticks'],
                cpu_ticks_per_second=raw['cpu_ticks_per_second'])
    if any(type(value) is not int or value < 0 for value in cost.values()):
        raise ValueError('invalid nonnegative integer observer cost')
    if not cost['cpu_ticks_per_second'] or cost['active_elapsed_ns'] > 8_000_000_000:
        raise ValueError('short observer gate exceeded active window bound')
    if cost['capture_max_ns'] > 250_000_000 or any(r['elapsed_ns'] > 1_500_000_000 for r in responses):
        raise ValueError('observer readback/request exceeded declared short-gate bound')
    return cost


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for role in ('baseline', 'candidate'):
        parser.add_argument(f'--{role}-binary', type=Path, required=True)
        parser.add_argument(f'--{role}-sha', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    binaries = {role: getattr(args, role + '_binary').resolve(strict=True) for role in ('baseline', 'candidate')}
    digests = {role: hashlib.sha256(binary.read_bytes()).hexdigest() for role, binary in binaries.items()}
    if len(set(digests.values())) != 2: raise ValueError('distinct source builds must have distinct binaries')
    for role in binaries:
        if not re.fullmatch('[0-9a-f]{40}', getattr(args, role + '_sha')): raise ValueError('source must be pinned')
    validate = load('pixel_validation', 'validate-interaction-pixels.py').validate
    output = args.output.resolve(); output.mkdir(parents=True, exist_ok=True)
    result = dict(schema='a04-short-pixel-gate-v1', budget_acceptance=False, physical_scanout_proven=False,
                  status='started', runs=[], observer_pairs=[],
                  limitations=['Xvfb virtual display readback only', 'one pair per scenario is descriptive, not statistical',
                               'full callback, memory, drag latency and 1200-second idle budgets remain open'])
    summary = output / 'pixel-summary.json'
    def save():
        temporary = summary.with_suffix('.tmp'); temporary.write_text(json.dumps(result, indent=2) + '\n'); temporary.replace(summary)
    try:
        for panes in (1, 8, 32):
            for role in ('baseline', 'candidate'):
                costs = {}
                # Alternate order to expose gross order effects without claiming statistical control.
                modes = ('control', 'observe') if panes != 8 else ('observe', 'control')
                for mode in modes:
                    record = dict(role=role, panes=panes, mode=mode, source_sha=getattr(args, role + '_sha'),
                                  binary_sha256=digests[role], status='started')
                    result['runs'].append(record); save()
                    raw_path = output / f'{role}-{panes}-{mode}.json'
                    with raw_path.with_suffix('.collector.log').open('w') as log:
                        subprocess.run(['bash', str(Path(__file__).with_name('run-interaction-budget.sh')),
                                        str(binaries[role]), str(panes), str(raw_path), '--smoke', '--pixel-gate', mode],
                                       stdout=log, stderr=subprocess.STDOUT, check=True)
                    raw = json.loads(raw_path.read_text())
                    record.update(validate(raw, panes, digests[role], raw_path.with_suffix('.pixels') if mode == 'observe' else None))
                    costs[mode] = observer_cost(raw); record['observed_cost'] = costs[mode]; save()
                result['observer_pairs'].append(dict(role=role, panes=panes, control=costs['control'], observe=costs['observe'],
                    active_elapsed_difference_ns=costs['observe']['active_elapsed_ns'] - costs['control']['active_elapsed_ns'],
                    app_cpu_tick_difference=costs['observe']['app_cpu_ticks'] - costs['control']['app_cpu_ticks'],
                    interpretation='descriptive single-pair perturbation; no population error bound'))
        result['status'] = 'short_virtual_display_correctness_passed'
    except BaseException as error:
        result['status'] = 'failed'; result['error'] = str(error)
        raise
    finally: save()


if __name__ == '__main__': main()
