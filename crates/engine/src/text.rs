//! Text rasterization (Phase 20, TEXT-01).
//!
//! The ONE genuinely-new capability of Phase 20: turn a text string into an
//! [`crate::Frame`] the existing Phase-18 compositor consumes as an ordinary
//! [`crate::Layer`] — zero shader changes, no new compositor entry point.
//!
//! ## Offline-core (D-04, Pitfall 2)
//!
//! The [`cosmic_text::FontSystem`] is built ONLY from the four Inter faces
//! bundled via `include_bytes!` in `crates/engine/assets/fonts/`. It NEVER runs
//! an OS font-store scan (fontdb's system-load path is deliberately not
//! invoked) — a system-font scan would break both offline-core (CLAUDE.md rule
//! 5) and reproducible export (a machine without "Inter" would render
//! differently). Removing a bundled `.ttf` must break text rendering; it must
//! NEVER silently fall back to an OS face.
//!
//! ## STRAIGHT-alpha, color-bled buffer (D-02, SC-3, Pitfall 1/3)
//!
//! `compositor.rs`'s `fs_layer` premultiplies IN-SHADER
//! (`return vec4(texel.rgb * ea, ea)` with blend `(One, OneMinusSrcAlpha)`), so
//! it expects a **STRAIGHT-alpha** input texture. The rasterized buffer is
//! therefore straight alpha with the fill color bled into EVERY pixel
//! (including fully-transparent texels): `rgb = fill_rgb` everywhere,
//! `a = glyph coverage` (0 where no glyph). Premultiplying here would
//! double-premultiply → the exact dark fringe SC-3 forbids. Bleeding the fill
//! color into transparent texels also keeps bilinear filtering (Phase-19
//! keyframe zoom) from pulling RGB toward black at magnified glyph edges.
//!
//! cosmic-text 0.19 API used (verified against the vendored 0.19.0 source, not
//! training memory — the API drifted across 0.1x, Pitfall 5):
//!   - `FontSystem::new_with_locale_and_db(locale, db)` — offline constructor.
//!   - `fontdb::Database::{new, load_font_data, set_sans_serif_family}`.
//!   - `Buffer::{new(&mut fs, Metrics), set_wrap(Wrap), set_size(Option<f32>,
//!     Option<f32>), set_text(&str, &Attrs, Shaping, Option<Align>),
//!     shape_until_scroll(&mut fs, bool), layout_runs(), draw(&mut fs,
//!     &mut SwashCache, Color, |x, y, w, h, color| ...)}`.
//!   - The `draw` callback fires once per glyph pixel `(x, y, 1, 1, color)`
//!     with `color.a()` == swash coverage (RGB carries the base fill color).

use cosmic_text::{
    fontdb, Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    Weight, Wrap,
};

use crate::ffmpeg::Frame;

/// The bundled Inter family name. `rasterize_text` shapes against this exact
/// family (never a system face). Plan 02 reads this to build `is_bundled_font`.
pub const BUNDLED_FONT_FAMILY: &str = "Inter";

/// Hard upper bound on either raster dimension (px). T-20-01 DoS guard: a huge
/// `font_size` or enormous `content` can never demand a dimension larger than
/// this before allocation.
pub const MAX_RASTER_DIM: u32 = 8192;

/// Hard upper bound on the raster AREA (`width * height`). At 4 bytes/px this
/// caps a single text buffer at 64 MiB, comfortably above a 4K frame
/// (8.3 Mpx) while refusing the multi-GB allocation an unclamped
/// `font_size = 1e6` would otherwise demand (T-20-01). When the natural laid-out
/// size exceeds this, both dims are scaled down proportionally BEFORE `vec!`.
pub const MAX_RASTER_PIXELS: u64 = 16_777_216;

/// Hard upper bound on the effective glyph `px`. T-20-01: this bounds not just
/// the output buffer but the per-glyph swash/zeno bitmap — an unclamped
/// `font_size = 1e6` overflows zeno's `width * height` mask allocation
/// (multiply-with-overflow panic) long before our buffer clamp runs. Capping
/// `px` keeps every glyph bitmap bounded (4096px covers a full-height glyph on a
/// 4K canvas with headroom; larger requests clamp, never panic).
pub const MAX_FONT_PX: f32 = 4096.0;

/// The line-height multiple applied to `font_size` for `Metrics`.
const LINE_HEIGHT_FACTOR: f32 = 1.2;

/// Horizontal alignment of laid-out text lines. Engine-local primitive — the
/// engine takes no dependency on the domain-model crate; the core→primitive
/// translation lives at the app-layer call sites introduced in Plan 04.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

impl TextAlign {
    fn to_cosmic(self) -> Align {
        match self {
            TextAlign::Left => Align::Left,
            TextAlign::Center => Align::Center,
            TextAlign::Right => Align::Right,
        }
    }
}

/// A reusable text rasterizer: an offline [`FontSystem`] (bundled Inter only) +
/// a [`SwashCache`] for glyph bitmaps. Construct once and reuse across many
/// `rasterize_text` calls (shaping + glyph caches are hot paths).
pub struct TextRasterizer {
    fs: FontSystem,
    cache: SwashCache,
}

impl TextRasterizer {
    /// Build a rasterizer over the offline (bundled-bytes-only) FontSystem.
    pub fn new() -> Self {
        Self {
            fs: offline_font_system(),
            cache: SwashCache::new(),
        }
    }

    /// Rasterize `content` into a natural-size, STRAIGHT-alpha, color-bled
    /// [`Frame`]. Delegates to the free [`rasterize_text`] using this
    /// rasterizer's owned `fs` + `cache`.
    #[allow(clippy::too_many_arguments)]
    pub fn rasterize_text(
        &mut self,
        content: &str,
        px: f32,
        fill: [u8; 4],
        bold: bool,
        italic: bool,
        align: TextAlign,
        wrap_px: Option<f32>,
    ) -> Frame {
        rasterize_text(
            &mut self.fs,
            &mut self.cache,
            content,
            px,
            fill,
            bold,
            italic,
            align,
            wrap_px,
        )
    }
}

impl Default for TextRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

/// A 1×1 straight-alpha, color-bled, fully-transparent Frame (the safe minimal
/// return for degenerate / non-finite inputs).
fn minimal_frame(fill: [u8; 4]) -> Frame {
    Frame {
        width: 1,
        height: 1,
        rgba: vec![fill[0], fill[1], fill[2], 0],
    }
}

/// Rasterize `content` into an [`engine::Frame`](Frame) sized to the text's
/// laid-out pixel footprint.
///
/// The buffer is **STRAIGHT alpha, color-bled** (see the module doc): every
/// pixel's RGB is `fill`'s RGB (transparent texels included) and alpha carries
/// glyph coverage. The Phase-18 compositor's `fs_layer` premultiplies in-shader,
/// so this is exactly the input it expects — no premultiplication here.
///
/// - `px`: glyph size in pixels (already resolution-resolved by the caller —
///   Plan 04 converts the normalized `font_size` via `font_size * project_h`).
/// - `wrap_px`: `Some(w)` wraps within `w` (SC-4, `Wrap::Word`); `None` lays the
///   text out on natural single/explicit lines (`Wrap::None`).
/// - Non-finite / non-positive `px` (or non-finite `wrap_px`) returns a minimal
///   1×1 Frame rather than panicking (T-20-01).
/// - The laid-out dims are clamped to [`MAX_RASTER_DIM`] / [`MAX_RASTER_PIXELS`]
///   BEFORE allocation (T-20-01 DoS guard).
#[allow(clippy::too_many_arguments)]
pub fn rasterize_text(
    fs: &mut FontSystem,
    cache: &mut SwashCache,
    content: &str,
    px: f32,
    fill: [u8; 4],
    bold: bool,
    italic: bool,
    align: TextAlign,
    wrap_px: Option<f32>,
) -> Frame {
    // Reject non-finite / non-positive px up front (Tampering guard).
    if !px.is_finite() || px <= 0.0 {
        return minimal_frame(fill);
    }
    // Clamp the effective glyph size BEFORE shaping (T-20-01): this bounds the
    // per-glyph swash/zeno bitmap, not just our output buffer, so an absurd
    // font_size clamps instead of overflowing zeno's mask allocation.
    let px = px.min(MAX_FONT_PX);
    // A non-finite wrap bound is treated as "no wrap" rather than a panic; a
    // non-positive wrap bound also degrades to no-wrap. A finite bound is itself
    // clamped so it can never exceed the raster cap.
    let wrap_bound: Option<f32> = match wrap_px {
        Some(w) if w.is_finite() && w > 0.0 => Some(w.min(MAX_RASTER_DIM as f32)),
        Some(_) => None,
        None => None,
    };

    let metrics = Metrics::new(px, px * LINE_HEIGHT_FACTOR);
    let mut buffer = Buffer::new(fs, metrics);
    buffer.set_wrap(if wrap_bound.is_some() {
        Wrap::Word
    } else {
        Wrap::None
    });
    // Width bound = the wrap box (SC-4); height unbounded so the raster grows to
    // fit every wrapped line.
    buffer.set_size(wrap_bound, None);

    let attrs = Attrs::new()
        .family(Family::Name(BUNDLED_FONT_FAMILY))
        .weight(if bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if italic { Style::Italic } else { Style::Normal });
    buffer.set_text(content, &attrs, Shaping::Advanced, Some(align.to_cosmic()));
    buffer.shape_until_scroll(fs, false);

    // Measure the laid-out extent from the shaped runs.
    let mut max_line_w = 0.0f32;
    let mut max_bottom = 0.0f32;
    for run in buffer.layout_runs() {
        max_line_w = max_line_w.max(run.line_w);
        max_bottom = max_bottom.max(run.line_top + run.line_height);
    }

    // Raster width: the wrap box when wrapping (so centered/right lines land
    // correctly within it), else the natural max line width.
    let natural_w = match wrap_bound {
        Some(w) => w,
        None => max_line_w,
    };
    let (mut rw, mut rh) = (
        natural_w.ceil().max(1.0) as u32,
        max_bottom.ceil().max(1.0) as u32,
    );

    // T-20-01 DoS clamp BEFORE allocation: bound each dim, then bound the area.
    rw = rw.min(MAX_RASTER_DIM);
    rh = rh.min(MAX_RASTER_DIM);
    let area = rw as u64 * rh as u64;
    if area > MAX_RASTER_PIXELS {
        let scale = (MAX_RASTER_PIXELS as f64 / area as f64).sqrt();
        rw = ((rw as f64 * scale).floor() as u32).max(1);
        rh = ((rh as f64 * scale).floor() as u32).max(1);
    }

    // Color-bleed: initialize EVERY pixel to (fill_rgb, a=0). Transparent texels
    // therefore carry the fill color, so bilinear magnification never pulls RGB
    // toward black (Pitfall 3 / SC-3).
    let mut rgba = vec![0u8; rw as usize * rh as usize * 4];
    for chunk in rgba.chunks_exact_mut(4) {
        chunk[0] = fill[0];
        chunk[1] = fill[1];
        chunk[2] = fill[2];
        chunk[3] = 0;
    }

    // Draw: the callback fires per glyph pixel with `color.a()` == coverage
    // (RGB is the base fill, already bled). Write `a = max(a, coverage)`; clamp
    // to the (possibly DoS-clamped) buffer bounds so nothing writes OOB.
    let base = Color::rgba(fill[0], fill[1], fill[2], 255);
    buffer.draw(fs, cache, base, |x, y, w, h, color| {
        let coverage = color.a();
        if coverage == 0 {
            return;
        }
        // The legacy renderer emits 1×1 spans, but honor any w/h defensively.
        for dy in 0..h.max(1) {
            for dx in 0..w.max(1) {
                let px_x = x + dx as i32;
                let px_y = y + dy as i32;
                if px_x < 0 || px_y < 0 || px_x as u32 >= rw || px_y as u32 >= rh {
                    continue;
                }
                let i = (px_y as u32 * rw + px_x as u32) as usize * 4;
                rgba[i + 3] = rgba[i + 3].max(coverage);
            }
        }
    });

    Frame {
        width: rw,
        height: rh,
        rgba,
    }
}

/// The four bundled Inter faces (SIL OFL 1.1), compiled into the engine binary.
/// PROVENANCE Entry 10. `load_font_data` registers each without touching disk
/// or the OS font store.
static INTER_REGULAR: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
static INTER_BOLD: &[u8] = include_bytes!("../assets/fonts/Inter-Bold.ttf");
static INTER_ITALIC: &[u8] = include_bytes!("../assets/fonts/Inter-Italic.ttf");
static INTER_BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/Inter-BoldItalic.ttf");

/// Build a [`FontSystem`] from the bundled Inter bytes ONLY.
///
/// CRITICAL (offline-core / Pitfall 2): this never runs fontdb's OS font-store
/// scan. The database contains exactly the four Inter faces and nothing else,
/// so shaping is deterministic and identical on every machine (dev, CI,
/// packaged `.exe`). If the bundled `.ttf` bytes were removed, text rendering
/// would break here rather than falling back to a system font.
pub fn offline_font_system() -> FontSystem {
    let mut db = fontdb::Database::new();
    // Register the four bundled faces. `load_font_data` takes ownership of the
    // bytes; `.to_vec()` copies out of the compiled-in `&'static [u8]`.
    db.load_font_data(INTER_REGULAR.to_vec());
    db.load_font_data(INTER_BOLD.to_vec());
    db.load_font_data(INTER_ITALIC.to_vec());
    db.load_font_data(INTER_BOLD_ITALIC.to_vec());
    // Make the generic sans-serif resolve to Inter too (belt-and-suspenders:
    // rasterize_text names the family explicitly, but any fallback path also
    // lands on the bundled face rather than an OS font).
    db.set_sans_serif_family(BUNDLED_FONT_FAMILY);
    // The OS font-store scan is deliberately NOT invoked here — that omission
    // is the offline-core seam (see the module doc).
    FontSystem::new_with_locale_and_db("en-US".to_string(), db)
}
