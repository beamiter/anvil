#!/usr/bin/env bash
# Check the committed tree without materializing a sibling core or changing it.
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
stage=$(mktemp -d "${TMPDIR:-/tmp}/anvil-clean-build.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
mkdir "$stage/anvil"
git -C "$root" archive HEAD | tar -xf - -C "$stage/anvil"
test ! -e "$stage/jterm_core"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
cd "$stage/anvil"
python3 scripts/check-production-core.py
python3 scripts/test-production-core.py
# metadata resolves dependencies but does not need GTK development libraries.
host=$(rustc -vV | sed -n 's/^host: //p')
cargo metadata --locked --format-version 1 --filter-platform "$host" > "$stage/metadata.json"
python3 - "$stage/metadata.json" <<'PY'
import json
from pathlib import Path
import sys
import tomllib
dependency = tomllib.loads(Path("Cargo.toml").read_text())["dependencies"]["jterm_core"]
revision = dependency["rev"]
expected = f"git+{dependency['git']}?rev={revision}#{revision}"
cores = [p for p in json.loads(Path(sys.argv[1]).read_text())["packages"] if p["name"] == "jterm_core"]
if len(cores) != 1 or cores[0].get("source") != expected:
    raise SystemExit("Resolved core is not the exact production Git source")
print(f"Clean checkout resolved production core: {expected}")
PY
test ! -e "$stage/jterm_core"
cargo check --all-targets --locked
test ! -e "$stage/jterm_core"
