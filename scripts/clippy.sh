#!/usr/bin/env bash
# Canonical repository lint policy. CI and local development both call this
# script so their warning baselines cannot silently drift apart.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

# No blanket -A allowlist. It had grown to seven lints, two of which the
# handoff simultaneously described as blocking the release check, so the
# documented gate and the gate CI runs disagreed about whether the tree was
# clean. Every sibling (forge, ember, frost, jsh, jagent, jterm_core) runs bare
# `-D warnings`; anvil now does too. A genuinely unavoidable lint gets a local
# #[allow] with a comment saying why, where a reviewer can see it.
cargo clippy --all-targets --all-features --locked -- -D warnings
