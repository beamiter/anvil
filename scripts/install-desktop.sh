#!/usr/bin/env bash
# Shared desktop entry encoding for source and prebuilt-release installers.
# Callers provide die() and own atomic publication of the rendered output.

desktop_exec_value() {
    local remaining="$1" escaped="" character
    if [[ "${remaining}" == anvil ]]; then
        printf 'anvil'
        return
    fi
    while [[ -n "${remaining}" ]]; do
        character="${remaining:0:1}"
        remaining="${remaining:1}"
        case "${character}" in
            \\) escaped="${escaped}\\\\\\\\" ;;
            # Desktop string decoding runs before command-line unquoting.
            '"'|'`'|'$') escaped+="\\\\${character}" ;;
            *) escaped+="${character}" ;;
        esac
    done
    printf '"%s"' "${escaped}"
}

desktop_try_exec_value() {
    local remaining="$1" escaped="" character
    while [[ -n "${remaining}" ]]; do
        character="${remaining:0:1}"
        remaining="${remaining:1}"
        case "${character}" in
            \\) escaped="${escaped}\\\\" ;;
            *) escaped+="${character}" ;;
        esac
    done
    printf '%s' "${escaped}"
}

validate_desktop_exec_path() {
    local path="$1"
    [[ "${path}" != *'='* ]] \
        || die "desktop executable path must not contain '=': ${path}"
    [[ "${path}" != *'%'* ]] \
        || die "desktop executable path must not contain '%': ${path}"
    if [[ "${path}" =~ [[:cntrl:]] ]]; then
        die "desktop executable path must not contain control characters"
    fi
}

render_desktop_entry() {
    local source="$1" exec_path="$2" exec_value try_exec_value
    validate_desktop_exec_path "${exec_path}"
    exec_value="$(desktop_exec_value "${exec_path}")"
    try_exec_value="$(desktop_try_exec_value "${exec_path}")"
    ANVIL_DESKTOP_EXEC_VALUE="${exec_value}" \
        ANVIL_DESKTOP_TRY_EXEC_VALUE="${try_exec_value}" \
        awk '
        BEGIN { exec_count = 0; try_exec_count = 0 }
        /^Exec=anvil([[:space:]]|$)/ {
            exec_count++
            eq = index($0, "=")
            print substr($0, 1, eq) ENVIRON["ANVIL_DESKTOP_EXEC_VALUE"] \
                substr($0, eq + 6)
            next
        }
        /^TryExec=anvil([[:space:]]|$)/ {
            try_exec_count++
            eq = index($0, "=")
            print substr($0, 1, eq) ENVIRON["ANVIL_DESKTOP_TRY_EXEC_VALUE"] \
                substr($0, eq + 6)
            next
        }
        /^Exec=/ { exit 45 }
        /^TryExec=/ { exit 46 }
        { print }
        END {
            if (exec_count < 1 || try_exec_count != 1) exit 44
        }
    ' "${source}"
}
