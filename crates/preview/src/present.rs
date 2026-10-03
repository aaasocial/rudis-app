//! Frame presentation (plan 46-07): the ONE seam every present-path branch
//! funnels through — single-layer play, multi-layer paused/scrub, gaps and the
//! startup placeholder — moved out of `src-tauri/src/native_surface.rs`
//! (`present_frame`, `black_frame`, `placeholder_frame`).
//!
//! This is the wave that makes [`crate::PresentSink`] load bearing: the actual
//! on-screen GPU present now flows through the port boundary this whole phase
//! exists to build, and the HOST supplies only the adapter — today
//! `crates/ffi`'s `panel::sink`, at the time of the move `src-tauri`.
//!
//! # What changed in the move
//!
//! The original took the `NativePreview` lock ONCE and did the
//! dirty-reconfigure, the `dims_changed` compare, the frame store, the overlay
//! composite and the config-size read all inside it. The port splits that into
//! [`crate::PresentSink::reconfigure_if_dirty`] and the composite call, so the
//! lock is taken twice. That difference was measured and accepted three waves
//! ago (D-46-03-02 item 2): the only other takers of that mutex use `try_lock`
//! and neither mutates `frame`/`config_*` in a way the other could corrupt, so
//! the worst case is one redundant reconfigure — never a wrong pixel.
//!
//! Ordering, error handling and the emit condition are otherwise verbatim: the
//! canvas-viewport hint fires only when the surface was reconfigured OR the
//! frame's content dimensions changed, and a failed composite logs and emits
//! nothing.

use crate::PresentContext;

/// Present one frame onto the video surface and remember it as the "current"
/// frame (so a later resize re-presents it). No-ops when the surface is not
/// managed (mock runtime). Never panics.
///
/// Takes the frame BY VALUE, as the original did, so callers keep handing over
/// ownership of a freshly decoded/composited frame rather than being made to
/// keep it alive.
///
/// Phase 49 (plan 49-01, OQ4): returns whether the present SUCCEEDED, so the
/// present loop can report a landing stamp
/// ([`crate::PresentSink::note_presented_stamp`]) only for frames that
/// actually reached the sink — never for a failed composite. Additive-only:
/// every pre-49 caller ignores the value (statement position) and is
/// untouched; the error logging is unchanged.
pub fn present_frame(ctx: &PresentContext, frame: engine::Frame) -> bool {
    // If a resize marked the surface dirty and the shell's reposition couldn't
    // reconfigure (it lost the GPU-lock race mid-playback), reconfigure to the
    // window's current size HERE before presenting — otherwise
    // get_current_texture returns Outdated and the preview freezes.
    let geometry_changed = ctx.sink().reconfigure_if_dirty().is_some();
    match crate::overlay::composite_with_overlay(ctx, &frame) {
        Ok(dims_changed) => {
            // Phase 13 spike: the frame CONTENT rect (not the letterboxed
            // stage) moved, so tell the frontend where it sits — the ink-layer
            // overlay matches the visible video exactly off this hint. A new
            // surface size or a new frame size both move it.
            if geometry_changed || dims_changed {
                let (cw, ch) = ctx.sink().configured_size();
                ctx.host()
                    .emit_canvas_viewport(cw, ch, frame.width, frame.height);
            }
            true
        }
        Err(e) => {
            eprintln!("preview: present frame failed: {e}");
            false
        }
    }
}

/// A tiny fully-black frame (letterboxes to an all-black surface) — shown in a
/// timeline gap or when nothing is loaded.
pub fn black_frame() -> engine::Frame {
    engine::Frame {
        width: 2,
        height: 2,
        rgba: vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
    }
}

/// The "no media loaded" placeholder: a subtle vertical gradient (dark violet
/// top -> near-black bottom) so the live GPU surface is visibly rendering in the
/// Preview hole and distinguishable from the flat opaque UI panels around it.
/// Replaced by real decoded frames when media loads. The shader stretches this
/// across the quad, so a small texture is fine at any size.
pub fn placeholder_frame(width: u32, height: u32) -> engine::Frame {
    let (w, h) = (width.max(1), height.max(1));
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h {
        // 0.0 at top -> 1.0 at bottom.
        let t = y as f32 / (h.max(2) - 1) as f32;
        let r = (26.0 * (1.0 - t) + 8.0 * t) as u8; // 0x1a -> 0x08
        let g = (16.0 * (1.0 - t) + 8.0 * t) as u8; // 0x10 -> 0x08
        let b = (36.0 * (1.0 - t) + 12.0 * t) as u8; // 0x24 -> 0x0c
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 0xFF;
        }
    }
    engine::Frame {
        width: w,
        height: h,
        rgba,
    }
}
