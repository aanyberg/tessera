# Tessera shell integration: zsh bootstrap.
#
# Tessera starts zsh with ZDOTDIR pointing at this directory, so zsh reads this
# file instead of the user's .zshenv. It puts ZDOTDIR back (the user's value
# from TESSERA_ORIG_ZDOTDIR, or unset), sources the user's .zshenv, and then,
# in interactive shells with TESSERA_SHELL_INTEGRATION set, loads
# tessera-integration. zsh reads .zprofile, .zshrc and .zlogin from the
# restored ZDOTDIR (or HOME) as usual. The approach is kitty's; see
# docs/adr/0021-shell-integration-injection.md.
#
# This file can be sourced with aliases enabled, so everything that can be
# quoted is quoted.

# `[[ -v ... ]]` needs zsh 5.4; `${VAR+X}` tells unset from empty everywhere.
if [[ -n "${TESSERA_ORIG_ZDOTDIR+X}" ]]; then
    # ZDOTDIR was in Tessera's environment, so it stays exported.
    'builtin' 'export' ZDOTDIR="$TESSERA_ORIG_ZDOTDIR"
    'builtin' 'unset' 'TESSERA_ORIG_ZDOTDIR'
else
    'builtin' 'unset' 'ZDOTDIR'
fi

# try/always: the integration loads even if the user's .zshenv fails or
# returns, and the status stays the user's.
{
    # An empty ZDOTDIR means "/" to zsh; `${ZDOTDIR-~}` keeps it empty.
    # typeset, in case warn_create_global is in effect.
    'builtin' 'typeset' _tessera_si_file=${ZDOTDIR-~}"/.zshenv"
    # zsh skips unreadable rc files and directories; so do we.
    [[ ! -r "$_tessera_si_file" ]] || 'builtin' 'source' '--' "$_tessera_si_file"
} always {
    if [[ -o 'interactive' && -n "${TESSERA_SHELL_INTEGRATION-}" && "${TESSERA_SHELL_INTEGRATION-}" != '0' ]]; then
        'builtin' 'autoload' '--' 'is-at-least'
        if 'is-at-least' '5.1'; then
            # ${(%):-%x} is this file; :A:h its directory.
            'builtin' 'typeset' _tessera_si_file="${${(%):-%x}:A:h}"/tessera-integration
            if [[ -r "$_tessera_si_file" ]]; then
                'builtin' 'autoload' '-Uz' '--' "$_tessera_si_file"
                "${_tessera_si_file:t}"
                'builtin' 'unfunction' '--' "${_tessera_si_file:t}"
            fi
        else
            'builtin' 'print' '-ru2' -- "zsh $ZSH_VERSION is too old for Tessera shell integration (needs 5.1)"
        fi
    fi
    'builtin' 'unset' '_tessera_si_file'
}
