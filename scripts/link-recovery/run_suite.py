#!/usr/bin/env python3
"""Hosted execution only. No production desktop settings or external service."""
import json, os, pathlib, signal, subprocess, sys, time
from bus_startup import read_bus_address, stop_children
from extract import verify
P = pathlib.Path
if len(sys.argv) > 1 and sys.argv[1] == '--record':
    assert os.environ.get('ANVIL_HOSTED_FIXTURE') == '1'
    assert len(sys.argv) == 3
    (P(os.environ['ANVIL_RECORD_DIR'])/'handler-uri.bin').write_bytes(os.fsencode(sys.argv[2]))
    raise SystemExit(0)
assert os.environ.get('ANVIL_HOSTED_FIXTURE') == '1'
assert set(os.listdir('/sys/class/net')) <= {'lo'}, 'network namespace isolation not established'
assert not P('/.flatpak-info').exists(), 'use non-Flatpak hosted runner'
fixture = P(__file__).resolve().parent
verify()
probe = P(sys.argv[1]).resolve(strict=True)
assert probe.is_file()
python = '/usr/bin/python3'
# Explicit private evidence destination is archived even when the suite fails.
root = P(sys.argv[2]).resolve()
root.mkdir(mode=0o700, parents=True, exist_ok=False)
status = {'passed': False, 'completed_cases': [], 'current_case': None,
          'native_boundary_only': True, 'gtk_ui_acceptance': False, 'cleanup_cases': {}}

def interrupted(signum, frame):
    raise RuntimeError('owned boundary interrupted')

signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)

def wait_file(path, process):
    deadline = time.monotonic() + 5
    while not path.exists():
        assert process.poll() is None, 'owned service exited before ready'
        assert time.monotonic() < deadline, 'owned service readiness timeout'
        time.sleep(.01)

def run_case(case):
    d = root / case
    d.mkdir(mode=0o700)
    if len(os.fsencode(d/'bus')) >= 100:
        raise RuntimeError('private bus path too long')
    for name in ['home', 'config', 'data', 'cache', 'runtime', 'empty-data', 'empty-config']:
        (d / name).mkdir(mode=0o700)
    (d / 'data/applications').mkdir()
    # URI bytes deliberately stress percent escapes, case, port zeros, query and fragment.
    uri = b'https://Example.invalid:00443/private%2fmarker?token=synthetic-secret%2B#fragment'
    (d / 'expected.bin').write_bytes(uri)
    env = {'PATH':'/usr/bin:/bin', 'LANG':'C.UTF-8', 'ANVIL_HOSTED_FIXTURE':'1',
        'HOME':str(d/'home'), 'XDG_CONFIG_HOME':str(d/'config'), 'XDG_DATA_HOME':str(d/'data'),
        'XDG_CACHE_HOME':str(d/'cache'), 'XDG_RUNTIME_DIR':str(d/'runtime'),
        'XDG_CONFIG_DIRS':str(d/'empty-config'), 'XDG_DATA_DIRS':str(d/'empty-data'),
        'GIO_USE_VFS':'local', 'GIO_USE_PORTALS':'0' if case in ('missing','record','activation-denied') else '1',
        'GSETTINGS_BACKEND':'memory', 'XDG_CURRENT_DESKTOP':'AnvilFixture', 'ANVIL_RECORD_DIR':str(d)}
    # Explicit configuration, no standard_session_servicedirs or activation directories.
    config = d/'bus.conf'
    config.write_text(f'''<busconfig><type>session</type><listen>unix:path={d}/bus</listen>
<auth>EXTERNAL</auth><policy context="default"><allow own="*"/><allow send_destination="*"/>
<allow receive_sender="*"/></policy></busconfig>''')
    bus = subprocess.Popen(['/usr/bin/dbus-daemon','--nofork','--print-address=1','--config-file='+str(config)],
        env=env, cwd=d, stdout=subprocess.PIPE, stderr=(d/'bus.stderr').open('w'))
    portal = None
    try:
        address = read_bus_address(bus, d/'bus')
        env['DBUS_SESSION_BUS_ADDRESS'] = address
        if case in ('record', 'activation-denied'):
            # Exec argument quoting; fixed fixture path must be ordinary (no Exec metacharacters).
            assert all(c not in str(fixture) for c in ' "%\\`$')
            desktop = 'org.example.AnvilRecorder.desktop' if case == 'activation-denied' else 'anvil-recorder.desktop'
            (d/'data/applications'/desktop).write_text(f'''[Desktop Entry]
Type=Application
Name=Owned no-network recorder
Exec={python} {fixture}/run_suite.py --record %u
Terminal=false
NoDisplay=true
DBusActivatable={'true' if case == 'activation-denied' else 'false'}
MimeType=x-scheme-handler/http;x-scheme-handler/https;
''')
            (d/'config/mimeapps.list').write_text(f'''[Default Applications]
x-scheme-handler/http={desktop};
x-scheme-handler/https={desktop};
''')
            if case == 'activation-denied':
                portal = subprocess.Popen([python, str(fixture/'fake_handler.py')], cwd=d, env=env,
                    stdout=(d/'portal.stdout').open('w'), stderr=(d/'portal.stderr').open('w'))
                wait_file(d/'handler.ready', portal)
        elif case != 'missing':
            portal = subprocess.Popen([python, str(fixture/'fake_portal.py'),case], cwd=d, env=env,
                stdout=(d/'portal.stdout').open('w'), stderr=(d/'portal.stderr').open('w'))
            wait_file(d/'portal.ready',portal)
        with (d/'probe.stdout').open('w') as out, (d/'probe.stderr').open('w') as err:
            result = subprocess.run([str(probe), case], cwd=d, env=env, stdout=out, stderr=err, timeout=12)
        assert result.returncode == 0, f'{case}: probe failure; inspect private evidence'
        if case in ('record', 'activation-denied'):
            deadline = time.monotonic()+3
            while not (d/'handler-uri.bin').exists() and time.monotonic()<deadline: time.sleep(.01)
            assert (d/'handler-uri.bin').read_bytes() == uri
        elif case != 'missing':
            assert (d/'portal-uri.bin').read_bytes() == uri
            wait_file(d/'portal.responded', portal)
        for log in ['probe.stdout','probe.stderr','portal.stdout','portal.stderr']:
            if (d/log).exists():
                b = (d/log).read_bytes()
                assert b'synthetic-secret' not in b and b'private%2fmarker' not in b, 'target leaked into diagnostics'
        print((d/'probe.stdout').read_text().strip(), flush=True)
    finally:
        try:
            stop_children([portal, bus])
            status['cleanup_cases'][case] = True
        except BaseException:
            status['cleanup_cases'][case] = False
            raise
try:
    for case in ['deny', 'chooser-cancel', 'unknown', 'client-cancel', 'stale', 'portal-success', 'missing', 'record', 'activation-denied']:
        status['current_case'] = case
        (root/'result.json').write_text(json.dumps(status, indent=2)+'\n')
        run_case(case)
        status['completed_cases'].append(case)
    status['passed'] = True
    status['current_case'] = None
except BaseException as error:
    # No arbitrary exception strings or target-bearing tracebacks in CI logs.
    status['failure_type'] = type(error).__name__
    print('FAIL owned GIO boundary; inspect private evidence.', file=sys.stderr)
    raise SystemExit(1) from None
finally:
    (root/'result.json').write_text(json.dumps(status, indent=2)+'\n')
print('PASS all owned native GIO lanes; this is not GTK UI acceptance.')
