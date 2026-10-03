//! Per-SEGMENT heaviness detection — CACHE-01's trigger (Phase 59, plan 59-05;
//! decisions D-11, D-12, D-13).
//!
//! This module answers ONE question: **which program-time segments have proved,
//! by measurement, that they cannot be composited live at real time?** 59-08's
//! background worker reads the answer through [`heavy_segments`]; 59-06 feeds
//! the evidence in through [`note_live_tick`] at the producer's existing
//! budget-comparison site.
//!
//! # Why this is NOT `dynres`, restated so it cannot blur (D-11, RQ3)
//!
//! [`crate::dynres::DynResController`] already scores every produced tick
//! against the frame budget, with hysteresis calibrated against this machine's
//! real noise. D-11 says to reuse that evidence — and research RQ3 says exactly
//! how to read that instruction:
//!
//! > D-11's "reuse the live budget-miss evidence" is best read as "reuse the
//! > SAME comparison, not the SAME controller instance."
//!
//! The controller answers a **per-SESSION** question ("what resolution should
//! THIS playback composite at right now?") with deliberately per-session
//! state — it is a stack local owned by `producer_loop`, it is `!Send` by
//! construction so it can never be shared or stored, and it snaps back to
//! `Full` on pause. CACHE-01 needs a **per-SEGMENT** answer that OUTLIVES the
//! session that measured it: a section identified as heavy on Monday should
//! still be a cache candidate on Tuesday without the user replaying it. Those
//! are two different lifetimes over the same input, so this is a second
//! accumulator fed the same numbers at the same call site — never an edit to
//! the controller, whose hysteresis is per-session on purpose.
//!
//! # The input contract: it MUST be the backpressure-subtracted number
//!
//! [`note_live_tick`]'s `effective_us` is the producer's `produce_us` MINUS the
//! time it spent parked on ring/target backpressure — the exact value
//! `ring.rs` already computes and hands `DynResController::on_tick`. Feeding
//! the RAW tick duration instead is not a rounding error, it is a known
//! failure: a producer that is KEEPING UP is by definition blocked waiting for
//! the presenter most of every tick, so a raw duration converges on one frame
//! step no matter how much headroom the machine has, and every segment on the
//! timeline would mark itself heavy. `57-DYNRES-CALIBRATION.md` records that
//! finding and 57-08 already paid to fix it once; a second detector that reads
//! the same input the wrong way re-discovers the same false positive.
//!
//! # D-13 — ticks served FROM the cache are EXCLUDED
//!
//! **Ticks served FROM the cache never reach this module.** A cached tick
//! measures the CACHE, not the SECTION: feeding it back would let a section
//! "recover" on paper the instant it was cached, which would un-mark it, stop
//! the worker refreshing it, and corrupt 59-10's calibration at the same time.
//!
//! Two halves enforce it. Here: [`note_live_tick`] is the ONLY inlet that can
//! move a miss streak, and `tests/render_cache_detect.rs` pins the module's
//! whole public surface so a second door cannot appear quietly. There: 59-06
//! gates the single call site so a cache-served tick returns before reaching
//! it, which is VALIDATION row 21's control-flow proof.
//!
//! # What this module deliberately does not know
//!
//! It never names the render-cache crate and never computes a segment index.
//! Callers hand [`note_live_tick`] and [`prearm_stack`] a `seg_index` they
//! already have, which keeps D-40's two-file rule — exactly one READER and one
//! WRITER in `crates/preview` may name that crate — a property of the code
//! rather than of anybody's restraint. Heat bookkeeping is not cache I/O.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// **D-12's pre-arm threshold.** A segment whose visible video layer count is
/// STRICTLY GREATER than this is marked a render candidate without waiting for
/// the user to play it.
///
/// The number is `crate::layer_sessions::MAX_HW_SESSIONS`, and it is that
/// number for a measured reason rather than a tidy one: every visible layer
/// past the cap falls onto `engine::decoder_pool`'s software pool, and
/// `artifacts/59-CEILING-VERDICT.md` § 3 measured exactly that split — 6 layers,
/// 3 hardware + 3 software, by the REAL selector's own answer — stalling
/// **992.3-1041.8 ms at every one of 10 boundary crossings** against a standing
/// worst-of-95 of 66.5 ms. The pre-arm exists so the background renderer can
/// start on that arm before the user has played it once.
///
/// **It is a pre-arm, not the trigger, and the verdict says why**: F5's steady
/// state is fine (a cut-free 6 s slice delivers 167/167 with zero drops). What
/// is not fine is the CUT. A predicate on layer count alone therefore arms
/// ranges that do not need caching, which is acceptable for a *pre*-arm and
/// would not be acceptable as the trigger. D-11's measured miss remains the
/// trigger.
///
/// Kept in lockstep with the real cap two ways: a compile-time assertion below
/// (on the configuration where the cap exists at all) and
/// `render_cache_detect_prearm_threshold_tracks_the_real_hw_cap`, so a cap
/// change forces a re-decision here instead of drifting silently.
pub const PREARM_LAYER_THRESHOLD: usize = 3;

// The compile-time half of the tripwire. Off Windows / without `hwdecode`
// there are no hardware sessions and therefore no cap to track, so the
// threshold keeps its own documented value; that is the honest state of
// affairs on such a build, not a hole.
#[cfg(all(windows, feature = "hwdecode"))]
const _: () = assert!(
    PREARM_LAYER_THRESHOLD == crate::layer_sessions::MAX_HW_SESSIONS,
    "D-12's pre-arm threshold is `visible video layers > MAX_HW_SESSIONS`; the \
     cap moved and the threshold did not"
);

/// How many CONSECUTIVE over-budget live ticks attributed to one segment mark
/// it heavy. One in-budget tick resets the run to zero.
///
/// **12, mirroring [`crate::dynres::MISS_STREAK_TICKS`], and the mirroring is
/// the point** — the two are scoring the same comparison on the same input at
/// the same call site, so a detector that demanded a different amount of
/// evidence than the controller would be making a second, unexplained claim
/// about the same machine.
///
/// The floor on the number is set by NOISE, not by taste: the frame-time
/// distribution on this machine is quantized by the ~15.6 ms Windows scheduler
/// tick, and 57-01 measured single frames past 33 ms on a fixture with 16x
/// headroom (p95 swinging 275 us -> 12 487 us across repetitions). A run
/// requirement in the low single digits would mark every segment on the
/// timeline. ~0.4 s at 30 fps is the calibrated answer
/// (`artifacts/57-DYNRES-CALIBRATION.md`).
///
/// **What this number cannot see, stated because 59-01 measured it.** The
/// ceiling this phase attacks is a BOUNDARY stall — ~1.0 s at a software-pool
/// clip cut — and a boundary stall is over in ONE tick. A sustained-run
/// detector cannot see it, and no amount of tuning this constant makes it able
/// to. That is why D-12's pre-arm is not optional decoration: on the F5
/// arrangement the pre-arm is what marks the range, and this constant is what
/// catches the genuinely sustained case (a stack heavy enough that its steady
/// state misses too). Both inlets feed one registry, and
/// [`heavy_segments`] does not distinguish them, because the worker's job is
/// the same either way.
pub const SEGMENT_MISS_STREAK: u32 = 12;

/// Hard cap on how many segment indices the registry remembers at once.
///
/// A map keyed by UNBOUNDED segment indices grows for the life of the process —
/// every 2 s of program the user ever scrubs through is a new key — and that is
/// exactly the shape that should carry a bound. Same discipline, same reason,
/// as `MAX_TRACKED_PROXY_JOBS` in `app-core`'s proxy-job registry, which
/// records it at length for a map keyed by caller-supplied paths.
///
/// 4096 segments is ~2.3 hours of program at the phase's 2 s grid pitch, so the
/// cap is unreachable for any timeline this milestone targets and the eviction
/// path is a safety net rather than a routine.
///
/// **Eviction is preference-ordered, and forgetting is lossless.** The coldest
/// NON-candidate goes first (a segment that has only ever been in budget is
/// carrying no information); only if every row is a candidate does the coldest
/// candidate go, because an unbounded map on the producer's own path is a real
/// denial of service and a forgotten mark is re-earned the next time the
/// section plays.
pub const MAX_TRACKED_SEGMENTS: usize = 4096;

/// One segment's accumulated heat.
#[derive(Debug, Clone, Copy, Default)]
struct SegHeat {
    /// Consecutive over-budget LIVE ticks since the last in-budget one.
    miss_streak: u32,
    /// Earned by measurement ([`note_live_tick`]).
    heavy: bool,
    /// Earned by the static predicate ([`prearm_stack`]).
    prearmed: bool,
    /// Recency stamp for eviction — the registry's own logical clock, never a
    /// wall clock (this runs on the producer thread; a `SystemTime::now` per
    /// tick would be a syscall the accounting does not need).
    last_touch: u64,
}

impl SegHeat {
    /// Is this segment worth the background worker's time?
    ///
    /// Measured heat and the static pre-arm are deliberately NOT distinguished
    /// here: the worker's job is identical either way, and a caller that could
    /// tell them apart would be a caller that could start treating the pre-arm
    /// as evidence.
    fn is_candidate(&self) -> bool {
        self.heavy || self.prearmed
    }
}

/// The registry and its logical clock.
struct Registry {
    segments: HashMap<i64, SegHeat>,
    clock: u64,
}

impl Registry {
    /// Bring `seg_index`'s row into existence (evicting first if the map is at
    /// its bound), stamp it as most-recently-used, and hand it back.
    fn touch(&mut self, seg_index: i64) -> &mut SegHeat {
        self.clock = self.clock.wrapping_add(1);
        let stamp = self.clock;
        if !self.segments.contains_key(&seg_index) {
            self.evict_to_make_room();
        }
        let heat = self.segments.entry(seg_index).or_default();
        heat.last_touch = stamp;
        heat
    }

    /// Drop rows until there is space for one more. See
    /// [`MAX_TRACKED_SEGMENTS`] for the preference order and why forgetting is
    /// lossless.
    fn evict_to_make_room(&mut self) {
        while self.segments.len() >= MAX_TRACKED_SEGMENTS {
            let victim = self
                .segments
                .iter()
                .filter(|(_, heat)| !heat.is_candidate())
                .min_by_key(|(_, heat)| heat.last_touch)
                .map(|(k, _)| *k)
                .or_else(|| {
                    self.segments
                        .iter()
                        .min_by_key(|(_, heat)| heat.last_touch)
                        .map(|(k, _)| *k)
                });
            match victim {
                Some(k) => {
                    self.segments.remove(&k);
                }
                // Unreachable while the map is non-empty; breaking rather than
                // spinning is the safe answer on a path the producer runs.
                None => break,
            }
        }
    }
}

/// The PROCESS-GLOBAL registry. Global on purpose — that globality IS the
/// Monday/Tuesday property (RQ3), and it is the single most important
/// difference between this and the `!Send`, stack-local controller it must not
/// be confused with.
static HEAT: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        segments: HashMap::new(),
        clock: 0,
    })
});

/// Run `f` under the registry lock, recovering from poisoning rather than
/// panicking.
///
/// A poisoned lock means some thread panicked while holding it. Every value in
/// here is plain `Copy` data with no invariant that spans two writes, so the
/// contents are intact — and a heat registry that could kill the producer
/// thread would be strictly worse than any wrong answer it could give. Same
/// posture as `decode_source`'s configuration lock.
fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut guard = HEAT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// Score ONE **live** produced tick against the frame budget and attribute the
/// result to `seg_index`.
///
/// * `effective_us` — the producer's `produce_us` **minus** the time it spent
///   parked on ring/target backpressure. See the module doc: passing the raw
///   duration is a known false-positive failure, not a rounding error.
/// * `budget_us` — the frame step, `engine::frame_step_us(fps)`, exactly as
///   `DynResController::on_tick` is given it.
///
/// The comparison is `effective_us > budget_us`, STRICTLY — a tick that exactly
/// fills the frame step is on time, so this and the live controller can never
/// disagree about what a miss is.
///
/// **Ticks served FROM the cache must never be passed here (D-13).** This is
/// the only inlet that can move a miss streak; 59-06 owns the gate.
///
/// A mark is STICKY: once a segment has earned [`SEGMENT_MISS_STREAK`]
/// consecutive misses, later in-budget ticks reset the run but never clear the
/// mark. A section that is heavy is a property of the arrangement, and the
/// arrangement has not changed just because one pass through it went well.
pub fn note_live_tick(seg_index: i64, effective_us: u64, budget_us: u64) {
    with_registry(|registry| {
        let heat = registry.touch(seg_index);
        if effective_us <= budget_us {
            // The `dynres` asymmetry, mirrored: ONE in-budget tick resets the
            // run to zero. A mark requires SEGMENT_MISS_STREAK misses with no
            // recovery in between.
            heat.miss_streak = 0;
            return;
        }
        heat.miss_streak = heat.miss_streak.saturating_add(1);
        if heat.miss_streak >= SEGMENT_MISS_STREAK {
            heat.heavy = true;
            // Clamp so a long over-budget section cannot run the counter away;
            // the mark is already set and nothing further depends on the value.
            heat.miss_streak = SEGMENT_MISS_STREAK;
        }
    });
}

/// **D-12's static pre-arm.** Mark `seg_index` a render candidate when its
/// visible video layer count is strictly greater than
/// [`PREARM_LAYER_THRESHOLD`].
///
/// A stack at or below the cap does not even create a row — there is no
/// published failure for it to pre-arm against, and a registry that recorded
/// every well-behaved segment would spend its bound on rows carrying no
/// information.
///
/// This is a PREDICTION, not evidence, and the two are kept apart in the row it
/// writes even though [`heavy_segments`] treats them alike. See
/// [`PREARM_LAYER_THRESHOLD`] for what the prediction is grounded in and for
/// the measured reason it cannot be the trigger on its own.
pub fn prearm_stack(seg_index: i64, visible_layer_count: usize) {
    if visible_layer_count <= PREARM_LAYER_THRESHOLD {
        return;
    }
    with_registry(|registry| {
        registry.touch(seg_index).prearmed = true;
    });
}

/// The candidate segments, ascending, capped at `limit` — 59-08's work queue.
///
/// Ascending rather than "hottest first" on purpose: the answer must not depend
/// on hash iteration order, or the worker would render a different set of
/// segments on every poll of the same registry. Which of the candidates the
/// worker actually takes (playhead proximity vs timeline order) is its
/// decision, not this module's.
///
/// **This is a census, not a work queue** (59-REVIEW WR-04). Truncating a sorted
/// list is a permanent PREFIX FILTER: a caller that walks the answer repeatedly
/// sees the same lowest `limit` candidates forever and can never reach the rest,
/// however many the registry holds. A scheduler wants
/// [`heavy_segments_from`]; this form is for callers that want "the lowest N",
/// and for tests.
pub fn heavy_segments(limit: usize) -> Vec<i64> {
    with_registry(|registry| {
        let mut out = sorted_candidates(registry);
        out.truncate(limit);
        out
    })
}

/// **59-REVIEW WR-04.** The candidate segments, ascending, capped at `limit`,
/// but STARTING just past `after` and wrapping.
///
/// The scheduler's form. [`heavy_segments`] sorts and truncates, so a caller
/// polling it repeatedly re-walks the same lowest `limit` candidates on every
/// poll and candidates past that prefix are never scheduled — ever. At
/// `HEAVY_SEGMENT_POLL_LIMIT = 64` that is 128 s of program at the 2 s pitch,
/// against a [`MAX_TRACKED_SEGMENTS`] built to hold ~2.3 hours of marks. The
/// detector was explicitly bounded to remember far more than the scheduler could
/// structurally reach.
///
/// Rotating rather than widening the cap keeps the scheduler's per-poll bound
/// exactly where it was — this still returns at most `limit` — while making the
/// window sweep the whole set across successive polls. A caller passes the last
/// index it considered and gets the next `limit`, wrapping at the end; passing
/// `i64::MIN` is "start from the beginning" and reproduces
/// [`heavy_segments`]'s answer.
pub fn heavy_segments_from(after: i64, limit: usize) -> Vec<i64> {
    with_registry(|registry| {
        let mut out = sorted_candidates(registry);
        // `partition_point` is <= len, and `rotate_left(len)` is the identity,
        // so reaching the end of the set wraps to its start rather than
        // panicking or emptying the window.
        let split = out.partition_point(|k| *k <= after);
        out.rotate_left(split);
        out.truncate(limit);
        out
    })
}

/// Every candidate index, ascending. The shared half of the two accessors
/// above, so they can never disagree about what a candidate IS.
fn sorted_candidates(registry: &Registry) -> Vec<i64> {
    let mut out: Vec<i64> = registry
        .segments
        .iter()
        .filter(|(_, heat)| heat.is_candidate())
        .map(|(k, _)| *k)
        .collect();
    out.sort_unstable();
    out
}

/// How many segment indices the registry is holding right now — the observable
/// [`MAX_TRACKED_SEGMENTS`]'s bound is asserted against.
///
/// Diagnostic only; nothing branches on it.
pub fn tracked_segment_count() -> usize {
    with_registry(|registry| registry.segments.len())
}

/// Forget every segment's heat.
///
/// **59-REVIEW WR-06** made this a production entry point, and it is named for
/// what it does rather than for who calls it. Heat is keyed by SEGMENT INDEX —
/// a coordinate on a global program-time grid — so a mark earned in one project
/// means something completely different in the next one. `app-core`'s project
/// new / project open paths call this beside their cancel, or the background
/// worker spends the shared encoder permit and a whole decode pool rendering
/// ranges the newly-opened project has never measured as heavy.
///
/// Forgetting is lossless, which is what makes this safe to call at any moment:
/// heat is advisory, [`heavy_segments`] is only ever a suggestion, and a cleared
/// registry re-earns its marks the next time the sections play.
pub fn forget_all_heat() {
    with_registry(|registry| {
        registry.segments.clear();
        registry.clock = 0;
    });
}

/// [`forget_all_heat`] under the name the test suites already call.
///
/// Kept as a distinct name rather than renamed at the call sites: it is `pub`
/// because the tests that need it are INTEGRATION tests in separate binaries,
/// and the two names now say two different things — this one is "a test wants a
/// clean registry", the other is "the world changed".
pub fn reset_for_tests() {
    forget_all_heat();
}
