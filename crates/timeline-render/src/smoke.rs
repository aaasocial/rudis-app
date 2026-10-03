//! The D-05 tripwire's own C ABI — the *measurement* half of plan 52-01.
//!
//! This is not a rendering API. It exists to answer one question with NUMBERS:
//! **do two independent `SwapChainPanel`-backed `wgpu` devices coexist in one real
//! WinUI 3 process without stealing frames from each other, corrupting, or dying?**
//!
//! Each [`rudis_timeline_smoke_attach`] call builds one [`TimelineSurface`] on the
//! CALLER'S thread (the panel's UI thread — the THREAD RULE in [`crate::surface`]) and
//! then hands it to a dedicated present thread that loops `present_solid` and records the
//! wall-clock delta between consecutive presents. Those deltas are the ablation's whole
//! evidence base: with the second panel off, panel A's p50 delta is one number; with it
//! on, it is another; a stolen frame shows up as a shifted percentile, not as an
//! impression of smoothness.
//!
//! Every export catches unwinds. A panic crossing an `extern "C"` boundary is undefined
//! behaviour, and this DLL is loaded by the CLR (threat T-52-05). `crates/ffi` sets this
//! discipline with its `ffi_guard!` macro; the BEHAVIOUR is mirrored here rather than the
//! macro imported, because this crate deliberately links nothing from the shipping ABI.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::surface::{TimelineError, TimelineSurface};

/// Success.
pub const RC_OK: i32 = 0;
/// A null handle or null out-pointer was passed.
pub const RC_NULL_ARG: i32 = -20;
/// A panic was caught at the boundary rather than unwound into the caller.
pub const RC_PANIC: i32 = -99;

use crate::trace;

/// How many inter-present deltas are retained. At Fifo/60Hz a phase of the ablation is
/// ~600 samples, so this is three orders of magnitude of headroom — and it is BOUNDED, so
/// a smoke window left open overnight cannot grow memory without limit (threat T-52-03).
const MAX_SAMPLES: usize = 300_000;

/// The numbers the ablation reads. `#[repr(C)]`, and the C# `SmokeStats` mirrors this
/// field order EXACTLY: five `u64` then two `u32`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SmokeStats {
    /// Frames successfully presented since the last reset.
    pub frames_presented: u64,
    /// Smallest observed gap between consecutive presents, microseconds.
    pub min_delta_us: u64,
    /// Median gap. THE headline number: under Fifo this is the display's frame period
    /// when nothing is being stolen.
    pub p50_delta_us: u64,
    /// 99th-percentile gap — where an occasional stolen frame would show up first.
    pub p99_delta_us: u64,
    /// Largest observed gap.
    pub max_delta_us: u64,
    /// Recoverable present failures (timeout / other).
    pub present_errors: u32,
    /// Lost-or-outdated surface events. Non-zero here is a NO-GO signal, not noise.
    pub device_lost: u32,
}

/// The present thread's accumulator.
struct Recorder {
    deltas: Vec<u64>,
    frames: u64,
    present_errors: u32,
    device_lost: u32,
    /// Skipped frames because the window was minimised or fully covered. Deliberately
    /// NOT a field of [`SmokeStats`] — that struct's seven-field layout is the ABI the
    /// C# side mirrors by hand, and occlusion is a window-manager fact, not a GPU-
    /// contention one. It is surfaced through [`rudis_timeline_smoke_describe`] so it is
    /// still auditable rather than swallowed.
    occluded: u64,
    /// Set by a reset; makes the present loop drop the first delta AFTER the reset so a
    /// span crossing the reset boundary is never counted as a frame time.
    restart: bool,
}

impl Recorder {
    fn new() -> Self {
        Self {
            deltas: Vec::with_capacity(4096),
            frames: 0,
            present_errors: 0,
            device_lost: 0,
            occluded: 0,
            restart: true,
        }
    }

    fn clear(&mut self) {
        self.deltas.clear();
        self.frames = 0;
        self.present_errors = 0;
        self.device_lost = 0;
        self.occluded = 0;
        self.restart = true;
    }

    fn snapshot(&self) -> SmokeStats {
        let mut sorted = self.deltas.clone();
        sorted.sort_unstable();
        let pick = |q: f64| -> u64 {
            if sorted.is_empty() {
                return 0;
            }
            let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
            sorted[idx]
        };
        SmokeStats {
            frames_presented: self.frames,
            min_delta_us: sorted.first().copied().unwrap_or(0),
            p50_delta_us: pick(0.50),
            p99_delta_us: pick(0.99),
            max_delta_us: sorted.last().copied().unwrap_or(0),
            present_errors: self.present_errors,
            device_lost: self.device_lost,
        }
    }
}

/// One attached panel: its present thread, its stop flag, and its counters.
pub struct SmokeHandle {
    stop: Arc<AtomicBool>,
    reset: Arc<AtomicBool>,
    recorder: Arc<Mutex<Recorder>>,
    thread: Option<JoinHandle<()>>,
    /// The surface, SHARED with the present thread rather than owned by it.
    ///
    /// Shared specifically so that it OUTLIVES the join: the panel must be unbound while
    /// the `wgpu` objects (and therefore the DXGI factory) are still alive, and only then
    /// may the device be destroyed. See the teardown order documented on
    /// [`crate::surface::unbind_swap_chain_panel`] — dropping the surface first crashes.
    surface: Option<Arc<TimelineSurface>>,
    /// The QI'd `ISwapChainPanelNative*`, kept HERE rather than only inside the surface
    /// because unbinding and releasing the panel must happen on the UI thread (the detach
    /// caller), in a specific order relative to the surface's own destruction.
    panel_native: *mut c_void,
    /// Recorded at attach so the artifact can name the adapter and format actually used.
    describe: String,
}

impl SmokeHandle {
    fn stats(&self) -> SmokeStats {
        match self.recorder.lock() {
            Ok(guard) => guard.snapshot(),
            // A poisoned mutex means the present thread panicked. Report it as a present
            // error rather than propagating: the tripwire's job is to RECORD failure.
            Err(poisoned) => {
                let mut stats = poisoned.into_inner().snapshot();
                stats.present_errors = stats.present_errors.saturating_add(1);
                stats
            }
        }
    }
}

impl Drop for SmokeHandle {
    fn drop(&mut self) {
        // Order is load-bearing and was learned the hard way (see
        // release_swap_chain_panel's docs — the first tripwire run measured cleanly and
        // then crashed in D3D12Core.dll at process exit):
        //   1. stop and JOIN the present thread, which drops the wgpu surface/device;
        //   2. only THEN unbind the panel from its (now orphaned) DXGI swapchain.
        // Reversing these would unbind a swapchain the present thread is still using.
        // ORDER IS LOAD-BEARING. Both wrong orders were measured and both crash — the
        // full account is on crate::surface::unbind_swap_chain_panel. Briefly:
        //   1. stop and join the present thread, so nothing is presenting;
        //   2. unbind the panel WHILE the wgpu objects are still alive (skip this and the
        //      panel keeps an orphaned swapchain that faults at process exit);
        //   3. NOW drop the surface, device and instance (do this before step 2 and the
        //      LAST panel's SetSwapChain access-violates, because the DXGI factory went
        //      with the last instance);
        //   4. release this crate's reference to the panel.
        trace("drop:enter");
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        trace("drop:present-thread-joined");

        // SAFETY: `panel_native` came from a successful QI in TimelineSurface::attach.
        // Drop runs on the thread that called rudis_timeline_smoke_detach — the panel's
        // UI thread, as SetSwapChain's contract requires.
        trace("drop:unbind-panel");
        unsafe { crate::surface::unbind_swap_chain_panel(self.panel_native) };

        trace("drop:release-surface");
        drop(self.surface.take());
        trace("drop:surface-released");

        // SAFETY: released exactly once, after the last use of the pointer.
        unsafe { crate::surface::release_panel_native(self.panel_native) };
        self.panel_native = std::ptr::null_mut();
        trace("drop:done");
    }
}

/// The present loop. Owns the surface for the rest of the handle's life.
fn present_loop(
    surface: Arc<TimelineSurface>,
    colour: [f64; 4],
    stop: Arc<AtomicBool>,
    reset: Arc<AtomicBool>,
    recorder: Arc<Mutex<Recorder>>,
) {
    let mut last = Instant::now();
    let mut have_last = false;

    while !stop.load(Ordering::Acquire) {
        if reset.swap(false, Ordering::AcqRel) {
            if let Ok(mut guard) = recorder.lock() {
                guard.clear();
            }
            have_last = false;
        }

        let outcome = surface.present_solid(colour);
        let now = Instant::now();

        let Ok(mut guard) = recorder.lock() else {
            // Poisoned by a previous panic; nothing useful left to record.
            return;
        };
        match outcome {
            Ok(()) => {
                guard.frames += 1;
                if have_last && !guard.restart {
                    let delta = now.duration_since(last).as_micros() as u64;
                    if guard.deltas.len() < MAX_SAMPLES {
                        guard.deltas.push(delta);
                    }
                }
                guard.restart = false;
                have_last = true;
                last = now;
            }
            Err(TimelineError::Occluded) => {
                // NOT an error and NOT a frame. wgpu's own guidance for an occluded
                // surface is "skip the frame and try again", so counting it either way
                // would put a window-manager event into a GPU-contention measurement.
                // It is still visible in the numbers: an occluded phase presents far
                // fewer frames than its 10-second window allows, and the artifact states
                // the expected count so a shortfall cannot pass unnoticed.
                guard.occluded = guard.occluded.saturating_add(1);
                have_last = false;
                drop(guard);
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            Err(TimelineError::DeviceLost) => {
                guard.device_lost = guard.device_lost.saturating_add(1);
                have_last = false;
                drop(guard);
                // Do not spin hot on a lost surface.
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            Err(_) => {
                guard.present_errors = guard.present_errors.saturating_add(1);
                have_last = false;
                drop(guard);
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        }
    }

    // Only THIS thread's Arc clone goes away here. The surface itself outlives the join
    // because the handle holds the other clone — see SmokeHandle::drop's step 3.
    trace("present-loop:exited");
    drop(surface);
    trace("present-loop:reference-dropped");
}

// ---------------------------------------------------------------------------
// The four exports. Every body is wrapped in catch_unwind (threat T-52-05).
// ---------------------------------------------------------------------------

/// Attach an independent `wgpu` device to `panel` and start presenting `(r, g, b)` at 100%
/// alpha on a dedicated thread.
///
/// **MUST be called on the panel's UI thread** — see [`crate::surface`]'s THREAD RULE.
///
/// Returns an opaque handle, or null on any failure. The handle must be released with
/// [`rudis_timeline_smoke_detach`].
///
/// # Why the tripwire carries a `scale` too (plan 52-12)
///
/// `width_px`/`height_px` are PHYSICAL pixels here exactly as they are on the shipping
/// path, so the tripwire's swapchain needs the same composition-scale compensation. A
/// flat clear colour would hide a missing transform perfectly — which is precisely why
/// leaving this export scale-free would put a THIRD uncompensated `SwapChainPanel`
/// surface in the repository, in the one place built to prove the hosting link is sound.
/// The caller already computes the panel's `CompositionScaleX`; it now passes it.
///
/// # Safety
/// `panel` must be a valid `SwapChainPanel` COM pointer (an `IInspectable*` is fine — it
/// is `QueryInterface`d here) or null.
#[no_mangle]
pub extern "C" fn rudis_timeline_smoke_attach(
    panel: *mut c_void,
    width_px: u32,
    height_px: u32,
    scale: f32,
    r: f32,
    g: f32,
    b: f32,
) -> *mut SmokeHandle {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let surface = match TimelineSurface::attach(panel, width_px, height_px, scale) {
            Ok(surface) => surface,
            Err(_) => return std::ptr::null_mut(),
        };
        let describe = format!(
            "adapter={} format={:?} size={}x{} scale={} present_mode=Fifo",
            surface.adapter_name(),
            surface.format(),
            width_px.max(1),
            height_px.max(1),
            scale
        );
        // Copied out BEFORE the surface is shared: the panel must be unbound and released
        // by the UI thread at detach, in a specific order around the surface's own drop.
        let panel_native = surface.panel_native();
        let surface = Arc::new(surface);

        let stop = Arc::new(AtomicBool::new(false));
        let reset = Arc::new(AtomicBool::new(false));
        let recorder = Arc::new(Mutex::new(Recorder::new()));

        let thread = {
            let stop = Arc::clone(&stop);
            let reset = Arc::clone(&reset);
            let recorder = Arc::clone(&recorder);
            let surface = Arc::clone(&surface);
            let colour = [r as f64, g as f64, b as f64, 1.0];
            std::thread::Builder::new()
                .name("rudis-timeline-smoke-present".to_owned())
                .spawn(move || present_loop(surface, colour, stop, reset, recorder))
        };
        let thread = match thread {
            Ok(thread) => thread,
            // No thread means no handle, so tear down here in the SAME order Drop uses:
            // unbind the panel while the surface lives, then drop it, then release.
            // SAFETY: UI thread, released once.
            Err(_) => {
                unsafe { crate::surface::unbind_swap_chain_panel(panel_native) };
                drop(surface);
                unsafe { crate::surface::release_panel_native(panel_native) };
                return std::ptr::null_mut();
            }
        };

        Box::into_raw(Box::new(SmokeHandle {
            stop,
            reset,
            recorder,
            thread: Some(thread),
            surface: Some(surface),
            panel_native,
            describe,
        }))
    }));

    result.unwrap_or(std::ptr::null_mut())
}

/// Copy the current counters into `out`.
///
/// Returns [`RC_OK`], [`RC_NULL_ARG`] or [`RC_PANIC`]. Safe from any thread.
///
/// # Safety
/// `handle` must be a live handle from [`rudis_timeline_smoke_attach`] (or null) and
/// `out` must point at writable [`SmokeStats`]-sized storage (or be null).
#[no_mangle]
pub extern "C" fn rudis_timeline_smoke_stats(
    handle: *mut SmokeHandle,
    out: *mut SmokeStats,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || out.is_null() {
            return RC_NULL_ARG;
        }
        let stats = unsafe { &*handle }.stats();
        unsafe { std::ptr::write(out, stats) };
        RC_OK
    }))
    .unwrap_or(RC_PANIC)
}

/// Zero the counters, so an ablation phase measures only its own window.
///
/// The first present after a reset is deliberately NOT counted as a delta — a span
/// straddling the reset would be an artefact, and an artefact in the p99 is exactly the
/// kind of number that gets explained away instead of investigated.
///
/// # Safety
/// `handle` must be a live handle from [`rudis_timeline_smoke_attach`], or null.
#[no_mangle]
pub extern "C" fn rudis_timeline_smoke_reset_stats(handle: *mut SmokeHandle) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return RC_NULL_ARG;
        }
        let handle = unsafe { &*handle };
        if let Ok(mut guard) = handle.recorder.lock() {
            guard.clear();
        }
        handle.reset.store(true, Ordering::Release);
        RC_OK
    }))
    .unwrap_or(RC_PANIC)
}

/// Stop the present thread, drop the device and swapchain, free the handle.
///
/// After this returns the panel is no longer bound to any swapchain — which is what makes
/// the ablation's "second panel OFF" phases real rather than merely paused.
///
/// # Safety
/// `handle` must be a handle from [`rudis_timeline_smoke_attach`] that has not already
/// been detached, or null. Never called twice on the same handle.
#[no_mangle]
pub extern "C" fn rudis_timeline_smoke_detach(handle: *mut SmokeHandle) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return RC_NULL_ARG;
        }
        // Reconstituting the Box runs SmokeHandle::drop: stop, join, then the surface and
        // device drop with it. No leaked device (threat T-52-03).
        drop(unsafe { Box::from_raw(handle) });
        RC_OK
    }))
    .unwrap_or(RC_PANIC)
}

/// A description of the adapter/format/size a handle actually negotiated, written into
/// `out` as NUL-terminated UTF-8 and truncated to `cap` bytes. Returns the number of bytes
/// written (excluding the NUL), or a negative code.
///
/// Not strictly needed to decide GO/NO-GO, but a verdict that cannot name the GPU it was
/// measured on is a weaker record than one that can.
///
/// # Safety
/// `handle` must be live or null; `out` must be writable for `cap` bytes or null.
#[no_mangle]
pub extern "C" fn rudis_timeline_smoke_describe(
    handle: *mut SmokeHandle,
    out: *mut u8,
    cap: u32,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || out.is_null() || cap == 0 {
            return RC_NULL_ARG;
        }
        let handle = unsafe { &*handle };
        let occluded = handle
            .recorder
            .lock()
            .map(|guard| guard.occluded)
            .unwrap_or(0);
        let full = format!("{} occluded_skips={}", handle.describe, occluded);
        let text = full.as_bytes();
        let n = text.len().min(cap as usize - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), out, n);
            std::ptr::write(out.add(n), 0u8);
        }
        n as i32
    }))
    .unwrap_or(RC_PANIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary contract: a null handle is a CODE, never a dereference. Exercised
    /// against the real exports, not a stand-in.
    #[test]
    fn null_handles_return_codes_not_crashes() {
        assert_eq!(
            rudis_timeline_smoke_stats(std::ptr::null_mut(), std::ptr::null_mut()),
            RC_NULL_ARG
        );
        assert_eq!(
            rudis_timeline_smoke_reset_stats(std::ptr::null_mut()),
            RC_NULL_ARG
        );
        assert_eq!(rudis_timeline_smoke_detach(std::ptr::null_mut()), RC_NULL_ARG);
        assert_eq!(
            rudis_timeline_smoke_describe(std::ptr::null_mut(), std::ptr::null_mut(), 0),
            RC_NULL_ARG
        );
    }

    /// A null panel must fail BEFORE any GPU work and yield a null handle — the same
    /// fail-fast the 44 spike proved, so a wrong pointer is never handed to wgpu.
    #[test]
    fn null_panel_yields_a_null_handle() {
        let handle = rudis_timeline_smoke_attach(std::ptr::null_mut(), 16, 16, 1.25, 1.0, 0.0, 0.0);
        assert!(handle.is_null());
    }

    /// The percentile maths, on a known input — because a p50 nobody checked is a number
    /// the verdict would rest on blindly.
    #[test]
    fn percentiles_are_computed_over_recorded_deltas() {
        let mut recorder = Recorder::new();
        recorder.deltas = (1..=100u64).collect();
        recorder.frames = 100;
        let stats = recorder.snapshot();
        assert_eq!(stats.frames_presented, 100);
        assert_eq!(stats.min_delta_us, 1);
        assert_eq!(stats.max_delta_us, 100);
        assert_eq!(stats.p50_delta_us, 51); // index round(99*0.5) = 50 -> value 51
        assert_eq!(stats.p99_delta_us, 99); // index round(99*0.99) = 98 -> value 99
    }

    /// An empty recorder must report zeros, not panic on an empty slice.
    #[test]
    fn empty_recorder_reports_zeros() {
        let stats = Recorder::new().snapshot();
        assert_eq!(stats.frames_presented, 0);
        assert_eq!(stats.p50_delta_us, 0);
        assert_eq!(stats.present_errors, 0);
        assert_eq!(stats.device_lost, 0);
    }

    /// The C# side mirrors this layout by hand; if the Rust struct ever grows a field,
    /// this pins the size so the mismatch is a red test rather than silent garbage.
    #[test]
    fn stats_layout_is_five_u64_then_two_u32() {
        assert_eq!(std::mem::size_of::<SmokeStats>(), 5 * 8 + 2 * 4);
    }
}
