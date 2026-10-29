//! Allocations per frame (#21): the renderer reuses each row's shaped
//! glyph and instance vectors, so a frame's allocation count does not grow
//! after the second frame, and a frame that re-shapes every row allocates
//! nothing per row beyond what shaping itself needs.
//!
//! Allocations are counted per thread, so the font coverage index (built
//! on a background thread) and wgpu's own threads do not disturb the
//! count. `TESSERA_SKIP_GPU_TESTS=1` skips the suite.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;

use tessera_grid::{Cell, DamageRows, GraphemeTable, Point, Style};
use tessera_render::{
    FontConfig, FrameInput, Gpu, OffscreenTarget, Overlay, OverlayKind, PhysicalSize, Renderer,
    Selection, Shaper, Theme,
};

/// The system allocator, counting the current thread's allocations.
struct Counting;

thread_local! {
    static ALLOCATIONS: StdCell<u64> = const { StdCell::new(0) };
}

// SAFETY: every call is forwarded to `System` unchanged; the counter is a
// const-initialised thread local that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(StdCell::get)
}

/// Rows and columns of the frames.
const ROWS: u32 = 24;
const COLS: u32 = 80;

fn config() -> FontConfig {
    FontConfig {
        family: "Menlo".into(),
        size_pt: 13.0,
        scale: 1.0,
        fallback: Vec::new(),
    }
}

#[test]
fn frames_stop_allocating_per_row_after_the_second() {
    if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
        return;
    }
    let gpu = Gpu::headless().expect("a Metal adapter");
    let shaper = Shaper::new(&config()).expect("Menlo ships with macOS");
    let mut renderer = Renderer::new(&gpu, Theme::default(), shaper).expect("renderer");
    let m = renderer.cell_metrics();
    let size = PhysicalSize::new(m.width_px * COLS + 8, m.height_px * ROWS + 8);
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8UnormSrgb);
    let line: Vec<Cell> =
        "the quick brown fox jumps over the lazy dog 0123456789 ~!@#$%^&*()_+-=[]{} "
            .chars()
            .cycle()
            .take(COLS as usize)
            .map(|ch| Cell::new(ch, Style::DEFAULT))
            .collect();
    let rows: Vec<&[Cell]> = vec![line.as_slice(); ROWS as usize];
    let clusters = vec![GraphemeTable::new(); ROWS as usize];
    let mut frame = |damage: DamageRows| {
        let before = allocations();
        let frame = FrameInput::new(&rows)
            .with_clusters(&clusters)
            .with_damage(damage);
        target.render(&gpu, &mut renderer, &frame).expect("render");
        gpu.device()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        allocations() - before
    };
    let full: Vec<u64> = (0..6).map(|_| frame(DamageRows::All)).collect();
    let idle: Vec<u64> = (0..3).map(|_| frame(DamageRows::None)).collect();

    eprintln!("allocations per full frame {full:?}, per undamaged frame {idle:?}");
    for (i, &n) in full.iter().enumerate().skip(2) {
        assert!(
            n <= full[1],
            "frame {} allocated {n} > {} (frame 2)",
            i + 1,
            full[1]
        );
    }
    // What shaping one row costs on its own (harfrust's buffers, the run
    // text), measured with a warmed-up shaper into a reused vector.
    let mut shaper = Shaper::new(&config()).expect("fonts");
    let mut out = Vec::new();
    let empty = GraphemeTable::new();
    shaper.shape_line_into(&line, &empty, &mut out);
    let before = allocations();
    shaper.shape_line_into(&line, &empty, &mut out);
    let per_row = allocations() - before;
    // A full frame costs the shaping of each row plus what an undamaged
    // frame costs (encoder, submission) plus a few per-frame uploads (one
    // write per pass): nothing more per row, so the per-row vectors are
    // reused. One allocation per row would exceed the per-frame slack.
    let slack = u64::from(ROWS) / 2;
    let budget = per_row * u64::from(ROWS) + idle[2] + slack;
    assert!(
        full[5] <= budget,
        "a full frame allocated {} > {budget} ({per_row} per row shaped, {} per frame)",
        full[5],
        idle[2]
    );
}

/// Highlights cost no allocation once they settle: an undamaged frame with
/// a selection and overlays that did not change allocates exactly what an
/// undamaged frame without any allocates, because the coverage comparison
/// reuses its vectors (#22 review, item 8). A frame that changes the
/// selection stays at a fixed count too.
#[test]
fn highlight_frames_reuse_their_vectors() {
    if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
        return;
    }
    let gpu = Gpu::headless().expect("a Metal adapter");
    let shaper = Shaper::new(&config()).expect("Menlo ships with macOS");
    let mut renderer = Renderer::new(&gpu, Theme::default(), shaper).expect("renderer");
    let m = renderer.cell_metrics();
    let size = PhysicalSize::new(m.width_px * COLS + 8, m.height_px * ROWS + 8);
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8UnormSrgb);
    let line: Vec<Cell> = "highlight frames "
        .chars()
        .cycle()
        .take(COLS as usize)
        .map(|ch| Cell::new(ch, Style::DEFAULT))
        .collect();
    let rows: Vec<&[Cell]> = vec![line.as_slice(); ROWS as usize];
    let overlays: Vec<Overlay> = (0..u16::try_from(ROWS).expect("few rows"))
        .map(|r| Overlay::new(OverlayKind::Match, r, 0..10))
        .collect();
    let mut frame = |highlights: Option<(Option<Selection>, &[Overlay])>, damage| {
        let before = allocations();
        let (selection, overlays) = highlights.unwrap_or((None, &[]));
        let frame = FrameInput::new(&rows)
            .with_overlays(overlays)
            .with_selection(selection)
            .with_damage(damage);
        target.render(&gpu, &mut renderer, &frame).expect("render");
        gpu.device()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        allocations() - before
    };
    frame(None, DamageRows::All);
    let plain: Vec<u64> = (0..3).map(|_| frame(None, DamageRows::None)).collect();
    let selection = |end: u16| Some(Selection::linear(Point::new(1, 2), Point::new(end, 5)));
    // The selection grows and shrinks over the same rows each frame.
    let moving: Vec<u64> = (0..6)
        .map(|i| frame(Some((selection(2 + (i % 2)), &overlays)), DamageRows::None))
        .collect();
    let settled: Vec<u64> = (0..4)
        .map(|_| frame(Some((selection(2), &overlays)), DamageRows::None))
        .collect();
    eprintln!(
        "allocations: plain undamaged {plain:?}, changing highlights {moving:?}, \
         unchanged highlights {settled:?}"
    );
    assert!(
        moving[2..].iter().all(|&n| n == moving[2]),
        "changing highlights do not settle: {moving:?}"
    );
    assert!(
        settled[1..].iter().all(|&n| n == plain[2]),
        "unchanged highlights allocate {settled:?}, a frame without any {}",
        plain[2]
    );
}
