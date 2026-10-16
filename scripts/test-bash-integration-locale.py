"""Owned real Bash PTY and exact-source cwd encoding/locale regressions."""
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import tempfile
import time
import unittest
from urllib.parse import quote_from_bytes

INTEGRATION = Path(__file__).resolve().parent / 'shell-integration/anvil.bash'


def clean_env(home):
    return dict(PATH='/usr/bin:/bin', HOME=str(home), LANG='C.UTF-8',
                TERM='xterm-256color', INPUTRC='/dev/null', HISTFILE='/dev/null')


class BashLocale(unittest.TestCase):
    def test_cwd_encoding_preserves_locale_value_export_and_unset_on_return(self):
        source = INTEGRATION.read_text()
        start = source.index('__anvil_report_cwd() {')
        function = source[start:source.index('\n}', start) + 2]
        with tempfile.TemporaryDirectory(prefix='anvil-locale-') as temp:
            cwd = Path(temp) / '中文 é space % #\nline'
            cwd.mkdir()
            for setup in ('unset LC_ALL', 'LC_ALL=C.UTF-8; export -n LC_ALL',
                          'export LC_ALL=C.UTF-8'):
                for status in (0, 7):
                    with self.subTest(setup=setup, return_status=status):
                        # Stub only the OSC writer's return status. The cwd
                        # function itself is copied byte-for-byte from source.
                        script = function + f'''
__anvil_osc() {{ printf '\\033]%s\\007' "$1"; return {status}; }}
{setup}
before=$(declare -p LC_ALL 2>/dev/null || :)
__anvil_report_cwd
result=$?
after=$(declare -p LC_ALL 2>/dev/null || :)
[[ "$before" == "$after" ]] || exit 73
printf '\\nstatus=%s\\n' "$result"
'''
                        env = clean_env(temp)
                        env['HOSTNAME'] = 'anvil-locale-owned'
                        result = subprocess.run(['/bin/bash', '--noprofile', '--norc', '-c', script],
                                                cwd=cwd, env=env, capture_output=True, timeout=5)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        encoded = quote_from_bytes(os.fsencode(cwd), safe='/-._~')
                        expected = f'\x1b]7;file://anvil-locale-owned{encoded}\x07\nstatus={status}\n'.encode()
                        self.assertEqual(result.stdout, expected)

    def test_real_readline_unicode_backspace_after_production_prompt(self):
        observations = []
        for integrated in (False, True):
            with tempfile.TemporaryDirectory(prefix='anvil-readline-') as temp:
                root = Path(temp)
                rc = root / 'rc'
                rc.write_text("set +o history\nset -o emacs\n"
                              "bind 'set enable-bracketed-paste off'\nPS1='ANVIL_LOCALEREADY> '\n"
                              "printf '%s' \"$LC_ALL\" > \"$HOME/before\"\n" +
                              (f'source "{INTEGRATION}"\n' if integrated else '') +
                              "printf '%s' \"$LC_ALL\" > \"$HOME/after-source\"\n")
                pid, fd = pty.fork()
                if pid == 0:
                    env = clean_env(temp)
                    env['LC_ALL'] = 'C.UTF-8'
                    os.execve('/bin/bash', ['bash', '--noprofile', '--rcfile', str(rc), '-i'], env)
                data = bytearray()

                def wait_for(marker):
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        if select.select([fd], [], [], .02)[0]:
                            data.extend(os.read(fd, 65536))
                        if marker in data:
                            data.clear()
                            return
                    self.fail(f'owned Bash deadline waiting for {marker!r}; PTY={bytes(data)!r}')

                try:
                    wait_for(b'ANVIL_LOCALEREADY> ')
                    os.write(fd, b'printf \'%s\' "$LC_ALL" > "$HOME/after-prompt"\n')
                    wait_for(b'ANVIL_LOCALEREADY> ')
                    # Real readline deletion, no bind-x observer and no locale
                    # reassignment after sourcing the production integration.
                    payload = "printf '%s' 中文X".encode() + b'\x7f\x7f > "$HOME/edit"\n'
                    os.write(fd, payload)
                    wait_for(b'ANVIL_LOCALEREADY> ')
                    observation = dict(integrated=integrated,
                        before=(root / 'before').read_text(),
                        after_source=(root / 'after-source').read_text(),
                        after_prompt=(root / 'after-prompt').read_text(),
                        input_hex=payload.hex(), output_hex=(root / 'edit').read_bytes().hex())
                    observations.append(observation)
                    for key in ('before', 'after_source', 'after_prompt'):
                        self.assertEqual(observation[key], 'C.UTF-8', observation)
                    self.assertEqual((root / 'edit').read_bytes(), '中'.encode(), observation)
                finally:
                    try:
                        os.kill(pid, signal.SIGHUP)
                    except ProcessLookupError:
                        pass
                    deadline = time.monotonic() + 2
                    while os.waitpid(pid, os.WNOHANG) == (0, 0):
                        if time.monotonic() >= deadline:
                            os.kill(pid, signal.SIGKILL)
                            os.waitpid(pid, 0)
                            break
                        time.sleep(.01)
                    os.close(fd)
        self.assertEqual(observations[0]['input_hex'], observations[1]['input_hex'])
        self.assertEqual(observations[0]['output_hex'], observations[1]['output_hex'])
        print(json.dumps(observations, ensure_ascii=False))


if __name__ == '__main__':
    unittest.main()
