//! Golden images of the grid passes, rendered headless at 2x with Menlo
//! 13 pt into an `Rgba8UnormSrgb` [`OffscreenTarget`].
//!
//! - `screen.png`: every ANSI foreground on every ANSI background, cube
//!   and grey samples, and rows of bold, dim, italic, underline, double
//!   underline, strikethrough, inverse and hidden text, with a block
//!   cursor. Compared with the font tolerance (see [`Tolerance::Font`]).
//! - `cursors.png`: the three cursor shapes over text, focused and then
//!   unfocused, one strip each. Font tolerance.
//! - `clusters.png`: grapheme clusters drawn whole: `e` + U+0301, a ZWJ
//!   family, a VS16 heart, a skin-toned hand, `a` + U+0338. Font
//!   tolerance.
//! - `geometry.png`: the same screen and strips with every character
//!   replaced by a space, so only backgrounds, decorations and cursors
//!   remain. Must match exactly: color and geometry bugs cannot hide
//!   behind the font tolerance.
//! - `selection_linear.png`, `selection_block.png`, `overlays.png`,
//!   `underlines.png`, `scripts.png` and `cursor_wide.png` (#22): the
//!   selection, search and link overlays, the underline styles and
//!   colour, overline, superscript and subscript, and the block cursor on
//!   wide characters, ligatures, emoji and decorated text. Font
//!   tolerance.
//!
//! `TESSERA_UPDATE_GOLDENS=1` rewrites the goldens instead of comparing;
//! `TESSERA_SKIP_GPU_TESTS=1` skips the suite like `headless_clear.rs`.
//! On a mismatch the rendered image is written next to the build's
//! temporary files and its path printed. See `tests/golden/README.md`.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel and cell counts in these tests are small and non-negative"
)]

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use tessera_grid::{Cell, CellFlags, Color, Point, Style};
use tessera_render::{
    CursorShape, CursorState, FontConfig, FrameInput, Gpu, OffscreenTarget, Overlay, OverlayKind,
    PhysicalSize, Renderer, Rgba, Selection, Shaper, Theme,
};

const SKIP_VAR: &str = "TESSERA_SKIP_GPU_TESTS";
const UPDATE_VAR: &str = "TESSERA_UPDATE_GOLDENS";
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const COLS: usize = 40;
const ROWS: usize = 12;

/// Brings up the headless GPU, or `None` only when the opt-out is set.
fn gpu() -> Option<Gpu> {
    if std::env::var_os(SKIP_VAR).is_some() {
        eprintln!("skipping: {SKIP_VAR} is set");
        return None;
    }
    Some(Gpu::headless().unwrap_or_else(|e| {
        panic!("no GPU adapter available ({e}); set {SKIP_VAR}=1 to skip the GPU tests")
    }))
}

fn renderer(gpu: &Gpu, theme: Theme) -> Renderer {
    let renderer = renderer_with(gpu, theme, "Menlo").expect("Menlo ships with macOS");
    assert_eq!(
        renderer.shaper().fonts().family(),
        "Menlo",
        "the goldens are drawn with Menlo"
    );
    renderer
}

/// A renderer for `family` at 13 pt, 2x, or `None` when the family is not
/// installed.
fn renderer_with(gpu: &Gpu, theme: Theme, family: &str) -> Option<Renderer> {
    let shaper = Shaper::new(&FontConfig {
        family: family.into(),
        size_pt: 13.0,
        scale: 2.0,
        fallback: Vec::new(),
    })
    .expect("fonts");
    if shaper.fonts().used_default_family() {
        return None;
    }
    Some(Renderer::new(gpu, theme, shaper).expect("renderer"))
}

/// The target size for `cols` x `rows` cells plus the theme padding.
fn target_size(renderer: &Renderer, cols: usize, rows: usize) -> PhysicalSize {
    let m = renderer.cell_metrics();
    let pad = (renderer.theme().padding * renderer.shaper().scale()).round() as u32;
    PhysicalSize::new(
        m.width_px * cols as u32 + 2 * pad,
        m.height_px * rows as u32 + 2 * pad,
    )
}

/// Renders `frame` into a fresh target and reads it back as RGBA bytes.
fn render(
    gpu: &Gpu,
    renderer: &mut Renderer,
    size: PhysicalSize,
    frame: &FrameInput<'_>,
) -> Vec<u8> {
    let target = OffscreenTarget::new(gpu, size, FORMAT);
    target.render(gpu, renderer, frame).expect("render");
    target.read_back(gpu).expect("readback")
}

fn styled(text: &str, style: Style) -> Vec<Cell> {
    text.chars().map(|ch| Cell::new(ch, style)).collect()
}

fn fg_bg(fg: u8, bg: u8) -> Style {
    Style {
        fg: Color::Indexed(fg),
        bg: Color::Indexed(bg),
        flags: 0,
        ..Style::DEFAULT
    }
}

/// A row padded with blank cells to [`COLS`].
fn padded(mut row: Vec<Cell>) -> Vec<Cell> {
    row.resize(COLS, Cell::BLANK);
    row.truncate(COLS);
    row
}

/// The test screen: 40 x 12 cells.
fn screen() -> Vec<Vec<Cell>> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut rows = Vec::with_capacity(ROWS);
    // Rows 0..8: two backgrounds per row, each with all 16 foregrounds,
    // then five samples of the color cube.
    for r in 0..8u8 {
        let mut row = Vec::new();
        for bg in [2 * r, 2 * r + 1] {
            row.extend(
                (0..16u8).map(|fg| Cell::new(char::from(HEX[usize::from(fg)]), fg_bg(fg, bg))),
            );
            row.push(Cell::BLANK);
        }
        row.extend((0..5u8).map(|k| {
            let style = Style {
                bg: Color::Indexed(16 + 27 * r + 5 * k),
                ..Style::DEFAULT
            };
            Cell::new(' ', style)
        }));
        rows.push(padded(row));
    }
    let mut attrs = |parts: &[(&str, Style)]| {
        let mut row = Vec::new();
        for (text, style) in parts {
            row.extend(styled(text, *style));
            row.push(Cell::BLANK);
        }
        rows.push(padded(row));
    };
    let d = Style::DEFAULT;
    let red = Style {
        fg: Color::Indexed(1),
        ..d
    };
    attrs(&[
        ("bold", d.with(Style::BOLD)),
        ("dim", d.with(Style::DIM)),
        ("italic", d.with(Style::ITALIC)),
        ("both", d.with(Style::BOLD | Style::ITALIC)),
        ("red", red.with(Style::BOLD)),
        ("dimred", red.with(Style::DIM)),
    ]);
    attrs(&[
        ("under", d.with(Style::UNDERLINE)),
        ("double", d.with(Style::UNDERLINE | Style::DOUBLE_UNDERLINE)),
        ("strike", d.with(Style::STRIKETHROUGH)),
        ("u+s", red.with(Style::UNDERLINE | Style::STRIKETHROUGH)),
        ("  gap  ", d.with(Style::UNDERLINE)),
    ]);
    attrs(&[
        ("inverse", d.with(Style::INVERSE)),
        ("hidden", d.with(Style::HIDDEN | Style::UNDERLINE)),
        ("invred", red.with(Style::INVERSE)),
        (
            "rgb",
            Style {
                fg: Color::Rgb(255, 160, 0),
                bg: Color::Rgb(0, 60, 120),
                flags: 0,
                ..Style::DEFAULT
            },
        ),
        ("─┼─╮", d),
    ]);
    attrs(&[("cursor on X:", d), ("X", d)]);
    rows
}

/// The cursor in [`screen`]: on the `X` of the last row.
const SCREEN_CURSOR: CursorState = CursorState::new(11, 13, CursorShape::Block);

/// One strip per cursor shape, focused then unfocused, over `ab`.
fn cursor_strips() -> Vec<(Vec<Cell>, CursorState, bool)> {
    let shapes = [
        CursorShape::Block,
        CursorShape::Beam,
        CursorShape::Underline,
    ];
    [true, false]
        .into_iter()
        .flat_map(|focused| {
            shapes.into_iter().map(move |shape| {
                let mut row = styled("shape ab", Style::DEFAULT);
                row.extend(styled("cd", Style::DEFAULT.with(Style::UNDERLINE)));
                (padded(row), CursorState::new(0, 7, shape), focused)
            })
        })
        .collect()
}

/// `rows` with every character replaced by a space.
fn blanked(rows: &[Vec<Cell>]) -> Vec<Vec<Cell>> {
    rows.iter()
        .map(|row| row.iter().map(|c| Cell::new(' ', c.style)).collect())
        .collect()
}

/// Renders the screen, and the strips stacked under it when `strips` is
/// given, into one RGBA image.
fn compose(
    gpu: &Gpu,
    renderer: &mut Renderer,
    screen: Option<&[Vec<Cell>]>,
    strips: &[(Vec<Cell>, CursorState, bool)],
) -> (PhysicalSize, Vec<u8>) {
    let mut image = Vec::new();
    let mut height = 0;
    let width = target_size(renderer, COLS, 1).width;
    if let Some(screen) = screen {
        let rows: Vec<&[Cell]> = screen.iter().map(Vec::as_slice).collect();
        let size = target_size(renderer, COLS, ROWS);
        let frame = FrameInput::new(&rows).with_cursor(Some(SCREEN_CURSOR));
        image.extend(render(gpu, renderer, size, &frame));
        height += size.height;
    }
    for (row, cursor, focused) in strips {
        let rows: [&[Cell]; 1] = [row];
        let size = target_size(renderer, COLS, 1);
        let frame = FrameInput::new(&rows)
            .with_cursor(Some(*cursor))
            .with_focus(*focused);
        image.extend(render(gpu, renderer, size, &frame));
        height += size.height;
    }
    (PhysicalSize::new(width, height), image)
}

/// How far a rendering may stray from its golden.
#[derive(Debug, Clone, Copy)]
enum Tolerance {
    /// In every [`TILE`] x [`TILE`] tile at most [`TILE_PIXELS`] pixels,
    /// and in the whole image at most 0.5% of the pixels, may differ by
    /// more than 16/255 in any channel. That absorbs anti-aliasing noise in
    /// glyph edges across macOS versions, while any change to one glyph's
    /// color, style or presence (dozens of pixels in one cell) fails.
    Font,
    /// Every byte must match.
    Exact,
}

/// Side of the tiles the font tolerance is applied to, in pixels: a
/// cell at 2x is 16 x 30, so a glyph spans two to four tiles.
const TILE: u32 = 16;

/// Pixels per tile that may exceed the threshold under the font tolerance.
const TILE_PIXELS: usize = 4;

/// The tile with the most pixels over `threshold`: `(x, y, count)`.
fn worst_tile(a: &[u8], b: &[u8], size: PhysicalSize, threshold: u8) -> (u32, u32, usize) {
    let tiles_x = size.width.div_ceil(TILE);
    let tiles_y = size.height.div_ceil(TILE);
    let mut counts = vec![0usize; (tiles_x * tiles_y) as usize];
    let pixels = a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0);
    for (i, (p, q)) in pixels.enumerate() {
        if p.iter().zip(q).any(|(x, y)| x.abs_diff(*y) > threshold) {
            let (x, y) = (i as u32 % size.width, i as u32 / size.width);
            counts[((y / TILE) * tiles_x + x / TILE) as usize] += 1;
        }
    }
    let (worst, count) = counts
        .iter()
        .enumerate()
        .max_by_key(|&(_, c)| *c)
        .map_or((0, 0), |(i, c)| (i as u32, *c));
    ((worst % tiles_x) * TILE, (worst / tiles_x) * TILE, count)
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn write_png(path: &Path, size: PhysicalSize, rgba: &[u8]) {
    let file = File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    let mut encoder = png::Encoder::new(BufWriter::new(file), size.width, size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(rgba).expect("png data");
}

fn read_png(path: &Path) -> Option<(PhysicalSize, Vec<u8>)> {
    let file = File::open(path).ok()?;
    let mut reader = png::Decoder::new(BufReader::new(file))
        .read_info()
        .expect("decodable golden");
    let mut buf = vec![0; reader.output_buffer_size().expect("buffer size")];
    let info = reader.next_frame(&mut buf).expect("golden frame");
    assert_eq!(
        (info.color_type, info.bit_depth),
        (png::ColorType::Rgba, png::BitDepth::Eight),
        "{} must be 8-bit RGBA",
        path.display()
    );
    buf.truncate(info.buffer_size());
    Some((PhysicalSize::new(info.width, info.height), buf))
}

/// Pixels whose largest channel difference exceeds `threshold`.
fn differing_pixels(a: &[u8], b: &[u8], threshold: u8) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p.iter().zip(*q).any(|(x, y)| x.abs_diff(*y) > threshold))
        .count()
}

/// Compares `rgba` with `tests/golden/<name>.png`, or rewrites the
/// golden when [`UPDATE_VAR`] is set.
fn check_golden(name: &str, size: PhysicalSize, rgba: &[u8], tolerance: Tolerance) {
    let path = golden_dir().join(format!("{name}.png"));
    if std::env::var_os(UPDATE_VAR).is_some() {
        write_png(&path, size, rgba);
        eprintln!("updated {}", path.display());
        return;
    }
    let Some((golden_size, golden)) = read_png(&path) else {
        panic!(
            "missing golden {}; run with {UPDATE_VAR}=1 to create it",
            path.display()
        );
    };
    let actual = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.actual.png"));
    if golden_size != size {
        write_png(&actual, size, rgba);
        panic!(
            "{name}: rendered {size:?}, golden is {golden_size:?}; actual image at {}",
            actual.display()
        );
    }
    let pixels = (size.width * size.height) as usize;
    let (threshold, allowed) = match tolerance {
        Tolerance::Font => (16, pixels / 200),
        Tolerance::Exact => (0, 0),
    };
    let differing = differing_pixels(rgba, &golden, threshold);
    if let Tolerance::Font = tolerance {
        let (x, y, count) = worst_tile(rgba, &golden, size, threshold);
        if count > TILE_PIXELS {
            write_png(&actual, size, rgba);
            panic!(
                "{name}: {count} pixels of the {TILE}x{TILE} tile at ({x}, {y}) differ by \
                 more than {threshold}/255 (allowed {TILE_PIXELS} per tile); actual image \
                 at {}. If the change is intended, rerun with {UPDATE_VAR}=1 and review \
                 the new golden.",
                actual.display()
            );
        }
    }
    if differing > allowed {
        write_png(&actual, size, rgba);
        panic!(
            "{name}: {differing} of {pixels} pixels differ by more than {threshold}/255 \
             (allowed {allowed}); actual image at {}. If the change is intended, rerun \
             with {UPDATE_VAR}=1 and review the new golden.",
            actual.display()
        );
    }
}

#[test]
fn screen_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let (size, image) = compose(&gpu, &mut renderer, Some(&screen()), &[]);
    check_golden("screen", size, &image, Tolerance::Font);
}

#[test]
fn cursors_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let (size, image) = compose(&gpu, &mut renderer, None, &cursor_strips());
    check_golden("cursors", size, &image, Tolerance::Font);
}

#[test]
fn geometry_golden_matches_exactly() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let strips: Vec<_> = cursor_strips()
        .into_iter()
        .map(|(row, cursor, focused)| (blanked(&[row]).remove(0), cursor, focused))
        .collect();
    let (size, image) = compose(&gpu, &mut renderer, Some(&blanked(&screen())), &strips);
    check_golden("geometry", size, &image, Tolerance::Exact);
}

/// The cluster row of `clusters.png`, as the grid lays it out: `e` +
/// U+0301 (one cell), a ZWJ family (two cells), a VS16 heart, a hand with
/// a skin tone and `a` + U+0338 (a mark Menlo cannot precompose), each
/// followed by a plain `x` and a space.
fn cluster_line() -> tessera_grid::Line {
    let mut grid = tessera_grid::Grid::new(1, 24);
    grid.put_str(
        "e\u{301}x \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}x \
         \u{2764}\u{FE0F}x \u{1F44D}\u{1F3FD}x a\u{338}x",
    );
    grid.line(0).expect("row 0").clone()
}

/// Full-cluster shaping (#21): each cluster is drawn whole, centred in its
/// cell or cells. Font tolerance, so drawing only the base character
/// (`e`, the man, a text heart, a yellow hand) fails: each puts dozens of
/// differing pixels into one tile.
#[test]
fn clusters_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let line = cluster_line();
    let rows: [&[Cell]; 1] = [&line.cells];
    let clusters = [line.graphemes.clone()];
    let size = target_size(&renderer, 24, 1);
    let frame = FrameInput::new(&rows).with_clusters(&clusters);
    let image = render(&gpu, &mut renderer, size, &frame);
    // Without the cluster table only base characters are drawn: the
    // image must differ, or the golden would not be testing anything.
    let base_only = render(&gpu, &mut renderer, size, &FrameInput::new(&rows));
    assert!(
        differing_pixels(&image, &base_only, 16) > 400,
        "clusters change the row"
    );
    check_golden("clusters", size, &image, Tolerance::Font);
    // A selection on the last cell of the VS16 heart (its spacer when the
    // heart is wide) takes the whole cluster and changes nothing outside
    // it: the cluster is not split or re-shaped (ADR 0016 open item,
    // closed by #22). The heart is red, so its colour survives the check.
    let heart = line
        .cells
        .iter()
        .position(|c| c.ch == '\u{2764}')
        .expect("heart");
    let width = if line.cells[heart].is_wide() { 2 } else { 1 };
    let last = (heart + width - 1) as u16;
    let selection = Selection::linear(Point::new(0, last), Point::new(0, last));
    let selected = render(
        &gpu,
        &mut renderer,
        size,
        &frame.clone().with_selection(Some(selection)),
    );
    let m = renderer.cell_metrics();
    let pad = (renderer.theme().padding * renderer.shaper().scale()).round() as u32;
    let x0 = pad + heart as u32 * m.width_px;
    let x1 = x0 + width as u32 * m.width_px;
    let outside = |img: &[u8]| -> Vec<u8> {
        img.as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(i, _)| !(x0..x1).contains(&(*i as u32 % size.width)))
            .flat_map(|(_, p)| *p)
            .collect()
    };
    assert!(
        outside(&selected) == outside(&image),
        "only the cluster's cells change"
    );
    let inside = |img: &[u8]| -> usize {
        img.as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                (x0..x1).contains(&(*i as u32 % size.width))
                    && (p[0].abs_diff(p[1]) > 40 || p[1].abs_diff(p[2]) > 40)
            })
            .count()
    };
    let (selected_ink, plain_ink) = (inside(&selected), inside(&image));
    assert!(
        plain_ink > 100 && selected_ink.abs_diff(plain_ink) * 10 < plain_ink,
        "the heart is still drawn whole: {selected_ink} vs {plain_ink} colourful pixels"
    );
}

/// Lines laid out by the grid (`cols` wide, text wrapping as it would in
/// the terminal). A `\n` in the text starts the next row, and each `@@`
/// becomes a wide character: Menlo's `W` with the WIDE flag and its
/// SPACER. The goldens then need no fallback font (CJK and emoji come from
/// Hiragino, Apple SD Gothic Neo and Apple Color Emoji, which differ
/// between macOS versions), while the renderer sees exactly the cells a
/// real wide character gives it.
fn grid_lines(text: &[(&str, Style)], rows: u16, cols: u16) -> Vec<tessera_grid::Line> {
    let mut grid = tessera_grid::Grid::new(rows, cols);
    for (text, style) in text {
        grid.set_pen(*style);
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                let row = grid.cursor().row + 1;
                grid.goto(row, 0);
            }
            grid.put_str(piece);
        }
    }
    let mut lines = grid.visible_lines().to_vec();
    for line in &mut lines {
        widen(&mut line.cells);
    }
    lines
}

/// Turns each pair of `@` cells into a wide `W` and its spacer.
fn widen(cells: &mut [Cell]) {
    let mut c = 0;
    while c + 1 < cells.len() {
        if cells[c].ch == '@' && cells[c + 1].ch == '@' {
            let style = cells[c].style;
            cells[c] = Cell {
                flags: CellFlags::WIDE,
                ..Cell::new('W', style)
            };
            cells[c + 1] = Cell {
                flags: CellFlags::SPACER,
                ..Cell::new(' ', style)
            };
            c += 2;
        } else {
            c += 1;
        }
    }
}

/// Renders `lines` (`cols` wide) with `selection` and `overlays`.
fn render_lines(
    gpu: &Gpu,
    renderer: &mut Renderer,
    lines: &[tessera_grid::Line],
    cols: usize,
    (selection, overlays): (Option<Selection>, &[Overlay]),
) -> (PhysicalSize, Vec<u8>) {
    let rows: Vec<&[Cell]> = lines.iter().map(|l| l.cells.as_slice()).collect();
    let clusters: Vec<_> = lines.iter().map(|l| l.graphemes.clone()).collect();
    let size = target_size(renderer, cols, rows.len());
    let frame = FrameInput::new(&rows)
        .with_clusters(&clusters)
        .with_selection(selection)
        .with_overlays(overlays);
    (size, render(gpu, renderer, size, &frame))
}

/// A linear selection over three rows of one wrapped line: it starts on
/// the right half of a wide character (taking the whole character), runs
/// over the whole wrapped middle row and ends on the left half of another
/// (taking its right half too).
#[test]
fn selection_linear_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let text = "linear: a@@b crosses a wrapped row of text into @@@@ and stops";
    let lines = grid_lines(&[(text, Style::DEFAULT)], 3, 24);
    assert!(lines[0].wrapped && lines[1].wrapped, "one wrapped line");
    let start = lines[0]
        .cells
        .iter()
        .position(Cell::is_spacer)
        .expect("a spacer in row 0");
    let end = lines[2]
        .cells
        .iter()
        .rposition(Cell::is_wide)
        .expect("a wide cell in row 2");
    let selection = Selection::linear(Point::new(0, start as u16), Point::new(2, end as u16));
    let (size, image) = render_lines(&gpu, &mut renderer, &lines, 24, (Some(selection), &[]));
    check_golden("selection_linear", size, &image, Tolerance::Font);
}

/// A block selection over mixed-width text: its left and right edges cut
/// through wide characters on some rows, which are then taken whole.
#[test]
fn selection_block_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let lines = grid_lines(
        &[
            ("block @@@@ selection\n", Style::DEFAULT),
            ("over @@@@@@@@ mixed\n", Style::DEFAULT),
            ("width ascii only\n", Style::DEFAULT),
            ("text @@@@@@ rows", Style::DEFAULT),
        ],
        4,
        24,
    );
    let selection = Selection::block(Point::new(3, 7), Point::new(0, 3));
    let (size, image) = render_lines(&gpu, &mut renderer, &lines, 24, (Some(selection), &[]));
    check_golden("selection_block", size, &image, Tolerance::Font);
}

/// Search matches, the current match and a hovered link underline on one
/// screen, one match over an explicit background and one under a
/// selection.
#[test]
fn overlays_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let blue = Style {
        bg: Color::Indexed(4),
        ..Style::DEFAULT
    };
    let link = Style {
        fg: Color::Indexed(6),
        ..Style::DEFAULT
    };
    let lines = grid_lines(
        &[
            ("find a match, the ", Style::DEFAULT),
            ("match", blue),
            ("\nand the match here\n", Style::DEFAULT),
            ("see https://example.org", link),
        ],
        3,
        24,
    );
    let overlays = [
        Overlay::new(OverlayKind::Match, 0, 7..12),
        Overlay::new(OverlayKind::Match, 0, 18..23),
        Overlay::new(OverlayKind::CurrentMatch, 1, 8..13),
        Overlay::new(OverlayKind::Underline, 2, 4..23),
    ];
    let selection = Selection::linear(Point::new(1, 4), Point::new(1, 10));
    let (size, image) = render_lines(
        &gpu,
        &mut renderer,
        &lines,
        24,
        (Some(selection), &overlays),
    );
    check_golden("overlays", size, &image, Tolerance::Font);
}

/// The five underline styles, each in the foreground and in an SGR 58
/// colour.
#[test]
fn underlines_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let u = Style::UNDERLINE;
    let styles = [
        ("single", u),
        ("double", u | Style::DOUBLE_UNDERLINE),
        ("curly", u | Style::CURLY_UNDERLINE),
        ("dotted", u | Style::DOTTED_UNDERLINE),
        ("dashed", u | Style::DASHED_UNDERLINE),
    ];
    let mut text = Vec::new();
    let labels: Vec<String> = styles
        .iter()
        .flat_map(|(name, _)| [format!("{name:<11}"), format!("{name:<11}\n")])
        .collect();
    for (i, (_, flags)) in styles.iter().enumerate() {
        let plain = Style::DEFAULT.with(*flags);
        let colored = Style {
            underline: Color::Rgb(0xff, 0x55, 0x55),
            ..plain
        };
        text.push((labels[2 * i].as_str(), plain));
        text.push((labels[2 * i + 1].as_str(), colored));
    }
    let lines = grid_lines(&text, 5, 24);
    let (size, image) = render_lines(&gpu, &mut renderer, &lines, 24, (None, &[]));
    check_golden("underlines", size, &image, Tolerance::Font);
}

/// Overline (foreground and SGR 58 colour), superscript, subscript and
/// both flags at once (drawn as superscript).
#[test]
fn scripts_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let d = Style::DEFAULT;
    let over = d.with(Style::OVERLINE);
    let lines = grid_lines(
        &[
            ("overline ", over),
            (
                "colored\n",
                Style {
                    underline: Color::Rgb(0x55, 0xff, 0x55),
                    ..over.with(Style::UNDERLINE)
                },
            ),
            ("E=mc", d),
            ("2", d.with(Style::SUPERSCRIPT)),
            (" x", d),
            ("n+1", d.with(Style::SUPERSCRIPT)),
            ("\nH", d),
            ("2", d.with(Style::SUBSCRIPT)),
            ("O a", d),
            ("i,j", d.with(Style::SUBSCRIPT)),
            ("\nboth ", d),
            ("xy", d.with(Style::SUPERSCRIPT | Style::SUBSCRIPT)),
            (" super ", d),
            ("xy", d.with(Style::SUPERSCRIPT)),
        ],
        4,
        24,
    );
    let (size, image) = render_lines(&gpu, &mut renderer, &lines, 24, (None, &[]));
    check_golden("scripts", size, &image, Tolerance::Font);
}

/// The block cursor on a wide character, on its spacer, on the second
/// cell of `=>` (a ligature in fonts that have one), on a built-in full
/// block (redrawn edge to edge) and on inverse underlined text, one strip
/// each. Emoji under the cursor are pinned by a pixel test in
/// `tests/overlay.rs` instead, so this golden needs Menlo only.
#[test]
fn cursor_wide_golden() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let line = |text: &str, style: Style| -> Vec<Cell> {
        let mut grid = tessera_grid::Grid::new(1, COLS as u16);
        grid.set_pen(Style::DEFAULT);
        grid.put_str("cursor: ");
        grid.set_pen(style);
        grid.put_str(text);
        let mut cells = grid.line(0).expect("row 0").cells.clone();
        widen(&mut cells);
        cells
    };
    let block = |col| CursorState::new(0, col, CursorShape::Block);
    let inverse_underline = Style::DEFAULT.with(Style::INVERSE | Style::UNDERLINE);
    let strips = [
        (line("@@@@", Style::DEFAULT), block(8), true),
        (line("@@@@", Style::DEFAULT), block(9), true),
        (line("a => b", Style::DEFAULT), block(11), true),
        (
            line("\u{2588}\u{2588}\u{2591}", Style::DEFAULT),
            block(9),
            true,
        ),
        (line("inverse", inverse_underline), block(9), true),
    ];
    let (size, image) = compose(&gpu, &mut renderer, None, &strips);
    check_golden("cursor_wide", size, &image, Tolerance::Font);
}

/// Renders one row of `text` with `cursor` and `focused`.
fn cursor_frame(
    gpu: &Gpu,
    renderer: &mut Renderer,
    cursor: Option<CursorState>,
    focused: bool,
) -> Vec<u8> {
    let row = styled("cursor here", Style::DEFAULT);
    let rows: [&[Cell]; 1] = [&row];
    let size = target_size(renderer, 12, 1);
    let frame = FrameInput::new(&rows)
        .with_cursor(cursor)
        .with_focus(focused);
    render(gpu, renderer, size, &frame)
}

#[test]
fn invisible_cursor_draws_nothing_and_unfocused_is_hollow() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let none = cursor_frame(&gpu, &mut renderer, None, true);
    let hollow = cursor_frame(
        &gpu,
        &mut renderer,
        Some(CursorState::new(0, 3, CursorShape::HollowBlock)),
        true,
    );
    assert_ne!(none, hollow, "a hollow cursor draws something");
    for shape in [
        CursorShape::Block,
        CursorShape::Beam,
        CursorShape::Underline,
    ] {
        let cursor = CursorState::new(0, 3, shape);
        let hidden = cursor.with_visible(false);
        for focused in [true, false] {
            assert_eq!(
                cursor_frame(&gpu, &mut renderer, Some(hidden), focused),
                none,
                "{shape:?} with visible = false (focused {focused}) draws nothing"
            );
        }
        let focused = cursor_frame(&gpu, &mut renderer, Some(cursor), true);
        assert_ne!(focused, none, "{shape:?} draws when focused");
        assert_eq!(
            cursor_frame(&gpu, &mut renderer, Some(cursor), false),
            hollow,
            "{shape:?} unfocused is the hollow block"
        );
    }
}

/// The RGBA bytes of pixel `(x, y)`.
fn pixel(image: &[u8], size: PhysicalSize, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * size.width + x) * 4) as usize;
    image[i..i + 4].try_into().expect("4 bytes")
}

#[test]
fn translucent_background_keeps_cells_and_glyphs_opaque() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme {
        background: Rgba::new(0.0, 0.0, 0.0, 0.5),
        padding: 0.0,
        ..Theme::default()
    };
    let mut renderer = renderer(&gpu, theme);
    let blue = Style {
        bg: Color::Indexed(4),
        ..Style::DEFAULT
    };
    let mut row = vec![Cell::BLANK, Cell::new(' ', blue)];
    row.extend(styled("█", Style::DEFAULT));
    let rows: [&[Cell]; 1] = [&row];
    let size = target_size(&renderer, 3, 1);
    let image = render(&gpu, &mut renderer, size, &FrameInput::new(&rows));
    let m = renderer.cell_metrics();
    let mid = m.height_px / 2;
    let cell = |c: u32| pixel(&image, size, c * m.width_px + m.width_px / 2, mid);
    assert_eq!(
        cell(0)[3],
        128,
        "default background keeps the theme opacity"
    );
    assert_eq!(cell(1), [0, 0, 0xee, 255], "explicit background is opaque");
    assert_eq!(cell(2), [0xe5, 0xe5, 0xe5, 255], "glyphs are opaque");
}

#[test]
fn translucent_background_is_premultiplied() {
    let Some(gpu) = gpu() else { return };
    let grey = Rgba::from_srgb8(0x80, 0x80, 0x80);
    let theme = Theme {
        background: grey.with_alpha(0.5),
        padding: 0.0,
        ..Theme::default()
    };
    let mut renderer = renderer(&gpu, theme);
    let explicit = Style {
        bg: Color::Rgb(0x80, 0x80, 0x80),
        ..Style::DEFAULT
    };
    let row = [Cell::BLANK, Cell::new(' ', explicit)];
    let rows: [&[Cell]; 1] = [&row];
    let size = target_size(&renderer, 2, 1);
    let image = render(&gpu, &mut renderer, size, &FrameInput::new(&rows));
    let m = renderer.cell_metrics();
    let cell = |c: u32| {
        pixel(
            &image,
            size,
            c * m.width_px + m.width_px / 2,
            m.height_px / 2,
        )
    };
    // (bg·a, a) in linear light, then sRGB-encoded by the target.
    let [r, g, b] = Rgba::new(grey.r * 0.5, grey.g * 0.5, grey.b * 0.5, 1.0).to_srgb8();
    let expected = [r, g, b, 128];
    let default = cell(0);
    for c in 0..4 {
        assert!(
            default[c].abs_diff(expected[c]) <= 1,
            "default cell {default:?}, expected premultiplied {expected:?}"
        );
    }
    assert_eq!(
        cell(1),
        [0x80, 0x80, 0x80, 255],
        "explicit background is opaque"
    );
}

#[test]
fn color_glyphs_keep_their_colors() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let mut row = styled("\u{1F600}", Style::DEFAULT);
    row.push(Cell::BLANK);
    let rows: [&[Cell]; 1] = [&row];
    let size = target_size(&renderer, 2, 1);
    let image = render(&gpu, &mut renderer, size, &FrameInput::new(&rows));
    let saturated = image
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| {
            let (lo, hi) = (p[..3].iter().min(), p[..3].iter().max());
            hi.zip(lo).is_some_and(|(hi, lo)| hi - lo > 100)
        })
        .count();
    assert!(
        saturated > 50,
        "an emoji draws in color, not tinted grey ({saturated} saturated pixels)"
    );
}

#[test]
fn rows_and_columns_past_the_target_are_clipped() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let wide = styled(&"x".repeat(500), Style::DEFAULT.with(Style::UNDERLINE));
    let rows: Vec<&[Cell]> = std::iter::repeat_n(wide.as_slice(), 200).collect();
    let size = target_size(&renderer, 3, 2);
    let cursor = CursorState::new(150, 400, CursorShape::Block);
    let frame = FrameInput::new(&rows).with_cursor(Some(cursor));
    let small = render(&gpu, &mut renderer, size, &frame);
    // Three columns and two rows fit; the padding shows part of one more
    // of each.
    let clipped: Vec<Vec<Cell>> = rows[..3].iter().map(|r| r[..4].to_vec()).collect();
    let clipped: Vec<&[Cell]> = clipped.iter().map(Vec::as_slice).collect();
    assert_eq!(
        render(&gpu, &mut renderer, size, &FrameInput::new(&clipped)),
        small,
        "only the visible cells are drawn"
    );
}

/// Renders one row of `cells` and returns the target size and image.
fn render_cells(
    gpu: &Gpu,
    renderer: &mut Renderer,
    cells: &[Cell],
    cursor: Option<CursorState>,
) -> (PhysicalSize, Vec<u8>) {
    let rows: [&[Cell]; 1] = [cells];
    let size = target_size(renderer, cells.len(), 1);
    let frame = FrameInput::new(&rows).with_cursor(cursor);
    (size, render(gpu, renderer, size, &frame))
}

/// The pixels of cell `col` (row 0) of an image from [`render_cells`].
fn cell_pixels(renderer: &Renderer, size: PhysicalSize, image: &[u8], col: u32) -> Vec<[u8; 4]> {
    let m = renderer.cell_metrics();
    let pad = (renderer.theme().padding * renderer.shaper().scale()).round() as u32;
    let x0 = pad + col * m.width_px;
    (pad..pad + m.height_px)
        .flat_map(|y| (x0..x0 + m.width_px).map(move |x| (x, y)))
        .map(|(x, y)| pixel(image, size, x, y))
        .collect()
}

#[test]
fn hidden_cells_draw_only_their_background() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let style = Style {
        fg: Color::Indexed(1),
        bg: Color::Indexed(4),
        flags: Style::HIDDEN | Style::UNDERLINE | Style::STRIKETHROUGH | Style::BOLD,
        ..Style::DEFAULT
    };
    let hidden = styled("Hi_─\u{1F600}", style);
    let plain: Vec<Cell> = hidden
        .iter()
        .map(|c| {
            Cell::new(
                ' ',
                Style {
                    flags: 0,
                    ..c.style
                },
            )
        })
        .collect();
    assert_eq!(
        render_cells(&gpu, &mut renderer, &hidden, None),
        render_cells(&gpu, &mut renderer, &plain, None),
        "hidden text, box drawing, emoji and decorations draw nothing"
    );
}

#[test]
fn block_cursor_redraws_the_glyph_in_the_background_color() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let cursor_color = theme.cursor.to_srgb8();
    let bg = theme.background.to_srgb8();
    let mut renderer = renderer(&gpu, theme);
    let cursor = Some(CursorState::new(0, 0, CursorShape::Block));
    let (size, image) = render_cells(&gpu, &mut renderer, &styled("X", Style::DEFAULT), cursor);
    let cell = cell_pixels(&renderer, size, &image, 0);
    let near = |p: &[u8; 4], c: [u8; 3]| p[..3].iter().zip(c).all(|(a, b)| a.abs_diff(b) <= 2);
    let ink = cell.iter().filter(|p| near(p, bg)).count();
    assert!(
        ink > 30,
        "the X is drawn in the background color ({ink} px)"
    );
    let brighter = cell
        .iter()
        .filter(|p| {
            p[..3]
                .iter()
                .zip(cursor_color)
                .any(|(a, b)| *a > b.saturating_add(2))
        })
        .count();
    assert_eq!(brighter, 0, "no foreground ink shows under the cursor");
    let (_, blank) = render_cells(&gpu, &mut renderer, &styled(" ", Style::DEFAULT), cursor);
    assert_ne!(image, blank, "the glyph is redrawn, not covered");
}

#[test]
fn transparent_cursor_draws_nothing() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme {
        cursor: Rgba::new(1.0, 0.0, 0.0, 0.0),
        ..Theme::default()
    };
    let mut renderer = renderer(&gpu, theme);
    let row = styled("Xy", Style::DEFAULT.with(Style::UNDERLINE));
    let (_, none) = render_cells(&gpu, &mut renderer, &row, None);
    for shape in [
        CursorShape::Block,
        CursorShape::Beam,
        CursorShape::Underline,
        CursorShape::HollowBlock,
    ] {
        let cursor = Some(CursorState::new(0, 0, shape));
        assert_eq!(
            render_cells(&gpu, &mut renderer, &row, cursor).1,
            none,
            "{shape:?} with alpha 0 draws exactly nothing"
        );
    }
}

#[test]
fn styles_change_the_glyphs() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let text = "Wag";
    let mut draw = |flags: u16| {
        let cells = styled(text, Style::DEFAULT.with(flags));
        render_cells(&gpu, &mut renderer, &cells, None)
    };
    let (size, regular) = draw(0);
    for (name, flag) in [
        ("bold", Style::BOLD),
        ("italic", Style::ITALIC),
        ("dim", Style::DIM),
        ("inverse", Style::INVERSE),
    ] {
        let n = differing_pixels(&regular, &draw(flag).1, 16);
        assert!(n > 100, "{name} changes only {n} pixels of {text:?}");
    }
    // Dim darkens the ink; inverse puts dark ink on a light cell.
    let brightest = |image: &[u8]| image.as_chunks::<4>().0.iter().map(|p| p[0]).max();
    let (dim, plain) = (brightest(&draw(Style::DIM).1), brightest(&regular));
    assert!(
        dim.zip(plain).is_some_and(|(d, p)| d + 40 < p),
        "dim ink is darker: {dim:?} vs {plain:?}"
    );
    let inverse = draw(Style::INVERSE).1;
    let cell = cell_pixels(&renderer, size, &inverse, 0);
    let dark = cell.iter().filter(|p| p[0] < 0x40).count();
    let light = cell.iter().filter(|p| p[0] > 0xc0).count();
    assert!(
        dark > 30 && light > dark,
        "inverse: {dark} dark ink px on {light} light px"
    );
}

#[test]
fn ligatures_never_cross_a_hidden_cell() {
    let Some(gpu) = gpu() else { return };
    let families = ["Cascadia Code", "JetBrains Mono", "Fira Code"];
    let Some(mut renderer) = families
        .iter()
        .find_map(|f| renderer_with(&gpu, Theme::default(), f))
    else {
        eprintln!("skipping: none of {families:?} is installed");
        return;
    };
    let family = renderer.shaper().fonts().family();
    let d = Style::DEFAULT;
    let h = d.with(Style::HIDDEN);
    let cell = Cell::new;
    let cases: [(Vec<Cell>, Vec<Cell>); 3] = [
        (
            vec![cell('=', d), cell('>', h)],
            vec![cell('=', d), Cell::BLANK],
        ),
        (
            vec![cell('=', h), cell('>', d)],
            vec![Cell::BLANK, cell('>', d)],
        ),
        (
            vec![cell('-', d), cell('>', h), cell('>', d)],
            vec![cell('-', d), Cell::BLANK, cell('>', d)],
        ),
    ];
    for (hidden, blank) in &cases {
        assert_eq!(
            render_cells(&gpu, &mut renderer, hidden, None),
            render_cells(&gpu, &mut renderer, blank, None),
            "{family}: hidden cells shape as blanks"
        );
    }
    // The ligature is real in this font: `=>` is not `=` and `>` apart.
    let joined = render_cells(&gpu, &mut renderer, &[cell('=', d), cell('>', d)], None).1;
    let apart = [cell('=', d), Cell::BLANK, cell('>', d)];
    let (size, apart) = render_cells(&gpu, &mut renderer, &apart, None);
    let m = renderer.cell_metrics();
    let gt: Vec<_> = cell_pixels(&renderer, size, &apart, 2);
    let joined_size = target_size(&renderer, 2, 1);
    let joined_gt = cell_pixels(&renderer, joined_size, &joined, 1);
    assert_ne!(gt, joined_gt, "{family}: `=>` forms a ligature ({m:?})");
}

#[test]
fn hidden_base_does_not_take_a_visible_mark_with_it() {
    // Font-independent twin of `ligatures_never_cross_a_hidden_cell`, so
    // the blanking also runs on machines with only the system fonts: a
    // hidden `e` followed by a visible combining acute. Shaped together
    // they form one cluster owned by the hidden cell, which would hide the
    // mark too; with the hidden cell blanked the mark draws on its own.
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, Theme::default());
    let d = Style::DEFAULT;
    let hidden = [
        Cell::new('e', d.with(Style::HIDDEN)),
        Cell::new('\u{301}', d),
    ];
    let blanked = [Cell::BLANK, Cell::new('\u{301}', d)];
    let (_, image) = render_cells(&gpu, &mut renderer, &hidden, None);
    assert_eq!(
        image,
        render_cells(&gpu, &mut renderer, &blanked, None).1,
        "the hidden base shapes as a blank"
    );
    let (_, nothing) = render_cells(&gpu, &mut renderer, &[Cell::BLANK; 2], None);
    assert_ne!(image, nothing, "the visible mark draws");
}
