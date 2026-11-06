#!/usr/bin/env bash
# Refresh the deterministic audit snapshot of the exact production Git source.
# Cargo, Nix and Flatpak still resolve the published pin, never this archive.
set -euo pipefail
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
core_dir=${1:-"$repo_dir/../jterm_core"}
revision=$(python3 - "$repo_dir/Cargo.toml" <<'PY'
import sys, tomllib
from pathlib import Path
core = tomllib.loads(Path(sys.argv[1]).read_text())["dependencies"]["jterm_core"]
if core.get("git") != "https://github.com/beamiter/jterm_core.git" or "path" in core:
    raise SystemExit("Expected the official exact Git core dependency")
print(core["rev"])
PY
)
test "$(git -C "$core_dir" rev-parse "$revision^{commit}")" = "$revision"
stage=$(mktemp -d "$repo_dir/packaging/.jterm-core-src.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
# Export committed blobs, not untracked or modified worktree inputs.
git -C "$core_dir" archive "$revision" Cargo.toml Cargo.lock README.md src runtime scripts tests assets \
  | tar -xf - -C "$stage"
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
  -C "$stage" -cf - Cargo.toml Cargo.lock README.md src runtime scripts tests assets \
  | gzip -n > "$stage/archive.tar.gz"
chmod 644 "$stage/archive.tar.gz"
mv -f "$stage/archive.tar.gz" "$repo_dir/packaging/jterm-core-src.tar.gz"
printf '%s\n' "$revision" > "$repo_dir/packaging/jterm-core-revision"
(cd "$repo_dir/packaging" && sha256sum jterm-core-src.tar.gz > jterm-core-src.tar.gz.sha256)
