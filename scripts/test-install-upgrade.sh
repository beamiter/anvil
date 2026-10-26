#!/usr/bin/env bash
# Real isolated-home installs; never execute the binary fixture.
set -Eeuo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
APP="anvil"
ROOT="$(mktemp -d)"
trap 'rm -rf -- "$ROOT"' EXIT
export HOME="$ROOT/home space" XDG_CONFIG_HOME="$ROOT/config space" XDG_DATA_HOME="$ROOT/data space"
export DESTDIR=
mkdir -p "$HOME/.local/bin" "$XDG_CONFIG_HOME/$APP"
printf '#!/bin/sh\necho old\n' > "$HOME/.local/bin/$APP"
printf '#!/bin/sh\necho new\n' > "$ROOT/binary"
chmod 0755 "$ROOT/binary"
printf 'custom = true\n' > "$XDG_CONFIG_HOME/$APP/config.toml"
installer="$SCRIPT_DIR/install.sh"
"$installer" --binary "$ROOT/binary" --no-desktop > "$ROOT/install.log"
cmp "$ROOT/binary" "$HOME/.local/bin/$APP"
cmp "$SCRIPT_DIR/../config.toml.example" "$XDG_CONFIG_HOME/$APP/config.toml"
shopt -s nullglob
backups=("$XDG_CONFIG_HOME/$APP/config.toml".backup.*)
((${#backups[@]} == 1))
[[ $(cat "${backups[0]}") == 'custom = true' ]]
[[ $(stat -c %a "${backups[0]}") == 600 ]]
"$installer" --binary "$ROOT/binary" --no-desktop > "$ROOT/repeat.log"
backups=("$XDG_CONFIG_HOME/$APP/config.toml".backup.*)
((${#backups[@]} == 1))
printf 'custom = false\n' > "$XDG_CONFIG_HOME/$APP/config.toml"
"$installer" --binary "$ROOT/binary" --no-desktop --no-config > "$ROOT/keep.log"
[[ $(cat "$XDG_CONFIG_HOME/$APP/config.toml") == 'custom = false' ]]
"$installer" --binary "$ROOT/binary" --no-desktop --dry-run > "$ROOT/dry.log"
[[ $(cat "$XDG_CONFIG_HOME/$APP/config.toml") == 'custom = false' ]]
# Exercise the shared source/release config helper with controlled schemas.
# The real templates intentionally differ (some contain only comments).
(
    # The shared helper is checked separately.
    # shellcheck source=/dev/null
    source "$SCRIPT_DIR/install-config.sh"
    die() { printf '%s\n' "$*" >&2; exit 1; }
    DRY_RUN=0
    fixture_dir="$ROOT/config comparison"
    mkdir -p "$fixture_dir"
    bundled="$fixture_dir/default.toml"
    current="$fixture_dir/config.toml"
    expected_backups=0

    assert_backup_count() {
        local copies=("$current".backup.*)
        ((${#copies[@]} == expected_backups))
    }
    install_fixture() {
        install_config_with_backup "$bundled" "$current" "$current" > "$ROOT/helper.log"
    }
    assert_preserved() {
        local identity
        cp "$current" "$fixture_dir/expected.toml"
        identity="$(stat -c '%d:%i:%s:%y:%z' "$current")"
        install_fixture
        cmp "$fixture_dir/expected.toml" "$current"
        [[ $(stat -c '%d:%i:%s:%y:%z' "$current") == "$identity" ]]
        assert_backup_count
    }
    assert_replaced() {
        local before=("$current".backup.*) copy found=0
        cp "$current" "$fixture_dir/expected.toml"
        install_fixture
        cmp "$bundled" "$current"
        expected_backups=$((expected_backups + 1))
        assert_backup_count
        for copy in "$current".backup.*; do
            if [[ ! " ${before[*]} " == *" $copy "* ]]; then
                cmp "$fixture_dir/expected.toml" "$copy"
                [[ $(stat -c %a "$copy") == 600 ]]
                found=$((found + 1))
            fi
        done
        ((found == 1))
    }

    cat > "$bundled" <<'TOML'
font = "default"
opacity = 0.95
[ui]
size = 14
TOML
    # First installation creates the config without a backup; repeating is inert.
    install_fixture
    cmp "$bundled" "$current"
    assert_backup_count
    assert_preserved
    # Changing values, comments, formatting and ordering must preserve all bytes.
    cat > "$current" <<'TOML'
# Personal settings stay byte-for-byte intact.
opacity=0.5
font = 'custom'
[ui]
size=21 # customized
TOML
    assert_preserved
    # A field added to the new defaults requires a backed-up replacement.
    printf 'color = "blue"\n' >> "$bundled"
    assert_replaced
    # A removed field also changes the schema, even without any additions.
    printf 'retired = true\n' >> "$current"
    assert_replaced
    # Renames and nested changes must not be missed by comparing only top-level keys.
    sed 's/^size = /renamed_size = /' "$bundled" > "$current"
    assert_replaced
    sed '/^color = /d' "$bundled" > "$current"
    assert_replaced
    # Dotted/quoted keys and inline tables are equivalent TOML representations.
    cat > "$bundled" <<'TOML'
"key.with.dots" = "default"
ui = { size = 14, color = "blue" }
TOML
    cat > "$current" <<'TOML'
'key.with.dots' = 'custom'
ui.color = 'red'
ui.size = 21
TOML
    assert_preserved
    # Arrays of tables compare their fields, while scalar array contents are values.
    cat > "$bundled" <<'TOML'
commands = ["default"]
[[bindings]]
key = "A"
action = "default"
TOML
    cat > "$current" <<'TOML'
commands = ['custom', 'second']
[[bindings]]
action = 'custom'
key = 'B'
TOML
    assert_preserved
    printf 'enabled = true\n' >> "$bundled"
    assert_replaced
    # Fields are compared across every array element, independent of entry count.
    cat > "$bundled" <<'TOML'
[[bindings]]
key = "A"
[[bindings]]
action = "default"
TOML
    cat > "$current" <<'TOML'
[[bindings]]
action = "custom"
key = "B"
TOML
    assert_preserved
    printf '[[bindings]]\nkey = "B"\n' > "$current"
    assert_replaced
    # A multiline string may contain text that looks like fields or sections.
    cat > "$bundled" <<'TOML'
text = """
[not_a_table]
not_a_key = true
"""
TOML
    printf 'text = "custom"\n' > "$current"
    assert_preserved
    # Scalar types are values too; changing a number to a string preserves edits.
    printf 'value = 14\n' > "$bundled"
    printf 'value = "custom"\n' > "$current"
    assert_preserved
    # User-created list entries must not trigger a reset against an empty default.
    printf 'hosts = []\n' > "$bundled"
    printf '[[hosts]]\nname = "custom"\naddress = "host.example"\n' > "$current"
    assert_preserved
    # Empty lists inside array entries likewise preserve populated custom tables.
    printf 'groups = [{ hosts = [] }]\n' > "$bundled"
    printf 'groups = [{ hosts = [{ name = "custom" }] }]\n' > "$current"
    assert_preserved
    # Valid but excessively nested TOML must fail closed at parser recursion limits.
    {
        printf 'nested = '
        for ((level=0; level<1200; level++)); do printf '['; done
        printf '0'
        for ((level=0; level<1200; level++)); do printf ']'; done
        printf '\n'
    } > "$bundled"
    printf 'value = true\n' > "$current"
    assert_preserved
    # Invalid configs cannot be compared safely and must never be overwritten.
    printf '[broken\n' > "$current"
    assert_preserved
    printf 'value = true\n' > "$current"
    printf '[broken\n' > "$bundled"
    assert_preserved
    # A missing/unavailable parser also keeps the current configuration.
    printf 'new_field = true\n' > "$bundled"
    (
        python3() { return 127; }
        assert_preserved
    )
    # An interpreter failure with exit 1 must not masquerade as changed fields.
    (
        python3() { return 1; }
        assert_preserved
    )
    # A dry-run should not create backups for changed fields.
    printf 'another_field = true\n' >> "$bundled"
    # Used by the sourced config helper.
    # shellcheck disable=SC2034
    DRY_RUN=1
    assert_preserved
)
printf '%s install upgrade: ok\n' "$APP"
