#!/usr/bin/env python3
"""Hosted short correctness smoke only; never normalize into budget acceptance."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


MISSING_GATES = ["independent pixel subset and display latency", "complete idle source attribution",
                 "event-to-geometry drag mapping", "paired full-duration matrix aggregation"]


def require(condition, message):
    if not condition:
        raise ValueError(message)


def validate(raw, panes, binary_sha256):
    require(raw.get("schema") == "raw-native-block-v1", "unexpected raw evidence schema")
    require(raw.get("smoke") is True and raw.get("budget_acceptance") is False, "not a smoke-only record")
    require(raw.get("panes") == panes and raw.get("binary_sha256") == binary_sha256, "scenario/binary mismatch")
    require(raw.get("raw_capture_completed") is True and raw.get("evidence_write_succeeded") is True,
            "capture or evidence publication incomplete")
    require(not raw.get("cancellation_count") and not raw.get("error") and not raw.get("finalization_errors")
            and not raw.get("harvest_errors"), "failed or cancelled capture")
    cleanup = raw.get("cleanup", {})
    require(cleanup.get("verified_empty") is True and cleanup.get("remaining_pids") == []
            and cleanup.get("errors") == [], "owned tree exit not proven")
    probe = raw["raw_probe"]
    require(probe.get("vte_spawn_count") == 0 and type(probe.get("vte_spawn_count")) is int,
            "VTE substitution counter invalid")
    require(probe.get("timer_fallback") is False and probe.get("overflow") is False, "invalid observer path")
    ids = probe["dispatch_ids"]; parents = probe["consumer_dispatch_ids"]
    require(ids and parents and len(set(ids)) == len(ids), "missing actual OwnedPty dispatch/Block consumption")
    require(len(ids) == len(probe["dispatch_ms"]) and len(parents) == len(probe["consumer_ms"]), "sample relation invalid")
    require(set(parents) <= set(ids) and probe["consumed_bytes"] > 0, "unattributed Block consumption")
    require(raw["setup_snapshot"].get("fixture_program_matches") is True, "production fixture program not verified")
    tabs = raw["setup_snapshot"]["tabs"]
    require(len(tabs) == (2 if panes == 32 else 1), "unsupported tab topology")
    require([len(t["panes"]) for t in tabs] == ([16, 16] if panes == 32 else [panes]), "unsupported pane topology")
    all_panes = [p for t in tabs for p in t["panes"]]
    require(all(p["block"] for p in all_panes), "non-Block pane")
    require(sum(p["mapped"] for p in all_panes) == min(panes, 16), "visible/background topology mismatch")
    fixture_pids = {p["pid"] for p in all_panes}
    owned = {p["pid"] for p in cleanup["owned_identities"]}
    require(len(fixture_pids) == panes and fixture_pids <= owned, "fixture ownership incomplete")
    require(len(raw["fixtures"]) == panes and all(raw["fixtures"]), "fixture reports incomplete")
    workload = raw["workload"]
    require(workload["kind"] == "short_smoke" and workload["tab_switch_seconds"] == .5
            and workload["key_count"] == 3 and workload["active_seconds"] == 2, "wrong smoke workload")
    topology_ids = [t["tab_id"] for t in tabs]
    require(len(set(topology_ids)) == len(topology_ids) and workload["tab_ids"] == topology_ids,
            "workload tab identities differ from topology")
    topology_pids = {t["tab_id"]: {p["pid"] for p in t["panes"]} for t in tabs}
    snapshots = {}
    previous_request = -1; previous_stamp = -1
    for snapshot in raw["snapshots"]:
        request = snapshot["request"]; stamp = snapshot["monotonic_ns"]
        require(type(request) is int and request > previous_request and type(stamp) is int
                and stamp >= previous_stamp, "duplicate or nonchronological snapshots")
        snapshots[request] = snapshot
        previous_request = request; previous_stamp = stamp
    keys = raw["keys"]
    require(len(keys) == 3, "wrong smoke key count")
    expected = {}; sequences = {}; observed_tabs = set()
    previous_injection = -1; previous_target_request = -1
    for event_id, key in enumerate(keys):
        require(type(key["id"]) is int and key["id"] == event_id, "duplicate or reordered key event ID")
        pid = key["pid"]; sequence = key["sequence"]; injected = key["injected_ns"]
        require(type(pid) is int and pid in fixture_pids and type(sequence) is int
                and sequence == sequences.get(pid, 0) + 1, "duplicate or skipped per-child key sequence")
        require(type(injected) is int and injected > previous_injection, "nonchronological key injection")
        require(type(key["scheduled_ns"]) is int and key["scheduled_ns"] <= injected, "key injected before schedule")
        request = key["target_snapshot_request"]
        require(type(request) is int and request in snapshots and request >= previous_target_request,
                "missing or nonchronological target snapshot")
        snapshot = snapshots[request]
        active = snapshot["tabs"][snapshot["active_tab_index"]]
        tab_id = active["tab_id"]
        require(tab_id in topology_pids and tab_id == key["active_tab_id"] == key["scheduled_tab_id"],
                "active tab not proven")
        require(pid in topology_pids[tab_id], "key pane differs from original tab topology")
        require(snapshot["monotonic_ns"] == key["target_snapshot_ns"] <= injected, "invalid target observation")
        require(any(p["pid"] == pid and p["mapped"] and p["block"] for p in active["panes"]),
                "key not addressed to confirmed active Block pane")
        observed_tabs.add(tab_id); sequences[pid] = sequence
        expected[(pid, sequence)] = injected
        previous_injection = injected; previous_target_request = request
    require(observed_tabs == set(topology_ids), "short smoke did not exercise every topology tab")
    painted = {}; previous_paint = -1
    for pid, sequence, stamp in probe["painted_markers"]:
        require(type(pid) is int and type(sequence) is int and type(stamp) is int, "invalid paint identity/time")
        identity = (pid, sequence)
        require(identity in expected and identity not in painted, "unexpected or duplicate paint marker")
        require(stamp >= expected[identity] and stamp >= previous_paint, "pre-injection or nonchronological paint")
        painted[identity] = stamp; previous_paint = stamp
    require(painted.keys() == expected.keys(), "raw echo proxy missing")
    require(len(raw["drags"]) == (0 if panes == 1 else 3), "smoke drag count invalid (one-pane N/A)")
    return dict(status="raw_echo_proxy_and_cleanup_pass", budget_acceptance=False, panes=panes,
                visible_panes=min(panes, 16), background_panes=max(0, panes - 16),
                divider_drag="not_applicable" if panes == 1 else "injected_only_geometry_gate_open",
                missing_gates=MISSING_GATES)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for role in ("baseline", "candidate"):
        parser.add_argument(f"--{role}-binary", type=Path, required=True)
        parser.add_argument(f"--{role}-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    for role in ("baseline", "candidate"):
        require(re.fullmatch(r"[0-9a-f]{40}", getattr(args, role + "_sha")), "source must be an immutable commit")
    output = args.output.resolve(); output.mkdir(parents=True, exist_ok=True)
    require(hashlib.sha256(args.baseline_binary.read_bytes()).digest() !=
            hashlib.sha256(args.candidate_binary.read_bytes()).digest(), "refusing identical paired binaries")
    result = dict(schema="a04-short-smoke-v1", budget_acceptance=False, missing_gates=MISSING_GATES, runs=[])
    summary = output / "smoke-summary.json"
    wrapper = Path(__file__).with_name("run-interaction-budget.sh")
    try:
        for panes in (1, 8, 32):
            for role in ("baseline", "candidate"):
                binary = getattr(args, role + "_binary").resolve(strict=True)
                digest = hashlib.sha256(binary.read_bytes()).hexdigest()
                record = dict(role=role, panes=panes, source_sha=getattr(args, role + "_sha"), binary_sha256=digest,
                              status="started", budget_acceptance=False)
                result["runs"].append(record)
                summary.write_text(json.dumps(result, indent=2) + "\n")
                raw_path = output / f"{role}-{panes}.json"
                with (output / f"{role}-{panes}.collector.log").open("w") as log:
                    subprocess.run(["bash", str(wrapper), str(binary), str(panes), str(raw_path), "--smoke"],
                                   stdout=log, stderr=subprocess.STDOUT, check=True)
                record.update(validate(json.loads(raw_path.read_text()), panes, digest))
        result["status"] = "all_short_smokes_passed_proxy_only"
    except Exception as error:
        result["status"] = "failed"; result["error"] = str(error)
        raise
    finally:
        summary.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
