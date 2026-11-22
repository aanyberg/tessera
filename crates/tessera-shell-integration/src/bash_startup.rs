//! Whether a bash that would get the `PROMPT_COMMAND` loader keeps it.
//!
//! The loader reaches bash as an exported `PROMPT_COMMAND`. A startup file
//! that assigns the variable removes the loader but keeps the export
//! attribute, so the user's value reaches every child: a child bash or `sh`
//! that lacks the function it names prints an error at each prompt. No text
//! of ours is left to guard that, so such a bash is started as if Tessera
//! knew nothing about shells. This module decides that from the files bash
//! would read, without running anything:
//!
//! - a login bash: the first of `~/.bash_profile`, `~/.bash_login` and
//!   `~/.profile` that exists, and `~/.bashrc`;
//! - a `--posix` bash: `$ENV` (bash 5), and `~/.bashrc` unless `--norc` (the
//!   macOS bash 3.2 reads that instead);
//! - from those, one level of `.`/`source` whose argument is a literal `~/`,
//!   `$HOME` or absolute path.
//!
//! The answer errs towards leaving bash alone: a false alarm costs the
//! integration of one shell, a miss prints errors at its children's prompts.
//! An assignment behind a computed `source` path or inside `eval` is not
//! seen, nor are the files in `/etc`.
//!
//! The scan also picks what follows the loader ([`Terminator`]): the files'
//! appends land right after its last word. An append that starts with a
//! separator (`PROMPT_COMMAND="$PROMPT_COMMAND; x"`) needs nothing there, one
//! glued to it (`PROMPT_COMMAND+='x'`, `"$PROMPT_COMMAND x"`) a newline. Files
//! with both kinds, and any array assignment (bash 3.2 never runs element 1),
//! leave bash alone.
//!
//! The files are untrusted, and spawning runs on the app's main thread: see
//! [`MAX_FILE_BYTES`], [`MAX_FILES`], [`MAX_SCANS`] and [`WAIT_SLICES`].

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use std::time::Duration;

/// The largest startup file that is read. A larger one counts as unknown.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// The most distinct paths one scan opens or queues; more counts as unknown.
const MAX_FILES: usize = 32;
/// The most scans alive at once. A scan stuck on a stalled disk outlives the
/// wait for it, so without a limit each new shell would leave a thread.
const MAX_SCANS: usize = 8;
/// How long one wait for the scan lasts.
const WAIT_SLICE: Duration = Duration::from_millis(10);
/// How many [`WAIT_SLICE`]s the caller (the main thread) waits for the scan
/// before starting bash without the loader: a second, and longer when the
/// machine is so busy that the slices stretch, as the scan's do.
const WAIT_SLICES: u32 = 100;
/// The longest `[...]` after the variable's name that is read as a subscript.
const MAX_SUBSCRIPT: usize = 256;
/// How far back from the variable its statement's start is looked for.
const MAX_LOOKBACK: usize = 64;

const VARIABLE: &[u8] = b"PROMPT_COMMAND";
/// Words that may stand before a statement's command.
const PREFIX_WORDS: [&[u8]; 11] = [
    b"then", b"do", b"else", b"elif", b"if", b"while", b"until", b"{", b"!", b"builtin", b"command",
];
/// What ends a statement on a line.
const STATEMENT_ENDS: &[u8] = b";&|()";
/// Names that tools keeping the entries already in `PROMPT_COMMAND` define,
/// though they assign it: bash-preexec (also when embedded in another tool's
/// script) and a cached `starship init bash`. Only a definition counts (see
/// [`defines`]), not a mention in a comment, a string or a heredoc.
const KEEPING_TOOLS: [&[u8]; 2] = [b"__bp_install_string", b"starship_precmd"];

/// The scans alive in this process.
static SCANS: AtomicUsize = AtomicUsize::new(0);

/// How bash is started, as far as it decides the files it reads.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Startup {
    /// A login shell.
    pub(crate) login: bool,
    /// `--posix`: `$ENV` is read, or on bash 3.2 `~/.bashrc`.
    pub(crate) posix: bool,
    /// `--norc`: `~/.bashrc` is not read by a `--posix` shell.
    pub(crate) norc: bool,
}

/// What follows the loader in `PROMPT_COMMAND`, so that the startup files'
/// appends, which land right after its last word, still run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Terminator {
    /// Nothing: every append starts with a separator (`;`, `&&`, `||`).
    Nothing,
    /// A newline: an append is glued to the loader's last word, or none was
    /// seen.
    Newline,
}

/// Whether the loader can be given to a bash started as `startup`, and what
/// must follow it: `None` when a startup file replaces `PROMPT_COMMAND`,
/// assigns it an array, appends to it both with and without a separator, or
/// when that cannot be ruled out in time. `user_env` is the environment bash
/// would get.
pub(crate) fn loader_terminator(
    startup: Startup,
    user_env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<Terminator> {
    let files = StartupFiles::new(startup, user_env)?;
    within_deadline(&SCANS, WAIT_SLICES, move || files.scan().ok())
}

/// Runs `work` on its own thread and returns its answer, or `None` when it
/// does not answer within `slices` waits, cannot start, panics, or
/// [`MAX_SCANS`] runs are already alive in `running`.
fn within_deadline<T: Send + 'static>(
    running: &'static AtomicUsize,
    slices: u32,
    work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    struct Running(&'static AtomicUsize);
    impl Drop for Running {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let guard = Running(running);
    if running.fetch_add(1, Ordering::AcqRel) >= MAX_SCANS {
        return None;
    }
    let (sender, receiver) = sync_channel(1);
    // If the thread cannot start, the sender is gone and the wait ends at once.
    drop(
        thread::Builder::new()
            .name("bash-startup-scan".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let _guard = guard;
                // The caller may have stopped waiting.
                let _ = sender.send(work());
            }),
    );
    for _ in 0..slices {
        match receiver.recv_timeout(WAIT_SLICE) {
            Ok(answer) => return answer,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
    None
}

/// The scan's verdict that the loader must not be given.
#[derive(Debug, PartialEq)]
struct LeaveAlone;

/// How an append to `PROMPT_COMMAND` starts, as seen from the loader's last
/// word that it follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lead {
    /// `;`, `&` or `|` first: works with no [`Terminator`], not after a newline.
    Separator,
    /// Anything else, a blank included: needs the newline.
    Glued,
    /// A newline first, or nothing appended: works either way.
    Neutral,
}

/// The kinds of appends one scan saw.
#[derive(Debug, Default)]
struct Appends {
    separated: bool,
    glued: bool,
}

impl Appends {
    fn note(&mut self, lead: Lead) {
        match lead {
            Lead::Separator => self.separated = true,
            Lead::Glued => self.glued = true,
            Lead::Neutral => {}
        }
    }

    fn terminator(&self) -> Result<Terminator, LeaveAlone> {
        match (self.separated, self.glued) {
            (true, true) => Err(LeaveAlone),
            (true, false) => Ok(Terminator::Nothing),
            (false, _) => Ok(Terminator::Newline),
        }
    }
}

/// The files bash reads first for one launch.
#[derive(Debug, Default)]
struct StartupFiles {
    home: Option<PathBuf>,
    /// Bash reads the first of these that exists.
    first_of: Vec<PathBuf>,
    /// Read as well.
    also: Vec<PathBuf>,
}

impl StartupFiles {
    /// `None` when the files cannot be named: `~/.bashrc` or a login file
    /// without an absolute `HOME`, or an `ENV` that is not a literal path.
    fn new(startup: Startup, user_env: &dyn Fn(&str) -> Option<OsString>) -> Option<Self> {
        let home = user_env("HOME")
            .filter(|home| home.as_bytes().first() == Some(&b'/'))
            .map(PathBuf::from);
        let mut files = Self::default();
        if startup.login {
            let home = home.as_ref()?;
            let names = [".bash_profile", ".bash_login", ".profile"].map(|name| home.join(name));
            if startup.posix {
                // A POSIX-mode login bash reads only `~/.profile`; 3.2 and
                // 5.x differ, so every one is checked.
                files.also.extend(names);
            } else {
                files.first_of = names.into();
            }
        }
        if startup.login || (startup.posix && !startup.norc) {
            files.also.push(home.as_ref()?.join(".bashrc"));
        }
        if startup.posix
            && let Some(env) = user_env("ENV").filter(|value| !value.is_empty())
        {
            let value = env.as_bytes();
            let (path, used) = literal_path(value, home.as_deref())?;
            if used != value.len() {
                return None;
            }
            files.also.push(path);
        }
        files.home = home;
        Some(files)
    }

    /// Reads the files and the ones they source.
    fn scan(&self) -> Result<Terminator, LeaveAlone> {
        let mut walk = Walk {
            home: self.home.as_deref(),
            visited: Vec::new(),
            sourced: Vec::new(),
            appends: Appends::default(),
        };
        for path in &self.first_of {
            if walk.visit_startup_file(path)? {
                break;
            }
        }
        for path in &self.also {
            walk.visit_startup_file(path)?;
        }
        for path in std::mem::take(&mut walk.sourced) {
            walk.visit_sourced_file(&path)?;
        }
        walk.appends.terminator()
    }
}

/// One scan's state.
struct Walk<'a> {
    home: Option<&'a Path>,
    visited: Vec<PathBuf>,
    sourced: Vec<PathBuf>,
    appends: Appends,
}

impl Walk<'_> {
    /// Reads `path` unless this scan already did; `None` when it does not
    /// exist or was read before.
    fn read(&mut self, path: &Path) -> Result<Option<Vec<u8>>, LeaveAlone> {
        if self.visited.iter().any(|seen| seen == path) {
            return Ok(None);
        }
        if self.visited.len() == MAX_FILES {
            return Err(LeaveAlone);
        }
        self.visited.push(path.to_path_buf());
        read_startup_file(path)
    }

    /// Checks a file bash reads itself and notes what it sources. Returns
    /// whether it exists.
    fn visit_startup_file(&mut self, path: &Path) -> Result<bool, LeaveAlone> {
        let Some(text) = self.read(path)? else {
            return Ok(false);
        };
        if uses_replace(&text, &mut self.appends) {
            return Err(LeaveAlone);
        }
        sourced_paths(&text, self.home, &mut self.sourced)?;
        Ok(true)
    }

    /// Checks a file a startup file sources.
    fn visit_sourced_file(&mut self, path: &Path) -> Result<(), LeaveAlone> {
        let Some(text) = self.read(path)? else {
            return Ok(());
        };
        // A tool that keeps the entries is not classified: its own appends
        // run before the loader's text is reached or after it is gone.
        let keeps_entries = KEEPING_TOOLS.iter().any(|tool| defines(&text, tool));
        if !keeps_entries && uses_replace(&text, &mut self.appends) {
            return Err(LeaveAlone);
        }
        Ok(())
    }
}

/// The contents of the regular file `path`; `None` when there is no such
/// file. Anything else (a FIFO, a device, a directory, also behind a link)
/// is never opened: opening a FIFO would count as its reader and take a
/// one-shot writer's text from bash. The open file is checked again (still
/// the same regular file), so a swap between the two is not read either.
fn read_startup_file(path: &Path) -> Result<Option<Vec<u8>>, LeaveAlone> {
    let named = match fs::metadata(path) {
        Ok(named) => named,
        Err(err) if is_missing(&err) => return Ok(None),
        Err(_) => return Err(LeaveAlone),
    };
    if !named.is_file() {
        return Err(LeaveAlone);
    }
    #[cfg(test)]
    tests::between_stat_and_open(path);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| LeaveAlone)?;
    let opened = file.metadata().map_err(|_| LeaveAlone)?;
    if opened.dev() != named.dev() || opened.ino() != named.ino() {
        return Err(LeaveAlone);
    }
    read_regular(file).map(Some).ok_or(LeaveAlone)
}

fn is_missing(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ENOTDIR)
}

/// The contents of `file` if it is a regular file within [`MAX_FILE_BYTES`].
fn read_regular(file: File) -> Option<Vec<u8>> {
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    read_capped(file)
}

/// All of `source` if it holds at most [`MAX_FILE_BYTES`], reading one byte
/// more than that at most: the file can grow after it was opened.
fn read_capped(source: impl Read) -> Option<Vec<u8>> {
    let mut text = Vec::new();
    source
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut text)
        .ok()?;
    (text.len() as u64 <= MAX_FILE_BYTES).then_some(text)
}

/// Whether `text` defines `name` as a function (`name()`, `function name`)
/// or assigns it (`name=`) at the start of a line outside a heredoc body.
fn defines(text: &[u8], name: &[u8]) -> bool {
    let mut heredoc: Option<(Vec<u8>, bool)> = None;
    for line in text.split(|&byte| byte == b'\n') {
        if let Some((word, tabs)) = &heredoc {
            let end = if *tabs { line.trim_ascii_start() } else { line };
            if end.trim_ascii_end() == word.as_slice() {
                heredoc = None;
            }
            continue;
        }
        let start = line.trim_ascii_start();
        let (keyword, statement) = match start.strip_prefix(b"function") {
            Some(rest) if rest.first().is_some_and(u8::is_ascii_whitespace) => {
                (true, rest.trim_ascii_start())
            }
            _ => (false, start),
        };
        if let Some(rest) = statement.strip_prefix(name) {
            let next = rest.trim_ascii_start().first();
            if rest.first() == Some(&b'=')
                || next == Some(&b'(')
                || (keyword && next.is_none_or(|&byte| byte == b'{'))
            {
                return true;
            }
        }
        heredoc = heredoc_start(line);
    }
    false
}

/// The delimiter of a heredoc that `line` starts, and whether it is `<<-`.
fn heredoc_start(line: &[u8]) -> Option<(Vec<u8>, bool)> {
    let at = line.windows(2).position(|pair| pair == b"<<")?;
    let rest = &line[at + 2..];
    if rest.first() == Some(&b'<') {
        return None;
    }
    let tabs = rest.first() == Some(&b'-');
    let rest = rest[usize::from(tabs)..].trim_ascii_start();
    let word: Vec<u8> = rest
        .iter()
        .copied()
        .take_while(|byte| !byte.is_ascii_whitespace() && !b";&|<>()".contains(byte))
        .filter(|byte| !b"'\"\\".contains(byte))
        .collect();
    (!word.is_empty()).then_some((word, tabs))
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether the variable's name, ending at `end`, is not part of a longer word.
fn ends_word(text: &[u8], end: usize) -> bool {
    !text.get(end).copied().is_some_and(is_word_byte)
}

/// The next whole-word use of the variable's name at or after `from`.
fn next_use(text: &[u8], mut from: usize) -> Option<usize> {
    while let Some(found) = text
        .get(from..)?
        .windows(VARIABLE.len())
        .position(|window| window == VARIABLE)
    {
        let at = from + found;
        let starts_word = at == 0 || !is_word_byte(text[at - 1]);
        if starts_word && ends_word(text, at + VARIABLE.len()) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Whether the line starting `line` is a comment.
fn is_comment_line(line: &[u8]) -> bool {
    line.iter().find(|byte| !matches!(byte, b' ' | b'\t')) == Some(&b'#')
}

/// Follows the scan through `text`, knowing whether its position is on a
/// comment line. Every byte is looked at once, however long the lines.
struct CommentLines {
    scanned: usize,
    comment: bool,
}

impl CommentLines {
    fn new(text: &[u8]) -> Self {
        Self {
            scanned: 0,
            comment: is_comment_line(text),
        }
    }

    fn at(&mut self, text: &[u8], position: usize) -> bool {
        let passed = &text[self.scanned..position];
        if let Some(newline) = passed.iter().rposition(|&byte| byte == b'\n') {
            self.comment = is_comment_line(&text[self.scanned + newline + 1..]);
        }
        self.scanned = position;
        self.comment
    }
}

/// Whether `text` may set `PROMPT_COMMAND` to something that lost its old
/// value or holds an array; see [`uses_replace`].
#[cfg(test)]
fn replaces_prompt_command(text: &[u8]) -> bool {
    uses_replace(text, &mut Appends::default())
}

/// Whether `text` may set `PROMPT_COMMAND` to something that lost its old
/// value or holds an array (bash 3.2 runs only element 0, so an element
/// appended to the loader's string never runs). Kept, and noted in `appends`
/// by how they start after the old value: `+=` and an assignment whose value
/// expands the variable. Also kept: reads, comments, and a bare
/// `export`/`unset`. Anything else that names the variable counts, which
/// includes a name in a string or a heredoc: a false alarm is the cheap error.
///
/// There is no quoting state across statements, so an apostrophe in a heredoc
/// or a comment cannot hide what follows it.
fn uses_replace(text: &[u8], appends: &mut Appends) -> bool {
    let mut comments = CommentLines::new(text);
    let mut from = 0;
    while let Some(at) = next_use(text, from) {
        let end = at + VARIABLE.len();
        from = end;
        if comments.at(text, at) || is_read(text, at) {
            continue;
        }
        let mut after = end;
        let subscript = text.get(end) == Some(&b'[');
        if subscript {
            let Some(close) = subscript_end(text, end) else {
                return true;
            };
            after = close;
        }
        let rest = &text[after..];
        let value = if rest.starts_with(b"+=") {
            Some((after + 2, true))
        } else if rest.first() == Some(&b'=') {
            Some((after + 1, false))
        } else {
            None
        };
        let Some((value, appended)) = value else {
            if subscript || !is_harmless_word(text, at, end) {
                return true;
            }
            continue;
        };
        if subscript || text.get(value) == Some(&b'(') {
            return true;
        }
        if appended {
            appends.note(lead_at(text, value, false));
            from = value;
        } else if let Some((resume, in_double)) = value_keeps_old(text, value) {
            appends.note(lead_at(text, resume, in_double));
            from = resume;
        } else {
            return true;
        }
    }
    false
}

/// How the text at `at` in a value continues the old value of the variable:
/// `in_double` tells whether `at` is inside double quotes or a `${...}`. A `}` closing the
/// reference, and quotes that open or close, are passed over.
fn lead_at(text: &[u8], mut at: usize, mut in_double: bool) -> Lead {
    if text.get(at) == Some(&b'}') {
        at += 1;
    }
    loop {
        let Some(&byte) = text.get(at) else {
            return Lead::Neutral;
        };
        match byte {
            b'"' => in_double = !in_double,
            b'$' => return dollar_lead(text, at, in_double),
            _ if in_double => return quoted_lead(&text[at..]),
            b'\'' => {
                return match text.get(at + 1) {
                    Some(b'\'') | None => Lead::Neutral,
                    Some(_) => quoted_lead(&text[at + 1..]),
                };
            }
            b' ' | b'\t' | b'\r' | b'\n' | b';' | b'&' | b'|' | b')' => return Lead::Neutral,
            _ => return Lead::Glued,
        }
        at += 1;
    }
}

/// How quoted text starting at `rest` goes on from the loader: blanks before
/// a separator do not glue (`. x 2>/dev/null && y` runs both).
fn quoted_lead(rest: &[u8]) -> Lead {
    let first = rest.iter().position(|byte| !matches!(byte, b' ' | b'\t'));
    match first.map(|at| rest[at]) {
        Some(b';' | b'&' | b'|') => Lead::Separator,
        Some(b'\n') if first == Some(0) => Lead::Neutral,
        _ => Lead::Glued,
    }
}

/// [`lead_at`] for a `$` at `at`: `$'\n...'` starts with a newline,
/// `${PROMPT_COMMAND:+x}` with what `x` starts with (the old value is not
/// empty: it holds the loader), anything else is glued.
fn dollar_lead(text: &[u8], at: usize, in_double: bool) -> Lead {
    let rest = &text[at + 1..];
    if !in_double && let Some(quoted) = rest.strip_prefix(b"'") {
        return match quoted.first() {
            Some(b'\\') if quoted.get(1) == Some(&b'n') => Lead::Neutral,
            Some(b'\'') | None => Lead::Neutral,
            Some(_) => quoted_lead(quoted),
        };
    }
    let Some(name) = rest.strip_prefix(b"{") else {
        return Lead::Glued;
    };
    let Some(operator) = name.strip_prefix(VARIABLE) else {
        return Lead::Glued;
    };
    let skip = if operator.starts_with(b":+") {
        2
    } else if operator.starts_with(b"+") {
        1
    } else {
        return Lead::Glued;
    };
    let alternative = at + 2 + VARIABLE.len() + skip;
    let old_value = text[alternative..]
        .strip_prefix(b"$")
        .map(|name| name.strip_prefix(b"{").unwrap_or(name))
        .is_some_and(|name| name.starts_with(VARIABLE));
    if old_value {
        // `${PROMPT_COMMAND:+$PROMPT_COMMAND...}` is the old value itself:
        // the scan reaches that reference next.
        Lead::Neutral
    } else {
        lead_at(text, alternative, true)
    }
}

/// Whether the name at `at` follows `$`, `${`, `${#` or `${!`.
fn is_read(text: &[u8], at: usize) -> bool {
    let before = &text[..at];
    before.ends_with(b"$")
        || before.ends_with(b"${")
        || before.ends_with(b"${#")
        || before.ends_with(b"${!")
}

/// The index after the `]` closing the `[` at `open`, on the same line.
fn subscript_end(text: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (offset, &byte) in text[open..].iter().enumerate().take(MAX_SUBSCRIPT) {
        match byte {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset + 1);
                }
            }
            b'\n' => return None,
            _ => {}
        }
    }
    None
}

/// Whether the bare name at `at..end` is an argument of `export` or `unset`.
fn is_harmless_word(text: &[u8], at: usize, end: usize) -> bool {
    let terminated = text
        .get(end)
        .is_none_or(|byte| b" \t\r\n;&|)".contains(byte));
    if !terminated {
        return false;
    }
    let window_start = at.saturating_sub(MAX_LOOKBACK);
    let window = &text[window_start..at];
    let statement = window
        .iter()
        .rposition(|byte| *byte == b'\n' || STATEMENT_ENDS.contains(byte))
        .map(|end_of_previous| &window[end_of_previous + 1..])
        .or((window_start == 0).then_some(window));
    let command = statement.and_then(command_word).map(|(word, _)| word);
    matches!(command, Some(b"export" | b"unset"))
}

/// The first word of `statement` that is not a [`PREFIX_WORDS`] entry, and
/// what follows it.
fn command_word(statement: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut rest = statement.trim_ascii_start();
    loop {
        let end = rest
            .iter()
            .position(u8::is_ascii_whitespace)
            .unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        let tail = tail.trim_ascii_start();
        if word.is_empty() {
            return None;
        }
        if !PREFIX_WORDS.contains(&word) {
            return Some((word, tail));
        }
        rest = tail;
    }
}

/// Whether the value starting at `start` expands the variable, so its old
/// text stays: the index after that reference and whether it is inside
/// double quotes or a `${...}`, or `None` when the value ends without one.
/// `${PROMPT_COMMAND:+x}` alone does not keep it, and the output of a command
/// substitution is not known, so that ends the search.
fn value_keeps_old(text: &[u8], start: usize) -> Option<(usize, bool)> {
    let mut in_double = false;
    let mut braces = 0_usize;
    let mut at = start;
    while let Some(&byte) = text.get(at) {
        match byte {
            b'\\' => at += 1,
            b'\'' if !in_double => {
                let ansi = text[..at].ends_with(b"$");
                at = single_quote_end(text, at, ansi)?;
            }
            b'"' => in_double = !in_double,
            b'`' => return None,
            b'$' => match reference_at(text, at) {
                Reference::Substitution => return None,
                Reference::Keeps(end) => return Some((end, in_double || braces > 0)),
                Reference::Other => {
                    if text.get(at + 1) == Some(&b'{') {
                        braces += 1;
                        at += 1;
                    }
                }
            },
            b'}' if braces > 0 => braces -= 1,
            _ if in_double || braces > 0 => {}
            b' ' | b'\t' | b'\r' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>' | b')' => return None,
            _ => {}
        }
        at += 1;
    }
    None
}

/// The index of the `'` closing the one at `open`.
fn single_quote_end(text: &[u8], open: usize, ansi: bool) -> Option<usize> {
    let mut at = open + 1;
    while let Some(&byte) = text.get(at) {
        match byte {
            b'\\' if ansi => at += 1,
            b'\'' => return Some(at),
            _ => {}
        }
        at += 1;
    }
    None
}

/// What the `$` in a value starts.
enum Reference {
    /// `$(`: a command substitution.
    Substitution,
    /// The variable's value, up to this index.
    Keeps(usize),
    Other,
}

fn reference_at(text: &[u8], dollar: usize) -> Reference {
    let rest = &text[dollar + 1..];
    if rest.first() == Some(&b'(') {
        return Reference::Substitution;
    }
    if rest.starts_with(VARIABLE) {
        let end = dollar + 1 + VARIABLE.len();
        return if ends_word(text, end) {
            Reference::Keeps(end)
        } else {
            Reference::Other
        };
    }
    let Some(name) = rest.strip_prefix(b"{") else {
        return Reference::Other;
    };
    let end = dollar + 2 + VARIABLE.len();
    if !name.starts_with(VARIABLE) || !ends_word(text, end) {
        return Reference::Other;
    }
    let after = if text.get(end) == Some(&b'[') {
        subscript_end(text, end).unwrap_or(end)
    } else {
        end
    };
    let operator = &text[after..];
    if operator.starts_with(b":+") || operator.starts_with(b"+") {
        // Expands to the alternative, which may name the variable itself.
        Reference::Other
    } else {
        Reference::Keeps(after)
    }
}

/// Adds to `found` the literal paths `text` sources, on lines of their own or
/// after `;`, `&&`, `||`, `then` and the like. Anything computed is skipped.
/// More than [`MAX_FILES`] distinct paths is an error.
fn sourced_paths(
    text: &[u8],
    home: Option<&Path>,
    found: &mut Vec<PathBuf>,
) -> Result<(), LeaveAlone> {
    for line in text.split(|&byte| byte == b'\n') {
        if is_comment_line(line) {
            continue;
        }
        for statement in line.split(|byte| STATEMENT_ENDS.contains(byte)) {
            let Some((word, argument)) = command_word(statement) else {
                continue;
            };
            if word != b"." && word != b"source" {
                continue;
            }
            let Some((path, _)) = literal_path(argument, home) else {
                continue;
            };
            if found.contains(&path) {
                continue;
            }
            if found.len() == MAX_FILES {
                return Err(LeaveAlone);
            }
            found.push(path);
        }
    }
    Ok(())
}

/// The absolute path the shell word at the start of `word` names, and the
/// length of that word: a word made of literal text, in or out of quotes,
/// that may start with `~/` (unquoted) or `$HOME`/`${HOME}` (not in single
/// quotes). `None` for anything else.
fn literal_path(word: &[u8], home: Option<&Path>) -> Option<(PathBuf, usize)> {
    let home_bytes = || home.map(|home| home.as_os_str().as_bytes().to_vec());
    let mut path = Vec::new();
    let mut quote = None;
    let mut at = 0;
    if word.starts_with(b"~/") {
        path = home_bytes()?;
        at = 1;
    }
    while let Some(&byte) = word.get(at) {
        let single_quoted = quote == Some(b'\'');
        match byte {
            _ if quote == Some(byte) => quote = None,
            b'$' if !single_quoted => {
                if !path.is_empty() {
                    return None;
                }
                let rest = &word[at..];
                let length = if rest.starts_with(b"${HOME}") {
                    b"${HOME}".len()
                } else if rest.starts_with(b"$HOME") && ends_word(rest, b"$HOME".len()) {
                    b"$HOME".len()
                } else {
                    return None;
                };
                path = home_bytes()?;
                at += length;
                continue;
            }
            b'`' | b'\\' if !single_quoted => return None,
            b'\'' | b'"' if quote.is_none() => quote = Some(byte),
            b'*' | b'?' | b'[' if quote.is_none() => return None,
            _ if quote.is_none() && byte.is_ascii_whitespace() => break,
            _ => path.push(byte),
        }
        at += 1;
    }
    if quote.is_some() || path.first() != Some(&b'/') {
        return None;
    }
    Some((PathBuf::from(OsString::from_vec(path)), at))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::channel;

    use proptest::prelude::*;

    use super::*;

    /// A hook run between the stat and the open.
    type Hook = Box<dyn FnMut(&Path)>;

    thread_local! {
        /// Runs on the reading thread between the stat and the open.
        static BETWEEN_STAT_AND_OPEN: std::cell::RefCell<Option<Hook>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn between_stat_and_open(path: &Path) {
        BETWEEN_STAT_AND_OPEN.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook(path);
            }
        });
    }

    const LOGIN: Startup = Startup {
        login: true,
        posix: false,
        norc: false,
    };

    fn loader_survives(startup: Startup, user_env: &dyn Fn(&str) -> Option<OsString>) -> bool {
        loader_terminator(startup, user_env).is_some()
    }

    /// What [`uses_replace`] says about `text`: `None` when it replaces,
    /// else whether it saw separator-led and glued appends.
    fn appends_in(text: &str) -> Option<(bool, bool)> {
        let mut appends = Appends::default();
        (!uses_replace(text.as_bytes(), &mut appends)).then_some((appends.separated, appends.glued))
    }

    /// B5: the spec rows' appends, and the other ways a value can go on
    /// after the old one.
    #[test]
    fn appends_are_told_apart_by_how_they_start() {
        const SEPARATOR: Option<(bool, bool)> = Some((true, false));
        const GLUED: Option<(bool, bool)> = Some((false, true));
        const NEUTRAL: Option<(bool, bool)> = Some((false, false));
        for (text, expected) in [
            ("PROMPT_COMMAND+='echo USER_PC'\n", GLUED),
            ("PROMPT_COMMAND+='echo USER_PC;'\n", GLUED),
            ("PROMPT_COMMAND=\"${PROMPT_COMMAND}echo USER_PC;\"\n", GLUED),
            ("PROMPT_COMMAND=\"$PROMPT_COMMAND echo USER_PC\"\n", GLUED),
            ("PROMPT_COMMAND+=$'\\necho USER_PC'\n", NEUTRAL),
            (
                "PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND;}echo USER_PC\"\n",
                SEPARATOR,
            ),
            (
                "PROMPT_COMMAND=\"$PROMPT_COMMAND; echo USER_PC\"\n",
                SEPARATOR,
            ),
            (
                "PROMPT_COMMAND=${PROMPT_COMMAND:+$PROMPT_COMMAND;}x\n",
                SEPARATOR,
            ),
            (
                "PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND}; x\"\n",
                SEPARATOR,
            ),
            ("PROMPT_COMMAND=\"$PROMPT_COMMAND && x\"\n", SEPARATOR),
            ("PROMPT_COMMAND+=' ; x'\n", SEPARATOR),
            ("PROMPT_COMMAND+='\t x'\n", GLUED),
            ("PROMPT_COMMAND=\"$PROMPT_COMMAND&& x\"\n", SEPARATOR),
            ("PROMPT_COMMAND+=';x'\n", SEPARATOR),
            ("PROMPT_COMMAND+=\"; x\"\n", SEPARATOR),
            ("PROMPT_COMMAND+=$';x'\n", SEPARATOR),
            ("PROMPT_COMMAND+=\"${PROMPT_COMMAND:+;}x\"\n", SEPARATOR),
            ("PROMPT_COMMAND+=\"${PROMPT_COMMAND:+$'\\n'}x\"\n", GLUED),
            ("PROMPT_COMMAND+=' x'\n", GLUED),
            ("PROMPT_COMMAND+=x\n", GLUED),
            ("PROMPT_COMMAND+=\"$x\"\n", GLUED),
            ("PROMPT_COMMAND+='\nx'\n", NEUTRAL),
            ("PROMPT_COMMAND+=\n", NEUTRAL),
            ("PROMPT_COMMAND+=''\n", NEUTRAL),
            // The old value last: nothing follows the loader.
            ("PROMPT_COMMAND=\"x;$PROMPT_COMMAND\"\n", NEUTRAL),
            ("PROMPT_COMMAND=$PROMPT_COMMAND\n", NEUTRAL),
            ("PROMPT_COMMAND=$PROMPT_COMMAND; x=1\n", NEUTRAL),
            ("PROMPT_COMMAND=\"x;${PROMPT_COMMAND}\"\n", NEUTRAL),
            ("PROMPT_COMMAND=\"$PROMPT_COMMAND\"$'\\nx'\n", NEUTRAL),
            ("PROMPT_COMMAND=$PROMPT_COMMAND';x'\n", SEPARATOR),
            // Both kinds in one file.
            (
                "PROMPT_COMMAND+=';a'\nPROMPT_COMMAND+=b\n",
                Some((true, true)),
            ),
            // B5i: an assignment replaces.
            ("PROMPT_COMMAND='history -a; echo USER_PC'\n", None),
        ] {
            assert_eq!(appends_in(text), expected, "{text:?}");
        }
    }

    #[test]
    fn the_appends_pick_the_terminator() {
        let terminator = |profile: &str| Home::new(&[(".bash_profile", profile)]).terminator(LOGIN);
        assert_eq!(Home::new(&[]).terminator(LOGIN), Some(Terminator::Newline));
        assert_eq!(terminator("x=1\n"), Some(Terminator::Newline));
        assert_eq!(terminator("PROMPT_COMMAND+=x\n"), Some(Terminator::Newline));
        assert_eq!(
            terminator("PROMPT_COMMAND+=$'\\nx'\n"),
            Some(Terminator::Newline)
        );
        assert_eq!(
            terminator("PROMPT_COMMAND+=';x'\n"),
            Some(Terminator::Nothing)
        );
        assert_eq!(
            terminator("PROMPT_COMMAND+=';x'\nPROMPT_COMMAND+=$'\\ny'\n"),
            Some(Terminator::Nothing)
        );
        // Mixed: no terminator works for both.
        assert_eq!(
            terminator("PROMPT_COMMAND+=';x'\nPROMPT_COMMAND+=y\n"),
            None
        );
        // Across files, sourced ones included.
        let home = Home::new(&[
            (".bash_profile", "PROMPT_COMMAND+=';x'\n. ~/lib\n"),
            ("lib", "PROMPT_COMMAND+=y\n"),
        ]);
        assert_eq!(home.terminator(LOGIN), None);
        let home = Home::new(&[
            (".bash_profile", "PROMPT_COMMAND+=';x'\n"),
            (".bashrc", "PROMPT_COMMAND+=y\n"),
        ]);
        assert_eq!(home.terminator(LOGIN), None);
        // A tool that keeps the entries is not classified.
        let home = Home::new(&[
            (".bash_profile", "PROMPT_COMMAND+=';x'\n. ~/tool\n"),
            ("tool", "__bp_install_string=x\nPROMPT_COMMAND+=y\n"),
        ]);
        assert_eq!(home.terminator(LOGIN), Some(Terminator::Nothing));
        // B5h.
        assert_eq!(terminator("PROMPT_COMMAND+=(\"echo USER_PC\")\n"), None);
    }

    /// M2: only a definition of a keeping tool's name exempts a file.
    #[test]
    fn a_mention_of_a_keeping_tool_does_not_exempt_a_file() {
        for (tool, exempt) in [
            ("# starship_precmd\n", false),
            ("echo \"__bp_install_string\"\n", false),
            ("echo '__bp_install_string=x'\n", false),
            ("x=starship_precmd\n", false),
            ("cat <<EOF\nstarship_precmd() { :; }\nEOF\n", false),
            ("cat <<-'EOF'\n\t__bp_install_string=x\n\tEOF\n", false),
            ("cat <<EOF\nx\nEOF\nstarship_precmd() { :; }\n", true),
            ("starship_precmd() { :; }\n", true),
            ("starship_precmd () {\n:\n}\n", true),
            ("function starship_precmd {\n:\n}\n", true),
            ("  function starship_precmd() { :; }\n", true),
            ("__bp_install_string=x\n", true),
            ("__bp_install_string=$'x'\n", true),
            ("starship_precmd_user_func=x\n", false),
        ] {
            let text = format!("{tool}PROMPT_COMMAND=__p\n");
            let home = Home::new(&[(".bash_profile", ". ~/tool\n"), ("tool", &text)]);
            assert_eq!(home.survives(LOGIN), exempt, "{tool:?}");
        }
    }

    #[test]
    fn a_posix_login_shell_reads_every_profile() {
        let posix_login = Startup {
            login: true,
            posix: true,
            norc: false,
        };
        let home = Home::new(&[(".bash_profile", ""), (".profile", ASSIGNS)]);
        assert!(home.survives(LOGIN));
        assert!(!home.survives(posix_login));
    }

    /// A function with an assignment naming it, a plain command, and every
    /// other form that loses the old value.
    #[test]
    fn assignments_that_lose_the_old_value_are_found() {
        for text in [
            "__p() { :; }; PROMPT_COMMAND=__p\n",
            "PROMPT_COMMAND='history -a'",
            "PROMPT_COMMAND=\"history -a\"\n",
            "  PROMPT_COMMAND=x",
            "export PROMPT_COMMAND=x\n",
            "declare -x PROMPT_COMMAND=x\n",
            "readonly PROMPT_COMMAND=x\n",
            "if true; then PROMPT_COMMAND=x; fi\n",
            "PROMPT_COMMAND=\n",
            "PROMPT_COMMAND=()\n",
            "PROMPT_COMMAND=(a b)\n",
            "PROMPT_COMMAND=(a # \"$PROMPT_COMMAND\"\n b)\n",
            "PROMPT_COMMAND[0]=x\n",
            "PROMPT_COMMAND[\n",
            "PROMPT_COMMAND='$PROMPT_COMMAND'\n",
            "PROMPT_COMMAND=$'a\\'$PROMPT_COMMAND'\n",
            "PROMPT_COMMAND=\\$PROMPT_COMMAND\n",
            "PROMPT_COMMAND=\"$PROMPT_COMMANDS\"\n",
            "PROMPT_COMMAND=\"${PROMPT_COMMANDS}\"\n",
            "PROMPT_COMMAND=\"${PROMPT_COMMAND:+x}\"\n",
            "PROMPT_COMMAND=\"${PROMPT_COMMAND+x}\"\n",
            "PROMPT_COMMAND=\"${PROMPT_COMMAND[*]:+x}\"\n",
            "PROMPT_COMMAND=\"${#PROMPT_COMMAND}\"\n",
            "PROMPT_COMMAND=$(cat f)\n",
            "PROMPT_COMMAND=\"$(f)$PROMPT_COMMAND\"\n",
            "PROMPT_COMMAND=`cat f`\n",
            "PROMPT_COMMAND=`f`$PROMPT_COMMAND\n",
            "PROMPT_COMMAND=(a)\necho $PROMPT_COMMAND\n",
            "PROMPT_COMMAND=a $PROMPT_COMMAND\n",
            "PROMPT_COMMAND=a; x=$PROMPT_COMMAND\n",
            "PROMPT_COMMAND='unterminated\n",
            "read PROMPT_COMMAND\n",
            "printf -v PROMPT_COMMAND x\n",
            "eval 'PROMPT_COMMAND=x'\n",
            "export PROMPT_COMMANDX PROMPT_COMMAND[0]\n",
            "unset 'PROMPT_COMMAND[0]'\n",
            "export\\\nPROMPT_COMMAND=x\n",
            "echo it's\nPROMPT_COMMAND=x\n",
            "cat <<E\nit's\nE\nPROMPT_COMMAND=x\n",
            "# a comment\nPROMPT_COMMAND=x\n",
            "PROMPT_COMMAND+=x\nPROMPT_COMMAND=y\n",
            "PROMPT_COMMAND=\"$PROMPT_COMMAND;x\"\nPROMPT_COMMAND=y\n",
            // B5h: an array, appended to or built from the old value: bash
            // 3.2 never runs element 1.
            "PROMPT_COMMAND+=(x)\n",
            "PROMPT_COMMAND+=(\"$PROMPT_COMMAND\")\n",
            "PROMPT_COMMAND=(\"${PROMPT_COMMAND[@]}\" x)\n",
            "PROMPT_COMMAND=(x\n \"$PROMPT_COMMAND\")\n",
            "PROMPT_COMMAND[${#PROMPT_COMMAND[@]}]=x\n",
            "PROMPT_COMMAND[1]+=x\n",
        ] {
            assert!(replaces_prompt_command(text.as_bytes()), "{text:?}");
        }
    }

    #[test]
    fn forms_that_keep_the_old_value_are_not_alarms() {
        for text in [
            "",
            "PROMPT_COMMAND=\"x;$PROMPT_COMMAND\"\n",
            "PROMPT_COMMAND=\"$PROMPT_COMMAND;x\"\n",
            "PROMPT_COMMAND=\"x;${PROMPT_COMMAND}\"\n",
            "PROMPT_COMMAND=$PROMPT_COMMAND\n",
            "PROMPT_COMMAND=${PROMPT_COMMAND:-x}\n",
            "PROMPT_COMMAND=\"${PROMPT_COMMAND[*]}\"\n",
            "PROMPT_COMMAND=\"x${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"\n",
            "PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND; }x\"\n",
            "PROMPT_COMMAND=$'x\\n'\"$PROMPT_COMMAND\"\n",
            "PROMPT_COMMAND='x;'$PROMPT_COMMAND\n",
            "PROMPT_COMMAND=\"it's;$PROMPT_COMMAND\"\n",
            "export PROMPT_COMMAND=\"x;$PROMPT_COMMAND\"\n",
            "PROMPT_COMMAND+=x\n",
            "PROMPT_COMMAND+=';x'\n",
            "export PROMPT_COMMAND\n",
            "export A PROMPT_COMMAND; x=1\n",
            "if a; then export PROMPT_COMMAND; fi\n",
            "export -n PROMPT_COMMAND\n",
            "unset PROMPT_COMMAND\n",
            "unset -v PROMPT_COMMAND\n",
            "# PROMPT_COMMAND=x\n",
            "  # PROMPT_COMMAND=x\n",
            "a=1\n\t# PROMPT_COMMAND=x\nb=2\n",
            "MY_PROMPT_COMMAND=x\n",
            "PROMPT_COMMAND_X=x\n",
            "echo \"$PROMPT_COMMAND\"\n",
            "[ -n \"${PROMPT_COMMAND}\" ]\n",
            "echo ${#PROMPT_COMMAND} ${!PROMPT_COMMAND}\n",
            "[[ $PROMPT_COMMAND == *x* ]] || PROMPT_COMMAND=\"x;$PROMPT_COMMAND\"\n",
        ] {
            assert!(!replaces_prompt_command(text.as_bytes()), "{text:?}");
        }
    }

    #[test]
    fn a_bare_name_counts_unless_export_or_unset_is_in_reach() {
        let far = format!("export {} PROMPT_COMMAND\n", "A".repeat(MAX_LOOKBACK));
        assert!(replaces_prompt_command(far.as_bytes()));
        let near = format!("{}\nexport PROMPT_COMMAND\n", "a=1 ".repeat(100));
        assert!(!replaces_prompt_command(near.as_bytes()));
        // A window cut inside a word does not make that word the command.
        let cut = format!("a{}PROMPT_COMMAND\n", format_args!("export{:58}", ""));
        assert!(replaces_prompt_command(cut.as_bytes()));
        let long_subscript = format!("PROMPT_COMMAND[{}]+=x\n", "1".repeat(MAX_SUBSCRIPT));
        assert!(replaces_prompt_command(long_subscript.as_bytes()));
    }

    #[test]
    fn sources_with_a_literal_path_are_listed() {
        let home = Path::new("/h");
        let text = r#"
. ~/a
source ~/b ignored args
. $HOME/c
. ${HOME}/d
. "$HOME/e f"
. "$HOME"/g
source "${HOME}/h"
. /abs/i
. "/abs/j k"
. '/abs/l m'
[ -f ~/n ] && . ~/n
if true; then . ~/o; fi
true; source ~/p
builtin source ~/q
command . ~/r
{ . ~/s; }
. ~/a
"#;
        let mut found = Vec::new();
        sourced_paths(text.as_bytes(), Some(home), &mut found).unwrap();
        let found: Vec<_> = found.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(
            found,
            [
                "/h/a", "/h/b", "/h/c", "/h/d", "/h/e f", "/h/g", "/h/h", "/abs/i", "/abs/j k",
                "/abs/l m", "/h/n", "/h/o", "/h/p", "/h/q", "/h/r", "/h/s",
            ]
        );
    }

    #[test]
    fn computed_or_relative_sources_are_not_followed() {
        let text = r#"
# . ~/comment
# a comment; . ~/after-a-semicolon
. "$OSH"/oh-my-bash.sh
. $HOMEX/a
. ${HOME:-x}/a
. ~/$x
. ~/a*
. /a?
. /a[b]
. "~/quoted"
'.' /not-this-form-either/../x'
. '~/single'
. '$HOME/single'
. ./relative
. relative
. ~user/a
. "/unterminated
. /a\ b
. `x`
. "/a`b"
. "/a\b"
. /x$HOME/y
echo . /not/sourced
.
"#;
        let mut found = Vec::new();
        sourced_paths(text.as_bytes(), Some(Path::new("/h")), &mut found).unwrap();
        assert_eq!(found, Vec::<PathBuf>::new());
        // Without a home, `~` and `$HOME` name nothing.
        sourced_paths(b". ~/a\n. $HOME/b\n", None, &mut found).unwrap();
        assert_eq!(found, Vec::<PathBuf>::new());
    }

    /// A throwaway home directory.
    struct Home {
        dir: tempfile::TempDir,
    }

    impl Home {
        fn new(files: &[(&str, &str)]) -> Self {
            let home = Self {
                dir: tempfile::tempdir().unwrap(),
            };
            for (name, text) in files {
                home.write(name, text.as_bytes());
            }
            home
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn write(&self, name: &str, text: &[u8]) {
            let path = self.path(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn env(&self) -> impl Fn(&str) -> Option<OsString> + use<> {
            let home = self.dir.path().as_os_str().to_os_string();
            move |key| (key == "HOME").then(|| home.clone())
        }

        fn survives(&self, startup: Startup) -> bool {
            loader_survives(startup, &self.env())
        }

        fn terminator(&self, startup: Startup) -> Option<Terminator> {
            loader_terminator(startup, &self.env())
        }
    }

    const ASSIGNS: &str = "__p() { :; }; PROMPT_COMMAND=__p\n";
    const KEEPS: &str = "PROMPT_COMMAND=\"x;$PROMPT_COMMAND\"\n";

    #[test]
    fn a_login_shell_reads_the_first_profile_and_bashrc() {
        assert!(Home::new(&[]).survives(LOGIN));
        for (files, survives) in [
            (&[(".bash_profile", KEEPS)][..], true),
            (&[(".bash_profile", ASSIGNS)][..], false),
            (&[(".bash_login", ASSIGNS)][..], false),
            (&[(".profile", ASSIGNS)][..], false),
            (&[(".bashrc", ASSIGNS)][..], false),
            // Bash stops at the first that exists.
            (&[(".bash_profile", ""), (".bash_login", ASSIGNS)][..], true),
            (&[(".bash_profile", ""), (".profile", ASSIGNS)][..], true),
            (&[(".bash_login", ""), (".profile", ASSIGNS)][..], true),
            // P2: in .bashrc, sourced by the profile.
            (
                &[(".bash_profile", ". ~/.bashrc\n"), (".bashrc", ASSIGNS)][..],
                false,
            ),
        ] {
            assert_eq!(Home::new(files).survives(LOGIN), survives, "{files:?}");
        }
    }

    #[test]
    fn a_sourced_file_counts_one_level_deep() {
        let sourcing = |line: &str| {
            let home = Home::new(&[(".bash_profile", line), ("lib/one", ASSIGNS)]);
            home.survives(LOGIN)
        };
        assert!(!sourcing(". ~/lib/one\n"));
        assert!(!sourcing("source $HOME/lib/one\n"));
        assert!(sourcing(". ~/lib/missing\n"));
        assert!(sourcing(". \"$DIR\"/lib/one\n"));
        // From .bashrc as well, and by absolute path.
        let home = Home::new(&[("lib/one", ASSIGNS)]);
        let line = format!(". '{}'\n", home.path("lib/one").display());
        home.write(".bashrc", line.as_bytes());
        assert!(!home.survives(LOGIN));
        // Not two levels.
        let home = Home::new(&[
            (".bash_profile", ". ~/lib/one\n"),
            ("lib/one", ". ~/lib/two\n"),
            ("lib/two", ASSIGNS),
        ]);
        assert!(home.survives(LOGIN));
    }

    #[test]
    fn sourced_tools_that_keep_the_entries_are_not_alarms() {
        for marker in ["__bp_install_string=x\n", "starship_precmd() { :; }\n"] {
            let tool = format!("{marker}PROMPT_COMMAND=x\n");
            let sourced = Home::new(&[(".bash_profile", ". ~/tool\n"), ("tool", &tool)]);
            assert!(sourced.survives(LOGIN), "{marker}");
            // The exception is for sourced files only.
            let direct = Home::new(&[(".bash_profile", &tool)]);
            assert!(!direct.survives(LOGIN), "{marker}");
        }
    }

    #[test]
    fn what_cannot_be_read_counts_as_unknown() {
        // A directory where a file should be.
        let home = Home::new(&[]);
        fs::create_dir(home.path(".bash_profile")).unwrap();
        assert!(!home.survives(LOGIN));
        // A sourced directory.
        let home = Home::new(&[(".bash_profile", ". ~/lib\n"), ("lib/x", "")]);
        assert!(!home.survives(LOGIN));
        // No permission (a root test run could read it anyway).
        let home = Home::new(&[(".bashrc", "")]);
        fs::set_permissions(home.path(".bashrc"), fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = fs::File::open(home.path(".bashrc")).is_err();
        assert_eq!(home.survives(LOGIN), !unreadable);
        // A parent that is a file is a missing file, as for bash.
        let home = Home::new(&[(".bash_profile", ". ~/.bashrc/x\n"), (".bashrc", "")]);
        assert!(home.survives(LOGIN));
        // A link to nowhere too.
        let home = Home::new(&[]);
        std::os::unix::fs::symlink("/nonexistent/x", home.path(".bashrc")).unwrap();
        assert!(home.survives(LOGIN));
    }

    /// Runs `read` on a thread of its own: without `O_NONBLOCK`, opening a
    /// FIFO blocks for as long as nobody writes to it.
    fn read_with_timeout(path: PathBuf) -> Result<Option<Vec<u8>>, LeaveAlone> {
        let (sender, receiver) = channel();
        thread::spawn(move || sender.send(read_startup_file(&path)));
        receiver
            .recv_timeout(Duration::from_secs(60))
            .expect("reading a startup file blocked")
    }

    #[test]
    fn a_fifo_is_neither_waited_for_nor_read() {
        let home = Home::new(&[]);
        let fifo = home.path(".bash_profile");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(read_with_timeout(fifo), Err(LeaveAlone));
        assert!(!home.survives(LOGIN));
    }

    fn mkfifo(path: &Path) {
        assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
    }

    /// S1-S3: a FIFO with a one-shot writer (open, write, close) that bash
    /// reads later. The scan must not open it: that would end the writer's
    /// wait for a reader, and its text would be lost before bash reads it.
    fn check_one_shot_writer(home: &Home, fifo: &Path) {
        let opened = Arc::new(AtomicBool::new(false));
        let writer = {
            let (fifo, opened) = (fifo.to_path_buf(), Arc::clone(&opened));
            thread::spawn(move || {
                use std::io::Write;
                let mut file = OpenOptions::new().write(true).open(&fifo).unwrap();
                opened.store(true, Ordering::Release);
                let _ = file.write_all(b"echo one-shot\n");
            })
        };
        assert!(!home.survives(LOGIN));
        // A scan that opened the FIFO let the writer go on; give it time.
        thread::sleep(Duration::from_millis(300));
        assert!(!opened.load(Ordering::Acquire), "the scan opened the FIFO");
        // bash, the first reader, gets the whole text.
        let text = fs::read(fifo).unwrap();
        writer.join().unwrap();
        assert_eq!(text, b"echo one-shot\n");
    }

    #[test]
    fn a_fifo_with_a_one_shot_writer_is_left_to_bash() {
        // S1: the profile itself.
        let home = Home::new(&[]);
        mkfifo(&home.path(".bash_profile"));
        check_one_shot_writer(&home, &home.path(".bash_profile"));
        // S2: a sourced file.
        let home = Home::new(&[(".bash_profile", ". ~/fifo\n")]);
        mkfifo(&home.path("fifo"));
        check_one_shot_writer(&home, &home.path("fifo"));
        // S3: behind a link.
        let home = Home::new(&[]);
        mkfifo(&home.path("fifo"));
        std::os::unix::fs::symlink(home.path("fifo"), home.path(".bash_profile")).unwrap();
        check_one_shot_writer(&home, &home.path("fifo"));
    }

    /// S3: links to what is not a regular file are not opened.
    #[test]
    fn a_link_to_a_device_or_a_directory_is_not_opened() {
        let opens = std::rc::Rc::new(std::cell::Cell::new(0));
        let home = Home::new(&[]);
        fs::create_dir(home.path("dir")).unwrap();
        for target in [
            Path::new("/dev/zero"),
            Path::new("/dev/tty"),
            &home.path("dir"),
        ] {
            let link = home.path(".bashrc");
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(target, &link).unwrap();
            let count = std::rc::Rc::clone(&opens);
            BETWEEN_STAT_AND_OPEN.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move |_| count.set(count.get() + 1)));
            });
            assert_eq!(read_startup_file(&link), Err(LeaveAlone), "{target:?}");
            BETWEEN_STAT_AND_OPEN.with_borrow_mut(|hook| *hook = None);
        }
        assert_eq!(opens.get(), 0);
    }

    /// S5: a file swapped between the stat and the open is not read.
    #[test]
    fn a_file_swapped_after_the_stat_is_not_read() {
        let home = Home::new(&[(".bashrc", "x=1\n"), ("other", "x=2\n")]);
        mkfifo(&home.path("fifo"));
        for replacement in ["fifo", "other"] {
            // Not through a FIFO left by the last round: that would wait.
            let _ = fs::remove_file(home.path(".bashrc"));
            home.write(".bashrc", b"x=1\n");
            let (from, to) = (home.path(replacement), home.path(".bashrc"));
            BETWEEN_STAT_AND_OPEN.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move |_| {
                    let moved = from.with_extension("moved");
                    // Keep the replacement for the next round.
                    if replacement == "other" {
                        fs::copy(&from, &moved).unwrap();
                        fs::rename(&moved, &to).unwrap();
                    } else {
                        fs::rename(&from, &to).unwrap();
                    }
                }));
            });
            assert_eq!(
                read_startup_file(&home.path(".bashrc")),
                Err(LeaveAlone),
                "{replacement}"
            );
            BETWEEN_STAT_AND_OPEN.with_borrow_mut(|hook| *hook = None);
        }
        // Unswapped, the same file is read.
        fs::remove_file(home.path(".bashrc")).unwrap();
        home.write(".bashrc", b"x=1\n");
        assert_eq!(
            read_startup_file(&home.path(".bashrc")),
            Ok(Some(b"x=1\n".to_vec()))
        );
    }

    /// S4: a sourced file of 10 000 lines is scanned within the wait.
    #[test]
    fn a_flood_of_lines_is_scanned_within_the_wait() {
        for (line, survives) in [
            ("# PROMPT_COMMAND=x", true),
            ("PROMPT_COMMAND+=$'\\nx'", true),
            ("PROMPT_COMMAND=\"$PROMPT_COMMAND;x\"", true),
            ("PROMPT_COMMAND=x", false),
        ] {
            let flood = format!("{line}\n").repeat(10_000);
            let home = Home::new(&[(".bash_profile", ". ~/lib\n"), ("lib", &flood)]);
            let started = std::time::Instant::now();
            assert_eq!(home.survives(LOGIN), survives, "{line}");
            // `survives` is false past the wait anyway; the true rows prove
            // the scan ended in it.
            assert!(started.elapsed() < WAIT_SLICE * (WAIT_SLICES + 10) || !survives);
        }
    }

    #[test]
    fn a_device_is_not_read() {
        assert_eq!(
            read_with_timeout(PathBuf::from("/dev/zero")),
            Err(LeaveAlone)
        );
    }

    #[test]
    fn a_file_at_the_byte_cap_is_read_and_one_over_is_not() {
        let cap = usize::try_from(MAX_FILE_BYTES).unwrap();
        let home = Home::new(&[]);
        home.write(".bashrc", &vec![b'\n'; cap]);
        assert!(home.survives(LOGIN));
        home.write(".bashrc", &vec![b'\n'; cap + 1]);
        assert!(!home.survives(LOGIN));
        // An assignment in the last bytes under the cap is found.
        let mut text = vec![b'\n'; cap - ASSIGNS.len()];
        text.extend_from_slice(ASSIGNS.as_bytes());
        home.write(".bashrc", &text);
        assert!(!home.survives(LOGIN));
    }

    #[test]
    fn a_source_without_an_end_is_read_to_the_cap_only() {
        /// Serves zeros without end and counts them.
        struct Endless(u64);
        impl Read for &mut Endless {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                // A reader past the cap stops here instead of filling memory.
                if self.0 > 2 * MAX_FILE_BYTES {
                    return Err(io::ErrorKind::Other.into());
                }
                buffer.fill(0);
                self.0 += buffer.len() as u64;
                Ok(buffer.len())
            }
        }
        let mut source = Endless(0);
        assert_eq!(read_capped(&mut source), None);
        assert_eq!(source.0, MAX_FILE_BYTES + 1);
    }

    #[test]
    fn the_file_count_is_capped() {
        let lines = |count: usize| {
            let paths: Vec<String> = (0..count).map(|n| format!(". ~/lib/{n}")).collect();
            paths.join("\n")
        };
        // .bash_profile and .bashrc take two of the cap.
        let within = lines(MAX_FILES - 2);
        assert!(Home::new(&[(".bash_profile", &within)]).survives(LOGIN));
        let over = lines(MAX_FILES - 1);
        assert!(!Home::new(&[(".bash_profile", &over)]).survives(LOGIN));
        // More sources than the list holds stop at the list.
        let flood = lines(100_000);
        let mut found = Vec::new();
        assert_eq!(
            sourced_paths(flood.as_bytes(), Some(Path::new("/h")), &mut found),
            Err(LeaveAlone)
        );
        assert_eq!(found.len(), MAX_FILES);
        // A path that is read anyway does not count again when sourced.
        let again = format!("{}\n. ~/.bashrc\n", lines(MAX_FILES - 2));
        assert!(Home::new(&[(".bash_profile", &again), (".bashrc", "")]).survives(LOGIN));
        // The same path many times counts once.
        let repeated = ". ~/lib/a\n".repeat(1000);
        assert!(Home::new(&[(".bash_profile", &repeated)]).survives(LOGIN));
    }

    #[test]
    fn home_and_env_decide_what_can_be_named() {
        let login_with = |home: Option<&str>| {
            let home = home.map(OsString::from);
            loader_survives(LOGIN, &move |key| {
                (key == "HOME").then(|| home.clone()).flatten()
            })
        };
        assert!(!login_with(None));
        assert!(!login_with(Some("")));
        assert!(!login_with(Some("relative")));

        let posix = Startup {
            login: false,
            posix: true,
            norc: true,
        };
        let posix_with_rc = Startup {
            norc: false,
            ..posix
        };
        let plain = Startup {
            login: false,
            posix: false,
            norc: true,
        };
        let home = Home::new(&[("env", ASSIGNS), ("clean", "x=1\n"), (".bashrc", ASSIGNS)]);
        // bash 3.2 reads .bashrc for --posix, unless --norc.
        assert!(!home.survives(posix_with_rc));
        assert!(!loader_survives(posix_with_rc, &|_| None));
        let with_env = |value: &str| {
            let value = OsString::from(value.replace('@', home.dir.path().to_str().unwrap()));
            let base = home.env();
            loader_survives(posix, &move |key| {
                if key == "ENV" {
                    Some(value.clone())
                } else {
                    base(key)
                }
            })
        };
        // A bash that reads no file needs no scan; `--posix --norc` reads
        // $ENV only.
        assert!(home.survives(plain));
        assert!(home.survives(posix));
        assert!(with_env(""));
        assert!(with_env("@/clean"));
        assert!(!with_env("@/env"));
        assert!(!with_env("$HOME/env"));
        assert!(!with_env("$OTHER/env"), "not a literal path");
        assert!(!with_env("@/clean extra"), "not one word");
    }

    #[test]
    fn a_stalled_scan_is_given_up_on_and_their_number_is_capped() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        let (release, released) = channel::<()>();
        let released = Arc::new(std::sync::Mutex::new(released));
        for _ in 0..MAX_SCANS {
            let released = Arc::clone(&released);
            let answer = within_deadline(&RUNNING, 1, move || {
                let _ = released.lock().unwrap().recv();
                Some(())
            });
            assert!(
                answer.is_none(),
                "the wait must end while the scan is stuck"
            );
        }
        assert_eq!(RUNNING.load(Ordering::Acquire), MAX_SCANS);
        // One more is refused without starting.
        let started = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&started);
        let answer = within_deadline(&RUNNING, 100, move || {
            flag.store(true, Ordering::Release);
            Some(())
        });
        assert!(answer.is_none());
        assert!(!started.load(Ordering::Acquire));
        assert_eq!(RUNNING.load(Ordering::Acquire), MAX_SCANS);
        // Once they end, scans run again.
        drop(release);
        while RUNNING.load(Ordering::Acquire) != 0 {
            thread::sleep(WAIT_SLICE);
        }
        assert_eq!(within_deadline(&RUNNING, u32::MAX, || Some(1)), Some(1));
        assert_eq!(within_deadline(&RUNNING, u32::MAX, || None::<()>), None);
    }

    #[test]
    fn a_scan_that_panics_counts_as_unknown() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        assert!(within_deadline::<()>(&RUNNING, u32::MAX, || panic!("expected")).is_none());
        while RUNNING.load(Ordering::Acquire) != 0 {
            thread::sleep(WAIT_SLICE);
        }
    }

    proptest! {
        #[test]
        fn any_bytes_are_handled(text in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = replaces_prompt_command(&text);
            let mut found = Vec::new();
            let _ = sourced_paths(&text, Some(Path::new("/h")), &mut found);
        }

        /// Quotes, heredoc text and other noise on the lines before cannot
        /// hide an assignment on a line of its own.
        #[test]
        fn noise_does_not_hide_an_assignment(
            before in proptest::collection::vec("[ -~]{0,40}", 0..8),
            after in proptest::collection::vec("[ -~]{0,40}", 0..8),
        ) {
            prop_assume!(!before.iter().chain(&after).any(|l| l.contains("PROMPT_COMMAND")));
            let text = format!(
                "{}\nPROMPT_COMMAND=x\n{}",
                before.join("\n"),
                after.join("\n")
            );
            prop_assert!(replaces_prompt_command(text.as_bytes()));
        }

        /// Text with only keeping forms never raises the alarm, whatever
        /// their order.
        #[test]
        fn keeping_forms_stay_quiet(forms in proptest::collection::vec(0..6_usize, 0..12)) {
            const KEEPING: [&str; 6] = [
                "PROMPT_COMMAND=\"a;$PROMPT_COMMAND\"",
                "PROMPT_COMMAND+=';b'",
                "PROMPT_COMMAND+=$'\\nc'",
                "export PROMPT_COMMAND",
                "echo \"$PROMPT_COMMAND\"",
                "# PROMPT_COMMAND=d",
            ];
            let text = forms.iter().map(|&n| KEEPING[n]).collect::<Vec<_>>().join("\n");
            prop_assert!(!replaces_prompt_command(text.as_bytes()), "{}", text);
        }
    }
}
