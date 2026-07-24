# jterm1 shell integration for bash.
#
# Emits FTCS / OSC 133 marks so jterm1 can attribute output to discrete blocks
# and know each command's exit code, plus OSC 7 so it tracks the working dir.
#
# Source from ~/.bashrc, e.g.
#     [[ $TERM_PROGRAM == jterm1 ]] && source /path/to/jterm1.bash
# or unconditionally:
#     source /path/to/jterm1.bash
#
# Safe to source under non-jterm1 terminals: the OSC sequences are silently
# discarded by anything that doesn't parse them.

# Keep jterm1's per-pane cwd authenticator in a private, non-exported shell
# variable. Remove the public environment variable before any prompt command
# (or child process) can inherit it. This intentionally precedes the load guard
# so re-sourcing cannot leave a newly supplied token exported.
if [[ ${JTERM1_CWD_TOKEN+x} ]]; then
    __jterm1_cwd_token=$JTERM1_CWD_TOKEN
    unset JTERM1_CWD_TOKEN
    export -n __jterm1_cwd_token
fi

# Guard against double-sourcing in the same shell.
[[ -n ${__JTERM1_BASH_LOADED:-} ]] && return 0
__JTERM1_BASH_LOADED=1
__jterm1_integration_source=${BASH_SOURCE[0]}

# Send raw OSC payload terminated with BEL.
__jterm1_osc() { printf '\033]%s\007' "$1"; }

# OSC 133 ;A — prompt is about to be drawn.
# OSC 133 ;B — prompt drawn, waiting for user input.
__jterm1_prompt_start() { __jterm1_osc "133;A"; }
__jterm1_prompt_end()   { __jterm1_osc "133;B"; }

# OSC 133 ;C — user submitted, command is running.
__jterm1_command_start() { __jterm1_osc "133;C"; }

# OSC 133 ;D;<exit> — command finished with the given exit code.
__jterm1_command_end() {
    local ec=$1
    __jterm1_osc "133;D;${ec}"
}

# OSC 7 — report current working directory as file:// URI.
__jterm1_report_cwd() {
    local host
    if [[ -n ${__jterm1_cwd_token:-} ]]; then
        host="jterm1-${__jterm1_cwd_token}"
    else
        host=${HOSTNAME:-localhost}
    fi
    local out="" i ch
    LC_ALL=C
    for (( i=0; i<${#PWD}; i++ )); do
        ch=${PWD:i:1}
        case $ch in
            [A-Za-z0-9._~/-]) out+=$ch ;;
            *) printf -v out '%s%%%02X' "$out" "'$ch" ;;
        esac
    done
    __jterm1_osc "7;file://${host}${out}"
}

# Track command lifecycle. We need two things:
#  1) emit OSC 133;C exactly once per submitted command, after the user hits
#     Enter and before the command actually runs.
#  2) emit OSC 133;D;<exit> after the command returns and before the next
#     prompt is rendered.
#
# Bash gives us DEBUG trap (fires before each simple command) and
# PROMPT_COMMAND (fires before each prompt). We use a flag so the DEBUG trap
# only emits ;C for the first command of a pipeline.

__jterm1_in_command=0
__jterm1_in_prompt_command=0

__jterm1_preexec() {
    [[ -n ${COMP_LINE:-} ]] && return
    [[ ${BASH_SOURCE[1]:-} == "$__jterm1_integration_source" ]] && return

    # DEBUG also fires inside PROMPT_COMMAND functions. Mark the complete
    # prompt phase so neither our hook nor a user's existing hook creates a
    # phantom command block.
    if [[ ${BASH_COMMAND} == "__jterm1_prompt_command" ]]; then
        __jterm1_in_prompt_command=1
        return
    fi
    (( __jterm1_in_prompt_command == 1 )) && return

    if (( __jterm1_in_command == 0 )); then
        __jterm1_in_command=1
        __jterm1_command_start
    fi
}

__jterm1_precmd() {
    local ec=$1
    if (( __jterm1_in_command == 1 )); then
        __jterm1_command_end "${ec}"
        __jterm1_in_command=0
    fi
    __jterm1_report_cwd
    __jterm1_prompt_start
    # Re-inject the user's PS1 with ;B appended so the prompt-end mark sits at
    # the exact spot where input begins. Use \[...\] so the OSC bytes don't
    # count toward readline's column accounting.
    if [[ -z ${__JTERM1_PS1_HOOKED:-} ]]; then
        PS1="${PS1}\[$(__jterm1_prompt_end)\]"
        __JTERM1_PS1_HOOKED=1
    fi
}

# Preserve every existing prompt hook, including Bash 5's array form, while
# making this dispatcher the sole PROMPT_COMMAND visible to the DEBUG trap.
__jterm1_saved_prompt_commands=("${PROMPT_COMMAND[@]:-}")
__jterm1_prompt_command() {
    local ec=$?
    local command
    __jterm1_in_prompt_command=1
    __jterm1_precmd "$ec"
    for command in "${__jterm1_saved_prompt_commands[@]}"; do
        [[ -n $command ]] && builtin eval -- "$command"
    done
    __jterm1_in_prompt_command=0
}

unset PROMPT_COMMAND
PROMPT_COMMAND=__jterm1_prompt_command
export TERM_PROGRAM=jterm1
trap '__jterm1_preexec' DEBUG
