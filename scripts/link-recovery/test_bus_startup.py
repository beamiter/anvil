"""Pipe-only tests: no bus, socket, namespace, GUI, browser or URI dispatch."""
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest
from bus_startup import read_bus_address, stop_children


class StartupTests(unittest.TestCase):
    def run_producer(self, program, expected_error=None):
        process = subprocess.Popen([sys.executable, '-c', program],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            env={'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8'})
        started = time.monotonic()
        try:
            if expected_error:
                with self.assertRaisesRegex(RuntimeError, expected_error) as caught:
                    read_bus_address(process, '/private/owned/bus', timeout=.3)
                self.assertNotIn('secret', str(caught.exception))
            else:
                address = read_bus_address(process, '/private/owned/bus', timeout=2)
                self.assertTrue(address.startswith('unix:path=/private/owned/bus'))
        finally:
            stop_children([process], timeout=.3)
        self.assertIsNotNone(process.poll(), 'child must be reaped after failure or success')
        self.assertTrue(process.stdout.closed)
        self.assertLess(time.monotonic() - started, 3)

    def test_hung_startup_times_out_and_reaps(self):
        self.run_producer('import time; time.sleep(30)', 'readiness timeout')

    def test_partial_line_does_not_block_readline(self):
        self.run_producer("import os,time; os.write(1,b'unix:path=/private'); time.sleep(30)", 'readiness timeout')

    def test_early_exit_reaps(self):
        self.run_producer('raise SystemExit(7)', 'exited|closed')

    def test_closed_pipe_without_exit_fails(self):
        self.run_producer('import os,time; os.close(1); time.sleep(30)', 'closed')

    def test_malformed_address_does_not_echo_input(self):
        self.run_producer("import os,time; os.write(1,b'tcp:host=secret\\n'); time.sleep(30)", 'unexpected')

    def test_prefix_only_match_is_rejected(self):
        self.run_producer("import os,time; os.write(1,b'unix:path=/private/owned/bus-secret\\n'); time.sleep(30)", 'unexpected')

    def test_non_ascii_address_is_rejected(self):
        self.run_producer("import os,time; os.write(1,b'\\xffsecret\\n'); time.sleep(30)", 'non-ASCII')

    def test_oversize_partial_line_is_bounded(self):
        self.run_producer("import os,time; os.write(1,b'x'*4097); time.sleep(30)", 'exceeds limit')

    def test_extra_line_is_rejected(self):
        self.run_producer("import os,time; os.write(1,b'unix:path=/private/owned/bus\\nsecret\\n'); time.sleep(30)", 'extra address')

    def test_valid_complete_address_is_positive_control(self):
        self.run_producer("import os,time; os.write(1,b'unix:path=/private/owned/bus,guid=' + b'a'*32 + b'\\n'); time.sleep(30)")

    def test_cleanup_attempts_later_children_after_unreapable_one(self):
        class FakeChild:
            stdout = None
            def __init__(self, stuck): self.stuck, self.actions = stuck, []
            def poll(self): return None
            def terminate(self): self.actions.append('terminate')
            def kill(self): self.actions.append('kill')
            def wait(self, timeout):
                self.actions.append('wait')
                if self.stuck: raise subprocess.TimeoutExpired('owned child', timeout)
                return 0
        stuck, normal = FakeChild(True), FakeChild(False)
        with self.assertRaisesRegex(RuntimeError, 'cleanup failed'):
            stop_children([stuck, normal], timeout=.01)
        self.assertEqual(stuck.actions, ['terminate', 'wait', 'kill', 'wait'])
        self.assertEqual(normal.actions, ['terminate', 'wait'])

    def test_runner_keeps_startup_inside_finally_cleanup(self):
        import ast
        source = ast.parse(Path(__file__).with_name('run_suite.py').read_text())
        matched = [node for node in ast.walk(source) if isinstance(node, ast.Try)
                   and any(isinstance(call, ast.Call) and isinstance(call.func, ast.Name)
                           and call.func.id == 'read_bus_address'
                           for item in node.body for call in ast.walk(item))]
        self.assertEqual(len(matched), 1)
        self.assertTrue(any(isinstance(call, ast.Call) and isinstance(call.func, ast.Name)
                            and call.func.id == 'stop_children'
                            for item in matched[0].finalbody for call in ast.walk(item)))


if __name__ == '__main__':
    unittest.main()
