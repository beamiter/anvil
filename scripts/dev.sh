#!/usr/bin/env bash
# Development convenience script.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
CMD="${1:-run}"

usage() {
    echo "Usage: $0 {run|build|test|check|fmt|clippy|security|verify|package|clean|watch}"
    echo
    echo "Commands:"
    echo "  run      - Run jterm1 in development mode"
    echo "  build    - Build the optimized release binary"
    echo "  test     - Run all tests"
    echo "  check    - Check all Rust targets"
    echo "  fmt      - Format the Rust source"
    echo "  clippy   - Run the repository lint policy"
    echo "  security - Audit dependencies and shell scripts"
    echo "  verify   - Run formatting, checks, tests, lints, and docs"
    echo "  package  - Build a relocatable release archive and checksum"
    echo "  clean    - Clean build artifacts"
    echo "  watch    - Watch for changes and rebuild"
}

case "${CMD}" in
    run|build|test|check|fmt|clippy|security|verify|package|clean|watch) ;;
    *)
        usage
        exit 1
        ;;
esac

if ! command -v nix >/dev/null 2>&1; then
    echo "Error: Nix with flakes support is required." >&2
    echo "Install Nix, then retry. CI uses the same flake environment." >&2
    exit 1
fi

cd "${PROJECT_ROOT}"

run_in_nix() {
    nix develop --command "$@"
}

case "${CMD}" in
    run)
        echo "Running jterm1 in development mode..."
        run_in_nix cargo run --locked
        ;;

    build)
        echo "Building jterm1..."
        run_in_nix cargo build --release --locked
        ;;

    test)
        echo "Running tests..."
        run_in_nix cargo test --all-targets --locked --no-fail-fast
        ;;

    check)
        echo "Checking code..."
        run_in_nix cargo check --all-targets --locked
        ;;

    fmt)
        echo "Formatting code..."
        run_in_nix cargo fmt --all
        ;;

    clippy)
        echo "Running Clippy..."
        run_in_nix bash scripts/clippy.sh
        ;;

    security)
        echo "Running dependency and shell-script security checks..."
        run_in_nix bash scripts/security-check.sh
        ;;

    verify)
        echo "Running the complete quality gate..."
        run_in_nix bash -c '
            set -euo pipefail
            cargo fmt --all -- --check
            cargo check --all-targets --locked
            cargo test --all-targets --locked --no-fail-fast
            bash scripts/clippy.sh
            cargo doc --no-deps --locked
        '
        ;;

    package)
        echo "Building a relocatable release bundle..."
        run_in_nix bash -c '
            set -euo pipefail
            cargo build --release --locked
            bash scripts/package-release.sh target/release/jterm1
        '
        ;;

    clean)
        echo "Cleaning build artifacts..."
        run_in_nix cargo clean
        ;;

    watch)
        echo "Watching for changes..."
        run_in_nix cargo watch -x "run --locked"
        ;;
esac
