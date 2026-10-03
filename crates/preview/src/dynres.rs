//! Phase 57 (plan 57-08, PLAY-05) — automatic, hysteretic **dynamic playback
//! resolution**: the pure decision logic, with no I/O, no GPU and no clock.
//!
//! # What it decides, and where the decision is applied
//!
//! [`ResLevel`] is one of `{Full, Half, Quarter}`. The producer's multi-layer
//! arm asks this controller once per produced tick — "did that tick fit inside
//! the frame budget?" — and scales the **composite target dimensions** by the
//! answer ([`ResLevel::scale_dims`]). Nothing else changes: decode still runs at
//! native resolution, the presenter's existing `contain_fit_viewport` still
//! upscales the (smaller) composite to fill the whole surface, and the user sees
//! a full-canvas picture that is softer rather than a stuttering one.
//!
//! **The playback LADDER is `Full -> Half`, and stops there** (2026-08-03).
//! `Quarter` remains a value of the enum — its discriminant is an ABI contract
//! and its `scale_dims` arithmetic is still asserted — but [`ResLevel::lower`]
//! never returns it, so no run of over-budget ticks, however long, can select
//! it. The rationale is in the D-09 section below.
//!
//! **Composite-side is the settled mechanism, not a convenience** (57-RESEARCH
//! § 9, and the project's own backlog-999.9 research against DaVinci Resolve's
//! `Playback ▸ Timeline Proxy Resolution`, `ROADMAP.md:1886-1898`):
//!
//! * *decode-side* scaling is not available for long-GOP H.264/HEVC — halving
//!   the output resolution does not reduce the number of frames a decoder must
//!   reconstruct from a keyframe — and would mean reconfiguring a live hardware
//!   session mid-playback;
//! * *present-side* scaling (composite at full size, then shrink) wastes exactly
//!   the fill-rate cost the degradation exists to cut.
//!
//! # D-09 — SUSTAINED, never a single late frame
//!
//! 57-01 measured this machine's 720p tail swinging p95 from 275 us to 12 487 us
//! across repetitions, with individual frames past 33 ms, on a fixture with 16x
//! headroom. A controller that dropped a level on one late frame would thrash
//! permanently on a machine that is nowhere near its limit. So a drop requires
//! [`MISS_STREAK_TICKS`] **consecutive** over-budget ticks — one single
//! in-budget tick resets the streak to zero — and successive drops are spaced by
//! [`HOLD_TICKS`].
//!
//! # D-09's FLOOR is Half — the 2026-08-03 owner-UAT correction
//!
//! As first shipped, D-09's bottom was `Quarter`. Owner UAT on a 3-layer stacked
//! timeline rejected it, and the rejection is measurable rather than a matter of
//! taste: at a 1920x1080 canvas, `Quarter` composites 480x270 and the presenter
//! then stretches that across a viewport around 1500 px wide — better than 5x
//! linear magnification. What reaches the screen at that ratio is not "softer",
//! it is mush, and a beginner reads mush as the app being broken rather than as
//! the app protecting their playback.
//!
//! `Half` costs the machine 4x more per composite than `Quarter` and still cuts
//! the fill-rate work to a quarter of `Full`, which is where the anti-stutter
//! win overwhelmingly comes from; the second rung was buying a throughput margin
//! that was rarely needed at a perceptual price that was always paid. So the
//! ladder was floored at `Half`, and the picture during degraded playback was
//! separately sharpened at PRESENT time (`engine`'s surface blit magnifies with
//! a Catmull-Rom reconstruction instead of a bilinear one — a present-only fork
//! that cannot touch export).
//!
//! What was deliberately NOT done, so the next reader does not re-litigate it:
//! no climb-back mid-play (see the next section — it is the oscillation trap),
//! and no change to the controller's semantics. One arm of
//! [`ResLevel::lower`] moved; everything else about D-09 stands.
//!
//! The consequence for the mechanism is that a timeline the machine cannot
//! sustain even at `Half` now stutters at `Half` instead of degrading further.
//! That is the accepted trade, stated plainly: the deeper rung existed for
//! exactly that case and was judged to make it look worse, not better.
//!
//! # Recovery is PAUSE-ONLY, and that is deliberate
//!
//! [`DynResController::on_tick`] never raises the level. The only way back to
//! [`ResLevel::Full`] is [`DynResController::on_pause`], which the producer
//! calls at the point the EXISTING `ctl.stop` / gen-bump handshake already runs
//! (57-RESEARCH Pitfall 8 — a second notification path racing that handshake is
//! the failure this avoids).
//!
//! Climbing back mid-play would be the oscillation trap: at `Half` the produce
//! cost is ~4x cheaper, so the budget is comfortably met, so a "hits climb back"
//! rule would immediately return to `Full`, immediately miss again, and the
//! picture would pump between two sharpnesses every couple of seconds. A level
//! that is stable for as long as the section plays, and clean again the moment
//! the user pauses, is the beginner-invisible behaviour the milestone's purpose
//! statement asks for. Resolve's own control has the same property: it is sticky
//! until you change it.
//!
//! # D-11 — this is transient ENGINE state
//!
//! There are deliberately **no** serialization derives, no `Command` variant,
//! no `Patch` field and no `.rud` representation anywhere in this module.
//! Playback resolution never enters the document model and undo can never reach
//! it. That is enforced by ABSENCE here and pinned by the grep audits in
//! `crates/preview/tests/dynres_pin.rs`.
//!
//! The audits are literal substring scans of this file's TEXT (the 48-05
//! zero-copy-audit pattern), so this module also avoids NAMING the
//! serialization crate in prose — a doc comment that mentions it reads as a
//! violation, correctly, since the audit cannot tell prose from a derive and
//! teaching it to would weaken the only thing standing between the D-11 claim
//! and a comment. 57-06 hit this exact class one layer out.
//!
//! # D-12 — export can never reach a degraded level
//!
//! [`DynResController`] is deliberately **`!Send`** (see the `_not_send` field).
//! It cannot be moved to another thread, stored in a `static`, or placed behind
//! an `Arc` shared with anything — including the export thread. Combined with
//! the crate graph (`crates/engine` does not depend on `crates/preview` at all,
//! and `crates/app-core` names `preview` only as a `[dev-dependencies]` edge, so
//! no SHIPPED export path can even name [`ResLevel`]), a degraded level is
//! structurally out of export's reach rather than merely unused by it. Both
//! halves are pinned in `dynres_pin.rs`.

use std::marker::PhantomData;

/// How many CONSECUTIVE over-budget produce ticks earn one resolution drop.
///
/// 12 ticks is ~0.4 s at 30 fps. Calibrated on the BENCH-02 fixtures — see
/// `artifacts/57-DYNRES-CALIBRATION.md` for the measurement that chose it and
/// the numbers for the settings that were tried.
///
/// The floor on this number is set by the noise, not by taste: the frame-time
/// distribution on this machine is quantized by the ~15.6 ms Windows scheduler
/// tick (wave-0 finding F4), and single frames land past 33 ms on fixtures with
/// large headroom. A streak requirement in the low single digits would fire on
/// that noise. The ceiling is set by responsiveness: a user who has genuinely
/// overloaded the machine should see the picture stabilise within about half a
/// second, not after several.
pub const MISS_STREAK_TICKS: u32 = 12;

/// Minimum ticks between two successive drops.
///
/// 60 ticks is ~2 s at 30 fps. A drop is not free — it reallocates the whole
/// composite target pool — and its effect is not instant either: frames already
/// in the ring were composited at the OLD level and still have to drain before
/// the cheaper ones reach the screen. Dropping again before the first drop has
/// had time to show up would be acting on measurements that predate the fix.
/// Two seconds is comfortably longer than the deepest runway the ring can hold.
///
/// Since the 2026-08-03 floor change the ladder has exactly ONE rung
/// (`Full -> Half`), so today this window spaces nothing: there is no second
/// drop for it to delay, and its only live effect is to be counted down. It is
/// kept — rather than deleted along with the rung it used to space — because the
/// spacing rule is a property of the MECHANISM, not of how many rungs the ladder
/// currently has: a future rung would need it back, and re-deriving the number
/// would mean re-running the calibration. What it must not be read as is a
/// promise that a second drop exists.
pub const HOLD_TICKS: u32 = 60;

/// Minimum composite dimension. A 1-pixel-wide render target is legal and
/// useless; 16 keeps a degraded composite something the presenter can still
/// contain-fit sanely on a pathologically small canvas.
pub const MIN_DIM: u32 = 16;

/// The active playback composite resolution (PLAY-05 / D-09).
///
/// `#[repr(u8)]` with explicit discriminants because the values cross the C ABI
/// as an `i32` through `rudis_get_playback_resolution_level` — the numbers are
/// part of that contract, not an implementation detail.
///
/// No serialization derives, deliberately (D-11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ResLevel {
    /// Composite at the project canvas. The default, and the only level export
    /// has ever seen or can see.
    Full = 0,
    /// Composite at half the project canvas in each axis (a quarter of the
    /// pixels).
    Half = 1,
    /// Composite at a quarter of the project canvas in each axis (a sixteenth
    /// of the pixels).
    ///
    /// **UNREACHABLE from the playback ladder since 2026-08-03** — see
    /// [`ResLevel::lower`], whose `Half` arm now answers `None`. The variant is
    /// retained for two reasons, both contractual rather than sentimental:
    ///
    /// * its discriminant `2` crosses the C ABI through
    ///   `rudis_get_playback_resolution_level` as `i32`, and the shell's mapping
    ///   of that value is pinned;
    /// * [`ResLevel::scale_dims`] must keep answering `(w >> 2, h >> 2)` for it,
    ///   which is still exercised (the export-side assertions name the quarter
    ///   dims as a size an export must NEVER come out at).
    ///
    /// A level nothing can select is not dead code here; it is a value the ABI
    /// still defines. [`DynResController::on_pause`] therefore stays total over
    /// the enum: the observable can only be trusted if a snap-back from this
    /// value is defined too.
    Quarter = 2,
}

impl ResLevel {
    /// The composite target dimensions this level asks for, given the project
    /// canvas.
    ///
    /// Right shifts rather than division so the arithmetic is exact and a
    /// zero-dimension canvas cannot divide by zero; [`MIN_DIM`] is the floor.
    pub fn scale_dims(self, w: u32, h: u32) -> (u32, u32) {
        let shift = match self {
            ResLevel::Full => 0,
            ResLevel::Half => 1,
            ResLevel::Quarter => 2,
        };
        ((w >> shift).max(MIN_DIM), (h >> shift).max(MIN_DIM))
    }

    /// The next level DOWN, or `None` at the floor.
    ///
    /// **The floor is [`ResLevel::Half`], not [`ResLevel::Quarter`]** (owner
    /// UAT, 2026-08-03). Quarter is deliberately excluded from the ladder: a
    /// 480x270 composite stretched across a ~1500 px-wide viewport was judged
    /// unacceptable on a 3-layer timeline, and its 4x throughput win over Half
    /// did not pay for that. Half is therefore both the first and the last rung.
    ///
    /// There is no `higher()` twin on purpose: nothing in this module raises a
    /// level except [`DynResController::on_pause`], which jumps straight to
    /// [`ResLevel::Full`]. See the module doc on pause-only recovery.
    pub fn lower(self) -> Option<ResLevel> {
        match self {
            ResLevel::Full => Some(ResLevel::Half),
            // THE PLAYBACK FLOOR (2026-08-03). Half has no rung below it, so a
            // run of misses of any length can never reach Quarter. Quarter is
            // excluded from the LADDER, not from the enum: its discriminant and
            // its `scale_dims` arithmetic are still contractual (see the variant
            // doc), it is simply not a level playback can arrive at.
            ResLevel::Half => None,
            ResLevel::Quarter => None,
        }
    }

    /// This level's slot in [`every_composite_size`]'s answer.
    ///
    /// Exhaustive with no wildcard arm, deliberately: a fourth level must be a
    /// COMPILE error here rather than a silently-missing entry in an array a
    /// safety predicate reads. Same discipline, same reason, as
    /// `rudis_core::SourceAlpha::is_proven_opaque`.
    fn enumeration_slot(self) -> usize {
        match self {
            ResLevel::Full => 0,
            ResLevel::Half => 1,
            ResLevel::Quarter => 2,
        }
    }
}

/// How many distinct composite sizes the ladder can ask for.
pub const COMPOSITE_SIZE_COUNT: usize = 3;

/// EVERY composite target size this ladder can ask for, given a project canvas.
///
/// # Why this exists, and why it is NOT a second place a dimension is minted
///
/// Phase 60 (OCCL-01): the occlusion predicate decides in the gather — where the
/// only size it knows is the project canvas — whether a layer paints every pixel
/// of what the compositor will actually target. It has to be conservative across
/// every size the producer might pick, because [`ResLevel::scale_dims`] is not a
/// uniform scaling: it right-shifts each axis independently and floors at
/// [`MIN_DIM`], so a degraded canvas is not always the same SHAPE as the full
/// one, and a layer that fills the canvas can letterbox inside the half-canvas.
///
/// It lives HERE, beside the ladder, for two reasons:
///
/// 1. **Correctness.** A consumer that hard-coded `[Full, Half, Quarter]` in its
///    own file would go silently stale the day a rung is added — and "stale" for
///    this consumer means culling a layer that no longer covers, i.e. changed
///    pixels. `enumeration_slot`'s wildcard-free match makes that a compile
///    error instead.
/// 2. **The D-12 reachability claim.** `scale_dims` still has exactly ONE
///    production call site that MINTS a dimension something composites at (the
///    producer's multi-layer arm in `ring.rs`), and
///    `dynres_pin.rs::d12_scale_dims_has_exactly_one_production_call_site` still
///    asserts precisely that, unweakened. This function mints nothing: it hands
///    back a set for INSPECTION, and its caller reduces it to a `bool`. Keeping
///    the enumeration inside the module that owns the ladder is what lets both
///    things be true at once.
pub fn every_composite_size(canvas_w: u32, canvas_h: u32) -> [(u32, u32); COMPOSITE_SIZE_COUNT] {
    let mut out = [(0u32, 0u32); COMPOSITE_SIZE_COUNT];
    for level in [ResLevel::Full, ResLevel::Half, ResLevel::Quarter] {
        out[level.enumeration_slot()] = level.scale_dims(canvas_w, canvas_h);
    }
    out
}

/// The hysteresis state machine. One per producer thread, owned as a stack
/// local by `producer_loop` — never shared, never stored, never serialized.
///
/// `!Send` by construction (`_not_send`): this type cannot be moved onto another
/// thread, put in a `static`, or shared through an `Arc`. That is the compile-
/// time half of D-12 ("a degraded level must be UNREACHABLE from export, not
/// merely unused by it"); the other half is the crate graph, pinned in
/// `dynres_pin.rs`.
pub struct DynResController {
    level: ResLevel,
    /// Consecutive over-budget ticks since the last in-budget tick or the last
    /// transition.
    miss_streak: u32,
    /// Ticks remaining before another drop is permitted.
    hold_remaining: u32,
    /// Makes the controller `!Send`/`!Sync`. Zero-sized; see the type doc.
    _not_send: PhantomData<*const ()>,
}

impl DynResController {
    pub fn new() -> Self {
        Self {
            level: ResLevel::Full,
            miss_streak: 0,
            hold_remaining: 0,
            _not_send: PhantomData,
        }
    }

    /// One call per PRODUCED multi-layer tick. Returns the level to composite
    /// the NEXT frame at.
    ///
    /// `produce_us` is the wall cost of producing that frame — resolve, session
    /// sync, decode gather, composite — **excluding** any time the producer
    /// spent parked on ring/target backpressure. That exclusion is not a detail:
    /// a producer that is keeping up is, by definition, blocked waiting for the
    /// presenter most of the time, so a raw tick duration reads ~one frame step
    /// on a pipeline with plenty of headroom and would put this controller
    /// permanently on the edge of its own threshold. The caller subtracts the
    /// backpressure wait before calling.
    ///
    /// `budget_us` is the frame step (`engine::frame_step_us(fps)`).
    pub fn on_tick(&mut self, produce_us: u64, budget_us: u64) -> ResLevel {
        if self.hold_remaining > 0 {
            self.hold_remaining -= 1;
        }
        if produce_us <= budget_us {
            // D-09: ONE in-budget tick resets the streak. A drop requires
            // MISS_STREAK_TICKS misses with no recovery in between.
            self.miss_streak = 0;
            return self.level;
        }
        self.miss_streak = self.miss_streak.saturating_add(1);
        if self.miss_streak >= MISS_STREAK_TICKS && self.hold_remaining == 0 {
            match self.level.lower() {
                Some(next) => {
                    self.level = next;
                    self.miss_streak = 0;
                    self.hold_remaining = HOLD_TICKS;
                }
                None => {
                    // Already at the ladder floor (Half since 2026-08-03).
                    // Clamp the streak so it cannot run away over a long
                    // over-budget section; the level stays where it is, and no
                    // amount of further pressure moves it.
                    self.miss_streak = MISS_STREAK_TICKS;
                }
            }
        }
        self.level
    }

    /// Snap back to [`ResLevel::Full`] and forget all history.
    ///
    /// Called by the producer at the point the EXISTING pause/flush handshake
    /// already runs (`ring.rs`'s loop-top `ctl.stop` / gen-bump observation) —
    /// never from a second signal path of its own (Pitfall 8).
    pub fn on_pause(&mut self) -> ResLevel {
        self.level = ResLevel::Full;
        self.miss_streak = 0;
        self.hold_remaining = 0;
        self.level
    }

    /// The level in force right now.
    pub fn level(&self) -> ResLevel {
        self.level
    }
}

impl Default for DynResController {
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

    /// Behavior 1 (D-09): a SINGLE late frame never drops a level, and one
    /// in-budget tick resets an almost-complete streak. 57-01 measured this
    /// machine producing single 33 ms+ frames on a fixture with 16x headroom —
    /// single-frame triggering would thrash there.
    #[test]
    fn eleven_misses_then_one_hit_stays_full() {
        let mut c = DynResController::new();
        for i in 0..(MISS_STREAK_TICKS - 1) {
            assert_eq!(
                c.on_tick(MISS, BUDGET),
                ResLevel::Full,
                "miss {i} of {} must not drop on its own",
                MISS_STREAK_TICKS - 1
            );
        }
        assert_eq!(c.on_tick(HIT, BUDGET), ResLevel::Full, "recovery tick");
        // The streak is genuinely reset, not merely paused: another full run of
        // MISS_STREAK_TICKS - 1 misses still holds Full.
        for _ in 0..(MISS_STREAK_TICKS - 1) {
            assert_eq!(c.on_tick(MISS, BUDGET), ResLevel::Full);
        }
        assert_eq!(c.level(), ResLevel::Full);
    }

    /// Behavior 2: MISS_STREAK_TICKS consecutive misses drop exactly one level,
    /// and the streak counter resets at the transition.
    #[test]
    fn sustained_miss_drops_one_level_and_resets_the_streak() {
        let mut c = DynResController::new();
        for _ in 0..(MISS_STREAK_TICKS - 1) {
            assert_eq!(c.on_tick(MISS, BUDGET), ResLevel::Full);
        }
        assert_eq!(
            c.on_tick(MISS, BUDGET),
            ResLevel::Half,
            "the MISS_STREAK_TICKS-th consecutive miss drops to Half"
        );
        assert_eq!(c.miss_streak, 0, "the streak resets at the transition");
        assert_eq!(c.hold_remaining, HOLD_TICKS, "the hold window opens");
    }

    /// Behavior 3: the hold window still spaces what it always spaced, and
    /// `Half` is where the walk ends — inside the hold nothing drops again, and
    /// PAST the hold nothing drops either, because there is no second rung.
    ///
    /// Both halves matter. The hold-window coverage is kept because the
    /// mechanism is kept ([`HOLD_TICKS`] is still counted down, and a future
    /// rung would depend on it); the past-the-hold assertion is the one that
    /// changed meaning on 2026-08-03 — it used to prove the drop to `Quarter`
    /// happened, and now proves it cannot.
    #[test]
    fn hold_window_elapses_and_half_is_the_floor() {
        let mut c = DynResController::new();
        for _ in 0..MISS_STREAK_TICKS {
            c.on_tick(MISS, BUDGET);
        }
        assert_eq!(c.level(), ResLevel::Half);

        // Every tick inside the hold window is a miss, and none of them drops.
        for i in 0..(HOLD_TICKS - 1) {
            assert_eq!(
                c.on_tick(MISS, BUDGET),
                ResLevel::Half,
                "tick {i} inside the {HOLD_TICKS}-tick hold must not drop again"
            );
        }
        // The tick that USED to drop to Quarter: the hold window has now
        // elapsed, the miss streak is long past MISS_STREAK_TICKS, and the
        // level still does not move — the gate is the LADDER, not the timer.
        assert_eq!(
            c.on_tick(MISS, BUDGET),
            ResLevel::Half,
            "the first miss PAST the hold window must stay at Half: with the \
             ladder floored (2026-08-03) there is no second drop for the hold \
             window to have been spacing"
        );

        // The floor holds under indefinite pressure.
        for _ in 0..(HOLD_TICKS * 3) {
            assert_eq!(c.on_tick(MISS, BUDGET), ResLevel::Half);
        }
        assert_eq!(
            c.level().lower(),
            None,
            "Half is the bottom of the playback ladder"
        );
    }

    /// Behavior 4: pause snaps back to Full from ANY level, with history cleared
    /// (so the next play earns its own degradation from scratch).
    ///
    /// "Any level" means every value of the enum, including the one the ladder
    /// can no longer reach. `Half` is driven there the way playback would — by
    /// ticks — while `Quarter` is INSTALLED directly through the private field
    /// this `#[cfg(test)]` module can see, because since 2026-08-03 no sequence
    /// of ticks arrives at it (see `half_lower_is_none`). Skipping it instead
    /// would be the wrong call: `Quarter` still crosses the C ABI as
    /// discriminant 2, so `on_pause` has to be TOTAL over the enum for the
    /// observable a shell reads to be trustworthy.
    #[test]
    fn pause_snaps_back_to_full_from_any_level() {
        for target in [ResLevel::Half, ResLevel::Quarter] {
            let mut c = DynResController::new();
            match target {
                ResLevel::Quarter => c.level = ResLevel::Quarter,
                _ => {
                    while c.level() != target {
                        c.on_tick(MISS, BUDGET);
                    }
                }
            }
            assert_eq!(c.level(), target);
            assert_eq!(c.on_pause(), ResLevel::Full);
            assert_eq!(c.miss_streak, 0);
            assert_eq!(c.hold_remaining, 0);
            // History cleared: the first miss after a pause starts a fresh
            // streak, so one late frame right after a resume cannot drop.
            assert_eq!(c.on_tick(MISS, BUDGET), ResLevel::Full);
        }
    }

    /// Behavior 5: recovery is PAUSE-ONLY. Hits at Half never climb back
    /// mid-play — the deliberate anti-oscillation choice recorded in the module
    /// doc.
    #[test]
    fn hits_at_half_never_climb_back_mid_play() {
        let mut c = DynResController::new();
        for _ in 0..MISS_STREAK_TICKS {
            c.on_tick(MISS, BUDGET);
        }
        assert_eq!(c.level(), ResLevel::Half);
        for _ in 0..(HOLD_TICKS * 5) {
            assert_eq!(
                c.on_tick(HIT, BUDGET),
                ResLevel::Half,
                "an in-budget tick must never raise the level mid-play"
            );
        }
        assert_eq!(c.level(), ResLevel::Half);
    }

    /// `scale_dims` halves/quarters each axis and floors at MIN_DIM.
    #[test]
    fn scale_dims_halves_quarters_and_floors() {
        assert_eq!(ResLevel::Full.scale_dims(1920, 1080), (1920, 1080));
        assert_eq!(ResLevel::Half.scale_dims(1920, 1080), (960, 540));
        assert_eq!(ResLevel::Quarter.scale_dims(1920, 1080), (480, 270));
        // The floor: a degenerate canvas never yields a 0-sized render target.
        assert_eq!(ResLevel::Quarter.scale_dims(8, 4), (MIN_DIM, MIN_DIM));
        assert_eq!(ResLevel::Quarter.scale_dims(0, 0), (MIN_DIM, MIN_DIM));
    }

    /// [`every_composite_size`] really enumerates EVERY level, and the occlusion
    /// predicate's reason for needing it is a fact rather than a worry: a
    /// degraded canvas is not always the same SHAPE as the full one.
    #[test]
    fn every_composite_size_covers_the_whole_ladder() {
        assert_eq!(
            every_composite_size(1920, 1080),
            [(1920, 1080), (960, 540), (480, 270)]
        );
        // No slot left unwritten — the guard against a level added to the enum
        // but forgotten in the loop (`enumeration_slot` makes that a compile
        // error, and this catches the array-length half of the same mistake).
        for (w, h) in every_composite_size(1920, 1080) {
            assert!(w > 0 && h > 0, "an unenumerated level left a (0,0) slot");
        }
        // The MEASURED reason OCCL-01 must check all three: 26x14 halves to
        // 13x7 and quarters into the MIN_DIM floor, so the aspect changes twice.
        let sizes = every_composite_size(26, 14);
        assert_eq!(sizes, [(26, 16), (16, 16), (16, 16)]);
        let full_aspect = sizes[0].0 as f64 / sizes[0].1 as f64;
        assert!(
            sizes
                .iter()
                .any(|(w, h)| (*w as f64 / *h as f64 - full_aspect).abs() > 1e-6),
            "non-vacuous: the ladder really can change the composite's shape"
        );
    }

    /// **THE FLOOR (2026-08-03).** The whole playback ladder in one pin: `Full`
    /// steps down to `Half`, and `Half` is the bottom — `lower()` returns `None`
    /// there, so no run of misses, however long, can reach `Quarter`.
    #[test]
    fn half_lower_is_none() {
        assert_eq!(
            ResLevel::Full.lower(),
            Some(ResLevel::Half),
            "the one rung the playback ladder still has"
        );
        assert_eq!(
            ResLevel::Half.lower(),
            None,
            "Half is the playback floor: Quarter is excluded from the ladder \
             (owner UAT 2026-08-03)"
        );
    }

    /// The ABI contract: the discriminants crossing the C boundary are 0/1/2.
    #[test]
    fn discriminants_are_the_abi_contract() {
        assert_eq!(ResLevel::Full as u8, 0);
        assert_eq!(ResLevel::Half as u8, 1);
        assert_eq!(ResLevel::Quarter as u8, 2);
    }

    /// A budget miss is STRICTLY over budget — a tick that exactly fills the
    /// frame step is on time, not late.
    #[test]
    fn exactly_on_budget_is_a_hit() {
        let mut c = DynResController::new();
        for _ in 0..(MISS_STREAK_TICKS * 4) {
            assert_eq!(c.on_tick(BUDGET, BUDGET), ResLevel::Full);
        }
    }
}
