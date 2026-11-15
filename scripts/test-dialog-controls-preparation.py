#!/usr/bin/env python3
"""Pure preparation checks; these do not claim native GTK acceptance."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("prepare", Path(__file__).with_name("prepare-dialog-controls-theme.py"))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class Sheets(unittest.TestCase):
    def test_production_candidate_matches_install_css_order(self):
        shell = (prepare.ROOT / "src/workspace_shell.css").read_text()
        popups = (prepare.ROOT / "src/workspace_popups.css").read_text()
        baseline, candidate = prepare.sheets(shell, popups)
        self.assertEqual(candidate, shell + "\n" + popups)
        begin, end = popups.index(prepare.START), popups.index(prepare.END)
        self.assertEqual(baseline, shell + "\n" + popups[:begin] + popups[end:])
        self.assertIn(prepare.END, baseline)
        self.assertNotIn(prepare.START, baseline)

    def test_missing_or_duplicate_boundaries_fail_closed(self):
        for popups in ("", prepare.START, prepare.END, prepare.START * 2 + prepare.END,
                       prepare.START + prepare.END * 2, prepare.END + prepare.START):
            with self.subTest(popups=popups), self.assertRaises(ValueError):
                prepare.sheets("shell", popups)


if __name__ == "__main__":
    unittest.main()
