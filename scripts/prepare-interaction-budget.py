#!/usr/bin/env python3
"""Apply the identical reviewed observer overlay in a NEW disposable source tree.

The original tree and normal release artifact are never modified. This script
builds nothing, opens no display/socket, and starts no application.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    source = args.source.resolve(strict=True)
    destination = args.destination.resolve()
    if destination.exists() or source == destination or source in destination.parents:
        raise SystemExit("destination must be a new tree outside source")
    scripts = Path(__file__).resolve().parent
    patch = scripts / "interaction-budget-overlay.patch"
    probe = scripts / "interaction_budget_probe.rs"
    if "interaction_budget" in (source / "src/main.rs").read_text():
        raise SystemExit("source already contains an observer; refusing double overlay")
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns(".git", "target", "__pycache__"))
    shutil.copyfile(probe, destination / "scripts" / probe.name)
    subprocess.run(["git", "apply", "--check", str(patch)], cwd=destination, check=True)
    subprocess.run(["git", "apply", str(patch)], cwd=destination, check=True)
    manifest = {"overlay_sha256": hashlib.sha256(patch.read_bytes() + probe.read_bytes()).hexdigest(),
                "patch_sha256": hashlib.sha256(patch.read_bytes()).hexdigest(),
                "probe_sha256": hashlib.sha256(probe.read_bytes()).hexdigest(),
                "normal_source_unchanged": True, "build_executed": False}
    (destination / "interaction-overlay.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest))


if __name__ == "__main__": main()
