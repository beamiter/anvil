#!/usr/bin/env python3
"""Run production SSH follow callbacks with deterministic UI/transport doubles.

Only Python and rustc are required. This checks request ownership, not native
GTK rendering, live process observation, or real SSH transport behavior.
"""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
ITEMS = [('// @PRODUCTION_MANUAL_CANCEL@', 'file_tree_ops.rs', 'fn cancel_file_tree_ssh_follow_for_files_intent('), ('// @PRODUCTION_DEDUP@', 'file_tree.rs', 'pub(crate) fn ssh_file_tree_observation_matches_target('), ('// @PRODUCTION_WORKER@', 'file_tree_ops.rs', 'move || {'), ('// @PRODUCTION_0@', 'file_tree.rs', 'impl ScanCancellation {'), ('// @PRODUCTION_1@', 'file_tree.rs', 'pub(crate) struct PendingTreeNavigation {'), ('// @PRODUCTION_2@', 'file_tree.rs', 'pub(crate) fn pending_navigation_is_current('), ('// @PRODUCTION_3@', 'file_tree.rs', 'pub(crate) fn ssh_file_tree_detection_is_current('), ('// @PRODUCTION_4@', 'file_tree.rs', 'pub(crate) struct SshFileTreeDetection {'), ('// @PRODUCTION_5@', 'file_tree.rs', 'pub(crate) enum SshFileTreeObservation {'), ('// @PRODUCTION_6@', 'file_tree.rs', 'pub(crate) struct SshFileTreeProbeResult {'), ('// @PRODUCTION_7@', 'file_tree_ops.rs', 'fn next_file_tree_ssh_detection_token('), ('// @PRODUCTION_8@', 'file_tree_ops.rs', 'pub(crate) fn invalidate_file_tree_ssh_detection_context('), ('// @PRODUCTION_9@', 'file_tree_ops.rs', 'fn next_file_tree_navigation_token('), ('// @PRODUCTION_10@', 'file_tree_ops.rs', 'fn invalidate_pending_file_tree_navigation('), ('// @PRODUCTION_11@', 'file_tree_ops.rs', 'pub(crate) fn file_tree_ssh_probe_resolved('), ('// @PRODUCTION_12@', 'file_tree_ops.rs', 'pub(crate) fn file_tree_navigation_resolved(')]

def production_item(source, signature):
    start = source.index(signature)
    opening = source.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def main():
    harness = (ROOT / "scripts/files_follow_harness.rs").read_text()
    for marker, filename, signature in ITEMS:
        source = (ROOT / "src" / filename).read_text()
        assert harness.count(marker) == 1, marker
        harness = harness.replace(marker, production_item(source, signature))
    with tempfile.TemporaryDirectory(prefix="anvil-files-follow-") as directory:
        source = Path(directory) / "tests.rs"
        binary = Path(directory) / "tests"
        source.write_text(harness)
        subprocess.run(["rustc", "--edition=2021", "--test", str(source), "-o", str(binary)], check=True)
        subprocess.run([str(binary), "--test-threads=1"], check=True)


if __name__ == "__main__":
    main()
