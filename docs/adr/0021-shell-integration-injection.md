# ADR 0021: Shell integration scripts and their injection on spawn

**Status:** accepted · 2026-10-29

## Context

Prompt navigation (#45), command timing and the working-directory features
(new tab in the same directory, #9) need the shell to tell the terminal
where prompts, commands and their output start, and which directory it is
in. The common vocabulary is `FinalTerm`'s OSC 133 marks (`A` prompt start,
`B` command start, `C` output start, `D;<status>` command end) and OSC 7
(`file://<host><path>`). Terminals deliver these in two ways: scripts the
user installs (`iTerm2` curls lines into the rc files; `WezTerm` ships
`wezterm.sh` for `/etc/profile.d` or the user's rc) or scripts the terminal
injects into the shells it starts (kitty; Ghostty copies kitty). Issue #44
ships Tessera's scripts for zsh, bash and fish, injects them, and decodes
OSC 7 and a handshake; OSC 133 decoding already existed. The spec pass
(`crates/tessera-shell-integration/tests/fixtures/osc_sequences.txt`) read
kitty, `WezTerm`, `iTerm2`, Ghostty and Terminal.app and ran zsh 5.9 and the
macOS `/bin/bash` 3.2.57 in a PTY; fish 4.9.3 was run by the implementation.

## Decision

**Injection, per shell (kitty's model).** Tessera changes only the spawned
shell's environment (and, for bash, its arguments), so nothing is installed,
no system file is touched and the user's startup files run as before.
`tessera_shell_integration::Injection::for_shell` computes the change and
`App::spawn_session` applies it when `[terminal] shell_integration = "auto"`
(the default; `"off"` disables it, and so does a `TESSERA_SHELL_INTEGRATION`
variable already in the child's environment, whatever its value). Every
injection sets `TESSERA_SHELL_INTEGRATION=enabled`; the scripts read its
space-separated feature words (`no-cwd`, `no-prompt-mark`, `0` for off) and
unset it, so shells started from an integrated shell are not integrated
again (as in kitty: a manual `zsh` inside a session is plain).

- **zsh**: `ZDOTDIR=<scripts>/zsh`, and `TESSERA_ORIG_ZDOTDIR` when the user
  had a `ZDOTDIR` (even an empty one: zsh reads "/" then). Our `.zshenv`
  restores or unsets `ZDOTDIR`, sources the user's `.zshenv` inside
  `{ } always { }` (so a failing user file cannot stop the integration) and
  autoloads `tessera-integration` in interactive shells; zsh then reads
  `.zprofile`, `.zshrc` and `.zlogin` from the user's directory. A user
  with no `.zshrc`, `.zshenv`, `.zprofile` or `.zlogin` (in `ZDOTDIR`, or in
  `HOME` when that is unset or empty) gets no injection, so
  `zsh-newuser-install` (present in macOS zsh) still runs; touching
  `~/.zshrc` turns the integration on. kitty also asks zsh for a `ZDOTDIR`
  set by `/etc/zshenv` in that case; we do not (macOS ships no
  `/etc/zshenv`), and a `ZDOTDIR` that `/etc/zshenv` sets for users with rc
  files is lost, as in kitty. A zsh running a command (`-c`, even `-ic`) or
  a script without `-i`, or told to skip its startup files (`-f`,
  `--no-rcs`, `-o norcs`, `+o rcs`), is not touched: it would never run our
  `.zshenv` (or not interactively), so our `ZDOTDIR` would reach the
  programs it starts (a tmux pane, say).
- **bash** (not injected yet, see "Bash: a follow-up" below; this is the
  design the script and `Injection::for_bash` implement): kitty's mechanism (`ENV=<script>` plus `--posix`, the script
  then leaving POSIX mode) does not work on the macOS `/bin/bash` 3.2.57:
  `bash --posix -i` never reads `$ENV` there (fixture B8), and kitty refuses
  bash older than 4 anyway. Tessera uses two mechanisms that work on 3.2 and
  on 5.x. An interactive non-login bash starts as `bash --rcfile
  <scripts>/bash/tessera.bash ...`; the script sources `~/.bashrc` (or the
  user's own `--rcfile`/`--init-file`, carried in `TESSERA_BASH_RCFILE` and
  removed from the arguments) before installing itself. A login shell
  ignores `--rcfile` and, as Tessera's default `Program::LoginShell`
  (`argv[0]` = `-bash`), takes no arguments at all; it, and a `--posix` or
  `--norc` shell, gets an exported `PROMPT_COMMAND='. "$TESSERA_BASH_SCRIPT"'`
  (after the user's exported `PROMPT_COMMAND`, if any), which sources the
  script at the first prompt, after every startup file ran; the script
  replaces that text with `:`. Why not kitty's route for bash 4 and later,
  where it works: Tessera controls the spawn and could start a login shell
  as `bash --posix` with `ENV`, but only after asking the binary for its
  version on every spawn (a fork and exec of the shell), and the 3.2 route
  would still be needed for the default macOS shell; one route that works on
  3.2 and 5.x is simpler. Its limitation: a startup file that assigns
  `PROMPT_COMMAND` instead of adding to it drops the loader and silently
  disables the integration for that shell (documented in
  `shell-integration/README.md`); the injected `TESSERA_BASH_*` and
  `TESSERA_SHELL_INTEGRATION` variables then stay in its environment, which
  is harmless, since nothing in a child reads them. Otherwise the scripts
  keep them out of children: the `--rcfile` route takes them out of the
  environment before sourcing `~/.bashrc` (a startup file can still set
  `TESSERA_SHELL_INTEGRATION` itself), the loader unsets them, and once its
  entries are in `PROMPT_COMMAND` the script stops exporting it (a child
  bash has none of our functions: it would print "command not found" at
  every prompt and lose its own `DEBUG` trap). A bash running a command (`-c`, even `-ic`) or a
  script without `-i` is not touched; the argument rules are kitty's
  `setup_bash_env`, plus `-o`/`+o` taking a name like `-O`. HISTFILE needs
  no handling (kitty's `HISTFILE` fix exists only for its POSIX mode).
- **fish**: `<scripts>` (the directory holding `fish/vendor_conf.d/`; fish
  appends `/fish` itself) goes first in `XDG_DATA_DIRS`, followed by the
  user's entries, or by the XDG default `/usr/local/share:/usr/share` when
  the variable was unset or empty (`TESSERA_FISH_XDG_WAS_UNSET=1`; kitty sets
  only its own dir, which hides `/usr/share/fish/vendor_*` from fish). fish
  sources `tessera-shell-integration.fish` after the user's `conf.d` and
  before `config.fish`; the script removes its entry again (erasing the
  variable when it was unset), and defers everything else to the first
  `fish_prompt`. A scripts path containing `:` cannot be listed and gets no
  injection.
- Any other program (`sh`, `nu`, `env`, ...) is started unchanged. The shell
  is the program's file name, or the child's `$SHELL` for a login shell.

Compared: `WezTerm` and `iTerm2` need the user to install something (and
`iTerm2`'s installer edits rc files); kitty's injection needs nothing, its
edge cases (empty `ZDOTDIR`, unreadable rc files, `warn_create_global`,
aliases shadowing builtins, deferred install for themes) are handled in
public code, and its feature words give users an opt-out. We keep its model
and fix its bash path for macOS.

**Delivery.** The scripts live in the repository's `shell-integration/`
directory and are embedded in `tessera-shell-integration` (`include_str!`).
`scripts::materialise` writes them to
`<Application Support>/Tessera/shell-integration/<SCRIPTS_VERSION>/` the
first time a shell needs them: into a temporary directory that is renamed
into place (so the app and the development window, which share the path,
never see a partial set; when two race, both use the first), directories
`0o700`, files `0o644`. An existing version directory is used as is. After
writing, directories of lower versions and temporary directories older
than an hour are removed. Bundle resources were rejected (the development
window has no bundle, and Xcode packaging is work for nothing) and so was
`~/.config` (not the macOS convention, and not where the rest of Tessera's
state lives). `SCRIPTS_VERSION` must change with any script: a test
compares a digest of the embedded scripts with a recorded one and fails
with "bump SCRIPTS_VERSION". If the scripts cannot be written the shell
starts without integration and a warning is logged.

**What the scripts send.**

- A handshake once, before the first prompt:
  `OSC 1337 ; ShellIntegrationVersion=1 ; shell=<zsh|bash|fish> BEL`,
  `iTerm2`'s form so existing tooling recognises it. The number is Tessera's
  protocol version (`scripts::PROTOCOL_VERSION`), not `iTerm2`'s script
  version (24 today), and changes only when what the scripts send changes
  incompatibly; the terminal (#9) logs a major mismatch.
  `parse_osc1337` returns the version and shell and `None` for every other
  OSC 1337 key.
- OSC 133: `A` before each prompt and `B` after it (in `PS1`, wrapped in
  `%{ %}` / `\[ \]` so the prompt width is right), `C` when a command starts
  and `D;<status>` at the next prompt. No `D` before the first prompt; a bare
  `D` when a prompt was shown and no command ran (an empty line, Ctrl-C at
  the prompt: zsh reports 130 and bash 3.2 the previous status there, and
  the bare `D` hides the difference); never `C` for an empty line. `exit`
  sends `C` and no `D` in zsh and bash. `C` carries no `cmdline=` (kitty
  sends it): the command line is the user's data and nothing consumes it
  yet; #45 can add it behind a feature word.
- OSC 7 `file://<host><path>` at startup, at every prompt and when the
  directory changes (zsh `chpwd`, skipped in a subshell or command
  substitution, whose `cd` is not the shell's; fish `--on-variable PWD`), as
  kitty does: a program run from the shell (ssh, say) may have reported
  another directory meanwhile, so a report only on change could leave the
  terminal with a wrong one. fish 4.3 and later report at every prompt
  themselves. The path is percent-encoded
  byte by byte with Terminal.app's rule (every byte outside
  `[/._~A-Za-z0-9-]` becomes `%XX`; fish's `string escape --style=url` is
  the same rule); the host is the shell's own `$HOST`/`$HOSTNAME`/
  `$hostname` reduced to `[A-Za-z0-9._-]`, `localhost` if that leaves
  nothing, never empty, so a remote shell running the same script is
  distinguishable. `parse_osc7` decodes `file://` (and kitty's
  `kitty-shell-cwd://` with its raw path), tolerates `iTerm2`'s `user@` and
  `?query`, and returns `None` for a payload over 4096 bytes, a path that is
  not absolute, a malformed escape, an encoded `/`, control characters,
  invalid UTF-8 or a `..` segment. Whether the host is this machine is
  decided by the consumer (#9), not the parser.

**Hooks.** zsh: `precmd_functions`/`preexec_functions`/`chpwd_functions`,
installed at the first prompt (kitty's reason: frameworks and themes loaded
by `.zshrc` reset `PS1` and hooks, and `.zshrc` can still change the feature
words); our `precmd` stays last so its `PS1` edit survives a theme that
rewrites `PS1` in its own hook (it moves itself to the end and prints `A`
directly for that one prompt); marks go to a close-on-exec descriptor for
the terminal. bash: `PROMPT_COMMAND` entries around the user's
(newline-separated, so any user value stays valid): the first takes the
status, sends `D` and OSC 7 and passes the status on; the last ones put the
marks into `PS1` and install a hand-written `DEBUG` trap at top level. The
trap marks the start of the user's command (`C`) and replaces itself with
the trap that was there before (or none), so it costs nothing while
commands run: a trap that stayed would run before every simple command and
made a 200 000-iteration loop in an interactive bash 3.2 ten times slower
(0.33 s to 3.2 s; with the self-removing trap 0.31 s to 0.35 s either way).
A trap set inside a function is put back when the function returns, so
the trap is installed and removed only at top level (the
`PROMPT_COMMAND` text and the trap text). The trap is installed by the
first entries of `PROMPT_COMMAND` (after the one that takes the status), so
the trap that was there before ours never runs for `PROMPT_COMMAND`'s
entries, only (through ours) for the user's command and then alone: a
prompt framework's preexec (starship's), which its precmd entry arms, starts
at the user's command as it would without Tessera. The trap's status is 0
unless that previous trap's decides it: under `extdebug` a non-zero status
skips the command. The trap saves `$?` and `$_` first
and ends with a function whose status and last argument they are
(bash-preexec's technique), so `mkdir d` followed by `cd $_` and a user trap
after ours see what they would without it. bash 5.1 and later run every
element of an array `PROMPT_COMMAND`; the script joins the elements into one
string (at load and at each prompt) so its entries stay first and last. bash
runs a copy of the array taken when the prompt starts, so at the prompt
right after an element is added interactively that element still runs
after ours: the script does not arm its trap then, and that one command has
no `C` and ends with a bare `D`. With bash-preexec loaded its
`preexec_functions`/`precmd_functions` are used instead. Limits, documented
in the script: a `DEBUG` trap the user sets after the script loaded is
replaced at the next prompt by ours chained to the one found at load
(re-reading `trap -p DEBUG` at every prompt would cost a fork per prompt);
a command line that runs only a subshell (`(cd x; make)`) does not fire the
`DEBUG` trap in the parent, so it ends with a bare `D` instead of `C` and
`D;<status>` (with `set -T` or `extdebug` the subshell inherits the trap and
sends the `C` itself, still followed by a bare `D`; a note for #45, which
must not read a bare `D` as "nothing ran"); with `set -T` the trap also fires inside functions, and ours are
skipped by name. Run on `/bin/bash` 3.2.57 and Homebrew bash 5.3.20.
fish: `fish_prompt`/`fish_cancel`/`fish_posterror`, `fish_preexec`,
`fish_postexec` and `--on-variable PWD`. fish 4.0 and later mark prompts
itself (while its `mark-prompt` feature is on; its `C` carries
`cmdline_url=`) and 4.3 and later send OSC 7 at every prompt, so the script
then sends only the handshake, as kitty's does.

**Notes for the consumers (#9, #45).**

- `vte` splits an OSC at `;` into parameters; `parse_osc133`, `parse_osc7`
  and `parse_osc1337` take the payload after the number joined back with
  `;` (`D;0`, `ShellIntegrationVersion=1;shell=zsh`), so #9 rejoins the
  parameters before calling them.
- fish 4 sends its own `C` with `cmdline_url=<the command line>`; the parser
  ignores it, and #45 must not store it either (the reason our `C` carries
  no command line applies to fish's own mark too).
- `scripts::materialise` uses an existing version directory as it is: the
  threat model is a user who can already write to their own
  `~/Library/Application Support` (and so to their own startup files);
  nothing outside that directory is trusted or written.

**What the terminal does with them later.** #9 routes OSC 133, 7 and 1337
from `tessera-term` to this crate: marks become `tessera_grid::Mark`s on
their rows, OSC 7 sets the session's working directory when the host is
local, and the handshake is recorded per session (and a protocol mismatch
logged). #45 navigates between `A` marks and selects command output
between `C` and `D`. Per-profile overrides of `shell_integration` arrive
with profiles (#39).

**Bash: a follow-up.** Tessera does not inject bash yet:
`Injection::for_shell` leaves every bash alone and `App::spawn_session`
starts it unchanged, while the script, `Injection::for_bash` (the rules
above) and their live tests stay in the tree. The adversarial run on the
login route found that prompt tools which wrap the existing
`PROMPT_COMMAND` break it: starship's init moves our exported loader into
its own `STARSHIP_PROMPT_COMMAND` and runs it at every prompt, so once the
loader has removed itself and unset `TESSERA_BASH_SCRIPT` it runs `. ""` and
prints an error after every command for the life of the shell. With
bash-preexec a login bash sends a bare `D` before the first prompt,
oh-my-bash's first prompt has no `A`/`B`, and a `.bash_profile` that
assigns `PROMPT_COMMAND` leaves the `TESSERA_*` variables and an exported
`PROMPT_COMMAND` to children. macOS's default shell is zsh; bash support
returns with a loader that survives being wrapped.

**Partial scripts.** `materialise` uses an existing version directory only
when every script is in it (one `stat` each per spawn); otherwise it writes
a fresh copy in its place.

## Consequences

- zsh and fish users get prompt marks and directory
  reports without installing anything; `TESSERA_SHELL_INTEGRATION=0` or
  `shell_integration = "off"` opts out, and the scripts can be sourced
  manually in other shells (`shell-integration/README.md`).
- bash users are not integrated automatically yet (a follow-up); a new zsh
  user without startup files and shells started inside a session (`exec
  zsh`, tmux) are not integrated automatically either.
- Every script change bumps `SCRIPTS_VERSION`; the old directory is pruned
  by the next write.
- The injected variables count toward the `execve` size limit of #74 like
  any other spawn variable (an `App` test proves a spawn that fits with
  integration off is refused with it on).
- `tests/live.rs` runs the scripts in `/bin/zsh`, `/bin/bash`, Homebrew's
  bash 5 and fish (each skipped when absent) through a real PTY on every
  test run.
