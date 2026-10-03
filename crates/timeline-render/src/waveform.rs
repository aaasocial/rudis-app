//! The audio waveform fill — SHELL-09's visible half (plan 52-08).
//!
//! # The shape of it
//!
//! One cached peak resolution (`crates/waveform`'s 10 ms block-RMS bytes), decimated
//! CLIENT-SIDE at draw time to whatever the current zoom needs, drawn as vertical bars in
//! `clip_waveform_line` inside the clip body. 52-04 cut the whole seam — the module, the
//! entry point, the ABI fields and the call site in `quads.rs`'s frame assembly — so this
//! plan adds no ABI field, no second pipeline and no draw-order decision. Bars are
//! [`QuadInstance`]s in the SAME buffer and the SAME instanced draw call as every other
//! rectangle the Timeline draws.
//!
//! # The five things this file gets right on purpose
//!
//! 1. **The slice is CLAMPED, always.** `peaks_ptr[..peaks_len]` is bounded at the boundary
//!    by `abi::slice_checked`, and the clip's own window into that slice is clamped again
//!    here. A clip can legitimately out-run its peaks: extraction is a background import
//!    job, so a clip placed a moment after import has a partially-written array, and 52-02
//!    measured `peaks.len()` disagreeing with `duration_us / block_us` by ~1% when a
//!    container overstates its decodable audio. `peaks.len()` is the authoritative number
//!    (T-52-37).
//!
//! 2. **A bar is the MAX of its bucket, never the mean.** At 40 px/s a 30-second clip is
//!    1,200 bars from 3,000 peaks, so most bars summarise two or three blocks; at a
//!    zoomed-out 5 px/s one bar summarises forty. The mean of a bucket containing one loud
//!    transient and thirty-nine quiet blocks is *quiet* — so the mean turns a drum hit into
//!    a shrug and a whole waveform into mush at low zoom, while the max keeps the envelope
//!    legible at every scale. Every editor that draws waveforms does this; it is the reason
//!    a zoomed-out waveform still looks like the audio.
//!
//! 3. **The height is NORMALISED, and the curve is measured rather than chosen.** A linear
//!    `peak / 255` renders normal speech as a flat strip in the bottom quarter of a 42 px
//!    lane — 52-02 measured real speech at an RMS of ~0.23 whose whole envelope lives inside
//!    54-65 distinct levels out of 255, and put the rendered picture in
//!    `artifacts/52-02-waveform.md` precisely so this plan would not rediscover it. See
//!    [`normalised_height`] for the curve and the arithmetic behind its constant. The cache
//!    stores the MEASUREMENT and this file owns the display transform, so changing the curve
//!    later costs a redraw rather than a re-extraction of every imported file.
//!
//! 4. **Two hard bounds, because the peak bytes are media-derived.** [`MAX_BARS_PER_CLIP`]
//!    stops one deeply-zoomed clip from emitting a hundred thousand sub-pixel instances, and
//!    [`MAX_WAVEFORM_QUADS_PER_FRAME`] stops a thousand-clip project from turning one frame
//!    into millions of them. Both truncate GRACEFULLY and REPORT it in the frame stats
//!    (T-52-36): a frame that draws fewer bars is a frame, where a frame that runs out of
//!    memory is a dead Timeline.
//!
//! 5. **Colour comes from the palette, by NAME.** `clip_waveform_line` was resolved from
//!    `Theme/Tokens.xaml` on the C# side and crossed the ABI as a `u32`; this crate has no
//!    colour of its own and contains no hex literal anywhere (CLAUDE.md rule 7, which a GPU
//!    boundary would otherwise quietly kill because a shader cannot read a XAML dictionary).
//!
//! # Which clips get a fill
//!
//! [`crate::frame::FLAG_HAS_AUDIO`] — `has_audio && !audio_detached`, which the C# side has
//! already folded into the flag (52-CONTEXT D-20). VIDEO clips with audio included; a video
//! clip whose audio was detached shows no fill, because its audio now lives on its own
//! audio-track clip, which does.

use crate::frame::{RudisTimelineClip, FLAG_HAS_AUDIO};
use crate::quads::{push_fill_into, Palette, QuadInstance, QuadPass};

/// Bars one clip may emit, however wide it is drawn.
///
/// **A DoS bound, not a quality setting.** `w_px` is a physical width the user controls by
/// zooming; without a cap, a 30-minute clip at a deep zoom would ask for hundreds of
/// thousands of instances, every one of them narrower than a pixel and therefore invisible.
/// 4,096 is wider than any real display, so the cap can only ever be reached by a clip whose
/// bars are already sub-pixel.
pub const MAX_BARS_PER_CLIP: usize = 4096;

/// Waveform bars one FRAME may emit, across all clips.
///
/// **The other half of the same DoS bound (T-52-36).** The per-clip cap is not sufficient on
/// its own: a 1,000-clip project with every clip 200 px wide would want 200,000 instances
/// without ever tripping [`MAX_BARS_PER_CLIP`]. Past this ceiling, remaining clips draw
/// their flat body colour with no bars, and the frame's stats record the truncation so the
/// degradation is a number anyone can read rather than a mystery.
pub const MAX_WAVEFORM_QUADS_PER_FRAME: u32 = 20_000;

/// The display gain applied on top of the square-root curve. See [`normalised_height`].
pub const WAVEFORM_DISPLAY_GAIN: f32 = 1.7;

/// The rectangle a clip's bars are drawn into — the clip body inset by its border, so a bar
/// can never paint over the border that separates one clip from the next.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InnerRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl InnerRect {
    /// Every component finite and both dimensions positive. Geometry is filtered HERE, in
    /// the layer that builds quads, exactly as `quads::rect_is_drawable` filters clip
    /// rectangles — a `NaN` width is a drawing problem, not a memory-safety problem, and the
    /// right answer is a skipped fill inside an otherwise complete frame.
    #[inline]
    pub fn is_drawable(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.w.is_finite()
            && self.h.is_finite()
            && self.w > 0.0
            && self.h > 0.0
    }
}

/// What one clip's fill did — bars emitted, and whether the frame budget cut it short.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BarOutcome {
    pub bars: u32,
    pub truncated: bool,
}

/// The half-open peak index range `[start, end)` a clip's `[clip_in_us, +clip_dur_us)` source
/// window maps onto, CLAMPED to `peaks_len`.
///
/// Every degenerate input resolves to an empty or truncated range rather than to a panic:
/// a non-positive `block_us` (which would divide by zero), a negative in-point, a negative
/// duration, an in-point past the end of the array, and a duration large enough to overflow
/// `i64` addition. The peak bytes and their metadata reach this crate from a cache file, and
/// a cache file is untrusted input however carefully it was written.
pub fn slice_range(
    clip_in_us: i64,
    clip_dur_us: i64,
    block_us: i64,
    peaks_len: usize,
) -> (usize, usize) {
    if block_us <= 0 || peaks_len == 0 {
        return (0, 0);
    }

    let in_us = clip_in_us.max(0);
    let dur_us = clip_dur_us.max(0);
    let end_us = in_us.saturating_add(dur_us);

    let start = (in_us / block_us).min(peaks_len as i64) as usize;
    let end = (end_us / block_us).min(peaks_len as i64) as usize;

    (start, end.max(start))
}

/// One bar per physical pixel of clip width, capped at [`MAX_BARS_PER_CLIP`].
///
/// A non-finite or non-positive width yields zero bars. `f32::INFINITY` deliberately
/// saturates to the cap rather than to zero: an infinite width is a geometry bug upstream,
/// and clamping it is more useful than treating it as "nothing to draw" — the clip's own
/// rectangle is filtered separately.
pub fn bar_count_for(w_px: f32) -> usize {
    if w_px.is_nan() || w_px <= 0.0 {
        return 0;
    }

    if w_px >= MAX_BARS_PER_CLIP as f32 {
        return MAX_BARS_PER_CLIP;
    }

    (w_px.floor() as usize).min(MAX_BARS_PER_CLIP)
}

/// The peak index range bar `bar` of `bar_count` summarises, over a `len`-long slice.
///
/// Guarantees a NON-EMPTY bucket whenever `len > 0`: when there are more bars than peaks the
/// window degenerates to a single repeated peak rather than to an empty range, because an
/// empty bucket drawn as zero is a black comb through the middle of a waveform.
#[inline]
fn bucket(len: usize, bar: usize, bar_count: usize) -> (usize, usize) {
    let lo = bar * len / bar_count;
    let hi = ((bar + 1) * len / bar_count).max(lo + 1).min(len);
    (lo.min(len.saturating_sub(1)), hi)
}

/// The MAX of bar `bar`'s bucket. See the module header for why max and not mean.
#[inline]
pub fn bucket_max(peaks: &[u8], bar: usize, bar_count: usize) -> u8 {
    if peaks.is_empty() || bar_count == 0 || bar >= bar_count {
        return 0;
    }

    let (lo, hi) = bucket(peaks.len(), bar, bar_count);
    let mut max = 0u8;
    for &p in &peaks[lo..hi] {
        if p > max {
            max = p;
        }
    }

    max
}

/// Decimate `peaks` to exactly `bar_count` values, max-per-bucket, into a CALLER-OWNED
/// buffer.
///
/// Exposed separately from [`emit_bars`] because the bucket arithmetic is the part that can
/// be wrong in a way a screenshot cannot show, and it is worth being able to assert on the
/// values themselves. `emit_bars` does not call it — it walks the same buckets straight into
/// quads, so the drawn path allocates no intermediate array at all — but both share
/// [`bucket_max`], so they cannot drift.
pub fn decimate(peaks: &[u8], bar_count: usize, out: &mut Vec<u8>) {
    out.clear();
    if peaks.is_empty() || bar_count == 0 {
        return;
    }

    out.reserve(bar_count);
    for bar in 0..bar_count {
        out.push(bucket_max(peaks, bar, bar_count));
    }
}

/// The display curve: a peak byte to a fraction of the lane height, in `[0, 1]`.
///
/// # Why a curve at all, with the numbers
///
/// The cached bytes are `u8`-quantised block RMS. 52-02 measured real speech at an RMS of
/// ~0.23 whose entire envelope occupies **54-65 distinct levels out of 255** — so a linear
/// `peak / 255` height puts the LOUDEST part of normal speech at 65/255 ≈ **0.25 of the
/// lane**, i.e. a 10-pixel strip in the middle of a 42-pixel audio lane with 32 pixels of
/// empty space around it. That is the flat-strip picture in `artifacts/52-02-waveform.md`,
/// and it is a display bug, not a data one: the bytes are fine, `u8` is not the limiting
/// factor, and widening the cache format would not move that number at all.
///
/// # The curve
///
/// `min(1, sqrt(peak / 255) * GAIN)`.
///
/// The square root is the standard half-power lift — it expands the crowded bottom of the
/// range where speech actually lives and compresses the top, which is also roughly how
/// loudness is perceived. The gain is then set from the measurement rather than by eye:
/// `0.85 / sqrt(65/255) = 1.683`, rounded to [`WAVEFORM_DISPLAY_GAIN`], so 52-02's measured
/// speech MAXIMUM renders at ~85% of the lane and its ~0.23 body sits comfortably in the
/// middle. Clipping begins at RMS 0.346 (-9.2 dBFS), which only a heavily-limited master
/// reaches, and clipping there is what every DAW's waveform view does anyway.
///
/// It is a DISPLAY transform and lives here rather than in the cache on purpose (52-02's own
/// decision): changing it costs a redraw, where baking it into the cached bytes would cost a
/// re-extraction of every imported file.
#[inline]
pub fn normalised_height(peak: u8) -> f32 {
    if peak == 0 {
        return 0.0;
    }

    let linear = peak as f32 / 255.0;
    (linear.sqrt() * WAVEFORM_DISPLAY_GAIN).min(1.0)
}

/// Push one clip's waveform bars into a caller-owned instance list.
///
/// `peaks` is the clip's ALREADY-BOUNDED byte slice (see [`WaveformPass::push_bars`], which
/// takes it through `abi::slice_checked`); `inner` is the clip body inset by its border;
/// `budget` is the frame's remaining waveform-quad allowance, decremented in place.
///
/// Returns the bars emitted and whether [`MAX_WAVEFORM_QUADS_PER_FRAME`] cut this clip
/// short. A clip that cannot have ALL its bars draws NONE of them: half a waveform reads as
/// quiet audio, which is a wrong picture, where a missing waveform reads as "not drawn".
pub fn emit_bars(
    clip: &RudisTimelineClip,
    peaks: &[u8],
    inner: InnerRect,
    line_colour: [f32; 4],
    out: &mut Vec<QuadInstance>,
    budget: &mut u32,
) -> BarOutcome {
    if !inner.is_drawable() || peaks.is_empty() {
        return BarOutcome::default();
    }

    let (start, end) = slice_range(
        clip.clip_in_us,
        clip.clip_dur_us,
        clip.peaks_block_us,
        peaks.len(),
    );
    let window = &peaks[start..end];
    if window.is_empty() {
        return BarOutcome::default();
    }

    let bar_count = bar_count_for(inner.w);
    if bar_count == 0 {
        return BarOutcome::default();
    }

    if (bar_count as u32) > *budget {
        return BarOutcome {
            bars: 0,
            truncated: true,
        };
    }

    let bar_w = inner.w / bar_count as f32;
    let centre_y = inner.y + inner.h * 0.5;
    let mut emitted = 0u32;

    for bar in 0..bar_count {
        // A minimum of one PHYSICAL pixel, so a quiet passage still reads as the
        // waveform's centre line rather than vanishing into the clip body — which is
        // also what makes a silent head or tail visibly silent rather than absent.
        let h = (normalised_height(bucket_max(window, bar, bar_count)) * inner.h).max(1.0);
        let x = inner.x + bar as f32 * bar_w;
        if push_fill_into(out, x, centre_y - h * 0.5, bar_w, h, line_colour) {
            emitted += 1;
        }
    }

    *budget -= emitted;
    BarOutcome {
        bars: emitted,
        truncated: false,
    }
}

/// The waveform fill pass.
///
/// Holds the per-frame counters and nothing else — no pipeline, no buffer, no state that
/// survives a frame. Bars are quads, so the instance list they go into belongs to
/// [`QuadPass`].
#[derive(Debug, Default)]
pub struct WaveformPass {
    /// Bars emitted by the most recent assembly, read by the frame stats so "the waveform
    /// drew nothing" is a countable fact rather than a visual impression.
    bars_emitted: std::cell::Cell<u32>,
    /// Remaining frame budget. Reset by [`Self::begin`].
    budget: std::cell::Cell<u32>,
    /// Clips the budget cut short this frame.
    truncated_clips: std::cell::Cell<u32>,
}

impl WaveformPass {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bars emitted during the most recent [`Self::push_bars`] sequence.
    pub fn bars_emitted(&self) -> u32 {
        self.bars_emitted.get()
    }

    /// Clips whose fill was refused because the frame budget was exhausted.
    pub fn truncated_clips(&self) -> u32 {
        self.truncated_clips.get()
    }

    /// Reset the per-frame counters and the budget. Called by the frame assembly before the
    /// clip loop.
    pub fn begin(&self) {
        self.bars_emitted.set(0);
        self.truncated_clips.set(0);
        self.budget.set(MAX_WAVEFORM_QUADS_PER_FRAME);
    }

    /// Push this clip's waveform bars into the shared quad buffer.
    ///
    /// Draws only when the clip has undetached audio ([`FLAG_HAS_AUDIO`], D-20) and carries a
    /// non-empty peak array. A clip with no peaks draws no fill — which is not a failure but
    /// the correct behaviour for a clip whose background extraction has not finished, and the
    /// permanent behaviour for one whose audio could not be decoded at all
    /// (52-CONTEXT D-19).
    ///
    /// # Safety
    ///
    /// `clip.peaks_ptr` must be valid for `clip.peaks_len` bytes for the duration of this
    /// call, which is the frame contract's own rule for every pointer it carries. The slice
    /// is built through the boundary's bound check, never from the raw pointer directly.
    pub fn push_bars(
        &self,
        quads: &mut QuadPass,
        clip: &RudisTimelineClip,
        palette: &Palette,
        inset_px: f32,
        band_h_px: f32,
    ) -> u32 {
        if !Self::has_drawable_peaks(clip) {
            return 0;
        }

        let Some(peaks) = crate::abi::peaks_checked(clip) else {
            return 0;
        };

        let inset = if inset_px.is_finite() && inset_px > 0.0 {
            inset_px
        } else {
            0.0
        };

        let inner = Self::body_inner_rect(clip, inset, band_h_px);

        let mut budget = self.budget.get();
        let outcome = emit_bars(
            clip,
            peaks,
            inner,
            palette.clip_waveform_line,
            quads.instances_mut(),
            &mut budget,
        );
        self.budget.set(budget);

        if outcome.truncated {
            self.truncated_clips
                .set(self.truncated_clips.get().saturating_add(1));
        }

        self.bars_emitted
            .set(self.bars_emitted.get().saturating_add(outcome.bars));
        outcome.bars
    }

    /// The rectangle [`Self::push_bars`] draws into: the clip's BODY — everything below
    /// D-01's title band — inset by the clip's own border.
    ///
    /// Factored out of `push_bars` for the same reason [`Self::has_drawable_peaks`] was:
    /// `push_bars` needs a [`QuadPass`], a `QuadPass` owns real wgpu handles, and a zeroed
    /// stand-in would be undefined behaviour the moment it dropped. Naming the geometry is
    /// the honest way to make it assertable on a machine with no adapter at all.
    ///
    /// On an audio lane this is the ~28px left under a 14px band, which is D-03's whole
    /// point: one clip anatomy across both lane kinds, and an audio clip finally gets the
    /// readable name label it never had.
    ///
    /// `band_h_px <= 0` makes `split_band` hand back the full clip rect, so the result is
    /// byte-identical to what plan 52-08 computed and the waveform renders exactly where it
    /// always did.
    #[inline]
    pub fn body_inner_rect(
        clip: &RudisTimelineClip,
        inset_px: f32,
        band_h_px: f32,
    ) -> InnerRect {
        let inset = if inset_px.is_finite() && inset_px > 0.0 {
            inset_px
        } else {
            0.0
        };
        let (_band, body) =
            crate::frame::split_band(clip.x_px, clip.y_px, clip.w_px, clip.h_px, band_h_px);
        InnerRect {
            x: body[0] + inset,
            y: body[1] + inset,
            w: body[2] - inset * 2.0,
            h: body[3] - inset * 2.0,
        }
    }

    /// The guard [`Self::push_bars`] returns on, factored out so it is testable WITHOUT a
    /// GPU device.
    ///
    /// Faking a `QuadPass` to prove the early return is not an option — it owns real wgpu
    /// handles, and a zeroed stand-in would be undefined behaviour the moment it dropped,
    /// which is a steep price for a test about an `if`. Naming the condition is the honest
    /// version.
    ///
    /// The pair matters, not either half: a stale non-null pointer beside a zeroed length is
    /// exactly what a partially-updated frame looks like, and reading it would be an
    /// out-of-bounds read dressed up as a waveform.
    #[inline]
    pub fn has_drawable_peaks(clip: &RudisTimelineClip) -> bool {
        clip.flags & FLAG_HAS_AUDIO != 0
            && !clip.peaks_ptr.is_null()
            && clip.peaks_len > 0
            && clip.peaks_block_us > 0
            && clip.clip_dur_us > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_with_no_peaks_has_nothing_to_draw_and_that_is_every_unextracted_clip() {
        // Not a placeholder assertion: null/0 peaks is what a not-yet-extracted clip
        // carries, permanently, because extraction is a background import job the
        // Timeline must never wait on (52-CONTEXT D-19).
        let clip: RudisTimelineClip = unsafe { std::mem::zeroed() };
        assert!(clip.peaks_ptr.is_null());
        assert!(!WaveformPass::has_drawable_peaks(&clip));
    }

    #[test]
    fn a_nonnull_pointer_with_zero_length_still_has_nothing_to_draw() {
        let mut clip: RudisTimelineClip = unsafe { std::mem::zeroed() };
        clip.flags = FLAG_HAS_AUDIO;
        clip.peaks_ptr = 0x1000usize as *const u8;
        clip.peaks_len = 0;
        assert!(!WaveformPass::has_drawable_peaks(&clip));
    }

    #[test]
    fn a_clip_without_the_audio_flag_draws_no_fill_even_with_peaks() {
        // D-20's other half: a video clip whose audio was DETACHED keeps whatever peaks
        // its media has, and must not draw them — the audio now lives on its own
        // audio-track clip, which does.
        let peaks = [10u8, 20, 30, 40];
        let mut clip: RudisTimelineClip = unsafe { std::mem::zeroed() };
        clip.peaks_ptr = peaks.as_ptr();
        clip.peaks_len = peaks.len() as u32;
        clip.peaks_block_us = 10_000;
        clip.clip_dur_us = 40_000;

        assert!(!WaveformPass::has_drawable_peaks(&clip));
        clip.flags = FLAG_HAS_AUDIO;
        assert!(WaveformPass::has_drawable_peaks(&clip));
    }

    #[test]
    fn waveform_bars_sit_below_the_band() {
        // D-03: an audio clip gets the SAME 14px band as a video clip, and its waveform
        // fills the ~28px underneath. A bar that strayed into the band would paint over
        // the clip's own name, which is the one thing the band exists to carry.
        const BAND_H: f32 = 17.5; // 14 logical px at 1.25 scale
        const INSET: f32 = 1.25;

        // Loud peaks throughout, so bars reach their FULL height — a quiet fixture would
        // keep every bar near the vertical centre and pass this test by accident.
        let peaks = [250u8; 80];
        let mut clip: RudisTimelineClip = unsafe { std::mem::zeroed() };
        clip.x_px = 100.0;
        clip.y_px = 200.0;
        clip.w_px = 64.0;
        clip.h_px = 52.5; // 42 logical px — the handoff's audio lane, scaled
        clip.flags = FLAG_HAS_AUDIO;
        clip.peaks_ptr = peaks.as_ptr();
        clip.peaks_len = peaks.len() as u32;
        clip.peaks_block_us = 10_000;
        clip.clip_dur_us = 800_000;

        let floor = clip.y_px + BAND_H;

        let inner = WaveformPass::body_inner_rect(&clip, INSET, BAND_H);
        let mut out = Vec::new();
        let mut budget = MAX_WAVEFORM_QUADS_PER_FRAME;
        let outcome = emit_bars(
            &clip,
            &peaks,
            inner,
            [0.0, 0.0, 0.0, 1.0],
            &mut out,
            &mut budget,
        );

        assert!(outcome.bars > 0, "the fixture must actually draw bars");
        assert_eq!(out.len(), outcome.bars as usize);

        // EVERY bar, not the first: a bucket-arithmetic slip shows up at the third bar
        // from the left, which is exactly where a spot check does not look.
        for (i, q) in out.iter().enumerate() {
            let [_, y, _, h] = q.rect;
            assert!(
                y >= floor,
                "bar {i} starts at y={y}, inside the band (floor {floor})"
            );
            assert!(
                y + h <= clip.y_px + clip.h_px + 0.001,
                "bar {i} overflows the clip's bottom edge"
            );
        }

        // NON-VACUITY. The same clip with no band puts bars ABOVE that floor — so the
        // assertion above is measuring the band's effect, not the fact that bars are
        // short. Without this, a bug that dropped `band_h_px` entirely would still pass.
        let unbanded = WaveformPass::body_inner_rect(&clip, INSET, 0.0);
        let mut before = Vec::new();
        let mut budget = MAX_WAVEFORM_QUADS_PER_FRAME;
        emit_bars(
            &clip,
            &peaks,
            unbanded,
            [0.0, 0.0, 0.0, 1.0],
            &mut before,
            &mut budget,
        );
        assert!(
            before.iter().any(|q| q.rect[1] < floor),
            "with no band the bars must reach above y={floor}, or the test above proves \
             nothing about the band"
        );
    }

    #[test]
    fn the_per_frame_counters_reset_and_are_readable() {
        let pass = WaveformPass::new();
        assert_eq!(pass.bars_emitted(), 0);
        pass.begin();
        assert_eq!(pass.bars_emitted(), 0);
        assert_eq!(pass.truncated_clips(), 0);
        assert_eq!(pass.budget.get(), MAX_WAVEFORM_QUADS_PER_FRAME);
    }
}
