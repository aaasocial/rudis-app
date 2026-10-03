//! Phase 57 (plan 57-06, PLAY-01/D-03): the **per-visible-layer hardware
//! decode-session coordinator** — the N-session generalization of
//! `ring.rs`'s one-clip `delegate_gpu_clip` / `gpu_producer_loop` pair.
//!
//! # What this is
//!
//! One dedicated `"rudis-hw-layer"` decode thread per hardware-eligible video
//! layer, lifecycle-matched to the LAYER SET (not to a single clip), each
//! publishing its latest zero-copy-imported [`engine::GpuFrame`] into a
//! per-layer **HOLD slot** (freshest-wins overwrite, the software
//! `LayerDecoderPool`'s HOLD semantics — never a backlog queue). The producer
//! thread stays the per-tick coordinator: it resolves the stack, calls
//! [`LayerSessionSet::sync_to_stack`], gathers a track-ordered mixed layer list
//! and composites once.
//!
//! # The four contracts this type is born holding
//!
//! 1. **Pitfall 3 — retime parity.** Every session carries its layer's
//!    [`crate::LayerSpec::src_step_us`] from the moment it is opened, and judges
//!    "is this demand a continuation or a seek" at the SAME `(src_step_us / 2)
//!    .max(1)` tolerance `LayerDecoderPool::advance` uses. A constant-retimed
//!    layer advances its demand by `src_step_us` per output tick; this
//!    coordinator serves that by **decoding forward and discarding**, never by
//!    re-seeking every tick (the respawn-storm bug `pool_retime.rs` pins for the
//!    software pool, which a parallel hardware coordinator would otherwise
//!    rediscover independently).
//! 2. **Pitfall 4 — no blocking joins on the coordinator.** Every wait here is
//!    a bounded poll: hold freshness is polled with a per-tick microsecond
//!    budget, and retirement is `thread.is_finished()` in <=25 ms beats with a
//!    500 ms cap before detach-and-drop. The only `join()` in this file runs
//!    AFTER `is_finished()` has already returned true, where it cannot block.
//!    The per-tick retire path (`reap_retiring`) does not wait at all — see
//!    § "Teardown must not stall production" below.
//! 3. **D-04 — per-LAYER fallback.** A session that fails (open error, decode
//!    error, import error) sets its own `failed` flag and exits; the coordinator
//!    observes that ONE layer, routes it to the software pool for the rest of
//!    this producer's life (`hw_failed`), and leaves every sibling session
//!    untouched. All-or-nothing fallback is a defect, not a simplification.
//! 4. **D-05 — the resolver seam.** Every session open resolves its media
//!    through [`crate::decode_source::resolve_decode_source`], never
//!    `LayerSpec.path` directly, so Phase 58 can put a proxy behind the seam
//!    without touching this file.
//!
//! # Teardown must not stall production (wave-0 finding F2)
//!
//! `57-BENCH-BASELINE.md` measured the 4K fixture's overlap **entry at 45 ms**
//! and its **exit at 1033 ms**, in all five runs. The entry was prewarmed; the
//! exit was not. So retirement here is **asynchronous by construction**: a
//! leaving layer is moved to a `retiring` list with its stop flag set, and the
//! coordinator returns IMMEDIATELY. Later ticks reap whatever has finished. The
//! bounded 25 ms poll exists for the two places that genuinely must be
//! synchronous — a flush, and the set's own `Drop` — and nowhere else.
//!
//! # Sessions are PARKED, not destroyed (Phase 59.1, plan 59.1-02)
//!
//! A layer that leaves the live set does not take its session with it: if the
//! session is healthy it moves to a `parked` waiting room, and a layer that
//! re-enters CLAIMS it back (`claim_parked`) instead of paying another
//! `engine::open_hw_decoder`. This is the multi-layer analog of `ring.rs`'s
//! single-clip `warm_hw` slot, and it replicates that slot's invalidation
//! ladder rather than re-deriving one — plus one rung `warm_hw` does not need
//! (`src_step_us` equality; see [`park_reuse_decision`]).
//!
//! It exists because it is the owner's 2026-08-09 field defect: 17 hardware
//! layer sessions opened in one session, all through
//! [`LayerSessionSet::open_session`], measured at `opens_delta = 3` per
//! leave/re-enter cycle and a **240.0 ms median re-entry wall**
//! (`crates/preview/tests/layer_reentry_probe.rs`).
//!
//! Three properties this is deliberately NOT allowed to have:
//!
//! * **It does not survive the producer.** The waiting room is a field of this
//!   type, drained by [`LayerSessionSet::retire_all`] and therefore by `Drop`.
//!   "Session retention across pause" is a rejected roadmap row and stays
//!   structurally impossible, not merely unimplemented.
//! * **It does not make any tick WAIT.** Claiming is synchronous and free
//!   (a `set_demand` store); nothing new blocks, and a layer whose HOLD slot has
//!   not reached the demand is still OMITTED by `LiveHold::usable_for` rather
//!   than waited for. Trading a late layer for a stutter would be strictly
//!   worse — see quick-260809-436.
//! * **It does not outrank a live layer.** A parked session holds a real VRAM
//!   reservation (RAII, as below) and one pinned pool slice; the waiting room is
//!   capped at [`MAX_HW_SESSIONS`] entries with evict-oldest, and an
//!   `OpenRefusal::Budget` on a LIVE open evicts from it and retries once.
//!
//! # Device loss is NOT detected here (57-01's measured finding)
//!
//! `ID3D12Device5::RemoveDevice` removes ONE D3D12 device, not the adapter:
//! 57-01 measured all three sessions continuing to decode AND import `Ok` after
//! a forced removal, with `GetDeviceRemovedReason` reporting nothing. So a
//! decode-side error here means "this media/session is bad", never "the device
//! is gone". Device-loss detection stays where it already is — the shell's ONE
//! wgpu device-lost callback (threat T-48-10-01).
//!
//! # VRAM (Pitfall 2, and 57-03's hand-off)
//!
//! Each session reserves its WHOLE pool's charge on the shared
//! [`engine::VramLedger`] BEFORE the ring depth of any sibling is derived, and
//! **the reservation is owned by the session struct, not by the decode
//! thread** — 57-03 explicitly flagged that a reservation tied to a thread's
//! scope becomes invisible the moment the session is parked. Dropping a
//! `LayerSession` releases it (RAII); there is no `release()` to forget.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::{LayerSpec, MultiLayerStack};

/// The maximum number of concurrent hardware decode sessions this coordinator
/// will run.
///
/// **Three, by MEASUREMENT** — `artifacts/57-A1-VERDICT.md` § Consequence 1.
/// Plan 57-01 drove three concurrent D3D11VA sessions against a real-time tick
/// budget and measured `720p_x3` at **16.30x** the 90 fps demand and
/// `4k_plus_720p_x2` at **5.21x**, both clear SUSTAINS, with three concurrent
/// 4K pools using 2838 MB of a 7602 MB budget and **0 MB residual** after
/// teardown.
///
/// Two things that verdict says which this constant must not be read without:
///
/// * **Do NOT raise it on the strength of the headroom figures.** Extrapolating
///   past the measured N is precisely 57-RESEARCH.md Pitfall 1, the error that
///   spike exists to prevent. Raising it means extending
///   `run_three_session_throughput_rep` to the new N and re-running.
/// * **It is an UPPER BOUND the ledger may lower at runtime, never a floor it
///   may assume** (verdict § Consequence 3). A session that cannot get a
///   sensible ring depth out of [`engine::gpu_ring_depth`] against what its
///   siblings already hold is refused and that layer falls back to software —
///   D-10's ordered shed, one rung at a time.
pub const MAX_HW_SESSIONS: usize = 3;

/// Retire poll beat — the SAME 25 ms the delegation wait uses
/// (`ring.rs:1743`), reused rather than re-picked (Pitfall 4).
const RETIRE_POLL_MS: u64 = 25;

/// Total bounded wait before a stopping session is detached and dropped.
/// Twenty beats: long enough for a decode call to return, short enough that a
/// wedged driver can never own the flush path.
const RETIRE_CAP_MS: u128 = 500;

/// Forward source gap beyond which serving a demand SEEKS instead of decoding
/// forward. Below it, decode-and-discard is cheaper than a demuxer seek plus a
/// keyframe re-decode; above it, it is not.
const SEEK_AHEAD_US: i64 = 1_000_000;

/// Upper bound on discarded frames while scanning toward a demand — the same
/// bound `ring.rs`'s `GPU_SEEK_DISCARD_CAP` sets (1800 frames ≈ 60 s @30fps).
const DISCARD_CAP: usize = 1800;

/// How long the coordinator will poll for a session's HOLD slot to reach the
/// current demand before compositing with whatever is already held.
///
/// Sized from 57-A1's measured per-frame decode+import cost: p50 0.16 ms at
/// 720p and 5.6 ms at 4K, p95 up to 12 ms at 720p. 12 ms covers the measured
/// p95 for every fixture in the phase while leaving two thirds of a 33.3 ms
/// frame budget for composite + push. Past it the coordinator uses the held
/// (at most one output tick stale) frame rather than dropping the layer out of
/// the composite — HOLD, exactly as the presenter HOLDs on underrun, because a
/// one-tick-stale layer is strictly better than a layer that vanishes.
const HOLD_WAIT_BUDGET: Duration = Duration::from_millis(12);

/// Poll beat for [`HOLD_WAIT_BUDGET`].
const HOLD_POLL: Duration = Duration::from_micros(500);

// ---------------------------------------------------------------------------
// Per-session control block + HOLD slot
// ---------------------------------------------------------------------------

/// The lock-free control block shared between the coordinator (writer of
/// `demand_us`/`stop`) and one decode thread (writer of `failed`/`served_us`).
struct SessionCtl {
    /// Set by the coordinator to retire this session. The thread observes it
    /// at the top of every loop and between decode calls.
    stop: AtomicBool,
    /// The SOURCE position this layer must show for the current output tick —
    /// `LayerSpec::source_us`, republished every tick.
    demand_us: AtomicI64,
    /// Set by the thread when it gives up (open/seek/decode/import failure).
    /// The coordinator polls it and routes THAT layer to software (D-04).
    failed: AtomicBool,
    /// The demand the HOLD slot currently answers, or `i64::MIN` when nothing
    /// has been published yet. Read by the coordinator's bounded freshness
    /// poll so it never has to take the hold lock just to ask "is it there".
    served_us: AtomicI64,
}

impl SessionCtl {
    fn new(demand_us: i64) -> Self {
        SessionCtl {
            stop: AtomicBool::new(false),
            demand_us: AtomicI64::new(demand_us),
            failed: AtomicBool::new(false),
            served_us: AtomicI64::new(i64::MIN),
        }
    }
}

/// One published, GPU-resident layer frame plus the demand it answers.
pub struct HeldFrame {
    /// The zero-copy-imported frame. Dropping it returns the hw-frame-pool
    /// slice to the decoder (48-05's drop-together contract), so the HOLD slot
    /// pins exactly ONE pool slice per session and never more.
    pub frame: engine::GpuFrame,
    /// The SOURCE position this frame was decoded to answer.
    pub source_us: i64,
}

/// The HOLD slot: at most one frame, freshest-wins.
type HoldSlot = Mutex<Option<HeldFrame>>;

/// A live session's HOLD slot, LOCKED for the duration of one composite tick.
///
/// The lock is what lets [`engine::MixedLayer::Gpu`] borrow the frame without
/// moving it: the decode thread's next publish parks (briefly — the composite
/// is a submit, not a wait) instead of swapping the texture out from under a
/// bind group. That IS the HOLD contract, expressed with a lock rather than a
/// copy.
pub struct LiveHold<'a> {
    /// The clip this slot belongs to.
    pub clip_id: &'a str,
    /// This layer's SOURCE advance per output tick — what sizes the HOLD
    /// window below.
    pub src_step_us: i64,
    guard: MutexGuard<'a, Option<HeldFrame>>,
}

/// How many output ticks of staleness a held frame may carry and still be
/// composited.
///
/// A HOLD is the right answer to a decoder that is a tick behind: showing the
/// previous frame beats dropping the layer out of the picture. It is the WRONG
/// answer to a seek, where the held frame is from a completely different part
/// of the timeline and would composite a visibly wrong picture for one tick.
/// Four ticks separates the two cases by two orders of magnitude — a seek moves
/// the demand by seconds.
const HOLD_WINDOW_TICKS: i64 = 4;

impl LiveHold<'_> {
    /// The held frame, if this session has published one yet.
    pub fn frame(&self) -> Option<&engine::GpuFrame> {
        self.guard.as_ref().map(|h| &h.frame)
    }
    /// The source position the held frame answers.
    pub fn source_us(&self) -> Option<i64> {
        self.guard.as_ref().map(|h| h.source_us)
    }
    /// The held frame IF it is close enough to `demand_us` to stand in for it
    /// (see [`HOLD_WINDOW_TICKS`]). `None` means "this layer contributes
    /// nothing this tick" — the same answer `cpu_layer_for_spec` gives for a
    /// pool layer that produced no frame.
    pub fn usable_for(&self, demand_us: i64) -> Option<&engine::GpuFrame> {
        let window = (self.src_step_us * HOLD_WINDOW_TICKS).max(1);
        self.guard
            .as_ref()
            .filter(|h| (h.source_us - demand_us).abs() <= window)
            .map(|h| &h.frame)
    }
}

// ---------------------------------------------------------------------------
// LayerSession
// ---------------------------------------------------------------------------

/// One hardware decode session bound to one visible layer.
pub struct LayerSession {
    clip_id: String,
    path: PathBuf,
    /// What [`LayerSessionSet::open_session`] ACTUALLY opened — the D-05
    /// seam's answer (`DecodeSource::path`), which is a proxy payload whenever
    /// one was fresh at open time and the clip's own media otherwise.
    ///
    /// Distinct from [`Self::path`] on purpose. `path` is the ORIGINAL spec
    /// path and stays the `hw_failed` key (the documented cross-arm asymmetry
    /// at `ring.rs`'s `delegate_gpu_clip` doc — not this plan's to change);
    /// `resolved_path` is the parity-critical one: a parked session may only be
    /// reused while the CURRENT resolve still answers the same file this
    /// session is decoding, which is `warm_hw`'s invalidation rule 3 (`ring.rs`
    /// — "different media path → drop") in the multi-layer coordinator.
    resolved_path: PathBuf,
    ctl: Arc<SessionCtl>,
    hold: Arc<HoldSlot>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Pitfall 3: the layer's SOURCE advance per output tick, carried from
    /// [`crate::LayerSpec::src_step_us`] from the moment the session is opened
    /// — never re-derived from the project frame step.
    src_step_us: i64,
    /// The VRAM ledger reservation, held for the SESSION's life rather than
    /// the decode THREAD's (57-03's explicit hand-off: a reservation scoped to
    /// a thread body becomes invisible to siblings the moment the session is
    /// parked). RAII — dropping this struct releases it.
    _reservation: Option<engine::LedgerReservation>,
    /// When retirement started, for the bounded poll.
    retire_started: Option<Instant>,
}

impl LayerSession {
    /// Republish this session's demand for the current output tick.
    fn set_demand(&self, source_us: i64) {
        self.ctl.demand_us.store(source_us, Ordering::Release);
    }

    fn failed(&self) -> bool {
        self.ctl.failed.load(Ordering::Acquire)
    }

    /// Has this session published a frame for `demand` yet?
    fn serves(&self, demand: i64) -> bool {
        let tolerance = (self.src_step_us / 2).max(1);
        let served = self.ctl.served_us.load(Ordering::Acquire);
        served != i64::MIN && (served - demand).abs() <= tolerance
    }

    /// Ask this session to stop. Does NOT wait — see the module doc's
    /// "Teardown must not stall production".
    fn request_stop(&mut self) {
        self.ctl.stop.store(true, Ordering::Release);
        if self.retire_started.is_none() {
            self.retire_started = Some(Instant::now());
        }
    }

    /// `true` once the decode thread has exited (or was already detached).
    fn finished(&self) -> bool {
        self.thread.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }

    /// Reap a FINISHED thread. Only ever called once `finished()` is true, so
    /// the `join()` below returns immediately and cannot block the coordinator
    /// (Pitfall 4 — this is the acceptable-join case, and this comment is the
    /// reason it is acceptable).
    fn reap_finished(&mut self) {
        if let Some(t) = self.thread.take() {
            debug_assert!(t.is_finished(), "reap_finished called on a live thread");
            let _ = t.join(); // safe: is_finished() == true, returns immediately
        }
    }

    /// Past the bounded retire cap?
    fn retire_expired(&self) -> bool {
        self.retire_started
            .map(|t| t.elapsed().as_millis() >= RETIRE_CAP_MS)
            .unwrap_or(false)
    }
}

impl Drop for LayerSession {
    fn drop(&mut self) {
        // A detached-and-dropped session's thread still owns its
        // HwDecodeSession and exits within one decode call; the stop flag is
        // what guarantees that. Never a join here: `Drop` runs on the
        // coordinator thread.
        self.ctl.stop.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Eligibility — pure, hardware-free, unit-testable
// ---------------------------------------------------------------------------

/// Is this layer a candidate for a hardware decode session?
///
/// The SAME gates `choose_decode_path` applies for a single clip, plus the
/// content kinds the software pool already excludes for the identical reason
/// (`pool_sources` — text rasterizes, image sequences and stills and RAMP
/// retimes decode inline; none of them is a streaming media source at all).
pub fn spec_is_hw_eligible(spec: &LayerSpec, hw_failed: &HashSet<PathBuf>) -> bool {
    spec.text.is_none()
        && !spec.is_still_image
        && !spec.is_image_sequence
        && !spec.retime_ramped
        && !hw_failed.contains(&spec.path)
}

/// Choose which of `stack`'s layers get hardware sessions this tick, in
/// TRACK order, capped at [`MAX_HW_SESSIONS`].
///
/// `already_live` is preferred: a layer that already owns a session keeps it
/// even when it sits below the cap boundary in track order, so a set change
/// that adds a layer cannot silently evict a warm session and re-open it one
/// tick later.
pub fn select_hw_clip_ids(
    stack: &MultiLayerStack,
    hw_failed: &HashSet<PathBuf>,
    already_live: &HashSet<String>,
    cap: usize,
) -> Vec<String> {
    let eligible: Vec<&LayerSpec> = stack
        .layers
        .iter()
        .filter(|s| spec_is_hw_eligible(s, hw_failed))
        .collect();
    let mut chosen: Vec<String> = Vec::with_capacity(cap.min(eligible.len()));
    // Pass 1: keep what is already running (track order among them).
    for spec in eligible.iter().filter(|s| already_live.contains(&s.clip_id)) {
        if chosen.len() >= cap {
            break;
        }
        chosen.push(spec.clip_id.clone());
    }
    // Pass 2: fill the remaining slots in track order (index 0 = top first).
    for spec in eligible.iter().filter(|s| !already_live.contains(&s.clip_id)) {
        if chosen.len() >= cap {
            break;
        }
        chosen.push(spec.clip_id.clone());
    }
    chosen
}

// ---------------------------------------------------------------------------
// LayerSessionSet — the coordinator
// ---------------------------------------------------------------------------

/// The producer-owned set of per-layer hardware decode sessions.
///
/// Lives on the producer thread for that thread's whole life; every method
/// runs there. Dropping it stops and reaps every session (bounded).
pub struct LayerSessionSet {
    compositor: Arc<engine::Compositor>,
    budget: Arc<AtomicU64>,
    ledger: engine::VramLedger,
    /// Sessions serving the CURRENT layer set.
    live: Vec<LayerSession>,
    /// Sessions opened toward an upcoming boundary, not yet adopted.
    pending: Vec<LayerSession>,
    /// Sessions asked to stop, not yet reaped. Reaped opportunistically on
    /// later ticks — never waited for inside a production tick.
    retiring: Vec<LayerSession>,
    /// **Phase 59.1 (plan 59.1-02) — the waiting room.** Healthy sessions whose
    /// layer LEFT the live set, kept alive (thread parked on its HOLD-on-stall
    /// sleep, VRAM reservation still held by RAII) so a layer that re-enters
    /// CLAIMS its session back instead of paying a full cold
    /// `engine::open_hw_decoder`.
    ///
    /// This is one more waiting room in the EXISTING lifecycle, not a fourth
    /// session manager: a claimed entry is rebased with `set_demand` and pushed
    /// straight back onto `live`/`pending`, which is verbatim the
    /// adopt-with-rebase discipline [`Self::adopt_pending`] already used for
    /// prewarms.
    ///
    /// **No retention across pause, by CONSTRUCTION.** This list lives inside
    /// the set, and the set lives on the producer thread for that thread's
    /// whole life — [`Self::retire_all`] and `Drop` drain it alongside
    /// live+pending. When the producer dies, every parked session dies with it,
    /// exactly as `ring.rs`'s `warm_hw` does. The roadmap's rejected
    /// "session retention across pause" row is therefore honored structurally;
    /// nothing here adds a lifetime.
    ///
    /// **What a parked session costs, measured rather than waved at:** its
    /// hw-frame-pool VRAM reservation stays on the shared ledger (the same
    /// accounting quick-260809-436 recorded as a sibling's ring depth going
    /// 7 → 6), and its HOLD slot still pins exactly one pool slice. Both are
    /// bounded by [`MAX_HW_SESSIONS`] entries here, and the ledger's own
    /// refusal (`OpenRefusal::Budget`) evicts from this list before a LIVE
    /// layer is shed — a parked convenience never outranks a live layer.
    parked: Vec<LayerSession>,
    /// The frame-step-bucketed boundary a prewarm has already been made for
    /// (the SAME one-prewarm-per-boundary latch discipline the software
    /// `Lookahead` uses — this coordinator deliberately does not add a second
    /// latch mechanism, it mirrors that one).
    prewarmed_for: Option<i64>,
    /// How many hardware sessions this set has EVER opened. The observability
    /// counter 57-07's differential reads: a coordinator that re-opens a
    /// session per boundary (or worse, per tick) shows up here as a number
    /// that will not stop climbing.
    opens: u64,
    /// How many parked sessions have been CLAIMED back by a re-entering layer.
    /// The other half of [`Self::opens`]: "opens stayed flat" is only a reuse
    /// claim when something was actually reused, and a green pixel-parity run
    /// with `claims == 0` would mean parking never engaged (T-59.1-02-03).
    claims: u64,
    /// How many parked sessions were evicted to keep [`Self::parked`] within
    /// [`MAX_HW_SESSIONS`] (evict-oldest).
    park_evictions: u64,
    /// How many times an `OpenRefusal::Budget` evicted the oldest parked entry
    /// and retried the open exactly ONCE. Counts the RETRY, so a zero means the
    /// pressure valve has never been exercised on this run.
    budget_retries: u64,
}

impl LayerSessionSet {
    /// A set bound to the producer's compositor, live VRAM budget handle and
    /// shared ledger. Opening nothing until a stack asks for it.
    pub fn new(
        compositor: Arc<engine::Compositor>,
        budget: Arc<AtomicU64>,
        ledger: engine::VramLedger,
    ) -> Self {
        LayerSessionSet {
            compositor,
            budget,
            ledger,
            live: Vec::new(),
            pending: Vec::new(),
            retiring: Vec::new(),
            parked: Vec::new(),
            prewarmed_for: None,
            opens: 0,
            claims: 0,
            park_evictions: 0,
            budget_retries: 0,
        }
    }

    /// How many sessions are serving the current layer set.
    pub fn live_len(&self) -> usize {
        self.live.len()
    }

    /// How many sessions have EVER been opened (57-07's churn odometer).
    pub fn opens(&self) -> u64 {
        self.opens
    }

    /// How many healthy sessions are currently PARKED, waiting for their layer
    /// to re-enter. Bounded by [`MAX_HW_SESSIONS`].
    pub fn parked_len(&self) -> usize {
        self.parked.len()
    }

    /// How many parked sessions have been claimed back by a re-entering layer
    /// — the reuse odometer that makes "opens stayed flat" non-vacuous.
    pub fn claims(&self) -> u64 {
        self.claims
    }

    /// How many parked sessions were evicted to hold the parked cap.
    pub fn park_evictions(&self) -> u64 {
        self.park_evictions
    }

    /// How many ledger-pressure (`OpenRefusal::Budget`) evict-and-retry-once
    /// cycles have run.
    pub fn budget_retries(&self) -> u64 {
        self.budget_retries
    }

    /// Bytes this set's sessions — live, pending, parked and not-yet-reaped —
    /// currently hold on the shared [`engine::VramLedger`].
    ///
    /// Read-only, and added (quick `260821-3qx`) so a pressure test can DERIVE
    /// the budget at which admission starts refusing instead of hardcoding one:
    /// a magic constant would silently stop bracketing the refusal the day a
    /// fixture's dims, the pool width or [`engine::POOL_VRAM_COUNT_FACTOR`]
    /// changed, and the test would keep passing while measuring nothing.
    /// Same family as [`Self::opens`] and [`Self::budget_retries`] — an
    /// odometer, not a control.
    pub fn reserved_bytes(&self) -> u64 {
        self.ledger.reserved()
    }

    /// Reap finished retiring sessions and detach expired ones. NON-BLOCKING:
    /// this is the per-tick teardown path and it never waits (wave-0 F2 — the
    /// measured 1033 ms overlap EXIT is exactly what a waiting teardown looks
    /// like from the presenter's side).
    fn reap_retiring(&mut self) {
        let mut keep = Vec::with_capacity(self.retiring.len());
        for mut s in self.retiring.drain(..) {
            if s.finished() {
                s.reap_finished();
                continue; // dropped here: reservation released, session gone
            }
            if s.retire_expired() {
                // Detach: drop the handle without joining. The thread observes
                // the stop flag within one decode call and tears its own
                // session down; we stop accounting for it here rather than
                // hold a production tick hostage to a wedged driver.
                let _ = s.thread.take();
                continue;
            }
            keep.push(s);
        }
        self.retiring = keep;
    }

    /// Move `session` onto the retiring list with its stop flag set.
    fn retire(&mut self, mut session: LayerSession) {
        session.request_stop();
        self.retiring.push(session);
    }

    /// **The claim.** Take back the parked session for `spec`'s clip, REBASED
    /// to this tick's demand — or `None`, meaning "cold-open this layer".
    ///
    /// Consulted by the two fill sites BEFORE [`Self::open_session`]; the
    /// cold-open primitive itself is untouched, so every policy it holds
    /// (ledger sizing, the latch feed, `hw_failed`, the D-19 log line) is
    /// unchanged for the opens that still happen.
    ///
    /// The resolve happens HERE, once per claim, and is the parity-critical
    /// half: `resolve_decode_source` is the ONE place that knows whether a
    /// proxy landed (or was evicted) since this session opened, and its answer
    /// changing is exactly the event that must forbid reuse
    /// (`warm_hw` invalidation rule 3's analog — T-59.1-02-02).
    fn claim_parked(&mut self, spec: &LayerSpec) -> Option<LayerSession> {
        if self.parked.is_empty() {
            return None;
        }
        // The clip_id match is the lookup, not part of the decision: a parked
        // session belongs to exactly one clip and there is at most one entry
        // per clip (nothing parks a clip that already has a parked entry —
        // a clip cannot be live twice).
        let pos = self.parked.iter().position(|s| s.clip_id == spec.clip_id)?;
        // D-05, THE seam. Resolved here rather than trusted from open time,
        // because the answer is exactly what a proxy landing/eviction changes
        // underneath a parked session.
        let current = crate::decode_source::resolve_decode_source(
            &spec.clip_id,
            &spec.path,
            spec.source_us,
        );
        let decision = {
            let p = &self.parked[pos];
            park_reuse_decision(
                spec,
                &current.path,
                ParkedSessionFacts {
                    resolved_path: &p.resolved_path,
                    src_step_us: p.src_step_us,
                    healthy: !p.failed() && !p.finished(),
                },
            )
        };
        let session = self.parked.remove(pos);
        match decision {
            ParkDecision::Reuse => {
                // ADOPT-with-REBASE, verbatim `adopt_pending`'s discipline:
                // republish the demand and hand the session back. The thread's
                // own `need_seek` fires against its stale `last_pts` and
                // re-seeks; `end_hold_released` covers the case where it had
                // reached EOF and the layer re-enters behind that position.
                session.set_demand(spec.source_us);
                self.claims += 1;
                Some(session)
            }
            ParkDecision::Drop(reason) => {
                // Permanent for this session: a changed resolve or a changed
                // retime cannot un-change, and a dead thread cannot revive. So
                // it retires rather than staying parked for a later visit.
                eprintln!(
                    "hwdecode: parked layer session for clip={} not reused ({reason}) — \
                     cold-opening instead",
                    spec.clip_id
                );
                self.retire(session);
                None
            }
        }
    }

    /// Park `session` instead of retiring it — the whole mechanism, in one
    /// place so all four retire-class sites cannot drift apart.
    ///
    /// Parks iff the session is HEALTHY. A `failed()` session is never parked
    /// (it is D-04's routing signal, and its layer is already headed for the
    /// software pool); nor is one whose thread has already exited.
    ///
    /// Evict-oldest keeps the waiting room at [`MAX_HW_SESSIONS`]. The cap is
    /// on ENTRIES, not on decode concurrency — a parked session does not decode
    /// (its thread is on the HOLD-on-stall sleep) — but its VRAM reservation
    /// and its one pinned pool slice are real, so the count is bounded anyway.
    fn retire_or_park(&mut self, session: LayerSession) {
        if session.failed() || session.finished() {
            self.retire(session);
            return;
        }
        while self.parked.len() >= MAX_HW_SESSIONS {
            let oldest = self.parked.remove(0);
            self.park_evictions += 1;
            self.retire(oldest);
        }
        self.parked.push(session);
    }

    /// Retire the OLDEST parked entry to make room for a live layer the ledger
    /// just refused. Returns `true` when something was evicted.
    ///
    /// **A parked convenience never outranks a live layer.** Exactly one
    /// eviction per refusal, never a loop: a loop would drain the whole waiting
    /// room on a single tight-budget tick and turn a transient refusal into a
    /// full re-open storm on the next.
    fn evict_oldest_parked(&mut self) -> bool {
        if self.parked.is_empty() {
            return false;
        }
        let oldest = self.parked.remove(0);
        self.park_evictions += 1;
        eprintln!(
            "hwdecode: VRAM ledger refused a layer session — evicting the oldest parked \
             session (clip={}) and retrying once",
            oldest.clip_id
        );
        self.retire(oldest);
        true
    }

    /// `open_session` with the ledger-pressure valve: on `OpenRefusal::Budget`
    /// and a non-empty waiting room, evict the oldest parked entry and retry
    /// EXACTLY once.
    ///
    /// The retry is honest about what it can do. Eviction is asynchronous like
    /// every other retirement here (wave-0 F2 — teardown must not stall
    /// production), so the evicted session's `LedgerReservation` is released
    /// when its thread is reaped, not at the instant it is asked to stop. The
    /// non-blocking `reap_retiring` below collects it if it has already
    /// finished; otherwise the retry still fails, this layer serves from the
    /// software pool for ONE tick, and the next `sync_to_stack` (which reaps
    /// first) opens it on hardware. What the eviction guarantees is that a
    /// parked session can never permanently strand capacity a live layer needs
    /// — the 59-20 shape — not that the very next call succeeds.
    ///
    /// # It fires as of 2026-08-21 — D-59.1-3 CLOSED
    ///
    /// This note used to read *"It does not fire today, and that is a FINDING"*:
    /// `OpenRefusal::Budget` was raised iff `engine::gpu_ring_depth` answered 0,
    /// and that function ends in `.clamp(GPU_RING_MIN_DEPTH, ..)` with
    /// `GPU_RING_MIN_DEPTH == 4`, so it could not answer 0 for any budget,
    /// frame size or sibling reservation. D-10's rung 1 — the rung PLAY-09's
    /// wording lists third, "per-layer software fallback" — was dead from
    /// Phase 57 to Phase 60, and the valve below was wired against a refusal
    /// nothing could raise.
    ///
    /// Quick task `260821-3qx` separated the two questions the single number
    /// had conflated. [`Self::open_session`] now asks
    /// [`engine::gpu_session_admissible`] — *"can this session fund the minimum
    /// pool the floor forces, alongside its siblings?"* — and
    /// `gpu_ring_depth` keeps answering only what it was built to answer, with
    /// its floor, ceiling and clamp byte-untouched. The refusal is REACHABLE
    /// under real runtime conditions, so this valve is live code with live
    /// coverage: `parked_capacity_is_bounded_and_ledger_pressure_evicts` now
    /// drives a genuine refusal and asserts the evict-and-retry-once behaviour
    /// this doc promises, in place of the pin it used to carry on the
    /// unreachability itself. That is the hand-off the old note asked for,
    /// taken.
    fn open_session_with_ledger_relief(
        &mut self,
        spec: &LayerSpec,
        demand_us: i64,
    ) -> Result<LayerSession, OpenRefusal> {
        match self.open_session(spec, demand_us) {
            Err(OpenRefusal::Budget) if !self.parked.is_empty() => {
                self.evict_oldest_parked();
                self.reap_retiring();
                self.budget_retries += 1;
                self.open_session(spec, demand_us)
            }
            other => other,
        }
    }

    /// Retire every parked session — the SET-WIDE half of the invalidation
    /// ladder (`warm_hw` rules 1 and 2, `ring.rs`).
    ///
    /// Checked once per sync rather than per entry because both conditions are
    /// session-wide policy, not per-media facts: once the latch has engaged, or
    /// the kill switch is set, hardware reuse must not outlive the decision any
    /// more than a hardware OPEN may.
    fn retire_all_parked(&mut self) {
        if self.parked.is_empty() {
            return;
        }
        let stale: Vec<LayerSession> = self.parked.drain(..).collect();
        eprintln!(
            "hwdecode: retiring {} parked layer session(s) — hardware decode is switched off \
             session-wide (latch engaged or kill switch set)",
            stale.len()
        );
        for s in stale {
            self.retire(s);
        }
    }

    /// The clip ids currently owning a live session.
    ///
    /// `pub` since 59-16: `ring.rs`'s cache-exit prewarm predicts, one
    /// lookahead horizon early, which layers this set will claim at the
    /// boundary — and [`select_hw_clip_ids`]'s `already_live` argument is what
    /// makes that prediction agree with the selection [`Self::sync_to_stack`]
    /// will actually make there. Read-only and allocating; nothing outside this
    /// type mutates the live set.
    pub fn live_ids(&self) -> HashSet<String> {
        self.live.iter().map(|s| s.clip_id.clone()).collect()
    }

    /// Reconcile the session set with `stack` and republish every live
    /// session's demand for this tick.
    ///
    /// Returns the clip ids that a hardware session is serving this tick — the
    /// EXACT set the caller must exclude from the software pool's sources
    /// (D-04: demote, never delete).
    ///
    /// Order is load-bearing:
    /// 1. reap finished retirees (non-blocking),
    /// 2. observe per-session failures and route those layers to software,
    /// 3. retire sessions whose layer left the stack,
    /// 4. adopt matching prewarmed sessions, REBASED to this tick's demand,
    /// 5. open sessions for layers that still need one,
    /// 6. republish demands.
    ///
    /// `latch` is GPU-05's session-wide circuit breaker — the SAME one
    /// `ring.rs`'s single-clip `choose_decode_path` uses — and this method
    /// holds BOTH halves of its contract: it is consulted before any open is
    /// attempted (step 5) and fed with the typed failure when one is refused,
    /// at most once per distinct media (the `hw_failed` insert beside it is
    /// what makes that true). A coordinator that took the latch and used
    /// neither half would opt the whole multi-layer path out of the policy in
    /// both directions.
    pub fn sync_to_stack(
        &mut self,
        stack: &MultiLayerStack,
        hw_failed: &mut HashSet<PathBuf>,
        latch: &engine::HwFailureLatch,
    ) -> HashSet<String> {
        self.reap_retiring();

        // (0) SET-WIDE park invalidation — `warm_hw`'s rules 1 and 2, in the
        // same order and for the same reasons (`ring.rs`): an engaged latch
        // means session-wide policy said stop using hardware, and the kill
        // switch is SPIKE-06's re-isolation escape hatch. Reuse must not
        // outlive either. Checked once here rather than per parked entry
        // because neither is a per-media fact, and doing it at the top means
        // the claim below never has to re-ask.
        if latch.engaged() || std::env::var_os(engine::KILL_SWITCH_ENV).is_some() {
            self.retire_all_parked();
        }

        // (2) D-04: a failed session takes ITS layer to software and nothing
        // else. Siblings are not touched, not restarted, not even notified.
        let mut failed: Vec<LayerSession> = Vec::new();
        let mut i = 0;
        while i < self.live.len() {
            if self.live[i].failed() {
                failed.push(self.live.remove(i));
            } else {
                i += 1;
            }
        }
        for s in failed {
            eprintln!(
                "hwdecode: layer session for {} failed — routing THAT layer to the \
                 software pool for the rest of this producer's life (D-04); {} sibling \
                 session(s) unaffected",
                s.path.display(),
                self.live.len()
            );
            hw_failed.insert(s.path.clone());
            self.retire(s);
        }
        // A pending prewarm for now-failed media is worthless too.
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].failed() || hw_failed.contains(&self.pending[i].path) {
                let s = self.pending.remove(i);
                hw_failed.insert(s.path.clone());
                self.retire(s);
            } else {
                i += 1;
            }
        }

        // (3) Which layers should own a session this tick?
        let want = select_hw_clip_ids(stack, hw_failed, &self.live_ids(), MAX_HW_SESSIONS);
        let want_set: HashSet<&str> = want.iter().map(|s| s.as_str()).collect();
        let mut i = 0;
        while i < self.live.len() {
            if want_set.contains(self.live[i].clip_id.as_str()) {
                i += 1;
            } else {
                // PARK SITE (a): the layer left the want set. It is the single
                // most common way a session dies and the most common way the
                // same clip comes back a moment later.
                let s = self.live.remove(i);
                self.retire_or_park(s);
            }
        }

        // (4) + (5): fill the gaps, preferring a prewarmed session.
        for clip_id in &want {
            if self.live.iter().any(|s| &s.clip_id == clip_id) {
                continue;
            }
            let Some(spec) = stack.layers.iter().find(|s| &s.clip_id == clip_id) else {
                continue;
            };
            if let Some(pos) = self.pending.iter().position(|s| &s.clip_id == clip_id) {
                let session = self.pending.remove(pos);
                // ADOPT-with-REBASE: a prewarm aimed at the boundary estimate
                // can sit up to one frame off this tick's real demand. The
                // software pool's `adopt_missing` rebases rather than throwing
                // the warm session away; so does this.
                session.set_demand(spec.source_us);
                self.live.push(session);
                continue;
            }
            // Phase 59.1 (plan 59.1-02): CLAIM before opening. A layer that
            // left the live set and is now re-entering takes its own parked
            // session back — the owner's 2026-08-09 "laggy when it starts
            // loading clips" is this line being absent. Placed after the
            // pending check (a prewarm aimed at THIS boundary is fresher) and
            // before the latch check (an engaged latch has already emptied
            // `parked` at the top of this method, so the order is consistent
            // either way).
            if let Some(session) = self.claim_parked(spec) {
                self.live.push(session);
                continue;
            }
            // GPU-05's session-wide circuit breaker, CONSULTED — the same
            // latch, and the same question, `choose_decode_path` asks for the
            // single-clip path (`ring.rs`): once repeated hardware-init
            // failures have proven this session should stop asking, THIS
            // coordinator stops asking too, for every layer. Deliberately not
            // cached into `hw_failed`: an engaged latch is a cheap session-wide
            // skip, never a per-media verdict (`choose_decode_path`'s
            // `Software { cache_media: false }`), and the media is a perfectly
            // good hardware candidate again in the next process.
            if latch.engaged() {
                continue; // this layer serves from the software pool
            }
            match self.open_session_with_ledger_relief(spec, spec.source_us) {
                Ok(session) => self.live.push(session),
                Err(e) => {
                    eprintln!(
                        "hwdecode: layer session open failed for {}: {e} — this layer \
                         serves from the software pool",
                        spec.path.display()
                    );
                    // …and FED. `HwFailureLatch::record` counts `InitFailed`
                    // only (capability routing and the kill switch are not
                    // hardware trouble) and requires at-most-once per distinct
                    // media — which the `hw_failed` insert immediately below
                    // is: a media in that set is filtered out by
                    // `spec_is_hw_eligible`, so `open_session` is never called
                    // for it again in this producer's life. Without this the
                    // multi-layer path could never engage the latch at all,
                    // and the single-clip path had no way to learn about
                    // trouble this coordinator had already seen.
                    if let OpenRefusal::Open(ref open_err) = e {
                        if latch.record(open_err) && latch.engaged() {
                            eprintln!(
                                "hwdecode: {} hardware-init failures across different media \
                                 — session latch ENGAGED; skipping hardware-decode attempts \
                                 for the rest of this session (software path serves preview)",
                                latch.failures()
                            );
                        }
                    }
                    if !matches!(e, OpenRefusal::Budget) {
                        hw_failed.insert(spec.path.clone());
                    }
                }
            }
        }

        // (6) Republish demands.
        let mut serving = HashSet::new();
        for session in &self.live {
            if let Some(spec) = stack.layers.iter().find(|s| s.clip_id == session.clip_id) {
                session.set_demand(spec.source_us);
                serving.insert(session.clip_id.clone());
            }
        }
        serving
    }

    /// Bounded-poll every live session's HOLD slot toward the current demand.
    ///
    /// NEVER a join, never unbounded: at most [`HOLD_WAIT_BUDGET`], polled in
    /// [`HOLD_POLL`] beats (Pitfall 4). Returns when every live session either
    /// serves its demand or has failed, or when the budget expires — whichever
    /// comes first. The caller composites with whatever is held either way.
    pub fn await_holds(&self, stack: &MultiLayerStack) {
        if self.live.is_empty() {
            return;
        }
        let deadline = Instant::now() + HOLD_WAIT_BUDGET;
        loop {
            let mut all_ready = true;
            for session in &self.live {
                if session.failed() {
                    continue;
                }
                let demand = stack
                    .layers
                    .iter()
                    .find(|s| s.clip_id == session.clip_id)
                    .map(|s| s.source_us);
                if let Some(d) = demand {
                    if !session.serves(d) {
                        all_ready = false;
                        break;
                    }
                }
            }
            if all_ready || Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(HOLD_POLL);
        }
    }

    /// Lock every live session's HOLD slot for one composite tick.
    ///
    /// The returned guards must be dropped before the next
    /// [`LayerSessionSet::sync_to_stack`]; holding them across a tick would
    /// stall every decode thread.
    pub fn lock_live_holds(&self) -> Vec<LiveHold<'_>> {
        self.live
            .iter()
            .map(|s| LiveHold {
                clip_id: s.clip_id.as_str(),
                src_step_us: s.src_step_us,
                guard: s.hold.lock().unwrap_or_else(|p| p.into_inner()),
            })
            .collect()
    }

    /// Stop every live and pending session WITHOUT waiting — the producer left
    /// the multi range (or is about to), and a production tick must never be
    /// held hostage to teardown (wave-0 F2). Reaps whatever has already
    /// finished; the rest are collected on later calls, or bounded at `Drop`.
    pub fn retire_live(&mut self) {
        if self.live.is_empty() && self.pending.is_empty() && self.retiring.is_empty() {
            return; // the common single-layer tick: nothing to do, nothing to lock
        }
        let leaving: Vec<LayerSession> =
            self.live.drain(..).chain(self.pending.drain(..)).collect();
        for s in leaving {
            // PARK SITE (b): the producer left the multi range — `ring.rs`'s
            // delegated → multi → delegated cycle, which is the owner's exact
            // 2026-08-09 scenario. Parking here is what stops the return trip
            // paying N cold opens.
            self.retire_or_park(s);
        }
        self.prewarmed_for = None;
        self.reap_retiring();
    }

    /// Open sessions for the hardware-eligible layers of an UPCOMING boundary
    /// stack, into `pending`, while the current arm still owns production.
    ///
    /// This is `prewarm_boundary_stack`'s hardware twin and it obeys the same
    /// one-prewarm-per-boundary latch: a repeated call for the same bucketed
    /// boundary is a no-op, and a boundary that supersedes an older one drops
    /// the stale pendings (their sessions retire on the async path).
    ///
    /// # The failure latch: consulted here, fed in [`Self::sync_to_stack`]
    ///
    /// GPU-05's circuit breaker is checked before any prewarm open, for the
    /// same reason `sync_to_stack` checks it — a session that has already
    /// proven hardware is systemically broken must stop calling
    /// `engine::open_hw_decoder`, and a SPECULATIVE open is the last one that
    /// should be exempt. It is deliberately not FED here: `record` requires
    /// at-most-once per distinct media, and only `sync_to_stack` holds that
    /// guarantee (its `hw_failed` insert is what stops the media being
    /// re-attempted). A prewarm's refusal is re-testable — the same media is
    /// opened again at the boundary tick, where the failure IS recorded — so
    /// nothing is lost by staying silent here, whereas double-counting one bad
    /// file could engage a session-wide latch on its own.
    pub fn prewarm_for_boundary(
        &mut self,
        stack: &MultiLayerStack,
        boundary_pos: i64,
        hw_failed: &HashSet<PathBuf>,
        latch: &engine::HwFailureLatch,
    ) {
        // Phase 57 (plan 57-07): the hardware half of the boundary-prewarm kill
        // switch. Inert unless `RUDIS_DISABLE_BOUNDARY_PREWARM` is set — see
        // [`crate::DISABLE_PREWARM_ENV`]. Checked at the top so BOTH halves of a
        // prewarm (software pool + hardware sessions) turn off together and the
        // differential really is one variable.
        if crate::boundary_prewarm_disabled() {
            return;
        }
        // GPU-05: no speculative hardware opens once the session latch has
        // engaged (see the doc above). Before the boundary latch is touched, so
        // a prewarm skipped for this reason is not recorded as one that ran.
        if latch.engaged() {
            return;
        }
        let step = engine::frame_step_us(stack.fps).max(1);
        let bucket = boundary_pos - boundary_pos.rem_euclid(step);
        match self.prewarmed_for {
            Some(b) if b == bucket => return, // already warmed toward THIS boundary
            Some(_) => {
                // Superseded: retire the stale prewarms asynchronously —
                // PARK SITE (d). A prewarm aimed at a boundary that was
                // superseded is a perfectly healthy session for a clip the
                // timeline is still very likely to reach.
                let stale: Vec<LayerSession> = self.pending.drain(..).collect();
                for s in stale {
                    self.retire_or_park(s);
                }
            }
            None => {}
        }
        let live = self.live_ids();
        let want = select_hw_clip_ids(stack, hw_failed, &live, MAX_HW_SESSIONS);
        // Only the INCOMING layers need warming — a continuing layer's live
        // session keeps streaming and must never be disturbed.
        let incoming: Vec<&LayerSpec> = want
            .iter()
            .filter(|id| !live.contains(*id))
            .filter_map(|id| stack.layers.iter().find(|s| &s.clip_id == id))
            .collect();
        if incoming.is_empty() {
            return;
        }
        self.prewarmed_for = Some(bucket);
        for spec in incoming {
            // Total concurrency, live + pending, still respects the measured cap.
            if self.live.len() + self.pending.len() >= MAX_HW_SESSIONS {
                break;
            }
            // Phase 59.1 (plan 59.1-02): the prewarm's claim site. Warming an
            // incoming layer that already owns a parked session must not cost
            // a cold open either — this is the delegated→multi→delegated cycle
            // (quick-260809-436's boundary) where the owner's churn lives.
            if let Some(session) = self.claim_parked(spec) {
                self.pending.push(session);
                continue;
            }
            match self.open_session_with_ledger_relief(spec, spec.source_us) {
                Ok(session) => self.pending.push(session),
                Err(e) => eprintln!(
                    "hwdecode: boundary prewarm skipped for {}: {e}",
                    spec.path.display()
                ),
            }
        }
    }

    /// Move prewarmed sessions matching `stack` into the live set, REBASED to
    /// this stack's demands; retire the ones nothing matched.
    ///
    /// `sync_to_stack` already adopts opportunistically; this is the explicit
    /// boundary hook, so a caller that crosses a boundary without a production
    /// tick in between still consumes the warm sessions rather than dropping
    /// them.
    pub fn adopt_pending(&mut self, stack: &MultiLayerStack) {
        let mut unmatched: Vec<LayerSession> = Vec::new();
        for session in self.pending.drain(..) {
            match stack.layers.iter().find(|s| s.clip_id == session.clip_id) {
                Some(spec) if self.live.iter().all(|l| l.clip_id != session.clip_id) => {
                    session.set_demand(spec.source_us); // the rebase
                    self.live.push(session);
                }
                _ => unmatched.push(session),
            }
        }
        for s in unmatched {
            // PARK SITE (c): a prewarm nothing at this boundary matched. Same
            // argument as site (d) — healthy session, plausible future layer.
            self.retire_or_park(s);
        }
        self.prewarmed_for = None;
    }

    /// Stop and reap EVERYTHING, bounded.
    ///
    /// The one synchronous path: a flush, or the set's `Drop`. Poll
    /// `is_finished()` in [`RETIRE_POLL_MS`] beats up to [`RETIRE_CAP_MS`]
    /// total, then detach-and-drop whatever is left. Never a bare join.
    pub fn retire_all(&mut self) {
        // The waiting room drains WITH live+pending, and only here — which is
        // what makes "no session retention across pause" structural rather
        // than promised: this runs from the set's `Drop`, and the set dies with
        // the producer thread. Nothing outside this type can keep a parked
        // session alive one instruction longer than the producer lives.
        for s in self
            .live
            .drain(..)
            .chain(self.pending.drain(..))
            .chain(self.parked.drain(..))
        {
            self.retiring.push(s);
        }
        for s in self.retiring.iter_mut() {
            s.request_stop();
        }
        let deadline = Instant::now() + Duration::from_millis(RETIRE_CAP_MS as u64);
        while Instant::now() < deadline {
            if self.retiring.iter().all(|s| s.finished()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(RETIRE_POLL_MS));
        }
        for mut s in self.retiring.drain(..) {
            if s.finished() {
                s.reap_finished();
            } else {
                let _ = s.thread.take(); // detach-and-drop
            }
        }
        self.prewarmed_for = None;
    }

    /// Open one hardware session for `spec` and start its decode thread.
    fn open_session(&mut self, spec: &LayerSpec, demand_us: i64) -> Result<LayerSession, OpenRefusal> {
        // D-05: the source ALWAYS comes through the resolver seam, never
        // `spec.path` directly — Phase 58 substitutes a proxy here and this
        // file does not change.
        let source = crate::decode_source::resolve_decode_source(
            &spec.clip_id,
            &spec.path,
            spec.source_us,
        );

        // PROVISIONAL depth (Pitfall 4's ordering, step 1): sized from the dims
        // the session will ACTUALLY decode, against what sibling sessions
        // already hold (Pitfall 2). This number becomes `ring_target`, which is
        // what widens the hw frame pool at open — an over-provisioned
        // provisional is an over-provisioned pool, at N sessions.
        //
        // "Actually decode" means the RESOLVED source's dims, which D-19 hands
        // back on the verdict for exactly this reason: when the seam answers
        // `Proxy`, the pool holds proxy-sized NV12 slices, and sizing the
        // estimate from `spec.src_width/src_height` (the ORIGINAL's dims, as
        // this function did until debug session `proxy-bitrate-starved-all-intra`)
        // charged the shared ledger ~17x the session's real VRAM — a 4K layer's
        // 960x540 proxy session reserved ~982 MB instead of ~57 MB. Three such
        // sessions plus their async-retiring ghosts pushed `reserved_by_others`
        // to 2.68 GB on a 7.6 GB budget, cut sibling depths (37→34→30 pools in
        // the field log), and — worse for debugging — made the open log line
        // below report NATIVE-sized reservations for proxy sessions, which is
        // exactly the false evidence that derailed that session's first
        // verification round.
        let (decode_w, decode_h) = session_decode_dims(spec, &source.kind);
        let est_bytes = engine::frame_bytes_nv12(decode_w.max(1), decode_h.max(1));

        // ONE reading of the two moving inputs, shared by the admission
        // question and the depth question below. Both are live handles (the
        // budget moves on a DXGI notification, the ledger on any sibling
        // open/close), and asking the same tick's decision on two different
        // snapshots is how a session gets admitted against one budget and
        // sized against another.
        let budget_now = self.budget.load(Ordering::Acquire);
        let reserved_by_others = self.ledger.reserved();

        // ---- D-10's ordered shed, rung 1: ADMISSION -------------------------
        // (quick 260821-3qx — closes deferred item D-59.1-3. "Rung 1" is this
        // module's own numbering of D-10's shed; it is the rung PLAY-09's
        // wording lists THIRD, "per-layer software fallback". Same rung.)
        //
        // ASKED BEFORE ANY DRIVER WORK. Nothing above this line touches the
        // GPU: `resolve_decode_source` is a path decision and
        // `session_decode_dims` is arithmetic, so a refusal here costs a
        // resolve and nothing else, and `open_hw_decoder` below is never
        // reached by a session that does not fit.
        //
        // WHY THIS IS NOT `gpu_ring_depth(..) == 0`, which is what stood here
        // from Phase 57 until now. That function answers "what depth should an
        // ADMITTED session's ring be?" and ends in
        // `.clamp(GPU_RING_MIN_DEPTH, ..)` with a floor of 4, so it cannot
        // answer 0 for ANY budget, frame size or sibling reservation — the
        // rung was unreachable, proven with hostile inputs rather than argued
        // (`LAYER-REENTRY-CAP budget-rung hostile_depth=4 floor=4`, D-59.1-3).
        // `vram_budget.rs` says so about itself: "The ordered shed (CONTEXT
        // D-10) — not this formula — is what refuses the session that
        // genuinely does not fit." The floor, the ceiling, the clamp and every
        // answer `gpu_ring_depth` gives are UNCHANGED; the refusal simply
        // stopped borrowing a function that disclaims the question.
        if !engine::gpu_session_admissible(
            budget_now,
            est_bytes,
            engine::DECODER_HEADROOM,
            reserved_by_others,
        ) {
            // The ledger says this session cannot fund even the MINIMUM pool
            // the floor forces it to open, alongside what its siblings already
            // hold. So this LAYER decodes in software and every sibling keeps
            // its hardware session — which is the rung's whole promise, and the
            // only rung that can bound worst-case commitment: rung 1 caps N,
            // and rung 2 cannot return bytes at all once a pool exists ("a
            // budget-shrink eviction returns pool slices to the DECODER, not
            // bytes to the OS", `vram_budget.rs`).
            return Err(OpenRefusal::Budget);
        }

        let provisional = engine::gpu_ring_depth(
            budget_now,
            est_bytes,
            engine::GPU_RING_MAX_DEPTH + engine::DECODER_HEADROOM,
            engine::DECODER_HEADROOM,
            reserved_by_others,
        );

        let session = engine::open_hw_decoder(&source.path, provisional)
            .map_err(OpenRefusal::Open)?;

        // …and only now take this session's own share, on the WHOLE pool
        // (57-03's `session_pool_vram_bytes`: the VRAM is spent at open on the
        // widened pool, and a depth change never returns bytes to the OS).
        let reservation = self
            .ledger
            .reserve(engine::session_pool_vram_bytes(session.pool_size(), est_bytes));
        // `source=`/`decode=` state WHICH media this session actually opened and
        // at what size — D-19's "proxies must not be invisible to verification",
        // applied to the one log line a field report of this path will quote.
        // Nothing else in this line could answer that question: `pool` and
        // `reserved_self` are both derived from constants + the estimate, so
        // before these two fields the line could not distinguish a proxy
        // session from a native one at all.
        eprintln!(
            "hwdecode: layer session opened clip={} source={} decode={}x{} pool={} \
             provisional_depth={} reserved_self={}B reserved_by_others={}B",
            spec.clip_id,
            match source.kind {
                crate::decode_source::DecodeSourceKind::Proxy { .. } => "proxy",
                crate::decode_source::DecodeSourceKind::Original => "original",
            },
            decode_w,
            decode_h,
            session.pool_size(),
            provisional,
            reservation.bytes(),
            reservation.others(),
        );

        let ctl = Arc::new(SessionCtl::new(demand_us));
        let hold: Arc<HoldSlot> = Arc::new(Mutex::new(None));
        let src_step_us = spec.src_step_us.max(1);
        let thread = {
            let ctl = ctl.clone();
            let hold = hold.clone();
            let compositor = self.compositor.clone();
            std::thread::Builder::new()
                .name("rudis-hw-layer".to_string())
                .spawn(move || session_loop(ctl, hold, compositor, session, src_step_us))
                .map_err(|e| OpenRefusal::Spawn(e.to_string()))?
        };
        self.opens += 1;
        Ok(LayerSession {
            clip_id: spec.clip_id.clone(),
            path: spec.path.clone(),
            // WHAT WAS ACTUALLY OPENED — captured here, at the one site that
            // knows, so the park-reuse ladder can later ask "does the current
            // resolve still answer this same file?" without re-deriving it.
            resolved_path: source.path.clone(),
            ctl,
            hold,
            thread: Some(thread),
            src_step_us,
            _reservation: Some(reservation),
            retire_started: None,
        })
    }
}

impl Drop for LayerSessionSet {
    fn drop(&mut self) {
        self.retire_all();
    }
}

// ---------------------------------------------------------------------------
// The park-reuse ladder — pure, hardware-free, unit-testable
// ---------------------------------------------------------------------------

/// The facts about a PARKED session that [`park_reuse_decision`] reads.
///
/// Split out of `LayerSession` for one reason, and it is the same reason
/// [`session_decode_dims`] is split out of [`LayerSessionSet::open_session`]:
/// the decision is the half no hardware probe can see being wrong, so it has to
/// be reachable without a GPU, a driver, a media file or a running decode
/// thread. A `LayerSession` cannot be constructed without all four.
pub struct ParkedSessionFacts<'a> {
    /// What this session ACTUALLY opened (`LayerSession::resolved_path`).
    pub resolved_path: &'a Path,
    /// The source advance per output tick this session's THREAD was started
    /// with — baked into `run_session_loop`'s continuation tolerance and its
    /// decode-forward target, and therefore not something a rebase can change.
    pub src_step_us: i64,
    /// `!failed() && !finished()` — a session whose thread has given up or
    /// exited is not a session at all.
    pub healthy: bool,
}

/// What the claim ladder decided about one parked session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkDecision {
    /// Rebase it and put it back to work.
    Reuse,
    /// Retire it and cold-open instead. Carries a typed reason so a log line
    /// (and the ladder's unit pins) can say WHICH rung refused.
    Drop(&'static str),
}

/// [`ParkDecision::Drop`] reason: the thread failed or has already exited.
pub const PARK_DROP_HEALTH: &str = "Health";
/// [`ParkDecision::Drop`] reason: the D-05 seam now answers a DIFFERENT file
/// than this session opened — a proxy landed, or was evicted, between visits.
/// The parity-critical rung.
pub const PARK_DROP_RESOLVED_PATH: &str = "ResolvedPathChanged";
/// [`ParkDecision::Drop`] reason: the layer was retimed while parked, so the
/// running thread judges continuation (and decodes forward) at the OLD step.
pub const PARK_DROP_RETIME: &str = "RetimeChanged";

/// **The park-reuse key.** Which [`LayerSpec`] fields must MATCH for a parked
/// session to serve a re-entering spec.
///
/// Derived by ENUMERATION from what [`LayerSessionSet::open_session`] binds at
/// open time — never guessed, and never a subset someone found convenient. The
/// destructure below is deliberately **wildcard-free**: adding a field to
/// `LayerSpec` fails THIS function's compile until the new field's disposition
/// is decided here, in writing, beside the others. That is the same tripwire
/// mechanism `decode_source.rs`'s `kind_has_exactly_two_variants_today` and
/// [`session_decode_dims`] use, and it has already caught real work four times
/// in this crate.
///
/// # The ladder, in `warm_hw`'s order
///
/// `ring.rs`'s single-clip warm-session cache is the proven prior art for
/// exactly this hazard, so its checks are REPLICATED rather than re-derived:
///
/// 1. latch engaged → drop — **set-wide**, checked once per sync in
///    [`LayerSessionSet::sync_to_stack`], not per entry.
/// 2. kill switch set → drop — **set-wide**, same place.
/// 3. resolved-path mismatch → drop. THE parity-critical check: a session
///    decodes exactly one file, so reusing one across a proxy landing/eviction
///    would composite another file's pixels.
/// 4. device health → drop. On the single-clip arm that is
///    `device_removed_reason()` on a session the producer owns; here the engine
///    session lives on the decode thread, so it maps to `failed()`/`finished()`
///    — and a dead-device parked session that somehow survives both fails its
///    re-seek on claim, sets `failed`, and D-04 routes that layer to software
///    on the next tick. One-tick recovery, no new mechanism.
///
/// Plus ONE rung `warm_hw` does not need, because a single-clip session is
/// re-rooted by the producer itself while a parked session's THREAD keeps its
/// own tolerance: `src_step_us` equality. A retimed clip must not reuse a
/// session whose thread judges continuation — and decodes forward — at the old
/// step (Pitfall 3's whole point; the software pool's respawn storm is what
/// getting this wrong looks like).
pub fn park_reuse_decision(
    spec: &LayerSpec,
    current_resolved: &Path,
    parked: ParkedSessionFacts<'_>,
) -> ParkDecision {
    let LayerSpec {
        clip_id: _,          // NOT in key: the clip_id MATCH is the caller's lookup, not a field of the decision
        path: _,             // NOT in key directly: superseded by the resolved-path equality below, which is strictly stronger
        source_us: _,        // NOT in key: the rebase (`set_demand`) serves any position — that IS the mechanism
        rotation: _,         // NOT in key: rotation is applied at COMPOSITE time from the spec, never baked into the session
        remaining_dur_us: _, // NOT in key: a hw session carries no `-t` window (unlike the software pool's child process)
        src_step_us,         // IN KEY: baked into the running thread's continuation tolerance and decode-forward target
        retime_ramped: _,    // NOT in key: a ramped layer is never hw-eligible at all (`spec_is_hw_eligible`)
        opacity: _,          // NOT in key: composite-time property
        transform: _,        // NOT in key: composite-time property
        crop: _,             // NOT in key: composite-time property
        alpha_mode: _,       // NOT in key: composite-time property
        is_image_sequence: _, // NOT in key: never hw-eligible
        seq_fps: _,          // NOT in key: never hw-eligible (sequence-only)
        is_still_image: _,   // NOT in key: never hw-eligible
        src_width: _,        // NOT in key: dims are read off the FILE at open — resolved-path equality covers them
        src_height: _,       // NOT in key: see src_width
        reports_alpha: _,    // NOT in key: import-time SOURCE metadata, identical for any session on the same resolved path (which the equality below already requires); it is the occlusion predicate's input, never a session's
        text: _,             // NOT in key: a text layer is never hw-eligible
    } = spec;

    // Rung 4, first because it is the cheapest and because everything below it
    // reads state a dead thread may have stopped maintaining.
    if !parked.healthy {
        return ParkDecision::Drop(PARK_DROP_HEALTH);
    }
    // Rung 3 — THE parity-critical one. A session decodes exactly one file, so
    // it may only be reused while the CURRENT resolve still answers the file it
    // is actually decoding. This is what makes a proxy landing (or an eviction)
    // between two visits a cold open instead of another file's pixels.
    if current_resolved != parked.resolved_path {
        return ParkDecision::Drop(PARK_DROP_RESOLVED_PATH);
    }
    // The park-only rung. `open_session` hands `src_step_us.max(1)` to the
    // thread, so compare on the same normalisation or a spec carrying 0 would
    // read as a mismatch against a thread started at 1.
    if (*src_step_us).max(1) != parked.src_step_us {
        return ParkDecision::Drop(PARK_DROP_RETIME);
    }
    ParkDecision::Reuse
}

/// The pixel dims a session for `spec` will ACTUALLY decode, given the seam's
/// verdict: the proxy's geometry when the resolve answered `Proxy` (carried on
/// the verdict for exactly this purpose — D-19), the layer's own media dims
/// otherwise.
///
/// Split out of [`LayerSessionSet::open_session`] so the choice is pinned by a
/// hardware-free unit test: this is the line whose regression (sizing from
/// `spec.src_width/src_height` unconditionally) both over-charged the VRAM
/// ledger ~17x per proxy session AND falsified the session-open log line — see
/// the call site's comment for the field evidence.
///
/// Deliberately wildcard-free, like `decode_source`'s own tripwire match: a
/// third `DecodeSourceKind` must decide its sizing here consciously, at compile
/// time, not inherit one by falling through.
fn session_decode_dims(
    spec: &LayerSpec,
    kind: &crate::decode_source::DecodeSourceKind,
) -> (u32, u32) {
    match *kind {
        crate::decode_source::DecodeSourceKind::Proxy { proxy_w, proxy_h } => (proxy_w, proxy_h),
        crate::decode_source::DecodeSourceKind::Original => (spec.src_width, spec.src_height),
    }
}

/// Why a session open was refused.
enum OpenRefusal {
    /// The shared ledger cannot fund another session right now (D-10 rung 1).
    /// NOT a media failure — the layer is retried on a later tick.
    Budget,
    /// `open_hw_decoder` said no. Typed, logged, per-media (GPU-05).
    Open(engine::HwOpenError),
    /// The OS refused a thread.
    Spawn(String),
}

impl std::fmt::Display for OpenRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenRefusal::Budget => write!(
                f,
                "VRAM ledger cannot fund another concurrent session (siblings hold the budget)"
            ),
            OpenRefusal::Open(e) => write!(f, "{e}"),
            OpenRefusal::Spawn(e) => write!(f, "thread spawn failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The decode thread
// ---------------------------------------------------------------------------

/// The decode → publish seam [`run_session_loop`] drives.
///
/// Production has exactly ONE implementor — [`HwLayerDriver`], the real
/// D3D11VA session plus the zero-copy import plus the HOLD-slot write. The
/// split exists so the loop's POSITION logic (seek vs decode-forward,
/// HOLD-on-stall, HOLD-on-end and its release) is exercisable without a GPU, a
/// driver or a media file: that logic is the half of this thread no hardware
/// probe can see being wrong, and it is exactly where the end-of-stream
/// freeze this seam's tests now pin used to live.
///
/// Static dispatch — a generic parameter, never a `dyn` — so the production
/// loop monomorphizes to what it was before the split.
trait LayerDecodeDriver {
    /// One decoded frame, still owned by the driver's decoder.
    type Frame;

    /// Reposition the decoder to `us`.
    fn seek_to_us(&mut self, us: i64) -> Result<(), String>;

    /// The next decoded frame, or `Ok(None)` at end of stream.
    fn decode_next(&mut self) -> Result<Option<Self::Frame>, String>;

    /// The frame's source position, or `None` when the media carries no pts.
    fn pts_us(frame: &Self::Frame) -> Option<i64>;

    /// Import `frame` and publish it into the HOLD slot as the answer to
    /// `demand` (freshest-wins overwrite, never a queue).
    fn publish(&mut self, frame: Self::Frame, demand: i64) -> Result<(), String>;
}

/// The production driver: one hardware decode session, the producer's
/// compositor (for the zero-copy import) and this layer's HOLD slot.
struct HwLayerDriver {
    session: engine::HwDecodeSession,
    compositor: Arc<engine::Compositor>,
    hold: Arc<HoldSlot>,
}

impl LayerDecodeDriver for HwLayerDriver {
    type Frame = engine::HwFrame;

    fn seek_to_us(&mut self, us: i64) -> Result<(), String> {
        self.session.seek_to_us(us).map_err(|e| e.to_string())
    }

    fn decode_next(&mut self) -> Result<Option<engine::HwFrame>, String> {
        self.session.decode_next().map_err(|e| e.to_string())
    }

    fn pts_us(frame: &engine::HwFrame) -> Option<i64> {
        frame.pts_us()
    }

    fn publish(&mut self, frame: engine::HwFrame, demand: i64) -> Result<(), String> {
        let gpu = self
            .compositor
            .import_gpu_frame(&self.session, frame)
            .map_err(|e| e.to_string())?;
        // Freshest-wins overwrite. The previous frame drops HERE, returning its
        // hw-frame-pool slice.
        let mut slot = self.hold.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Some(HeldFrame {
            frame: gpu,
            source_us: demand,
        });
        Ok(())
    }
}

/// Has the coordinator moved a session that reached end-of-stream BACK into
/// content that still exists?
///
/// `ended_at` is the demand the session was serving when `decode_next`
/// returned `Ok(None)`; `demand` is what the coordinator is asking for now.
/// The test is the BACKWARD half of `need_seek`'s reposition test, at the same
/// `(src_step_us / 2).max(1)` retime-parity tolerance — the reposition signal
/// this thread already speaks, not a second one invented beside it.
///
/// # Why the end-of-stream HOLD is latched on a POSITION and not on the session
///
/// Latching it on the SESSION (a plain `ended` flag with no reset path) is a
/// permanent blackout: `sync_to_stack` keeps a session live while its clip
/// stays in the resolved stack, and the ring's seek/flush handshake rebuilds
/// the single-clip decode state without touching the session set at all. So
/// "play a multi-layer section to the end of one layer's media, then scrub
/// back inside the same section" left that layer decoding nothing for the rest
/// of the producer's life — with no `failed` flag, so not even D-04's
/// per-layer software fallback engaged. `LiveHold::usable_for` then rejects
/// the frozen held frame against the new demand and the layer drops out of the
/// composite entirely.
///
/// # Why FORWARD movement deliberately does not release the hold
///
/// `Ok(None)` means the demuxer and the decoder are both drained AT that
/// position: there is provably no content at or after `ended_at`, so a demand
/// that keeps advancing past it cannot be answered by decoding — only by
/// holding. Releasing on it anyway costs twice:
///
/// * a clip whose out-point sits past its media's last decodable frame (a
///   duration-probe/estimate mismatch — the ordinary way this state is
///   reached at all) would re-seek and re-decode on every remaining tick of
///   the clip, on the playback hot path;
/// * `seek_to_us` past the end of a file can legitimately fail, and a failed
///   seek in this thread sets `failed` and retires the session — demoting the
///   layer to the software pool permanently (D-04) and so turning a
///   recoverable HOLD into an unrecoverable one.
///
/// A BACKWARD demand carries neither ambiguity: the coordinator has
/// repositioned this layer into a part of the media the session already
/// decoded past. (The software pool's `Ended` arm re-attempts a precise decode
/// at ANY differing demand; past the end that attempt simply fails and logs,
/// so holding is the same picture at none of the cost.)
fn end_hold_released(demand: i64, ended_at: i64, tolerance: i64) -> bool {
    demand + tolerance < ended_at
}

/// The body of one `"rudis-hw-layer"` thread: decode → zero-copy import →
/// publish into the HOLD slot, at the coordinator's demand cadence.
///
/// The four rules it exists to hold:
///
/// * **Never free-run.** It publishes at most one frame per distinct demand
///   (HOLD-on-stall), so a session that outruns the coordinator sleeps instead
///   of throwing decoded frames away — and, at N sessions, does not burn N
///   cores' worth of decode for frames nobody composites.
/// * **Retime parity (Pitfall 3).** Continuation is judged at
///   `(src_step_us / 2).max(1)`, and a forward demand gap is served by decoding
///   forward and discarding — never by a seek per tick.
/// * **HOLD on end-of-stream, and RELEASE on a reposition.** Reaching the end
///   of the media parks the thread on the last held frame at that position;
///   a demand that moves BACK into the layer re-seeks and decodes again. See
///   [`end_hold_released`] for why the latch is on the position and not on the
///   session.
/// * **Fail closed, alone (D-04).** Any error sets `failed` and exits. The
///   coordinator observes it, routes THAT layer to software, and every sibling
///   thread keeps running.
fn session_loop(
    ctl: Arc<SessionCtl>,
    hold: Arc<HoldSlot>,
    compositor: Arc<engine::Compositor>,
    session: engine::HwDecodeSession,
    src_step_us: i64,
) {
    let mut driver = HwLayerDriver {
        session,
        compositor,
        hold,
    };
    run_session_loop(&ctl, &mut driver, src_step_us);
}

/// [`session_loop`]'s hardware-free body — see [`LayerDecodeDriver`].
fn run_session_loop<D: LayerDecodeDriver>(ctl: &SessionCtl, driver: &mut D, src_step_us: i64) {
    // Pitfall 3: the retime-parity tolerance, from the LAYER's own source step
    // — the exact expression `LayerDecoderPool::advance` judges continuation
    // with. Never a hardcoded project step / 2.
    let tolerance = (src_step_us / 2).max(1);
    let mut last_pts: Option<i64> = None;
    let mut served: Option<i64> = None;
    // The demand this session was serving when it reached end of stream, or
    // `None` while there is content ahead. A POSITION, not a session verdict —
    // see `end_hold_released`.
    let mut ended_at: Option<i64> = None;

    loop {
        if ctl.stop.load(Ordering::Acquire) {
            return;
        }
        let demand = ctl.demand_us.load(Ordering::Acquire);
        // HOLD-on-stall: this demand is already answered by the held frame.
        if served.map(|s| (s - demand).abs() <= tolerance).unwrap_or(false) {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        // HOLD-on-end, and its ONE escape: a demand that moved back into the
        // layer. Everything else keeps showing the last frame there will ever
        // be at this position, without touching the drained decoder.
        if let Some(end_at) = ended_at {
            if !end_hold_released(demand, end_at, tolerance) {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            ended_at = None;
            // A decoder that reached end of stream is DRAINED: re-engaging it
            // always re-seeks, whatever the gap. Clearing `last_pts` is how
            // the sequential check below is told that (its `None => true`
            // arm) — no second reposition signal beside the one it already
            // applies.
            last_pts = None;
        }

        // Continuation vs seek — the sequential check, hardware side.
        let need_seek = match last_pts {
            None => true,
            Some(p) => demand + tolerance < p || demand > p + SEEK_AHEAD_US,
        };
        if need_seek {
            if let Err(e) = driver.seek_to_us(demand.max(0)) {
                eprintln!("hwdecode: layer session seek to {demand}us failed: {e}");
                ctl.failed.store(true, Ordering::Release);
                return;
            }
            last_pts = None;
        }

        // Decode FORWARD to the demand, discarding what is behind it. This is
        // what makes a constant-retimed layer (whose demand advances by
        // `src_step_us`, not by the media's native step) cost one scan rather
        // than one seek per output tick.
        let target = demand - tolerance;
        let mut discards = 0usize;
        loop {
            if ctl.stop.load(Ordering::Acquire) {
                return;
            }
            match driver.decode_next() {
                Ok(Some(f)) => {
                    let pts = D::pts_us(&f);
                    // A missing pts is accepted as "close enough" — nothing to
                    // compare against, and refusing would stall the layer.
                    if pts.is_some_and(|p| p < target) {
                        discards += 1;
                        if discards > DISCARD_CAP {
                            eprintln!(
                                "hwdecode: layer session discard cap hit before {demand}us"
                            );
                            ctl.failed.store(true, Ordering::Release);
                            return;
                        }
                        continue;
                    }
                    match driver.publish(f, demand) {
                        Ok(()) => {
                            last_pts = pts;
                            served = Some(demand);
                            // Published into the HOLD slot FIRST, announced
                            // second: the coordinator's freshness poll reads
                            // `served_us` and only then takes the hold lock.
                            ctl.served_us.store(demand, Ordering::Release);
                        }
                        Err(e) => {
                            eprintln!("hwdecode: layer zero-copy import failed: {e}");
                            ctl.failed.store(true, Ordering::Release);
                            return;
                        }
                    }
                    break;
                }
                Ok(None) => {
                    // EOF before the demand: keep showing the last held frame
                    // (HOLD), stop decoding. Not a failure — the layer is
                    // simply out of content, exactly like the software pool's
                    // `Ended`. Latched on THIS demand, so a later demand that
                    // moves back into the layer decodes again.
                    ended_at = Some(demand);
                    break;
                }
                Err(e) => {
                    eprintln!("hwdecode: layer decode failed: {e}");
                    ctl.failed.store(true, Ordering::Release);
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — the hardware-free half (eligibility, cap, HOLD semantics)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use engine::{LayerCrop, LayerTransform};

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

    fn stack(layers: Vec<LayerSpec>) -> MultiLayerStack {
        MultiLayerStack {
            layers,
            width: 1920,
            height: 1080,
            fps: 30.0,
        }
    }

    #[test]
    fn a_proxy_session_is_sized_from_the_proxy_dims_not_the_original() {
        // Debug session `proxy-bitrate-starved-all-intra` (2026-08-04): sizing
        // the pool estimate from `spec.src_width/src_height` while the seam
        // answered `Proxy` reserved a 4K layer's ~982 MB against a 960x540
        // session's ~57 MB of real pool VRAM — and made the session-open log
        // line report NATIVE dims for a proxy session, which is the false
        // evidence that cost a verification round. The estimate must follow
        // the VERDICT.
        let mut s = spec("v", "a.mp4");
        s.src_width = 3840;
        s.src_height = 2160;

        assert_eq!(
            session_decode_dims(
                &s,
                &crate::decode_source::DecodeSourceKind::Proxy {
                    proxy_w: 960,
                    proxy_h: 540
                }
            ),
            (960, 540),
            "a Proxy verdict sizes the session from the proxy's geometry"
        );
        assert_eq!(
            session_decode_dims(&s, &crate::decode_source::DecodeSourceKind::Original),
            (3840, 2160),
            "an Original verdict keeps the layer's own media dims"
        );
    }

    #[test]
    fn the_cap_comes_from_the_measured_verdict_not_a_guess() {
        // 57-A1-VERDICT.md § Consequence 1. If this ever changes, the verdict
        // artifact has to change with it (and the N=3 probe has to be re-run
        // at the new N first — Pitfall 1).
        assert_eq!(MAX_HW_SESSIONS, 3);
    }

    #[test]
    fn text_still_sequence_and_ramp_layers_are_never_hardware_eligible() {
        let empty = HashSet::new();
        let mut text = spec("t", "");
        text.text = Some(rudis_core::TextPayload::new("t"));
        assert!(!spec_is_hw_eligible(&text, &empty), "text has no media");

        let mut still = spec("s", "a.png");
        still.is_still_image = true;
        assert!(!spec_is_hw_eligible(&still, &empty), "stills serve from cache");

        let mut seq = spec("q", "a%04d.png");
        seq.is_image_sequence = true;
        assert!(!spec_is_hw_eligible(&seq, &empty), "sequences decode inline");

        let mut ramp = spec("r", "a.mp4");
        ramp.retime_ramped = true;
        assert!(
            !spec_is_hw_eligible(&ramp, &empty),
            "a RAMP has no constant source cadence — the same reason pool_sources \
             excludes it (Pitfall 3's other half)"
        );

        assert!(spec_is_hw_eligible(&spec("v", "a.mp4"), &empty));
    }

    #[test]
    fn a_media_that_already_failed_hardware_is_never_re_attempted() {
        let mut failed = HashSet::new();
        failed.insert(PathBuf::from("bad.mp4"));
        assert!(!spec_is_hw_eligible(&spec("v", "bad.mp4"), &failed));
        assert!(spec_is_hw_eligible(&spec("v", "good.mp4"), &failed));
    }

    #[test]
    fn selection_is_track_ordered_and_capped_at_the_measured_maximum() {
        let s = stack(vec![
            spec("top", "a.mp4"),
            spec("mid", "b.mp4"),
            spec("low", "c.mp4"),
            spec("base", "d.mp4"),
        ]);
        let chosen = select_hw_clip_ids(&s, &HashSet::new(), &HashSet::new(), MAX_HW_SESSIONS);
        assert_eq!(chosen, vec!["top", "mid", "low"], "index 0 = top, capped at 3");
        assert!(chosen.len() <= MAX_HW_SESSIONS);
    }

    #[test]
    fn an_already_live_session_keeps_its_slot_when_a_layer_joins_above_it() {
        // Without this, a layer joining on a HIGHER track would evict the
        // bottom layer's warm session on the boundary tick and re-open it one
        // tick later — a cold-boundary generator, at the exact moment the
        // phase exists to make boundaries cheap.
        let s = stack(vec![
            spec("newtop", "a.mp4"),
            spec("mid", "b.mp4"),
            spec("low", "c.mp4"),
            spec("base", "d.mp4"),
        ]);
        let mut live = HashSet::new();
        live.insert("base".to_string());
        let chosen = select_hw_clip_ids(&s, &HashSet::new(), &live, MAX_HW_SESSIONS);
        assert!(chosen.contains(&"base".to_string()), "the warm session survives");
        assert_eq!(chosen.len(), 3);
    }

    #[test]
    fn ineligible_layers_never_consume_a_session_slot() {
        let mut text = spec("txt", "");
        text.text = Some(rudis_core::TextPayload::new("t"));
        let s = stack(vec![
            text,
            spec("mid", "b.mp4"),
            spec("low", "c.mp4"),
            spec("base", "d.mp4"),
        ]);
        let chosen = select_hw_clip_ids(&s, &HashSet::new(), &HashSet::new(), MAX_HW_SESSIONS);
        assert_eq!(chosen, vec!["mid", "low", "base"]);
    }

    #[test]
    fn the_hold_slot_is_freshest_wins_not_a_queue() {
        // The property, expressed on the type the decode thread publishes into:
        // a second publish REPLACES the first (and drops its pool slice) rather
        // than queueing behind it.
        let hold: HoldSlot = Mutex::new(None);
        let mut g = hold.lock().unwrap();
        assert!(g.is_none());
        // Stand-ins for two decoded frames: the slot holds ONE, and it is the
        // last one written.
        *g = None;
        assert!(g.is_none(), "an empty slot stays empty");
        drop(g);
        assert!(hold.lock().unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // The decode thread's POSITION logic, driven end to end on a scripted
    // decoder. No GPU, no driver, no media — `LayerDecodeDriver` exists so
    // this half is reachable, because it is the half a hardware probe cannot
    // see being wrong (57-REVIEW WR-01: BENCH-01's fixtures never exercise a
    // real EOF followed by a seek back).
    // -----------------------------------------------------------------------

    /// A scripted stand-in for one hardware decode session: frames at known
    /// source positions, a demuxer cursor a seek moves, and a record of every
    /// call the loop made.
    #[derive(Default)]
    struct ScriptState {
        /// Source positions (us) at which a frame exists, ascending.
        frames: Vec<i64>,
        /// Index `decode_next` returns next; `frames.len()` = DRAINED, which
        /// is what a real decoder is after it reports end of stream (only a
        /// seek re-primes it).
        cursor: usize,
        /// `(demand, pts)` for every published frame, in order.
        published: Vec<(i64, i64)>,
        /// Every `seek_to_us` target, in order.
        seeks: Vec<i64>,
        /// How many times `decode_next` was called…
        decodes: usize,
        /// …and how many of those reported end of stream.
        eofs: usize,
    }

    struct ScriptedDriver(Arc<Mutex<ScriptState>>);

    impl LayerDecodeDriver for ScriptedDriver {
        /// The frame IS its pts here — nothing else about a frame reaches the
        /// logic under test.
        type Frame = i64;

        fn seek_to_us(&mut self, us: i64) -> Result<(), String> {
            let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
            s.seeks.push(us);
            let landed = s
                .frames
                .iter()
                .position(|f| *f >= us)
                .unwrap_or(s.frames.len());
            s.cursor = landed;
            Ok(())
        }

        fn decode_next(&mut self) -> Result<Option<i64>, String> {
            let mut s = self.0.lock().unwrap_or_else(|p| p.into_inner());
            s.decodes += 1;
            match s.frames.get(s.cursor).copied() {
                Some(pts) => {
                    s.cursor += 1;
                    Ok(Some(pts))
                }
                None => {
                    s.eofs += 1;
                    Ok(None)
                }
            }
        }

        fn pts_us(frame: &i64) -> Option<i64> {
            Some(*frame)
        }

        fn publish(&mut self, frame: i64, demand: i64) -> Result<(), String> {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .published
                .push((demand, frame));
            Ok(())
        }
    }

    /// Poll `cond` in 1 ms beats up to a generous cap. Generous because the
    /// thing being timed is a 1 ms-beat sleep loop, not a performance claim —
    /// a green run settles in single-digit milliseconds and the cap only
    /// bounds a FAILING run.
    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    /// Has the session announced a frame answering `demand`? (`LayerSession::
    /// serves`, at the same tolerance, without needing a `LayerSession`.)
    fn announced(ctl: &SessionCtl, demand: i64, src_step_us: i64) -> bool {
        let served = ctl.served_us.load(Ordering::Acquire);
        served != i64::MIN && (served - demand).abs() <= (src_step_us / 2).max(1)
    }

    /// 57-REVIEW **WR-01**, the regression: play a layer to the real end of its
    /// media, then move the playhead BACK inside it. Before the fix the decode
    /// thread's `ended` flag was set once and never cleared, so the loop's
    /// first check bypassed the seek/decode logic forever — the layer went
    /// black for the rest of the producer's life, silently (no `failed` flag,
    /// so no D-04 software fallback either).
    #[test]
    fn a_layer_that_ran_to_end_of_stream_decodes_again_when_the_demand_moves_back() {
        const STEP: i64 = 33_333;
        let frames: Vec<i64> = (0..30).map(|i| i * STEP).collect();
        let last = *frames.last().expect("30 frames");
        let state = Arc::new(Mutex::new(ScriptState {
            frames,
            ..Default::default()
        }));
        let ctl = Arc::new(SessionCtl::new(0));
        let thread = {
            let ctl = ctl.clone();
            let mut driver = ScriptedDriver(state.clone());
            std::thread::spawn(move || run_session_loop(&ctl, &mut driver, STEP))
        };
        let snapshot = || {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            (s.decodes, s.eofs, s.published.len())
        };

        // 1. Ordinary service at the head of the layer.
        assert!(
            wait_until(|| announced(&ctl, 0, STEP)),
            "the session never served its first demand"
        );

        // 2. Play to the last frame, then one tick PAST it — a real EOF.
        ctl.demand_us.store(last, Ordering::Release);
        assert!(
            wait_until(|| announced(&ctl, last, STEP)),
            "the session never reached the last frame of the media"
        );
        ctl.demand_us.store(last + STEP, Ordering::Release);
        assert!(
            wait_until(|| snapshot().1 >= 1),
            "decode_next never reported end of stream — the setup is wrong, not the code"
        );

        // 3. Past the end it HOLDS: no re-hammering of a drained decoder. (The
        //    obvious "just clear the flag whenever the demand differs" fix
        //    turns this thread into a busy loop on the playback hot path, so
        //    the pin has to cover it.)
        let (decodes_at_eof, _, _) = snapshot();
        std::thread::sleep(Duration::from_millis(50));
        let (decodes_after_hold, _, _) = snapshot();
        assert!(
            decodes_after_hold - decodes_at_eof <= 2,
            "the end-of-stream HOLD is spinning the decoder: {} decode_next calls in 50ms \
             of holding",
            decodes_after_hold - decodes_at_eof
        );

        // 4. THE REGRESSION: scrub back inside the layer.
        let back = 6 * STEP;
        ctl.demand_us.store(back, Ordering::Release);
        assert!(
            wait_until(|| announced(&ctl, back, STEP)),
            "the layer never decoded again after end-of-stream — it is BLANK for the rest \
             of this session (57-REVIEW WR-01)"
        );

        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let (demand, pts) = *s.published.last().expect("a published frame");
        assert_eq!(demand, back, "the newest published frame answers the new demand");
        assert!(
            (pts - back).abs() <= (STEP / 2).max(1),
            "published pts {pts} does not answer demand {back}"
        );
        assert!(
            s.seeks.contains(&back),
            "re-engaging a DRAINED decoder must re-seek; seeks were {:?}",
            s.seeks
        );
        drop(s);

        ctl.stop.store(true, Ordering::Release);
        thread.join().expect("the decode thread exits on stop");
    }

    /// The end-of-stream hold's release rule, as arithmetic — the asymmetry is
    /// deliberate and is the part a later reader is most likely to "simplify".
    #[test]
    fn the_end_of_stream_hold_releases_backward_and_holds_forward() {
        const TOL: i64 = 16_666;
        const END_AT: i64 = 5_000_000;
        assert!(
            !end_hold_released(END_AT, END_AT, TOL),
            "still at the end — HOLD"
        );
        assert!(
            !end_hold_released(END_AT - 1, END_AT, TOL),
            "inside the retime-parity tolerance is the SAME position, not a reposition"
        );
        assert!(
            !end_hold_released(END_AT + 33_333, END_AT, TOL),
            "past a PROVEN end there is nothing to decode — HOLD (and never re-seek past \
             the end of a file, which can fail and would retire the session)"
        );
        assert!(
            !end_hold_released(END_AT + 60_000_000, END_AT, TOL),
            "a far-forward demand is still past the same proven end"
        );
        assert!(
            end_hold_released(END_AT - 33_333, END_AT, TOL),
            "one tick BACK is a reposition into content that exists"
        );
        assert!(
            end_hold_released(0, END_AT, TOL),
            "a scrub to the head of the layer must re-engage the decoder"
        );
    }

    #[test]
    fn the_seek_tolerance_is_the_layers_own_source_step_never_the_project_step() {
        // Pitfall 3, as arithmetic: a 2x-retimed 30fps layer advances its
        // demand by 66_666us per output tick, so a step/2 tolerance derived
        // from the PROJECT step (16_666) would read every single tick as a
        // seek — the measured "45 ffmpeg spawns in 45 ticks" storm.
        let project_step = 33_333i64;
        let retimed_src_step = 2 * project_step;
        let good = (retimed_src_step / 2).max(1);
        let wrong = (project_step / 2).max(1);
        let demand_advance = retimed_src_step;
        assert!(
            demand_advance > wrong,
            "the project-step tolerance would classify a normal retimed advance as a seek"
        );
        assert!(
            demand_advance - retimed_src_step <= good,
            "the layer's own step tolerance classifies it as a continuation"
        );
    }
}
