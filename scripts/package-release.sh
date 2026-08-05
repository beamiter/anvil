#!/usr/bin/env bash
# Assemble a deterministic, user-local Linux release bundle.

set -euo pipefail
umask 022

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

BINARY="${1:-target/release/anvil}"
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

PACKAGE_NAME="anvil-${VERSION}-${TARGET}"
ARCHIVE_NAME="${PACKAGE_NAME}.tar.gz"
STAGE_DIR="$(mktemp -d)"
PACKAGE_ROOT="${STAGE_DIR}/${PACKAGE_NAME}"
trap 'rm -rf -- "${STAGE_DIR}"' EXIT

install -Dm755 "${BINARY}" "${PACKAGE_ROOT}/bin/anvil"
install -Dm755 scripts/support-bundle.sh "${PACKAGE_ROOT}/bin/anvil-support-bundle"
install -Dm755 packaging/install-release.sh "${PACKAGE_ROOT}/install.sh"
install -Dm755 scripts/uninstall.sh "${PACKAGE_ROOT}/uninstall.sh"
install -Dm644 packaging/RELEASE_README.md "${PACKAGE_ROOT}/README.txt"
printf '%s\n' "${VERSION}" > "${PACKAGE_ROOT}/VERSION"

install -Dm644 data/io.github.beamiter.anvil.desktop \
    "${PACKAGE_ROOT}/share/applications/io.github.beamiter.anvil.desktop"
install -Dm644 data/io.github.beamiter.anvil.metainfo.xml \
    "${PACKAGE_ROOT}/share/metainfo/io.github.beamiter.anvil.metainfo.xml"
install -Dm644 data/io.github.beamiter.anvil.svg \
    "${PACKAGE_ROOT}/share/icons/hicolor/scalable/apps/io.github.beamiter.anvil.svg"
for size in 128 256; do
    if [[ -f "data/io.github.beamiter.anvil-${size}.png" ]]; then
        install -Dm644 "data/io.github.beamiter.anvil-${size}.png" \
            "${PACKAGE_ROOT}/share/icons/hicolor/${size}x${size}/apps/io.github.beamiter.anvil.png"
    fi
done
install -Dm644 README.md "${PACKAGE_ROOT}/share/doc/anvil/README.md"
install -Dm644 config.toml.example \
    "${PACKAGE_ROOT}/share/doc/anvil/config.toml.example"
install -Dm644 Cargo.lock "${PACKAGE_ROOT}/share/doc/anvil/Cargo.lock"
cat >"${PACKAGE_ROOT}/share/doc/anvil/BUILDINFO" <<EOF_BUILDINFO
version=${VERSION}
target=${TARGET}
source_date_epoch=${SOURCE_DATE_EPOCH}
git_commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)
rustc=$(rustc --version)
EOF_BUILDINFO

install -d "${PACKAGE_ROOT}/share/anvil/shell-integration"
install -m644 scripts/shell-integration/README.md \
    "${PACKAGE_ROOT}/share/anvil/shell-integration/"
install -m644 scripts/shell-integration/anvil.* \
    "${PACKAGE_ROOT}/share/anvil/shell-integration/"

install -d "${PACKAGE_ROOT}/share/anvil/workflows"
install -m644 scripts/workflows/*.yaml \
    "${PACKAGE_ROOT}/share/anvil/workflows/"

install -Dm644 scripts/notebooks/welcome.jtnb.md \
    "${PACKAGE_ROOT}/share/anvil/notebooks/welcome.jtnb.md"

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
