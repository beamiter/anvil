#!/usr/bin/env bash
# Install a prebuilt anvil release bundle for the current user.

set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${HOME}/.local/bin"
CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
DATA_HOME="${XDG_DATA_HOME:-${HOME}/.local/share}"
CONFIG_DIR="${CONFIG_HOME}/anvil"
DATA_DIR="${DATA_HOME}/anvil"
APPLICATIONS_DIR="${DATA_HOME}/applications"
DOC_DIR="${DATA_HOME}/doc/anvil"
INSTALL_TEMP=""
INSTALL_TEMP_IDENTITY=""
INSTALL_TEMP_FD=""

die() {
    printf 'anvil release install: %s\n' "$*" >&2
    exit 1
}

# Only clean up the exact regular file this invocation reserved. A changed
# staging name belongs to another writer and must not be unlinked by our trap.
install_temp_is_owned() {
    [[ -n "${INSTALL_TEMP}" && -n "${INSTALL_TEMP_IDENTITY}" \
        && -f "${INSTALL_TEMP}" && ! -L "${INSTALL_TEMP}" ]] \
        && [[ "$(stat -c '%d:%i' -- "${INSTALL_TEMP}" 2>/dev/null)" == \
            "${INSTALL_TEMP_IDENTITY}" ]]
}

cleanup_install_temp() {
    if [[ -n "${INSTALL_TEMP}" ]]; then
        if install_temp_is_owned; then
            rm -f -- "${INSTALL_TEMP}"
        elif [[ -e "${INSTALL_TEMP}" || -L "${INSTALL_TEMP}" ]]; then
            printf 'anvil release install: warning: changed staging path retained: %q\n' \
                "${INSTALL_TEMP}" >&2
        fi
        INSTALL_TEMP=""
        INSTALL_TEMP_IDENTITY=""
    fi
    if [[ -n "${INSTALL_TEMP_FD}" ]]; then
        exec {INSTALL_TEMP_FD}>&-
        INSTALL_TEMP_FD=""
    fi
}
trap cleanup_install_temp EXIT

reserve_install_temp() {
    local dest="$1" directory basename
    directory="${dest%/*}"
    basename="${dest##*/}"
    INSTALL_TEMP="$(mktemp "${directory}/.${basename}.install.XXXXXX")" \
        || die "cannot create temporary file beside ${dest}"
    INSTALL_TEMP_IDENTITY="$(stat -c '%d:%i' -- "${INSTALL_TEMP}")" \
        || die "cannot identify temporary file beside ${dest}"
    install_temp_is_owned || die "temporary file changed beside ${dest}"
    exec {INSTALL_TEMP_FD}<>"${INSTALL_TEMP}" \
        || die "cannot open temporary file beside ${dest}"
    [[ "$(stat -Lc '%d:%i' -- "/proc/self/fd/${INSTALL_TEMP_FD}")" == \
        "${INSTALL_TEMP_IDENTITY}" ]] \
        || die "temporary file changed while opening beside ${dest}"
    install_temp_is_owned || die "temporary file changed beside ${dest}"
}

# link(2), with -T to reject even a directory symlink at the destination,
# publishes without replacing an existing file, dangling link, or race winner.
install_config_if_absent() {
    local source="$1" dest="$2"
    if [[ -e "${dest}" || -L "${dest}" ]]; then
        printf 'Keeping existing configuration: %s\n' "${dest}"
        return 0
    fi
    reserve_install_temp "${dest}"
    # Write and link the bound descriptor, not a staging pathname another
    # writer could replace. mktemp creates it mode 0600 under the private umask.
    cat -- "${source}" >&"${INSTALL_TEMP_FD}" \
        || die "cannot stage configuration for ${dest}"
    install_temp_is_owned || die "configuration staging file changed for ${dest}"
    if ln -L -T -- "/proc/self/fd/${INSTALL_TEMP_FD}" "${dest}" 2>/dev/null; then
        cleanup_install_temp
        printf 'Created %s\n' "${dest}"
    elif [[ -e "${dest}" || -L "${dest}" ]]; then
        cleanup_install_temp
        printf 'Keeping concurrently created configuration: %s\n' "${dest}"
    else
        die "cannot atomically create configuration at ${dest}"
    fi
}

if [[ ! -x "${SCRIPT_DIR}/bin/anvil" ]]; then
    echo "Error: ${SCRIPT_DIR}/bin/anvil is missing or not executable." >&2
    exit 1
fi

printf 'Installing anvil for %s...\n' "${USER:-the current user}"
install -Dm755 "${SCRIPT_DIR}/bin/anvil" "${INSTALL_DIR}/anvil"
install -Dm755 "${SCRIPT_DIR}/bin/anvil-support-bundle" \
    "${INSTALL_DIR}/anvil-support-bundle"

install -d -m 0700 "${CONFIG_DIR}"
install_config_if_absent \
    "${SCRIPT_DIR}/share/doc/anvil/config.toml.example" \
    "${CONFIG_DIR}/config.toml"

# A desktop session fixes its PATH at login, so `Exec=anvil` fails TryExec and
# hides the launcher entry whenever ${INSTALL_DIR} is missing from that PATH.
# This bundle always installs per-user, so point the entry at the absolute path.
install -d -m 0755 "${APPLICATIONS_DIR}"
# The old predictable .desktop.new path may be somebody else's symlink.
# Reserve a private name and never open or remove the old path.
reserve_install_temp "${APPLICATIONS_DIR}/io.github.beamiter.anvil.desktop"
awk -v exec_path="${INSTALL_DIR}/anvil" '
    /^Exec=anvil([[:space:]]|$)/ || /^TryExec=anvil([[:space:]]|$)/ {
        eq = index($0, "=")
        print substr($0, 1, eq) exec_path substr($0, eq + 7)
        next
    }
    { print }
' "${SCRIPT_DIR}/share/applications/io.github.beamiter.anvil.desktop" \
    >&"${INSTALL_TEMP_FD}"
install_temp_is_owned || die "desktop staging file changed"
chmod 0644 -- "/proc/self/fd/${INSTALL_TEMP_FD}"
mv -fT -- "${INSTALL_TEMP}" \
    "${APPLICATIONS_DIR}/io.github.beamiter.anvil.desktop"
INSTALL_TEMP=""
INSTALL_TEMP_IDENTITY=""
exec {INSTALL_TEMP_FD}>&-
INSTALL_TEMP_FD=""
# Launchers left by installs from before the jterm1 -> anvil rename.
rm -f -- "${APPLICATIONS_DIR}/app.jterm1.desktop" \
    "${APPLICATIONS_DIR}/io.github.beamiter.jterm1.desktop"
install -Dm644 \
    "${SCRIPT_DIR}/share/metainfo/io.github.beamiter.anvil.metainfo.xml" \
    "${DATA_HOME}/metainfo/io.github.beamiter.anvil.metainfo.xml"
install -Dm644 \
    "${SCRIPT_DIR}/share/icons/hicolor/scalable/apps/io.github.beamiter.anvil.svg" \
    "${DATA_HOME}/icons/hicolor/scalable/apps/io.github.beamiter.anvil.svg"
for size in 128 256; do
    icon="${SCRIPT_DIR}/share/icons/hicolor/${size}x${size}/apps/io.github.beamiter.anvil.png"
    if [[ -f "${icon}" ]]; then
        install -Dm644 "${icon}" \
            "${DATA_HOME}/icons/hicolor/${size}x${size}/apps/io.github.beamiter.anvil.png"
    fi
done

install -d "${DATA_DIR}/shell-integration"
install -m644 "${SCRIPT_DIR}/share/anvil/shell-integration/README.md" \
    "${DATA_DIR}/shell-integration/README.md"
install -m644 "${SCRIPT_DIR}"/share/anvil/shell-integration/anvil.* \
    "${DATA_DIR}/shell-integration/"

install -d "${DATA_DIR}/workflows"
install -m644 "${SCRIPT_DIR}"/share/anvil/workflows/*.yaml \
    "${DATA_DIR}/workflows/"

install -Dm644 \
    "${SCRIPT_DIR}/share/anvil/notebooks/welcome.jtnb.md" \
    "${DATA_DIR}/notebooks/welcome.jtnb.md"
install -Dm644 "${SCRIPT_DIR}/share/doc/anvil/README.md" \
    "${DOC_DIR}/README.md"
install -Dm644 "${SCRIPT_DIR}/share/doc/anvil/Cargo.lock" \
    "${DOC_DIR}/Cargo.lock"
install -Dm644 "${SCRIPT_DIR}/share/doc/anvil/BUILDINFO" \
    "${DOC_DIR}/BUILDINFO"

# The caches below are generated files the desktop shell reads back, so they run
# under a relaxed umask instead of the owner-only one this script installs with.
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "${APPLICATIONS_DIR}/io.github.beamiter.anvil.desktop" || true
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

anvil installation complete.
  Binary:            ${INSTALL_DIR}/anvil
  Support bundle:    ${INSTALL_DIR}/anvil-support-bundle
  Configuration:     ${CONFIG_DIR}/config.toml
  Shell integration: ${DATA_DIR}/shell-integration
  Welcome notebook:  ${DATA_DIR}/notebooks/welcome.jtnb.md
  Desktop metadata:  ${DATA_HOME}/metainfo/io.github.beamiter.anvil.metainfo.xml

Make sure ${INSTALL_DIR} is in PATH, then run:
  anvil --doctor
  anvil
EOF_MESSAGE
