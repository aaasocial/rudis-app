//! Phase 59's DELIVERY half (plan 59-08): the render-cache SCHEDULER — the
//! third and thinnest layer of 59-CONTEXT **D-39**'s split.
//!
//! ```text
//! crates/rendercache      identity, file format, atomic commit, fail-closed read, LRU
//! crates/preview          the lookup (D-19 reader) and render_segment (the writer)
//! app-core/render_cache_job   <- YOU ARE HERE: only WHEN a render happens
//! ```
//!
//! This module owns admission, cancellation, the triggers and the poll-only
//! status. It owns **nothing** about compositing: every pixel decision lives one
//! crate down in [`preview::render_cache_writer`], which this file only ever
//! calls. That is why the plan's own verification greps
//! `rendercache` across `crates/app-core/src` and expects hits in exactly this
//! file — the cache crate's name reaches app-core here and nowhere else.
//!
//! Its shape is [`crate::proxy_job`]'s, deliberately and almost line for line:
//! a bounded background job, a registry of rows carrying a cancel latch, an
//! epoch on every row, and a pure poll-only read behind the C ABI that cannot
//! start work. Read that module before changing this one.
//!
//! # The one genuinely NEW obligation: shared encoder admission (D-23)
//!
//! A proxy transcode and a segment render drive the **same silicon** — `h264_mf`
//! on the same GPU that already runs the preview path's hardware DECODE fleet,
//! which Phase 57 bounded at `MAX_HW_SESSIONS = 3`. Two independent
//! `Semaphore(1)`s would be two encoders, and 58-CONTEXT D-10's priority order
//! ("playback is the privileged consumer") would quietly stop holding.
//!
//! So admission here is not a sibling bound, it is
//! [`crate::proxy_job::try_take_permit`] — the SAME process-wide static the
//! proxy worker takes its permit from. This module constructs no permit pool of
//! its own, and `render_cache_job_owns_no_permit_pool_of_its_own` scans this
//! file's source to keep it that way, because "we agreed to share" is a
//! convention and a shared static is a fact.
//!
//! The permit is taken with the NON-blocking `try_` form and the job **defers**
//! when it is unavailable, rather than queueing behind the proxy worker: a
//! background render that is merely late costs nothing, and a queue of them
//! would be a second encoder waiting for its turn — exactly the thing D-23
//! forbids. The next poll picks the segment up.
//!
//! # Why a thread and not a `tokio` task
//!
//! [`crate::proxy_job::spawn_generation`] detaches a `tokio` task because it is
//! called from `import_one_path`, which is `async` and already inside the host's
//! runtime. This module's trigger is [`crate::transport::run_transport`] — a
//! synchronous funnel with no runtime guarantee at all — and the work itself is
//! ~1.3 s of fully blocking decode/composite/encode (59-07 measured it). A named
//! `std::thread` is what `spawn_blocking` would have produced anyway, minus a
//! dependency on a runtime that may not be entered. `tokio::sync::Semaphore`'s
//! `try_acquire` and its permit's `Drop` are both synchronous, so the SHARED
//! admission works identically either way.
//!
//! # D-24 — rendering yields to playback
//!
//! Two mechanisms, not one:
//!
//! * [`PLAYBACK_GUARD_SEGMENTS`] — while the timeline is PLAYING, segments
//!   within that many segments of the playhead are skipped. Rendering the
//!   section the user is watching is self-defeating: it competes for the encoder
//!   and the decode pool with the frames it exists to protect.
//! * [`request_cancel_all`] — raised from project close, media removal and
//!   process shutdown. That caller list is `proxy_job::cancel_all`'s, verbatim;
//!   this function stands beside it at every one of them.
//!
//! # D-26 — this is ENGINE state, and undo cannot reach it
//!
//! Nothing here is `.rud` document state. The public surface takes and returns
//! no domain-mutation type: no `Command`, no `Patch`, no `PatchKind`. The
//! registry lives in memory and the segments live on disk under
//! `AppCtx::app_cache_dir()`, and an undo can no more resurrect a cache entry
//! than it can un-delete a thumbnail.
//!
//! `render_cache_job_never_names_a_mutation_type` asserts that as a source scan
//! rather than as a promise. The two type names ARE spelled out in this
//! paragraph, which is deliberate and is the reason that scan blanks comments
//! before it looks: the preview crate's dynamic-resolution module recorded the
//! opposite footgun (a doc comment tripping the rule it describes) and it has
//! since bitten five plans in two phases.
//!
//! **And this very paragraph was the sixth**, which is why it now describes that
//! module instead of naming it — including in the sentence below, where naming
//! the guarding TEST FILE would trip the guard just as surely, since its own
//! name carries the forbidden substring.
//!
//! The guard is Phase 57's `d12_app_core_shipped_code_cannot_name_a_resolution
//! _level`, in the preview crate's dynamic-resolution pin file. It scans the
//! SHIPPED half of every `crates/app-core/src` file for those names WITHOUT
//! blanking comments — deliberately, because `crates/app-core` hosts the export
//! path and D-12 says export can never reach playback degradation, so here prose
//! is not a false positive. **That pin went red the moment this file landed and
//! stayed red until 59-10 ran `cargo test -p preview`**; 59-08's own gates were
//! `-p app-core` and `-p ffi`, neither of which can see it. The reusable lesson:
//! a rule enforced by a scan living in ANOTHER crate needs that crate's suite in
//! the gate, or the rule is unguarded in practice however well it is written.
//!
//! # D-38 — the bounded re-scan
//!
//! Correctness is already REACTIVE: a hash mismatch is a miss whenever a segment
//! is next touched (D-18), so no stale frame can be presented however late the
//! staleness is noticed. [`on_structural_edit`] is HYGIENE — it reclaims
//! orphaned bytes around the playhead so the byte budget is not held by segments
//! of an arrangement that no longer exists. It re-scans **only**
//! [`RESCAN_WINDOW_SEGMENTS`] either side of the playhead (plus the edit's own
//! extent, when a caller can supply one — see [`on_structural_edit_over`]),
//! never every cached segment and never nothing. Outside the window, D-25's LRU
//! reclaims, which is exactly what a byte-budget LRU is for.
//!
//! **Its production trigger is [`on_structural_edit_at_playhead`]**, called from
//! [`crate::dispatch`] (59-REVIEW WR-05). Before that it had no shipped caller
//! at all: the paragraph above described a reclaim that never happened,
//! [`RENDER_CACHE_RESCAN_DELETED`] was a permanently-zero observable, and
//! [`RESCAN_WINDOW_SEGMENTS`] was dead configuration. [`on_structural_edit_over`]
//! — the half that takes the edit's own extent — still has none; converting a
//! patch into an extent needs a caller that knows which clip moved, and no host
//! computes one today (recorded in `deferred-items.md`).
//!
//! # Where the tests live
//!
//! In this file, not in `tests/`. 59-VALIDATION rows 10 and 27 were drafted as
//! `cargo test -p app-core --test render_cache_rescan_window` /
//! `--test render_cache_job`, and 59-08's plan corrects both to in-module
//! app-core tests: the registry, the row epochs and the shared permit are all
//! `pub(crate)` or private, so an integration test in a separate crate could
//! only assert on them by widening exactly the surface this module exists to
//! keep narrow. `proxy_job`'s own gates for the identical claims are in-module
//! for the identical reason. The reach is
//! `cargo test -p app-core render_cache_job`.
//!
//! # Threat model
//!
//! * **T-59-08-01 (encoder/GPU DoS)** — the shared permit above, proven both
//!   directions by `render_cache_job_shared_admission`.
//! * **T-59-08-02 (an edit storm re-spinning aborted renders)** —
//!   [`MAX_SEGMENT_ATTEMPTS`], plus 59-07's PERMANENT/TRANSIENT refusal split:
//!   `NotCacheable` is never re-enqueued at all.
//! * **T-59-08-03 (a leaked live row making a segment permanently
//!   unrenderable)** — [`RowGuard`], inheriting 58-REVIEW WR-04's discipline and
//!   its epoch check.
//! * **T-59-08-04 (cache state leaking into undoable project state)** — D-26
//!   above, source-scanned.
//! * **T-59-08-05 (an unbounded dir walk on every status poll)** —
//!   [`MAX_STATUS_DIR_ENTRIES`], name-only counting, no file opened, nothing
//!   spawned, nothing decoded.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use crate::AppCtx;
use preview::render_cache_lookup::SegmentLookup;
use preview::render_cache_writer::RenderOutcome;
use preview::PreviewHost;

// ---------------------------------------------------------------------------
// The calibrated constants (D-10 records every one of these in 59-10)
// ---------------------------------------------------------------------------

/// How many segment renders may be in flight at once, process-wide.
///
/// **One** — the same value and, unlike
/// [`crate::filmstrip_job::MAX_CONCURRENT_FILMSTRIP_JOBS`], the same *reasoning*
/// as [`crate::proxy_job::MAX_CONCURRENT_PROXY_JOBS`]: a segment render occupies
/// a hardware encoder session on the silicon playback is already decoding on.
///
/// It is enforced by the registry (at most one live row at a time) AND by the
/// shared permit, which is a strictly stronger bound because it counts the proxy
/// worker too. Stated as a named constant anyway, with a test on it, because the
/// bound must be legible where the policy is written and not only where it
/// happens to be implemented.
pub const MAX_CONCURRENT_RENDER_CACHE_JOBS: usize = 1;

/// Hard cap on remembered segment rows.
///
/// [`crate::proxy_job::MAX_TRACKED_PROXY_JOBS`]'s twin, keyed by segment index
/// rather than by path. Forgetting a finished row is lossless — the disk is the
/// truth, and [`run_get_render_cache_status`]'s census reads it — with one
/// exception the sweep respects: a PERMANENTLY refused segment
/// ([`ROW_REFUSED`]) must be remembered, or the poller would re-enqueue a
/// segment 59-07 has already said can never be cached.
pub const MAX_TRACKED_RENDER_CACHE_JOBS: usize = 512;

/// **D-38's window.** How many segments either side of the playhead
/// [`on_structural_edit`] re-checks after a structural edit.
///
/// Eight segments is ±16 s of program at `SEG_US = 2 s` — about ten times the
/// 1.7 s a user can scrub through in one gesture, and small enough that the
/// whole sweep is one bounded directory listing plus at most 17 memoized hash
/// lookups (59-05 measured a one-clip walk at 368 µs, so a 6-layer window is
/// ~37 ms of pure hygiene, off the playback thread).
///
/// **It is not load-bearing for correctness and must never become so.** D-18
/// makes a hash mismatch a miss whenever a segment is next touched, so a segment
/// outside this window is refused by the reader exactly as reliably as one
/// inside it — this constant only decides how soon its BYTES come back.
/// Recorded with the rest in 59-10's `59-CACHE-CALIBRATION.md`.
pub const RESCAN_WINDOW_SEGMENTS: i64 = 8;

/// **D-24's guard.** While the timeline is playing, segments within this many
/// segments of the playhead are not rendered.
///
/// Two segments is ±4 s of program: one segment ahead is where the producer's
/// lookahead already is, and the second covers a boundary crossing that happens
/// while a render is in flight (59-07 measured a whole segment at 1 270 ms, i.e.
/// most of one `SEG_US`). Rendering the range the user is watching would put a
/// second decode pool and an encoder child against the very frames the cache
/// exists to protect — "a cache that costs live frames is a regression, not a
/// feature" (D-23).
///
/// Deliberately small rather than "never render while playing": the heavy
/// sections a user replays are usually not the one under the playhead right now,
/// and refusing every segment during playback would mean the cache only ever
/// warms while the app is idle — which is precisely when nobody benefits from
/// it.
pub const PLAYBACK_GUARD_SEGMENTS: i64 = 2;

/// How many times one segment may be re-enqueued after a TRANSIENT refusal.
///
/// 59-07 split the writer's refusals into permanent and transient on purpose:
/// `NotCacheable` is a property of the arrangement (never retried — see
/// [`ROW_REFUSED`]), while `Incomplete`, `Cancelled`, `StaleAborted` and
/// `Failed` are bad luck and are worth another attempt. Without a bound, an edit
/// storm — or one permanently undecodable clip that still hashes — would spin
/// this worker forever (T-59-08-02, and 59-07's `deferred-items` D-8 addressed
/// to this plan by name).
///
/// Three, matching the retry budgets elsewhere in this codebase, and cleared for
/// a segment whenever its row is evicted or the process restarts.
///
/// **It counts only outcomes the RENDER is responsible for** (59-REVIEW
/// **WR-03**): `Incomplete` and `Failed`. `Cancelled` and `StaleAborted` are
/// raised by the world moving, not by the render going wrong, and they are
/// budgeted separately by [`MAX_EXTERNAL_ABORTS`]. `Committed` clears it
/// outright — a segment that just rendered successfully has no bad luck left to
/// remember, and without that a segment re-invalidated by three successful
/// edits would exhaust a budget it never actually spent.
pub const MAX_SEGMENT_ATTEMPTS: u32 = 3;

/// How many times one segment may be abandoned for an EXTERNAL reason before
/// the scheduler stops re-enqueueing it.
///
/// The second half of 59-REVIEW **WR-03**. The writer raises `StaleAborted` on
/// every store mutation during a render, and a render is ~1.3 s; `Cancelled` is
/// raised by `request_cancel_all`, whose callers are project open, project new,
/// media removal and shutdown. All of those are USER ACTIONS. Charging them to
/// [`MAX_SEGMENT_ATTEMPTS`] meant three edits during a background render
/// permanently disabled caching for that segment — the exact opposite of the
/// intent, since the sections a user is actively working on are the ones the
/// cache should keep retrying hardest, and it turned T-59-08-02's anti-spin
/// guard into a starvation guard.
///
/// A separate, much larger budget rather than an unbounded refund, because
/// T-59-08-02 is real: an edit storm against a segment that can never finish
/// must still terminate. 32 is ~40 s of continuous 1.3 s renders all aborted
/// mid-flight — far past any plausible burst of user actions, and still finite.
pub const MAX_EXTERNAL_ABORTS: u32 = 32;

/// How many heavy candidates one poll considers before giving up for this tick.
///
/// The detector is already bounded at `MAX_TRACKED_SEGMENTS = 4096`; this is the
/// scheduler's own bound on how much of that it will walk on a synchronous
/// trigger that runs on the transport path.
///
/// **It is a ROTATING window, not a prefix** (59-REVIEW **WR-04**). The detector
/// hands back its candidates sorted, and heat marks are sticky by design, so
/// truncating that list gave a stable lowest-64 that successive polls re-walked
/// forever — candidates past it were never scheduled at all, and 64 segments is
/// 128 s of program against a detector built to hold ~2.3 hours of marks. The
/// window now starts just past [`LAST_CONSIDERED`] and wraps, so the per-poll
/// cost is unchanged and the coverage is the whole set.
pub const HEAVY_SEGMENT_POLL_LIMIT: usize = 64;

/// T-59-08-05: the status census stops after this many directory entries.
///
/// A poll-only getter on a 100 ms cold-path poll must have a cost ceiling that
/// does not depend on how big the user's cache happens to be. At the 8 GiB
/// budget and 59-07's measured 432 KB per 2 s segment, a full cache is ~19 000
/// entries; this cap reads names only (no file is opened) and early-outs, so the
/// worst case is one bounded `read_dir` walk.
const MAX_STATUS_DIR_ENTRIES: usize = 8_192;

// ---------------------------------------------------------------------------
// Wire states (D-30) and internal row states
// ---------------------------------------------------------------------------

/// A segment render is in flight right now.
const STATE_RENDERING: &str = "rendering";
/// This host has a render cache directory, and nothing is rendering.
const STATE_IDLE: &str = "idle";
/// This host has no render cache at all — no directory has ever been created
/// for it, which is the compatibility floor a process that never plays anything
/// stays on.
const STATE_NONE: &str = "none";

/// Registered, waiting for its thread to start. Momentary.
const ROW_QUEUED: &str = "queued";
/// Holding the shared permit, rendering.
const ROW_RUNNING: &str = "running";
/// Committed. The file is on disk and the reader can serve it.
const ROW_DONE: &str = "done";
/// A TRANSIENT refusal. Eligible for re-enqueue until [`MAX_SEGMENT_ATTEMPTS`].
const ROW_RETRY: &str = "retry";
/// A PERMANENT refusal — 59-07's `NotCacheable`. **Never re-enqueued.** Getting
/// this wrong livelocks the worker on an arrangement that can never be cached.
const ROW_REFUSED: &str = "refused";

// ---------------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------------

/// Segment renders this process actually started.
pub static RENDER_CACHE_JOBS_SPAWNED: AtomicU64 = AtomicU64::new(0);

/// **Phase 71 (TRUST-01): the render cache is paused for a device-lost recovery.**
///
/// While set, [`pump_tick`] and [`poll_and_spawn`] return before anything can
/// spawn. A bake builds (or reuses) the background compositor, which is a
/// reference to the same per-adapter D3D12 device the preview lost; one bake
/// starting inside the recovery window keeps the hardware adapter hidden and
/// sends the preview's recreate to WARP (71-05, measured).
///
/// Set and cleared only through [`set_paused`]. A FAILED recovery leaves it set
/// for the session on purpose: a bake on WARP would re-hide the adapter and is a
/// silent downgrade.
pub static RENDER_CACHE_PAUSED: AtomicBool = AtomicBool::new(false);

/// Pause or resume the render cache (see [`RENDER_CACHE_PAUSED`]). Pausing does
/// not cancel what is already running; pair it with [`request_cancel_all`] and
/// wait on [`live_row_count`].
pub fn set_paused(paused: bool) {
    RENDER_CACHE_PAUSED.store(paused, Ordering::SeqCst);
}

/// **71-REVIEW WR-01: pause and cancel ATOMICALLY with respect to registration.**
///
/// Takes the registry lock, sets [`RENDER_CACHE_PAUSED`] and raises every row's
/// cancel latch before releasing it. `poll_core` re-checks the pause under the
/// same lock immediately before it inserts a new row, so once this returns no row
/// can exist whose latch was not raised, and no new row can be registered until
/// the pause is lifted. Device-lost recovery uses this instead of the separate
/// [`set_paused`] + [`request_cancel_all`] pair, which left a window for a poll
/// that had already passed its entry check to register a fresh, un-cancelled bake.
pub fn pause_and_cancel_all() {
    let jobs = jobs();
    RENDER_CACHE_PAUSED.store(true, Ordering::SeqCst);
    for row in jobs.values() {
        row.cancel.store(true, Ordering::Relaxed);
    }
}

/// Rows that are queued or running right now, i.e. renders that may still hold
/// a clone of the background compositor. Device-lost recovery polls this down
/// to zero after [`request_cancel_all`].
pub fn live_row_count() -> usize {
    jobs().values().filter(|row| row.is_live()).count()
}

/// Polls that found work but could not take the SHARED encoder permit — i.e.
/// times the proxy worker had the encoder and this one stood down (D-23).
///
/// The attribution handle for the admission rule: a bound nobody ever hits and a
/// bound that is never enforced look identical from the outside.
pub static RENDER_CACHE_JOBS_DEFERRED_ADMISSION: AtomicU64 = AtomicU64::new(0);

/// Candidate segments skipped because the user was playing within
/// [`PLAYBACK_GUARD_SEGMENTS`] of them (D-24).
pub static RENDER_CACHE_JOBS_DEFERRED_PLAYBACK: AtomicU64 = AtomicU64::new(0);

/// Cache files [`on_structural_edit`] deleted because their identity no longer
/// matched the arrangement (D-38's hygiene, as a number).
pub static RENDER_CACHE_RESCAN_DELETED: AtomicU64 = AtomicU64::new(0);

/// **Phase 61 (D-17).** Ticks the IDLE pump has taken in this process — the
/// "is the clock beating at all" observable.
///
/// Incremented at the TOP of `pump_tick`, BEFORE the kill-switch re-read and
/// before the host lookup, so a pump that is parked (no host registered) or
/// switched off is still visibly alive rather than indistinguishable from a
/// pump that never started. Relaxed and read as a DELTA around a span, like
/// every other counter in this file.
pub static RENDER_CACHE_PUMP_TICKS: AtomicU64 = AtomicU64::new(0);

/// **Phase 61 (D-17).** The subset of [`RENDER_CACHE_JOBS_SPAWNED`] the IDLE
/// pump started, as against the ones a transport command started.
///
/// The difference of the two is exactly the question the field session could
/// not answer — *was this segment warmed while the app sat idle, or only
/// because the user played it again?* Incremented at the SAME site as
/// [`RENDER_CACHE_JOBS_SPAWNED`], from the `from_pump` flag [`poll_core`]'s
/// caller passes, so attribution is exact even when a transport poll and a pump
/// tick race: the flag travels with the spawn, so neither can be credited to
/// the other.
pub static RENDER_CACHE_JOBS_SPAWNED_IDLE: AtomicU64 = AtomicU64::new(0);

/// **Phase 71 (TRUST-03).** Segments actually COMMITTED to the render cache in
/// this process.
///
/// Process-lifetime, never reset, never a rate. Bumped exactly once per
/// `RenderOutcome::Committed`, at the commit site in the render thread, and
/// NEVER on `Cancelled`, `StaleAborted`, `Incomplete` or `Failed` — so it counts
/// work that landed, not work that was attempted. It is the only honest
/// per-bake progress quantity this module has: `cached_segments` and
/// `heavy_segments` are censuses and were MEASURED to mislead as progress
/// (D-63-04-02). A consumer that wants "segments committed this session, for
/// this project" reads it at its first `rendering` observation and subtracts.
pub static RENDER_CACHE_SEGMENTS_COMMITTED: AtomicU64 = AtomicU64::new(0);

/// What a status poll looks like on the wire (D-30).
///
/// [`crate::proxy_job::ProxyStatusPayload`]'s discipline, one field wider: plain
/// scalars, a `state` that is a plain lowercase string rather than an integer or
/// a tagged union, and every value written out in prose because the field NAMES
/// and the state STRINGS are a wire contract with no compiler on the other side
/// of the ABI to catch a rename.
///
/// * `state` is exactly one of `"rendering"`, `"idle"`, `"none"`.
///   `"none"` means this host has no render-cache directory at all — nothing has
///   ever been rendered for it; `"idle"` means it has one and nothing is
///   rendering right now.
/// * `rendering_segment` is the segment index currently being rendered, or
///   **`-1`** when nothing is. `-1` rather than a nullable field so a C# DTO
///   deserializes it into a plain `long`.
/// * `cached_segments` counts DISTINCT segment indices with a committed meta on
///   disk. It is a census of what has been rendered, NOT a count of what would
///   be served right now: re-deriving each segment's identity would mean one
///   key-material walk per entry on a 100 ms poll, which is exactly the cost
///   T-59-08-05 exists to refuse. The reader answers "would this serve?" per
///   tick, fail-closed, and is the only honest place for that question.
/// * `heavy_segments` is the detector's current candidate count — the work the
///   scheduler still has to do.
/// * `idle_spawned` is how many renders the IDLE pump has started in this
///   process — see the field's own doc below.
///
/// # Adding a field is an ABI event, and it is pinned in two places
///
/// `serde` serializes these in DECLARATION order, so a new field goes LAST and
/// nowhere else. `crates/ffi/tests/render_cache_status.rs` pins the result
/// TWICE — a sorted key list and the raw envelope bytes — and this file's own
/// `..._status_payload_wire_shape` pins it a third time. All of them are in a
/// different crate or a different module from the struct, so `cargo test -p
/// app-core` alone cannot tell you the contract still holds: run
/// `cargo test -p ffi --test render_cache_status` in the SAME commit that
/// touches this struct (61-RESEARCH Pitfall 2).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RenderCacheStatusPayload {
    pub state: String,
    pub rendering_segment: i64,
    pub cached_segments: u64,
    pub heavy_segments: u64,
    /// Phase 61 (D-17): renders spawned by the idle pump, as against the
    /// play-triggered total — the attribution the 2026-08-27 field session
    /// could not make. One relaxed atomic load; the payload stays poll-only
    /// and poll-cheap (T-59-08-05 / 59-CONTEXT D-30 unchanged).
    ///
    /// A process-lifetime running total, never a rate and never a reset: a
    /// consumer that wants "did anything warm while I was away" reads it twice
    /// and subtracts, which is how every other counter in this file is read.
    /// It is deliberately NOT paired with an exported hit/miss rate — those
    /// statics stay test-only (D-19), and widening this ABI for them is named
    /// out of scope.
    pub idle_spawned: u64,
    /// Phase 71 (TRUST-03): [`RENDER_CACHE_SEGMENTS_COMMITTED`] — a
    /// process-lifetime count of segments actually committed, appended LAST.
    /// One relaxed atomic load, same poll-cost posture as `idle_spawned`.
    /// Difference it against a baseline for "this session"; never show
    /// `cached_segments`/`heavy_segments` as progress (D-63-04-02).
    pub committed_total: u64,
}

// ---------------------------------------------------------------------------
// The registry (58-REVIEW WR-04's discipline, inherited verbatim)
// ---------------------------------------------------------------------------

/// One segment's row: what is known about it, and the latch its render observes.
#[derive(Debug)]
struct JobEntry {
    /// Which spawn owns this row (58-REVIEW **WR-04**). A key alone is not an
    /// identity: a finished row can be REPLACED by a later spawn for the same
    /// segment, and the earlier thread must not then write to — or clear — its
    /// successor's row. Every write goes through this check.
    epoch: u64,
    state: &'static str,
    cancel: Arc<AtomicBool>,
    /// How many times this segment has been enqueued and the render then failed
    /// on its OWN account. Survives the row's terminal states so
    /// [`MAX_SEGMENT_ATTEMPTS`] is a budget rather than a per-attempt
    /// formality — but a render abandoned for an external reason refunds it, and
    /// a committed one clears it (59-REVIEW WR-03).
    attempts: u32,
    /// How many times this segment's render was abandoned because the WORLD
    /// moved — a store mutation mid-render, or a project switch / media removal
    /// / shutdown raising the latch. Budgeted by [`MAX_EXTERNAL_ABORTS`], which
    /// is deliberately much larger than [`MAX_SEGMENT_ATTEMPTS`]: these are user
    /// actions, not failures, and the only thing this bound exists to stop is an
    /// unterminating storm (T-59-08-02).
    external_aborts: u32,
    /// **59-REVIEW WR-01.** The material generation at which this segment was
    /// last CONFIRMED already-fresh on disk.
    ///
    /// A row carrying the current generation here is skipped by [`poll_and_spawn`]
    /// without hashing anything at all, which is what turns the steady state (
    /// every candidate already warm) from "one key-material walk per candidate
    /// per poll" into "one map probe per candidate per poll". It is a CACHE of a
    /// conclusion, never an authority: the generation is in the key, so it is
    /// unreadable the moment the material moves, and the reader re-derives the
    /// truth on every tick regardless.
    fresh_at: Option<MaterialGen>,
}

/// `(store edit generation, decode-answer generation)` — the pair a memoized
/// conclusion about a segment's identity is only valid within.
///
/// The same pair `preview::render_cache_lookup` keys its own memo on, and for
/// the same reason (59-REVIEW WR-02): half the material is store state and half
/// is filesystem state, and only one of the two has a mutation counter.
type MaterialGen = (u64, u64);

impl JobEntry {
    /// Queued and running are the two states a later poll must not duplicate and
    /// the eviction sweep must not forget.
    fn is_live(&self) -> bool {
        self.state == ROW_QUEUED || self.state == ROW_RUNNING
    }

    /// A permanent refusal is remembered forever, because forgetting it is how
    /// an uncacheable arrangement becomes an infinite retry.
    fn is_permanent(&self) -> bool {
        self.state == ROW_REFUSED
    }
}

/// Hands out per-row identities. Monotonic and process-global, like the registry
/// it labels.
static ROW_EPOCH: AtomicU64 = AtomicU64::new(0);

/// **59-REVIEW WR-04.** The last candidate index a poll looked at.
///
/// [`poll_and_spawn`] asks the detector for the [`HEAVY_SEGMENT_POLL_LIMIT`]
/// candidates starting just past this value, and stores where it stopped, so
/// successive polls advance through the whole candidate set and wrap. Without
/// it the window was a fixed prefix of a sorted list and everything past it was
/// unreachable for the life of the process.
///
/// Process-global for the registry's own reason: there is one scheduler, and the
/// value is advisory — a wrong one costs a poll that considers the same segments
/// twice, never a wrong render.
static LAST_CONSIDERED: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(i64::MIN);

/// Every segment render this process knows about, keyed by segment index.
static JOBS: LazyLock<Mutex<HashMap<i64, JobEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The registry guard. A poisoned registry is RECOVERED rather than propagated,
/// for [`crate::proxy_job`]'s reason: a status poll that panicked because some
/// unrelated render unwound would be a strictly worse failure than any cache
/// problem this module exists to solve.
fn jobs() -> MutexGuard<'static, HashMap<i64, JobEntry>> {
    JOBS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Overwrite one row's state, if it is still that spawn's row (WR-04's epoch).
fn set_state(seg: i64, epoch: u64, state: &'static str) {
    if let Some(row) = jobs().get_mut(&seg) {
        if row.epoch == epoch {
            row.state = state;
        }
    }
}

/// What a terminal [`RenderOutcome`] costs the segment's budgets
/// (59-REVIEW **WR-03**).
///
/// The three arms exist because the six outcomes are three different KINDS of
/// event, and collapsing them into one counter is what turned an anti-spin
/// guard into a starvation guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptSettlement {
    /// The render did its job. Nothing to remember.
    Cleared,
    /// The render was abandoned because the world moved, not because it went
    /// wrong. Refund the attempt; charge [`MAX_EXTERNAL_ABORTS`] instead.
    External,
    /// Bad luck, or a genuinely bad clip. This is what
    /// [`MAX_SEGMENT_ATTEMPTS`] is for.
    Charged,
}

/// Publish a terminal state AND settle the segment's budgets, if the row is
/// still that spawn's (WR-04's epoch).
///
/// One function rather than a `set_state` plus a separate adjustment, because
/// the two must observe the SAME epoch check: a stale thread that skipped the
/// state write but still moved a counter would corrupt its successor's budget.
fn set_state_and_settle(seg: i64, epoch: u64, state: &'static str, how: AttemptSettlement) {
    if let Some(row) = jobs().get_mut(&seg) {
        if row.epoch != epoch {
            return;
        }
        row.state = state;
        match how {
            AttemptSettlement::Cleared => {
                row.attempts = 0;
                row.external_aborts = 0;
            }
            AttemptSettlement::External => {
                // Refund the enqueue, and charge the external budget instead.
                row.attempts = row.attempts.saturating_sub(1);
                row.external_aborts = row.external_aborts.saturating_add(1);
            }
            AttemptSettlement::Charged => {}
        }
    }
}

/// **59-REVIEW WR-01.** Record that `seg` was found already-fresh on disk under
/// `gen`, so the next poll can skip it without re-deriving its identity.
///
/// Called only from [`poll_and_spawn`], and only after the SHIPPED read path
/// has said `Some` — this never guesses. A row is created for a segment that
/// has none, because "already warm" is exactly the state the scheduler most
/// needs to remember and the registry is where scheduler conclusions live.
///
/// A LIVE row is left alone (there can be none — the poll returns early while
/// any row is live — but a function that writes to the registry should not
/// depend on its caller's control flow for that), and so is a permanent
/// refusal.
fn mark_fresh_on_disk(seg: i64, gen: MaterialGen) {
    let mut jobs = jobs();
    match jobs.get_mut(&seg) {
        Some(row) if row.is_live() || row.is_permanent() => {}
        Some(row) => {
            row.state = ROW_DONE;
            row.fresh_at = Some(gen);
        }
        None => {
            evict_finished_if_full(&mut jobs);
            let epoch = ROW_EPOCH.fetch_add(1, Ordering::Relaxed);
            jobs.insert(
                seg,
                JobEntry {
                    epoch,
                    state: ROW_DONE,
                    cancel: Arc::new(AtomicBool::new(false)),
                    attempts: 0,
                    external_aborts: 0,
                    fresh_at: Some(gen),
                },
            );
        }
    }
}

/// Owns one row's LIFETIME, so a live row cannot outlive the thread that made it
/// (58-REVIEW **WR-04**, inherited whole).
///
/// The failure it prevents is the same one, one subject over: [`poll_and_spawn`]
/// refuses to start a second render while any row is live, so a `queued` or
/// `running` row with no thread behind it would stop the render cache for the
/// whole PROCESS, not merely for its own segment. There is no timeout and no
/// reaper, so nothing would correct it.
///
/// It differs from `proxy_job::RowGuard` in exactly one way, and the difference
/// is deliberate: this guard **demotes** a stranded live row to [`ROW_RETRY`]
/// instead of removing it, so the segment's [`JobEntry::attempts`] budget
/// survives a panicking render. Removing the row would reset the budget, and a
/// render that panics reproducibly would then be retried forever — which is
/// T-59-08-02 arriving through the door WR-04 opened.
///
/// Constructed BEFORE the thread and moved in, for WR-04's own reason: a guard
/// declared as the closure's first statement does not exist until the closure
/// runs, and a thread that fails to spawn never runs at all.
struct RowGuard {
    seg: i64,
    epoch: u64,
}

impl Drop for RowGuard {
    fn drop(&mut self) {
        let mut jobs = jobs();
        if let Some(row) = jobs.get_mut(&self.seg) {
            if row.epoch == self.epoch && row.is_live() {
                row.state = ROW_RETRY;
            }
        }
    }
}

/// Drop finished rows once the registry is at its cap.
///
/// Live rows are never touched (they own a latch a cancel may still need to
/// reach) and neither are permanent refusals (forgetting one re-opens
/// T-59-08-02). If every row is one of those the map is allowed to exceed the
/// cap rather than lose either guarantee.
fn evict_finished_if_full(jobs: &mut HashMap<i64, JobEntry>) {
    if jobs.len() < MAX_TRACKED_RENDER_CACHE_JOBS {
        return;
    }
    jobs.retain(|_, row| row.is_live() || row.is_permanent());
}

// ---------------------------------------------------------------------------
// The host port
// ---------------------------------------------------------------------------

/// The `PreviewHost` a background render composites through.
///
/// A render needs the shell-SERVICES port (`store`, `rasterize_text`), and
/// `AppCtx` does not provide one — deliberately, since 46-CONTEXT D-08: a host
/// that owns a preview surface implements `PreviewHost`, and one that does not
/// should not be forced to. So the host REGISTERS its own, once, and this module
/// holds it for the life of the process.
///
/// Process-global rather than per-ctx, matching every other seam in this area
/// (`configure_proxy_cache_dir`, `configure_render_cache_dir`, `JOBS`, the
/// shared permit): a second ctx in one process replaces the first's host, which
/// is the same trade every one of those already makes and is invisible in
/// production, where there is exactly one.
static HOST: LazyLock<RwLock<Option<Arc<dyn PreviewHost>>>> = LazyLock::new(|| RwLock::new(None));

/// Register the host background renders composite through.
///
/// Idempotent; a later call overwrites. Best-effort and infallible by design —
/// a host that never registers one simply never renders segments, which is the
/// compatibility floor the reader already has for an unconfigured directory.
pub fn set_render_host(host: Arc<dyn PreviewHost>) {
    match HOST.write() {
        Ok(mut slot) => *slot = Some(host),
        Err(poisoned) => *poisoned.into_inner() = Some(host),
    }
}

/// Forget the registered host (a ctx going away).
///
/// Raises every cancel latch first: a render holding an `Arc` to a host whose
/// store is about to drop should stop now, not at its next tick.
pub fn clear_render_host() {
    request_cancel_all();
    match HOST.write() {
        Ok(mut slot) => *slot = None,
        Err(poisoned) => *poisoned.into_inner() = None,
    }
}

fn render_host() -> Option<Arc<dyn PreviewHost>> {
    match HOST.read() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

// ---------------------------------------------------------------------------
// The cache directory (59-06/59-07's ONE missing production wiring point)
// ---------------------------------------------------------------------------

/// This host's render-cache directory, WITHOUT creating it.
///
/// `AppCtx::app_cache_dir()` joined with the cache crate's own directory name —
/// the same root as `filmstrips/`, `posters/` and `proxies/` (D-25), and
/// [`crate::proxy_job::proxy_dir`]'s exact shape for its exact reason: the read
/// path runs on a poll and must have no side effects, so the join lives here,
/// once, and the `create_dir_all` lives on the write path (inside
/// `rendercache::SegmentEncodeSession::begin`).
pub fn render_cache_dir<C: AppCtx>(ctx: &C) -> Result<PathBuf, String> {
    Ok(ctx.app_cache_dir()?.join(rendercache::RENDER_CACHE_DIR_NAME))
}

// ---------------------------------------------------------------------------
// The runtime kill switch (59-24)
// ---------------------------------------------------------------------------

/// The runtime kill-switch environment variable. Checked at the top of
/// [`poll_and_spawn`]: when set (to anything), the render cache fails closed —
/// no directory is configured, so no lookup serves and no writer bakes, and no
/// render job is spawned. Unset means ENABLED, which is the default and always
/// has been.
///
/// `engine::hwdecode::KILL_SWITCH_ENV`'s idiom, mirrored deliberately: a `pub
/// const` naming the variable once, read with `var_os` PER CALL so a flip takes
/// effect in the field with no rebuild, and presence — not value — is the
/// signal.
///
/// # Why it exists now
///
/// The cache is on by default at [`crate::transport::run_transport`]'s
/// unconditional call site, and 59-22 measured `CACHE-02`'s exit bar still
/// missed by 68.09 ms. Shipping an on-by-default feature that sits above a
/// missed quality bar with no way off is the bad trade; the switch is the way
/// off. It also hands every future bench a first-class DISABLED arm, exactly as
/// `RUDIS_DISABLE_HWDECODE` and `RUDIS_DISABLE_BOUNDARY_PREWARM` already do for
/// this phase's own instruments.
///
/// The switch only ever DISABLES. There is no value of it that turns anything
/// on, and its absence must never become an opt-out of the default.
pub const KILL_SWITCH_ENV: &str = "RUDIS_DISABLE_RENDER_CACHE";

/// The kill-switch check every poll starts with — shared, verbatim, by the
/// transport path and by Phase 61's idle pump.
///
/// Re-reads [`KILL_SWITCH_ENV`] with `var_os` on EVERY call — presence, never
/// value — so a flip takes effect on the very next poll with no rebuild
/// (`engine::hwdecode`'s idiom, same reasoning). When it is set this CLEARS the
/// one directory slot the reader and the writer share rather than merely
/// answering `true`: a switch that only stopped new renders would leave every
/// already-baked segment being served, which is not "off".
///
/// **Phase 61 (D-06)** lifted it out of [`poll_and_spawn`]'s body so the pump
/// re-reads it per tick through the SAME code rather than through a second copy
/// that could drift. Answering `true` never means "exit": both callers simply
/// do nothing more this tick, and a flip back re-enables with no rebuild.
fn kill_switch_engaged() -> bool {
    if std::env::var_os(KILL_SWITCH_ENV).is_some() {
        preview::render_cache_lookup::configure_render_cache_dir(None);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// The trigger
// ---------------------------------------------------------------------------

/// Point this process's reader and writer at `ctx`'s cache directory, and start
/// at most ONE background segment render if the world wants one.
///
/// Called from [`crate::transport::run_transport`] — the one host-agnostic
/// funnel every playback command passes through, which is where 58-04 put
/// `configure_proxy_cache_dir` for the identical reason: the configuration is a
/// property of "there is an app cache dir", which is exactly what `AppCtx`
/// already provides, so neither shell has to learn what a render cache is
/// (D-34's no-shell-code rule).
///
/// Everything it does is best-effort:
///
/// 0. **the kill switch** ([`KILL_SWITCH_ENV`]) — set, and this returns having
///    cleared the shared directory slot, so nothing below happens and nothing
///    already baked is served either;
/// 1. configure the directory (one uncontended write lock);
/// 2. bail unless a host is registered and no render is already live;
/// 3. ask the detector for heavy candidates ([`HEAVY_SEGMENT_POLL_LIMIT`]);
/// 4. drop candidates the D-24 guard covers, ones permanently refused, ones out
///    of retries, and ones a previous poll already confirmed fresh on disk under
///    the current material generation — all of which are map probes;
/// 5. for the FIRST candidate that survives all of those, and only that one,
///    derive the segment's identity and ask the shipped read path whether it is
///    already on disk;
/// 6. take the SHARED encoder permit — **defer if it is not free** (D-23);
/// 7. register a row and detach one thread holding that permit.
///
/// # What this costs, honestly (59-REVIEW **WR-01**)
///
/// This function used to claim it "returns in microseconds" and "cannot block".
/// It did not. Step 5 was inside step 4's loop, so every candidate the guard did
/// not cover paid a full key-material assembly — one store-lock walk plus, per
/// visible clip, a `canonicalize`, a `metadata` and a `resolve_decode_source`
/// (itself a proxy-cache read). 59-05 measured a one-clip walk at 368 µs, so a
/// 6-layer window is ~2 ms per segment; the poll walks up to
/// [`HEAVY_SEGMENT_POLL_LIMIT`] = 64 of them, and the STEADY STATE was the worst
/// case — once every candidate is warm, every candidate reached the walk and the
/// poll returned having done nothing, at ~140 ms. On the caller's thread. Across
/// the C ABI. On every `Play`, `Pause`, `Seek`, `Step` and `SetLooping`.
///
/// The bound now is **at most ONE key-material walk per poll**, and zero once
/// the registry has caught up, because a confirmed-fresh row is remembered
/// ([`JobEntry::fresh_at`]) rather than re-derived. A cost claim the code
/// contradicts is worse than no claim, so: the remaining synchronous cost is one
/// write lock, one map probe per candidate, and — only while there is work
/// genuinely outstanding — one walk.
///
/// It cannot fail and cannot render more than one segment.
///
/// # Phase 61 (D-04) — the ctx-free split
///
/// Steps 2-7 above now live in [`poll_core`], which takes no [`AppCtx`] at all.
/// This function is the thin ctx-taking SHELL that resolves the only two things
/// the core cannot: the cache directory, and D-24's guard input. Its observable
/// behaviour is unchanged — the same steps run in the same order with the same
/// inputs — and the existing suite is the proof of that.
///
/// The split is not a tidy-up. A long-lived thread cannot hold an `AppCtx`
/// (`FfiAppCtx<'a>(&'a RudisCtx)` is a BORROW, not an `Arc`), so a ctx-free core
/// is the only way the idle pump can drive the SAME scheduler. There is exactly
/// one scheduler in this process, and not two.
///
/// # Quick 260828-h0u — steps 0-1 moved OUT, to [`ensure_pump_started`]
///
/// The preamble (kill switch, directory, slot configuration, `Once`-guarded
/// start) is now a function of its own so the PROJECT-OPEN funnel can start the
/// clock as well. Nothing about this path changed: the same steps run in the
/// same order with the same inputs and the same two silent early returns.
pub fn poll_and_spawn<C: AppCtx>(ctx: &C) {
    // Steps 0-1, now shared verbatim with the project-open funnel. `None` is
    // the same pair of silent early returns this function has always had —
    // switch set, or no cache dir.
    let Some(dir) = ensure_pump_started(ctx) else {
        return;
    };
    // Phase 71 (TRUST-01): nothing may spawn inside a device-lost recovery.
    if RENDER_CACHE_PAUSED.load(Ordering::SeqCst) {
        return;
    }

    // D-24's guard input, read from the ctx's OWN store — the same store the
    // registered host reads, on every host that has both (`FfiAppCtx` and
    // `ShellPreviewHost` share one `Arc`).
    //
    // Resolved HERE, into a plain value, and the lock released before
    // `poll_core` is entered. That ordering is the invariant on `poll_core`'s
    // signature; read it before touching either.
    let guard_seg = playing_playhead_segment(ctx);
    poll_core(&dir, guard_seg, false);
}

/// **Quick 260828-h0u (gate 1): the pump's START PATH, callable from the
/// PROJECT-OPEN funnel as well as the transport funnel.**
///
/// This is EXACTLY [`poll_and_spawn`]'s former preamble, moved without a change
/// of behaviour: the kill switch (D-06, re-read per call through
/// [`kill_switch_engaged`], which clears the shared directory slot when set),
/// the directory resolution, the ONE reader/writer slot configuration, and the
/// `Once`-guarded D-01/D-02 start.
///
/// # Why a second caller was needed at all — the measurement
///
/// Owner's six-layer 4K project, Release build, 2026-08-28: opened, left PAUSED
/// and untouched for 8+ minutes, it committed **zero** segments; a single 14 s
/// Play followed by Pause committed **all four** within ~60 s of paused wall.
/// The pump was never born, because the only caller of [`poll_and_spawn`] is
/// the transport funnel (`crate::transport::run_transport`) — so a project
/// nobody played had no clock at all. The clock is not a property of "the user
/// pressed Play"; it is a property of "a project is open and there is a cache
/// directory to write into".
///
/// # Idempotent by construction, which is what makes a second caller free
///
/// `configure_render_cache_dir` is a slot WRITE and [`start_pump_once`] is a
/// process-`Once` (D-01/D-02), so a second — or hundredth — call from either
/// funnel costs one `var_os`, one path join and one uncontended write lock. A
/// second ctx still does not get a second pump.
///
/// Under `cargo test` [`start_pump_once`] is the `#[cfg(test)]` NO-OP twin, so
/// no test can spawn a production pump through this path either: the
/// ~2100-test hazard closure (61-RESEARCH §B3) is INHERITED here, never
/// re-opened.
///
/// # Lock discipline (61-RESEARCH §A4)
///
/// Takes NO store lock — [`render_cache_dir`] reads `AppCtx::app_cache_dir()`
/// and nothing else — so it is safe to call from any point in either open
/// funnel, and in particular it can never nest against the non-reentrant
/// `Arc<Mutex<Store>>` that `AppCtx::store()` and `PreviewHost::store()` both
/// resolve to.
///
/// Returns the resolved cache directory so [`poll_and_spawn`] keeps its exact
/// shape. `None` means "kill switch set, or no cache dir" — the two silent
/// early returns the shipped preamble already had, unchanged.
pub(crate) fn ensure_pump_started<C: AppCtx>(ctx: &C) -> Option<PathBuf> {
    // THE RUNTIME KILL SWITCH — read PER CALL, before anything else happens
    // (D-06). See [`kill_switch_engaged`], which the pump shares.
    if kill_switch_engaged() {
        return None;
    }

    let dir = render_cache_dir(ctx).ok()?;
    // THE production wiring point 59-06 and 59-07 both handed to this plan by
    // name. Idempotent; the reader and the writer both read this one slot, so
    // they can never be pointed at two different places.
    preview::render_cache_lookup::configure_render_cache_dir(Some(dir.clone()));

    // D-01/D-02: THE CLOCK, started lazily from the first call that reached
    // here — after the kill-switch check and after the directory resolved, so
    // the pump is born holding an already-resolved `PathBuf` and can never run
    // against a directory nobody configured. Once per process: a second ctx
    // does not get a second pump, matching the process-global posture every
    // other seam in this area already takes.
    start_pump_once(dir.clone());

    Some(dir)
}

/// **Phase 61 (D-04): the ctx-free core** — everything [`poll_and_spawn`] does
/// from "bail unless a host is registered" onward, taking the two ctx-derived
/// inputs as parameters so a thread with no `AppCtx` can drive it.
///
/// `from_pump` is ATTRIBUTION ONLY (D-17): it is never branched on for policy,
/// it selects no different behaviour, and the one thing it does is add the
/// spawn it produced to [`RENDER_CACHE_JOBS_SPAWNED_IDLE`] as well as to
/// [`RENDER_CACHE_JOBS_SPAWNED`]. In particular the pump's proxy-deference rule
/// (D-10) is NOT here — it lives in the pump's own tick, above this call, so the
/// transport path cannot inherit it.
///
/// # ⚠ INVARIANT — `guard_seg` is a VALUE, never a held lock (61-RESEARCH §A4)
///
/// `guard_seg` is a plain `Option<i64>` whose store lock the CALLER acquired,
/// read and RELEASED before entering here. `AppCtx::store()` and
/// `PreviewHost::store()` resolve to the SAME `Arc<Mutex<Store>>` — `RudisCtx`
/// and `ShellPreviewHost` hold clones of one `Arc` — and a `std::sync::Mutex`
/// is NOT reentrant. This function takes that lock again, separately, through
/// [`canvas_and_generation`]. Sequentially: never nested.
///
/// Smuggling a live `MutexGuard` across this boundary would deadlock the app
/// the first time a poll found work, and the `Option<i64>` parameter type is
/// what makes it structurally unavailable. **Do not "optimize" the guard input
/// into a guard.** `render_cache_job_poll_core_two_concurrent_callers_spawn_at_most_once`
/// pins the shape under real two-thread concurrency.
fn poll_core(dir: &Path, guard_seg: Option<i64>, from_pump: bool) {
    let Some(host) = render_host() else {
        return;
    };

    // MAX_CONCURRENT_RENDER_CACHE_JOBS, as control flow. The shared permit below
    // is the stronger bound (it counts the proxy worker too); this one keeps the
    // registry honest and skips the candidate walk while a render is in flight.
    if jobs().values().any(JobEntry::is_live) {
        return;
    }

    // 59-REVIEW WR-04: a ROTATING window, so the poll advances through the whole
    // candidate set across successive calls instead of re-walking a fixed
    // lowest-64 prefix forever.
    let after = LAST_CONSIDERED.load(Ordering::Relaxed);
    let candidates =
        preview::render_cache_detect::heavy_segments_from(after, HEAVY_SEGMENT_POLL_LIMIT);
    if candidates.is_empty() {
        return;
    }

    let Some((canvas, edit_gen)) = canvas_and_generation(host.as_ref()) else {
        return;
    };

    // The generation any conclusion about a segment's identity is valid within.
    // Both halves, for 59-REVIEW WR-02's reason — see [`MaterialGen`].
    let material_gen: MaterialGen = (edit_gen, preview::decode_source::decode_answer_generation());

    // ---- pass one: the CHEAP disqualifiers, over every candidate ------------
    //
    // Nothing in this loop touches the filesystem or the store. It is a map
    // probe and two integer comparisons per candidate (59-REVIEW WR-01).
    let mut chosen: Option<(i64, u32, u32)> = None;
    // Where this poll stopped. Advanced for EVERY candidate it looks at,
    // including the ones it declines, so a segment the guard covers or a
    // refusal remembers does not pin the window in place (WR-04).
    let mut last_seen = after;
    for seg in candidates {
        last_seen = seg;
        if let Some(playhead) = guard_seg {
            if (seg - playhead).abs() <= PLAYBACK_GUARD_SEGMENTS {
                RENDER_CACHE_JOBS_DEFERRED_PLAYBACK.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        let budgets = {
            let jobs = jobs();
            match jobs.get(&seg) {
                // 59-07's PERMANENT refusal. Never re-enqueued, at all.
                Some(row) if row.is_permanent() => continue,
                Some(row) if row.attempts >= MAX_SEGMENT_ATTEMPTS => continue,
                Some(row) if row.external_aborts >= MAX_EXTERNAL_ABORTS => continue,
                // A previous poll already derived this segment's identity and
                // found the file on disk, under the SAME material generation.
                // Nothing can have changed it without moving that generation, so
                // re-deriving it would be paying a key-material walk to learn
                // what the registry already knows.
                Some(row) if row.fresh_at == Some(material_gen) => continue,
                Some(row) => (row.attempts, row.external_aborts),
                None => (0, 0),
            }
        };
        chosen = Some((seg, budgets.0, budgets.1));
        break;
    }
    LAST_CONSIDERED.store(last_seen, Ordering::Relaxed);
    let Some((seg, attempts, external_aborts)) = chosen else {
        return;
    };

    // ---- pass two: ONE key-material walk, for the one survivor -------------
    //
    // Already on disk under the CURRENT identity? Then there is nothing to do —
    // and this is the shipped read path, not a second opinion about what "fresh"
    // means. The conclusion is REMEMBERED so the next poll skips this segment in
    // pass one instead of arriving back here.
    let mut lookup = SegmentLookup::new();
    if let Some(hash) = lookup.segment_hash_memo(host.as_ref(), seg, edit_gen, canvas) {
        if rendercache::cache::read_fresh_segment(dir, seg, hash, canvas.0, canvas.1, canvas.2)
            .is_some()
        {
            mark_fresh_on_disk(seg, material_gen);
            return;
        }
    }

    // ---- D-23: the SHARED admission, and the load-bearing line of this file --
    //
    // `try_take_permit` is `crate::proxy_job`'s own accessor for the ONE
    // process-wide encoder semaphore. Not a copy of it, not a sibling with the
    // same bound — the same static. If the proxy worker holds it, this render
    // DEFERS: it does not queue, because a queued render is a second encoder
    // waiting its turn, and the next poll is milliseconds away.
    let Some(permit) = crate::proxy_job::try_take_permit() else {
        RENDER_CACHE_JOBS_DEFERRED_ADMISSION.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let epoch = ROW_EPOCH.fetch_add(1, Ordering::Relaxed);
    {
        let mut jobs = jobs();
        // 71-REVIEW WR-01: the entry checks above are a fast path only. The poll
        // between them and here is long (candidate walk, disk read, permit), so a
        // device-lost recovery may have paused-and-cancelled in the meantime. The
        // AUTHORITATIVE check is this one, under the same lock the row is
        // registered under and the same lock `pause_and_cancel_all` takes: a row
        // is either registered BEFORE the pause (and so has its latch raised by
        // it) or not at all. The permit drops on this return.
        if RENDER_CACHE_PAUSED.load(Ordering::SeqCst) {
            return;
        }
        evict_finished_if_full(&mut jobs);
        jobs.insert(
            seg,
            JobEntry {
                epoch,
                state: ROW_QUEUED,
                cancel: Arc::clone(&cancel),
                attempts: attempts.saturating_add(1),
                external_aborts,
                // This poll just proved it is NOT fresh on disk.
                fresh_at: None,
            },
        );
    }
    // WR-04: from here the row belongs to the guard, and the guard belongs to
    // whoever does the work. Built BEFORE the spawn and moved in, so a thread
    // that fails to spawn still clears its own row.
    let row = RowGuard { seg, epoch };

    // The render thread outlives this call, so it needs an OWNED directory.
    // (Before Phase 61's split `dir` was a `PathBuf` local moved straight in.)
    let job_dir = dir.to_path_buf();

    let spawned = std::thread::Builder::new()
        .name("rudis-render-cache".to_string())
        .spawn(move || {
            // Both named here so they are captured by move and cover every exit,
            // including a panic inside the writer: Rust runs `Drop` while
            // unwinding, so a panicking render returns the permit and demotes
            // its row rather than wedging the pool and the worker together.
            let permit = permit;
            let _row = row;

            set_state(seg, epoch, ROW_RUNNING);
            RENDER_CACHE_JOBS_SPAWNED.fetch_add(1, Ordering::Relaxed);
            // D-17's attribution, at the EXISTING spawn-count site so it cannot
            // drift from it: the flag travelled with THIS spawn, so a concurrent
            // transport spawn can never be miscounted as idle.
            if from_pump {
                RENDER_CACHE_JOBS_SPAWNED_IDLE.fetch_add(1, Ordering::Relaxed);
            }

            let outcome = {
                // The ONE test-only branch on this path — see [`RENDER_GATE`].
                // One uncontended lock acquisition per SEGMENT, never per frame,
                // scoped so the gate is held for exactly the render.
                #[cfg(test)]
                let _gate = RENDER_GATE.lock().unwrap_or_else(|p| p.into_inner());
                preview::render_cache_writer::render_segment(host.as_ref(), seg, &cancel)
            };

            // Hand the encoder back BEFORE publishing the terminal state, so
            // "this segment has no live row" IMPLIES "the shared pool is whole
            // again". The other order is correct too but unobservable: a poller
            // that saw the row finish could still find `try_take_permit`
            // answering `None` for as long as it took this frame to return, and
            // an admission bound whose release cannot be observed is one nobody
            // can write a gate for.
            drop(permit);

            // 59-07's refusal split, which is the whole reason `RenderOutcome`
            // has six arms instead of two. Getting this wrong livelocks the
            // worker on a permanently undecodable clip — or, as 59-REVIEW WR-03
            // found, starves the very sections the user is working on.
            let (state, settlement) = match &outcome {
                RenderOutcome::Committed { .. } => (ROW_DONE, AttemptSettlement::Cleared),
                // Never re-enqueued at all, so its budget is moot; recorded as
                // charged rather than cleared so nothing here depends on that.
                RenderOutcome::NotCacheable { .. } => (ROW_REFUSED, AttemptSettlement::Charged),
                // EXTERNALLY caused. `StaleAborted` is raised on every store
                // mutation during a ~1.3 s render, and `Cancelled` comes from
                // project open / project new / media removal / shutdown. Every
                // one of those is a user action, and none of them says anything
                // about whether this segment can be rendered.
                RenderOutcome::Cancelled | RenderOutcome::StaleAborted => {
                    (ROW_RETRY, AttemptSettlement::External)
                }
                // Genuinely bad luck or a genuinely bad clip: what the budget is
                // for.
                RenderOutcome::Incomplete { .. } | RenderOutcome::Failed(_) => {
                    (ROW_RETRY, AttemptSettlement::Charged)
                }
            };
            set_state_and_settle(seg, epoch, state, settlement);

            if let RenderOutcome::Committed {
                payload_bytes,
                wall_ms,
                frames,
            } = outcome
            {
                // Phase 71 (TRUST-03): the ONLY bump site — a commit, and
                // nothing else, moves the progress number the shell shows.
                RENDER_CACHE_SEGMENTS_COMMITTED.fetch_add(1, Ordering::Relaxed);
                // D-32: the generation cost is written down, never hidden.
                eprintln!(
                    "render-cache: committed segment {seg} in {wall_ms} ms \
                     ({frames} frames, {payload_bytes} bytes)"
                );
                // D-25's byte budget, enforced right after the commit that could
                // have exceeded it — the same place `proxy::generate` prunes.
                rendercache::cache::prune_bytes(
                    &job_dir,
                    rendercache::cache::MAX_RENDER_CACHE_BYTES,
                );
            }
        })
        .is_ok();
    if !spawned {
        // The guard was moved into the closure that never ran, so it has already
        // dropped and demoted the row. Nothing to unwind; the permit went with
        // it.
        eprintln!("render-cache: could not start a render thread for segment {seg}");
    }
}

// ---------------------------------------------------------------------------
// Phase 61: the clock (D-01 .. D-06, D-10)
// ---------------------------------------------------------------------------

/// **D-03.** How long the idle pump sleeps between ticks: **500 ms**.
///
/// The arithmetic, written down because a cost claim this file cannot justify
/// is worse than no claim ([`poll_and_spawn`]'s own doc records why). A
/// first-time bake is **5.3-7.4 s** of wall, and [`poll_core`] returns at its
/// `jobs().values().any(is_live)` early-out for the whole of it — so at most
/// ~1 cheap no-op poll in every ~10-15 lands during a bake. In the steady warm
/// state a poll is one uncontended write lock plus one map probe per candidate
/// (59-REVIEW WR-01's post-fix bound), so 2 Hz is free. 500 ms also bounds
/// re-warm latency after an invalidating edit to half a second, which is well
/// under human notice.
///
/// `pub` so a bench cites the SHIPPED number instead of typing its own, and so
/// a later phase moves the cadence in exactly one place.
pub const RENDER_CACHE_PUMP_INTERVAL: Duration = Duration::from_millis(500);

/// **ONE REAL pump tick** — the whole of what the idle clock does, in the order
/// it does it, and the only body any pump (production or test) ever runs.
///
/// 1. count the tick ([`RENDER_CACHE_PUMP_TICKS`], D-17) — FIRST, so a parked
///    or switched-off pump is still visibly beating;
/// 2. **D-06** re-read the kill switch, which clears the shared directory slot
///    when set. Returns for this tick only; it never exits, so a flip back
///    re-enables with no rebuild;
/// 3. **D-05** park when no host is registered — a ctx going away calls
///    `clear_render_host()`, and a later ctx registering a new host must find
///    the pump still beating;
/// 4. point the shared directory slot at the directory this pump was born
///    holding — the OTHER half of step 2, and the reason it is here is below;
/// 5. **D-10/D-11** defer to pending proxy work, BEFORE anything can reach the
///    shared encoder permit. This check lives HERE and never inside
///    [`poll_core`]: inside the core it would also apply to the
///    transport-triggered path, whose behaviour D-12 requires be unchanged;
/// 6. derive D-24's guard input from the host's store and drive the SAME
///    scheduler the transport funnel drives, flagged as the idle path.
///
/// # Why step 4 exists (found by WARM-05's own gate, 61-05)
///
/// D-06 says the pump does *what [`poll_and_spawn`] does* with the switch.
/// [`poll_and_spawn`] does TWO things about the shared directory slot: it
/// CLEARS it when the switch is set, and it CONFIGURES it when the switch is
/// not. Phase 61 shipped only the first half here, and the asymmetry is not
/// cosmetic — the slot is the single source of truth for the reader AND the
/// writer (`render_cache_lookup::configured_dir`), so a pump that re-armed
/// after a flip-back would spawn renders that refuse at
/// *"no render-cache directory is configured for this process"*, every tick,
/// until some transport command happened to re-point it. On a PAUSED app there
/// is no such command — which is the entire situation this pump exists for.
///
/// It is unconditional, exactly as [`poll_and_spawn`]'s is, because "the pump
/// does what the poll does" is the only rule here that stays true under
/// review. The cost is one uncontended `RwLock` write plus one `PathBuf` clone
/// per 500 ms — nothing beside a tick that is already allowed to take an
/// encoder permit and a decode pool. It sits AFTER the host park (a ctx that
/// went away should not have a slot pointed at its directory) and BEFORE the
/// proxy deference (that rule is about the ENCODER; serving a frame the cache
/// already holds costs the proxy worker nothing).
///
/// `pub` so a bench arm can drive the body production runs rather than a
/// hand-rolled stand-in that would prove something else (research Pitfall 1).
/// **Not part of any stable ABI** — nothing exports it across the C boundary.
pub fn pump_tick(dir: &Path) {
    RENDER_CACHE_PUMP_TICKS.fetch_add(1, Ordering::Relaxed);
    if kill_switch_engaged() {
        return;
    }
    // Phase 71 (TRUST-01): the tick still counts (the pump is visibly beating)
    // but nothing may spawn inside a device-lost recovery.
    if RENDER_CACHE_PAUSED.load(Ordering::SeqCst) {
        return;
    }
    let Some(host) = render_host() else {
        return;
    };
    preview::render_cache_lookup::configure_render_cache_dir(Some(dir.to_path_buf()));
    if crate::proxy_job::has_pending_work() {
        return;
    }
    let guard_seg = playing_playhead_segment_from_host(host.as_ref());
    poll_core(dir, guard_seg, true);
}

/// The pump's own loop: sleep, tick, forever.
///
/// A plain [`std::thread::sleep`] loop, `crates/ffi/src/self_advance.rs`'s
/// `tick_loop` shape — and deliberately **never `tokio`**. `RudisCtx.runtime` is
/// a `OnceLock` built lazily on the first `block_on`, so an app that opens a
/// project and presses Play before ever importing has no runtime at all at the
/// moment D-01 starts this thread, and asking the runtime for its current
/// handle would panic exactly there. The module doc already makes this argument
/// for the one-shot render thread; it applies more urgently to a loop that
/// outlives every call.
///
/// The rule is greppable on purpose: this file names no runtime-timer or
/// task-spawn API at all, in code OR in prose, so the check is a plain scan
/// with no comment-blanking step.
///
/// It sleeps FIRST so the poll that started it is not immediately repeated, and
/// it holds no lock, no permit and no file handle across the sleep.
///
/// `#[cfg(not(test))]` alongside its only caller: under `cargo test` this loop
/// has no exit at all, and the test lifecycle deliberately runs the same
/// [`pump_tick`] under a stop flag instead (see `spawn_pump_for_test`).
#[cfg(not(test))]
fn pump_loop(dir: PathBuf) {
    loop {
        std::thread::sleep(RENDER_CACHE_PUMP_INTERVAL);
        pump_tick(&dir);
    }
}

/// **D-01/D-02.** Start the clock, at most once per process.
///
/// Called from [`poll_and_spawn`] after the kill-switch check and the directory
/// resolve, so the thread is born holding a real `PathBuf` (the host has a store
/// but no `app_cache_dir()`, which is why `set_render_host` could not be the
/// trigger). The `Once` makes a second ctx's first poll a no-op rather than a
/// second pump. Best-effort: a failed spawn is dropped on the floor, matching
/// this module's posture everywhere else.
///
/// The production pump PARKS and is never joined — there is no owning ctx to
/// join it, and on process exit every thread goes at once. It holds nothing
/// across its sleep, so that is not a new class of risk (`self_advance`'s
/// per-ctx stop/join shape is reused only by the TEST handle below).
#[cfg(not(test))]
fn start_pump_once(dir: PathBuf) {
    static PUMP_STARTED: std::sync::Once = std::sync::Once::new();
    PUMP_STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("rudis-render-cache-pump".to_string())
            .spawn(move || pump_loop(dir));
    });
}

/// The `cargo test` twin: **a no-op, at compile time**.
///
/// `JOBS`, `HOST`, `LAST_CONSIDERED`, the detector's registry and the SHARED
/// encoder permit are process-globals shared by every test in this crate's ONE
/// test binary, which the harness runs on as many threads as the machine has
/// cores. An always-on 500 ms thread inside that binary would fire during ~2100
/// unrelated tests and could poison every test after it — so no test spawns the
/// production pump, by construction, with zero changes to any existing test.
///
/// A test that needs the REAL pump uses [`spawn_pump_for_test`], which runs the
/// SAME [`pump_tick`] body on a joinable thread inside the module's `lease()`.
#[cfg(test)]
fn start_pump_once(_dir: PathBuf) {}

/// The playhead's segment index, but ONLY while the timeline is genuinely
/// playing — `None` otherwise, which is the "no guard applies" answer.
///
/// `Store::playback()` is the PROGRAM (timeline) playback specifically, never
/// `active_playback()`. Source-monitor playback is not timeline playback: a
/// MediaBin clip in the Source monitor advances its own `position_us` against
/// that CLIP's clock, which has nothing to do with the program grid the cache is
/// cut on, so guarding a range derived from it would guard an arbitrary and
/// wrong one.
fn playing_playhead_segment<C: AppCtx>(ctx: &C) -> Option<i64> {
    let store = ctx.store().lock().ok()?;
    let playback = store.playback();
    if !playback.playing {
        return None;
    }
    Some(preview::render_cache_lookup::segment_index_for(
        playback.position_us,
    ))
}

/// **Phase 61 (D-04): the same answer, off the HOST's store.**
///
/// [`playing_playhead_segment`]'s twin for a caller that has no `AppCtx` — the
/// idle pump. It mirrors that function line for line (the PROGRAM playback, the
/// same `playing` test, the same `position_us`, the same grid function),
/// deliberately, because a guard the two paths computed differently would be
/// two policies wearing one name.
///
/// Reading it from the host is legitimate and is what
/// [`playing_playhead_segment`]'s own doc already records: the guard input comes
/// from the ctx's own store, which IS the store the registered host reads —
/// `FfiAppCtx` and `ShellPreviewHost` hold clones of one `Arc`.
///
/// Like its twin it returns a plain VALUE with the lock already released, which
/// is what [`poll_core`]'s invariant requires of its caller.
fn playing_playhead_segment_from_host(host: &dyn PreviewHost) -> Option<i64> {
    let store = host.store()?;
    let playback = store.playback();
    if !playback.playing {
        return None;
    }
    Some(preview::render_cache_lookup::segment_index_for(
        playback.position_us,
    ))
}

/// `((canvas_w, canvas_h, fps), edit_generation)` read from the HOST's store —
/// the same two reads the writer makes, through the same accessors, so a
/// scheduler decision and the render it schedules cannot disagree about which
/// arrangement they are talking about.
fn canvas_and_generation(host: &dyn PreviewHost) -> Option<((u32, u32, f64), u64)> {
    let store = host.store()?;
    Some((store.project_canvas(), store.seq()))
}

// ---------------------------------------------------------------------------
// D-38: the bounded re-scan
// ---------------------------------------------------------------------------

/// **D-38.** After a structural edit, re-check the cached segments within
/// [`RESCAN_WINDOW_SEGMENTS`] of `playhead_seg` and delete the ones whose
/// on-disk identity no longer matches the arrangement.
///
/// HYGIENE, never correctness — see the module doc. Nothing outside the window
/// is touched, and nothing outside the window can be served stale either, which
/// is the pair of facts `render_cache_job_rescan_window` asserts together
/// because either one alone would be misleading.
pub fn on_structural_edit<C: AppCtx>(ctx: &C, playhead_seg: i64) {
    rescan(ctx, playhead_seg, None);
}

/// How often D-38's re-scan may actually run, however often an edit arrives
/// (59-REVIEW **WR-05**).
///
/// The sweep is ~17 memoized key-material lookups — 59-05 measured a one-clip
/// walk at 368 µs, so a 6-layer window is ~37 ms — and its trigger is the
/// dispatch funnel, which a drag can hit dozens of times a second. Un-throttled
/// that would put the whole sweep on the user's own edit, repeatedly, to reclaim
/// bytes nobody is waiting for.
///
/// One second is chosen so the WORST case is ~4 % of one thread during a
/// continuous drag, and it costs nothing that matters: D-38 is explicitly
/// hygiene and never correctness, D-18 refuses a stale segment reactively
/// whether or not this ever runs, and D-25's LRU reclaims anything this misses.
/// A skipped sweep is a slightly later reclaim, not a wrong frame.
const RESCAN_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// When the last re-scan actually ran. `None` until the first one.
static LAST_RESCAN: Mutex<Option<Instant>> = Mutex::new(None);

/// Claim the next re-scan slot, or answer `false` because one ran too recently.
///
/// Poisoning is RECOVERED rather than propagated, for [`jobs`]'s reason: a
/// throttle that could panic its caller would turn a hygiene sweep into a failed
/// user edit.
fn take_rescan_slot() -> bool {
    let mut slot = LAST_RESCAN.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    match *slot {
        Some(last) if now.duration_since(last) < RESCAN_MIN_INTERVAL => false,
        _ => {
            *slot = Some(now);
            true
        }
    }
}

/// **D-38's production trigger** (59-REVIEW **WR-05**).
///
/// [`on_structural_edit`] and [`on_structural_edit_over`] shipped fully
/// implemented and fully tested with no caller anywhere outside this file's own
/// test module, which made three things in the tree false at once: the module
/// doc's present-tense claim that orphaned bytes are reclaimed around the
/// playhead, [`RENDER_CACHE_RESCAN_DELETED`] as an observable that could ever
/// move, and [`RESCAN_WINDOW_SEGMENTS`] as live configuration rather than dead.
///
/// This is the five lines that make them true. It goes on
/// [`crate::dispatch`] — the ONE funnel every host and every agent tool passes
/// through, and the one place that already knows an edit happened AND can read
/// the playhead — filtered by `preview::patch_touches_preview` so a media-bin
/// import or an ink stroke does not pay for a sweep it cannot have invalidated
/// anything with, and throttled by [`RESCAN_MIN_INTERVAL`] so a drag does not
/// pay for one per dispatch.
///
/// Best-effort and infallible, like everything else here: no host, no cache
/// directory, an unreadable store or a throttled slot all return silently, and
/// none of them can fail the edit that triggered them.
pub fn on_structural_edit_at_playhead<C: AppCtx>(ctx: &C) {
    if !take_rescan_slot() {
        return;
    }
    let playhead_seg = {
        let Ok(store) = ctx.store().lock() else {
            return;
        };
        preview::render_cache_lookup::segment_index_for(store.playback().position_us)
    };
    rescan(ctx, playhead_seg, None);
}

/// [`on_structural_edit`] plus the edit's own extent, for the callers that have
/// one.
///
/// The extent arrives as a plain `(start_us, end_us)` pair of program
/// microseconds, NOT as a domain patch: D-26 keeps every mutation type out of
/// this module, and an extent is arithmetic rather than document state. A caller
/// that knows which clip moved converts it once, at its own altitude.
///
/// `PatchKind::ClipRemoved` carries no pre-removal extent (D-38's own finding),
/// which is exactly why the window exists and why this is the optional half.
pub fn on_structural_edit_over<C: AppCtx>(ctx: &C, playhead_seg: i64, extent_us: (i64, i64)) {
    rescan(ctx, playhead_seg, Some(extent_us));
}

fn rescan<C: AppCtx>(ctx: &C, playhead_seg: i64, extent_us: Option<(i64, i64)>) {
    let Ok(dir) = render_cache_dir(ctx) else {
        return;
    };
    let Some(host) = render_host() else {
        return;
    };
    let Some((canvas, edit_gen)) = canvas_and_generation(host.as_ref()) else {
        return;
    };

    let mut window: HashSet<i64> =
        ((playhead_seg - RESCAN_WINDOW_SEGMENTS)..=(playhead_seg + RESCAN_WINDOW_SEGMENTS))
            .collect();
    if let Some((start_us, end_us)) = extent_us {
        // Half-open on the right, matching the segment grid's own `[start, end)`
        // — an edit that ends exactly on a boundary does not touch the segment
        // that starts there.
        let first = preview::render_cache_lookup::segment_index_for(start_us.min(end_us));
        let last =
            preview::render_cache_lookup::segment_index_for(end_us.max(start_us).saturating_sub(1));
        for k in first..=last {
            window.insert(k);
        }
    }

    // The expected identity for each segment in the window, memoized on
    // `(segment, generation)` by the reader's own builder — so this sweep and
    // the serving path cannot disagree about what a segment IS.
    let mut lookup = SegmentLookup::new();
    let mut expected: HashMap<i64, Option<u64>> = HashMap::with_capacity(window.len());
    for &k in &window {
        let hash = lookup.segment_hash_memo(host.as_ref(), k, edit_gen, canvas);
        expected.insert(k, hash);
    }

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten().take(MAX_STATUS_DIR_ENTRIES) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(rendercache::cache::SEGMENT_META_SUFFIX) else {
            continue;
        };
        let Some((hash, seg)) = parse_stem(stem) else {
            continue;
        };
        if !window.contains(&seg) {
            continue;
        }
        match expected.get(&seg) {
            // Still current: leave it exactly where it is.
            Some(Some(current)) if *current == hash => continue,
            // Unhashable right now (a clip whose media cannot be stat-ed).
            // Deliberately NOT deleted: the segment is already unservable —
            // fail-closed, reactively — and a transiently missing file must not
            // cost the user a cache entry they will want back in a second. The
            // LRU reclaims it if it really is an orphan.
            Some(None) => continue,
            _ => {}
        }
        // The META FIRST: it is the commit marker, so removing it makes the
        // segment unservable immediately and a crash between the two deletes
        // leaves an orphaned payload rather than a payload with no pixels
        // behind a meta that promises them.
        let meta_removed = std::fs::remove_file(dir.join(&name)).is_ok();
        let _ = std::fs::remove_file(dir.join(rendercache::cache::payload_file_name(stem)));
        if meta_removed {
            RENDER_CACHE_RESCAN_DELETED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `{hash:016x}-k{seg_index}` -> `(hash, seg_index)`.
///
/// The inverse of `rendercache::cache::file_stem_for`. Anything that does not
/// parse is not one of ours and is left alone — a cache directory is a place a
/// user may have dropped a file.
fn parse_stem(stem: &str) -> Option<(u64, i64)> {
    let (hex, rest) = stem.split_once("-k")?;
    let hash = u64::from_str_radix(hex, 16).ok()?;
    let seg = rest.parse::<i64>().ok()?;
    Some((hash, seg))
}

// ---------------------------------------------------------------------------
// Cancellation (D-24's caller list = 58 D-11's)
// ---------------------------------------------------------------------------

/// Raise EVERY render-cache cancel latch.
///
/// Called from the same three places `proxy_job::cancel_all` is — creating or
/// opening a project, removing media, and process shutdown — because all three
/// mean the arrangement the in-flight render was named after is about to stop
/// existing. 59-07 proved the latch is real at the layer below: the encoder
/// child is KILLED and reaped and nothing is left on disk, measured at ~40 ms
/// from latch to return.
///
/// Best-effort and idempotent: latches for finished rows are raised too, which
/// is harmless and keeps this function free of any assumption about which states
/// are terminal.
pub fn request_cancel_all() {
    for row in jobs().values() {
        row.cancel.store(true, Ordering::Relaxed);
    }
}

/// **D-07: the targeted sibling of [`request_cancel_all`]** — raise ONE row's
/// existing cancel latch, keyed by segment index, and only when that row is
/// actually LIVE. Answers whether it raised anything.
///
/// It takes the registry map by reference rather than acquiring `jobs()`
/// itself, so `on_play_transition` can settle the WHOLE guard band under a
/// single lock acquisition. That is the same "the caller owns the lock, the
/// callee gets a value" discipline `poll_core`'s `guard_seg` parameter
/// enforces, one level down.
///
/// No new state and no new settlement class (D-09). A latch raised mid-render
/// settles as `RenderOutcome::Cancelled` -> `AttemptSettlement::External`,
/// which is the EXISTING correct classification — this is a user action, not a
/// segment failure — and it is bounded by the existing
/// [`MAX_EXTERNAL_ABORTS`]. Nothing partial can reach the final name either:
/// the writer commits by atomic temp-then-rename and its abort arm DELETES the
/// temp file instead of renaming it, so a cancelled bake leaves the
/// previously-committed segment, or nothing, and never a torn one.
fn cancel_segment(rows: &HashMap<i64, JobEntry>, seg: i64) -> bool {
    match rows.get(&seg) {
        Some(row) if row.is_live() => {
            row.cancel.store(true, Ordering::Relaxed);
            true
        }
        _ => false,
    }
}

/// **D-07/D-08 — THE YIELD: resume-from-pause must not contend.**
///
/// Called by [`crate::transport::run_transport`] AFTER its store apply, and
/// only on an actual `false -> true` PROGRAM-playing transition. Cancels a LIVE
/// bake within [`PLAYBACK_GUARD_SEGMENTS`] of the new playhead segment; a bake
/// OUTSIDE the band runs to completion, because it is paid-for work, the guard
/// already says it is not in the user's way, and discarding it would make
/// resume cost more rather than less.
///
/// Why this exists at all, given the guard already re-engages on its own:
/// `poll_core`'s D-24 guard only gates a bake that has not STARTED. An
/// already-running one keeps its encoder and keeps the SHARED permit for the
/// rest of its 5.3-7.4 s, which is precisely the contention a user feels when
/// they press Play. Phase 61's idle clock makes an in-flight bake strictly MORE
/// likely at that instant, so this behaviour is DECIDED and proven here rather
/// than inherited.
///
/// It probes the `2 * PLAYBACK_GUARD_SEGMENTS + 1` keys of the band directly
/// instead of scanning the registry, so its cost is bounded by that constant
/// and never by [`MAX_TRACKED_RENDER_CACHE_JOBS`]. Saturating arithmetic
/// because a segment index is derived from a position, and nothing here should
/// depend on that never being extreme.
///
/// Ctx-free on purpose, exactly like `poll_core`: `new_seg` is an
/// already-copied VALUE whose store lock the caller released, and the only
/// state this touches is the process-global registry. `AppCtx::store()` and
/// `PreviewHost::store()` resolve to the same non-reentrant mutex, so a hook
/// that wanted a ctx here would be one refactor away from deadlocking the app
/// on Play.
pub fn on_play_transition(new_seg: i64) {
    let rows = jobs();
    let lo = new_seg.saturating_sub(PLAYBACK_GUARD_SEGMENTS);
    let hi = new_seg.saturating_add(PLAYBACK_GUARD_SEGMENTS);
    let mut cancelled = 0usize;
    for seg in lo..=hi {
        if cancel_segment(&rows, seg) {
            cancelled += 1;
        }
    }
    if cancelled > 0 {
        // D-32's posture: the cost of a decision is written down, never hidden.
        eprintln!(
            "render-cache: play at segment {new_seg} yielded {cancelled} \
             in-flight segment render(s) inside the \
             {PLAYBACK_GUARD_SEGMENTS}-segment playback guard"
        );
    }
}

/// **59-REVIEW WR-06.** Forget every finished row, and restart the rotating
/// candidate window.
///
/// Called from the project new / project open paths, immediately after
/// [`request_cancel_all`] — in that order, because a cleared row is a latch
/// nobody can raise. Every row is keyed by SEGMENT INDEX, a coordinate on a
/// global program-time grid, so a row that means "permanently refused" or "two
/// attempts spent" in one project says nothing at all about the next one, and
/// carrying it over is a silent permanent disable for a segment index the new
/// project has never even rendered.
///
/// Correctness never depended on this — the segment HASH differs across
/// projects, so nothing wrong is ever served — but the wasted work and the
/// refusal leak are real.
///
/// **A LIVE row is kept**, exactly as [`evict_finished_if_full`] keeps one and
/// for the same reason: it owns the latch the cancel above just raised, and it
/// is the concurrency check a second render would otherwise walk straight past.
/// It settles into an ordinary retry row within a tick of the cancel landing
/// (59-04 measured ~40 ms from latch to return), so the residue is at most one
/// row for at most that long.
pub fn forget_all_rows() {
    jobs().retain(|_, row| row.is_live());
    LAST_CONSIDERED.store(i64::MIN, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// THE PURE READ (D-30)
// ---------------------------------------------------------------------------

/// **The poll-only status getter.**
///
/// It is not possible for this function to start a render, delete a file, decode
/// a frame or spawn a process. There is no code path from here to
/// `render_segment`, and that is load-bearing rather than incidental: this is
/// the shell's only route to cache state, so "a poll never triggers work" must
/// hold even if a future caller polls it in a tight loop.
/// [`crate::proxy_job::run_get_proxy_status`]'s rule, verbatim.
///
/// Unlike the proxy getter it takes **no arguments**: a proxy is a property of
/// one media item, while the render cache is a property of the PROGRAM. There is
/// no id to pass and therefore — pleasantly — no opaque-id/traversal surface to
/// defend (T-58-05-01 has no twin here).
///
/// `Err` only for a host that cannot resolve its own cache directory, which is a
/// genuine host fault and is unreachable on the C ABI host (`FfiAppCtx`'s
/// `app_cache_dir` is an infallible clone).
pub fn run_get_render_cache_status<C: AppCtx>(
    ctx: &C,
) -> Result<RenderCacheStatusPayload, String> {
    let dir = render_cache_dir(ctx)?;

    let rendering_segment = {
        let jobs = jobs();
        jobs.iter()
            .find(|(_, row)| row.is_live())
            .map(|(seg, _)| *seg)
            .unwrap_or(-1)
    };

    let (cached_segments, dir_exists) = census(&dir);
    let heavy_segments =
        preview::render_cache_detect::heavy_segments(HEAVY_SEGMENT_POLL_LIMIT).len() as u64;

    let state = if rendering_segment >= 0 {
        STATE_RENDERING
    } else if dir_exists {
        STATE_IDLE
    } else {
        STATE_NONE
    };

    Ok(RenderCacheStatusPayload {
        state: state.to_string(),
        rendering_segment,
        cached_segments,
        heavy_segments,
        // D-17. One relaxed load, beside a `census` that already walked a
        // directory — free at this poll's own scale, and the only thing on this
        // payload that can distinguish "warmed while idle" from "warmed because
        // you played it again".
        idle_spawned: RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed),
        // TRUST-03: one more relaxed load; the poll still starts no work.
        committed_total: RENDER_CACHE_SEGMENTS_COMMITTED.load(Ordering::Relaxed),
    })
}

/// Distinct segment indices with a committed meta on disk, and whether the
/// directory exists at all.
///
/// T-59-08-05: names only. No file is opened, the walk stops at
/// [`MAX_STATUS_DIR_ENTRIES`], and an unreadable directory answers `(0, false)`
/// rather than propagating — a status poll must not be able to fail because of
/// a filesystem the user is free to delete under it (D-25).
fn census(dir: &Path) -> (u64, bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, false);
    };
    let mut seen: HashSet<i64> = HashSet::new();
    for entry in entries.flatten().take(MAX_STATUS_DIR_ENTRIES) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(rendercache::cache::SEGMENT_META_SUFFIX) else {
            continue;
        };
        if let Some((_, seg)) = parse_stem(stem) {
            seen.insert(seg);
        }
    }
    (seen.len() as u64, true)
}

// ---------------------------------------------------------------------------
// Test-only surface
// ---------------------------------------------------------------------------

/// Held by a render thread for the whole of its `render_segment` call, so a test
/// can park a job IN FLIGHT — with the shared permit held — and ask what the
/// world looks like from outside.
///
/// The only `#[cfg(test)]` branch in the production path, and it is one lock
/// acquisition of an uncontended process-global mutex per SEGMENT (not per
/// frame, not per tick). There is no other way to observe "this render is
/// holding the encoder permit right now" from another thread: 59-07's own gates
/// had to publish a frame counter for the same reason, and a wall-clock sleep
/// was MEASURED to race a 1.3 s render and lose.
#[cfg(test)]
static RENDER_GATE: Mutex<()> = Mutex::new(());

/// Park every render at the top of its work until the returned guard drops.
#[cfg(test)]
pub(crate) fn hold_render_gate() -> MutexGuard<'static, ()> {
    RENDER_GATE.lock().unwrap_or_else(|p| p.into_inner())
}

/// How many rows the registry holds — diagnostic only; nothing branches on it.
#[cfg(test)]
pub(crate) fn tracked_row_count() -> usize {
    jobs().len()
}

/// Forget every row, so a later test in the same process starts clean.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    jobs().clear();
    LAST_CONSIDERED.store(i64::MIN, Ordering::Relaxed);
    *LAST_RESCAN.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// A running test pump, stopped and JOINED by its own `Drop`.
///
/// `self_advance::SelfAdvanceHandle`'s stop-flag-plus-join shape, borrowed for
/// the one place it fits: the production pump parks forever and has nothing to
/// join, but a test's must be provably dead before the next test takes the
/// lease. Holding the handle for the whole test and letting it drop at the end
/// of scope is the contract — see [`spawn_pump_for_test`].
#[cfg(test)]
pub(crate) struct TestPumpHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
impl Drop for TestPumpHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// **The REAL pump body, on a joinable thread** — the test-only lifecycle the
/// `#[cfg(test)]` no-op [`start_pump_once`] leaves room for.
///
/// It runs [`pump_tick`] at [`RENDER_CACHE_PUMP_INTERVAL`], exactly as
/// [`pump_loop`] does, so a test that passes here is a test about what ships
/// and not about a stand-in.
///
/// **Callers MUST hold `lease()` for the whole life of the handle.** The lease
/// already serializes this module's globals and the shared encoder permit
/// against every other test in the binary; the handle's `Drop` then stops and
/// joins the thread before the next test can take that lease, so no pump ever
/// outlives the test that started it.
#[cfg(test)]
pub(crate) fn spawn_pump_for_test(dir: PathBuf) -> TestPumpHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let latch = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("rudis-render-cache-pump-test".to_string())
        .spawn(move || {
            while !latch.load(Ordering::Relaxed) {
                std::thread::sleep(RENDER_CACHE_PUMP_INTERVAL);
                pump_tick(&dir);
            }
        })
        .expect("spawn the test pump");
    TestPumpHandle {
        stop,
        thread: Some(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestAppCtx;
    use std::time::{Duration, Instant};

    // -----------------------------------------------------------------------
    // Harness
    // -----------------------------------------------------------------------

    /// Serializes every test in this module.
    ///
    /// [`JOBS`], [`HOST`], the detector's registry, the reader's configured
    /// directory and — critically — the SHARED encoder permit are all
    /// process-global, and `cargo test` runs this binary on as many threads as
    /// the machine has cores. One lease, taken at the top of each test, is the
    /// only correct shape; `proxy_job::encoder_lease`'s reasoning, same resource
    /// plus this module's own globals.
    static SCHEDULER: Mutex<()> = Mutex::new(());

    struct Lease {
        _scheduler: MutexGuard<'static, ()>,
        _encoder: MutexGuard<'static, ()>,
    }

    /// Take BOTH leases, always in this order, so this module's tests and
    /// `proxy_job`'s can never deadlock against each other.
    fn lease() -> Lease {
        let scheduler = SCHEDULER.lock().unwrap_or_else(|p| p.into_inner());
        let encoder = crate::proxy_job::encoder_lease();
        // SETTLE. A render thread from the previous test in this binary may
        // still be a few instructions from returning its permit; both leases
        // above are ours, but neither of them is the permit. Start from a pool
        // that is provably whole, or the first gate that takes it fails for the
        // neighbour's reason.
        assert!(
            wait_until(Duration::from_secs(90), || {
                crate::proxy_job::try_take_permit().is_some()
            }),
            "a previous test left the shared encoder permit taken"
        );
        reset_for_tests();
        preview::render_cache_detect::reset_for_tests();
        clear_render_host();
        Lease {
            _scheduler: scheduler,
            _encoder: encoder,
        }
    }

    /// A tauri-free [`PreviewHost`] over a store this test owns.
    ///
    /// The twin of `export.rs`'s `TestPreviewHost`, method for method, with the
    /// store behind an `Arc` because a background render is detached and needs a
    /// `'static` host — which is exactly why [`set_render_host`] exists.
    struct BgHost {
        store: Arc<crate::SharedStore>,
        mirror: preview::PlaybackMirror,
    }

    impl BgHost {
        fn empty() -> Arc<BgHost> {
            Arc::new(BgHost {
                store: Arc::new(Mutex::new(rudis_core::Store::default())),
                mirror: preview::PlaybackMirror::new(),
            })
        }
    }

    impl PreviewHost for BgHost {
        fn store(&self) -> Option<MutexGuard<'_, rudis_core::Store>> {
            self.store.lock().ok()
        }
        fn playback_mirror(&self) -> &preview::PlaybackMirror {
            &self.mirror
        }
        fn resolve_overlay(&self) -> Vec<(rudis_core::Annotation, f32)> {
            Vec::new()
        }
        fn live_gesture(&self) -> Vec<(f32, f32)> {
            Vec::new()
        }
        fn overlay_ink(&self) -> [u8; 4] {
            [0x4F, 0x8A, 0xFF, 0xFF]
        }
        fn draw_ink(
            &self,
            _frame: &mut engine::Frame,
            _annotations: &[rudis_core::Annotation],
            _ink: [u8; 4],
            _dashed: bool,
        ) {
        }
        fn rasterize_text(
            &self,
            text: &rudis_core::TextPayload,
            transform: engine::LayerTransform,
            opacity: f32,
            crop: engine::LayerCrop,
            project_w: u32,
            project_h: u32,
        ) -> engine::Layer {
            let mut rasterizer = engine::TextRasterizer::new();
            crate::compose::rasterize_text_layer(
                &mut rasterizer,
                text,
                transform,
                opacity,
                crop,
                project_w,
                project_h,
            )
        }
        fn emit_canvas_viewport(&self, _w: u32, _h: u32, _fw: u32, _fh: u32) {}
    }

    /// Mark `seg` heavy through the detector's ONLY inlet — the same call
    /// `ring.rs` makes on a live tick, at a cost the budget cannot accept.
    fn mark_heavy(seg: i64) {
        for _ in 0..preview::render_cache_detect::SEGMENT_MISS_STREAK {
            preview::render_cache_detect::note_live_tick(seg, 90_000, 33_333);
        }
    }

    /// Wait for a predicate, or give up. Returns whether it held.
    fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pred() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// This module's own source, for the structural scans.
    fn own_source() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/render_cache_job.rs");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("this module must be able to read itself ({path:?}): {e}"))
    }

    /// `src` with every `//` comment blanked, so a scan reads CODE and not
    /// prose.
    ///
    /// `dynres.rs` recorded the footgun this exists for — a module doc that
    /// mentions a forbidden identifier trips the scan describing the rule — and
    /// it bit 59-05 twice and 59-07 once more. Blanking is the fix that lets the
    /// rule stay explained where a reader will find it.
    fn comment_blanked(src: &str) -> String {
        src.lines()
            .map(|line| match line.find("//") {
                Some(at) => line[..at].to_string(),
                None => line.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A cache pair for `seg` under `hash`, written through the cache crate's
    /// OWN public write path — so what these gates plant is exactly what a real
    /// render commits.
    fn plant_segment(dir: &Path, seg: i64, hash: u64, canvas: (u32, u32, f64)) {
        std::fs::create_dir_all(dir).expect("cache dir");
        let stem = rendercache::cache::file_stem_for(hash, seg);
        let payload = dir.join(rendercache::cache::payload_file_name(&stem));
        std::fs::write(&payload, vec![0x5Au8; 2048]).expect("write the payload");
        let bytes = std::fs::metadata(&payload).expect("stat").len();
        let meta =
            rendercache::cache::SegmentMeta::new(seg, hash, canvas.0, canvas.1, canvas.2, bytes);
        assert!(
            rendercache::cache::write_meta(dir, &stem, &meta),
            "fix the fixture, not the cache: it refused to commit segment {seg}"
        );
    }

    fn segment_files_on_disk(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // -----------------------------------------------------------------------
    // VALIDATION row 27 / D-23 — ONE encoder admission, shared with the proxy
    // -----------------------------------------------------------------------

    /// **Row 27, both directions.**
    ///
    /// Half one: with the PROXY worker's permit held, a poll that has real work
    /// to do starts nothing at all — no row is registered, and the deferral is
    /// attributed to admission rather than to any of the other four reasons a
    /// poll can decline. Release the permit and the same poll starts the same
    /// segment.
    ///
    /// Half two — the one that makes this a claim about a SHARED semaphore
    /// rather than about a well-behaved sibling: while the render holds its
    /// permit, `proxy_job::try_take_permit` answers `None`. A second independent
    /// `Semaphore(1)` would pass half one and fail here.
    #[test]
    fn render_cache_job_shared_admission() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(7);

        // ---- half one: blocked ------------------------------------------
        let permit = crate::proxy_job::try_take_permit().expect("the idle pool has its permit");
        let deferrals_before = RENDER_CACHE_JOBS_DEFERRED_ADMISSION.load(Ordering::Relaxed);
        poll_and_spawn(&ctx);
        assert_eq!(
            tracked_row_count(),
            0,
            "with the PROXY worker holding the only encoder permit, a render \
             must not even be registered — it defers, it does not queue (D-23)"
        );
        assert_eq!(
            RENDER_CACHE_JOBS_DEFERRED_ADMISSION.load(Ordering::Relaxed) - deferrals_before,
            1,
            "and the deferral must be attributed to ADMISSION, not to the \
             playback guard, a permanent refusal, an exhausted retry budget or \
             an already-fresh segment — otherwise this gate would pass for a \
             poll that declined for some completely different reason"
        );
        drop(permit);

        // ---- half two: released, and the permit is provably THE SAME ------
        //
        // The gate is held first, so the render thread parks inside its work
        // while holding the permit. Without it the whole render (which fails
        // fast here — no cache dir content, no media) could be over before the
        // assertion below runs, and the gate would be a race.
        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().get(&7).map(|row| row.state) == Some(ROW_RUNNING)
            }),
            "with the permit free the same poll must start the same segment, \
             saw {:?}",
            jobs().get(&7).map(|row| row.state)
        );
        assert!(
            crate::proxy_job::try_take_permit().is_none(),
            "THE structural claim: while a render holds its admission, the \
             PROXY worker's own accessor must find the pool empty. A sibling \
             semaphore with the same bound would hand out a second permit here \
             and put two encoders on one piece of silicon"
        );
        drop(gate);

        assert!(
            wait_until(Duration::from_secs(90), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "the render must finish and release its permit"
        );
        assert!(
            crate::proxy_job::try_take_permit().is_some(),
            "and the shared pool must be whole again afterwards — a leaked \
             permit would zero BOTH workers for the life of the process"
        );

        // CLAUDE.md rule 3, and the reason "it started" is not the claim worth
        // making: the scheduler drove the REAL writer, through the REAL
        // encoder, onto a REAL file — and the SHIPPED read path can serve it.
        assert_eq!(
            jobs().get(&7).map(|row| row.state),
            Some(ROW_DONE),
            "the released arm must have COMMITTED, not merely started"
        );
        let host = render_host().expect("the host is still registered");
        let (canvas, edit_gen) = canvas_and_generation(host.as_ref()).expect("store");
        let hash = SegmentLookup::new()
            .segment_hash_memo(host.as_ref(), 7, edit_gen, canvas)
            .expect("hashable");
        let dir = render_cache_dir(&ctx).expect("dir");
        let hit = rendercache::cache::read_fresh_segment(
            &dir, 7, hash, canvas.0, canvas.1, canvas.2,
        )
        .expect(
            "the segment this scheduler committed must be servable by the \
             reader — writer and reader agreeing on identity is what makes the \
             whole cache work",
        );
        let bytes = std::fs::metadata(&hit.path).expect("stat the payload").len();
        eprintln!(
            "CACHE-JOB-COMMIT seg=7 path_exists={} {}x{}@{} bytes={bytes}",
            hit.path.is_file(),
            hit.w,
            hit.h,
            hit.fps
        );
        assert!(bytes > 0, "a committed payload has pixels in it");
    }

    /// The STRUCTURAL half of row 27, as a source scan: this module builds no
    /// permit pool of its own.
    ///
    /// The needle is assembled at runtime rather than written as a literal, for
    /// the reason the module doc gives about comment-blanking: a scan for a
    /// token cannot be written in the file it scans without matching itself, and
    /// this plan's acceptance gate greps this file for exactly that token
    /// expecting zero hits.
    #[test]
    fn render_cache_job_owns_no_permit_pool_of_its_own() {
        let needle = format!("{}::{}", "Semaphore", "new");
        let src = own_source();
        assert!(
            !comment_blanked(&src).contains(&needle),
            "this module must take its admission from `proxy_job::try_take_permit` \
             — the SAME process-wide static the proxy worker uses. Constructing \
             one here would be a second encoder bound, and D-23's whole point is \
             that there is only one encoder"
        );
        // Non-vacuity: the scanner CAN find things in this file.
        assert!(
            comment_blanked(&src).contains("try_take_permit"),
            "control: the shared accessor must be named in code, or the scan \
             above is passing over a file it cannot read"
        );
    }

    // -----------------------------------------------------------------------
    // VALIDATION row 10 / D-38 — the bounded re-scan
    // -----------------------------------------------------------------------

    /// **Row 10, both halves.**
    ///
    /// Four segments are warmed on disk: 0 and 10 stale, 5 CURRENT, 40 stale and
    /// far outside the window. The playhead is at segment 5 and a structural
    /// edit fires the re-scan.
    ///
    /// * WINDOW-ONLY DELETION: 0 and 10 are gone; 5 (still current) survives —
    ///   without that control the test would pass for a sweep that deletes
    ///   everything it can reach; and 40's two files are byte-present on disk
    ///   afterwards.
    /// * AND STILL NEVER SERVED STALE: the shipped read path is asked for 40
    ///   under the CURRENT identity and answers `None`. That is the half that
    ///   makes the window hygiene rather than truth — a reader that trusted
    ///   "the sweep did not delete it" would be serving a frame from an
    ///   arrangement that no longer exists.
    #[test]
    fn render_cache_job_rescan_window() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        let host = BgHost::empty();
        set_render_host(host.clone());
        let dir = render_cache_dir(&ctx).expect("cache dir");

        let (canvas, edit_gen) =
            canvas_and_generation(host.as_ref()).expect("the test host has a store");
        let mut lookup = SegmentLookup::new();
        let current = |k: i64, lookup: &mut SegmentLookup| -> u64 {
            lookup
                .segment_hash_memo(host.as_ref(), k, edit_gen, canvas)
                .expect("an empty arrangement still has an identity")
        };

        // Deliberately WRONG identities for 0, 10 and 40; the real one for 5.
        let stale = |k: i64, lookup: &mut SegmentLookup| current(k, lookup) ^ 0xDEAD_BEEF_DEAD_BEEF;
        let h0 = stale(0, &mut lookup);
        let h5 = current(5, &mut lookup);
        let h10 = stale(10, &mut lookup);
        let h40 = stale(40, &mut lookup);
        plant_segment(&dir, 0, h0, canvas);
        plant_segment(&dir, 5, h5, canvas);
        plant_segment(&dir, 10, h10, canvas);
        plant_segment(&dir, 40, h40, canvas);
        assert_eq!(
            segment_files_on_disk(&dir).len(),
            8,
            "four segments, two files each, before anything is swept"
        );

        // Segment 40 is 80 s of program away from the playhead — 5 times the
        // window, so a sweep that reaches it is not "slightly wide", it is
        // unbounded.
        assert!(
            (40i64 - 5).abs() > RESCAN_WINDOW_SEGMENTS,
            "the out-of-window fixture must actually be out of the window"
        );

        on_structural_edit(&ctx, 5);

        let after = segment_files_on_disk(&dir);
        let named = |hash: u64, seg: i64| rendercache::cache::file_stem_for(hash, seg);
        assert!(
            !after.iter().any(|n| n.starts_with(&named(h0, 0))),
            "a STALE segment inside the window is reclaimed: {after:?}"
        );
        assert!(
            !after.iter().any(|n| n.starts_with(&named(h10, 10))),
            "both of them: {after:?}"
        );
        assert_eq!(
            after
                .iter()
                .filter(|n| n.starts_with(&named(h5, 5)))
                .count(),
            2,
            "the CONTROL: a segment inside the window whose identity still \
             matches must be left alone. Without this the sweep could be \
             deleting everything it can reach and this test would not notice. \
             Saw {after:?}"
        );
        assert_eq!(
            after
                .iter()
                .filter(|n| n.starts_with(&named(h40, 40)))
                .count(),
            2,
            "and a segment OUTSIDE the window is not touched at all — D-38's \
             whole point is that this sweep is bounded. Saw {after:?}"
        );

        // The second half, and the reason the first half is only hygiene: the
        // SHIPPED read path still refuses segment 40, because the identity it
        // would have to match is not the one on disk.
        let expected_40 = current(40, &mut lookup);
        assert_ne!(
            expected_40, h40,
            "the fixture must really be stale, or the refusal below is vacuous"
        );
        assert!(
            rendercache::cache::read_fresh_segment(
                &dir,
                40,
                expected_40,
                canvas.0,
                canvas.1,
                canvas.2
            )
            .is_none(),
            "an un-swept stale segment must STILL never be served: correctness \
             is reactive (D-18), and this window is bytes-back hygiene rather \
             than the thing that keeps a stale frame off the screen"
        );
        // Non-vacuity for that refusal: the same read path DOES serve a segment
        // whose identity matches.
        assert!(
            rendercache::cache::read_fresh_segment(&dir, 5, h5, canvas.0, canvas.1, canvas.2)
                .is_some(),
            "control: the reader is capable of a hit, so the miss above is \
             about identity and not about a read path that never answers"
        );
    }

    /// The extent half of D-38: a caller that knows which range an edit touched
    /// gets that range swept too, even when it is nowhere near the playhead.
    #[test]
    fn render_cache_job_rescan_reaches_a_supplied_extent() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        let host = BgHost::empty();
        set_render_host(host.clone());
        let dir = render_cache_dir(&ctx).expect("cache dir");
        let (canvas, edit_gen) = canvas_and_generation(host.as_ref()).expect("store");
        let mut lookup = SegmentLookup::new();
        let stale_40 = lookup
            .segment_hash_memo(host.as_ref(), 40, edit_gen, canvas)
            .expect("hashable")
            ^ 0x1234_5678_9ABC_DEF0;
        plant_segment(&dir, 40, stale_40, canvas);

        // Segment 40 spans [80 s, 82 s) on the grid.
        on_structural_edit_over(&ctx, 5, (80_500_000, 81_500_000));

        assert!(
            segment_files_on_disk(&dir).is_empty(),
            "an edit that CARRIES its extent sweeps that extent as well as the \
             playhead window, saw {:?}",
            segment_files_on_disk(&dir)
        );
    }

    // -----------------------------------------------------------------------
    // 59-REVIEW WR-05 — D-38's re-scan has a PRODUCTION trigger
    // -----------------------------------------------------------------------

    /// **WR-05.** A real edit, through the real dispatch funnel, reclaims the
    /// orphaned bytes D-38's module doc says it reclaims.
    ///
    /// `on_structural_edit` shipped fully implemented and fully tested with no
    /// caller outside this file's own test module. Three things were therefore
    /// false in the tree at once: the doc's present-tense reclaim,
    /// [`RENDER_CACHE_RESCAN_DELETED`] as an observable that could ever move,
    /// and [`RESCAN_WINDOW_SEGMENTS`] as live configuration. This gate is what
    /// makes them true, and it drives `dispatch_command_inner` rather than
    /// `on_structural_edit` — calling the sweep directly is what the existing
    /// row-10 gate already does, and it is exactly the thing that passed for a
    /// year with nothing wired.
    #[test]
    fn render_cache_job_a_real_dispatch_triggers_the_rescan() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        let host = BgHost::empty();
        set_render_host(host.clone());
        let dir = render_cache_dir(&ctx).expect("cache dir");
        let (canvas, edit_gen) = canvas_and_generation(host.as_ref()).expect("store");

        // A segment at the playhead whose identity no longer matches anything —
        // an orphan of an arrangement that no longer exists.
        let mut lookup = SegmentLookup::new();
        let orphan = lookup
            .segment_hash_memo(host.as_ref(), 0, edit_gen, canvas)
            .expect("hashable")
            ^ 0xC0FF_EE00_C0FF_EE00;
        plant_segment(&dir, 0, orphan, canvas);
        assert_eq!(
            segment_files_on_disk(&dir).len(),
            2,
            "fixture: one segment, two files, before the edit"
        );
        assert_eq!(
            playing_playhead_segment(&ctx),
            None,
            "the fixture is paused, so the sweep's window is centred on segment \
             0 — where the orphan is"
        );

        let deleted_before = RENDER_CACHE_RESCAN_DELETED.load(Ordering::Relaxed);

        // THE REAL FUNNEL. `AddTrack` is a structural edit that needs no media,
        // and `patch_touches_preview` admits it.
        crate::test_support::dispatch_a_structural_edit(&ctx)
            .expect("the dispatch must apply");

        assert!(
            segment_files_on_disk(&dir).is_empty(),
            "a structural edit must reclaim the orphaned bytes around the \
             playhead — that is what D-38's module doc says happens, and until \
             this wiring landed nothing did it at all. Saw {:?}",
            segment_files_on_disk(&dir)
        );
        assert!(
            RENDER_CACHE_RESCAN_DELETED.load(Ordering::Relaxed) > deleted_before,
            "and the counter that attributes it must move: a permanently-zero \
             observable is indistinguishable from a mechanism that does not run"
        );
    }

    /// The sweep is THROTTLED, so an edit storm cannot put ~17 key-material
    /// lookups on every one of a drag's dispatches.
    ///
    /// Deterministic rather than timing-dependent: both edits below happen well
    /// inside [`RESCAN_MIN_INTERVAL`], so the second must find the slot taken
    /// whatever the machine is doing.
    #[test]
    fn render_cache_job_the_rescan_is_throttled() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        let host = BgHost::empty();
        set_render_host(host.clone());
        let dir = render_cache_dir(&ctx).expect("cache dir");
        let (canvas, edit_gen) = canvas_and_generation(host.as_ref()).expect("store");
        let mut lookup = SegmentLookup::new();
        let orphan = |k: i64, lookup: &mut SegmentLookup| {
            lookup
                .segment_hash_memo(host.as_ref(), k, edit_gen, canvas)
                .expect("hashable")
                ^ 0x0BAD_0BAD_0BAD_0BAD
        };

        let add_track = |ctx: &TestAppCtx| {
            crate::test_support::dispatch_a_structural_edit(ctx).expect("dispatch");
        };

        // First edit: the slot is free, the orphan goes.
        let h0 = orphan(0, &mut lookup);
        plant_segment(&dir, 0, h0, canvas);
        add_track(&ctx);
        assert!(
            segment_files_on_disk(&dir).is_empty(),
            "control: the first edit inside the interval must sweep, or the \
             second's silence below proves nothing"
        );

        // Second edit, immediately: the slot is taken and NOTHING is swept.
        let h1 = orphan(1, &mut lookup);
        plant_segment(&dir, 1, h1, canvas);
        let deleted_before = RENDER_CACHE_RESCAN_DELETED.load(Ordering::Relaxed);
        add_track(&ctx);
        assert_eq!(
            segment_files_on_disk(&dir).len(),
            2,
            "a second edit within {RESCAN_MIN_INTERVAL:?} must not run the \
             sweep again: it is ~17 key-material lookups (~37 ms on a 6-layer \
             arrangement) and a drag dispatches dozens of times a second. Saw \
             {:?}",
            segment_files_on_disk(&dir)
        );
        assert_eq!(
            RENDER_CACHE_RESCAN_DELETED.load(Ordering::Relaxed),
            deleted_before,
            "and nothing was attributed to it either"
        );

        // …and the skipped sweep costs correctness NOTHING, which is the whole
        // reason a throttle is acceptable here: the reader still refuses the
        // un-swept orphan under the current identity (D-18).
        let current_1 = lookup
            .segment_hash_memo(host.as_ref(), 1, canvas_and_generation(host.as_ref()).expect("store").1, canvas)
            .expect("hashable");
        assert_ne!(current_1, h1, "the fixture must really be an orphan");
        assert!(
            rendercache::cache::read_fresh_segment(
                &dir, 1, current_1, canvas.0, canvas.1, canvas.2
            )
            .is_none(),
            "an un-swept orphan must STILL never be served — D-38 is hygiene \
             and D-18 is the truth, and that split is exactly what makes \
             throttling the sweep a free choice"
        );
    }

    // -----------------------------------------------------------------------
    // D-24 — the playback guard
    // -----------------------------------------------------------------------

    /// While the timeline is PLAYING, segments within
    /// [`PLAYBACK_GUARD_SEGMENTS`] of the playhead are skipped and one outside
    /// it is taken.
    ///
    /// Both halves in one gate on purpose: "it skipped everything" and "it
    /// skipped the right thing" are different claims, and only the second is
    /// D-24.
    #[test]
    fn render_cache_job_playback_guard() {
        let _lease = lease();
        // Playing at 21 s on a 120 s timeline -> the playhead is in segment 10.
        // Built as a whole `Project` and handed to `Store::from_project`, which
        // is `test_support`'s own documented way to seed a ctx: transport
        // commands clamp to the timeline's duration, so a Seek on an empty
        // project could never put the playhead anywhere but zero.
        let mut project = rudis_core::Project::default();
        project.playback.playing = true;
        project.playback.position_us = 21_000_000;
        project.playback.duration_us = 120_000_000;
        project.playback.fps = 30.0;
        let ctx = TestAppCtx::with_store(Mutex::new(rudis_core::Store::from_project(project)));
        set_render_host(BgHost::empty());

        assert_eq!(
            playing_playhead_segment(&ctx),
            Some(10),
            "the fixture must put the playhead where the guard is being tested"
        );

        mark_heavy(10); // under the playhead
        mark_heavy(11); // one segment away  -> inside the guard
        mark_heavy(30); // far away          -> takeable

        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().values().any(JobEntry::is_live)
            }),
            "a candidate outside the guard must still be rendered — D-24 defers \
             the section the user is watching, it does not stop the cache"
        );
        let live: Vec<i64> = jobs()
            .iter()
            .filter(|(_, row)| row.is_live())
            .map(|(seg, _)| *seg)
            .collect();
        assert_eq!(
            live,
            vec![30],
            "and it must be the FAR segment: 10 is under the playhead and 11 is \
             within +-{PLAYBACK_GUARD_SEGMENTS} of it, so rendering either \
             would put a second decode pool and an encoder child against the \
             very frames the cache exists to protect"
        );
        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(30), || {
            !jobs().values().any(JobEntry::is_live)
        });
    }

    /// The guard's OFF state, so the test above is not passing for a scheduler
    /// that always picks the highest segment: paused, the segment under the
    /// playhead is taken first.
    #[test]
    fn render_cache_job_takes_the_playhead_segment_when_paused() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        assert_eq!(
            playing_playhead_segment(&ctx),
            None,
            "a store that has never played must impose no guard"
        );
        mark_heavy(10);
        mark_heavy(30);

        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().values().any(JobEntry::is_live)
            }),
            "a paused editor is exactly when a background render should run"
        );
        let live: Vec<i64> = jobs()
            .iter()
            .filter(|(_, row)| row.is_live())
            .map(|(seg, _)| *seg)
            .collect();
        assert_eq!(
            live,
            vec![10],
            "with no guard the lowest heavy segment is taken first"
        );
        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(30), || {
            !jobs().values().any(JobEntry::is_live)
        });
    }

    // -----------------------------------------------------------------------
    // 59-REVIEW WR-01 — what a poll costs the transport funnel
    // -----------------------------------------------------------------------

    /// **WR-01.** One poll performs AT MOST ONE key-material walk, and a warm
    /// steady state performs none.
    ///
    /// The counter is `preview`'s own `RENDER_CACHE_HASH_WALKS`, whose whole
    /// purpose is to say when a memo has been defeated. It is the right
    /// instrument here for the same reason: nothing about the SCHEDULER's
    /// decisions changes with this fix — the same segment is chosen and the same
    /// render is (not) started — so the only observable that can tell the fix
    /// from the bug is what the poll paid to decide.
    ///
    /// The cost being bounded matters because `poll_and_spawn` runs on
    /// [`crate::transport::run_transport`], synchronously, on the caller's
    /// thread across the C ABI, for every `Play`, `Pause`, `Seek`, `Step` and
    /// `SetLooping`. Before this fix the STEADY STATE was the worst case: every
    /// candidate warm meant every candidate reached the walk, and the poll
    /// returned having done nothing at all — ~2 ms per candidate on a 6-layer
    /// arrangement, up to [`HEAVY_SEGMENT_POLL_LIMIT`] of them, so ~140 ms per
    /// transport command and a visibly stalled scrub drag.
    #[test]
    fn render_cache_job_hashes_at_most_one_segment_per_poll() {
        use preview::render_cache_lookup::RENDER_CACHE_HASH_WALKS;

        let _lease = lease();
        let ctx = TestAppCtx::new();
        let host = BgHost::empty();
        set_render_host(host.clone());
        let dir = render_cache_dir(&ctx).expect("cache dir");
        let (canvas, edit_gen) = canvas_and_generation(host.as_ref()).expect("store");

        /// Enough candidates that "one walk" and "one walk per candidate" are
        /// an order of magnitude apart, and small enough to converge inside one
        /// test.
        const CANDIDATES: i64 = 10;

        let mut lookup = SegmentLookup::new();
        for k in 0..CANDIDATES {
            mark_heavy(k);
            let hash = lookup
                .segment_hash_memo(host.as_ref(), k, edit_gen, canvas)
                .expect("an empty arrangement still has an identity");
            plant_segment(&dir, k, hash, canvas);
            // The CONTROL, per segment: the SHIPPED read path really does serve
            // what was planted. Without it "the poll found nothing to start"
            // would be true for a fixture the reader refuses.
            assert!(
                rendercache::cache::read_fresh_segment(
                    &dir, k, hash, canvas.0, canvas.1, canvas.2
                )
                .is_some(),
                "fixture: segment {k} must be readable under the CURRENT \
                 identity, or this gate measures a cold cache rather than a \
                 warm one"
            );
        }

        // ---- one poll, one walk ------------------------------------------
        let walks0 = RENDER_CACHE_HASH_WALKS.load(Ordering::Relaxed);
        let spawned0 = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        poll_and_spawn(&ctx);
        assert_eq!(
            RENDER_CACHE_HASH_WALKS.load(Ordering::Relaxed) - walks0,
            1,
            "ONE poll assembled key material more than once. Every assembly is \
             a store-lock walk plus a canonicalize + metadata + decode-source \
             resolve per visible clip (59-05: 368 us for one clip), and this \
             runs synchronously on the transport funnel — 59-REVIEW WR-01."
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed),
            spawned0,
            "and nothing was started: every candidate is already on disk"
        );

        // ---- converge: one candidate confirmed per poll -------------------
        for _ in 0..CANDIDATES {
            poll_and_spawn(&ctx);
        }
        assert_eq!(
            RENDER_CACHE_HASH_WALKS.load(Ordering::Relaxed) - walks0,
            CANDIDATES as u64,
            "{CANDIDATES} candidates must cost {CANDIDATES} walks in total, \
             never {CANDIDATES} per poll"
        );

        // ---- the steady state is FREE ------------------------------------
        let walks1 = RENDER_CACHE_HASH_WALKS.load(Ordering::Relaxed);
        for _ in 0..50 {
            poll_and_spawn(&ctx);
        }
        assert_eq!(
            RENDER_CACHE_HASH_WALKS.load(Ordering::Relaxed) - walks1,
            0,
            "once every candidate has been confirmed warm, a poll must cost \
             nothing but map probes. This is the case the transport funnel is \
             in for most of a session, and it was previously the WORST case."
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed),
            spawned0,
            "still nothing started — the poll declining cheaply must not have \
             become the poll declining for a different reason"
        );

        // ---- and the remembered conclusion is NOT an authority ------------
        //
        // Deleting the files must put the work back, because the registry's
        // memory is keyed on the material generation and D-25 lets a user
        // delete the cache at any moment. The generation has not moved, so this
        // is the honest limit of the optimisation rather than a hidden
        // guarantee: what brings the work back here is the row eviction, and
        // what brings it back in production is the next edit.
        for k in 0..CANDIDATES {
            jobs().remove(&k);
        }
        let gate = hold_render_gate();
        for k in 0..CANDIDATES {
            let hash = lookup
                .segment_hash_memo(host.as_ref(), k, edit_gen, canvas)
                .expect("hashable");
            let stem = rendercache::cache::file_stem_for(hash, k);
            let _ = std::fs::remove_file(dir.join(rendercache::cache::meta_file_name(&stem)));
            let _ = std::fs::remove_file(dir.join(rendercache::cache::payload_file_name(&stem)));
        }
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().values().any(JobEntry::is_live)
            }),
            "with the segments gone from disk the poll must start a render \
             again — the cheap skip is a cache of a conclusion, not a claim that \
             the segment can never need work"
        );
        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(30), || {
            !jobs().values().any(JobEntry::is_live)
        });
    }

    /// **59-REVIEW WR-04.** A candidate past the lowest
    /// [`HEAVY_SEGMENT_POLL_LIMIT`] is reachable.
    ///
    /// The detector sorts its candidates and the old poll truncated that sorted
    /// list, so the window was a fixed prefix: heat marks are sticky by design,
    /// so once the lowest 64 were all accounted for, every poll walked the same
    /// 64 and returned — and candidates 65+ were never scheduled, ever. 64
    /// segments is 128 s of program at the 2 s pitch, against a detector
    /// deliberately built to hold ~2.3 hours of marks.
    ///
    /// The fixture makes the lowest 65 ineligible through the registry rather
    /// than by rendering them, because 65 real segment renders would be a minute
    /// of encoder time to establish a precondition. What is under test is which
    /// candidates the WINDOW can see, and the segment it lands on is then started
    /// through the real writer.
    #[test]
    fn render_cache_job_reaches_a_candidate_past_the_poll_limit() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());

        /// One past the window, so `FIRST_ELIGIBLE` is unreachable by any
        /// prefix of length [`HEAVY_SEGMENT_POLL_LIMIT`].
        const FIRST_ELIGIBLE: i64 = HEAVY_SEGMENT_POLL_LIMIT as i64 + 1;
        const MARKED: i64 = FIRST_ELIGIBLE + 5;

        for k in 0..MARKED {
            mark_heavy(k);
        }
        // Everything below FIRST_ELIGIBLE is permanently refused, so the only
        // segment a poll may take is out of the prefix's reach.
        {
            let mut jobs = jobs();
            for k in 0..FIRST_ELIGIBLE {
                jobs.insert(
                    k,
                    JobEntry {
                        epoch: ROW_EPOCH.fetch_add(1, Ordering::Relaxed),
                        state: ROW_REFUSED,
                        cancel: Arc::new(AtomicBool::new(false)),
                        attempts: 0,
                        external_aborts: 0,
                        fresh_at: None,
                    },
                );
            }
        }
        assert!(
            !preview::render_cache_detect::heavy_segments(HEAVY_SEGMENT_POLL_LIMIT)
                .contains(&FIRST_ELIGIBLE),
            "fixture: the only eligible segment must be OUTSIDE the truncating \
             form's answer, or this gate would pass without any rotation"
        );

        let gate = hold_render_gate();
        // Two polls: the first sweeps the refused prefix and advances the
        // window, the second reaches past it. A scheduler that could never
        // advance would spin on the prefix forever, however many times it is
        // polled.
        let mut live: Vec<i64> = Vec::new();
        for _ in 0..8 {
            poll_and_spawn(&ctx);
            live = jobs()
                .iter()
                .filter(|(_, row)| row.is_live())
                .map(|(seg, _)| *seg)
                .collect();
            if !live.is_empty() {
                break;
            }
        }
        assert_eq!(
            live,
            vec![FIRST_ELIGIBLE],
            "a candidate past the lowest {HEAVY_SEGMENT_POLL_LIMIT} must be \
             reachable. Truncating a SORTED candidate list is a permanent \
             prefix filter, not sampling, and the detector holds far more marks \
             than the prefix can cover (59-REVIEW WR-04)."
        );

        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(60), || {
            !jobs().values().any(JobEntry::is_live)
        });
    }

    // -----------------------------------------------------------------------
    // 59-REVIEW WR-06 — the registries do not survive a project switch
    // -----------------------------------------------------------------------

    /// **WR-06.** Creating or opening a project forgets both process-global
    /// registries.
    ///
    /// Every key in both of them is a SEGMENT INDEX — a coordinate on a global
    /// program-time grid — so a mark or a row from project A says nothing about
    /// project B. Before this fix the two entry points raised the cancel latches
    /// and nothing else, so segments marked heavy in A were scheduled in B, a
    /// segment permanently refused in A could never be cached in B, and A's
    /// attempt budgets applied to B.
    ///
    /// Driven through the REAL `run_new_project` / `run_open_project`, because
    /// the claim is that those two entry points do it — calling the reset
    /// functions directly would prove only that they clear a map.
    #[test]
    fn render_cache_job_a_project_switch_forgets_both_registries() {
        use crate::project::{run_new_project, run_open_project};

        let _lease = lease();
        let ctx = TestAppCtx::new();

        // A registry that has learned things about "yesterday's" project: a
        // heavy mark, and a row that says a segment index can never be cached.
        let seed = || {
            mark_heavy(31);
            jobs().insert(
                31,
                JobEntry {
                    epoch: ROW_EPOCH.fetch_add(1, Ordering::Relaxed),
                    state: ROW_REFUSED,
                    cancel: Arc::new(AtomicBool::new(false)),
                    attempts: MAX_SEGMENT_ATTEMPTS,
                    external_aborts: 0,
                    fresh_at: None,
                },
            );
            assert_eq!(
                preview::render_cache_detect::heavy_segments(64),
                vec![31],
                "fixture: the heat registry must actually hold the mark"
            );
            assert_eq!(tracked_row_count(), 1, "fixture: and the row registry the row");
        };

        seed();
        run_new_project(&ctx, &serde_json::json!({ "name": "wr06-today" }))
            .expect("new_project applies");
        assert!(
            preview::render_cache_detect::heavy_segments(64).is_empty(),
            "creating a project must forget the previous one's heat: segment \
             index 31 is 62 s into a program that no longer exists, and \
             rendering it spends the SHARED encoder permit and a whole decode \
             pool on a range nobody measured"
        );
        assert_eq!(
            tracked_row_count(),
            0,
            "and its rows: a segment index permanently refused in one project \
             would otherwise be uncacheable in every later one, silently, for \
             the life of the process"
        );

        seed();
        run_new_project(&ctx, &serde_json::json!({ "name": "wr06-yesterday" }))
            .expect("a second project to switch back to");
        seed();
        run_open_project(&ctx, &serde_json::json!({ "name": "wr06-today" }))
            .expect("open_project applies");
        assert!(
            preview::render_cache_detect::heavy_segments(64).is_empty()
                && tracked_row_count() == 0,
            "OPENING a project must do the same as creating one — both entry \
             points already stand beside the same cancel, and a fix applied to \
             one of the two is the half nobody notices is missing"
        );
    }

    // -----------------------------------------------------------------------
    // Cancellation (D-24's caller list)
    // -----------------------------------------------------------------------

    /// [`request_cancel_all`] raises the latch the in-flight render is actually
    /// watching — the same `Arc` 59-07's writer polls before every tick — and
    /// the render ends without committing anything.
    #[test]
    fn render_cache_job_cancel_all_raises_the_live_latch() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(3);

        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().get(&3).map(|row| row.state) == Some(ROW_RUNNING)
            }),
            "the render must be in flight before the latch is raised, or this \
             gate cancels a job that never started"
        );
        let latch = jobs()
            .get(&3)
            .map(|row| Arc::clone(&row.cancel))
            .expect("the live row owns a latch");
        assert!(!latch.load(Ordering::Relaxed), "not cancelled yet");

        request_cancel_all();
        assert!(
            latch.load(Ordering::Relaxed),
            "cancel_all must reach the latch the RUNNING render holds — a latch \
             raised on some other Arc is a flag nobody reads"
        );
        drop(gate);

        assert!(
            wait_until(Duration::from_secs(30), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "and the render must end"
        );
        assert!(
            segment_files_on_disk(&render_cache_dir(&ctx).expect("dir")).is_empty(),
            "with nothing committed, 59-04's kill-and-reap leaves no residue: {:?}",
            segment_files_on_disk(&render_cache_dir(&ctx).expect("dir"))
        );
    }

    /// Cancelling with nothing in flight is a no-op, never a panic — the
    /// dispatch peek and both project entry points call this unconditionally.
    #[test]
    fn render_cache_job_cancel_all_on_an_empty_registry_is_a_no_op() {
        let _lease = lease();
        request_cancel_all();
        clear_render_host();
        request_cancel_all();
    }

    // -----------------------------------------------------------------------
    // The refusal split (59-07's NotCacheable vs Incomplete)
    // -----------------------------------------------------------------------

    /// A PERMANENTLY refused segment is never re-enqueued; a transiently refused
    /// one is, up to [`MAX_SEGMENT_ATTEMPTS`].
    ///
    /// Registry-level, deliberately: driving a real `NotCacheable` needs a
    /// degenerate single-layer arrangement (59-07's `deferred-items` D-7 records
    /// that it is currently unreachable-by-luck through the live path), and the
    /// thing that would livelock the worker is this branch, not that
    /// arrangement.
    #[test]
    fn render_cache_job_never_re_enqueues_a_permanent_refusal() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(12);

        let insert = |state: &'static str, attempts: u32| {
            jobs().insert(
                12,
                JobEntry {
                    epoch: ROW_EPOCH.fetch_add(1, Ordering::Relaxed),
                    state,
                    cancel: Arc::new(AtomicBool::new(false)),
                    attempts,
                    external_aborts: 0,
                    fresh_at: None,
                },
            );
        };

        insert(ROW_REFUSED, 1);
        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        poll_and_spawn(&ctx);
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed),
            spawned_before,
            "a segment 59-07's writer called NotCacheable is a property of the \
             ARRANGEMENT: re-enqueueing it is an infinite retry on media that \
             can never be cached"
        );

        insert(ROW_RETRY, MAX_SEGMENT_ATTEMPTS);
        poll_and_spawn(&ctx);
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed),
            spawned_before,
            "and a transient refusal still stops at MAX_SEGMENT_ATTEMPTS — an \
             edit storm must not be able to spin this worker forever"
        );

        // The control: one attempt short of the budget, it goes again.
        insert(ROW_RETRY, MAX_SEGMENT_ATTEMPTS - 1);
        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) > spawned_before
            }),
            "control: a TRANSIENT refusal inside its budget must be retried, or \
             the two assertions above would pass for a scheduler that never \
             starts anything"
        );
        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(30), || {
            !jobs().values().any(JobEntry::is_live)
        });
    }

    /// **59-REVIEW WR-03.** A render abandoned because the WORLD moved does not
    /// spend the segment's retry budget.
    ///
    /// The bug this regresses: `Cancelled` and `StaleAborted` shared one budget
    /// of three with `Incomplete` and `Failed`, and `attempts` was never reset.
    /// `StaleAborted` is raised by the writer on EVERY store mutation during a
    /// ~1.3 s render, and `Cancelled` comes from project open / project new /
    /// media removal / shutdown. So three edits while the background worker was
    /// warming a heavy section permanently disabled caching for that segment for
    /// the rest of the process — the exact opposite of the intent, since the
    /// sections a user is actively working on are the ones the cache should keep
    /// retrying hardest.
    ///
    /// Driven through the REAL writer and the REAL latch, more times than
    /// [`MAX_SEGMENT_ATTEMPTS`] allows, because the claim is about what the
    /// scheduler does after a genuine cancellation and a hand-written row would
    /// prove only that this test can set a field.
    #[test]
    fn render_cache_job_an_external_cancel_does_not_spend_the_retry_budget() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(21);

        // MORE cycles than the budget allows. Under the old accounting the
        // third one was the last this segment ever got.
        let cycles = MAX_SEGMENT_ATTEMPTS + 1;
        let committed_before = RENDER_CACHE_SEGMENTS_COMMITTED.load(Ordering::Relaxed);
        for cycle in 0..cycles {
            let gate = hold_render_gate();
            poll_and_spawn(&ctx);
            assert!(
                wait_until(Duration::from_secs(20), || {
                    jobs().get(&21).map(|row| row.state) == Some(ROW_RUNNING)
                }),
                "cycle {cycle}: the segment must still be scheduled after \
                 {cycle} external cancellations. Three user actions must not \
                 permanently stop a section from ever being cached (59-REVIEW \
                 WR-03). Row: {:?}",
                jobs()
                    .get(&21)
                    .map(|row| (row.state, row.attempts, row.external_aborts))
            );

            // The REAL latch, raised by the REAL production entry point — the
            // one project open, project new, media removal and shutdown all
            // call.
            request_cancel_all();
            drop(gate);
            assert!(
                wait_until(Duration::from_secs(60), || {
                    !jobs().values().any(JobEntry::is_live)
                }),
                "cycle {cycle}: the cancelled render must end"
            );

            assert_eq!(
                jobs()
                    .get(&21)
                    .map(|row| (row.state, row.attempts, row.external_aborts)),
                Some((ROW_RETRY, 0, cycle + 1)),
                "cycle {cycle}: an externally cancelled render must REFUND its \
                 attempt and charge the separate external budget instead"
            );
        }

        // Nothing was committed by any of them — so the eligibility above is
        // genuinely "still retrying", not "already done".
        assert!(
            segment_files_on_disk(&render_cache_dir(&ctx).expect("dir")).is_empty(),
            "no cancelled render may leave a committed segment behind: {:?}",
            segment_files_on_disk(&render_cache_dir(&ctx).expect("dir"))
        );
        // Phase 71 (TRUST-03): and none of them moved the commit counter the
        // shell shows as progress — a cancel is not work done.
        assert_eq!(
            RENDER_CACHE_SEGMENTS_COMMITTED.load(Ordering::Relaxed) - committed_before,
            0,
            "a cancelled render must never bump RENDER_CACHE_SEGMENTS_COMMITTED"
        );
    }

    /// The other half of WR-03, and the reason the refund is bounded rather than
    /// free: T-59-08-02 still terminates.
    ///
    /// Registry-level for `render_cache_job_never_re_enqueues_a_permanent_refusal`'s
    /// stated reason — driving 32 real 1.3 s renders to their cancellation would
    /// be a minute of encoder time to assert one comparison.
    #[test]
    fn render_cache_job_the_external_abort_budget_is_finite() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(22);

        jobs().insert(
            22,
            JobEntry {
                epoch: ROW_EPOCH.fetch_add(1, Ordering::Relaxed),
                state: ROW_RETRY,
                cancel: Arc::new(AtomicBool::new(false)),
                attempts: 0,
                external_aborts: MAX_EXTERNAL_ABORTS,
                fresh_at: None,
            },
        );
        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        poll_and_spawn(&ctx);
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed),
            spawned_before,
            "the refund is generous, not unlimited: an edit storm against a \
             segment that can never finish must still terminate (T-59-08-02)"
        );

        // The control, one short of the bound.
        jobs().insert(
            22,
            JobEntry {
                epoch: ROW_EPOCH.fetch_add(1, Ordering::Relaxed),
                state: ROW_RETRY,
                cancel: Arc::new(AtomicBool::new(false)),
                attempts: 0,
                external_aborts: MAX_EXTERNAL_ABORTS - 1,
                fresh_at: None,
            },
        );
        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(20), || {
                RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) > spawned_before
            }),
            "control: inside the external budget the segment is still \
             scheduled, or the assertion above would pass for a scheduler that \
             never starts anything"
        );
        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(60), || {
            !jobs().values().any(JobEntry::is_live)
        });
    }

    /// A COMMITTED render clears both budgets.
    ///
    /// Without this, a segment re-invalidated by three successful edits would
    /// exhaust a budget it never actually spent: `attempts` was incremented on
    /// every enqueue and never reset, so three successful renders — three
    /// successes — locked the segment out exactly as three failures did.
    #[test]
    fn render_cache_job_a_commit_clears_the_budgets() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(23);

        // Start from a row that has already spent most of both budgets.
        jobs().insert(
            23,
            JobEntry {
                epoch: ROW_EPOCH.fetch_add(1, Ordering::Relaxed),
                state: ROW_RETRY,
                cancel: Arc::new(AtomicBool::new(false)),
                attempts: MAX_SEGMENT_ATTEMPTS - 1,
                external_aborts: MAX_EXTERNAL_ABORTS - 1,
                fresh_at: None,
            },
        );

        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(90), || {
                jobs().get(&23).map(|row| row.state) == Some(ROW_DONE)
            }),
            "the render must COMMIT for this gate to mean anything, saw {:?}",
            jobs().get(&23).map(|row| row.state)
        );
        assert_eq!(
            jobs()
                .get(&23)
                .map(|row| (row.attempts, row.external_aborts)),
            Some((0, 0)),
            "a segment that just rendered successfully has no bad luck left to \
             remember — and a budget that survives success is a budget three \
             EDITS can exhaust on a segment that never once failed"
        );
    }

    /// A live row cannot outlive its thread (58-REVIEW WR-04, one subject over).
    ///
    /// Here the consequence is worse than proxy's: [`poll_and_spawn`] refuses to
    /// start ANY render while a row is live, so one stranded row would stop the
    /// render cache for the whole process.
    #[test]
    fn render_cache_job_a_stranded_live_row_is_demoted_not_left_live() {
        let _lease = lease();
        jobs().insert(
            9,
            JobEntry {
                epoch: 4242,
                state: ROW_RUNNING,
                cancel: Arc::new(AtomicBool::new(false)),
                attempts: 1,
                external_aborts: 0,
                fresh_at: None,
            },
        );

        drop(RowGuard {
            seg: 9,
            epoch: 4242,
        });
        assert_eq!(
            jobs().get(&9).map(|row| row.state),
            Some(ROW_RETRY),
            "a stranded live row must be demoted so the worker is not blocked \
             forever — and demoted rather than REMOVED, so the attempt budget \
             survives a render that panics reproducibly"
        );
        assert_eq!(
            jobs().get(&9).map(|row| row.attempts),
            Some(1),
            "the budget survives"
        );

        // WR-04's epoch: a stale guard must not touch a successor's row.
        jobs().insert(
            9,
            JobEntry {
                epoch: 4243,
                state: ROW_RUNNING,
                cancel: Arc::new(AtomicBool::new(false)),
                attempts: 2,
                external_aborts: 0,
                fresh_at: None,
            },
        );
        drop(RowGuard {
            seg: 9,
            epoch: 4242,
        });
        assert_eq!(
            jobs().get(&9).map(|row| row.state),
            Some(ROW_RUNNING),
            "a stale guard must not demote its successor's live row"
        );
        set_state(9, 4242, ROW_DONE);
        assert_eq!(
            jobs().get(&9).map(|row| row.state),
            Some(ROW_RUNNING),
            "and a stale write must not overwrite it either"
        );
    }

    /// The eviction sweep keeps live rows AND permanent refusals, and forgets
    /// the rest — the property that makes the cap safe.
    #[test]
    fn render_cache_job_the_registry_sweep_keeps_what_it_must() {
        let mut map: HashMap<i64, JobEntry> = HashMap::new();
        for i in 0..MAX_TRACKED_RENDER_CACHE_JOBS as i64 {
            let state = match i % 3 {
                0 => ROW_DONE,
                1 => ROW_RUNNING,
                _ => ROW_REFUSED,
            };
            map.insert(
                i,
                JobEntry {
                    epoch: i as u64,
                    state,
                    cancel: Arc::new(AtomicBool::new(false)),
                    attempts: 1,
                    external_aborts: 0,
                    fresh_at: None,
                },
            );
        }
        evict_finished_if_full(&mut map);
        assert!(
            map.values().all(|r| r.is_live() || r.is_permanent()),
            "only live rows and permanent refusals survive"
        );
        assert!(
            map.values().any(JobEntry::is_permanent),
            "and the permanent refusals really are among them — forgetting one \
             re-opens the infinite-retry hole"
        );

        let mut small: HashMap<i64, JobEntry> = HashMap::new();
        small.insert(
            0,
            JobEntry {
                epoch: 0,
                state: ROW_DONE,
                cancel: Arc::new(AtomicBool::new(false)),
                attempts: 1,
                external_aborts: 0,
                fresh_at: None,
            },
        );
        evict_finished_if_full(&mut small);
        assert_eq!(small.len(), 1, "an under-full registry is left alone");
    }

    // -----------------------------------------------------------------------
    // D-26 — engine state, and undo cannot reach it
    // -----------------------------------------------------------------------

    /// This module's CODE names no domain-mutation type.
    ///
    /// Both needles are assembled at runtime and the source is comment-blanked
    /// before the scan, because the module doc has to be able to state the rule
    /// using the very names the rule forbids — `dynres.rs`'s recorded footgun,
    /// which has now bitten five plans across two phases.
    #[test]
    fn render_cache_job_never_names_a_mutation_type() {
        let code = comment_blanked(&own_source());
        for needle in [format!("{}{}", "Patch", "Kind"), format!("{}::", "Command")] {
            assert!(
                !code.contains(&needle),
                "D-26: cache state is ENGINE state. A scheduler that took or \
                 produced `{needle}` would be one refactor away from putting a \
                 cache entry somewhere undo can resurrect it"
            );
        }
        // Non-vacuity, and the positive statement of the same rule: the module
        // DOES name the store, which is where it reads (never writes) domain
        // state.
        assert!(
            code.contains("ctx.store()"),
            "control: the scan can see this file's code"
        );
    }

    /// **The standing CACHE-05 precondition, as a durable scan.** Exactly one
    /// file in `crates/app-core/src` may name the cache leaf crate: THIS one.
    ///
    /// D-39 puts the scheduler here and everything about pixels one crate down,
    /// and D-27(a) requires that no export-path module can reach the cache at
    /// all. `export.rs` in particular must stay entirely ignorant that a render
    /// cache exists — 59-09 proves that on real delivered files, and this is the
    /// cheap structural half that fails the moment someone adds an import.
    ///
    /// The needle is assembled at runtime and the sources are comment-blanked,
    /// so a file may still EXPLAIN the rule (`lib.rs`'s module doc does, without
    /// spelling the name) without tripping it.
    #[test]
    fn render_cache_job_is_the_only_cache_naming_file_in_app_core() {
        let needle = format!("{}{}", "render", "cache");
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut namers: Vec<String> = std::fs::read_dir(&src_dir)
            .expect("app-core has a src directory")
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
            .filter(|e| {
                let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                comment_blanked(&text).contains(&needle)
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        namers.sort();
        eprintln!("CACHE-APPCORE-SCAN needle={needle:?} namers={namers:?}");
        assert_eq!(
            namers,
            vec!["render_cache_job.rs".to_string()],
            "D-39/D-27(a): the cache leaf crate is reachable from app-core's \
             SCHEDULER and from nowhere else. `export.rs` naming it would be a \
             delivered file one refactor away from playback pixels"
        );
        // Non-vacuity: the scanner really did read files, and really can find
        // the needle.
        assert!(
            comment_blanked(&own_source()).contains(&needle),
            "control: the scan can see this file's own code"
        );
    }

    /// Neither of the two named constants may quietly become a literal.
    #[test]
    fn render_cache_job_bounds_are_named_constants() {
        assert_eq!(MAX_CONCURRENT_RENDER_CACHE_JOBS, 1);
        assert_eq!(RESCAN_WINDOW_SEGMENTS, 8);
        assert_eq!(PLAYBACK_GUARD_SEGMENTS, 2);
        assert!(
            PLAYBACK_GUARD_SEGMENTS < RESCAN_WINDOW_SEGMENTS,
            "the playback guard is about the frames on screen right now and the \
             re-scan window is about bytes worth reclaiming; if the guard ever \
             grew past the window, one of the two has been misunderstood"
        );
    }

    // -----------------------------------------------------------------------
    // D-30 — the poll-only status
    // -----------------------------------------------------------------------

    /// The wire contract, pinned as the literals it crosses the ABI as.
    ///
    /// Phase 61 (D-17) appended `idle_spawned` LAST, and Phase 71 (TRUST-03)
    /// appended `committed_total` after it — LAST is the only place a field may
    /// go: `serde` writes them in declaration order and a C# DTO
    /// reads them by name. The value here is a NON-ZERO literal on purpose —
    /// the counter's real value is a process global this test must not depend
    /// on, and `0` would also be what a field serialized from the wrong source
    /// produced.
    #[test]
    fn render_cache_job_status_payload_wire_shape() {
        let json = serde_json::to_string(&RenderCacheStatusPayload {
            state: STATE_RENDERING.to_string(),
            rendering_segment: 7,
            cached_segments: 3,
            heavy_segments: 2,
            idle_spawned: 5,
            committed_total: 9,
        })
        .expect("serializes");
        assert_eq!(
            json,
            r#"{"state":"rendering","rendering_segment":7,"cached_segments":3,"heavy_segments":2,"idle_spawned":5,"committed_total":9}"#,
            "the field NAMES and their order are a wire contract with no \
             compiler on the other side of the ABI"
        );
        for state in [STATE_RENDERING, STATE_IDLE, STATE_NONE] {
            assert!(
                state.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "every state is a bare lowercase word, like the proxy getter's"
            );
        }
    }

    /// A fresh host has no render cache directory at all, so it reports
    /// `"none"` with `-1` and zeroes — and the getter creates nothing while
    /// saying so.
    #[test]
    fn render_cache_job_status_is_none_before_anything_exists() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        let dir = render_cache_dir(&ctx).expect("dir");
        let status = run_get_render_cache_status(&ctx).expect("a test ctx resolves a cache dir");
        assert_eq!(status.state, "none");
        assert_eq!(status.rendering_segment, -1);
        assert_eq!(status.cached_segments, 0);
        assert!(
            !dir.exists(),
            "T-59-08-05: a POLL must not create the directory it reports on"
        );
    }

    /// With segments on disk the census counts DISTINCT segment indices, and the
    /// state becomes `"idle"` — a cache that exists and is not working.
    #[test]
    fn render_cache_job_status_counts_committed_segments() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        let dir = render_cache_dir(&ctx).expect("dir");
        let canvas = (640u32, 360u32, 30.0f64);
        plant_segment(&dir, 1, 0xAAAA, canvas);
        plant_segment(&dir, 2, 0xBBBB, canvas);
        // A SECOND identity for segment 2 — a stale file the reader will never
        // serve. It must not double-count.
        plant_segment(&dir, 2, 0xCCCC, canvas);

        let status = run_get_render_cache_status(&ctx).expect("status");
        assert_eq!(status.state, "idle");
        assert_eq!(status.rendering_segment, -1);
        assert_eq!(
            status.cached_segments, 2,
            "distinct SEGMENTS, not files: two identities for segment 2 are one \
             cached segment and one orphan"
        );

        // And the detector's candidate count is reported, so the shell can see
        // work outstanding.
        mark_heavy(77);
        assert_eq!(
            run_get_render_cache_status(&ctx).expect("status").heavy_segments,
            1
        );
    }

    /// `"rendering"` names the segment in flight.
    #[test]
    fn render_cache_job_status_reports_the_segment_in_flight() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(6);

        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().values().any(JobEntry::is_live)
            }),
            "a render must be in flight for this gate to mean anything"
        );
        let status = run_get_render_cache_status(&ctx).expect("status");
        assert_eq!(status.state, "rendering");
        assert_eq!(status.rendering_segment, 6);
        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(30), || {
            !jobs().values().any(JobEntry::is_live)
        });
    }

    /// **Phase 71 (TRUST-01).** A paused render cache spawns nothing from either
    /// entry point, and unpausing restores exactly the behaviour it had. The
    /// control half (a spawn after unpausing) is what keeps the paused half from
    /// passing on a scheduler that never starts anything.
    #[test]
    fn a_paused_pump_spawns_nothing_and_unpausing_restores_it() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        let dir = render_cache_dir(&ctx).expect("cache dir");
        mark_heavy(14);

        set_paused(true);
        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        for _ in 0..20 {
            poll_and_spawn(&ctx);
            pump_tick(&dir);
        }
        std::thread::sleep(Duration::from_millis(100));
        let spawned_while_paused =
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) - spawned_before;
        let live_while_paused = live_row_count();
        set_paused(false);
        assert_eq!(
            spawned_while_paused, 0,
            "a paused render cache started a render: inside a device-lost recovery that \
             render's compositor would keep the removed D3D12 device alive"
        );
        assert_eq!(live_while_paused, 0, "and no row may be registered while paused");

        // The control: the same heavy segment, unpaused, is started.
        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) > spawned_before
            }),
            "control: unpaused, the same heavy segment must be started"
        );
        assert!(live_row_count() >= 1, "and its row is live while it renders");
        drop(gate);
        request_cancel_all();
        assert!(
            wait_until(Duration::from_secs(30), || live_row_count() == 0),
            "cancelled rows must drain to zero live rows"
        );
    }

    /// **71-REVIEW WR-01.** The pause is re-checked under the registry lock, so a
    /// poll that passed its entry check BEFORE a recovery paused the cache still
    /// registers nothing, and `pause_and_cancel_all` raises the latch of every row
    /// that was registered before it. Calling `poll_core` directly IS the
    /// "already past the entry check" poll: the entry checks live in its callers.
    #[test]
    fn a_poll_past_its_entry_check_registers_nothing_once_paused() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        let dir = render_cache_dir(&ctx).expect("cache dir");
        mark_heavy(15);

        // A bake registered before the pause has its latch raised by it.
        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || live_row_count() >= 1),
            "a render must be in flight for the latch half to mean anything"
        );
        pause_and_cancel_all();
        let paused = RENDER_CACHE_PAUSED.load(Ordering::SeqCst);
        let all_latched = jobs()
            .values()
            .filter(|row| row.is_live())
            .all(|row| row.cancel.load(Ordering::Relaxed));
        drop(gate);
        let drained = wait_until(Duration::from_secs(30), || live_row_count() == 0);

        // A poll already past its entry check, racing the pause.
        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        poll_core(&dir, None, false);
        std::thread::sleep(Duration::from_millis(100));
        let spawned_while_paused =
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) - spawned_before;
        let live_while_paused = live_row_count();
        set_paused(false);

        assert!(paused, "pause_and_cancel_all must set the pause");
        assert!(all_latched, "every row registered before the pause must be cancelled");
        assert!(drained, "cancelled rows must drain to zero live rows");
        assert_eq!(
            spawned_while_paused, 0,
            "a poll that passed its entry check before the pause started a render"
        );
        assert_eq!(live_while_paused, 0, "and registered a row while paused");

        // The control: the same direct poll, unpaused, does start the render.
        let gate = hold_render_gate();
        poll_core(&dir, None, false);
        assert!(
            wait_until(Duration::from_secs(10), || {
                RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) > spawned_before
            }),
            "control: unpaused, the same direct poll must start the render"
        );
        drop(gate);
        request_cancel_all();
        assert!(
            wait_until(Duration::from_secs(30), || live_row_count() == 0),
            "cancelled rows must drain to zero live rows"
        );
    }

    /// The getter cannot start work. Polling it a hundred times against a heavy
    /// segment with a host registered spawns nothing at all.
    #[test]
    fn render_cache_job_status_never_starts_a_render() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(4);
        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        for _ in 0..100 {
            run_get_render_cache_status(&ctx).expect("status");
        }
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed),
            spawned_before,
            "there is no code path from the poll to a render, and there must \
             never be one: this is the shell's only route to cache state"
        );
        assert_eq!(tracked_row_count(), 0, "and nothing was registered either");
    }

    // -----------------------------------------------------------------------
    // 59-24 — the runtime kill switch, both directions
    // -----------------------------------------------------------------------

    /// Sets (or clears) one environment variable for a scope and restores
    /// whatever was there before — including "nothing was there", which is the
    /// case a naive `set_var`/`remove_var` pair silently gets wrong on a
    /// developer machine that really does have the override set.
    ///
    /// `crates/rendercache/tests/encoder_license.rs`'s guard, same shape for
    /// the same reason (T-59-24-04): a test that LEAKED this variable would
    /// silently disable the cache for every test that runs after it in this
    /// binary, and the failures would be attributed anywhere but here.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> EnvGuard {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            EnvGuard { key, prev }
        }

        fn cleared(key: &'static str) -> EnvGuard {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            EnvGuard { key, prev }
        }

        /// Drop the override EARLY, inside a test, while still restoring the
        /// original value at scope end. The per-call-read proof needs exactly
        /// this: a flip, mid-process, with nothing rebuilt.
        fn clear_now(&self) {
            std::env::remove_var(self.key);
        }

        /// Whether this process can see `key` right now.
        ///
        /// Takes the key as a VALUE rather than naming the constant, so this
        /// module keeps exactly ONE textual gate expression — the production
        /// one, at the top of [`poll_and_spawn`] — which is what the acceptance
        /// scan pins. A second textual gate in a test would make "there is one
        /// gate" unverifiable by the cheapest available means.
        fn is_set(key: &str) -> bool {
            std::env::var_os(key).is_some()
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// **The DISABLED direction, behaviourally — and then the per-call read.**
    ///
    /// With the switch set, one poll does none of the four things this funnel
    /// exists to do: it configures no directory (so the SHIPPED read path
    /// cannot serve a segment that is byte-present on disk under the current
    /// identity), asks the detector for nothing, takes no encoder permit and
    /// registers no row. "No lookups and no spawns" is asserted as behaviour —
    /// a probe that misses, a registry that stays empty, a whole permit pool —
    /// rather than as a flag reading false (CLAUDE.md rule 3).
    ///
    /// Then, in the SAME test and the SAME process, the variable is removed and
    /// the SAME arrangement spawns. That is the half that proves the read is
    /// per-poll: a `OnceLock`, or any cache of the first answer, passes
    /// everything above and fails exactly here.
    #[test]
    fn render_cache_job_kill_switch_disables_the_whole_funnel() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        let host = BgHost::empty();
        set_render_host(host.clone());
        let dir = render_cache_dir(&ctx).expect("cache dir");

        // A segment that IS on disk under the CURRENT identity, planted through
        // the cache crate's own write path — so "no lookup serves" below is a
        // claim about the switch and not about an empty cache.
        let (canvas, edit_gen) =
            canvas_and_generation(host.as_ref()).expect("the test host has a store");
        let hash = SegmentLookup::new()
            .segment_hash_memo(host.as_ref(), 0, edit_gen, canvas)
            .expect("segment 0 must be hashable");
        plant_segment(&dir, 0, hash, canvas);

        // ---- the control: the DEFAULT is ON ------------------------------
        //
        // No candidate is marked yet, so this poll does only the wiring — and
        // the wiring is precisely what the disabled arm below has to undo.
        assert!(
            !EnvGuard::is_set(KILL_SWITCH_ENV),
            "the harness must start from an unset switch, or the arm below \
             proves nothing at all"
        );
        poll_and_spawn(&ctx);
        assert!(
            SegmentLookup::new()
                .probe(host.as_ref(), 0, edit_gen, canvas)
                .is_some(),
            "control: with the switch unset the SHIPPED read path must serve \
             the planted segment — otherwise the miss below is the fixture's \
             fault rather than the switch's"
        );

        // ---- the switch, SET ---------------------------------------------
        let guard = EnvGuard::set(KILL_SWITCH_ENV, "1");
        mark_heavy(10);
        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        let admission_before = RENDER_CACHE_JOBS_DEFERRED_ADMISSION.load(Ordering::Relaxed);
        let playback_before = RENDER_CACHE_JOBS_DEFERRED_PLAYBACK.load(Ordering::Relaxed);
        let walks_before =
            preview::render_cache_lookup::RENDER_CACHE_HASH_WALKS.load(Ordering::Relaxed);

        poll_and_spawn(&ctx);

        assert_eq!(
            tracked_row_count(),
            0,
            "with the switch set no row may be registered — the funnel is off, \
             not merely quiet"
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed),
            spawned_before,
            "and no render thread may start"
        );
        assert!(
            crate::proxy_job::try_take_permit().is_some(),
            "and the SHARED encoder permit must be untouched — a disabled \
             cache that still took admission would starve the proxy worker for \
             nothing"
        );
        assert_eq!(
            LAST_CONSIDERED.load(Ordering::Relaxed),
            i64::MIN,
            "and the detector must never have been walked: the gate is at the \
             TOP of the poll, before the candidate window advances"
        );
        assert_eq!(
            (
                RENDER_CACHE_JOBS_DEFERRED_ADMISSION.load(Ordering::Relaxed),
                RENDER_CACHE_JOBS_DEFERRED_PLAYBACK.load(Ordering::Relaxed),
            ),
            (admission_before, playback_before),
            "and it is not a DEFERRAL either — a disabled cache declines for \
             its own reason and must not be counted against admission or the \
             playback guard, or the deferral columns stop meaning anything"
        );
        assert_eq!(
            preview::render_cache_lookup::RENDER_CACHE_HASH_WALKS.load(Ordering::Relaxed),
            walks_before,
            "and the poll paid no key-material walk"
        );
        // NON-VACUITY: the arrangement really was spawn-capable — the candidate
        // the poll declined to take is still sitting in the detector.
        assert!(
            preview::render_cache_detect::heavy_segments_from(i64::MIN, HEAVY_SEGMENT_POLL_LIMIT)
                .contains(&10),
            "control: segment 10 must still be a heavy candidate, or the poll \
             above declined an empty world"
        );
        // THE serving half. The file is byte-present on disk and the identity
        // has not moved, so the only thing that can turn the control's Some()
        // into a None is the cleared directory slot: a mid-session flip stops
        // SERVING, not merely spawning.
        assert!(
            !segment_files_on_disk(&dir).is_empty(),
            "the planted segment must still be on disk — the switch disables \
             the cache, it does not delete it"
        );
        assert!(
            SegmentLookup::new()
                .probe(host.as_ref(), 0, edit_gen, canvas)
                .is_none(),
            "with the switch set the SHIPPED read path must serve NOTHING, \
             even though the segment is still on disk under the current \
             identity — a switch that only stopped new renders would leave \
             every already-baked segment being served"
        );
        // Quick 260828-h0u: and the PROJECT-OPEN start path obeys the same one
        // switch. It is a THIRD entry into this funnel that no transport
        // command drives — opening a project would otherwise start the clock
        // for a user who had just been told the cache was off. WARM-05,
        // extended to the open funnel rather than re-invented for it.
        assert!(
            ensure_pump_started(&ctx).is_none(),
            "with the switch set the project-open start path must start              nothing and configure nothing — quick 260828-h0u extends WARM-05              to the open funnel"
        );

        // ---- and now the PUMP, over the same set switch (WARM-05 / D-22) --
        //
        // Phase 61 gave this funnel a second caller that no transport command
        // drives. A switch that only stopped `poll_and_spawn` would leave a
        // background thread baking every 500 ms for a user who had just been
        // told the cache was off — which is the one failure a kill switch
        // exists to make impossible.
        //
        // D-10 means a live proxy row would ALSO park the pump, so the "started
        // nothing" below would pass for the neighbour's reason. Settle first,
        // and the only remaining explanation is the switch.
        settle_the_proxy_registry();
        // Re-point the reader at this ctx's directory, so the slot the ticks
        // below clear is one that was demonstrably serving an instant earlier —
        // otherwise this leg would be re-reading the poll above's work.
        preview::render_cache_lookup::configure_render_cache_dir(Some(dir.clone()));
        assert!(
            SegmentLookup::new()
                .probe(host.as_ref(), 0, edit_gen, canvas)
                .is_some(),
            "control: the reader is pointed back at the planted segment, so the \
             miss below is the PUMP's tick clearing the slot and not a leftover \
             from the poll above"
        );
        let pump_ticks_before = RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed);
        let pump_idle_before = RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed);
        let pump_spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);

        // Direct ticks rather than `spawn_pump_for_test` + a bounded wait: the
        // pump thread's whole body IS `pump_tick`, so this is equally real, and
        // it makes "the clock beat exactly N times" an equality rather than an
        // inequality.
        const KILL_SWITCH_PUMP_TICKS: u64 = 4;
        for _ in 0..KILL_SWITCH_PUMP_TICKS {
            pump_tick(&dir);
        }

        assert_eq!(
            RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed) - pump_ticks_before,
            KILL_SWITCH_PUMP_TICKS,
            "D-06: the switch STOPS the work, it does not stop the clock. The \
             pump must not exit — a flip back has to re-enable with no rebuild, \
             and a thread that returned cannot"
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed) - pump_idle_before,
            0,
            "and NOTHING may be attributed to the idle pump while the switch is \
             set — this is the counter a field session reads to answer \"did it \
             warm anything behind my back?\""
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) - pump_spawned_before,
            0,
            "and no render thread of any provenance started"
        );
        assert_eq!(
            tracked_row_count(),
            0,
            "and no row was registered by the pump either"
        );
        assert!(
            crate::proxy_job::try_take_permit().is_some(),
            "and the pump took no encoder admission — an idle bake that still \
             held the permit while the cache was OFF would starve the proxy \
             worker for work nobody asked for"
        );
        assert_eq!(
            LAST_CONSIDERED.load(Ordering::Relaxed),
            i64::MIN,
            "and the pump's gate is above the candidate walk too, exactly as \
             the poll's is"
        );
        assert!(
            SegmentLookup::new()
                .probe(host.as_ref(), 0, edit_gen, canvas)
                .is_none(),
            "and the PUMP'S OWN tick clears the shared directory slot: the \
             switch stops SERVING from the background path as well, or a paused \
             app would keep the reader wired up through a thread the user \
             cannot see"
        );

        // ---- the PER-CALL read: flip it off, same process, no rebuild -----
        guard.clear_now();
        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().values().any(JobEntry::is_live)
            }),
            "the switch is read on EVERY poll (var_os, hwdecode's idiom), so \
             removing it mid-session must re-arm the funnel with no rebuild — \
             a latched read would fail exactly here"
        );
        assert!(
            SegmentLookup::new()
                .probe(host.as_ref(), 0, edit_gen, canvas)
                .is_some(),
            "and the reader must be pointed back at this ctx's directory"
        );
        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(30), || {
            !jobs().values().any(JobEntry::is_live)
        });
        drop(guard);
    }

    /// **The ENABLED direction, pinned on its own.**
    ///
    /// With the switch absent — the shipped default, which `transport.rs`'s
    /// unconditional call site has had since 59-08 and which this plan did NOT
    /// re-implement — the funnel behaves exactly as it did before the gate
    /// existed: the reader is pointed at this ctx's directory and the heavy
    /// segment is taken.
    ///
    /// Separate from the test above on purpose (T-59-24-01). A two-armed test
    /// only ever compares its arms against each other, so an INVERTED gate —
    /// one that disables when the variable is absent — would satisfy "the two
    /// arms differ" while breaking every user who has never heard of it.
    #[test]
    fn render_cache_job_kill_switch_absent_is_the_shipped_default_on() {
        let _lease = lease();
        let _guard = EnvGuard::cleared(KILL_SWITCH_ENV);
        assert!(
            !EnvGuard::is_set(KILL_SWITCH_ENV),
            "this gate's whole subject is the ABSENT case"
        );
        let ctx = TestAppCtx::new();
        let host = BgHost::empty();
        set_render_host(host.clone());
        let dir = render_cache_dir(&ctx).expect("cache dir");

        let (canvas, edit_gen) =
            canvas_and_generation(host.as_ref()).expect("the test host has a store");
        let hash = SegmentLookup::new()
            .segment_hash_memo(host.as_ref(), 0, edit_gen, canvas)
            .expect("segment 0 must be hashable");
        plant_segment(&dir, 0, hash, canvas);
        mark_heavy(10);

        // Quick 260828-h0u: the ABSENT direction for the project-open start
        // path, pinned in the same test that pins it for the poll — one
        // switch, one meaning, now across three callers.
        assert!(
            ensure_pump_started(&ctx).is_some(),
            "with the switch absent the open-funnel start path resolves the              dir and runs the Once-guarded start — the shipped default is ON              for the open path too"
        );

        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().values().any(JobEntry::is_live)
            }),
            "the render cache is ON BY DEFAULT: the switch only ever DISABLES, \
             and adding it may not turn the feature into an opt-in"
        );
        let live: Vec<i64> = jobs()
            .iter()
            .filter(|(_, row)| row.is_live())
            .map(|(seg, _)| *seg)
            .collect();
        assert_eq!(
            live,
            vec![10],
            "and it must be the segment the detector actually named"
        );
        assert!(
            SegmentLookup::new()
                .probe(host.as_ref(), 0, edit_gen, canvas)
                .is_some(),
            "and the default-on poll must point the SHIPPED read path at this \
             ctx's directory — serving is the half a user notices"
        );
        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(30), || {
            !jobs().values().any(JobEntry::is_live)
        });

        // ---- the PUMP, over the SAME switch, in BOTH directions -----------
        //
        // WARM-05 / D-22: one switch, one meaning. Phase 61 added a caller no
        // transport command drives, so "absent = on" and "set = off" both have
        // to be re-pinned for it — and the flip has to work MID-SESSION, in this
        // same process, with nothing rebuilt (D-06, `engine::hwdecode`'s idiom).
        //
        // D-10 would also park the pump on a live proxy row, so settle first:
        // afterwards the ONLY thing that can explain a declined tick is the
        // switch.
        settle_the_proxy_registry();

        // (a) SET, mid-session. The clock beats and starts nothing.
        let flip = EnvGuard::set(KILL_SWITCH_ENV, "1");
        // Quick 260828-h0u: a flip mid-process disables the OPEN path exactly
        // as it disables the poll — asserted BEFORE the re-point below, so the
        // pump's own tick is still the only thing that can clear the slot for
        // the assertion at the end of this leg.
        assert!(
            ensure_pump_started(&ctx).is_none(),
            "a mid-session flip must disable the project-open start path too,              or reopening a project would re-arm a cache the user just turned              off"
        );
        preview::render_cache_lookup::configure_render_cache_dir(Some(dir.clone()));
        let ticks_before = RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed);
        let idle_before = RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed);
        // Not zero: the transport half above already left segment 10's row
        // behind in a TERMINAL state, which is correct and is what makes the
        // re-arm below a retry rather than a first sighting. What must not move
        // is the count.
        let rows_before = tracked_row_count();
        const FLIP_TICKS: u64 = 3;
        for _ in 0..FLIP_TICKS {
            pump_tick(&dir);
        }
        assert_eq!(
            RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed) - ticks_before,
            FLIP_TICKS,
            "the switch stops the WORK, never the clock — a pump that exited \
             could not be turned back on below"
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed) - idle_before,
            0,
            "with the switch set the pump starts nothing"
        );
        assert_eq!(
            tracked_row_count(),
            rows_before,
            "and registers nothing new"
        );
        assert!(
            !jobs().values().any(JobEntry::is_live),
            "and leaves nothing live"
        );
        assert!(
            SegmentLookup::new()
                .probe(host.as_ref(), 0, edit_gen, canvas)
                .is_none(),
            "and clears the shared directory slot on its own tick"
        );

        // (b) REMOVED, same process, no rebuild. The pump warms again.
        flip.clear_now();
        let gate = hold_render_gate();
        pump_tick(&dir);
        assert!(
            wait_until(Duration::from_secs(10), || {
                RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed) - idle_before == 1
            }),
            "D-06/D-22: removing the variable mid-session must re-arm the PUMP \
             with no rebuild, and the spawn must be attributed to it. A latched \
             read, or a pump that exited on the set half above, fails exactly \
             here"
        );
        let live_after_flip: Vec<i64> = jobs()
            .iter()
            .filter(|(_, row)| row.is_live())
            .map(|(seg, _)| *seg)
            .collect();
        assert_eq!(
            live_after_flip,
            vec![10],
            "and the re-armed pump takes the segment the detector named, not \
             some other one"
        );
        assert!(
            SegmentLookup::new()
                .probe(host.as_ref(), 0, edit_gen, canvas)
                .is_some(),
            "and the reader is pointed back at this ctx's directory — the pump \
             re-arms SERVING as well as spawning"
        );

        drop(gate);
        request_cancel_all();
        assert!(
            wait_until(Duration::from_secs(60), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "teardown: the re-armed bake must end before the lease is released"
        );
        drop(flip);
    }
    // -----------------------------------------------------------------------
    // Phase 61 (WARM-01, D-01..D-06) — the clock
    // -----------------------------------------------------------------------

    /// How many segments the Criterion-1 fixture covers.
    const PUMP_SEGMENT_COUNT: usize = 3;
    /// Which ones. `SEG_US` is 2 s, so these are program time `[0 s, 6 s)`.
    const PUMP_SEGMENTS: [i64; PUMP_SEGMENT_COUNT] = [0, 1, 2];
    /// The fixture's clip length — 8 s, so segment 2's `[4 s, 6 s)` is inside
    /// the media with slack, and the pump is never asked to bake a gap.
    const PUMP_SPAN_US: i64 = 8_000_000;

    /// A REAL, cacheable, MULTI-layer arrangement covering [`PUMP_SEGMENTS`].
    ///
    /// TWO overlapping video layers, and that is load-bearing rather than
    /// decorative — `crate::test_support::seed_layered_video_arrangement`'s own
    /// doc records why a one-clip arrangement is PERMANENTLY uncacheable, and
    /// why the builder lives over there rather than in this file.
    ///
    /// Both sources are real files off `test-media/`, decoded and encoded
    /// through the BUNDLED LGPL sidecar: [`crate::test_support::fixture`]'s
    /// first act is to pin `RUDIS_FFMPEG_DIR` at `runtime/binaries`, which is
    /// CLAUDE.md rule 6 and — as `test_support`'s own doc measured — the
    /// difference between this module's real-media gates passing and failing.
    fn two_layer_host() -> Arc<BgHost> {
        let sources = [
            crate::test_support::fixture("solid_red_720p30_20s.mp4"),
            crate::test_support::fixture("bars_720p30_75s.mp4"),
        ];
        let host = BgHost::empty();
        {
            let mut store = host.store.lock().unwrap_or_else(|p| p.into_inner());
            crate::test_support::seed_layered_video_arrangement(&mut store, &sources, PUMP_SPAN_US);
        }
        host
    }

    /// **WARM-01 / Criterion 1 — the field stall does not reproduce.**
    ///
    /// The measurement this phase exists to kill, quoted so the gate is legible:
    /// on the owner's six-layer stack, segment `k0` committed at 14:01:40 and
    /// then **sat alone for 2 min 48 s** with the app open and PAUSED. The
    /// remaining segments appeared only once playback resumed — because
    /// `poll_and_spawn` had exactly one caller, the transport funnel, and a
    /// paused app never calls it.
    ///
    /// So: a paused store, three heavy segments, the REAL pump on a joinable
    /// thread — and from the moment it starts, this test issues **zero**
    /// transport commands and **zero** `poll_and_spawn` calls. Nothing but the
    /// clock advances the scheduler.
    ///
    /// The observable is CLAUDE.md rule 3's, not a status payload's: every
    /// segment is read back through the SHIPPED read path
    /// (`rendercache::cache::read_fresh_segment`) off the real filesystem, under
    /// the identity the writer must have committed it as. A `cached_segments`
    /// census counts names; this reads files.
    ///
    /// It renders real media through the real encoder, so it costs seconds. The
    /// 120 s bound is slack, not an expectation.
    #[test]
    fn render_cache_job_pump_commits_all_heavy_segments_while_idle() {
        let _lease = lease();

        // D-10 means a live proxy row makes every pump tick defer, forever. Ours
        // registers none, but a neighbour's could still be settling — the same
        // hazard `lease()` already settles for the shared permit, handled the
        // same way rather than left as a flake.
        assert!(
            wait_until(Duration::from_secs(30), || {
                !crate::proxy_job::has_pending_work()
            }),
            "a previous test left a live proxy row behind; the pump would defer \
             to it for the whole of this gate (D-10)"
        );

        let tmp = tempfile::TempDir::new().expect("a cache dir of this test's own");
        let dir = tmp.path().to_path_buf();
        preview::render_cache_lookup::configure_render_cache_dir(Some(dir.clone()));

        let host = two_layer_host();
        set_render_host(host.clone());
        let paused = !host
            .store()
            .expect("the fixture has a store")
            .playback()
            .playing;
        assert!(
            paused,
            "the fixture must be PAUSED — this gate is about the IDLE case, and \
             a playing one would also engage the D-24 guard"
        );

        for seg in PUMP_SEGMENTS {
            mark_heavy(seg);
        }

        // The identity each segment must be committed under, derived exactly as
        // `render_cache_job_shared_admission` derives its one.
        let (canvas, edit_gen) =
            canvas_and_generation(host.as_ref()).expect("the fixture host has a store");
        let mut lookup = SegmentLookup::new();
        let hashes: Vec<u64> = PUMP_SEGMENTS
            .iter()
            .map(|seg| {
                lookup
                    .segment_hash_memo(host.as_ref(), *seg, edit_gen, canvas)
                    .expect("a real two-layer arrangement has an identity")
            })
            .collect();
        let on_disk = |i: usize| -> Option<rendercache::cache::SegmentHit> {
            rendercache::cache::read_fresh_segment(
                &dir,
                PUMP_SEGMENTS[i],
                hashes[i],
                canvas.0,
                canvas.1,
                canvas.2,
            )
        };
        for i in 0..PUMP_SEGMENT_COUNT {
            assert!(
                on_disk(i).is_none(),
                "control: segment {} must not already be cached, or this gate \
                 would pass for a pump that never ran",
                PUMP_SEGMENTS[i]
            );
        }

        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        let idle_before = RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed);
        let ticks_before = RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed);
        let committed_before = RENDER_CACHE_SEGMENTS_COMMITTED.load(Ordering::Relaxed);

        // ---- FROM HERE THE TEST DOES NOTHING BUT WATCH -------------------
        //
        // No transport command, no `poll_and_spawn`, no hand-driven
        // `pump_tick`. That IS the criterion.
        let pump = spawn_pump_for_test(dir.clone());

        let mut found = [false; PUMP_SEGMENT_COUNT];
        let all_committed = wait_until(Duration::from_secs(120), || {
            for i in 0..PUMP_SEGMENT_COUNT {
                if !found[i] {
                    found[i] = on_disk(i).is_some();
                }
            }
            found.iter().all(|f| *f)
        });
        drop(pump);

        let ticks = RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed) - ticks_before;
        assert!(
            all_committed,
            "WARM-01: with the app PAUSED and ZERO transport commands, the idle \
             pump alone must commit every heavy segment on disk. After {ticks} \
             ticks the on-disk answer per segment was {found:?} for {PUMP_SEGMENTS:?}"
        );
        assert!(
            ticks > 0,
            "the clock must be observable as well as effective — \
             RENDER_CACHE_PUMP_TICKS is how the next field session tells a \
             parked pump from one that never started (D-17)"
        );

        // CLAUDE.md rule 3, as evidence and not as a claim: the bytes are on
        // disk, and the SHIPPED reader can serve them.
        for i in 0..PUMP_SEGMENT_COUNT {
            let hit = on_disk(i).expect("just asserted present");
            let bytes = std::fs::metadata(&hit.path)
                .expect("stat the committed payload")
                .len();
            eprintln!(
                "PUMP-IDLE-COMMIT seg={} path_exists={} {}x{}@{} bytes={bytes}",
                PUMP_SEGMENTS[i],
                hit.path.is_file(),
                hit.w,
                hit.h,
                hit.fps
            );
            assert!(bytes > 0, "a committed payload has pixels in it");
        }

        let spawned = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) - spawned_before;
        let idle = RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed) - idle_before;
        assert_eq!(
            idle, PUMP_SEGMENT_COUNT as u64,
            "D-17: every one of the {PUMP_SEGMENT_COUNT} bakes must be \
             ATTRIBUTED to the pump, not merely have happened — the counter is \
             the whole answer to \"warmed while idle, or warmed because you \
             played it again?\""
        );
        assert_eq!(
            idle, spawned,
            "and it must account for ALL of them: a spawn this test cannot \
             attribute to the pump would mean something else drove the \
             scheduler, which is exactly what this gate denies"
        );

        // Phase 71 (TRUST-03): the commit counter moved by EXACTLY the number
        // of segments read back off disk above. The bump follows the file
        // commit by a few instructions in the render thread, so allow it to
        // land before comparing.
        let committed_delta = || {
            RENDER_CACHE_SEGMENTS_COMMITTED.load(Ordering::Relaxed) - committed_before
        };
        wait_until(Duration::from_secs(10), || {
            committed_delta() >= PUMP_SEGMENT_COUNT as u64
        });
        eprintln!("TRUST-03 COMMITTED-DELTA {}", committed_delta());
        assert_eq!(
            committed_delta(),
            PUMP_SEGMENT_COUNT as u64,
            "committed_total must move by exactly the {PUMP_SEGMENT_COUNT} segments this bake committed"
        );

        clear_render_host();
    }

    /// **Research Pitfall 4 — the first-ever CONCURRENT caller of the
    /// scheduler core, proven safe.**
    ///
    /// Every test in this file before Phase 61 drove `poll_and_spawn` from one
    /// thread at a time, serialized by [`lease`]. The pump is the first caller
    /// that can run genuinely concurrently with the transport path, and the
    /// registry's "check `is_live`, then much later insert a row" sequence has a
    /// real TOCTOU window between two callers that nothing exercised.
    ///
    /// Two threads, released together on a barrier, race over ONE heavy
    /// candidate in ONE cache directory: the pump side calls [`pump_tick`], the
    /// transport side calls [`poll_and_spawn`] with a real ctx. The claims:
    ///
    /// * exactly ONE spawn, so the shared permit plus the `is_live` check really
    ///   do close the window (a duplicate would put two encodes on one piece of
    ///   silicon and race two writers onto one file name);
    /// * exactly ONE registry row, for the segment the detector named;
    /// * and it TERMINATES. `poll_core`'s guard input is a plain `Option<i64>`
    ///   whose lock the caller released; `AppCtx::store()` and
    ///   `PreviewHost::store()` are the same non-reentrant mutex, so a future
    ///   edit that held one across this boundary would hang here rather than in
    ///   the field. **A hang in this test IS that regression** (61-RESEARCH §A4).
    #[test]
    fn render_cache_job_poll_core_two_concurrent_callers_spawn_at_most_once() {
        let _lease = lease();
        // Pin the bundled LGPL sidecar for the render this gate starts — the
        // path itself is unused, the pin is the point (see `two_layer_host`).
        let _pin = crate::test_support::fixture("bars_720p30_5s.mp4");

        let ctx = TestAppCtx::new();
        // ONE directory for both racers: the transport shell resolves it from
        // the ctx, and the pump is handed that same path, so the two threads
        // contend over the same registry, the same candidate and the same
        // permit rather than politely missing each other.
        let dir = render_cache_dir(&ctx).expect("cache dir");
        preview::render_cache_lookup::configure_render_cache_dir(Some(dir.clone()));
        set_render_host(BgHost::empty());
        mark_heavy(7);

        // The one legitimate spawn parks inside its work still holding the
        // permit, so the race is observable instead of being over before the
        // second thread's first poll.
        let gate = hold_render_gate();
        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        let ticks_before = RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed);

        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("rudis-rc-race-pump".to_string())
                .spawn_scoped(scope, || {
                    start.wait();
                    for _ in 0..50 {
                        pump_tick(&dir);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })
                .expect("spawn the pump-side racer");
            std::thread::Builder::new()
                .name("rudis-rc-race-transport".to_string())
                .spawn_scoped(scope, || {
                    start.wait();
                    for _ in 0..50 {
                        poll_and_spawn(&ctx);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })
                .expect("spawn the transport-side racer");
        });

        assert!(
            wait_until(Duration::from_secs(10), || {
                jobs().get(&7).map(|row| row.state) == Some(ROW_RUNNING)
            }),
            "one of the two racers must have started the one candidate — \
             without that the count below would pass for a pair of threads that \
             raced to do nothing, saw {:?}",
            jobs().get(&7).map(|row| row.state)
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) - spawned_before,
            1,
            "ONE candidate, two concurrent callers, exactly one spawn: the \
             loser must defer or early-out, never duplicate. Two spawns would \
             be two encodes on one encoder and two writers on one file name"
        );
        assert_eq!(
            tracked_row_count(),
            1,
            "and exactly one registry row survives the race"
        );
        assert_eq!(
            RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed) - ticks_before,
            50,
            "control: the pump-side racer must really have taken all 50 of its              ticks. Without this the assertions above would also pass for a              thread that never ran and a race that never happened"
        );
        assert_eq!(
            jobs().keys().copied().collect::<Vec<i64>>(),
            vec![7],
            "which must be the segment the detector actually named"
        );

        drop(gate);
        request_cancel_all();
        wait_until(Duration::from_secs(30), || {
            !jobs().values().any(JobEntry::is_live)
        });
        clear_render_host();
    }

    // -----------------------------------------------------------------------
    // Phase 61 (WARM-04, D-07/D-08/D-09) — THE YIELD
    // -----------------------------------------------------------------------

    /// The segment every yield gate bakes.
    ///
    /// `1` for two independent reasons that have to hold at once: `[2 s, 4 s)`
    /// is comfortably inside [`PUMP_SPAN_US`], so the fixture can really commit
    /// it, and it is within [`PLAYBACK_GUARD_SEGMENTS`] of segment 0 — where a
    /// store that has never been seeked puts the playhead — so the transport
    /// gate below drives a REAL transition into a REAL band without having to
    /// arrange one.
    const YIELD_BAKE_SEG: i64 = 1;

    /// Start ONE real, gated, in-flight bake of [`YIELD_BAKE_SEG`], and hand
    /// back the latch its writer is watching.
    ///
    /// The ctx's store is PAUSED, so D-24's guard is lifted and the scheduler
    /// is free to take the candidate; the render gate the CALLER holds is what
    /// keeps the bake in flight for as long as that gate lives. `ROW_RUNNING`
    /// is published before the writer reaches the gate, which is exactly why
    /// this can return with the render provably started and provably not
    /// finished.
    fn start_gated_bake<C: AppCtx>(ctx: &C) -> Arc<AtomicBool> {
        mark_heavy(YIELD_BAKE_SEG);
        poll_and_spawn(ctx);
        assert!(
            wait_until(Duration::from_secs(30), || {
                jobs().get(&YIELD_BAKE_SEG).map(|row| row.state) == Some(ROW_RUNNING)
            }),
            "the bake must be IN FLIGHT before the yield is asked to do \
             anything, or the gate proves something about a render that never \
             started. Row: {:?}",
            jobs().get(&YIELD_BAKE_SEG).map(|row| row.state)
        );
        let latch = jobs()
            .get(&YIELD_BAKE_SEG)
            .map(|row| Arc::clone(&row.cancel))
            .expect("a live row owns the latch its render watches");
        assert!(
            !latch.load(Ordering::Relaxed),
            "control: a fresh bake starts with its latch DOWN"
        );
        latch
    }

    /// **WARM-04 / D-07 — the IN-BAND bake yields.**
    ///
    /// A live bake is in flight for [`YIELD_BAKE_SEG`] when playback starts
    /// within [`PLAYBACK_GUARD_SEGMENTS`] of it, and the latch the writer polls
    /// at the top of every frame comes up. It then settles by the EXISTING
    /// external classification (D-09): the attempt is REFUNDED and the separate
    /// [`MAX_EXTERNAL_ABORTS`] budget is charged instead, because a user
    /// pressing Play says nothing at all about whether this segment can be
    /// rendered. No new state, no new budget, no new settlement arm.
    ///
    /// The case exercised is the band's OUTERMOST in-band distance, with one
    /// segment further out as the control immediately before it — so a `<` that
    /// should have been a `<=`, and a raise that ignored the band entirely,
    /// both fail here.
    #[test]
    fn render_cache_job_play_transition_cancels_in_guard_band_bake() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        set_render_host(two_layer_host());

        let gate = hold_render_gate();
        let latch = start_gated_bake(&ctx);

        // CONTROL FIRST: one segment PAST the band changes nothing.
        on_play_transition(YIELD_BAKE_SEG + PLAYBACK_GUARD_SEGMENTS + 1);
        assert!(
            !latch.load(Ordering::Relaxed),
            "a play transition {} segments away is outside the guard band and \
             must not reach the bake at all — without this control the \
             assertion below would also pass for a hook that cancels \
             everything, which is the `request_cancel_all` this deliberately \
             is not",
            PLAYBACK_GUARD_SEGMENTS + 1
        );

        // THE YIELD, at the band's outermost in-band distance.
        on_play_transition(YIELD_BAKE_SEG + PLAYBACK_GUARD_SEGMENTS);
        assert!(
            latch.load(Ordering::Relaxed),
            "D-07: a play transition within {PLAYBACK_GUARD_SEGMENTS} segments \
             must raise the latch the RUNNING render is actually watching — a \
             latch raised on some other Arc is a flag nobody reads"
        );

        drop(gate);
        assert!(
            wait_until(Duration::from_secs(60), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "and the cancelled render must END, not merely be asked to"
        );
        assert_eq!(
            jobs()
                .get(&YIELD_BAKE_SEG)
                .map(|row| (row.state, row.attempts, row.external_aborts)),
            Some((ROW_RETRY, 0, 1)),
            "D-09: the yield is a USER action, so it settles exactly as project \
             open, media removal and shutdown already do — refund the attempt, \
             charge the existing external budget. A new state string or a new \
             budget here would be a second policy wearing the same name"
        );
        let dir = render_cache_dir(&ctx).expect("dir");
        assert!(
            segment_files_on_disk(&dir).is_empty(),
            "and it leaves NOTHING on disk: the writer commits by atomic \
             temp-then-rename and its abort arm deletes the temp, so a \
             cancelled bake can never be served as a torn segment. Found {:?}",
            segment_files_on_disk(&dir)
        );
        clear_render_host();
    }

    /// **WARM-04 / D-07 — the OUT-OF-BAND bake is left to finish.**
    ///
    /// The other half of the decision, and the reason it is a decision rather
    /// than "cancel on Play": a bake the guard already says is not in the
    /// user's way is PAID-FOR WORK. Discarding it would make resume-from-pause
    /// cost more rather than less, and the very next poll would start it again.
    ///
    /// Proven on real media and on the real filesystem (CLAUDE.md rule 3): the
    /// undisturbed bake is allowed to run, and the segment it commits is read
    /// back through the SHIPPED reader under the identity the writer must have
    /// named it with. A latch assertion alone could not make this half of the
    /// claim.
    #[test]
    fn render_cache_job_play_transition_leaves_out_of_band_bake_alone() {
        let _lease = lease();
        let ctx = TestAppCtx::new();
        let host = two_layer_host();
        set_render_host(host.clone());

        let dir = render_cache_dir(&ctx).expect("dir");
        let (canvas, edit_gen) =
            canvas_and_generation(host.as_ref()).expect("the fixture host has a store");
        let mut lookup = SegmentLookup::new();
        let hash = lookup
            .segment_hash_memo(host.as_ref(), YIELD_BAKE_SEG, edit_gen, canvas)
            .expect("a real two-layer arrangement has an identity");
        let on_disk = || {
            rendercache::cache::read_fresh_segment(
                &dir,
                YIELD_BAKE_SEG,
                hash,
                canvas.0,
                canvas.1,
                canvas.2,
            )
        };
        assert!(
            on_disk().is_none(),
            "control: the segment must not already be cached, or the commit \
             asserted below would pass for a bake that never ran"
        );

        let gate = hold_render_gate();
        let latch = start_gated_bake(&ctx);

        // Playback starts far enough away that the guard already covers the
        // user: |YIELD_BAKE_SEG - play_seg| > PLAYBACK_GUARD_SEGMENTS.
        let play_seg = YIELD_BAKE_SEG + PLAYBACK_GUARD_SEGMENTS + 1;
        on_play_transition(play_seg);
        assert!(
            !latch.load(Ordering::Relaxed),
            "an out-of-band bake's latch must stay DOWN"
        );
        assert!(
            jobs().get(&YIELD_BAKE_SEG).is_some_and(JobEntry::is_live),
            "and the bake must still be LIVE — un-latched is not the same claim \
             as undisturbed"
        );

        // Now let it finish. Nothing cancels it, at any point.
        drop(gate);
        assert!(
            wait_until(Duration::from_secs(180), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "the undisturbed bake must run to its OWN end"
        );
        assert_eq!(
            jobs()
                .get(&YIELD_BAKE_SEG)
                .map(|row| (row.state, row.external_aborts)),
            Some((ROW_DONE, 0)),
            "it COMMITTED, and it was never charged an external abort — which \
             together say the yield did not reach it"
        );
        let hit = on_disk().expect(
            "CLAUDE.md rule 3: the claim is bytes on disk that the SHIPPED \
             reader can serve, not a row in a registry",
        );
        let bytes = std::fs::metadata(&hit.path)
            .expect("stat the committed payload")
            .len();
        eprintln!(
            "YIELD-OUT-OF-BAND-COMMIT seg={YIELD_BAKE_SEG} play_seg={play_seg} \
             {}x{}@{} bytes={bytes}",
            hit.w, hit.h, hit.fps
        );
        assert!(bytes > 0, "a committed payload has pixels in it");
        clear_render_host();
    }

    /// **WARM-04 / D-08 — the hook is EDGE-triggered, through the REAL funnel.**
    ///
    /// Driven by [`crate::transport::run_transport`] rather than by calling
    /// [`on_play_transition`] directly, because the claim under test is the
    /// WIRING: the hook has to sit AFTER the store apply — read before it,
    /// `playing` is still the old value and there is no transition to see,
    /// which is why it cannot ride the `poll_and_spawn` call that funnel
    /// already makes — and it has to compare the old value with the new one
    /// instead of firing on every Play.
    ///
    /// Four commands, one real live bake, one real latch: Pause-while-paused
    /// and Seek-while-paused raise nothing, the FIRST Play raises it, and a
    /// second Play while already playing raises nothing.
    #[test]
    fn render_cache_job_play_transition_fires_only_on_false_to_true() {
        let _lease = lease();
        let host = two_layer_host();

        // The ctx needs a timeline of its OWN: the real Play command refuses a
        // program with nothing to preview, and this gate drives the real
        // command rather than a stand-in for it.
        let ctx = TestAppCtx::new();
        {
            let sources = [
                crate::test_support::fixture("solid_red_720p30_20s.mp4"),
                crate::test_support::fixture("bars_720p30_75s.mp4"),
            ];
            let mut store = ctx.store().lock().unwrap_or_else(|p| p.into_inner());
            crate::test_support::seed_layered_video_arrangement(&mut store, &sources, PUMP_SPAN_US);
        }
        set_render_host(host);

        let gate = hold_render_gate();
        let latch = start_gated_bake(&ctx);

        // The playhead has never moved, so Play will announce whatever segment
        // it sits on now. Read it the way the hook will, and assert the bake is
        // inside its band — otherwise the three controls below could all pass
        // simply because the band was empty.
        let play_seg = {
            let store = ctx.store().lock().unwrap_or_else(|p| p.into_inner());
            preview::render_cache_lookup::segment_index_for(store.playback().position_us)
        };
        assert!(
            (YIELD_BAKE_SEG - play_seg).abs() <= PLAYBACK_GUARD_SEGMENTS,
            "fixture: the bake at {YIELD_BAKE_SEG} has to be IN the band Play \
             announces at {play_seg}, or this gate proves nothing about edges"
        );

        // CONTROL: transport commands that do not START playback.
        crate::transport::run_transport(&ctx, rudis_core::TransportCmd::Pause)
            .expect("pause applies");
        assert!(
            !latch.load(Ordering::Relaxed),
            "Pause while already paused is not a transition"
        );
        crate::transport::run_transport(&ctx, rudis_core::TransportCmd::Seek { position_us: 0 })
            .expect("seek applies");
        assert!(
            !latch.load(Ordering::Relaxed),
            "Seek while PAUSED is not a transition either — the playhead moved, \
             playback did not start"
        );

        // THE EDGE.
        crate::transport::run_transport(&ctx, rudis_core::TransportCmd::Play)
            .expect("play applies");
        assert!(
            latch.load(Ordering::Relaxed),
            "D-08: the yield must be reachable from the REAL transport funnel, \
             and it must sit AFTER the apply — read before it, `playing` is \
             still false and the transition is invisible"
        );

        // CONTROL: level, not edge. The writer is parked on the render gate
        // this test still holds, so it has not read the latch even once —
        // lowering it is invisible to the render and re-arms the same real row.
        latch.store(false, Ordering::Relaxed);
        crate::transport::run_transport(&ctx, rudis_core::TransportCmd::Play)
            .expect("play applies again");
        assert!(
            !latch.load(Ordering::Relaxed),
            "a second Play while ALREADY playing is not a transition. A \
             level-triggered hook would re-cancel the same bake on every \
             command a playing timeline sends — including every per-frame \
             Advance"
        );

        request_cancel_all();
        drop(gate);
        assert!(
            wait_until(Duration::from_secs(60), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "teardown: the bake must end"
        );
        clear_render_host();
    }

    // -----------------------------------------------------------------------
    // Phase 61 (WARM-03 / D-10, D-11, D-12) — the discriminating defer PAIR
    // -----------------------------------------------------------------------

    /// The literal registry key both defer gates plant.
    ///
    /// `proxy_job::job_key` canonicalizes real paths, so a bare literal can
    /// never collide with a row a real import made — and a planted row costs no
    /// encoder, no file and no permit, which is precisely what makes the pair
    /// below discriminating (see `plant_queued_row_for_test`'s own doc).
    const DEFER_PROXY_KEY: &str = "phase61-defer";

    /// A planted proxy row that removes itself on the way out, **including on
    /// the way out of a panic**.
    ///
    /// Not a nicety, and it was MEASURED: with the defer neutered on purpose to
    /// take these gates' RED evidence, the defer gate panicked before its
    /// hand-written cleanup line and left the row behind — which then parked
    /// the pump for every test after it in the binary and turned a one-test
    /// failure into a two-test failure with a misleading second message. D-10
    /// makes ANY live proxy row global, latching state; the only correct owner
    /// of one is a `Drop` impl. `RowGuard`'s own reasoning, one layer up.
    struct PlantedProxyRow(&'static str);

    impl PlantedProxyRow {
        fn plant(key: &'static str) -> PlantedProxyRow {
            crate::proxy_job::plant_queued_row_for_test(key);
            PlantedProxyRow(key)
        }
    }

    impl Drop for PlantedProxyRow {
        fn drop(&mut self) {
            crate::proxy_job::remove_planted_row_for_test(self.0);
        }
    }

    /// D-10 means ONE live proxy row parks every pump tick, so a neighbour's
    /// unsettled row would silently make either gate below pass for the wrong
    /// reason. Settled the way [`lease`] settles the shared permit rather than
    /// left as a cross-test flake — WARM-01's test set this precedent.
    fn settle_the_proxy_registry() {
        assert!(
            wait_until(Duration::from_secs(30), || {
                !crate::proxy_job::has_pending_work()
            }),
            "a previous test left a live proxy row behind; these gates plant \
             their own and must start from an empty registry to mean anything"
        );
    }

    /// **WARM-03a / D-10 — the pump defers to pending proxy work, and the
    /// deferral is the PREDICATE's and not the semaphore's.**
    ///
    /// One proxy row is planted `queued`. The shared encoder permit is left
    /// **free**, and that is the whole design of this gate: `try_take_permit`'s
    /// non-blocking defer already prevents two SIMULTANEOUS encodes, so a test
    /// that held the permit would pass for a pump with no `has_pending_work()`
    /// check in it at all (`render_cache_job_shared_admission` is that test, and
    /// it shipped in Phase 59). What this proves is the thing D-10 added: a
    /// pump that could take the permit right now still declines, because a
    /// ten-file import loses 50-70 s to a bake winning the gap between proxies.
    ///
    /// The CONTROL is in the same test and differs by exactly one fact: remove
    /// the planted row, tick the same pump against the same directory and the
    /// same candidate, and it spawns. Without that half, "nothing was spawned"
    /// would be satisfied by a pump that declined for any of the five other
    /// reasons a poll can decline.
    #[test]
    fn render_cache_job_pump_defers_to_pending_proxy_work() {
        let _lease = lease();
        settle_the_proxy_registry();

        let tmp = tempfile::TempDir::new().expect("a cache dir of this test's own");
        let dir = tmp.path().to_path_buf();
        set_render_host(BgHost::empty());
        mark_heavy(3);

        let planted = PlantedProxyRow::plant(DEFER_PROXY_KEY);
        assert!(
            crate::proxy_job::has_pending_work(),
            "the planted row must make D-10's predicate answer true, or this \
             gate is watching an empty registry decline an empty world"
        );
        // THE discriminator. Taken and dropped in one statement — the pool is
        // whole at the instant the ticks below run.
        assert!(
            crate::proxy_job::try_take_permit().is_some(),
            "a PLANTED row holds no permit: the deferral below must be D-10's \
             doing and not D-23's, or this gate is a duplicate of \
             `render_cache_job_shared_admission`"
        );

        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        let idle_before = RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed);
        let ticks_before = RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed);

        const DEFER_TICKS: u64 = 5;
        for _ in 0..DEFER_TICKS {
            pump_tick(&dir);
        }

        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) - spawned_before,
            0,
            "with a proxy job pending the idle pump must start NOTHING"
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed) - idle_before,
            0,
            "and nothing may be attributed to it either"
        );
        assert_eq!(
            tracked_row_count(),
            0,
            "not even a registered row — the pump defers, it does not queue"
        );
        assert_eq!(
            RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed) - ticks_before,
            DEFER_TICKS,
            "NON-VACUITY: the clock must still be beating. A pump that stopped \
             ticking would also spawn nothing, and would be a much worse bug \
             than the one this gate is about"
        );
        assert!(
            crate::proxy_job::try_take_permit().is_some(),
            "and the shared permit must be untouched afterwards — the point of \
             deferring is to leave it for the proxy worker"
        );
        assert_eq!(
            LAST_CONSIDERED.load(Ordering::Relaxed),
            i64::MIN,
            "the check sits ABOVE `poll_core`, so the rotating candidate window \
             must not have advanced either (Pitfall 5's structural half)"
        );
        assert!(
            preview::render_cache_detect::heavy_segments_from(i64::MIN, HEAVY_SEGMENT_POLL_LIMIT)
                .contains(&3),
            "control: segment 3 must still be a heavy candidate, or the ticks \
             above declined an empty world"
        );

        // ---- the CONTROL: same pump, same tick, one fact different --------
        drop(planted);
        assert!(
            !crate::proxy_job::has_pending_work(),
            "the planted row must be gone, or the control below is the defer \
             arm a second time"
        );
        let gate = hold_render_gate();
        pump_tick(&dir);
        assert!(
            wait_until(Duration::from_secs(10), || {
                RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed) - idle_before == 1
            }),
            "CONTROL: with the proxy registry empty the SAME tick against the \
             SAME candidate must spawn — otherwise the zero above is not the \
             predicate's doing"
        );

        drop(gate);
        request_cancel_all();
        assert!(
            wait_until(Duration::from_secs(60), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "teardown: the control's bake must end"
        );
        clear_render_host();
    }

    /// **D-12 — the transport path did NOT inherit the defer.**
    ///
    /// The IDENTICAL arrangement to the gate above — one planted `queued` proxy
    /// row, one heavy candidate, the permit free — driven through
    /// [`poll_and_spawn`] instead of [`pump_tick`]. It spawns, exactly as it did
    /// before Phase 61 existed.
    ///
    /// Same inputs, opposite outcomes: that is what makes this a PAIR rather
    /// than two tests, and it is the only available proof that
    /// `has_pending_work()` is checked in the pump's own tick and not inside
    /// [`poll_core`] (61-RESEARCH Pitfall 5). A structural grep can say the call
    /// is not in the core today; only this can say the core's BEHAVIOUR is
    /// unchanged.
    #[test]
    fn render_cache_job_transport_path_ignores_pending_proxy_work() {
        let _lease = lease();
        settle_the_proxy_registry();

        let ctx = TestAppCtx::new();
        set_render_host(BgHost::empty());
        mark_heavy(3);

        let _planted = PlantedProxyRow::plant(DEFER_PROXY_KEY);
        assert!(
            crate::proxy_job::has_pending_work(),
            "the setup must be the DEFER gate's setup, fact for fact, or the \
             two are not a pair"
        );
        assert!(
            crate::proxy_job::try_take_permit().is_some(),
            "and the permit must be free here too"
        );

        let spawned_before = RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed);
        let idle_before = RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed);

        // The gate is held first so the render parks inside its work and the
        // assertions below are not racing a fast failure.
        let gate = hold_render_gate();
        poll_and_spawn(&ctx);
        assert!(
            wait_until(Duration::from_secs(10), || {
                RENDER_CACHE_JOBS_SPAWNED.load(Ordering::Relaxed) - spawned_before == 1
            }),
            "D-12: a transport command must still spawn while proxy work is \
             pending. The pump's fairness rule is pump-ONLY by construction, \
             and a poll that started deferring here would be a behaviour change \
             to code that shipped in Phase 59, invisible to every other test"
        );
        assert_eq!(
            RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed) - idle_before,
            0,
            "and it is attributed to the TRANSPORT path, not to the pump — \
             `idle_spawned` would be a lie otherwise"
        );
        assert!(
            crate::proxy_job::has_pending_work(),
            "the planted row is still there: the transport path did not consume \
             it, notice it, or clear it"
        );

        drop(gate);
        request_cancel_all();
        assert!(
            wait_until(Duration::from_secs(60), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "teardown: the bake must end"
        );
        clear_render_host();
    }

    /// The three REAL sources the fairness arm proxies.
    ///
    /// All 720p, so the long edge (1280) is above `proxy::cache::PROXY_LONG_EDGE`
    /// (960) and every one of them is genuinely proxiable — a source at or below
    /// the target answers `NotProxiable` in microseconds without spawning an
    /// encoder, which would make the whole measurement vacuous. Three DISTINCT
    /// files because the registry dedupes on the canonical source path.
    const FAIRNESS_SOURCES: [&str; 3] = [
        "bars_720p30_75s.mp4",
        "solid_red_720p30_20s.mp4",
        "bars_av_720p30_75s.mp4",
    ];

    /// The stated pass bar: pump-on within ~8 s of control — ONE first-time
    /// bake's worth of wall (the field measured 5.3-7.4 s), which is the
    /// documented worst case of `has_pending_work()`'s narrow race
    /// (61-RESEARCH §D2). Reported, never asserted: T-59-14-04 says a miss
    /// reaches the artifact as a NUMBER and never kills the run.
    const FAIRNESS_SLACK_BAR_S: f64 = 8.0;

    /// **WARM-03b — the proxy worker is not starved, as a NUMBER.**
    ///
    /// Arm 1 (control): three real 720p sources proxied through the real
    /// `h264_mf` sidecar into a fresh cache directory, with no pump anywhere.
    /// Arm 2: the same three sources, the same work, with the SHIPPED pump
    /// running at the SHIPPED cadence and three heavy segments marked on a real
    /// two-layer arrangement, so the pump genuinely wants the one encoder
    /// permit the proxy worker needs.
    ///
    /// The claim is a comparison of wall-clocks, so the arms are the same three
    /// files rather than three different ones: a fresh cache directory per arm
    /// is what makes both arms do the full encode, and identical inputs are what
    /// make the difference attributable to the pump.
    ///
    /// **What it asserts is only non-vacuity** — that both arms really encoded
    /// three payloads onto disk, and that the pump in arm 2 really ticked. The
    /// verdict is the printed number. `#[ignore]`d because it costs real
    /// encoder time; 61-06's gate runs it with `-- --ignored`.
    #[test]
    #[ignore = "real encoder time — run from 61-06's gate with -- --ignored"]
    fn render_cache_job_pump_proxy_fairness_timing() {
        let _lease = lease();
        settle_the_proxy_registry();

        let paths: Vec<PathBuf> = FAIRNESS_SOURCES
            .iter()
            .map(|name| PathBuf::from(crate::test_support::fixture(name)))
            .collect();

        /// Proxy every path into a FRESH cache directory and time the lot.
        /// Returns (wall seconds, committed payload count, terminal states).
        fn drive_proxies(paths: &[PathBuf]) -> (f64, usize, Vec<String>) {
            let cache = tempfile::TempDir::new().expect("a proxy cache dir of this arm's own");
            let runtime = crate::proxy_job::test_runtime();
            let _enter = runtime.enter();

            let t0 = Instant::now();
            for path in paths {
                crate::proxy_job::spawn_generation(cache.path().to_path_buf(), path.clone());
            }
            let settled = wait_until(Duration::from_secs(600), || {
                !crate::proxy_job::has_pending_work()
            });
            let elapsed = t0.elapsed().as_secs_f64();

            let states: Vec<String> = paths
                .iter()
                .map(|p| format!("{:?}", crate::proxy_job::test_row_state_for(p)))
                .collect();
            assert!(
                settled,
                "an arm never settled; its rows were {states:?} after 600 s"
            );
            // CLAUDE.md rule 3: the arm is timed against real files on disk,
            // not against a state string.
            let payloads = std::fs::read_dir(cache.path())
                .expect("the arm's cache dir is readable")
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".proxy.mp4"))
                .count();
            for path in paths {
                crate::proxy_job::test_forget(path);
            }
            (elapsed, payloads, states)
        }

        // ---- ARM 1: the control, no pump ---------------------------------
        let (control_s, control_payloads, control_states) = drive_proxies(&paths);

        // ---- ARM 2: the same work, with the SHIPPED pump running ----------
        let tmp = tempfile::TempDir::new().expect("a render cache dir of this test's own");
        let dir = tmp.path().to_path_buf();
        preview::render_cache_lookup::configure_render_cache_dir(Some(dir.clone()));
        set_render_host(two_layer_host());
        for seg in PUMP_SEGMENTS {
            mark_heavy(seg);
        }
        let ticks_before = RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed);
        let idle_before = RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed);

        let pump = spawn_pump_for_test(dir.clone());
        let (pump_on_s, pump_payloads, pump_states) = drive_proxies(&paths);
        let ticks = RENDER_CACHE_PUMP_TICKS.load(Ordering::Relaxed) - ticks_before;
        let idle = RENDER_CACHE_JOBS_SPAWNED_IDLE.load(Ordering::Relaxed) - idle_before;
        drop(pump);

        request_cancel_all();
        assert!(
            wait_until(Duration::from_secs(180), || {
                !jobs().values().any(JobEntry::is_live)
            }),
            "teardown: whatever the pump started after the proxies finished \
             must end before this test releases the lease"
        );
        clear_render_host();

        // THE VERDICT LINE — unrounded, both arms, the bar named, and the
        // miss/pass word derived from the numbers rather than asserted.
        let delta_s = pump_on_s - control_s;
        eprintln!(
            "BENCH61-PROXY-FAIRNESS control_s={control_s} pump_on_s={pump_on_s} \
             delta_s={delta_s} slack_bar_s={FAIRNESS_SLACK_BAR_S} \
             verdict={} pump_ticks={ticks} idle_spawned_in_window={idle} \
             control_payloads={control_payloads} pump_payloads={pump_payloads} \
             control_states={control_states:?} pump_states={pump_states:?}",
            if delta_s <= FAIRNESS_SLACK_BAR_S {
                "WITHIN-BAR"
            } else {
                "OVER-BAR"
            }
        );

        // NON-VACUITY ONLY. The number above is the finding; these say the
        // number is about something real.
        assert_eq!(
            control_payloads,
            FAIRNESS_SOURCES.len(),
            "the control arm must have encoded every source onto disk, or its \
             wall-clock is the time it took to decline"
        );
        assert_eq!(
            pump_payloads,
            FAIRNESS_SOURCES.len(),
            "and so must the pump-on arm — the claim is that the pump does not \
             starve the proxy worker, not that it stops it"
        );
        assert!(
            ticks > 0,
            "the pump must actually have been running during arm 2, or the two \
             arms are the same arm measured twice"
        );
    }
}
