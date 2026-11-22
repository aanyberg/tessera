# Stand-in for the PROMPT_COMMAND-wrapping part of `starship init bash` as released in starship v1.27.0
# (2026-10-17). Upstream: https://github.com/starship/starship/blob/v1.27.0/src/init/starship.bash
# Written for tests (not a copy): same variable names and the same control flow around PROMPT_COMMAND,
# the DEBUG trap and PS1; the real `starship` binary is replaced by _fake_starship_prompt.
# v1.27.0 behaviour that matters: the existing PROMPT_COMMAND is MOVED into STARSHIP_PROMPT_COMMAND and
# PROMPT_COMMAND becomes the single word `starship_precmd`, which evals the moved text at EVERY prompt and
# then OVERWRITES PS1.
_fake_starship_prompt() { printf '%s' 'S$ '; }
_starship_set_return() { return "${1:-0}"; }
starship_preexec() {
    local PREV_LAST_ARG=$1
    if [ "${STARSHIP_PREEXEC_READY:-}" = "true" ]; then
        STARSHIP_PREEXEC_READY=false
        STARSHIP_START_TIME=0
    fi
    : "$PREV_LAST_ARG"
}
starship_precmd() {
    STARSHIP_CMD_STATUS=$? STARSHIP_PIPE_STATUS=("${PIPESTATUS[@]}")
    jobs &>/dev/null
    "${starship_precmd_user_func-:}"
    _starship_set_return "$STARSHIP_CMD_STATUS"
    if [[ -n "${STARSHIP_PROMPT_COMMAND-}" ]]; then
        eval "$STARSHIP_PROMPT_COMMAND"
    fi
    PS1="$(_fake_starship_prompt)"
    STARSHIP_PREEXEC_READY=true
    _starship_set_return "$STARSHIP_CMD_STATUS"
}
# bash < 4.4 / no bash-preexec: layer the DEBUG trap, as upstream does (4.4+ uses PS0 instead).
if [[ -n "${bash_preexec_imported:-}" || -n "${__bp_imported:-}" || -n "${preexec_functions-}" || -n "${precmd_functions-}" ]]; then
    starship_preexec_all() { starship_preexec "$_"; }
    preexec_functions+=(starship_preexec_all)
    precmd_functions+=(starship_precmd)
else
    if [[ "${BASH_VERSINFO[0]}" -gt 4 || ( "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 ) ]]; then
        starship_preexec_ps0() { printf 0; }
        if [[ "${PS0-}" != *"starship_preexec_ps0"* ]]; then
            PS0='${STARSHIP_START_TIME:$((STARSHIP_START_TIME="$(starship_preexec_ps0)",STARSHIP_PREEXEC_READY=0,0)):0}'"${PS0-}"
        fi
    else
        eval "STARSHIP_DEBUG_TRAP=($(trap -p DEBUG))"
        STARSHIP_DEBUG_TRAP=("${STARSHIP_DEBUG_TRAP[2]}")
        if [[ -z "$STARSHIP_DEBUG_TRAP" ]]; then
            trap 'starship_preexec "$_"' DEBUG
        elif [[ "$STARSHIP_DEBUG_TRAP" != 'starship_preexec "$_"' && "$STARSHIP_DEBUG_TRAP" != 'starship_preexec_all "$_"' ]]; then
            starship_preexec_all() {
                local PREV_LAST_ARG=$1 ; eval -- "$STARSHIP_DEBUG_TRAP"; starship_preexec; : "$PREV_LAST_ARG";
            }
            trap 'starship_preexec_all "$_"' DEBUG
        fi
    fi
    if [[ -z "${PROMPT_COMMAND-}" ]]; then
        PROMPT_COMMAND="starship_precmd"
    elif [[ "$PROMPT_COMMAND" != *"starship_precmd"* ]]; then
        # the move that breaks a `. "$TESSERA_BASH_SCRIPT"` loader once the variable is unset
        STARSHIP_PROMPT_COMMAND="$PROMPT_COMMAND"
        PROMPT_COMMAND="starship_precmd"
    fi
fi
shopt -s checkwinsize
export STARSHIP_SHELL="bash"
PS2='> '
