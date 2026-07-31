#!/usr/bin/env bash
# Install a prebuilt jterm1 release bundle for the current user.

set -euo pipefail
umask 077

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
install -Dm755 "${SCRIPT_DIR}/bin/jterm1-support-bundle" \
    "${INSTALL_DIR}/jterm1-support-bundle"

install -d -m 0700 "${CONFIG_DIR}"
if [[ ! -e "${CONFIG_DIR}/config.toml" ]]; then
    install -m600 \
        "${SCRIPT_DIR}/share/doc/jterm1/config.toml.example" \
        "${CONFIG_DIR}/config.toml"
    echo "Created ${CONFIG_DIR}/config.toml"
else
    echo "Keeping existing configuration: ${CONFIG_DIR}/config.toml"
fi

# A desktop session fixes its PATH at login, so `Exec=jterm1` fails TryExec and
# hides the launcher entry whenever ${INSTALL_DIR} is missing from that PATH.
# This bundle always installs per-user, so point the entry at the absolute path.
install -d -m 0755 "${APPLICATIONS_DIR}"
awk -v exec_path="${INSTALL_DIR}/jterm1" '
    /^Exec=jterm1([[:space:]]|$)/ || /^TryExec=jterm1([[:space:]]|$)/ {
        eq = index($0, "=")
        print substr($0, 1, eq) exec_path substr($0, eq + 7)
        next
    }
    { print }
' "${SCRIPT_DIR}/share/applications/io.github.beamiter.jterm1.desktop" \
    >"${APPLICATIONS_DIR}/io.github.beamiter.jterm1.desktop.new"
chmod 0644 "${APPLICATIONS_DIR}/io.github.beamiter.jterm1.desktop.new"
mv -f -- "${APPLICATIONS_DIR}/io.github.beamiter.jterm1.desktop.new" \
    "${APPLICATIONS_DIR}/io.github.beamiter.jterm1.desktop"
rm -f -- "${APPLICATIONS_DIR}/app.jterm1.desktop"
install -Dm644 \
    "${SCRIPT_DIR}/share/metainfo/io.github.beamiter.jterm1.metainfo.xml" \
    "${DATA_HOME}/metainfo/io.github.beamiter.jterm1.metainfo.xml"
install -Dm644 \
    "${SCRIPT_DIR}/share/icons/hicolor/scalable/apps/io.github.beamiter.jterm1.svg" \
    "${DATA_HOME}/icons/hicolor/scalable/apps/io.github.beamiter.jterm1.svg"
for size in 128 256; do
    icon="${SCRIPT_DIR}/share/icons/hicolor/${size}x${size}/apps/io.github.beamiter.jterm1.png"
    if [[ -f "${icon}" ]]; then
        install -Dm644 "${icon}" \
            "${DATA_HOME}/icons/hicolor/${size}x${size}/apps/io.github.beamiter.jterm1.png"
    fi
done

install -d "${DATA_DIR}/shell-integration"
install -m644 "${SCRIPT_DIR}/share/jterm1/shell-integration/README.md" \
    "${DATA_DIR}/shell-integration/README.md"
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
install -Dm644 "${SCRIPT_DIR}/share/doc/jterm1/Cargo.lock" \
    "${DOC_DIR}/Cargo.lock"
install -Dm644 "${SCRIPT_DIR}/share/doc/jterm1/BUILDINFO" \
    "${DOC_DIR}/BUILDINFO"

# The caches below are generated files the desktop shell reads back, so they run
# under a relaxed umask instead of the owner-only one this script installs with.
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "${APPLICATIONS_DIR}/io.github.beamiter.jterm1.desktop" || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    (umask 022 && update-desktop-database "${APPLICATIONS_DIR}") >/dev/null 2>&1 || true
fi
# A stale icon cache shadows the icons installed above, so always rebuild it.
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    (umask 022 && gtk-update-icon-cache --force --ignore-theme-index --quiet \
        "${DATA_HOME}/icons/hicolor") >/dev/null 2>&1 || true
fi

cat <<EOF_MESSAGE

jterm1 installation complete.
  Binary:            ${INSTALL_DIR}/jterm1
  Support bundle:    ${INSTALL_DIR}/jterm1-support-bundle
  Configuration:     ${CONFIG_DIR}/config.toml
  Shell integration: ${DATA_DIR}/shell-integration
  Welcome notebook:  ${DATA_DIR}/notebooks/welcome.jtnb.md
  Desktop metadata:  ${DATA_HOME}/metainfo/io.github.beamiter.jterm1.metainfo.xml

Make sure ${INSTALL_DIR} is in PATH, then run:
  jterm1 --doctor
  jterm1
EOF_MESSAGE
