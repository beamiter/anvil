#!/usr/bin/env python3
"""Bounded IPC for the cleanup-owned input worker; no Xlib calls in this process."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys

spec = importlib.util.spec_from_file_location('input_pipe_client', Path(__file__).with_name('interaction-pixel-client.py'))
pipe = importlib.util.module_from_spec(spec); spec.loader.exec_module(pipe)

class InputClient(pipe.PixelClient):
    def __init__(self, pid, env, check_cancelled, on_spawn):
        self.check_cancelled = check_cancelled
        self.buffer = b''; self.sequence = 0; self.last_completed_ns = None
        self.process = subprocess.Popen([sys.executable, '-I', str(Path(__file__).with_name('interaction-input-helper.py'))],
                                        env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        on_spawn(self.process.pid)
        os.set_blocking(self.process.stdout.fileno(), False)
        # Initialization and every request use the same five-second parent deadline.
        response = self.command('initialize', pid=pid)
        self.identity = response['identity']
        self.native_frame = None
        self.origin = tuple(self.identity['surface_root_origin'])
        if len(self.origin) != 2 or any(type(v) is not int or not 0 <= v < maximum for v,maximum in zip(self.origin,(1600,1000))):
            raise ValueError('invalid input worker window origin')

    def command(self, kind, **values):
        name = self.sequence; self.sequence += 1
        try:
            result = self.request(kind=kind, name=name, **values)
        except TimeoutError as error:
            raise TimeoutError('owned input helper request deadline exceeded') from error
        expected = {'kind','name','identity'} if kind == 'initialize' else {'kind','name','injected_ns','completed_ns'}
        if kind == 'configure': expected |= {'origin','identity'}
        if kind == 'drag': expected.add('pointer_observation')
        if set(result) != expected or type(result['name']) is not int:
            raise ValueError('invalid input worker reply fields')
        if kind != 'initialize':
            start, end = result.get('injected_ns'), result.get('completed_ns')
            if type(start) is not int or type(end) is not int or not 0 < start <= end:
                raise ValueError('invalid input worker completion interval')
            self.last_completed_ns = end
        return result

    def configure(self, snapshot):
        origin = pipe.native_coordinate_frame(snapshot, self.identity)
        if self.native_frame is not None and self.native_frame != snapshot['native_frame']:
            raise ValueError('fixed native coordinate frame changed during short gate')
        result = self.command('configure', native_frame=snapshot['native_frame'], window_size=snapshot['window_size'])
        if result['origin'] != origin or result['identity'] != self.identity:
            raise ValueError('input helper disagrees with fresh native frame')
        self.native_frame = snapshot['native_frame']; self.origin = tuple(origin)
        return dict(snapshot_request=snapshot['request'], native_frame=self.native_frame,
                    widget_root_origin=origin, surface_identity=self.identity, configured_ns=result['completed_ns'])

    def focus(self, bounds): self.command('focus', bounds=bounds)
    def keypress(self): return self.command('key')['injected_ns']
    def drag(self, divider, delta):
        result = self.command('drag', divider={k:divider[k] for k in ('x','y','horizontal')}, delta=delta)
        self.last_drag_pointer = result['pointer_observation']
        expected = [[round(self.origin[i] + divider[['x','y'][i]] + (delta if end and i == (0 if divider['horizontal'] else 1) else 0)) for i in (0,1)] for end in (False,True)]
        if self.last_drag_pointer != dict(start_root=expected[0],end_root=expected[1]):
            raise ValueError('observed pointer differs from requested drag')
        return result['injected_ns']
    def close(self): self.close_pipes()
