#!/usr/bin/env python3
"""Prepare geometry comparison sheets from current production CSS, without GTK."""
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
START = "/* Candidate: neutral dialog controls inherit their owning surface's text tone."
END = ".workspace-dialog checkbutton check:checked,.workspace-dialog checkbutton radio:checked,.workspace-dialog switch:checked"


def sheets(shell, popups):
    # Keep every existing dialog rule and the selected accent in the baseline.
    # Only the neutral control paint block is removed for the comparison.
    if popups.count(START) != 1 or popups.count(END) != 1:
        raise ValueError("Expected exactly one neutral control block and selected-accent boundary")
    begin, end = popups.index(START), popups.index(END)
    if begin >= end:
        raise ValueError("Control paint boundaries are out of order")
    return shell + "\n" + popups[:begin] + popups[end:], shell + "\n" + popups


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: prepare-dialog-controls-theme.py ARTIFACT_DIRECTORY")
    baseline, candidate = sheets((ROOT / "src/workspace_shell.css").read_text(),
                                 (ROOT / "src/workspace_popups.css").read_text())
    output = pathlib.Path(sys.argv[1])
    output.mkdir(parents=True, exist_ok=True)
    (output / "baseline.css").write_text(baseline)
    (output / "candidate.css").write_text(candidate)
