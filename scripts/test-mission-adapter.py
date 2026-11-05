#!/usr/bin/env python3
"""Run the real GTK-independent Mission form adapter against the locked core.

No widgets, display, IME, provider or host runtime is simulated or accepted here.
The temporary crate compiles the production source unchanged. Its core path is
resolved from the exact production lock, never a sibling or historical bundle.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def main():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT
    ))
    dependency = tomllib.loads((ROOT / "Cargo.toml").read_text())["dependencies"]["jterm_core"]
    expected = f"git+{dependency['git']}?rev={dependency['rev']}#{dependency['rev']}"
    cores = [p for p in metadata["packages"] if p["name"] == "jterm_core" and p["source"] == expected]
    if len(cores) != 1:
        raise SystemExit("The adapter fixture requires the exact production core source")
    core = Path(cores[0]["manifest_path"]).parent
    with tempfile.TemporaryDirectory(prefix="anvil-mission-adapter-") as directory:
        root = Path(directory)
        (root / "src").mkdir()
        (root / "Cargo.toml").write_text(
            '[package]\nname = "anvil-mission-adapter-tests"\nversion = "0.0.0"\nedition = "2021"\n'
            '[dependencies]\njterm_core = { path = ' + json.dumps(str(core)) + ' }\n'
            '[patch."https://github.com/beamiter/jcompanion.git"]\n'
            'companion-client = { path = ' + json.dumps(str(ROOT / "vendor/companion-client")) + ' }\n'
            'companion-protocol = { path = ' + json.dumps(str(ROOT / "vendor/companion-protocol")) + ' }\n'
        )
        (root / "src/lib.rs").write_text(
            '#[path = ' + json.dumps(str(ROOT / "src/workspace_mission_model.rs"), ensure_ascii=False) + ']\n'
            'mod workspace_mission_model;\n'
        )
        shutil.copyfile(ROOT / "Cargo.lock", root / "Cargo.lock")
        environment = dict(os.environ)
        environment.setdefault("CARGO_TARGET_DIR", str(ROOT / "target/mission-adapter"))
        subprocess.run(["cargo", "test", "--lib", "--offline", "--manifest-path", str(root / "Cargo.toml")],
                       cwd=root, env=environment, check=True)


if __name__ == "__main__":
    main()
