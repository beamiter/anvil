#!/usr/bin/env bash
# Reproducible dependency and shell-script security checks.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

cargo metadata --locked --format-version 1 --no-deps >/dev/null

if ! command -v cargo-audit >/dev/null 2>&1; then
    echo "Error: cargo-audit is required (install with 'cargo install cargo-audit --locked')." >&2
    exit 1
fi
cargo audit

# Duplicate crates are not automatically vulnerabilities, but surfacing them
# here makes dependency drift visible during reviews and scheduled audits.
cargo tree --locked --duplicates

if ! command -v shellcheck >/dev/null 2>&1; then
    echo "Error: shellcheck is required." >&2
    exit 1
fi
mapfile -t shell_files < <(find scripts packaging -type f -name '*.sh' -print | sort)
shellcheck "${shell_files[@]}"
