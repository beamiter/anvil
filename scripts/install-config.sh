#!/usr/bin/env bash
# Shared source-install policy; kept byte-identical in the four terminals.
# Called only after the caller has validated its staging/config directory.

# 0: same fields, 10: changed fields, other: comparison unavailable. Reserve a
# distinct success result for changes so interpreter failures cannot reset a config.
# Parse TOML rather than matching lines: quoted/dotted keys, inline tables and multiline
# strings must have the same meaning as they do when the application loads them.
config_fields_match() {
    if ! command -v python3 >/dev/null 2>&1; then
        printf 'Cannot compare config fields: python3 is unavailable.\n' >&2
        return 2
    fi
    python3 - "$1" "$2" <<'PYTHON'
import sys

try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        print("Cannot compare config fields: Python needs tomllib or tomli.", file=sys.stderr)
        sys.exit(2)


def fields(value, path=()):
    keys, empty_arrays = set(), set()
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = path + (key,)
            keys.add(child_path)
            child_keys, child_arrays = fields(child, child_path)
            keys.update(child_keys)
            empty_arrays.update(child_arrays)
    elif isinstance(value, list):
        # List lengths and scalar contents are user values. Compare the union
        # of table fields, without treating a user's extra entries as new keys.
        tables = [child for child in value if isinstance(child, (dict, list))]
        if not tables:
            empty_arrays.add(path)
        for child in tables:
            child_keys, child_arrays = fields(child, path)
            keys.update(child_keys)
            empty_arrays.update(child_arrays)
    return keys, empty_arrays


try:
    with open(sys.argv[1], "rb") as source, open(sys.argv[2], "rb") as current:
        new_keys, new_empty = fields(tomllib.load(source))
        old_keys, old_empty = fields(tomllib.load(current))
except (OSError, ValueError, RecursionError):
    # Do not echo configuration contents, which may contain private values.
    print("Cannot compare config fields: unreadable or invalid TOML.", file=sys.stderr)
    sys.exit(2)

# An empty default list (for example remote_hosts = []) describes no item
# fields. It must not reset a user's populated list, including nested lists.
empty_arrays = new_empty | old_empty
def comparable(keys):
    return {key for key in keys if not any(
        len(key) > len(array) and key[:len(array)] == array
        for array in empty_arrays
    )}

sys.exit(0 if comparable(new_keys) == comparable(old_keys) else 10)
PYTHON
}

# Only changes between installed templates are schema changes. A user's optional
# tables, saved defaults, and custom list entries are not a new application schema.
keep_existing_config() {
    local source="$1" current="$2" runtime_dest="$3" installed="$4"
    local comparison=0 current_comparison=0
    config_fields_match "${source}" "${current}" || current_comparison=$?
    if ((current_comparison != 0 && current_comparison != 10)); then
        printf 'Keeping config (field comparison unavailable): %s\n' "${runtime_dest}"
        return 2
    fi
    if [[ ! -e "${installed}" ]]; then
        printf 'Keeping config (recording initial installed template): %s\n' "${runtime_dest}"
        return 0
    fi
    config_fields_match "${source}" "${installed}" || comparison=$?
    case "${comparison}" in
        0) printf 'Keeping config (template fields unchanged): %s\n' "${runtime_dest}"; return 0 ;;
        10)
            if ((current_comparison == 0)); then
                printf 'Keeping config (already has current fields): %s\n' "${runtime_dest}"
                return 0
            fi
            return 1
            ;;
        *) printf 'Keeping config (field comparison unavailable): %s\n' "${runtime_dest}"; return 2 ;;
    esac
}

install_config_with_backup() (
    set -Eeuo pipefail
    umask 077
    local source="$1" dest="$2" runtime_dest="$3" directory basename
    local temporary="" temporary_identity="" backup="" original_identity="" directory_fd=""
    local installed installed_identity="" comparison=0
    # Invoked by the EXIT trap inside this function subshell.
    # shellcheck disable=SC2317
    cleanup_config_temp() {
        if [[ -n "${temporary}" && -n "${temporary_identity}" \
            && -f "${temporary}" && ! -L "${temporary}" ]] \
            && [[ "$(stat -Lc '%d:%i' -- "${temporary}")" == "${temporary_identity}" ]]; then
            rm -f -- "${temporary}"
        fi
    }
    trap cleanup_config_temp EXIT
    directory="${dest%/*}"
    basename="${dest##*/}"
    installed="${directory}/.${basename}.installed-defaults"
    # Preserve explicit external configuration links, including dangling links.
    if [[ -L "${dest}" ]]; then
        printf 'Keeping symlinked config: %s\n' "${runtime_dest}"
        return 0
    fi
    if [[ -e "${dest}" && ! -f "${dest}" ]]; then
        die "configuration destination is not a regular file: ${runtime_dest}"
    fi
    if [[ -L "${installed}" || ( -e "${installed}" && ! -f "${installed}" ) ]]; then
        printf 'Keeping config (installed template is not a regular file): %s\n' "${runtime_dest}"
        return 0
    fi
    if ((DRY_RUN != 0)); then
        if [[ -f "${dest}" ]]; then
            keep_existing_config "${source}" "${dest}" "${runtime_dest}" "${installed}" || comparison=$?
            ((comparison == 1)) || return 0
        fi
        printf '  update config %q (back up existing contents before replacement)\n' "${runtime_dest}"
        return 0
    fi
    # Pin the destination directory so a later ancestor rename cannot redirect
    # the backup or publication into a different directory.
    exec {directory_fd}<"${directory}" || die "cannot open config directory"
    local bound="/proc/self/fd/${directory_fd}" target
    target="${bound}/${basename}"
    installed="${bound}/.${basename}.installed-defaults"
    if [[ -L "${installed}" || ( -e "${installed}" && ! -f "${installed}" ) ]]; then
        printf 'Keeping config (installed template changed type): %s\n' "${runtime_dest}"
        return 0
    fi
    if [[ -f "${installed}" ]]; then
        installed_identity="$(stat -Lc '%d:%i:%s:%y:%z' -- "${installed}")"
    fi
    # Invoked only after the config has been kept or safely published. Advance
    # the template last so a failed update is retried at the next installation.
    save_installed_template() {
        cleanup_config_temp
        temporary=""
        if [[ -n "${installed_identity}" ]]; then
            [[ ! -L "${installed}" && -f "${installed}" \
                && "$(stat -Lc '%d:%i:%s:%y:%z' -- "${installed}")" == "${installed_identity}" ]] \
                || die "installed config template changed during installation"
            if cmp -s -- "${source}" "${installed}"; then
                return 0
            fi
        fi
        temporary="$(mktemp "${bound}/.${basename}.template.XXXXXX")"
        temporary_identity="$(stat -Lc '%d:%i' -- "${temporary}")"
        install -m 0600 -- "${source}" "${temporary}"
        # install may replace the reserved inode; cleanup must track its output.
        temporary_identity="$(stat -Lc '%d:%i' -- "${temporary}")"
        if [[ -n "${installed_identity}" ]]; then
            [[ ! -L "${installed}" && -f "${installed}" \
                && "$(stat -Lc '%d:%i:%s:%y:%z' -- "${installed}")" == "${installed_identity}" ]] \
                || die "installed config template changed during installation"
            mv -fT -- "${temporary}" "${installed}"
            temporary=""
        elif ! ln -T -- "${temporary}" "${installed}" 2>/dev/null; then
            die "installed config template was concurrently created"
        fi
    }
    if [[ -L "${target}" ]]; then
        printf 'Keeping concurrently linked config: %s\n' "${runtime_dest}"
        return 0
    fi
    if [[ -e "${target}" ]]; then
        [[ -f "${target}" ]] || die "configuration destination changed type"
        original_identity="$(stat -Lc '%d:%i:%s:%y:%z' -- "${target}")"
        keep_existing_config "${source}" "${target}" "${runtime_dest}" "${installed}" || comparison=$?
        if ((comparison != 1)); then
            if ((comparison == 0)); then
                [[ ! -L "${target}" && -f "${target}" \
                    && "$(stat -Lc '%d:%i:%s:%y:%z' -- "${target}")" == "${original_identity}" ]] \
                    || die "config changed while comparing fields"
                save_installed_template
            fi
            return 0
        fi
        [[ "$(stat -Lc '%d:%i:%s:%y:%z' -- "${target}")" == "${original_identity}" ]] \
            || die "config changed while comparing fields"
        printf '  update config %q (back up existing contents before replacement)\n' "${runtime_dest}"
        backup="$(mktemp "${bound}/${basename}.backup.XXXXXX")"
        # A copy, not a hard link: later edits to the old inode cannot corrupt
        # the saved configuration. No backup path is reused or overwritten.
        cp --no-dereference -- "${target}" "${backup}"
        [[ -f "${backup}" && ! -L "${backup}" ]] || die "config changed while backing up"
        chmod 0600 -- "${backup}"
        printf 'Saved previous config: %s/%s\n' "${runtime_dest%/*}" "${backup##*/}"
    else
        printf '  update config %q\n' "${runtime_dest}"
    fi
    temporary="$(mktemp "${bound}/.${basename}.install.XXXXXX")"
    install -m 0600 -- "${source}" "${temporary}"
    temporary_identity="$(stat -Lc '%d:%i' -- "${temporary}")"
    if [[ -z "${original_identity}" ]]; then
        if ! ln -T -- "${temporary}" "${target}" 2>/dev/null; then
            if [[ -e "${target}" || -L "${target}" ]]; then
                printf 'Keeping concurrently created config: %s\n' "${runtime_dest}"
                return 0
            fi
            die "cannot create config: ${runtime_dest}"
        fi
    else
        [[ ! -L "${target}" && -f "${target}" ]] \
            || die "config changed while preparing upgrade; backup retained"
        if [[ "$(stat -Lc '%d:%i:%s:%y:%z' -- "${target}")" != "${original_identity}" ]] \
            || ! cmp -s -- "${backup}" "${target}"; then
            die "config changed while preparing upgrade; backup retained"
        fi
        mv -fT -- "${temporary}" "${target}"
        temporary=""
    fi
    save_installed_template
)
