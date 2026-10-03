//! `ShellPresentSink` — the C# shell's GPU OUTPUT adapter (Phase 51, plan
//! 51-02, task 2): the second implementation of the frozen
//! `preview::PresentSink` port.
//!
//! # Traceability rule for this file
//!
//! Every method names the `src-tauri/src/preview_host.rs` line range of the
//! `TauriPresentSink` method it twins, so the two can be diffed by inspection.
//! Keep it that way: the bodies differ ONLY in where the state lives
//! (`Arc<PreviewSurfaceState>` here, `AppHandle` + managed state there), never
//! in WHAT they do. `crates/preview`'s present loop drives both through the
//! same trait object, so a behavioural divergence is a preview-parity bug.
//!
//! ONE method breaks that rule and cannot obey it: `present_composited` (quick
//! 260803-cws) is the first method written AFTER Phase 55's cutover deleted
//! `src-tauri/`, so it has no `TauriPresentSink` twin to name and never will.
//! Every method that predates the cutover still carries its line range.
//!
//! # The two rules that hold on EVERY method
//!
//! 1. **A poisoned lock degrades to a skipped present, never a second unwind**
//!    (T-51-03). No `.unwrap()`/`.expect()` on any lock in this file.
//! 2. **No panel attached is a silent no-op**, exactly as `TauriPresentSink`
//!    degrades under the mock runtime — asserted by this module's tests rather
//!    than assumed. The distinction the shell host CAN make, and does: an
//!    UNATTACHED instance answers `Ok(false)` (nothing to present to), while an
//!    ATTACHED one whose GPU set is momentarily `None` answers `Err` — that is
//!    WR-01's device-lost release→rebuild window, and the caller must see it.
//!
//! # `note_presented_stamp` is deliberately NOT overridden
//!
//! The trait carries a default no-op body (Phase 49, instrumentation only).
//! `RecordingPresentSink` overrides it because a landing-latency harness
//! consumes the stamps; the C# host has no consumer for them — the shell polls
//! `rudis_get_playback_position` instead — so taking the default is the correct
//! twin of "this capability has no reader here", not an omission.

use super::state::{PreviewGpu, PreviewSurfaceState};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

/// One-shot latch for the "the zero-readback route is live" log line.
///
/// The composited present is the only path in this file whose ENGAGEMENT
/// cannot be observed by any headless test — a `wgpu::Surface` needs a real
/// `SwapChainPanel` — so a live run needs something to look at. Printed once
/// per process, on the first successful blit, and never again: a per-frame log
/// on the present thread would be its own latency bug.
static COMPOSITED_ENGAGED: AtomicBool = AtomicBool::new(false);

/// The dims of the previously-SHOWN frame, whichever KIND it was.
///
/// The `dims_changed` contract every `PresentSink` present method shares is
/// "computed against the previously-shown frame, whichever kind it was" — and
/// since quick 260803-cws there are THREE kinds (CPU frame, GPU-resident frame,
/// composited target). Without this one shared answer, a dynamic-playback-
/// resolution level change (57-08 varies the composited entry's dims) would be
/// compared against a frame nobody is looking at, and the present loop would
/// either skip an `emit_canvas_viewport` it owes or emit one it does not.
///
/// Precedence is recency: `composited_dims` is `Some` only while a composited
/// present was the most recent one (each other path clears it), `gpu_frame`
/// likewise, and the stored CPU frame is the floor — it is seeded with the 1x1
/// placeholder, so this is total from the very first tick.
fn prev_shown_dims(gpu: &PreviewGpu) -> (u32, u32) {
    if let Some(dims) = gpu.composited_dims {
        return dims;
    }
    #[cfg(windows)]
    if let Some(g) = gpu.gpu_frame.as_ref() {
        return (g.width, g.height);
    }
    (gpu.frame.width, gpu.frame.height)
}

#[allow(
    dead_code,
    reason = "plan 51-03 constructs this and hands it to \
              preview::PresentContext at first attach; task 2 delivers the \
              adapter and its degradation contract, tested here in full."
)]
pub(crate) struct ShellPresentSink {
    state: Arc<PreviewSurfaceState>,
}

#[allow(dead_code, reason = "see the struct above — 51-03 is the constructor's caller")]
impl ShellPresentSink {
    pub(crate) fn new(state: Arc<PreviewSurfaceState>) -> Self {
        Self { state }
    }

    /// The ONE place the contain-fit content rect is computed (`present`,
    /// `present_overlay` and `present_gpu` all call it) — one formula, no
    /// drift, exactly the invariant `engine::contain_fit_viewport`'s own doc
    /// states for the letterbox/pointer pair it already serves.
    fn publish_content_rect(&self, config_w: u32, config_h: u32, frame_w: u32, frame_h: u32) {
        self.state
            .publish_content_rect(engine::contain_fit_viewport(
                config_w as f32,
                config_h as f32,
                frame_w as f32,
                frame_h as f32,
            ));
    }

    /// The WR-01 / no-panel split shared by every composite path: `Ok(false)`
    /// when nothing is attached (there is no surface to present to and never
    /// was), `Err` when a panel IS attached but its GPU set is momentarily
    /// absent — device-lost recovery between its release and rebuild, or after
    /// a FAILED recovery (fail closed).
    fn no_gpu_set(&self) -> Result<bool, engine::EngineError> {
        if self.state.attached.load(Relaxed) {
            Err(engine::EngineError::Gpu(
                "no live GPU set (device-lost recovery in progress or failed); present skipped"
                    .to_string(),
            ))
        } else {
            Ok(false)
        }
    }

    fn poisoned() -> engine::EngineError {
        engine::EngineError::Gpu("preview surface lock poisoned; present skipped".to_string())
    }

    /// **The NOTICE half of TRUST-01** (Phase 63, plan 63-02): every composite
    /// path funnels its outcome through here, so "the device is gone" is
    /// observed at the one place that actually touches the swapchain, and
    /// `presented` counts only presents that genuinely reached it.
    ///
    /// # Two independent detectors, and why both
    ///
    /// 1. **`Compositor::device_lost_pending()`** — the AUTHORITATIVE one. It
    ///    is raised by the response armed at `Compositor::build`'s single
    ///    `request_device` (quick `260829-n96` + plan 63-01), i.e. by wgpu's
    ///    own device-lost callback, which is the same chain a real driver TDR
    ///    fires. This is the detector that cannot produce a false positive.
    /// 2. **A device-loss-class present ERROR** — the belt to that braces.
    ///    `get_current_texture` can surface the removal before wgpu has
    ///    delivered the callback, and on a dead device a failed composite
    ///    submits nothing, so nothing would drive `maintain` to deliver it
    ///    either. Hence the `poll` nudge below — the same technique
    ///    `PreviewRecovery::wait_for_device_lost` uses and documents, and one
    ///    of the two probe-measured-safe post-removal operations (48-03).
    ///
    /// ⚠ `Outdated` is deliberately NOT loss. It is the ordinary
    /// window-resized answer, and treating it as a removal would tear down and
    /// rebuild a perfectly live surface every time the user dragged the window
    /// edge — a self-inflicted version of the defect this plan closes.
    fn record_present_outcome(
        &self,
        compositor: &engine::Compositor,
        outcome: Result<(), engine::EngineError>,
    ) -> Result<(), engine::EngineError> {
        match outcome {
            Ok(()) => {
                if compositor_lost(compositor) {
                    self.state.mark_device_lost("compositor.device_lost_pending");
                }
                self.state.note_presented();
                Ok(())
            }
            Err(e) => {
                let text = e.to_string();
                // Drive wgpu's maintain path so a removal that has not yet been
                // DELIVERED as a callback still gets delivered — without this a
                // failing present submits nothing and the callback can never
                // arrive, which would leave `device_lost_pending` false forever
                // on exactly the tree that needs it true.
                let _ = compositor.device().poll(wgpu::PollType::Poll);
                if compositor_lost(compositor) {
                    self.state
                        .mark_device_lost("compositor.device_lost_pending (after a failed present)");
                } else if text.contains("Lost")
                    || text.contains("lost")
                    || text.contains("removed")
                    || text.contains("Removed")
                {
                    self.state.mark_device_lost(&text);
                }
                Err(e)
            }
        }
    }

    /// The same probe with no present to report — for the paths that ask "is
    /// there a surface at all" every tick while PAUSED, where no composite runs
    /// and therefore [`Self::record_present_outcome`] never fires.
    ///
    /// RECORDED LIMITATION, stated rather than implied: a preview that is
    /// paused AND idle (no scrub, no repaint) performs neither, so a TDR taken
    /// in that state is noticed on the next interaction rather than
    /// immediately. The picture on screen is identical either way — a frozen
    /// last frame — so the delay costs the user nothing but the honesty of
    /// saying so costs nothing either.
    fn probe_device_lost(&self, compositor: &engine::Compositor) {
        if compositor_lost(compositor) {
            self.state
                .mark_device_lost("compositor.device_lost_pending (idle probe)");
        }
    }
}

/// **The device-lost probe the STATUS POLL drives** (Phase 63, plan 63-02).
///
/// # Why this exists, measured rather than reasoned
///
/// The first RED run of `DeviceLostRecoveryUiaTests` removed the live D3D12 device in
/// the running shell and watched `lost` stay `false` for 66 seconds. The engine's own
/// stderr proved the removal landed and the birth-site callback fired
/// (`device_lost: detected via=wgpu-device-lost-callback`), and then there was TOTAL
/// SILENCE — because **while the transport is PLAYING, a dead GPU makes the present loop
/// call no `PresentSink` method at all.** The playing branch only reaches
/// [`preview::PresentSink::present_gpu`] / `present_composited` when it POPS a frame from
/// the ring, the producer composites through the same dead device and pushes nothing, and
/// [`preview::PresentSink::has_surface`] is on the PAUSED branch. So every detector that
/// hangs off a present is unreachable in exactly the state a user is in when a TDR hits.
///
/// This probe therefore hangs off the SHELL'S POLL instead, which runs whatever the
/// transport is doing.
///
/// # Why it is still not a lock-holding poll (threat T-63-06)
///
/// `try_lock`, never `lock`. A CONTENDED lock means the present thread is inside a
/// composite right now, which is itself proof the device is alive — so a skipped probe is
/// never a missed detection, only a deferred one. And the poll runs on the interop worker
/// thread (`RunOut`), never on the UI thread, so the UI cannot stall behind it either way.
///
/// The `poll(Poll)` is the second half and is not optional: wgpu delivers the device-lost
/// callback from its maintain path, so on a device that is failing every submit there
/// must be something driving `maintain` or the callback never arrives. NON-BLOCKING
/// (`PollType::Poll`), and one of the two probe-measured-safe post-removal operations
/// (48-03).
#[cfg(windows)]
pub(crate) fn probe_device_lost_from_poll(state: &PreviewSurfaceState) {
    if state.device_lost.load(Relaxed) || !state.attached.load(Relaxed) {
        return;
    }
    let Ok(gpu) = state.gpu.try_lock() else {
        return;
    };
    let Some(compositor) = gpu.compositor.as_ref() else {
        return;
    };
    let _ = compositor.device().poll(wgpu::PollType::Poll);
    if compositor.device_lost_pending() {
        state.mark_device_lost("device-status poll probe (the present loop was idle)");
    }
}

/// Non-Windows twin — see [`compositor_lost`].
#[cfg(not(windows))]
pub(crate) fn probe_device_lost_from_poll(_state: &PreviewSurfaceState) {}

/// `Compositor::device_lost_pending`, behind the SAME `cfg(windows)` predicate
/// the rest of this file uses for engine capabilities gated on the engine's
/// default-on `hwdecode` feature (see `PreviewGpu::gpu_frame`'s note for why a
/// `feature = "hwdecode"` predicate here would read the WRONG namespace).
///
/// The non-Windows twin answers `false` because there is no `SwapChainPanel`
/// surface to lose there either — `wgpu`'s panel surface target is `cfg(dx12)`.
#[cfg(windows)]
fn compositor_lost(compositor: &engine::Compositor) -> bool {
    compositor.device_lost_pending()
}

#[cfg(not(windows))]
fn compositor_lost(_compositor: &engine::Compositor) -> bool {
    false
}

impl preview::PresentSink for ShellPresentSink {
    /// Twins `TauriPresentSink::reconfigure_if_dirty`
    /// (`src-tauri/src/preview_host.rs:317-360`): swap the dirty flag, read the
    /// target size lock-free, then reconfigure under the GPU lock.
    ///
    /// Blocking `lock()`, matching the Tauri present-thread path rather than
    /// the main-thread reposition path's `try_lock` (which has no counterpart
    /// here — under `SwapChainPanel` nothing moves a child window).
    ///
    /// `None` on: not dirty · a poisoned lock · no GPU set (pre-attach,
    /// post-detach, or mid-recovery — WR-01's fail-closed degrade, retried
    /// because recovery re-marks `surface_dirty`) · a failed reconfigure.
    fn reconfigure_if_dirty(&self) -> Option<(u32, u32)> {
        if !self.state.surface_dirty.swap(false, Relaxed) {
            return None;
        }
        let tw = self.state.target_w.load(Relaxed);
        let th = self.state.target_h.load(Relaxed);
        let lock_result = self.state.gpu.lock();
        let Ok(mut gpu) = lock_result else {
            // Poisoned present lock: skip this reconfigure, never unwind again.
            return None;
        };
        let result = {
            let (Some(compositor), Some(surface)) = (gpu.compositor.as_ref(), gpu.surface.as_ref())
            else {
                return None;
            };
            compositor.configure_surface(surface, tw, th)
        };
        match result {
            Ok(()) => {
                gpu.config_w = tw;
                gpu.config_h = th;
                // D-09's other half, on the thread D-09 names: re-apply the
                // composition-scale inverse transform after EVERY reconfigure.
                // A DPI or monitor change arrives here as a resize carrying a
                // NEW scale, and the swapchain keeps whatever transform it was
                // last given — so skipping this would leave the picture sized
                // for the old monitor until something else reconfigured it.
                // Re-applying an unchanged scale is a no-op at the driver.
                if let Some(surface) = gpu.surface.as_ref() {
                    if let Err(e) =
                        super::surface::apply_composition_scale(surface, self.state.target_scale())
                    {
                        eprintln!("panel::sink: composition-scale transform not re-applied: {e}");
                    }
                }
                Some((tw, th))
            }
            Err(e) => {
                eprintln!("panel::sink: configure_surface failed: {e}");
                None
            }
        }
    }

    /// Twins `TauriPresentSink::present` (`preview_host.rs:374-418`): the
    /// `dims_changed` compare against the PREVIOUSLY-shown frame, then the
    /// store, then the composite — in that order and under the one lock.
    ///
    /// This is the ZERO-INK path: it STORES what it composites, so an
    /// ink-annotated clone must never reach it (Pitfall 2 / D-46-03-02 — a
    /// resize re-presents the stored frame, which would double the ink).
    fn present(&self, frame: &engine::Frame) -> Result<bool, engine::EngineError> {
        let lock_result = self.state.gpu.lock();
        let Ok(mut gpu) = lock_result else {
            return Err(Self::poisoned());
        };
        let (prev_w, prev_h) = prev_shown_dims(&gpu);
        let dims_changed = prev_w != frame.width || prev_h != frame.height;
        gpu.frame = frame.clone();
        // A CPU frame is now current — release any stored GPU frame so
        // `current_frame()`/`present_gpu` never answer from a stale handle
        // (and its hw-frame-pool slice unpins), and drop the composited
        // marker for the same reason.
        #[cfg(windows)]
        {
            gpu.gpu_frame = None;
        }
        gpu.composited_dims = None;
        let (config_w, config_h) = (gpu.config_w, gpu.config_h);
        let (Some(compositor), Some(surface)) = (gpu.compositor.as_ref(), gpu.surface.as_ref())
        else {
            return self.no_gpu_set();
        };
        self.record_present_outcome(compositor, compositor.composite_to_surface(&gpu.frame, surface))?;
        drop(gpu);
        self.publish_content_rect(config_w, config_h, frame.width, frame.height);
        Ok(dims_changed)
    }

    /// Twins `TauriPresentSink::present_overlay` (`preview_host.rs:429-474`) —
    /// the Pitfall-2 split: `clean` is what gets STORED (and only when
    /// `Some`), `annotated` is what gets SHOWN. `clean = None` is the
    /// paused-repaint case, where the stored frame is already pristine and
    /// must not be reassigned.
    fn present_overlay(
        &self,
        clean: Option<&engine::Frame>,
        annotated: &engine::Frame,
    ) -> Result<bool, engine::EngineError> {
        let lock_result = self.state.gpu.lock();
        let Ok(mut gpu) = lock_result else {
            return Err(Self::poisoned());
        };
        let dims_changed = match clean {
            Some(clean) => {
                let (prev_w, prev_h) = prev_shown_dims(&gpu);
                let changed = prev_w != clean.width || prev_h != clean.height;
                // Pitfall 2: `gpu.frame` is only EVER assigned the pristine frame.
                gpu.frame = clean.clone();
                #[cfg(windows)]
                {
                    gpu.gpu_frame = None;
                }
                gpu.composited_dims = None;
                changed
            }
            // Re-present of the stored frame: nothing new arrived, so the
            // content rect cannot have moved and `gpu.frame` must not be touched.
            None => false,
        };
        let (config_w, config_h) = (gpu.config_w, gpu.config_h);
        let (Some(compositor), Some(surface)) = (gpu.compositor.as_ref(), gpu.surface.as_ref())
        else {
            return self.no_gpu_set();
        };
        self.record_present_outcome(compositor, compositor.composite_to_surface(annotated, surface))?;
        drop(gpu);
        // Published from what was SHOWN: the annotated clone always carries the
        // clean frame's dimensions (the ink is drawn into a clone, never a
        // resize), so this is the same rect either branch would compute.
        self.publish_content_rect(config_w, config_h, annotated.width, annotated.height);
        Ok(dims_changed)
    }

    /// Twins `TauriPresentSink::current_frame` (`preview_host.rs:486-512`): the
    /// GPU-readback branch first, else the stored CPU frame.
    ///
    /// Divergence, deliberate: the Tauri twin answers `None` when NO surface is
    /// managed (the mock runtime), because there is then no `NativePreview` to
    /// read a frame out of. This host always owns its state, so an unattached
    /// instance honestly answers `Some(placeholder)` — the frame it would
    /// present the moment a panel arrives. `None` here means only "poisoned, or
    /// the readback failed".
    ///
    /// Second divergence, added with the composited present (quick 260803-cws)
    /// and stated rather than hidden: after a COMPOSITED present this still
    /// answers the last stored CPU frame, because a pool-recycled
    /// `engine::PooledTarget` cannot be kept (see
    /// `PreviewGpu::composited_dims`). The window is bounded by playback
    /// itself — pausing re-stores a fresh frame through
    /// `present_still`/`present_multilayer`, and every consumer of this method
    /// (resize repaint, paused readback) runs on the paused side of that.
    fn current_frame(&self) -> Option<engine::Frame> {
        let gpu = self.state.gpu.lock().ok()?;
        #[cfg(windows)]
        if let Some(g) = gpu.gpu_frame.as_ref() {
            // Belt and braces: recovery clears `gpu_frame` before it strips the
            // GPU set, so these should never co-occur.
            let Some(compositor) = gpu.compositor.as_ref() else {
                return None;
            };
            return match compositor.composite_gpu_to_rgba(g) {
                Ok(rgba) => Some(engine::Frame {
                    width: g.width,
                    height: g.height,
                    rgba,
                }),
                Err(e) => {
                    eprintln!("panel::sink: paused GPU readback failed: {e}");
                    None
                }
            };
        }
        Some(gpu.frame.clone())
    }

    /// Twins `TauriPresentSink::configured_size` (`preview_host.rs:518-524`).
    /// `(0, 0)` before the first configure or on a poisoned lock —
    /// `contain_fit_viewport` is already total on zero inputs.
    fn configured_size(&self) -> (u32, u32) {
        self.state
            .gpu
            .lock()
            .map(|gpu| (gpu.config_w, gpu.config_h))
            .unwrap_or((0, 0))
    }

    /// Twins `TauriPresentSink::mark_shown_once` (`preview_host.rs:534-550`):
    /// the one-shot latch. `false` on a poisoned lock (nothing may be revealed
    /// on the strength of a broken present path).
    fn mark_shown_once(&self) -> bool {
        let lock_result = self.state.gpu.lock();
        let Ok(mut gpu) = lock_result else {
            return false;
        };
        if gpu.shown {
            return false;
        }
        gpu.shown = true;
        true
    }

    /// Twins `TauriPresentSink::composite_layers` (`preview_host.rs:564-588`):
    /// the offscreen render + readback the multi-layer path needs. The guard is
    /// scoped to this call and released on return, so `present`/
    /// `present_overlay` re-take the lock afterwards rather than nesting.
    /// `None` folds together no GPU set, a poisoned lock and a failed
    /// composite — each of which the caller already treats as "return without
    /// presenting".
    fn composite_layers(
        &self,
        layers: &[engine::Layer],
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>> {
        let lock_result = self.state.gpu.lock();
        let Ok(gpu) = lock_result else {
            return None; // poisoned present lock: skip, never unwind again
        };
        let compositor = gpu.compositor.as_ref()?;
        match compositor.composite_layers_to_rgba(layers, width, height) {
            Ok(rgba) => Some(rgba),
            Err(e) => {
                eprintln!("panel::sink: multi-layer composite failed: {e}");
                None
            }
        }
    }

    /// Twins `TauriPresentSink::has_surface` (`preview_host.rs:597-601`) — the
    /// "is there anything to present to at all" probe `present_still` runs
    /// FIRST, so the paused/scrub path resolves and decodes nothing when there
    /// is not.
    ///
    /// The Tauri twin can answer without a lock (the managed state either
    /// exists or does not). This host always owns its state, so the honest
    /// question is whether the GPU SET is live — which needs the lock. A
    /// poisoned lock answers `false`: a broken present path has no surface
    /// worth decoding for.
    fn has_surface(&self) -> bool {
        let Ok(gpu) = self.state.gpu.lock() else {
            return false;
        };
        let live = gpu.surface.is_some() && gpu.compositor.is_some();
        // Plan 63-02: the PAUSED tick's device-lost probe. `present_still` asks
        // this first on every paused tick, which makes it the one hook that
        // runs when no composite does — see `probe_device_lost` for the
        // limitation this does and does not cover.
        if live {
            if let Some(compositor) = gpu.compositor.as_ref() {
                self.probe_device_lost(compositor);
            }
        }
        live
    }

    /// Twins `TauriPresentSink::compositor` (`preview_host.rs:618-626`): the
    /// `Arc` clone the ring producer composites through on ITS own thread,
    /// re-fetched per respawn so a rebuilt device is picked up. `None`
    /// mid-recovery, pre-attach, or on a poisoned lock.
    fn compositor(&self) -> Option<Arc<engine::Compositor>> {
        self.state
            .gpu
            .lock()
            .ok()
            .and_then(|gpu| gpu.compositor.clone())
    }

    /// Twins `TauriPresentSink::present_gpu` (`preview_host.rs:649-695`): the
    /// GPU-RESIDENT twin of [`preview::PresentSink::present`]. Same lock, same
    /// degradation rules, same `dims_changed` contract (computed against the
    /// previously-shown frame, whichever kind it was), same
    /// store-what-you-present order.
    ///
    /// **No pixel readback happens here (GPU-06)**: `composite_gpu_to_surface`
    /// renders the NV12 plane views straight into the swapchain. The
    /// paused-only readback stays on `current_frame`.
    ///
    /// `cfg(windows)` only, deliberately NOT a feature predicate — see
    /// `PreviewGpu::gpu_frame`'s note. The trait declares this method under
    /// `all(windows, feature = "hwdecode")` evaluated in PREVIEW's namespace,
    /// where the forwarder feature is default-on; repeating that predicate here
    /// would read THIS crate's namespace, which has no such feature, and the
    /// impl would silently fail to satisfy the trait.
    #[cfg(windows)]
    fn present_gpu(&self, frame: &engine::GpuFrame) -> Result<bool, engine::EngineError> {
        let lock_result = self.state.gpu.lock();
        let Ok(mut gpu) = lock_result else {
            return Err(Self::poisoned());
        };
        let (prev_w, prev_h) = prev_shown_dims(&gpu);
        let dims_changed = prev_w != frame.width || prev_h != frame.height;
        // Store-what-you-present, before the composite (mirroring `present`).
        // A failed handle clone costs only the paused-repaint capability until
        // the next frame — never the present.
        match frame.try_clone() {
            Ok(clone) => gpu.gpu_frame = Some(clone),
            Err(e) => {
                eprintln!("panel::sink: GpuFrame handle clone failed (current-frame store skipped): {e}");
                gpu.gpu_frame = None;
            }
        }
        // A GPU-resident frame is now current, whatever the clone did.
        gpu.composited_dims = None;
        let (config_w, config_h) = (gpu.config_w, gpu.config_h);
        let (Some(compositor), Some(surface)) = (gpu.compositor.as_ref(), gpu.surface.as_ref())
        else {
            return self.no_gpu_set();
        };
        self.record_present_outcome(compositor, compositor.composite_gpu_to_surface(frame, surface))?;
        drop(gpu);
        self.publish_content_rect(config_w, config_h, frame.width, frame.height);
        Ok(dims_changed)
    }

    /// NO Tauri twin — see the module header. This is the first method written
    /// after Phase 55's cutover deleted `src-tauri/`.
    ///
    /// Can this sink blit an already-composited GPU texture into a swapchain
    /// RIGHT NOW? Only when the GPU set is live in BOTH halves: the compositor
    /// does the blit, the surface receives it, and `blit_texture_to_surface`
    /// needs a compositor built by `new_with_surface` — which the attach path
    /// (`panel/surface.rs`) always is.
    ///
    /// Read exactly ONCE, at producer-spawn time (`present_loop.rs:352`), and
    /// that is safe here rather than lucky: the present thread — the only
    /// thread that spawns a producer — starts at FIRST ATTACH, so the GPU set
    /// is already live before any read; and WR-01's device-lost recovery
    /// respawns the producer, which re-reads it against the rebuilt set. A
    /// `false` answer is never wrong, only slower: the producer then reads the
    /// same target back on its OWN thread and pushes a CPU payload, which is
    /// precisely the pre-cutover behaviour.
    ///
    /// Deliberately the same probe as [`preview::PresentSink::has_surface`],
    /// restated rather than delegated: `has_surface` answers a DIFFERENT
    /// question for a different caller ("is it worth decoding a paused frame at
    /// all?", `present_still`'s first check), and letting that meaning drift
    /// would silently re-route the producer's carrier.
    fn supports_composited_present(&self) -> bool {
        self.state
            .gpu
            .lock()
            .map(|gpu| gpu.surface.is_some() && gpu.compositor.is_some())
            .unwrap_or(false)
    }

    /// NO Tauri twin — see the module header.
    ///
    /// Present the producer's ALREADY-COMPOSITED target straight into the
    /// swapchain (PLAY-02 / PLAY-07, quick 260803-cws). This is the method
    /// whose absence made 57-06's mechanism a mechanism and not a win: with it,
    /// [`preview::PresentSink::supports_composited_present`] answers `true`, the
    /// producer's carrier flips to `preview::RingPayload::Composited`, and the
    /// per-composited-frame `blit_texture_to_rgba` readback — plus the
    /// `blit-readback-target` texture and bind group it allocated on the
    /// producer thread every single call — is DELETED rather than relocated.
    /// `crates/ffi/tests/composited_present_pin.rs` holds that structurally.
    ///
    /// Same lock, same degradation rules, same store-then-composite order as
    /// [`preview::PresentSink::present`] and `present_gpu`, with ONE honest
    /// difference: the store is the DIMS, never the target. See
    /// `PreviewGpu::composited_dims` for why storing a pooled target would
    /// starve the producer and re-present future pixels.
    ///
    /// NOT `cfg`-gated: the trait method is not either (a composited target is
    /// plain `wgpu`, unlike `GpuFrame`'s D3D11VA handles).
    fn present_composited(
        &self,
        target: &engine::PooledTarget,
        content_w: u32,
        content_h: u32,
    ) -> Result<bool, engine::EngineError> {
        let lock_result = self.state.gpu.lock();
        let Ok(mut gpu) = lock_result else {
            return Err(Self::poisoned());
        };
        let (prev_w, prev_h) = prev_shown_dims(&gpu);
        let dims_changed = prev_w != content_w || prev_h != content_h;
        // Store-what-you-present, adapted to a pool-recycled target: DIMS only.
        // Stored BEFORE the GPU-set check, mirroring `present`.
        gpu.composited_dims = Some((content_w, content_h));
        // A composited frame is now current — unpin any stored hw-pool slice.
        #[cfg(windows)]
        {
            gpu.gpu_frame = None;
        }
        let (config_w, config_h) = (gpu.config_w, gpu.config_h);
        let (Some(compositor), Some(surface)) = (gpu.compositor.as_ref(), gpu.surface.as_ref())
        else {
            return self.no_gpu_set();
        };
        self.record_present_outcome(
            compositor,
            compositor.blit_texture_to_surface(target.view(), content_w, content_h, surface),
        )?;
        drop(gpu);
        if !COMPOSITED_ENGAGED.swap(true, Relaxed) {
            eprintln!("panel::sink: composited GPU present engaged (PLAY-02 zero-readback route)");
        }
        // Constraint 2, and the likeliest real bug if it were missed: every
        // other present path publishes this, and the XAML ink overlay maps the
        // pointer through it. The formula is byte-identical — `content_w`/
        // `content_h` are the CONTENT dims (which 57-08 varies by resolution
        // level), and `contain_fit_viewport` is aspect-stable across them.
        self.publish_content_rect(config_w, config_h, content_w, content_h);
        Ok(dims_changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use preview::PresentSink;

    /// A sink over a state with an EMPTY GPU set — the "no panel attached"
    /// world every one of these assertions describes. Needs no GPU, no COM and
    /// no window, which is exactly why the degradation contract is testable at
    /// all.
    fn unattached() -> (Arc<PreviewSurfaceState>, ShellPresentSink) {
        let state = Arc::new(PreviewSurfaceState::default());
        (Arc::clone(&state), ShellPresentSink::new(state))
    }

    fn frame(w: u32, h: u32) -> engine::Frame {
        engine::Frame {
            width: w,
            height: h,
            rgba: vec![0u8; w as usize * h as usize * 4],
        }
    }

    #[test]
    fn an_unattached_sink_reports_no_surface_and_no_compositor() {
        let (_state, sink) = unattached();
        assert!(!sink.has_surface(), "nothing is attached, so nothing to present to");
        assert!(sink.compositor().is_none(), "no device exists yet");
        assert_eq!(sink.configured_size(), (0, 0), "nothing has been configured");
    }

    #[test]
    fn an_unattached_present_is_a_silent_no_op_never_a_panic() {
        let (_state, sink) = unattached();
        assert!(
            sink.present(&frame(64, 36)).is_ok(),
            "with no panel attached there is nothing to present to, and that is a \
             no-op SUCCESS -- exactly how TauriPresentSink degrades under the mock \
             runtime. Never an error, never a panic."
        );
    }

    #[test]
    fn present_stores_the_frame_even_when_the_composite_is_skipped() {
        // The store happens BEFORE the GPU-set check, exactly as in the Tauri
        // twin: the stored frame is the paused restore point, and a skipped
        // composite must not lose it.
        let (_state, sink) = unattached();
        assert!(sink.present(&frame(64, 36)).is_ok());
        let stored = sink.current_frame().expect("a stored frame is readable");
        assert_eq!((stored.width, stored.height), (64, 36));
    }

    #[test]
    fn present_overlay_stores_only_the_clean_frame_never_the_annotated_one() {
        // Pitfall 2 / D-46-03-02, pinned mechanically: a resize re-presents the
        // STORED frame, so storing the ink-annotated clone would bake the ink
        // in and double it on every resize.
        let (_state, sink) = unattached();
        let clean = frame(8, 8); // all zero bytes
        let mut annotated = frame(8, 8);
        annotated.rgba.fill(0xFF); // "ink"
        assert!(sink.present_overlay(Some(&clean), &annotated).is_ok());
        let stored = sink.current_frame().expect("a stored frame is readable");
        assert!(
            stored.rgba.iter().all(|b| *b == 0),
            "the CLEAN frame is what gets stored; the annotated clone is shown and thrown away"
        );

        // ...and a paused repaint (`clean = None`) must not touch it at all.
        assert!(sink.present_overlay(None, &annotated).is_ok());
        let stored = sink.current_frame().unwrap();
        assert!(
            stored.rgba.iter().all(|b| *b == 0),
            "a repaint re-composites the stored frame and never reassigns it"
        );
    }

    #[test]
    fn an_unattached_present_overlay_is_a_silent_no_op_too() {
        let (_state, sink) = unattached();
        let clean = frame(64, 36);
        let annotated = frame(64, 36);
        assert!(
            sink.present_overlay(Some(&clean), &annotated).is_ok(),
            "the ink path degrades exactly like the zero-ink one"
        );
        assert!(
            !sink.present_overlay(None, &annotated).unwrap(),
            "a paused repaint never reports a dims change"
        );
    }

    #[test]
    fn current_frame_answers_with_the_placeholder_before_any_present() {
        let (_state, sink) = unattached();
        let f = sink.current_frame().expect("an owned state always has a frame");
        assert_eq!((f.width, f.height), (1, 1), "the 1x1 placeholder seed");
        // ...and with whatever was stored afterwards.
        let _ = sink.present(&frame(48, 24));
        let f = sink.current_frame().unwrap();
        assert_eq!((f.width, f.height), (48, 24));
    }

    #[test]
    fn reconfigure_if_dirty_returns_none_even_with_the_flag_set() {
        let (state, sink) = unattached();
        assert!(sink.reconfigure_if_dirty().is_none(), "not dirty -> None");
        state.target_w.store(1280, Relaxed);
        state.target_h.store(720, Relaxed);
        state.surface_dirty.store(true, Relaxed);
        assert!(
            sink.reconfigure_if_dirty().is_none(),
            "dirty but no GPU set -> None, never a panic"
        );
        assert!(
            !state.surface_dirty.load(Relaxed),
            "the flag is CONSUMED either way, so a stale dirty bit cannot pin the \
             present thread in a reconfigure loop"
        );
    }

    #[test]
    fn mark_shown_once_latches_exactly_once() {
        let (_state, sink) = unattached();
        assert!(sink.mark_shown_once(), "the first call is the reveal");
        assert!(!sink.mark_shown_once(), "every later call is not");
    }

    #[test]
    fn composite_layers_degrades_to_none_without_a_compositor() {
        let (_state, sink) = unattached();
        assert!(sink.composite_layers(&[], 64, 36).is_none());
    }

    #[test]
    fn an_attached_instance_with_no_gpu_set_reports_an_error_instead() {
        // WR-01's release->rebuild window (and a FAILED recovery): the caller
        // must SEE this, unlike the never-attached case above.
        let (state, sink) = unattached();
        state.attached.store(true, Relaxed);
        assert!(
            sink.present(&frame(64, 36)).is_err(),
            "an attached panel with no live GPU set is a real, reportable fault"
        );
    }

    #[test]
    fn the_content_rect_is_published_by_the_composite_path_only() {
        let (state, sink) = unattached();
        assert_eq!(state.content_rect(), (0, 0, 0, 0), "nothing composited yet");
        // Unattached, the composite is skipped before the rect is published --
        // publishing a rect for a surface that never rendered would hand the
        // ink overlay a geometry the user cannot see.
        let _ = sink.present(&frame(64, 36));
        assert_eq!(
            state.content_rect(),
            (0, 0, 0, 0),
            "a skipped composite publishes nothing"
        );
    }

    #[test]
    fn the_contain_fit_rect_is_the_engine_formula_letterboxed() {
        // The rect this sink publishes IS `engine::contain_fit_viewport` -- one
        // formula, no drift. A 16:9 frame in a 4:3 panel letterboxes top/bottom.
        let rect = engine::contain_fit_viewport(800.0, 600.0, 1920.0, 1080.0);
        let state = PreviewSurfaceState::default();
        state.publish_content_rect(rect);
        assert_eq!(
            state.content_rect(),
            (0, 75, 800, 450),
            "800x450 content, centred vertically in an 800x600 panel"
        );
    }

    // -----------------------------------------------------------------------
    // Quick 260803-cws: the COMPOSITED present (PLAY-02 / PLAY-07).
    // -----------------------------------------------------------------------

    /// One DX12 device at a time across this module's GPU tests.
    ///
    /// Not cargo-culted, and deliberately a plain `Mutex` rather than a shared
    /// `OnceLock<Compositor>`: the abi_validation D-1 lesson is that the
    /// concurrent create/destroy of several real devices inside one test binary
    /// is what destabilises, not the cycles themselves. Serialising keeps each
    /// test owning its own device (so a poisoned one cannot leak into the next)
    /// while never running two at once.
    static GPU_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A REAL offscreen compositor + its target pool. There is no other way to
    /// obtain a [`engine::PooledTarget`] — its only constructor is a pool
    /// checkout, and a pool's textures must live on the device that renders
    /// into them — so the two `present_composited` degradation tests genuinely
    /// need a device even though neither reaches the GPU.
    ///
    /// Non-`#[ignore]`d, matching `engine`'s own in-crate offscreen-compositor
    /// convention (`compositor.rs`'s `shared_compositor`).
    fn offscreen_pool() -> (engine::Compositor, engine::CompositeTargetPool) {
        let compositor = engine::Compositor::new().expect("an offscreen DX12 compositor");
        let pool = compositor.create_composite_target_pool(64, 36);
        (compositor, pool)
    }

    #[test]
    fn an_unattached_sink_does_not_claim_the_composited_capability() {
        let (_state, sink) = unattached();
        assert!(
            !sink.supports_composited_present(),
            "the capability is read ONCE at producer-spawn and decides the ring's carrier: \
             claiming it with no surface would make the producer push texture entries this \
             sink could not blit"
        );
    }

    #[test]
    fn the_content_rect_is_aspect_stable_across_resolution_levels() {
        // The 57-08 dynamic-playback-resolution guard: a level change hands
        // `present_composited` SMALLER content dims for the SAME picture, and
        // the published rect must not move -- otherwise the pointer mapping and
        // the XAML ink overlay would jump whenever the engine drops resolution.
        //
        // Asserted on the PUBLISHED (integer px) rect rather than the raw
        // `[f32; 4]`, because that is what `rudis_preview_content_rect` hands
        // the overlay, and because the two float vectors are NOT bit-equal:
        // 1920 * (800f32/1920f32) is 799.99998, not 800.0. Rounding to whole
        // pixels is part of the contract, not a way around it.
        let full = engine::contain_fit_viewport(800.0, 600.0, 1920.0, 1080.0);
        let quarter = engine::contain_fit_viewport(800.0, 600.0, 320.0, 180.0);
        for (rect, what) in [(full, "Full 1920x1080"), (quarter, "Quarter 320x180")] {
            let state = PreviewSurfaceState::default();
            state.publish_content_rect(rect);
            assert_eq!(
                state.content_rect(),
                (0, 75, 800, 450),
                "{what}: the same 16:9 letterbox in the same 800x600 panel"
            );
        }
    }

    #[test]
    fn an_unattached_composited_present_is_a_silent_no_op() {
        let _serial = GPU_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (_compositor, pool) = offscreen_pool();
        let target = pool.try_checkout().expect("a fresh pool always has a free slot");
        let (state, sink) = unattached();
        let _ = sink.present(&frame(48, 24));
        assert_eq!(
            sink.present_composited(&target, 64, 36).ok(),
            Some(false),
            "with no panel attached there is nothing to blit into, and that is a no-op \
             SUCCESS -- exactly how every other present path degrades here"
        );
        assert_eq!(
            state.content_rect(),
            (0, 0, 0, 0),
            "a skipped blit publishes no rect -- handing the ink overlay a geometry for a \
             surface that never rendered is worse than handing it nothing"
        );
        let stored = sink.current_frame().expect("the stored CPU frame still answers");
        assert_eq!(
            (stored.width, stored.height),
            (48, 24),
            "a composited present stores DIMS, never a pooled target, so `current_frame` \
             keeps answering the last CPU frame (see PreviewGpu::composited_dims)"
        );
    }

    #[test]
    fn an_attached_instance_with_no_gpu_set_errors_on_composited_present() {
        let _serial = GPU_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (_compositor, pool) = offscreen_pool();
        let target = pool.try_checkout().expect("a fresh pool always has a free slot");
        let (state, sink) = unattached();
        state.attached.store(true, Relaxed);
        assert!(
            sink.present_composited(&target, 64, 36).is_err(),
            "WR-01's release->rebuild window: an ATTACHED panel whose GPU set is momentarily \
             absent is a real, reportable fault on this path too -- the same split \
             `no_gpu_set()` already draws for `present`/`present_gpu`"
        );
    }

    #[test]
    fn target_scale_defaults_to_identity_and_round_trips() {
        let state = PreviewSurfaceState::default();
        assert_eq!(state.target_scale(), 1.0, "D-10: never silently unscaled");
        state
            .target_scale_bits
            .store(1.5f32.to_bits(), Relaxed);
        assert_eq!(state.target_scale(), 1.5);
    }
}
