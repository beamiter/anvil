#!/usr/bin/env python3
"""Collector contract checks without X11, sockets, PTYs or application launch."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
spec = importlib.util.spec_from_file_location("collector", Path(__file__).with_name("collect-interaction-budget.py"))
collector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collector)


class Collector(unittest.TestCase):
    def test_proc_fields_are_rss_cpu_and_context_switches_not_wakes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); proc = root / "123"; proc.mkdir()
            (proc / "status").write_text("Name:\tfixture\nVmRSS:\t100 kB\n")
            # /proc stat field 3 starts after the final parenthesis, even for a spaced name.
            fields = ["S"] + ["0"] * 30; fields[11] = "11"; fields[12] = "7"
            (proc / "stat").write_text("123 (fixture name) " + " ".join(fields))
            for tid, v, n in ((123, 5, 2), (124, 7, 4)):
                thread = proc / "task" / str(tid); thread.mkdir(parents=True)
                (thread / "status").write_text(f"voluntary_ctxt_switches:\t{v}\nnonvoluntary_ctxt_switches:\t{n}\n")
            sample = collector.proc_sample(123, root)
            self.assertEqual(sample["rss_bytes"], 102400)
            self.assertEqual(sample["cpu_ticks"], 18)
            self.assertEqual(sample["threads"]["124"]["voluntary_ctxt_switches"], 7)
            self.assertNotIn("wakes", sample)

    def test_missing_report_is_pending(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertIsNone(collector.read_json(Path(directory) / "absent"))

    def test_no_accidental_full_run(self):
        result = subprocess.run([sys.executable, str(Path(collector.__file__)), "--binary", "/absent",
                                 "--panes", "1", "--output", "/absent/output"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("--smoke --full-budget", result.stderr)

    def test_dead_application_cannot_satisfy_wait(self):
        class Dead:
            def poll(self): return 1
        with self.assertRaises(RuntimeError): collector.wait_for(lambda: True, Dead())




class Finalization(unittest.TestCase):
    class Process:
        pid = 100
        def __init__(self, timeout=False): self.events = []; self.alive = True; self.timeout = timeout
        def poll(self): return None if self.alive else 0
        def terminate(self): self.events.append("terminate")
        def kill(self): self.events.append("kill"); self.alive = False
        def wait(self, timeout=None):
            self.events.append("wait")
            if self.timeout and self.alive: raise subprocess.TimeoutExpired("fixture", timeout)
            self.alive = False; return 0

    class Owner:
        def __init__(self, survives=False, raises=False): self.events = []; self.survives = survives; self.raises = raises
        def identities(self): return [dict(pid=100, ppid=1, start_ticks=10)]
        def cleanup(self):
            self.events.append("cleanup")
            if self.raises: raise OSError("cleanup fault")
            return dict(verified_empty=not self.survives, remaining_pids=[101] if self.survives else [], errors=[])
        def close(self): self.events.append("close")

    class Display:
        def close(self): raise OSError("XCloseDisplay fault")

    def test_output_failure_and_display_error_cannot_skip_any_process_lane(self):
        import contextlib, io
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); reports = root / "reports"; reports.mkdir()
            (reports / "101.json").write_text('{"pid":101,"input_count":2}')
            (root / "response.json").write_text('{"request":5}')
            output = root / "directory-instead-of-file"; output.mkdir()
            process = self.Process(timeout=True); owner = self.Owner()
            evidence = dict(keys=[dict(id=0)], drags=[dict(id=0)], resources=[dict(rss_bytes=1)], snapshots=[dict(request=1)])
            with contextlib.redirect_stderr(io.StringIO()):
                completed = collector.finalize(evidence, output, process, self.Display(), None, owner, root, reports, True)
            self.assertFalse(completed)
            self.assertEqual(process.events, ["terminate", "wait", "kill", "wait"])
            self.assertEqual(owner.events, ["cleanup", "close"])
            saved = json.loads(Path(evidence["fallback_evidence_path"]).read_text())
            self.assertEqual(saved["keys"], [dict(id=0)])
            self.assertEqual(saved["drags"], [dict(id=0)])
            self.assertEqual(saved["resources"], [dict(rss_bytes=1)])
            self.assertEqual(saved["snapshots"], [dict(request=1)])
            self.assertEqual(saved["fixture_reports"]["101"]["input_count"], 2)
            self.assertEqual(saved["last_response"]["request"], 5)
            self.assertFalse(saved["raw_capture_completed"])

    def test_child_survival_is_never_success(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); output = root / "evidence.json"
            evidence = {}
            completed = collector.finalize(evidence, output, self.Process(), None, None,
                                             self.Owner(survives=True), root, root, True)
            self.assertFalse(completed)
            self.assertEqual(json.loads(output.read_text())["cleanup"]["remaining_pids"], [101])

    def test_ownership_exception_still_terminates_and_waits_app(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); process = self.Process()
            completed = collector.finalize({}, root / "result.json", process, None, None,
                                             self.Owner(raises=True), root, root, True)
            self.assertFalse(completed)
            self.assertEqual(process.events, ["terminate", "wait"])

    def test_success_only_after_verified_empty_and_report_written(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); evidence = {}
            self.assertTrue(collector.finalize(evidence, root / "out.json", self.Process(), None,
                                              None, self.Owner(), root, root, True))
            self.assertTrue(json.loads((root / "out.json").read_text())["cleanup"]["verified_empty"])


class RealPtyCleanup(unittest.TestCase):
    def test_real_pty_descendant_ignores_term_then_is_killed_and_reaped_on_failure(self):
        """Owned Linux PTY only: no AF_UNIX, display, GTK, or network."""
        import contextlib, io, os, signal, time
        owner = collector.OwnedProcesses()
        process = None
        master, slave = os.openpty()
        try:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory); ready = root / "descendant-ready"
                code = '''import os,sys,signal,select
signal.signal(signal.SIGTERM, signal.SIG_IGN)
child=os.fork()
if child == 0:
    open(sys.argv[1], "w").write(str(os.getpid()))
while True: select.select([int(sys.argv[2])], [], [], None)
'''
                process = subprocess.Popen([sys.executable, "-c", code, str(ready), str(slave)], pass_fds=(slave,))
                owner.bind(process)
                deadline = time.monotonic() + 5
                while not ready.exists() and time.monotonic() < deadline:
                    owner.refresh(); time.sleep(.01)
                self.assertTrue(ready.exists(), "owned PTY descendant failed to start")
                child = int(ready.read_text()); owner.refresh()
                self.assertIn(child, owner.handles)
                identities = {row["pid"]: row["start_ticks"] for row in owner.identities()}
                output = root / "unwritable-as-file"; output.mkdir()
                evidence = dict(keys=[dict(id=0)], resources=[dict(rss_bytes=99)])
                with contextlib.redirect_stderr(io.StringIO()):
                    result = collector.finalize(evidence, output, process, Finalization.Display(),
                                                None, owner, root, root, False)
                self.assertFalse(result)
                self.assertTrue(evidence["cleanup"]["verified_empty"], evidence["cleanup"])
                for pid, start in identities.items():
                    row = collector.process_identity(pid)
                    self.assertTrue(row is None or row["start_ticks"] != start)
                self.assertIsNotNone(process.returncode)
                self.assertEqual(json.loads(Path(evidence["fallback_evidence_path"]).read_text())["keys"], [dict(id=0)])
        finally:
            if owner.handles:
                owner.cleanup(grace=.2); owner.close()
            if process is not None and process.poll() is None:
                process.kill(); process.wait()
            os.close(master); os.close(slave)


class MainFailure(unittest.TestCase):
    def test_readiness_failure_with_directory_output_still_cleans_owned_application(self):
        from unittest import mock
        import contextlib, io, os
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); output = root / "evidence-directory"; output.mkdir()
            process = Finalization.Process(timeout=True)
            owner = Finalization.Owner()
            owner.bind = lambda process: None
            owner.refresh = lambda: {}
            arguments = ["collector", "--binary", str(Path(__file__).resolve()), "--panes", "1",
                         "--output", str(output), "--smoke"]
            with mock.patch.object(sys, "argv", arguments), \
                 mock.patch.dict(os.environ, {"DISPLAY": ":999", "ANVIL_BUDGET_ISOLATED_DISPLAY": "1"}), \
                 mock.patch.object(collector.subprocess, "Popen", return_value=process), \
                 mock.patch.object(collector, "OwnedProcesses", return_value=owner), \
                 mock.patch.object(collector, "wait_for", side_effect=RuntimeError("injected readiness failure")), \
                 contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(RuntimeError): collector.main()
            self.assertEqual(process.events, ["terminate", "wait", "kill", "wait"])
            self.assertEqual(owner.events, ["cleanup", "close"])
            fallbacks = list(root.glob("evidence-directory.failed-*.json"))
            self.assertEqual(len(fallbacks), 1)
            saved = json.loads(fallbacks[0].read_text())
            self.assertEqual(saved["error"], "injected readiness failure")
            self.assertEqual(saved["keys"], [])
            self.assertFalse(saved["raw_capture_completed"])


class OwnershipBoundary(unittest.TestCase):
    def test_early_app_exit_leaves_adopted_descendant_discoverable(self):
        import os, time
        owner = collector.OwnedProcesses(); process = None
        try:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory); ready = root / "ready"
                code = "import os,sys,time\nif os.fork(): os._exit(0)\nopen(sys.argv[1], 'w').write(str(os.getpid()))\ntime.sleep(60)\n"
                process = subprocess.Popen([sys.executable, "-c", code, str(ready)])
                process.wait(timeout=5)
                deadline = time.monotonic() + 5
                while not ready.exists() and time.monotonic() < deadline: time.sleep(.01)
                self.assertTrue(ready.exists())
                child = int(ready.read_text())
                owner.bind(process)  # Deliberately only discover after app has died.
                self.assertIn(child, owner.handles)
                self.assertEqual(owner.observed[child]["ppid"], os.getpid())
                result = owner.cleanup(grace=.3)
                self.assertTrue(result["verified_empty"], result)
                self.assertIsNone(collector.process_identity(child))
        finally:
            if owner.handles: owner.cleanup(grace=.2)
            owner.close()
            if process is not None and process.poll() is None: process.kill(); process.wait()

    def test_failed_atomic_report_keeps_previous_complete_file(self):
        from unittest import mock
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"; output.write_text('{"previous":true}')
            with mock.patch.object(collector.json, "dump", side_effect=OSError("partial-write fault")):
                with self.assertRaises(OSError): collector.write_evidence(output, {"new": True})
            self.assertEqual(json.loads(output.read_text()), {"previous": True})
            self.assertEqual(list(Path(directory).glob("*.pending-*")), [])


class CancellationBoundary(unittest.TestCase):
    def test_record_only_handlers_preserve_cleanup_and_restore(self):
        import signal
        previous = {s: signal.getsignal(s) for s in (signal.SIGTERM, signal.SIGINT)}
        evidence = {}
        cancellation = collector.CancellationSignals(evidence)
        try:
            cancellation._record(signal.SIGTERM, None)
            with self.assertRaises(collector.CollectorCancelled): cancellation.check()
            cancellation.begin_cleanup()
            cancellation._record(signal.SIGINT, None)
            cancellation.check()
            self.assertEqual(evidence['cancellation_count'], 2)
        finally: cancellation.restore()
        self.assertEqual({s: signal.getsignal(s) for s in previous}, previous)

    def test_dedicated_collector_term_and_int_reap_real_pty_tree_only(self):
        import os, signal, time
        for cancellation_signal in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=cancellation_signal), tempfile.TemporaryDirectory() as directory:
                root = Path(directory); ready = root / 'fixture-ready.json'
                output = root / 'evidence.json'; app = root / 'fake-app'
                app.write_text('#!' + sys.executable + '\n' + '''import os,signal,json,time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.signal(signal.SIGINT, signal.SIG_IGN)
master,slave=os.openpty()
child=os.fork()
if child == 0:
    os.close(master)
    while True: time.sleep(1)
os.close(slave)
with open(os.environ['A04_SIGNAL_TEST_READY'], 'w') as report:
    json.dump({'app':os.getpid(), 'child':child}, report)
while True: time.sleep(1)
''')
                app.chmod(0o700)
                sentinel = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
                process = None; handles = {}; identities = {}
                try:
                    env = dict(os.environ, DISPLAY=':999', ANVIL_BUDGET_ISOLATED_DISPLAY='1',
                               A04_SIGNAL_TEST_READY=str(ready))
                    with open(root / 'collector.stderr', 'wb') as log:
                        process = subprocess.Popen([sys.executable, str(Path(collector.__file__)),
                            '--binary', str(app), '--panes', '1', '--output', str(output), '--smoke'],
                            env=env, stdout=log, stderr=log)
                        handles[process.pid] = os.pidfd_open(process.pid)
                        deadline = time.monotonic() + 5
                        fixture = None
                        while time.monotonic() < deadline:
                            if ready.exists():
                                try: fixture = json.loads(ready.read_text()); break
                                except json.JSONDecodeError: pass
                            if process.poll() is not None: break
                            time.sleep(.01)
                        self.assertIsNotNone(fixture, (root / 'collector.stderr').read_text())
                        for pid in fixture.values():
                            identities[pid] = collector.process_identity(pid)['start_ticks']
                            handles[pid] = os.pidfd_open(pid)
                        signal.pidfd_send_signal(handles[process.pid], cancellation_signal)
                        # A second catchable cancellation during TERM grace cannot
                        # interrupt the unconditional cleanup and evidence lanes.
                        time.sleep(.15)
                        signal.pidfd_send_signal(handles[process.pid], cancellation_signal)
                        process.wait(timeout=15)
                    self.assertNotEqual(process.returncode, 0)
                    saved = json.loads(output.read_text())
                    self.assertFalse(saved['raw_capture_completed'])
                    self.assertGreaterEqual(saved['cancellation_count'], 2)
                    self.assertIn(cancellation_signal, saved['cancellation_signals'])
                    self.assertTrue(saved['cleanup']['verified_empty'], saved['cleanup'])
                    recorded = {row['pid']: row['start_ticks'] for row in saved['owned_identities']}
                    for pid, start in identities.items():
                        self.assertEqual(recorded[pid], start)
                        row = collector.process_identity(pid)
                        self.assertTrue(row is None or row['start_ticks'] != start)
                    self.assertNotIn(sentinel.pid, recorded)
                    self.assertIsNone(sentinel.poll(), 'unrelated outside sentinel was terminated')
                finally:
                    for handle in handles.values():
                        try: signal.pidfd_send_signal(handle, signal.SIGKILL)
                        except ProcessLookupError: pass
                        os.close(handle)
                    if process is not None: process.wait(timeout=5)
                    sentinel.terminate(); sentinel.wait(timeout=5)

class FixtureLauncher(unittest.TestCase):
    def test_single_executable_config_path_execs_fixture_without_changing_pid(self):
        import os, tomllib
        with tempfile.TemporaryDirectory(prefix="a04 quoted '") as directory:
            root = Path(directory); fixture = root / "fixture with space.py"
            fixture.write_text("import os,sys,json; print(json.dumps(dict(pid=os.getpid(),args=sys.argv[1:])))")
            reports = root / 'reports'; reports.mkdir(mode=0o700)
            launcher = collector.create_fixture_launcher(root, fixture, reports)
            config = tomllib.loads('shell = ' + json.dumps(str(launcher)))
            self.assertEqual(config['shell'], str(launcher))
            self.assertTrue(Path(config['shell']).is_file() and os.access(config['shell'], os.X_OK))
            self.assertEqual(launcher.stat().st_mode & 0o777, 0o700)
            process = subprocess.Popen([config['shell']], stdout=subprocess.PIPE, text=True)
            output, _ = process.communicate(timeout=5)
            self.assertEqual(process.returncode, 0)
            self.assertEqual(json.loads(output), dict(pid=process.pid, args=['--reports', str(reports)]))

if __name__ == "__main__": unittest.main()
