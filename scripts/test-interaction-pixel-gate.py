#!/usr/bin/env python3
import copy
import importlib.util
from pathlib import Path
import unittest
spec = importlib.util.spec_from_file_location('pixel_runner', Path(__file__).with_name('run-interaction-pixel-gate.py'))
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)

class ObserverCost(unittest.TestCase):
    def raw(self):
        response = dict(cpu_ns=5, elapsed_ns=12, samples=[dict(started_ns=2, finished_ns=7)])
        return dict(pixel_events=[dict(before=response, after=response)], pixel_drags=[], active_started_ns=10,
                    active_stopped_ns=2_000_000_010, active_collector_cpu_ns=12,
                    active_app_before=dict(cpu_ticks=5), active_app_after=dict(cpu_ticks=8), cpu_ticks_per_second=100)
    def test_records_independent_costs(self):
        cost=m.observer_cost(self.raw()); self.assertEqual(cost['captures'], 2)
        self.assertEqual(cost['observer_cpu_ns'], 10); self.assertEqual(cost['app_cpu_ticks'], 3)
        self.assertEqual(cost['capture_max_ns'], 5)
    def test_control_zero_observer_cost(self):
        raw=self.raw(); raw['pixel_events']=[]
        cost=m.observer_cost(raw); self.assertEqual(cost['captures'], 0); self.assertEqual(cost['observer_cpu_ns'], 0)
    def test_slow_readback_rejected(self):
        raw=self.raw(); raw['pixel_events'][0]['before']['samples'][0]['finished_ns']=250_000_003
        with self.assertRaises(ValueError): m.observer_cost(raw)
    def test_slow_request_rejected(self):
        raw=self.raw(); raw['pixel_events'][0]['before']['elapsed_ns']=1_500_000_001
        with self.assertRaises(ValueError): m.observer_cost(raw)
    def test_overlong_active_window_rejected(self):
        raw=self.raw(); raw['active_stopped_ns']=8_000_000_011
        with self.assertRaises(ValueError): m.observer_cost(raw)
    def test_counter_type_and_cpu_backwards(self):
        for key,value in [('active_collector_cpu_ns', True),('cpu_ticks_per_second',0),('active_collector_cpu_ns',float('nan'))]:
            raw=self.raw();raw[key]=value
            with self.assertRaises(ValueError):m.observer_cost(raw)
        raw=self.raw();raw['active_app_after']['cpu_ticks']=4
        with self.assertRaises(ValueError):m.observer_cost(raw)

if __name__ == '__main__':unittest.main()
