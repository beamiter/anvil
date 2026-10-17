#!/usr/bin/env python3
"""Pure oracle tests only: does not load GI, launch processes or open a bus."""
import importlib.util
from pathlib import Path
import unittest
from unittest import mock
import json
import subprocess
import os
import signal
import tempfile
spec = importlib.util.spec_from_file_location('observer', Path(__file__).with_name('qa-task-accessibility.py'))
observer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(observer)

class OracleTests(unittest.TestCase):
    marker = 'A11 0123456789abcdef same task ['
    text = marker + '12345678]: Task failed; open Tasks for details'
    window = 'A11 fixture 0123456789abcdef'
    def event(self, **kwargs):
        row = dict(type='object:announcement', name=self.window, priority=1, text=self.text)
        row.update(kwargs)
        return row
    def speech(self):
        return f"12:34:56 SPEECH OUTPUT: '{self.text}' {{}}\n"
    def test_actual_presentation_and_polite_event_pass(self):
        observer.verify_phase([self.text], [self.event()], self.speech(), self.window, self.marker)
    def test_received_event_log_is_not_presentation(self):
        with self.assertRaises(AssertionError):
            observer.verify_phase([self.text], [self.event()], 'EVENT: '+self.text, self.window, self.marker)
    def test_duplicate_events_or_speech_fail(self):
        for events, speech in [([self.event()]*2,self.speech()),([self.event()],self.speech()*2)]:
            with self.assertRaises(AssertionError):
                observer.verify_phase([self.text],events,speech,self.window,self.marker)
    def test_priority_source_and_focus_fail(self):
        for events in [[self.event(priority=2)],[self.event(name='other')],
                       [self.event(),self.event(type='object:state-changed:focused',name='other')],
                       [self.event(),self.event(type='window:activate')]]:
            with self.assertRaises(AssertionError):
                observer.verify_phase([self.text],events,self.speech(),self.window,self.marker)
    def test_quiet_phase_rejects_replay(self):
        observer.verify_phase([],[], '',self.window,self.marker)
        with self.assertRaises(AssertionError):
            observer.verify_phase([],[self.event()],self.speech(),self.window,self.marker)

class OwnershipTests(unittest.TestCase):
    class Child:
        def __init__(self, fail=False):
            self.calls = []
            self.fail = fail
        def poll(self): return None
        def terminate(self):
            self.calls.append('terminate')
            if self.fail: raise RuntimeError('fake helper failure')
        def wait(self, timeout): self.calls.append('wait'); return 0
        def kill(self): self.calls.append('kill')
    class File:
        def __init__(self): self.closed = False
        def close(self): self.closed = True
    def test_host_routes_are_removed(self):
        env = observer.child_environment(dict(PATH='/usr/bin:/bin', DISPLAY=':99',
            AT_SPI_BUS_ADDRESS='host-accessibility', WAYLAND_DISPLAY='host-wayland',
            GDK_BACKEND='wayland', DBUS_STARTER_ADDRESS='host-bus', PYTHONOPTIMIZE='1',
            LD_PRELOAD='host.so', PULSE_SERVER='unix:/private/no-pulse'))
        self.assertEqual(env['GDK_BACKEND'], 'x11')
        self.assertEqual(env['TZ'], 'UTC')
        for key in ('AT_SPI_BUS_ADDRESS','WAYLAND_DISPLAY','DBUS_STARTER_ADDRESS','PYTHONOPTIMIZE','LD_PRELOAD'):
            self.assertNotIn(key, env)
    def test_evidence_failure_still_cleans_only_owned_helpers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sentinel = root/'outside-sentinel'
            sentinel.write_text('untouched')
            first, bad = self.Child(), self.Child(fail=True)
            file = self.File()
            with self.assertRaises(FileNotFoundError):
                observer.finish_owned(root/'missing', {}, [first,bad], [file])
            self.assertIn('terminate', first.calls)
            self.assertIn('terminate', bad.calls)
            self.assertTrue(file.closed)
            self.assertEqual(sentinel.read_text(), 'untouched')
    def test_term_and_int_trigger_cleanup_without_touching_outside(self):
        for sig in (signal.SIGTERM, signal.SIGINT):
            child, file = self.Child(), self.File()
            prior = signal.signal(sig, observer.interrupted)
            try:
                with self.assertRaises(InterruptedError):
                    try:
                        signal.raise_signal(sig)
                    finally:
                        observer.cleanup_children([child], [file])
            finally:
                signal.signal(sig, prior)
            self.assertEqual(child.calls, ['terminate','wait'])
            self.assertTrue(file.closed)

class FinalizationTests(unittest.TestCase):
    Child = OwnershipTests.Child
    File = OwnershipTests.File
    class Unkillable(OwnershipTests.Child):
        def terminate(self):
            self.calls.append('terminate')
            raise RuntimeError('terminate failed')
        def kill(self):
            self.calls.append('kill')
            raise RuntimeError('kill failed')
    class BadFile:
        def close(self): raise OSError('close failed')
    def test_normal_cleanup_persists_success(self):
        with tempfile.TemporaryDirectory() as directory:
            child, file = self.Child(), self.File()
            observer.finish_owned(Path(directory), {'passed':True}, [child], [file])
            result = json.loads((Path(directory)/'result.json').read_text())
            self.assertTrue(result['passed'])
            self.assertTrue(result['cleanup_completed'])
            self.assertEqual(result['cleanup_errors'], [])
            self.assertTrue(file.closed)
            self.assertEqual(child.calls, ['terminate','wait'])
    def test_unkillable_child_persists_failure_and_attempts_other_helpers(self):
        with tempfile.TemporaryDirectory() as directory:
            first, bad, file = self.Child(), self.Unkillable(), self.File()
            with self.assertRaisesRegex(RuntimeError, 'cleanup failed'):
                observer.finish_owned(Path(directory), {'passed':True}, [first,bad], [file])
            result = json.loads((Path(directory)/'result.json').read_text())
            self.assertFalse(result['passed'])
            self.assertFalse(result['cleanup_completed'])
            self.assertEqual(len(result['cleanup_errors']), 2)
            self.assertEqual(first.calls, ['terminate','wait'])
            self.assertEqual(bad.calls, ['terminate','kill'])
            self.assertTrue(file.closed)
    def test_file_close_failure_does_not_skip_other_files(self):
        with tempfile.TemporaryDirectory() as directory:
            file = self.File()
            with self.assertRaisesRegex(RuntimeError, 'close failed'):
                observer.finish_owned(Path(directory), {'passed':True}, [], [self.BadFile(), file])
            result = json.loads((Path(directory)/'result.json').read_text())
            self.assertFalse(result['passed'])
            self.assertEqual(len(result['cleanup_errors']), 1)
            self.assertTrue(file.closed)
    def test_version_hang_is_bounded_and_cleanup_still_runs(self):
        with tempfile.TemporaryDirectory() as directory:
            child = self.Child()
            result = {'passed':False}
            with mock.patch.object(observer.subprocess, 'check_output', side_effect=subprocess.TimeoutExpired('orca',5)) as probe:
                with self.assertRaises(subprocess.TimeoutExpired):
                    try:
                        observer.orca_version({'PATH':'/usr/bin:/bin'})
                    finally:
                        observer.finish_owned(Path(directory), result, [child], [])
                self.assertEqual(probe.call_args.kwargs['timeout'], 5)
            self.assertEqual(child.calls, ['terminate','wait'])
            self.assertFalse(json.loads((Path(directory)/'result.json').read_text())['passed'])

class FlushedSpeechTests(unittest.TestCase):
    marker = 'A11 token same task ['
    message = marker + '12345678]: Ready for review'
    def fixtures(self):
        import datetime
        start = datetime.datetime(2026,10,16,23,59,59,tzinfo=datetime.timezone.utc).timestamp()
        return [dict(name='complete',wall_start=start,wall_end=start+0.8,expected=[self.message]),
                dict(name='quiet',wall_start=start+1,wall_end=start+1.8,expected=[])]
    def log(self, stamp='23:59:59.200000'):
        return f"{stamp} - SPEECH OUTPUT: '{self.message}' {{}}\n"
    def test_flushed_presentation_retains_phase_assignment_across_midnight(self):
        observer.verify_flushed_speech(self.log(), self.fixtures(), self.marker)
    def test_missing_duplicate_quiet_and_gap_speech_fail(self):
        for log in ['',self.log()*2,self.log()+self.log('00:00:00.200000'),
                    self.log()+self.log('23:59:59.900000')]:
            with self.assertRaises(AssertionError):
                observer.verify_flushed_speech(log,self.fixtures(),self.marker)
    def test_raw_event_and_untimestamped_speech_cannot_pass(self):
        for log in ['EVENT: '+self.message, f"SPEECH OUTPUT: '{self.message}'"]:
            with self.assertRaises(AssertionError):
                observer.verify_flushed_speech(log,self.fixtures(),self.marker)

class AttributionRegressionTests(unittest.TestCase):
    fixtures = FlushedSpeechTests.fixtures
    log = FlushedSpeechTests.log
    marker = FlushedSpeechTests.marker
    message = FlushedSpeechTests.message
    def test_overlap_with_outside_record_cannot_cancel_assignment_count(self):
        phases = self.fixtures()
        phases[1].update(wall_start=phases[0]['wall_start'],wall_end=phases[0]['wall_end'],expected=[self.message])
        with self.assertRaises(AssertionError):
            observer.verify_flushed_speech(self.log()+self.log('00:00:05.000000'),phases,self.marker)
    def test_backwards_phase_interval_is_rejected(self):
        phases = self.fixtures()[::-1]
        with self.assertRaises(AssertionError):
            observer.verify_flushed_speech(self.log(),phases,self.marker)
    def test_continuous_reference_rejects_interphase_rollback(self):
        with mock.patch.object(observer.time,'time',side_effect=[1000,1001,999]), \
             mock.patch.object(observer.time,'monotonic',side_effect=[10,11,12]):
            guard = observer.ClockGuard()
            self.assertEqual(guard.checkpoint(),1001)
            with self.assertRaises(AssertionError):
                guard.checkpoint()

class ShutdownOrderingTests(unittest.TestCase):
    class Clock:
        def checkpoint(self): pass
    class Orca:
        returncode = None
        def __init__(self): self.terminated = False
        def terminate(self): self.terminated = True
        def poll(self): return self.returncode
    def test_shutdown_pumps_owned_bus_before_gui_can_be_released(self):
        orca = self.Orca()
        state = {'gui_alive':True,'pumps':0}
        def pump():
            self.assertTrue(state['gui_alive'])
            self.assertTrue(orca.terminated)
            state['pumps'] += 1
            orca.returncode = 0
        observer.shutdown_orca(orca,pump,self.Clock(),lambda:None,lambda:True)
        state['gui_alive'] = False
        self.assertEqual(state['pumps'],1)
    def test_hung_shutdown_remains_bounded_failure(self):
        orca = self.Orca()
        with mock.patch.object(observer.time,'monotonic',side_effect=[10,16]):
            with self.assertRaises(subprocess.TimeoutExpired):
                observer.shutdown_orca(orca,lambda:None,self.Clock(),lambda:None,lambda:True)
    def test_missing_wake_cannot_pass_even_after_clean_exit(self):
        orca = self.Orca()
        orca.returncode = 0
        with mock.patch.object(observer.time,'monotonic',side_effect=[10,11,16]):
            with self.assertRaises(subprocess.TimeoutExpired):
                observer.shutdown_orca(orca,lambda:None,self.Clock(),lambda:None,lambda:False)
    def test_late_wake_and_exit_cannot_pass(self):
        orca = self.Orca()
        orca.returncode = 0
        with mock.patch.object(observer.time,'monotonic',side_effect=[10,16]):
            with self.assertRaises(subprocess.TimeoutExpired):
                observer.shutdown_orca(orca,lambda:None,self.Clock(),lambda:None,lambda:True)
    def test_signal_or_error_exit_cannot_pass(self):
        for code in (-9,1):
            orca = self.Orca()
            orca.returncode = code
            with self.assertRaises(AssertionError):
                observer.shutdown_orca(orca,lambda:None,self.Clock(),lambda:None,lambda:True)

class UtcReplayTests(unittest.TestCase):
    def test_utc_log_replay_does_not_depend_on_observer_timezone(self):
        import time
        fixture = FlushedSpeechTests()
        for zone in ('UTC', 'Etc/GMT-8'):
            try:
                with mock.patch.dict(os.environ, {'TZ':zone}):
                    time.tzset()
                    observer.verify_flushed_speech(fixture.log(),fixture.fixtures(),fixture.marker)
            finally:
                time.tzset()

class WakeEvidenceTests(unittest.TestCase):
    def data(self):
        return [dict(pid=7,type='object:property-change:accessible-name',name='shutdown',role='frame')], dict(pid=7,name='shutdown',focus_preserved=True,draft_preserved=True)
    def test_valid_wake_requires_event_and_ack(self):
        events, ack = self.data()
        self.assertTrue(observer.validate_shutdown_wake(events,ack,7,'shutdown'))
        self.assertFalse(observer.validate_shutdown_wake([],ack,7,'shutdown'))
        self.assertFalse(observer.validate_shutdown_wake(events,None,7,'shutdown'))
    def test_transient_focus_activation_and_announcements_fail(self):
        for kind in ('object:announcement','object:state-changed:focused','window:activate'):
            events, ack = self.data()
            events.append(dict(pid=7,type=kind,name='A11 draft owner',role='entry'))
            with self.assertRaises(AssertionError):
                observer.validate_shutdown_wake(events,ack,7,'shutdown')
    def test_duplicate_and_foreign_wake_fail(self):
        events, ack = self.data()
        for rows in (events*2,[dict(events[0],pid=8)]):
            with self.assertRaises(AssertionError):
                observer.validate_shutdown_wake(rows,ack,7,'shutdown')
    def test_slow_ack_cannot_cross_absolute_deadline(self):
        orca = ShutdownOrderingTests.Orca()
        orca.returncode = 0
        with mock.patch.object(observer.time,'monotonic',side_effect=[10,11,16]):
            with self.assertRaises(AssertionError):
                observer.shutdown_orca(orca,lambda:None,ShutdownOrderingTests.Clock(),lambda:None,lambda:True)

if __name__ == '__main__':
    unittest.main()
