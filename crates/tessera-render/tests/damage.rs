//! Damage tracking in the renderer (ADR 0016): rows outside
//! `FrameInput::damage` are drawn from the previous frame's work, and an
//! incremental frame draws exactly the pixels a full frame of the same
//! content draws.
//!
//! `TESSERA_SKIP_GPU_TESTS=1` skips the suite like the other GPU tests.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel and cell counts in these tests are small and non-negative"
)]

use tessera_grid::{Cell, Color, DamageRows, GraphemeTable, Grid, Style};
use tessera_render::{
    CursorShape, CursorState, FontConfig, FrameInput, Gpu, OffscreenTarget, PhysicalSize, Renderer,
    Shaper, Theme,
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

fn menlo() -> Shaper {
    Shaper::new(&FontConfig {
        family: "Menlo".into(),
        size_pt: 13.0,
        scale: 1.0,
        fallback: Vec::new(),
    })
    .expect("Menlo ships with macOS")
}

fn renderer(gpu: &Gpu, atlas_size: Option<u32>) -> Renderer {
    let theme = Theme::default();
    match atlas_size {
        Some(size) => Renderer::with_atlas_size(gpu, theme, menlo(), size),
        None => Renderer::new(gpu, theme, menlo()),
    }
    .expect("renderer")
}

/// A screen built through the grid, so wide characters, spacers and
/// clusters are laid out as the emulator lays them out.
struct Screen {
    grid: Grid,
}

impl Screen {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            grid: Grid::new(rows, cols),
        }
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

struct Target {
    target: OffscreenTarget,
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
            target: OffscreenTarget::new(gpu, size, FORMAT),
        }
    }

    fn draw(&self, gpu: &Gpu, renderer: &mut Renderer, frame: &FrameInput<'_>) -> Vec<u8> {
        self.target.render(gpu, renderer, frame).expect("render");
        self.target.read_back(gpu).expect("readback")
    }
}

fn style(fg: u8, bg: Option<u8>, flags: u16) -> Style {
    Style {
        fg: Color::Indexed(fg),
        bg: bg.map_or(Color::Default, Color::Indexed),
        flags,
        ..Style::DEFAULT
    }
}

#[test]
fn an_undamaged_frame_shapes_nothing_and_draws_the_same_pixels() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, None);
    let mut screen = Screen::new(4, 20);
    screen.write(0, "hello, world", Style::DEFAULT);
    screen.write(2, "中文 and e\u{301}", style(2, Some(4), Style::UNDERLINE));
    let target = Target::new(&gpu, &renderer, 4, 20);
    let rows = screen.rows();
    let clusters = screen.clusters();
    let cursor = Some(CursorState::new(1, 3, CursorShape::Block));
    let frame = FrameInput::new(&rows)
        .with_clusters(&clusters)
        .with_cursor(cursor);
    let first = target.draw(&gpu, &mut renderer, &frame);
    let before = renderer.stats();
    assert_eq!(before.rows_shaped, 4);
    let again = target.draw(
        &gpu,
        &mut renderer,
        &frame.clone().with_damage(DamageRows::None),
    );
    let after = renderer.stats();
    assert_eq!(after.rows_shaped, before.rows_shaped, "nothing re-shaped");
    assert_eq!(after.rows_built, before.rows_built, "nothing rebuilt");
    assert_eq!(after.relayouts, before.relayouts);
    assert_eq!(
        after.instances_uploaded, before.instances_uploaded,
        "nothing is uploaded, not even the unchanged cursor"
    );
    assert!(first == again, "an undamaged frame draws the same pixels");
}

#[test]
fn one_damaged_row_is_the_only_row_shaped_and_uploaded() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, None);
    let mut screen = Screen::new(6, 30);
    for row in 0..6 {
        screen.write(
            row,
            &format!("row {row}: the quick brown fox"),
            Style::DEFAULT,
        );
    }
    let target = Target::new(&gpu, &renderer, 6, 30);
    {
        let rows = screen.rows();
        target.draw(&gpu, &mut renderer, &FrameInput::new(&rows));
    }
    let before = renderer.stats();
    screen.write(
        3,
        "row 3: jumps over the lazy dog",
        style(1, None, Style::BOLD),
    );
    let rows = screen.rows();
    let incremental = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&rows).with_damage(DamageRows::Rows(3..=3)),
    );
    let after = renderer.stats();
    assert_eq!(after.rows_shaped - before.rows_shaped, 1);
    assert_eq!(after.rows_built - before.rows_built, 1);
    assert_eq!(after.relayouts, before.relayouts, "the row fits its slot");
    // One row's three slots (at most 64 glyph slots for 30 cells, 4 + 4
    // background and decoration) and the cursor slot.
    let uploaded = after.instances_uploaded - before.instances_uploaded;
    assert!(uploaded <= 64 + 4 + 4 + 16, "uploaded {uploaded} instances");

    let mut reference = renderer_for_reference(&gpu);
    let full = target.draw(&gpu, &mut reference, &FrameInput::new(&rows));
    assert!(
        incremental == full,
        "the incremental frame matches a full one"
    );
}

fn renderer_for_reference(gpu: &Gpu) -> Renderer {
    renderer(gpu, None)
}

/// A deterministic xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const SAMPLES: &[&str] = &[
    "the quick brown fox",
    "-> => != ===",
    "中文字 かな",
    "e\u{301}a\u{308} n\u{303}",
    "\u{1F600} \u{1F44D}\u{1F3FD} \u{2764}\u{FE0F}",
    "\u{2500}\u{2500}\u{252C}\u{2500}\u{2510}",
    "",
    "    indented",
];

/// The fast path (incremental frames with exact damage) agrees with the
/// reference path (a full frame) on every step of a random edit sequence,
/// byte for byte, including when a row grows past its slot.
#[test]
fn incremental_frames_match_full_frames() {
    let Some(gpu) = gpu() else { return };
    let (rows_n, cols) = (8u16, 40u16);
    let mut fast = renderer(&gpu, None);
    let mut reference = renderer(&gpu, None);
    let target = Target::new(&gpu, &fast, rows_n, cols);
    let mut screen = Screen::new(rows_n, cols);
    screen.grid.take_damage();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut cursor = CursorState::new(0, 0, CursorShape::Block);
    let mut focused = true;
    {
        let rows = screen.rows();
        target.draw(&gpu, &mut fast, &FrameInput::new(&rows));
    }
    let mut partial = 0;
    let mut damaged_rows = u64::from(rows_n);
    for step in 0..40 {
        // Edit one or two rows (the damage is the range covering them).
        let a = rng.below(u64::from(rows_n)) as u16;
        let b = if rng.below(3) == 0 {
            rng.below(u64::from(rows_n)) as u16
        } else {
            a
        };
        for row in [a, b] {
            let mut text = String::new();
            for _ in 0..=rng.below(4) {
                text.push_str(SAMPLES[rng.below(SAMPLES.len() as u64) as usize]);
                text.push(' ');
            }
            let flags = [
                0,
                Style::BOLD,
                Style::UNDERLINE,
                Style::INVERSE,
                Style::HIDDEN,
            ][rng.below(5) as usize];
            let bg = (rng.below(3) == 0).then(|| rng.below(8) as u8);
            screen.write(row, &text, style(rng.below(16) as u8, bg, flags));
        }
        if rng.below(2) == 0 {
            cursor = CursorState::new(
                rng.below(u64::from(rows_n)) as u16,
                rng.below(u64::from(cols)) as u16,
                [
                    CursorShape::Block,
                    CursorShape::Beam,
                    CursorShape::Underline,
                ][rng.below(3) as usize],
            );
        }
        if rng.below(5) == 0 {
            focused = !focused;
        }
        // The grid's own damage: a write that wraps or scrolls reaches
        // past the edited rows.
        let damage = screen.grid.take_damage().rows;
        partial += usize::from(damage != DamageRows::All);
        damaged_rows += match &damage {
            DamageRows::None => 0,
            DamageRows::Rows(r) => u64::from(r.end() - r.start() + 1),
            DamageRows::All => u64::from(rows_n),
        };
        let rows = screen.rows();
        let clusters = screen.clusters();
        let frame = FrameInput::new(&rows)
            .with_clusters(&clusters)
            .with_cursor(Some(cursor))
            .with_focus(focused);
        let incremental = target.draw(&gpu, &mut fast, &frame.clone().with_damage(damage.clone()));
        let full = target.draw(&gpu, &mut reference, &frame);
        if incremental != full {
            let width = target.target.size().width as usize;
            let m = fast.cell_metrics();
            let pad = (fast.theme().padding * fast.shaper().scale()).round() as usize;
            let mut cells = std::collections::BTreeSet::new();
            for (i, (x, y)) in incremental.chunks(4).zip(full.chunks(4)).enumerate() {
                if x != y {
                    let (px, py) = (i % width, i / width);
                    cells.insert((
                        py.saturating_sub(pad) / m.height_px as usize,
                        px.saturating_sub(pad) / m.width_px as usize,
                    ));
                }
            }
            panic!(
                "step {step}: incremental frame differs from the full frame in cells {cells:?} \
                 (damage {damage:?}, cursor {cursor:?})\n{:?}",
                screen
                    .grid
                    .visible_lines()
                    .iter()
                    .map(tessera_grid::Line::text)
                    .collect::<Vec<_>>()
            );
        }
    }
    assert!(partial > 20, "most steps were partial: {partial}");
    // Exactly the damaged rows were shaped (no fallback is pending for
    // these characters).
    assert_eq!(fast.stats().rows_shaped, damaged_rows);
}

/// Rows kept from an earlier frame reference atlas slots; when a damaged
/// row's new glyphs evict theirs, the renderer rebuilds them, so the frame
/// still matches one drawn with an atlas that never fills.
#[test]
fn an_eviction_rebuilds_the_rows_it_affects() {
    let Some(gpu) = gpu() else { return };
    let (rows_n, cols) = (2u16, 24u16);
    // 96x96 texels hold a few dozen Menlo glyphs at 1x: both rows fit,
    // but not every alphabet row 0 shows over time.
    let mut small = renderer(&gpu, Some(96));
    let mut reference = renderer(&gpu, None);
    let target = Target::new(&gpu, &small, rows_n, cols);
    let mut screen = Screen::new(rows_n, cols);
    // Row 1 never changes after the first frame, so its glyphs age in the
    // atlas and are the first to be evicted.
    screen.write(1, "kept: wxyz 789", Style::DEFAULT);
    let alphabets = [
        "ABCDEFGHIJKLMNOPQRSTUV",
        "abcdefghijklmnopqrstuv",
        "0123456!@#$%^&*()[]{}<",
        "ÀÁÂÃÄÅÆÇÈÉÊËÌÍÎÏÐÑÒÓÔÕ",
        "àáâãäåæçèéêëìíîïðñòóôõ",
        "ABCDEFGHIJKLMNOPQRSTUV",
    ];
    for (step, text) in alphabets.iter().enumerate() {
        screen.write(0, text, Style::DEFAULT);
        let damage = screen.grid.take_damage().rows;
        assert!(step == 0 || damage == DamageRows::Rows(0..=0), "{damage:?}");
        let rows = screen.rows();
        let frame = FrameInput::new(&rows);
        let incremental = target.draw(&gpu, &mut small, &frame.clone().with_damage(damage));
        let full = target.draw(&gpu, &mut reference, &frame);
        assert!(
            incremental == full,
            "step {step}: stale glyphs after eviction"
        );
    }
    let stats = small.atlas_stats();
    assert!(
        stats.evicted > 0,
        "the atlas evicted glyphs, so the test exercised the rebuild: {stats:?}"
    );
    assert!(
        small.stats().rows_built > small.stats().rows_shaped,
        "kept rows were rebuilt without being shaped again: {:?}",
        small.stats()
    );
}

#[test]
fn a_theme_change_rebuilds_every_row() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = renderer(&gpu, None);
    let mut screen = Screen::new(3, 10);
    screen.write(0, "abc", Style::DEFAULT);
    let target = Target::new(&gpu, &renderer, 3, 10);
    let rows = screen.rows();
    target.draw(&gpu, &mut renderer, &FrameInput::new(&rows));
    let before = renderer.stats();
    assert!(!renderer.needs_redraw());
    let theme = Theme {
        foreground: tessera_render::Rgba::new(1.0, 0.0, 0.0, 1.0),
        ..Theme::default()
    };
    renderer.set_theme(theme);
    assert!(renderer.needs_redraw());
    let red = target.draw(
        &gpu,
        &mut renderer,
        &FrameInput::new(&rows).with_damage(DamageRows::None),
    );
    assert_eq!(renderer.stats().rows_shaped - before.rows_shaped, 3);
    let mut reference = renderer_for_reference(&gpu);
    reference.set_theme(renderer.theme().clone());
    let full = target.draw(&gpu, &mut reference, &FrameInput::new(&rows));
    assert!(red == full, "the new theme is drawn on every row");
}
