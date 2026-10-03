//! Canvas-overlay compositing (plan 46-07): the ink pass that sits between a
//! decoded frame and the GPU surface, moved out of
//! `src-tauri/src/native_surface.rs` (`composite_with_overlay`,
//! `overlay_signature`, `repaint_overlay`).
//!
//! # What changed in the move, and what did not
//!
//! The ink ALGORITHM is verbatim: one draw call per committed annotation at its
//! own fade alpha, then the in-progress gesture trail as a plain dashed open
//! stroke, then composite. What changed is only where the three shell services
//! it needs come from — `resolve_overlay(app)`, `canvas_input::LIVE_GESTURE`,
//! `crate::OVERLAY_INK` and `crate::draw_annotations_onto_styled` are now
//! [`crate::PreviewHost::resolve_overlay`] / [`crate::PreviewHost::live_gesture`]
//! / [`crate::PreviewHost::overlay_ink`] / [`crate::PreviewHost::draw_ink`], and
//! the surface is reached through [`crate::PresentSink`] instead of through a
//! borrowed `NativePreview`.
//!
//! # Pitfall 2 — the reason [`crate::PresentSink::present_overlay`] exists
//!
//! The stored "current" frame is what a resize re-presents. Drawing ink into it
//! would bake the ink in and draw it AGAIN — doubling it — on the next resize,
//! so the original never mutated `np.frame`: it stored the CLEAN decoded frame
//! and composited a throwaway annotated clone.
//!
//! The wave-2 [`crate::PresentSink::present`] port stores exactly what it
//! composites, which cannot express that (recorded as D-46-03-02 three waves
//! before this one, precisely so it would not be discovered at runtime as a
//! doubled-ink resize bug). The resolution is a SECOND sink method that takes
//! the frame to store and the bytes to show separately:
//!
//! * no ink at all -> [`crate::PresentSink::present`] (store + composite the
//!   same pristine frame; the common-case fast path, zero clones here)
//! * ink, new frame -> `present_overlay(Some(clean), &annotated)`
//! * ink, paused repaint -> `present_overlay(None, &annotated)` — the stored
//!   frame is ALREADY the pristine one, so it must not be reassigned
//!
//! `np.frame` therefore only ever receives a clean frame, on every path.
//!
//! # Lock order
//!
//! The original resolved the overlay and read the live gesture while HOLDING
//! the `NativePreview` lock. Here both reads happen through the host BEFORE the
//! sink takes that lock, so the order is inverted and the GPU lock is held for
//! strictly less time. Neither read can block (both are `try_lock` with a
//! fallback inside the shell adapter), and neither is taken while the GPU lock
//! is held any more, so no new deadlock is reachable.
//!
//! # Scope
//!
//! ONLY the native preview present path reaches this module. `export_timeline`
//! / `composite_to_rgba` never do, so overlay ink can never leak into an
//! exported file (Pitfall 3 / threat T-14.1-07).

use crate::PresentContext;

/// Draw the CURRENT overlay onto a clone of `frame`, or return `None` when
/// there is nothing to draw.
///
/// `None` is the common-case ZERO-CLONE fast path (T-14.1-08): the caller
/// composites the pristine frame directly instead of copying it. Both public
/// entry points below share this so the "what does the ink look like right now"
/// decision exists exactly once.
fn annotated_clone(ctx: &PresentContext, frame: &engine::Frame) -> Option<engine::Frame> {
    let overlay = ctx.host().resolve_overlay();
    // The in-progress gesture (lock-free; empty off Windows / when idle).
    let live = ctx.host().live_gesture();

    // Common-case fast path: nothing to draw.
    if overlay.is_empty() && live.len() < 2 {
        return None;
    }

    // NEVER draw into the caller's frame (Pitfall 2) — only ever a clone.
    let mut annotated = frame.clone();
    let ink = ctx.host().overlay_ink();
    // Draw each committed mark at its OWN fade alpha (so a fading mark grows
    // gradually transparent). One draw call per annotation because each may be
    // at a different point in its fade ramp; the ink's RGB is the accent color,
    // the A channel is the fade alpha (engine::set_pixel alpha-composites it).
    for (ann, alpha) in &overlay {
        let a = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        if a == 0 {
            continue;
        }
        ctx.host().draw_ink(
            &mut annotated,
            std::slice::from_ref(ann),
            [ink[0], ink[1], ink[2], a],
            true,
        );
    }
    if live.len() >= 2 {
        // MVP (RESEARCH Open Question 1): render ANY in-progress tool as a plain
        // dashed OPEN stroke in the overlay ink. Tool-accurate live rendering
        // (dashed-closed lasso vs. open pen vs. arrow) is deferred discretion.
        // The live trail is only ever present mid-gesture, so it does NOT
        // participate in the fade clock.
        let (w, h) = (annotated.width as f64, annotated.height as f64);
        let px: Vec<(f64, f64)> = live
            .iter()
            .map(|(x, y)| (*x as f64 * w, *y as f64 * h))
            .collect();
        engine::draw_stroke_dashed(&mut annotated, &px, ink);
    }
    Some(annotated)
}

/// Composite `frame` onto the surface with any visible canvas annotations drawn
/// over it as DASHED accent ink, and remember `frame` — the CLEAN one — as the
/// current frame for the next resize re-present.
///
/// Returns `dims_changed` (see [`crate::PresentSink::present`]): whether this
/// frame's content dimensions differ from the previously shown one, which is
/// half of the caller's decision about re-emitting the canvas-viewport hint.
pub fn composite_with_overlay(
    ctx: &PresentContext,
    frame: &engine::Frame,
) -> Result<bool, engine::EngineError> {
    match annotated_clone(ctx, frame) {
        None => ctx.sink().present(frame),
        Some(annotated) => ctx.sink().present_overlay(Some(frame), &annotated),
    }
}

/// Cheap fingerprint of what the overlay would draw RIGHT NOW: the ids of the
/// currently-VISIBLE (un-faded) annotations plus the in-progress gesture's point
/// count. It changes exactly when a mark is added, removed, fades out, is
/// resurfaced, or the live trail grows — i.e. every moment a PAUSED frame must
/// be re-composited. Stable (no change) for an idle paused frame, so this never
/// busy-repaints.
///
/// This is what makes [`repaint_overlay`] IDEMPOTENT: the paused branch only
/// repaints when the signature CHANGES, and the signature is computed from
/// exactly the same two host reads the repaint itself draws from — so an
/// unchanged overlay produces an unchanged signature, no repaint, and therefore
/// no accumulation. (The other half of the guarantee is that a repaint never
/// mutates the stored frame; see this module's Pitfall-2 section.)
pub fn overlay_signature(ctx: &PresentContext) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let overlay = ctx.host().resolve_overlay();
    let live_len = ctx.host().live_gesture().len();
    let mut h = DefaultHasher::new();
    overlay.len().hash(&mut h);
    for (a, alpha) in &overlay {
        a.id.hash(&mut h);
        // Quantize the fade alpha into buckets so the signature CHANGES as a
        // mark fades out (→ the paused branch re-composites through the fade,
        // animating it) yet stays STABLE at full opacity (no idle churn).
        ((alpha * 64.0) as u8).hash(&mut h);
    }
    live_len.hash(&mut h);
    h.finish()
}

/// Re-composite the CURRENTLY stored decoded frame with the current overlay,
/// WITHOUT re-decoding — used while paused so newly drawn / removed / faded /
/// resurfaced ink (and the live gesture trail) update immediately. Previously
/// the overlay only reached the surface while playing, so ink drawn on a paused
/// frame was composited nowhere (CANV-01 visible-ink bug).
///
/// The stored frame is NEVER mutated: the ink goes onto a throwaway clone, and
/// the `None` first argument tells the sink to leave what it holds alone. Repeat
/// calls at an unchanged overlay therefore produce byte-identical output.
pub fn repaint_overlay(ctx: &PresentContext) {
    let Some(frame) = ctx.sink().current_frame() else {
        return; // no surface managed (mock runtime): nothing to re-present
    };
    let annotated = annotated_clone(ctx, &frame);
    let shown = annotated.as_ref().unwrap_or(&frame);
    if let Err(e) = ctx.sink().present_overlay(None, shown) {
        eprintln!("preview: paused overlay repaint failed: {e}");
    }
}
