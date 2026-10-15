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
printf '%s install upgrade: ok\n' "$APP"
