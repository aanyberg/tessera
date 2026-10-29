# Tessera shell integration for bash (3.2+, the macOS /bin/bash included):
# OSC 133 prompt marks, OSC 7 working directory reports and a one-time
# OSC 1337 handshake.
#
# Tessera does not load this file yet: bash support is a follow-up (the
# login route's PROMPT_COMMAND loader breaks under prompt tools that wrap
# PROMPT_COMMAND, such as starship; docs/adr/0021-shell-integration-injection.md).
# Source it manually (below). The design for loading it automatically:
# - non-login shells start as `bash --rcfile <this file>`, and this file then
#   sources ~/.bashrc (or the file the user gave to --rcfile);
# - login shells (and --posix or --norc ones) get an exported
#   PROMPT_COMMAND that sources this file at the first prompt, after every
#   startup file ran.
# To load it in other shells, add to the end of ~/.bashrc:
#
#   export TESSERA_SHELL_INTEGRATION=enabled
#   source /path/to/shell-integration/bash/tessera.bash
#
# TESSERA_SHELL_INTEGRATION holds space-separated feature words: `enabled`
# (or anything else) turns it on, `0` (or unset) turns it off, `no-cwd`
# drops the OSC 7 reports and `no-prompt-mark` the OSC 133 marks. It is
# unset once read, so shells started from this one are not integrated twice.
#
# Hooks: PROMPT_COMMAND entries close the last command (D), put the prompt
# marks into PS1 (A before it, B after it) and install a DEBUG trap that
# marks the start of the next command (C) and then removes itself, so it
# costs nothing while commands run; it keeps $? and $_ as it found them. An
# existing DEBUG trap keeps running (after ours, and alone while a command
# runs); with bash-preexec loaded, its hook lists are used instead. Once
# installed, PROMPT_COMMAND is no longer exported (child shells have none of
# our functions). An array PROMPT_COMMAND (bash 5.1+) is joined into one
# string. Limits:
# - a PROMPT_COMMAND entry or array element added after ours runs after the
#   prompt marks for one prompt (ours moves back to the end at the next);
#   for an element the trap is not armed that time, so that command ends
#   with a bare D and no C;
# - a DEBUG trap set after this script loaded is replaced at the next
#   prompt (by ours, chained to the trap found at load);
# - a command line that only runs a subshell, `(cd x; make)`, does not fire
#   the DEBUG trap: it ends with a bare D, no C (with `set -T` or `shopt -s
#   extdebug` the subshell inherits the trap and sends the C itself, still
#   followed by a bare D);
# - a login shell's startup file that assigns PROMPT_COMMAND (rather than
#   adding to it) drops the loader, so that shell is not integrated (and
#   the TESSERA_* variables stay in its environment, unread).
#
# bash 3.2 compatibility: no PS0, no associative arrays, no ${var,,}, no
# BASHPID; PROMPT_COMMAND arrays only on 5.1+. Run on 3.2.57 and 5.3.20.

# bash 5.1 and later run every element of an array PROMPT_COMMAND (earlier
# versions only the first): join them into one string so our entries can
# stay first and last.
_tessera_si_arrays=
if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
    _tessera_si_arrays=1
fi
_tessera_si_join_prompt_command() {
    # `${PROMPT_COMMAND[1]+x}`, not `${#PROMPT_COMMAND[@]}`: the latter is
    # an error under `set -u` when PROMPT_COMMAND is a plain string.
    if [[ -n "$_tessera_si_arrays" && -n "${PROMPT_COMMAND[1]+x}" ]]; then
        builtin local IFS=$'\n' joined
        joined="${PROMPT_COMMAND[*]}"
        builtin unset PROMPT_COMMAND
        PROMPT_COMMAND=$joined
    fi
}

# Startup files first: they run even when the integration is disabled.
if [[ -n "${TESSERA_BASH_INJECT-}" ]]; then
    _tessera_si_inject=" $TESSERA_BASH_INJECT "
    builtin unset TESSERA_BASH_INJECT
    case $_tessera_si_inject in
        *" rcfile "*)
            # Started as `bash --rcfile <this file>` (never a login shell):
            # read what bash would have read instead of us. The feature
            # words stay out of the environment of what the startup files
            # run (a startup file may still set its own).
            if [[ -n "${TESSERA_SHELL_INTEGRATION+X}" ]]; then
                _tessera_si_injected=$TESSERA_SHELL_INTEGRATION
                builtin unset TESSERA_SHELL_INTEGRATION
            fi
            if [[ -n "${TESSERA_BASH_RCFILE+X}" ]]; then
                _tessera_si_rc=$TESSERA_BASH_RCFILE
                builtin unset TESSERA_BASH_RCFILE
            else
                _tessera_si_rc=~/.bashrc
            fi
            [[ -f "$_tessera_si_rc" && -r "$_tessera_si_rc" ]] && builtin source "$_tessera_si_rc"
            builtin unset _tessera_si_rc
            # Not exported: read below, then unset (unless a startup file
            # loaded this script already).
            if [[ -z "${TESSERA_SHELL_INTEGRATION+X}" && -n "${_tessera_si_injected+X}" &&
                -z "${_tessera_si_loaded-}" ]]; then
                TESSERA_SHELL_INTEGRATION=$_tessera_si_injected
            fi
            builtin unset _tessera_si_injected
            ;;
        *" prompt-command "*)
            # Sourced from PROMPT_COMMAND at the first prompt: take the
            # loader out again, and keep PROMPT_COMMAND out of the
            # environment of child shells (which have no such functions).
            # The loader text is literal (Tessera sets it with the quotes).
            # Elements after the loader's still run at this prompt (bash
            # runs a copy of the array): see _tessera_si_ret.
            _tessera_si_late_elements=1
            if [[ -n "$_tessera_si_arrays" && -n "${PROMPT_COMMAND[1]+x}" ]]; then
                _tessera_si_late_elements=2
            fi
            _tessera_si_join_prompt_command
            # shellcheck disable=SC2016
            PROMPT_COMMAND=${PROMPT_COMMAND//'. "$TESSERA_BASH_SCRIPT"'/:}
            [[ "$PROMPT_COMMAND" == ":" ]] && PROMPT_COMMAND=
            builtin export -n PROMPT_COMMAND
            builtin unset TESSERA_BASH_SCRIPT
            _tessera_si_late=1
            ;;
    esac
    builtin unset _tessera_si_inject
fi

if [[ $- == *i* && -n "${TESSERA_SHELL_INTEGRATION-}" && "${TESSERA_SHELL_INTEGRATION-}" != 0 && -z "${_tessera_si_loaded-}" ]]; then
    _tessera_si_loaded=1
    _tessera_si_words=" $TESSERA_SHELL_INTEGRATION "
    builtin unset TESSERA_SHELL_INTEGRATION

    # 0: no mark written yet (no D before the first prompt); 1: a C is
    # open; 2: a prompt is showing and no command ran since.
    _tessera_si_state=0
    # Set by _tessera_si_ret: the next command the DEBUG trap sees is the
    # user's. _tessera_si_disarm: that command has started.
    _tessera_si_armed=
    _tessera_si_disarm=
    _tessera_si_status=0
    # $? and $_ as the DEBUG trap found them.
    _tessera_si_saved=(0 "")
    _tessera_si_elements=1
    _tessera_si_last_pwd=
    _tessera_si_mark_a='\[\033]133;A\007\]'
    _tessera_si_mark_b='\[\033]133;B\007\]'

    # OSC 7: `file://<host><path>`, the path percent-encoded byte by byte
    # with Terminal.app's rule (every byte outside [/._~A-Za-z0-9-] becomes
    # %XX), the host from $HOSTNAME reduced to [A-Za-z0-9._-]. Sent only
    # when $PWD changed.
    _tessera_si_report_pwd() {
        [[ "$PWD" == "$_tessera_si_last_pwd" ]] && builtin return 0
        _tessera_si_last_pwd=$PWD
        builtin local LC_ALL=C LC_CTYPE=C LC_COLLATE=C
        # bash 3.2 word-splits unquoted expansions in `local` arguments.
        builtin local host="${HOSTNAME//[!A-Za-z0-9._-]/}" url="$PWD" ch hex i
        [[ -n "$host" ]] || host=localhost
        case $url in
            *[!/._~A-Za-z0-9-]*)
                url=
                for (( i = 0; i < ${#PWD}; i++ )); do
                    ch=${PWD:i:1}
                    case $ch in
                        [/._~A-Za-z0-9-]) url+=$ch ;;
                        *)
                            builtin printf -v hex '%02X' "'$ch"
                            # bash 3.2 sign-extends bytes >= 0x80.
                            url+=%${hex: -2}
                            ;;
                    esac
                done
                ;;
        esac
        builtin printf '\033]7;file://%s%s\007' "$host" "$url"
    }

    # First PROMPT_COMMAND entry: close the command and report the
    # directory. Keeps $? (in _tessera_si_status, for _tessera_si_pass).
    _tessera_si_precmd() {
        # $? is the status of the user's command (the first thing run).
        # shellcheck disable=SC2319
        builtin local status=$?
        _tessera_si_status=$status
        # bash 5.1+ runs a copy of an array PROMPT_COMMAND taken now: elements
        # after ours (added at a prompt; _tessera_si_prompt joins them for
        # the next prompt) run after the tail this time.
        _tessera_si_elements=1
        if [[ -n "$_tessera_si_arrays" && -n "${PROMPT_COMMAND[1]+x}" ]]; then
            _tessera_si_elements=2
        fi
        # The head installs the trap next: the last command's disarm is done.
        _tessera_si_disarm=
        _tessera_si_armed=
        case $_tessera_si_words in *" no-prompt-mark "*) ;; *)
            if [[ $_tessera_si_state == 1 ]]; then
                builtin printf '\033]133;D;%s\007' "$status"
            elif [[ $_tessera_si_state == 2 ]]; then
                # A prompt was shown and no command ran (empty line, Ctrl-C).
                builtin printf '\033]133;D\007'
            fi
            _tessera_si_state=2
            ;;
        esac
        case $_tessera_si_words in *" no-cwd "*) ;; *) _tessera_si_report_pwd ;; esac
        builtin return $status
    }

    # Next to last PROMPT_COMMAND entry: put the marks into PS1 (after any
    # entry that rebuilds it). Keeps $? (for PS1) in _tessera_si_status.
    # Third PROMPT_COMMAND entry, after the head installed the trap: give
    # the user's entries the status of the user's command.
    _tessera_si_pass() {
        builtin return "$_tessera_si_status"
    }

    # Next to last PROMPT_COMMAND entry: put the marks into PS1 (after any
    # entry that rebuilds it).
    _tessera_si_prompt() {
        builtin local status=$?
        case $_tessera_si_words in *" no-prompt-mark "*) ;; *)
            case $PS1 in *"$_tessera_si_mark_a"*) ;; *) PS1=$_tessera_si_mark_a$PS1 ;; esac
            case $PS1 in
                *"$_tessera_si_mark_b") ;;
                *) PS1=${PS1//"$_tessera_si_mark_b"/}$_tessera_si_mark_b ;;
            esac
            ;;
        esac
        # Keep the head first and the tail last (an entry added after the
        # tail would run with the trap armed); takes effect from the next
        # prompt.
        if [[ -z "$_tessera_si_bp" ]]; then
            _tessera_si_join_prompt_command
            case $PROMPT_COMMAND in
                "$_tessera_si_head"*) ;;
                *) PROMPT_COMMAND=$_tessera_si_head${PROMPT_COMMAND//"$_tessera_si_head"/} ;;
            esac
            case $PROMPT_COMMAND in
                *"$_tessera_si_tail") ;;
                *) PROMPT_COMMAND=${PROMPT_COMMAND//"$_tessera_si_tail"/}$_tessera_si_tail ;;
            esac
        fi
        builtin return "$status"
    }

    # Last PROMPT_COMMAND entry: arm the trap (only now, so our and the
    # user's entries are not taken for the user's command).
    _tessera_si_ret() {
        # Not when more elements run after this one (see _tessera_si_precmd):
        # their commands would be taken for the user's. The next command
        # then has no C and ends with a bare D, once.
        if (( _tessera_si_elements <= 1 )); then
            _tessera_si_armed=1
        fi
        builtin return "$_tessera_si_status"
    }

    # The user's command is starting (C). Called from the DEBUG trap, and
    # from bash-preexec's preexec_functions.
    _tessera_si_preexec() {
        _tessera_si_armed=
        case $_tessera_si_words in *" no-prompt-mark "*) builtin return 0 ;; esac
        # No `cmdline=`: the command line is the user's data (ADR 0021).
        builtin printf '\033]133;C\007'
        _tessera_si_state=1
    }

    # In the trap text, before the trap that was there before ours: sets $?
    # and (through its argument) $_ back to what they were when the trap
    # started.
    _tessera_si_restore() {
        builtin return "${_tessera_si_saved[0]}"
    }

    # Last command of the trap text: keeps the status of the command before
    # it (0, or that of the trap before ours) and sets $_ back (through its
    # argument).
    _tessera_si_keep() {
        builtin return $?
    }

    _tessera_si_debug() {
        # Our own functions (with `set -T` the trap fires inside them).
        case ${FUNCNAME[1]-} in _tessera_si_*) builtin return 0 ;; esac
        case $BASH_COMMAND in _tessera_si_*) builtin return 0 ;; esac
        _tessera_si_preexec
        # The trap text then removes the trap (a trap set inside a function
        # is put back when it returns).
        _tessera_si_disarm=1
    }

    _tessera_si_bp=
    if [[ -n "${bash_preexec_imported-}${__bp_imported-}" ]]; then
        # bash-preexec owns the DEBUG trap and PROMPT_COMMAND; use its hooks.
        _tessera_si_bp=1
        precmd_functions+=(_tessera_si_precmd _tessera_si_prompt)
        preexec_functions+=(_tessera_si_preexec)
    else
        # The DEBUG trap fires before every simple command, so ours is
        # there only from the start of PROMPT_COMMAND (the head installs it,
        # at top level: a trap set in a function is put back when it
        # returns) to the start of the user's command. Then it marks C and
        # replaces itself with the trap that was there before ours (or
        # none), which it runs for that command too. That trap never runs
        # for PROMPT_COMMAND's entries after the head: a prompt framework's
        # preexec (starship's) armed by its precmd entry would otherwise
        # start at our tail instead of at the user's command. Unarmed, ours
        # only saves and restores $? and $_. While the command runs, ours
        # costs nothing.
        _tessera_si_prev_trap=$(builtin trap -p DEBUG)
        _tessera_si_prev_trap=${_tessera_si_prev_trap#"trap -- '"}
        _tessera_si_prev_trap=${_tessera_si_prev_trap%"' DEBUG"}
        _tessera_si_prev_trap=${_tessera_si_prev_trap//"'\\''"/"'"}
        if [[ -n "$_tessera_si_prev_trap" ]]; then
            # shellcheck disable=SC2016
            _tessera_si_untrap='builtin trap -- "$_tessera_si_prev_trap" DEBUG'
        else
            _tessera_si_untrap='builtin trap - DEBUG'
        fi
        # The trap's own commands would change `$_` (the last argument of
        # the previous command) and, for the trap before ours, `$?`: it
        # saves both first, restores them before that trap, and ends with
        # _tessera_si_keep, whose argument `$_` becomes. Its status is 0
        # unless the trap before ours decided otherwise: under `extdebug` a
        # DEBUG trap returning non-zero skips the command (2: `return`).
        # shellcheck disable=SC2016
        _tessera_si_trap='_tessera_si_saved=("$?" "$_")
${_tessera_si_armed:+_tessera_si_debug}
${_tessera_si_disarm:+builtin eval "$_tessera_si_untrap"}'
        if [[ -n "$_tessera_si_prev_trap" ]]; then
            # shellcheck disable=SC2016
            _tessera_si_trap+='
_tessera_si_restore "${_tessera_si_saved[1]}"
${_tessera_si_disarm:+builtin eval "$_tessera_si_prev_trap"}'
        fi
        # shellcheck disable=SC2016
        _tessera_si_trap+='
_tessera_si_keep "${_tessera_si_saved[1]}"'
        # shellcheck disable=SC2016
        _tessera_si_head=$'_tessera_si_precmd\nbuiltin trap -- "$_tessera_si_trap" DEBUG\n_tessera_si_pass\n'
        _tessera_si_tail=$'\n_tessera_si_prompt\n_tessera_si_ret'
        # Newline-separated: valid whatever the user's PROMPT_COMMAND ends
        # with (`;`, `&`, a newline or nothing).
        _tessera_si_join_prompt_command
        PROMPT_COMMAND=$_tessera_si_head"${PROMPT_COMMAND-}$_tessera_si_tail"
        # Child shells have none of these functions: an exported
        # PROMPT_COMMAND would print "command not found" there at every
        # prompt and replace their DEBUG trap.
        builtin export -n PROMPT_COMMAND
    fi

    builtin printf '\033]1337;ShellIntegrationVersion=1;shell=bash\007'

    # Loaded from PROMPT_COMMAND: this prompt's hooks have been skipped.
    # Nothing may run at top level after the trap is armed. (bash 4 and
    # later put a DEBUG trap set in a sourced file back on return when one
    # was set before: then the first command has no C.)
    if [[ -n "${_tessera_si_late-}" ]]; then
        builtin unset _tessera_si_late
        _tessera_si_precmd
        _tessera_si_elements=${_tessera_si_late_elements-1}
        builtin unset _tessera_si_late_elements
        if [[ -z "$_tessera_si_bp" ]]; then
            builtin trap -- "$_tessera_si_trap" DEBUG
        fi
        _tessera_si_prompt
        [[ -n "$_tessera_si_bp" ]] || _tessera_si_ret
    fi
else
    builtin unset _tessera_si_late _tessera_si_late_elements
fi
