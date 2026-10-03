//! `ShellPreviewHost` — the C# shell's SHELL-SERVICES adapter (Phase 51, plan
//! 51-02, task 3): the second implementation of the frozen
//! `preview::PreviewHost` port.
//!
//! # Traceability rule for this file
//!
//! Every method names the `src-tauri/src/preview_host.rs` line range of the
//! `TauriPreviewHost` method it twins. Where this host DIVERGES it says so and
//! says why, so a reader can tell a decision from an omission. There are
//! exactly two divergences, both recorded decisions rather than gaps:
//!
//! * [`ShellPreviewHost::live_gesture`] returns empty **by design** (D-13 — the
//!   in-progress trail is XAML-drawn, one visual layer above the panel).
//! * [`ShellPreviewHost::emit_canvas_viewport`] is an empty **documented no-op**
//!   (D-03 — the push has no consumer under `SwapChainPanel`; the same geometry
//!   is pulled through `rudis_preview_content_rect`).
//!
//! # Ownership
//!
//! Every field is an `Arc` CLONE of a `RudisCtx` field — never a raw ctx
//! pointer and never a borrow — so the present thread's `Send + Sync +
//! 'static` requirement is compiler-proven rather than asserted. That is the
//! T-50-01 discipline `self_advance::spawn` already follows.
//!
//! # The rule that holds on every method
//!
//! A poisoned lock degrades (an empty overlay, a `None` store) and NEVER
//! unwinds a second time (T-51-03) — the same contract `ffi_guard!`'s doc
//! comment already states for the poisoned store, and the same one
//! `app-core`'s `*_inner` bodies state when they map a poisoned mutex to a
//! recoverable error.
#![allow(
    dead_code,
    reason = "plan 51-03 constructs this and hands it to \
              preview::PresentContext at first attach. Task 3 delivers the \
              adapter itself; its behaviour is covered by this module's tests."
)]

use std::sync::{Arc, Mutex};

thread_local! {
    /// One reusable text rasterizer per thread — shaping + glyph caches are hot
    /// and content is stable across ticks (Phase 20, TEXT-01).
    ///
    /// This is a THREAD-LOCAL and must stay one: "one rasterizer per present
    /// thread" is a hot-cache performance invariant, not incidental placement.
    /// `TextRasterizer::new` only parses the bundled Inter bytes (offline, no
    /// OS font scan) and is deterministic, so preview and export produce
    /// byte-identical rasters no matter WHICH instance ran — that is the MAD-0
    /// parity proof. Twins `preview_host.rs:69-86`.
    static TEXT_RASTERIZER: std::cell::RefCell<engine::TextRasterizer> =
        std::cell::RefCell::new(engine::TextRasterizer::new());
}

pub(crate) struct ShellPreviewHost {
    store: Arc<app_core::SharedStore>,
    mirror: Arc<preview::PlaybackMirror>,
    overlay: Arc<Mutex<super::overlay::CanvasOverlayMirror>>,
    /// Phase 57 (plan 57-08, PLAY-05): the ctx's OWN engine→shell diagnostic
    /// mirror — the far end of `rudis_get_playback_resolution_level`.
    ///
    /// Carried per instance rather than taking `PreviewHost::engine_diag`'s
    /// process-wide default, for the same reason `mirror` above is per instance:
    /// D-07 says two `RudisCtx`s share no state, and a ctx whose getter answered
    /// with some other ctx's producer level would be a quiet exception to that.
    diag: Arc<preview::EngineDiag>,
}

impl ShellPreviewHost {
    pub(crate) fn new(
        store: Arc<app_core::SharedStore>,
        mirror: Arc<preview::PlaybackMirror>,
        overlay: Arc<Mutex<super::overlay::CanvasOverlayMirror>>,
        diag: Arc<preview::EngineDiag>,
    ) -> Self {
        Self {
            store,
            mirror,
            overlay,
            diag,
        }
    }
}

impl preview::PreviewHost for ShellPreviewHost {
    /// Twins `TauriPreviewHost::store` (`preview_host.rs:126-129`). `None` on a
    /// POISONED mutex, never a panic — the graceful-degradation rule
    /// `ffi_guard!`'s doc comment already states for the poisoned store, and
    /// the reason `app-core` maps a poisoned lock to a recoverable error rather
    /// than unwinding again.
    ///
    /// Divergence: the Tauri twin also answers `None` when the store is simply
    /// not MANAGED (the mock runtime). A `RudisCtx` always owns exactly one
    /// store, so that case does not exist here.
    fn store(&self) -> Option<std::sync::MutexGuard<'_, rudis_core::Store>> {
        self.store.lock().ok()
    }

    /// Twins `TauriPreviewHost::playback_mirror` (`preview_host.rs:135-140`).
    ///
    /// Divergence, deliberate: the Tauri host carries a `fallback_mirror`
    /// because its real one lives in managed state that the mock runtime never
    /// manages, while the port returns a plain `&PlaybackMirror` rather than an
    /// `Option`. This host ALWAYS owns a real mirror (`RudisCtx::mirror` exists
    /// from construction and `rudis_get_playback_position` already reads it),
    /// so no fallback instance is needed. That is an absent PROBLEM, not an
    /// absent feature.
    fn playback_mirror(&self) -> &preview::PlaybackMirror {
        &self.mirror
    }

    /// Phase 57 (plan 57-08, PLAY-05/D-01): this ctx's own [`preview::EngineDiag`].
    ///
    /// The port's DEFAULT body answers with a process-wide instance, which is
    /// what the Tauri adapter and every test double take (and why D-01 holds —
    /// no shell file changed for PLAY-05 to work). This host overrides it so
    /// `rudis_get_playback_resolution_level` reads the level THIS ctx's producer
    /// wrote. There is no Tauri twin to point at: the Tauri shell has no getter
    /// for it, because rendering the level is post-cutover work that v8 does not
    /// do.
    fn engine_diag(&self) -> &preview::EngineDiag {
        &self.diag
    }

    /// Twins `TauriPreviewHost::resolve_overlay` (`preview_host.rs:151-181`),
    /// ported verbatim including its cache.
    ///
    /// The present thread MUST NEVER block on the overlay mirror, so this uses
    /// `try_lock` with a THREAD-LOCAL last-known cache: if the (low-frequency)
    /// dispatch-side writer momentarily holds the mirror, this tick reuses the
    /// previous visible set rather than stalling the GPU present (T-14.1-05,
    /// re-registered as T-51-11). Each calling thread keeps its own cache, so
    /// no cross-thread state is threaded through the callers.
    fn resolve_overlay(&self) -> Vec<(rudis_core::Annotation, f32)> {
        use std::cell::RefCell;
        thread_local! {
            static LAST_VISIBLE: RefCell<Vec<(rudis_core::Annotation, f32)>> = const { RefCell::new(Vec::new()) };
        }
        // Bind the lock Result to a local so its (Err-variant, poison-guard)
        // temporary drops before the guard does — the same discipline the
        // sink's present path uses.
        let lock_result = self.overlay.try_lock();
        match lock_result {
            Ok(guard) => {
                let visible = super::overlay::visible_annotations_with_alpha(
                    &guard,
                    std::time::Instant::now(),
                );
                LAST_VISIBLE.with(|c| *c.borrow_mut() = visible.clone());
                visible
            }
            // Contended by the (rare) dispatch-side write, or poisoned — reuse
            // the last visible set, never block the present thread.
            Err(_) => LAST_VISIBLE.with(|c| c.borrow().clone()),
        }
    }

    /// D-13: under `SwapChainPanel` the in-progress gesture trail is drawn by
    /// XAML, in the same visual tree, one layer above the panel — it never
    /// crosses the ABI at all. The engine therefore composites COMMITTED ink
    /// only, and this port returns empty BY DESIGN, not because the capability
    /// is missing.
    ///
    /// The costed fallback, if the hand-off ever shows a visible seam
    /// (51-RESEARCH "Pattern: The Ink Hand-off"): feed live points through a
    /// new lock-free publish export into a buffer read here, restoring today's
    /// single-drawing-path model at the cost of one present tick of stroke
    /// latency. Plan 51-05 Task 1 observes the seam and records the decision;
    /// do not switch without that evidence.
    ///
    /// (The Tauri twin, `preview_host.rs:186-191`, reads a `Mutex<Vec<..>>`
    /// filled by a WndProc subclass. That subclass exists only because a Win32
    /// child HWND swallowed input the WebView never saw — a problem this shell
    /// does not have, and D-11 retires rather than ports.)
    fn live_gesture(&self) -> Vec<(f32, f32)> {
        Vec::new()
    }

    /// Twins `TauriPreviewHost::overlay_ink` (`preview_host.rs:194-196`):
    /// a pass-through to the `accent` design-token constant.
    fn overlay_ink(&self) -> [u8; 4] {
        super::overlay::OVERLAY_INK
    }

    /// Twins `TauriPreviewHost::draw_ink` (`preview_host.rs:199-207`):
    /// a pass-through to the four-arm shape dispatch.
    fn draw_ink(
        &self,
        frame: &mut engine::Frame,
        annotations: &[rudis_core::Annotation],
        ink: [u8; 4],
        dashed: bool,
    ) {
        super::overlay::draw_annotations_onto_styled(frame, annotations, ink, dashed);
    }

    /// Twins `TauriPreviewHost::rasterize_text` (`preview_host.rs:218-238`):
    /// a PASS-THROUGH to the ONE shared `app_core::rasterize_text_layer`, fed
    /// this thread's cached rasterizer.
    ///
    /// Deliberately a pass-through and NEVER a duplicate: preview and export
    /// must composite text byte-identically, and they do so by calling the same
    /// function. The port's signature takes no rasterizer — supplying one is
    /// precisely this adapter's job, which is why `TEXT_RASTERIZER` lives here.
    fn rasterize_text(
        &self,
        text: &rudis_core::TextPayload,
        transform: engine::LayerTransform,
        opacity: f32,
        crop: engine::LayerCrop,
        project_w: u32,
        project_h: u32,
    ) -> engine::Layer {
        TEXT_RASTERIZER.with(|r| {
            app_core::rasterize_text_layer(
                &mut r.borrow_mut(),
                text,
                transform,
                opacity,
                crop,
                project_w,
                project_h,
            )
        })
    }

    /// D-03: a documented NO-OP.
    ///
    /// The trait method stays on the frozen `PreviewHost` (removing it would
    /// modify `crates/preview`; its removal is Phase 55 cleanup, together with
    /// the shell it was written for). Under `SwapChainPanel` the ink overlay is
    /// an ordinary XAML sibling in the same visual tree, so the contain-fit
    /// viewport PUSH has no consumer — nothing subscribes, nothing listens, and
    /// there is no window to compute a scale factor from. The same geometry is
    /// PULLED on demand through `rudis_preview_content_rect` (plan 51-03),
    /// published by `ShellPresentSink` on every composite path.
    ///
    /// ⚠ The body must stay literally `{}`. Plan 51-01's mechanical gate
    /// (`forbidden_hole_punch_symbols_are_absent_from_the_shipped_path`)
    /// extracts it and asserts exactly that, so a stray statement here turns
    /// the build red on purpose: the allow-list excuses the NAME, never a
    /// re-implementation of the retired behaviour.
    fn emit_canvas_viewport(&self, _win_w: u32, _win_h: u32, _frame_w: u32, _frame_h: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use preview::PreviewHost;
    use rudis_core::{AnnotationShape, AnnotationSpace, NormPoint, Patch, PatchKind};

    fn mark(id: &str) -> rudis_core::Annotation {
        rudis_core::Annotation {
            id: id.into(),
            shape: AnnotationShape::Stroke {
                points: vec![NormPoint { x: 0.1, y: 0.1 }, NormPoint { x: 0.4, y: 0.4 }],
            },
            linked_range_us: None,
            space: AnnotationSpace::FrameLinked,
        }
    }

    /// A host over a fresh store/mirror and the given overlay mirror. The
    /// overlay is seeded BEFORE it is wrapped, so no test ever needs to take
    /// that lock — the no-unwrapped-lock rule (T-51-03) holds in the tests too,
    /// and the mechanical gate on this file stays clean of the very token it
    /// forbids.
    fn host_over(overlay: super::super::overlay::CanvasOverlayMirror) -> ShellPreviewHost {
        ShellPreviewHost::new(
            Arc::new(Mutex::new(rudis_core::Store::default())),
            Arc::new(preview::PlaybackMirror::new()),
            Arc::new(Mutex::new(overlay)),
            Arc::new(preview::EngineDiag::new()),
        )
    }

    #[test]
    fn store_is_readable_through_the_port() {
        let host = host_over(Default::default());
        let guard = host.store().expect("a healthy store mutex answers Some");
        assert!(
            guard.canvas().annotations.is_empty(),
            "a fresh store has an empty canvas"
        );
    }

    #[test]
    fn live_gesture_is_empty_by_design() {
        // D-13, not a gap: the in-progress trail is XAML-drawn one layer above
        // the panel and never crosses the ABI.
        let host = host_over(Default::default());
        assert!(host.live_gesture().is_empty());
    }

    #[test]
    fn overlay_ink_is_the_accent_design_token() {
        let host = host_over(Default::default());
        assert_eq!(host.overlay_ink(), [109, 84, 232, 255]);
    }

    #[test]
    fn resolve_overlay_is_empty_on_a_fresh_mirror_and_sees_a_committed_mark() {
        let host = host_over(Default::default());
        assert!(host.resolve_overlay().is_empty(), "no ink has been committed");

        let now = std::time::Instant::now();
        let mut seeded = super::super::overlay::CanvasOverlayMirror::default();
        super::super::overlay::apply_patch_to_overlay(
            &mut seeded,
            &Patch {
                kind: PatchKind::AnnotationAdded,
                ids: vec!["a1".into()],
                entities: None,
            },
            &[mark("a1")],
            now,
        );
        let host = host_over(seeded);
        let visible = host.resolve_overlay();
        assert_eq!(visible.len(), 1, "the committed mark reaches the present path");
        assert_eq!(visible[0].0.id, "a1");
        assert_eq!(visible[0].1, 1.0, "freshly committed ink is fully opaque");
    }

    #[test]
    fn draw_ink_reaches_the_shape_dispatch() {
        let host = host_over(Default::default());
        let mut frame = engine::Frame {
            width: 64,
            height: 64,
            rgba: vec![0u8; 64 * 64 * 4],
        };
        host.draw_ink(&mut frame, &[mark("a1")], host.overlay_ink(), true);
        assert!(
            frame.rgba.iter().any(|b| *b != 0),
            "the port must actually mutate pixels, not silently drop the call"
        );
    }

    #[test]
    fn emit_canvas_viewport_is_an_observable_no_op() {
        // D-03. Calling it changes nothing anywhere — there is no event, no
        // window and no consumer. The MECHANICAL half of this claim (the body
        // is literally `{}`) is asserted by plan 51-01's gate in the C# tier;
        // this half proves the call is reachable and harmless.
        let host = host_over(Default::default());
        host.emit_canvas_viewport(1920, 1080, 1280, 720);
        host.emit_canvas_viewport(1, 1, 1, 1);
        assert!(host.resolve_overlay().is_empty());
        assert!(host.live_gesture().is_empty());
    }

    #[test]
    fn the_playback_mirror_is_the_instance_own_never_a_fallback() {
        let mirror = Arc::new(preview::PlaybackMirror::new());
        let host = ShellPreviewHost::new(
            Arc::new(Mutex::new(rudis_core::Store::default())),
            Arc::clone(&mirror),
            Arc::new(Mutex::new(Default::default())),
            Arc::new(preview::EngineDiag::new()),
        );
        assert!(
            std::ptr::eq(host.playback_mirror(), Arc::as_ptr(&mirror)),
            "the port must hand back the ctx's OWN mirror -- a fallback instance \
             would read as 'not playing, position 0' forever"
        );
    }
}
