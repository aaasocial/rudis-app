//! The program-level render-cache **READER** (Phase 59, plan 59-05; decisions
//! D-14, D-16, D-17, D-19, and the reader half of D-40).
//!
//! # The altitude, stated so it cannot blur (D-19)
//!
//! | | [`crate::decode_source`] (Phase 58) | this module (Phase 59) |
//! |---|---|---|
//! | Unit | ONE clip's source media | ONE time-range of the **composited program** |
//! | Question | *"what media should this CLIP's session open?"* | *"can this whole TICK skip compositing?"* |
//! | Cheapens | decoding an input | *the whole composite*, layers included |
//! | Invalidated by | the source file changing | **any edit that changes what the range composites to** |
//!
//! D-19 makes the consequence a rule: the render-cache lookup is **its own
//! module at its own altitude, NOT an extension of `decode_source.rs`**.
//! Jamming a `Cache` variant into the clip-level seam would put program-level
//! state inside a per-clip resolver and break Phase 58 D-16's
//! one-file-knows-about-proxies property. Instead this module **calls** that
//! seam, once per visible clip, and takes its answer as plain data — which is
//! exactly what keeps `decode_source.rs` the only file in this crate that knows
//! what a playback proxy is.
//!
//! # D-16 — the edit counter is the TRIGGER, the hash is the TRUTH
//!
//! `PreviewEditSeq.seq` is a cheap counter the FFI layer bumps at every command
//! dispatch. This module **LOADS** it and uses it only as a memo key: a bump
//! says *"look again"*, never *"this is stale"*. What decides freshness is the
//! recomputed segment hash compared against the identity stored inside the
//! cached segment's own meta. Using the edit signal alone as truth would be one
//! missed listener away from presenting a stale frame.
//!
//! **This module must never DRAIN the pending-invalidation set** that lives
//! beside that counter. That set belongs to the presenter, which takes it to
//! decide whether its ring needs flushing; a lookup that consumed it would eat
//! the presenter's own signal, and the symptom would be a stale frame on screen
//! after an edit — the precise failure D-16 splits TRIGGER from TRUTH to
//! prevent.
//!
//! The rule is pinned by a literal text scan of this file, so this module also
//! avoids NAMING the draining method in prose. A doc comment that mentioned it
//! would read as a violation — correctly, since the scan cannot tell prose from
//! a call, and teaching it to would weaken the only thing standing between the
//! claim and a comment. `dynres.rs` records the same footgun for the same
//! reason, having been bitten by it three times in one phase.
//!
//! # D-17 — memoized per `(segment index, material generation)`, and MEASURED
//!
//! The material generation is the PAIR `(store edit generation, decode-answer
//! generation)` — 59-REVIEW WR-02's correction, since half the material this
//! module hashes comes from the filesystem and the store's counter cannot see
//! it moving. See [`MaterialGen`].
//!
//! Assembling a segment's key material walks every clip visible anywhere in the
//! segment and resolves each one's decode source. 58-09 measured ONE such
//! clip-level resolve at **730-840 us**, running 2.59 times per composited tick
//! and costing ~4 % of the producer's wall time. A 2 s segment is 60 ticks at
//! 30 fps, so an unmemoized program-level lookup would pay that walk 60 times
//! over the very segment it exists to make free. The memo makes the amortized
//! per-tick cost one hash-map probe.
//!
//! D-32 asks for the number to be **instrumented and printed, not assumed in
//! either direction** — in either direction is the operative phrase, since
//! "surely it's cheap" is how the 58-09 finding got missed for a whole phase.
//! [`RENDER_CACHE_LOOKUP_NANOS`] / [`RENDER_CACHE_LOOKUPS`] /
//! [`RENDER_CACHE_HITS`] / [`RENDER_CACHE_MISSES`] plus
//! [`RENDER_CACHE_HASH_WALKS`] / [`RENDER_CACHE_HASH_NANOS`] carry it; 59-10
//! publishes it.
//!
//! # D-40 — this is ONE of exactly TWO files here that may name the cache crate
//!
//! The other is 59-07's composite-side WRITER. The ring, the compositor, the
//! coordinator, the presenter and `decode_source.rs` stay entirely ignorant
//! that a render cache exists, and a source-level scan pins that — the number
//! and the two filenames both.
//!
//! # The SERVING half (plan 59-06)
//!
//! [`SegmentLookup`] answers the question; [`CacheServe`] is what the producer
//! actually holds. It owns the memoized lookup plus at most two SOFTWARE
//! segment decode sessions (the current one and A2's prewarmed next), and its
//! whole public surface is [`CacheServe::try_serve`] — "here is `prod_pos`,
//! give me a frame or tell me to composite live".
//!
//! Two properties are structural rather than intended:
//!
//! * **Never a hardware session.** `MAX_HW_SESSIONS = 3` is the exact resource
//!   the six-layer ceiling saturates, so a cache decode that took one of those
//!   slots would be paying for its own win. A source scan pins that this file
//!   names no hardware-session symbol at all; 59-01's A1 spike is why it does
//!   not need one (209.8 fps against a 30 fps demand, 0 late pulls in 300
//!   tick-paced demands).
//! * **Never a new GPU path.** A served frame is handed back as a plain
//!   [`engine::Frame`] and the producer wraps it as ONE `MixedLayer::Cpu`
//!   through the same composite call the live gather makes. Nothing here
//!   composites, presents, or knows what a ring is.
//!
//! # LEAVING a cached run (plan 59-13, `deferred-items § D-4`)
//!
//! A cache-hit tick deliberately skips the live reconciliation, so across a
//! cached span the layer pool's `ffmpeg` children sit blocked at stale source
//! positions and the first LIVE tick after the span respawns them cold. 59-10
//! measured that as a **729.78 ms** stall at the cache exit against a
//! **42.62 ms** control at the identical position — the phase's headline
//! negative, and a stall this design MOVED rather than removed.
//!
//! Three fixes were priced in D-4. This module implements **(b)**: the A2
//! branch below already probes segment `k + 1` one lookahead horizon ahead of
//! the boundary, so the arm where that probe misses IS the cache→live boundary,
//! and [`CacheServe::take_exit_boundary`] hands it to the producer, which warms
//! the live pool toward it through the boundary-prewarm machinery that already
//! serves clip cuts.
//!
//! ## The disposition table, as it stands after plan 59-16
//!
//! | option | disposition |
//! |---|---|
//! | **(a)** keep the layer pool advancing through cached ticks | **REJECTED** (59-13). It spends, on every cached tick, exactly the decode the cache exists to save, to buy one boundary. |
//! | **(b)** prewarm the live pool toward the cache→live boundary | **IMPLEMENTED** (59-13), **cost-fixed** (59-16). 59-14 measured it buying `731.05 → 289.54 ms` off the exit at a cost of 13–15 dropped frames a run and `sw_sidecar_spawns` +3; 59-16 removed both costs at their source in `ring.rs::prewarm_boundary_stack` (predict the hardware claim and do not warm it; seed the pending pool's dims from the live one). |
//! | **(c)** fix the engine layer pool's `FramePull::Ended` cold-decode arm | **IMPLEMENTED** (59-15), under explicit owner authorization for one pass scoped to `crates/engine/src/decoder_pool.rs` plus its new test file. It was an OWNER FINDING here through 59-13 and 59-14; it is no longer open. A three-layer stream-end tick fell `771.5 → 145.7 ms` and the 4K Long-GOP arm `859.8 → 118.0 ms`, at MAD **0.0000** frame- and composite-level parity. |
//!
//! **(c) did not make (b) unnecessary, and that was measured rather than
//! assumed.** 59-16's deciding run took the committed pair with the shipped
//! `RUDIS_DISABLE_BOUNDARY_PREWARM` control ON TOP of 59-15's engine fix: the
//! cache exit still read **717.62 ms** (median of five; `[760.90, 717.62,
//! 713.57, 697.10, 764.46]`) against a 66.50 ms bar. The two stalls are
//! different mechanisms that happened to cost the same order of magnitude — (c)
//! was a stream END on the producer thread, and this one is a cold pool BUILD
//! (`pool.is_none()` at the boundary tick → one serialized `ffmpeg` spawn per
//! software layer). 59-15 moved the first and left the second exactly where it
//! was; on the same run the fixture's clip cuts fell `678.23 → 43.53 ms`.
//!
//! # Fail-closed, everywhere, silently
//!
//! Every answer here is an `Option` and every doubt is `None`: no directory
//! configured, no store, a clip whose media cannot be stat-ed, a segment whose
//! file is absent / stale / truncated / corrupt / wrong-geometry /
//! wrong-encoder. D-21 forbids playback failing because a *cache* is bad, and
//! there is nothing for the producer to handle: the answer to every failure is
//! "composite live".

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant, UNIX_EPOCH};

use rendercache::key::{ClipKeyMaterial, DecodeAnswerTag, MediaIdentity, SegmentKeyInputs};

use crate::decode_source::{resolve_decode_source, DecodeSourceKind};
use crate::PreviewHost;

/// Bound on the per-generation hash memo.
///
/// One [`SegmentLookup`] is owned by one producer, and the memo is cleared
/// whenever the edit generation moves, so it can only fill with segments from a
/// single generation. 512 segments is ~17 minutes of program at the 2 s grid
/// pitch — far more than one uninterrupted playback pass — so eviction is a
/// safety net for a scrub-heavy session rather than a routine.
///
/// It carries a bound anyway, for the same reason
/// `render_cache_detect::MAX_TRACKED_SEGMENTS` does: a map keyed by unbounded
/// segment indices that lives on the producer's own path is exactly the shape
/// that should not be able to grow without limit.
pub const MAX_MEMOIZED_SEGMENTS: usize = 512;

/// The render-cache directory this process probes against, or `None` — which is
/// the STARTING state and means "there is no cache", i.e. exactly the behaviour
/// of every build before this module existed.
///
/// `RwLock` rather than `OnceLock`, for the two reasons `decode_source.rs`'s
/// own configuration slot states: the host sets it idempotently (the
/// app-context that knows the cache root is not available at process start), and
/// integration tests reconfigure per fixture inside one process. The cost is one
/// uncontended read-lock acquisition per probe, and that cost is charged to
/// [`RENDER_CACHE_LOOKUP_NANOS`] along with everything else, so it is measured
/// rather than argued about.
///
/// `Option` rather than a "point it at a path that cannot exist" convention:
/// this one genuinely needs an unset, because `configure_render_cache_dir(None)`
/// is how a host turns the cache off.
static RENDER_CACHE_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Point this process's probes at a render-cache directory, or unset it.
///
/// Idempotent; a later call simply overwrites. The directory need not exist and
/// need not contain anything: a configured but empty directory misses on every
/// segment, exactly like an unconfigured process, at the cost of one extra
/// `stat`. Nothing here creates, validates or sweeps the directory — the cache
/// owns its own disk, and `app-core` (plan 59-08) passes
/// `AppCtx::app_cache_dir()` joined with the cache crate's own directory name.
pub fn configure_render_cache_dir(dir: Option<PathBuf>) {
    match RENDER_CACHE_DIR.write() {
        Ok(mut slot) => *slot = dir,
        // A poisoned lock means some thread panicked while holding it. The
        // stored value is intact (this module only ever assigns a whole
        // `Option<PathBuf>`), so recover rather than panic: a configuration
        // setter that could kill its caller would be a worse failure than any
        // cache problem it exists to enable.
        Err(poisoned) => *poisoned.into_inner() = dir,
    }
}

/// The configured directory, or `None`.
///
/// `pub(crate)` since plan 59-07: the composite-side WRITER
/// ([`crate::render_cache_writer`]) commits into the SAME directory this reader
/// probes, and it reads it from HERE rather than taking one as a parameter —
/// one source of truth, so a writer and a reader can never be pointed at two
/// different places. In-crate only; nothing about the public surface changes.
pub(crate) fn configured_dir() -> Option<PathBuf> {
    match RENDER_CACHE_DIR.read() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Is a render-cache directory configured at all — WITHOUT cloning the path.
///
/// [`CacheServe::try_serve`] asks this before it does anything else, so a
/// process that never configured a cache pays one uncontended read-lock per
/// produced tick and not one byte more: no store lock, no probe, no atomic, no
/// allocation. That is the compatibility floor stated as a cost rather than as
/// an intention (`RENDER-CACHE-LOOKUP unconfigured … mean_ns=59`).
fn is_configured() -> bool {
    match RENDER_CACHE_DIR.read() {
        Ok(slot) => slot.is_some(),
        Err(poisoned) => poisoned.into_inner().is_some(),
    }
}

/// Which segment on the cache's fixed program-time grid `prod_pos` falls in.
///
/// A pass-through to the cache crate's own grid function, re-exported from HERE
/// so the producer can attribute a tick to a segment — for
/// [`crate::render_cache_detect::note_live_tick`] and
/// [`crate::render_cache_detect::prearm_stack`] — without naming that crate.
/// D-40 allows exactly two files in this crate to name it, and `ring.rs` is not
/// one of them; a source scan pins that, and a scan cannot tell a grid query
/// from cache I/O.
pub fn segment_index_for(prod_pos: i64) -> i64 {
    rendercache::key::segment_index_for(prod_pos)
}

/// Cumulative wall-clock nanoseconds spent inside [`SegmentLookup::probe`],
/// across every probe in this process.
///
/// **Row 22's instrument.** Covers the whole call: the configuration read, the
/// memo probe, the key-material walk when one happens, and the cache crate's
/// own two stats plus one bounded meta read. Divide by [`RENDER_CACHE_LOOKUPS`]
/// for a mean.
///
/// Relaxed, and read as a DELTA around a span (snapshot, act, snapshot) — the
/// absolute value carries every probe since process start, including other
/// tests sharing a binary. It is a diagnostic, never a synchronisation edge, and
/// nothing branches on it. Same discipline as
/// `decode_source::PROXY_RESOLVE_NANOS`, `engine::audio::AUDIO_STREAM_OPENS` and
/// `engine::hwdecode::HW_OPEN_COUNT`.
pub static RENDER_CACHE_LOOKUP_NANOS: AtomicU64 = AtomicU64::new(0);

/// Calls to [`SegmentLookup::probe`] — the denominator for
/// [`RENDER_CACHE_LOOKUP_NANOS`], and equal to
/// [`RENDER_CACHE_HITS`] + [`RENDER_CACHE_MISSES`] by construction.
pub static RENDER_CACHE_LOOKUPS: AtomicU64 = AtomicU64::new(0);

/// Probes that found a fresh cached segment. **The attribution handle**: a
/// green cache test that never served a segment proves nothing, and PROXY-02's
/// lesson one altitude up is that the thing hidden from the USER must stay
/// fully visible to verification.
pub static RENDER_CACHE_HITS: AtomicU64 = AtomicU64::new(0);

/// Probes that answered "composite live", for any of D-21's reasons or because
/// no directory is configured. The half that makes a hit claim non-vacuous:
/// "the cache served it" means nothing without "and the live path did not".
pub static RENDER_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// Key-material assemblies actually performed — the memo's whole point,
/// expressed as a number.
///
/// With D-17 honoured this rises at most once per `(segment, generation)`. If it
/// ever tracks [`RENDER_CACHE_LOOKUPS`], the memo has been defeated and the
/// lookup is paying a clip-level resolve per visible clip per tick.
pub static RENDER_CACHE_HASH_WALKS: AtomicU64 = AtomicU64::new(0);

/// Cumulative wall-clock nanoseconds spent inside the key-material assembly —
/// the expensive half of [`RENDER_CACHE_LOOKUP_NANOS`], separated out so 59-10
/// can report the amortized per-tick tax and the per-walk cost as two numbers
/// instead of one blended one.
pub static RENDER_CACHE_HASH_NANOS: AtomicU64 = AtomicU64::new(0);

/// Frames actually SERVED from a cached segment (plan 59-06) — i.e. ticks the
/// producer composited from cache bytes instead of from live layers.
///
/// Distinct from [`RENDER_CACHE_HITS`] on purpose, and the gap between the two
/// is the phase's most useful diagnostic: a probe can HIT and the serve can
/// still fall back (D-21) when the segment file will not open, will not decode,
/// runs out of frames, or answers with the wrong geometry. `HITS - SERVED` is
/// exactly that population.
pub static RENDER_CACHE_SERVED_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Ticks where a probe HIT but the serve fell back to compositing live.
///
/// D-21's fallback is total, silent and per-segment — playback never fails
/// because a *cache* is bad — so this counter is the only place such an event
/// is visible at all. It moving is not a failure; it moving on EVERY tick is.
pub static RENDER_CACHE_SERVE_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Segment decode sessions opened (cold entries + prewarms).
///
/// The anti-storm handle: a corrupt-but-readable segment that failed to open
/// must not cost one `ffprobe` + one `ffmpeg` spawn per tick, so
/// [`CacheServe`] latches the failure per segment and this counter is what
/// proves the latch holds.
///
/// 59-REVIEW **WR-07**: the latch covers two events, not one. A failed
/// `open_session` was always latched; a payload that OPENS and then stops
/// producing frames before its own last written index was not, and cost exactly
/// the same spawn-per-tick storm by a different route. Both now latch; a short
/// TAIL still does not.
pub static RENDER_CACHE_SEG_OPENS: AtomicU64 = AtomicU64::new(0);

/// Of those opens, the ones made AHEAD of the boundary (A2's prewarm).
///
/// 59-CEILING-VERDICT § 4 measured ~180 ms of cold segment entry
/// (`session_open_ms ≈ 75` + `first_pull_ms ≈ 97-105`) and says plainly that
/// un-hidden, at every `SEG_US` boundary, that would be worse than the ceiling
/// it is meant to remove. This counter is how a later plan checks the hiding
/// actually happened.
pub static RENDER_CACHE_SEG_PREWARMS: AtomicU64 = AtomicU64::new(0);

/// How many times the producer prewarmed the LIVE layer pool toward a
/// cache→live exit — the instrument that proves `deferred-items § D-4`
/// option (b) actually fires (plan 59-13).
///
/// Compare it against the number of exit boundaries a run crosses: one per
/// boundary is the design, because the discovery rides
/// [`CacheServe::prewarmed_for`]'s one-per-boundary latch and the value is
/// handed out at most once by [`CacheServe::take_exit_boundary`].
///
/// **It counts prewarms ISSUED, and it is deliberately NOT incremented when
/// `RUDIS_DISABLE_BOUNDARY_PREWARM` is engaged.** That switch is the disclosed
/// control a differential is measured against, and a control that leaves the
/// instrument moving cannot tell its two arms apart. The existing
/// `prewarm_boundary_stack` may still decline an issued prewarm for its own
/// reasons (the boundary does not resolve to a multi stack, the stack has no
/// pooled sources, the frame-step bucket is already latched); those show up as
/// a prewarm that issued and warmed nothing, never as a second counter.
pub static RENDER_CACHE_EXIT_PREWARMS: AtomicU64 = AtomicU64::new(0);

/// Cumulative wall-clock nanoseconds spent inside [`CacheServe::try_serve`] —
/// probe, session management, prewarm and the frame pull, everything a served
/// tick actually pays. 59-10 reports it INSIDE the win rather than beside it
/// (58-09's discipline).
pub static RENDER_CACHE_SERVE_NANOS: AtomicU64 = AtomicU64::new(0);

/// The producer's per-tick render-cache question, memoized per
/// `(segment index, edit generation)` (D-17).
///
/// Owned by ONE producer as a stack local, exactly as `DynResController` is;
/// nothing here is shared, and the memo is deliberately per-lookup rather than
/// process-global so two producers can never observe each other's answers about
/// two different projects.
pub struct SegmentLookup {
    /// `(segment, generation) -> hash`, with `None` recorded for a segment that
    /// could not be hashed at all. Caching the negative is deliberate: a clip
    /// whose media is gone will still be gone on the next tick, and re-walking
    /// the whole segment 30 times a second to re-learn that is precisely the
    /// stat storm D-17 exists to prevent. The next edit clears it.
    memo: HashMap<(i64, MaterialGen), Option<u64>>,
    /// Insertion order, so [`MAX_MEMOIZED_SEGMENTS`] can evict the oldest.
    order: VecDeque<(i64, MaterialGen)>,
    /// The generation every entry in [`Self::memo`] belongs to.
    generation: Option<MaterialGen>,
}

/// **59-REVIEW WR-02.** The full invalidation key for a memoized segment
/// identity: `(store edit generation, decode-answer generation)`.
///
/// The store's counter alone does not cover the material. Two of the terms
/// [`assemble_segment_hash`] gathers come from the FILESYSTEM rather than from
/// the store — the media's `MediaIdentity` and the resolved
/// [`DecodeAnswerTag`] — and both can move with no mutation to announce it. The
/// case that matters is D-14's own: a proxy generation completing dispatches
/// nothing, so a memo keyed on the store counter alone keeps handing back the
/// pre-proxy hash, `read_fresh_segment` keeps finding the pre-proxy file, and
/// the cached range keeps serving an ORIGINALS render while the live ticks
/// either side of it decode proxies. That visible sharpness discontinuity at
/// the boundary is exactly what CACHE-02 forbids and what putting
/// `DecodeAnswerTag` in the material was supposed to make impossible.
///
/// [`crate::decode_source::decode_answer_generation`] is the second axis, and
/// it deliberately lives THERE: what moves is that seam's answer, so the seam
/// owns the signal and this module only reads it. That also keeps the knowledge
/// of what a playback proxy is inside the one file that is allowed to have it.
type MaterialGen = (u64, u64);

impl Default for SegmentLookup {
    fn default() -> Self {
        Self::new()
    }
}

impl SegmentLookup {
    pub fn new() -> Self {
        SegmentLookup {
            memo: HashMap::new(),
            order: VecDeque::new(),
            generation: None,
        }
    }

    /// Segment `seg_index`'s identity under the CURRENT store state, memoized on
    /// `(seg_index, edit_gen)`.
    ///
    /// `canvas` is `(width, height, fps)` — the PROJECT canvas, passed in rather
    /// than read here because the producer has already resolved it for the tick
    /// it is compositing (`MultiLayerStack`'s own three fields, read under the
    /// same store lock as the layers). Reading it again here would be a second
    /// lock acquisition for three scalars the caller is holding.
    ///
    /// `None` means the segment cannot be identified — no store, or a clip whose
    /// media cannot be stat-ed — and therefore can never be served. Fail-closed:
    /// a caller that could not prove a media file is unchanged must not claim a
    /// cached render of it is current.
    ///
    /// # The generation is a TRIGGER, and clearing on it is the bound
    ///
    /// The generation is IN the key, so a stale entry could never be *read* even
    /// if nothing cleared it. It is *also* cleared wholesale when the generation
    /// moves, which is what keeps the memo holding one generation's worth of
    /// segments rather than every generation the session has seen. Belt and
    /// braces, and the belt is the one that carries the correctness.
    ///
    /// **That claim is only true for the WHOLE key** (59-REVIEW WR-02). The
    /// store's `edit_gen` covers the store-derived half of the material; the
    /// filesystem-derived half — the media identity and the resolved decode
    /// answer — moves on its own axis, which is why the key is a
    /// [`MaterialGen`] pair rather than one counter. See that type.
    pub fn segment_hash_memo(
        &mut self,
        host: &dyn PreviewHost,
        seg_index: i64,
        edit_gen: u64,
        canvas: (u32, u32, f64),
    ) -> Option<u64> {
        let material_gen: MaterialGen =
            (edit_gen, crate::decode_source::decode_answer_generation());
        if self.generation != Some(material_gen) {
            self.memo.clear();
            self.order.clear();
            self.generation = Some(material_gen);
        }
        if let Some(known) = self.memo.get(&(seg_index, material_gen)) {
            return *known;
        }

        let started = Instant::now();
        RENDER_CACHE_HASH_WALKS.fetch_add(1, Ordering::Relaxed);
        let computed = assemble_segment_hash(host, seg_index, canvas);
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        RENDER_CACHE_HASH_NANOS.fetch_add(nanos, Ordering::Relaxed);

        while self.memo.len() >= MAX_MEMOIZED_SEGMENTS {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.memo.remove(&oldest);
                }
                None => break,
            }
        }
        self.memo.insert((seg_index, material_gen), computed);
        self.order.push_back((seg_index, material_gen));
        computed
    }

    /// **The producer's question.** Is there a fresh cached segment covering the
    /// tick at `prod_pos`?
    ///
    /// `None` — meaning "composite live" — for every one of: no directory
    /// configured, the segment unhashable, and the cache crate's own total
    /// fail-closed verdict (absent, stale, truncated, corrupt, wrong version,
    /// wrong grid pitch, wrong geometry, wrong cadence, wrong encoder). This
    /// function adds NO trust of its own: it forwards `None` on any doubt, and
    /// the identity comparison that makes a stale segment a MISS happens inside
    /// the cache crate against the identity stored in the segment's own meta.
    ///
    /// The unconfigured case short-circuits BEFORE the walk — no store lock, no
    /// stat, no clip-level resolve — which is what keeps every process that has
    /// never configured a cache byte-identical in behaviour and near-identical
    /// in cost to the day before this module existed.
    pub fn probe(
        &mut self,
        host: &dyn PreviewHost,
        prod_pos: i64,
        edit_gen: u64,
        canvas: (u32, u32, f64),
    ) -> Option<rendercache::cache::SegmentHit> {
        let started = Instant::now();
        let hit = self.probe_inner(host, prod_pos, edit_gen, canvas);

        // Measured, not assumed, and in BOTH directions (D-32). The span covers
        // everything a call site actually pays, the configuration read included.
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        RENDER_CACHE_LOOKUP_NANOS.fetch_add(nanos, Ordering::Relaxed);
        RENDER_CACHE_LOOKUPS.fetch_add(1, Ordering::Relaxed);
        if hit.is_some() {
            RENDER_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
        } else {
            RENDER_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    /// [`Self::probe`]'s decision, with the counting lifted out so every exit
    /// path is instrumented identically and none of them can forget.
    fn probe_inner(
        &mut self,
        host: &dyn PreviewHost,
        prod_pos: i64,
        edit_gen: u64,
        canvas: (u32, u32, f64),
    ) -> Option<rendercache::cache::SegmentHit> {
        // FIRST, and deliberately: an unconfigured process must not walk the
        // timeline, take the store lock or stat anything at all.
        let dir = configured_dir()?;
        let seg_index = rendercache::key::segment_index_for(prod_pos);
        let hash = self.segment_hash_memo(host, seg_index, edit_gen, canvas)?;
        // THE one render-cache read call in this crate. Two stats plus one
        // bounded meta read; it opens no video and starts no child process,
        // which that crate pins at the source level precisely because this runs
        // on the producer's per-tick path.
        rendercache::cache::read_fresh_segment(
            &dir,
            seg_index,
            hash,
            canvas.0,
            canvas.1,
            canvas.2,
        )
    }
}

/// One clip's contribution, gathered under the store lock and carried out of it
/// as OWNED data.
///
/// The split exists because the two halves cannot share a lock: the walk needs
/// the store, and the per-clip work that follows it (a `canonicalize`, two
/// `metadata` calls, and a resolve that reads the proxy cache's meta) is
/// filesystem I/O. Holding the store lock across that would block every command
/// dispatch for the duration of a disk read — the mistake `resolve_multilayer`
/// already avoids by returning an owned `MultiLayerStack`.
struct PendingClip {
    clip: rudis_core::Clip,
    track_index: usize,
    clip_index: usize,
    /// `None` for a TEXT clip, which rasterizes its own payload and has no
    /// media file at all. A clip with a media file always carries its path here
    /// — a clip whose media item is missing from the bin aborts the whole
    /// assembly instead.
    media_path: Option<PathBuf>,
}

/// The D-15 key material for segment `seg_index`, assembled from the CURRENT
/// store state, hashed.
///
/// Two-phase by construction: gather under one store lock, then do the I/O.
fn assemble_segment_hash(
    host: &dyn PreviewHost,
    seg_index: i64,
    canvas: (u32, u32, f64),
) -> Option<u64> {
    let (seg_start, seg_end) = rendercache::key::segment_bounds(seg_index);

    // ---- phase 1: ONE store lock. Acquire, walk, release. ----
    let pending: Vec<PendingClip> = {
        let guard = host.store()?;
        let timeline = guard.timeline();
        let mut pending = Vec::new();
        for (track_index, track) in timeline.tracks.iter().enumerate() {
            // VIDEO lanes only, and the index is the RAW index into `tracks` —
            // both exactly as `Timeline::active_layers_at` does it, so the
            // z-order term in the material is the compositor's own ordering
            // rather than a second opinion about it. An audio lane composites
            // nothing, and including it would make a pure audio rearrangement
            // invalidate a segment whose pixels cannot have changed (D-08).
            if track.kind != rudis_core::TrackKind::Video {
                continue;
            }
            for (clip_index, clip) in track.clips.iter().enumerate() {
                // RQ5's range-overlap filter: every clip visible ANYWHERE in the
                // segment, not merely at one sampled position. Half-open on both
                // sides, matching the segment grid's own `[start, end)`.
                if !(clip.start_us < seg_end && clip.timeline_end_us() > seg_start) {
                    continue;
                }
                let media_path = if clip.text.is_some() {
                    // A TEXT clip's `media_id` is the empty sentinel; it
                    // rasterizes rather than decodes. Handled BEFORE the media
                    // lookup, exactly as `resolve_multilayer` handles it, or
                    // `media_item("")` would answer `None` and abort below.
                    None
                } else {
                    match guard.media_item(&clip.media_id) {
                        Some(item) => Some(PathBuf::from(&item.path)),
                        // Fail-closed: a clip pointing at a bin entry that is
                        // not there cannot be identified, so neither can the
                        // segment. `resolve_multilayer` lets black show through
                        // for this case; a cache must not cache that guess.
                        None => return None,
                    }
                };
                pending.push(PendingClip {
                    clip: clip.clone(),
                    track_index,
                    clip_index,
                    media_path,
                });
            }
        }
        pending
    };

    // ---- phase 2: the I/O, with no lock held ----
    let mut clips = Vec::with_capacity(pending.len());
    for entry in &pending {
        let (media, decode) = match &entry.media_path {
            None => (MediaIdentity::absent(), DecodeAnswerTag::Original),
            Some(path) => {
                // Stat'd FRESH, the same way the playback-proxy cache computes
                // its own key: canonical path + `mtime_ns` + `size_bytes`.
                // `mtime_ns`, not `mtime_ms` — Windows file timestamps advance
                // in coarse (~15.6 ms) steps, so a millisecond-truncated stamp
                // can compare EQUAL across two genuinely different writes.
                let media = media_identity(path)?;
                // D-14: the resolved decode-source answer is part of the
                // segment's identity, because a cached range rendered from
                // originals while the surrounding live playback decodes proxies
                // would be a visible sharpness discontinuity at the boundary —
                // which is exactly what CACHE-02 forbids. The seam is CALLED;
                // the knowledge of what a proxy is stays in it.
                let resolved = resolve_decode_source(&entry.clip.id, path, entry.clip.in_us);
                // WILDCARD-FREE on purpose. `DecodeSourceKind` carries a
                // tripwire in its own crate for exactly this: a third variant
                // must fail to compile HERE too, rather than silently mapping
                // to `Original` and letting a segment rendered from something
                // else wear an identity that says it was not.
                let decode = match resolved.kind {
                    DecodeSourceKind::Original => DecodeAnswerTag::Original,
                    DecodeSourceKind::Proxy { proxy_w, proxy_h } => DecodeAnswerTag::Proxy {
                        w: proxy_w,
                        h: proxy_h,
                    },
                };
                (media, decode)
            }
        };
        clips.push(ClipKeyMaterial::from_clip(
            &entry.clip,
            entry.track_index,
            entry.clip_index,
            media,
            decode,
        ));
    }

    Some(rendercache::key::segment_hash(&SegmentKeyInputs {
        canvas_w: canvas.0,
        canvas_h: canvas.1,
        fps: canvas.2,
        seg_index,
        clips,
    }))
}

/// One media file's identity, computed the way the playback-proxy cache's own
/// `key_for` computes it — canonical path, `mtime_ns`, `size_bytes`.
///
/// Not shared with that crate, deliberately: this returns the cache crate's own
/// plain-data [`MediaIdentity`], and reaching into a *different* cache's key
/// type to build it would be a second file in this crate that knows how
/// playback proxies are identified. The technique is copied; the type is not.
///
/// `None` means "not identifiable" — a stat failure, a non-file, an unreadable
/// or absurd timestamp — and it makes the whole SEGMENT unhashable. That is the
/// fail-closed direction: a caller that cannot see a file must not claim a
/// render of it is current.
///
/// **Known consequence, recorded rather than discovered later:** an imported
/// numbered image sequence stores a `%0Nd` pattern as its path, which cannot be
/// stat-ed, so any segment containing one is never cached. That is correct and
/// safe (it costs speed, never a wrong frame) and it is the same shape as the
/// proxy cache's own answer for such media.
fn media_identity(path: &Path) -> Option<MediaIdentity> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let meta = std::fs::metadata(&canonical).ok()?;
    if !meta.is_file() {
        return None;
    }
    let since_epoch = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    let mtime_ns = u64::try_from(since_epoch.as_nanos()).ok()?;
    // `0` means "unknown" and is never a trustworthy staleness key.
    if mtime_ns == 0 {
        return None;
    }
    Some(MediaIdentity {
        canonical_path: canonical.to_string_lossy().into_owned(),
        mtime_ns,
        size_bytes: meta.len(),
    })
}

// ==========================================================================
// The SERVING session (Phase 59, plan 59-06) — D-19's producer-side half
// ==========================================================================

/// How long one bounded pull from a segment session may block before the serve
/// gives up and lets the tick composite LIVE.
///
/// Generous on purpose, for `POOL_PULL_TIMEOUT`'s reason one altitude up: a
/// pull returns the moment a frame arrives, so a long bound never delays
/// production — it only caps how long a genuinely-stalled segment decode can
/// hold the producer between control checks. 59-CEILING-VERDICT § 4 measured
/// the worst single pull on a 1080p all-intra segment at **17.9 ms** against a
/// 66.5 ms budget, and ~105 ms for the FIRST pull after a cold open, so a bound
/// of one second is two orders of magnitude of headroom over the measurement
/// and still bounded.
const SEG_PULL_BUDGET: Duration = Duration::from_millis(1_000);

/// Slice of [`SEG_PULL_BUDGET`] spent inside one blocking `recv`, between
/// control checks — the same "poll, never join" discipline the target checkout
/// and the hardware freshness poll already use (57-RESEARCH Pitfall 4).
const SEG_PULL_SLICE: Duration = Duration::from_millis(20);

/// How far a tick's demanded frame index may run ahead of the open session
/// before the session is REOPENED (seeked) rather than fast-forwarded.
///
/// Normal service advances exactly one frame per tick. A larger jump means the
/// producer lost time somewhere, and pulling N frames to discard N-1 of them
/// would spend the segment's whole headroom catching up. The payload is
/// all-intra (D-05), so a reopen seeks exactly and costs one session open —
/// which is bounded, unlike an unbounded catch-up loop.
const MAX_SEG_CATCHUP: i64 = 4;

/// One open segment decode — **software only, always**.
///
/// A cache hit must never open a hardware decode session. `MAX_HW_SESSIONS = 3`
/// is the exact resource the D-02 scenario saturates (59-CEILING-VERDICT § 3:
/// six layers, three on hardware, three on the software pool, ~1.0 s of stall
/// at every software-pool cut), so a serve that took a fourth slot would either
/// steal it from a live layer or be refused — self-defeating in both
/// directions. 59-01's A1 spike settled that it does not need one: **209.8 fps
/// against a 30 fps demand on the hard arm, 0 late pulls in 300 tick-paced
/// demands.**
struct SegSession {
    /// Which segment on the fixed grid this session is decoding.
    seg_index: i64,
    session: engine::StreamingDecodeSession,
    /// Index of the NEXT frame this session will yield, counted from the
    /// segment's own frame 0. Advanced by exactly one per successful pull, and
    /// SEEDED at open time from the seek position — so it is the file's real
    /// position, not a hopeful counter.
    next_index: i64,
    /// The geometry the probe promised. A frame that disagrees is a fallback
    /// (D-21's wrong-geometry arm) rather than a stretched picture.
    w: u32,
    h: u32,
}

/// The producer's cache-serving state — one per producer loop, exactly as
/// `DynResController` and the decode `session` are.
///
/// Owns the memoized [`SegmentLookup`], the currently-serving segment session,
/// and at most ONE pre-opened next-segment session (A2's prewarm, mirroring
/// `Lookahead`/`PendingSession`).
pub struct CacheServe {
    lookup: SegmentLookup,
    cur: Option<SegSession>,
    /// Pre-opened for the NEXT segment on the grid; promoted at the boundary.
    next: Option<SegSession>,
    /// One-prewarm-per-boundary latch (the `Lookahead::prewarmed_for` shape):
    /// the segment index already ATTEMPTED, whether or not it produced a
    /// session. Without it, a next segment that is simply not cached would be
    /// re-probed on every tick for the last 800 ms of the current one.
    prewarmed_for: Option<i64>,
    /// A segment whose session could not be opened or could not decode. Latched
    /// so a bad file costs one failed open per segment, not one per tick — the
    /// `failed_clip` backoff `ring.rs` already applies to a clip that will not
    /// start.
    failed_seg: Option<i64>,
    /// Program time of the **cache→live boundary**: set when the A2 prewarm
    /// probe finds segment `k + 1` NOT cached, consumed by the producer to
    /// prewarm the LIVE layer pool toward it (`deferred-items § D-4`
    /// option (b), plan 59-13).
    ///
    /// This is the ONE thing the serve knows that the producer cannot cheaply
    /// work out for itself: the A2 branch already probes the next segment, one
    /// `LOOKAHEAD_US` ahead of the boundary and exactly once per boundary, so
    /// the moment that probe MISSES is the moment the end of the cached run is
    /// known — for free, on the tick that already asked the question.
    ///
    /// It is a prediction, so it is voided by anything that invalidates the
    /// prediction: [`Self::retire`] clears it (a seek, a flush, leaving the
    /// multi range, a probe miss), and the material-generation change above
    /// clears it through the same call.
    exit_at: Option<i64>,
    /// The material generation every field above belongs to — see
    /// [`MaterialGen`]. The FILESYSTEM axis is in here for 59-REVIEW WR-02's
    /// reason: a proxy landing mid-playback must drop the session that is
    /// currently serving an originals render, and no store mutation announces
    /// that.
    generation: Option<MaterialGen>,
}

impl Default for CacheServe {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheServe {
    pub fn new() -> Self {
        CacheServe {
            lookup: SegmentLookup::new(),
            cur: None,
            next: None,
            prewarmed_for: None,
            failed_seg: None,
            exit_at: None,
            generation: None,
        }
    }

    /// Drop every open segment session.
    ///
    /// Called on the producer's flush handshake (a seek re-roots `prod_pos`, so
    /// a session positioned for the old playhead is worthless) and when the
    /// producer leaves a multi-layer range (nothing single-layer is cached this
    /// phase, and an idle `ffmpeg` child holding a segment open is pure cost).
    /// Correctness over warmth, the same call the flush already makes of
    /// `pool` and `Lookahead`.
    pub fn retire(&mut self) {
        self.cur = None;
        self.next = None;
        self.prewarmed_for = None;
        // The exit prediction goes with them (plan 59-13). Every caller of this
        // method is an event that makes "the cached run ends at X" untrue or
        // unknowable: a seek re-roots `prod_pos`, a flush discards the runway,
        // an edit moves the material, a probe miss says the range is not cached
        // at all. Prewarming a live pool toward a boundary that no longer
        // exists would spend real `ffmpeg` children on a stale guess.
        self.exit_at = None;
    }

    /// Take the discovered cache→live boundary, if one is waiting.
    ///
    /// **At most once per discovery**, which — together with the A2 branch's
    /// own `prewarmed_for` latch and the producer-side frame-step bucket latch
    /// in `Lookahead` — is what makes a per-tick prewarm storm impossible
    /// rather than merely unlikely (`deferred-items § D-4` option (b),
    /// threat T-59-13-01).
    ///
    /// The producer is the only caller: it hands the boundary to the SAME
    /// boundary-prewarm machinery that already warms toward a clip cut, so the
    /// first live tick after a cached span adopts warm children instead of
    /// paying the synchronous cold respawn 59-10 measured at 729.78 ms against
    /// a 42.62 ms control at the identical position.
    pub fn take_exit_boundary(&mut self) -> Option<i64> {
        self.exit_at.take()
    }

    /// **The producer's per-tick question, answered as a FRAME.**
    ///
    /// `Some(frame)` means this tick can skip compositing entirely: the frame is
    /// the cached render of `prod_pos`, ready to go through the SAME
    /// `composite_mixed_layers_to_target` + `push_with_lookahead` path a live
    /// composite takes. `None` means "composite live" — for every one of D-21's
    /// reasons and with no distinction between them, because there is nothing
    /// for the producer to handle differently.
    ///
    /// `canvas` is `(width, height, fps)` from the ALREADY-RESOLVED
    /// `MultiLayerStack`, so no second store lock is taken for it. `step` is the
    /// producer's own frame step. `abort` is polled around every blocking wait —
    /// it must answer the producer's `stop`/`gen` question, so a serve can never
    /// out-live a pause or a seek-flush.
    ///
    /// # Cost, by case
    ///
    /// * no cache configured — one uncontended read-lock, nothing else;
    /// * configured, warm, serving — one read-lock, one store lock (the edit
    ///   generation), the cache crate's own two stats + bounded meta read, and
    ///   one frame pull;
    /// * configured, cold segment entry — the above plus ~180 ms of session
    ///   open + first pull, WHICH THE PREWARM EXISTS TO HIDE.
    pub fn try_serve(
        &mut self,
        host: &dyn PreviewHost,
        prod_pos: i64,
        canvas: (u32, u32, f64),
        step: i64,
        abort: &dyn Fn() -> bool,
    ) -> Option<engine::Frame> {
        // FIRST, and deliberately — see `is_configured`. A process that never
        // configured a cache must not take the store lock, must not probe, and
        // must not touch an atomic, so its per-tick behaviour is what it was the
        // day before this module existed.
        if !is_configured() {
            if self.cur.is_some() || self.next.is_some() {
                // D-25: the directory was turned OFF mid-project. Let go of the
                // open segments rather than keeping children alive against a
                // cache that no longer exists.
                self.retire();
            }
            return None;
        }

        let started = Instant::now();
        let served = self.serve_inner(host, prod_pos, canvas, step, abort);
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        RENDER_CACHE_SERVE_NANOS.fetch_add(nanos, Ordering::Relaxed);
        if served.is_some() {
            RENDER_CACHE_SERVED_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        served
    }

    /// [`Self::try_serve`]'s decision, with the timing/counting lifted out so
    /// every exit path is instrumented identically and none of them can forget.
    fn serve_inner(
        &mut self,
        host: &dyn PreviewHost,
        prod_pos: i64,
        canvas: (u32, u32, f64),
        step: i64,
        abort: &dyn Fn() -> bool,
    ) -> Option<engine::Frame> {
        let step = step.max(1);
        let seg_index = rendercache::key::segment_index_for(prod_pos);
        let (seg_start, seg_end) = rendercache::key::segment_bounds(seg_index);
        let edit_gen = current_edit_gen(host);

        // An edit retires every open session BEFORE anything is served from it.
        // The probe below would refuse a stale segment on its own (the hash is
        // the truth), but a session already open on the OLD segment's file would
        // otherwise sit there holding a child for a range that no longer exists.
        //
        // 59-REVIEW WR-02: the FILESYSTEM axis is in this comparison too, so a
        // proxy landing mid-playback drops the open session on the originals
        // render rather than letting it serve out the range.
        let material_gen: MaterialGen =
            (edit_gen, crate::decode_source::decode_answer_generation());
        if self.generation != Some(material_gen) {
            self.generation = Some(material_gen);
            self.retire();
            self.failed_seg = None;
        }

        // Hash-as-truth, EVERY tick. The key-material walk is memoized (D-17);
        // the cache crate's own read deliberately is not, which is what makes
        // "delete the whole directory mid-project" safe (D-25) — 58-07 proved by
        // mutation that memoizing the equivalent proxy answer breaks exactly
        // that promise.
        let Some(hit) = self.lookup.probe(host, prod_pos, edit_gen, canvas) else {
            // **A MISS MUST ALSO LET GO.** Not returning here would leave the
            // open session — and its `ffmpeg` child — holding the segment file
            // for the rest of the range, which breaks D-25's promise in the most
            // literal way available on Windows: an open handle makes the file
            // UNDELETABLE, so a user who deletes the cache mid-project would
            // find the cache deleting itself only partly and only eventually.
            // Measured before this line existed: `remove_dir_all` needed **63
            // attempts over 2.68 s** on a cache the producer was serving from,
            // failing throughout with `ERROR_SHARING_VIOLATION`.
            //
            // It is also the correct answer for the ordinary cases that reach
            // here — an edit moved the identity, or the range simply is not
            // cached — because in every one of them the open session is decoding
            // a file that will not be served again.
            if self.cur.is_some() || self.next.is_some() {
                self.retire();
            }
            return None;
        };

        if self.failed_seg == Some(seg_index) {
            return None;
        }

        // D-07's identity, read backwards: frame `i` of segment `k` IS program
        // time `k*SEG_US + i*step`, so the frame this tick wants is the one
        // NEAREST `prod_pos - seg_start`. Nearest, not floor: the grid is
        // anchored in TIME and the producer's own tick grid is anchored at
        // whatever position playback started from, so the two are generally
        // offset by up to one step. Rounding keeps the served content within
        // half a frame of the truth instead of up to a whole one, and it stays
        // strictly monotone (+1 per tick) either way.
        let want = (prod_pos - seg_start + step / 2).div_euclid(step);
        if want < 0 {
            return None;
        }

        // Bring the right session into `cur`: promote the prewarm when it fits,
        // otherwise open one seeked to the frame this tick needs.
        if self.cur.as_ref().map(|s| s.seg_index) != Some(seg_index) {
            let promoted = match self.next.take() {
                Some(p) if p.seg_index == seg_index && p.next_index <= want => Some(p),
                // Anything else is a mispredicted prewarm: dropping it reaps its
                // child, exactly as `Lookahead`'s husk drop does.
                _ => None,
            };
            self.cur = match promoted {
                Some(p) => Some(p),
                None => self.open_session(&hit, seg_index, want, step),
            };
            self.prewarmed_for = None;
            if self.cur.is_none() {
                self.failed_seg = Some(seg_index);
                RENDER_CACHE_SERVE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }

        // A rewind inside the segment, or a jump too far ahead to fast-forward
        // through, is answered by REOPENING at the demanded frame rather than by
        // pulling frames nobody will look at (see `MAX_SEG_CATCHUP`).
        let need_reopen = match self.cur.as_ref() {
            Some(s) => want < s.next_index || want - s.next_index > MAX_SEG_CATCHUP,
            None => true,
        };
        if need_reopen {
            self.cur = self.open_session(&hit, seg_index, want, step);
            if self.cur.is_none() {
                self.failed_seg = Some(seg_index);
                RENDER_CACHE_SERVE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }

        // The pull. Normally exactly one; at most `MAX_SEG_CATCHUP + 1`.
        let mut frame: Option<engine::Frame> = None;
        let promised;
        {
            let cur = self.cur.as_mut()?;
            promised = (cur.w, cur.h);
            while cur.next_index <= want {
                match pull_one(&cur.session, abort) {
                    Some(f) => {
                        cur.next_index += 1;
                        frame = Some(f);
                    }
                    None => {
                        frame = None;
                        break;
                    }
                }
            }
        }
        let Some(frame) = frame else {
            // Ran out of frames, timed out, or the producer was told to stop.
            // Total, silent, per-segment: this ONE tick composites live and the
            // session is let go.
            self.cur = None;
            RENDER_CACHE_SERVE_FALLBACKS.fetch_add(1, Ordering::Relaxed);

            // 59-REVIEW **WR-07** — the TAIL and the BODY are not the same event.
            //
            // A segment whose TAIL is short is still perfectly good for its body,
            // and D-9's overshoot is bounded at ONE index past the last frame the
            // writer can have written, so that case must stay unlatched: the next
            // segment must not inherit a verdict about this one.
            //
            // A payload that stops producing EARLIER than that is different in
            // kind — a frame the encoder dropped, a decode error mid-file, a
            // truncation that still matches `payload_bytes` because it was
            // committed that way. `self.cur` is now `None`, so the next tick
            // re-enters the session bring-up branch and calls `open_session`
            // again: one `ffmpeg` child spawn, plus the synchronous kill + wait +
            // channel drain + thread join `StreamingDecodeSession::Drop` performs,
            // for EVERY remaining tick of the segment — 30 of each per second at
            // 30 fps, all on the playback producer thread. That is exactly the
            // storm [`RENDER_CACHE_SEG_OPENS`]'s doc claims the latch prevents,
            // and the latch only ever covered `open_session` answering `None`.
            //
            // An ABORT is excluded deliberately: `pull_one` also answers `None`
            // when the producer is told to stop or flush, and a pause or a
            // seek-flush arriving mid-body must not disable a good segment. The
            // check is made here rather than inside `pull_one` so the ordinary
            // exits keep paying nothing for it.
            let last_written = (seg_end - seg_start - 1).div_euclid(step);
            if want < last_written && !abort() {
                self.failed_seg = Some(seg_index);
            }
            return None;
        };

        // D-21's wrong-geometry arm, checked against the geometry THIS SESSION
        // was opened on. `read_fresh_segment` already refused a meta whose
        // canvas disagrees with the caller's, so this can only fire when the
        // PAYLOAD disagrees with its own meta — precisely the case a meta-only
        // check cannot see, and precisely the case that would otherwise reach
        // `composite_mixed_layers_to_target` as a `BadOutputSize`.
        if (frame.width, frame.height) != promised {
            self.cur = None;
            self.failed_seg = Some(seg_index);
            RENDER_CACHE_SERVE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // ---- A2: the prewarm. -------------------------------------------
        // 59-CEILING-VERDICT § 4: ~180 ms of cold segment entry, which un-hidden
        // at every SEG_US boundary would be WORSE than the 1013 ms ceiling this
        // phase exists to remove, measured against the 66.5 ms bar. So the next
        // segment's probe and session open happen while the current one is still
        // serving — the same anticipatory shape, and the same one-per-boundary
        // latch, as the producer's existing `Lookahead`.
        //
        // Plan 59-13: this branch is ALSO where the end of the cached run is
        // discovered, for free. It already asks "is segment k+1 cached?" once
        // per boundary, one `LOOKAHEAD_US` ahead of it — so the arm where that
        // probe MISSES is, by definition, a cache→LIVE boundary, and the arm
        // where it hits is a cache→cache one that the segment prewarm above
        // already owns. `deferred-items § D-4` option (b) needs exactly that
        // one bit, at exactly that moment, and nothing else.
        if self.next.is_none()
            && self.prewarmed_for != Some(seg_index + 1)
            && prod_pos.saturating_add(crate::LOOKAHEAD_US) >= seg_end
        {
            self.prewarmed_for = Some(seg_index + 1);
            match self.lookup.probe(host, seg_end, edit_gen, canvas) {
                Some(next_hit) => {
                    // cache→cache. The segment prewarm owns this boundary; the
                    // live pool must NOT be warmed toward it, or every interior
                    // boundary of a cached run would spawn a pool the cached
                    // ticks then leave un-adopted (children spawned, reaped,
                    // and nothing served differently).
                    self.exit_at = None;
                    self.next = self.open_session(&next_hit, seg_index + 1, 0, step);
                    if self.next.is_some() {
                        RENDER_CACHE_SEG_PREWARMS.fetch_add(1, Ordering::Relaxed);
                    }
                }
                None => {
                    // cache→LIVE: the run ends at `seg_end` and the producer is
                    // about to need a warm layer pool there.
                    //
                    // Recorded on the probe's answer rather than on whether a
                    // session opened, because the QUESTION here is "is the next
                    // range cached", not "did our decode of it succeed". A
                    // segment that is cached but will not open is a D-21
                    // fallback at the boundary, not a cache→live exit.
                    self.exit_at = Some(seg_end);
                }
            }
        }

        Some(frame)
    }

    /// Open a SOFTWARE streaming decode of `hit`'s payload, seeked to the
    /// segment's frame `first_index`.
    ///
    /// Seeked rather than opened-at-zero-and-fast-forwarded because the payload
    /// is all-intra by construction (D-05), so `-ss` lands on the exact frame at
    /// the cheapest possible cost — which is what makes entering a cached
    /// segment mid-way (a seek) as cheap as entering it at its head.
    fn open_session(
        &self,
        hit: &rendercache::cache::SegmentHit,
        seg_index: i64,
        first_index: i64,
        step: i64,
    ) -> Option<SegSession> {
        let file_us = first_index.max(0).saturating_mul(step);
        RENDER_CACHE_SEG_OPENS.fetch_add(1, Ordering::Relaxed);
        // Rotation 0: a segment is a render of the PROJECT canvas, which carries
        // no source-side rotation metadata by construction.
        let session = match engine::StreamingDecodeSession::start(&hit.path, file_us, 0) {
            Ok(s) => s,
            // Silent (D-21). A cache that cannot be opened is a cache that is
            // not used; nothing about playback changes, and the counter above
            // plus `RENDER_CACHE_SERVE_FALLBACKS` are where it is visible.
            Err(_) => return None,
        };
        Some(SegSession {
            seg_index,
            session,
            next_index: first_index.max(0),
            w: hit.w,
            h: hit.h,
        })
    }
}

/// The store's own monotonic mutation counter, as this module's edit generation.
///
/// # Why `Store::seq()` and not `PreviewEditSeq.seq`
///
/// D-16 wants a cheap counter whose movement says *"look again"*. The producer
/// thread has no handle on `PreviewEditSeq` — `present_loop` owns it and
/// `spawn_producer` has never taken it — and threading one through would change
/// three public spawn signatures and every test route that calls them, to buy a
/// counter that is strictly WEAKER than the one already reachable here.
///
/// `rudis_core::Store::seq` is bumped inside `dispatch`/`undo`/`redo`
/// themselves, on EVERY state-changing mutation, and it is read here under the
/// same lock the key-material walk uses. So it cannot lag the material, it
/// cannot be missed by a listener that was never registered, and it moves for
/// strictly MORE edits than the preview-relevant filter admits — which is the
/// safe direction for a cache: an extra recompute costs microseconds, a missed
/// one costs a wrong frame. 59-05's own carry-forward concern ("if it ever
/// passes a constant, every test still passes and the cache serves stale
/// frames") is closed by construction rather than by discipline.
///
/// `pub(crate)` since plan 59-07: the WRITER
/// ([`crate::render_cache_writer::render_segment`]) re-reads this between
/// frames to decide whether the segment it is rendering is still a render of
/// the arrangement it was named after (D-18's write half). Reading it through
/// the same function means writer and reader can never disagree about what an
/// "edit" is — a second answer to that question is how a cache commits a
/// segment one half of it considers current and the other half does not.
pub(crate) fn current_edit_gen(host: &dyn PreviewHost) -> u64 {
    host.store().map(|guard| guard.seq()).unwrap_or(0)
}

/// Pull ONE frame, polling `abort` between bounded waits.
///
/// `None` means "no frame, composite live" for all three of its reasons: the
/// producer was told to stop or flush, the segment ran out of frames (the
/// channel closed), or the decode stalled past [`SEG_PULL_BUDGET`].
///
/// The end-of-stream case is distinguished from a timeout by ELAPSED time, not
/// by an API that does not offer it: `StreamingDecodeSession::try_next_frame`
/// collapses `Timeout` and `Disconnected` into one `None`, and a closed channel
/// returns instantly. Without that check a finished segment would spin this loop
/// for the whole budget at the end of every playthrough.
fn pull_one(
    session: &engine::StreamingDecodeSession,
    abort: &dyn Fn() -> bool,
) -> Option<engine::Frame> {
    let deadline = Instant::now() + SEG_PULL_BUDGET;
    loop {
        if abort() {
            return None;
        }
        let waited = Instant::now();
        match session.try_next_frame(SEG_PULL_SLICE) {
            Some(f) => return Some(f),
            None => {
                if waited.elapsed() < SEG_PULL_SLICE / 2 {
                    return None; // channel closed: end of segment, or a dead child
                }
                if Instant::now() >= deadline {
                    return None;
                }
            }
        }
    }
}
