# anvil shell integration for fish.
#
# Source from ~/.config/fish/config.fish:
#     if test "$TERM_PROGRAM" = anvil
#         source /path/to/anvil.fish
#     end
#
# Emits OSC 133 (FTCS) command lifecycle marks and OSC 7 cwd updates.

# Retain the per-pane cwd authenticator only as an unexported fish global, and
# erase its public environment spelling before any child process can inherit it.
# This precedes the load guard so re-sourcing cannot leak a newly supplied token.
if set -q ANVIL_CWD_TOKEN
    set --global --unexport __anvil_cwd_token "$ANVIL_CWD_TOKEN"
    set --erase ANVIL_CWD_TOKEN
end

# Fish does not participate in authenticated Agent execution, but scrub the
# reserved channel spellings before any child process can inherit them.
set --erase ANVIL_SHELL_INTEGRATION_FD ANVIL_SHELL_INTEGRATION_TOKEN

if set -q __anvil_fish_loaded
    return 0
end
set -g __anvil_fish_loaded 1
set --global --unexport __anvil_command_token "anvil-fish-$fish_pid"
set --global --unexport __anvil_marker_seq 0
set --global --unexport __anvil_marker_id ""

function __anvil_osc
    printf '\033]%s\007' $argv[1]
end

function __anvil_report_cwd --on-variable PWD
    set -l host
    if set -q __anvil_cwd_token; and test -n "$__anvil_cwd_token"
        set host "anvil-$__anvil_cwd_token"
    else
        set host (hostname 2>/dev/null; or echo localhost)
    end
    set -l enc (string escape --style=url -- $PWD)
    __anvil_osc "7;file://$host$enc"
end

function __anvil_prompt_start  ; __anvil_osc "133;A" ; end
function __anvil_prompt_end    ; __anvil_osc "133;B" ; end
function __anvil_command_start
    set -g __anvil_marker_seq (math "$__anvil_marker_seq + 1")
    set -g __anvil_marker_id "$__anvil_command_token-$__anvil_marker_seq"
    __anvil_osc "133;C;id=$__anvil_marker_id"
end
function __anvil_command_end
    __anvil_osc "133;D;$argv[1];id=$__anvil_marker_id"
    set -g __anvil_marker_id ""
end

function __anvil_preexec --on-event fish_preexec
    __anvil_command_start
end

function __anvil_postexec --on-event fish_postexec
    __anvil_command_end $status
end

# Wrap the existing fish_prompt so we don't have to fight user customizations.
if not functions -q __anvil_orig_prompt
    functions -c fish_prompt __anvil_orig_prompt
    function fish_prompt
        __anvil_prompt_start
        __anvil_orig_prompt
        __anvil_prompt_end
    end
end

# Initial cwd report on first load.
__anvil_report_cwd

set -gx TERM_PROGRAM anvil
