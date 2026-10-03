//! Coordinated device-lost / TDR recovery (Phase 48, plan 48-10 — GPU-04).
//!
//! A TDR resets the WHOLE adapter: the D3D11 decode device and the D3D12/wgpu
//! device fail together. Recovery must therefore tear down and recreate ALL
//! FOUR components — the device(+surface), the swapchain(-shared machinery),
//! the compositor resources, AND the hardware-decoder frame pool (the one
//! GPU-04 names explicitly because it is the component most easily forgotten)
//! — in a coordinated order, or it leaves dangling pool references / presents
//! from a partially-recreated resource set.
//!
//! ## Which injection branch shipped (the 48-03 probe's answer)
//!
//! **`PROBE_RESULT=works`** — recorded in
//! `.planning/phases/48-gpu-resident-preview-frame-path/artifacts/48-03-remove-device-probe.txt`:
//! `ID3D12Device5` QI succeeded on Windows 10.0.19045 (RTX 3070 / driver
//! 610.47) and `RemoveDevice()` drove wgpu's own `into_device_result` →
//! `DeviceError::Lost` → `set_device_lost_callback` chain in < 2 s with reason
//! `Unknown`. [`RecoveryPlan::simulate_device_lost`] therefore takes the
//! **RemoveDevice-on-the-live-device branch** (CONTEXT D-13 route (a)): a REAL
//! device removal through the SAME detection path a genuine TDR fires — not a
//! synthetic error-return shim. The seam-injection fallback (calling the same
//! detection handler directly) remains available by construction — the
//! teardown/recreate path is reachable from any [`DeviceLostSignal`] record —
//! but is not the shipped trigger.
//!
//! **The 48-03 MEASURED ordering constraint, honored here:** after
//! `RemoveDevice`, calling `create_command_encoder` is a native
//! `STATUS_ACCESS_VIOLATION` — an uncatchable AV inside wgpu/driver code, not
//! a Rust panic. The traffic encoder is therefore created BEFORE the trigger;
//! only **submit** and **poll** touch the removed device afterward.
//!
//! ## Detection (48-RESEARCH.md Pattern 5, VERIFIED)
//!
//! Three sites can observe a loss, and all three must funnel into ONE recovery
//! entry (threat T-48-10-01 — never three competing recoveries):
//!
//! 1. **wgpu device side** — wgpu-hal maps `DXGI_ERROR_DEVICE_RESET |
//!    DXGI_ERROR_DEVICE_REMOVED` → `DeviceError::Lost` at EVERY DX12 call site
//!    (`Present()` and `Signal()` included), delivered through
//!    `Device::set_device_lost_callback`. [`DeviceLostSignal::register`] wires
//!    it. `DeviceLostReason` has exactly two variants: `Unknown` (covers TDR)
//!    and `Destroyed` (an explicit destroy — NOT a TDR).
//! 2. **Surface side** — `SurfaceError::Lost` from `get_current_texture`.
//!    Funnel via [`DeviceLostSignal::record`].
//! 3. **D3D11 decode-device side** —
//!    [`crate::hwdecode::HwDecodeSession::device_removed_reason`] polls
//!    `ID3D11Device::GetDeviceRemovedReason` so the decode thread can classify
//!    an av error as "media problem" vs "whole-adapter loss". Because a TDR
//!    resets the whole adapter, either failure implies the other device is
//!    gone too.
//!
//! The wgpu-hal `device_lost_panic` / `internal_error_panic` features must
//! stay OFF (48-RESEARCH.md Pitfall 5) or this entire module is dead code and
//! the process just crashes; the manifest-guard test at the bottom of this
//! file makes that a red gate.
//!
//! ## Recovery order (Pattern 5, steps 1–6 — implemented VERBATIM)
//!
//! [`RecoveryPlan::recover`] runs the six steps in order, logging each with
//! the stable prefix `device_lost: step<n>` so a test can assert the ORDER,
//! not just the outcome:
//!
//! 1. Stop the decode thread and the present/producer threads.
//! 2. Tear down the FFmpeg side FIRST, while its handles are
//!    known-bad-but-addressable: `av_buffer_unref(&hw_frames_ctx)` then
//!    `av_buffer_unref(&hw_device_ctx)`, and drop + RECREATE the
//!    `AVCodecContext` entirely. Dropping [`crate::hwdecode::HwDecodeSession`]
//!    encapsulates exactly that unref order (its field order IS drop order:
//!    codec context first — which unrefs the hw frame pool — then the demuxer,
//!    then the session's own device ref).
//! 3. Drop the wgpu `Device`/`Surface`/compositor-owned GPU resources tied to
//!    the dead device.
//! 4. Recreate in the OPPOSITE order: new D3D11VA device → new
//!    `AVHWFramesContext` (the SAME widened `initial_pool_size` —
//!    `open_hw_decoder` re-applies the widening formula, never FFmpeg's bare
//!    default 17) → new wgpu device (RE-RUNNING the same-adapter LUID
//!    assertion, since a TDR can in principle cause Windows to re-enumerate
//!    adapters differently — the Phase-44 assert is NOT a one-time startup
//!    check) → new swapchain/surface → compositor resources.
//! 5. Restore the playhead position and resume **PAUSED** (locked decision —
//!    never mid-frame), surfacing the non-modal notice.
//! 6. Restart the decode/present threads.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Direct3D12::ID3D12Device5;

use crate::import::{d3d11_adapter_luid, luid_hex};
use crate::EngineError;

// ---------------------------------------------------------------------------
// The single detection funnel.
// ---------------------------------------------------------------------------

/// One observed device-loss event (whichever of the three detection sites saw
/// it first).
#[derive(Debug, Clone)]
pub struct DeviceLostEvent {
    /// Which detection site reported it (e.g. `"wgpu-device-lost-callback"`,
    /// `"surface-get-current-texture"`, `"decode-thread-removed-reason"`).
    pub source: String,
    /// Human-readable detail (reason + message / HRESULT).
    pub detail: String,
}

/// The ONE detection funnel all three loss-observation sites report into.
///
/// First report wins (later reports are logged as duplicates and counted);
/// the consumer — the recovery driver — observes via [`DeviceLostSignal::take`]
/// or [`DeviceLostSignal::wait`] and invokes [`RecoveryPlan::recover`] ONCE.
/// Cheap to clone (`Arc` innards) so every detection site can hold one.
#[derive(Clone, Default)]
pub struct DeviceLostSignal {
    first: Arc<Mutex<Option<DeviceLostEvent>>>,
    seen: Arc<AtomicUsize>,
}

impl DeviceLostSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire detection site 1: register this funnel as `device`'s
    /// device-lost callback (`Device::set_device_lost_callback` — the exact
    /// chain the 48-03 probe measured end-to-end).
    ///
    /// `Destroyed` is deliberately NOT funneled: it means an explicit
    /// `destroy()`/orderly drop (wgpu-types: "only for an explicit destroy"),
    /// which happens during normal teardown and during recovery's own step 3 —
    /// recovering from it would fight shutdown. A TDR reports `Unknown`
    /// (probe-measured: `lost_reason=Unknown`).
    pub fn register(&self, device: &wgpu::Device) {
        self.register_with_response(device, || {});
    }

    /// [`DeviceLostSignal::register`] plus an on-loss `response` — **the
    /// PRODUCTION arming entry** (quick-260829-n96).
    ///
    /// [`crate::Compositor`]'s `build` calls this at its device-birth site for
    /// presentation-capable devices, so a real TDR both funnels here AND runs
    /// the degrade ([`crate::note_hw_device_reset`]) with no registration step
    /// a future host rewrite can forget — the same un-losable placement
    /// argument `install_uncaptured_error_guard` already carries.
    ///
    /// ONE callback body, not two: [`DeviceLostSignal::register`] delegates
    /// here with an empty response, so the `Destroyed` filter and the funnel
    /// `record` cannot drift between the two entry points (threat T-48-10-01 —
    /// one funnel, never a second callback wired around it). `response` runs
    /// AFTER the record, at most once, and never for `Destroyed`: an orderly
    /// destroy/drop returns before it, so app exit and offscreen-compositor
    /// drops cannot fire a degrade.
    ///
    /// Registration REPLACES any previously registered callback, and the
    /// replaced closure is dropped WITHOUT being invoked (read from wgpu-core
    /// 26.0.1 `Global::device_set_device_lost_closure`, `device/global.rs:2152`
    /// — `.lock().replace(..)`). Production registers exactly once, here;
    /// `crates/engine/tests/device_lost_recovery.rs` deliberately takes the
    /// callback over with its own signal, which is why arming at device birth
    /// cannot change that test's behaviour.
    pub fn register_with_response(
        &self,
        device: &wgpu::Device,
        response: impl FnOnce() + Send + 'static,
    ) {
        let funnel = self.clone();
        // `set_device_lost_callback` wants an `Fn`, while the response is a
        // once-only action: the `Mutex<Option<_>>::take` makes "runs at most
        // once" structural. wgpu-core `take`s the closure when it fires
        // (`device/resource.rs:3950`), so the callback is already once-only on
        // its side too.
        let response: Mutex<Option<Box<dyn FnOnce() + Send>>> =
            Mutex::new(Some(Box::new(response)));
        device.set_device_lost_callback(move |reason, message| {
            let label = match reason {
                wgpu::DeviceLostReason::Unknown => "Unknown",
                wgpu::DeviceLostReason::Destroyed => "Destroyed",
            };
            if matches!(reason, wgpu::DeviceLostReason::Destroyed) {
                eprintln!(
                    "device_lost: device reported Destroyed ({message}) — orderly teardown, \
                     not a TDR; no recovery"
                );
                return;
            }
            funnel.record("wgpu-device-lost-callback", format!("{label}: {message}"));
            if let Some(run) = response.lock().unwrap_or_else(|e| e.into_inner()).take() {
                run();
            }
        });
    }

    /// Report a loss observed at any detection site (sites 2 and 3 call this
    /// directly; site 1 arrives via [`DeviceLostSignal::register`]).
    pub fn record(&self, source: &str, detail: impl Into<String>) {
        let detail = detail.into();
        self.seen.fetch_add(1, Ordering::SeqCst);
        let mut guard = self.first.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            eprintln!("device_lost: detected via={source} detail={detail}");
            *guard = Some(DeviceLostEvent {
                source: source.to_owned(),
                detail,
            });
        } else {
            eprintln!(
                "device_lost: duplicate report via={source} ignored (already funneled): {detail}"
            );
        }
    }

    /// The first observed event, if any (leaves it in place so late duplicate
    /// reporters still see "already funneled").
    pub fn observed(&self) -> Option<DeviceLostEvent> {
        self.first.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Total reports funneled (diagnostics).
    pub fn reports(&self) -> usize {
        self.seen.load(Ordering::SeqCst)
    }

    /// Poll for the first event for up to `timeout` (the wgpu callback fires
    /// asynchronously — the probe measured < 2 s on this machine).
    pub fn wait(&self, timeout: Duration) -> Option<DeviceLostEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(e) = self.observed() {
                return Some(e);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

// ---------------------------------------------------------------------------
// The forced-loss injection seam.
// ---------------------------------------------------------------------------

/// Force a REAL device removal on a LIVE wgpu device — the
/// **PROBE_RESULT=works** branch (CONTEXT D-13 route (a)): QI `ID3D12Device5`
/// via `as_hal` (the 48-03 probe's exact shape) and call `RemoveDevice()`, so
/// detection is exercised through the SAME `into_device_result` ->
/// `DeviceError::Lost` -> device-lost-callback chain a genuine TDR fires — not
/// a synthetic error-return shim.
///
/// Honors the 48-03 MEASURED ordering constraint: every encoder is created
/// BEFORE the trigger (post-remove `create_command_encoder` is a native,
/// uncatchable STATUS_ACCESS_VIOLATION); only submit and poll — both
/// probe-measured safe — touch the removed device afterward.
///
/// Factored VERBATIM out of [`RecoveryPlan::simulate_device_lost`]
/// (quick-260829-n96) so the same injection can be aimed at a compositor built
/// through the PRODUCTION construction funnel, which owns its device and hands
/// out only `&` borrows (`crates/engine/tests/device_lost_wiring.rs`).
/// `simulate_device_lost` keeps its live-handle check and delegates here — one
/// injection body, never two that can drift.
pub fn inject_forced_device_loss(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> Result<(), EngineError> {
    let raw = {
        let hal_device = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }.ok_or_else(|| {
            EngineError::Gpu(
                "live device exposes no DX12 hal device — RemoveDevice injection \
                     requires the DX12 backend"
                    .into(),
            )
        })?;
        hal_device.raw_device().clone()
    };
    let device5: ID3D12Device5 = raw.cast::<ID3D12Device5>().map_err(|e| {
        EngineError::Gpu(format!(
            "ID3D12Device5 QI failed ({e}) — the 48-03 probe recorded PROBE_RESULT=works \
             on this machine; if this fires the environment changed and the probe should \
             be re-run"
        ))
    })?;

    // 48-03 measured constraint: EVERY resource/encoder is created BEFORE
    // the trigger (post-remove `create_command_encoder` is a native,
    // uncatchable STATUS_ACCESS_VIOLATION); only submit + poll touch the
    // removed device below.
    //
    // TWO traffic submissions, each with REAL recorded work — a NEW
    // measurement from building this seam (bisected on this machine,
    // recorded in 48-10-SUMMARY.md): on a device whose queue history
    // includes a readback (`composite_gpu_to_rgba`'s
    // copy_texture_to_buffer + map_async), the FIRST post-remove submit is
    // absorbed without any device-touching call failing — poll returns
    // Ok(QueueEmpty), no uncaptured error, no lost callback, ever. The
    // SECOND submit reliably surfaces DXGI_ERROR_DEVICE_REMOVED through
    // wgpu's own into_device_result -> DeviceError::Lost -> the registered
    // callback. On a fresh device (the 48-03 probe's shape) one submit
    // sufficed; two is correct for both.
    let traffic_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("48-10-simulate-device-lost-traffic-buffer"),
        size: 256,
        usage: wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder1 = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("48-10-simulate-device-lost-traffic-1"),
    });
    encoder1.clear_buffer(&traffic_buffer, 0, None);
    let mut encoder2 = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("48-10-simulate-device-lost-traffic-2"),
    });
    encoder2.clear_buffer(&traffic_buffer, 0, None);

    eprintln!(
        "device_lost: simulate_device_lost — calling ID3D12Device5::RemoveDevice on the \
         live device (PROBE_RESULT=works branch)"
    );
    unsafe { device5.RemoveDevice() };

    // Only submit + poll post-trigger (probe-measured safe), twice.
    queue.submit(Some(encoder1.finish()));
    let poll1 = device.poll(wgpu::PollType::Wait);
    queue.submit(Some(encoder2.finish()));
    let poll2 = device.poll(wgpu::PollType::Wait);
    eprintln!("device_lost: simulate_device_lost post-remove polls: {poll1:?} / {poll2:?}");
    Ok(())
}

// ---------------------------------------------------------------------------
// The LUID re-assert (recovery step 4's structural check).
// ---------------------------------------------------------------------------

/// RE-RUN the same-adapter LUID assertion Phase 44 established, on the
/// RECREATED devices.
///
/// A TDR can in principle cause Windows to re-enumerate adapters differently
/// (and two DX12 adapters genuinely enumerate on the dev machine: the RTX 3070
/// and WARP), so the startup-time assert is NOT sufficient — recovery must
/// prove the NEW D3D11 decode device and the NEW wgpu device still share one
/// adapter before any shared-handle import is attempted. Same read shape as
/// `import.rs`: the D3D11 side walks device → adapter desc → `AdapterLuid`;
/// the wgpu side asks its own raw `ID3D12Device` via `GetAdapterLuid` — never
/// `wgpu::AdapterInfo`.
///
/// Returns the (shared) LUID on success so the caller can log/record it.
pub fn assert_same_adapter_luid(
    d3d11_device: &ID3D11Device,
    wgpu_device: &wgpu::Device,
) -> Result<i64, EngineError> {
    let d3d11_luid = d3d11_adapter_luid(d3d11_device)?;
    let d3d12_luid = {
        let hal_device =
            unsafe { wgpu_device.as_hal::<wgpu::hal::api::Dx12>() }.ok_or_else(|| {
                EngineError::Gpu(
                    "recreated wgpu device exposes no DX12 hal device — not DX12?".into(),
                )
            })?;
        let luid = unsafe { hal_device.raw_device().GetAdapterLuid() };
        ((luid.HighPart as i64) << 32) | (luid.LowPart as i64)
    };
    if d3d11_luid != d3d12_luid {
        return Err(EngineError::Gpu(format!(
            "post-recovery adapter mismatch: the recreated decoder is on {} but the recreated \
             wgpu device is on {} — the TDR re-enumeration hazard the re-assert exists for; \
             refusing to resume (a shared handle cannot cross adapters)",
            luid_hex(d3d11_luid),
            luid_hex(d3d12_luid)
        )));
    }
    Ok(d3d12_luid)
}

// ---------------------------------------------------------------------------
// The coordinator.
// ---------------------------------------------------------------------------

/// What step 4's recreate hook must hand back so the coordinator can run the
/// structural checks itself (the LUID re-assert and the pool-size record are
/// performed BY [`RecoveryPlan::recover`], not left to hook convention).
pub struct RecreatedComponents {
    /// The REBUILT hw frame pool's size, read off the real recreated session
    /// (`HwDecodeSession::pool_size` — the authoritative `ArraySize`, never a
    /// restated request). The forced-loss test asserts this equals the
    /// pre-loss widened value — a recreated pool silently reverting to
    /// FFmpeg's default 17 would starve the ring without failing loudly
    /// (threat T-48-10-02).
    ///
    /// `0` is the ONE reserved value and it means **no hw frame pool was live
    /// under this coordinator** — not "a pool of size zero". A
    /// [`PreviewRecovery`] running the engine's default hooks reports it,
    /// because frame pools belong to [`crate::HwDecodeSession`]s and the
    /// compositor owns none; a host that does own sessions supplies
    /// `RecoveryHostHooks::recreate_decode` and reports the real re-widened
    /// size, which is what makes the T-48-10-02 assertion meaningful.
    pub pool_size: usize,
    /// The NEW D3D11VA decode device (for the LUID re-assert).
    pub d3d11_device: ID3D11Device,
    /// The NEW wgpu device (for the LUID re-assert; also re-arms the seam).
    pub wgpu_device: wgpu::Device,
    /// The NEW wgpu queue (re-arms the seam for a repeat injection).
    pub wgpu_queue: wgpu::Queue,
}

/// The six recovery hooks — one per step of the Pattern-5 order. The
/// coordinator owns WHEN each runs and the logged order; the hooks own the
/// component-specific work (the headless test wires them to a real
/// session/compositor pair; the shell wires them to its managed state).
pub struct RecoveryHooks {
    /// Step 1: stop the decode thread and the present/producer threads
    /// (mirror the existing pause-path teardown discipline in
    /// `present_loop.rs`/`ring.rs`).
    pub stop_threads: Box<dyn FnMut() -> Result<(), EngineError> + Send>,
    /// Step 2: tear down the FFmpeg side FIRST — `av_buffer_unref` the
    /// hw_frames_ctx then the hw_device_ctx and drop the `AVCodecContext`
    /// (dropping the `HwDecodeSession` encapsulates exactly that order).
    /// Any held `GpuFrame`s (pool-slice pins) must drop before/with it.
    pub teardown_hw_pool: Box<dyn FnMut() -> Result<(), EngineError> + Send>,
    /// Step 3: drop the wgpu `Device`/`Surface`/compositor resources tied to
    /// the dead device.
    pub teardown_gpu: Box<dyn FnMut() -> Result<(), EngineError> + Send>,
    /// Step 4: recreate in the OPPOSITE order (D3D11VA device → widened
    /// frames ctx → wgpu device → swapchain/surface → compositor resources)
    /// and hand back the components the coordinator verifies.
    pub recreate: Box<dyn FnMut() -> Result<RecreatedComponents, EngineError> + Send>,
    /// Step 5: position the rebuilt decode side at the restored playhead. The
    /// coordinator — not this hook — owns the PAUSED contract: there is no
    /// play/resume path anywhere in `recover`, so playback can only continue
    /// when the user (or the shell's transport) explicitly asks afterwards.
    pub restore_playhead: Box<dyn FnMut(i64) -> Result<(), EngineError> + Send>,
    /// Step 6: restart the decode/present threads (against the rebuilt state).
    pub restart_threads: Box<dyn FnMut() -> Result<(), EngineError> + Send>,
}

/// What a completed recovery proved (the forced-loss test's assertion
/// surface).
#[derive(Debug)]
pub struct RecoveryReport {
    /// The six `device_lost: step<n> …` lines, in execution order.
    pub steps: Vec<String>,
    /// The rebuilt hw frame pool's size (see [`RecreatedComponents::pool_size`]).
    pub recreated_pool_size: usize,
    /// The LUID the step-4 re-assert proved shared between the recreated
    /// D3D11 decode device and the recreated wgpu device.
    pub reasserted_luid: i64,
    /// The playhead position recovery restored to.
    pub restored_playhead_us: i64,
    /// Always `true` by CONSTRUCTION, not by convention: `recover` has no
    /// play/resume path at all (the locked decision — resume PAUSED, never
    /// mid-frame), so a completed recovery is necessarily paused.
    pub resumed_paused: bool,
}

/// The coordinator owning the recreate hooks/handles for the four components
/// (device+surface, compositor resources, hw frame pool/session,
/// swapchain-config) plus the live-device handles the injection seam needs.
pub struct RecoveryPlan {
    /// The LIVE wgpu device (the one a loss would kill). `Option` because
    /// recovery step 3 must RELEASE it (measured on this machine, recorded in
    /// 48-10-SUMMARY.md): while ANY handle to a removed D3D12 device is still
    /// alive in the process, DXGI hides the hardware adapter from fresh
    /// enumeration — a step-4 recreate would silently land on WARP and the
    /// LUID re-assert refuses it. Updated from [`RecreatedComponents`] after a
    /// successful recovery so the seam can re-fire against the rebuilt device.
    live_device: Option<wgpu::Device>,
    live_queue: Option<wgpu::Queue>,
    hooks: RecoveryHooks,
    /// The single-entry guard (threat T-48-10-01): all detection funnels race
    /// to ONE recovery; a failed recovery leaves it ENGAGED (fail closed — no
    /// competing retry can run against half-recreated state).
    recovering: Arc<AtomicBool>,
}

impl RecoveryPlan {
    pub fn new(live_device: wgpu::Device, live_queue: wgpu::Queue, hooks: RecoveryHooks) -> Self {
        Self {
            live_device: Some(live_device),
            live_queue: Some(live_queue),
            hooks,
            recovering: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The shared recovering flag, for external detection funnels that want
    /// to check/annotate state (the shell keeps its own managed twin).
    pub fn recovering_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.recovering)
    }

    /// The forced-loss injection seam (CONTEXT D-13 route (a)) —
    /// **PROBE_RESULT=works branch**: QI `ID3D12Device5` from the LIVE wgpu
    /// device (`as_hal`, the 48-03 probe's exact shape) and call
    /// `RemoveDevice()` — a REAL device removal driving wgpu's own
    /// `into_device_result` → `DeviceError::Lost` path, so recovery is invoked
    /// from the SAME detection callback a genuine TDR would fire.
    ///
    /// Honors the 48-03 MEASURED constraint: the traffic encoder is created
    /// BEFORE the trigger (post-remove `create_command_encoder` is a native,
    /// uncatchable STATUS_ACCESS_VIOLATION); only submit + poll — both
    /// probe-measured safe — touch the removed device afterward.
    pub fn simulate_device_lost(&mut self) -> Result<(), EngineError> {
        let (live_device, live_queue) = match (&self.live_device, &self.live_queue) {
            (Some(d), Some(q)) => (d, q),
            _ => {
                return Err(EngineError::Gpu(
                    "no live device armed (mid-recovery?) — cannot inject a loss".into(),
                ))
            }
        };
        inject_forced_device_loss(live_device, live_queue)
    }

    /// Run the coordinated recovery — the Pattern-5 six-step order VERBATIM,
    /// each step logged with the stable `device_lost: step<n>` prefix (the
    /// forced-loss test asserts the ORDER off [`RecoveryReport::steps`]).
    ///
    /// Single-entry: a second call while one is in flight (or after a FAILED
    /// recovery — fail closed, never retry against half-recreated state)
    /// returns an error without touching anything.
    pub fn recover(&mut self, playhead_us: i64) -> Result<RecoveryReport, EngineError> {
        if self.recovering.swap(true, Ordering::SeqCst) {
            return Err(EngineError::Gpu(
                "device-lost recovery already in progress (or a previous recovery failed and \
                 the guard is engaged) — refusing a competing recovery"
                    .into(),
            ));
        }

        let mut steps: Vec<String> = Vec::with_capacity(6);
        // The tag is a LITERAL per call (never interpolated) so the stable
        // `device_lost: step<n>` prefixes are grep-able in this source text
        // exactly as the plan's acceptance criteria check them.
        let step = |steps: &mut Vec<String>, tag: &str, what: &str| {
            let line = format!("device_lost: {tag} {what}");
            eprintln!("{line}");
            steps.push(line);
        };

        step(
            &mut steps,
            "step1",
            "stop the decode thread and the present/producer threads",
        );
        (self.hooks.stop_threads)()?;

        step(
            &mut steps,
            "step2",
            "tear down the FFmpeg side FIRST: av_buffer_unref(hw_frames_ctx) then \
             av_buffer_unref(hw_device_ctx), dropping the AVCodecContext (session drop \
             encapsulates the unref order)",
        );
        (self.hooks.teardown_hw_pool)()?;

        step(
            &mut steps,
            "step3",
            "drop the wgpu device/surface/compositor resources tied to the dead device",
        );
        (self.hooks.teardown_gpu)()?;
        // Including the coordinator's OWN handles — MEASURED requirement, not
        // hygiene: while any handle to the removed D3D12 device lives, DXGI
        // hides the hardware adapter from fresh enumeration in this process,
        // and step 4 would silently recreate on WARP (the LUID re-assert
        // caught exactly that while this plan was built; 48-10-SUMMARY.md).
        self.live_device = None;
        self.live_queue = None;

        step(
            &mut steps,
            "step4",
            "recreate in the OPPOSITE order: D3D11VA device -> widened hw frames ctx -> \
             wgpu device (same-adapter LUID re-assert) -> swapchain/surface -> compositor \
             resources",
        );
        let components = (self.hooks.recreate)()?;
        // The re-assert is run BY the coordinator on the recreated handles —
        // structural, not a hook convention.
        let reasserted_luid =
            assert_same_adapter_luid(&components.d3d11_device, &components.wgpu_device)?;
        eprintln!(
            "device_lost: step4 LUID re-assert passed — shared adapter {} (pool recreated at \
             size {})",
            luid_hex(reasserted_luid),
            components.pool_size
        );

        step(
            &mut steps,
            "step5",
            "restore the playhead position, resume PAUSED (never mid-frame), surface the \
             non-modal notice",
        );
        (self.hooks.restore_playhead)(playhead_us)?;

        step(&mut steps, "step6", "restart the decode/present threads");
        (self.hooks.restart_threads)()?;

        // Re-arm the seam against the rebuilt device (a second forced loss
        // must be injectable against the NEW device, exactly like a second
        // real TDR would hit it).
        self.live_device = Some(components.wgpu_device);
        self.live_queue = Some(components.wgpu_queue);

        self.recovering.store(false, Ordering::SeqCst);
        Ok(RecoveryReport {
            steps,
            recreated_pool_size: components.pool_size,
            reasserted_luid,
            restored_playhead_us: playhead_us,
            resumed_paused: true,
        })
    }
}

// ---------------------------------------------------------------------------
// The PRODUCTION driver (Phase 63, plan 63-01 — TRUST-01).
// ---------------------------------------------------------------------------

/// The halves of the six recovery hooks a HOST owns, layered onto the engine's
/// defaults (Phase 63, plan 63-01).
///
/// [`RecoveryPlan`] takes all six hooks and has no opinion about who writes
/// them; that is exactly why it sat for four phases with zero production
/// callers — nobody owned all six. This type splits them the way the codebase
/// actually splits: the **GPU / hw-pool halves are the engine's** and get real
/// defaults here, while the **thread choreography and the live surface are the
/// host's** and stay caller-supplied.
///
/// The default is therefore genuinely usable at the ENGINE tier (no threads to
/// stop, nothing but the compositor to rebuild) and plan 63-02 layers the FFI
/// present-thread + `SwapChainPanel` choreography on top by filling these
/// fields in — without editing `compositor.rs` or this coordinator again.
/// Step 4's GPU-half hook: hand back a REBUILT presentation-capable
/// compositor. Named (rather than written inline) so the field below reads as
/// prose and so this and its decode twin are referable from plan 63-02.
pub type RecreateDeviceHook =
    Box<dyn FnMut() -> Result<crate::Compositor, EngineError> + Send>;

/// Step 4's decode-half hook: hand back the new `ID3D11Device` for the
/// same-adapter LUID re-assert, plus the rebuilt hw FRAME pool's size (see
/// [`RecreatedComponents::pool_size`] for what `0` reserves).
pub type RecreateDecodeHook =
    Box<dyn FnMut() -> Result<(ID3D11Device, usize), EngineError> + Send>;

pub struct RecoveryHostHooks {
    /// Step 1. Default: no-op. The engine owns no threads; the real
    /// present/decode threads live in `crates/ffi` and `crates/preview`.
    pub stop_threads: Box<dyn FnMut() -> Result<(), EngineError> + Send>,
    /// Step 5. Default: no-op. The coordinator — not this hook — owns the
    /// PAUSED contract ([`RecoveryPlan::recover`] has no play path at all), so
    /// a host that supplies nothing still resumes paused, never mid-frame.
    pub restore_playhead: Box<dyn FnMut(i64) -> Result<(), EngineError> + Send>,
    /// Step 6. Default: no-op (see `stop_threads`).
    pub restart_threads: Box<dyn FnMut() -> Result<(), EngineError> + Send>,
    /// Step 4's GPU half. `None` = the engine default: rebuild through
    /// [`crate::Compositor::rebuild_from_birth`], i.e. the SAME `build` funnel
    /// the original came out of, so the uncaptured-error guard and GPU-04
    /// detection re-arm on the recreated device by construction.
    ///
    /// A [`crate::CompositorBirth::HostSurface`] compositor MUST
    /// supply this: its `wgpu::Surface` belongs to the shell's
    /// `SwapChainPanel`, which the engine cannot see, let alone recreate
    /// (CONTEXT D-03 — plan 63-02). [`PreviewRecovery::adopt_with`] refuses
    /// such a compositor UP FRONT rather than at step 4, because a step-4
    /// failure happens *after* step 3 has torn the old device down: it would
    /// trade "preview is dead until restart" for "preview is dead and the
    /// compositor is gone too", with the single-entry guard latched closed.
    pub recreate_device: Option<RecreateDeviceHook>,
    /// Step 4's DECODE half — the new D3D11VA device for the same-adapter LUID
    /// re-assert, plus the rebuilt hw FRAME pool's size.
    ///
    /// `None` = the engine default: acquire a fresh pooled D3D11VA device via
    /// [`crate::hwdecode::acquire_pooled_d3d11_device`] (step 2 released the
    /// dead master, so this genuinely creates one) and report pool size `0`,
    /// meaning **no hw frame pool was live under this coordinator** — frame
    /// pools belong to [`crate::HwDecodeSession`]s, which the compositor does
    /// not own. A host that DOES own sessions (the ring; plan 63-02) supplies
    /// this and reports the real re-widened `HwDecodeSession::pool_size`, which
    /// is what makes the T-48-10-02 "silently reverted to FFmpeg's default 17"
    /// assertion meaningful.
    pub recreate_decode: Option<RecreateDecodeHook>,
}

impl Default for RecoveryHostHooks {
    fn default() -> Self {
        Self {
            stop_threads: Box::new(|| Ok(())),
            restore_playhead: Box::new(|_| Ok(())),
            restart_threads: Box::new(|| Ok(())),
            recreate_device: None,
            recreate_decode: None,
        }
    }
}

/// **[`RecoveryPlan::recover`]'s production caller** (Phase 63, plan 63-01 —
/// TRUST-01, CONTEXT D-02).
///
/// # What was wrong, in one paragraph
///
/// `v8.0-MILESTONE-AUDIT` § 2a: this module was fully implemented and fully
/// tested and had **zero production callers**, so a real driver TDR left
/// preview dead for the life of the process. Quick `260829-n96` restored
/// DETECTION at the device-birth site in `Compositor::build`; the response
/// degraded decode and stopped there, because nobody owned the six recovery
/// hooks. This type owns the four the engine can own and demands the other two
/// from its host, which is what finally makes recreation reachable from
/// production construction.
///
/// # The request/run split
///
/// The device-birth response REQUESTS recovery
/// ([`crate::Compositor::device_lost_pending`]); it never runs it. wgpu
/// delivers the device-lost callback on its own internal thread and
/// [`RecoveryPlan::recover`] tears down the very device that callback belongs
/// to. So the shape is: **poll [`PreviewRecovery::device_lost_pending`] from
/// the thread that presents, and call
/// [`PreviewRecovery::recover_after_device_lost`] there.**
///
/// # Why it OWNS the compositor
///
/// Not encapsulation taste — the MEASURED constraint recorded on
/// [`RecoveryPlan::live_device`]: while ANY handle to a removed D3D12 device
/// lives, DXGI hides the hardware adapter from fresh enumeration in this
/// process, and step 4 then silently recreates on WARP (where step 4's own LUID
/// re-assert refuses it, turning a recoverable TDR into a hard failure). Every
/// handle therefore lives inside this coordinator, which drops them in step 3
/// itself. Callers reach the compositor through
/// [`PreviewRecovery::with_compositor`], which cannot leak one past a recovery.
pub struct PreviewRecovery {
    /// The live compositor. `Option` so step 3 can genuinely DROP it, and
    /// `Arc<Mutex<..>>` because the step-3 and step-4 hooks are `'static`
    /// boxed closures owned by the plan.
    live: Arc<Mutex<Option<crate::Compositor>>>,
    plan: RecoveryPlan,
}

impl PreviewRecovery {
    /// Adopt a compositor built through the production funnel, with the
    /// engine's default hooks ([`RecoveryHostHooks::default`]).
    ///
    /// Errors if the compositor is not presentation-capable (no detection was
    /// armed at its birth — export/offscreen, GPU-07) or if it is a
    /// [`crate::CompositorBirth::HostSurface`] compositor, which needs
    /// `recreate_device` and therefore [`PreviewRecovery::adopt_with`].
    pub fn adopt(compositor: crate::Compositor) -> Result<Self, EngineError> {
        Self::adopt_with(compositor, RecoveryHostHooks::default())
    }

    /// [`PreviewRecovery::adopt`] with the host's halves of the six hooks
    /// filled in (plan 63-02's entry point).
    pub fn adopt_with(
        compositor: crate::Compositor,
        host: RecoveryHostHooks,
    ) -> Result<Self, EngineError> {
        if compositor.device_lost_signal().is_none() {
            return Err(EngineError::Gpu(
                "this compositor armed no device-lost detection at its birth — it is an \
                 export/offscreen device (GPU-07), which has no preview to recover"
                    .into(),
            ));
        }
        let birth = compositor.birth();
        let RecoveryHostHooks {
            stop_threads,
            restore_playhead,
            restart_threads,
            mut recreate_device,
            mut recreate_decode,
        } = host;

        // REFUSE UP FRONT, never at step 4. Step 4 runs AFTER step 3 has torn
        // the old device down, so an unrecreatable compositor discovered there
        // would leave the host with no compositor at all and the single-entry
        // guard latched closed (fail-closed, by design). Discovering it here
        // costs nothing and leaves the caller its working preview.
        if birth == crate::CompositorBirth::HostSurface && recreate_device.is_none() {
            return Err(EngineError::Gpu(
                "a live-surface compositor cannot be recovered by the engine alone: its \
                 wgpu::Surface belongs to the host's SwapChainPanel. Supply \
                 RecoveryHostHooks::recreate_device (plan 63-02's present-loop choreography) \
                 — refusing now rather than after step 3 has torn the device down"
                    .into(),
            ));
        }

        let live_device = compositor.device().clone();
        let live_queue = compositor.queue().clone();
        let live = Arc::new(Mutex::new(Some(compositor)));

        let hooks = RecoveryHooks {
            stop_threads,
            // Step 2 — the FFmpeg side FIRST. The engine owns no session here
            // (sessions belong to the ring/preview callers and die with their
            // own owners), but it DOES own the process-wide pooled D3D11VA
            // device master, which no session drop can reach: leaving it would
            // hand the dead device to every reopened session (59.1-03).
            teardown_hw_pool: Box::new(|| {
                crate::clear_pooled_device();
                Ok(())
            }),
            // Step 3 — drop the wgpu device/surface/compositor resources tied
            // to the dead device. This is THE handle release the step-3 note
            // exists for.
            teardown_gpu: {
                let live = Arc::clone(&live);
                Box::new(move || {
                    let had = live
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .is_some();
                    eprintln!(
                        "device_lost: step3 released the coordinator's compositor \
                         (device+queue+pipelines): had_live={had}"
                    );
                    Ok(())
                })
            },
            // Step 4 — recreate in the OPPOSITE order: decode device first,
            // then the wgpu device/surface/compositor resources.
            recreate: {
                let live = Arc::clone(&live);
                Box::new(move || {
                    let (d3d11_device, pool_size) = match recreate_decode.as_mut() {
                        Some(host) => host()?,
                        None => {
                            let device =
                                crate::hwdecode::acquire_pooled_d3d11_device().map_err(|e| {
                                    EngineError::Gpu(format!(
                                        "recovery could not recreate the D3D11VA decode \
                                         device: {e}"
                                    ))
                                })?;
                            // 0 = no hw FRAME pool was live under this
                            // coordinator (see `recreate_decode`'s doc). Said
                            // out loud rather than reported as a plausible
                            // number nobody measured.
                            (device, 0usize)
                        }
                    };
                    let compositor = match recreate_device.as_mut() {
                        Some(host) => host()?,
                        // THROUGH THE SAME BUILD FUNNEL — which is what re-arms
                        // the uncaptured-error guard and GPU-04 detection on
                        // the new device with no second arming site.
                        None => crate::Compositor::rebuild_from_birth(birth)?,
                    };
                    let out = RecreatedComponents {
                        pool_size,
                        d3d11_device,
                        wgpu_device: compositor.device().clone(),
                        wgpu_queue: compositor.queue().clone(),
                    };
                    *live.lock().unwrap_or_else(|e| e.into_inner()) = Some(compositor);
                    Ok(out)
                })
            },
            restore_playhead,
            restart_threads,
        };

        Ok(Self {
            live,
            plan: RecoveryPlan::new(live_device, live_queue, hooks),
        })
    }

    /// Has a loss been detected and coordinated recovery been REQUESTED?
    ///
    /// Poll this from the thread that presents. `true` while no compositor is
    /// live at all (step 3 has run and step 4 has not finished): a recovery in
    /// flight is the strongest form of pending there is, and reporting `false`
    /// there would invite a caller to present into a hole.
    pub fn device_lost_pending(&self) -> bool {
        match &*self.live.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(compositor) => compositor.device_lost_pending(),
            None => true,
        }
    }

    /// **Run the coordinated six-step recovery** — [`RecoveryPlan::recover`],
    /// from production.
    ///
    /// Call from a thread that may legally tear the device down (never from the
    /// device-lost callback). Single-entry and fail-closed: a second call while
    /// one is in flight, or after a FAILED recovery, returns an error without
    /// touching anything (threat T-63-01 — a repeated-TDR storm degrades to the
    /// pre-recovery detect-and-degrade behaviour, never a recreate loop).
    ///
    /// On success the coordinator holds a REBUILT compositor whose device was
    /// born through the same `build` funnel, so detection and the
    /// uncaptured-error guard are armed on it and a SECOND loss recovers
    /// exactly like the first.
    pub fn recover_after_device_lost(
        &mut self,
        playhead_us: i64,
    ) -> Result<RecoveryReport, EngineError> {
        self.plan.recover(playhead_us)
    }

    /// Borrow the live compositor for the duration of `f` (present, composite,
    /// configure). `Err` while a recovery is in flight.
    ///
    /// Deliberately a scoped borrow rather than a handle: a `Compositor` (or
    /// `Arc<Compositor>`) that outlived a recovery would keep a handle to the
    /// removed D3D12 device alive, and step 4 would then recreate on WARP. See
    /// the type doc.
    pub fn with_compositor<T>(
        &self,
        f: impl FnOnce(&crate::Compositor) -> T,
    ) -> Result<T, EngineError> {
        let guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let compositor = guard.as_ref().ok_or_else(|| {
            EngineError::Gpu(
                "no live compositor — a device-lost recovery is in flight (step 3 has released \
                 the old device and step 4 has not finished)"
                    .into(),
            )
        })?;
        Ok(f(compositor))
    }

    /// The shared single-entry recovering flag ([`RecoveryPlan::recovering_flag`]),
    /// for a host that wants to annotate its own state.
    pub fn recovering_flag(&self) -> Arc<AtomicBool> {
        self.plan.recovering_flag()
    }

    /// Wait up to `timeout` for the device-lost callback armed at the live
    /// compositor's birth to fire.
    ///
    /// Polls the live device while waiting: `poll` is one of the two
    /// probe-measured-safe post-removal operations (48-03) and is what drives
    /// wgpu's maintain path to OBSERVE the removal and deliver the callback —
    /// without it a headless caller can wait forever for an event that only a
    /// running present loop would have collected. `None` if nothing arrived, or
    /// if a recovery is already in flight.
    pub fn wait_for_device_lost(&self, timeout: Duration) -> Option<DeviceLostEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
                let compositor = guard.as_ref()?;
                if let Some(event) = compositor.device_lost_signal().and_then(|s| s.observed()) {
                    return Some(event);
                }
                let _ = compositor.device().poll(wgpu::PollType::Poll);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// TEST/PROOF ONLY: force a REAL device removal on the compositor this
    /// coordinator owns ([`inject_forced_device_loss`], the
    /// PROBE_RESULT=works branch), so the whole production chain —
    /// birth-site callback → degrade → recovery request → coordinated
    /// recreation → a frame presenting again — is drivable in one process with
    /// no driver TDR and no reboot.
    ///
    /// Lives here rather than on the caller so the injection reaches the
    /// coordinator's own handles: a caller holding its own device clone to
    /// inject with would keep the removed device alive past step 3 and send
    /// step 4 to WARP.
    #[doc(hidden)]
    pub fn inject_forced_device_loss(&mut self) -> Result<(), EngineError> {
        self.plan.simulate_device_lost()
    }
}

// ---------------------------------------------------------------------------
// The wgpu panic-feature manifest guard (48-RESEARCH.md Pitfall 5).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// If wgpu-hal's `device_lost_panic` or `internal_error_panic` opt-in
    /// features are EVER enabled (directly or via a well-intentioned feature
    /// tweak), `into_device_result` PANICS instead of returning
    /// `DeviceError::Lost` — recovery never runs and the process just dies.
    /// This guard greps the real manifest text so turning either on is a red
    /// gate, not a silent behavior change.
    #[test]
    fn wgpu_panic_features_stay_off() {
        let manifest = include_str!("../Cargo.toml");
        // Positive anchor: this IS the manifest with the wgpu dependency.
        assert!(
            manifest.contains("wgpu = \"26\""),
            "engine Cargo.toml no longer carries the bare `wgpu = \"26\"` line — re-point this \
             guard at the real wgpu dependency declaration"
        );
        assert!(
            !manifest.contains("device_lost_panic"),
            "crates/engine/Cargo.toml mentions device_lost_panic — with that wgpu-hal feature \
             on, DXGI_ERROR_DEVICE_REMOVED panics instead of surfacing DeviceError::Lost and \
             GPU-04 recovery NEVER runs (48-RESEARCH.md Pitfall 5)"
        );
        assert!(
            !manifest.contains("internal_error_panic"),
            "crates/engine/Cargo.toml mentions internal_error_panic — with that wgpu-hal \
             feature on, internal device errors panic instead of surfacing as recoverable \
             errors and GPU-04 recovery NEVER runs (48-RESEARCH.md Pitfall 5)"
        );
    }
}
