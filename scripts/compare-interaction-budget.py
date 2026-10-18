#!/usr/bin/env python3
"""Compare one paired native run. Synthetic tests are not runtime evidence."""
import argparse
import json
import math
from pathlib import Path
import re

ENV_KEYS = (
    "machine_boot_id", "cpu", "kernel", "renderer", "backend", "font_sha256",
    "scale", "geometry", "gtk", "vte", "glib", "build_profile", "compiler",
    "lock_sha256", "fixture_sha256", "instrumentation_sha256", "workload_sha256",
)
SERIES = ("input_to_after_paint_ms", "drag_to_after_paint_ms", "dispatch_ms", "consumer_ms")
ENV_DIGEST_KEYS = tuple(key for key in ENV_KEYS if key.endswith("_sha256"))
INTEGER_METRICS = (
    "owned_pty_dispatch_count", "reader_ctx_consumer_count", "vte_spawn_count",
    "instrumentation_idle_callbacks", "rss_peak_bytes", "context_switches",
    "idle_callbacks", "idle_pty_empty_polls", "output_bytes", "consumed_bytes",
    "backlog_after_drain_bytes", "queue_high_water_bytes", "key_expected", "drag_expected",
)
MAX_FILE_BYTES = 16 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise ValueError(message)


def number(value, label, positive=False):
    require(type(value) in (int, float), f"{label}: invalid number")
    try:
        finite = math.isfinite(value)
    except OverflowError:
        # JSON integers need not fit the finite float domain used by timings.
        finite = False
    require(finite, f"{label}: invalid number")
    require(value > 0 if positive else value >= 0, f"{label}: invalid range")
    return value


def integer(value, label):
    require(type(value) is int and value >= 0, f"{label}: expected nonnegative integer")
    return value


def digest(value, length, label):
    require(isinstance(value, str) and re.fullmatch(rf"[0-9a-fA-F]{{{length}}}", value),
            f"{label}: expected {length}-character hexadecimal digest")


def validate_callbacks(case, label):
    """Each dispatch may have zero or many consumers; arrays retain exact lineage.

    dispatch_ids aligns with dispatch_ms and is unique within this scenario.
    consumer_dispatch_ids aligns with consumer_ms and may repeat a dispatch ID.
    Counters cover the same phase/window as their complete, bounded sample arrays.
    A whole output run still needs both production paths to have been observed.
    """
    dispatch_ids = case.get("dispatch_ids")
    require(isinstance(dispatch_ids, list) and
            len(dispatch_ids) == len(case["dispatch_ms"]) == case["owned_pty_dispatch_count"],
            f"{label}: dispatch IDs, samples and counter disagree")
    require(all(type(value) is int and value >= 0 for value in dispatch_ids),
            f"{label}: invalid dispatch ID")
    require(len(set(dispatch_ids)) == len(dispatch_ids), f"{label}: duplicate dispatch IDs")
    parent_ids = case.get("consumer_dispatch_ids")
    require(isinstance(parent_ids, list) and
            len(parent_ids) == len(case["consumer_ms"]) == case["reader_ctx_consumer_count"],
            f"{label}: consumer parent IDs, samples and counter disagree")
    require(all(type(value) is int and value >= 0 for value in parent_ids),
            f"{label}: invalid consumer parent dispatch ID")
    dispatch_set = set(dispatch_ids)
    require(all(value in dispatch_set for value in parent_ids),
            f"{label}: consumer references an unobserved dispatch")


def percentile(values, quantile):
    """Nearest-rank percentile; caller validates nonempty finite values."""
    return sorted(values)[max(0, math.ceil(len(values) * quantile) - 1)]


def summary(values):
    return {"count": len(values), "p50": percentile(values, .50),
            "p95": percentile(values, .95), "p99": percentile(values, .99),
            "max": max(values)}


def validate(run):
    require(isinstance(run, dict), "run must be an object")
    require(type(run.get("schema")) is int and run["schema"] == 1, "unsupported schema")
    require(run.get("evidence") == "native_after_paint_with_pixel_subset", "native pixel subset oracle required")
    digest(run.get("source_sha"), 40, "source_sha")
    digest(run.get("binary_sha256"), 64, "binary_sha256")
    require(isinstance(run.get("pair_id"), str) and run["pair_id"].strip(), "missing pair_id")
    env = run.get("environment", {})
    require(isinstance(env, dict), "environment must be an object")
    for key in ENV_KEYS:
        require(key in env and env[key] is not None and env[key] != "", f"missing environment {key}")
    for key in ENV_DIGEST_KEYS:
        digest(env[key], 64, f"environment.{key}")
    require(env["build_profile"] == "release", "release build required")
    require(env["backend"] == "x11" and env["renderer"] == "cairo", "unsupported rendering contract")
    cases = run.get("scenarios")
    require(isinstance(cases, list) and len(cases) == 1, "require one paired scenario per runner job")
    require(all(isinstance(c, dict) for c in cases), "scenario must be an object")
    require(type(cases[0].get("panes")) is int and cases[0]["panes"] in (1, 8, 32), "require 1, 8 or 32 panes")
    for c in cases:
        label = str(c["panes"])
        require(c.get("surface") == "block", "A04 requires Block OwnedPty surface")
        require(c.get("topology") == ("two_tabs_16_each" if c["panes"] == 32 else "one_tab"),
                "unsupported application topology")
        for key in INTEGER_METRICS:
            integer(c.get(key), f"{label}.{key}")
        for key in ("owned_pty_dispatch_count", "reader_ctx_consumer_count"):
            require(c[key] > 0, "missing production path counters")
        require(c.get("vte_spawn_count") == 0, "VTE substitution forbidden")
        require(c.get("instrumentation_idle_callbacks") == 0, "observer contaminated idle")
        require(c.get("transport") == "eventfd", f"{label}: fallback is a separate scenario")
        for key in ("warmup_seconds", "output_seconds", "idle_seconds", "cpu_seconds"):
            number(c.get(key), f"{label}.{key}")
        require(c["warmup_seconds"] >= 5 and c["output_seconds"] >= 60, "incomplete active phase")
        require(c["idle_seconds"] >= 1200, "incomplete idle phase")
        require(c.get("pixel_oracle_passed") is True, "pixel oracle did not pass")
        require(c.get("input_semantics_passed") is True, "input semantics did not pass")
        require(c.get("idle_source_audit_passed") is True, "idle callback attribution incomplete")
        if c["panes"] == 1:
            require(c.get("drag_not_applicable") == "single_pane_no_divider", "1-pane drag N/A required")
            require(c.get("drag_expected") == 0 and c.get("drag_observed_ids") == [] and
                    c.get("drag_to_after_paint_ms") == [], "1-pane must not fabricate drag events")
        for kind in (("key",) if c["panes"] == 1 else ("key", "drag")):
            expected = c.get(f"{kind}_expected")
            ids = c.get(f"{kind}_observed_ids")
            require(type(expected) is int and 300 <= expected <= 10000, "invalid event count")
            require(isinstance(ids, list) and len(ids) == expected and
                    all(type(v) is int for v in ids) and sorted(ids) == list(range(expected)),
                    f"{label}: missing or duplicate {kind} events")
        for key in (SERIES[:1] + SERIES[2:] if c["panes"] == 1 else SERIES):
            values = c.get(key)
            require(isinstance(values, list) and 1 <= len(values) <= 100000, f"invalid {key} samples")
            for value in values:
                number(value, key, positive=key.endswith("to_after_paint_ms"))
        require(len(c[SERIES[0]]) == c["key_expected"], "key timing count mismatch")
        require(len(c[SERIES[1]]) == c["drag_expected"], "drag timing count mismatch")
        validate_callbacks(c, label)
        target = c["panes"] * 256 * 1024 * c["output_seconds"]
        for key in ("output_bytes", "consumed_bytes"):
            actual = c[key]
            require(target * .99 <= actual <= target * 1.01, "output workload did not meet fixed rate")
        require(c.get("backlog_after_drain_bytes") == 0, "undrained output backlog")
        for key in ("blocked_write_ms", "max_write_lag_ms"):
            number(c.get(key), key)
        allocations = c.get("pane_allocations")
        require(isinstance(allocations, list) and len(allocations) == c["panes"], "missing pane geometry")
        for allocation in allocations:
            require(isinstance(allocation, dict) and
                    type(allocation.get("columns")) is int and allocation["columns"] >= 16 and
                    type(allocation.get("rows")) is int and allocation["rows"] >= 6 and
                    type(allocation.get("mapped")) is bool and
                    (allocation.get("marker_visible") is True or not allocation["mapped"]), "unreadable pane marker area")
        require(sum(a["mapped"] for a in allocations) == min(c["panes"], 16), "visible pane topology mismatch")
        pixel_events = c.get("pixel_validation_events")
        require(isinstance(pixel_events, list) and all(type(value) is int for value in pixel_events) and
                pixel_events == list(range(0, 300, 30)), "pixel subset incomplete")
    return {c["panes"]: c for c in cases}


def compare(baseline, candidate):
    left, right = validate(baseline), validate(candidate)
    require(baseline["pair_id"] == candidate["pair_id"], "different run pairs")
    for key in ENV_KEYS:
        require(baseline["environment"][key] == candidate["environment"][key],
                f"incomparable environment: {key}")
    result = {"schema": 1, "baseline": baseline["source_sha"],
              "candidate": candidate["source_sha"], "pass": True,
              "acceptance": "presentation_proxy_only_display_acceptance_open", "scenarios": []}
    require(left.keys() == right.keys(), "pane count mismatch")
    for panes in sorted(left):
        a, b = left[panes], right[panes]
        stats = {k: {"baseline": summary(a[k]), "candidate": summary(b[k])}
                 for k in SERIES if k != "drag_to_after_paint_ms" or panes != 1}
        if panes == 1:
            stats["drag_to_after_paint_ms"] = {"not_applicable": "single_pane_no_divider"}
        input_ratio = stats[SERIES[0]]["candidate"]["p95"] / stats[SERIES[0]]["baseline"]["p95"]
        number(input_ratio, f"{panes}.input_p95_ratio")
        gates = {"input_p95_within_15_percent": input_ratio <= 1.15,
                 "dispatch_target_16ms": max(b["dispatch_ms"]) <= 16,
                 "consumer_target_16ms": max(b["consumer_ms"]) <= 16,
                 "baseline_idle_no_pty_polling": a["idle_pty_empty_polls"] == 0,
                 "idle_no_pty_polling": b["idle_pty_empty_polls"] == 0}
        row = {"panes": panes, "gates": gates, "input_p95_ratio": input_ratio,
               "timings": stats, "dispatch_over_16ms": sum(v > 16 for v in b["dispatch_ms"]),
               "diagnostics": {k: {"baseline": a[k], "candidate": b[k], "delta": b[k] - a[k]}
                               for k in ("rss_peak_bytes", "cpu_seconds", "context_switches", "idle_callbacks")}}
        result["scenarios"].append(row)
        result["pass"] &= all(gates.values())
    return result


def read(path):
    require(path.stat().st_size <= MAX_FILE_BYTES, "input file too large")
    def reject_duplicates(pairs):
        obj = {}
        for key, value in pairs:
            require(key not in obj, f"duplicate JSON key: {key}")
            obj[key] = value
        return obj
    return json.loads(path.read_text(), object_pairs_hook=reject_duplicates)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    args = parser.parse_args()
    try:
        result = compare(read(args.baseline), read(args.candidate))
        output = json.dumps(result, indent=2, allow_nan=False)
    except (ValueError, TypeError, OSError, KeyError, OverflowError) as error:
        print(json.dumps({"schema": 1, "pass": False, "invalid_evidence": str(error)}))
        return 2
    print(output)
    return 0 if result["pass"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
