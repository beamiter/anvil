#!/usr/bin/env python3
"""Pure geometry and mocked pipe contracts; never open a socket or display."""
from copy import deepcopy
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("interaction_pixel_client", Path(__file__).with_name("interaction-pixel-client.py"))
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)


def pane():
    return dict(pid=123, bounds=[10, 20, 800, 600],
                pixel_geometry=dict(bounds=[18, 28, 784, 584], cell_width=10, cell_height=20))


def snapshot(horizontal=True, shift=0):
    if horizontal:
        bounds = [[0, 0, 300 + shift, 200], [306 + shift, 0, 294 - shift, 200]]
    else:
        bounds = [[0, 0, 200, 300 + shift], [0, 306 + shift, 200, 294 - shift]]
    return dict(active_tab_index=0, tabs=[dict(tab_id=11, panes=[
        dict(pid=123, bounds=bounds[0]), dict(pid=124, bounds=bounds[1])])],
        dividers=[dict(horizontal=horizontal, position=303 + shift)])


class MarkerGeometry(unittest.TestCase):
    def test_clips_to_pane_and_terminal_intersection_near_bottom(self):
        rect, cw, ch = client.marker_rect(pane(), [100, 100])
        self.assertEqual((rect, cw, ch), ([118, 632, 330, 80], 10, 20))
        value = pane(); value['bounds'] = [20, 40, 315, 45]
        self.assertEqual(client.marker_rect(value, [100, 100])[0], [120, 140, 315, 45])

    def test_fractional_edges_round_inward_without_crossing_intersection(self):
        value = pane()
        value['bounds'] = [0.2, 0.2, 320.2, 40.6]
        value['pixel_geometry']['bounds'] = [0.2, 0.2, 320.2, 40.6]
        # The safe intersection in screen pixels is [101, 201, 320, 240].
        with self.assertRaises(ValueError):
            client.marker_rect(value, [100.1, 200.1])  # < two complete cell heights after inward rounding
        value['bounds'][3] = value['pixel_geometry']['bounds'][3] = 42.7
        rect, _, _ = client.marker_rect(value, [100.1, 200.1])
        self.assertGreaterEqual(rect[0], 101)
        self.assertGreaterEqual(rect[1], 201)
        self.assertLessEqual(rect[0] + rect[2], 420)
        self.assertLessEqual(rect[1] + rect[3], 243)

    def test_complete_thirty_one_cell_width_is_required(self):
        for width in (309, 0, -1):
            value = pane(); value['bounds'][2] = width
            with self.subTest(width=width), self.assertRaises(ValueError): client.marker_rect(value, [100, 100])
        value = pane(); value['bounds'] = [18, 28, 310, 40]
        self.assertEqual(client.marker_rect(value, [100, 100])[0][2:], [310, 40])

    def test_insufficient_height_and_disjoint_bounds(self):
        for bounds in ([18, 28, 310, 39], [1000, 28, 310, 40], [18, 1000, 310, 40]):
            value = pane(); value['bounds'] = bounds
            with self.subTest(bounds=bounds), self.assertRaises(ValueError): client.marker_rect(value, [0, 0])

    def test_boolean_nonfinite_and_malformed_geometry_rejected(self):
        for invalid in (True, False, float('nan'), float('inf'), -float('inf'), '12', None):
            for target in ('bounds', 'pixel_bounds', 'origin'):
                value, origin = pane(), [100, 100]
                if target == 'bounds': value['bounds'][0] = invalid
                elif target == 'pixel_bounds': value['pixel_geometry']['bounds'][1] = invalid
                else: origin[0] = invalid
                with self.subTest(invalid=invalid, target=target), self.assertRaises(ValueError):
                    client.marker_rect(value, origin)
        for invalid in ([], [0], [0, 1, 2], None, 'ab'):
            with self.subTest(origin=invalid), self.assertRaises(ValueError): client.marker_rect(pane(), invalid)
        for invalid in ([], [1, 2, 3], None, 'abcd'):
            value = pane(); value['bounds'] = invalid
            with self.subTest(bounds=invalid), self.assertRaises(ValueError): client.marker_rect(value, [100, 100])

    def test_cell_geometry_is_bounded_exact_integer(self):
        for key, values in [('cell_width', (True, 0, 21, 10.0, float('nan'))),
                            ('cell_height', (False, 0, 65, 20.0, float('inf')))]:
            for invalid in values:
                value = pane(); value['pixel_geometry'][key] = invalid
                with self.subTest(key=key, invalid=invalid), self.assertRaises(ValueError):
                    client.marker_rect(value, [100, 100])

    def test_screen_bounds_rejected_before_observation(self):
        for origin in ([-100, 100], [1400, 100], [100, 500], [100, -1000]):
            with self.subTest(origin=origin), self.assertRaises(ValueError): client.marker_rect(pane(), origin)
        value = pane(); value['bounds'] = value['pixel_geometry']['bounds'] = [0, 0, 330, 80]
        self.assertEqual(client.marker_rect(value, [1270, 920])[0], [1270, 920, 330, 80])


class DividerGeometry(unittest.TestCase):
    def test_gtk_horizontal_uses_wide_x_axis_and_expected_center(self):
        rect, expected = client.divider_rect(dict(horizontal=True, x=203, y=100), [10, 20])
        self.assertEqual(rect, [181, 96, 64, 48])
        self.assertEqual(expected, 32)
        self.assertEqual(rect[0] + expected, 213)

    def test_gtk_vertical_uses_tall_y_axis_and_expected_center(self):
        rect, expected = client.divider_rect(dict(horizontal=False, x=100, y=203), [10, 20])
        self.assertEqual(rect, [86, 191, 48, 64])
        self.assertEqual(expected, 32)
        self.assertEqual(rect[1] + expected, 223)

    def test_orientation_must_be_an_actual_boolean(self):
        for horizontal in (0, 1, 'horizontal', None, [], {}):
            with self.subTest(horizontal=horizontal), self.assertRaises(ValueError):
                client.divider_rect(dict(horizontal=horizontal, x=200, y=200), [0, 0])

    def test_coordinates_reject_bool_nonfinite_and_malformed_values(self):
        for invalid in (True, False, float('nan'), float('inf'), -float('inf'), '200', None):
            for axis in ('x', 'y'):
                value = dict(horizontal=True, x=200, y=200); value[axis] = invalid
                with self.subTest(axis=axis, invalid=invalid), self.assertRaises(ValueError):
                    client.divider_rect(value, [0, 0])
        for origin in ([True, 0], [0, float('nan')], [0], [0, 0, 0], None):
            with self.subTest(origin=origin), self.assertRaises(ValueError):
                client.divider_rect(dict(horizontal=False, x=200, y=200), origin)

    def test_screen_edges_allow_exact_fit_and_reject_crossing(self):
        for horizontal, center in ((True, [32, 24]), (False, [24, 32])):
            self.assertEqual(client.divider_rect(dict(horizontal=horizontal, x=center[0], y=center[1]), [0, 0])[0][:2], [0, 0])
        for x, y in ((31, 24), (32, 23), (1569, 400), (400, 977)):
            with self.subTest(x=x, y=y), self.assertRaises(ValueError):
                client.divider_rect(dict(horizontal=True, x=x, y=y), [0, 0])


class AllocationGeometry(unittest.TestCase):
    def test_valid_horizontal_and_vertical_positive_and_negative_shifts(self):
        for horizontal in (True, False):
            for delta in (16, -16):
                with self.subTest(horizontal=horizontal, delta=delta):
                    self.assertEqual(client.allocation_shift(snapshot(horizontal), snapshot(horizontal, delta), delta), delta)

    def test_small_allocation_rounding_tolerance(self):
        self.assertEqual(client.allocation_shift(snapshot(), snapshot(shift=14), 16), 14)

    def test_unchanged_allocation_wrong_direction_and_wrong_distance_rejected(self):
        for shift in (0, -16, 10, 22):
            with self.subTest(shift=shift), self.assertRaises(ValueError):
                client.allocation_shift(snapshot(), snapshot(shift=shift), 16)
        before, after = snapshot(), snapshot(shift=16)
        after['tabs'][0]['panes'] = deepcopy(before['tabs'][0]['panes'])
        with self.assertRaises(ValueError): client.allocation_shift(before, after, 16)

    def test_wrong_tab_pid_order_and_orientation_rejected(self):
        for mutate in (
            lambda s: s['tabs'][0].update(tab_id=12),
            lambda s: s['tabs'][0]['panes'][0].update(pid=999),
            lambda s: s['tabs'][0]['panes'].reverse(),
            lambda s: s['dividers'][0].update(horizontal=False),
            lambda s: s['dividers'][0].update(horizontal=1),
        ):
            after = snapshot(shift=16); mutate(after)
            with self.assertRaises(ValueError): client.allocation_shift(snapshot(), after, 16)

    def test_wrong_axis_allocation_cannot_support_divider_movement(self):
        for horizontal in (True, False):
            before, after = snapshot(horizontal), snapshot(horizontal, 16)
            after['tabs'][0]['panes'] = deepcopy(before['tabs'][0]['panes'])
            wrong_axis = 1 if horizontal else 0
            after['tabs'][0]['panes'][0]['bounds'][wrong_axis] += 16
            with self.subTest(horizontal=horizontal), self.assertRaises(ValueError):
                client.allocation_shift(before, after, 16)

    def test_invalid_active_indices_rejected(self):
        for index in (-1, 1, True, False, 0.0, None):
            for target in ('before', 'after'):
                before, after = snapshot(), snapshot(shift=16)
                (before if target == 'before' else after)['active_tab_index'] = index
                with self.subTest(index=index, target=target), self.assertRaises(ValueError):
                    client.allocation_shift(before, after, 16)

    def test_nonfinite_boolean_zero_delta_and_position_rejected(self):
        for invalid in (True, False, float('nan'), float('inf'), -float('inf'), '16', None, 0):
            with self.subTest(delta=invalid), self.assertRaises(ValueError):
                client.allocation_shift(snapshot(), snapshot(shift=16), invalid)
        for invalid in (True, False, float('nan'), float('inf'), -float('inf'), '319', None):
            after = snapshot(shift=16); after['dividers'][0]['position'] = invalid
            with self.subTest(position=invalid), self.assertRaises(ValueError):
                client.allocation_shift(snapshot(), after, 16)

    def test_malformed_or_nonfinite_pane_bounds_rejected(self):
        for invalid in ([0, 0, 1], [0, True, 316, 200], [0, 0, float('nan'), 200], None):
            after = snapshot(shift=16); after['tabs'][0]['panes'][0]['bounds'] = invalid
            with self.subTest(bounds=invalid), self.assertRaises(ValueError):
                client.allocation_shift(snapshot(), after, 16)


class FakeStream:
    def __init__(self, fd):
        self.fd = fd; self.writes = []; self.flushes = 0; self.closes = 0
        self.write_error = self.flush_error = self.close_error = None
    def fileno(self): return self.fd
    def write(self, value):
        if self.write_error: raise self.write_error
        self.writes.append(value)
        return len(value)
    def flush(self):
        if self.flush_error: raise self.flush_error
        self.flushes += 1
    def close(self):
        self.closes += 1
        if self.close_error: raise self.close_error


class PipeClient(unittest.TestCase):
    def make(self):
        temporary = tempfile.TemporaryDirectory(); self.addCleanup(temporary.cleanup)
        directory = Path(temporary.name) / 'pixels'
        process = Mock()
        process.stdin, process.stdout = FakeStream(111), FakeStream(112)
        process.poll.return_value = None
        cancelled = Mock()
        env = {'ANVIL_BUDGET_ISOLATED_DISPLAY': '1', 'DISPLAY': ':99'}
        with patch.object(client.subprocess, 'Popen', return_value=process) as popen, patch.object(client.os, 'set_blocking') as blocking:
            connection = client.PixelClient(directory, env, cancelled)
        return connection, process, cancelled, directory, env, popen, blocking

    def respond(self, connection, chunks, **command):
        chunks = list(chunks)
        def ready(*args): return ([connection.process.stdout] if chunks else [], [], [])
        with patch.object(client.select, 'select', side_effect=ready), patch.object(client.os, 'read', side_effect=lambda *args: chunks.pop(0)), patch.object(client.time, 'monotonic', side_effect=(step / 100 for step in range(1000))):
            return connection.request(**command)

    def test_constructor_private_directory_isolated_process_and_nonblocking_stdout(self):
        connection, process, cancelled, directory, env, popen, blocking = self.make()
        self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
        args, kwargs = popen.call_args
        self.assertEqual(args[0][:2], [sys.executable, '-I'])
        self.assertEqual(Path(args[0][2]).name, 'interaction-pixel-observer.py')
        self.assertEqual(args[0][3], str(directory))
        self.assertEqual(kwargs['env'], env)
        self.assertEqual(kwargs['stdin'], client.subprocess.PIPE)
        self.assertEqual(kwargs['stdout'], client.subprocess.PIPE)
        blocking.assert_called_once_with(112, False)

    def test_roundtrip_partial_reads_preserves_numeric_response(self):
        connection, process, cancelled, *_ = self.make()
        value = dict(kind='marker_wait', name='key-0', matched=True, capture_count=2, elapsed_ns=15)
        wire = (json.dumps(value) + '\n').encode()
        result = self.respond(connection, [wire[:3], wire[3:17], wire[17:]], kind='marker_wait', name='key-0')
        self.assertEqual(result, value)
        self.assertEqual(json.loads(process.stdin.writes[0]), dict(kind='marker_wait', name='key-0'))
        self.assertEqual(process.stdin.flushes, 1)
        self.assertGreaterEqual(cancelled.call_count, 3)

    def test_buffered_next_record_is_preserved_for_next_request(self):
        connection, process, cancelled, *_ = self.make()
        first = dict(kind='divider', name='drag-0-before')
        second = dict(kind='divider', name='drag-0-after')
        wire = (json.dumps(first) + '\n' + json.dumps(second) + '\n').encode()
        self.assertEqual(self.respond(connection, [wire], **first), first)
        self.assertEqual(self.respond(connection, [], **second), second)
        self.assertEqual(connection.buffer, b'')

    def test_response_identity_mismatch_rejected(self):
        for response in (dict(kind='divider', name='key-0'), dict(kind='marker_wait', name='key-1'), {}):
            connection, process, cancelled, *_ = self.make()
            with self.subTest(response=response), self.assertRaisesRegex(ValueError, 'identity mismatch'):
                self.respond(connection, [(json.dumps(response) + '\n').encode()], kind='marker_wait', name='key-0')

    def test_malformed_response_rejected(self):
        connection, process, cancelled, *_ = self.make()
        with self.assertRaises(ValueError): self.respond(connection, [b'not-json\n'], kind='divider', name='drag-0')

    def test_oversized_command_is_never_written(self):
        connection, process, cancelled, *_ = self.make()
        with self.assertRaisesRegex(ValueError, 'oversized'):
            connection.request(kind='divider', name='x' * 4096)
        self.assertEqual(process.stdin.writes, [])
        self.assertEqual(process.stdin.flushes, 0)

    def test_oversized_response_is_bounded_without_newline(self):
        connection, process, cancelled, *_ = self.make()
        with self.assertRaisesRegex(ValueError, 'response exceeded bound'):
            self.respond(connection, [b'x' * 32768, b'x' * 32768, b'x'], kind='divider', name='drag-0')
        self.assertEqual(len(connection.buffer), 65537)

    def test_timeout_uses_mock_clock_and_never_waits_in_real_time(self):
        connection, process, cancelled, *_ = self.make()
        with patch.object(client.time, 'monotonic', side_effect=[0, 1, 6]), patch.object(client.select, 'select', return_value=([], [], [])), patch.object(client.os, 'read') as read:
            with self.assertRaises(TimeoutError): connection.request(kind='divider', name='drag-0')
            read.assert_not_called()
        cancelled.assert_called_once_with()

    def test_cancellation_propagates_before_read_or_poll(self):
        connection, process, cancelled, *_ = self.make()
        class Cancelled(RuntimeError): pass
        cancelled.side_effect = Cancelled('cancel now')
        with patch.object(client.select, 'select') as select, patch.object(client.os, 'read') as read:
            with self.assertRaises(Cancelled): connection.request(kind='divider', name='drag-0')
            select.assert_not_called(); read.assert_not_called()
        process.poll.assert_not_called()

    def test_exited_owned_process_fails_without_read(self):
        connection, process, cancelled, *_ = self.make()
        process.poll.return_value = 2
        with patch.object(client.os, 'read') as read:
            with self.assertRaisesRegex(RuntimeError, 'observer exited'):
                connection.request(kind='divider', name='drag-0')
            read.assert_not_called()

    def test_write_and_flush_pipe_errors_propagate(self):
        for field in ('write_error', 'flush_error'):
            connection, process, cancelled, *_ = self.make()
            setattr(process.stdin, field, BrokenPipeError('observer disappeared'))
            with self.subTest(field=field), self.assertRaises(BrokenPipeError):
                connection.request(kind='divider', name='drag-0')
            process.poll.assert_not_called()

    def test_close_attempts_both_pipes_despite_errors_without_signalling(self):
        connection, process, cancelled, *_ = self.make()
        process.stdin.close_error = BrokenPipeError('closed')
        process.stdout.close_error = OSError('already closed')
        connection.close_pipes()
        self.assertEqual(process.stdin.closes, 1)
        self.assertEqual(process.stdout.closes, 1)
        process.terminate.assert_not_called(); process.kill.assert_not_called(); process.wait.assert_not_called()

    def test_constructor_does_not_reuse_an_existing_evidence_directory(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(client.subprocess, 'Popen') as popen:
            with self.assertRaises(FileExistsError): client.PixelClient(Path(directory), {}, lambda: None)
            popen.assert_not_called()


if __name__ == '__main__':
    unittest.main()
