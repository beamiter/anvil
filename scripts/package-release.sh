#!/usr/bin/env bash
# Assemble a deterministic, user-local Linux release bundle.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

BINARY="${1:-target/release/jterm1}"
DIST_DIR="${DIST_DIR:-${PROJECT_ROOT}/target/dist}"
VERSION="${VERSION:-$(awk -F ' *= *' '$1 == "version" { gsub(/"/, "", $2); print $2; exit }' Cargo.toml)}"
TARGET="${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}"

if [[ -z "${VERSION}" ]]; then
    echo "Error: could not read the package version from Cargo.toml." >&2
    exit 1
fi
if [[ -z "${TARGET}" ]]; then
    echo "Error: could not determine the Rust host target." >&2
    exit 1
fi
if [[ ! -x "${BINARY}" ]]; then
    echo "Error: release binary not found or not executable: ${BINARY}" >&2
    echo "Run 'cargo build --release --locked' first." >&2
    exit 1
fi

PACKAGE_NAME="jterm1-${VERSION}-${TARGET}"
ARCHIVE_NAME="${PACKAGE_NAME}.tar.gz"
STAGE_DIR="$(mktemp -d)"
PACKAGE_ROOT="${STAGE_DIR}/${PACKAGE_NAME}"
trap 'rm -rf -- "${STAGE_DIR}"' EXIT

install -Dm755 "${BINARY}" "${PACKAGE_ROOT}/bin/jterm1"
install -Dm755 packaging/install-release.sh "${PACKAGE_ROOT}/install.sh"
install -Dm644 packaging/RELEASE_README.md "${PACKAGE_ROOT}/README.txt"
printf '%s\n' "${VERSION}" > "${PACKAGE_ROOT}/VERSION"

install -Dm644 packaging/app.jterm1.desktop \
    "${PACKAGE_ROOT}/share/applications/app.jterm1.desktop"
install -Dm644 README.md "${PACKAGE_ROOT}/share/doc/jterm1/README.md"
install -Dm644 config.toml.example \
    "${PACKAGE_ROOT}/share/doc/jterm1/config.toml.example"

install -d "${PACKAGE_ROOT}/share/jterm1/shell-integration"
install -m644 scripts/shell-integration/jterm1.* \
    "${PACKAGE_ROOT}/share/jterm1/shell-integration/"

install -d "${PACKAGE_ROOT}/share/jterm1/workflows"
install -m644 scripts/workflows/*.yaml \
    "${PACKAGE_ROOT}/share/jterm1/workflows/"

install -Dm644 scripts/notebooks/welcome.jtnb.md \
    "${PACKAGE_ROOT}/share/jterm1/notebooks/welcome.jtnb.md"

mkdir -p "${DIST_DIR}"
rm -f -- "${DIST_DIR}/${ARCHIVE_NAME}" "${DIST_DIR}/${ARCHIVE_NAME}.sha256"

tar \
    --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH}" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "${STAGE_DIR}" \
    -cf - "${PACKAGE_NAME}" \
    | gzip -n -9 > "${DIST_DIR}/${ARCHIVE_NAME}"

(
    cd "${DIST_DIR}"
    sha256sum "${ARCHIVE_NAME}" > "${ARCHIVE_NAME}.sha256"
)

echo "Created ${DIST_DIR}/${ARCHIVE_NAME}"
echo "Created ${DIST_DIR}/${ARCHIVE_NAME}.sha256"
