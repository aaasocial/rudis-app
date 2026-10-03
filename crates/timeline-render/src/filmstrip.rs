//! Filmstrip tile emission — D-05, D-06, D-07, D-08 and D-13, as arithmetic.
//!
//! # The shape of it, and why almost none of it touches a GPU
//!
//! A strip is cached ONCE PER MEDIA at ONE fixed source interval (D-06/D-07). A clip is a
//! WINDOW into that strip. So everything the user does at the Timeline — zooming, trimming,
//! splitting, duplicating — is a different selection of already-decoded tiles, and costs
//! **zero decode and zero I/O**. That is the whole design, and it is why the interesting
//! part of this file is three pure functions rather than a pipeline:
//!
//! * [`tile_count_for`] — D-05's edge-to-edge tiling at the thumbnail's natural width,
//!   with the mandatory per-clip cap.
//! * [`tile_index_for`] — D-06/D-07's nearest-cached-frame selection across the clip's
//!   `[in, in + dur)` source window. `waveform::slice_range`'s clamping discipline applied
//!   to tiles, because the numbers come from a cache file and **a cache file is untrusted
//!   input however carefully it was written** (T-52-37, recurring here as T-53.2-18).
//! * [`validated_strip`] — the geometry and the byte length checked AS A UNIT, before any
//!   slice exists.
//!
//! # Two bounds, and the second is not redundant
//!
//! [`MAX_TILES_PER_CLIP_DRAW`] stops one deeply-zoomed clip asking for thousands of tiles;
//! [`MAX_FILMSTRIP_QUADS_PER_FRAME`] stops a thousand clips each legally asking for 64.
//! Exactly the pair `waveform.rs` carries, for exactly its reasons — the per-clip cap alone
//! is not a frame bound, and a frame that draws fewer tiles is a frame, where a frame that
//! runs out of memory is a dead Timeline. Both truncate GRACEFULLY and REPORT it in the
//! stats (T-53.2-20).
//!
//! # This crate still holds no colour
//!
//! Nothing here picks a pixel. Tile bytes are the media's own; the letterbox gaps beside a
//! vertical clip's thumbnail (D-08) are transparent in the sheet and show the clip's body
//! fill through, which is a token the C# side resolved by name. CLAUDE.md convention 7
//! survives a GPU boundary because no colour ever crosses it as a literal.

use crate::atlas::{FilmstripAtlas, SheetUv};
use crate::frame::{RudisTimelineClip, FLAG_STRIP_PLACEHOLDER};
use crate::texquad::{push_tex_into, TexQuadInstance};
use crate::waveform::{InnerRect, WaveformPass};

/// Tiles ONE clip may draw, however wide it is zoomed.
///
/// **A DoS bound, not a quality setting**, and the one D-05 calls mandatory. At
/// `TimelineMetrics.MaxPxPerSecond = 600` a ten-minute clip is 360,000 physical px wide and
/// an uncapped `floor(w / 60)` would ask for 6,000 instances from a single clip. When the
/// cap binds the tiles WIDEN to `w / 64` rather than leaving the body half-drawn.
///
/// 64 is `TimelineMetrics.MaxFilmstripTilesPerClip` — the two sides agree BY VALUE and each
/// is documented as the other's mirror. They are deliberately not derived from one another:
/// the C# side needs the number to decide how much strip to ask for, this side needs it to
/// decide how much to draw, and a shared constant would mean a new ABI field for a number
/// that has changed zero times.
pub const MAX_TILES_PER_CLIP_DRAW: usize = 64;

/// Filmstrip tiles one FRAME may emit, across all clips.
///
/// **The other half of the same bound (T-53.2-20).** 8,192 is 64 tiles for each of ~128
/// fully-tiled clips — comfortably more than a 1080p viewport can show at any zoom where
/// tiles are still legible, and four orders of magnitude below a memory problem. Past it,
/// remaining clips draw their flat body colour and the frame's stats record the truncation,
/// so the degradation is a number anyone can read rather than a mystery.
pub const MAX_FILMSTRIP_QUADS_PER_FRAME: u32 = 8_192;

// ---------------------------------------------------------------------------
// THE GEOMETRY BOUNDS. Restated from `crates/filmstrip::cache`'s own constants,
// deliberately rather than shared: that crate is the PRODUCER and this one is a
// CONSUMER on the far side of a C# hop and a disk file. A bound that only the
// producer knows is a bound one edit away from being absent, which is the same
// argument `abi::MAX_PEAK_BYTES` already makes for the peak cache.
// ---------------------------------------------------------------------------

/// Minimum tile cell dimension, px.
pub const MIN_TILE_DIM: u32 = 1;
/// Maximum tile cell dimension, px. `crates/filmstrip` writes 96×54; a poster placeholder
/// carries its own, larger dims (D-13), which is why this is not pinned at the tile size.
pub const MAX_TILE_DIM: u32 = 512;
/// Minimum tiles per sheet row.
pub const MIN_TILES_PER_ROW: u32 = 1;
/// Maximum tiles per sheet row. `crates/filmstrip::TILES_PER_ROW` is 16.
pub const MAX_TILES_PER_ROW: u32 = 64;
/// Minimum tiles in a strip.
pub const MIN_TOTAL_TILES: u32 = 1;
/// Maximum tiles in a strip. `crates/filmstrip::FILMSTRIP_MAX_TILES` is 256.
pub const MAX_TOTAL_TILES: u32 = 4096;
/// Maximum sheet bytes for one clip. The real worst case is a full 1536×864 sheet at
/// 5.06 MiB; this is the ceiling a hostile length is rejected against, three times that.
pub const MAX_STRIP_BYTES: u32 = 16 * 1024 * 1024;

/// One clip's strip fields, VALIDATED as a unit and turned into a bounded slice.
///
/// Constructing this is the only way to reach the sheet bytes, so "the geometry was checked
/// before the pointer was read" is a property of the type rather than a discipline anyone
/// has to remember — the same idea `FrameView` uses at the ABI boundary, one level down.
#[derive(Clone, Copy, Debug)]
pub struct StripView<'a> {
    pub bytes: &'a [u8],
    pub tile_w: u32,
    pub tile_h: u32,
    pub tiles_per_row: u32,
    pub total_tiles: u32,
    pub completed_tiles: u32,
    pub interval_us: i64,
    pub key: u64,
    /// `tiles_per_row * tile_w`.
    pub sheet_w: u32,
    /// The height of the WHOLE grid — what the atlas allocates, so D-14 growth re-uploads
    /// into the rect it already has.
    pub sheet_h_total: u32,
    /// Sheet rows the `completed_tiles` occupy. The payload is WHOLE rows, always.
    pub rows_valid: u32,
}

impl StripView<'_> {
    /// The valid payload's height in px — `rows_valid` whole sheet rows.
    pub fn rows_valid_h(&self) -> u32 {
        self.rows_valid * self.tile_h
    }
}

/// What one clip's tiles did — emitted, and whether the frame budget cut it short.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileOutcome {
    pub tiles: u32,
    pub truncated: bool,
}

/// D-05: tiles at the thumbnail's NATURAL width, edge to edge, capped.
///
/// Returns `(count, draw_w)`. `draw_w` is `body_w / count`, so the tiles always span the
/// body exactly — at the natural width when the count is natural, and WIDENED when the cap
/// binds. Widening rather than leaving a gap is the point: a capped clip should look like a
/// coarse filmstrip, not like a filmstrip that stopped.
///
/// A width narrower than one tile yields ONE squeezed tile rather than zero. The floor
/// below which the body shows a solid fill instead is D-04's, and it lives on the C# side
/// (`TimelineMetrics.FilmstripWidthFloorPx`) because that is where the clip's width is
/// decided; by the time a strip reaches here the clip is wide enough to show something.
///
/// `f32::INFINITY` saturates to the cap rather than to zero, exactly as
/// `waveform::bar_count_for` does: an infinite width is a geometry bug upstream, and the
/// clip's own rectangle is filtered separately.
pub fn tile_count_for(body_w_px: f32, tile_draw_w_px: f32) -> (usize, f32) {
    if body_w_px.is_nan() || body_w_px <= 0.0 {
        return (0, 0.0);
    }
    if body_w_px == f32::INFINITY {
        return (MAX_TILES_PER_CLIP_DRAW, f32::INFINITY);
    }

    // A degenerate tile width cannot become a divisor. One tile across the body is the
    // honest answer: something is drawn, and it is the clip's own frame.
    let count = if !tile_draw_w_px.is_finite() || tile_draw_w_px <= 0.0 {
        1
    } else {
        let natural = (body_w_px / tile_draw_w_px).floor();
        if !(natural >= 1.0) {
            1
        } else {
            (natural as usize).min(MAX_TILES_PER_CLIP_DRAW)
        }
    };

    (count, body_w_px / count as f32)
}

/// D-06 + D-07: which CACHED tile does tile position `tile_pos` show?
///
/// The strip belongs to the MEDIA and holds a frame every `interval_us`. The clip owns a
/// window `[clip_in_us, clip_in_us + clip_dur_us)` into that media. Position `tile_pos` of
/// `tile_count` therefore sits at `clip_in + tile_pos * dur / tile_count` in SOURCE time,
/// and the tile shown is the nearest cached one to it. Left-edge sampled, not centred, so
/// the leftmost tile is exactly the clip's in-point — which is what a user checks first
/// after a trim.
///
/// Zooming changes `tile_count` and nothing else. Trimming changes `clip_in`/`clip_dur` and
/// nothing else. Neither reaches a decoder, which is D-06's whole claim.
///
/// # Every degenerate input resolves, and none of them panics
///
/// `slice_range`'s discipline, restated for tiles. Nothing completed, a non-positive
/// interval (which would divide by zero), a position past the count, and a zero count all
/// yield `None`. A negative in-point, a negative duration, and an index past the completed
/// tail all CLAMP. The arithmetic runs in `i128` so a `clip_in_us` of `i64::MAX` beside a
/// `clip_dur_us` of `i64::MAX` cannot wrap into a small plausible-looking index.
pub fn tile_index_for(
    clip_in_us: i64,
    clip_dur_us: i64,
    interval_us: i64,
    completed_tiles: u32,
    tile_pos: usize,
    tile_count: usize,
) -> Option<u32> {
    if completed_tiles == 0 || interval_us <= 0 || tile_count == 0 || tile_pos >= tile_count {
        return None;
    }

    let in_us = clip_in_us.max(0) as i128;
    let dur_us = clip_dur_us.max(0) as i128;
    let offset = dur_us * tile_pos as i128 / tile_count as i128;
    let t_us = in_us + offset;

    // NEAREST, not floor: at an interval of 1s a tile landing at 1.9s should show the 2s
    // frame, not the 1s one. Half-interval rounding is the whole of "nearest".
    let interval = interval_us as i128;
    let idx = (t_us + interval / 2) / interval;

    Some(idx.clamp(0, (completed_tiles - 1) as i128) as u32)
}

/// Validate one clip's strip fields AS A UNIT and bound the sheet slice — T-53.2-18.
///
/// Every check happens BEFORE a slice exists, and the length is **proven from the
/// geometry** rather than trusted: the payload is `ceil(completed / tiles_per_row)` whole
/// sheet rows, each `tiles_per_row * tile_w * 4` bytes wide and `tile_h` rows tall, which is
/// exactly what `crates/filmstrip::cache` writes. A header claiming more rows than its
/// bytes hold is how an out-of-bounds read gets dressed up as a filmstrip.
///
/// The expected size is computed in `u64`, so a hostile combination cannot overflow into a
/// small number that happens to match a `u32` `strip_len`. `abi::slice_checked` then applies
/// its own ordered null/bound checks, and the resulting slice's length is compared against
/// the expectation a second time — belt and braces, because this is the one function
/// standing between a cache file and a `from_raw_parts`.
///
/// A malformed strip does NOT fail the frame. The clip draws its flat body fill, which is
/// byte-identical to D-04's degraded state and to the not-yet-extracted state — the three
/// are deliberately the same drawing.
pub fn validated_strip(clip: &RudisTimelineClip) -> Option<StripView<'_>> {
    if clip.strip_len == 0 || clip.strip_ptr.is_null() {
        return None;
    }
    if !(MIN_TILE_DIM..=MAX_TILE_DIM).contains(&clip.strip_tile_w)
        || !(MIN_TILE_DIM..=MAX_TILE_DIM).contains(&clip.strip_tile_h)
        || !(MIN_TILES_PER_ROW..=MAX_TILES_PER_ROW).contains(&clip.strip_tiles_per_row)
        || !(MIN_TOTAL_TILES..=MAX_TOTAL_TILES).contains(&clip.strip_total_tiles)
    {
        return None;
    }
    if clip.strip_completed_tiles == 0 || clip.strip_completed_tiles > clip.strip_total_tiles {
        return None;
    }

    let rows_valid = clip.strip_completed_tiles.div_ceil(clip.strip_tiles_per_row);
    let rows_total = clip.strip_total_tiles.div_ceil(clip.strip_tiles_per_row);
    // Both fit u32 comfortably at the bounds above (64 * 512 = 32,768 wide;
    // 4,096 * 512 = 2,097,152 tall), but the multiply is checked anyway — the bounds and
    // the arithmetic that depends on them are edited by different people at different times.
    let sheet_w = clip.strip_tiles_per_row.checked_mul(clip.strip_tile_w)?;
    let sheet_h_total = rows_total.checked_mul(clip.strip_tile_h)?;

    let expected = rows_valid as u64 * clip.strip_tile_h as u64 * sheet_w as u64 * 4;
    if expected != clip.strip_len as u64 {
        return None;
    }

    // SAFETY: the ABI contract is that a non-null `strip_ptr` addresses at least `strip_len`
    // bytes for the duration of the render call that carried it. `slice_checked` applies the
    // bound before the null check before the `from_raw_parts`, in that order, for the reason
    // it documents.
    let bytes = unsafe { crate::abi::slice_checked::<u8>(clip.strip_ptr, clip.strip_len, MAX_STRIP_BYTES) }
        .ok()?;
    if bytes.len() as u64 != expected {
        return None;
    }

    Some(StripView {
        bytes,
        tile_w: clip.strip_tile_w,
        tile_h: clip.strip_tile_h,
        tiles_per_row: clip.strip_tiles_per_row,
        total_tiles: clip.strip_total_tiles,
        completed_tiles: clip.strip_completed_tiles,
        interval_us: clip.strip_interval_us,
        key: clip.strip_key,
        sheet_w,
        sheet_h_total,
        rows_valid,
    })
}

/// One tile's uv sub-rect inside a RESIDENT sheet.
///
/// The sheet's own dimensions come from `sheet` — the ATLAS's record of what it allocated
/// and uploaded — not from the frame struct alone. Those two can legitimately disagree for
/// a frame (a grown strip whose re-upload was capped), and computing a sub-rect from the
/// frame's newer geometry against the atlas's older allocation is precisely how a tile ends
/// up sampling the neighbouring media's pixels.
///
/// Every offset is clamped to the sheet, so no tile index — including one the atlas has
/// never heard of — can produce a uv outside the strip's own rectangle.
pub fn tile_uv(sheet: &SheetUv, strip: &StripView<'_>, tile: u32) -> [f32; 4] {
    let per_row = strip.tiles_per_row.max(1);
    let row = tile / per_row;
    let col = tile % per_row;

    let px0 = col.saturating_mul(strip.tile_w).min(sheet.sheet_w);
    let px1 = px0.saturating_add(strip.tile_w).min(sheet.sheet_w);
    let py0 = row.saturating_mul(strip.tile_h).min(sheet.sheet_h_total);
    let py1 = py0.saturating_add(strip.tile_h).min(sheet.sheet_h_total);

    let du = sheet.uv[2] - sheet.uv[0];
    let dv = sheet.uv[3] - sheet.uv[1];
    let sw = sheet.sheet_w.max(1) as f32;
    let sh = sheet.sheet_h_total.max(1) as f32;

    [
        sheet.uv[0] + du * px0 as f32 / sw,
        sheet.uv[1] + dv * py0 as f32 / sh,
        sheet.uv[0] + du * px1 as f32 / sw,
        sheet.uv[1] + dv * py1 as f32 / sh,
    ]
}

/// Push one clip's tiles into a caller-owned instance list.
///
/// `sheet` is the atlas's record of the RESIDENT strip; `body` is the clip body inset by its
/// border (so a tile never paints over the 1px rule that separates one clip from the next,
/// nor over D-01's title band); `budget` is the frame's remaining allowance, decremented in
/// place.
///
/// A clip that cannot have ALL its tiles draws NONE of them, mirroring
/// `waveform::emit_bars`: a half-drawn filmstrip reads as a clip whose frames ran out, which
/// is a wrong picture, where an untiled clip reads as "not drawn yet" — which is the truth.
pub fn emit_tiles(
    clip: &RudisTimelineClip,
    strip: &StripView<'_>,
    sheet: &SheetUv,
    body: InnerRect,
    out: &mut Vec<TexQuadInstance>,
    budget: &mut u32,
) -> TileOutcome {
    if !body.is_drawable() {
        return TileOutcome::default();
    }

    // D-13: the poster placeholder is ONE quad stretched across the whole body. The aspect
    // is deliberately not preserved — a recognisable stretched poster beats a
    // correct-aspect sliver, and the transition to real frames is then
    // strip-replaces-poster rather than colour-replaces-nothing.
    if clip.flags & FLAG_STRIP_PLACEHOLDER != 0 {
        if *budget == 0 {
            return TileOutcome {
                tiles: 0,
                truncated: true,
            };
        }
        return if push_tex_into(out, body.x, body.y, body.w, body.h, tile_uv(sheet, strip, 0)) {
            *budget -= 1;
            TileOutcome {
                tiles: 1,
                truncated: false,
            }
        } else {
            TileOutcome::default()
        };
    }

    // The tile's NATURAL draw width: as tall as the body, at the sheet cell's own aspect.
    // Taken from the strip rather than from a constant so a placeholder-shaped sheet or a
    // future cell size needs no second opinion here.
    let tile_draw_w = body.h * strip.tile_w as f32 / strip.tile_h.max(1) as f32;
    let (count, draw_w) = tile_count_for(body.w, tile_draw_w);
    if count == 0 {
        return TileOutcome::default();
    }

    if count as u32 > *budget {
        return TileOutcome {
            tiles: 0,
            truncated: true,
        };
    }

    let mut emitted = 0u32;
    for pos in 0..count {
        // `sheet.completed_tiles`, NOT `strip.completed_tiles`: the atlas reports what it
        // actually uploaded, and clamping against the frame's newer number would sample
        // atlas rows nobody has written yet.
        let Some(idx) = tile_index_for(
            clip.clip_in_us,
            clip.clip_dur_us,
            strip.interval_us,
            sheet.completed_tiles,
            pos,
            count,
        ) else {
            break;
        };
        let x = body.x + pos as f32 * draw_w;
        if push_tex_into(out, x, body.y, draw_w, body.h, tile_uv(sheet, strip, idx)) {
            emitted += 1;
        }
    }

    *budget -= emitted;
    TileOutcome {
        tiles: emitted,
        truncated: false,
    }
}

/// The filmstrip pass: the per-frame counters and the clip loop. No pipeline, no buffer —
/// tiles are instances, so the list they go into belongs to [`crate::texquad::TexQuadPass`].
#[derive(Debug, Default)]
pub struct FilmstripPass {
    quads_drawn: u32,
    truncated_clips: u32,
    budget: u32,
}

impl FilmstripPass {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tiles emitted by the most recent assembly.
    pub fn quads_drawn(&self) -> u32 {
        self.quads_drawn
    }

    /// Clips the frame budget cut short.
    pub fn truncated_clips(&self) -> u32 {
        self.truncated_clips
    }

    /// Reset the per-frame counters and the budget.
    pub fn begin(&mut self) {
        self.quads_drawn = 0;
        self.truncated_clips = 0;
        self.budget = MAX_FILMSTRIP_QUADS_PER_FRAME;
    }

    /// The clip loop, with residency INJECTED.
    ///
    /// Split from [`Self::assemble`] so the whole emission path — validation, the budget,
    /// the placeholder branch, the window mapping — runs under plain `cargo test` on a
    /// machine with no adapter. `crates/waveform::extract_peaks_with` is this repository's
    /// own precedent for the shape (recorded in PROVENANCE Entry 31), and it is the same
    /// argument `emit_bars` makes: the part that can be wrong in a way a screenshot cannot
    /// show is the part that must be assertable without a screenshot.
    pub fn assemble_with<F>(
        &mut self,
        out: &mut Vec<TexQuadInstance>,
        clips: &[RudisTimelineClip],
        inset_px: f32,
        band_h_px: f32,
        mut resolve: F,
    ) where
        F: FnMut(&StripView<'_>) -> Option<SheetUv>,
    {
        for clip in clips {
            let Some(strip) = validated_strip(clip) else {
                continue;
            };

            let body = WaveformPass::body_inner_rect(clip, inset_px, band_h_px);
            if !body.is_drawable() {
                continue;
            }

            // Not resident: the atlas is full of strips already drawn this frame, or the
            // upload cap is spent, or the sheet does not fit. The clip draws its flat body
            // fill — D-16's own stated fallback, and the same picture as D-04's degraded
            // state. NOT a truncation: the budget was never the thing that refused it, and
            // conflating the two would make the truncation counter unreadable.
            let Some(sheet) = resolve(&strip) else {
                continue;
            };

            let outcome = emit_tiles(clip, &strip, &sheet, body, out, &mut self.budget);
            if outcome.truncated {
                self.truncated_clips = self.truncated_clips.saturating_add(1);
            }
            self.quads_drawn = self.quads_drawn.saturating_add(outcome.tiles);
        }
    }

    pub fn assemble(
        &mut self,
        out: &mut Vec<TexQuadInstance>,
        atlas: &mut FilmstripAtlas,
        queue: &wgpu::Queue,
        clips: &[RudisTimelineClip],
        inset_px: f32,
        band_h_px: f32,
    ) {
        self.assemble_with(out, clips, inset_px, band_h_px, |strip| {
            atlas.ensure_resident(
                queue,
                strip.key,
                strip.bytes,
                strip.sheet_w,
                strip.sheet_h_total,
                strip.completed_tiles,
                strip.rows_valid_h(),
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `crates/filmstrip`'s real grid, restated as fixture numbers.
    const T_W: u32 = 96;
    const T_H: u32 = 54;
    const PER_ROW: u32 = 16;

    fn sheet_bytes(tiles: u32, tile_w: u32, tile_h: u32, per_row: u32) -> Vec<u8> {
        let rows = tiles.div_ceil(per_row);
        vec![0u8; (rows * tile_h * per_row * tile_w * 4) as usize]
    }

    /// A clip carrying a WELL-FORMED strip. Zeroed first, so any field a test does not
    /// name is provably not consulted.
    fn strip_clip(bytes: &[u8], total: u32, completed: u32) -> RudisTimelineClip {
        let mut c: RudisTimelineClip = unsafe { std::mem::zeroed() };
        c.x_px = 100.0;
        c.y_px = 200.0;
        c.w_px = 300.0;
        c.h_px = 48.0;
        c.clip_in_us = 0;
        c.clip_dur_us = 10_000_000;
        c.strip_tile_w = T_W;
        c.strip_tile_h = T_H;
        c.strip_tiles_per_row = PER_ROW;
        c.strip_total_tiles = total;
        c.strip_completed_tiles = completed;
        c.strip_interval_us = 1_000_000;
        c.strip_key = 0xFEED_BEEF;
        c.strip_ptr = bytes.as_ptr();
        c.strip_len = bytes.len() as u32;
        c
    }

    fn fake_sheet(strip: &StripView<'_>) -> SheetUv {
        SheetUv {
            uv: [0.0, 0.0, strip.sheet_w as f32 / 4096.0, strip.sheet_h_total as f32 / 4096.0],
            sheet_w: strip.sheet_w,
            sheet_h_total: strip.sheet_h_total,
            completed_tiles: strip.completed_tiles,
        }
    }

    #[test]
    fn tile_count_is_capped() {
        // MaxPxPerSecond=600 x 10 minutes = 360,000 physical px. Uncapped, floor(w/60)
        // would ask for 6,000 tiles in ONE clip's draw.
        let (n, draw_w) = tile_count_for(360_000.0, 60.0);
        assert_eq!(n, MAX_TILES_PER_CLIP_DRAW, "the cap binds");
        assert!(
            (draw_w - 360_000.0 / 64.0).abs() < 1e-3,
            "when the cap binds the tiles WIDEN to w/64 rather than leaving a gap: {draw_w}"
        );

        // The ordinary case: edge-to-edge at natural width (D-05).
        let (n, draw_w) = tile_count_for(120.0, 60.0);
        assert_eq!(n, 2);
        assert!((draw_w - 60.0).abs() < 1e-6);

        // Narrower than one tile: ONE squeezed tile, not zero. D-04's solid-fill floor is
        // the C# side's decision (TimelineMetrics.FilmstripWidthFloorPx); by the time a
        // strip reaches here the clip is wide enough to show something.
        let (n, draw_w) = tile_count_for(30.0, 96.0);
        assert_eq!(n, 1);
        assert!((draw_w - 30.0).abs() < 1e-6);

        // Degenerate geometry draws nothing rather than dividing by it.
        assert_eq!(tile_count_for(0.0, 60.0).0, 0);
        assert_eq!(tile_count_for(-5.0, 60.0).0, 0);
        assert_eq!(tile_count_for(f32::NAN, 60.0).0, 0);
        assert_eq!(tile_count_for(f32::INFINITY, 60.0).0, MAX_TILES_PER_CLIP_DRAW);
    }

    #[test]
    fn tile_index_maps_the_trim_window() {
        // D-07: the strip is per-MEDIA and the clip SELECTS A WINDOW into it. A clip
        // trimmed to start 10s in must show the media's 10s frame in its leftmost tile —
        // this is the whole reason a trim drag costs zero decode.
        const INTERVAL: i64 = 1_000_000;
        const COMPLETED: u32 = 30;
        let count = 10;

        let left = tile_index_for(10_000_000, 10_000_000, INTERVAL, COMPLETED, 0, count);
        let right = tile_index_for(10_000_000, 10_000_000, INTERVAL, COMPLETED, count - 1, count);
        assert_eq!(left, Some(10), "the leftmost tile is the clip's IN point");
        assert_eq!(right, Some(19), "the rightmost is one interval short of the out point");

        // An untrimmed clip starts at the media's own first frame.
        assert_eq!(
            tile_index_for(0, 10_000_000, INTERVAL, COMPLETED, 0, count),
            Some(0)
        );

        // ...and the mapping is MONOTONIC across the window, which is what makes it read
        // as a filmstrip rather than as a shuffle.
        let mut prev = 0;
        for pos in 0..count {
            let idx = tile_index_for(10_000_000, 10_000_000, INTERVAL, COMPLETED, pos, count)
                .expect("every position inside the window resolves");
            assert!(idx >= prev, "tile {pos} went backwards: {idx} after {prev}");
            prev = idx;
        }
    }

    #[test]
    fn tile_index_clamps_hostile_input() {
        // The T-52-37 recurrence, for tiles. `slice_range`'s own rule: a cache file is
        // untrusted input however carefully it was written.
        assert_eq!(
            tile_index_for(0, 1_000_000, 1_000_000, 0, 0, 4),
            None,
            "nothing is completed, so there is no tile to point at"
        );
        assert_eq!(tile_index_for(0, 1_000_000, 0, 8, 0, 4), None, "zero interval");
        assert_eq!(tile_index_for(0, 1_000_000, -1, 8, 0, 4), None, "negative interval");
        assert_eq!(tile_index_for(0, 1_000_000, 1_000_000, 8, 0, 0), None, "zero tile count");
        assert_eq!(tile_index_for(0, 1_000_000, 1_000_000, 8, 9, 4), None, "position past count");

        // A clip that outruns its strip clamps to the LAST completed tile — D-14's
        // progressive fill makes this the ordinary case, not an error.
        assert_eq!(
            tile_index_for(0, 600_000_000, 1_000_000, 8, 3, 4),
            Some(7),
            "past the completed tail, clamp to completed-1"
        );

        // Negative and saturating inputs resolve, never panic, never go out of bounds.
        for (in_us, dur_us) in [
            (-5_000_000i64, 1_000_000i64),
            (i64::MIN, 1_000_000),
            (i64::MAX, i64::MAX),
            (0, 0),
            (0, -1),
        ] {
            for pos in 0..4 {
                if let Some(idx) = tile_index_for(in_us, dur_us, 1_000_000, 8, pos, 4) {
                    assert!(idx < 8, "in={in_us} dur={dur_us} pos={pos} gave OOB index {idx}");
                }
            }
        }
    }

    #[test]
    fn a_geometry_len_mismatch_draws_nothing() {
        // T-53.2-18: the length must be PROVEN from the geometry, not trusted. A strip
        // whose header says more rows than its bytes hold is how an out-of-bounds read
        // gets dressed up as a filmstrip.
        let bytes = sheet_bytes(32, T_W, T_H, PER_ROW);
        let good = strip_clip(&bytes, 256, 32);
        assert!(validated_strip(&good).is_some(), "the fixture must be VALID first");

        for (name, mutate) in [
            ("strip_len one byte short", (|c: &mut RudisTimelineClip| c.strip_len -= 1) as fn(&mut RudisTimelineClip)),
            ("strip_len one byte long", |c| c.strip_len += 1),
            ("strip_len doubled", |c| c.strip_len *= 2),
            ("tile_w widened", |c| c.strip_tile_w += 1),
            ("tile_h widened", |c| c.strip_tile_h += 1),
            ("tiles_per_row widened", |c| c.strip_tiles_per_row += 1),
            ("completed past total", |c| c.strip_completed_tiles = c.strip_total_tiles + 1),
            ("completed zero", |c| c.strip_completed_tiles = 0),
            ("tile_w zero", |c| c.strip_tile_w = 0),
            ("tile_w past the bound", |c| c.strip_tile_w = MAX_TILE_DIM + 1),
            ("tiles_per_row past the bound", |c| c.strip_tiles_per_row = MAX_TILES_PER_ROW + 1),
            ("total past the bound", |c| c.strip_total_tiles = MAX_TOTAL_TILES + 1),
            ("null pointer", |c| c.strip_ptr = std::ptr::null()),
            ("zero length", |c| c.strip_len = 0),
        ] {
            let mut hostile = good;
            mutate(&mut hostile);
            assert!(
                validated_strip(&hostile).is_none(),
                "{name}: a strip whose geometry disagrees with its bytes must draw NOTHING"
            );

            // ...and the whole emission path agrees, so "validation rejects it" and
            // "no quads are drawn" are the same fact rather than two hopeful ones.
            let mut pass = FilmstripPass::new();
            pass.begin();
            let mut out = Vec::new();
            pass.assemble_with(&mut out, &[hostile], 1.0, 14.0, |s| Some(fake_sheet(s)));
            assert!(out.is_empty(), "{name}: emitted {} quads", out.len());
            assert_eq!(pass.quads_drawn(), 0, "{name}");
        }
    }

    #[test]
    fn placeholder_draws_one_stretched_quad() {
        // D-13: the poster, stretched across the body. ONE quad however wide the clip is,
        // and the aspect is deliberately NOT preserved — a recognisable stretched poster
        // beats a correct-aspect sliver, and it is the state the handoff already names.
        let bytes = sheet_bytes(1, 128, 72, 1);
        for w in [24.0f32, 300.0, 4000.0] {
            let mut clip = strip_clip(&bytes, 1, 1);
            clip.strip_tile_w = 128;
            clip.strip_tile_h = 72;
            clip.strip_tiles_per_row = 1;
            clip.strip_interval_us = 0; // placeholders carry no cadence
            clip.flags = FLAG_STRIP_PLACEHOLDER;
            clip.w_px = w;

            let mut pass = FilmstripPass::new();
            pass.begin();
            let mut out = Vec::new();
            pass.assemble_with(&mut out, &[clip], 1.0, 14.0, |s| Some(fake_sheet(s)));

            assert_eq!(out.len(), 1, "width {w}: exactly one stretched quad");
            let body = crate::waveform::WaveformPass::body_inner_rect(&clip, 1.0, 14.0);
            assert!((out[0].rect[0] - body.x).abs() < 1e-3, "width {w}");
            assert!((out[0].rect[2] - body.w).abs() < 1e-3, "width {w}: spans the WHOLE body");
            assert!((out[0].rect[3] - body.h).abs() < 1e-3, "width {w}");

            // ...sampling the whole single tile, not a sub-rect of it.
            let strip = validated_strip(&clip).unwrap();
            assert_eq!(out[0].uv, tile_uv(&fake_sheet(&strip), &strip, 0), "width {w}");
        }
    }

    #[test]
    fn the_frame_budget_holds() {
        // T-53.2-20's other half. The per-clip cap alone is not enough: 1,000 clips each
        // asking for a legal 64 tiles is 64,000 instances without ever tripping it.
        let bytes = sheet_bytes(256, T_W, T_H, PER_ROW);
        let mut clip = strip_clip(&bytes, 256, 256);
        clip.w_px = 4000.0; // wide enough that the per-clip cap binds
        let clips = vec![clip; 1000];

        let mut pass = FilmstripPass::new();
        pass.begin();
        let mut out = Vec::new();
        pass.assemble_with(&mut out, &clips, 1.0, 14.0, |s| Some(fake_sheet(s)));

        assert!(
            out.len() as u32 <= MAX_FILMSTRIP_QUADS_PER_FRAME,
            "1,000 clips emitted {} quads, past the {MAX_FILMSTRIP_QUADS_PER_FRAME} ceiling",
            out.len()
        );
        assert_eq!(pass.quads_drawn(), out.len() as u32);
        assert!(
            pass.truncated_clips() > 0,
            "a cap nobody can see reached is a cap nobody can trust"
        );
        // NON-VACUITY: the cap must be reached by DRAWING, not by drawing nothing.
        assert!(out.len() as u32 > MAX_FILMSTRIP_QUADS_PER_FRAME / 2);
    }
}
