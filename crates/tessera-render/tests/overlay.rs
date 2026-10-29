//! The overlay pass (selection, search matches, link underlines), the
//! decorations added with it (underline styles and colour, overline,
//! superscript and subscript) and the block cursor's redraw rules, drawn
//! headless and checked pixel by pixel (ADR 0010).
//!
//! `TESSERA_SKIP_GPU_TESTS=1` skips the suite like the other GPU tests.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "pixel and cell counts in these tests are small and non-negative"
)]

use tessera_grid::{Cell, Color, DamageRows, GraphemeTable, Grid, Point, Style};
use tessera_render::{
    CursorShape, CursorState, FontConfig, FrameInput, Gpu, OffscreenTarget, Overlay, OverlayKind,
    PhysicalSize, Renderer, Rgba, Selection, Shaper, Theme,
};

const SKIP_VAR: &str = "TESSERA_SKIP_GPU_TESTS";
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn gpu() -> Option<Gpu> {
    if std::env::var_os(SKIP_VAR).is_some() {
        eprintln!("skipping: {SKIP_VAR} is set");
        return None;
    }
    Some(Gpu::headless().unwrap_or_else(|e| {
        panic!("no GPU adapter available ({e}); set {SKIP_VAR}=1 to skip the GPU tests")
    }))
}

/// A renderer for `family` at 13 pt and `scale`, or `None` when the family
/// is not installed.
fn renderer_with(gpu: &Gpu, theme: Theme, family: &str, scale: f32) -> Option<Renderer> {
    let shaper = Shaper::new(&FontConfig {
        family: family.into(),
        size_pt: 13.0,
        scale,
        fallback: Vec::new(),
    })
    .expect("fonts");
    if shaper.fonts().used_default_family() {
        return None;
    }
    Some(Renderer::new(gpu, theme, shaper).expect("renderer"))
}

fn menlo(gpu: &Gpu) -> Renderer {
    renderer_with(gpu, Theme::default(), "Menlo", 2.0).expect("Menlo ships with macOS")
}

/// The first installed ligature font, for the tests that need one.
fn ligature_renderer(gpu: &Gpu) -> Option<Renderer> {
    let families = ["Cascadia Code", "JetBrains Mono", "Fira Code"];
    let found = families
        .iter()
        .find_map(|f| renderer_with(gpu, Theme::default(), f, 2.0));
    if found.is_none() {
        eprintln!("skipping: none of {families:?} is installed");
    }
    found
}

/// Rows of cells laid out by the grid (wide characters, spacers, clusters).
struct Screen {
    grid: Grid,
}

impl Screen {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            grid: Grid::new(rows, cols),
        }
    }

    fn with(rows: &[(&str, Style)], cols: u16) -> Self {
        let mut screen = Self::new(rows.len() as u16, cols);
        for (r, (text, style)) in rows.iter().enumerate() {
            screen.write(r as u16, text, *style);
        }
        screen
    }

    fn write(&mut self, row: u16, text: &str, style: Style) {
        self.grid.goto(row, 0);
        self.grid.set_pen(style);
        self.grid.erase_in_line(tessera_grid::EraseLine::All);
        self.grid.put_str(text);
    }

    fn rows(&self) -> Vec<&[Cell]> {
        self.grid
            .visible_lines()
            .iter()
            .map(|l| l.cells.as_slice())
            .collect()
    }

    fn clusters(&self) -> Vec<GraphemeTable> {
        self.grid
            .visible_lines()
            .iter()
            .map(|l| l.graphemes.clone())
            .collect()
    }
}

/// An offscreen target sized for `rows` x `cols` cells plus padding.
struct Target {
    texture: OffscreenTarget,
    size: PhysicalSize,
    pad: u32,
    cell: [u32; 2],
}

impl Target {
    fn new(gpu: &Gpu, renderer: &Renderer, rows: u16, cols: u16) -> Self {
        let m = renderer.cell_metrics();
        let pad = (renderer.theme().padding * renderer.shaper().scale()).round() as u32;
        let size = PhysicalSize::new(
            m.width_px * u32::from(cols) + 2 * pad,
            m.height_px * u32::from(rows) + 2 * pad,
        );
        Self {
            texture: OffscreenTarget::new(gpu, size, FORMAT),
            size,
            pad,
            cell: [m.width_px, m.height_px],
        }
    }

    fn draw(&self, gpu: &Gpu, renderer: &mut Renderer, frame: &FrameInput<'_>) -> Vec<u8> {
        self.texture.render(gpu, renderer, frame).expect("render");
        self.texture.read_back(gpu).expect("readback")
    }

    fn pixel(&self, image: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.size.width + x) * 4) as usize;
        [image[i], image[i + 1], image[i + 2], image[i + 3]]
    }

    /// Top-left pixel of cell `(row, col)`.
    fn origin(&self, row: u32, col: u32) -> (u32, u32) {
        (self.pad + col * self.cell[0], self.pad + row * self.cell[1])
    }

    /// Every pixel of cells `(row, col..col + n)`, row-major.
    fn cells(&self, image: &[u8], row: u32, col: u32, n: u32) -> Vec<[u8; 4]> {
        let (x0, y0) = self.origin(row, col);
        let mut out = Vec::new();
        for y in y0..y0 + self.cell[1] {
            for x in x0..x0 + n * self.cell[0] {
                out.push(self.pixel(image, x, y));
            }
        }
        out
    }

    /// True when `a` and `b` differ only inside cells `(row, cols)`.
    fn differ_only_in(&self, a: &[u8], b: &[u8], row: u32, cols: std::ops::Range<u32>) -> bool {
        let (x0, y0) = self.origin(row, cols.start);
        let (x1, y1) = (x0 + cols.len() as u32 * self.cell[0], y0 + self.cell[1]);
        (0..self.size.height).all(|y| {
            (0..self.size.width).all(|x| {
                let inside = (x0..x1).contains(&x) && (y0..y1).contains(&y);
                inside || self.pixel(a, x, y) == self.pixel(b, x, y)
            })
        })
    }
}

/// `color` alpha-blended over `under` in linear light, as the target stores
/// it (sRGB bytes).
fn blended(color: Rgba, under: Rgba) -> [u8; 3] {
    let a = color.a;
    Rgba::new(
        color.r * a + under.r * (1.0 - a),
        color.g * a + under.g * (1.0 - a),
        color.b * a + under.b * (1.0 - a),
        1.0,
    )
    .to_srgb8()
}

fn near(p: [u8; 4], c: [u8; 3], tolerance: u8) -> bool {
    p[..3]
        .iter()
        .zip(c)
        .all(|(a, b)| a.abs_diff(b) <= tolerance)
}

/// How many pixels of `pixels` are within 2 of `c`.
fn count(pixels: &[[u8; 4]], c: [u8; 3]) -> usize {
    pixels.iter().filter(|p| near(**p, c, 2)).count()
}

fn rgb(c: Rgba) -> [u8; 3] {
    c.to_srgb8()
}

// ------------------------------------------------------------ damage

/// D3: the renderer diffs the selection itself. Adding a one-row
/// selection to an undamaged frame rebuilds and uploads only that row's
/// overlay slot (no shaping, no other row), and removing it rebuilds the
/// same row; both frames match a fresh renderer drawing them in full.
#[test]
fn a_selection_change_rebuilds_only_the_rows_it_covers() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = menlo(&gpu);
    let text: Vec<String> = (0..6)
        .map(|r| format!("row {r}: the quick brown fox"))
        .collect();
    let rows: Vec<(&str, Style)> = text.iter().map(|t| (t.as_str(), Style::DEFAULT)).collect();
    let screen = Screen::with(&rows, 30);
    let target = Target::new(&gpu, &renderer, 6, 30);
    let cells = screen.rows();
    let plain = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let undamaged = FrameInput::new(&cells).with_damage(DamageRows::None);

    let before = renderer.stats();
    let selection = Selection::linear(Point::new(3, 2), Point::new(3, 9));
    let selected = target.draw(
        &gpu,
        &mut renderer,
        &undamaged.clone().with_selection(Some(selection)),
    );
    let after = renderer.stats();
    assert_eq!(after.rows_shaped, before.rows_shaped, "nothing re-shaped");
    assert_eq!(after.rows_built, before.rows_built, "no row rebuilt whole");
    assert_eq!(
        after.overlay_rows_built - before.overlay_rows_built,
        1,
        "row 3 only"
    );
    assert_eq!(after.relayouts, before.relayouts);
    let uploaded = after.instances_uploaded - before.instances_uploaded;
    assert!((1..=4).contains(&uploaded), "one overlay slot: {uploaded}");
    assert!(
        target.differ_only_in(&plain, &selected, 3, 2..10),
        "only the selected cells change"
    );
    assert_ne!(plain, selected);
    let mut reference = menlo(&gpu);
    let full = target.draw(
        &gpu,
        &mut reference,
        &FrameInput::new(&cells).with_selection(Some(selection)),
    );
    assert!(selected == full, "matches a full frame with the selection");

    // The same selection again: nothing to do.
    let before = renderer.stats();
    target.draw(
        &gpu,
        &mut renderer,
        &undamaged.clone().with_selection(Some(selection)),
    );
    let after = renderer.stats();
    assert_eq!(after.overlay_rows_built, before.overlay_rows_built);
    assert_eq!(after.instances_uploaded, before.instances_uploaded);

    // Removing it rebuilds row 3 again and restores the plain frame.
    let before = renderer.stats();
    let cleared = target.draw(&gpu, &mut renderer, &undamaged);
    let after = renderer.stats();
    assert_eq!(after.overlay_rows_built - before.overlay_rows_built, 1);
    assert_eq!(after.rows_built, before.rows_built);
    assert!(cleared == plain, "the highlight is gone");
}

/// Moving a selection's end down one row rebuilds the rows whose
/// coverage changed (the old last row and the new one), not the rows in
/// between whose spans stayed the same.
#[test]
fn growing_a_selection_rebuilds_the_rows_whose_coverage_changed() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = menlo(&gpu);
    let screen = Screen::with(&[("abc", Style::DEFAULT); 6], 10);
    let target = Target::new(&gpu, &renderer, 6, 10);
    let cells = screen.rows();
    let at = |end_row| {
        FrameInput::new(&cells)
            .with_damage(DamageRows::None)
            .with_selection(Some(Selection::linear(
                Point::new(1, 1),
                Point::new(end_row, 2),
            )))
    };
    target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    target.draw(&gpu, &mut renderer, &at(3));
    let before = renderer.stats();
    let grown = target.draw(&gpu, &mut renderer, &at(4));
    let after = renderer.stats();
    assert_eq!(
        after.overlay_rows_built - before.overlay_rows_built,
        2,
        "row 3 becomes whole, row 4 is new; rows 1 and 2 are unchanged"
    );
    let mut reference = menlo(&gpu);
    let full = target.draw(&gpu, &mut reference, &at(4).with_damage(DamageRows::All));
    assert!(grown == full, "matches a full frame");
}

/// Overlays are compared like the selection: changing the current match
/// rebuilds the two rows involved, and every frame matches a full one.
#[test]
fn overlay_changes_are_diffed_per_row() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = menlo(&gpu);
    let screen = Screen::with(&[("find me, find me", Style::DEFAULT); 4], 20);
    let target = Target::new(&gpu, &renderer, 4, 20);
    let cells = screen.rows();
    let matches = |current: u16| -> Vec<Overlay> {
        (0..4)
            .map(|r| {
                let kind = if r == current {
                    OverlayKind::CurrentMatch
                } else {
                    OverlayKind::Match
                };
                Overlay::new(kind, r, 0..4)
            })
            .collect()
    };
    let first = matches(0);
    let second = matches(2);
    target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&cells).with_overlays(&first),
    );
    let before = renderer.stats();
    let frame = FrameInput::new(&cells)
        .with_overlays(&second)
        .with_damage(DamageRows::None);
    let moved = target.draw(&gpu, &mut renderer, &frame);
    let after = renderer.stats();
    assert_eq!(after.overlay_rows_built - before.overlay_rows_built, 2);
    assert_eq!(after.rows_built, before.rows_built);
    let mut reference = menlo(&gpu);
    let full = target.draw(
        &gpu,
        &mut reference,
        &frame.clone().with_damage(DamageRows::All),
    );
    assert!(moved == full, "matches a full frame");
}

/// A damaged row keeps its highlight: the overlay is rebuilt with the
/// row even when its coverage did not change, and a link underline takes
/// the row's new foreground.
#[test]
fn a_damaged_row_keeps_its_highlights() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = menlo(&gpu);
    let mut screen = Screen::with(&[("hello", Style::DEFAULT), ("world", Style::DEFAULT)], 10);
    let target = Target::new(&gpu, &renderer, 2, 10);
    let overlays = [Overlay::new(OverlayKind::Underline, 1, 0..5)];
    let selection = Some(Selection::linear(Point::new(0, 1), Point::new(1, 1)));
    {
        let cells = screen.rows();
        let frame = FrameInput::new(&cells)
            .with_overlays(&overlays)
            .with_selection(selection);
        target.draw(&gpu, &mut renderer, &frame);
    }
    let red = Style {
        fg: Color::Indexed(1),
        ..Style::DEFAULT
    };
    screen.write(1, "WORLD", red);
    let cells = screen.rows();
    let frame = FrameInput::new(&cells)
        .with_overlays(&overlays)
        .with_selection(selection);
    let incremental = target.draw(
        &gpu,
        &mut renderer,
        &frame.clone().with_damage(DamageRows::Rows(1..=1)),
    );
    let mut reference = menlo(&gpu);
    let full = target.draw(&gpu, &mut reference, &frame);
    assert!(incremental == full, "matches a full frame");
}

// ------------------------------------------------------------ colours

/// D2: the selection is the theme colour blended over the cell
/// background, on default and explicit backgrounds alike, and the text
/// keeps its foreground (no inverse video).
#[test]
fn the_selection_blends_over_the_background_and_keeps_the_text() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let blue_bg = Style {
        bg: Color::Indexed(4),
        ..Style::DEFAULT
    };
    let screen = Screen::with(&[("X  ", Style::DEFAULT), ("   ", blue_bg)], 4);
    let target = Target::new(&gpu, &renderer, 2, 4);
    let cells = screen.rows();
    let selection = Selection::linear(Point::new(0, 0), Point::new(1, 2));
    let image = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&cells).with_selection(Some(selection)),
    );
    let (x, y) = target.origin(0, 1);
    let on_default = target.pixel(&image, x + 2, y + 2);
    assert!(
        near(on_default, blended(theme.selection, theme.background), 2),
        "{on_default:?}"
    );
    let (x, y) = target.origin(1, 1);
    let on_blue = target.pixel(&image, x + 2, y + 2);
    assert!(
        near(on_blue, blended(theme.selection, theme.ansi[4]), 2),
        "{on_blue:?}"
    );
    // The X keeps its light foreground: its brightest pixel is as bright
    // as the X drawn without a selection.
    let plain = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let brightest = |img: &[u8]| target.cells(img, 0, 0, 1).iter().map(|p| p[0]).max();
    let (with, without) = (brightest(&image), brightest(&plain));
    assert!(
        with.zip(without).is_some_and(|(w, o)| w.abs_diff(o) <= 2),
        "text is not inverted: {with:?} vs {without:?}"
    );
    // Inner rows are whole: a row shorter than the frame is highlighted
    // past its last cell.
    let short: [&[Cell]; 3] = [&cells[0][..1], &cells[0][..1], &cells[0][..1]];
    let target = Target::new(&gpu, &renderer, 3, 4);
    let selection = Selection::linear(Point::new(0, 3), Point::new(2, 0));
    let image = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&short).with_selection(Some(selection)),
    );
    let highlight = blended(theme.selection, theme.background);
    let (x, y) = target.origin(1, 3);
    assert!(near(target.pixel(&image, x + 2, y + 2), highlight, 2));
    let (x, y) = target.origin(0, 3);
    assert!(
        near(target.pixel(&image, x + 2, y + 2), highlight, 2),
        "first row to the end"
    );
    let (x, y) = target.origin(2, 1);
    assert!(
        near(target.pixel(&image, x + 2, y + 2), rgb(theme.background), 0),
        "last row stops"
    );
}

/// Search matches use their own theme colours and stack over the
/// selection with the current match on top, whatever the slice order;
/// overlapping matches highlight a cell once (their union); the link
/// underline is a thin bar in the covered cells' foreground at the
/// underline position.
#[test]
fn overlays_use_their_colours_in_order() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let screen = Screen::with(&[("      ", Style::DEFAULT)], 6);
    let target = Target::new(&gpu, &renderer, 1, 6);
    let cells = screen.rows();
    let overlays = [
        Overlay::new(OverlayKind::Match, 0, 0..1),
        Overlay::new(OverlayKind::Match, 0, 0..1),
        Overlay::new(OverlayKind::Match, 0, 0..2),
        Overlay::new(OverlayKind::CurrentMatch, 0, 1..2),
        Overlay::new(OverlayKind::CurrentMatch, 0, 2..3),
        Overlay::new(OverlayKind::Match, 0, 2..3),
        Overlay::new(OverlayKind::Underline, 0, 4..6),
        Overlay::new(OverlayKind::Underline, 0, 4..6),
    ];
    let selection = Some(Selection::linear(Point::new(0, 2), Point::new(0, 2)));
    let frame = FrameInput::new(&cells)
        .with_overlays(&overlays)
        .with_selection(selection);
    let image = target.draw(&gpu, &mut renderer, &frame);
    let centre = |col| {
        let (x, y) = target.origin(0, col);
        target.pixel(&image, x + 4, y + 4)
    };
    assert!(near(
        centre(0),
        blended(theme.search_match, theme.background),
        2
    ));
    // Cell 0: three overlapping matches, highlighted once.
    // Cell 1: a match under the current match.
    let below = |c: [u8; 3]| Rgba::from_srgb8(c[0], c[1], c[2]);
    let match_then_current = blended(
        theme.search_match_current,
        below(blended(theme.search_match, theme.background)),
    );
    assert!(near(centre(1), match_then_current, 3), "{:?}", centre(1));
    // Cell 2: selection, then match, then current match (listed before the
    // match in the slice), each blended over what is below.
    let stacked = [
        theme.selection,
        theme.search_match,
        theme.search_match_current,
    ]
    .into_iter()
    .fold(rgb(theme.background), |under, top| {
        blended(top, below(under))
    });
    assert!(
        near(centre(2), stacked, 3),
        "{:?} vs {stacked:?}",
        centre(2)
    );
    // The link underline: the foreground in the underline rows of cells 4
    // and 5, nothing above.
    let fg = rgb(theme.foreground);
    let link = target.cells(&image, 0, 4, 2);
    let lit = count(&link, fg);
    let height = renderer.shaper().scale() as usize;
    assert_eq!(
        lit,
        2 * target.cell[0] as usize * height,
        "a bar one point high"
    );
    let (x, y) = target.origin(0, 4);
    assert!(near(
        target.pixel(&image, x + 3, y + 3),
        rgb(theme.background),
        0
    ));
}

/// A selection ending on the left half of a wide character covers its
/// right half, one starting on the right half covers the left, and one
/// reaching far past the frame is clipped without panicking.
#[test]
fn selections_take_wide_characters_whole_and_clip_at_the_frame() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let screen = Screen::with(&[("a中b", Style::DEFAULT), ("中文", Style::DEFAULT)], 6);
    let target = Target::new(&gpu, &renderer, 2, 6);
    let cells = screen.rows();
    let highlight = blended(theme.selection, theme.background);
    let lit = |image: &[u8], row, col| {
        let (x, y) = target.origin(row, col);
        // Bottom-left corner: no glyph ink there.
        near(
            target.pixel(image, x + 1, y + target.cell[1] - 2),
            highlight,
            2,
        )
    };
    let draw = |renderer: &mut Renderer, selection| {
        target.draw(
            &gpu,
            renderer,
            &FrameInput::new(&cells).with_selection(Some(selection)),
        )
    };
    let end_on_wide = draw(
        &mut renderer,
        Selection::linear(Point::new(0, 0), Point::new(0, 1)),
    );
    assert!(
        lit(&end_on_wide, 0, 2),
        "the spacer is selected with its wide half"
    );
    assert!(!lit(&end_on_wide, 0, 3));
    let start_on_spacer = draw(
        &mut renderer,
        Selection::linear(Point::new(0, 2), Point::new(0, 3)),
    );
    assert!(
        lit(&start_on_spacer, 0, 1),
        "the wide half is selected with its spacer"
    );
    assert!(!lit(&start_on_spacer, 0, 0));
    let block = draw(
        &mut renderer,
        Selection::block(Point::new(0, 2), Point::new(1, 2)),
    );
    for (row, col) in [(0, 1), (0, 2), (1, 2), (1, 3)] {
        assert!(lit(&block, row, col), "block covers ({row}, {col})");
    }
    assert!(!lit(&block, 1, 1) && !lit(&block, 0, 3));
    let far = Selection::linear(Point::new(1, 5), Point::new(u16::MAX, u16::MAX));
    let image = draw(&mut renderer, far);
    assert!(lit(&image, 1, 5));
    let off = Selection::linear(Point::new(9, 0), Point::new(20, 3));
    let plain = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    assert!(
        draw(&mut renderer, off) == plain,
        "nothing visible is selected"
    );
    let overlays = [
        Overlay::new(OverlayKind::Match, 7, 0..3),
        Overlay::new(OverlayKind::Match, 0, 90..u16::MAX),
    ];
    let image = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&cells).with_overlays(&overlays),
    );
    assert!(image == plain, "overlays outside the frame draw nothing");
}

/// The ADR 0016 open item: a selection on the last character of a
/// ligature does not shorten the ligature. Shaping never sees the
/// selection, so the image differs from the unselected one only inside
/// the selected cell, and the arrow still differs from `-` and `>` drawn
/// apart.
#[test]
fn a_selection_on_a_ligatures_last_character_keeps_the_ligature() {
    let Some(gpu) = gpu() else { return };
    let Some(mut renderer) = ligature_renderer(&gpu) else {
        return;
    };
    let screen = Screen::with(&[("a->b", Style::DEFAULT)], 6);
    let target = Target::new(&gpu, &renderer, 1, 6);
    let cells = screen.rows();
    let plain = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let shaped = renderer.stats().rows_shaped;
    let selection = Some(Selection::linear(Point::new(0, 2), Point::new(0, 2)));
    let frame = FrameInput::new(&cells)
        .with_selection(selection)
        .with_damage(DamageRows::None);
    let selected = target.draw(&gpu, &mut renderer, &frame);
    assert_eq!(
        renderer.stats().rows_shaped,
        shaped,
        "the selection never re-shapes"
    );
    assert!(target.differ_only_in(&plain, &selected, 0, 2..3));
    let apart = Screen::with(&[("a- >b", Style::DEFAULT)], 6);
    let apart_cells = apart.rows();
    let apart_image = target.draw(&gpu, &mut renderer, &FrameInput::new(&apart_cells));
    assert_ne!(
        target.cells(&plain, 0, 1, 1),
        target.cells(&apart_image, 0, 1, 1),
        "`->` is a ligature in this font"
    );
}

// ------------------------------------------------------------ decorations

/// Ink pixels (anything not the background) per pixel row of cell
/// `(row, col)`.
fn ink_rows(target: &Target, image: &[u8], row: u32, col: u32, bg: [u8; 3]) -> Vec<usize> {
    let (x0, y0) = target.origin(row, col);
    (y0..y0 + target.cell[1])
        .map(|y| {
            (x0..x0 + target.cell[0])
                .filter(|&x| !near(target.pixel(image, x, y), bg, 8))
                .count()
        })
        .collect()
}

/// Runs of lit pixels along pixel row `y` of cells `col..col + n`.
fn runs_along(
    target: &Target,
    image: &[u8],
    y: u32,
    cols: (u32, u32),
    lit: [u8; 3],
) -> Vec<(u32, u32)> {
    let (x0, _) = target.origin(0, cols.0);
    let x1 = x0 + cols.1 * target.cell[0];
    let mut runs = Vec::new();
    let mut open: Option<u32> = None;
    for x in x0..=x1 {
        let on = x < x1 && near(target.pixel(image, x, y), lit, 40);
        match (open, on) {
            (None, true) => open = Some(x),
            (Some(start), false) => {
                runs.push((start - x0, x - start));
                open = None;
            }
            _ => {}
        }
    }
    runs
}

/// Each underline style draws its own pattern on blank cells: single and
/// double are unbroken bars, dotted and dashed alternate at their pitch,
/// curly moves up and down; the underline colour (SGR 58) is used when set.
#[test]
fn underline_styles_draw_their_patterns() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let u = Style::UNDERLINE;
    let red = Style {
        underline: Color::Rgb(255, 0, 0),
        ..Style::DEFAULT
    };
    let rows = [
        ("    ", Style::DEFAULT.with(u)),
        ("    ", Style::DEFAULT.with(u | Style::DOUBLE_UNDERLINE)),
        ("    ", Style::DEFAULT.with(u | Style::CURLY_UNDERLINE)),
        ("    ", Style::DEFAULT.with(u | Style::DOTTED_UNDERLINE)),
        ("    ", Style::DEFAULT.with(u | Style::DASHED_UNDERLINE)),
        ("    ", red.with(u | Style::CURLY_UNDERLINE)),
    ];
    let screen = Screen::with(&rows, 4);
    let target = Target::new(&gpu, &renderer, 6, 4);
    let cells = screen.rows();
    let image = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let bg = rgb(theme.background);
    let fg = rgb(theme.foreground);
    let (cw, ch) = (target.cell[0], target.cell[1]);
    let lit_rows = |row| -> Vec<usize> {
        ink_rows(&target, &image, row, 1, bg)
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(y, _)| y)
            .collect()
    };
    // Single: one bar of 2 px at 2x, across the whole cell.
    assert_eq!(lit_rows(0), [26, 27]);
    assert_eq!(ink_rows(&target, &image, 0, 1, bg)[26], cw as usize);
    // Double: two bars one thickness apart.
    assert_eq!(lit_rows(1), [24, 25, 28, 29]);
    // Curly: ink on more rows than a bar, inside the cell, wavy.
    let curly = lit_rows(2);
    assert!(
        curly.len() >= 4 && curly.iter().all(|y| *y < ch as usize),
        "{curly:?}"
    );
    let (_, y2) = target.origin(2, 0);
    let top_band = runs_along(&target, &image, y2 + 24, (0, 4), fg);
    assert!(
        top_band.len() >= 3,
        "the crest comes back once per cell: {top_band:?}"
    );
    // Dotted: 2 px dots every 4 px.
    let (_, y3) = target.origin(3, 0);
    let dots = runs_along(&target, &image, y3 + 26, (0, 4), fg);
    assert_eq!(dots.len(), 4 * cw as usize / 4, "{dots:?}");
    assert!(dots.iter().all(|(_, len)| *len == 2), "{dots:?}");
    assert!(dots.windows(2).all(|w| w[1].0 - w[0].0 == 4), "{dots:?}");
    // Dashed: two dashes per cell, about a third of a cell long.
    let (_, y4) = target.origin(4, 0);
    let dashes = runs_along(&target, &image, y4 + 26, (0, 4), fg);
    assert_eq!(dashes.len(), 8, "{dashes:?}");
    assert!(
        dashes.iter().all(|(_, len)| (5..=6).contains(len)),
        "{dashes:?}"
    );
    // The underline colour.
    let (_, y5) = target.origin(5, 0);
    let red_px = (y5..y5 + ch)
        .flat_map(|y| (target.pad..target.pad + 4 * cw).map(move |x| (x, y)))
        .filter(|&(x, y)| near(target.pixel(&image, x, y), [255, 0, 0], 8))
        .count();
    assert!(red_px > 30, "curly in the SGR 58 colour: {red_px}");
}

/// Decorations honour INVERSE and DIM (deferred from #20): an underline on
/// inverse text is drawn in the swapped foreground (the default
/// background) so it is visible on the inverted cell, and a dimmed
/// underline is darker than a plain one.
#[test]
fn decorations_follow_inverse_and_dim() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let u = Style::DEFAULT.with(Style::UNDERLINE);
    let screen = Screen::with(
        &[
            ("  ", u),
            ("  ", u.with(Style::INVERSE)),
            ("  ", u.with(Style::DIM)),
        ],
        2,
    );
    let target = Target::new(&gpu, &renderer, 3, 2);
    let cells = screen.rows();
    let image = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let bar = |row| {
        let (x, y) = target.origin(row, 0);
        target.pixel(&image, x + 4, y + 26)
    };
    let fill = |row| {
        let (x, y) = target.origin(row, 0);
        target.pixel(&image, x + 4, y + 4)
    };
    assert!(near(bar(0), rgb(theme.foreground), 1));
    assert!(
        near(fill(1), rgb(theme.foreground), 1),
        "the cell is inverted"
    );
    assert!(
        near(bar(1), rgb(theme.background), 1),
        "the underline is too: {:?}",
        bar(1)
    );
    assert!(
        bar(2)[0] + 40 < bar(0)[0],
        "dim darkens the underline: {:?}",
        bar(2)
    );
}

/// Overline (D4): a bar at the top of the cell, in the underline colour
/// when set, else the foreground.
#[test]
fn overline_sits_on_top_in_the_underline_colour() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let over = Style::DEFAULT.with(Style::OVERLINE);
    let colored = Style {
        underline: Color::Rgb(0, 255, 0),
        ..over
    };
    let screen = Screen::with(&[("  ", over), ("  ", colored)], 2);
    let target = Target::new(&gpu, &renderer, 2, 2);
    let cells = screen.rows();
    let image = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let bg = rgb(theme.background);
    let lit: Vec<usize> = ink_rows(&target, &image, 0, 0, bg)
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(y, _)| y)
        .collect();
    assert_eq!(lit, [0, 1], "the top two pixel rows at 2x");
    let (x, y) = target.origin(0, 0);
    assert!(near(
        target.pixel(&image, x + 3, y),
        rgb(theme.foreground),
        1
    ));
    let (x, y) = target.origin(1, 0);
    assert!(near(target.pixel(&image, x + 3, y), [0, 255, 0], 1));
}

/// Superscript and subscript (D5, D7) draw the same glyph smaller, higher
/// or lower; with both flags superscript wins.
#[test]
fn scripts_shrink_and_shift_the_glyph() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let d = Style::DEFAULT;
    let rows = [
        ("x", d),
        ("x", d.with(Style::SUPERSCRIPT)),
        ("x", d.with(Style::SUBSCRIPT)),
        ("x", d.with(Style::SUPERSCRIPT | Style::SUBSCRIPT)),
    ];
    let screen = Screen::with(&rows, 1);
    let target = Target::new(&gpu, &renderer, 4, 1);
    let cells = screen.rows();
    let image = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let bg = rgb(theme.background);
    // (first ink row, last ink row, ink pixels) of each cell.
    let extent = |row| {
        let ink = ink_rows(&target, &image, row, 0, bg);
        let lit: Vec<usize> = ink
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(y, _)| y)
            .collect();
        (lit[0], lit[lit.len() - 1], ink.iter().sum::<usize>())
    };
    let (top, bottom, regular) = extent(0);
    let (high_top, high_bottom, high) = extent(1);
    let (low_top, low_bottom, low) = extent(2);
    let height = |(t, b): (usize, usize)| b - t + 1;
    let h = height((top, bottom)) as f32;
    for (name, t, b, ink) in [
        ("super", high_top, high_bottom, high),
        ("low", low_top, low_bottom, low),
    ] {
        let scaled = height((t, b)) as f32 / h;
        assert!(
            (0.55..=0.85).contains(&scaled),
            "{name}: {scaled} of the height"
        );
        assert!(ink < regular, "{name}: less ink ({ink} vs {regular})");
    }
    assert!(
        high_bottom + 4 < bottom,
        "superscript rises: {high_bottom} vs {bottom}"
    );
    assert!(
        low_bottom > bottom + 2,
        "subscript drops: {low_bottom} vs {bottom}"
    );
    assert!(low_bottom < target.cell[1] as usize, "and stays in its row");
    assert_eq!(
        target.cells(&image, 3, 0, 1),
        target.cells(&image, 1, 0, 1),
        "superscript wins"
    );
}

// ------------------------------------------------------------ cursor

/// The block cursor on either half of a wide character covers both
/// columns and redraws the whole glyph; on its spacer it draws exactly
/// what it draws on the wide half.
#[test]
fn a_block_cursor_on_a_wide_character_covers_both_columns() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let screen = Screen::with(&[("a中b", Style::DEFAULT)], 5);
    let target = Target::new(&gpu, &renderer, 1, 5);
    let cells = screen.rows();
    let at = |col| {
        FrameInput::new(&cells).with_cursor(Some(CursorState::new(0, col, CursorShape::Block)))
    };
    let on_wide = target.draw(&gpu, &mut renderer, &at(1));
    let on_spacer = target.draw(&gpu, &mut renderer, &at(2));
    assert!(on_wide == on_spacer, "same cursor on either half");
    let both = target.cells(&on_wide, 0, 1, 2);
    let cursor = count(&both, rgb(theme.cursor));
    let ink = count(&both, rgb(theme.background));
    assert!(
        cursor > both.len() / 2,
        "both columns are cursor-coloured ({cursor})"
    );
    assert!(
        ink > 60,
        "the glyph is redrawn in the background colour ({ink})"
    );
    // Both halves of the glyph are redrawn: each column has ink.
    for col in [1, 2] {
        let half = target.cells(&on_wide, 0, col, 1);
        assert!(count(&half, rgb(theme.background)) > 20, "column {col}");
    }
    let plain = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    assert!(
        target.differ_only_in(&plain, &on_wide, 0, 1..3),
        "nothing outside the cursor"
    );
}

/// The redraw clip is exactly the cursor's cells: a full block glyph
/// under the cursor is redrawn edge to edge (a clip shrunk by a pixel
/// leaves cursor-coloured columns), and a ligature's redraw does not
/// spill into its other cell (no clip paints it).
#[test]
fn the_redraw_is_clipped_to_the_cursor_exactly() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let screen = Screen::with(&[("\u{2588}\u{2588}", Style::DEFAULT)], 3);
    let target = Target::new(&gpu, &renderer, 1, 3);
    let cells = screen.rows();
    let frame =
        FrameInput::new(&cells).with_cursor(Some(CursorState::new(0, 0, CursorShape::Block)));
    let image = target.draw(&gpu, &mut renderer, &frame);
    let cell = target.cells(&image, 0, 0, 1);
    let inverted = count(&cell, rgb(theme.background));
    assert_eq!(
        inverted,
        cell.len(),
        "the full block is redrawn over the whole cell"
    );
    let neighbour = target.cells(&image, 0, 1, 1);
    assert_eq!(
        count(&neighbour, rgb(theme.foreground)),
        neighbour.len(),
        "and not beyond"
    );

    let Some(mut lig) = ligature_renderer(&gpu) else {
        return;
    };
    let screen = Screen::with(&[("=>", Style::DEFAULT)], 3);
    let target = Target::new(&gpu, &lig, 1, 3);
    let cells = screen.rows();
    let plain = target.draw(&gpu, &mut lig, &FrameInput::new(&cells));
    let frame =
        FrameInput::new(&cells).with_cursor(Some(CursorState::new(0, 0, CursorShape::Block)));
    let image = target.draw(&gpu, &mut lig, &frame);
    assert!(
        target.differ_only_in(&plain, &image, 0, 0..1),
        "the ligature's redraw stays in the cursor"
    );
}

/// A block cursor on the second cell of a ligature redraws the part of the
/// ligature in that cell (the whole cluster, from the shaped row), so the
/// cell shows background-coloured ink.
#[test]
fn a_block_cursor_on_a_ligatures_second_cell_redraws_the_ligature() {
    let Some(gpu) = gpu() else { return };
    let Some(mut renderer) = ligature_renderer(&gpu) else {
        return;
    };
    let theme = Theme::default();
    let screen = Screen::with(&[("=>", Style::DEFAULT)], 3);
    let target = Target::new(&gpu, &renderer, 1, 3);
    let cells = screen.rows();
    let frame =
        FrameInput::new(&cells).with_cursor(Some(CursorState::new(0, 1, CursorShape::Block)));
    let image = target.draw(&gpu, &mut renderer, &frame);
    let cell = target.cells(&image, 0, 1, 1);
    assert!(
        count(&cell, rgb(theme.background)) > 20,
        "the arrow head is redrawn"
    );
}

/// Emoji under the block cursor are redrawn in their own colours, and
/// decorations under it are redrawn in the cursor's text colour (the
/// cell's background, swapped by INVERSE).
#[test]
fn the_block_cursor_redraws_emoji_and_decorations() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let inverse_underline = Style::DEFAULT.with(Style::INVERSE | Style::UNDERLINE);
    let screen = Screen::with(
        &[
            ("\u{1F600}", Style::DEFAULT),
            ("  ", inverse_underline),
            ("  ", Style::DEFAULT.with(Style::UNDERLINE)),
        ],
        3,
    );
    let target = Target::new(&gpu, &renderer, 3, 3);
    let cells = screen.rows();
    let clusters = screen.clusters();
    let colourful = |pixels: &[[u8; 4]]| {
        pixels
            .iter()
            .filter(|p| p[0].abs_diff(p[1]) > 40 || p[1].abs_diff(p[2]) > 40)
            .count()
    };
    let cursor = |row, col| {
        FrameInput::new(&cells)
            .with_clusters(&clusters)
            .with_cursor(Some(CursorState::new(row, col, CursorShape::Block)))
    };
    let emoji = target.draw(&gpu, &mut renderer, &cursor(0, 0));
    let n = colourful(&target.cells(&emoji, 0, 0, 2));
    assert!(
        n > 100,
        "the emoji keeps its colours under the cursor ({n})"
    );
    // Inverse + underline: the cursor's text colour is the swapped
    // background, the default foreground; the underline rows of the cell
    // show it over the cursor colour.
    let image = target.draw(&gpu, &mut renderer, &cursor(1, 0));
    let (x, y) = target.origin(1, 0);
    assert!(
        near(
            target.pixel(&image, x + 4, y + 26),
            rgb(theme.foreground),
            1
        ),
        "inverse"
    );
    assert!(near(
        target.pixel(&image, x + 4, y + 10),
        rgb(theme.cursor),
        1
    ));
    let image = target.draw(&gpu, &mut renderer, &cursor(2, 0));
    let (x, y) = target.origin(2, 0);
    assert!(
        near(
            target.pixel(&image, x + 4, y + 26),
            rgb(theme.background),
            1
        ),
        "plain"
    );
}

/// A wide character whose right half falls past the visible columns is not
/// drawn (it would draw into the padding); its background still is.
#[test]
fn a_wide_character_cut_by_the_right_edge_is_not_drawn() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = menlo(&gpu);
    let screen = Screen::with(&[("abc中", Style::DEFAULT)], 6);
    let cells = screen.rows();
    // Room for three columns and the padding, in which a fourth column
    // starts: the wide character begins in that last, partly visible
    // column and its right half is past the target.
    let target = Target::new(&gpu, &renderer, 1, 3);
    let image = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let blank = Screen::with(&[("abc", Style::DEFAULT)], 6);
    let blank_cells = blank.rows();
    let expected = target.draw(&gpu, &mut renderer, &FrameInput::new(&blank_cells));
    assert!(
        image == expected,
        "no ink of the cut character, not even in the padding"
    );
    // One column more and it is drawn.
    let wider = Target::new(&gpu, &renderer, 1, 4);
    let drawn = wider.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
    let without = wider.draw(&gpu, &mut renderer, &FrameInput::new(&blank_cells));
    assert!(drawn != without, "a whole wide character is drawn");
}

/// The ADR 0016 case on the cursor side: `->` with a combining mark on
/// `>`. Whether the shaper keeps `>` in the arrow's span or gives it a
/// cluster of its own, a block cursor on that cell redraws the arrow's ink
/// there. (Cascadia Code keeps the span on macOS 26, so the ink path
/// itself is pinned by the `⟶` test below.)
#[test]
fn a_block_cursor_on_a_marked_ligature_end_redraws_the_arrow() {
    let Some(gpu) = gpu() else { return };
    let Some(mut renderer) = ligature_renderer(&gpu) else {
        return;
    };
    let theme = Theme::default();
    let screen = Screen::with(&[("->\u{301}", Style::DEFAULT)], 3);
    let target = Target::new(&gpu, &renderer, 1, 3);
    let cells = screen.rows();
    let clusters = screen.clusters();
    let frame = FrameInput::new(&cells).with_clusters(&clusters);
    let plain = target.draw(&gpu, &mut renderer, &frame);
    let ink = |image: &[u8], c| count(&target.cells(image, 0, 1, 1), c);
    let lit = ink(&plain, rgb(theme.foreground));
    let cursor = frame.with_cursor(Some(CursorState::new(0, 1, CursorShape::Block)));
    let image = target.draw(&gpu, &mut renderer, &cursor);
    let redrawn = ink(&image, rgb(theme.background));
    assert!(
        lit > 20,
        "the arrow and the mark have ink in the `>` cell ({lit})"
    );
    assert!(
        redrawn * 10 >= lit * 8,
        "most of it is redrawn under the cursor: {redrawn} of {lit}"
    );
}

/// Font-independent of ligatures: `⟶` (U+27F6, from the fallback chain)
/// draws past its cell into the next, which holds an `x` of its own, so
/// the arrow's span stays one cell. A block cursor on the `x` redraws both
/// the `x` and the arrow's ink in that cell (the arrow selected by ink),
/// so the cell differs from a cursor on an `x` after a blank. A cursor on
/// the arrow leaves the `x` cell untouched.
#[test]
fn a_block_cursor_redraws_ink_spilling_in_from_the_previous_cell() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let target = Target::new(&gpu, &renderer, 1, 3);
    let x = Cell::new('x', Style::DEFAULT);
    let arrow = [Cell::new('\u{27F6}', Style::DEFAULT), x, Cell::BLANK];
    let blank = [Cell::BLANK, x, Cell::BLANK];
    let (arrow_rows, blank_rows): ([&[Cell]; 1], [&[Cell]; 1]) = ([&arrow], [&blank]);
    // Pixels of cell 1 that are not `c`.
    let off = |image: &[u8], c: [u8; 3]| {
        target
            .cells(image, 0, 1, 1)
            .iter()
            .filter(|p| !near(**p, c, 8))
            .count()
    };
    let bg = rgb(theme.background);
    let plain = target.draw(&gpu, &mut renderer, &FrameInput::new(&arrow_rows));
    let alone = target.draw(&gpu, &mut renderer, &FrameInput::new(&blank_rows));
    let (with_arrow, x_only) = (off(&plain, bg), off(&alone, bg));
    eprintln!("ink in cell 1: {with_arrow} with the arrow, {x_only} for the x alone");
    if with_arrow < x_only + 5 {
        eprintln!("skipping: this system's U+27F6 does not reach the next cell");
        return;
    }
    let cursor = Some(CursorState::new(0, 1, CursorShape::Block));
    let image = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&arrow_rows).with_cursor(cursor),
    );
    let x_cursor = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&blank_rows).with_cursor(cursor),
    );
    // Under the cursor everything that is not cursor colour was redrawn.
    let c = rgb(theme.cursor);
    let (redrawn, x_redrawn) = (off(&image, c), off(&x_cursor, c));
    assert!(
        redrawn >= x_redrawn + (with_arrow - x_only) / 2,
        "the arrow's ink is redrawn too: {redrawn} vs {x_redrawn} px for the x alone"
    );
    // And on the arrow itself the redraw stays in its cell: the spilled
    // part is not repainted over the `x` (the clip, on system fonts).
    let on_arrow = Some(CursorState::new(0, 0, CursorShape::Block));
    let image = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&arrow_rows).with_cursor(on_arrow),
    );
    assert!(
        target.differ_only_in(&plain, &image, 0, 0..1),
        "the redraw stays in the cursor"
    );
}

/// Superscript and subscript glyphs stay inside their row (#22 review): a
/// lowered descender or a raised accent would paint into the neighbouring
/// rows, which only redraw when they are damaged themselves.
#[test]
fn scripts_stay_in_their_row() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    for scale in [1.0, 2.0] {
        let mut renderer = renderer_with(&gpu, Theme::default(), "Menlo", scale).expect("Menlo");
        let d = Style::DEFAULT;
        let screen = Screen::with(
            &[
                ("      ", d),
                ("pgjy", d.with(Style::SUBSCRIPT)),
                ("      ", d),
                ("\u{c5}\u{c9}|[", d.with(Style::SUPERSCRIPT)),
                ("      ", d),
            ],
            6,
        );
        let target = Target::new(&gpu, &renderer, 5, 6);
        let cells = screen.rows();
        let image = target.draw(&gpu, &mut renderer, &FrameInput::new(&cells));
        let bg = rgb(theme.background);
        for row in [0, 2, 4] {
            let stray = target
                .cells(&image, row, 0, 6)
                .iter()
                .filter(|p| !near(**p, bg, 0))
                .count();
            assert_eq!(stray, 0, "{scale}x: {stray} px of script ink in row {row}");
        }
        for row in [1, 3] {
            let ink = target
                .cells(&image, row, 0, 4)
                .iter()
                .filter(|p| !near(**p, bg, 8))
                .count();
            assert!(
                ink > 20,
                "{scale}x: row {row} still draws its glyphs ({ink})"
            );
        }
    }
}

/// DIM under the block cursor (#22 review, lead decision): the redrawn
/// glyph and the redrawn underline are the cell's background dimmed, as
/// the cell's foreground would be.
#[test]
fn the_block_cursor_honours_dim() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = menlo(&gpu);
    let bright = Style {
        bg: Color::Rgb(255, 255, 255),
        flags: Style::UNDERLINE,
        ..Style::DEFAULT
    };
    let screen = Screen::with(&[("X ", bright), ("X ", bright.with(Style::DIM))], 2);
    let target = Target::new(&gpu, &renderer, 2, 2);
    let cells = screen.rows();
    let cursor = |row| {
        FrameInput::new(&cells).with_cursor(Some(CursorState::new(row, 0, CursorShape::Block)))
    };
    let plain = target.draw(&gpu, &mut renderer, &cursor(0));
    let dimmed = target.draw(&gpu, &mut renderer, &cursor(1));
    let white = [255, 255, 255];
    let grey = [170, 170, 170];
    let (x, y0) = target.origin(0, 0);
    let (_, y1) = target.origin(1, 0);
    assert!(
        near(target.pixel(&plain, x + 4, y0 + 26), white, 1),
        "underline, plain"
    );
    assert!(
        near(target.pixel(&dimmed, x + 4, y1 + 26), grey, 2),
        "underline, dim"
    );
    assert!(
        count(&target.cells(&plain, 0, 0, 1), white) > 30,
        "glyph, plain"
    );
    let glyph = target.cells(&dimmed, 1, 0, 1);
    assert!(count(&glyph, grey) > 30, "the glyph is redrawn dimmed");
    assert_eq!(count(&glyph, white), 0, "nothing undimmed under the cursor");
}

/// Scaled (superscript and subscript) glyphs sample their bitmap by their
/// position inside it, so the same glyph draws the same pixels wherever
/// it sits in the atlas (#22 review: absolute atlas coordinates made
/// bilinear weights differ by one step between slots).
#[test]
fn scripts_draw_the_same_pixels_in_any_atlas_slot() {
    let Some(gpu) = gpu() else { return };
    let d = Style::DEFAULT;
    let text = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let filler: String = ('\u{100}'..='\u{17f}')
        .chain('\u{391}'..='\u{3c9}')
        .collect();
    let mut total = 0;
    for size_pt in [11.0, 13.0, 15.0, 17.0, 19.0] {
        let make = || {
            let shaper = Shaper::new(&FontConfig {
                family: "Menlo".into(),
                size_pt,
                scale: 2.0,
                fallback: Vec::new(),
            })
            .expect("fonts");
            Renderer::new(&gpu, Theme::default(), shaper).expect("renderer")
        };
        let screen = Screen::with(
            &[
                (text, d.with(Style::SUBSCRIPT)),
                (text, d.with(Style::SUPERSCRIPT)),
            ],
            64,
        );
        let cells = screen.rows();
        let mut fresh = make();
        let target = Target::new(&gpu, &fresh, 2, 64);
        let expected = target.draw(&gpu, &mut fresh, &FrameInput::new(&cells));
        // A renderer whose atlas already holds other glyphs, so the same
        // glyphs land in other slots.
        let mut used = make();
        for flags in [Style::BOLD, Style::ITALIC, Style::BOLD | Style::ITALIC, 0] {
            let fill = Screen::with(&[(filler.as_str(), d.with(flags)); 2], 64);
            let fill_cells = fill.rows();
            target.draw(&gpu, &mut used, &FrameInput::new(&fill_cells));
        }
        let drawn = target.draw(&gpu, &mut used, &FrameInput::new(&cells));
        let differing = drawn
            .chunks(4)
            .zip(expected.chunks(4))
            .filter(|(a, b)| a != b)
            .count();
        eprintln!("{size_pt} pt: {differing} px differ");
        total += differing;
    }
    assert_eq!(total, 0, "{total} px depend on the atlas slot");
}

/// Dotted underlines are phased from the grid's left edge, not from each
/// run, so with an odd cell width the dots of neighbouring runs (here every
/// cell has another underline colour) keep one pitch (#22 review).
#[test]
fn dotted_underlines_keep_their_pitch_across_runs() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let shaper = Shaper::new(&FontConfig {
        family: "Menlo".into(),
        size_pt: 12.0,
        scale: 1.0,
        fallback: Vec::new(),
    })
    .expect("fonts");
    let width = shaper.cell_metrics().width_px;
    assert_eq!(width % 2, 1, "Menlo 12 pt at 1x has odd cells ({width} px)");
    let mut renderer = Renderer::new(&gpu, Theme::default(), shaper).expect("renderer");
    let dotted = Style::DEFAULT.with(Style::UNDERLINE | Style::DOTTED_UNDERLINE);
    let cells: Vec<Cell> = (0..8u8)
        .map(|i| {
            let underline = Color::Indexed(1 + i % 2);
            Cell::new(
                ' ',
                Style {
                    underline,
                    ..dotted
                },
            )
        })
        .collect();
    let rows: [&[Cell]; 1] = [&cells];
    let target = Target::new(&gpu, &renderer, 1, 8);
    let image = target.draw(&gpu, &mut renderer, &FrameInput::new(&rows));
    let bg = rgb(theme.background);
    let (x0, y0) = target.origin(0, 0);
    // The underline's pixel row: the one with the most ink.
    let y = (y0..y0 + target.cell[1])
        .max_by_key(|&y| {
            (x0..x0 + 8 * width)
                .filter(|&x| !near(target.pixel(&image, x, y), bg, 8))
                .count()
        })
        .expect("a row");
    let lit: Vec<u32> = (x0..x0 + 8 * width)
        .filter(|&x| !near(target.pixel(&image, x, y), bg, 8))
        .map(|x| x - x0)
        .collect();
    assert!(lit.len() > 20, "dots are drawn: {lit:?}");
    // Thickness 1 at this size: a dot every second pixel from the edge.
    assert!(lit.iter().all(|x| x % 2 == 0), "dots out of phase: {lit:?}");
}

/// A hovered link's underline (an overlay) stays visible under the block
/// cursor, redrawn in the cursor's text colour like the cell's own
/// underline (#22 review).
#[test]
fn the_block_cursor_redraws_a_link_underline() {
    let Some(gpu) = gpu() else { return };
    let theme = Theme::default();
    let mut renderer = menlo(&gpu);
    let screen = Screen::with(&[("link", Style::DEFAULT)], 5);
    let target = Target::new(&gpu, &renderer, 1, 5);
    let cells = screen.rows();
    let overlays = [Overlay::new(OverlayKind::Underline, 0, 0..4)];
    let cursor = Some(CursorState::new(0, 1, CursorShape::Block));
    let frame = FrameInput::new(&cells).with_overlays(&overlays);
    let image = target.draw(&gpu, &mut renderer, &frame.clone().with_cursor(cursor));
    let (x, y) = target.origin(0, 1);
    let bar = target.pixel(&image, x + 1, y + 26);
    assert!(
        near(bar, rgb(theme.background), 1),
        "the link bar shows: {bar:?}"
    );
    let above = target.pixel(&image, x + 1, y + 24);
    assert!(near(above, rgb(theme.cursor), 1), "only the bar: {above:?}");
    // No link: the cursor covers the underline position.
    let plain = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&cells).with_cursor(cursor),
    );
    assert!(near(
        target.pixel(&plain, x + 1, y + 26),
        rgb(theme.cursor),
        1
    ));
}
