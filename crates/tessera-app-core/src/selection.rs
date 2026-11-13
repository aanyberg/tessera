//! Text selection, its text, and link hit-testing over detected URLs
//! (ADR 0025).
//!
//! A session has at most one selection. The frontend drives it through
//! [`crate::App::select_begin`] (a click, double-click or triple-click),
//! [`crate::App::select_extend`] (a drag), [`crate::App::select_adjust`]
//! (shift-click), [`crate::App::select_all`] and
//! [`crate::App::select_clear`], reads it back with
//! [`crate::App::has_selection`] and [`crate::App::selection_text`] (Copy),
//! and every frame draws it. Points are view-relative cells
//! ([`Point`]: row 0 is the top row the view shows at its scroll offset);
//! the selection itself is kept in absolute logical-line numbers, so it
//! stays on its text while output scrolls the screen and while the view is
//! scrolled back.
//!
//! # Granularity
//!
//! - [`Granularity::Char`]: whole cells. A click alone selects nothing; the
//!   first extend selects from the clicked cell to the pointer's, both
//!   included.
//! - [`Granularity::Word`]: the best smart-selection match under the
//!   click (see below), else the run of word characters (Unicode
//!   alphanumerics and [`WORD_CHARS`]) or of whitespace under the click;
//!   any other character is selected alone; past the end of the line's
//!   text nothing is selected. Words continue across soft wraps.
//! - [`Granularity::Line`]: the whole logical line, every soft-wrapped row
//!   of it at full width.
//!
//! Dragging extends by the same unit and keeps the unit first clicked
//! whole. A block selection is the rectangle between the two points.
//!
//! # Smart selection
//!
//! A double-click first tries the rules of `[selection] smart_rules` (or
//! the defaults, [`tessera_config::DEFAULT_SMART_RULES`]: paths, URLs,
//! e-mail addresses, quoted strings, Git hashes, IPv4 and IPv6 addresses
//! and more; ADR 0042) on the clicked character's logical line, soft wraps
//! joined: at most [`SMART_SCAN_LEN`] characters around the click. Every
//! match of a rule that covers the clicked character is a candidate,
//! scored as its precision's weight times its length in characters; the
//! highest score wins, then the higher precision, the longer match, the
//! earlier start and the earlier rule. Where the logical line goes on past
//! an edge of that window, a match starting (or ending) in the run of
//! non-blank characters at that edge may be part of a longer one, so it
//! is no candidate. Without a candidate the word is selected. Each rule scans
//! from left to right, matches not overlapping (the regex crate's
//! `find_iter`). Only the unit first clicked is smart: a drag and a
//! shift-click extend by words.
//!
//! # When it is cleared
//!
//! On the next look at the terminal (any of the calls above, and every
//! frame) the selection is dropped when:
//!
//! - a row whose change can change the copied text no longer holds what it
//!   held when the selection was made: a selected row on the live screen,
//!   the newest history line if selected (a whole-screen erase unwraps it
//!   from the screen's top row), and the rest of the last selected row's
//!   logical line (its text decides whether blanks at the selection's end
//!   are kept). Cells, clusters and the soft-wrap flag are compared, so a
//!   program repainting the same text keeps it. When that logical line
//!   still runs on past the screen's bottom row (its last row there is
//!   soft-wrapped), a row scrolling in below it clears the selection too.
//!   Any change to those rows clears, also where the copy would not have
//!   changed (a selection ending on text does not depend on the rest of
//!   its line): clearing too often is safe, keeping a stale copy is not;
//! - the history is cleared (ED 3, RIS, [`Grid::clear_to_cursor`]) while it
//!   held a line of it, or evicts every line of it at its cap (when it
//!   evicts only the top, the start moves to the oldest line left, at
//!   column 0). [`Grid::history_clear_generation`] tells the two apart;
//! - the grid changes size, or the screen switches to or from the alternate
//!   screen.
//!
//! [`crate::App::clear_scrollback`] and [`crate::App::resize`] to another
//! size clear it at once. The comparison looks only at the rows the grid
//! reports changed since the last look, and only while a selection exists.
//! [`crate::App::select_all`] stops at the last row with text, so output
//! written below it does not clear the selection.

use std::cmp::Reverse;
use std::ops::Range;
use std::time::{Duration, Instant};

use tessera_config::{RuleCheck, SmartRules};
pub use tessera_grid::Point;
use tessera_grid::{Cell, Color, DamageRows, Grid, Line};
use tessera_render::{Overlay, OverlayKind, Selection as Highlight};
use tessera_term::ThemeColors;

use crate::rich_text::{RunStyle, Styled};
use crate::url::{self, MAX_URL_LEN};

/// The characters besides Unicode alphanumerics that a double-click
/// selects as part of a word: iTerm2's default, so a path or a file name
/// with dashes and dots is one word. `:`, `=`, `@`, `?`, `&`, `%` and `#`
/// end a word.
pub const WORD_CHARS: &str = "/-+\\~_.";

/// What a click selects and a drag extends by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Granularity {
    /// Single cells (a click and drag).
    #[default]
    Char,
    /// Words (a double-click and drag).
    Word,
    /// Whole logical lines (a triple-click, or more, and drag).
    Line,
}

/// A link under a cell and the cells it covers, as
/// [`crate::App::link_at`] reports it: an OSC 8 hyperlink the program
/// wrote, or else a URL detected in the terminal's text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Link {
    /// The OSC 8 URI as the program sent it, or the detected URL as written
    /// on the screen (its scheme's case kept).
    pub url: String,
    /// The cells the link covers, one entry per row it is on that the view
    /// shows, top to bottom: the view-relative row and its columns (end
    /// exclusive; a wide character's two columns are both included). An OSC
    /// 8 link covers all its cells in the view, so a row has several
    /// entries when the cells of one link (one `id`) are not adjacent.
    /// Trailing punctuation that is not part of a detected URL is outside
    /// it.
    pub span: Vec<(u16, Range<u16>)>,
}

/// A cell by absolute logical-line number: line `n` is the line that was
/// the screen's top row when `n` lines had entered the history
/// ([`Grid::scrollback_pushed`]), so a line keeps its number while it
/// scrolls up the screen and into the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Pos {
    line: i64,
    col: u16,
}

/// A grid's lines by absolute number.
#[derive(Clone, Copy)]
struct Lines<'a> {
    grid: &'a Grid,
    /// The number of logical line 0 (the oldest history line).
    base: i64,
    /// The first selectable line: the oldest history line, or the top row
    /// on the alternate screen, which shows no history.
    first: i64,
    /// The number of the screen's top row.
    top: i64,
    rows: u16,
    cols: u16,
}

impl<'a> Lines<'a> {
    fn of(grid: &'a Grid) -> Self {
        let top = i64::try_from(grid.scrollback_pushed()).unwrap_or(i64::MAX);
        let len = i64::try_from(grid.scrollback_len()).unwrap_or(i64::MAX);
        let base = top.saturating_sub(len);
        Self {
            grid,
            base,
            first: if grid.is_alt_screen() { top } else { base },
            top,
            rows: grid.rows(),
            cols: grid.cols(),
        }
    }

    /// The bottom row's number.
    fn last(&self) -> i64 {
        self.top + i64::from(self.rows) - 1
    }

    /// Line `n`, if it is selectable.
    fn line(&self, n: i64) -> Option<&'a Line> {
        if n < self.first || n > self.last() {
            return None;
        }
        self.grid.logical_line(usize::try_from(n - self.base).ok()?)
    }

    /// The cell at `pos`.
    fn cell(&self, pos: Pos) -> Option<&'a Cell> {
        self.line(pos.line)?.cells.get(usize::from(pos.col))
    }

    /// The number of the view's top row at scroll `offset` (clamped as the
    /// view clamps it; always the screen on the alternate screen).
    fn view_top(&self, offset: usize) -> i64 {
        if self.grid.is_alt_screen() {
            return self.top;
        }
        let offset = i64::try_from(offset.min(self.grid.scrollback_len())).unwrap_or(0);
        self.top - offset
    }

    /// View-relative `at`, clamped to the grid, as an absolute cell.
    fn clamped(&self, offset: usize, at: Point) -> Pos {
        Pos {
            line: self.view_top(offset) + i64::from(at.row.min(self.rows - 1)),
            col: at.col.min(self.cols - 1),
        }
    }

    /// View-relative `at` as an absolute cell, `None` outside the grid.
    fn exact(&self, offset: usize, at: Point) -> Option<Pos> {
        (at.row < self.rows && at.col < self.cols).then(|| Pos {
            line: self.view_top(offset) + i64::from(at.row),
            col: at.col,
        })
    }

    /// The character at `pos`: the cell itself, or the wide character a
    /// spacer belongs to; `None` on padding and outside the grid.
    fn char_at(&self, pos: Pos) -> Option<Pos> {
        let cell = self.cell(pos)?;
        if cell.is_pad() {
            return None;
        }
        if cell.is_spacer() && pos.col > 0 {
            return Some(Pos {
                col: pos.col - 1,
                ..pos
            });
        }
        Some(pos)
    }

    /// The character after the one at `pos` on its logical line (soft
    /// wraps continue it), skipping spacers and padding.
    fn next_char(&self, pos: Pos) -> Option<Pos> {
        let mut pos = Pos {
            col: pos.col + 1,
            ..pos
        };
        loop {
            if pos.col >= self.cols {
                if !self.line(pos.line)?.wrapped {
                    return None;
                }
                pos = Pos {
                    line: pos.line + 1,
                    col: 0,
                };
            }
            let cell = self.cell(pos)?;
            if !cell.is_spacer() && !cell.is_pad() {
                return Some(pos);
            }
            pos.col += 1;
        }
    }

    /// The character before the one at `pos` on its logical line.
    fn prev_char(&self, pos: Pos) -> Option<Pos> {
        let mut pos = pos;
        loop {
            if pos.col == 0 {
                let above = self.line(pos.line - 1)?;
                if !above.wrapped {
                    return None;
                }
                pos = Pos {
                    line: pos.line - 1,
                    col: self.cols,
                };
            }
            pos.col -= 1;
            let cell = self.cell(pos)?;
            if !cell.is_spacer() && !cell.is_pad() {
                return Some(pos);
            }
        }
    }

    /// The last cell of the character at `pos` (its spacer, if wide).
    fn char_end(&self, pos: Pos) -> Pos {
        match self.cell(pos) {
            Some(cell) if cell.is_wide() && pos.col + 1 < self.cols => Pos {
                col: pos.col + 1,
                ..pos
            },
            _ => pos,
        }
    }

    /// The number of the logical line's first row for the row `n` is on.
    fn logical_top(&self, n: i64) -> i64 {
        let mut top = n;
        while top > self.first && self.line(top - 1).is_some_and(|l| l.wrapped) {
            top -= 1;
        }
        top
    }

    /// The number of the logical line's last row for the row `n` is on.
    fn logical_bottom(&self, n: i64) -> i64 {
        let mut bottom = n;
        while bottom < self.last() && self.line(bottom).is_some_and(|l| l.wrapped) {
            bottom += 1;
        }
        bottom
    }

    /// The last row of row `n`'s logical line and its last cell with text
    /// at row `n` or below it (`None`: only blanks from row `n` on).
    fn text_end(&self, n: i64) -> (i64, Option<Pos>) {
        let bottom = self.logical_bottom(n);
        let last = (n..=bottom).rev().find_map(|line| {
            let cells = &self.line(line)?.cells;
            let col = cells.iter().rposition(|c| !is_blank(c))?;
            Some(Pos {
                line,
                col: u16::try_from(col).unwrap_or(u16::MAX),
            })
        });
        (bottom, last)
    }
}

/// A cell that holds no text: a space with no marker but padding and a
/// hyperlink (styles do not count, so an erased coloured cell is blank too).
pub(crate) fn is_blank(cell: &Cell) -> bool {
    cell.ch == ' '
        && cell
            .flags
            .difference(tessera_grid::CellFlags::PAD | tessera_grid::CellFlags::LINK)
            .is_empty()
}

/// Whether two rows hold the same cells, compared field by field.
///
/// Not with `==`: `Cell`'s and `Style`'s derived comparisons are what
/// `Snapshot::sync_row` runs on every frame, and calling them from here as
/// well made the compiler stop inlining `Style::eq` there, costing frames
/// without a selection about 15 % more cycles (ADR 0025).
fn same_cells(a: &[Cell], b: &[Cell]) -> bool {
    fn same_color(left: Color, right: Color) -> bool {
        match (left, right) {
            (Color::Default, Color::Default) => true,
            (Color::Indexed(one), Color::Indexed(other)) => one == other,
            (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => (r1, g1, b1) == (r2, g2, b2),
            _ => false,
        }
    }
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.ch == y.ch
                && x.flags.bits() == y.flags.bits()
                && x.style.flags == y.style.flags
                && same_color(x.style.fg, y.style.fg)
                && same_color(x.style.bg, y.style.bg)
                && same_color(x.style.underline, y.style.underline)
        })
}

/// What a double-click groups a character with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class_of(cell: &Cell) -> Class {
    if is_blank(cell) {
        Class::Space
    } else if cell.ch.is_alphanumeric() || WORD_CHARS.contains(cell.ch) {
        Class::Word
    } else {
        Class::Other
    }
}

/// The cells the word, whitespace run or single other character at `pos`
/// covers; `None` past the end of its logical line's text and on padding.
fn word_at(lines: &Lines<'_>, pos: Pos) -> Option<(Pos, Pos)> {
    let at = lines.char_at(pos)?;
    let (_, last) = lines.text_end(at.line);
    if last.is_none_or(|last| at > last) {
        return None;
    }
    let class = class_of(lines.cell(at)?);
    if class == Class::Other {
        return Some((at, lines.char_end(at)));
    }
    let same = |p: &Pos| lines.cell(*p).is_some_and(|c| class_of(c) == class);
    let mut start = at;
    while let Some(p) = lines.prev_char(start).filter(same) {
        start = p;
    }
    let mut end = at;
    while let Some(p) = lines.next_char(end).filter(same) {
        end = p;
    }
    Some((start, lines.char_end(end)))
}

/// The cells of the whole logical line `pos` is on, every row at full
/// width.
fn line_at(lines: &Lines<'_>, pos: Pos) -> (Pos, Pos) {
    (
        Pos {
            line: lines.logical_top(pos.line),
            col: 0,
        },
        Pos {
            line: lines.logical_bottom(pos.line),
            col: lines.cols - 1,
        },
    )
}

/// The unit `granularity` selects at `pos`; `None` for a word past the
/// end of the text.
fn unit_at(lines: &Lines<'_>, pos: Pos, granularity: Granularity) -> Option<(Pos, Pos)> {
    match granularity {
        Granularity::Char => Some((pos, pos)),
        Granularity::Word => word_at(lines, pos),
        Granularity::Line => Some(line_at(lines, pos)),
    }
}

/// The most characters smart selection scans around the click (S5): the
/// clicked one, up to 4096 before it and up to 4095 after it on its
/// logical line.
pub const SMART_SCAN_LEN: usize = 8192;

/// The characters before the click a smart scan takes at most.
const SMART_SCAN_BEFORE: usize = SMART_SCAN_LEN / 2;

/// The text smart-selection rules scan: part of one logical line around
/// the clicked character, one entry per character (a cell's cluster or
/// character; spacers and padding give none).
#[derive(Debug, Default)]
struct ScanWindow {
    text: String,
    /// Each character's cell.
    cells: Vec<Pos>,
    /// The byte each character's text starts at.
    starts: Vec<usize>,
    /// The clicked character's index.
    click: usize,
    /// Whether the window starts where its logical line starts.
    line_start: bool,
    /// Whether the window ends where its logical line's text ends.
    line_end: bool,
    /// The end of the run of characters at the window's start that may
    /// continue before it: a match starting before this byte may be the
    /// tail of a longer one and is not taken. 0 when the window starts with
    /// its line; else the first character and the non-blank characters
    /// after it.
    cut_before: usize,
    /// Likewise at the end: a match ending after this byte may be cut.
    /// The text's length when the window ends with its line's text.
    cut_after: usize,
}

impl ScanWindow {
    /// The window around `at`, a character on or before `last` (the last
    /// cell with text on `at`'s logical line).
    fn around(lines: &Lines<'_>, at: Pos, last: Pos) -> Self {
        let mut before = Vec::new();
        let mut line_start = true;
        let mut pos = at;
        while let Some(prev) = lines.prev_char(pos) {
            if before.len() == SMART_SCAN_BEFORE {
                line_start = false;
                break;
            }
            before.push(prev);
            pos = prev;
        }
        let after_cap = SMART_SCAN_LEN - SMART_SCAN_BEFORE - 1;
        let mut window = Self {
            cells: Vec::with_capacity(before.len() + 1),
            line_start,
            line_end: true,
            click: before.len(),
            ..Self::default()
        };
        for &pos in before.iter().rev() {
            window.push(lines, pos);
        }
        window.push(lines, at);
        let mut pos = at;
        while pos < last {
            let Some(next) = lines.next_char(pos) else {
                break;
            };
            if window.cells.len() - window.click - 1 == after_cap {
                window.line_end = false;
                break;
            }
            window.push(lines, next);
            pos = next;
        }
        window.cut_before = if window.line_start {
            0
        } else {
            match window.text.find(char::is_whitespace) {
                Some(0) => window.bytes(0).end,
                Some(blank) => blank,
                None => window.text.len(),
            }
        };
        window.cut_after = if window.line_end {
            window.text.len()
        } else {
            let last = window.cells.len().saturating_sub(1);
            match window.text.rfind(char::is_whitespace) {
                Some(blank) if blank == window.starts.get(last).copied().unwrap_or(0) => blank,
                Some(blank) => {
                    blank
                        + window.text[blank..]
                            .chars()
                            .next()
                            .map_or(1, char::len_utf8)
                }
                None => 0,
            }
        };
        window
    }

    fn push(&mut self, lines: &Lines<'_>, pos: Pos) {
        let Some(line) = lines.line(pos.line) else {
            return;
        };
        let col = usize::from(pos.col);
        let Some(cell) = line.cells.get(col) else {
            return;
        };
        self.cells.push(pos);
        self.starts.push(self.text.len());
        match line.cluster(col) {
            Some(text) if cell.is_cluster() => self.text.push_str(text),
            _ => self.text.push(cell.ch),
        }
    }

    /// The bytes of character `i`.
    fn bytes(&self, i: usize) -> Range<usize> {
        let end = self.starts.get(i + 1).copied().unwrap_or(self.text.len());
        self.starts[i]..end
    }

    /// The characters `bytes` touches.
    fn chars(&self, bytes: Range<usize>) -> Range<usize> {
        let first = self.starts.partition_point(|&s| s <= bytes.start) - 1;
        let end = self.starts.partition_point(|&s| s < bytes.end);
        first..end
    }
}

/// The span `check` keeps of match `span` of `text`, or `None` when the
/// check drops the match.
fn checked(check: RuleCheck, text: &str, span: Range<usize>) -> Option<Range<usize>> {
    match check {
        RuleCheck::UrlTrail => Some(trim_url(text, span)),
        RuleCheck::NotBeforeHexOrColon => text[span.end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_hexdigit() && c != ':')
            .then_some(span),
        RuleCheck::NotAfterAlnumOrColon => text[..span.start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != ':')
            .then_some(span),
        _ => Some(span),
    }
}

/// URL detection's U4 and U5 on a matched URL: trailing punctuation
/// stripped, and a trailing `)` or `]` while the URL holds more of it than
/// of its opener, repeated until neither applies. Counts once, so a run of
/// closers costs one pass.
fn trim_url(text: &str, span: Range<usize>) -> Range<usize> {
    let url = &text[span.clone()];
    let count = |c: char| url.bytes().filter(|&b| b == c as u8).count();
    let (open_paren, mut close_paren) = (count('('), count(')'));
    let (open_bracket, mut close_bracket) = (count('['), count(']'));
    let mut end = url.len();
    while let Some(last) = url[..end].chars().next_back() {
        let strip = match last {
            c if url::is_trailing_punctuation(c) => true,
            ')' if close_paren > open_paren => {
                close_paren -= 1;
                true
            }
            ']' if close_bracket > open_bracket => {
                close_bracket -= 1;
                true
            }
            _ => false,
        };
        if !strip {
            break;
        }
        end -= last.len_utf8();
    }
    span.start..span.start + end
}

/// The wall time one double-click gives its smart-selection rules (#351
/// m1): checked before each rule after the first, a rule not started
/// within it is skipped. Rules are bounded in count and size (S5), not in
/// time, and 64 slow user rules take ~425 ms on 8192 characters.
pub const SMART_RULE_BUDGET: Duration = Duration::from_millis(50);

#[cfg(test)]
thread_local! {
    /// Test hook: wall time charged to the budget per rule run, so a test
    /// can spend the budget without slow rules.
    static RULE_COST: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    /// Test hook: how many rules the last [`smart_at`] on this thread ran.
    static RULES_RUN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The time the rules of a click have taken: since `started`, plus in tests
/// the charged cost of the `ran` rules.
fn rules_time(started: Instant, ran: usize) -> Duration {
    #[cfg(test)]
    let charged = RULE_COST
        .with(std::cell::Cell::get)
        .saturating_mul(u32::try_from(ran).unwrap_or(u32::MAX));
    #[cfg(not(test))]
    let charged = {
        let _ = ran;
        Duration::ZERO
    };
    started.elapsed().saturating_add(charged)
}

/// The cells of the best smart-selection match of `rules` covering the
/// character at `pos` (see the module docs), or `None`: no rule matches
/// there, or `pos` is past the end of its line's text or on padding.
///
/// The rules share [`SMART_RULE_BUDGET`]: once it is spent, the rules not
/// yet started are skipped (one debug log with the counts, S8) and the
/// best match of those that ran wins; with none the word is selected. The
/// first rule always runs.
fn smart_at(lines: &Lines<'_>, pos: Pos, rules: &SmartRules) -> Option<(Pos, Pos)> {
    // (score, weight, length, start, rule) and the characters.
    type Key = (u64, u64, usize, Reverse<usize>, Reverse<usize>);
    if rules.is_empty() {
        return None;
    }
    let at = lines.char_at(pos)?;
    let (_, last) = lines.text_end(at.line);
    let last = last.filter(|last| at <= *last)?;
    let window = ScanWindow::around(lines, at, last);
    let click = window.bytes(window.click);
    let mut best: Option<(Key, Range<usize>)> = None;
    let started = Instant::now();
    #[cfg(test)]
    RULES_RUN.with(|run| run.set(rules.len()));
    for (index, rule) in rules.rules().iter().enumerate() {
        // `index` rules have run.
        if index > 0 && rules_time(started, index) >= SMART_RULE_BUDGET {
            let ran = index;
            #[cfg(test)]
            RULES_RUN.with(|run| run.set(ran));
            tracing::debug!(
                ran,
                skipped = rules.len() - ran,
                rules = rules.len(),
                "smart selection budget spent; rules skipped"
            );
            break;
        }
        for found in rule.regex().find_iter(&window.text) {
            if found.start() >= click.end {
                break;
            }
            if found.end() <= click.start || found.is_empty() {
                continue;
            }
            // A match in a run that reaches a window edge where the line
            // goes on may be cut: dropped, never taken shortened.
            let cut = found.start() < window.cut_before || found.end() > window.cut_after;
            if cut {
                continue;
            }
            let Some(span) = checked(rule.check(), &window.text, found.range()) else {
                continue;
            };
            if span.is_empty() {
                continue;
            }
            let chars = window.chars(span);
            let weight = rule.precision().weight();
            let len = chars.len();
            let score = weight.saturating_mul(u64::try_from(len).unwrap_or(u64::MAX));
            let key = (score, weight, len, Reverse(chars.start), Reverse(index));
            if best.as_ref().is_none_or(|(best, _)| key > *best) {
                best = Some((key, chars));
            }
        }
    }
    let (_, chars) = best?;
    let first = *window.cells.get(chars.start)?;
    let last = *window.cells.get(chars.end.checked_sub(1)?)?;
    Some((first, lines.char_end(last)))
}

/// What a selection was taken against, to tell when it must be cleared.
#[derive(Debug, Clone, Default)]
struct Guard {
    rows: u16,
    cols: u16,
    alt: bool,
    /// The grid's damage generation at the last look.
    generation: u64,
    /// [`Grid::scrollback_pushed`] and [`Grid::scrollback_len`] at the last
    /// look: when either moved, every guarded row is compared.
    pushed: u64,
    len: usize,
    /// [`Grid::history_clear_generation`] at the last look: when it moved,
    /// lines left the history by clearing, not only by eviction.
    clears: u64,
    /// The number of `lines[0]`.
    first: i64,
    /// Whether the last guarded row was soft-wrapped on the screen's
    /// bottom row: its logical line continues on a row that does not
    /// exist yet, and a row scrolling in below it becomes part of it.
    open: bool,
    /// The rows whose change can change the selection's text, as they were
    /// when it was made ([`Guard::retake`]): at most the screen and one
    /// history line.
    lines: Vec<Line>,
    /// The view's top line at the last frame that checked the guard, if
    /// it is unchanged since: every guarded row that view showed then
    /// held the guarded text, and so did the frame's snapshot, which is an
    /// exact copy of the rows in view after its capture.
    synced: Option<i64>,
}

impl Guard {
    /// Takes the guard of a selection over lines `start..=end` of `lines`,
    /// reusing `self`'s buffers.
    ///
    /// The guarded rows are those whose change can change the text the
    /// selection copies, from the newest history line down: the selected
    /// rows on the live screen; the newest history line when it is
    /// selected, whose soft-wrap flag a whole-screen erase (ED 2, RIS)
    /// clears, splitting a selected line that wraps onto the screen; and
    /// the rest of the last selected row's logical line below it, whose
    /// text decides whether blanks at the end of the selection are kept
    /// (text follows on the line) or dropped. Older history lines change
    /// only by eviction, clearing and resizing, which the selection
    /// handles on its own. When that logical line runs past the screen's
    /// bottom row, the guard is open: a row scrolling in below it fails it
    /// ([`Guard::holds`]).
    fn retake(&mut self, lines: &Lines<'_>, start: i64, end: i64) {
        let grid = lines.grid;
        self.rows = lines.rows;
        self.cols = lines.cols;
        self.alt = grid.is_alt_screen();
        self.generation = grid.damage_generation();
        self.pushed = grid.scrollback_pushed();
        self.len = grid.scrollback_len();
        self.clears = grid.history_clear_generation();
        self.first = start.max(lines.top - 1).max(lines.first);
        self.synced = None;
        let last = lines.logical_bottom(end.min(lines.last()));
        self.open = last == lines.last() && lines.line(last).is_some_and(|line| line.wrapped);
        let count = usize::try_from(last - self.first + 1).unwrap_or(0);
        self.lines.truncate(count);
        for (i, n) in (self.first..=last).enumerate() {
            let Some(line) = lines.line(n) else { break };
            match self.lines.get_mut(i) {
                Some(kept) => kept.clone_from(line),
                None => self.lines.push(line.clone()),
            }
        }
    }

    /// Whether the selection over `start..` survives the grid as it is
    /// now; updates the guard's view of the grid either way.
    ///
    /// Compares the guarded rows the grid reports changed since the last
    /// look ([`Grid::damage_since`], every row when the history moved).
    /// With `frame`, the rows a view at `offset` just captured and the
    /// rows whose content that capture found changed: when the previous
    /// frame checked the guard with the view on the same lines, a guarded
    /// row in view that the capture found unchanged still holds the text,
    /// so only the changed ones are compared (a cursor moving over a
    /// selection compares nothing but each row's soft-wrap flag, which the
    /// snapshot does not hold). Rows outside the view use the damage. When
    /// the history moved or the whole screen changed, the guarded lines in
    /// the history are compared as well: a row rewritten and then scrolled
    /// off between two looks changed while it was live, and a whole-screen
    /// erase ends the newest history line. An open guard fails once a row
    /// exists below its last row: that row continues the guarded line.
    fn holds(&mut self, lines: &Lines<'_>, frame: Option<(usize, &DamageRows)>) -> bool {
        let grid = lines.grid;
        if (lines.rows, lines.cols, grid.is_alt_screen()) != (self.rows, self.cols, self.alt) {
            return false;
        }
        let guarded_last = self.first + i64::try_from(self.lines.len()).unwrap_or(i64::MAX) - 1;
        if self.open && lines.last() > guarded_last {
            return false;
        }
        let (pushed, len) = (grid.scrollback_pushed(), grid.scrollback_len());
        let moved = (pushed, len) != (self.pushed, self.len);
        let damage = if moved {
            DamageRows::All
        } else {
            grid.damage_since(self.generation)
        };
        self.generation = grid.damage_generation();
        self.pushed = pushed;
        self.len = len;
        self.clears = grid.history_clear_generation();
        let view = frame.map(|(offset, changed)| (lines.view_top(offset), changed));
        let synced = view.filter(|(top, _)| self.synced == Some(*top));
        let rows = i64::from(lines.rows);
        // Guarded rows that scrolled into the history since the last look
        // may have changed on the screen first, and erasing the whole
        // screen (ED 2, RIS) ends the newest history line (clears its
        // wrap flag): when the history moved or the whole screen changed,
        // compare the guarded lines in the history too (at most a
        // screen's worth).
        let same = |line: &Line, kept: &Line| {
            line.wrapped == kept.wrapped
                && same_cells(&line.cells, &kept.cells)
                && line.graphemes == kept.graphemes
        };
        let history_holds = !matches!(damage, DamageRows::All)
            || self.lines.iter().zip(self.first..).all(|(kept, n)| {
                n < lines.first
                    || n >= lines.top
                    || lines.line(n).is_some_and(|line| same(line, kept))
            });
        let holds = history_holds
            && (0..lines.rows).all(|row| {
                let n = lines.top + i64::from(row);
                let Some(kept) = usize::try_from(n - self.first)
                    .ok()
                    .and_then(|i| self.lines.get(i))
                else {
                    return true;
                };
                let check = match synced {
                    Some((top, changed)) if (top..top + rows).contains(&n) => {
                        changed.contains(u16::try_from(n - top).unwrap_or(u16::MAX))
                    }
                    _ => damage.contains(row),
                };
                let Some(line) = grid.line(row) else {
                    return false;
                };
                // The snapshot holds no soft-wrap flags, so the capture cannot
                // report a change of one: that flag is compared on every row.
                line.wrapped == kept.wrapped && (!check || same(line, kept))
            });
        if holds {
            self.synced = view.map(|(top, _)| top).or(self.synced);
        }
        holds
    }
}

/// The selection itself.
#[derive(Debug, Clone)]
struct Sel {
    /// The unit first clicked (or, after an adjust, the end that stayed):
    /// a drag selects from it to the unit under the pointer.
    anchor: (Pos, Pos),
    /// The selected cells, both included: the first and last in reading
    /// order, or the top-left and bottom-right corners of a block.
    start: Pos,
    end: Pos,
    granularity: Granularity,
    block: bool,
    /// False after a character click with no drag yet, and after a
    /// double-click past the text: nothing is selected.
    active: bool,
    guard: Guard,
}

impl Sel {
    /// Cuts off the lines the history evicted at its cap (LD-1): the start
    /// moves to the oldest line left, at column 0 (a block keeps its
    /// columns). False when no selected line is left, or when the history
    /// was cleared since the guard's last look (ED 3, RIS,
    /// [`Grid::clear_to_cursor`]: [`Grid::history_clear_generation`] moved)
    /// and a selected line is gone: only eviction trims. A clear and an
    /// eviction between the same two looks count as a clear.
    fn trim_evicted(&mut self, lines: &Lines<'_>) -> bool {
        let first = lines.first;
        if lines.grid.history_clear_generation() != self.guard.clears {
            return self.start.min(self.anchor.0).line >= first;
        }
        if self.end.max(self.anchor.1).line < first {
            return false;
        }
        let block = self.block;
        let trim = |pos: &mut Pos| {
            if pos.line < first {
                *pos = Pos {
                    line: first,
                    col: if block { pos.col } else { 0 },
                };
            }
        };
        trim(&mut self.start);
        trim(&mut self.anchor.0);
        trim(&mut self.anchor.1);
        true
    }

    /// Selects from the anchor to `unit`.
    fn span_to(&mut self, unit: (Pos, Pos)) {
        let points = [self.anchor.0, self.anchor.1, unit.0, unit.1];
        if self.block {
            let line = |f: fn(i64, i64) -> i64| points.iter().map(|p| p.line).reduce(f);
            let col = |f: fn(u16, u16) -> u16| points.iter().map(|p| p.col).reduce(f);
            self.start = Pos {
                line: line(i64::min).unwrap_or_default(),
                col: col(u16::min).unwrap_or_default(),
            };
            self.end = Pos {
                line: line(i64::max).unwrap_or_default(),
                col: col(u16::max).unwrap_or_default(),
            };
        } else {
            self.start = points.iter().copied().min().unwrap_or(unit.0);
            self.end = points.iter().copied().max().unwrap_or(unit.1);
        }
        self.active = true;
    }
}

/// What a frame draws besides the cells: the selection, clipped to the
/// rows the view shows, and the hovered link's underlines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Decorations {
    pub(crate) selection: Option<Highlight>,
    pub(crate) overlays: Vec<Overlay>,
}

/// A session's selection and link hover. Lives under its own lock in the
/// session, taken before the terminal lock (and after the view table's).
#[derive(Debug, Default)]
pub(crate) struct SelectionState {
    sel: Option<Sel>,
    /// The cell the pointer hovers with the link modifier held,
    /// view-relative.
    hover: Option<Point>,
}

impl SelectionState {
    /// Drops the selection if the grid no longer holds what it selected;
    /// `frame` as for [`Guard::holds`].
    fn validate(&mut self, grid: &Grid, frame: Option<(usize, &DamageRows)>) {
        if let Some(sel) = &mut self.sel {
            let lines = Lines::of(grid);
            if !sel.trim_evicted(&lines) || !sel.guard.holds(&lines, frame) {
                self.sel = None;
            }
        }
    }

    /// A click at `at` (view-relative at scroll `offset`): starts a new
    /// selection of `granularity`, a block one if `block`. A character
    /// click selects nothing until [`Self::extend`]; a word click past the
    /// end of the text selects nothing either. A word click selects the
    /// best match of `rules` there, if any, instead of the word (smart
    /// selection, see the module docs).
    pub(crate) fn begin(
        &mut self,
        grid: &Grid,
        offset: usize,
        at: Point,
        granularity: Granularity,
        block: bool,
        rules: &SmartRules,
    ) {
        let lines = Lines::of(grid);
        let pos = lines.clamped(offset, at);
        let unit = match granularity {
            Granularity::Char => None,
            Granularity::Word => smart_at(&lines, pos, rules).or_else(|| word_at(&lines, pos)),
            Granularity::Line => unit_at(&lines, pos, granularity),
        };
        let mut guard = self.sel.take().map_or_default(|sel| sel.guard);
        let (start, end) = unit.unwrap_or((pos, pos));
        guard.retake(&lines, start.line, end.line);
        self.sel = Some(Sel {
            anchor: (start, end),
            start,
            end,
            granularity,
            block,
            active: unit.is_some(),
            guard,
        });
    }

    /// A drag to `at`: selects from the anchor to the unit under `at`.
    /// Nothing without a selection begun.
    pub(crate) fn extend(&mut self, grid: &Grid, offset: usize, at: Point) {
        self.validate(grid, None);
        let Some(sel) = &mut self.sel else { return };
        let lines = Lines::of(grid);
        let pos = lines.clamped(offset, at);
        let unit = unit_at(&lines, pos, sel.granularity).unwrap_or((pos, pos));
        sel.span_to(unit);
        sel.guard.retake(&lines, sel.start.line, sel.end.line);
    }

    /// A shift-click at `at`: moves the selection's end nearer to `at`
    /// (measured in cells along the lines; a tie moves the start) to the
    /// unit there, keeping the granularity and block-ness. Nothing without
    /// a selection.
    pub(crate) fn adjust(&mut self, grid: &Grid, offset: usize, at: Point) {
        self.validate(grid, None);
        let Some(sel) = self.sel.as_mut().filter(|sel| sel.active) else {
            return;
        };
        let lines = Lines::of(grid);
        let pos = lines.clamped(offset, at);
        let unit = unit_at(&lines, pos, sel.granularity).unwrap_or((pos, pos));
        let cols = i128::from(lines.cols);
        let distance = |a: Pos, b: Pos| {
            ((i128::from(a.line) - i128::from(b.line)) * cols + i128::from(a.col)
                - i128::from(b.col))
            .abs()
        };
        let move_end = if pos > sel.end {
            true
        } else if pos < sel.start {
            false
        } else {
            distance(pos, sel.end) < distance(pos, sel.start)
        };
        let unit_of = |p: Pos| unit_at(&lines, p, sel.granularity).unwrap_or((p, p));
        let (start, end) = (sel.start, sel.end);
        // The end that stays becomes the anchor a drag continues from.
        if move_end {
            sel.end = unit.1;
            sel.anchor = (start, unit_of(start).1.min(sel.end));
        } else {
            sel.start = unit.0;
            sel.anchor = (unit_of(end).0.max(sel.start), end);
        }
        if sel.block {
            let (a, b) = (sel.start, sel.end);
            sel.start = Pos {
                line: a.line.min(b.line),
                col: a.col.min(b.col),
            };
            sel.end = Pos {
                line: a.line.max(b.line),
                col: a.col.max(b.col),
            };
        }
        sel.guard.retake(&lines, sel.start.line, sel.end.line);
    }

    /// Selects every line up to the last row with text: the history and the
    /// screen, or only the screen on the alternate screen. The blank rows
    /// below that row are left out, so output that lands in them (the next
    /// line of `yes` or `tail -f`, written on the cursor's blank row) does
    /// not change a selected row and keep the selection. A terminal with no
    /// text at all is selected whole, as it always was (its text is empty).
    pub(crate) fn select_all(&mut self, grid: &Grid) {
        let lines = Lines::of(grid);
        let start = Pos {
            line: lines.first,
            col: 0,
        };
        let last_with_text = (lines.first..=lines.last()).rev().find(|&n| {
            lines
                .line(n)
                .is_some_and(|line| !line.cells.iter().all(is_blank))
        });
        let end = Pos {
            line: last_with_text.unwrap_or_else(|| lines.last()),
            col: lines.cols - 1,
        };
        self.select_whole(&lines, start, end);
    }

    /// Selects from the start of line `first` of `grid` to column
    /// `last_col` of line `last`, both included, as a character selection
    /// (a command's output, ADR 0039); returns whether it did: `false`,
    /// the selection unchanged, unless both are selectable lines (in the
    /// history or on the screen, the screen alone on the alternate
    /// screen), `first` is not below `last` and the column is on the grid.
    pub(crate) fn select_lines(
        &mut self,
        grid: &Grid,
        first: i64,
        last: i64,
        last_col: u16,
    ) -> bool {
        let lines = Lines::of(grid);
        if first > last || first < lines.first || last > lines.last() || last_col >= lines.cols {
            return false;
        }
        let start = Pos {
            line: first,
            col: 0,
        };
        let end = Pos {
            line: last,
            col: last_col,
        };
        self.select_whole(&lines, start, end);
        true
    }

    /// Makes `start..=end` of `lines` the selection, a character one.
    fn select_whole(&mut self, lines: &Lines<'_>, start: Pos, end: Pos) {
        let mut guard = self.sel.take().map_or_default(|sel| sel.guard);
        guard.retake(lines, start.line, end.line);
        self.sel = Some(Sel {
            anchor: (start, end),
            start,
            end,
            granularity: Granularity::Char,
            block: false,
            active: true,
            guard,
        });
    }

    /// Drops the selection.
    pub(crate) fn clear(&mut self) {
        self.sel = None;
    }

    /// Whether something is selected (it may be blank, or outside the
    /// view).
    pub(crate) fn has_selection(&mut self, grid: &Grid) -> bool {
        self.validate(grid, None);
        self.sel.as_ref().is_some_and(|sel| sel.active)
    }

    /// The selected text, or `None` without a selection and for a
    /// selection on the alternate screen that holds no text; see
    /// [`crate::App::selection_text`] for the rules. `trim` drops the
    /// blanks after each logical line's text (`trim_trailing_whitespace`).
    pub(crate) fn text(&mut self, grid: &Grid, trim: bool) -> Option<String> {
        self.validate(grid, None);
        let sel = self.sel.as_ref().filter(|sel| sel.active)?;
        let text = plain(&Lines::of(grid), sel, trim);
        (!text.is_empty() || !grid.is_alt_screen()).then_some(text)
    }

    /// The selected text as [`Self::text`] gives it, and the same text in
    /// runs of one look under `colors`, collected up to `cap` bytes of
    /// text (past it the runs are [`Styled::is_over`]); `None` as for
    /// [`Self::text`]. One look at the grid gives both.
    pub(crate) fn styled_text(
        &mut self,
        grid: &Grid,
        trim: bool,
        colors: &ThemeColors,
        cap: usize,
    ) -> Option<(String, Styled)> {
        self.validate(grid, None);
        let sel = self.sel.as_ref().filter(|sel| sel.active)?;
        let lines = Lines::of(grid);
        let text = plain(&lines, sel, trim);
        if text.is_empty() && grid.is_alt_screen() {
            return None;
        }
        let mut styled = Styled::default();
        walk(&lines, sel, trim, |piece| {
            match piece {
                Piece::Cell(cell, cluster) => {
                    styled.push_cell(cell, cluster, RunStyle::of(&cell.style, colors), cap);
                }
                Piece::Newline => styled.push_newline(cap),
            }
            !styled.is_over()
        });
        Some((text, styled))
    }

    /// Stores the hovered cell (view-relative), or none.
    pub(crate) fn set_hover(&mut self, at: Option<Point>) {
        self.hover = at;
    }

    /// What the next frame of the view at scroll `offset` draws: the
    /// selection clipped to its rows, and one underline per row of the
    /// hovered link. Frames use [`Self::frame_decorations`]; this is for
    /// looks outside a frame (tests).
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn decorations(&mut self, grid: &Grid, offset: usize) -> Decorations {
        self.validate(grid, None);
        self.draw(grid, offset)
    }

    /// [`Self::decorations`] for a frame whose snapshot just captured the
    /// rows of the view at `offset`, finding the rows in `changed` changed
    /// ([`crate::view::View::capture`]): the selection is checked only
    /// against those, and against the damage outside the view.
    pub(crate) fn frame_decorations(
        &mut self,
        grid: &Grid,
        offset: usize,
        changed: &DamageRows,
    ) -> Decorations {
        self.validate(grid, Some((offset, changed)));
        self.draw(grid, offset)
    }

    /// The selection clipped to the rows of the view at scroll `offset`,
    /// unchecked as [`Self::draw`] does (checking it compares every line it
    /// guards, too slow under the terminal lock for the accessible text);
    /// `None` without one in view (ADR 0041).
    pub(crate) fn view_highlight(&self, grid: &Grid, offset: usize) -> Option<Highlight> {
        let lines = Lines::of(grid);
        self.sel
            .as_ref()
            .filter(|sel| sel.active)
            .and_then(|sel| clip(&lines, offset, sel))
    }

    /// The decorations of the selection as it is, unchecked.
    fn draw(&self, grid: &Grid, offset: usize) -> Decorations {
        let lines = Lines::of(grid);
        let selection = self
            .sel
            .as_ref()
            .filter(|sel| sel.active)
            .and_then(|sel| clip(&lines, offset, sel));
        let overlays = self
            .hover
            .and_then(|at| link_at(grid, offset, at))
            .map_or_default(|link| {
                link.span
                    .into_iter()
                    .map(|(row, cols)| Overlay::new(OverlayKind::Underline, row, cols))
                    .collect()
            });
        Decorations {
            selection,
            overlays,
        }
    }
}

/// `sel` in the rows of the view at scroll `offset`, clipped to them;
/// `None` when it is wholly outside.
fn clip(lines: &Lines<'_>, offset: usize, sel: &Sel) -> Option<Highlight> {
    let top = lines.view_top(offset);
    let bottom = top + i64::from(lines.rows) - 1;
    if sel.end.line < top || sel.start.line > bottom {
        return None;
    }
    let row = |line: i64| u16::try_from(line - top).unwrap_or(0);
    if sel.block {
        let start = Point::new(row(sel.start.line.max(top)), sel.start.col);
        let end = Point::new(row(sel.end.line.min(bottom)), sel.end.col);
        return Some(Highlight::block(start, end));
    }
    let start = if sel.start.line < top {
        Point::new(0, 0)
    } else {
        Point::new(row(sel.start.line), sel.start.col)
    };
    let end = if sel.end.line > bottom {
        Point::new(lines.rows - 1, lines.cols - 1)
    } else {
        Point::new(row(sel.end.line), sel.end.col)
    };
    Some(Highlight::linear(start, end))
}

/// One piece of a selection's text, in reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece<'a> {
    /// A cell and, for a cluster, its whole text.
    Cell(&'a Cell, Option<&'a str>),
    /// A hard line end, or the end of a block's row.
    Newline,
}

/// The pieces of `sel`'s text in order, to `emit` until it returns
/// `false`: soft-wrapped rows joined, a newline at every hard line end
/// (every row of a block), spacers and padding skipped, and no newline
/// after the last cell given (the newlines of trailing blank rows are
/// dropped). With `trim`, the blanks after the last text of each logical
/// line are left out; without it every cell of the selection is given on
/// the rows up to the last one the trimmed text has a cell on (text
/// follows the row's first selected cell on its logical line), and none
/// after it (`trim_trailing_whitespace = false`, ADR 0042).
fn walk<'a>(lines: &Lines<'a>, sel: &Sel, trim: bool, mut emit: impl FnMut(Piece<'a>) -> bool) {
    let columns = |n: i64| {
        if sel.block {
            (sel.start.col, sel.end.col)
        } else {
            (
                if n == sel.start.line {
                    sel.start.col
                } else {
                    0
                },
                if n == sel.end.line {
                    sel.end.col
                } else {
                    lines.cols - 1
                },
            )
        }
    };
    // Without trimming, the rows after the last one the trimmed text has
    // a cell on give nothing: a row counts when text follows its first
    // selected cell on its logical line (`tail`, the last text at or
    // after the row, found walking up from the last row in one pass).
    let last_row = if trim {
        sel.end.line
    } else {
        let mut tail = lines.text_end(sel.end.line).1;
        let mut row = sel.end.line;
        loop {
            let Some(line) = lines.line(row) else { return };
            if row < sel.end.line {
                let own = line
                    .cells
                    .iter()
                    .rposition(|c| !is_blank(c))
                    .map(|col| Pos {
                        line: row,
                        col: u16::try_from(col).unwrap_or(u16::MAX),
                    });
                tail = if line.wrapped { tail.or(own) } else { own };
            }
            let first = Pos {
                line: row,
                col: columns(row).0,
            };
            if tail.is_some_and(|tail| tail >= first) {
                break row;
            }
            if row == sel.start.line {
                return;
            }
            row -= 1;
        }
    };
    // Newlines wait for the next cell, so trailing ones are never given.
    let mut newlines = 0usize;
    // The current logical line's last row and last cell with text.
    let mut logical: Option<(i64, Option<Pos>)> = None;
    for n in sel.start.line..=last_row {
        let Some(line) = lines.line(n) else { break };
        let (first, last) = columns(n);
        let text_end = if trim {
            if logical.is_none_or(|(bottom, _)| n > bottom) {
                logical = Some(lines.text_end(n));
            }
            logical.and_then(|(_, end)| end)
        } else {
            None
        };
        let mut first = usize::from(first);
        if first > 0 && line.cells.get(first).is_some_and(Cell::is_spacer) {
            first -= 1;
        }
        let last = usize::from(last).min(line.cells.len().saturating_sub(1));
        for col in first..=last {
            let pos = Pos {
                line: n,
                col: u16::try_from(col).unwrap_or(u16::MAX),
            };
            if trim && text_end.is_none_or(|end| pos > end) {
                break;
            }
            let cell = &line.cells[col];
            if cell.is_spacer() || cell.is_pad() {
                continue;
            }
            for _ in 0..newlines {
                if !emit(Piece::Newline) {
                    return;
                }
            }
            newlines = 0;
            let cluster = line.cluster(col).filter(|_| cell.is_cluster());
            if !emit(Piece::Cell(cell, cluster)) {
                return;
            }
        }
        if n < sel.end.line && (sel.block || !line.wrapped) {
            newlines += 1;
        }
    }
}

/// The text of `sel` ([`walk`]).
fn plain(lines: &Lines<'_>, sel: &Sel, trim: bool) -> String {
    let mut out = String::new();
    walk(lines, sel, trim, |piece| {
        match piece {
            Piece::Cell(_, Some(text)) => out.push_str(text),
            Piece::Cell(cell, None) => out.push(cell.ch),
            Piece::Newline => out.push('\n'),
        }
        true
    });
    out
}

/// The text of `sel`: soft-wrapped rows joined, a newline at every hard
/// line end (every row of a block), the blanks after the last text of a
/// logical line dropped, spacers and padding skipped, clusters whole, and
/// the newlines of trailing blank rows dropped. The reference
/// [`plain`] with trimming must equal (a proptest checks it).
#[cfg(test)]
fn extract(lines: &Lines<'_>, sel: &Sel) -> String {
    let mut out = String::new();
    // The current logical line's last row and last cell with text.
    let mut logical: Option<(i64, Option<Pos>)> = None;
    for n in sel.start.line..=sel.end.line {
        let Some(line) = lines.line(n) else { break };
        let (first, last) = if sel.block {
            (sel.start.col, sel.end.col)
        } else {
            (
                if n == sel.start.line {
                    sel.start.col
                } else {
                    0
                },
                if n == sel.end.line {
                    sel.end.col
                } else {
                    lines.cols - 1
                },
            )
        };
        if logical.is_none_or(|(bottom, _)| n > bottom) {
            logical = Some(lines.text_end(n));
        }
        let text_end = logical.and_then(|(_, end)| end);
        let mut first = usize::from(first);
        if first > 0 && line.cells.get(first).is_some_and(Cell::is_spacer) {
            first -= 1;
        }
        let last = usize::from(last).min(line.cells.len().saturating_sub(1));
        for col in first..=last {
            let pos = Pos {
                line: n,
                col: u16::try_from(col).unwrap_or(u16::MAX),
            };
            if text_end.is_none_or(|end| pos > end) {
                break;
            }
            let cell = &line.cells[col];
            if cell.is_spacer() || cell.is_pad() {
                continue;
            }
            match line.cluster(col) {
                Some(text) if cell.is_cluster() => out.push_str(text),
                _ => out.push(cell.ch),
            }
        }
        if n < sel.end.line && (sel.block || !line.wrapped) {
            out.push('\n');
        }
    }
    let kept = out.trim_end_matches('\n').len();
    out.truncate(kept);
    out
}

/// The character a cell (at column `col` of `line`) stands for in URL
/// detection: its cluster's base scalar, or the first hidden URL end in its
/// cluster ([`url::is_hidden_url_end`]: a ZWJ joined to the character
/// before it), which is no URL character.
fn url_char(line: &Line, col: u16, cell: &Cell) -> char {
    if cell.is_cluster()
        && let Some(hidden) = line.cluster(usize::from(col)).and_then(hidden_url_end)
    {
        return hidden;
    }
    cell.ch
}

/// The first hidden URL end ([`url::is_hidden_url_end`]) in `text`, a
/// cell's cluster or a link's text.
///
/// Of the hidden ends only the ZWJ and the bidi controls from U+200E on can
/// be in a cluster, and each starts with byte `0xE2` in UTF-8, which a
/// combining mark (`0xCC` to `0xCF` for U+0300-U+036F) and an ASCII
/// character never hold, so most texts cost one byte scan. U+061C (ALM,
/// lead byte `0xD8`) is GCB=Control: the terminal drops it at print and no
/// cluster holds it (#281, tested), so the scan does not look for it.
fn hidden_url_end(text: &str) -> Option<char> {
    // No early exit, so the scan vectorises.
    if text.bytes().fold(0u8, |n, b| n | u8::from(b == 0xE2)) == 0 {
        return None;
    }
    text.chars().find(|&c| url::is_hidden_url_end(c))
}

/// Whether a cell of `cells` hides a URL end ([`url_char`] differs from
/// its base scalar).
fn hides_url_end(lines: &Lines<'_>, cells: &[Pos]) -> bool {
    let mut row: Option<(i64, &Line)> = None;
    cells.iter().any(|&p| {
        let line = match row {
            Some((n, line)) if n == p.line => line,
            _ => match lines.line(p.line) {
                Some(line) => {
                    row = Some((p.line, line));
                    line
                }
                None => return false,
            },
        };
        line.cells.get(usize::from(p.col)).is_some_and(|cell| {
            cell.is_cluster()
                && line
                    .cluster(usize::from(p.col))
                    .and_then(hidden_url_end)
                    .is_some()
        })
    })
}

/// Whether a hidden URL end in a cell of the window `cells` (with `chars`,
/// their base scalars) could give the point (`index`) a URL when
/// [`url::url_at`] finds none on the base scalars: whether one of the cells
/// that can change the answer hides one ([`hides_url_end`]).
///
/// A hidden end makes its cell a character that is no URL character, which
/// ends the run of URL characters there. It cannot start a scheme, and so
/// it cannot add a candidate. Proof that only these cells can change "no
/// URL", given the starts `s <= index` where a scheme's prefix starts
/// ([`url::starts_scheme`]; with none there is no candidate on either
/// side, so no cell matters):
///
/// - **the cell before a start** (`s - 1`), when its base scalar blocks the
///   scheme (U1, [`url::blocks_scheme`]): the hidden end does not, so the
///   start can become a URL. When the base scalar does not block, both
///   leave U1 passing and only the run's start moves, which changes no
///   candidate of a later start;
/// - **every cell after the point**: cutting the run there shortens each
///   candidate, which can bring an over-cap one under U9 or turn a CJK cut
///   (where U4 strips a trailing `-`) into the run's end (where it keeps
///   it);
/// - no other cell. One before the point that is not before a start cuts
///   the run: the candidates of starts before it lose the point, and those
///   after it are the base's own (same scheme, same character before it,
///   same text from it on, as the CJK cut's label scan stops at the
///   scheme), which found nothing. The point's own cell is checked by
///   [`window`].
///
/// The cells inside a scheme only break it, which also adds no URL. So
/// over text that has a `:` but no scheme (a time, a path) this reads no
/// cluster (#281).
fn hidden_end_can_link(lines: &Lines<'_>, chars: &[char], cells: &[Pos], index: usize) -> bool {
    let mut started = false;
    for s in 0..=index {
        if !url::starts_scheme(&chars[s..]) {
            continue;
        }
        started = true;
        if s > 0 && url::blocks_scheme(chars[s - 1]) && hides_url_end(lines, &cells[s - 1..s]) {
            return true;
        }
    }
    started && hides_url_end(lines, &cells[index + 1..])
}

/// The character before `pos` on its logical line, as
/// [`Lines::prev_char`] finds it, with its base scalar if that is a URL
/// character (a cell that hides a URL end is found later, see
/// [`link_with_reach`]). `row` is the line of `pos` and moves along with
/// it, so the walk looks a line up once per row, not once per cell.
fn prev_url_char<'a>(lines: &Lines<'a>, row: &mut &'a Line, pos: Pos) -> Option<(Pos, char)> {
    let mut pos = pos;
    loop {
        if pos.col == 0 {
            let above = lines.line(pos.line - 1)?;
            if !above.wrapped {
                return None;
            }
            *row = above;
            pos = Pos {
                line: pos.line - 1,
                col: lines.cols,
            };
        }
        pos.col -= 1;
        let cell = row.cells.get(usize::from(pos.col))?;
        if !cell.is_spacer() && !cell.is_pad() {
            return url::is_url_char(cell.ch).then_some((pos, cell.ch));
        }
    }
}

/// The character after `pos`, as [`Lines::next_char`] finds it; otherwise
/// as [`prev_url_char`].
fn next_url_char<'a>(lines: &Lines<'a>, row: &mut &'a Line, pos: Pos) -> Option<(Pos, char)> {
    let mut pos = Pos {
        col: pos.col + 1,
        ..pos
    };
    loop {
        if pos.col >= lines.cols {
            if !row.wrapped {
                return None;
            }
            *row = lines.line(pos.line + 1)?;
            pos = Pos {
                line: pos.line + 1,
                col: 0,
            };
        }
        let cell = row.cells.get(usize::from(pos.col))?;
        if !cell.is_spacer() && !cell.is_pad() {
            return url::is_url_char(cell.ch).then_some((pos, cell.ch));
        }
        pos.col += 1;
    }
}

/// The run of URL characters of the logical line around `at`, one base
/// scalar per character cell, with the cell each starts at: at most
/// `reach` characters to either side of `at`'s, and `at`'s index in them.
/// `None` on padding, outside the grid, on a cell whose [`url_char`] is no
/// URL character, and when no scheme's `:` can be near `at` (none from the
/// run's start to [`url::COLON_REACH`] characters past `at`): then no URL
/// contains `at`, and the walk back over the run is all the lookup reads.
///
/// Only `at`'s cell is checked for a hidden URL end here. Another cell
/// that hides one is no URL character either, so the run of [`url_char`]s
/// is this run cut at that cell: the `:` check over base scalars sees
/// every `:` it would, and [`link_with_reach`] deals with the cut.
fn window(lines: &Lines<'_>, at: Pos, reach: usize) -> Option<(Vec<char>, Vec<Pos>, usize)> {
    let at = lines.char_at(at)?;
    let line = lines.line(at.line)?;
    let here = url_char(line, at.col, lines.cell(at)?);
    if !url::is_url_char(here) {
        return None;
    }
    // Back to the run's start, noting a `:`.
    let (mut cells, mut chars) = (Vec::new(), Vec::new());
    let (mut pos, mut row, mut colon) = (at, line, here == ':');
    while cells.len() < reach {
        let Some((prev, c)) = prev_url_char(lines, &mut row, pos) else {
            break;
        };
        colon |= c == ':';
        cells.push(prev);
        chars.push(c);
        pos = prev;
    }
    let back = cells.len();
    if !colon {
        let (mut pos, mut row) = (at, line);
        let found = (0..url::COLON_REACH).any(|_| match next_url_char(lines, &mut row, pos) {
            Some((next, c)) => {
                pos = next;
                c == ':'
            }
            None => false,
        });
        if !found {
            return None;
        }
    }
    cells.reverse();
    chars.reverse();
    cells.push(at);
    chars.push(here);
    let (mut pos, mut row) = (at, line);
    for _ in 0..reach {
        let Some((next, c)) = next_url_char(lines, &mut row, pos) else {
            break;
        };
        cells.push(next);
        chars.push(c);
        pos = next;
    }
    Some((chars, cells, back))
}

/// The link under view-relative `at` at scroll `offset` (see
/// [`crate::App::link_at`]): the OSC 8 hyperlink of the cell when it has
/// one, else the detected URL.
pub(crate) fn link_at(grid: &Grid, offset: usize, at: Point) -> Option<Link> {
    osc8_link(grid, offset, at).or_else(|| link_with_reach(grid, offset, at, MAX_URL_LEN + 1))
}

/// The OSC 8 hyperlink of the cell at view-relative `at`, with every cell
/// of the same link entity ([`tessera_grid::Hyperlink::same_link`]) that
/// the view shows: runs of adjacent columns, row by row, top to bottom.
fn osc8_link(grid: &Grid, offset: usize, at: Point) -> Option<Link> {
    let lines = Lines::of(grid);
    let pos = lines.exact(offset, at)?;
    let link = lines.line(pos.line)?.hyperlink(usize::from(pos.col))?;
    let top = lines.view_top(offset);
    let mut span: Vec<(u16, Range<u16>)> = Vec::new();
    for row in 0..lines.rows {
        let Some(shown) = lines.line(top + i64::from(row)) else {
            continue;
        };
        let mut cols: Option<Range<u16>> = None;
        for (col, other) in shown.graphemes.links() {
            if !other.same_link(link) {
                continue;
            }
            let col = u16::try_from(col).unwrap_or(u16::MAX);
            match &mut cols {
                Some(run) if run.end == col => run.end = col + 1,
                _ => {
                    if let Some(run) = cols.replace(col..col + 1) {
                        span.push((row, run));
                    }
                }
            }
        }
        if let Some(run) = cols {
            span.push((row, run));
        }
    }
    Some(Link {
        url: link.uri().to_owned(),
        span,
    })
}

/// [`link_at`], looking `reach` characters to either side: the cap
/// (U9) makes [`MAX_URL_LEN`] + 1 enough to give what the whole logical
/// line would.
///
/// The window holds base scalars, and a cell that hides a URL end
/// ([`url_char`]) ends the run of URL characters there, which can change
/// the answer. The lookup checks the cells outside the span it found here,
/// and [`link_of`] those inside as it reads their clusters anyway: a
/// cluster cell costs one more lookup only outside the link. When it found
/// none, it checks only the cells that could give the point a URL
/// ([`hidden_end_can_link`]). When a cell hides an end, the lookup runs
/// again on every cell's [`url_char`], which gives the answer the whole
/// logical line of [`url_char`]s would.
fn link_with_reach(grid: &Grid, offset: usize, at: Point, reach: usize) -> Option<Link> {
    let lines = Lines::of(grid);
    let pos = lines.exact(offset, at)?;
    let (mut chars, cells, index) = window(&lines, pos, reach)?;
    match url::url_at(&chars, index) {
        Some(span)
            if !hides_url_end(&lines, &cells[..span.start])
                && !hides_url_end(&lines, &cells[span.end..]) =>
        {
            let (link, hidden) = link_of(&lines, offset, &cells[span])?;
            if !hidden {
                return Some(link);
            }
        }
        None if !hidden_end_can_link(&lines, &chars, &cells, index) => return None,
        _ => {}
    }
    for (c, &p) in chars.iter_mut().zip(&cells) {
        if let Some(line) = lines.line(p.line)
            && let Some(cell) = line.cells.get(usize::from(p.col))
        {
            *c = url_char(line, p.col, cell);
        }
    }
    let span = url::url_at(&chars, index)?;
    link_of(&lines, offset, &cells[span]).map(|(link, _)| link)
}

/// The link whose characters start at `cells`: their text (clusters
/// whole, default-ignorable code points left out) and their cells in the
/// view at scroll `offset`, row by row, and whether one of the clusters
/// hides a URL end ([`url_char`]).
///
/// The text leaves out what [`url::is_default_ignorable`] names (ZWNJ,
/// variation selectors, CGJ, tags, fillers; #279): the cells show nothing
/// for them, so the URL that opens is the text the cells show. The cap
/// (U9) has already counted the cells, one character each, a cell whose
/// only scalar is left out included; leaving them out only shortens the
/// text, so the URL stays within [`MAX_URL_LEN`] characters after it.
fn link_of(lines: &Lines<'_>, offset: usize, cells: &[Pos]) -> Option<(Link, bool)> {
    let mut text = String::new();
    let top = lines.view_top(offset);
    let mut rows: Vec<(u16, Range<u16>)> = Vec::new();
    for &p in cells {
        let line = lines.line(p.line)?;
        match line.cluster(usize::from(p.col)) {
            Some(cluster) if lines.cell(p).is_some_and(Cell::is_cluster) => text.push_str(cluster),
            _ => text.push(lines.cell(p)?.ch),
        }
        let row = p.line - top;
        let Ok(row) = u16::try_from(row) else {
            continue;
        };
        if row >= lines.rows {
            continue;
        }
        let end = lines.char_end(p).col + 1;
        match rows.last_mut() {
            Some((last, cols)) if *last == row => cols.end = end,
            _ => rows.push((row, p.col..end)),
        }
    }
    // A hidden end is in a cluster: no base scalar of a URL cell is one.
    // Looked for before the invisible characters go (ZWJ is one of them).
    let hidden = hidden_url_end(&text).is_some();
    url::strip_default_ignorables(&mut text);
    Some((
        Link {
            url: text,
            span: rows,
        },
        hidden,
    ))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use std::sync::Arc;
    use tessera_config::{MAX_SMART_RULES, Precision};
    use tessera_term::Terminal;

    use super::*;

    fn term(rows: u16, cols: u16, bytes: &[u8]) -> Terminal {
        let mut term = Terminal::new(rows, cols);
        term.advance(bytes);
        term
    }

    fn point(row: u16, col: u16) -> Point {
        Point::new(row, col)
    }

    #[test]
    fn a_selection_survives_identical_repaints_and_clears_on_change() {
        let mut term = term(3, 10, b"hello\r\nworld");
        let mut state = SelectionState::default();
        state.begin(
            term.grid(),
            0,
            point(0, 0),
            Granularity::Char,
            false,
            &SmartRules::default(),
        );
        state.extend(term.grid(), 0, point(0, 4));
        // A full repaint of the same text (ED 2 + rewrite) keeps it.
        term.advance(b"\x1b[H\x1b[2Jhello\r\nworld");
        assert!(state.has_selection(term.grid()));
        // A change on an unselected row keeps it too.
        term.advance(b"\x1b[2;1HWORLD");
        assert!(state.has_selection(term.grid()));
        // A change on the selected row, outside the selected cells, clears.
        term.advance(b"\x1b[1;9Hx");
        assert!(!state.has_selection(term.grid()));
    }

    #[test]
    fn eviction_at_the_default_cap_clears() {
        let cap = tessera_grid::DEFAULT_MAX_SCROLLBACK;
        let mut term = term(2, 5, b"");
        for i in 0..=cap {
            term.advance(format!("{}\r\n", i % 10).as_bytes());
        }
        let mut state = SelectionState::default();
        // The oldest history line, selected while scrolled to the top.
        let offset = term.grid().scrollback_len();
        state.begin(
            term.grid(),
            offset,
            point(0, 0),
            Granularity::Line,
            false,
            &SmartRules::default(),
        );
        assert!(state.has_selection(term.grid()));
        term.advance(b"x\r\n");
        assert!(!state.has_selection(term.grid()));
    }

    #[test]
    fn validation_compares_only_damaged_rows_until_the_history_moves() {
        let mut term = term(4, 10, b"a\r\nb\r\nc\r\nd");
        let mut state = SelectionState::default();
        state.select_all(term.grid());
        // Cursor motion on the bottom row: no change, kept.
        term.advance(b"\x1b[4;5H");
        assert!(state.has_selection(term.grid()));
        // A line feed at the bottom pushes `a` into history: every row is
        // compared by its line number, and none changed.
        term.advance(b"\r\n");
        assert!(state.has_selection(term.grid()));
        assert_eq!(state.text(term.grid(), true).as_deref(), Some("a\nb\nc\nd"));
    }

    #[test]
    fn switching_to_the_alternate_screen_clears() {
        let mut term = term(3, 10, b"hello");
        let mut state = SelectionState::default();
        state.begin(
            term.grid(),
            0,
            point(0, 0),
            Granularity::Word,
            false,
            &SmartRules::default(),
        );
        term.advance(b"\x1b[?1049h");
        assert!(!state.has_selection(term.grid()));
    }

    #[test]
    fn osc8_links_win_over_detected_urls_and_cover_the_whole_entity() {
        // The visible text is a URL, the link points elsewhere.
        let term = term(
            4,
            20,
            b"\x1b]8;;https://real.example/\x1b\\https://fake.example/x\x1b]8;;\x1b\\ https://plain.example/",
        );
        let grid = term.grid();
        let link = link_at(grid, 0, point(1, 1)).expect("a link");
        assert_eq!(link.url, "https://real.example/");
        // Row 0 cols 0..20, row 1 cols 0..2 (the wrapped rest of the text).
        assert_eq!(link.span, vec![(0, 0..20), (1, 0..2)]);
        // Hovering any of its cells gives the same link; the detected URL
        // after it is still found by the old path.
        assert_eq!(link_at(grid, 0, point(0, 0)), Some(link));
        let plain = link_at(grid, 0, point(1, 5)).expect("detected URL");
        assert_eq!(plain.url, "https://plain.example/");
        assert_eq!(link_at(grid, 0, point(3, 0)), None);
    }

    #[test]
    fn osc8_link_of_a_non_url_text_and_scheme_is_reported_as_is() {
        let term = term(
            2,
            20,
            b"\x1b]8;;slack://open?id=1\x07click me\x1b]8;;\x07 x",
        );
        let link = link_at(term.grid(), 0, point(0, 2)).expect("a link");
        assert_eq!(link.url, "slack://open?id=1");
        assert_eq!(link.span, vec![(0, 0..8)]);
        assert_eq!(
            link_at(term.grid(), 0, point(0, 9)),
            None,
            "after the close"
        );
    }

    #[test]
    fn an_id_groups_cells_that_are_not_adjacent_and_other_ids_do_not() {
        let term = term(
            3,
            12,
            b"\x1b]8;id=k;https://a.example/\x1b\\ab\x1b]8;;\x1b\\ \x1b]8;id=k;https://a.example/\x1b\\cd\x1b]8;;\x1b\\\r\n\
              \x1b]8;;https://a.example/\x1b\\ef\x1b]8;;\x1b\\",
        );
        let grid = term.grid();
        let link = link_at(grid, 0, point(0, 1)).expect("a link");
        assert_eq!(link.span, vec![(0, 0..2), (0, 3..5)]);
        // The same URI without the id is another entity.
        let other = link_at(grid, 0, point(1, 0)).expect("another link");
        assert_eq!(other.url, "https://a.example/");
        assert_eq!(other.span, vec![(1, 0..2)]);
    }

    #[test]
    fn osc8_spans_follow_the_scroll_offset_and_wide_characters() {
        let mut term = term(
            2,
            6,
            b"\x1b]8;;https://a.example/\x1b\\\xe4\xb8\x96x\x1b]8;;\x1b\\\r\n",
        );
        term.advance(b"b\r\nc");
        // History holds the link row; the view scrolled back by one shows it
        // on row 0, the wide character's both cells included.
        let link = link_at(term.grid(), 1, point(0, 1)).expect("link on the spacer");
        assert_eq!(link.span, vec![(0, 0..3)]);
        assert_eq!(
            link_at(term.grid(), 0, point(0, 0)),
            None,
            "scrolled out of view"
        );
    }

    #[test]
    fn hover_underlines_every_row_of_an_osc8_link() {
        let term = term(
            3,
            4,
            b"\x1b]8;;https://a.example/\x1b\\abcdef\x1b]8;;\x1b\\",
        );
        let mut state = SelectionState::default();
        state.set_hover(Some(point(1, 0)));
        let draw = state.decorations(term.grid(), 0);
        assert_eq!(
            draw.overlays,
            vec![
                Overlay::new(OverlayKind::Underline, 0, 0..4),
                Overlay::new(OverlayKind::Underline, 1, 0..2),
            ]
        );
    }

    #[test]
    fn linked_trailing_spaces_are_not_text() {
        let term = term(2, 10, b"\x1b]8;;https://a.example/\x1b\\ab  \x1b]8;;\x1b\\");
        let mut state = SelectionState::default();
        state.select_all(term.grid());
        assert_eq!(state.text(term.grid(), true).as_deref(), Some("ab"));
    }

    /// The reference link lookup: every character of the whole logical
    /// line, from its first row, no reach limit and no pre-scan.
    fn reference_link(grid: &Grid, at: Point) -> Option<Link> {
        let lines = Lines::of(grid);
        let at = lines.char_at(lines.exact(0, at)?)?;
        let first = Pos {
            line: lines.logical_top(at.line),
            col: 0,
        };
        let mut cells = Vec::new();
        let mut next = lines
            .cell(first)
            .filter(|c| !c.is_spacer() && !c.is_pad())
            .map(|_| first)
            .or_else(|| lines.next_char(first));
        while let Some(pos) = next {
            cells.push(pos);
            next = lines.next_char(pos);
        }
        let index = cells.iter().position(|&p| p == at)?;
        let chars: Vec<char> = cells
            .iter()
            .map(|&p| {
                let line = lines.line(p.line).expect("a line");
                url_char(line, p.col, lines.cell(p).expect("a cell"))
            })
            .collect();
        let span = url::url_at(&chars, index)?;
        let (link, hidden) = link_of(&lines, 0, &cells[span])?;
        assert!(!hidden, "a link holds a hidden URL end: {:?}", link.url);
        Some(link)
    }

    #[test]
    fn a_hover_lookup_reads_a_bounded_window_of_a_huge_line() {
        // One logical line of 2 000 rows of URL characters (no newline)
        // with a `:` at the point: the lookup walks at most the cap to
        // either side.
        let mut bytes = vec![b'a'; 2_000 * 200 - 1];
        bytes[1_000 * 200 + 100] = b':';
        let mut term = Terminal::new(2_000, 200);
        term.advance(&bytes);
        let lines = Lines::of(term.grid());
        let at = Pos {
            line: lines.top + 1_000,
            col: 100,
        };
        let (chars, _, index) = window(&lines, at, MAX_URL_LEN + 1).expect("a character");
        assert_eq!(chars.len(), 2 * (MAX_URL_LEN + 1) + 1);
        assert_eq!(index, MAX_URL_LEN + 1);
        assert_eq!(link_at(term.grid(), 0, point(1_000, 100)), None);
    }

    #[test]
    fn a_hover_with_no_colon_near_builds_no_window() {
        // No `:` in the run before the point or within a scheme's reach
        // after it: no window. A `:` past a space, or seven characters on,
        // is out of reach; six on is in it.
        for (text, at, some) in [
            ("aaaa aaaaaaaaaa", 7, false),
            ("aaaaaaa a:", 3, false),
            ("aaaaaaaaaaa:aaa", 4, false),
            ("aaaaaaaaaa:aaaa", 4, true),
            ("aa:aaaaaaaaaaaa", 9, true),
        ] {
            let term = term(2, 20, text.as_bytes());
            let lines = Lines::of(term.grid());
            let at = Pos {
                line: lines.top,
                col: at,
            };
            assert_eq!(
                window(&lines, at, MAX_URL_LEN + 1).is_some(),
                some,
                "{text} at {}",
                at.col
            );
        }
        // The run stops at characters that are no URL characters.
        let term = term(2, 30, b"x https://a.bc/d y");
        let lines = Lines::of(term.grid());
        let (chars, cells, index) = window(
            &lines,
            Pos {
                line: lines.top,
                col: 9,
            },
            MAX_URL_LEN + 1,
        )
        .expect("a window");
        assert_eq!(chars.iter().collect::<String>(), "https://a.bc/d");
        assert_eq!((cells[0].col, index), (2, 7));
    }

    #[test]
    fn a_zwj_or_bidi_control_in_a_cell_ends_the_url() {
        // The ZWJ joins the `o` before it: that cell ends the URL, so no
        // joiner splices a host the text hides into the link. Bidi
        // controls break no cluster and join none: the terminal drops them,
        // so the link is the text the cells show.
        for (bytes, want) in [
            ("see https://go\u{200d}od.com/ ok", "https://g"),
            (
                "see https://a.com/x\u{202e}moc.live ok",
                "https://a.com/xmoc.live",
            ),
            ("see https://a.com/\u{2067}x ok", "https://a.com/x"),
            // A combining mark stays (an IRI's `é`).
            (
                "see https://caf\u{0065}\u{0301}.fr/ ok",
                "https://cafe\u{301}.fr/",
            ),
        ] {
            let term = term(2, 40, bytes.as_bytes());
            let link = link_at(term.grid(), 0, point(0, 6)).expect("a link");
            assert_eq!(link.url, want, "{bytes:?}");
            assert_eq!(Some(link), reference_link(term.grid(), point(0, 6)));
        }
        // The hover walk reads base scalars: a hidden end in the cell under
        // the pointer, in the link it finds, before it or right after it
        // changes the answer as the cell's [`url_char`] would.
        for (bytes, col, want) in [
            // The cell holding `o` + ZWJ, and the one after it.
            ("see https://go\u{200d}od.com/ ok", 13, None),
            ("see https://go\u{200d}od.com/ ok", 14, None),
            ("see https://go\u{200d}od.com/ ok", 15, None),
            // The letter before the scheme hides an end: no letter to U1.
            ("see a\u{200d}https://b.com ok", 8, Some("https://b.com")),
            ("see ab\u{200d}https://b.com ok", 8, Some("https://b.com")),
            // With the letter, the first scheme is no start and the second
            // one's URL is the link; without it the first one's is.
            (
                "see a\u{200d}https://x.com/https://y.com ok",
                28,
                Some("https://x.com/https://y.com"),
            ),
            (
                "see ahttps://x.com/https://y.com ok",
                28,
                Some("https://y.com"),
            ),
            // The CJK cut cell hides one: the `-` before it is no longer at
            // a CJK cut (U4), the run just ends there.
            (
                "see https://a.jp-\u{65e5}\u{200d} ok",
                6,
                Some("https://a.jp-"),
            ),
            ("see https://a.jp-\u{65e5} ok", 6, Some("https://a.jp")),
            // No URL on the base scalars, a hidden end after the point
            // (#281): on the `-` that U4 strips before a CJK cut, the cell
            // after it ends the run, and the `-` stays.
            (
                "see https://a.jp--\u{200d}-\u{65e5} ok",
                16,
                Some("https://a.jp-"),
            ),
            ("see https://a.jp---\u{65e5} ok", 16, None),
            // A `:` and a hidden end but no scheme: nothing to find.
            ("see 12:30\u{200d}x ok", 5, None),
        ] {
            let term = term(2, 40, bytes.as_bytes());
            let link = link_at(term.grid(), 0, point(0, col));
            assert_eq!(
                link.as_ref().map(|l| l.url.as_str()),
                want,
                "{bytes:?} at {col}"
            );
            assert_eq!(
                link,
                reference_link(term.grid(), point(0, col)),
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn a_hidden_end_after_the_point_can_bring_a_run_under_the_cap() {
        // One logical line: a scheme and more URL characters than the cap
        // (U9: no URL), cut by a cell that hides a URL end 100 cells after
        // the point. The cut run is under the cap, so the point is in a
        // URL; without the joiner it is not.
        for (joiner, want) in [(true, true), (false, false)] {
            let mut text = "https://a.b/".to_owned();
            text.push_str(&"x".repeat(1_000));
            text.push('y');
            if joiner {
                text.push('\u{200d}');
            }
            text.push_str(&"z".repeat(MAX_URL_LEN));
            let term = term(40, 100, text.as_bytes());
            let at = point(9, 12);
            let link = link_at(term.grid(), 0, at);
            assert_eq!(link.is_some(), want, "joiner {joiner}");
            assert_eq!(link, reference_link(term.grid(), at), "joiner {joiner}");
            if let Some(link) = link {
                assert_eq!(link.url.chars().count(), 12 + 1_000);
            }
        }
    }

    #[test]
    fn invisible_characters_are_left_out_of_a_detected_link() {
        // #279: each class probed in the review (ZWNJ, VS16, CGJ, a tag)
        // and the first and last code point of every default-ignorable
        // range, inside a URL. The terminal drops some at print and joins
        // others to the cell before them or gives them a cell: either way
        // the URL that opens is the cells' text without them, and the
        // hover span covers the cells the link shows.
        let mut probes = vec!['\u{200C}', '\u{FE0F}', '\u{034F}', '\u{E0041}', '\u{E0020}'];
        for &(first, last) in &[
            (0x00AD, 0x00AD),
            (0x034F, 0x034F),
            (0x115F, 0x1160),
            (0x17B4, 0x17B5),
            (0x180B, 0x180F),
            (0x200B, 0x200F),
            (0x2060, 0x206F),
            (0x3164, 0x3164),
            (0xFE00, 0xFE0F),
            (0xFEFF, 0xFEFF),
            (0xFFA0, 0xFFA0),
            (0xFFF0, 0xFFF8),
            (0x1_BCA0, 0x1_BCA3),
            (0x1_D173, 0x1_D17A),
            (0xE_0000, 0xE_0FFF),
            (0xE_0100, 0xE_01EF),
        ] {
            probes.extend([first, last].into_iter().filter_map(char::from_u32));
        }
        for c in probes {
            if url::is_hidden_url_end(c) {
                continue; // they end the URL (a_zwj_or_bidi_control...)
            }
            let bytes = format!("see https://a.com/x{c}y ok");
            let term = term(2, 40, bytes.as_bytes());
            let name = format!("U+{:04X}", u32::from(c));
            let link = link_at(term.grid(), 0, point(0, 6)).expect(&name);
            // The Hangul fillers are CJK letters: one with a cell of its own
            // ends a path as any CJK character does (U3); U+1160 joins the
            // `x` cell's cluster.
            let own_cell = term
                .grid()
                .line(0)
                .expect("a row")
                .cells
                .iter()
                .any(|cell| cell.ch == c);
            let want = if own_cell && url::is_cjk(c) {
                "https://a.com/x"
            } else {
                "https://a.com/xy"
            };
            assert_eq!(link.url, want, "{name}");
            assert_eq!(
                Some(&link),
                reference_link(term.grid(), point(0, 6)).as_ref()
            );
            // The span is the cells from the scheme to `y`, and their text
            // without the invisible characters is the URL.
            let row = term.grid().line(0).expect("a row");
            let [(0, cols)] = link.span.as_slice() else {
                panic!("{name}: one row, {:?}", link.span);
            };
            let shown: String = (cols.start..cols.end)
                .filter_map(|col| {
                    let cell = row.cells.get(usize::from(col))?;
                    if cell.is_spacer() || cell.is_pad() {
                        return None;
                    }
                    Some(match row.cluster(usize::from(col)) {
                        Some(cluster) if cell.is_cluster() => cluster.to_owned(),
                        _ => cell.ch.to_string(),
                    })
                })
                .collect::<String>()
                .chars()
                .filter(|&c| !url::is_default_ignorable(c))
                .collect();
            assert_eq!(shown, link.url, "{name}");
            assert_eq!(cols.start, 4, "{name}");
            // The cell that holds it, if one does, hovers the same link.
            for col in cols.clone() {
                if let Some(other) = link_at(term.grid(), 0, point(0, col)) {
                    assert_eq!(other.url, link.url, "{name} at {col}");
                }
            }
        }
    }

    #[test]
    fn alm_never_reaches_a_cluster() {
        // The hidden-end scan skips U+061C's lead byte (#281): the terminal
        // drops ALM at print, alone, after a letter, a mark or a joiner,
        // and in a URL.
        for text in [
            "a\u{061C}b",
            "\u{061C}ab",
            "e\u{301}\u{061C}b",
            "a\u{200d}\u{061C}b",
            "https://a.b/\u{061C}c",
        ] {
            let term = term(2, 30, text.as_bytes());
            let row = term.grid().line(0).expect("a row");
            for col in 0..row.cells.len() {
                let cluster = row.cluster(col).unwrap_or_default();
                assert!(!cluster.contains('\u{061C}'), "{text:?} at {col}");
                assert_ne!(row.cells[col].ch, '\u{061C}', "{text:?} at {col}");
            }
        }
    }

    #[test]
    fn an_alternate_screen_selection_with_no_text_is_none() {
        let mut term = term(3, 10, b"\x1b[?1049h");
        let mut state = SelectionState::default();
        state.select_all(term.grid());
        assert!(state.has_selection(term.grid()));
        assert_eq!(state.text(term.grid(), true), None);
        // With text it copies it; on the primary screen blanks copy as "".
        term.advance(b"ab");
        state.select_all(term.grid());
        assert_eq!(state.text(term.grid(), true).as_deref(), Some("ab"));
        let term = self::term(3, 10, b"");
        state.select_all(term.grid());
        assert_eq!(state.text(term.grid(), true).as_deref(), Some(""));
    }

    #[test]
    fn ed3_then_a_refill_to_the_cap_in_one_look_is_a_clear() {
        // History l1..l3 at its cap of 3, screen s1, s2; a block selection
        // from the newest history line to the screen. ED 3 removes l3, and
        // three more lines refill the history to its cap before the next
        // look: the history at its cap must not read as an eviction (which
        // would trim the start to s1 and keep the selection).
        for block in [false, true] {
            let mut term = term(2, 5, b"l1\r\nl2\r\nl3\r\ns1\r\ns2");
            term.set_max_scrollback(3);
            let mut state = SelectionState::default();
            state.begin(
                term.grid(),
                1,
                point(0, 0),
                Granularity::Char,
                block,
                &SmartRules::default(),
            );
            state.extend(term.grid(), 0, point(1, 1));
            assert_eq!(state.text(term.grid(), true).as_deref(), Some("l3\ns1\ns2"));
            term.advance(b"\x1b[3J\r\nx\r\ny\r\nz");
            assert_eq!(term.grid().scrollback_len(), term.grid().max_scrollback());
            assert!(!state.has_selection(term.grid()), "block {block}");
        }
        // Eviction alone, to the same history, still trims.
        let mut term = term(2, 5, b"l1\r\nl2\r\nl3\r\ns1\r\ns2");
        term.set_max_scrollback(3);
        let mut state = SelectionState::default();
        state.begin(
            term.grid(),
            1,
            point(0, 0),
            Granularity::Char,
            true,
            &SmartRules::default(),
        );
        state.extend(term.grid(), 0, point(1, 1));
        term.advance(b"\r\nx\r\ny\r\nz");
        assert_eq!(state.text(term.grid(), true).as_deref(), Some("s1\ns2"));
    }

    #[test]
    fn select_all_with_no_history_then_continuous_output_clears() {
        // `scrollback: 0`: every line output pushes off the screen is
        // gone. Select All, then output that scrolls every selected line
        // off: no line of it is left.
        let mut term = term(3, 8, b"");
        term.set_max_scrollback(0);
        term.advance(b"l1\r\nl2\r\nl3");
        let mut state = SelectionState::default();
        state.select_all(term.grid());
        assert_eq!(state.text(term.grid(), true).as_deref(), Some("l1\nl2\nl3"));
        term.advance(b"\r\nm1");
        assert_eq!(state.text(term.grid(), true).as_deref(), Some("l2\nl3"));
        term.advance(b"\r\nm2\r\nm3\r\nm4");
        assert!(!state.has_selection(term.grid()));
        assert_eq!(state.text(term.grid(), true), None);
    }

    /// Feeds `steps` one by one, and after each requires Copy to give
    /// nothing or `first` (the adversarial-tester's #191 oracle).
    fn copy_stays(app: &crate::App, id: crate::SessionId, first: &str, steps: &[&str]) {
        for (i, step) in steps.iter().enumerate() {
            app.feed_for_test(id, step.as_bytes()).unwrap();
            let text = app.selection_text(id).unwrap();
            assert!(
                text.as_deref().is_none_or(|text| text == first),
                "after step {i} {step:?}: {text:?}, selected {first:?}"
            );
        }
    }

    #[test]
    fn a_continuation_row_scrolling_in_under_a_wrapped_bottom_row_clears() {
        // adv191_repro.rs: the last selected row is soft-wrapped on the
        // screen's bottom row with no row after it yet; a line feed
        // scrolls one in below it, and text there would keep the blanks
        // after the selection's end.
        let app = crate::App::new(
            crate::Config::default(),
            crate::ConfigPaths::new("/nonexistent/tessera"),
        );
        let id = app.spawn_test_session(crate::PtySize::new(2, 11)).unwrap();
        app.set_max_scrollback_for_test(id, 0).unwrap();
        for c in [
            "\x1b[?69h\x1b[2;4se\u{301}\x1b[1J\x1b7\x1b[?5l👨\u{200d}👩\u{200d}👧\x1b[?6l",
            "w0rd日本\x1b[B",
            "e\u{301}\x1b[62\"p\x1b[7m",
        ] {
            app.feed_for_test(id, c.as_bytes()).unwrap();
        }
        app.select_begin(id, point(1, 5), Granularity::Char, false)
            .unwrap();
        app.select_extend(id, point(2, 0)).unwrap();
        let first = app.selection_text(id).unwrap().expect("selected");
        assert_eq!(first, " 本e\u{301}");
        copy_stays(
            &app,
            id,
            &first,
            &[
                "\x1b[?2026l",
                "\n",
                "\x1b[?5l",
                "\x1b[3'}",
                "ａ",
                "ｂ",
                "a",
                "b",
            ],
        );
    }

    #[test]
    fn select_all_over_a_wrapped_bottom_row_then_a_scroll_clears() {
        // adv191_repro2.rs: Select All whose last row is soft-wrapped on
        // the bottom row; SU scrolls a continuation row in and text lands
        // there.
        let app = crate::App::new(
            crate::Config::default(),
            crate::ConfigPaths::new("/nonexistent/tessera"),
        );
        let id = app.spawn_test_session(crate::PtySize::new(2, 9)).unwrap();
        app.set_max_scrollback_for_test(id, 2).unwrap();
        for c in [
            "\x1b[?1049l",
            "👍🏽ab\x1b[2J日本",
            "\x1b[H👍🏽👍🏽xyz\x1b[2;1H日本",
            "hello  \nhello \t日本\r\n\r\n",
            "👍🏽\t\x1b[S\t日本",
            "xyz\x1b[2J\x1b[?1049l\x1b[K👍🏽\x1b[1;1Hzze\u{301}\x1b[K",
            "\x1b[2;1H",
            "https://ex.com/a(b)xyz👍🏽",
            "\x1b[2J\r\nab\r\n\x1b[1;1Hzz\t\x1b[2J👍🏽",
            "xyz\x1b[2;1H\x1b[K\t\x1b[Hab",
            "\x1bM",
            "ab",
        ] {
            app.feed_for_test(id, c.as_bytes()).unwrap();
        }
        app.scroll_to(id, crate::ScrollTarget::Offset(5)).unwrap();
        app.select_all(id).unwrap();
        let first = app.selection_text(id).unwrap().expect("selected");
        assert_eq!(first, "\n\n  ab\nab");
        copy_stays(
            &app,
            id,
            &first,
            &["\x1b[2;1H", "\x1b[S", "a", "b", "x", "y", "z", "日", "本"],
        );
    }

    #[test]
    fn the_reach_window_agrees_with_the_whole_line_on_a_long_wrapped_line() {
        let mut bytes = Vec::new();
        bytes.extend(std::iter::repeat_n(b'/', 3000));
        bytes.extend(b"https://a.bc/x ");
        bytes.extend(std::iter::repeat_n(b'a', 2000));
        bytes.extend(b" https://d.ef/");
        let term = term(120, 50, &bytes);
        for row in 0..110 {
            for col in [0, 7, 25, 49] {
                assert_eq!(
                    link_at(term.grid(), 0, point(row, col)),
                    reference_link(term.grid(), point(row, col)),
                    "row {row} col {col}"
                );
            }
        }
    }

    /// An App with a fake session of `rows` x `cols` and an offscreen
    /// view attached the way `App::attach_view` attaches one; `None` when
    /// GPU tests are skipped.
    fn app_with_view(rows: u16, cols: u16) -> Option<(crate::App, crate::SessionId)> {
        if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
            return None;
        }
        let app = crate::App::new(
            crate::Config::default(),
            crate::ConfigPaths::new("/nonexistent/tessera"),
        );
        let id = app
            .spawn_test_session(crate::PtySize::new(rows, cols))
            .expect("session");
        let theme = app.view_theme();
        let gpu = tessera_render::Gpu::headless().expect("a Metal adapter");
        let fonts = app.fonts_for(&theme).expect("the configured font");
        let view = crate::view::View::offscreen(
            gpu,
            fonts,
            theme,
            tessera_render::PhysicalSize::new(400, 300),
            1.0,
        )
        .expect("view");
        crate::session::lock(&app.views).insert(id, view);
        // The view would resize the session; keep the fixture's size.
        app.resize(id, crate::PtySize::new(rows, cols))
            .expect("resize");
        Some((app, id))
    }

    fn shown(app: &crate::App, id: crate::SessionId) -> Decorations {
        crate::session::lock(&app.views)[&id]
            .snapshot()
            .decorations()
            .clone()
    }

    #[test]
    fn frames_carry_the_selection_and_hover_and_skip_when_unchanged() {
        use crate::view::{FrameStatus, MIN_FRAME_INTERVAL};
        let Some((app, id)) = app_with_view(4, 30) else {
            return;
        };
        let next = || std::thread::sleep(MIN_FRAME_INTERVAL + std::time::Duration::from_millis(1));
        app.feed_for_test(id, b"see https://a.bc/x ok").unwrap();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Skipped);
        // A selection is drawn, and an unchanged one is skipped.
        app.select_begin(id, point(0, 0), Granularity::Word, false)
            .unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        let selection = Highlight::linear(point(0, 0), point(0, 2));
        assert_eq!(shown(&app, id).selection, Some(selection));
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Skipped);
        // A hover over the URL underlines it (trailing text outside).
        app.set_hover(id, Some(point(0, 8))).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(
            shown(&app, id).overlays,
            vec![Overlay::new(OverlayKind::Underline, 0, 4..18)]
        );
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Skipped);
        // Moving the hover within the same link draws nothing new.
        app.set_hover(id, Some(point(0, 10))).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Skipped);
        // Off the link, and clearing the selection, each draw a frame.
        app.set_hover(id, Some(point(0, 1))).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(shown(&app, id).overlays, Vec::new());
        app.select_clear(id).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(shown(&app, id), Decorations::default());
        // The frame clips a selection to the rows in view: scrolled back
        // over history, the selection on the live screen is outside it.
        for _ in 0..6 {
            app.feed_for_test(id, b"\r\nline").unwrap();
        }
        app.select_begin(id, point(1, 0), Granularity::Line, false)
            .unwrap();
        app.scroll_to(id, crate::ScrollTarget::Offset(4)).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(shown(&app, id).selection, None);
        assert!(app.has_selection(id).unwrap());
        app.scroll_to(id, crate::ScrollTarget::Offset(2)).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(
            shown(&app, id).selection,
            Some(Highlight::linear(point(3, 0), point(3, 29)))
        );
    }

    /// One step of the differential test below.
    #[derive(Debug, Clone)]
    enum Op {
        /// Output.
        Feed(String),
        /// A frame of the view at this scroll offset.
        Frame(usize),
        /// A look outside a frame (`has_selection`).
        Look,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => prop_oneof![
                "[a-c ]{1,3}",
                Just("\r\n".to_owned()),
                (1u16..6, 1u16..9).prop_map(|(r, c)| format!("\x1b[{r};{c}H")),
                Just("\x1b[1T".to_owned()),
                Just("\x1b[2;4r\x1b[4H\n\x1b[r".to_owned()),
                Just("\x1b[H\x1b[2J".to_owned()),
                // ED 2 alone: it unwraps the newest history line.
                Just("\x1b[2J".to_owned()),
                Just("\x1b[2Jab".to_owned()),
                Just("\x1b[3J".to_owned()),
                // ED 3, then lines that may refill the history to its cap
                // before the next look (#225: a clear, not an eviction).
                Just("\x1b[3J\x1b[5;1H\n\n\n\n\n\n".to_owned()),
                // A rewrite of the top row, then lines that scroll it off.
                Just("\x1b[1;1Hzz\x1b[5;1H\n\n".to_owned()),
                // From the #30 critic's scratch test: RIS, the alternate
                // screen both ways, ED 0, a scroll region at the top, RI at
                // the top, a line that wraps, rewrites that scroll off.
                Just("\x1bc".to_owned()),
                Just("\x1b[?1049h".to_owned()),
                Just("\x1b[?1049l".to_owned()),
                Just("\x1b[H\x1b[J".to_owned()),
                Just("\x1b[1;3r\x1b[3H\n\x1b[r".to_owned()),
                Just("\x1b[1;1H\x1bM".to_owned()),
                Just("abcabcabcab".to_owned()),
                Just("\x1b[2;1Hzz\x1b[5;1H\n".to_owned()),
                Just("\x1b[5;8Hq\x1b[1;1Hy\x1b[5;1H\n\n\n".to_owned()),
                // A blank over the first cell of a row (it may hold the only
                // text after a wrapped row's trailing blanks).
                (1u16..6).prop_map(|r| format!("\x1b[{r};1H ")),
            ]
            .prop_map(Op::Feed),
            2 => (0usize..4).prop_map(Op::Frame),
            1 => Just(Op::Look),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4_000))]

        /// The frame path (only rows the capture found changed, damage
        /// outside the view) and the look path (damage) clear the selection
        /// exactly when a full comparison of every guarded row would, with
        /// the history at a random cap, for a linear or a block selection;
        /// and a selection that is kept copies
        /// the text it copied when it was made (or, after eviction trimmed
        /// it, the end of that text).
        #[test]
        fn validation_agrees_with_a_full_comparison(
            // Lines of up to a row and a half (8 columns): some soft-wrap,
            // and enough of them scroll wrapped lines into the history. A
            // scroll down (SD) can leave a wrapped row at the bottom with
            // its continuation gone: an open last line.
            setup in proptest::collection::vec(
                prop_oneof![
                    4 => "[a-c ]{1,12}",
                    4 => Just("\r\n".to_owned()),
                    1 => Just("\x1b[T".to_owned()),
                ],
                0..16,
            ),
            from in (0u16..5, 0u16..8),
            to in (0u16..5, 0u16..8),
            offset in 0usize..3,
            cap in prop_oneof![Just(1_000usize), 0usize..6],
            block in proptest::bool::weighted(0.25),
            ops in proptest::collection::vec(op(), 1..30),
        ) {
            let mut term = Terminal::new(5, 8);
            term.set_max_scrollback(cap);
            term.advance(setup.concat().as_bytes());
            let mut state = SelectionState::default();
            let grid = term.grid();
            state.begin(grid, offset, point(from.0, from.1), Granularity::Char, block, &SmartRules::default());
            state.extend(grid, offset, point(to.0, to.1));
            let sel = state.sel.clone().expect("selected");
            let original = extract(&Lines::of(grid), &sel);
            // The reference: the rows whose change can change the text, as
            // they were, by line: the selected live rows, the newest
            // history line if selected, and the rest of the last selected
            // row's logical line.
            // If that logical line is still open (its last row is the
            // bottom row and wrapped), any row added below it may continue
            // it: the selection dies when one is.
            let (reference, last, open): (Vec<(i64, Line)>, i64, bool) = {
                let lines = Lines::of(grid);
                let mut last = sel.end.line;
                while last < lines.last() && lines.line(last).is_some_and(|l| l.wrapped) {
                    last += 1;
                }
                let open = last == lines.last() && lines.line(last).is_some_and(|l| l.wrapped);
                let rows = (sel.start.line..=last)
                    .filter(|&n| n >= lines.top - 1)
                    .filter_map(|n| Some((n, lines.line(n)?.clone())))
                    .collect();
                (rows, last, open)
            };
            // `kept` is the first line still selected: eviction trims it.
            let (start, end) = (sel.start.line, sel.end.line);
            let mut kept = start;
            let mut clears = term.grid().history_clear_generation();
            let mut snapshot = crate::view::Snapshot::default();
            let mut alive = true;
            let mut trimmed = false;
            for op in ops {
                let checked = match op {
                    Op::Feed(bytes) => {
                        // The clear generation counts ED 3 and RIS, and
                        // nothing else in this alphabet.
                        let before = term.grid().history_clear_generation();
                        term.advance(bytes.as_bytes());
                        let clearing = bytes.matches("\x1b[3J").count() + bytes.matches("\x1bc").count();
                        prop_assert_eq!(term.grid().history_clear_generation() - before, clearing as u64);
                        false
                    }
                    Op::Frame(offset) => {
                        let changed = snapshot.capture(&term, offset);
                        state.frame_decorations(term.grid(), offset, &changed);
                        true
                    }
                    Op::Look => {
                        state.has_selection(term.grid());
                        true
                    }
                };
                if !checked {
                    continue;
                }
                // Lines the history lost since the previous look: cleared
                // (the clear generation moved: the selection dies if it held
                // one) or else evicted at the cap (cut off, the selection
                // lives while any line of it is left).
                let grid = term.grid();
                let cleared = grid.history_clear_generation() != clears;
                clears = grid.history_clear_generation();
                let lines = Lines::of(grid);
                trimmed = trimmed || start < lines.first;
                alive = alive
                    && !grid.is_alt_screen()
                    && if cleared {
                        kept >= lines.first
                    } else {
                        end >= lines.first
                    }
                    && !(open && lines.last() > last);
                kept = kept.max(lines.first);
                alive = alive
                    && reference.iter().all(|(n, line)| {
                        *n < lines.first
                            || lines.line(*n).is_some_and(|now| {
                                now.cells == line.cells
                                    && now.graphemes == line.graphemes
                                    && now.wrapped == line.wrapped
                            })
                    });
                prop_assert_eq!(state.sel.is_some(), alive);
                if let Some(sel) = state.sel.as_ref().filter(|sel| sel.active) {
                    let text = extract(&lines, sel);
                    if trimmed {
                        prop_assert!(original.ends_with(&text), "{:?} is not the end of {:?}", text, original);
                    } else {
                        prop_assert_eq!(&text, &original);
                    }
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(300))]

        /// The windowed lookup agrees with the whole logical line, and
        /// every span is within the grid and the link text starts with a
        /// scheme.
        #[test]
        fn windowed_links_agree_with_the_reference(
            parts in proptest::collection::vec(
                prop_oneof![
                    Just("https://".to_owned()),
                    Just("mailto:".to_owned()),
                    "[a-c@./()\\[\\] \u{4e16}\u{301}\u{200d}\u{200c}\u{fe0f}\u{3164}:-]{0,5}",
                    Just("\r\n".to_owned()),
                ],
                0..10,
            ),
            cols in 3u16..9,
        ) {
            let term = term(6, cols, parts.concat().as_bytes());
            for row in 0..6 {
                for col in 0..cols {
                    let at = point(row, col);
                    let full = link_at(term.grid(), 0, at);
                    prop_assert_eq!(&full, &reference_link(term.grid(), at));
                    if let Some(link) = full {
                        prop_assert!(!link.url.chars().any(url::is_default_ignorable));
                        prop_assert!(link.url.to_ascii_lowercase().starts_with("https://")
                            || link.url.to_ascii_lowercase().starts_with("mailto:"));
                        for (r, c) in &link.span {
                            prop_assert!(*r < 6 && c.start < c.end && c.end <= cols);
                        }
                    }
                }
            }
        }
    }

    /// #351 r2 n2: a window edge on a blank where the line goes on. A rule
    /// that matches blanks (no default rule starts or ends with one) may
    /// continue past that blank, so a match taking it is dropped, at the
    /// start edge and at the end edge alike.
    #[test]
    fn a_match_taking_a_blank_at_a_window_edge_is_dropped() {
        let rules = smart_rules(&[("[ x]+", Precision::Normal)]);
        let check = |text: &str, click: usize, inside: &str| {
            let mut term = Terminal::new(2, 100);
            term.set_max_scrollback(1_000);
            term.advance(text.as_bytes());
            let lines = Lines::of(term.grid());
            let at = Pos {
                line: lines.base + i64::try_from(click / 100).unwrap(),
                col: u16::try_from(click % 100).unwrap(),
            };
            let (_, last) = lines.text_end(at.line);
            let window = ScanWindow::around(&lines, at, last.unwrap());
            assert!(!(window.line_start && window.line_end), "{inside}: an edge");
            assert_eq!(smart_at(&lines, at, &rules), None, "{inside}");
        };
        // Start edge: the window's first character is a blank of the run
        // before "xx" (5100 - 4096 = 1004).
        check(
            &format!("{}{}xx", "b".repeat(100), " ".repeat(5_000)),
            5_100,
            "start",
        );
        // End edge: the window's last character (1 + 4095) is a blank.
        check(&format!("xx{}b", " ".repeat(9_000)), 1, "end");
        // Wholly inside the window the same match is found.
        let mut term = Terminal::new(2, 100);
        term.advance(b"b   xx b");
        let lines = Lines::of(term.grid());
        let at = Pos {
            line: lines.base,
            col: 4,
        };
        assert!(smart_at(&lines, at, &rules).is_some());
    }

    /// Rules made from `(pattern, precision)` pairs.
    fn smart_rules(rules: &[(&str, Precision)]) -> Arc<SmartRules> {
        let mut selection = tessera_config::Selection::default();
        selection.smart_rules = Some(
            rules
                .iter()
                .map(|&(regex, precision)| tessera_config::SmartRule::new(regex, precision))
                .collect(),
        );
        selection.compiled_rules()
    }

    /// #351 m1: once the rules of a click have spent [`SMART_RULE_BUDGET`],
    /// the rules not started are skipped and the best match of those that
    /// ran wins; the first rule always runs. The cost is charged through
    /// the test hook, so the test does not depend on the machine's speed.
    #[test]
    fn rules_past_the_click_budget_are_skipped() {
        let rules = smart_rules(&[
            ("[0-9]+", Precision::Normal),
            ("q+", Precision::Normal),
            ("/tmp/[a-z]+", Precision::VeryHigh),
        ]);
        let mut term = Terminal::new(2, 40);
        term.advance(b"see /tmp/xyz ok");
        let lines = Lines::of(term.grid());
        let at = Pos {
            line: lines.base,
            col: 6,
        };
        let path = Some((
            Pos {
                line: lines.base,
                col: 4,
            },
            Pos {
                line: lines.base,
                col: 11,
            },
        ));
        let run = |cost: Duration| {
            RULE_COST.with(|c| c.set(cost));
            let found = smart_at(&lines, at, &rules);
            RULE_COST.with(|c| c.set(Duration::ZERO));
            (found, RULES_RUN.with(std::cell::Cell::get))
        };
        assert_eq!(run(Duration::ZERO), (path, 3), "within the budget");
        assert_eq!(
            run(SMART_RULE_BUDGET / 3),
            (path, 3),
            "spent after the third"
        );
        assert_eq!(
            run(SMART_RULE_BUDGET / 2),
            (None, 2),
            "spent after the second"
        );
        assert_eq!(
            run(SMART_RULE_BUDGET * 10),
            (None, 1),
            "the first always runs"
        );
        // The best of the rules that ran still wins.
        let rules = smart_rules(&[
            ("/tmp/[a-z]+", Precision::Normal),
            ("[a-z]+", Precision::Normal),
        ]);
        RULE_COST.with(|c| c.set(SMART_RULE_BUDGET));
        assert_eq!(smart_at(&lines, at, &rules), path);
        RULE_COST.with(|c| c.set(Duration::ZERO));
        assert_eq!(RULES_RUN.with(std::cell::Cell::get), 1);
    }

    /// #351 m1 in real time: 64 slow user rules (`[a-z]+X|[a-z]`, ~425 ms
    /// a click on 8192 letters in a release build before the budget) stop
    /// near [`SMART_RULE_BUDGET`]. The budget is wall time, so load makes
    /// fewer rules run, not more; the bound is loose (500 ms, which one
    /// rule run late under load still meets) and the fastest of five
    /// clicks counts, so one descheduled click does not fail it.
    #[test]
    fn sixty_four_slow_rules_stay_near_the_click_budget() {
        let slow = vec![("[a-z]+X|[a-z]", Precision::Normal); MAX_SMART_RULES];
        let rules = smart_rules(&slow);
        assert_eq!(rules.len(), MAX_SMART_RULES);
        let mut term = Terminal::new(2, 200);
        term.set_max_scrollback(1_000);
        term.advance("a".repeat(20_000).as_bytes());
        let lines = Lines::of(term.grid());
        let mid = Pos {
            line: lines.base + 50,
            col: 100,
        };
        let mut fastest = Duration::MAX;
        let mut ran = MAX_SMART_RULES;
        for _ in 0..5 {
            let started = Instant::now();
            let _ = smart_at(&lines, mid, &rules);
            let took = started.elapsed();
            if took < fastest {
                fastest = took;
                ran = RULES_RUN.with(std::cell::Cell::get);
            }
        }
        assert!(
            fastest < Duration::from_millis(500),
            "fastest click took {fastest:?} ({ran} rules ran)"
        );
        assert!(ran < MAX_SMART_RULES, "{ran} rules ran in {fastest:?}");
    }

    /// SS-C5: a match running past the scan window where its logical line
    /// goes on is no candidate (the word is selected); a match inside the
    /// window is found; the window holds at most [`SMART_SCAN_LEN`]
    /// characters.
    #[test]
    fn the_scan_window_is_bounded_and_cut_matches_are_dropped() {
        let rules = tessera_config::Selection::default().compiled_rules();
        let mut term = Terminal::new(2, 200);
        term.set_max_scrollback(1_000);
        let line = "a/".repeat(50_000);
        term.advance(line.as_bytes());
        let lines = Lines::of(term.grid());
        let mid = Pos {
            line: lines.base + 250,
            col: 100,
        };
        let (_, last) = lines.text_end(mid.line);
        let window = ScanWindow::around(&lines, mid, last.unwrap());
        assert_eq!(window.cells.len(), SMART_SCAN_LEN);
        assert_eq!(window.text.len(), SMART_SCAN_LEN);
        assert_eq!(window.click, 4096, "4096 characters before the click");
        assert!(!window.line_start && !window.line_end);
        assert_eq!(smart_at(&lines, mid, &rules), None);
        // Near the line's start the window starts with it, but the match
        // still runs past its end.
        let near = Pos {
            line: lines.base,
            col: 10,
        };
        let window = ScanWindow::around(&lines, near, last.unwrap());
        assert!(window.line_start && !window.line_end);
        assert_eq!(window.cells.len(), 10 + SMART_SCAN_LEN - SMART_SCAN_BEFORE);
        assert_eq!(smart_at(&lines, near, &rules), None);
        // A double-click there selects the word: the whole line.
        let mut state = SelectionState::default();
        state.begin(
            term.grid(),
            249,
            point(0, 100),
            Granularity::Word,
            false,
            &rules,
        );
        assert_eq!(
            state.text(term.grid(), true).as_deref(),
            Some(line.as_str())
        );

        // A path inside the window of a long line is found.
        let mut term = Terminal::new(2, 200);
        term.set_max_scrollback(1_000);
        let text = format!("{}/usr/lib/x {}", "a ".repeat(3_000), "a ".repeat(3_000));
        term.advance(text.as_bytes());
        let lines = Lines::of(term.grid());
        let lib = Pos {
            line: lines.base + 30,
            col: 5,
        };
        assert_eq!(
            smart_at(&lines, lib, &rules),
            Some((
                Pos {
                    line: lines.base + 30,
                    col: 0
                },
                Pos {
                    line: lines.base + 30,
                    col: 9
                }
            ))
        );
    }

    /// U5 on a URL of closers only strips them in one pass: a hostile run
    /// costs the run's length, not its square.
    #[test]
    fn url_trimming_is_linear() {
        let url = format!("https://a.test/x(y){}.,", ")".repeat(8_000));
        assert_eq!(trim_url(&url, 0..url.len()), 0.."https://a.test/x(y)".len());
        let url = "https://a.test/[x]]";
        assert_eq!(trim_url(url, 0..url.len()), 0..url.len() - 1);
        let url = "https://a.test/x?";
        assert_eq!(trim_url(url, 0..url.len()), 0..url.len() - 1);
    }

    /// The IPv6 post-checks drop matches that a look-around would have.
    #[test]
    fn ipv6_checks_drop_partial_matches() {
        let text = "fe80::1g";
        assert_eq!(
            checked(RuleCheck::NotBeforeHexOrColon, text, 0..7),
            Some(0..7)
        );
        assert_eq!(
            checked(RuleCheck::NotBeforeHexOrColon, "fe80::1a", 0..7),
            None
        );
        assert_eq!(
            checked(RuleCheck::NotBeforeHexOrColon, "fe80::1:", 0..7),
            None
        );
        assert_eq!(checked(RuleCheck::NotAfterAlnumOrColon, "x::1", 1..4), None);
        assert_eq!(checked(RuleCheck::NotAfterAlnumOrColon, ":::1", 1..4), None);
        assert_eq!(
            checked(RuleCheck::NotAfterAlnumOrColon, " ::1", 1..4),
            Some(1..4)
        );
        assert_eq!(checked(RuleCheck::None, "x", 0..1), Some(0..1));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1_000))]

        /// The walker with trimming gives exactly the text the reference
        /// `extract` does (Copy with `trim_trailing_whitespace = true` is
        /// unchanged), and without trimming it gives that text plus blanks:
        /// removing the blanks after each line's text gives it back.
        #[test]
        fn the_walker_agrees_with_the_reference(
            parts in proptest::collection::vec(
                prop_oneof![
                    4 => "[ab ]{1,9}",
                    1 => Just("\u{65e5}".to_owned()),
                    1 => Just("e\u{301}".to_owned()),
                    1 => Just("\x1b[41m  \x1b[0m".to_owned()),
                    2 => Just("\r\n".to_owned()),
                ],
                0..14,
            ),
            from in (0u16..5, 0u16..7),
            to in (0u16..5, 0u16..7),
            offset in 0usize..3,
            block in proptest::bool::weighted(0.25),
            all in proptest::bool::weighted(0.1),
        ) {
            let mut term = Terminal::new(5, 7);
            term.advance(parts.concat().as_bytes());
            let grid = term.grid();
            let mut state = SelectionState::default();
            if all {
                state.select_all(grid);
            } else {
                state.begin(grid, offset, point(from.0, from.1), Granularity::Char, block, &SmartRules::default());
                state.extend(grid, offset, point(to.0, to.1));
            }
            let sel = state.sel.clone().expect("selected");
            let lines = Lines::of(grid);
            let reference = extract(&lines, &sel);
            prop_assert_eq!(&plain(&lines, &sel, true), &reference);
            let padded = plain(&lines, &sel, false);
            prop_assert!(padded.len() >= reference.len());
            if !sel.block {
                let strip = |text: &str| {
                    text.split('\n').map(|l| l.trim_end_matches(' ')).collect::<Vec<_>>().join("\n")
                };
                prop_assert_eq!(strip(&padded), strip(&reference));
            }
            // The styled text holds the same characters as the plain one.
            let colors = ThemeColors::default();
            let (text, _) = state
                .styled_text(grid, false, &colors, crate::MAX_RICH_COPY_LEN)
                .expect("selected");
            prop_assert_eq!(text, padded);
        }
    }
}
