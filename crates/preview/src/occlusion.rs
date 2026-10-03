//! Phase 60 (OCCL-01), plan 60-07: the occlusion predicate — *may the layers
//! underneath this one be thrown away before anything decodes them?*
//!
//! # The bar this module is held to
//!
//! OCCL-01 is a CORRECTNESS requirement wearing a performance requirement's
//! clothes: **a culled composite must be pixel-identical to the un-culled one,
//! MAD 0.0000, zero differing bytes.** Culling that changes any pixel is a bug,
//! not an optimization. Everything below is therefore written to REFUSE by
//! default and to accept only what it can prove, and the proof that it works is
//! not in this file — it is `tests/occlusion_pin.rs`, which composites real
//! decoded media both ways through the production path and diffs the bytes.
//! A predicate tested only against its own logic proves only that it agrees
//! with itself.
//!
//! # Where the cull is applied, and why there
//!
//! [`cull_occluded`] is called from [`crate::multilayer::resolve_multilayer`] as
//! its FINAL step, not from the `ring.rs` live-tick call site. `resolve_multilayer`
//! is the one gather every consumer shares — the live tick, the boundary prewarm
//! and the exit discovery (which both resolve stacks at FUTURE positions), and the
//! paused/scrub present path. A call-site-only cull would leave the prewarm
//! re-opening decode sessions for layers the live tick had just culled, silently
//! undoing OCCL-02's payoff; placed in the gather, every consumer sees a
//! position-correct culled stack and `layer_sessions::select_hw_clip_ids` never
//! even sees a hidden layer in its `eligible` set.
//!
//! # The predicate, rung by rung
//!
//! A layer is a full-canvas occluder only when ALL of these hold. Any one of them
//! failing leaves the layer — and everything under it — untouched:
//!
//! | Rung | Why it is required |
//! |---|---|
//! | `text.is_none()` | a rasterized glyph run is mostly transparent, and a text spec has no probed source to ask |
//! | `!is_image_sequence` | a `%0Nd` pattern is not one probeable container; its per-frame alpha is unknown and need not even agree frame to frame |
//! | `opacity == 1.0` EXACTLY | see "No epsilon" below |
//! | identity transform | `position (0,0)`, `scale (1,1)`, `rotation_deg 0.0`, each compared exactly |
//! | identity crop | all four insets exactly `0.0` |
//! | `rotation == 0` | see "Two reasons for the rotation rung" below |
//! | `reports_alpha.is_proven_opaque()` | `Unknown` is not evidence of opacity (plan 60-06) |
//! | `src_width > 0 && src_height > 0` | zero dims mean the mirror never learned this media's shape |
//! | canvas coverage | the contain-fit of the source into the canvas leaves ZERO letterbox margin — computed by MIRRORING the compositor's own arithmetic, see below |
//!
//! ## No epsilon, anywhere
//!
//! `opacity == 1.0` is an exact float compare, and so is every geometry compare.
//! That is deliberate and it is the whole discipline in one line: an epsilon that
//! culled at opacity `0.9999` would let 0.01 % of the layer beneath show through
//! the occluder — a real, measurable pixel difference, and an instant MAD-bar
//! failure. A conservative predicate refuses the ambiguous case; it does not
//! round it in its own favour. (`NaN == 1.0` is false, so a poisoned opacity
//! refuses too, with no special case.)
//!
//! ## Two reasons for the rotation rung, not one
//!
//! `LayerTransform::rotation_deg` must be `0.0` because a rotated quad can leave
//! the canvas's own corners uncovered even when the un-rotated rect covered it
//! (60-RESEARCH Pitfall 5). `LayerSpec::rotation` — the SOURCE display rotation —
//! must be `0` for an independent reason: at 90/270 the decoder transposes the
//! frame, so the decoded frame's dims are the TRANSPOSE of `src_width`/`src_height`
//! and the coverage arithmetic below would be computing with the wrong shape.
//!
//! ## The coverage check MIRRORS `layer_params_bytes_from`; it does not re-derive it
//!
//! `engine`'s `layer_params_bytes_from` (`compositor.rs:516`) is the ONE place a
//! transform + crop becomes GPU uniform bytes, and its own doc states the rule:
//! *"the CROPPED (visible) SOURCE is CONTAIN-FIT (letterboxed, never fit-cropped)
//! inside that rect"*. So "does this layer paint every canvas pixel?" is not a
//! question about the destination rect — it is a question about whether the
//! contain-fit inside that rect has zero letterbox margin.
//!
//! [`fills_dest_exactly`] answers it by re-walking that function's own steps in
//! its own order and calling the very same public [`engine::contain_fit_viewport`]
//! the compositor calls, then requiring the fitted quad to equal the destination
//! rect exactly. A second, independently-derived aspect test (`a*d == b*c`, say)
//! would be a second opinion about geometry, and a second opinion that disagrees
//! by one pixel at one aspect ratio is precisely how a cull changes pixels.
//!
//! ## Coverage is checked at EVERY size the shipped pipeline can composite at
//!
//! The predicate runs in the gather, which knows only the PROJECT canvas. The
//! composite does not always happen at that size, and the layer does not always
//! decode at its source's size:
//!
//! * `ring.rs` composites at a PLAY-05 ladder size — that axis halves each
//!   dimension independently under sustained overload and floors at
//!   `dynres::MIN_DIM`, so a degraded canvas is not always the same aspect as the
//!   full one (the ladder enumerates its own sizes for this predicate through
//!   [`crate::dynres::every_composite_size`]; this file never names a level);
//! * a PLAYING-path layer may decode from a PROXY (Phase 58/D-05), whose dims come
//!   from a rounded, snapped-to-even policy and are therefore NOT always the
//!   source's exact aspect — measured: a 2704x1520 source proxies to 960x540,
//!   which contain-fits into a 2704x1520 canvas with a ~1 px letterbox.
//!
//! Both are real ways a "covers the canvas" verdict taken at one size becomes
//! false at another, so [`covers_every_composite_size`] requires the fill to be
//! exact at the cartesian product of {source dims, proxy dims if any} x {the raw
//! canvas, Full, Half, Quarter}. That is a strict narrowing: it refuses some
//! layers that would in fact have covered, and refusing costs frames while
//! accepting wrongly costs pixels.
//!
//! # What is DEFERRED — left un-culled rather than guessed at
//!
//! Recorded here because each of these is a payoff this module knowingly declines
//! (60-RESEARCH A4, Pitfalls 4/5):
//!
//! * **Rotation coverage geometry.** A rotated quad big enough to still contain the
//!   canvas AABB is a legitimate occluder. Deciding that needs real geometry with
//!   its own edge cases (scale compensating for rotation, non-square canvases) and
//!   is not needed for OCCL-01/02's stated payoff.
//! * **Non-identity transforms that still cover.** A layer scaled to 1.2 at
//!   position (-0.1, -0.1) covers the canvas. Un-culled here.
//! * **Crop with a matching aspect.** A crop reshapes the fitted quad (the CR-01
//!   fix), so a cropped layer CAN fill a rect its uncropped self would letterbox
//!   inside. Un-culled here.
//! * **Letterboxed layers inside a covering dest rect** stay un-culled by
//!   construction — that is Pitfall 4 and it is the case the coverage rung exists
//!   to catch, not a deferral.
//!
//! # Two honest caveats, published rather than rounded away
//!
//! 1. **A source that MISREPORTS its alpha will be culled behind.** `reports_alpha`
//!    is probe metadata (threat T-60-25). The mitigation is not trust: it is that
//!    the failure mode is a visual defect on a hostile/broken file, caught by the
//!    pixel pin, never anything worse.
//! 2. **If the occluder itself fails to decode, culled and un-culled differ.**
//!    Un-culled, a failed layer is skipped and whatever is beneath shows through;
//!    culled, there is nothing beneath and the frame is black. Both are degraded
//!    output for a broken file and neither is what the user asked for, but they are
//!    not the same pixels. Every MAD proof in `occlusion_pin.rs` runs on media that
//!    decodes, which is the case the requirement is about.
//!
//! # Not a blend-mode model
//!
//! There is no blend-mode concept anywhere in `crates/engine`, `crates/preview` or
//! `crates/core` (60-RESEARCH verified this by exhaustive grep), so this module
//! adds no speculative generality for one. The occluder's own
//! [`engine::AlphaMode`] is likewise irrelevant once the source is proven opaque:
//! `Straight` premultiplies by `texel.a` (= 1) and `Premultiplied` skips that
//! multiply, so both write source alpha 1.0, and the LOCKED premultiplied-over
//! blend `(One, OneMinusSrcAlpha)` then scales the destination by exactly zero.
//! Whatever was underneath is gone either way — which is the pixel-level reason a
//! full-canvas opaque layer makes everything below it dead.

use std::sync::atomic::{AtomicU64, Ordering};

use engine::{LayerCrop, LayerTransform};

use crate::dynres::{every_composite_size, COMPOSITE_SIZE_COUNT};
use crate::multilayer::LayerSpec;

/// Is `spec` a layer that provably paints EVERY pixel of a `canvas_w x canvas_h`
/// canvas, opaquely?
///
/// See the module doc for the full rung list, the no-epsilon rule and the
/// deliberately deferred cases. `false` is always a safe answer; `true` is a
/// claim that everything below this layer can be discarded without changing a
/// single byte of output.
pub fn is_full_canvas_occluder(spec: &LayerSpec, canvas_w: u32, canvas_h: u32) -> bool {
    // Media kinds this predicate does not reason about at all.
    if spec.text.is_some() || spec.is_image_sequence {
        return false;
    }
    // The compositing scalar. EXACT — see the module doc's no-epsilon rule.
    // `NaN == 1.0` is false, so a poisoned opacity refuses here with no special
    // case of its own.
    if spec.opacity != 1.0 {
        return false;
    }
    if !is_identity_transform(&spec.transform) || !is_identity_crop(&spec.crop) {
        return false;
    }
    // SOURCE display rotation. At 90/270 the decoded frame is the transpose of
    // the dims the coverage arithmetic below reads.
    if spec.rotation != 0 {
        return false;
    }
    // The alpha rung. `is_proven_opaque` is an exhaustive, wildcard-free match
    // (plan 60-06), so `Unknown` has to refuse by name rather than by omission.
    if !spec.reports_alpha.is_proven_opaque() {
        return false;
    }
    // Unknown source shape: the mirror never learned this media's dims.
    if spec.src_width == 0 || spec.src_height == 0 {
        return false;
    }
    covers_every_composite_size(spec.src_width, spec.src_height, canvas_w, canvas_h)
}

/// Layers the cull has actually thrown away, process-wide, since start.
///
/// Quick task `260825-mgq`: OBSERVABILITY ONLY, and the
/// [`crate::framedrop::FRAMEDROP_SKIPPED_TICKS`] idiom exactly -- a plain
/// relaxed counter that exists so a test can prove the mechanism ENGAGED on a
/// real run instead of inferring it from configuration. Nothing reads it to
/// decide anything, there is no C ABI getter and no `EngineDiag` field, and
/// [`cull_occluded`]'s behaviour is identical with or without it: the
/// no-occluder path -- which is every layer of every BENCH-02 fixture as
/// committed, since those carry `reports_alpha: None` -- never reaches the
/// `fetch_add` at all.
///
/// **Snapshot and difference; never read the absolute.** The render-cache
/// WRITER reaches this same cull through
/// `render_cache_writer::render_segment` -> [`crate::multilayer::resolve_multilayer`]
/// (`render_cache_writer.rs:384`), and so do the boundary prewarm and the exit
/// discovery, none of which is the live producer. An absolute reading therefore
/// mixes warm-time and prewarm-time culls into a playback-time question. Every
/// consumer brackets its measured row with a before/after pair.
pub static OCCLUSION_CULLED_LAYERS: AtomicU64 = AtomicU64::new(0);

/// Drop every layer hidden behind the topmost full-canvas occluder.
///
/// `layers` is TRACK-ORDERED with index 0 = top (the compositor's 18-02
/// contract: index 0 is painted LAST), so this is a top-down walk. At the first
/// index `i` that [`is_full_canvas_occluder`] accepts, everything at index
/// `> i` is dead and is truncated away. Layers ABOVE the occluder (index `< i`)
/// are KEPT — they paint over it.
///
/// Truncation ONLY. This function never empties a non-empty vec, never reorders,
/// and never removes a layer from the middle.
pub fn cull_occluded(layers: &mut Vec<LayerSpec>, canvas_w: u32, canvas_h: u32) {
    for i in 0..layers.len() {
        if is_full_canvas_occluder(&layers[i], canvas_w, canvas_h) {
            // Everything at a LATER index is painted EARLIER and is completely
            // covered by this layer, whatever its own opacity/alpha/geometry —
            // the premultiplied-over blend scales the destination by
            // `1 - src_alpha`, and this layer writes src_alpha 1.0 everywhere.
            // Quick task 260825-mgq: count what is actually removed, BEFORE
            // the truncation makes it uncountable. Observability only; see
            // `OCCLUSION_CULLED_LAYERS`. On the no-occluder path this line is
            // never reached, so the predicate's cost is unchanged.
            OCCLUSION_CULLED_LAYERS.fetch_add((layers.len() - (i + 1)) as u64, Ordering::Relaxed);
            layers.truncate(i + 1);
            return;
        }
    }
}

/// Does the contain-fit of a `src_w x src_h` source into a `dest_w x dest_h`
/// destination rect leave ZERO letterbox margin?
///
/// **A MIRROR of `engine`'s `layer_params_bytes_from` (`compositor.rs:516`) for
/// the identity-transform / identity-crop case, not a re-derivation.** Same
/// steps, same order, and the fit itself is the very same public
/// [`engine::contain_fit_viewport`] the compositor calls — so this can never
/// disagree with what the shader actually draws. If that function's geometry
/// ever changes, this must change with it; a cross-reference sits at
/// `layer_params_bytes_from`'s doc pointing back here.
fn fills_dest_exactly(src_w: u32, src_h: u32, dest_w: f32, dest_h: f32) -> bool {
    // --- mirrored from layer_params_bytes_from, in its order ---------------
    // Its degenerate-rect guard. Under an identity transform the dest rect IS
    // `scale (1,1) * (out_w, out_h)`, so this is the same `!(> 0)` reject that
    // makes the compositor treat the draw as a no-op.
    if !(dest_w > 0.0 && dest_h > 0.0) || !dest_w.is_finite() || !dest_h.is_finite() {
        return false;
    }
    // Its `native.max(1) as f32` and its crop-adjusted effective dims. With an
    // identity crop each `(1.0 - inset - inset)` factor is exactly 1.0 — the
    // byte-for-byte identity-crop invariant that function's own doc states — so
    // the factors are written out rather than folded away, to keep this a mirror
    // of the same steps and not a shortcut past them.
    let native_w = src_w.max(1) as f32;
    let native_h = src_h.max(1) as f32;
    let effective_w = native_w * (1.0 - 0.0 - 0.0);
    let effective_h = native_h * (1.0 - 0.0 - 0.0);
    // Its fit — literally the same public function, not a second implementation
    // of the same formula.
    let [_, _, fit_w, fit_h] = engine::contain_fit_viewport(dest_w, dest_h, effective_w, effective_h);
    // --- the question this module adds ------------------------------------
    // Zero letterbox margin, on both axes, exactly. `contain_fit_viewport`
    // centres the fitted quad, so any shortfall at all is a band of canvas this
    // layer does not paint — and half a pixel of the layer beneath showing
    // through is still a pixel difference.
    fit_w == dest_w && fit_h == dest_h
}

/// The composite sizes the shipped pipeline can ask for, given a project canvas.
///
/// Two sources, and neither is enumerated here:
/// * the RAW canvas — what the paused/scrub present path (`present_multilayer`)
///   and the render-cache writer composite at;
/// * every rung of the PLAY-05 ladder, asked of [`crate::dynres`] itself
///   ([`every_composite_size`]) rather than re-listed in this file, so a rung
///   added there cannot leave this predicate silently stale. See that function's
///   doc for why the enumeration lives beside the ladder.
fn composite_sizes(canvas_w: u32, canvas_h: u32) -> [(u32, u32); 1 + COMPOSITE_SIZE_COUNT] {
    let mut out = [(canvas_w, canvas_h); 1 + COMPOSITE_SIZE_COUNT];
    out[1..].copy_from_slice(&every_composite_size(canvas_w, canvas_h));
    out
}

/// Does a source of `src_w x src_h` fill the canvas exactly at EVERY size the
/// pipeline can composite at, decoding from EITHER the original or its proxy?
///
/// See the module doc: this is the narrowing that keeps a gather-time verdict
/// true at composite time.
fn covers_every_composite_size(src_w: u32, src_h: u32, canvas_w: u32, canvas_h: u32) -> bool {
    // The dims this layer can actually arrive at the compositor with: its own,
    // or — on the PLAYING decode arms — the proxy substitute for its source.
    // The proxy question is asked through `decode_source`, the one file in this
    // crate that may know what a proxy is.
    let source_dims = [
        Some((src_w, src_h)),
        crate::decode_source::proxy_substitute_dims(src_w, src_h),
    ];
    for (cw, ch) in composite_sizes(canvas_w, canvas_h) {
        for dims in source_dims.iter().flatten() {
            if !fills_dest_exactly(dims.0, dims.1, cw as f32, ch as f32) {
                return false;
            }
        }
    }
    true
}

/// Identity transform: dest rect is the whole canvas, unrotated. Exact compares
/// (see the module doc's no-epsilon rule).
fn is_identity_transform(t: &LayerTransform) -> bool {
    t.position == (0.0, 0.0) && t.scale == (1.0, 1.0) && t.rotation_deg == 0.0
}

/// Identity crop: all four insets exactly zero, so the fit is sized from the
/// native source dims (`layer_params_bytes_from`'s stated byte-for-byte
/// identity-crop invariant).
fn is_identity_crop(c: &LayerCrop) -> bool {
    c.left == 0.0 && c.top == 0.0 && c.right == 0.0 && c.bottom == 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudis_core::SourceAlpha;
    use std::path::PathBuf;

    /// A spec that PASSES every rung on a 1920x1080 canvas: a 1280x720 opaque
    /// video, identity everything. Each test below breaks exactly ONE rung, so
    /// a failure names the rung that stopped working.
    fn occluder_spec() -> LayerSpec {
        LayerSpec {
            clip_id: "occ".into(),
            path: PathBuf::from("occluder.mp4"),
            source_us: 0,
            rotation: 0,
            remaining_dur_us: 5_000_000,
            src_step_us: 33_333,
            retime_ramped: false,
            opacity: 1.0,
            transform: LayerTransform::default(),
            crop: LayerCrop::default(),
            alpha_mode: engine::AlphaMode::Straight,
            is_image_sequence: false,
            seq_fps: 0.0,
            is_still_image: false,
            src_width: 1280,
            src_height: 720,
            reports_alpha: SourceAlpha::Opaque,
            text: None,
        }
    }

    const CW: u32 = 1920;
    const CH: u32 = 1080;

    fn accepts(spec: &LayerSpec) -> bool {
        is_full_canvas_occluder(spec, CW, CH)
    }

    fn accepts_on(spec: &LayerSpec, canvas_w: u32, canvas_h: u32) -> bool {
        is_full_canvas_occluder(spec, canvas_w, canvas_h)
    }

    // ---------------------------------------------------------------- accepts

    #[test]
    fn accepts_an_opaque_identity_layer_whose_dims_are_the_canvas() {
        let mut s = occluder_spec();
        s.src_width = CW;
        s.src_height = CH;
        assert!(accepts(&s), "src dims == canvas dims must be an occluder");
    }

    #[test]
    fn accepts_a_same_aspect_source_that_contain_fits_with_no_margin() {
        // 1280x720 into 1920x1080: scale 1.5 exactly, so the fitted quad IS the
        // canvas. This is the common real case ("cut to a full-screen clip").
        assert!(accepts(&occluder_spec()));
    }

    // --------------------------------------------------------------- refusals

    #[test]
    fn refuses_opacity_a_hair_below_one() {
        let mut s = occluder_spec();
        s.opacity = 0.999_999;
        assert!(
            !accepts(&s),
            "no epsilon: 0.999999 lets the layer beneath show through"
        );
    }

    #[test]
    fn refuses_a_non_finite_opacity() {
        let mut s = occluder_spec();
        s.opacity = f32::NAN;
        assert!(!accepts(&s));
    }

    #[test]
    fn refuses_a_scaled_transform() {
        let mut s = occluder_spec();
        s.transform.scale = (1.001, 1.0);
        assert!(!accepts(&s));
    }

    #[test]
    fn refuses_an_offset_transform() {
        let mut s = occluder_spec();
        s.transform.position = (1.0, 0.0);
        assert!(!accepts(&s));
    }

    #[test]
    fn refuses_a_rotated_transform() {
        let mut s = occluder_spec();
        s.transform.rotation_deg = 0.1;
        assert!(!accepts(&s), "Pitfall 5: a rotated quad uncovers corners");
    }

    #[test]
    fn refuses_any_nonzero_crop_inset() {
        for (name, crop) in [
            ("left", LayerCrop { left: 0.001, ..Default::default() }),
            ("top", LayerCrop { top: 0.001, ..Default::default() }),
            ("right", LayerCrop { right: 0.001, ..Default::default() }),
            ("bottom", LayerCrop { bottom: 0.001, ..Default::default() }),
        ] {
            let mut s = occluder_spec();
            s.crop = crop;
            assert!(!accepts(&s), "a {name} crop inset must refuse");
        }
    }

    #[test]
    fn refuses_a_rotated_source() {
        let mut s = occluder_spec();
        s.rotation = 90;
        assert!(
            !accepts(&s),
            "at 90 the decoded frame is the transpose of src_width/src_height"
        );
    }

    #[test]
    fn refuses_an_unprobed_source() {
        let mut s = occluder_spec();
        s.reports_alpha = SourceAlpha::Unknown;
        assert!(!accepts(&s), "Unknown is not evidence of opacity");
    }

    #[test]
    fn refuses_an_alpha_carrying_source() {
        let mut s = occluder_spec();
        s.reports_alpha = SourceAlpha::Transparent;
        assert!(!accepts(&s), "Pitfall 6: VP9-alpha / transparent PNG");
    }

    #[test]
    fn refuses_an_aspect_mismatch_that_letterboxes_inside_the_canvas() {
        // 4:3 into 16:9 — the dest rect covers the canvas, the PICTURE does not
        // (Pitfall 4).
        let mut s = occluder_spec();
        s.src_width = 1440;
        s.src_height = 1080;
        assert!(!accepts(&s));
    }

    #[test]
    fn refuses_zero_or_unknown_source_dims() {
        for (w, h) in [(0u32, 1080u32), (1920, 0), (0, 0)] {
            let mut s = occluder_spec();
            s.src_width = w;
            s.src_height = h;
            assert!(!accepts(&s), "dims {w}x{h} say the mirror knows nothing");
        }
    }

    #[test]
    fn refuses_a_text_layer() {
        let mut s = occluder_spec();
        s.text = Some(rudis_core::TextPayload::new("hello"));
        assert!(!accepts(&s));
    }

    #[test]
    fn refuses_an_image_sequence() {
        let mut s = occluder_spec();
        s.is_image_sequence = true;
        assert!(!accepts(&s));
    }

    #[test]
    fn refuses_a_zero_canvas() {
        assert!(!is_full_canvas_occluder(&occluder_spec(), 0, 0));
    }

    // -------------------------------------------- the two substitution rungs

    /// A source whose PROXY reshapes it must be refused, even though the source
    /// itself fills the canvas exactly.
    ///
    /// 2704x1520 is a real GoPro mode, not a contrived pair. `proxy_dims`
    /// rounds `1520 * 960 / 2704 = 539.7` to 540 and snaps to even, so the
    /// substitute is 960x540 (16:9) against a source that is 1.77895:1 — and a
    /// PLAYING-path layer decodes from that substitute whenever a fresh proxy
    /// exists.
    #[test]
    fn refuses_a_source_whose_proxy_substitute_reshapes_it() {
        const W: u32 = 2704;
        const H: u32 = 1520;
        let mut s = occluder_spec();
        s.src_width = W;
        s.src_height = H;

        // NON-VACUITY: without the proxy rung this would be accepted — the
        // source's own dims fill this canvas exactly at every composite size.
        for (cw, ch) in composite_sizes(W, H) {
            assert!(
                fills_dest_exactly(W, H, cw as f32, ch as f32),
                "control: {W}x{H} fills {cw}x{ch} on its own"
            );
        }
        let proxy = crate::decode_source::proxy_substitute_dims(W, H)
            .expect("control: a 2704-long-edge source really is proxied");
        println!("OCCL-PROXY src={W}x{H} substitute={}x{}", proxy.0, proxy.1);
        assert!(
            !fills_dest_exactly(proxy.0, proxy.1, W as f32, H as f32),
            "control: the substitute is what letterboxes"
        );

        assert!(!accepts_on(&s, W, H), "the proxy rung must refuse this");
    }

    /// A source that fills the canvas is still refused when the DEGRADED canvas
    /// PLAY-05 composites at is not the same shape.
    #[test]
    fn refuses_when_a_dynres_degraded_canvas_changes_the_shape() {
        // 26x14 halves to 13x7 and quarters to the MIN_DIM floor 16x16, so a
        // source that fills it at Full does not fill it at Quarter.
        const W: u32 = 26;
        const H: u32 = 14;
        let mut s = occluder_spec();
        s.src_width = W;
        s.src_height = H;

        assert!(
            fills_dest_exactly(W, H, W as f32, H as f32),
            "control: it fills the FULL canvas"
        );
        let sizes = composite_sizes(W, H);
        println!("OCCL-DYNRES canvas={W}x{H} composite_sizes={sizes:?}");
        assert!(
            sizes
                .iter()
                .any(|(cw, ch)| !fills_dest_exactly(W, H, *cw as f32, *ch as f32)),
            "control: some degraded size is a different shape"
        );

        assert!(!accepts_on(&s, W, H), "the dynres rung must refuse this");
    }

    /// PUBLISHED NEGATIVE (measured, not rounded away): exact float equality on
    /// the compositor's own arithmetic refuses some sources that fill the canvas
    /// in exact real arithmetic. Refusing costs frames; accepting wrongly costs
    /// pixels, so this is the trade the module doc says it makes — but the
    /// numbers belong in the record rather than in a claim.
    #[test]
    fn measured_which_aspect_equal_sources_this_predicate_accepts() {
        // Every pair here is aspect-equal to its canvas in exact arithmetic.
        let cases: [(u32, u32, u32, u32); 8] = [
            (1920, 1080, 1920, 1080),
            (1280, 720, 1920, 1080),
            (3840, 2160, 1920, 1080),
            (640, 360, 1920, 1080),
            (1600, 900, 1920, 1080),
            (960, 540, 1920, 1080),
            (1440, 1080, 1440, 1080),
            (1000, 560, 1000, 560),
        ];
        let mut accepted = Vec::new();
        let mut refused = Vec::new();
        for (sw, sh, cw, ch) in cases {
            let mut s = occluder_spec();
            s.src_width = sw;
            s.src_height = sh;
            let verdict = accepts_on(&s, cw, ch);
            let row = format!(
                "{sw}x{sh}->{cw}x{ch} proxy={:?}",
                crate::decode_source::proxy_substitute_dims(sw, sh)
            );
            if verdict {
                accepted.push(row);
            } else {
                refused.push(row);
            }
        }
        println!("OCCL-ASPECT accepted={accepted:?}");
        println!("OCCL-ASPECT refused={refused:?}");
        // The claim the fixture and the pixel proof depend on.
        assert!(
            accepted.iter().any(|r| r.starts_with("1280x720->1920x1080")),
            "the fixture's own geometry must be accepted"
        );
        // And the sweep is non-vacuous in both directions.
        assert!(!refused.is_empty(), "no conservative miss found to publish");
    }

    // ------------------------------------------------------------------- walk

    fn spec_named(id: &str) -> LayerSpec {
        let mut s = occluder_spec();
        s.clip_id = id.into();
        // Not an occluder: a portrait source letterboxes inside a 16:9 canvas.
        s.src_width = 720;
        s.src_height = 1280;
        s
    }

    fn ids(layers: &[LayerSpec]) -> Vec<String> {
        layers.iter().map(|s| s.clip_id.clone()).collect()
    }

    #[test]
    fn an_occluder_at_the_top_kills_everything_under_it() {
        let mut layers = vec![occluder_spec(), spec_named("b"), spec_named("c")];
        cull_occluded(&mut layers, CW, CH);
        assert_eq!(ids(&layers), vec!["occ"]);
    }

    #[test]
    fn layers_above_the_occluder_are_kept() {
        let mut layers = vec![
            spec_named("pip-0"),
            spec_named("pip-1"),
            occluder_spec(),
            spec_named("hidden-a"),
            spec_named("hidden-b"),
        ];
        cull_occluded(&mut layers, CW, CH);
        assert_eq!(ids(&layers), vec!["pip-0", "pip-1", "occ"]);
    }

    #[test]
    fn a_stack_with_no_occluder_is_untouched() {
        let mut layers = vec![spec_named("a"), spec_named("b"), spec_named("c")];
        let before = ids(&layers);
        cull_occluded(&mut layers, CW, CH);
        assert_eq!(ids(&layers), before);
    }

    #[test]
    fn the_cull_never_empties_a_non_empty_stack_and_never_reorders() {
        // Every arrangement of one occluder among four layers.
        for at in 0..4usize {
            let mut layers: Vec<LayerSpec> = (0..4)
                .map(|i| {
                    if i == at {
                        occluder_spec()
                    } else {
                        spec_named(&format!("l{i}"))
                    }
                })
                .collect();
            let before = ids(&layers);
            cull_occluded(&mut layers, CW, CH);
            let after = ids(&layers);
            assert!(!after.is_empty(), "occluder at {at}: emptied the stack");
            assert_eq!(after.len(), at + 1, "occluder at {at}: wrong truncation");
            assert_eq!(after[..], before[..after.len()], "order changed at {at}");
        }
    }

    #[test]
    fn an_empty_stack_stays_empty() {
        let mut layers: Vec<LayerSpec> = Vec::new();
        cull_occluded(&mut layers, CW, CH);
        assert!(layers.is_empty());
    }
}
