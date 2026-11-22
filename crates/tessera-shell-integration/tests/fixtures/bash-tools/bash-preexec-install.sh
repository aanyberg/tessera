# Stand-in for bash-preexec (https://github.com/rcaloras/bash-preexec): the install hook of v0.7.0 by default, of
# v0.6.0 or v0.5.0 when __bp_standin_version says so (bash-preexec-0.6.sh, bash-preexec-0.5.sh). Written for tests
# (not a copy), same function names and control flow around PROMPT_COMMAND, the DEBUG trap and PS0.
# What matters for Tessera:
#  1. sourcing adds __bp_install_string to PROMPT_COMMAND: 0.7.0 `__bp_install "$_"`, as a new ARRAY element on
#     bash >= 5.1, newline-joined before; 0.5.0/0.6.0 a three-line text ending in `__bp_install`, always on element 0;
#  2. at the FIRST prompt __bp_install rewrites PROMPT_COMMAND to
#       <prologue> \n <existing> \n __bp_interactive_mode
#     (0.7.0 and 0.6.0 on bash >= 5.1: the closing entry is a new array element; 0.5.0 keeps it in element 0, before
#     any other element), then runs precmd_functions itself and turns preexec on;
#  3. <prologue> runs precmd_functions BEFORE the other PROMPT_COMMAND entries at every later prompt;
#  4. the 0.5.0/0.6.0 install text saves `trap -p DEBUG`; __bp_install turns that trap into a preexec function;
#  5. preexec_functions run from PS0 (0.7.0 on bash >= 5.3) or from the DEBUG trap once __bp_interactive_mode armed
#     it; the trap drops the armed state for a command that is itself a PROMPT_COMMAND entry.
__bp_standin_version=${__bp_standin_version:-0.7}
bash_preexec_imported=defined
__bp_imported=$bash_preexec_imported
__bp_preexec_interactive_mode=
__bp_inside_precmd=0
__bp_inside_preexec=0
__bp_last_ret_value=0
precmd_functions=()
preexec_functions=()
__bp_prologue=__bp_precmd_invoke_cmd
__bp_install_string=$'__bp_trap_string="$(trap -p DEBUG)"\ntrap - DEBUG\n__bp_install'
if [[ $__bp_standin_version == 0.7 ]]; then
    __bp_prologue='__bp_precmd_invoke_cmd "$_"'
    __bp_install_string='__bp_install "$_"'
fi
__bp_use_array_prompt_command() { (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); }
__bp_use_ps0() {
    [[ $__bp_standin_version == 0.7 && -z "${__bp_standin_no_ps0-}" ]] &&
        (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 3) ))
}
__bp_set_ret_value() { return ${1:+"$1"}; }
# Drops `:` lines and the separators at both ends.
__bp_sanitize_string() {
    local var=$1 text=$'\n'$2$'\n' nl=$'\n'
    while [[ $text == *"$nl:$nl"* ]]; do text=${text//"$nl:$nl"/$nl}; done
    while [[ $text == [$';\n\t ']* ]]; do text=${text#?}; done
    while [[ $text == *[$';\n\t '] ]]; do text=${text%?}; done
    [[ $text == : ]] && text=
    printf -v "$var" '%s' "$text"
}
__bp_interactive_mode() {
    if [[ $__bp_standin_version == 0.7 && "${1-}" != force ]] && (( ${#FUNCNAME[*]} > 1 )); then
        return 0
    fi
    __bp_preexec_interactive_mode=on
}
__bp_in_prompt_command() {
    local prompt_command_array IFS=$'\n;' command
    read -rd '' -a prompt_command_array <<< "${PROMPT_COMMAND[*]:-}"
    for command in "${prompt_command_array[@]:-}"; do
        [[ $command == "${1:-}" ]] && return 0
    done
    return 1
}
__bp_precmd_invoke_cmd() {
    __bp_last_ret_value=$?
    local lastarg=$_ f
    (( __bp_inside_precmd > 0 )) && return "$__bp_last_ret_value"
    [[ $__bp_standin_version == 0.7 ]] && __bp_install_prompt_command
    local __bp_inside_precmd=1
    for f in "${precmd_functions[@]}"; do
        if type -t "$f" >/dev/null; then
            __bp_set_ret_value "$__bp_last_ret_value" "$lastarg"
            "$f"
        fi
    done
    __bp_set_ret_value "$__bp_last_ret_value" "$lastarg"
}
__bp_invoke_preexec_functions() {
    local f
    for f in "${preexec_functions[@]:-}"; do
        if type -t "$f" >/dev/null; then
            __bp_set_ret_value "$__bp_last_ret_value" "$1"
            "$f" "$2"
        fi
    done
}
__bp_invoke_preexec_from_ps0() { __bp_invoke_preexec_functions "${1:-}" "$(HISTTIMEFORMAT='' builtin history 1)"; }
__bp_preexec_invoke_exec() {
    local lastarg=$_
    (( __bp_inside_preexec > 0 )) && return 0
    local __bp_inside_preexec=1
    [[ -n "$__bp_preexec_interactive_mode" ]] || return 0
    (( BASH_SUBSHELL == 0 )) && __bp_preexec_interactive_mode=
    if __bp_in_prompt_command "${BASH_COMMAND:-}"; then
        __bp_preexec_interactive_mode=
        return 0
    fi
    __bp_invoke_preexec_functions "$lastarg" "$BASH_COMMAND"
    __bp_set_ret_value 0 "$lastarg"
}
__bp_remove_from_prompt_command() {
    local i text
    if __bp_use_array_prompt_command; then
        for i in "${!PROMPT_COMMAND[@]}"; do
            text=${PROMPT_COMMAND[i]:-}
            text=${text//"$1"/:}
            __bp_sanitize_string text "$text"
            if [[ -n "$text" ]]; then PROMPT_COMMAND[i]=$text; else unset -v 'PROMPT_COMMAND[i]'; fi
        done
    else
        text=${PROMPT_COMMAND:-}
        __bp_sanitize_string PROMPT_COMMAND "${text//"$1"/:}"
    fi
}
# Puts the prologue first and __bp_interactive_mode last; status 1 when they are there already.
__bp_install_prompt_command() {
    local prompt_command=${PROMPT_COMMAND:-} epilogue=__bp_interactive_mode
    if __bp_use_array_prompt_command; then
        local IFS=$'\n'
        prompt_command="${PROMPT_COMMAND[*]:-}"
        IFS=$' \t\n'
    fi
    [[ "$prompt_command" == "$__bp_prologue"$'\n'* && "$prompt_command" == *$'\n'"$epilogue" ]] && return 1
    __bp_remove_from_prompt_command "$__bp_prologue"
    __bp_remove_from_prompt_command "$epilogue"
    PROMPT_COMMAND=$__bp_prologue${PROMPT_COMMAND:+$'\n'$PROMPT_COMMAND}
    if __bp_use_array_prompt_command; then PROMPT_COMMAND+=("$epilogue"); else PROMPT_COMMAND+=$'\n'$epilogue; fi
}
__bp_install() {
    local lastexit=$? lastarg=$_ existing
    [[ "${PROMPT_COMMAND[*]:-}" == *__bp_precmd_invoke_cmd* ]] && return 1
    if __bp_use_ps0; then
        PS0=${PS0-}'${ __bp_invoke_preexec_from_ps0 "$_" >&2; }'
    else
        trap '__bp_preexec_invoke_exec "$_"' DEBUG
    fi
    # The DEBUG trap the 0.5/0.6 install text read is kept as a preexec function, as bash-preexec does.
    local prior_trap
    prior_trap=$(sed "s/[^']*'\(.*\)'[^']*/\1/" <<<"${__bp_trap_string:-}")
    unset __bp_trap_string
    if [[ -n "$prior_trap" ]]; then
        eval '__bp_original_debug_trap() {
          '"$prior_trap"'
        }'
        preexec_functions+=(__bp_original_debug_trap)
    fi
    case $__bp_standin_version in
        0.7)
            __bp_remove_from_prompt_command "$__bp_install_string"
            __bp_install_prompt_command
            ;;
        *)
            existing=${PROMPT_COMMAND:-}
            __bp_sanitize_string existing "${existing//"$__bp_install_string"/:}"
            PROMPT_COMMAND=$__bp_prologue${existing:+$'\n'$existing}
            if [[ $__bp_standin_version == 0.6 ]] && __bp_use_array_prompt_command; then
                PROMPT_COMMAND+=(__bp_interactive_mode)
            else
                PROMPT_COMMAND+=$'\n__bp_interactive_mode'
            fi
            ;;
    esac
    precmd_functions+=(precmd)
    preexec_functions+=(preexec)
    __bp_set_ret_value "$lastexit" "$lastarg"
    __bp_precmd_invoke_cmd force
    __bp_interactive_mode force
}
declare -ft __bp_install
if [[ $__bp_standin_version == 0.7 ]] && __bp_use_array_prompt_command; then
    PROMPT_COMMAND+=("$__bp_install_string")
else
    __bp_sanitize_string __bp_sanitized "${PROMPT_COMMAND:-}"
    [[ -n "$__bp_sanitized" ]] && PROMPT_COMMAND=$__bp_sanitized$'\n'
    unset __bp_sanitized
    PROMPT_COMMAND+=$__bp_install_string
fi
