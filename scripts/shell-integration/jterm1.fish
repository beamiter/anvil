# jterm1 shell integration for fish.
#
# Source from ~/.config/fish/config.fish:
#     if test "$TERM_PROGRAM" = jterm1
#         source /path/to/jterm1.fish
#     end
#
# Emits OSC 133 (FTCS) command lifecycle marks and OSC 7 cwd updates.

# Retain the per-pane cwd authenticator only as an unexported fish global, and
# erase its public environment spelling before any child process can inherit it.
# This precedes the load guard so re-sourcing cannot leak a newly supplied token.
if set -q JTERM1_CWD_TOKEN
    set --global --unexport __jterm1_cwd_token "$JTERM1_CWD_TOKEN"
    set --erase JTERM1_CWD_TOKEN
end

if set -q __jterm1_fish_loaded
    return 0
end
set -g __jterm1_fish_loaded 1

function __jterm1_osc
    printf '\033]%s\007' $argv[1]
end

function __jterm1_report_cwd --on-variable PWD
    set -l host
    if set -q __jterm1_cwd_token; and test -n "$__jterm1_cwd_token"
        set host "jterm1-$__jterm1_cwd_token"
    else
        set host (hostname 2>/dev/null; or echo localhost)
    end
    set -l enc (string escape --style=url -- $PWD)
    __jterm1_osc "7;file://$host$enc"
end

function __jterm1_prompt_start  ; __jterm1_osc "133;A" ; end
function __jterm1_prompt_end    ; __jterm1_osc "133;B" ; end
function __jterm1_command_start ; __jterm1_osc "133;C" ; end
function __jterm1_command_end   ; __jterm1_osc "133;D;$argv[1]" ; end

function __jterm1_preexec --on-event fish_preexec
    __jterm1_command_start
end

function __jterm1_postexec --on-event fish_postexec
    __jterm1_command_end $status
end

# Wrap the existing fish_prompt so we don't have to fight user customizations.
if not functions -q __jterm1_orig_prompt
    functions -c fish_prompt __jterm1_orig_prompt
    function fish_prompt
        __jterm1_prompt_start
        __jterm1_orig_prompt
        __jterm1_prompt_end
    end
end

# Initial cwd report on first load.
__jterm1_report_cwd

set -gx TERM_PROGRAM jterm1
