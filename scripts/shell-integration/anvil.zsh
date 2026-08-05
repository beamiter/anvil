# anvil shell integration for zsh.
#
# Source from ~/.zshrc:
#     [[ $TERM_PROGRAM == anvil ]] && source /path/to/anvil.zsh
# or unconditionally.
#
# Emits OSC 133 (FTCS) command lifecycle marks and OSC 7 cwd updates.

# Capture the per-pane cwd authenticator as a private shell parameter, then
# immediately remove its exported spelling so child processes cannot inherit
# it. Do this before the load guard for safe re-sourcing.
if (( ${+ANVIL_CWD_TOKEN} )); then
    typeset -g __anvil_cwd_token=$ANVIL_CWD_TOKEN
    unset ANVIL_CWD_TOKEN
    typeset -g +x __anvil_cwd_token
fi

[[ -n ${__ANVIL_ZSH_LOADED:-} ]] && return 0
__ANVIL_ZSH_LOADED=1
typeset -g __anvil_marker_nonce="$$-${RANDOM}-${RANDOM}-${RANDOM}-${RANDOM}-${RANDOM}-${RANDOM}-${RANDOM}-${RANDOM}"
typeset -gi __anvil_marker_seq=0
typeset -g __anvil_marker_id=""

__anvil_osc() { printf '\033]%s\007' "$1"; }

__anvil_prompt_start()  { __anvil_osc "133;A"; }
__anvil_prompt_end()    { __anvil_osc "133;B"; }
__anvil_command_start() {
    (( __anvil_marker_seq++ ))
    __anvil_marker_id="${__anvil_marker_nonce}-${__anvil_marker_seq}"
    __anvil_osc "133;C;id=${__anvil_marker_id}"
}
__anvil_command_end() {
    __anvil_osc "133;D;$1;id=${__anvil_marker_id}"
    __anvil_marker_id=""
}

__anvil_report_cwd() {
    local host
    if [[ -n ${__anvil_cwd_token:-} ]]; then
        host="anvil-${__anvil_cwd_token}"
    else
        host=${HOST:-${HOSTNAME:-localhost}}
    fi
    local out="" i ch
    for (( i=1; i<=${#PWD}; i++ )); do
        ch=${PWD[i]}
        case $ch in
            [A-Za-z0-9._~/-]) out+=$ch ;;
            *) printf -v out '%s%%%02X' "$out" "'$ch" ;;
        esac
    done
    __anvil_osc "7;file://${host}${out}"
}

__anvil_in_command=0
__anvil_preexec() {
    if (( __anvil_in_command == 0 )); then
        __anvil_in_command=1
        __anvil_command_start
    fi
}

__anvil_precmd() {
    local ec=$?
    if (( __anvil_in_command == 1 )); then
        __anvil_command_end "$ec"
        __anvil_in_command=0
    fi
    __anvil_report_cwd
    __anvil_prompt_start
}

# Append the prompt-end mark to PS1 inside %{...%} so widths stay correct.
if [[ -z ${__ANVIL_PS1_HOOKED:-} ]]; then
    PS1="${PS1}%{$(__anvil_prompt_end)%}"
    __ANVIL_PS1_HOOKED=1
fi

autoload -Uz add-zsh-hook
add-zsh-hook preexec __anvil_preexec
add-zsh-hook precmd  __anvil_precmd

export TERM_PROGRAM=anvil
