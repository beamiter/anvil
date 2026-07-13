#!/usr/bin/env bash
# Development convenience script

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
CMD="${1:-run}"

usage() {
    echo "Usage: $0 {run|build|test|check|fmt|clippy|clean|watch}"
    echo
    echo "Commands:"
    echo "  run     - Run jterm1 in development mode"
    echo "  build   - Build release version"
    echo "  test    - Run all tests"
    echo "  check   - Check code without building"
    echo "  fmt     - Format code"
    echo "  clippy  - Lint code"
    echo "  clean   - Clean build artifacts"
    echo "  watch   - Watch for changes and rebuild"
}

case "${CMD}" in
    run|build|test|check|fmt|clippy|clean|watch) ;;
    *)
        usage
        exit 1
        ;;
esac

if ! command -v nix >/dev/null 2>&1; then
    echo "Error: Nix with flakes support is required." >&2
    exit 1
fi

cd "${PROJECT_ROOT}"

case "$CMD" in
    run)
        echo "Running jterm1 in development mode..."
        nix develop --command cargo run
        ;;

    build)
        echo "Building jterm1..."
        nix develop --command cargo build --release
        ;;

    test)
        echo "Running tests..."
        nix develop --command cargo test --all-targets
        ;;

    check)
        echo "Checking code..."
        nix develop --command cargo check --all-targets
        ;;

    fmt)
        echo "Formatting code..."
        nix develop --command cargo fmt --all
        ;;

    clippy)
        echo "Running clippy..."
        nix develop --command cargo clippy --all-targets -- -D warnings
        ;;

    clean)
        echo "Cleaning build artifacts..."
        nix develop --command cargo clean
        ;;

    watch)
        echo "Watching for changes..."
        if ! nix develop --command sh -c 'command -v cargo-watch >/dev/null 2>&1'; then
            echo "Error: cargo-watch is not installed." >&2
            echo "Install it with 'cargo install cargo-watch', then retry." >&2
            exit 1
        fi
        nix develop --command cargo watch -x run
        ;;

esac
