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
use crate::cmap;
use crate::error::RenderError;
use crate::font_scan;

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
const CODE_SPACE_U32: u32 = 0x11_0000;
/// [`CODE_SPACE_U32`] as an index.
const CODE_SPACE: usize = CODE_SPACE_U32 as usize;
/// The `cmap` table's tag.
const CMAP_TAG: u32 = u32::from_be_bytes(*b"cmap");

/// `font`'s `cmap` table, `None` when it has none or its record puts it
/// at offset 0, which swash's character map and the index's read
/// ([`read_face_cmap`]) take for no table (`FontRef::table` gives the
/// bytes from the file's start).
fn cmap_table<'a>(font: &swash::FontRef<'a>) -> Option<&'a [u8]> {
    font.table(CMAP_TAG)
        .filter(|table| !std::ptr::eq(table.as_ptr(), font.data.as_ptr()))
}

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

/// What the face choice needs to know about one face of a family.
///
/// `I` is the face's identity: a `fontdb::ID` in [`Inner`], anything
/// comparable in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Face<I> {
    id: I,
    /// Italic or oblique.
    italic: bool,
    /// The face's own weight (the default instance of a variable face).
    weight: u16,
    /// A `wght` axis reaching [`BOLD_THRESHOLD`]; only looked at, and only
    /// computed, for faces lighter than that.
    bold_axis: bool,
    /// Carries colour glyphs (`sbix`, `COLR` or `CBDT`).
    color: bool,
}

/// A face chosen to draw a style: the face instantiated at `weight`, with
/// the synthesis it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Picked<I> {
    id: I,
    weight: u16,
    synthetic_bold: bool,
    synthetic_italic: bool,
}

impl Picked<fontdb::ID> {
    fn resolved(self) -> ResolvedFace {
        ResolvedFace {
            face: FaceId::font(self.id, fontdb::Weight(self.weight)),
            synthetic_bold: self.synthetic_bold,
            synthetic_italic: self.synthetic_italic,
        }
    }
}

/// One family, as [`pick_in_family`] queries it.
trait Family {
    /// What identifies a face of the family.
    type Id: Copy + PartialEq;
    /// The family's best match for `query` (as the font database matches
    /// a weight and slant), or `None` when the family has no face at all.
    fn face(&mut self, query: FontStyle) -> Option<Face<Self::Id>>;
    /// Whether `picked` has a glyph for the character being resolved.
    fn covers(&mut self, picked: &Picked<Self::Id>) -> bool;
}

/// The pass over the chain that tries a family's styled face, the upright
/// face at the requested weight with synthetic italic, and (for italic)
/// the regular face with synthetic italic.
const STYLED_PASS: u8 = 0;

/// The pass over the chain, for bold styles only and only after
/// [`STYLED_PASS`] found nothing in any family, that tries a family's
/// regular face with synthetic bold: a later family's real bold beats an
/// earlier family's synthetic bold (CJK bold, #119).
const SYNTHETIC_BOLD_PASS: u8 = 1;

/// The number of passes over the chain that `style` needs.
const fn passes(style: FontStyle) -> u8 {
    if style.is_bold() {
        SYNTHETIC_BOLD_PASS + 1
    } else {
        STYLED_PASS + 1
    }
}

/// The queries tried within one family to draw `style`, in order, each
/// with the pass over the chain it belongs to: the styled face; the
/// upright face at the requested weight (synthetic italic); the regular
/// face, with synthetic italic in the styled pass and with synthetic bold
/// in the synthetic-bold pass. ADR 0007.
const fn family_order(style: FontStyle) -> &'static [(FontStyle, u8)] {
    match style {
        FontStyle::Regular => &[(FontStyle::Regular, STYLED_PASS)],
        FontStyle::Bold => &[
            (FontStyle::Bold, STYLED_PASS),
            (FontStyle::Regular, SYNTHETIC_BOLD_PASS),
        ],
        FontStyle::Italic => &[
            (FontStyle::Italic, STYLED_PASS),
            (FontStyle::Regular, STYLED_PASS),
        ],
        FontStyle::BoldItalic => &[
            (FontStyle::BoldItalic, STYLED_PASS),
            (FontStyle::Bold, STYLED_PASS),
            (FontStyle::Regular, SYNTHETIC_BOLD_PASS),
        ],
    }
}

/// `face` drawing `style`, with the synthesis it needs: bold when the
/// face is lighter than [`BOLD_THRESHOLD`] and has no `wght` axis reaching
/// it, italic when the face is upright. A colour face is never
/// synthesised and is always drawn at its own weight, so an emoji looks
/// (and is cached) the same in every style.
const fn synthesise<I: Copy>(face: &Face<I>, style: FontStyle) -> Picked<I> {
    if face.color {
        return Picked {
            id: face.id,
            weight: face.weight,
            synthetic_bold: false,
            synthetic_italic: false,
        };
    }
    Picked {
        id: face.id,
        weight: style.weight().0,
        synthetic_bold: style.is_bold() && face.weight < BOLD_THRESHOLD && !face.bold_axis,
        synthetic_italic: style.is_italic() && !face.italic,
    }
}

/// The face of `family` that draws the character in `style` in `pass`:
/// the first face of that pass in [`family_order`] that covers it, drawn
/// with the [`synthesise`] rule, or `None` when no face of the pass covers
/// it (the caller moves on to the next family, then to the next pass). A
/// face the database matches for two queries, in this pass or an earlier
/// one, is tested once.
fn pick_in_family<F: Family>(family: &mut F, style: FontStyle, pass: u8) -> Option<Picked<F::Id>> {
    let mut tried: [Option<F::Id>; 3] = [None; 3];
    for (slot, &(query, query_pass)) in family_order(style).iter().enumerate() {
        if query_pass > pass {
            break;
        }
        let Some(face) = family.face(query) else {
            continue;
        };
        if tried.contains(&Some(face.id)) {
            continue;
        }
        tried[slot] = Some(face.id);
        if query_pass < pass {
            // Tested in the earlier pass.
            continue;
        }
        let picked = synthesise(&face, style);
        if family.covers(&picked) {
            return Some(picked);
        }
    }
    None
}

/// A family of the fallback chain, by its index in `Inner::families`;
/// its faces are cached per query style.
struct ChainFamily<'a> {
    inner: &'a mut Inner,
    index: usize,
    ch: char,
}

impl Family for ChainFamily<'_> {
    type Id = fontdb::ID;

    fn face(&mut self, query: FontStyle) -> Option<Face<fontdb::ID>> {
        self.inner.family_face(self.index, query)
    }

    fn covers(&mut self, picked: &Picked<fontdb::ID>) -> bool {
        self.inner.covers(picked.resolved().face, self.ch)
    }
}

/// A family found through the coverage index, by name; not cached (the
/// per-character result is).
struct NamedFamily<'a> {
    inner: &'a mut Inner,
    name: &'a str,
    ch: char,
}

impl Family for NamedFamily<'_> {
    type Id = fontdb::ID;

    fn face(&mut self, query: FontStyle) -> Option<Face<fontdb::ID>> {
        let id = query_family(self.inner.system.db(), self.name, query)?;
        self.inner.face_traits(id)
    }

    fn covers(&mut self, picked: &Picked<fontdb::ID>) -> bool {
        self.inner.covers(picked.resolved().face, self.ch)
    }
}

/// The database's best match in family `name` for `style`'s weight and
/// slant.
fn query_family(db: &fontdb::Database, name: &str, style: FontStyle) -> Option<fontdb::ID> {
    db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(name)],
        weight: style.weight(),
        stretch: fontdb::Stretch::Normal,
        style: if style.is_italic() {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
    })
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
    /// Faces of `system` whose file is read into memory on first use;
    /// each is admitted by [`Self::face_traits`], before its first load
    /// (#683).
    owned: font_scan::OwnedFaces,
    scaler: ScaleContext,
    /// `families[0]` is the primary; the rest are fallbacks, each spelled
    /// as the font database spells it.
    families: Vec<String>,
    chain: Chain,
    used_default_family: bool,
    /// The face each family matches for each query style; bounded by
    /// families x 4.
    styled_faces: HashMap<(usize, FontStyle), Option<Face<fontdb::ID>>>,
    char_faces: HashMap<(char, FontStyle), ResolvedFace>,
    /// Filled by a background thread started in [`Fonts::new`]; read
    /// without blocking, so a frame never waits for it.
    coverage: Arc<OnceLock<Coverage>>,
    color_faces: HashMap<FaceId, bool>,
    ink: HashMap<(FaceId, u16), GlyphInk>,
    /// Resolutions handed out while the coverage index was building that
    /// may change once it is ready (a `.notdef`, or a chain family's
    /// synthetic bold the index may beat), counted (wrapping) so a shaper
    /// can tell whether a line used one ([`Self::provisional`]).
    provisional: u64,
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
        Self::installed(config, None)
    }

    /// [`Self::new`], calling `notice` once the background coverage index
    /// has landed ([`Self::fallback_index_ready`] is `true` by then), so a
    /// host that pauses its drawing while nothing changes can draw the
    /// rows shaped with a provisional face again (#594).
    ///
    /// `notice` runs exactly once, on the index thread (or on this thread,
    /// before this returns, if no thread could be spawned), and never if
    /// loading the fonts fails. It must return quickly and must not take
    /// these fonts' lock for long: the index thread holds no lock while it
    /// runs.
    pub fn with_index_notice(
        config: &FontConfig,
        notice: impl FnOnce() + Send + 'static,
    ) -> Result<Self, RenderError> {
        Self::installed(config, Some(Box::new(notice)))
    }

    /// The installed fonts and their coverage index, `notice` called once
    /// it lands: the process's shared ones while a test shares them
    /// (`share_installed_fonts`, feature `test-hooks`), else loaded and
    /// built for these fonts.
    fn installed(config: &FontConfig, notice: Option<IndexNotice>) -> Result<Self, RenderError> {
        #[cfg(feature = "test-hooks")]
        if let Some((db, owned, coverage)) = shared_fonts::get() {
            return Self::with_fonts(
                font_scan::font_system(db),
                owned,
                config,
                IndexBuild::Shared(coverage, notice),
            );
        }
        let (system, owned) = font_scan::installed_font_system();
        Self::with_fonts(system, owned, config, IndexBuild::Background(notice))
    }

    /// [`Self::with_fonts`] over `system` with no lazily read user fonts.
    #[cfg(test)]
    fn with_system(
        system: FontSystem,
        config: &FontConfig,
        index: IndexBuild,
    ) -> Result<Self, RenderError> {
        Self::with_fonts(system, font_scan::OwnedFaces::default(), config, index)
    }

    /// The fonts of `system`, whose faces in `owned` are read into memory
    /// on first use (#683).
    fn with_fonts(
        system: FontSystem,
        owned: font_scan::OwnedFaces,
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

        let coverage = index.slot();
        let mut inner = Inner {
            system,
            owned,
            scaler: ScaleContext::new(),
            families,
            chain,
            used_default_family,
            styled_faces: HashMap::new(),
            char_faces: HashMap::new(),
            coverage,
            color_faces: HashMap::new(),
            ink: HashMap::new(),
            provisional: 0,
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
        index.start(inner.system.db(), &inner.coverage);
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

    /// [`Self::new`] without the background coverage index; tests land
    /// it with [`Self::build_index_now`].
    #[cfg(test)]
    pub(crate) fn without_index(config: &FontConfig) -> Result<Self, RenderError> {
        let (system, owned) = font_scan::installed_font_system();
        Self::with_fonts(system, owned, config, IndexBuild::Never)
    }

    /// Builds the coverage index on this thread and lands it, as the
    /// background thread would.
    #[cfg(test)]
    pub(crate) fn build_index_now(&self) {
        let index = Coverage::build(self.lock().system.db());
        let _ = self.coverage.set(index);
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
    /// fallback chain covers shapes to the primary face's `.notdef`, and a
    /// bold character no chain family has a bold face for is a chain
    /// family's regular face with synthetic bold, which a real bold found
    /// through the index may beat; once it is ready, shaping the same
    /// cells again gives the final face. A renderer that caches shaped
    /// rows should re-shape the rows shaped with such a provisional face
    /// when this flips to `true` (the renderer asks the shaper whether a
    /// line used one).
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
        self.family_face(index, style)
            .map(|face| synthesise(&face, style).resolved())
    }

    /// The face family `index` matches for `query`, cached.
    fn family_face(&mut self, index: usize, query: FontStyle) -> Option<Face<fontdb::ID>> {
        if let Some(cached) = self.styled_faces.get(&(index, query)) {
            return *cached;
        }
        let face = self
            .families
            .get(index)
            .and_then(|family| query_family(self.system.db(), family, query))
            .and_then(|id| self.face_traits(id));
        self.styled_faces.insert((index, query), face);
        face
    }

    /// What [`synthesise`] needs to know about face `id`, read from the
    /// face loaded at its own weight.
    ///
    /// Every [`FaceId`] of a font is made from what this returns, so this
    /// is where a lazily read user face is admitted (#683), before any
    /// load of it, and [`Self::load`] needs no check of its own.
    fn face_traits(&mut self, id: fontdb::ID) -> Option<Face<fontdb::ID>> {
        self.owned.admit(&mut self.system, id);
        let info = self.system.db().face(id)?;
        let italic = info.style != fontdb::Style::Normal;
        let weight = info.weight;
        let own = FaceId::font(id, weight);
        let color = self.is_color(own);
        let bold_axis = !color
            && weight.0 < BOLD_THRESHOLD
            && self.load(own).is_some_and(|font| {
                font.as_swash()
                    .variations()
                    .find_by_tag(swash::tag_from_bytes(b"wght"))
                    .is_some_and(|axis| axis.max_value() >= f32::from(BOLD_THRESHOLD))
            });
        Some(Face {
            id,
            italic,
            weight: weight.0,
            bold_axis,
            color,
        })
    }

    /// Face `id` drawing `style`, with the synthesis it needs (the
    /// [`synthesise`] rule).
    fn with_synthesis(&mut self, id: fontdb::ID, style: FontStyle) -> Option<ResolvedFace> {
        self.face_traits(id)
            .map(|face| synthesise(&face, style).resolved())
    }

    /// The face that draws `ch` in `style`: the primary family, then the
    /// fallback chain, then any installed face that covers it. Falls back to
    /// the primary (which draws `.notdef`) when nothing covers it.
    pub(crate) fn resolve(&mut self, ch: char, style: FontStyle) -> ResolvedFace {
        if let Some(r) = self.char_faces.get(&(ch, style)) {
            return *r;
        }
        self.resolve_miss(ch, style)
    }

    /// [`Self::resolve`] on a cache miss. Out of line so the walk of the
    /// chain does not bloat the hit path (a lookup per character drawn).
    #[inline(never)]
    fn resolve_miss(&mut self, ch: char, style: FontStyle) -> ResolvedFace {
        let (resolved, cacheable) = self.resolve_uncached(ch, style);
        if !cacheable {
            // The coverage index is still building; resolve again later.
            self.provisional = self.provisional.wrapping_add(1);
            return resolved;
        }
        if self.char_faces.len() >= MAX_CHAR_FACES {
            self.char_faces.clear();
        }
        self.char_faces.insert((ch, style), resolved);
        resolved
    }

    /// Like [`Self::resolve`] with the presentation decided by the caller:
    /// `emoji` (a cluster with VS16) tries the colour emoji family first,
    /// before even the primary family; `!emoji` (VS15) tries it only after
    /// the system fallbacks. Not cached; such clusters are rare.
    pub(crate) fn resolve_presented(
        &mut self,
        ch: char,
        style: FontStyle,
        emoji: bool,
    ) -> ResolvedFace {
        if emoji {
            if let Some(index) = self.chain.emoji
                && let Some(r) = self.styled_face(index, style)
                && self.covers(r.face, ch)
            {
                return r;
            }
            return self.resolve(ch, style);
        }
        if !prefers_emoji(ch) {
            return self.resolve(ch, style);
        }
        let (resolved, settled) = self.resolve_uncached_as(ch, style, false);
        if !settled {
            self.provisional = self.provisional.wrapping_add(1);
        }
        resolved
    }

    /// How many resolutions so far may change once the coverage index is
    /// ready (wrapping): a line shaped under one lock whose count moved
    /// holds such a face and should be shaped again when
    /// [`Fonts::fallback_index_ready`] turns `true`. Never moves once the
    /// index is ready.
    pub(crate) const fn provisional(&self) -> u64 {
        self.provisional
    }

    /// The resolved face, and whether it is final (`false` while the
    /// coverage index is still building and nothing in the chain covers
    /// `ch`).
    fn resolve_uncached(&mut self, ch: char, style: FontStyle) -> (ResolvedFace, bool) {
        self.resolve_uncached_as(ch, style, prefers_emoji(ch))
    }

    /// [`Self::resolve_uncached`] with an explicit emoji presentation.
    fn resolve_uncached_as(
        &mut self,
        ch: char,
        style: FontStyle,
        emoji: bool,
    ) -> (ResolvedFace, bool) {
        let chain = self.chain.clone();
        let tail: Vec<usize> = if emoji {
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
        // Each pass walks the chain, then the family the coverage index
        // finds, so a real bold found through the index beats the chain's
        // synthetic bold. A synthetic bold the chain finds before the
        // index is ready may lose to the index later: not cached.
        let coverage = Arc::clone(&self.coverage);
        let index = coverage.get();
        let mut found = None;
        for pass in 0..passes(style) {
            let order = std::iter::once(0)
                .chain(chain.user.iter().copied())
                .chain(tail.iter().copied());
            for family_index in order {
                let mut family = ChainFamily {
                    inner: self,
                    index: family_index,
                    ch,
                };
                if let Some(picked) = pick_in_family(&mut family, style, pass) {
                    return (picked.resolved(), pass == STYLED_PASS || index.is_some());
                }
            }
            if pass == STYLED_PASS {
                found = index.and_then(|index| index.lookup(ch));
            }
            if let Some(id) = found
                && let Some(r) = self.styled_sibling(id, ch, style, pass)
            {
                return (r, true);
            }
        }
        if index.is_none() {
            return (self.notdef(style), false);
        }
        if let Some(r) = found.and_then(|id| self.with_synthesis(id, style)) {
            return (r, true);
        }
        (self.notdef(style), true)
    }

    /// `style` drawn in `pass` from the family of face `found` (which
    /// covers `ch`): the family's face chosen by [`pick_in_family`], or
    /// `None` (the caller falls back to `found` itself, with the
    /// synthesis it needs, after the last pass).
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
        pass: u8,
    ) -> Option<ResolvedFace> {
        let name = self
            .system
            .db()
            .face(found)?
            .families
            .first()
            .map(|(name, _)| name.clone());
        let mut family = NamedFamily {
            inner: self,
            name: &name?,
            ch,
        };
        pick_in_family(&mut family, style, pass).map(Picked::resolved)
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
        self.nominal_glyph(face, ch) != 0
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
        let cmap = cmap_table(&font).unwrap_or_default();
        let cell_advance = ['0', 'M']
            .into_iter()
            .map(|c| cmap::map(cmap, u32::from(c)))
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
    /// substitution (0 when the face does not cover it): swash's
    /// `Charmap::map`, through Tessera's own reader of the same subtable
    /// ([`cmap::map`]), whose glyph arithmetic wraps where swash's
    /// overflowed in debug builds (#716).
    pub(crate) fn nominal_glyph(&mut self, face: FaceId, ch: char) -> u16 {
        self.load(face).map_or(0, |font| {
            cmap_table(&font.as_swash()).map_or(0, |table| cmap::map(table, u32::from(ch)))
        })
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

/// Called once when the coverage index lands ([`Fonts::with_index_notice`]).
type IndexNotice = Box<dyn FnOnce() + Send + 'static>;

/// How [`Fonts::with_system`] provides the coverage index.
enum IndexBuild {
    /// Build it on a background thread, then call the notice, if any.
    Background(Option<IndexNotice>),
    /// Leave it unset (tests fill it by hand).
    #[cfg(test)]
    Never,
    /// Use the process's shared index in this slot, built or building, and
    /// call the notice, if any, once it lands (#660).
    #[cfg(feature = "test-hooks")]
    Shared(Arc<OnceLock<Coverage>>, Option<IndexNotice>),
}

impl IndexBuild {
    /// The slot the index lands in: the shared one, or a new one.
    fn slot(&self) -> Arc<OnceLock<Coverage>> {
        match self {
            Self::Background(_) => Arc::new(OnceLock::new()),
            #[cfg(test)]
            Self::Never => Arc::new(OnceLock::new()),
            #[cfg(feature = "test-hooks")]
            Self::Shared(slot, _) => Arc::clone(slot),
        }
    }

    /// Starts the index of `db` landing in `slot` (from [`Self::slot`]).
    fn start(self, db: &fontdb::Database, slot: &Arc<OnceLock<Coverage>>) {
        match self {
            Self::Background(notice) => spawn_coverage_build(db.clone(), slot, notice),
            #[cfg(test)]
            Self::Never => {}
            #[cfg(feature = "test-hooks")]
            Self::Shared(_, notice) => {
                if let Some(notice) = notice {
                    shared_fonts::notice_on_landing(slot, notice);
                }
            }
        }
    }
}

/// Builds the coverage index of `db` off the render path, stores it in
/// `slot`, then calls `notice`. Falls back to building it on this thread if
/// no thread can be spawned, so the index always appears.
fn spawn_coverage_build(
    db: fontdb::Database,
    slot: &Arc<OnceLock<Coverage>>,
    notice: Option<IndexNotice>,
) {
    let db = Arc::new(db);
    let (thread_db, thread_slot) = (Arc::clone(&db), Arc::clone(slot));
    // The notice moves into the thread's closure, and comes back with the
    // error if the thread cannot be spawned.
    let notice = Mutex::new(notice);
    let notice = Arc::new(notice);
    let thread_notice = Arc::clone(&notice);
    let spawned = std::thread::Builder::new()
        .name("tessera-font-index".into())
        .spawn(move || land(&thread_slot, Coverage::build(&thread_db), &thread_notice));
    if let Err(e) = spawned {
        tracing::warn!(
            target: "tessera_render",
            error = %e,
            "could not spawn the font index thread; building it inline"
        );
        land(slot, Coverage::build(&db), &notice);
    }
}

/// Stores `index` in `slot` and calls the notice, once: only the first
/// landing of a slot calls it.
fn land(slot: &OnceLock<Coverage>, index: Coverage, notice: &Mutex<Option<IndexNotice>>) {
    #[cfg(feature = "test-hooks")]
    test_hold::wait();
    if slot.set(index).is_err() {
        return;
    }
    let notice = notice.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(notice) = notice {
        notice();
    }
}

/// Holds every coverage index that finishes building while the hold is on
/// until it is released (feature `test-hooks`): a test opens a view with
/// provisional rows, then lands the index with nothing else going on.
#[cfg(feature = "test-hooks")]
mod test_hold {
    use std::sync::{Condvar, Mutex, PoisonError};

    static HELD: Mutex<bool> = Mutex::new(false);
    static RELEASED: Condvar = Condvar::new();

    /// Holds (`true`) or releases (`false`) the indexes.
    pub(super) fn set(hold: bool) {
        *HELD.lock().unwrap_or_else(PoisonError::into_inner) = hold;
        if !hold {
            RELEASED.notify_all();
        }
    }

    /// Whether the hold is on.
    pub(super) fn held() -> bool {
        *HELD.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits while the hold is on.
    pub(super) fn wait() {
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        while *held {
            held = RELEASED.wait(held).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Holds (`true`) every coverage index that finishes building from now on,
/// so it does not land, until a call with `false` releases them (#594).
/// Process-wide. Test-only (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn hold_fallback_index(hold: bool) {
    test_hold::set(hold);
}

/// One scan of the installed fonts and one coverage index for every
/// [`Fonts`] loaded while a test shares them (feature `test-hooks`, #660):
/// a test host that replaces its core many times builds the index once,
/// where each core would build its own, and Thread Sanitizer's `munmap`
/// interceptor faulted under that churn (docs/testing.md).
#[cfg(feature = "test-hooks")]
mod shared_fonts {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};

    use cosmic_text::fontdb;

    use super::{Coverage, IndexNotice, font_scan, spawn_coverage_build, test_hold};

    /// The scan and the index slot every shared load clones.
    type Shared = (
        fontdb::Database,
        font_scan::OwnedFaces,
        Arc<OnceLock<Coverage>>,
    );

    static SHARE: AtomicBool = AtomicBool::new(false);
    static SHARED: Mutex<Option<Shared>> = Mutex::new(None);
    /// Coverage index builds this process started.
    pub(super) static BUILDS: AtomicU64 = AtomicU64::new(0);

    /// Shares (`true`) from the next load on, or stops sharing and forgets
    /// the shared fonts (`false`).
    pub(super) fn set(share: bool) {
        SHARE.store(share, Ordering::SeqCst);
        if !share {
            *SHARED.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    /// The shared scan, the first call scanning and starting the index,
    /// or `None` when nothing is shared or the indexes are held: a held
    /// load wants an index of its own that waits for the release (#594).
    ///
    /// `SHARE` is not read again under the `SHARED` lock, so a load racing
    /// [`set`]`(false)` can fill `SHARED` after `set` emptied it, and that
    /// scan is the one the next sharing load reuses. Test-only, and tests
    /// switch sharing while no font loads, so it is left so.
    pub(super) fn get() -> Option<Shared> {
        if !SHARE.load(Ordering::SeqCst) || test_hold::held() {
            return None;
        }
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        let (db, owned, slot) = shared.get_or_insert_with(|| {
            let (db, owned) = font_scan::installed_fonts();
            let slot = Arc::new(OnceLock::new());
            spawn_coverage_build(db.clone(), &slot, None);
            (db, owned, slot)
        });
        Some((db.clone(), owned.clone(), Arc::clone(slot)))
    }

    /// Calls `notice` once the index lands in `slot`, on a thread of its
    /// own as an unshared index's notice is called, or on this one, after
    /// the landing, if no thread can be spawned.
    ///
    /// Test-only limits, left so: a shared build that panicked never
    /// lands, so every later load leaves one thread here waiting for ever;
    /// and when the build's own thread cannot be spawned, [`get`] builds
    /// the index inline while it holds the `SHARED` lock, so other loads
    /// wait for that build.
    pub(super) fn notice_on_landing(slot: &Arc<OnceLock<Coverage>>, notice: IndexNotice) {
        let notice = Arc::new(Mutex::new(Some(notice)));
        let (thread_slot, thread_notice) = (Arc::clone(slot), Arc::clone(&notice));
        let call = |slot: &OnceLock<Coverage>, notice: &Mutex<Option<IndexNotice>>| {
            slot.wait();
            let notice = notice.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(notice) = notice {
                notice();
            }
        };
        let spawned = std::thread::Builder::new()
            .name("tessera-font-index".into())
            .spawn(move || call(&thread_slot, &thread_notice));
        if spawned.is_err() {
            call(slot, &notice);
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::mpsc;
        use std::time::Duration;

        use super::*;

        /// #660 M2: a shared load's notice waits for the index to land,
        /// and comes once.
        #[test]
        fn a_shared_notice_comes_once_the_index_lands() {
            let slot = Arc::new(OnceLock::new());
            let (tx, rx) = mpsc::channel();
            notice_on_landing(&slot, Box::new(move || tx.send(()).expect("receiver")));
            assert!(
                rx.recv_timeout(Duration::from_millis(300)).is_err(),
                "a notice before the index landed"
            );
            assert!(slot.set(Coverage::default()).is_ok());
            rx.recv_timeout(Duration::from_secs(30))
                .expect("the notice once the index landed");
            assert!(
                rx.recv_timeout(Duration::from_millis(300)).is_err(),
                "a second notice"
            );
        }
    }
}

/// Makes every [`Fonts`] loaded from now on share one scan of the
/// installed fonts and one coverage index, process-wide (`true`), so the
/// index is built once however many fonts a test host loads; `false`
/// stops that and forgets the shared ones. A load while the indexes are
/// held ([`hold_fallback_index`]) builds its own. The fonts are those
/// installed when the first shared load scans them (#660).
/// Test-only (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn share_installed_fonts(share: bool) {
    shared_fonts::set(share);
}

/// How many coverage indexes this process has started building (#660).
/// Test-only (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
#[must_use]
pub fn fallback_index_builds() -> u64 {
    shared_fonts::BUILDS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Which installed face first covers each code point: sorted, disjoint,
/// inclusive `(first, last, face)` ranges.
#[derive(Debug, Default)]
struct Coverage {
    ranges: Vec<(u32, u32, fontdb::ID)>,
}

impl Coverage {
    /// Walks every face's character map once, so each code point maps to
    /// the first face in database order that has a glyph for it (what a
    /// per-character linear search would find).
    fn build(db: &fontdb::Database) -> Self {
        Self::build_with_stats(db).0
    }

    /// [`Self::build`], also returning what it cost.
    ///
    /// A font file is read with `read(2)`, never memory mapped: another
    /// process may truncate or rewrite an installed font in place while
    /// the index builds, and touching a mapped page past a file's new end
    /// raises `SIGBUS`, which kills the app (#668). A read of a file that
    /// changes under it only comes back short or with other bytes, which
    /// leaves the face out of the index or indexes what the file now
    /// holds; it never takes the process down.
    ///
    /// Each file is opened once, however many faces a collection (`.ttc`)
    /// holds (#660), and only what the index needs is read from it: the
    /// collection header, each face's table directory (a binary search of
    /// 16-byte records, as swash does) and its `cmap` table, at most
    /// [`MAX_INDEX_CMAP_BYTES`] of it. One buffer per file holds the `cmap`
    /// being read, reused by the next face when the faces share it, and
    /// is dropped before the next file opens; files are read one at a
    /// time. In-memory fonts (`Source::Binary`) are read where they are.
    ///
    /// A face whose `cmap` is the one the face before it in the same file
    /// read (same offset and length) is not walked again (#716): it comes
    /// later in database order, so it can win no code point the earlier
    /// face did not already take. The whole build walks at most
    /// [`MAX_INDEX_STEPS`] and reads at most [`MAX_INDEX_CMAP_READ`] of
    /// `cmap` tables (a collection's face count is unbounded): the face
    /// that runs out of either is cut there and every later face is left
    /// out, with one warning.
    fn build_with_stats(db: &fontdb::Database) -> (Self, BuildStats) {
        #[cfg(feature = "test-hooks")]
        shared_fonts::BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let started = std::time::Instant::now();
        let mut faces: Vec<fontdb::ID> = Vec::new();
        // One entry per face: (its source, index into `faces`, face index
        // in the source). A source is named by the index of its first face:
        // one per font file, keyed by path, and one per face for data held
        // in memory (`Source::Binary`).
        let mut members: Vec<(u32, u32, u32)> = Vec::new();
        let mut by_path: HashMap<&std::path::Path, u32> = HashMap::new();
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
            let source = match &info.source {
                fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => {
                    *by_path.entry(path.as_path()).or_insert(index)
                }
                fontdb::Source::Binary(_) => index,
            };
            members.push((source, index, info.index));
        }
        drop(by_path);
        // Each source's faces become one run, in database order.
        members.sort_unstable();
        // Index into `faces` per code point; `u32::MAX` = uncovered. A
        // source can hold a face that comes after a later source's first
        // face, so a slot takes the lowest index that covers it, not the
        // first one written: first face wins in database order.
        let mut build = Build::new();
        for run in members.chunk_by(|a, b| a.0 == b.0) {
            if build.stopped {
                break;
            }
            let Some(&load_by) = run
                .first()
                .and_then(|&(source, _, _)| faces.get(source as usize))
            else {
                continue;
            };
            let faces_of_run = run
                .iter()
                .map(|&(_, index, face_index)| (index, face_index));
            match db.face(load_by).map(|f| &f.source) {
                Some(fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _)) => {
                    build.stats.opens += 1;
                    index_file(path, faces_of_run, &mut build);
                }
                Some(fontdb::Source::Binary(_)) => {
                    // Resident data: `with_face_data` maps nothing for it.
                    let _ = db.with_face_data(load_by, |data, _| {
                        for (index, face_index) in faces_of_run {
                            if let Some(table) =
                                swash::FontRef::from_index(data, face_index as usize)
                                    .and_then(|font| cmap_table(&font))
                            {
                                build.walk_face(index, table);
                            }
                        }
                    });
                }
                None => {}
            }
        }
        drop(members);
        let Build { owner, stats, .. } = build;
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
            opens = stats.opens,
            code_points = stats.walked.code_points,
            records = stats.walked.records,
            cmap_bytes = stats.cmap_bytes,
            reused = stats.reused,
            elapsed = ?started.elapsed(),
            "built the fallback coverage index"
        );
        (Self { ranges }, stats)
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

/// Most steps the whole coverage build walks over all faces' character
/// maps (#716): a code point visited or a group or segment read is one
/// step ([`cmap::Walk::steps`]). 64 walks of the code space; the
/// installed fonts on macOS 26 (1 016 faces) take about 2.8 million, 4 %
/// of it. A face costs at most one walk of the code space plus one step
/// per record (#689), but a collection may hold any number of faces.
const MAX_INDEX_STEPS: u64 = 64 * CODE_SPACE_U32 as u64;

/// Most bytes of `cmap` tables the whole coverage build reads (#716): 64
/// tables at [`MAX_INDEX_CMAP_BYTES`], 128 MiB. A `cmap` shared with the
/// face before it is read once. The installed fonts on macOS 26 read
/// about 13 MiB.
const MAX_INDEX_CMAP_READ: u64 = 64 * MAX_INDEX_CMAP_BYTES as u64;

/// What one coverage build cost.
#[derive(Debug, Default)]
struct BuildStats {
    /// Font files opened.
    opens: usize,
    /// Steps of every face's walk, `cut` when the step budget ran out.
    walked: cmap::Walk,
    /// Bytes of `cmap` tables read from font files.
    cmap_bytes: u64,
    /// Faces whose walk was skipped: their `cmap` is the one the face
    /// before them in the same file read.
    reused: u64,
}

/// The state of one coverage build.
struct Build {
    /// Index into the build's face list per code point; `u32::MAX` =
    /// uncovered. A source can hold a face that comes after a later
    /// source's first face, so a slot takes the lowest index that covers
    /// it, not the first one written: first face wins in database order.
    owner: Vec<u32>,
    stats: BuildStats,
    /// `true` once either budget ran out: no further face is read or
    /// walked.
    stopped: bool,
}

impl Build {
    fn new() -> Self {
        Self {
            owner: vec![u32::MAX; CODE_SPACE],
            stats: BuildStats::default(),
            stopped: false,
        }
    }

    /// Steps left of [`MAX_INDEX_STEPS`].
    fn steps_left(&self) -> u64 {
        MAX_INDEX_STEPS.saturating_sub(self.stats.walked.steps())
    }

    /// Bytes left of [`MAX_INDEX_CMAP_READ`].
    fn bytes_left(&self) -> u64 {
        MAX_INDEX_CMAP_READ.saturating_sub(self.stats.cmap_bytes)
    }

    /// Ends the build at the face at `index`, with one warning.
    fn stop(&mut self, index: u32, budget: &str) {
        if !std::mem::replace(&mut self.stopped, true) {
            tracing::warn!(
                target: "tessera_render",
                face = index,
                budget,
                "the fallback index ran out of its budget; later faces are left out"
            );
        }
    }

    /// Walks the character map `table` of the face at `index` into
    /// [`Self::owner`]: each code point it maps to a glyph goes to `index`
    /// unless a lower index has it. The walk is bounded by the code space
    /// (#689, see [`cmap::enumerate`]) and by the steps left of the
    /// build's budget (#716), which ends the build when the walk runs
    /// out. A walk that panics leaves that face out (but for what it
    /// wrote before), with one warning naming the face's index, instead of
    /// the index thread dying with the index unlanded.
    fn walk_face(&mut self, index: u32, table: &[u8]) {
        if self.stopped {
            return;
        }
        let budget = self.steps_left();
        let owner = &mut self.owner;
        let walk = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            test_read_hook::during_walk(index);
            cmap::enumerate(table, CODE_SPACE_U32, budget, |cp, glyph| {
                if glyph != 0
                    && let Some(slot) = owner.get_mut(cp as usize)
                    && index < *slot
                {
                    *slot = index;
                }
            })
        }));
        let Ok(walk) = walk else {
            tracing::warn!(
                target: "tessera_render",
                face = index,
                "a character map walk panicked; the face is left out of the fallback index"
            );
            return;
        };
        let total = &mut self.stats.walked;
        total.code_points += walk.code_points;
        total.records += walk.records;
        total.cut |= walk.cut;
        if walk.cut {
            self.stop(index, "steps");
        }
    }
}

/// Largest `cmap` table the coverage index reads from a font file: 2 MiB,
/// about six times the largest installed on macOS 26 (Songti's 312 KiB).
/// A face whose `cmap` is larger is left out of the index, with one
/// warning per file (#668); the read buffer never grows past this plus a
/// 28-byte header.
const MAX_INDEX_CMAP_BYTES: u32 = 2 << 20;

/// Bytes in front of a `cmap` in the buffer [`read_face_cmap`] fills: an
/// sfnt header (12) and one table record (16).
const CMAP_FONT_HEADER: usize = 28;

/// What [`read_face_cmap`] found for one face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmapRead {
    /// The buffer holds the face's `cmap` as a one-table font.
    Read,
    /// No `cmap` to index: not a font, no such face, no `cmap` table, or
    /// a read that failed or came back short (the file changed).
    Missing,
    /// The `cmap` is this many bytes, more than [`MAX_INDEX_CMAP_BYTES`];
    /// nothing was read.
    TooLarge(u32),
    /// The buffer already holds the face's `cmap`: the face before it read
    /// the same file range.
    Reused,
    /// The `cmap` is more bytes than the build may still read; nothing
    /// was read.
    OverBudget,
}

/// The faces `faces` (index into the coverage build's face list, face
/// index in the file) of the font file at `path`, each walked into
/// `build` from its `cmap` table, unless that is the `cmap` the face
/// before it read (#716). The file is opened once and read,
/// never mapped (#668); a face that cannot be read is skipped, and so is
/// the whole path when it is no longer a regular file (#669, see
/// [`open_regular_file`]).
fn index_file(path: &std::path::Path, faces: impl Iterator<Item = (u32, u32)>, build: &mut Build) {
    let file = match open_regular_file(path) {
        Ok(file) => file,
        Err(error) => {
            tracing::debug!(
                target: "tessera_render",
                path = %path.display(),
                %error,
                "a font file did not open for the coverage index"
            );
            return;
        }
    };
    let mut cmap = Vec::new();
    let mut loaded = None;
    let mut warned = false;
    for (face, face_index) in faces {
        if build.stopped {
            break;
        }
        #[cfg(test)]
        test_read_hook::before_face(path);
        let budget = build.bytes_left();
        match read_face_cmap(&file, face_index, budget, &mut cmap, &mut loaded) {
            CmapRead::Read => {
                if let Some(table) = cmap.get(CMAP_FONT_HEADER..) {
                    build.stats.cmap_bytes += table.len() as u64;
                    build.walk_face(face, table);
                }
            }
            CmapRead::Reused => build.stats.reused += 1,
            CmapRead::OverBudget => build.stop(face, "cmap bytes"),
            CmapRead::Missing => {}
            CmapRead::TooLarge(len) => {
                if !std::mem::replace(&mut warned, true) {
                    tracing::warn!(
                        target: "tessera_render",
                        path = %path.display(),
                        cmap_bytes = len,
                        cap = MAX_INDEX_CMAP_BYTES,
                        "a font's character map is too large for the fallback index; its faces are left out"
                    );
                }
            }
        }
    }
}

/// Opens the font file at `path` for reading, refusing anything that is
/// not a regular file without blocking on it (#669).
///
/// The path was a font when fontdb scanned it, but another process may
/// have put a FIFO, socket or device there since. A plain open of a FIFO
/// waits for a writer, forever if none comes, and would leave the index
/// thread blocked and the index unlanded. So the open is non-blocking
/// (`O_NONBLOCK`; `O_NOCTTY` so a terminal device never becomes the
/// controlling terminal), and the descriptor, not the path, is checked
/// with `fstat`, so a swap between the check and the open cannot slip
/// through. A regular file reads the same with `O_NONBLOCK` set. Opening
/// a socket fails by itself.
pub(crate) fn open_regular_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    let kind = file.metadata()?.file_type();
    if kind.is_file() {
        return Ok(file);
    }
    let kind = if kind.is_fifo() {
        "a FIFO"
    } else if kind.is_char_device() || kind.is_block_device() {
        "a device"
    } else if kind.is_dir() {
        "a directory"
    } else {
        "not a regular file"
    };
    Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, kind))
}

/// Reads face `face_index`'s `cmap` table from `file` into `buf`, laid out
/// as a font with that one table (an sfnt header, one table record, then
/// the table) so swash parses it as it would the whole file: offsets
/// inside a `cmap` are relative to its start. `loaded` names the table
/// directory (its offset in the file) that led to the `cmap` `buf` holds
/// and that `cmap`'s file range (offset, length), so faces of a
/// collection that share one `cmap` read it once ([`CmapRead::Reused`]),
/// and faces that share one table directory read only its offset in the
/// collection header (#716). Only the collection header, the face's
/// table directory records the binary search visits and the `cmap` are
/// read; the `cmap` at most [`MAX_INDEX_CMAP_BYTES`], and only when it is
/// at most `budget` bytes.
fn read_face_cmap(
    file: &std::fs::File,
    face_index: u32,
    budget: u64,
    buf: &mut Vec<u8>,
    loaded: &mut Option<(u32, u32, u32)>,
) -> CmapRead {
    const TTCF: u32 = u32::from_be_bytes(*b"ttcf");
    const CMAP: u32 = u32::from_be_bytes(*b"cmap");
    let Some((base, start, len)) = (|| {
        // swash's `raw_data::offset`: a single font is at 0 whatever the
        // index below its count of one.
        let head = read_u32s::<3>(file, 0)?;
        let base = if is_sfnt(head[0]) {
            (face_index == 0).then_some(0)?
        } else if head[0] == TTCF && face_index < head[2] {
            read_u32s::<1>(file, 12 + 4 * u64::from(face_index))?[0]
        } else {
            return None;
        };
        if let Some(same @ (directory, _, _)) = *loaded
            && directory == base
        {
            return Some(same);
        }
        let [tag, tables, _] = read_u32s::<3>(file, u64::from(base))?;
        if !is_sfnt(tag) {
            return None;
        }
        // swash's `table_range`: a binary search of the records.
        let records = u64::from(base) + 12;
        let (mut low, mut high) = (0, u64::from(tables >> 16));
        while low < high {
            let mid = u64::midpoint(low, high);
            let [record_tag, _, start, len] = read_u32s::<4>(file, records + 16 * mid)?;
            match CMAP.cmp(&record_tag) {
                std::cmp::Ordering::Less => high = mid,
                std::cmp::Ordering::Greater => low = mid + 1,
                std::cmp::Ordering::Equal => {
                    start.checked_add(len)?;
                    // A table at 0 is no table to swash.
                    return (start != 0).then_some((base, start, len));
                }
            }
        }
        None
    })() else {
        return CmapRead::Missing;
    };
    if len > MAX_INDEX_CMAP_BYTES {
        return CmapRead::TooLarge(len);
    }
    if loaded.is_some_and(|(_, at, size)| (at, size) == (start, len)) {
        *loaded = Some((base, start, len));
        return CmapRead::Reused;
    }
    if u64::from(len) > budget {
        return CmapRead::OverBudget;
    }
    *loaded = None;
    // `len` is at most MAX_INDEX_CMAP_BYTES, so this fits usize.
    let needed = CMAP_FONT_HEADER + len as usize;
    if buf.capacity() < needed {
        // Free the smaller table first: growing in place would hold both
        // while it copies.
        *buf = Vec::new();
        buf.reserve_exact(needed);
    }
    buf.clear();
    buf.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
    buf.extend_from_slice(&1_u16.to_be_bytes());
    buf.extend_from_slice(&[0; 6]);
    buf.extend_from_slice(&CMAP.to_be_bytes());
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(&u32::try_from(CMAP_FONT_HEADER).unwrap_or(0).to_be_bytes());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.resize(needed, 0);
    if read_exact_at(file, u64::from(start), &mut buf[CMAP_FONT_HEADER..]).is_err() {
        return CmapRead::Missing;
    }
    *loaded = Some((base, start, len));
    CmapRead::Read
}

/// Whether `tag` opens an sfnt table directory (swash's `is_font`).
pub(crate) fn is_sfnt(tag: u32) -> bool {
    matches!(&tag.to_be_bytes(), [0, 1, 0, 0] | b"OTTO" | b"true")
}

/// `N` big-endian `u32`s read from `file` at `offset`; `None` when the
/// read fails or the file ends first.
pub(crate) fn read_u32s<const N: usize>(file: &std::fs::File, offset: u64) -> Option<[u32; N]> {
    let mut bytes = [0; 16];
    let bytes = bytes.get_mut(..4 * N)?;
    read_exact_at(file, offset, bytes).ok()?;
    let mut words = [0; N];
    for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*chunk);
    }
    Some(words)
}

/// Fills `out` from `file` at `offset` with `pread(2)`, one call where a
/// seek and a read took two (#716: a collection's faces each read their
/// table directory); a file that ends first is an error.
pub(crate) fn read_exact_at(
    file: &std::fs::File,
    offset: u64,
    out: &mut [u8],
) -> std::io::Result<()> {
    #[cfg(test)]
    test_read_hook::count_read();
    std::os::unix::fs::FileExt::read_exact_at(file, out, offset)
}

/// Test-only hook the coverage build calls before it reads each face of
/// a font file, on the building thread (#668: a test truncates the file
/// there).
#[cfg(test)]
mod test_read_hook {
    use std::cell::RefCell;
    use std::path::Path;

    type Hook = Box<dyn FnMut(&Path)>;
    type WalkHook = Box<dyn FnMut(u32)>;

    thread_local! {
        static BEFORE_FACE: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static DURING_WALK: RefCell<Option<WalkHook>> = const { RefCell::new(None) };
        static READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Counts one `pread(2)` of a font file on this thread.
    pub(super) fn count_read() {
        READS.with(|reads| reads.set(reads.get() + 1));
    }

    /// The font file reads on this thread so far (#716).
    pub(super) fn reads() -> u64 {
        READS.with(std::cell::Cell::get)
    }

    /// Runs `hook` with the face's index in the build at the start of each
    /// character map walk on this thread, inside the walk's
    /// `catch_unwind`, until the next call; `None` removes it.
    pub(super) fn set_during_walk(hook: Option<WalkHook>) {
        DURING_WALK.with(|slot| *slot.borrow_mut() = hook);
    }

    pub(super) fn during_walk(index: u32) {
        DURING_WALK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(index);
            }
        });
    }

    /// Runs `hook` before each face read on this thread until the next
    /// call; `None` removes it.
    pub(super) fn set(hook: Option<Hook>) {
        BEFORE_FACE.with(|slot| *slot.borrow_mut() = hook);
    }

    pub(super) fn before_face(path: &Path) {
        BEFORE_FACE.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(path);
            }
        });
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
///
/// These are the wide (per `unicode-width`) pictographs of the symbol and
/// emoji blocks; text-presentation pictographs such as U+263A or U+1F321
/// are one column wide and stay with the text fonts.
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

    /// A synthetic family: its faces, the queries the "database" answers
    /// with each, and which faces cover the character.
    struct TestFamily {
        faces: Vec<Face<&'static str>>,
        /// `(query, face id)`; a query without a row has no match.
        matches: Vec<(FontStyle, &'static str)>,
        covering: Vec<&'static str>,
        /// Every face whose coverage was asked for, in order.
        asked: Vec<&'static str>,
    }

    impl Family for TestFamily {
        type Id = &'static str;

        fn face(&mut self, query: FontStyle) -> Option<Face<&'static str>> {
            let id = self.matches.iter().find(|(q, _)| *q == query)?.1;
            self.faces.iter().copied().find(|f| f.id == id)
        }

        fn covers(&mut self, picked: &Picked<&'static str>) -> bool {
            self.asked.push(picked.id);
            self.covering.contains(&picked.id)
        }
    }

    const fn test_face(id: &'static str, weight: u16, italic: bool) -> Face<&'static str> {
        Face {
            id,
            italic,
            weight,
            bold_axis: false,
            color: false,
        }
    }

    /// A family with all four faces, as a database matches them.
    fn four_faces(covering: &[&'static str]) -> TestFamily {
        TestFamily {
            faces: vec![
                test_face("regular", 400, false),
                test_face("bold", 700, false),
                test_face("italic", 400, true),
                test_face("bold-italic", 700, true),
            ],
            matches: vec![
                (FontStyle::Regular, "regular"),
                (FontStyle::Bold, "bold"),
                (FontStyle::Italic, "italic"),
                (FontStyle::BoldItalic, "bold-italic"),
            ],
            covering: covering.to_vec(),
            asked: Vec::new(),
        }
    }

    fn picked(id: &'static str, weight: u16, bold: bool, italic: bool) -> Picked<&'static str> {
        Picked {
            id,
            weight,
            synthetic_bold: bold,
            synthetic_italic: italic,
        }
    }

    #[test]
    fn pick_takes_the_styled_face_when_it_covers() {
        let all = ["regular", "bold", "italic", "bold-italic"];
        let mut family = four_faces(&all);
        assert_eq!(
            pick_in_family(&mut family, FontStyle::Regular, STYLED_PASS),
            Some(picked("regular", 400, false, false))
        );
        assert_eq!(
            pick_in_family(&mut family, FontStyle::Bold, STYLED_PASS),
            Some(picked("bold", 700, false, false))
        );
        assert_eq!(
            pick_in_family(&mut family, FontStyle::Italic, STYLED_PASS),
            Some(picked("italic", 400, false, false))
        );
        assert_eq!(
            pick_in_family(&mut family, FontStyle::BoldItalic, STYLED_PASS),
            Some(picked("bold-italic", 700, false, false))
        );
        assert_eq!(family.asked, all, "one coverage test each");
    }

    #[test]
    fn pick_synthesises_italic_on_the_regular_face_when_italic_lacks_it() {
        let mut family = four_faces(&["regular", "bold", "bold-italic"]);
        assert_eq!(
            pick_in_family(&mut family, FontStyle::Italic, STYLED_PASS),
            Some(picked("regular", 400, false, true))
        );
        assert_eq!(family.asked, ["italic", "regular"]);
    }

    /// Bold the bold face lacks is left to the next family's bold face in
    /// the styled pass, and is the regular face with synthetic bold only
    /// in the synthetic-bold pass (#119: real CJK bold from a later family
    /// beats an earlier family's synthetic bold).
    #[test]
    fn pick_synthesises_bold_on_the_regular_face_only_in_the_synthetic_bold_pass() {
        let mut family = four_faces(&["regular", "italic", "bold-italic"]);
        assert_eq!(
            pick_in_family(&mut family, FontStyle::Bold, STYLED_PASS),
            None
        );
        assert_eq!(family.asked, ["bold"]);
        // Drawn at the requested weight, emboldened; the bold face is not
        // tested again.
        assert_eq!(
            pick_in_family(&mut family, FontStyle::Bold, SYNTHETIC_BOLD_PASS),
            Some(picked("regular", 700, true, false))
        );
        assert_eq!(family.asked, ["bold", "regular"]);
    }

    #[test]
    fn pick_prefers_upright_bold_over_regular_for_bold_italic() {
        let mut family = four_faces(&["regular", "bold", "italic"]);
        assert_eq!(
            pick_in_family(&mut family, FontStyle::BoldItalic, STYLED_PASS),
            Some(picked("bold", 700, false, true))
        );
        assert_eq!(family.asked, ["bold-italic", "bold"]);
    }

    #[test]
    fn pick_synthesises_both_on_the_regular_face_for_bold_italic() {
        let mut family = four_faces(&["regular", "italic"]);
        assert_eq!(
            pick_in_family(&mut family, FontStyle::BoldItalic, STYLED_PASS),
            None
        );
        assert_eq!(family.asked, ["bold-italic", "bold"]);
        assert_eq!(
            pick_in_family(&mut family, FontStyle::BoldItalic, SYNTHETIC_BOLD_PASS),
            Some(picked("regular", 700, true, true))
        );
        assert_eq!(family.asked, ["bold-italic", "bold", "regular"]);
    }

    /// Regular and italic need one pass: the regular face with synthetic
    /// italic is part of the styled pass, so a second pass tests nothing.
    #[test]
    fn pick_needs_a_second_pass_for_bold_styles_only() {
        assert_eq!(passes(FontStyle::Regular), 1);
        assert_eq!(passes(FontStyle::Italic), 1);
        assert_eq!(passes(FontStyle::Bold), 2);
        assert_eq!(passes(FontStyle::BoldItalic), 2);
        for style in [FontStyle::Regular, FontStyle::Italic] {
            let mut family = four_faces(&["regular", "bold", "italic", "bold-italic"]);
            assert_eq!(
                pick_in_family(&mut family, style, SYNTHETIC_BOLD_PASS),
                None,
                "{style:?}"
            );
            assert_eq!(family.asked, [] as [&str; 0], "{style:?}");
        }
    }

    #[test]
    fn pick_returns_none_when_no_face_of_the_family_covers() {
        for style in [
            FontStyle::Regular,
            FontStyle::Bold,
            FontStyle::Italic,
            FontStyle::BoldItalic,
        ] {
            for pass in 0..passes(style) {
                let mut family = four_faces(&[]);
                assert_eq!(pick_in_family(&mut family, style, pass), None, "{style:?}");
            }
        }
        let mut empty = TestFamily {
            faces: Vec::new(),
            matches: Vec::new(),
            covering: Vec::new(),
            asked: Vec::new(),
        };
        assert_eq!(
            pick_in_family(&mut empty, FontStyle::BoldItalic, STYLED_PASS),
            None
        );
        assert_eq!(empty.asked, [] as [&str; 0], "nothing to test");
    }

    /// A database answers every query of a one-face family with that face;
    /// it is tested once, and drawn with the synthesis each style needs.
    #[test]
    fn pick_tests_a_face_matched_twice_once() {
        let one_face = |covering: &[&'static str]| TestFamily {
            faces: vec![test_face("only", 400, false)],
            matches: [
                FontStyle::Regular,
                FontStyle::Bold,
                FontStyle::Italic,
                FontStyle::BoldItalic,
            ]
            .map(|q| (q, "only"))
            .to_vec(),
            covering: covering.to_vec(),
            asked: Vec::new(),
        };
        let mut family = one_face(&[]);
        assert_eq!(
            pick_in_family(&mut family, FontStyle::BoldItalic, STYLED_PASS),
            None
        );
        assert_eq!(family.asked, ["only"]);
        // The regular query matches the same face: not tested again.
        assert_eq!(
            pick_in_family(&mut family, FontStyle::BoldItalic, SYNTHETIC_BOLD_PASS),
            None
        );
        assert_eq!(family.asked, ["only"]);
        let mut family = one_face(&["only"]);
        assert_eq!(
            pick_in_family(&mut family, FontStyle::BoldItalic, STYLED_PASS),
            Some(picked("only", 700, true, true))
        );
    }

    #[test]
    fn pick_draws_a_colour_family_as_is() {
        let emoji = Face {
            color: true,
            ..test_face("emoji", 400, false)
        };
        for style in [
            FontStyle::Regular,
            FontStyle::Bold,
            FontStyle::Italic,
            FontStyle::BoldItalic,
        ] {
            let mut family = TestFamily {
                faces: vec![emoji],
                matches: vec![
                    (style, "emoji"),
                    (FontStyle::Bold, "emoji"),
                    (FontStyle::Regular, "emoji"),
                ],
                covering: vec!["emoji"],
                asked: Vec::new(),
            };
            assert_eq!(
                pick_in_family(&mut family, style, STYLED_PASS),
                Some(picked("emoji", 400, false, false)),
                "{style:?}"
            );
        }
    }

    #[test]
    fn pick_uses_a_wght_axis_instead_of_synthetic_bold() {
        let variable = Face {
            bold_axis: true,
            ..test_face("variable", 400, false)
        };
        let mut family = TestFamily {
            faces: vec![variable],
            matches: vec![(FontStyle::Bold, "variable")],
            covering: vec!["variable"],
            asked: Vec::new(),
        };
        assert_eq!(
            pick_in_family(&mut family, FontStyle::Bold, STYLED_PASS),
            Some(picked("variable", 700, false, false))
        );
    }

    /// #526 (24.6 m1): bold on a real variable family comes from its
    /// `wght` axis, not from synthetic bold. The premise (no bold face, an
    /// axis reaching bold) is read from the font itself, so it does not
    /// depend on the code under test.
    #[test]
    fn bold_on_an_installed_variable_family_is_not_synthetic() {
        let mut checked = 0;
        for family in [".SF NS Mono", "System Font"] {
            let config = FontConfig {
                family: family.into(),
                size_pt: 13.0,
                scale: 1.0,
                fallback: Vec::new(),
            };
            let Ok(fonts) = Fonts::with_system(FontSystem::new(), &config, IndexBuild::Never)
            else {
                eprintln!("skipping {family}: it does not load");
                continue;
            };
            if fonts.family() != family {
                eprintln!("skipping {family}: not installed");
                continue;
            }
            let mut inner = fonts.lock();
            let face = inner.family_face(0, FontStyle::Bold).expect("a face");
            let own = FaceId::font(face.id, fontdb::Weight(face.weight));
            let axis_reaches_bold = inner.load(own).is_some_and(|font| {
                font.as_swash()
                    .variations()
                    .find_by_tag(swash::tag_from_bytes(b"wght"))
                    .is_some_and(|axis| axis.max_value() >= f32::from(BOLD_THRESHOLD))
            });
            if face.weight >= BOLD_THRESHOLD || !axis_reaches_bold {
                eprintln!("skipping {family}: not a variable family without a bold face");
                continue;
            }
            let bold = inner.resolve('a', FontStyle::Bold);
            assert_eq!(
                bold.face,
                FaceId::font(face.id, fontdb::Weight::BOLD),
                "{family}"
            );
            assert!(!bold.synthetic_bold, "{family}: {bold:?}");
            checked += 1;
        }
        if checked == 0 {
            eprintln!("skipping: no variable family is installed");
        }
    }

    /// #526 (24.6 m2): an oblique face is already slanted, so italic text
    /// drawn with it gets no synthetic italic. No face installed on macOS
    /// is classified oblique by `fontdb`, so Menlo Regular's data is
    /// registered as one, in a family of its own.
    #[test]
    fn an_oblique_face_is_not_slanted_again() {
        let fonts = menlo_without_index();
        if fonts.family() != DEFAULT_FAMILY {
            eprintln!("skipping: Menlo is not installed");
            return;
        }
        let mut inner = fonts.lock();
        let regular = query_family(inner.system.db(), DEFAULT_FAMILY, FontStyle::Regular)
            .expect("Menlo Regular");
        let mut info = inner.system.db().face(regular).expect("face").clone();
        info.families = vec![(
            "Test Oblique".to_owned(),
            fontdb::Language::English_UnitedStates,
        )];
        info.style = fontdb::Style::Oblique;
        let oblique = inner.system.db_mut().push_face_info(info);
        assert_eq!(
            query_family(inner.system.db(), "Test Oblique", FontStyle::Italic),
            Some(oblique),
            "the italic query finds the oblique face"
        );
        assert!(inner.face_traits(oblique).expect("traits").italic);
        let drawn = inner
            .with_synthesis(oblique, FontStyle::Italic)
            .expect("resolved");
        assert!(
            !drawn.synthetic_italic && !drawn.synthetic_bold,
            "{drawn:?}"
        );
    }

    /// Family name, weight and slant of `face`.
    fn describe(inner: &Inner, face: FaceId) -> Option<(String, u16, fontdb::Style)> {
        let FaceRepr::Font { id, .. } = face.0 else {
            return None;
        };
        let info = inner.system.db().face(id)?;
        Some((info.families.first()?.0.clone(), info.weight.0, info.style))
    }

    /// Whether `family`'s face matched for `style` covers `ch`, or `None`
    /// when the match is not a face of that exact weight and slant.
    fn own_face_covers(
        inner: &mut Inner,
        family: &str,
        style: FontStyle,
        ch: char,
    ) -> Option<bool> {
        let index = inner.families.iter().position(|f| f == family)?;
        let face = inner.family_face(index, style)?;
        let exact =
            (face.weight >= BOLD_THRESHOLD) == style.is_bold() && face.italic == style.is_italic();
        exact.then(|| inner.covers(FaceId::font(face.id, fontdb::Weight(face.weight)), ch))
    }

    /// #119: Menlo Italic lacks U+01DE but Menlo Regular has it, so italic
    /// U+01DE is Menlo Regular with synthetic italic, not the next family
    /// that covers it. Runs without the coverage index: the fixed chain
    /// alone must find it.
    #[test]
    fn italic_missing_from_the_styled_face_stays_in_the_family() {
        let fonts = menlo_without_index();
        if fonts.family() != DEFAULT_FAMILY {
            eprintln!(
                "skipping: Menlo is not installed (primary is {})",
                fonts.family()
            );
            return;
        }
        let ch = '\u{01DE}';
        let mut inner = fonts.lock();
        let regular = own_face_covers(&mut inner, "Menlo", FontStyle::Regular, ch);
        let italic = own_face_covers(&mut inner, "Menlo", FontStyle::Italic, ch);
        assert_eq!(
            (regular, italic),
            (Some(true), Some(false)),
            "the premise of #119: Menlo Regular has U+01DE, Menlo Italic does not"
        );

        let r = inner.resolve(ch, FontStyle::Italic);
        assert_eq!(
            describe(&inner, r.face),
            Some(("Menlo".to_owned(), 400, fontdb::Style::Normal)),
            "italic U+01DE: {r:?}"
        );
        assert!(r.synthetic_italic && !r.synthetic_bold, "{r:?}");

        // Bold: Menlo Bold when it has the character. (When it lacks it,
        // a later family's bold face comes first: see
        // `bold_a_bold_face_lacks_is_a_later_familys_real_bold`.)
        let bold_has = own_face_covers(&mut inner, "Menlo", FontStyle::Bold, ch);
        let r = inner.resolve(ch, FontStyle::Bold);
        if bold_has == Some(true) {
            assert_eq!(
                describe(&inner, r.face),
                Some(("Menlo".to_owned(), 700, fontdb::Style::Normal)),
                "bold U+01DE: Menlo Bold has it: {r:?}"
            );
            assert!(!r.synthetic_bold && !r.synthetic_italic, "{r:?}");
        } else {
            eprintln!("Menlo Bold lacks U+01DE here: bold not checked");
        }

        // Bold italic: Menlo Bold Italic, else the upright Menlo Bold with
        // synthetic italic, when either has the character.
        let bold_italic_has = own_face_covers(&mut inner, "Menlo", FontStyle::BoldItalic, ch);
        let r = inner.resolve(ch, FontStyle::BoldItalic);
        let described = describe(&inner, r.face);
        match (bold_italic_has, bold_has) {
            (Some(true), _) => {
                assert_eq!(
                    described,
                    Some(("Menlo".to_owned(), 700, fontdb::Style::Italic)),
                    "{r:?}"
                );
                assert!(!r.synthetic_bold && !r.synthetic_italic, "{r:?}");
            }
            (_, Some(true)) => {
                assert_eq!(
                    described,
                    Some(("Menlo".to_owned(), 700, fontdb::Style::Normal)),
                    "{r:?}"
                );
                assert!(!r.synthetic_bold && r.synthetic_italic, "{r:?}");
            }
            _ => eprintln!("Menlo's bold faces lack U+01DE here: bold italic not checked"),
        }
    }

    /// #119 critic B1: Hiragino Sans Bold lacks 类 but Hiragino Sans
    /// Regular has it. Bold 类 is a later family's real bold (Hiragino Sans
    /// GB W6, as before #119), not Hiragino Sans Regular with synthetic
    /// bold; 型, which Hiragino Sans Bold has, stays Hiragino Sans.
    #[test]
    fn bold_a_bold_face_lacks_is_a_later_familys_real_bold() {
        let fonts = menlo_without_index();
        let mut inner = fonts.lock();
        let (lei, xing) = ('\u{7C7B}', '\u{578B}');
        let premise = (
            own_face_covers(&mut inner, "Hiragino Sans", FontStyle::Regular, lei),
            own_face_covers(&mut inner, "Hiragino Sans", FontStyle::Bold, lei),
            own_face_covers(&mut inner, "Hiragino Sans GB", FontStyle::Bold, lei),
            own_face_covers(&mut inner, "Hiragino Sans", FontStyle::Bold, xing),
        );
        if premise != (Some(true), Some(false), Some(true), Some(true)) {
            eprintln!(
                "skipping: Hiragino Sans and Hiragino Sans GB coverage differs here: {premise:?}"
            );
            return;
        }
        let hiragino_sans = inner.families.iter().position(|f| f == "Hiragino Sans");
        let hiragino_gb = inner.families.iter().position(|f| f == "Hiragino Sans GB");
        assert!(
            hiragino_sans < hiragino_gb,
            "the premise: Hiragino Sans comes first"
        );
        for style in [FontStyle::Bold, FontStyle::BoldItalic] {
            let r = inner.resolve(lei, style);
            let (family, weight, slant) = describe(&inner, r.face).expect("a font face");
            assert_eq!(family, "Hiragino Sans GB", "{style:?} 类: {r:?}");
            assert!(weight >= BOLD_THRESHOLD, "{style:?} 类: {r:?}");
            assert_eq!(slant, fontdb::Style::Normal, "{style:?} 类: {r:?}");
            assert!(!r.synthetic_bold, "{style:?} 类: {r:?}");
            assert_eq!(r.synthetic_italic, style.is_italic(), "{style:?} 类: {r:?}");

            let r = inner.resolve(xing, style);
            let (family, weight, _) = describe(&inner, r.face).expect("a font face");
            assert_eq!(family, "Hiragino Sans", "{style:?} 型: {r:?}");
            assert!(
                weight >= BOLD_THRESHOLD && !r.synthetic_bold,
                "{style:?} 型: {r:?}"
            );
        }
    }

    /// A real bold found through the coverage index beats the chain's
    /// synthetic bold: no chain family's bold face has U+24F5, Hiragino
    /// Sans Regular does, and Hiragino Mincho `ProN` W6 (found only through
    /// the index) has a real bold one. A synthetic bold resolved before
    /// the index is ready is not cached, so it gives way once it is.
    #[test]
    fn bold_found_through_the_index_beats_the_chains_synthetic_bold() {
        let fonts = menlo_without_index();
        let ch = '\u{24F5}';
        let before = fonts.lock().resolve(ch, FontStyle::Bold);
        if !before.synthetic_bold {
            eprintln!("skipping: a chain family has a bold U+24F5 here: {before:?}");
            return;
        }
        let index = Coverage::build(fonts.lock().system.db());
        let _ = fonts.lock().coverage.set(index);
        let mut inner = fonts.lock();
        let found = inner
            .coverage
            .get()
            .and_then(|index| index.lookup(ch))
            .and_then(|id| inner.system.db().face(id))
            .and_then(|f| f.families.first().map(|(n, _)| n.clone()));
        let Some(family) = found else {
            eprintln!("skipping: the index has no face for U+24F5");
            return;
        };
        let bold = query_family(inner.system.db(), &family, FontStyle::Bold)
            .and_then(|id| inner.face_traits(id))
            .filter(|face| face.weight >= BOLD_THRESHOLD);
        let bold_covers = bold.is_some_and(|face| {
            inner.covers(FaceId::font(face.id, fontdb::Weight(face.weight)), ch)
        });
        if !bold_covers {
            eprintln!("skipping: {family}, found through the index, has no bold U+24F5");
            return;
        }
        for style in [FontStyle::Bold, FontStyle::BoldItalic] {
            let r = inner.resolve(ch, style);
            let (drawn, weight, _) = describe(&inner, r.face).expect("a font face");
            assert_eq!(
                drawn, family,
                "{style:?}: {r:?} (before the index: {before:?})"
            );
            assert!(
                weight >= BOLD_THRESHOLD && !r.synthetic_bold,
                "{style:?}: {r:?}"
            );
        }
    }

    /// A character no family's bold face has is the earliest covering
    /// family's regular face with synthetic bold. Menlo Bold lacks the box
    /// drawing block, Menlo Regular has it; with the chain cut to Menlo
    /// alone, no family has a bold U+2500.
    #[test]
    fn bold_no_bold_face_has_is_the_regular_face_with_synthetic_bold() {
        let fonts = menlo_without_index();
        if fonts.family() != DEFAULT_FAMILY {
            eprintln!(
                "skipping: Menlo is not installed (primary is {})",
                fonts.family()
            );
            return;
        }
        let ch = '\u{2500}';
        let mut inner = fonts.lock();
        let premise = (
            own_face_covers(&mut inner, "Menlo", FontStyle::Regular, ch),
            own_face_covers(&mut inner, "Menlo", FontStyle::Bold, ch),
        );
        if premise != (Some(true), Some(false)) {
            eprintln!("skipping: Menlo Bold has U+2500 here or Regular lacks it: {premise:?}");
            return;
        }
        inner.chain = Chain::default();
        for (style, italic) in [(FontStyle::Bold, false), (FontStyle::BoldItalic, true)] {
            let r = inner.resolve(ch, style);
            assert_eq!(
                describe(&inner, r.face),
                Some(("Menlo".to_owned(), 400, fontdb::Style::Normal)),
                "{style:?} U+2500: {r:?}"
            );
            assert!(
                matches!(r.face.0, FaceRepr::Font { weight: 700, .. }),
                "drawn at the bold weight: {r:?}"
            );
            assert!(r.synthetic_bold, "{style:?}: {r:?}");
            assert_eq!(r.synthetic_italic, italic, "{style:?}: {r:?}");
        }
    }

    /// An emoji keeps its colour face in every style, with no synthesis:
    /// the in-family step never reaches a text family's regular face for
    /// it, and colour faces are never synthesised.
    #[test]
    fn italic_emoji_is_the_colour_face_without_synthesis() {
        let fonts = menlo_without_index();
        if !fonts.has_family(EMOJI_FAMILY) {
            eprintln!("skipping: {EMOJI_FAMILY} is not installed");
            return;
        }
        let mut inner = fonts.lock();
        for ch in ['\u{1F600}', '\u{231A}', '\u{1F44D}'] {
            let regular = inner.resolve(ch, FontStyle::Regular);
            assert!(inner.is_color(regular.face), "{ch:?}: {regular:?}");
            for style in [FontStyle::Italic, FontStyle::Bold, FontStyle::BoldItalic] {
                let r = inner.resolve(ch, style);
                assert_eq!(r, regular, "{ch:?} in {style:?}");
                assert!(
                    !r.synthetic_bold && !r.synthetic_italic,
                    "{ch:?} in {style:?}"
                );
            }
        }
    }

    /// #594: the notice of [`Fonts::with_index_notice`] runs once, after
    /// the index landed; a second landing of the same slot calls nothing.
    #[test]
    fn the_index_notice_runs_once_after_the_index_lands() {
        use std::sync::atomic::AtomicUsize;

        let calls = Arc::new(AtomicUsize::new(0));
        let ready_when_called = Arc::new(Mutex::new(None));
        let (tx, rx) = std::sync::mpsc::channel();
        let fonts = {
            let (calls, ready) = (Arc::clone(&calls), Arc::clone(&ready_when_called));
            let slot = Arc::new(Mutex::new(None::<Fonts>));
            let probe = Arc::clone(&slot);
            let fonts = Fonts::with_index_notice(&FontConfig::default(), move || {
                calls.fetch_add(1, Ordering::SeqCst);
                *ready.lock().unwrap() = probe
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(Fonts::fallback_index_ready);
                let _ = tx.send(());
            })
            .expect("fonts");
            *slot.lock().unwrap() = Some(fonts.clone());
            fonts
        };
        rx.recv_timeout(std::time::Duration::from_secs(60))
            .expect("the notice never ran");
        assert!(fonts.fallback_index_ready());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Landing the slot again (as `build_index_now` would) is refused
        // and calls no notice.
        let notice: IndexNotice = Box::new({
            let calls = Arc::clone(&calls);
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        });
        land(
            &fonts.coverage,
            Coverage::default(),
            &Mutex::new(Some(notice)),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one call per landing");
        // The notice may run before the test stores its handle; when it
        // did see one, the index was ready already.
        assert_ne!(*ready_when_called.lock().unwrap(), Some(false));
    }

    /// #555: `resolve_presented` with text presentation (VS15) of a
    /// character that prefers emoji and that no chain family covers
    /// resolves provisionally before the index lands (the count moves) and
    /// for good after (it does not).
    #[test]
    fn a_text_presented_emoji_resolution_is_provisional_until_the_index_lands() {
        let fonts = menlo_without_index();
        let ch = '\u{1F210}';
        assert!(prefers_emoji(ch));
        let (before, first) = {
            let mut inner = fonts.lock();
            let before = inner.provisional();
            let r = inner.resolve_presented(ch, FontStyle::Regular, false);
            (inner.provisional().wrapping_sub(before), r)
        };
        assert_eq!(before, 1, "provisional before the index: {first:?}");
        fonts.build_index_now();
        let mut inner = fonts.lock();
        let count = inner.provisional();
        let _ = inner.resolve_presented(ch, FontStyle::Regular, false);
        assert_eq!(
            inner.provisional(),
            count,
            "settled once the index is ready"
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

    /// The per-face build #660 replaced: one `with_face_data` call, so one
    /// map of a file-backed face, per face, each slot taken by the first
    /// face written to it. The reference the grouped build must match.
    fn per_face_reference(db: &fontdb::Database) -> Vec<(u32, u32, fontdb::ID)> {
        let mut faces = Vec::new();
        let mut owner = vec![u32::MAX; CODE_SPACE];
        for info in db.faces() {
            if info.families.iter().any(|(name, _)| {
                FORBIDDEN_FAMILIES
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(name))
            }) {
                continue;
            }
            let index = u32::try_from(faces.len()).expect("fewer than 2^32 faces");
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
            let (face, start) = (owner[cp], cp);
            while cp < owner.len() && owner[cp] == face {
                cp += 1;
            }
            if face != u32::MAX {
                let first = u32::try_from(start).expect("below CODE_SPACE");
                let last = u32::try_from(cp - 1).expect("below CODE_SPACE");
                ranges.push((first, last, faces[face as usize]));
            }
        }
        ranges
    }

    /// The distinct files behind `db`'s file-backed, non-forbidden faces,
    /// and how many such faces there are.
    fn file_backed(db: &fontdb::Database) -> (usize, usize) {
        let mut paths = std::collections::HashSet::new();
        let mut faces = 0;
        for info in db.faces() {
            let forbidden = info.families.iter().any(|(name, _)| {
                FORBIDDEN_FAMILIES
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(name))
            });
            if let (false, fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _)) =
                (forbidden, &info.source)
            {
                paths.insert(path.clone());
                faces += 1;
            }
        }
        (paths.len(), faces)
    }

    /// #660: one build of the index over the installed fonts opens each
    /// font file once (#668: and reads it, where #660 mapped it once),
    /// however many faces a collection (`.ttc`) holds, and lands the same
    /// ranges, in the same first-face-wins order, as the per-face build it
    /// replaced, which read each whole face through a map; #716: well
    /// inside the whole-build budgets.
    #[test]
    fn one_index_build_opens_each_font_file_once() {
        let system = FontSystem::new();
        let db = system.db();
        let (files, faces) = file_backed(db);
        assert!(
            files < faces,
            "the installed fonts include a collection ({files} files, {faces} faces)"
        );
        let (index, stats) = Coverage::build_with_stats(db);
        eprintln!("{faces} faces in {files} files: {stats:?}");
        assert_eq!(stats.opens, files, "one open per font file ({faces} faces)");
        // #716: the whole-build budgets leave the installed fonts whole,
        // with room to spare.
        assert!(!stats.walked.cut, "{stats:?}");
        assert!(stats.walked.steps() < MAX_INDEX_STEPS / 8, "{stats:?}");
        assert!(stats.cmap_bytes < MAX_INDEX_CMAP_READ / 8, "{stats:?}");
        assert_eq!(index.ranges, per_face_reference(db));
    }

    /// A collection whose faces are not adjacent in database order: a face
    /// removed from it leaves a slot that a later file's face fills, so
    /// that face comes between the collection's first face and the rest.
    /// It still wins over those later faces of the collection, as in the
    /// per-face build; in-memory fonts are read too, without opening a
    /// file.
    #[test]
    fn a_split_collection_keeps_first_face_wins_order() {
        const INDIA: &str = "/System/Library/Fonts/SFIndia.ttc";
        const ARIAL: &str = "/System/Library/Fonts/Supplemental/Arial Unicode.ttf";
        const MENLO: &str = "/System/Library/Fonts/Menlo.ttc";
        let mut db = fontdb::Database::new();
        db.load_font_file(INDIA).expect("SFIndia.ttc is installed");
        let india: Vec<fontdb::ID> = db.faces().map(|f| f.id).collect();
        assert!(india.len() > 2, "SFIndia.ttc holds one face per script");
        // Its Devanagari face; Arial Unicode takes its slot.
        db.remove_face(india[1]);
        db.load_font_file(ARIAL)
            .expect("Arial Unicode is installed");
        db.load_font_data(std::fs::read(MENLO).expect("Menlo.ttc is installed"));
        let order: Vec<String> = db.faces().map(|f| f.post_script_name.clone()).collect();
        assert_eq!(
            order.get(1).map(String::as_str),
            Some("ArialUnicodeMS"),
            "{order:?}"
        );
        assert!(
            matches!(
                db.faces().last().map(|f| &f.source),
                Some(fontdb::Source::Binary(_))
            ),
            "{order:?}"
        );

        let (index, BuildStats { opens, .. }) = Coverage::build_with_stats(&db);
        assert_eq!(opens, 2, "SFIndia.ttc and Arial Unicode, once each");
        let reference = per_face_reference(&db);
        assert_eq!(index.ranges, reference);
        // The split matters: Arial Unicode owns Tamil (U+0B95), which a
        // later SFIndia face also covers.
        let arial = db.faces().nth(1).map(|f| f.id);
        assert_eq!(index.lookup('\u{0B95}'), arial);
        assert_eq!(
            index.lookup('\u{0995}'),
            Some(india[0]),
            "Bengali stays SFIndia's first face"
        );
    }
    /// A fresh directory for one test's font copies (`tessera-font-<name>-*`
    /// in the temporary directory), removed when the guard drops: when the
    /// test passes and when it fails.
    fn font_scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("tessera-font-{name}-"))
            .tempdir()
            .expect("temp dir")
    }

    /// Truncates the file at `path` in place to `len` bytes.
    fn truncate(path: &std::path::Path, len: u64) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.set_len(len))
            .expect("truncate the font copy");
    }

    /// #668: a font file truncated in place while the index reads it
    /// cannot kill the process. The hook truncates a copy of Menlo.ttc
    /// after the file is opened and before its first face is read: a map
    /// taken then would raise SIGBUS on the first page read. Its faces are
    /// left out and the next file's face takes their characters.
    #[test]
    fn a_font_file_truncated_during_the_index_build_is_skipped() {
        const MENLO: &str = "/System/Library/Fonts/Menlo.ttc";
        const MONACO: &str = "/System/Library/Fonts/Monaco.ttf";
        let temp = font_scratch("truncated");
        let dir = temp.path();
        let copy = dir.join("Menlo.ttc");
        std::fs::copy(MENLO, &copy).expect("Menlo.ttc is installed");
        let mut db = fontdb::Database::new();
        db.load_font_file(&copy).expect("the copy loads");
        db.load_font_file(MONACO).expect("Monaco is installed");
        let monaco = db.faces().last().map(|f| f.id);
        let target = copy.clone();
        test_read_hook::set(Some(Box::new(move |path| {
            if path == target {
                truncate(path, 0);
            }
        })));
        let (index, BuildStats { opens, .. }) = Coverage::build_with_stats(&db);
        test_read_hook::set(None);
        assert_eq!(opens, 2);
        assert_eq!(index.lookup('A'), monaco, "Menlo's faces are left out");
        assert!(
            index.ranges.iter().all(|&(_, _, id)| Some(id) == monaco),
            "{:?}",
            index.ranges
        );
    }

    /// #668: a collection cut short or rewritten with another font's bytes
    /// between two of its faces: the faces read before stay indexed, the
    /// rest index whatever the file holds then, and nothing crashes.
    #[test]
    fn a_collection_rewritten_between_its_faces_is_read_as_it_is_then() {
        const MENLO: &str = "/System/Library/Fonts/Menlo.ttc";
        const MONACO: &str = "/System/Library/Fonts/Monaco.ttf";
        let monaco_bytes = std::fs::read(MONACO).expect("Monaco is installed");
        for rewrite in [false, true] {
            let temp = font_scratch(if rewrite { "rewritten" } else { "cut" });
            let dir = temp.path();
            let copy = dir.join("Menlo.ttc");
            std::fs::copy(MENLO, &copy).expect("Menlo.ttc is installed");
            let mut db = fontdb::Database::new();
            db.load_font_file(&copy).expect("the copy loads");
            let menlo: Vec<fontdb::ID> = db.faces().map(|f| f.id).collect();
            assert!(menlo.len() > 1, "Menlo.ttc is a collection");
            let mut seen = 0;
            let bytes = monaco_bytes.clone();
            test_read_hook::set(Some(Box::new(move |path| {
                seen += 1;
                if seen == 2 {
                    truncate(path, 0);
                    if rewrite {
                        std::fs::write(path, &bytes).expect("rewrite in place");
                    }
                }
            })));
            let index = Coverage::build(&db);
            test_read_hook::set(None);
            assert_eq!(index.lookup('A'), Some(menlo[0]), "rewrite {rewrite}");
        }
    }

    /// #668: builds while another thread rewrites the font file in place
    /// (truncate, then write the bytes back) all finish, and a build that
    /// read the whole file lands what the intact file gives.
    #[test]
    fn index_builds_survive_a_file_rewritten_in_place_meanwhile() {
        const SOURCE: &str = "/System/Library/Fonts/SFIndia.ttc";
        let temp = font_scratch("race");
        let dir = temp.path();
        let copy = dir.join("SFIndia.ttc");
        std::fs::copy(SOURCE, &copy).expect("SFIndia.ttc is installed");
        let bytes = std::fs::read(&copy).expect("read the copy");
        let mut db = fontdb::Database::new();
        db.load_font_file(&copy).expect("the copy loads");
        let intact = Coverage::build(&db).ranges;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (stop, copy) = (Arc::clone(&stop), copy.clone());
            std::thread::spawn(move || {
                let mut rewrites = 0_u32;
                while !stop.load(Ordering::Relaxed) {
                    truncate(&copy, 0);
                    std::fs::write(&copy, &bytes).expect("rewrite in place");
                    rewrites += 1;
                }
                rewrites
            })
        };
        let mut whole = 0;
        for _ in 0..40 {
            let ranges = Coverage::build(&db).ranges;
            if ranges == intact {
                whole += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let rewrites = writer.join().expect("the writer finishes");
        assert!(rewrites > 0, "the file was rewritten during the builds");
        eprintln!("{whole} of 40 builds saw the whole file, {rewrites} rewrites");
    }

    /// Writes a one-table font whose `cmap` record says `len` bytes, the
    /// file extended (sparse) to hold them.
    fn font_with_cmap_len(path: &std::path::Path, len: u32) {
        let mut head = Vec::new();
        head.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
        head.extend_from_slice(&1_u16.to_be_bytes());
        head.extend_from_slice(&[0; 6]);
        head.extend_from_slice(b"cmap");
        head.extend_from_slice(&[0; 4]);
        head.extend_from_slice(&28_u32.to_be_bytes());
        head.extend_from_slice(&len.to_be_bytes());
        std::fs::write(path, &head).expect("write the font");
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.set_len(28 + u64::from(len)))
            .expect("extend the font");
    }

    /// #668, S5: the `cmap` read is capped at `MAX_INDEX_CMAP_BYTES`. A
    /// larger one is refused before anything is allocated; one at the cap
    /// is read into a buffer of the cap plus the header.
    #[test]
    fn the_index_cmap_read_is_capped() {
        let temp = font_scratch("cap");
        let dir = temp.path();
        let path = dir.join("big.ttf");
        for (len, expected) in [
            (
                MAX_INDEX_CMAP_BYTES + 1,
                CmapRead::TooLarge(MAX_INDEX_CMAP_BYTES + 1),
            ),
            (u32::MAX - 28, CmapRead::TooLarge(u32::MAX - 28)),
            (MAX_INDEX_CMAP_BYTES, CmapRead::Read),
        ] {
            let flood = len > MAX_INDEX_CMAP_BYTES;
            // The file holds at most the cap and a byte; the record says
            // `len`.
            font_with_cmap_len(&path, len.min(MAX_INDEX_CMAP_BYTES + 1));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open");
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(24)).expect("seek");
            std::io::Write::write_all(&mut file, &len.to_be_bytes()).expect("write");
            let file = std::fs::File::open(&path).expect("open");
            let (mut buf, mut loaded) = (Vec::new(), None);
            assert_eq!(
                read_face_cmap(&file, 0, u64::MAX, &mut buf, &mut loaded),
                expected,
                "{len}"
            );
            if flood {
                assert_eq!(buf.capacity(), 0, "nothing allocated for {len}");
                assert_eq!(loaded, None);
            } else {
                assert_eq!(buf.len(), CMAP_FONT_HEADER + MAX_INDEX_CMAP_BYTES as usize);
                assert_eq!(loaded, Some((0, 28, len)));
            }
        }
    }

    /// #668: a file whose `cmap` record runs past its end (cut short
    /// after the scan) reads as no `cmap`, not as a short table; so does
    /// a record at offset 0, which swash reads as no table.
    #[test]
    fn a_cmap_past_the_end_of_the_file_or_at_zero_is_missing() {
        let temp = font_scratch("short");
        let dir = temp.path();
        let path = dir.join("short.ttf");
        font_with_cmap_len(&path, 4096);
        truncate(&path, 28 + 100);
        let file = std::fs::File::open(&path).expect("open");
        let (mut buf, mut loaded) = (Vec::new(), None);
        assert_eq!(
            read_face_cmap(&file, 0, u64::MAX, &mut buf, &mut loaded),
            CmapRead::Missing
        );
        assert_eq!(loaded, None);

        font_with_cmap_len(&path, 64);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open");
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(20)).expect("seek");
        std::io::Write::write_all(&mut file, &0_u32.to_be_bytes()).expect("write");
        assert_eq!(
            read_face_cmap(&file, 0, u64::MAX, &mut buf, &mut loaded),
            CmapRead::Missing
        );
    }

    /// #716: a `cmap` record at offset 0 is no table to the lookups, as
    /// to swash and [`read_face_cmap`]. `FontRef::table` gives the file's
    /// first bytes for it, and this file's header reads as a `cmap` with
    /// a symbol record (`numTables` 3, `searchRange` 0) whose subtable,
    /// at the `rangeShift` offset 60, maps 'A' to glyph 5.
    #[test]
    fn a_cmap_at_offset_zero_maps_nothing() {
        let subtable = cmap_with_groups(12, &[(0x41, 0x41, 5)])[12..].to_vec();
        let len = u32::try_from(60 + subtable.len()).expect("small");
        let mut font = b"OTTO".to_vec();
        for field in [3_u16, 0, 0, 60] {
            font.extend_from_slice(&field.to_be_bytes());
        }
        for (tag, start, len) in [(b"aaaa", 60, 0), (b"cmap", 0, len), (b"zzzz", 60, 0)] {
            font.extend_from_slice(tag);
            for field in [0_u32, start, len] {
                font.extend_from_slice(&field.to_be_bytes());
            }
        }
        font.extend_from_slice(&subtable);
        let face = swash::FontRef::from_index(&font, 0).expect("an sfnt");
        let header = face.table(CMAP_TAG).expect("the record");
        assert_eq!(cmap::map(header, 0x41), 5, "the header reads as a cmap");
        assert_eq!(cmap_table(&face), None);
        assert_eq!(face.charmap().map('A'), 0);
    }

    /// How long the #669 tests wait for an open or a build that a FIFO
    /// could block: far past a two-file build even under load, and well
    /// inside nextest's 300 s termination, so a blocked open fails the
    /// test instead of hanging the run.
    const FIFO_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Makes a FIFO at `path` (the `mkfifo` tool: this crate forbids
    /// `unsafe`).
    fn mkfifo(path: &std::path::Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo {}", path.display());
    }

    /// Lets an open of the FIFO at `path` that is blocked waiting for a
    /// writer return (it then reads end of file), so a failed test does
    /// not leave a thread blocked. A non-blocking write open: it fails at
    /// once when nobody waits to read.
    fn release_fifo_reader(path: &std::path::Path) {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path);
    }

    /// #669: a font path that fontdb scanned as a font and that is a FIFO
    /// when the index builds does not block the index thread: the build
    /// skips it, indexes the other file and lands. A plain open of the
    /// FIFO waits for a writer forever; here that fails after
    /// [`FIFO_WAIT`].
    #[test]
    fn a_fifo_swapped_in_after_the_scan_does_not_block_the_index() {
        const MENLO: &str = "/System/Library/Fonts/Menlo.ttc";
        const MONACO: &str = "/System/Library/Fonts/Monaco.ttf";
        let temp = font_scratch("fifo");
        let dir = temp.path();
        let copy = dir.join("Menlo.ttc");
        std::fs::copy(MENLO, &copy).expect("Menlo.ttc is installed");
        let mut db = fontdb::Database::new();
        db.load_font_file(&copy).expect("the copy loads");
        db.load_font_file(MONACO).expect("Monaco is installed");
        let monaco = db.faces().last().map(|f| f.id);
        std::fs::remove_file(&copy).expect("remove the copy");
        mkfifo(&copy);
        let slot = Arc::new(OnceLock::new());
        let (landed, wait) = std::sync::mpsc::channel();
        spawn_coverage_build(
            db,
            &slot,
            Some(Box::new(move || {
                let _ = landed.send(());
            })),
        );
        if wait.recv_timeout(FIFO_WAIT).is_err() {
            release_fifo_reader(&copy);
            panic!("the index did not land in {FIFO_WAIT:?}: the build blocked on the FIFO");
        }
        let index = slot.get().expect("the index landed");
        assert_eq!(index.lookup('A'), monaco, "the FIFO's faces are left out");
        assert!(
            index.ranges.iter().all(|&(_, _, id)| Some(id) == monaco),
            "{:?}",
            index.ranges
        );
    }

    /// #669: the index's open refuses a FIFO, a device, a socket and a
    /// directory, at once, and opens a regular file. Each open runs on a
    /// helper thread so one that blocks fails after [`FIFO_WAIT`].
    #[test]
    fn the_index_open_refuses_what_is_not_a_regular_file() {
        let temp = font_scratch("kinds");
        let dir = temp.path();
        let fifo = dir.join("fifo.ttf");
        mkfifo(&fifo);
        let socket = dir.join("socket.ttf");
        let _listener =
            std::os::unix::net::UnixListener::bind(&socket).expect("bind a socket file");
        let regular = dir.join("regular.ttf");
        std::fs::write(&regular, b"not a font").expect("write a file");
        let subdir = dir.join("dir.ttf");
        std::fs::create_dir(&subdir).expect("make a directory");
        for (path, regular) in [
            (fifo.clone(), false),
            (std::path::PathBuf::from("/dev/null"), false),
            (socket, false),
            (subdir, false),
            (regular, true),
        ] {
            let (done, wait) = std::sync::mpsc::channel();
            let target = path.clone();
            std::thread::spawn(move || {
                let _ = done.send(open_regular_file(&target).map(drop));
            });
            let Ok(opened) = wait.recv_timeout(FIFO_WAIT) else {
                release_fifo_reader(&path);
                panic!("opening {} blocked for {FIFO_WAIT:?}", path.display());
            };
            assert_eq!(opened.is_ok(), regular, "{}: {opened:?}", path.display());
        }
    }

    /// A `cmap` table with one Windows Unicode full-repertoire (3, 10)
    /// subtable of `format` (12 or 13) holding `groups`, each (first code
    /// point, last code point, glyph).
    fn cmap_with_groups(format: u16, groups: &[(u32, u32, u32)]) -> Vec<u8> {
        let count = u32::try_from(groups.len()).expect("a test table");
        let mut cmap = Vec::new();
        for field in [0_u16, 1, 3, 10] {
            cmap.extend_from_slice(&field.to_be_bytes());
        }
        cmap.extend_from_slice(&12_u32.to_be_bytes());
        cmap.extend_from_slice(&format.to_be_bytes());
        cmap.extend_from_slice(&0_u16.to_be_bytes());
        for field in [16 + 12 * count, 0, count] {
            cmap.extend_from_slice(&field.to_be_bytes());
        }
        for &(start, end, glyph) in groups {
            for field in [start, end, glyph] {
                cmap.extend_from_slice(&field.to_be_bytes());
            }
        }
        cmap
    }

    /// Writes Monaco to `path` with its `cmap` record pointing at `cmap`,
    /// appended at the end of the file.
    fn write_monaco_with_cmap(path: &std::path::Path, cmap: &[u8]) {
        let mut font =
            std::fs::read("/System/Library/Fonts/Monaco.ttf").expect("Monaco is installed");
        font.resize(font.len().next_multiple_of(4), 0);
        let offset = u32::try_from(font.len()).expect("Monaco is small");
        let len = u32::try_from(cmap.len()).expect("a test table");
        let tables = usize::from(u16::from_be_bytes([font[4], font[5]]));
        for record in (0..tables).map(|t| 12 + 16 * t) {
            if &font[record..record + 4] == b"cmap" {
                font[record + 8..record + 12].copy_from_slice(&offset.to_be_bytes());
                font[record + 12..record + 16].copy_from_slice(&len.to_be_bytes());
            }
        }
        font.extend_from_slice(cmap);
        std::fs::write(path, font).expect("write the font");
    }

    /// Writes a collection (`.ttc`) of `faces` faces of Monaco to `path`,
    /// every table but `cmap` Monaco's and shared by all faces. With
    /// `shared`, every face is one table directory, whose `cmap` is
    /// `cmap_of(0)`; otherwise face `n` has its own copy of the directory,
    /// whose `cmap` is `cmap_of(n)`. `cmap_of` gives a range (offset,
    /// length) of `cmaps`, which is written after Monaco's tables (#716:
    /// the file #689's review indexed, 20 000 faces of one 40-byte `cmap`,
    /// is `shared` with that many faces).
    fn write_monaco_collection(
        path: &std::path::Path,
        faces: u32,
        shared: bool,
        cmaps: &[u8],
        cmap_of: impl Fn(usize) -> (usize, usize),
    ) {
        let mut monaco =
            std::fs::read("/System/Library/Fonts/Monaco.ttf").expect("Monaco is installed");
        monaco.resize(monaco.len().next_multiple_of(4), 0);
        let tables = usize::from(u16::from_be_bytes([monaco[4], monaco[5]]));
        let directory_len = 12 + 16 * tables;
        let copies = if shared { 1 } else { faces as usize };
        let header = 12 + 4 * faces as usize;
        let data = header + directory_len * copies;
        let cmaps_at = data + monaco.len();
        let at = |n: usize| u32::try_from(n).expect("a small collection");
        let mut file = Vec::with_capacity(cmaps_at + cmaps.len());
        file.extend_from_slice(b"ttcf");
        file.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
        file.extend_from_slice(&faces.to_be_bytes());
        for face in 0..faces as usize {
            let copy = if shared { 0 } else { face };
            file.extend_from_slice(&at(header + directory_len * copy).to_be_bytes());
        }
        for copy in 0..copies {
            let mut directory = monaco[..directory_len].to_vec();
            for record in (0..tables).map(|t| 12 + 16 * t) {
                let fields = &mut directory[record..record + 16];
                let (offset, len) = if &fields[..4] == b"cmap" {
                    let (offset, len) = cmap_of(copy);
                    (at(cmaps_at + offset), at(len))
                } else {
                    let offset = u32::from_be_bytes(fields[8..12].try_into().expect("4 bytes"));
                    let len = u32::from_be_bytes(fields[12..16].try_into().expect("4 bytes"));
                    (offset + at(data), len)
                };
                fields[8..12].copy_from_slice(&offset.to_be_bytes());
                fields[12..16].copy_from_slice(&len.to_be_bytes());
            }
            file.extend_from_slice(&directory);
        }
        file.extend_from_slice(&monaco);
        file.extend_from_slice(cmaps);
        std::fs::write(path, file).expect("write the collection");
    }

    /// The index over one collection of Monaco faces
    /// ([`write_monaco_collection`]), built on a helper thread: the index,
    /// what it cost, how long the build took and how many reads of the
    /// file it made. Fails when the build
    /// panics or takes more than four minutes.
    fn index_monaco_collection(
        name: &str,
        faces: u32,
        shared: bool,
        cmaps: &[u8],
        cmap_of: impl Fn(usize) -> (usize, usize),
    ) -> (Coverage, BuildStats, std::time::Duration, u64) {
        let dir = font_scratch(name);
        let path = dir.path().join("Monaco.ttc");
        write_monaco_collection(&path, faces, shared, cmaps, cmap_of);
        let mut db = fontdb::Database::new();
        db.load_font_file(&path).expect("the collection loads");
        assert_eq!(db.faces().count(), faces as usize);
        let first = db.faces().next().map(|f| f.id);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let (index, stats) = Coverage::build_with_stats(&db);
            let elapsed = started.elapsed();
            let _ = tx.send((index, stats, elapsed, test_read_hook::reads()));
        });
        let built = rx
            .recv_timeout(std::time::Duration::from_secs(240))
            .expect("the index lands within four minutes, without a panic");
        assert!(
            built
                .0
                .ranges
                .iter()
                .all(|&(_, _, face)| Some(face) == first),
            "the first face takes every code point"
        );
        built
    }

    /// The ranges `cmap_with_groups(12, &[(0, u32::MAX, 0)])` indexes:
    /// the glyph is the code point's low 16 bits, 0 (uncovered) at each
    /// 0x10000th.
    fn every_code_point_but_each_plane_start() -> Vec<(u32, u32)> {
        (0..0x11)
            .map(|plane| ((plane << 16) | 1, (plane << 16) | 0xFFFF))
            .collect()
    }

    fn first_last(index: &Coverage) -> Vec<(u32, u32)> {
        index
            .ranges
            .iter()
            .map(|&(first, last, _)| (first, last))
            .collect()
    }

    /// #716: a collection of 20 000 faces sharing one table directory and
    /// one 40-byte `cmap` whose group reaches `u32::MAX` took 8.9 s to
    /// index in release (11.2 s here), the walk of the code space once
    /// per face. A face whose `cmap` is the one the face before it read
    /// is not walked again, and one that shares its table directory reads
    /// only the directory's offset: one walk in all, and under 100 ms in
    /// release (about 20 ms here, at a load of 150).
    #[test]
    fn twenty_thousand_faces_of_one_hostile_cmap_walk_it_once() {
        let cmap = cmap_with_groups(12, &[(0, u32::MAX, 0)]);
        assert_eq!(cmap.len(), 40);
        let (index, stats, elapsed, reads) =
            index_monaco_collection("shared", 20_000, true, &cmap, |_| (0, 40));
        eprintln!("20 000 faces indexed in {elapsed:?} with {reads} reads: {stats:?}");
        // Each face reads the collection header and its directory's
        // offset there; the first one also its directory and `cmap`.
        assert!(reads < 2 * 20_000 + 16, "{reads} reads");
        assert_eq!(first_last(&index), every_code_point_but_each_plane_start());
        assert_eq!(
            stats.walked,
            cmap::Walk {
                code_points: u64::from(CODE_SPACE_U32),
                records: 1,
                cut: false,
            }
        );
        assert_eq!((stats.reused, stats.cmap_bytes), (19_999, 40));
        if cfg!(not(debug_assertions)) {
            assert!(
                elapsed < std::time::Duration::from_millis(100),
                "{elapsed:?}"
            );
        }
    }

    /// #716: faces with their own table directories and their own
    /// hostile `cmap`s each walk the code space, so the whole build stops
    /// at [`MAX_INDEX_STEPS`], 64 such walks: the 64th face is cut, the
    /// rest are not read, and the first face's coverage stands.
    #[test]
    fn the_whole_build_walks_at_most_its_step_budget() {
        let cmap = cmap_with_groups(12, &[(0, u32::MAX, 0)]);
        let faces = 80;
        let (index, stats, _, _) =
            index_monaco_collection("steps", faces, false, &cmap.repeat(80), |copy| {
                (40 * copy, 40)
            });
        assert_eq!(first_last(&index), every_code_point_but_each_plane_start());
        assert_eq!(stats.walked.steps(), MAX_INDEX_STEPS);
        assert!(stats.walked.cut);
        assert_eq!(stats.walked.records, 64);
        assert_eq!((stats.reused, stats.cmap_bytes), (0, 64 * 40));
    }

    /// #716: faces with their own `cmap`s of 2 MiB (overlapping ranges of
    /// one table of zeros, which walks nothing) each read that much, so
    /// the whole build stops at [`MAX_INDEX_CMAP_READ`]: 64 are read, the
    /// 65th would pass it and it and the rest are left out.
    #[test]
    fn the_whole_build_reads_at_most_its_cmap_budget() {
        let len = MAX_INDEX_CMAP_BYTES as usize;
        let faces = 70;
        let zeros = vec![0; len + 4 * faces as usize];
        let (index, stats, _, _) =
            index_monaco_collection("bytes", faces, false, &zeros, |copy| (4 * copy, len));
        assert_eq!(index.ranges, []);
        assert_eq!(stats.cmap_bytes, MAX_INDEX_CMAP_READ);
        assert_eq!((stats.walked, stats.reused), (cmap::Walk::default(), 0));
    }

    /// The index over one font, Monaco with `cmap` as its character map,
    /// built on a helper thread: its ranges (first, last), and how long the
    /// build took. Fails when the build panics or takes more than a minute.
    fn index_monaco_with(name: &str, cmap: &[u8]) -> (Vec<(u32, u32)>, std::time::Duration) {
        let dir = font_scratch(name);
        let path = dir.path().join("Monaco.ttf");
        write_monaco_with_cmap(&path, cmap);
        let mut db = fontdb::Database::new();
        db.load_font_file(&path).expect("the copy loads");
        assert_eq!(db.faces().count(), 1);
        let id = db.faces().next().map(|f| f.id);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let index = Coverage::build(&db);
            let _ = tx.send((index, started.elapsed()));
        });
        let (index, elapsed) = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the index lands within a minute, without a panic");
        assert!(index.ranges.iter().all(|&(_, _, face)| Some(face) == id));
        let ranges = index
            .ranges
            .iter()
            .map(|&(first, last, _)| (first, last))
            .collect();
        (ranges, elapsed)
    }

    /// #689: a format-12 group whose glyph offset is `u32::MAX` overflowed
    /// swash's glyph arithmetic, a panic in debug builds that killed the
    /// index thread. The arithmetic wraps, as in release: 'A' takes glyph
    /// 0xFFFF, 'B' glyph 0 (not covered), 'C' to 'Z' glyphs 1 to 24. Run
    /// in debug and in release (`cargo test --release`).
    #[test]
    fn a_format12_glyph_offset_of_u32_max_wraps_without_a_panic() {
        let (ranges, _) =
            index_monaco_with("wrap", &cmap_with_groups(12, &[(0x41, 0x5A, u32::MAX)]));
        assert_eq!(ranges, [(0x41, 0x41), (0x43, 0x5A)]);
    }

    /// #716: with the index landed, a face whose format-12 glyph offset
    /// is near `u32::MAX` is offered for the characters it maps, and the
    /// lookups that follow went through swash's `Charmap::map`, whose
    /// glyph arithmetic overflowed: a panic in debug builds in
    /// `nominal_glyph`, `covers` and `metrics`, and on the render path in
    /// `classify` and `first_uncovered`. They read the character map with
    /// Tessera's own reader now, which wraps as the index does. Menlo
    /// comes first; the hostile Monaco copy maps '/' and '0', 'L' and 'M'
    /// (Menlo's) and U+E000 to U+E0FF (its own), each group from glyph
    /// `u32::MAX`: U+E000 is glyph 0xFFFF, U+E001 glyph 0, U+E002 glyph 1.
    /// Meaningful in debug builds, which the workspace tests are.
    #[test]
    fn lookups_in_a_face_with_a_glyph_offset_near_u32_max_wrap_without_a_panic() {
        let dir = font_scratch("lookups");
        let path = dir.path().join("Monaco.ttf");
        let groups = [
            (0x2F, 0x30, u32::MAX),
            (0x4C, 0x4D, u32::MAX),
            (0xE000, 0xE0FF, u32::MAX),
        ];
        write_monaco_with_cmap(&path, &cmap_with_groups(12, &groups));
        let mut db = fontdb::Database::new();
        db.load_font_file("/System/Library/Fonts/Menlo.ttc")
            .expect("Menlo is installed");
        db.load_font_file(&path).expect("the copy loads");
        let hostile = db.faces().last().map(|f| f.id).expect("the copy");
        let system = FontSystem::new_with_locale_and_db("en-US".into(), db);
        let config = FontConfig {
            family: DEFAULT_FAMILY.into(),
            size_pt: 13.0,
            scale: 1.0,
            fallback: Vec::new(),
        };
        let fonts = Fonts::with_system(system, &config, IndexBuild::Never).expect("Menlo");
        fonts.build_index_now();
        {
            let mut inner = fonts.lock();
            let face = inner.resolve('\u{E002}', FontStyle::Regular).face;
            assert!(
                matches!(face.0, FaceRepr::Font { id, .. } if id == hostile),
                "the index offers the copy: {face:?}"
            );
            for (ch, glyph) in [
                ('\u{E000}', 0xFFFF),
                ('\u{E001}', 0),
                ('\u{E002}', 1),
                ('\u{E0FF}', 0xFE),
                ('/', 0xFFFF),
                ('0', 0),
                ('L', 0xFFFF),
                ('M', 0),
                ('A', 0),
            ] {
                assert_eq!(inner.nominal_glyph(face, ch), glyph, "{ch:?}");
                assert_eq!(inner.covers(face, ch), glyph != 0, "{ch:?}");
            }
            assert!(inner.metrics(face, 13.0).is_some());
            // Menlo's '0' gives the cell advance, not the 0.6 em fallback.
            let menlo = inner.resolve('0', FontStyle::Regular).face;
            let zero = inner.nominal_glyph(menlo, '0');
            assert_ne!(zero, 0);
            let advance = inner
                .load(menlo)
                .map(|font| {
                    font.as_swash()
                        .glyph_metrics(&[])
                        .scale(13.0)
                        .advance_width(zero)
                })
                .expect("Menlo loads");
            let cell = inner.metrics(menlo, 13.0).expect("Menlo").cell_advance;
            assert!((cell - advance).abs() < 1e-4, "{cell} against {advance}");
            assert!((cell - 13.0 * 0.6).abs() > 1e-3, "{cell}");
        }
        // A plain cell, and a cluster whose combining mark the copy does
        // not cover: `classify` asks the copy for the cluster's nominal
        // glyphs, `first_uncovered` asks it and then Menlo.
        let mut grid = tessera_grid::Grid::new(1, 8);
        grid.put_str("\u{E002}\u{E003}\u{301}L0");
        let line = grid.line(0).expect("row 0");
        let mut shaping = crate::shaping::Shaper::with_fonts(fonts, 13.0, 1.0);
        let glyphs = shaping.shape_grid_line(line).len();
        assert_ne!(glyphs, 0);
    }

    /// #689: format-12 groups reaching `u32::MAX` walked four billion code
    /// points each, and the 2 MiB `cmap` cap holds about 174 000 groups.
    /// Each is cut at the code space and no code point is walked twice, so
    /// the build at the cap costs at most one walk of the code space: this
    /// test fails at its one-minute bound otherwise. The walk follows the
    /// lookup's binary search (#725), whose first group read, a middle
    /// one, covers the code space but each 0x10000th code point (glyph 0):
    /// the first group, (U+0041, U+005A), is never reached, by the lookup
    /// or the walk.
    #[test]
    fn format12_groups_reaching_u32_max_cost_one_walk_of_the_code_space() {
        let count = (MAX_INDEX_CMAP_BYTES as usize - 28) / 12;
        let mut groups = vec![(0, u32::MAX, 0); count];
        groups[0] = (0x41, 0x5A, 1);
        let cmap = cmap_with_groups(12, &groups);
        assert!(
            cmap.len() <= MAX_INDEX_CMAP_BYTES as usize,
            "{}",
            cmap.len()
        );
        assert_eq!(
            cmap::map(&cmap, 0x41),
            0x41,
            "U+0041 should map through the middle group (glyph 0x41), not the first group"
        );
        let (ranges, elapsed) = index_monaco_with("reach", &cmap);
        assert_eq!(ranges, every_code_point_but_each_plane_start());
        eprintln!("{count} groups indexed in {elapsed:?}");
    }

    /// #725: a face whose format-12 groups are unsorted, (U+E050, U+E060),
    /// (U+E010, U+E020), (U+E030, U+E040), was given U+E050 to U+E060 by
    /// the index, which walked the groups in turn, but the lookups' binary
    /// search never reaches that first group: `resolve` offered the face
    /// for U+E055, which then drew as `.notdef` though a later face covers
    /// it. The index follows the lookup's search: the later face draws
    /// U+E055, and the unsorted face still draws what its lookup finds.
    #[test]
    fn an_unsorted_cmap_claims_only_what_its_lookup_finds() {
        let dir = font_scratch("unsorted");
        let unsorted = dir.path().join("Unsorted.ttf");
        let covering = dir.path().join("Covering.ttf");
        let groups = [
            (0xE050, 0xE060, 1),
            (0xE010, 0xE020, 2),
            (0xE030, 0xE040, 3),
        ];
        write_monaco_with_cmap(&unsorted, &cmap_with_groups(12, &groups));
        write_monaco_with_cmap(&covering, &cmap_with_groups(12, &[(0xE000, 0xE0FF, 1)]));
        let mut db = fontdb::Database::new();
        db.load_font_file("/System/Library/Fonts/Menlo.ttc")
            .expect("Menlo is installed");
        db.load_font_file(&unsorted)
            .expect("the unsorted copy loads");
        let unsorted = db.faces().last().map(|f| f.id).expect("the copy");
        db.load_font_file(&covering)
            .expect("the covering copy loads");
        let covering = db.faces().last().map(|f| f.id).expect("the copy");
        let index = Coverage::build(&db);
        for (cp, face) in [
            (0xE00F, covering),
            (0xE010, unsorted),
            (0xE020, unsorted),
            (0xE021, covering),
            (0xE035, unsorted),
            (0xE041, covering),
            (0xE050, covering),
            (0xE055, covering),
            (0xE060, covering),
        ] {
            let ch = char::from_u32(cp).expect("a scalar");
            assert_eq!(index.lookup(ch), Some(face), "U+{cp:04X}");
        }
        let system = FontSystem::new_with_locale_and_db("en-US".into(), db);
        let config = FontConfig {
            family: DEFAULT_FAMILY.into(),
            size_pt: 13.0,
            scale: 1.0,
            fallback: Vec::new(),
        };
        let fonts = Fonts::with_system(system, &config, IndexBuild::Never).expect("Menlo");
        fonts.build_index_now();
        let mut inner = fonts.lock();
        for (ch, face, glyph) in [
            ('\u{E055}', covering, 0x56),
            ('\u{E015}', unsorted, 7),
            ('\u{E035}', unsorted, 8),
        ] {
            let resolved = inner.resolve(ch, FontStyle::Regular).face;
            assert!(
                matches!(resolved.0, FaceRepr::Font { id, .. } if id == face),
                "{ch:?} from {resolved:?}"
            );
            assert_eq!(inner.nominal_glyph(resolved, ch), glyph, "{ch:?}");
        }
    }

    /// #689: a panic in a face's character map walk leaves that face out
    /// and the index still lands with the others. Menlo's faces come first
    /// and the walk of each panics; Monaco then takes their characters.
    #[test]
    fn a_panic_in_a_cmap_walk_leaves_the_face_out_and_the_index_lands() {
        const MENLO: &str = "/System/Library/Fonts/Menlo.ttc";
        const MONACO: &str = "/System/Library/Fonts/Monaco.ttf";
        let mut db = fontdb::Database::new();
        db.load_font_file(MENLO).expect("Menlo is installed");
        let menlo_faces = u32::try_from(db.faces().count()).expect("a few faces");
        db.load_font_file(MONACO).expect("Monaco is installed");
        let monaco = db.faces().last().map(|f| f.id);
        let walks = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&walks);
        test_read_hook::set_during_walk(Some(Box::new(move |index| {
            counted.fetch_add(1, Ordering::Relaxed);
            assert!(index >= menlo_faces, "a hostile walk of face {index}");
        })));
        let index = Coverage::build(&db);
        test_read_hook::set_during_walk(None);
        assert_eq!(walks.load(Ordering::Relaxed), u64::from(menlo_faces) + 1);
        assert_eq!(index.lookup('A'), monaco, "Menlo's faces are left out");
        assert!(
            index.ranges.iter().all(|&(_, _, id)| Some(id) == monaco),
            "{:?}",
            index.ranges
        );
    }

    /// #689: swash never reads a format-13 (many-to-one) subtable, so a
    /// group reaching `u32::MAX` there is not walked at all.
    #[test]
    fn a_format13_subtable_is_not_walked() {
        let (ranges, _) =
            index_monaco_with("format13", &cmap_with_groups(13, &[(0, u32::MAX, 1); 4]));
        assert_eq!(ranges, []);
    }

    /// Fonts over Menlo (a system font, mapped) and the user font
    /// directory `dir` (read into memory, #683), with `family` primary and
    /// `fallback` configured, and no coverage index.
    fn fonts_with_user_dir(dir: &std::path::Path, family: &str, fallback: &[&str]) -> Fonts {
        let budget = Box::leak(Box::new(font_scan::OwnedBudget::new(
            font_scan::MAX_OWNED_TOTAL_BYTES,
        )));
        fonts_with_budget(dir, family, fallback, budget)
    }

    /// [`fonts_with_user_dir`], the user fonts' owned bytes counted
    /// against `budget`.
    fn fonts_with_budget(
        dir: &std::path::Path,
        family: &str,
        fallback: &[&str],
        budget: &'static font_scan::OwnedBudget,
    ) -> Fonts {
        let mut db = fontdb::Database::new();
        db.load_font_file("/System/Library/Fonts/Menlo.ttc")
            .expect("Menlo is installed");
        let owned = font_scan::load_user_fonts_dir(&mut db, dir, budget);
        let system = FontSystem::new_with_locale_and_db("en-US".into(), db);
        let config = FontConfig {
            family: family.into(),
            size_pt: 13.0,
            scale: 1.0,
            fallback: fallback.iter().map(|&f| f.to_owned()).collect(),
        };
        Fonts::with_fonts(system, owned, &config, IndexBuild::Never).expect("fonts")
    }

    /// The first family name of `face`.
    fn family_of(inner: &Inner, face: FaceId) -> Option<String> {
        let FaceRepr::Font { id, .. } = face.0 else {
            return None;
        };
        let info = inner.system.db().face(id)?;
        info.families.first().map(|(name, _)| name.clone())
    }

    /// The face that draws `ch` and the glyphs its shaping and rasterising
    /// give, one (glyph, ink pixels) pair per glyph.
    fn shape_and_draw(inner: &mut Inner, ch: char) -> (FaceId, Vec<(u16, usize)>) {
        let face = inner.resolve(ch, FontStyle::Regular).face;
        let drawn = inner
            .shape(face, &ch.to_string(), 26.0)
            .iter()
            .map(|glyph| {
                let key = GlyphKey {
                    face_id: face,
                    glyph_id: glyph.glyph_id,
                    size_px_64: 26 * 64,
                    subpixel_x: 0,
                    synthetic_bold: false,
                    synthetic_italic: false,
                    cell_size: [16, 32],
                    baseline_px: 24,
                };
                let ink = inner.rasterize(&key).map_or(0, |image| {
                    image.data.iter().filter(|&&byte| byte != 0).count()
                });
                (glyph.glyph_id, ink)
            })
            .collect();
        (face, drawn)
    }

    /// #683: a user font in use that another process truncates in place
    /// keeps shaping and drawing from the copy in memory. Mapped, as before
    /// #683, the next read of its glyphs past the file's new end raised
    /// `SIGBUS` and killed the process (this test's).
    #[test]
    fn a_user_font_truncated_in_place_while_in_use_keeps_drawing() {
        let dir = font_scratch("in-use");
        let path = dir.path().join("Monaco.ttf");
        std::fs::copy("/System/Library/Fonts/Monaco.ttf", &path).expect("copy Monaco");
        let fonts = fonts_with_user_dir(dir.path(), "Monaco", &[]);
        let mut inner = fonts.lock();
        let text: Vec<char> = ('!'..='~').collect();
        let before: Vec<_> = text
            .iter()
            .map(|&ch| shape_and_draw(&mut inner, ch))
            .collect();
        let copy = before[0].0;
        assert_eq!(
            family_of(&inner, copy).as_deref(),
            Some("Monaco"),
            "the copy draws: {copy:?}"
        );
        truncate(&path, 0);
        for (&ch, want) in text.iter().zip(&before) {
            let got = shape_and_draw(&mut inner, ch);
            assert_eq!(&got, want, "{ch:?} after the truncation");
            assert!(got.1.iter().all(|&(glyph, _)| glyph != 0), "{ch:?}");
        }
        assert!(
            before
                .iter()
                .any(|(_, glyphs)| glyphs.iter().any(|&(_, ink)| ink > 0))
        );
    }

    /// #683: a user font whose read the owned budget refuses is mapped
    /// before its first load, as a file over the per-file cap is, and draws
    /// exactly as the copy in memory does. Read without that admission, it
    /// would be an empty source and fail to load.
    #[test]
    fn a_user_font_over_the_owned_budget_is_mapped_and_draws() {
        let dir = font_scratch("over-budget");
        std::fs::copy(
            "/System/Library/Fonts/Monaco.ttf",
            dir.path().join("Monaco.ttf"),
        )
        .expect("copy Monaco");
        let text: Vec<char> = ('!'..='~').collect();
        let draw = |fonts: &Fonts| -> Vec<_> {
            let mut inner = fonts.lock();
            let drawn: Vec<_> = text
                .iter()
                .map(|&ch| shape_and_draw(&mut inner, ch))
                .collect();
            assert_eq!(family_of(&inner, drawn[0].0).as_deref(), Some("Monaco"));
            drawn
        };
        let owned = draw(&fonts_with_user_dir(dir.path(), "Monaco", &[]));
        let budget = Box::leak(Box::new(font_scan::OwnedBudget::new(1)));
        let fonts = fonts_with_budget(dir.path(), "Monaco", &[], budget);
        assert_eq!(draw(&fonts), owned);
        assert_eq!(budget.used(), 0, "nothing read into memory");
    }

    /// #683: a user font truncated in place before any face of it is used
    /// draws nothing: its characters fall back past it, here to Menlo's
    /// `.notdef`, and nothing crashes. The scan read only its tables, so
    /// nothing of it was in memory yet. An untruncated copy (the control)
    /// draws the characters itself.
    #[test]
    fn a_user_font_truncated_before_its_first_use_falls_back() {
        let groups = [(0xE000, 0xE0FF, 36)];
        for truncated in [false, true] {
            let dir = font_scratch("first-use");
            let path = dir.path().join("Monaco.ttf");
            write_monaco_with_cmap(&path, &cmap_with_groups(12, &groups));
            let fonts = fonts_with_user_dir(dir.path(), DEFAULT_FAMILY, &["Monaco"]);
            if truncated {
                truncate(&path, 0);
            }
            let mut inner = fonts.lock();
            let (face, glyphs) = shape_and_draw(&mut inner, '\u{E010}');
            let family = family_of(&inner, face);
            if truncated {
                assert_ne!(family.as_deref(), Some("Monaco"), "{face:?}");
                assert!(glyphs.iter().all(|&(glyph, _)| glyph == 0), "{glyphs:?}");
                // The ASCII the primary face draws is untouched.
                let (menlo, ascii) = shape_and_draw(&mut inner, 'A');
                assert_eq!(family_of(&inner, menlo).as_deref(), Some("Menlo"));
                assert!(ascii.iter().all(|&(glyph, ink)| glyph != 0 && ink > 0));
            } else {
                assert_eq!(family.as_deref(), Some("Monaco"), "{face:?}");
                assert_eq!(glyphs.first().map(|&(glyph, _)| glyph), Some(52));
            }
        }
    }
}
