#!/usr/bin/env python3
"""Synthetic retained-PNG contracts only. No display, sockets, Cargo or native app."""
import copy
from dataclasses import asdict
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch
import zlib

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("pixel_validator", Path(__file__).with_name("validate-interaction-pixels.py"))
gate = importlib.util.module_from_spec(spec); sys.modules[spec.name] = gate; spec.loader.exec_module(gate)
smoke_fixture = gate.load("pixel_smoke_fixture", "test-interaction-smoke.py")
DIGEST = "a" * 64


def marker_rgb(width, height, pid=None, sequence=1, cw=2, ch=4):
    rgb = bytearray(bytes([37, 41, 43]) * (width * height))
    if pid is not None:
        for cell, symbol in enumerate(gate.codec.marker_symbols(pid, sequence)):
            for y in range(2, 2 + ch):
                for x in range(2 + cell * cw, 2 + (cell + 1) * cw):
                    offset = (y * width + x) * 3
                    rgb[offset:offset + 3] = bytes(gate.codec.PALETTE[symbol])
    return bytes(rgb)


def divider_rgb(width, height, position, horizontal=True):
    return b"".join(bytes([220, 220, 220] if (x if horizontal else y) >= position else [20, 20, 20])
                    for y in range(height) for x in range(width))


def retain(directory, filename, rgb, width, height):
    data = gate.observer.encode_png_rgb(rgb, width, height)
    path = directory / filename
    path.write_bytes(data); path.chmod(0o600)
    return dict(final_png_path=filename, final_png_sha256=hashlib.sha256(data).hexdigest())


def response(directory, kind, name, rect, start, end, rgb, pid=None, sequence=None, position=32, horizontal=True):
    width, height = rect[2:]
    if kind == "divider":
        decoded = gate.observer.detect_divider(rgb, width, height, horizontal=horizontal, expected=position, radius=5)
    else:
        decoded = gate.codec.decode_rgb(rgb, width, height, expected_pane_id=pid, expected_sequence=sequence, cell_width=2, cell_height=4)
        decoded = dict(matched=decoded.matched, reason=decoded.reason, markers=[asdict(m) for m in decoded.markers])
    sample = dict(started_ns=start, finished_ns=end, matched=decoded["matched"], reason=decoded["reason"])
    result = dict(kind=kind, name=name, rect=rect, **decoded, samples=[sample],
                  last_absent=None if decoded["matched"] else copy.deepcopy(sample), capture_count=1, elapsed_ns=1000, cpu_ns=10)
    result.update(retain(directory, name + ("-before.png" if kind == "marker_before" else "-final.png"), rgb, width, height))
    return result


def valid(directory, panes=8, mode="observe", horizontal=True):
    raw = smoke_fixture.valid(panes)
    raw.update(pixel_mode=mode, pixel_metric=gate.METRIC, window_origin=[0, 0],
               input_helper_pid=998, pixel_observer_pid=999 if mode == "observe" else None, pixel_events=[], pixel_drags=[], allocation_drags=[], drags=[])
    raw["cleanup"]["owned_identities"].append(dict(pid=998))
    if mode == "observe": raw["cleanup"]["owned_identities"].append(dict(pid=999))
    base_tabs = copy.deepcopy(raw["setup_snapshot"]["tabs"])
    for tab in base_tabs:
        for i, pane in enumerate(tab["panes"]):
            bounds = [20 + (i % 4) * 380, 80 + (i // 4) * 200, 380, 200]
            pane.update(columns=80, rows=24, bounds=bounds,
                        pixel_geometry=dict(cell_width=2, cell_height=4, bounds=list(bounds)))
    raw["snapshots"] = []
    for index, key in enumerate(raw["keys"]):
        base = 1_000_000_000 + index * 100_000_000
        active = 1 if panes == 32 and index == 1 else 0
        tabs = copy.deepcopy(base_tabs)
        for i, tab in enumerate(tabs):
            for pane in tab["panes"]: pane["mapped"] = i == active
        snapshot = dict(request=index * 3 + 1, monotonic_ns=base + 10, tabs=tabs, active_tab_index=active,
                        fixture_program_matches=True, dividers=[] if panes == 1 else [dict(x=780, y=400, horizontal=horizontal, position=760)])
        raw["snapshots"].append(snapshot)
        key.update(scheduled_ns=base + 11, injected_ns=base + 30, target_snapshot_request=snapshot["request"], target_snapshot_ns=base + 10)
        pane = tabs[active]["panes"][0]
        if mode == "observe":
            rect, cw, ch = gate.client.marker_rect(pane, raw["window_origin"])
            event = dict(id=index, pid=key["pid"], sequence=key["sequence"], rect=rect, cell_width=cw, cell_height=ch,
                         injected_ns=base + 30, injection_completed_ns=base + 31)
            event["before"] = response(directory, "marker_before", f"key-{index}", rect, base + 20, base + 25,
                                       marker_rgb(*rect[2:]), key["pid"], key["sequence"])
            event["after"] = response(directory, "marker_wait", f"key-{index}", rect, base + 40, base + 45,
                                      marker_rgb(*rect[2:], key["pid"], key["sequence"]), key["pid"], key["sequence"])
            event.update(visibility_interval_ns=[base + 20, base + 45], latency_interval_ns=[0, 15])
            raw["pixel_events"].append(event)
        if panes == 1:
            continue
        delta = 12 if index % 2 == 0 else -12
        before = copy.deepcopy(snapshot); before.update(request=index * 3 + 2, monotonic_ns=base + 50)
        after = copy.deepcopy(before); after.update(request=index * 3 + 3, monotonic_ns=base + 80)
        after["dividers"][0]["position"] += delta
        after["dividers"][0]["x" if horizontal else "y"] += delta
        for field in (after["tabs"][active]["panes"][0]["bounds"], after["tabs"][active]["panes"][0]["pixel_geometry"]["bounds"]):
            field[2 if horizontal else 3] += delta
        raw["snapshots"] += [before, after]
        raw["drags"].append(dict(id=index, injected_ns=base + 70, delta=delta))
        raw["allocation_drags"].append(dict(id=index, delta=delta, shift=delta, before=before, after=after))
        if mode == "observe":
            rect, expected = gate.client.divider_rect(before["dividers"][0], raw["window_origin"])
            event = dict(id=index, tab_id=tabs[active]["tab_id"], rect=rect, horizontal=horizontal, delta=delta,
                         before_snapshot=before, after_snapshot=after, injected_ns=base + 70, allocation_shift=delta, pixel_shift=delta)
            event["before"] = response(directory, "divider", f"drag-{index}-before", rect, base + 60, base + 65,
                                        divider_rgb(*rect[2:], expected, horizontal), position=expected, horizontal=horizontal)
            event["after"] = response(directory, "divider", f"drag-{index}-after", rect, base + 90, base + 95,
                                       divider_rgb(*rect[2:], expected + delta, horizontal), position=expected + delta, horizontal=horizontal)
            raw["pixel_drags"].append(event)
    raw["setup_snapshot"] = copy.deepcopy(raw["snapshots"][-1])
    raw["raw_probe"]["painted_markers"] = [[k["pid"], k["sequence"], k["injected_ns"] + 2] for k in raw["keys"]]
    return raw


class PixelGate(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.directory.chmod(0o700)
        self.raw = valid(self.directory)

    def check(self, raw=None):
        raw = self.raw if raw is None else raw
        return gate.validate(raw, raw["panes"], DIGEST, self.directory)

    def reject(self, mutate, error=ValueError):
        raw = copy.deepcopy(self.raw); mutate(raw)
        with self.assertRaises(error): self.check(raw)

    def test_all_topologies_observed_and_control_are_short_only(self):
        for panes in (1, 8, 32):
            for mode in ("observe", "control"):
                with self.subTest(panes=panes, mode=mode), tempfile.TemporaryDirectory() as directory:
                    raw = valid(Path(directory), panes, mode)
                    result = gate.validate(raw, panes, DIGEST, Path(directory) if mode == "observe" else None)
                    self.assertEqual(result["status"], "short_gate_only")
                    self.assertFalse(result["budget_acceptance"])
                    self.assertFalse(result["physical_scanout_proven"])
                    self.assertEqual(result["pixel_key_count"], 3 if mode == "observe" else 0)
                    if panes == 1: self.assertEqual(result["divider_drag"], "single_pane_no_divider")
                    for key in result["keys"]:
                        self.assertEqual(key["retained_png_latency_interval_ns"], [0, 15])
                        self.assertEqual(set(key["captured_pixel_negative_controls"]), {"wrong_pane", "next_sequence", "stale_sequence", "blank_rgb"})
                        self.assertTrue(all(v["rejected"] for v in key["captured_pixel_negative_controls"].values()))
                        self.assertEqual(key["captured_pixel_negative_control_crop"], dict(
                            source_roi_rectangle=[2, 2, 62, 4], padding_pixels=1,
                            padding_rgb=[0, 0, 0], width=64, height=6))

    def test_vertical_divider_roundtrip(self):
        self.check(valid(self.directory, horizontal=False))

    def test_requires_existing_smoke_cleanup_and_echo_validation(self):
        self.reject(lambda r: r["cleanup"].update(verified_empty=False))
        self.reject(lambda r: r["raw_probe"].update(painted_markers=[]))
        self.reject(lambda r: r.update(budget_acceptance=True))

    def test_key_ids_pid_sequence_and_order_must_match(self):
        for field, value in (("id", 1), ("id", False), ("pid", 101), ("sequence", 2), ("injected_ns", 1)):
            with self.subTest(field=field, value=value):
                self.reject(lambda r: r["pixel_events"][0].update({field: value}))
        self.reject(lambda r: r["pixel_events"].pop())
        self.reject(lambda r: r["pixel_events"].reverse())
        self.reject(lambda r: r["pixel_events"].__setitem__(1, r["pixel_events"][0]))

    def test_future_marker_must_be_absent_before_input(self):
        event = self.raw["pixel_events"][0]
        event["before"] = response(self.directory, "marker_before", "key-0", event["rect"], 1_000_000_020, 1_000_000_025,
                                   marker_rgb(*event["rect"][2:], event["pid"], event["sequence"]), event["pid"], event["sequence"])
        with self.assertRaisesRegex(ValueError, "preexisting"): self.check()

    def test_stale_wrong_or_blank_retained_marker_rejected_even_with_updated_hash(self):
        for pid, sequence in ((100, 0), (101, 1), (100, 2), (None, 1)):
            with self.subTest(pid=pid, sequence=sequence):
                raw = copy.deepcopy(self.raw); event = raw["pixel_events"][0]
                event["after"].update(retain(self.directory, "key-0-final.png", marker_rgb(*event["rect"][2:], pid, sequence), *event["rect"][2:]))
                with self.assertRaisesRegex(ValueError, "absent/stale/wrong"): self.check(raw)

    def test_decoder_metadata_must_match_retained_rgb(self):
        self.reject(lambda r: r["pixel_events"][0]["after"]["markers"][0].update(sequence=2))
        self.reject(lambda r: r["pixel_events"][0]["after"]["markers"][0].update(sequence=True))
        self.reject(lambda r: r["pixel_events"][0]["after"].update(reason="not-real"))

    def test_current_marker_crop_controls_allow_stale_neighbor_in_whole_roi(self):
        event = self.raw["pixel_events"][0]; width, height = event["rect"][2:]
        rgb = bytearray(marker_rgb(width, height, event["pid"], event["sequence"]))
        stale = marker_rgb(width, height, event["pid"], event["sequence"] - 1)
        for y in range(4):
            rgb[((9 + y) * width + 2) * 3:((9 + y) * width + 64) * 3] = stale[((2 + y) * width + 2) * 3:((2 + y) * width + 64) * 3]
        event["after"] = response(self.directory, "marker_wait", "key-0", event["rect"], 1_000_000_040, 1_000_000_045,
                                   bytes(rgb), event["pid"], event["sequence"])
        self.assertEqual(len(event["after"]["markers"]), 2)
        result = self.check()["keys"][0]
        self.assertTrue(result["captured_pixel_negative_controls"]["stale_sequence"]["rejected"])
        self.assertEqual(result["captured_pixel_negative_control_crop"]["source_roi_rectangle"], [2, 2, 62, 4])

    def test_response_identity_roi_and_owned_pane_geometry(self):
        for field, value in (("name", "key-1"), ("kind", "divider"), ("rect", [0, 0, 66, 16])):
            self.reject(lambda r: r["pixel_events"][0]["after"].update({field: value}))
        self.reject(lambda r: r["pixel_events"][0].update(rect=[0, 0, 66, 16]))
        for origin in ([True, 0], [-1, 0], [1600, 0], [0], [0.0, 0]):
            self.reject(lambda r: r.update(window_origin=origin))

    def test_negative_scrolled_terminal_origin_uses_valid_clipped_intersection(self):
        raw = valid(self.directory, 1)
        for snapshot in raw["snapshots"]:
            snapshot["tabs"][0]["panes"][0]["pixel_geometry"]["bounds"] = [-20, -40, 420, 320]
        self.assertEqual(self.check(raw)["pixel_key_count"], 3)
        # Terminal has positive size but its intersection cannot hold the code.
        raw["snapshots"][0]["tabs"][0]["panes"][0]["pixel_geometry"]["bounds"] = [-20, -40, 50, 130]
        with self.assertRaises(ValueError): self.check(raw)

    def test_visible_pane_must_stay_inside_isolated_screen(self):
        self.reject(lambda r: r["snapshots"][0]["tabs"][0]["panes"][0].update(bounds=[20, 80, 2000, 200]))
        self.reject(lambda r: r["snapshots"][0]["tabs"][0]["panes"][0].update(bounds=[-20, 80, 380, 200]))

    def test_nonpositive_boolean_and_reordered_capture_times(self):
        for value in (0, True, 1_000_000_046):
            self.reject(lambda r: r["pixel_events"][0]["after"]["samples"][0].update(started_ns=value))
        self.reject(lambda r: r["pixel_events"][0]["before"]["samples"][0].update(finished_ns=1_000_000_031))
        self.reject(lambda r: r["pixel_events"][0].update(injection_completed_ns=1_000_000_041))
        self.reject(lambda r: r["pixel_events"][0].update(injection_completed_ns=1_000_000_029))
        self.reject(lambda r: r["pixel_events"][0]["after"].update(elapsed_ns=True))
        self.reject(lambda r: r["pixel_events"][0]["after"].update(capture_count=True))

    def test_fabricated_interval_and_unbounded_observer_rejected(self):
        for field, value in (("latency_interval_ns", [0, 1]), ("latency_interval_ns", [False, 15]),
                             ("visibility_interval_ns", [1, 2])):
            self.reject(lambda r: r["pixel_events"][0].update({field: value}))
        self.reject(lambda r: r["pixel_events"][0]["after"].update(elapsed_ns=gate.MAX_RESPONSE_NS + 1))

    def test_metadata_absence_has_only_conservative_retained_png_bound(self):
        raw = copy.deepcopy(self.raw); event = raw["pixel_events"][0]; result = event["after"]
        absent = dict(started_ns=1_000_000_034, finished_ns=1_000_000_036, matched=False, reason="marker_not_found")
        result["samples"].insert(0, absent); result.update(last_absent=copy.deepcopy(absent), capture_count=2)
        event.update(visibility_interval_ns=[1_000_000_034, 1_000_000_045], latency_interval_ns=[3, 15])
        summary = self.check(raw)["keys"][0]
        self.assertEqual(summary["retained_png_latency_interval_ns"], [0, 15])
        self.assertEqual(summary["metadata_latency_interval_ns"], [3, 15])
        self.assertFalse(summary["intermediate_absence_png_verified"])
        result["last_absent"]["started_ns"] += 1
        with self.assertRaises(ValueError): self.check(raw)

    def test_wait_stops_at_first_match_and_sample_bound(self):
        def update(r):
            result = r["pixel_events"][0]["after"]
            result["samples"].insert(0, dict(started_ns=1_000_000_034, finished_ns=1_000_000_036, matched=True, reason="matched"))
            result["capture_count"] = 2
        self.reject(update)
        self.reject(lambda r: r["pixel_events"][0]["after"].update(samples=[]))
        self.reject(lambda r: r["pixel_events"][0]["before"].update(last_absent=None))
        self.reject(lambda r: r["pixel_events"][0]["after"].pop("last_absent"))

    def test_drag_ids_arrays_and_one_pane_explicit_empty(self):
        for field in ("drags", "allocation_drags", "pixel_drags"):
            self.reject(lambda r: r[field].pop())
            self.reject(lambda r: r[field].reverse())
            self.reject(lambda r: r[field][0].update(id=False))
        for field in ("drags", "allocation_drags", "pixel_drags"):
            raw = valid(self.directory, 1); raw[field] = [{}]
            with self.assertRaises(ValueError): self.check(raw)

    def test_unchanged_divider_allocation_and_wrong_direction(self):
        self.reject(lambda r: r["allocation_drags"][0]["after"]["dividers"][0].update(position=760))
        self.reject(lambda r: r["allocation_drags"][0].update(shift=0))
        self.reject(lambda r: r["drags"][0].update(delta=-12))
        def unchanged(r):
            a = r["allocation_drags"][0]
            a["after"]["tabs"] = copy.deepcopy(a["before"]["tabs"])
        self.reject(unchanged)

    def test_divider_screen_coordinates_must_follow_allocation(self):
        self.reject(lambda r: r["allocation_drags"][0]["after"]["dividers"][0].update(x=780))
        self.reject(lambda r: r["allocation_drags"][0]["after"]["dividers"][0].update(y=412))

    def test_unchanged_pixels_and_forged_pixel_shift(self):
        event = self.raw["pixel_drags"][0]
        event["after"].update(retain(self.directory, "drag-0-after-final.png", divider_rgb(*event["rect"][2:], 32), *event["rect"][2:]))
        with self.assertRaises(ValueError): self.check()
        self.raw = valid(self.directory)
        self.reject(lambda r: r["pixel_drags"][0].update(pixel_shift=0))
        self.reject(lambda r: r["pixel_drags"][0]["after"].update(position=32))

    def test_drag_snapshot_linkage_and_temporal_bracketing(self):
        self.reject(lambda r: r["pixel_drags"][0].update(before_snapshot=copy.deepcopy(r["snapshots"][-1])))
        self.reject(lambda r: r["allocation_drags"][0]["after"].update(request=9999))
        self.reject(lambda r: r["drags"][0].update(injected_ns=1_000_000_090))
        self.reject(lambda r: r["pixel_drags"][0]["before"]["samples"][0].update(finished_ns=1_000_000_071))
        self.reject(lambda r: r["pixel_drags"][0]["after"]["samples"][0].update(started_ns=1_000_000_079))

    def test_32_panels_cannot_reuse_one_active_tab(self):
        raw = valid(self.directory, 32)
        middle = raw["keys"][1]; middle.update(pid=101, active_tab_id=10, scheduled_tab_id=10)
        target = next(s for s in raw["snapshots"] if s["request"] == middle["target_snapshot_request"])
        target["active_tab_index"] = 0
        for i, tab in enumerate(target["tabs"]):
            for pane in tab["panes"]: pane["mapped"] = i == 0
        raw["raw_probe"]["painted_markers"][1] = [101, 1, middle["injected_ns"] + 2]
        with self.assertRaisesRegex(ValueError, "every topology tab"): self.check(raw)
        raw = valid(self.directory, 32)
        raw["pixel_drags"][1]["tab_id"] = 10
        with self.assertRaises(ValueError): self.check(raw)

    def test_control_forbids_pixels_and_observer_but_checks_allocations(self):
        for field, value in (("pixel_events", [{}]), ("pixel_drags", [{}]), ("pixel_observer_pid", 999)):
            raw = valid(self.directory, mode="control"); raw[field] = value
            with self.assertRaises(ValueError): self.check(raw)
        raw = valid(self.directory, mode="control")
        raw["allocation_drags"][0]["after"]["dividers"][0]["position"] = 760
        with self.assertRaises(ValueError): self.check(raw)
        raw = valid(self.directory)
        raw["pixel_observer_pid"] = 100
        with self.assertRaises(ValueError): self.check(raw)
        raw["pixel_observer_pid"] = 123456
        with self.assertRaises(ValueError): self.check(raw)

    def test_stationary_two_edges_never_prove_motion_even_with_unrelated_pixel_change(self):
        for unrelated in (False, True):
            raw = valid(self.directory)
            for event in raw["pixel_drags"]:
                width, height = event['rect'][2:]; old, new = 32, 32 + event['delta']
                rgb = bytes(v for y in range(height) for x in range(width)
                            for v in ([220]*3 if min(old,new) <= x < max(old,new) else [20]*3))
                for phase, position in [('before',old),('after',new)]:
                    data=bytearray(rgb)
                    if unrelated and phase=='after':data[:3]=bytes([90,90,90])
                    previous=event[phase];sample=previous['samples'][0]
                    event[phase]=response(self.directory,'divider',f"drag-{event['id']}-{phase}",event['rect'],
                                          sample['started_ns'],sample['finished_ns'],bytes(data),position=position)
            with self.assertRaises(ValueError):self.check(raw)

    def test_portable_replay_preserves_records_and_image_hashes(self):
        import shutil
        original=copy.deepcopy(self.raw)
        with tempfile.TemporaryDirectory() as directory:
            relocated=Path(directory)/'downloaded';shutil.copytree(self.directory,relocated);relocated.chmod(0o700)
            result=gate.validate(self.raw,8,DIGEST,relocated)
            self.assertEqual(result['status'],'short_gate_only')
            self.assertEqual(self.raw,original)

    def test_input_helper_must_have_distinct_cleaned_identity(self):
        for value in (None,True,999,100,123456):
            self.reject(lambda r:r.update(input_helper_pid=value))

    def test_png_hash_shape_and_changed_bytes(self):
        self.reject(lambda r: r["pixel_events"][0]["after"].update(final_png_sha256="a" * 64))
        self.reject(lambda r: r["pixel_events"][0]["after"].update(final_png_sha256="invalid"))
        path = self.directory / self.raw["pixel_events"][0]["after"]["final_png_path"]
        path.write_bytes(path.read_bytes() + b"corruption")
        with self.assertRaisesRegex(ValueError, "hash mismatch"): self.check()

    def test_safe_exact_paths_private_directory_and_symlinks(self):
        for path in ("/tmp/outside.png", "nested/key-0-final.png", str(self.directory / ".." / self.directory.name / "key-0-final.png")):
            self.reject(lambda r: r["pixel_events"][0]["after"].update(final_png_path=path))
        self.directory.chmod(0o755)
        with self.assertRaises(ValueError): self.check()
        self.directory.chmod(0o700)
        path = self.directory / "key-0-final.png"; path.unlink(); path.symlink_to(self.directory / "key-1-final.png")
        with self.assertRaises(OSError): self.check()

    def test_hardlinked_or_public_png_rejected(self):
        path = self.directory / "key-0-final.png"
        path.chmod(0o644)
        with self.assertRaises(ValueError): self.check()
        path.chmod(0o600)
        os.link(path, self.directory / "alias.png")
        with self.assertRaises(ValueError): self.check()

    def test_explicit_evidence_directory_required_and_root_symlink_rejected(self):
        with self.assertRaises(ValueError): gate.validate(self.raw, 8, DIGEST)
        with tempfile.TemporaryDirectory() as parent:
            link = Path(parent) / "linked"; link.symlink_to(self.directory, target_is_directory=True)
            with self.assertRaises(OSError): gate.validate(self.raw, 8, DIGEST, link)

    def test_corrupted_png_rejected_even_with_rehashed_metadata(self):
        path = self.directory / "key-0-final.png"
        original = path.read_bytes()
        for data in (original[:-1], original + b"tail", original[:20] + bytes([original[20] ^ 1]) + original[21:]):
            with self.subTest(length=len(data)):
                path.write_bytes(data)
                self.raw["pixel_events"][0]["after"]["final_png_sha256"] = hashlib.sha256(data).hexdigest()
                with self.assertRaises(ValueError): self.check()

    def test_png_dimensions_filters_deflate_bomb_and_crc(self):
        def chunk(kind, data):
            return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xffffffff)
        def png(width, height, raw, extra=b"", depth=8):
            return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, depth, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw) + extra) + chunk(b"IEND", b"")
        for data in (png(2, 1, bytes(7)), png(1, 1, bytes(4), depth=16), png(1, 1, b"\x01\0\0\0"),
                     png(1, 1, bytes(1000000)), png(1, 1, bytes(4), b"tail")):
            with self.assertRaises(ValueError): gate.decode_png(data, 1, 1)
        self.assertEqual(gate.decode_png(png(1, 1, bytes(4)), 1, 1), bytes(3))

    def test_json_duplicate_nonfinite_and_cli_no_native_execution(self):
        raw_path = self.directory / "raw.json"
        for content in ('{"a":1,"a":2}', '{"a":NaN}'):
            raw_path.write_text(content)
            with self.assertRaises(ValueError): gate.read_json(raw_path)
        raw_path.write_text(json.dumps(self.raw))
        args = [str(raw_path), "--panes", "8", "--binary-sha256", DIGEST, "--evidence-directory", str(self.directory)]
        with patch.object(gate.observer.ctypes, "CDLL", side_effect=AssertionError("native X11 forbidden")), patch("sys.stdout", new_callable=io.StringIO) as stdout:
            self.assertEqual(gate.main(args), 0)
            self.assertEqual(json.loads(stdout.getvalue())["status"], "short_gate_only")
        self.raw["pixel_events"][0]["id"] = 1; raw_path.write_text(json.dumps(self.raw))
        with patch("sys.stdout", new_callable=io.StringIO) as stdout:
            self.assertEqual(gate.main(args), 2)
            self.assertFalse(json.loads(stdout.getvalue())["budget_acceptance"])


if __name__ == "__main__":
    unittest.main()
