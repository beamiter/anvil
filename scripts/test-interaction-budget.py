#!/usr/bin/env python3
"""Synthetic contract tests. No native runtime evidence is generated."""
import copy
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("budget", Path(__file__).with_name("compare-interaction-budget.py"))
budget = importlib.util.module_from_spec(spec)
spec.loader.exec_module(budget)


def fixture(panes=8):
    # Syntactically valid digests and measurements are only synthetic contract data.
    env = {k: "fixed" for k in budget.ENV_KEYS}
    env.update(build_profile="release", backend="x11", renderer="cairo")
    env.update({key: "b" * 64 for key in budget.ENV_DIGEST_KEYS})
    c = dict(panes=panes, surface="block", topology="two_tabs_16_each" if panes == 32 else "one_tab",
             owned_pty_dispatch_count=300, reader_ctx_consumer_count=300,
             dispatch_ids=list(range(300)), consumer_dispatch_ids=list(range(300)),
             vte_spawn_count=0, instrumentation_idle_callbacks=0, transport="eventfd", warmup_seconds=5, output_seconds=60,
             idle_seconds=1200, rss_peak_bytes=1024, cpu_seconds=1, context_switches=10,
             idle_callbacks=2, idle_pty_empty_polls=0, pixel_oracle_passed=True,
             input_semantics_passed=True, idle_source_audit_passed=True,
             output_bytes=panes * 256 * 1024 * 60, consumed_bytes=panes * 256 * 1024 * 60,
             backlog_after_drain_bytes=0, blocked_write_ms=0, max_write_lag_ms=0,
             queue_high_water_bytes=32768,
             pane_allocations=[dict(columns=16, rows=6, marker_visible=i < 16, mapped=i < 16) for i in range(panes)],
             pixel_validation_events=list(range(0, 300, 30)))
    for k in ("key", "drag"):
        c[k + "_expected"] = 300
        c[k + "_observed_ids"] = list(range(300))
    c.update({k: [10.0] * 300 for k in budget.SERIES})
    if panes == 1:
        c.update(drag_expected=0, drag_observed_ids=[], drag_to_after_paint_ms=[],
                 drag_not_applicable="single_pane_no_divider")
    return dict(schema=1, evidence="native_after_paint_with_pixel_subset", source_sha="a" * 40,
                binary_sha256="c" * 64, pair_id="synthetic", environment=env, scenarios=[c])


class Contract(unittest.TestCase):
    def test_valid_each_pane_count_and_proxy_label(self):
        for n in (1, 8, 32):
            r = budget.compare(fixture(n), fixture(n))
            self.assertTrue(r["pass"])
            self.assertEqual(r["acceptance"], "presentation_proxy_only_display_acceptance_open")

    def test_percentile_nearest_rank(self):
        self.assertEqual(budget.percentile(list(range(1, 101)), .99), 99)
        self.assertEqual(budget.percentile([7], .50), 7)

    def test_latency_boundary_and_regression(self):
        a, b = fixture(), fixture()
        b["scenarios"][0]["input_to_after_paint_ms"] = [11.5] * 300
        self.assertTrue(budget.compare(a, b)["pass"])
        b["scenarios"][0]["input_to_after_paint_ms"] = [11.501] * 300
        self.assertFalse(budget.compare(a, b)["pass"])

    def test_callback_max_not_hidden_by_percentile(self):
        for key in ("dispatch_ms", "consumer_ms"):
            with self.subTest(key=key):
                a, b = fixture(), fixture()
                b["scenarios"][0][key][-1] = 16
                self.assertTrue(budget.compare(a, b)["pass"])
                b["scenarios"][0][key][-1] = 16.01
                self.assertFalse(budget.compare(a, b)["pass"])

    def test_dispatch_can_have_zero_or_multiple_consumers(self):
        for parents in (list(range(299)), list(range(300)) + [0], [0] * 300, [299]):
            with self.subTest(consumer_count=len(parents), first_parent=parents[0]):
                b = fixture()
                c = b["scenarios"][0]
                c["consumer_dispatch_ids"] = parents
                c["consumer_ms"] = [0.01] * len(parents)
                c["reader_ctx_consumer_count"] = len(parents)
                result = budget.compare(fixture(), b)
                self.assertTrue(result["pass"])
                self.assertEqual(result["scenarios"][0]["timings"]["consumer_ms"]["candidate"]["count"],
                                 len(parents))

    def test_dispatch_ids_need_not_be_contiguous_or_sorted(self):
        b = fixture()
        c = b["scenarios"][0]
        c["dispatch_ids"] = list(range(999, 699, -1))
        c["consumer_dispatch_ids"] = [999] * 300
        self.assertTrue(budget.compare(fixture(), b)["pass"])

    def test_callback_lineage_and_counters_must_be_complete(self):
        changes = dict(dispatch_ids=list(range(299)),
                       consumer_dispatch_ids=list(range(299)),
                       dispatch_ms=[1] * 299, consumer_ms=[1] * 299,
                       owned_pty_dispatch_count=299, reader_ctx_consumer_count=299)
        for key, value in changes.items():
            with self.subTest(key=key):
                b = fixture(); b["scenarios"][0][key] = value
                with self.assertRaises(ValueError): budget.validate(b)
        for key in ("dispatch_ids", "consumer_dispatch_ids", "owned_pty_dispatch_count",
                    "reader_ctx_consumer_count"):
            with self.subTest(missing=key):
                b = fixture(); del b["scenarios"][0][key]
                with self.assertRaises(ValueError): budget.validate(b)

    def test_duplicate_dispatch_and_unknown_consumer_parent_are_rejected(self):
        for key, value in (("dispatch_ids", 1), ("consumer_dispatch_ids", 300)):
            with self.subTest(key=key):
                b = fixture(); b["scenarios"][0][key][0] = value
                with self.assertRaises(ValueError): budget.validate(b)

    def test_callback_ids_are_nonnegative_integers(self):
        for key in ("dispatch_ids", "consumer_dispatch_ids"):
            for value in (True, False, 0.0, -1, "0", None, [], {}):
                with self.subTest(key=key, value=value):
                    b = fixture(); b["scenarios"][0][key][0] = value
                    with self.assertRaises(ValueError): budget.validate(b)

    def test_full_workload_requires_observed_consumer_path(self):
        b = fixture()
        b["scenarios"][0].update(reader_ctx_consumer_count=0, consumer_ms=[], consumer_dispatch_ids=[])
        with self.assertRaises(ValueError): budget.validate(b)

    def test_idle_poll_is_failure(self):
        a, b = fixture(), fixture()
        b["scenarios"][0]["idle_pty_empty_polls"] = 1
        self.assertFalse(budget.compare(a, b)["pass"])

    def test_busy_baseline_cannot_qualify_relative_pass(self):
        a, b = fixture(), fixture()
        a["scenarios"][0]["idle_pty_empty_polls"] = 1
        a["scenarios"][0]["input_to_after_paint_ms"] = [100.0] * 300
        result = budget.compare(a, b)
        self.assertFalse(result["pass"])
        gates = result["scenarios"][0]["gates"]
        self.assertTrue(gates["input_p95_within_15_percent"])
        self.assertTrue(gates["idle_no_pty_polling"])
        self.assertFalse(gates["baseline_idle_no_pty_polling"])

    def test_baseline_idle_observer_and_audit_must_be_healthy(self):
        for key, value in (("instrumentation_idle_callbacks", 1), ("idle_source_audit_passed", False)):
            with self.subTest(key=key):
                a = fixture(); a["scenarios"][0][key] = value
                with self.assertRaises(ValueError): budget.compare(a, fixture())

    def test_environment_pair_and_pane_mismatch(self):
        for key in budget.ENV_KEYS:
            a, b = fixture(), fixture()
            b["environment"][key] = "d" * 64 if key in budget.ENV_DIGEST_KEYS else "different"
            with self.assertRaises(ValueError):
                budget.compare(a, b)
        b = fixture(); b["pair_id"] = "other"
        with self.assertRaises(ValueError): budget.compare(fixture(), b)
        with self.assertRaises(ValueError): budget.compare(fixture(), fixture(32))

    def test_source_binary_and_environment_digest_shapes(self):
        fields = [(None, "source_sha", 40), (None, "binary_sha256", 64)]
        fields += [("environment", key, 64) for key in budget.ENV_DIGEST_KEYS]
        for container, key, length in fields:
            for value in (None, True, 0, "synthetic", "a" * (length - 1), "a" * (length + 1),
                          "g" * length, "a" * (length - 1) + "\n", " " + "a" * length):
                with self.subTest(field=key, value=value):
                    b = fixture()
                    (b[container] if container else b)[key] = value
                    with self.assertRaises(ValueError): budget.validate(b)
            with self.subTest(field=key, case="uppercase hex accepted"):
                b = fixture()
                (b[container] if container else b)[key] = "ABCDEF01" * (length // 8)
                budget.validate(b)

    def test_all_counters_and_byte_metrics_are_exact_integers(self):
        for key in budget.INTEGER_METRICS:
            for value in (True, False, 0.0, 1.0, -1, None, "0"):
                with self.subTest(key=key, value=value):
                    b = fixture(); b["scenarios"][0][key] = value
                    with self.assertRaises(ValueError): budget.validate(b)

    def test_boolean_or_float_cannot_alias_zero_in_one_pane_drag(self):
        for value in (False, 0.0):
            b = fixture(1); b["scenarios"][0]["drag_expected"] = value
            with self.assertRaises(ValueError): budget.validate(b)

    def test_schema_and_pixel_ids_reject_boolean_aliases(self):
        for value in (True, 1.0):
            b = fixture(); b["schema"] = value
            with self.assertRaises(ValueError): budget.validate(b)
        for value in (False, 0.0):
            b = fixture(); b["scenarios"][0]["pixel_validation_events"][0] = value
            with self.assertRaises(ValueError): budget.validate(b)

    def test_incomplete_and_invalid_evidence(self):
        changes = dict(surface="vte", owned_pty_dispatch_count=0, vte_spawn_count=1,
                       instrumentation_idle_callbacks=1, topology="fake_grid", idle_seconds=1199, warmup_seconds=4, output_seconds=59,
                       pixel_oracle_passed=False, input_semantics_passed=False,
                       idle_source_audit_passed=False, output_bytes=1, consumed_bytes=1,
                       backlog_after_drain_bytes=1, pixel_validation_events=[],
                       pane_allocations=[], transport="timer", key_observed_ids=[0] * 300,
                       drag_observed_ids=list(range(299)), key_expected=True)
        for key, value in changes.items():
            with self.subTest(key=key):
                b = fixture(); b["scenarios"][0][key] = value
                with self.assertRaises(ValueError): budget.compare(fixture(), b)

    def test_one_pane_rejects_fabricated_drag(self):
        b = fixture(1); b["scenarios"][0]["drag_to_after_paint_ms"] = [1]
        with self.assertRaises(ValueError): budget.validate(b)

    def test_nonfinite_negative_boolean_zero_latency(self):
        for v in (float("nan"), float("inf"), -float("inf"), -1, True, 0):
            b = fixture(); b["scenarios"][0]["input_to_after_paint_ms"][0] = v
            with self.assertRaises(ValueError): budget.validate(b)

    def test_all_numeric_measurements_reject_unrepresentable_and_nonfinite_values(self):
        scalar_fields = ("warmup_seconds", "output_seconds", "idle_seconds", "cpu_seconds",
                         "blocked_write_ms", "max_write_lag_ms")
        for value in (10 ** 400, -(10 ** 400), float("nan"), float("inf"), -float("inf")):
            for key in scalar_fields + budget.SERIES:
                with self.subTest(key=key, value=value):
                    b = fixture()
                    if key in budget.SERIES:
                        b["scenarios"][0][key][0] = value
                    else:
                        b["scenarios"][0][key] = value
                    with self.assertRaisesRegex(ValueError, "invalid number"):
                        budget.validate(b)

    def test_finite_p95_inputs_cannot_produce_a_nonfinite_ratio(self):
        a, b = fixture(), fixture()
        a["scenarios"][0]["input_to_after_paint_ms"] = [float.fromhex("0x0.0000000000001p-1022")] * 300
        b["scenarios"][0]["input_to_after_paint_ms"] = [sys.float_info.max] * 300
        budget.validate(a)
        budget.validate(b)
        with self.assertRaisesRegex(ValueError, "input_p95_ratio: invalid number"):
            budget.compare(a, b)

    def test_finite_positive_extremes_preserve_relative_comparison(self):
        for value in (float.fromhex("0x0.0000000000001p-1022"), sys.float_info.min,
                      sys.float_info.max, 10 ** 300):
            with self.subTest(value=value):
                a, b = fixture(), fixture()
                for run in (a, b):
                    run["scenarios"][0]["input_to_after_paint_ms"] = [value] * 300
                result = budget.compare(a, b)
                self.assertTrue(result["pass"])
                self.assertEqual(result["scenarios"][0]["input_p95_ratio"], 1.0)
                json.dumps(result, allow_nan=False)

    def test_cli_serialization_failure_is_structured_invalid_evidence(self):
        for value in (float("nan"), float("inf"), -float("inf")):
            with self.subTest(value=value):
                output = io.StringIO()
                with mock.patch.object(sys, "argv", ["compare", "baseline", "candidate"]), \
                        mock.patch.object(budget, "read", return_value=fixture()), \
                        mock.patch.object(budget, "compare", return_value={"pass": True, "unexpected": value}), \
                        contextlib.redirect_stdout(output):
                    status = budget.main()
                self.assertEqual(status, 2)
                result = json.loads(output.getvalue())
                self.assertEqual(result["schema"], 1)
                self.assertFalse(result["pass"])
                self.assertIn("invalid_evidence", result)

    def test_cli_numeric_edges_have_json_output_and_correct_status(self):
        with tempfile.TemporaryDirectory() as d:
            left, right = Path(d) / "baseline.json", Path(d) / "candidate.json"
            invalid_values = (10 ** 400, -(10 ** 400), float("nan"), float("inf"), -float("inf"))
            cases = [(value, 10.0, 2) for value in invalid_values]
            cases += [(10.0, value, 2) for value in invalid_values]
            cases += [(float.fromhex("0x0.0000000000001p-1022"), sys.float_info.max, 2),
                      (sys.float_info.max, sys.float_info.max, 0), (10.0, 11.5, 0), (10.0, 11.501, 1)]
            for baseline_value, candidate_value, expected_code in cases:
                with self.subTest(baseline=baseline_value, candidate=candidate_value):
                    a, b = fixture(), fixture()
                    a["scenarios"][0]["input_to_after_paint_ms"] = [baseline_value] * 300
                    b["scenarios"][0]["input_to_after_paint_ms"] = [candidate_value] * 300
                    left.write_text(json.dumps(a))
                    right.write_text(json.dumps(b))
                    process = subprocess.run([sys.executable, str(Path(budget.__file__)), str(left), str(right)],
                                             capture_output=True, text=True, check=False)
                    self.assertEqual(process.returncode, expected_code, process.stderr)
                    self.assertEqual(process.stderr, "")
                    result = json.loads(process.stdout)
                    self.assertEqual(result["schema"], 1)
                    self.assertEqual(result["pass"], expected_code == 0)
                    if expected_code == 2:
                        self.assertIsInstance(result["invalid_evidence"], str)
                        self.assertTrue(result["invalid_evidence"])
                    else:
                        self.assertEqual(result["acceptance"], "presentation_proxy_only_display_acceptance_open")

    def test_cli_derived_workload_overflow_keeps_finite_input_contract(self):
        with tempfile.TemporaryDirectory() as d:
            left, right = Path(d) / "baseline.json", Path(d) / "candidate.json"
            for duration, expected_code in ((10 ** 300, 0), (10 ** 308, 2)):
                with self.subTest(duration=duration):
                    # Both inputs are finite. Only the latter's derived byte
                    # target cannot be converted for the existing 1% range.
                    self.assertEqual(budget.number(duration, "output_seconds"), duration)
                    a, b = fixture(), fixture()
                    for run in (a, b):
                        c = run["scenarios"][0]
                        c["output_seconds"] = duration
                        c["output_bytes"] = c["consumed_bytes"] = c["panes"] * 256 * 1024 * duration
                    left.write_text(json.dumps(a))
                    right.write_text(json.dumps(b))
                    process = subprocess.run([sys.executable, str(Path(budget.__file__)), str(left), str(right)],
                                             capture_output=True, text=True, check=False)
                    self.assertEqual(process.returncode, expected_code, process.stderr)
                    self.assertEqual(process.stderr, "")
                    result = json.loads(process.stdout)
                    self.assertEqual(result["schema"], 1)
                    self.assertEqual(result["pass"], expected_code == 0)
                    if expected_code == 2:
                        self.assertIsInstance(result["invalid_evidence"], str)
                        self.assertTrue(result["invalid_evidence"])
                    else:
                        self.assertEqual(result["acceptance"], "presentation_proxy_only_display_acceptance_open")

    def test_missing_and_duplicate_json(self):
        b = fixture(); del b["environment"]["compiler"]
        with self.assertRaises(ValueError): budget.validate(b)
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "bad.json"; p.write_text('{"schema":1,"schema":1}')
            with self.assertRaises(ValueError): budget.read(p)

    def test_cli_distinguishes_proxy_pass_failure_and_invalid_evidence(self):
        with tempfile.TemporaryDirectory() as d:
            left, right = Path(d) / "baseline.json", Path(d) / "candidate.json"
            left.write_text(json.dumps(fixture()))
            valid = fixture()
            busy = copy.deepcopy(valid); busy["scenarios"][0]["idle_pty_empty_polls"] = 1
            invalid = copy.deepcopy(valid); invalid["binary_sha256"] = "synthetic"
            for candidate, expected_code in ((valid, 0), (busy, 1), (invalid, 2)):
                with self.subTest(expected_code=expected_code):
                    right.write_text(json.dumps(candidate))
                    process = subprocess.run([sys.executable, str(Path(budget.__file__)), str(left), str(right)],
                                             capture_output=True, text=True, check=False)
                    self.assertEqual(process.returncode, expected_code, process.stderr)
                    result = json.loads(process.stdout)
                    self.assertEqual(result["pass"], expected_code == 0)
                    if expected_code == 2:
                        self.assertIn("invalid_evidence", result)
                    else:
                        self.assertEqual(result["acceptance"], "presentation_proxy_only_display_acceptance_open")


if __name__ == "__main__":
    unittest.main()
