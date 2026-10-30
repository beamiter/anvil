#!/usr/bin/env bash
# Materialize the checked-in shared-core snapshot for an isolated app checkout.
# Existing sibling checkouts (including explicit upgrade candidates) are preserved.
set -euo pipefail
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
# Exact Git releases are acquired by Cargo, not the historical bundled archive.
# Preserve the existing helper entrypoint used by CI without creating siblings.
if python3 - "$repo_dir/Cargo.toml" <<'PY_CHECK'
import sys, tomllib
with open(sys.argv[1], 'rb') as stream:
    dep = tomllib.load(stream)['dependencies']['jterm_core']
raise SystemExit(0 if 'git' in dep else 1)
PY_CHECK
then
  (cd "$repo_dir" && cargo fetch --locked)
  printf 'Using Cargo-locked shared core; no sibling checkout created.\n'
  exit 0
fi
core_dir=${1:-"$repo_dir/../jterm_core"}
if [[ -f "$core_dir/Cargo.toml" ]]; then
  printf 'Using existing shared core: %s\n' "$core_dir"
  exit 0
fi
if [[ -e "$core_dir" ]]; then
  printf 'Refusing to overwrite an existing incomplete core directory: %s\n' "$core_dir" >&2
  exit 1
fi
(cd "$repo_dir/packaging" && sha256sum --check jterm-core-src.tar.gz.sha256)
parent=$(dirname -- "$core_dir")
mkdir -p -- "$parent"
stage=$(mktemp -d "$parent/.jterm-core-materialize.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
# The archive has reproducible epoch mtimes. Fresh extraction timestamps make
# Cargo invalidate older cached path-dependency metadata after a core upgrade.
tar --touch --no-same-owner --no-same-permissions -xzf "$repo_dir/packaging/jterm-core-src.tar.gz" -C "$stage"
test -f "$stage/src/agent_sessions.rs"
test -f "$stage/Cargo.toml"
mv -- "$stage" "$core_dir"
printf 'Materialized bundled shared core: %s\n' "$core_dir"
