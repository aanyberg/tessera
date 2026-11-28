//! Packed scrollback blocks (ADR 0051): the compact form of history rows
//! that are out of the viewport and no longer edited.
//!
//! A [`Block`] holds up to [`BLOCK_ROWS`] rows. Each row keeps 24 bytes of
//! metadata ([`RowMeta`]) and, in the block's shared vectors:
//!
//! - the UTF-8 of every cell's [`Cell::ch`] up to the last cell that is not
//!   [`Cell::BLANK`] (the rest of the row is [`Cell::BLANK`] again on
//!   decode), one scalar per cell;
//! - its style runs, 4 bytes each: a run length in cells and an index into
//!   the block's style table, which holds each distinct [`Style`] once;
//! - a sparse list of the cells whose [`CellFlags`] are not empty (wide,
//!   spacer, cluster, padding, link), 4 bytes each;
//! - its grapheme clusters (column and text) and, for the rare rows that
//!   have them, its hyperlink runs, marks and mark anchors.
//!
//! A plain 80-column ASCII row costs about 24 + 80 + 4 bytes. Decoding a row
//! ([`Block::decode_into`]) rebuilds exactly the [`Line`] that was packed;
//! a re-wrap reads rows straight into its joined buffer
//! ([`Block::join_into`]) without building a [`Line`].
//!
//! # Bounds
//!
//! A block is sealed (its vectors trimmed to fit, its style index dropped)
//! when it holds [`BLOCK_ROWS`] rows, or before a row whose worst case
//! would overflow a 16-bit style index or a 32-bit offset: at most
//! [`BLOCK_ROWS`] × 65 535 cells, each at most 4 bytes of text and
//! [`crate::MAX_CLUSTER_SCALARS`] × 4 bytes of cluster text, so every
//! offset fits in a `u32` anyway; the checks are belt and braces.

use std::collections::HashMap;
use std::ops::Range;

use crate::line::MarkAnchors;
use crate::{Cell, CellFlags, Hyperlink, Line, Mark, Style};

/// Rows per packed block.
pub(crate) const BLOCK_ROWS: usize = 256;

/// One packed row: where its parts end in the block's vectors (each starts
/// where the previous row's ends), its width and how many cells were
/// encoded. 24 bytes.
#[derive(Debug, Clone, Copy)]
struct RowMeta {
    /// End of its text in [`Block::text`].
    text_end: u32,
    /// End of its runs in [`Block::runs`].
    run_end: u32,
    /// End of its flagged cells in [`Block::flags`].
    flag_end: u32,
    /// End of its clusters in [`Block::clusters`].
    cluster_end: u32,
    /// Its width in cells ([`Line::cells`]'s length).
    cols: u16,
    /// Cells encoded: the rest are [`Cell::BLANK`].
    cells: u16,
    /// [`Line::wrapped`].
    wrapped: bool,
    /// Whether every encoded cell's text is one byte (ASCII).
    ascii: bool,
}

/// A run of cells of one style.
#[derive(Debug, Clone, Copy)]
struct Run {
    /// Cells in the run.
    len: u16,
    /// Index into [`Block::styles`].
    style: u16,
}

/// A cell whose flags are not empty.
#[derive(Debug, Clone, Copy)]
struct Flagged {
    /// Its column.
    col: u16,
    /// Its flags.
    flags: CellFlags,
}

/// A grapheme cluster: its column and the end of its text in
/// [`Block::cluster_text`].
#[derive(Debug, Clone, Copy)]
struct ClusterAt {
    /// Its column.
    col: u16,
    /// End of its text.
    end: u32,
}

/// What a few rows carry beyond text, styles, flags and clusters.
#[derive(Debug, Clone)]
struct Extra {
    /// The row, in the block.
    row: u16,
    /// Hyperlink runs, by ascending column.
    links: Vec<(Range<u16>, Hyperlink)>,
    /// [`Line::marks`].
    marks: Vec<Mark>,
    /// [`Line::anchors`].
    anchors: MarkAnchors,
}

/// Up to [`BLOCK_ROWS`] packed rows (see the module docs).
#[derive(Debug, Clone, Default)]
pub(crate) struct Block {
    /// Per-row metadata.
    rows: Vec<RowMeta>,
    /// Cell text of every row.
    text: String,
    /// Style runs of every row.
    runs: Vec<Run>,
    /// Distinct styles.
    styles: Vec<Style>,
    /// Flagged cells of every row.
    flags: Vec<Flagged>,
    /// Clusters of every row.
    clusters: Vec<ClusterAt>,
    /// Their text.
    cluster_text: String,
    /// Rows with links, marks or anchors, by ascending row.
    extras: Vec<Extra>,
    /// Style index while the block is open; `None` once sealed.
    index: Option<HashMap<Style, u16>>,
    /// The style interned last and its index: rows mostly repeat it, and
    /// a compare is cheaper than a hash.
    last_style: Option<(Style, u16)>,
    /// Sequence number of the block's first row in its store, for lookups.
    pub(crate) first: u64,
}

impl Block {
    /// An open, empty block whose first row has sequence number `first`.
    pub(crate) fn open(first: u64) -> Self {
        Self {
            index: Some(HashMap::default()),
            first,
            ..Self::default()
        }
    }

    /// Rows held.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the block takes more rows: open and below [`BLOCK_ROWS`].
    pub(crate) fn is_open(&self) -> bool {
        self.index.is_some() && self.rows.len() < BLOCK_ROWS
    }

    /// Whether `line` is sure to fit: its every cell could add a style and
    /// at most 4 + 128 bytes of text without overflowing an index.
    pub(crate) fn fits(&self, line: &Line) -> bool {
        let cells = line.cells.len();
        let max_text = u32::MAX as usize / 2;
        u16::try_from(self.styles.len() + cells).is_ok()
            && self.text.len() + cells * 4 <= max_text
            && self.cluster_text.len() + cells * 4 * crate::MAX_CLUSTER_SCALARS <= max_text
            && self.runs.len() + cells <= max_text
            && self.flags.len() + cells <= max_text
            && self.clusters.len() + cells <= max_text
    }

    /// Trims the vectors to their contents and drops the style index: no
    /// more rows come.
    pub(crate) fn seal(&mut self) {
        self.index = None;
        self.last_style = None;
        self.rows.shrink_to_fit();
        self.text.shrink_to_fit();
        self.runs.shrink_to_fit();
        self.styles.shrink_to_fit();
        self.flags.shrink_to_fit();
        self.clusters.shrink_to_fit();
        self.cluster_text.shrink_to_fit();
        self.extras.shrink_to_fit();
    }

    /// Bytes the block holds on the heap (tests and the memory budget).
    #[cfg(test)]
    pub(crate) fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        self.rows.capacity() * size_of::<RowMeta>()
            + self.text.capacity()
            + self.runs.capacity() * size_of::<Run>()
            + self.styles.capacity() * size_of::<Style>()
            + self.flags.capacity() * size_of::<Flagged>()
            + self.clusters.capacity() * size_of::<ClusterAt>()
            + self.cluster_text.capacity()
            + self.extras.capacity() * size_of::<Extra>()
    }

    /// The style's index, interned in the open block's table.
    fn intern(&mut self, style: Style) -> u16 {
        if let Some((last, id)) = self.last_style
            && last == style
        {
            return id;
        }
        let id = self.intern_new(style);
        self.last_style = Some((style, id));
        id
    }

    /// [`Block::intern`] through the hash index.
    #[inline(never)]
    fn intern_new(&mut self, style: Style) -> u16 {
        let styles = &mut self.styles;
        let index = self.index.get_or_insert_with(HashMap::default);
        *index.entry(style).or_insert_with(|| {
            // `fits` keeps the table below `u16::MAX` entries.
            let id = u16::try_from(styles.len()).unwrap_or(u16::MAX);
            styles.push(style);
            id
        })
    }

    /// Appends `line` as the block's newest row. Takes the line's marks,
    /// clusters and links; its cells are left for the caller to recycle.
    /// The block must be open and `line` must fit ([`Block::fits`]).
    pub(crate) fn push(&mut self, line: &mut Line) {
        let cols = line.cells.len();
        let plain = plain_end(&line.cells);
        let odd = plain.is_none();
        let cells = plain.unwrap_or_else(|| {
            line.cells
                .iter()
                .rposition(|cell| *cell != Cell::BLANK)
                .map_or(0, |i| i + 1)
        });
        if self.rows.is_empty() {
            // Room for a block of rows like this one: a seal trims it.
            self.rows.reserve_exact(BLOCK_ROWS);
            self.text.reserve_exact(BLOCK_ROWS * cells);
            self.runs.reserve_exact(BLOCK_ROWS);
        }
        let text_start = self.text.len();
        let used = &line.cells[..cells];
        if !odd {
            if cells > 0 {
                push_ascii(&mut self.text, used);
                let style = self.intern(Style::DEFAULT);
                self.runs.push(Run {
                    len: to_u16(cells),
                    style,
                });
            }
        } else if let Some(first) = used.first()
            && plain_row(used, first.style)
        {
            // One style, no flags, ASCII: the text and one run.
            self.text.extend(used.iter().map(|cell| cell.ch));
            let style = self.intern(first.style);
            self.runs.push(Run {
                len: to_u16(cells),
                style,
            });
        } else {
            self.push_cells(used);
        }
        self.push_rest(line, cols, cells, text_start);
    }

    /// The per-cell part of [`Block::push`]: text, style runs and flags.
    fn push_cells(&mut self, used: &[Cell]) {
        let mut run: Option<(Style, u16)> = None;
        for (col, cell) in used.iter().enumerate() {
            self.text.push(cell.ch);
            match &mut run {
                Some((style, len)) if *style == cell.style => *len += 1,
                _ => {
                    if let Some((style, len)) = run {
                        let style = self.intern(style);
                        self.runs.push(Run { len, style });
                    }
                    run = Some((cell.style, 1));
                }
            }
            if !cell.flags.is_empty() {
                self.flags.push(Flagged {
                    col: to_u16(col),
                    flags: cell.flags,
                });
            }
        }
        if let Some((style, len)) = run {
            let style = self.intern(style);
            self.runs.push(Run { len, style });
        }
    }

    /// The rest of [`Block::push`]: clusters, extras and the row's
    /// metadata.
    fn push_rest(&mut self, line: &mut Line, cols: usize, cells: usize, text_start: usize) {
        for (col, text) in line.graphemes.iter() {
            self.cluster_text.push_str(text);
            self.clusters.push(ClusterAt {
                col: to_u16(col),
                end: to_u32(self.cluster_text.len()),
            });
        }
        let row = to_u16(self.rows.len());
        let links: Vec<_> = line
            .graphemes
            .link_runs()
            .map(|(run, link)| (to_u16(run.start)..to_u16(run.end), link.clone()))
            .collect();
        if !links.is_empty() || !line.marks.is_empty() || line.anchors != MarkAnchors::default() {
            self.extras.push(Extra {
                row,
                links,
                marks: std::mem::take(&mut line.marks),
                anchors: std::mem::take(&mut line.anchors),
            });
        }
        line.graphemes.clear();
        self.rows.push(RowMeta {
            text_end: to_u32(self.text.len()),
            run_end: to_u32(self.runs.len()),
            flag_end: to_u32(self.flags.len()),
            cluster_end: to_u32(self.clusters.len()),
            cols: to_u16(cols),
            cells: to_u16(cells),
            wrapped: line.wrapped,
            ascii: self.text.len() - text_start == cells,
        });
    }

    /// Where row `r`'s parts start: text, runs, flags, clusters and
    /// cluster text.
    fn starts(&self, r: usize) -> (usize, usize, usize, usize) {
        r.checked_sub(1)
            .and_then(|p| self.rows.get(p))
            .map_or((0, 0, 0, 0), |m| {
                (
                    m.text_end as usize,
                    m.run_end as usize,
                    m.flag_end as usize,
                    m.cluster_end as usize,
                )
            })
    }

    /// Start of cluster `i`'s text.
    fn cluster_start(&self, i: usize) -> usize {
        i.checked_sub(1)
            .and_then(|p| self.clusters.get(p))
            .map_or(0, |c| c.end as usize)
    }

    /// The extras of row `r`, if any.
    fn extra(&self, r: usize) -> Option<&Extra> {
        let row = to_u16(r);
        self.extras
            .binary_search_by_key(&row, |e| e.row)
            .ok()
            .map(|i| &self.extras[i])
    }

    /// Row `r`'s width.
    pub(crate) fn cols(&self, r: usize) -> usize {
        usize::from(self.rows[r].cols)
    }

    /// Whether row `r` continues onto the next.
    pub(crate) fn wrapped(&self, r: usize) -> bool {
        self.rows[r].wrapped
    }

    /// Whether row `r`'s last cell is padding ([`Cell::is_pad`]).
    pub(crate) fn last_is_pad(&self, r: usize) -> bool {
        let meta = self.rows[r];
        if meta.cells != meta.cols || meta.cols == 0 {
            return false;
        }
        let (_, _, flags, _) = self.starts(r);
        self.flags[flags..meta.flag_end as usize]
            .last()
            .is_some_and(|f| f.col + 1 == meta.cols && f.flags.contains(CellFlags::PAD))
    }

    /// Appends row `r`'s first `len` cells (at most its width) to `cells`,
    /// each with its style and flags.
    fn cells_into(&self, r: usize, len: usize, cells: &mut Vec<Cell>) {
        let meta = self.rows[r];
        let (text, runs, flags, _) = self.starts(r);
        let base = cells.len();
        let encoded = usize::from(meta.cells).min(len);
        let text = &self.text[text..meta.text_end as usize];
        let runs = &self.runs[runs..meta.run_end as usize];
        cells.reserve(len);
        let mut left = encoded;
        if meta.ascii {
            let mut bytes = text.as_bytes();
            for run in runs {
                let n = usize::from(run.len).min(left);
                let style = self.styles[usize::from(run.style)];
                cells.extend(bytes[..n].iter().map(|&b| Cell::new(char::from(b), style)));
                bytes = &bytes[n..];
                left -= n;
                if left == 0 {
                    break;
                }
            }
        } else {
            let mut chars = text.chars();
            for run in runs {
                let n = usize::from(run.len).min(left);
                let style = self.styles[usize::from(run.style)];
                cells.extend(chars.by_ref().take(n).map(|ch| Cell::new(ch, style)));
                left -= n;
                if left == 0 {
                    break;
                }
            }
        }
        for f in &self.flags[flags..meta.flag_end as usize] {
            let col = usize::from(f.col);
            if col < encoded {
                cells[base + col].flags = f.flags;
            }
        }
        cells.resize(base + len, Cell::BLANK);
    }

    /// Rebuilds row `r` into `line`, reusing its allocations: exactly the
    /// line that was packed.
    pub(crate) fn decode_into(&self, r: usize, line: &mut Line) {
        let meta = self.rows[r];
        line.cells.clear();
        self.cells_into(r, usize::from(meta.cols), &mut line.cells);
        line.graphemes.clear();
        let (_, _, _, clusters) = self.starts(r);
        for i in clusters..meta.cluster_end as usize {
            let at = self.clusters[i];
            let text = &self.cluster_text[self.cluster_start(i)..at.end as usize];
            line.graphemes.set(usize::from(at.col), text.into());
        }
        line.marks.clear();
        line.anchors = MarkAnchors::default();
        if let Some(extra) = self.extra(r) {
            for (run, link) in &extra.links {
                line.graphemes
                    .set_link_range(usize::from(run.start)..usize::from(run.end), link);
            }
            line.marks.clone_from(&extra.marks);
            line.anchors = extra.anchors;
        }
        line.graphemes.compact();
        line.wrapped = meta.wrapped;
    }

    /// Row `r`'s marks ([`Line::marks`]), read from its extras without
    /// decoding the row: empty for a row with none.
    pub(crate) fn marks(&self, r: usize) -> &[Mark] {
        self.extra(r).map_or(&[], |extra| &extra.marks)
    }

    /// The rows with marks, by ascending row: each row's index in the
    /// block, its marks and its [`Line::wrapped`]. Reads only the rows'
    /// extras and metadata (O(rows with extras)), never their cells.
    pub(crate) fn marked_rows(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, &[Mark], bool)> + Clone + '_ {
        self.extras
            .iter()
            .filter(|extra| !extra.marks.is_empty())
            .map(|extra| {
                let r = usize::from(extra.row);
                (r, extra.marks.as_slice(), self.rows[r].wrapped)
            })
    }

    /// Row `r` as a new line.
    pub(crate) fn decode(&self, r: usize) -> Line {
        let mut line = Line::default();
        self.decode_into(r, &mut line);
        line
    }

    /// Appends row `r`'s first `len` cells to a re-wrap's joined line
    /// (`joined`), its clusters and links below `len` with their columns
    /// moved to the joined indices, and its marks and anchors (when it
    /// has marks) as `(start index, marks, anchors)`. Reads the packed row
    /// directly: no [`Line`] is built.
    pub(crate) fn join_into(&self, r: usize, len: usize, sink: JoinSink<'_>) {
        let JoinSink {
            joined,
            clusters,
            links,
            row_marks,
        } = sink;
        let meta = self.rows[r];
        let base = joined.len();
        self.cells_into(r, len, joined);
        let (_, _, _, first) = self.starts(r);
        for i in first..meta.cluster_end as usize {
            let at = self.clusters[i];
            let col = usize::from(at.col);
            if col < len {
                let text = &self.cluster_text[self.cluster_start(i)..at.end as usize];
                clusters.push((base + col, text.into()));
            }
        }
        if let Some(extra) = self.extra(r) {
            for (run, link) in &extra.links {
                let end = usize::from(run.end).min(len);
                let start = usize::from(run.start);
                if start < end {
                    links.push((base + start..base + end, link.clone()));
                }
            }
            if !extra.marks.is_empty() {
                row_marks.push((base, extra.marks.clone(), extra.anchors));
            }
        }
    }

    /// Drops the links and marks of rows below `rows` (rows evicted from
    /// the front of the block), so evicted rows hold no hyperlink entity.
    pub(crate) fn release_before(&mut self, rows: usize) {
        let row = to_u16(rows);
        let k = self.extras.partition_point(|e| e.row < row);
        self.extras.drain(..k);
    }

    /// Removes and returns the newest row.
    pub(crate) fn pop(&mut self) -> Option<Line> {
        let r = self.rows.len().checked_sub(1)?;
        let line = self.decode(r);
        let (text, runs, flags, clusters) = self.starts(r);
        let cluster_text = self.cluster_start(clusters);
        self.text.truncate(text);
        self.runs.truncate(runs);
        self.flags.truncate(flags);
        self.clusters.truncate(clusters);
        self.cluster_text.truncate(cluster_text);
        if self.extras.last().is_some_and(|e| usize::from(e.row) == r) {
            self.extras.pop();
        }
        self.rows.pop();
        Some(line)
    }
}

/// Where [`Block::join_into`] appends a row: the re-wrap's joined cells,
/// clusters, link runs and row marks.
/// The used width of `cells` (up to the last non-space) when every cell is
/// unflagged ASCII in the default style; `None` otherwise.
fn plain_end(cells: &[Cell]) -> Option<usize> {
    let mut end = 0;
    for (i, cell) in cells.iter().enumerate() {
        let style = &cell.style;
        if !cell.ch.is_ascii()
            || !cell.flags.is_empty()
            || style.flags != 0
            || !matches!(style.fg, crate::Color::Default)
            || !matches!(style.bg, crate::Color::Default)
            || !matches!(style.underline, crate::Color::Default)
        {
            return None;
        }
        if cell.ch != ' ' {
            end = i + 1;
        }
    }
    Some(end)
}

/// Appends the characters of `cells`, every one ASCII, to `text`.
fn push_ascii(text: &mut String, cells: &[Cell]) {
    let mut bytes = [0u8; 64];
    for chunk in cells.chunks(bytes.len()) {
        for (byte, cell) in bytes.iter_mut().zip(chunk) {
            // Truncation is exact: the caller checked the cells are ASCII.
            *byte = u32::from(cell.ch).to_le_bytes()[0];
        }
        let ascii = std::str::from_utf8(&bytes[..chunk.len()]).expect("the cells are ASCII");
        text.push_str(ascii);
    }
}

/// Whether every cell of `cells` is ASCII, unflagged and in `style`: the
/// single-style row of [`Block::push`].
fn plain_row(cells: &[Cell], style: Style) -> bool {
    cells
        .iter()
        .all(|cell| cell.ch.is_ascii() && cell.flags.is_empty() && cell.style == style)
}

pub(crate) struct JoinSink<'a> {
    /// Joined cells.
    pub(crate) joined: &'a mut Vec<Cell>,
    /// Clusters by joined index.
    pub(crate) clusters: &'a mut Vec<(usize, Box<str>)>,
    /// Link runs by joined index.
    pub(crate) links: &'a mut Vec<(Range<usize>, Hyperlink)>,
    /// Row marks by joined index of the row's first cell.
    pub(crate) row_marks: &'a mut Vec<(usize, Vec<Mark>, MarkAnchors)>,
}

/// `n` as a `u16`; every caller's value is a column or a row of a block,
/// below 65 536 by construction.
fn to_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// `n` as a `u32`; [`Block::fits`] keeps every offset below `u32::MAX / 2`.
fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Color;

    fn round_trip(line: &Line) -> Line {
        let mut block = Block::open(0);
        let mut copy = line.clone();
        block.push(&mut copy);
        block.decode(0)
    }

    #[test]
    fn plain_row_round_trips_and_is_small() {
        let mut line = Line::blank(80, Style::DEFAULT);
        for (cell, ch) in line.cells.iter_mut().zip("hello world, packed".chars()) {
            cell.ch = ch;
        }
        line.wrapped = true;
        assert_eq!(round_trip(&line), line);
        let mut block = Block::open(0);
        for _ in 0..BLOCK_ROWS {
            block.push(&mut line.clone());
        }
        block.seal();
        let per_row = block.heap_bytes() / BLOCK_ROWS;
        assert!(per_row <= 24 + 19 + 4 + 1, "{per_row} B per row");
    }

    #[test]
    fn styled_wide_cluster_row_round_trips() {
        let mut line = Line::blank(10, Style::DEFAULT.with(Style::BOLD));
        line.cells[0] = Cell::new('\u{4e2d}', Style::DEFAULT);
        line.cells[0].flags = CellFlags::WIDE;
        line.cells[1] = Cell::spacer(Style::DEFAULT);
        line.cells[2] = Cell::new(
            'e',
            Style {
                fg: Color::Rgb(1, 2, 3),
                ..Style::DEFAULT
            },
        );
        line.cells[2].flags = CellFlags::CLUSTER;
        line.graphemes.set(2, "e\u{301}".into());
        line.cells[9] = Cell::pad_with(Style::DEFAULT);
        line.set_mark(Mark::PromptStart);
        line.set_mark_at(Mark::OutputStart, 7);
        assert_eq!(round_trip(&line), line);
        let mut block = Block::open(0);
        block.push(&mut line.clone());
        assert!(block.last_is_pad(0));
    }

    #[test]
    fn pop_restores_the_previous_rows() {
        let mut block = Block::open(0);
        let mut a = Line::blank(4, Style::DEFAULT);
        a.cells[0].ch = 'a';
        let mut b = Line::blank(4, Style::DEFAULT);
        b.cells[1] = Cell::new('\u{e9}', Style::DEFAULT.with(Style::ITALIC));
        b.graphemes.set(1, "e\u{301}".into());
        b.cells[1].flags = CellFlags::CLUSTER;
        b.set_mark(Mark::CommandStart);
        block.push(&mut a.clone());
        block.push(&mut b.clone());
        assert_eq!(block.pop(), Some(b));
        let mut c = Line::blank(4, Style::DEFAULT);
        c.cells[3].ch = 'c';
        block.push(&mut c.clone());
        assert_eq!(block.decode(0), a);
        assert_eq!(block.decode(1), c);
    }
}
