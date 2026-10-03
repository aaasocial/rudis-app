//! **The production `SwapChainPanel` device-lost recovery** (Phase 63, plan
//! 63-02 — TRUST-01, CONTEXT D-02/D-03).
//!
//! Plan 63-01 gave `engine::RecoveryPlan::recover` its first production caller
//! and proved recovery headlessly, through the debug-surface twin of the live
//! path. This module is the **live path**: the six steps run against the WinUI
//! shell's real `SwapChainPanel`, on the panel's own UI thread, and the preview
//! presents again with no restart.
//!
//! # What was actually hard here, stated plainly
//!
//! 63-CONTEXT D-03 predicted it with the code open: *"the hard part is the
//! surface, not the device."* Three constraints meet, and only one arrangement
//! satisfies all three.
//!
//! 1. **Step 4 must run on the panel's UI thread.** Recreating the surface is a
//!    FIRST `Surface::configure`, which reaches
//!    `ISwapChainPanelNative::SetSwapChain` and returns `RPC_E_WRONG_THREAD`
//!    anywhere else ([`super::surface::attach_gpu`] step 5).
//! 2. **Recovery must not run on wgpu's device-lost callback thread**, because
//!    step 3 destroys the very device that callback belongs to (63-01's
//!    request/run split — the callback REQUESTS, something else RUNS).
//! 3. **Every `Arc<Compositor>` clone must be released before step 4.** While
//!    any handle to a removed D3D12 device lives, DXGI hides the hardware
//!    adapter from fresh enumeration IN THIS PROCESS and step 4 silently
//!    recreates on WARP — where step 4's own LUID re-assert refuses it, turning
//!    a recoverable TDR into a hard failure. (MEASURED; recorded on
//!    `RecoveryPlan::live_device` and repeated in 63-01's hand-off note.)
//!
//! The panel's UI thread is neither the callback thread nor the present thread,
//! so it is a legal place to tear the device down AND the only legal place to
//! stand a new surface up. Constraint 3 is why step 1 WAITS for the ring
//! producer's compositor clone to be released, and why the shell pauses the
//! transport before calling in: `present_loop` answers `!playing` with
//! `ring.request_stop()`, and the producer's exit is what drops that clone.
//!
//! # This is a second HOST of one coordinator, never a second coordinator
//!
//! The sequence is `engine::RecoveryPlan::recover` — the same six steps in the
//! same order, the same single-entry fail-closed guard, the same step-4
//! same-adapter LUID re-assert, the same step-2 pooled-D3D11VA-device release.
//! What differs from 63-01's `engine::PreviewRecovery` is only WHO owns the six
//! hooks.
//!
//! `PreviewRecovery` OWNS its compositor (`Arc<Mutex<Option<Compositor>>>`) and
//! hands out only scoped borrows, which is the right shape at the engine tier
//! and is what makes constraint 3 structural there. It cannot be the shape
//! here, and the reason is a frozen port rather than a preference:
//! `preview::PresentSink::compositor(&self) -> Option<Arc<engine::Compositor>>`
//! hands the ring producer an OWNED `Arc` that outlives the call (it is fetched
//! per producer respawn and moved into the producer thread), so the FFI's
//! compositor must be an `Arc` in [`super::state::PreviewGpu`]. Adopting into
//! `PreviewRecovery` would mean either changing that frozen trait method or
//! never being able to get the rebuilt compositor back out (`PreviewRecovery`
//! deliberately exposes no `into_compositor`).
//!
//! `engine::RecoveryHooks`' own doc anticipates this exact split — *"the
//! headless test wires them to a real session/compositor pair; **the shell
//! wires them to its managed state**"* — as does
//! `RecoveryPlan::recovering_flag`'s (*"the shell keeps its own managed
//! twin"*). So constraint 3 is enforced HERE, by hand, with a wait and a
//! refusal, and this module says so out loud rather than implying the
//! structural guarantee it does not have.

use super::state::{PreviewGpu, PreviewSurfaceState};
use crate::{RudisCtx, RudisStatus};
use std::ffi::c_void;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long step 1 waits for the ring producer to release its
/// `Arc<engine::Compositor>` clone.
///
/// The producer exits on `ring.request_stop()`, which `present_loop` issues on
/// its next tick after the transport stops playing — so the realistic wait is
/// one present tick plus however long the producer's in-flight decode takes to
/// unwind. Three seconds is generous for that and still bounded: this runs on
/// the UI thread, and an unbounded wait would trade a dead preview for a hung
/// app, which is strictly worse.
const PRODUCER_RELEASE_TIMEOUT: Duration = Duration::from_secs(3);

/// Poll cadence for the wait above. Coarse on purpose — the thing being waited
/// for is a thread exit, not a scalar.
const PRODUCER_RELEASE_POLL: Duration = Duration::from_millis(25);

/// How long the PRE-FLIGHT waits for DXGI to list a physical (non-software) adapter.
///
/// Phase 71: the pre-flight no longer asks wgpu for a DX12 adapter. That question cannot
/// be answered "yes" while the preview itself still holds the removed device (71-05
/// stage A: `hardware=false` 10/10), so it refused every recovery. It now asks DXGI
/// whether the hardware is still THERE, which creates no D3D12 device and is not hidden
/// by a removed one. A real TDR can take the adapter away for a moment, so the wait
/// stays bounded at 1.5 s on the UI thread.
const HARDWARE_ADAPTER_TIMEOUT: Duration = Duration::from_millis(1_500);

/// How long step 4 waits for wgpu to offer the hardware DX12 adapter again, AFTER the
/// last reference to the removed device has been released.
///
/// 71-05 measured the adapter coming back ~200 ms after the last release (worst case
/// 485 ms under load, quantized by the 150 ms enumeration cadence) and advised a budget
/// of at least 1 s. 3 s keeps margin and stays bounded on the UI thread.
const STEP4_ADAPTER_TIMEOUT: Duration = Duration::from_millis(3_000);

/// How long the quiesce stage waits for cancelled render-cache bakes to return (and so
/// drop their clones of the background compositor). 59-07 measured ~40 ms from cancel
/// latch to return; 3 s is generous and bounded.
const QUIESCE_ROWS_TIMEOUT: Duration = Duration::from_millis(3_000);

/// Poll cadence for the wait above.
const QUIESCE_ROWS_POLL: Duration = Duration::from_millis(50);

// ─── 71-REVIEW IN-06: the TOTAL UI-thread budget of one recovery ───────────────
//
// Every wait above is bounded, and they run in sequence on the panel's UI thread:
//
//   QUIESCE_ROWS_TIMEOUT      3.0 s  (cancelled render-cache bakes drain)
// + HARDWARE_ADAPTER_TIMEOUT  1.5 s  (DXGI pre-flight, plus one 150 ms poll overrun)
// + PRODUCER_RELEASE_TIMEOUT  3.0 s  (step 1: the ring producer drops its clone)
// + STEP4_ADAPTER_TIMEOUT     3.0 s  (step 4: the lost adapter re-enumerates)
// ≈ 10.5 s worst case, plus the recreate itself. The measured happy path is far
// shorter (71-05/71-06: rows drain in ~0-40 ms, the adapter returns ~200-485 ms after
// the last release). The worst case only occurs on a path that then refuses or
// fails, which is why each wait refuses rather than waits longer.
const _: () = assert!(
    QUIESCE_ROWS_TIMEOUT.as_millis()
        + HARDWARE_ADAPTER_TIMEOUT.as_millis()
        + PRODUCER_RELEASE_TIMEOUT.as_millis()
        + STEP4_ADAPTER_TIMEOUT.as_millis()
        <= 10_500
);

/// Non-Windows twin. `SwapChainPanel` hosting is Windows-only BY CONSTRUCTION
/// (`wgpu::SurfaceTargetUnsafe::SwapChainPanel` is `#[cfg(dx12)]`), so there is
/// no live surface to recover and saying so is the honest answer.
#[cfg(not(windows))]
pub(crate) fn recover_device(
    _ctx: &RudisCtx,
    _panel: *mut c_void,
    _width_px: u32,
    _height_px: u32,
    _scale: f32,
) -> RudisStatus {
    RudisStatus::SurfaceCreateFailed
}

/// Drive `engine::RecoveryPlan::recover` against the live panel. See
/// [`super::exports::rudis_preview_recover_device`] for the caller-facing
/// contract; everything below is the sequence itself.
#[cfg(windows)]
pub(crate) fn recover_device(
    ctx: &RudisCtx,
    panel: *mut c_void,
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> RudisStatus {
    let state = Arc::clone(&ctx.preview_surface);

    // The affinity rule, for the same reason attach has one: step 4 ends in a
    // FIRST `configure` and therefore in `SetSwapChain`. The check subsumes the
    // attached check — an unattached state has no recorded thread.
    if let Err(status) = super::exports::check_affinity_public(&state, std::thread::current().id())
    {
        return status;
    }

    // Nothing to do. Deliberately `Ok` rather than an error: the shell's poll
    // is edge-triggered on `lost && !recovering`, so a call that arrives after
    // some other path already cleared the loss is a benign race, not a fault.
    if !state.device_lost.load(Relaxed) {
        return RudisStatus::Ok;
    }

    // A latched guard refuses BEFORE the quiesce stage, so a refusal pauses and
    // releases nothing (the `a_latched_recovering_flag_refuses_every_later_attempt`
    // property). The swap below is still the real single-entry guard.
    if state.recovering.load(Acquire) {
        eprintln!(
            "[rudis_ffi] panel preview: recovery refused — one is already in flight, or a \
             previous one FAILED and the guard is latched closed (fail-closed, by design)."
        );
        return RudisStatus::SurfaceCreateFailed;
    }

    // 71-REVIEW WR-02: WHICH adapter was lost, read off the removed device itself
    // before anything is paused or released. Both adapter probes below match this
    // LUID, so on a hybrid machine the other GPU can neither pass the pre-flight nor
    // cut short step 4's wait for the lost one. Unreadable (no compositor, so there
    // is nothing to recover) refuses here, before the quiesce stage, and latches.
    let Some(lost_luid) = lost_adapter_luid(&state) else {
        state.recovering.store(true, Release);
        eprintln!(
            "[rudis_ffi] panel preview: RECOVERY REFUSED before quiesce, and NOTHING WAS \
             TORN DOWN. The lost device's adapter LUID could not be read (the preview GPU set \
             is empty or not DX12), so there is no adapter to wait for and match."
        );
        return RudisStatus::SurfaceCreateFailed;
    };
    eprintln!("[rudis_ffi] panel preview: the lost device's adapter LUID is {lost_luid:#018x}");

    // ═══ QUIESCE: release every other D3D12 reference this process can reach ═══
    //
    // Phase 71 (TRUST-01), measured by 71-05: D3D12 devices are singletons per adapter.
    // The render cache's background compositor, any running bake and the Timeline's
    // wgpu-29 device are all references to the SAME removed device, and the hardware
    // adapter stays hidden until the LAST one is released. The shell suspends the
    // Timeline surface on the UI thread before calling this export; this stage does the
    // engine side. Steps 1-3 below then release the preview's own references.
    let quiesce = quiesce_process_gpu_holders();

    // ═══ PRE-FLIGHT: refuse BEFORE tearing anything down ════════════════════
    //
    // ⚠ THIS CHECK IS WHY RECOVERY NEVER HALF-WIRES A SURFACE (63-CONTEXT D-03).
    //
    // Step 4's same-adapter LUID re-assert REFUSES a software recreate (T-63-02),
    // correctly: the D3D11VA decoder and the wgpu device share textures by handle and a
    // shared handle cannot cross adapters. So recovery is refused here, while nothing
    // has been released, when it cannot succeed:
    //
    //  1. a cancelled bake did not return in time, so a reference to the removed device
    //     is still live outside the preview and step 4 would only ever see WARP; or
    //  2. DXGI lists no physical adapter at all (the GPU really went away).
    //
    // What this no longer does is ask wgpu for a DX12 adapter. 63-02's pre-flight did,
    // and 71-05 measured why it could never pass: the preview's own compositor is itself
    // a reference to the removed singleton, so the adapter cannot reappear until step 3
    // has run. Step 4 now carries that wait, after the last release.
    if quiesce.rows_live_after > 0 {
        state.recovering.store(true, Release);
        eprintln!(
            "[rudis_ffi] panel preview: RECOVERY REFUSED at pre-flight, and NOTHING WAS \
             TORN DOWN. {} render-cache bake(s) did not return within {QUIESCE_ROWS_TIMEOUT:?} \
             of cancel, and each holds a reference to the removed D3D12 device (devices are \
             singletons per adapter), so step 4 could only recreate on WARP, which the \
             same-adapter LUID re-assert refuses (T-63-02).",
            quiesce.rows_live_after
        );
        return RudisStatus::SurfaceCreateFailed;
    }
    // 71-REVIEW WR-01: a strong count above 1 when the slot dropped its reference
    // means some clone of the background compositor is still alive outside the
    // registry's view, and it holds the removed device. Nothing is torn down yet,
    // so refusing here is the clean outcome.
    if let Some(count) = quiesce.bg_released.filter(|&c| c > 1) {
        state.recovering.store(true, Release);
        eprintln!(
            "[rudis_ffi] panel preview: RECOVERY REFUSED at pre-flight, and NOTHING WAS \
             TORN DOWN. The background compositor still had {} clone(s) besides the slot \
             when it was released, and each holds a reference to the removed D3D12 device, \
             so step 4 could only recreate on WARP, which the same-adapter LUID re-assert \
             refuses (T-63-02).",
            count - 1
        );
        return RudisStatus::SurfaceCreateFailed;
    }
    match wait_for_physical_adapter(HARDWARE_ADAPTER_TIMEOUT, lost_luid) {
        Ok(name) => eprintln!(
            "[rudis_ffi] panel preview: pre-flight passed — DXGI lists the physical adapter \
             \"{name}\" and the render cache is quiet (rows drained in {} ms, bg \
             compositor released: {:?}); running the coordinated recovery",
            quiesce.rows_wait_ms,
            quiesce.bg_released
        ),
        Err(seen) => {
            // Latch closed WITHOUT touching the preview GPU set. The render cache stays
            // paused: a bake on WARP would re-hide the adapter.
            state.recovering.store(true, Release);
            eprintln!(
                "[rudis_ffi] panel preview: RECOVERY REFUSED at pre-flight, and NOTHING WAS \
                 TORN DOWN. DXGI did not list the lost physical adapter (LUID \
                 {lost_luid:#018x}) within {HARDWARE_ADAPTER_TIMEOUT:?} of the loss — only \
                 [{seen}] — so there is no hardware to recreate on, and a recreate on WARP \
                 or on another adapter is refused by the same-adapter LUID re-assert \
                 (T-63-02)."
            );
            return RudisStatus::SurfaceCreateFailed;
        }
    }

    // FAIL CLOSED (threat T-63-05). `recovering` is set for the duration and,
    // on failure, is LEFT SET for the rest of the session — `RecoveryPlan`'s
    // own documented convention ("a failed recovery leaves it ENGAGED"),
    // mirrored here so the shell's trigger can never become a retry storm
    // against half-recreated state. It is also what makes a second entry
    // impossible from any thread, not merely unlikely.
    if state.recovering.swap(true, Release) {
        eprintln!(
            "[rudis_ffi] panel preview: recovery refused — one is already in flight, or a \
             previous one FAILED and the guard is latched closed (fail-closed, by design)."
        );
        return RudisStatus::SurfaceCreateFailed;
    }

    let outcome = run_recovery(ctx, &state, panel, width_px, height_px, scale, lost_luid);

    match outcome {
        Ok(report) => {
            // Order matters: clear `lost` only once the surface is genuinely
            // live again, so a poll that interleaves can never read
            // "not lost, not recovering" over a hole.
            state.recovered.fetch_add(1, Relaxed);
            state.device_lost.store(false, Relaxed);
            state.recovering.store(false, Release);
            // The render cache resumes only after a SUCCESSFUL recovery; its next bake
            // rebuilds the background compositor on the recovered hardware adapter.
            app_core::render_cache_job::set_paused(false);
            eprintln!("[rudis_ffi] quiesce: render cache resumed after the recovery");
            eprintln!(
                "[rudis_ffi] panel preview: RECOVERED — {} steps, LUID {:#018x}, playhead \
                 {} us, resumed paused={}. attach_epoch={}",
                report.steps.len(),
                report.reasserted_luid,
                report.restored_playhead_us,
                report.resumed_paused,
                state.attach_epoch.load(Relaxed)
            );
            RudisStatus::Ok
        }
        Err(e) => {
            // `recovering` stays TRUE — see the comment on the swap above.
            eprintln!(
                "[rudis_ffi] panel preview: RECOVERY FAILED and is now latched closed: {e}. \
                 The preview stays dark until the app is restarted; this is detect-and-degrade, \
                 never a teardown/attach retry storm."
            );
            RudisStatus::SurfaceCreateFailed
        }
    }
}

/// The six steps, expressed as `engine::RecoveryHooks` and run by
/// `engine::RecoveryPlan::recover`.
#[cfg(windows)]
fn run_recovery(
    ctx: &RudisCtx,
    state: &Arc<PreviewSurfaceState>,
    panel: *mut c_void,
    width_px: u32,
    height_px: u32,
    scale: f32,
    lost_luid: i64,
) -> Result<engine::RecoveryReport, engine::EngineError> {
    use engine::EngineError;

    // T-51-01: never trust the caller's claimed pointer type, on this export
    // exactly as on attach. Done BEFORE anything is torn down, so a wrong
    // pointer costs nothing.
    let panel_native = super::surface::query_swap_chain_panel_native(panel)
        .map_err(|e| EngineError::Gpu(format!("recovery: panel pointer rejected: {e}")))?;

    // T-51-06: clamp before anything reaches the driver, same bounds as attach.
    let w = width_px.clamp(1, 16_384);
    let h = height_px.clamp(1, 16_384);
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };

    // The live device/queue handles the coordinator needs — for the injection
    // seam's re-arm and, more importantly, so `RecoveryPlan::recover` can NULL
    // THEM ITSELF immediately after step 3. That is not bookkeeping: they are
    // handles to the removed device, and constraint 3 in this module's header
    // is why the coordinator owning them is the point.
    let (live_device, live_queue) = {
        let gpu = lock_gpu(state);
        match gpu.compositor.as_ref() {
            Some(c) => (c.device().clone(), c.queue().clone()),
            None => {
                return Err(EngineError::Gpu(
                    "recovery: the preview GPU set is already empty — there is no device to \
                     recover from (a previous recovery released it and did not finish)"
                        .into(),
                ))
            }
        }
    };

    // A `'static` Tokio handle for step 4's adapter request (the GPU-03 VRAM
    // budget watch). Taken through this crate's EXISTING per-instance runtime
    // rather than a second executor — `crates/ffi` deliberately takes no
    // `pollster` dependency (see `surface::RequestAdapter`'s doc), and
    // `FfiAppCtx::block_on` is that seam. Resolving the handle from INSIDE the
    // runtime is what makes it `Clone + Send + 'static` and therefore storable
    // in a `RecoveryHooks` closure, with no duplicate builder shape anywhere.
    let rt: tokio::runtime::Handle = {
        use app_core::AppCtx as _;
        crate::ctx::FfiAppCtx::new(ctx).block_on(async { tokio::runtime::Handle::current() })
    };

    // The playhead the coordinator restores to. Read from THIS ctx's mirror —
    // the loss did not move it, so this is a record of where the user was, and
    // step 5's contract (resume PAUSED, never mid-frame) is the coordinator's
    // by construction: `RecoveryPlan::recover` has no play path at all.
    let playhead_us = ctx.mirror.position_us.load(Relaxed);

    // The panel interface pointer crosses into the `'static + Send` hooks as a
    // `usize`. That is the whole reason it is an integer: casting a pointer to
    // an integer and back is SAFE Rust, so this needs no `unsafe impl Send`
    // anywhere (there is none in this crate, and this does not become the
    // first). The pointer is live for the whole of this function — it is the
    // QI'd reference obtained above and released below — and the plan that
    // holds these closures is a local, built here and dropped before this
    // function returns, so the address can never outlive its referent.
    let panel_addr = panel_native as usize;

    let hooks = engine::RecoveryHooks {
        // ── STEP 1 ────────────────────────────────────────────────────────
        // Stop the present/producer threads. There is no join here and there
        // must not be: `preview::present_loop_with_gpu_budget` has no stop
        // signal and is frozen (`rudis_preview_detach_panel` documents the same
        // thing for the same reason). What stops is the WORK — flipping
        // `attached` makes every `PresentSink` method degrade to the documented
        // no-op, and `compositor()` answers `None` once step 3 has run, so no
        // producer can respawn against a dead device.
        //
        // Then the part that is genuinely load-bearing: WAIT for the ring
        // producer to release its `Arc<Compositor>` clone. See constraint 3 in
        // this module's header — this is the difference between step 4 landing
        // on the real adapter and step 4 landing on WARP.
        stop_threads: {
            let state = Arc::clone(state);
            Box::new(move || {
                state.attached.store(false, Release);
                wait_for_sole_compositor_owner(&state)
            })
        },

        // ── STEP 2 ────────────────────────────────────────────────────────
        // The FFmpeg side FIRST, and inside it the pool-slice pins BEFORE the
        // pool. `PreviewGpu::gpu_frame` holds an imported texture plus its
        // refcounted `AVFrame` keeper, which pins one slice of the D3D11VA
        // frame pool for as long as it is stored — releasing the pooled device
        // master underneath a live pin is precisely the ordering hook step 2
        // documents.
        //
        // The FFI panel layer owns no `HwDecodeSession` itself (sessions belong
        // to the ring producer, which step 1 just waited out), so there is
        // nothing else here to unref: what it DOES own a share of is the
        // process-wide pooled D3D11VA device master, which no session drop can
        // reach (59.1-03) and which would otherwise hand the dead device to
        // every reopened session.
        teardown_hw_pool: {
            let state = Arc::clone(state);
            Box::new(move || {
                {
                    let mut gpu = lock_gpu(&state);
                    gpu.gpu_frame = None;
                    gpu.composited_dims = None;
                }
                engine::clear_pooled_device();
                Ok(())
            })
        },

        // ── STEP 3 ────────────────────────────────────────────────────────
        // Drop the wgpu device/surface/compositor resources tied to the dead
        // device, in the order `PreviewGpu`'s own field comments state: the
        // compositor and surface first, the instance LAST (the surface and the
        // compositor's adapter/device all reference the instance's internals),
        // and the VRAM watch with them because its `Drop` is what unregisters
        // the DXGI budget notification.
        //
        // This is THE handle release the WARP hazard exists for. The
        // coordinator nulls its OWN `live_device`/`live_queue` immediately
        // after this hook returns.
        teardown_gpu: {
            let state = Arc::clone(state);
            Box::new(move || {
                {
                    let mut gpu = lock_gpu(&state);
                    let outstanding =
                        gpu.compositor.as_ref().map(Arc::strong_count).unwrap_or(0);
                    gpu.compositor = None;
                    gpu.surface = None;
                    gpu._instance = None;
                    gpu.vram_watch = None;
                    gpu.shown = false;
                    gpu.config_w = 0;
                    gpu.config_h = 0;
                    eprintln!(
                        "[rudis_ffi] step3 released the panel's GPU set \
                         (compositor+surface+instance+vram_watch); strong_count at release \
                         was {outstanding} (1 = this slot only, which is what step 4 needs)"
                    );
                }

                // ⚠ AND THE HANDLE THAT IS NOT OURS TO DROP.
                //
                // Releasing the `wgpu::Surface` above released WGPU's reference to the
                // `IDXGISwapChain`. The PANEL still holds its own — taken by
                // `ISwapChainPanelNative::SetSwapChain` at the first configure — and a
                // swapchain keeps its command queue, and therefore the removed
                // `ID3D12Device`, alive. That single surviving handle is enough for DXGI
                // to hide the hardware adapter from step 4's enumeration and send the
                // recreate to WARP. Plan 63-02's first GREEN attempt measured exactly
                // that, with `strong_count` already down to 1; the verbatim record is in
                // `surface::clear_panel_swap_chain`'s doc.
                //
                // NOT fatal on its own: a failure here resurfaces a moment later as the
                // step-4 LUID re-assert refusing a WARP recreate, which is a louder
                // failure with a better diagnosis than anything this arm could print.
                //
                // SAFETY: `panel_addr` is this call's QI'd `ISwapChainPanelNative*` — see
                // step 4's note on why it crosses into these closures as an integer.
                match super::surface::clear_panel_swap_chain(panel_addr as *mut c_void) {
                    Ok(()) => eprintln!(
                        "[rudis_ffi] step3 unbound the SwapChainPanel from the dead swapchain \
                         (SetSwapChain(nullptr)) — without this the panel's own reference \
                         keeps the removed D3D12 device alive and step 4 lands on WARP"
                    ),
                    Err(e) => eprintln!(
                        "[rudis_ffi] step3 could not unbind the panel's swapchain: {e}. Step \
                         4 will very likely recreate on WARP and the LUID re-assert will \
                         refuse it."
                    ),
                }
                Ok(())
            })
        },

        // ── STEP 4 ────────────────────────────────────────────────────────
        // Recreate in the OPPOSITE order: D3D11VA decode device first, then the
        // wgpu instance → surface → compositor → configure → composition scale
        // → placeholder composite. The second half is `attach_gpu` — the SAME
        // function the first attach ran, called again, which is what makes
        // "the recreated surface is built exactly like the original" true by
        // construction rather than by review.
        //
        // ⚠ This is the leg that has to be on the UI thread, and the reason
        // `rudis_preview_recover_device` is a synchronous export the shell
        // calls from `Preview.xaml.cs` rather than something the present loop
        // drives.
        recreate: {
            let state = Arc::clone(state);
            Box::new(move || {
                // ⚠ FIRST: wait for DXGI to offer the hardware adapter again.
                // Everything below builds on whichever adapter enumeration hands
                // back, and step 4's LUID re-assert REFUSES a WARP recreate — so
                // building one moment too early does not degrade, it fails, and
                // it fails after step 3 has already torn the old device down.
                // Bounded, logged, and it reports what DXGI really offered.
                // Phase 71: this IS the wait for the adapter now. Step 3 just released
                // the preview's own references, the quiesce stage and the shell released
                // the others, and 71-05 measured the adapter reappearing ~200 ms after
                // the last release (max 485 ms under load). Bounded by
                // STEP4_ADAPTER_TIMEOUT. The pre-flight established that the physical
                // adapter is present, so a miss here means a reference nobody released,
                // and the error says so.
                // 71-REVIEW WR-02: it waits for the LOST adapter by LUID, so another
                // GPU on a hybrid machine cannot end the wait early.
                match super::surface::wait_for_hardware_adapter(STEP4_ADAPTER_TIMEOUT, lost_luid) {
                    Ok(name) => eprintln!("[rudis_ffi] step4 recreating on \"{name}\""),
                    Err(seen) => {
                        return Err(EngineError::Gpu(format!(
                            "recovery step4: the lost hardware DX12 adapter (LUID \
                             {lost_luid:#018x}) did not come back \
                             within {STEP4_ADAPTER_TIMEOUT:?} of the last known release — \
                             only [{seen}], so some reference to the removed device is still \
                             alive in this process. \
                             Recreating on a software adapter is refused by the same-adapter \
                             LUID re-assert (T-63-02), so this stops here rather than \
                             building a preview the decoder could never share a texture with."
                        )))
                    }
                }

                // The decode half. `acquire_pooled_d3d11_device` goes through
                // the ONE gate every hardware open already runs through
                // (T-59.1-03-05), so this is not a second creation path; step 2
                // released the dead master, so it genuinely creates one.
                let d3d11_device = engine::acquire_pooled_d3d11_device().map_err(|e| {
                    EngineError::Gpu(format!(
                        "recovery step4: could not recreate the D3D11VA decode device: {e}"
                    ))
                })?;

                let request_adapter =
                    |instance: &wgpu::Instance, surface: &wgpu::Surface<'static>| {
                        rt.block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                            power_preference: wgpu::PowerPreference::HighPerformance,
                            force_fallback_adapter: false,
                            compatible_surface: Some(surface),
                        }))
                        .map_err(|e| format!("request_adapter: {e}"))
                    };

                // SAFETY(the cast): `panel_addr` is the `ISwapChainPanelNative*`
                // this call's `query_swap_chain_panel_native` returned; the
                // reference is alive for the whole of `run_recovery`, which
                // outlives this closure (the plan holding it is a local dropped
                // before `run_recovery` returns).
                let attached = super::surface::attach_gpu(
                    panel_addr as *mut c_void,
                    w,
                    h,
                    s,
                    &request_adapter,
                )
                .map_err(|e| {
                    EngineError::Gpu(format!(
                        "recovery step4: the SwapChainPanel surface could not be recreated: {e}"
                    ))
                })?;

                let wgpu_device = attached.compositor.device().clone();
                let wgpu_queue = attached.compositor.queue().clone();
                let gpu_budget = attached.gpu_budget.clone();

                {
                    let mut gpu = lock_gpu(&state);
                    *gpu = PreviewGpu {
                        _instance: Some(attached.instance),
                        compositor: Some(Arc::clone(&attached.compositor)),
                        surface: Some(attached.surface),
                        frame: attached.frame,
                        config_w: w,
                        config_h: h,
                        // `attach_gpu` composited its placeholder, so the
                        // swapchain genuinely holds a picture already — the
                        // same claim the first attach makes.
                        shown: true,
                        composited_dims: None,
                        gpu_frame: None,
                        vram_watch: attached.vram_watch,
                    };
                }

                // The geometry the present thread reconfigures against, and the
                // dirty flag that makes it do so on its next tick.
                state.target_w.store(w, Relaxed);
                state.target_h.store(h, Relaxed);
                state.target_scale_bits.store(s.to_bits(), Relaxed);
                state.surface_dirty.store(true, Release);

                // ⚠ RECORDED, not hidden: the rebuilt GPU-03 VRAM budget handle
                // is NOT re-armed into the running present loop. That handle is
                // passed to `present_loop_with_gpu_budget` ONCE, at thread
                // spawn, and the loop is frozen with no seam to re-feed it —
                // so after a recovery the producer's GPU decode gate reads the
                // OLD watch's handle. The old watch was dropped in step 3, so
                // the value it reads is frozen at its last observation rather
                // than wrong-and-moving; the consequence is a stale VRAM
                // budget, never a wrong picture, and the honest fix is a
                // `crates/preview` seam this plan does not open.
                let _ = gpu_budget;

                Ok(engine::RecreatedComponents {
                    // 0 = "no hw FRAME pool was live under this coordinator",
                    // the reserved meaning `RecreatedComponents::pool_size`
                    // documents — and it is literally true here: frame pools
                    // belong to `HwDecodeSession`s owned by the ring producer,
                    // which step 1 waited out before step 2 ran. Reporting a
                    // plausible number nobody measured would be worse.
                    pool_size: 0,
                    d3d11_device,
                    wgpu_device,
                    wgpu_queue,
                })
            })
        },

        // ── STEP 5 ────────────────────────────────────────────────────────
        // Restore the playhead, resume PAUSED. The coordinator owns the PAUSED
        // half by construction (`recover` has no play path), and the shell has
        // already paused the transport before calling in — so the playhead has
        // not moved and there is nothing to reposition. Logged rather than
        // silently skipped, so the step is visible in the same `device_lost:
        // step<n>` transcript as the other five.
        restore_playhead: Box::new(move |us| {
            eprintln!(
                "[rudis_ffi] step5 playhead kept at {us} us; the transport was paused by the \
                 shell before recovery and stays paused (never resumed mid-frame)"
            );
            Ok(())
        }),

        // ── STEP 6 ────────────────────────────────────────────────────────
        // Restart the present path against the rebuilt state. The present
        // THREAD never died (step 1's note), so what restarts is its work:
        // `attached` goes true again and the attach epoch advances, which is
        // the observable a caller compares across a recovery to know the
        // surface it is looking at is a new one.
        restart_threads: {
            let state = Arc::clone(state);
            Box::new(move || {
                state.attached.store(true, Release);
                state.attach_epoch.fetch_add(1, Relaxed);
                Ok(())
            })
        },
    };

    let mut plan = engine::RecoveryPlan::new(live_device, live_queue, hooks);
    let result = plan.recover(playhead_us);

    // The plan (and with it every hook closure, and with them every `Arc`
    // clone of the state) is dropped here, before this function returns and
    // before the caller releases the panel's COM reference.
    drop(plan);
    result
}

/// Wait until this slot is the ONLY owner of the compositor `Arc`.
///
/// The ring producer takes an owned clone at spawn (`present_loop.rs`'s
/// `ctx.sink().compositor()`) and holds it for its whole life, so a producer
/// still running is a live handle to the removed device — constraint 3 in this
/// module's header, and the single most likely way a recovery silently degrades
/// to WARP.
///
/// Refusing here is SAFE: step 1 has torn nothing down, so a timeout leaves the
/// caller exactly where it was — a dead preview and a clear log — rather than a
/// dead preview with no compositor either.
#[cfg(windows)]
fn wait_for_sole_compositor_owner(
    state: &Arc<PreviewSurfaceState>,
) -> Result<(), engine::EngineError> {
    let deadline = Instant::now() + PRODUCER_RELEASE_TIMEOUT;
    let mut last = usize::MAX;
    loop {
        let count = {
            let gpu = lock_gpu(state);
            gpu.compositor.as_ref().map(Arc::strong_count).unwrap_or(0)
        };
        if count <= 1 {
            if last != usize::MAX {
                eprintln!(
                    "[rudis_ffi] step1 the ring producer released its compositor clone \
                     (strong_count {last} -> {count})"
                );
            }
            return Ok(());
        }
        if last != count {
            eprintln!(
                "[rudis_ffi] step1 waiting for {} outstanding compositor clone(s) to be \
                 released — a live handle to the removed device sends step 4 to WARP",
                count - 1
            );
            last = count;
        }
        if Instant::now() >= deadline {
            return Err(engine::EngineError::Gpu(format!(
                "recovery step1: {} handle(s) to the dead compositor are still outstanding after \
                 {:?} (the ring producer did not exit). Refusing to continue: step 4 would \
                 recreate on WARP and the same-adapter LUID re-assert would then fail AFTER the \
                 old device had already been torn down. Nothing has been released yet.",
                count - 1,
                PRODUCER_RELEASE_TIMEOUT
            )));
        }
        std::thread::sleep(PRODUCER_RELEASE_POLL);
    }
}

/// What the quiesce stage did, for the pre-flight and the log.
#[cfg(windows)]
struct QuiesceReport {
    /// Render-cache rows still queued/running when the bounded wait ended. Non-zero
    /// means a bake still holds a clone of the background compositor, i.e. a live
    /// reference to the removed device.
    rows_live_after: usize,
    /// How long the rows took to drain (or the timeout, if they did not).
    rows_wait_ms: u128,
    /// `release_background_compositor()`'s answer: `Some(strong_count)` just before the
    /// slot's reference dropped (1 = the slot was the only owner), or `None` when no bake
    /// had built one yet.
    bg_released: Option<usize>,
}

/// **Phase 71 (TRUST-01): release every engine-side D3D12 reference outside the preview.**
///
/// The engine half of 71-RESEARCH's quiescence protocol (steps 2 and 4); the shell
/// suspends the Timeline surface (step 5) before calling the export, and the preview's
/// own six steps follow. Order:
///
/// 1. pause the render cache, so no new bake can start (and build a compositor) inside
///    the recovery window;
/// 2. raise every bake's cancel latch;
/// 3. wait, bounded, for the live rows to drain, because a running bake holds a clone of
///    the background compositor until it returns;
/// 4. drop the background compositor slot.
///
/// Proxy encodes are NOT cancelled: they are out-of-process ffmpeg sidecars and hold no
/// D3D12 reference in this process (the singleton is per process).
///
/// The pause is lifted only by a SUCCESSFUL recovery. On a refusal or failure the render
/// cache stays paused for the session: a bake on WARP would re-hide the adapter.
#[cfg(windows)]
fn quiesce_process_gpu_holders() -> QuiesceReport {
    use app_core::render_cache_job;

    // 71-REVIEW WR-01: pause + cancel in ONE critical section on the registry lock.
    // The render-cache poll re-checks the pause under that same lock before it
    // registers a row, so no bake can register un-cancelled after this returns.
    render_cache_job::pause_and_cancel_all();
    eprintln!(
        "[rudis_ffi] quiesce: render cache paused (no bake may start in the recovery window) \
         and cancel raised on every render-cache bake, atomically; proxy encodes are left \
         running (out-of-process sidecars, no D3D12 reference in this process)"
    );

    let started = Instant::now();
    let deadline = started + QUIESCE_ROWS_TIMEOUT;
    let mut live = render_cache_job::live_row_count();
    while live > 0 && Instant::now() < deadline {
        std::thread::sleep(QUIESCE_ROWS_POLL);
        live = render_cache_job::live_row_count();
    }
    let rows_wait_ms = started.elapsed().as_millis();
    eprintln!(
        "[rudis_ffi] quiesce: live render-cache rows after {rows_wait_ms} ms = {live} \
         (bound {QUIESCE_ROWS_TIMEOUT:?})"
    );

    let bg_released = preview::render_cache_writer::release_background_compositor();
    match bg_released {
        Some(count) => eprintln!(
            "[rudis_ffi] quiesce: background compositor released (strong_count before the \
             drop = {count}; 1 = the slot was the only owner)"
        ),
        None => eprintln!(
            "[rudis_ffi] quiesce: background compositor slot was empty (no bake built one)"
        ),
    }

    QuiesceReport {
        rows_live_after: live,
        rows_wait_ms,
        bg_released,
    }
}

/// **The replacement pre-flight probe (Phase 71): is a physical adapter still listed?**
///
/// Asks DXGI directly (`CreateDXGIFactory1` + `EnumAdapters1`), never wgpu or D3D12.
/// wgpu's enumeration tries `D3D12CreateDevice` on each adapter, which fails for the
/// removed singleton's adapter while any reference lives, and that is the only reason
/// the adapter looked "gone" to 63-02's pre-flight. DXGI's own list is not affected by
/// a removed D3D12 device, so this answers the question the pre-flight actually needs:
/// is the hardware still there to recreate on. A fresh factory per attempt, because
/// DXGI caches the list on the factory.
///
/// Holds nothing across the call: the factory and adapters are released before it
/// returns, and none of them references the removed device.
///
/// **71-REVIEW WR-02: it must be the adapter that was LOST.** An adapter counts only
/// when its `AdapterLuid` equals `lost_luid`, so the other GPU of a hybrid machine
/// cannot pass the pre-flight while the lost one is really gone. As belt-and-braces it
/// also never counts Microsoft's basic render/display adapter (`VendorId 0x1414`),
/// in case a driver failure surfaces it without `DXGI_ADAPTER_FLAG_SOFTWARE`.
#[cfg(windows)]
fn wait_for_physical_adapter(timeout: Duration, lost_luid: i64) -> Result<String, String> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    /// Microsoft's PCI vendor id: WARP / Basic Render Driver / Basic Display.
    const MICROSOFT_VENDOR_ID: u32 = 0x1414;

    let deadline = Instant::now() + timeout;
    loop {
        let mut seen: Vec<String> = Vec::new();
        let mut physical: Option<String> = None;
        // SAFETY: plain DXGI factory/adapter calls; every interface is an owned
        // windows-rs smart pointer released at the end of this block.
        match unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
            Ok(factory) => {
                let mut index = 0u32;
                while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
                    index += 1;
                    let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
                        continue;
                    };
                    let end = desc
                        .Description
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.Description.len());
                    let name = String::from_utf16_lossy(&desc.Description[..end]);
                    let software = desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0
                        || desc.VendorId == MICROSOFT_VENDOR_ID;
                    let luid = ((desc.AdapterLuid.HighPart as i64) << 32)
                        | (desc.AdapterLuid.LowPart as i64);
                    seen.push(format!(
                        "{name} ({}, LUID {luid:#018x})",
                        if software { "software" } else { "physical" }
                    ));
                    if !software && luid == lost_luid && physical.is_none() {
                        physical = Some(name);
                    }
                }
            }
            Err(e) => seen.push(format!("CreateDXGIFactory1 failed: {e}")),
        }
        if let Some(name) = physical {
            return Ok(name);
        }
        if Instant::now() >= deadline {
            return Err(seen.join(" | "));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// **71-REVIEW WR-02: the adapter LUID of the device the preview lost.**
///
/// Read from the preview compositor's own `ID3D12Device` via `GetAdapterLuid`, the
/// same packing `engine::assert_same_adapter_luid` uses. `GetAdapterLuid` is a cached
/// read that stays valid on a removed device. The hal guard is dropped before this
/// returns and no COM reference is taken, so nothing here outlives the call or holds
/// the removed device across the teardown. `None` when the GPU set is empty.
#[cfg(windows)]
fn lost_adapter_luid(state: &PreviewSurfaceState) -> Option<i64> {
    let gpu = lock_gpu(state);
    let compositor = gpu.compositor.as_ref()?;
    // SAFETY: the guard borrows wgpu's hal device and is dropped at the end of this
    // function; nothing is destroyed and no reference outlives the borrow.
    let hal = unsafe { compositor.device().as_hal::<wgpu_hal::api::Dx12>() }?;
    let luid = unsafe { hal.raw_device().GetAdapterLuid() };
    Some(((luid.HighPart as i64) << 32) | (luid.LowPart as i64))
}

/// The GPU lock, recovering from poisoning.
///
/// The same reasoning `rudis_preview_attach_panel` and
/// `rudis_preview_detach_panel` already record: a poisoned lock here means a
/// previous present unwound, and the set it protects is being wholly REPLACED,
/// so recovering is sound — while refusing would make the panel permanently
/// unrecoverable, which is the very defect this module exists to close.
#[cfg(windows)]
fn lock_gpu(state: &PreviewSurfaceState) -> std::sync::MutexGuard<'_, PreviewGpu> {
    match state.gpu.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(all(test, windows))]
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

    /// The affinity rule reaches recovery too: an unattached ctx has no
    /// recorded attaching thread, so a recovery call answers `NotAttached`
    /// rather than reaching for a panel that is not there.
    #[test]
    fn recovery_on_an_unattached_ctx_is_not_attached() {
        let ctx = in_process_ctx();
        // SAFETY: the live box this test just created.
        let status = super::recover_device(unsafe { &*ctx }, 0x1 as *mut c_void, 800, 600, 1.0);
        assert_eq!(status, RudisStatus::NotAttached);
        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// A ctx that has never lost a device answers `Ok` and does NOTHING —
    /// the benign-race arm. Asserted on the observables rather than on the
    /// return value alone: a recovery that had actually run would have moved
    /// `attach_epoch` and `recovered`.
    #[test]
    fn recovery_without_a_loss_is_a_no_op() {
        let ctx = in_process_ctx();
        // SAFETY: the live box this test just created.
        let ctx_ref = unsafe { &*ctx };
        // Pretend a panel attached on THIS thread, without any GPU work: the
        // affinity guard reads only the recorded thread id.
        match ctx_ref.preview_surface.attach_thread.lock() {
            Ok(mut g) => *g = Some(std::thread::current().id()),
            Err(p) => *p.into_inner() = Some(std::thread::current().id()),
        }

        let status = super::recover_device(ctx_ref, 0x1 as *mut c_void, 800, 600, 1.0);
        assert_eq!(status, RudisStatus::Ok, "no loss observed -> nothing to do");
        let (lost, recovering, recovered, _presented, epoch) =
            ctx_ref.preview_surface.device_status();
        assert!(!lost && !recovering);
        assert_eq!(recovered, 0, "no recovery ran, so none may be reported");
        assert_eq!(epoch, 0, "no re-attach happened");

        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// **The fail-closed latch, as a property.** Once `recovering` is engaged —
    /// which is what a FAILED recovery leaves behind, by
    /// `RecoveryPlan::recover`'s own convention — every later attempt is
    /// refused without touching anything (threat T-63-05: never a
    /// teardown/attach retry storm).
    #[test]
    fn a_latched_recovering_flag_refuses_every_later_attempt() {
        let ctx = in_process_ctx();
        // SAFETY: the live box this test just created.
        let ctx_ref = unsafe { &*ctx };
        match ctx_ref.preview_surface.attach_thread.lock() {
            Ok(mut g) => *g = Some(std::thread::current().id()),
            Err(p) => *p.into_inner() = Some(std::thread::current().id()),
        }
        ctx_ref.preview_surface.device_lost.store(true, Relaxed);
        ctx_ref.preview_surface.recovering.store(true, Release);

        for _ in 0..3 {
            assert_eq!(
                super::recover_device(ctx_ref, 0x1 as *mut c_void, 800, 600, 1.0),
                RudisStatus::SurfaceCreateFailed,
                "a latched guard must refuse, every time, without side effects"
            );
        }
        let (lost, recovering, recovered, _presented, epoch) =
            ctx_ref.preview_surface.device_status();
        assert!(lost && recovering, "the latch and the loss both survive a refusal");
        assert_eq!((recovered, epoch), (0, 0), "a refusal changes nothing");

        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }
}
