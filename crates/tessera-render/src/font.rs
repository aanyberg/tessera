//! Font discovery, the fallback chain and glyph rasterisation.
//!
//! [`Fonts`] wraps a [`cosmic_text::FontSystem`] (system fonts loaded through
//! `fontdb`, no fontconfig) and owns every decision about *which face* draws a
//! character: the configured primary family (or [`DEFAULT_FAMILY`] when it
//! is not installed), the configured fallback families, Apple Color Emoji
//! and a fixed list of system fallbacks for CJK and symbols. It also
//! rasterises glyphs with `swash` for the atlas, so the shaper and the atlas
//! share one font database through a cheap [`Clone`] handle.
//!
//! See ADR 0007 for the policy this module implements.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};

use cosmic_text::harfrust::{BufferClusterLevel, Direction, UnicodeBuffer};
use cosmic_text::{Font, FontSystem, fontdb};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Scaler, Source, StrikeWith};
use swash::zeno::{Angle, Format, Transform, Vector};

use crate::atlas::GlyphKey;
use crate::boxdraw;
use crate::error::RenderError;

/// Family used when the configured one is not installed.
///
/// Menlo ships with every macOS release Tessera supports.
pub const DEFAULT_FAMILY: &str = "Menlo";

/// The colour emoji family appended to every fallback chain.
pub const EMOJI_FAMILY: &str = "Apple Color Emoji";

/// System families tried after the configured fallbacks, in order.
///
/// Symbols first (they cover arrows, maths and dingbats in a monospace-ish
/// design), then CJK faces (Japanese, Simplified Chinese, Korean), then the
/// broad-coverage Arial Unicode MS. Families that are not installed are
/// skipped. Anything none of them covers is looked up in a coverage index
/// of every installed face, built on a background thread when the
/// [`Fonts`] is created. Until that index is ready such characters draw the
/// primary face's `.notdef` (and are resolved again once it is; see
/// [`Fonts::fallback_index_ready`]).
pub const SYSTEM_FALLBACK: &[&str] = &[
    "Menlo",
    "Apple Symbols",
    "Hiragino Sans",
    "PingFang SC",
    "Hiragino Sans GB",
    "Apple SD Gothic Neo",
    "Arial Unicode MS",
];

/// Smallest font size accepted, in points. Matches `tessera-config`.
pub const MIN_FONT_SIZE_PT: f32 = 4.0;
/// Largest font size accepted, in points. Matches `tessera-config`.
pub const MAX_FONT_SIZE_PT: f32 = 512.0;
/// Largest backing scale factor a [`crate::Shaper`] accepts; larger ones
/// are capped. macOS ships 1x, 2x and 3x displays.
pub const MAX_SCALE: f32 = 8.0;
/// Size used when the configured one is not a finite number.
pub const DEFAULT_FONT_SIZE_PT: f32 = 13.0;

/// Faces at or above this `wght` count as bold.
const BOLD_THRESHOLD: u16 = 600;
/// Skew applied to synthesise italics, in degrees (what Core Text and
/// `cosmic-text` use).
const SYNTHETIC_ITALIC_SKEW_DEG: f32 = 14.0;
/// Families never used by the whole-database scan.
const FORBIDDEN_FAMILIES: &[&str] = &["LastResort", ".LastResort"];
/// `Inner::char_faces` is emptied when it reaches this many entries, so
/// a stream of distinct characters cannot grow it without bound.
const MAX_CHAR_FACES: usize = 1 << 16;
/// `Inner::ink` is emptied when it reaches this many entries; untrusted
/// output reaching many distinct glyphs must not grow it without bound.
const MAX_GLYPH_INK: usize = 1 << 16;
/// One past the largest Unicode scalar value.
const CODE_SPACE: usize = 0x11_0000;

/// Font selection for one terminal surface.
///
/// `size_pt` is in typographic points, which on macOS are logical pixels;
/// the device pixel size is `size_pt * scale`.
#[derive(Debug, Clone, PartialEq)]
pub struct FontConfig {
    /// Primary family name, matched case-insensitively.
    pub family: String,
    /// Size in points.
    pub size_pt: f32,
    /// Backing scale factor (2.0 on Retina).
    pub scale: f32,
    /// Families tried, in order, for characters the primary family lacks,
    /// before the system fallbacks.
    pub fallback: Vec<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: DEFAULT_FAMILY.to_owned(),
            size_pt: DEFAULT_FONT_SIZE_PT,
            scale: 1.0,
            fallback: Vec::new(),
        }
    }
}

/// `size_pt` clamped to [`MIN_FONT_SIZE_PT`]..=[`MAX_FONT_SIZE_PT`]; a
/// non-finite size becomes [`DEFAULT_FONT_SIZE_PT`].
#[must_use]
pub fn sanitize_font_size(size_pt: f32) -> f32 {
    if size_pt.is_finite() {
        size_pt.clamp(MIN_FONT_SIZE_PT, MAX_FONT_SIZE_PT)
    } else {
        DEFAULT_FONT_SIZE_PT
    }
}

/// Identifies the face a glyph comes from.
///
/// Opaque and only meaningful for the [`Fonts`] that produced it. Two
/// weights of one variable font are different faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FaceId(FaceRepr);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FaceRepr {
    /// Box drawing and block elements drawn by [`crate::boxdraw`]; the
    /// glyph id is the code point.
    Builtin,
    /// A face in the font database, instantiated at `weight`.
    Font { id: fontdb::ID, weight: u16 },
}

impl FaceId {
    /// The face for procedurally drawn box-drawing and block characters.
    pub const BUILTIN: Self = Self(FaceRepr::Builtin);

    /// `true` for [`Self::BUILTIN`].
    #[must_use]
    pub const fn is_builtin(self) -> bool {
        matches!(self.0, FaceRepr::Builtin)
    }

    fn font(id: fontdb::ID, weight: fontdb::Weight) -> Self {
        Self(FaceRepr::Font {
            id,
            weight: weight.0,
        })
    }
}

/// Which styled faces the primary family ships.
///
/// A `false` entry means glyphs in that style are synthesised: emboldened
/// outlines for bold, a 14° skew for italic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleSupport {
    /// A face with weight 600 or more (or a `wght` axis reaching it).
    pub bold: bool,
    /// An italic or oblique face.
    pub italic: bool,
    /// A face that is both.
    pub bold_italic: bool,
}

/// The four faces a terminal cell can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FontStyle {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl FontStyle {
    pub(crate) const fn new(bold: bool, italic: bool) -> Self {
        match (bold, italic) {
            (false, false) => Self::Regular,
            (true, false) => Self::Bold,
            (false, true) => Self::Italic,
            (true, true) => Self::BoldItalic,
        }
    }

    const fn is_bold(self) -> bool {
        matches!(self, Self::Bold | Self::BoldItalic)
    }

    const fn is_italic(self) -> bool {
        matches!(self, Self::Italic | Self::BoldItalic)
    }

    const fn weight(self) -> fontdb::Weight {
        if self.is_bold() {
            fontdb::Weight::BOLD
        } else {
            fontdb::Weight::NORMAL
        }
    }
}

/// The face chosen for a character in a style, with the synthesis it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ResolvedFace {
    pub(crate) face: FaceId,
    pub(crate) synthetic_bold: bool,
    pub(crate) synthetic_italic: bool,
}

/// One glyph out of the shaper, in device pixels, before cell placement.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawGlyph {
    pub(crate) glyph_id: u16,
    /// Byte offset of the glyph's cluster in the shaped text.
    pub(crate) cluster: usize,
    pub(crate) x_advance: f32,
    pub(crate) x_offset: f32,
    pub(crate) y_offset: f32,
}

/// Vertical and horizontal metrics of a face at a pixel size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FaceMetrics {
    pub(crate) ascent: f32,
    pub(crate) descent: f32,
    pub(crate) leading: f32,
    /// Advance of `0`, else `M`, else the em width.
    pub(crate) cell_advance: f32,
}

/// Whether a glyph draws anything, and how far its ink reaches left and
/// right of its origin, in font units.
#[derive(Debug, Clone, Copy)]
struct GlyphInk {
    empty: bool,
    min_x_units: f32,
    max_x_units: f32,
}

/// What a rasterised glyph holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RasterKind {
    /// One coverage byte per pixel.
    Alpha,
    /// Four bytes per pixel, sRGB-encoded RGBA with premultiplied alpha.
    Color,
}

/// A rasterised glyph, ready for upload.
#[derive(Debug, Clone)]
pub(crate) struct RasterImage {
    pub(crate) kind: RasterKind,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Bitmap left edge relative to the pen position, in pixels.
    pub(crate) left: i32,
    /// Bitmap top edge above the baseline, in pixels.
    pub(crate) top: i32,
    pub(crate) data: Vec<u8>,
}

/// Indices into `Inner::families` for each fallback tier.
#[derive(Debug, Clone, Default)]
struct Chain {
    user: Vec<usize>,
    emoji: Option<usize>,
    system: Vec<usize>,
}

pub(crate) struct Inner {
    system: FontSystem,
    scaler: ScaleContext,
    /// `families[0]` is the primary; the rest are fallbacks, each spelled
    /// as the font database spells it.
    families: Vec<String>,
    chain: Chain,
    used_default_family: bool,
    styled_faces: HashMap<(usize, FontStyle), Option<ResolvedFace>>,
    char_faces: HashMap<(char, FontStyle), ResolvedFace>,
    /// Filled by a background thread started in [`Fonts::new`]; read
    /// without blocking, so a frame never waits for it.
    coverage: Arc<OnceLock<Coverage>>,
    color_faces: HashMap<FaceId, bool>,
    ink: HashMap<(FaceId, u16), GlyphInk>,
}

/// The font database, fallback chain and rasteriser shared by a
/// [`crate::Shaper`] and the [`crate::GlyphAtlas`] it feeds.
///
/// Cloning is cheap and every clone sees the same state. All methods take
/// `&self`; the state sits behind a mutex so a shaper on one thread and an
/// atlas on another may share it.
///
/// Each [`crate::Shaper`] keeps its own invalidation counter (see
/// [`Self::invalidate`]), so a font size change in one window does not
/// clear the atlases of other windows sharing these fonts.
#[derive(Clone)]
pub struct Fonts {
    inner: Arc<Mutex<Inner>>,
    /// Bumped whenever cached glyph bitmaps become stale (a font size
    /// change); atlases compare it to drop their contents.
    epoch: Arc<AtomicU64>,
    /// The same slot as `Inner::coverage`, so readiness is checked without
    /// taking the mutex.
    coverage: Arc<OnceLock<Coverage>>,
}

impl fmt::Debug for Fonts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Fonts");
        // `try_lock`: formatting must not deadlock when the caller (or a
        // `tracing` call inside `shape_line`) already holds the lock.
        match self.inner.try_lock() {
            Ok(inner) => {
                s.field("families", &inner.families)
                    .field("used_default_family", &inner.used_default_family);
            }
            Err(TryLockError::Poisoned(e)) => {
                let inner = e.into_inner();
                s.field("families", &inner.families)
                    .field("used_default_family", &inner.used_default_family);
            }
            Err(TryLockError::WouldBlock) => {
                s.field("state", &format_args!("<locked>"));
            }
        }
        s.field("epoch", &self.epoch.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Fonts {
    /// Loads the system fonts and resolves `config`'s families.
    ///
    /// The primary family is `config.family` when installed, else
    /// [`DEFAULT_FAMILY`], else the first monospaced family in the
    /// database. Loading the font database takes a noticeable fraction of a
    /// second (more in debug builds), so create one `Fonts` per process and
    /// share it.
    pub fn new(config: &FontConfig) -> Result<Self, RenderError> {
        Self::with_system(FontSystem::new(), config, IndexBuild::Background)
    }

    fn with_system(
        system: FontSystem,
        config: &FontConfig,
        index: IndexBuild,
    ) -> Result<Self, RenderError> {
        let db = system.db();
        let (primary, used_default_family) = if let Some(name) = find_family(db, &config.family) {
            (name, false)
        } else {
            tracing::warn!(
                target: "tessera_render",
                family = %config.family,
                fallback = DEFAULT_FAMILY,
                "configured font family is not installed"
            );
            let name = find_family(db, DEFAULT_FAMILY)
                .or_else(|| first_monospace_family(db))
                .ok_or_else(|| {
                    RenderError::NoFont(format!(
                        "neither {:?} nor {DEFAULT_FAMILY:?} nor any monospaced font is installed",
                        config.family
                    ))
                })?;
            (name, true)
        };

        let mut families = vec![primary];
        let push = |families: &mut Vec<String>, name: &str| -> Option<usize> {
            let found = find_family(db, name)?;
            if let Some(i) = families.iter().position(|f| *f == found) {
                return (i != 0).then_some(i);
            }
            families.push(found);
            Some(families.len() - 1)
        };
        let mut chain = Chain::default();
        for name in &config.fallback {
            if let Some(i) = push(&mut families, name) {
                if !chain.user.contains(&i) {
                    chain.user.push(i);
                }
            } else if find_family(db, name).is_none() {
                tracing::debug!(
                    target: "tessera_render",
                    family = %name,
                    "configured fallback family is not installed"
                );
            }
        }
        chain.emoji = push(&mut families, EMOJI_FAMILY);
        for name in SYSTEM_FALLBACK {
            if let Some(i) = push(&mut families, name)
                && !chain.user.contains(&i)
                && chain.emoji != Some(i)
                && !chain.system.contains(&i)
            {
                chain.system.push(i);
            }
        }

        let mut inner = Inner {
            system,
            scaler: ScaleContext::new(),
            families,
            chain,
            used_default_family,
            styled_faces: HashMap::new(),
            char_faces: HashMap::new(),
            coverage: Arc::new(OnceLock::new()),
            color_faces: HashMap::new(),
            ink: HashMap::new(),
        };
        let regular = inner.styled_face(0, FontStyle::Regular).ok_or_else(|| {
            RenderError::NoFont(format!(
                "family {:?} has no loadable face",
                inner.families[0]
            ))
        })?;
        if inner.load(regular.face).is_none() {
            return Err(RenderError::NoFont(format!(
                "family {:?} could not be parsed",
                inner.families[0]
            )));
        }
        if index == IndexBuild::Background {
            spawn_coverage_build(inner.system.db().clone(), &inner.coverage);
        }
        let coverage = Arc::clone(&inner.coverage);
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            epoch: Arc::new(AtomicU64::new(0)),
            coverage,
        })
    }

    /// A handle on the same fonts with its own invalidation counter, so
    /// [`Self::invalidate`] on it reaches only atlases built from it.
    pub(crate) fn with_own_epoch(self) -> Self {
        Self {
            inner: self.inner,
            epoch: Arc::new(AtomicU64::new(0)),
            coverage: self.coverage,
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves only caches half-updated;
        // every cache entry is written whole, so the state stays usable.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The primary family, spelled as the font database spells it.
    #[must_use]
    pub fn family(&self) -> String {
        self.lock().families[0].clone()
    }

    /// `true` when the configured family was missing and the primary is
    /// [`DEFAULT_FAMILY`] (or another monospaced family).
    #[must_use]
    pub fn used_default_family(&self) -> bool {
        self.lock().used_default_family
    }

    /// The fallback families in the order they are tried (configured, then
    /// emoji, then system), restricted to installed ones.
    ///
    /// Characters with emoji presentation try the emoji family before the
    /// system families; everything else tries it after them.
    #[must_use]
    pub fn fallback_families(&self) -> Vec<String> {
        let inner = self.lock();
        let chain = &inner.chain;
        chain
            .user
            .iter()
            .chain(chain.emoji.iter())
            .chain(chain.system.iter())
            .map(|&i| inner.families[i].clone())
            .collect()
    }

    /// Whether `family` is installed (case-insensitive).
    #[must_use]
    pub fn has_family(&self, family: &str) -> bool {
        find_family(self.lock().system.db(), family).is_some()
    }

    /// Which styled faces the primary family has; missing ones are
    /// synthesised.
    #[must_use]
    pub fn style_support(&self) -> StyleSupport {
        let mut inner = self.lock();
        let real = |inner: &mut Inner, style| {
            inner
                .styled_face(0, style)
                .is_some_and(|r| !r.synthetic_bold && !r.synthetic_italic)
        };
        StyleSupport {
            bold: real(&mut inner, FontStyle::Bold),
            italic: real(&mut inner, FontStyle::Italic),
            bold_italic: real(&mut inner, FontStyle::BoldItalic),
        }
    }

    /// The face the primary family uses for regular text.
    #[must_use]
    pub fn primary_face(&self) -> FaceId {
        self.lock()
            .styled_face(0, FontStyle::Regular)
            .map_or(FaceId::BUILTIN, |r| r.face)
    }

    /// The family name of `face`, or `None` for [`FaceId::BUILTIN`] and
    /// unknown faces.
    #[must_use]
    pub fn face_family(&self, face: FaceId) -> Option<String> {
        let FaceRepr::Font { id, .. } = face.0 else {
            return None;
        };
        let inner = self.lock();
        inner
            .system
            .db()
            .face(id)
            .and_then(|info| info.families.first())
            .map(|(name, _)| name.clone())
    }

    /// `true` when `face` carries colour glyphs (`sbix`, `COLR` or `CBDT`).
    #[must_use]
    pub fn face_is_color(&self, face: FaceId) -> bool {
        self.lock().is_color(face)
    }

    /// `true` once the background coverage index of every installed face
    /// is ready. Never goes back to `false`.
    ///
    /// Until then, a character that neither the primary family nor the
    /// fallback chain covers shapes to the primary face's `.notdef`; once
    /// it is ready, shaping the same cells again gives the covering face.
    /// A renderer that caches shaped rows should re-shape the rows that
    /// held `.notdef` glyphs (glyph id 0) when this flips to `true`.
    #[must_use]
    pub fn fallback_index_ready(&self) -> bool {
        self.coverage.get().is_some()
    }

    /// Counter bumped by [`Self::invalidate`]; atlases drop their contents
    /// when it moves.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Marks every cached glyph bitmap stale. Every [`crate::GlyphAtlas`]
    /// built on this handle (or a clone of it) clears itself at its next
    /// [`crate::GlyphAtlas::begin_frame`] (never in the middle of a frame).
    ///
    /// A [`crate::Shaper`] holds its own handle, returned by
    /// [`crate::Shaper::fonts`], and [`crate::Shaper::set_font_size`]
    /// invalidates only that one: atlases built from another shaper's
    /// fonts (another window) keep their glyphs.
    pub fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Rasterises `key`, or `None` when the face cannot draw it.
    pub(crate) fn rasterize(&self, key: &GlyphKey) -> Option<RasterImage> {
        self.lock().rasterize(key)
    }
}

impl Inner {
    /// Loads the font behind `face`.
    fn load(&mut self, face: FaceId) -> Option<Arc<Font>> {
        let FaceRepr::Font { id, weight } = face.0 else {
            return None;
        };
        self.system.get_font(id, fontdb::Weight(weight))
    }

    /// The face family `index` uses for `style`, with the synthesis needed
    /// when the family has no matching face.
    fn styled_face(&mut self, index: usize, style: FontStyle) -> Option<ResolvedFace> {
        if let Some(cached) = self.styled_faces.get(&(index, style)) {
            return *cached;
        }
        let resolved = self.query_styled_face(index, style);
        self.styled_faces.insert((index, style), resolved);
        resolved
    }

    fn query_styled_face(&mut self, index: usize, style: FontStyle) -> Option<ResolvedFace> {
        let family = self.families.get(index)?;
        let db = self.system.db();
        let id = db.query(&fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight: style.weight(),
            stretch: fontdb::Stretch::Normal,
            style: if style.is_italic() {
                fontdb::Style::Italic
            } else {
                fontdb::Style::Normal
            },
        })?;
        self.with_synthesis(id, style)
    }

    /// Face `id` drawing `style`, with the synthesis it needs: bold when
    /// the face is lighter than [`BOLD_THRESHOLD`] and has no `wght` axis
    /// reaching it, italic when the face is upright. Colour faces are
    /// never synthesised and always drawn at their own weight, so an emoji
    /// looks (and is cached) the same in every style.
    fn with_synthesis(&mut self, id: fontdb::ID, style: FontStyle) -> Option<ResolvedFace> {
        let info = self.system.db().face(id)?;
        let face_style = info.style;
        let face_weight = info.weight.0;
        let own = FaceId::font(id, info.weight);
        if self.is_color(own) {
            return Some(ResolvedFace {
                face: own,
                synthetic_bold: false,
                synthetic_italic: false,
            });
        }
        let face = FaceId::font(id, style.weight());
        let has_wght_axis = self.load(face).is_some_and(|font| {
            font.as_swash()
                .variations()
                .find_by_tag(swash::tag_from_bytes(b"wght"))
                .is_some_and(|axis| axis.max_value() >= f32::from(BOLD_THRESHOLD))
        });
        Some(ResolvedFace {
            face,
            synthetic_bold: style.is_bold() && face_weight < BOLD_THRESHOLD && !has_wght_axis,
            synthetic_italic: style.is_italic() && face_style == fontdb::Style::Normal,
        })
    }

    /// The face that draws `ch` in `style`: the primary family, then the
    /// fallback chain, then any installed face that covers it. Falls back to
    /// the primary (which draws `.notdef`) when nothing covers it.
    pub(crate) fn resolve(&mut self, ch: char, style: FontStyle) -> ResolvedFace {
        if let Some(r) = self.char_faces.get(&(ch, style)) {
            return *r;
        }
        let (resolved, cacheable) = self.resolve_uncached(ch, style);
        if !cacheable {
            // The coverage index is still building; resolve again later.
            return resolved;
        }
        if self.char_faces.len() >= MAX_CHAR_FACES {
            self.char_faces.clear();
        }
        self.char_faces.insert((ch, style), resolved);
        resolved
    }

    /// The resolved face, and whether it is final (`false` while the
    /// coverage index is still building and nothing in the chain covers
    /// `ch`).
    fn resolve_uncached(&mut self, ch: char, style: FontStyle) -> (ResolvedFace, bool) {
        let chain = self.chain.clone();
        let tail: Vec<usize> = if prefers_emoji(ch) {
            chain
                .emoji
                .iter()
                .chain(chain.system.iter())
                .copied()
                .collect()
        } else {
            chain
                .system
                .iter()
                .chain(chain.emoji.iter())
                .copied()
                .collect()
        };
        for index in std::iter::once(0).chain(chain.user).chain(tail) {
            if let Some(r) = self.styled_face(index, style)
                && self.covers(r.face, ch)
            {
                return (r, true);
            }
        }
        let coverage = Arc::clone(&self.coverage);
        let Some(index) = coverage.get() else {
            return (self.notdef(style), false);
        };
        if let Some(r) = index
            .lookup(ch)
            .and_then(|id| self.styled_sibling(id, ch, style))
        {
            return (r, true);
        }
        (self.notdef(style), true)
    }

    /// `style` drawn from the family of face `found` (which covers `ch`):
    /// the family's best face for `style` when that face also covers `ch`;
    /// else, for italic, the family's upright face at the requested weight;
    /// else the family's regular face; else `found` itself. Each is drawn
    /// with the synthesis it needs.
    ///
    /// The coverage index records the first face in database order, which
    /// is often a Black or Bold cut; a regular cell must not be drawn
    /// from it when the family ships a regular face, and an italic cell
    /// whose italic sibling lacks `ch` is the regular face plus synthetic
    /// italic, not the Bold cut plus synthetic italic.
    fn styled_sibling(
        &mut self,
        found: fontdb::ID,
        ch: char,
        style: FontStyle,
    ) -> Option<ResolvedFace> {
        let family = self
            .system
            .db()
            .face(found)?
            .families
            .first()
            .map(|(name, _)| name.clone());
        if let Some(family) = family {
            let mut wanted = vec![(style.weight(), style.is_italic())];
            if style.is_italic() {
                wanted.push((style.weight(), false));
            }
            if !wanted.contains(&(fontdb::Weight::NORMAL, false)) {
                wanted.push((fontdb::Weight::NORMAL, false));
            }
            for (weight, italic) in wanted {
                let Some(id) = self.system.db().query(&fontdb::Query {
                    families: &[fontdb::Family::Name(&family)],
                    weight,
                    stretch: fontdb::Stretch::Normal,
                    style: if italic {
                        fontdb::Style::Italic
                    } else {
                        fontdb::Style::Normal
                    },
                }) else {
                    continue;
                };
                if let Some(r) = self.with_synthesis(id, style)
                    && self.covers(r.face, ch)
                {
                    return Some(r);
                }
            }
        }
        self.with_synthesis(found, style)
    }

    /// The primary face in `style`, which draws `.notdef` for characters
    /// it does not cover.
    fn notdef(&mut self, style: FontStyle) -> ResolvedFace {
        self.styled_face(0, style).unwrap_or(ResolvedFace {
            face: FaceId::BUILTIN,
            synthetic_bold: false,
            synthetic_italic: false,
        })
    }

    fn covers(&mut self, face: FaceId, ch: char) -> bool {
        self.load(face)
            .is_some_and(|font| font.as_swash().charmap().map(ch) != 0)
    }

    pub(crate) fn is_color(&mut self, face: FaceId) -> bool {
        if let Some(&c) = self.color_faces.get(&face) {
            return c;
        }
        let color = self.load(face).is_some_and(|font| {
            let font = font.as_swash();
            [b"sbix", b"COLR", b"CBDT"]
                .iter()
                .any(|tag| font.table(swash::tag_from_bytes(tag)).is_some())
        });
        self.color_faces.insert(face, color);
        color
    }

    pub(crate) fn metrics(&mut self, face: FaceId, size_px: f32) -> Option<FaceMetrics> {
        let font = self.load(face)?;
        let font = font.as_swash();
        let metrics = font.metrics(&[]).scale(size_px);
        let glyphs = font.glyph_metrics(&[]).scale(size_px);
        let charmap = font.charmap();
        let cell_advance = ['0', 'M']
            .into_iter()
            .map(|c| charmap.map(c))
            .find(|&g| g != 0)
            .map_or(size_px * 0.6, |g| glyphs.advance_width(g));
        Some(FaceMetrics {
            ascent: metrics.ascent,
            descent: metrics.descent,
            leading: metrics.leading,
            cell_advance,
        })
    }

    /// Shapes `text` with `face` left to right at `size_px`.
    pub(crate) fn shape(&mut self, face: FaceId, text: &str, size_px: f32) -> Vec<RawGlyph> {
        let Some(font) = self.load(face) else {
            return Vec::new();
        };
        let units_per_em = f32::from(font.as_swash().metrics(&[]).units_per_em.max(1));
        let scale = size_px / units_per_em;
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(text);
        buffer.set_direction(Direction::LeftToRight);
        // `shape_run` partitions cells by cluster and needs clusters that
        // never decrease left to right and group a base with its marks;
        // that is the default today, set explicitly so it cannot drift.
        buffer.set_cluster_level(BufferClusterLevel::MonotoneGraphemes);
        buffer.guess_segment_properties();
        let shaped = font.shaper().shape(buffer, &[]);
        #[expect(
            clippy::cast_precision_loss,
            reason = "glyph positions are font units, far below f32's exact-integer range"
        )]
        let px = |units: i32| units as f32 * scale;
        shaped
            .glyph_infos()
            .iter()
            .zip(shaped.glyph_positions())
            .map(|(info, pos)| RawGlyph {
                glyph_id: u16::try_from(info.glyph_id).unwrap_or(0),
                cluster: info.cluster as usize,
                x_advance: px(pos.x_advance),
                x_offset: px(pos.x_offset),
                y_offset: px(pos.y_offset),
            })
            .collect()
    }

    /// The glyph `face`'s character map gives `ch`, before any shaping
    /// substitution (0 when the face does not cover it).
    pub(crate) fn nominal_glyph(&mut self, face: FaceId, ch: char) -> u16 {
        self.load(face)
            .map_or(0, |font| font.as_swash().charmap().map(ch))
    }

    /// `None` when the glyph draws nothing; otherwise the horizontal extent
    /// `(left, right)` of its ink relative to the origin at `size_px`
    /// (a negative left reaches into the previous cell).
    pub(crate) fn ink_extent(
        &mut self,
        face: FaceId,
        glyph_id: u16,
        size_px: f32,
    ) -> Option<(f32, f32)> {
        let ink = if let Some(&ink) = self.ink.get(&(face, glyph_id)) {
            ink
        } else {
            let ink = self.measure_ink(face, glyph_id);
            if self.ink.len() >= MAX_GLYPH_INK {
                self.ink.clear();
            }
            self.ink.insert((face, glyph_id), ink);
            ink
        };
        let font = self.load(face)?;
        let units_per_em = f32::from(font.as_swash().metrics(&[]).units_per_em.max(1));
        let px = size_px / units_per_em;
        (!ink.empty).then_some((ink.min_x_units * px, ink.max_x_units * px))
    }

    fn measure_ink(&mut self, face: FaceId, glyph_id: u16) -> GlyphInk {
        const EMPTY: GlyphInk = GlyphInk {
            empty: true,
            min_x_units: 0.0,
            max_x_units: 0.0,
        };
        let FaceRepr::Font { weight, .. } = face.0 else {
            return EMPTY;
        };
        let Some(font) = self.load(face) else {
            return EMPTY;
        };
        let swash_font = font.as_swash();
        let units_per_em = f32::from(swash_font.metrics(&[]).units_per_em.max(1));
        let mut scaler = build_scaler(&mut self.scaler, &font, units_per_em, weight);
        if !scaler.has_outlines() {
            // Bitmap-only faces (emoji): assume every glyph draws, within
            // its own advance.
            return GlyphInk {
                empty: false,
                min_x_units: 0.0,
                max_x_units: 0.0,
            };
        }
        match scaler.scale_outline(glyph_id) {
            Some(outline) if !outline.points().is_empty() => GlyphInk {
                empty: false,
                min_x_units: outline.bounds().min.x,
                max_x_units: outline.bounds().max.x,
            },
            _ => EMPTY,
        }
    }

    fn rasterize(&mut self, key: &GlyphKey) -> Option<RasterImage> {
        match key.face_id.0 {
            FaceRepr::Builtin => {
                let ch = char::from_u32(u32::from(key.glyph_id))?;
                let [width, height] = key.cell_size;
                let data = boxdraw::rasterize(ch, width, height)?;
                Some(RasterImage {
                    kind: RasterKind::Alpha,
                    width,
                    height,
                    left: 0,
                    top: i32::try_from(key.baseline_px).unwrap_or(i32::MAX),
                    data,
                })
            }
            FaceRepr::Font { weight, .. } => {
                let font = self.load(key.face_id)?;
                let size_px = key.size_px();
                let mut scaler = build_scaler(&mut self.scaler, &font, size_px, weight);
                let sources = [
                    Source::ColorOutline(0),
                    Source::ColorBitmap(StrikeWith::BestFit),
                    Source::Outline,
                ];
                let mut render = Render::new(&sources);
                render
                    .format(Format::Alpha)
                    .offset(Vector::new(f32::from(key.subpixel_x) / 3.0, 0.0));
                if key.synthetic_bold {
                    render.embolden(embolden_strength(size_px));
                }
                if key.synthetic_italic {
                    render.transform(Some(Transform::skew(
                        Angle::from_degrees(SYNTHETIC_ITALIC_SKEW_DEG),
                        Angle::from_degrees(0.0),
                    )));
                }
                let image = render.render(&mut scaler, key.glyph_id)?;
                let kind = match image.content {
                    Content::Mask => RasterKind::Alpha,
                    Content::Color => RasterKind::Color,
                    Content::SubpixelMask => {
                        tracing::warn!(
                            target: "tessera_render",
                            "swash returned a subpixel mask for an alpha request"
                        );
                        return None;
                    }
                };
                let mut data = image.data;
                // Colour outlines are composited onto transparent black,
                // which leaves them premultiplied; bitmap strikes (sbix PNGs)
                // decode straight and are premultiplied here, in linear
                // space, so the sRGB texture decodes to linear premultiplied
                // colour (ADR 0007).
                if kind == RasterKind::Color && matches!(image.source, Source::ColorBitmap(_)) {
                    premultiply(&mut data);
                }
                Some(RasterImage {
                    kind,
                    width: image.placement.width,
                    height: image.placement.height,
                    left: image.placement.left,
                    top: image.placement.top,
                    data,
                })
            }
        }
    }
}

/// How [`Fonts::with_system`] provides the coverage index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexBuild {
    /// Build it on a background thread.
    Background,
    /// Leave it unset (tests fill it by hand).
    #[cfg(test)]
    Never,
}

/// Builds the coverage index of `db` off the render path and stores it in
/// `slot`. Falls back to building it on this thread if no thread can be
/// spawned, so the index always appears.
fn spawn_coverage_build(db: fontdb::Database, slot: &Arc<OnceLock<Coverage>>) {
    let db = Arc::new(db);
    let (thread_db, thread_slot) = (Arc::clone(&db), Arc::clone(slot));
    let spawned = std::thread::Builder::new()
        .name("tessera-font-index".into())
        .spawn(move || {
            let _ = thread_slot.set(Coverage::build(&thread_db));
        });
    if let Err(e) = spawned {
        tracing::warn!(
            target: "tessera_render",
            error = %e,
            "could not spawn the font index thread; building it inline"
        );
        let _ = slot.set(Coverage::build(&db));
    }
}

/// Which installed face first covers each code point: sorted, disjoint,
/// inclusive `(first, last, face)` ranges.
#[derive(Debug, Default)]
struct Coverage {
    ranges: Vec<(u32, u32, fontdb::ID)>,
}

impl Coverage {
    /// Walks every face's character map once, in database order, so each
    /// code point maps to the first face that has a glyph for it (what a
    /// per-character linear search would find).
    fn build(db: &fontdb::Database) -> Self {
        let started = std::time::Instant::now();
        let mut faces: Vec<fontdb::ID> = Vec::new();
        // Index into `faces` per code point; `u32::MAX` = uncovered.
        let mut owner = vec![u32::MAX; CODE_SPACE];
        for info in db.faces() {
            let forbidden = info.families.iter().any(|(name, _)| {
                FORBIDDEN_FAMILIES
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(name))
            });
            if forbidden {
                continue;
            }
            let Ok(index) = u32::try_from(faces.len()) else {
                break;
            };
            faces.push(info.id);
            db.with_face_data(info.id, |data, face_index| {
                let Some(font) = swash::FontRef::from_index(data, face_index as usize) else {
                    return;
                };
                font.charmap().enumerate(|cp, glyph| {
                    if glyph != 0
                        && let Some(slot) = owner.get_mut(cp as usize)
                        && *slot == u32::MAX
                    {
                        *slot = index;
                    }
                });
            });
        }
        let mut ranges = Vec::new();
        let mut cp = 0;
        while cp < owner.len() {
            let face = owner[cp];
            let start = cp;
            while cp < owner.len() && owner[cp] == face {
                cp += 1;
            }
            if face != u32::MAX {
                // Both bounds are below CODE_SPACE, so they fit u32.
                let (first, last) = (
                    u32::try_from(start).unwrap_or(u32::MAX),
                    u32::try_from(cp - 1).unwrap_or(u32::MAX),
                );
                ranges.push((first, last, faces[face as usize]));
            }
        }
        tracing::debug!(
            target: "tessera_render",
            faces = faces.len(),
            ranges = ranges.len(),
            elapsed = ?started.elapsed(),
            "built the fallback coverage index"
        );
        Self { ranges }
    }

    fn lookup(&self, ch: char) -> Option<fontdb::ID> {
        let cp = u32::from(ch);
        let i = self.ranges.partition_point(|&(_, last, _)| last < cp);
        self.ranges
            .get(i)
            .filter(|&&(first, _, _)| first <= cp)
            .map(|&(_, _, id)| id)
    }
}

/// A swash scaler for `font` at `size_px`, with the `wght` axis (if any)
/// set to `weight`.
fn build_scaler<'a>(
    context: &'a mut ScaleContext,
    font: &'a Font,
    size_px: f32,
    weight: u16,
) -> Scaler<'a> {
    let swash_font = font.as_swash();
    let wght = swash::tag_from_bytes(b"wght");
    let mut builder = context.builder(swash_font).size(size_px).hint(false);
    if let Some(axis) = swash_font.variations().find_by_tag(wght) {
        let value = f32::from(weight).clamp(axis.min_value(), axis.max_value());
        builder =
            builder.normalized_coords(swash_font.variations().normalized_coords([(wght, value)]));
    }
    builder.build()
}

/// Outline growth for synthetic bold, in pixels: about 1/24 em, never
/// thinner than a third of a pixel nor thicker than two.
fn embolden_strength(size_px: f32) -> f32 {
    (size_px / 24.0).clamp(0.33, 2.0)
}

/// Premultiplies sRGB-encoded, straight-alpha RGBA in place, in linear
/// light: each colour byte becomes `encode(decode(c) * a)`.
///
/// The atlas texture is `Rgba8UnormSrgb`, whose sampler decodes each
/// channel on its own; multiplying the encoded bytes instead would decode
/// to `decode(c * a)`, which is darker than `decode(c) * a` at partial
/// alpha.
fn premultiply(rgba: &mut [u8]) {
    let decode: [f32; 256] = std::array::from_fn(|i| {
        // i < 256, so the conversion is exact.
        srgb_to_linear(f32::from(u8::try_from(i).unwrap_or(u8::MAX)) / 255.0)
    });
    for px in rgba.as_chunks_mut::<4>().0 {
        match px[3] {
            255 => {}
            0 => px[..3].fill(0),
            a => {
                let alpha = f32::from(a) / 255.0;
                for c in &mut px[..3] {
                    *c = unit_to_byte(linear_to_srgb(decode[usize::from(*c)] * alpha));
                }
            }
        }
    }
}

/// The sRGB transfer function's inverse (encoded to linear), `0.0..=1.0`.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The sRGB transfer function (linear to encoded), `0.0..=1.0`.
fn linear_to_srgb(l: f32) -> f32 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped to 0..=255 first"
)]
fn unit_to_byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The database spelling of `name`, matched case-insensitively against
/// every family name of every face.
fn find_family(db: &fontdb::Database, name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    db.faces()
        .flat_map(|info| info.families.iter())
        .find(|(family, _)| family.eq_ignore_ascii_case(name))
        .map(|(family, _)| family.clone())
}

fn first_monospace_family(db: &fontdb::Database) -> Option<String> {
    db.faces()
        .filter(|info| info.monospaced && !info.post_script_name.contains("Emoji"))
        .find_map(|info| info.families.first().map(|(name, _)| name.clone()))
}

/// Characters whose default presentation is emoji: they try
/// [`EMOJI_FAMILY`] before the system fallbacks so a monochrome symbol font
/// does not win.
pub(crate) fn prefers_emoji(ch: char) -> bool {
    let c = u32::from(ch);
    crate::shaping::is_wide(ch) && (c >= 0x1_F000 || (0x2300..=0x2BFF).contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_is_clamped_and_nan_safe() {
        assert!((sanitize_font_size(f32::NAN) - DEFAULT_FONT_SIZE_PT).abs() < f32::EPSILON);
        assert!((sanitize_font_size(f32::INFINITY) - DEFAULT_FONT_SIZE_PT).abs() < f32::EPSILON);
        assert!((sanitize_font_size(1.0) - MIN_FONT_SIZE_PT).abs() < f32::EPSILON);
        assert!((sanitize_font_size(1e6) - MAX_FONT_SIZE_PT).abs() < f32::EPSILON);
        assert!((sanitize_font_size(13.5) - 13.5).abs() < f32::EPSILON);
    }

    #[test]
    fn style_flags_map_to_weights() {
        assert_eq!(FontStyle::new(true, false).weight(), fontdb::Weight::BOLD);
        assert_eq!(FontStyle::new(false, true).weight(), fontdb::Weight::NORMAL);
        assert!(FontStyle::new(true, true).is_italic());
        assert!(FontStyle::new(true, true).is_bold());
    }

    #[test]
    fn premultiply_happens_in_linear_space() {
        let mut px = [255, 128, 0, 128, 10, 20, 30, 255, 200, 200, 200, 0];
        premultiply(&mut px);
        // Opaque pixels are unchanged and transparent ones become zero.
        assert_eq!(px[4..], [10, 20, 30, 255, 0, 0, 0, 0]);
        // Half-transparent white is encode(0.5 * decode(1.0)) = 188, not
        // the 128 a multiply on the encoded bytes gives; the sampler's
        // per-channel decode then yields exactly the linear 0.5.
        assert_eq!(px[..4], [188, 93, 0, 128]);
        let decoded = srgb_to_linear(f32::from(px[0]) / 255.0);
        assert!((decoded - 128.0 / 255.0).abs() < 0.005, "{decoded}");
    }

    #[test]
    fn emoji_preference_covers_pictographs_not_cjk() {
        assert!(prefers_emoji('\u{1F600}'));
        assert!(prefers_emoji('\u{2B50}'));
        assert!(!prefers_emoji('\u{4E2D}'));
        assert!(!prefers_emoji('a'));
    }

    fn menlo_without_index() -> Fonts {
        let config = FontConfig {
            family: DEFAULT_FAMILY.into(),
            size_pt: 13.0,
            scale: 1.0,
            fallback: Vec::new(),
        };
        Fonts::with_system(FontSystem::new(), &config, IndexBuild::Never).expect("Menlo")
    }

    #[test]
    fn debug_does_not_deadlock_while_the_lock_is_held() {
        let fonts = menlo_without_index();
        let guard = fonts.lock();
        let text = format!("{fonts:?}");
        drop(guard);
        assert!(text.contains("<locked>"), "{text}");
        assert!(format!("{fonts:?}").contains("Menlo"));
    }

    #[test]
    fn misses_wait_for_the_index_without_being_cached() {
        let fonts = menlo_without_index();
        let primary = fonts.primary_face();
        let mut inner = fonts.lock();
        // A character the fixed chain is unlikely to cover; if none of the
        // installed faces covers it either, there is nothing to resolve.
        let db_index = Coverage::build(inner.system.db());
        let Some(ch) = ['\u{13000}', '\u{10900}', '\u{1E900}', '\u{11400}']
            .into_iter()
            .find(|&c| {
                db_index.lookup(c).is_some()
                    && !matches!(inner.resolve_uncached(c, FontStyle::Regular), (_, true))
            })
        else {
            eprintln!("skipping: no installed face outside the chain covers the probes");
            return;
        };
        // Index not ready: the primary's .notdef, and nothing is cached.
        assert_eq!(inner.resolve(ch, FontStyle::Regular).face, primary);
        assert!(!inner.char_faces.contains_key(&(ch, FontStyle::Regular)));
        drop(inner);
        assert!(!fonts.fallback_index_ready());
        // Index ready: resolves to the covering face, now cached.
        let _ = fonts.lock().coverage.set(db_index);
        assert!(fonts.fallback_index_ready());
        let mut inner = fonts.lock();
        let resolved = inner.resolve(ch, FontStyle::Regular);
        assert_ne!(resolved.face, primary);
        assert!(inner.covers(resolved.face, ch));
        assert!(inner.char_faces.contains_key(&(ch, FontStyle::Regular)));
    }

    #[test]
    fn uncovered_characters_are_resolved_once_after_warm_up() {
        let fonts = menlo_without_index();
        let primary = fonts.primary_face();
        let index = Coverage::build(fonts.lock().system.db());
        let _ = fonts.lock().coverage.set(index);
        let mut inner = fonts.lock();
        // Unassigned plane 5 and supplementary private use area B, filtered
        // through the index so an installed font can never make one covered.
        let probes: Vec<char> = ['\u{50000}', '\u{100000}', '\u{100100}']
            .into_iter()
            .filter(|&c| {
                inner
                    .coverage
                    .get()
                    .is_some_and(|index| index.lookup(c).is_none())
            })
            .collect();
        assert!(!probes.is_empty(), "installed fonts cover every probe");
        for ch in probes {
            let key = (ch, FontStyle::Regular);
            assert!(!inner.char_faces.contains_key(&key));
            assert_eq!(inner.resolve(ch, FontStyle::Regular).face, primary);
            // Final and cached: later lookups are a map hit, not another
            // walk of the chain and the coverage index.
            assert_eq!(inner.char_faces.get(&key).map(|r| r.face), Some(primary));
            let before = inner.char_faces.len();
            assert_eq!(inner.resolve(ch, FontStyle::Regular).face, primary);
            assert_eq!(inner.char_faces.len(), before);
        }
    }

    #[test]
    fn coverage_fallback_honours_the_requested_style() {
        let fonts = menlo_without_index();
        let index = Coverage::build(fonts.lock().system.db());
        let _ = fonts.lock().coverage.set(index);
        assert!(fonts.fallback_index_ready());
        let mut inner = fonts.lock();
        let ch = '\u{1000}';
        let face_info = |inner: &Inner, face: FaceId| {
            let FaceRepr::Font { id, .. } = face.0 else {
                return None;
            };
            let info = inner.system.db().face(id)?;
            Some((info.families.first()?.0.clone(), info.weight.0, info.style))
        };
        let resolved = inner.resolve(ch, FontStyle::Regular);
        let Some((family, weight, style)) = face_info(&inner, resolved.face) else {
            eprintln!("skipping: no installed face covers U+1000");
            return;
        };
        // Skip only when the covering family ships no regular face that
        // has the character.
        let regulars: Vec<FaceId> = inner
            .system
            .db()
            .faces()
            .filter(|f| {
                f.families.iter().any(|(n, _)| *n == family)
                    && f.weight.0 == 400
                    && f.style == fontdb::Style::Normal
            })
            .map(|f| FaceId::font(f.id, f.weight))
            .collect();
        let regular_exists = regulars.into_iter().any(|f| inner.covers(f, ch));
        if !regular_exists {
            eprintln!("skipping: {family} has no regular face covering U+1000");
            return;
        }
        assert_eq!(
            (weight, style),
            (400, fontdb::Style::Normal),
            "regular U+1000 drawn from {family} weight {weight} {style:?}"
        );
        assert!(!resolved.synthetic_bold && !resolved.synthetic_italic);
    }

    /// When the covering family's italic face lacks the character, italic
    /// is synthesised on the family's regular face, not on whichever cut
    /// (often Bold) the coverage index recorded.
    #[test]
    fn coverage_fallback_synthesises_italic_on_the_regular_sibling() {
        let fonts = menlo_without_index();
        let index = Coverage::build(fonts.lock().system.db());
        let _ = fonts.lock().coverage.set(index);
        let mut inner = fonts.lock();
        let ch = '\u{0600}';
        let FaceRepr::Font { id, .. } = inner.resolve(ch, FontStyle::Regular).face.0 else {
            eprintln!("skipping: no installed face covers U+0600");
            return;
        };
        let Some(family) = inner
            .system
            .db()
            .face(id)
            .and_then(|f| f.families.first().map(|(n, _)| n.clone()))
        else {
            return;
        };
        let faces: Vec<(fontdb::ID, u16, fontdb::Style)> = inner
            .system
            .db()
            .faces()
            .filter(|f| f.families.iter().any(|(n, _)| *n == family))
            .map(|f| (f.id, f.weight.0, f.style))
            .collect();
        let covering = |inner: &mut Inner, weight: u16, style: fontdb::Style| {
            faces.iter().any(|&(id, w, s)| {
                w == weight && s == style && inner.covers(FaceId::font(id, fontdb::Weight(w)), ch)
            })
        };
        if covering(&mut inner, 400, fontdb::Style::Italic)
            || !covering(&mut inner, 400, fontdb::Style::Normal)
        {
            eprintln!("skipping: {family} has an italic U+0600 or no regular one");
            return;
        }
        let italic = inner.resolve(ch, FontStyle::new(false, true));
        let FaceRepr::Font { id, .. } = italic.face.0 else {
            panic!("italic U+0600 resolved to {italic:?}");
        };
        let info = inner.system.db().face(id).expect("face");
        assert_eq!(
            (info.weight.0, info.style),
            (400, fontdb::Style::Normal),
            "italic U+0600 drawn from {family} {} {:?}",
            info.weight.0,
            info.style
        );
        assert!(
            italic.synthetic_italic && !italic.synthetic_bold,
            "{italic:?}"
        );
    }

    #[test]
    fn fallback_index_ready_reports_when_the_index_lands() {
        let fonts = menlo_without_index();
        assert!(!fonts.fallback_index_ready());
        let index = Coverage::build(fonts.lock().system.db());
        let _ = fonts.lock().coverage.set(index);
        assert!(fonts.fallback_index_ready());
        assert!(fonts.clone().fallback_index_ready(), "clones share it");
    }

    #[test]
    fn fallback_index_ready_does_not_take_the_fonts_lock() {
        let fonts = menlo_without_index();
        let probe = fonts.clone();
        let _guard = fonts.lock();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(probe.fallback_index_ready());
        });
        let ready = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("fallback_index_ready blocked on the Fonts mutex");
        assert!(!ready);
    }

    #[test]
    fn ink_cache_is_bounded() {
        let fonts = menlo_without_index();
        let primary = fonts.primary_face();
        let mut inner = fonts.lock();
        let filler = GlyphInk {
            empty: true,
            min_x_units: 0.0,
            max_x_units: 0.0,
        };
        // Fill to the cap with distinct keys, without measuring anything.
        let mut ids = (0..=u16::MAX).flat_map(|g| [(FaceId::BUILTIN, g), (primary, g)]);
        while inner.ink.len() < MAX_GLYPH_INK {
            inner.ink.insert(ids.next().expect("enough keys"), filler);
        }
        inner.ink.remove(&(primary, u16::MAX));
        inner.ink.insert((FaceId::BUILTIN, u16::MAX), filler);
        let _ = inner.ink_extent(primary, u16::MAX, 13.0);
        assert!(inner.ink.len() <= MAX_GLYPH_INK, "{}", inner.ink.len());
        assert!(inner.ink.contains_key(&(primary, u16::MAX)));
    }

    #[test]
    fn ink_of_the_builtin_face_is_empty_not_a_panic() {
        let fonts = menlo_without_index();
        let mut inner = fonts.lock();
        assert!(inner.measure_ink(FaceId::BUILTIN, 0x2500).empty);
        assert_eq!(inner.ink_extent(FaceId::BUILTIN, 0x2500, 13.0), None);
    }

    #[test]
    fn embolden_strength_is_bounded() {
        assert!((embolden_strength(1.0) - 0.33).abs() < f32::EPSILON);
        assert!((embolden_strength(24.0) - 1.0).abs() < f32::EPSILON);
        assert!((embolden_strength(500.0) - 2.0).abs() < f32::EPSILON);
    }
}
