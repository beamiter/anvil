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
extra=()
if git -C "$core_dir" cat-file -e "$revision:crates" 2>/dev/null; then extra+=(crates); fi
git -C "$core_dir" archive "$revision" Cargo.toml Cargo.lock README.md src runtime scripts tests assets "${extra[@]}" \
  | tar -xf - -C "$stage"
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
  -C "$stage" -cf - Cargo.toml Cargo.lock README.md src runtime scripts tests assets "${extra[@]}" \
  | gzip -n > "$stage/archive.tar.gz"
chmod 644 "$stage/archive.tar.gz"
mv -f "$stage/archive.tar.gz" "$repo_dir/packaging/jterm-core-src.tar.gz"
printf '%s\n' "$revision" > "$repo_dir/packaging/jterm-core-revision"
(cd "$repo_dir/packaging" && sha256sum jterm-core-src.tar.gz > jterm-core-src.tar.gz.sha256)
# Keep archive identity and content receipt in the same update.
python3 - "$repo_dir" "$core_dir" "$revision" <<'PYRECEIPT'
import hashlib, json, subprocess, sys, tarfile
from pathlib import Path
root, core, revision = sys.argv[1:]
p = Path(root) / "packaging"
archive = p / "jterm-core-src.tar.gz"
with tarfile.open(archive) as tf:
    members = tf.getmembers()
receipt = {
    "published_core_commit": revision,
    "published_core_tree": subprocess.check_output(["git", "-C", core, "rev-parse", revision + "^{tree}"], text=True).strip(),
    "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
    "archive_members": len(members),
    "archive_files": sum(m.isfile() for m in members),
    "scope": "Cargo files, README.md, src/, runtime/, scripts/, tests/, assets/ and crates/ when present, exported from immutable committed blobs; docs and CI excluded.",
    "verification": "git archive exports committed blobs and executable modes; archive timestamps and owner IDs normalized.",
    "usage": "Audit export only; production Cargo, Nix and Flatpak use the published Git pin."
}
production = p / "jterm-core-production.json"
if production.exists():
    source = json.loads(production.read_text())
    if source.get("revision") == revision and source.get("tree") == receipt["published_core_tree"]:
        receipt["source_nar_sha256"] = source["source_nar_sha256"]
        receipt["publication_boundary"] = "Public source verified at " + source["publication_verified_at"] + "; no native acceptance implied."
(p / "jterm-core-origin.json").write_text(json.dumps(receipt, indent=2) + "\n")
PYRECEIPT
