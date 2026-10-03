//! Phase 46 (XTRC-02, plan 46-04): the PURE preview predicates, moved verbatim
//! out of `src-tauri/src/native_surface.rs`.
//!
//! Every function here is already shell-free — it takes plain Rust values
//! (`bool` / `i64` / `&str` / [`rudis_core::PatchKind`]) and returns a decision.
//! None of them names an app handle, managed state, or a shell event, so this is
//! the LOWEST-RISK batch of the extraction: nothing needed the
//! [`crate::PreviewHost`] / [`crate::PresentSink`] ports, and no call site had
//! to change. `native_surface.rs` keeps a `pub(crate) use preview::{…}` shim so
//! its own bare calls (`present_loop`, `apply_preview_region`,
//! `register_edit_seq_listener`, `edit_touches_playhead_audio`) still resolve.
//!
//! Widening `pub(crate)` to `pub` here is NOT an XTRC-04 public-API change: it
//! is this new crate's own surface, not `engine`'s / `core`'s / the agent
//! crates'. The XTRC-04 pub-item baseline deliberately excludes `crates/preview`.
//!
//! The bodies and doc comments below are byte-for-byte what `native_surface.rs`
//! held, with one exception noted inline on [`PreviewRect`] (whose doc stated its
//! old `pub(crate)` visibility as a fact).

/// `#preview-stage` rect as reported by the frontend: CSS px relative to the
/// main window's webview client area (top-left origin).
///
/// Originally `pub(crate)` in `native_surface.rs` (Quick 260726-e86) so the
/// shell's headless tests could construct one directly; now `pub` because it is
/// this crate's own surface and [`window_local_occluder`] takes it. It also
/// doubles as the per-occluder item type carried by the `preview-occluders`
/// event (same coordinate space), which is why the shell still deserializes it.
#[derive(Clone, Copy, Default, serde::Deserialize)]
pub struct PreviewRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Translate a webview-relative CSS-px occluder rect into WINDOW-LOCAL
/// physical px, relative to the video window's own client origin
/// (`preview_rect`'s origin) — the coordinate space `SetWindowRgn` /
/// `CreateRoundRectRgn` expect. Returns `None` when the translated rect does
/// not intersect the window's own `[0,0,win_w,win_h)` client rect (nothing to
/// punch) or is degenerate (zero width/height). Pure — no Win32 calls — so it
/// is headlessly unit-tested; the `#[cfg(windows)]` region builder
/// (`win_embed::apply_region`) is the only caller that isn't.
pub fn window_local_occluder(
    occluder: &PreviewRect,
    preview_rect: &PreviewRect,
    scale_factor: f64,
    win_w: i32,
    win_h: i32,
) -> Option<(i32, i32, i32, i32)> {
    let x = ((occluder.x - preview_rect.x) * scale_factor).round() as i32;
    let y = ((occluder.y - preview_rect.y) * scale_factor).round() as i32;
    let w = (occluder.width * scale_factor).round() as i32;
    let h = (occluder.height * scale_factor).round() as i32;
    if w <= 0 || h <= 0 {
        return None;
    }
    // AABB intersection against the window's own client rect.
    if x >= win_w || y >= win_h || x + w <= 0 || y + h <= 0 {
        return None; // fully outside — nothing to punch
    }
    Some((x, y, w, h))
}

/// Decide whether the LIVE preview-audio mix survives a boundary this tick
/// (18.3-01 / Part A of the superseded 18.2-06). Both mix builders
/// (resolve_audio_mix consumed by the single restart block, and
/// start_multi_audio) sum ALL timeline audio_contributors at absolute
/// offsets over their window — so while the live mix's span still covers
/// the playhead AND playback is CONTINUOUS (no Seek/Step reposition: the
/// mix's hardware clock cannot jump), a rebuilt mix would be
/// CONTENT-IDENTICAL. Reusing it kills both boundary audio gaps AND the
/// present-thread block of AudioOutput::drop (which joins a producer that
/// checks `stop` only between 2s windows). This decision is also the ring
/// buffer's audio model (preview-ring-buffer-design.md §4b).
/// See .planning/debug/multilayer-boundary-freeze.md (approach 3).
pub fn reuse_audio(
    span: Option<(i64, i64)>,
    has_live_mix: bool,
    position_us: i64,
    repositioned: bool,
) -> bool {
    if !has_live_mix || repositioned {
        return false;
    }
    span.is_some_and(|(s, e)| s <= position_us && position_us < e)
}

/// True iff a `PROJECT_CHANGED` patch can change what the SINGLE-LAYER preview
/// shows or sounds. Media-bin, annotation/canvas, and pure-notification kinds
/// never alter the on-screen video frame or its audible mix, so they must NOT
/// arm a playback-ring flush (importing media / drawing ink while playing must
/// not stutter the preview). Unknown/future kinds default to `true` (conservative
/// — a new clip mutation should invalidate rather than silently go stale).
pub fn patch_touches_preview(kind: rudis_core::PatchKind) -> bool {
    use rudis_core::PatchKind::*;
    !matches!(
        kind,
        MediaBinItemAdded
            | MediaBinItemRemoved
            | AnnotationAdded
            | AnnotationRemoved
            | CanvasCleared
            | CanvasRestored
            | AnnotationMoved
    )
}

/// True for edits whose effect on the VISIBLE clip cannot be ruled out by an
/// active-clip-id match alone: the clip SET / layout / global timebase changed
/// (a removed/merged clip leaves the active set; a whole-turn revert/apply can
/// restructure; project-settings changes the timebase globally). Add/Move/Split
/// are NOT structural — the post-mutation active clip's id lands in the patch's
/// `ids`, so the id match catches them precisely (and a cross-track move, whose
/// id is NOT the visible clip, correctly does not flush).
///
/// `ProjectSwitched` (MULTIPROJECT-UI): the ENTIRE clip set was replaced by a
/// `Store::from_project` swap — nothing the old active-clip id says survives,
/// so it is maximally structural (flush the ring / rebuild audio / re-present).
pub fn patch_is_structural(kind: rudis_core::PatchKind) -> bool {
    use rudis_core::PatchKind::*;
    matches!(
        kind,
        ClipRemoved
            | ClipMerged
            | TurnReverted
            | TurnApplied
            | ProjectSettingsChanged
            | ProjectSwitched
    )
}

/// Pure edit-flush GATE (headless-tested — `edit_flush_decision_matrix`). A
/// flush is *considered* only while PLAYING and when the edit seq changed since
/// the last tick. Paused edits do NOT flush: the paused branch re-presents on
/// its own (`present_still`) and the ring is already stopped there.
pub fn edit_flush_needed(cur_seq: u64, last_seen: u64, playing: bool) -> bool {
    playing && cur_seq != last_seen
}

/// Pure PAUSED-branch re-present gate (headless-tested —
/// `paused_represent_decision_matrix`; live-UAT PREVIEW-STALE fix). The paused
/// branch re-presents the still when ANY of:
///
/// 1. the (position, monitor) pair changed — a scrub / monitor switch (the
///    pre-existing trigger; a fast drag still coalesces to the newest value);
/// 2. an EXPLICIT seek was signaled (`seek_seq` bumped) — including the
///    frontend's deliberate no-op same-position seek after `project:changed`,
///    which expects a re-present handback (the old `transport()` synchronous
///    handback was removed when paused decoding moved off the UI thread, and
///    this pulse is its replacement);
/// 3. a preview-relevant edit bumped `PreviewEditSeq` — an agent/UI mutation
///    (transform, text, layout, clip edit, …) changes the COMPOSITE at an
///    UNCHANGED playhead, which the position-only trigger (1) can never see.
///
/// `present_still` re-resolves the store and re-composites from CURRENT state,
/// so a re-present always shows the edited frame; its `FrameCache` stays keyed
/// on immutable media identity (path, source_us, rotation), so an unchanged
/// frame (scrub-back, or a re-present whose layers' decodes are unaffected)
/// still hits the cache — no legitimate hit is regressed.
pub fn paused_represent_needed(
    position_us: i64,
    is_source: bool,
    last_paused: (i64, bool),
    seek_signaled: bool,
    edit_signaled: bool,
) -> bool {
    (position_us, is_source) != last_paused || seek_signaled || edit_signaled
}

/// Pure single-layer invalidation decision (18.3-03 Issue-B, headless-tested).
/// Given the clips a mid-play edit touched, the clip currently visible at the
/// playhead, the monitor, and whether the edit was structural, decide whether
/// the buffered ring is now STALE and must be flushed. A cross-track /
/// non-visible edit returns `false` — the producer reflects it on future frames
/// with NO visible drain-and-refill (the whole point of the fix).
pub fn edit_affects_single_layer(
    changed_ids: &[String],
    active_clip_id: Option<&str>,
    is_source: bool,
    structural: bool,
) -> bool {
    if is_source {
        // The Source monitor previews a MediaBin item, not the timeline — a
        // timeline edit never changes what it shows.
        return false;
    }
    if structural {
        return true; // clip set / layout / timebase changed — flush to be safe
    }
    active_clip_id.is_some_and(|id| changed_ids.iter().any(|c| c == id))
}

/// Pure timeline-span coverage: does clip `[start_us, start_us + (out_us-in_us))`
/// contain `position_us`? (Half-open — a clip ending exactly at the playhead is
/// the next clip's frame.) Extracted so the audio-invalidation overlap math is
/// headless-tested (`clip_span_covers_playhead`).
///
/// **Un-retimed clips ONLY** (quick task 260730-x2t): `out_us - in_us` is the
/// clip's SOURCE span, which equals its timeline occupancy only at speed 1.0.
/// Callers holding a whole [`rudis_core::Clip`] must use [`clip_covers`], which
/// goes through the retime-aware `Clip::timeline_end_us()`.
pub fn clip_span_covers(start_us: i64, in_us: i64, out_us: i64, position_us: i64) -> bool {
    start_us <= position_us && position_us < start_us + (out_us - in_us)
}

/// Retime-aware timeline-span coverage (quick task 260730-x2t): the
/// [`clip_span_covers`] twin for callers that hold the whole clip. Uses
/// `Clip::timeline_end_us()`, which reads the clip's PRECOMPUTED retimed
/// occupancy — so a 4 s source range at 2x correctly covers only 2 s of
/// timeline. Identical to [`clip_span_covers`] for an un-retimed clip.
pub fn clip_covers(clip: &rudis_core::Clip, position_us: i64) -> bool {
    clip.start_us <= position_us && position_us < clip.timeline_end_us()
}

/// Pure single-layer pop-target derivation (18.3-03 Issue-A pin). When a live
/// audio mix covers the range, video is AUDIO-MASTERED: the target is the mix's
/// timeline start plus the audio hardware clock (`AudioOutput::elapsed_us` —
/// output samples the device has consumed), so video tracks audio with NO
/// accumulating drift regardless of wall time. With no live mix it is the
/// wall-clock origin plus wall-elapsed. This is the ground truth the presenter
/// pops against; keeping it a pure fn pins "video is locked to the audio clock,
/// never wall time, while audio plays" against regression.
pub fn preview_target_us(
    audio_span_start: Option<i64>,
    audio_elapsed_us: Option<i64>,
    wall_origin_us: i64,
    wall_elapsed_us: i64,
) -> i64 {
    match (audio_span_start, audio_elapsed_us) {
        (Some(s), Some(e)) => s + e,                    // audio-master
        _ => wall_origin_us + wall_elapsed_us,          // video-only / gap
    }
}

/// Phase 49.1 (SEEK-04): how far the presentation clock may fall behind the
/// wall-projected transport schedule before a NEW audio span is re-anchored
/// forward to it. Must sit ABOVE ordinary pop/present jitter (a frame step or
/// two, ~33-67ms) and BELOW the smallest per-boundary pacing-clock stall
/// measured on the owner's real fixtures (~270,000-390,000us of
/// `AudioOutput::start_mix` open cost per boundary — WASAPI stream + ffmpeg
/// audio decode spin-up; see artifacts/49.1-01A-RULER-FIX.txt). 100ms = 3
/// frame steps: ~3x above jitter, ~2.7x below the smallest measured stall.
pub const RESYNC_LAG_THRESHOLD_US: i64 = 100_000;

/// Pure position-driven resync decision (SEEK-04, headless-tested —
/// `presentation_resync_decision_matrix`). Called at audio-span REBUILD time
/// (a clip boundary, or a fresh mix): true iff the presentation clock
/// (`target_us`, audio-mastered) has fallen behind the wall-projected
/// transport schedule (`wall_pos_us` = play-origin position + wall elapsed)
/// by MORE than [`RESYNC_LAG_THRESHOLD_US`] — strictly greater, so the
/// threshold itself never fires.
///
/// Why this exists (measured, 49.1-01A): each boundary's
/// `AudioOutput::start_mix` resets the audio hardware clock to 0 and
/// re-anchors `target_us` to the new span start, so the mix-open cost
/// (~0.27-0.39s on the owner's media) elapses in wall time while the pacing
/// clock stands still — and the clock carries the loss forward. Anchoring
/// the NEW span at the wall-projected position instead (skipping the
/// deficit's worth of content — SEEK-04's "drop/skip to re-sync") absorbs
/// each boundary's stall instead of compounding it.
///
/// The caller answers `true` by anchoring the new span at `wall_pos_us`
/// (never by rewinding: `wall_pos_us < target_us` — an audio clock AHEAD of
/// wall — must return false so presentation never jumps backward). No
/// cooldown latch is needed: the decision is only consulted at span-rebuild
/// ticks, which occur once per boundary by construction (`reuse_audio`
/// covers every within-span tick), so a flush-storm cannot arise.
pub fn presentation_resync_needed(target_us: i64, wall_pos_us: i64) -> bool {
    wall_pos_us - target_us > RESYNC_LAG_THRESHOLD_US
}
