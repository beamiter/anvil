#!/usr/bin/env python3
"""Owned-session GTK/AT-SPI/Orca integration observer. Fail closed on missing evidence.
No UI injection, account access, provider invocation, or existing-session takeover.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import uuid


def check(condition, message):
    if not condition:
        raise AssertionError(message)

# Explicit allowlist: no inherited accessibility, Wayland or host audio routes.
def child_environment(source):
    allowed = ('PATH', 'HOME', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME',
               'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS', 'DISPLAY', 'XAUTHORITY',
               'ANVIL_A11_OWNED_SESSION', 'PULSE_SERVER', 'SPEECHD_ADDRESS', 'ALSA_CONFIG_PATH')
    env = {key: source[key] for key in allowed if key in source}
    env.update(GTK_A11Y='atspi', GDK_BACKEND='x11', GSETTINGS_BACKEND='memory', LC_ALL='C.UTF-8', TZ='UTC')
    return env

def cleanup_children(children, files):
    errors = []
    for child in reversed(children):
        try:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
        except BaseException as error:
            errors.append(repr(error))
            try:
                child.kill()
                child.wait(timeout=5)
            except BaseException as fallback_error:
                errors.append(repr(fallback_error))
    for output in files:
        try:
            output.close()
        except BaseException as error:
            errors.append(repr(error))
    return errors

def finish_owned(evidence, result, children, files):
    # Finish every owned cleanup attempt before recording any success. Each
    # resource failure is isolated by cleanup_children and must fail the run.
    errors = cleanup_children(children, files)
    result['cleanup_errors'] = errors
    result['cleanup_completed'] = not errors
    if errors:
        result['passed'] = False
    try:
        (evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    except BaseException:
        result['passed'] = False
        raise
    if errors:
        raise RuntimeError('owned fixture cleanup failed: ' + '; '.join(errors))

def orca_version(env):
    # check_output kills and reaps its child on TimeoutExpired; a hung probe
    # cannot prevent the enclosing finally from cleaning other owned helpers.
    return subprocess.check_output(['orca', '--version'], env=env, text=True, timeout=5).strip()

def interrupted(signum, _frame):
    raise InterruptedError(f'owned fixture interrupted by signal {signum}')

TEST = 'task_announcements::native_tests::semantic_task_results_reach_atspi_and_orca'
SPEECH = re.compile(r"SPEECH OUTPUT: '(.*?)'")

def presented_messages(log, marker):
    # GNOME Orca 46 speech._speak logs its actual presentation boundary. Raw
    # event logs are deliberately excluded; event reception alone cannot pass.
    return [text for text in SPEECH.findall(log) if text.startswith(marker)]

def verify_phase(expected, events, speech, window, marker):
    announcements = [e for e in events if e['type'].startswith('object:announcement')]
    check([e['text'] for e in announcements] == expected, ('AT-SPI announcements', announcements, expected))
    check(all(e['priority'] == 1 and e['name'] == window for e in announcements), 'wrong source/politeness')
    check(presented_messages(speech, marker) == expected, ('Orca presentation', presented_messages(speech, marker), expected))
    check(not any(e['type'].startswith('object:state-changed:focused') and e['priority'] and e['name'] != 'A11 draft owner' for e in events), 'focus moved')
    check(not any(e['type'].startswith('window:activate') for e in events), 'window activated during feedback')

class ClockGuard:
    def __init__(self):
        self.wall = time.time()
        self.monotonic = time.monotonic()
    def checkpoint(self):
        wall = time.time()
        elapsed = time.monotonic() - self.monotonic
        check(abs((wall - self.wall) - elapsed) < 0.25,
              'wall clock drifted from continuous monotonic reference')
        return wall

def verify_flushed_speech(log, phases, marker):
    # Orca 46 buffers its debug file. Read only after graceful owned shutdown,
    # but attribute each utterance using Orca's recorded wall-clock timestamp.
    check(bool(phases), 'missing measured phases')
    previous_end = None
    for phase in phases:
        check(phase['wall_start'] < phase['wall_end'], 'invalid phase interval')
        if previous_end is not None:
            check(previous_end < phase['wall_start'], 'overlapping or unordered phase intervals')
        previous_end = phase['wall_end']
    records = []
    for line in log.splitlines():
        messages = presented_messages(line, marker)
        if not messages:
            continue
        match = re.match(r'(\d{2}:\d{2}:\d{2}\.\d+) - SPEECH OUTPUT:', line)
        check(match is not None, 'speech presentation lacks timestamp')
        clock = datetime.datetime.strptime(match.group(1), '%H:%M:%S.%f').time()
        anchor = datetime.datetime.fromtimestamp(phases[0]['wall_start'], datetime.timezone.utc)
        candidates = [datetime.datetime.combine(anchor.date() + datetime.timedelta(days=day), clock, tzinfo=datetime.timezone.utc).timestamp()
                      for day in (-1, 0, 1)]
        stamp = min(candidates, key=lambda value: abs(value - phases[0]['wall_start']))
        records.extend((stamp, message) for message in messages)
    assigned = [[] for _ in phases]
    for stamp, message in records:
        matches = [index for index, phase in enumerate(phases)
                   if phase['wall_start'] <= stamp <= phase['wall_end']]
        check(len(matches) == 1, 'speech record must belong to exactly one phase')
        assigned[matches[0]].append(message)
    for phase, speech in zip(phases, assigned):
        check(speech == phase['expected'], ('Orca phase presentation', phase['name'], speech, phase['expected']))
        phase['orca_presented'] = speech


def validate_shutdown_wake(events, acknowledgement, fixture_pid, wake_name):
    check(all(event['pid'] == fixture_pid for event in events), 'foreign shutdown event')
    check(not any(event['type'].startswith(('object:announcement',
                  'object:state-changed:focused', 'window:activate')) for event in events),
          'shutdown wake announced, moved focus or activated a window')
    matching = [event for event in events
                if event['type'].startswith('object:property-change:accessible-name')
                and event['name'] == wake_name and event['role'] == 'frame']
    check(len(matching) <= 1, 'duplicate shutdown wake')
    if acknowledgement is None or not matching:
        return False
    check(acknowledgement['pid'] == fixture_pid and acknowledgement['name'] == wake_name,
          'wrong wake ownership')
    check(acknowledgement['focus_preserved'] and acknowledgement['draft_preserved'],
          'wake stole focus/draft')
    return True

def shutdown_orca(orca, pump, clock_guard, request_wake, wake_observed):
    # SIGTERM is handled by Orca46 shutdownOnSignal -> shutdown. Keep the
    # owned GTK accessible objects and bus alive until it unregisters itself.
    clock_guard.checkpoint()
    deadline = time.monotonic() + 5
    orca.terminate()
    request_wake()
    while True:
        clock_guard.checkpoint()
        if time.monotonic() >= deadline:
            raise subprocess.TimeoutExpired('owned Orca graceful shutdown and wake', 5)
        exited = orca.poll() is not None
        observed = wake_observed()
        if exited and observed:
            check(time.monotonic() < deadline, 'shutdown evidence arrived after deadline')
            break
        pump()
    check(orca.returncode == 0, f'Orca did not shut down gracefully: {orca.returncode}')


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--test-binary', required=True)
    p.add_argument('--evidence', required=True)
    args = p.parse_args()
    if os.environ.get('ANVIL_A11_OWNED_SESSION') != '1':
        raise SystemExit('Run only through test-task-accessibility.sh in its fresh D-Bus/Xvfb session')
    import gi
    gi.require_version('Atspi', '2.0')
    from gi.repository import Atspi, GLib
    evidence = Path(args.evidence).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    if any(evidence.iterdir()):
        raise SystemExit('Evidence directory must be empty')
    binary = Path(args.test_binary).resolve(strict=True)
    token = uuid.uuid4().hex[:16]
    marker = f'A11 {token} same task ['
    env = child_environment(os.environ)
    env.update(ANVIL_A11_TOKEN=token, ANVIL_A11_EVIDENCE=str(evidence))
    clock_guard = ClockGuard()
    children = []
    files = []
    events = []
    fixture = None
    result = {'passed': False, 'native_executed': True, 'token': token,
              'orca_presentation_verified': False, 'audible_output_verified': False,
              'speech_clock': 'UTC wall clock, Orca HH:MM:SS.microseconds',
              'timezone': 'UTC', 'observer_timezone': list(time.tzname),
              'scope': 'production delivery helper and reducer; synthetic lifecycle, not full AppModel/runtime'}
    def start(argv, log):
        out = open(evidence / log, 'w')
        files.append(out)
        child = subprocess.Popen(argv, env=env, stdout=out, stderr=subprocess.STDOUT)
        children.append(child)
        return child
    def pump():
        clock_guard.checkpoint()
        context = GLib.MainContext.default()
        while context.pending():
            context.iteration(False)
        time.sleep(0.01)
    def wait(predicate, seconds=30):
        end = time.monotonic() + seconds
        while not predicate():
            if fixture is not None and fixture.poll() is not None:
                raise RuntimeError(f'GTK fixture exited early: {fixture.returncode}')
            if time.monotonic() >= end:
                raise TimeoutError('native accessibility evidence did not arrive')
            pump()
    def tree():
        desktop = Atspi.get_desktop(0)
        apps = [desktop.get_child_at_index(i) for i in range(desktop.get_child_count())]
        roots = [app for app in apps if app.get_process_id() == fixture.pid]
        nodes, queue = [], list(roots)
        while queue:
            check(len(nodes) < 500, 'unexpectedly large fixture tree')
            node = queue.pop(0)
            nodes.append({'name': node.get_name(), 'role': node.get_role_name(),
                          'alert': node.get_role() == Atspi.Role.ALERT,
                          'focused': node.get_state_set().contains(Atspi.StateType.FOCUSED),
                          'showing': node.get_state_set().contains(Atspi.StateType.SHOWING)})
            queue.extend(node.get_child_at_index(i) for i in range(node.get_child_count()))
        return nodes
    def event_received(event, *_):
        # Never collect another application's content, even in this isolated session.
        if fixture is None or event.source.get_process_id() != fixture.pid:
            return
        row = {'time': time.monotonic(), 'type': event.type, 'name': event.source.get_name(),
               'role': event.source.get_role_name(), 'pid': fixture.pid,
               'priority': event.detail1, 'detail2': event.detail2,
               'text': event.any_data if isinstance(event.any_data, str) else None}
        events.append(row)
        with open(evidence / 'events.jsonl', 'a') as output:
            output.write(json.dumps(row) + '\n')
    prior_handlers = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGTERM, signal.SIGINT)}
    try:
        result['orca_version'] = orca_version(env)
        # A window manager is required for genuine active/focused AT-SPI state.
        wm = start(['openbox', '--sm-disable'], 'window-manager.log')
        end = time.monotonic() + 1.0
        while time.monotonic() < end:
            pump()
        check(wm.poll() is None, 'owned window manager failed')
        Atspi.init()
        listener = Atspi.EventListener.new(event_received)
        for event_type in ['object:announcement', 'object:state-changed:focused', 'window:activate', 'object:property-change:accessible-name']:
            registered = listener.register(event_type)
            check(registered, f'failed to register {event_type}')
        orca_log = evidence / 'orca-debug.log'
        orca = start(['orca', '--debug-file=' + str(orca_log), '-u', str(evidence / 'orca-prefs'), '-e', 'speech'], 'orca-launch.log')
        # No --replace and no settings modifications outside the private HOME.
        wait(lambda: orca_log.exists() and 'registering listener for: object:announcement' in orca_log.read_text(errors='replace'), 15)
        check(orca.poll() is None, 'Orca did not remain running')
        fixture = start([str(binary), '--exact', TEST, '--ignored', '--nocapture', '--test-threads=1'], 'fixture.log')
        phases = []
        index = 0
        while True:
            ready = evidence / f'{index:02}.ready.json'
            finished = evidence / 'finished.json'
            wait(lambda: ready.exists() or finished.exists())
            if finished.exists() and not ready.exists():
                break
            phase = json.loads(ready.read_text())
            check(phase['pid'] == fixture.pid, 'native oracle failed')
            wait(lambda: any(n['focused'] and n['name'] == 'A11 draft owner' for n in tree()))
            # Finish initial activation before measuring any phase, including
            # zero-announcement seeding. Subsequent activation is a failure.
            end = time.monotonic() + 0.5
            while time.monotonic() < end:
                pump()
            event_start = len(events)
            wall_start = clock_guard.checkpoint()
            (evidence / f'{index:02}.go').touch()
            wait(lambda: (evidence / f'{index:02}.done.json').exists())
            expected = phase['expected']
            # Include trailing events/speech so duplicates cannot be hidden by
            # advancing as soon as the first matching utterance appears.
            end = time.monotonic() + 1.0
            while time.monotonic() < end:
                pump()
            captured = events[event_start:]
            wall_end = clock_guard.checkpoint()
            # Event/focus checks are immediate; speech is checked from the
            # flushed genuine Orca log after shutdown, never synthetic text.
            announcements = [e for e in captured if e['type'].startswith('object:announcement')]
            check([e['text'] for e in announcements] == expected, 'AT-SPI announcement mismatch')
            check(all(e['priority'] == 1 and e['name'] == phase['window'] for e in announcements), 'wrong source/politeness')
            check(not any(e['type'].startswith('object:state-changed:focused') and e['priority'] and e['name'] != 'A11 draft owner' for e in captured), 'focus moved')
            check(not any(e['type'].startswith('window:activate') for e in captured), 'window activated during feedback')
            nodes = tree()
            check(not any(n['alert'] for n in nodes), 'task-result Alert remains in the exported tree')
            check(any(n['focused'] and n['name'] == 'A11 draft owner' for n in nodes), 'native oracle failed')
            if phase['visible'] is not None:
                check(any(n['name'] == phase['visible'] and n['showing'] for n in nodes), 'feedback text absent from actual AT-SPI tree')
            done = json.loads((evidence / f'{index:02}.done.json').read_text())
            check(done['focus_preserved'] and done['draft_preserved'], 'native oracle failed')
            phases.append({'name': phase['name'], 'expected': expected, 'events': captured,
                           'wall_start': wall_start, 'wall_end': wall_end, 'tree': nodes})
            (evidence / f'{index:02}.ack').touch()
            index += 1
        check(json.loads(finished.read_text())['phases'] == index == 21, 'incomplete scenario suite')
        result['phases'] = phases
        result['phase_count'] = index
        check(orca.poll() is None, 'Orca exited before completion')
        # The GTK fixture is still pumping while waiting for finish.ack.
        # Do not destroy its accessibility tree before Orca's graceful cleanup.
        wake_name = f'A11 fixture shutdown {token}'
        wake_start = len(events)
        def request_wake():
            temporary = evidence / 'shutdown.request.tmp'
            temporary.write_text('owned observer shutdown wake\n')
            temporary.replace(evidence / 'shutdown.request')
        def wake_observed():
            ack = evidence / 'shutdown.ack.json'
            value = json.loads(ack.read_text()) if ack.exists() else None
            return validate_shutdown_wake(events[wake_start:], value, fixture.pid, wake_name)
        shutdown_orca(orca, pump, clock_guard, request_wake, wake_observed)
        result['shutdown_wake'] = events[wake_start:]
        speech_log = orca_log.read_text(errors='strict')
        check('ORCA: Shutdown complete' in speech_log, 'Orca shutdown completion missing')
        verify_flushed_speech(speech_log, phases, marker)
        (evidence / 'finish.ack').touch()
        exit_code = fixture.wait(timeout=10)
        clock_guard.checkpoint()
        check(exit_code == 0, 'GTK fixture failed after final phase')
        result.update(passed=True, orca_presentation_verified=True)
    except Exception as error:
        result['error'] = repr(error)
        raise
    finally:
        # Block repeated termination signals while bounded cleanup runs.
        for sig in prior_handlers:
            signal.signal(sig, signal.SIG_IGN)
        try:
            finish_owned(evidence, result, children, files)
        finally:
            for sig, handler in prior_handlers.items():
                signal.signal(sig, handler)

if __name__ == '__main__':
    main()
