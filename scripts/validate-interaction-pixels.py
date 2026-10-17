#!/usr/bin/env python3
"""Offline short-gate evidence checks; never prove physical scanout or a budget.

Only the observer's bounded, filter-0 RGB PNG format is accepted. Replaying its
RGB decoder verifies retained before/final images, not provenance/authenticity.
Intermediate absent samples are metadata only because their PNGs are not saved.
"""
import argparse
from dataclasses import asdict
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import stat
import struct
import sys
import zlib


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


smoke = load("pixel_gate_smoke", "run-interaction-smoke.py")
client = load("pixel_gate_client", "interaction-pixel-client.py")
observer = load("pixel_gate_observer", "interaction-pixel-observer.py")
codec = load("pixel_gate_codec", "interaction-pixel-codec.py")
MAX_PNG_BYTES = 1024 * 1024
MAX_JSON_BYTES = 16 * 1024 * 1024
MAX_RESPONSE_NS = 5_000_000_000  # Collector pipe deadline, not a latency budget.
METRIC = "virtual_display_readback_not_physical_scanout"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def integer(value, label, low=0, high=None):
    require(type(value) is int and value >= low and (high is None or value <= high),
            f"{label}: invalid integer")
    return value


def number(value, label):
    require(type(value) in (int, float) and math.isfinite(value), f"{label}: invalid number")
    return value


def exact(left, right):
    """JSON equality without Python's bool/integer aliasing."""
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(exact(left[k], right[k]) for k in left)
    if isinstance(left, list):
        return len(left) == len(right) and all(exact(a, b) for a, b in zip(left, right))
    return left == right


def decode_png(data, expected_width, expected_height):
    """Bound decompression before allocating; verify every CRC and row filter."""
    require(type(data) is bytes and len(data) <= MAX_PNG_BYTES and data[:8] == b"\x89PNG\r\n\x1a\n",
            "invalid PNG signature or size")
    offset = 8
    chunks = []
    for expected in (b"IHDR", b"IDAT", b"IEND"):
        require(offset + 12 <= len(data), "truncated PNG chunk")
        size = struct.unpack_from(">I", data, offset)[0]
        kind = data[offset + 4:offset + 8]
        require(kind == expected and size <= MAX_PNG_BYTES and offset + size + 12 <= len(data),
                "unsupported or truncated PNG chunk")
        payload = data[offset + 8:offset + 8 + size]
        crc = struct.unpack_from(">I", data, offset + 8 + size)[0]
        require(crc == zlib.crc32(kind + payload) & 0xffffffff, "PNG CRC mismatch")
        chunks.append(payload)
        offset += size + 12
    require(offset == len(data) and not chunks[2], "PNG trailing data")
    require(len(chunks[0]) == 13, "invalid PNG IHDR")
    width, height, depth, color, compression, filtering, interlace = struct.unpack(">IIBBBBB", chunks[0])
    require((width, height) == (expected_width, expected_height) and
            1 <= width <= observer.MAX_WIDTH and 1 <= height <= observer.MAX_HEIGHT,
            "PNG dimensions differ from capture ROI")
    require((depth, color, compression, filtering, interlace) == (8, 2, 0, 0, 0), "unsupported PNG encoding")
    row_size = 1 + width * 3
    expected_size = row_size * height
    inflater = zlib.decompressobj()
    try:
        raw = inflater.decompress(chunks[1], expected_size + 1)
    except zlib.error as error:
        raise ValueError("invalid PNG deflate stream") from error
    require(len(raw) == expected_size and inflater.eof and not inflater.unused_data and
            not inflater.unconsumed_tail, "PNG decompression length or trailing stream mismatch")
    require(all(raw[y * row_size] == 0 for y in range(height)), "unsupported PNG row filter")
    return b"".join(raw[y * row_size + 1:(y + 1) * row_size] for y in range(height))


class Evidence:
    """Read only exact named, private, singly linked files via a pinned dirfd."""
    def __init__(self, directory):
        require(directory is not None, "observe requires an explicit evidence directory")
        supplied = Path(directory).absolute()
        require(".." not in supplied.parts, "unsafe evidence directory")
        self.fd = os.open(supplied, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            info = os.fstat(self.fd)
            require(info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) == 0o700,
                    "evidence directory must be owned and mode 0700")
            self.path = supplied.resolve(strict=True)
            resolved = os.stat(self.path)
            require((resolved.st_dev, resolved.st_ino) == (info.st_dev, info.st_ino), "evidence directory changed")
        except BaseException:
            os.close(self.fd)
            raise
        self.seen = set()

    def read(self, result, filename, rect):
        expected = filename
        require(type(result.get("final_png_path")) is str and result["final_png_path"] == expected,
                "PNG path must be its restricted relative evidence basename")
        require(filename not in self.seen, "reused PNG evidence")
        digest = result.get("final_png_sha256")
        require(type(digest) is str and re.fullmatch(r"[0-9a-f]{64}", digest), "invalid PNG digest")
        fd = os.open(filename, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=self.fd)
        with os.fdopen(fd, "rb") as stream:
            info = os.fstat(stream.fileno())
            require(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid() and
                    stat.S_IMODE(info.st_mode) == 0o600 and info.st_nlink == 1 and
                    0 < info.st_size <= MAX_PNG_BYTES, "PNG must be a bounded private owned regular file")
            data = stream.read(MAX_PNG_BYTES + 1)
        require(hashlib.sha256(data).hexdigest() == digest, "retained PNG hash mismatch")
        self.seen.add(filename)
        return decode_png(data, rect[2], rect[3])

    def close(self):
        os.close(self.fd)


def intervals(result, kind, name, rect):
    require(type(result) is dict and result.get("kind") == kind and result.get("name") == name and
            exact(result.get("rect"), rect), "observer response identity/ROI mismatch")
    samples = result.get("samples")
    require(type(samples) is list and 1 <= len(samples) <= observer.MAX_SAMPLES, "invalid capture sample count")
    integer(result.get("capture_count"), "capture_count", 1, observer.MAX_SAMPLES)
    require(result["capture_count"] == len(samples), "capture counter differs from samples")
    if kind != "marker_wait":
        require(len(samples) == 1, "single capture response has extra samples")
    previous_end = 0
    for sample in samples:
        require(type(sample) is dict, "invalid capture interval")
        start = integer(sample.get("started_ns"), "capture start", 1)
        end = integer(sample.get("finished_ns"), "capture end", 1)
        require(previous_end <= start < end, "nonpositive/overlapping/reordered capture interval")
        require(type(sample.get("matched")) is bool and type(sample.get("reason")) is str and sample["reason"],
                "invalid capture verdict")
        previous_end = end
    elapsed = integer(result.get("elapsed_ns"), "observer elapsed", 1, MAX_RESPONSE_NS)
    integer(result.get("cpu_ns"), "observer CPU")
    require(samples[-1]["finished_ns"] - samples[0]["started_ns"] <= elapsed,
            "capture span exceeds observer elapsed time")
    require(type(result.get("matched")) is bool and result["matched"] is samples[-1]["matched"] and
            result.get("reason") == samples[-1]["reason"], "final sample verdict mismatch")
    absent = [s for s in samples if not s["matched"]]
    require("last_absent" in result and exact(result["last_absent"], absent[-1] if absent else None), "last-absent metadata mismatch")
    if kind == "marker_wait":
        require(all(not s["matched"] for s in samples[:-1]) and samples[-1]["matched"],
                "wait did not stop at first positive capture")
        require(samples[-1]["started_ns"] - samples[0]["started_ns"] < observer.WAIT_NS,
                "capture starts exceed observer wait window")
    return samples


def geometry(snapshot, topology, panes, origin):
    require(type(snapshot) is dict and type(snapshot.get("tabs")) is list, "invalid geometry snapshot")
    tabs = snapshot["tabs"]
    require([t["tab_id"] for t in tabs] == list(topology), "snapshot tab topology changed")
    active = integer(snapshot.get("active_tab_index"), "active tab", 0, len(tabs) - 1)
    for index, tab in enumerate(tabs):
        require([p["pid"] for p in tab["panes"]] == topology[tab["tab_id"]], "snapshot pane topology changed")
        for pane in tab["panes"]:
            require(pane.get("block") is True and pane.get("mapped") is (index == active), "unproven visible Block topology")
            if index != active:
                continue
            integer(pane.get("columns"), "pane columns", 16)
            integer(pane.get("rows"), "pane rows", 6)
            for bounds in (pane.get("bounds"), (pane.get("pixel_geometry") or {}).get("bounds")):
                require(type(bounds) is list and len(bounds) == 4, "missing pane bounds")
                for value in bounds:
                    number(value, "pane bounds")
                require(bounds[2] > 0 and bounds[3] > 0, "nonpositive pane/terminal bounds")
            bounds = pane["bounds"]
            require(0 <= origin[0] + bounds[0] and 0 <= origin[1] + bounds[1] and
                    origin[0] + bounds[0] + bounds[2] <= observer.MAX_SCREEN_WIDTH and
                    origin[1] + bounds[1] + bounds[3] <= observer.MAX_SCREEN_HEIGHT,
                    "visible pane bounds exceed isolated screen")
            # The terminal can be scrolled beyond its viewport. Only its
            # intersection with the visible pane must retain the complete ROI.
            client.marker_rect(pane, origin)
    if panes > 1:
        require(type(snapshot.get("dividers")) is list and snapshot["dividers"], "missing real divider")
        divider = snapshot["dividers"][0]
        require(type(divider.get("horizontal")) is bool, "invalid divider orientation")
        integer(divider.get("position"), "divider position", 1)
        number(divider.get("x"), "divider x"); number(divider.get("y"), "divider y")
        client.divider_rect(divider, origin)
    return tabs[active]


def linked_snapshot(snapshot, snapshots):
    require(type(snapshot) is dict, "missing linked snapshot")
    request = integer(snapshot.get("request"), "snapshot request")
    require(request in snapshots and exact(snapshot, snapshots[request]), "geometry snapshot not in retained source record")
    return integer(snapshot.get("monotonic_ns"), "snapshot time", 1)


def marker_response(evidence, event, phase):
    kind = "marker_before" if phase == "before" else "marker_wait"
    name = f"key-{event['id']}"
    command = dict(kind=kind, name=name, rect=event["rect"], pane_id=event["pid"], sequence=event["sequence"],
                   cell_width=event["cell_width"], cell_height=event["cell_height"])
    observer.validate_command(command)
    result = event[phase]
    samples = intervals(result, kind, name, event["rect"])
    rgb = evidence.read(result, f"{name}-{'before' if phase == 'before' else 'final'}.png", event["rect"])
    decoded = codec.decode_rgb(rgb, *event["rect"][2:], expected_pane_id=event["pid"],
                               expected_sequence=event["sequence"], cell_width=event["cell_width"], cell_height=event["cell_height"])
    require(decoded.matched is (phase == "after"), "retained PNG has absent/stale/wrong/preexisting marker")
    require(result["matched"] is decoded.matched and result["reason"] == decoded.reason and
            exact(result.get("markers"), [asdict(marker) for marker in decoded.markers]), "retained RGB decoder disagrees with marker metadata")
    controls, crop = {}, None
    if phase == "after":
        # Older neighboring frames may legitimately remain in the ROI. Exercise
        # changed expectations on this independently decoded current marker only,
        # surrounded by an explicitly reported synthetic one-pixel black border.
        marker = next(m for m in decoded.markers if (m.pane_id, m.sequence) == (event["pid"], event["sequence"]))
        crop_width, crop_height = marker.width + 2, marker.height + 2
        cropped = bytearray(crop_width * crop_height * 3)
        for y in range(marker.height):
            source = ((marker.y + y) * event["rect"][2] + marker.x) * 3
            target = ((y + 1) * crop_width + 1) * 3
            cropped[target:target + marker.width * 3] = rgb[source:source + marker.width * 3]
        cropped = bytes(cropped)
        crop = dict(source_roi_rectangle=[marker.x, marker.y, marker.width, marker.height],
                    padding_pixels=1, padding_rgb=[0, 0, 0], width=crop_width, height=crop_height)
        positive = codec.decode_rgb(cropped, crop_width, crop_height, expected_pane_id=event["pid"],
            expected_sequence=event["sequence"], cell_width=event["cell_width"], cell_height=event["cell_height"])
        require(positive.matched, "decoded current-marker crop did not retain its identity")
        wrong_pid = event["pid"] + 1 if event["pid"] < codec.MAX_PANE_ID else 1
        next_sequence = event["sequence"] + 1
        require(0 < event["sequence"] < codec.MAX_SEQUENCE, "sequence cannot support bounded negative controls")
        for label, data, pid, sequence in (
                ("wrong_pane", cropped, wrong_pid, event["sequence"]),
                ("next_sequence", cropped, event["pid"], next_sequence),
                ("stale_sequence", cropped, event["pid"], event["sequence"] - 1),
                ("blank_rgb", bytes(len(cropped)), event["pid"], event["sequence"])):
            negative = codec.decode_rgb(data, crop_width, crop_height, expected_pane_id=pid,
                expected_sequence=sequence, cell_width=event["cell_width"], cell_height=event["cell_height"])
            require(not negative.matched, f"captured-pixel negative control matched: {label}")
            controls[label] = dict(rejected=True, expected_pane_id=pid, expected_sequence=sequence, reason=negative.reason)
    return samples, controls, crop


def divider_response(evidence, event, phase, expected, radius):
    result = event[phase]; name = f"drag-{event['id']}-{phase}"
    command = dict(kind="divider", name=name, rect=event["rect"], horizontal=event["horizontal"], expected=expected, radius=radius)
    observer.validate_command(command)
    samples = intervals(result, "divider", name, event["rect"])
    rgb = evidence.read(result, f"{name}-final.png", event["rect"])
    decoded = observer.detect_divider(rgb, *event["rect"][2:], horizontal=event["horizontal"], expected=expected, radius=radius)
    require(decoded["matched"] is True and all(exact(result.get(k), v) for k, v in decoded.items()),
            "retained PNG divider decoder disagrees with metadata")
    return samples[0], decoded["position"], rgb


def validate(raw, panes, binary_sha256, evidence_directory=None):
    """Validate one short observe/control record; no native execution occurs."""
    integer(panes, "panes", 1, 32)
    require(panes in (1, 8, 32) and type(raw) is dict, "unsupported scenario")
    require(type(raw.get("panes")) is int and type(binary_sha256) is str and
            re.fullmatch(r"[0-9a-f]{64}", binary_sha256), "invalid scenario/binary identity")
    smoke.validate(raw, panes, binary_sha256)
    mode = raw.get("pixel_mode")
    require(mode in ("observe", "control") and raw.get("pixel_metric") == METRIC, "not a short pixel-gate record")
    origin = raw.get("window_origin")
    require(type(origin) is list and len(origin) == 2, "missing window origin")
    for value in origin: require(-64 <= number(value, "widget root origin") <= 1664, "widget root origin outside bound")
    for field in ("pixel_events", "pixel_drags", "allocation_drags", "drags"):
        require(type(raw.get(field)) is list, f"missing {field} array")
    observed = mode == "observe"
    require(len(raw["pixel_events"]) == (3 if observed else 0), "wrong pixel key event count")
    expected_drags = 0 if panes == 1 else 3
    require(len(raw["allocation_drags"]) == len(raw["drags"]) == expected_drags and
            len(raw["pixel_drags"]) == (expected_drags if observed else 0), "wrong drag counts; one pane requires explicit empty arrays")
    input_pid = integer(raw.get("input_helper_pid"), "input helper PID", 1)
    require(input_pid in {p["pid"] for p in raw["cleanup"]["owned_identities"]}, "input helper not in cleaned owned tree")
    observer_pid = raw.get("pixel_observer_pid")
    require("pixel_observer_pid" in raw, "missing observer identity declaration")
    if observed:
        integer(observer_pid, "observer PID", 1)
        require(observer_pid in {p["pid"] for p in raw["cleanup"]["owned_identities"]}, "observer not in cleaned owned process tree")
    else:
        require(observer_pid is None, "control launched an observer")
    topology = {t["tab_id"]: [p["pid"] for p in t["panes"]] for t in raw["setup_snapshot"]["tabs"]}
    fixture_pids = {pid for values in topology.values() for pid in values}
    require(input_pid not in fixture_pids and input_pid != observer_pid, "input helper aliases fixture or observer")
    require(not observed or observer_pid not in fixture_pids, "observer aliases fixture PID")
    snapshots = {s["request"]: s for s in raw["snapshots"]}
    evidence = Evidence(evidence_directory) if observed else None
    identity = raw.get("surface_identity")
    require(type(identity) is dict, "missing owned surface identity")
    frames = raw.get("input_coordinate_frames")
    require(type(frames) is list and frames, "missing bounded input frame acknowledgements")
    frame_times = {}; previous_frame = -1; previous_time = -1
    for frame in frames:
        request = integer(frame.get("snapshot_request"), "frame snapshot request")
        configured = integer(frame.get("configured_ns"), "configured frame timestamp", 1)
        require(request > previous_frame and configured > previous_time and request in snapshots, "invalid frame acknowledgement order")
        snapshot = snapshots[request]
        require(exact(frame.get("native_frame"), snapshot.get("native_frame")) and
                exact(frame.get("surface_identity"), identity), "frame does not match observed native window")
        calculated = client.native_coordinate_frame(snapshot, identity)
        client.coordinates(frame.get("widget_root_origin"), 2)
        require(frame.get("widget_root_origin") == calculated == origin and configured >= snapshot["monotonic_ns"],
                "incorrect composed widget/surface/root frame")
        frame_times[request] = configured; previous_frame = request; previous_time = configured
    summaries = []; divider_proofs = []
    drag_tabs = set()
    previous_end = 0
    try:
        for index, key in enumerate(raw["keys"]):
            target = snapshots[key["target_snapshot_request"]]
            target_time = linked_snapshot(target, snapshots)
            require(target["request"] in frame_times and frame_times[target["request"]] <= key["injected_ns"], "key has no fresh configured native frame")
            target_time = frame_times[target["request"]]
            active = geometry(target, topology, panes, origin)
            pane = next(p for p in active["panes"] if p["pid"] == key["pid"])
            require(key["injected_ns"] > previous_end, "next key precedes prior completed observation/drag")
            key_end = key["injected_ns"]
            if observed:
                event = raw["pixel_events"][index]
                for field in ("id", "pid", "sequence", "injected_ns"):
                    require(exact(event.get(field), key[field]), f"pixel key {field} differs from injected event")
                rect, cw, ch = client.marker_rect(pane, origin)
                require(exact(event.get("rect"), rect) and exact(event.get("cell_width"), cw) and exact(event.get("cell_height"), ch),
                        "pixel marker ROI differs from owned visible pane")
                injection_end = integer(event.get("injection_completed_ns"), "injection completion", 1)
                require(injection_end >= key["injected_ns"], "injection completed before it started")
                before, _, _ = marker_response(evidence, event, "before")
                after, controls, crop = marker_response(evidence, event, "after")
                require(max(target_time, previous_end) <= before[0]["started_ns"] and
                        before[-1]["finished_ns"] <= key["injected_ns"] <= injection_end <= after[0]["started_ns"],
                        "marker capture does not bracket injection")
                final_end = after[-1]["finished_ns"]
                require(0 < final_end - key["injected_ns"] <= MAX_RESPONSE_NS, "unbounded key observation interval")
                absent = event["after"]["last_absent"] or before[-1]
                visibility = [absent["started_ns"], final_end]
                latency = [max(0, absent["started_ns"] - injection_end), final_end - key["injected_ns"]]
                require(exact(event.get("visibility_interval_ns"), visibility) and exact(event.get("latency_interval_ns"), latency),
                        "fabricated visibility/latency interval")
                summaries.append(dict(id=index, pid=key["pid"], sequence=key["sequence"],
                    retained_png_latency_interval_ns=[0, final_end - key["injected_ns"]],
                    metadata_latency_interval_ns=latency, intermediate_absence_png_verified=False,
                    captured_pixel_negative_controls=controls, captured_pixel_negative_control_crop=crop))
                key_end = final_end
            previous_end = key_end
            if panes == 1:
                continue
            drag = raw["drags"][index]; allocation = raw["allocation_drags"][index]
            for item in (drag, allocation):
                require(exact(item.get("id"), index), "duplicate/reordered/missing drag ID")
                require(exact(item.get("delta"), 12 if index % 2 == 0 else -12), "wrong scheduled drag delta")
            stamp = integer(drag.get("injected_ns"), "drag injection", 1)
            before_snapshot, after_snapshot = allocation["before"], allocation["after"]
            before_time = linked_snapshot(before_snapshot, snapshots); after_time = linked_snapshot(after_snapshot, snapshots)
            require(before_snapshot["request"] in frame_times and after_snapshot["request"] in frame_times, "drag missing configured native frame")
            before_time = frame_times[before_snapshot["request"]]; after_time = frame_times[after_snapshot["request"]]
            before_tab = geometry(before_snapshot, topology, panes, origin)
            after_tab = geometry(after_snapshot, topology, panes, origin)
            require(before_tab["tab_id"] == after_tab["tab_id"] == key["active_tab_id"], "drag not on the key's active tab")
            require(key_end <= before_time <= stamp < after_time and before_snapshot["request"] < after_snapshot["request"],
                    "allocation observations do not bracket drag injection")
            model = client.expected_drag(before_snapshot, drag["delta"], origin)
            require(exact(allocation.get("model"), model), "recorded drag prediction differs from measured pre-injection model")
            require(exact(allocation.get('pointer_observation'), dict(start_root=model['pointer_start_root'],
                        end_root=model['pointer_end_root'])), "actual pointer observations differ from predicted drag")
            plans=raw.get('divider_sampling_plans')
            require(type(plans) is list and len(plans)==3, "missing pre-injection sampling plans")
            planned=plans[index]
            require(exact(planned.get('id'),index) and exact(planned.get('snapshot_request'),before_snapshot['request']), "sampling plan identity mismatch")
            chosen_ns=integer(planned.get('chosen_ns'),'sampling choice timestamp',1)
            require(before_time<=chosen_ns<=stamp, "sampling choice was not before injection")
            sampling_plan=client.divider_sampling_plan(before_snapshot,origin)
            require(exact(planned.get('plan'),sampling_plan), "recorded pre-injection sampling choice mismatch")
            require(exact(allocation.get('sampling_plan'),sampling_plan), "sampling plan is not the pre-input geometry-derived choice")
            sampling_after=client.verify_divider_sampling_after(sampling_plan,after_snapshot,origin)
            require(exact(allocation.get('sampling_after'),sampling_after), "sampling lost same adjacent content after movement")
            predicted_shift = model['expected_allocation_shift']
            shift = client.allocation_shift(before_snapshot, after_snapshot, predicted_shift)
            require(exact(allocation.get("shift"), shift), "recorded allocation shift differs from geometry")
            first, last = before_snapshot["dividers"][0], after_snapshot["dividers"][0]
            require(first.get("paned_identity") == last.get("paned_identity"), "drag changed measured paned identity")
            axis, cross = ("x", "y") if first["horizontal"] else ("y", "x")
            require(abs((last[axis] - first[axis]) - shift) <= 1 and abs(last[cross] - first[cross]) <= 1,
                    "divider screen coordinates disagree with allocated motion")
            drag_tabs.add(before_tab["tab_id"])
            previous_end = after_time
            if observed:
                event = raw["pixel_drags"][index]
                for field in ("id", "delta", "injected_ns"):
                    require(exact(event.get(field), drag[field]), f"pixel drag {field} mismatch")
                require(exact(event.get("tab_id"), before_tab["tab_id"]) and
                        exact(event.get("before_snapshot"), before_snapshot) and exact(event.get("after_snapshot"), after_snapshot) and
                        exact(event.get("allocation_shift"), shift), "pixel drag allocation linkage mismatch")
                divider = before_snapshot["dividers"][0]
                rect, expected = sampling_plan["rect"], sampling_plan["expected"]
                require(exact(event.get("sampling_plan"),sampling_plan), "pixel ROI plan differs from allocation plan")
                require(exact(event.get("rect"), rect) and exact(event.get("horizontal"), divider["horizontal"]), "pixel divider ROI mismatch")
                center, radius = expected + predicted_shift // 2, abs(predicted_shift) // 2 + 5
                before_capture, before_position, before_rgb = divider_response(evidence, event, "before", center, radius)
                after_capture, after_position, after_rgb = divider_response(evidence, event, "after", center, radius)
                motion_proof = observer.divider_motion(before_rgb, after_rgb, *rect[2:], horizontal=event["horizontal"],
                                        expected=expected, delta=predicted_shift)
                require(chosen_ns <= before_capture["started_ns"] < before_capture["finished_ns"] <= stamp and
                        after_time <= after_capture["started_ns"] < after_capture["finished_ns"] and
                        after_capture["finished_ns"] - stamp <= MAX_RESPONSE_NS, "divider captures do not bracket drag/geometry")
                pixel_shift = after_position - before_position
                require(pixel_shift * drag["delta"] > 0 and abs(pixel_shift - shift) <= 2 and
                        exact(event.get("pixel_shift"), pixel_shift), "divider pixels did not follow allocated motion")
                divider_proofs.append(dict(id=index, **motion_proof))
                previous_end = after_capture["finished_ns"]
        if panes > 1:
            require(drag_tabs == set(topology), "drag validation did not exercise every active topology tab")
    finally:
        if evidence is not None:
            evidence.close()
    return dict(status="short_gate_only", short_gate_only=True, mode=mode, panes=panes,
                budget_acceptance=False, physical_scanout_proven=False, pixel_metric=METRIC,
                pixel_key_count=len(summaries), allocation_drag_count=expected_drags,
                divider_drag="single_pane_no_divider" if panes == 1 else "allocation_and_pixels" if observed else "allocation_only",
                keys=summaries, divider_proofs=divider_proofs, missing_gates=["physical scanout", "full-duration idle attribution", "paired full-budget matrix"])


def read_json(path):
    require(path.stat().st_size <= MAX_JSON_BYTES, "raw JSON exceeds byte bound")
    with path.open("rb") as stream:
        data = stream.read(MAX_JSON_BYTES + 1)
    require(len(data) <= MAX_JSON_BYTES, "raw JSON exceeds byte bound")
    return json.loads(data, object_pairs_hook=observer.no_duplicate_keys,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite JSON")))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("raw", type=Path)
    parser.add_argument("--panes", type=int, choices=(1, 8, 32), required=True)
    parser.add_argument("--binary-sha256", required=True)
    parser.add_argument("--evidence-directory", type=Path)
    args = parser.parse_args(argv)
    try:
        result = validate(read_json(args.raw), args.panes, args.binary_sha256,
                          args.evidence_directory or args.raw.with_suffix(".pixels"))
    except (ValueError, TypeError, KeyError, IndexError, StopIteration, OSError, OverflowError, RecursionError) as error:
        print(json.dumps(dict(status="invalid_evidence", budget_acceptance=False, physical_scanout_proven=False, error=str(error))))
        return 2
    print(json.dumps(result, indent=2, allow_nan=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
