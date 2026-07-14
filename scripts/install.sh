#!/usr/bin/env bash
# Build and install jterm1 for the current user.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
INSTALL_DIR="${HOME}/.local/bin"
CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
DATA_HOME="${XDG_DATA_HOME:-${HOME}/.local/share}"
CONFIG_DIR="${CONFIG_HOME}/jterm1"
DATA_DIR="${DATA_HOME}/jterm1"
APPLICATIONS_DIR="${DATA_HOME}/applications"
BINARY="${PROJECT_ROOT}/target/release/jterm1"

echo "Installing jterm1..."

if ! command -v nix >/dev/null 2>&1; then
    echo "Error: Nix with flakes support is required." >&2
    echo "Install Nix from https://nixos.org/download/ and try again." >&2
    exit 1
fi

echo "Building the release binary..."
(
    cd "${PROJECT_ROOT}"
    nix develop --command cargo build --release
)

if [[ ! -x "${BINARY}" ]]; then
    echo "Error: build completed without creating ${BINARY}" >&2
    exit 1
fi

echo "Installing ${INSTALL_DIR}/jterm1..."
install -Dm755 "${BINARY}" "${INSTALL_DIR}/jterm1"
install -Dm755 "${PROJECT_ROOT}/scripts/support-bundle.sh" \
    "${INSTALL_DIR}/jterm1-support-bundle"

mkdir -p "${CONFIG_DIR}"
if [[ ! -e "${CONFIG_DIR}/config.toml" ]]; then
    echo "Creating ${CONFIG_DIR}/config.toml..."
    install -m600 "${PROJECT_ROOT}/config.toml.example" "${CONFIG_DIR}/config.toml"
else
    echo "Keeping existing config: ${CONFIG_DIR}/config.toml"
fi

echo "Installing desktop entry and user-facing examples..."
install -Dm644 \
    "${PROJECT_ROOT}/packaging/app.jterm1.desktop" \
    "${APPLICATIONS_DIR}/app.jterm1.desktop"

SHELL_INTEGRATION_DIR="${DATA_DIR}/shell-integration"
mkdir -p "${SHELL_INTEGRATION_DIR}"
for source_file in "${PROJECT_ROOT}"/scripts/shell-integration/jterm1.*; do
    install -m644 "${source_file}" "${SHELL_INTEGRATION_DIR}/$(basename -- "${source_file}")"
done

WORKFLOW_DIR="${DATA_DIR}/workflows"
mkdir -p "${WORKFLOW_DIR}"
for workflow in "${PROJECT_ROOT}"/scripts/workflows/*.yaml; do
    install -m644 "${workflow}" "${WORKFLOW_DIR}/$(basename -- "${workflow}")"
done

install -Dm644 \
    "${PROJECT_ROOT}/scripts/notebooks/welcome.jtnb.md" \
    "${DATA_DIR}/notebooks/welcome.jtnb.md"

echo
echo "jterm1 installation complete."
echo "  Binary:            ${INSTALL_DIR}/jterm1"
echo "  Support bundle:    ${INSTALL_DIR}/jterm1-support-bundle"
echo "  Configuration:     ${CONFIG_DIR}/config.toml"
echo "  Shell integration: ${SHELL_INTEGRATION_DIR}"
echo "  Welcome notebook:  ${DATA_DIR}/notebooks/welcome.jtnb.md"
echo
echo "Make sure ${INSTALL_DIR} is in PATH, then run: jterm1"
echo "Use Ctrl+Shift+P for the command palette and Ctrl+Shift+O for settings."
