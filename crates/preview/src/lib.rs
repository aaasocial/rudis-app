//! Rudis preview engine (Phase 46, XTRC-02): the present loop, frame pacing,
//! the A/V clock, and seek/flush — everything that decides WHAT pixels to show
//! WHEN, extracted out of `src-tauri/src/native_surface.rs` and
//! `src-tauri/src/preview_ring.rs`.
//!
//! # The one architectural rule (D-02, locked)
//!
//! This crate has ZERO dependency on the desktop shell framework `src-tauri` is
//! built on. It never names an `AppHandle`, never registers or emits a shell
//! event directly, never touches managed state. Everything it needs from the
//! shell arrives through two narrow ports it defines itself and the shell
//! implements:
//!
//! - [`PresentSink`] — the GPU output port (reconfigure / present / size /
//!   first-show latch).
//! - [`PreviewHost`] — the shell-services port (project store, playback mirror,
//!   overlay resolution, live gesture trail, ink drawing, text rasterization,
//!   canvas-viewport hint).
//!
//! [`PresentContext`] owns one of each and is what every moved function is
//! written against. The dependency arrow points `src-tauri` -> `preview` and
//! ONLY that way; that is what lets the Phase 47 C ABI / C# shell reuse this
//! crate unchanged.
//!
//! The control that keeps the rule honest is a literal substring check over
//! `crates/preview/Cargo.toml`, so THAT file may not name the framework even in
//! prose (see its header). This module doc is not substring-gated and names
//! things plainly.
//!
//! # Status
//!
//! Plan 46-02 stands the crate up and defines the COMPLETE, final port shapes.
//! Later waves move code in behind them one batch at a time, each batch a
//! mechanical "move this function, wire it to the already-existing trait method"
//! step rather than a trait redesign. BOTH ports are load bearing now:
//! [`PreviewHost`] since wave 46-06 (`resolve.rs`'s store readers), and
//! [`PresentSink`] since wave 46-07 (`present.rs` / `overlay.rs`, the actual
//! on-screen present path).
//!
//! # Why the whole contract is defined up front
//!
//! Rust requires every method of a trait to be implemented in the SAME `impl`
//! block, so growing these ports wave-by-wave would force `src-tauri`'s adapter
//! (`TauriPreviewHost`, wave 46-03) to be rewritten on every batch instead of
//! written once. The signatures below are therefore LOCKED for the rest of the
//! phase: extend them only by adding a wholly new method, never by changing an
//! existing one.
//!
//! ## The one exception, taken at wave 46-07 and now closed
//!
//! Wiring the first REAL caller (plan 46-07) found three places where the
//! wave-2 draft could not express what the code it replaces actually does, and
//! corrected them while the ports were still called by nothing outside this
//! phase's own waves:
//!
//! 1. [`PresentSink::present`] returns `Result<bool, _>` (was `Result<(), _>`)
//!    — the `bool` is today's `dims_changed`, which only the sink can compute
//!    (it alone still holds the PRIOR frame at compare time).
//! 2. [`PresentSink::current_frame`] and [`PresentSink::present_overlay`] were
//!    ADDED (the permitted kind of extension). `present_overlay` is the
//!    deliberate resolution of D-46-03-02: `present` stores whatever it is
//!    handed, so the ink path needs a way to store the CLEAN frame while
//!    compositing the ANNOTATED one, or a resize re-present doubles the ink.
//! 3. `PreviewHost::emit(event, payload)` became
//!    [`PreviewHost::emit_canvas_viewport`] — the generic emit was speculative,
//!    and the real function needs a window `scale_factor()` query this crate
//!    may not make, so the WHOLE function had to go behind the port.
//!
//! Everything else is untouched, and the ports are closed to further change:
//! waves 46-08 … 46-11 extend by adding a method or not at all.
//!
//! ## The "add a method" route, taken once at wave 46-08
//!
//! Wave 46-08 moved the MULTI-LAYER composite path in ([`multilayer`]), and it
//! needed two shell capabilities no existing method could express — so it took
//! the permitted route and ADDED them, changing not one existing signature:
//!
//! 1. [`PresentSink::composite_layers`] — `present_multilayer` renders the
//!    resolved stack OFFSCREEN at project resolution and reads it back, which is
//!    the compositor the shell owns alongside the surface, not the surface
//!    itself.
//! 2. [`PresentSink::has_surface`] — `present_still`'s "is a surface managed at
//!    all" probe, which guarded a whole decode before any present was attempted.
//!    Every other sink method folds "unmanaged" into its own return value, so
//!    the probe had no existing expression that did not also do work.
//!
//! `git diff` over this file at wave 46-08 shows those two additions, the
//! `multilayer` module declaration, and nothing else.
//!
//! ## The same route, taken again at wave 46-09 — on the CONTEXT, not a port
//!
//! Wave 46-09 moved the read-ahead ring and its background producer in
//! ([`ring`]). `spawn_producer`'s callee is a THREAD, so it needs the host port
//! to OUTLIVE the calling stack frame — something neither
//! [`PresentContext::host`] (a borrow) nor any trait method can express. The
//! addition is therefore [`PresentContext::host_arc`], a cheap `Arc::clone` of
//! the field this context already owns. Note what it is NOT: no method was
//! added to [`PreviewHost`] or [`PresentSink`], and no existing signature
//! changed — the ports themselves are byte-untouched at wave 46-09.
//!
//! ## And once more at wave 46-10 — the last one
//!
//! Wave 46-10 moved the present loop itself in. Its whole body already spoke
//! through the ports; the ONE shell touch no method could express was the
//! `Arc<Compositor>` it clones out of the managed surface state at startup and
//! hands to the producer thread. That is a sink capability — the same object
//! that owns the GPU device owns the compositor — so the addition is
//! [`PresentSink::compositor`]. Again nothing existing changed: it is the
//! SIXTH addition in this crate's life (46-07's two, 46-08's two, 46-09's one
//! on the context, this one) against ZERO re-signings, and the rule that
//! produced all six is unchanged — **extend by adding a method, never by
//! changing an existing one**.
//!
//! ## The budgeted Phase-48 extension (plan 48-08) — the seventh addition
//!
//! Phase 48's GPU-resident frame path needs the one capability the
//! [`PresentSink`] doc has anticipated since this crate stood up: presenting a
//! hardware-decoded, GPU-RESIDENT frame without ever collapsing it to CPU
//! bytes. The addition is [`PresentSink::present_gpu`] — the GPU twin of
//! [`PresentSink::present`] — gated `cfg(all(windows, feature = "hwdecode"))`
//! because the [`engine::GpuFrame`] it carries exists nowhere else (the
//! `hwdecode` feature is this crate's own default-on forwarder to
//! `engine/hwdecode`, added by the same plan so the cfg predicate has
//! something to read). Per D-46-10-03 — "a port ADDITION breaks every
//! implementor, including test doubles" — the extension landed in ONE commit
//! updating all three implementors (the shell sink, [`RecordingPresentSink`],
//! and the overlay tests' FakeSink), so no deferred E0046 exists anywhere;
//! per D-46-08-01, zero existing signatures changed — this is the SEVENTH
//! addition against ZERO re-signings.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};

/// The pure preview predicates (plan 46-04): the shell-free decision functions
/// the present loop and the edit-seq listener are built from, moved verbatim out
/// of `src-tauri/src/native_surface.rs`. Glob-re-exported so `preview::X`
/// resolves exactly as `crate::native_surface::X` used to, which is what lets
/// the shell keep a one-line `pub(crate) use preview::{…}` shim instead of
/// rewriting every call site.
mod predicates;
pub use predicates::*;

/// The mid-play edit signal (plan 46-05): `PreviewEditPending` /
/// `PreviewEditSeq` plus `observe_patch`, the parse-and-record half of today's
/// `register_edit_seq_listener` closure. The shell keeps ONLY the registration
/// mechanics (`manage` / `listen_any` / the payload parse) — see the module's
/// own doc for why this is a shared-`Arc`-state port rather than a
/// [`PreviewHost`] callback.
mod edit_seq;
pub use edit_seq::*;

/// Preview resolution (plan 46-06): `Resolved` + `resolve_active` (what the
/// active monitor decodes here), `resolve_audio_mix` / `resolve_program_audio`
/// (what the preview mix sums here) and `edit_touches_playhead_audio`, moved out
/// of `src-tauri/src/native_surface.rs`. These are the FIRST callers of
/// [`PreviewHost::store`] — the wave where the port stops being a design and
/// starts being load bearing.
mod resolve;
pub use resolve::*;

/// Canvas-overlay compositing (plan 46-07): `composite_with_overlay`,
/// `overlay_signature` and `repaint_overlay`, moved out of
/// `src-tauri/src/native_surface.rs`. The module doc carries the Pitfall-2
/// analysis that decided [`PresentSink::present_overlay`]'s shape — read it
/// before touching either.
mod overlay;
pub use overlay::*;

/// Frame presentation (plan 46-07): `present_frame` — the ONE seam every
/// present-path branch funnels through — plus `black_frame` and
/// `placeholder_frame`, moved out of `src-tauri/src/native_surface.rs`. This is
/// the first REAL caller of [`PresentSink`], so the actual on-screen GPU present
/// now crosses the port boundary this phase exists to build.
mod present;
pub use present::*;

/// The multi-layer composite path (plan 46-08): `FrameCache`, `LayerSpec`,
/// `MultiLayerStack`, `resolve_multilayer`, `present_multilayer`,
/// `compose_multilayer_from_pool`, `start_multi_audio` and `present_still`,
/// moved out of `src-tauri/src/native_surface.rs`. This is the wave that makes
/// [`PreviewHost::rasterize_text`] load bearing — the ONE shared text-layer
/// builder preview and export both call, reached through the port rather than
/// duplicated, so text composites byte-identically on both sides.
mod multilayer;
pub use multilayer::*;

/// The read-ahead composited-frame ring buffer and its background producer
/// (plan 46-09): `RingEntry`, `RingCtl`, `ring_depth`, `spawn_producer` and the
/// whole `producer_loop` / lookahead-prewarm state machine, moved WHOLE out of
/// `src-tauri/src/preview_ring.rs` — that file is deleted, not shimmed. It is
/// the only module here whose entry point takes an OWNED
/// `std::sync::Arc<dyn PreviewHost>` ([`PresentContext::host_arc`]) rather than
/// a borrowed `&dyn PreviewHost`, because the producer THREAD outlives the frame
/// that spawns it. This is the file Phase 48's GPU-resident path replaces, which
/// is why D-03 scopes it into this crate.
mod ring;
pub use ring::*;

/// The present loop itself (plan 46-10): the dedicated thread that reads the
/// playback mirror, drives the flush handshake, paces video off the audio
/// hardware clock and pops/presents every frame — moved out of
/// `src-tauri/src/native_surface.rs`, which keeps only the Win32 window glue and
/// the `setup()` wiring that spawns this. It is the LAST module of the split and
/// the reason the other eight exist: every function it calls came across in an
/// earlier wave, so the move was mechanical rather than a redesign.
mod present_loop;
pub use present_loop::*;

/// Real-PTS → timeline stamping (Phase 49, SEEK-02): the ONE shared mapping
/// both decode paths (hardware `gpu_producer_loop` + software `producer_loop`)
/// stamp `RingEntry::timeline_us` through, plus the observable
/// synthetic-fallback counter the locked decision requires (a silent fallback
/// would re-create the VFR drift bug and hide it). Kept `pub mod` so call
/// sites and tests can name the qualified `pts_map::` path; glob-re-exported
/// like every sibling module.
pub mod pts_map;
pub use pts_map::*;

/// The headless recording sink (Phase 48, plan 48-01): `RecordingPresentSink`,
/// a timestamped-recording implementor of the UNCHANGED [`PresentSink`] trait
/// below. It exists to close two findings Phase 46 carried forward —
/// D-46-11-03 (no automated test in either tier exercised
/// [`present_loop`]'s pacing / A-V-clock / seek-flush body) and D-46-11-02
/// (present-path dispatch cost was bounded but never measured) — and to serve
/// as the before/after ruler for Phase 48's present-path replacement (plans
/// 48-09 / 48-11 diff against its `PRESENT_BASELINE` numbers). Production
/// code, not `#[cfg(test)]`: plan 48-11's GPU-06 readback-isolation proof
/// drives it from other test targets, and it carries zero shell dependencies.
mod recording_sink;
pub use recording_sink::*;

/// The `clip → decode-source` resolver seam (Phase 57, CONTEXT D-05):
/// `DecodeSource`, `DecodeSourceKind` and `resolve_decode_source` — the ONE
/// place that maps (clip identity, media path, source position) → what a
/// decode session opens. Phase 58 adds proxies behind it (REQUIREMENTS
/// PROXY-03) without touching the decode or composite code, which is what
/// makes Phase 57 ∥ Phase 58 parallel-eligible. `pub mod` like [`pts_map`] so
/// call sites can name the qualified `decode_source::` path; glob-re-exported
/// like every sibling module.
pub mod decode_source;
pub use decode_source::*;

/// Automatic, hysteretic dynamic playback resolution (Phase 57, plan 57-08,
/// PLAY-05/D-09) — `ResLevel` + `DynResController`. Pure decision logic, no
/// `cfg` gate: the composite it governs is plain `wgpu` and runs on every
/// platform and every build. `pub mod` like [`pts_map`] / [`decode_source`],
/// glob-re-exported the same way.
pub mod dynres;
pub use dynres::*;

/// Automatic, hysteretic frame dropping (Phase 60, plan 60-04, DROP-01/DROP-02)
/// — `DropMode` + `FrameDropController`, the SECOND playback degradation axis
/// and the structural sibling of [`dynres`]. Pure decision logic with the same
/// properties: no `cfg` gate, `!Send`, stack-local in the producer, transient.
///
/// The two axes are ORDERED, not peers: this one counts a miss only while
/// [`dynres`]'s ladder is already at its floor, so every overload the resolution
/// axis alone can absorb behaves exactly as it did before this module existed.
/// `pub mod` like [`pts_map`] / [`decode_source`] / [`dynres`], glob-re-exported
/// the same way.
pub mod framedrop;
pub use framedrop::*;

/// The occlusion predicate (Phase 60, plan 60-07, OCCL-01) —
/// `is_full_canvas_occluder` + `cull_occluded`. Pure decision logic over an
/// already-resolved stack: no `cfg` gate, no I/O, no allocation, and it is
/// applied at exactly ONE place, [`multilayer::resolve_multilayer`]'s final step,
/// so every consumer of a resolved stack sees the same position-correct answer.
///
/// **NOT glob-re-exported, deliberately.** `cull_occluded` is a truncation of the
/// stack every downstream decode/session/composite decision is made from, so the
/// wiring is pinned by source-level scans that count which files reach it — and a
/// glob re-export would let a second call site apply the cull while spelling
/// nothing a scan can find. Same rule, same reason, as [`render_cache_detect`].
pub mod occlusion;

/// The per-visible-layer hardware decode-session coordinator (Phase 57, plan
/// 57-06, PLAY-01/D-03) — `LayerSessionSet`, the N-session generalization of
/// the ring's one-clip GPU delegation. Windows + hwdecode only, because every
/// type it owns (`HwDecodeSession`, `GpuFrame`, `VramLedger`) exists only
/// there; the producer's multi arm compiles to a pure-software path elsewhere.
/// `pub mod` like [`pts_map`] / [`decode_source`], glob-re-exported the same
/// way.
#[cfg(all(windows, feature = "hwdecode"))]
pub mod layer_sessions;
#[cfg(all(windows, feature = "hwdecode"))]
pub use layer_sessions::*;

/// Per-SEGMENT heaviness detection (Phase 59, plan 59-05, CACHE-01/D-11/D-12/
/// D-13) — `note_live_tick`, `prearm_stack`, `heavy_segments`. The measured
/// budget-miss evidence [`dynres`] already computes, re-scored against the
/// PROGRAM RANGE that produced it instead of against the session that observed
/// it.
///
/// **NOT glob-re-exported, deliberately** (the first module in this crate that
/// is not). The ring, the background worker and `app-core` name it explicitly
/// as `render_cache_detect::note_live_tick`, because the whole render-cache
/// seam is pinned by source-level scans that count which files reach it — and a
/// glob re-export would let a call site touch the detector while spelling
/// nothing that a scan can find. See `render_cache_lookup` for the same rule
/// applied to the reader.
pub mod render_cache_detect;

/// The program-level render-cache READER (Phase 59, plan 59-05, CACHE-01/
/// CACHE-03, D-19) — `SegmentLookup`, `configure_render_cache_dir` and the
/// lookup-cost instrumentation.
///
/// A DIFFERENT ALTITUDE from [`decode_source`], and the difference is the whole
/// design: `resolve_decode_source` answers *"what media should this CLIP's
/// session open?"*, while this answers *"can this whole TICK skip compositing
/// altogether?"*. Jamming a cache variant into the clip-level seam would put
/// program-level state inside a per-clip resolver and break Phase 58 D-16's
/// one-file-knows-about-proxies property; this module CALLS that seam instead,
/// per clip, and takes its answer as data.
///
/// **NOT glob-re-exported**, for the same reason [`render_cache_detect`] is
/// not: D-40 pins the number of files in this crate that may name the
/// render-cache crate at exactly two — this reader and 59-07's writer — by a
/// source-level scan, and a glob re-export would let a third file reach the
/// lookup while spelling nothing that scan can find.
pub mod render_cache_lookup;

/// The program-level render-cache WRITER (Phase 59, plan 59-07, CACHE-01/
/// CACHE-02, D-06/D-14/D-18/D-39) — `render_segment` and `RenderOutcome`: one
/// segment index in, one committed cache file out, or a clean abort.
///
/// It lives in THIS crate, beside the reader, for the reason D-39 records: the
/// render loop needs [`multilayer`]'s `pub(crate)` compose path
/// (`pool_sources`, `cpu_layer_for_spec`), and widening those to `pub` to buy
/// one caller would push the multilayer resolver's internals across a crate
/// boundary. Moving the LOOP here instead widens nothing. Scheduling — when a
/// render runs, how many at once, who cancels one — is deliberately NOT here;
/// that is `app-core`'s layer (plan 59-08), which calls in.
///
/// **NOT glob-re-exported**, for the same reason [`render_cache_lookup`] and
/// [`render_cache_detect`] are not: this is the SECOND and last file in this
/// crate allowed to name the render-cache crate, and the source scan that pins
/// that count cannot see a call site that spells nothing.
pub mod render_cache_writer;

/// Boundary-PREWARM kill switch (Phase 57, plan 57-07) — the one-variable
/// lever a warm-vs-cold entry differential needs.
///
/// When this environment variable is set (to anything), **both** boundary
/// prewarm paths become no-ops for the rest of the process's ticks:
/// [`prewarm_boundary_stack`]'s software pool warm-up during a GPU delegation
/// wait, and [`layer_sessions::LayerSessionSet::prewarm_for_boundary`]'s
/// hardware twin. Nothing else changes — the boundary is simply entered cold.
///
/// # Why an env var and not a parameter
///
/// The prewarm decision is made three call levels below anything a test can
/// reach: the producer thread owns the `Lookahead`, the delegation wait owns
/// the timing, and `present_loop_with_gpu_budget` — the shipping app's route,
/// which is the ONLY route a PLAY-03 number may be measured on — takes no
/// configuration at all. Threading a flag down would mean changing three
/// production signatures to serve a measurement; an env var read at the two
/// decision points changes none of them.
///
/// # Cost, stated because it sits near a hot loop
///
/// Read with `var_os` at each prewarm DECISION, never cached — a differential
/// has to flip it between repetitions inside one process (the same shape
/// `hwdecode_zero_copy.rs` already uses for [`engine::KILL_SWITCH_ENV`]).
/// `prewarm_boundary_stack` runs at most once per delegation, and
/// `prewarm_for_boundary` only while a boundary is latched in the lookahead
/// horizon — tens of reads per boundary, not per frame.
pub const DISABLE_PREWARM_ENV: &str = "RUDIS_DISABLE_BOUNDARY_PREWARM";

/// Is boundary prewarm switched off for this process right now?
/// See [`DISABLE_PREWARM_ENV`].
pub fn boundary_prewarm_disabled() -> bool {
    std::env::var_os(DISABLE_PREWARM_ENV).is_some()
}

/// The narrow GPU output port (D-01). Takes today's plain CPU
/// [`engine::Frame`], unchanged — every present-path branch in the current code
/// already collapses to a `Frame` before reaching the surface (verified:
/// `present_frame`, `present_multilayer`, `present_still` all funnel here).
/// Phase 48 EXTENDS this trait with a new method for the GPU-resident path; it
/// does not need to widen [`PresentSink::present`]'s signature (SPIKE-06 /
/// GPU-05: the CPU path is kept alive as the permanent, never-deleted
/// fallback).
pub trait PresentSink: Send + Sync {
    /// Reconfigure the surface if a resize is pending (mirrors
    /// `reconfigure_and_present`'s `surface_dirty` check). Returns the size it
    /// was reconfigured to, if any.
    fn reconfigure_if_dirty(&self) -> Option<(u32, u32)>;
    /// Composite `frame` onto the surface and present it; remembers it as
    /// "current" (today's `np.frame = frame`) for the next resize re-present.
    ///
    /// Returns `dims_changed`: whether `frame`'s content dimensions differ from
    /// those of the frame previously shown (today's inline
    /// `np.frame.width != frame.width || …`, computed BEFORE `np.frame` is
    /// overwritten). Only the sink still has the prior frame at the moment of
    /// that compare, so the sink is what must compute and report it; the caller
    /// ORs it with `reconfigure_if_dirty`'s answer to decide whether the
    /// canvas-viewport hint needs re-emitting.
    ///
    /// This is the ZERO-INK path only. When the overlay has something to draw,
    /// [`PresentSink::present_overlay`] is what must be called instead — see its
    /// doc for why handing the annotated clone to THIS method would be a bug.
    fn present(&self, frame: &engine::Frame) -> Result<bool, engine::EngineError>;
    /// Composite `annotated` onto the surface WITHOUT ever remembering the ink.
    ///
    /// This method exists because [`PresentSink::present`] stores exactly what
    /// it composites, which is right for a pristine decoded frame and WRONG for
    /// an ink-annotated one: a resize re-presents the STORED frame, so storing
    /// the annotated clone would bake the ink in and draw it again — doubling it
    /// — on every resize (Pitfall 2, and the trap recorded as D-46-03-02 for
    /// exactly this wave). `overlay::composite_with_overlay` therefore routes
    /// its ink path through here.
    ///
    /// * `clean` — the pristine frame to remember as "current". `None` means
    ///   "keep whatever is already stored", which is the paused-repaint case
    ///   (`overlay::repaint_overlay` re-composites the ALREADY-stored frame, so
    ///   re-storing identical bytes would only cost a second full-frame copy).
    /// * `annotated` — the bytes to actually put on the surface.
    ///
    /// Returns the same `dims_changed` [`PresentSink::present`] does, computed
    /// against `clean`; `false` when `clean` is `None` (nothing new arrived, so
    /// the content rect cannot have moved).
    fn present_overlay(
        &self,
        clean: Option<&engine::Frame>,
        annotated: &engine::Frame,
    ) -> Result<bool, engine::EngineError>;
    /// The frame currently stored as "current" (today's `np.frame`), cloned.
    /// `None` when no surface is managed.
    ///
    /// `overlay::repaint_overlay` needs to re-composite the ALREADY-stored frame
    /// with a changed overlay WITHOUT decoding a new one, and only the sink has
    /// it. Fires ONLY on the paused branch when the overlay signature changes at
    /// an unchanged playhead — never on the ~60Hz playing hot path (Assumption
    /// A5: a GPU-resident Phase-48 sink answers this with a paused-only,
    /// off-hot-path CPU readback, the same capability class GPU-06 already
    /// requires it to support for export / thumbnails / agent frame inspection).
    fn current_frame(&self) -> Option<engine::Frame>;
    /// `(configured_w, configured_h)` — what `emit_canvas_viewport` needs today
    /// via `np.config_w`/`np.config_h`.
    fn configured_size(&self) -> (u32, u32);
    /// True the first time this is called (mirrors `NativePreview.shown`).
    fn mark_shown_once(&self) -> bool;
    /// Render `layers` OFFSCREEN at `width` x `height` and read the RGBA back —
    /// today's `np.compositor.composite_layers_to_rgba(..)`, the ONE composite
    /// call preview and export share (D-02 one-composite-path).
    ///
    /// ADDED at wave 46-08 for [`multilayer::present_multilayer`], which needs
    /// the compositor the shell owns ALONGSIDE the surface rather than the
    /// surface itself. It is a sink capability, not a host one: the same object
    /// that owns the GPU device owns this, and Phase 48's GPU-resident sink
    /// answers it without a readback at all.
    ///
    /// `None` folds together the three cases the original treated identically —
    /// no surface managed (mock runtime), a poisoned present lock, and a failed
    /// composite — each of which returned from `present_multilayer` without
    /// presenting anything. The implementation logs a real composite failure, as
    /// the original did; the other two are silent, as they were.
    ///
    /// The lock is taken and RELEASED inside this call, exactly as the original
    /// scoped its guard, so [`present`](PresentSink::present) can re-take it
    /// afterwards without a nested acquisition.
    fn composite_layers(
        &self,
        layers: &[engine::Layer],
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>>;
    /// Is a surface managed at all? Today's
    /// `app.try_state::<Mutex<NativePreview>>().is_some()`.
    ///
    /// ADDED at wave 46-08 for [`multilayer::present_still`], whose FIRST
    /// statement is this probe: under a mock runtime with no surface there is
    /// nothing to present to, so it must not resolve, decode or composite
    /// anything. Every other method on this trait folds "unmanaged" into its own
    /// return value, which means asking through one of them would already have
    /// done the work the probe exists to avoid (or, for
    /// [`current_frame`](PresentSink::current_frame), cloned a whole frame).
    fn has_surface(&self) -> bool;
    /// A clone of the compositor the shell owns alongside the surface — today's
    /// `np.compositor.clone()`.
    ///
    /// ADDED at wave 46-10 for [`present_loop::present_loop`], which extracts it
    /// ONCE at thread startup and hands it to [`ring::spawn_producer`] so the
    /// producer composites multi-layer frames OFFSCREEN on its own thread,
    /// through the SAME `Arc<Compositor>` (wgpu `Device`/`Queue` are internally
    /// synchronized and an offscreen render+readback never touches the
    /// presenter's surface lock). It is the ownership twin of
    /// [`PresentContext::host_arc`]: the producer THREAD outlives the frame that
    /// spawns it, so it needs an OWNED handle, not a borrow.
    ///
    /// `None` means no surface is managed (a mock runtime without `setup()`),
    /// which is exactly what the original's `try_state(..).and_then(..)` yielded
    /// — and where nothing presents anyway, so no producer is spawned.
    ///
    /// This is the ONE call that must not be mistaken for the render path:
    /// [`PresentSink::composite_layers`] is the shell doing an offscreen
    /// composite for a caller on the SHELL's lock; this hands the compositor
    /// itself to a caller that will use it on ANOTHER thread, off that lock
    /// entirely. Phase 48's GPU-resident sink answers both without a readback.
    fn compositor(&self) -> Option<std::sync::Arc<engine::Compositor>>;
    /// Phase 49 (SEEK-01, OQ4): the present loop reports the timeline stamp
    /// (`timeline_us`) of the ring entry it just presented, immediately after
    /// a SUCCESSFUL `present`/`present_gpu` — and, on the paused branch, after
    /// a successful still re-present with the CURRENT position (a paused
    /// re-present IS a landing: scrub-release lands paused, and without this
    /// call site a landing harness cannot see paused landings at all). Never
    /// called on a failed present.
    ///
    /// ADDED at wave 49-01 as the port-doc-justified Phase 46 exception
    /// (D-46-08-01: the ports are CLOSED to signature changes, but an ADDITION
    /// is allowed when justified here in the port doc): this method carries a
    /// DEFAULT no-op body, so all four existing implementors compile untouched
    /// — the D-46-10-03 implementor-break cost never arises. Instrumentation
    /// only, never load-bearing for correctness: a stamp proves the LOOP
    /// presented that entry, not what its pixels were (pixel content stays
    /// covered by the parity suites). `RecordingPresentSink` is the one
    /// implementor that overrides it, pairing each stamp with a monotonic
    /// `Instant` so SEEK-01's landing-latency gate can measure "landed at
    /// TARGET position" as a distinct event from "a present happened".
    fn note_presented_stamp(&self, _timeline_us: i64) {}
    /// Composite a GPU-RESIDENT frame onto the surface and present it (Phase
    /// 48, GPU-01; plan 48-08). The GPU twin of [`PresentSink::present`]: same
    /// `dims_changed` contract (computed against the PREVIOUSLY-shown frame,
    /// whichever kind it was, before this one replaces it), same
    /// stored-as-current semantics — a [`engine::GpuFrame`] is always pristine
    /// decoded output, never annotated, so D-46-03-02's ink trap does not
    /// arise here and the ink-annotated path stays on
    /// [`PresentSink::present_overlay`], which remains CPU-side this phase.
    ///
    /// Implementations MUST NOT read pixels back on this path (GPU-06): the
    /// composite renders the frame's NV12 plane views straight to the
    /// swapchain. The paused-only readback capability stays on
    /// [`PresentSink::current_frame`], which answers for a stored GPU frame
    /// via the offscreen `composite_gpu_to_rgba` twin — never the present hot
    /// path.
    ///
    /// Windows + hwdecode builds only — the type it carries does not exist
    /// elsewhere. `feature = "hwdecode"` reads THIS crate's namespace: it is
    /// preview's own default-on forwarder feature (`hwdecode =
    /// ["engine/hwdecode"]`, added at 48-08 alongside this method).
    ///
    /// Nothing produces GPU frames until plan 48-09 flips the producer; this
    /// method is the 48-08 structural seam, exercised live from 48-09 on.
    #[cfg(all(windows, feature = "hwdecode"))]
    fn present_gpu(&self, frame: &engine::GpuFrame) -> Result<bool, engine::EngineError>;

    /// Can this sink present an ALREADY-COMPOSITED GPU texture without a
    /// readback? (Phase 57, plan 57-06.)
    ///
    /// Defaults to `false`, and the default is load-bearing rather than
    /// cautious: it is what lets the producer decide, at spawn time, whether to
    /// push [`crate::RingPayload::Composited`] at all. A sink that answers
    /// `false` never sees one — the producer performs the (single, identical)
    /// readback on its OWN thread and pushes the CPU payload, exactly as it did
    /// before this phase. So a sink that has not been taught the texture path
    /// keeps today's behaviour AND today's cost profile, with the readback on
    /// the thread that has always paid it, never moved onto the latency-critical
    /// present thread.
    ///
    /// The SHIPPED shell adapter no longer takes this default. At 57-06 both
    /// production adapters did — D-01 froze the shells for that phase — and the
    /// override was routed "to the first plan permitted to touch a shell adapter
    /// (post-cutover)". Phase 55's cutover deleted `src-tauri/` (so there is no
    /// `TauriPresentSink` left to teach) and opened that window, and quick
    /// 260803-cws walked through it: `crates/ffi`'s `ShellPresentSink`
    /// (`crates/ffi/src/panel/sink.rs`) overrides this AND
    /// [`PresentSink::present_composited`], answering `true` whenever its
    /// compositor and surface are both live.
    ///
    /// What still takes the default: every test double, and any future adapter
    /// that has not been taught the texture path — which is exactly the point
    /// of the default being `false`.
    fn supports_composited_present(&self) -> bool {
        false
    }

    /// Present an already-composited GPU texture, contain-fit letterboxed at
    /// `content_w`x`content_h` (Phase 57, plan 57-06, PLAY-02/D-07).
    ///
    /// Same `dims_changed` contract as [`PresentSink::present`] /
    /// [`PresentSink::present_gpu`]: computed against the previously-shown
    /// frame, whichever kind it was, before this one replaces it.
    ///
    /// # What an implementor that owns a surface should do
    ///
    /// ```ignore
    /// let (compositor, surface) = /* the sink's own GPU set */;
    /// compositor.blit_texture_to_surface(target.view(), content_w, content_h, surface)?;
    /// ```
    ///
    /// That is the whole override — `engine::Compositor::blit_texture_to_surface`
    /// exists for exactly this call — and it is what deletes the LAST readback
    /// from the multi-layer playback path.
    ///
    /// The SHIPPED implementation of that is
    /// `crates/ffi/src/panel/sink.rs::ShellPresentSink::present_composited`
    /// (quick 260803-cws), pinned structurally by
    /// `crates/ffi/tests/composited_present_pin.rs`. It is not five lines: the
    /// blit is, but a composited frame is a THIRD frame kind, so the sink also
    /// carries cross-kind `dims_changed` bookkeeping and must publish the
    /// contain-fit content rect the pointer mapping and ink overlay read.
    ///
    /// # Why the default reads back instead
    ///
    /// The default cannot reach a surface: the port deliberately exposes the
    /// compositor ([`PresentSink::compositor`]) and not the swapchain, because
    /// the surface lock is the sink's own. So it does the honest thing —
    /// samples the target through `blit_texture_to_rgba` and delegates to
    /// [`PresentSink::present`], which every implementor already has. Pixels are
    /// correct; the readback is not deleted.
    ///
    /// In practice the default is a SAFETY NET, not the shipped path: a sink
    /// reaches it only by answering `true` to
    /// [`PresentSink::supports_composited_present`] and then not overriding
    /// this — a combination no implementor in the tree has. The two that DO
    /// answer `true` both override: the shipped `ShellPresentSink` (which
    /// blits, as above) and the harness [`RecordingPresentSink`], which has no
    /// swapchain and so reads back deliberately — a HARNESS cost its own doc
    /// names, not a path cost.
    ///
    /// Added under the D-46-08-01 port exception (an ADDITION with a default
    /// body is allowed when justified in the port doc), for the same reason
    /// `note_presented_stamp` was: all existing implementors compile untouched.
    fn present_composited(
        &self,
        target: &engine::PooledTarget,
        content_w: u32,
        content_h: u32,
    ) -> Result<bool, engine::EngineError> {
        let Some(compositor) = self.compositor() else {
            return Ok(false); // no surface managed — nothing to present to
        };
        let rgba = compositor.blit_texture_to_rgba(target.view(), content_w, content_h)?;
        self.present(&engine::Frame {
            width: content_w,
            height: content_h,
            rgba,
        })
    }
}

/// The shell-services port (D-01/D-02). `crates/preview` is written against
/// `&dyn PreviewHost` only — zero dependency on the desktop shell framework,
/// zero knowledge of app handles or shell events. The Rudis shell crate
/// implements this via `TauriPreviewHost` (wave 46-03).
pub trait PreviewHost: Send + Sync {
    /// Read the backend-owned project store. `None` when unmanaged (mock
    /// runtime without `setup()`) — callers already treat that as "nothing
    /// resolves here", matching today's `app.try_state::<SharedStore>()?`.
    fn store(&self) -> Option<std::sync::MutexGuard<'_, rudis_core::Store>>;
    /// The lock-free playback mirror the present thread / producer thread read
    /// every tick.
    fn playback_mirror(&self) -> &PlaybackMirror;
    /// The engine→shell diagnostic mirror the producer WRITES (Phase 57, plan
    /// 57-08, PLAY-05/D-01).
    ///
    /// The direction is the opposite of [`PreviewHost::playback_mirror`]'s, and
    /// that is the whole reason it is a separate method on a separate struct —
    /// see [`EngineDiag`].
    ///
    /// Added under the D-46-08-01 port exception (an ADDITION with a default
    /// body is allowed when justified in the port doc), for exactly the reason
    /// [`PresentSink::note_presented_stamp`] and
    /// [`PresentSink::supports_composited_present`] were: every existing
    /// implementor — the Tauri adapter, `crates/ffi`'s `ShellPreviewHost`, and
    /// the dozen test doubles — compiles untouched. D-01 holds because no shell
    /// file has to change for the level to be produced.
    ///
    /// The default answers with the process-wide [`process_engine_diag`]. A host
    /// that wants a per-instance observable overrides it; `crates/ffi` does, so
    /// its ABI getter reads THAT ctx's producer and two ctxs stay independent.
    fn engine_diag(&self) -> &EngineDiag {
        process_engine_diag()
    }
    /// The current fade-filtered visible annotation set (never blocks the
    /// caller — the shell impl keeps today's `try_lock` + thread-local-cache
    /// discipline internally; `crates/preview` only ever sees the result).
    fn resolve_overlay(&self) -> Vec<(rudis_core::Annotation, f32)>;
    /// The in-progress Win32 pointer gesture trail (empty off Windows / when
    /// idle). Mirrors today's `canvas_input::LIVE_GESTURE.try_lock()`.
    fn live_gesture(&self) -> Vec<(f32, f32)>;
    /// The design-token ink color (today's `crate::OVERLAY_INK`).
    fn overlay_ink(&self) -> [u8; 4];
    /// Draw `annotations` onto `frame` in `ink` (today's
    /// `crate::draw_annotations_onto_styled`).
    fn draw_ink(
        &self,
        frame: &mut engine::Frame,
        annotations: &[rudis_core::Annotation],
        ink: [u8; 4],
        dashed: bool,
    );
    /// Rasterize a text layer (today's `rasterize_text_layer`, which owns the
    /// per-thread `TEXT_RASTERIZER` cache internally — it stays the ONE shared
    /// implementation preview and export both call, so the two composite text
    /// byte-identically. Deliberately a pass-through, never a duplicate.)
    fn rasterize_text(
        &self,
        text: &rudis_core::TextPayload,
        transform: engine::LayerTransform,
        opacity: f32,
        crop: engine::LayerCrop,
        project_w: u32,
        project_h: u32,
    ) -> engine::Layer;
    /// Tell the renderer where the frame CONTENT (contain-fit, excluding the
    /// letterbox bars) sits, so the ink-layer overlay can match the visible
    /// video exactly — today's `native_surface::emit_canvas_viewport`, moved
    /// WHOLE behind the port rather than just its `emit` call.
    ///
    /// The whole function has to live here, not just the event dispatch: the
    /// payload is computed from the main webview's `scale_factor()`, a
    /// shell-window query `crates/preview` cannot perform (D-02). Wave 2's
    /// generic `emit(event, payload)` was speculative — it named
    /// `"canvas-viewport"` as the ONLY engine-side emit and kept the signature
    /// wide "just in case"; this is the narrow, real capability, and it matches
    /// `46-CONTEXT.md`'s own code map, which already classifies
    /// `emit_canvas_viewport` as shell-half.
    ///
    /// `win_w`/`win_h` are the video window's CONFIGURED physical size
    /// ([`PresentSink::configured_size`]); `frame_w`/`frame_h` the presented
    /// frame's content dimensions.
    fn emit_canvas_viewport(&self, win_w: u32, win_h: u32, frame_w: u32, frame_h: u32);
}

/// Owns both ports; what `present_loop` / `producer_loop` / `present_frame` etc.
/// are written against. Cheap to `Clone` (`Arc` bumps) so the present thread and
/// the producer thread can each hold their own.
#[derive(Clone)]
pub struct PresentContext {
    host: std::sync::Arc<dyn PreviewHost>,
    sink: std::sync::Arc<dyn PresentSink>,
}

impl PresentContext {
    pub fn new(
        host: std::sync::Arc<dyn PreviewHost>,
        sink: std::sync::Arc<dyn PresentSink>,
    ) -> Self {
        Self { host, sink }
    }
    pub fn host(&self) -> &dyn PreviewHost {
        self.host.as_ref()
    }
    /// The host port as an OWNED handle (a cheap `Arc` bump of the field this
    /// context already holds).
    ///
    /// ADDED at wave 46-09 for [`ring::spawn_producer`], the one consumer whose
    /// callee outlives the calling stack frame: the producer THREAD keeps the
    /// port for its whole life, so a borrowed [`PresentContext::host`] cannot
    /// express it. Sharing this context's `Arc` — rather than building a second
    /// adapter inside the producer — keeps the present thread and the producer
    /// thread on ONE host instance, which is what the old
    /// `spawn_producer(app.clone(), …)` effectively did with its handle.
    pub fn host_arc(&self) -> std::sync::Arc<dyn PreviewHost> {
        std::sync::Arc::clone(&self.host)
    }
    pub fn sink(&self) -> &dyn PresentSink {
        self.sink.as_ref()
    }
}

/// Phase 9 Wave 3: a lock-free atomic mirror of the playback fields the native
/// present thread reads every frame (30-120Hz). It must NOT lock `SharedStore`
/// on that hot path (Pitfall D), so `transport()` publishes the authoritative
/// `Playback` here alongside `PLAYBACK_CHANGED_EVENT`. This is an
/// implementation detail of the present thread's lock-avoidance, NOT new domain
/// state — mutation still flows exclusively through `TransportCmd::apply()`
/// under the store mutex ("backend owns state"). `Relaxed` ordering is fine: a
/// one-frame-stale position is a harmless presentation-pacing hint.
///
/// Only present under the windowed app (managed in `native_surface::setup`);
/// the mock-runtime `configure()` never manages it, so `transport()` fetches it
/// with `try_state` and no-ops when absent.
///
/// Phase 46 (plan 46-02): MOVED here verbatim from `src-tauri/src/lib.rs`,
/// field-for-field and method-for-method. It carries no shell dependency (five
/// atomics and two plain methods), and [`PreviewHost::playback_mirror`] must be
/// able to name it — a return type living in `src-tauri` would point the
/// dependency arrow the wrong way. The shell keeps a `pub use` re-export so
/// every existing `crate::PlaybackMirror` call site compiles unchanged.
pub struct PlaybackMirror {
    pub position_us: AtomicI64,
    pub playing: AtomicBool,
    pub duration_us: AtomicI64,
    /// Active preview monitor: `true` = Source (a MediaBin clip), `false` =
    /// Program (the timeline). The present thread resolves what to decode from
    /// this (source clip vs `top_video_active_at`).
    pub is_source: AtomicBool,
    /// Explicit reposition signal (18.2-05): bumped by `transport()` ONLY on
    /// genuine reposition commands (`Seek` / `Step`) — NEVER on per-frame
    /// `Advance`. The present thread's MULTI-LAYER branch keys its
    /// pool-teardown decision off this (a changed seq == a real user seek),
    /// replacing the wall-clock position-jump heuristic that misread slow
    /// decoder warm-up ticks as seeks (the live black-preview doom loop).
    /// Wrapping is harmless — consumers compare inequality only.
    pub seek_seq: AtomicU64,
}

impl PlaybackMirror {
    pub fn new() -> Self {
        Self {
            position_us: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            duration_us: AtomicI64::new(0),
            is_source: AtomicBool::new(false),
            seek_seq: AtomicU64::new(0),
        }
    }

    /// Publish the latest authoritative ACTIVE `Playback` + mode (single writer:
    /// `transport`).
    pub fn update(&self, pb: &rudis_core::Playback, is_source: bool) {
        self.position_us.store(pb.position_us, Ordering::Relaxed);
        self.playing.store(pb.playing, Ordering::Relaxed);
        self.duration_us.store(pb.duration_us, Ordering::Relaxed);
        self.is_source.store(is_source, Ordering::Relaxed);
    }
}

impl Default for PlaybackMirror {
    fn default() -> Self {
        Self::new()
    }
}

/// Phase 57 (plan 57-08, PLAY-05/D-01): the ENGINE→shell diagnostic mirror.
///
/// [`PlaybackMirror`] above is the same idea pointed the other way: the shell
/// writes it (`transport()`, single writer) and the engine reads it every tick.
/// PLAY-05 needs the reverse — a value the ENGINE computes and the shell reads —
/// so it is a new, small struct rather than a field on the mirror. Mixing the
/// two directions in one struct would put two writers on one object and lose the
/// single-writer property that makes `Relaxed` ordering obviously correct there.
///
/// # Why an atomic and not an event
///
/// This is the exact shape `rudis_get_playback_position`
/// (`crates/ffi/src/commands.rs:583`) already established for playback position,
/// and its doc block is the justification, quoted rather than re-derived: *"this
/// path needs NO event mechanism at all — no envelope, no allocation, no JSON,
/// just the `i64`."* A resolution level is a single small scalar polled by a UI
/// at frame rate; an event envelope per change would be strictly more machinery
/// for strictly less. D-01 is satisfied by using the EXISTING envelope pattern
/// rather than by inventing a region.
///
/// # Nothing in v8 renders it
///
/// D-01 says v8 adds nothing to either shell, so the getter existing and being
/// tested IS the observable this phase ships. Rendering a "1/2" badge in the
/// Transport region is post-cutover shell work and is explicitly out of scope.
///
/// # D-11
///
/// This carries a *transient property of a running preview session*. It is not
/// project state: it has no serialization derives, it never enters a `Command`
/// or a `Patch`, and it is reconstructed from nothing every time a producer
/// starts. Killing the process loses it, which is correct.
pub struct EngineDiag {
    /// The active [`dynres::ResLevel`] as its `repr(u8)` discriminant —
    /// `0 = Full`, `1 = Half`, `2 = Quarter`.
    ///
    /// Written ONLY by the producer thread's multi-layer arm (and by its
    /// pause/flush snap-back), read by `rudis_get_playback_resolution_level`
    /// and by tests. `Relaxed` for the same reason [`PlaybackMirror`] uses it: a
    /// one-tick-stale diagnostic is harmless, and nothing branches on it.
    pub playback_res_level: AtomicU8,
}

impl EngineDiag {
    pub fn new() -> Self {
        Self {
            playback_res_level: AtomicU8::new(dynres::ResLevel::Full as u8),
        }
    }

    /// Publish the active level (producer-side single writer).
    pub fn set_playback_res_level(&self, level: dynres::ResLevel) {
        self.playback_res_level
            .store(level as u8, Ordering::Relaxed);
    }

    /// Read the active level back, saturating an unknown discriminant to
    /// [`dynres::ResLevel::Full`] — an out-of-range byte can only come from a
    /// future writer, and "assume nothing is degraded" is the safe reading.
    pub fn playback_res_level(&self) -> dynres::ResLevel {
        match self.playback_res_level.load(Ordering::Relaxed) {
            1 => dynres::ResLevel::Half,
            2 => dynres::ResLevel::Quarter,
            _ => dynres::ResLevel::Full,
        }
    }
}

impl Default for EngineDiag {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide [`EngineDiag`] that [`PreviewHost::engine_diag`]'s DEFAULT
/// body answers with.
///
/// Every host that does not carry its own instance — the Tauri adapter, and
/// every test double in the tree — writes here. That keeps the port addition
/// free for existing implementors (the D-46-08-01 exception: an ADDITION with a
/// default body, the same shape `note_presented_stamp` and
/// `supports_composited_present` already use), which is what makes D-01 hold
/// without editing a single shell file.
///
/// A host that wants a PER-INSTANCE observable overrides the method —
/// `crates/ffi`'s `ShellPreviewHost` does, so `rudis_get_playback_resolution_level`
/// answers for THAT ctx's producer and two ctxs never read each other's level
/// (the D-07 independence property).
static PROCESS_ENGINE_DIAG: std::sync::OnceLock<EngineDiag> = std::sync::OnceLock::new();

/// The process-wide [`EngineDiag`] — see [`PROCESS_ENGINE_DIAG`].
pub fn process_engine_diag() -> &'static EngineDiag {
    PROCESS_ENGINE_DIAG.get_or_init(EngineDiag::new)
}
