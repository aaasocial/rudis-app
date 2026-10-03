//! The C# shell's COMMITTED-INK mirror: the fade curve, the `Patch` state
//! machine, and the annotation draw dispatch (Phase 51, plan 51-02, task 1).
//!
//! # What this is a port OF, and why it is a re-implementation
//!
//! `src-tauri/src/canvas_overlay.rs` (the fade curve + mirror) and
//! `src-tauri/src/lib.rs`'s `OVERLAY_INK` / `draw_annotations_onto_styled` /
//! `frame_linked_marks` are the originals. Neither is frozen — but `crates/ffi`
//! **cannot link the shell crate at all** (that is the whole point of the C
//! ABI), so this is a re-implementation in the new crate rather than a `pub`
//! widening. Behaviour is reproduced EXACTLY, including the comments that
//! explain the non-obvious arms: the two implementations must composite the
//! same pixels or the Tauri preview and the C# preview disagree about what the
//! user drew.
//!
//! # The injected clock
//!
//! `is_visible` / `fade_alpha` / [`apply_patch_to_overlay`] / [`resurface_all`]
//! / [`visible_annotations_with_alpha`] ALL take `now` as a parameter and never
//! call `Instant::now()` themselves — the discipline the source module states in
//! its own doc comment, and what makes the fade window testable with pure
//! `Instant + Duration` arithmetic (no `thread::sleep` anywhere in the suite
//! below).
//!
//! # Not a second source of truth
//!
//! The backend `Store` remains authoritative (CLAUDE.md rule 4). This mirror is
//! presentational bookkeeping: it only ever REFRESHES to the store's
//! post-patch annotation list, and it can only ever be stale by one patch.
#![allow(
    dead_code,
    reason = "task 1 of 3: `panel` is a private module, so the ink constant, \
              the draw dispatch and the visible-set query read as dead until \
              task 3's `host.rs` calls them through the PreviewHost port. The \
              two that stay unwired after that (`is_visible`, `resurface_all`) \
              carry their own item-level allow with a recorded reason."
)]

use rudis_core::{Annotation, Patch, PatchKind};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Total lifetime of an annotation's ink in the overlay after it was last
/// shown. Past this window [`fade_alpha`] returns 0 and the present thread
/// stops drawing it — the annotation still lives in `Project.canvas`, only its
/// on-preview ink fades. 15s, per the Phase 14.1 UAT.
///
/// `pub(crate)` — see [`OVERLAY_INK`]'s note on why nothing in this private
/// module may be `pub`.
pub(crate) const FADE: Duration = Duration::from_secs(15);

/// How long the ink stays at FULL opacity before it begins to fade. From `HOLD`
/// to [`FADE`] the alpha ramps linearly to 0, so a mark is clearly visible for
/// a while and then gradually disappears rather than popping out.
///
/// `pub(crate)` — see [`OVERLAY_INK`]'s note.
pub(crate) const HOLD: Duration = Duration::from_secs(10);

/// The annotation ink colour: the `accent` design token `#6D54E8`, opaque.
///
/// ⚠ This is the SAME value `src-tauri/src/lib.rs:256` carries, and the two
/// must not drift (CLAUDE.md rule 7 — named design tokens, never raw hex). The
/// C# `accent` token in `shell/Rudis.Shell/Theme/Tokens.xaml` is the third
/// copy and is pinned by Phase 50's mechanical raw-hex gate. The overlay draws
/// this DASHED and fade-alpha'd; the agent-vision snapshot draws it SOLID —
/// same hue, so the model describes the mark the way the user sees it.
///
/// ⚠ `pub(crate)`, NOT `pub` — narrowed by plan 51-03. `cbindgen` parses this
/// crate's SOURCE with `syn` and does not honour module privacy: a `pub const`
/// here was emitted into the committed C header as `#define OVERLAY_INK
/// { 109, 84, 232, 255, }` even though `mod panel` is private and this is not
/// ABI surface. That is the same consumer-macro-namespace pollution
/// `cbindgen.toml`'s `exclude` list already guards `DEFAULT_CAPACITY` against;
/// here the honest fix is at the source, because `cbindgen` skips any item
/// that is not `pub`. [`FADE`] and [`HOLD`] are narrowed for the same reason
/// (they escape today only because `cbindgen` cannot evaluate
/// `Duration::from_secs`, which is luck, not design).
pub(crate) const OVERLAY_INK: [u8; 4] = [109, 84, 232, 255];

/// Renderer-side mirror of the canvas annotations currently drawable over the
/// live preview, plus the instant each was last shown (its fade anchor).
/// `Default` = an empty overlay (no ink).
#[derive(Default)]
pub struct CanvasOverlayMirror {
    /// The annotation bodies to draw, mirrored from `Project.canvas`.
    pub annotations: Vec<Annotation>,
    /// Per-annotation-id fade anchor: the instant it was last (re)shown.
    pub shown_since: HashMap<String, Instant>,
}

/// True while `now` is still within [`FADE`] of when the annotation was shown
/// (i.e. [`fade_alpha`] > 0). Pure function of two injected instants.
#[allow(
    dead_code,
    reason = "the ported predicate half of the fade window. `crates/preview`'s \
              overlay path consumes ONLY the alpha-carrying query below, so \
              this has no production caller in the C# host yet; it is kept \
              because it is half of the curve's contract and is pinned by its \
              own test."
)]
pub fn is_visible(shown_since: Instant, now: Instant) -> bool {
    now.duration_since(shown_since) < FADE
}

/// The ink opacity in `[0.0, 1.0]` for an annotation shown at `shown_since`,
/// evaluated at `now`: FULL (`1.0`) until [`HOLD`] elapses, then a linear ramp
/// down to `0.0` at [`FADE`], and `0.0` after. Pure function of two injected
/// instants.
pub fn fade_alpha(shown_since: Instant, now: Instant) -> f32 {
    let elapsed = now.duration_since(shown_since);
    if elapsed < HOLD {
        return 1.0;
    }
    if elapsed >= FADE {
        return 0.0;
    }
    // Linear ramp across the fade-out window [HOLD, FADE).
    let fade_span = (FADE - HOLD).as_secs_f32();
    if fade_span <= 0.0 {
        return 0.0;
    }
    let into_fade = (elapsed - HOLD).as_secs_f32();
    (1.0 - into_fade / fade_span).clamp(0.0, 1.0)
}

/// Refresh the overlay mirror in reaction to one `Patch`.
///
/// `project_canvas` is the AUTHORITATIVE post-patch annotation list (the
/// backend's ground truth, already filtered by [`frame_linked`]) — the mirror
/// refreshes to it rather than trying to reconstruct annotation bodies from the
/// patch alone, which carries only ids + kind, never full shapes. The clock is
/// the injected `now`.
///
/// Only canvas-affecting kinds mutate the mirror; any other kind (clip ops,
/// media-bin adds, turn markers) is a no-op — presentation-only, and it
/// self-corrects on the next canvas patch (threat T-14.1-02, ported).
pub fn apply_patch_to_overlay(
    mirror: &mut CanvasOverlayMirror,
    patch: &Patch,
    project_canvas: &[Annotation],
    now: Instant,
) {
    match patch.kind {
        PatchKind::AnnotationAdded => {
            // Refresh bodies to ground truth; anchor the newly added id(s) at now.
            mirror.annotations = project_canvas.to_vec();
            for id in &patch.ids {
                mirror.shown_since.insert(id.clone(), now);
            }
        }
        PatchKind::AnnotationRemoved => {
            mirror.annotations = project_canvas.to_vec();
            for id in &patch.ids {
                mirror.shown_since.remove(id);
            }
        }
        PatchKind::CanvasCleared => {
            // A SCOPED clear (Phase 14.2 — e.g. clearing only whiteboard-space
            // marks) must NOT blank the whole Preview overlay: the frame-linked
            // marks that survived are still in `project_canvas` (the
            // authoritative FrameLinked-only list `frame_linked` produced).
            // Refresh from that ground truth and drop ONLY the cleared ids,
            // exactly like AnnotationRemoved. An UNSCOPED clear passes an empty
            // `project_canvas`, so this still empties the overlay in that case.
            mirror.annotations = project_canvas.to_vec();
            for id in &patch.ids {
                mirror.shown_since.remove(id);
            }
        }
        PatchKind::CanvasRestored => {
            // A restore counts as freshly shown — re-anchor EVERY id at now.
            mirror.annotations = project_canvas.to_vec();
            for a in project_canvas {
                mirror.shown_since.insert(a.id.clone(), now);
            }
        }
        // AnnotationMoved (14.3) deliberately excluded: moves are
        // whiteboard-space, and this overlay is FrameLinked-only. Revisit if a
        // future phase adds frame-linked moves.
        // Any non-canvas kind (clip ops, media bin, turn markers) is a no-op.
        _ => {}
    }
}

/// Reset every tracked annotation's fade anchor to `now` — the batch re-show
/// (e.g. the user reopens the Canvas). Injected clock.
#[allow(
    dead_code,
    reason = "the ported `canvas-resurface` handler. The C ABI has no resurface \
              export yet (the Canvas region lands in plan 51-04); ported now \
              because it is part of the mirror's contract and is cheaper to \
              carry than to re-derive, and it is pinned by its own test."
)]
pub fn resurface_all(mirror: &mut CanvasOverlayMirror, now: Instant) {
    for anchor in mirror.shown_since.values_mut() {
        *anchor = now;
    }
}

/// Every still-visible mirrored annotation paired with its current
/// [`fade_alpha`] opacity — the set the present thread draws, each mark at its
/// own point on the ramp. Entries at alpha 0 (fully faded) are omitted
/// ENTIRELY rather than returned at zero.
pub fn visible_annotations_with_alpha(
    mirror: &CanvasOverlayMirror,
    now: Instant,
) -> Vec<(Annotation, f32)> {
    mirror
        .annotations
        .iter()
        .filter_map(|a| {
            let anchor = mirror.shown_since.get(&a.id)?;
            let alpha = fade_alpha(*anchor, now);
            (alpha > 0.0).then(|| (a.clone(), alpha))
        })
        .collect()
}

/// Draw every annotation onto `frame` in `ink`, denormalizing each normalized
/// `[0,1]` coordinate by the frame's pixel dimensions (`x * width`,
/// `y * height`) and dispatching to the matching `engine::annotate` primitive.
///
/// When `dashed`, line shapes route to the DASHED primitives
/// (`draw_*_dashed`); otherwise the SOLID ones. A `Label` ALWAYS renders as a
/// position marker regardless of `dashed` — there is no line to dash, and the
/// structured text carries the label's actual string.
///
/// Structure copied from `src-tauri/src/lib.rs:335`'s
/// `draw_annotations_onto_styled` exactly: the two implementations must
/// composite the same pixels, or preview and export disagree.
pub fn draw_annotations_onto_styled(
    frame: &mut engine::Frame,
    annotations: &[rudis_core::Annotation],
    ink: [u8; 4],
    dashed: bool,
) {
    let (w, h) = (frame.width as f64, frame.height as f64);
    for ann in annotations {
        match &ann.shape {
            rudis_core::AnnotationShape::Stroke { points } => {
                let px: Vec<(f64, f64)> = points.iter().map(|p| (p.x * w, p.y * h)).collect();
                if dashed {
                    engine::draw_stroke_dashed(frame, &px, ink);
                } else {
                    engine::draw_stroke(frame, &px, ink);
                }
            }
            rudis_core::AnnotationShape::Lasso { points } => {
                let px: Vec<(f64, f64)> = points.iter().map(|p| (p.x * w, p.y * h)).collect();
                if dashed {
                    engine::draw_lasso_dashed(frame, &px, ink);
                } else {
                    engine::draw_lasso(frame, &px, ink);
                }
            }
            rudis_core::AnnotationShape::Arrow { start, end } => {
                let (a, b) = ((start.x * w, start.y * h), (end.x * w, end.y * h));
                if dashed {
                    engine::draw_arrow_dashed(frame, a, b, ink);
                } else {
                    engine::draw_arrow(frame, a, b, ink);
                }
            }
            rudis_core::AnnotationShape::Label { position, .. } => {
                engine::draw_marker(frame, (position.x * w, position.y * h), ink);
            }
        }
    }
}

/// The FRAME-LINKED subset of the canvas — the marks that belong on real video.
/// This is the "authoritative list" every [`apply_patch_to_overlay`] call is
/// fed.
///
/// Whiteboard (`space == Whiteboard`, project-global) marks are EXCLUDED
/// structurally, so they can never composite onto the live preview (Phase 14.2
/// SC-2 no-leak; threat T-51-13). They live on a separate blank-board
/// rasterization sent to the agent.
///
/// Takes the `CanvasState` rather than the `&Project` the Tauri twin takes:
/// `rudis_core::Store` exposes `canvas()` but no borrowable `&Project`, and
/// `snapshot()` would deep-clone the entire project on every dispatched patch.
pub fn frame_linked(canvas: &rudis_core::CanvasState) -> Vec<rudis_core::Annotation> {
    canvas
        .annotations
        .iter()
        .filter(|a| a.space == rudis_core::AnnotationSpace::FrameLinked)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudis_core::{AnnotationShape, AnnotationSpace, NormPoint, PatchKind};

    fn stroke(id: &str) -> Annotation {
        Annotation {
            id: id.into(),
            shape: AnnotationShape::Stroke {
                points: vec![NormPoint { x: 0.1, y: 0.1 }, NormPoint { x: 0.2, y: 0.2 }],
            },
            linked_range_us: None,
            space: AnnotationSpace::FrameLinked,
        }
    }

    fn patch(kind: PatchKind, ids: &[&str]) -> Patch {
        Patch {
            kind,
            ids: ids.iter().map(|s| s.to_string()).collect(),
            entities: None,
        }
    }

    /// One frame-linked mark, added and anchored at `now`.
    fn seeded(now: Instant) -> CanvasOverlayMirror {
        let mut mirror = CanvasOverlayMirror::default();
        apply_patch_to_overlay(
            &mut mirror,
            &patch(PatchKind::AnnotationAdded, &["a1"]),
            &[stroke("a1")],
            now,
        );
        mirror
    }

    fn blank(width: u32, height: u32) -> engine::Frame {
        engine::Frame {
            width,
            height,
            rgba: vec![0u8; width as usize * height as usize * 4],
        }
    }

    fn inked_pixels(frame: &engine::Frame) -> usize {
        frame.rgba.chunks_exact(4).filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0).count()
    }

    // --- the fade curve, at the four points the plan names -----------------

    #[test]
    fn fade_alpha_is_full_at_zero_and_through_the_hold_window() {
        let t = Instant::now();
        assert_eq!(fade_alpha(t, t), 1.0, "freshly shown ink is fully opaque");
        assert_eq!(
            fade_alpha(t, t + Duration::from_secs(9)),
            1.0,
            "still full at 9s — inside the 10s HOLD"
        );
    }

    #[test]
    fn fade_alpha_is_half_at_the_midpoint_of_the_ramp() {
        let t = Instant::now();
        let mid = fade_alpha(t, t + Duration::from_millis(12_500));
        assert!(
            (mid - 0.5).abs() < 1e-6,
            "12.5s is the exact midpoint of the linear [HOLD, FADE) ramp; got {mid}"
        );
    }

    #[test]
    fn fade_alpha_is_zero_at_and_after_fade() {
        let t = Instant::now();
        assert_eq!(fade_alpha(t, t + FADE), 0.0, "gone exactly at FADE");
        assert_eq!(
            fade_alpha(t, t + Duration::from_secs(20)),
            0.0,
            "stays gone past FADE"
        );
    }

    #[test]
    fn is_visible_flips_exactly_at_fade() {
        let t = Instant::now();
        assert!(
            is_visible(t, t + Duration::from_millis(14_999)),
            "still visible one millisecond before FADE"
        );
        assert!(
            !is_visible(t, t + FADE),
            "not visible at FADE — the window is half-open [0, FADE)"
        );
    }

    // --- the four patch arms ----------------------------------------------

    #[test]
    fn annotation_added_refreshes_bodies_and_anchors_the_patched_ids() {
        let now = Instant::now();
        let mut mirror = CanvasOverlayMirror::default();
        apply_patch_to_overlay(
            &mut mirror,
            &patch(PatchKind::AnnotationAdded, &["a1"]),
            &[stroke("a1")],
            now,
        );
        assert_eq!(mirror.annotations.len(), 1, "bodies come from the authoritative list");
        assert_eq!(mirror.annotations[0].id, "a1");
        assert_eq!(
            mirror.shown_since.get("a1").copied(),
            Some(now),
            "the patched id is anchored at the injected `now`"
        );
    }

    #[test]
    fn annotation_removed_drops_only_the_patched_anchor() {
        let now = Instant::now();
        let mut mirror = seeded(now);
        apply_patch_to_overlay(
            &mut mirror,
            &patch(PatchKind::AnnotationAdded, &["a2"]),
            &[stroke("a1"), stroke("a2")],
            now,
        );
        apply_patch_to_overlay(
            &mut mirror,
            &patch(PatchKind::AnnotationRemoved, &["a2"]),
            &[stroke("a1")],
            now,
        );
        assert_eq!(mirror.annotations.len(), 1, "bodies refresh to ground truth");
        assert!(mirror.shown_since.contains_key("a1"), "the survivor keeps its anchor");
        assert!(!mirror.shown_since.contains_key("a2"), "only the patched id is dropped");
    }

    #[test]
    fn scoped_canvas_cleared_keeps_surviving_frame_linked_marks() {
        // Phase 14.2 regression, ported: a WHITEBOARD-scoped clear emits a
        // CanvasCleared whose ids are the whiteboard ids. The Preview overlay's
        // authoritative list still holds the frame-linked survivor, so blanking
        // the whole mirror would erase ink the user can still see.
        let now = Instant::now();
        let mut mirror = seeded(now);
        apply_patch_to_overlay(
            &mut mirror,
            &patch(PatchKind::CanvasCleared, &["wb1"]),
            &[stroke("a1")],
            now,
        );
        assert_eq!(mirror.annotations.len(), 1, "the frame-linked mark survives");
        assert_eq!(mirror.annotations[0].id, "a1");
        assert!(mirror.shown_since.contains_key("a1"), "and keeps its fade anchor");
    }

    #[test]
    fn unscoped_canvas_cleared_empties_the_overlay() {
        let now = Instant::now();
        let mut mirror = seeded(now);
        apply_patch_to_overlay(&mut mirror, &patch(PatchKind::CanvasCleared, &["a1"]), &[], now);
        assert!(mirror.annotations.is_empty());
        assert!(mirror.shown_since.is_empty());
    }

    #[test]
    fn canvas_restored_reanchors_every_id_in_the_authoritative_list() {
        let now = Instant::now();
        let later = now + Duration::from_secs(5);
        let mut mirror = CanvasOverlayMirror::default();
        apply_patch_to_overlay(
            &mut mirror,
            &patch(PatchKind::CanvasRestored, &["a1", "a2"]),
            &[stroke("a1"), stroke("a2")],
            later,
        );
        assert_eq!(mirror.annotations.len(), 2);
        assert_eq!(mirror.shown_since.get("a1").copied(), Some(later));
        assert_eq!(mirror.shown_since.get("a2").copied(), Some(later));
    }

    #[test]
    fn annotation_moved_and_non_canvas_kinds_are_noops() {
        let now = Instant::now();
        for kind in [
            PatchKind::AnnotationMoved,
            PatchKind::ClipTrimmed,
            PatchKind::MediaBinItemAdded,
        ] {
            let mut mirror = seeded(now);
            // A deliberately EMPTY authoritative list: a no-op arm must never
            // read it, so the mirror is unchanged either way.
            apply_patch_to_overlay(&mut mirror, &patch(kind, &["a1"]), &[], now);
            assert_eq!(mirror.annotations.len(), 1, "{kind:?} must not touch bodies");
            assert!(
                mirror.shown_since.contains_key("a1"),
                "{kind:?} must not touch fade anchors"
            );
        }
    }

    // --- the visible set and the FrameLinked-only filter --------------------

    #[test]
    fn visible_annotations_with_alpha_omits_fully_faded_entries() {
        let now = Instant::now();
        let mirror = seeded(now);
        let fresh = visible_annotations_with_alpha(&mirror, now);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].1, 1.0, "a fresh mark carries alpha 1.0");

        let mid = visible_annotations_with_alpha(&mirror, now + Duration::from_millis(12_500));
        assert_eq!(mid.len(), 1, "a mid-ramp mark is still drawn");
        assert!(mid[0].1 > 0.0 && mid[0].1 < 1.0, "at a partial alpha");

        let gone = visible_annotations_with_alpha(&mirror, now + FADE);
        assert!(gone.is_empty(), "an alpha-0 entry is omitted ENTIRELY, not returned at 0");
    }

    #[test]
    fn frame_linked_excludes_whiteboard_space_marks() {
        // T-51-13 / the Phase-14.2 SC-2 no-leak rule: a whiteboard-space mark
        // must never composite onto real video.
        let mut board = stroke("board-1");
        board.space = AnnotationSpace::Whiteboard;
        let canvas = rudis_core::CanvasState {
            annotations: vec![stroke("frame-1"), board],
        };
        let filtered = frame_linked(&canvas);
        assert_eq!(filtered.len(), 1, "only the FrameLinked mark is authoritative here");
        assert_eq!(filtered[0].id, "frame-1");

        // ...and it stays absent all the way through the mirror.
        let now = Instant::now();
        let mut mirror = CanvasOverlayMirror::default();
        apply_patch_to_overlay(
            &mut mirror,
            &patch(PatchKind::AnnotationAdded, &["frame-1", "board-1"]),
            &filtered,
            now,
        );
        let visible = visible_annotations_with_alpha(&mirror, now);
        assert_eq!(visible.len(), 1);
        assert!(
            !visible.iter().any(|(a, _)| a.id == "board-1"),
            "the whiteboard mark never reaches the live preview"
        );
    }

    #[test]
    fn resurface_all_resets_every_fade_anchor() {
        let now = Instant::now();
        let mut mirror = seeded(now);
        let later = now + Duration::from_secs(10);
        resurface_all(&mut mirror, later);
        assert_eq!(mirror.shown_since.get("a1").copied(), Some(later));
    }

    // --- the four-arm draw dispatch ----------------------------------------

    #[test]
    fn draw_dispatch_mutates_pixels_and_dashed_draws_less_than_solid() {
        let long = Annotation {
            id: "s".into(),
            shape: AnnotationShape::Stroke {
                points: vec![NormPoint { x: 0.02, y: 0.5 }, NormPoint { x: 0.98, y: 0.5 }],
            },
            linked_range_us: None,
            space: AnnotationSpace::FrameLinked,
        };

        let mut dashed = blank(256, 64);
        draw_annotations_onto_styled(&mut dashed, std::slice::from_ref(&long), OVERLAY_INK, true);
        let mut solid = blank(256, 64);
        draw_annotations_onto_styled(&mut solid, std::slice::from_ref(&long), OVERLAY_INK, false);

        assert!(inked_pixels(&dashed) > 0, "the dashed dispatch must actually draw");
        assert!(
            inked_pixels(&solid) > inked_pixels(&dashed),
            "`dashed = false` must route to the SOLID primitive, which covers more pixels \
             (solid {}, dashed {})",
            inked_pixels(&solid),
            inked_pixels(&dashed)
        );
    }

    #[test]
    fn draw_dispatch_routes_a_label_to_the_marker_regardless_of_dashed() {
        let label = Annotation {
            id: "l".into(),
            shape: AnnotationShape::Label {
                position: NormPoint { x: 0.5, y: 0.5 },
                text: "cut here".into(),
            },
            linked_range_us: None,
            space: AnnotationSpace::FrameLinked,
        };
        let mut dashed = blank(64, 64);
        draw_annotations_onto_styled(&mut dashed, std::slice::from_ref(&label), OVERLAY_INK, true);
        let mut solid = blank(64, 64);
        draw_annotations_onto_styled(&mut solid, std::slice::from_ref(&label), OVERLAY_INK, false);

        assert!(inked_pixels(&dashed) > 0, "a Label draws a marker disc");
        assert_eq!(
            dashed.rgba, solid.rgba,
            "a Label has no line to dash — both flags must reach `draw_marker` and produce \
             byte-identical pixels"
        );
    }

    #[test]
    fn lasso_and_arrow_reach_their_own_primitives() {
        // The remaining two arms of the dispatch, pinned the same way: each
        // shape draws, and its dashed form covers strictly fewer pixels.
        let lasso = Annotation {
            id: "lasso".into(),
            shape: AnnotationShape::Lasso {
                points: vec![
                    NormPoint { x: 0.05, y: 0.05 },
                    NormPoint { x: 0.95, y: 0.05 },
                    NormPoint { x: 0.5, y: 0.95 },
                ],
            },
            linked_range_us: None,
            space: AnnotationSpace::FrameLinked,
        };
        let arrow = Annotation {
            id: "arrow".into(),
            shape: AnnotationShape::Arrow {
                start: NormPoint { x: 0.02, y: 0.5 },
                end: NormPoint { x: 0.98, y: 0.5 },
            },
            linked_range_us: None,
            space: AnnotationSpace::FrameLinked,
        };
        for ann in [lasso, arrow] {
            let mut dashed = blank(256, 128);
            draw_annotations_onto_styled(&mut dashed, std::slice::from_ref(&ann), OVERLAY_INK, true);
            let mut solid = blank(256, 128);
            draw_annotations_onto_styled(
                &mut solid,
                std::slice::from_ref(&ann),
                OVERLAY_INK,
                false,
            );
            assert!(inked_pixels(&dashed) > 0, "{} draws", ann.shape.kind_label());
            assert!(
                inked_pixels(&solid) > inked_pixels(&dashed),
                "{}'s solid form must cover more pixels than its dashed form",
                ann.shape.kind_label()
            );
        }
    }

    #[test]
    fn the_ink_constant_is_the_accent_design_token() {
        // CLAUDE.md rule 7 + the Tauri twin (`src-tauri/src/lib.rs:256`): #6D54E8
        // at full alpha. Two copies of one token must never drift.
        assert_eq!(OVERLAY_INK, [109, 84, 232, 255]);
        assert_eq!(
            [OVERLAY_INK[0], OVERLAY_INK[1], OVERLAY_INK[2]],
            [0x6D, 0x54, 0xE8]
        );
    }
}
