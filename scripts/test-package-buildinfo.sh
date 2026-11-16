#!/usr/bin/env bash
# Real packaging with non-executed binary/compiler fixtures, not a Rust build.
set -Eeuo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(mktemp -d)"
trap 'rm -rf -- "${ROOT}"' EXIT
APP=anvil
mkdir -p "${ROOT}/bin" "${ROOT}/tmp" "${ROOT}/dist"
printf '#!/bin/sh\nexit 0\n' >"${ROOT}/binary"
chmod +x "${ROOT}/binary"
cat >"${ROOT}/bin/rustc" <<'STUB'
#!/bin/sh
if [ "$1" = -vV ]; then
    [ "${PROBE_MODE}" != missing ] || exit 127
    [ "${PROBE_MODE}" != failed ] || exit 61
    printf 'host: fixture-linux\n'
    exit 0
fi
case "${PROBE_MODE}" in
    good) printf 'rustc packaging-test-stub\n' ;;
    missing) exit 127 ;;
    failed|version-failed) printf 'not-valid-partial-output\n'; exit 61 ;;
    empty) exit 0 ;;
    newline) printf 'rustc stub\ninjected=1\n' ;;
    cr) printf 'rustc stub\r\n' ;;
    tab) printf 'rustc\tstub\n' ;;
    escape) printf 'rustc\033stub\n' ;;
    nul) printf 'rustc\000stub\n' ;;
    *) exit 91 ;;
esac
STUB
chmod +x "${ROOT}/bin/rustc"
export PATH="${ROOT}/bin:${PATH}" TMPDIR="${ROOT}/tmp"
export DIST_DIR="${ROOT}/dist" VERSION=1.0.0 TARGET=fixture-linux SOURCE_DATE_EPOCH=0
export PROBE_MODE=good
package() { bash "${SCRIPT_DIR}/package-release.sh" "${ROOT}/binary"; }
archive="${DIST_DIR}/${APP}-${VERSION}-${TARGET}.tar.gz"
check_info() {
    tar -xOf "${archive}" "${APP}-${VERSION}-${TARGET}/share/doc/${APP}/BUILDINFO" >"${ROOT}/info"
    grep -Fx "rustc=$1" "${ROOT}/info" >/dev/null
    [[ "$(wc -l <"${ROOT}/info")" -eq 5 ]]
    (cd "${DIST_DIR}" && sha256sum --check "${archive##*/}.sha256")
}
package >"${ROOT}/good.log" 2>&1
check_info 'rustc packaging-test-stub'
for mode in missing failed; do
    PROBE_MODE="${mode}" package >"${ROOT}/unknown.log" 2>&1
    check_info unknown
    grep -F 'Warning: packaging rustc unavailable' "${ROOT}/unknown.log" >/dev/null
done
# Auto-detection remains supported with a functioning compiler.
PROBE_MODE=good env -u TARGET bash "${SCRIPT_DIR}/package-release.sh" "${ROOT}/binary" >"${ROOT}/auto.log" 2>&1
check_info 'rustc packaging-test-stub'
cp -- "${archive}" "${ROOT}/old.tar.gz"
cp -- "${archive}.sha256" "${ROOT}/old.sha256"
cat >"${ROOT}/bin/mktemp" <<'STUB'
#!/bin/sh
printf 'staging reached\n' >"${STAGE_MARKER}"
exit 79
STUB
chmod +x "${ROOT}/bin/mktemp"
export STAGE_MARKER="${ROOT}/stage-reached"
for mode in empty newline cr tab escape nul; do
    for target_mode in explicit inferred; do
        rm -f -- "${STAGE_MARKER}"
        args=(env)
        [[ "${target_mode}" != inferred ]] || args+=(-u TARGET)
        if PROBE_MODE="${mode}" "${args[@]}" bash "${SCRIPT_DIR}/package-release.sh" "${ROOT}/binary" >"${ROOT}/bad.log" 2>&1; then
            printf 'FAIL: accepted %s with %s target\n' "${mode}" "${target_mode}" >&2; exit 1
        fi
        grep -F 'Error: rustc --version must return' "${ROOT}/bad.log" >/dev/null
        [[ ! -e "${STAGE_MARKER}" ]]
        cmp -- "${archive}" "${ROOT}/old.tar.gz"
        cmp -- "${archive}.sha256" "${ROOT}/old.sha256"
    done
done
for mode in missing failed version-failed; do
    if PROBE_MODE="${mode}" env -u TARGET bash "${SCRIPT_DIR}/package-release.sh" "${ROOT}/binary" >"${ROOT}/bad.log" 2>&1; then
        printf 'FAIL: inferred target accepted failed compiler\n' >&2; exit 1
    fi
    grep -F 'Error: cannot' "${ROOT}/bad.log" >/dev/null
    [[ ! -e "${STAGE_MARKER}" ]]
    cmp -- "${archive}" "${ROOT}/old.tar.gz"
    cmp -- "${archive}.sha256" "${ROOT}/old.sha256"
done
printf 'PASS: compiler metadata validated before staging; unknown explicit, strict inferred, old outputs preserved\n'
