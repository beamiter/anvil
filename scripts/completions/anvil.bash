# bash completion for anvil

_anvil()
{
    local current previous
    current="${COMP_WORDS[COMP_CWORD]}"
    previous="${COMP_WORDS[COMP_CWORD-1]}"

    case "${previous}" in
        -c|--config|--check-config)
            COMPREPLY=($(compgen -f -- "${current}"))
            return
            ;;
        -d|--working-directory)
            COMPREPLY=($(compgen -d -- "${current}"))
            return
            ;;
        --mode)
            COMPREPLY=($(compgen -W "block vte" -- "${current}"))
            return
            ;;
        --shell-integration|--generate-completion|--completion)
            COMPREPLY=($(compgen -W "bash zsh fish pwsh" -- "${current}"))
            return
            ;;
        -e|--execute)
            COMPREPLY=($(compgen -c -- "${current}"))
            return
            ;;
    esac

    case "${current}" in
        --mode=*)
            local value="${current#*=}"
            COMPREPLY=($(compgen -W "block vte" -- "${value}"))
            COMPREPLY=("${COMPREPLY[@]/#/--mode=}")
            return
            ;;
        --shell-integration=*|--generate-completion=*|--completion=*)
            local prefix="${current%%=*}="
            local value="${current#*=}"
            COMPREPLY=($(compgen -W "bash zsh fish pwsh" -- "${value}"))
            COMPREPLY=("${COMPREPLY[@]/#/${prefix}}")
            return
            ;;
        --config=*|--check-config=*|--working-directory=*)
            local prefix="${current%%=*}="
            local value="${current#*=}"
            COMPREPLY=($(compgen -f -- "${value}"))
            COMPREPLY=("${COMPREPLY[@]/#/${prefix}}")
            return
            ;;
    esac

    local options="
        -h --help -V --version
        -c --config -d --working-directory -e --execute
        --mode --no-restore --safe-mode
        --doctor --json --check-config --restore-config-backup
        --config-path --init-config --print-default-config
        --shell-integration --generate-completion --completion
    "
    COMPREPLY=($(compgen -W "${options}" -- "${current}"))
}

complete -o bashdefault -o default -F _anvil anvil
