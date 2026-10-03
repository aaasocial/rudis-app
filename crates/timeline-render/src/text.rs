//! The text pass — a thin wrapper over `glyphon`, and deliberately nothing more.
//!
//! # Why this is not hand-rolled
//!
//! 52-RESEARCH's "Don't Hand-Roll" table puts font shaping, glyph rasterisation and
//! texture-atlas packing in the "deceptively complex, already solved" category: kerning,
//! font fallback, subpixel positioning and RTL are all real, all easy to get subtly wrong,
//! and all invisible until a user's filename contains a character nobody tested. So text is
//! `glyphon`'s problem — MIT OR Apache-2.0 OR Zlib, wgpu-native, wrapping `cosmic-text`
//! (shaping/layout) and `etagere` (atlas packing).
//!
//! Three alternatives were explicitly NOT taken, and the reasons are recorded so a later
//! reader does not "improve" this. **Their names are deliberately absent from this file.**
//! The acceptance gate for this plan is a grep pinning those identifiers at zero across
//! this crate, and it is a USE-detector: writing "we do not use X" would trip it just as a
//! `use X` would, and a gate that reddens on its own rationale gets the rationale deleted
//! (52-02 hit this twice — see `52-02-SUMMARY.md` deviation 7). So, by description:
//!
//! * **glyphon's own superseded predecessor** — the older wgpu text crate whose repository
//!   declares itself superseded and points at glyphon as the successor.
//! * **The Linebender GPU vector renderer** — self-declared *alpha* as of its own January
//!   2026 status post, and it solves arbitrary vector paths with gradients, a far bigger
//!   problem than a Timeline of flat rectangles has.
//! * **A hand-rolled bitmap-font path** — the exact thing the research section exists to
//!   forbid.
//!
//! All three are named in full, with citations, in `52-RESEARCH.md` § Don't Hand-Roll and
//! § State of the Art, and in 52-CONTEXT D-03.
//!
//! Quads, by contrast, ARE hand-rolled here (`quads.rs`), because a rectangle with a
//! slightly wrong corner radius looks slightly wrong and is trivially fixable, whereas a
//! slightly wrong font metric is a correctness bug users cannot see coming. That is the
//! whole dividing line, and this file is on the far side of it.
//!
//! # ONE of everything, reused forever
//!
//! One [`glyphon::FontSystem`], one [`glyphon::SwashCache`], one [`glyphon::TextAtlas`],
//! one [`glyphon::Viewport`], one [`glyphon::TextRenderer`], all owned by the surface and
//! reused across every frame. Constructing a font system enumerates the machine's whole
//! font database and costs real time; doing it per frame — or, worse, per label — would be
//! the single most expensive mistake available in this file.
//!
//! "One" is greppable rather than merely stated: this file contains exactly ONE
//! `font_system: FontSystem,` field and exactly ONE construction of one. Both sentences
//! here are phrased to avoid spelling that constructor, because the count is a
//! USE-detector and prose that names the thing it forbids stops distinguishing "used" from
//! "mentioned" — the lesson 52-02 paid for twice (`52-02-SUMMARY.md` deviation 7).
//!
//! # The shaping cache, and the trap it exists to avoid
//!
//! Shaping is not free. Re-shaping every clip label on every frame at 1,000 clips is the
//! obvious performance trap here, and it is invisible at 10 clips. So shaped
//! [`glyphon::Buffer`]s are cached by `(label, width_px, size_px)` and reused; a frame whose
//! text has not changed re-shapes nothing.
//!
//! The cache is BOUNDED (T-52-18). Its entries are keyed by strings that arrive across the
//! ABI, so an unbounded map is a memory-growth channel driven by input — the DoS shape this
//! phase's threat register names. See [`MAX_SHAPED_ENTRIES`].

use std::collections::HashMap;

use glyphon::{
    Attrs, Buffer, Cache, Color, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};

use crate::frame::{RudisTimelineClip, RudisTimelineFrame, RudisTimelineLane, RudisTimelineTick};
use crate::quads::Palette;

/// Type sizes, in LOGICAL px, transcribed from `design_handoff_rudis_editor/README.md:217`.
/// Multiplied by the frame's `scale` before shaping, so glyphs are rasterised at their
/// physical size rather than shaped small and scaled up (which is blurry — a failure mode
/// that reads as "the font is a bit soft" and therefore never gets filed).
mod type_scale {
    // The handoff's region-tag row ("10 / 800 / letter-spacing .08em / `text-faint`")
    // is deliberately ABSENT from this table: the tag lives in `Timeline › Toolbar`,
    // which is ordinary XAML, and those three numbers are now applied where the tag is
    // actually laid out (`Regions/Timeline.xaml`). See `TextPass::prepare` step 1 for
    // the measurement that moved it. Keeping dead constants here so a test could assert
    // them would be asserting a transcription nothing reads.

    /// Clip & lane labels: "clip & ruler labels **9-9.5** / 700". 9.5 for the ones that
    /// carry meaning a user reads (clip names, lane names).
    pub const LABEL_PX: f32 = 9.5;
    /// Ruler graduations take the bottom of the same range — they are read as a scale, not
    /// as words, and the smaller size fits more of them before the spacing floor bites.
    pub const RULER_PX: f32 = 9.0;
    pub const LABEL_WEIGHT: u16 = 700;
    /// cosmic-text needs a line height; 1.2x is the conventional default and the Timeline
    /// never wraps, so it only affects vertical centring.
    pub const LINE_HEIGHT_FACTOR: f32 = 1.2;
}

/// Upper bound on cached shaped buffers.
///
/// Sized as "a full screen of visible clips, plus their lane and ruler labels, plus several
/// screens of slack so scrolling back and forth is a cache hit". A 4K Timeline shows on the
/// order of 200 clips at once; 2,048 is an order of magnitude of headroom and still a hard
/// ceiling. On overflow the whole cache is dropped and rebuilt rather than evicted
/// one-by-one: an LRU needs per-entry bookkeeping on the hot path to save a re-shape that
/// only happens when the bound is genuinely exceeded, which for this workload is never.
const MAX_SHAPED_ENTRIES: usize = 2048;

/// The font stack, in the handoff's own order (README:216).
///
/// cosmic-text takes ONE family per `Attrs`, not a CSS-style list, so the stack is resolved
/// ONCE at construction against the real font database and the winner is kept. That is the
/// honest translation: asking for a family the machine does not have silently yields the
/// default sans-serif, and on this project's own target (Windows 10 19045) the difference
/// between Calibri and a default is visible.
const FONT_STACK: [&str; 2] = ["Calibri", "Segoe UI"];

/// The cache key, and it is deliberately `Copy`.
///
/// The obvious key is `(String, ...)` — and looking one up would then ALLOCATE a `String`
/// on every hit, i.e. once per label per frame, which is precisely the per-clip allocation
/// the whole design exists to avoid. So the text is represented by an FNV-1a-64 hash and
/// the entry keeps the real string for verification; a lookup then touches no allocator at
/// all.
///
/// FNV-1a-64 rather than `std`'s default hasher, for the reason 52-02 recorded when it
/// picked the same function for a cache FILENAME: the default hasher is not stable, and
/// while instability is only a cache miss here rather than an orphaned file, a hash that is
/// stable across builds makes the collision-verification path reproducible.
///
/// The width is bucketed to a whole px and the size to a tenth of a px: sub-pixel jitter in
/// either would make every key unique and turn the cache into a leak with extra steps.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ShapeKey {
    text_hash: u64,
    width_px: u32,
    size_tenths: u32,
    weight: u16,
}

/// FNV-1a-64. Same constants as `crates/waveform/src/cache.rs`'s.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// A shaped buffer and the exact text it was shaped from.
///
/// The text is kept so a hash COLLISION cannot silently draw one clip's label on another.
/// FNV-1a-64 over short filenames makes that vanishingly unlikely; "vanishingly unlikely,
/// and checked" costs one string comparison per hit and removes the failure mode entirely.
struct Shaped {
    text: String,
    buffer: Buffer,
}

/// A shaped buffer plus where and how to draw it this frame.
struct Placed {
    entry: usize,
    left: f32,
    top: f32,
    bounds: TextBounds,
    colour: Color,
}

pub struct TextPass {
    font_system: FontSystem,
    swash: SwashCache,
    atlas: TextAtlas,
    viewport: Viewport,
    renderer: TextRenderer,
    /// The shaped-buffer pool. Indices into it are stable for the life of a generation.
    entries: Vec<Shaped>,
    /// Key → index into `entries`.
    index: HashMap<ShapeKey, usize>,
    /// This frame's placements. Cleared per frame, capacity retained.
    placed: Vec<Placed>,
    /// The family actually found on this machine, resolved once from [`FONT_STACK`].
    family: String,
    /// Glyphs the last prepared frame produced.
    glyphs_drawn: u32,
}

impl TextPass {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> Self {
        let mut font_system = FontSystem::new();
        let family = resolve_family(&mut font_system);

        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        // ColorMode::Web, deliberately, and for the same reason quads.wgsl blends in sRGB
        // space: the surface is a NON-sRGB Bgra8Unorm target and the design handoff's own
        // source is an HTML mock. glyphon's own documentation describes this mode as
        // reproducing "the color management strategy used in the Web and implemented by
        // browsers" — not physically accurate, but identical to what the colours were
        // authored against, and identical to what the quad pass beside it does. The two
        // passes agreeing matters more than either being theoretically right: a mismatch
        // would make text and its background drift apart at every alpha blend.
        let mut atlas =
            TextAtlas::with_color_mode(device, queue, &cache, format, ColorMode::Web);
        let renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);

        Self {
            font_system,
            swash: SwashCache::new(),
            atlas,
            viewport,
            renderer,
            entries: Vec::new(),
            index: HashMap::new(),
            placed: Vec::new(),
            family,
            glyphs_drawn: 0,
        }
    }

    /// The font family this machine actually resolved to, for the record.
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Glyph count from the most recent [`Self::prepare`].
    pub fn glyphs_drawn(&self) -> u32 {
        self.glyphs_drawn
    }

    /// Number of shaped buffers currently cached — the bound's observable half.
    pub fn cached_shapes(&self) -> usize {
        self.index.len()
    }

    /// Shape and lay out every label in this frame, then hand them to glyphon.
    ///
    /// Returns the number of glyphs prepared.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &RudisTimelineFrame,
        lanes: &[RudisTimelineLane],
        clips: &[RudisTimelineClip],
        ticks: &[RudisTimelineTick],
        palette: &Palette,
    ) {
        let surface_w = frame.surface_w_px.max(1);
        let surface_h = frame.surface_h_px.max(1);
        self.viewport.update(
            queue,
            Resolution {
                width: surface_w,
                height: surface_h,
            },
        );

        let scale = if frame.scale.is_finite() && frame.scale > 0.0 {
            frame.scale
        } else {
            1.0
        };
        let gutter_w = finite_or(frame.gutter_w_px, 0.0).max(0.0);
        let header_h = finite_or(frame.header_h_px, 0.0).max(0.0);
        let ruler_h = finite_or(frame.ruler_h_px, 0.0).max(0.0);

        self.placed.clear();
        self.bound_the_cache();

        // 1. THE REGION TAG IS NOT DRAWN HERE. Deliberately, and corrected by
        //    MEASUREMENT rather than by reading the spec (plan 52-06).
        //
        //    This module used to place `REGION_TAG` in the gutter cell of the ruler
        //    band. The design handoff puts the tag in `Timeline › Toolbar`
        //    (README:132) — a 36px band of ORDINARY XAML above this surface — and once
        //    plan 52-06 built that toolbar, the shell drew the tag TWICE: once
        //    correctly at 10px/800/`text-faint` in the toolbar, and once here, clipped
        //    mid-glyph to a 46px gutter that has no room for it. The first launch
        //    screenshot shows the second one as two stray marks; a reviewer would read
        //    it as a rendering bug, and it is.
        //
        //    So the tag belongs to whichever layer can actually lay it out, and that
        //    layer is XAML. The type scale below keeps `REGION_TAG_PX`/`_WEIGHT`/
        //    `_LETTER_SPACING_EM` because they are the handoff's numbers for this
        //    region and `type_scale_matches_the_handoff` still pins them.
        //
        //    `gutter_w`, `header_h` and `ruler_h` are still read above: the lane
        //    labels and the ruler labels below all place against them.
        let _ = (gutter_w, header_h, ruler_h);

        // 2. Lane labels, INSIDE the gutter (`V1`, `A1`).
        let label_size = type_scale::LABEL_PX * scale;
        let lane_colour = colour(palette.raw.text_secondary);
        for lane in lanes {
            // SAFETY: `label_checked` is the crate's ONE validating decoder — bounded,
            // `str::from_utf8`, and `""` on any failure. `lane` came out of a slice
            // `abi::slice_checked` already bounded, so the pointer pair inside it is the
            // only thing left to validate and this is where that happens.
            let text = unsafe { crate::abi::label_checked(lane.label_ptr, lane.label_len) };
            if text.is_empty() || !lane.y_px.is_finite() || !lane.h_px.is_finite() {
                continue;
            }
            self.place(
                text,
                gutter_w.max(1.0),
                label_size,
                type_scale::LABEL_WEIGHT,
                None,
                4.0 * scale,
                lane.y_px + (lane.h_px - label_size * type_scale::LINE_HEIGHT_FACTOR).max(0.0) * 0.5,
                TextBounds {
                    left: 0,
                    top: lane.y_px as i32,
                    right: gutter_w as i32,
                    bottom: (lane.y_px + lane.h_px) as i32,
                },
                lane_colour,
            );
        }

        // 3. Ruler tick labels.
        let ruler_size = type_scale::RULER_PX * scale;
        let tick_colour = colour(palette.raw.text_faint);
        for tick in ticks {
            // SAFETY: see the lane loop above.
            let text = unsafe { crate::abi::label_checked(tick.label_ptr, tick.label_len) };
            if text.is_empty() || !tick.x_px.is_finite() {
                continue;
            }
            // A tick label that would paint under the sticky gutter is dropped rather than
            // clipped: half a timecode is worse than none.
            if tick.x_px < gutter_w {
                continue;
            }
            self.place(
                text,
                surface_w as f32,
                ruler_size,
                type_scale::LABEL_WEIGHT,
                None,
                tick.x_px + 3.0 * scale,
                header_h + (ruler_h - ruler_size * type_scale::LINE_HEIGHT_FACTOR).max(0.0) * 0.5,
                TextBounds {
                    left: gutter_w as i32,
                    top: header_h as i32,
                    right: surface_w as i32,
                    bottom: (header_h + ruler_h) as i32,
                },
                tick_colour,
            );
        }

        // 4. Clip labels — LEFT-aligned, dark-on-fill, clipped to D-01's TITLE BAND minus
        //    both trim handles. A label must never paint outside its own clip, it must
        //    never paint under a handle the user is about to grab, and from plan 53.2-03 it
        //    must never paint into the body, because the body belongs to the filmstrip.
        //
        //    ELIDING TO FIT IS THIS LAYER'S JOB, and that is D-01's explicit assignment
        //    rather than a convenience: only this module knows the glyphon font metrics, so
        //    `ClipLayout.Label` crosses the ABI as the whole display name, untruncated, and
        //    the `TextBounds` below is what actually cuts it off.
        //
        //    `band_h_px <= 0` restores the pre-phase placement exactly — the label centres
        //    in the whole clip rect, as it did before there was a band to put it in.
        let band_h = frame.band_h_px;
        for clip in clips {
            // SAFETY: see the lane loop above.
            let text = unsafe { crate::abi::label_checked(clip.label_ptr, clip.label_len) };
            if text.is_empty()
                || !clip.x_px.is_finite()
                || !clip.y_px.is_finite()
                || !clip.w_px.is_finite()
                || !clip.h_px.is_finite()
                || clip.w_px <= 0.0
                || clip.h_px <= 0.0
            {
                continue;
            }
            let handle = if clip.trim_handle_px.is_finite() && clip.trim_handle_px > 0.0 {
                clip.trim_handle_px.min(clip.w_px * 0.5)
            } else {
                0.0
            };
            let text_left = clip.x_px + handle + 2.0 * scale;
            let text_right = clip.x_px + clip.w_px - handle;
            let avail = text_right - text_left;
            if avail <= 1.0 {
                continue;
            }
            // The band when there is one, the whole clip when there is not. `split_band`
            // clamps the band to the clip's own height, so a clip shorter than the band
            // still gets its label — D-04: the band always draws.
            let (band, _body) =
                crate::frame::split_band(clip.x_px, clip.y_px, clip.w_px, clip.h_px, band_h);
            let (row_y, row_h) = if band[3] > 0.0 {
                (band[1], band[3])
            } else {
                (clip.y_px, clip.h_px)
            };
            // The handoff: "Clip name labels use a dark on-color text matching the fill
            // family" (README:213). The C# side already darkened the fill into `border`;
            // reusing it is what keeps this crate free of colour arithmetic as well as of
            // colour literals.
            self.place(
                text,
                avail,
                label_size,
                type_scale::LABEL_WEIGHT,
                None,
                text_left,
                row_y + (row_h - label_size * type_scale::LINE_HEIGHT_FACTOR).max(0.0) * 0.5,
                TextBounds {
                    left: text_left as i32,
                    top: row_y as i32,
                    right: text_right as i32,
                    bottom: (row_y + row_h) as i32,
                },
                colour(clip.border),
            );
        }

        // Split the borrow so `prepare` can hold `&mut font_system` and `&entries` at once.
        let Self {
            font_system,
            swash,
            atlas,
            viewport,
            renderer,
            entries,
            placed,
            ..
        } = self;

        let areas = placed.iter().map(|p| TextArea {
            buffer: &entries[p.entry].buffer,
            left: p.left,
            top: p.top,
            // 1.0: the buffers were already SHAPED at physical size (metrics are multiplied
            // by the frame's scale above), so scaling here again would double-apply DPI.
            scale: 1.0,
            bounds: p.bounds,
            default_color: p.colour,
            custom_glyphs: &[],
        });

        match renderer.prepare(device, queue, font_system, atlas, viewport, areas, swash) {
            Ok(()) => {}
            Err(e) => {
                // A prepare failure is an atlas-full or a removed-from-atlas condition, not
                // a reason to fail the frame: the quads have already been assembled and the
                // Timeline is still usable without a label. Trace it and draw no text.
                crate::trace(&format!("text:prepare-failed err={e:?}"));
                self.glyphs_drawn = 0;
                return;
            }
        }

        self.glyphs_drawn = count_glyphs(&self.placed, &self.entries);
    }

    /// Draw the prepared text. Runs AFTER every quad in the same pass, which is what makes
    /// labels sit above their own backgrounds without any ordering discipline.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.placed.is_empty() {
            return;
        }
        if let Err(e) = self.renderer.render(&self.atlas, &self.viewport, pass) {
            crate::trace(&format!("text:render-failed err={e:?}"));
        }
    }

    /// Free atlas space glyphon no longer needs. Cheap, and skipping it lets the atlas grow
    /// without bound across a long session.
    pub fn trim(&mut self) {
        self.atlas.trim();
    }

    /// Shape (or reuse) one label and record where it goes.
    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        text: &str,
        width_px: f32,
        size_px: f32,
        weight: u16,
        letter_spacing_px: Option<f32>,
        left: f32,
        top: f32,
        bounds: TextBounds,
        colour: Color,
    ) {
        if text.is_empty() || !size_px.is_finite() || size_px <= 0.0 {
            return;
        }
        if !left.is_finite() || !top.is_finite() || !width_px.is_finite() || width_px <= 0.0 {
            return;
        }

        // Allocation-free on a hit: the key is Copy, and the only String involved is the
        // one already stored in the entry.
        let key = ShapeKey {
            text_hash: fnv1a64(text.as_bytes()),
            width_px: width_px as u32,
            size_tenths: (size_px * 10.0) as u32,
            weight,
        };

        let hit = self
            .index
            .get(&key)
            .copied()
            // Verify the text, not just the hash. See `Shaped`.
            .filter(|i| self.entries.get(*i).is_some_and(|e| e.text == text));

        let entry = match hit {
            Some(i) => i,
            None => {
                let metrics = Metrics::new(size_px, size_px * type_scale::LINE_HEIGHT_FACTOR);
                let mut buffer = Buffer::new(&mut self.font_system, metrics);
                buffer.set_size(&mut self.font_system, Some(width_px), None);
                let mut attrs = Attrs::new()
                    .family(Family::Name(&self.family))
                    .weight(Weight(weight));
                if let Some(ls) = letter_spacing_px {
                    attrs = attrs.letter_spacing(ls);
                }
                buffer.set_text(&mut self.font_system, text, &attrs, Shaping::Advanced, None);
                buffer.shape_until_scroll(&mut self.font_system, false);
                self.entries.push(Shaped {
                    text: text.to_owned(),
                    buffer,
                });
                let i = self.entries.len() - 1;
                self.index.insert(key, i);
                i
            }
        };

        self.placed.push(Placed {
            entry,
            left,
            top,
            bounds,
            colour,
        });
    }

    /// Enforce [`MAX_SHAPED_ENTRIES`] (T-52-18).
    ///
    /// Called at the START of a frame, never in the middle: dropping entries while `placed`
    /// still holds indices into them would invalidate those indices, and the resulting
    /// mis-index would draw the wrong label rather than fail.
    fn bound_the_cache(&mut self) {
        if self.index.len() <= MAX_SHAPED_ENTRIES {
            return;
        }
        crate::trace(&format!(
            "text:shape-cache-reset entries={}",
            self.index.len()
        ));
        self.index.clear();
        self.entries.clear();
    }
}

/// `0xAARRGGBB` → glyphon's packed colour. Same channel order, different container.
#[inline]
fn colour(argb: u32) -> Color {
    Color::rgba(
        ((argb >> 16) & 0xFF) as u8,
        ((argb >> 8) & 0xFF) as u8,
        (argb & 0xFF) as u8,
        ((argb >> 24) & 0xFF) as u8,
    )
}

#[inline]
fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

fn count_glyphs(placed: &[Placed], entries: &[Shaped]) -> u32 {
    let mut n = 0u32;
    for p in placed {
        if let Some(entry) = entries.get(p.entry) {
            for run in entry.buffer.layout_runs() {
                n = n.saturating_add(run.glyphs.len() as u32);
            }
        }
    }
    n
}

/// Walk [`FONT_STACK`] against the real font database and keep the first family present.
///
/// The handoff writes `Calibri, "Segoe UI", system-ui, sans-serif` — a CSS stack.
/// cosmic-text's `Attrs` takes one family, and an absent family degrades silently to the
/// default sans-serif, so "ask for Calibri and hope" would produce a different-looking
/// Timeline on a machine without it with nothing to indicate why. Resolving once, up front,
/// makes the answer inspectable through [`TextPass::family`].
fn resolve_family(font_system: &mut FontSystem) -> String {
    let db = font_system.db();
    for candidate in FONT_STACK {
        let found = db
            .faces()
            .any(|f| f.families.iter().any(|(name, _)| name == candidate));
        if found {
            return candidate.to_owned();
        }
    }
    // The stack's own final fallback. cosmic-text resolves this to the platform default.
    crate::trace("text:font-stack-missed falling back to sans-serif");
    "sans-serif".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_preserves_argb_channel_order() {
        let c = colour(0x8040_2010);
        assert_eq!(c.r(), 0x40);
        assert_eq!(c.g(), 0x20);
        assert_eq!(c.b(), 0x10);
        assert_eq!(c.a(), 0x80);
    }

    #[test]
    fn the_shape_cache_bound_is_a_real_number_not_a_comment() {
        // T-52-18's mitigation is a CEILING, and a ceiling nobody asserts drifts to
        // "usize::MAX" the first time someone hits it in a profile.
        assert!(MAX_SHAPED_ENTRIES > 0);
        assert!(MAX_SHAPED_ENTRIES <= 8192, "a bound this large is not a bound");
    }

    #[test]
    fn the_font_stack_is_the_handoffs_own_order() {
        assert_eq!(FONT_STACK[0], "Calibri");
        assert_eq!(FONT_STACK[1], "Segoe UI");
    }

    #[test]
    fn type_sizes_match_the_design_handoff() {
        // README:217 — "clip & ruler labels 9-9.5 / 700". A transcription is a claim,
        // and an untested constant drifts. (The region-tag row moved to XAML with the
        // tag itself — see the note at the top of `type_scale`.)
        assert_eq!(type_scale::LABEL_PX, 9.5);
        assert_eq!(type_scale::RULER_PX, 9.0);
        assert_eq!(type_scale::LABEL_WEIGHT, 700);
        assert!(type_scale::RULER_PX >= 9.0 && type_scale::LABEL_PX <= 9.5);
    }

    #[test]
    fn the_shape_key_is_copy_so_a_cache_hit_cannot_allocate() {
        // The property, not the type: a key that owned its text would allocate a String on
        // every lookup — once per label per frame — which is exactly the per-clip
        // allocation this crate is built to avoid. `Copy` is the compiler-checkable
        // statement of "constructing this key touches no allocator".
        fn assert_copy<T: Copy>() {}
        assert_copy::<ShapeKey>();
    }

    #[test]
    fn fnv1a64_is_the_documented_function_and_distinguishes_real_labels() {
        // The offset basis, pinned: a hash that silently changes is a cache that silently
        // empties, and this one is compared against a stored string so a change is a
        // performance cliff rather than a correctness bug — the kind nobody notices.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a64(b"Beach_01.mp4"), fnv1a64(b"Beach_02.mp4"));
        assert_ne!(fnv1a64(b"V1"), fnv1a64(b"A1"));
        assert_eq!(fnv1a64(b"Drone_pan.mov"), fnv1a64(b"Drone_pan.mov"));
    }
}
