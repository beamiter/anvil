#!/usr/bin/env bash
# Exercise build-free packaging, runtime/staging path separation, safe desktop
# rewriting, and install/uninstall symmetry in a private temporary tree.

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
INSTALLER="${SCRIPT_DIR}/install.sh"
UNINSTALLER="${SCRIPT_DIR}/uninstall.sh"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/anvil-install-paths.XXXXXX")"
TEST_HOME="${TEST_ROOT}/home"
TEST_PATH="/usr/bin:/bin"

trap 'rm -rf -- "${TEST_ROOT}"' EXIT
mkdir -p "${TEST_HOME}"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

assert_contains() {
    local label="$1" output="$2" expected="$3"
    [[ "${output}" == *"${expected}"* ]] \
        || fail "${label} did not contain ${expected@Q}"
}

assert_not_contains() {
    local label="$1" output="$2" unexpected="$3"
    [[ "${output}" != *"${unexpected}"* ]] \
        || fail "${label} unexpectedly contained ${unexpected@Q}"
}

assert_regular_file() {
    local label="$1" path="$2"
    [[ -f "${path}" ]] || fail "${label} is not a regular file: ${path}"
}

assert_mode() {
    local label="$1" path="$2" expected="$3" actual
    actual="$(stat -c '%a' -- "${path}")"
    [[ "${actual}" == "${expected}" ]] \
        || fail "${label} mode was ${actual}, expected ${expected}: ${path}"
}

install_dry_run() {
    local destdir="$1"
    shift
    env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${destdir}" \
        CARGO_TARGET_DIR= XDG_DATA_HOME= "${INSTALLER}" --dry-run "$@"
}

uninstall_dry_run() {
    local destdir="$1"
    shift
    env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${destdir}" \
        XDG_DATA_HOME= "${UNINSTALLER}" --dry-run "$@"
}

default_prefix="${TEST_HOME}/.local"
mkdir -p "${default_prefix}/bin"
touch "${default_prefix}/bin/anvil"
assert_contains "default install" "$(install_dry_run "")" \
    "Installed anvil to ${default_prefix}/bin/anvil"
assert_contains "default uninstall" "$(uninstall_dry_run "")" \
    "${default_prefix}/bin/anvil"

custom_prefix="${TEST_ROOT}/prefix"
custom_bin="${TEST_ROOT}/bin"
custom_data="${TEST_ROOT}/data"
mkdir -p "${custom_bin}"
touch "${custom_bin}/anvil"
custom_install="$(install_dry_run "" --prefix "${custom_prefix}" \
    --bin-dir "${custom_bin}" --data-dir "${custom_data}")"
custom_uninstall="$(uninstall_dry_run "" --prefix "${custom_prefix}" \
    --bin-dir "${custom_bin}" --data-dir "${custom_data}")"
assert_contains "custom binary install" "${custom_install}" \
    "Installed anvil to ${custom_bin}/anvil"
assert_contains "custom data install" "${custom_install}" \
    "Installed runtime assets under ${custom_data}/anvil"
assert_contains "custom binary uninstall" "${custom_uninstall}" "${custom_bin}/anvil"

root_stage="$(install_dry_run / --prefix /opt/anvil-root)"
assert_contains "root DESTDIR cache policy" "${root_stage}" \
    "Staged install (DESTDIR set); skipping desktop cache refresh."
assert_contains "root DESTDIR summary" "${root_stage}" \
    "Staged file: /opt/anvil-root/bin/anvil"

for bad_path in '/opt/anvil/../escape' '/opt/anvil/'$'bad\npath'; do
    if install_dry_run "${TEST_ROOT}/stage" --prefix "${bad_path}" >/dev/null 2>&1; then
        fail "installer accepted unsafe prefix ${bad_path@Q}"
    fi
    if uninstall_dry_run "${TEST_ROOT}/stage" --prefix "${bad_path}" >/dev/null 2>&1; then
        fail "uninstaller accepted unsafe prefix ${bad_path@Q}"
    fi
done
if install_dry_run "${TEST_ROOT}/stage/../escape" --prefix /opt/anvil \
    >"${TEST_ROOT}/bad-destdir.log" 2>&1; then
    fail "installer accepted a DESTDIR parent component"
fi
assert_contains "DESTDIR parent diagnostic" "$(<"${TEST_ROOT}/bad-destdir.log")" \
    "DESTDIR must not contain '..' path components"

for option in --bin-dir= --data-dir=; do
    if install_dry_run "" "${option}" >"${TEST_ROOT}/empty-option.log" 2>&1; then
        fail "installer accepted ${option}"
    fi
    if uninstall_dry_run "" "${option}" >"${TEST_ROOT}/empty-option.log" 2>&1; then
        fail "uninstaller accepted ${option}"
    fi
done

unicode_prefix='/opt/./铁砧 terminal'
unicode_data='/opt/./共享 data'
unicode_output="$(install_dry_run "${TEST_ROOT}/stage" \
    --prefix "${unicode_prefix}" --data-dir "${unicode_data}")"
assert_contains "Unicode binary path" "${unicode_output}" \
    "Installed anvil to ${unicode_prefix}/bin/anvil"
assert_contains "Unicode data path" "${unicode_output}" \
    "Installed runtime assets under ${unicode_data}/anvil"

prebuilt_dir="${TEST_ROOT}/prebuilt"
prebuilt_binary="${prebuilt_dir}/anvil"
stage="${TEST_ROOT}/roundtrip-stage"
runtime_prefix='/opt/anvil release \dir $'
runtime_bin="${runtime_prefix}/bin"
runtime_data='/opt/anvil data \dir $'
config_home="/etc/anvil contract"
app_id="io.github.beamiter.anvil"
mkdir -p "${prebuilt_dir}"
printf '#!/bin/sh\nprintf "anvil fixture\\n"\n' >"${prebuilt_binary}"
chmod 0600 "${prebuilt_binary}"

install_output="$(
    env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${stage}" \
        XDG_CONFIG_HOME="${config_home}" "${INSTALLER}" \
        --binary "${prebuilt_binary}" --prefix "${runtime_prefix}" \
        --data-dir "${runtime_data}" 2>&1
)"
assert_contains "prebuilt selected" "${install_output}" \
    "Using prebuilt anvil binary: ${prebuilt_binary}"
assert_not_contains "prebuilt skips build" "${install_output}" "Building anvil"

installed_binary="${stage}${runtime_bin}/anvil"
installed_support="${stage}${runtime_bin}/anvil-support-bundle"
installed_desktop="${stage}${runtime_data}/applications/${app_id}.desktop"
installed_metainfo="${stage}${runtime_data}/metainfo/${app_id}.metainfo.xml"
installed_asset="${stage}${runtime_data}/anvil/notebooks/welcome.jtnb.md"
installed_config="${stage}${config_home}/anvil/config.toml"
for file in "${installed_binary}" "${installed_support}" "${installed_desktop}" \
    "${installed_asset}" "${installed_config}"; do
    assert_regular_file "staged output" "${file}"
done
cmp -- "${prebuilt_binary}" "${installed_binary}" \
    || fail "installed binary differs from fixture"
assert_mode "binary" "${installed_binary}" 755
assert_mode "desktop" "${installed_desktop}" 644
assert_mode "config" "${installed_config}" 600

expected_exec='Exec="/opt/anvil release \\\\dir \\$/bin/anvil"'
[[ "$(grep -Fxc "${expected_exec}" "${installed_desktop}")" == 2 ]] \
    || fail "desktop Exec paths were not safely rewritten"
expected_try_exec='TryExec=/opt/anvil release \\dir $/bin/anvil'
grep -Fxq "${expected_try_exec}" "${installed_desktop}" \
    || fail "desktop TryExec path was not safely rewritten"
if grep -Fq "${stage}" "${installed_desktop}"; then
    fail "desktop entry leaked DESTDIR into its runtime path"
fi
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "${installed_desktop}"
fi

for workflow in docker-tail-logs find-large-files git-feature \
    git-rebase-interactive kill-port ssh-tunnel; do
    assert_regular_file "frozen workflow asset" \
        "${stage}${runtime_data}/anvil/workflows/${workflow}.yaml"
done

# Public resources are committed with rename, so a hostile destination link is
# replaced rather than followed and its target remains untouched.
asset_victim="${TEST_ROOT}/asset-must-not-change"
printf 'asset victim\n' >"${asset_victim}"
rm -f -- "${installed_metainfo}"
ln -s -- "${asset_victim}" "${installed_metainfo}"
env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${stage}" \
    XDG_CONFIG_HOME="${config_home}" "${INSTALLER}" \
    --binary "${prebuilt_binary}" --prefix "${runtime_prefix}" \
    --data-dir "${runtime_data}" >/dev/null
[[ ! -L "${installed_metainfo}" ]] \
    || fail "metainfo destination symlink survived reinstall"
[[ "$(<"${asset_victim}")" == 'asset victim' ]] \
    || fail "public asset install followed destination symlink"

victim="${TEST_ROOT}/must-not-change"
printf 'victim\n' >"${victim}"
rm -f -- "${installed_binary}" "${installed_config}"
ln -s -- "${victim}" "${installed_binary}"
ln -s -- missing-config "${installed_config}"
env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${stage}" \
    XDG_CONFIG_HOME="${config_home}" "${INSTALLER}" \
    --binary "${prebuilt_binary}" --prefix "${runtime_prefix}" \
    --data-dir "${runtime_data}" --no-desktop >/dev/null
[[ ! -L "${installed_binary}" ]] || fail "binary destination symlink survived reinstall"
[[ "$(<"${victim}")" == victim ]] || fail "binary install followed destination symlink"
[[ -L "${installed_config}" ]] || fail "installer replaced a dangling config symlink"

shopt -s nullglob
binary_temps=("${installed_binary}.install."*)
desktop_temps=("${stage}${runtime_data}/applications/.${app_id}.desktop.install."*)
config_temps=("${stage}${config_home}/anvil/.config.toml.install."*)
shopt -u nullglob
(( ${#binary_temps[@]} == 0 )) || fail "binary temporary files remain"
(( ${#desktop_temps[@]} == 0 )) || fail "desktop temporary files remain"
(( ${#config_temps[@]} == 0 )) || fail "config temporary files remain"

env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${stage}" \
    XDG_CONFIG_HOME="${config_home}" "${UNINSTALLER}" \
    --prefix "${runtime_prefix}" --data-dir "${runtime_data}" >/dev/null
[[ ! -e "${installed_binary}" && ! -L "${installed_binary}" ]] \
    || fail "uninstaller left binary"
[[ ! -e "${installed_desktop}" && ! -L "${installed_desktop}" ]] \
    || fail "uninstaller left desktop entry"
[[ -L "${installed_config}" ]] || fail "uninstaller removed preserved configuration"

interrupt_tools="${TEST_ROOT}/interrupt-tools"
interrupt_stage="${TEST_ROOT}/interrupt-stage"
interrupt_prefix="/opt/anvil-interrupt"
interrupt_binary="${interrupt_stage}${interrupt_prefix}/bin/anvil"
mkdir -p "${interrupt_tools}" "$(dirname -- "${interrupt_binary}")"
printf 'old interrupt anvil\n' >"${interrupt_binary}"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'last=""' \
    'for argument do last="${argument}"; done' \
    '/usr/bin/install "$@"' \
    'case "${last}" in *.install.*) kill -TERM "${PPID}" ;; esac' \
    >"${interrupt_tools}/install"
chmod 0755 "${interrupt_tools}/install"
if {
    env HOME="${TEST_HOME}" PATH="${interrupt_tools}:${TEST_PATH}" \
        DESTDIR="${interrupt_stage}" "${INSTALLER}" --binary "${prebuilt_binary}" \
        --prefix "${interrupt_prefix}" --no-config --no-desktop
} >"${TEST_ROOT}/interrupt.log" 2>&1; then
    fail "interrupted installer unexpectedly succeeded"
fi
[[ "$(<"${interrupt_binary}")" == 'old interrupt anvil' ]] \
    || fail "pre-rename interruption replaced the old binary"
shopt -s nullglob
interrupt_temps=("${interrupt_binary}.install."*)
shopt -u nullglob
(( ${#interrupt_temps[@]} == 0 )) \
    || fail "pre-rename interruption left a binary temporary"

prebuilt_symlink="${prebuilt_dir}/anvil-link"
ln -s -- "${prebuilt_binary}" "${prebuilt_symlink}"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${stage}" \
    "${INSTALLER}" --binary "${prebuilt_symlink}" --prefix /opt/anvil \
    --no-config --no-desktop >"${TEST_ROOT}/symlink-source.log" 2>&1; then
    fail "installer accepted a symlinked prebuilt binary"
fi
assert_contains "symlink source diagnostic" "$(<"${TEST_ROOT}/symlink-source.log")" \
    "prebuilt binary must not be a symbolic link"

if install_dry_run "" --binary= >"${TEST_ROOT}/empty-binary.log" 2>&1; then
    fail "installer accepted an empty --binary"
fi
assert_contains "empty binary diagnostic" "$(<"${TEST_ROOT}/empty-binary.log")" \
    "--binary must not be empty"

empty_prebuilt="${prebuilt_dir}/anvil-empty"
: >"${empty_prebuilt}"
empty_stage="${TEST_ROOT}/empty-prebuilt-stage"
empty_prefix="/opt/anvil-empty"
empty_sentinel="${empty_stage}${empty_prefix}/bin/anvil"
mkdir -p "$(dirname -- "${empty_sentinel}")"
printf 'old empty anvil\n' >"${empty_sentinel}"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${empty_stage}" \
    "${INSTALLER}" --binary "${empty_prebuilt}" --prefix "${empty_prefix}" \
    --no-config --no-desktop >"${TEST_ROOT}/empty-prebuilt.log" 2>&1; then
    fail "installer accepted a zero-byte prebuilt binary"
fi
assert_contains "zero-byte prebuilt diagnostic" \
    "$(<"${TEST_ROOT}/empty-prebuilt.log")" "prebuilt binary must not be empty"
[[ "$(<"${empty_sentinel}")" == 'old empty anvil' ]] \
    || fail "zero-byte preflight replaced the old binary"

if install_dry_run "" --binary "${prebuilt_binary}" --backend cargo \
    >"${TEST_ROOT}/backend-binary.log" 2>&1; then
    fail "installer accepted both --backend and --binary"
fi
assert_contains "backend/binary diagnostic" "$(<"${TEST_ROOT}/backend-binary.log")" \
    "--backend cannot be combined with --binary"

# Packaging roots are caller controlled: reject an existing symlink ancestor
# beneath the shared-data path before any staged write can escape DESTDIR.
ancestor_stage="${TEST_ROOT}/ancestor-stage"
ancestor_victim="${TEST_ROOT}/ancestor-victim"
mkdir -p "${ancestor_stage}" "${ancestor_victim}"
ln -s -- "${ancestor_victim}" "${ancestor_stage}/srv"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${ancestor_stage}" \
    "${INSTALLER}" --binary "${prebuilt_binary}" --prefix /opt/anvil \
    --data-dir /srv/anvil --no-config --no-desktop \
    >"${TEST_ROOT}/ancestor.log" 2>&1; then
    fail "installer accepted a symlink ancestor beneath DESTDIR"
fi
assert_contains "staged symlink diagnostic" "$(<"${TEST_ROOT}/ancestor.log")" \
    "staged install path contains a symbolic-link ancestor"
[[ -z "$(find "${ancestor_victim}" -mindepth 1 -print -quit)" ]] \
    || fail "staged install escaped through a symlink ancestor"

# Force another writer to win immediately before link(2). The first-run config
# must preserve that file and leave no private staging name behind.
race_tools="${TEST_ROOT}/config-race-tools"
race_stage="${TEST_ROOT}/config-race-stage"
race_prefix="/opt/anvil-config-race"
race_data="/opt/anvil-config-race-data"
race_config_home="/etc/anvil-config-race"
race_config="${race_stage}${race_config_home}/anvil/config.toml"
mkdir -p "${race_tools}"
# shellcheck disable=SC2016
printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'destination=""' \
    'for argument do destination="${argument}"; done' \
    'printf "concurrent config\\n" >"${destination}"' \
    'exec /usr/bin/ln "$@"' \
    >"${race_tools}/ln"
chmod 0755 "${race_tools}/ln"
env HOME="${TEST_HOME}" PATH="${race_tools}:${TEST_PATH}" DESTDIR="${race_stage}" \
    XDG_CONFIG_HOME="${race_config_home}" "${INSTALLER}" \
    --binary "${prebuilt_binary}" --prefix "${race_prefix}" \
    --data-dir "${race_data}" --no-desktop >"${TEST_ROOT}/config-race.log" 2>&1
[[ "$(<"${race_config}")" == 'concurrent config' ]] \
    || fail "initial config publication overwrote a concurrent writer"
assert_contains "config race diagnostic" "$(<"${TEST_ROOT}/config-race.log")" \
    "Keeping concurrently created config"
shopt -s nullglob
race_config_temps=("${race_config%/*}/.config.toml.install."*)
shopt -u nullglob
(( ${#race_config_temps[@]} == 0 )) \
    || fail "config race left a temporary file"

invalid_stage="${TEST_ROOT}/invalid-desktop-stage"
invalid_prefix='/opt/anvil=invalid'
sentinel_binary="${invalid_stage}${invalid_prefix}/bin/anvil"
mkdir -p "$(dirname -- "${sentinel_binary}")"
printf 'old anvil\n' >"${sentinel_binary}"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${invalid_stage}" \
    "${INSTALLER}" --binary "${prebuilt_binary}" --prefix "${invalid_prefix}" \
    --no-config >"${TEST_ROOT}/desktop-preflight.log" 2>&1; then
    fail "installer accepted an invalid desktop executable path"
fi
[[ "$(<"${sentinel_binary}")" == 'old anvil' ]] \
    || fail "desktop preflight failure replaced the old binary"

invalid_xdg_prefix="/opt/anvil-invalid-xdg"
xdg_sentinel="${stage}${invalid_xdg_prefix}/bin/anvil"
mkdir -p "$(dirname -- "${xdg_sentinel}")"
printf 'old xdg anvil\n' >"${xdg_sentinel}"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${stage}" \
    XDG_CONFIG_HOME='/etc/anvil/../escape' "${INSTALLER}" \
    --binary "${prebuilt_binary}" --prefix "${invalid_xdg_prefix}" --no-desktop \
    >"${TEST_ROOT}/xdg-preflight.log" 2>&1; then
    fail "installer accepted an escaping XDG_CONFIG_HOME"
fi
[[ "$(<"${xdg_sentinel}")" == 'old xdg anvil' ]] \
    || fail "XDG preflight failure replaced the old binary"

# Purge validates every recursive-removal root before removing the binary.
purge_stage="${TEST_ROOT}/purge-stage"
purge_prefix="/opt/anvil-purge"
mkdir -p "${purge_stage}${purge_prefix}/bin"
touch "${purge_stage}${purge_prefix}/bin/anvil"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${purge_stage}" \
    XDG_STATE_HOME='/var/lib/anvil/../escape' "${UNINSTALLER}" \
    --prefix "${purge_prefix}" --purge-config >/dev/null 2>&1; then
    fail "purge accepted an escaping XDG_STATE_HOME"
fi
assert_regular_file "binary after rejected purge" \
    "${purge_stage}${purge_prefix}/bin/anvil"

uninstall_link_stage="${TEST_ROOT}/uninstall-link-stage"
uninstall_link_victim="${TEST_ROOT}/uninstall-link-victim"
uninstall_link_prefix="/opt/anvil-uninstall-link"
mkdir -p "${uninstall_link_stage}" \
    "${uninstall_link_victim}/anvil-uninstall-link/bin"
printf 'outside anvil\n' \
    >"${uninstall_link_victim}/anvil-uninstall-link/bin/anvil"
ln -s -- "${uninstall_link_victim}" "${uninstall_link_stage}/opt"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" \
    DESTDIR="${uninstall_link_stage}" XDG_DATA_HOME= "${UNINSTALLER}" \
    --prefix "${uninstall_link_prefix}" >"${TEST_ROOT}/uninstall-link.log" 2>&1; then
    fail "uninstaller followed a symbolic-link ancestor below DESTDIR"
fi
assert_contains "uninstall ancestor diagnostic" \
    "$(<"${TEST_ROOT}/uninstall-link.log")" \
    "staged uninstall path contains a symbolic-link ancestor"
[[ "$(<"${uninstall_link_victim}/anvil-uninstall-link/bin/anvil")" == \
    'outside anvil' ]] || fail "uninstaller removed a file outside DESTDIR"

purge_link_stage="${TEST_ROOT}/purge-link-stage"
purge_link_victim="${TEST_ROOT}/purge-link-victim"
purge_link_prefix="/opt/anvil-purge-link"
mkdir -p "${purge_link_stage}${purge_link_prefix}/bin" \
    "${purge_link_stage}/var" "${purge_link_victim}/state/anvil"
printf 'installed anvil\n' >"${purge_link_stage}${purge_link_prefix}/bin/anvil"
printf 'outside state\n' >"${purge_link_victim}/state/anvil/sentinel"
ln -s -- "${purge_link_victim}/state" "${purge_link_stage}/var/state"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${purge_link_stage}" \
    XDG_DATA_HOME= XDG_STATE_HOME=/var/state "${UNINSTALLER}" \
    --prefix "${purge_link_prefix}" --purge-config \
    >"${TEST_ROOT}/purge-link.log" 2>&1; then
    fail "purge followed a symbolic-link ancestor below DESTDIR"
fi
assert_regular_file "binary after rejected symlink purge" \
    "${purge_link_stage}${purge_link_prefix}/bin/anvil"
[[ "$(<"${purge_link_victim}/state/anvil/sentinel")" == 'outside state' ]] \
    || fail "purge removed state outside DESTDIR"

# Normalize disguised root spellings before checking every existing DESTDIR
# component. `link/.` and `link//` must never turn a staging root symlink into
# an outside install/uninstall target.
root_link="${TEST_ROOT}/destdir-root-link"
root_victim="${TEST_ROOT}/destdir-root-victim"
root_prefix="/opt/anvil-destdir-root"
root_binary="${root_victim}${root_prefix}/bin/anvil"
mkdir -p "${root_victim}"
ln -s -- "${root_victim}" "${root_link}"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${root_link}/." \
    "${INSTALLER}" --binary "${prebuilt_binary}" --prefix "${root_prefix}" \
    --data-dir /srv/anvil-root --no-config --no-desktop \
    >"${TEST_ROOT}/root-link-install.log" 2>&1; then
    fail "installer accepted a symlinked DESTDIR root disguised with /."
fi
assert_contains "symlinked DESTDIR install diagnostic" \
    "$(<"${TEST_ROOT}/root-link-install.log")" \
    "DESTDIR path contains a symbolic-link component"
[[ -z "$(find "${root_victim}" -mindepth 1 -print -quit)" ]] \
    || fail "symlinked DESTDIR install wrote outside its staging boundary"

mkdir -p "$(dirname -- "${root_binary}")"
printf 'outside root anvil\n' >"${root_binary}"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${root_link}//" \
    "${UNINSTALLER}" --prefix "${root_prefix}" \
    >"${TEST_ROOT}/root-link-uninstall.log" 2>&1; then
    fail "uninstaller accepted a symlinked DESTDIR root with trailing separators"
fi
assert_contains "symlinked DESTDIR uninstall diagnostic" \
    "$(<"${TEST_ROOT}/root-link-uninstall.log")" \
    "DESTDIR path contains a symbolic-link component"
[[ "$(<"${root_binary}")" == 'outside root anvil' ]] \
    || fail "symlinked DESTDIR uninstall removed an outside binary"

root_state="${root_victim}/var/lib/anvil-root/anvil/sentinel"
root_config="${root_victim}/etc/anvil-root/anvil/sentinel"
mkdir -p "$(dirname -- "${root_state}")" "$(dirname -- "${root_config}")"
printf 'outside root state\n' >"${root_state}"
printf 'outside root config\n' >"${root_config}"
if env HOME="${TEST_HOME}" PATH="${TEST_PATH}" DESTDIR="${root_link}/./" \
    XDG_STATE_HOME=/var/lib/anvil-root XDG_CONFIG_HOME=/etc/anvil-root \
    "${UNINSTALLER}" --prefix "${root_prefix}" --purge-config \
    >"${TEST_ROOT}/root-link-purge.log" 2>&1; then
    fail "purge accepted a symlinked DESTDIR root"
fi
assert_contains "symlinked DESTDIR purge diagnostic" \
    "$(<"${TEST_ROOT}/root-link-purge.log")" \
    "DESTDIR path contains a symbolic-link component"
assert_regular_file "binary after rejected root-symlink purge" "${root_binary}"
[[ "$(<"${root_state}")" == 'outside root state' ]] \
    || fail "root-symlink purge removed outside state"
[[ "$(<"${root_config}")" == 'outside root config' ]] \
    || fail "root-symlink purge removed outside config"

printf 'install/uninstall path contract: ok\n'
