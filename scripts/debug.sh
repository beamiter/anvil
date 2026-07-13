#!/usr/bin/env bash
# Debug helper for jterm1.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
CONFIG_DIR="${CONFIG_HOME}/jterm1"
CONFIG_FILE="${CONFIG_DIR}/config.toml"
LEGACY_STATE_FILE="${CONFIG_DIR}/tabs.state"
RELEASE_BINARY="${PROJECT_ROOT}/target/release/jterm1"
DEBUG_BINARY="${PROJECT_ROOT}/target/debug/jterm1"
CMD="${1:-info}"

find_binary() {
    if [[ -x "${RELEASE_BINARY}" ]]; then
        printf '%s\n' "${RELEASE_BINARY}"
    elif [[ -x "${DEBUG_BINARY}" ]]; then
        printf '%s\n' "${DEBUG_BINARY}"
    elif command -v jterm1 >/dev/null 2>&1; then
        command -v jterm1
    else
        return 1
    fi
}

require_binary() {
    local binary
    if ! binary="$(find_binary)"; then
        echo "Error: no jterm1 binary found; run 'make build' first." >&2
        exit 1
    fi
    printf '%s\n' "${binary}"
}

case "${CMD}" in
    info)
        echo "jterm1 debug information"
        echo "========================"
        echo
        echo "Paths:"
        echo "  Config: ${CONFIG_FILE}"
        echo "  States: ${CONFIG_DIR}/tabs.<pid>.state"
        echo "  Binary: $(find_binary || echo 'not found')"
        echo
        echo "Session snapshots:"
        shopt -s nullglob
        state_files=("${CONFIG_DIR}"/tabs.*.state)
        [[ -f "${LEGACY_STATE_FILE}" ]] && state_files+=("${LEGACY_STATE_FILE}")
        shopt -u nullglob
        if ((${#state_files[@]} > 0)); then
            for state_file in "${state_files[@]}"; do
                echo "  $(basename -- "${state_file}"): $(wc -c < "${state_file}") bytes"
            done
        else
            echo "  None."
        fi
        echo
        echo "Configuration:"
        if [[ -f "${CONFIG_FILE}" ]]; then
            echo "  terminal_mode: $(sed -n 's/^[[:space:]]*terminal_mode[[:space:]]*=[[:space:]]*//p; T; q' "${CONFIG_FILE}")"
            echo "  theme: $(sed -n 's/^[[:space:]]*theme[[:space:]]*=[[:space:]]*//p; T; q' "${CONFIG_FILE}")"
        else
            echo "  Not present; built-in defaults are active."
        fi
        echo
        echo "Running processes:"
        if ! ps -C jterm1 -o pid=,stat=,rss=,args= 2>/dev/null; then
            echo "  None."
        fi
        ;;

    logs)
        binary="$(require_binary)"
        echo "Running jterm1 with debug logging environment..."
        RUST_BACKTRACE=1 RUST_LOG=jterm1=debug exec "${binary}"
        ;;

    trace)
        binary="$(require_binary)"
        echo "Running jterm1 with trace logging environment..."
        RUST_BACKTRACE=full RUST_LOG=jterm1=trace exec "${binary}"
        ;;

    state)
        "${SCRIPT_DIR}/show-state.sh"
        ;;

    clean-state)
        shopt -s nullglob
        state_files=("${CONFIG_DIR}"/tabs.*.state "${CONFIG_DIR}"/tabs.*.state.claim.*)
        [[ -f "${LEGACY_STATE_FILE}" ]] && state_files+=("${LEGACY_STATE_FILE}")
        shopt -u nullglob
        if ((${#state_files[@]} > 0)); then
            rm -- "${state_files[@]}"
            echo "Removed ${#state_files[@]} session snapshot(s)."
        else
            echo "No session snapshots to remove."
        fi
        ;;

    reset-config)
        mkdir -p "${CONFIG_DIR}"
        if [[ -f "${CONFIG_FILE}" ]]; then
            backup="${CONFIG_FILE}.bak.$(date +%Y%m%d%H%M%S)"
            cp -- "${CONFIG_FILE}" "${backup}"
            echo "Backed up the current config to ${backup}"
        fi
        install -m600 "${PROJECT_ROOT}/config.toml.example" "${CONFIG_FILE}"
        echo "Reset ${CONFIG_FILE}"
        ;;

    valgrind)
        command -v valgrind >/dev/null 2>&1 || {
            echo "Error: valgrind is not installed." >&2
            exit 1
        }
        binary="$(require_binary)"
        exec valgrind --leak-check=full --show-leak-kinds=all "${binary}"
        ;;

    strace)
        command -v strace >/dev/null 2>&1 || {
            echo "Error: strace is not installed." >&2
            exit 1
        }
        binary="$(require_binary)"
        trace_file="${TMPDIR:-/tmp}/jterm1-strace.log"
        strace -o "${trace_file}" "${binary}"
        echo "Trace saved to ${trace_file}"
        ;;

    *)
        echo "Usage: $0 {info|logs|trace|state|clean-state|reset-config|valgrind|strace}"
        echo
        echo "Commands:"
        echo "  info         Show paths, configuration and process information"
        echo "  logs         Run with RUST_LOG=jterm1=debug"
        echo "  trace        Run with RUST_LOG=jterm1=trace"
        echo "  state        Pretty-print the saved session"
        echo "  clean-state  Remove the saved session"
        echo "  reset-config Back up and replace the config with the example"
        echo "  valgrind     Run the built binary under valgrind"
        echo "  strace       Run the built binary under strace"
        exit 1
        ;;
esac
