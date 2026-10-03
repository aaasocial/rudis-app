//! The C# shell's preview-surface state (Phase 51, plan 51-02, task 2).
//!
//! Twins `src-tauri/src/native_surface.rs`'s `NativePreview` (the heavy GPU
//! set) and `PreviewGeometry` (the lock-free resize scalars), collapsed into
//! ONE type because this host has no managed-state registry to keep the two
//! apart — the ctx owns exactly one of these and hands `Arc` clones out.
//!
//! Nothing here creates a GPU object: plan 51-03 owns the `wgpu::Instance`
//! construction, the `ISwapChainPanelNative` QueryInterface and the
//! attach/resize/detach exports. This file is the SHAPE those exports fill and
//! the present thread reads, which is what keeps the COM/threading risk
//! isolated in one plan.
#![allow(
    dead_code,
    reason = "plan 51-03 is the writer: it constructs PreviewSurfaceState, \
              fills the GPU set from rudis_preview_attach_panel, publishes \
              target_*/scale from rudis_preview_resize, records attach_thread \
              for the D-07 affinity guard and latches present_started. Until \
              then only sink.rs (task 2) and host.rs (task 3) READ these, so \
              the compiler cannot see a production writer for several fields."
)]

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64};
use std::sync::Mutex;

/// The heavy GPU set plus the lock-free geometry the present thread reads
/// every tick.
///
/// `Option` on every GPU field is LOAD-BEARING, not defensive: before attach
/// (and after detach) there genuinely is no surface, and every `PresentSink`
/// method must then degrade to the same silent no-op `TauriPresentSink`
/// performs under the mock runtime. It is also the shape WR-01's device-lost
/// recovery needs — the dead set must be dropped BEFORE a new adapter is
/// requested, or DXGI hides the hardware adapter and the rebuild lands on WARP.
pub(crate) struct PreviewSurfaceState {
    /// The GPU set, behind the ONE lock the present thread takes per tick.
    pub(crate) gpu: Mutex<PreviewGpu>,
    /// Set by `rudis_preview_resize` (51-03); consumed by the present thread's
    /// [`crate::panel::sink::ShellPresentSink::reconfigure_if_dirty`].
    /// Lock-free so a resize never waits on a composite — the same
    /// `surface_dirty` + `target_w`/`target_h` triple `PreviewGeometry` uses,
    /// and (per 51-RESEARCH's read of `wgpu-hal-26.0.6/src/dx12/mod.rs`) the
    /// reason resize needs no UI-thread affinity: `SetSwapChain` fires only on
    /// the FIRST configure, and every later one takes the `ResizeBuffers`
    /// branch.
    pub(crate) surface_dirty: AtomicBool,
    pub(crate) target_w: AtomicU32,
    pub(crate) target_h: AtomicU32,
    /// The panel's composition scale, published by C# alongside the size
    /// (D-10: `CompositionScaleX/Y` and `XamlRoot.RasterizationScale` are
    /// WinUI-native signals Rust cannot query, so physical pixels AND the
    /// scale cross the ABI and there is no second source of truth). Stored as
    /// bits so it can live in an atomic: `f32::to_bits` / `f32::from_bits`.
    pub(crate) target_scale_bits: AtomicU32,
    /// The contain-fit CONTENT rect inside the panel, in physical px:
    /// published by the present path on every composite, read lock-free by
    /// `rudis_preview_content_rect` (51-03).
    ///
    /// This is the capability the retired viewport PUSH provided; here it is
    /// PULLED on demand, matching the hot-scalar-poll idiom
    /// (`v7-ARCHITECTURE.md:101-102`) — under `SwapChainPanel` the ink overlay
    /// is an ordinary XAML sibling that can simply ask.
    pub(crate) content_x: AtomicI32,
    pub(crate) content_y: AtomicI32,
    pub(crate) content_w: AtomicU32,
    pub(crate) content_h: AtomicU32,
    /// True once a panel has been attached; false again after detach.
    pub(crate) attached: AtomicBool,
    /// The managed thread that attached, for the D-07 affinity guard
    /// (`ISwapChainPanelNative::SetSwapChain` returns `RPC_E_WRONG_THREAD` off
    /// the panel's UI thread). Plan 51-03 fills it; it lives here so the guard
    /// is pure and unit-testable.
    pub(crate) attach_thread: Mutex<Option<std::thread::ThreadId>>,
    /// The present thread is spawned exactly ONCE per ctx, at first attach.
    pub(crate) present_started: AtomicBool,

    // ── Phase 63, plan 63-02 (TRUST-01): the device-lost/recovery observables ──
    //
    // ALL FIVE ARE PLAIN ATOMICS, DELIBERATELY. `rudis_preview_device_status`
    // is polled by the shell's Preview region on a cold cadence and must never
    // take the GPU lock (threat T-63-06: the present thread owns that lock for
    // the whole of a composite, and a UI-thread poll that queued behind one
    // would stall the shell on the very tick a TDR is being handled). Reading
    // five relaxed atomics is the same cost model `rudis_preview_content_rect`
    // already established in this file.
    /// Monotonic count of SUCCESSFUL surface presents, incremented by every
    /// [`crate::panel::sink::ShellPresentSink`] composite path that actually
    /// reached the swapchain.
    ///
    /// This is the observable that makes "a frame presents again after the
    /// loss" a readable VALUE rather than an inference: it FREEZES the instant
    /// presenting stops and RESUMES climbing the instant it works again, which
    /// is exactly the before/after a paying customer experiences.
    pub(crate) presented: AtomicU64,
    /// Incremented once per SUCCESSFUL attach (first attach = 1, and again on
    /// every recovery re-attach). The epoch a caller can compare across a
    /// recovery to know the surface it is looking at is a NEW one.
    pub(crate) attach_epoch: AtomicU64,
    /// Incremented once per COMPLETED coordinated recovery.
    pub(crate) recovered: AtomicU64,
    /// Latched `true` the moment a device loss is OBSERVED on the live preview
    /// device — either through the compositor's own birth-site detection
    /// (`engine::Compositor::device_lost_pending`, armed at the one
    /// `request_device`) or through a device-loss-class present error. Cleared
    /// only by a completed recovery.
    pub(crate) device_lost: AtomicBool,
    /// Mirrors `engine::RecoveryPlan::recovering_flag` for the duration of a
    /// recovery, so a poll landing mid-sequence reads `recovering = true` and
    /// the shell does not start a second one (threat T-63-05).
    ///
    /// A SEPARATE atomic rather than a clone of the plan's flag because the
    /// plan is built, driven and dropped inside one export call: there is no
    /// long-lived plan to hold a handle to, and a flag that outlived its plan
    /// would be a lie. The single-entry guarantee itself is still the ENGINE's
    /// (`RecoveryPlan::recover` refuses a competing entry); this is the
    /// shell-visible twin `RecoveryPlan::recovering_flag`'s own doc anticipates
    /// ("the shell keeps its own managed twin").
    pub(crate) recovering: AtomicBool,
}

impl Default for PreviewSurfaceState {
    fn default() -> Self {
        Self {
            gpu: Mutex::new(PreviewGpu::default()),
            surface_dirty: AtomicBool::new(false),
            target_w: AtomicU32::new(0),
            target_h: AtomicU32::new(0),
            // 1.0 — the identity composition scale, so a host that never
            // reports one is not silently scaled to nothing.
            target_scale_bits: AtomicU32::new(1.0f32.to_bits()),
            content_x: AtomicI32::new(0),
            content_y: AtomicI32::new(0),
            content_w: AtomicU32::new(0),
            content_h: AtomicU32::new(0),
            attached: AtomicBool::new(false),
            attach_thread: Mutex::new(None),
            present_started: AtomicBool::new(false),
            presented: AtomicU64::new(0),
            attach_epoch: AtomicU64::new(0),
            recovered: AtomicU64::new(0),
            device_lost: AtomicBool::new(false),
            recovering: AtomicBool::new(false),
        }
    }
}

impl PreviewSurfaceState {
    /// The composition scale C# last published, or `1.0`.
    pub(crate) fn target_scale(&self) -> f32 {
        f32::from_bits(
            self.target_scale_bits
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Publish the contain-fit content rect computed by a composite. `Relaxed`
    /// throughout: the four values are a HINT for the ink overlay's geometry,
    /// never a correctness input to the composite itself, so a torn read costs
    /// at most one frame of overlay misalignment and never a wrong picture.
    pub(crate) fn publish_content_rect(&self, rect: [f32; 4]) {
        use std::sync::atomic::Ordering::Relaxed;
        self.content_x.store(rect[0].round() as i32, Relaxed);
        self.content_y.store(rect[1].round() as i32, Relaxed);
        self.content_w.store(rect[2].round().max(0.0) as u32, Relaxed);
        self.content_h.store(rect[3].round().max(0.0) as u32, Relaxed);
    }

    /// The last published content rect, `(x, y, w, h)` in physical px.
    pub(crate) fn content_rect(&self) -> (i32, i32, u32, u32) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.content_x.load(Relaxed),
            self.content_y.load(Relaxed),
            self.content_w.load(Relaxed),
            self.content_h.load(Relaxed),
        )
    }

    /// Latch "the live preview device is gone" and say so ONCE (Phase 63,
    /// plan 63-02).
    ///
    /// Idempotent by construction — the `swap` is what makes the log line fire
    /// exactly once per loss rather than sixty times a second for the rest of
    /// the session, which is the difference between a diagnosable stderr and an
    /// unreadable one.
    pub(crate) fn mark_device_lost(&self, why: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.device_lost.swap(true, Relaxed) {
            eprintln!(
                "[rudis_ffi] panel preview: DEVICE LOST observed on the live surface ({why}). \
                 Presenting stops here; the shell's device-status poll drives the coordinated \
                 recovery from the UI thread (TRUST-01)."
            );
        }
    }

    /// One successful surface present.
    pub(crate) fn note_presented(&self) {
        self.presented
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// The whole device-status observable, read as ONE tuple so a caller
    /// cannot assemble a half-updated picture out of five separate loads at
    /// five separate moments.
    ///
    /// Still not atomic ACROSS the five (nothing short of a lock would be, and
    /// a lock is exactly what T-63-06 forbids here) — but the only field that
    /// moves at frame rate is `presented`, and a `presented` one frame stale is
    /// a poll one frame early, never a wrong answer about whether the device is
    /// lost.
    pub(crate) fn device_status(&self) -> (bool, bool, u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.device_lost.load(Relaxed),
            self.recovering.load(Relaxed),
            self.recovered.load(Relaxed),
            self.presented.load(Relaxed),
            self.attach_epoch.load(Relaxed),
        )
    }
}

/// The GPU objects themselves — everything that must be dropped together, and
/// in order, when a panel detaches or a device is lost.
pub(crate) struct PreviewGpu {
    /// Kept alive for the surface's lifetime: the surface and the
    /// compositor's adapter/device all reference this instance's internals.
    /// Never used directly, hence the underscore — the same field
    /// `NativePreview` carries for the same reason.
    pub(crate) _instance: Option<wgpu::Instance>,
    /// `Arc` so the ring producer can composite OFFSCREEN through its own
    /// clone without contending for this lock (wgpu `Device`/`Queue` are
    /// internally synchronized).
    pub(crate) compositor: Option<std::sync::Arc<engine::Compositor>>,
    pub(crate) surface: Option<wgpu::Surface<'static>>,
    /// The CLEAN frame currently shown; re-presented on every resize. Only
    /// ever assigned a pristine frame — never an ink-annotated clone (Pitfall
    /// 2 / D-46-03-02: a resize re-presents this, so storing annotated bytes
    /// would bake the ink in and double it).
    pub(crate) frame: engine::Frame,
    /// Physical size the surface is currently configured for.
    pub(crate) config_w: u32,
    pub(crate) config_h: u32,
    /// Whether the first present has happened yet (the `mark_shown_once`
    /// latch).
    pub(crate) shown: bool,
    /// The GPU-RESIDENT frame currently shown, when the last present came
    /// through `PresentSink::present_gpu`. Holds the imported texture and the
    /// refcounted `AVFrame` keeper together, so while stored its
    /// hw-frame-pool slice stays pinned. A CPU present clears it, so
    /// `current_frame()` never answers from a stale GPU handle.
    ///
    /// `cfg(windows)` only, deliberately NOT a feature predicate:
    /// `engine::GpuFrame` exists exactly when the engine's default-on
    /// `hwdecode` feature is compiled, which this crate's dependency edge
    /// guarantees; a `feature = "hwdecode"` predicate here would read THIS
    /// crate's namespace, which has no such feature. Identical reasoning to
    /// `native_surface.rs:298-301`.
    #[cfg(windows)]
    pub(crate) gpu_frame: Option<engine::GpuFrame>,
    /// The content dims of the last COMPOSITED present, when the last present
    /// came through `PresentSink::present_composited` (quick 260803-cws,
    /// PLAY-02). `None` whenever a CPU or GPU present was more recent.
    ///
    /// **The pooled target itself is NEVER stored, and that is deliberate.** A
    /// `engine::PooledTarget`'s `Drop` returns its slot to the K=4
    /// `CompositeTargetPool`, and its texture is overwritten by a later
    /// composite: pinning one would starve the producer's runway (it blocks on
    /// `checkout`), and holding only its view would re-present FUTURE pixels on
    /// the next resize repaint. So this stores what the cross-kind
    /// `dims_changed` contract actually needs — the dims — and lets the pixels
    /// go, which is what the pool is for.
    ///
    /// The visible consequence, stated rather than hidden: while a composited
    /// present is the most recent one, `ShellPresentSink::current_frame()`
    /// still answers the last stored CPU frame. That is stale for at most the
    /// PLAYING window, because pausing re-stores a fresh frame through
    /// `present_still`/`present_multilayer` before anything consumes it.
    ///
    /// NOT `cfg`-gated (unlike `gpu_frame`): `present_composited` is a
    /// platform-independent trait method, so a non-Windows build overrides it
    /// too and needs this bookkeeping just the same.
    pub(crate) composited_dims: Option<(u32, u32)>,
    /// The live GPU-03 VRAM budget watch (Phase 51, plan 51-03).
    ///
    /// Held HERE, beside the surface it belongs to, because its `Drop` is what
    /// calls `UnregisterVideoMemoryBudgetChangeNotification`: keeping only the
    /// `Arc<AtomicU64>` handle and dropping the watch would leave the
    /// producer's decode gate reading a budget frozen at its startup value —
    /// silently wrong rather than loudly absent. Detach drops it with the rest
    /// of the set, in one place, in order. `None` when the watch could not be
    /// registered, which disarms the GPU decode gate exactly as the Tauri path
    /// does.
    ///
    /// `cfg(windows)`, deliberately NOT a feature predicate — same reasoning as
    /// `gpu_frame` above: `engine::VramBudgetWatch` is gated on the ENGINE's
    /// default-on `hwdecode` feature, which this crate's dependency edge
    /// guarantees, and a `feature = "hwdecode"` predicate here would read THIS
    /// crate's namespace, which has no such feature.
    #[cfg(windows)]
    pub(crate) vram_watch: Option<engine::VramBudgetWatch>,
}

impl Default for PreviewGpu {
    fn default() -> Self {
        Self {
            _instance: None,
            compositor: None,
            surface: None,
            // The 1x1 seed `native_surface` uses before any media loads: a
            // real `Frame`, so every `dims_changed` compare and every
            // `current_frame()` answer is total from the very first tick.
            frame: preview::placeholder_frame(1, 1),
            config_w: 0,
            config_h: 0,
            shown: false,
            composited_dims: None,
            #[cfg(windows)]
            gpu_frame: None,
            #[cfg(windows)]
            vram_watch: None,
        }
    }
}
