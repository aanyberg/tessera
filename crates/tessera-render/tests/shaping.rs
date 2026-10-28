//! Font resolution, cell metrics and shaping against the fonts installed on
//! the machine (no GPU needed).
//!
//! Menlo and Apple Color Emoji ship with macOS, so the tests that use them
//! always run. The ligature tests need `JetBrains Maple Mono`, `Cascadia
//! Code` or the Monaspace families and skip, with a logged reason, the faces
//! that are not installed.

#![expect(
    clippy::cast_precision_loss,
    reason = "cell sizes in these tests are a few dozen pixels"
)]

use tessera_grid::{Cell, Style};
use tessera_render::{CellMetrics, FaceId, FontConfig, Fonts, ShapedGlyph, Shaper};

const LIGATURE_FAMILY: &str = "JetBrains Maple Mono";

fn config(family: &str, size_pt: f32, scale: f32) -> FontConfig {
    FontConfig {
        family: family.into(),
        size_pt,
        scale,
        fallback: Vec::new(),
    }
}

fn menlo(size_pt: f32, scale: f32) -> Shaper {
    Shaper::new(&config("Menlo", size_pt, scale)).expect("Menlo ships with macOS")
}

fn cells(text: &str) -> Vec<Cell> {
    text.chars()
        .map(|ch| Cell::new(ch, Style::DEFAULT))
        .collect()
}

fn styled(text: &str, flags: u16) -> Vec<Cell> {
    text.chars()
        .map(|ch| Cell::new(ch, Style::DEFAULT.with(flags)))
        .collect()
}

fn assert_whole(m: CellMetrics) {
    assert!(m.width_px > 0 && m.height_px > 0, "{m:?}");
    assert!(
        (m.baseline_px - m.baseline_px.round()).abs() < f32::EPSILON,
        "baseline is not whole: {m:?}"
    );
    assert!(
        m.baseline_px > 0.0 && m.baseline_px <= m.height_px as f32,
        "baseline outside the cell: {m:?}"
    );
}

#[test]
fn menlo_cell_metrics_are_whole_pixels_and_stable() {
    let mut shaper = menlo(13.0, 1.0);
    for size in [13.0, 20.0] {
        let at_1x = {
            shaper.set_font_size(size, 1.0);
            shaper.cell_metrics()
        };
        for scale in [1.0, 2.0] {
            shaper.set_font_size(size, scale);
            let m = shaper.cell_metrics();
            assert_whole(m);
            for _ in 0..3 {
                assert_eq!(
                    shaper.cell_metrics(),
                    m,
                    "repeated call at {size}pt x{scale}"
                );
            }
            // A fresh shaper at the same size agrees.
            assert_eq!(menlo(size, scale).cell_metrics(), m, "{size}pt x{scale}");
            // Setting the size again is a no-op.
            shaper.set_font_size(size, scale);
            assert_eq!(shaper.cell_metrics(), m);
            eprintln!("Menlo {size}pt x{scale}: {m:?}");
        }
        shaper.set_font_size(size, 2.0);
        let at_2x = shaper.cell_metrics();
        assert!(
            at_2x.width_px.abs_diff(2 * at_1x.width_px) <= 1
                && at_2x.height_px.abs_diff(2 * at_1x.height_px) <= 1,
            "2x metrics {at_2x:?} should be about twice 1x {at_1x:?}"
        );
    }
    // Menlo is 0.6 em wide and about 1.16 em tall.
    shaper.set_font_size(13.0, 1.0);
    let m = shaper.cell_metrics();
    assert_eq!((m.width_px, m.height_px), (8, 15), "Menlo 13pt: {m:?}");
}

#[test]
fn larger_sizes_give_larger_cells() {
    let mut shaper = menlo(10.0, 1.0);
    let mut last = shaper.cell_metrics();
    for size in [12.0, 14.0, 18.0, 24.0, 36.0] {
        shaper.set_font_size(size, 1.0);
        let m = shaper.cell_metrics();
        assert!(
            m.width_px >= last.width_px && m.height_px > last.height_px,
            "{size}pt"
        );
        last = m;
    }
}

#[test]
fn invalid_sizes_are_clamped_not_propagated() {
    let mut shaper = menlo(f32::NAN, f32::NAN);
    assert!((shaper.size_pt() - tessera_render::DEFAULT_FONT_SIZE_PT).abs() < f32::EPSILON);
    assert!((shaper.scale() - 1.0).abs() < f32::EPSILON);
    shaper.set_font_size(-3.0, 0.0);
    assert!((shaper.size_pt() - tessera_render::MIN_FONT_SIZE_PT).abs() < f32::EPSILON);
    assert_whole(shaper.cell_metrics());
}

#[test]
fn tiny_scales_keep_the_em_at_least_one_pixel() {
    for scale in [1e-6, f32::MIN_POSITIVE, 1e-3, 0.01] {
        let mut shaper = menlo(13.0, scale);
        assert!(
            shaper.size_px() >= 1.0,
            "scale {scale}: {}",
            shaper.size_px()
        );
        shaper.set_font_size(tessera_render::MIN_FONT_SIZE_PT, scale);
        assert!(
            shaper.size_px() >= 1.0,
            "scale {scale}: {}",
            shaper.size_px()
        );
        assert_whole(shaper.cell_metrics());
        let glyphs = shaper.shape_line(&cells("."));
        // swash treats a zero size as "unscaled" (font units), so the
        // atlas key must never carry one.
        for g in &glyphs {
            assert!(shaper.glyph_key(g).size_px_64 >= 64, "scale {scale}: {g:?}");
        }
    }
}

fn spans(glyphs: &[ShapedGlyph]) -> Vec<(usize, usize)> {
    glyphs.iter().map(|g| (g.cell_index, g.cell_span)).collect()
}

#[test]
fn programming_ligatures_span_their_cells() {
    let probe = Fonts::new(&FontConfig::default()).expect("fonts");
    if !probe.has_family(LIGATURE_FAMILY) {
        eprintln!("skipping: {LIGATURE_FAMILY} is not installed, no ligature-capable face to test");
        return;
    }
    let mut shaper = Shaper::new(&config(LIGATURE_FAMILY, 13.0, 2.0)).expect("shaper");
    assert!(!shaper.fonts().used_default_family());

    let arrow = shaper.shape_line(&cells("->"));
    assert_eq!(arrow.len(), 1, "-> must be one glyph: {arrow:?}");
    assert_eq!(
        (arrow[0].cell_index, arrow[0].cell_span),
        (0, 2),
        "{arrow:?}"
    );

    let eq = shaper.shape_line(&cells("=="));
    assert_eq!(spans(&eq), [(0, 2)], "== must be one glyph: {eq:?}");

    // `fi` is not a ligature in a monospace face, but whatever the font
    // does, the glyphs cover both cells exactly once.
    let fi = shaper.shape_line(&cells("fi"));
    assert_eq!(fi.iter().map(|g| g.cell_span).sum::<usize>(), 2, "{fi:?}");

    // Ligatures are positioned on the grid: the arrow's origin is within
    // its two cells.
    let w = shaper.cell_metrics().width_px as f32;
    assert!(
        arrow[0].x_offset >= 0.0 && arrow[0].x_offset < 2.0 * w,
        "{arrow:?}"
    );

    // In context: `a -> b` keeps the ligature on cells 2..4.
    let line = shaper.shape_line(&cells("a -> b"));
    assert_eq!(spans(&line), [(0, 1), (2, 2), (5, 1)], "{line:?}");

    // A style boundary splits the run, so no ligature forms across it.
    let mut mixed = cells("->");
    mixed[1].style = Style::DEFAULT.with(Style::BOLD);
    assert_eq!(spans(&shaper.shape_line(&mixed)), [(0, 1), (1, 1)]);
}

/// Cascadia Code draws a ligature in its first glyph and follows it with
/// empty spacers; the ligature must still report every cell it covers.
#[test]
fn leading_glyph_ligatures_span_their_cells() {
    let probe = Fonts::new(&FontConfig::default()).expect("fonts");
    if !probe.has_family("Cascadia Code") {
        eprintln!("skipping: Cascadia Code is not installed");
        return;
    }
    let mut s = Shaper::new(&config("Cascadia Code", 13.0, 2.0)).expect("shaper");
    assert!(!s.fonts().used_default_family());
    let g = s.shape_line(&cells("<!--"));
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!((g[0].cell_index, g[0].cell_span), (0, 4), "{g:?}");
    let line = s.shape_line(&cells("a <!-- b"));
    assert_eq!(spans(&line), [(0, 1), (2, 4), (7, 1)], "{line:?}");
}

const OPERATORS: &[&str] = &[
    "==", "===", "!==", "|>", "::", "...", "/*", "//", "&&", "||", ">>=", "!=", ">=", "->", "<!--",
    ":::", "<<=", "<$>", ";;", "!!", "||=",
];

/// Ligature faces the coverage sweep runs on when they are installed.
const LIGATURE_FACES: &[&str] = &[
    "Cascadia Code",
    LIGATURE_FAMILY,
    "Monaspace Argon",
    "Monaspace Neon",
    "Monaspace Radon",
    "Monaspace Xenon",
    "Monaspace Krypton",
];

const REGULAR: u16 = 0;
const BOLD: u16 = Style::BOLD;
const ITALIC: u16 = Style::ITALIC;
const BOLD_ITALIC: u16 = Style::BOLD | Style::ITALIC;

/// The installed subset of `families`, logging each one skipped.
fn installed<'a>(families: &[&'a str]) -> Vec<&'a str> {
    let probe = Fonts::new(&FontConfig::default()).expect("fonts");
    families
        .iter()
        .copied()
        .filter(|f| {
            let ok = probe.has_family(f);
            if !ok {
                eprintln!("skipping {f}: not installed");
            }
            ok
        })
        .collect()
}

/// Asserts that `glyphs` partition `line`: glyphs sharing a cell range are
/// consecutive and report the same range, distinct ranges are in order and
/// never overlap, and every non-blank cell is covered exactly once.
#[track_caller]
fn assert_partition(context: &str, line: &[Cell], glyphs: &[ShapedGlyph]) {
    let mut covered = vec![0_u32; line.len()];
    let mut last: Option<(usize, usize)> = None;
    for g in glyphs {
        let range = (g.cell_index, g.cell_span);
        if last == Some(range) {
            continue;
        }
        assert!(g.cell_span >= 1, "{context}: empty span: {glyphs:?}");
        if let Some((start, span)) = last {
            assert!(
                g.cell_index >= start + span,
                "{context}: out of order or overlapping: {:?}",
                spans(glyphs)
            );
        }
        let end = g.cell_index + g.cell_span;
        assert!(end <= line.len(), "{context}: past the end: {glyphs:?}");
        for c in &mut covered[g.cell_index..end] {
            *c += 1;
        }
        last = Some(range);
    }
    for (i, cell) in line.iter().enumerate() {
        if cell.ch != ' ' {
            assert_eq!(
                covered[i],
                1,
                "{context}: cell {i} covered {} times: {:?}",
                covered[i],
                spans(glyphs)
            );
        }
    }
}

/// The texts the sweep shapes for each operator: alone, spaced, glued,
/// and after letters whose italic ink overhangs into the operator.
fn sweep_texts() -> Vec<String> {
    let mut texts = Vec::new();
    for op in OPERATORS {
        texts.push((*op).to_owned());
        texts.push(format!("a {op} b"));
        texts.push(format!("x{op}y"));
        for prefix in ["W", "w", "/", "M", "Now", "Loading"] {
            texts.push(format!("{prefix}{op}"));
        }
    }
    texts
}

/// Whatever technique a ligature face uses (spacers before the drawing
/// glyph, after it, or merged into its cluster), the output is a partition
/// of the line's cells at every size, scale and style.
///
/// Release builds sweep 8–36 pt in 1 pt steps at 1x, 1.5x and 2x; debug
/// builds sweep a subset that keeps the extremes and every size and scale
/// of the regression tests below, so the gate stays fast.
#[test]
fn ligature_spans_partition_the_cells_at_every_size() {
    let combos: Vec<(f32, f32)> = if cfg!(debug_assertions) {
        vec![
            (8.0, 1.0),
            (11.0, 1.0),
            (11.0, 2.0),
            (13.0, 1.0),
            (13.0, 2.0),
            (14.0, 1.0),
            (15.0, 2.0),
            (36.0, 1.5),
        ]
    } else {
        (8_u8..=36)
            .flat_map(|size| [1.0, 1.5, 2.0].map(|scale| (f32::from(size), scale)))
            .collect()
    };
    let styles = [REGULAR, BOLD, ITALIC, BOLD_ITALIC];
    let texts = sweep_texts();
    let lines: Vec<Vec<Vec<Cell>>> = styles
        .iter()
        .map(|&s| texts.iter().map(|t| styled(t, s)).collect())
        .collect();
    let started = std::time::Instant::now();
    let mut count = 0_usize;
    for family in installed(LIGATURE_FACES) {
        let mut shaper = Shaper::new(&config(family, combos[0].0, 1.0)).expect("shaper");
        assert!(!shaper.fonts().used_default_family(), "{family}");
        for &(size, scale) in &combos {
            shaper.set_font_size(size, scale);
            for (style, lines) in styles.iter().zip(&lines) {
                for (text, line) in texts.iter().zip(lines) {
                    let glyphs = shaper.shape_line(line);
                    assert_partition(
                        &format!("{family} {size}pt x{scale} style {style} {text:?}"),
                        line,
                        &glyphs,
                    );
                    count += 1;
                }
            }
        }
        eprintln!("{family}: partition holds");
    }
    eprintln!("{count} lines shaped in {:?}", started.elapsed());
}

/// Shapes `text` in `family` at `size_pt` x `scale` with style `flags`,
/// asserting the partition; `None` (logged) when the face is missing.
fn shape_checked(
    family: &str,
    size_pt: f32,
    scale: f32,
    flags: u16,
    text: &str,
) -> Option<Vec<ShapedGlyph>> {
    if installed(&[family]).is_empty() {
        return None;
    }
    let mut shaper = Shaper::new(&config(family, size_pt, scale)).expect("shaper");
    assert!(!shaper.fonts().used_default_family(), "{family}");
    let line = styled(text, flags);
    let glyphs = shaper.shape_line(&line);
    assert_partition(
        &format!("{family} {size_pt}pt x{scale} style {flags} {text:?}"),
        &line,
        &glyphs,
    );
    Some(glyphs)
}

/// Cascadia Code's compressed `...` and `:::` glyphs reach only about a
/// fifth of a cell into their last spacer; the spacer is still theirs.
#[test]
fn cascadia_dot_ligatures_cover_their_last_cell() {
    let Some(dots) = shape_checked("Cascadia Code", 13.0, 1.0, REGULAR, "...") else {
        return;
    };
    assert_eq!(spans(&dots), [(0, 3)], "{dots:?}");
    for text in [":::", "Loading...", "a...b", "f...", "...=", "W(..."] {
        shape_checked("Cascadia Code", 13.0, 1.0, REGULAR, text);
    }
    let loading = shape_checked("Cascadia Code", 13.0, 1.0, REGULAR, "Loading...").expect("face");
    assert_eq!(spans(&loading).last(), Some(&(7, 3)), "{loading:?}");
    shape_checked("Cascadia Code", 15.0, 2.0, REGULAR, "...");
}

/// Monaspace merges a spacer and its ligature glyph into one cluster; an
/// overhanging italic letter before it must not absorb that cluster.
#[test]
fn monaspace_italic_ligatures_do_not_overlap_their_neighbours() {
    let cases: &[(&str, f32, f32, u16, &str)] = &[
        ("Monaspace Neon", 13.0, 1.0, ITALIC, "W..."),
        ("Monaspace Neon", 13.0, 1.0, ITALIC, "W!="),
        ("Monaspace Argon", 13.0, 1.0, ITALIC, "W//"),
        ("Monaspace Radon", 13.0, 1.0, ITALIC, "/||"),
        ("Monaspace Xenon", 13.0, 1.0, BOLD_ITALIC, "Now..."),
        ("Monaspace Xenon", 13.0, 2.0, BOLD_ITALIC, "w!="),
    ];
    for &(family, size, scale, flags, text) in cases {
        if let Some(glyphs) = shape_checked(family, size, scale, flags, text) {
            // The letters before the operator keep one cell each.
            let letters = text.chars().take_while(char::is_ascii_alphabetic).count();
            for (i, g) in glyphs.iter().take(letters).enumerate() {
                assert_eq!(
                    (g.cell_index, g.cell_span),
                    (i, 1),
                    "{family} {text:?}: {glyphs:?}"
                );
            }
        }
    }
}

/// Spacers in their own cluster before an arrow belong to the arrow even
/// when the arrow's ink barely reaches them, and a drawing glyph of its
/// own cluster keeps its cell.
#[test]
fn exotic_prefixes_keep_the_partition() {
    // The arrow takes its spacer.
    for (size, scale, flags, text) in [
        (11.0, 2.0, ITALIC, "\u{2177}->"),
        (14.0, 1.0, BOLD_ITALIC, "\u{1EBF}=>"),
    ] {
        if let Some(glyphs) = shape_checked(LIGATURE_FAMILY, size, scale, flags, text) {
            assert_eq!(spans(&glyphs).last(), Some(&(1, 2)), "{text:?}: {glyphs:?}");
        }
    }
    // Cascadia's bold `<<=` draws `=` in its own cluster: that glyph keeps
    // its cell, so the split is acceptable as long as it is a partition.
    shape_checked("Cascadia Code", 11.0, 1.0, BOLD, "<<=");
}

#[test]
fn menlo_has_no_ligatures() {
    let mut shaper = menlo(13.0, 2.0);
    for text in ["fi", "->", "=="] {
        let glyphs = shaper.shape_line(&cells(text));
        assert_eq!(spans(&glyphs), [(0, 1), (1, 1)], "{text}: {glyphs:?}");
        let primary = shaper.fonts().primary_face();
        assert!(
            glyphs
                .iter()
                .all(|g| g.face_id == primary && g.glyph_id != 0)
        );
    }
}

#[test]
fn glyphs_are_anchored_to_their_cells() {
    let mut shaper = menlo(13.0, 1.0);
    let line = "The quick brown fox jumps over the lazy dog 0123456789";
    let glyphs = shaper.shape_line(&cells(line));
    let non_blank = line.chars().filter(|c| *c != ' ').count();
    assert_eq!(glyphs.len(), non_blank, "one glyph per non-blank cell");
    let w = shaper.cell_metrics().width_px as f32;
    for g in &glyphs {
        assert_eq!(g.cell_span, 1);
        // No drift: every glyph sits within its own cell, at a third of a
        // pixel.
        assert!(g.x_offset.abs() < w / 2.0, "{g:?}");
        assert!(
            ((g.x_offset * 3.0).round() - g.x_offset * 3.0).abs() < 1e-4,
            "{g:?}"
        );
    }
    // Indices skip the blank cells.
    assert_eq!(glyphs[2].cell_index, 2);
    assert_eq!(glyphs[3].cell_index, 4);
}

#[test]
fn emoji_shapes_to_a_colour_glyph_from_apple_color_emoji() {
    let mut shaper = menlo(13.0, 2.0);
    let glyphs = shaper.shape_line(&cells("a\u{1F600}b"));
    assert_eq!(glyphs.len(), 3, "{glyphs:?}");
    let emoji = glyphs[1];
    assert_eq!(emoji.cell_index, 1);
    assert!(emoji.is_color, "{emoji:?}");
    assert_ne!(emoji.glyph_id, 0);
    assert_eq!(
        shaper.fonts().face_family(emoji.face_id).as_deref(),
        Some("Apple Color Emoji")
    );
    assert!(!glyphs[0].is_color && !glyphs[2].is_color);
}

#[test]
fn colour_glyphs_are_never_synthesised() {
    // Bold and italic must not embolden or skew emoji, and must not cache
    // one bitmap per style combination.
    let mut shaper = menlo(13.0, 2.0);
    let regular = shaper.shape_line(&cells("\u{1F600}"))[0];
    let bold_italic = shaper.shape_line(&styled("\u{1F600}", Style::BOLD | Style::ITALIC))[0];
    assert!(bold_italic.is_color, "{bold_italic:?}");
    assert!(
        !bold_italic.synthetic_bold && !bold_italic.synthetic_italic,
        "{bold_italic:?}"
    );
    assert_eq!(shaper.glyph_key(&bold_italic), shaper.glyph_key(&regular));
}

#[test]
fn cjk_resolves_to_a_fallback_face() {
    let mut shaper = menlo(13.0, 1.0);
    let glyphs = shaper.shape_line(&cells("\u{4E2D}"));
    assert_eq!(glyphs.len(), 1, "{glyphs:?}");
    let g = glyphs[0];
    assert_ne!(g.face_id, shaper.fonts().primary_face(), "Menlo has no Han");
    assert!(!g.face_id.is_builtin());
    assert_ne!(g.glyph_id, 0, "fallback must cover U+4E2D");
    assert!(!g.is_color);
    let family = shaper.fonts().face_family(g.face_id).expect("named face");
    eprintln!("U+4E2D drawn by {family}");
    // A wide glyph is centred in two columns, so it starts near the cell.
    let w = shaper.cell_metrics().width_px as f32;
    assert!(g.x_offset > -w / 2.0 && g.x_offset < w, "{g:?}");
}

#[test]
fn box_drawing_comes_from_the_builtin_face() {
    let mut shaper = menlo(13.0, 1.0);
    let glyphs = shaper.shape_line(&cells("\u{250C}\u{2500}\u{2510}x"));
    assert_eq!(glyphs.len(), 4);
    for (i, g) in glyphs[..3].iter().enumerate() {
        assert_eq!(g.face_id, FaceId::BUILTIN, "{g:?}");
        assert_eq!(g.cell_index, i);
        assert_eq!(g.x_offset, 0.0);
    }
    assert_eq!(glyphs[1].glyph_id, 0x2500);
    assert!(!glyphs[3].face_id.is_builtin());
}

#[test]
fn bold_and_italic_use_real_faces_when_the_family_has_them() {
    let mut shaper = menlo(13.0, 1.0);
    let support = shaper.fonts().style_support();
    assert!(
        support.bold && support.italic && support.bold_italic,
        "{support:?}"
    );
    let regular = shaper.shape_line(&cells("a"))[0];
    let bold = shaper.shape_line(&styled("a", Style::BOLD))[0];
    let italic = shaper.shape_line(&styled("a", Style::ITALIC))[0];
    for g in [bold, italic] {
        assert_ne!(g.face_id, regular.face_id, "{g:?}");
        assert!(!g.synthetic_bold && !g.synthetic_italic, "{g:?}");
    }
}

#[test]
fn missing_styles_are_synthesised() {
    // Monaco ships as a single regular face.
    let probe = Fonts::new(&config("Monaco", 13.0, 1.0)).expect("fonts");
    if probe.used_default_family() {
        eprintln!("skipping: Monaco is not installed");
        return;
    }
    let support = probe.style_support();
    assert!(!support.bold && !support.italic, "{support:?}");
    let mut shaper = Shaper::with_fonts(probe, 13.0, 1.0);
    let bi = shaper.shape_line(&styled("a", Style::BOLD | Style::ITALIC))[0];
    assert!(bi.synthetic_bold && bi.synthetic_italic, "{bi:?}");
    let plain = shaper.shape_line(&cells("a"))[0];
    assert!(!plain.synthetic_bold && !plain.synthetic_italic);
}

#[test]
fn missing_family_falls_back_to_menlo() {
    let fonts = Fonts::new(&config("Definitely Not A Font 9000", 13.0, 1.0)).expect("fonts");
    assert!(fonts.used_default_family());
    assert_eq!(fonts.family(), "Menlo");
    let fallbacks = fonts.fallback_families();
    assert!(
        fallbacks.iter().any(|f| f == "Apple Color Emoji"),
        "{fallbacks:?}"
    );
    assert!(
        !fallbacks.iter().any(|f| f == "Menlo"),
        "primary is not its own fallback"
    );
}

#[test]
fn configured_fallback_is_tried_before_system_fallback() {
    let probe = Fonts::new(&FontConfig::default()).expect("fonts");
    if !probe.has_family(LIGATURE_FAMILY) {
        eprintln!("skipping: {LIGATURE_FAMILY} is not installed");
        return;
    }
    let mut cfg = config("Monaco", 13.0, 1.0);
    cfg.fallback = vec!["no such font".into(), LIGATURE_FAMILY.into()];
    let fonts = Fonts::new(&cfg).expect("fonts");
    assert_eq!(
        fonts.fallback_families().first().map(String::as_str),
        Some(LIGATURE_FAMILY)
    );
    // JetBrains Maple Mono carries CJK; it must win over the system faces.
    let mut shaper = Shaper::with_fonts(fonts, 13.0, 1.0);
    let g = shaper.shape_line(&cells("\u{4E2D}"))[0];
    assert_eq!(
        shaper.fonts().face_family(g.face_id).as_deref(),
        Some(LIGATURE_FAMILY)
    );
}

#[test]
fn blank_and_control_cells_produce_no_glyphs() {
    let mut shaper = menlo(13.0, 1.0);
    assert_eq!(shaper.shape_line(&cells("   ")), []);
    assert_eq!(shaper.shape_line(&cells("\u{0}\u{7}\t")), []);
    assert_eq!(shaper.shape_line(&[]), []);
}

#[test]
fn fonts_and_shaper_can_move_between_threads() {
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<Fonts>();
    is_send_sync::<Shaper>();
    is_send_sync::<tessera_render::GlyphAtlas>();
    let fonts = Fonts::new(&FontConfig::default()).expect("fonts");
    let worker = fonts.clone();
    let family = std::thread::spawn(move || worker.family())
        .join()
        .expect("thread");
    assert_eq!(family, fonts.family());
}

/// Probe ranges for uncovered characters: unassigned plane 5 and
/// supplementary private use area B (plane 16). Plane 3 is not a candidate:
/// it holds CJK Unified Ideographs Extensions G and H, and U+323B0.. is
/// Extension J (Unicode 17).
const PROBE_STARTS: [u32; 3] = [0x5_0000, 0x10_0000, 0x10_0100];
/// How far past each start to look for code points no installed face maps.
const PROBE_SPAN: u32 = 0x100;

/// The code points in the probe ranges that some installed face maps, so an
/// installed font (a PUA-B icon font, say) can never make a probe covered.
fn covered_probes() -> std::collections::HashSet<u32> {
    let system = cosmic_text::FontSystem::new();
    let db = system.db();
    let mut covered = std::collections::HashSet::new();
    for face in db.faces() {
        db.with_face_data(face.id, |data, index| {
            let Some(font) = swash::FontRef::from_index(data, index as usize) else {
                return;
            };
            let charmap = font.charmap();
            for start in PROBE_STARTS {
                covered.extend(
                    (start..start + PROBE_SPAN)
                        .filter(|&c| char::from_u32(c).is_some_and(|ch| charmap.map(ch) != 0)),
                );
            }
        });
    }
    covered
}

/// `count` distinct code points from `start` that no installed face covers.
fn uncovered(covered: &std::collections::HashSet<u32>, start: u32, count: usize) -> Vec<Cell> {
    let line: Vec<Cell> = (start..start + PROBE_SPAN)
        .filter(|c| !covered.contains(c))
        .take(count)
        .map(|c| Cell::new(char::from_u32(c).expect("scalar value"), Style::DEFAULT))
        .collect();
    assert_eq!(
        line.len(),
        count,
        "installed fonts cover the probes from U+{start:X}"
    );
    line
}

/// Generous: the unloaded cost is about 2 ms and loaded runs reached 106 ms.
const WARM_BOUND: std::time::Duration = std::time::Duration::from_millis(500);

#[test]
fn uncovered_characters_are_cheap_after_warm_up() {
    // A coarse guard against the render path waiting on the coverage index
    // or rescanning the font database per character; the structural check
    // is `font::tests::uncovered_characters_are_resolved_once_after_warm_up`.
    // Unloaded this takes about 2 ms, so the bound leaves wide headroom for
    // a loaded machine. Nextest serialises it with the other timing tests;
    // the 500 ms bound is what absorbs load.
    let covered = covered_probes();
    let mut shaper = menlo(13.0, 1.0);
    // The coverage index builds on a background thread, so even the first
    // uncovered character never waits for it on the render path.
    let began = std::time::Instant::now();
    let warm = shaper.shape_line(&uncovered(&covered, PROBE_STARTS[1], 1));
    let first = began.elapsed();
    eprintln!("first uncovered character: {first:?}");
    assert!(
        first < WARM_BOUND,
        "the first uncovered character took {first:?}"
    );
    assert_eq!(warm.len(), 1, "an uncovered character draws .notdef");
    assert_eq!(warm[0].face_id, shaper.fonts().primary_face());
    for start in [PROBE_STARTS[0], PROBE_STARTS[2]] {
        let line = uncovered(&covered, start, 80);
        let began = std::time::Instant::now();
        let glyphs = shaper.shape_line(&line);
        let took = began.elapsed();
        eprintln!("80 uncovered characters from U+{start:X}: {took:?}");
        assert_eq!(glyphs.len(), 80);
        assert!(
            glyphs
                .iter()
                .all(|g| g.face_id == shaper.fonts().primary_face()),
            "uncovered characters fall back to the primary face's .notdef"
        );
        assert!(
            took < WARM_BOUND,
            "shaping 80 uncovered characters took {took:?}"
        );
    }
}

#[test]
fn absurd_scale_is_capped() {
    let shaper = menlo(13.0, 1e6);
    assert!(shaper.scale() <= tessera_render::MAX_SCALE);
    assert!(shaper.size_px() <= tessera_render::MAX_FONT_SIZE_PT * tessera_render::MAX_SCALE);
    let m = shaper.cell_metrics();
    assert!(m.width_px < 1000 && m.height_px < 1000, "{m:?}");
}

/// Default-ignorable characters the shaper hides own no cells, so they
/// never hand a ligature's spacer to a plain letter or a plain letter's
/// cells to a ligature.
#[test]
fn default_ignorable_characters_own_no_cells() {
    let mut m = menlo(13.0, 2.0);
    let line = m.shape_line(&cells("a\u{AD}\u{AD}b"));
    assert_eq!(spans(&line), [(0, 1), (3, 1)], "{line:?}");

    let probe = Fonts::new(&FontConfig::default()).expect("fonts");
    if !probe.has_family(LIGATURE_FAMILY) {
        eprintln!("skipping the ligature cases: {LIGATURE_FAMILY} is not installed");
        return;
    }
    let mut j = Shaper::new(&config(LIGATURE_FAMILY, 13.0, 2.0)).expect("shaper");
    let a = j.shape_line(&cells("a"))[0].glyph_id;
    for ign in ['\u{200B}', '\u{AD}', '\u{FEFF}', '\u{180E}', '\u{17B4}'] {
        for text in [
            format!("a-{ign}>"),
            format!("a{ign}->"),
            format!("->{ign}a"),
        ] {
            let line = j.shape_line(&cells(&text));
            // The cells of `-` and `>`; `a` may span its own cell plus a
            // following U+17B4, which joins its grapheme like a mark.
            let chars: Vec<char> = text.chars().collect();
            let op: Vec<usize> = (0..chars.len())
                .filter(|&i| "->".contains(chars[i]))
                .collect();
            for g in line.iter().filter(|g| g.glyph_id == a) {
                let own = g.cell_index..g.cell_index + g.cell_span;
                assert!(
                    !op.iter().any(|c| own.contains(c)),
                    "{text:?}: a plain `a` took a ligature's cell: {:?}",
                    spans(&line)
                );
            }
        }
    }
    let line = j.shape_line(&cells("a-\u{200B}>"));
    assert_eq!(spans(&line), [(0, 1), (1, 3)], "{line:?}");
}

/// Assigning spans is linear in the line length. Equal work is compared,
/// so both samples are long enough to be preempted alike under load: one
/// 20,000-column line against twenty separate 1,000-column lines. Linear
/// assignment gives a ratio near 1; the old per-cluster owner scan gave
/// about 10.
#[test]
fn shaping_one_long_line_costs_the_same_as_many_short_ones() {
    const SHORT: usize = 1_000;
    const COUNT: usize = 20;
    let mut m = menlo(13.0, 2.0);
    let short_line = cells(&"a".repeat(SHORT));
    let long_line = cells(&"a".repeat(SHORT * COUNT));
    assert_eq!(m.shape_line(&short_line).len(), SHORT);
    assert_eq!(m.shape_line(&long_line).len(), SHORT * COUNT);
    let min_of_three = |run: &mut dyn FnMut()| {
        (0..3)
            .map(|_| {
                let began = std::time::Instant::now();
                run();
                began.elapsed()
            })
            .min()
            .expect("three samples")
    };
    let many = min_of_three(&mut || {
        for _ in 0..COUNT {
            assert_eq!(m.shape_line(&short_line).len(), SHORT);
        }
    });
    let long = min_of_three(&mut || {
        assert_eq!(m.shape_line(&long_line).len(), SHORT * COUNT);
    });
    let ratio = long.as_secs_f64() / many.as_secs_f64();
    eprintln!(
        "{COUNT} x {SHORT} cols {many:?}, 1 x {} cols {long:?}, ratio {ratio:.2}",
        SHORT * COUNT
    );
    assert!(
        ratio < 4.0,
        "one long line took {ratio:.2}x the time of the same cells in short lines"
    );
}

// ------------------------------------------------------------ full clusters

/// Shapes `text` as the grid lays it out, with the line's cluster table.
fn shape_grid(shaper: &mut Shaper, text: &str) -> (tessera_grid::Line, Vec<ShapedGlyph>) {
    let mut grid = tessera_grid::Grid::new(1, 12);
    grid.put_str(text);
    let line = grid.line(0).expect("row 0").clone();
    let glyphs = shaper.shape_grid_line(&line);
    (line, glyphs)
}

#[test]
fn a_combining_mark_is_drawn_with_its_base_in_one_cell() {
    let mut shaper = menlo(13.0, 2.0);
    let w = shaper.cell_metrics().width_px as f32;
    // Menlo composes `e` + U+0301 into its own `é`; U+0338 (long solidus
    // overlay) has no precomposed form after `a`, so it stays a mark.
    for text in ["e\u{301}x", "a\u{338}x"] {
        let (line, glyphs) = shape_grid(&mut shaper, text);
        assert!(line.cells[0].is_cluster(), "the grid made a cluster");
        let whole: Vec<&ShapedGlyph> = glyphs.iter().filter(|g| g.cell_index == 0).collect();
        assert!(
            !whole.is_empty() && whole.iter().all(|g| g.cell_span == 1),
            "{glyphs:?}"
        );
        for g in &whole {
            assert!(
                g.x_offset > -w / 2.0 && g.x_offset < w,
                "inside the cell: {g:?}"
            );
        }
        // Base-only shaping draws the bare base (#20's behaviour); the whole
        // cluster draws something else (a composed glyph or a mark too).
        let base_only = shaper.shape_line(&line.cells);
        let bare: Vec<u16> = base_only
            .iter()
            .filter(|g| g.cell_index == 0)
            .map(|g| g.glyph_id)
            .collect();
        let drawn: Vec<u16> = whole.iter().map(|g| g.glyph_id).collect();
        assert_eq!(bare.len(), 1);
        assert_ne!(drawn, bare, "{text:?}: the mark is drawn");
    }
    let (_, stroke) = shape_grid(&mut shaper, "a\u{338}");
    assert_eq!(stroke.len(), 2, "base and overlay mark: {stroke:?}");
    assert_eq!((stroke[1].cell_index, stroke[1].cell_span), (0, 1));
}

#[test]
fn emoji_sequences_are_one_colour_glyph() {
    let mut shaper = menlo(13.0, 2.0);
    for (name, text) in [
        (
            "ZWJ family",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}",
        ),
        ("skin tone", "\u{1F44D}\u{1F3FD}"),
        ("flag", "\u{1F1F8}\u{1F1EA}"),
        ("VS16 heart", "\u{2764}\u{FE0F}"),
    ] {
        let (line, glyphs) = shape_grid(&mut shaper, text);
        assert!(
            line.cells[0].is_cluster() && line.cells[0].is_wide(),
            "{name}"
        );
        assert_eq!(glyphs.len(), 1, "{name}: one glyph, {glyphs:?}");
        let g = glyphs[0];
        assert_eq!((g.cell_index, g.cell_span), (0, 1), "{name}");
        assert!(g.is_color, "{name}: drawn by the colour emoji face");
        assert_eq!(
            shaper.fonts().face_family(g.face_id).as_deref(),
            Some("Apple Color Emoji"),
            "{name}"
        );
        // Centred over both columns.
        let w = shaper.cell_metrics().width_px as f32;
        assert!(g.x_offset >= -1.0 && g.x_offset < w, "{name}: {g:?}");
    }
    // The base alone is a different glyph: the sequence was shaped whole.
    let (line, whole) = shape_grid(&mut shaper, "\u{1F44D}\u{1F3FD}");
    let base = shaper.shape_line(&line.cells);
    assert_ne!(
        whole[0].glyph_id, base[0].glyph_id,
        "the modifier changed the glyph"
    );
}

#[test]
fn vs15_asks_for_text_presentation() {
    let mut shaper = menlo(13.0, 2.0);
    let (line, glyphs) = shape_grid(&mut shaper, "\u{231A}\u{FE0E}");
    assert!(line.cells[0].is_cluster() && !line.cells[0].is_wide());
    assert_eq!(glyphs.len(), 1, "{glyphs:?}");
    assert!(!glyphs[0].is_color, "text presentation: {glyphs:?}");
}
