#!/usr/bin/env bash
# Rebuild or verify the Flatpak offline Cargo source manifest reproducibly.

set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
TARGET="${PROJECT_ROOT}/packaging/flatpak/cargo-sources.json"
REQUIREMENTS="${PROJECT_ROOT}/packaging/flatpak/generator-requirements.txt"
GENERATOR_COMMIT="737c0085912f9f7dabf9341d4608e2a77a51a73a"
GENERATOR_SHA256="b373c8ab1a05378ec5d8ed0645c7b127bcec7d2f7a1798694fbc627d570d856c"

usage() {
    cat <<'EOF'
Usage: scripts/update-flatpak-cargo-sources.sh [--check | --update] [--output PATH]

  --check        Compare pinned generator output with the committed manifest.
                 This is the default.
  --update       Atomically replace the committed manifest when it differs.
  --output PATH  Retain generated output at PATH (valid with --check only).
EOF
}

mode="check"
mode_selected=0
output=""
while (($#)); do
    case "$1" in
        --check | --update)
            if ((mode_selected)); then
                printf 'Error: choose exactly one of --check or --update.\n' >&2
                exit 2
            fi
            mode="${1#--}"
            mode_selected=1
            ;;
        --output)
            shift
            if (($# == 0)); then
                printf 'Error: --output requires a path.\n' >&2
                exit 2
            fi
            output="$1"
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            printf 'Error: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

if [[ "${mode}" == "update" && -n "${output}" ]]; then
    printf 'Error: --output cannot be combined with --update.\n' >&2
    exit 2
fi

for program in curl git python3 realpath sha256sum; do
    if ! command -v "${program}" >/dev/null 2>&1; then
        printf 'Error: %s is required.\n' "${program}" >&2
        exit 1
    fi
done

if [[ -n "${output}" ]] &&
    [[ "$(realpath -m -- "${output}")" == "$(realpath -m -- "${TARGET}")" ]]; then
    printf 'Error: --output must not name the committed manifest.\n' >&2
    exit 2
fi

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/anvil-flatpak-generator.XXXXXX")"
STAGED_UPDATE=""
cleanup() {
    if [[ -n "${STAGED_UPDATE}" ]]; then
        rm -f -- "${STAGED_UPDATE}"
    fi
    rm -rf -- "${WORK_DIR}"
}
trap cleanup EXIT

VENV="${WORK_DIR}/venv"
if python3 -c 'import ensurepip' >/dev/null 2>&1; then
    python3 -m venv "${VENV}"
    "${VENV}/bin/pip" install --disable-pip-version-check \
        --only-binary=:all: --require-hashes --requirement "${REQUIREMENTS}"
elif command -v uv >/dev/null 2>&1; then
    UV_CACHE_DIR="${WORK_DIR}/uv-cache" uv venv --python "$(command -v python3)" "${VENV}"
    UV_CACHE_DIR="${WORK_DIR}/uv-cache" uv pip install \
        --python "${VENV}/bin/python" --only-binary=:all: --require-hashes \
        --requirement "${REQUIREMENTS}"
else
    printf '%s\n' \
        'Error: Python venv support or uv is required for the pinned generator environment.' >&2
    exit 1
fi
GENERATOR="${WORK_DIR}/flatpak-cargo-generator.py"
curl --fail --location --silent --show-error \
    --output "${GENERATOR}" \
    "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/${GENERATOR_COMMIT}/cargo/flatpak-cargo-generator.py"

read -r actual_generator_sha256 _ < <(sha256sum "${GENERATOR}")
if [[ "${actual_generator_sha256}" != "${GENERATOR_SHA256}" ]]; then
    printf 'Error: Flatpak Cargo generator checksum mismatch.\n' >&2
    printf 'Expected: %s\nActual:   %s\n' \
        "${GENERATOR_SHA256}" "${actual_generator_sha256}" >&2
    exit 1
fi

GENERATED="${WORK_DIR}/cargo-sources.json"
XDG_CACHE_HOME="${WORK_DIR}/xdg-cache" "${VENV}/bin/python" "${GENERATOR}" \
    "${PROJECT_ROOT}/Cargo.lock" -o "${GENERATED}"
# The pinned generator omits the customary final newline.
printf '\n' >>"${GENERATED}"

if [[ -n "${output}" ]]; then
    install -m 0644 "${GENERATED}" "${output}"
fi

if [[ "${mode}" == "check" ]]; then
    if ! diff -u "${TARGET}" "${GENERATED}"; then
        printf '%s\n' \
            'Error: Flatpak Cargo sources are stale; run this script with --update.' >&2
        exit 1
    fi
    printf 'Flatpak Cargo sources match Cargo.lock.\n'
    exit 0
fi

if cmp -s "${TARGET}" "${GENERATED}"; then
    printf 'Flatpak Cargo sources are already current.\n'
    exit 0
fi

STAGED_UPDATE="$(mktemp "$(dirname -- "${TARGET}")/.cargo-sources.json.XXXXXX")"
install -m 0644 "${GENERATED}" "${STAGED_UPDATE}"
mv -f -- "${STAGED_UPDATE}" "${TARGET}"
STAGED_UPDATE=""
printf 'Updated %s\n' "${TARGET}"
