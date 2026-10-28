"""Owned real Bash PTYs: optional fresh-history metadata, without observer traps."""
import os
from pathlib import Path
import pty
import re
import select
import runpy
import tempfile
import time
import unittest
from urllib.parse import unquote_to_bytes

INTEGRATION = Path(__file__).resolve().parent / 'shell-integration/anvil.bash'
# Reuse the independently tested bounded cleanup, without importing bytecode.
cleanup_owned = runpy.run_path(str(Path(__file__).with_name('test-bash-osc-transport.py')))['cleanup_owned']


def observe(setup, commands, values=None):
    with tempfile.TemporaryDirectory(prefix='anvil-command-') as temp:
        root = Path(temp)
        (root / 'import').write_text('external owned fixture entry\n')
        rc = root / 'rc'
        rc.write_text("set -o history\nshopt -s cmdhist\n"
                      "unset HISTCONTROL HISTIGNORE HISTTIMEFORMAT\n"
                      "PS1='F13READY> '\nPS2='F13CONT> '\n" + setup + '\n' +
                      f'source "{INTEGRATION}"\n')
        pid, fd = pty.fork()
        if pid == 0:
            try:
                os.execve('/bin/bash', ['bash', '--noprofile', '--rcfile', str(rc), '-i'],
                          dict(PATH='/usr/bin:/bin', HOME=temp, LC_ALL='C.UTF-8',
                               TERM='xterm-256color', INPUTRC='/dev/null', HISTFILE='/dev/null'))
            except BaseException:
                os._exit(127)
        data = bytearray()
        original_error = None
        def prompt():
            end = time.monotonic() + 5
            while time.monotonic() < end:
                if select.select([fd], [], [], .02)[0]:
                    data.extend(os.read(fd, 65536))
                if b'F13READY> ' in data and b'\x1b]133;B\x07' in data:
                    result = bytes(data)
                    data.clear()
                    return result
            raise AssertionError(f'owned prompt deadline: {bytes(data)!r}')
        try:
            os.set_blocking(fd, False)
            prompt()
            result = []
            packet_ids = []
            for command in commands:
                pending = (command if isinstance(command, bytes) else command.encode()) + b'\n'
                deadline = time.monotonic() + 10
                while pending:
                    if time.monotonic() >= deadline:
                        raise AssertionError('owned input delivery deadline')
                    readable, writable, _ = select.select([fd], [fd], [], .02)
                    if readable:
                        data.extend(os.read(fd, 65536))
                    if writable:
                        pending = pending[os.write(fd, pending):]
                output = prompt()
                packets = re.findall(rb'\x1b\]133;C;([^\x07]*)\x07', output)
                fields = [dict(field.split(b'=', 1) for field in packet.split(b';'))
                          for packet in packets]
                assert len(fields) <= 1, fields
                ended = re.findall(rb'\x1b\]133;D;[^;]*;id=([^\x07]*)\x07', output)
                assert ended == [f[b'id'] for f in fields], (fields, ended)
                if fields and packet_ids:
                    assert fields[0][b'id'].rsplit(b'-', 1)[0] == packet_ids[0].rsplit(b'-', 1)[0]
                for fields_item in fields:
                    packet_ids.append(fields_item[b'id'])
                    assert fields_item[b'id'].endswith(f'-{len(packet_ids)}'.encode()), packet_ids
                result.append([unquote_to_bytes(f[b'cmdline_url'])
                               if b'cmdline_url' in f else None for f in fields])
            if values is not None:
                for name in values:
                    values[name] = (root / name).read_bytes()
            return result
        except BaseException as error:
            original_error = error
            raise
        finally:
            errors = cleanup_owned(pid, fd, None)
            if errors:
                details = 'Owned metadata PTY cleanup failed: ' + '; '.join(errors)
                if original_error is not None:
                    original_error.add_note(details)
                else:
                    raise RuntimeError(details)


class Metadata(unittest.TestCase):
    def test_fresh_full_logical_commands(self):
        commands = ["printf '%s' '中文 ＡＢＣ' > \"$HOME/value\"",
                    "printf '%s' pipe | cat > \"$HOME/value\"",
                    "printf '%s' 'line1\nline2' > \"$HOME/value\"",
                    "printf '%s' one\\\ntwo > \"$HOME/value\"",
                    ':; :; :', "printf '%s' '%;中' > \"$HOME/value\""]
        expected = [c.encode() for c in commands]
        expected[3] = commands[3].replace('\\\n', '').encode()
        self.assertEqual(observe('', commands), [[c] for c in expected])

    def test_standard_privacy_policies_and_combinations(self):
        commands = [': first', ': first', ' : hidden', ': last', ': first']
        for policy, skipped in [('ignorespace', {2}), ('ignoredups', {1}),
                                ('ignoreboth', {1, 2}),
                                ('ignorespace:ignoredups', {1, 2}),
                                (':ignoreboth::ignoredups:', {1, 2})]:
            with self.subTest(policy=policy):
                self.assertEqual(observe('HISTCONTROL=' + policy, commands),
                                 [[None if i in skipped else c.encode()]
                                  for i, c in enumerate(commands)])

    def test_privacy_policy_hooks_rollover_and_multiline(self):
        commands = [': first', ': first', " : 'hidden\nsecond'", '', '  ', ': last']
        expected = [[b': first'], [None], [None], [], [], [b': last']]
        for setup in ['HISTCONTROL=ignoreboth', 'HISTCONTROL=ignoreboth; HISTSIZE=1',
                      "HISTCONTROL=ignoreboth; PROMPT_COMMAND='history -c'"]:
            with self.subTest(setup=setup):
                actual = observe(setup, commands)
                # Clearing history deliberately makes the repeated line fresh.
                wanted = expected.copy()
                if 'history -c' in setup:
                    wanted[1] = [b': first']
                self.assertEqual(actual, wanted)

    def test_ignoreboth_seed_import_expansion_and_settings(self):
        self.assertEqual(observe("HISTCONTROL=ignoreboth",
                                 ["history -s ': seeded'", ": seeded", ": fresh"]),
                         [[b"history -s ': seeded'"], [None], [b": fresh"]])
        setup = "HISTCONTROL=ignoreboth; PROMPT_COMMAND='history -r \"$HOME/import\"'"
        self.assertEqual(observe(setup, [': fresh', ' : hidden', ': last']),
                         [[b': fresh'], [None], [b': last']])
        self.assertEqual(observe('HISTCONTROL=ignoreboth', [': expanded', '!!', ': last']),
                         [[b': expanded'], [None], [b': last']])
        for policy in ['ignorespace', 'ignoredups', 'ignoreboth']:
            setup = ('HISTCONTROL=' + policy + '; HISTSIZE=7; HISTFILESIZE=9; '
                     'declare -p HISTCONTROL HISTIGNORE HISTSIZE HISTFILESIZE '
                     '> "$HOME/before" 2>/dev/null')
            command = ('declare -p HISTCONTROL HISTIGNORE HISTSIZE HISTFILESIZE '
                       '> "$HOME/after" 2>/dev/null')
            values = {'before': None, 'after': None}
            observe(setup, [command], values)
            self.assertEqual(values['before'], values['after'])

    def test_pipefail_large_history_is_omitted_and_recovers(self):
        for setup in ['set -o pipefail', 'set -e -o pipefail']:
            commands = [': ' + 'x' * 100000, ': recovered']
            self.assertEqual(observe(setup, commands), [[None], [b': recovered']])

    def test_uncertain_policies_omit_metadata(self):
        command = "printf '%s' 'owned' > \"$HOME/value\""
        for setup in ['set +o history', 'HISTCONTROL=erasedups',
                      'HISTCONTROL=ignoreboth:erasedups', 'HISTCONTROL=unknown',
                      "HISTIGNORE='printf*'", 'shopt -u cmdhist',
                      "trap ':' USR1"]:
            with self.subTest(setup=setup):
                self.assertEqual(observe(setup, [command, ' ' + command]), [[None], [None]])

    def test_prompt_and_history_changes_rebaseline(self):
        commands = ['history -c', ':', 'history -r "$HOME/import"', ':',
                    "history -s 'old owned entry'", ':']
        self.assertEqual(observe('', commands), [[c.encode()] for c in commands])
        self.assertEqual(observe("PROMPT_COMMAND='history -s \"prompt owned entry\"'", [':', ':']),
                         [[b':'], [b':']])

    def test_replaced_debug_does_not_reuse_command(self):
        self.assertEqual(observe('', ["trap ':' DEBUG", ':']), [[b"trap ':' DEBUG"], []])


    def test_policy_transitions_never_capture_ignored_commands(self):
        commands = ["HISTCONTROL=ignorespace", " : private_owned", 'unset HISTCONTROL', ':']
        self.assertEqual(observe('', commands), [[commands[0].encode()], [None], [b'unset HISTCONTROL'], [b':']])
        self.assertEqual(observe('HISTSIZE=0', [':', ':']), [[None], [None]])

    def test_trailing_quoted_newline_and_lithist(self):
        command = "printf '%s' 'line\n' > \"$HOME/value\""
        self.assertEqual(observe('shopt -s lithist', [command]), [[command.encode()]])
        command = 'for x in a b; do\nprintf \'%s\' "$x" > "$HOME/value"\ndone'
        self.assertEqual(observe('shopt -s lithist', [command]), [[command.encode()]])

    def test_locale_and_timestamp_scope_preserved(self):
        for setup in ['unset LC_ALL', 'LC_ALL=C.UTF-8; export -n LC_ALL',
                      'export LC_ALL=C.UTF-8']:
            setup += "\nexport HISTTIMEFORMAT='owned %Y'\n" + \
                     '(declare -p LC_ALL || :; declare -p HISTTIMEFORMAT) > "$HOME/before"'
            command = '(declare -p LC_ALL || :; declare -p HISTTIMEFORMAT) > "$HOME/after"'
            values = {'before': None, 'after': None}
            observe(setup, [command], values)
            self.assertEqual(values['before'], values['after'])

    def test_readonly_locale_and_timestamp_fail_closed(self):
        for setup in ['readonly LC_ALL', "readonly HISTTIMEFORMAT='owned '"]:
            self.assertEqual(observe(setup, [':', ':']), [[None], [None]])

    def test_compound_redirect_metadata_and_user_bytes(self):
        command = "for x in a b; do printf '%s' \"$x\"; done > \"$HOME/value\""
        values = {'value': None}
        self.assertEqual(observe('', [command], values), [[command.encode()]])
        self.assertEqual(values['value'], b'ab')

    def test_regex_match_state_is_not_clobbered(self):
        commands = ['[[ abc =~ (b) ]]', 'declare -p BASH_REMATCH > "$HOME/value"']
        values = {'value': None}
        self.assertEqual(observe('', commands, values), [[c.encode()] for c in commands])
        self.assertEqual(values['value'], b'declare -a BASH_REMATCH=([0]="b" [1]="b")\n')

    def test_command_exit_status_is_preserved(self):
        commands = ['false', 'printf \'%s\' "$?" > "$HOME/value"']
        values = {'value': None}
        self.assertEqual(observe('', commands, values), [[c.encode()] for c in commands])
        self.assertEqual(values['value'], b'1')

    def test_errexit_fallback_does_not_abort_shell(self):
        self.assertEqual(observe('set -e; set +o history', [':', ':']), [[None], [None]])

    def test_real_readline_edits_and_accepted_bytes(self):
        prefix = "printf '%s' 中文".encode()
        suffix = b' > "$HOME/value"'
        values = {'value': None}
        observed = observe('', [prefix + '界X'.encode() + b'\x7f\x7f' + suffix], values)
        self.assertEqual(observed, [[prefix + suffix]])
        self.assertEqual(values['value'], '中文'.encode())

    def test_byte_cap_and_multibyte_percent_encoding(self):
        for size in [16384, 16385]:
            # ':' and spaces are two bytes; each CJK codepoint is three.
            body = '中' * ((size - 2) // 3) + 'a' * ((size - 2) % 3)
            command = ': ' + body
            self.assertEqual(len(command.encode()), size)
            self.assertEqual(observe('', [command]),
                             [[command.encode() if size == 16384 else None]])

    def test_functrace_and_prompt_expansion(self):
        self.assertEqual(observe('set -T', [':; :', ':']), [[b':; :'], [b':']])
        self.assertEqual(observe("PS1='$(printf F13READY)\x3e '", [':', ':']), [[b':'], [b':']])

    def test_prompt_arrays_and_empty_inputs_preserve_freshness(self):
        self.assertEqual(observe("PROMPT_COMMAND=('history -s \"owned hook\"' ':')", [':', ':']),
                         [[b':'], [b':']])
        self.assertEqual(observe("PROMPT_COMMAND='history -c'", [':', ':']), [[b':'], [b':']])
        self.assertEqual(observe('', [':', '', '   ', ':']), [[b':'], [], [], [b':']])

    def test_mutating_debug_cannot_attribute_injected_history(self):
        replacement = "trap 'history -s \"stale owned\"; __anvil_preexec' DEBUG"
        self.assertEqual(observe('', [replacement, ':', ':']),
                         [[replacement.encode()], [None], [None]])

    def test_hook_interference_falls_back(self):
        # A changed hook that calls the old preexec must not reuse its cached
        # trap evidence after the replacement command consumed it.
        commands = ["trap '__anvil_preexec' DEBUG", ':', ':']
        self.assertEqual(observe('', commands), [[commands[0].encode()], [None], [None]])
        self.assertEqual(observe("PROMPT_COMMAND='trap \"__anvil_preexec\" DEBUG'", [':']), [[None]])


if __name__ == '__main__':
    unittest.main()
