#!/usr/bin/env bash
# Hosted native only. Never use a preexisting/user display or session bus.
set -Eeuo pipefail
if [[ $# -ne 4 ]]; then
    echo 'usage: run-interaction-budget.sh INSTRUMENTED_BINARY PANES OUTPUT_JSON --smoke|--full-budget' >&2
    exit 2
fi
if [[ "$4" != --smoke && "$4" != --full-budget ]]; then
    echo 'explicit --smoke or reviewed --full-budget mode required' >&2
    exit 2
fi
for tool in dbus-run-session xvfb-run xdotool python3; do
    command -v "$tool" >/dev/null || { echo "missing native fixture dependency: $tool" >&2; exit 2; }
done
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
export ANVIL_BUDGET_ISOLATED_DISPLAY=1
exec dbus-run-session -- xvfb-run -a -s '-screen 0 1600x1000x24' \
    python3 "${script_dir}/collect-interaction-budget.py" \
    --binary "$1" --panes "$2" --output "$3" "${@:4}"
