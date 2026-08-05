#!/usr/bin/env bash
# Remove anvil while preserving user configuration and state by default.

set -Eeuo pipefail

APP_ID="io.github.beamiter.anvil"
HOME_DIR="${HOME:-}"
DESTDIR="${DESTDIR:-}"
PREFIX="${HOME_DIR}/.local"
BIN_DIR=""
DATA_HOME=""
PURGE_CONFIG=0
DRY_RUN=0

usage() {
    cat <<'USAGE'
Usage:
  ./scripts/uninstall.sh [options]  # source checkout
  ./uninstall.sh [options]          # extracted release bundle

Options:
  --prefix PATH          Runtime prefix (default: ~/.local)
  --bin-dir PATH         Runtime binary directory (overrides --prefix)
  --data-dir PATH        Shared-data base (default: $XDG_DATA_HOME or PREFIX/share)
  --purge-config         Also remove anvil config and default XDG state
  --dry-run              Print commands without changing files
  -h, --help             Show this help

Environment:
  DESTDIR                Optional staging root for packaging
  XDG_CONFIG_HOME        Config base (default: ~/.config)
  XDG_DATA_HOME          Shared-data base (default: PREFIX/share)
  XDG_STATE_HOME         State base (default: ~/.local/state)
USAGE
}

die() {
    printf 'anvil uninstall: %s\n' "$*" >&2
    exit 1
}

run() {
    printf '  '
    printf '%q ' "$@"
    printf '\n'
    if ((DRY_RUN == 0)); then
        "$@"
    fi
}

remove_file() {
    local path="$1"
    if [[ -e "${path}" || -L "${path}" ]]; then
        run rm -f -- "${path}"
    fi
}

remove_dir_if_empty() {
    local path="$1"
    if [[ -d "${path}" ]]; then
        run rmdir --ignore-fail-on-non-empty -- "${path}"
    fi
}

while (($# > 0)); do
    case "$1" in
        --prefix)
            (($# >= 2)) || die "--prefix requires a path"
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#*=}"
            shift
            ;;
        --bin-dir)
            (($# >= 2)) || die "--bin-dir requires a path"
            BIN_DIR="$2"
            shift 2
            ;;
        --bin-dir=*)
            BIN_DIR="${1#*=}"
            shift
            ;;
        --data-dir)
            (($# >= 2)) || die "--data-dir requires a path"
            DATA_HOME="$2"
            shift 2
            ;;
        --data-dir=*)
            DATA_HOME="${1#*=}"
            shift
            ;;
        --purge-config)
            PURGE_CONFIG=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            (($# == 0)) || die "unexpected positional arguments: $*"
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

[[ -n "${HOME_DIR}" ]] || die "HOME is not set"
[[ -n "${PREFIX}" && "${PREFIX}" == /* ]] || die "--prefix must be an absolute path"
if [[ -z "${BIN_DIR}" ]]; then
    BIN_DIR="${PREFIX}/bin"
fi
[[ "${BIN_DIR}" == /* ]] || die "--bin-dir must be an absolute path"
if [[ -z "${DATA_HOME}" ]]; then
    DATA_HOME="${XDG_DATA_HOME:-${PREFIX}/share}"
fi
[[ "${DATA_HOME}" == /* ]] || die "--data-dir/XDG_DATA_HOME must be an absolute path"
if [[ -n "${DESTDIR}" ]]; then
    [[ "${DESTDIR}" == /* ]] || die "DESTDIR must be an absolute path"
    DESTDIR="${DESTDIR%/}"
fi

remove_file "${DESTDIR}${BIN_DIR}/anvil"
remove_file "${DESTDIR}${BIN_DIR}/anvil-support-bundle"
SHARE_DIR="${DESTDIR}${DATA_HOME}"
remove_file "${SHARE_DIR}/applications/${APP_ID}.desktop"
# Desktop integration from before the jterm1 -> anvil rename.
remove_file "${SHARE_DIR}/applications/app.jterm1.desktop"
remove_file "${SHARE_DIR}/applications/io.github.beamiter.jterm1.desktop"
remove_file "${SHARE_DIR}/metainfo/io.github.beamiter.jterm1.metainfo.xml"
remove_file "${SHARE_DIR}/icons/hicolor/scalable/apps/io.github.beamiter.jterm1.svg"
remove_file "${SHARE_DIR}/icons/hicolor/128x128/apps/io.github.beamiter.jterm1.png"
remove_file "${SHARE_DIR}/icons/hicolor/256x256/apps/io.github.beamiter.jterm1.png"
remove_file "${SHARE_DIR}/metainfo/${APP_ID}.metainfo.xml"
remove_file "${SHARE_DIR}/icons/hicolor/scalable/apps/${APP_ID}.svg"
remove_file "${SHARE_DIR}/icons/hicolor/128x128/apps/${APP_ID}.png"
remove_file "${SHARE_DIR}/icons/hicolor/256x256/apps/${APP_ID}.png"
for file in README.md anvil.bash anvil.zsh anvil.fish anvil.ps1; do
    remove_file "${SHARE_DIR}/anvil/shell-integration/${file}"
done
for file in docker-tail-logs.yaml find-large-files.yaml git-feature.yaml git-rebase-interactive.yaml kill-port.yaml ssh-tunnel.yaml; do
    remove_file "${SHARE_DIR}/anvil/workflows/${file}"
done
remove_file "${SHARE_DIR}/anvil/notebooks/welcome.jtnb.md"
remove_file "${SHARE_DIR}/doc/anvil/README.md"
remove_file "${SHARE_DIR}/doc/anvil/Cargo.lock"
remove_file "${SHARE_DIR}/doc/anvil/BUILDINFO"
remove_dir_if_empty "${SHARE_DIR}/anvil/shell-integration"
remove_dir_if_empty "${SHARE_DIR}/anvil/workflows"
remove_dir_if_empty "${SHARE_DIR}/anvil/notebooks"
remove_dir_if_empty "${SHARE_DIR}/anvil"
remove_dir_if_empty "${SHARE_DIR}/doc/anvil"
remove_dir_if_empty "${SHARE_DIR}/doc"

# Without this the launcher keeps offering a dead entry and a cached icon.
if [[ -z "${DESTDIR}" ]] && ((DRY_RUN == 0)); then
    if command -v update-desktop-database >/dev/null 2>&1 \
        && [[ -d "${SHARE_DIR}/applications" ]]; then
        (umask 022 && update-desktop-database "${SHARE_DIR}/applications") \
            >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && [[ -d "${SHARE_DIR}/icons/hicolor" ]]; then
        (umask 022 && gtk-update-icon-cache --force --ignore-theme-index --quiet \
            "${SHARE_DIR}/icons/hicolor") >/dev/null 2>&1 || true
    fi
fi

if ((PURGE_CONFIG == 1)); then
    CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME_DIR}/.config}"
    STATE_HOME="${XDG_STATE_HOME:-${HOME_DIR}/.local/state}"
    [[ "${CONFIG_HOME}" == /* ]] || die "XDG_CONFIG_HOME must be an absolute path"
    [[ "${STATE_HOME}" == /* ]] || die "XDG_STATE_HOME must be an absolute path"
    for directory in "${DESTDIR}${CONFIG_HOME}/anvil" "${DESTDIR}${STATE_HOME}/anvil"; do
        if [[ -e "${directory}" ]]; then
            run rm -rf -- "${directory}"
        fi
    done
else
    printf 'Preserved config and state. Use --purge-config to remove them.\n'
fi
