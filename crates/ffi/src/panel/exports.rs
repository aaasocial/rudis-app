//! The four `SwapChainPanel` exports (Phase 51, plan 51-03, task 3):
//! `rudis_preview_attach_panel`, `rudis_preview_resize`,
//! `rudis_preview_content_rect`, `rudis_preview_detach_panel`.
//!
//! # Why four exports and not one
//!
//! Their THREADING requirements genuinely differ, and collapsing them would
//! force the strictest rule onto all of them (51-RESEARCH Open Question 2):
//!
//! | export | thread rule | cost model |
//! |---|---|---|
//! | attach | the panel's UI thread, no exceptions | one COM-heavy call per panel lifetime |
//! | resize | any thread | three atomic stores + a best-effort paused re-present (try_lock PROBE — never blocks; skipped whenever the present thread owns the GPU set) |
//! | content_rect | any thread | four relaxed atomic loads, no allocation |
//! | detach | the attaching thread (conservative — see [`check_affinity`]) | one lock, teardown only |
//!
//! A single `rudis_preview_configure(panel_or_null, ..)` would either put
//! resize on the UI thread (latency on every DPI change) or take attach off it
//! (`RPC_E_WRONG_THREAD` on every attach). Four symbols is the cheaper trade.
//!
//! # Every body is exactly one guard-macro invocation (D-08)
//!
//! Written LITERALLY at module level, never macro-emitted, because `cbindgen`
//! parses with `syn` and cannot see macro-generated items on the pinned stable
//! toolchain (`export.rs`'s DESIGN NOTE). The guard's expansion is where
//! `catch_unwind` AND the `FFI_EXPORTS` registration come from — inseparably —
//! so an export cannot be written without a panic guard (threat T-51-03), and
//! `tests/export_table.rs`'s set-equality catches any attempt in both
//! directions.
//!
//! That inseparability is what makes the panic property (T-51-03) provable
//! without a GPU: `lib.rs`'s `ffi_guard_maps_a_real_panic_to_the_panic_ret`
//! proves the macro converts a REAL unwind into its `$panic_ret` on an actual
//! panic, and [`tests::all_four_panel_exports_are_registered_and_therefore_guarded`]
//! plus the DLL export-table equality prove all four bodies here are that
//! macro, with `RudisStatus::PanicCaught` as the return.
//!
//! # No envelope, therefore named statuses
//!
//! These four carry no `RudisBuffer`, so there is nowhere for a `{"Err": ".."}`
//! to ride. Their real failure modes are named in
//! [`RudisStatus`](crate::RudisStatus) instead (`-5..=-9`), which is a
//! deliberate widening of the transport-fault vocabulary rather than an
//! overload of `InvalidHandle`. The V5 null-check-before-dereference discipline
//! from `commands.rs` still applies to every raw pointer (threat T-51-05).

use crate::panel::state::PreviewGpu;
use crate::panel::surface::{attach_gpu, query_swap_chain_panel_native};
use crate::{RudisCtx, RudisPreviewRect, RudisStatus};
use std::ffi::c_void;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::Arc;

/// D-07: the attaching thread is recorded at attach, and every PANEL-AFFINE
/// call must arrive from it.
///
/// Which calls are panel-affine was decided by READING
/// `wgpu-hal-26.0.6/src/dx12/mod.rs`, not by generalising the Phase-44 spike
/// (which never resizes and so says nothing about it):
///
/// * **attach — YES.** The first `configure()` creates the composition
///   swapchain and calls `ISwapChainPanelNative::SetSwapChain` (`:1348-1355`),
///   which returns `RPC_E_WRONG_THREAD` off the panel's UI thread.
/// * **detach — YES, conservatively.** Dropping the surface releases the
///   composition swapchain bound to the panel, and this crate has NOT verified
///   that path to be apartment-agnostic (51-RESEARCH Assumptions Log A2).
///   Requiring the same thread costs nothing — C# detaches from `Unloaded`,
///   the same UI thread as `Loaded` — and removes an unproven assumption.
/// * **resize — NO.** The second and later `configure()` calls take the
///   `ResizeBuffers` branch (`:1254-1272`), which never reaches `SetSwapChain`
///   and has no COM thread rule.
/// * **content_rect — NO.** Four relaxed atomic loads.
///
/// Pure and total, so the rule is unit-testable without a panel, a window or a
/// GPU.
pub(crate) fn check_affinity(
    recorded: Option<std::thread::ThreadId>,
    current: std::thread::ThreadId,
) -> Result<(), RudisStatus> {
    match recorded {
        None => Err(RudisStatus::NotAttached),
        Some(owner) if owner != current => Err(RudisStatus::WrongThread),
        Some(_) => Ok(()),
    }
}

/// Read the recorded attaching thread, recovering from a poisoned lock.
///
/// The protected value is a plain `Option<ThreadId>` — a panic elsewhere
/// cannot leave it logically inconsistent, only flagged — so refusing to read
/// it after a poisoning would turn a recoverable state into a permanently
/// undetachable panel. Recover, and say so here rather than at the call site.
fn recorded_attach_thread(state: &super::state::PreviewSurfaceState) -> Option<std::thread::ThreadId> {
    match state.attach_thread.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// [`check_affinity`] over a whole [`super::state::PreviewSurfaceState`], for
/// the panel-affine export that lives in a sibling module
/// ([`super::recovery::recover_device`], plan 63-02).
///
/// A thin pass-through rather than a re-implementation, so there is exactly ONE
/// affinity rule in this crate and D-07's truth table keeps covering all of it.
pub(crate) fn check_affinity_public(
    state: &super::state::PreviewSurfaceState,
    current: std::thread::ThreadId,
) -> Result<(), RudisStatus> {
    check_affinity(recorded_attach_thread(state), current)
}

/// Attach a WinUI 3 `SwapChainPanel` and start presenting into it.
///
/// `panel` is a COM pointer to the panel. An `IInspectable*` — what
/// `WinRT.MarshalInspectable<object>.FromManaged` yields — is fine: this
/// function `QueryInterface`s it for `ISwapChainPanelNative` itself, so a
/// wrong pointer is `NotASwapChainPanel`, never undefined behaviour. The
/// CALLER keeps ownership of its own reference and must release it; this
/// function takes its own.
///
/// `width_px`/`height_px` are the panel's size in PHYSICAL pixels and `scale`
/// its composition scale — WinUI-native signals Rust cannot query, so the
/// caller sends both and Rust keeps one source of truth. All three are clamped
/// (`1..=16384`, finite positive scale) before any GPU call: a 0-px or absurd
/// swapchain is a driver allocation failure or a hard OOM, not a graceful
/// error.
///
/// A panel must be detached before another is attached; re-attaching over a
/// live surface answers `AlreadyAttached`.
///
/// ⚠ MUST be called on the UI thread that owns `panel`. `wgpu`'s
/// `Surface::configure` calls `ISwapChainPanelNative::SetSwapChain`, which
/// returns `RPC_E_WRONG_THREAD` off that thread. Call it from the panel's
/// `Loaded` handler, synchronously — never from a background interop worker
/// (a serialized interop queue is the WRONG thread by construction).
///
/// Returns `Ok`, or: `InvalidHandle` (null ctx) · `NullPointer` (null panel) ·
/// `AlreadyAttached` · `NotASwapChainPanel` · `SurfaceCreateFailed` ·
/// `PanicCaught`.
#[no_mangle]
pub extern "C" fn rudis_preview_attach_panel(
    ctx: *mut RudisCtx,
    panel: *mut c_void,
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> RudisStatus {
    crate::ffi_guard!("rudis_preview_attach_panel", RudisStatus::PanicCaught, {
        if ctx.is_null() {
            return RudisStatus::InvalidHandle;
        }
        if panel.is_null() {
            return RudisStatus::NullPointer;
        }
        // SAFETY: non-null by the check above; by the ABI contract this is a
        // live pointer from `rudis_init` that has not been shut down.
        let ctx_ref = unsafe { &*ctx };
        let state = Arc::clone(&ctx_ref.preview_surface);

        // `Acquire` pairs with the `Release` store at the end of this function,
        // so a second attach observing `true` also observes the filled GPU set.
        if state.attached.load(Acquire) {
            return RudisStatus::AlreadyAttached;
        }

        // T-51-06: clamp BEFORE anything reaches the driver.
        let w = width_px.clamp(1, 16_384);
        let h = height_px.clamp(1, 16_384);
        let s = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };

        // T-51-01: never trust the caller's claimed pointer type.
        let panel_native = match query_swap_chain_panel_native(panel) {
            Ok(native) => native,
            Err(_) => return RudisStatus::NotASwapChainPanel,
        };

        // The adapter request rides this crate's EXISTING per-instance Tokio
        // runtime (`AppCtx::block_on`) rather than a second executor.
        let app = crate::ctx::FfiAppCtx::new(ctx_ref);
        let request_adapter = |instance: &wgpu::Instance, surface: &wgpu::Surface<'static>| {
            use app_core::AppCtx as _;
            app.block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(surface),
            }))
            .map_err(|e| format!("request_adapter: {e}"))
        };

        // ⚠ The UI-thread-affine step lives inside here (the FIRST configure).
        let attached = match attach_gpu(panel_native, w, h, s, &request_adapter) {
            Ok(gpu) => gpu,
            Err(e) => {
                eprintln!("[rudis_ffi] rudis_preview_attach_panel failed: {e}");
                return RudisStatus::SurfaceCreateFailed;
            }
        };

        let gpu_budget = attached.gpu_budget.clone();

        // Publish the geometry the present thread reconfigures against BEFORE
        // the surface goes live, so its first tick cannot read a zeroed target.
        state.target_w.store(w, Relaxed);
        state.target_h.store(h, Relaxed);
        state.target_scale_bits.store(s.to_bits(), Relaxed);

        {
            let mut slot = match state.gpu.lock() {
                Ok(slot) => slot,
                // A poisoned GPU lock here means a previous present unwound.
                // The set it protects is being wholly REPLACED, so recovering
                // is sound and refusing would make the panel unattachable for
                // the rest of the ctx's life.
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = PreviewGpu {
                _instance: Some(attached.instance),
                compositor: Some(Arc::clone(&attached.compositor)),
                surface: Some(attached.surface),
                frame: attached.frame,
                config_w: w,
                config_h: h,
                // The placeholder composite in `attach_gpu` HAS happened, so
                // the surface genuinely holds a picture already.
                shown: true,
                // ...and it was a CPU composite of `attached.frame`, not a
                // composited target, so the cross-kind `dims_changed` contract
                // must start from that frame's dims (quick 260803-cws).
                composited_dims: None,
                #[cfg(windows)]
                gpu_frame: None,
                #[cfg(windows)]
                vram_watch: attached.vram_watch,
            };
        }

        match state.attach_thread.lock() {
            Ok(mut guard) => *guard = Some(std::thread::current().id()),
            Err(poisoned) => *poisoned.into_inner() = Some(std::thread::current().id()),
        }
        // Plan 63-02: the attach epoch a caller compares across a device-lost
        // recovery to know the surface it is looking at is a NEW one. Stored
        // BEFORE `attached` goes true, so an `Acquire` observer that sees the
        // panel live also sees the epoch that describes it.
        state.attach_epoch.fetch_add(1, Relaxed);
        // Plan 63-02 / threat T-63-04: latch the forced-loss debug gate HERE,
        // at first attach -- i.e. during app startup, long before any user
        // action -- so `rudis_preview_simulate_device_lost`'s answer is fixed
        // by the environment the process was STARTED with and cannot be changed
        // by anything the process does later.
        let _ = debug_device_loss_enabled();
        // `Release`: everything above is visible to any thread that observes
        // `attached == true` with `Acquire`.
        state.attached.store(true, Release);

        // Spawn the present thread EXACTLY once per ctx. A detach/re-attach
        // cycle reuses it — `present_loop_with_gpu_budget` has no stop signal
        // and is frozen, and every `PresentSink` method degrades to a
        // documented no-op while nothing is attached.
        if !state.present_started.swap(true, AcqRel) {
            let host: Arc<dyn preview::PreviewHost> = Arc::new(super::host::ShellPreviewHost::new(
                Arc::clone(&ctx_ref.store),
                Arc::clone(&ctx_ref.mirror),
                Arc::clone(&ctx_ref.overlay),
                Arc::clone(&ctx_ref.diag),
            ));
            let sink: Arc<dyn preview::PresentSink> =
                Arc::new(super::sink::ShellPresentSink::new(Arc::clone(&state)));
            let present_ctx = preview::PresentContext::new(host, sink);
            let edit_seq = Arc::clone(&ctx_ref.edit_seq);

            // Only `Arc` clones cross into the closure — never the ctx pointer
            // (T-50-01). `Send + Sync + 'static` is therefore compiler-proven
            // rather than asserted, and no `unsafe impl` exists anywhere here.
            let spawned = std::thread::Builder::new()
                .name("rudis-preview-present".into())
                .spawn(move || {
                    preview::present_loop_with_gpu_budget(present_ctx, edit_seq, gpu_budget)
                });
            let handle = match spawned {
                Ok(handle) => handle,
                Err(e) => {
                    eprintln!("[rudis_ffi] spawn rudis-preview-present failed: {e}");
                    // Un-latch so a later attach can try again — a failed
                    // spawn must not silently disable presenting forever.
                    state.present_started.store(false, Release);
                    return RudisStatus::SurfaceCreateFailed;
                }
            };

            // The watchdog `native_surface.rs:1254-1270` established: the
            // present thread is meant to live for the whole process, and
            // `present_loop_with_gpu_budget` NEVER returns normally, so any
            // return at all is a defect. Without this, a panic there leaves a
            // state indistinguishable at the UI level from "no frame due" —
            // the transport keeps advancing while the surface holds its last
            // frame forever, with zero further log lines.
            let _ = std::thread::Builder::new()
                .name("rudis-preview-watchdog".into())
                .spawn(move || {
                    let outcome = match handle.join() {
                        Ok(()) => "EXITED NORMALLY (it never should)".to_string(),
                        Err(payload) => format!(
                            "PANICKED: {}",
                            crate::export::panic_message(payload.as_ref())
                        ),
                    };
                    eprintln!(
                        "[rudis_ffi] !!! the rudis-preview-present thread ENDED: {outcome}. \
                         The preview surface will hold its last frame forever while the \
                         transport keeps advancing. This is always a defect."
                    );
                });
        }

        RudisStatus::Ok
    })
}

/// Publish a new panel size — and, best-effort, consume it immediately.
///
/// Callable from ANY thread. The publish half is lock-free: it stores three
/// scalars and sets a dirty flag the present paths' `reconfigure_if_dirty`
/// consumes. Rust owns the resize; the caller only forwards the notification.
/// The second and later `Surface::configure` calls take `ResizeBuffers`, which
/// has no COM apartment rule.
///
/// The consume half ([`resize_reconfigure_and_present`]) is a try_lock PROBE
/// that NEVER blocks the caller: during playback the present thread owns the
/// GPU set and its own inline reconfigure already covers the resize, so a held
/// lock is a skip — the pre-fix behaviour, verbatim. It exists because a PAUSED
/// transport presents nothing, so before it the dirty flag had NO consumer
/// until the next scrub/edit/play and the swapchain kept its old dimensions
/// (debug session `preview-swapchain-not-reconfigured-on-window-resize`,
/// measured live 2026-07-31).
///
/// `width_px`/`height_px` are PHYSICAL pixels; `scale` is the panel's
/// composition scale. All three are clamped exactly as at attach.
///
/// Returns `Ok`, or: `InvalidHandle` (null ctx) · `NotAttached` ·
/// `PanicCaught`.
#[no_mangle]
pub extern "C" fn rudis_preview_resize(
    ctx: *mut RudisCtx,
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> RudisStatus {
    crate::ffi_guard!("rudis_preview_resize", RudisStatus::PanicCaught, {
        if ctx.is_null() {
            return RudisStatus::InvalidHandle;
        }
        // SAFETY: same contract as `rudis_preview_attach_panel`'s deref.
        let ctx_ref = unsafe { &*ctx };
        let state = &ctx_ref.preview_surface;
        if !state.attached.load(Acquire) {
            return RudisStatus::NotAttached;
        }

        // T-51-06: the same clamp as attach, at the other entry point.
        let w = width_px.clamp(1, 16_384);
        let h = height_px.clamp(1, 16_384);
        let s = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };

        state.target_w.store(w, Relaxed);
        state.target_h.store(h, Relaxed);
        state.target_scale_bits.store(s.to_bits(), Relaxed);
        // LAST, and with `Release`: the present thread swaps this flag and
        // then reads the three scalars above, so publishing the flag first
        // would let it reconfigure to stale dimensions.
        state.surface_dirty.store(true, Release);

        // AFTER the publish, so the re-present path reads the very geometry
        // this call just stored. Best-effort by construction: playing skips on
        // the probe (the present thread consumes the flag inline, as before),
        // and a missing GPU set skips WITHOUT consuming the flag.
        resize_reconfigure_and_present(ctx_ref);

        RudisStatus::Ok
    })
}

/// The shell twin of the Tauri host's main-thread `reconfigure_and_present`
/// (`src-tauri/src/native_surface.rs:441-459`) — the missing half of the D-09
/// resize story. Restored by debug session
/// `preview-swapchain-not-reconfigured-on-window-resize` (2026-07-31).
///
/// # Why the dirty flag alone was not enough
///
/// `surface_dirty`'s only consumers are the present paths
/// (`preview::present_frame`, and the GPU ring branch of the present loop),
/// which run only when a frame is presented. While the transport is PAUSED at
/// an unchanged playhead, the present loop's `paused_represent_needed` never
/// fires — a resize is not one of its five inputs — and the overlay repaint
/// does not reconfigure. So a paused resize left the swapchain at its old
/// dimensions until the next scrub/edit/play incidentally re-presented.
/// Measured live at the pre-fix build (`PreviewResizeReconfigureTests`): the C#
/// chain published `3214x435 -> 2854x275` while the engine's content rect
/// stayed frozen at `(1220,0 773x435)` for the full settle window; the PLAYING
/// twin of the same measurement passed, because `present_frame` reconfigures
/// inline per presented frame. The Tauri host never had this gap: its
/// `reposition` runs `reconfigure_and_present` on every `Moved`/`Resized`
/// event. The `ShellPresentSink` port dropped that path with the note "under
/// `SwapChainPanel` nothing moves a child window" (`sink.rs`) — true for MOVE,
/// but the reposition path also carried the RESIZE re-present duty.
///
/// # The Tauri-parity shape, kept deliberately
///
/// * **try_lock PROBE, never a blocking lock.** This runs on the CALLER's
///   thread (in practice WinUI's UI thread, via `Preview.PublishSize`), and
///   during playback the present thread holds the GPU lock almost
///   continuously — a held lock means SKIP, exactly as the Tauri twin's
///   `try_lock` does, and the present thread's inline reconfigure covers that
///   case as it always has. Winning the probe and then losing the real lock to
///   the present thread is possible but bounded by one composite — the same
///   accepted race the Tauri twin documents.
/// * **No live GPU set → skip WITHOUT consuming the dirty flag.** Pre-attach
///   and WR-01's release→rebuild window must keep the flag as their retry
///   signal (recovery re-marks it; the present thread retries it), and eating
///   it here would also log one spurious device-lost error per resize event.
///   Pinned by `a_resize_with_no_live_gpu_set_leaves_the_dirty_flag_intact`.
/// * **The re-present goes through `preview::present_frame`** — the one frozen
///   seam every present branch funnels through (CALLED, never edited): it
///   consumes the dirty flag, reconfigures to the just-published target,
///   re-applies the composition-scale inverse transform, composites the STORED
///   pristine frame plus the current ink overlay, and republishes the content
///   rect. `current_frame` only ever answers the pristine frame (Pitfall 2 /
///   D-46-03-02), so a resize can never double ink; a GPU-resident stored
///   frame takes its documented paused-only readback.
///
/// Per-call construction of the host/sink pair is the Tauri precedent too
/// (`reposition` builds `present_context(app)` per event): two `Arc`
/// allocations per GEOMETRY-CHANGED event — the C# publisher's idempotence
/// guard has already filtered the per-layout-pass storm by the time this runs.
fn resize_reconfigure_and_present(ctx_ref: &RudisCtx) {
    let state = &ctx_ref.preview_surface;
    {
        // The probe. `Err` folds together "present thread owns it" (playing —
        // skip, it reconfigures inline) and a poisoned lock (skip, never a
        // second unwind — T-51-03).
        let Ok(gpu) = state.gpu.try_lock() else {
            return;
        };
        // Nothing to present to: pre-attach, post-detach, or mid-recovery.
        // Return BEFORE anything can consume `surface_dirty` — it is the
        // recovery window's retry signal.
        if gpu.compositor.is_none() || gpu.surface.is_none() {
            return;
        }
        // The guard drops HERE: `current_frame` and `present_frame` take the
        // lock themselves, and std's Mutex is not reentrant.
    }

    let host: Arc<dyn preview::PreviewHost> = Arc::new(super::host::ShellPreviewHost::new(
        Arc::clone(&ctx_ref.store),
        Arc::clone(&ctx_ref.mirror),
        Arc::clone(&ctx_ref.overlay),
        Arc::clone(&ctx_ref.diag),
    ));
    let sink: Arc<dyn preview::PresentSink> =
        Arc::new(super::sink::ShellPresentSink::new(Arc::clone(state)));
    let present_ctx = preview::PresentContext::new(host, sink);

    // The stored frame — the pristine paused frame, or the placeholder before
    // any media has presented. `None` is a poisoned lock or a failed GPU
    // readback, both already logged where they happened; the dirty flag is
    // still set in that case and the present paths keep their retry.
    let Some(frame) = present_ctx.sink().current_frame() else {
        return;
    };
    preview::present_frame(&present_ctx, frame);
}

/// Read the frame-content sub-rect inside the panel — contain-fit, EXCLUDING
/// the letterbox bars — in physical pixels, panel-relative.
///
/// Four relaxed atomic loads; callable from any thread, zero allocation, no
/// lock — the same hot-path shape as `rudis_get_playback_position`. Written by
/// the present path on every composite, from the SAME
/// `engine::contain_fit_viewport` call the compositor letterboxes with, so the
/// ink overlay's geometry and the drawn picture cannot drift apart. Do not
/// re-derive contain-fit math in the host.
///
/// This is the capability the retired viewport push provided, re-expressed as
/// a PULL: under `SwapChainPanel` the ink overlay is an ordinary sibling in the
/// same visual tree, so it can simply ask.
///
/// `width`/`height` are `0` until a composite has actually happened — the
/// caller's cue that there is nothing to align to yet.
///
/// Returns `Ok`, or: `InvalidHandle` (null ctx) · `NullPointer` (null `out`) ·
/// `NotAttached` · `PanicCaught`. `*out` is written only on `Ok`.
#[no_mangle]
pub extern "C" fn rudis_preview_content_rect(
    ctx: *mut RudisCtx,
    out: *mut RudisPreviewRect,
) -> RudisStatus {
    crate::ffi_guard!("rudis_preview_content_rect", RudisStatus::PanicCaught, {
        if ctx.is_null() {
            return RudisStatus::InvalidHandle;
        }
        if out.is_null() {
            return RudisStatus::NullPointer;
        }
        // SAFETY: same contract as `rudis_preview_attach_panel`'s deref.
        let state = &unsafe { &*ctx }.preview_surface;
        if !state.attached.load(Acquire) {
            return RudisStatus::NotAttached;
        }
        let (x, y, width, height) = state.content_rect();
        // SAFETY: non-null by the check above; the caller hands us a writable
        // out-slot by the ABI contract.
        unsafe {
            *out = RudisPreviewRect {
                x,
                y,
                width,
                height,
            }
        };
        RudisStatus::Ok
    })
}

/// Detach the panel: stop presenting into it and release the GPU set.
///
/// ⚠ MUST be called on the SAME thread that attached. Call it from the panel's
/// `Unloaded` handler, which WinUI runs on the same UI thread as `Loaded`.
///
/// The present thread is NOT joined: `preview::present_loop_with_gpu_budget`
/// has no stop signal and is frozen, so it keeps ticking against a
/// surface-less sink, and every `PresentSink` method already degrades to a
/// documented no-op there. That is the same degradation the Tauri host
/// performs under its mock runtime, not a leak of live work — and it is what
/// makes a later re-attach cheap.
///
/// Idempotent-safe: a second detach answers `NotAttached` rather than faulting.
///
/// Returns `Ok`, or: `InvalidHandle` (null ctx) · `NotAttached` ·
/// `WrongThread` · `PanicCaught`.
#[no_mangle]
pub extern "C" fn rudis_preview_detach_panel(ctx: *mut RudisCtx) -> RudisStatus {
    crate::ffi_guard!("rudis_preview_detach_panel", RudisStatus::PanicCaught, {
        if ctx.is_null() {
            return RudisStatus::InvalidHandle;
        }
        // SAFETY: same contract as `rudis_preview_attach_panel`'s deref.
        let state = &unsafe { &*ctx }.preview_surface;

        // The affinity check subsumes the attached check: an unattached state
        // has no recorded thread, so `check_affinity` answers `NotAttached`.
        if let Err(status) = check_affinity(
            recorded_attach_thread(state),
            std::thread::current().id(),
        ) {
            return status;
        }

        // Flip the flag FIRST so a concurrent `reconfigure_if_dirty` reads
        // "not attached" (a silent skip) rather than "attached but no GPU set"
        // (a reported device-lost error) during the release window.
        state.attached.store(false, Release);

        {
            let mut slot = match state.gpu.lock() {
                Ok(slot) => slot,
                // Recover: teardown must not be the one operation a poisoned
                // lock can permanently block, or the GPU set outlives its
                // panel.
                Err(poisoned) => poisoned.into_inner(),
            };
            slot.compositor = None;
            slot.surface = None;
            // The instance drops LAST of the three, and the whole set drops
            // together — the WR-01 ordering the field comments state.
            slot._instance = None;
            slot.shown = false;
            slot.config_w = 0;
            slot.config_h = 0;
            #[cfg(windows)]
            {
                slot.gpu_frame = None;
                // Dropping the watch is what unregisters the DXGI budget
                // notification.
                slot.vram_watch = None;
            }
        }

        match state.attach_thread.lock() {
            Ok(mut guard) => *guard = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }

        RudisStatus::Ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InitConfig;

    fn in_process_ctx() -> *mut RudisCtx {
        Box::into_raw(Box::new(
            RudisCtx::new_in_process(
                InitConfig::default(),
                Box::new(agent_llm::InMemoryKeyStore::new()),
            )
            .expect("in-process ctx builds"),
        ))
    }

    /// D-07's rule as a truth table, all three arms — the mitigation for
    /// T-51-02 stated as a property rather than a comment.
    #[test]
    fn check_affinity_truth_table() {
        let here = std::thread::current().id();

        // Never attached: there is no owner to compare against.
        assert_eq!(
            check_affinity(None, here),
            Err(RudisStatus::NotAttached),
            "an unattached state must say so, not claim a thread mismatch"
        );

        // The attaching thread itself.
        assert_eq!(check_affinity(Some(here), here), Ok(()));

        // A genuinely different OS thread — a real spawn, not a fabricated id.
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("the probe thread returns its own id");
        assert_ne!(other, here, "the probe must really be another thread");
        assert_eq!(
            check_affinity(Some(here), other),
            Err(RudisStatus::WrongThread),
            "a panel-affine call from the wrong thread must be a NAMED error, never a hang"
        );
    }

    /// T-51-05: every raw pointer is null-checked BEFORE any dereference, on
    /// all four exports. A null ctx is `InvalidHandle` (the handle is wrong);
    /// a null non-handle pointer is `NullPointer` (an argument is wrong) —
    /// the same split `commands.rs` already draws.
    #[test]
    fn null_pointers_are_rejected_before_any_dereference() {
        assert_eq!(
            rudis_preview_attach_panel(std::ptr::null_mut(), 0x1 as *mut c_void, 800, 600, 1.0),
            RudisStatus::InvalidHandle
        );
        assert_eq!(
            rudis_preview_resize(std::ptr::null_mut(), 800, 600, 1.0),
            RudisStatus::InvalidHandle
        );
        assert_eq!(
            rudis_preview_content_rect(std::ptr::null_mut(), std::ptr::null_mut()),
            RudisStatus::InvalidHandle
        );
        assert_eq!(
            rudis_preview_detach_panel(std::ptr::null_mut()),
            RudisStatus::InvalidHandle
        );

        let ctx = in_process_ctx();
        // A live ctx with a null PANEL is an argument fault, not a handle one.
        assert_eq!(
            rudis_preview_attach_panel(ctx, std::ptr::null_mut(), 800, 600, 1.0),
            RudisStatus::NullPointer
        );
        // A live ctx with a null out-slot, likewise. Note this is checked
        // BEFORE the attached check, so the ordering cannot hide it.
        assert_eq!(
            rudis_preview_content_rect(ctx, std::ptr::null_mut()),
            RudisStatus::NullPointer
        );
        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// The whole unattached world, on a REAL ctx: three of the four exports
    /// answer `NotAttached` and nothing panics, allocates a surface, or hangs.
    /// This is the state every ctx is born in and returns to after detach.
    #[test]
    fn an_unattached_ctx_answers_not_attached() {
        let ctx = in_process_ctx();

        assert_eq!(rudis_preview_resize(ctx, 1920, 1080, 2.0), RudisStatus::NotAttached);
        let mut rect = RudisPreviewRect {
            x: 7,
            y: 7,
            width: 7,
            height: 7,
        };
        assert_eq!(
            rudis_preview_content_rect(ctx, &mut rect),
            RudisStatus::NotAttached
        );
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (7, 7, 7, 7),
            "a rejected read must not write the out-slot at all"
        );
        assert_eq!(rudis_preview_detach_panel(ctx), RudisStatus::NotAttached);
        // Idempotent: a second detach is the same answer, never a fault.
        assert_eq!(rudis_preview_detach_panel(ctx), RudisStatus::NotAttached);

        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// A resize on an unattached ctx must publish NOTHING — the rejection
    /// happens before the stores, so a later attach cannot inherit a stale
    /// target size or a set dirty flag from a call that was refused.
    #[test]
    fn a_rejected_resize_publishes_nothing() {
        let ctx = in_process_ctx();
        // SAFETY: `ctx` is the live box this test just created.
        let state = Arc::clone(&unsafe { &*ctx }.preview_surface);

        assert_eq!(rudis_preview_resize(ctx, 4096, 2160, 3.5), RudisStatus::NotAttached);

        assert_eq!(state.target_w.load(Relaxed), 0);
        assert_eq!(state.target_h.load(Relaxed), 0);
        assert!(
            !state.surface_dirty.load(Acquire),
            "a refused resize must never leave the present thread a dirty flag to chase"
        );
        assert_eq!(state.target_scale(), 1.0, "the identity scale is untouched");

        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// The content rect an ATTACHED panel would return is exactly what the
    /// present path published, field for field — including a NEGATIVE origin,
    /// which is the case a `uint` transcription on either side would corrupt.
    ///
    /// Driven by writing the state directly rather than attaching a real
    /// panel: the read path is four atomic loads and a struct write, and
    /// pinning it needs no GPU, no COM and no window.
    #[test]
    fn content_rect_reads_back_exactly_what_the_present_path_published() {
        let ctx = in_process_ctx();
        // SAFETY: `ctx` is the live box this test just created.
        let state = Arc::clone(&unsafe { &*ctx }.preview_surface);

        state.publish_content_rect([-12.0, 34.0, 1280.0, 720.0]);
        state.attached.store(true, Release);

        let mut rect = RudisPreviewRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert_eq!(rudis_preview_content_rect(ctx, &mut rect), RudisStatus::Ok);
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (-12, 34, 1280, 720));

        // Leave the ctx unattached so its Drop is the ordinary path.
        state.attached.store(false, Release);
        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// T-51-06, on the path a caller can actually reach: an attached ctx's
    /// resize clamps 0 and an absurd dimension into `1..=16384`, and a
    /// non-finite scale into the identity — so no 0-px or 100000-px swapchain
    /// and no NaN transform ever reaches the driver.
    #[test]
    fn resize_clamps_degenerate_geometry_before_it_can_reach_the_driver() {
        let ctx = in_process_ctx();
        // SAFETY: `ctx` is the live box this test just created.
        let state = Arc::clone(&unsafe { &*ctx }.preview_surface);
        state.attached.store(true, Release);

        assert_eq!(rudis_preview_resize(ctx, 0, 100_000, f32::NAN), RudisStatus::Ok);
        assert_eq!(state.target_w.load(Relaxed), 1, "0 px clamps UP to 1");
        assert_eq!(state.target_h.load(Relaxed), 16_384, "100000 px clamps DOWN");
        assert_eq!(state.target_scale(), 1.0, "NaN scale falls back to identity");

        assert_eq!(rudis_preview_resize(ctx, 1920, 1080, -2.0), RudisStatus::Ok);
        assert_eq!(state.target_scale(), 1.0, "a negative scale is not finite-positive");

        assert_eq!(rudis_preview_resize(ctx, 1920, 1080, 1.5), RudisStatus::Ok);
        assert_eq!(state.target_w.load(Relaxed), 1920);
        assert_eq!(state.target_h.load(Relaxed), 1080);
        assert_eq!(state.target_scale(), 1.5, "a real scale rides through untouched");
        assert!(
            state.surface_dirty.load(Acquire),
            "the present thread must be told there is something to reconfigure"
        );

        state.attached.store(false, Release);
        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// The resize-time re-present's ONE unit-provable branch (debug session
    /// `preview-swapchain-not-reconfigured-on-window-resize`): an attached ctx
    /// whose GPU set is ABSENT (pre-attach fake here; WR-01's release→rebuild
    /// window in production) must keep `surface_dirty` SET after a resize —
    /// the synchronous best-effort re-present must skip WITHOUT consuming the
    /// flag, or recovery's re-mark/retry and the present thread's inline
    /// reconfigure would both lose their signal. The GREEN half — a real
    /// reconfigure + re-present on a live GPU set — needs a panel, a window
    /// and a GPU, and is pinned where those exist:
    /// `Rudis.Shell.UiTests/PreviewResizeReconfigureTests.cs` (paused twin RED
    /// at the pre-fix build, GREEN after; playing twin GREEN at both).
    #[test]
    fn a_resize_with_no_live_gpu_set_leaves_the_dirty_flag_intact() {
        let ctx = in_process_ctx();
        // SAFETY: `ctx` is the live box this test just created.
        let state = Arc::clone(&unsafe { &*ctx }.preview_surface);
        state.attached.store(true, Release);

        assert_eq!(rudis_preview_resize(ctx, 1920, 1080, 1.25), RudisStatus::Ok);
        assert!(
            state.surface_dirty.load(Acquire),
            "with no live GPU set the resize-time re-present must SKIP without \
             consuming surface_dirty — the flag is the recovery window's (and \
             the present thread's) retry signal"
        );

        state.attached.store(false, Release);
        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// D-07 end-to-end on the real export, not just on the pure guard: a
    /// detach from a DIFFERENT OS thread than the one that "attached" answers
    /// `WrongThread` — a named error, never a hang — and leaves the state
    /// attached so the owning thread can still detach it properly.
    #[test]
    fn detach_from_the_wrong_thread_is_named_never_a_hang() {
        let ctx = in_process_ctx();
        // SAFETY: `ctx` is the live box this test just created.
        let state = Arc::clone(&unsafe { &*ctx }.preview_surface);

        // Record THIS thread as the attaching one, without a real panel.
        *state.attach_thread.lock().expect("fresh lock") = Some(std::thread::current().id());
        state.attached.store(true, Release);

        let ctx_addr = ctx as usize;
        let from_other = std::thread::spawn(move || {
            // The raw address is rebuilt inside the thread rather than moved
            // as a pointer: this test is precisely about which THREAD calls,
            // and the ctx outlives the join below.
            rudis_preview_detach_panel(ctx_addr as *mut RudisCtx)
        })
        .join()
        .expect("the probe thread returns rather than hanging");
        assert_eq!(from_other, RudisStatus::WrongThread);
        assert!(
            state.attached.load(Acquire),
            "a rejected detach must not have torn anything down"
        );

        // The owning thread still succeeds, and a second call is NotAttached.
        assert_eq!(rudis_preview_detach_panel(ctx), RudisStatus::Ok);
        assert!(!state.attached.load(Acquire));
        assert_eq!(rudis_preview_detach_panel(ctx), RudisStatus::NotAttached);

        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// FFI-02 / T-51-03 for the four NEW symbols.
    ///
    /// Registration into `FFI_EXPORTS` and the `catch_unwind` wrapper come
    /// from the SAME macro expansion and cannot be had separately (D-08), so
    /// a name's PRESENCE here is a proof that its body is guarded — there is
    /// no way to register without also being wrapped. `lib.rs`'s
    /// `ffi_guard_maps_a_real_panic_to_the_panic_ret` supplies the other half
    /// on a real unwind: that the wrapper maps a panic to its `$panic_ret`
    /// rather than crossing `extern "C"`. Together the two cover all four
    /// exports without needing a panel, a window or a GPU.
    #[test]
    fn all_four_panel_exports_are_registered_and_therefore_guarded() {
        for name in [
            "rudis_preview_attach_panel",
            "rudis_preview_resize",
            "rudis_preview_content_rect",
            "rudis_preview_detach_panel",
        ] {
            assert!(
                crate::export::FFI_EXPORTS.contains(&name),
                "`{name}` must be registered by its own guard-macro expansion — registration \
                 and catch_unwind come from the SAME expansion, so a missing entry means the \
                 panic guard is missing too, and a panic would unwind across the C ABI"
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Phase 63, plan 63-02 (TRUST-01): the device-lost/recovery trio.
//
// THREE ADDITIVE EXPORTS. Every pre-existing signature above is byte-identical
// and `ring::EVENT_NAMES` is still 6 — the shell learns of a loss by POLLING
// `rudis_preview_device_status`, which is the sixth use of the poll-don't-push
// pattern in this ABI (63-CONTEXT D-10, and the four precedents that
// `tests/export_table.rs`'s own history comment enumerates).
//
// They are NOT all one shape, for the same reason the original four are not
// (see the module header's table):
//
// | export | envelope? | thread rule |
// |---|---|---|
// | device_status         | YES `{"Ok":{..}}` | any thread — five relaxed atomic loads |
// | simulate_device_lost  | YES `{"Ok":null}` / `{"Err":".."}` | any thread — DEBUG-gated |
// | recover_device        | NO — named status | the ATTACHING thread, no exceptions |
// //
// The two envelope-carrying ones are deliberate exceptions to the module
// header's "no envelope, therefore named statuses" rule, and the reason is that
// rule's own reason inverted: `device_status` has a STRUCTURED answer (five
// fields) rather than a pass/fail, and `simulate_device_lost`'s refusal is a
// DOMAIN outcome ("the debug gate is closed") rather than a transport fault, so
// naming it in `RudisStatus` would have meant appending, to an append-only
// enum, a variant that means "you did not ask nicely".
// `rudis_get_render_cache_status` is the shape both follow (`commands.rs:361`).
// ═══════════════════════════════════════════════════════════════════════════

/// The device-status payload — counters and booleans ONLY.
///
/// Threat T-63-08, disposition ACCEPT, and this type is what makes the
/// disposition true rather than asserted: there are no paths here, no adapter
/// names, no serials and no key material, so a poll that leaked into a log or a
/// crash report discloses nothing.
#[derive(serde::Serialize)]
pub(crate) struct PreviewDeviceStatus {
    /// A loss has been OBSERVED on the live preview device and presenting has
    /// stopped. Cleared only by a completed recovery.
    lost: bool,
    /// A coordinated recovery is in flight — **or a previous one FAILED**, in
    /// which case this stays `true` for the rest of the session. That is not a
    /// bug and not sloppiness: it is `RecoveryPlan`'s own documented fail-closed
    /// convention ("a failed recovery leaves it ENGAGED"), mirrored here so the
    /// shell's `lost && !recovering` trigger can never become a teardown/attach
    /// retry storm against half-recreated state (threat T-63-05).
    recovering: bool,
    /// Completed recoveries this session.
    recovered: u64,
    /// Monotonic count of SUCCESSFUL surface presents. **This is the field that
    /// makes "a frame presents afterwards" a readable VALUE**: it freezes at the
    /// loss and resumes climbing when recovery lands.
    presented: u64,
    /// Successful attaches this session — `1` after the first `Loaded`, `2`
    /// after a recovery re-attach, and so on.
    attach_epoch: u64,
}

/// The fail-closed debug gate behind [`rudis_preview_simulate_device_lost`]
/// (threat T-63-04).
///
/// `OnceLock`, so the environment is read ONCE and a value written later in the
/// session cannot change the answer — an ungated "remove my GPU" lever must not
/// be reachable in a paid build, and a gate that re-read the environment on
/// every call would be one `SetEnvironmentVariable` away from being no gate at
/// all. [`rudis_preview_attach_panel`] forces the read at first attach, i.e.
/// during app startup and long before any user action, so the latch is in place
/// before the export it guards can be meaningfully called.
static DEBUG_DEVICE_LOSS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub(crate) fn debug_device_loss_enabled() -> bool {
    *DEBUG_DEVICE_LOSS.get_or_init(|| {
        let on = std::env::var("RUDIS_DEBUG_DEVICE_LOSS")
            .map(|v| v == "1")
            .unwrap_or(false);
        if on {
            eprintln!(
                "[rudis_ffi] RUDIS_DEBUG_DEVICE_LOSS=1 — rudis_preview_simulate_device_lost is \
                 ARMED for this process. That lever really removes the live D3D12 device."
            );
        }
        on
    })
}

/// Poll the preview device's health and the present counter.
///
/// `(ctx, out)` — the shape `rudis_get_render_cache_status` and
/// `rudis_get_current_seq` already use. Always `{"Ok": {..}}`, never
/// `{"Ok": null}` and never `{"Err": ..}`: an unattached ctx honestly answers
/// all-zeroes, because "no panel has ever been attached" is a state, not a
/// fault.
///
/// **NEVER COMPUTES AND NEVER TAKES THE GPU LOCK** (threat T-63-06). Five
/// relaxed atomic loads. The present thread owns the GPU lock for the whole of
/// a composite, and a cold-cadence UI-thread poll that queued behind one would
/// stall the shell on exactly the tick a TDR is being handled — so this reads
/// the atomics [`super::sink::ShellPresentSink`] publishes and nothing else.
///
/// POLL-ONLY BY DECISION (63-CONTEXT D-10), not by omission: `ring::EVENT_NAMES`
/// stays at 6.
#[no_mangle]
pub extern "C" fn rudis_preview_device_status(
    ctx: *mut RudisCtx,
    out: *mut crate::RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_preview_device_status", RudisStatus::PanicCaught, {
        crate::commands::call_out(ctx, out, |c| {
            // ⚠ The poll is also the DETECTOR, and that is a measured decision rather
            // than an optimisation — see `sink::probe_device_lost_from_poll` for the RED
            // run that found the present loop makes no sink call at all while the
            // transport is playing and the GPU is dead. Non-blocking `try_lock` plus one
            // relaxed atomic load: it still never computes and still cannot queue behind
            // a composite (threat T-63-06).
            super::sink::probe_device_lost_from_poll(&c.preview_surface);
            let (lost, recovering, recovered, presented, attach_epoch) =
                c.preview_surface.device_status();
            Ok::<_, String>(PreviewDeviceStatus {
                lost,
                recovering,
                recovered,
                presented,
                attach_epoch,
            })
        })
    })
}

/// **TEST/PROOF ONLY — force a REAL device removal on the LIVE preview device.**
///
/// Drives [`engine::inject_forced_device_loss`], which is the body factored
/// verbatim out of `RecoveryPlan::simulate_device_lost` (quick `260829-n96`), so
/// this export reuses the 48-03 probe-measured discipline rather than
/// re-implementing `RemoveDevice` here: every encoder is created BEFORE the
/// trigger (a post-remove `create_command_encoder` is a native, uncatchable
/// `STATUS_ACCESS_VIOLATION`) and only submit + poll — both probe-measured safe
/// — touch the device afterwards. There is exactly ONE injection body in this
/// repository and it is the engine's.
///
/// ⚠ **FAIL-CLOSED DEBUG GATE (threat T-63-04).** Refuses with a DOMAIN error
/// unless `RUDIS_DEBUG_DEVICE_LOSS=1` was in the environment when
/// [`debug_device_loss_enabled`] first latched. A shipped, paid build must not
/// carry an ungated lever that kills the user's GPU device on request; the C#
/// caller is additionally `#if DEBUG`-only, so a Release shell does not even
/// contain the call.
///
/// Callable from any thread. Takes the GPU lock for the duration — deliberately,
/// so the present thread cannot be mid-composite into the device being removed.
#[no_mangle]
pub extern "C" fn rudis_preview_simulate_device_lost(
    ctx: *mut RudisCtx,
    out: *mut crate::RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!(
        "rudis_preview_simulate_device_lost",
        RudisStatus::PanicCaught,
        {
            crate::commands::call_out(ctx, out, |c| {
                if !debug_device_loss_enabled() {
                    return Err(
                        "rudis_preview_simulate_device_lost is refused: it is a DEBUG-ONLY lever \
                         that really calls ID3D12Device5::RemoveDevice on the live preview \
                         device, and it is gated fail-closed. Set RUDIS_DEBUG_DEVICE_LOSS=1 in \
                         the environment before the process starts to arm it."
                            .to_string(),
                    );
                }
                let state = &c.preview_surface;
                if !state.attached.load(Acquire) {
                    return Err("no panel is attached, so there is no live preview device to \
                                remove"
                        .to_string());
                }
                let gpu = match state.gpu.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let Some(compositor) = gpu.compositor.as_ref() else {
                    return Err("the preview GPU set is not live (a device-lost recovery is in \
                                flight, or a previous one failed) — nothing to remove"
                        .to_string());
                };
                engine::inject_forced_device_loss(compositor.device(), compositor.queue())
                    .map_err(|e| format!("forced device removal failed: {e}"))?;
                Ok::<_, String>(())
            })
        }
    )
}

/// **Run the coordinated six-step recovery against the LIVE `SwapChainPanel`**
/// (Phase 63, plan 63-02 — TRUST-01, CONTEXT D-02/D-03).
///
/// This is the production surface half of what plan 63-01 proved headlessly. The
/// shell's Preview region polls [`rudis_preview_device_status`], sees
/// `lost && !recovering`, pauses the transport, and calls this — **on the UI
/// thread that owns the panel**, handing back a FRESH COM pointer to the same
/// `SwapChainPanel`.
///
/// # Why the whole sequence runs here, on the caller's thread
///
/// Two constraints meet, and exactly one placement satisfies both:
///
/// * **Step 4 must run on the panel's UI thread.** Recreating the surface means
///   a FIRST `Surface::configure`, which reaches
///   `ISwapChainPanelNative::SetSwapChain` and returns `RPC_E_WRONG_THREAD`
///   anywhere else (see [`super::surface::attach_gpu`] step 5).
/// * **Recovery must NOT run on wgpu's device-lost callback thread**, because
///   step 3 destroys the very device that callback belongs to (plan 63-01's
///   request/run split).
///
/// The panel's UI thread is neither the callback thread nor the present thread,
/// so it is a legal place to tear the device down AND the only legal place to
/// stand a new surface up. That is why this is a synchronous export rather than
/// something the present loop drives.
///
/// # Honest scope note: this drives `RecoveryPlan`, it does not fork it
///
/// The sequence IS `engine::RecoveryPlan::recover` — the same coordinator, the
/// same six-step order, the same single-entry fail-closed guard, the same
/// step-4 same-adapter LUID re-assert. What differs from plan 63-01's
/// `engine::PreviewRecovery` is only WHO owns the six hooks. `PreviewRecovery`
/// owns the compositor itself and is the right owner at the ENGINE tier; here
/// the compositor lives in [`super::state::PreviewGpu`], where the present
/// thread and the ring producer reach it through `Arc` clones, and
/// `PreviewRecovery`'s scoped-borrow ownership model cannot express that
/// without changing the frozen `preview::PresentSink::compositor` port.
/// `engine::RecoveryHooks`' own doc anticipates exactly this split — *"the
/// headless test wires them to a real session/compositor pair; **the shell
/// wires them to its managed state**"* — as does
/// `RecoveryPlan::recovering_flag`'s (*"the shell keeps its own managed
/// twin"*). So this is the second HOST of one coordinator, not a second
/// recovery discipline.
///
/// # The `Arc` discipline plan 63-01 warned about
///
/// While ANY handle to the removed D3D12 device lives, DXGI hides the hardware
/// adapter from fresh enumeration in this process and step 4 silently recreates
/// on WARP — where the LUID re-assert refuses it, turning a recoverable TDR into
/// a hard failure. `PreviewGpu::compositor` is an `Arc` and the ring producer
/// holds a clone, so **step 1 waits for that clone to be released** (the
/// producer exits on `ring.request_stop()`, which `present_loop` issues the
/// moment the transport is not playing — which is why the shell pauses first)
/// and refuses to proceed if it is not. Failing THERE is safe: nothing has been
/// torn down yet.
///
/// Returns `Ok`, or: `InvalidHandle` (null ctx) · `NullPointer` (null panel) ·
/// `WrongThread` / `NotAttached` (affinity) · `NotASwapChainPanel` ·
/// `SurfaceCreateFailed` (any step failed — the sequence is then latched
/// closed) · `PanicCaught`.
#[no_mangle]
pub extern "C" fn rudis_preview_recover_device(
    ctx: *mut RudisCtx,
    panel: *mut c_void,
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> RudisStatus {
    crate::ffi_guard!("rudis_preview_recover_device", RudisStatus::PanicCaught, {
        if ctx.is_null() {
            return RudisStatus::InvalidHandle;
        }
        if panel.is_null() {
            return RudisStatus::NullPointer;
        }
        // SAFETY: non-null by the checks above; by the ABI contract this is a
        // live pointer from `rudis_init` that has not been shut down — the same
        // contract `rudis_preview_attach_panel`'s deref relies on.
        let ctx_ref = unsafe { &*ctx };
        super::recovery::recover_device(ctx_ref, panel, width_px, height_px, scale)
    })
}
