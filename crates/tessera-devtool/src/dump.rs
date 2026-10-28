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

use std::fmt::Write as _;

use tessera_grid::{Cell, CellFlags, Color, Grid, Line, Style, UnderlineStyle};

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
/// bold, `2` dim, `3` italic, the underline (`4` single, `21` double, `4:3`
/// curly, `4:4` dotted, `4:5` dashed), `5` blink, `7` inverse, `8` hidden,
/// `9` strikethrough, `53` overline, `73` superscript, `74` subscript,
/// then the foreground (`30`–`37`, `90`–`97`, `38;5;n` or
/// `38;2;r;g;b`), the background (`40`–`47`, `100`–`107`, `48;5;n` or
/// `48;2;r;g;b`) and the underline colour (`58;5;n` or `58;2;r;g;b`). A
/// default colour adds nothing. Fed back into `tessera_term::Terminal`, the
/// dump reproduces every cell's style (as SGR produces it). Every row ends with `ESC[0m` and a newline. Trailing cells equal
/// to [`Cell::BLANK`] (a space in the default style), or default-style
/// padding a wrapped wide character left, are trimmed; any other
/// cell is kept, including a space with a background colour, a foreground
/// colour or attributes, so coloured areas survive. Spacers print nothing
/// and clusters print their whole text. The cursor is never marked.
pub fn ansi_with(grid: &Grid, options: DumpOptions) -> String {
    let mut out = String::new();
    for (_, line) in rows(grid, options) {
        let end = line
            .cells
            .iter()
            .rposition(|cell| !blank_or_padding(cell))
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
/// - `flags` lists the markers `wide`, `spacer`, `cluster`, `pad` joined by
///   `|`, or `-`.
/// - `style` is `fg=… bg=… ul=… attrs=…` with colours (`ul` is the
///   underline colour) as `default`, `idx(n)` or `rgb(r,g,b)` and attributes
///   as names joined by `|` (`bold`, `dim`, `italic`, the underline shape as
///   one of `underline`, `double-underline`, `curly-underline`,
///   `dotted-underline`, `dashed-underline`, then `blink`, `inverse`,
///   `hidden`, `strikethrough`, `overline`, `superscript`, `subscript`),
///   or `-`.
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
    let underline = match style.underline_style() {
        UnderlineStyle::None => None,
        UnderlineStyle::Single => Some("4"),
        UnderlineStyle::Double => Some("21"),
        UnderlineStyle::Curly => Some("4:3"),
        UnderlineStyle::Dotted => Some("4:4"),
        UnderlineStyle::Dashed => Some("4:5"),
    };
    set(&[(Style::BOLD, "1"), (Style::DIM, "2"), (Style::ITALIC, "3")])
        .into_iter()
        .chain(underline)
        .chain(set(&[
            (Style::BLINK, "5"),
            (Style::INVERSE, "7"),
            (Style::HIDDEN, "8"),
            (Style::STRIKETHROUGH, "9"),
            (Style::OVERLINE, "53"),
            (Style::SUPERSCRIPT, "73"),
            (Style::SUBSCRIPT, "74"),
        ]))
        .map(str::to_owned)
        .chain(color_param(style.fg, 30, 90, 38))
        .chain(color_param(style.bg, 40, 100, 48))
        .chain(underline_color_param(style.underline))
        .collect()
}

/// The SGR parameter for the underline colour: `58;5;n` or `58;2;r;g;b`
/// (there is no short form for the first sixteen colours).
fn underline_color_param(color: Color) -> Option<String> {
    match color {
        Color::Default => None,
        Color::Indexed(n) => Some(format!("58;5;{n}")),
        Color::Rgb(r, g, b) => Some(format!("58;2;{r};{g};{b}")),
    }
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

/// True for a cell the ANSI dump trims at the end of a row: [`Cell::BLANK`],
/// or padding a wrapped wide character left ([`Cell::is_pad`]) in the
/// default style.
fn blank_or_padding(cell: &Cell) -> bool {
    let unmarked = Cell {
        flags: cell.flags.difference(CellFlags::PAD),
        ..*cell
    };
    unmarked == Cell::BLANK
}

/// The cell's width markers for [`cells_with`].
fn flag_names(cell: &Cell) -> String {
    let names: Vec<&str> = [
        (cell.is_wide(), "wide"),
        (cell.is_spacer(), "spacer"),
        (cell.is_cluster(), "cluster"),
        (cell.is_pad(), "pad"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    join_or_dash(&names)
}

/// `fg=… bg=… ul=… attrs=…` for [`cells_with`].
fn style_description(style: Style) -> String {
    format!(
        "fg={} bg={} ul={} attrs={}",
        color_name(style.fg),
        color_name(style.bg),
        color_name(style.underline),
        join_or_dash(&attribute_names(style))
    )
}

/// Names of the attributes set in `style`, in SGR order; the underline is
/// named by its shape.
fn attribute_names(style: Style) -> Vec<&'static str> {
    let set = |flags: &[(u16, &'static str)]| -> Vec<&'static str> {
        flags
            .iter()
            .filter_map(|&(flag, name)| style.has(flag).then_some(name))
            .collect()
    };
    let underline = match style.underline_style() {
        UnderlineStyle::None => None,
        UnderlineStyle::Single => Some("underline"),
        UnderlineStyle::Double => Some("double-underline"),
        UnderlineStyle::Curly => Some("curly-underline"),
        UnderlineStyle::Dotted => Some("dotted-underline"),
        UnderlineStyle::Dashed => Some("dashed-underline"),
    };
    set(&[
        (Style::BOLD, "bold"),
        (Style::DIM, "dim"),
        (Style::ITALIC, "italic"),
    ])
    .into_iter()
    .chain(underline)
    .chain(set(&[
        (Style::BLINK, "blink"),
        (Style::INVERSE, "inverse"),
        (Style::HIDDEN, "hidden"),
        (Style::STRIKETHROUGH, "strikethrough"),
        (Style::OVERLINE, "overline"),
        (Style::SUPERSCRIPT, "superscript"),
        (Style::SUBSCRIPT, "subscript"),
    ]))
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
            (Style::UNDERLINE | Style::DOUBLE_UNDERLINE, "21"),
            (Style::OVERLINE, "53"),
            (Style::SUPERSCRIPT, "73"),
            (Style::SUBSCRIPT, "74"),
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
    fn underline_shapes_and_colour() {
        let cases = [
            (UnderlineStyle::Single, "4", "underline"),
            (UnderlineStyle::Double, "21", "double-underline"),
            (UnderlineStyle::Curly, "4:3", "curly-underline"),
            (UnderlineStyle::Dotted, "4:4", "dotted-underline"),
            (UnderlineStyle::Dashed, "4:5", "dashed-underline"),
        ];
        for (shape, param, name) in cases {
            let style = Style::DEFAULT.with_underline_style(shape);
            assert_eq!(sgr_params(style), [param], "{shape:?}");
            assert_eq!(attribute_names(style), [name], "{shape:?}");
        }
        let mut style = Style::DEFAULT.with_underline_style(UnderlineStyle::Curly);
        style.underline = Color::Indexed(1);
        assert_eq!(sgr_params(style), ["4:3", "58;5;1"]);
        style.underline = Color::Rgb(1, 2, 3);
        style.fg = Color::Indexed(9);
        assert_eq!(sgr_params(style), ["4:3", "91", "58;2;1;2;3"]);
        assert_eq!(
            style_description(style),
            "fg=idx(9) bg=default ul=rgb(1,2,3) attrs=curly-underline"
        );
    }

    /// An ANSI dump fed into a fresh terminal reproduces every cell,
    /// underline shape and colour included.
    #[test]
    fn ansi_dump_round_trips_through_the_terminal() {
        let input = "\x1b[4:3;58;5;1mcurly\x1b[0m \x1b[4:4;58:2::1:2:3mdot\x1b[0m \
                     \x1b[1;2;3;21;5;7;8;9;38;5;200;48;2;9;8;7;58;5;12mall\x1b[0m \
                     \x1b[4:5;31;104mdash\x1b[0m \x1b[4;58;2;0;0;0mx";
        let mut original = tessera_term::Terminal::new(2, 40);
        original.advance(input.as_bytes());
        let dump = ansi(original.grid());
        assert!(dump.contains("\x1b[0;4:3;58;5;1mcurly"), "{dump:?}");
        assert!(dump.contains("\x1b[0;4:4;58;2;1;2;3mdot"), "{dump:?}");
        let mut copy = tessera_term::Terminal::new(2, 40);
        // Rows end in a newline; the copy needs a carriage return too, and
        // the last row's newline would scroll the screen.
        let replay = dump
            .strip_suffix('\n')
            .unwrap_or(&dump)
            .replace('\n', "\r\n");
        copy.advance(replay.as_bytes());
        for row in 0..2 {
            let want = &original.grid().line(row).expect("row").cells;
            let got = &copy.grid().line(row).expect("row").cells;
            assert_eq!(got, want, "row {row}\n{dump:?}");
        }
        let first = original.grid().line(0).expect("row 0").cells[0];
        assert_eq!(first.style.underline_style(), UnderlineStyle::Curly);
    }

    /// Every combination of overline, superscript and subscript (the last
    /// two only one at a time, as SGR sets them), with and without the
    /// older attributes, survives an ANSI dump fed back into a terminal.
    #[test]
    fn overline_and_vertical_align_round_trip() {
        let older = [0, Style::BOLD | Style::STRIKETHROUGH | Style::INVERSE];
        let overline = [0, Style::OVERLINE];
        let vertical = [0, Style::SUPERSCRIPT, Style::SUBSCRIPT];
        let mut styles = Vec::new();
        for flags in older {
            for over in overline {
                for align in vertical {
                    let mut style = styled(Color::Indexed(3), Color::Default, flags | over | align);
                    style = style.with_underline_style(UnderlineStyle::Curly);
                    styles.push(style);
                }
            }
        }
        let mut grid = Grid::new(1, 16);
        for style in &styles {
            grid.set_pen(*style);
            grid.put_str("x");
        }
        let dump = ansi(&grid);
        assert!(dump.contains(";9;53;74;"), "{dump:?}");
        let mut copy = tessera_term::Terminal::new(1, 16);
        copy.advance(dump.trim_end_matches('\n').as_bytes());
        let got: Vec<Style> = copy.grid().line(0).expect("row").cells[..styles.len()]
            .iter()
            .map(|cell| cell.style)
            .collect();
        assert_eq!(got, styles, "{dump:?}");
        assert_eq!(
            attribute_names(Style::DEFAULT.with(Style::OVERLINE | Style::SUBSCRIPT)),
            ["overline", "subscript"]
        );
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
            "fg=idx(200) bg=rgb(9,8,7) ul=default attrs=italic"
        );
        assert_eq!(
            style_description(Style::DEFAULT),
            "fg=default bg=default ul=default attrs=-"
        );
        assert_eq!(flag_names(&Cell::BLANK), "-");
    }

    /// The column a wrapped wide character leaves is padding: the ANSI
    /// dump trims it like a blank and the cell dump names it.
    #[test]
    fn padding_is_trimmed_and_named() {
        let mut grid = Grid::new(2, 5);
        grid.put_str("abcd中");
        let ansi = ansi_with(&grid, DumpOptions::default());
        assert!(ansi.starts_with("abcd\x1b[0m\n"), "{ansi:?}");
        let cells = cells_with(&grid, DumpOptions::default());
        assert!(cells.contains(":4 \" \" pad "), "{cells}");
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
