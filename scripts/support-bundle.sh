#!/usr/bin/env bash
# Create a privacy-preserving jterm1 support archive.

set -euo pipefail
umask 077

usage() {
    echo "Usage: $0 [OUTPUT_DIRECTORY]" >&2
}

if (( $# > 1 )); then
    usage
    exit 2
fi

OUTPUT_DIR="${1:-.}"
JTERM1_BIN="${JTERM1_BIN:-jterm1}"
if ! command -v "${JTERM1_BIN}" >/dev/null 2>&1; then
    echo "Error: jterm1 executable not found: ${JTERM1_BIN}" >&2
    exit 1
fi

mkdir -p "${OUTPUT_DIR}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BUNDLE_NAME="jterm1-support-${STAMP}"
WORK_DIR="$(mktemp -d)"
BUNDLE_DIR="${WORK_DIR}/${BUNDLE_NAME}"
trap 'rm -rf -- "${WORK_DIR}"' EXIT
mkdir -p "${BUNDLE_DIR}"

human_status=0
json_status=0
config_status=0
config_json_status=0
"${JTERM1_BIN}" --doctor >"${BUNDLE_DIR}/doctor.txt" 2>&1 || human_status=$?
"${JTERM1_BIN}" --doctor --json >"${BUNDLE_DIR}/doctor.json" 2>"${BUNDLE_DIR}/doctor-json.stderr" || json_status=$?
"${JTERM1_BIN}" --check-config >"${BUNDLE_DIR}/config-check.txt" 2>&1 || config_status=$?
"${JTERM1_BIN}" --check-config --json >"${BUNDLE_DIR}/config-check.json" 2>"${BUNDLE_DIR}/config-check-json.stderr" || config_json_status=$?

binary_path="$(command -v "${JTERM1_BIN}")"
version="$("${JTERM1_BIN}" --version 2>&1 || true)"
config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
data_home="${XDG_DATA_HOME:-${HOME}/.local/share}"
state_home="${XDG_STATE_HOME:-${HOME}/.local/state}"

{
    printf 'generated_at_utc=%s\n' "${STAMP}"
    printf 'version=%s\n' "${version}"
    printf 'binary=%s\n' "${binary_path}"
    printf 'doctor_exit=%s\n' "${human_status}"
    printf 'doctor_json_exit=%s\n' "${json_status}"
    printf 'config_check_exit=%s\n' "${config_status}"
    printf 'config_check_json_exit=%s\n' "${config_json_status}"
    printf 'uname=%s\n' "$(uname -a 2>/dev/null || true)"
    printf 'architecture=%s\n' "$(uname -m 2>/dev/null || true)"
    printf 'session_type=%s\n' "${XDG_SESSION_TYPE:-unset}"
    printf 'wayland_display_present=%s\n' "$([[ -n "${WAYLAND_DISPLAY:-}" ]] && echo yes || echo no)"
    printf 'x11_display_present=%s\n' "$([[ -n "${DISPLAY:-}" ]] && echo yes || echo no)"
} >"${BUNDLE_DIR}/system.txt"

{
    printf 'config=%s/jterm1/config.toml\n' "${config_home}"
    printf 'data=%s/jterm1\n' "${data_home}"
    printf 'state=%s/jterm1\n' "${state_home}"
    for path in \
        "${config_home}/jterm1/config.toml" \
        "${config_home}/jterm1/config.toml.bak" \
        "${config_home}/jterm1/config.toml.bak.1" \
        "${config_home}/jterm1/config.toml.before-restore" \
        "${config_home}/jterm1/config.toml.lock" \
        "${state_home}/jterm1/history.jsonl"; do
        if [[ -e "${path}" ]]; then
            stat --printf='%A %a %s bytes %n\n' "${path}" 2>/dev/null || ls -ld -- "${path}"
        else
            printf 'missing %s\n' "${path}"
        fi
    done
} >"${BUNDLE_DIR}/paths-and-metadata.txt"

{
    for name in ANTHROPIC_API_KEY OPENAI_API_KEY JTERM1_AI_PROVIDER JTERM1_AI_MODEL JTERM1_AI_BASE_URL; do
        if [[ -n "${!name:-}" ]]; then
            printf '%s=present\n' "${name}"
        else
            printf '%s=absent\n' "${name}"
        fi
    done
} >"${BUNDLE_DIR}/environment-presence.txt"

if command -v ldd >/dev/null 2>&1; then
    ldd "${binary_path}" >"${BUNDLE_DIR}/linked-libraries.txt" 2>&1 || true
fi
if command -v locale >/dev/null 2>&1; then
    locale >"${BUNDLE_DIR}/locale.txt" 2>&1 || true
fi

cat >"${BUNDLE_DIR}/README.txt" <<'EOF_README'
This support bundle intentionally excludes configuration contents, terminal
history, command output, clipboard data, environment values, API keys, SSH host
details, and session snapshots. It contains diagnostics, configuration schema
issues without values, file metadata, system identity, and the presence/absence
of selected integration variables only.
Review every file before sharing the archive.
EOF_README

ARCHIVE_PATH="${OUTPUT_DIR}/${BUNDLE_NAME}.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner -C "${WORK_DIR}" -cf - "${BUNDLE_NAME}" \
    | gzip -n -9 >"${ARCHIVE_PATH}"
printf 'Created %s\n' "${ARCHIVE_PATH}"
