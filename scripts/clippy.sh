#!/usr/bin/env bash
# Canonical repository lint policy. CI and local development both call this
# script so their warning baselines cannot silently drift apart.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

cargo clippy --all-targets --locked -- \
    -D warnings \
    -A clippy::type_complexity \
    -A clippy::doc_lazy_continuation \
    -A clippy::items_after_test_module \
    -A clippy::too_many_arguments \
    -A clippy::manual_clamp \
    -A clippy::if_same_then_else \
    -A clippy::needless_range_loop
