#!/usr/bin/env bash
# Assemble a deterministic, user-local Linux release bundle.

set -euo pipefail
umask 022
export LC_ALL=C

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

BINARY="${1:-target/release/anvil}"
DIST_DIR="${DIST_DIR:-${PROJECT_ROOT}/target/dist}"
VERSION="${VERSION:-$(awk -F ' *= *' '$1 == "version" { gsub(/"/, "", $2); print $2; exit }' Cargo.toml)}"
# TARGET may be supplied when packaging a prebuilt binary without Rust installed.
TARGET_EXPLICIT=1
if [[ -z "${TARGET:-}" ]]; then
    TARGET_EXPLICIT=0
    if ! RUSTC_VERBOSE="$(rustc -vV)"; then
        printf 'Error: cannot determine Rust target; set TARGET for a prebuilt binary.\n' >&2
        exit 1
    fi
    TARGET="$(sed -n 's/^host: //p' <<<"${RUSTC_VERBOSE}")"
fi
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}"

if [[ -z "${VERSION}" ]]; then
    echo "Error: could not read the package version from Cargo.toml." >&2
    exit 1
fi
if [[ -z "${TARGET}" ]]; then
    echo "Error: could not determine the Rust host target." >&2
    exit 1
fi
# These fields become archive paths and line-oriented BUILDINFO values. Reject
# separators and control characters before creating any staging/output paths.
# Keep semver build metadata (+) and ordinary Rust target triples supported.
for field in VERSION TARGET; do
    value="${!field}"
    if [[ "${value}" == *[!a-zA-Z0-9._+-]* ]]; then
        echo "Error: ${field} must contain only ASCII letters, digits, dot, underscore, plus or hyphen." >&2
        exit 1
    fi
done
if [[ "${SOURCE_DATE_EPOCH}" == *[!0-9]* ]]; then
    echo "Error: SOURCE_DATE_EPOCH must be a nonnegative integer." >&2
    exit 1
fi
# This records the packaging environment, not the compiler used for BINARY.
# A failed probe is permitted for an explicitly targeted prebuilt binary, but
# successful output must be one nonempty, control-free BUILDINFO value.
# Map NUL to a rejected control byte before Bash can discard it. pipefail
# preserves a failed rustc status through this byte filter.
if ! PACKAGE_RUSTC="$(rustc --version | tr '\000' '\r')"; then
    if [[ "${TARGET_EXPLICIT}" -eq 0 ]]; then
        printf 'Error: cannot read packaging rustc version without explicit TARGET.\n' >&2
        exit 1
    fi
    PACKAGE_RUSTC=unknown
    printf 'Warning: packaging rustc unavailable; BUILDINFO rustc=unknown (not binary provenance).\n' >&2
elif [[ -z "${PACKAGE_RUSTC}" || "${PACKAGE_RUSTC}" == *[[:cntrl:]]* ]]; then
    printf 'Error: rustc --version must return one nonempty line without control characters.\n' >&2
    exit 1
fi

if [[ ! -x "${BINARY}" ]]; then
    echo "Error: release binary not found or not executable: ${BINARY}" >&2
    echo "Run 'make package' outside a Nix shell, or pass an explicit system-linked release binary." >&2
    exit 1
fi

PACKAGE_NAME="anvil-${VERSION}-${TARGET}"
ARCHIVE_NAME="${PACKAGE_NAME}.tar.gz"
STAGE_DIR="$(mktemp -d)"
PACKAGE_ROOT="${STAGE_DIR}/${PACKAGE_NAME}"
PUBLISH_DIR=""
cleanup() {
    rm -rf -- "${STAGE_DIR}"
    if [[ -n "${PUBLISH_DIR}" ]]; then
        rm -rf -- "${PUBLISH_DIR}"
    fi
}
trap cleanup EXIT

install -Dm755 "${BINARY}" "${PACKAGE_ROOT}/bin/anvil"
install -Dm755 scripts/support-bundle.sh "${PACKAGE_ROOT}/bin/anvil-support-bundle"
install -Dm755 packaging/install-release.sh "${PACKAGE_ROOT}/install.sh"
install -Dm0644 scripts/install-config.sh "${PACKAGE_ROOT}/libexec/install-config.sh"
install -Dm0644 scripts/install-desktop.sh "${PACKAGE_ROOT}/libexec/install-desktop.sh"
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
rustc=${PACKAGE_RUSTC}
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
# Build both outputs privately on the destination filesystem. A failed tar,
# compressor or checksum command must leave the last usable release intact.
PUBLISH_DIR="$(mktemp -d -- "${DIST_DIR%/}/.anvil-package.XXXXXX")"

tar \
    --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH}" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "${STAGE_DIR}" \
    -cf - "${PACKAGE_NAME}" \
    | gzip -n -9 > "${PUBLISH_DIR}/${ARCHIVE_NAME}"

(
    cd "${PUBLISH_DIR}"
    sha256sum "${ARCHIVE_NAME}" > "${ARCHIVE_NAME}.sha256"
)

# Each rename publishes a complete file; the pair is not a transaction.
mv -fT -- "${PUBLISH_DIR}/${ARCHIVE_NAME}" "${DIST_DIR}/${ARCHIVE_NAME}"
mv -fT -- "${PUBLISH_DIR}/${ARCHIVE_NAME}.sha256" "${DIST_DIR}/${ARCHIVE_NAME}.sha256"

echo "Created ${DIST_DIR}/${ARCHIVE_NAME}"
echo "Created ${DIST_DIR}/${ARCHIVE_NAME}.sha256"
