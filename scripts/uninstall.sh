#!/usr/bin/env bash
# Remove anvil while preserving user configuration and state by default.

set -Eeuo pipefail

APP_ID="io.github.beamiter.anvil"
HOME_DIR="${HOME:-}"
DESTDIR="${DESTDIR:-}"
DESTDIR_ACTIVE=0
if [[ -n "${DESTDIR}" ]]; then
    DESTDIR_ACTIVE=1
fi
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
  --bin-dir PATH         Runtime binary directory (default: PREFIX/bin)
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
    validate_staging_removal_target "${path}"
    if [[ -e "${path}" || -L "${path}" ]]; then
        run rm -f -- "${path}"
    fi
}

remove_dir_if_empty() {
    local path="$1"
    validate_staging_removal_target "${path}"
    if [[ -d "${path}" ]]; then
        run rmdir --ignore-fail-on-non-empty -- "${path}"
    fi
}

validate_absolute_path() {
    local label="$1" path="$2"
    [[ -n "${path}" ]] || die "${label} must not be empty"
    [[ "${path}" == /* ]] || die "${label} must be an absolute path"
    if [[ "${path}" =~ [[:cntrl:]] ]]; then
        die "${label} must not contain control characters"
    fi
    case "/${path#/}/" in
        */../*) die "${label} must not contain '..' path components" ;;
    esac
}

normalize_absolute_path() {
    local path="$1" normalized="" component
    local -a components=()
    IFS='/' read -r -a components <<<"${path}"
    for component in "${components[@]}"; do
        [[ -n "${component}" && "${component}" != . ]] || continue
        normalized="${normalized}/${component}"
    done
    printf '%s' "${normalized:-/}"
}

# Full-chain point-in-time check for the normalized staging boundary.
validate_destdir_root() {
    local suffix current="" component
    local -a components=()
    ((DESTDIR_ACTIVE == 1)) || return 0
    [[ -n "${DESTDIR}" && "${DESTDIR}" != / ]] || return 0
    suffix="${DESTDIR#/}"
    IFS='/' read -r -a components <<<"${suffix}"
    for component in "${components[@]}"; do
        [[ -n "${component}" ]] || continue
        current="${current}/${component}"
        [[ ! -L "${current}" ]] \
            || die "DESTDIR path contains a symbolic-link component: ${current}"
        [[ -e "${current}" ]] || break
    done
}

# Refuse a directory symlink in the parent chain of a staged removal.  The
# final component may itself be a symlink because rm removes that link without
# following it; an ancestor link would redirect deletion outside DESTDIR.
validate_staging_removal_target() {
    local target="$1" parent suffix current component
    local -a components=()
    ((DESTDIR_ACTIVE == 1)) || return 0
    [[ -n "${DESTDIR}" ]] || return 0
    validate_destdir_root
    case "${target}" in
        "${DESTDIR}"/*) ;;
        *) die "staged uninstall target is outside DESTDIR: ${target}" ;;
    esac
    parent="${target%/*}"
    suffix="${parent#"${DESTDIR}"}"
    suffix="${suffix#/}"
    current="${DESTDIR}"
    IFS='/' read -r -a components <<<"${suffix}"
    for component in "${components[@]}"; do
        [[ -n "${component}" && "${component}" != . ]] || continue
        current="${current}/${component}"
        [[ ! -L "${current}" ]] \
            || die "staged uninstall path contains a symbolic-link ancestor: ${current}"
        [[ -e "${current}" ]] || break
    done
}

while (($# > 0)); do
    case "$1" in
        --prefix)
            (($# >= 2)) || die "--prefix requires a path"
            PREFIX="$2"
            [[ -n "${PREFIX}" ]] || die "--prefix must not be empty"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#*=}"
            [[ -n "${PREFIX}" ]] || die "--prefix must not be empty"
            shift
            ;;
        --bin-dir)
            (($# >= 2)) || die "--bin-dir requires a path"
            BIN_DIR="$2"
            [[ -n "${BIN_DIR}" ]] || die "--bin-dir must not be empty"
            shift 2
            ;;
        --bin-dir=*)
            BIN_DIR="${1#*=}"
            [[ -n "${BIN_DIR}" ]] || die "--bin-dir must not be empty"
            shift
            ;;
        --data-dir)
            (($# >= 2)) || die "--data-dir requires a path"
            DATA_HOME="$2"
            [[ -n "${DATA_HOME}" ]] || die "--data-dir must not be empty"
            shift 2
            ;;
        --data-dir=*)
            DATA_HOME="${1#*=}"
            [[ -n "${DATA_HOME}" ]] || die "--data-dir must not be empty"
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
validate_absolute_path "--prefix" "${PREFIX}"
if [[ -z "${BIN_DIR}" ]]; then
    BIN_DIR="${PREFIX}/bin"
fi
validate_absolute_path "--bin-dir" "${BIN_DIR}"
if [[ -z "${DATA_HOME}" ]]; then
    DATA_HOME="${XDG_DATA_HOME:-${PREFIX}/share}"
fi
validate_absolute_path "--data-dir/XDG_DATA_HOME" "${DATA_HOME}"
if ((DESTDIR_ACTIVE == 1)); then
    validate_absolute_path "DESTDIR" "${DESTDIR}"
    DESTDIR="$(normalize_absolute_path "${DESTDIR}")"
    validate_destdir_root
    if [[ "${DESTDIR}" == / ]]; then
        DESTDIR=""
    fi
fi

if ((PURGE_CONFIG == 1)); then
    CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME_DIR}/.config}"
    STATE_HOME="${XDG_STATE_HOME:-${HOME_DIR}/.local/state}"
    validate_absolute_path "XDG_CONFIG_HOME" "${CONFIG_HOME}"
    validate_absolute_path "XDG_STATE_HOME" "${STATE_HOME}"
    validate_staging_removal_target "${DESTDIR}${CONFIG_HOME}/anvil"
    validate_staging_removal_target "${DESTDIR}${STATE_HOME}/anvil"
fi

remove_file "${DESTDIR}${BIN_DIR}/anvil"
remove_file "${DESTDIR}${BIN_DIR}/anvil-support-bundle"
LEGACY_SOURCE_BIN="${HOME_DIR}/.cargo/bin/anvil"
if ((DESTDIR_ACTIVE == 0)) \
    && [[ "${LEGACY_SOURCE_BIN}" != "${BIN_DIR}/anvil" ]] \
    && [[ -e "${LEGACY_SOURCE_BIN}" || -L "${LEGACY_SOURCE_BIN}" ]]; then
    printf 'Note: legacy source install left untouched at %s.\n' "${LEGACY_SOURCE_BIN}"
fi
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
if ((DESTDIR_ACTIVE == 0 && DRY_RUN == 0)); then
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
    for directory in "${DESTDIR}${CONFIG_HOME}/anvil" "${DESTDIR}${STATE_HOME}/anvil"; do
        validate_staging_removal_target "${directory}"
        if [[ -e "${directory}" || -L "${directory}" ]]; then
            run rm -rf -- "${directory}"
        fi
    done
else
    printf 'Preserved config and state. Use --purge-config to remove them.\n'
fi
