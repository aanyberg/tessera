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
//! events and [`crate::App::command_at`], and tell Select Last Output
//! whether a `C` above the newest command's is an older command's
//! (`CommandLog::last_output_line`). The menu's
//! [`crate::App::has_command_output`] computes the same range as the
//! command (`last_output`, #334).
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
//! # The exit-status gutter
//!
//! Each prompt of a finished command in view gets a mark in the left
//! padding (#45b): `gutter` reads the lines' marks, never the records, so
//! a pasted `a; b; c` marks all three (D-COALESCE). The records give the
//! hover its duration (`command_mark_at`). The scan reads the rows in
//! view and, while the last prompt in view has not finished, at most
//! [`MAX_GUTTER_SCAN_LINES`] lines below it (S5, S6).
//!
//! # Locks
//!
//! The command log's lock is a leaf: taken alone by the reader thread, and
//! after the terminal lock was released (or while holding it) elsewhere;
//! nothing is locked while it is held.

use std::collections::VecDeque;
use std::ops::Range;
use std::time::{Duration, Instant};

use tessera_grid::{Grid, Line, LineMarks, Mark, Style};
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

/// Most lines below the view `gutter` reads to learn how the command of
/// the last prompt in view ended (S5, S6): a command whose end lies further
/// down gets no mark while the view is scrolled back that far. The rows in
/// view are read as well, so one scan reads at most the screen's rows plus
/// this many lines, under the terminal lock, at each capture that saw the
/// lines change.
pub const MAX_GUTTER_SCAN_LINES: usize = 1024;

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

/// The exit-status mark on a row of the view, as
/// [`crate::App::command_mark_at`] returns it for the hover (#45b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommandMark {
    /// The exit status the mark is drawn for: the `D` mark's on the grid,
    /// `None` when it had none or the next prompt came without one
    /// (unknown, drawn neutral).
    pub exit_code: Option<i32>,
    /// How long the command ran, from its record: `None` when no record of
    /// the current line epoch starts at this prompt and agrees on the exit
    /// status (after a resize, ADR 0039; for a command coalesced away,
    /// D-COALESCE, and the older ones whose lines it drew over in place,
    /// #368; or one whose record the ring dropped).
    pub duration: Option<Duration>,
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
    /// The `D` mark's exit status when `exit_known` ([`Record::exit_code`]);
    /// apart from an `Option` so that the flags share its word and the
    /// record stays within 96 bytes.
    exit_status: i32,
    exit_known: bool,
    /// The line id of its `C` now, in epoch `anchor_epoch`: `output` at
    /// first, carried across each resize by the `C`'s rank from the bottom
    /// ([`resize`]); [`ERASED`] once the `C` was gone at a resize or a
    /// whole-screen clear blanked it ([`CommandLog::screen_cleared`]),
    /// below the oldest line once it left the history.
    anchor: i64,
    anchor_epoch: u64,
    /// Whether a screen clear ([`CommandLog::screen_cleared`]) blanked
    /// the line of what ended it while its prompt, above the clear, stayed:
    /// its `D` (and the exit status on it) is gone from the grid, so the
    /// gutter reports the status unknown, yet the record still describes
    /// the prompt and keeps its duration (#378 M3).
    end_erased: bool,
    /// Whether its `C` came at or below the top of a whole-screen clear
    /// reported since the last `A` ([`CommandLog::clear_since_prompt`]):
    /// a chunk's clears are reported before its marks, so the clear may
    /// have come after the `C` and erased it (`clear`, `tput clear`,
    /// Ctrl-L typed ahead; #334 B1).
    cleared: bool,
}

impl Record {
    /// The `D` mark's exit status: `None` when it had none, or none came.
    fn exit_code(&self) -> Option<i32> {
        self.exit_known.then_some(self.exit_status)
    }
}

/// [`Record::epoch`] of a record whose lines were drawn over in place
/// ([`CommandLog::drawn_over`]): no line epoch reaches it, so the record
/// reports no lines and no hover pairs with it.
const REDRAWN: u64 = u64::MAX;

/// [`Record::anchor`] of a `C` that a whole-screen clear blanked, or that
/// a screen clear or a deleted line erased before a resize: newer than
/// every line.
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
    /// The top line and epoch of the whole-screen clears reported since
    /// the last `A` (the lowest top of that epoch), which a `C` reported
    /// after them may have come before ([`Record::cleared`]).
    clear_since_prompt: Option<(u64, u64)>,
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
                self.clear_since_prompt = None;
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
                    // A `C` older than the prompt: the prompt stays. It was
                    // set after that prompt's `A`, so the lines from its
                    // own down were drawn again, by a command whose `A`
                    // coalescing hid (an in-place clear, #368): the
                    // records there no longer describe them.
                    self.drawn_over(line, epoch);
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
                    exit_status: 0,
                    exit_known: false,
                    anchor: id(line),
                    anchor_epoch: epoch,
                    end_erased: false,
                    cleared: self
                        .clear_since_prompt
                        .is_some_and(|(top, at)| at == epoch && line >= top),
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

    /// Takes the lines from the records of epoch `epoch` that reach line
    /// `line` or below: those lines were drawn over in place. Such a
    /// record keeps its times and exit status, like one of an older epoch
    /// ([`REDRAWN`]).
    fn drawn_over(&mut self, line: u64, epoch: u64) {
        for record in &mut self.records {
            if record.epoch == epoch && last_line(record) >= line {
                record.epoch = REDRAWN;
            }
        }
    }

    /// Applies a clear of the whole primary screen in epoch `epoch` whose
    /// top row was line `top` ([`Terminal::take_first_clear_top`]; #377,
    /// ADR 0039): the lines from `top` down were blanked in place, so the
    /// records whose first line is there no longer describe any line
    /// ([`REDRAWN`], like [`CommandLog::drawn_over`]), a prompt there that
    /// ran no command is forgotten, and a finished record whose first line
    /// is above the clear (in the history) but whose end was not keeps the
    /// lines above `top`, ends at `top` and notes that the line that ended
    /// it was erased ([`Record::end_erased`]). A record's `C` followed into
    /// this epoch ([`Record::anchor`]) at or below `top` is [`ERASED`],
    /// whatever epoch the record's other lines belong to, so Select Last
    /// Output knows it was blanked (#334); a `C` reported after it, before
    /// the next `A`, may have come before it ([`Record::cleared`]).
    ///
    /// Clears come in order and their tops never decrease within an epoch,
    /// so the first clear after a record's `A` decides: a later one cannot
    /// reach above it. Visits each record once (S6: at most
    /// [`MAX_COMMAND_RECORDS`]), allocates nothing.
    pub(crate) fn screen_cleared(&mut self, top: u64, epoch: u64) {
        self.clear_since_prompt = Some(match self.clear_since_prompt {
            Some((earlier, at)) if at == epoch => (earlier.min(top), epoch),
            _ => (top, epoch),
        });
        self.pending
            .take_if(|p| p.epoch == epoch && p.prompt >= top);
        for record in &mut self.records {
            // Its `C`, followed into this epoch across resizes, was blanked
            // with the rest of the screen, whatever epoch its other lines
            // belong to (#334).
            if record.anchor_epoch == epoch && record.anchor >= id(top) {
                record.anchor = ERASED;
            }
            if record.epoch != epoch {
                continue;
            }
            if first_line(record) >= top {
                record.epoch = REDRAWN;
            } else if record.finished.is_some() && record.end != NO_LINE && record.end >= top {
                // Its lines from `top` down are new ones now: it ends where
                // they start (the line the next prompt is drawn on).
                record.end = top;
                record.end_erased = true;
            }
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
        record.exit_status = exit_code.unwrap_or_default();
        record.exit_known = exit_code.is_some();
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
        self.record_at(line, view).map(|r| public(r, view))
    }

    /// The record [`CommandLog::command_at`] returns, as the log keeps it.
    fn record_at(&self, line: u64, view: LineView) -> Option<&Record> {
        if id(line) < view.base {
            return None;
        }
        self.records
            .iter()
            .rev()
            .filter(|r| r.epoch == view.epoch)
            .find(|r| {
                let end = if r.finished.is_some() {
                    r.end
                } else {
                    u64::MAX
                };
                first_line(r) <= line && line < end
            })
    }

    /// How long the command whose prompt is line `line` ran, for the
    /// gutter's hover of a mark drawn with `exit_code`: the record of
    /// [`CommandLog::command_at`] if it is finished, starts at this prompt
    /// and agrees on the exit status. It agrees when the statuses are
    /// equal, or when the gutter reports none because a screen clear
    /// erased the record's `D` below its prompt ([`Record::end_erased`],
    /// #378 M3); no other loss of the `D` (a deleted line, #368) lends a
    /// record's duration to a mark it may not be.
    pub(crate) fn hover_duration(
        &self,
        line: u64,
        exit_code: Option<i32>,
        view: LineView,
    ) -> Option<Duration> {
        let record = self.record_at(line, view)?;
        let agrees = record.exit_code() == exit_code || (exit_code.is_none() && record.end_erased);
        let finished = record.finished?;
        (view.line(record, record.prompt) == Some(line) && agrees)
            .then(|| finished.saturating_duration_since(record.started))
    }

    /// The line of the newest finished command's `C` ([`Record::anchor`]),
    /// when known in line epoch `epoch`, and whether the log knows that
    /// line was blanked since: a whole-screen clear or an in-place redraw
    /// retired the record ([`REDRAWN`]), its `C` was gone at a clear or
    /// a resize ([`ERASED`]), or a clear reported with it may have erased
    /// it ([`Record::cleared`]).
    fn last_output_line(&self, epoch: u64) -> Option<(i64, bool)> {
        self.records
            .iter()
            .rev()
            .find(|r| r.finished.is_some())
            .filter(|r| r.anchor_epoch == epoch)
            .map(|r| {
                (
                    r.anchor,
                    r.epoch == REDRAWN || r.anchor == ERASED || r.cleared,
                )
            })
    }
}

/// The first line of `record`: its prompt, command line or output,
/// whichever is known and highest up ([`NO_LINE`] when none is).
fn first_line(record: &Record) -> u64 {
    record.prompt.min(record.input).min(record.output)
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
        exit_code: record.exit_code(),
        state: if record.finished.is_some() {
            CommandState::Finished
        } else {
            CommandState::Running
        },
    }
}

/// Whether `session` holds output Select Last Output selects: exactly
/// [`last_output`] found one, so the menu item and the command never
/// disagree (#334). What a frontend enables the item for
/// ([`crate::App::has_command_output`]).
pub(crate) fn has_command_output(session: &Session) -> bool {
    last_output(session).is_some()
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
    if index >= count || !has(grid.line_marks(index), &Mark::OutputStart) {
        return Some(Rank::Erased);
    }
    if count - index > MAX_PROMPT_SCAN_LINES {
        return None;
    }
    let below = grid
        .marked_lines(index..count)
        .filter(|(_, marks)| marks.contains(&Mark::OutputStart))
        .count();
    Some(Rank::Nth(below))
}

/// The index of the `n`th line with a [`Mark::OutputStart`] from the
/// bottom of `grid`, within [`MAX_PROMPT_SCAN_LINES`] lines.
fn nth_from_bottom(grid: &Grid, n: usize) -> Option<usize> {
    let count = line_count(grid);
    grid.marked_lines(count.saturating_sub(MAX_PROMPT_SCAN_LINES)..count)
        .rev()
        .filter(|(_, marks)| marks.contains(&Mark::OutputStart))
        .nth(n.checked_sub(1)?)
        .map(|(index, _)| index)
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
    at: i64,
    bound: Bound,
    budget: &mut usize,
    mut hit: impl FnMut(&Grid, usize) -> bool,
) -> Scan {
    walk(source, epoch, at, bound, budget, |grid, window| {
        if bound == Bound::Up {
            window.rev().find(|&index| hit(grid, index))
        } else {
            window.into_iter().find(|&index| hit(grid, index))
        }
    })
}

/// [`scan`] for a `hit` that holds only for lines with marks: it is given
/// those lines alone, with their marks ([`Grid::marked_lines`]), so a
/// packed history row is never decoded (#781). Lines without marks still
/// take their line of `budget` and of the chunk, as in [`scan`].
fn scan_marks(
    source: &impl TerminalSource,
    epoch: u64,
    at: i64,
    bound: Bound,
    budget: &mut usize,
    mut hit: impl FnMut(&Grid, usize, LineMarks<'_>) -> bool,
) -> Scan {
    walk(source, epoch, at, bound, budget, |grid, window| {
        let mut marked = grid.marked_lines(window);
        let found = if bound == Bound::Up {
            marked.rfind(|&(index, marks)| hit(grid, index, marks))
        } else {
            marked.find(|&(index, marks)| hit(grid, index, marks))
        };
        found.map(|(index, _)| index)
    })
}

/// The walk of [`scan`] and [`scan_marks`]: under each hold of the lock
/// it hands `find` the window of line indices (ascending) it may look at
/// in this hold, at most [`PROMPT_SCAN_LINES_PER_LOCK`] and what is left
/// of `budget`, and `find` returns the first index, in the direction of
/// `bound`, its predicate holds for. The lines up to and including the
/// one found are taken from `budget`.
fn walk(
    source: &impl TerminalSource,
    epoch: u64,
    mut at: i64,
    bound: Bound,
    budget: &mut usize,
    mut find: impl FnMut(&Grid, Range<usize>) -> Option<usize>,
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
            // Lines left before the bound.
            let room = match bound {
                Bound::Up => at - base + 1,
                Bound::DownToScreen => top - at,
            };
            let room = usize::try_from(room).unwrap_or(0);
            if room == 0 {
                return Some(Scan::Exhausted);
            }
            let n = room.min(PROMPT_SCAN_LINES_PER_LOCK).min(*budget);
            if n == 0 {
                return Some(Scan::Capped);
            }
            // At or above `base` here.
            let index = usize::try_from(at - base).unwrap_or(usize::MAX);
            let window = match bound {
                Bound::Up => index + 1 - n..index + 1,
                Bound::DownToScreen => index..index + n,
            };
            if let Some(found) = find(grid, window) {
                *budget -= found.abs_diff(index) + 1;
                return Some(Scan::Found(base + id(found as u64)));
            }
            *budget -= n;
            let step = id(n as u64);
            at += if bound == Bound::Up { -step } else { step };
            if n == PROMPT_SCAN_LINES_PER_LOCK {
                None
            } else if n == room {
                Some(Scan::Exhausted)
            } else {
                Some(Scan::Capped)
            }
        });
        if let Some(scan) = step {
            return scan;
        }
        // The reader thread locks the terminal without being counted as
        // waiting, and an unfair mutex hands it straight back to this
        // thread: let a woken waiter take it before the next hold (#334).
        std::thread::yield_now();
    }
}

fn has(line: Option<LineMarks<'_>>, mark: &Mark) -> bool {
    line.is_some_and(|line| line.contains(mark))
}

/// Whether the line at `index` starts a prompt: it has a
/// [`Mark::PromptStart`] and is not the continuation of a prompt that
/// soft-wrapped into it (a redrawn long prompt marks both rows: one stop).
/// A secondary prompt (`k=s`) has no mark at all.
fn prompt_stop(grid: &Grid, index: usize, marks: LineMarks<'_>) -> bool {
    marks.contains(&Mark::PromptStart) && !continues_prompt(grid, index)
}

/// Whether the line above `index` wraps into it with a
/// [`Mark::PromptStart`] on it: an `A` on line `index` continues that
/// prompt.
fn continues_prompt(grid: &Grid, index: usize) -> bool {
    index.checked_sub(1).is_some_and(|above| {
        grid.line_marks(above)
            .is_some_and(|line| line.wrapped && line.contains(&Mark::PromptStart))
    })
}

/// Calls `mark(row, exit_code)` for each row of the view `offset` lines
/// above `grid`'s live screen that starts the prompt of a finished command,
/// top to bottom: the exit-status gutter (#45b). `offset` is at most
/// [`Grid::scrollback_len`]; the alternate screen has no marks.
///
/// A prompt is a [`prompt_stop`]. Its command finished when a `C` follows
/// it and then a `D` (whose exit status it reports) or the next prompt (no
/// status: unknown) comes before the next prompt. A prompt with no `C`
/// before what ends it ran nothing (an empty Enter, Ctrl-C at the prompt)
/// and gets no mark; nor does a command still running, nor one whose end
/// lies more than [`MAX_GUTTER_SCAN_LINES`] lines below the view. A row's
/// marks are read in the order they were set, so zsh's `D` before `A` on
/// the next prompt's row ends the command above.
///
/// At most one call per row in view, whatever the lines hold (S6).
pub(crate) fn gutter(grid: &Grid, offset: usize, mut mark: impl FnMut(u16, Option<i32>)) {
    // Defence only: the alternate screen refuses marks (`osc_mark`), so
    // its rows have none to read and no test can tell this guard is gone.
    if grid.is_alt_screen() {
        return;
    }
    let first = grid.scrollback_len().saturating_sub(offset);
    let view_end = first + usize::from(grid.rows());
    let end = view_end
        .saturating_add(MAX_GUTTER_SCAN_LINES)
        .min(grid.total_lines());
    // The prompt in view whose command has not ended yet: its row, and
    // whether its output started (`C`).
    let mut pending: Option<(u16, bool)> = None;
    // The line above wrapped into this one with a prompt on it: this
    // line's `A` continues that prompt (a redrawn long prompt). Lines
    // without marks change nothing but this, so only the marked lines are
    // read (#781).
    let mut above: Option<(usize, LineMarks<'_>)> = first
        .checked_sub(1)
        .and_then(|index| Some((index, grid.line_marks(index)?)));
    for (index, line) in grid.marked_lines(first..end) {
        if index >= view_end && pending.is_none() {
            break;
        }
        let continues = above.is_some_and(|(at, above)| {
            at + 1 == index && above.wrapped && above.contains(&Mark::PromptStart)
        });
        above = Some((index, line));
        for kind in line.marks {
            match kind {
                Mark::PromptStart if !continues => {
                    if let Some((row, true)) = pending {
                        mark(row, None);
                    }
                    pending = (index < view_end)
                        .then(|| (u16::try_from(index - first).unwrap_or(u16::MAX), false));
                }
                Mark::OutputStart => {
                    if let Some((_, ran)) = &mut pending {
                        *ran = true;
                    }
                }
                Mark::CommandEnd { exit_code } => {
                    if let Some((row, true)) = pending.take() {
                        mark(row, *exit_code);
                    }
                }
                _ => {}
            }
        }
    }
}

/// The exit-status mark on row `row` of `session`'s view, and the duration
/// of its command when a record knows it: what the gutter's hover shows
/// (#45b). `None` for a row with no mark ([`gutter`]).
///
/// Reads the rows in view (and at most [`MAX_GUTTER_SCAN_LINES`] below)
/// under one hold of the terminal lock, and the records (at most
/// [`MAX_COMMAND_RECORDS`]) under the log's leaf lock inside it.
pub(crate) fn command_mark_at(session: &Session, row: u16) -> Option<CommandMark> {
    session.with_viewport(|term, offset| {
        let grid = term.grid();
        let offset = offset.min(grid.scrollback_len());
        let mut exit = None;
        gutter(grid, offset, |at, exit_code| {
            if at == row {
                exit = Some(exit_code);
            }
        });
        let exit_code = exit?;
        let line = grid
            .scrollback_pushed()
            .wrapping_sub(offset as u64)
            .wrapping_add(u64::from(row));
        // The log's lock is a leaf: taking it under the terminal's is fine.
        let duration =
            session
                .reports()
                .commands()
                .hover_duration(line, exit_code, LineView::of(term));
        Some(CommandMark {
            exit_code,
            duration,
        })
    })
}

fn has_prompt(_: &Grid, _: usize, marks: LineMarks<'_>) -> bool {
    marks.contains(&Mark::PromptStart)
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
        match scan_marks(
            session,
            epoch,
            start.top - 1,
            Bound::Up,
            &mut budget,
            prompt_stop,
        ) {
            Scan::Found(line) => session.scroll_to_line(line, epoch),
            Scan::Exhausted => {
                match scan_marks(
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
        match scan_marks(
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
                match scan_marks(session, epoch, from, Bound::Up, &mut budget, has_prompt) {
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
    /// Its first line, the first line of the prompt after it, and whether a
    /// command ran after that prompt's `A` ([`ran_after`]).
    Lines(i64, i64, bool),
    /// The last finished command printed nothing.
    Empty,
}

/// The output of the last finished command, as [`last_output`] finds it:
/// whole lines from the line of its `C`, ending at a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputRange {
    /// The [`Terminal::line_epoch`] the line ids belong to.
    epoch: u64,
    /// The line of its `C`.
    first: i64,
    /// The last line with text, not above `first`.
    last: i64,
    /// The last cell with text on `last`.
    col: u16,
}

/// The output of the last finished command in `session`, a running one
/// skipped: what Select Last Output selects and the menu offers it for
/// (one function, so they never disagree: #334). Computed on request, never
/// per byte of output.
///
/// From the bottom up: a line with a [`Mark::OutputStart`] starts a
/// command's output, which ends before the next line with a
/// [`Mark::PromptStart`]; one with no prompt below it is still running.
/// The range runs from the start of the output to the last cell with text
/// before that prompt: interior blank lines are kept, trailing blank lines
/// and cells dropped (also on a row that soft-wraps into the prompt).
///
/// **The end of the output.** A last output row that soft-wraps into the
/// prompt's row is a line the command left without a newline, padded by
/// the shell so its prompt starts on a row of its own (zsh's `PROMPT_SP`,
/// fish's omitted-newline mark), after the shell's end-of-line mark: the
/// run of cells with text at the end of that row drawn in reverse video or
/// dim (zsh's default `PROMPT_EOL_MARK` is a bold reverse-video `%` or
/// `#`, fish's mark a dim `⏎`) is that mark and left out
/// ([`last_text_col`]). The marks cannot say where it starts (they hold a
/// row, not a column), the wrap into the prompt's row says it is there.
///
/// **An older command's `C`.** The log's newest finished command decides
/// whether a `C` found above its own is an older command's, whose output is
/// not offered instead ([`CommandLog::last_output_line`]): when the line
/// of its `C` still carries one, or the log knows that line was blanked (a
/// whole-screen clear, an erased `C` at a resize), an older `C` is
/// refused. Otherwise an edit the log does not see (DL, IL, SU or SD, a
/// scroll region) moved the rows or erased the `C`, and the marks decide:
/// the older `C` is the moved one unless a command ran after its prompt
/// (a `D` with an exit status follows that prompt's `A`), whose `C` is
/// gone.
///
/// `None` when the last finished command printed nothing (its output
/// starts on the next prompt's line, or holds only blanks and its
/// end-of-line mark), when its first line left the history or lost its
/// `C`, without prompt marks, on the alternate screen, when the scans
/// reached [`MAX_PROMPT_SCAN_LINES`] or the line ids moved (a resize).
///
/// Rows are read whole: a row with an `A` before its `C` (the shell sent
/// `C` on the prompt's row) starts the output there, prompt included, and
/// a last output line on the next prompt's row (bash without `PROMPT_SP`,
/// or zsh's padded row merged into the prompt's by a re-wrap) is left out.
pub(crate) fn last_output(session: &Session) -> Option<OutputRange> {
    // Without a prompt ever reported no line has an `A` to end an output
    // (the terminal reports every prompt mark it sets): no scan, so the
    // menu's check costs nothing in a shell without integration.
    if !session.reports().commands().prompt_seen() {
        return None;
    }
    let (epoch, alt, bottom) = session.read_terminal(|term, _| {
        let grid = term.grid();
        let bottom = id(grid.scrollback_pushed()) + i64::from(grid.rows()) - 1;
        (term.line_epoch(), grid.is_alt_screen(), bottom)
    });
    if alt {
        return None;
    }
    let mut budget = MAX_PROMPT_SCAN_LINES;
    let mut found = None;
    let mut next_prompt: Option<i64> = None;
    // A `D` with an exit status on the rows below the one looked at.
    let mut ended_below = false;
    let mut ran = false;
    let result = scan_marks(
        session,
        epoch,
        bottom,
        Bound::Up,
        &mut budget,
        |grid, index, row| {
            let this = base_of(grid) + id(index as u64);
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
                        found = Some(Output::Lines(this, end, ran));
                        return true;
                    }
                }
                _ => {}
            }
            if let Some(prompt) = prompt {
                next_prompt = Some(this);
                ran = ended_below || ran_after(&row.marks[prompt + 1..]);
            }
            ended_below |= ran_after(row.marks);
            false
        },
    );
    let (Scan::Found(_), Some(Output::Lines(first, end, ran))) = (result, found) else {
        return None;
    };
    let refused = session.read_terminal(|term, _| {
        let grid = term.grid();
        // Only saves the look: the scan below gives up on these too.
        if term.line_epoch() != epoch || grid.is_alt_screen() {
            return true;
        }
        // The log's lock is a leaf: taking it under the terminal's is fine.
        let recorded = session.reports().commands().last_output_line(epoch);
        recorded.is_some_and(|(recorded, blanked)| {
            let kept = recorded
                .checked_sub(base_of(grid))
                .and_then(|index| usize::try_from(index).ok())
                .is_some_and(|index| has(grid.line_marks(index), &Mark::OutputStart));
            first < recorded && (kept || blanked || ran)
        })
    });
    if refused {
        return None;
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
            col = grid
                .logical_line(index)
                .and_then(|row| last_text_col(&row, this == end - 1 && row.wrapped));
            this < first || col.is_some()
        },
    );
    let (Scan::Found(last), Some(col)) = (result, col) else {
        return None;
    };
    // `last < first`: only blanks.
    (last >= first).then(|| OutputRange {
        epoch,
        first,
        last,
        col: u16::try_from(col).unwrap_or(u16::MAX),
    })
}

/// Whether `marks` hold a [`Mark::CommandEnd`] with an exit status: a
/// command ended there. A prompt that ran nothing (an empty line, Ctrl-C)
/// ends with a bare `D` from Tessera's scripts, or none.
fn ran_after(marks: &[Mark]) -> bool {
    marks
        .iter()
        .any(|mark| matches!(mark, Mark::CommandEnd { exit_code: Some(_) }))
}

/// The column of the last cell with text on `row`; with `padded` (the row
/// soft-wraps into the next prompt's: [`last_output`]) the shell's
/// end-of-line mark at its end, the cells with text drawn in reverse video
/// or dim, is left out first. `None` for a row with no text left.
fn last_text_col(row: &Line, padded: bool) -> Option<usize> {
    let end = row
        .cells
        .iter()
        .rposition(|cell| !selection::is_blank(cell))?;
    if !padded {
        return Some(end);
    }
    let mark = Style::INVERSE | Style::DIM;
    let kept = row.cells[..=end]
        .iter()
        .rposition(|cell| selection::is_blank(cell) || cell.style.flags & mark == 0)?;
    row.cells[..=kept]
        .iter()
        .rposition(|cell| !selection::is_blank(cell))
}

/// Selects the output of the last finished command in `session`
/// ([`last_output`]); returns whether it selected anything. Nothing is
/// selected (and a selection there was stays) when there is none, or the
/// line ids moved since it was found. The view scrolls so the start of the
/// output is visible when it was not.
pub(crate) fn select_command_output(session: &Session) -> bool {
    let Some(range) = last_output(session) else {
        return false;
    };
    let OutputRange {
        epoch,
        first,
        last,
        col,
    } = range;
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

    /// Only a prompt mark (`A`) tells the log the shell reports prompts
    /// (#334): `B`, `C` and `D` alone, from a script that prints them or a
    /// shell sending only some, leave the prompt keys paging at once.
    #[test]
    fn only_a_prompt_mark_sets_prompt_seen() {
        use Mark::*;
        let (log, ..) = log_of(&[
            (CommandStart, 1, 0),
            (OutputStart, 2, 0),
            (d(Some(0)), 3, 0),
            (d(None), 4, 0),
            (OutputStart, 5, 0),
        ]);
        assert!(!log.prompt_seen(), "no A, no prompt");
        let (log, ..) = log_of(&[(OutputStart, 1, 0), (PromptStart, 2, 0)]);
        assert!(log.prompt_seen());
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
        assert_eq!(
            log.last_output_line(1),
            None,
            "unknown lines: the grid decides"
        );
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

    /// #368: a `C` above the prompt was drawn after it, over the lines of
    /// the records that reach its line, which then report no lines (their
    /// times and status stay); a record above that line keeps its lines.
    #[test]
    fn a_c_above_the_prompt_takes_the_lines_of_records_it_drew_over() {
        use Mark::*;
        let (log, _, _) = log_of(&[
            (PromptStart, 0, 0),
            (OutputStart, 1, 0),
            (d(Some(0)), 3, 5),
            (PromptStart, 3, 5),
            (OutputStart, 4, 5),
            (d(Some(2)), 6, 9),
            (PromptStart, 6, 9),
            // Drawn over from line 4 down.
            (OutputStart, 4, 20),
        ]);
        let lines = |r: &CommandRecord| (r.prompt_line, r.output_line, r.end_line);
        let records = log.records(VIEW);
        assert_eq!(records.len(), 2);
        assert_eq!(lines(&records[0]), (Some(0), Some(1), Some(3)));
        assert_eq!(lines(&records[1]), (None, None, None));
        assert_eq!(records[1].exit_code, Some(2));
        assert!(log.command_at(4, VIEW).is_none());
        assert!(log.command_at(1, VIEW).is_some());
        assert_eq!(log.pending.map(|p| p.prompt), Some(6));
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

    /// The veto of Select Last Output reads the newest finished command's
    /// `C`, a running one skipped (#334: whether it has output is the
    /// grid's to say, [`last_output`]).
    #[test]
    fn last_output_line_reads_the_newest_finished_command() {
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
        assert_eq!(log.last_output_line(0), Some((9, false)));
        // S04: the running command is skipped.
        let (log, _, _) = log_of(&[
            (OutputStart, 7, 0),
            (d(Some(0)), 8, 0),
            (PromptStart, 8, 0),
            (OutputStart, 9, 0),
        ]);
        assert_eq!(log.last_output_line(0), Some((7, false)));
        assert_eq!(log.last_output_line(1), None, "another epoch's line");
        assert_eq!(CommandLog::default().last_output_line(0), None, "S09");
        let mut log = log;
        log.forget_finished();
        assert_eq!(log.last_output_line(0), None);
        assert_eq!(log.records(VIEW).len(), 1, "the running one stays");
    }

    /// A whole-screen clear marks the `C` of a record carried into this
    /// epoch by a resize as blanked, even when its other lines are another
    /// epoch's (#334).
    #[test]
    fn a_clear_blanks_a_carried_output_line() {
        use Mark::*;
        let (mut log, _, _) =
            log_of(&[(PromptStart, 3, 0), (OutputStart, 4, 0), (d(Some(0)), 6, 0)]);
        // Carried into epoch 1 at line 4 by a resize.
        for record in &mut log.records {
            record.anchor_epoch = 1;
        }
        assert_eq!(log.last_output_line(1), Some((4, false)));
        log.screen_cleared(5, 1);
        assert_eq!(log.last_output_line(1), Some((4, false)), "above the clear");
        log.screen_cleared(4, 1);
        assert_eq!(log.last_output_line(1), Some((ERASED, true)));
    }

    /// A `C` drawn above the pending prompt retires the records it drew
    /// over: the newest finished one's `C` line counts as blanked, though
    /// neither a clear nor a resize touched it (#334).
    #[test]
    fn a_record_drawn_over_reports_its_output_line_blanked() {
        use Mark::*;
        let (log, _, _) = log_of(&[
            (PromptStart, 3, 0),
            (OutputStart, 4, 0),
            (d(Some(0)), 6, 0),
            (PromptStart, 6, 0),
            (OutputStart, 4, 0),
        ]);
        assert_eq!(log.last_output_line(0), Some((4, true)));
    }

    /// A clear reported in an older epoch than the one a `C` was carried
    /// into says nothing about that line: its ids are another numbering's
    /// (#334).
    #[test]
    fn a_clear_of_an_older_epoch_leaves_a_carried_output_line() {
        use Mark::*;
        let (mut log, _, _) =
            log_of(&[(PromptStart, 3, 0), (OutputStart, 4, 0), (d(Some(0)), 6, 0)]);
        for record in &mut log.records {
            record.anchor = 8;
            record.anchor_epoch = 1;
        }
        log.screen_cleared(5, 0);
        assert_eq!(log.last_output_line(1), Some((8, false)));
    }

    /// A `C` after a clear may have come before it only if the clear
    /// reached its line: one above the clear's top, or under a clear of
    /// another epoch, is not [`Record::cleared`]; one at the top is (#334).
    #[test]
    fn only_a_c_the_clear_reached_counts_as_cleared() {
        use Mark::*;
        for (top, clear_epoch, cleared) in [(5, 0, false), (0, 1, false), (4, 0, true)] {
            let (mut log, _, _) = log_of(&[(PromptStart, 3, 0)]);
            log.screen_cleared(top, clear_epoch);
            let t = Instant::now();
            log.apply(&OutputStart, 4, 0, t, |_| {});
            log.apply(&d(Some(0)), 6, 0, t, |_| {});
            assert_eq!(
                log.last_output_line(0),
                Some((4, cleared)),
                "clear at {top} of epoch {clear_epoch}"
            );
        }
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
                grid.line_marks(index)
                    .is_some_and(|marks| prompt_stop(grid, index, marks))
            },
        );
        // The mark scrolled off with line 0, evicted at the cap.
        assert_eq!(found, Scan::Exhausted);
        let holds = source.holds.load(Ordering::Relaxed);
        assert_eq!(looked.get(), 5000, "every line of the history");
        assert_eq!(holds, 5000usize.div_ceil(PROMPT_SCAN_LINES_PER_LOCK));
        // The mark walk holds the lock as often and takes as much budget,
        // though it is handed the marked lines alone.
        let mut budget = MAX_PROMPT_SCAN_LINES;
        source.holds.store(0, Ordering::Relaxed);
        let found = scan_marks(&source, epoch, top - 1, Bound::Up, &mut budget, prompt_stop);
        assert_eq!(found, Scan::Exhausted);
        assert_eq!(MAX_PROMPT_SCAN_LINES - budget, 5000);
        let holds = source.holds.load(Ordering::Relaxed);
        assert_eq!(holds, 5000usize.div_ceil(PROMPT_SCAN_LINES_PER_LOCK));
        // The budget caps the scan.
        let mut budget = 100;
        let found = scan(&source, epoch, top - 1, Bound::Up, &mut budget, |_, _| {
            false
        });
        assert_eq!((found, budget), (Scan::Capped, 0));
        let mut budget = 100;
        let found = scan_marks(
            &source,
            epoch,
            top - 1,
            Bound::Up,
            &mut budget,
            |_, _, _| false,
        );
        assert_eq!((found, budget), (Scan::Capped, 0));
        // A budget that runs out on the oldest line reached its bound: the
        // last window (5000 = 19 * 256 + 136) is both, and it is Exhausted.
        let mut budget = 5000;
        let found = scan(&source, epoch, top - 1, Bound::Up, &mut budget, |_, _| {
            false
        });
        assert_eq!((found, budget), (Scan::Exhausted, 0));
        let mut budget = 5000;
        let found = scan_marks(
            &source,
            epoch,
            top - 1,
            Bound::Up,
            &mut budget,
            |_, _, _| false,
        );
        assert_eq!((found, budget), (Scan::Exhausted, 0));
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

    /// #368 (M10): a row that wraps into the next with a `C` but no `A`
    /// on it does not make the next row's `A` a continuation: that `A`
    /// starts a prompt of its own and gets its mark.
    #[test]
    fn an_a_below_a_wrapped_row_without_one_starts_a_prompt() {
        let mut term = Terminal::new(8, 10);
        term.advance(b"\x1b]133;A\x07$ \r\n\x1b]133;C\x070123456789ab");
        term.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \r\n\x1b]133;C\x07out\r\n");
        term.advance(b"\x1b]133;D;1\x07\x1b]133;A\x07$ ");
        let grid = term.grid();
        assert!(grid.line(1).is_some_and(|line| line.wrapped));
        let mut marks = Vec::new();
        gutter(grid, 0, |row, exit| marks.push((row, exit)));
        assert_eq!(marks, [(0, Some(0)), (2, Some(1))]);
    }

    /// The gutter as it was before #781: every line decoded and read in
    /// turn. The reference for [`gutter`], which reads marked lines only.
    fn gutter_by_decoding(grid: &Grid, offset: usize) -> Vec<(u16, Option<i32>)> {
        let mut out = Vec::new();
        let first = grid.scrollback_len().saturating_sub(offset);
        let view_end = first + usize::from(grid.rows());
        let end = view_end
            .saturating_add(MAX_GUTTER_SCAN_LINES)
            .min(grid.total_lines());
        let mut pending: Option<(u16, bool)> = None;
        let mut continues = first.checked_sub(1).is_some_and(|above| {
            grid.logical_line(above)
                .is_some_and(|line| line.wrapped && line.marks.contains(&Mark::PromptStart))
        });
        for index in first..end {
            if index >= view_end && pending.is_none() {
                break;
            }
            let line = grid.logical_line(index).unwrap();
            for kind in &line.marks {
                match kind {
                    Mark::PromptStart if !continues => {
                        if let Some((row, true)) = pending {
                            out.push((row, None));
                        }
                        pending = (index < view_end)
                            .then(|| (u16::try_from(index - first).unwrap(), false));
                    }
                    Mark::OutputStart => {
                        if let Some((_, ran)) = &mut pending {
                            *ran = true;
                        }
                    }
                    Mark::CommandEnd { exit_code } => {
                        if let Some((row, true)) = pending.take() {
                            out.push((row, *exit_code));
                        }
                    }
                    _ => {}
                }
            }
            continues = line.wrapped && line.marks.contains(&Mark::PromptStart);
        }
        out
    }

    /// A history of 1 500 commands, most of it packed (ADR 0051): every
    /// fifth prompt is long and redrawn, so it wraps into a second marked
    /// row; every seventh command runs nothing, nor does every eleventh,
    /// whose prompt wraps onto a row without marks.
    fn marked_history() -> Terminal {
        let mut term = Terminal::new(4, 12);
        term.set_max_scrollback(10_000);
        for i in 0..1_500 {
            let prompt: &[u8] = if i % 5 == 0 {
                b"\x1b]133;A\x07~/a/long/dir \x1b]133;A\x07$ "
            } else {
                b"\x1b]133;A\x07$ "
            };
            term.advance(prompt);
            if i % 7 == 0 {
                term.advance(b"\r\n");
                continue;
            }
            if i % 11 == 0 {
                // A prompt that wraps onto a row of its own, without a mark.
                term.advance(b"0123456789abc\r\n");
                continue;
            }
            term.advance(b"\x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\r\nmore\r\n");
            term.advance(format!("\x1b]133;D;{}\x07", i % 3).as_bytes());
        }
        term.advance(b"\x1b]133;A\x07$ ");
        let grid = term.grid();
        assert!(grid.scrollback_len() > 4 * tessera_grid::HOT_ROWS);
        assert_eq!(grid.cached_scrollback_cells(), 0);
        term
    }

    /// #781: a resize's ranks and the gutter read packed history rows'
    /// marks without decoding a row, and find what the decoded rows say.
    #[test]
    fn ranks_and_the_gutter_decode_no_packed_row() {
        let term = marked_history();
        let grid = term.grid();
        // The oldest and a middle `C`, and a line without one.
        let outputs: Vec<usize> = grid
            .marked_lines(0..grid.total_lines())
            .filter(|(_, m)| m.contains(&Mark::OutputStart))
            .map(|(i, _)| i)
            .collect();
        let anchors = [outputs[0], outputs[outputs.len() / 2], 3];
        let ranks: Vec<_> = anchors
            .iter()
            .map(|&index| rank(grid, base_of(grid) + id(index as u64)))
            .collect();
        let ns = [1, outputs.len() / 2, outputs.len(), outputs.len() + 1];
        let nth: Vec<_> = ns.iter().map(|&n| nth_from_bottom(grid, n)).collect();
        let offsets = [0, 100, 777, 2_000, grid.scrollback_len()];
        let gutters: Vec<_> = offsets
            .iter()
            .map(|&offset| {
                let mut marks = Vec::new();
                gutter(grid, offset, |row, exit| marks.push((row, exit)));
                marks
            })
            .collect();
        assert_eq!(
            grid.cached_scrollback_cells(),
            0,
            "a mark scan decoded a row"
        );

        // The decoded rows (these reads fill the cache).
        let c = |index: usize| {
            grid.logical_line(index)
                .unwrap()
                .marks
                .contains(&Mark::OutputStart)
        };
        let count = grid.total_lines();
        let want_ranks: Vec<_> = anchors
            .iter()
            .map(|&index| {
                Some(if c(index) {
                    Rank::Nth((index..count).filter(|&i| c(i)).count())
                } else {
                    Rank::Erased
                })
            })
            .collect();
        assert_eq!(ranks, want_ranks);
        assert_eq!(ranks[0], Some(Rank::Nth(outputs.len())));
        let want_nth: Vec<_> = ns
            .iter()
            .map(|&n| (0..count).rev().filter(|&i| c(i)).nth(n - 1))
            .collect();
        assert_eq!(nth, want_nth);
        assert_eq!(nth[2], Some(outputs[0]));
        for (offset, got) in offsets.iter().zip(&gutters) {
            assert_eq!(*got, gutter_by_decoding(grid, *offset), "offset {offset}");
        }
        assert!(
            gutters.iter().filter(|g| !g.is_empty()).count() >= 3,
            "{gutters:?}"
        );
    }

    /// #781: prompt navigation's walks read packed history rows' marks
    /// without decoding a row, and stop where the decoded rows say, having
    /// taken one line of budget per line looked at.
    #[test]
    fn prompt_walks_decode_no_packed_row() {
        for back in [3_000, 3_001, 2_222] {
            // A fresh decode cache for each walk.
            let source = Counting {
                term: Mutex::new(marked_history()),
                holds: AtomicUsize::new(0),
            };
            let (epoch, top) =
                source.read_terminal(|t, _| (t.line_epoch(), id(t.grid().scrollback_pushed())));
            let from = top - back;
            let mut up_budget = MAX_PROMPT_SCAN_LINES;
            let up = scan_marks(&source, epoch, from, Bound::Up, &mut up_budget, prompt_stop);
            let mut down_budget = MAX_PROMPT_SCAN_LINES;
            let down = scan_marks(
                &source,
                epoch,
                from,
                Bound::DownToScreen,
                &mut down_budget,
                prompt_stop,
            );
            let term = source.term.lock().unwrap();
            let grid = term.grid();
            assert_eq!(
                grid.cached_scrollback_cells(),
                0,
                "a mark scan decoded a row"
            );
            // The nearest prompt stops above and below line `from`, decoded.
            let at = usize::try_from(from - base_of(grid)).unwrap();
            let stop = |i: usize| {
                let marked = |i: usize| {
                    grid.logical_line(i)
                        .is_some_and(|l| l.marks.contains(&Mark::PromptStart))
                };
                marked(i) && !(marked(i - 1) && grid.logical_line(i - 1).unwrap().wrapped)
            };
            let above = (0..=at).rev().find(|&i| stop(i)).unwrap();
            let below = (at..grid.scrollback_len()).find(|&i| stop(i)).unwrap();
            assert_eq!(up, Scan::Found(base_of(grid) + id(above as u64)));
            assert_eq!(MAX_PROMPT_SCAN_LINES - up_budget, at - above + 1);
            assert_eq!(down, Scan::Found(base_of(grid) + id(below as u64)));
            assert_eq!(MAX_PROMPT_SCAN_LINES - down_budget, below - at + 1);
        }
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
        let marks = |index| grid.line_marks(index).unwrap();
        assert!(has_prompt(grid, 2, marks(2)));
        assert!(prompt_stop(grid, 0, marks(0)));
        assert!(prompt_stop(grid, 1, marks(1)));
        assert!(!prompt_stop(grid, 2, marks(2)), "the continuation of row 1");
    }

    /// #377: a screen clear whose top is line 20 takes the lines of the
    /// records from line 20 down and forgets a prompt there; a record
    /// above it whose end was erased ends at 20 and keeps its duration for
    /// a mark drawn unknown (#378 M3); a record wholly above it, or of
    /// another epoch, is left alone.
    #[test]
    fn a_screen_clear_takes_the_lines_it_erased() {
        use Mark::*;
        let (mut log, _, t0) = log_of(&[
            // Above the clear: ended above it.
            (PromptStart, 2, 0),
            (OutputStart, 3, 10),
            (d(Some(1)), 5, 30),
            // Above the clear, its `D` below its top (erased).
            (PromptStart, 5, 30),
            (OutputStart, 6, 40),
            (d(Some(0)), 24, 100),
            // Below the clear's top.
            (PromptStart, 24, 100),
            (OutputStart, 25, 110),
            (d(Some(0)), 26, 150),
            (PromptStart, 26, 150),
        ]);
        log.records.push_front(Record {
            epoch: 7,
            ..log.records[0]
        });
        log.screen_cleared(20, 0);
        assert!(log.pending.is_none(), "the prompt on line 26 was erased");
        let epochs: Vec<_> = log.records.iter().map(|r| r.epoch).collect();
        assert_eq!(epochs, [7, 0, 0, REDRAWN]);
        let r = &log.records[2];
        assert_eq!((r.end, r.end_erased), (20, true));
        assert_eq!(log.command_at(20, VIEW), None, "line 20 is a new one");
        assert!(!log.records[1].end_erased);
        // The hover pairs the erased `D`'s record with an unknown mark.
        let at = |line, exit| log.hover_duration(line, exit, VIEW);
        assert_eq!(at(5, None), Some(Duration::from_millis(60)));
        assert_eq!(at(5, Some(0)), Some(Duration::from_millis(60)));
        assert_eq!(at(5, Some(1)), None);
        // Not the one whose `D` is still there.
        assert_eq!(at(2, Some(1)), Some(Duration::from_millis(20)));
        assert_eq!(at(2, None), None);
        assert_eq!(at(24, Some(0)), None, "its record was erased");
        // A prompt above the top stays; the next `C` starts from it.
        log.apply(&PromptStart, 21, 0, t0, |_| {});
        log.screen_cleared(30, 0);
        log.apply(&OutputStart, 22, 0, t0, |_| {});
        assert_eq!(log.records.back().map(|r| r.prompt), Some(21));
        // A clear of another epoch changes nothing of this one.
        log.screen_cleared(0, 1);
        assert_eq!(log.records.back().map(|r| r.epoch), Some(0));
        // A record with its first line known only from its `C`.
        let (mut log, ..) = log_of(&[(OutputStart, 25, 0)]);
        log.screen_cleared(20, 0);
        assert_eq!(log.records[0].epoch, REDRAWN);
    }

    /// #377: the clear's top line is one it erased. A record that starts
    /// on it is taken, one whose `D` is on it ends there with its `D`
    /// erased, and a prompt on it is forgotten (of the clear's epoch
    /// only); a record known only from a `C` above the top is cut, not
    /// taken, though its prompt line is unknown.
    #[test]
    fn a_screen_clear_takes_the_records_on_its_top_line() {
        use Mark::*;
        let (mut log, _, t0) = log_of(&[
            // Its `D` on the top line.
            (PromptStart, 10, 0),
            (OutputStart, 11, 10),
            (d(Some(0)), 20, 30),
            // Its prompt on the top line.
            (PromptStart, 20, 30),
            (OutputStart, 21, 40),
            (d(Some(0)), 22, 50),
        ]);
        log.screen_cleared(20, 0);
        let kept: Vec<_> = log
            .records
            .iter()
            .map(|r| (r.epoch, r.end, r.end_erased))
            .collect();
        assert_eq!(kept, [(0, 20, true), (REDRAWN, 22, false)]);
        assert_eq!(
            log.hover_duration(10, None, VIEW),
            Some(Duration::from_millis(20)),
            "its `D` was erased: an unknown mark gets its duration"
        );
        // A prompt on the top line, of another epoch and then of this one.
        log.apply(&PromptStart, 20, 0, t0, |_| {});
        log.screen_cleared(20, 1);
        assert_eq!(log.pending.map(|p| p.prompt), Some(20), "another epoch");
        log.screen_cleared(20, 0);
        assert!(log.pending.is_none(), "the prompt on the top line");
        // No prompt: the first line is the `C`'s, above the top.
        let (mut log, ..) = log_of(&[(OutputStart, 12, 0), (d(Some(0)), 25, 10)]);
        log.screen_cleared(20, 0);
        let r = &log.records[0];
        assert_eq!((r.epoch, r.end, r.end_erased), (0, 20, true));
        assert_eq!(
            log.command_at(12, VIEW).map(|c| c.output_line),
            Some(Some(12))
        );
    }
}
