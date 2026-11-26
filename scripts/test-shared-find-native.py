#!/usr/bin/env python3
"""Exercise Anvil's Find entry points on one native shared transcript."""
import argparse
import json
import os
from pathlib import Path
import tempfile

from native import Atspi, accessible_text, application_nodes, first_named, start_accessibility, wait_for
from native_jsh import application, xdotool


def run(binary, root, mode="unified", legacy_environment=False):
    fixture = root / 'shell'
    fixture.write_text(
        '#!/usr/bin/python3\n'
        'import os\n'
        'from pathlib import Path\n'
        "Path(__file__).with_name('leader').write_text(str(os.getpid()))\n"
        "print('shared-find-probe archived', flush=True)\n"
        'for row in range(90):\n'
        '    print(row)\n'
        "print('shared-find-probe live', flush=True)\n"
        'input()\n'
    )
    fixture.chmod(0o700)
    config = root / 'anvil.toml'
    config.write_text(f'shell = {json.dumps(str(fixture))}\nterminal_mode = {json.dumps(mode)}\n')
    config.chmod(0o600)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(('JSH_', 'JTERM_SHARED_'))
           and key not in ('ANVIL_SHELL', 'ANVIL_HISTORY_PATH', 'BASH_ENV')}
    for key in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME', 'XDG_RUNTIME_DIR'):
        directory = root / key
        directory.mkdir(mode=0o700)
        env[key] = str(directory)
    env.update(GDK_BACKEND='x11', GSK_RENDERER='cairo', GTK_A11Y='atspi')
    if legacy_environment:
        env.update(JTERM_SHARED_RUNTIME='0', JTERM_SHARED_ACTIVE_TERMINAL='0', JTERM_SHARED_TRANSCRIPT='0')

    with application(binary, config, env, root / 'anvil.log', restore=False) as process:
        pid = process.pid

        def screen_view():
            return next((node for node in application_nodes(pid)
                         if (node.get_description() == 'Shared active terminal' or node.get_name() == 'Shared terminal screen')
                         and node.get_state_set().contains(Atspi.StateType.SHOWING)), None)

        def screen_text():
            return accessible_text(screen_view())

        def find_entry(state=Atspi.StateType.FOCUSED):
            return first_named(pid, 'Find in terminal output', state)

        def result(marker, count):
            screen = screen_view()
            displayed = accessible_text(screen)
            if marker not in displayed or not first_named(pid, count) or not find_entry():
                return False
            # Navigation must paint the current occurrence, even when the GTK
            # history viewport arrives after the result count changes.
            offset = displayed.index(marker)
            next_offset = offset + len('shared-find-probe')
            text = screen.get_text_iface()
            match_style, start, end = Atspi.Text.get_attribute_run(text, offset + 1, False)
            neighbor_style, _, _ = Atspi.Text.get_attribute_run(text, next_offset + 1, False)
            return (start, end) == (offset, next_offset) and (
                match_style.get('bg-color') != neighbor_style.get('bg-color')
            )

        wait_for('the live transcript', lambda: 'shared-find-probe live' in screen_text())
        assert 'shared-find-probe archived' not in screen_text()
        leader = int((root / 'leader').read_text())
        tty = Path(f'/proc/{leader}/fd/0').readlink()
        assert str(tty).startswith('/dev/pts/')
        if mode == 'vte':
            xdotool('key', '--clearmodifiers', 'ctrl+shift+f')
        else:
            button = wait_for('Unified Find toolbar button', lambda: first_named(pid, 'Find text in this terminal'))
            assert Atspi.Action.do_action(button.get_action_iface(), 0)
        wait_for('application Find from the toolbar', find_entry)
        xdotool('type', '--clearmodifiers', '--delay', '2', 'shared-find-probe')
        wait_for('first hit in the live viewport', lambda: result('shared-find-probe live', '1 of 2'))
        xdotool('key', '--clearmodifiers', 'Return')
        wait_for('next hit on the mounted archived transcript', lambda: result('shared-find-probe archived', '2 of 2'))
        xdotool('key', '--clearmodifiers', 'shift+Return')
        wait_for('previous hit in the live viewport', lambda: result('shared-find-probe live', '1 of 2'))
        xdotool('key', '--clearmodifiers', 'Escape')
        wait_for('Find bar closes', lambda: not find_entry(Atspi.StateType.SHOWING))
        xdotool('key', '--clearmodifiers', 'ctrl+shift+f')
        wait_for('application Find from the workspace shortcut', find_entry)
        assert Path(f'/proc/{leader}/fd/0').readlink() == tty
    print(f'Anvil native shared Find passed mode={mode}, legacy_environment={legacy_environment}, live and archived hits, Next/Previous, focus and same PTY checks.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--mode', choices=('vte', 'unified'), default='unified')
    parser.add_argument('--legacy-environment', action='store_true')
    start_accessibility()
    with tempfile.TemporaryDirectory(prefix='anvil-shared-find-') as directory:
        args = parser.parse_args()
        run(args.binary.resolve(strict=True), Path(directory), args.mode, args.legacy_environment)
