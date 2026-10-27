//! The glyph atlas: rasterised glyphs packed into two GPU textures.
//!
//! Alpha-coverage glyphs (ordinary text, synthetic bold and italic, the
//! builtin box-drawing set) go into an `R8Unorm` texture; colour glyphs
//! (emoji) go into an `Rgba8UnormSrgb` texture holding premultiplied,
//! sRGB-encoded RGBA. Each layer is a square packed with shelves. When a
//! layer is full, the glyphs that were used longest ago (by frame
//! generation, see [`GlyphAtlas::begin_frame`]) are evicted until the new
//! glyph fits; glyphs used in the current frame are never evicted.
//!
//! Eviction frees space but shelves can stay fragmented: glyphs of the
//! current frame scattered over many short shelves leave no room for a
//! taller one even at low use. [`GlyphAtlas::recover_from_full`] is the
//! policy for that case (ADR 0010): empty the atlas once per frame and
//! re-insert only the current frame's glyphs, tallest first.

use std::collections::HashMap;

use crate::error::RenderError;
use crate::font::{FaceId, Fonts, RasterImage, RasterKind};
use crate::gpu::Gpu;
use crate::shaping::Shaper;

/// Side length of a layer created by [`GlyphAtlas::new`], in texels.
///
/// Fixed, not derived from the cell size: at about 13 pt on Retina a layer
/// holds roughly 1,500 glyphs, but at very large sizes (around 100 pt on
/// Retina) only a few dozen, so a full screen of distinct glyphs gets
/// [`RenderError::AtlasFull`] for the excess. Large font sizes are out of
/// scope until atlas growth exists (ADR 0007).
pub const DEFAULT_ATLAS_SIZE: u32 = 1024;

/// Empty texels kept around every glyph so linear sampling at a glyph's
/// edge never picks up its neighbour.
const PADDING: u32 = 1;

/// Shelf heights are rounded up to a multiple of this, so glyphs of
/// slightly different heights share shelves.
const SHELF_QUANTUM: u32 = 4;

/// Outline glyphs whose em square is more than this many layer sizes are
/// rejected before rasterising: no real glyph that large fits, and
/// rasterising it could allocate gigabytes.
const MAX_EM_PER_LAYER: f32 = 2.0;

/// `GlyphAtlas::rejected` is emptied when it reaches this many keys, so a
/// stream of distinct oversized keys cannot grow it without bound.
const MAX_REJECTED: usize = 4096;

/// One of the atlas's two textures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AtlasLayer {
    /// `R8Unorm` coverage.
    Alpha,
    /// `Rgba8UnormSrgb`, premultiplied alpha.
    Color,
}

impl AtlasLayer {
    /// Both layers, in index order.
    pub const ALL: [Self; 2] = [Self::Alpha, Self::Color];

    /// The texture format of this layer.
    #[must_use]
    pub const fn format(self) -> wgpu::TextureFormat {
        match self {
            Self::Alpha => wgpu::TextureFormat::R8Unorm,
            Self::Color => wgpu::TextureFormat::Rgba8UnormSrgb,
        }
    }

    /// Bytes per texel.
    #[must_use]
    pub const fn bytes_per_texel(self) -> u32 {
        match self {
            Self::Alpha => 1,
            Self::Color => 4,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Alpha => 0,
            Self::Color => 1,
        }
    }
}

/// Everything that determines a glyph bitmap.
///
/// Build one with [`crate::Shaper::glyph_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct GlyphKey {
    /// The face.
    pub face_id: FaceId,
    /// Glyph index in the face (the code point for [`FaceId::BUILTIN`]).
    pub glyph_id: u16,
    /// Font size in 1/64 device pixel.
    pub size_px_64: u32,
    /// Horizontal subpixel phase in thirds of a pixel, `0..=2`.
    pub subpixel_x: u8,
    /// Embolden the outline.
    pub synthetic_bold: bool,
    /// Skew the outline.
    pub synthetic_italic: bool,
    /// Cell width and height in device pixels; builtin glyphs are drawn
    /// at exactly this size.
    pub cell_size: [u32; 2],
    /// Baseline from the cell top in whole device pixels.
    pub baseline_px: u32,
}

impl GlyphKey {
    /// Font size in device pixels.
    #[must_use]
    pub fn size_px(&self) -> f32 {
        // Font sizes are at most a few thousand pixels, so the 64ths fit
        // f32's exact-integer range.
        #[expect(clippy::cast_precision_loss, reason = "see above")]
        let size = self.size_px_64 as f32;
        size / 64.0
    }
}

/// Where a glyph sits in the atlas and how to place it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasSlot {
    /// The texture holding the bitmap.
    pub layer: AtlasLayer,
    /// Normalised texture coordinates `[u0, v0, u1, v1]` of the bitmap
    /// (top-left, bottom-right), excluding padding.
    pub uv: [f32; 4],
    /// `[left, top]` in device pixels: the bitmap's left edge relative to
    /// the glyph origin and its top edge *above* the baseline. A bitmap's
    /// top-left pixel therefore lands at
    /// `(origin_x + left, baseline_y - top)`.
    pub offset: [i32; 2],
    /// Bitmap width and height in device pixels; `[0, 0]` for glyphs that
    /// draw nothing.
    pub size: [u32; 2],
}

impl AtlasSlot {
    /// `true` when there is nothing to draw.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.size[0] == 0 || self.size[1] == 0
    }

    const EMPTY: Self = Self {
        layer: AtlasLayer::Alpha,
        uv: [0.0; 4],
        offset: [0, 0],
        size: [0, 0],
    };
}

/// Counters for observing the atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AtlasStats {
    /// Glyphs rasterised (cache misses), including re-rasterised evictees.
    pub rasterized: u64,
    /// Glyphs evicted to make room.
    pub evicted: u64,
    /// Times the atlas was emptied by [`GlyphAtlas::clear`], a font
    /// change or [`GlyphAtlas::recover_from_full`].
    pub cleared: u64,
    /// Times [`GlyphAtlas::recover_from_full`] emptied the atlas because a
    /// frame's glyphs did not fit. At most one per frame.
    pub full_resets: u64,
}

/// A texel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// A horizontal strip of the texture with free spans along it.
#[derive(Debug, Clone)]
struct Shelf {
    y: u32,
    height: u32,
    /// Free `(x, width)` spans, sorted by `x`, never adjacent.
    free: Vec<(u32, u32)>,
    used: usize,
}

impl Shelf {
    fn new(y: u32, height: u32, width: u32) -> Self {
        Self {
            y,
            height,
            free: vec![(0, width)],
            used: 0,
        }
    }

    fn alloc(&mut self, w: u32) -> Option<u32> {
        let i = self.free.iter().position(|&(_, fw)| fw >= w)?;
        let (x, fw) = self.free[i];
        if fw == w {
            self.free.remove(i);
        } else {
            self.free[i] = (x + w, fw - w);
        }
        self.used += 1;
        Some(x)
    }

    fn release(&mut self, x: u32, w: u32) {
        let i = self.free.partition_point(|&(fx, _)| fx < x);
        self.free.insert(i, (x, w));
        // Merge with the following span, then with the preceding one.
        if i + 1 < self.free.len() && self.free[i].0 + self.free[i].1 == self.free[i + 1].0 {
            self.free[i].1 += self.free[i + 1].1;
            self.free.remove(i + 1);
        }
        if i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == self.free[i].0 {
            self.free[i - 1].1 += self.free[i].1;
            self.free.remove(i);
        }
        self.used -= 1;
    }
}

/// Shelf packing with per-shelf free lists, so evicted glyphs' space is
/// reused, and empty shelves merge back into open space.
#[derive(Debug, Clone)]
struct ShelfPacker {
    size: u32,
    /// Sorted by `y`, contiguous from 0 to `top`.
    shelves: Vec<Shelf>,
    top: u32,
}

impl ShelfPacker {
    fn new(size: u32) -> Self {
        Self {
            size,
            shelves: Vec::new(),
            top: 0,
        }
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<Rect> {
        if w > self.size || h > self.size {
            return None;
        }
        // Best fit among shelves tall enough and not wastefully tall:
        // occupied shelves accept glyphs whose quantised height is at
        // least 2/3 of theirs (so a shelf opened for a glyph always takes
        // more glyphs of the same height), empty shelves any glyph that
        // fits (and are then split down to it).
        let quantised = h.next_multiple_of(SHELF_QUANTUM);
        let best = self
            .shelves
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.height >= h
                    && (s.used == 0 || s.height * 2 <= quantised * 3)
                    && s.free.iter().any(|&(_, fw)| fw >= w)
            })
            .min_by_key(|(_, s)| s.height)
            .map(|(i, _)| i);
        if let Some(i) = best {
            // An empty shelf (often several merged ones) keeps only the
            // height this glyph needs; the rest becomes a new empty shelf
            // below it, so glyphs of the same height can use it too.
            let chosen = &mut self.shelves[i];
            if chosen.used == 0 && chosen.height > quantised {
                let rest = Shelf::new(chosen.y + quantised, chosen.height - quantised, self.size);
                chosen.height = quantised;
                self.shelves.insert(i + 1, rest);
            }
            let target = &mut self.shelves[i];
            let x = target.alloc(w)?;
            return Some(Rect {
                x,
                y: target.y,
                w,
                h,
            });
        }
        let height = quantised.min(self.size - self.top);
        if height < h {
            return None;
        }
        let mut opened = Shelf::new(self.top, height, self.size);
        let x = opened.alloc(w)?;
        let rect = Rect {
            x,
            y: opened.y,
            w,
            h,
        };
        self.top += height;
        self.shelves.push(opened);
        Some(rect)
    }

    fn release(&mut self, rect: Rect) {
        let Ok(i) = self.shelves.binary_search_by_key(&rect.y, |s| s.y) else {
            debug_assert!(false, "released a rect on no shelf: {rect:?}");
            return;
        };
        self.shelves[i].release(rect.x, rect.w);
        self.coalesce();
    }

    /// Merges runs of adjacent empty shelves into one and gives empty
    /// shelves at the top back to open space.
    fn coalesce(&mut self) {
        let mut i = 0;
        while i + 1 < self.shelves.len() {
            if self.shelves[i].used == 0 && self.shelves[i + 1].used == 0 {
                let merged = self.shelves[i].height + self.shelves[i + 1].height;
                self.shelves[i] = Shelf::new(self.shelves[i].y, merged, self.size);
                self.shelves.remove(i + 1);
            } else {
                i += 1;
            }
        }
        while self.shelves.last().is_some_and(|s| s.used == 0) {
            if let Some(s) = self.shelves.pop() {
                self.top = s.y;
            }
        }
    }

    fn reset(&mut self) {
        self.shelves.clear();
        self.top = 0;
    }
}

#[derive(Debug)]
struct Layer {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    packer: ShelfPacker,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    slot: AtlasSlot,
    rect: Option<Rect>,
    last_used: u64,
}

/// Rasterised glyphs on the GPU, keyed by [`GlyphKey`].
///
/// Call [`Self::begin_frame`] once per frame, then [`Self::get_or_insert`]
/// for every glyph the frame draws. A font invalidation (for example
/// [`crate::Shaper::set_font_size`]) takes effect at the next
/// [`Self::begin_frame`], never between two inserts of one frame, so slots
/// handed out during a frame stay valid until the frame ends.
#[derive(Debug)]
pub struct GlyphAtlas {
    fonts: Fonts,
    epoch: u64,
    size: u32,
    layers: [Layer; 2],
    entries: HashMap<GlyphKey, Entry>,
    /// Keys whose bitmap turned out larger than a layer, with that bitmap
    /// size, so they are not rasterised again every frame. Emptied by
    /// [`Self::clear`].
    rejected: HashMap<GlyphKey, [u32; 2]>,
    generation: u64,
    /// Set by [`Self::recover_from_full`], cleared by
    /// [`Self::begin_frame`]: one reset per frame.
    reset_this_frame: bool,
    stats: AtlasStats,
}

impl GlyphAtlas {
    /// An atlas of [`DEFAULT_ATLAS_SIZE`] (or the device maximum, if
    /// smaller) texels per side for the glyphs `shaper` produces.
    ///
    /// Prefer this to [`Self::new`]: the atlas then follows
    /// [`crate::Shaper::set_font_size`] on `shaper` and clears itself.
    #[must_use]
    pub fn for_shaper(gpu: &Gpu, shaper: &Shaper) -> Self {
        Self::new(gpu, shaper.fonts())
    }

    /// An atlas of [`DEFAULT_ATLAS_SIZE`] (or the device maximum, if
    /// smaller) texels per side for glyphs of `fonts`.
    ///
    /// `fonts` must be the handle of the shaper that feeds this atlas,
    /// [`crate::Shaper::fonts`], not the [`Fonts`] the shaper was built
    /// from: [`crate::Shaper::with_fonts`] gives the shaper its own
    /// invalidation counter, so an atlas built from the original handle is
    /// never cleared by that shaper's [`crate::Shaper::set_font_size`]
    /// (old-size glyphs then linger until evicted).
    /// [`Self::for_shaper`] cannot get this wrong.
    #[must_use]
    pub fn new(gpu: &Gpu, fonts: &Fonts) -> Self {
        Self::with_size(gpu, fonts, DEFAULT_ATLAS_SIZE)
    }

    /// An atlas whose layers are `size × size` texels, clamped to
    /// `1..=Gpu::max_texture_dimension`. `fonts` must be
    /// [`crate::Shaper::fonts`], as for [`Self::new`].
    #[must_use]
    pub fn with_size(gpu: &Gpu, fonts: &Fonts, size: u32) -> Self {
        let size = size.clamp(1, gpu.max_texture_dimension());
        let layer = |layer: AtlasLayer| {
            let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
                label: Some(match layer {
                    AtlasLayer::Alpha => "tessera.atlas.alpha",
                    AtlasLayer::Color => "tessera.atlas.color",
                }),
                size: wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: layer.format(),
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            Layer {
                texture,
                view,
                packer: ShelfPacker::new(size),
            }
        };
        Self {
            fonts: fonts.clone(),
            epoch: fonts.epoch(),
            size,
            layers: [layer(AtlasLayer::Alpha), layer(AtlasLayer::Color)],
            entries: HashMap::new(),
            rejected: HashMap::new(),
            generation: 0,
            reset_this_frame: false,
            stats: AtlasStats::default(),
        }
    }

    /// Side length of each layer in texels.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.size
    }

    /// The texture of `layer`.
    #[must_use]
    pub fn texture(&self, layer: AtlasLayer) -> &wgpu::Texture {
        &self.layers[layer.index()].texture
    }

    /// A view of `layer` for binding.
    #[must_use]
    pub fn view(&self, layer: AtlasLayer) -> &wgpu::TextureView {
        &self.layers[layer.index()].view
    }

    /// The current frame generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Number of cached glyphs (including ones that draw nothing).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `key` is cached. Does not count as a use.
    #[must_use]
    pub fn contains(&self, key: &GlyphKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Counters since creation.
    #[must_use]
    pub fn stats(&self) -> AtlasStats {
        self.stats
    }

    /// Starts a new frame: glyphs used from now on are newer than every
    /// glyph used before. Also drops everything if the fonts were
    /// invalidated (for example by [`crate::Shaper::set_font_size`]).
    pub fn begin_frame(&mut self) {
        self.sync_fonts();
        self.generation += 1;
        self.reset_this_frame = false;
    }

    /// Recovers from [`RenderError::AtlasFull`] on `layer` in the current
    /// frame: empties the atlas, starts a new generation and re-inserts
    /// the glyphs used so far this frame (in both layers), tallest first,
    /// so they pack densely and the rest of the frame has room.
    ///
    /// Every [`AtlasSlot`] handed out before the call is invalid after it:
    /// the caller must look its glyphs up again (they are hits). Counted
    /// in [`AtlasStats::full_resets`] and [`AtlasStats::cleared`].
    ///
    /// # Errors
    ///
    /// [`RenderError::AtlasFull`] for `layer`, without touching the atlas,
    /// when this frame was already reset: the frame needs more than one
    /// layer holds and retrying cannot help. The same error when the
    /// re-inserted glyphs alone do not fit, and [`RenderError::Gpu`] when
    /// an upload fails.
    pub fn recover_from_full(&mut self, gpu: &Gpu, layer: AtlasLayer) -> Result<(), RenderError> {
        if self.reset_this_frame {
            return Err(RenderError::AtlasFull(layer));
        }
        let current = self.generation;
        let mut keep: Vec<(GlyphKey, [u32; 2], usize, Rect)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.last_used == current)
            .filter_map(|(k, e)| Some((*k, e.slot.size, e.slot.layer.index(), e.rect?)))
            .collect();
        // Tallest first, then widest: shelf packing wastes least that way.
        // Ties break on the old position (unique per layer), so the order
        // never depends on the map's iteration order.
        keep.sort_unstable_by_key(|&(_, [w, h], layer, rect)| {
            (std::cmp::Reverse((h, w)), layer, rect.y, rect.x)
        });
        self.clear();
        self.generation += 1;
        self.reset_this_frame = true;
        self.stats.full_resets += 1;
        for (key, ..) in keep {
            self.get_or_insert(gpu, key)?;
        }
        Ok(())
    }

    /// Drops every cached glyph. Texture contents are left in place and
    /// overwritten as glyphs are inserted again.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.rejected.clear();
        for layer in &mut self.layers {
            layer.packer.reset();
        }
        self.stats.cleared += 1;
    }

    /// The slot for `key`, rasterising and uploading the glyph on a miss.
    ///
    /// A glyph that draws nothing (or that its face cannot render) gets an
    /// empty slot. When the target layer is full, glyphs from the oldest
    /// generations are evicted until the glyph fits.
    ///
    /// # Errors
    ///
    /// [`RenderError::GlyphTooLarge`] when the bitmap is larger than a
    /// layer, or when the key alone shows it cannot fit (a builtin glyph's
    /// cell, or an outline's em square more than twice the layer size);
    /// such keys are rejected before rasterising, so an absurd size costs
    /// nothing. A key whose bitmap only turns out too large after
    /// rasterising is remembered and rejected without rasterising until
    /// the next [`Self::clear`]. [`RenderError::AtlasFull`] when it does
    /// not fit even after evicting every glyph not used in the current
    /// generation, and [`RenderError::Gpu`] when the upload fails
    /// validation.
    pub fn get_or_insert(&mut self, gpu: &Gpu, key: GlyphKey) -> Result<AtlasSlot, RenderError> {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.generation;
            return Ok(entry.slot);
        }
        if let Some(&[width, height]) = self.rejected.get(&key) {
            return Err(RenderError::GlyphTooLarge { width, height });
        }
        self.check_key_size(&key)?;
        self.stats.rasterized += 1;
        let image = self
            .fonts
            .rasterize(&key)
            .filter(|i| i.width > 0 && i.height > 0);
        let Some(image) = image else {
            self.entries.insert(
                key,
                Entry {
                    slot: AtlasSlot::EMPTY,
                    rect: None,
                    last_used: self.generation,
                },
            );
            return Ok(AtlasSlot::EMPTY);
        };
        let layer = match image.kind {
            RasterKind::Alpha => AtlasLayer::Alpha,
            RasterKind::Color => AtlasLayer::Color,
        };
        let (w, h) = (image.width + 2 * PADDING, image.height + 2 * PADDING);
        if w > self.size || h > self.size {
            if self.rejected.len() >= MAX_REJECTED {
                self.rejected.clear();
            }
            self.rejected.insert(key, [image.width, image.height]);
            return Err(RenderError::GlyphTooLarge {
                width: image.width,
                height: image.height,
            });
        }
        let rect = match self.layers[layer.index()].packer.alloc(w, h) {
            Some(rect) => rect,
            None => self.evict_until_fits(layer, w, h)?,
        };
        if let Err(e) = self.upload(gpu, layer, rect, &image) {
            self.layers[layer.index()].packer.release(rect);
            return Err(e);
        }
        let slot = self.slot(layer, rect, &image);
        self.entries.insert(
            key,
            Entry {
                slot,
                rect: Some(rect),
                last_used: self.generation,
            },
        );
        Ok(slot)
    }

    fn sync_fonts(&mut self) {
        let epoch = self.fonts.epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.clear();
        }
    }

    /// Rejects keys whose bitmap cannot fit in a layer, before anything is
    /// rasterised or allocated for them.
    fn check_key_size(&self, key: &GlyphKey) -> Result<(), RenderError> {
        let limit = self.size.saturating_sub(2 * PADDING);
        if key.face_id.is_builtin() {
            let [width, height] = key.cell_size;
            if width > limit || height > limit {
                return Err(RenderError::GlyphTooLarge { width, height });
            }
            return Ok(());
        }
        let em = key.size_px();
        #[expect(
            clippy::cast_precision_loss,
            reason = "texture sizes are far below 2^24"
        )]
        let max_em = MAX_EM_PER_LAYER * limit as f32;
        if em > max_em {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "em is positive; `as` saturates at u32::MAX"
            )]
            let side = em.ceil() as u32;
            return Err(RenderError::GlyphTooLarge {
                width: side,
                height: side,
            });
        }
        Ok(())
    }

    /// Evicts whole generations, oldest first, from `layer` until a
    /// `w × h` rectangle fits, and returns it. Glyphs used in the current
    /// generation are never evicted. Empty entries of an evicted
    /// generation (or older) are dropped too.
    ///
    /// Sorts the candidates once, so an insert costs `O(n log n)` however
    /// many generations it has to evict.
    fn evict_until_fits(&mut self, layer: AtlasLayer, w: u32, h: u32) -> Result<Rect, RenderError> {
        let current = self.generation;
        let mut victims: Vec<(u64, GlyphKey)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.rect.is_some() && e.slot.layer == layer && e.last_used < current)
            .map(|(k, e)| (e.last_used, *k))
            .collect();
        victims.sort_unstable_by_key(|&(generation, _)| generation);
        let packer = &mut self.layers[layer.index()].packer;
        let mut evicted = 0;
        let mut evicted_through = None;
        let mut found = None;
        for batch in victims.chunk_by(|a, b| a.0 == b.0) {
            for (_, key) in batch {
                if let Some(Entry {
                    rect: Some(rect), ..
                }) = self.entries.remove(key)
                {
                    packer.release(rect);
                    evicted += 1;
                }
            }
            evicted_through = Some(batch[0].0);
            if let Some(rect) = packer.alloc(w, h) {
                found = Some(rect);
                break;
            }
        }
        self.stats.evicted += evicted;
        if let Some(through) = evicted_through {
            self.entries
                .retain(|_, e| e.rect.is_some() || e.last_used > through);
        }
        found.ok_or(RenderError::AtlasFull(layer))
    }

    fn upload(
        &self,
        gpu: &Gpu,
        layer: AtlasLayer,
        rect: Rect,
        image: &RasterImage,
    ) -> Result<(), RenderError> {
        let bpp = layer.bytes_per_texel() as usize;
        let (w, h) = (rect.w as usize, rect.h as usize);
        let (iw, ih) = (image.width as usize, image.height as usize);
        let pad = PADDING as usize;
        let mut padded = vec![0u8; w * h * bpp];
        for (y, src) in image.data.chunks_exact(iw * bpp).take(ih).enumerate() {
            let start = ((y + pad) * w + pad) * bpp;
            padded[start..start + iw * bpp].copy_from_slice(src);
        }
        gpu.capture_errors(|| {
            gpu.queue().write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.layers[layer.index()].texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: rect.x,
                        y: rect.y,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &padded,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(rect.w * layer.bytes_per_texel()),
                    rows_per_image: Some(rect.h),
                },
                wgpu::Extent3d {
                    width: rect.w,
                    height: rect.h,
                    depth_or_array_layers: 1,
                },
            );
        })
    }

    fn slot(&self, layer: AtlasLayer, rect: Rect, image: &RasterImage) -> AtlasSlot {
        #[expect(
            clippy::cast_precision_loss,
            reason = "texture sizes are far below 2^24"
        )]
        let norm = |v: u32| v as f32 / self.size as f32;
        let (x0, y0) = (rect.x + PADDING, rect.y + PADDING);
        AtlasSlot {
            layer,
            uv: [
                norm(x0),
                norm(y0),
                norm(x0 + image.width),
                norm(y0 + image.height),
            ],
            offset: [image.left, image.top],
            size: [image.width, image.height],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shelf_free_list_merges_neighbours() {
        let mut s = Shelf::new(0, 8, 30);
        let a = s.alloc(10).expect("a");
        let b = s.alloc(10).expect("b");
        let c = s.alloc(10).expect("c");
        assert_eq!((a, b, c), (0, 10, 20));
        assert!(s.alloc(1).is_none());
        s.release(b, 10);
        s.release(a, 10);
        assert_eq!(s.free, vec![(0, 20)]);
        s.release(c, 10);
        assert_eq!(s.free, vec![(0, 30)]);
        assert_eq!(s.used, 0);
    }

    #[test]
    fn packer_reuses_released_space() {
        let mut p = ShelfPacker::new(16);
        let rects: Vec<_> = (0..4).map(|_| p.alloc(8, 8).expect("fits")).collect();
        assert!(p.alloc(8, 8).is_none(), "16x16 holds four 8x8");
        p.release(rects[1]);
        let again = p.alloc(8, 8).expect("reuses the hole");
        assert_eq!(again, rects[1]);
    }

    #[test]
    fn packer_merges_empty_shelves_for_taller_glyphs() {
        let mut packer = ShelfPacker::new(16);
        let a = packer.alloc(16, 4).expect("a");
        let b = packer.alloc(16, 4).expect("b");
        let c = packer.alloc(16, 8).expect("c");
        assert!(packer.alloc(1, 8).is_none());
        packer.release(a);
        packer.release(b);
        // Two empty 4-high shelves become one 8-high shelf.
        let d = packer
            .alloc(16, 8)
            .expect("merged shelves fit an 8-high glyph");
        assert_eq!(d.y, 0);
        packer.release(c);
        packer.release(d);
        assert!(packer.shelves.is_empty());
        assert_eq!(packer.top, 0);
    }

    #[test]
    fn glyphs_of_one_height_share_a_shelf() {
        // A 5-high glyph opens an 8-high (quantised) shelf; the next
        // 5-high glyphs must join it rather than each open their own.
        let mut p = ShelfPacker::new(64);
        let rects: Vec<_> = (0..6).map(|_| p.alloc(10, 5).expect("fits")).collect();
        assert!(rects.iter().all(|r| r.y == 0), "{rects:?}");
        assert_eq!(p.shelves.len(), 1);
        assert_eq!(p.top, 8);
    }

    #[test]
    fn packer_rejects_oversized_requests() {
        let mut p = ShelfPacker::new(16);
        assert!(p.alloc(17, 1).is_none());
        assert!(p.alloc(1, 17).is_none());
        assert!(p.alloc(16, 16).is_some());
    }

    #[test]
    fn packer_does_not_waste_tall_shelves_on_short_glyphs() {
        let mut p = ShelfPacker::new(64);
        let tall = p.alloc(8, 32).expect("tall");
        let short = p.alloc(8, 8).expect("short");
        assert_ne!(short.y, tall.y, "an 8-high glyph opens its own shelf");
    }

    #[test]
    fn packer_splits_merged_empty_shelves_on_reuse() {
        let mut p = ShelfPacker::new(64);
        let rects: Vec<_> = (0..16).map(|_| p.alloc(16, 16).expect("fits")).collect();
        assert!(p.alloc(16, 16).is_none(), "64x64 holds sixteen 16x16");
        // Empty the two middle shelves; they merge into one 32-high shelf.
        for r in rects.iter().filter(|r| r.y == 16 || r.y == 32) {
            p.release(*r);
        }
        let again: Vec<_> = (0..8).filter_map(|_| p.alloc(16, 16)).collect();
        assert_eq!(again.len(), 8, "all eight freed slots are reusable");
        assert!(p.alloc(16, 16).is_none());
        // Shelves stay sorted and contiguous.
        let mut y = 0;
        for s in &p.shelves {
            assert_eq!(s.y, y);
            y += s.height;
        }
        assert_eq!(y, p.top);
    }

    #[test]
    fn layer_formats() {
        assert_eq!(AtlasLayer::Alpha.format(), wgpu::TextureFormat::R8Unorm);
        assert_eq!(
            AtlasLayer::Color.format(),
            wgpu::TextureFormat::Rgba8UnormSrgb
        );
        assert_eq!(AtlasLayer::Color.bytes_per_texel(), 4);
    }
}
