#!/usr/bin/env bash
# Pretty-print anvil's JSON session state.

set -euo pipefail

CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
STATE_DIR="${CONFIG_HOME}/anvil"

if (($# > 1)); then
    echo "Usage: $0 [STATE_FILE]" >&2
    exit 2
fi

if (($# == 1)); then
    STATE_FILE="$1"
else
    shopt -s nullglob
    candidates=("${STATE_DIR}"/tabs.*.state)
    [[ -f "${STATE_DIR}/tabs.state" ]] && candidates+=("${STATE_DIR}/tabs.state")
    shopt -u nullglob
    if ((${#candidates[@]} == 0)); then
        echo "No session snapshots found under ${STATE_DIR}" >&2
        exit 1
    fi
    STATE_FILE="${candidates[0]}"
    for candidate in "${candidates[@]:1}"; do
        if [[ "${candidate}" -nt "${STATE_FILE}" ]]; then
            STATE_FILE="${candidate}"
        fi
    done
fi

if [[ ! -f "${STATE_FILE}" ]]; then
    echo "No state file found at ${STATE_FILE}" >&2
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "Error: jq is required to inspect ${STATE_FILE}" >&2
    exit 1
fi

if ! jq empty "${STATE_FILE}" 2>/dev/null; then
    echo "Error: ${STATE_FILE} is not valid JSON." >&2
    exit 1
fi

tab_count="$(jq '.tabs | length' "${STATE_FILE}")"
active_index="$(jq '.active // 0' "${STATE_FILE}")"

echo "anvil session state"
echo "===================="
echo "File: ${STATE_FILE}"
echo "Tabs: ${tab_count}"
if ((tab_count > 0)); then
    echo "Active tab: $((active_index + 1)) (stored index ${active_index})"
fi
echo
echo "Warning: state may contain working directories and commands."
echo

jq_color=()
if [[ -t 1 ]]; then
    jq_color=(-C)
fi

for ((index = 0; index < tab_count; index++)); do
    title="$(jq -r --argjson index "${index}" '.tabs[$index].title // "Untitled"' "${STATE_FILE}")"
    custom_title="$(jq -r --argjson index "${index}" '.tabs[$index].custom_title // false' "${STATE_FILE}")"
    pane_count="$(jq --argjson index "${index}" '[.tabs[$index].layout | .. | objects | select(.type == "leaf")] | length' "${STATE_FILE}")"
    layout_type="$(jq -r --argjson index "${index}" '.tabs[$index].layout.type // "unknown"' "${STATE_FILE}")"

    marker=" "
    if ((index == active_index)); then
        marker="*"
    fi
    echo "${marker} Tab $((index + 1)): ${title}"
    echo "    Custom title: ${custom_title}"
    echo "    Layout: ${layout_type}; panes: ${pane_count}"
    jq "${jq_color[@]}" --argjson index "${index}" '.tabs[$index].layout' "${STATE_FILE}" | sed 's/^/      /'
    echo
done
