//! Grid formatters: turn a [`Grid`] into text a person, a terminal or a test
//! can read.
//!
//! Three formats, each a pure function of the grid:
//!
//! - [`ansi`]: the screen re-encoded with SGR sequences, for viewing in a
//!   real terminal. Every style change re-emits the whole style after a
//!   reset (`ESC[0;…m`), and every row ends with `ESC[0m`, so a row can be
//!   copied on its own.
//! - [`text`]: the plain screen, one line per row: wide characters once,
//!   grapheme clusters whole, trailing blanks trimmed and `\` after a row
//!   that soft-wraps onto the next. These are the rules of
//!   `render_screen` in `tessera-term`'s test helpers, so a dump can be
//!   compared with an emulator snapshot.
//! - [`cells`]: one line per cell with its text, width markers and style,
//!   for debugging what a renderer would see.
//!
//! The `*_with` variants take [`DumpOptions`] to prepend the scrollback history
//! or mark the cursor; [`render`] picks a format at run time.
//!
//! ```
//! use tessera_grid::{Color, Grid, Style};
//!
//! let mut grid = Grid::new(2, 8);
//! grid.set_pen(Style { fg: Color::Indexed(1), ..Style::DEFAULT });
//! grid.put_str("hi");
//! assert_eq!(tessera_devtool::dump::text(&grid), "hi\n\n");
//! assert_eq!(tessera_devtool::dump::ansi(&grid), "\x1b[0;31mhi\x1b[0m\n\x1b[0m\n");
//! ```

// TODO(#7): when `Style` gains `underline: Color` and the curly, dotted and
// dashed underline bits, emit them as `4:3`/`4:4`/`4:5` and `58;5;n` /
// `58;2;r;g;b` in `sgr_params`, and name them in `attribute_names`.

use std::fmt::Write as _;

use tessera_grid::{Cell, Color, Grid, Line, Style};

/// How a dump is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Format {
    /// Styled text with SGR sequences ([`ansi`]).
    #[default]
    Ansi,
    /// Plain text ([`text`]).
    Text,
    /// One line per cell ([`cells`]).
    Cells,
}

/// What a dump shows besides the visible screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DumpOptions {
    /// Print the scrollback history (oldest line first) above the screen.
    pub scrollback: bool,
    /// Mark the cursor: a `|` after the cursor's cell in [`text`], a trailing
    /// `<cursor` on the cursor's line in [`cells`]. [`ansi`] ignores it, so
    /// its output stays a faithful copy of the screen.
    pub cursor: bool,
}

/// Renders `grid` in `format`.
pub fn render(grid: &Grid, format: Format, options: DumpOptions) -> String {
    match format {
        Format::Ansi => ansi_with(grid, options),
        Format::Text => text_with(grid, options),
        Format::Cells => cells_with(grid, options),
    }
}

/// The visible screen with SGR attributes; see [`ansi_with`].
pub fn ansi(grid: &Grid) -> String {
    ansi_with(grid, DumpOptions::default())
}

/// The screen (and with [`DumpOptions::scrollback`] the history above it) as
/// text with SGR sequences, one line per row.
///
/// Each row starts in the default style. Before a cell whose style differs
/// from the previous cell's, the dump writes `ESC[0;<params>m` (or `ESC[0m`
/// for the default style), where the parameters are, in this order: `1`
/// bold, `2` dim, `3` italic, `4` underline or `21` double underline, `5`
/// blink, `7` inverse, `8` hidden, `9` strikethrough, then the foreground
/// (`30`–`37`, `90`–`97`, `38;5;n` or `38;2;r;g;b`) and the background
/// (`40`–`47`, `100`–`107`, `48;5;n` or `48;2;r;g;b`). A default colour adds
/// nothing. Every row ends with `ESC[0m` and a newline. Trailing cells equal
/// to [`Cell::BLANK`] (a space in the default style) are trimmed; any other
/// cell is kept, including a space with a background colour, a foreground
/// colour or attributes, so coloured areas survive. Spacers print nothing
/// and clusters print their whole text. The cursor is never marked.
pub fn ansi_with(grid: &Grid, options: DumpOptions) -> String {
    let mut out = String::new();
    for (_, line) in rows(grid, options) {
        let end = line
            .cells
            .iter()
            .rposition(|cell| *cell != Cell::BLANK)
            .map_or(0, |i| i + 1);
        let mut current = Style::DEFAULT;
        for (col, cell) in line.cells[..end].iter().enumerate() {
            if cell.is_spacer() {
                continue;
            }
            if cell.style != current {
                push_sgr(&mut out, cell.style);
                current = cell.style;
            }
            out.push_str(&cell_text(line, col, cell));
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

/// The visible screen as plain text; see [`text_with`].
pub fn text(grid: &Grid) -> String {
    text_with(grid, DumpOptions::default())
}

/// The screen (and with [`DumpOptions::scrollback`] the history above it) as
/// plain text, one line per row, each ending in a newline.
///
/// A spacer renders nothing, a cluster renders its whole text, trailing
/// spaces are trimmed and a row that soft-wraps onto the next ends with
/// `\`. With [`DumpOptions::cursor`], a `|` follows the cursor's cell, and the
/// blanks before the marker are kept so it shows where the cursor sits.
pub fn text_with(grid: &Grid, options: DumpOptions) -> String {
    let cursor = grid.cursor();
    let mut out = String::new();
    for (row, line) in rows(grid, options) {
        // One piece per cell, so the cursor marker lands after the cursor's
        // cell even when a cell renders as zero or several characters.
        let mut pieces: Vec<String> = line
            .cells
            .iter()
            .enumerate()
            .map(|(col, cell)| cell_text(line, col, cell))
            .collect();
        if options.cursor && row == Row::Screen(cursor.row) {
            let at = (usize::from(cursor.col) + 1).min(pieces.len());
            pieces.insert(at, String::from('|'));
        }
        let end = pieces.iter().rposition(|p| p != " ").map_or(0, |i| i + 1);
        out.extend(pieces.drain(..end));
        if line.wrapped {
            out.push('\\');
        }
        out.push('\n');
    }
    out
}

/// One line per cell of the visible screen; see [`cells_with`].
pub fn cells(grid: &Grid) -> String {
    cells_with(grid, DumpOptions::default())
}

/// One line per cell: `row:col text flags style`.
///
/// - `row` is the screen row from 0; scrollback rows (with
///   [`DumpOptions::scrollback`]) are negative, `-1` being the line that most
///   recently scrolled off.
/// - `text` is the cell's text as a Rust string literal (`"a"`, `"e\u{301}"`
///   for a cluster, `" "` for a spacer or blank).
/// - `flags` lists the width markers `wide`, `spacer`, `cluster` joined by
///   `|`, or `-`.
/// - `style` is `fg=… bg=… attrs=…` with colours as `default`, `idx(n)` or
///   `rgb(r,g,b)` and attributes as names joined by `|` (`bold`, `dim`,
///   `italic`, `underline`, `blink`, `inverse`, `hidden`, `strikethrough`,
///   `double-underline`), or `-`.
///
/// A line for a row that soft-wraps is followed by `row:wrapped`. With
/// [`DumpOptions::cursor`] the cursor's cell line ends with ` <cursor`.
pub fn cells_with(grid: &Grid, options: DumpOptions) -> String {
    let cursor = grid.cursor();
    let mut out = String::new();
    for (row, line) in rows(grid, options) {
        let label = row.label();
        for (col, cell) in line.cells.iter().enumerate() {
            let text = cell_text(line, col, cell);
            let text = if text.is_empty() { " " } else { &text };
            // Writing to a `String` cannot fail.
            let _ = write!(
                out,
                "{label}:{col} {text:?} {} {}",
                flag_names(cell),
                style_description(cell.style)
            );
            if options.cursor && row == Row::Screen(cursor.row) && col == usize::from(cursor.col) {
                out.push_str(" <cursor");
            }
            out.push('\n');
        }
        if line.wrapped {
            let _ = writeln!(out, "{label}:wrapped");
        }
    }
    out
}

/// Where a dumped line comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A scrollback line, `n` lines above the top of the screen (1 = the
    /// newest history line).
    History(usize),
    /// A visible row, from 0 at the top.
    Screen(u16),
}

impl Row {
    /// The row number printed by [`cells_with`].
    fn label(self) -> String {
        match self {
            Self::History(n) => format!("-{n}"),
            Self::Screen(row) => row.to_string(),
        }
    }
}

/// The lines a dump covers, top to bottom: the scrollback (oldest first)
/// when asked for, then the visible rows.
fn rows(grid: &Grid, options: DumpOptions) -> impl Iterator<Item = (Row, &Line)> {
    let history_len = if options.scrollback {
        grid.scrollback_len()
    } else {
        0
    };
    let history = (0..history_len).filter_map(move |index| {
        grid.scrollback_line(index)
            .map(|line| (Row::History(history_len - index), line))
    });
    let screen = (0..grid.rows())
        .zip(grid.visible_lines())
        .map(|(row, line)| (Row::Screen(row), line));
    history.chain(screen)
}

/// What a cell shows: nothing for a spacer, the cluster for a cluster
/// cell, otherwise its character.
fn cell_text(line: &Line, col: usize, cell: &Cell) -> String {
    if cell.is_spacer() {
        return String::new();
    }
    match line.cluster(col) {
        Some(cluster) if cell.is_cluster() => cluster.to_owned(),
        _ => String::from(cell.ch),
    }
}

/// Appends the SGR sequence that switches a terminal from any state to
/// `style`: a reset followed by the style's parameters.
fn push_sgr(out: &mut String, style: Style) {
    out.push_str("\x1b[0");
    for param in sgr_params(style) {
        out.push(';');
        out.push_str(&param);
    }
    out.push('m');
}

/// SGR parameters that set `style` on top of the default style, in the
/// order documented on [`ansi_with`].
fn sgr_params(style: Style) -> Vec<String> {
    let set = |flags: &[(u16, &'static str)]| -> Vec<&'static str> {
        flags
            .iter()
            .filter_map(|&(flag, param)| style.has(flag).then_some(param))
            .collect()
    };
    // Double is checked first: once #7 lands a double underline also
    // carries `UNDERLINE`, and `21` on its own already means "double".
    let underline = if style.has(Style::DOUBLE_UNDERLINE) {
        Some("21")
    } else if style.has(Style::UNDERLINE) {
        Some("4")
    } else {
        None
    };
    set(&[(Style::BOLD, "1"), (Style::DIM, "2"), (Style::ITALIC, "3")])
        .into_iter()
        .chain(underline)
        .chain(set(&[
            (Style::BLINK, "5"),
            (Style::INVERSE, "7"),
            (Style::HIDDEN, "8"),
            (Style::STRIKETHROUGH, "9"),
        ]))
        .map(str::to_owned)
        .chain(color_param(style.fg, 30, 90, 38))
        .chain(color_param(style.bg, 40, 100, 48))
        .collect()
}

/// The SGR parameter for a colour slot: `base + n` for the eight standard
/// colours, `bright + n - 8` for the eight bright ones, `extended;5;n` for
/// the rest of the palette and `extended;2;r;g;b` for true colour.
fn color_param(color: Color, base: u8, bright: u8, extended: u8) -> Option<String> {
    match color {
        Color::Default => None,
        Color::Indexed(n @ 0..8) => Some((base + n).to_string()),
        Color::Indexed(n @ 8..16) => Some((bright + n - 8).to_string()),
        Color::Indexed(n) => Some(format!("{extended};5;{n}")),
        Color::Rgb(r, g, b) => Some(format!("{extended};2;{r};{g};{b}")),
    }
}

/// The cell's width markers for [`cells_with`].
fn flag_names(cell: &Cell) -> String {
    let names: Vec<&str> = [
        (cell.is_wide(), "wide"),
        (cell.is_spacer(), "spacer"),
        (cell.is_cluster(), "cluster"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    join_or_dash(&names)
}

/// `fg=… bg=… attrs=…` for [`cells_with`].
fn style_description(style: Style) -> String {
    format!(
        "fg={} bg={} attrs={}",
        color_name(style.fg),
        color_name(style.bg),
        join_or_dash(&attribute_names(style))
    )
}

/// Names of the attributes set in `style`, in SGR order.
fn attribute_names(style: Style) -> Vec<&'static str> {
    [
        (Style::BOLD, "bold"),
        (Style::DIM, "dim"),
        (Style::ITALIC, "italic"),
        (Style::UNDERLINE, "underline"),
        (Style::BLINK, "blink"),
        (Style::INVERSE, "inverse"),
        (Style::HIDDEN, "hidden"),
        (Style::STRIKETHROUGH, "strikethrough"),
        (Style::DOUBLE_UNDERLINE, "double-underline"),
    ]
    .into_iter()
    .filter_map(|(flag, name)| style.has(flag).then_some(name))
    .collect()
}

/// `default`, `idx(n)` or `rgb(r,g,b)`.
fn color_name(color: Color) -> String {
    match color {
        Color::Default => "default".to_owned(),
        Color::Indexed(n) => format!("idx({n})"),
        Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
    }
}

/// `names` joined by `|`, or `-` when empty.
fn join_or_dash(names: &[&str]) -> String {
    if names.is_empty() {
        "-".to_owned()
    } else {
        names.join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Field by field rather than a literal, so the helper compiles
    /// whether or not `Style` has gained fields (#7).
    fn styled(fg: Color, bg: Color, flags: u16) -> Style {
        let mut style = Style::DEFAULT;
        style.fg = fg;
        style.bg = bg;
        style.flags = flags;
        style
    }

    #[test]
    fn default_style_has_no_parameters() {
        assert_eq!(sgr_params(Style::DEFAULT), Vec::<String>::new());
        let mut out = String::new();
        push_sgr(&mut out, Style::DEFAULT);
        assert_eq!(out, "\x1b[0m");
    }

    #[test]
    fn colors_use_the_shortest_form() {
        let cases = [
            (Color::Indexed(0), "30", "40"),
            (Color::Indexed(7), "37", "47"),
            (Color::Indexed(8), "90", "100"),
            (Color::Indexed(15), "97", "107"),
            (Color::Indexed(16), "38;5;16", "48;5;16"),
            (Color::Indexed(255), "38;5;255", "48;5;255"),
            (Color::Rgb(1, 2, 3), "38;2;1;2;3", "48;2;1;2;3"),
        ];
        for (color, fg, bg) in cases {
            assert_eq!(
                sgr_params(styled(color, Color::Default, 0)),
                [fg],
                "fg {color:?}"
            );
            assert_eq!(
                sgr_params(styled(Color::Default, color, 0)),
                [bg],
                "bg {color:?}"
            );
        }
    }

    #[test]
    fn every_attribute_maps_to_its_sgr_number() {
        let cases = [
            (Style::BOLD, "1"),
            (Style::DIM, "2"),
            (Style::ITALIC, "3"),
            (Style::UNDERLINE, "4"),
            (Style::BLINK, "5"),
            (Style::INVERSE, "7"),
            (Style::HIDDEN, "8"),
            (Style::STRIKETHROUGH, "9"),
            (Style::DOUBLE_UNDERLINE, "21"),
        ];
        for (flag, param) in cases {
            assert_eq!(
                sgr_params(styled(Color::Default, Color::Default, flag)),
                [param],
                "flag {flag:#x}"
            );
        }
    }

    #[test]
    fn double_underline_wins_over_single() {
        let style = styled(
            Color::Default,
            Color::Default,
            Style::UNDERLINE | Style::DOUBLE_UNDERLINE,
        );
        assert_eq!(sgr_params(style), ["21"]);
    }

    #[test]
    fn attributes_precede_colors() {
        let style = styled(
            Color::Indexed(2),
            Color::Rgb(0, 0, 0),
            Style::STRIKETHROUGH | Style::BOLD,
        );
        assert_eq!(sgr_params(style), ["1", "9", "32", "48;2;0;0;0"]);
    }

    #[test]
    fn cell_description_names_markers_and_style() {
        let style = styled(Color::Indexed(200), Color::Rgb(9, 8, 7), Style::ITALIC);
        assert_eq!(
            style_description(style),
            "fg=idx(200) bg=rgb(9,8,7) attrs=italic"
        );
        assert_eq!(
            style_description(Style::DEFAULT),
            "fg=default bg=default attrs=-"
        );
        assert_eq!(flag_names(&Cell::BLANK), "-");
    }

    #[test]
    fn history_rows_count_up_from_the_screen() {
        let mut grid = Grid::new(2, 4);
        grid.put_str("a");
        for _ in 0..3 {
            grid.line_feed();
        }
        let labels: Vec<Row> = rows(
            &grid,
            DumpOptions {
                scrollback: true,
                cursor: false,
            },
        )
        .map(|(row, _)| row)
        .collect();
        let screen_only: Vec<Row> = rows(&grid, DumpOptions::default())
            .map(|(row, _)| row)
            .collect();
        assert_eq!(grid.scrollback_len(), 2);
        assert_eq!(
            labels,
            [
                Row::History(2),
                Row::History(1),
                Row::Screen(0),
                Row::Screen(1)
            ]
        );
        assert_eq!(screen_only, [Row::Screen(0), Row::Screen(1)]);
    }
}
