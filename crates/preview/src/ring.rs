//! Phase 18.3 (PERF-01) — read-ahead composited-frame ring buffer.
//!
//! A bounded, **byte-budgeted** queue of finished, ready-to-present playback
//! frames that sits between frame PRODUCTION (decode + composite, on the
//! background [`spawn_producer`] thread) and frame PRESENTATION (`present_frame`
//! on the present thread). Stage 2 (18.3-04): ONE producer sources EVERYTHING —
//! single-layer streams, timeline gaps, AND multi-layer composites (per-frame
//! `resolve_multilayer` → per-layer decode → one composite via a cloned
//! `Arc<Compositor>`; see the Phase 57 section below for what that
//! per-layer decode is today). Merge/unmerge
//! boundaries are INTERNAL here: crossing one is just "some frames cost more to
//! produce", absorbed by the ring's runway; the presenter cannot tell single
//! from multi and does not care.
//!
//! Faithful to `.planning/research/preview-ring-buffer-design.md`:
//!   * §1 — the seam, the unified [`RingEntry`], and the byte-budget depth table.
//!   * §2 — the producer's responsibilities, store-access rule, and lifecycle.
//!   * §4 — [`RingCtl`] + the gen-stamped flush handshake
//!     (restart_pos store → gen bump → clear → notify_all).
//!   * §6 — the failure modes the headless tests pin (pop-by-count A/V drift,
//!     wall-paced producer, no-flush-on-seek, stale-frame race, unbounded ring).
//!
//! Nothing here crosses IPC: frames flow ring → `present_frame` → wgpu surface
//! only (CLAUDE.md rule). The ring holds plain `engine::Frame` memory; producer
//! exit drops its decode session so the engine `Drop` impls reap every ffmpeg
//! child.
//!
//! # Phase 46 (plan 46-09): this file MOVED here whole
//!
//! It IS `src-tauri/src/preview_ring.rs`, relocated into `crates/preview` and
//! deleted from the shell. The move is byte-identical wherever the crate
//! boundary allows; the only lines that changed are the ones it forces:
//!
//! * `pub(crate)` -> `pub` on the 21 items the shell still names — the exact
//!   analog of "visible to the whole shell crate" once the definitions live in
//!   a different crate.
//! * [`spawn_producer`] and `producer_loop` take an OWNED
//!   `Arc<dyn PreviewHost>` instead of an `AppHandle` plus a
//!   `<R: Runtime>` generic. Owned, not the `&dyn PreviewHost` that waves
//!   46-06/46-08 threaded through [`lookahead_probe`] and
//!   [`push_with_lookahead`], because the producer THREAD outlives the frame
//!   that spawns it.
//! * `is_source` is read through [`PreviewHost::playback_mirror`] rather than
//!   `app.try_state::<Arc<PlaybackMirror>>()`. The shell adapter's
//!   always-available fallback mirror ("not playing, position 0, Program
//!   mode") reproduces the old `.unwrap_or(false)` exactly.
//! * `preview::` and `crate::native_surface::` prefixes became `crate::` —
//!   `resolve.rs`, `multilayer.rs` and this file share a crate now.
//!
//! Nothing else moved. The ring policy (depth table, catch-up/hold pop, the
//! gen-stamped flush handshake), the producer state machine, and all six unit
//! tests at the bottom are the same bytes the shell held.
//!
//! # Phase 48 (plan 48-08): the GPU payload variant + depth-capacity mode
//!
//! [`RingEntry`]'s payload became [`RingPayload`] — today's CPU frame OR a
//! GPU-resident [`engine::GpuFrame`] carried WHOLE (texture + refcounted
//! `AVFrame` keeper in one struct, 48-05's drop-together contract: dropping an
//! entry returns the hw-frame-pool slice and releases the texture view
//! atomically, T-48-08-01). [`RingCtl`] gained [`RingCapacity`]: the existing
//! byte budget is untouched for CPU rings ([`RingCtl::new`], behavior
//! byte-identical), while GPU rings are budgeted by COUNT
//! ([`RingCtl::with_gpu_depth`]) because every held GPU entry PINS a pool
//! slice the decoder cannot reuse — pool pins are counted, never
//! byte-budgeted (GPU-03 prohibits `RING_BUDGET_BYTES` on the GPU ring by
//! name).
//!
//! # Phase 48 (plan 48-09): the producer flip — GPU entries are LIVE
//!
//! The single-layer arm of [`producer_loop`] now runs a per-media decode-path
//! gate at clip start (GPU-05): when a live VRAM budget handle is armed
//! (`spawn_producer_gpu` — only the real app's `native_surface::setup` arms
//! it; every test route and `spawn_producer` stay CPU-only and byte-identical)
//! and in-process hardware decode opens for the clip's media, the clip is
//! DELEGATED to a dedicated `"rudis-gpu-decode"` thread (CONTEXT D-03: decode
//! on its own thread; the present thread only samples and composites) that
//! decodes → zero-copy imports → pushes [`RingPayload::Gpu`] entries under the
//! SAME gen/flush/stop contract as the CPU producer. Ring depth while a GPU
//! clip plays is `gpu_ring_depth`'s min-of-two-ceilings (VRAM budget vs
//! hw-frame-pool − headroom), entered via [`RingCtl::enter_gpu_depth`] and
//! re-derived on every live budget change — a budget SHRINK evicts the oldest
//! entries and KEEPS PLAYING degraded, never dropping to software decode
//! (CONTEXT D-12; software fallback is reserved for decode CAPABILITY
//! failures, which route THAT media to the untouched CPU sidecar path below).
//!
//! # Phase 57 (plan 57-06): the multi arm is GPU-resident too
//!
//! The multi-layer arm no longer means "one ffmpeg CLI child per layer, blended
//! on the CPU, read back per frame". Every hardware-eligible visible layer now
//! owns a [`crate::layer_sessions::LayerSession`] — an in-process D3D11VA decode
//! thread publishing zero-copy `GpuFrame`s into a HOLD slot — and the producer
//! per tick:
//!
//! 1. reconciles the session set with the resolved stack (`sync_to_stack`),
//! 2. hands the software `LayerDecoderPool` only what hardware did NOT take
//!    (D-04: demoted per layer, never deleted, never all-or-nothing),
//! 3. gathers ONE track-ordered `Vec<MixedLayer>` by walking `stack.layers` and
//!    asking, per spec, "hardware HOLD slot or software frame" (Pitfall 5 — the
//!    list is never two lists concatenated),
//! 4. renders it ONCE into a persistent pooled target via
//!    `composite_mixed_layers_to_target`.
//!
//! What that deletes from the steady-state path: the per-layer CLI children
//! (PLAY-01), the per-frame layer-texture uploads and readback round trip
//! (PLAY-02/D-07), and the whole-`Project` deep clone `resolve_multilayer` used
//! to make per produced frame (PLAY-07/D-08 — `Store::PROJECT_CLONE_COUNT` is
//! the counter that proves it stayed deleted).
//!
//! The composited target rides the ring as [`RingPayload::Composited`] when the
//! sink can present a texture ([`crate::PresentSink::supports_composited_present`]),
//! and is read back on THIS thread into a [`RingPayload::Cpu`] entry when it
//! cannot — byte-identically, since 57-04 pinned
//! `composite_mixed_layers_to_target` + `blit_texture_to_rgba` against
//! `composite_layers_to_rgba` at zero differing bytes. One composite path, two
//! carriers.
//!
//! # Phase 57 (plan 57-08): dynamic playback resolution governs the composite
//!
//! The multi arm now carries a [`crate::dynres::DynResController`] — a stack
//! local of THIS thread — and scales the composite target dims by the level it
//! answers (PLAY-05/D-09). Full -> 1/2 -> 1/4, on a SUSTAINED budget miss only,
//! back to full at the existing stop/flush handshake.
//!
//! **Scoped to the multi arm, deliberately.** The single-clip arms — CPU
//! sidecar and GPU delegation — are untouched, because a level is a lever over
//! COMPOSITE cost and those arms do not composite: they hand the presenter one
//! decoded frame, at its native resolution, exactly as they did before this
//! phase. Both already sustain real time on every BENCH fixture (F3's dense-cut
//! single track delivers 1690/1690 at `composite=0`), so there is no cost there
//! to cut and scaling them would trade picture quality for nothing. It would
//! also break their pixel identity with export for no benefit.
//!
//! What the presenter sees is unchanged in kind: a `Composited` entry carries
//! its OWN `w`/`h`, and `contain_fit_viewport` letterboxes whatever arrives to
//! the surface — so entries composited at two different levels can sit in the
//! ring together and each presents correctly.

use crate::{MultiLayerStack, PreviewHost};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Hard byte budget for buffered ready-to-present frames (design §1/§5).
/// DECIMAL MB (not MiB) — the ONLY value consistent with the design §1
/// byte-budget table: 1080p RGBA (8_294_400 B) -> depth 19 (~0.63s runway);
/// 160 * 1024 * 1024 (=167_772_160) would give 20 and break both the table
/// AND the ring_depth_matches_design_table test below.
pub const RING_BUDGET_BYTES: usize = 160_000_000; // 160 MB (decimal)
/// Depth floor: even huge (4K native) entries keep a few frames of runway.
pub const RING_MIN_DEPTH: usize = 8;
/// Depth cap: small frames must not buffer unbounded latency (~1s max).
pub const RING_MAX_DEPTH: usize = 32;

/// Wall cost, in microseconds, of the producer's LAST multi-layer produce tick
/// — resolve → sessions → gather → composite → payload.
///
/// Phase 57 (plan 57-06), consumed by plan 57-08: dynamic playback resolution
/// drops a level on a **sustained** budget miss (D-09), and "budget miss" means
/// this number against the frame step. One relaxed store per produced multi
/// frame; no behaviour depends on it here.
pub static LAST_MULTI_PRODUCE_US: AtomicU64 = AtomicU64::new(0);

/// Dynamic-playback-resolution DEGRADATION transitions the producer has made,
/// process-wide, since start.
///
/// Quick task `260825-mgq`: OBSERVABILITY ONLY, the
/// [`crate::framedrop::FRAMEDROP_SKIPPED_TICKS`] /
/// [`crate::occlusion::OCCLUSION_CULLED_LAYERS`] idiom. Incremented at the ONE
/// site that already announces a level change on stderr, so the counter and the
/// `preview_ring: playback resolution` line can never disagree.
///
/// Two things it deliberately does NOT count, both load-bearing when a zero is
/// read as evidence:
///
/// * `DynResController::on_tick` never RAISES a level (the ladder is one-way;
///   recovery is pause-only, `dynres.rs`), so every count here is a
///   degradation.
/// * The pause-path reset ([`crate::dynres::DynResController::on_pause`])
///   does not pass this site and is
///   uncounted. A snap-back is not a transition the playback budget caused.
///
/// So `DYNRES_TRANSITIONS` unchanged across a measured span is a positive
/// finding, not an absent one: it proves no sustained budget miss ever reached
/// `MISS_STREAK_TICKS`. Snapshot and difference -- one process can drive many
/// producers.
pub static DYNRES_TRANSITIONS: AtomicU64 = AtomicU64::new(0);


/// Cumulative microseconds the producer has spent PARKED on the composite
/// target pool — i.e. waiting for the presenter to drain a target, not doing
/// work (Phase 57, plan 57-08).
///
/// Instrumentation only; no behaviour reads it. It exists for two reasons, and
/// both are load-bearing:
///
/// 1. **It is subtracted from the produce cost the PLAY-05 hysteresis sees.**
///    A producer that is comfortably keeping up is, by definition, blocked on
///    backpressure most of every tick, so the raw tick duration converges on one
///    frame step no matter how much headroom the machine has. Feeding that to
///    the controller would park it permanently on the edge of its own threshold.
///    See `dynres::DynResController::on_tick`.
/// 2. **It measures the runway hypothesis for F1's inherited tail regression.**
///    57-06 and 57-07 both recorded F1's `max_ms` moving 49.24 -> 77.98 ms and
///    named the likely mechanism — a composited entry pins one of the FIXED
///    `COMPOSITE_TARGET_POOL_DEPTH = 4` targets where a 1080p CPU composite was
///    byte-budgeted to a ring depth of 19 — and both declined to act on a
///    hypothesis. A producer that is target-starved spends measurable time
///    HERE; one that is not, does not. That makes the hypothesis a number.
pub static COMPOSITE_TARGET_WAIT_US: AtomicU64 = AtomicU64::new(0);

/// How many multi-layer produce ticks had to wait at all for a composite target
/// (Phase 57, plan 57-08). The denominator for [`COMPOSITE_TARGET_WAIT_US`]:
/// "the producer waited 40 s in total" means something very different at 1 tick
/// than at 1500.
pub static COMPOSITE_TARGET_WAIT_TICKS: AtomicU64 = AtomicU64::new(0);

/// TEST-ONLY: microseconds of artificial cost to add to every multi-layer
/// produce tick, read ONCE at producer start (Phase 57, plan 57-08).
///
/// Unset or `0` is a total no-op — the shipping app never sets it and pays
/// nothing for its existence beyond one `std::env::var` at thread start. Set to
/// e.g. `50000`, every multi tick sleeps 50 ms INSIDE the measured produce
/// window, which is what lets an end-to-end test drive a genuine, sustained
/// budget miss on a machine that has far too much headroom to miss on its own.
///
/// An env var rather than a parameter for the same reason
/// [`crate::DISABLE_PREWARM_ENV`] and `RUDIS_DISABLE_HWDECODE` are: the decision
/// point is inside a thread three call levels below anything an integration test
/// can reach, and every spawn signature on the way there is frozen (D-01/D-13).
pub const TEST_PRODUCE_SLOWDOWN_ENV: &str = "RUDIS_TEST_PRODUCE_SLOWDOWN_US";

/// Byte-budgeted depth for a given frame size — clamp(budget/bytes, 8, 32).
///
/// Design §1 depth table (documented verbatim):
///
/// | Frame size                         | Depth | Runway @30fps | Peak ring memory |
/// |------------------------------------|-------|---------------|------------------|
/// | 1080p (8_294_400 B)                | 19    | ~0.63 s       | ~158 MB          |
/// | 720p (3_686_400 B)                 | 32*   | ~1.07 s       | ~118 MB          |
/// | 4K single-layer degenerate (33 MB) | 8*    | ~0.27 s       | ~265 MB          |
///
/// `*` = clamped by [`RING_MAX_DEPTH`] / [`RING_MIN_DEPTH`]. Byte-budgeting
/// handles the "single-layer entries are native-res" wrinkle automatically (a
/// 4K source self-limits its depth). The `.max(1)` guard means a zero-byte
/// frame never divides by zero (clamps to the floor).
pub fn ring_depth(frame_bytes: usize) -> usize {
    (RING_BUDGET_BYTES / frame_bytes.max(1)).clamp(RING_MIN_DEPTH, RING_MAX_DEPTH)
}

/// The payload of one ring entry (Phase 48, plan 48-08): the frame either as
/// today's CPU bytes or as a GPU-resident hardware-decoded frame.
///
/// The GPU variant carries the WHOLE [`engine::GpuFrame`] — imported texture,
/// plane views AND the refcounted `AVFrame` keeper in ONE struct (48-05's
/// drop-together contract): dropping a ring entry returns the hw-frame-pool
/// slice to the decoder and releases the texture view ATOMICALLY. Splitting
/// the two would either leak a GPU handle or free a pool slice a view still
/// points at (T-48-08-01) — the type system carries the rule, not convention.
///
/// Nothing constructs the GPU variant yet — plan 48-09 flips the producer;
/// the CPU path stays the live default after 48-08 (structural extension only).
pub enum RingPayload {
    /// Today's CPU frame — native-res raw (single-layer, byte-identical
    /// pixels to the Phase-9 path) or project-res composite (multi-layer,
    /// Stage 2) — exactly what [`RingEntry`]'s old `frame` field held.
    Cpu(engine::Frame),
    /// A hardware-decoded, GPU-resident frame (Windows + hwdecode builds
    /// only — the type does not exist elsewhere). Every held entry PINS one
    /// hw-frame-pool slice the decoder cannot reuse, which is why GPU rings
    /// are budgeted by COUNT ([`RingCapacity::Depth`]) and never by the
    /// CPU-RAM-tuned byte budget (GPU-03's prohibition).
    #[cfg(all(windows, feature = "hwdecode"))]
    Gpu(engine::GpuFrame),
    /// Phase 57 (plan 57-06, PLAY-02/D-07): an ALREADY-COMPOSITED multi-layer
    /// frame, still on the GPU. The producer's multi arm renders every visible
    /// layer — hardware-resident and CPU-fallback, mixed — into a persistent
    /// pooled target at PRODUCE time, so the presenter's whole job becomes a
    /// sample-and-blit and the `upload → render → readback → poll(Wait) →
    /// row-strip → re-upload` round trip D-07 names leaves the playback path.
    /// (The upload call is deliberately NOT spelled out here: `hwdecode_zero_copy.rs`'s
    /// 48-05 static audit scans this file's TEXT for it, and a doc comment that
    /// names it reads as a violation — correctly, since the audit cannot tell
    /// prose from a call, and weakening it to tell them apart would weaken the
    /// only thing standing between the zero-copy claim and a comment.)
    ///
    /// **NOT hwdecode-gated**, deliberately: the composite is plain `wgpu`, and
    /// an all-software layer set composes through the very same entry point
    /// (D-04's other half). A build with no hardware decode still produces
    /// these; it simply has no `MixedLayer::Gpu` inputs to feed them.
    Composited(CompositedEntry),
}

/// The payload of a [`RingPayload::Composited`] entry.
///
/// # The target returns itself
///
/// `target` is an [`engine::PooledTarget`], whose `Drop` hands its slot back to
/// the [`engine::CompositeTargetPool`] it came from. That is what makes this
/// variant leak-proof under every ring path that discards entries without
/// presenting them — a seek flush (`RingCtl::flush` clears the queue), a
/// budget-shrink eviction (`evict_oldest_to`), a stale-gen pop, or the
/// presenter's catch-up drop. Each of those drops the entry, which drops the
/// target, which frees the slot. There is no release call to forget.
///
/// The pool is FIXED at [`engine::COMPOSITE_TARGET_POOL_DEPTH`] targets, so the
/// number of composited entries in flight is bounded by the pool, not by the
/// CPU byte budget — see [`RingCtl::entry_depth`].
pub struct CompositedEntry {
    /// The composited frame, on the GPU. Sampled by the presenter, never read
    /// back on the present path.
    pub target: engine::PooledTarget,
    /// Composited content width (the project canvas, or a degraded level once
    /// plan 57-08 lands).
    pub w: u32,
    /// Composited content height.
    pub h: u32,
}

/// One produced, ready-to-present playback frame (design §1 — ONE unified type
/// for both the single-layer and multi-layer paths). The entry deliberately
/// does NOT carry a single-vs-multi marker: boundaries are internal to the
/// producer, and the presenter cannot tell (and must not care) — `present_frame`
/// contain-fits either a native-res raw frame or a project-res composite.
pub struct RingEntry {
    /// The produced frame (see [`RingPayload`]; this was `frame:
    /// engine::Frame` until plan 48-08 — CPU consumers now reach the frame
    /// through [`RingEntry::frame`] or by matching the payload).
    pub payload: RingPayload,
    /// Presentation timestamp on the ACTIVE monitor's clock (Program timeline
    /// µs, or Source position µs). Strictly increasing within a gen.
    pub timeline_us: i64,
    /// Flush generation stamped at production time. The presenter discards
    /// entries whose gen != the current one (belt-and-braces vs. flush races).
    pub gen: u64,
}

impl RingEntry {
    /// The CPU frame, when this entry carries one — the convenience accessor
    /// every pre-48-08 `.frame` consumer moved to. `None` for GPU-resident
    /// entries, which never collapse to CPU bytes on the preview path
    /// (GPU-06; the present loop routes them to `PresentSink::present_gpu`).
    pub fn frame(&self) -> Option<&engine::Frame> {
        match &self.payload {
            RingPayload::Cpu(frame) => Some(frame),
            #[cfg(all(windows, feature = "hwdecode"))]
            RingPayload::Gpu(_) => None,
            // Mirrors the Gpu arm: a composited entry never collapses to CPU
            // bytes on the preview path (D-07). The presenter routes it to
            // `PresentSink::present_composited`.
            RingPayload::Composited(_) => None,
        }
    }
}

/// How the ring bounds its depth (Phase 48, plan 48-08).
pub enum RingCapacity {
    /// The existing CPU byte budget: depth derives PER ENTRY from
    /// [`ring_depth`] over the frame's byte length, exactly as before this
    /// enum existed. [`RingCtl::new`] constructs this with
    /// [`RING_BUDGET_BYTES`] — CPU behavior is byte-identical (GPU-03's
    /// prohibition applies to the GPU ring; the CPU semantics are NOT
    /// touched).
    Bytes(usize),
    /// GPU depth cap: a FIXED maximum entry count. GPU entries pin
    /// hw-frame-pool slices, so capacity is a count of pins — the caller
    /// derives it as the min of two independent ceilings (VRAM budget vs
    /// `pool_size − headroom`, 48-07's `gpu_ring_depth`) — NEVER the
    /// CPU-RAM-tuned byte budget.
    Depth(usize),
}

/// The shared control block between the presenter (owner of flush/stop) and the
/// producer (read-only on the atomics; sole pusher). Design §4.
pub struct RingCtl {
    q: Mutex<VecDeque<RingEntry>>,
    /// Capacity mode (plan 48-08): fixed at construction (`Bytes` for the CPU
    /// ring, `Depth` for the GPU ring); only the `Depth` VALUE mutates at
    /// runtime ([`RingCtl::set_gpu_depth`], 48-09's budget-shrink path).
    /// Locked briefly inside the push paths (which already hold `q`) and by
    /// `set_gpu_depth` (which never takes `q`) — one lock order, no inversion.
    capacity: Mutex<RingCapacity>,
    /// The producer parks here when a push would exceed the byte budget; the
    /// presenter `notify_one`s after every pop and `notify_all`s on flush/stop.
    space: Condvar,
    /// Flush generation. Starts at 1 so a 0-stamped entry can never be current.
    pub gen: AtomicU64,
    /// Where the producer resumes after a flush (stored BEFORE the gen bump so
    /// the producer never reads a stale restart target — see [`RingCtl::flush`]).
    pub restart_pos: AtomicI64,
    /// Set on pause/stop; the producer exits within one bounded op.
    pub stop: AtomicBool,
}

impl RingCtl {
    /// A fresh ring: gen 1 (so a 0-stamped entry can never be current),
    /// restart_pos 0, not stopped. Byte-budget capacity — the pre-48-08 CPU
    /// behavior, byte-identical.
    pub fn new() -> Arc<RingCtl> {
        Arc::new(RingCtl {
            q: Mutex::new(VecDeque::new()),
            capacity: Mutex::new(RingCapacity::Bytes(RING_BUDGET_BYTES)),
            space: Condvar::new(),
            gen: AtomicU64::new(1),
            restart_pos: AtomicI64::new(0),
            stop: AtomicBool::new(false),
        })
    }

    /// A fresh GPU ring (Phase 48, plan 48-08): capacity is a FIXED entry
    /// count — every held GPU entry pins a hw-frame-pool slice, so depth is a
    /// count of pins (min of the VRAM-derived depth and `pool_size −
    /// headroom`, 48-07's `gpu_ring_depth`), never a byte budget. Same gen /
    /// restart / stop semantics as [`RingCtl::new`] in every other respect.
    pub fn with_gpu_depth(depth: usize) -> Arc<RingCtl> {
        Arc::new(RingCtl {
            q: Mutex::new(VecDeque::new()),
            capacity: Mutex::new(RingCapacity::Depth(depth)),
            space: Condvar::new(),
            gen: AtomicU64::new(1),
            restart_pos: AtomicI64::new(0),
            stop: AtomicBool::new(false),
        })
    }

    /// Update the GPU depth cap (plan 48-09's budget-shrink eviction calls
    /// this on a VRAM budget-change notification). Evict-oldest down to
    /// `new_depth` happens at the PRODUCER — this only updates the cap and
    /// wakes parked pushers so they re-evaluate against it. A no-op (with a
    /// debug assert) on a byte-budget ring, where no GPU depth exists to set.
    pub fn set_gpu_depth(&self, new_depth: usize) {
        let mut cap = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        debug_assert!(
            matches!(*cap, RingCapacity::Depth(_)),
            "set_gpu_depth called on a Bytes-budget (CPU) ring"
        );
        if let RingCapacity::Depth(d) = &mut *cap {
            *d = new_depth;
        }
        drop(cap);
        self.space.notify_all();
    }

    /// Enter (or refresh) GPU depth-capacity mode on THIS ring — plan 48-09's
    /// per-clip transition. The presenter's ring is a long-lived local shared
    /// with the present thread, so a fresh [`RingCtl::with_gpu_depth`] ring
    /// cannot be swapped in mid-play; instead the GPU producer switches the
    /// SHARED ring's capacity for the duration of a delegated clip and
    /// restores it via [`RingCtl::exit_gpu_depth`] the moment the delegation
    /// ends. `depth` is `gpu_ring_depth`'s min-of-two-ceilings — the ONLY
    /// legitimate source of a GPU depth (Pitfall 4). CPU entries queued at the
    /// transition simply count against the depth; pushes re-evaluate on wake.
    pub fn enter_gpu_depth(&self, depth: usize) {
        let mut cap = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        *cap = RingCapacity::Depth(depth);
        drop(cap);
        self.space.notify_all();
    }

    /// Return this ring to the CPU byte budget (the pre-48-08 semantics,
    /// byte-identical) — called by the producer when a GPU clip delegation
    /// ends, whatever the reason (clip end, flush, stop, mid-clip failure), so
    /// the NEXT content produced (gap black, multi-layer composite, or a
    /// software-fallback clip) is budgeted exactly as it always was. GPU
    /// entries still queued keep draining through the presenter unaffected
    /// (capacity only gates pushes).
    pub fn exit_gpu_depth(&self) {
        let mut cap = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        *cap = RingCapacity::Bytes(RING_BUDGET_BYTES);
        drop(cap);
        self.space.notify_all();
    }

    /// Evict the OLDEST queued entries down to `depth` (plan 48-09's
    /// budget-shrink reaction, CONTEXT D-12): dropping a GPU entry unpins its
    /// hw-frame-pool slice immediately, and playback KEEPS GOING degraded —
    /// the presenter simply has less runway. Eviction lives at the producer
    /// (48-08's contract: `set_gpu_depth` only updates the cap). Returns the
    /// number of entries dropped.
    pub fn evict_oldest_to(&self, depth: usize) -> usize {
        let mut q = self.lock_q();
        let mut evicted = 0usize;
        while q.len() > depth {
            q.pop_front();
            evicted += 1;
        }
        drop(q);
        if evicted > 0 {
            self.space.notify_one();
        }
        evicted
    }

    /// Lock the queue without ever panic-poisoning the present/producer threads.
    fn lock_q(&self) -> MutexGuard<'_, VecDeque<RingEntry>> {
        self.q.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The depth cap that applies to `entry` under this ring's capacity mode
    /// (plan 48-08). `Bytes` preserves the pre-48-08 policy exactly: the SAME
    /// [`ring_depth`] call over the CPU frame's byte length. `Depth` (GPU) is
    /// a fixed pin count, independent of entry size.
    fn entry_depth(&self, entry: &RingEntry) -> usize {
        let cap = self.capacity.lock().unwrap_or_else(|p| p.into_inner());
        match *cap {
            RingCapacity::Bytes(budget) => {
                debug_assert_eq!(
                    budget, RING_BUDGET_BYTES,
                    "Bytes mode always carries the design-table budget"
                );
                match &entry.payload {
                    RingPayload::Cpu(frame) => ring_depth(frame.rgba.len()),
                    // A GPU entry in a byte-budget ring is a producer wiring
                    // bug (GPU-03: the CPU byte budget must never budget pool
                    // pins) — pin conservatively at the floor rather than
                    // panic the producer thread.
                    #[cfg(all(windows, feature = "hwdecode"))]
                    RingPayload::Gpu(_) => {
                        debug_assert!(false, "GPU entry pushed into a Bytes-budget ring");
                        RING_MIN_DEPTH
                    }
                    // Phase 57 (57-06): a composited entry carries no CPU
                    // bytes to budget — it pins one of the FIXED
                    // `COMPOSITE_TARGET_POOL_DEPTH` persistent targets, so the
                    // pool IS the bound and the ring's cap simply agrees with
                    // it. Budgeting these by the CPU-RAM byte table would
                    // either be vacuous (zero bytes → depth 32, a cap the pool
                    // can never reach) or arbitrary.
                    RingPayload::Composited(_) => engine::COMPOSITE_TARGET_POOL_DEPTH,
                }
            }
            RingCapacity::Depth(depth) => depth,
        }
    }

    /// Push a produced entry, blocking (zero-CPU) while the ring is full. Returns
    /// `false` — caller ABANDONS the entry — when the ring was stopped, or a
    /// flush made this entry's gen stale while we waited. Returns `true` once the
    /// entry is enqueued.
    ///
    /// `wait_timeout` (not `wait`) so stop/flush can never be missed even without
    /// a matching notify; the 100ms poll is invisible in practice because the
    /// queue drains ~30/s while playing.
    pub fn push_blocking(&self, entry: RingEntry) -> bool {
        let mut q = self.lock_q();
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return false; // stopped — abandon
            }
            if entry.gen != self.gen.load(Ordering::SeqCst) {
                return false; // flushed while waiting — this entry is stale
            }
            let depth = self.entry_depth(&entry);
            if q.len() < depth {
                q.push_back(entry);
                return true;
            }
            // Full: park until a slot opens / stop / flush. Timed so a missed
            // notify can never wedge the producer.
            let (guard, _timeout) = self
                .space
                .wait_timeout(q, Duration::from_millis(100))
                .unwrap_or_else(|p| p.into_inner());
            q = guard;
        }
    }

    /// Non-blocking twin of [`RingCtl::push_blocking`] (18.3-05 Stage 3): the
    /// SAME stop/stale-gen checks, but a FULL ring returns `Err(entry)`
    /// (WouldBlock — the entry is handed back) instead of parking. The producer
    /// uses the WouldBlock to DETECT a backpressure park episode: it runs the
    /// lookahead prewarm probe once, then falls back to the blocking wait.
    /// `Ok(true)` = enqueued; `Ok(false)` = abandoned (stopped or flushed-stale),
    /// mirroring `push_blocking`'s return semantics exactly.
    pub fn try_push(&self, entry: RingEntry) -> Result<bool, RingEntry> {
        let mut q = self.lock_q();
        if self.stop.load(Ordering::SeqCst) {
            return Ok(false); // stopped — abandon (as push_blocking would)
        }
        if entry.gen != self.gen.load(Ordering::SeqCst) {
            return Ok(false); // flushed — this entry is stale, abandon
        }
        let depth = self.entry_depth(&entry);
        if q.len() < depth {
            q.push_back(entry);
            return Ok(true);
        }
        drop(q);
        Err(entry) // WouldBlock: ring full — the caller probes, then blocks
    }

    /// Catch-up/hold pop policy (design §3, MLT drop-image-keep-time). Discards
    /// any stale-gen front entries on sight, then pops while the front's
    /// timestamp is `<= target_us`, keeping only the LAST such entry (catch-up);
    /// a front newer than the target is left in place (HOLD — never pop a future
    /// frame). Notifies one parked pusher if anything was removed. Returns the
    /// kept entry (the newest `<= target`), or `None` on underrun/hold.
    pub fn pop_for_target(&self, cur_gen: u64, target_us: i64) -> Option<RingEntry> {
        let mut q = self.lock_q();
        let mut removed = false;
        let mut kept: Option<RingEntry> = None;
        loop {
            let front = match q.front() {
                Some(f) => f,
                None => break,
            };
            if front.gen != cur_gen {
                // Stale generation: discard on sight regardless of timestamp.
                q.pop_front();
                removed = true;
                continue;
            }
            if front.timeline_us <= target_us {
                // Due: this becomes the kept frame; any prior kept is dropped
                // (catch-up = keep only the newest <= target).
                kept = q.pop_front();
                removed = true;
                continue;
            }
            break; // front is in the future — HOLD it.
        }
        if removed {
            // A slot (or several) opened — wake a parked pusher.
            self.space.notify_one();
        }
        kept
    }

    /// Flush-on-seek handshake (design §4). Order is LOAD-BEARING: store the
    /// restart position BEFORE bumping the gen, so a producer that observes the
    /// gen change then reads `restart_pos` can never see a stale target. Clears
    /// the queue and wakes every parked pusher (their gen went stale → they
    /// return `false` and rebuild at `restart_pos`).
    pub fn flush(&self, restart_pos_us: i64) {
        self.restart_pos.store(restart_pos_us, Ordering::SeqCst);
        self.gen.fetch_add(1, Ordering::SeqCst);
        self.lock_q().clear();
        self.space.notify_all();
    }

    /// Terminate the producer (pause/stop): set the stop flag, clear the queue,
    /// wake every parked pusher (they return `false` and the thread exits, its
    /// session dropping → engine Drop reaps the ffmpeg children).
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.lock_q().clear();
        self.space.notify_all();
    }

    /// Current queued entry count (test/introspection only).
    pub fn len(&self) -> usize {
        self.lock_q().len()
    }
}

/// A tiny fully-black 2x2 RGBA frame (16 bytes) for timeline gaps (design §2
/// note b — tiny, so gaps buffer at the depth cap). `present_frame`
/// contain-fits it to an all-black surface, exactly like today's gap black.
fn black_gap_frame() -> engine::Frame {
    engine::Frame {
        width: 2,
        height: 2,
        rgba: vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
    }
}

/// Spawn the background frame producer (design §2). Owns ALL playback decode
/// state — the single-clip `StreamingDecodeSession`, the multi-layer
/// `LayerDecoderPool` (Stage 2), and the gap cadence; the presenter only pops.
/// `compositor` is a clone of the presenter's `Arc<Compositor>` (wgpu
/// Device/Queue are Send+Sync and internally synchronized): producer-side
/// offscreen composites never contend with the presenter's surface lock.
///
/// The producer is demand-paced by the ring's condvar backpressure: it sprints
/// until the ring is full, then sleeps (production is NOT wall-paced — that is
/// how it gets AHEAD of the playhead, the whole point). It takes the SAME brief
/// store locks the present thread takes today (`resolve_multilayer` /
/// `resolve_active`), at the same ~30/s rate under backpressure — a deliberate,
/// documented departure from the 18.2-06 warmer's snapshot rule (design §2
/// "Store access": keyframe sampling requires per-position resolution). It is a
/// second BACKEND thread; nothing crosses to the renderer.
///
/// Phase 46 (plan 46-09): `host` is the OWNED shell-services port the
/// producer thread keeps for its whole lifetime — it replaces the
/// `AppHandle<R>` this took before, and it is owned rather than borrowed
/// because the spawned thread outlives this call. The caller
/// (`present_loop`) gets one from `PresentContext::host_arc()`, so the
/// present thread and the producer thread share ONE adapter instance.
pub fn spawn_producer(
    host: std::sync::Arc<dyn PreviewHost>,
    ctl: Arc<RingCtl>,
    compositor: Arc<engine::Compositor>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rudis-preview-producer".to_string())
        // `composited_present = false`: this spawn's signature is FROZEN (D-13
        // — `crates/app-core`'s dev-dep parity twins call it at
        // export.rs:3755), and a caller that never named a sink cannot have
        // told us its sink can present a GPU texture. So this route keeps
        // pushing CPU payloads, produced through the SAME mixed composite —
        // see `producer_loop`'s multi arm.
        .spawn(move || producer_loop(host, ctl, compositor, None, false))
        .expect("spawn rudis-preview-producer thread")
}

/// Phase 48 (plan 48-09): the GPU-capable producer spawn — the SAME
/// [`producer_loop`] with the live VRAM budget handle armed. The handle is
/// what switches the per-media decode-path gate on: only the real app's
/// `native_surface::setup` (which spawned the `VramBudgetWatch`) hands one in,
/// so every existing test route — and [`spawn_producer`] itself — keeps the
/// CPU-only producer byte-for-byte.
///
/// Phase 57 (plan 57-06) adds `composited_present`: whether the sink this
/// producer feeds can present an already-composited GPU texture
/// ([`crate::PresentSink::supports_composited_present`]). `true` lets the multi
/// arm push [`RingPayload::Composited`] and delete the last readback from the
/// path; `false` reads the SAME composited target back and pushes the CPU
/// payload every pre-57 consumer expects. Either way there is exactly ONE
/// composite, through `composite_mixed_layers_to_target`.
#[cfg(all(windows, feature = "hwdecode"))]
pub fn spawn_producer_gpu(
    host: std::sync::Arc<dyn PreviewHost>,
    ctl: Arc<RingCtl>,
    compositor: Arc<engine::Compositor>,
    gpu_budget: Arc<AtomicU64>,
    composited_present: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rudis-preview-producer".to_string())
        .spawn(move || {
            producer_loop(host, ctl, compositor, Some(gpu_budget), composited_present)
        })
        .expect("spawn rudis-preview-producer thread")
}

/// Producer-side per-layer pool pull bound (18.3-04). DELIBERATELY generous —
/// NOT the present thread's 100ms (18.2): a pull returns the moment a frame
/// arrives, so a longer bound never delays production; it only caps how long a
/// genuinely-stalled decoder can hold the producer between control checks
/// (stop/flush latency worst-case ≈ active_layers × this). The generous bound
/// is what keeps SC-R2's byte-parity TRUE: the frozen pool contract's
/// Timedout-lockstep permanently offsets stamps from pixels by one frame per
/// miss (frames are positional-by-ORDER in the decoder queue), so avoiding
/// artificial cold-start misses — exactly like the export loop's BLOCKING
/// `next_frame()` — is a correctness property of the one-composite-path
/// invariant, not a tuning preference. The producer never blocks the presenter
/// (the single-layer 250ms precedent, design §2), so it can afford this.
const POOL_PULL_TIMEOUT: Duration = Duration::from_millis(1_000);

/// Lookahead horizon for the Stage-3 prewarm probe (18.3-05, design §8 Stage 3):
/// how far ahead of `prod_pos` the PARKED producer looks for an upcoming
/// layer-set change. 800ms sits INSIDE the 1080p ring runway (19 frames ≈
/// 0.63s) plus the margin the prewarmed children need to fill their 6-frame
/// channels before the boundary frame is due — the same order as MLT's
/// 25-frame read-ahead window. The probe's target is a FIXED position (the
/// boundary the future resolves to), never a moving playhead — no
/// pace-advance arithmetic anywhere (nle-boundary-playback-techniques.md Q4:
/// every surveyed NLE starts early; none catches up).
pub const LOOKAHEAD_US: i64 = 800_000;

/// 18.3-05 (Stage 3) producer-local prewarm state — anticipatory pre-roll
/// toward a FIXED upcoming boundary (the GStreamer `about-to-finish` analog).
/// Owned by the producer loop; reset WHOLESALE on the flush handshake, so a
/// superseded prewarm (seek / flush / stop) drops its sessions and the engine
/// `Drop` impls reap every child (SC-R3 preserved).
struct Lookahead {
    /// One-prewarm-per-boundary latch (T-18.3-05-01): the frame-step-bucketed
    /// future position whose boundary has already been prewarmed. While set
    /// (and not yet passed) further park-probes are skipped; cleared when
    /// `prod_pos` passes it (and on flush via the wholesale reset).
    prewarmed_for: Option<i64>,
    /// A pool prewarmed BEFORE its multi range is entered. It must ride
    /// OUTSIDE the loop's `pool` slot (the non-multi arm drops that slot every
    /// iteration); the multi arm CONSUMES it at range entry, and the
    /// boundary's first `advance` retains only the active set (leavers reaped
    /// by the frozen engine logic).
    pending_pool: Option<engine::LayerDecoderPool>,
    /// At most ONE pre-started next single-layer clip — the same lookahead
    /// generalizes ordinary cuts for free (design §8 Stage 3 item 2). Consumed
    /// when the producer's session-start step hits this clip at a matching
    /// source position; dropped (child reaped) on flush / supersede / mismatch.
    pending_session: Option<PendingSession>,
}

/// The pre-started next single-layer clip (see [`Lookahead::pending_session`]).
struct PendingSession {
    /// The clip this session was pre-started for (Program mode → `Some`).
    clip_id: Option<String>,
    /// The source position the session decodes from (probed at `future` — a
    /// sub-frame past the boundary when parks fire per produced frame).
    source_us: i64,
    session: engine::StreamingDecodeSession,
    /// SEEK-02 (Phase 49): the session's `start_with_pts` side channel,
    /// carried WITH the prewarmed session — a warm path that lost its PTS
    /// receiver would silently stamp the synthetic grid again (Pitfall 1's
    /// hardware/software split, on the warm boundary specifically).
    pts_rx: std::sync::mpsc::Receiver<(u64, Option<i64>)>,
    /// The clip's time remap, carried for the same reason as `pts_rx`: a warm
    /// boundary that lost it would stamp 1:1 and drift (260730-x2t, SR-2).
    retime: Option<rudis_core::Retime>,
}

/// Phase 49 (SEEK-02, software half): the single-clip CPU decode session
/// bundled with its `start_with_pts` PTS side-channel state. EVERY
/// construction site builds one of these (the two cold starts AND the
/// lookahead prewarm consume) — a session left on plain `start()` would be
/// the Pitfall-1 split: a path that silently loses PTS and re-creates the
/// VFR drift the 2026-07-12 diagnosis confirmed.
struct SwSession {
    session: engine::StreamingDecodeSession,
    /// Per-frame `(n, pts_us)` showinfo records from the SAME ffmpeg child
    /// (49-03's side channel: same process, same filter graph, `-fps_mode
    /// passthrough` — record n describes stdout frame n by construction).
    pts_rx: std::sync::mpsc::Receiver<(u64, Option<i64>)>,
    /// Source-media position this session decodes from (the anchor origin).
    source_us: i64,
    /// `prod_pos` at session start — the timeline position of the session's
    /// first frame (the mapping's `clip_start_pos`).
    base_pos: i64,
    /// The first real pts received. Session pts are SEEK-RELATIVE (input-side
    /// `-ss` RESETS output timestamps to ~0 — measured; stream_pts.rs's
    /// `ss_offset_semantics_are_measured` pins it), so anchoring on the first
    /// frame preserves the REAL inter-frame deltas — exactly what the
    /// synthetic grid gets wrong on VFR media.
    first_pts_us: Option<i64>,
    /// Frames pulled from this session so far — the pairing index K. Pairing
    /// is BY INDEX n, never by arrival timing: a late stderr line must not
    /// cascade an off-by-one (the reason 49-03 sends n at all).
    pulled: u64,
    /// Records that arrived ahead of their frame, keyed by n. BOUNDED
    /// (T-49-05-03): drained on every pull, entries at/below the consumed
    /// index are discarded as stale, and production is bounded by the frame
    /// channel's backpressure on the same child — the map cannot grow
    /// unboundedly.
    pts_buf: std::collections::BTreeMap<u64, Option<i64>>,
    /// Last stamp pushed from this session — `stamp_or_fallback`'s
    /// monotonicity-guard input.
    last_stamp: Option<i64>,
    /// The clip's time remap (260730-x2t). `None` for an un-retimed clip, in
    /// which case the stamp mapping is byte-for-byte the pre-retime 1:1 form.
    retime: Option<rudis_core::Retime>,
}

impl SwSession {
    /// Cold-start a PTS-carrying streaming session at `source_us`, anchored
    /// at timeline `base_pos`.
    fn start(
        path: &std::path::Path,
        source_us: i64,
        rotation: u32,
        base_pos: i64,
        retime: Option<rudis_core::Retime>,
    ) -> Result<SwSession, engine::EngineError> {
        let (session, pts_rx) =
            engine::StreamingDecodeSession::start_with_pts(path, source_us, rotation)?;
        Ok(SwSession::new(session, pts_rx, source_us, base_pos, retime))
    }

    /// Adopt a lookahead-prewarmed session (its channels already filling),
    /// anchored at the CURRENT `prod_pos` — the boundary position the
    /// producer consumes it at.
    fn from_pending(p: PendingSession, base_pos: i64) -> SwSession {
        SwSession::new(p.session, p.pts_rx, p.source_us, base_pos, p.retime)
    }

    fn new(
        session: engine::StreamingDecodeSession,
        pts_rx: std::sync::mpsc::Receiver<(u64, Option<i64>)>,
        source_us: i64,
        base_pos: i64,
        retime: Option<rudis_core::Retime>,
    ) -> SwSession {
        SwSession {
            session,
            pts_rx,
            source_us,
            base_pos,
            first_pts_us: None,
            pulled: 0,
            pts_buf: std::collections::BTreeMap::new(),
            last_stamp: None,
            retime,
        }
    }

    /// Buffer one side-channel record (capturing the anchor on the first
    /// real pts received).
    fn note_record(&mut self, n: u64, pts: Option<i64>) {
        if self.first_pts_us.is_none() {
            if let Some(p) = pts {
                self.first_pts_us = Some(p);
            }
        }
        self.pts_buf.insert(n, pts);
    }

    /// Pair the frame JUST PULLED (index K = frames pulled before this one)
    /// with its showinfo record and return its timeline stamp: the mapped
    /// real PTS when a usable record exists, else the synthetic grid stamp
    /// (counted + logged by `stamp_or_fallback` — never silent).
    fn stamp_pulled(&mut self, synthetic: i64) -> i64 {
        let k = self.pulled;
        self.pulled += 1;
        // Drain whatever the reader thread already delivered.
        while let Ok((n, p)) = self.pts_rx.try_recv() {
            self.note_record(n, p);
        }
        // Bounded secondary wait: the record for THIS frame may still be in
        // flight through the reader thread (the stderr line races the stdout
        // bytes) — a bounded recv_timeout collects it; past the bound the
        // frame is treated as PTS-less (the loud fallback), never stalled on.
        if !self.pts_buf.contains_key(&k) {
            let deadline = std::time::Instant::now() + Duration::from_millis(50);
            loop {
                if self.pts_buf.contains_key(&k) {
                    break;
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                match self.pts_rx.recv_timeout(deadline - now) {
                    Ok((n, p)) => self.note_record(n, p),
                    Err(_) => break,
                }
            }
        }
        let rec = self.pts_buf.remove(&k).flatten();
        // Discard anything below the NEXT index (stale — its frame has
        // passed) so the buffer stays bounded (T-49-05-03).
        self.pts_buf = self.pts_buf.split_off(&(k + 1));
        let mapped = match (rec, self.first_pts_us) {
            (Some(pts), Some(first)) => {
                // Anchor mapping: seek-relative pts → absolute source µs →
                // timeline, through the ONE shared helper the hardware path
                // uses (a second mapping formula here is exactly how the two
                // paths would drift apart later — locked decision).
                let abs_source_us = self.source_us + (pts - first);
                Some(crate::pts_map::map_source_pts_to_timeline(
                    abs_source_us,
                    self.source_us,
                    self.base_pos,
                    self.retime.as_ref(),
                ))
            }
            _ => None,
        };
        let stamped =
            crate::pts_map::stamp_or_fallback(mapped, synthetic, self.last_stamp, "sw");
        self.last_stamp = Some(stamped);
        stamped
    }
}

/// How much TIMELINE time is left in the resolved clip from the position it was
/// resolved at. Pre-retime this was the plain source remainder
/// `audio_end_us - source_us`; under retime a 2x clip's decode session would
/// otherwise be told it runs for twice as long as it actually occupies (quick
/// task 260730-x2t).
///
/// WR-06: this used to convert the remaining SOURCE through
/// `retimed_timeline_len_us`, which integrates from the curve's ORIGIN — i.e.
/// it applies the ramp's EARLY rates to the clip's LATE source and is wrong in
/// whichever direction the ramp leans, ending the session early (the ring
/// starves and the tail visibly stalls) or late (decoding past the clip). The
/// correct quantity is `timeline_end_us() - position_us`, which
/// [`crate::Resolved`] now carries: O(1), exact at every point in the curve,
/// and identical to the old answer for the un-retimed and constant-speed cases.
fn clip_remaining_timeline_us(r: &crate::Resolved) -> i64 {
    r.remaining_timeline_us
}

/// The earliest timeline position in `(after, horizon)` at which ANY OTHER
/// video-track clip becomes active — i.e. the first moment the single-layer
/// range playing at `after` stops being the whole picture (an upper/lower
/// layer joins → multi-layer composite; or a same-track last-wins overlap
/// takes over the pixel).
///
/// Debug session `multitrack-preview-lag-choppy` (2026-08-01, the "no multi
/// layer stuff during Play" root cause): `delegate_gpu_clip` bounded the
/// delegated GPU range ONLY by the clip's own remaining timeline extent, and
/// the producer parks in the delegation join — `resolve_multilayer`, the
/// multi-arm gate at the producer loop top, cannot run until the join
/// returns. So an overlap that BEGINS mid-clip (a long base clip continuing
/// under new upper-track clips) played single-layer GPU frames straight
/// through the whole base clip; the upper layers never rendered (mechanized:
/// `tests/multilayer_playback_probe.rs`, 150/150 gpu-single frames in the
/// overlap core, on both pre- and post-prewarm-fix trees — pre-existing since
/// 48-09, visible only on hw-decodable media). The caller clamps the
/// delegated range here so the producer resumes orchestration exactly where
/// the layer set changes.
///
/// Scan shape: a clip already active at `after` can never RE-join later
/// (active intervals are contiguous), and the delegated clip itself is
/// skipped — so the earliest `start_us` strictly inside the window IS the
/// earliest layer-set change. Starts at exactly `horizon` (an adjacent
/// same-track clip at the delegated clip's end) are the ordinary cut the
/// existing resume already handles. Text clips count (a text layer joining
/// makes the range multi-layer too — it lives on a video track).
#[cfg(all(windows, feature = "hwdecode"))]
fn next_video_layer_join(
    timeline: &rudis_core::Timeline,
    skip_clip: Option<&str>,
    after: i64,
    horizon: i64,
) -> Option<i64> {
    timeline
        .tracks
        .iter()
        .filter(|t| t.kind == rudis_core::TrackKind::Video)
        .flat_map(|t| t.clips.iter())
        .filter(|c| Some(c.id.as_str()) != skip_clip)
        .map(|c| c.start_us)
        .filter(|&s| s > after && s < horizon)
        .min()
}

/// Warm the multi-layer stack that begins where a GPU delegation ends (debug
/// session `multitrack-preview-lag-choppy`, 2026-08-01 round 2 — the "~3s
/// freeze entering the multi section" residual).
///
/// While a clip is delegated the producer sits in the delegation wait — the
/// ONE window in which `lookahead_probe` structurally cannot run (it fires
/// from producer pushes and from the multi arm; neither executes during the
/// wait). So a multi-layer range starting at the delegated range's end — a
/// capped layer join, or a base-track cut straight into an overlap — was
/// ALWAYS entered with a stone-cold pool: serialized per-layer
/// ffprobe + spawn + accurate-seek + first-frame waits at the boundary tick,
/// which real media stretches far past the ring's runway (the green probe's
/// tiny synthetics built faster than the runway, which is why it measured
/// "0ms entry delay" while the owner saw a multi-second freeze).
///
/// Called from the delegation wait loop on the producer thread. Resolves the
/// stack AT the delegation's end position; if it is multi, prewarms ALL its
/// pool sources into the PENDING pool — the multi arm's entry adopt
/// (`pool.is_none()` → `adopt_missing`) already consumes it, rebased, at the
/// boundary tick. A stale latch from before the delegation (its boundary was
/// consumed when the delegated clip started; the probe that normally clears
/// it cannot run while delegated) is superseded rather than obeyed.
///
/// Plan 59-13 gave it a SECOND caller — the producer's cache-exit wiring in the
/// multi arm — and with it the `cfg(all(windows, feature = "hwdecode"))` gate
/// this function used to carry came off. That gate never described the function:
/// its body names nothing hardware-specific (`resolve_multilayer`,
/// `pool_sources`, `LayerDecoderPool`, `Lookahead` are all unconditional), it
/// warms the SOFTWARE pool, and the gate existed only because its one caller
/// sat inside the GPU-delegation path and an uncalled function warns. The new
/// caller is not hardware-gated — the cold-respawn cost it hides is a software
/// pool cost that a CPU-only build pays in full — so gating it would have
/// switched the fix off in exactly the configuration that needs it most.
///
/// # Plan 59-16: the two costs the second caller made visible, removed
///
/// 59-14 measured 59-13's cache-exit prewarm on the committed ruler and found
/// it buying 475 ms off the cache exit while COSTING 13-15 dropped frames a
/// run, with `sw_sidecar_spawns` up **+3 per warm playthrough**. Both numbers
/// come from this function's own body, on the ONE discovery tick it runs on:
///
///  * it warmed **every** pooled source at the boundary, including the three
///    the hardware coordinator was about to claim — so three `ffmpeg` children
///    were spawned and then reaped un-adopted, ~102 ms of
///    `ExportRunDecoder::start` each, serialized on the producer thread;
///  * it seeded no dims, so it also paid one `ffprobe` per distinct media path
///    even for media that was already playing.
///
/// Both are now optional inputs the CALLERS supply, and both are `None` for the
/// Phase-57 delegation caller — whose behaviour is byte-equivalent to before,
/// deliberately, because D-3's unexplained ~180 -> 38 ms landing benefit is not
/// something this plan gambles with.
///
/// * `live_pool` — the producer's live [`engine::LayerDecoderPool`], if it has
///   one. Its probed dims are copied in (insert-if-absent) before any spawn, the
///   `lookahead_probe` precedent verbatim: prewarming media that is already
///   playing then spawns ZERO `ffprobe` on the producer thread.
/// * `hw_predict` — `(already-live hardware clip ids, hw_failed)`, i.e. the two
///   inputs [`crate::layer_sessions::sync_to_stack`] will hand
///   [`crate::layer_sessions::select_hw_clip_ids`] at the boundary tick. Given
///   them, this function makes the SAME selection PREDICTIVELY and drops those
///   layers from the warm-up, so hardware-owned sources are never warmed into
///   the software pool. The selection happens HERE rather than at the call site
///   for one reason worth stating: the stack is resolved here, so predicting
///   here keeps the resolve count at ONE on a tick this plan exists to make
///   cheaper. A misprediction is not a correctness class — the mispredicted
///   layer simply takes the pre-59-13 cold spawn at the boundary, which 59-15
///   made non-blocking — never a missing frame.
/// The two sets `select_hw_clip_ids` needs, in the one shape both
/// [`prewarm_boundary_stack`] callers can name on EVERY build: the clip ids a
/// hardware session is already serving, and the per-media hardware-failure set.
/// Both are `std` types, so the signature above needs no `cfg` — only the
/// selection itself does, because `layer_sessions` is Windows + `hwdecode` only.
type HwPredictInputs<'a> = (
    &'a std::collections::HashSet<String>,
    &'a std::collections::HashSet<std::path::PathBuf>,
);

/// Which of `stack`'s layers hardware is predicted to claim at the boundary
/// tick — the REAL selector, given the REAL inputs that tick's `sync_to_stack`
/// will use (59-16), behind the SAME capability gates that tick's real open
/// path applies (59-20, closing § D-43).
///
/// **Why the capability gates are read here.** [`engine::open_hw_decoder`]
/// fails closed with `Disabled` the moment [`engine::KILL_SWITCH_ENV`] is set,
/// and both [`crate::layer_sessions::LayerSessionSet::sync_to_stack`] and
/// `prewarm_for_boundary` skip every attempt while the session-wide
/// [`HW_LATCH`] is engaged. **Neither outcome is ever recorded into
/// `hw_failed`** — [`DecodePath::Software`]'s `cache_media: false` arm says so
/// in as many words ("the cheap, re-testable outcomes") — so the two sets this
/// function is handed structurally CANNOT carry them, and a prediction built
/// from those sets alone cannot see which arm it is on. Measured consequence
/// before this gate existed: on the kill-switched all-software arm the cache
/// exit read **759.99 ms against 44.09 ms** (a 17x regression) with warm
/// `sw_sidecar_spawns` up **+3 per run** — exactly
/// [`crate::layer_sessions::MAX_HW_SESSIONS`] layers named by the predictor,
/// dropped from the warm-up, and then built cold at the boundary.
///
/// **Why NOT the evidence heuristic.** § D-43's owner line proposed declining
/// to predict whenever the producer has no live sessions AND no successful open
/// yet. Across a fully cached span the producer never runs `sync_to_stack` at
/// all, so at the exit-discovery tick the HEALTHY hardware arm presents the
/// very same empty evidence: the heuristic cannot tell the arms apart, and
/// declining there would warm the three layers hardware is about to claim —
/// restoring the +3-spawns-per-run cost 59-16 removed (135 -> 120). The
/// capability gates DO tell them apart, for one `var_os` plus one relaxed
/// atomic load per boundary discovery — the same per-decision cost class as
/// [`crate::boundary_prewarm_disabled`]'s own env read, never per frame.
///
/// A stale answer remains outside the correctness class on BOTH arms: the
/// mispredicted layer simply takes the pre-59-13 cold spawn at the boundary,
/// which 59-15 made non-blocking — never a missing frame (T-59-16-03's
/// argument, now made for the software arm too).
#[cfg(all(windows, feature = "hwdecode"))]
fn predicted_hw_claim(
    stack: &MultiLayerStack,
    hw_predict: Option<HwPredictInputs<'_>>,
) -> Vec<String> {
    // The two gates the real open path consults — the engine CONST, not a
    // re-typed literal, because the coupling to the real gate is the point.
    let arm_can_claim = std::env::var_os(engine::KILL_SWITCH_ENV).is_none() && !HW_LATCH.engaged();
    predicted_hw_claim_inner(stack, hw_predict, arm_can_claim)
}

/// [`predicted_hw_claim`]'s decision, made pure: no env read, no process-global
/// static. Both are lifted into `arm_can_claim` by the wrapper so this table is
/// unit-testable on BOTH arms without engaging the process-wide [`HW_LATCH`],
/// whose engagement is permanent for the process lifetime by design and would
/// poison every later test in the binary.
///
/// | `arm_can_claim` | `hw_predict` | answer |
/// |---|---|---|
/// | `false` | anything | EMPTY — hardware cannot claim on this run |
/// | `true`  | `None`   | EMPTY — the caller declined (no session set) |
/// | `true`  | `Some`   | the real selector's own answer |
#[cfg(all(windows, feature = "hwdecode"))]
fn predicted_hw_claim_inner(
    stack: &MultiLayerStack,
    hw_predict: Option<HwPredictInputs<'_>>,
    arm_can_claim: bool,
) -> Vec<String> {
    if !arm_can_claim {
        // 59-20 / § D-43. Naming a layer here would only strip it from the
        // warm-up on an arm where nothing will ever come to claim it.
        return Vec::new();
    }
    match hw_predict {
        Some((live_ids, hw_failed)) => crate::layer_sessions::select_hw_clip_ids(
            stack,
            hw_failed,
            live_ids,
            crate::layer_sessions::MAX_HW_SESSIONS,
        ),
        // The caller declined to predict (no session set: no GPU budget, so
        // every layer is on the software pool anyway).
        None => Vec::new(),
    }
}

/// CPU-only port surface: `layer_sessions` does not exist there and no layer
/// can be hardware-claimed, so nothing is ever filtered out. Both callers pass
/// `None` on this build; the parameter is kept so the one call site above needs
/// no `cfg` of its own.
#[cfg(not(all(windows, feature = "hwdecode")))]
fn predicted_hw_claim(
    _stack: &MultiLayerStack,
    _hw_predict: Option<HwPredictInputs<'_>>,
) -> Vec<String> {
    Vec::new()
}

fn prewarm_boundary_stack(
    host: &dyn PreviewHost,
    boundary_pos: i64,
    la: &mut Lookahead,
    live_pool: Option<&engine::LayerDecoderPool>,
    hw_predict: Option<HwPredictInputs<'_>>,
) {
    // Phase 57 (plan 57-07): the one-variable lever for the PLAY-03 warm-vs-cold
    // entry differential. Inert unless `RUDIS_DISABLE_BOUNDARY_PREWARM` is set;
    // see `crate::DISABLE_PREWARM_ENV` for why the switch is an env var.
    if crate::boundary_prewarm_disabled() {
        return;
    }
    let Some(stack) = crate::resolve_multilayer(host, boundary_pos) else {
        return; // single clip / gap after the delegation — existing paths own it
    };
    let step = engine::frame_step_us(stack.fps).max(1);
    let bucket = boundary_pos - boundary_pos.rem_euclid(step);
    match la.prewarmed_for {
        Some(b) if b == bucket => return, // already warmed toward THIS boundary
        Some(_) => {
            // Stale pre-delegation latch: drop it (and any husk — children
            // reaped by Drop) so the REAL upcoming boundary gets its warm-up.
            la.prewarmed_for = None;
            la.pending_pool = None;
            la.pending_session = None;
        }
        None => {}
    }
    let mut sources = crate::pool_sources(&stack);
    // 59-16: the PREDICTIVE hardware filter — the same shape the multi arm's
    // live tick applies reactively (`hw_served` from `sync_to_stack`, then
    // `sources.retain(..)`), made one lookahead horizon early with the REAL
    // selector and the SAME two inputs that tick will use. Without it, a
    // six-layer stack whose top three go to hardware spawns three CLI children
    // here that the boundary tick's `adopt_missing` then leaves un-adopted:
    // 59-14 read them as `sw_sidecar_spawns` 120 -> 135, +3 per warm run.
    let claimed = predicted_hw_claim(&stack, hw_predict);
    if !claimed.is_empty() {
        sources.retain(|s| !claimed.iter().any(|id| id == &s.clip_id));
    }
    if sources.is_empty() {
        // All-text/still stack — or, since 59-16, a boundary whose every pooled
        // layer hardware is predicted to claim. Both warm nothing, and both
        // deliberately leave `prewarmed_for` unlatched: declining to warm is not
        // the same event as having warmed.
        return;
    }
    la.prewarmed_for = Some(bucket);
    let pending = la.pending_pool.get_or_insert_with(|| {
        let mut p = engine::LayerDecoderPool::new(stack.fps, POOL_PULL_TIMEOUT);
        // Live path (59-15 / § D-2): a stream end HOLDs for at most one source
        // step and re-roots off the pull, instead of cold-decoding on the
        // producer thread. Set at EVERY ring-side construction — `adopt_missing`
        // moves SESSIONS between pools and the RECEIVING pool's flag governs,
        // so a pending pool that never calls `advance()` still must not be able
        // to hand a session to a pool with the other policy.
        p.set_ended_nonblocking(true);
        p
    });
    // 59-16: reuse the LIVE pool's probed dims, the `lookahead_probe`
    // precedent verbatim (see its own comment: "prewarming media that is
    // already playing spawns ZERO ffprobe on the producer thread"). Insert-if-
    // absent, so nothing here can overwrite a dim this pool probed itself.
    if let Some(p) = live_pool {
        pending.seed_dims_from(p);
    }
    pending.prewarm(&sources);
}

impl Lookahead {
    fn new() -> Self {
        Lookahead {
            prewarmed_for: None,
            pending_pool: None,
            pending_session: None,
        }
    }
}

/// The lookahead prewarm probe (18.3-05, design §8 Stage 3 items 1-2). Runs
/// ONLY while the producer is parked on a FULL ring — idle time by definition
/// (T-18.3-05-02) — at most once per park episode, Program mode only. Probes
/// what `prod_pos + LOOKAHEAD_US` resolves to; when the upcoming layer set
/// DIFFERS from the current one (merge / unmerge / clip cut / gap exit),
/// pre-warms the INCOMING decoders toward that FIXED boundary so their
/// children fill their channels before the boundary frame is due —
/// start-early, never catch-up (the anticipatory pre-roll every surveyed NLE
/// uses; nle-boundary-playback-techniques.md Q4).
///
/// Phase 46 (plans 46-06 and 46-08): takes the [`PreviewHost`] port
/// ONLY. 46-06 moved `resolve_active` behind it while `resolve_multilayer` still
/// needed the handle, so this carried both for one wave; 46-08 moved
/// `resolve_multilayer` too, which was the last user of `app` here — so the
/// parameter and the runtime generic went away entirely rather than being
/// carried dead (the same collapse `start_multi_audio` made at 46-06). Threading
/// the port down from `producer_loop` rather than rebuilding an adapter here
/// keeps the producer thread on exactly ONE `TauriPreviewHost` for its lifetime.
fn lookahead_probe(
    host: &dyn PreviewHost,
    prod_pos: i64,
    cur_stack: Option<&MultiLayerStack>,
    cur_clip: &Option<String>,
    pool: &mut Option<engine::LayerDecoderPool>,
    la: &mut Lookahead,
) {
    // Latch maintenance: once prod_pos passes the latched boundary the latch
    // clears; a prewarm its boundary never consumed is stale — drop it
    // (children reaped by Drop). Consumption happens BEFORE the park within an
    // iteration (multi arm takes pending_pool / single arm takes
    // pending_session before it pushes), so a live prewarm is never dropped
    // here. While still approaching the latched boundary, skip: ONE prewarm
    // per boundary (T-18.3-05-01).
    if let Some(b) = la.prewarmed_for {
        // Debug session `multitrack-preview-lag-choppy` (2026-08-01): clear
        // with a GRACE window past the latched boundary, never at it. The
        // latched `b` is a bucketed ESTIMATE that can sit up to one frame
        // step BELOW the true boundary, and the multi arm consumes the
        // pending pool via adopt at the boundary production tick — clearing
        // at `prod_pos >= b` could drop the warm children one tick BEFORE
        // that adopt (the exact cold-boundary failure this session fixes).
        // 100ms ≈ 2-3 frames at any project fps: the husk (already-adopted
        // sessions removed; only mispredictions remain) outlives the boundary
        // by a bounded beat, then its children are reaped by Drop (SC-R3).
        const LATCH_CLEAR_GRACE_US: i64 = 100_000;
        if prod_pos >= b + LATCH_CLEAR_GRACE_US {
            la.prewarmed_for = None;
            la.pending_pool = None;
            la.pending_session = None;
        } else {
            return;
        }
    }
    let future = prod_pos + LOOKAHEAD_US;
    // Debug session `multitrack-preview-lag-choppy` (2026-08-01): inside a
    // multi range this probe now runs PER PRODUCED FRAME (the park-only
    // trigger never fires there — multi production hovers near RTF 1 so the
    // ring rarely fills), so it needs a cheap no-boundary fast path. Comparing
    // the ACTIVE-HIT id sets at prod_pos vs future under one store lock is
    // borrow-only (no Project clone, no LayerSpec building); hit-set equality
    // implies stack equality (the hit→layer filter depends only on
    // time-invariant clip/media properties), so an equal set means no
    // boundary inside the horizon and the expensive resolve is skipped.
    if cur_stack.is_some() {
        if let Some(guard) = host.store() {
            let tl = guard.timeline();
            let now_hits = tl.active_layers_at(prod_pos);
            let fut_hits = tl.active_layers_at(future);
            if now_hits.len() == fut_hits.len()
                && now_hits
                    .iter()
                    .zip(fut_hits.iter())
                    .all(|(a, b)| a.clip_id == b.clip_id)
            {
                return; // same layer set ahead — no boundary in the horizon
            }
        }
    }
    if let Some(stack) = crate::resolve_multilayer(host, future) {
        // Future is MULTI: a boundary iff the current position isn't multi, or
        // the layer SET differs (a layer joins/leaves mid-overlap).
        let changed = match cur_stack {
            Some(cur) => {
                let cur_ids: std::collections::HashSet<&str> =
                    cur.layers.iter().map(|l| l.clip_id.as_str()).collect();
                let fut_ids: std::collections::HashSet<&str> =
                    stack.layers.iter().map(|l| l.clip_id.as_str()).collect();
                cur_ids != fut_ids
            }
            None => true, // single/gap now → a merge is coming
        };
        if !changed {
            return;
        }
        let step = engine::frame_step_us(stack.fps).max(1);
        la.prewarmed_for = Some(future - future.rem_euclid(step));
        // Build the incoming sources with the SAME mapping
        // `compose_multilayer_from_pool` applies (`pool_sources` — text specs
        // rasterize, sequences decode inline, stills serve from the cache;
        // none are ever pooled, so none are ever prewarmed either — debug
        // session `multitrack-preview-lag-choppy` aligned this filter with the
        // compose-side one, which the old text-only filter had drifted from).
        let mut sources = crate::pool_sources(&stack);
        // Multi→multi set change: only the INCOMING layers need warming — the
        // continuing layers' live sessions keep streaming and must never be
        // disturbed (prewarming them would spawn redundant children the adopt
        // step would immediately throw away).
        if let Some(cur) = cur_stack {
            let cur_ids: std::collections::HashSet<&str> =
                cur.layers.iter().map(|l| l.clip_id.as_str()).collect();
            sources.retain(|s| !cur_ids.contains(s.clip_id.as_str()));
        }
        if sources.is_empty() {
            return; // all-text stack / leave-only boundary: nothing to warm
        }
        // Debug session `multitrack-preview-lag-choppy` (2026-08-01): ALWAYS
        // warm into the PENDING pool, never the live one. The old multi→multi
        // arm prewarmed into the LIVE pool, but `advance()` opens with
        // `sessions.retain(active_ids)` and the boundary is ~24 ticks out — so
        // the very next tick reaped the prewarmed session (child killed) and
        // every in-section cut started cold anyway (measured 393-823ms
        // production stall per cut). The pending pool rides OUTSIDE retain's
        // reach; the multi arm ADOPTS its sessions (rebased) exactly at the
        // boundary tick via `LayerDecoderPool::adopt_missing`.
        let pending = la.pending_pool.get_or_insert_with(|| {
            let mut p = engine::LayerDecoderPool::new(stack.fps, POOL_PULL_TIMEOUT);
            p.set_ended_nonblocking(true); // live path — see prewarm_boundary_stack
            p
        });
        // Reuse the live pool's probed dims so prewarming media that is
        // already playing (the common cut-to-same-media case) spawns ZERO
        // ffprobe on the producer thread.
        if let Some(p) = pool.as_ref() {
            pending.seed_dims_from(p);
        }
        pending.prewarm(&sources);
    } else if let Some(r) = crate::resolve_active(host, false, future) {
        // Future is a SINGLE clip: an ordinary cut iff its id differs from the
        // clip playing now. (A multi→single unmerge also lands here: cur_clip
        // is None inside a multi range, so a Some future id reads as a change.)
        if cur_stack.is_none() && *cur_clip == r.clip_id {
            return; // the same clip just continuing — no boundary ahead
        }
        let step = engine::frame_step_us(r.fps).max(1);
        la.prewarmed_for = Some(future - future.rem_euclid(step));
        // Supersede any previous pending session (dropped → child reaped):
        // bounded to at most ONE pending session ever. SEEK-02: prewarm goes
        // through start_with_pts too — the warm boundary path must carry the
        // PTS side channel or it silently re-creates the synthetic stamp.
        // D-05: the source ALWAYS comes through the resolver seam, never
        // `r.path` directly — Phase 58 (D-27) substitutes a playback proxy here
        // and this decode arm does not otherwise change. This single-clip CPU
        // prewarm was one of the THREE arms that still opened from `r.path`
        // after Phase 57 (research RQ3), which is why a proxy engaged only
        // during multi-layer overlaps until 58-06 wired it.
        let src = crate::decode_source::resolve_decode_source(
            r.clip_id.as_deref().unwrap_or(""),
            &r.path,
            r.source_us,
        );
        la.pending_session = match engine::StreamingDecodeSession::start_with_pts(
            &src.path,
            src.source_us,
            r.rotation,
        ) {
            Ok((session, pts_rx)) => Some(PendingSession {
                clip_id: r.clip_id,
                // Deliberately the TIMELINE-derived value (`r`'s), not the
                // resolved one: the adopt match at the cold-start site compares
                // `p.source_us - r.source_us`, so both sides must be in the
                // same clock. D-04 makes the two equal today — a proxy keeps
                // its source's fps, timebase and duration, so the seam's
                // mapping is the identity — and storing `r`'s keeps that
                // comparison like-with-like even if some future source kind
                // ever does remap.
                source_us: r.source_us,
                session,
                pts_rx,
                retime: r.retime,
            }),
            // Pre-start failed → the boundary starts cold (today's
            // behavior); the latch stops a retry storm (WR-03 spirit).
            Err(_) => None,
        };
    }
    // Future is a gap (or nothing loaded): nothing to warm.
}

/// Push an entry with the Stage-3 lookahead hook (18.3-05): [`RingCtl::try_push`]
/// first; on WouldBlock — the ring is full, so the producer has free time by
/// definition — run the prewarm probe ONCE, then fall back to the blocking
/// park exactly as before. Returns `push_blocking`'s semantics (`false` =
/// stale/stopped, entry abandoned).
#[allow(clippy::too_many_arguments)]
fn push_with_lookahead(
    ctl: &RingCtl,
    entry: RingEntry,
    host: &dyn PreviewHost,
    prod_pos: i64,
    is_source: bool,
    cur_stack: Option<&MultiLayerStack>,
    cur_clip: &Option<String>,
    pool: &mut Option<engine::LayerDecoderPool>,
    la: &mut Lookahead,
) -> bool {
    match ctl.try_push(entry) {
        Ok(pushed) => pushed,
        Err(entry) => {
            if !is_source {
                lookahead_probe(host, prod_pos, cur_stack, cur_clip, pool, la);
            }
            ctl.push_blocking(entry)
        }
    }
}

/// Check out one composite target, POLLING the control flags instead of
/// blocking on the pool's own condvar (57-04, threat T-57-08).
///
/// `CompositeTargetPool::checkout` blocks when every target is in flight, and
/// that backpressure is intended — but a blocking checkout would also make the
/// producer deaf to stop/flush for as long as the presenter takes, which is
/// Pitfall 4 in a new shape. This is not routing around the backpressure; it is
/// the backpressure with a control check in it: the SAME bound (K targets),
/// re-checking `stop`/`gen` every beat.
///
/// Returns the target (or `None` on stop/flush) and the microseconds spent
/// waiting — every one of which is the producer waiting for the PRESENTER, not
/// producing, which is why plan 57-08's hysteresis subtracts it.
///
/// **Extracted (Phase 59, plan 59-06) rather than copied.** The multi arm now
/// has TWO composite entry points — the live gather and the cached-segment
/// serve — and two hand-rolled copies of a stop/gen-polling loop is exactly the
/// shape that diverges later, in the direction where one of them stops
/// answering a flush.
fn checkout_target_polling(
    tp: &engine::CompositeTargetPool,
    ctl: &RingCtl,
    my_gen: u64,
) -> (Option<engine::PooledTarget>, u64) {
    let checkout_started = std::time::Instant::now();
    let target = loop {
        if let Some(t) = tp.try_checkout() {
            break Some(t);
        }
        if ctl.stop.load(Ordering::SeqCst) || ctl.gen.load(Ordering::SeqCst) != my_gen {
            break None;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    (target, checkout_started.elapsed().as_micros() as u64)
}

// ---------------------------------------------------------------------------
// Phase 48 (plan 48-09): the dedicated GPU decode thread + per-clip delegation.
//
// CONTEXT D-03: decode runs on its OWN thread ("rudis-gpu-decode") feeding the
// ring; the present thread only samples and composites. The producer thread
// ("rudis-preview-producer") stays the ORCHESTRATOR: it resolves content,
// runs the per-media policy gate at clip start, and — when hardware decode
// opens — parks in a join while the GPU thread owns production for that clip.
// Every bounded op on the GPU thread re-checks stop/gen, so the join is
// bounded by the same control contract that bounds the CPU producer.
// ---------------------------------------------------------------------------

/// How a delegated GPU clip ended (returned by the `"rudis-gpu-decode"`
/// thread to the producer that joined it).
///
/// Phase 49 (plan 49-06, SEEK-01): `Completed` and `Interrupted` HAND THE
/// LIVE SESSION BACK to the producer, which caches it (`warm_hw`) so the next
/// delegation for the SAME media re-seeks via `HwDecodeSession::seek_to_us`
/// instead of paying the full open+probe+device+pool reopen (measured at
/// ~71-81ms of the ~96-137ms baseline seek chain — the dominant stage).
/// `HwDecodeSession` is Send (documented unsafe impl), so it crosses the
/// JoinHandle. `Failed` deliberately carries NO session: a failed session is
/// never reused (invalidation rule 3).
#[cfg(all(windows, feature = "hwdecode"))]
enum GpuExit {
    /// The clip range completed (out point reached, or stream EOF — snap to
    /// the clip end exactly like the CPU path's `session_end_pos` snap).
    Completed {
        next_pos: i64,
        session: Option<engine::HwDecodeSession>,
    },
    /// Stop or flush observed (a push went stale, or the flags flipped): the
    /// producer's loop-top handshake owns what happens next.
    Interrupted {
        session: Option<engine::HwDecodeSession>,
    },
    /// Mid-clip decode/import failure: resume THIS media at `next_pos` on the
    /// CPU sidecar path (per-media fallback — GPU-05's "slower, still
    /// correct", never a black screen).
    Failed { next_pos: i64 },
}

/// Outcome of [`land_gpu_session`].
#[cfg(all(windows, feature = "hwdecode"))]
enum GpuLanding {
    /// The first frame at/after the demanded position.
    Landed(engine::HwFrame),
    /// Stream ended before the demanded position (degrade, don't spin).
    StreamEnded,
}

/// Land `session` on `source_us`: demuxer seek (skipped only on the fresh
/// position-0 open, where the decoder is already there) + the CPU sidecar's
/// accurate-seek twin — decode-and-discard until the first frame at/after
/// `source_us − frame_step/2`, so the FIRST pushed frame's pixels match its
/// timeline stamp (the D-46-11-07 stale-frame class must not be re-entrenched
/// here). ONE routine shared by the fresh-open path (`gpu_producer_loop`'s
/// top) and the warm-reuse path (`delegate_gpu_clip`) — two landing loops
/// would drift apart (plan 49-06's factoring requirement).
#[cfg(all(windows, feature = "hwdecode"))]
fn land_gpu_session(
    session: &mut engine::HwDecodeSession,
    source_us: i64,
    frame_step: i64,
    force_seek: bool,
) -> Result<GpuLanding, String> {
    if source_us > 0 || force_seek {
        session
            .seek_to_us(source_us)
            .map_err(|e| format!("seek to {source_us}us failed: {e}"))?;
    }
    let target = source_us - frame_step / 2;
    let mut discards = 0usize;
    loop {
        match session.decode_next() {
            Ok(Some(f)) => {
                // A missing pts is accepted as "close enough" — nothing to
                // compare against, and refusing would stall the clip.
                if f.pts_us().is_none_or(|p| p >= target) {
                    return Ok(GpuLanding::Landed(f));
                }
                discards += 1;
                if discards > GPU_SEEK_DISCARD_CAP {
                    return Err(format!(
                        "seek discard cap hit before reaching {source_us}us"
                    ));
                }
            }
            Ok(None) => return Ok(GpuLanding::StreamEnded),
            Err(e) => return Err(format!("decode failed while seeking: {e}")),
        }
    }
}

/// Everything the GPU decode thread needs to produce ONE clip range —
/// captured by the producer at delegation time so the thread never touches
/// the store.
#[cfg(all(windows, feature = "hwdecode"))]
struct GpuClipParams {
    /// Timeline position of the clip range's first frame (the producer's
    /// `prod_pos` at delegation).
    start_pos: i64,
    /// Source-media position the range decodes from.
    source_us: i64,
    /// Timeline position at which the clip is exhausted (the CPU path's
    /// `session_end_pos` formula, same snap discipline).
    end_pos: i64,
    /// Per-frame timeline step from the media's fps.
    frame_step: i64,
    /// The flush generation every pushed entry is stamped with.
    gen: u64,
    /// The clip's time remap (260730-x2t) — the hardware half of SR-2. Both
    /// decode paths stamp through the same helper, so both must carry it.
    retime: Option<rudis_core::Retime>,
}

/// Spawn the dedicated GPU decode thread for one delegated clip (CONTEXT
/// D-03). Loop shape: `decode_next` → `import_gpu_frame` (the audited 48-05
/// zero-copy chain, onto the compositor's own device) → push a
/// [`RingPayload::Gpu`] entry under the SAME gen/flush/stop contract as the
/// CPU producer, budget-checked every iteration.
#[cfg(all(windows, feature = "hwdecode"))]
fn spawn_gpu_producer(
    ctl: Arc<RingCtl>,
    compositor: Arc<engine::Compositor>,
    session: engine::HwDecodeSession,
    budget: Arc<AtomicU64>,
    ledger: engine::VramLedger,
    clip: GpuClipParams,
    prelanded: Option<engine::HwFrame>,
) -> std::thread::JoinHandle<GpuExit> {
    std::thread::Builder::new()
        .name("rudis-gpu-decode".to_string())
        .spawn(move || gpu_producer_loop(ctl, compositor, session, budget, ledger, clip, prelanded))
        .expect("spawn rudis-gpu-decode thread")
}

/// Upper bound on seek-discard decodes (a keyframe-sparse stream can force
/// decoding from far behind the target; 1800 frames ≈ 60 s @30fps — beyond
/// that, treat the seek as failed rather than spin).
#[cfg(all(windows, feature = "hwdecode"))]
const GPU_SEEK_DISCARD_CAP: usize = 1800;

/// Phase 48 plan 48-10 (GPU-04 detection, the D3D11 side): when an av error
/// hits the GPU decode thread, poll the session's device for
/// `GetDeviceRemovedReason` to CLASSIFY it — an ordinary media/codec error
/// fails just this clip closed (GPU-05, unchanged), but a whole-adapter reset
/// means the wgpu device is gone too. Classification here is LOG-ONLY and the
/// clip still fails closed identically: recovery is owned by the shell's ONE
/// entry (the wgpu device-lost callback registered in `native_surface.rs`) —
/// never a second competing recovery from this thread (threat T-48-10-01).
#[cfg(all(windows, feature = "hwdecode"))]
fn classify_gpu_thread_failure(session: &engine::HwDecodeSession, what: &str) {
    if let Some(hr) = session.device_removed_reason() {
        eprintln!(
            "device_lost: decode thread classified '{what}' as whole-adapter device loss \
             (GetDeviceRemovedReason={hr}); failing this clip closed — recovery is driven by \
             the shell's single entry (wgpu device-lost callback)"
        );
    }
}

/// The body of the `"rudis-gpu-decode"` thread: one clip range, GPU-resident
/// end to end.
///
/// Depth ordering (Pitfall 4 — ORDER MATTERS, and it happened at the caller):
/// the session was OPENED with `ring_target` = the provisional VRAM-derived
/// depth, so the hw frame pool is already widened to `target + headroom +
/// threads` (48-05); here the FINAL depth is re-derived against the REAL
/// widened `session.pool_size()` and the frame's REAL dims before the first
/// push, and [`RingCtl::enter_gpu_depth`] arms the count-capacity the pushes
/// park against.
///
/// Budget reaction (CONTEXT D-12): the `AtomicU64` is re-read every
/// iteration (cheap Acquire load); a changed budget re-derives ONLY the VRAM
/// half (the pool half is fixed for the session's life). A SHRINK updates the
/// cap (`set_gpu_depth`), evicts the oldest entries down to the new depth and
/// KEEPS PLAYING degraded — this branch contains NO software-fallback call of
/// any kind; capability failures are the ONLY fallback trigger, and they are
/// decided at decoder-open, never here.
///
/// VRAM ledger (Phase 57, plan 57-03 — 57-RESEARCH.md Pitfall 2): the depth is
/// derived against the budget MINUS whatever sibling sessions already hold,
/// and this session then reserves its own pool's cost for as long as its
/// session lives. The reservation is taken AFTER the depth is computed — a
/// session that counted its own bytes against itself would shrink its own ring
/// — and it is an RAII guard, so an early return or a panic on this thread
/// releases it (threat T-57-06).
#[cfg(all(windows, feature = "hwdecode"))]
fn gpu_producer_loop(
    ctl: Arc<RingCtl>,
    compositor: Arc<engine::Compositor>,
    mut session: engine::HwDecodeSession,
    budget: Arc<AtomicU64>,
    ledger: engine::VramLedger,
    clip: GpuClipParams,
    prelanded: Option<engine::HwFrame>,
) -> GpuExit {
    // --- land on the demanded source position via the ONE shared landing
    // routine (`land_gpu_session`). The warm-reuse path (plan 49-06) already
    // landed on the PRODUCER thread — so a warm seek/decode error could fall
    // through to the cold open instead of failing the clip — and hands the
    // first frame in as `prelanded`; the fresh path lands here.
    let first = match prelanded {
        Some(f) => f,
        None => match land_gpu_session(&mut session, clip.source_us, clip.frame_step, false) {
            Ok(GpuLanding::Landed(f)) => f,
            Ok(GpuLanding::StreamEnded) => {
                // Stream ended before the demanded position: nothing to show
                // for this range — snap to the clip end (degrade, don't spin).
                return GpuExit::Completed {
                    next_pos: clip.end_pos,
                    session: Some(session),
                };
            }
            Err(e) => {
                eprintln!("hwdecode: {e}");
                classify_gpu_thread_failure(&session, "landing seek/decode");
                return GpuExit::Failed {
                    next_pos: clip.start_pos,
                };
            }
        },
    };

    // --- FINAL depth from the two ceilings (GPU-03): the runtime-queried
    // budget vs the REAL widened pool. Real dims from the first frame; the
    // provisional estimate's only job (sizing the pool at open) is done.
    let (fw, fh) = (first.width().max(1) as u32, first.height().max(1) as u32);
    let frame_bytes = engine::frame_bytes_nv12(fw, fh);
    let mut last_budget = budget.load(Ordering::Acquire);
    // Pitfall 2: siblings FIRST. `reserved()` is what everyone ELSE holds —
    // this session has not reserved yet, so it cannot be counting itself.
    let siblings = ledger.reserved();
    let mut depth = engine::gpu_ring_depth(
        last_budget,
        frame_bytes,
        session.pool_size(),
        engine::DECODER_HEADROOM,
        siblings,
    );
    if depth == 0 {
        // A degenerate pool cannot host a ring at all — treat like an open
        // failure (a capability problem at session start, NOT a VRAM event).
        eprintln!("hwdecode: derived ring depth 0 (pool {})", session.pool_size());
        return GpuExit::Failed {
            next_pos: clip.start_pos,
        };
    }
    // …and only now take this session's own share. The reserved amount is the
    // WHOLE pool's charge, not `depth × frame_bytes`: the VRAM is spent at
    // decoder-open on the widened pool and a depth change never returns bytes
    // to the OS (see `session_pool_vram_bytes`). Held for the rest of this
    // function — every exit path below, and any panic, releases it.
    let reservation =
        ledger.reserve(engine::session_pool_vram_bytes(session.pool_size(), frame_bytes));
    ctl.enter_gpu_depth(depth);
    // GPU-03's "logged at startup": the session-open depth derivation, every
    // input named (Phase 57 adds the sibling term and this session's own
    // reservation, so an N-session log reads as a ledger trace).
    eprintln!(
        "vram_budget: budget={last_budget}B fraction={} pool={} headroom={} \
         reserved_by_others={siblings}B reserved_self={}B -> ring_depth={depth}",
        engine::VRAM_BUDGET_FRACTION,
        session.pool_size(),
        engine::DECODER_HEADROOM,
        reservation.bytes(),
    );

    let mut pos = clip.start_pos;
    // SEEK-02 (Phase 49): the last stamp pushed for this clip range — the
    // monotonicity-guard input for `stamp_or_fallback` (a hostile backward
    // PTS jump must never violate the ring's strictly-increasing invariant).
    let mut last_stamp: Option<i64> = None;
    let mut pending = Some(first);
    loop {
        // Control checks bracket every bounded op — the CPU producer's
        // discipline, mirrored. Both interrupts HAND THE SESSION BACK (the
        // 49-06 warm cache): a flush is exactly the case whose next
        // delegation re-seeks the same media.
        if ctl.stop.load(Ordering::SeqCst) {
            return GpuExit::Interrupted {
                session: Some(session),
            };
        }
        if ctl.gen.load(Ordering::SeqCst) != clip.gen {
            // flushed — the orchestrator rebuilds
            return GpuExit::Interrupted {
                session: Some(session),
            };
        }

        // Live budget reaction (D-12): re-derive the VRAM half; on a SHRINK,
        // cap + evict oldest + KEEP PLAYING. No fallback of any kind here.
        let b = budget.load(Ordering::Acquire);
        if b != last_budget {
            last_budget = b;
            // `others()`, NOT `ledger.reserved()`: this session already holds
            // a reservation, and counting its own bytes against itself would
            // shrink its ring on every budget wake for no reason.
            let new_depth = engine::gpu_ring_depth(
                b,
                frame_bytes,
                session.pool_size(),
                engine::DECODER_HEADROOM,
                reservation.others(),
            );
            if new_depth != depth {
                ctl.set_gpu_depth(new_depth);
                if new_depth < depth {
                    let evicted = ctl.evict_oldest_to(new_depth);
                    eprintln!(
                        "vram_budget: budget shrank to {b}B -> ring_depth={new_depth} \
                         (evicted {evicted} oldest entries; playback continues degraded)"
                    );
                }
                depth = new_depth;
            }
        }

        if pos >= clip.end_pos {
            return GpuExit::Completed {
                next_pos: clip.end_pos,
                session: Some(session),
            };
        }

        let hw = match pending.take() {
            Some(f) => f,
            None => match session.decode_next() {
                Ok(Some(f)) => f,
                Ok(None) => {
                    // EOF before the out point (fps rounding vs the trim):
                    // snap to the clip end, exactly like the CPU path's
                    // near-end miss handling.
                    return GpuExit::Completed {
                        next_pos: clip.end_pos,
                        session: Some(session),
                    };
                }
                Err(e) => {
                    eprintln!("hwdecode: mid-clip decode failed: {e}");
                    classify_gpu_thread_failure(&session, "mid-clip decode");
                    return GpuExit::Failed { next_pos: pos };
                }
            },
        };
        // The audited zero-copy chain (48-05), onto the compositor's own
        // device — the frame never touches CPU memory.
        let gpu = match compositor.import_gpu_frame(&session, hw) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("hwdecode: zero-copy import failed: {e}");
                classify_gpu_thread_failure(&session, "zero-copy import");
                return GpuExit::Failed { next_pos: pos };
            }
        };
        // SEEK-02 (Phase 49): stamp from the frame's REAL source PTS mapped
        // onto the timeline through the ONE shared helper both decode paths
        // use (`gpu.pts_us` is absolute source-media µs — av_rescale_q in
        // HwFrame::pts_us — the same value the seek-landing discard loop
        // above already compares). `pos` STILL advances by frame_step per
        // pushed frame: it remains the production clock / end_pos cursor and
        // the synthetic-fallback grid — only the STAMP source changes.
        let mapped = gpu
            .pts_us
            .map(|p| {
                crate::pts_map::map_source_pts_to_timeline(
                    p,
                    clip.source_us,
                    clip.start_pos,
                    clip.retime.as_ref(),
                )
            });
        let stamped = crate::pts_map::stamp_or_fallback(mapped, pos, last_stamp, "hw");
        last_stamp = Some(stamped);
        if !ctl.push_blocking(RingEntry {
            payload: RingPayload::Gpu(gpu),
            timeline_us: stamped,
            gen: clip.gen,
        }) {
            // stale/stopped — abandon the entry, hand the session back
            return GpuExit::Interrupted {
                session: Some(session),
            };
        }
        pos += clip.frame_step;
    }
}

/// The session-wide hardware-failure latch (GPU-05 / CONTEXT D-15): after
/// [`engine::LATCH_THRESHOLD`] counted `InitFailed` results across DIFFERENT
/// media, hardware attempts are skipped for the rest of the session. A
/// process-wide static because "session" means the app's lifetime; the
/// per-media de-duplication lives in the producer's `hw_failed` set (each
/// media records at most once per producer lifetime).
///
/// **The instance now lives in `engine`** ([`engine::PROCESS_HW_LATCH`]) and is
/// ADOPTED here under its historical name — same single process-wide latch,
/// every reader below unchanged. It moved because the one mechanism that arms
/// it from outside a decoder open (the live preview device's uncaptured-error
/// handler, `engine::Compositor`'s `install_uncaptured_error_guard`) has to be
/// able to reach it from the crate that CREATES the device. While the instance
/// lived here, that coupling needed a `pub fn` in this crate called from
/// whichever crate owned the device — `src-tauri/src/native_surface.rs` — and
/// when GATE-07 deleted that file the containment path silently had no caller
/// for four phases (debug session `gpu-oom-guard-lost-in-the-cutover`).
/// `preview::force_hw_latch_engage` was the orphaned seam and is now
/// [`engine::force_hw_latch_engage`], called by the guard itself.
#[cfg(all(windows, feature = "hwdecode"))]
use engine::PROCESS_HW_LATCH as HW_LATCH;

/// What the per-clip policy gate decided (GPU-05).
#[cfg(all(windows, feature = "hwdecode"))]
enum DecodePath {
    /// Hardware decode opened — delegate the clip to the GPU decode thread.
    Hardware(engine::HwDecodeSession),
    /// Use the EXISTING CPU sidecar producer for this clip, unchanged.
    /// `cache_media` = true when a REAL open ran and failed (never re-attempt
    /// this media this producer lifetime); false for the cheap, re-testable
    /// outcomes (kill-switch env check, engaged latch).
    Software { cache_media: bool },
}

/// The per-media decode-path policy gate (GPU-05, CONTEXT D-15) — ONE
/// function, unit-testable: skip hardware entirely while the session latch is
/// engaged; otherwise try [`engine::open_hw_decoder`], and on failure log the
/// typed reason (`Disabled` / `InitFailed` / `UnsupportedSwFormat` — P010 =
/// ordinary 10-bit phone/camera content, not exotic / `UnsupportedColorspace`)
/// and route THAT media to the CPU sidecar path. GPU-05's core promise: a
/// vendor- or codec-specific failure degrades to "slower, still correct" —
/// never to broken.
#[cfg(all(windows, feature = "hwdecode"))]
fn choose_decode_path(
    path: &std::path::Path,
    ring_target: usize,
    latch: &engine::HwFailureLatch,
) -> DecodePath {
    if latch.engaged() {
        // Session-wide latch: repeated hardware trouble already proved this
        // session should stop asking. Cheap skip, logged once at engage time.
        return DecodePath::Software { cache_media: false };
    }
    match engine::open_hw_decoder(path, ring_target) {
        Ok(session) => DecodePath::Hardware(session),
        Err(e) => {
            eprintln!(
                "hwdecode: falling back to software for {}: {e}",
                path.display()
            );
            // Only InitFailed counts (capability routing is not hardware
            // trouble); the caller guarantees at-most-once per media.
            if latch.record(&e) && latch.failures() >= engine::LATCH_THRESHOLD {
                eprintln!(
                    "hwdecode: {} hardware-init failures across different media — session \
                     latch ENGAGED; skipping hardware-decode attempts for the rest of this \
                     session (software path serves preview)",
                    latch.failures()
                );
            }
            DecodePath::Software {
                // The kill-switch is a cheap env check — leave it re-testable
                // so clearing the var takes effect on the next clip start.
                cache_media: !matches!(e, engine::HwOpenError::Disabled),
            }
        }
    }
}

/// PROOF SEAM (debug session `gpu-oom-guard-lost-in-the-cutover`): ask the REAL
/// per-media policy gate [`choose_decode_path`], against the REAL process-wide
/// [`HW_LATCH`], and report only WHICH ARM it chose.
///
/// This exists so an out-of-crate test can assert *"the fallback is taken"*
/// about the shipped decision rather than about a restatement of it. It calls
/// the production function, with the production static, on a real media file:
/// before the guard fires it answers `true` (hardware genuinely available on
/// this machine, so the post-condition is not vacuous), and after a real
/// uncaptured `OutOfMemory` has engaged the latch it answers `false` — from the
/// gate's own `latch.engaged()` short-circuit, with no driver work at all.
///
/// Any session it opens is dropped immediately; nothing here is on a hot path.
/// The lesson this seam encodes: the 48-gpu-oom-4k containment was lost because
/// the ONLY thing that could have caught it — a test watching the gate change
/// its answer — did not exist.
#[cfg(all(windows, feature = "hwdecode"))]
#[doc(hidden)]
pub fn hw_decode_chosen_for(path: &std::path::Path, ring_target: usize) -> bool {
    matches!(
        choose_decode_path(path, ring_target, &HW_LATCH),
        DecodePath::Hardware(_)
    )
}

/// What the producer does next after (attempting) a GPU delegation.
#[cfg(all(windows, feature = "hwdecode"))]
enum GpuDelegation {
    /// The delegation ran; resume orchestration at `next_pos` (clip complete,
    /// or a mid-clip failure whose media is now routed to the CPU path).
    Resume { next_pos: i64 },
    /// Stop/flush ended the delegation; the loop-top handshake owns recovery.
    Interrupted,
    /// Hardware decode was not attempted or did not open — use the EXISTING
    /// CPU sidecar start below, unchanged (GPU-05's per-media fallback).
    Fallback,
}

/// The per-clip GPU delegation (runs on the producer thread). Opens hardware
/// decode for the clip's media via the policy gate, hands the open session to
/// the dedicated decode thread, and parks in a bounded join until the clip
/// range ends. ALWAYS restores the CPU byte budget on the way out, whatever
/// the exit reason, so the next content produced is budgeted exactly as
/// before this plan.
///
/// Phase 58 (D-27): this arm resolves its decode source through the D-05 seam
/// ONCE, at the top, and keys `hw_failed` / `warm_hw` on that resolved path at
/// every touchpoint — see the comment there for why the ordering is load-
/// bearing. **Known and deliberate cross-arm asymmetry:** the multi-layer
/// coordinator shares this very `hw_failed` set (`layer_sessions.rs` —
/// `spec_is_hw_eligible` and its inserts) but keys it on the ORIGINAL
/// `spec.path` while opening on the resolved one. That is a Phase-57 shape
/// outside this plan's scope and it is bounded: the worst case is ONE extra
/// failed hardware open per media per arm, because each arm latches its own key
/// on its first failure. It is not the unbounded per-reopen retry that keying
/// this function's short-circuit and its latch writes differently would produce.
#[cfg(all(windows, feature = "hwdecode"))]
#[allow(clippy::too_many_arguments)]
fn delegate_gpu_clip(
    ctl: &Arc<RingCtl>,
    compositor: &Arc<engine::Compositor>,
    budget: &Arc<AtomicU64>,
    ledger: &engine::VramLedger,
    r: &crate::Resolved,
    prod_pos: i64,
    my_gen: u64,
    hw_failed: &mut std::collections::HashSet<std::path::PathBuf>,
    warm_hw: &mut Option<(std::path::PathBuf, engine::HwDecodeSession)>,
    delegation_cap: Option<i64>,
    host: &dyn PreviewHost,
    la: &mut Lookahead,
    mut sessions: Option<&mut crate::layer_sessions::LayerSessionSet>,
    is_source: bool,
) -> GpuDelegation {
    // D-05 / Phase 58 D-27 + T-58-06-03: resolve ONCE for the whole delegation,
    // and do it HERE — above the `hw_failed` short-circuit, above the warm-reuse
    // compare, above the cold open. Every latch, every cache write and the open
    // itself key THIS resolved path and never `r.path`, so "hardware failed for
    // the media I actually attempted" and "already routed to software, don't
    // re-attempt" are statements about the same file. Resolving lower down would
    // latch a proxy under its resolved path while the short-circuit still tested
    // the original — so the early exit would never fire and every reopen would
    // re-attempt (and re-fail) a hardware open for that clip, forever.
    // Session-open granularity: this function runs on a clip start, never per
    // frame (the caller gates on `session.is_none() || cur_clip != r.clip_id`).
    let src = crate::decode_source::resolve_decode_source(
        r.clip_id.as_deref().unwrap_or(""),
        &r.path,
        r.source_us,
    );
    if hw_failed.contains(&src.path) {
        return GpuDelegation::Fallback; // already routed to software — no re-attempt
    }

    let frame_step = engine::frame_step_us(r.fps).max(1);
    // Debug session `multitrack-preview-lag-choppy` (2026-08-01): the
    // delegated range ends at the clip's remaining extent OR at the first
    // position another video-track clip joins (`next_video_layer_join`,
    // computed by the caller) — WHICHEVER COMES FIRST. Without the clamp the
    // producer is parked in the join while an overlap begins mid-clip, and
    // the multi-layer composite arm never runs ("only the bottom layer
    // shows"). Every Completed exit already resumes orchestration at
    // `end_pos`, so the clamp lands the producer exactly on the boundary,
    // where `resolve_multilayer` goes multi. The cap is strictly greater
    // than `prod_pos` by the scan (`start_us > after`), so progress is
    // guaranteed; the `max` is a defensive floor only.
    let end_pos = prod_pos + clip_remaining_timeline_us(r).max(frame_step);
    let end_pos = match delegation_cap {
        Some(cap) => end_pos.min(cap.max(prod_pos + 1)),
        None => end_pos,
    };

    // ---- Phase 49 (plan 49-06, SEEK-01): the warm-session reuse lever. ----
    // The cache holds at most ONE session and every miss/error path below
    // DROPS it and falls through to the cold open — never a retry loop
    // (threat T-49-06-01). The policy gate is NOT bypassed: hw_failed already
    // short-circuited above, and the latch / kill-switch checks below
    // short-circuit reuse exactly as they short-circuit an open.
    let mut warm: Option<(engine::HwDecodeSession, engine::HwFrame)> = None;
    if let Some((cached_path, mut s)) = warm_hw.take() {
        if HW_LATCH.engaged() {
            // INVALIDATION: process latch engaged → drop (session-wide policy
            // said stop using hardware; reuse must not outlive it).
        } else if std::env::var_os(engine::KILL_SWITCH_ENV).is_some() {
            // INVALIDATION: runtime kill-switch set → drop (reuse must not
            // outlive the SPIKE-06 re-isolation escape hatch).
        } else if cached_path != src.path {
            // INVALIDATION: different media path → drop (a session decodes
            // exactly one file; a cross-media reuse would show wrong pixels).
            // Compared against the RESOLVED path (Phase 58 D-27): the cache
            // stores what the session actually decodes, so a warm session may
            // only be re-seeked while the CURRENT resolve still answers that
            // same media. When a proxy newly appears for a clip already playing
            // from its original, this compare misses, the warm session drops,
            // and the cold open below opens the proxy — which is exactly D-09's
            // "the timeline starts using it on its next resolve".
        } else if s.device_removed_reason().is_some() {
            // INVALIDATION: device lost while cached → drop (D-48-10-SWAPORDER's
            // lesson — dead-device handles must never be re-seeked; the
            // respawn path then cold-opens against the recovered device;
            // threat T-49-06-02).
            eprintln!("hwdecode: warm session dropped — device removed while cached");
        } else {
            // Warm reuse: re-seek the LIVE session (seek_to_us + the shared
            // landing routine) instead of a full reopen. Landing runs HERE on
            // the producer thread so any seek/decode error DROPS the warm
            // session and falls through to the cold open below — a warm-path
            // miss never fails the clip and never feeds the latch.
            // The landing target stays on `r`'s clock (the producer's timeline
            // math is in `r`'s terms throughout); D-04 pins the seam's mapping
            // to the identity for proxies, so `src.source_us == r.source_us`.
            match land_gpu_session(&mut s, r.source_us, frame_step, true) {
                Ok(GpuLanding::Landed(f)) => warm = Some((s, f)),
                Ok(GpuLanding::StreamEnded) => {
                    // Source beyond EOF: the fresh path's landing snaps to the
                    // clip end the same way; the session stays warm and cached.
                    *warm_hw = Some((cached_path, s));
                    return GpuDelegation::Resume { next_pos: end_pos };
                }
                Err(e) => {
                    eprintln!(
                        "hwdecode: warm re-seek failed ({e}) — dropping cached session, \
                         cold-opening"
                    );
                }
            }
        }
    }

    let (session, prelanded) = match warm {
        Some((s, f)) => (s, Some(f)),
        None => {
            // The per-frame VRAM estimate BOTH budget questions below are asked
            // in terms of. Phase 58 (D-27): deliberately the ORIGINAL media's
            // stored dims, even when `src` is a smaller proxy — the seam answers
            // geometry but this estimate only sizes a VRAM reservation, and
            // over-reserving is the safe direction (a short pool starves the
            // decode thread).
            let est_bytes = engine::frame_bytes_nv12(r.width.max(1), r.height.max(1));

            // ONE reading of the two moving inputs, shared by the admission
            // question and the depth question — the same discipline (and the
            // same reason) as `layer_sessions.rs`'s open path: the budget moves
            // on a DXGI notification and the ledger on any sibling open/close,
            // and answering one tick's decision from two snapshots is how a
            // session gets admitted against one budget and sized against
            // another.
            let budget_now = budget.load(Ordering::Acquire);
            let reserved_by_others = ledger.reserved();

            // ---- D-10's ordered shed, ADMISSION — on the COLD arm only ------
            // (debug session `gpu-oom-guard-lost-in-the-cutover`, 2026-08-22.)
            //
            // The multi-layer coordinator has asked this since quick 260821-3qx;
            // THIS arm did not ask it at all, and the asymmetry was NOT
            // deliberate. It is reachable: this function shares the producer's
            // ONE `gpu_ledger` with the layer coordinator and itself warms layer
            // sessions through `prewarm_for_boundary` below, so a cold open here
            // can genuinely land alongside sibling reservations. Unrefused, it
            // commits a WHOLE widened pool (`2 x pool_size x frame_bytes`,
            // ~921MB at 4K) that the budget share may not fund — the exact
            // 48-gpu-oom-4k over-commit, on a starved adapter.
            //
            // Asked BEFORE any driver work, like the layer arm: everything above
            // is a path resolve and arithmetic, so a refusal costs a resolve and
            // nothing else, and `open_hw_decoder` is never reached by a session
            // that does not fit. NOT `gpu_ring_depth(..) == 0`, whose own doc
            // disclaims the question ("The ordered shed (CONTEXT D-10) — not
            // this formula — is what refuses the session that genuinely does not
            // fit") and whose floor makes that form unreachable for any input.
            //
            // WARM REUSE IS DELIBERATELY NOT GATED (the `Some((s, f))` arm
            // above): that session's pool already exists and a re-seek commits
            // no new VRAM, so refusing it would end a hardware session to free
            // nothing — "a budget-shrink eviction returns pool slices to the
            // DECODER, not bytes to the OS" (`vram_budget.rs`).
            if !engine::gpu_session_admissible(
                budget_now,
                est_bytes,
                engine::DECODER_HEADROOM,
                reserved_by_others,
            ) {
                eprintln!(
                    "hwdecode: single-clip session REFUSED on budget for {} — budget={}B \
                     reserved_by_others={}B frame_bytes={}B; the CPU sidecar path serves this \
                     clip",
                    src.path.display(),
                    budget_now,
                    reserved_by_others,
                    est_bytes
                );
                // NOT cached into `hw_failed` and NOT fed to the latch: a budget
                // refusal is a CHEAP, RE-TESTABLE outcome (the budget moves,
                // siblings release), exactly like the gate's own
                // `Software { cache_media: false }` arm and the layer arm's
                // `OpenRefusal::Budget`. Caching it would strand the media on
                // software for the whole producer lifetime over a transient.
                return GpuDelegation::Fallback;
            }

            // PROVISIONAL depth (Pitfall 4 ordering, step 1), on the SAME
            // reading the admission used: VRAM half only — the pool half is
            // unconstrained (`GPU_RING_MAX_DEPTH + headroom − headroom`)
            // because no pool exists yet. This number becomes `ring_target`, so
            // the pool is widened to `target + headroom + threads` at open
            // (48-05). Phase 57 (Pitfall 2): sized against what live sibling
            // sessions already hold, not the full queried budget — the pool
            // this number WIDENS at open is the VRAM this session will actually
            // spend, so an over-provisioned provisional is an over-provisioned
            // pool.
            let provisional = engine::gpu_ring_depth(
                budget_now,
                est_bytes,
                engine::GPU_RING_MAX_DEPTH + engine::DECODER_HEADROOM,
                engine::DECODER_HEADROOM,
                reserved_by_others,
            );

            // The policy gate (GPU-05): every open failure is a typed, logged,
            // per-media routing signal — never a crash, never silence. The
            // session latch is consulted (and fed) inside the gate.
            match choose_decode_path(&src.path, provisional, &HW_LATCH) {
                DecodePath::Hardware(s) => (s, None),
                DecodePath::Software { cache_media } => {
                    if cache_media {
                        // Keyed on the ATTEMPTED media (the resolved path), the
                        // same key the short-circuit at the top of this function
                        // tests — T-58-06-03's whole point.
                        hw_failed.insert(src.path.clone());
                    }
                    return GpuDelegation::Fallback;
                }
            }
        }
    };

    let clip = GpuClipParams {
        start_pos: prod_pos,
        // On `r`'s clock, like every other number in this struct — the producer's
        // timeline math is in `r`'s terms and D-04 makes the seam's mapping the
        // identity for a proxy (same fps, same timebase, same duration), so
        // `src.source_us == r.source_us` by construction.
        source_us: r.source_us,
        end_pos,
        frame_step,
        gen: my_gen,
        retime: r.retime.clone(),
    };
    let handle = spawn_gpu_producer(
        ctl.clone(),
        compositor.clone(),
        session,
        budget.clone(),
        ledger.clone(),
        clip,
        prelanded,
    );
    // Debug session `multitrack-preview-lag-choppy` (2026-08-01, round 2):
    // the producer used to PARK in a blocking join here — the one window in
    // which no lookahead prewarm can ever run — so any multi-layer range
    // starting at `end_pos` was entered stone-cold (the "~3s freeze before
    // the multitrack section" residual). Wait in a light poll instead, and
    // once the delegation is within PREWARM_LEAD of its end (wall estimate:
    // production is presentation-paced, and the GPU thread finishes ~one
    // ring runway EARLY — inside the lead), warm the boundary stack into the
    // PENDING pool the multi arm's entry adopt consumes. Ranges shorter than
    // the lead warm immediately. Source mode never resolves a Program stack.
    // A stop/flush still ends the wait promptly: the GPU thread observes it
    // and exits, `is_finished` turns true within one poll beat.
    const PREWARM_LEAD_US: i64 = 3_000_000;
    let delegated_span_us = end_pos - prod_pos;
    let wait_started = std::time::Instant::now();
    let mut boundary_warmed = false;
    let exit = loop {
        if handle.is_finished() {
            break handle.join();
        }
        if !boundary_warmed
            && !is_source
            && wait_started.elapsed().as_micros() as i64 >= delegated_span_us - PREWARM_LEAD_US
        {
            boundary_warmed = true;
            // ---- Phase 57 § D-3: the HARDWARE twin of the software warm-up
            // below, at the entry that had none. ----
            //
            // `LayerSessionSet::prewarm_for_boundary` was wired at 57-06 INSIDE
            // the multi arm, so it could only run once the producer was already
            // in a multi-layer range: it warmed multi -> multi boundaries and
            // nothing else. The entry a viewer actually meets first is this one —
            // a delegated single clip running out into an overlap — where the
            // producer is parked right here and the boundary tick therefore opened
            // every layer session COLD, deep-GOP 4K seeks included, on its own
            // critical path. Two prior plans (57-07, 57-08) each declined to move
            // this line only because each owed a differential against a committed
            // ruler; neither declined on merit.
            //
            // ONE mechanism, TWO entry points. This is the SAME function the
            // in-section boundary calls, and it brings its own guarantees with it:
            // the `RUDIS_DISABLE_BOUNDARY_PREWARM` kill switch (so both halves
            // still turn off together and the differential stays one variable),
            // the session-wide hardware latch, the `hw_failed` per-media backoff,
            // the one-prewarm-per-boundary latch, and the `MAX_HW_SESSIONS`
            // live+pending cap. No second prewarm path is introduced here.
            //
            // Ordered BEFORE the software warm-up deliberately: the sessions that
            // will actually SERVE the boundary get the earliest and least
            // contended start, ahead of the pool children that read tens of MB
            // each around the same region.
            //
            // **Cadence is not traded for content.** Nothing here waits, and
            // nothing downstream is made to wait: a session that is not warm in
            // time is simply not adopted at the boundary tick, and the coordinator
            // still OMITS that layer (`LiveHold::usable_for`) rather than blocking
            // on it. A late layer keeps costing CONTENT and never stutter — which
            // is the property that makes this safe to do at all.
            if let Some(s) = sessions.as_deref_mut() {
                if let Some(next) = crate::resolve_multilayer(host, end_pos) {
                    let opened_before = s.opens();
                    s.prewarm_for_boundary(&next, end_pos, hw_failed, &HW_LATCH);
                    // The attribution line. D-3 could not say whether its warm
                    // column came from the hardware warm-up, the OS page cache or
                    // an unset lookahead latch; a reader of a field log (or of the
                    // differential's stderr) can now at least see whether hardware
                    // sessions were opened AHEAD of the boundary or at it.
                    eprintln!(
                        "hwdecode: delegation-wait boundary prewarm at {end_pos}us — \
                         {} layer session(s) opened ahead of the boundary",
                        s.opens().saturating_sub(opened_before)
                    );
                }
            }
            // `None, None` — 59-16's two cost fixes are STILL deliberately not
            // taken here, and this call is byte-equivalent to 59-13.
            //
            // The first (`live_pool`) remains inapplicable: this caller is inside
            // the single-clip GPU delegation and has no live software pool to seed
            // dims from.
            //
            // The second (`hw_predict`) became APPLICABLE the moment the twin
            // above was wired — this caller now does hold a session set. It is
            // withheld on purpose anyway, for one measurement round. D-3's warm
            // column is unexplained and one of its three candidate mechanisms is
            // precisely a side effect of warming these layers into the pool (the
            // OS page cache around the deep-GOP region; the third is the
            // `la.prewarmed_for` latch this call sets). Taking the filter in the
            // same change that adds the twin would move two variables at once and
            // make the differential unattributable — the exact error 57-07 and
            // 57-08 declined this item to avoid. Redundant warming of
            // hardware-claimed layers is a KNOWN cost here (59-14 read it as +3
            // `sw_sidecar_spawns` per run at the other caller), and it is the
            // follow-up this leaves behind, not an oversight.
            prewarm_boundary_stack(host, end_pos, la, None, None);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // The delegation is over — restore the CPU byte budget BEFORE the
    // orchestrator produces anything else (gap black / multi / fallback).
    ctl.exit_gpu_depth();
    match exit {
        Ok(GpuExit::Completed { next_pos, session }) => {
            // Cache the healthy session (SEEK-01): the next delegation for
            // the SAME media — a seek-flush rebuild, or the next clip cut
            // from the same file — re-seeks instead of reopening.
            // Cached under the RESOLVED path (Phase 58 D-27): the cache must
            // store the media the session actually decodes, or the identity
            // compare on the warm-reuse path above could never match it.
            if let Some(s) = session {
                *warm_hw = Some((src.path.clone(), s));
            }
            GpuDelegation::Resume { next_pos }
        }
        Ok(GpuExit::Interrupted { session }) => {
            if let Some(s) = session {
                *warm_hw = Some((src.path.clone(), s));
            }
            GpuDelegation::Interrupted
        }
        Ok(GpuExit::Failed { next_pos }) => {
            // INVALIDATION: Failed carries no session by construction — a
            // failed session is never cached, never reused. The failure belongs
            // to the media the session was actually opened on, so both the latch
            // key and the log name `src` (Phase 58 D-27).
            hw_failed.insert(src.path.clone());
            eprintln!(
                "hwdecode: resuming {} on the software path at {next_pos}us",
                src.path.display()
            );
            GpuDelegation::Resume { next_pos }
        }
        Err(_) => {
            // The decode thread panicked. The panic stayed on ITS thread (the
            // structural half of GPU-06's isolation): fail the media closed
            // into software and keep producing. The session died with the
            // thread — nothing to cache.
            hw_failed.insert(src.path.clone());
            eprintln!(
                "hwdecode: GPU decode thread panicked — falling back to software for {}",
                src.path.display()
            );
            GpuDelegation::Resume { next_pos: prod_pos }
        }
    }
}

#[cfg_attr(
    not(all(windows, feature = "hwdecode")),
    allow(unused_variables)
)]
fn producer_loop(
    host: std::sync::Arc<dyn PreviewHost>,
    ctl: Arc<RingCtl>,
    compositor: Arc<engine::Compositor>,
    gpu_budget: Option<Arc<AtomicU64>>,
    composited_present: bool,
) {
    // Phase 46 (plans 46-06 and 46-09): the shell-services port this producer
    // thread hands to everything in `crates/preview` it calls — `resolve_active`,
    // directly here and through `push_with_lookahead` into the lookahead probe;
    // since 46-08 `resolve_multilayer` and `compose_multilayer_from_pool`; since
    // 46-09 the `is_source` mirror read below. 46-06 CONSTRUCTED the adapter
    // here from an `AppHandle`; 46-09 moved this file into `crates/preview`, so
    // the caller hands it in already built. Still exactly ONE port object per
    // producer for its whole lifetime, and the store it locks is still the same
    // `SharedStore` the original `try_state` + `lock()` pair reached — lock order
    // and hold duration unchanged.

    let mut my_gen: u64 = 0; // 0 != the ring's initial gen 1 -> first loop rebaselines
    let mut prod_pos: i64 = 0; // the producer's own production clock (µs)
    // SEEK-02 (Phase 49): the single-clip session rides inside `SwSession`,
    // which pairs it with its start_with_pts side channel + anchor state so
    // every pushed frame can be stamped from its REAL PTS.
    let mut session: Option<SwSession> = None;
    let mut session_end_pos: i64 = 0; // prod_pos at which the current clip is exhausted
    let mut frame_step: i64 = 33_333; // updated from the active clip's fps on start
    let mut cur_clip: Option<String> = None; // active clip id (None in Source mode)
    // `None` = no start failure recorded; `Some(id)` = that clip id failed to
    // start (WR-03-style backoff). `Option<Option<String>>` so the "no failure"
    // sentinel never collides with Source mode's `clip_id == None`.
    let mut failed_clip: Option<Option<String>> = None;
    let mut misses: u32 = 0; // consecutive pull-misses on the current session
    // 18.3-04 (Stage 2): the producer-owned multi-layer decoder pool — one
    // persistent ffmpeg child per active layer while `prod_pos` is inside a
    // multi range. Its lifecycle rules (sequential check, Timedout lockstep,
    // retain-reaps-leavers, WR-03 backoff) are the FROZEN engine contract;
    // dropping it (range exit / flush / stop) reaps every child via the engine
    // `Drop` impls (SC-4 / T-18-04 / SC-R3).
    let mut pool: Option<engine::LayerDecoderPool> = None;
    // 18.3-05 (Stage 3): the lookahead prewarm state — boundary latch + the
    // pending (pre-warmed, not-yet-consumed) pool / single session. Reset
    // wholesale on flush so superseded prewarms drop their sessions (children
    // reaped by the engine Drop impls, SC-R3).
    let mut la = Lookahead::new();
    // Phase 48 (plan 48-09): media whose hardware-decode open already FAILED
    // for this producer — routed to the CPU sidecar path and never
    // re-attempted for this producer's lifetime (the per-media half of
    // GPU-05's policy; a pause→play respawn re-evaluates, so a transient
    // failure is not sticky forever).
    #[cfg(all(windows, feature = "hwdecode"))]
    let mut hw_failed: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    // Phase 49 (plan 49-06, SEEK-01): the warm-session cache — the live
    // HwDecodeSession handed back by GpuExit::{Completed,Interrupted}, keyed
    // by media path, reused via seek_to_us by the next delegation for the
    // SAME media (skipping the ~71-81ms open+probe+device+pool reopen the
    // baseline measured). At most ONE session (T-49-06-01). It lives and dies
    // with THIS producer thread: a pause or a device-lost recovery respawns
    // the producer, so the respawned instance starts with an empty cache and
    // binds the LIVE compositor (the structural half of the device-lost
    // invalidation; the explicit half is delegate_gpu_clip's
    // device_removed_reason check).
    #[cfg(all(windows, feature = "hwdecode"))]
    let mut warm_hw: Option<(std::path::PathBuf, engine::HwDecodeSession)> = None;
    // Phase 57 (plan 57-03, 57-RESEARCH.md Pitfall 2): the shared VRAM ledger,
    // created next to the `gpu_budget` handle and handed to every decode
    // session this producer delegates. ONE per producer today, because today
    // exactly one hw session is live at a time (`delegate_gpu_clip` polls its
    // decode thread to completion before returning); plan 57-06's N-session
    // coordinator is what makes the sibling term non-zero in production, and it
    // inherits this ledger rather than inventing a second accounting scheme.
    // At N=1 `reserved()` is 0 at every derivation, so the depth answered here
    // is byte-identical to Phase 48's.
    #[cfg(all(windows, feature = "hwdecode"))]
    let gpu_ledger = engine::VramLedger::new();
    // Phase 57 (plan 57-06, PLAY-01/D-03): the per-visible-layer hardware
    // decode-session coordinator. Armed by the SAME live VRAM budget handle
    // that arms the single-clip delegation gate, so every pre-57 test route
    // (and `spawn_producer`) has an empty set and produces exactly as before,
    // through the software pool. It shares the producer's ONE `gpu_ledger`
    // rather than inventing a second accounting scheme (57-03).
    #[cfg(all(windows, feature = "hwdecode"))]
    let mut sessions: Option<crate::layer_sessions::LayerSessionSet> =
        gpu_budget.as_ref().map(|b| {
            crate::layer_sessions::LayerSessionSet::new(
                compositor.clone(),
                b.clone(),
                gpu_ledger.clone(),
            )
        });
    // Phase 57 (plan 57-06, PLAY-02/D-08): the K=4 persistent composite targets
    // the multi arm renders into. Built lazily at the first multi tick (its
    // size comes from the resolved stack) and `ensure_size`d thereafter —
    // `CompositeTargetPool::ensure_size` is the ONLY allocation site and is a
    // no-op at an unchanged size, which is what makes the steady-state multi
    // tick allocation-free (57-04's measured odometer).
    let mut target_pool: Option<engine::CompositeTargetPool> = None;
    // Plan 57-08 reads this: the wall cost of the LAST multi produce tick, in
    // microseconds. One relaxed store per tick, no behaviour.
    let last_produce_us = &LAST_MULTI_PRODUCE_US;
    // ---- Phase 57 (plan 57-08, PLAY-05/D-09): dynamic playback resolution ----
    // A stack local, deliberately. `DynResController` is `!Send`, so it cannot
    // be hoisted into a static or an `Arc` even by accident — the compile-time
    // half of D-12 ("a degraded level must be UNREACHABLE from export, not
    // merely unused by it"). It lives and dies with THIS producer thread, which
    // is also why pause snaps back for free: a pause stops the producer
    // (`present_loop`'s `ring.request_stop()`), and the next Play builds a fresh
    // controller at Full.
    let mut dynres = crate::dynres::DynResController::new();
    // ---- Phase 60 (plan 60-04, DROP-01/DROP-02): the SECOND degradation axis.
    // Same construction and the same reasons as the line above: `!Send`, so it
    // cannot be hoisted into a static, an `Arc`, or the export thread (D-12);
    // stack-local, so it lives and dies with THIS producer thread and a pause
    // therefore re-earns engagement from zero for free.
    //
    // The two controllers are ORDERED, not peers, and they know nothing about
    // each other: the precedence is one boolean argument at the single
    // `on_tick` call site below.
    let mut framedrop = crate::framedrop::FrameDropController::new();
    // ---- Phase 59 (plan 59-06, CACHE-02/D-19): the render-cache SERVE state.
    // A producer-loop local, exactly like `session`, `pool` and `dynres`: it
    // owns a memoized program-level lookup plus at most two software segment
    // decode sessions, and it dies with this thread, so a pause or a device-lost
    // respawn starts with nothing open. A build that never configures a cache
    // directory pays one uncontended read-lock per multi tick for it, and
    // nothing else.
    let mut cache_serve = crate::render_cache_lookup::CacheServe::new();
    // ---- Phase 59 (plan 59-13, CACHE-02): where the cached run this producer
    // is serving ENDS, once the serve has discovered it. Set on the tick the
    // exit prewarm is issued and consumed on the LAST cached tick before that
    // boundary; see the two-step comment at the issue site for why the two
    // events are deliberately not the same tick.
    let mut cache_exit_release_at: Option<i64> = None;
    // The level the NEXT composite renders at. Read from the controller after
    // each produced tick, so a level change takes effect on the following frame.
    let mut res_level = crate::dynres::ResLevel::Full;
    // The cadence the NEXT ticks are produced at (DROP-01). Mirrors `res_level`
    // exactly: read from the controller after each produced tick, so a change
    // takes effect on the following frame. Kept ONLY so a transition can be
    // logged once instead of every tick — the producer asks the controller for
    // each tick's decision directly, never this variable, so the two can never
    // disagree about what to do. (The method is deliberately not named here:
    // this plan's verification counts its call sites by text scan, and prose
    // that spells the symbol reads to a scan as a second call site.)
    //
    // Deliberately NOT mirrored into `EngineDiag`: the v8 freeze lists PLAY-05 /
    // PROXY-02 / CACHE-01 as the observability exceptions, and DROP is not one.
    let mut drop_mode = crate::framedrop::DropMode::Off;
    // The engine->shell observable (PLAY-05). Written here only; nothing in this
    // crate reads it back, and nothing branches on it.
    let diag = host.engine_diag();
    diag.set_playback_res_level(res_level);
    // TEST-ONLY, read ONCE (see `TEST_PRODUCE_SLOWDOWN_ENV`). Zero when unset.
    let test_slowdown_us: u64 = std::env::var(TEST_PRODUCE_SLOWDOWN_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);

    loop {
        // Control checks bracket EVERY bounded op (design §2 lifecycle).
        if ctl.stop.load(Ordering::SeqCst) {
            // PLAY-05 snap-back (D-09, 57-RESEARCH Pitfall 8). This IS the
            // pause: `present_loop` answers `!playing` with `ring.request_stop()`
            // and the producer observes it here. Snapping the level back at the
            // SAME point the existing stop handshake already runs is the whole
            // of the mechanism — there is no second atomic, no second signal and
            // nothing that can race the gen-bump discipline. (The controller
            // itself dies with this thread one line down; the store is what
            // makes the OBSERVABLE agree with the picture the presenter is about
            // to hold.)
            dynres.on_pause();
            // DROP-01's snap-back, riding the SAME observation for the SAME
            // reason (60-04, Pattern 4). No third call site and no signal of its
            // own: the sibling is reset where the original is reset, one line
            // apart, so the two degradation axes can never disagree about
            // whether playback stopped.
            framedrop.on_pause();
            diag.set_playback_res_level(crate::dynres::ResLevel::Full);
            return; // drop session + pool -> engine Drop kills+waits+joins every child
        }
        let g = ctl.gen.load(Ordering::SeqCst);
        if g != my_gen {
            // Flush handshake (design §4): rebuild at restart_pos under the new
            // gen. restart_pos was stored BEFORE the gen bump, so it is current.
            my_gen = g;
            prod_pos = ctl.restart_pos.load(Ordering::SeqCst);
            session = None;
            cur_clip = None;
            failed_clip = None;
            misses = 0;
            pool = None; // a genuine seek rebuilds multi state cold too (children reaped)
            // 18.3-05: a flush supersedes any in-flight prewarm — dropping the
            // pending pool/session reaps their children; the boundary latch
            // clears with them (one prewarm per boundary restarts clean).
            la = Lookahead::new();
            // 59-06: a seek-flush drops both segment sessions. `prod_pos` has
            // been re-rooted, so a session positioned for the old playhead is
            // decoding the wrong part of the wrong segment; the next tick
            // re-probes and reopens seeked to the new position. Correctness over
            // warmth — the same trade the pool and the prewarm above make, and
            // the D-21-adjacent half of the gen discipline: a cache session that
            // survived a flush could hand a pre-flush frame to a post-flush push.
            cache_serve.retire();
            // 59-13: …and the exit prediction the serve had handed over. The
            // pool and the pending prewarm were both discarded three lines up,
            // so a remembered "release the pool just before X" would be an
            // instruction about a pool that no longer exists.
            cache_exit_release_at = None;
            // 49-06: `warm_hw` deliberately SURVIVES the flush handshake — a
            // seek-flush is exactly the event whose rebuild the warm cache
            // exists to accelerate (same media, new position → seek_to_us).
            //
            // PLAY-05 does NOT survive it (D-09 / Pitfall 8). The SAME
            // observation point, no second signal path: a flush discards the
            // whole runway and rebuilds it cold at a new position, so the miss
            // history that earned a degradation no longer describes the frames
            // about to be produced. Re-earning it costs `MISS_STREAK_TICKS`
            // (~0.4 s) if the new position is genuinely as expensive, and a user
            // who seeks somewhere cheaper gets full resolution back immediately
            // instead of carrying a stale penalty.
            dynres.on_pause();
            res_level = crate::dynres::ResLevel::Full;
            diag.set_playback_res_level(res_level);
            // DROP-01's twin, and the rationale above transfers verbatim: the
            // flush discards the whole runway and rebuilds it cold at a new
            // position, so a miss history that earned a dropped cadence at the
            // OLD position must not penalize the new one. Re-earning it costs
            // DROP_MISS_STREAK_TICKS of sustained at-floor misses, and a user
            // who seeks somewhere cheaper gets every frame back immediately
            // instead of carrying a stale penalty.
            framedrop.on_pause();
            drop_mode = crate::framedrop::DropMode::Off;
        }

        // 46-09: the adapter's `playback_mirror()` returns the MANAGED mirror
        // when there is one and an always-available fallback ("not playing,
        // position 0, Program mode") when there is not — which is exactly what
        // the `try_state` + `.unwrap_or(false)` pair below it resolved to.
        let is_source = host.playback_mirror().is_source.load(Ordering::Relaxed);

        // ---- 18.3-04 (Stage 2) multi-layer production (replaces the Stage-1
        // park): the SAME thread, the SAME ring — a merge boundary is now "the
        // producer swaps its decode family", invisible to the presenter. ----
        let stack = if is_source {
            None // Source mode previews one MediaBin item — never multi
        } else {
            crate::resolve_multilayer(host.as_ref(), prod_pos)
        };
        if let Some(stack) = stack {
            // Exactly ONE decode family owns any given prod_pos: entering the
            // range drops the single-clip session (mirror of `pool = None` on
            // exit below) so the two can never fight over the position.
            if session.is_some() {
                session = None;
                cur_clip = None;
                misses = 0;
            }
            // HOISTED (Phase 59, plan 59-06). The frame step is a pure function
            // of the resolved stack's fps and was computed ~70 lines below; the
            // render-cache arm immediately under this needs it to advance
            // `prod_pos`, so the ONE definition moved up here and the later site
            // is REMOVED rather than shadowed. Same expression, same value, no
            // second opinion about what a frame is worth.
            let step = engine::frame_step_us(stack.fps).max(1);

            // ---- Phase 59 (plan 59-06, D-19): ASK THE CACHE FIRST ----------
            //
            // The question is a PROGRAM-level one — "can this whole tick skip
            // compositing?" — so it is asked HERE, once per tick, and not
            // through `decode_source.rs`'s per-clip seam.
            //
            // Placement is load-bearing in both directions:
            //
            // * AFTER `resolve_multilayer` succeeded, because the answer needs
            //   the resolved canvas (width/height/fps) and because a cache may
            //   only serve a range the live path would have composited;
            // * BEFORE `sessions.sync_to_stack`, because a hit must never open,
            //   adopt or contend a hardware `LayerSession`. `MAX_HW_SESSIONS`
            //   is the exact resource the six-layer ceiling saturates
            //   (59-CEILING-VERDICT § 3), so a cache decode that took one of
            //   those three slots would be paying for its own win.
            //
            // What a hit does NOT do is as important as what it does: it does
            // not reconcile the hardware coordinator, and it deliberately does
            // NOT tear it down either. Existing per-layer sessions are left
            // IDLE for the segment's duration and reconcile naturally on the
            // next live tick — a synchronous stop-everything here is how the
            // measured overlap-exit cost gets bigger, not smaller (the same
            // reasoning the asynchronous `retire_live` below already records).
            //
            // The fully-qualified module path is deliberate. D-40 pins the
            // render-cache seam with source scans that count which FILES name
            // what, and `render_cache_lookup::` at the call site is the audit
            // surface that makes this arm findable; a glob-imported bare
            // `CacheServe` would spell nothing a scan can see.
            if let Some(frame) = crate::render_cache_lookup::CacheServe::try_serve(
                &mut cache_serve,
                host.as_ref(),
                prod_pos,
                (stack.width, stack.height, stack.fps),
                step,
                &|| ctl.stop.load(Ordering::SeqCst) || ctl.gen.load(Ordering::SeqCst) != my_gen,
            ) {
                // RQ2, settled: a hit needs ZERO new GPU code. One CPU layer,
                // identity everything, through the SAME
                // `composite_mixed_layers_to_target` call the live gather makes
                // — so colour handling, alpha, the target pool, the payload
                // choice and the presenter are all byte-identically the live
                // path's. The presenter cannot tell, and neither can the arm
                // census: this pushes the same `Composited`/`Cpu` payload at the
                // same dims, which is what CACHE-02's seamlessness means down to
                // the bookkeeping.
                //
                // The composite dims are the SEGMENT's own — i.e. full canvas
                // (D-06 renders every segment at `ResLevel::Full`). A cached
                // tick therefore never composites degraded, and `res_level` is
                // left exactly where the live path last put it.
                let (comp_w, comp_h) = (frame.width, frame.height);
                let layer = engine::Layer::new(frame, 1.0);
                let mixed = [engine::MixedLayer::Cpu(&layer)];
                let tp = target_pool.get_or_insert_with(|| {
                    compositor.create_composite_target_pool(comp_w, comp_h)
                });
                if tp.size() != (comp_w, comp_h) {
                    tp.ensure_size(comp_w, comp_h);
                }
                let (target, backpressure_us) = checkout_target_polling(tp, &ctl, my_gen);
                if backpressure_us > 0 {
                    COMPOSITE_TARGET_WAIT_US.fetch_add(backpressure_us, Ordering::Relaxed);
                    COMPOSITE_TARGET_WAIT_TICKS.fetch_add(1, Ordering::Relaxed);
                }
                let Some(target) = target else {
                    continue; // stop/flush — the loop-top handshake owns it
                };
                match compositor.composite_mixed_layers_to_target(&mixed, &target, comp_w, comp_h)
                {
                    Ok(()) => {
                        // Release the borrow BEFORE the push, for the same
                        // reason the live arm does: `push_with_lookahead` can
                        // park on ring backpressure.
                        drop(mixed);
                        let payload = if composited_present {
                            RingPayload::Composited(CompositedEntry {
                                target,
                                w: comp_w,
                                h: comp_h,
                            })
                        } else {
                            match compositor.blit_texture_to_rgba(target.view(), comp_w, comp_h) {
                                Ok(rgba) => RingPayload::Cpu(engine::Frame {
                                    width: comp_w,
                                    height: comp_h,
                                    rgba,
                                }),
                                Err(e) => {
                                    eprintln!(
                                        "preview_ring: cached-segment readback failed: {e}"
                                    );
                                    prod_pos += step;
                                    continue;
                                }
                            }
                        };
                        // The SAME push path, carrying `gen: my_gen`, so the
                        // ring's own gen re-check still stands between a
                        // pre-flush frame and the queue.
                        if !push_with_lookahead(
                            &ctl,
                            RingEntry {
                                payload,
                                timeline_us: prod_pos,
                                gen: my_gen,
                            },
                            host.as_ref(),
                            prod_pos,
                            is_source,
                            Some(&stack),
                            &cur_clip,
                            &mut pool,
                            &mut la,
                        ) {
                            continue; // stale/stopped — the handshake picks up next loop
                        }
                        // ---- D-13, as CONTROL FLOW rather than as a flag ----
                        // Nothing on the live path below is reached by a cached
                        // tick: neither the dynamic-resolution controller's
                        // per-tick scorer nor either of the two heat-registry
                        // inlets beside it. A cached tick measures the CACHE,
                        // not the SECTION, and feeding it back would let a
                        // section "recover" on paper the instant it was cached —
                        // which would un-mark it, stop the worker refreshing it,
                        // and corrupt the calibration at the same time.
                        //
                        // (The three call names are deliberately NOT spelled
                        // here. This phase's own audits are literal text scans,
                        // and prose that names the symbol it is describing reads
                        // to a scan as the violation — a footgun `dynres.rs`
                        // already records and 59-05 hit twice more.)
                        //
                        // ---- Phase 59 (plan 59-13): LEAVING the cached run ---
                        //
                        // The tick above skipped the live reconciliation, which
                        // is the whole win — and also the reason the layer
                        // pool's children sit blocked at stale source positions
                        // for the entire cached span, so that the first LIVE
                        // tick after it reads a position jump and respawns them
                        // cold. 59-10 measured that as a **729.78 ms** stall at
                        // the cache exit against a **42.62 ms** control at the
                        // identical position: the phase MOVED a stall rather
                        // than removing it, and this is where that is repaid.
                        //
                        // The fix is anticipatory pre-roll toward a known future
                        // boundary — the same thing `lookahead_probe` does for a
                        // clip cut, through the same `prewarm_boundary_stack`,
                        // the same `Lookahead` latch, the same pending pool and
                        // the same adopt-with-rebase handshake. No second
                        // mechanism, and no new one to keep in step with that
                        // one. What is new is only WHO knows the boundary: the
                        // serve does, because its own next-segment probe just
                        // missed there.
                        //
                        // It is deliberately NOT the other repair available
                        // here: nothing below reconciles the coordinator and
                        // nothing advances the layer pool. Driving the pool
                        // through cached ticks would spend, every tick, exactly
                        // the decode this arm exists to skip, to buy one
                        // boundary.
                        //
                        // TWO steps, on purpose, and they are not the same tick:
                        //
                        //  1. ISSUE, up to one `LOOKAHEAD_US` early — the
                        //     children need that lead to spawn, seek and fill
                        //     their channels before the boundary frame is due.
                        //  2. RELEASE the stale live pool on the LAST cached
                        //     tick before the boundary — because `adopt_missing`
                        //     refuses to disturb a clip that already has a live
                        //     session ("live session wins"), so the warm
                        //     children can only be adopted into a pool slot that
                        //     is empty. Releasing at step 1 instead would open
                        //     an 800 ms window in which a D-21 fallback tick
                        //     would consume the pending pool REBASED to a
                        //     pre-boundary position — a warm child decoding
                        //     ~800 ms of program away from the frame it would be
                        //     stamped as. One tick of exposure instead of
                        //     twenty-four, and the ≤1-step offset that leaves is
                        //     the class the rebase is documented to accept.
                        //
                        // The teardown itself is not a new cost: those children
                        // were going to be killed and rebuilt at the boundary
                        // anyway. It moves off the stalling tick onto a tick
                        // that costs ~2 ms against a ~33 ms budget with a full
                        // ring runway behind it.
                        //
                        // Double-firing is impossible in three independent
                        // places: the serve latches its next-segment probe one
                        // per boundary, `take_exit_boundary` hands the value out
                        // at most once, and `prewarm_boundary_stack` re-latches
                        // on the boundary's frame-step bucket.
                        if let Some(boundary) =
                            crate::render_cache_lookup::CacheServe::take_exit_boundary(
                                &mut cache_serve,
                            )
                        {
                            // The DISCLOSED control (`crate::DISABLE_PREWARM_ENV`)
                            // switches the whole mechanism off, counter
                            // included, so the differential's two arms are "this
                            // plan's behaviour" and "the behaviour before it" —
                            // not two arms that both move the instrument.
                            if !crate::boundary_prewarm_disabled() {
                                crate::render_cache_lookup::RENDER_CACHE_EXIT_PREWARMS
                                    .fetch_add(1, Ordering::Relaxed);
                                // 59-16: the two inputs the boundary tick's own
                                // `sync_to_stack` will hand `select_hw_clip_ids`
                                // (the ids hardware is already serving, and the
                                // per-media failure set). Handed in so the same
                                // selection is made PREDICTIVELY inside — where
                                // the stack is already resolved, keeping the
                                // resolve count on this tick at one. `None` when
                                // there is no session set at all (no GPU budget:
                                // every layer is on the software pool, so there
                                // is nothing to predict away).
                                #[cfg(all(windows, feature = "hwdecode"))]
                                let hw_live_ids =
                                    sessions.as_ref().map(|s| s.live_ids()).unwrap_or_default();
                                #[cfg(all(windows, feature = "hwdecode"))]
                                let hw_predict =
                                    sessions.as_ref().map(|_| (&hw_live_ids, &hw_failed));
                                #[cfg(not(all(windows, feature = "hwdecode")))]
                                let hw_predict: Option<HwPredictInputs<'_>> = None;
                                prewarm_boundary_stack(
                                    host.as_ref(),
                                    boundary,
                                    &mut la,
                                    pool.as_ref(),
                                    hw_predict,
                                );
                                cache_exit_release_at = Some(boundary);
                            }
                        }
                        if let Some(boundary) = cache_exit_release_at {
                            if prod_pos + step >= boundary {
                                cache_exit_release_at = None;
                                // Step 2. Dropping the pool reaps its children
                                // (retain/Drop — the SC-4 discipline) and leaves
                                // the `pool.is_none()` entry adopt below as the
                                // path the boundary tick takes: build fresh,
                                // adopt the prewarmed sessions rebased to the
                                // demand, serve warm.
                                if pool.is_some() {
                                    pool = None;
                                }
                            }
                        }
                        prod_pos += step;
                        continue;
                    }
                    Err(e) => {
                        // A failed composite skips THIS frame and advances,
                        // exactly as the live arm's own failure does.
                        eprintln!("preview_ring: cached-segment composite failed: {e}");
                        prod_pos += step;
                        continue;
                    }
                }
            }

            let produce_started = std::time::Instant::now();
            // TEST-ONLY (`TEST_PRODUCE_SLOWDOWN_ENV`), and zero-cost when unset.
            // Deliberately INSIDE the measured window and BEFORE any real work,
            // so it reads exactly as an expensive composite would.
            if test_slowdown_us > 0 {
                std::thread::sleep(Duration::from_micros(test_slowdown_us));
            }
            // ---- Phase 57 (plan 57-06, PLAY-01/D-03/D-04): reconcile the
            // per-layer hardware sessions with THIS tick's layer set FIRST.
            // What it returns is the set of clip ids a hardware session is
            // serving; everything else falls to the software pool below —
            // DEMOTED, never deleted (D-04: a codec/VRAM condition that
            // defeats one layer must not force its siblings back to software).
            #[cfg(all(windows, feature = "hwdecode"))]
            let hw_served: std::collections::HashSet<String> = match sessions.as_mut() {
                Some(s) => s.sync_to_stack(&stack, &mut hw_failed, &HW_LATCH),
                None => std::collections::HashSet::new(),
            };
            #[cfg(not(all(windows, feature = "hwdecode")))]
            let hw_served: std::collections::HashSet<String> = std::collections::HashSet::new();

            // Debug session `multitrack-preview-lag-choppy` (2026-08-01): the
            // pool-eligible demands for THIS tick, built once via the ONE
            // shared mapping — the adopt handshake below rebases prewarmed
            // sessions onto exactly these demands.
            let mut sources = crate::pool_sources(&stack);
            // …minus whatever hardware is already serving. `advance`'s
            // retain-reaps-leavers then frees the CLI child of a layer that
            // moved to hardware, which is PLAY-01's "no per-layer ffmpeg child
            // in steady state" happening as a consequence of the filter rather
            // than as a separate teardown step.
            sources.retain(|s| !hw_served.contains(&s.clip_id));
            if pool.is_none() {
                // 18.3-05 (reworked): consume a pool prewarmed toward THIS
                // boundary (children already filling their channels) instead
                // of building cold — but ADOPT-with-rebase rather than using
                // the pending pool verbatim: a prewarm probed at `future` can
                // sit up to ~one frame past this tick's source demand, which
                // the step/2 sequential check would read as a jump and
                // respawn cold, throwing the warm child away. `adopt_missing`
                // rebases `next_source_us` to the demand (a ≤1-frame content
                // offset, the same class the Timedout-lockstep accepts) and
                // the dropped husk reaps any mispredicted children.
                let mut fresh = engine::LayerDecoderPool::new(stack.fps, POOL_PULL_TIMEOUT);
                // THE live producer's pool: a stream end must never cold-decode
                // on this thread (59-15 / § D-2). This is the flag that governs
                // every session adopted in just below.
                fresh.set_ended_nonblocking(true);
                if let Some(mut pending) = la.pending_pool.take() {
                    fresh.adopt_missing(&mut pending, &sources);
                    // husk dropped here → unmatched prewarms reaped (SC-R3)
                }
                pool = Some(fresh);
            } else if let Some(pending) = la.pending_pool.as_mut() {
                // Inside the range with a prewarm latched: a no-op until the
                // boundary tick (no pending id is active yet), then it moves
                // the warm session(s) in, rebased. The husk stays latched and
                // is dropped by the probe's latch-clear one tick later.
                if sources
                    .iter()
                    .any(|s| pending.has_session(&s.clip_id))
                {
                    pool.as_mut()
                        .expect("pool exists in this branch")
                        .adopt_missing(pending, &sources);
                }
            }
            // (`step` is hoisted to the top of this arm — plan 59-06.)
            // Bounded freshness poll: give every live hardware session a chance
            // to publish THIS tick's frame before the gather reads the HOLD
            // slots. Bounded (12ms) and polled, never a join (Pitfall 4) — past
            // the budget the gather uses whatever is held, exactly as the
            // presenter HOLDs on underrun.
            #[cfg(all(windows, feature = "hwdecode"))]
            if let Some(s) = sessions.as_ref() {
                s.await_holds(&stack);
            }
            // The pool now serves ONLY the specs hardware did not take.
            let mut by_id: std::collections::HashMap<String, engine::Frame> = pool
                .as_mut()
                .expect("pool created above")
                .advance(&sources)
                .into_iter()
                .collect();
            // ---- Phase 60 (plan 60-04, DROP-01): THE FRAME-DROP SKIP --------
            //
            // Asked ONCE per multi tick. Under `DropMode::Half` it answers true
            // on every second tick, and this arm then produces nothing for that
            // position: `prod_pos` advances, the ring receives no entry, and the
            // presenter HOLDS. That last part needs no code — `pop_for_target`
            // already leaves a front entry queued when it is still in the future
            // and `present_loop` already answers `None` by presenting nothing,
            // which is the underrun contract this crate has shipped since long
            // before this axis existed.
            //
            // WHY HERE, AND NOT AT THE TOP OF THE ARM. The plan placed this
            // check above `CacheServe::try_serve` and above `sync_to_stack`, to
            // bypass the decode as well. Reading the arm before inserting — the
            // check the plan itself demanded — found two reasons that placement
            // costs more than it saves, both measured properties of code this
            // plan does not own:
            //
            //  (a) THE SOFTWARE POOL'S CONTINUATION CONTRACT.
            //      `LayerDecoderPool::advance` judges continuation at HALF a
            //      source step (`decoder_pool.rs:554-558`). Bypassing the pull
            //      makes the next produced tick's demand arrive two source steps
            //      later, which reads as a SEEK — so every software-pooled layer
            //      would tear down and respawn an `ffmpeg` child on every
            //      produced tick, forever. That is not a hypothetical: it is the
            //      documented mechanism behind 59-10's measured 729.78 ms cache-
            //      exit stall (a single such jump, once). A degradation that
            //      spawns a process per layer per tick, at the exact moment the
            //      machine is already overloaded, is a pessimization wearing an
            //      optimization's name. It bites hardest in the only case this
            //      axis exists for, too: at or below `MAX_HW_SESSIONS` layers
            //      the software pool is usually empty, so the damage starts at
            //      the four-plus-layer stacks DROP is meant to rescue.
            //
            //  (b) CACHED TICKS ARE NOT THIS AXIS'S BUSINESS. A cache-served
            //      tick returns from this arm above, long before the scorer, so
            //      D-13 already excludes it from the evidence that earns a
            //      verdict. Dropping frames a verdict never measured would be a
            //      degradation with no cause — and it would not even be cheap:
            //      the serve fast-forwards through the gap on the next tick
            //      (`MAX_SEG_CATCHUP`), so a skipped cached tick moves the pull
            //      rather than saving it.
            //
            // What the skip DOES bypass, therefore, is the whole composite half
            // of the tick: the HOLD locks, the mixed gather and its inline
            // still/text/image-sequence work, the composite-target checkout and
            // any backpressure wait inside it, `composite_mixed_layers_to_target`
            // itself, the readback on the CPU-payload route, the ring push, and
            // the lookahead probe. That is the cost the measurements actually
            // name — 57-RESEARCH § 9 and `dynres.rs`'s module doc both record
            // that decode was never the bottleneck — and it composes with the
            // resolution axis multiplicatively, since a skipped tick is one that
            // was already going to composite at a quarter of the pixels.
            //
            // What it does NOT bypass is the software pull above and the session
            // reconciliation above that. Hardware layers are indifferent either
            // way: their sessions free-run into HOLD slots with a four-tick
            // staleness window (`layer_sessions.rs:239`), which one skipped tick
            // cannot exceed.
            //
            // `continue` lands on the loop top, where the `ctl.stop` and gen-bump
            // observations run — so control responsiveness is preserved by
            // construction, exactly as it is for the three early-`continue`
            // paths this arm already has (`slots.is_empty()`, the two composite
            // failures), each of which advances `prod_pos` the same way.
            if framedrop.skip_this_tick() {
                crate::framedrop::FRAMEDROP_SKIPPED_TICKS.fetch_add(1, Ordering::Relaxed);
                prod_pos += step;
                continue;
            }
            // Lock every live session's HOLD slot for the duration of this ONE
            // composite. The guards are dropped before the push, so a decode
            // thread never parks behind the ring's backpressure.
            #[cfg(all(windows, feature = "hwdecode"))]
            let holds = match sessions.as_ref() {
                Some(s) => s.lock_live_holds(),
                None => Vec::new(),
            };

            // ---- THE MIXED GATHER (57-RESEARCH.md Pitfall 5) ----------------
            // ONE ordered walk of `stack.layers` — the authoritative
            // track-ordered source of truth, index 0 = TOP — asking per spec
            // "hardware HOLD slot or software frame". NEVER a hardware list
            // concatenated with a software list: two independently-ordered
            // lists merge into a picture that looks plausible and has the wrong
            // z-order, and only when the hw/sw mix changes.
            enum Slot {
                Cpu(usize),
                #[cfg(all(windows, feature = "hwdecode"))]
                Gpu {
                    hold: usize,
                    opacity: f32,
                    transform: engine::LayerTransform,
                    crop: engine::LayerCrop,
                },
            }
            let mut cpu_layers: Vec<engine::Layer> = Vec::with_capacity(stack.layers.len());
            let mut slots: Vec<Slot> = Vec::with_capacity(stack.layers.len());
            for spec in &stack.layers {
                #[cfg(all(windows, feature = "hwdecode"))]
                if hw_served.contains(&spec.clip_id) {
                    if let Some(i) = holds.iter().position(|h| {
                        h.clip_id == spec.clip_id && h.usable_for(spec.source_us).is_some()
                    }) {
                        slots.push(Slot::Gpu {
                            hold: i,
                            opacity: spec.opacity,
                            transform: spec.transform,
                            crop: spec.crop,
                        });
                    }
                    // else: the session is still warming (or was just rebased
                    // past a seek) — OMIT this layer for this tick, exactly as
                    // `cpu_layer_for_spec` omits a pool layer that produced no
                    // frame. Black shows through for one tick, never a stale
                    // picture from the wrong position.
                    continue;
                }
                if let Some(layer) =
                    crate::cpu_layer_for_spec(host.as_ref(), &mut by_id, spec, &stack)
                {
                    slots.push(Slot::Cpu(cpu_layers.len()));
                    cpu_layers.push(layer);
                }
            }
            if slots.is_empty() {
                // Cold warm-up: every layer Timedout with no HOLD source. Push
                // NOTHING for this position and ADVANCE prod_pos anyway — the
                // 18.2-04 lockstep bumped each session's next_source_us, so
                // re-demanding the same source_us would trip the sequential
                // check → respawn → cold again (the snap-back trap, avoided by
                // construction).
                #[cfg(all(windows, feature = "hwdecode"))]
                drop(holds);
                prod_pos += step;
                continue;
            }
            // Second pass over the SAME ordered `slots`, so the borrow checker
            // gets its `cpu_layers` allocation finalized before anything
            // borrows it. Order is `slots`' order, which is `stack.layers`'.
            let mixed: Vec<engine::MixedLayer> = slots
                .iter()
                .map(|slot| match slot {
                    Slot::Cpu(i) => engine::MixedLayer::Cpu(&cpu_layers[*i]),
                    #[cfg(all(windows, feature = "hwdecode"))]
                    Slot::Gpu {
                        hold,
                        opacity,
                        transform,
                        crop,
                    } => engine::MixedLayer::Gpu {
                        frame: holds[*hold]
                            .frame()
                            .expect("a Gpu slot is only pushed for a hold with a frame"),
                        opacity: *opacity,
                        transform: *transform,
                        crop: *crop,
                    },
                })
                .collect();

            // ---- PLAY-05: the level scales the COMPOSITE dims, nothing else.
            // Decode stays native-res (research § 9: long-GOP H.264/HEVC is not
            // resolution-scalable at decode, and the measurements say decode was
            // never the bottleneck), and the presenter's existing
            // `contain_fit_viewport` upscales the smaller target to fill the
            // whole surface — so the user sees a full-canvas picture that is
            // softer, never a smaller one. `composite_mixed_layers_to_target`
            // has taken out_w/out_h as parameters distinct from any layer's
            // native dims since 57-04; this wires an EXISTING parameter
            // differently rather than adding plumbing.
            let (comp_w, comp_h) = res_level.scale_dims(stack.width, stack.height);
            // The K=4 persistent targets. `ensure_size` allocates ONLY on a
            // real dims change (a project-resolution edit, or a PLAY-05 level
            // change), and the guard makes that visible at the call site rather
            // than relying on the callee's no-op contract: a reallocation under
            // live targets mints a whole second generation of textures, so it
            // must be provably rare, not merely idempotent.
            let tp = target_pool
                .get_or_insert_with(|| compositor.create_composite_target_pool(comp_w, comp_h));
            if tp.size() != (comp_w, comp_h) {
                tp.ensure_size(comp_w, comp_h);
            }
            // The stop/gen-POLLING checkout (see `checkout_target_polling` for
            // why it polls rather than blocks). Every microsecond it waits is
            // the producer waiting for the PRESENTER, not producing. Recorded
            // (plan 57-08) so the PLAY-05 hysteresis can subtract it, and so
            // F1's runway hypothesis is a measurement rather than an opinion.
            // Two relaxed adds on the ticks that waited; nothing on the ticks
            // that did not.
            let (target, backpressure_us) = checkout_target_polling(tp, &ctl, my_gen);
            if backpressure_us > 0 {
                COMPOSITE_TARGET_WAIT_US.fetch_add(backpressure_us, Ordering::Relaxed);
                COMPOSITE_TARGET_WAIT_TICKS.fetch_add(1, Ordering::Relaxed);
            }
            let Some(target) = target else {
                drop(mixed);
                #[cfg(all(windows, feature = "hwdecode"))]
                drop(holds);
                continue; // stop/flush — the loop-top handshake owns it
            };
            match compositor.composite_mixed_layers_to_target(&mixed, &target, comp_w, comp_h) {
                Ok(()) => {
                    // Release the borrows BEFORE the push: `push_with_lookahead`
                    // can park on ring backpressure for up to 100ms, and a
                    // parked producer holding N HOLD locks would stall every
                    // decode thread behind it.
                    drop(mixed);
                    drop(cpu_layers);
                    #[cfg(all(windows, feature = "hwdecode"))]
                    drop(holds);
                    // PLAY-02/D-07: the composited frame stays on the GPU when
                    // the sink can present a texture. When it cannot — the two
                    // shell `PresentSink` adapters are frozen this phase (D-01)
                    // — read back the SAME target and push the CPU payload.
                    // That readback is byte-identical to the old
                    // `composite_layers_to_rgba` (57-04's
                    // `mixed_entry_point_with_cpu_layers_matches_composite_layers_to_rgba`),
                    // so a consumer on this route cannot tell the refactor
                    // happened — which is precisely what `crates/app-core`'s
                    // untouched parity twins prove.
                    let payload = if composited_present {
                        RingPayload::Composited(CompositedEntry {
                            target,
                            w: comp_w,
                            h: comp_h,
                        })
                    } else {
                        match compositor.blit_texture_to_rgba(target.view(), comp_w, comp_h) {
                            Ok(rgba) => RingPayload::Cpu(engine::Frame {
                                width: comp_w,
                                height: comp_h,
                                rgba,
                            }),
                            Err(e) => {
                                eprintln!("preview_ring: composite readback failed: {e}");
                                prod_pos += step;
                                continue;
                            }
                        }
                    };
                    let produce_us = produce_started.elapsed().as_micros() as u64;
                    last_produce_us.store(produce_us, Ordering::Relaxed);
                    // ---- PLAY-05: score THIS tick against the frame budget.
                    // `produce_us` MINUS the target-pool wait: a producer that
                    // is keeping up spends most of every tick parked waiting for
                    // the presenter, so the raw duration converges on one frame
                    // step no matter how much headroom the machine has. Feeding
                    // that in would sit the controller permanently on its own
                    // threshold and make the degradation fire on the fixtures
                    // that already sustain — the exact failure wave-0 finding F1
                    // warns against ("any plan claiming 'we made 3-layer 720p
                    // real-time' would be claiming credit for the starting
                    // state"; degrading it would be worse still).
                    let effective_us = produce_us.saturating_sub(backpressure_us);
                    // ---- Phase 59 (plan 59-06, D-11/D-12/D-13): the SAME
                    // comparison, attributed to the SEGMENT this tick belongs
                    // to, from the SAME call site — a second accumulator, never
                    // a second controller (the per-SESSION vs per-SEGMENT
                    // lifetime split). Reached ONLY on the live path: a
                    // cache-served tick returned from this arm long before
                    // `produce_started`, which is D-13's gate expressed as
                    // control flow rather than as a flag anybody has to
                    // remember to set.
                    let seg_index = crate::render_cache_lookup::segment_index_for(prod_pos);
                    crate::render_cache_detect::note_live_tick(
                        seg_index,
                        effective_us,
                        step.max(1) as u64,
                    );
                    // D-12's static pre-arm. Cheap by construction — the
                    // predicate returns before touching the registry for any
                    // stack at or below the hardware cap, which is every stack
                    // the published measurements say plays fine.
                    crate::render_cache_detect::prearm_stack(seg_index, stack.layers.len());
                    let next = dynres.on_tick(effective_us, step.max(1) as u64);
                    // ---- Phase 60 (plan 60-04, DROP-01): ORDERED ESCALATION,
                    // expressed as ONE boolean at ONE call site.
                    //
                    // The same signal, the same site, the same tick. The third
                    // argument is the whole of the precedence rule: frame
                    // dropping counts a miss only once the resolution ladder has
                    // no rung left to spend, so every overload that ladder alone
                    // can absorb behaves byte-identically to how it behaved
                    // before this axis existed.
                    //
                    // "At the floor" is DERIVED from the ladder (`lower()`
                    // answering `None`) rather than compared against a named
                    // variant. That is deliberate: the floor has already moved
                    // once (2026-08-03, `Quarter` -> `Half`, on owner UAT), and
                    // a hardcoded variant here would have silently become a
                    // different rule that day instead of following the ladder it
                    // is gating on.
                    let next_drop = framedrop.on_tick(
                        effective_us,
                        step.max(1) as u64,
                        next.lower().is_none(),
                    );
                    if next_drop != drop_mode {
                        eprintln!(
                            "preview_ring: frame drop {drop_mode:?} -> {next_drop:?} \
                             (produce {effective_us}us vs budget {step}us, \
                             resolution {next:?} at ladder floor)"
                        );
                        drop_mode = next_drop;
                    }
                    if next != res_level {
                        // Quick task 260825-mgq: the counter and the line
                        // below it are the SAME event, so a run can prove
                        // engagement (or, as legitimately, dormancy) without
                        // parsing stderr. See `DYNRES_TRANSITIONS`.
                        DYNRES_TRANSITIONS.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "preview_ring: playback resolution {res_level:?} -> {next:?} \
                             (produce {effective_us}us vs budget {step}us)"
                        );
                        res_level = next;
                        diag.set_playback_res_level(res_level);
                    }
                    if !push_with_lookahead(
                        &ctl,
                        RingEntry {
                            payload,
                            // SEEK-02 deliberately NOT applied here: pool
                            // frames are already fps-conformed to the project
                            // grid by ExportRunDecoder's fps= filter, so this
                            // prod_pos grid IS the correct stamp for an
                            // N-clip composite (49-RESEARCH Pitfall 3) — do
                            // not "fix" this to the pts_map path.
                            timeline_us: prod_pos,
                            gen: my_gen,
                        },
                        host.as_ref(),
                        prod_pos,
                        is_source,
                        Some(&stack),
                        &cur_clip,
                        &mut pool,
                        &mut la,
                    ) {
                        continue; // stale/stopped — the handshake picks up next loop
                    }
                    // Debug session `multitrack-preview-lag-choppy`
                    // (2026-08-01): probe EVERY produced frame while inside a
                    // multi range, at the JUST-PRODUCED position (the same
                    // pre-increment position the parked-path probe sees). The
                    // park-only trigger structurally never fires here — multi
                    // production hovers near RTF 1, so the ring rarely fills
                    // — which left every in-section clip boundary cold
                    // (measured 393-823ms production stall per cut, a freeze
                    // + catch-up skip on screen). The probe self-latches
                    // one-prewarm-per-boundary, and its new borrow-only
                    // hit-set fast path makes the no-boundary tick cost a set
                    // compare, not a Project clone. Per-frame cadence is
                    // REQUIRED for correctness, not just latency: the first
                    // probe that sees the set change fixes the prewarm source
                    // within one frame of the boundary, which is what lets
                    // the adopt rebase stay inside the accepted ≤1-frame
                    // offset.
                    lookahead_probe(
                        host.as_ref(),
                        prod_pos,
                        Some(&stack),
                        &cur_clip,
                        &mut pool,
                        &mut la,
                    );
                    // Phase 57 (57-06): the hardware twin of the probe's pool
                    // prewarm. It rides the probe's OWN latch (the probe sets
                    // `la.prewarmed_for` when — and only when — it detects a
                    // boundary inside the horizon), so this coordinator does
                    // not add a second latch mechanism; it consumes the one
                    // that already decided a boundary is coming.
                    #[cfg(all(windows, feature = "hwdecode"))]
                    if let (Some(s), Some(boundary)) = (sessions.as_mut(), la.prewarmed_for) {
                        if let Some(next) =
                            crate::resolve_multilayer(host.as_ref(), boundary)
                        {
                            s.prewarm_for_boundary(&next, boundary, &hw_failed, &HW_LATCH);
                        }
                    }
                    prod_pos += step;
                }
                Err(e) => {
                    // A failed composite skips THIS frame (push nothing,
                    // advance) — never panic the producer thread.
                    eprintln!("preview_ring: multi-layer composite failed: {e}");
                    prod_pos += step;
                }
            }
            continue;
        }
        // Not (or no longer) inside a multi range: dropping the pool reaps
        // every per-layer ffmpeg child (retain/Drop — the SC-4 discipline).
        if pool.is_some() {
            pool = None;
        }
        // 59-06: …and the render-cache twin. Nothing single-layer is cached this
        // phase (`resolve_multilayer` answers `None` for the degenerate case, so
        // the serve arm is unreachable here), and an idle `ffmpeg` child holding
        // a segment open across a single-layer range is pure cost. `retire` is
        // two `Option` stores when there is nothing open.
        cache_serve.retire();
        // 59-13: the pool this producer was told to release just before a cache
        // exit has been released by the line above, for a different reason. The
        // remembered instruction has nothing left to act on.
        cache_exit_release_at = None;
        // …and the hardware twin. Phase 57 (57-06): this is the OTHER side of
        // the overlap boundary, and it is deliberately the ASYNCHRONOUS retire
        // (stop flags set, reaped on later calls) rather than a wait. Wave-0
        // finding F2 measured a 1033 ms overlap EXIT on the 4K fixture — a
        // synchronous stop-everything at this exact point is how that number
        // gets bigger, not smaller. (The measured 1033 ms itself is NOT here:
        // `crates/engine/tests/boundary_exit_cost_probe.rs` locates it in
        // `LayerDecoderPool::advance`'s `FramePull::Ended` cold-decode arm.)
        #[cfg(all(windows, feature = "hwdecode"))]
        if let Some(s) = sessions.as_mut() {
            s.retire_live();
        }

        match crate::resolve_active(host.as_ref(), is_source, prod_pos) {
            None => {
                // Gap / nothing loaded: emit a tiny black entry at cadence. The
                // condvar backpressure paces this (no sleep) — a run of gap frames
                // fills to the depth cap and then blocks (where the lookahead
                // probe pre-starts the clip the gap runs into, 18.3-05).
                if !push_with_lookahead(
                    &ctl,
                    RingEntry {
                        payload: RingPayload::Cpu(black_gap_frame()),
                        // SEEK-02 not applicable: synthetic black gap frames
                        // have no source PTS by definition — the grid stays.
                        timeline_us: prod_pos,
                        gen: my_gen,
                    },
                    host.as_ref(),
                    prod_pos,
                    is_source,
                    None,
                    &cur_clip,
                    &mut pool,
                    &mut la,
                ) {
                    continue; // stale/stopped — the handshake picks up next loop
                }
                prod_pos += frame_step;
            }
            Some(r) => {
                // WR-03-style backoff: don't re-spawn ffprobe/ffmpeg every loop
                // for a clip whose session already failed to start.
                if failed_clip.as_ref() == Some(&r.clip_id) {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                if session.is_none() || cur_clip != r.clip_id {
                    // ---- Phase 48 (plan 48-09): the per-media decode-path
                    // gate (GPU-05), at the EXACT site the CPU decoder session
                    // is chosen. Armed only when a live VRAM budget handle
                    // exists (the real app); every test route stays CPU-only.
                    // On delegation the clip runs GPU-resident on the
                    // dedicated "rudis-gpu-decode" thread; on Fallback the
                    // UNCHANGED CPU start below serves the clip.
                    #[cfg(all(windows, feature = "hwdecode"))]
                    if let Some(budget) = gpu_budget.as_ref() {
                        // Debug session `multitrack-preview-lag-choppy`
                        // (2026-08-01): bound the delegation at the first
                        // position ANOTHER video-track clip becomes active
                        // (see `next_video_layer_join`) so the producer
                        // resumes orchestration — and the multi-layer arm can
                        // engage — exactly where the layer set changes.
                        // Source mode previews one MediaBin item: no timeline,
                        // no cap. One store lock per delegation (clip start),
                        // never per frame.
                        let delegation_cap = if is_source {
                            None
                        } else {
                            host.store().and_then(|guard| {
                                next_video_layer_join(
                                    guard.timeline(),
                                    r.clip_id.as_deref(),
                                    prod_pos,
                                    prod_pos + clip_remaining_timeline_us(&r),
                                )
                            })
                        };
                        match delegate_gpu_clip(
                            &ctl,
                            &compositor,
                            budget,
                            &gpu_ledger,
                            &r,
                            prod_pos,
                            my_gen,
                            &mut hw_failed,
                            &mut warm_hw,
                            delegation_cap,
                            host.as_ref(),
                            &mut la,
                            // Phase 57 § D-3: the coordinator goes IN with the
                            // producer, so the delegation wait can warm the
                            // upcoming overlap's hardware sessions through the
                            // very same `prewarm_for_boundary` the in-section
                            // boundary uses. `None` on every route with no GPU
                            // budget, where the set does not exist and every
                            // layer serves from the software pool anyway.
                            sessions.as_mut(),
                            is_source,
                        ) {
                            GpuDelegation::Resume { next_pos } => {
                                // A CPU prewarm made for this clip is
                                // superseded (dropped -> child reaped).
                                if la
                                    .pending_session
                                    .as_ref()
                                    .is_some_and(|p| p.clip_id == r.clip_id)
                                {
                                    la.pending_session = None;
                                }
                                prod_pos = next_pos;
                                session = None;
                                cur_clip = None;
                                misses = 0;
                                continue;
                            }
                            GpuDelegation::Interrupted => {
                                // stop/flush — the loop-top handshake owns it.
                                session = None;
                                cur_clip = None;
                                misses = 0;
                                continue;
                            }
                            GpuDelegation::Fallback => {
                                // fall through to the CPU start, unchanged
                            }
                        }
                    }
                    // 18.3-05: consume the prewarmed pending session when it is
                    // for exactly this clip AND decodes from within one frame of
                    // the demanded source position (the pre-start was probed at
                    // `future` — a sub-frame past the boundary when parks fire
                    // per produced frame, so ffmpeg's accurate seek landed on
                    // the same first frame). Right clip at the wrong position
                    // (a restart elsewhere in the clip) can never be served by
                    // this prewarm — drop it (child reaped) and start cold; a
                    // pending for a DIFFERENT upcoming clip stays latched for
                    // its own boundary.
                    let step = engine::frame_step_us(r.fps).max(1);
                    let started = match la.pending_session.take() {
                        Some(p)
                            if p.clip_id == r.clip_id
                                && (p.source_us - r.source_us).abs() < step =>
                        {
                            // warmed: channels (frames + pts) already filling
                            Ok(SwSession::from_pending(p, prod_pos))
                        }
                        Some(p) if p.clip_id == r.clip_id => {
                            drop(p); // mismatched position — reap, start cold
                            // D-05: the source ALWAYS comes through the
                            // resolver seam, never `r.path` directly — Phase 58
                            // (D-27) substitutes a playback proxy here. Cold
                            // start #1 of the single-clip CPU arm; the warm
                            // ADOPT arm above deliberately has no resolve of
                            // its own, because it inherits the prewarm's
                            // already-resolved session.
                            let src = crate::decode_source::resolve_decode_source(
                                r.clip_id.as_deref().unwrap_or(""),
                                &r.path,
                                r.source_us,
                            );
                            SwSession::start(
                                &src.path,
                                src.source_us,
                                r.rotation,
                                prod_pos,
                                r.retime.clone(),
                            )
                        }
                        other => {
                            la.pending_session = other; // not ours — keep latched
                            // D-05 again (cold start #2, no prewarm to adopt):
                            // same seam, same reason — Phase 58 D-27.
                            let src = crate::decode_source::resolve_decode_source(
                                r.clip_id.as_deref().unwrap_or(""),
                                &r.path,
                                r.source_us,
                            );
                            SwSession::start(
                                &src.path,
                                src.source_us,
                                r.rotation,
                                prod_pos,
                                r.retime.clone(),
                            )
                        }
                    };
                    match started {
                        Ok(s) => {
                            session = Some(s);
                            cur_clip = r.clip_id.clone();
                            frame_step = engine::frame_step_us(r.fps).max(1);
                            // 18.3-03 Issue-A diagnostic (env-gated, OFF by
                            // default): log the derived frame_step + media fps at
                            // each session start so a live run can rule out H1 (a
                            // wrong frame_step for 60fps media). Real 60fps →
                            // frame_step 16667 (≈ true 16666.67µs).
                            if std::env::var("RUDIS_PREVIEW_PACE_LOG").is_ok() {
                                eprintln!(
                                    "[pace-prod] media_fps={} frame_step_us={} prod_pos={}",
                                    r.fps, frame_step, prod_pos
                                );
                            }
                            // The clip is exhausted once prod_pos reaches its end
                            // (mapped from the audio/out span); floor a one-frame
                            // window so a zero-length span still yields a frame.
                            session_end_pos =
                                prod_pos + clip_remaining_timeline_us(&r).max(frame_step);
                            misses = 0;
                            failed_clip = None;
                        }
                        Err(_) => {
                            failed_clip = Some(r.clip_id.clone());
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                    }
                }
                if prod_pos >= session_end_pos {
                    // Clip exhausted: re-resolve the next clip / gap at prod_pos.
                    // 18.3-03 Issue-A: SNAP prod_pos back to the exact clip-end
                    // (`session_end_pos`) before re-resolving. prod_pos advances in
                    // whole `frame_step`s, so it overshoots the true boundary by
                    // up to one frame; without the snap that overshoot ACCUMULATES
                    // across every clip→clip cut, so the next clip's frame stamps
                    // drift a fraction ahead of the presenter's audio span.start
                    // (which anchors to the exact timeline boundary) — a slow A/V
                    // divergence over a multi-clip timeline. Snapping keeps every
                    // clip's stamps on the timeline grid the audio clock uses.
                    prod_pos = session_end_pos;
                    session = None;
                    cur_clip = None;
                    continue;
                }
                // Longer producer pull timeout (design §2): the producer never
                // blocks the presenter, so it can afford 250ms — fewer
                // wasted-timeout iterations during warm-up.
                let pulled = session
                    .as_ref()
                    .and_then(|s| s.session.try_next_frame(Duration::from_millis(250)));
                match pulled {
                    Some(frame) => {
                        misses = 0;
                        // SEEK-02 (Phase 49): stamp the pulled frame from its
                        // REAL PTS (paired BY INDEX with the start_with_pts
                        // side channel) through the SAME shared helper the
                        // hardware path uses; prod_pos stays the production
                        // clock / end_pos cursor and the fallback grid.
                        let stamped = match session.as_mut() {
                            Some(sw) => sw.stamp_pulled(prod_pos),
                            None => prod_pos, // unreachable: a pull implies a session
                        };
                        if !push_with_lookahead(
                            &ctl,
                            RingEntry {
                                payload: RingPayload::Cpu(frame),
                                timeline_us: stamped,
                                gen: my_gen,
                            },
                            host.as_ref(),
                            prod_pos,
                            is_source,
                            None,
                            &cur_clip,
                            &mut pool,
                            &mut la,
                        ) {
                            continue; // stale/stopped — re-loop; handshake picks up
                        }
                        // Single-layer misses NEVER advance prod_pos (below): a
                        // StreamingDecodeSession frame is positional-by-ORDER
                        // (unlike the pool's Timedout-lockstep), so advancing on a
                        // miss would desync the stamp from the pixels. Advance the
                        // production clock ONLY on a real pushed frame.
                        prod_pos += frame_step;
                    }
                    None => {
                        // None = timeout OR EOF (try_next_frame cannot tell them
                        // apart). Near the clip end an early EOF (fps rounding vs
                        // -t) is indistinguishable from a stall — treat >= 4
                        // consecutive misses within one frame of the end as ended
                        // (mirror of the pool's disclosed Ended residual). A
                        // mid-clip stall just retries: the frame arrives on a
                        // later pull, HELD upstream by the presenter.
                        misses += 1;
                        if misses >= 4 && prod_pos + frame_step >= session_end_pos {
                            session = None;
                            cur_clip = None;
                        }
                        // NEVER advance prod_pos on a miss.
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Debug session `multitrack-preview-lag-choppy` (2026-08-01): the
    /// delegation-cap scan — the earliest OTHER video-track clip start
    /// strictly inside the delegated window, so a mid-clip layer join clamps
    /// the GPU delegation and the multi arm engages at the boundary.
    #[cfg(all(windows, feature = "hwdecode"))]
    mod delegation_cap {
        use super::super::next_video_layer_join;
        use rudis_core::{Clip, Timeline, Track, TrackKind};

        fn clip(id: &str, start_us: i64, out_us: i64) -> Clip {
            Clip {
                id: id.into(),
                media_id: "m".into(),
                start_us,
                in_us: 0,
                out_us,
                volume: 1.0,
                audio_detached: false,
                transform: rudis_core::ClipTransform::default(),
                opacity: 1.0,
                crop: rudis_core::ClipCrop::default(),
                keyframes: Default::default(),
                text: None,
                alpha_mode: Default::default(),
                retime: None,
            }
        }

        fn timeline(tracks: Vec<(TrackKind, Vec<Clip>)>) -> Timeline {
            Timeline {
                tracks: tracks
                    .into_iter()
                    .map(|(kind, clips)| Track { kind, clips })
                    .collect(),
            }
        }

        /// The owner's repro shape: a long base clip delegated from 0 while an
        /// upper-track clip joins at 4s — the cap must be exactly that start.
        #[test]
        fn upper_track_join_mid_clip_caps_the_delegation() {
            let tl = timeline(vec![
                (TrackKind::Video, vec![clip("top", 6_000_000, 4_000_000)]),
                (TrackKind::Video, vec![clip("mid", 4_000_000, 6_000_000)]),
                (TrackKind::Video, vec![clip("base", 0, 20_000_000)]),
                (TrackKind::Audio, vec![]),
            ]);
            assert_eq!(
                next_video_layer_join(&tl, Some("base"), 0, 20_000_000),
                Some(4_000_000),
                "earliest OTHER video clip start inside the window caps the range"
            );
            // Resuming INSIDE the overlap section (multi arm exited at 10s,
            // base re-delegates): no further joins — no cap.
            assert_eq!(next_video_layer_join(&tl, Some("base"), 10_000_000, 20_000_000), None);
        }

        /// The delegated clip itself and clips outside `(after, horizon)` never
        /// cap; starts exactly AT the horizon (an adjacent cut) never cap; a
        /// clip already active at `after` never caps (it cannot re-join).
        #[test]
        fn cap_scan_boundary_conditions() {
            let tl = timeline(vec![
                (TrackKind::Video, vec![clip("next", 8_000_000, 2_000_000)]),
                (TrackKind::Video, vec![clip("base", 0, 8_000_000)]),
                (TrackKind::Audio, vec![clip("aud", 1_000_000, 5_000_000)]),
            ]);
            // "next" starts exactly at the horizon (base's end): ordinary cut,
            // no cap. The audio-track clip never counts.
            assert_eq!(next_video_layer_join(&tl, Some("base"), 0, 8_000_000), None);
            // Same scan mid-clip: still nothing strictly inside the window.
            assert_eq!(next_video_layer_join(&tl, Some("base"), 3_000_000, 8_000_000), None);
            // Delegating "next" from its own start: a start AT `after` is
            // already-active (start-inclusive) — never a JOIN — and nothing
            // else starts inside the window.
            assert_eq!(
                next_video_layer_join(&tl, Some("next"), 8_000_000, 10_000_000),
                None,
                "the clip starting AT `after` is active there, not joining later"
            );
        }
    }

    /// A tiny 2x2 black RGBA frame (16 bytes) — small enough that the byte budget
    /// clamps depth to the cap (32), so the backpressure test fills fast.
    fn black_2x2() -> engine::Frame {
        engine::Frame {
            width: 2,
            height: 2,
            rgba: vec![
                0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255,
            ],
        }
    }

    fn entry(timeline_us: i64, gen: u64) -> RingEntry {
        RingEntry {
            payload: RingPayload::Cpu(black_2x2()),
            timeline_us,
            gen,
        }
    }

    /// design §1 depth table: byte-budget → depth (clamped 8..32).
    #[test]
    fn ring_depth_matches_design_table() {
        assert_eq!(ring_depth(8_294_400), 19, "1080p RGBA -> 19 (~0.63s runway)");
        assert_eq!(ring_depth(3_686_400), 32, "720p -> 32 (cap)");
        assert_eq!(ring_depth(33_177_600), 8, "4K single-layer -> 8 (floor)");
        // Zero-byte frame must not divide-by-zero (max(1) guard -> floor).
        assert_eq!(ring_depth(0), RING_MAX_DEPTH, "0 bytes clamps to the cap");
    }

    /// design §3/§6: catch-up drops stale-time frames keeping the newest <=
    /// target; a future frame HOLDs (never popped); stale-gen entries are
    /// discarded on sight.
    #[test]
    fn pop_for_target_catch_up_hold_and_stale_gen() {
        let ctl = RingCtl::new(); // gen == 1
        ctl.gen.store(1, Ordering::SeqCst);
        assert!(ctl.push_blocking(entry(0, 1)));
        assert!(ctl.push_blocking(entry(33_333, 1)));
        assert!(ctl.push_blocking(entry(66_666, 1)));

        // target 40_000: keep 33_333 (newest <= target), drop 0 (catch-up),
        // leave 66_666 (future).
        let kept = ctl.pop_for_target(1, 40_000).expect("a due frame");
        assert_eq!(kept.timeline_us, 33_333);
        assert_eq!(ctl.len(), 1, "the future 66_666 entry remains");

        // target 10_000 against the remaining {66_666}: None (HOLD — never pop a
        // future frame).
        assert!(
            ctl.pop_for_target(1, 10_000).is_none(),
            "a future-only queue must HOLD"
        );
        assert_eq!(ctl.len(), 1, "HOLD leaves the future entry queued");

        // A stale-gen entry is discarded regardless of timestamp.
        ctl.push_blocking(entry(0, 99)); // gen 99 != cur — but push checks gen!
        // push_blocking rejects a gen != current at enqueue; force one in
        // directly to exercise pop's stale-gen discard.
        ctl.lock_q().push_front(entry(1_000, 7));
        assert!(
            ctl.pop_for_target(1, i64::MAX).is_some(),
            "stale-gen front discarded, real due frame returned"
        );
    }

    /// design §4/§6: flush restamps the gen, empties the queue, records
    /// restart_pos, and a still-in-flight stale-gen push returns false.
    #[test]
    fn flush_clears_restamps_and_rejects_stale_push() {
        let ctl = RingCtl::new(); // gen == 1
        assert!(ctl.push_blocking(entry(0, 1)));
        assert!(ctl.push_blocking(entry(33_333, 1)));
        assert_eq!(ctl.gen.load(Ordering::SeqCst), 1);

        ctl.flush(500_000);
        assert_eq!(ctl.gen.load(Ordering::SeqCst), 2, "gen bumped");
        assert_eq!(ctl.len(), 0, "queue emptied");
        assert_eq!(
            ctl.restart_pos.load(Ordering::SeqCst),
            500_000,
            "restart_pos recorded"
        );

        // An entry still stamped the OLD gen (a producer mid-flight) is stale.
        assert!(
            !ctl.push_blocking(entry(600_000, 1)),
            "a gen-1 push after a flush to gen 2 is stale — rejected"
        );
        assert_eq!(ctl.len(), 0, "stale entry never enqueued");
    }

    /// 18.3-05 (Stage 3): `try_push` mirrors `push_blocking`'s stop/stale-gen
    /// checks exactly, but a FULL ring hands the entry BACK as WouldBlock
    /// instead of parking — the producer's hook for running the lookahead
    /// prewarm probe during its idle backpressure time.
    #[test]
    fn try_push_wouldblock_on_full_mirrors_push_semantics() {
        let ctl = RingCtl::new(); // gen == 1
        assert!(
            matches!(ctl.try_push(entry(0, 1)), Ok(true)),
            "a non-full ring enqueues (Ok(true))"
        );
        assert_eq!(ctl.len(), 1);

        // Stale gen: abandoned (Ok(false)), never enqueued.
        assert!(
            matches!(ctl.try_push(entry(0, 99)), Ok(false)),
            "a stale-gen entry is abandoned, exactly like push_blocking"
        );
        assert_eq!(ctl.len(), 1, "the stale entry was not enqueued");

        // Fill to the 2x2 depth cap (32): the next try_push hands the SAME
        // entry back as WouldBlock instead of parking.
        for i in 1..32 {
            assert!(matches!(ctl.try_push(entry(i as i64 * 33_333, 1)), Ok(true)));
        }
        assert_eq!(ctl.len(), 32, "ring at the depth cap");
        match ctl.try_push(entry(99 * 33_333, 1)) {
            Err(e) => assert_eq!(
                e.timeline_us,
                99 * 33_333,
                "WouldBlock hands the same entry back for the blocking retry"
            ),
            Ok(_) => panic!("a full ring must WouldBlock, never enqueue/abandon"),
        }
        assert_eq!(ctl.len(), 32, "nothing enqueued on WouldBlock");

        // Stop: abandoned (Ok(false)) even though the queue check would block.
        ctl.request_stop();
        assert!(
            matches!(ctl.try_push(entry(0, 1)), Ok(false)),
            "a stopped ring abandons the entry, exactly like push_blocking"
        );
    }

    /// design §2/§6: byte-budget backpressure. A full ring blocks the pusher
    /// (zero-CPU) until a pop opens a slot; a flush or a stop each unblock it
    /// with `false` (its gen went stale / the ring stopped).
    #[test]
    fn backpressure_blocks_until_pop_flush_or_stop() {
        // --- variant A: pop unblocks with true ---
        let ctl = RingCtl::new();
        // 2x2 frames (16 bytes) -> depth == cap 32. Fill to capacity.
        for i in 0..32 {
            assert!(ctl.push_blocking(entry(i as i64 * 33_333, 1)));
        }
        assert_eq!(ctl.len(), 32, "ring at the depth cap");

        let (tx, rx) = mpsc::channel::<bool>();
        let ctl_t = ctl.clone();
        let pusher = std::thread::spawn(move || {
            // The 33rd push must BLOCK until a slot opens.
            let ok = ctl_t.push_blocking(entry(33 * 33_333, 1));
            let _ = tx.send(ok);
        });
        // It must NOT complete while the ring stays full.
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "pusher must block while the ring is full"
        );
        // One pop opens a slot -> the parked pusher completes with true.
        assert!(ctl.pop_for_target(1, i64::MAX / 2).is_some());
        let done = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("pusher unblocks within 2s after a pop");
        assert!(done, "a pop-freed slot lets the push enqueue (true)");
        pusher.join().unwrap();

        // --- variant B: flush unblocks with false ---
        let ctl = RingCtl::new();
        for i in 0..32 {
            assert!(ctl.push_blocking(entry(i as i64 * 33_333, 1)));
        }
        let (tx, rx) = mpsc::channel::<bool>();
        let ctl_t = ctl.clone();
        let pusher = std::thread::spawn(move || {
            let ok = ctl_t.push_blocking(entry(99 * 33_333, 1));
            let _ = tx.send(ok);
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        ctl.flush(1_000_000); // bumps gen -> the parked gen-1 push is stale
        let done = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("pusher unblocks within 2s after a flush");
        assert!(!done, "a flush makes the parked push stale (false)");
        pusher.join().unwrap();

        // --- variant C: stop unblocks with false ---
        let ctl = RingCtl::new();
        for i in 0..32 {
            assert!(ctl.push_blocking(entry(i as i64 * 33_333, 1)));
        }
        let (tx, rx) = mpsc::channel::<bool>();
        let ctl_t = ctl.clone();
        let pusher = std::thread::spawn(move || {
            let ok = ctl_t.push_blocking(entry(99 * 33_333, 1));
            let _ = tx.send(ok);
        });
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        ctl.request_stop();
        let done = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("pusher unblocks within 2s after a stop");
        assert!(!done, "a stop makes the parked push return false");
        pusher.join().unwrap();
    }

    /// Phase 48 (plan 48-09): the GPU depth-capacity transitions on the ONE
    /// shared ring. `enter_gpu_depth` caps pushes by COUNT, `evict_oldest_to`
    /// drops the oldest entries (the budget-shrink reaction — D-12's "keep
    /// playing degraded"), and `exit_gpu_depth` restores the CPU byte budget
    /// byte-identically (the same 2x2 frames clamp to the 32 cap again).
    #[test]
    fn gpu_depth_capacity_transitions_and_eviction() {
        let ctl = RingCtl::new(); // Bytes mode — the CPU default
        for i in 0..4 {
            assert!(ctl.push_blocking(entry(i as i64 * 33_333, 1)));
        }

        // Enter GPU mode at depth 5: exactly ONE more push fits.
        ctl.enter_gpu_depth(5);
        assert!(matches!(ctl.try_push(entry(4 * 33_333, 1)), Ok(true)));
        match ctl.try_push(entry(5 * 33_333, 1)) {
            Err(e) => assert_eq!(e.timeline_us, 5 * 33_333, "depth 5 must WouldBlock the 6th"),
            Ok(_) => panic!("a full Depth(5) ring must WouldBlock"),
        }
        assert_eq!(ctl.len(), 5);

        // Budget shrink: cap down + evict oldest down to the new depth. The
        // two SURVIVORS are the NEWEST entries (oldest were dropped).
        ctl.set_gpu_depth(2);
        assert_eq!(ctl.evict_oldest_to(2), 3, "5 entries down to 2 evicts 3");
        assert_eq!(ctl.len(), 2);
        let kept = ctl
            .pop_for_target(1, i64::MAX / 2)
            .expect("survivors still pop");
        assert_eq!(
            kept.timeline_us,
            4 * 33_333,
            "catch-up keeps the newest survivor — eviction dropped the OLDEST entries"
        );

        // Exit: the CPU byte budget is back (2x2 frames clamp to the 32 cap).
        ctl.exit_gpu_depth();
        for i in 0..32 {
            assert!(
                matches!(ctl.try_push(entry(100 + i as i64, 1)), Ok(true)),
                "Bytes mode restored: 2x2 frames fill to the 32-cap again (i={i})"
            );
        }
        assert!(
            ctl.try_push(entry(999, 1)).is_err(),
            "the restored byte budget still caps at ring_depth()"
        );
    }

    /// Phase 48 (plan 48-09): the per-media decode-path gate (GPU-05), unit
    /// level. Both paths below are GPU-free when green: the kill-switch
    /// returns before any libav/D3D11 work, and an engaged latch skips the
    /// attempt entirely — on THIS machine an ATTEMPTED open would succeed
    /// (Hardware), so a Software answer proves the skip actually happened.
    #[cfg(all(windows, feature = "hwdecode"))]
    mod decode_path_gate {
        use super::super::{choose_decode_path, DecodePath};
        use std::sync::Mutex;

        /// Serializes the env-mutating test against its sibling (the same
        /// binary runs tests in parallel; nothing else here reads the env).
        static ENV_SERIAL: Mutex<()> = Mutex::new(());

        fn fixture() -> std::path::PathBuf {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../test-media/bars_720p30_5s.mp4")
        }

        #[test]
        fn kill_switch_routes_to_software_and_never_feeds_the_latch() {
            let _g = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
            let latch = engine::HwFailureLatch::new();
            std::env::set_var(engine::KILL_SWITCH_ENV, "1");
            let decided = choose_decode_path(&fixture(), 19, &latch);
            std::env::remove_var(engine::KILL_SWITCH_ENV);
            match decided {
                DecodePath::Software { cache_media } => assert!(
                    !cache_media,
                    "the kill-switch is a cheap env check — clearing it must take effect \
                     on the next clip start, so the media is NOT cached as failed"
                ),
                DecodePath::Hardware(_) => {
                    panic!("hardware opened despite the kill-switch")
                }
            }
            assert_eq!(
                latch.failures(),
                0,
                "Disabled must not count toward the session latch (D-15)"
            );
        }

        #[test]
        fn engaged_latch_skips_hardware_attempts_session_wide() {
            let _g = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
            let latch = engine::HwFailureLatch::new();
            for i in 0..engine::LATCH_THRESHOLD {
                assert!(
                    latch.record(&engine::HwOpenError::InitFailed(format!("synthetic {i}"))),
                    "InitFailed must count"
                );
            }
            assert!(latch.engaged(), "{} InitFailed engage", engine::LATCH_THRESHOLD);
            // On this machine an ATTEMPTED open succeeds — Software proves
            // the engaged latch genuinely skipped the attempt.
            assert!(
                matches!(
                    choose_decode_path(&fixture(), 19, &latch),
                    DecodePath::Software { cache_media: false }
                ),
                "an engaged latch must skip hardware without caching the media"
            );
        }
    }

    /// 59-20 (§ D-43): the exit prewarm's hardware prediction must be honest
    /// about the ARM it is running on.
    ///
    /// The defect these pin: across a fully cached span the producer never runs
    /// `sync_to_stack`, so at the exit-discovery tick BOTH of the predictor's
    /// inputs (`live_ids`, `hw_failed`) are empty — on every arm, including one
    /// where hardware will claim nothing. The unguarded selector then named the
    /// top [`super::super::layer_sessions::MAX_HW_SESSIONS`] clips in track
    /// order, those layers were dropped from the warm-up, and the boundary
    /// built them cold: a 17x cache-exit regression (44.09 -> 759.99 ms) with
    /// warm `sw_sidecar_spawns` +3 per run.
    ///
    /// Everything here exercises the PURE inner
    /// ([`super::super::predicted_hw_claim_inner`]) with `arm_can_claim` passed
    /// explicitly. **No test may engage the process-global `HW_LATCH`** — its
    /// engagement is permanent for the process lifetime by design, so a single
    /// test doing so would silently change what every later test in this binary
    /// measures. The latch-coupling pin below therefore uses a LOCAL
    /// [`engine::HwFailureLatch`], the same discipline `decode_path_gate` above
    /// already follows.
    #[cfg(all(windows, feature = "hwdecode"))]
    mod hw_claim_prediction {
        use super::super::predicted_hw_claim_inner;
        use crate::layer_sessions::{select_hw_clip_ids, MAX_HW_SESSIONS};
        use crate::{LayerSpec, MultiLayerStack};
        use engine::{LayerCrop, LayerTransform};
        use std::collections::HashSet;
        use std::path::PathBuf;

        /// `layer_sessions.rs`'s own fixture idiom, so these pins and the
        /// selector's in-file tests describe the same kind of layer.
        fn spec(clip_id: &str, path: &str) -> LayerSpec {
            LayerSpec {
                clip_id: clip_id.to_string(),
                path: PathBuf::from(path),
                source_us: 0,
                rotation: 0,
                remaining_dur_us: 5_000_000,
                src_step_us: 33_333,
                retime_ramped: false,
                opacity: 1.0,
                transform: LayerTransform {
                    position: (0.0, 0.0),
                    scale: (1.0, 1.0),
                    rotation_deg: 0.0,
                },
                crop: LayerCrop::default(),
                alpha_mode: engine::AlphaMode::Straight,
                is_image_sequence: false,
                seq_fps: 0.0,
                is_still_image: false,
                src_width: 1280,
                src_height: 720,
                // Synthetic spec: no probed source, so no alpha knowledge.
                reports_alpha: rudis_core::SourceAlpha::Unknown,
                text: None,
            }
        }

        /// The committed benchmark fixture's shape: six hardware-ELIGIBLE
        /// layers, so an ungated prediction has something to name and the
        /// EMPTY assertions below are non-vacuous.
        fn stack6() -> MultiLayerStack {
            MultiLayerStack {
                layers: vec![
                    spec("l0", "a.mp4"),
                    spec("l1", "b.mp4"),
                    spec("l2", "c.mp4"),
                    spec("l3", "d.mp4"),
                    spec("l4", "e.mp4"),
                    spec("l5", "f.mp4"),
                ],
                width: 1920,
                height: 1080,
                fps: 30.0,
            }
        }

        /// The empty pair the exit-discovery tick really hands in across a
        /// fully cached span — on BOTH arms. That is the whole point: these two
        /// sets cannot distinguish them.
        fn empty_inputs() -> (HashSet<String>, HashSet<PathBuf>) {
            (HashSet::new(), HashSet::new())
        }

        #[test]
        fn disabled_arm_predicts_zero_claims() {
            let (live, failed) = empty_inputs();
            let s = stack6();
            // Non-vacuity first: the SAME stack and the SAME inputs on an arm
            // that can claim name three clips. If this ever stops being three,
            // the EMPTY assertion below stops meaning anything.
            assert_eq!(
                predicted_hw_claim_inner(&s, Some((&live, &failed)), true).len(),
                MAX_HW_SESSIONS,
                "fixture sanity: an arm that CAN claim names MAX_HW_SESSIONS layers"
            );
            assert!(
                predicted_hw_claim_inner(&s, Some((&live, &failed)), false).is_empty(),
                "an arm where hardware cannot claim must predict NOTHING, so every \
                 pooled layer is warmed and the cache exit adopts warm children \
                 instead of building the top three cold (§ D-43)"
            );
        }

        #[test]
        fn healthy_arm_prediction_unchanged() {
            let (live, failed) = empty_inputs();
            let s = stack6();
            let expected = select_hw_clip_ids(&s, &failed, &live, MAX_HW_SESSIONS);
            assert_eq!(
                expected,
                vec!["l0", "l1", "l2"],
                "the real selector takes the top three in track order"
            );
            assert_eq!(
                predicted_hw_claim_inner(&s, Some((&live, &failed)), true),
                expected,
                "on the scoring arm the gate must be transparent — the prediction is \
                 the selector's own answer, byte for byte (the 135 -> 120 spawn win \
                 59-16 measured depends on it)"
            );
        }

        #[test]
        fn declined_predict_still_warms_everything() {
            let s = stack6();
            assert!(
                predicted_hw_claim_inner(&s, None, true).is_empty(),
                "the pre-existing contract: no session set means nothing to predict \
                 away, so every pooled layer is warmed (the Phase-57 delegation \
                 caller's path — pinned so the refactor cannot drop it)"
            );
            assert!(
                predicted_hw_claim_inner(&s, None, false).is_empty(),
                "and the gate cannot change that answer"
            );
        }

        #[test]
        fn the_gate_is_kill_switch_unset_and_latch_disengaged() {
            // A LOCAL latch — never the process-global HW_LATCH (see the module
            // doc). This pins the half of the wrapper's predicate that a pure
            // test can reach: `engaged()` flips false -> true and never back.
            let latch = engine::HwFailureLatch::new();
            assert!(!latch.engaged(), "a fresh latch is disengaged");
            latch.force_engage();
            assert!(latch.engaged(), "force_engage is the containment route (48-oom)");

            // And the decision table over BOTH inputs of the wrapper's
            // conjunction, so neither term can be dropped unnoticed: the answer
            // is non-empty for exactly one of the four combinations.
            let (live, failed) = empty_inputs();
            let s = stack6();
            for (kill_switch_set, latch_engaged) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                let arm_can_claim = !kill_switch_set && !latch_engaged;
                let got = predicted_hw_claim_inner(&s, Some((&live, &failed)), arm_can_claim);
                if arm_can_claim {
                    assert_eq!(
                        got.len(),
                        MAX_HW_SESSIONS,
                        "kill_switch={kill_switch_set} latch={latch_engaged}: the only \
                         combination that predicts a claim"
                    );
                } else {
                    assert!(
                        got.is_empty(),
                        "kill_switch={kill_switch_set} latch={latch_engaged}: either gate \
                         alone must decline"
                    );
                }
            }
        }
    }

    /// 18.3-03 Task 2 (design §3): the presenter's HOLD contract, pinned pure. A
    /// ring whose only entries are in the FUTURE (`timeline_us > target`) returns
    /// `None` from `pop_for_target` and leaves them queued — so the presenter
    /// HOLDs the last surface frame (NEVER black inside media; real gaps arrive as
    /// black ENTRIES from the producer, not a presenter special case). A queue
    /// drained to empty also returns `None` (underrun) without panicking.
    #[test]
    fn underrun_holds_never_pops_future() {
        let ctl = RingCtl::new(); // gen == 1
        assert!(ctl.push_blocking(entry(100_000, 1)));
        assert!(ctl.push_blocking(entry(133_333, 1)));

        // target BEFORE every entry: HOLD — return None, leave BOTH queued (never
        // pop a future frame).
        assert!(
            ctl.pop_for_target(1, 50_000).is_none(),
            "a future-only queue must HOLD (None), never pop a future frame"
        );
        assert_eq!(ctl.len(), 2, "HOLD leaves every future entry queued");

        // Once the target passes them, catch-up drains the queue (keeps newest).
        let kept = ctl
            .pop_for_target(1, i64::MAX / 2)
            .expect("due frames pop once the target passes them");
        assert_eq!(kept.timeline_us, 133_333, "catch-up keeps the newest <= target");
        assert_eq!(ctl.len(), 0, "catch-up drained the queue");

        // An empty queue underruns to None — no panic, no future-pop.
        assert!(
            ctl.pop_for_target(1, i64::MAX / 2).is_none(),
            "an empty queue underruns to None without panicking"
        );
    }
}
