# Tessera shell integration for bash (3.2+, the macOS /bin/bash included):
# OSC 133 prompt marks, OSC 7 working directory reports and a one-time
# OSC 1337 handshake.
#
# Tessera loads this file into the bash it starts
# (docs/adr/0021-shell-integration-injection.md):
# - a non-login interactive bash starts as `bash --rcfile <this file>`, and
#   this file then sources ~/.bashrc (or the file the user gave to
#   --rcfile);
# - a login bash (and a --posix or --norc one) gets an exported
#   PROMPT_COMMAND loader that sources this file at the first prompt, after
#   every startup file ran:
#     builtin [ -n "${_tessera_si_loader-}" ] ||
#         _tessera_si_load='<words>' . '<this file>' 2>/dev/null   (on one line)
#   The feature words and the path are literal, so no variable of ours is in
#   the environment. The guard makes the loader a no-op once this file ran
#   (a prompt tool that keeps a copy of it, as starship 1.27 does by moving
#   PROMPT_COMMAND into STARSHIP_PROMPT_COMMAND, runs a cheap test), and the
#   redirection keeps it silent when this file is gone (`exec tmux` in a
#   startup file leaves the loader in the tmux server's environment, for
#   shells started after an update removed this version's scripts). Tessera
#   does not inject a login bash whose startup files replace PROMPT_COMMAND
#   (inject.rs reads them first).
# To load it in other shells, add to the end of ~/.bashrc:
#
#   export TESSERA_SHELL_INTEGRATION=enabled
#   source /path/to/shell-integration/bash/tessera.bash
#
# TESSERA_SHELL_INTEGRATION holds space-separated feature words: `enabled`
# (or anything else) turns it on, `0` (or unset) turns it off, `no-cwd`
# drops the OSC 7 reports and `no-prompt-mark` the OSC 133 marks. It is
# unset once read, so shells started from this one are not integrated twice.
# A startup file's own value wins over the words Tessera passes.
#
# Hooks: PROMPT_COMMAND entries close the last command (D), put the prompt
# marks into PS1 (A before it, B after it) and install a DEBUG trap that
# marks the start of the next command (C) and then removes itself, so it
# costs nothing while commands run; it keeps $? and $_ as it found them.
# The marks go to standard output as it is at the prompt (the session's
# stream, also when that is a pipe to a logger such as `exec > >(tee log)`),
# never into a command's redirected output (`{ ...; } > f` runs the trap
# with f as standard output): the C mark goes to copies of standard output
# taken at the prompt, closed before the command runs.
# The hooks run no ERR trap and do not exit a `set -e` shell, whatever IFS
# holds. A DEBUG trap of the user's keeps running (after ours, and alone
# while a command runs), whenever it was set: before or after this script
# loaded, or at a prompt (it is read again at every prompt, a fork that
# costs about 0.5 ms of CPU on bash 3.2 and 0.7 ms on 5.3). With
# bash-preexec loaded, its hooks are used instead (our D in
# precmd_functions, our C in preexec_functions, the prompt marks as the last
# PROMPT_COMMAND entry before bash-preexec's closing one, so a theme that
# rebuilds PS1 in an entry keeps them). Once installed, PROMPT_COMMAND is no
# longer exported (child shells have none of our functions). PROMPT_COMMAND
# stays a string (entries the user appends with `PROMPT_COMMAND=
# "$PROMPT_COMMAND; f"` keep working), its entries joined between our head
# and tail; an array (bash 5.1+) is not integrated (below). A C is only sent
# for a prompt that shows our marks, and a bare D only after one. Limits:
# - a PROMPT_COMMAND entry added after ours (`PROMPT_COMMAND+="; cmd"`)
#   runs after the prompt marks for one prompt, seeing the command's $?
#   but not its PIPESTATUS (ours moves back to the end at the next, and
#   the entry then sees both); the trap is then armed by PS1's expansion instead (not
#   with `shopt -u promptvars`, or when that entry rebuilds PS1: then that
#   command has no C and no D); an entry put before ours (`PROMPT_COMMAND=
#   "cmd; $PROMPT_COMMAND"`) runs before our head reads $?, so that command
#   ends with a bare D (our head is moved back to the start at the next);
# - bash 5.1+ with an array PROMPT_COMMAND (bash-preexec 0.6 and later,
#   oh-my-bash) is not integrated: at load nothing is installed; turned into
#   one at a prompt, the next prompt takes ours out (_tessera_si_opt_out). A
#   whole new array, like a whole new string, leaves nothing of ours to run:
#   PS1 keeps its A and B marks, and no C or D is sent;
# - a DEBUG trap set before this script loaded, in a login shell that loads
#   it from PROMPT_COMMAND, is lost on bash 5.3 (bash hides it while it
#   sources a file, and 5.3 keeps the trap the file set) and the first
#   command has no C on 3.2 (which puts the hidden one back);
# - with `set -T` or `shopt -s extdebug` a DEBUG trap that writes to
#   descriptor 3 writes into the hooks' read of the trap, so it is not
#   taken up (it stays as it was at the last prompt; a new one is dropped);
# - a command line that only runs a subshell, `(cd x; make)`, does not fire
#   the DEBUG trap: it ends with a bare D, no C (with `set -T` or `shopt -s
#   extdebug` the subshell inherits the trap and sends the C itself, still
#   followed by a bare D);
# - loaded by the PROMPT_COMMAND loader from inside another tool's hook, or
#   with PROMPT_COMMAND entries after the loader (starship's init wraps or
#   moves PROMPT_COMMAND and rewrites PS1 after the loader ran), the first prompt has no marks and the first command no C and
#   no D; from the second prompt on every prompt and command is marked;
# - a login shell whose startup files assign PROMPT_COMMAND (rather than
#   add to it) is started without the loader, so it is not integrated. An
#   assignment Tessera cannot see (behind a computed `source` path or in
#   `eval`) still drops the loader, and what it assigned stays exported, as
#   Tessera exported the variable: a child bash or sh then runs that text at
#   its prompts.
# - bash 3.2 resets PIPESTATUS in any DEBUG trap (bash 4 and later restore
#   it). Ours puts it back, with a pipeline of _tessera_si_rv (one fork per
#   element), before a command line whose text names PIPESTATUS and before
#   each command of the user's PROMPT_COMMAND entries; bash 4 and later
#   fork for a string PROMPT_COMMAND's entries only (the head's restore,
#   after a pipeline of 2 to 9 commands). Under
#   `set -T` on 3.2 the entries' restore also runs before each command
#   inside a user entry function (and twice for its call), never inside
#   our own functions. Not restored: under extdebug (the trap's status would
#   skip the command), with a DEBUG trap of the user's (3.2 loses it there
#   anyway), after more than 9 pipeline commands, for a command that reads
#   it without naming it (a function: `f` after `a | b`);
# - a DEBUG trap of the user's also runs for the hooks' first commands at
#   each prompt (until the head puts ours back);
# - on the login route an entry appended to PROMPT_COMMAND in a way that
#   only works after a newline and one that only works after the loader's
#   last word, in the same startup files, leave the shell to bash (no
#   single ending suits both), as do an array PROMPT_COMMAND (`+=(...)`,
#   oh-my-bash on 5.1+) and any subscripted assignment (an element at [1]
#   never runs on 3.2); the entry on the loading prompt sees the status the
#   loader left (0);
# - a command line that only runs a subshell also leaves the two copies
#   of standard output (descriptors from 59 down to 50) open in it, closed
#   at the next prompt, so its commands inherit them;
# - a compound command that redirects one of those descriptors
#   (`{ ...; } 59>f`) gets no C, and its commands inherit the other copy;
#   one that points both at the same file, or one at the file the other
#   is (`59>&1` at the terminal), gets C written to it and both closed;
# - bash-preexec 0.7.0 sourced at a prompt (after this script) sanitizes
#   PROMPT_COMMAND with an extglob loop whose cost grows steeply with the
#   text before its install `:` in the same string, with or without
#   Tessera (an 80-byte entry stalls bash 3.2 for about 8 s). Ours before
#   the user's entries is one line on 3.2 (71 bytes, under 1 s); on 5.1+
#   bash-preexec 0.7 adds its install string as an array element (the
#   shell then leaves the integration).
#
# bash 3.2 compatibility: no PS0, no associative arrays, no ${var,,}, no
# BASHPID; PROMPT_COMMAND arrays only on 5.1+. Run on 3.2.57 and 5.3.20.

# bash 5.1 and later run every element of an array PROMPT_COMMAND (earlier
# versions only the first). The integration takes a string only: a shell
# whose PROMPT_COMMAND is an array when this file loads, or turns into one
# later, is left as it is without Tessera (_tessera_si_off, ADR 0021, #613).
_tessera_si_arrays=
if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
    _tessera_si_arrays=1
fi
# Whether PROMPT_COMMAND is an array (bash 5.1+ only: `@a` and `local -`
# are 4.4 syntax). Under `set -u`, `@a` of an unset variable or an empty
# array is an unbound-variable error, so `set -u` is off in here.
_tessera_si_is_array() {
    builtin local -
    builtin set +u
    [[ ${PROMPT_COMMAND@a} == *a* ]]
}

# Startup files first: they run even when the integration is disabled.
if [[ -n "${_tessera_si_load+X}" ]]; then
    # Sourced by the PROMPT_COMMAND loader at the first prompt (its
    # `_tessera_si_load=<words>` holds for this `.` only). Take the loader
    # out again and keep PROMPT_COMMAND out of the environment of child
    # shells (which have no such functions). Elements after the loader's
    # still run at this prompt (bash runs a copy of the array): see
    # _tessera_si_ret.
    _tessera_si_injected=$_tessera_si_load
    builtin unset _tessera_si_load
    _tessera_si_loader=1
    _tessera_si_late_elements=1
    # The loader's text, as inject.rs writes it: the words and this file's
    # path (as `.` was given it) single-quoted, a `'` as `'\''`.
    _tessera_si_sq="'\\''"
    _tessera_si_q="'"
    # shellcheck disable=SC2016
    _tessera_si_text='builtin [ -n "${_tessera_si_loader-}" ] || _tessera_si_load='
    _tessera_si_text+="'${_tessera_si_injected//$_tessera_si_q/$_tessera_si_sq}' . "
    _tessera_si_text+="'${BASH_SOURCE[0]//$_tessera_si_q/$_tessera_si_sq}' 2>/dev/null"
    # Whether this prompt's hooks can run (and the trap be armed) from here:
    # sourced at top level (not from another tool's function, as starship
    # 1.27 runs it) and with nothing after the loader in PROMPT_COMMAND
    # (starship master puts its PS1 rewrite there). Otherwise the first
    # command has no C, and no D either.
    _tessera_si_can_arm=
    case $PROMPT_COMMAND in
        # FUNCNAME: empty at top level on 3.2, `source` alone on 5 (not
        # `${#FUNCNAME[@]}`: an error under `set -u` on 3.2 when empty).
        # Tessera ends the loader with a newline when a startup file appends
        # without a separator (`PROMPT_COMMAND+='cmd'`, which would otherwise
        # glue `cmd` to the redirection), and with nothing when every append
        # starts with `;` (a newline would make that `;` a syntax error).
        *"$_tessera_si_text" | *"$_tessera_si_text"$'\n')
            [[ -n "${FUNCNAME[1]+x}" ]] || _tessera_si_can_arm=1
            ;;
    esac
    # The loader goes completely, with the separators that followed it, so
    # that the entries after it see the user's command's $? and PIPESTATUS
    # (a `:` left in its place reset them), and a leading `;`, `&&` or blank
    # left by `"${PROMPT_COMMAND:+$PROMPT_COMMAND;}cmd"` or
    # `"$PROMPT_COMMAND && cmd"` goes too (any `;`, `&` or `|` there would be
    # a syntax error at every later prompt). For a leading `||` that changes
    # what the user wrote: `"$PROMPT_COMMAND || cmd"` runs `cmd` at every
    # later prompt (without Tessera it is a syntax error at every prompt).
    _tessera_si_lead=$'\n; \t&|'
    case $PROMPT_COMMAND in
        *"$_tessera_si_text"*)
            _tessera_si_after=${PROMPT_COMMAND#*"$_tessera_si_text"}
            while [[ "$_tessera_si_after" == ["$_tessera_si_lead"]* ]]; do
                _tessera_si_after=${_tessera_si_after:1}
            done
            PROMPT_COMMAND=${PROMPT_COMMAND%%"$_tessera_si_text"*}$_tessera_si_after
            ;;
    esac
    while [[ "$PROMPT_COMMAND" == ["$_tessera_si_lead"]* ]]; do
        PROMPT_COMMAND=${PROMPT_COMMAND:1}
    done
    while [[ "$PROMPT_COMMAND" == *[$'\n \t'] ]]; do
        PROMPT_COMMAND=${PROMPT_COMMAND%?}
    done
    # An array (bash 5.1+, not integrated): the loader was its first
    # element alone, so that element goes.
    if [[ -n "$_tessera_si_arrays" && -z "$PROMPT_COMMAND" && -n "${PROMPT_COMMAND[1]+x}" ]]; then
        PROMPT_COMMAND=("${PROMPT_COMMAND[@]:1}")
    fi
    builtin unset _tessera_si_sq _tessera_si_q _tessera_si_text _tessera_si_lead _tessera_si_after
    builtin export -n PROMPT_COMMAND
    # A startup file's TESSERA_SHELL_INTEGRATION wins (and one that loaded
    # this script already is not loaded again).
    if [[ -z "${TESSERA_SHELL_INTEGRATION+X}" && -z "${_tessera_si_loaded-}" ]]; then
        TESSERA_SHELL_INTEGRATION=$_tessera_si_injected
    fi
    builtin unset _tessera_si_injected
    _tessera_si_late=1
elif [[ -n "${TESSERA_BASH_INJECT-}" ]]; then
    builtin unset TESSERA_BASH_INJECT
    # Started as `bash --rcfile <this file>` (never a login shell): read
    # what bash would have read instead of us. The feature words stay out
    # of the environment of what the startup files run (a startup file may
    # still set its own).
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
    # Not exported: read below, then unset (unless a startup file loaded
    # this script already).
    if [[ -z "${TESSERA_SHELL_INTEGRATION+X}" && -n "${_tessera_si_injected+X}" &&
        -z "${_tessera_si_loaded-}" ]]; then
        TESSERA_SHELL_INTEGRATION=$_tessera_si_injected
    fi
    builtin unset _tessera_si_injected
fi

# An array PROMPT_COMMAND is not integrated: the shell runs as without
# Tessera (see the top of this file).
if [[ -n "$_tessera_si_arrays" ]] && _tessera_si_is_array; then
    _tessera_si_off=array
    builtin unset TESSERA_SHELL_INTEGRATION
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
    _tessera_si_mark_a='\[\033]133;A\007\]'
    _tessera_si_mark_b='\[\033]133;B\007\]'
    # The B mark that also arms the trap when PS1 is expanded, after every
    # PROMPT_COMMAND entry ran (an arithmetic subscript of an unset array
    # expands to nothing): used when text follows our tail (see
    # _tessera_si_prompt).
    # shellcheck disable=SC2016
    _tessera_si_mark_b_arm='\[${_tessera_si_none[_tessera_si_armed=1]-}\033]133;B\007\]'

    # The C mark goes to standard output as it was at the prompt (the
    # session's stream: the terminal, or a pipe such as `exec > >(tee log)`
    # that C must stay in order in), never into a command's redirected
    # output (the DEBUG trap fires inside `{ ...; } > f`, with f as
    # standard output). _tessera_si_prompt keeps two copies of standard
    # output on descriptors of ours, _tessera_si_out and _tessera_si_out2:
    # the first two of 59 down to 50 that are not open (bash keeps 63 and
    # down for process substitutions and 255 for the terminal; no
    # `{var}>&1`, which 3.2 lacks and which picks 10 or above, the numbers
    # users redirect). A compound command can still redirect one of them
    # (`{ ...; } 59>f`) before the trap runs: preexec writes C and closes
    # both (bash cannot make them close-on-exec) only while they are still
    # the same file; otherwise it writes no C and leaves the user's
    # descriptor alone. Plain `exec`: 3.2 drops the redirections of
    # `builtin exec` when it returns.
    _tessera_si_out=
    _tessera_si_out2=
    _tessera_si_close_out() {
        if [[ -n "$_tessera_si_out" ]]; then
            builtin eval "exec $_tessera_si_out>&- $_tessera_si_out2>&-"
            _tessera_si_out=
            _tessera_si_out2=
        fi
    }
    _tessera_si_open_out() {
        _tessera_si_close_out
        builtin local fd first=
        for fd in 59 58 57 56 55 54 53 52 51 50; do
            [[ -e /dev/fd/$fd ]] && continue
            { builtin eval "exec $fd>&1"; } 2>/dev/null || continue
            if [[ -z "$first" ]]; then
                first=$fd
            else
                _tessera_si_out=$first
                _tessera_si_out2=$fd
                builtin return 0
            fi
        done
        # Fewer than two free: none (C then goes to standard output).
        [[ -n "$first" ]] && builtin eval "exec $first>&-"
        builtin return 0
    }

    # OSC 7: `file://<host><path>`, the path percent-encoded byte by byte
    # with Terminal.app's rule (every byte outside [/._~A-Za-z0-9-] becomes
    # %XX), the host from $HOSTNAME reduced to [A-Za-z0-9._-]. Sent at
    # every prompt (as zsh and fish do): a program run from the shell, ssh
    # say, may have reported another directory, or another host, meanwhile.
    # Tessera drops a payload over 4096 bytes (MAX_OSC7_LEN) whole, which
    # would keep the previous report in effect, so a longer one names the
    # deepest ancestor directory that fits.
    # The encoded text is kept for one directory and host (the last ones):
    # the loop is slow on bash 3.2, and most prompts are in the directory
    # of the one before. _tessera_si_encodes counts the encodings (tests).
    _tessera_si_osc7_pwd=
    _tessera_si_osc7_host=
    _tessera_si_osc7=
    _tessera_si_encodes=0
    _tessera_si_report_pwd() {
        if [[ -z "$_tessera_si_osc7" || "${PWD-}" != "$_tessera_si_osc7_pwd" ||
            "${HOSTNAME-}" != "$_tessera_si_osc7_host" ]]; then
            _tessera_si_encode_pwd
        fi
        builtin printf '\033]7;%s\007' "$_tessera_si_osc7"
    }
    _tessera_si_encode_pwd() {
        builtin local LC_ALL=C LC_CTYPE=C LC_COLLATE=C
        # bash 3.2 word-splits unquoted expansions in `local` arguments.
        builtin local host="${HOSTNAME-}" url="${PWD-}" ch hex i
        host=${host//[!A-Za-z0-9._-]/}
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
        # `file://` is 7 bytes; cut at the last `/` that leaves the rest
        # within the cap (the encoding keeps `/` as is, so no %XX is split).
        i=$(( 4096 - 7 - ${#host} ))
        if (( i > 0 && ${#url} > i )); then
            url=${url:0:i+1}
            url=${url%/*}
            [[ -n "$url" ]] || url=/
        fi
        _tessera_si_osc7_pwd=${PWD-}
        _tessera_si_osc7_host=${HOSTNAME-}
        _tessera_si_osc7=file://$host$url
        _tessera_si_encodes=$(( _tessera_si_encodes + 1 ))
    }

    # Whether the prompt on screen has our marks: PS1 is still what was
    # displayed when a command starts, and when the next prompt's first
    # entry runs. An entry that ran after ours may have rebuilt it.
    _tessera_si_marked() {
        case ${PS1-} in
            *"$_tessera_si_mark_b"* | *"$_tessera_si_mark_b_arm"*) builtin return 0 ;;
        esac
        builtin return 1
    }

    # With bash-preexec, called from its precmd at every prompt: keeps our
    # entry the last one before its closing `__bp_interactive_mode`, after
    # any entry or array element that rebuilds PS1. Takes effect from the
    # next prompt.
    _tessera_si_bp_place() {
        builtin local entry=$'\n_tessera_si_prompt && _tessera_si_nop' last=$'\n__bp_interactive_mode' text
        # An array (bash-preexec 0.6 and later on bash 5.1+ make one): our
        # entry takes the integration out (_tessera_si_opt_out).
        if [[ -n "$_tessera_si_arrays" ]] && _tessera_si_is_array; then
            builtin return 0
        fi
        # shellcheck disable=SC2128,SC2178 # a string here, never an array
        case ${PROMPT_COMMAND-} in *"$entry$last") builtin return 0 ;; esac
        text=$'\n'${PROMPT_COMMAND-}
        text=${text//"$entry"/}
        case $text in
            *"$last") text=${text%"$last"}$entry$last ;;
            *) text+=$entry ;;
        esac
        PROMPT_COMMAND=${text#$'\n'}
    }

    # First PROMPT_COMMAND entry: close the command and report the
    # directory. Keeps $? (in _tessera_si_status, for _tessera_si_pass).
    _tessera_si_precmd() {
        # $? and PIPESTATUS are the user's command's (the first thing run;
        # bash keeps PIPESTATUS at a function's entry). When the trap is
        # still armed (an empty line, Ctrl-C, a line that only runs a
        # subshell) it fired just before this entry and reset PIPESTATUS on
        # bash 3.2, so its first statement's copy is the user's.
        # shellcheck disable=SC2319
        _tessera_si_ps=("$?" "${PIPESTATUS[@]}")
        if [[ -n "$_tessera_si_armed" && -n "${_tessera_si_saved[2]+x}" ]]; then
            _tessera_si_ps=("${_tessera_si_saved[0]}" "${_tessera_si_saved[@]:2}")
        fi
        builtin local status=${_tessera_si_ps[0]}
        _tessera_si_status=$status
        _tessera_si_dopipe=
        _tessera_si_headran=1
        _tessera_si_elements=1
        # The head ran first, so $? is the user's command's: not when an
        # entry was put before it (`PROMPT_COMMAND="cmd; $PROMPT_COMMAND"`;
        # the tail puts the head first again for the next prompt). With
        # bash-preexec (its prologue keeps $? for its precmd functions and
        # the entries after it) its prologue must be first: an entry put
        # before that gives a bare D at every prompt, since the string is
        # left as it is (see _tessera_si_prompt).
        _tessera_si_first=
        # shellcheck disable=SC2128 # a string, or an array taken out at the tail
        case ${PROMPT_COMMAND-} in
            __bp_precmd_invoke_cmd*) _tessera_si_first=1 ;;
            "$_tessera_si_head"*) [[ -n "$_tessera_si_bp" ]] || _tessera_si_first=1 ;;
        esac
        # The head installs the trap next: the last command's disarm is done.
        # A prompt with no command since the last (empty line, Ctrl-C, a
        # line that only runs a subshell), or a command that redirected a
        # copy, left the copies open: none outlives a prompt.
        _tessera_si_close_out
        _tessera_si_disarm=
        _tessera_si_armed=
        case $_tessera_si_words in *" no-prompt-mark "*) ;; *)
            if [[ $_tessera_si_state == 1 && -z "$_tessera_si_first" ]]; then
                builtin printf '\033]133;D\007'
            elif [[ $_tessera_si_state == 1 ]]; then
                builtin printf '\033]133;D;%s\007' "$status"
            elif [[ $_tessera_si_state == 2 ]] && _tessera_si_marked; then
                # A prompt was shown and no command ran (empty line, Ctrl-C).
                builtin printf '\033]133;D\007'
            fi
            _tessera_si_state=2
            ;;
        esac
        case $_tessera_si_words in *" no-cwd "*) ;; *) _tessera_si_report_pwd ;; esac
        if [[ -n "$_tessera_si_bp" ]]; then
            _tessera_si_bp_place
            # Run by bash-preexec's install entry, the last to run at the
            # first prompt when it is the last entry.
            if [[ -n "${_tessera_si_bp_first-}" ]]; then
                builtin unset _tessera_si_bp_first
                _tessera_si_prompt && _tessera_si_nop
            fi
        fi
        # 0: the head's `&&` list goes on to its eval (which ends with
        # the user's status, see _tessera_si_h).
        builtin return 0
    }

    # Fourth PROMPT_COMMAND entry, after the head installed the trap: take
    # the DEBUG trap the head found in its place (_tessera_si_found, in
    # `trap -p` form) for the user's, unless it was ours (no command fired
    # it since the last prompt: an empty line, Ctrl-C, a line that only
    # runs a subshell), and give the user's entries the status of the
    # user's command.
    # Only exactly `trap -- '<text>' DEBUG` with no quote left in <text>
    # once every `'\''` is gone is a trap (the head's read keeps what a
    # trap prints out of it; anything printed before it would leave the
    # opening quote bare). Anything else is never installed or run (it may
    # hold a directory's name); the trap stays as it was.
    _tessera_si_pass() {
        builtin local found="$_tessera_si_found"
        # PIPESTATUS for the user's entries, which the head's last command
        # leaves (see _tessera_si_cases): only with entries, after a
        # pipeline of 2 to 9 commands whose status is the one a pipeline
        # of the same statuses returns (not after `!`).
        _tessera_si_epipe=
        if [[ -n "$_tessera_si_entries" ]]; then
            _tessera_si_pipe_code
            # (On bash 4 and later the head's `case` has pipelines of up
            # to _tessera_si_head_max commands.)
            if [[ -n "$_tessera_si_code" && "$_tessera_si_code_status" == "${_tessera_si_ps[0]}" ]] &&
                { [[ -n "$_tessera_si_fixps" || "$_tessera_si_code" == o ]] ||
                    (( ${_tessera_si_code:1} <= _tessera_si_head_max )); }; then
                _tessera_si_epipe=$_tessera_si_code
                _tessera_si_restores=$(( _tessera_si_restores + 1 ))
                # bash 3.2 resets it again in our trap before each entry's
                # command: the trap puts it back there too (not under
                # extdebug, see _tessera_si_debug).
                if [[ -n "$_tessera_si_fixps" ]] && ! builtin shopt -q extdebug; then
                    _tessera_si_dopipe=$_tessera_si_code
                fi
            fi
        fi
        case $found in
            "") ;;
            "trap -- '"*"' DEBUG")
                found=${found#"trap -- '"}
                found=${found%"' DEBUG"}
                if [[ "${found//"'\\''"/}" == *"'"* ]]; then
                    found=$_tessera_si_trap
                else
                    found=${found//"'\\''"/"'"}
                fi
                ;;
            *) found=$_tessera_si_trap ;;
        esac
        if [[ "$found" != "$_tessera_si_trap" ]]; then
            _tessera_si_prev_trap=$found
            if [[ -n "$found" ]]; then
                # It runs (for the user's command) with $? and $_ as they
                # were when the trap started, as the first command of an
                # `&&` list: a non-zero status it ends with (a false `&&`
                # list in it) is not made a failure by the hooks (they run
                # no ERR trap and do not exit a `set -e` shell) and is
                # still the list's. A command failing inside the trap runs
                # ERR as in a plain bash, and more often: the trap also
                # runs during the prompt hooks.
                # shellcheck disable=SC2016
                _tessera_si_untrap='builtin trap -- "$_tessera_si_prev_trap" DEBUG
_tessera_si_restore "${_tessera_si_saved[1]}" && _tessera_si_nop "${_tessera_si_saved[1]}"
builtin eval "$_tessera_si_prev_trap" && _tessera_si_nop'
            else
                _tessera_si_untrap='builtin trap - DEBUG'
            fi
        fi
        builtin return "$_tessera_si_status"
    }

    # This prompt's command gets its C from PS1's expansion, not from
    # _tessera_si_ret (which does not arm the trap when something else may
    # run after it, see _tessera_si_elements): not with `shopt -u
    # promptvars` (that command then has no C and ends with a bare D).
    _tessera_si_arm_by_ps1() {
        case ${PS1-} in
            *"$_tessera_si_mark_b")
                builtin shopt -q promptvars &&
                    PS1=${PS1%"$_tessera_si_mark_b"}$_tessera_si_mark_b_arm
                ;;
        esac
        builtin return 0
    }

    # From the tail: whether the head ran at this prompt. When it did not
    # (the user's command replaced or removed it; the tail puts it back),
    # $? and the statuses saved are not the last command's: that command
    # ends with a bare D (none if it had no C), never a status.
    _tessera_si_headless() {
        if [[ -z "$_tessera_si_headran" ]]; then
            if [[ $_tessera_si_state == 1 ]]; then
                case $_tessera_si_words in *" no-prompt-mark "*) ;; *)
                    builtin printf '\033]133;D\007'
                    ;;
                esac
                _tessera_si_state=2
            fi
            _tessera_si_elements=2
        fi
        _tessera_si_headran=
        builtin return 0
    }

    # PROMPT_COMMAND turned into an array (bash 5.1+): take the
    # integration out, so the shell runs as without Tessera from the next
    # command on. Our text leaves every element, the marks leave PS1,
    # bash-preexec gets back its own functions only, and our trap (the head
    # installed it at this prompt) puts the user's back when it fires next.
    # Nothing of ours runs at later prompts.
    _tessera_si_opt_out() {
        builtin local i e
        _tessera_si_off=array
        for i in "${!PROMPT_COMMAND[@]}"; do
            e=${PROMPT_COMMAND[i]}
            e=${e//"$_tessera_si_head"/}
            e=${e//"$_tessera_si_tail"/}
            e=${e//$'\n_tessera_si_prompt && _tessera_si_nop'/}
            e=${e//'_tessera_si_prompt && _tessera_si_nop'/}
            PROMPT_COMMAND[i]=$e
        done
        PS1=${PS1-}
        PS1=${PS1//"$_tessera_si_mark_a"/}
        PS1=${PS1//"$_tessera_si_mark_b_arm"/}
        PS1=${PS1//"$_tessera_si_mark_b"/}
        if [[ -n "$_tessera_si_bp" ]]; then
            for i in "${!precmd_functions[@]}"; do
                [[ "${precmd_functions[i]}" == _tessera_si_precmd ]] && builtin unset 'precmd_functions[i]'
            done
            for i in "${!preexec_functions[@]}"; do
                [[ "${preexec_functions[i]}" == _tessera_si_preexec ]] && builtin unset 'preexec_functions[i]'
            done
        fi
        _tessera_si_elements=2
        _tessera_si_armed=
        _tessera_si_disarm=1
        _tessera_si_state=0
        builtin return 0
    }

    # Next to last PROMPT_COMMAND entry: put the marks into PS1 (after any
    # entry that rebuilds it).
    _tessera_si_prompt() {
        builtin local status=$?
        if [[ -n "$_tessera_si_arrays" ]] && _tessera_si_is_array; then
            _tessera_si_opt_out
            builtin return "$status"
        fi
        case $_tessera_si_words in *" no-prompt-mark "*) ;; *)
            # An unset PS1 (`set -u`) gets the marks alone.
            PS1=${PS1-}
            case $PS1 in *"$_tessera_si_mark_b_arm"*) PS1=${PS1//"$_tessera_si_mark_b_arm"/} ;; esac
            _tessera_si_open_out
            case $PS1 in *"$_tessera_si_mark_a"*) ;; *) PS1=$_tessera_si_mark_a$PS1 ;; esac
            case $PS1 in
                *"$_tessera_si_mark_b") ;;
                *) PS1=${PS1//"$_tessera_si_mark_b"/}$_tessera_si_mark_b ;;
            esac
            ;;
        esac
        # Keep the head first and the tail last; takes effect from the next
        # prompt. Text added after the tail (`PROMPT_COMMAND+="; cmd"`) runs
        # after _tessera_si_ret this time: _tessera_si_ret does not arm the
        # trap then (it would take that text for the user's command), the B
        # mark in PS1 does (with `shopt -u promptvars`, or a PS1 rebuilt by
        # that text, nothing does: the next command has no C and ends with a
        # bare D, once). The text after the tail moves up to where the tail
        # was, without the `;`, `&`, `|`, newlines and blanks it starts with
        # (it may be `; cmd`), so it follows the head's last command and
        # sees the user's command's $? and PIPESTATUS (a `:` left in the
        # tail's place reset them); a leading `||` runs `cmd` at every
        # prompt, as for the loader.
        if [[ -n "$_tessera_si_bp" ]]; then
            :
        elif [[ "${PROMPT_COMMAND-}" == *__bp_precmd_invoke_cmd* ]]; then
            # bash-preexec sourced at a prompt (after this script) wrapped
            # the string in its own entries: left as it is, since every
            # change makes bash-preexec sanitize it again (seconds per
            # prompt on bash 3.2, see the limits). Its closing entry runs
            # after our tail, so PS1 arms the trap.
            _tessera_si_headless
            # shellcheck disable=SC2128 # a string here, never an array
            [[ "$PROMPT_COMMAND" == *"$_tessera_si_tail" ]] || _tessera_si_elements=2
            (( _tessera_si_elements > 1 )) && _tessera_si_arm_by_ps1
            _tessera_si_entries=1
        else
            _tessera_si_headless
            # PROMPT_COMMAND is a string on this branch.
            # shellcheck disable=SC2128,SC2178 # a string here, never an array
            case ${PROMPT_COMMAND-} in
                "$_tessera_si_head"*) ;;
                *)
                    PROMPT_COMMAND=${PROMPT_COMMAND-}
                    PROMPT_COMMAND=$_tessera_si_head${PROMPT_COMMAND//"$_tessera_si_head"/}
                    ;;
            esac
            # shellcheck disable=SC2128,SC2178 # a string here, never an array
            case ${PROMPT_COMMAND-} in
                *"$_tessera_si_tail") ;;
                *)
                    _tessera_si_elements=2
                    builtin local after lead=$'\n; \t&|'
                    after=${PROMPT_COMMAND#*"$_tessera_si_tail"}
                    after=${after//"$_tessera_si_tail"/}
                    while [[ "$after" == ["$lead"]* ]]; do
                        after=${after:1}
                    done
                    PROMPT_COMMAND=${PROMPT_COMMAND%%"$_tessera_si_tail"*}${after:+$'\n'$after}$_tessera_si_tail
                    ;;
            esac
            (( _tessera_si_elements > 1 )) && _tessera_si_arm_by_ps1
            _tessera_si_entries=
            # shellcheck disable=SC2128 # a string here, never an array
            [[ "${PROMPT_COMMAND-}" == "$_tessera_si_head$_tessera_si_tail" ]] || _tessera_si_entries=1
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
        # bash-preexec 0.5 and 0.6 sourced at a prompt append their install
        # text, which saves `trap -p DEBUG` as the trap to keep and runs it
        # from preexec: that must be the user's trap, not ours (ours would
        # replace bash-preexec's own at the first command, and its preexec
        # functions would never run again). Our trap puts the user's back
        # when it fires for that text (a function cannot: bash restores a
        # trap set in a function when it returns).
        # shellcheck disable=SC2016
        case ${PROMPT_COMMAND[*]-} in
            *'__bp_trap_string="$(trap -p DEBUG)"'*) _tessera_si_disarm=1 ;;
        esac
        builtin return "$_tessera_si_status"
    }

    # The user's command is starting (C). Called from the DEBUG trap, and
    # from bash-preexec's preexec_functions.
    _tessera_si_preexec() {
        _tessera_si_armed=
        case $_tessera_si_words in *" no-prompt-mark "*) builtin return 0 ;; esac
        # An entry that ran after ours rebuilt PS1: this prompt has no marks,
        # so its command gets none either.
        if ! _tessera_si_marked; then
            _tessera_si_state=0
            builtin return 0
        fi
        # No `cmdline=`: the command line is the user's data (ADR 0021).
        if [[ -z "$_tessera_si_out" ]]; then
            builtin printf '\033]133;C\007'
        elif [[ /dev/fd/$_tessera_si_out -ef /dev/fd/$_tessera_si_out2 ]]; then
            builtin printf '\033]133;C\007' >&"$_tessera_si_out"
            _tessera_si_close_out
        fi
        # Else the command redirects a copy: no C, and the copies stay open
        # for it (the next prompt closes them).
        _tessera_si_state=1
    }

    # In the trap text, before the trap that was there before ours: sets $?
    # and (through its argument) $_ back to what they were when the trap
    # started.
    _tessera_si_restore() {
        builtin return "${_tessera_si_saved[0]}"
    }

    # Second command of the `&&` lists that run our status-returning hooks
    # (see _tessera_si_head; in the trap text its argument sets $_ back).
    _tessera_si_nop() {
        builtin return 0
    }

    # The restoring pipelines' commands: each returns its argument.
    _tessera_si_rv() {
        builtin return "$1"
    }
    _tessera_si_ri() {
        builtin return "${_tessera_si_ps[$1]}"
    }

    # The selector for _tessera_si_cases from the statuses precmd saved:
    # `z<n>`/`n<n>` for a pipeline of n (2 to 9) commands that ends with
    # status 0 / not 0 (under pipefail, the last failing one's), `o` for
    # one failed command, else empty; _tessera_si_code_status is that
    # pipeline's status.
    _tessera_si_pipe_code() {
        _tessera_si_code=
        builtin local n=$(( ${#_tessera_si_ps[@]} - 1 )) i end
        end=${_tessera_si_ps[n]}
        if [[ -o pipefail ]]; then
            end=0
            for (( i = 1; i <= n; i++ )); do
                [[ "${_tessera_si_ps[i]}" == 0 ]] || end=${_tessera_si_ps[i]}
            done
        fi
        _tessera_si_code_status=$end
        if (( n == 1 )); then
            [[ "$end" == 0 ]] || _tessera_si_code=o
        elif (( n >= 2 && n <= 9 )); then
            if [[ "$end" == 0 ]]; then
                _tessera_si_code=z$n
            else
                _tessera_si_code=n$n
            fi
        fi
        builtin return 0
    }

    _tessera_si_debug() {
        # Our own functions (with `set -T` the trap fires inside them).
        case ${FUNCNAME[1]-} in _tessera_si_*) builtin return 0 ;; esac
        case $BASH_COMMAND in _tessera_si_*) builtin return 0 ;; esac
        # bash 3.2 resets PIPESTATUS in a DEBUG trap (4 and later keep it):
        # the trap text ends by putting the last line's back when this
        # command reads it (see _tessera_si_cases). Not with a trap of the
        # user's (bash 3.2 loses PIPESTATUS in it anyway) or extdebug (the
        # trap's status would skip the command).
        _tessera_si_dopipe=
        if [[ -n "$_tessera_si_fixps" && -z "$_tessera_si_prev_trap" &&
            "$BASH_COMMAND" == *PIPESTATUS* ]] && ! builtin shopt -q extdebug; then
            _tessera_si_pipe_code
            if [[ -n "$_tessera_si_code" ]]; then
                _tessera_si_dopipe=$_tessera_si_code
                _tessera_si_restores=$(( _tessera_si_restores + 1 ))
            fi
        fi
        _tessera_si_preexec
        # The trap text then removes the trap (a trap set inside a function
        # is put back when it returns).
        _tessera_si_disarm=1
    }

    # PIPESTATUS kept for the next line (see _tessera_si_debug) and for the
    # user's PROMPT_COMMAND entries (see _tessera_si_pass): the statuses
    # precmd saved, the selectors, whether the entries exist, and a count
    # of the restores (each forks one process per pipeline command; tests).
    _tessera_si_ps=(0 0)
    _tessera_si_code=
    _tessera_si_code_status=0
    _tessera_si_dopipe=
    _tessera_si_now=
    _tessera_si_kept=0
    _tessera_si_epipe=
    _tessera_si_entries=
    _tessera_si_restores=0
    _tessera_si_fixps=
    if (( BASH_VERSINFO[0] < 4 )); then
        _tessera_si_fixps=1
    fi
    # The longest pipeline whose PIPESTATUS the head restores for a string
    # PROMPT_COMMAND's entries on bash 4 and later (see _tessera_si_h).
    _tessera_si_head_max=9
    _tessera_si_head_el=
    _tessera_si_tail_el=
    # Set with a string PROMPT_COMMAND only; the opt-out reads them in
    # every mode (`set -u`).
    _tessera_si_head=
    _tessera_si_tail=
    # Set by the head at each prompt, cleared by the tail (see
    # _tessera_si_headless).
    _tessera_si_headran=
    _tessera_si_first=
    # Sets _tessera_si_cases_text to a `case` on the variable named $1 with
    # an arm per restoring pipeline, `z<n>`/`n<n>` for n of 2 to 9 commands
    # (at most 16 pipelines of 9: the text is bounded), and the arm $2. The
    # pipeline is never the last command of its list (`|| _tessera_si_nop`
    # when it ends with 0, `&& _tessera_si_nop` when not, so the nop never
    # runs): no ERR trap and no `set -e` exit, and it leaves PIPESTATUS.
    # Not `!` (bash 3.2 runs ERR for `! true`) and not `eval` (its own
    # status replaces PIPESTATUS).
    _tessera_si_cases() {
        builtin local text="case \$$1 in" n i pipe
        for (( n = 2; n <= 9; n++ )); do
            pipe=
            for (( i = 1; i <= n; i++ )); do
                pipe+="${pipe:+ | }_tessera_si_rv \"\${_tessera_si_ps[$i]}\""
            done
            text+=$'\n'"z$n) $pipe || _tessera_si_nop ;;"$'\n'"n$n) $pipe && _tessera_si_nop ;;"
        done
        _tessera_si_cases_text=$text$'\n'"$2"$'\n'"esac"
    }
    # The head's `case` on _tessera_si_epipe, as short as it can be (it is
    # in PROMPT_COMMAND): _tessera_si_ri returns the saved status it
    # indexes.
    _tessera_si_head_cases() {
        builtin local text="case \$_tessera_si_epipe in" n i pipe
        for (( n = 2; n <= _tessera_si_head_max; n++ )); do
            pipe=
            for (( i = 1; i <= n; i++ )); do
                pipe+="${pipe:+|}_tessera_si_ri $i"
            done
            text+=$'\n'"z$n) $pipe||_tessera_si_nop;;"$'\n'"n$n) $pipe&&_tessera_si_nop;;"
        done
        _tessera_si_cases_text=$text$'\n*) _tessera_si_rv "$_tessera_si_status"&&_tessera_si_nop;;\nesac'
    }

    _tessera_si_bp=
    if [[ -n "${bash_preexec_imported-}${__bp_imported-}" ]]; then
        # bash-preexec owns the DEBUG trap and PROMPT_COMMAND; use its hooks.
        _tessera_si_bp=1
        # The prompt marks are a PROMPT_COMMAND entry, not a precmd function:
        # bash-preexec runs precmd_functions first, before the entries a
        # theme rebuilds PS1 in (oh-my-bash's); _tessera_si_bp_place keeps
        # ours after them. Until the first prompt bash-preexec has only its
        # install entry (its text differs between versions): that goes last,
        # so entries that followed it (an array element of oh-my-bash's) run
        # before it and not after preexec is on, and our precmd, which it
        # runs, puts the marks in at the end of the first prompt.
        precmd_functions+=(_tessera_si_precmd)
        preexec_functions+=(_tessera_si_preexec)
        # shellcheck disable=SC2016
        _tessera_si_bp_install=${__bp_install_string:-'__bp_install "$_"'}
        # A string: an array on bash 5.1+ is not integrated.
        # shellcheck disable=SC2128,SC2178
        case ${PROMPT_COMMAND-} in
            *"$_tessera_si_bp_install"*)
                _tessera_si_bp_first=1
                # `:` keeps what follows valid (it may start with `;`).
                PROMPT_COMMAND=${PROMPT_COMMAND%%"$_tessera_si_bp_install"*}:${PROMPT_COMMAND#*"$_tessera_si_bp_install"}$'\n'$_tessera_si_bp_install
                ;;
            *) _tessera_si_bp_place ;;
        esac
        builtin unset _tessera_si_bp_install
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
        # The trap that was there is read by the head, at every prompt and
        # at top level: bash hides the DEBUG trap from `trap -p` in a
        # function and in a sourced file (this one, sourced from .bashrc),
        # and the user may set one later (in .bashrc, or at a prompt).
        # _tessera_si_untrap puts it back and runs it (see _tessera_si_pass).
        _tessera_si_prev_trap=
        _tessera_si_found=
        _tessera_si_untrap='builtin trap - DEBUG'
        # The trap's own commands would change `$_` (the last argument of
        # the previous command) and, for the trap before ours, `$?`: it
        # saves both first, restores them before that trap (in
        # _tessera_si_untrap), and ends with a command whose last argument
        # `$_` becomes (the `*)` arm). Its status is 0 unless the trap before
        # ours decided otherwise: under `extdebug` a DEBUG trap returning
        # non-zero skips the command (2: `return`). That status is handed
        # on through `&&` lists, as in the hooks (they make it no ERR
        # trap and no exit under `set -e`).
        # The conditional commands are eval'd quoted strings (`eval ""` is
        # a no-op with status 0), not unquoted expansions: those would be
        # split with the user's IFS (`IFS=,` made `builtin eval ...` one
        # word, "command not found"). The text never changes
        # (_tessera_si_pass compares the trap the head found with it).
        # shellcheck disable=SC2016
        # Its first statement also keeps PIPESTATUS, which is still the
        # last line's there (for _tessera_si_precmd after an empty line), and
        # its last one restores PIPESTATUS on bash 3.2 (_tessera_si_cases).
        _tessera_si_trap='_tessera_si_saved=("$?" "$_" "${PIPESTATUS[@]}")
builtin eval "${_tessera_si_armed:+_tessera_si_debug}"
builtin eval "${_tessera_si_disarm:+$_tessera_si_untrap}" && _tessera_si_nop
_tessera_si_kept=$?
_tessera_si_now=$_tessera_si_dopipe
case $BASH_COMMAND in _tessera_si_*) _tessera_si_now= ;; esac
case ${FUNCNAME[0]-} in _tessera_si_*) _tessera_si_now= ;; esac'
        # shellcheck disable=SC2016
        # Not before our own commands, nor (under `set -T`, where the trap
        # runs in functions) before a command inside one of our functions:
        # the entries' restore, see _tessera_si_pass, is for the user's.
        # The last command sets $_ back and, when nothing is restored, ends
        # with the status the user's trap left (under extdebug a non-zero
        # one skips the command).
        _tessera_si_cases _tessera_si_now 'o) _tessera_si_rv "${_tessera_si_ps[1]}" "${_tessera_si_saved[1]}" && _tessera_si_nop "${_tessera_si_saved[1]}" ;;
*) _tessera_si_rv "$_tessera_si_kept" "${_tessera_si_saved[1]}" && _tessera_si_nop "${_tessera_si_saved[1]}" ;;'
        _tessera_si_trap+=$'\n'$_tessera_si_cases_text
        # The head reads the trap with a command substitution, a fork per
        # prompt (about 0.5 ms of CPU on 3.2, 0.7 ms on 5.3). Not 5.3's `${ ...; }`: that runs in this
        # shell, where the user's trap fires and prints into the read.
        # Under `set -T` or extdebug the user's trap runs in the fork too,
        # before `trap -p`: its standard output goes to /dev/null for that
        # one hidden run, and only `trap -p` writes (through 3) to the read.
        # shellcheck disable=SC2016
        _tessera_si_read_trap='_tessera_si_found=$( { builtin trap -p DEBUG >&3; } 3>&1 1>/dev/null )'
        # shellcheck disable=SC2016
        # The entries that hand on a non-zero status do it as the first
        # command of an `&&` list: a failing command there does not run an
        # ERR trap or exit the shell under `set -e` (both would for a
        # plain one), and the list's status is still theirs.
        # _tessera_si_nop, not `:`, ends the list: the trap takes any
        # command but ours for the user's (after _tessera_si_ret, `:` would
        # get the C).
        # The head ends by giving the user's entries the status and
        # PIPESTATUS of the user's command.
        # shellcheck disable=SC2016
        # The head's commands that must run at top level (bash hides the
        # DEBUG trap in a function, and puts it back when one returns) are
        # eval'd from _tessera_si_h: PROMPT_COMMAND holds only one short
        # line of ours before the user's entries (bash-preexec 0.7.0's
        # sanitizer is super-linear in the text there, see the limits).
        # _tessera_si_precmd returns 0; the eval ends with the user's
        # status, handed on as the first command of an `&&` list.
        _tessera_si_h=$_tessera_si_read_trap$'\nbuiltin trap -- "$_tessera_si_trap" DEBUG\n_tessera_si_pass && _tessera_si_nop'
        # shellcheck disable=SC2016
        _tessera_si_head_el='_tessera_si_precmd && builtin eval "$_tessera_si_h" && _tessera_si_nop'
        _tessera_si_head=$_tessera_si_head_el$'\n'
        # A string PROMPT_COMMAND on bash 4 and later: PIPESTATUS for the
        # user's entries (eval's own status replaced it; bash 3.2 gets it
        # back from the trap before each entry's command), for a pipeline of 2 to $_tessera_si_head_max commands.
        if [[ -z "$_tessera_si_fixps" ]] && (( _tessera_si_head_max >= 2 )); then
            _tessera_si_head_cases
            _tessera_si_head+=$_tessera_si_cases_text$'\n'
        fi
        builtin unset _tessera_si_read_trap _tessera_si_cases_text
        _tessera_si_tail_el=$'_tessera_si_prompt && _tessera_si_nop\n_tessera_si_ret && _tessera_si_nop'
        _tessera_si_tail=$'\n'$_tessera_si_tail_el
        # Newline-separated: valid whatever the user's PROMPT_COMMAND ends
        # with (`;`, `&`, a newline or nothing).
        # shellcheck disable=SC2178 # a string stays a string
        PROMPT_COMMAND=$_tessera_si_head"${PROMPT_COMMAND-}$_tessera_si_tail"
    fi
    builtin unset -f _tessera_si_cases _tessera_si_head_cases
    # Child shells have none of these functions: an exported PROMPT_COMMAND
    # would print "command not found" there at every prompt and replace
    # their DEBUG trap.
    builtin export -n PROMPT_COMMAND

    builtin printf '\033]1337;ShellIntegrationVersion=1;shell=bash\007'

    # Loaded from PROMPT_COMMAND: this prompt's hooks have been skipped.
    # Nothing may run at top level after the trap is armed. (bash 3.2 puts
    # a DEBUG trap that was set before the file was sourced back on return:
    # then the first command has no C. 5.3 keeps ours, and the hidden one
    # is lost: see the limits.)
    if [[ -n "${_tessera_si_late-}" ]]; then
        builtin unset _tessera_si_late
        if [[ -n "$_tessera_si_bp" ]]; then
            builtin unset _tessera_si_late_elements _tessera_si_can_arm
            # bash-preexec's install entry still runs at this prompt and
            # runs precmd_functions (ours included) itself.
            [[ -n "${_tessera_si_bp_first-}" ]] || { _tessera_si_precmd && _tessera_si_nop; }
            _tessera_si_prompt && _tessera_si_nop
        else
            _tessera_si_precmd && _tessera_si_nop
            # The status precmd saw is the loader's own test's (1), not the
            # startup's: the first command sees 0, and the `.` the loader
            # runs as the last command of its list ends with 0 (`set -e` in
            # a startup file would exit the shell on 1).
            _tessera_si_status=0
            _tessera_si_elements=${_tessera_si_late_elements-1}
            builtin unset _tessera_si_late_elements
            if [[ -n "$_tessera_si_can_arm" ]]; then
                builtin unset _tessera_si_can_arm
                builtin trap -- "$_tessera_si_trap" DEBUG
                _tessera_si_prompt && _tessera_si_nop
                _tessera_si_ret && _tessera_si_nop
            else
                builtin unset _tessera_si_can_arm
                # The trap cannot be armed from here (see the loader block):
                # no D for the first command (no C either).
                _tessera_si_prompt && _tessera_si_nop
                _tessera_si_state=0
            fi
        fi
    fi
    # (Nothing more: the trap may be armed.)
else
    builtin unset _tessera_si_late _tessera_si_late_elements _tessera_si_can_arm
fi
