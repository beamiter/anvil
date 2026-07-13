#!/usr/bin/env bash
# Reproducible build/test snapshot for jterm1.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
BINARY="${PROJECT_ROOT}/target/release/jterm1"

if ! command -v nix >/dev/null 2>&1; then
    echo "Error: Nix with flakes support is required." >&2
    exit 1
fi

cd "${PROJECT_ROOT}"

echo "jterm1 performance snapshot"
echo "==========================="
echo

echo "Release build (incremental):"
start_ns="$(date +%s%N)"
nix develop --command cargo build --release --quiet
end_ns="$(date +%s%N)"
echo "  $(((end_ns - start_ns) / 1000000)) ms"
echo

if [[ ! -x "${BINARY}" ]]; then
    echo "Error: release binary not found at ${BINARY}" >&2
    exit 1
fi

echo "Binary size:"
size_bytes="$(wc -c < "${BINARY}")"
size_human="$(du -h "${BINARY}" | awk '{print $1}')"
echo "  ${size_human} (${size_bytes} bytes)"
echo

echo "Test suite (all targets):"
start_ns="$(date +%s%N)"
nix develop --command cargo test --all-targets --quiet
end_ns="$(date +%s%N)"
echo "  $(((end_ns - start_ns) / 1000000)) ms"
echo

echo "Direct dependency entries:"
dependency_lines="$(nix develop --command cargo tree --depth 1 --prefix none | wc -l)"
echo "  $((dependency_lines > 0 ? dependency_lines - 1 : 0))"
echo

echo "Running jterm1 processes:"
if pgrep -x jterm1 >/dev/null 2>&1; then
    ps -C jterm1 -o pid=,rss=,args= | awk '{printf "  PID %s: %.1f MiB RSS  %s\n", $1, $2 / 1024, $3}'
else
    echo "  None."
fi

echo
echo "Snapshot complete."
