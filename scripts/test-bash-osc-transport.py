"""Owned controlling-PTY integration transport and redirected user-byte tests."""
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import signal
import subprocess
import tempfile
import termios
import time
import unittest
from unittest import mock

INTEGRATION = Path(__file__).resolve().parent / 'shell-integration/anvil.bash'


def cleanup_owned(pid, fd, original_tty):
    """Attempt every owned resource cleanup; never replace the caller's error."""
    errors = []
    def record(action, error):
        errors.append(f'{action}: {type(error).__name__}: {error}')
    def signal_child(sig):
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass  # Still waitpid: an exited child may need reaping.
        except BaseException as error:
            record(f'signal {sig}', error)
    def reap_for(seconds):
        deadline = time.monotonic() + seconds
        while True:
            try:
                if os.waitpid(pid, os.WNOHANG)[0] == pid:
                    return True
            except ChildProcessError:
                return True  # Already reaped, not a reason to skip fd cleanup.
            except BaseException as error:
                record('waitpid', error)
                return False
            if time.monotonic() >= deadline:
                return False
            time.sleep(.01)
    try:
        if original_tty is not None:
            try:
                termios.tcsetattr(fd, termios.TCSANOW, original_tty)
            except BaseException as error:
                record('restore terminal', error)
        signal_child(signal.SIGHUP)
        if not reap_for(2):
            signal_child(signal.SIGKILL)
            if not reap_for(2):
                errors.append('child was not reaped before bounded cleanup deadline')
    except BaseException as error:
        record('cleanup', error)
        # Even an unexpected clock/sleep failure must attempt termination/reap.
        signal_child(signal.SIGKILL)
        try:
            os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            pass
        except BaseException as wait_error:
            record('final waitpid', wait_error)
    finally:
        try:
            os.close(fd)
        except BaseException as error:
            record('close PTY', error)
    return errors


def session(integrated, commands):
    with tempfile.TemporaryDirectory(prefix='anvil-osc-') as temp:
        root = Path(temp)
        rc = root / 'rc'
        rc.write_text("set +o history\nPS1='F13READY> '\nPS2='F13CONT> '\n" +
                      (f'source "{INTEGRATION}"\n' if integrated else ''))
        pid, fd = pty.fork()
        if pid == 0:
            try:
                os.execve('/bin/bash', ['bash', '--noprofile', '--rcfile', str(rc), '-i'],
                          dict(PATH='/usr/bin:/bin', HOME=temp, LC_ALL='C.UTF-8',
                               TERM='xterm-256color', INPUTRC='/dev/null', HISTFILE='/dev/null'))
            except BaseException:
                os._exit(127)
        original_tty = None
        original_error = None
        data = bytearray()
        def prompt():
            end = time.monotonic() + 5
            marker = b'F13READY> ' + (b'\x1b]133;B\x07' if integrated else b'')
            while time.monotonic() < end:
                if select.select([fd], [], [], .02)[0]:
                    data.extend(os.read(fd, 65536))
                if marker in data:
                    result = bytes(data)
                    data.clear()
                    return result
            raise AssertionError(f'owned prompt deadline: {bytes(data)!r}')
        try:
            original_tty = termios.tcgetattr(fd)
            outputs = [prompt()]
            for command in commands:
                os.write(fd, command.encode() + b'\n')
                outputs.append(prompt())
            files = {p.name: p.read_bytes() for p in root.iterdir() if p.name != 'rc'}
            return outputs, files
        except BaseException as error:
            original_error = error
            raise
        finally:
            errors = cleanup_owned(pid, fd, original_tty)
            if errors:
                details = 'Owned PTY cleanup failed: ' + '; '.join(errors)
                if original_error is not None:
                    original_error.add_note(details)
                else:
                    raise RuntimeError(details)


class Cleanup(unittest.TestCase):
    def test_restore_failure_still_reaps_and_closes(self):
        with mock.patch.object(termios, 'tcsetattr', side_effect=OSError('restore')), \
             mock.patch.object(os, 'kill') as kill, \
             mock.patch.object(os, 'waitpid', return_value=(123, 0)) as wait, \
             mock.patch.object(os, 'close') as close:
            errors = cleanup_owned(123, 77, [])
        self.assertIn('restore terminal', errors[0])
        kill.assert_called_once_with(123, signal.SIGHUP)
        wait.assert_called_once_with(123, os.WNOHANG)
        close.assert_called_once_with(77)

    def test_kill_exit_and_reap_races_close_fd(self):
        for wait_result in [(123, 0), ChildProcessError()]:
            with mock.patch.object(os, 'kill', side_effect=ProcessLookupError()), \
                 mock.patch.object(os, 'waitpid', side_effect=[wait_result]), \
                 mock.patch.object(os, 'close') as close:
                self.assertEqual(cleanup_owned(123, 77, None), [])
            close.assert_called_once_with(77)

    def test_escalation_is_bounded_and_always_closes(self):
        with mock.patch.object(os, 'kill') as kill, \
             mock.patch.object(os, 'waitpid', return_value=(0, 0)), \
             mock.patch.object(time, 'monotonic', side_effect=range(20)), \
             mock.patch.object(time, 'sleep'), mock.patch.object(os, 'close') as close:
            errors = cleanup_owned(123, 77, None)
        self.assertEqual(kill.call_args_list,
                         [mock.call(123, signal.SIGHUP), mock.call(123, signal.SIGKILL)])
        self.assertIn('not reaped', errors[-1])
        close.assert_called_once_with(77)

    def test_initial_read_failure_preserves_original_and_cleans(self):
        original = OSError('initial terminal read')
        real_close = os.close
        with mock.patch.object(pty, 'fork', return_value=(123, 77)), \
             mock.patch.object(termios, 'tcgetattr', side_effect=original), \
             mock.patch.object(os, 'kill'), \
             mock.patch.object(os, 'waitpid', return_value=(123, 0)), \
             mock.patch.object(os, 'close', side_effect=lambda fd: None if fd == 77 else real_close(fd)) as close:
            with self.assertRaises(OSError) as caught:
                session(True, [])
        self.assertIs(caught.exception, original)
        self.assertEqual(close.call_args_list.count(mock.call(77)), 1)

    def test_cleanup_failure_prevents_success_return(self):
        real_close = os.close
        with mock.patch.object(pty, 'fork', return_value=(123, 77)), \
             mock.patch.object(termios, 'tcgetattr', return_value=[]), \
             mock.patch.object(select, 'select', return_value=([77], [], [])), \
             mock.patch.object(os, 'read', return_value=b'F13READY> \x1b]133;B\x07'), \
             mock.patch.object(termios, 'tcsetattr', side_effect=OSError('restore')), \
             mock.patch.object(os, 'kill'), \
             mock.patch.object(os, 'waitpid', return_value=(123, 0)), \
             mock.patch.object(os, 'close', side_effect=lambda fd: None if fd == 77 else real_close(fd)) as close:
            with self.assertRaisesRegex(RuntimeError, 'restore terminal'):
                session(True, [])
        self.assertEqual(close.call_args_list.count(mock.call(77)), 1)

    def test_original_read_error_retained_with_cleanup_error(self):
        original = ValueError('original PTY read')
        real_close = os.close
        with mock.patch.object(pty, 'fork', return_value=(123, 77)), \
             mock.patch.object(termios, 'tcgetattr', return_value=[]), \
             mock.patch.object(select, 'select', side_effect=original), \
             mock.patch.object(termios, 'tcsetattr', side_effect=OSError('restore')), \
             mock.patch.object(os, 'kill', side_effect=ProcessLookupError()), \
             mock.patch.object(os, 'waitpid', side_effect=ChildProcessError()), \
             mock.patch.object(os, 'close', side_effect=lambda fd: None if fd == 77 else real_close(fd)) as close:
            with self.assertRaises(ValueError) as caught:
                session(True, [])
        self.assertIs(caught.exception, original)
        self.assertIn('restore terminal', original.__notes__[0])
        self.assertEqual(close.call_args_list.count(mock.call(77)), 1)


class Transport(unittest.TestCase):
    def test_redirected_user_bytes_match_without_integration(self):
        commands = [
            "for x in a b; do printf '%s' \"$x\"; done > \"$HOME/compound\"",
            "(printf '%s' '中 subshell') > \"$HOME/subshell\"",
            "{ printf '%s' background; } > \"$HOME/background\" & wait",
            "printf '%s' pipeline | cat > \"$HOME/pipeline\"",
            "value=$(printf '%s' substitution); printf '%s' \"$value\" > \"$HOME/substitution\"",
            "{ printf '%s' output; printf '%s' error >&2; } > \"$HOME/stdout\" 2> \"$HOME/stderr\"",
        ]
        expected = dict(compound=b'ab', subshell='中 subshell'.encode(), background=b'background',
                        pipeline=b'pipeline', substitution=b'substitution', stdout=b'output', stderr=b'error')
        records = []
        for enabled in [False, True]:
            outputs, files = session(enabled, commands)
            self.assertEqual(files, expected)
            records.append(dict(integrated=enabled, files={k:v.hex() for k,v in files.items()},
                                pty_hex=[o.hex() for o in outputs]))
            if enabled:
                # An isolated subshell has no lifecycle events in the existing
                # integration. Preserve that bound; transport does not invent them.
                for output, count in zip(outputs[1:], [1, 0, 1, 1, 1, 1]):
                    self.assertEqual(len(re.findall(rb'\x1b\]133;C;', output)), count, output)
                    self.assertEqual(len(re.findall(rb'\x1b\]133;D;', output)), count, output)
                    if count:
                        self.assertLess(output.index(b'\x1b]133;D;'), output.index(b'F13READY> '))
                    self.assertTrue(output.endswith(b'F13READY> \x1b]133;B\x07'))
        print(json.dumps(records, ensure_ascii=False))

    def test_functrace_background_tostop_keeps_user_job_running(self):
        commands = [
            'saved_tty=$(stty -g); set -T; stty tostop',
            '{ printf owned; } > "$HOME/background" &',
            'for i in {1..50}; do stopped=$(jobs -s -p); '
            '[[ -n $stopped || -s "$HOME/background" ]] && break; sleep .02; done; '
            'printf \'%s\' "$stopped" > "$HOME/stopped"; '
            'if [[ -n $stopped ]]; then kill -KILL %1; fi; '
            'wait 2>/dev/null; printf \'%s\' "$?" > "$HOME/status"; '
            'stty "$saved_tty"; stty -g > "$HOME/restored"; '
            'printf \'%s\n\' "$saved_tty" > "$HOME/original"',
        ]
        outputs, files = session(True, commands)
        self.assertEqual(files['background'], b'owned')
        self.assertEqual(files['stopped'], b'')
        self.assertEqual(files['status'], b'0')
        self.assertEqual(files['restored'], files['original'])
        # The asynchronous compound itself has no parent DEBUG event. Its child
        # must not invent a second execution. The surrounding parent lists do.
        counts = [len(re.findall(rb'\x1b\]133;C;', output)) for output in outputs]
        self.assertEqual(counts, [0, 1, 0, 1])

    def test_no_persistent_descriptor_in_child(self):
        inventory = """import json, os
result = {}
for name in os.listdir('/proc/self/fd'):
    fd = int(name)
    if fd <= 2:
        continue
    try:
        target = os.readlink('/proc/self/fd/' + name)
        stat = os.fstat(fd)
    except FileNotFoundError:
        continue  # The temporary listdir descriptor has already closed.
    result[name] = [target, stat.st_dev, stat.st_ino, stat.st_mode]
print(json.dumps(result, sort_keys=True))
"""
        def capture(name):
            return 'python3 -c ' + shlex.quote(inventory) + f' > "$HOME/{name}"'
        # The runner may already own inherited descriptors. Compare exact
        # inventories in the SAME Bash before/after sourcing, rather than
        # incorrectly assuming every descriptor above stderr is our leak.
        commands = [capture('before'), f'source "{INTEGRATION}"', capture('after'),
                    'exec {injected_fd}>"$HOME/injected"', capture('positive'),
                    'exec {injected_fd}>&-', capture('restored')]
        # Deliberately inherit an owned descriptor to reproduce the CI premise.
        with tempfile.TemporaryFile() as inherited:
            os.set_inheritable(inherited.fileno(), True)
            _, files = session(False, commands)
            snapshots = {name: json.loads(files[name]) for name in
                         ['before', 'after', 'positive', 'restored']}
            self.assertIn(str(inherited.fileno()), snapshots['before'])
        self.assertEqual(snapshots['after'], snapshots['before'])
        self.assertEqual(snapshots['restored'], snapshots['before'])
        self.assertNotEqual(snapshots['positive'], snapshots['before'])
        added = set(snapshots['positive']) - set(snapshots['before'])
        self.assertEqual(len(added), 1)
        self.assertTrue(snapshots['positive'][added.pop()][0].endswith('/injected'))
        print(json.dumps({'descriptor_inventories': snapshots}, sort_keys=True))

    def test_no_controlling_tty_fails_closed(self):
        # A new session with only pipes has no controlling terminal. No fallback
        # to stdout/stderr is permitted, and the application status is preserved.
        code = f'source "{INTEGRATION}"; set -e; shopt -s extdebug; __anvil_osc "133;C;owned" || exit 73; printf ordinary; exit 7'
        result = subprocess.run(['/bin/bash', '--noprofile', '--norc', '-c', code],
                                start_new_session=True, capture_output=True, timeout=5,
                                env=dict(PATH='/usr/bin:/bin', LC_ALL='C.UTF-8'))
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout, b'ordinary')
        self.assertEqual(result.stderr, b'')


if __name__ == '__main__':
    unittest.main()
