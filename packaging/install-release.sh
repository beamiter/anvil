#!/usr/bin/env bash
# Install a prebuilt jterm1 release bundle for the current user.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${HOME}/.local/bin"
CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
DATA_HOME="${XDG_DATA_HOME:-${HOME}/.local/share}"
CONFIG_DIR="${CONFIG_HOME}/jterm1"
DATA_DIR="${DATA_HOME}/jterm1"
APPLICATIONS_DIR="${DATA_HOME}/applications"
DOC_DIR="${DATA_HOME}/doc/jterm1"

if [[ ! -x "${SCRIPT_DIR}/bin/jterm1" ]]; then
    echo "Error: ${SCRIPT_DIR}/bin/jterm1 is missing or not executable." >&2
    exit 1
fi

printf 'Installing jterm1 for %s...\n' "${USER:-the current user}"
install -Dm755 "${SCRIPT_DIR}/bin/jterm1" "${INSTALL_DIR}/jterm1"

mkdir -p "${CONFIG_DIR}"
if [[ ! -e "${CONFIG_DIR}/config.toml" ]]; then
    install -m600 \
        "${SCRIPT_DIR}/share/doc/jterm1/config.toml.example" \
        "${CONFIG_DIR}/config.toml"
    echo "Created ${CONFIG_DIR}/config.toml"
else
    echo "Keeping existing configuration: ${CONFIG_DIR}/config.toml"
fi

install -Dm644 \
    "${SCRIPT_DIR}/share/applications/app.jterm1.desktop" \
    "${APPLICATIONS_DIR}/app.jterm1.desktop"

install -d "${DATA_DIR}/shell-integration"
install -m644 "${SCRIPT_DIR}"/share/jterm1/shell-integration/jterm1.* \
    "${DATA_DIR}/shell-integration/"

install -d "${DATA_DIR}/workflows"
install -m644 "${SCRIPT_DIR}"/share/jterm1/workflows/*.yaml \
    "${DATA_DIR}/workflows/"

install -Dm644 \
    "${SCRIPT_DIR}/share/jterm1/notebooks/welcome.jtnb.md" \
    "${DATA_DIR}/notebooks/welcome.jtnb.md"
install -Dm644 "${SCRIPT_DIR}/share/doc/jterm1/README.md" \
    "${DOC_DIR}/README.md"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${APPLICATIONS_DIR}" >/dev/null 2>&1 || true
fi

cat <<EOF_MESSAGE

jterm1 installation complete.
  Binary:            ${INSTALL_DIR}/jterm1
  Configuration:     ${CONFIG_DIR}/config.toml
  Shell integration: ${DATA_DIR}/shell-integration
  Welcome notebook:  ${DATA_DIR}/notebooks/welcome.jtnb.md

Make sure ${INSTALL_DIR} is in PATH, then run:
  jterm1 --doctor
  jterm1
EOF_MESSAGE
