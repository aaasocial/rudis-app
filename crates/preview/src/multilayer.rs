//! The multi-layer composite path (plan 46-08): what a timeline position
//! resolves to when more than the streaming fast-path is needed, and how that
//! resolved stack becomes pixels on both the paused/scrub path and the
//! play path.
//!
//! Moved out of `src-tauri/src/native_surface.rs`: [`FrameCache`], [`LayerSpec`],
//! [`MultiLayerStack`], [`resolve_multilayer`], `text_layer_from_spec`,
//! [`present_multilayer`], [`STILL_CACHE`], [`compose_multilayer_from_pool`],
//! [`start_multi_audio`] and [`present_still`]. Every body is the one that lived
//! there; the only edits are the port accessors described below.
//!
//! # What changed in the move
//!
//! | Was | Is |
//! |---|---|
//! | `app.try_state::<crate::SharedStore>()?.lock().ok()?` | `host.store()?` |
//! | `app.try_state::<Mutex<NativePreview>>().is_none()` | `!ctx.sink().has_surface()` |
//! | `np.compositor.composite_layers_to_rgba(..)` under the present lock | `ctx.sink().composite_layers(..)` |
//! | `TEXT_RASTERIZER.with(\|r\| crate::rasterize_text_layer(&mut r.borrow_mut(), ..))` | `host.rasterize_text(..)` |
//! | `crate::timeline_clip` / `crate::engine_alpha_mode` / `crate::decode_clip_frame` | the private mirrors at the bottom of this file |
//!
//! # The ONE shared text-layer builder is NOT duplicated
//!
//! `rasterize_text_layer` (today `app_core::compose::rasterize_text_layer`) is
//! deliberately the single implementation that BOTH the export loop and both
//! preview present paths call, so a text overlay composites byte-identically in
//! preview and in the exported file (the MAD-0 parity proof). It stays exactly
//! one function: this module reaches it through
//! [`crate::PreviewHost::rasterize_text`], a pass-through the shell adapter
//! implements. The adapter also supplies the per-thread `engine::TextRasterizer`
//! the shared builder takes as its first argument — which is precisely why the
//! port method does not take one, and why the `TEXT_RASTERIZER` thread-local
//! lives in the adapter rather than here.
//!
//! # Three tiny pure helpers ARE duplicated, on purpose
//!
//! `timeline_clip`, `engine_alpha_mode` and `decode_clip_frame` live in
//! `crates/app-core`, which this crate may not depend on: `app-core` is the
//! shell's application layer (it owns `AppCtx`, commands and the export
//! pipeline), and a preview engine that a future non-Rust shell embeds must sit
//! BELOW it, not beside it. All three are pure, under ten lines, and depend on
//! nothing but `rudis_core` + `engine`, so they are mirrored at the bottom of
//! this file rather than dragging the dependency arrow the wrong way.
//!
//! This is the codebase's own existing convention for exactly this boundary:
//! `app_core::compose::text_align_to_engine` mirrors `rudis_core::TextAlign` into
//! `engine::TextAlign` for the same reason ("core cannot depend on the engine").
//! The accepted cost is the same one that precedent accepts: if either copy ever
//! needs a behavior change, BOTH must change together.

use std::path::PathBuf;

use engine::{AudioOutput, Frame, Layer, LayerCrop, LayerTransform};
use rudis_core::MediaKind;

use crate::{black_frame, present_frame, resolve_active, resolve_program_audio};
use crate::{PresentContext, PreviewHost};

/// Small LRU of decoded still frames for responsive paused scrubbing: seeking
/// back over a recently-viewed position re-presents INSTANTLY instead of
/// re-spawning ffmpeg. Keyed by (path, source_us, rotation) — the exact identity
/// of a decoded frame. Frames are large (~3.6MB at 720p) so the cap is small.
///
/// Phase 46 (plan 46-08): `pub` with a `pub` constructor, because the present
/// thread that OWNS the cache (`present_loop`) still lived in the shell then and
/// hands `&mut` down into [`present_still`]. It joined this crate at 46-10
/// ([`crate::present_loop`]), so both are now on the same side of the boundary;
/// the visibility is kept rather than narrowed, since Phase 47's C ABI shell
/// will construct the loop from outside again. `get`/`put` stay module-private —
/// nothing outside this file has ever called them.
pub struct FrameCache {
    entries: Vec<((PathBuf, i64, u32), Frame)>,
    cap: usize,
}

impl FrameCache {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: Vec::new(),
            cap,
        }
    }

    /// Return a clone of the cached frame for `key` (and mark it most-recently
    /// used), or `None`.
    fn get(&mut self, key: &(PathBuf, i64, u32)) -> Option<Frame> {
        let i = self.entries.iter().position(|(k, _)| k == key)?;
        let (k, f) = self.entries.remove(i);
        let clone = f.clone();
        self.entries.push((k, f)); // move to MRU end
        Some(clone)
    }

    fn put(&mut self, key: (PathBuf, i64, u32), frame: Frame) {
        if self.entries.iter().any(|(k, _)| *k == key) {
            return;
        }
        if self.entries.len() >= self.cap {
            self.entries.remove(0); // evict LRU
        }
        self.entries.push((key, frame));
    }
}

/// Phase 18 (COMP-02, preview half): one resolved video layer at a timeline
/// position — everything the present thread needs to decode + composite it
/// WITHOUT holding the store lock.
pub struct LayerSpec {
    /// Stable pool key for the play-path `LayerDecoderPool` (a clip re-entering
    /// after a gap restarts clean). The paused/scrub `present_multilayer` path
    /// ignores it (it keys its `FrameCache` on path/source_us/rotation).
    pub clip_id: String,
    pub path: PathBuf,
    pub source_us: i64,
    pub rotation: u32,
    /// Remaining span for the pool decoder's `-t` window (`clip.out_us -
    /// source_us`); the pool floors it to one frame step.
    pub remaining_dur_us: i64,
    /// The SOURCE advance one project-fps output tick represents at this
    /// position (debug session `multitrack-preview-lag-choppy`, 2026-08-01 —
    /// export SR-3 parity): `source_offset_at(rel + step) -
    /// source_offset_at(rel)`. Un-retimed clips carry exactly the project
    /// frame step; a constant-speed clip carries `tempo * step`, which the
    /// play-path pool uses for its sequential-continuation tolerance and its
    /// `fps / tempo` decode cadence (pre-fix the pool hard-coded the output
    /// step and respawned ffmpeg EVERY tick for a retimed layer — starving it
    /// out of the live composite on real media).
    pub src_step_us: i64,
    /// `true` when this layer's clip carries a RAMP retime curve. A ramp has
    /// no constant source cadence, so — exactly like the export loop — the
    /// layer is kept OUT of the streaming pool and decoded per-frame inline
    /// at its precise retimed timestamps (`decode_clip_frame`), slow but
    /// correct by construction (WYSIWYG with the export's ramp branch).
    pub retime_ramped: bool,
    pub opacity: f32,
    pub transform: LayerTransform,
    pub crop: LayerCrop,
    /// Phase 28 (OVL-01): the clip's alpha interpretation, resolved once here
    /// (core→engine via [`engine_alpha_mode`]) and carried into BOTH present
    /// paths (paused [`present_multilayer`] + play
    /// [`compose_multilayer_from_pool`]), so the preview composite feeds the
    /// shader the SAME `alpha_mode` the export loop does — preview == export for
    /// transparent overlays by construction. Text/synthetic specs carry
    /// `Straight`.
    pub alpha_mode: engine::AlphaMode,
    /// Phase 28 (OVL-01): `true` when this layer's media is an imported numbered
    /// image sequence. Both present paths decode it via [`decode_clip_frame`]
    /// (→ `decode_frame_rgba_at_seq`) at `seq_fps`, and it is kept OUT of the
    /// play-path `LayerDecoderPool` (image2 `%0Nd` can't stream at project-fps
    /// timing) — decoded per-frame inline instead, exactly as the export/inspect
    /// composite loop resolves it (WYSIWYG). Ordinary media / text: `false`/`0.0`.
    pub is_image_sequence: bool,
    pub seq_fps: f64,
    /// UAT perf fix (preview-multilayer-not-compositing follow-up): `true` when
    /// this layer's media is a single STILL image (`MediaKind::Image`, not a
    /// numbered sequence). A still's frame NEVER changes across ticks, so it is
    /// kept OUT of the play-path `LayerDecoderPool` — streaming a one-frame PNG
    /// hits `Ended` every tick, which tore down + respawned ffmpeg 2-3x PER
    /// OUTPUT FRAME (the UAT "extremely laggy over the overlay" symptom).
    /// Instead both present paths decode it ONCE (source_us pinned to 0 — the
    /// decoder clamps a still there anyway) and serve it from a frame cache.
    pub is_still_image: bool,
    /// The media's own coded dimensions (`MediaBinItem::width`/`height`), read
    /// under the SAME store lock as everything else here.
    ///
    /// Phase 57 (plan 57-06): the per-layer hardware-session coordinator
    /// ([`crate::layer_sessions`]) sizes a session's hw frame pool from the
    /// LAYER's own dims, never the project canvas — a 4K layer on a 1080p
    /// canvas costs ~4x the VRAM the canvas would suggest, and the provisional
    /// depth handed to `open_hw_decoder` IS what widens that pool (48-05). The
    /// software pool never needed this (its children are CLI processes with no
    /// VRAM accounting), which is why the field arrives only now. Text /
    /// synthetic specs carry `(0, 0)` and are never hardware-eligible anyway.
    pub src_width: u32,
    /// See [`LayerSpec::src_width`].
    pub src_height: u32,
    /// Phase 60 (OCCL-01): whether this layer's SOURCE can carry real per-pixel
    /// transparency — the media's own `MediaBinItem::reports_alpha`, read under
    /// the SAME store lock as `src_width`/`src_height` and carried here so the
    /// occlusion predicate can ask the question at GATHER time, before any
    /// decode session is opened.
    ///
    /// That timing is the whole point: decoding a probe frame to look at its
    /// alpha would cost exactly the decode the cull exists to avoid, so the
    /// answer has to arrive as import-time metadata or not at all.
    ///
    /// **Typed as the tri-state, not as `Option<bool>`, on purpose.** The
    /// consumer's dangerous mistake is folding "never probed" into "opaque",
    /// and [`rudis_core::SourceAlpha::is_proven_opaque`] is the only way to ask
    /// — an exhaustive match that has to name `Unknown` explicitly. Text and
    /// synthetic specs carry [`rudis_core::SourceAlpha::Unknown`]: they are
    /// never occluders and must never claim knowledge they do not have.
    ///
    /// Inert until plan 60-07 wires the predicate; nothing reads it yet.
    pub reports_alpha: rudis_core::SourceAlpha,
    /// Phase 20 (TEXT-01): `Some` marks this a TEXT spec — both present paths
    /// rasterize the payload via the shared ONE text-layer builder, reached
    /// through [`crate::PreviewHost::rasterize_text`], instead of decoding media
    /// (`path`/`source_us`/`rotation` are unused dummies for it), and it is NEVER
    /// sent to the play-path decoder pool (no media to decode). `None` = ordinary
    /// media spec.
    pub text: Option<rudis_core::TextPayload>,
}

/// A multi-layer (or non-identity single-layer) stack resolved at one timeline
/// position, plus the PROJECT canvas resolution the composite targets
/// (COMP-01 — the same output shape the export encodes).
///
/// `pub` (Phase 19 made it `pub(crate)`; Phase 46 widened it when the type left
/// the shell crate): the SC-4 preview/export parity gate composites a resolved
/// stack offscreen (through the real [`resolve_multilayer`]) and frame-diffs it
/// against the decoded export, and the ring producer holds one across ticks.
pub struct MultiLayerStack {
    /// TRACK-ORDERED, index-0 = top — the compositor's 18-02 contract
    /// (`Timeline::active_layers_at` hands track order through unreversed;
    /// the compositor paints index-0 LAST).
    pub layers: Vec<LayerSpec>,
    pub width: u32,
    pub height: u32,
    /// PROJECT timebase — the cadence the play-path `LayerDecoderPool` is paced
    /// off (self-paced preview, not the frontend `position_us` poll) and the
    /// value each per-layer decoder resamples to. Read under the SAME store lock
    /// as width/height so it can never drift from the resolved layers.
    pub fps: f64,
}

/// Resolve whether `position_us` (Program mode) sits inside a multi-layer /
/// non-identity range, and if so return everything needed to composite it.
///
/// Returns `None` for the single-layer DEGENERATE case — at most one active
/// video layer whose clip has identity transform, opacity 1.0 and no crop —
/// so the caller keeps the EXISTING full-rate streaming present path
/// byte-for-byte unchanged (the preview twin of the export's
/// `export_is_single_layer_degenerate` detection, D-02). A timeline gap also
/// returns `None` (the existing black-frame path owns gaps).
///
/// Phase 46 (plan 46-08): reads the project through [`crate::PreviewHost::store`]
/// instead of the shell's managed-state lookup. `None` from the port covers the
/// unmanaged AND poisoned cases the `try_state` + `lock().ok()` pair folded
/// together, so the early-return behavior is unchanged.
pub fn resolve_multilayer(host: &dyn PreviewHost, position_us: i64) -> Option<MultiLayerStack> {
    let guard = host.store()?;
    let hits = guard.timeline().active_layers_at(position_us);
    if hits.is_empty() {
        return None; // gap: the streaming path's black frame owns this
    }
    // Phase 28 (OVL-01): a numbered image sequence must ALWAYS composite — the
    // single-layer streaming decode path can't decode a `%0Nd` image2 pattern at
    // project-fps timing. Routing it through this multi-layer path decodes each
    // frame via `decode_clip_frame` (→ `decode_frame_rgba_at_seq`), the SAME
    // resolution the export/inspect loop uses (WYSIWYG).
    let single_is_sequence = hits.len() == 1
        && guard
            .media_item(&hits[0].media_id)
            .map(|m| m.is_image_sequence)
            .unwrap_or(false);
    // Phase 28 (OVL-01) fix: a STILL image (MediaKind::Image) likewise cannot
    // take the single-layer streaming fast-path — the streaming decoder can't
    // stream a single-frame PNG and `resolve_active` skips non-video, so a lone
    // still image would present BLACK. Route it through this multi-layer
    // composite instead (present_multilayer / the play-path pool both decode a
    // still via the shared ExportRunDecoder/decode_clip_frame → WYSIWYG with the
    // export, which composites stills the same way).
    let single_is_still_image = hits.len() == 1
        && guard
            .media_item(&hits[0].media_id)
            .map(|m| m.media_kind == MediaKind::Image)
            .unwrap_or(false);
    if hits.len() == 1 && !single_is_sequence && !single_is_still_image {
        // Single layer: composite ONLY if its clip carries non-identity
        // visuals (the fast streaming decode can't render transform/opacity/
        // crop); otherwise the Phase-9 streaming path is byte-identical.
        match timeline_clip(guard.timeline(), &hits[0].clip_id) {
            Some(clip)
                if clip.transform == rudis_core::ClipTransform::default()
                    && clip.opacity == 1.0
                    && clip.crop == rudis_core::ClipCrop::default()
                    // Phase 19 (COMP-04, T-19-04): an ANIMATED clip must
                    // composite even when its STATIC visuals are identity —
                    // otherwise `sample_at` never runs and the preview shows
                    // the un-animated frame (the SC-4 fast-path trap). Only a
                    // clip that is BOTH static-identity AND has no keyframe
                    // track takes the streaming fast-path.
                    && clip.keyframes.is_empty()
                    // Phase 20 (TEXT-01): a TEXT clip must ALWAYS composite —
                    // the streaming fast-path decodes media and cannot
                    // rasterize (and its media_id is the empty sentinel). Never
                    // degenerate, mirroring the export degenerate guard.
                    && clip.text.is_none() =>
            {
                return None; // degenerate: streaming path unchanged
            }
            // Unresolvable clip id (unreachable — the hit came from this
            // timeline): the streaming path is the safe answer.
            None => return None,
            Some(_) => {} // non-identity visuals: composite below
        }
    }
    // Composite target = the PROJECT canvas (COMP-01); `fps` is the PROJECT
    // timebase the keyframe sampler needs (O-1) — read under the SAME lock as
    // width/height/the layer resolution so the sampler and the export path
    // (which samples with the identical project fps) can never drift (SC-4).
    // Phase 57 (plan 57-06, PLAY-07/D-08): this was `guard.snapshot()` — a deep
    // clone of the WHOLE `Project`, per produced frame, to read three scalars.
    // `Store::project_canvas` is the borrow-only accessor for exactly them, in
    // the same narrow grain `timeline()`/`media_item()` already established.
    // `Store::PROJECT_CLONE_COUNT` is the counter that proves this stayed gone.
    let (width, height, project_fps) = guard.project_canvas();
    // One output tick of the PROJECT timebase — the step the per-layer
    // source-advance (`src_step_us`) is measured against (SR-3 parity).
    let step_us = engine::frame_step_us(project_fps).max(1);
    let mut layers: Vec<LayerSpec> = Vec::with_capacity(hits.len());
    for hit in &hits {
        let Some(clip) = timeline_clip(guard.timeline(), &hit.clip_id) else {
            continue; // unreachable; a missing clip contributes nothing
        };
        // Phase 19 (COMP-04): resolve this clip's CONCRETE visual properties
        // at `position_us` through the ONE shared sampler, passing the PROJECT
        // fps (O-1) — the SAME value the export layer-build loop passes, so a
        // keyframed preview frame equals the exported frame at the identical
        // timestamp by construction (SC-4). `sampled.volume` is unused here
        // (audio is Plan 04).
        let sampled = clip.sample_at(position_us - clip.start_us, project_fps);
        // Field-for-field map — core's ClipTransform/ClipCrop and engine's
        // LayerTransform/LayerCrop share shape AND semantics by construction
        // (Plan 18-03); identical to the export branch's map. Built once so the
        // text and media branches carry the exact same resolved transform.
        let transform = LayerTransform {
            position: sampled.transform.position,
            scale: sampled.transform.scale,
            rotation_deg: sampled.transform.rotation_deg,
        };
        let crop = LayerCrop {
            left: sampled.crop.left,
            top: sampled.crop.top,
            right: sampled.crop.right,
            bottom: sampled.crop.bottom,
        };

        // Phase 20 (TEXT-01): a TEXT clip carries no media item (its media_id
        // is the empty sentinel), so it MUST be handled BEFORE the media lookup
        // — otherwise `media_item("")` returns None and the text would be
        // skipped. Its `path`/`source_us`/`rotation` are unused dummies; both
        // present paths rasterize `text` via the shared helper.
        if let Some(payload) = &clip.text {
            layers.push(LayerSpec {
                clip_id: hit.clip_id.clone(),
                path: PathBuf::new(),
                source_us: 0,
                rotation: 0,
                remaining_dur_us: clip.out_us - hit.source_us,
                src_step_us: step_us, // unused: text is never pooled
                retime_ramped: false,
                opacity: sampled.opacity,
                transform,
                crop,
                // Text is straight-alpha by convention (Phase 20: the shader
                // premultiplies the rasterized glyph once — never premultiply
                // here), so a text spec is always Straight regardless of the
                // clip's stored alpha_mode.
                alpha_mode: engine::AlphaMode::Straight,
                is_image_sequence: false,
                seq_fps: 0.0,
                is_still_image: false,
                // Text rasterizes at canvas size; it is never hardware-decoded,
                // so it carries no source dims.
                src_width: 0,
                src_height: 0,
                // Phase 60 (OCCL-01): a text layer has no probed SOURCE, and a
                // rasterized glyph run is mostly transparent anyway. Unknown —
                // never an occluder, and never claiming otherwise.
                reports_alpha: rudis_core::SourceAlpha::Unknown,
                text: Some(payload.clone()),
            });
            continue;
        }

        let Some(item) = guard.media_item(&hit.media_id) else {
            continue; // missing media: black shows through (export twin)
        };
        // Phase 28 (OVL-01) fix: a STILL image (MediaKind::Image) is legitimate
        // transparent-overlay visual content on a video track (import a PNG, place
        // it over footage) — INCLUDE it in the composite. Both present paths decode
        // a still via the shared ExportRunDecoder/decode_clip_frame, exactly as the
        // export loop does (line ~4543), so preview == export. Only AUDIO on a video
        // track contributes no visual layer. (Pre-fix this dropped every still-image
        // overlay from the LIVE preview while the export composited it correctly —
        // a preview≠export bug the export-based SC tests, which used VIDEO overlays,
        // never caught.)
        if item.media_kind == MediaKind::Audio {
            continue; // audio on a video track: no visual layer
        }
        // A STILL image's frame is position-invariant: pin `source_us` to 0 (the
        // decoder clamps a still there regardless) so the paused FrameCache key
        // (path, source_us, rotation) is STABLE across scrub positions — one
        // decode total instead of one per scrub step — and the play path can
        // serve every tick from its still cache (see `compose_multilayer_from_pool`).
        let is_still_image = item.media_kind == MediaKind::Image;
        // Export SR-3 parity (debug session `multitrack-preview-lag-choppy`):
        // the SOURCE advance one output tick represents at this position —
        // exact for constant retime (linear) and for the un-retimed identity
        // (== step_us, keeping every pre-retime pool behavior identical).
        let rel = position_us - clip.start_us;
        let src_step_us =
            (clip.source_offset_at(rel + step_us) - clip.source_offset_at(rel)).max(1);
        // A RAMP has no constant cadence — no single decode fps can express
        // it, so (exactly like the export loop's ramp branch) the layer skips
        // the streaming pool and decodes per-frame inline in
        // `compose_multilayer_from_pool`.
        let retime_ramped = matches!(
            clip.retime.as_ref().map(|r| &r.curve),
            Some(rudis_core::RetimeCurve::Ramp(_))
        );
        layers.push(LayerSpec {
            clip_id: hit.clip_id.clone(),
            path: PathBuf::from(&item.path),
            source_us: if is_still_image { 0 } else { hit.source_us },
            rotation: item.rotation_degrees,
            // Pool decoder `-t` window; the pool floors it to one frame step.
            remaining_dur_us: clip.out_us - hit.source_us,
            src_step_us,
            retime_ramped,
            opacity: sampled.opacity,
            transform,
            crop,
            // Phase 28 (OVL-01): carry the real per-clip alpha interpretation
            // (core→engine) into the preview composite so a transparent overlay
            // previews exactly as it exports.
            alpha_mode: engine_alpha_mode(clip.alpha_mode),
            is_image_sequence: item.is_image_sequence,
            seq_fps: item.fps,
            is_still_image,
            // Phase 57 (57-06): the layer's OWN media dims, for the hw
            // session's pool sizing / VRAM reservation.
            src_width: item.width,
            src_height: item.height,
            // Phase 60 (OCCL-01): the mirror item's own import-time verdict,
            // read under THIS lock alongside the dims above — no second lookup,
            // no probe, no decode. `item.source_alpha()` is the only accessor,
            // so an unprobed source arrives as `Unknown` rather than as a bare
            // `false` some later reader could mistake for evidence.
            reports_alpha: item.source_alpha(),
            text: None,
        });
    }
    // Phase 60 (OCCL-01, plan 60-07): THE cull. Everything hidden behind a
    // provably canvas-covering opaque layer is dropped here, in the gather,
    // before anything downstream can decide to decode it.
    //
    // (a) TRUNCATION ONLY. `cull_occluded` never empties a non-empty vec and
    //     never reorders, and this call deliberately does NOT re-run the
    //     single-layer degenerate check above on the culled result. A stack
    //     culled down to one layer stays a MULTI-layer stack: redirecting it
    //     into the streaming fast path would change which producer arm runs for
    //     a frame, which is a correctness risk (and a different composite) taken
    //     for no part of OCCL-01's claim.
    // (b) THIS PLACEMENT is what makes every consumer position-correct. The live
    //     tick (`ring.rs:2488`), the boundary prewarm (`ring.rs:1176`) and the
    //     exit discovery (`ring.rs:1321`, `:3123`) all resolve stacks at FUTURE
    //     positions, and the paused/scrub present path resolves at the playhead;
    //     each gets the cull computed against ITS OWN position. Culling at the
    //     live-tick call site instead would leave the prewarm re-opening decode
    //     sessions for layers the tick had just culled — silently undoing
    //     OCCL-02's payoff.
    // (c) By construction it runs UPSTREAM of `CacheServe::try_serve`
    //     (`ring.rs:2537`), `sessions.sync_to_stack` (`:2783`) and `prearm_stack`
    //     (`:3056`), so `select_hw_clip_ids`' `eligible` set never contains a
    //     hidden layer and cannot hold a hardware slot open for one.
    crate::occlusion::cull_occluded(&mut layers, width, height);
    Some(MultiLayerStack {
        layers,
        width,
        height,
        fps: project_fps,
    })
}

/// Rasterize a TEXT [`LayerSpec`] into a composited [`Layer`] via the ONE shared
/// text-layer builder, feeding it the spec's ALREADY-resolved (Phase-19
/// `sample_at`) transform/opacity/crop — byte-identical to the inputs the export
/// loop feeds the same builder, so the resulting `Layer` (and its composite) is
/// identical in preview and export.
///
/// Phase 46 (plan 46-08): the builder is reached through
/// [`crate::PreviewHost::rasterize_text`]. The port takes no `TextRasterizer`
/// because supplying one — from a per-thread cache, the hot-path invariant — is
/// the shell adapter's job; the builder itself is emphatically NOT duplicated
/// here (see this module's header).
fn text_layer_from_spec(
    host: &dyn PreviewHost,
    text: &rudis_core::TextPayload,
    spec: &LayerSpec,
    stack: &MultiLayerStack,
) -> Layer {
    host.rasterize_text(
        text,
        spec.transform,
        spec.opacity,
        spec.crop,
        stack.width,
        stack.height,
    )
}

/// The MAXIMUM number of paused-path miss-decodes in flight at once — the
/// bound on quick task `260803-dvp`'s fix for `deferred-items.md § D-6`.
///
/// # Why 3, specifically
///
/// * It is [`crate::layer_sessions::MAX_HW_SESSIONS`]
///   (`layer_sessions.rs:101`) — the per-layer concurrency this codebase
///   already commits to, so the paused path does not invent a second,
///   independent guess at "how many layers at once".
/// * It is the concurrency this machine is MEASURED to sustain, not a hope:
///   `scrub_probe.rs`'s PLAYING series re-lands the same three layers of the
///   BENCH-02 F1 fixture concurrently at p50 260.4 ms while the serial paused
///   series pays 824.1 ms for the identical three.
/// * The resource each worker holds is CHEAP relative to the hardware sessions
///   the cap is borrowed from: one `ffprobe` + `ffmpeg` CLI child pair piping
///   rawvideo into system memory, with no VRAM-ledger term at all (the ledger
///   exists for the hw decoder pools, which this path never opens).
/// * The decoded frames add NO new peak memory. All N frames already coexisted
///   in the serial path's `Vec<Layer>` before the composite ran; concurrency
///   overlaps only the CHILDREN's own working sets.
///
/// With more misses than workers the jobs drain off a shared index in
/// `ceil(N / PAUSED_DECODE_WORKERS)` waves — latency degrades linearly and
/// never more than three children exist at once. Thread-per-layer is
/// deliberately not on offer: a 12-layer stack must not translate a single
/// timeline drag into 24 processes.
pub const PAUSED_DECODE_WORKERS: usize = 3;

/// How many decode workers a batch of `jobs` cache misses gets: never more than
/// [`PAUSED_DECODE_WORKERS`], and never more than there is work to do (`0` jobs
/// spawns nothing at all).
pub fn paused_decode_worker_count(jobs: usize) -> usize {
    jobs.min(PAUSED_DECODE_WORKERS)
}

/// One MISSED per-layer decode, as OWNED plain data — everything a worker needs
/// and nothing it must not touch.
///
/// This is what makes the fan-out safe rather than clever: `resolve_multilayer`
/// already RELEASED the store guard before returning its owned
/// [`MultiLayerStack`] (see [`LayerSpec`]'s doc — "everything the present thread
/// needs to decode + composite it WITHOUT holding the store lock"), and
/// [`decode_clip_frame`] is a free function over plain data that spawns its own
/// CLI children. So a worker holds no lock, no `&mut` into the [`FrameCache`]
/// (which stays present-thread-owned: hits gather BEFORE the fan-out, results
/// fold back AFTER the join) and no handle to anything the present thread is
/// also using.
struct MissJob {
    /// The [`FrameCache`] key `(path, source_us, rotation)` — which is also
    /// exactly the three decode arguments.
    key: (PathBuf, i64, u32),
    is_image_sequence: bool,
    seq_fps: f64,
}

impl MissJob {
    /// Run the decode. The error is STRINGIFIED here, in the worker: the
    /// degradation contract needs only the message text for its `eprintln!`,
    /// and a `String` is unconditionally `Send` regardless of what
    /// `engine::EngineError` gains later.
    fn decode(&self) -> Result<Frame, String> {
        decode_clip_frame(
            &self.key.0,
            self.key.1,
            self.key.2,
            self.is_image_sequence,
            self.seq_fps,
        )
        .map_err(|e| e.to_string())
    }
}

/// Decode + composite a resolved multi-layer stack at PROJECT resolution
/// through the SAME `composite_layers_to_rgba` the export branch calls (D-02
/// one-composite-path), then present the composed frame through the UNCHANGED
/// single-frame surface present ([`present_frame`] → contain-fit letterbox +
/// overlay ink). A multi-layer preview frame is therefore the export's
/// composite, letterboxed — identical geometry in preview and export by
/// construction, never a second compositing opinion at the surface's own
/// aspect. Per-layer media decodes go through the paused-scrub [`FrameCache`];
/// text specs rasterize via the shared builder (no decode), so a stalled
/// playhead / repeated paused repaint re-presents instantly.
///
/// Phase 46 (plan 46-08): the offscreen composite is
/// [`crate::PresentSink::composite_layers`] now. The shell held the present lock
/// for the composite, RELEASED it, then re-took it inside `present_frame`; the
/// port keeps exactly that shape (the sink takes and releases the lock inside
/// `composite_layers`, and `present_frame` re-takes it), so no lock is held
/// across a call that could re-enter the surface. All three of the original's
/// early returns — no surface managed, poisoned lock, composite error — are the
/// port's single `None`.
/// Phase 49 (plan 49-01, OQ4): returns whether a frame actually reached the
/// sink (the composite-and-present succeeded, or the empty-stack black frame
/// presented), so [`present_still`] can propagate landing success to the
/// present loop's stamp reporting. All pre-49 callers ignore the value.
///
/// # Quick task `260803-dvp` (D-6 fix shape 1): the misses decode CONCURRENTLY
///
/// The per-spec loop used to decode each cache miss where it found it, one
/// after another, on the present thread. `deferred-items.md § D-6` measured
/// that on the BENCH-02 F1 fixture — `per_layer_decode_ms=[262.2, 263.0,
/// 256.3]`, `decode_total_ms=781.5` against an end-to-end p50 of 824.1 ms, so
/// ~95 % of a paused 3-layer scrub was three FRESH `ffprobe` + `ffmpeg`
/// accurate seeks taken in series.
///
/// It is now four passes, and the split is the whole point:
///
/// 1. **Gather** (present thread, TRACK order) — text specs are marked for pass
///    4; media specs try the [`FrameCache`]; the misses become [`MissJob`]s of
///    owned plain data, DEDUPED by decode key in first-occurrence order (the
///    serial code decoded a repeated key once, because its own `cache.put` made
///    the second occurrence a hit — so this must too).
/// 2. **Fan out** — at most [`PAUSED_DECODE_WORKERS`] workers pull jobs off a
///    shared index inside a `std::thread::scope`. One job decodes inline (no
///    thread tax on the common single-miss case); zero jobs spawn nothing.
/// 3. **Fold back** (present thread, the jobs' TRACK-ordered build order) —
///    `Ok` results go into the cache and a local key→frame map, `Err` results
///    get the same `eprintln!` the serial arm emitted and are not cached (the
///    serial path never cached a failure either).
/// 4. **Assemble** — `Vec<Layer>` is built by walking `stack.layers` in TRACK
///    order exactly as before, so index-0 is still the top layer the compositor
///    paints LAST (the 18-02 contract). A layer with no frame is skipped and
///    black shows through, unchanged.
///
/// **What did NOT change:** which pixels composite, their z-order, the alpha
/// handling, the D-27 originals-not-proxy decision, the degradation on an
/// undecodable layer, and the fact that this function is SYNCHRONOUS — every
/// worker is joined before it returns, so "one decode batch per present tick"
/// holds by construction and `present_loop`'s `paused_represent_needed` still
/// coalesces a fast drag to its newest position.
pub fn present_multilayer(
    ctx: &PresentContext,
    stack: &MultiLayerStack,
    cache: &mut FrameCache,
) -> bool {
    // ---- Pass 1: GATHER, on the present thread, in TRACK order ----
    //
    // The cache is `&mut`-owned here and never crosses into a worker: every
    // `get` happens before the fan-out and every `put` after the join.
    let mut hits: Vec<Option<Frame>> = Vec::new();
    hits.resize_with(stack.layers.len(), || None);
    let mut jobs: Vec<MissJob> = Vec::new();
    for (i, spec) in stack.layers.iter().enumerate() {
        // Phase 20 (TEXT-01): a TEXT spec rasterizes its payload via the ONE
        // shared text-layer builder (the SAME the export loop and the play-path
        // pool-compose call) — never a media decode. Preview == export by
        // construction.
        //
        // It is rasterized INLINE in pass 4 and never handed to a worker:
        // `host.rasterize_text` rides the shell adapter's THREAD-LOCAL
        // `TEXT_RASTERIZER`, so calling it off the present thread would build a
        // fresh rasterizer per worker per tick. It costs no decode anyway.
        if spec.text.is_some() {
            continue;
        }
        let key = (spec.path.clone(), spec.source_us, spec.rotation);
        if let Some(f) = cache.get(&key) {
            hits[i] = Some(f);
            continue;
        }
        // Phase 28 (OVL-01): decode via the ONE shared helper — an image
        // sequence resolves through `decode_frame_rgba_at_seq` at its project
        // fps, the SAME frame the export/inspect loop produces (WYSIWYG).
        //
        // Phase 58 (D-27), a DECISION and not an omission: the paused/scrub
        // still path deliberately keeps decoding ORIGINALS. `spec.path` is
        // passed here on purpose and this site is NOT wired to the resolver
        // seam. Pause is exactly when a user inspects a composite, so a
        // still gets full quality — the same posture as PLAY-05's pause
        // snap-back to the Full resolution level (57-08). D-27 names four
        // PLAYING-path decode arms (the multi-layer hardware coordinator,
        // the single-clip CPU arm, the single-clip GPU-delegated arm and the
        // software-fallback pool) and this is none of them. It also keeps
        // every paused-frame pixel pin in the workspace byte-valid with a
        // warmed proxy cache present.
        if jobs.iter().any(|j| j.key == key) {
            continue; // same decode key as an earlier layer: decode it ONCE
        }
        jobs.push(MissJob {
            key,
            is_image_sequence: spec.is_image_sequence,
            seq_fps: spec.seq_fps,
        });
    }

    // ---- Pass 2: FAN OUT the misses, bounded ----
    let mut results: Vec<Option<Result<Frame, String>>> = Vec::new();
    results.resize_with(jobs.len(), || None);
    if jobs.len() == 1 {
        // The common case (one layer changed, or a single-layer non-degenerate
        // stack): spawning a thread to wait on it is pure overhead.
        results[0] = Some(jobs[0].decode());
    } else if jobs.len() > 1 {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let (jobs_ref, next_ref) = (&jobs, &next);
        let mut collected: Vec<(usize, Result<Frame, String>)> = Vec::new();
        std::thread::scope(|scope| {
            let worker_count = paused_decode_worker_count(jobs_ref.len());
            let mut handles = Vec::with_capacity(worker_count);
            for w in 0..worker_count {
                let spawned = std::thread::Builder::new()
                    .name(format!("preview-paused-decode-{w}"))
                    .spawn_scoped(scope, move || {
                        let mut out: Vec<(usize, Result<Frame, String>)> = Vec::new();
                        // Wave drain off ONE shared index: with more misses
                        // than workers this is ceil(N / workers) waves, never
                        // one thread per layer.
                        loop {
                            let idx = next_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(job) = jobs_ref.get(idx) else {
                                break;
                            };
                            out.push((idx, job.decode()));
                        }
                        out
                    });
                match spawned {
                    Ok(h) => handles.push(h),
                    Err(e) => {
                        // OS refused a thread. The workers already running still
                        // drain the whole queue off the shared index, so this is
                        // a slowdown, not a loss.
                        eprintln!("preview: multi-layer decode worker spawn failed: {e}");
                        break;
                    }
                }
            }
            if handles.is_empty() {
                // Not one worker started: fall back to exactly the old serial
                // behaviour rather than dropping every layer.
                for (idx, job) in jobs_ref.iter().enumerate() {
                    collected.push((idx, job.decode()));
                }
                return;
            }
            // EXPLICIT join of EVERY handle, and the `Err` is CONSUMED.
            // `std::thread::scope` re-panics the present thread at end-of-block
            // for any handle dropped un-joined, so a panicking worker would
            // otherwise take the whole preview down. Consumed here, it degrades
            // exactly as a decode error does: the jobs that worker had claimed
            // report no result, and pass 3 skips those layers.
            for h in handles {
                match h.join() {
                    Ok(out) => collected.extend(out),
                    Err(_) => eprintln!(
                        "preview: multi-layer decode failed: a decode worker panicked \
                         (its layer(s) are skipped, exactly as a decode error would be)"
                    ),
                }
            }
        });
        for (idx, r) in collected {
            results[idx] = Some(r);
        }
    }

    // ---- Pass 3: FOLD BACK, on the present thread, in TRACK order ----
    //
    // `jobs` was built by walking `stack.layers` in track order, so draining it
    // in index order is a track-ordered fold — results are addressed by JOB
    // INDEX, never by which worker finished first.
    let mut decoded: Vec<((PathBuf, i64, u32), Frame)> = Vec::with_capacity(jobs.len());
    for (idx, job) in jobs.into_iter().enumerate() {
        match results[idx].take() {
            Some(Ok(f)) => {
                cache.put(job.key.clone(), f.clone());
                decoded.push((job.key, f));
            }
            Some(Err(e)) => {
                // An undecodable layer contributes nothing — black shows
                // through, the preview twin of the export branch's skip. Not
                // cached: the serial path never cached a failure.
                eprintln!("preview: multi-layer decode failed: {e}");
            }
            None => {
                eprintln!(
                    "preview: multi-layer decode failed: no result for {} @ {}us \
                     (the worker that claimed it panicked)",
                    job.key.0.display(),
                    job.key.1
                );
            }
        }
    }

    // ---- Pass 4: ASSEMBLE, in TRACK order (index-0 = top, painted LAST) ----
    let mut layers: Vec<Layer> = Vec::with_capacity(stack.layers.len());
    for (i, spec) in stack.layers.iter().enumerate() {
        if let Some(text) = &spec.text {
            layers.push(text_layer_from_spec(ctx.host(), text, spec, stack));
            continue;
        }
        let frame = match hits[i].take() {
            Some(f) => f,
            // Served from the FOLD's own map, never by re-`get`ting the cache:
            // a batch larger than the cache cap could evict a frame that was
            // just `put`, which would silently drop a layer the serial path
            // would have shown.
            None => {
                let key = (spec.path.clone(), spec.source_us, spec.rotation);
                match decoded.iter().find(|(k, _)| *k == key) {
                    Some((_, f)) => f.clone(),
                    None => continue, // decode failed for this key: layer skipped
                }
            }
        };
        layers.push(Layer {
            frame,
            opacity: spec.opacity,
            transform: spec.transform,
            crop: spec.crop,
            // Phase 28 (OVL-01): the spec carries the clip's resolved alpha
            // interpretation — the SAME value the export loop feeds the shader
            // (preview == export for transparent overlays).
            alpha_mode: spec.alpha_mode,
        });
    }
    if layers.is_empty() {
        return present_frame(ctx, black_frame());
    }
    // Composite in PROJECT space under the GPU lock (offscreen render +
    // readback), then release it before `present_frame` re-locks to present.
    let Some(rgba) = ctx
        .sink()
        .composite_layers(&layers, stack.width, stack.height)
    else {
        // no surface / poisoned lock / composite failed: present nothing
        return false;
    };
    present_frame(
        ctx,
        Frame {
            width: stack.width,
            height: stack.height,
            rgba,
        },
    )
}

thread_local! {
    /// UAT perf fix: per-thread cache of decoded STILL-image frames, keyed
    /// (path, 0, rotation) — the same key shape as the paused path's
    /// [`FrameCache`]. A still is decoded at most once per present/producer
    /// thread and served from here on every later tick (the pool's one-frame
    /// stream respawned ffmpeg per tick — the UAT choppiness). Small LRU: a
    /// timeline rarely has more than a handful of distinct stills active.
    static STILL_CACHE: std::cell::RefCell<FrameCache> =
        std::cell::RefCell::new(FrameCache::new(8));
}

/// PLAY-path frame sourcing (Phase 18.2, PERF-01): source each layer's frame
/// from the persistent [`engine::LayerDecoderPool`] and attach the resolved
/// (Phase-19 `sample_at`) opacity/transform/crop. The ONLY thing that changes
/// versus the export branch is where `frame` comes from — the returned
/// `Vec<Layer>` is identical in SHAPE to the export branch's and is composited
/// by the UNCHANGED `composite_layers_to_rgba` (SC-3, one-composite-path). No
/// surface / `Store` touched, so this is unit-testable (Plan 03's SC-3 parity
/// gate drives this REAL glue, never a reconstruction).
///
/// One `advance()` per output tick sources every active layer with bounded
/// spawns (`<= active.len()`); a layer the pool produced no frame for this tick
/// is omitted (black shows through, mirroring the export skip). Frames are
/// re-zipped by `clip_id` back into `stack.layers` TRACK order (index-0 = top,
/// the compositor's 18-02 contract).
///
/// STILL images (UAT perf fix) are never pooled: their position-invariant frame
/// is decoded at most once per thread into [`STILL_CACHE`] and served from it
/// every tick — the pool's one-frame stream hit `Ended` each tick and respawned
/// ffmpeg 2-3x per output frame, the UAT "laggy over the overlay" cause.
///
/// Phase 46 (plan 46-08): gains `host`, used for ONE thing — the text branch's
/// [`crate::PreviewHost::rasterize_text`]. Everything else here is pure.
/// The pool-eligible frame sources of a resolved stack — every MEDIA layer
/// that streams through the play-path `LayerDecoderPool` (text is rasterized,
/// image sequences decode inline, stills serve from `STILL_CACHE` — none of
/// those ever reach the pool). Extracted (debug session
/// `multitrack-preview-lag-choppy`, 2026-08-01) so the producer's boundary
/// prewarm/adopt handshake and [`compose_multilayer_from_pool`] can never
/// drift on WHICH layers are pooled.
pub(crate) fn pool_sources(stack: &MultiLayerStack) -> Vec<engine::LayerFrameSource> {
    stack
        .layers
        .iter()
        // RAMP-retimed layers are likewise never pooled (no constant source
        // cadence a streaming session could honor — the export loop's exact
        // ramp discipline): they decode per-frame inline in
        // `compose_multilayer_from_pool`.
        .filter(|spec| {
            spec.text.is_none()
                && !spec.is_image_sequence
                && !spec.is_still_image
                && !spec.retime_ramped
        })
        .map(|spec| {
            // D-05: the source ALWAYS comes through the resolver seam, never
            // `spec.path` directly — Phase 58 (D-27) substitutes a playback
            // proxy here. This is the multi-layer SOFTWARE-FALLBACK pool, and it
            // matters twice over: it is the arm `GPU-05` routes a layer to
            // whenever hardware decode is unavailable, unsupported, or already
            // holding `MAX_HW_SESSIONS`, and it is the arm D-28's PROXY-06
            // measurement is scored on (the hardware path was already at
            // RTF 1.000 after Phase 57, so the remaining ceiling is here).
            let src = crate::decode_source::resolve_decode_source(
                &spec.clip_id,
                &spec.path,
                spec.source_us,
            );
            engine::LayerFrameSource {
                clip_id: spec.clip_id.clone(),
                path: src.path,
                source_us: src.source_us,
                // Unchanged by D-04: a proxy keeps its source's fps, timebase
                // and duration, so the remaining duration and the source step
                // are the same numbers for either media.
                rotation: spec.rotation,
                remaining_dur_us: spec.remaining_dur_us,
                src_step_us: spec.src_step_us,
            }
        })
        .collect()
}

pub fn compose_multilayer_from_pool(
    host: &dyn PreviewHost,
    pool: &mut engine::LayerDecoderPool,
    stack: &MultiLayerStack,
) -> Vec<Layer> {
    // Phase 20 (TEXT-01): TEXT specs have NO media path and must NEVER be sent
    // to `pool.advance` (it would try to decode the empty-sentinel path). Only
    // media specs source from the pool; text specs are rasterized inline below
    // and re-zipped back into `stack.layers` TRACK order.
    // Phase 28 (OVL-01): image-sequence specs, like text, are NEVER sent to the
    // pool (image2 `%0Nd` can't stream at project-fps timing) — they are decoded
    // per-frame inline below via the shared `decode_clip_frame` helper.
    // UAT perf fix: STILL images are likewise never pooled — a one-frame PNG
    // stream hits `Ended` every tick, so the pool tore down + respawned ffmpeg
    // 2-3x per output frame (the "extremely laggy over the overlay" UAT
    // symptom). A still's frame never changes: decode ONCE into the thread-local
    // STILL_CACHE below and serve every subsequent tick from it (zero spawns).
    let sources: Vec<engine::LayerFrameSource> = pool_sources(stack);
    // `advance` returns owned `(clip_id, Frame)` pairs; index them so we can
    // re-zip in the authoritative `stack.layers` order (each clip_id appears at
    // most once per stack).
    let mut by_id: std::collections::HashMap<String, Frame> = pool.advance(&sources).into_iter().collect();
    let mut layers: Vec<Layer> = Vec::with_capacity(stack.layers.len());
    for spec in &stack.layers {
        if let Some(layer) = cpu_layer_for_spec(host, &mut by_id, spec, stack) {
            layers.push(layer);
        }
    }
    layers
}

/// Resolve ONE `LayerSpec` to a CPU [`Layer`], or `None` when this spec
/// contributes nothing this tick (black shows through — the play-path twin of
/// the export skip).
///
/// Phase 57 (plan 57-06): extracted VERBATIM from
/// [`compose_multilayer_from_pool`]'s loop body so the producer's mixed
/// GPU/CPU gather reuses the identical text / image-sequence / RAMP-retime /
/// still-image / pool-frame branches instead of re-deriving them. That reuse is
/// the point: a second copy of this decision tree is exactly how a hardware
/// coordinator would drift from the software path it is supposed to match
/// pixel-for-pixel.
///
/// `by_id` is the `clip_id → Frame` map `LayerDecoderPool::advance` returned
/// for THIS tick; the pooled branch REMOVES from it, so a clip is served at
/// most once per tick (each `clip_id` appears at most once per stack anyway).
///
/// `compose_multilayer_from_pool` survives above as the public 3-arg wrapper
/// that loops this — `crates/app-core`'s dev-dep parity twins name it directly
/// (`export.rs:3594`, `:4071`) to prove the preview half of the
/// one-composite-path invariant, and D-13 forbids editing app-core.
pub(crate) fn cpu_layer_for_spec(
    host: &dyn PreviewHost,
    by_id: &mut std::collections::HashMap<String, Frame>,
    spec: &LayerSpec,
    stack: &MultiLayerStack,
) -> Option<Layer> {
    {
        if let Some(text) = &spec.text {
            // Rasterize inline via the SAME shared builder the paused path and
            // the export loop use — mixed into the track-ordered stack (index-0
            // = top) alongside the pool-sourced media frames.
            return Some(text_layer_from_spec(host, text, spec, stack));
        }
        if spec.is_image_sequence {
            // Decode the sequence frame per-tick via the ONE shared helper (the
            // SAME `decode_frame_rgba_at_seq` path the export/inspect/paused
            // composite uses) — WYSIWYG across preview and export.
            match decode_clip_frame(
                &spec.path,
                spec.source_us,
                spec.rotation,
                true,
                spec.seq_fps,
            ) {
                Ok(frame) => {
                    return Some(Layer {
                        frame,
                        opacity: spec.opacity,
                        transform: spec.transform,
                        crop: spec.crop,
                        alpha_mode: spec.alpha_mode,
                    })
                }
                Err(e) => eprintln!("preview: image-sequence decode failed: {e}"),
            }
            return None;
        }
        if spec.retime_ramped {
            // A RAMP-retimed layer decodes per-frame at its precise retimed
            // timestamp — the export loop's exact ramp resolution (WYSIWYG),
            // never the streaming pool (whose sequential session cannot
            // follow a varying cadence; pre-fix it respawned ffmpeg per tick
            // and starved the layer out of the composite entirely). Slow but
            // correct; the presenter HOLDs between productions.
            match decode_clip_frame(&spec.path, spec.source_us, spec.rotation, false, 0.0) {
                Ok(frame) => {
                    return Some(Layer {
                        frame,
                        opacity: spec.opacity,
                        transform: spec.transform,
                        crop: spec.crop,
                        alpha_mode: spec.alpha_mode,
                    })
                }
                Err(e) => eprintln!("preview: ramped-layer decode failed: {e}"),
            }
            return None;
        }
        if spec.is_still_image {
            // UAT perf fix: serve the position-invariant still frame from the
            // thread-local cache — decoded at most ONCE per (path, rotation) on
            // this thread, zero ffmpeg spawns on every later tick.
            let key = (spec.path.clone(), 0i64, spec.rotation);
            let frame = STILL_CACHE.with(|c| {
                let mut cache = c.borrow_mut();
                if let Some(f) = cache.get(&key) {
                    return Some(f);
                }
                match decode_clip_frame(&spec.path, 0, spec.rotation, false, 0.0) {
                    Ok(f) => {
                        cache.put(key.clone(), f.clone());
                        Some(f)
                    }
                    Err(e) => {
                        // Undecodable still contributes nothing — black shows
                        // through, mirroring the pool-miss / export skip.
                        eprintln!("preview: still-image decode failed: {e}");
                        None
                    }
                }
            });
            return frame.map(|frame| Layer {
                frame,
                opacity: spec.opacity,
                transform: spec.transform,
                crop: spec.crop,
                alpha_mode: spec.alpha_mode,
            });
        }
        // A frame from the pool this tick, or nothing (black shows through) —
        // the play-path twin of the export skip.
        by_id.remove(&spec.clip_id).map(|frame| Layer {
            frame,
            opacity: spec.opacity,
            transform: spec.transform,
            crop: spec.crop,
            // Phase 28 (OVL-01): play-path twin of the paused path — the
            // spec carries the clip's resolved alpha interpretation, feeding
            // the shader the SAME value the export loop does.
            alpha_mode: spec.alpha_mode,
        })
    }
}

/// Start the SAME all-contributor audio sum-mix the streaming path starts,
/// for the multi-layer branch: the mix range mirrors the streaming session's
/// (from `position_us` to the TOP active clip's timeline end). Returns the
/// output plus the timeline position where the mix ends — the caller restarts
/// the mix at that boundary — or `(None, i64::MAX)` when nothing resolves here
/// (retry only on a real change, never per tick).
///
/// Phase 46 (plan 46-06 gave it the [`crate::PreviewHost`] port instead of an
/// `AppHandle`; plan 46-08 moved it here). Its ONE store-touching callee
/// ([`resolve_program_audio`]) already lived in this crate, and nothing else in
/// this body ever used the handle, so the parameter — and with it the runtime
/// generic — went away entirely rather than being carried dead alongside `host`.
pub fn start_multi_audio(host: &dyn PreviewHost, position_us: i64) -> (Option<AudioOutput>, i64) {
    // quick-k0q: resolved through the ONE Program-mode seam (shared with the
    // streaming play loop). `None` → nothing audible here; the caller reads
    // `end` only when the output is `Some`, so `i64::MAX` is inert.
    let Some((mix_sources, tl_start, tl_end)) = resolve_program_audio(host, position_us) else {
        return (None, i64::MAX);
    };
    match AudioOutput::start_mix(mix_sources, tl_start, tl_end) {
        Ok(a) => (Some(a), tl_end),
        Err(e) => {
            eprintln!("preview: multi-layer audio start failed: {e}");
            (None, tl_end)
        }
    }
}

/// Present the EXACT frame at `position_us` on the ACTIVE monitor — the
/// paused/scrub still. Runs ONLY on the present (background) thread, NEVER the
/// UI thread: decoding here is what keeps scrubbing and replay from freezing the
/// window. Frame-accurate via `decode_frame_rgba_at`, with a small LRU so
/// scrub-back is instant. A timeline gap / no source loaded shows black.
///
/// Phase 46 (plans 46-06, 46-07 and 46-08): the handle is finally gone. 46-06
/// gave it a bare `&dyn PreviewHost` for `resolve_active`; 46-07 widened that to
/// the full [`PresentContext`] when `present_frame` / `black_frame` moved; 46-08
/// moved the last three shell-resident callees ([`resolve_multilayer`],
/// [`present_multilayer`]) into this crate and turned the `NativePreview` probe
/// into [`crate::PresentSink::has_surface`]. The half-extracted
/// `app`-and-`ctx`-together shape is collapsed.
/// Phase 49 (plan 49-01, OQ4): returns whether a frame actually reached the
/// sink for `position_us` — `false` when no surface is managed or the decode/
/// composite failed — so the present loop stamps a paused landing
/// ([`crate::PresentSink::note_presented_stamp`]) only when the still truly
/// presented. A gap's black frame IS a landing (the position was shown).
pub fn present_still(
    ctx: &PresentContext,
    is_source: bool,
    position_us: i64,
    cache: &mut FrameCache,
) -> bool {
    if !ctx.sink().has_surface() {
        return false;
    }
    // Phase 18 (COMP-02): a paused/scrubbed frame inside a multi-layer /
    // non-identity range shows the SAME composite the export encodes — never
    // the top layer alone (pause is exactly when a user inspects a composite).
    if !is_source {
        if let Some(stack) = resolve_multilayer(ctx.host(), position_us) {
            return present_multilayer(ctx, &stack, cache);
        }
    }
    let Some(resolved) = resolve_active(ctx.host(), is_source, position_us) else {
        return present_frame(ctx, black_frame());
    };
    let key = (resolved.path.clone(), resolved.source_us, resolved.rotation);
    if let Some(frame) = cache.get(&key) {
        return present_frame(ctx, frame);
    }
    // Phase 58 (D-27): the single-clip half of the same recorded decision as
    // `present_multilayer`'s decode site above — a paused/scrub still decodes
    // the ORIGINAL, at full quality, never a proxy. Not wired to the resolver
    // seam, deliberately.
    match engine::decode_frame_rgba_at(&resolved.path, resolved.source_us, resolved.rotation) {
        Ok(frame) => {
            cache.put(key, frame.clone());
            present_frame(ctx, frame)
        }
        Err(e) => {
            eprintln!("preview: scrub/paused decode failed: {e}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Mirrored pure helpers (see this module's header for WHY these are duplicated
// rather than imported). Each is byte-for-byte the body that lives in
// `crates/app-core/src/compose.rs`; `app-core` is the shell's application layer
// and sits ABOVE this crate, so importing them would invert the dependency
// arrow this whole phase exists to establish.
//
// MAINTENANCE CONTRACT: if either copy of any of the three ever needs a
// behavior change, BOTH copies must change together. Same accepted risk, and
// the same reason, as `app_core::compose::text_align_to_engine` mirroring
// `rudis_core::TextAlign`.
//
// Note for provenance bookkeeping: this is first-party Rudis code moving within
// the workspace, so `PROVENANCE.md` is NOT implicated (that file tracks
// third-party-derived work only).
// ---------------------------------------------------------------------------

/// Locate a clip anywhere on the timeline by id (the export-side twin of
/// core's private `locate()`), for reading its Phase-18 visual properties.
///
/// Mirror of `app_core::compose::timeline_clip`.
fn timeline_clip<'a>(
    timeline: &'a rudis_core::Timeline,
    clip_id: &str,
) -> Option<&'a rudis_core::Clip> {
    timeline
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .find(|c| c.id == clip_id)
}

/// Phase 28 (OVL-01): map a clip's core `AlphaMode` to the engine mirror the
/// compositor's `fs_layer` shader reads (via `Layer.alpha_mode` → `misc.y`).
/// A TOTAL match over the two-variant enum (no wildcard) — the compiler forces
/// any future variant to be handled here (threat T-28-08), so a media clip's
/// alpha interpretation can never silently fall back to `Straight`.
///
/// Mirror of `app_core::compose::engine_alpha_mode`.
fn engine_alpha_mode(m: rudis_core::AlphaMode) -> engine::AlphaMode {
    match m {
        rudis_core::AlphaMode::Straight => engine::AlphaMode::Straight,
        rudis_core::AlphaMode::Premultiplied => engine::AlphaMode::Premultiplied,
    }
}

/// Phase 28 (OVL-01, T-28-15): the ONE frame-decode entry point shared by EVERY
/// production composite/export site. A normal clip decodes via
/// [`engine::decode_frame_rgba_at`]; an imported numbered image sequence
/// (`is_image_sequence`, whose `path` is a confined `%0Nd` pattern) decodes via
/// [`engine::decode_frame_rgba_at_seq`] at its stored project fps. Keeping the
/// branch in ONE helper means preview, inspect and export can never diverge on
/// how a sequence frame is resolved — WYSIWYG by construction. A corrupt member
/// frame fails cleanly via the sidecar `Result` path (T-28-14), never a panic.
///
/// Mirror of `app_core::compose::decode_clip_frame`.
fn decode_clip_frame(
    path: &std::path::Path,
    source_us: i64,
    rotation: u32,
    is_image_sequence: bool,
    seq_fps: f64,
) -> Result<engine::Frame, engine::EngineError> {
    if is_image_sequence {
        engine::decode_frame_rgba_at_seq(path, source_us, seq_fps)
    } else {
        engine::decode_frame_rgba_at(path, source_us, rotation)
    }
}
