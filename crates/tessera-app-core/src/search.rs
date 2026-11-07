//! Scrollback search: one query per session, stepped over the session's
//! lines a bounded chunk at a time (ADR 0038, decision D6).
//!
//! [`App::search_step`](crate::App::search_step) copies at most [`SEARCH_LINES_PER_LOCK`] lines
//! (fewer on wide screens: about [`MAX_HAYSTACK_BYTES`] cells) out of the
//! terminal under its lock, releases the
//! lock and runs the regex on the copy, one piece (at most
//! [`MAX_HAYSTACK_BYTES`] of text) at a time. The step's deadline is
//! checked between pieces, so a chunk can be searched over several steps
//! (`Chunk`) and a step overruns its budget by at most one piece. The
//! frontend calls it from its display link while a search is active; when
//! nothing changed it returns after one short look at the terminal.
//!
//! # Lines and order
//!
//! Matches are keyed by absolute line number, the numbering of ADR 0023 and
//! the selection (ADR 0025) shifted by a constant `ORIGIN`: the screen's
//! top row is line `ORIGIN +` [`Grid::scrollback_pushed`], history line
//! `i` is `top - len + i`. (A resize can re-wrap the history into more rows
//! than were ever pushed; the origin keeps the numbers unsigned.)
//! Output does not renumber a line, so a match keeps its key while the
//! history grows; a line evicted at the cap or cleared takes its matches
//! with it, and a resize (which re-wraps the history) starts the search
//! over.
//!
//! The scan runs newest first: the screen, then the history upwards. The
//! screen region is looked at again whenever the terminal changed, but a
//! segment whose rows did not change since the last pass keeps that pass's
//! matches without a search (rows the grid's damage does not cover, or
//! equal to the kept copy); the history below the region is scanned once
//! (lines in the history do not change), new history as it scrolls in. At [`MAX_MATCHES`] the oldest matches
//! give way and the count reads "more than".
//!
//! On the alternate screen only its matches are shown and counted; the
//! primary screen's history matches are kept and come back when the
//! application leaves it.
//!
//! The overlays of the last frame are kept with its window and handed out
//! again while neither the window nor the search changed.
//!
//! # Locks
//!
//! The search lock is taken before the terminal lock and never while it
//! is held. The reader thread never takes it.

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tessera_grid::{DamageRows, Grid, Line};
use tessera_render::{Overlay, OverlayKind};
use tessera_search::{
    Haystack, MAX_HAYSTACK_BYTES, MAX_SEGMENT_ROWS, Match, Piece, Query, QueryError, QueryOptions,
    Rows, Segment, is_segment_start, scan_piece, segment_period, segments,
};
use tessera_term::Terminal;

use crate::ScrollTarget;
use crate::session::Session;

/// Most matches one search keeps (security rule S5). Past it the oldest
/// give way and [`SearchStatus::truncated`] is set: the count reads "more
/// than 100 000".
pub const MAX_MATCHES: usize = 100_000;

/// Most lines one step copies out of the terminal under one hold of its
/// lock (security rule S6). Wider screens copy fewer: about
/// [`MAX_HAYSTACK_BYTES`] cells.
pub const SEARCH_LINES_PER_LOCK: usize = 256;

/// Most bytes [`App::search_matches_text`](crate::App::search_matches_text) returns (S5): it stops before
/// the match that would pass it.
pub const MAX_MATCHES_TEXT_BYTES: usize = 1 << 20;

// A chunk always holds at least one whole segment.
const _: () = assert!(SEARCH_LINES_PER_LOCK >= MAX_SEGMENT_ROWS);

/// Most cells of the screen region a search keeps a copy of between
/// passes ([`Scanned`]), 2.5 MB of cells: a larger region is searched
/// whole on every pass.
const MAX_KEPT_CELLS: usize = 128 << 10;

/// Where a search stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SearchStatus {
    /// A query is set. False after [`App::search_clear`](crate::App::search_clear), an empty query
    /// and a refused one.
    pub active: bool,
    /// Matches found so far, in the lines shown (only the alternate
    /// screen's while it is active).
    pub matches: usize,
    /// The current match's index in document order (0 is the oldest), or
    /// `None` when there are no matches.
    pub current: Option<usize>,
    /// The last step found nothing left to scan. New output makes more
    /// work, so this turns false again; until it is true the count may
    /// still grow.
    pub done: bool,
    /// More than [`MAX_MATCHES`] matched: the oldest are not kept.
    pub truncated: bool,
}

/// The text of every match, as [`App::search_matches_text`](crate::App::search_matches_text) returns it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchesText {
    /// Each match's text, in document order, joined by `"\n"`.
    pub text: String,
    /// The text stopped before a match that would have taken it past
    /// [`MAX_MATCHES_TEXT_BYTES`].
    pub truncated: bool,
}

/// Lock access to a session's terminal for a search: runs `f` with the
/// terminal locked and the session's resize count, read under the lock.
pub(crate) trait TerminalSource {
    /// Runs `f` with the terminal locked.
    fn read_terminal<R>(&self, f: impl FnOnce(&Terminal, u64) -> R) -> R;
}

/// The view a frame draws: what [`SearchState::overlays`] needs, read
/// under the terminal lock with the frame's capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    /// Absolute line of the view's top row.
    top: u64,
    rows: u16,
    layout: Layout,
    alt: bool,
    /// Absolute line of the oldest line left.
    base: u64,
}

impl Window {
    /// The window of a view `offset` lines above the live screen of
    /// `grid`, after `resizes` resizes.
    pub(crate) fn of(grid: &Grid, offset: usize, resizes: u64) -> Self {
        let pushed = screen_top(grid);
        let len = grid.scrollback_len() as u64;
        let alt = grid.is_alt_screen();
        let back = if alt { 0 } else { (offset as u64).min(len) };
        Self {
            top: pushed - back,
            rows: grid.rows(),
            layout: Layout::of(grid, resizes),
            alt,
            base: pushed - len,
        }
    }
}

/// The grid's size and resize count: matches hold only for the layout
/// they were found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    rows: u16,
    cols: u16,
    resizes: u64,
}

impl Layout {
    fn of(grid: &Grid, resizes: u64) -> Self {
        Self {
            rows: grid.rows(),
            cols: grid.cols(),
            resizes,
        }
    }
}

/// What the screen looked like: a change of any part makes the screen
/// region worth scanning again. Printing on the cursor's row does not
/// move the damage generation (the grid marks that row dirty instead), so
/// a copy of that row is compared too ([`same_row`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    pushed: u64,
    base: u64,
    clears: u64,
    damage: u64,
    alt: bool,
    cursor_row: u16,
}

impl Seen {
    fn of(grid: &Grid) -> Self {
        let pushed = screen_top(grid);
        Self {
            pushed,
            base: pushed - grid.scrollback_len() as u64,
            clears: grid.history_clear_generation(),
            damage: grid.damage_generation(),
            alt: grid.is_alt_screen(),
            cursor_row: grid.cursor().row,
        }
    }

    /// The first line a logical line can start at: the alternate screen's
    /// top row continues nothing above it.
    fn first(&self) -> u64 {
        if self.alt { self.pushed } else { self.base }
    }
}

/// Added to every absolute line number, so that the oldest history line
/// is never below zero: a resize can re-wrap the history into more rows
/// than [`Grid::scrollback_pushed`] has counted.
const ORIGIN: u64 = 1 << 62;

/// The absolute line number of `grid`'s top screen row.
fn screen_top(grid: &Grid) -> u64 {
    ORIGIN + grid.scrollback_pushed()
}

/// A match's identity: its first cell.
type Key = (u64, u16);

fn key(m: &Match) -> Key {
    (m.start_line, m.start_col)
}

/// One rescan of the screen region, newest chunk first, possibly over
/// several steps.
#[derive(Debug)]
struct Pass {
    /// The screen as the pass started.
    seen: Seen,
    /// First line of the region (the start of the segment holding the
    /// screen's top row).
    start: u64,
    /// The rows still to scan end here (exclusive).
    end: u64,
    /// The cursor's row as the pass started.
    row: Line,
    /// Matches so far, newest segment first (sorted on completion).
    found: Vec<Match>,
    truncated: bool,
    /// The rows scanned so far and their cuts, for the next pass to
    /// reuse; `keep` is false once that is off (too many cells, or rows
    /// that did not join up).
    next: Scanned,
    keep: bool,
}

/// One segment by absolute line, with the flags its text depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cut {
    start: u64,
    end: u64,
    above: bool,
    below: bool,
}

impl Cut {
    fn of(first: u64, segment: Segment) -> Self {
        Self {
            start: first + segment.start as u64,
            end: first + segment.end as u64,
            above: segment.above,
            below: segment.below,
        }
    }
}

/// The rows the last finished screen pass scanned, as copied, and how it
/// cut them: a segment with the same cut and rows equal to these has the
/// matches that pass found (in [`Active::screen`]), so it is not searched
/// again. Output that scrolls the screen leaves most rows as they were.
#[derive(Debug, Default)]
struct Scanned {
    /// The screen as the pass that copied the rows started; `None` while
    /// that pass is under way.
    seen: Option<Seen>,
    /// Absolute line of `rows[0]`.
    first: u64,
    /// The copies; `None` once a chunk took one ([`copy_lines`]).
    rows: VecDeque<Option<Line>>,
    /// The cuts, newest first.
    cuts: Vec<Cut>,
}

impl Scanned {
    /// Empties it, handing its rows to `spare` for reuse.
    fn clear(&mut self, spare: &mut Vec<Line>) {
        spare.extend(self.rows.drain(..).flatten());
        self.cuts.clear();
        self.seen = None;
    }

    /// What `grid` says about the copies without looking at them: the
    /// screen's top line and the screen rows that may have changed since
    /// the copies were taken. `None` when the grid cannot say (the screen
    /// scrolled, history was cleared, the other screen is showing): every
    /// copy is then compared. Lines above that top are history, which
    /// does not change (the one change, ED 2 or RIS ending the newest
    /// history line, drops the copies: [`Active::plan`]).
    fn untouched(&self, grid: &Grid) -> Option<(u64, DamageRows)> {
        let seen = self.seen?;
        let now = Seen::of(grid);
        (now.pushed == seen.pushed && now.clears == seen.clears && now.alt == seen.alt)
            .then(|| (now.pushed, grid.damage_since(seen.damage)))
    }

    /// True when the last pass cut `cut` (its rows are then known to be
    /// unchanged only from [`copy_lines`]).
    fn has(&self, cut: Cut) -> bool {
        let i = self.cuts.partition_point(|c| c.start > cut.start);
        self.cuts.get(i) == Some(&cut)
    }

    /// The copy of `line` when it still has the text of `row`, taken out:
    /// without a compare when `untouched` ([`Scanned::untouched`]) says the
    /// row has not changed since it was copied.
    fn take_same(
        &mut self,
        line: u64,
        row: &Line,
        untouched: Option<&(u64, DamageRows)>,
    ) -> Option<Line> {
        let index = usize::try_from(line.checked_sub(self.first)?).ok()?;
        let slot = self.rows.get_mut(index)?;
        let trusted = untouched.is_some_and(|(top, damage)| {
            line.checked_sub(*top)
                .is_none_or(|row| u16::try_from(row).is_ok_and(|row| !damage.contains(row)))
        });
        if slot
            .as_ref()
            .is_some_and(|kept| trusted || same_row(row, kept))
        {
            slot.take()
        } else {
            None
        }
    }
}

/// The work one chunk does.
#[derive(Debug, Clone, Copy)]
enum Work {
    /// New history below the scanned part: lines `start..end`.
    CatchUp { start: u64, end: u64 },
    /// The screen region's next chunk.
    Screen {
        start: u64,
        end: u64,
        more_below: bool,
    },
    /// History above the scanned part.
    Older { start: u64, end: u64 },
    /// The oldest history segment, `start..end`, again: eviction cut its
    /// line, so what is left reads as a line of its own.
    Head { start: u64, end: u64 },
}

/// A query being searched for.
#[derive(Debug)]
struct Active {
    pattern: String,
    query: Query,
    /// The layout the matches were found in; `None` before the first step.
    layout: Option<Layout>,
    /// The scanned history: lines `low..high`.
    low: u64,
    high: u64,
    /// Matches in `low..high`, in document order.
    history: VecDeque<Match>,
    /// Matches in the screen region from `high` down, in document order,
    /// found on the screen `screen_seen` describes.
    screen: Vec<Match>,
    screen_seen: Option<Seen>,
    /// The cursor's row on the screen `screen_seen` describes.
    screen_row: Line,
    pass: Option<Pass>,
    /// The history scan stopped at [`MAX_MATCHES`], or matches gave way.
    truncated: bool,
    /// Eviction reached into the scanned history: its oldest segment is
    /// to be searched again ([`Work::Head`]).
    head: bool,
    /// The newest history line when the last plan saw it wrap: ED 2 and
    /// RIS can end it later (#86), the one change history lines take.
    wrapping: Option<u64>,
    current: Option<Key>,
    done: bool,
    /// Lines copied out of the terminal, reused from chunk to chunk.
    pool: Vec<Line>,
    haystack: Haystack,
    /// What the last finished pass scanned, matching `screen`.
    scanned: Scanned,
    /// Line allocations to reuse for the next pass's copy.
    spare: Vec<Line>,
    /// Segments of the chunk being searched.
    cuts: Vec<Segment>,
    /// For each pool line, true when it is the last pass's copy, unchanged.
    unchanged: Vec<bool>,
    /// The chunk copied and not searched to its end yet: a step's deadline
    /// can pass between its pieces.
    chunk: Option<Chunk>,
}

impl Active {
    fn new(pattern: &str, query: Query) -> Self {
        Self {
            pattern: pattern.to_owned(),
            query,
            layout: None,
            low: 0,
            high: 0,
            history: VecDeque::new(),
            screen: Vec::new(),
            screen_seen: None,
            screen_row: Line::default(),
            pass: None,
            truncated: false,
            head: false,
            wrapping: None,
            current: None,
            done: false,
            pool: Vec::new(),
            haystack: Haystack::new(),
            scanned: Scanned::default(),
            spare: Vec::new(),
            cuts: Vec::new(),
            unchanged: Vec::new(),
            chunk: None,
        }
    }

    /// Whether the screen matches are the ones to show on a screen that
    /// `alt` says is the alternate one.
    fn screen_shown(&self, alt: bool) -> bool {
        self.screen_seen.is_some_and(|seen| seen.alt == alt)
    }

    fn alt(&self) -> bool {
        self.screen_seen
            .or(self.pass.as_ref().map(|pass| pass.seen))
            .is_some_and(|seen| seen.alt)
    }

    /// The matches shown and counted, in document order: the history's
    /// (unless the alternate screen is active) then the screen's.
    fn shown(&self) -> (&[Match], &[Match], &[Match]) {
        let alt = self.alt();
        let (a, b) = if alt {
            (&[][..], &[][..])
        } else {
            self.history.as_slices()
        };
        let screen = if self.screen_shown(alt) {
            &self.screen[..]
        } else {
            &[][..]
        };
        (a, b, screen)
    }

    fn shown_len(&self) -> usize {
        let (a, b, c) = self.shown();
        a.len() + b.len() + c.len()
    }

    fn shown_at(&self, mut index: usize) -> Option<Match> {
        let (a, b, c) = self.shown();
        for part in [a, b, c] {
            if let Some(m) = part.get(index) {
                return Some(*m);
            }
            index -= part.len();
        }
        None
    }

    /// The index of the first shown match at or after `key`.
    fn shown_index(&self, key: Key) -> (usize, bool) {
        let (a, b, c) = self.shown();
        let mut offset = 0;
        for part in [a, b, c] {
            let i = part.partition_point(|m| self::key(m) < key);
            if i < part.len() {
                return (offset + i, self::key(&part[i]) == key);
            }
            offset += part.len();
        }
        (offset, false)
    }

    /// Re-points the current match at a shown one: kept while it is
    /// shown, else the next shown one in document order, else the last;
    /// the newest when there was none.
    fn fix_current(&mut self) {
        let len = self.shown_len();
        if len == 0 {
            self.current = None;
            return;
        }
        let index = match self.current {
            None => len - 1,
            Some(key) => {
                let (index, _) = self.shown_index(key);
                index.min(len - 1)
            }
        };
        self.current = self.shown_at(index).map(|m| key(&m));
    }

    fn status(&self) -> SearchStatus {
        let current = self.current.and_then(|key| {
            let (index, exact) = self.shown_index(key);
            exact.then_some(index)
        });
        SearchStatus {
            active: true,
            matches: self.shown_len(),
            current,
            done: self.done,
            truncated: self.truncated || self.pass.as_ref().is_some_and(|pass| pass.truncated),
        }
    }

    /// Drops everything found: the layout changed.
    fn restart(&mut self, layout: Layout, seen: &Seen, grid: &Grid) {
        self.layout = Some(layout);
        self.history.clear();
        self.screen.clear();
        self.screen_seen = None;
        self.drop_pass();
        self.scanned.clear(&mut self.spare);
        self.truncated = false;
        self.head = false;
        self.current = None;
        // On the alternate screen too: the primary's history is scanned
        // for when it comes back.
        let start = Lines::new(grid, seen, seen.base).segment_start(seen.pushed);
        self.low = start;
        self.high = start;
    }

    /// Brings the bookkeeping up to date with the terminal, then picks the
    /// next chunk of work, if any.
    fn plan(&mut self, grid: &Grid, resizes: u64) -> Option<Work> {
        let layout = Layout::of(grid, resizes);
        let seen = Seen::of(grid);
        if self.layout != Some(layout) {
            self.restart(layout, &seen, grid);
        }
        // An alternate-screen switch: the screen matches are of the other
        // screen.
        if self.alt() != seen.alt {
            self.screen.clear();
            self.screen_seen = None;
            self.drop_pass();
            self.scanned.clear(&mut self.spare);
            self.current = None;
        }
        // Lines evicted at the cap or cleared take their matches along.
        // When that cuts into the scanned history, the rest of its oldest
        // line is a line of its own now (as a fresh search reads it).
        let base = seen.base;
        if base > self.low {
            self.head = base < self.high;
        }
        while self.history.front().is_some_and(|m| m.start_line < base) {
            self.history.pop_front();
        }
        self.screen.retain(|m| m.start_line >= base);
        self.low = self.low.max(base);
        self.high = self.high.max(base);
        let rows = chunk_rows(layout.cols);
        let history = Lines::new(grid, &seen, base);
        self.see_ended_line(&history, seen.pushed, base);

        if !seen.alt {
            let target = history.segment_start(seen.pushed).max(self.high);
            if target > self.high {
                let end = if target - self.high <= rows as u64 {
                    target
                } else {
                    history.segment_start(self.high + rows as u64)
                };
                return Some(Work::CatchUp {
                    start: self.high,
                    end,
                });
            }
        }

        let cursor_row = grid.line(seen.cursor_row);
        let unchanged = self.screen_seen == Some(seen)
            && cursor_row.is_some_and(|row| same_row(row, &self.screen_row));
        if self.pass.is_none() && !unchanged {
            let start = if seen.alt {
                seen.pushed
            } else {
                history.segment_start(seen.pushed).max(self.high)
            };
            let mut row = std::mem::take(&mut self.screen_row);
            if let Some(cursor_row) = cursor_row {
                copy_line(&mut row, cursor_row);
            }
            let end = seen.pushed + u64::from(layout.rows);
            let cells = usize::try_from(end - start)
                .unwrap_or(usize::MAX)
                .saturating_mul(usize::from(layout.cols));
            self.pass = Some(Pass {
                seen,
                start,
                end,
                row,
                found: Vec::new(),
                truncated: false,
                next: Scanned::default(),
                keep: cells <= MAX_KEPT_CELLS,
            });
        }
        if let Some(pass) = &mut self.pass {
            let floor = pass.start.max(seen.first());
            let live_end = seen.pushed + u64::from(layout.rows);
            pass.end = pass.end.min(live_end);
            if pass.end > floor && !pass.truncated {
                let lines = Lines::new(grid, &seen, seen.first());
                return Some(Work::Screen {
                    start: lines.chunk_above(floor, pass.end, rows),
                    end: pass.end,
                    more_below: pass.end < live_end,
                });
            }
            self.finish_pass();
        }

        if self.head && !self.truncated {
            return Some(Work::Head {
                start: base,
                end: history.segment_end(base, self.high),
            });
        }
        if !self.truncated && self.low > base {
            return Some(Work::Older {
                start: history.chunk_above(base, self.low, rows),
                end: self.low,
            });
        }
        None
    }

    /// Installs the finished pass's matches as the screen's.
    fn finish_pass(&mut self) {
        let Some(mut pass) = self.pass.take() else {
            return;
        };
        // Segments arrive newest first, each in order: a stable sort
        // merges those runs.
        pass.found.sort();
        let high = self.high;
        pass.found.retain(|m| m.start_line >= high);
        self.screen = pass.found;
        self.screen_seen = Some(pass.seen);
        self.screen_row = pass.row;
        self.scanned.clear(&mut self.spare);
        if pass.truncated {
            self.truncated = true;
            pass.next.clear(&mut self.spare);
        } else if pass.keep {
            self.scanned = pass.next;
            self.scanned.seen = Some(pass.seen);
        } else {
            pass.next.clear(&mut self.spare);
        }
        // Spare lines are never more than one kept copy's worth.
        let cols = usize::from(self.layout.map_or(1, |layout| layout.cols).max(1));
        self.spare.truncate(MAX_KEPT_CELLS / cols);
        self.trim();
    }

    /// Sees the newest history line end after it wrapped: ED 2 and RIS end
    /// it when they erase the top row that continued it (#86), so its
    /// segment's text changes. The copies kept of it are dropped, and when
    /// it was searched as history its segment is searched again as new
    /// history. Then notes whether the newest history line, below `pushed`,
    /// wraps now.
    fn see_ended_line(&mut self, history: &Lines<'_>, pushed: u64, base: u64) {
        if let Some(line) = self
            .wrapping
            .filter(|&line| line >= base && !history.wraps(line))
        {
            self.drop_pass();
            self.scanned.clear(&mut self.spare);
            let start = history.segment_start(line);
            if self.low <= start && start < self.high {
                self.high = start;
                while self.history.back().is_some_and(|m| m.start_line >= start) {
                    self.history.pop_back();
                }
            }
        }
        self.wrapping = Some(pushed - 1).filter(|&line| line >= base && history.wraps(line));
    }

    /// Abandons the pass under way, keeping its row allocations.
    fn drop_pass(&mut self) {
        if let Some(mut pass) = self.pass.take() {
            pass.next.clear(&mut self.spare);
        }
    }

    /// Lets the oldest history matches give way past [`MAX_MATCHES`].
    fn trim(&mut self) {
        while self.history.len() + self.screen.len() > MAX_MATCHES {
            if self.history.pop_front().is_none() {
                self.screen.truncate(MAX_MATCHES);
            }
            self.truncated = true;
        }
    }

    /// Takes up the copied rows of `work` (the first `count` of the pool)
    /// as the chunk to search ([`Active::search_piece`]).
    fn begin(&mut self, work: Work, count: usize, run_above: usize, period: usize) {
        let (first, more_below) = match work {
            Work::CatchUp { start, .. } | Work::Older { start, .. } | Work::Head { start, .. } => {
                (start, true)
            }
            Work::Screen {
                start, more_below, ..
            } => (start, more_below),
        };
        let at = Rows {
            first,
            run_above,
            more_below,
        };
        segments(&self.pool[..count], at, period, &mut self.cuts);
        // The screen and older history keep the newest matches that fit;
        // the room does not change while the chunk is searched.
        let sink = match work {
            Work::CatchUp { .. } | Work::Head { .. } => Sink::All(Vec::new()),
            Work::Screen { .. } => Sink::Newest(Newest::new(
                MAX_MATCHES.saturating_sub(self.pass.as_ref().map_or(0, |pass| pass.found.len())),
            )),
            Work::Older { .. } => Sink::Newest(Newest::new(
                MAX_MATCHES.saturating_sub(self.history.len() + self.screen.len()),
            )),
        };
        self.chunk = Some(Chunk {
            work,
            count,
            at,
            left: self.cuts.len(),
            piece: None,
            stopped: false,
            sink,
        });
    }

    /// Searches the chunk on, segment by segment from the bottom one up,
    /// until one piece ([`scan_piece`]) has run or the chunk is done; a
    /// segment the last pass scanned with the same rows takes that pass's
    /// matches without a search ([`Scanned`]). A done chunk is filed.
    /// Returns whether a piece ran.
    fn search_piece(&mut self) -> bool {
        let Some(chunk) = &mut self.chunk else {
            return false;
        };
        let rows = &self.pool[..chunk.count];
        let mut ran = false;
        while chunk.left > 0 && !chunk.stopped && !ran {
            let segment = self.cuts[chunk.left - 1];
            let piece = if let Some(piece) = chunk.piece {
                piece
            } else {
                let key = Cut::of(chunk.at.first, segment);
                if let (Work::Screen { .. }, Some(pass)) = (chunk.work, &mut self.pass) {
                    pass.next.cuts.push(key);
                }
                let unchanged = self.unchanged[segment.start..segment.end]
                    .iter()
                    .all(|&same| same);
                if unchanged && self.scanned.has(key) {
                    let screen = &self.screen;
                    let from = screen.partition_point(|m| m.start_line < key.start);
                    for m in screen[from..].iter().take_while(|m| m.start_line < key.end) {
                        if chunk.sink.push(*m).is_break() {
                            chunk.stopped = true;
                            break;
                        }
                    }
                    chunk.left -= 1;
                    continue;
                }
                Piece::first(segment)
            };
            let sink = &mut chunk.sink;
            let (flow, next) = scan_piece(
                &self.query,
                rows,
                chunk.at.first,
                segment,
                piece,
                &mut self.haystack,
                |m| sink.push(m),
            );
            ran = true;
            chunk.piece = next;
            if next.is_none() {
                chunk.left -= 1;
            }
            chunk.stopped |= flow.is_break();
        }
        let done = chunk.left == 0 || chunk.stopped;
        if done && let Some(chunk) = self.chunk.take() {
            self.file(chunk);
        }
        ran
    }

    /// Files the matches of a searched chunk.
    fn file(&mut self, chunk: Chunk) {
        let count = chunk.count;
        match (chunk.work, chunk.sink) {
            (Work::CatchUp { end, .. }, Sink::All(mut found)) => {
                found.sort_unstable();
                self.history.extend(found);
                self.high = end;
                let high = self.high;
                self.screen.retain(|m| m.start_line >= high);
                self.trim();
            }
            (Work::Screen { start, .. }, Sink::Newest(newest)) => {
                let Some(pass) = &mut self.pass else { return };
                let next = &mut pass.next;
                pass.truncated |= newest.into(&mut pass.found);
                if pass.keep && (next.rows.is_empty() || next.first == start + count as u64) {
                    // Keep the copies (the pool takes spare allocations).
                    for line in self.pool[..count].iter_mut().rev() {
                        let spare = self.spare.pop().unwrap_or_default();
                        next.rows.push_front(Some(std::mem::replace(line, spare)));
                    }
                    next.first = start;
                } else {
                    pass.keep = false;
                }
                pass.end = start;
                if pass.end <= pass.start || pass.truncated {
                    // Done: show its matches now, not after the next look.
                    self.finish_pass();
                }
            }
            (Work::Older { start, .. }, Sink::Newest(newest)) => {
                let mut found = Vec::new();
                if newest.into(&mut found) {
                    self.truncated = true;
                }
                found.sort_unstable();
                for m in found.into_iter().rev() {
                    self.history.push_front(m);
                }
                self.low = start;
            }
            (Work::Head { end, .. }, Sink::All(mut found)) => {
                found.sort_unstable();
                while self.history.front().is_some_and(|m| m.start_line < end) {
                    self.history.pop_front();
                }
                for m in found.into_iter().rev() {
                    self.history.push_front(m);
                }
                self.head = false;
                self.trim();
            }
            // `begin` pairs each kind of work with its sink.
            _ => {}
        }
    }
}

/// A chunk copied out of the terminal and searched over one or more
/// steps, one piece at a time, so a step's deadline is checked between
/// pieces (ADR 0038). Nothing it found is filed before its last piece.
#[derive(Debug)]
struct Chunk {
    work: Work,
    /// Lines of the pool it holds.
    count: usize,
    at: Rows,
    /// Segments (`Active::cuts`) still to search: the first `left`; the
    /// bottom one goes next.
    left: usize,
    /// Where the next segment stands: `None` before its first piece.
    piece: Option<Piece>,
    /// The sink was full: no older segment can add a match.
    stopped: bool,
    sink: Sink,
}

/// Where a chunk's matches go until it is filed.
#[derive(Debug)]
enum Sink {
    /// Every match (new history, the history head).
    All(Vec<Match>),
    /// The newest that fit (the screen, older history).
    Newest(Newest),
}

impl Sink {
    fn push(&mut self, m: Match) -> ControlFlow<()> {
        match self {
            Self::All(found) => {
                found.push(m);
                ControlFlow::Continue(())
            }
            Self::Newest(newest) => newest.push(m),
        }
    }
}

/// Collects the newest `room` matches of a scan that runs segment by
/// segment from the bottom up (in document order within a segment), and
/// stops it once a segment fills the room: no older segment can add one.
#[derive(Debug)]
struct Newest {
    room: usize,
    /// Matches of finished segments, newest segment first.
    kept: Vec<Match>,
    /// The current segment's matches: its newest `room` once it has more
    /// (a ring from `next`).
    segment: Vec<Match>,
    next: usize,
    full: bool,
}

impl Newest {
    fn new(room: usize) -> Self {
        Self {
            room,
            kept: Vec::new(),
            segment: Vec::new(),
            next: 0,
            full: false,
        }
    }

    fn push(&mut self, m: Match) -> ControlFlow<()> {
        if self.segment.last().is_some_and(|last| m < *last) {
            // A new (older) segment: the last one is finished.
            if self.finish_segment() {
                self.full = true;
                return ControlFlow::Break(());
            }
        }
        if self.room == 0 {
            self.full = true;
            return ControlFlow::Break(());
        }
        if self.segment.len() < self.room {
            self.segment.push(m);
        } else {
            // Keep the segment's newest `room`: overwrite the oldest.
            self.full = true;
            self.segment[self.next] = m;
            self.next = (self.next + 1) % self.room;
        }
        ControlFlow::Continue(())
    }

    /// Moves the current segment into `kept`; true when that used up the
    /// room.
    fn finish_segment(&mut self) -> bool {
        self.segment.rotate_left(self.next);
        self.next = 0;
        self.room -= self.segment.len();
        self.kept.append(&mut self.segment);
        self.room == 0
    }

    /// Appends what was kept to `out`; true when matches were left out.
    fn into(mut self, out: &mut Vec<Match>) -> bool {
        self.finish_segment();
        out.append(&mut self.kept);
        self.full
    }
}

/// Lines one chunk copies at `cols` columns: about [`MAX_HAYSTACK_BYTES`]
/// cells, at most [`SEARCH_LINES_PER_LOCK`], and always more than the
/// longest segment.
fn chunk_rows(cols: u16) -> usize {
    (MAX_HAYSTACK_BYTES / usize::from(cols.max(1))).clamp(1, SEARCH_LINES_PER_LOCK)
}

/// The grid's lines by absolute number, from `first` (the first line a
/// logical line can start at) down, cut into segments as
/// [`is_segment_start`] cuts them. Every walk is bounded by two periods.
struct Lines<'a> {
    grid: &'a Grid,
    base: u64,
    first: u64,
    period: usize,
}

impl<'a> Lines<'a> {
    fn new(grid: &'a Grid, seen: &Seen, first: u64) -> Self {
        Self {
            grid,
            base: seen.base,
            first,
            period: segment_period(grid.cols()),
        }
    }

    fn wraps(&self, line: u64) -> bool {
        line_at(self.grid, self.base, line).is_some_and(|l| l.wrapped)
    }

    /// The rows directly above `line` that wrap into it, counted up to
    /// `limit`; the count stops at `first`.
    fn run_above(&self, line: u64, limit: usize) -> usize {
        let mut run = 0;
        while run < limit && line > self.first + run as u64 && self.wraps(line - 1 - run as u64) {
            run += 1;
        }
        run
    }

    /// The first line of the segment holding `line`.
    fn segment_start(&self, line: u64) -> u64 {
        let period = self.period as u64;
        let run = self.run_above(line, 2 * self.period) as u64;
        let cut = line - line % period;
        if run == 2 * period {
            // The line began two periods or more above: `cut` is a cut.
            return cut;
        }
        let start = line - run;
        if cut >= start + period { cut } else { start }
    }

    /// The line after the segment that starts at `start`, at most `limit`.
    fn segment_end(&self, start: u64, limit: u64) -> u64 {
        let mut run = self.run_above(start, self.period);
        let mut line = start;
        while line < limit {
            run = if self.wraps(line) {
                (run + 1).min(self.period)
            } else {
                0
            };
            line += 1;
            if is_segment_start(line, run, self.period) {
                break;
            }
        }
        line
    }

    /// The first line of a chunk of whole segments that ends at `end` (a
    /// segment start, or the end of the lines) and holds at most `rows`
    /// lines, none above `floor` (a segment start).
    fn chunk_above(&self, floor: u64, end: u64, rows: usize) -> u64 {
        let top = end.saturating_sub(rows as u64).max(floor);
        let mut start = self.segment_start(top).max(floor);
        if start < top {
            // That segment reaches above the chunk: start below it.
            start = self.segment_end(start, end);
        }
        if start >= end {
            start = self.segment_start(end - 1).max(floor);
        }
        start
    }
}

fn line_at(grid: &Grid, base: u64, line: u64) -> Option<&Line> {
    let index = usize::try_from(line.checked_sub(base)?).ok()?;
    grid.logical_line(index)
}

/// Copies what a search reads of `source` (cells, clusters, wrap flag)
/// into `copy`, reusing its allocations.
fn copy_line(copy: &mut Line, source: &Line) {
    copy.cells.clone_from(&source.cells);
    copy.graphemes.clone_from(&source.graphemes);
    copy.wrapped = source.wrapped;
}

/// True when `copy` ([`copy_line`]) still has the text of `row`: the
/// same characters, cell flags, clusters and wrap. Styles are not text
/// (and comparing them per cell would cost more than the search).
fn same_row(row: &Line, copy: &Line) -> bool {
    // A fold without an early exit: the rows are short and usually equal.
    let differ = |(a, b): (&tessera_grid::Cell, &tessera_grid::Cell)| {
        (u32::from(a.ch) ^ u32::from(b.ch)) | u32::from(a.flags.bits() ^ b.flags.bits())
    };
    row.wrapped == copy.wrapped
        && row.cells.len() == copy.cells.len()
        && row
            .cells
            .iter()
            .zip(&copy.cells)
            .map(differ)
            .fold(0, |acc, bits| acc | bits)
            == 0
        && row.graphemes == copy.graphemes
}

/// Copies lines `start..end` of `grid` into `pool`, reusing its
/// allocations; returns how many. A line `scanned` holds unchanged
/// ([`same_row`]) is moved from there instead of copied, and `unchanged`
/// says which ones were; `scanned` is `None` when its matches cannot stand
/// in (some gave way).
fn copy_lines(
    pool: &mut Vec<Line>,
    unchanged: &mut Vec<bool>,
    mut scanned: Option<(&mut Scanned, &mut Vec<Line>)>,
    grid: &Grid,
    base: u64,
    start: u64,
    end: u64,
) -> usize {
    unchanged.clear();
    let untouched = scanned
        .as_ref()
        .and_then(|(scanned, _)| scanned.untouched(grid));
    let mut count = 0;
    for line in start..end {
        let Some(source) = line_at(grid, base, line) else {
            break;
        };
        if pool.len() == count {
            pool.push(Line::default());
        }
        let kept = scanned
            .as_mut()
            .and_then(|(scanned, _)| scanned.take_same(line, source, untouched.as_ref()));
        unchanged.push(kept.is_some());
        match kept {
            Some(kept) => {
                let old = std::mem::replace(&mut pool[count], kept);
                if let Some((_, spare)) = scanned.as_mut() {
                    spare.push(old);
                }
            }
            None => copy_line(&mut pool[count], source),
        }
        count += 1;
    }
    count
}

/// What [`SearchState::overlays`] last computed.
#[derive(Debug, Default)]
struct Drawn {
    /// The window they are for; `None` once the search changed.
    window: Option<Window>,
    overlays: Vec<Overlay>,
}

/// A session's search: none, or one query with what it found.
#[derive(Debug, Default)]
pub(crate) struct SearchState {
    active: Option<Active>,
    /// The overlays last handed out and the window they are for, reused
    /// while neither changes (an idle frame); every change to the search
    /// drops them. At most one window's worth.
    drawn: Drawn,
    /// How long each hold of the terminal lock lasted and how many cells
    /// it copied, for the lock-hold test.
    holds: Holds,
    /// How long each piece's regex run took, for the step-deadline test.
    #[cfg(test)]
    pieces: Vec<Duration>,
}

/// The lock-hold record of [`SearchState`]: kept by test builds only.
#[cfg(test)]
type Holds = Vec<(Duration, usize)>;
#[cfg(not(test))]
type Holds = ();

impl SearchState {
    /// Starts searching for `pattern`. The same pattern and options as the
    /// current search keep what it found; an empty pattern clears the
    /// search; a refused one clears it and returns why.
    pub(crate) fn start(&mut self, pattern: &str, options: QueryOptions) -> Result<(), QueryError> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.pattern == pattern && active.query.options() == options)
        {
            return Ok(());
        }
        self.active = None;
        self.drawn.window = None;
        if let Some(query) = Query::new(pattern, options)? {
            self.active = Some(Active::new(pattern, query));
        }
        Ok(())
    }

    /// Ends the search.
    pub(crate) fn clear(&mut self) {
        self.active = None;
        self.drawn.window = None;
    }

    /// Where the search stands.
    pub(crate) fn status(&self) -> SearchStatus {
        self.active.as_ref().map(Active::status).unwrap_or_default()
    }

    /// Searches until nothing is left or `budget` has passed, checking the
    /// deadline between pieces ([`scan_piece`]): at least one piece,
    /// at most one when `budget` is zero, so a step overruns its budget by
    /// at most one piece's regex run, whose time depends on the pattern. A chunk of lines is copied under one
    /// hold of the terminal lock and may be searched over several steps;
    /// its matches are filed when its last piece has run.
    pub(crate) fn step(&mut self, source: &impl TerminalSource, budget: Duration) -> SearchStatus {
        self.drawn.window = None;
        let started = Instant::now();
        while self.unit(source) && started.elapsed() < budget {}
        self.status()
    }

    /// Searches one piece of the chunk under way, copying the next chunk
    /// out of the terminal first when none is; false when there was no work
    /// left.
    fn unit(&mut self, source: &impl TerminalSource) -> bool {
        let Some(active) = &mut self.active else {
            return false;
        };
        if active.chunk.is_none() && !copy_chunk(active, source, &mut self.holds) {
            active.done = true;
            active.fix_current();
            return false;
        }
        #[cfg(test)]
        let started = Instant::now();
        let ran = active.search_piece();
        #[cfg(test)]
        if ran {
            self.pieces.push(started.elapsed());
        }
        #[cfg(not(test))]
        let _ = ran;
        active.done = false;
        active.fix_current();
        true
    }

    /// Moves the current match one toward older output (`older`) or newer,
    /// wrapping around at either end; returns it, if there is one.
    pub(crate) fn advance(&mut self, older: bool) -> Option<Match> {
        self.drawn.window = None;
        let active = self.active.as_mut()?;
        let len = active.shown_len();
        if len == 0 {
            return None;
        }
        let index = match active.current {
            None => len - 1,
            Some(key) => {
                let (index, exact) = active.shown_index(key);
                match (older, exact) {
                    (true, _) => index.checked_sub(1).unwrap_or(len - 1),
                    (false, true) => (index + 1) % len,
                    (false, false) => index % len,
                }
            }
        };
        let m = active.shown_at(index)?;
        active.current = Some(key(&m));
        Some(m)
    }

    /// Appends the overlays of the matches in `window` to `overlays`:
    /// [`OverlayKind::CurrentMatch`] for the current match,
    /// [`OverlayKind::Match`] for the others, one per row a match covers.
    /// Matches found in another layout, on the other screen or on lines
    /// gone since draw nothing.
    pub(crate) fn overlays(&mut self, window: &Window, overlays: &mut Vec<Overlay>) {
        if self.drawn.window == Some(*window) {
            overlays.extend_from_slice(&self.drawn.overlays);
            return;
        }
        let from = overlays.len();
        self.draw(window, overlays);
        self.drawn.overlays.clear();
        self.drawn.overlays.extend_from_slice(&overlays[from..]);
        self.drawn.window = Some(*window);
    }

    /// [`SearchState::overlays`] without the reuse.
    fn draw(&self, window: &Window, overlays: &mut Vec<Overlay>) {
        let Some(active) = &self.active else {
            return;
        };
        if active.layout != Some(window.layout) {
            return;
        }
        let top = window.top;
        let bottom = top + u64::from(window.rows);
        let cols = window.layout.cols;
        let current = active.current;
        let mut draw = |part: &[Match]| {
            // The matches that reach into the window: sorted by start, and
            // so by end too (matches do not overlap).
            let first = part.partition_point(|m| m.end_line < top);
            let part = &part[first..];
            let part = &part[..part.partition_point(|m| m.start_line < bottom)];
            overlays.reserve(part.len());
            for m in part {
                if m.start_line < window.base {
                    continue;
                }
                let kind = if current == Some(key(m)) {
                    OverlayKind::CurrentMatch
                } else {
                    OverlayKind::Match
                };
                // Rows of the window are below 2^16 lines apart.
                let row = |line: u64| u16::try_from(line - top).unwrap_or(u16::MAX);
                if m.start_line == m.end_line {
                    overlays.push(Overlay::new(
                        kind,
                        row(m.start_line),
                        m.start_col..m.end_col,
                    ));
                    continue;
                }
                let last = m.end_line.min(bottom - 1);
                let mut line = m.start_line.max(top);
                while line <= last {
                    let start = if line == m.start_line { m.start_col } else { 0 };
                    let end = if line == m.end_line { m.end_col } else { cols };
                    overlays.push(Overlay::new(kind, row(line), start..end));
                    line += 1;
                }
            }
        };
        if !window.alt {
            let (a, b) = active.history.as_slices();
            draw(a);
            draw(b);
        }
        if active.screen_shown(window.alt) {
            draw(&active.screen);
        }
    }

    /// The text of every shown match, in document order, joined by `"\n"`
    /// (no newline after the last): a match across a soft wrap is one line
    /// of text, a wide character appears once. Reads at most
    /// [`SEARCH_LINES_PER_LOCK`] lines per hold of the terminal lock and
    /// stops before the match that would pass
    /// [`MAX_MATCHES_TEXT_BYTES`].
    pub(crate) fn matches_text(&self, source: &impl TerminalSource) -> MatchesText {
        let mut out = MatchesText::default();
        let Some(active) = &self.active else {
            return out;
        };
        let (a, b, c) = active.shown();
        let mut matches = a.iter().chain(b).chain(c).peekable();
        let mut piece = String::new();
        while matches.peek().is_some() {
            let stop = source.read_terminal(|term, resizes| {
                let grid = term.grid();
                if active.layout != Some(Layout::of(grid, resizes)) {
                    return true;
                }
                let base = screen_top(grid) - grid.scrollback_len() as u64;
                let mut lines = 0;
                while lines < SEARCH_LINES_PER_LOCK {
                    let Some(m) = matches.next() else {
                        return false;
                    };
                    lines += usize::try_from(m.end_line - m.start_line + 1).unwrap_or(usize::MAX);
                    piece.clear();
                    if !match_text(grid, base, m, &mut piece) {
                        continue;
                    }
                    let separator = usize::from(!out.text.is_empty());
                    if out.text.len() + separator + piece.len() > MAX_MATCHES_TEXT_BYTES {
                        out.truncated = true;
                        return true;
                    }
                    if separator == 1 {
                        out.text.push('\n');
                    }
                    out.text.push_str(&piece);
                }
                false
            });
            if stop {
                break;
            }
        }
        out
    }

    /// The terminal lock holds recorded so far (time, cells copied), for
    /// the lock-hold test.
    #[cfg(test)]
    pub(crate) fn take_holds(&mut self) -> Vec<(Duration, usize)> {
        std::mem::take(&mut self.holds)
    }

    /// The pieces' regex run times recorded so far, for the step-deadline
    /// test.
    #[cfg(test)]
    pub(crate) fn take_pieces(&mut self) -> Vec<Duration> {
        std::mem::take(&mut self.pieces)
    }
}

/// Plans the next chunk of `active`'s work and copies its lines under one
/// hold of the terminal lock; false when there was none.
fn copy_chunk(active: &mut Active, source: &impl TerminalSource, holds: &mut Holds) -> bool {
    #[cfg(not(test))]
    let () = *holds;
    let copied = source.read_terminal(|term, resizes| {
        #[cfg(test)]
        let held = Instant::now();
        let grid = term.grid();
        let work = active.plan(grid, resizes);
        let copied = work.map(|work| {
            let (start, end) = match work {
                Work::CatchUp { start, end }
                | Work::Screen { start, end, .. }
                | Work::Older { start, end }
                | Work::Head { start, end } => (start, end),
            };
            let seen = Seen::of(grid);
            let first = match work {
                Work::Screen { .. } => seen.first(),
                _ => seen.base,
            };
            let lines = Lines::new(grid, &seen, first);
            let run = lines.run_above(start, lines.period);
            // Reuse needs every match of the last pass: none gave way.
            let reuse =
                !active.truncated && !matches!(work, Work::Older { .. } | Work::Head { .. });
            let count = copy_lines(
                &mut active.pool,
                &mut active.unchanged,
                reuse.then_some((&mut active.scanned, &mut active.spare)),
                grid,
                seen.base,
                start,
                end,
            );
            (work, count, run, lines.period)
        });
        #[cfg(test)]
        holds.push((
            held.elapsed(),
            copied
                .as_ref()
                .map_or(0, |&(_, count, ..)| count * usize::from(grid.cols())),
        ));
        copied
    });
    let Some((work, count, run, period)) = copied else {
        return false;
    };
    active.begin(work, count, run, period);
    true
}

/// Appends the text of `m` to `out`: spacers and padding contribute
/// nothing, a cluster its whole text. False when its lines are gone.
fn match_text(grid: &Grid, base: u64, m: &Match, out: &mut String) -> bool {
    for number in m.start_line..=m.end_line {
        let Some(line) = line_at(grid, base, number) else {
            return false;
        };
        let start = if number == m.start_line {
            usize::from(m.start_col)
        } else {
            0
        };
        let end = if number == m.end_line {
            usize::from(m.end_col)
        } else {
            line.cells.len()
        }
        .min(line.cells.len());
        for col in start..end {
            let cell = &line.cells[col];
            if cell.is_spacer() || cell.is_pad() {
                continue;
            }
            match line.cluster(col).filter(|_| cell.is_cluster()) {
                Some(text) => out.push_str(text),
                None => out.push(cell.ch),
            }
        }
    }
    true
}

/// Scrolls `session`'s view so `found` is in view, centred, unless it is
/// in view already (or on the alternate screen, which does not scroll).
pub(crate) fn reveal(session: &Session, found: Match) {
    let offset = session.with_viewport(|term, offset| {
        let grid = term.grid();
        if grid.is_alt_screen() {
            return None;
        }
        let pushed = screen_top(grid);
        let len = grid.scrollback_len() as u64;
        let base = pushed - len;
        let rows = u64::from(grid.rows());
        let top = pushed - (offset as u64).min(len);
        if found.start_line < base || (found.start_line >= top && found.end_line < top + rows) {
            return None;
        }
        let height = found.end_line - found.start_line + 1;
        let centred = found
            .start_line
            .saturating_sub(rows.saturating_sub(height) / 2);
        let top = centred.clamp(base, pushed);
        Some(usize::try_from(pushed - top).unwrap_or(usize::MAX))
    });
    if let Some(offset) = offset {
        session.scroll_to(ScrollTarget::Offset(offset));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;

    /// A terminal behind a lock, as a session holds it.
    struct Shared {
        term: Mutex<Terminal>,
        resizes: AtomicU64,
    }

    impl Shared {
        fn new(term: Terminal) -> Self {
            Self {
                term: Mutex::new(term),
                resizes: AtomicU64::new(0),
            }
        }
    }

    impl TerminalSource for Shared {
        fn read_terminal<R>(&self, f: impl FnOnce(&Terminal, u64) -> R) -> R {
            let term = self.term.lock().unwrap();
            f(&term, self.resizes.load(Ordering::Relaxed))
        }
    }

    const LITERAL: QueryOptions = QueryOptions {
        regex: false,
        case_sensitive: true,
    };

    /// A terminal of `rows` x `cols` holding `lines` lines of history.
    fn filled(rows: u16, cols: u16, lines: usize, line: &str) -> Terminal {
        let mut term = Terminal::new(rows, cols);
        term.set_max_scrollback(lines + 1_000);
        let chunk = format!("{line}\r\n").repeat(1_000);
        for _ in 0..lines.div_ceil(1_000) {
            term.advance(chunk.as_bytes());
        }
        term
    }

    fn percentile(sorted: &[Duration], per_cent: usize) -> Duration {
        sorted[(sorted.len() * per_cent / 100).min(sorted.len() - 1)]
    }

    /// S6: while output floods the terminal, each search step holds its
    /// lock briefly: over 1000 steps on a 10 000-line, 2048-column history,
    /// the 99th percentile hold is at most 1 ms. The pattern never
    /// matches, so the match cap does not stop the older-history scan and
    /// every chunk is a full one.
    ///
    /// Timing-based, so run it only in release builds:
    /// `cargo nextest run --release -p tessera-app-core
    /// search_lock_hold`. Up to three attempts, as for
    /// `reader_lock_hold_is_bounded`. A step that copied a 256-line chunk
    /// regardless of width (512 Ki cells here) fails the cell check of
    /// [`each_hold_copies_at_most_a_haystack_of_cells`] at once and the
    /// time bar on most attempts.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn search_lock_hold_is_bounded() {
        const ATTEMPTS: usize = 3;
        let line: String = "lorem ipsum dolor sit amet "
            .chars()
            .cycle()
            .take(2048)
            .collect();
        let mut misses = Vec::new();
        for attempt in 1..=ATTEMPTS {
            let shared = Arc::new(Shared::new(filled(50, 2048, 10_000, &line)));
            let stop = Arc::new(AtomicBool::new(false));
            let flood = {
                let shared = Arc::clone(&shared);
                let stop = Arc::clone(&stop);
                let chunk = format!("{line}\r\n").repeat(4);
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        shared.term.lock().unwrap().advance(chunk.as_bytes());
                        thread::yield_now();
                    }
                })
            };
            let mut search = SearchState::default();
            search.start("zebra", LITERAL).unwrap();
            for _ in 0..1000 {
                search.step(&*shared, Duration::ZERO);
            }
            stop.store(true, Ordering::Relaxed);
            flood.join().unwrap();
            let (mut holds, cells): (Vec<_>, Vec<_>) = search.take_holds().into_iter().unzip();
            holds.sort_unstable();
            let most = cells.iter().copied().max().unwrap_or(0);
            assert!(
                most <= MAX_HAYSTACK_BYTES,
                "a hold copied {most} cells, over {MAX_HAYSTACK_BYTES}"
            );
            let p99 = percentile(&holds, 99);
            eprintln!(
                "attempt {attempt}: {} holds, p50 {:?}, p99 {p99:?}, max {:?}, {} matches",
                holds.len(),
                percentile(&holds, 50),
                holds.last().unwrap(),
                search.status().matches,
            );
            // A zero-budget step searches one piece. Here a piece is one
            // 2048-cell line and a hold copies a haystack of cells (32
            // lines), so a chunk takes 32 steps; output caught up comes in
            // smaller chunks, so more holds.
            assert!(
                (1000 / 32..=1000).contains(&holds.len()),
                "{} holds over 1000 zero-budget steps",
                holds.len()
            );
            if p99 <= Duration::from_millis(1) {
                return;
            }
            misses.push(format!("attempt {attempt}: p99 hold {p99:?} > 1 ms"));
        }
        panic!("{misses:#?}");
    }

    /// S6, without a clock: a full scan of a 2000-line, 2048-column history
    /// (screen, older history, then new output caught up) copies at most
    /// [`MAX_HAYSTACK_BYTES`] cells under each hold of the terminal lock.
    #[test]
    fn each_hold_copies_at_most_a_haystack_of_cells() {
        let line: String = "lorem ipsum dolor sit amet "
            .chars()
            .cycle()
            .take(2048)
            .collect();
        let shared = Shared::new(filled(50, 2048, 2_000, &line));
        let mut search = SearchState::default();
        search.start("zebra", LITERAL).unwrap();
        while !search.step(&shared, Duration::ZERO).done {}
        shared
            .term
            .lock()
            .unwrap()
            .advance(format!("{line}\r\n").repeat(300).as_bytes());
        while !search.step(&shared, Duration::ZERO).done {}
        let holds = search.take_holds();
        let most = holds.iter().map(|&(_, cells)| cells).max().unwrap_or(0);
        eprintln!("{} holds, at most {most} cells", holds.len());
        assert!(holds.len() > 2_000 / 32, "{} holds", holds.len());
        assert!(
            most <= MAX_HAYSTACK_BYTES,
            "a hold copied {most} cells, over {MAX_HAYSTACK_BYTES}"
        );
    }

    /// S5: the copy of the screen rows a pass keeps and the spare
    /// allocations stay within [`MAX_KEPT_CELLS`] while wide screens are
    /// rewritten pass after pass; a screen region over it keeps nothing.
    #[test]
    fn the_kept_copy_and_its_spares_stay_within_the_cap() {
        fn kept(search: &SearchState) -> (usize, usize) {
            let active = search.active.as_ref().unwrap();
            (
                active.scanned.rows.iter().flatten().count(),
                active.spare.len(),
            )
        }
        let line: String = "lorem ipsum dolor sit amet "
            .chars()
            .cycle()
            .take(2048)
            .collect();
        for (rows, keeps) in [(50, true), (100, false)] {
            let shared = Shared::new(filled(rows, 2048, 200, &line));
            let mut search = SearchState::default();
            search.start("dolor", LITERAL).unwrap();
            let mut most = (0, 0);
            for round in 0..40 {
                while !search.step(&shared, Duration::ZERO).done {}
                let (copies, spares) = kept(&search);
                most = (most.0.max(copies), most.1.max(spares));
                // Rewrite a row in place, scroll a few in, or switch
                // screens (which drops the copy into the spares).
                let out = match round % 4 {
                    0 => format!("\x1b[{};1H{}", round % usize::from(rows) + 1, &line[..2000]),
                    1 => format!("{line}\r\n").repeat(3),
                    2 => format!(
                        "\x1b[?1049h{}",
                        format!("{line}\r\n").repeat(usize::from(rows))
                    ),
                    _ => "\x1b[?1049l".to_owned(),
                };
                shared.term.lock().unwrap().advance(out.as_bytes());
            }
            eprintln!(
                "{rows} rows: at most {} kept rows, {} spare",
                most.0, most.1
            );
            assert!(most.0 * 2048 <= MAX_KEPT_CELLS, "{rows} rows: {most:?}");
            assert!(most.1 * 2048 <= MAX_KEPT_CELLS, "{rows} rows: {most:?}");
            assert_eq!(most.0 > 0, keeps, "{rows} rows: {most:?}");
        }
    }

    /// A full scan of 1 000 000 history lines (80 columns, one match every
    /// line) finishes within 2 s. Release only, like the lock-hold bar.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn a_million_lines_scan_within_two_seconds() {
        let shared = Shared::new(filled(
            24,
            80,
            1_000_000,
            "the quick brown fox jumps over the lazy dog 0123456789",
        ));
        let mut search = SearchState::default();
        search.start("fox", LITERAL).unwrap();
        let started = Instant::now();
        let mut status = search.step(&shared, Duration::from_millis(16));
        while !status.done {
            status = search.step(&shared, Duration::from_millis(16));
        }
        let took = started.elapsed();
        eprintln!("1 000 000 lines: {took:?}, {status:?}");
        assert_eq!(status.matches, MAX_MATCHES);
        assert!(status.truncated);
        assert!(took <= Duration::from_secs(2), "{took:?}");
    }

    /// The same scan with no match: every line is read.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn a_million_lines_without_a_match_scan_within_two_seconds() {
        let shared = Shared::new(filled(
            24,
            80,
            1_000_000,
            "the quick brown fox jumps over the lazy dog 0123456789",
        ));
        let mut search = SearchState::default();
        search.start("zebra", LITERAL).unwrap();
        let started = Instant::now();
        let mut status = search.step(&shared, Duration::from_millis(16));
        while !status.done {
            status = search.step(&shared, Duration::from_millis(16));
        }
        let took = started.elapsed();
        eprintln!("1 000 000 lines, no match: {took:?}, {status:?}");
        assert_eq!(status.matches, 0);
        assert!(took <= Duration::from_secs(2), "{took:?}");
    }

    /// A cell of `e` and 31 combining acute accents: 63 bytes of text in
    /// one column, every scalar a word character.
    fn zalgo() -> String {
        format!("e{}", "\u{301}".repeat(31))
    }

    /// A cell of 128 bytes: a mathematical bold `A` and 31 combining
    /// stems.
    fn cluster128() -> String {
        format!("\u{1d400}{}", "\u{1d167}".repeat(31))
    }

    /// One logical line of `rows` rows of `cell` at `cols` columns, on a
    /// screen two rows taller.
    fn zalgo_terminal(cols: u16, rows: usize, cell: &str) -> Terminal {
        let mut term = Terminal::new(u16::try_from(rows + 2).unwrap(), cols);
        term.advance(cell.repeat(usize::from(cols) * rows).as_bytes());
        term
    }

    /// The history and screen matches of `search`, as filed.
    fn filed(search: &SearchState) -> (Vec<Match>, Vec<Match>) {
        let active = search.active.as_ref().unwrap();
        (
            active.history.iter().copied().collect(),
            active.screen.clone(),
        )
    }

    /// E1 (#46b): a zero-budget step runs at most one piece's regex, so a
    /// segment of heavy combining marks (16 pieces at 2048 columns) is
    /// searched over many steps; nothing is filed before its last piece,
    /// and the matches are those of one long step.
    #[test]
    fn a_zero_budget_step_runs_at_most_one_piece() {
        let cell = zalgo();
        let mut input = String::new();
        for n in 0..2048_usize * 8 {
            input.push_str(if n.is_multiple_of(97) { "X" } else { &cell });
        }
        let mut term = Terminal::new(10, 2048);
        term.advance(input.as_bytes());
        let shared = Shared::new(term);
        let mut long = SearchState::default();
        long.start("X", LITERAL).unwrap();
        while !long.step(&shared, Duration::from_secs(60)).done {}
        let want = filed(&long);
        assert!(want.1.len() > 150, "{} matches", want.1.len());

        let mut search = SearchState::default();
        search.start("X", LITERAL).unwrap();
        let mut steps = 0;
        let mut pending = 0;
        loop {
            let status = search.step(&shared, Duration::ZERO);
            steps += 1;
            assert!(
                search.take_pieces().len() <= 1,
                "step {steps} ran two pieces"
            );
            if status.done {
                break;
            }
            if search.active.as_ref().unwrap().chunk.is_some() {
                pending += 1;
                assert_eq!(status.matches, 0, "matches filed before the last piece");
            }
        }
        eprintln!("{steps} steps, {pending} with a chunk under way");
        assert!(pending > 12, "{pending} steps left a chunk under way");
        assert_eq!(filed(&search), want);
    }

    /// E1 (#46b): output that arrives, scrolls and evicts while a chunk is
    /// searched over many steps leaves the same matches as a fresh search
    /// of the final terminal: the new history, the screen, the older
    /// history and the evicted head are each filed whole.
    #[test]
    fn a_chunk_searched_over_steps_files_every_match() {
        let row = |n: usize| {
            let mut row = String::new();
            for col in 0..300 {
                row.push_str(if (n + col).is_multiple_of(41) {
                    "X"
                } else {
                    "\u{434}\u{301}"
                });
            }
            row
        };
        let mut term = Terminal::new(10, 300);
        term.set_max_scrollback(400);
        for n in 0..300 {
            term.advance(row(n).as_bytes());
            if n.is_multiple_of(3) {
                term.advance(b"\r\n");
            }
        }
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start("X", LITERAL).unwrap();
        let mut n = 300;
        let mut pending = 0;
        for step in 0..4_000_usize {
            search.step(&shared, Duration::ZERO);
            if search.active.as_ref().unwrap().chunk.is_some() {
                pending += 1;
            }
            if step.is_multiple_of(7) && step < 3_000 {
                let mut out = row(n);
                if n.is_multiple_of(2) {
                    out.push_str("\r\n");
                }
                shared.term.lock().unwrap().advance(out.as_bytes());
                n += 1;
            }
        }
        while !search.step(&shared, Duration::ZERO).done {}
        let mut fresh = SearchState::default();
        fresh.start("X", LITERAL).unwrap();
        while !fresh.step(&shared, Duration::from_secs(60)).done {}
        eprintln!(
            "{pending} steps with a chunk under way, {} lines fed",
            n - 300
        );
        assert!(pending > 100, "{pending} steps left a chunk under way");
        let (history, screen) = filed(&search);
        let (want_history, want_screen) = filed(&fresh);
        assert!(want_history.len() > 1_000, "{} matches", want_history.len());
        let all = |a: Vec<Match>, b: Vec<Match>| {
            let mut all: Vec<Match> = a.into_iter().chain(b).collect();
            all.sort_unstable();
            all
        };
        assert_eq!(all(history, screen), all(want_history, want_screen));
        // The current match is not compared: the live one was evicted and
        // moved on, the fresh one is the newest.
        let counts = |status: SearchStatus| (status.matches, status.done, status.truncated);
        assert_eq!(counts(search.status()), counts(fresh.status()));
    }

    /// E1 (#46b): with the quadratic pattern `\w*X|\w` on text of heavy
    /// combining marks (a regex run of a second or more per 64 KiB piece,
    /// a hundred pieces per chunk), a step with the C API's largest budget
    /// (`TESSERA_SEARCH_MAX_STEP_MICROS`, 1 s) returns within that budget
    /// plus one piece. Checking the deadline between chunks only, as
    /// before, ran 28 s to 91 s.
    ///
    /// Timing-based: release only, with the other timing bars.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn a_step_ends_within_one_piece_of_its_budget() {
        const BUDGET: Duration = Duration::from_secs(1);
        /// Time for the copy under the lock and the bookkeeping around the
        /// pieces.
        const SLACK: Duration = Duration::from_millis(50);
        let regex = QueryOptions {
            regex: true,
            case_sensitive: true,
        };
        for (cols, rows, cell, name) in [
            (2048, 31, zalgo(), "e + 31 marks, 2048 x 31"),
            (80, 255, zalgo(), "e + 31 marks, 80 x 255"),
            (2048, 31, cluster128(), "128-byte clusters, 2048 x 31"),
        ] {
            let shared = Shared::new(zalgo_terminal(cols, rows, &cell));
            let mut search = SearchState::default();
            search.start(r"\w*X|\w", regex).unwrap();
            let started = Instant::now();
            let status = search.step(&shared, BUDGET);
            let took = started.elapsed();
            let pieces = search.take_pieces();
            let longest = pieces.iter().copied().max().unwrap_or_default();
            eprintln!(
                "{name}: step {took:?}, {} pieces, longest {longest:?}, {status:?}",
                pieces.len()
            );
            assert!(
                search.active.as_ref().unwrap().chunk.is_some(),
                "{name}: the step finished its chunk, so it tested nothing"
            );
            assert!(
                took <= BUDGET + longest + SLACK,
                "{name}: the step took {took:?}, over {BUDGET:?} + one piece ({longest:?})"
            );
        }
    }
}
