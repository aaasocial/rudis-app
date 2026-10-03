//! Phase 60 (plan 60-04, DROP-01/DROP-02) — automatic, hysteretic **frame
//! dropping**: the pure decision logic, with no I/O, no GPU and no clock.
//!
//! # The structural sibling of [`crate::dynres`], and only the second axis
//!
//! [`DropMode`] is one of `{Off, Half}`. The producer's multi-layer arm asks
//! this controller once per produced tick — "did that tick fit inside the frame
//! budget, *with the resolution ladder already spent*?" — and, under
//! [`DropMode::Half`], produces every SECOND tick instead of every tick. The
//! presenter needs no change at all for that: `RingCtl::pop_for_target` already
//! answers `None` when the front entry is still in the future and leaves it
//! queued, and `present_loop` already answers `None` by presenting nothing, so
//! the last surface content simply stays on screen. Holding the previous frame
//! on underrun is a contract this crate has shipped since long before this
//! module existed; DROP-01 inherits it rather than building it.
//!
//! Read the [`crate::dynres`] module doc before this one. Every design rule
//! there — sustained-only engagement, pause-only recovery, `!Send`,
//! stack-local, transient — is repeated here for the same reasons, and the two
//! controllers are deliberately independent pure state machines that know
//! nothing about each other.
//!
//! # ORDERED ESCALATION — resolution first, frames second
//!
//! The two axes are not peers racing the same signal. This controller counts a
//! miss ONLY while the resolution ladder is already at its floor, which the
//! caller passes as the `dynres_at_floor` argument of [`FrameDropController::on_tick`].
//! An over-budget tick that the resolution ladder can still answer is treated
//! here as if it had been in-budget: the streak resets, and frame dropping does
//! not engage.
//!
//! Three reasons, recorded so the next reader does not re-litigate the order:
//!
//! * both axes firing off the same streak from ONE overload event would deliver
//!   a picture that is soft AND stuttery at the same instant — the worst of both
//!   trades from a single cause;
//! * escalation keeps the shipped, measured behaviour of the resolution ladder
//!   byte-identical for every overload that ladder alone can absorb, which is
//!   every overload anyone has actually measured on this project's hardware;
//! * the precedence then lives in ONE boolean at ONE call site rather than
//!   inside either module, so neither state machine has to know the other
//!   exists.
//!
//! Both axes CAN be active at once — a half-resolution tick that is also
//! skipped. That is escalation's intended end state under extreme load, not a
//! conflict.
//!
//! # D-09's lineage — SUSTAINED, never a single late frame
//!
//! 57-01 measured this machine's 720p tail swinging p95 from 275 us to 12 487 us
//! across repetitions, with individual frames past 33 ms, on a fixture with 16x
//! headroom. A controller that dropped a frame on one late tick would thrash
//! permanently on a machine nowhere near its limit — and a dropped frame is a
//! visible hitch, so thrash here is *more* noticeable than thrash on the
//! resolution axis, not less. Engagement therefore requires
//! [`DROP_MISS_STREAK_TICKS`] **consecutive** at-floor over-budget ticks, and one
//! single qualifying-miss-free tick resets the streak to zero.
//!
//! Note which ticks are scored: [`FrameDropController::on_tick`] is called from
//! the producer only on ticks it actually PRODUCED. A skipped tick did none of
//! the work the budget is about, so its wall cost would say nothing about the
//! section; scoring it would be measuring the skip.
//!
//! # Recovery is PAUSE-ONLY, and that is deliberate
//!
//! [`FrameDropController::on_tick`] never disengages. The only way back to
//! [`DropMode::Off`] is [`FrameDropController::on_pause`], which the producer
//! calls at the two points the EXISTING stop / gen-bump handshake already runs —
//! never from a signal path of its own (57-RESEARCH Pitfall 8, pinned for the
//! resolution axis and pinned again for this one).
//!
//! An "un-drop on one good tick" rule would be the same oscillation trap the
//! resolution ladder documents, arriving by the same arithmetic: producing every
//! second tick roughly halves the per-tick composite work, so the budget is
//! comfortably met, so the controller would immediately disengage, immediately
//! miss again, and the picture would pump between smooth and stuttery every
//! couple of seconds. A cadence that is stable for as long as the section plays,
//! and clean again the moment the user pauses, is the beginner-invisible
//! behaviour the milestone asks for.
//!
//! # D-11 — this is transient ENGINE state
//!
//! There are deliberately **no** persistence derives on anything in this module,
//! no variant of the document model's mutation type, no field on the state
//! mirror's update record, and no `.rud` representation anywhere. A playback
//! cadence never enters the document model and undo can never reach it. That is
//! enforced by ABSENCE here and pinned by the grep audits in
//! `crates/preview/tests/framedrop_pin.rs`.
//!
//! Those audits are literal substring scans of this file's TEXT (the 48-05
//! zero-copy-audit pattern), so this module also avoids NAMING the forbidden
//! machinery in prose — a doc comment that spells one of those identifiers reads
//! to a scan as a violation, correctly, since the scan cannot tell prose from a
//! derive and teaching it to would weaken the only thing standing between the
//! D-11 claim and a comment. `dynres.rs` records the same footgun and 59-05 hit
//! it twice more; this module is stricter than `dynres.rs` about it, because its
//! audit needle list is longer.
//!
//! # D-12 — export can never reach a dropped cadence
//!
//! [`FrameDropController`] is deliberately **`!Send`** (see the `_not_send`
//! field). It cannot be moved to another thread, stored in a `static`, or placed
//! behind an `Arc` shared with anything — including the export thread. Combined
//! with the crate graph (`crates/engine` does not depend on `crates/preview` at
//! all, so no encode or composite entry point on the export path can even name
//! [`DropMode`]), a dropped cadence is structurally out of export's reach rather
//! than merely unused by it. Both halves are pinned in `framedrop_pin.rs`.
//!
//! # What the SKIP actually skips — the honest scope (plan 60-04)
//!
//! This module decides; `ring.rs` acts. The action is narrower than the plan
//! that commissioned it assumed, and the reason is a measured property of the
//! software decoder pool rather than a preference:
//! `engine::LayerDecoderPool::advance` judges continuation at half a source step
//! (`decoder_pool.rs:554-558`), so a produced tick whose demand jumped TWO source
//! steps reads as a seek and respawns an `ffmpeg` child for every software-pooled
//! layer, every produced tick. A skip that bypassed the pull would therefore have
//! bought a halved composite by paying a process spawn per layer per tick — a
//! pessimization, arriving exactly when the machine is already overloaded.
//!
//! So the skip bypasses the whole COMPOSITE half of the tick (the layer gather
//! and its inline still/text/image-sequence work, the composite-target checkout
//! and its backpressure wait, the composite itself, the readback, the ring push,
//! and the lookahead probe) and keeps the software pull that the pool's
//! continuation contract requires. Hardware layers are unaffected either way:
//! their sessions free-run into HOLD slots with a four-tick staleness window
//! (`layer_sessions.rs:239`), which a one-tick skip cannot exceed.
//!
//! That targets the cost the measurements actually name — 57-RESEARCH § 9 and
//! `dynres.rs`'s own module doc both record that decode was never the bottleneck
//! — and it composes with the resolution ladder multiplicatively: at
//! `ResLevel::Half` the fill-rate per composite is already a quarter, and
//! [`DropMode::Half`] halves the NUMBER of composites on top of that. The
//! decode-side saving DROP-01's plan also wanted is not delivered here; the two
//! costed routes to it are recorded in this phase's `deferred-items.md`.

use std::marker::PhantomData;
use std::sync::atomic::AtomicU64;

/// How many CONSECUTIVE at-floor over-budget produce ticks earn engagement.
///
/// **This is DROP's own constant, not a copy of the resolution ladder's.** It is
/// PROVISIONALLY seeded from that ladder's calibrated cadence — 12 ticks is
/// ~0.4 s at 30 fps, and ~0.6 s at the ~50 ms tick the injected-overload pin
/// runs at — because the two axes answer the same question ("is this section
/// sustainedly over budget, or is this scheduler noise?") on the same signal, and
/// re-deriving the noise floor from scratch would produce the same number by the
/// same argument. What is NOT inherited is the perceptual price: a resolution
/// drop is continuous and a frame drop is a visible hitch, so the right value
/// here could legitimately end up higher than the ladder's.
///
/// **Measured** (plan 60-05, `60-DROP-CALIBRATION.md § 1`): 12 ticks is
/// ~0.70 s of unbroken at-floor overload before the first frame is skipped, the
/// same to within 3 ms across two runs, and it reads directly off the presented
/// frame census — exactly 24 composites reach the sink before the first skip,
/// which is the resolution ladder's streak plus this one.
///
/// That artifact is also where the honest limitation lives: this axis has never
/// engaged on the reference machine without an injected cost, so the value is
/// calibrated against a measured NOISE floor and verified under a synthetic
/// overload — not tuned against hardware that genuinely cannot keep up.
/// Re-calibrate if such hardware ever exercises it.
pub const DROP_MISS_STREAK_TICKS: u32 = 12;

/// Minimum ticks between two successive cadence transitions.
///
/// 60 ticks is ~2 s at 30 fps. As with the resolution ladder's twin, the ladder
/// here currently has exactly ONE rung (`Off -> Half`), so today this window
/// spaces nothing: there is no second transition for it to delay, and its only
/// live effect is to be counted down. It is kept because the spacing rule is a
/// property of the MECHANISM, not of how many rungs the ladder currently has — a
/// `Quarter`-style third rung (produce one tick in four) would need it back, and
/// re-deriving the number would mean re-running the calibration.
///
/// What it must not be read as is a promise that a second transition exists.
pub const DROP_HOLD_TICKS: u32 = 60;

/// Ticks the producer actually skipped, process-wide, since start.
///
/// The PLAY-07 churn-counter idiom: a plain relaxed counter that exists so a
/// test can prove the mechanism engaged on a real run, incremented at the ONE
/// producer call site that honours [`FrameDropController::skip_this_tick`].
///
/// Deliberately NOT an observable: no C ABI getter, no shell surface, no
/// `EngineDiag` field. The v8 freeze lists PLAY-05 / PROXY-02 / CACHE-01 as the
/// exceptions that earned a shell-visible reading, and DROP is not among them.
pub static FRAMEDROP_SKIPPED_TICKS: AtomicU64 = AtomicU64::new(0);

/// The active playback frame cadence (DROP-01).
///
/// A DISCRETE MODE, mirroring the resolution ladder's `ResLevel`, and not a
/// reactive per-tick "am I late right now?" question — a cadence that reacts to
/// a single late frame is exactly what D-09 forbids.
///
/// `#[repr(u8)]` with explicit discriminants for shape parity with its sibling,
/// NOT because anything crosses an ABI with them: nothing outside this crate
/// reads this type (D-12), and nothing should start.
///
/// No persistence derives, deliberately (D-11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum DropMode {
    /// Produce every tick. The default, and the only cadence export has ever
    /// seen or can see.
    Off = 0,
    /// Produce every SECOND tick; the presenter holds the previous frame through
    /// the skipped one.
    Half = 1,
}

impl DropMode {
    /// The next mode DOWN, or `None` at the floor.
    ///
    /// The floor is [`DropMode::Half`]. There is no `higher()` twin on purpose:
    /// nothing in this module raises the cadence except
    /// [`FrameDropController::on_pause`], which jumps straight to
    /// [`DropMode::Off`]. See the module doc on pause-only recovery.
    ///
    /// A one-rung ladder does not need this function to make a decision — the
    /// engagement arm could match on the mode directly. It exists because the
    /// resolution ladder learned the lesson first: when that ladder's floor
    /// moved, the ONE arm here is where the change lands, and every hysteresis
    /// rule around it stays untouched and unre-derived.
    pub fn lower(self) -> Option<DropMode> {
        match self {
            DropMode::Off => Some(DropMode::Half),
            DropMode::Half => None,
        }
    }
}

/// The hysteresis state machine. One per producer thread, owned as a stack
/// local by `producer_loop` — never shared, never stored, never persisted.
///
/// `!Send` by construction (`_not_send`): this type cannot be moved onto another
/// thread, put in a `static`, or shared through an `Arc`. That is the compile-
/// time half of D-12 ("a dropped cadence must be UNREACHABLE from export, not
/// merely unused by it"); the other half is the crate graph, pinned in
/// `framedrop_pin.rs`.
pub struct FrameDropController {
    mode: DropMode,
    /// Consecutive AT-FLOOR over-budget ticks since the last tick that did not
    /// qualify as a miss, or since the last transition.
    miss_streak: u32,
    /// Ticks remaining before another transition is permitted.
    hold_remaining: u32,
    /// The alternation state [`FrameDropController::skip_this_tick`] walks under
    /// [`DropMode::Half`]: `false` produces, `true` skips. Reset at every
    /// transition and at every pause, so an engagement always begins with a
    /// PRODUCED tick — the first thing the user sees after the machine gives up
    /// resolution is a fresh frame, not a held one.
    skip_parity: bool,
    /// Makes the controller `!Send`/`!Sync`. Zero-sized; see the type doc.
    _not_send: PhantomData<*const ()>,
}

impl FrameDropController {
    pub fn new() -> Self {
        Self {
            mode: DropMode::Off,
            miss_streak: 0,
            hold_remaining: 0,
            skip_parity: false,
            _not_send: PhantomData,
        }
    }

    /// One call per PRODUCED multi-layer tick. Returns the cadence in force from
    /// here on.
    ///
    /// `effective_us` is the wall cost of producing that frame MINUS any time
    /// the producer spent parked on ring/composite-target backpressure — the
    /// identical signal, from the identical call site, that the resolution
    /// ladder is scored on. The exclusion is not a detail: a producer that is
    /// keeping up is by definition blocked waiting for the presenter most of the
    /// time, so a raw tick duration reads ~one frame step on a pipeline with
    /// plenty of headroom and would sit this controller permanently on the edge
    /// of its own threshold.
    ///
    /// `budget_us` is the frame step (`engine::frame_step_us(fps)`).
    ///
    /// `dynres_at_floor` is the ESCALATION GATE (see the module doc): `false`
    /// means the resolution ladder still has a rung to spend, and this tick is
    /// then treated exactly as an in-budget one — frame dropping is the SECOND
    /// escape hatch and only earns engagement from misses the first one can no
    /// longer answer.
    ///
    /// Like its sibling, the verdict affects a FUTURE tick: this is called after
    /// the tick's work is already done, so the cadence it returns governs the
    /// tick after next. That latency is inherited from the call site, not new.
    pub fn on_tick(&mut self, effective_us: u64, budget_us: u64, dynres_at_floor: bool) -> DropMode {
        if self.hold_remaining > 0 {
            self.hold_remaining -= 1;
        }
        // A miss must be BOTH over budget AND unanswerable by the first axis.
        // Strictly over budget: a tick that exactly fills the frame step is on
        // time, not late (the sibling's `exactly_on_budget_is_a_hit`).
        let qualifying_miss = effective_us > budget_us && dynres_at_floor;
        if !qualifying_miss {
            // D-09: ONE non-qualifying tick resets the streak. Engagement
            // requires DROP_MISS_STREAK_TICKS at-floor misses with no recovery
            // in between.
            self.miss_streak = 0;
            return self.mode;
        }
        self.miss_streak = self.miss_streak.saturating_add(1);
        if self.miss_streak >= DROP_MISS_STREAK_TICKS && self.hold_remaining == 0 {
            match self.mode.lower() {
                Some(next) => {
                    self.mode = next;
                    self.miss_streak = 0;
                    self.hold_remaining = DROP_HOLD_TICKS;
                    // Engagement begins with a PRODUCED tick.
                    self.skip_parity = false;
                }
                None => {
                    // Already at the cadence floor. Clamp the streak so it
                    // cannot run away over a long over-budget section; the
                    // cadence stays where it is, and no amount of further
                    // pressure moves it.
                    self.miss_streak = DROP_MISS_STREAK_TICKS;
                }
            }
        }
        self.mode
    }

    /// Called exactly ONCE per multi-layer tick by the producer, at the top of
    /// the tick, INCLUDING on ticks it goes on to skip. `true` means "do not
    /// produce this one".
    ///
    /// Under [`DropMode::Off`] this is always `false` and the alternation state
    /// does not move, so the parity a later engagement starts from is a property
    /// of the transition rather than of how long the producer ran before it.
    pub fn skip_this_tick(&mut self) -> bool {
        match self.mode {
            DropMode::Off => false,
            DropMode::Half => {
                let skip = self.skip_parity;
                self.skip_parity = !skip;
                skip
            }
        }
    }

    /// Snap back to [`DropMode::Off`] and forget all history.
    ///
    /// Called by the producer at the two points the EXISTING pause/flush
    /// handshake already runs (`ring.rs`'s loop-top `ctl.stop` observation and
    /// its gen-bump flush branch) — never from a second signal path of its own
    /// (Pitfall 8).
    ///
    /// Clearing the history is the load-bearing half: the next play must re-earn
    /// engagement across a full [`DROP_MISS_STREAK_TICKS`] streak, so one late
    /// frame right after a resume can never drop one. The flush case has the
    /// same rationale the resolution axis records — a seek discards the whole
    /// runway and rebuilds it cold at a new position, so miss history describing
    /// the old position must not penalize the new one.
    pub fn on_pause(&mut self) -> DropMode {
        self.mode = DropMode::Off;
        self.miss_streak = 0;
        self.hold_remaining = 0;
        self.skip_parity = false;
        self.mode
    }

    /// The cadence in force right now.
    pub fn mode(&self) -> DropMode {
        self.mode
    }
}

impl Default for FrameDropController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 30 fps.
    const BUDGET: u64 = 33_333;
    const MISS: u64 = 50_000; // comfortably over
    const HIT: u64 = 8_000; // comfortably under
    /// The escalation gate, spelled at every call so no test can silently mean
    /// the other thing.
    const AT_FLOOR: bool = true;
    const HAS_RUNGS: bool = false;

    /// Drive `c` to [`DropMode::Half`] the way playback would.
    fn engage(c: &mut FrameDropController) {
        for _ in 0..DROP_MISS_STREAK_TICKS {
            c.on_tick(MISS, BUDGET, AT_FLOOR);
        }
        assert_eq!(c.mode(), DropMode::Half, "fixture failed to engage");
    }

    /// Behavior 1 (D-09): fewer than DROP_MISS_STREAK_TICKS at-floor misses
    /// never engage, and one in-budget tick resets an almost-complete streak.
    #[test]
    fn eleven_misses_then_one_hit_stays_off() {
        let mut c = FrameDropController::new();
        for i in 0..(DROP_MISS_STREAK_TICKS - 1) {
            assert_eq!(
                c.on_tick(MISS, BUDGET, AT_FLOOR),
                DropMode::Off,
                "miss {i} of {} must not engage on its own",
                DROP_MISS_STREAK_TICKS - 1
            );
        }
        assert_eq!(c.on_tick(HIT, BUDGET, AT_FLOOR), DropMode::Off, "recovery tick");
        // The streak is genuinely reset, not merely paused.
        for _ in 0..(DROP_MISS_STREAK_TICKS - 1) {
            assert_eq!(c.on_tick(MISS, BUDGET, AT_FLOOR), DropMode::Off);
        }
        assert_eq!(c.mode(), DropMode::Off);
    }

    /// Behavior 2: exactly DROP_MISS_STREAK_TICKS consecutive at-floor misses
    /// engage Half, and the streak counter resets at the transition.
    #[test]
    fn sustained_at_floor_miss_engages_half_and_resets_the_streak() {
        let mut c = FrameDropController::new();
        for _ in 0..(DROP_MISS_STREAK_TICKS - 1) {
            assert_eq!(c.on_tick(MISS, BUDGET, AT_FLOOR), DropMode::Off);
        }
        assert_eq!(
            c.on_tick(MISS, BUDGET, AT_FLOOR),
            DropMode::Half,
            "the DROP_MISS_STREAK_TICKS-th consecutive at-floor miss engages"
        );
        assert_eq!(c.miss_streak, 0, "the streak resets at the transition");
        assert_eq!(c.hold_remaining, DROP_HOLD_TICKS, "the hold window opens");
        assert!(!c.skip_parity, "engagement begins with a PRODUCED tick");
    }

    /// **THE ESCALATION GATE.** A miss while the resolution ladder still has a
    /// rung is not a miss here: the streak resets exactly as an in-budget tick
    /// would reset it, and no run of such misses — however long — ever engages.
    ///
    /// The non-vacuous direction is the second half: the SAME controller, given
    /// the SAME over-budget costs with the gate open, engages immediately. So
    /// this test fails if the gate is dropped from the signature *or* if it is
    /// wired backwards.
    #[test]
    fn misses_while_the_resolution_ladder_has_rungs_never_engage() {
        let mut c = FrameDropController::new();
        for _ in 0..(DROP_MISS_STREAK_TICKS * 10) {
            assert_eq!(
                c.on_tick(MISS, BUDGET, HAS_RUNGS),
                DropMode::Off,
                "frame dropping is the SECOND escape hatch: an overload the \
                 resolution ladder can still answer must never earn a skip"
            );
        }
        assert_eq!(c.miss_streak, 0, "an ungated miss must not accumulate");
        // …and the identical pressure WITH the gate closed engages at once.
        for _ in 0..(DROP_MISS_STREAK_TICKS - 1) {
            assert_eq!(c.on_tick(MISS, BUDGET, AT_FLOOR), DropMode::Off);
        }
        assert_eq!(c.on_tick(MISS, BUDGET, AT_FLOOR), DropMode::Half);
    }

    /// One at-floor miss anywhere inside the streak is not enough on its own,
    /// and one gated miss inside an otherwise-complete at-floor streak resets it
    /// — the gate is checked per tick, not once.
    #[test]
    fn one_gated_tick_inside_the_streak_resets_it() {
        let mut c = FrameDropController::new();
        for _ in 0..(DROP_MISS_STREAK_TICKS - 1) {
            c.on_tick(MISS, BUDGET, AT_FLOOR);
        }
        assert_eq!(
            c.on_tick(MISS, BUDGET, HAS_RUNGS),
            DropMode::Off,
            "the ladder regained a rung — this tick is not this axis's business"
        );
        assert_eq!(c.miss_streak, 0);
        for _ in 0..(DROP_MISS_STREAK_TICKS - 1) {
            assert_eq!(c.on_tick(MISS, BUDGET, AT_FLOOR), DropMode::Off);
        }
    }

    /// Behavior 3: under Half the skip alternates STRICTLY false/true, starting
    /// produced; under Off it is always false and never moves the parity.
    #[test]
    fn skip_alternates_under_half_and_is_inert_under_off() {
        let mut c = FrameDropController::new();
        for _ in 0..7 {
            assert!(!c.skip_this_tick(), "Off never skips");
        }
        engage(&mut c);
        let walk: Vec<bool> = (0..8).map(|_| c.skip_this_tick()).collect();
        assert_eq!(
            walk,
            vec![false, true, false, true, false, true, false, true],
            "Half produces every second tick, beginning with a produced one"
        );
        // Exactly half the ticks are skipped over any even-length window — the
        // property the presenter's hold contract is sized against.
        assert_eq!(walk.iter().filter(|s| **s).count(), walk.len() / 2);
    }

    /// The Off arm must not consume parity: a controller that ran for an ODD
    /// number of ticks before engaging still begins its engagement produced.
    #[test]
    fn off_ticks_do_not_shift_the_engagement_parity() {
        for pre_ticks in 0..5 {
            let mut c = FrameDropController::new();
            for _ in 0..pre_ticks {
                assert!(!c.skip_this_tick());
            }
            engage(&mut c);
            assert!(
                !c.skip_this_tick(),
                "after {pre_ticks} Off ticks the first Half tick must still produce"
            );
            assert!(c.skip_this_tick());
        }
    }

    /// Behavior 4: the hold window spaces what it spaces, and Half is where the
    /// walk ends — inside the hold nothing transitions again, and PAST the hold
    /// nothing does either, because there is no second rung.
    #[test]
    fn hold_window_elapses_and_half_is_the_floor() {
        let mut c = FrameDropController::new();
        engage(&mut c);
        for i in 0..(DROP_HOLD_TICKS - 1) {
            assert_eq!(
                c.on_tick(MISS, BUDGET, AT_FLOOR),
                DropMode::Half,
                "tick {i} inside the {DROP_HOLD_TICKS}-tick hold must not transition again"
            );
        }
        assert_eq!(
            c.on_tick(MISS, BUDGET, AT_FLOOR),
            DropMode::Half,
            "the first miss PAST the hold window must stay at Half: the ladder \
             has one rung, so there is no second transition for the hold window \
             to have been spacing"
        );
        for _ in 0..(DROP_HOLD_TICKS * 3) {
            assert_eq!(c.on_tick(MISS, BUDGET, AT_FLOOR), DropMode::Half);
        }
        assert_eq!(
            c.mode().lower(),
            None,
            "Half is the bottom of the cadence ladder"
        );
        assert_eq!(
            c.miss_streak, DROP_MISS_STREAK_TICKS,
            "the streak is CLAMPED at the floor, not left to run away"
        );
    }

    /// Behavior 5: pause snaps back to Off from any mode, with all history
    /// cleared, so the next play re-earns engagement from scratch.
    #[test]
    fn pause_snaps_back_to_off_and_clears_history() {
        let mut c = FrameDropController::new();
        engage(&mut c);
        // Land mid-alternation, so the parity reset is genuinely exercised.
        assert!(!c.skip_this_tick());
        assert_eq!(c.on_pause(), DropMode::Off);
        assert_eq!(c.miss_streak, 0);
        assert_eq!(c.hold_remaining, 0);
        assert!(!c.skip_parity);
        assert!(!c.skip_this_tick(), "Off never skips");
        // History cleared: a fresh full streak is required.
        for _ in 0..(DROP_MISS_STREAK_TICKS - 1) {
            assert_eq!(c.on_tick(MISS, BUDGET, AT_FLOOR), DropMode::Off);
        }
        assert_eq!(c.on_tick(MISS, BUDGET, AT_FLOOR), DropMode::Half);
    }

    /// `on_pause` is TOTAL over the enum and idempotent — a pause on an
    /// already-Off controller is a no-op, which is what makes the two producer
    /// call sites safe to hit in any order.
    #[test]
    fn pause_is_total_and_idempotent() {
        for engaged in [false, true] {
            let mut c = FrameDropController::new();
            if engaged {
                engage(&mut c);
            }
            assert_eq!(c.on_pause(), DropMode::Off);
            assert_eq!(c.on_pause(), DropMode::Off);
            assert_eq!(c.mode(), DropMode::Off);
        }
    }

    /// Behavior 6: recovery is PAUSE-ONLY. In-budget ticks at Half never
    /// disengage mid-play — the deliberate anti-oscillation choice recorded in
    /// the module doc, and the one a "it's fast again now" refactor would break.
    #[test]
    fn hits_at_half_never_disengage_mid_play() {
        let mut c = FrameDropController::new();
        engage(&mut c);
        for _ in 0..(DROP_HOLD_TICKS * 5) {
            assert_eq!(
                c.on_tick(HIT, BUDGET, AT_FLOOR),
                DropMode::Half,
                "an in-budget tick must never raise the cadence mid-play"
            );
        }
        // …nor does the resolution ladder recovering a rung.
        for _ in 0..(DROP_HOLD_TICKS * 2) {
            assert_eq!(c.on_tick(HIT, BUDGET, HAS_RUNGS), DropMode::Half);
        }
        assert_eq!(c.mode(), DropMode::Half);
    }

    /// A budget miss is STRICTLY over budget — a tick that exactly fills the
    /// frame step is on time, not late.
    #[test]
    fn exactly_on_budget_is_a_hit() {
        let mut c = FrameDropController::new();
        for _ in 0..(DROP_MISS_STREAK_TICKS * 4) {
            assert_eq!(c.on_tick(BUDGET, BUDGET, AT_FLOOR), DropMode::Off);
        }
    }

    /// The one-rung ladder, in one pin.
    #[test]
    fn off_lowers_to_half_and_half_is_the_floor() {
        assert_eq!(DropMode::Off.lower(), Some(DropMode::Half));
        assert_eq!(DropMode::Half.lower(), None);
    }

    /// **D-12's compile-time half, asserted rather than assumed.** The
    /// controller is `!Send`, so it cannot be hoisted onto another thread, into
    /// a `static`, or behind an `Arc` shared with the export thread.
    ///
    /// A negative trait bound cannot be written in Rust, so this uses the
    /// standard inherent-impl-beats-trait-impl resolution trick: the inherent
    /// `SEND = true` is only applicable when `T: Send`, and a `T` that is not
    /// falls through to the blanket trait's `SEND = false`. The `u32` control is
    /// what makes a `false` here a fact about `FrameDropController` rather than
    /// a fact about the probe.
    #[test]
    fn controller_is_not_send() {
        struct SendProbe<T>(PhantomData<T>);
        trait DefaultNotSend {
            const SEND: bool = false;
        }
        impl<T> DefaultNotSend for SendProbe<T> {}
        impl<T: Send> SendProbe<T> {
            const SEND: bool = true;
        }
        assert!(
            SendProbe::<u32>::SEND,
            "control: the probe must see a genuinely Send type as Send, or the \
             assertion below is a statement about the probe"
        );
        assert!(
            !SendProbe::<FrameDropController>::SEND,
            "FrameDropController must stay !Send (D-12): the `_not_send` marker \
             is what makes a dropped cadence structurally unreachable from the \
             export thread rather than merely unused by it"
        );
    }
}
