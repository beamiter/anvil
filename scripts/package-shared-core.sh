#!/usr/bin/env bash
# Refresh the deterministic source snapshot used by isolated Nix builds.
set -euo pipefail
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
core_dir=${1:-"$repo_dir/../jterm_core"}
archive="$repo_dir/packaging/jterm-core-src.tar.gz"
stage=$(mktemp "$repo_dir/packaging/.jterm-core-src.XXXXXX")
trap 'rm -f "$stage"' EXIT
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
  -C "$core_dir" -cf - Cargo.toml Cargo.lock README.md src runtime scripts tests \
  | gzip -n > "$stage"
chmod 644 "$stage"
mv -f "$stage" "$archive"
(cd "$repo_dir/packaging" && sha256sum jterm-core-src.tar.gz > jterm-core-src.tar.gz.sha256)
