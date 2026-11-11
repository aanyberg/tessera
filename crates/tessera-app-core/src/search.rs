//! Scrollback search: one query per session, stepped over the session's
//! lines a bounded chunk at a time (ADR 0038, decision D6).
//!
//! [`App::search_step`](crate::App::search_step) copies at most [`SEARCH_LINES_PER_LOCK`] lines
//! (fewer on wide screens: about [`MAX_HAYSTACK_BYTES`] cells) out of the
//! terminal under its lock, releases the
//! lock and runs the regex on the copy, one piece (at most
//! [`MAX_HAYSTACK_BYTES`] of text) at a time, so a chunk can be searched
//! over several steps (`Chunk`). The frontend calls it from its display
//! link while a search is active; when nothing changed it returns after
//! one short look at the terminal.
//!
//! # Time
//!
//! The clock is read after every regex run inside a piece ([`scan_piece`];
//! for a literal query after every
//! [`LITERAL_RUNS_PER_CLOCK_READ`](tessera_search::LITERAL_RUNS_PER_CLOCK_READ) runs,
//! which together scan the piece's text at most once), so no step outlives
//! `max(4 x budget, 250 ms)` by more than one regex call, the one under way
//! at the deadline (#331). That call is not
//! interruptible: it runs on at most the current piece's text, 64 KiB until
//! a slow piece halves the limit and at most [`MIN_PIECE_BYTES`] at the
//! floor. It is bounded by bytes, not time: a pathological pattern costs
//! about 0.65-0.9 s per KiB in one call, so the first call of a query can
//! take many seconds, and each retry at half the slow piece about half as
//! long (7.3 s, then 3.5 s, measured on an 8 KB line; ADR 0038 has the
//! figures). The clock is
//! read after the last call of a piece too, the one that finds nothing, so
//! such a piece is late, not on time (F9c). The first piece of a
//! step may run for four times the budget (at least [`MIN_PIECE_TIME`]); a
//! piece that takes longer is searched again at half its own size
//! ([`PieceLimit::halved_from`]), and every later piece of the search is
//! cut at half the [`PieceLimit`] it had (#368: a short piece, late from a
//! stall, no longer cuts every later line to the floor), down to one row
//! and [`MIN_PIECE_BYTES`]. A piece searched again and late again is slow
//! at its size, not stalled: later pieces follow it to its next half (#378:
//! not one halving per slow piece). A piece at that floor may run for
//! [`LINE_TIME_LIMIT`] (or four budgets, when longer); one slower still,
//! even in one run, is skipped, keeping the matches found before the stop,
//! and so is a piece searched again, no larger than the floor, that already
//! had that deadline (searched again, it would be the same run). Above the
//! floor, a piece late the first time is always searched again, so one
//! stall does not skip a row (#378). A segment a halved limit cuts inside its line can lose
//! a match across the cut, so it counts as skipped too: either way the
//! status says lines were skipped ([`SearchStatus::lines_skipped`]). A
//! later piece of a step stops at the budget and is searched first in the
//! next step. A piece's matches reach the chunk's sink only once it has
//! run to its end (or been skipped), so a piece searched again is never
//! counted twice.
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
    Haystack, MAX_HAYSTACK_BYTES, MAX_SEGMENT_ROWS, MIN_PIECE_BYTES, Match, Piece, PieceLimit,
    Query, QueryError, QueryOptions, Rows, Segment, is_segment_start, scan_piece, segment_period,
    segments,
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

/// The time the first piece of a step may take before it counts as slow
/// when four budgets are shorter (a zero budget still searches): a slower
/// piece is searched again in halves.
pub const MIN_PIECE_TIME: Duration = Duration::from_millis(16);

/// The time a piece at the smallest [`PieceLimit`] may take, when four
/// budgets are shorter, before it is skipped (#331).
pub const LINE_TIME_LIMIT: Duration = Duration::from_millis(250);

/// Most cells of the screen region a search keeps a copy of between
/// passes ([`Scanned`]), 2.5 MB of cells: a larger region is searched
/// whole on every pass.
const MAX_KEPT_CELLS: usize = 128 << 10;

/// Where a search stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags the find bar shows"
)]
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
    /// More than [`MAX_MATCHES`] matched: the oldest are not kept. Clears
    /// once the lines whose matches gave way are gone (cleared, evicted).
    pub truncated: bool,
    /// Some lines were too slow to search whole with this query: one row
    /// of at most [`MIN_PIECE_BYTES`] of text took over
    /// `max(4 x budget, `[`LINE_TIME_LIMIT`]`)` and was skipped, or a line was cut into pieces finer than the full
    /// limit cuts (a match across such a cut is not found). Matches there
    /// may be missing from the count. Clears once those lines are gone.
    pub lines_skipped: bool,
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
    /// A piece of the region was skipped (too slow).
    skipped: bool,
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about the search's progress"
)]
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
    /// History matches gave way at [`MAX_MATCHES`] (or the older-history
    /// scan stopped there) on the lines below this one: their counts may
    /// be short. 0 when none did; cleared once the oldest line left is at
    /// or past it (#318).
    lost: u64,
    /// The last finished screen pass stopped at [`MAX_MATCHES`].
    screen_truncated: bool,
    /// A history piece below this line was skipped (too slow); 0 when none
    /// was. Cleared as `lost` is.
    skipped: u64,
    /// The last finished screen pass skipped a piece.
    screen_skipped: bool,
    /// The size of the pieces the regex runs on: halved for the rest of the
    /// search when one is slow (module docs, Time).
    limit: PieceLimit,
    /// The matches of the piece being searched, handed to the chunk's sink
    /// once the piece has run (at most one per byte of a piece:
    /// [`MAX_HAYSTACK_BYTES`]).
    found: Vec<Match>,
    /// How long each piece's regex run took, its longest single regex run
    /// (a gap between two matches) and the time its deadline allowed from
    /// the step's start, for the step-deadline tests.
    #[cfg(test)]
    pieces: Vec<(Duration, Duration, Duration)>,
    /// Pieces skipped as too slow at the smallest limit, for the tests.
    #[cfg(test)]
    skips: usize,
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
            lost: 0,
            screen_truncated: false,
            skipped: 0,
            screen_skipped: false,
            limit: PieceLimit::FULL,
            found: Vec::new(),
            #[cfg(test)]
            pieces: Vec::new(),
            #[cfg(test)]
            skips: 0,
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

    /// Matches gave way at [`MAX_MATCHES`] on lines still there: no room
    /// for older ones.
    fn truncated(&self) -> bool {
        self.lost != 0 || self.screen_truncated
    }

    fn status(&self) -> SearchStatus {
        let current = self.current.and_then(|key| {
            let (index, exact) = self.shown_index(key);
            exact.then_some(index)
        });
        let alt = self.alt();
        SearchStatus {
            active: true,
            matches: self.shown_len(),
            current,
            done: self.done,
            // On the alternate screen the history is not shown: its lost
            // matches do not make the count short (#368).
            truncated: (!alt && self.lost != 0)
                || (self.screen_shown(alt) && self.screen_truncated)
                || self.pass.as_ref().is_some_and(|pass| pass.truncated),
            lines_skipped: (!alt && self.skipped != 0)
                || (self.screen_shown(alt) && self.screen_skipped)
                || self.pass.as_ref().is_some_and(|pass| pass.skipped),
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
        self.lost = 0;
        self.screen_truncated = false;
        self.skipped = 0;
        self.screen_skipped = false;
        self.head = false;
        self.current = None;
        // On the alternate screen too: the primary's history is scanned
        // for when it comes back.
        let start = Lines::new(grid, seen, seen.base).segment_start(seen.pushed);
        self.low = start;
        self.high = start;
    }

    /// Lines below `base`, evicted at the cap or cleared, take their
    /// matches along. When that cuts into the scanned history, the rest of
    /// its oldest line is a line of its own now (as a fresh search reads
    /// it).
    fn forget_below(&mut self, base: u64) {
        if base > self.low {
            self.head = base < self.high;
        }
        while self.history.front().is_some_and(|m| m.start_line < base) {
            self.history.pop_front();
        }
        self.screen.retain(|m| m.start_line >= base);
        self.low = self.low.max(base);
        self.high = self.high.max(base);
        // Lines whose matches gave way, or that were skipped, are gone
        // (ED 3, eviction): the count is whole again (#318).
        if self.lost <= base {
            self.lost = 0;
        }
        if self.skipped <= base {
            self.skipped = 0;
        }
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
            self.screen_truncated = false;
            self.screen_skipped = false;
            self.drop_pass();
            self.scanned.clear(&mut self.spare);
            self.current = None;
        }
        let base = seen.base;
        self.forget_below(base);
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
                skipped: false,
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

        if self.head && !self.truncated() {
            return Some(Work::Head {
                start: base,
                end: history.segment_end(base, self.high),
            });
        }
        if !self.truncated() && self.low > base {
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
        self.screen_truncated = pass.truncated;
        self.screen_skipped = pass.skipped;
        if pass.truncated {
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
            if let Some(m) = self.history.pop_front() {
                self.lost = self.lost.max(m.start_line + 1);
            } else {
                self.screen.truncate(MAX_MATCHES);
                self.screen_truncated = true;
            }
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
            retry: None,
            stopped: false,
            sink,
        });
    }

    /// Searches the chunk on, segment by segment from the bottom one up,
    /// until one piece ([`scan_piece`]) has run or the chunk is done; a
    /// segment the last pass scanned with the same rows takes that pass's
    /// matches without a search ([`Scanned`]). A done chunk is filed.
    ///
    /// The piece runs until `clock`'s deadline for it (module docs, Time):
    /// one that times out is searched again later (in halves when it was
    /// the step's first piece) or, at the smallest limit, skipped.
    fn search_piece(&mut self, clock: Clock) -> Ran {
        let Some(chunk) = &mut self.chunk else {
            return Ran::Nothing;
        };
        let rows = &self.pool[..chunk.count];
        let cols = self.layout.map_or(1, |layout| layout.cols);
        let mut ran = Ran::Nothing;
        while chunk.left > 0 && !chunk.stopped && ran == Ran::Nothing {
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
                    chunk.stopped = push_kept(&self.screen, key, &mut chunk.sink).is_break();
                    chunk.left -= 1;
                    continue;
                }
                Piece::first(chunk.at.first, segment)
            };
            // A piece searched again after it was slow has a limit of its
            // own: half its size, below the search's.
            let limit = chunk.retry.unwrap_or(self.limit);
            let floor = limit.halved(cols).is_none();
            self.found.clear();
            let found = &mut self.found;
            #[cfg(test)]
            let started = Instant::now();
            // The last match's time and the longest gap between two: with a
            // pattern that matches every cell, its longest regex run.
            #[cfg(test)]
            let mut run = (started, Duration::ZERO);
            let deadline = clock.deadline(floor);
            let scan = scan_piece(
                &self.query,
                rows,
                piece,
                limit,
                Some(deadline),
                &mut self.haystack,
                |m| {
                    #[cfg(test)]
                    {
                        let now = Instant::now();
                        run = (now, run.1.max(now - run.0));
                    }
                    found.push(m);
                    ControlFlow::Continue(())
                },
            );
            #[cfg(test)]
            self.pieces.push((
                started.elapsed(),
                run.1.max(run.0.elapsed()),
                deadline.duration_since(clock.started),
            ));
            ran = Ran::Piece;
            if scan.timed_out {
                ran = Ran::Stop;
                // Searched again from where it started, its cut noted.
                chunk.piece = Some(piece);
                if !clock.first {
                    // The step's budget ran out: the next step starts with
                    // this piece, at its own deadline.
                    break;
                }
                let retried = chunk.retry.is_some();
                let half = retry_limit(limit, scan.size, cols, clock, retried);
                self.limit = later_limit(self.limit, limit, half, retried, cols);
                if let Some(half) = half {
                    // Too slow at this size: searched again at half its own
                    // size.
                    chunk.retry = Some(half);
                    break;
                }
                // Too slow at the smallest size: skipped, with what it
                // found before the stop.
                #[cfg(test)]
                {
                    self.skips += 1;
                }
                incomplete(chunk, segment, &mut self.pass, &mut self.skipped);
            } else if scan.next.is_some() && limit != PieceLimit::FULL {
                // Cut inside its line finer than the full limit cuts: a
                // match across the cut is lost, so it counts as skipped
                // (#331, F9a).
                incomplete(chunk, segment, &mut self.pass, &mut self.skipped);
            }
            for m in self.found.drain(..) {
                if chunk.sink.push(m).is_break() {
                    chunk.stopped = true;
                    break;
                }
            }
            chunk.piece = scan.next;
            chunk.retry = None;
            if scan.next.is_none() {
                chunk.left -= 1;
            }
            chunk.stopped |= scan.flow.is_break();
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
            (Work::Older { start, end }, Sink::Newest(newest)) => {
                let mut found = Vec::new();
                let full = newest.into(&mut found);
                found.sort_unstable();
                if full {
                    // Matches above the oldest one kept gave way.
                    let line = found.first().map_or(end, |m| m.start_line + 1);
                    self.lost = self.lost.max(line);
                }
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
            (work, _) => {
                debug_assert!(false, "{work:?} filed with the wrong sink");
            }
        }
    }
}

/// The limit a first piece too slow at `limit` (its own size `size`) is
/// searched again at: half its size ([`PieceLimit::halved_from`]); `None`
/// when it is to be skipped. A piece no larger than the floor that was
/// `retried` already (so it was late twice, not from one stall) and had the
/// floor's deadline would be the same run again at the floor, so it is
/// skipped at once (#368). Late the first time it is searched again, though
/// four budgets of 62.5 ms or more are the floor's deadline too: a stall,
/// not the row, may have made it late (#378).
fn retry_limit(
    limit: PieceLimit,
    size: PieceLimit,
    cols: u16,
    clock: Clock,
    retried: bool,
) -> Option<PieceLimit> {
    let floor = limit.halved(cols).is_none();
    let fits = size.bytes() <= MIN_PIECE_BYTES && size.cells() <= usize::from(cols.max(1));
    let same = retried && fits && clock.deadline(true) <= clock.deadline(floor);
    limit.halved_from(size, cols).filter(|_| !same)
}

/// Hands `sink` the matches of `screen` (sorted by line) inside `key`'s
/// rows, a segment the last pass kept, until it breaks.
fn push_kept(screen: &[Match], key: Cut, sink: &mut Sink) -> ControlFlow<()> {
    let from = screen.partition_point(|m| m.start_line < key.start);
    for m in screen[from..].iter().take_while(|m| m.start_line < key.end) {
        sink.push(*m)?;
    }
    ControlFlow::Continue(())
}

/// The search's limit for later pieces once a first piece run at `limit`
/// was late, with `search` the limit before and `half` the piece's
/// [`retry_limit`]. Late the first time, every later piece goes one halving
/// smaller than before, not cut as fine as this piece's half, so a short
/// piece late from a stall does not cut every later line to the floor
/// (#368). Late again at its own half (`retried`), the piece is slow at its
/// size, not stalled, so later pieces follow it: to its next half or,
/// skipped, to the limit it had (#378).
fn later_limit(
    search: PieceLimit,
    limit: PieceLimit,
    half: Option<PieceLimit>,
    retried: bool,
    cols: u16,
) -> PieceLimit {
    if retried {
        // The retry's limit is half its piece, at most one halving of the
        // limit before, so no larger than `search`.
        let follow = half.unwrap_or(limit);
        debug_assert!(fits_in(follow, search), "{follow:?} above {search:?}");
        follow
    } else if half.is_some() {
        search.halved(cols).unwrap_or(search)
    } else {
        search
    }
}

/// `small` is no larger than `large` in bytes and in cells.
fn fits_in(small: PieceLimit, large: PieceLimit) -> bool {
    small.bytes() <= large.bytes() && small.cells() <= large.cells()
}

/// Notes that `segment` of `chunk` was not searched whole (a piece too
/// slow was skipped, or a halved limit cut it): the status says lines were
/// skipped until those lines are gone.
fn incomplete(chunk: &Chunk, segment: Segment, pass: &mut Option<Pass>, skipped: &mut u64) {
    match (chunk.work, pass) {
        (Work::Screen { .. }, Some(pass)) => {
            pass.skipped = true;
            // Its matches cannot stand in for a search.
            pass.keep = false;
        }
        (Work::Screen { .. }, None) => {}
        _ => {
            let end = chunk.at.first + segment.end as u64;
            *skipped = (*skipped).max(end);
        }
    }
}

/// What [`Active::search_piece`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ran {
    /// No regex ran: segments took the last pass's matches, or the chunk
    /// was done.
    Nothing,
    /// A piece ran to its end.
    Piece,
    /// A piece timed out (or was skipped): the step ends.
    Stop,
}

/// When the pieces of one step must stop (module docs, Time).
#[derive(Debug, Clone, Copy)]
struct Clock {
    started: Instant,
    budget: Duration,
    /// No piece has run in this step yet.
    first: bool,
}

impl Clock {
    /// The deadline of the next piece: for the step's first, four budgets
    /// (at least [`MIN_PIECE_TIME`], or [`LINE_TIME_LIMIT`] at the
    /// smallest piece, `floor`); for a later one, the budget.
    fn deadline(&self, floor: bool) -> Instant {
        let allowed = if self.first {
            let least = if floor {
                LINE_TIME_LIMIT
            } else {
                MIN_PIECE_TIME
            };
            self.budget.saturating_mul(4).max(least)
        } else {
            self.budget
        };
        self.started + allowed
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
    /// The limit `piece` is searched again at after it was too slow (half
    /// its own size); `None` for the search's limit.
    retry: Option<PieceLimit>,
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

    /// Searches until nothing is left or `budget` has passed: at most one
    /// piece when `budget` is zero. The step ends within
    /// `max(4 x budget, LINE_TIME_LIMIT)` plus one regex call, which is not
    /// interruptible (module docs, Time). A chunk of lines is copied under
    /// one hold of the terminal lock and may be searched over several
    /// steps; its matches are filed when its last piece has run.
    pub(crate) fn step(&mut self, source: &impl TerminalSource, budget: Duration) -> SearchStatus {
        self.drawn.window = None;
        let mut clock = Clock {
            started: Instant::now(),
            budget,
            first: true,
        };
        loop {
            match self.unit(source, clock) {
                None | Some(Ran::Stop) => break,
                Some(Ran::Piece) => clock.first = false,
                Some(Ran::Nothing) => {}
            }
            if clock.started.elapsed() >= budget {
                break;
            }
        }
        self.status()
    }

    /// Searches one piece of the chunk under way, copying the next chunk
    /// out of the terminal first when none is; `None` when there was no
    /// work left.
    fn unit(&mut self, source: &impl TerminalSource, clock: Clock) -> Option<Ran> {
        let active = self.active.as_mut()?;
        if active.chunk.is_none() && !copy_chunk(active, source, &mut self.holds) {
            active.done = true;
            active.fix_current();
            return None;
        }
        let ran = active.search_piece(clock);
        active.done = false;
        active.fix_current();
        Some(ran)
    }

    /// The current match, if it is shown.
    pub(crate) fn current_match(&self) -> Option<Match> {
        let active = self.active.as_ref()?;
        let (index, exact) = active.shown_index(active.current?);
        exact.then(|| active.shown_at(index)).flatten()
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

    /// The pieces' regex run times recorded so far, each with its longest
    /// single regex run and the time its deadline allowed from the step's
    /// start, for the step-deadline test.
    #[cfg(test)]
    pub(crate) fn take_pieces(&mut self) -> Vec<(Duration, Duration, Duration)> {
        self.active
            .as_mut()
            .map(|active| std::mem::take(&mut active.pieces))
            .unwrap_or_default()
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
                !active.truncated() && !matches!(work, Work::Older { .. } | Work::Head { .. });
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

    /// E1 (#46b): a zero-budget step runs at most one piece's regex, so
    /// segments of heavy combining marks (four lines of two rows, four
    /// pieces each at 2048 columns) are searched over many steps; nothing
    /// is filed before the chunk's last piece, and the matches are those of
    /// one long step. Several segments (#331): a unit that ends only at the
    /// end of a segment, or at the last one, still runs two pieces.
    #[test]
    fn a_zero_budget_step_runs_at_most_one_piece() {
        let cell = zalgo();
        let mut input = String::new();
        for n in 0..2048_usize * 8 {
            input.push_str(if n.is_multiple_of(97) { "X" } else { &cell });
            if n % 4096 == 4095 {
                input.push_str("\r\n");
            }
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

    const REGEX: QueryOptions = QueryOptions {
        regex: true,
        case_sensitive: true,
    };

    /// About 64 KiB of text with no `X`, as `(name, terminal, cells)`: e
    /// with 31 combining marks (63 bytes a cell), Cyrillic (2 bytes) and
    /// CJK (3 bytes for two columns), each one logical line.
    fn slow_texts() -> Vec<(&'static str, Terminal, usize)> {
        let text = |rows: u16, cols: u16, cell: &str, cells: usize| {
            let mut term = Terminal::new(rows, cols);
            term.advance(cell.repeat(cells).as_bytes());
            term
        };
        vec![
            ("zalgo", zalgo_terminal(80, 13, &zalgo()), 1040),
            ("cyrillic", text(120, 300, "\u{434}", 32_768), 32_768),
            ("cjk", text(24, 2048, "\u{4e2d}", 21_845), 21_845),
        ]
    }

    /// The pattern of the #331 round-1 review (swift B1): its program is
    /// near the size limit, so one regex run on a 64 KiB piece takes
    /// 0.5-3 s depending on load and one on a 1 KiB piece 0.05-0.2 s.
    const COSTLY_RUN: &str = r"(?:[\x{434}\x{4e2d}e\x{301}]{1,200}){1,40}\b|\w";

    /// A pattern of the #331 adversarial run (F9c) that finds nothing in
    /// [`slow_texts`], so each piece is one regex run that reaches its end
    /// (about 0.1 s on 64 KiB). When the clock was read only before each
    /// run, such a piece was never late, never halved and never skipped.
    const NO_MATCH: &str = r"(?:\w{1,40}){1,5}\b\d";

    /// What a step may take beyond its deadline and its longest regex run in
    /// the step-deadline tests: the terminal lock, the copy of up to a
    /// haystack of cells and the filing of its matches, on a loaded machine.
    const STEP_SLACK: Duration = Duration::from_millis(25);

    /// The adversarial run's pathological pattern (F9c): one run takes
    /// about 0.65 s per KiB of `e`, so a 1 KiB piece at the floor outlasts
    /// [`LINE_TIME_LIMIT`].
    const NO_MATCH_PER_KIB: &str = r"(?:e{1,400}){1,250}\b\d";

    /// #331, F9, F9a, #378: over 64 KiB of combining marks, Cyrillic and
    /// CJK, no step takes longer than its first piece's deadline (four
    /// budgets, at least 16 ms, or 250 ms at the floor; never more than
    /// `max(4 x budget, 250 ms)`) plus one regex run (its longest gap
    /// between two matches: both patterns match every cell they do not
    /// take into a longer match) plus [`STEP_SLACK`] for the lock, the copy
    /// and the filing, and no piece longer than its deadline plus its own
    /// longest run; the search finishes, with every cell found
    /// (each is one match of `\w*X\b|\w`) or a piece skipped.
    /// `\w*X\b|\w` is quadratic (a 64 KiB piece takes 19 s to 32 s);
    /// with [`COSTLY_RUN`] a single run is the cost. Before the clock was
    /// read inside a piece, one step took the whole piece; before it was
    /// read after every run, one step took 16 runs. [`NO_MATCH`] finds
    /// nothing, so its one run per piece is the cost, and it must halve;
    /// at the floor, [`NO_MATCH_PER_KIB`] must skip rows and say so (F9c:
    /// before the clock was read after a last run without a match, both
    /// searched every piece to its end).
    ///
    /// Timing-based: release only, with the other timing bars.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn no_step_outlasts_four_budgets_and_a_quarter_second() {
        let quadratic = [
            Duration::ZERO,
            Duration::from_millis(4),
            Duration::from_millis(50),
        ]
        .map(|budget| (r"\w*X\b|\w", budget));
        // The find bar's budget.
        let costly = [COSTLY_RUN, NO_MATCH].map(|pattern| (pattern, Duration::from_millis(4)));
        for (pattern, budget) in quadratic.into_iter().chain(costly) {
            let bound = (budget * 4).max(LINE_TIME_LIMIT);
            for (name, term, cells) in slow_texts() {
                let shared = Shared::new(term);
                let mut search = SearchState::default();
                search.start(pattern, REGEX).unwrap();
                let mut longest = Duration::ZERO;
                let mut longest_piece = Duration::ZERO;
                let mut longest_run = Duration::ZERO;
                // The most a step took over its deadline and longest run.
                let mut longest_over = Duration::ZERO;
                let mut steps = 0;
                let started = Instant::now();
                let status = loop {
                    let step = Instant::now();
                    let status = search.step(&shared, budget);
                    let took = step.elapsed();
                    steps += 1;
                    let pieces = search.take_pieces();
                    if budget.is_zero() {
                        assert!(
                            pieces.len() <= 1,
                            "{name}: a zero budget ran {} pieces",
                            pieces.len()
                        );
                    }
                    longest = longest.max(took);
                    let mut run = Duration::ZERO;
                    // A step with no piece only copies.
                    let mut allowed = if pieces.is_empty() {
                        bound
                    } else {
                        Duration::ZERO
                    };
                    for (piece, piece_run, piece_allowed) in pieces {
                        assert!(
                            piece_allowed <= bound,
                            "{name}: a deadline of {piece_allowed:?}"
                        );
                        // A single piece's regex run, timed alone: two
                        // pieces in one unit could not hide behind the
                        // step's bound.
                        assert!(
                            piece <= piece_allowed + piece_run + STEP_SLACK,
                            "{name}, {pattern}: a piece took {piece:?}, its longest run \
                             {piece_run:?}, its deadline {piece_allowed:?}"
                        );
                        longest_piece = longest_piece.max(piece);
                        run = run.max(piece_run);
                        allowed = allowed.max(piece_allowed);
                    }
                    longest_run = longest_run.max(run);
                    longest_over = longest_over.max(took.saturating_sub(allowed + run));
                    assert!(
                        took <= allowed + run + STEP_SLACK,
                        "{name}, {pattern}, budget {budget:?}: step {steps} took {took:?}, over \
                         its deadline {allowed:?} and its longest run {run:?}"
                    );
                    if status.done {
                        break status;
                    }
                };
                let active = search.active.as_ref().unwrap();
                let (limit, skips) = (active.limit, active.skips);
                eprintln!(
                    "{name}, {pattern}, budget {budget:?}: {steps} steps in {:?}, longest \
                     {longest:?}, longest piece {longest_piece:?}, longest run \
                     {longest_run:?}, over by {longest_over:?}, limit {limit:?}, {skips} \
                     skipped, {status:?}",
                    started.elapsed()
                );
                assert!(
                    limit != PieceLimit::FULL,
                    "{name}: never halved, so tested nothing"
                );
                // A halved limit cuts these lines: the status says so.
                assert!(status.lines_skipped, "{name}: {status:?}");
                if pattern == NO_MATCH {
                    assert_eq!(status.matches, 0, "{name}: {status:?}");
                } else if pattern != COSTLY_RUN {
                    assert!(
                        skips > 0 || status.matches == cells,
                        "{name}, budget {budget:?}: {status:?}, {cells} cells"
                    );
                }
                assert!(status.matches <= cells, "{name}: {status:?}, counted twice");
            }
        }
        // At the floor, a row whose single run outlasts the line limit is
        // skipped and reported (F9c).
        floor_skips_a_row_whose_one_run_outlasts_the_line_limit();
    }

    /// At the floor, a row whose single run outlasts the line limit is
    /// skipped and reported, not searched to its end (F9c): three rows of
    /// 1100 `e`, one logical line, each row cut to 1 KiB pieces.
    fn floor_skips_a_row_whose_one_run_outlasts_the_line_limit() {
        let budget = Duration::from_millis(4);
        let bound = (budget * 4).max(LINE_TIME_LIMIT);
        let mut term = Terminal::new(30, 1100);
        term.advance("e".repeat(1100 * 3).as_bytes());
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start(NO_MATCH_PER_KIB, REGEX).unwrap();
        let mut limit = PieceLimit::FULL;
        while let Some(half) = limit.halved(1100) {
            limit = half;
        }
        search.active.as_mut().unwrap().limit = limit;
        let mut longest = Duration::ZERO;
        let status = loop {
            let step = Instant::now();
            let status = search.step(&shared, budget);
            let took = step.elapsed();
            let run = search
                .take_pieces()
                .into_iter()
                .map(|(_, run, _)| run)
                .max()
                .unwrap_or_default();
            assert!(
                took <= bound + run + STEP_SLACK,
                "floor: a step took {took:?}, its longest run {run:?}"
            );
            longest = longest.max(took);
            if status.done {
                break status;
            }
        };
        let skips = search.active.as_ref().unwrap().skips;
        eprintln!(
            "floor, {NO_MATCH_PER_KIB}: longest step {longest:?}, {skips} skipped, {status:?}"
        );
        assert!(skips > 0, "floor: no row skipped, {status:?}");
        assert!(status.lines_skipped, "floor: {status:?}");
    }

    /// F9: a piece too slow for the step's first-piece deadline is searched
    /// again in halves (the limit shrinks and stays shrunk), each match is
    /// counted once, and a zero-budget step still runs at most one piece.
    /// The 64 KiB piece of combining marks takes seconds; the halves get
    /// under 16 ms at about a kilobyte, far from the 250 ms skip.
    #[test]
    fn a_slow_piece_is_searched_again_in_halves() {
        let (_, term, cells) = slow_texts().swap_remove(0);
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start(r"\w*X\b|\w", REGEX).unwrap();
        let mut steps = 0;
        let mut limits = vec![PieceLimit::FULL];
        let status = loop {
            let status = search.step(&shared, Duration::ZERO);
            steps += 1;
            assert!(steps < 1_000, "no end: {status:?}");
            assert!(
                search.take_pieces().len() <= 1,
                "step {steps} ran two pieces"
            );
            let limit = search.active.as_ref().unwrap().limit;
            if limits.last() != Some(&limit) {
                limits.push(limit);
            }
            if status.done {
                break status;
            }
        };
        eprintln!("{steps} steps, limits {limits:?}, {status:?}");
        assert!(limits.len() > 2, "{limits:?}");
        // Each change shrinks the limit, in bytes or (at the byte floor,
        // when the halves are slow too under load) in cells, never grows
        // it (#331 round 1, rust B2).
        assert!(
            limits.windows(2).all(|w| w[1] != w[0]
                && w[1].bytes() <= w[0].bytes()
                && w[1].cells() <= w[0].cells()),
            "{limits:?}"
        );
        assert!(status.matches <= cells, "{status:?}: counted twice");
        let skips = search.active.as_ref().unwrap().skips;
        assert!(skips > 0 || status.matches == cells, "{status:?}");
        // The halved limit cut the line: the status says so.
        assert!(status.lines_skipped, "{status:?}");
        // A new query starts at the full size again.
        search.start("x", LITERAL).unwrap();
        assert_eq!(search.active.as_ref().unwrap().limit, PieceLimit::FULL);
    }

    /// F9: a piece still too slow at the smallest size is skipped, with
    /// the matches found before its stop, and the search goes on to the
    /// end and says lines were skipped. The clock is faked: the step's
    /// start is put a second in the past, so every piece is late.
    #[test]
    fn a_piece_too_slow_at_the_floor_is_skipped() {
        let mut term = Terminal::new(6, 40);
        // 15 lines in the history, five on the screen.
        term.advance("aXa\r\n".repeat(20).as_bytes());
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start("X", LITERAL).unwrap();
        // At the floor for 40 columns.
        let mut limit = PieceLimit::FULL;
        while let Some(half) = limit.halved(40) {
            limit = half;
        }
        search.active.as_mut().unwrap().limit = limit;
        let late = Clock {
            started: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            budget: Duration::ZERO,
            first: true,
        };
        let mut units = 0;
        while search.unit(&shared, late).is_some() {
            units += 1;
            assert!(units < 100, "never done");
        }
        let status = search.status();
        eprintln!("{units} units, {status:?}");
        assert!(status.lines_skipped, "{status:?}");
        assert!(status.done, "{status:?}");
        // The pieces stopped before their first regex run: nothing found.
        assert_eq!(status.matches, 0, "{status:?}");
        let active = search.active.as_ref().unwrap();
        assert!(active.skipped != 0, "no history line skipped");
        assert!(active.screen_skipped, "no screen line skipped");
        // A fresh search on time finds all 20 and skips nothing.
        let mut fresh = SearchState::default();
        fresh.start("X", LITERAL).unwrap();
        while !fresh.step(&shared, Duration::from_secs(60)).done {}
        let status = fresh.status();
        assert_eq!((status.matches, status.lines_skipped), (20, false));
        // Once the skipped lines are gone, the live search says so.
        shared
            .term
            .lock()
            .unwrap()
            .advance(b"\x1b[3J\x1b[H\x1b[2JcXc");
        while !search.step(&shared, Duration::from_secs(60)).done {}
        let status = search.status();
        assert_eq!(
            (status.matches, status.lines_skipped),
            (1, false),
            "{status:?}"
        );
    }

    /// #331 adversarial case A (F9c): a piece too slow is searched again
    /// at half its own size, so the first retry already cuts it. One line
    /// of 8000 `e` is one piece far below the full limit; halving the
    /// limit from 64 KiB re-ran the whole line at 32, 16 and 8 KiB, a
    /// costly call each time. A deadline already past makes the first
    /// piece late without any slow pattern.
    #[test]
    fn a_slow_piece_is_retried_at_half_its_own_size() {
        // 27 rows: the line is the screen's bottom row, searched first.
        let mut term = Terminal::new(27, 300);
        term.advance("e".repeat(8000).as_bytes());
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start(r"e\d", REGEX).unwrap();
        let late = Clock {
            started: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            budget: Duration::ZERO,
            first: true,
        };
        let retry = (0..10)
            .find_map(|_| {
                search.unit(&shared, late);
                search.active.as_ref().unwrap().chunk.as_ref()?.retry
            })
            .expect("never retried");
        assert!(
            retry.bytes() <= 4000 && retry.cells() <= 4000,
            "the retry of an 8000-byte line runs at {retry:?}, not cutting it"
        );
        assert!(retry.cells() >= 300, "below one row: {retry:?}");
        // Late once (perhaps a stall): later pieces go one halving down,
        // not as fine as this one's half (#368).
        assert_eq!(
            search.active.as_ref().unwrap().limit,
            PieceLimit::FULL.halved(300).unwrap()
        );
        // The scan runs at the retry's limit: late again, the piece is
        // retried smaller still or, at the floor, skipped; never retried
        // at the same limit, as a scan at the search's limit would be.
        search.unit(&shared, late);
        let active = search.active.as_ref().unwrap();
        let again = active.chunk.as_ref().and_then(|chunk| chunk.retry);
        assert!(
            again.map_or(active.skips == 1, |again| again.bytes() < retry.bytes()
                || again.cells() < retry.cells()),
            "the scan ignored the retry at {retry:?}: retried at {again:?}"
        );
        // Late again at its own half, it is slow, not stalled: later
        // pieces follow its next half, not one more halving (#378, m1).
        let again = again.expect("4000 bytes are above the floor");
        assert_eq!(search.active.as_ref().unwrap().limit, again);
    }

    /// A clock whose step started a minute ago: every piece is late.
    fn late(budget: Duration) -> Clock {
        Clock {
            started: Instant::now().checked_sub(Duration::from_secs(60)).unwrap(),
            budget,
            first: true,
        }
    }

    /// A clock whose step starts now with a long budget: on time.
    fn on_time() -> Clock {
        Clock {
            started: Instant::now(),
            budget: Duration::from_secs(60),
            first: true,
        }
    }

    /// The retry limit of `search`'s chunk under way, and its skips.
    fn retry_and_skips(search: &SearchState) -> (Option<PieceLimit>, usize) {
        let active = search.active.as_ref().unwrap();
        (
            active.chunk.as_ref().and_then(|chunk| chunk.retry),
            active.skips,
        )
    }

    /// #378 (M1): with a C API budget of 62.5 ms or more, four budgets are
    /// the floor's deadline too. A one-row piece late once, from a stall,
    /// is searched again at the floor, not skipped: on time there, its
    /// match is found and nothing is reported skipped. Late again, it is
    /// slow, not stalled, and is skipped and reported. The find bar's 4 ms
    /// budget retries it the same way.
    #[test]
    fn a_floor_sized_piece_late_once_is_retried_and_late_twice_is_skipped() {
        let mut term = Terminal::new(6, 40);
        // On the bottom row, searched first.
        term.advance(b"\r\n\r\n\r\n\r\n\r\naXa");
        let shared = Shared::new(term);
        for budget in [
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::ZERO,
        ] {
            // A stall: late once, then on time.
            let mut search = SearchState::default();
            search.start("X", LITERAL).unwrap();
            assert_eq!(search.unit(&shared, late(budget)), Some(Ran::Stop));
            assert_eq!(
                retry_and_skips(&search),
                (Some(floor(40)), 0),
                "budget {budget:?}: a stall skipped the row"
            );
            // The rest of the search goes one halving down, not to the floor.
            assert_eq!(
                search.active.as_ref().unwrap().limit,
                PieceLimit::FULL.halved(40).unwrap()
            );
            assert_eq!(search.unit(&shared, on_time()), Some(Ran::Piece));
            let status = run(&mut search, &shared);
            assert_eq!(
                (status.matches, status.lines_skipped),
                (1, false),
                "budget {budget:?}: {status:?}"
            );
            // Slow: late at the floor too, skipped and reported.
            let mut search = SearchState::default();
            search.start("X", LITERAL).unwrap();
            search.unit(&shared, late(budget));
            assert_eq!(search.unit(&shared, late(budget)), Some(Ran::Stop));
            assert_eq!(retry_and_skips(&search), (None, 1), "budget {budget:?}");
            // Skipped, the rest of the search follows it to the floor.
            assert_eq!(search.active.as_ref().unwrap().limit, floor(40));
            while search.unit(&shared, late(budget)).is_some() {}
            let status = search.status();
            assert_eq!(
                (status.matches, status.lines_skipped, status.done),
                (0, true, true),
                "budget {budget:?}: {status:?}"
            );
        }
    }

    /// #368, #378 (m4): a retried piece late again is skipped at once only
    /// when its retry at the floor would be the same run: the piece is no
    /// larger than the floor in bytes and in cells, and four budgets are
    /// the floor's deadline. Each case is late twice; `cases` lists the
    /// line, the budget and whether the second late run skips.
    #[test]
    fn a_retried_piece_late_again_is_skipped_only_if_the_floor_runs_it_again() {
        let zalgo_row = zalgo().repeat(40);
        // One row of 40 bytes, then a row of 2520: the retry (1280 bytes)
        // takes the first row only, which fits the floor.
        let short_then_heavy = format!("{}{}", "a".repeat(40), zalgo().repeat(40));
        let slow = Duration::from_millis(100);
        let cases = [
            // 2520 bytes in 40 cells: the retry's 20 cells hold 1260
            // bytes, above the floor's 1 KiB, so the floor cuts it again.
            ("zalgo row", zalgo_row.as_str(), slow, false),
            // 120 bytes in three rows: the retry's 60 cells are more than
            // one row, so the floor cuts it again.
            ("ascii rows", &"a".repeat(120), slow, false),
            // The retry holds one 40-byte row: the floor would run it again
            // with the same deadline.
            ("short row", short_then_heavy.as_str(), slow, true),
            // With the find bar's budget the floor's deadline is longer.
            (
                "short row, 4 ms",
                short_then_heavy.as_str(),
                Duration::from_millis(4),
                false,
            ),
        ];
        for (name, line, budget, skipped) in cases {
            // The line ends on the bottom row, so it is searched first.
            let cells = line.chars().filter(|&c| c != '\u{301}').count();
            let rows = cells.div_ceil(40);
            let mut term = Terminal::new(u16::try_from(rows).unwrap(), 40);
            term.advance(line.as_bytes());
            let shared = Shared::new(term);
            let mut search = SearchState::default();
            search.start("X", LITERAL).unwrap();
            assert_eq!(search.unit(&shared, late(budget)), Some(Ran::Stop));
            let (first, skips) = retry_and_skips(&search);
            assert!(first.is_some() && skips == 0, "{name}: {first:?}, {skips}");
            assert_eq!(search.unit(&shared, late(budget)), Some(Ran::Stop));
            let expected = if skipped {
                (None, 1)
            } else {
                (Some(floor(40)), 0)
            };
            assert_eq!(retry_and_skips(&search), expected, "{name}");
        }
    }

    /// #378 (m3): a retried piece's limit is its own: once it has run, the
    /// next piece runs at the search's limit again. The bottom line's first
    /// piece is late and retried at half its 120 cells; on time from then
    /// on, the rest of it and the 240-cell line above take one piece each at
    /// the search's limit (at the retry's 60 cells, five).
    #[test]
    fn the_piece_after_a_retried_one_runs_at_the_search_limit() {
        // Nine rows: no blank row takes a piece.
        let mut term = Terminal::new(9, 40);
        term.advance(format!("{}\r\n{}", "b".repeat(240), "a".repeat(120)).as_bytes());
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start("X", LITERAL).unwrap();
        assert_eq!(search.unit(&shared, late(Duration::ZERO)), Some(Ran::Stop));
        let (retry, _) = retry_and_skips(&search);
        assert_eq!(retry.map(PieceLimit::cells), Some(60));
        assert_eq!(search.unit(&shared, on_time()), Some(Ran::Piece));
        assert_eq!(
            retry_and_skips(&search),
            (None, 0),
            "the retry outlived its piece"
        );
        let mut units = 0;
        while search.unit(&shared, on_time()).is_some() {
            units += 1;
            assert!(units < 20, "never done");
        }
        assert_eq!(units, 2, "the pieces after the retry ran at its limit");
        assert!(search.status().done);
    }

    /// The deadline of a step's pieces (module docs, Time): the first gets
    /// four budgets, at least [`MIN_PIECE_TIME`] or, at the floor,
    /// [`LINE_TIME_LIMIT`]; a later one gets the budget (#368).
    #[test]
    fn a_step_first_piece_gets_four_budgets_or_its_least() {
        let started = Instant::now();
        let after = |budget: u64, first: bool, floor: bool| {
            let clock = Clock {
                started,
                budget: Duration::from_millis(budget),
                first,
            };
            clock.deadline(floor) - started
        };
        let ms = Duration::from_millis;
        assert_eq!(after(10, true, false), ms(40));
        assert_eq!(after(1, true, false), MIN_PIECE_TIME);
        assert_eq!(after(1, true, true), LINE_TIME_LIMIT);
        assert_eq!(after(100, true, true), ms(400));
        assert_eq!(after(10, false, false), ms(10));
        assert_eq!(after(10, false, true), ms(10));
    }

    /// #331 round 1 (rust B1, F9a): a halved limit cuts a logical line
    /// inside, and a match across the cut is not found; the status then
    /// says lines were skipped, so the count is never short without saying
    /// so. At the floor for 40 columns every row is a piece, and each
    /// `hello` crosses a row's end. History and screen both say so, and a
    /// second screen pass does not reuse the cut segment's matches.
    #[test]
    fn a_match_lost_at_a_halved_cut_is_reported() {
        let mut term = Terminal::new(6, 40);
        let hellos = format!("hello{}", "a".repeat(35)).repeat(30);
        // Two such lines: the screen region takes the second whole (it
        // reaches the screen), the history holds the first.
        let line = format!("{}{hellos}\r\n", "a".repeat(37));
        term.advance(line.repeat(2).as_bytes());
        let shared = Shared::new(term);
        // At the full limit: all 60 found, nothing skipped.
        let mut fresh = SearchState::default();
        fresh.start("hello", LITERAL).unwrap();
        while !fresh.step(&shared, Duration::from_secs(60)).done {}
        let status = fresh.status();
        assert_eq!((status.matches, status.lines_skipped), (60, false));
        let mut search = SearchState::default();
        search.start("hello", LITERAL).unwrap();
        let mut limit = PieceLimit::FULL;
        while let Some(half) = limit.halved(40) {
            limit = half;
        }
        search.active.as_mut().unwrap().limit = limit;
        while !search.step(&shared, Duration::from_secs(60)).done {}
        let status = search.status();
        assert!(
            status.matches < 30,
            "{status:?}: nothing lost, tested nothing"
        );
        assert!(status.lines_skipped, "{status:?}: lost at cuts, unreported");
        let active = search.active.as_ref().unwrap();
        assert_eq!(active.skips, 0, "no piece was slow");
        assert!(active.skipped != 0, "the history's cut not noted");
        assert!(active.screen_skipped, "the screen's cut not noted");
        // New output on the last row: the next screen pass searches the cut
        // segment again rather than keep its short count.
        shared.term.lock().unwrap().advance(b"x");
        while !search.step(&shared, Duration::from_secs(60)).done {}
        assert!(search.active.as_ref().unwrap().screen_skipped);
        assert!(search.status().lines_skipped);
    }

    /// F9: a later piece of a step that passes the budget is not halved or
    /// skipped: it runs first in the next step.
    #[test]
    fn a_later_piece_past_the_budget_runs_first_next_step() {
        let mut term = Terminal::new(6, 40);
        term.advance(b"aXa\r\nbXb\r\n");
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start("X", LITERAL).unwrap();
        let late = Clock {
            started: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            budget: Duration::ZERO,
            first: false,
        };
        for _ in 0..10 {
            assert_eq!(search.unit(&shared, late), Some(Ran::Stop));
        }
        let active = search.active.as_ref().unwrap();
        assert_eq!(active.limit, PieceLimit::FULL);
        assert_eq!(search.status().matches, 0);
        assert!(!search.status().lines_skipped);
        while !search.step(&shared, Duration::ZERO).done {}
        let status = search.status();
        assert_eq!(
            (status.matches, status.lines_skipped),
            (2, false),
            "{status:?}"
        );
    }

    /// Steps `search` over `shared` to the end with a long budget.
    fn run(search: &mut SearchState, shared: &Shared) -> SearchStatus {
        while !search.step(shared, Duration::from_secs(60)).done {}
        search.status()
    }

    /// What a fresh search for `x` finds on `shared`: count, truncation.
    fn fresh_x(shared: &Shared) -> (usize, bool, bool) {
        let mut fresh = SearchState::default();
        fresh.start("x", LITERAL).unwrap();
        let status = run(&mut fresh, shared);
        (status.matches, status.truncated, status.lines_skipped)
    }

    fn counts(status: SearchStatus) -> (usize, bool, bool) {
        (status.matches, status.truncated, status.lines_skipped)
    }

    /// 600 lines of 200 `x` on a 24 x 200 screen: 120 000 matches, so the
    /// search for `x` is truncated.
    fn truncated_x(scrollback: usize) -> (Shared, SearchState) {
        let mut term = Terminal::new(24, 200);
        term.set_max_scrollback(scrollback);
        term.advance(format!("{}\r\n", "x".repeat(200)).repeat(600).as_bytes());
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start("x", LITERAL).unwrap();
        let status = run(&mut search, &shared);
        assert_eq!((status.matches, status.truncated), (MAX_MATCHES, true));
        (shared, search)
    }

    /// #318 (adversarial t1): ED 3 clears the history whose matches gave
    /// way; the live search is no longer truncated, as a fresh one is not.
    #[test]
    fn truncation_clears_when_ed3_clears_its_lines() {
        let (shared, mut search) = truncated_x(10_000);
        shared.term.lock().unwrap().advance(b"\x1b[3J");
        let live = counts(run(&mut search, &shared));
        let fresh = fresh_x(&shared);
        assert_eq!(live, fresh);
        assert!(!fresh.1, "{fresh:?}");
    }

    /// #318 (adversarial t1b): eviction takes every line whose matches gave
    /// way; the live search is no longer truncated.
    #[test]
    fn truncation_clears_when_its_lines_are_evicted() {
        let (shared, mut search) = truncated_x(1_000);
        shared
            .term
            .lock()
            .unwrap()
            .advance("plain\r\n".repeat(1_100).as_bytes());
        let live = counts(run(&mut search, &shared));
        assert_eq!(live, fresh_x(&shared));
        assert_eq!(live, (0, false, false));
    }

    /// #318 (adversarial t1d): the same on the alternate screen, and after
    /// leaving it.
    #[test]
    fn truncation_clears_on_the_alternate_screen() {
        let (shared, mut search) = truncated_x(10_000);
        shared
            .term
            .lock()
            .unwrap()
            .advance(b"\x1b[?1049h\x1b[3Jxx\r\nx");
        let live = counts(run(&mut search, &shared));
        assert_eq!(live, fresh_x(&shared));
        assert!(!live.1, "{live:?}");
        shared.term.lock().unwrap().advance(b"\x1b[?1049l");
        let live = counts(run(&mut search, &shared));
        assert_eq!(live, fresh_x(&shared));
        assert!(!live.1, "{live:?}");
    }

    /// #318: a screen pass cut at the cap (102 400 matches on a 50 x 2048
    /// screen) makes the search truncated only until a pass that fits.
    #[test]
    fn a_truncated_screen_pass_clears_with_the_next_one() {
        let mut term = Terminal::new(50, 2048);
        term.set_max_scrollback(0);
        term.advance("x".repeat(50 * 2048 - 1).as_bytes());
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start("x", LITERAL).unwrap();
        let status = run(&mut search, &shared);
        assert_eq!((status.matches, status.truncated), (MAX_MATCHES, true));
        shared.term.lock().unwrap().advance(b"\x1b[H\x1b[2Jxx");
        let live = counts(run(&mut search, &shared));
        assert_eq!(live, fresh_x(&shared));
        assert_eq!(live, (2, false, false));
    }

    /// The floor limit for `cols` columns.
    fn floor(cols: u16) -> PieceLimit {
        let mut limit = PieceLimit::FULL;
        while let Some(half) = limit.halved(cols) {
            limit = half;
        }
        limit
    }

    /// Searches `shared` for `X` with every piece late at the floor (all
    /// skipped), then on time at the full limit.
    fn skip_all(shared: &Shared) -> SearchState {
        let mut search = SearchState::default();
        search.start("X", LITERAL).unwrap();
        search.active.as_mut().unwrap().limit = floor(40);
        let late = Clock {
            started: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            budget: Duration::ZERO,
            first: true,
        };
        while search.unit(shared, late).is_some() {}
        assert!(search.active.as_ref().unwrap().skipped != 0);
        search.active.as_mut().unwrap().limit = PieceLimit::FULL;
        run(&mut search, shared);
        search
    }

    /// #368 (M10): the skipped mark is the skipped segment's end, so
    /// evicting the first row of a skipped three-row line leaves the status
    /// saying lines were skipped; evicting all of it clears that.
    #[test]
    fn a_skipped_line_counts_until_its_last_row_is_evicted() {
        let mut term = Terminal::new(6, 40);
        term.set_max_scrollback(3);
        term.advance(format!("{}\r\n{}", "aX".repeat(60), "b\r\n".repeat(5)).as_bytes());
        assert_eq!(term.grid().scrollback_len(), 3);
        let shared = Shared::new(term);
        let mut search = skip_all(&shared);
        assert!(search.status().lines_skipped);
        let mut evict = |lines: usize| {
            shared
                .term
                .lock()
                .unwrap()
                .advance("b\r\n".repeat(lines).as_bytes());
            run(&mut search, &shared).lines_skipped
        };
        assert!(evict(1), "a row of the skipped line left, unreported");
        assert!(evict(1), "a row of the skipped line left, unreported");
        assert!(!evict(1), "the skipped line is gone");
    }

    /// #368 (M4, and the screen flags' reset): history lines skipped do not
    /// count on the alternate screen, which does not show them, and count
    /// again back on the primary. The screen's flags are of the other
    /// screen from the switch on.
    #[test]
    fn skipped_history_does_not_count_on_the_alternate_screen() {
        let mut term = Terminal::new(6, 40);
        term.advance("aXa\r\n".repeat(20).as_bytes());
        let shared = Shared::new(term);
        let mut search = skip_all(&shared);
        assert!(search.status().lines_skipped);
        // The primary screen skipped too: its flag must not survive.
        search.active.as_mut().unwrap().screen_skipped = true;
        search.active.as_mut().unwrap().screen_truncated = true;
        shared.term.lock().unwrap().advance(b"\x1b[?1049hX");
        search.step(&shared, Duration::ZERO);
        let active = search.active.as_ref().unwrap();
        assert!(!active.screen_skipped && !active.screen_truncated);
        let status = run(&mut search, &shared);
        assert_eq!(
            (status.matches, status.truncated, status.lines_skipped),
            (1, false, false)
        );
        shared.term.lock().unwrap().advance(b"\x1b[?1049l");
        assert!(run(&mut search, &shared).lines_skipped);
    }

    /// #368: matches lost from the history at the cap do not make the
    /// alternate screen's count short; back on the primary they do again.
    #[test]
    fn lost_history_matches_do_not_truncate_the_alternate_screen() {
        let (shared, mut search) = truncated_x(10_000);
        shared.term.lock().unwrap().advance(b"\x1b[?1049hxx");
        let live = counts(run(&mut search, &shared));
        assert_eq!(live, (2, false, false));
        assert_eq!(live, fresh_x(&shared));
        shared.term.lock().unwrap().advance(b"\x1b[?1049l");
        let live = counts(run(&mut search, &shared));
        assert_eq!(live, (MAX_MATCHES, true, false));
    }

    /// #368 (M2): the search stays truncated while one line whose matches
    /// gave way is left, and is whole once eviction takes exactly the last.
    #[test]
    fn truncation_clears_exactly_when_the_last_lost_line_is_evicted() {
        let (shared, mut search) = truncated_x(500);
        let lost = search.active.as_ref().unwrap().lost;
        let base = || Seen::of(shared.term.lock().unwrap().grid()).base;
        assert!(base() < lost, "nothing lost is left");
        let plain = "plain\r\n";
        while base() + 1 < lost {
            shared.term.lock().unwrap().advance(plain.as_bytes());
        }
        assert!(run(&mut search, &shared).truncated, "a lost line is left");
        shared.term.lock().unwrap().advance(plain.as_bytes());
        assert_eq!(base(), lost);
        let live = counts(run(&mut search, &shared));
        assert!(!live.1, "{live:?}");
        assert_eq!(live, fresh_x(&shared));
    }

    /// #368 (M1): a screen pass that skipped a piece is not kept as the
    /// copy the next pass reuses, so that pass searches the segment again
    /// (and skips it again, at the floor) instead of taking its short count.
    #[test]
    fn a_screen_pass_that_skipped_is_not_reused() {
        let mut term = Terminal::new(6, 40);
        term.set_max_scrollback(0);
        term.advance(b"aXa\r\nbXb");
        let shared = Shared::new(term);
        let mut search = SearchState::default();
        search.start("X", LITERAL).unwrap();
        search.active.as_mut().unwrap().limit = floor(40);
        let late = Clock {
            started: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            budget: Duration::ZERO,
            first: true,
        };
        while search.unit(&shared, late).is_some() {}
        // Both rows skipped; a step plans the pass's end.
        search.step(&shared, Duration::ZERO);
        assert!(search.status().lines_skipped);
        assert_eq!(search.active.as_ref().unwrap().scanned.seen, None);
        // On time now, after output on the last row only: the first row is
        // searched again, not taken from a copy of the skipped pass.
        shared.term.lock().unwrap().advance(b"c");
        let status = run(&mut search, &shared);
        assert_eq!((status.matches, status.lines_skipped), (2, false));
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        /// #368: the live-versus-fresh check of `tests/search.rs` at a
        /// halved limit, where pieces cut lines: the live search, stepped
        /// a piece at a time through output, resizes and screen switches,
        /// agrees with a fresh one at the same limit. Runs a stall moved
        /// the limit on are not compared.
        #[test]
        fn a_live_search_at_a_halved_limit_agrees_with_a_fresh_one(
            ops in proptest::collection::vec(halved_op(), 1..24),
            halvings in 1usize..8,
            scrollback in 0usize..300,
        ) {
            let mut term = Terminal::new(12, 40);
            term.set_max_scrollback(scrollback);
            let shared = Shared::new(term);
            let mut limit = PieceLimit::FULL;
            for _ in 0..halvings {
                limit = limit.halved(40).unwrap_or(limit);
            }
            let start = |search: &mut SearchState| {
                search.start("X[ab]*|\u{4e00}", REGEX).unwrap();
                search.active.as_mut().unwrap().limit = limit;
            };
            let mut live = SearchState::default();
            start(&mut live);
            for op in &ops {
                match op {
                    HalvedOp::Feed(text) => shared.term.lock().unwrap().advance(text.as_bytes()),
                    HalvedOp::Step => {
                        live.step(&shared, Duration::ZERO);
                    }
                    HalvedOp::Resize(rows, cols) => {
                        shared.term.lock().unwrap().resize(*rows, *cols);
                        shared.resizes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            let live_status = run(&mut live, &shared);
            let mut fresh = SearchState::default();
            start(&mut fresh);
            let fresh_status = run(&mut fresh, &shared);
            let stalled = |search: &SearchState| {
                let active = search.active.as_ref().unwrap();
                active.limit != limit || active.skips != 0
            };
            proptest::prop_assume!(!stalled(&live) && !stalled(&fresh));
            proptest::prop_assert_eq!(counts(live_status), counts(fresh_status));
            let shown = |search: &SearchState| {
                let (a, b, c) = search.active.as_ref().unwrap().shown();
                [a, b, c].concat()
            };
            proptest::prop_assert_eq!(shown(&live), shown(&fresh));
        }
    }

    /// An operation of [`a_live_search_at_a_halved_limit_agrees_with_a_fresh_one`].
    #[derive(Debug, Clone)]
    enum HalvedOp {
        Feed(String),
        Step,
        Resize(u16, u16),
    }

    /// Heavy output (zalgo runs of several kilobytes, so one line is many
    /// pieces at a halved limit), `X` marks and the screen controls of the
    /// integration tests' `heavy_op`.
    fn halved_op() -> impl proptest::strategy::Strategy<Value = HalvedOp> {
        use proptest::prelude::*;
        let piece = prop_oneof![
            4 => (1usize..200).prop_map(|n| zalgo().repeat(n)),
            4 => Just("X".to_owned()),
            2 => "[ab ]{1,12}",
            2 => Just("\r\n".to_owned()),
            1 => Just("\u{4e00}".to_owned()),
            1 => Just("\x1b[2J".to_owned()),
            1 => Just("\x1b[3J".to_owned()),
            1 => Just("\x1b[H".to_owned()),
            1 => Just("\x1b[5;7H".to_owned()),
            1 => Just("\x1b[?1049h".to_owned()),
            1 => Just("\x1b[?1049l".to_owned()),
            1 => Just("\x1bc".to_owned()),
        ];
        prop_oneof![
            5 => proptest::collection::vec(piece, 1..6).prop_map(|p| HalvedOp::Feed(p.concat())),
            5 => Just(HalvedOp::Step),
            1 => (6u16..14, 20u16..48).prop_map(|(rows, cols)| HalvedOp::Resize(rows, cols)),
        ]
    }
}
