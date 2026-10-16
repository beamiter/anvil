#!/usr/bin/env python3
"""Owned numeric-only XInput worker; all new XSync calls stay off cleanup owner."""
import importlib.util
import json
import math
import os
from pathlib import Path
import sys
import time


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec); sys.modules[name] = module; spec.loader.exec_module(module)
    return module


def valid_numbers(values, count):
    if not isinstance(values, list) or len(values) != count or any(type(v) not in (int, float) or not math.isfinite(v) for v in values):
        raise ValueError('invalid bounded numeric input geometry')
    return values


def execute(command, xinput):
    """Injection and completion fence must use this exact same X connection."""
    kind = command['kind']
    if kind == 'focus':
        if set(command) != {'kind', 'name', 'bounds'}: raise ValueError('unexpected focus field')
        bounds = valid_numbers(command['bounds'], 4)
        if bounds[0] < 0 or bounds[1] < 0 or min(bounds[2:]) <= 0 or bounds[0]+bounds[2] > 1600 or bounds[1]+bounds[3] > 1000:
            raise ValueError('focus outside isolated screen')
        xinput.focus(bounds); stamp = time.monotonic_ns()
    elif kind == 'key':
        if set(command) != {'kind', 'name'}: raise ValueError('unexpected key field')
        stamp = xinput.keypress()
    elif kind == 'drag':
        if set(command) != {'kind', 'name', 'divider', 'delta'}: raise ValueError('unexpected drag field')
        divider = command['divider']; delta = command['delta']
        if type(divider) is not dict or set(divider) != {'x', 'y', 'horizontal'} or type(divider['horizontal']) is not bool:
            raise ValueError('invalid divider fields')
        x, y = valid_numbers([divider['x'], divider['y']], 2)
        if type(delta) is not int or abs(delta) != 12 or not (12 <= x < 1588 and 12 <= y < 988):
            raise ValueError('drag outside isolated screen')
        stamp = xinput.drag(divider, delta)
    else: raise ValueError('unknown input command')
    xinput.x.XSync(xinput.display, False)
    return dict(kind=kind, name=command['name'], injected_ns=stamp, completed_ns=time.monotonic_ns())


def serve(source, sink, factory):
    xinput = None; sequence = 0
    try:
        while True:
            line = source.readline(4097)
            if not line: return
            if len(line) > 4096 or not line.endswith(b'\n'): raise ValueError('invalid input frame')
            command = json.loads(line)
            if type(command) is not dict or command.get('name') != sequence or type(command.get('name')) is not int:
                raise ValueError('input command identity mismatch')
            sequence += 1
            if xinput is None:
                if set(command) != {'kind','name','pid'} or command['kind'] != 'initialize' or type(command['pid']) is not int or command['pid'] <= 0:
                    raise ValueError('expected owned app initialization')
                fields = Path(f"/proc/{command['pid']}/stat").read_text().rsplit(')',1)[1].split()
                if int(fields[1]) != os.getppid(): raise ValueError('app is not owned by this collector')
                xinput = factory(command['pid'])
                result = dict(kind='initialize', name=command['name'], origin=list(xinput.origin))
            else: result = execute(command, xinput)
            sink.write(json.dumps(result, allow_nan=False)+'\n'); sink.flush()
    finally:
        if xinput is not None: xinput.close()


def main():
    if os.environ.get('ANVIL_BUDGET_ISOLATED_DISPLAY') != '1': raise SystemExit('isolated display required')
    collector = load('input_worker_collector', 'collect-interaction-budget.py')
    serve(sys.stdin.buffer, sys.stdout, collector.XInput)


if __name__ == '__main__': main()
