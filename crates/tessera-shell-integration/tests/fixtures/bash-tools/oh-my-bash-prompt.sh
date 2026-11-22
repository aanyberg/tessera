# Stand-in for oh-my-bash's prompt setup: lib/utils.sh `_omb_util_add_prompt_command` +
# `_omb_util_prompt_command_hook`, and a theme whose hook rebuilds PS1 (themes/font/font.theme.sh style).
# Upstream: https://github.com/ohmybash/oh-my-bash (lib/utils.sh, master 2026-11; themes/font/font.theme.sh).
# Written for tests (not a copy). Behaviour that matters:
#  - bash < 5.1: PROMPT_COMMAND="_omb_util_prompt_command_hook${PROMPT_COMMAND:+;$PROMPT_COMMAND}" (PREPENDS, joins with `;`)
#  - bash >= 5.1: PROMPT_COMMAND+=(_omb_util_prompt_command_hook) (APPENDS an array element, i.e. runs after ours)
#  - the hook runs the registered theme function, which REBUILDS PS1 from scratch each prompt.
_omb_bash_version=$(( BASH_VERSINFO[0] * 10000 + BASH_VERSINFO[1] * 100 + BASH_VERSINFO[2] ))
_omb_util_prompt_command=()
_omb_util_prompt_command_setup=
_omb_util_setexit() { return "${1:-0}"; }
_omb_util_prompt_command_hook() {
    local status=$? lastarg=$_ hook
    for hook in "${_omb_util_prompt_command[@]}"; do
        _omb_util_setexit "$status" "$lastarg"
        eval -- "$hook"
    done
    _omb_util_setexit "$status"
}
_omb_util_add_prompt_command() {
    local other
    for other in "${_omb_util_prompt_command[@]}"; do [[ $1 == "$other" ]] && return 0; done
    _omb_util_prompt_command+=("$1")
    if [[ ! $_omb_util_prompt_command_setup ]]; then
        _omb_util_prompt_command_setup=1
        local hook=_omb_util_prompt_command_hook
        [[ $PROMPT_COMMAND == *"$hook"* ]] && return 0
        if ((_omb_bash_version >= 50100)); then
            PROMPT_COMMAND+=("$hook")
        else
            PROMPT_COMMAND="$hook${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
        fi
    fi
}
_omb_theme_PROMPT_COMMAND() {
    local RC="$?"
    history -a
    PS1="[omb \W] -> "
}
_omb_util_add_prompt_command _omb_theme_PROMPT_COMMAND
