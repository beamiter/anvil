#!/usr/bin/env python3
"""Collect raw native Block evidence on an isolated X11 display.

This collector deliberately cannot emit a passing comparator record: pixel
validation and whole-GLib idle-source attribution are still separate gates.
Never run it on a user's display. The instrumented binary must be built using
the reviewed temporary overlay. It does not modify the normal installed binary.
"""
import argparse
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import select
import sys
import subprocess
import tempfile
import time


_CANCELLATION = None


class CollectorCancelled(RuntimeError):
    pass


class CancellationSignals:
    """Handlers only record intent; checkpoints raise outside teardown.

    Repeated TERM/INT during cleanup must not asynchronously interrupt child
    reaping or evidence publication. Outermost SIGKILL is explicitly not covered.
    """
    def __init__(self, evidence):
        global _CANCELLATION
        self.evidence = evidence
        self.cleaning = False
        self.previous_active = _CANCELLATION
        self.previous = {sig: signal.getsignal(sig) for sig in (signal.SIGTERM, signal.SIGINT)}
        evidence["cancellation_signals"] = []
        evidence["cancellation_count"] = 0
        _CANCELLATION = self
        for sig in self.previous: signal.signal(sig, self._record)

    def _record(self, signum, _frame):
        self.evidence["cancellation_count"] += 1
        if len(self.evidence["cancellation_signals"]) < 32:
            self.evidence["cancellation_signals"].append(signum)

    def check(self):
        if self.evidence["cancellation_count"] and not self.cleaning:
            raise CollectorCancelled(f"collector cancelled by signal {self.evidence['cancellation_signals'][0]}")

    def begin_cleanup(self):
        self.cleaning = True

    def restore(self):
        global _CANCELLATION
        for sig, previous in self.previous.items(): signal.signal(sig, previous)
        _CANCELLATION = self.previous_active


def check_cancelled():
    if _CANCELLATION is not None: _CANCELLATION.check()


def sleep_cancellable(seconds):
    deadline = time.monotonic() + seconds
    while True:
        check_cancelled()
        remaining = deadline - time.monotonic()
        if remaining <= 0: return
        time.sleep(min(.05, remaining))


def create_fixture_launcher(root, fixture, reports):
    """Config accepts ONE executable path, never a shell command string."""
    interpreter = str(Path(sys.executable).resolve(strict=True))
    if any(c.isspace() for c in interpreter):
        raise ValueError("fixture interpreter path cannot contain shebang whitespace")
    launcher = root / "fixture-launcher"
    program = ("#!" + interpreter + " -I\nimport os\n" +
               "os.execv(" + repr(interpreter) + ", " +
               repr([interpreter, "-I", str(fixture.resolve(strict=True)), "--reports", str(reports)]) + ")\n")
    with tempfile.NamedTemporaryFile(mode="w", dir=root, prefix="launcher-", delete=False) as temporary:
        temporary.write(program)
        os.fchmod(temporary.fileno(), 0o700)
        temporary_path = Path(temporary.name)
    temporary_path.replace(launcher)
    return launcher


def scheduled_tab_index(elapsed_seconds, interval_seconds, tab_count):
    """Compute an integer index without truncating subsecond smoke elapsed time."""
    if elapsed_seconds < 0 or interval_seconds <= 0 or type(tab_count) is not int or tab_count < 1:
        raise ValueError("invalid tab schedule")
    return int(elapsed_seconds / interval_seconds) % tab_count


def wait_for(predicate, process, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        check_cancelled()
        if process.poll() is not None:
            raise RuntimeError("owned application exited before evidence was ready")
        value = predicate()
        if value:
            return value
        sleep_cancellable(.025)
    raise TimeoutError("native collector readiness deadline exceeded")


def read_json(path):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return None


def proc_sample(pid, proc_root=Path("/proc")):
    base = proc_root / str(pid)
    status = dict(line.split(":", 1) for line in (base / "status").read_text().splitlines() if ":" in line)
    stat = (base / "stat").read_text().rsplit(")", 1)[1].split()
    threads = {}
    for entry in (base / "task").iterdir():
        try:
            fields = dict(line.split(":", 1) for line in (entry / "status").read_text().splitlines() if ":" in line)
        except FileNotFoundError:
            continue
        threads[entry.name] = {k: int(fields[k]) for k in ("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")}
    return dict(monotonic_ns=time.monotonic_ns(), rss_bytes=int(status["VmRSS"].split()[0]) * 1024,
                cpu_ticks=int(stat[11]) + int(stat[12]), threads=threads)


def process_identity(pid):
    """Numeric identity only. start ticks prevents PID-reuse confusion."""
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return dict(pid=pid, ppid=int(fields[1]), start_ticks=int(fields[19]), state=fields[0])
    except (FileNotFoundError, ProcessLookupError):
        return None


class OwnedProcesses:
    """Dedicated collector's descendants, pinned by pidfd and numeric birth time.

    Become a child subreaper BEFORE spawning the app. Unexpected early app exit
    therefore cannot orphan an unobserved fixture out of our ownership tree.
    Existing collector children are excluded. No process name or stale PID is
    ever a signal target. This is Linux-only, like the eventfd workload.
    """
    def __init__(self):
        self.owner = os.getpid()
        self.handles = {}
        self.observed = {}
        self.app = None
        self.libc = ctypes.CDLL(None, use_errno=True)
        previous = ctypes.c_int()
        if self.libc.prctl(37, ctypes.byref(previous), 0, 0, 0) != 0:
            raise OSError(ctypes.get_errno(), "cannot read child-subreaper state")
        self.previous_subreaper = previous.value
        self.excluded = {row["pid"] for row in self._table().values() if row["ppid"] == self.owner}
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise RuntimeError("Linux pidfd ownership is required")
        if self.libc.prctl(36, 1, 0, 0, 0) != 0:
            raise OSError(ctypes.get_errno(), "cannot establish owned child subreaper")

    @staticmethod
    def _table():
        table = {}
        for path in Path("/proc").iterdir():
            if path.name.isdigit():
                row = process_identity(int(path.name))
                if row is not None: table[row["pid"]] = row
        return table

    def bind(self, process):
        self.app = process
        self.refresh()

    def refresh(self):
        table = self._table()
        parents = {self.owner}
        allowed = {}
        changed = True
        while changed:
            changed = False
            for pid, row in table.items():
                if pid not in self.excluded and pid not in allowed and row["ppid"] in parents:
                    allowed[pid] = row; parents.add(pid); changed = True
        for pid, row in allowed.items():
            if pid in self.handles:
                if self.observed[pid]["start_ticks"] != row["start_ticks"]:
                    raise RuntimeError("owned PID identity changed while handle was live")
                continue
            try: descriptor = os.pidfd_open(pid)
            except ProcessLookupError: continue
            again = process_identity(pid)
            if again is None or again["start_ticks"] != row["start_ticks"]:
                os.close(descriptor); continue
            self.handles[pid] = descriptor
            self.observed[pid] = dict(row)
        return allowed

    def send(self, pid, sig):
        if pid not in self.handles:
            raise RuntimeError("refusing signal to unregistered descendant")
        signal.pidfd_send_signal(self.handles[pid], sig)

    def identities(self):
        return [dict(self.observed[pid]) for pid in sorted(self.observed)]

    def _reap(self):
        if self.app is not None: self.app.poll()
        for pid in self.handles:
            if self.app is not None and pid == self.app.pid: continue
            try: os.waitpid(pid, os.WNOHANG)
            except ChildProcessError: pass  # Its still-live parent owns reaping.

    def remaining(self):
        self.refresh(); self._reap()
        result = []
        for pid, descriptor in self.handles.items():
            row = process_identity(pid)
            # A readable pidfd proves this identity exited. /proc absence after
            # reaping also excludes retained zombies; reused PIDs are not ours.
            exited = bool(select.select([descriptor], [], [], 0)[0])
            same = row is not None and row["start_ticks"] == self.observed[pid]["start_ticks"]
            if not exited or same: result.append(pid)
        return result

    def cleanup(self, grace=3):
        errors = []
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                self.refresh()
                for pid in self.remaining():
                    try: self.send(pid, sig)
                    except ProcessLookupError: pass
                    except Exception as error: errors.append(f"signal {pid}: {error}")
                deadline = time.monotonic() + grace
                while time.monotonic() < deadline:
                    if not self.remaining(): break
                    time.sleep(.025)
            except Exception as error:
                errors.append(f"descendant cleanup: {error}")
            # Always try the Popen-owned app as a second independent cleanup lane.
            if self.app is not None:
                try:
                    if self.app.poll() is None:
                        if sig == signal.SIGTERM: self.app.terminate()
                        else: self.app.kill()
                    self.app.wait(timeout=grace)
                except subprocess.TimeoutExpired: pass
                except Exception as error: errors.append(f"application cleanup: {error}")
        try: remaining = self.remaining()
        except Exception as error:
            errors.append(f"exit verification: {error}"); remaining = None
        return dict(verified_empty=remaining == [] and not errors,
                    remaining_pids=remaining, errors=errors, owned_identities=self.identities())

    def close(self):
        errors = []
        for fd in self.handles.values():
            try: os.close(fd)
            except OSError as error: errors.append(str(error))
        self.handles.clear()
        if self.libc.prctl(36, self.previous_subreaper, 0, 0, 0) != 0:
            errors.append("cannot restore child-subreaper state")
        if errors: raise RuntimeError("; ".join(errors))


def harvest(evidence, root, reports, ownership):
    """Capture all currently available evidence before temporary files disappear."""
    failures = evidence.setdefault("harvest_errors", [])
    for name in ("ready", "response"):
        try:
            value = read_json(root / (name + ".json"))
            if value is not None: evidence["last_" + name] = value
        except Exception as error: failures.append(f"{name}: {error}")
    fixtures = evidence.setdefault("fixture_reports", {})
    try: paths = list(reports.glob("*.json"))
    except Exception as error: failures.append(f"fixture enumeration: {error}"); paths = []
    for path in paths:
        try: fixtures[path.stem] = read_json(path)
        except Exception as error: failures.append(f"fixture report: {error}")
    if ownership is not None:
        try: evidence["owned_identities"] = ownership.identities()
        except Exception as error: failures.append(f"identity snapshot: {error}")


def write_evidence(output, evidence):
    """Publish a complete report atomically; a partial write is never a success file."""
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", prefix="." + output.name + ".pending-",
                                         dir=output.parent, delete=False) as stream:
            temporary = Path(stream.name)
            json.dump(evidence, stream, indent=2)
            stream.flush()
        temporary.replace(output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def finalize(evidence, output, process, xinput, log, ownership, root, reports, capture_ok):
    """Every cleanup lane runs even when evidence or display teardown fails."""
    errors = evidence.setdefault("finalization_errors", [])
    try:
        harvest(evidence, root, reports, ownership)
    except Exception as error: errors.append(f"harvest: {error}")
    # Process cleanup precedes output I/O and display teardown. A failing Xlib
    # close or an unwritable report can never prevent terminating owned children.
    try:
        if ownership is not None:
            evidence["cleanup"] = ownership.cleanup()
        else:
            evidence["cleanup"] = dict(verified_empty=process is None, errors=["ownership unavailable"] if process else [])
    except Exception as error:
        errors.append(f"ownership cleanup: {error}")
        evidence["cleanup"] = dict(verified_empty=False, errors=[str(error)])
    # Independent Popen lane remains mandatory even if the ownership helper fails.
    if process is not None:
        try:
            if process.poll() is None: process.terminate()
            try: process.wait(timeout=3)
            except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=3)
        except Exception as error: errors.append(f"application final wait: {error}")
    try: harvest(evidence, root, reports, ownership)
    except Exception as error: errors.append(f"post-cleanup harvest: {error}")
    if xinput is not None:
        try: xinput.close()
        except Exception as error: errors.append(f"display close: {error}")
    if log is not None:
        try: log.close()
        except Exception as error: errors.append(f"log close: {error}")
    if ownership is not None:
        try: ownership.close()
        except Exception as error: errors.append(f"ownership close: {error}")
    evidence["raw_capture_completed"] = bool(capture_ok and evidence["cleanup"]["verified_empty"]
                                             and not errors and not evidence.get("harvest_errors")
                                             and not evidence.get("cancellation_count"))
    evidence["evidence_write_succeeded"] = False
    try:
        # A file-write failure still leaves a complete in-memory report for a
        # caller/test and a serialized stderr fallback for hosted log artifacts.
        evidence["evidence_write_succeeded"] = True
        write_evidence(output, evidence)
    except Exception as error:
        evidence["evidence_write_succeeded"] = False
        evidence["raw_capture_completed"] = False
        errors.append(f"evidence write: {error}")
        try:
            with tempfile.NamedTemporaryFile(mode="w", prefix=output.name + ".failed-", suffix=".json",
                                             dir=output.parent, delete=False) as fallback:
                evidence["fallback_evidence_path"] = fallback.name
                json.dump(evidence, fallback, indent=2)
            print("ANVIL_BUDGET_EVIDENCE_FALLBACK " + evidence["fallback_evidence_path"], file=sys.stderr)
        except Exception as fallback_error:
            errors.append(f"fallback evidence write: {fallback_error}")
            print("ANVIL_BUDGET_EVIDENCE_WRITE_FAILED " + json.dumps(evidence), file=sys.stderr)
    # A first catchable cancellation arriving during publication also invalidates
    # success. Once failed, later signals cannot turn the record back into success.
    if evidence.get("cancellation_count") and evidence["raw_capture_completed"]:
        evidence["raw_capture_completed"] = False
        try: write_evidence(output, evidence)
        except Exception as error:
            errors.append(f"cancelled evidence rewrite: {error}")
            print("ANVIL_BUDGET_CANCELLED_EVIDENCE " + json.dumps(evidence), file=sys.stderr)
    return evidence["raw_capture_completed"]


class XInput:
    def __init__(self, pid):
        # Reuse reviewed XTest declarations; importing it does not open a display.
        helper = Path(__file__).with_name("qa-cross-selection-pointer.py")
        spec = importlib.util.spec_from_file_location("pointer", helper)
        self.qa = importlib.util.module_from_spec(spec); spec.loader.exec_module(self.qa)
        self.x, self.t = self.qa.x11, self.qa.xtest
        self.x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.x.XSync.restype = ctypes.c_int
        self.x.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        self.x.XKeysymToKeycode.restype = ctypes.c_ubyte
        self.t.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
        self.display = self.x.XOpenDisplay(None)
        if not self.display:
            raise RuntimeError("isolated X11 display unavailable")
        windows = subprocess.check_output(["xdotool", "search", "--onlyvisible", "--pid", str(pid)], text=True, timeout=5).split()
        if len(windows) != 1:
            raise RuntimeError("require exactly one visible window belonging to owned application PID")
        self.window = int(windows[0])
        subprocess.run(["xdotool", "windowsize", str(self.window), "1600", "1000"], check=True, timeout=5)
        subprocess.run(["xdotool", "windowfocus", "--sync", str(self.window)], check=True, timeout=5)
        self.key = self.x.XKeysymToKeycode(self.display, ord("a"))
        if not self.key: raise RuntimeError("fixture key unavailable")
        self.root = self.x.XDefaultRootWindow(self.display)
        self.refresh_origin()

    def refresh_origin(self):
        x, y, child = ctypes.c_int(), ctypes.c_int(), ctypes.c_ulong()
        if not self.x.XTranslateCoordinates(self.display, self.window, self.root, 0, 0,
                ctypes.byref(x), ctypes.byref(y), ctypes.byref(child)):
            raise RuntimeError("cannot locate owned application")
        self.origin = (x.value, y.value)

    def move(self, x, y):
        if not self.t.XTestFakeMotionEvent(self.display, -1, round(self.origin[0] + x), round(self.origin[1] + y), 0):
            raise RuntimeError("XTest motion refused")

    def button(self, pressed):
        if not self.t.XTestFakeButtonEvent(self.display, 1, int(pressed), 0):
            raise RuntimeError("XTest button refused")

    def focus(self, bounds):
        self.move(bounds[0] + bounds[2] / 2, bounds[1] + bounds[3] / 2)
        self.button(True); self.button(False); self.x.XFlush(self.display)

    def keypress(self):
        stamp = time.monotonic_ns()
        for pressed in (1, 0):
            if not self.t.XTestFakeKeyEvent(self.display, self.key, pressed, 0):
                raise RuntimeError("XTest key refused")
        self.x.XFlush(self.display)
        return stamp

    def drag(self, divider, delta):
        self.move(divider["x"], divider["y"]); self.button(True)
        self.x.XFlush(self.display)
        stamp = time.monotonic_ns()
        self.move(divider["x"] + (delta if divider["horizontal"] else 0),
                  divider["y"] + (0 if divider["horizontal"] else delta))
        self.button(False); self.x.XFlush(self.display)
        return stamp

    def close(self):
        self.x.XCloseDisplay(self.display)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--panes", type=int, choices=(1, 8, 32), required=True)
    parser.add_argument("--output", type=Path, required=True)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--smoke", action="store_true", help="3 key events and 3-second idle; never budget acceptance")
    mode.add_argument("--full-budget", action="store_true", help="explicitly reviewed full window, only after native smoke passes")
    parser.add_argument("--pixel-gate", choices=("observe", "control"), help="short pixel gate only; control emits same barcode without observer")
    args = parser.parse_args()
    if args.pixel_gate and not args.smoke: raise SystemExit("pixel gate only supports short smoke")
    if os.environ.get("ANVIL_BUDGET_ISOLATED_DISPLAY") != "1" or not os.environ.get("DISPLAY"):
        raise SystemExit("launch only through the reviewed isolated Xvfb/session-bus wrapper")
    binary = args.binary.resolve(strict=True)
    fixture = Path(__file__).with_name("interaction-pty-fixture.py").resolve(strict=True)
    output = args.output.resolve(); output.parent.mkdir(parents=True, exist_ok=True)
    evidence = dict(schema="raw-native-block-v1", budget_acceptance=False,
                    missing_gates=["independent pixel subset", "complete idle source attribution", "paired matrix aggregation"],
                    panes=args.panes, binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), smoke=args.smoke)
    process = None; xinput = None; ownership = None; capture_ok = False; pixel = None; pixel_module = None
    evidence.update(keys=[], drags=[], resources=[], snapshots=[], fixture_reports={}, raw_capture_completed=False,
                    pixel_mode=args.pixel_gate, input_helper_pid=None, pixel_observer_pid=None, pixel_events=[], pixel_drags=[], allocation_drags=[], pixel_metric="virtual_display_readback_not_physical_scanout")
    with tempfile.TemporaryDirectory(prefix="anvil-interaction-") as directory:
        root = Path(directory)
        reports = root / "fixtures"; reports.mkdir(mode=0o700)
        env = {key: value for key, value in os.environ.items() if not key.startswith("PYTHON")}
        for key in ("CONFIG", "DATA", "STATE", "CACHE", "RUNTIME"):
            path = root / key.lower(); path.mkdir(mode=0o700)
            env["XDG_" + key + ("_DIR" if key == "RUNTIME" else "_HOME")] = str(path)
        env.update(ANVIL_INTERACTION_BUDGET_DIR=str(root), GDK_BACKEND="x11", GSK_RENDERER="cairo", GDK_SCALE="1", GDK_DPI_SCALE="1",
                   LIBGL_ALWAYS_SOFTWARE="1", GTK_A11Y="none", NO_AT_BRIDGE="1")
        # Configured executable preserves normal session-persistence and pane caps.
        shell = str(create_fixture_launcher(root, fixture, reports))
        env["ANVIL_INTERACTION_FIXTURE_PROGRAM"] = shell
        config = root / "config.toml"
        config.write_text('terminal_mode = "block"\nfont = "DejaVu Sans Mono 12"\nfont_scale = 1.0\n'
                          'opacity = 1.0\nsidebar_visible = false\ntab_placement = "top"\n'
                          'jsh_update_check = "never"\n' + 'shell = ' + json.dumps(shell) + '\n')
        if args.pixel_gate: env["ANVIL_INTERACTION_PIXEL_MARKERS"] = "1"
        request = 0
        log = None
        cancellation = CancellationSignals(evidence)
        try:
            check_cancelled()
            log = open(output.with_suffix(".app.log"), "wb")
            ownership = OwnedProcesses()
            process = subprocess.Popen([str(binary), "--config", str(config), "--no-restore"], env=env,
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            ownership.bind(process)
            wait_for(lambda: (ownership.refresh() is not None and (root / "ready.json").exists()), process)
            def control(command, **values):
                nonlocal request
                request += 1
                temporary = root / "command.tmp"
                temporary.write_text(json.dumps(dict(command=command, request=request, **values)))
                temporary.replace(root / "command.json")
                ownership.send(process.pid, signal.SIGUSR1)
                response = wait_for(lambda: (r if (r := read_json(root / "response.json")) and r["request"] == request else None), process)
                ownership.refresh()
                evidence["snapshots"].append(response)
                evidence["owned_identities"] = ownership.identities()
                harvest(evidence, root, reports, ownership)
                return response
            snapshot = control("snapshot")
            if snapshot.get("fixture_program_matches") is not True:
                raise RuntimeError("production config parser did not select the exact executable fixture program")
            if not snapshot["session_persistence"]:
                raise RuntimeError("benchmark must preserve production persistence/pane limits")
            tab_ids = []
            for tab_index in range(2 if args.panes == 32 else 1):
                if tab_index: snapshot = control("new_tab")
                tab_id = snapshot["tabs"][snapshot["active_tab_index"]]["tab_id"]
                tab_ids.append(tab_id)
                goal = 16 if args.panes == 32 else args.panes
                level = 0; count = 1
                while count < goal:
                    # Split every existing leaf once, alternating axis by level.
                    for pane_index in range(count):
                        snapshot = control("split", pane_index=pane_index, horizontal=level % 2 == 0)
                        sleep_cancellable(.15)
                    count *= 2; level += 1
                sleep_cancellable(.5)
                snapshot = control("snapshot")
                actual = next(t for t in snapshot["tabs"] if t["tab_id"] == tab_id)
                if len(actual["panes"]) != goal: raise RuntimeError("production split count did not match")
            if args.pixel_gate:
                input_spec = importlib.util.spec_from_file_location("input_client", Path(__file__).with_name("interaction-input-client.py"))
                input_module = importlib.util.module_from_spec(input_spec); input_spec.loader.exec_module(input_module)
                def input_started(pid):
                    evidence["input_helper_pid"] = pid
                    ownership.refresh()
                xinput = input_module.InputClient(process.pid, env, check_cancelled, input_started)
            else:
                xinput = XInput(process.pid)
            evidence["window_origin"] = list(xinput.origin)
            if args.pixel_gate:
                spec = importlib.util.spec_from_file_location("pixel_client", Path(__file__).with_name("interaction-pixel-client.py"))
                pixel_module = importlib.util.module_from_spec(spec); spec.loader.exec_module(pixel_module)
                if args.pixel_gate == "observe":
                    pixel = pixel_module.PixelClient(output.with_suffix(".pixels"), env, check_cancelled)
                    evidence["pixel_observer_pid"] = pixel.process.pid
                    ownership.refresh()
            sleep_cancellable(.5)
            snapshot = control("select_tab", tab_id=tab_ids[0]); sleep_cancellable(.25)
            snapshot = control("snapshot")
            for tab_id in tab_ids:
                snapshot = control("select_tab", tab_id=tab_id); sleep_cancellable(.25)
                snapshot = control("snapshot")
                visible = [p for t in snapshot["tabs"] for p in t["panes"] if p["mapped"]]
                if len(visible) != min(args.panes, 16) or any(not p["block"] or p["columns"] < 16 or p["rows"] < 6 for p in visible):
                    raise RuntimeError("production layout cannot retain every required visible marker area")
                if args.pixel_gate:
                    for visible_pane in visible: pixel_module.marker_rect(visible_pane, xinput.origin)
            snapshot = control("select_tab", tab_id=tab_ids[0]); sleep_cancellable(.25)
            snapshot = control("snapshot")
            children = [p["pid"] for t in snapshot["tabs"] for p in t["panes"]]
            if len(children) != args.panes or len(set(children)) != args.panes:
                raise RuntimeError("fixture child ownership/count mismatch")
            for pid in children:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                if int(fields[1]) != process.pid or not (reports / f"{pid}.json").exists():
                    raise RuntimeError("refusing to signal a process not verified as this application's fixture child")
            def child_signal(sig):
                for pid in children: ownership.send(pid, sig)
            child_signal(signal.SIGUSR1); sleep_cancellable(5)
            child_signal(signal.SIGUSR2); sleep_cancellable(1)
            snapshot = control("active")
            child_signal(signal.SIGUSR1)
            evidence["active_app_before"] = proc_sample(process.pid)
            evidence["active_collector_cpu_started_ns"] = time.process_time_ns()
            start = time.monotonic_ns(); count = 3 if args.smoke else 300
            duration = 2 if args.smoke else 60
            key_events = evidence["keys"]; drag_events = evidence["drags"]; resources = evidence["resources"]
            counters = {pid: 0 for pid in children}
            evidence["active_started_ns"] = start
            current_tab = 0; next_resource = 0
            tab_switch_seconds = .5 if args.smoke else 10
            evidence["workload"] = dict(kind="short_smoke" if args.smoke else "full_budget",
                                       tab_switch_seconds=tab_switch_seconds, tab_ids=tab_ids,
                                       key_count=count, active_seconds=duration)
            def target(snap):
                tab = snap["tabs"][snap["active_tab_index"]]
                pane = tab["panes"][0]
                if not pane["block"] or not pane["mapped"] or pane["columns"] < 16 or pane["rows"] < 6:
                    raise RuntimeError("real Block marker surface is not readable/mapped")
                return pane
            pane = target(snapshot); xinput.focus(pane["bounds"])
            divider = dict(snapshot["dividers"][0]) if args.panes > 1 else None
            for index in range(count):
                deadline = start + int((.1 + index * (duration - .2) / count) * 1e9)
                while time.monotonic_ns() < deadline:
                    sleep_cancellable(max(0, min(.005, (deadline - time.monotonic_ns()) / 1e9)))
                wanted_tab = scheduled_tab_index((time.monotonic_ns() - start) / 1e9, tab_switch_seconds, len(tab_ids))
                if wanted_tab != current_tab:
                    snapshot = control("select_tab", tab_id=tab_ids[wanted_tab]); current_tab = wanted_tab
                    sleep_cancellable(.1); snapshot = control("snapshot")
                    pane = target(snapshot); xinput.focus(pane["bounds"])
                    divider = dict(snapshot["dividers"][0])
                actual_tab = snapshot["tabs"][snapshot["active_tab_index"]]
                if actual_tab["tab_id"] != tab_ids[wanted_tab]:
                    raise RuntimeError("confirmed active tab differs from scheduled key target")
                next_sequence = counters[pane["pid"]] + 1
                pixel_event = None
                if args.pixel_gate:
                    roi, cell_width, cell_height = pixel_module.marker_rect(pane, xinput.origin)
                if pixel:
                    pixel_event = dict(id=index, pid=pane["pid"], sequence=next_sequence, rect=roi,
                                       cell_width=cell_width, cell_height=cell_height)
                    evidence["pixel_events"].append(pixel_event)
                    before = pixel.request(kind="marker_before", name=f"key-{index}", rect=roi,
                        pane_id=pane["pid"], sequence=next_sequence, cell_width=cell_width, cell_height=cell_height)
                    pixel_event["before"] = before
                    if before["matched"]: raise RuntimeError("future marker already visible before key injection")
                counters[pane["pid"]] += 1
                stamp = xinput.keypress()
                injection_end = time.monotonic_ns()
                key_events.append(dict(id=index, pid=pane["pid"], sequence=counters[pane["pid"]],
                                       scheduled_ns=deadline, injected_ns=stamp,
                                       scheduled_tab_id=tab_ids[wanted_tab], active_tab_id=actual_tab["tab_id"],
                                       target_snapshot_request=snapshot["request"],
                                       target_snapshot_ns=snapshot["monotonic_ns"]))
                if pixel:
                    pixel_event.update(injected_ns=stamp, injection_completed_ns=injection_end)
                    after = pixel.request(kind="marker_wait", name=f"key-{index}", rect=roi,
                        pane_id=pane["pid"], sequence=next_sequence, cell_width=cell_width, cell_height=cell_height)
                    pixel_event["after"] = after
                    if not after["matched"]: raise RuntimeError("expected marker did not independently reach screen pixels")
                    absent = after["last_absent"] or before["samples"][-1]
                    present = after["samples"][-1]
                    pixel_event["visibility_interval_ns"] = [absent["started_ns"], present["finished_ns"]]
                    pixel_event["latency_interval_ns"] = [max(0, absent["started_ns"] - injection_end), present["finished_ns"] - stamp]
                if divider:
                    if args.pixel_gate:
                        snapshot = control("snapshot")
                        divider = dict(snapshot["dividers"][0])
                        drag_before_snapshot = snapshot
                    delta = 12 if index % 2 == 0 else -12
                    pixel_drag = None
                    if pixel:
                        rect, expected = pixel_module.divider_rect(divider, xinput.origin)
                        pixel_drag = dict(id=index, tab_id=actual_tab["tab_id"], rect=rect, horizontal=divider["horizontal"],
                                          delta=delta, before_snapshot=drag_before_snapshot)
                        evidence["pixel_drags"].append(pixel_drag)
                        pixel_drag["before"] = pixel.request(kind="divider", name=f"drag-{index}-before", rect=rect,
                            horizontal=divider["horizontal"], expected=expected + delta // 2, radius=abs(delta) // 2 + 5)
                    stamp = xinput.drag(divider, delta)
                    if args.pixel_gate:
                        sleep_cancellable(.1)
                        after_drag = control("snapshot")
                        moved = after_drag["dividers"][0]
                        shift = pixel_module.allocation_shift(drag_before_snapshot, after_drag, delta)
                        evidence["allocation_drags"].append(dict(id=index, delta=delta, shift=shift,
                            before=drag_before_snapshot, after=after_drag))
                        for visible_tab in after_drag["tabs"]:
                            for visible_pane in visible_tab["panes"]:
                                if visible_pane["mapped"]: pixel_module.marker_rect(visible_pane, xinput.origin)
                        if pixel:
                            pixel_drag.update(injected_ns=stamp, after_snapshot=after_drag, allocation_shift=shift)
                            after = pixel.request(kind="divider", name=f"drag-{index}-after", rect=rect,
                                horizontal=divider["horizontal"], expected=expected + delta // 2, radius=abs(delta) // 2 + 5)
                            pixel_drag["after"] = after
                            pixel_shift = after["position"] - pixel_drag["before"]["position"]
                            pixel_drag["pixel_shift"] = pixel_shift
                            if abs(pixel_shift - shift) > 2: raise RuntimeError("separator pixels disagree with actual allocation")
                        snapshot = after_drag
                        pane = target(snapshot)
                    drag_events.append(dict(id=index, injected_ns=stamp, delta=delta))
                    if divider["horizontal"]: divider["x"] += delta
                    else: divider["y"] += delta
                elapsed = (time.monotonic_ns() - start) / 1e9
                if elapsed >= next_resource:
                    resources.append(proc_sample(process.pid)); next_resource += 1
            sleep_cancellable(max(0, duration - (time.monotonic_ns() - start) / 1e9))
            child_signal(signal.SIGUSR2)
            stopped = time.monotonic_ns()
            evidence["active_stopped_ns"] = stopped
            evidence["active_app_after"] = proc_sample(process.pid)
            evidence["active_collector_cpu_ns"] = time.process_time_ns() - evidence["active_collector_cpu_started_ns"]
            evidence["cpu_ticks_per_second"] = os.sysconf("SC_CLK_TCK")
            harvest(evidence, root, reports, ownership)
            sleep_cancellable(10)
            idle = control("idle"); idle_start = time.monotonic_ns()
            evidence["idle_started_ns"] = idle_start
            for _ in range(3 if args.smoke else 1200):
                resources.append(proc_sample(process.pid)); sleep_cancellable(1)
            idle_end = time.monotonic_ns(); raw = control("finish")
            evidence.update(raw_probe=raw, setup_snapshot=snapshot, keys=key_events, drags=drag_events,
                            active_started_ns=start, active_stopped_ns=stopped, idle_started_ns=idle_start,
                            idle_ended_ns=idle_end, resources=resources,
                            fixtures=[read_json(reports / f"{pid}.json") for pid in children])
            if raw["overflow"] or raw["timer_fallback"] or raw["vte_spawn_count"]:
                raise RuntimeError("native probe invalid: overflow, fallback or VTE substitution")
            painted = {(pid, seq) for pid, seq, _ in raw["painted_markers"]}
            if any((e["pid"], e["sequence"]) not in painted for e in key_events):
                raise RuntimeError("raw Block marker did not reach observed after-paint; no trust workaround permitted")
            capture_ok = True
        except Exception as error:
            evidence["raw_capture_completed"] = False
            evidence["error"] = str(error)
        finally:
            cancellation.begin_cleanup()
            if pixel is not None:
                try: pixel.close_pipes()
                except Exception as error: evidence.setdefault("finalization_errors", []).append(f"pixel pipes: {error}")
            try:
                completed = finalize(evidence, output, process, xinput, log, ownership, root, reports, capture_ok)
            finally:
                cancellation.restore()
        if not completed:
            raise RuntimeError("native capture/finalization failed; inspect preserved raw evidence")
    print("Raw native evidence saved; this is not an A04 acceptance result.")


if __name__ == "__main__": main()
