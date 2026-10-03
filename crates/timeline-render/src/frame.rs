//! THE FRAME CONTRACT — the `#[repr(C)]` structs both languages compile against.
//!
//! One flat description of a whole Timeline frame crosses the ABI per redraw: arrays of
//! lanes, clips and ruler ticks as pointer/length pairs, plus a handful of scalars. There
//! is **no per-clip object**, no per-clip allocation, and no per-clip call — that is
//! SHELL-05's "a clip is a rectangle in an array", expressed as a type.
//!
//! # Three rules this module exists to enforce
//!
//! 1. **The renderer holds no colour of its own.** Every colour arrives here as a `u32`
//!    the C# side resolved from `Theme/Tokens.xaml` BY NAME. CLAUDE.md convention 7 says
//!    named design tokens, never raw hex — and a GPU boundary is exactly where that rule
//!    would otherwise quietly die, because a shader cannot read a XAML dictionary. So the
//!    palette is data, and `grep` for a hex literal anywhere in this crate returns zero.
//!
//! 2. **Layout is pinned, not assumed.** `tests/layout_canary.rs` asserts `size_of`,
//!    `align_of`, and per-field `offset_of` as literal numbers, plus a known-value byte
//!    pattern per struct. Plan 52-06's C# mirror asserts the SAME numbers. A silent field
//!    reorder then fails loudly on both sides instead of corrupting one of them — the same
//!    technique `crates/ffi/tests/layout_canary.rs` already uses for `RudisBuffer`.
//!
//! 3. **Pointers are read-only borrows for the duration of ONE call.** The renderer never
//!    stores a pointer from a frame past the `rudis_timeline_render` that carried it, and
//!    never writes through one. The C# side may reuse or free its buffers the moment the
//!    call returns.
//!
//! # Colour encoding
//!
//! Every colour is `u32` as `0xAARRGGBB` — the byte order .NET's `Windows.UI.Color` and
//! WinUI's `#AARRGGBB` XAML notation already use, so the C# side packs a token with no
//! reordering. Conversion to the linear `[f32; 4]` the shader wants happens ONCE, when a
//! palette is uploaded (see `quads.rs`), never per quad.
//!
//! # Units
//!
//! Every `_px` field is **PHYSICAL** pixels (device pixels), already multiplied through by
//! `scale`. The C# hot path works exclusively in LOGICAL px and converts at exactly one
//! seam (`TimelineViewport.LogicalToPhysical`, 52-03 §3.5). `scale` rides along anyway
//! because the text pass needs it: glyphon rasterises at a physical size, and a label
//! shaped for the wrong scale is blurry rather than wrong, which is the failure mode
//! nobody notices for a month.

use std::os::raw::c_void;

/// Every colour the Timeline can draw, resolved from `Theme/Tokens.xaml` by the C# side.
///
/// Uploaded by `rudis_timeline_set_palette` and cached until it changes (a theme switch),
/// NOT re-sent per frame: it is the one piece of frame state that is genuinely constant
/// across thousands of redraws.
///
/// The field names are the design handoff's own token names
/// (`design_handoff_rudis_editor/README.md` § Design tokens, lines 184-213), lower-snaked.
/// Keeping the names identical is what makes "did the renderer use the right token?" a
/// question anyone can answer by reading two files side by side.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RudisTimelinePalette {
    /// `bg-bar` — the `Timeline › Toolbar` band, the ruler band, and the sticky
    /// `TrackHeader` gutter (README:192, :138).
    pub bg_bar: u32,
    /// `bg-app` — the base lane band (README:189).
    pub bg_app: u32,
    /// `bg-panel` — the alternating lane band, so adjacent lanes are separable without a
    /// heavier separator (README:190).
    pub bg_panel: u32,
    /// `border-subtle` — Timeline track separators, the handoff's stated use (README:197).
    pub border_subtle: u32,
    /// `border-hairline` — minor ruler graduations (README:195).
    pub border_hairline: u32,
    /// `text-primary` — the live timecode (README:198). Drawn by the C# `Timeline ›
    /// Toolbar`, not here; carried so the palette is one complete object rather than two
    /// partial ones that can disagree.
    pub text_primary: u32,
    /// `text-secondary` — lane labels in the gutter (README:199).
    pub text_secondary: u32,
    /// `text-faint` — the `TIMELINE` region tag and ruler tick labels (README:201).
    pub text_faint: u32,
    /// `accent` — the playhead line and the selection outline (README:202, :136, :160).
    pub accent: u32,
    /// `accent-bright` — trim handles and snap guides (README:203).
    pub accent_bright: u32,
    /// The audio clip fill — the mint the handoff names at README:213.
    ///
    /// The literal value is deliberately NOT written here. `no_raw_hex` is a
    /// USE-detector pinned at zero over this crate's sources, and 52-02 hit twice the
    /// lesson that a gate a rationale comment can trip stops distinguishing "used" from
    /// "mentioned" (52-02-SUMMARY deviation 7). Read the value from the handoff.
    pub clip_audio_fill: u32,
    /// The audio waveform line — the translucent dark green beside the mint fill at
    /// README:213, value deliberately not transcribed here for the reason above. Consumed
    /// by `waveform.rs`, which plan 52-08 fills.
    pub clip_waveform_line: u32,
    /// The 8-entry clip poster palette, assigned per media item and cycled
    /// (README:213). The C# side does the assignment; the renderer only ever receives an
    /// already-resolved `fill` per clip. Carried anyway so a future in-renderer default
    /// (e.g. a clip whose media item vanished) has a token to reach for rather than an
    /// invented colour.
    pub clip_poster: [u32; 8],
}

/// One track lane — a horizontal band with a label in the gutter.
///
/// Built by `LaneModel.Build` on the C# side (52-03), which numbers `V{n}` / `A{n}` per
/// kind and skips any `TrackKind` this build cannot render.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RudisTimelineLane {
    /// Top edge of the band, physical px from the surface top.
    pub y_px: f32,
    /// Band height, physical px. Video 48 / audio 42 LOGICAL px at the handoff
    /// (README:239), scaled.
    pub h_px: f32,
    /// `0` = video, `1` = audio. Any other value is treated as video and counted — an
    /// unknown lane kind must degrade, never panic (mirrors 52-03's `SkippedTrackKinds`
    /// posture).
    ///
    /// There is deliberately no `2` for the handoff's `T` (text/captions) lane:
    /// `crates/core::TrackKind` is `Video | Audio` only (`crates/core/src/model.rs:399-409`)
    /// and that crate is frozen, so the lane has nothing to port (52-CONTEXT D-06).
    pub kind: u32,
    /// UTF-8 label bytes (`V1`, `A1`, …). Borrowed for this call only. May be null.
    pub label_ptr: *const u8,
    /// Length of `label_ptr` in bytes. Validated against a bound BEFORE any read; invalid
    /// UTF-8 renders as an empty label, never a panic (see `abi::label_checked`).
    pub label_len: u32,
}

/// One ruler graduation.
///
/// Positions come from `RulerTicks.Select` (52-03), which picks the smallest ladder entry
/// whose label spacing clears `MinTickLabelSpacingPx`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RudisTimelineTick {
    /// Physical px from the surface left (already past the gutter).
    pub x_px: f32,
    /// Non-zero for a major graduation (`border-strong`, full height, labelled); zero for
    /// a minor one (`border-hairline`, short, unlabelled).
    pub major: u32,
    /// UTF-8 timecode label, e.g. `0:05`. May be null (minor ticks carry none).
    pub label_ptr: *const u8,
    /// Length of `label_ptr` in bytes; bounded and UTF-8-validated before use.
    pub label_len: u32,
}

/// One clip: a rectangle, two colours, a few flags, and a borrowed label.
///
/// This is the struct the whole phase is about. At 1,000 clips the C# side hands over one
/// contiguous array of these and the renderer walks it — no object per clip on either
/// side of the boundary.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RudisTimelineClip {
    /// Left edge, physical px from the surface left.
    pub x_px: f32,
    /// Top edge, physical px from the surface top.
    pub y_px: f32,
    /// Width, physical px. Non-finite or non-positive ⇒ the quad is SKIPPED (`quads.rs`),
    /// not clamped to something plausible-looking.
    pub w_px: f32,
    /// Height, physical px.
    pub h_px: f32,
    /// Body fill, `0xAARRGGBB`. One of the handoff's 8 poster colours for video, the mint
    /// `clip_audio_fill` for audio — chosen by the C# side from named tokens.
    pub fill: u32,
    /// Border, `0xAARRGGBB`. The handoff specifies "~15% darker shade of the fill"
    /// (README:213); that darkening is the C# side's, so this crate never computes a
    /// colour either.
    pub border: u32,
    /// Bit flags:
    ///
    /// * `bit0` (`FLAG_SELECTED`) — draw the 2px `accent` selection outline (README:160).
    /// * `bit1` (`FLAG_HAS_AUDIO`) — the clip has audio, so the waveform fill applies
    ///   (`has_audio && !audio_detached`, 52-CONTEXT D-20). Consumed by `waveform.rs`.
    /// * `bit2` (`FLAG_RESERVED_AGENT_EDITED`) — **RESERVED. MUST ALWAYS BE 0 this phase.**
    ///
    /// # The RESERVED bit, in full
    ///
    /// The design handoff marks the agent-edited clip with a `✦` and a 2px `accent` border
    /// (`design_handoff_rudis_editor/README.md:137`). There is **no domain field to drive
    /// it**. `Clip.agentEdited` was reserved in `.planning/research/ARCHITECTURE.md:413`
    /// (and is named in CLAUDE.md's Architecture section as a "typed-but-empty
    /// extensibility seam"), but it was never added to `crates/core::Clip` — a repo-wide
    /// grep for `agent_edited` / `agentEdited` finds no field in any `.rs` — and
    /// `crates/core` is under the annotated `engine-axis-freeze` tag, so adding one is not
    /// an available move either.
    ///
    /// So the bit is reserved and never set. Do NOT invent the field to fill it: that
    /// would be new backend scope smuggled in under a UI port, which is exactly what
    /// 52-CONTEXT D-06 refused for the `T` lane. Plan 52-10 records the gap; a future
    /// phase that genuinely adds agent-edit provenance to the domain model claims the bit.
    pub flags: u32,
    /// Trim-handle width in physical px, per edge. The C# hit tester uses
    /// `min(TrimHandleWidth, w / 3)` so a tiny clip keeps a body zone (52-03 §3.4); the
    /// renderer draws whatever it is handed, so the drawn handle and the hit zone can
    /// never disagree.
    pub trim_handle_px: f32,
    /// UTF-8 clip label (the media item's display name or filename). Clipped to the clip
    /// rect minus both trim handles — a label never paints outside its own clip.
    pub label_ptr: *const u8,
    /// Length of `label_ptr` in bytes; bounded and UTF-8-validated before use.
    pub label_len: u32,
    /// `u8` RMS peaks at `peaks_block_us` resolution, from `crates/waveform`'s cache.
    ///
    /// **Null / zero until plan 52-08 wires the read path.** `waveform.rs` already accepts
    /// it and draws nothing, so 52-08 adds no ABI change and no new pipeline.
    pub peaks_ptr: *const u8,
    /// Length of `peaks_ptr` in bytes (one byte per block).
    pub peaks_len: u32,
    /// Microseconds of source audio each peak byte summarises (`crates/waveform`'s
    /// `PEAK_BLOCK_US`, 10,000). Read from the cache entry rather than assumed, so a
    /// `CACHE_VERSION` bump cannot silently mis-scale old data (52-02's own hand-off note).
    pub peaks_block_us: i64,
    /// The clip's in-point within its source media, µs — where to start reading `peaks`.
    pub clip_in_us: i64,
    /// The clip's duration, µs — how much of `peaks` this rectangle covers.
    pub clip_dur_us: i64,

    // ── APPENDED by plan 53.2-03. See the append-only note below the struct. ──
    //
    // D-12's CORRECTION is why these are FIELDS and not a seventh export: bulk read-only
    // data reaches this renderer through the per-frame pointer/length pattern the peaks
    // above already prove. `abi.rs`'s export surface is pinned at six and test-enforced,
    // and widening it would have been a decision disguised as a diff.
    /// Band fill, `0xAARRGGBB` from a NAMED token (C#-resolved, convention 7).
    /// `0` ⇒ fall back to `fill` (a band that merges with the body until wired).
    pub band_fill: u32,
    /// Strip sheet byte length. `0` ⇒ no strip data; body renders `fill` (D-04's
    /// degraded state and the pre-wiring state are the same drawing).
    pub strip_len: u32,
    /// Tile cell width in the sheet, px (the filmstrip crate's tile width;
    /// placeholder posters carry their own dims). Validated in `[1, 512]` before
    /// any use.
    pub strip_tile_w: u32,
    /// Tile cell height, px. Validated in `[1, 512]`.
    pub strip_tile_h: u32,
    /// Tiles per sheet row. Validated in `[1, 64]`.
    pub strip_tiles_per_row: u32,
    /// Total tiles the finished strip will hold. Validated in `[1, 4096]`.
    pub strip_total_tiles: u32,
    /// Tiles valid so far (D-14 progressive fill). `<= strip_total_tiles` or the
    /// strip is ignored. Also the residency revision: growth ⇒ re-upload.
    pub strip_completed_tiles: u32,
    /// Explicit padding, always `0` — keeps `strip_ptr` 8-aligned with NO implicit
    /// padding (the layout-canary discipline).
    pub strip_pad: u32,
    /// The sheet bytes: raw RGBA rows, `sheet_w = strip_tiles_per_row * strip_tile_w`.
    /// Borrowed for this call only, length-checked via `abi::slice_checked`-style
    /// validation BEFORE any slice construction.
    ///
    /// **Never dereferenced by plan 53.2-03** — the field is DEFINED here and CONSUMED
    /// by 53.2-05, which owns T-53.2-12's out-of-bounds-read mitigation.
    pub strip_ptr: *const u8,
    /// Source microseconds per tile (D-06's one fixed density). `0` for placeholders.
    pub strip_interval_us: i64,
    /// Residency key: C#-computed FNV-1a 64 of the media id (placeholders use the
    /// same hash with bit 0 flipped). Keyed per MEDIA, not per clip — D-07: many
    /// clips share one media's one strip.
    pub strip_key: u64,
}

// THE ELEVEN FIELDS ABOVE ARE APPENDED AT THE END, DELIBERATELY — the same discipline
// `RudisTimelineStats` records further down this file, applied to the struct that actually
// carries a pointer.
//
// Appending keeps EVERY existing offset valid: 0/4/8/12/16/20/24/28/32/40/48/56/64/72/80
// are byte-identical to what 52-04 committed and 52-06 mirrored, INCLUDING the two padding
// holes at 44 and 60. The only literal that moved is the total size, 88 -> 144, and both
// canaries (`tests/layout_canary.rs` here, `TimelineInteropLayoutTests.cs` on the C# side)
// were updated in the SAME commit that added the fields.
//
// 144 = 88 + 8*u32 (32 bytes, taking the struct to 120) + ptr + i64 + u64 (24 bytes). The
// eight u32s are an EVEN count on purpose: they land `strip_ptr` at 120, already 8-aligned,
// so appending introduces NO new implicit padding hole. `strip_pad` is the eighth of them
// and exists for exactly that reason — it is not slack, it is the alignment.

/// Bit 0 of [`RudisTimelineClip::flags`] — the clip is the single primary selection (D-08).
pub const FLAG_SELECTED: u32 = 1 << 0;
/// Bit 1 of [`RudisTimelineClip::flags`] — the clip has undetached audio (D-20).
pub const FLAG_HAS_AUDIO: u32 = 1 << 1;
/// Bit 2 of [`RudisTimelineClip::flags`] — **RESERVED, always 0.** See the field docs on
/// [`RudisTimelineClip::flags`] for why `agent_edited` / `agentEdited` cannot be rendered
/// this phase and must not be invented.
pub const FLAG_RESERVED_AGENT_EDITED: u32 = 1 << 2;
/// Bit 3 of [`RudisTimelineClip::flags`] — the strip fields carry D-13's poster
/// placeholder: draw ONE stretched quad across the body, not tiles. Set by the C# side
/// when it serves the poster fallback (plan 53.2-05 consumes it).
pub const FLAG_STRIP_PLACEHOLDER: u32 = 1 << 3;
/// Every bit this build understands. Anything outside it is ignored, not an error —
/// forward compatibility for a C# side that is a build ahead of this DLL.
///
/// Bit 2 stays OUT: the reservation above is a gap in the DOMAIN model, and claiming it
/// here would start drawing an agent-edited marker this build has no data for.
pub const FLAG_KNOWN_MASK: u32 = FLAG_SELECTED | FLAG_HAS_AUDIO | FLAG_STRIP_PLACEHOLDER;

/// One whole Timeline frame.
///
/// # `dirty`
///
/// `dirty == 0` means "nothing changed since the last render". The renderer returns
/// BEFORE acquiring a surface texture and increments `skipped_clean_frames`, so an idle
/// Timeline costs a function call and nothing else — no GPU work, no present, no vsync
/// wait. That is the redraw clause of the phase's criterion 1, implemented in the one
/// place that can actually honour it.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RudisTimelineFrame {
    /// Surface width in physical px (must match the last `attach`/`resize`).
    pub surface_w_px: u32,
    /// Surface height in physical px.
    pub surface_h_px: u32,
    /// DPI scale (`RasterizationScale`), e.g. `1.25`. The geometry above is ALREADY
    /// scaled; this is carried for the text pass's rasterisation size.
    pub scale: f32,
    /// `Timeline › TrackHeader` gutter width, physical px (46 logical, README:239). Drawn
    /// LAST over the lane area so a horizontal scroll cannot bleed under it — the gutter
    /// is sticky horizontally (README:138).
    pub gutter_w_px: f32,
    /// `Timeline › Toolbar` height, physical px (36 logical, README:239). That toolbar is
    /// a real XAML control drawn by the C# side; the renderer only needs to know how much
    /// vertical space is not its own.
    pub header_h_px: f32,
    /// `Timeline › Ruler` height, physical px (20 logical, README:239).
    pub ruler_h_px: f32,
    /// Lane array. May be null iff `lanes_len == 0`.
    pub lanes_ptr: *const RudisTimelineLane,
    /// Lane count. Bounded by `abi::MAX_LANES`.
    pub lanes_len: u32,
    /// Clip array — already viewport-culled by `TimelineModel.CullTo` (52-03 §3.6), so
    /// this is O(visible), not O(project).
    pub clips_ptr: *const RudisTimelineClip,
    /// Clip count. Bounded by `abi::MAX_CLIPS`.
    pub clips_len: u32,
    /// Ruler tick array.
    pub ticks_ptr: *const RudisTimelineTick,
    /// Tick count. Bounded by `abi::MAX_TICKS`.
    pub ticks_len: u32,
    /// Playhead x, physical px from the surface left. Non-finite ⇒ no playhead is drawn
    /// (and no error): a garbage coordinate must not become a plausible-looking line.
    pub playhead_x_px: f32,
    /// Drag/trim preview rectangles, drawn at 50% alpha over the real clips. Filled by
    /// plan 52-07's interaction state machine; null/0 until then.
    pub ghost_ptr: *const RudisTimelineClip,
    /// Ghost count. Bounded by `abi::MAX_GHOSTS` — a drag previews one clip, not a
    /// thousand.
    pub ghost_len: u32,
    /// Snap-guide x positions, physical px. Filled by plan 52-07; null/0 until then.
    pub snap_guides_ptr: *const f32,
    /// Snap-guide count. Bounded by `abi::MAX_SNAP_GUIDES`.
    pub snap_guides_len: u32,
    /// `0` ⇒ skip the present entirely. See the struct docs.
    pub dirty: u32,

    // ── APPENDED by plan 53.2-03, at the END, for the reason the clip struct records. ──
    /// D-01 band height, PHYSICAL px (14 LOGICAL px × `scale`, C#-computed).
    ///
    /// `<= 0` ⇒ **no band anywhere**, and the frame renders exactly as it did before this
    /// phase — same quad count, same label rect. That is not leniency, it is the
    /// back-compat clause: an older C# build sends a zeroed tail and must get the old
    /// picture rather than a subtly different one.
    ///
    /// Clamped per clip to that clip's own height by [`split_band`], so a lane shorter
    /// than the band is ALL band rather than a body with negative height (D-04: the band
    /// always draws, the body degrades first).
    pub band_h_px: f32,
    /// Explicit padding, always `0`. Without it the struct would carry an IMPLICIT
    /// 4-byte trailing hole to reach its 8-byte alignment, and this file's whole
    /// discipline is that padding is written down rather than inferred.
    pub band_pad: u32,
}

/// The band/body split for one clip, in PHYSICAL px.
///
/// Returns `(band_rect, body_rect)` as `[x, y, w, h]`. `band_h` is clamped to the clip's
/// OWN height, so a degenerate clip is ALL band and the body's height is `0` — D-04's
/// "every clip stays identifiable" expressed as arithmetic rather than as a comment.
///
/// # Why this is pure, and why it lives in the contract module
///
/// Three callers need the identical split and must not disagree: `quads.rs` draws the two
/// rectangles, `text.rs` clips the label to the band, and `waveform.rs` starts its bars
/// below it. A fourth (plan 53.2-05's textured pass) will fill the body. Any drift between
/// them is a label floating over a waveform, which renders fine and is wrong. So the split
/// is ONE function, it touches no GPU handle, and it is unit-tested directly.
///
/// # Degenerate input is SKIPPED, not clamped into something plausible
///
/// Non-finite or non-positive geometry yields two empty rects, which `quads.rs`'s own
/// `rect_is_drawable` then filters — the crate's existing posture (`quads.rs`'s "WHICH
/// LAYER OWNS WHICH CHECK"). A `NaN` band height is treated as no band at all rather than
/// propagated: `NaN` reaching a vertex buffer is a quad at an undefined position, and a
/// quad nobody can locate is worse than a quad nobody drew.
#[inline]
pub fn split_band(x: f32, y: f32, w: f32, h: f32, band_h_px: f32) -> ([f32; 4], [f32; 4]) {
    const EMPTY: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

    if !x.is_finite() || !y.is_finite() || !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0
    {
        return (EMPTY, EMPTY);
    }

    // `clamp` would panic on a NaN bound and propagate a NaN value; both are ruled out
    // above and here, in that order, so this cannot be turned into a panic by a caller.
    let band_h = if band_h_px.is_finite() {
        band_h_px.max(0.0).min(h)
    } else {
        0.0
    };

    ([x, y, w, band_h], [x, y + band_h, w, h - band_h])
}

/// Counters the C# side reads back through `rudis_timeline_stats`, and the channel plan
/// 52-09's introspection hook reports from.
///
/// Monotonic for the life of one attached renderer; a detach/attach cycle starts fresh.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RudisTimelineStats {
    /// Wall time of the most recent render that actually presented, microseconds.
    pub last_render_us: u64,
    /// Frames that reached `present()`.
    pub frames_rendered: u64,
    /// Frames refused at the door because `dirty == 0`. An idle Timeline drives this and
    /// nothing else, which is how "an idle Timeline costs nothing" becomes checkable
    /// rather than asserted.
    pub skipped_clean_frames: u64,
    /// Quads emitted by the most recent rendered frame — after non-finite geometry is
    /// filtered, so a skipped quad shows up as a smaller number rather than invisibly.
    pub quads_drawn: u32,
    /// Glyphs emitted by the most recent rendered frame.
    pub glyphs_drawn: u32,
    /// Cumulative recoverable present failures (timeout / validation).
    pub present_errors: u32,
    /// Cumulative lost-or-outdated surface events. The C# side treats this as a
    /// recoverable reattach, never an unhandled exception (T-52-19).
    pub device_lost: u32,

    // ── APPENDED by plan 52-08. See the note below before adding a third. ──
    /// Waveform bars in the most recent rendered frame — the subset of `quads_drawn` the
    /// audio fill contributed. Reported separately because "the waveform drew nothing" and
    /// "the frame drew nothing" are different problems with different causes, and a single
    /// total cannot tell them apart.
    pub waveform_quads_drawn: u32,
    /// Clips whose fill was refused in the most recent rendered frame because
    /// `waveform::MAX_WAVEFORM_QUADS_PER_FRAME` was exhausted (T-52-36). A cap nobody can
    /// see reached is a cap nobody can trust.
    pub waveform_truncated_clips: u32,

    // ── APPENDED by plan 53.2-05. Same discipline, same reasons. ──
    /// Filmstrip tiles in the most recent rendered frame. NOT a subset of `quads_drawn`,
    /// unlike the waveform pair above — tiles are a different pipeline's instances, so
    /// they are counted separately because they genuinely are separate.
    pub filmstrip_quads_drawn: u32,
    /// Clips whose tiles were refused because
    /// `filmstrip::MAX_FILMSTRIP_QUADS_PER_FRAME` was exhausted (T-53.2-20).
    ///
    /// Deliberately NOT incremented for a clip whose strip simply was not resident: the
    /// atlas refusing a strip and the frame budget refusing a clip are different problems
    /// with different fixes, and one counter that meant both would be unreadable. The
    /// atlas's side is visible in the two counters below.
    pub filmstrip_truncated_clips: u32,
    /// Strips resident in the atlas right now. A LEVEL, not a per-frame count: it answers
    /// "how much of the atlas is in use", which is a question about the present rather than
    /// about the last frame.
    pub atlas_resident_strips: u32,
    /// Cumulative atlas evictions since attach. A TOTAL, not a per-frame count, because
    /// "the atlas is thrashing" is a question about a trend — a per-frame number could not
    /// distinguish steady churn from one project-open burst.
    pub atlas_evictions: u32,
}

// THE SIX FIELDS ABOVE ARE APPENDED AT THE END, DELIBERATELY.
//
// This struct is layout-pinned in two languages: `tests/layout_canary.rs` here and
// `shell/Rudis.Shell.Tests/TimelineInteropLayoutTests.cs` on the C# side. Appending keeps
// EVERY existing offset valid — 0/8/16/24/28/32/36/40/44 are unchanged — so the only literal
// that moves is the total size (40 → 48 at plan 52-08, 48 → 64 at plan 53.2-05), and both
// canaries were updated in the SAME commit that added the fields, both times. Inserting them
// anywhere else would have shifted `present_errors` and `device_lost` under a C# mirror that
// reads them by offset, which is a silent corruption rather than a failed test. Anything a
// future plan adds goes at the end too.

// 64 = 3 * u64 + 10 * u32, with 8-byte alignment and no trailing padding: the ten u32s pair
// up exactly. Plan 53.2-05's four are an EVEN count for that reason — an odd number would
// have taken the struct to 60 rounded up to 64 anyway, but with a trailing hole nobody
// wrote down, and this file's whole discipline is that padding is written down rather than
// inferred.

/// The opaque handle `rudis_timeline_attach` returns. Defined in `lib.rs`; forward-declared
/// here so this module reads as the whole contract.
///
/// `c_void` is imported for the `attach` signature's panel pointer, which is a
/// `Microsoft.UI.Xaml.Controls.SwapChainPanel` COM pointer — see `surface.rs`.
pub type PanelPtr = *mut c_void;

#[cfg(test)]
mod tests {
    use super::*;

    /// The reserved bit must be reachable as a named constant and must NOT be inside the
    /// known mask — if a future edit "helpfully" adds it there, every clip flagged by a
    /// forward-dated C# build would start drawing an agent-edited marker this build has no
    /// data for.
    ///
    /// Plan 53.2-03 took the mask from `0b011` to `0b1011` by claiming bit 3 for the
    /// filmstrip placeholder. Bit 2 is asserted STILL excluded in the same breath,
    /// deliberately: the mask growing is exactly the moment someone would "tidy" the gap
    /// away, and the agent-edited reservation must survive a neighbour being added.
    #[test]
    fn reserved_bit_is_named_and_excluded_from_the_known_mask() {
        assert_eq!(FLAG_RESERVED_AGENT_EDITED, 4);
        assert_eq!(FLAG_KNOWN_MASK & FLAG_RESERVED_AGENT_EDITED, 0);
        assert_eq!(FLAG_STRIP_PLACEHOLDER, 8);
        assert_eq!(FLAG_KNOWN_MASK, 0b1011);
    }

    /// [`split_band`] is the one place three draw paths agree on where the band ends, so
    /// every degenerate input it can be handed must resolve to a rectangle, never to a
    /// `NaN` and never to a panic. The values reaching it are physical pixels the C# side
    /// computed from a user-controlled zoom, which is to say arithmetic, which is to say
    /// they can be anything.
    #[test]
    fn split_band_is_pure_and_clamped() {
        // The ordinary case first, so the degenerate ones below read as departures from a
        // stated norm rather than as a list of edge cases.
        let (band, body) = split_band(10.0, 20.0, 300.0, 48.0, 17.5);
        assert_eq!(band, [10.0, 20.0, 300.0, 17.5]);
        assert_eq!(body, [10.0, 37.5, 300.0, 30.5]);
        assert_eq!(band[3] + body[3], 48.0, "the split must not lose or invent height");

        // D-04: the band never out-runs the clip. A 10px-tall clip under a 17.5px band is
        // ALL band with no body left, rather than a body of height -7.5.
        let (band, body) = split_band(0.0, 0.0, 100.0, 10.0, 17.5);
        assert_eq!(band[3], 10.0);
        assert_eq!(body[3], 0.0);

        // No band, and a NEGATIVE band, are the same thing: the whole clip is body. A
        // negative height must not become a rectangle growing upwards out of the clip.
        for none in [0.0f32, -5.0] {
            let (band, body) = split_band(0.0, 0.0, 100.0, 48.0, none);
            assert_eq!(band[3], 0.0, "band_h_px = {none}");
            assert_eq!(body, [0.0, 0.0, 100.0, 48.0], "band_h_px = {none}");
        }

        // Non-finite anything is SKIPPED, never propagated. A `NaN` that reaches a vertex
        // buffer is a quad at an undefined position — worse than a quad nobody drew,
        // because nobody can find it either.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for (i, (x, y, w, h, band_h)) in [
                (bad, 0.0, 100.0, 48.0, 17.5),
                (0.0, bad, 100.0, 48.0, 17.5),
                (0.0, 0.0, bad, 48.0, 17.5),
                (0.0, 0.0, 100.0, bad, 17.5),
                (0.0, 0.0, 100.0, 48.0, bad),
            ]
            .into_iter()
            .enumerate()
            {
                let (band, body) = split_band(x, y, w, h, band_h);
                for v in band.iter().chain(body.iter()) {
                    assert!(
                        v.is_finite(),
                        "case {i} with {bad} leaked a non-finite value: {band:?} {body:?}"
                    );
                }
            }
        }

        // A non-finite BAND height specifically degrades to "no band" and leaves the clip
        // whole — the clip is still drawable, it just has no header.
        let (band, body) = split_band(0.0, 0.0, 100.0, 48.0, f32::NAN);
        assert_eq!(band[3], 0.0);
        assert_eq!(body, [0.0, 0.0, 100.0, 48.0]);

        // Degenerate clip geometry yields two EMPTY rects, which `quads::rect_is_drawable`
        // then filters — this function does not get to decide a clip is undrawable, it
        // just declines to invent a shape for one.
        assert_eq!(split_band(0.0, 0.0, 0.0, 48.0, 17.5), ([0.0; 4], [0.0; 4]));
        assert_eq!(split_band(0.0, 0.0, 100.0, -1.0, 17.5), ([0.0; 4], [0.0; 4]));
        assert_eq!(split_band(0.0, 0.0, 100.0, 0.0, 17.5), ([0.0; 4], [0.0; 4]));

        // PURE: no interior state, so the same inputs give the same answer forever. The
        // three call sites depend on exactly this — a split that drifted between them is a
        // label floating over a waveform, which renders fine and is wrong.
        assert_eq!(
            split_band(1.0, 2.0, 3.0, 4.0, 1.5),
            split_band(1.0, 2.0, 3.0, 4.0, 1.5)
        );
    }
}
