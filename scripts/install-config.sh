#!/usr/bin/env bash
# Shared source-install policy; kept byte-identical in the four terminals.
# Called only after the caller has validated its staging/config directory.
install_config_with_backup() (
    set -Eeuo pipefail
    umask 077
    local source="$1" dest="$2" runtime_dest="$3" directory basename
    local temporary="" temporary_identity="" backup="" original_identity="" directory_fd=""
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
    # Preserve explicit external configuration links, including dangling links.
    if [[ -L "${dest}" ]]; then
        printf 'Keeping symlinked config: %s\n' "${runtime_dest}"
        return 0
    fi
    if [[ -e "${dest}" && ! -f "${dest}" ]]; then
        die "configuration destination is not a regular file: ${runtime_dest}"
    fi
    if [[ -f "${dest}" ]] && cmp -s -- "${source}" "${dest}"; then
        printf 'Config already current: %s\n' "${runtime_dest}"
        return 0
    fi
    printf '  update config %q (back up existing contents before replacement)\n' "${runtime_dest}"
    ((DRY_RUN == 0)) || return 0
    # Pin the destination directory so a later ancestor rename cannot redirect
    # the backup or publication into a different directory.
    exec {directory_fd}<"${directory}" || die "cannot open config directory"
    local bound="/proc/self/fd/${directory_fd}" target
    target="${bound}/${basename}"
    if [[ -L "${target}" ]]; then
        printf 'Keeping concurrently linked config: %s\n' "${runtime_dest}"
        return 0
    fi
    if [[ -e "${target}" ]]; then
        [[ -f "${target}" ]] || die "configuration destination changed type"
        original_identity="$(stat -Lc '%d:%i:%s:%y:%z' -- "${target}")"
        backup="$(mktemp "${bound}/${basename}.backup.XXXXXX")"
        # A copy, not a hard link: later edits to the old inode cannot corrupt
        # the saved configuration. No backup path is reused or overwritten.
        cp --no-dereference -- "${target}" "${backup}"
        [[ -f "${backup}" && ! -L "${backup}" ]] || die "config changed while backing up"
        chmod 0600 -- "${backup}"
        printf 'Saved previous config: %s/%s\n' "${runtime_dest%/*}" "${backup##*/}"
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
)
