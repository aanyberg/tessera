//! The glyph atlas on a headless GPU: layer selection, exact cell-sized
//! box drawing, LRU eviction (alpha and colour layers), fragmentation
//! under churn, size limits, invalidation on a font size change, and the
//! atlas-full policy (reset once per frame, ADR 0010), directly and through
//! the renderer.
//!
//! Tests skip (with a logged reason) when the machine has no Metal adapter
//! or `TESSERA_SKIP_GPU_TESTS` is set; `tests/headless_clear.rs` is the suite
//! that fails outright without a GPU.

#![expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "texel coordinates in these tests are small and non-negative"
)]

use tessera_grid::{Cell, Style};
use tessera_render::{
    AtlasLayer, AtlasSlot, FontConfig, FrameInput, GlyphAtlas, GlyphKey, Gpu, OffscreenTarget,
    PhysicalSize, RenderError, Renderer, ShapedGlyph, Shaper, Theme,
};

const SKIP_VAR: &str = "TESSERA_SKIP_GPU_TESTS";

fn gpu() -> Option<Gpu> {
    if std::env::var_os(SKIP_VAR).is_some() {
        eprintln!("skipping: {SKIP_VAR} is set");
        return None;
    }
    match Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(RenderError::NoAdapter(e)) => {
            eprintln!("skipping: no Metal adapter ({e})");
            None
        }
        Err(e) => panic!("GPU bring-up failed: {e}"),
    }
}

fn menlo(size_pt: f32, scale: f32) -> Shaper {
    Shaper::new(&FontConfig {
        family: "Menlo".into(),
        size_pt,
        scale,
        fallback: Vec::new(),
    })
    .expect("Menlo ships with macOS")
}

fn cells(text: &str) -> Vec<Cell> {
    text.chars()
        .map(|ch| Cell::new(ch, Style::DEFAULT))
        .collect()
}

fn shape_one(shaper: &mut Shaper, ch: char) -> ShapedGlyph {
    let glyphs = shaper.shape_line(&cells(&ch.to_string()));
    assert_eq!(glyphs.len(), 1, "{ch:?}: {glyphs:?}");
    glyphs[0]
}

fn key_of(shaper: &mut Shaper, ch: char) -> GlyphKey {
    let glyph = shape_one(shaper, ch);
    shaper.glyph_key(&glyph)
}

/// Copies the texels of `slot` back to the CPU, tightly packed.
fn read_slot(gpu: &Gpu, atlas: &GlyphAtlas, slot: &AtlasSlot) -> Vec<u8> {
    let size = atlas.size() as f32;
    let x = (slot.uv[0] * size).round() as u32;
    let y = (slot.uv[1] * size).round() as u32;
    let [w, h] = slot.size;
    let bpp = slot.layer.bytes_per_texel();
    let row = w * bpp;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = gpu.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("test.atlas.readback"),
        size: u64::from(padded) * u64::from(h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: atlas.texture(slot.layer),
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue().submit(Some(encoder.finish()));
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    gpu.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().expect("map callback").expect("map");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped range");
    mapped
        .chunks_exact(padded as usize)
        .flat_map(|r| r[..row as usize].iter().copied())
        .collect()
}

#[test]
fn emoji_rasterises_into_the_colour_layer() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 2.0);
    let mut atlas = GlyphAtlas::new(&gpu, shaper.fonts());
    let glyph = shape_one(&mut shaper, '\u{1F600}');
    assert!(glyph.is_color, "{glyph:?}");
    let key = shaper.glyph_key(&glyph);
    assert_eq!(key.subpixel_x, 0, "colour bitmaps are not phase-shifted");
    atlas.begin_frame();
    let slot = atlas.get_or_insert(&gpu, key).expect("insert");
    eprintln!(
        "emoji slot at 13pt x2: {slot:?}, cell {:?}",
        shaper.cell_metrics()
    );
    assert_eq!(slot.layer, AtlasLayer::Color, "{slot:?}");
    assert!(!slot.is_empty(), "{slot:?}");
    let texels = read_slot(&gpu, &atlas, &slot);
    let opaque = texels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] > 200)
        .count();
    assert!(
        opaque > texels.len() / 16,
        "emoji is mostly transparent: {opaque} opaque texels"
    );
    let coloured = texels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] > 0 && (p[0].abs_diff(p[1]) > 40 || p[1].abs_diff(p[2]) > 40))
        .count();
    assert!(coloured > 0, "emoji has no colour");
    // Premultiplied in linear light: no decoded channel exceeds alpha.
    let decode = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    assert!(texels.as_chunks::<4>().0.iter().all(|p| {
        let a = f32::from(p[3]) / 255.0 + 0.01;
        p[..3].iter().all(|&c| decode(c) <= a)
    }));
}

#[test]
fn text_glyphs_rasterise_into_the_alpha_layer() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    let mut atlas = GlyphAtlas::new(&gpu, shaper.fonts());
    let key = key_of(&mut shaper, 'M');
    let slot = atlas.get_or_insert(&gpu, key).expect("insert");
    assert_eq!(slot.layer, AtlasLayer::Alpha);
    let m = shaper.cell_metrics();
    assert!(
        slot.size[0] <= m.width_px + 1 && slot.size[1] <= m.height_px,
        "{slot:?} {m:?}"
    );
    // The top of an M sits on the cap height, above the baseline.
    assert!(slot.offset[1] > 0, "{slot:?}");
    let texels = read_slot(&gpu, &atlas, &slot);
    assert!(
        texels.iter().any(|&a| a > 200),
        "M has near-full-coverage stems"
    );
    // A hit returns the same slot without rasterising again.
    let before = atlas.stats().rasterized;
    assert_eq!(atlas.get_or_insert(&gpu, key).expect("hit"), slot);
    assert_eq!(atlas.stats().rasterized, before);
}

#[test]
fn box_drawing_bitmaps_are_exactly_one_cell() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    let mut atlas = GlyphAtlas::new(&gpu, shaper.fonts());
    for size in [13.0, 20.0] {
        for scale in [1.0, 2.0] {
            shaper.set_font_size(size, scale);
            atlas.begin_frame();
            let m = shaper.cell_metrics();
            let (w, h) = (m.width_px, m.height_px);

            let horiz = shape_one(&mut shaper, '\u{2500}');
            let slot = atlas
                .get_or_insert(&gpu, shaper.glyph_key(&horiz))
                .expect("─");
            assert_eq!(slot.layer, AtlasLayer::Alpha);
            assert_eq!(slot.size[0], w, "─ width at {size}pt x{scale}");
            assert_eq!(slot.size, [w, h], "─ fills the cell at {size}pt x{scale}");
            assert_eq!(
                slot.offset,
                [0, m.baseline_px as i32],
                "─ top aligns with the cell top"
            );
            let texels = read_slot(&gpu, &atlas, &slot);
            let line_rows: Vec<u32> = (0..h)
                .filter(|&y| texels[(y * w) as usize] == 255)
                .collect();
            assert!(!line_rows.is_empty(), "─ has a line at {size}pt x{scale}");
            for &y in &line_rows {
                let row = &texels[(y * w) as usize..((y + 1) * w) as usize];
                assert!(
                    row.iter().all(|&a| a == 255),
                    "─ row {y} has a gap: {row:?}"
                );
            }

            let vert = shape_one(&mut shaper, '\u{2502}');
            let slot = atlas
                .get_or_insert(&gpu, shaper.glyph_key(&vert))
                .expect("│");
            assert_eq!(slot.size[1], h, "│ height at {size}pt x{scale}");
            assert_eq!(slot.size, [w, h]);
            let texels = read_slot(&gpu, &atlas, &slot);
            let line_cols: Vec<u32> = (0..w).filter(|&x| texels[x as usize] == 255).collect();
            assert!(!line_cols.is_empty(), "│ has a line at {size}pt x{scale}");
            for &x in &line_cols {
                assert!(
                    (0..h).all(|y| texels[(y * w + x) as usize] == 255),
                    "│ column {x} has a gap at {size}pt x{scale}"
                );
            }
        }
    }
}

#[test]
fn full_atlas_evicts_the_oldest_generation_and_re_rasterises() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    // 64x64 texels hold a couple of dozen 13px glyphs.
    let mut atlas = GlyphAtlas::with_size(&gpu, shaper.fonts(), 64);
    let text: String = ('!'..='~').collect();
    let glyphs = shaper.shape_line(&cells(&text));
    assert_eq!(glyphs.len(), 94);
    let keys: Vec<_> = glyphs.iter().map(|g| shaper.glyph_key(g)).collect();
    for key in &keys {
        atlas.begin_frame();
        let slot = atlas
            .get_or_insert(&gpu, *key)
            .expect("one glyph per frame always fits");
        assert!(!slot.is_empty());
    }
    let stats = atlas.stats();
    assert!(
        stats.evicted > 0,
        "94 glyphs cannot fit in 64x64: {stats:?}"
    );
    assert_eq!(stats.rasterized, 94);
    assert!(atlas.len() < 94);
    assert!(!atlas.contains(&keys[0]), "the oldest glyph was evicted");
    assert!(atlas.contains(&keys[93]), "the newest glyph survives");

    atlas.begin_frame();
    let again = atlas
        .get_or_insert(&gpu, keys[0])
        .expect("evicted key comes back");
    assert!(!again.is_empty());
    assert_eq!(
        atlas.stats().rasterized,
        95,
        "an evicted glyph is rasterised again"
    );
    let texels = read_slot(&gpu, &atlas, &again);
    assert!(
        texels.iter().any(|&a| a > 128),
        "re-uploaded bitmap has ink"
    );
}

#[test]
fn glyphs_used_this_frame_are_never_evicted() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    let mut atlas = GlyphAtlas::with_size(&gpu, shaper.fonts(), 64);
    let text: String = ('!'..='~').collect();
    let glyphs = shaper.shape_line(&cells(&text));
    atlas.begin_frame();
    let mut inserted = Vec::new();
    let mut failure = None;
    for g in &glyphs {
        let key = shaper.glyph_key(g);
        match atlas.get_or_insert(&gpu, key) {
            Ok(_) => inserted.push(key),
            Err(e) => {
                failure = Some((key, e));
                break;
            }
        }
    }
    let (rejected, error) = failure.expect("a 64x64 atlas cannot hold 94 glyphs in one frame");
    assert!(
        matches!(error, RenderError::AtlasFull(AtlasLayer::Alpha)),
        "{error:?}"
    );
    assert_eq!(atlas.stats().evicted, 0);
    assert!(inserted.iter().all(|k| atlas.contains(k)));
    assert!(!atlas.contains(&rejected));
    // The next frame may evict them, so the rejected glyph fits.
    atlas.begin_frame();
    atlas
        .get_or_insert(&gpu, rejected)
        .expect("fits after eviction");
    assert!(atlas.stats().evicted > 0);
}

#[test]
fn set_font_size_changes_metrics_and_clears_the_atlas() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    let mut atlas = GlyphAtlas::new(&gpu, shaper.fonts());
    atlas.begin_frame();
    for g in shaper.shape_line(&cells("hello")) {
        atlas
            .get_or_insert(&gpu, shaper.glyph_key(&g))
            .expect("insert");
    }
    assert_eq!(atlas.len(), 4, "h, e, l, o");
    let before = shaper.cell_metrics();
    let old_key = key_of(&mut shaper, 'h');

    shaper.set_font_size(20.0, 1.0);
    let after = shaper.cell_metrics();
    assert_ne!(after, before);
    assert!(after.width_px > before.width_px && after.height_px > before.height_px);

    atlas.begin_frame();
    assert!(atlas.is_empty(), "font size change must clear the atlas");
    assert_eq!(atlas.stats().cleared, 1);
    let new_key = key_of(&mut shaper, 'h');
    assert_ne!(new_key, old_key);
    let slot = atlas
        .get_or_insert(&gpu, new_key)
        .expect("insert at new size");
    assert!(slot.size[1] > 0);

    // An explicit clear works too, and a same-size call keeps the atlas.
    shaper.set_font_size(20.0, 1.0);
    atlas.begin_frame();
    assert_eq!(atlas.len(), 1);
    atlas.clear();
    assert!(atlas.is_empty());
    assert_eq!(atlas.stats().cleared, 2);
}

#[test]
fn subpixel_phases_are_distinct_bitmaps() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    let mut atlas = GlyphAtlas::new(&gpu, shaper.fonts());
    let base = shape_one(&mut shaper, 'l');
    let mut bitmaps = Vec::new();
    for third in 0..3 {
        let glyph = ShapedGlyph {
            x_offset: f32::from(third) / 3.0,
            ..base
        };
        let key = shaper.glyph_key(&glyph);
        assert_eq!(key.subpixel_x, third);
        let slot = atlas.get_or_insert(&gpu, key).expect("insert");
        bitmaps.push(read_slot(&gpu, &atlas, &slot));
    }
    assert_eq!(atlas.len(), 3);
    assert_ne!(bitmaps[0], bitmaps[1]);
    assert_ne!(bitmaps[1], bitmaps[2]);
}

#[test]
fn synthetic_bold_is_heavier_than_regular() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = Shaper::new(&FontConfig {
        family: "Monaco".into(),
        ..FontConfig::default()
    })
    .expect("fonts");
    if shaper.fonts().used_default_family() {
        eprintln!("skipping: Monaco is not installed");
        return;
    }
    let mut atlas = GlyphAtlas::new(&gpu, shaper.fonts());
    let regular = shape_one(&mut shaper, 'H');
    let bold_cells = [Cell::new('H', Style::DEFAULT.with(Style::BOLD))];
    let bold = shaper.shape_line(&bold_cells)[0];
    assert!(bold.synthetic_bold);
    let ink = |atlas: &mut GlyphAtlas, g: &ShapedGlyph| {
        let slot = atlas
            .get_or_insert(&gpu, shaper.glyph_key(g))
            .expect("insert");
        read_slot(&gpu, atlas, &slot)
            .iter()
            .map(|&a| u32::from(a))
            .sum::<u32>()
    };
    let (r, b) = (ink(&mut atlas, &regular), ink(&mut atlas, &bold));
    assert!(b > r + r / 10, "synthetic bold ink {b} vs regular {r}");
}

#[test]
fn oversized_glyphs_are_an_error_not_a_panic() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(40.0, 2.0);
    let mut atlas = GlyphAtlas::with_size(&gpu, shaper.fonts(), 16);
    let key = key_of(&mut shaper, 'M');
    let err = atlas
        .get_or_insert(&gpu, key)
        .expect_err("80px glyph in a 16px atlas");
    assert!(matches!(err, RenderError::GlyphTooLarge { .. }), "{err:?}");
    assert!(atlas.is_empty());
}

#[test]
fn churning_generations_never_reports_full_while_space_is_free() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    let mut atlas = GlyphAtlas::with_size(&gpu, shaper.fonts(), 64);
    let text: String = ('!'..='~').collect();
    let glyphs = shaper.shape_line(&cells(&text));
    let keys: Vec<_> = glyphs.iter().map(|g| shaper.glyph_key(g)).collect();
    // Each frame draws a few new glyphs; a handful always fits in 64x64
    // once older generations are evicted, however fragmented the layer.
    for (frame, batch) in keys.chunks(5).cycle().take(80).enumerate() {
        atlas.begin_frame();
        for key in batch {
            if let Err(e) = atlas.get_or_insert(&gpu, *key) {
                panic!("frame {frame}: {e} with {} glyphs cached", atlas.len());
            }
        }
    }
    assert!(atlas.stats().evicted > 0);
}

#[test]
fn colour_layer_evicts_the_oldest_generation() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 2.0);
    let mut atlas = GlyphAtlas::with_size(&gpu, shaper.fonts(), 64);
    let emoji: Vec<_> = ('\u{1F600}'..='\u{1F610}')
        .map(|ch| key_of(&mut shaper, ch))
        .collect();
    for key in &emoji {
        atlas.begin_frame();
        let slot = atlas.get_or_insert(&gpu, *key).expect("one emoji fits");
        assert_eq!(slot.layer, AtlasLayer::Color);
    }
    assert!(atlas.stats().evicted > 0, "{:?}", atlas.stats());
    assert!(!atlas.contains(&emoji[0]), "the oldest emoji was evicted");
    assert!(atlas.contains(&emoji[emoji.len() - 1]));
    atlas.begin_frame();
    let again = atlas.get_or_insert(&gpu, emoji[0]).expect("comes back");
    assert_eq!(again.layer, AtlasLayer::Color);
}

#[test]
fn absurd_sizes_are_an_error_not_an_abort() {
    let Some(gpu) = gpu() else { return };
    // The scale is capped, but 512pt x 8 is still far larger than a layer.
    let mut shaper = menlo(512.0, 1e6);
    let mut atlas = GlyphAtlas::new(&gpu, shaper.fonts());
    atlas.begin_frame();
    for ch in ['\u{2500}', 'M', '\u{1F600}'] {
        let key = key_of(&mut shaper, ch);
        let err = atlas.get_or_insert(&gpu, key).expect_err("too large");
        assert!(
            matches!(err, RenderError::GlyphTooLarge { .. }),
            "{ch:?}: {err:?}"
        );
    }
    // A key edited to an impossible size is rejected before rasterising.
    let mut shaper = menlo(13.0, 1.0);
    let mut key = key_of(&mut shaper, '\u{2502}');
    key.cell_size = [7_826_660, 15_132_813];
    let err = atlas.get_or_insert(&gpu, key).expect_err("huge cell");
    assert!(matches!(err, RenderError::GlyphTooLarge { .. }), "{err:?}");
    let mut key = key_of(&mut shaper, 'M');
    key.size_px_64 = u32::MAX;
    let err = atlas.get_or_insert(&gpu, key).expect_err("huge em");
    assert!(matches!(err, RenderError::GlyphTooLarge { .. }), "{err:?}");
    assert!(atlas.is_empty());
}

#[test]
fn a_font_change_mid_frame_waits_for_the_next_frame() {
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    let mut atlas = GlyphAtlas::new(&gpu, shaper.fonts());
    atlas.begin_frame();
    let first = key_of(&mut shaper, 'a');
    let slot = atlas.get_or_insert(&gpu, first).expect("insert");
    shaper.set_font_size(20.0, 1.0);
    // Still the same frame: slots already handed out stay valid.
    let second = key_of(&mut shaper, 'b');
    atlas.get_or_insert(&gpu, second).expect("insert");
    assert!(atlas.contains(&first));
    assert_eq!(atlas.get_or_insert(&gpu, first).expect("hit"), slot);
    assert_eq!(atlas.stats().cleared, 0);
    atlas.begin_frame();
    assert!(atlas.is_empty());
    assert_eq!(atlas.stats().cleared, 1);
}

#[test]
fn an_oversized_glyph_is_rasterised_once_until_the_next_clear() {
    let Some(gpu) = gpu() else { return };
    // 100 px em in a 64 px layer: passes the key-size check (em at most
    // twice the layer) but the bitmap itself does not fit.
    let mut shaper = menlo(50.0, 2.0);
    let mut atlas = GlyphAtlas::with_size(&gpu, shaper.fonts(), 64);
    let key = key_of(&mut shaper, 'M');
    for frame in 0..3 {
        atlas.begin_frame();
        let err = atlas.get_or_insert(&gpu, key).expect_err("too large");
        assert!(matches!(err, RenderError::GlyphTooLarge { .. }), "{err:?}");
        assert_eq!(atlas.stats().rasterized, 1, "frame {frame}");
    }
    assert!(atlas.is_empty());
    atlas.clear();
    let err = atlas.get_or_insert(&gpu, key).expect_err("still too large");
    assert!(matches!(err, RenderError::GlyphTooLarge { .. }), "{err:?}");
    assert_eq!(atlas.stats().rasterized, 2, "a clear forgets the failure");
}

#[test]
fn a_font_size_change_in_one_window_keeps_other_windows_atlases() {
    let Some(gpu) = gpu() else { return };
    let shared = menlo(13.0, 1.0).fonts().clone();
    let mut a = Shaper::with_fonts(shared.clone(), 13.0, 1.0);
    let mut b = Shaper::with_fonts(shared, 13.0, 1.0);
    let mut atlas_a = GlyphAtlas::new(&gpu, a.fonts());
    let mut atlas_b = GlyphAtlas::new(&gpu, b.fonts());
    atlas_a.begin_frame();
    atlas_b.begin_frame();
    let (ka, kb) = (key_of(&mut a, 'x'), key_of(&mut b, 'x'));
    atlas_a.get_or_insert(&gpu, ka).expect("insert");
    atlas_b.get_or_insert(&gpu, kb).expect("insert");

    a.set_font_size(20.0, 1.0);
    atlas_a.begin_frame();
    atlas_b.begin_frame();
    assert!(atlas_a.is_empty(), "the resized window's atlas clears");
    assert_eq!(atlas_b.len(), 1, "the other window's atlas is untouched");
    assert_eq!(atlas_b.stats().cleared, 0);
}

#[test]
fn an_atlas_built_for_a_shaper_on_shared_fonts_clears_on_its_resize() {
    let Some(gpu) = gpu() else { return };
    // The natural sequence: load fonts once, hand a clone to the shaper,
    // then build the atlas for that shaper (not from the original handle).
    let fonts = menlo(13.0, 1.0).fonts().clone();
    let mut shaper = Shaper::with_fonts(fonts.clone(), 13.0, 1.0);
    let mut atlas = GlyphAtlas::for_shaper(&gpu, &shaper);
    atlas.begin_frame();
    let key = key_of(&mut shaper, 'x');
    atlas.get_or_insert(&gpu, key).expect("insert");

    shaper.set_font_size(20.0, 1.0);
    atlas.begin_frame();
    assert!(atlas.is_empty(), "the shaper's resize must clear its atlas");
    assert_eq!(atlas.stats().cleared, 1);
}

/// Short glyphs in every style and subpixel phase: many distinct keys
/// that each need only a short shelf.
fn short_keys(shaper: &mut Shaper) -> Vec<GlyphKey> {
    let styles = [
        Style::DEFAULT,
        Style::DEFAULT.with(Style::BOLD),
        Style::DEFAULT.with(Style::ITALIC),
        Style::DEFAULT.with(Style::BOLD | Style::ITALIC),
    ];
    let mut keys = Vec::new();
    for style in styles {
        let row: Vec<Cell> = ".,-_'`~:;^="
            .chars()
            .map(|ch| Cell::new(ch, style))
            .collect();
        for glyph in shaper.shape_line(&row) {
            for third in 0..3u8 {
                let shifted = ShapedGlyph {
                    x_offset: glyph.x_offset.floor() + f32::from(third) / 3.0,
                    ..glyph
                };
                let key = shaper.glyph_key(&shifted);
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

#[test]
fn a_fragmented_atlas_resets_once_per_frame() {
    const SIZE: u32 = 64;
    let Some(gpu) = gpu() else { return };
    let mut shaper = menlo(13.0, 1.0);
    let mut atlas = GlyphAtlas::with_size(&gpu, shaper.fonts(), SIZE);
    let keys = short_keys(&mut shaper);

    // Frame 1 fills the layer with short shelves.
    atlas.begin_frame();
    let mut placed: Vec<(GlyphKey, AtlasSlot)> = Vec::new();
    for key in &keys {
        match atlas.get_or_insert(&gpu, *key) {
            Ok(slot) if !slot.is_empty() => placed.push((*key, slot)),
            Ok(_) => {}
            Err(RenderError::AtlasFull(AtlasLayer::Alpha)) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert!(placed.len() > 8, "{} short glyphs placed", placed.len());
    assert!(
        placed.len() < keys.len(),
        "the short glyphs must fill the layer ({} keys)",
        keys.len()
    );

    // Frame 2 keeps one glyph on every shelf, so eviction cannot merge
    // the shelves, and then needs a full-cell glyph.
    atlas.begin_frame();
    let mut kept: Vec<GlyphKey> = Vec::new();
    let mut shelves: Vec<u32> = Vec::new();
    for (key, slot) in &placed {
        let shelf = (slot.uv[1] * SIZE as f32).round() as u32;
        if !shelves.contains(&shelf) {
            shelves.push(shelf);
            atlas.get_or_insert(&gpu, *key).expect("hit");
            kept.push(*key);
        }
    }
    let tall = key_of(&mut shaper, '█');
    let err = atlas
        .get_or_insert(&gpu, tall)
        .expect_err("the kept glyphs pin every shelf");
    assert!(
        matches!(err, RenderError::AtlasFull(AtlasLayer::Alpha)),
        "{err:?}"
    );
    let before = atlas.stats();

    atlas
        .recover_from_full(&gpu, AtlasLayer::Alpha)
        .expect("the kept glyphs alone fit");
    let after = atlas.stats();
    assert_eq!(after.full_resets, before.full_resets + 1);
    assert_eq!(after.cleared, before.cleared + 1);
    assert_eq!(atlas.len(), kept.len(), "only this frame's glyphs are kept");
    assert!(kept.iter().all(|k| atlas.contains(k)));
    let slot = atlas
        .get_or_insert(&gpu, tall)
        .expect("fits after the reset");
    assert!(
        read_slot(&gpu, &atlas, &slot).iter().all(|&a| a == 255),
        "full block"
    );
    for key in &kept {
        let slot = atlas.get_or_insert(&gpu, *key).expect("kept glyph");
        assert!(
            read_slot(&gpu, &atlas, &slot).iter().any(|&a| a > 0),
            "re-inserted bitmap has ink"
        );
    }

    // A second reset in the same frame is refused and changes nothing.
    let len = atlas.len();
    let err = atlas
        .recover_from_full(&gpu, AtlasLayer::Color)
        .expect_err("one reset per frame");
    assert!(
        matches!(err, RenderError::AtlasFull(AtlasLayer::Color)),
        "{err:?}"
    );
    assert_eq!(atlas.stats().full_resets, after.full_resets);
    assert_eq!(atlas.len(), len);
    assert!(atlas.contains(&tall));

    // The next frame may reset again.
    atlas.begin_frame();
    atlas
        .recover_from_full(&gpu, AtlasLayer::Alpha)
        .expect("a new frame may reset");
    assert_eq!(atlas.stats().full_resets, after.full_resets + 1);
}

/// A renderer with a tiny atlas, and one with the default size, at 1x.
fn renderers(gpu: &Gpu, atlas_size: u32) -> (Renderer, Renderer) {
    let theme = Theme {
        padding: 0.0,
        ..Theme::default()
    };
    let small = Renderer::with_atlas_size(gpu, theme.clone(), menlo(13.0, 1.0), atlas_size)
        .expect("renderer");
    let reference = Renderer::new(gpu, theme, menlo(13.0, 1.0)).expect("renderer");
    (small, reference)
}

fn render_row(gpu: &Gpu, renderer: &mut Renderer, row: &[Cell]) -> Result<Vec<u8>, RenderError> {
    let m = renderer.cell_metrics();
    let size = PhysicalSize::new(m.width_px * row.len() as u32, m.height_px);
    let target = OffscreenTarget::new(gpu, size, wgpu::TextureFormat::Rgba8UnormSrgb);
    let rows: [&[Cell]; 1] = [row];
    target.render(gpu, renderer, &FrameInput::new(&rows))?;
    Ok(target.read_back(gpu).expect("readback"))
}

#[test]
fn renderer_resets_a_full_atlas_at_most_once_per_frame() {
    let Some(gpu) = gpu() else { return };
    let (mut small, mut reference) = renderers(&gpu, 64);
    let styles = [
        Style::DEFAULT,
        Style::DEFAULT.with(Style::BOLD),
        Style::DEFAULT.with(Style::ITALIC),
    ];
    let printable: Vec<char> = ('!'..='~').collect();
    // A deterministic walk over characters and styles: every frame draws
    // eighteen glyphs, most of them new, in mixed heights. A 64x64 layer
    // holds about 28 at this size, so eviction alone leaves it fragmented
    // now and then.
    let mut seed = 0x2545_f491_u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as usize
    };
    let mut resets_seen = 0;
    for frame in 0..120 {
        let row: Vec<Cell> = (0..18)
            .map(|_| {
                let ch = printable[next() % printable.len()];
                Cell::new(ch, styles[next() % styles.len()])
            })
            .collect();
        let before = small.atlas_stats().full_resets;
        let pixels =
            render_row(&gpu, &mut small, &row).unwrap_or_else(|e| panic!("frame {frame}: {e}"));
        let resets = small.atlas_stats().full_resets - before;
        assert!(resets <= 1, "frame {frame} reset the atlas {resets} times");
        resets_seen += resets;
        let expected = render_row(&gpu, &mut reference, &row).expect("reference");
        assert!(
            pixels == expected,
            "frame {frame} differs from a renderer whose atlas never fills"
        );
    }
    eprintln!("{resets_seen} resets in 120 frames");
    assert!(resets_seen > 0, "the walk must exercise the reset");
    assert_eq!(reference.atlas_stats().full_resets, 0);
}

#[test]
fn renderer_reports_a_frame_that_cannot_fit() {
    let Some(gpu) = gpu() else { return };
    let (mut small, _) = renderers(&gpu, 64);
    let row: Vec<Cell> = ('!'..='~')
        .map(|ch| Cell::new(ch, Style::DEFAULT))
        .collect();
    let err = render_row(&gpu, &mut small, &row).expect_err("94 glyphs exceed a 64x64 layer");
    assert!(
        matches!(err, RenderError::AtlasFull(AtlasLayer::Alpha)),
        "{err:?}"
    );
    assert_eq!(
        small.atlas_stats().full_resets,
        1,
        "one reset, then give up"
    );
    // The renderer recovers on the next frame that fits.
    render_row(&gpu, &mut small, &cells("hello")).expect("a small frame renders");
    assert!(small.atlas_stats().full_resets <= 2);
}
