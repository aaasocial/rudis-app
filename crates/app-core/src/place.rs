//! The MediaBin→Timeline placement command (`place_clip`, Phase 5) — relocated
//! from `src-tauri/src/lib.rs` by plan 47-01 (Phase 47, FFI-01; RESEARCH A1
//! row 11).
//!
//! # What changed, and what did not
//!
//! Body byte-copied with exactly three mechanical substitutions:
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `lock(&store)?` | `ctx.store().lock().map_err(..)?` — the SAME poisoned-mutex string that helper baked in (the `dispatch_command_inner` idiom) |
//! | `next_id("clip")` | [`crate::import::next_id`]`("clip")` — the identical function; it moved here in 45-07 and `src-tauri` reaches it through a re-export shim |
//! | `emit_changed(&app, &patch, base_seq, seq)?` | `ctx.emit_patch(&patch, base_seq, seq)?` — behavior-identical for the Tauri build BY CONSTRUCTION: `TauriAppCtx::emit_patch` IS a delegation to the unchanged `emit_changed`, so the `#[serde(flatten)]`ed LAT-02 envelope is byte-identical |
//!
//! Nothing else. Every error string, the `DEFAULT_STILL_DURATION_US`
//! still-image defaulting, and the `drop(guard)`-before-emit ordering are
//! carried verbatim. The `#[tauri::command]` stays in `src-tauri` as a thin
//! wrapper (the macro cannot resolve a `&impl AppCtx` parameter) — the exact
//! `dispatch_command` split from 45-12.

use crate::AppCtx;
use rudis_core::{Clip, Command};

/// Place a WHOLE MediaBin item on a track: builds a `Clip` with `in=0` /
/// `out=media.duration_us` at `start_us` (negative start clamps to 0 in the
/// core) and dispatches an undoable `AddClip`. Returns the placed clip (the
/// renderer learns the backend-generated clip id from it).
///
/// Kind-vs-track validation (M1): video tracks take video media; audio
/// tracks take audio media (or a video's audio — Phase 6 refines detach).
/// Still images probe duration 0 and get the shared
/// `DEFAULT_STILL_DURATION_US` placement length (live-UAT
/// GENERATE-IMAGE-UNPLACEABLE fix, backlog 999.3 — same default as the
/// agent's placeClip/add_clips/insert_clips tools).
///
/// The `#[tauri::command] fn place_clip` in `src-tauri` is now exactly this
/// call plus a ctx construction (plan 47-01; RESEARCH A1 noted everything it
/// needs — `ctx.store()`, `ctx.emit_patch()` — was already on [`AppCtx`]).
pub fn run_place_clip<C: AppCtx>(
    ctx: &C,
    media_id: String,
    track: usize,
    start_us: i64,
) -> Result<Clip, String> {
    let mut guard = ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?;

    let (media_kind, has_audio, duration_us) = {
        let item = guard
            .media_item(&media_id)
            .ok_or_else(|| format!("media bin item not found: {media_id}"))?;
        (item.media_kind, item.has_audio, item.duration_us)
    };
    let track_kind = guard
        .timeline()
        .tracks
        .get(track)
        .map(|t| t.kind)
        .ok_or_else(|| {
            format!(
                "track index {track} out of range ({} tracks exist)",
                guard.timeline().tracks.len()
            )
        })?;

    // Shared with Command::MoveClipToTrack (crates/core) -- ONE compatibility
    // rule, kept in one place. This function's own error text is unchanged.
    if !rudis_core::track_accepts_media(track_kind, media_kind, has_audio) {
        return Err(format!(
            "cannot place {media_kind:?} media on a {track_kind:?} track"
        ));
    }
    // A still image probes duration 0: place it with the SAME default length
    // the agent placement tools use (never a zero-length clip — AddClip's
    // out_us > in_us invariant stays intact). The still stays trimmable to
    // any positive length afterward.
    let duration_us = if duration_us > 0 {
        duration_us
    } else {
        rudis_core::tools::DEFAULT_STILL_DURATION_US
    };

    let clip = Clip {
        id: crate::import::next_id("clip"),
        media_id,
        start_us: start_us.max(0),
        in_us: 0,
        out_us: duration_us,
        volume: 1.0,
        audio_detached: false,
        // A BRAND-NEW clip placed from the MediaBin has no parent to inherit
        // from: identity visuals are semantically correct (Phase 18; contrast
        // the SplitClip/DetachAudio struct-update inheritance in core).
        transform: rudis_core::ClipTransform::default(),
        opacity: 1.0,
        crop: rudis_core::ClipCrop::default(),
        // New MediaBin clip: default alpha interpretation (Phase 28 OVL-01).
        alpha_mode: rudis_core::AlphaMode::default(),
        // New clip: no animation yet (Phase 19).
        keyframes: rudis_core::KeyframeTracks::default(),
        // A MediaBin placement is never a text clip (Phase 20 AddText mints those).
        text: None,
        // A newly placed clip plays at 1:1 (quick task 260730-x2t). Retime is
        // an explicit, undoable `Command::SetClipRetime` — never an implicit
        // property of placement. Kept exhaustive on purpose: a future Clip
        // field must fail the build here, not silently default-fill.
        retime: None,
    };
    let (patch, base_seq, seq) = guard
        .dispatch(Command::AddClip {
            track,
            clip: clip.clone(),
        })
        .map_err(|e| e.to_string())?;
    drop(guard);
    ctx.emit_patch(&patch, base_seq, seq)?;
    Ok(clip)
}
