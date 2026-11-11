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
//! such a cut is not found either. A caller can search in smaller pieces
//! ([`PieceLimit`]) and stop a piece's regex run at a deadline
//! ([`scan_piece`]).

use std::fmt;
use std::ops::ControlFlow;
use std::time::Instant;

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

/// Most cells of one piece of text at the full [`PieceLimit`]: a segment
/// never holds more (it has fewer than two periods of rows, each period at
/// most half of [`MAX_HAYSTACK_BYTES`] cells), so at the full limit only
/// the byte cap cuts a segment into pieces.
pub const MAX_PIECE_CELLS: usize = MAX_HAYSTACK_BYTES;

/// The fewest bytes a halved [`PieceLimit`] allows: halving stops once a
/// piece is at most one row and at most this many bytes.
pub const MIN_PIECE_BYTES: usize = 1 << 10;

/// Most regex runs of a literal (not regex) query between two reads of the
/// clock in [`Query::find_until`]. A regex's clock is read after every run;
/// a literal's run ends at its match, so reading the clock after each
/// match of a dense literal cost about a quarter of the search (#368).
pub const LITERAL_RUNS_PER_CLOCK_READ: u32 = 16;

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
/// rule S8). Its [`Debug`](fmt::Debug) form gives the message's length
/// only, so a `{:?}` of the error (or of an error that wraps it) does not
/// carry the pattern into a log.
#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { bytes } => f.debug_struct("TooLong").field("bytes", bytes).finish(),
            // The message quotes the pattern (S8): its length only.
            Self::Syntax(message) => f
                .debug_struct("Syntax")
                .field("message_bytes", &message.len())
                .finish(),
            Self::TooComplex => f.write_str("TooComplex"),
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
        found: impl FnMut(SegmentMatch) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.find_until(haystack, None, found)
            .unwrap_or(ControlFlow::Continue(()))
    }

    /// [`Query::find`] that stops once `deadline` has passed: `None` when
    /// it stopped there, the matches before it having been handed to
    /// `found`. The clock is read before the first of the regex crate's
    /// runs and after every run of a regex, the last one too, whether or
    /// not it found a match, so the stop comes at most one run late (#331):
    /// a deadline already past stops before any regex runs, and a search
    /// whose last run ended past the deadline returns `None` even though it
    /// reached the haystack's end. One run is not interruptible; it costs
    /// up to the haystack's length times the program's size.
    ///
    /// A literal query's clock is read after every
    /// [`LITERAL_RUNS_PER_CLOCK_READ`]-th run instead (and after the last):
    /// each of its runs scans the text only up to its match's end, so the
    /// runs between two reads scan disjoint stretches of the text and cost
    /// no more than one run over them would. The stop then comes at most
    /// that many short runs late, never more text than one run could take.
    pub fn find_until(
        &self,
        haystack: &Haystack,
        deadline: Option<Instant>,
        mut found: impl FnMut(SegmentMatch) -> ControlFlow<()>,
    ) -> Option<ControlFlow<()>> {
        let text = haystack.text.as_str();
        let mut at = haystack.start;
        let mut previous_end: Option<(u32, u16)> = None;
        // Runs before the clock is read next: one for a regex, whose run
        // may scan far past its match (#331), more for a literal.
        let runs_per_read = if self.options.regex {
            1
        } else {
            LITERAL_RUNS_PER_CLOCK_READ
        };
        let mut unread = 0;
        while at <= haystack.end {
            if unread == 0 {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return None;
                }
                unread = runs_per_read;
            }
            unread -= 1;
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
            let flow = found(SegmentMatch {
                start_row: first.row,
                start_col: first.col,
                end_row: last.row,
                end_col: end.1,
            });
            if flow.is_break() {
                return Some(flow);
            }
        }
        // The last run, which found nothing past the matches already
        // handed over, may itself have passed the deadline (#331, F9c).
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return None;
        }
        Some(ControlFlow::Continue(()))
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
    /// Cells the last fill took in, spacers and padding included.
    cells: usize,
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
        let _ = self.fill_from(
            rows,
            (0, 0),
            PieceLimit::FULL,
            continues_above,
            continues_below,
        );
    }

    /// [`Haystack::fill`] from the cell at `from` (row index in `rows`,
    /// column) on, at most `limit`'s bytes and cells: the cells before
    /// `from` are left out. Returns the position of the first cell left
    /// out at the cap, or `None` when the text reaches the end of `rows`.
    ///
    /// The first cell always goes in, so a refill from the returned
    /// position makes progress; a cell holds at most
    /// `4 * MAX_CLUSTER_SCALARS` bytes, far below the cap.
    fn fill_from(
        &mut self,
        rows: &[Line],
        from: (usize, usize),
        limit: PieceLimit,
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
        let cell_limit = limit.cells.max(1);
        let mut cells_left = cell_limit;
        let limit = self.start + limit.bytes.max(1);
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
            if first_col < end && cells_left == 0 {
                // The cell cap: the piece ends before this row.
                self.capped = true;
                stopped = Some((row, first_col));
                break;
            }
            // The cells this row gives, at most what the cell cap leaves.
            let row_end = end.min(first_col.saturating_add(cells_left));
            cells_left -= row_end - first_col;
            // True while the last span is a run this row can extend.
            let mut open = false;
            for (col, cell) in cells[..row_end].iter().enumerate().skip(first_col) {
                if is_plain(cell) {
                    let before = self.text.len();
                    if before >= limit {
                        self.capped = true;
                        stopped = Some((row, col));
                        cells_left += row_end - col;
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
                    cells_left += row_end - col;
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
            if row_end < end {
                // The cell cap fell inside this row.
                self.capped = true;
                stopped = Some((row, row_end));
                break;
            }
        }
        self.end = self.text.len();
        self.cells = cell_limit - cells_left;
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

/// How much text one piece holds: at most this many bytes and this many
/// cells (columns, spacers included). [`PieceLimit::FULL`] cuts a segment
/// only at [`MAX_HAYSTACK_BYTES`]; a caller whose regex is slow on a piece
/// halves the limit ([`PieceLimit::halved`]) so a piece's run takes about a
/// quarter of the time (the regex crate's match iteration is quadratic in
/// the worst case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PieceLimit {
    bytes: usize,
    cells: usize,
}

impl PieceLimit {
    /// [`MAX_HAYSTACK_BYTES`] and [`MAX_PIECE_CELLS`].
    pub const FULL: Self = Self {
        bytes: MAX_HAYSTACK_BYTES,
        cells: MAX_PIECE_CELLS,
    };

    /// Most bytes of text of one piece.
    pub fn bytes(self) -> usize {
        self.bytes
    }

    /// Most cells of one piece.
    pub fn cells(self) -> usize {
        self.cells
    }

    /// Half the limit, but not below one row of `cols` cells and
    /// [`MIN_PIECE_BYTES`] bytes; `None` when it is at that floor already.
    pub fn halved(self, cols: u16) -> Option<Self> {
        self.halved_from(self, cols)
    }

    /// Half of `piece`, a piece's own size ([`PieceScan::size`]), but not
    /// below the floor of [`PieceLimit::halved`] and not above this limit:
    /// the limit for searching a piece too slow at `piece` again, so the
    /// first retry already cuts it, however far below this limit it was
    /// (#331, F9c). `None` when this limit is at the floor already.
    pub fn halved_from(self, piece: Self, cols: u16) -> Option<Self> {
        let row = usize::from(cols.max(1));
        let half = Self {
            bytes: (piece.bytes / 2).max(MIN_PIECE_BYTES).min(self.bytes),
            cells: (piece.cells / 2).max(row).min(self.cells),
        };
        (half != self).then_some(half)
    }
}

impl Default for PieceLimit {
    fn default() -> Self {
        Self::FULL
    }
}

/// Where the scan of a segment stands between two of its pieces
/// ([`scan_piece`]): the segment and the cell the next piece starts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Piece {
    /// Absolute row number of the first row of the run the segment is in.
    first: u64,
    segment: Segment,
    /// Row index within the segment and column of the piece's first cell.
    from: (usize, usize),
    /// The piece starts behind a cut marker.
    above: bool,
}

impl Piece {
    /// The first piece of `segment` of a run of rows whose first row is
    /// absolute row `first`.
    pub fn first(first: u64, segment: Segment) -> Self {
        Self {
            first,
            segment,
            from: (0, 0),
            above: segment.above,
        }
    }

    /// The segment the piece is in.
    pub fn segment(&self) -> Segment {
        self.segment
    }

    /// Absolute row number of the piece's first row.
    pub fn line(&self) -> u64 {
        self.first + (self.segment.start + self.from.0) as u64
    }
}

/// How the search of one piece ended ([`scan_piece`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PieceScan {
    /// [`ControlFlow::Break`] when `found` returned it.
    pub flow: ControlFlow<()>,
    /// The piece after this one, `None` when this one reached the
    /// segment's end. Known before the regex runs, so it is set when the
    /// run timed out too.
    pub next: Option<Piece>,
    /// The deadline passed before the regex reached the piece's end: the
    /// matches handed to `found` are those before the stop.
    pub timed_out: bool,
    /// The piece's own bytes of text (cut markers left out) and cells, at
    /// most the limit it was searched at.
    pub size: PieceLimit,
}

/// Searches one piece of a segment of `rows`, at most `limit`'s bytes and
/// cells of its text from `piece` on, and calls `found` with each match in
/// it, in document order, until it returns [`ControlFlow::Break`] or
/// `deadline` passes ([`Query::find_until`]).
///
/// Without a deadline a run can take seconds: a quadratic pattern such as
/// `\w*X\b|\w` costs about a millisecond per match on 64 KiB of combining
/// marks. A caller bounds it with the deadline and, when a piece times
/// out, retries it at a [halved](PieceLimit::halved) limit or skips it.
pub fn scan_piece(
    query: &Query,
    rows: &[Line],
    piece: Piece,
    limit: PieceLimit,
    deadline: Option<Instant>,
    haystack: &mut Haystack,
    mut found: impl FnMut(Match) -> ControlFlow<()>,
) -> PieceScan {
    let segment = piece.segment;
    let rows = &rows[segment.start..segment.end];
    let base = piece.first + segment.start as u64;
    let stopped = haystack.fill_from(rows, piece.from, limit, piece.above, segment.below);
    let flow = query.find_until(haystack, deadline, |m| {
        found(Match {
            start_line: base + u64::from(m.start_row),
            start_col: m.start_col,
            end_line: base + u64::from(m.end_row),
            end_col: m.end_col,
        })
    });
    PieceScan {
        flow: flow.unwrap_or(ControlFlow::Continue(())),
        next: stopped.map(|from| Piece {
            from,
            above: true,
            ..piece
        }),
        timed_out: flow.is_none(),
        size: PieceLimit {
            bytes: haystack.end - haystack.start,
            cells: haystack.cells,
        },
    }
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
    let mut piece = Piece::first(first, segment);
    loop {
        let scan = scan_piece(
            query,
            rows,
            piece,
            PieceLimit::FULL,
            None,
            haystack,
            &mut found,
        );
        scan.flow?;
        let Some(next) = scan.next else {
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

    /// Fills `rows` piece by piece at `limit`, as [`scan_piece`] does, and
    /// checks that the pieces' texts, joined, are the whole text, that
    /// each byte maps to the cell the per-cell map gives it, that every
    /// piece after the first starts behind a cut marker and that no piece
    /// holds more than the limit's bytes or cells (beyond its first cell,
    /// which always goes in). Returns the number of pieces.
    fn check_pieces(rows: &[Line], above: bool, below: bool, limit: PieceLimit) -> usize {
        let (text, cells, _) = reference(rows, above, below, usize::MAX);
        let skip = usize::from(above);
        let mut haystack = Haystack::new();
        let mut joined = String::new();
        let mut from = (0, 0);
        let mut pieces = 0;
        loop {
            let marker = above || pieces > 0;
            let stopped = haystack.fill_from(rows, from, limit, marker, below);
            pieces += 1;
            assert_eq!(haystack.start, usize::from(marker));
            assert_eq!(haystack.is_capped(), stopped.is_some());
            let spans = &haystack.spans;
            if let (Some(first), Some(last)) = (spans.first(), spans.last()) {
                // Columns from the first cell to past the last one.
                let columns =
                    |span: &Span| span.cell.row as usize * 100_000 + usize::from(span.cell.col);
                let last_width = if last.run {
                    haystack.end - last.cell.byte as usize
                } else {
                    usize::from(last.cell.width)
                };
                let one_row = first.cell.row == last.cell.row;
                if one_row {
                    // A wide character the cap falls on brings its spacer.
                    let width = columns(last) + last_width - columns(first);
                    let spill = if last.run {
                        0
                    } else {
                        usize::from(last.cell.width) - 1
                    };
                    assert!(
                        width <= limit.cells().max(usize::from(first.cell.width)) + spill,
                        "a piece of {width} cells at {limit:?}"
                    );
                }
                let bytes = haystack.end - haystack.start;
                let first_bytes = spans
                    .get(1)
                    .map_or(bytes, |next| next.cell.byte as usize - haystack.start);
                assert!(
                    bytes <= limit.bytes().max(first_bytes),
                    "a piece of {bytes} bytes at {limit:?}"
                );
            }
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
                    "piece {pieces}, byte {byte} at {limit:?}"
                );
            }
            joined.push_str(haystack.text());
            let Some(next) = stopped else { break };
            assert!(next > from, "no progress from {from:?}");
            from = next;
        }
        assert_eq!(joined, text[skip..], "at {limit:?}");
        pieces
    }

    /// A limit of `bytes` bytes and `cells` cells.
    fn limit(bytes: usize, cells: usize) -> PieceLimit {
        PieceLimit { bytes, cells }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// #331: pieces cut at any byte and cell limit cover the text of a
        /// run of rows exactly, each within the limit, every byte mapped to
        /// its cell; resuming a column late or early loses or repeats text.
        #[test]
        fn pieces_at_any_limit_cover_the_text(
            cols in 2u16..12,
            pieces in prop::collection::vec(piece(), 0..40),
            above in any::<bool>(),
            below in any::<bool>(),
            bytes in 1usize..24,
            cells in 1usize..16,
        ) {
            let rows = rows(cols, &pieces.concat());
            check_pieces(&rows, above, below, limit(bytes, cells));
        }

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

    /// Halving goes down to one row and [`MIN_PIECE_BYTES`], each step at
    /// least halving one of the two, and stops there.
    #[test]
    fn halving_stops_at_one_row_and_the_byte_floor() {
        for cols in [1, 80, 300, 2048, u16::MAX] {
            let mut at = PieceLimit::FULL;
            let mut steps = 0;
            while let Some(half) = at.halved(cols) {
                assert!(half.bytes <= at.bytes && half.cells <= at.cells);
                assert!(
                    half.bytes * 2 <= at.bytes.max(MIN_PIECE_BYTES * 2)
                        || half.cells * 2 <= at.cells.max(usize::from(cols) * 2)
                );
                at = half;
                steps += 1;
                assert!(steps < 64, "{cols} columns never reach the floor");
            }
            assert_eq!(at.bytes, MIN_PIECE_BYTES, "{cols} columns");
            assert_eq!(
                at.cells,
                usize::from(cols).min(MAX_PIECE_CELLS),
                "{cols} columns"
            );
            eprintln!("{cols} columns: {steps} halvings to {at:?}");
        }
    }

    /// #331: a full segment of heavy combining marks, CJK and plain text
    /// cut at every halving down to the floor: the pieces cover it exactly
    /// and grow in number as the limit shrinks.
    #[test]
    fn pieces_at_every_halving_cover_a_long_segment() {
        let marks = format!("e{}", "\u{301}".repeat(30));
        let cases: [(u16, usize, &str, usize); 4] = [
            (300, 12, &marks, 1),
            (2048, 31, "\u{4e2d}", 2),
            (80, 250, "a", 1),
            (200, 160, "\u{434}", 1),
        ];
        for (cols, height, cell, width) in cases {
            let rows = long_line(cols, height, cell, width);
            let mut at = PieceLimit::FULL;
            let mut last = check_pieces(&rows, false, true, at);
            while let Some(half) = at.halved(cols) {
                at = half;
                let pieces = check_pieces(&rows, true, false, at);
                assert!(
                    pieces >= last,
                    "{cell:?}: {pieces} pieces at {at:?}, {last} before"
                );
                last = pieces;
            }
            assert!(
                last >= height,
                "{cell:?}: {last} pieces at the floor for {height} rows"
            );
        }
    }

    /// S8 (#318): the Debug form of a syntax error, which quotes the
    /// pattern in its message, gives the message's length only.
    #[test]
    fn the_debug_form_of_a_refusal_omits_the_pattern() {
        let regex = QueryOptions {
            regex: true,
            case_sensitive: false,
        };
        let error = Query::new("secret-pattern(", regex).unwrap_err();
        assert!(error.to_string().contains("secret-pattern"), "{error}");
        let debug = format!("{error:?} {error:#?}");
        assert!(!debug.contains("secret"), "{debug}");
        assert!(debug.contains("Syntax"), "{debug}");
        assert_eq!(
            format!("{:?}", QueryError::TooLong { bytes: 2000 }),
            "TooLong { bytes: 2000 }"
        );
        assert_eq!(format!("{:?}", QueryError::TooComplex), "TooComplex");
    }

    /// A deadline already past stops a piece before any regex run; a far
    /// one finds what [`Query::find`] finds.
    #[test]
    fn a_passed_deadline_stops_the_run_and_a_far_one_finds_everything() {
        let query = Query::new("a", QueryOptions::default()).unwrap().unwrap();
        let rows = rows(40, &"ab".repeat(100));
        let mut haystack = Haystack::new();
        haystack.fill(&rows, false, false);
        let mut all = Vec::new();
        let _ = query.find(&haystack, |m| {
            all.push(m);
            ControlFlow::Continue(())
        });
        assert_eq!(all.len(), 100);
        let mut early = Vec::new();
        let past = Instant::now();
        let ended = query.find_until(&haystack, Some(past), |m| {
            early.push(m);
            ControlFlow::Continue(())
        });
        assert_eq!(ended, None);
        assert!(
            early.is_empty(),
            "{} matches past the deadline",
            early.len()
        );
        let mut late = Vec::new();
        let far = Instant::now() + std::time::Duration::from_secs(3600);
        let ended = query.find_until(&haystack, Some(far), |m| {
            late.push(m);
            ControlFlow::Continue(())
        });
        assert_eq!(ended, Some(ControlFlow::Continue(())));
        assert_eq!(late, all);
    }

    /// #331 round 1 (swift B1): the clock is read after every regex run,
    /// so at most one run ends after the deadline: the one under way when
    /// it passed. Each run of `\w*X\b|\w` over 64 KiB of Cyrillic scans
    /// to the text's end, takes milliseconds and finds one cell; the
    /// deadline passes during the first. Reading the clock every 16 runs
    /// ran 15 more.
    #[test]
    fn at_most_one_run_ends_after_the_deadline() {
        let rows = long_line(300, 109, "\u{434}", 1);
        let query = Query::new(
            r"\w*X\b|\w",
            QueryOptions {
                regex: true,
                case_sensitive: true,
            },
        )
        .unwrap()
        .unwrap();
        let mut haystack = Haystack::new();
        haystack.fill(&rows, false, false);
        let deadline = Instant::now() + std::time::Duration::from_micros(200);
        let mut late = 0_usize;
        let mut found = 0_usize;
        let ended = query.find_until(&haystack, Some(deadline), |_| {
            found += 1;
            if Instant::now() >= deadline {
                late += 1;
            }
            ControlFlow::Continue(())
        });
        assert_eq!(ended, None, "{found} matches, none stopped");
        assert!(late <= 1, "{late} runs ended after the deadline");
    }

    /// #378: a literal query reads the clock every
    /// [`LITERAL_RUNS_PER_CLOCK_READ`] runs, so at most that many runs end
    /// after the deadline. Every cell of 64 KiB is an `X`, so each run
    /// finds one; the deadline passes after a few thousand. Never reading
    /// the clock handed out every match after it.
    #[test]
    fn a_literal_stops_within_its_runs_per_clock_read() {
        let rows = long_line(300, 218, "X", 1);
        let query = Query::new(
            "X",
            QueryOptions {
                regex: false,
                case_sensitive: true,
            },
        )
        .unwrap()
        .unwrap();
        let mut haystack = Haystack::new();
        haystack.fill(&rows, false, false);
        let deadline = Instant::now() + std::time::Duration::from_micros(200);
        let mut late = 0_usize;
        let mut found = 0_usize;
        let ended = query.find_until(&haystack, Some(deadline), |_| {
            found += 1;
            if Instant::now() >= deadline {
                late += 1;
            }
            ControlFlow::Continue(())
        });
        assert_eq!(ended, None, "{found} matches, none stopped");
        let most = usize::try_from(LITERAL_RUNS_PER_CLOCK_READ).unwrap();
        assert!(late <= most, "{late} runs ended after the deadline");
    }

    /// #331 adversarial cycle 1 (F9c): a run that finds nothing and ends
    /// past the deadline is a stop too, not a piece searched to its end.
    /// `(?:\w{1,40}){1,5}\b\d` has no match in 64 KiB of Cyrillic, and its
    /// single run takes about 0.1 s, so the deadline passes inside it;
    /// reading the clock only before each run returned `Continue`.
    #[test]
    fn a_last_run_without_a_match_past_the_deadline_is_a_stop() {
        let rows = long_line(300, 109, "\u{434}", 1);
        let query = Query::new(
            r"(?:\w{1,40}){1,5}\b\d",
            QueryOptions {
                regex: true,
                case_sensitive: true,
            },
        )
        .unwrap()
        .unwrap();
        let mut haystack = Haystack::new();
        haystack.fill(&rows, false, false);
        let started = Instant::now();
        let deadline = started + std::time::Duration::from_millis(1);
        let mut found = 0_usize;
        let ended = query.find_until(&haystack, Some(deadline), |_| {
            found += 1;
            ControlFlow::Continue(())
        });
        let took = started.elapsed();
        assert_eq!(found, 0, "the pattern matched");
        assert_eq!(ended, None, "a run of {took:?} ended past the deadline");
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
                    let stopped = haystack.fill_from(&rows, from, PieceLimit::FULL, marker, below);
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
