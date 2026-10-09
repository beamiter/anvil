#!/usr/bin/env bash
# Exercise the prebuilt release installer only in isolated HOME/XDG trees.
# The archive contains a tiny fixture executable; no Cargo build or system
# installation is performed.

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/anvil-release-install.XXXXXX")"
trap 'rm -rf -- "${TEST_ROOT}"' EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

fixture="${TEST_ROOT}/anvil"
printf '#!/bin/sh\nprintf "anvil release fixture\\n"\n' >"${fixture}"
chmod 0755 "${fixture}"
env DIST_DIR="${TEST_ROOT}/dist" VERSION=0.0.0-test TARGET=fixture-linux \
    SOURCE_DATE_EPOCH=1700000000 \
    bash "${SCRIPT_DIR}/package-release.sh" "${fixture}" >/dev/null
archive="${TEST_ROOT}/dist/anvil-0.0.0-test-fixture-linux.tar.gz"
(cd "${TEST_ROOT}/dist" && sha256sum --check "${archive##*/}.sha256") >/dev/null
tar -xzf "${archive}" -C "${TEST_ROOT}"
bundle="${TEST_ROOT}/anvil-0.0.0-test-fixture-linux"

run_install() {
    local root="$1"
    shift
    env HOME="${root}/home" XDG_CONFIG_HOME="${root}/config" \
        XDG_DATA_HOME="${root}/data" PATH=/usr/bin:/bin USER=anvil-release-test \
        "$@" bash "${bundle}/install.sh"
}

assert_no_staging_files() {
    local root="$1"
    [[ -z "$(find "${root}" -name '.*.install.*' -print -quit)" ]] \
        || fail "owned staging files were left under ${root}"
}

# A preexisting predictable staging link is unrelated user data. Do not
# dereference, remove, chmod, or rename it into the final desktop entry.
root="${TEST_ROOT}/predictable-link"
applications="${root}/data/applications"
mkdir -p "${applications}" "${root}/config/anvil"
victim="${root}/victim"
printf 'untouched desktop victim\n' >"${victim}"
chmod 0600 "${victim}"
old_stage="${applications}/io.github.beamiter.anvil.desktop.new"
ln -s -- "${victim}" "${old_stage}"
config="${root}/config/anvil/config.toml"
ln -s -- missing-user-config "${config}"
run_install "${root}" >"${root}/install.log" 2>&1
[[ "$(<"${victim}")" == 'untouched desktop victim' ]] \
    || fail "release installer wrote through the old predictable staging link"
[[ "$(stat -c '%a' -- "${victim}")" == 600 ]] \
    || fail "release installer changed the staging link target's mode"
[[ -L "${old_stage}" && "$(readlink -- "${old_stage}")" == "${victim}" ]] \
    || fail "release installer changed the old predictable staging link"
desktop="${applications}/io.github.beamiter.anvil.desktop"
[[ -f "${desktop}" && ! -L "${desktop}" ]] \
    || fail "published desktop entry is not a regular file"
[[ "$(stat -c '%a' -- "${desktop}")" == 644 ]] \
    || fail "published desktop mode is not 0644"
[[ -L "${config}" && "$(readlink -- "${config}")" == missing-user-config ]] \
    || fail "release installer replaced a dangling configuration link"
[[ ! -e "${root}/config/anvil/missing-user-config" ]] \
    || fail "release installer wrote to the dangling config link target"
assert_no_staging_files "${root}"

# First-run publication creates a private file, while later installs preserve
# the user's exact bytes. Directory symlinks are existing choices too.
root="${TEST_ROOT}/fresh"
run_install "${root}" >"${TEST_ROOT}/fresh.log" 2>&1
config="${root}/config/anvil/config.toml"
[[ -f "${config}" && ! -L "${config}" ]] || fail "initial config is not a regular file"
[[ "$(stat -c '%a' -- "${config}")" == 600 ]] || fail "initial config mode is not 0600"
printf 'keep user configuration\n' >"${config}"
run_install "${root}" >>"${TEST_ROOT}/fresh.log" 2>&1
[[ "$(<"${config}")" == 'keep user configuration' ]] || fail "reinstall replaced user config"
assert_no_staging_files "${root}"

# A valid configuration symlink is also preserved byte-for-byte.
root="${TEST_ROOT}/existing-config-link"
mkdir -p "${root}/config/anvil"
victim="${root}/user-config"
printf 'existing linked user configuration\n' >"${victim}"
config="${root}/config/anvil/config.toml"
ln -s -- "${victim}" "${config}"
run_install "${root}" >"${root}/install.log" 2>&1
[[ -L "${config}" && "$(readlink -- "${config}")" == "${victim}" ]] \
    || fail "release installer replaced a valid configuration link"
[[ "$(<"${victim}")" == 'existing linked user configuration' ]] \
    || fail "release installer changed a configuration link target"
assert_no_staging_files "${root}"

# Failed copying or publication must leave no owned staging files and must
# not create a partial config or replace the previous desktop entry.
failure_tools="${TEST_ROOT}/failure-tools"
mkdir -p "${failure_tools}"
printf '#!/bin/sh\n/usr/bin/cat "$@"\nexit 73\n' >"${failure_tools}/cat"
chmod 0755 "${failure_tools}/cat"
root="${TEST_ROOT}/failed-copy"
if run_install "${root}" PATH="${failure_tools}:/usr/bin:/bin" \
    >"${TEST_ROOT}/failed-copy.log" 2>&1; then
    fail "release installer accepted a failed configuration copy"
fi
[[ ! -e "${root}/config/anvil/config.toml" ]] \
    || fail "failed config copy published a partial config"
assert_no_staging_files "${root}"
rm -- "${failure_tools}/cat"
printf '#!/bin/sh\nexit 74\n' >"${failure_tools}/mv"
chmod 0755 "${failure_tools}/mv"
root="${TEST_ROOT}/failed-publish"
mkdir -p "${root}/data/applications"
desktop="${root}/data/applications/io.github.beamiter.anvil.desktop"
printf 'old failed-publish desktop\n' >"${desktop}"
if run_install "${root}" PATH="${failure_tools}:/usr/bin:/bin" \
    >"${root}/install.log" 2>&1; then
    fail "release installer accepted a failed desktop publication"
fi
[[ "$(<"${desktop}")" == 'old failed-publish desktop' ]] \
    || fail "failed publication replaced the old desktop"
assert_no_staging_files "${root}"

# Force an ordinary file or a directory symlink to win immediately before
# link(2). ln -T must preserve the winner instead of copying into its referent.
race_tools="${TEST_ROOT}/race-tools"
mkdir -p "${race_tools}"
cat >"${race_tools}/ln" <<'HOOK'
#!/bin/sh
set -eu
destination=''
for argument do destination="${argument}"; done
if [ "${ANVIL_RACE_KIND}" = directory ]; then
    /usr/bin/ln -s -- "${ANVIL_RACE_VICTIM}" "${destination}"
else
    printf 'concurrent user configuration\n' >"${destination}"
fi
exec /usr/bin/ln "$@"
HOOK
chmod 0755 "${race_tools}/ln"
for kind in file directory; do
    root="${TEST_ROOT}/race-${kind}"
    victim="${root}/outside-config"
    mkdir -p "${victim}"
    run_install "${root}" PATH="${race_tools}:/usr/bin:/bin" \
        ANVIL_RACE_KIND="${kind}" ANVIL_RACE_VICTIM="${victim}" \
        >"${root}/install.log" 2>&1
    config="${root}/config/anvil/config.toml"
    if [[ "${kind}" == file ]]; then
        [[ "$(<"${config}")" == 'concurrent user configuration' ]] \
            || fail "release installer replaced a concurrent config writer"
    else
        [[ -L "${config}" && "$(readlink -- "${config}")" == "${victim}" ]] \
            || fail "release installer replaced a concurrent directory link"
        [[ -z "$(find "${victim}" -mindepth 1 -print -quit)" ]] \
            || fail "release installer published inside a config directory link"
    fi
    grep -Fq 'Keeping concurrently created configuration:' "${root}/install.log" \
        || fail "concurrent config preservation was not reported"
    assert_no_staging_files "${root}"
done

# Replace the reserved desktop name before awk writes. The bound descriptor
# protects the victim; publication fails and cleanup preserves the unknown link.
attack_tools="${TEST_ROOT}/attack-tools"
mkdir -p "${attack_tools}"
cat >"${attack_tools}/awk" <<'HOOK'
#!/bin/sh
set -eu
stage=$(/usr/bin/find "${ANVIL_ATTACK_APPLICATIONS}" -maxdepth 1 \
    -name '.io.github.beamiter.anvil.desktop.install.*' -print -quit)
[ -n "${stage}" ]
/usr/bin/rm -f -- "${stage}"
/usr/bin/ln -s -- "${ANVIL_ATTACK_VICTIM}" "${stage}"
printf '%s\n' "${stage}" >"${ANVIL_ATTACK_RECORD}"
exec /usr/bin/awk "$@"
HOOK
chmod 0755 "${attack_tools}/awk"
root="${TEST_ROOT}/replaced-stage"
applications="${root}/data/applications"
mkdir -p "${applications}"
victim="${root}/victim"
printf 'untouched replaced-stage victim\n' >"${victim}"
desktop="${applications}/io.github.beamiter.anvil.desktop"
printf 'old desktop bytes\n' >"${desktop}"
if run_install "${root}" PATH="${attack_tools}:/usr/bin:/bin" \
    ANVIL_ATTACK_APPLICATIONS="${applications}" ANVIL_ATTACK_VICTIM="${victim}" \
    ANVIL_ATTACK_RECORD="${root}/stage-record" >"${root}/install.log" 2>&1; then
    fail "release installer published a replaced desktop staging name"
fi
[[ "$(<"${victim}")" == 'untouched replaced-stage victim' ]] \
    || fail "release installer followed a replaced desktop staging link"
[[ "$(<"${desktop}")" == 'old desktop bytes' ]] \
    || fail "failed desktop publication changed the old desktop"
changed_stage="$(<"${root}/stage-record")"
[[ -L "${changed_stage}" && "$(readlink -- "${changed_stage}")" == "${victim}" ]] \
    || fail "cleanup unlinked an unknown replacement staging path"

printf 'release installer isolation contract: ok\n'
