//! `RecordingPresentSink` (Phase 48, plan 48-01): a headless, timestamped
//! recording implementor of [`crate::PresentSink`] — the harness Phase 46's
//! close asked for and Phase 48 builds FIRST as its before/after ruler.
//!
//! # Why this exists (D-46-11-03 / D-46-11-02)
//!
//! - **D-46-11-03**: no automated test in either tier exercised
//!   [`crate::present_loop`]'s pacing / A-V-clock / seek-flush body. This sink
//!   records every present-path call with an [`std::time::Instant`], so the
//!   pacing tests in `tests/recording_sink_pacing.rs` can drive the REAL loop
//!   and assert on its observable cadence.
//! - **D-46-11-02**: present-path dispatch cost was BOUNDED but never measured
//!   (LAT-01's `sustained_frame_time` never enters the present path). Calling
//!   [`crate::PresentSink::present`] on this sink in a tight loop yields the
//!   first-ever measured dispatch+record number (`PRESENT_BASELINE`).
//!
//! # What it records
//!
//! Every trait call that matters to pacing lands in one `Mutex<Vec<_>>` of
//! [`PresentRecord`]s: `present` / `present_overlay` (the ~30-60Hz hot calls),
//! `reconfigure_if_dirty` (when it actually fires), and `composite_layers`
//! (plan 48-11's readback-stall probe needs to see it was asked). The
//! remaining methods are honest stubs mirroring the established `FakeSink`
//! shape in `tests/overlay_present.rs`.
//!
//! # Why this is production code, not `#[cfg(test)]`
//!
//! Plan 48-11's GPU-06 readback-isolation proof and later phases use this
//! sink from integration tests in other targets. It has ZERO shell
//! dependencies (plain std + the `engine` types already in the port
//! signatures), so the crate's shell-free guarantee — the Cargo.toml
//! substring rule — is untouched; this module adds no manifest entry at all.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::PresentSink;

/// Which [`crate::PresentSink`] method produced a [`PresentRecord`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordKind {
    /// [`crate::PresentSink::present`] — the ZERO-INK hot path whose interval
    /// variance is the phase's pacing ruler.
    Present,
    /// [`crate::PresentSink::present_overlay`]. `clean_was_some`
    /// distinguishes a mid-play ink present (`Some` — a fresh frame arrived)
    /// from a paused repaint (`None` — re-showing the stored frame).
    PresentOverlay {
        /// Whether the caller handed a fresh clean frame to remember.
        clean_was_some: bool,
    },
    /// [`crate::PresentSink::reconfigure_if_dirty`] observed a pending resize
    /// and consumed it (recorded only when it FIRES, not on every poll).
    Reconfigure,
    /// [`crate::PresentSink::composite_layers`] was asked for an offscreen
    /// composite (the recording sink answers `None`; the CALL is the datum —
    /// plan 48-11's stall test needs to see readback requests interleaved
    /// with presents).
    CompositeLayers,
    /// [`crate::PresentSink::present_gpu`] — the GPU-RESIDENT hot path (Phase
    /// 48, plan 48-08). Records the same `(Instant, w, h, dims_changed)`
    /// tuple as [`RecordKind::Present`]; NO GPU work happens in this headless
    /// double. The variant itself is unconditional (it names no GPU type) so
    /// record-matching code needs no cfg — only the trait method that pushes
    /// it is gated.
    PresentGpu,
    /// [`crate::PresentSink::present_composited`] — the ALREADY-COMPOSITED
    /// multi-layer hot path (Phase 57, plan 57-06). `width`/`height` carry the
    /// composited content size (the project canvas at full resolution), so the
    /// arm census that used to read "a `Present` at project resolution IS a
    /// composite" reads this variant instead and means exactly the same thing.
    /// Unconditional (it names no GPU type) so record-matching code needs no
    /// cfg.
    PresentComposited,
}

/// One timestamped present-path call, as captured by [`RecordingPresentSink`].
#[derive(Debug, Clone, PartialEq)]
pub struct PresentRecord {
    /// When the call happened (monotonic).
    pub at: Instant,
    /// Content dimensions of the frame the call presented (for
    /// [`RecordKind::Reconfigure`]: the size the surface was reconfigured to;
    /// for [`RecordKind::CompositeLayers`]: the requested composite size).
    pub width: u32,
    /// See `width`.
    pub height: u32,
    /// The `dims_changed` answer the sink computed for this call, exactly as
    /// the trait doc requires: compared against the PREVIOUSLY stored frame
    /// BEFORE overwriting it (`false` for kinds that store nothing).
    pub dims_changed: bool,
    /// Which method produced this record.
    pub kind: RecordKind,
}

/// A headless [`crate::PresentSink`] that records every present-path call
/// with a timestamp instead of touching a GPU. See the module doc for why.
///
/// Two constructors decide how much machinery a test drives:
///
/// - [`RecordingPresentSink::new`] — `compositor() == None`. Pure
///   dispatch-cost recording, zero GPU; `present_loop` spawns NO producer
///   against this (its `if let Some(comp)` gate), so use it for direct-call
///   measurement only.
/// - [`RecordingPresentSink::with_compositor`] — hands the loop a real
///   offscreen [`engine::Compositor`] (DX12-pinned, headless-proven by
///   `crates/engine/tests/surface_present.rs`), which is what lets the REAL
///   producer/ring/present pipeline run under `cargo test`.
pub struct RecordingPresentSink {
    records: Mutex<Vec<PresentRecord>>,
    /// The frame remembered as "current" — mirrors the real sink's
    /// `np.frame` so [`crate::PresentSink::current_frame`] behaves
    /// identically (and so the ink-doubling contract — store CLEAN, never
    /// ANNOTATED — is honored, not just stubbed).
    stored: Mutex<Option<engine::Frame>>,
    /// Settable pending-resize, consumed by
    /// [`crate::PresentSink::reconfigure_if_dirty`].
    dirty: Mutex<Option<(u32, u32)>>,
    /// Settable configured surface size (defaults to 1280x720).
    configured: Mutex<(u32, u32)>,
    shown_once: Mutex<bool>,
    compositor: Option<Arc<engine::Compositor>>,
    /// Dims of the last GPU-RESIDENT frame presented (Phase 48, plan 48-08).
    /// A headless double cannot store a CPU twin of a GPU frame without the
    /// readback GPU-06 forbids on this path, so `present_gpu`'s
    /// stored-as-current contract is honored at the DIMS level: this is what
    /// its `dims_changed` compares against (falling back to the stored CPU
    /// frame's dims when no GPU frame has been shown). `present` — the 48-01
    /// before-ruler — is deliberately byte-untouched and does not clear this;
    /// mixed CPU/GPU sequencing is 48-09's producer wiring, not this seam's.
    gpu_dims: Mutex<Option<(u32, u32)>>,
    /// Phase 49 (plan 49-01, OQ4): every
    /// [`crate::PresentSink::note_presented_stamp`] call from the present
    /// loop, as `(when it was reported, the presented entry's timeline_us)`.
    /// Kept SEPARATE from `records` on purpose: `PresentRecord`'s exhaustive
    /// literals are constructed in 6 places and nothing needs per-record
    /// stamps — the stamp stream is its own instrument (the "landed at
    /// TARGET" ruler), not a per-present annotation.
    stamps: Mutex<Vec<(Instant, i64)>>,
}

/// Poison-tolerant lock (the pacing tests stop `present_loop` — which has no
/// exit path of its own — by panicking a host port call, so a recording lock
/// must never turn that intentional unwind into a cascading test failure).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl RecordingPresentSink {
    /// A recording sink with NO compositor: `present_loop` spawns no
    /// producer against this. For direct dispatch-cost measurement.
    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            stored: Mutex::new(None),
            dirty: Mutex::new(None),
            configured: Mutex::new((1280, 720)),
            shown_once: Mutex::new(false),
            compositor: None,
            gpu_dims: Mutex::new(None),
            stamps: Mutex::new(Vec::new()),
        }
    }

    /// A recording sink that answers [`crate::PresentSink::compositor`] with
    /// a real offscreen compositor, so the full `present_loop` +
    /// `spawn_producer` pipeline runs headlessly.
    pub fn with_compositor(compositor: Arc<engine::Compositor>) -> Self {
        Self {
            compositor: Some(compositor),
            ..Self::new()
        }
    }

    /// Arm the next [`crate::PresentSink::reconfigure_if_dirty`] poll with a
    /// pending resize (test control surface).
    pub fn set_dirty(&self, size: (u32, u32)) {
        *lock(&self.dirty) = Some(size);
    }

    /// Set the configured surface size
    /// ([`crate::PresentSink::configured_size`]'s answer).
    pub fn set_configured_size(&self, size: (u32, u32)) {
        *lock(&self.configured) = size;
    }

    /// A cloned snapshot of every record captured so far, in call order.
    pub fn records(&self) -> Vec<PresentRecord> {
        lock(&self.records).clone()
    }

    /// Deltas (µs) between consecutive [`RecordKind::Present`] records — the
    /// inter-present cadence the pacing assertions run on. Other kinds are
    /// skipped so a reconfigure or a readback probe never pollutes the
    /// pacing series.
    pub fn intervals_us(&self) -> Vec<i64> {
        let records = lock(&self.records);
        let mut out = Vec::new();
        let mut prev: Option<Instant> = None;
        for r in records.iter() {
            if r.kind != RecordKind::Present {
                continue;
            }
            if let Some(p) = prev {
                out.push(r.at.duration_since(p).as_micros() as i64);
            }
            prev = Some(r.at);
        }
        out
    }

    /// Drop every captured record (the stored "current" frame is sink STATE,
    /// not a record, and is deliberately kept — clearing between measurement
    /// windows must not fake a dims change on the next present).
    pub fn clear(&self) {
        lock(&self.records).clear();
    }

    /// Phase 49 (plan 49-01, OQ4): a cloned snapshot of every landing stamp
    /// the present loop reported via
    /// [`crate::PresentSink::note_presented_stamp`], in call order — each a
    /// `(monotonic Instant, presented timeline_us)` pair. This is what lets a
    /// harness assert "a present LANDED AT the seek target" (stamp within one
    /// frame_step of the target, recorded after the seek instant) instead of
    /// merely "a present happened" (the 48-01 `records()` stream).
    pub fn presented_stamps(&self) -> Vec<(Instant, i64)> {
        lock(&self.stamps).clone()
    }

    fn push(&self, record: PresentRecord) {
        lock(&self.records).push(record);
    }

    /// The trait-doc `dims_changed` compare: does `frame` differ in content
    /// dimensions from the previously stored frame? `true` when nothing was
    /// stored yet (the first frame establishes the content rect).
    fn dims_changed_against(stored: &Option<engine::Frame>, frame: &engine::Frame) -> bool {
        stored
            .as_ref()
            .map(|s| s.width != frame.width || s.height != frame.height)
            .unwrap_or(true)
    }

    /// Remember `frame` as "current", reusing the existing allocation when
    /// one is already stored (`clone_from` — the recording overhead should
    /// perturb the pacing it measures as little as possible).
    fn store_frame(stored: &mut Option<engine::Frame>, frame: &engine::Frame) {
        match stored {
            Some(s) => s.clone_from(frame),
            None => *stored = Some(frame.clone()),
        }
    }
}

impl Default for RecordingPresentSink {
    fn default() -> Self {
        Self::new()
    }
}

impl PresentSink for RecordingPresentSink {
    fn reconfigure_if_dirty(&self) -> Option<(u32, u32)> {
        let taken = lock(&self.dirty).take();
        if let Some((w, h)) = taken {
            self.push(PresentRecord {
                at: Instant::now(),
                width: w,
                height: h,
                dims_changed: false,
                kind: RecordKind::Reconfigure,
            });
        }
        taken
    }

    fn present(&self, frame: &engine::Frame) -> Result<bool, engine::EngineError> {
        let mut stored = lock(&self.stored);
        let dims_changed = Self::dims_changed_against(&stored, frame);
        Self::store_frame(&mut stored, frame);
        drop(stored);
        self.push(PresentRecord {
            at: Instant::now(),
            width: frame.width,
            height: frame.height,
            dims_changed,
            kind: RecordKind::Present,
        });
        Ok(dims_changed)
    }

    fn present_overlay(
        &self,
        clean: Option<&engine::Frame>,
        annotated: &engine::Frame,
    ) -> Result<bool, engine::EngineError> {
        // Per the trait doc (D-46-03-02 / Pitfall 2): store ONLY the CLEAN
        // frame — never the annotated one, or a resize re-present would
        // double the ink. `dims_changed` is computed against `clean`, and is
        // `false` when `clean` is `None` (nothing new arrived).
        let mut stored = lock(&self.stored);
        let dims_changed = match clean {
            Some(clean) => {
                let changed = Self::dims_changed_against(&stored, clean);
                Self::store_frame(&mut stored, clean);
                changed
            }
            None => false,
        };
        drop(stored);
        self.push(PresentRecord {
            at: Instant::now(),
            width: annotated.width,
            height: annotated.height,
            dims_changed,
            kind: RecordKind::PresentOverlay {
                clean_was_some: clean.is_some(),
            },
        });
        Ok(dims_changed)
    }

    fn current_frame(&self) -> Option<engine::Frame> {
        lock(&self.stored).clone()
    }

    fn configured_size(&self) -> (u32, u32) {
        *lock(&self.configured)
    }

    fn mark_shown_once(&self) -> bool {
        let mut shown = lock(&self.shown_once);
        if *shown {
            return false;
        }
        *shown = true;
        true
    }

    fn composite_layers(
        &self,
        _layers: &[engine::Layer],
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>> {
        // Record the CALL (plan 48-11's stall test asserts readback requests
        // interleave with presents without stretching the present cadence),
        // answer `None` — a real, exercised production answer ("no surface
        // managed"), exactly the case every caller already folds in.
        self.push(PresentRecord {
            at: Instant::now(),
            width,
            height,
            dims_changed: false,
            kind: RecordKind::CompositeLayers,
        });
        None
    }

    fn has_surface(&self) -> bool {
        true // the loop must run — a headless harness always "has" a surface
    }

    fn compositor(&self) -> Option<Arc<engine::Compositor>> {
        self.compositor.clone()
    }

    /// Phase 49 (plan 49-01, OQ4): record the landing stamp the loop reported.
    /// The ONE overriding implementor of the defaulted trait method — the
    /// production sinks keep the no-op default (instrumentation never touches
    /// the live present path).
    fn note_presented_stamp(&self, timeline_us: i64) {
        lock(&self.stamps).push((Instant::now(), timeline_us));
    }

    /// The GPU-RESIDENT present (Phase 48, plan 48-08): record the same
    /// `(Instant, w, h, dims_changed)` tuple as `present`, do NO GPU work
    /// (headless double — and GPU-06 forbids a readback on this path anyway).
    /// `dims_changed` follows the trait contract against the previously-shown
    /// frame of EITHER kind: the last GPU dims when one is current, else the
    /// stored CPU frame's dims, else `true` (first frame establishes the
    /// content rect, mirroring `dims_changed_against`).
    /// Phase 57 (plan 57-06): a recording sink CAN carry a composited texture —
    /// it owns a real offscreen [`engine::Compositor`] when built with
    /// [`RecordingPresentSink::with_compositor`] — so the headless harness is
    /// what exercises [`crate::RingPayload::Composited`] end to end.
    ///
    /// `false` without a compositor: there is nothing to sample the target
    /// with, and a producer told `true` would push entries this sink could not
    /// read.
    fn supports_composited_present(&self) -> bool {
        self.compositor.is_some()
    }

    /// Record an already-composited present (Phase 57, plan 57-06).
    ///
    /// A headless sink has no swapchain to blit into, so it does the ONE thing
    /// it can that keeps every existing pixel-asserting test working: samples
    /// the composited target through `blit_texture_to_rgba` — the readback twin
    /// of `blit_texture_to_surface`, sharing its shader — and stores the result
    /// as "current", so [`crate::PresentSink::current_frame`] answers with the
    /// composited pixels exactly as it did when the producer handed over CPU
    /// bytes. `multilayer_pixel_pin.rs` polls precisely that, and needed no
    /// change.
    ///
    /// The readback here is a HARNESS cost, not a path cost: on the production
    /// route this method is never reached (the shells answer `false` to
    /// `supports_composited_present`, so the producer reads back on its own
    /// thread instead).
    fn present_composited(
        &self,
        target: &engine::PooledTarget,
        content_w: u32,
        content_h: u32,
    ) -> Result<bool, engine::EngineError> {
        let Some(compositor) = self.compositor.as_ref() else {
            return Ok(false);
        };
        let rgba = compositor.blit_texture_to_rgba(target.view(), content_w, content_h)?;
        let frame = engine::Frame {
            width: content_w,
            height: content_h,
            rgba,
        };
        let mut stored = lock(&self.stored);
        let dims_changed = Self::dims_changed_against(&stored, &frame);
        Self::store_frame(&mut stored, &frame);
        drop(stored);
        // A composited frame is now current — release any stored GPU frame's
        // dims so a later `present_gpu` compares against what is actually on
        // screen (mirroring the production sinks' `gpu_frame = None`).
        *lock(&self.gpu_dims) = None;
        self.push(PresentRecord {
            at: Instant::now(),
            width: content_w,
            height: content_h,
            dims_changed,
            kind: RecordKind::PresentComposited,
        });
        Ok(dims_changed)
    }

    #[cfg(all(windows, feature = "hwdecode"))]
    fn present_gpu(&self, frame: &engine::GpuFrame) -> Result<bool, engine::EngineError> {
        let stored = lock(&self.stored);
        let mut gpu_dims = lock(&self.gpu_dims);
        let prev = (*gpu_dims).or_else(|| stored.as_ref().map(|s| (s.width, s.height)));
        let dims_changed = prev
            .map(|(w, h)| w != frame.width || h != frame.height)
            .unwrap_or(true);
        *gpu_dims = Some((frame.width, frame.height));
        drop(gpu_dims);
        drop(stored);
        self.push(PresentRecord {
            at: Instant::now(),
            width: frame.width,
            height: frame.height,
            dims_changed,
            kind: RecordKind::PresentGpu,
        });
        Ok(dims_changed)
    }
}
