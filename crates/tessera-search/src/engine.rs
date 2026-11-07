//! The matcher: compiled queries, the text of a run of rows and the
//! mapping of regex matches back to grid cells (ADR 0038).
//!
//! The search unit is the logical line: the rows a soft wrap joins
//! ([`Line::wrapped`]). A logical line of up to [`segment_period`] rows is
//! searched whole. A longer one is searched in pieces (segments): it is cut
//! at each absolute row number that is a multiple of the period and at
//! least a period below the line's first row, so a piece holds fewer than
//! two periods of rows and the cuts do not move as output arrives. A match
//! that would cross a cut is not found.
//!
//! A segment whose text is longer than [`MAX_HAYSTACK_BYTES`] (multi-byte
//! characters, combining marks) is searched in pieces of at most that many
//! bytes, cut between cells where the cap falls; a match that would cross
//! such a cut is not found either.

use std::fmt;
use std::ops::ControlFlow;

use regex::{Regex, RegexBuilder};
use tessera_grid::{Cell, CellFlags, Line};

/// Longest query accepted, in bytes of UTF-8 (security rule S5): a longer
/// one is refused with [`QueryError::TooLong`], never truncated.
pub const MAX_QUERY_BYTES: usize = 1024;

/// Most heap the compiled program of one query may take, in bytes
/// ([`RegexBuilder::size_limit`]). A query whose program would be larger
/// is refused with [`QueryError::TooComplex`]. 10 MiB lets a Unicode
/// `\w{209}` compile; `\w{210}` and `a{1000}{1000}` are refused.
pub const REGEX_SIZE_LIMIT: usize = 10 << 20;

/// Most heap the lazy DFA's cache of one query may take, in bytes
/// ([`RegexBuilder::dfa_size_limit`]). Past it the regex crate falls back
/// to a slower engine, not to failure.
pub const DFA_SIZE_LIMIT: usize = 4 << 20;

/// Most bytes of text one regex run searches. [`Haystack::fill`] leaves
/// text past it out; [`scan_segment`] searches a segment with more text in
/// pieces of at most this many bytes, each cut between cells, and a match
/// that would cross from one piece to the next is not found. The cap
/// bounds the text buffer and the regex crate's quadratic worst case for
/// iterating matches (`O(m * n^2)` for a haystack of `n` bytes).
pub const MAX_HAYSTACK_BYTES: usize = 64 << 10;

/// The longest [`segment_period`], in rows.
pub const MAX_SEGMENT_PERIOD: usize = 128;

/// Most rows of one segment, whatever the width: just under two periods.
pub const MAX_SEGMENT_ROWS: usize = 2 * MAX_SEGMENT_PERIOD - 1;

/// The text a segment (or a 64 KiB piece of one) starts with when it
/// continues a logical line from above, and ends with when the line goes on
/// past it. No cell holds NUL, so `^` and `$` do not match at a cut.
///
/// What the regex sees at a cut is the marker, not the neighbouring cell:
/// NUL is a non-word character, so `\b` is true and `\B` false at every
/// cut, whatever the cells on either side hold. A pattern with `\b` can
/// then report false matches at a cut (`\be` matches an `e` that follows a
/// letter across the cut), not only miss them. A match that would straddle
/// a cut is not found: it would have to match the marker itself, and a
/// match reaching into the trailing marker is dropped.
const CUT: char = '\0';

/// How a query reads its pattern.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct QueryOptions {
    /// Read the pattern as a Rust regex (`regex` crate syntax: no
    /// look-around, no backreferences). When false the pattern is a
    /// literal string.
    pub regex: bool,
    /// Match case exactly. When false, letters match under Unicode simple
    /// case folding. A regex's inline `(?i)` and `(?-i)` win over this.
    pub case_sensitive: bool,
}

/// Why a query was refused.
///
/// The message of [`QueryError::Syntax`] quotes the pattern, which may be
/// anything the user typed: show it to the user, never log it (security
/// rule S8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The pattern is longer than [`MAX_QUERY_BYTES`] bytes.
    TooLong {
        /// The pattern's length in bytes.
        bytes: usize,
    },
    /// The pattern is not a valid regex; the message says why.
    Syntax(String),
    /// The pattern is valid but its compiled program would exceed
    /// [`REGEX_SIZE_LIMIT`] (for example a large counted repetition of a
    /// Unicode class).
    TooComplex,
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { bytes } => write!(
                f,
                "the query is {bytes} bytes long; at most {MAX_QUERY_BYTES} are searched"
            ),
            Self::Syntax(message) => f.write_str(message),
            Self::TooComplex => f.write_str(
                "the pattern is too complex to search; a smaller repetition count \
                 or ASCII classes such as (?-u:\\w) compile smaller",
            ),
        }
    }
}

impl std::error::Error for QueryError {}

/// A compiled search query.
#[derive(Debug, Clone)]
pub struct Query {
    regex: Regex,
    options: QueryOptions,
}

impl Query {
    /// Compiles `pattern`. An empty pattern is no query: `Ok(None)`, the
    /// search is idle.
    ///
    /// # Errors
    ///
    /// [`QueryError::TooLong`] for a pattern over [`MAX_QUERY_BYTES`]
    /// bytes, [`QueryError::Syntax`] for an invalid regex and
    /// [`QueryError::TooComplex`] for one over [`REGEX_SIZE_LIMIT`]. A
    /// literal pattern is never a syntax error.
    pub fn new(pattern: &str, options: QueryOptions) -> Result<Option<Self>, QueryError> {
        if pattern.len() > MAX_QUERY_BYTES {
            return Err(QueryError::TooLong {
                bytes: pattern.len(),
            });
        }
        if pattern.is_empty() {
            return Ok(None);
        }
        let escaped;
        let source = if options.regex {
            pattern
        } else {
            escaped = regex::escape(pattern);
            &escaped
        };
        let regex = RegexBuilder::new(source)
            .case_insensitive(!options.case_sensitive)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(DFA_SIZE_LIMIT)
            .build()
            .map_err(|error| match error {
                regex::Error::CompiledTooBig(_) => QueryError::TooComplex,
                other => QueryError::Syntax(other.to_string()),
            })?;
        Ok(Some(Self { regex, options }))
    }

    /// The options the query was compiled with.
    pub fn options(&self) -> QueryOptions {
        self.options
    }

    /// Calls `found` with each match in `haystack`, in order, until it
    /// returns [`ControlFlow::Break`].
    ///
    /// Matches are the regex crate's leftmost-first, non-overlapping
    /// matches with three changes: an empty match is skipped; a match is
    /// widened to whole cells (a boundary inside a grapheme cluster takes
    /// the cluster's cell); and a match that, widened, starts before the
    /// previous one ends is dropped.
    pub fn find(
        &self,
        haystack: &Haystack,
        mut found: impl FnMut(SegmentMatch) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        let text = haystack.text.as_str();
        let mut at = haystack.start;
        let mut previous_end: Option<(u32, u16)> = None;
        while at <= haystack.end {
            let Some(m) = self.regex.find_at(text, at) else {
                break;
            };
            if m.end() > haystack.end {
                // The match reaches into the cut marker or past the
                // searched text: so does every later one.
                break;
            }
            if m.is_empty() {
                at = m.end() + text[m.end()..].chars().next().map_or(1, char::len_utf8);
                continue;
            }
            at = m.end();
            let first = haystack.cell_at(m.start());
            let last = haystack.cell_at(m.end() - 1);
            let start = (first.row, first.col);
            if previous_end.is_some_and(|end| start < end) {
                continue;
            }
            let end = (last.row, last.col + last.width);
            previous_end = Some(end);
            found(SegmentMatch {
                start_row: first.row,
                start_col: first.col,
                end_row: last.row,
                end_col: end.1,
            })?;
        }
        ControlFlow::Continue(())
    }
}

/// A match within one segment: rows count from the segment's first row,
/// `end_col` is exclusive and on the row of the match's last cell (so a
/// match ending at a row's end has `end_col == cols` on that row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentMatch {
    /// Row of the first cell.
    pub start_row: u32,
    /// Column of the first cell.
    pub start_col: u16,
    /// Row of the last cell.
    pub end_row: u32,
    /// Column after the last cell.
    pub end_col: u16,
}

/// Where one cell's text sits in a [`Haystack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellText {
    /// Offset of the cell's first byte in the text.
    byte: u32,
    /// Row within the segment.
    row: u32,
    /// Column of the cell.
    col: u16,
    /// Columns the cell covers (2 for a wide character).
    width: u16,
}

/// Where the text of a run of cells sits in a [`Haystack`]: one cell, or
/// (`run`) consecutive cells of one row that each hold one ASCII character
/// in one column, so byte `byte + i` is column `col + i`. Plain text then
/// costs one entry per row, not one per cell.
#[derive(Debug, Clone, Copy)]
struct Span {
    /// The first cell.
    cell: CellText,
    /// True for a run of single-byte, single-column cells.
    run: bool,
}

/// True when `cell` is text of one byte in one column: an ASCII character
/// that is not wide, a spacer, padding or the base of a cluster.
fn is_plain(cell: &Cell) -> bool {
    const NOT_PLAIN: u8 = CellFlags::WIDE.bits()
        | CellFlags::SPACER.bits()
        | CellFlags::PAD.bits()
        | CellFlags::CLUSTER.bits();
    cell.ch.is_ascii() && cell.flags.bits() & NOT_PLAIN == 0
}

/// The text of one segment, reused from segment to segment so a scan
/// allocates only while it grows.
///
/// A wide character contributes its character once (its spacer nothing),
/// a cluster cell its whole cluster, the padding a wrapped wide character
/// leaves at a row's end nothing, and a blank cell a space. Blanks at the
/// end of a logical line are not text; blanks at the end of a row that
/// wraps are (a space at a wrap is content). No newline is ever in the
/// text, so no match crosses a hard line break.
#[derive(Debug, Default, Clone)]
pub struct Haystack {
    text: String,
    spans: Vec<Span>,
    /// First byte of searched text (after a leading cut marker).
    start: usize,
    /// End of the searched text (before a trailing cut marker).
    end: usize,
    /// True when text past [`MAX_HAYSTACK_BYTES`] was left out.
    capped: bool,
    /// Segments, kept for [`scan`] to reuse.
    segments: Vec<Segment>,
}

impl Haystack {
    /// An empty haystack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the text with that of `rows`, consecutive rows of one
    /// logical line, top to bottom. `continues_above` says the first row
    /// is not the start of its logical line; `continues_below` that the
    /// last row is not its end (the segment was cut). Text past
    /// [`MAX_HAYSTACK_BYTES`] is left out.
    pub fn fill(&mut self, rows: &[Line], continues_above: bool, continues_below: bool) {
        // The rest of a capped segment is left out, as documented.
        let _ = self.fill_from(rows, (0, 0), continues_above, continues_below);
    }

    /// [`Haystack::fill`] from the cell at `from` (row index in `rows`,
    /// column) on: the cells before it are left out. Returns the position
    /// of the first cell left out at the cap, or `None` when the text
    /// reaches the end of `rows`.
    ///
    /// The first cell always goes in, so a refill from the returned
    /// position makes progress; a cell holds at most
    /// `4 * MAX_CLUSTER_SCALARS` bytes, far below the cap.
    fn fill_from(
        &mut self,
        rows: &[Line],
        from: (usize, usize),
        continues_above: bool,
        continues_below: bool,
    ) -> Option<(usize, usize)> {
        self.text.clear();
        self.spans.clear();
        self.capped = false;
        if continues_above {
            self.text.push(CUT);
        }
        self.start = self.text.len();
        let limit = self.start + MAX_HAYSTACK_BYTES;
        let last_row = rows.len().saturating_sub(1);
        let mut stopped = None;
        'rows: for (row, line) in rows.iter().enumerate().skip(from.0) {
            let cells = &line.cells;
            let end = if row == last_row && !continues_below {
                content_end(line)
            } else {
                cells.len()
            };
            let first_col = if row == from.0 { from.1.min(end) } else { 0 };
            // True while the last span is a run this row can extend.
            let mut open = false;
            for (col, cell) in cells[..end].iter().enumerate().skip(first_col) {
                if is_plain(cell) {
                    let before = self.text.len();
                    if before >= limit {
                        self.capped = true;
                        stopped = Some((row, col));
                        break 'rows;
                    }
                    if !open {
                        self.spans.push(Span {
                            cell: CellText {
                                byte: u32::try_from(before).unwrap_or(u32::MAX),
                                row: u32::try_from(row).unwrap_or(u32::MAX),
                                col: u16::try_from(col).unwrap_or(u16::MAX),
                                width: 1,
                            },
                            run: true,
                        });
                        open = true;
                    }
                    self.text.push(cell.ch);
                    continue;
                }
                open = false;
                if cell.is_spacer() || cell.is_pad() {
                    continue;
                }
                let before = self.text.len();
                let cluster = if cell.is_cluster() {
                    line.cluster(col)
                } else {
                    None
                };
                let bytes = cluster.map_or(cell.ch.len_utf8(), str::len);
                if before + bytes > limit && before > self.start {
                    self.capped = true;
                    stopped = Some((row, col));
                    break 'rows;
                }
                match cluster {
                    Some(text) => self.text.push_str(text),
                    None => self.text.push(cell.ch),
                }
                self.spans.push(Span {
                    cell: CellText {
                        // Bounded by MAX_HAYSTACK_BYTES plus the marker.
                        byte: u32::try_from(before).unwrap_or(u32::MAX),
                        row: u32::try_from(row).unwrap_or(u32::MAX),
                        col: u16::try_from(col).unwrap_or(u16::MAX),
                        width: cell.width().max(1),
                    },
                    run: false,
                });
            }
        }
        self.end = self.text.len();
        if continues_below || self.capped {
            self.text.push(CUT);
        }
        stopped
    }

    /// The searched text, without cut markers.
    pub fn text(&self) -> &str {
        &self.text[self.start..self.end]
    }

    /// True when text past [`MAX_HAYSTACK_BYTES`] was left out.
    pub fn is_capped(&self) -> bool {
        self.capped
    }

    /// The cell whose text holds byte `byte` of the searched text.
    fn cell_at(&self, byte: usize) -> CellText {
        let index = self
            .spans
            .partition_point(|span| span.cell.byte as usize <= byte)
            .saturating_sub(1);
        let span = self.spans[index];
        if !span.run {
            return span.cell;
        }
        // Within a run byte and column advance together.
        let offset = byte.saturating_sub(span.cell.byte as usize);
        CellText {
            byte: u32::try_from(byte).unwrap_or(u32::MAX),
            col: span
                .cell
                .col
                .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
            ..span.cell
        }
    }
}

/// The columns of `line` before its trailing blanks, by the rule of
/// [`Line::text`]: a cell is text unless it is a space whose only flags
/// are padding or a hyperlink.
fn content_end(line: &Line) -> usize {
    line.cells
        .iter()
        .rposition(|cell| {
            cell.ch != ' '
                || !cell
                    .flags
                    .difference(tessera_grid::CellFlags::PAD | tessera_grid::CellFlags::LINK)
                    .is_empty()
        })
        .map_or(0, |i| i + 1)
}

/// The rows between the cuts of a long logical line at `cols` columns: as
/// many as fit half of [`MAX_HAYSTACK_BYTES`] of single-byte text, at least
/// one and at most [`MAX_SEGMENT_PERIOD`]. A segment holds fewer than two
/// periods of rows, so at most [`MAX_HAYSTACK_BYTES`] of such text; a
/// segment of multi-byte text can hold more, and [`scan_segment`] searches
/// it in pieces.
pub fn segment_period(cols: u16) -> usize {
    (MAX_HAYSTACK_BYTES / 2 / usize::from(cols.max(1))).clamp(1, MAX_SEGMENT_PERIOD)
}

/// True when absolute row `row` starts a segment. `run` is the number of
/// rows directly above it that wrap (into it), counted up to at least
/// `period`; 0 when there is no row above. A row starts a segment when it
/// starts its logical line (`run` is 0), or when its line began at least a
/// period above it (`run >= period`) and `row` is a multiple of `period`.
pub fn is_segment_start(row: u64, run: usize, period: usize) -> bool {
    run == 0 || (run >= period && row.is_multiple_of(period as u64))
}

/// A match in absolute rows ([`scan`]): `end_col` is exclusive and on the
/// row of the match's last cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Match {
    /// Absolute row of the first cell.
    pub start_line: u64,
    /// Column of the first cell.
    pub start_col: u16,
    /// Absolute row of the last cell.
    pub end_line: u64,
    /// Column after the last cell.
    pub end_col: u16,
}

/// Where a run of rows handed to [`scan`] sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rows {
    /// Absolute row number of the first row.
    pub first: u64,
    /// How many rows directly above the first wrap (counted up to at
    /// least the period; 0 when there is none): not 0 when the first row
    /// continues a logical line. The first row must start a segment
    /// ([`is_segment_start`]).
    pub run_above: usize,
    /// True when rows below the last exist: a last row that wraps then
    /// continues on them. When false a wrapped last row ends its line.
    pub more_below: bool,
}

/// One segment of a run of rows ([`segments`]): rows `start..end` of the
/// run, searched as one text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Segment {
    /// Index of the segment's first row in the run.
    pub start: usize,
    /// Index after its last row.
    pub end: usize,
    /// The first row continues a logical line from above.
    pub above: bool,
    /// The logical line goes on below the last row.
    pub below: bool,
}

/// Cuts `rows` (where `at` says they sit) into segments, top down, into
/// `out` (cleared first). `period` is [`segment_period`] for the rows'
/// width. The text and matches of a segment depend only on its rows,
/// `above` and `below`: equal rows and flags give equal matches.
pub fn segments(rows: &[Line], at: Rows, period: usize, out: &mut Vec<Segment>) {
    out.clear();
    let mut run = at.run_above;
    for (index, row) in rows.iter().enumerate() {
        if index == 0 || is_segment_start(at.first + index as u64, run, period) {
            if let Some(last) = out.last_mut() {
                last.end = index;
            }
            let above = if index == 0 {
                at.run_above > 0
            } else {
                rows[index - 1].wrapped
            };
            out.push(Segment {
                start: index,
                end: rows.len(),
                above,
                below: false,
            });
        }
        run = if row.wrapped {
            (run + 1).min(period)
        } else {
            0
        };
    }
    for segment in out.iter_mut() {
        segment.below =
            rows[segment.end - 1].wrapped && (segment.end < rows.len() || at.more_below);
    }
}

/// Where the scan of a segment stands between two of its pieces
/// ([`scan_piece`]): the cell the next piece starts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Piece {
    /// Row index within the segment and column of the piece's first cell.
    from: (usize, usize),
    /// The piece starts behind a cut marker.
    above: bool,
}

impl Piece {
    /// The first piece of `segment`.
    pub fn first(segment: Segment) -> Self {
        Self {
            from: (0, 0),
            above: segment.above,
        }
    }
}

/// Searches one piece of `segment` of `rows` (the first of which is
/// absolute row `first`), at most [`MAX_HAYSTACK_BYTES`] of its text from
/// `piece` on, and calls `found` with each match in it, in document order,
/// until it returns [`ControlFlow::Break`]. Returns that and the next
/// piece, `None` when this one reached the segment's end.
///
/// A caller that checks a deadline between calls bounds its overrun by one
/// piece's regex run: a segment of heavy combining marks can hold a hundred
/// pieces, and a quadratic pattern can take seconds on one (the time
/// depends on the pattern and has no fixed ceiling).
pub fn scan_piece(
    query: &Query,
    rows: &[Line],
    first: u64,
    segment: Segment,
    piece: Piece,
    haystack: &mut Haystack,
    mut found: impl FnMut(Match) -> ControlFlow<()>,
) -> (ControlFlow<()>, Option<Piece>) {
    let rows = &rows[segment.start..segment.end];
    let base = first + segment.start as u64;
    let stopped = haystack.fill_from(rows, piece.from, piece.above, segment.below);
    let flow = query.find(haystack, |m| {
        found(Match {
            start_line: base + u64::from(m.start_row),
            start_col: m.start_col,
            end_line: base + u64::from(m.end_row),
            end_col: m.end_col,
        })
    });
    let next = stopped.map(|from| Piece { from, above: true });
    (flow, next)
}

/// Calls `found` with each match of `segment` of `rows` (the first of
/// which is absolute row `first`), in document order, until it returns
/// [`ControlFlow::Break`].
///
/// A segment with more than [`MAX_HAYSTACK_BYTES`] of text is searched in
/// pieces ([`scan_piece`]), top down: each piece starts at the cell the
/// previous one's cap left out, behind a cut marker as at a segment cut, so
/// every cell is searched and no piece is longer than the cap. A match that
/// would cross from one piece to the next is not found.
pub fn scan_segment(
    query: &Query,
    rows: &[Line],
    first: u64,
    segment: Segment,
    haystack: &mut Haystack,
    mut found: impl FnMut(Match) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let mut piece = Piece::first(segment);
    loop {
        let (flow, next) = scan_piece(query, rows, first, segment, piece, haystack, &mut found);
        flow?;
        let Some(next) = next else {
            return ControlFlow::Continue(());
        };
        piece = next;
    }
}

/// Splits `rows` into segments and calls `found` with each match, segment
/// by segment from the bottom one up and in document order within a
/// segment, until it returns [`ControlFlow::Break`]. `period` is
/// [`segment_period`] for the rows' width.
///
/// Segments run from the bottom up so a caller that keeps the newest
/// matches can stop early.
pub fn scan(
    query: &Query,
    rows: &[Line],
    at: Rows,
    period: usize,
    haystack: &mut Haystack,
    mut found: impl FnMut(Match) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let mut list = std::mem::take(&mut haystack.segments);
    segments(rows, at, period, &mut list);
    let mut result = ControlFlow::Continue(());
    for &segment in list.iter().rev() {
        result = scan_segment(query, rows, at.first, segment, haystack, &mut found);
        if result.is_break() {
            break;
        }
    }
    haystack.segments = list;
    result
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use tessera_term::Terminal;

    use super::*;

    /// The haystack before the plain-text runs: one entry per cell, at most
    /// `cap` bytes of text. The reference the run-length map must agree
    /// with.
    fn reference(
        rows: &[Line],
        above: bool,
        below: bool,
        cap: usize,
    ) -> (String, Vec<CellText>, bool) {
        let mut text = String::new();
        let mut cells = Vec::new();
        let mut capped = false;
        if above {
            text.push(CUT);
        }
        let limit = text.len().saturating_add(cap);
        let last_row = rows.len().saturating_sub(1);
        'rows: for (row, line) in rows.iter().enumerate() {
            let end = if row == last_row && !below {
                content_end(line)
            } else {
                line.cells.len()
            };
            for (col, cell) in line.cells[..end].iter().enumerate() {
                if cell.is_spacer() || cell.is_pad() {
                    continue;
                }
                let before = text.len();
                let cluster = if cell.is_cluster() {
                    line.cluster(col)
                } else {
                    None
                };
                let bytes = cluster.map_or(cell.ch.len_utf8(), str::len);
                if before + bytes > limit {
                    capped = true;
                    break 'rows;
                }
                match cluster {
                    Some(cluster) => text.push_str(cluster),
                    None => text.push(cell.ch),
                }
                cells.push(CellText {
                    byte: u32::try_from(before).unwrap(),
                    row: u32::try_from(row).unwrap(),
                    col: u16::try_from(col).unwrap(),
                    width: cell.width().max(1),
                });
            }
        }
        (text, cells, capped)
    }

    /// Fills a haystack from `rows` and checks its text, cap and the cell
    /// of every byte against [`reference`].
    fn check(rows: &[Line], above: bool, below: bool) {
        let mut haystack = Haystack::new();
        haystack.fill(rows, above, below);
        let (text, cells, capped) = reference(rows, above, below, MAX_HAYSTACK_BYTES);
        let start = usize::from(above);
        assert_eq!(haystack.text(), &text[start..]);
        assert_eq!(haystack.is_capped(), capped);
        for byte in start..text.len() {
            let index = cells
                .partition_point(|cell| cell.byte as usize <= byte)
                .saturating_sub(1);
            let want = cells[index];
            let got = haystack.cell_at(byte);
            assert_eq!(
                (got.row, got.col, got.width),
                (want.row, want.col, want.width),
                "byte {byte} of {text:?}"
            );
        }
    }

    /// Rows of `input` at `cols` columns, as the terminal lays them out.
    fn rows(cols: u16, input: &str) -> Vec<Line> {
        let mut terminal = Terminal::new(8, cols);
        terminal.advance(input.as_bytes());
        terminal.grid().visible_lines().to_vec()
    }

    /// Text pieces covering every cell kind: ASCII, blanks, non-ASCII in
    /// one column, wide (spacers, and padding at a wrap), clusters, a
    /// hard line break.
    fn piece() -> impl Strategy<Value = &'static str> {
        prop::sample::select(vec![
            "a",
            "Zq",
            " ",
            "  ",
            "é",
            "中",
            "e\u{301}",
            "👍🏽",
            "\r\n",
            "x\u{200d}",
        ])
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// The run-length cell map gives every byte the cell the per-cell
        /// map gives it.
        #[test]
        fn runs_agree_with_the_per_cell_map(
            cols in 2u16..12,
            pieces in prop::collection::vec(piece(), 0..40),
            above in any::<bool>(),
            below in any::<bool>(),
        ) {
            check(&rows(cols, &pieces.concat()), above, below);
        }
    }

    /// At the byte cap the runs stop where the per-cell map stops, with a
    /// wide character and a cluster straddling the limit.
    #[test]
    fn runs_agree_at_the_cap() {
        let cols = 2048;
        for tail in ["中中中中", "e\u{301}e\u{301}", "aaaa"] {
            for shift in 0..4 {
                let mut input = String::new();
                for _ in 0..31 {
                    input.push_str(&"a".repeat(2048));
                }
                input.push_str(&"b".repeat(2048 - 2 - shift));
                input.push_str(&tail.repeat(512));
                let mut terminal = Terminal::new(40, cols);
                terminal.advance(input.as_bytes());
                let rows: Vec<Line> = terminal.grid().visible_lines().to_vec();
                check(&rows, false, true);
                check(&rows, true, false);
            }
        }
    }

    /// One logical line of `rows` rows at `cols` columns: `cell` repeated,
    /// then `XYZ` in the last three cells.
    fn long_line(cols: u16, rows: usize, cell: &str, width: usize) -> Vec<Line> {
        let cells = usize::from(cols) * rows - 3;
        let mut input = cell.repeat(cells / width);
        input.push_str(&"a".repeat(cells % width));
        input.push_str("XYZ");
        let mut terminal = Terminal::new(u16::try_from(rows + 2).unwrap(), cols);
        terminal.advance(input.as_bytes());
        terminal.grid().visible_lines()[..rows].to_vec()
    }

    /// B1 (#46a): a segment of multi-byte text longer than the cap is
    /// refilled from the cell each piece's cap left out. The pieces'
    /// texts, joined, are the segment's whole text; each byte maps to the
    /// cell the per-cell map gives it; every piece after the first starts
    /// behind a cut marker; and no piece is longer than the cap.
    #[test]
    fn refills_cover_a_segment_past_the_cap() {
        let marks = format!("e{}", "\u{301}".repeat(30));
        let cases: [(u16, usize, &str, usize); 6] = [
            (2048, 15, "\u{2500}", 1),
            (300, 200, "\u{434}", 1),
            (80, 250, "\u{1d400}", 1),
            (80, 100, &marks, 1),
            (2047, 31, "\u{4e2d}", 2),
            (2048, 31, "a\u{4e2d}e\u{301}\u{1d400}", 5),
        ];
        for (cols, height, cell, width) in cases {
            let rows = long_line(cols, height, cell, width);
            for (above, below) in [(false, false), (true, true)] {
                let (text, cells, _) = reference(&rows, above, below, usize::MAX);
                let skip = usize::from(above);
                assert!(
                    text.len() - skip > MAX_HAYSTACK_BYTES,
                    "{cell:?} is not capped"
                );
                let mut haystack = Haystack::new();
                let mut joined = String::new();
                let mut from = (0, 0);
                let mut pieces = 0;
                loop {
                    let marker = above || pieces > 0;
                    let stopped = haystack.fill_from(&rows, from, marker, below);
                    pieces += 1;
                    assert_eq!(haystack.start, usize::from(marker));
                    assert!(haystack.text().len() <= MAX_HAYSTACK_BYTES);
                    assert_eq!(haystack.is_capped(), stopped.is_some());
                    for byte in haystack.start..haystack.end {
                        let at = skip + joined.len() + byte - haystack.start;
                        let index = cells
                            .partition_point(|cell| cell.byte as usize <= at)
                            .saturating_sub(1);
                        let want = cells[index];
                        let got = haystack.cell_at(byte);
                        assert_eq!(
                            (got.row, got.col, got.width),
                            (want.row, want.col, want.width),
                            "{cell:?} at {cols} columns, piece {pieces}, byte {byte}"
                        );
                    }
                    joined.push_str(haystack.text());
                    let Some(next) = stopped else { break };
                    from = next;
                }
                assert!(pieces > 1, "{cell:?}: one piece");
                assert_eq!(joined, text[skip..], "{cell:?} at {cols} columns");
            }
        }
    }
}
