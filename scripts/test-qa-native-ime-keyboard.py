#!/usr/bin/env python3
"""Pure mocks for the fixed native IME injector; never opens a display."""
import ctypes
import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

# Also works next to the production helper in scripts/.
HELPER = Path(__file__).with_name("qa-native-ime-keyboard.py")
spec = importlib.util.spec_from_file_location("ime_keys", HELPER)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class FocusTests(unittest.TestCase):
    def libraries(self, focus=42):
        x, xt = Mock(), Mock()
        x.XOpenDisplay.return_value = 1
        x.XDefaultRootWindow.return_value = 42
        name = ctypes.create_string_buffer(helper.TITLE)

        def fetch_name(display, window, out):
            ctypes.cast(out, ctypes.POINTER(ctypes.c_void_p))[0] = ctypes.addressof(name)
            return 1

        def get_focus(display, out, revert):
            ctypes.cast(out, ctypes.POINTER(ctypes.c_ulong))[0] = focus
            return 1

        x.XFetchName.side_effect = fetch_name
        x.XGetInputFocus.side_effect = get_focus
        x.XStringToKeysym.return_value = 97
        x.XKeysymToKeycode.return_value = 38
        xt.XTestFakeKeyEvent.return_value = 1
        return x, xt

    def run_main(self, args, x, xt):
        with patch.object(sys, "argv", ["helper", *args]), patch.object(
                helper.ctypes, "CDLL", side_effect=[x, xt]):
            helper.main()

    def test_all_existing_actions_preserve_default_activation(self):
        for action in helper.ACTIONS:
            with self.subTest(action=action):
                x, xt = self.libraries()
                self.run_main([action], x, xt)
                x.XSetInputFocus.assert_called_once_with(1, 42, 1, 0)
                self.assertTrue(xt.XTestFakeKeyEvent.called)

    def test_strict_owned_focus_never_activates(self):
        x, xt = self.libraries()
        self.run_main(["ascii-a", "--require-focus"], x, xt)
        x.XSetInputFocus.assert_not_called()
        self.assertEqual(x.XGetInputFocus.call_count, 2)
        self.assertEqual(xt.XTestFakeKeyEvent.call_count, 2)

    def test_strict_wrong_focus_never_injects(self):
        x, xt = self.libraries(focus=99)
        with self.assertRaisesRegex(RuntimeError, "does not own"):
            self.run_main(["compose", "--require-focus"], x, xt)
        x.XSetInputFocus.assert_not_called()
        xt.XTestFakeKeyEvent.assert_not_called()
        x.XCloseDisplay.assert_called_once()

    def test_focus_query_failure_never_injects(self):
        x, xt = self.libraries()
        x.XGetInputFocus.side_effect = None
        x.XGetInputFocus.return_value = 0
        with self.assertRaisesRegex(RuntimeError, "Could not query"):
            self.run_main(["commit", "--require-focus"], x, xt)
        x.XSetInputFocus.assert_not_called()
        xt.XTestFakeKeyEvent.assert_not_called()

    def test_focus_loss_between_presses_stops_and_releases_held_keys(self):
        x, xt = self.libraries()
        calls = 0

        def get_focus(display, out, revert):
            nonlocal calls
            calls += 1
            ctypes.cast(out, ctypes.POINTER(ctypes.c_ulong))[0] = 42 if calls < 3 else 99
            return 1

        x.XGetInputFocus.side_effect = get_focus
        with self.assertRaisesRegex(RuntimeError, "does not own"):
            self.run_main(["compose", "--require-focus"], x, xt)
        x.XSetInputFocus.assert_not_called()
        self.assertEqual([call.args[2] for call in xt.XTestFakeKeyEvent.call_args_list], [1, 0])
        x.XCloseDisplay.assert_called_once()

    def test_focus_loss_releases_both_distinct_modifiers(self):
        x, xt = self.libraries()
        x.XStringToKeysym.side_effect = lambda name: {b"Control_L": 37, b"Shift_L": 50}[name]
        x.XKeysymToKeycode.side_effect = lambda display, symbol: symbol
        calls = 0

        def get_focus(display, out, revert):
            nonlocal calls
            calls += 1
            ctypes.cast(out, ctypes.POINTER(ctypes.c_ulong))[0] = 42 if calls < 4 else 99
            return 1

        x.XGetInputFocus.side_effect = get_focus
        with self.assertRaisesRegex(RuntimeError, "does not own"):
            self.run_main(["compose", "--require-focus"], x, xt)
        events = [(call.args[1], call.args[2]) for call in xt.XTestFakeKeyEvent.call_args_list]
        self.assertEqual(events[:2], [(37, 1), (50, 1)])
        self.assertEqual(sorted(events[2:]), [(37, 0), (50, 0)])
        x.XSetInputFocus.assert_not_called()
        x.XCloseDisplay.assert_called_once()

    def test_invalid_arguments_fail_before_loading_libraries(self):
        for args in ([], ["arbitrary"], ["compose", "--repair"], ["compose", "--require-focus", "42"]):
            with self.subTest(args=args), patch.object(sys, "argv", ["helper", *args]), patch.object(
                    helper.ctypes, "CDLL") as load:
                with self.assertRaises(SystemExit):
                    helper.main()
                load.assert_not_called()


if __name__ == "__main__":
    unittest.main()
