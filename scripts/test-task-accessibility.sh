#!/usr/bin/env bash
set -euo pipefail
# Takes an already-built Anvil test executable, not cargo and never the app CLI.
# Native execution must be separately approved; this owns a fresh display/bus.
if [[ $# != 2 ]]; then
  echo "Usage: $0 /absolute/anvil-test-binary /absolute/empty-evidence-directory" >&2
  exit 2
fi
binary=$(realpath "$1")
evidence=$(realpath -m "$2")
script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
for command in dbus-run-session xvfb-run openbox orca python3; do
  command -v "$command" >/dev/null || { echo "Missing $command; native gate NOT RUN" >&2; exit 2; }
done
sandbox=$(mktemp -d)
trap 'rm -rf -- "$sandbox"' EXIT
mkdir -p "$sandbox"/{home,config,cache,data,runtime}
chmod 700 "$sandbox/runtime"
# No inherited bus, accessibility, display, audio, loader or Python settings.
printf 'pcm.!default { type null }\nctl.!default { type hw card -1 }\n' > "$sandbox/alsa.conf"
env -i TZ=UTC PATH=/usr/bin:/bin HOME="$sandbox/home" \
XDG_CONFIG_HOME="$sandbox/config" XDG_CACHE_HOME="$sandbox/cache" \
XDG_DATA_HOME="$sandbox/data" XDG_RUNTIME_DIR="$sandbox/runtime" \
PULSE_SERVER="unix:$sandbox/runtime/no-pulse" \
SPEECHD_ADDRESS="unix_socket:$sandbox/runtime/no-speechd" ALSA_CONFIG_PATH="$sandbox/alsa.conf" \
GSETTINGS_BACKEND=memory GDK_BACKEND=x11 GTK_A11Y=atspi ANVIL_A11_OWNED_SESSION=1 \
dbus-run-session -- xvfb-run -a -s '-screen 0 1280x900x24 -nolisten tcp' \
  python3 "$script_dir/qa-task-accessibility.py" --test-binary "$binary" --evidence "$evidence"
