//! Shell commands: the records the core keeps of them, prompt navigation
//! and Select Last Output (#45a, ADR 0039).
//!
//! The shell marks its prompt, the command line, the start of the output
//! and the end of each command with OSC 133 `A`, `B`, `C` and `D` (ADR
//! 0022). The terminal puts each mark on its line and reports it as
//! [`Request::Mark`](tessera_term::Request::Mark), coalesced to the last
//! mark of each kind per call (and again per chunk of output here), so a
//! flood of marks costs at most four requests per chunk (S6).
//!
//! # Records are metadata
//!
//! `CommandLog` turns the reported marks into [`CommandRecord`]s: a ring
//! of at most [`MAX_COMMAND_RECORDS`] commands that reached their output
//! (`C`), with their lines, start and finish times and exit status. Since
//! the reports are coalesced, commands that start and finish inside one
//! chunk of output (a pasted `a; b; c`) leave no record; navigation and
//! select therefore never read the records: they read the marks on the
//! lines, which are complete. The records feed the
//! [`SessionEvent::CommandStarted`] and [`SessionEvent::CommandFinished`]
//! events, [`crate::App::command_at`] and the menu's
//! [`crate::App::has_command_output`].
//!
//! Line ids are [`Grid::scrollback_pushed`] plus the screen row, the
//! numbering of the selection (ADR 0025), valid within one
//! [`Terminal::line_epoch`]: a resize re-wraps the history without counting
//! the rows it moves, so a record from an older epoch keeps its exit status
//! and times but reports no lines. Records whose lines left the history are
//! skipped when read (lazily: nothing is pruned on the output path).
//!
//! # Scans
//!
//! Prompt navigation and Select Last Output walk the lines from the view
//! (or the bottom) a chunk of [`PROMPT_SCAN_LINES_PER_LOCK`] lines per hold
//! of the terminal lock, at most [`MAX_PROMPT_SCAN_LINES`] lines per call
//! (S5, S6). A scan that sees the line epoch change or the alternate screen
//! come up gives up.
//!
//! # Locks
//!
//! The command log's lock is a leaf: taken alone by the reader thread, and
//! after the terminal lock was released (or while holding it) elsewhere;
//! nothing is locked while it is held.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tessera_grid::{Grid, Line, Mark};
use tessera_term::Terminal;

use crate::search::TerminalSource;
use crate::session::{Session, SessionEvent};
use crate::{ScrollTarget, selection};

/// Most command records one session keeps (S5); the oldest gives way.
pub const MAX_COMMAND_RECORDS: usize = 1000;

/// Most lines one prompt navigation or Select Last Output call looks at
/// (S5): a prompt further away than this is not found (navigation stays
/// where it is), whatever the scrollback holds (up to
/// `[terminal] scrollback_lines` = 10 000 000).
pub const MAX_PROMPT_SCAN_LINES: usize = 1_000_000;

/// Most lines a scan looks at under one hold of the terminal lock (S6),
/// so the reader thread and the frame never wait long behind it: 256,
/// like a search step (fixture D-SCAN-LOCK).
pub const PROMPT_SCAN_LINES_PER_LOCK: usize = 256;

/// A line id no line has: a record's line it never learnt.
const NO_LINE: u64 = u64::MAX;

/// Where a command is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CommandState {
    /// Its output started (`C`) and nothing has ended it yet.
    Running,
    /// A `D` ended it, or the next prompt (`A`) or output (`C`) did: the
    /// shell sent no `D` (exit status unknown).
    Finished,
}

/// One command the shell reported, as [`crate::App::command_records`] and
/// [`crate::App::command_at`] return it. Holds no command text (ADR 0021):
/// only where it is, when and how it ended.
///
/// The lines are ids in [`Grid::scrollback_pushed`] plus screen row
/// numbering; `None` when the shell sent no such mark (or it was coalesced
/// away), when the line left the history, and for every line of a record
/// taken before the last resize ([`Terminal::line_epoch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommandRecord {
    /// The line of the prompt (`A`).
    pub prompt_line: Option<u64>,
    /// The line the command line started on (`B`).
    pub input_line: Option<u64>,
    /// The first line of the output (`C`).
    pub output_line: Option<u64>,
    /// The line of what ended it: the `D`, or the next `A` or `C`.
    pub end_line: Option<u64>,
    /// When the core processed the `C`.
    pub started: Instant,
    /// When the core processed what ended it.
    pub finished: Option<Instant>,
    /// The `D` mark's exit status: `None` when it had none, or none came
    /// (unknown, which is neither success nor failure).
    pub exit_code: Option<i32>,
    /// Running or finished.
    pub state: CommandState,
}

impl CommandRecord {
    /// From the start of the output to the end; `None` while running.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        self.finished
            .map(|finished| finished.saturating_duration_since(self.started))
    }
}

/// A command as the log keeps it: about 80 bytes, lines [`NO_LINE`] when
/// unknown.
#[derive(Debug, Clone, Copy)]
struct Record {
    prompt: u64,
    input: u64,
    output: u64,
    end: u64,
    /// [`Terminal::line_epoch`] its lines belong to.
    epoch: u64,
    started: Instant,
    finished: Option<Instant>,
    exit_code: Option<i32>,
    /// The line id of its `C` now, in epoch `anchor_epoch`: `output` at
    /// first, carried across each resize by the `C`'s rank from the bottom
    /// ([`resize`]); [`ERASED`] once the `C` was gone at a resize, below
    /// the oldest line once it left the history.
    anchor: i64,
    anchor_epoch: u64,
}

/// [`Record::anchor`] of a `C` that a screen clear or a deleted line
/// erased before a resize: newer than every line.
const ERASED: i64 = i64::MAX;

/// A prompt whose command has not started (no `C` yet): not a record.
#[derive(Debug, Clone, Copy)]
struct Pending {
    prompt: u64,
    input: u64,
    epoch: u64,
}

/// Where the lines are now, read under the terminal lock: what a query
/// resolves line ids against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineView {
    epoch: u64,
    /// The id of the oldest line left (it may be negative after a resize).
    base: i64,
}

impl LineView {
    /// The lines of `term` as they are now.
    pub(crate) fn of(term: &Terminal) -> Self {
        let grid = term.grid();
        Self {
            epoch: term.line_epoch(),
            base: id(grid.scrollback_pushed()) - id(grid.scrollback_len() as u64),
        }
    }

    /// `line` if `record` belongs to this epoch and the line is still
    /// there.
    fn line(self, record: &Record, line: u64) -> Option<u64> {
        (record.epoch == self.epoch && line != NO_LINE && id(line) >= self.base).then_some(line)
    }
}

/// A line id as a signed number, for comparing with a base that a resize
/// may have made negative.
fn id(line: u64) -> i64 {
    i64::try_from(line).unwrap_or(i64::MAX)
}

/// The commands of one session (S5: at most [`MAX_COMMAND_RECORDS`]).
#[derive(Debug, Default)]
pub(crate) struct CommandLog {
    records: VecDeque<Record>,
    pending: Option<Pending>,
    /// Whether the shell ever reported a prompt (`A`): without one no line
    /// carries a [`Mark::PromptStart`], so the prompt keys page at once.
    prompt_seen: bool,
}

impl CommandLog {
    /// Applies one reported mark on line `line` of epoch `epoch`,
    /// processed at `now`, and hands each event it makes to `emit` (at
    /// most two: a `C` that ends the running command and starts the next).
    ///
    /// * `A` ends the running command (exit status unknown) and starts a
    ///   prompt, replacing one that never ran a command (an empty Enter,
    ///   Ctrl-C at the prompt, a redraw).
    /// * `B` sets the prompt's command line; ignored without a prompt or
    ///   above it. The last `B` before the `C` wins.
    /// * `C` above the prompt is dropped (coalescing reorders marks);
    ///   otherwise it ends the running command and starts a new one.
    /// * `D` ends the running command; ignored without one (a prompt with
    ///   no command: R05) and above its output.
    pub(crate) fn apply(
        &mut self,
        mark: &Mark,
        line: u64,
        epoch: u64,
        now: Instant,
        mut emit: impl FnMut(SessionEvent),
    ) {
        match mark {
            Mark::PromptStart => {
                self.prompt_seen = true;
                self.finish(None, line, epoch, now, &mut emit);
                self.pending = Some(Pending {
                    prompt: line,
                    input: NO_LINE,
                    epoch,
                });
            }
            Mark::CommandStart => {
                if let Some(pending) = &mut self.pending
                    && pending.epoch == epoch
                    && line >= pending.prompt
                {
                    pending.input = line;
                }
            }
            Mark::OutputStart => {
                let pending = self
                    .pending
                    .take_if(|p| p.epoch == epoch && line < p.prompt);
                if pending.is_some() {
                    // A `C` older than the prompt: the prompt stays.
                    self.pending = pending;
                    return;
                }
                self.finish(None, line, epoch, now, &mut emit);
                let (prompt, input) = match self.pending.take() {
                    Some(p) if p.epoch == epoch => (p.prompt, p.input),
                    _ => (NO_LINE, NO_LINE),
                };
                if self.records.len() == MAX_COMMAND_RECORDS {
                    self.records.pop_front();
                }
                self.records.push_back(Record {
                    prompt,
                    input,
                    output: line,
                    end: NO_LINE,
                    epoch,
                    started: now,
                    finished: None,
                    exit_code: None,
                    anchor: id(line),
                    anchor_epoch: epoch,
                });
                emit(SessionEvent::CommandStarted);
            }
            Mark::CommandEnd { exit_code }
                if self
                    .running()
                    .is_some_and(|r| r.epoch != epoch || line >= r.output) =>
            {
                self.finish(*exit_code, line, epoch, now, &mut emit);
            }
            // A `D` with nothing running, or above its output; and `Mark`
            // is non-exhaustive: a kind this layer does not know is not a
            // command boundary.
            _ => {}
        }
    }

    /// Whether a [`Mark::PromptStart`] was ever reported: when it was not,
    /// no line of the session carries one (the terminal reports every
    /// prompt mark it sets, the last of each chunk at least), and no scan
    /// is needed to know it.
    pub(crate) fn prompt_seen(&self) -> bool {
        self.prompt_seen
    }

    /// The running command, if any.
    fn running(&self) -> Option<&Record> {
        self.records.back().filter(|r| r.finished.is_none())
    }

    /// Ends the running command, if any, at `line` with `exit_code`.
    fn finish(
        &mut self,
        exit_code: Option<i32>,
        line: u64,
        epoch: u64,
        now: Instant,
        emit: &mut impl FnMut(SessionEvent),
    ) {
        let Some(record) = self.records.back_mut().filter(|r| r.finished.is_none()) else {
            return;
        };
        record.finished = Some(now);
        record.exit_code = exit_code;
        if record.epoch == epoch {
            record.end = line;
        }
        emit(SessionEvent::CommandFinished {
            exit_code,
            duration: now.saturating_duration_since(record.started),
        });
    }

    /// Forgets the finished commands: [`crate::App::clear_scrollback`]
    /// dropped every line above the current prompt, theirs included.
    pub(crate) fn forget_finished(&mut self) {
        self.records.retain(|r| r.finished.is_none());
    }

    /// The records with a line still there (or of an older epoch, whose
    /// lines are unknown), oldest first. The line that ended a command is
    /// the next one's, so a command whose output left the history goes
    /// with it.
    pub(crate) fn records(&self, view: LineView) -> Vec<CommandRecord> {
        self.records
            .iter()
            .filter(|r| r.epoch != view.epoch || view.line(r, last_line(r)).is_some())
            .map(|r| public(r, view))
            .collect()
    }

    /// The command line `line` belongs to: from its prompt (or its first
    /// known line) up to, not including, the line that ended it; a
    /// running command to the end. The newest such command; `None` for a
    /// line no record of this epoch covers.
    pub(crate) fn command_at(&self, line: u64, view: LineView) -> Option<CommandRecord> {
        if id(line) < view.base {
            return None;
        }
        self.records
            .iter()
            .rev()
            .filter(|r| r.epoch == view.epoch)
            .find(|r| {
                let start = r.prompt.min(r.input).min(r.output);
                let end = if r.finished.is_some() {
                    r.end
                } else {
                    u64::MAX
                };
                start <= line && line < end
            })
            .map(|r| public(r, view))
    }

    /// Whether the newest finished command printed something that is still
    /// there, as far as the records tell: what the menu offers Select Last
    /// Output for. True for a record of an older epoch (unknown), so the
    /// menu never hides output the grid still has; Select Last Output reads
    /// the grid and may still find nothing.
    pub(crate) fn has_output(&self, view: LineView) -> bool {
        self.records
            .iter()
            .rev()
            .find(|r| r.finished.is_some())
            .is_some_and(|r| {
                r.epoch != view.epoch || (view.line(r, r.output).is_some() && r.output < r.end)
            })
    }

    /// The line of the newest finished command's `C` ([`Record::anchor`]),
    /// when known in line epoch `epoch`.
    fn last_output_line(&self, epoch: u64) -> Option<i64> {
        self.records
            .iter()
            .rev()
            .find(|r| r.finished.is_some())
            .filter(|r| r.anchor_epoch == epoch)
            .map(|r| r.anchor)
    }
}

/// The newest line of `record` it knows: its prompt, command line or
/// output, or the line before what ended it (that line is the next
/// prompt's).
fn last_line(record: &Record) -> u64 {
    let end = (record.end != NO_LINE).then(|| record.end.saturating_sub(1));
    [record.prompt, record.input, record.output]
        .into_iter()
        .filter(|&line| line != NO_LINE)
        .chain(end)
        .max()
        .unwrap_or(NO_LINE)
}

fn public(record: &Record, view: LineView) -> CommandRecord {
    CommandRecord {
        prompt_line: view.line(record, record.prompt),
        input_line: view.line(record, record.input),
        output_line: view.line(record, record.output),
        end_line: view.line(record, record.end),
        started: record.started,
        finished: record.finished,
        exit_code: record.exit_code,
        state: if record.finished.is_some() {
            CommandState::Finished
        } else {
            CommandState::Running
        },
    }
}

/// Whether the newest finished command in `session` printed something that
/// is still there, as its records tell and with its `C` still on its line
/// (ED 2 or DL erases the mark with the row): what the menu offers Select
/// Last Output for ([`crate::App::has_command_output`]). `false` on the
/// alternate screen.
pub(crate) fn has_command_output(session: &Session) -> bool {
    session.with_terminal(|term| {
        let grid = term.grid();
        if grid.is_alt_screen() {
            return false;
        }
        let view = LineView::of(term);
        // The log's lock is a leaf: taking it under the terminal's is fine.
        let log = session.reports().commands();
        log.has_output(view)
            && log.last_output_line(view.epoch).is_none_or(|line| {
                let index = line
                    .checked_sub(view.base)
                    .and_then(|index| usize::try_from(index).ok());
                index.is_some_and(|index| has(grid.logical_line(index), &Mark::OutputStart))
            })
    })
}

/// Where a record's `C` stood before a resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rank {
    /// The `n`th line with a [`Mark::OutputStart`] from the bottom (1 is
    /// the bottom-most).
    Nth(usize),
    /// Its line lost the mark (erased).
    Erased,
    /// Its line left the history.
    Evicted,
}

/// The id of the oldest line of `grid`.
fn base_of(grid: &Grid) -> i64 {
    id(grid.scrollback_pushed()) - id(grid.scrollback_len() as u64)
}

/// The number of lines in `grid` (history and screen).
fn line_count(grid: &Grid) -> usize {
    grid.scrollback_len() + usize::from(grid.rows())
}

/// The rank of the `C` on line `anchor` of `grid`; `None` when more than
/// [`MAX_PROMPT_SCAN_LINES`] lines lie below it (Select would not reach
/// it either).
fn rank(grid: &Grid, anchor: i64) -> Option<Rank> {
    let Some(index) = anchor
        .checked_sub(base_of(grid))
        .map(|index| usize::try_from(index).ok())
    else {
        return Some(Rank::Erased);
    };
    let Some(index) = index else {
        return Some(Rank::Evicted);
    };
    let count = line_count(grid);
    if index >= count || !has(grid.logical_line(index), &Mark::OutputStart) {
        return Some(Rank::Erased);
    }
    if count - index > MAX_PROMPT_SCAN_LINES {
        return None;
    }
    let below = (index..count)
        .filter(|&i| has(grid.logical_line(i), &Mark::OutputStart))
        .count();
    Some(Rank::Nth(below))
}

/// The index of the `n`th line with a [`Mark::OutputStart`] from the
/// bottom of `grid`, within [`MAX_PROMPT_SCAN_LINES`] lines.
fn nth_from_bottom(grid: &Grid, n: usize) -> Option<usize> {
    (0..line_count(grid))
        .rev()
        .take(MAX_PROMPT_SCAN_LINES)
        .filter(|&i| has(grid.logical_line(i), &Mark::OutputStart))
        .nth(n.checked_sub(1)?)
}

/// Resizes `term` to `rows` × `cols` and carries the `C` lines of the last
/// two records (the newest finished command and a running one) into the
/// new line epoch ([`Record::anchor`]), so Select Last Output still tells
/// an erased `C` from an older command's after a resize.
///
/// A re-wrap keeps every mark and its order (D-MARKPOS) and drops lines
/// only at the top, so a `C`'s rank from the bottom among the lines with a
/// `C` names its line before and after. A `C` erased before the resize
/// stays erased; one that left the history (or more than
/// [`MAX_PROMPT_SCAN_LINES`] lines up after the re-wrap) is below the
/// oldest line. Anchors are not carried on the alternate screen (the
/// primary screen's lines are stashed) nor when more than
/// [`MAX_PROMPT_SCAN_LINES`] lines lie below them: those records keep
/// their old epoch and the grid alone decides, as for any record of an
/// older epoch. O(lines below the anchors), under the terminal lock, next
/// to the re-wrap's O(cells).
pub(crate) fn resize(log: &mut CommandLog, term: &mut Terminal, rows: u16, cols: u16) {
    let epoch = term.line_epoch();
    let ranks: Vec<(usize, Rank)> = if term.grid().is_alt_screen() {
        Vec::new()
    } else {
        let grid = term.grid();
        let from = log.records.len().saturating_sub(2);
        log.records
            .iter()
            .enumerate()
            .skip(from)
            .filter(|(_, r)| r.anchor_epoch == epoch)
            .filter_map(|(i, r)| Some((i, rank(grid, r.anchor)?)))
            .collect()
    };
    term.resize(rows, cols);
    let new_epoch = term.line_epoch();
    if new_epoch == epoch {
        return;
    }
    let grid = term.grid();
    let below_oldest = base_of(grid) - 1;
    for (i, rank) in ranks {
        let anchor = match rank {
            Rank::Nth(n) => nth_from_bottom(grid, n)
                .map_or(below_oldest, |index| base_of(grid) + id(index as u64)),
            Rank::Erased => ERASED,
            Rank::Evicted => below_oldest,
        };
        if let Some(record) = log.records.get_mut(i) {
            record.anchor = anchor;
            record.anchor_epoch = new_epoch;
        }
    }
}

// ------------------------------------------------------------------ scans

/// How a scan ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// The line the predicate held for.
    Found(i64),
    /// It reached its bound (or the oldest line) without finding one.
    Exhausted,
    /// It looked at its budget of lines without finding one.
    Capped,
    /// The line ids moved (a resize) or the alternate screen came up.
    Invalid,
}

/// Which way a scan walks, and how far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    /// Towards older lines, down to the oldest line left.
    Up,
    /// Towards newer lines, up to (not including) the screen's top row
    /// at the time of each look: the history only.
    DownToScreen,
}

/// Walks from line `at` in the direction of `bound`, a chunk of
/// [`PROMPT_SCAN_LINES_PER_LOCK`] lines per call of `source`, until `hit`
/// holds for a line (given the grid and the line's index in it), taking
/// one line of `budget` per line looked at.
fn scan(
    source: &impl TerminalSource,
    epoch: u64,
    mut at: i64,
    bound: Bound,
    budget: &mut usize,
    mut hit: impl FnMut(&Grid, usize) -> bool,
) -> Scan {
    loop {
        let step = source.read_terminal(|term, _| {
            let grid = term.grid();
            if term.line_epoch() != epoch || grid.is_alt_screen() {
                return Some(Scan::Invalid);
            }
            let top = id(grid.scrollback_pushed());
            let base = top - id(grid.scrollback_len() as u64);
            if bound == Bound::DownToScreen {
                // Evicted meanwhile: go on from the oldest line left.
                at = at.max(base);
            }
            for _ in 0..PROMPT_SCAN_LINES_PER_LOCK {
                let inside = match bound {
                    Bound::Up => at >= base,
                    Bound::DownToScreen => at < top,
                };
                if !inside {
                    return Some(Scan::Exhausted);
                }
                if *budget == 0 {
                    return Some(Scan::Capped);
                }
                *budget -= 1;
                let index = usize::try_from(at - base).unwrap_or(usize::MAX);
                if hit(grid, index) {
                    return Some(Scan::Found(at));
                }
                at += if bound == Bound::Up { -1 } else { 1 };
            }
            None
        });
        if let Some(scan) = step {
            return scan;
        }
    }
}

fn has(line: Option<&Line>, mark: &Mark) -> bool {
    line.is_some_and(|line| line.marks.contains(mark))
}

/// Whether the line at `index` starts a prompt: it has a
/// [`Mark::PromptStart`] and is not the continuation of a prompt that
/// soft-wrapped into it (a redrawn long prompt marks both rows: one stop).
/// A secondary prompt (`k=s`) has no mark at all.
fn prompt_stop(grid: &Grid, index: usize) -> bool {
    has(grid.logical_line(index), &Mark::PromptStart)
        && !index.checked_sub(1).is_some_and(|above| {
            grid.logical_line(above)
                .is_some_and(|line| line.wrapped && line.marks.contains(&Mark::PromptStart))
        })
}

fn has_prompt(grid: &Grid, index: usize) -> bool {
    has(grid.logical_line(index), &Mark::PromptStart)
}

/// The view's position for a scan, read in one look.
struct Start {
    epoch: u64,
    alt: bool,
    /// The id of the view's top row.
    top: i64,
}

fn start(session: &Session) -> Start {
    session.with_viewport(|term, offset| {
        let grid = term.grid();
        Start {
            epoch: term.line_epoch(),
            alt: grid.is_alt_screen(),
            top: id(grid.scrollback_pushed()) - id(offset as u64),
        }
    })
}

/// [`ScrollTarget::PreviousPrompt`] (`previous`) or
/// [`ScrollTarget::NextPrompt`] in `session`; returns whether the view
/// moved.
///
/// The prompt found becomes the view's top row (kitty), a prompt on the
/// screen returns the view to the live screen. Previous looks strictly
/// above the top row; next strictly below it, in the history: with none
/// there the view returns to the live screen. At the oldest prompt
/// previous stays. With no [`Mark::PromptStart`] in the history at all,
/// the keys page instead ([`ScrollTarget::PageUp`] /
/// [`ScrollTarget::PageDown`], D4), so history from before the shell
/// loaded its integration stays reachable: at once, without a scan, when
/// the shell never reported a prompt, and also when the look for any
/// prompt in the history reached [`MAX_PROMPT_SCAN_LINES`]. Nothing moves
/// on the alternate screen, after a resize during the scan, or when the
/// look for the next prompt itself reached the cap with prompts reported
/// (one may be further away: N13).
pub(crate) fn scroll_to_prompt(session: &Session, previous: bool) -> bool {
    let start = start(session);
    if start.alt {
        return false;
    }
    let page = if previous {
        ScrollTarget::PageUp
    } else {
        ScrollTarget::PageDown
    };
    if !session.reports().commands().prompt_seen() {
        return session.scroll_to(page);
    }
    let mut budget = MAX_PROMPT_SCAN_LINES;
    let epoch = start.epoch;
    if previous {
        match scan(
            session,
            epoch,
            start.top - 1,
            Bound::Up,
            &mut budget,
            prompt_stop,
        ) {
            Scan::Found(line) => session.scroll_to_line(line, epoch),
            Scan::Exhausted => {
                match scan(
                    session,
                    epoch,
                    start.top,
                    Bound::DownToScreen,
                    &mut budget,
                    has_prompt,
                ) {
                    // None in the history, or none within the cap.
                    Scan::Exhausted | Scan::Capped => session.scroll_to(page),
                    // At the oldest prompt.
                    Scan::Found(_) | Scan::Invalid => false,
                }
            }
            Scan::Capped | Scan::Invalid => false,
        }
    } else {
        match scan(
            session,
            epoch,
            start.top + 1,
            Bound::DownToScreen,
            &mut budget,
            prompt_stop,
        ) {
            Scan::Found(line) => session.scroll_to_line(line, epoch),
            Scan::Exhausted => {
                // Nothing newer in the history: whether there is any prompt
                // in it decides between the live screen and a page.
                let from = start.top.min(top_of(session) - 1);
                match scan(session, epoch, from, Bound::Up, &mut budget, has_prompt) {
                    Scan::Found(_) => session.scroll_to(ScrollTarget::Bottom),
                    Scan::Exhausted | Scan::Capped => session.scroll_to(page),
                    Scan::Invalid => false,
                }
            }
            Scan::Capped | Scan::Invalid => false,
        }
    }
}

/// The id of the screen's top row now.
fn top_of(session: &Session) -> i64 {
    session.read_terminal(|term, _| id(term.grid().scrollback_pushed()))
}

/// What a backwards look for the last finished command's output found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    /// Its first line, and the first line of the prompt after it.
    Lines(i64, i64),
    /// The last finished command printed nothing.
    Empty,
}

/// Selects the output of the last finished command in `session`, a running
/// one skipped; returns whether it selected anything.
///
/// From the bottom up: a line with a [`Mark::OutputStart`] starts a
/// command's output, which ends before the next line with a
/// [`Mark::PromptStart`]; one with no prompt below it is still running.
/// The selection runs from the start of the output to the last cell with
/// text before that prompt: interior blank lines are kept, trailing blank
/// lines and cells dropped (also on a row that soft-wraps into the prompt,
/// as zsh's `PROMPT_SP` leaves it). Nothing is selected (and a selection
/// there was stays) when the last finished command printed nothing (its
/// output starts on the next prompt's line, or holds only blanks), when
/// its first line left the history or lost its `C` (a screen clear or a
/// deleted line erased it: an older command's output is not offered
/// instead), without marks, on the alternate
/// screen, or when the scan reached [`MAX_PROMPT_SCAN_LINES`]. The view
/// scrolls so the start of the output is visible when it was not.
///
/// Rows are read whole: a row with an `A` before its `C` (the shell sent
/// `C` on the prompt's row) starts the output there, prompt included, and
/// a last output line without a newline on the next prompt's row (bash
/// without `PROMPT_SP`) is left out.
pub(crate) fn select_command_output(session: &Session) -> bool {
    let (epoch, alt, bottom) = session.read_terminal(|term, _| {
        let grid = term.grid();
        let bottom = id(grid.scrollback_pushed()) + i64::from(grid.rows()) - 1;
        (term.line_epoch(), grid.is_alt_screen(), bottom)
    });
    if alt {
        return false;
    }
    let mut budget = MAX_PROMPT_SCAN_LINES;
    let mut found = None;
    let mut next_prompt: Option<i64> = None;
    let mut line = bottom;
    let result = scan(
        session,
        epoch,
        bottom,
        Bound::Up,
        &mut budget,
        |grid, index| {
            let this = line;
            line -= 1;
            let Some(row) = grid.logical_line(index) else {
                return false;
            };
            let at = |kind: &Mark| row.marks.iter().position(|mark| mark == kind);
            let prompt = at(&Mark::PromptStart);
            match (at(&Mark::OutputStart), prompt) {
                // `C D A` on one row: the command that the prompt ended
                // printed nothing.
                (Some(output), Some(prompt)) if output < prompt => {
                    found = Some(Output::Empty);
                    return true;
                }
                // Its output, up to the prompt below; with none below it
                // is still running and skipped.
                (Some(_), _) => {
                    if let Some(end) = next_prompt {
                        found = Some(Output::Lines(this, end));
                        return true;
                    }
                }
                _ => {}
            }
            if prompt.is_some() {
                next_prompt = Some(this);
            }
            false
        },
    );
    let (Scan::Found(_), Some(Output::Lines(first, end))) = (result, found) else {
        return false;
    };
    // A `C` older than the newest finished command's belongs to an older
    // command: that command's own output rows were erased (ED 2, DL).
    // A newer one is the grid's (the records lost it to coalescing).
    // The newest finished command's `C` is followed across resizes
    // ([`resize`]).
    let recorded = session.reports().commands().last_output_line(epoch);
    if recorded.is_some_and(|line| first < line) {
        return false;
    }
    // The last line with text before the prompt, not above the output's
    // first, and its last cell with text.
    let mut line = end - 1;
    let mut col = None;
    let result = scan(
        session,
        epoch,
        end - 1,
        Bound::Up,
        &mut budget,
        |grid, index| {
            let this = line;
            line -= 1;
            col = grid.logical_line(index).and_then(|row| {
                row.cells
                    .iter()
                    .rposition(|cell| !selection::is_blank(cell))
            });
            this < first || col.is_some()
        },
    );
    let (Scan::Found(last), Some(col)) = (result, col) else {
        return false;
    };
    if last < first {
        // Only blanks.
        return false;
    }
    let col = u16::try_from(col).unwrap_or(u16::MAX);
    let selected = session.with_selection(|selection, term, offset| {
        if term.line_epoch() != epoch {
            return None;
        }
        let grid = term.grid();
        if !selection.select_lines(grid, first, last, col) {
            return None;
        }
        let view_top = id(grid.scrollback_pushed()) - id(offset as u64);
        let view_bottom = view_top + i64::from(grid.rows()) - 1;
        Some(!(view_top..=view_bottom).contains(&first))
    });
    match selected {
        Some(reveal) => {
            if reveal {
                session.scroll_to_line(first, epoch);
            }
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const fn d(exit_code: Option<i32>) -> Mark {
        Mark::CommandEnd { exit_code }
    }

    /// A log fed with `(mark, line, ms)` at a fake clock.
    fn log_of(marks: &[(Mark, u64, u64)]) -> (CommandLog, Vec<SessionEvent>, Instant) {
        let t0 = Instant::now();
        let mut log = CommandLog::default();
        let mut events = Vec::new();
        for (mark, line, ms) in marks {
            log.apply(mark, *line, 0, t0 + Duration::from_millis(*ms), |event| {
                events.push(event);
            });
        }
        (log, events, t0)
    }

    const VIEW: LineView = LineView { epoch: 0, base: 0 };

    fn finished(ms: u64) -> SessionEvent {
        SessionEvent::CommandFinished {
            exit_code: Some(0),
            duration: Duration::from_millis(ms),
        }
    }

    #[test]
    fn r01_full_cycle() {
        use Mark::*;
        let (log, events, t0) = log_of(&[
            (PromptStart, 10, 0),
            (CommandStart, 10, 0),
            (OutputStart, 11, 100),
            (d(Some(0)), 13, 350),
            (PromptStart, 13, 350),
        ]);
        let records = log.records(VIEW);
        assert_eq!(records.len(), 1, "the pending prompt is no record");
        let r = records[0];
        assert_eq!(
            (r.prompt_line, r.input_line, r.output_line, r.end_line),
            (Some(10), Some(10), Some(11), Some(13))
        );
        assert_eq!(r.started, t0 + Duration::from_millis(100));
        assert_eq!(r.finished, Some(t0 + Duration::from_millis(350)));
        assert_eq!((r.exit_code, r.state), (Some(0), CommandState::Finished));
        assert_eq!(r.duration(), Some(Duration::from_millis(250)));
        assert_eq!(events, [SessionEvent::CommandStarted, finished(250)]);
        assert_eq!(log.pending.map(|p| p.prompt), Some(13));
    }

    #[test]
    fn r02_exit_codes() {
        for (payload, exit) in [
            ("D", None),
            ("D;0", Some(0)),
            ("D;1", Some(1)),
            ("D;130", Some(130)),
            ("D;255", Some(255)),
            ("D;-1", Some(-1)),
            ("D;+3", Some(3)),
            ("D;", None),
            ("D;abc", None),
            ("D;0;aid=1", Some(0)),
            ("D;;aid=1", None),
            ("D;99999999999", None),
        ] {
            let mark = Mark::from_osc133(payload.as_bytes()).unwrap_or_else(|| panic!("{payload}"));
            let (log, events, _) = log_of(&[(Mark::OutputStart, 0, 0), (mark, 1, 5)]);
            let r = log.records(VIEW)[0];
            assert_eq!(
                (r.exit_code, r.state),
                (exit, CommandState::Finished),
                "{payload}"
            );
            assert_eq!(
                events[1],
                SessionEvent::CommandFinished {
                    exit_code: exit,
                    duration: Duration::from_millis(5)
                }
            );
        }
    }

    #[test]
    fn r03_missing_d_closed_by_next_a() {
        use Mark::*;
        let (log, events, t0) = log_of(&[
            (PromptStart, 0, 0),
            (OutputStart, 1, 10),
            (PromptStart, 5, 900),
        ]);
        let r = log.records(VIEW)[0];
        assert_eq!(
            (r.exit_code, r.end_line, r.state),
            (None, Some(5), CommandState::Finished)
        );
        assert_eq!(r.finished, Some(t0 + Duration::from_millis(900)));
        assert_eq!(
            events[1],
            SessionEvent::CommandFinished {
                exit_code: None,
                duration: Duration::from_millis(890)
            }
        );
        assert_eq!(log.pending.map(|p| p.prompt), Some(5));
    }

    #[test]
    fn r04_c_without_a() {
        let (log, _, _) = log_of(&[(Mark::OutputStart, 4, 0), (d(Some(2)), 6, 50)]);
        let r = log.records(VIEW)[0];
        assert_eq!(
            (
                r.prompt_line,
                r.input_line,
                r.output_line,
                r.end_line,
                r.exit_code
            ),
            (None, None, Some(4), Some(6), Some(2))
        );
        assert_eq!(r.duration(), Some(Duration::from_millis(50)));
    }

    #[test]
    fn r05_d_without_c_is_ignored_by_records() {
        use Mark::*;
        let (log, events, _) = log_of(&[
            (PromptStart, 0, 0),
            (CommandStart, 0, 1),
            (d(None), 1, 2),
            (PromptStart, 1, 2),
        ]);
        assert_eq!(log.records(VIEW), []);
        assert_eq!(events, []);
        assert_eq!(log.pending.map(|p| p.prompt), Some(1));
    }

    #[test]
    fn r06_a_redraw_before_c() {
        use Mark::*;
        let (log, _, _) = log_of(&[
            (PromptStart, 10, 0),
            (CommandStart, 10, 0),
            (PromptStart, 30, 500),
            (CommandStart, 30, 500),
        ]);
        assert_eq!(log.records(VIEW), []);
        let pending = log.pending.expect("one pending prompt");
        assert_eq!((pending.prompt, pending.input), (30, 30));
    }

    /// `A;k=s` parses to no mark, so it never reaches the log.
    #[test]
    fn r07_secondary_prompt_changes_nothing() {
        use Mark::*;
        assert_eq!(Mark::from_osc133(b"A;k=s"), None);
        let (log, _, _) = log_of(&[
            (PromptStart, 0, 0),
            (CommandStart, 0, 0),
            (OutputStart, 2, 20),
        ]);
        let r = log.records(VIEW)[0];
        assert_eq!(
            (r.prompt_line, r.input_line, r.output_line),
            (Some(0), Some(0), Some(2))
        );
    }

    #[test]
    fn r08_b_edge_cases() {
        use Mark::*;
        // (a) No prompt: nothing.
        let (log, _, _) = log_of(&[(CommandStart, 3, 0)]);
        assert!(log.pending.is_none() && log.records.is_empty());
        // (b) After C: the running record keeps its input line.
        let (log, _, _) = log_of(&[
            (PromptStart, 0, 0),
            (CommandStart, 0, 0),
            (OutputStart, 1, 1),
            (CommandStart, 2, 2),
        ]);
        assert_eq!(log.records(VIEW)[0].input_line, Some(0));
        // (c) The last B before C wins; one above the prompt is ignored.
        let (log, _, _) = log_of(&[
            (PromptStart, 5, 0),
            (CommandStart, 5, 0),
            (CommandStart, 6, 1),
            (CommandStart, 4, 1),
            (OutputStart, 7, 2),
        ]);
        assert_eq!(log.records(VIEW)[0].input_line, Some(6));
    }

    #[test]
    fn r09_aid_ignored_nested_shells() {
        use Mark::*;
        let (log, events, _) = log_of(&[
            (PromptStart, 0, 0),
            (OutputStart, 1, 1),
            (PromptStart, 2, 50),
            (OutputStart, 3, 60),
            (d(Some(0)), 4, 70),
            (PromptStart, 4, 70),
            (d(Some(0)), 10, 900),
            (PromptStart, 10, 900),
        ]);
        let records = log.records(VIEW);
        assert_eq!(records.len(), 2);
        assert_eq!((records[0].exit_code, records[0].end_line), (None, Some(2)));
        assert_eq!(
            (records[1].exit_code, records[1].end_line),
            (Some(0), Some(4))
        );
        // The outer D at t=900 has no running command.
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn r10_clock_and_ordering() {
        let (log, events, _) = log_of(&[(Mark::OutputStart, 0, 100), (d(Some(0)), 1, 100)]);
        assert_eq!(log.records(VIEW)[0].duration(), Some(Duration::ZERO));
        assert_eq!(events[1], finished(0));
        // A clock that went backwards saturates.
        let mut log = CommandLog::default();
        let t = Instant::now() + Duration::from_secs(1);
        log.apply(&Mark::OutputStart, 0, 0, t, |_| {});
        let mut got = Vec::new();
        let earlier = t.checked_sub(Duration::from_millis(5)).unwrap();
        log.apply(&d(Some(0)), 1, 0, earlier, |e| got.push(e));
        assert_eq!(got, [finished(0)]);
    }

    /// Events carry no command text: their whole payload is an exit
    /// status and a duration (the type has no other field).
    #[test]
    fn r11_events_internal_only() {
        let (_, events, _) = log_of(&[(Mark::OutputStart, 0, 0), (d(Some(7)), 1, 3)]);
        assert_eq!(
            events,
            [
                SessionEvent::CommandStarted,
                SessionEvent::CommandFinished {
                    exit_code: Some(7),
                    duration: Duration::from_millis(3)
                }
            ]
        );
    }

    #[test]
    fn r12_record_fields_after_reflow() {
        use Mark::*;
        let (mut log, _, _) = log_of(&[
            (PromptStart, 10, 0),
            (CommandStart, 10, 0),
            (OutputStart, 11, 100),
            (d(Some(0)), 13, 350),
            (PromptStart, 13, 350),
        ]);
        let after = LineView { epoch: 1, base: 0 };
        let r = log.records(after)[0];
        assert_eq!(
            (r.exit_code, r.duration()),
            (Some(0), Some(Duration::from_millis(250)))
        );
        assert_eq!(
            (r.prompt_line, r.output_line, r.end_line),
            (None, None, None)
        );
        assert_eq!(log.command_at(11, after), None);
        assert!(log.command_at(11, VIEW).is_some());
        // New commands after the resize resolve.
        let t = Instant::now();
        log.apply(&PromptStart, 20, 1, t, |_| {});
        log.apply(&OutputStart, 21, 1, t, |_| {});
        log.apply(&d(Some(1)), 23, 1, t, |_| {});
        let r = log.command_at(22, after).expect("the new command");
        assert_eq!(
            (r.prompt_line, r.output_line, r.exit_code),
            (Some(20), Some(21), Some(1))
        );
    }

    /// E06: a resize while a command runs: it still finishes with its
    /// status and time, its lines unknown.
    #[test]
    fn e06_resize_during_running_command() {
        let t = Instant::now();
        let mut log = CommandLog::default();
        log.apply(&Mark::PromptStart, 0, 0, t, |_| {});
        log.apply(&Mark::OutputStart, 1, 0, t, |_| {});
        log.apply(&d(Some(0)), 0, 1, t + Duration::from_millis(9), |_| {});
        let view = LineView { epoch: 1, base: 0 };
        let r = log.records(view)[0];
        assert_eq!(
            (r.state, r.exit_code, r.output_line, r.end_line),
            (CommandState::Finished, Some(0), None, None)
        );
        assert_eq!(r.duration(), Some(Duration::from_millis(9)));
        assert!(log.has_output(view), "unknown lines: the grid decides");
    }

    /// T06's reordering reaches the log as B@0, A@1, C@2: no panic, and
    /// the C below the prompt starts the command; a C above it is dropped.
    #[test]
    fn coalesced_marks_out_of_order() {
        use Mark::*;
        let (log, _, _) = log_of(&[
            (CommandStart, 0, 0),
            (PromptStart, 1, 0),
            (OutputStart, 2, 0),
        ]);
        let r = log.records(VIEW)[0];
        assert_eq!(
            (r.prompt_line, r.input_line, r.output_line),
            (Some(1), None, Some(2))
        );
        let (log, _, _) = log_of(&[(PromptStart, 5, 0), (OutputStart, 4, 0)]);
        assert_eq!(log.records(VIEW), []);
        assert_eq!(log.pending.map(|p| p.prompt), Some(5));
        // A D above the running command's output is ignored.
        let (log, _, _) = log_of(&[(OutputStart, 5, 0), (d(Some(1)), 4, 0)]);
        assert_eq!(log.records(VIEW)[0].state, CommandState::Running);
    }

    #[test]
    fn e01_ring_cap() {
        use Mark::*;
        let mut log = CommandLog::default();
        let t = Instant::now();
        for i in 0..=MAX_COMMAND_RECORDS as u64 {
            let line = i * 3;
            log.apply(&PromptStart, line, 0, t, |_| {});
            log.apply(&OutputStart, line + 1, 0, t, |_| {});
            log.apply(&d(i32::try_from(i).ok()), line + 2, 0, t, |_| {});
        }
        let records = log.records(VIEW);
        assert_eq!(records.len(), MAX_COMMAND_RECORDS);
        assert_eq!(records[0].exit_code, Some(1), "command #0 gave way");
        assert_eq!(records[MAX_COMMAND_RECORDS - 1].exit_code, Some(1000));
        assert!(size_of::<Record>() <= 96, "{} bytes", size_of::<Record>());
        assert_eq!(
            log.records.capacity().min(2 * MAX_COMMAND_RECORDS),
            log.records.capacity()
        );
    }

    /// E03: records whose lines were all evicted are skipped; a line
    /// above the base is no command's.
    #[test]
    fn e03_scrollback_eviction_drops_records() {
        use Mark::*;
        let mut log = CommandLog::default();
        let t = Instant::now();
        for i in 0..20u64 {
            let line = i * 3;
            log.apply(&PromptStart, line, 0, t, |_| {});
            log.apply(&OutputStart, line + 1, 0, t, |_| {});
            log.apply(&d(Some(0)), line + 3, 0, t, |_| {});
        }
        // Lines 0..=49 are gone.
        let view = LineView { epoch: 0, base: 50 };
        let records = log.records(view);
        assert!(records.iter().all(|r| r.end_line.is_some()));
        assert_eq!(records.len(), 4, "commands ending at 51, 54, 57, 60");
        assert_eq!(records[0].output_line, None, "its output line was evicted");
        assert_eq!(log.command_at(49, view), None);
        assert!(log.command_at(52, view).is_some());
    }

    #[test]
    fn has_output_reads_the_newest_finished_command() {
        use Mark::*;
        // S03: `true` printed nothing (C and the next A on one line).
        let (log, _, _) = log_of(&[
            (OutputStart, 7, 0),
            (d(Some(0)), 8, 0),
            (PromptStart, 8, 0),
            (OutputStart, 9, 0),
            (d(Some(0)), 9, 0),
            (PromptStart, 9, 0),
        ]);
        assert!(!log.has_output(VIEW));
        // S04: the running command is skipped.
        let (log, _, _) = log_of(&[
            (OutputStart, 7, 0),
            (d(Some(0)), 8, 0),
            (PromptStart, 8, 0),
            (OutputStart, 9, 0),
        ]);
        assert!(log.has_output(VIEW));
        assert!(
            !log.has_output(LineView { epoch: 0, base: 8 }),
            "S08: evicted"
        );
        assert!(!CommandLog::default().has_output(VIEW), "S09");
        let mut log = log;
        log.forget_finished();
        assert!(!log.has_output(VIEW));
        assert_eq!(log.records(VIEW).len(), 1, "the running one stays");
    }

    /// A terminal behind a mutex that counts the lines each lock hold
    /// looks at.
    struct Counting {
        term: Mutex<Terminal>,
        holds: AtomicUsize,
    }

    impl TerminalSource for Counting {
        fn read_terminal<R>(&self, f: impl FnOnce(&Terminal, u64) -> R) -> R {
            self.holds.fetch_add(1, Ordering::Relaxed);
            f(&self.term.lock().unwrap(), 0)
        }
    }

    /// N13 and N19: a scan looks at most [`PROMPT_SCAN_LINES_PER_LOCK`]
    /// lines per hold of the lock and [`MAX_PROMPT_SCAN_LINES`] in all.
    #[test]
    fn n19_scans_hold_the_lock_a_chunk_at_a_time() {
        let mut term = Terminal::new(4, 10);
        term.set_max_scrollback(5000);
        term.advance(b"\x1b]133;A\x07");
        term.advance(&b"x\r\n".repeat(6000));
        let source = Counting {
            term: Mutex::new(term),
            holds: AtomicUsize::new(0),
        };
        let (epoch, top) =
            source.read_terminal(|t, _| (t.line_epoch(), id(t.grid().scrollback_pushed())));
        let looked = std::cell::Cell::new(0usize);
        let mut budget = MAX_PROMPT_SCAN_LINES;
        source.holds.store(0, Ordering::Relaxed);
        let found = scan(
            &source,
            epoch,
            top - 1,
            Bound::Up,
            &mut budget,
            |grid, index| {
                looked.set(looked.get() + 1);
                prompt_stop(grid, index)
            },
        );
        // The mark scrolled off with line 0, evicted at the cap.
        assert_eq!(found, Scan::Exhausted);
        let holds = source.holds.load(Ordering::Relaxed);
        assert_eq!(looked.get(), 5000, "every line of the history");
        assert_eq!(holds, 5000usize.div_ceil(PROMPT_SCAN_LINES_PER_LOCK));
        // The budget caps the scan.
        let mut budget = 100;
        let found = scan(&source, epoch, top - 1, Bound::Up, &mut budget, |_, _| {
            false
        });
        assert_eq!((found, budget), (Scan::Capped, 0));
        // A resize between holds gives up.
        source.term.lock().unwrap().resize(5, 10);
        let mut budget = MAX_PROMPT_SCAN_LINES;
        assert_eq!(
            scan(&source, epoch, top - 1, Bound::Up, &mut budget, |_, _| {
                false
            }),
            Scan::Invalid
        );
    }

    /// N09: a prompt redrawn over two rows (the first soft-wrapped into
    /// the second, both marked) is one stop, at its first row.
    #[test]
    fn n09_wrapped_prompt_counts_once() {
        let mut term = Terminal::new(4, 10);
        term.advance(b"\x1b]133;A\x07$ \r\n\x1b]133;A\x07$ abcdefgh");
        term.advance(b"ij\x1b]133;A\x07");
        let grid = term.grid();
        assert!(grid.logical_line(1).is_some_and(|l| l.wrapped));
        assert!(has_prompt(grid, 2));
        assert!(prompt_stop(grid, 0));
        assert!(prompt_stop(grid, 1));
        assert!(!prompt_stop(grid, 2), "the continuation of row 1");
    }
}
