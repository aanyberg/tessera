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
- **bash** (injected since the 2026-11-17 amendment below, which replaces
  the login route described here): kitty's mechanism (`ENV=<script>` plus `--posix`, the script
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
element of an array `PROMPT_COMMAND`; the integration takes a string
`PROMPT_COMMAND` only, and leaves a shell whose `PROMPT_COMMAND` is an array
as it is without Tessera (see **Arrays are not integrated** below). With bash-preexec loaded its
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

**Bash: a follow-up** (resolved by the 2026-11-17 amendment). Tessera did
not inject bash at first:
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
- bash users are integrated automatically too (2026-11-17 amendment),
  except a login bash whose startup files assign `PROMPT_COMMAND`; a new zsh
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

## Amendment 2026-11-17: bash is injected, with a guarded loader

`Injection::for_shell(Shell::Bash, ..)` returns the bash injection
(`Injection::for_bash` is gone) and `App::spawn_session` uses it. The
routes, by launch, on every bash version (one route each, no version probe;
kitty's `--posix`/`ENV` route is not used for bash 4 and later either, so
3.2 and 5.x run the same code):

| Launch | Route |
| --- | --- |
| interactive non-login (`-i`, or no `-c`/script operand) | `--rcfile <scripts>/bash/tessera.bash`, unchanged from above (`TESSERA_SHELL_INTEGRATION`, `TESSERA_BASH_INJECT=rcfile`, `TESSERA_BASH_RCFILE`, all taken out of the environment before `~/.bashrc`) |
| login (`-bash`, `-l`), `--posix`, `--norc` | an exported `PROMPT_COMMAND` ending in the loader below; no other variable |
| `-c` (even `-ic`), a script without `-i` | none |

The loader is self-contained text, with the feature words and the script's
path quoted literally (a `'` as `'\''`), and no variable of ours:

```bash
builtin [ -n "${_tessera_si_loader-}" ] || _tessera_si_load='enabled' . '<scripts>/bash/tessera.bash' 2>/dev/null
```

The guard and the redirection keep a loader that outlives its shell silent.
A prompt tool
that copies `PROMPT_COMMAND` before the loader removed itself (starship
moves it into `STARSHIP_PROMPT_COMMAND` and evaluates that copy at every
prompt) runs a copy that the first guard turns into a no-op once the
script has set `_tessera_si_loader`, instead of the old
`. "$TESSERA_BASH_SCRIPT"`, which ran `. ""` and printed `No such file or
directory` after every command once the variable was unset. A loader that
never ran stays exported when a startup file replaces the shell
(`.bash_profile` with `exec tmux` leaves it in the tmux server's
environment, so every new pane inherits it), and an update prunes the
script it names: the redirection makes such a shell's failing `.` silent at
every prompt, with no integration, rather than print that error (an
interactive shell survives it, also in POSIX mode). The path is written once
because bash-preexec's sanitizer on bash 3.2 slows down steeply with the
length of `PROMPT_COMMAND`. The temporary assignment uses `.`, not `builtin .`: bash 3.2 drops a temporary
assignment to `builtin` after the first command of the sourced file. The
script takes the feature words from `_tessera_si_load`, unsets it, sets
`TESSERA_SHELL_INTEGRATION` unexported (unless a startup file set it),
replaces the loader text it rebuilds from the same words and its own
`BASH_SOURCE` with `:`, and stops exporting `PROMPT_COMMAND`. So no
`TESSERA_*` variable and no `PROMPT_COMMAND` reaches a child of an
integrated login bash.

Prompt tools (stand-ins of starship 1.27 and master, bash-preexec 0.7.0,
0.6.0 and 0.5.0 and oh-my-bash in
`crates/tessera-shell-integration/tests/fixtures/bash-tools/`, table in
`tests/fixtures/bash_spec.txt`, run by `tests/bash_tools.rs` on `/bin/bash`
3.2.57 and, with its own first-prompt columns, on bash 5.1 and later; taken
on Homebrew's 5.3):

- Loaded where the shell's own `PROMPT_COMMAND` runs the loader (no tool,
  bash-preexec, oh-my-bash, both): the first prompt has `A`/`B` and every
  command `C` and `D;<status>`, except for the bash 5.1 cases below. With
  bash-preexec, whose install entry the script finds through
  `__bp_install_string` (each release has its own text), that entry moves
  to the end of `PROMPT_COMMAND` and the first prompt gets its marks from
  the precmd it forces; afterwards the script's prompt entry stays the last
  one before `__bp_interactive_mode`, so a theme's entry that rebuilds `PS1`
  runs first. A `C` is only sent for a prompt whose `PS1` shows our marks
  (no `C` that does not follow a `B`, also with bash-preexec's `PS0` path on
  bash 5.3) and a bare `D` only after one. The `DEBUG` trap is armed at load only when the
  loader was the last entry of `PROMPT_COMMAND` at top level, since an
  entry after it, or a tool's function around it, would fire the trap and
  send a stray `C` before the first prompt.
- Loaded inside another tool's hook (starship evaluates its copy of
  `PROMPT_COMMAND` in a function and rewrites `PS1` after it): the first
  prompt and the first command are unmarked, with no stray `C` and no bare
  `D`; every later prompt has `A`/`B` and every later command `C` and
  `D;<status>` (lead decision D3).
- bash 5.1 and later with an array `PROMPT_COMMAND` (oh-my-bash appends an
  element, bash-preexec 0.6 and later make one): not integrated, on every
  route (the 2026-11-22 amendment, **Arrays are not integrated** below).
- A startup file that assigns `PROMPT_COMMAND` would drop the loader and,
  because bash keeps the export attribute of a variable that came from the
  environment, leave its own value exported with no code of ours running to
  clear it. A child `bash`, `bash --norc`, `sh -i` or `script` that lacks the
  function it names then prints `__p: command not found` at every prompt. So
  before giving a bash the loader, `Injection::for_shell` reads what that
  bash would read, without running anything: for a login shell the first of
  `~/.bash_profile`, `~/.bash_login` and `~/.profile`, and `~/.bashrc`; for
  `--posix` `$ENV` and, unless `--norc`, `~/.bashrc` (the macOS bash 3.2 reads
  that instead); and one level of `.`/`source` with a literal `~/`, `$HOME` or
  absolute path. If one of them assigns the variable without expanding its
  old value (`+=`, `"x;$PROMPT_COMMAND"` and array appends stay injected) bash
  starts exactly as if Tessera did not know it. Sourced files holding
  bash-preexec or a cached `starship init` are exempt: they assign the
  variable and keep its entries.
  The decision errs towards not injecting, since a false alarm costs one
  shell's integration and a miss prints errors in its children. The files are
  untrusted input: regular files only, opened so that a FIFO or device cannot
  block, 1 MiB per file and 32 paths per scan, and anything over a limit,
  unreadable or without an absolute `HOME` counts as "do not inject". Spawning
  runs on the app's main thread and a regular file on a stalled disk can block
  in `read`, so the scan runs on its own thread: the caller waits for about a
  second and then starts bash uninjected, and at most 8 scans can be alive.
  What remains: an assignment behind a computed `source` path, in `eval`, or
  in `/etc` is not seen, and then the child errors above still happen. The
  way to end them is a route that runs before the startup files, `--noprofile`
  with an emulated login through the `--rcfile` route (prototyped; the 0.3.0
  follow-up). The `--rcfile` route itself is not affected: there the script
  clears the export, with or without bash-preexec.
- An entry appended with `PROMPT_COMMAND="$PROMPT_COMMAND; cmd"` stays
  valid (the script's own entries are newline-separated and the last ends
  without a separator); since bash now runs `cmd` after our entries, the
  trap is armed from the expansion of `PS1` instead
  (`${_tessera_si_none[_tessera_si_armed=1]-}` inside the `B` mark), so
  every command keeps its `C` (not with `promptvars` off or with `PS1`
  rebuilt by the appended command, which then gets the bare-`D` case).
- OSC 7 is encoded once per directory and host (the encoded value is
  cached; one value, S5), not at every prompt: a 3.4 KB non-ASCII
  directory took 79 ms per prompt on bash 3.2.

A bash 4.0 to 5.0, when it is the one installed, runs the same tests and
holds every rule except the first-prompt marks; a real login bash with the
user's own dotfiles is eye check E5. The other limits stand as described under **Hooks** (a command
line that only runs a subshell ends with a bare `D`; `extdebug` and `set -T`
let the subshell send the `C`). powerlevel10k is zsh-only, so it does not
apply to bash.

## Amendment: bash marks stay out of redirected output (#538, 2026-11-19)

The bash `DEBUG` trap fires before the first simple command inside a
compound command's redirection, so `{ echo hi; } > f` ran it with `f` as
standard output and the `C` mark landed in `f`. So the prompt hook, where
standard output is the session's own stream, keeps two copies of it on
private descriptors, the first two of 59 down to 50 that are not open
(checked with `/dev/fd/N`, a dup since #611, see below; 63 and down are bash's process substitutions
and 255 its terminal descriptor; not bash 4.1's `{var}>&1`, which 3.2
lacks and which picks 10 or above, the numbers users redirect). The trap
writes `C` to the first copy and closes both before the user's command
runs (bash cannot mark a descriptor close-on-exec, so an open copy would
be inherited by every command), but only while the two are still the same
file: the trap runs inside a compound command's redirections, so
`{ echo a >&59; } 59>f` or `while read -u 59 l; do ...; done 59<in` has
already pointed one of them at the user's file, and writing `C` there and
closing it would corrupt the command. Then the trap writes no `C` and
closes nothing, and the next prompt closes the copies (every prompt closes
the last line's copies before taking new ones, so none outlives a prompt).
`C` therefore follows the session's stream
exactly as the command's output and `D` do: a session logged through
`exec > >(tee log)` gets `C` and `D` in order with the output still in the
pipe. Writing `C` to `/dev/tty` instead put it around the pipe: `D` sent
that way arrived before the last 40 KB of `seq 1 20000`, and the `C` of a
second pasted line some 30 KB inside the first line's output. A command
line that only runs a subshell (`( cmd ) > f`) runs no trap in the shell
itself, so the copies stay open in that subshell until the next prompt
closes them. A command that redirects either copy's descriptor gets no `C` for
that line (and its commands inherit the copy it left alone); one that
points both at the same file, or one at the file the other already is
(`59>&1` at the terminal), gets `C` written to it and its descriptors
closed. `D`, OSC 7 and the handshake are written at a prompt or at load
and go to standard output. The same review found
three more bugs in the hooks, fixed with it: the trap's conditional
commands were unquoted expansions, split with the user's `IFS` (`IFS=,`
gave "command not found" around every command), so they are now `eval`'d
quoted strings; and the hooks that pass the last status on (to the user's
`PROMPT_COMMAND` entries, `PS1` and the trap before ours) failed with it,
so a user's `ERR` trap ran for them and `set -e` exited the shell after
`false && true`. They now run as the first command of an `&&` list
(`_tessera_si_precmd && _tessera_si_nop`), which keeps the status without
counting as a failed command. bash 3.2 resets `PIPESTATUS` in any `DEBUG`
trap, ours included (4 and later restore it): restoring it would take a
pipeline of forked subshells before every command, so it is a documented
limit of the script.

**#611 (2026-11-24).** A stat of `/dev/fd/N` fails now and then (`EBADF`)
while another process stats its own `N`: XNU shares one fdesc node per
number, and every integrated shell uses the same numbers, so under load a
free descriptor looked open or the two copies looked like different files
and the line lost its `C`. Whether a descriptor is open is now a dup
(`: >&N`) instead of `-e /dev/fd/N`, and the trap counts the copies as
different files only when `-ef` says so on 8 tries running. A dup
redirects, and bash 3.2 keeps a redirection whose saved copy it cannot
make (no free descriptor from 10 up, EMFILE), which left standard output
on the probed descriptor for good; so the copies are taken only while two
of 10 to 19 are free (closed for `-e` 8 tries running), and otherwise `C`
goes to standard output. Free also means below the soft limit (`ulimit -n
11` left 11 closed but unusable, and standard output went to the user's
descriptor 50). Reading the limit takes a fork in 3.2 (+12% per prompt), so
each of the two is proven by a dup onto it instead, the second with standard
error on `/dev/null`; a limit at or below the first free one from 10 still
shows bash's error at each prompt.

## Amendment: bash keeps the user's DEBUG trap however the script is loaded (#542, 2026-11-19)

bash hides the `DEBUG` trap from `trap -p` inside a function and inside a
sourced file (3.2 and 5.3 alike), so the script sourced from `.bashrc` by
hand saw no trap at load and dropped the user's at the first prompt. The
`PROMPT_COMMAND` entries that install ours therefore read the trap in its
place at every prompt, at top level, and take it for the user's unless it is
ours (no command fired it since the last prompt). That also keeps a trap set
after the script loaded or typed at a prompt, which was a documented limit
above: the fork per prompt the Decision rejected is now accepted (a command
substitution: about 0.5 ms of CPU per prompt on bash 3.2 and 0.7 ms on 5.3,
measured against main). The read is a command substitution on every bash, not
5.3's `${ ...; }`: that runs in the shell itself, where the user's trap
fires and prints into the read (a title trap printing the directory's name
had that name installed and run as the trap). With `set -T` or `extdebug`
the user's trap fires inside the fork as well, so the fork sends its
standard output to `/dev/null` and only `trap -p` writes to the read
(through descriptor 3). As a second guard only text that is exactly
`trap -- '<text>' DEBUG`, with no quote left in `<text>` once every `'\''`
is removed, is taken for a trap; anything else leaves the trap as it was.
So under `set -T` or `extdebug` a trap that writes to descriptor 3 (into the
read) is not taken up (documented in the script). The hooks run the user's
trap as the first command of an `&&` list, so a non-zero status it ends with
(a false `&&` list, in a trap that acts on some commands only) is not made a
failure by the hooks' own plumbing (which ran `ERR` three times per command
and exited a `set -e` shell), and still reaches bash (under `extdebug` it
skips the command). A command that fails inside the user's trap still runs
`ERR`, as in a plain bash, and more often: the trap also runs during the
prompt hooks (a bare `false` in it gave 15 `ERR` lines on bash 3.2 for two
commands against 9 in a plain bash).
The user's trap now also runs for the read, once more per prompt. Loaded from
`PROMPT_COMMAND` (the login route) the script still cannot see a trap a
startup file set: bash 5.3 keeps the trap the file sets, so the user's is
lost, and 3.2 puts it back, so the first command has no `C` (documented in
the script).

## Amendment: bash injection is on, with the scan's terminator, a 3.2 PIPESTATUS restore and the M10 table (#158, #540, 2026-11-22)

Bash is injected on the `--rcfile` route and the login route. The decisions
of the wave 27 review (fixture rows in
`crates/tessera-shell-integration/tests/fixtures/bash_spec.txt`, one live test
each in `tests/bash_rows.rs`):

- **The scan picks what follows the loader (B5).** The login route's
  `PROMPT_COMMAND` ends with the loader, so `.bash_profile` appends land right
  after its last word. The startup scan classifies every append it sees: one
  that starts with a separator (`"$PROMPT_COMMAND; x"`,
  `"${PROMPT_COMMAND:+$PROMPT_COMMAND;}x"`, blanks then a separator) or a
  newline is fine after the bare loader; a glued one (`+='x'`,
  `"${PROMPT_COMMAND}x"`, `"$PROMPT_COMMAND x"`, and one led by an expansion
  the scan does not know) needs a newline after it. Every append
  separator-led: nothing follows the loader; any glued one, or none seen: a
  newline follows it; both kinds: no injection (no single ending suits both).
  A separator-led append the scan cannot see (beyond the caps, in `eval`,
  behind a computed path) prints one syntax error on the loading prompt.
  The scan's own choices, each a fixture row: blanks before a separator
  (`"$PROMPT_COMMAND ; x"`) count as separator-led (pc B5j, scan U2); an
  append led by an expansion the scan cannot read (`"$PROMPT_COMMAND$x y"`)
  counts as glued, so a `$x` that starts with `;` gets the newline and
  prints a syntax error at the loading prompt (pc B5k, scan U3); a
  POSIX-mode login bash has every profile scanned (`.bash_profile`,
  `.bash_login` and `.profile`), since 3.2 and 5.x differ on which they read
  (scan U1). Once loaded, the script removes the loader together with the
  `;`, `&&`, `||`, `&`, `|`, newlines and blanks that followed it, so the
  user's entry starts with its command (`b5_loaded_entries_lose_the_separator`;
  a leading `&&` was a syntax error at every later prompt, pc B5l). A leading
  `||` is a semantic change: `"$PROMPT_COMMAND || cmd"` runs `cmd` at every
  prompt from the second on, where without Tessera it is a syntax error at
  every prompt; the lesser harm, and rare.
- **Arrays leave the shell alone (B5h).** `PROMPT_COMMAND+=(...)`,
  `PROMPT_COMMAND=(...)` and any subscripted assignment mean no injection on
  the login route, on every bash: on 3.2 an element added to a string
  `PROMPT_COMMAND` lands at `[1]`, which never runs. Users of such startup
  files get no marks on the login route. oh-my-bash is one: its
  `PROMPT_COMMAND+=(...)` is in its own files, which the scan reads as text
  whatever the bash version, so an oh-my-bash login shell is not injected on
  any bash (its fixture rows X5, X6, X7, X14e, X15c and X15d are skipped on
  the login route as `login-alone`, and run on the `--rcfile` route).
  Workaround: start a non-login bash (the `--rcfile` route; for example the
  profile's command `/bin/bash -i`, or `bash` run from the login shell), or
  source `tessera.bash` from `.bashrc` after oh-my-bash.
- **The loader text is removed completely (B6).** After loading, nothing of
  the loader is left in `PROMPT_COMMAND` (no `:` or test). No status-capturing
  wrapper: the entry on the loading prompt sees the status the loader left
  (0; B6b, stated limit); from the second prompt on, the user's entries see
  the command's `$?` and `PIPESTATUS` as without Tessera. An entry added
  at a prompt (`PROMPT_COMMAND+=$'\n'cmd`, `PROMPT_COMMAND="$PROMPT_COMMAND;
  cmd"`) runs after our tail at that prompt and sees
  the command's `$?` but not its `PIPESTATUS` there, as on main; from the
  next prompt on the script has moved it up behind the head's restore,
  without its leading separators, and it sees both as without Tessera
  (`entries_added_at_a_prompt_see_the_users_status`).
  A string `PROMPT_COMMAND` is never made an array (the
  `PROMPT_COMMAND="$PROMPT_COMMAND; f"` idiom has to keep working); an
  array is not integrated (**Arrays are not integrated** below).
- **The scan does not block on what it reads (B8).** A path is stat'ed
  (following links) first; anything but a regular file is left to bash
  unopened. It is then opened with `O_NONBLOCK|O_NOCTTY|O_CLOEXEC` and
  fstat'ed: a file swapped between the two (other device or inode, or no
  longer regular) is not read. The caps (1 MiB, 32 files, 8 scans, 1 s) are
  unchanged.
- **Tool names count as definitions only (M2).** A file that assigns
  `PROMPT_COMMAND` is exempt as a prompt tool's only when it defines one of
  the tool names (`name=`, `name()`, `function name` at a line start, outside
  a heredoc body), not when it mentions one.
- **The `--rcfile` route only where bash reads it (M10).** Not for `--norc`,
  `-norc`, `--posix`, `-posix`, `-o posix` (bash's single-dash long options
  are recognised), an environment with `POSIXLY_CORRECT` or a `SHELLOPTS`
  holding `posix`; those launches take the loader or no injection, so no
  `TESSERA_*` variable is left in a shell that never read our rc file.
  Posix mode counts on the login route too, where the scan then reads every
  profile.
- **PIPESTATUS on bash 3.2 (#540).** bash 3.2 resets `PIPESTATUS` in any
  DEBUG trap. `_tessera_si_precmd` saves `$?` and `PIPESTATUS` first; the
  armed trap, for a command whose text names `PIPESTATUS` (no trap of the
  user's, `extdebug` off), puts them back with a pre-generated pipeline of
  `_tessera_si_rv` calls (2 to 9 commands, a `z`/`n` pair per length by the
  status's sign: 16 bounded pipelines, plus a function call for one failed
  command), never the last command of its list. The user's `PROMPT_COMMAND`
  entries get the same restore before each of their commands on 3.2, which
  is all they need there (so the head has no restore on 3.2, P18 under
  `set -T`); on bash 4 and later (which keep `PIPESTATUS` in the trap) a
  string `PROMPT_COMMAND`'s entries get the head's restore. The
  head's top-level commands (the trap read and install) are `eval`'d from a
  variable, so `PROMPT_COMMAND` holds one line of ours before the user's
  entries: 71 bytes on 3.2, that line plus a compact `case` on bash 4 and
  later (2026 bytes; `the_head_before_the_users_entries_stays_short` bounds
  both). The bound: the trap text and the bash 4+ head hold 16
  pre-generated pipelines each (`z2`..`z9`, `n2`..`n9`, at most 9 commands),
  fixed when the script loads, never built from what the user ran; more than 9 pipeline
  commands are not restored (P15). Cost, one fork per pipeline command per
  restore: on bash 3.2, after a pipeline of 2 to 9 commands, for a command
  line naming `PIPESTATUS` and, with user entries, before each top-level
  command of those entries; on bash 4 and later only the head's restore for
  the user's entries, that is with a `PROMPT_COMMAND` entry of the user's and
  after a pipeline of 2 to 9 commands, once per prompt (up to 9 forks).
  Under `set -T` (the DEBUG trap runs in functions) bash 3.2 also restores
  before each command a user entry function runs and twice for the
  function's call; the trap text skips commands inside our own functions
  (`FUNCNAME`), so the hooks add nothing per command of theirs. With
  `f() { echo; true; true; }` as the entry, `true | false | true` costs 15
  forks on 3.2 (5 restores) and 3 on 5.3, the head's
  (`p18_set_t_forks_for_the_users_commands_only`).
  With no user entries bash 4 and later fork for nothing: P18
  (`p18_bash_4_restores_nothing`) logs every restoring command's `$BASHPID`
  on 5.3 and sees none forked after `true | false | true`, then 3 forked once
  an entry is added. An ordinary line forks nothing on any bash
  (`_tessera_si_restores` counts the restores).
- **Accepted limits**, each pinned by a fixture row: P14 (`extdebug`: no
  restore), P15 (more than 9 pipeline commands), the gate miss G1 (a function
  that reads `PIPESTATUS` called by a line that does not name it); P16 (after
  an empty line) passes. P17 / M7 (a user DEBUG trap: no restore, and the trap also runs for
  the hooks' first commands at each prompt, so it fires more often than
  without Tessera); M1 (as documented above, the user's trap also runs
  during the prompt hooks); on the login route with a DEBUG trap set in
  `.bash_profile`, bash 3.2's first command has no `C` and a bare `D`.
- **Known limits.** bash-preexec 0.7.0's sanitizer (`__bp_sanitize_string`,
  run when `. bash-preexec.sh` installs at a prompt) is slow on any string
  `PROMPT_COMMAND` with text before its install `:`, with or without
  Tessera: its extglob loop grows steeply with that text (with an 80-byte
  string before the `:`, sourcing it stalls bash 3.2 for about 8 s and 5.3
  for about 2 s; main's 260-byte head stalled it for 7 to 11 s on 3.2 and
  5.5 to 6 s on 5.3). Our part is kept short for it: measured at a prompt
  with bash-preexec 0.7.0 (`. bash-preexec.sh` until the next prompt,
  load 9 to 16), 0.5 to 0.95 s on 3.2 (main 10.7 s), 0.01 s on 5.3 (main
  5.45 s; there bash-preexec 0.7 adds its install as an array element, so
  the shell leaves the integration); on bash 4.x and 5.0, which sanitize
  the whole string, the bash 4+ head costs the sanitizer 0.09 s (measured
  on 5.3). A user's own long entries before
  bash-preexec still stall it as they do without Tessera.
- **`set -u`.** Every expansion of `PROMPT_COMMAND` and of our variables is
  safe under `set -u` (the array attribute test turns `nounset` off inside
  its function), on 3.2 and 5.3, on both routes
  (`set_u_shells_are_integrated`; an array under `set -u` is left alone
  without an error, `an_array_prompt_command_is_left_alone`).
- **Arrays are not integrated (2026-11-22, lead decision after #158 r4,
  [#613](https://github.com/aanyberg/tessera/issues/613)).** On bash 5.1 and later a shell whose `PROMPT_COMMAND` is an
  array when the script loads, or turns into one at a prompt, runs as it
  does without Tessera: no hooks, no DEBUG trap of ours, no marks.
  `_tessera_si_off=array` records why (no user text). In plain words: bash
  5.1+ with bash-preexec 0.6 or later (atuin, for example, loads it), or
  with oh-my-bash, gets no shell integration. Placing our head and tail as
  elements of their own (r3 B2) left the user's indices shifted by one and
  edits of our slots running stale elements; that path is removed. At
  load, an array means nothing is installed and no handshake is sent
  (`an_array_prompt_command_is_left_alone`, the `off` rows of
  `bash_spec.txt`). Turned into one at a prompt, the next prompt takes the
  integration out: our text leaves every element, the marks leave `PS1`,
  our functions leave bash-preexec's lists, and our trap puts the user's
  back when it fires next; the command that made the array ends with
  `D;<status>` when our head still ran first and a bare `D` otherwise
  (`a_string_turned_into_an_array_is_left_alone`). bash-preexec 0.6 makes
  the array at its first prompt, after the handshake and that prompt's
  OSC 7: no `133` mark is sent. Known limit: a whole new array
  (`PROMPT_COMMAND=(...)`), like a whole new string, leaves nothing of ours
  to run, so it is not taken out: `PS1` keeps its `A` and `B` marks, and no
  `C` or `D` (so no status) is sent from then on.
- **An entry put before our head (#158 r4 B1).** `PROMPT_COMMAND="cmd;
  $PROMPT_COMMAND"` at a prompt, or a `.bashrc` that does this and is
  sourced again (`. ~/.bashrc`), runs `cmd` before the head reads `$?`. The
  head sends `D;<status>` only when it is the start of `PROMPT_COMMAND` (or
  bash-preexec's prologue, which keeps `$?`, is: #158 r5 B2); otherwise that
  command's `D` is bare. With bash-preexec, whether sourced before the
  script or at a prompt, the string is left as it is, so every later `D` is
  bare while the entry stays first
  (`an_entry_put_before_bash_preexec_never_sends_a_wrong_status`). The tail puts the head first again, and from the
  next prompt every command gets its own status
  (`an_entry_put_before_our_head_never_sends_a_wrong_status`, both repros
  on 3.2 and 5.3). Not covered, and tracked in #590: removing our head's
  line (`PROMPT_COMMAND=${PROMPT_COMMAND#*$'\n'}`) gives the user's entry a
  stale `$?` for one prompt and that command no `C`; removing our tail's
  last line (`PROMPT_COMMAND=${PROMPT_COMMAND%$'\n'*}`) sends two `D`s at
  every later prompt and runs the user's entries twice.
- **bash-preexec sourced at a prompt on bash 3.2 (#158 r3 B3).** Once a
  string `PROMPT_COMMAND` holds bash-preexec's `__bp_precmd_invoke_cmd`, the
  hooks no longer move head and tail around it (re-placing them each prompt
  made bash-preexec 0.7.0's sanitizer run on our text: 3.5 to 5.6 s per
  prompt). Head and tail stay where bash-preexec put them, the `B` mark
  comes from the `PS1` arm, and a prompt whose head did not run sends a bare
  `D`. The first command after `. bash-preexec.sh` runs while bash-preexec
  installs itself: no `C`, a bare `D`; every later command keeps its `C` and
  `D;<status>` (`bash_preexec_sourced_at_a_prompt_keeps_its_prompt_command`).
  Measured with bash-preexec 0.7.0 on 3.2 at load 10 to 20: 0.004 s per later
  prompt (0.002 s without Tessera); the source itself 1.4 s.
- **bash-preexec 0.5 and 0.6 sourced at a prompt.** Their install text,
  appended after our tail, saves `trap -p DEBUG` as the trap to keep and
  runs it as a preexec function. While that text is in `PROMPT_COMMAND`,
  our trap puts the user's trap back when it fires for it, so bash-preexec
  keeps the user's trap and never ours (ours, run from its preexec, would
  replace its trap and its preexec functions would never run again). The
  command after the source gets no `C` and a bare `D`, as above
  (`bash_preexec_sourced_at_a_prompt_runs_preexec_functions`, every bash and
  version). On bash 5.1+ 0.6 (and 0.7) make `PROMPT_COMMAND` an array at
  that prompt: the shell leaves the integration, and their preexec
  functions run as without Tessera.
- **direnv sourced again at a prompt (#158 r3 M6).** direnv's hook tests
  `";${PROMPT_COMMAND[*]};"` for `;_direnv_hook;`. On a string
  `PROMPT_COMMAND` our head is first and ends with a newline, so a second
  `eval "$(direnv hook bash)"` at a prompt does not see the first hook and
  adds another; `_direnv_hook` then runs twice per prompt (it is
  idempotent). Startup files are not affected: they run before the head is
  placed.

## Amendment: a start the injection takes over the exec limit runs without it (#519, 2026-11-24)

The variables (and, for bash, the arguments) the injection adds count
toward the `execve` size check (#74) like any others. An inherited
environment just under the limit (one 943 KB variable, say) therefore
started `zsh -i` with shell integration off and failed with it on: the
injection's 175 to 235 bytes took it over, and the refusal said nothing of
shell integration, so the user got no shell and no hint why.

**Decision (lead).** When the injected start is refused with
`PtyError::ArgumentsTooLong`, `App::spawn_pty` starts the same options once
more without the injection, and the session's first line says so:

```text
[Tessera] Shell integration is off for this session: the environment is too large (<bytes> bytes with it, more than the <max> allowed).
```

`<bytes>` and `<max>` are the refused start's footprint and the limit, as
the error gives them; the user learns how much to trim to get shell
integration back. The size check runs before anything is opened or forked,
so the refused start leaves nothing behind and the retry costs no fork.
One warning is logged with the two sizes as fields and nothing else: no
variable's name or value, no path, no shell name (which may come from
`$SHELL`; docs/security.md S8). The notice is fixed text and numbers.

- **Once.** The retry is the start integration off would make; there is no
  third start. When it fails, its error is returned, so a start too large
  without the injection too is refused exactly as with integration off
  (the same `bytes` and `max`), and a program the injection leaves alone
  (`sh`, `bash -c`, a preset `TESSERA_SHELL_INTEGRATION`) is refused as
  before, with no retry and no notice.
- **With a profile (#333).** The retry happens inside each start, so a
  profile whose start the injection takes over the limit runs on the
  profile without integration, with this notice only. A profile that falls
  back to the built-in one, whose start the injection also takes over the
  limit, gets both lines, the profile's first.
- **Not an error, not refused.** Refusing with a message naming shell
  integration was the alternative; it leaves the user without a shell for
  ~200 bytes of ours. The notice keeps the cause visible.
- On Linux, where one string over `MAX_ARG_STRLEN` is refused too, the
  sizes are that string's and its limit; Tessera ships for macOS only.

Tests: `a_start_the_injection_takes_over_the_exec_limit_runs_without_it`
(zsh's `ZDOTDIR` redirect and bash's `--rcfile`, the limit measured from a
refusal at run time, so no injection size is written into the test),
`a_program_without_integration_over_the_limit_is_refused_as_before`,
`a_profile_fallback_and_the_exec_limit_retry_each_say_so_once` (all in
`crates/tessera-app-core/tests/shell_integration.rs`) and
`the_exec_limit_retry_logs_the_sizes_only` (`lib.rs`, the S8 check).

The same task closes the `spawn_errno` minors of #526 (M3): a
`portable-pty` refusal (`Unable to spawn <path> because ...`) quotes the
program's path and `PATH`, and an entry like `x (os error 13)` chose the
fallback notice's `errno`. Only the fixed words after the path are read
now (`profile::lookup_errno`), so a path can pick none of its own; a `PATH`
entry with a line break can at most make up a candidate line, which picks
among `EISDIR`, `EACCES` and `ENOENT`, the three a search can give.

## Amendment: bash arrays are integrated with one element of ours (#613, 2026-11-25)

This amendment supersedes **Arrays are not integrated** (2026-11-22) above,
and the B5h exception stays: a login shell whose startup files make an
array is still left to bash.

- **Design.** On bash 5.1 and later an array `PROMPT_COMMAND` gets one
  element of ours, `_tessera_si_element && builtin eval "$_tessera_si_go"
  && _tessera_si_nop`. It is appended at index max(1, last index + 1), so it
  is never at index 0, where a scalar assignment (`PROMPT_COMMAND=f`, or
  starship's wrapping) would overwrite it. It is found by its exact text,
  never by its index, and it never moves (lead decisions D1, D3).
  - bash starts every element with the user's command's `$?` and
    PIPESTATUS. The element reads `$?` first and then:
    - closes the command (`D;<status>`, or a bare `D` when no C went out);
    - reports the directory;
    - puts A first and B last into `PS1`;
    - appends our arming piece to `PS0` when it is missing;
    - installs our trap at top level through the eval.
  - **PS0 arms the trap** (lead decision L1, 2026-11-25). bash expands PS0
    only for a command line it is about to run, after every element ran,
    so an empty line or Ctrl-C at the prompt arms nothing. The piece
    `${_tessera_si_none[_tessera_si_armed=1]-}` is found by its exact text
    and appended after the user's and starship's PS0 text, which is never
    rewritten or reordered; a command substitution before it runs unarmed.
    The element puts it back at each prompt when it is missing, so a PS0
    the user reassigns at a prompt costs no prompt: that line ran with the
    old PS0 (row PS0b). An element after ours that reassigns PS0 at every
    prompt leaves every command without C and with a bare D, never a
    wrong status. With `shopt -u promptvars` nothing expands the piece: no
    C, a bare D.
  - **Stdout copies** (adversarial M5). The copies of standard output for
    the C are opened only when a trap of ours is installed to close them.
    When our trap fires unarmed while the prompt shows and PS0 has lost the
    piece (or `promptvars` went off), it closes them and puts the user's
    trap back before the command runs, so no command or child inherits
    them (row `array_mode_leaks_no_stdout_copy`).
    - We first armed from PS1's expansion, with the command number `\#` to
      tell an empty line from a command. On bash 5.3, `\#` does not
      advance for the first command after PS0 is first set, so with
      starship the next command lost C and D. The command-number check is
      gone, and one design serves 5.1 and 5.3.
  - **Duplicates.** `PROMPT_COMMAND+=("${PROMPT_COMMAND[@]}")` (E9)
    yields two copies. The copy that closes the command counts our
    element's copies in the array, and the later copies at that prompt
    only put the marks back (lead decision L3, 2026-11-25). The first
    design counted prompts in an expansion inside PS1's B mark, which an
    element's `${PS1@P}` (prompt-width code) also ran, so the second copy
    sent a bare D after `D;<status>` and the status was lost (adversarial
    B1, row `a_doubled_ps1_width_element_keeps_one_d_with_the_status`).
    Limit: a Ctrl-C that stops `PROMPT_COMMAND` between two copies shifts
    the work to the next prompt's second copy; if the copies are then
    removed, that one prompt gets no D.
  - A startup array that already holds our element's text (pasted from
    `declare -p`) keeps that one copy and gets no second.
- **KEEP and DROP** (rows of `bash_rows.rs`
  `array_edits_keep_or_drop_the_integration`, on 5.3.20 and 5.1.16).
  - KEEP rows: every command's `D;<status>` equals the `$?` the user's
    elements see; every prompt has one A, B, C and OSC 7; the transcript
    equals the same shell without Tessera. Each row's edit:
    - E1/S4: none;
    - E2: `unset 'PROMPT_COMMAND[0]'`;
    - E3: `PROMPT_COMMAND[0]=f1`;
    - E4: `+=(u3)`;
    - E7/E8: append or prepend around ours;
    - E9: the array doubled;
    - E10: other in-place edits;
    - E11: the scalar `PROMPT_COMMAND=f1`;
    - E12: starship, which assigns element 0;
    - E12b: `PROMPT_COMMAND=()` at startup, with ours at index 1 under
      starship;
    - E15: `=("${PROMPT_COMMAND[@]}")`;
    - E16: `+=(u3 u3)`;
    - PS0a: a PS0 set at startup;
    - PS0b: PS0 reassigned at a prompt.
  - E17 (`set -o pipefail`, `false | true`) gives `D;1`.
  - DROP rows (D2: no heal): E5 `=(f1)`, E6 a whole new array, E13 `=()`
    and E14 unsetting our element by index. The edit line gets its C and no
    D. Later prompts keep A and B in `PS1` with no C or D, no trap of ours
    is left, and the user's elements run as without Tessera. Our PS0 piece
    stays and only sets a variable.
- **bash-preexec placement** (D4). Once bash-preexec is imported, at load
  or at a prompt, no trap of ours is set:
  - our D and C go into `precmd_functions` and `preexec_functions`;
  - the marks entry sits before `__bp_interactive_mode`, never after
    bash-preexec's epilogue;
  - our PS0 piece leaves PS0;
  - bash-preexec 0.7 on 5.3 keeps its own PS0 hook, and its array does not
    grow.
  - The counters of the 0.5/0.6/0.7 matrix equal the shell without
    Tessera.
  - Accepted deviation: with bash-preexec 0.5 and a user array, 0.5's
    fused epilogue runs its preexec for the user's element (its own B10
    limit), so those shells get bare A, B and D only, no C.
- **Spec rows.** `bash_spec.txt` rows of bash 5.1+ flip from `off` to
  marks. Accepted deviations:
  - X8g, X14f and X15e (bash-preexec, then oh-my-bash, `--rcfile`) leave
    the first prompt and its command unmarked, because oh-my-bash's element
    runs after the install in the array bash copied;
  - X16a (login) measured fully marked, where the spec predicted an
    unmarked first prompt.
- **First prompt** (D5). On the `--rcfile` route the script's last command
  returns the startup file's status, so the user's elements (and string
  entries) at the first prompt see the `$?` and PIPESTATUS of bash without
  Tessera. The status is the one a RETURN trap reads as `source` finishes
  (#659), which is bash's own: a startup file that ends in `return N`
  leaves the status before the `return`, as without Tessera. That trap
  also turns `set -e` off for bash's check of `source`'s status and back
  on after it, so a startup file that runs `set -e` and ends in a failing
  `&&` list or `return N` keeps the shell, as bash alone does, while
  `set -e` stays live inside the file (no `&&`, `||`, `!` or `if` around
  `source`). Limits (adversarial M1): PIPESTATUS is that one status: a
  final pipeline's `0 1 0` becomes `0`, a final `! cmd` leaves the negated
  status where bash keeps the command's own (measured on 5.1.16; 3.2 gives
  0), and on bash 3.2 PIPESTATUS is always `0`. After a syntax error in
  the file the status is `source`'s (bash's 2 on 5.x; 1 on 3.2, where bash
  alone has 258). A startup file that sets or clears its own RETURN trap
  replaces ours: the status is then `source`'s (`return N` gives `N`) and
  its `set -e` exits the shell at those endings, as before #659. While
  the file runs, `trap -p` in it shows our RETURN trap (bash without
  Tessera has none there).
- **Readonly array** (adversarial M2). A startup file that makes
  `PROMPT_COMMAND` a readonly array leaves the shell to bash, as before
  arrays were integrated: no error at startup, and the first prompt's
  elements see the startup file's status (row
  `a_readonly_array_is_left_to_bash`).
- **`shopt -u promptvars`** (#613 r3). bash would print our PS0 piece
  instead of expanding it, so the piece leaves PS0 and no trap of ours is
  armed. The user's DEBUG trap keeps running, and each command has no C and
  a bare D. Limit: an element after ours that turns `promptvars` off does
  so after our element ran, so that one prompt prints our B-mark and PS0
  pieces as text, and the user's trap misses that one command.
- **From no `PROMPT_COMMAND`.** `PROMPT_COMMAND+=(f1)` at a prompt, with
  no `PROMPT_COMMAND` at startup, gives `([1]=f1 [2]=ours)`. Element 0 held
  only our head and tail, so it goes. Bash without Tessera has `([0]=f1)`,
  and `f1` runs as often. This is not a regression.
- **Whole-array limit.** Our element lives in the user's array. Any
  assignment that replaces the whole array without carrying it over, or
  unsets it, ends the integration for the session (DROP above).
- **Cost.** We counted CPU (user + sys) of 150 prompts of `:` on the
  `--rcfile` route, best of 5 interleaved runs at load 5 to 7. The
  3-element array `(u1 u2 u3)` was compared with the string `'u1; u2; u3'`:
  - bash 5.3.20: 214.7 ms against 437.5 ms, ratio 0.49;
  - bash 5.1.16: 180.3 ms against 354.4 ms, ratio 0.51.
  The array route does less work per prompt than placing head and tail in
  a string; the bar was 1.10.
- **Security** (docs/security.md S5, S7, S8). There is no new buffer, queue
  or table, and nothing from child output is read or evaluated. The eval
  runs only `_tessera_si_go`, which our element sets to our own trap text.
  Arrays are never exported, so `bash -c env` shows nothing of ours (S8,
  checked at the end of every edit row).
- `SCRIPTS_VERSION` is 13.
- bash 5.1 rows run when `TESSERA_TEST_BASH51` names a bash 5.1. Without
  it, they print `bash 5.1 part skipped: TESSERA_TEST_BASH51 is unset`.

## Amendment: the `--rcfile` route's startup file ends as without Tessera (#659, 2026-11-25)

The script sources the user's `.bashrc` with a plain `builtin source`, so
the status `source` returns went through bash's `set -e` check, where bash
reading the file itself checks nothing: a `.bashrc` that ran `set -e` and
ended in a failing `&&` list (or `return N` with N > 0) exited the shell at
once. A RETURN trap, set just before the `source` and removed by itself,
now runs as the file finishes: it reads bash's own status of the file and
turns `set -e` off for the check; the script turns it back on right after.
`source` gets no `&&`, `||` or `if` around it, which on bash 5 would run the
file with `set -e` off. The D5 paragraph of the #613 amendment above
carries the new status rule and its limits.

- Rows `rcexit` E1-E13 in `bash_spec.txt` (tests/bash_rows.rs) compare the
  injected shell with bash alone on 3.2, 5.1 (`TESSERA_TEST_BASH51`) and
  5.3: the shell survives such endings, still exits at a failing command
  inside the file (also inside a function it calls), and the first
  prompt's entry and the first command see bash's status. E10 (the file's
  own RETURN trap) and E13 (a syntax error on 3.2) are accepted limits.
- `SCRIPTS_VERSION` is 14.

## Amendment: a `.bashrc` that sources the script itself (#676, 2026-11-25)

On the `--rcfile` route the script set the startup file's status variable
before it sourced the file, and defined `_tessera_si_rc_end` only after.
A `.bashrc` that sources `tessera.bash` itself ran the script's end with
that variable set and no function yet: `_tessera_si_rc_end: command not
found` ended that `source` with 127, which exited a `set -e` shell. The
variable is now set once the file ran (0 when there is none to read), so
the script sourced from inside the file does none of the route's end.
What that load does is unchanged: with the file's own
`TESSERA_SHELL_INTEGRATION` it is the live integration (its words hold)
and the route's load after the file is skipped; without, the injected
words load it after the file.

- Rows `rcexit` E15-E18 cover the source with and without the file's own
  words, followed by a command, by a failing `&&` list, or as the file's
  last line; the first prompt's entry and the first command see bash's
  status, and no `_tessera_si_rc_` variable, function or RETURN trap is
  left. `a_bashrc_that_sources_the_script_keeps_the_marks` checks one
  handshake, a C and D for every command and the file's words;
  `a_missing_startup_file_leaves_nothing_of_the_rc_end` the route without
  a startup file.
- `SCRIPTS_VERSION` is 15.

## Amendment: a Ctrl-C while the `.bashrc` runs (#713, 2026-11-26)

On the `--rcfile` route a Ctrl-C while the startup file ran ended the
rest of bash's startup, which here is the rest of `tessera.bash`: our
RETURN trap and `_tessera_si_rc*` state stayed (the trap then fired at
the user's next `source` and could turn `set -e` off), and the shell got
no integration. The script now sets an INT trap for the file's `source`
only. On a Ctrl-C it returns from the file with 130, so the rest of the
script runs as after any other file: the route's end cleans up and the
integration loads; the first prompt sees 130. The trap acts only while
the file runs (a Ctrl-C in the rest of the script is dropped), and the
route's last command removes it unless the file set its own (bash alone
keeps that one too). It is not removed right after the file: bash 3.2
runs startup files as a non-interactive shell, and `trap - INT` there
makes SIGINT end the shell until bash puts its interactive handler back
after the startup files, so a Ctrl-C in the rest of the script closed
the tab. Removing it at the first prompt instead does not help on 3.2:
`PROMPT_COMMAND` and traps run non-interactive too, and the handler then
stays the terminating one through the first command. Nothing of the
user's runs after the removal: the route's last command is a call with
`set -T` off (put back inside it before the trap goes), so a DEBUG or
RETURN trap of the file's runs before the call, with our trap still in
place, and not inside it, and with status 0 no command follows the call
(with any other status the `&&` list that keeps `set -e` off ends
there). What is left is the time between that call's removal and bash's
own reader loop (bash internals and loading the history, no shell code),
where a Ctrl-C still ends a 3.2 shell.
A trap was chosen over aborting the startup from inside the handler:
bash 3.2 runs trap handlers as a non-interactive shell, so a handler
that resets SIGINT and sends it again kills the shell there.

- Rows `rcexit` E19 (no trap, variable or function left; 3.2, 5.1, 5.3),
  E20 (the first prompt's entry and the first command see 130, as
  bash alone; 5.1, 5.3), E23 (a Ctrl-C after the file returned keeps
  the shell; 3.2, 5.1, 5.3) and E24, E25 (a Ctrl-C in the file's DEBUG
  trap at the first command it sees without our trap, the first prompt;
  status 0, and 1 under `set -T`; 3.2, 5.1, 5.3) cover it.
  `a_ctrl_c_in_the_bashrc_keeps_the_marks` checks the handshake and the
  marks after it.
- ACCEPTED LIMIT (E21): inside a function the file called, the Ctrl-C
  returns from that function and the file goes on, where bash alone drops
  the rest of the file. After a loop the first prompt sees 130 where
  bash 3.2 alone shows 128 (E22).
- `SCRIPTS_VERSION` is 16.

## Amendment: a DEBUG trap in the INT trap check (#726, 2026-11-26)

The `--rcfile` route's check that our INT trap is still in place after
the `.bashrc` read `$(builtin trap -p INT)`. Under `set -T`, or extdebug
(which turns `set -T` on), a DEBUG trap of the file's runs in that
command substitution before `trap -p`. What it wrote to standard output
came first in the read, so the match failed and our INT trap stayed in
the interactive shell; reading through descriptor 3 instead (the form
the prompt's DEBUG trap read uses) still takes what such a trap writes
to descriptor 3, and under extdebug a trap's non-zero status skips
`trap -p` in the fork. The check now turns `set -T` off for the read
and back on right after it (extdebug itself stays on), so no trap of the
user's runs in the fork. The script's only other command substitution
of `trap -p`, the prompt's DEBUG trap read, keeps the descriptor 3 form
(not changed here).

- Rows `rcexit` E26 and E27 (a trap writing to standard output, under
  `set -T` and extdebug), E28 and E29 (writing to descriptor 3) and E30
  (extdebug, non-zero status), on 3.2, 5.1 and 5.3, check that no INT
  trap is left and that the startup status, `set -T` and extdebug are as
  bash alone leaves them.
- `SCRIPTS_VERSION` is 17.

## Amendment: `set -x` in the INT trap check (#733, 2026-11-26)

Under `set -x` with `BASH_XTRACEFD=1` (bash 4.1 and later; 3.2 always
traces to standard error) the fork of the same check traced
`builtin trap -p INT` to its standard output, the read, so the match
failed and our INT trap stayed in the interactive shell, as in #726. The
check now also turns `set -x` off for the read, first, and back on last,
after `set -T`: neither the `set -T` switch nor the restore adds a trace
line (the user sees the trace of `[[ -o xtrace ]]`, `_tessera_si_rc_x=1`
and `builtin set +x` in place of the check's own). With the trace on standard error or another
descriptor nothing reached the read; the rows check that `set -x` comes
back there too.

The prompt's DEBUG trap read (`{ builtin trap -p DEBUG >&3; } 3>&1`)
takes the trace when `BASH_XTRACEFD` is 3: a DEBUG trap the user sets at
a prompt under `set -x` is then lost (bash alone runs it). It is not
changed here (a per-prompt path, recorded on #733); `BASH_XTRACEFD=1`
is safe there, as standard output is /dev/null for that read.

- Rows `rcexit` E31 (`BASH_XTRACEFD=1`), E32 (standard error), E33
  (`BASH_XTRACEFD=1` with `set -T` and a DEBUG trap) and E34
  (`BASH_XTRACEFD=5`, a file) check that no INT trap and no
  `_tessera_si_rc_` variable is left and that `$-` is as bash alone
  leaves it. E35 (`BASH_XTRACEFD=5` with `set -T` and a DEBUG trap)
  checks that the check's trace has no `set -T`, `set +T` or `trap -p`
  line. The rows run on 3.2 and 5.3; 5.1 only where
  `TESSERA_TEST_BASH51` names one, which CI does not set.
- `SCRIPTS_VERSION` is 18.

## Amendment: `set -x` in the prompt's DEBUG trap read (#740, 2026-11-26)

The head reads the DEBUG trap in place at every prompt with
`{ builtin trap -p DEBUG >&3; } 3>&1 1>/dev/null` in a command
substitution. Under `set -x` with `BASH_XTRACEFD=3` (bash 4.1 and later)
the fork traced `builtin trap -p DEBUG` to descriptor 3, the read, so a
DEBUG trap the user set at a prompt was taken for something else and
lost (bash alone runs it), as recorded in the #733 amendment. The head
now turns `set -x` off for the read and back on right after it, before
our trap is installed; run while it is off, `builtin set -x` adds no
trace line (the user sees the trace of `[[ -o xtrace ]]`, the head's
assignment and `builtin set +x` in place of the read's own). The test
runs in `_tessera_si_precmd` (string `PROMPT_COMMAND`) or
`_tessera_si_element` (array), which picks the head text for the
prompt, `_tessera_si_h_x` or `_tessera_si_h_full`: inside our function
no DEBUG trap of the user's runs for it without `set -T` (under
`set -T` it runs there too, as for our other functions), so with
`set -x` off the one `[[ -o xtrace || -n $_tessera_si_xoff ]]` test is
the whole cost (about 0.5 µs per call on 3.2 and 5.3; per-prompt time
unchanged against main within noise). A head stopped between the
switches (a Ctrl-C in the read's fork, which under `set -T` lasts as
long as the user's trap takes there) leaves `_tessera_si_xoff` set: the
next prompt's test turns `set -x` back on first (bash runs
PROMPT_COMMAND again right after such a Ctrl-C). The flag is set before
`set +x` and cleared only after `set -x`: the user's DEBUG trap runs
before each of the head's commands, and a Ctrl-C while it runs before
the restore stops the head there, so `set -x` is never off with the flag
empty (#740 B1). Clearing it adds one trace line per prompt under
`set -x`, `_tessera_si_xoff=`, after the restore. With `set -x` on,
the user's DEBUG trap also runs for the two switches, as it does for the
head's other first commands (P17).

- Rows `rcexit` E36 (`BASH_XTRACEFD=3`), E37 (1) and E38 (2) set a DEBUG
  trap at a prompt under `set -x` and check that it runs for the next
  command and that `$-` is as bash alone leaves it; E39 is E36 with an
  array `PROMPT_COMMAND`; E40 (`BASH_XTRACEFD=5`, a file) checks that
  the trace has no line of our `builtin trap -p` or `builtin set -x`
  and traces the user's `set +x` at both prompts, as bash alone.
  `a_ctrl_c_in_the_heads_read_keeps_set_x` (5.3) stops the head in the
  read's fork, and `a_ctrl_c_before_the_heads_restore_keeps_set_x`
  (5.3, string and array) in the user's DEBUG trap right before the
  head's `builtin set -x`; both check that `set -x` is back and the flag
  cleared.
  E36, E39 and E40 failed before the fix on 5.3; E37 and E38 passed
  then too (standard output is /dev/null for the read, standard error is
  not read). E36 to E38 run on 3.2 and 5.3, E39 and E40 on 5.3 (5.1 where
  `TESSERA_TEST_BASH51` names one).
- `SCRIPTS_VERSION` is 19.
