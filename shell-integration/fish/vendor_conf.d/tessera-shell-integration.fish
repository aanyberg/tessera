# Tessera shell integration for fish (3.2+): OSC 133 prompt marks, OSC 7
# working directory reports and a one-time OSC 1337 handshake.
#
# Tessera puts the directory holding fish/vendor_conf.d/ first in
# XDG_DATA_DIRS, so fish sources this file at startup, after the user's
# conf.d files and before config.fish (a user conf.d file with this name
# replaces it). Everything waits for the first prompt so config.fish can
# still change TESSERA_SHELL_INTEGRATION or the prompt. To load it in other
# shells, add to ~/.config/fish/config.fish:
#
#   set -gx TESSERA_SHELL_INTEGRATION enabled
#   source /path/to/shell-integration/fish/vendor_conf.d/tessera-shell-integration.fish
#
# TESSERA_SHELL_INTEGRATION holds space-separated feature words: `enabled`
# (or anything else) turns it on, `0` (or unset) turns it off, `no-cwd`
# drops the OSC 7 reports and `no-prompt-mark` the OSC 133 marks. It is
# erased once read, so shells started from this one are not integrated
# twice.
#
# fish 4.0 and later write OSC 133 marks themselves (unless the
# `mark-prompt` feature is off) and 4.3 and later OSC 7; this script then
# leaves those to fish and only sends the handshake. fish's own C mark
# carries the command line (`cmdline_url=`); ours never does (ADR 0021).

# Take our directory out of XDG_DATA_DIRS again, so programs started from
# this shell see the user's value (or none, if there was none).
if set -q TESSERA_FISH_XDG_DATA_DIR
    if set -q XDG_DATA_DIRS
        set --global --export --path XDG_DATA_DIRS "$XDG_DATA_DIRS"
        if set --local index (contains --index -- "$TESSERA_FISH_XDG_DATA_DIR" $XDG_DATA_DIRS)
            set --erase --global XDG_DATA_DIRS[$index]
        end
        if set -q TESSERA_FISH_XDG_WAS_UNSET; or test -z "$XDG_DATA_DIRS"
            set --erase --global XDG_DATA_DIRS
        else
            set --global --export --unpath XDG_DATA_DIRS "$XDG_DATA_DIRS"
        end
    end
    set --erase TESSERA_FISH_XDG_DATA_DIR
    set --erase TESSERA_FISH_XDG_WAS_UNSET
end

status is-interactive; or exit 0
test -n "$TESSERA_SHELL_INTEGRATION" -a "$TESSERA_SHELL_INTEGRATION" != 0; or exit 0
set -q __tessera_si_loaded; and exit 0
set --global __tessera_si_loaded 1

function __tessera_si_schedule --on-event fish_prompt -d "Set up Tessera shell integration at the first prompt"
    functions --erase __tessera_si_schedule
    set --local words (string split ' ' -- "$TESSERA_SHELL_INTEGRATION")
    set --erase TESSERA_SHELL_INTEGRATION
    if test -z "$words[1]"; or contains -- 0 $words
        return 0
    end

    # fish's version as major and minor numbers.
    set --local v (string match -r '^(\d+)\.(\d+)' -- $version)
    set --local major 0
    set --local minor 0
    if test (count $v) -ge 3
        set major $v[2]
        set minor $v[3]
    end

    printf '\e]1337;ShellIntegrationVersion=1;shell=fish\a'

    if not contains -- no-cwd $words
        and test $major -lt 4 -o \( $major -eq 4 -a $minor -lt 3 \)
        # OSC 7: `file://<host><path>`. fish's url style encodes every byte
        # outside [/._~A-Za-z0-9-] as %XX, Terminal.app's rule; the host is
        # reduced to [A-Za-z0-9._-]. Sent at every prompt (a program run
        # from the shell, ssh say, may have reported another directory
        # meanwhile) and when the directory changes.
        function __tessera_si_report_pwd --on-variable PWD --on-event fish_prompt -d "Report the working directory to Tessera"
            status is-command-substitution; and return
            set --local host (string replace -ra '[^A-Za-z0-9._-]' '' -- $hostname)
            test -n "$host"; or set host localhost
            printf '\e]7;file://%s%s\a' $host (string escape --style=url -- $PWD)
        end
        __tessera_si_report_pwd
    end

    if not contains -- no-prompt-mark $words
        # fish >= 4.0 marks prompts itself while the mark-prompt feature is
        # on; test-feature knows it from 4.0.6 on (it was always on before).
        set --local native 0
        if test $major -ge 4
            status test-feature mark-prompt 2>/dev/null
            test $status -ne 1; and set native 1
        end
        if test $native -eq 0
            # 0: no mark yet (no D before the first prompt); 1: a C is open;
            # 2: a prompt is showing and no command ran since.
            set --global __tessera_si_state 0
            function __tessera_si_mark_prompt_start --on-event fish_prompt --on-event fish_cancel --on-event fish_posterror
                # A prompt after no command (empty line, Ctrl-C, a parse
                # error) closes with a bare D.
                test "$__tessera_si_state" = 2; and printf '\e]133;D\a'
                set --global __tessera_si_state 2
                printf '\e]133;A\a'
            end
            function __tessera_si_mark_output_start --on-event fish_preexec
                set --global __tessera_si_state 1
                printf '\e]133;C\a'
            end
            function __tessera_si_mark_output_end --on-event fish_postexec
                set --local last $status
                test "$__tessera_si_state" = 1; or return
                printf '\e]133;D;%s\a' $last
                set --global __tessera_si_state 0
            end
            # B: after the prompt text. fish's prompt width ignores OSC
            # sequences. A fish_prompt defined later (a theme loaded after
            # the first prompt) drops it.
            if functions -q fish_prompt; and not functions -q __tessera_si_orig_fish_prompt
                functions --copy fish_prompt __tessera_si_orig_fish_prompt
                function fish_prompt
                    __tessera_si_orig_fish_prompt
                    printf '\e]133;B\a'
                end
            end
            __tessera_si_mark_prompt_start
        end
    end
end
