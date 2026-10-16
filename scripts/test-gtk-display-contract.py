#!/usr/bin/env python3
"""Check runner aggregation with fake child commands, never GTK or D-Bus.

This is shell-control-flow evidence only, not a display-backed test pass.
"""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


RUNNER = Path(__file__).with_name("test-gtk-display.sh")
ENTER = "enter_ownership::tests::queued_review_write_claims_enter_at_both_backend_boundaries"
HELD = "block_view::tests::held_enter_history_recall_does_not_submit_the_recalled_command"
A11Y = "organism_ui::tests::live_settings_reuse_body_preserve_focus_and_stop_disabled_timers"
IME_TESTS = {
    "block_view::chat::tests::native_ime_commit_cancel_and_independent_ptys",
    "block_view::chat::tests::native_ime_send_button_diagnostic",
}
VARIANT_KEYS = (
    "ANVIL_ENTER_MODE", "ANVIL_QA_PREWARM_ENTER", "ANVIL_QA_INHERITED_ENTER",
    "ANVIL_QA_FILE_DROP", "ANVIL_QA_DUAL_ENTER", "ANVIL_QA_FRESH_ENTER",
)

FAKE_CHILD = r'''
import json
import os
from pathlib import Path
import sys

name, args = Path(sys.argv[0]).name, sys.argv[1:]
env = os.environ
tests = json.loads(Path(env["CONTRACT_TESTS"]).read_text())
with open(env["CONTRACT_LOG"], "a") as log:
    log.write(json.dumps({"command": name, "args": args, "pid": os.getpid(),
                         "env": {key: value for key, value in env.items()
                                 if key.startswith(("XDG_", "ANVIL_QA_")) or key in
                                 ["ANVIL_ENTER_MODE", "GTK_A11Y", "GTK_IM_MODULE", "RUST_TEST_THREADS",
                                  "GDK_BACKEND", "GSK_RENDERER", "LIBGL_ALWAYS_SOFTWARE",
                                  "NO_AT_BRIDGE"]}}) + "\n")
if name == "dbus-run-session":
    assert args[0].startswith("--config-file=") and args[1] == "--", args
    os.execvp(args[2], args[2:])
elif name == "xvfb-run":
    assert args[:2] == ["--auto-servernum", "--server-args=-screen 0 1280x800x24 -nolisten tcp"], args
    os.execvp(args[2], args[2:])
elif name == "cargo":
    if args == ["test", "--locked", "--", "--ignored", "--list"]:
        if env["CONTRACT_MODE"] == "list-failure":
            sys.exit(31)
        for test in tests[:-1] if env["CONTRACT_MODE"] == "missing-test" else tests:
            print(test + ": test")
    else:
        assert args[:2] == ["test", "--locked"], args
        assert args[3:] == ["--", "--ignored", "--exact", "--nocapture"], args
        assert args[2] in tests, args
        if env["CONTRACT_MODE"] == "failures":
            if args[2] == tests[0]:
                sys.exit(17)
            if all(env.get(key) == "1" for key in
                   ["ANVIL_QA_FILE_DROP", "ANVIL_QA_INHERITED_ENTER", "ANVIL_QA_DUAL_ENTER"]):
                sys.exit(23)
            if args[2] == tests[-1]:
                sys.exit(29)
else:
    raise AssertionError("Unexpected fake child: " + name)
'''


def expected_cases(tests):
    cases = []
    for test in tests:
        cases.append((test, {}))
        if test == ENTER:
            for inherited in ("0", "1"):
                for dual in ("0", "1"):
                    cases.append((test, {"ANVIL_QA_FILE_DROP": "1",
                                        "ANVIL_QA_INHERITED_ENTER": inherited,
                                        "ANVIL_QA_DUAL_ENTER": dual}))
            cases.append((test, {"ANVIL_QA_FILE_DROP": "1", "ANVIL_QA_FRESH_ENTER": "1"}))
            cases.append((test, {"ANVIL_QA_DUAL_ENTER": "1"}))
            for dual in ("0", "1"):
                cases.append((test, {"ANVIL_QA_INHERITED_ENTER": "1", "ANVIL_QA_DUAL_ENTER": dual}))
        if test == HELD:
            for mode in ("review-mouse-only-fresh", "review-mouse-only-keypad-fresh",
                         "review-mouse-only-keypad-pane-fresh"):
                cases.append((test, {"ANVIL_ENTER_MODE": mode, "ANVIL_QA_PREWARM_ENTER": "1",
                                    "ANVIL_QA_INHERITED_ENTER": "1"}))
    return cases


def main():
    source = RUNNER.read_text()
    tests = re.search(r"^tests=\(\n(.*?)\n\)", source, re.M | re.S).group(1).split()
    assert len(tests) == len(set(tests)) and {ENTER, HELD, A11Y, *IME_TESTS}.issubset(tests)
    expected = expected_cases(tests)
    assert len(expected) == len(tests) + 11
    bash = shutil.which("bash")
    with tempfile.TemporaryDirectory(prefix="anvil-gtk-runner-contract-") as directory:
        root = Path(directory)
        bindir = root / "bin"
        bindir.mkdir()
        (root / "scripts").mkdir()
        runner = root / "scripts" / RUNNER.name
        runner.write_text(source)
        policy = root / "share/dbus-1/session.conf"
        policy.parent.mkdir(parents=True)
        policy.write_text("Fake policy; no daemon is started.\n")
        # A closed PATH prevents any real Cargo, D-Bus, or Xvfb fallback.
        for command in ("dirname", "mktemp", "mkdir", "chmod", "rm", "grep"):
            (bindir / command).symlink_to(shutil.which(command))
        for command in ("cargo", "dbus-run-session", "dbus-daemon", "xvfb-run"):
            child = bindir / command
            child.write_text(f"#!{sys.executable}\n" + FAKE_CHILD)
            child.chmod(0o755)
        inventory = root / "tests.json"
        inventory.write_text(json.dumps(tests))
        env = {key: value for key, value in os.environ.items() if key not in VARIANT_KEYS}
        env.pop("BASH_ENV", None)
        env.update(GTK_IM_MODULE="contract-inherited", PATH=str(bindir), CONTRACT_TESTS=str(inventory), TMPDIR=str(root))

        for mode, status in (("success", 0), ("failures", 1), ("missing-test", 1),
                             ("list-failure", 31), ("missing-wrapper", 1)):
            log = root / f"{mode}.jsonl"
            if mode == "missing-wrapper":
                (bindir / "xvfb-run").unlink()
            result = subprocess.run([bash, str(runner)], env={**env, "CONTRACT_MODE": mode,
                                    "CONTRACT_LOG": str(log)}, text=True, capture_output=True)
            output = result.stdout + result.stderr
            assert result.returncode == status, (mode, result.returncode, output)
            records = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            calls = [record for record in records if record["command"] == "cargo"
                     and "--list" not in record["args"]]
            if mode in ("success", "failures"):
                assert len(calls) == len(expected), (mode, len(calls), len(expected))
                assert len({call["pid"] for call in calls}) == len(calls)
                assert [record["command"] for record in records[1:]] == \
                    ["dbus-run-session", "xvfb-run", "cargo"] * len(expected)
                for call, (test, variant) in zip(calls, expected):
                    assert call["args"][2] == test
                    actual = {key: call["env"][key] for key in VARIANT_KEYS if key in call["env"]}
                    assert actual == variant, (test, actual, variant)
                    for key, value in {"GTK_A11Y": "test" if test == A11Y else "none",
                                       "GTK_IM_MODULE": "simple" if test in IME_TESTS else "contract-inherited",
                                       "RUST_TEST_THREADS": "1", "GDK_BACKEND": "x11",
                                       "GSK_RENDERER": "cairo", "LIBGL_ALWAYS_SOFTWARE": "1",
                                       "NO_AT_BRIDGE": "1"}.items():
                        assert call["env"][key] == value, (test, key)
                failures = 3 if mode == "failures" else 0
                assert f"GTK display regressions passed: {len(expected) - failures}/{len(expected)}." in output
                if failures:
                    assert "Failed GTK display regressions (3):" in output
                    for label, code in ((tests[0], 17),
                                        (f"{ENTER} (file-drop Enter ownership: inherited=1 dual=1)", 23),
                                        (tests[-1], 29)):
                        assert f"  {label} (exit {code})" in output
                else:
                    assert "Failed GTK display regressions" not in output
            else:
                assert not calls and "GTK display regressions passed:" not in output
                assert len(records) == (0 if mode == "missing-wrapper" else 1)
                if mode == "missing-test":
                    assert f"GTK display regression is not registered: {tests[-1]}" in output
                if mode == "missing-wrapper":
                    assert "xvfb-run is required for display-backed tests" in output
            for record in records:
                for key in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                            "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"):
                    assert not Path(record["env"][key]).exists(), (mode, key, "was not cleaned up")
            assert not list(root.glob("tmp.*")), (mode, "temporary XDG tree was not cleaned up")
            print(f"PASS: fake-command runner contract: {mode}")
    print(f"Checked {len(tests)} base cases and 11 variants; no native GTK/display execution.")


if __name__ == "__main__":
    main()
