//! Phase 58's DELIVERY half (plan 58-05): the bounded, detached, genuinely
//! cancellable background PROXY GENERATION job, and the pure poll-only status
//! read behind the C ABI.
//!
//! `crates/proxy` (plans 58-02/58-03) knows how to turn one heavy source into
//! one all-intra playback proxy. This module decides *when* that happens, *how
//! many* may happen at once, *how* it stops, and *how* the shell is allowed to
//! ask about it — and every one of those four answers is lifted, deliberately
//! unchanged, from [`crate::filmstrip_job`], the module that settled the same
//! questions for thumbnail strips two phases ago:
//!
//! * **Production is a DETACHED, semaphore-bounded background job** started
//!   from `import::import_one_path` beside the poster job, the peaks job and
//!   the filmstrip job. Nothing on the import path ever awaits it, so
//!   `project:changed` — and therefore the item's appearance in the MediaBin
//!   and on the Timeline — never waits on a transcode that costs seconds
//!   (58-CONTEXT D-09, Phase 52's D-19 reused verbatim for the third time).
//! * **Consumption is [`crate::proxy_job::run_get_proxy_status`], which cannot
//!   start work.** There is no code path from the read to `proxy::generate`; a
//!   miss is a
//!   miss. The read reaches exactly three things — the store (to resolve an id
//!   to a path), the in-memory job registry, and `proxy::cache::read_fresh`
//!   (stats plus one bounded file read) — and none of them can reach an
//!   encoder.
//!
//! # Why the status is POLLED and not pushed (58-CONTEXT D-29)
//!
//! `crates/ffi`'s notification ring carries a CLOSED set of exactly six names,
//! shared across the C ABI by both shells, and growing it is an ABI-level
//! change that 58-CONTEXT D-24 forbids while the shell cutover is owner-blocked.
//! So progress crosses as an EXPORT — `rudis_get_proxy_status`, riding Phase 50
//! D-06's existing 100 ms cold-path poll — exactly as waveform peaks (52-05)
//! and filmstrip strips (53.2-04) already do. Nothing in this module notifies
//! anybody of anything; a push channel can be added by a later phase **when a
//! shell region actually consumes it**.
//!
//! # The one thing that is NOT the filmstrip pipeline: cancellation
//!
//! A filmstrip extraction is abandoned by simply not caring about its result.
//! A proxy encode spawns an `ffmpeg` CHILD PROCESS that will happily burn a
//! hardware encoder session for minutes after nobody wants its output, so
//! 58-CONTEXT D-11 requires a real stop. The mechanism is
//! `crates/agent-gen`'s `JobRecord` shape — a per-job `Arc<AtomicBool>` latch
//! held in a registry — wired to the poll loop inside
//! `proxy::generate`, which reads the latch FIRST on every iteration and
//! **kills and reaps the child**. This module owns the latches and the two
//! callers that raise them: removing the media (`dispatch.rs`) and
//! opening/creating a project (`project.rs`).
//!
//! # Threat model
//!
//! * **T-58-05-01 (path traversal / info disclosure, ASVS V5)** —
//!   [`crate::proxy_job::run_get_proxy_status`] treats `media_id` as an OPAQUE
//!   DOMAIN ID: it is
//!   looked up in the real `Store` to obtain a path, and is never joined onto
//!   the proxy cache dir or any other filesystem path. A traversal payload is
//!   simply an id nothing matches. This is `filmstrip_job::read_strip`'s
//!   T-53.2-13 discipline, applied verbatim, and asserted by
//!   `a_traversal_payload_is_just_an_id_that_matches_nothing`.
//! * **T-58-05-02 (resource exhaustion)** —
//!   [`crate::proxy_job::MAX_CONCURRENT_PROXY_JOBS`]. A
//!   folder import of twenty 4K sources must not open twenty hardware encoder
//!   sessions. Asserted, not assumed, by
//!   `two_spawns_never_run_two_encodes_at_the_same_instant`.
//! * **T-58-05-03 (a leaked permit zeroing the pool forever)** — the permit is
//!   dropped on every path including the `spawn_blocking` panic arm, exactly as
//!   `filmstrip_job::spawn_extraction` does and for the same reason: with a cap
//!   of ONE, a single leaked permit would zero the pool for the rest of the
//!   process's life.
//! * **T-58-05-04 (a cancel that only flags)** — proven through THIS layer by
//!   `cancel_mid_encode_leaves_no_proxy_and_no_residue`, which waits until the
//!   encode is provably in flight (its in-flight file exists on disk) before
//!   raising the latch, and then asserts the cache directory is as it was.
//! * **T-58-05-05 (silent failures)** — every non-trivial outcome reaches
//!   stderr with the source path, and `Created` carries 58-CONTEXT D-23's
//!   measured `wall_ms`/`bytes` into the log rather than discarding them.
//! * **Unbounded registry growth** — the registry is capped
//!   ([`crate::proxy_job::MAX_TRACKED_PROXY_JOBS`]); see that constant for why
//!   forgetting a FINISHED job is lossless.
//! * **A STRANDED LIVE ROW (58-REVIEW WR-04)** — a `queued`/`running` row with
//!   no task behind it makes [`spawn_generation`] a permanent no-op for that
//!   source, for the life of the PROCESS, because the registry is global while
//!   the runtime is per-ctx and nothing ever reaps a live row. `RowGuard` moves
//!   the row's lifetime into the task itself, so every exit — including the task
//!   being dropped before it was ever polled — clears it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use crate::AppCtx;
use proxy::cache;
use proxy::GenerateOutcome;

/// How many proxy encodes may be running at the same instant, process-wide.
///
/// **One**, and — unlike [`crate::filmstrip_job::MAX_CONCURRENT_FILMSTRIP_JOBS`]'s
/// identical value — the reason is not memory. A proxy encode occupies a
/// **hardware encoder session** (`h264_mf`, Windows Media Foundation), and the
/// same silicon is already running the preview path's hardware DECODE fleet,
/// which Phase 57 bounded at `MAX_HW_SESSIONS = 3`. 58-CONTEXT D-10 states the
/// priority order plainly: *playback is the privileged consumer*; a proxy
/// transcode that costs the user frames is a regression, not a feature. Serial
/// generation, at the below-normal process priority 58-01's
/// `engine::spawn_proxy_encode` already assigns the child, is that order
/// expressed as a bound.
///
/// The second reason is throughput honesty. Proxy generation is not a race:
/// 58-03 MEASURED 479 ms for a 5 s 720p source and 4872 ms for a 240 s one, all
/// far below the length of the footage each serves, so two concurrent encodes
/// would buy a beginner nothing they could perceive while doubling the
/// contention with the thing they can.
///
/// Same DoS-cap SHAPE the rest of this codebase already uses — `audio_sync`'s
/// five-minute sync window, `whisper`'s window cap, `waveform_job`'s and
/// `filmstrip_job`'s permit counts: a bound that is a named constant with a
/// test on it, rather than a property of whatever the runtime happens to do.
pub const MAX_CONCURRENT_PROXY_JOBS: usize = 1;

/// The permits. One process-wide semaphore, deliberately: the resource being
/// bounded (the machine's hardware encoder, and the FFmpeg sidecar processes the
/// encode spawns) is process-wide too, so a per-ctx bound would multiply with
/// the number of contexts and stop being a bound.
static SEM: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_PROXY_JOBS));

/// Hard cap on how many source paths the registry remembers at once.
///
/// Without it the map grows for the life of the process — every distinct
/// imported video is a new key — and its keys are strings derived from
/// caller-supplied paths, which is exactly the shape that should carry a bound.
///
/// Forgetting a FINISHED job is lossless, which is what makes the eviction
/// policy below safe: [`run_get_proxy_status`] falls through a registry miss to
/// `proxy::cache::read_fresh`, so a forgotten `ready` answers `ready` from the
/// cache itself, and a forgotten `not_needed`/`failed`/`cancelled` answers
/// `none` — "there is no proxy for this", which is true. Only jobs that are
/// still queued or running are load-bearing, and those are never evicted.
pub const MAX_TRACKED_PROXY_JOBS: usize = 512;

/// A job has been accepted but has not taken a permit yet.
const STATE_QUEUED: &str = "queued";
/// A job holds a permit and its encode is in flight.
const STATE_RUNNING: &str = "running";
/// A current proxy exists on disk for this source.
const STATE_READY: &str = "ready";
/// The source is at or below the proxy's target long edge, or is not a
/// cacheable file: it will never be proxied, and that is a policy answer rather
/// than a failure (58-CONTEXT D-05).
const STATE_NOT_NEEDED: &str = "not_needed";
/// The encode failed. Playback is unaffected — the resolver falls back to the
/// original, silently and per-clip (58-CONTEXT D-17).
const STATE_FAILED: &str = "failed";
/// The latch was raised while the encode was in flight; the child was killed
/// and nothing was committed.
const STATE_CANCELLED: &str = "cancelled";
/// Nothing is known about this source: no job has run in this process and no
/// current proxy is on disk.
const STATE_NONE: &str = "none";

/// What a status poll looks like on the wire.
///
/// `state` is a plain string and the struct has exactly one field — both
/// deliberate. `{"state":"running"}` is a shape a C# DTO deserializes without
/// ceremony, and a string rather than an integer or a tagged union means adding
/// a state later cannot renumber anything already shipped. The field name is a
/// WIRE CONTRACT with no compiler on the other side of the ABI to catch a
/// rename.
///
/// The seven values are `"queued"`, `"running"`, `"ready"`, `"not_needed"`,
/// `"failed"`, `"cancelled"` and `"none"` — written out here as the literals
/// they cross the ABI as, rather than as links to the private constants that
/// produce them, because this paragraph IS the contract a C# consumer reads. A
/// consumer that only wants "is there a proxy yet?" compares against `ready` and
/// treats every other value as "not yet, keep playing the original" — which is
/// precisely what the resolver already does on its own.
///
/// Phase 71 (TRUST-03) appended ONE optional field, `progress_permille`, and it
/// is PRESENT ONLY on `running`: every other state serializes byte-identically
/// to the one-field shape above (`{"state":"none"}` stays `{"state":"none"}`),
/// so a consumer that reads only `state` is unaffected.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProxyStatusPayload {
    pub state: String,
    /// How far the running encode has got, in thousandths of the source's
    /// probed duration — ffmpeg's own `out_time_us` read off the job's atomic
    /// (`proxy::generate_with_progress`). 0..=999 and monotonic per job: 999 is
    /// the ceiling while running, because `ready` — and only `ready` — means
    /// done. `None` (and therefore ABSENT from the JSON) on every state but
    /// `running`: a finished, failed or cancelled job carries no number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_permille: Option<u32>,
}

/// The registry's per-source row: the last known state plus the cancel latch
/// the encode's poll loop observes.
///
/// `crates/agent-gen`'s `JobRecord` shape, carried across by subject: an
/// `AtomicBool` behind an `Arc` (not a channel) because cancellation is a
/// one-way latch with no payload, and the loop is already waking on its own
/// timer.
#[derive(Debug)]
struct JobEntry {
    /// Which spawn owns this row (58-REVIEW **WR-04**). A key alone is not an
    /// identity: a finished row can be REPLACED by a later spawn for the same
    /// source, and the earlier task must not then write to — or clear — its
    /// successor's row. Every write goes through this check.
    epoch: u64,
    state: &'static str,
    cancel: Arc<AtomicBool>,
    /// The encode's own progress (Phase 71, TRUST-03): 0 at registration, then
    /// written ONLY by `proxy::generate_with_progress` (a `fetch_max`, so it is
    /// monotonic at the source) and read by [`run_get_proxy_status`] while the
    /// row is `running`.
    progress: Arc<AtomicU32>,
}

impl JobEntry {
    /// Queued and running are the two states a later spawn must not duplicate
    /// and the eviction sweep must not forget.
    fn is_live(&self) -> bool {
        self.state == STATE_QUEUED || self.state == STATE_RUNNING
    }
}

/// Hands out the per-row identity in [`JobEntry::epoch`]. Monotonic and
/// process-global, like the registry it labels.
static ROW_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Owns one registry row's LIFETIME, so the row cannot outlive the task that
/// created it (58-REVIEW **WR-04**).
///
/// `spawn_generation` registers a `queued` row BEFORE spawning, and dedupes
/// later spawns on [`JobEntry::is_live`]. Every path that leaves a live row
/// behind with no task running therefore makes proxy generation for that source
/// a **permanent no-op** for the life of the process — the source can never get
/// a proxy again, `run_get_proxy_status` answers `queued`/`running` forever, and
/// [`evict_finished_if_full`] can never reclaim the row because live rows are
/// exempt. There is no timeout and no reaper, so nothing would ever correct it.
///
/// Three such paths existed:
///
/// 1. `SEM.acquire()` returning `Err` (documented unreachable — but it was the
///    one arm that returned without clearing the row);
/// 2. the detached task being **dropped mid-await**, which is exactly what a
///    runtime shutdown does to it — and since [`JOBS`] is process-global while
///    the runtime is per-ctx, that stranded every queued job for any LATER ctx
///    in the same process;
/// 3. a task wedged forever on a full stderr pipe (58-REVIEW WR-01, fixed).
///
/// A `Drop` impl closes 1 and 2 by construction: Rust runs it on the normal
/// return, on the early return, and on the future being dropped.
///
/// It only clears a row that is still LIVE and still ITS OWN (the epoch): a
/// FINISHED row was set deliberately and is the answer a later poll should read,
/// and a row belonging to a newer spawn belongs to that spawn.
///
/// **Where it is constructed matters as much as what it does.** It is built by
/// [`spawn_generation`] *before* the spawn and captured by move, NOT declared as
/// the future's first statement — because a guard inside the body does not exist
/// until the first poll, and a runtime shutting down under load drops tasks that
/// were never polled. `a_runtime_shutdown_clears_a_row_whose_task_was_never_polled`
/// is that case, made deterministic with a current-thread runtime.
///
/// MUTATION-VERIFIED 2026-08-03: neutering this `Drop` body turns both
/// shutdown gates red at `Some("queued")`.
struct RowGuard {
    key: String,
    epoch: u64,
}

impl RowGuard {
    fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for RowGuard {
    fn drop(&mut self) {
        let mut jobs = jobs();
        let mine_and_live = jobs
            .get(&self.key)
            .is_some_and(|row| row.epoch == self.epoch && row.is_live());
        if mine_and_live {
            jobs.remove(&self.key);
        }
    }
}

/// Every proxy generation this process knows about, keyed by the CANONICAL
/// source path.
///
/// Keyed by path and not by `media_id` on purpose: a proxy is an artifact of the
/// MEDIA FILE, so two bin items over one file (a re-import, a relink) share one
/// proxy and must share one job — which is also why a trim, a split and a
/// duplicate cost zero new encoding.
static JOBS: LazyLock<Mutex<HashMap<String, JobEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The registry guard. A poisoned registry is RECOVERED rather than propagated:
/// every value in it is a small owned struct that cannot be observed
/// half-written, and a status poll that panicked because some unrelated job's
/// task unwound would be a strictly worse failure than any proxy problem this
/// module exists to solve.
fn jobs() -> MutexGuard<'static, HashMap<String, JobEntry>> {
    JOBS.lock().unwrap_or_else(|p| p.into_inner())
}

/// The registry key for a source file.
///
/// Canonicalized so the same file reached by two different spellings (a relative
/// path, a `..` hop, a different case on Windows) resolves to ONE job — the same
/// normalization `proxy::cache::key_for` performs to derive the cache file name,
/// so the registry and the cache cannot disagree about which file is which.
/// A path that cannot be canonicalized (it was deleted, or the volume went away)
/// degrades to its own literal text: such a job will never match a later lookup,
/// which costs a stale registry row and nothing else.
fn job_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Overwrite one job's state.
///
/// A no-op for a key the registry has forgotten, AND a no-op for a row that now
/// belongs to a LATER spawn (58-REVIEW WR-04's epoch): a task that has already
/// published its terminal state must never reach back and overwrite its
/// successor's `queued`.
fn set_state(key: &str, epoch: u64, state: &'static str) {
    if let Some(row) = jobs().get_mut(key) {
        if row.epoch == epoch {
            row.state = state;
        }
    }
}

/// Start a DETACHED, semaphore-bounded background proxy generation for `path`,
/// writing into the proxy cache at `dir`.
///
/// Returns `()` IMMEDIATELY and can never propagate a failure into the import
/// path: every error path logs to stderr and returns, exactly like the poster
/// block, the peaks block and the filmstrip block it sits beside. Losing a proxy
/// costs a clip that plays from its original (58-CONTEXT D-17), never an import.
///
/// Takes OWNED values only — no `ctx`, no `SharedStore`, no borrow — so
/// `import_one_path`'s lock-discipline invariant (no store guard alive across a
/// spawn) holds by construction rather than by review, and the call adds no
/// suspension point to the import path.
///
/// Calling it twice for a source whose job is still queued or running is a
/// NO-OP. That is not merely an optimization: two concurrent generations of one
/// source would race for the same cache stem, and while
/// `proxy::generate`'s per-call temp nonce makes that race safe, it would still
/// burn a second encoder session to produce a byte-identical file.
pub fn spawn_generation(dir: PathBuf, path: PathBuf) {
    let key = job_key(&path);

    // Dedupe + register, under ONE guard so two importers cannot both decide
    // they are the first.
    let (cancel, progress, epoch) = {
        let mut jobs = jobs();
        if jobs.get(&key).is_some_and(JobEntry::is_live) {
            return;
        }
        evict_finished_if_full(&mut jobs);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(0));
        let epoch = ROW_EPOCH.fetch_add(1, Ordering::Relaxed);
        jobs.insert(
            key.clone(),
            JobEntry {
                epoch,
                state: STATE_QUEUED,
                cancel: Arc::clone(&cancel),
                progress: Arc::clone(&progress),
            },
        );
        (cancel, progress, epoch)
    };

    // WR-04: from here on the ROW BELONGS TO THE GUARD, and the guard belongs to
    // whoever is going to do the work.
    //
    // It is constructed HERE, before the spawn, and MOVED into the future —
    // NOT declared as the future's first statement. That difference is
    // load-bearing and was found by measurement: a guard created inside the
    // async body only exists once the task has been POLLED, and a runtime
    // shutting down under load drops tasks that were never polled at all. Those
    // are precisely the tasks WR-04 is about, so a guard that starts at the
    // first poll would miss the case it exists for. As a captured upvar it is
    // part of the future's state from the instant the future exists.
    let row = RowGuard { key, epoch };

    // `tokio::spawn` PANICS outside a runtime. Every real call site is inside
    // one (`import_one_path` is `async`, and is polled by the host's runtime on
    // every import surface), but this function is `pub` and a panic here would
    // be a panic in the import path — the one thing this module promises never
    // to do. Degrade to "no proxy" instead; `row` drops on the way out and takes
    // the queued row with it, so a later spawn from inside a runtime is not
    // deduped against a job that never existed.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        eprintln!(
            "proxy: no tokio runtime; skipping background proxy for {}",
            path.display()
        );
        return;
    };

    handle.spawn(async move {
        // `row` is captured by move; naming it here is what makes the capture
        // happen, and it covers every exit below — the early return, the normal
        // return, the task being dropped mid-await, AND the task being dropped
        // before it was ever polled.
        //
        // T-58-05-02: take a permit BEFORE reaching the blocking pool, so N
        // queued jobs cost N cheap futures rather than N parked blocking threads
        // each waiting for a hardware encoder session to free up.
        let permit = match SEM.acquire().await {
            Ok(p) => p,
            // Unreachable: nothing ever closes this semaphore. Handled anyway
            // because the alternative is an `unwrap` in a detached task — and
            // now the row is cleared on the way out rather than stranded.
            Err(_) => return,
        };
        set_state(row.key(), epoch, STATE_RUNNING);

        let for_log = path.clone();
        let joined = tokio::task::spawn_blocking(move || {
            // `generate` BLOCKS for the whole encode (58-03 measured 0.5 s to
            // ~5 s on committed fixtures, longer on real 4K), which is why it
            // runs here and never on a runtime worker.
            //
            // Phase 71 (TRUST-03): the `_with_progress` form publishes the
            // encoder's own `out_time_us` as a permille into this row's atomic,
            // which the status poll reads without taking any other lock.
            proxy::generate::generate_with_progress(&path, &dir, &cancel, &progress)
        })
        .await;

        let state = match joined {
            Ok(GenerateOutcome::Created { wall_ms, bytes }) => {
                // 58-CONTEXT D-23: the generation cost is MEASURED and written
                // down, never hidden. A proxy that takes longer to build than
                // the session it serves is a finding worth reading in a log.
                eprintln!(
                    "proxy: generated for {} in {wall_ms} ms ({bytes} bytes)",
                    for_log.display()
                );
                // 59-REVIEW **WR-02**: a completed generation changes what
                // `preview::decode_source` answers for every clip over this
                // media, and it dispatches NOTHING — so no consumer memoizing
                // that answer against the store's mutation counter can see it.
                // Announce it on the seam's own axis instead.
                //
                // Placed on `Created` specifically: `AlreadyFresh` changed
                // nothing, and the two refusal arms committed nothing. A
                // `Created` also runs the proxy cache's byte-budget prune, which
                // can EVICT some other media's proxy — the other direction of
                // the same change, and covered by the same bump.
                preview::decode_source::note_decode_answers_changed();
                STATE_READY
            }
            Ok(GenerateOutcome::AlreadyFresh) => STATE_READY,
            Ok(GenerateOutcome::NotProxiable) => STATE_NOT_NEEDED,
            Ok(GenerateOutcome::Cancelled) => {
                eprintln!("proxy: generation cancelled for {}", for_log.display());
                STATE_CANCELLED
            }
            Ok(GenerateOutcome::Failed(e)) => {
                eprintln!("proxy: generation failed for {}: {e}", for_log.display());
                STATE_FAILED
            }
            Err(e) => {
                // Mirrors the poster block's own panic arm (`import.rs`): a
                // panicking job is logged and dropped, never re-raised into a
                // caller that has long since returned.
                eprintln!(
                    "proxy: generation task panicked for {}: {e}",
                    for_log.display()
                );
                STATE_FAILED
            }
        };
        set_state(row.key(), epoch, state);

        // The permit drops HERE, on every path including the panicking one —
        // `spawn_blocking` catches the unwind and hands it back as a
        // `JoinError`, so this task itself never unwinds and the permit is never
        // leaked. With a cap of ONE that is not a nicety: a single leaked permit
        // would zero the pool for the rest of the process's life.
        drop(permit);
    });
}

/// Drop FINISHED rows once the registry is at its cap, so it cannot grow
/// without bound across a long session.
///
/// Live rows (queued/running) are never touched: they own a latch a cancel may
/// still need to reach. If every row is live — which needs
/// [`MAX_TRACKED_PROXY_JOBS`] simultaneous in-flight jobs against a pool of
/// [`MAX_CONCURRENT_PROXY_JOBS`] — the map is allowed to exceed the cap rather
/// than lose a cancellation handle.
fn evict_finished_if_full(jobs: &mut HashMap<String, JobEntry>) {
    if jobs.len() < MAX_TRACKED_PROXY_JOBS {
        return;
    }
    jobs.retain(|_, row| row.is_live());
}

/// Raise the cancel latch for `path`'s job, if it has one (58-CONTEXT D-11).
///
/// Best-effort and idempotent: an unknown path, an already-finished job and a
/// second call are all no-ops. Cancellation is not instant — the latch is read
/// at the top of `proxy::generate`'s poll loop, so the child dies within one
/// poll interval (50 ms) plus the kill, which 58-03 MEASURED end-to-end at
/// 64 ms.
pub fn request_cancel_for_path(path: &Path) {
    let key = job_key(path);
    if let Some(row) = jobs().get(&key) {
        row.cancel.store(true, Ordering::Relaxed);
    }
}

/// Raise EVERY cancel latch (58-CONTEXT D-11's "closing the project cancels the
/// job").
///
/// Latches for already-finished jobs are raised too, which is harmless: nothing
/// reads them again. Doing it that way — rather than filtering to live rows —
/// keeps this function free of any assumption about which states are terminal.
pub fn cancel_all() {
    for row in jobs().values() {
        row.cancel.store(true, Ordering::Relaxed);
    }
}

/// THE PURE READ: what is known about `media_id`'s proxy, right now.
///
/// **It is not possible for this function to start a generation.** There is no
/// code path from here to `proxy::generate`, and that is deliberate and
/// load-bearing: this is the shell's only route to proxy state, so "a poll never
/// triggers work" holds even if a future caller polls it in a tight loop. Do not
/// add a "generate it if missing" fallback here; put it on the write path, which
/// is [`spawn_generation`].
///
/// `Ok(None)` means the id is unknown to the store — the ONLY none-case, and it
/// serializes as exactly `{"Ok":null}`. A known id always answers with a state,
/// because "there is no proxy and none is coming" is real information
/// (`"none"`) rather than an absence. The only `Err` this can produce is a
/// host that cannot resolve its own cache directory, which is a genuine host
/// fault and is unreachable on the C ABI host (`FfiAppCtx`'s `app_cache_dir` is
/// an infallible clone).
///
/// # `media_id` is an ID, not a path (ASVS V5, T-58-05-01)
///
/// The caller's string is looked up in the real `Store`; the path — and through
/// it the cache key, the registry key and every filesystem access this function
/// makes — comes from the domain model. The id itself is NEVER joined onto the
/// proxy cache dir or any other path, so `"../../../Windows/System32/config/SAM"`
/// is just an id that matches nothing.
pub fn run_get_proxy_status<C: AppCtx>(
    ctx: &C,
    media_id: &str,
) -> Result<Option<ProxyStatusPayload>, String> {
    // Scope the guard: the lock is held for one lookup and a `String` clone, and
    // is released BEFORE any filesystem work. A poisoned store degrades to a
    // miss rather than unwinding again.
    let path = {
        let Ok(store) = ctx.store().lock() else {
            return Ok(None);
        };
        match store.media_item(media_id) {
            Some(item) => PathBuf::from(&item.path),
            None => return Ok(None),
        }
    };

    // A live or remembered job is the freshest answer available — EXCEPT for
    // `ready`, which is a claim about a FILE and must be confirmed against that
    // file (58-REVIEW WR-05).
    //
    // The registry remembers `ready` forever, but `prune_bytes` can evict the
    // pair at any time to keep the byte budget: any later commit for any other
    // source may take this one's bytes. The resolver handles that correctly and
    // silently (D-17's total fallback), so PLAYBACK is never wrong — but this
    // getter would keep answering `ready` about a proxy that no longer exists.
    // The module doc reasoned only about a FORGOTTEN `ready` (which self-corrects
    // through `read_fresh` below); a REMEMBERED one could lie indefinitely.
    //
    // Every other state is answered from the registry, because the cache cannot
    // express any of them: `queued`/`running` are about a job, and
    // `failed`/`cancelled`/`not_needed` are about an outcome that produced no
    // file by design.
    let key = job_key(&path);
    let remembered = jobs()
        .get(&key)
        .map(|row| (row.state, row.progress.load(Ordering::Relaxed)));
    if let Some((state, progress)) = remembered {
        if state != STATE_READY {
            // TRUST-03: only `running` carries a number, clamped to 999 so the
            // wire can never claim "done" before the state does.
            let progress_permille = (state == STATE_RUNNING).then(|| progress.min(999));
            return Ok(Some(ProxyStatusPayload {
                state: state.to_string(),
                progress_permille,
            }));
        }
    }

    // Registry miss, or a `ready` awaiting confirmation: ask the cache. This is
    // what makes a proxy generated by a PREVIOUS run of the app report `ready` in
    // this one, and it is the same stats-plus-one-bounded-read lookup the resolve
    // path uses — no subprocess, no decode, and still no way to start one.
    let dir = proxy_dir(ctx)?;
    let state = if cache::read_fresh(&path, &dir).is_some() {
        STATE_READY
    } else {
        STATE_NONE
    };
    Ok(Some(ProxyStatusPayload {
        state: state.to_string(),
        progress_permille: None,
    }))
}

/// Re-arm proxy generation for a project's media that is heavy enough to want a
/// proxy (58-REVIEW **WR-05**).
///
/// # The gap this closes
///
/// Before this, [`spawn_generation`] had exactly ONE call site —
/// `import::import_one_path` — so a proxy existed only for media imported in
/// THIS session. Two consequences, both real:
///
/// * a project opened from disk whose media was imported yesterday (or before
///   Phase 58 existed) got no proxies at all, which is exactly the
///   "reopen the project I made yesterday" flow;
/// * `cache::prune_bytes` enforces the byte budget by evicting whole pairs, so
///   in a library larger than the budget the proxy for file 1 is evicted to make
///   room for file N — and nothing would ever build it again.
///
/// # What it deliberately does NOT do
///
/// It is not a resolve-miss trigger. `resolve_decode_source` runs on the
/// playback producer thread with no ctx and no runtime, and
/// [`run_get_proxy_status`] is contractually incapable of starting work — both
/// by design (D-16/D-18). A trigger belongs on a WRITE path, and project-open is
/// the write path that exists.
///
/// It reaches only the heaviness predicate's **resolution rung**:
/// `MediaBinItem` carries `width`/`height` but neither `bit_rate` nor `vcodec`,
/// so a high-bitrate 1080p camera original imported by an earlier session is not
/// re-armed. Passing `None` for both is the honest input, not a guess — and
/// [`proxy::heaviness::needs_proxy`] treats an absent bitrate as failing the
/// bitrate rung rather than as rescuing anything, so the subset is conservative
/// in the safe direction. Widening it means carrying those two fields on the
/// domain item, which is a `.rud` document change and squarely out of this
/// phase (D-24).
///
/// # Cost, and why it is small
///
/// Every re-armed source with a current proxy short-circuits inside
/// `proxy::generate` at its SECOND early exit — `cache::read_fresh`, stats plus
/// one bounded read — **before** the `ffprobe` subprocess. So the steady-state
/// cost of reopening a project whose proxies are all warm is one cheap task per
/// heavy item, serialized behind [`MAX_CONCURRENT_PROXY_JOBS`].
///
/// # Known narrow gap, recorded rather than hidden
///
/// `run_open_project` raises every cancel latch before it loads, and a job that
/// is still winding down keeps its `running` row for up to one poll interval.
/// A source present in BOTH the outgoing and incoming project can therefore be
/// deduped against that dying row and miss this re-arm. It costs one session of
/// playing the original for that clip; the next open re-arms it.
///
/// Best-effort throughout: a host that cannot resolve its cache directory logs
/// and returns, exactly like the import trigger.
pub(crate) fn rearm_project_media<C: AppCtx>(ctx: &C, sources: &[(PathBuf, u32, u32)]) {
    if sources.is_empty() {
        return;
    }
    let dir = match proxy_dir(ctx) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("proxy: no cache directory, skipping the project re-arm: {e}");
            return;
        }
    };
    // Same idempotent overwrite `import_one_path` performs beside its own spawn:
    // it makes a proxy usable in THIS session without a transport command having
    // happened first.
    preview::decode_source::configure_proxy_cache_dir(dir.clone());

    let heavy: Vec<PathBuf> = sources
        .iter()
        .filter(|(_, width, height)| proxy::heaviness::needs_proxy(*width, *height, None, None))
        .map(|(path, _, _)| path.clone())
        .collect();
    if heavy.is_empty() {
        return;
    }
    let spawn_all = move || {
        for path in heavy {
            // `spawn_generation`'s dedupe and `generate`'s freshness check both
            // make this a no-op when a proxy already exists.
            spawn_generation(dir.clone(), path);
        }
    };

    // D-63-04-01 (Phase 71): `spawn_generation` needs an ambient tokio runtime
    // and degrades to a logged no-op without one. The C-ABI host calls
    // `run_open_project` with NO runtime entered, so before this fix a project
    // opened from disk queued no proxy work at all. The host's runtime is a
    // capability of the ctx (`AppCtx::block_on`), so spawn from inside it: the
    // detached tasks outlive this call because both `FfiAppCtx` and
    // `TestAppCtx` keep that runtime alive in a `OnceLock`.
    //
    // The `try_current` guard is mandatory, not tidiness: a nested
    // `Runtime::block_on` PANICS, and the agent-tool route already runs under
    // one — so when a runtime is ambient, spawn directly on it.
    if tokio::runtime::Handle::try_current().is_ok() {
        spawn_all();
    } else {
        ctx.block_on(async move { spawn_all() });
    }
}

/// The proxy cache directory, WITHOUT creating it.
///
/// `filmstrip_job::filmstrip_dir`'s exact shape and for its exact reason: the
/// read path runs on a 100 ms poll and must have no side effects, so the join
/// lives here, once, and the `create_dir_all` lives on the write path — which
/// for proxies is inside `proxy::generate` itself (it creates `dir` before
/// spawning the encode), so no second helper is needed here at all.
pub(crate) fn proxy_dir<C: AppCtx>(ctx: &C) -> Result<PathBuf, String> {
    Ok(ctx.app_cache_dir()?.join(cache::PROXY_CACHE_DIR_NAME))
}

// ---------------------------------------------------------------------------
// Test-only surface, shared with `import`'s trigger gates
// ---------------------------------------------------------------------------

/// Serializes every test in this crate that starts a REAL proxy encode.
///
/// [`SEM`] and [`JOBS`] are process-global by design, and `cargo test` runs this
/// binary's tests on as many threads as the machine has cores. Two
/// unsynchronized tests would queue behind each other's encoder permit and read
/// each other's registry rows — the concurrency gate in particular would see a
/// neighbour's job and could not tell it from its own. One lease, taken at the
/// top of the test, is the only correct shape (the `test_support::gpu_lease`
/// precedent, same reasoning, different resource).
#[cfg(test)]
static ENCODER: Mutex<()> = Mutex::new(());

/// Take the crate-wide proxy-encoder lease. Call ONCE, at the top of the test.
///
/// Poison is recovered, not propagated: a test that fails while holding this
/// must report ITS OWN assertion, not a poison panic in the next five.
#[cfg(test)]
pub(crate) fn encoder_lease() -> MutexGuard<'static, ()> {
    ENCODER.lock().unwrap_or_else(|p| p.into_inner())
}

/// Take one permit from the encoder pool, or answer `None` because it is busy.
///
/// # This is the SHARED admission (59-CONTEXT D-23)
///
/// [`SEM`] bounds the machine's hardware video ENCODER, and since Phase 59 the
/// proxy worker is not the only thing that drives it: `render_cache_job`'s
/// background segment renders run `h264_mf` on the same silicon as the preview
/// path's hardware DECODE fleet, which Phase 57 bounded at `MAX_HW_SESSIONS = 3`.
/// Two independent `Semaphore(1)`s would be two encoders, and 58-CONTEXT D-10's
/// priority order — *playback is the privileged consumer* — would quietly stop
/// holding.
///
/// So `crate::render_cache_job` takes its admission from HERE rather than
/// building a pool of its own. The sharing is therefore a shared static, not a
/// convention someone forgets, and
/// `render_cache_job::tests::render_cache_job_shared_admission` proves it in
/// both directions: a render defers while a proxy encode holds this permit, and
/// this function answers `None` while a render holds it.
///
/// It is `pub(crate)` and NOT `#[cfg(test)]` for that reason — 59-08 promoted
/// it. The non-blocking `try_` form is the whole contract: a caller that cannot
/// get in **defers**, it never queues, because a queued background encode is a
/// second encoder waiting for its turn.
///
/// It was, and still is, also the tests' instrument: with the only permit held,
/// a job spawned afterwards is provably STUCK in `queued`, which turns "the
/// import returned before the proxy finished" from a race that usually wins into
/// a fact. `filmstrip_job`'s own D-09 gate holds its extraction open for exactly
/// the same reason. In a test, only meaningful while the caller holds
/// [`encoder_lease`].
pub(crate) fn try_take_permit() -> Option<tokio::sync::SemaphorePermit<'static>> {
    SEM.try_acquire().ok()
}

/// **Phase 61 (D-10/D-11): is any proxy job queued or running right now?**
///
/// The idle render-cache pump asks this BEFORE it reaches [`try_take_permit`]
/// and skips the whole tick when the answer is `true`. `try_take_permit`'s
/// non-blocking defer is not enough on its own: it prevents two SIMULTANEOUS
/// encodes, but not a background bake winning the permit in the gap between two
/// proxies. One bake is 5.3-7.4 s of wall, so a ten-file import can lose 50-70 s
/// that way — starvation, measured, which is what this predicate exists to stop.
///
/// It is the SAME `is_live` predicate this module's own dedupe and eviction use,
/// over the SAME `jobs()` mutex — never a second "is the worker busy" flag,
/// which would be a second source of truth that can drift from the registry.
/// A map scan under one uncontended lock, called at 2 Hz by one thread, never
/// per frame.
///
/// # Racy, narrowly, and it does not matter
///
/// [`spawn_generation`] registers its `queued` row SYNCHRONOUSLY, under one
/// `jobs()` guard, BEFORE it spawns anything — so the only window in which this
/// can answer `false` for an import that is about to happen is the sub-
/// millisecond gap between the caller deciding to import and the call landing.
/// The pump's tick is 500 ms. Worst case that costs ONE bake's duration on the
/// FIRST file of a batch, and the spawned proxy then `acquire().await`s (a
/// waiting acquire, not `try_`) rather than failing.
///
/// **Pump-only by construction (D-12).** It is deliberately NOT called from
/// `poll_core`: inside the core it would also apply to the transport-triggered
/// path, whose behaviour this phase must leave byte-unchanged.
pub(crate) fn has_pending_work() -> bool {
    jobs().values().any(JobEntry::is_live)
}

/// The state string for one media id, for tests in sibling modules that need to
/// read it without going through the `Result<Option<..>>` envelope.
#[cfg(test)]
pub(crate) fn test_state_of<C: AppCtx>(ctx: &C, media_id: &str) -> String {
    run_get_proxy_status(ctx, media_id)
        .expect("a test ctx always resolves a cache dir")
        .expect("the id is registered in the store")
        .state
}

/// Forget the registry row for `path`, so a later test in the same process
/// starts from a clean slate.
#[cfg(test)]
pub(crate) fn test_forget(path: &Path) {
    jobs().remove(&job_key(path));
}

/// **Phase 61 test seam (WARM-03): plant a `queued` row under a literal key,
/// so a sibling module's test can make [`has_pending_work`] answer `true`
/// WITHOUT running a real generation.**
///
/// [`JobEntry`]'s fields are module-private, so `render_cache_job`'s tests
/// cannot build one, and the alternative — actually spawning a proxy — would
/// take the SHARED encoder permit and destroy the very thing the discriminating
/// pair is trying to isolate. The pair's whole claim is *"pending proxy work
/// alone parks the pump, with the permit still free"*: a planted row leaves the
/// permit untouched, so a pump that declined for admission reasons instead of
/// for this predicate would be caught rather than accommodated.
///
/// It takes a LITERAL key rather than a path on purpose. [`job_key`]
/// canonicalizes, so a path key would depend on a file existing; a caller that
/// only needs the predicate to answer `true` should not have to own a file to
/// say so. Nothing else in this module cares what the key spells — the dedupe
/// compares whole keys, and a planted key matches no real source.
///
/// Paired with [`remove_planted_row_for_test`], which the planting test MUST
/// call: a live row left behind parks every pump tick in every test that runs
/// after it (D-10), which is the loudest possible cross-test flake.
#[cfg(test)]
pub(crate) fn plant_queued_row_for_test(key: &str) {
    let epoch = ROW_EPOCH.fetch_add(1, Ordering::Relaxed);
    jobs().insert(
        key.to_string(),
        JobEntry {
            epoch,
            state: STATE_QUEUED,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicU32::new(0)),
        },
    );
}

/// Undo [`plant_queued_row_for_test`]. See its doc for why this is mandatory.
#[cfg(test)]
pub(crate) fn remove_planted_row_for_test(key: &str) {
    jobs().remove(key);
}

/// One registry row's state string, by key — for sibling-module tests that
/// watch a REAL generation settle without registering a `MediaBinItem` first.
///
/// [`test_state_of`] goes through the shipped getter, which resolves a
/// `media_id` through the store; a caller that only has a PATH would have to
/// name a mutation type to register one, and `render_cache_job`'s own D-26
/// source scan forbids that file from naming one at all. This reads the same
/// registry the getter reads, one layer lower.
#[cfg(test)]
pub(crate) fn test_row_state_for(path: &Path) -> Option<&'static str> {
    jobs().get(&job_key(path)).map(|row| row.state)
}

/// A multi-thread tokio runtime for sibling-module tests that drive
/// [`spawn_generation`] (which detaches a task and then `spawn_blocking`s, so a
/// current-thread runtime would serialize the very overlap being measured).
///
/// It lives HERE rather than in the caller for a documented reason:
/// `render_cache_job.rs` deliberately names no runtime-timer or task-spawn API
/// anywhere in its source, in code OR prose, so that rule stays checkable by a
/// plain grep with no comment-blanking step (61-02's own finding). A test over
/// there that needed a runtime would have re-introduced exactly the token that
/// rule exists to keep out.
#[cfg(test)]
pub(crate) fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .build()
        .expect("test runtime builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixture, TestAppCtx};
    use std::time::{Duration, Instant};

    // -----------------------------------------------------------------------
    // Harness
    // -----------------------------------------------------------------------

    /// Multi-thread, not current-thread: [`spawn_generation`] detaches a task
    /// that then `spawn_blocking`s, and the bound gate is about two of those
    /// overlapping.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .expect("test runtime builds")
    }

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

    /// A `MediaBinItem` in the store pointing at a REAL file — the store knows
    /// the id, and nothing has generated anything for it yet.
    fn register(ctx: &TestAppCtx, id: &str, path: &Path) {
        let item = rudis_core::MediaBinItem {
            id: id.to_string(),
            path: path.to_string_lossy().into_owned(),
            media_kind: rudis_core::MediaKind::Video,
            duration_us: 5_000_000,
            width: 1280,
            height: 720,
            fps: 30.0,
            is_vfr: false,
            rotation_degrees: 0,
            has_audio: false,
            poster_path: None,
            folder: String::new(),
            display_name: None,
            is_image_sequence: false,
            reports_alpha: None,
        };
        ctx.store()
            .lock()
            .expect("store")
            .dispatch(rudis_core::Command::AddMediaBinItem(item))
            .expect("the item registers");
    }

    /// The state string for one registered media id, through the REAL getter.
    fn state_of(ctx: &TestAppCtx, media_id: &str) -> String {
        test_state_of(ctx, media_id)
    }

    /// Every entry in the cache dir whose name contains `needle`.
    fn names_containing(dir: &Path, needle: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(needle))
            .collect()
    }

    /// Forget every registry row this test made, so a later test in the same
    /// process starts from a clean slate.
    fn forget(keys: &[String]) {
        let mut jobs = jobs();
        for key in keys {
            jobs.remove(key);
        }
    }

    // -----------------------------------------------------------------------
    // V-03 — the spawn returns long before the proxy does
    // -----------------------------------------------------------------------

    /// 58-CONTEXT D-09's load-bearing ordering claim, TIMED rather than read off
    /// the source: the call that starts a proxy returns in microseconds while
    /// the encode it started runs for seconds, and the proxy shows up afterwards.
    ///
    /// The fixture is a REAL 75 s 720p source and the encode is the REAL
    /// `h264_mf` sidecar — 58-03 measured this exact file at 1447 ms end to end,
    /// so "returned under 500 ms" and "not ready when it returned" are separated
    /// by a factor of three on the slowest of the two numbers.
    #[test]
    fn spawn_generation_returns_before_the_proxy_finishes() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let src = PathBuf::from(fixture("bars_720p30_75s.mp4"));
        let key = job_key(&src);
        register(&ctx, "media-v03", &src);

        let rt = runtime();
        let _enter = rt.enter();

        let t0 = Instant::now();
        spawn_generation(dir.clone(), src.clone());
        let returned_in = t0.elapsed();

        // Half of D-09: the call itself is not the encode.
        assert!(
            returned_in < Duration::from_millis(500),
            "spawn_generation must return immediately, took {returned_in:?}"
        );
        // The other half, and the one that would catch a synchronous
        // implementation that merely happened to be fast: at the instant the
        // call returned, there was no proxy.
        let at_return = state_of(&ctx, "media-v03");
        assert!(
            at_return == STATE_QUEUED || at_return == STATE_RUNNING,
            "the job is still in flight when the spawn returns, saw {at_return:?}"
        );

        // And then it lands, through the real encoder, into the real cache.
        let became_ready = wait_until(Duration::from_secs(90), || {
            state_of(&ctx, "media-v03") == STATE_READY
        });
        assert!(
            became_ready,
            "the proxy must eventually report ready, stuck at {:?}",
            state_of(&ctx, "media-v03")
        );

        // Non-vacuity: `ready` came from a real file, not from an optimistic
        // state transition.
        let payloads = names_containing(&dir, ".proxy.mp4");
        assert_eq!(
            payloads.len(),
            1,
            "exactly one committed proxy payload, saw {payloads:?}"
        );
        assert!(
            cache::read_fresh(&src, &dir).is_some(),
            "and the cache itself reports a fresh hit for the source"
        );

        forget(&[key]);
    }

    /// **59-REVIEW WR-02, the wiring half.** A COMPLETED generation announces
    /// itself on `decode_source`'s invalidation axis.
    ///
    /// The consumer that needs this is the program-level render cache's memo:
    /// it hashes the resolved decode answer into a segment's identity, and
    /// memoizes on the store's mutation counter — which a proxy landing does
    /// not move, because this worker dispatches nothing. Without the
    /// announcement the memo keeps handing back the pre-proxy hash, the cached
    /// range keeps serving an originals render, and the live ticks either side
    /// of it decode proxies: the boundary sharpness discontinuity CACHE-02
    /// forbids.
    ///
    /// Driven through the REAL encoder onto a REAL file, because the claim is
    /// about what a completed generation does and a hand-set state would prove
    /// only that this test can call a function.
    #[test]
    fn a_completed_generation_announces_the_decode_answer_change() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let src = PathBuf::from(fixture("bars_720p30_75s.mp4"));
        let key = job_key(&src);
        register(&ctx, "media-v02b", &src);

        let rt = runtime();
        let _enter = rt.enter();

        let before = preview::decode_source::decode_answer_generation();
        spawn_generation(dir.clone(), src.clone());
        assert!(
            wait_until(Duration::from_secs(90), || {
                state_of(&ctx, "media-v02b") == STATE_READY
            }),
            "the proxy must land through the real encoder, stuck at {:?}",
            state_of(&ctx, "media-v02b")
        );

        // Non-vacuity: `ready` came from a real committed pair, not from an
        // optimistic state transition.
        assert!(
            cache::read_fresh(&src, &dir).is_some(),
            "the cache must report a fresh hit for the source, or the \
             announcement below would be about a generation that produced \
             nothing"
        );

        let after = preview::decode_source::decode_answer_generation();
        println!("WR02-ANNOUNCE before={before} after={after}");
        assert_ne!(
            after, before,
            "a completed proxy generation must move the decode-answer \
             generation. It is the only signal a consumer memoizing this seam's \
             answer can key on — the store's own counter cannot see it, because \
             this worker never dispatches (59-REVIEW WR-02)."
        );

        forget(&[key]);
    }

    // -----------------------------------------------------------------------
    // V-04 — the cancel kills the child, it does not merely flag it
    // -----------------------------------------------------------------------

    /// 58-CONTEXT D-11 through THIS layer: raising the latch mid-encode ends the
    /// job as `cancelled` and leaves the cache directory exactly as it was — no
    /// payload, no meta, no in-flight residue.
    ///
    /// The in-flight detection is by file EXISTENCE, never by length: 58-03
    /// MEASURED that Windows serves `fs::metadata().len()` from the cached
    /// directory entry, which another process's open unflushed write does not
    /// update, so a `len() > 0` gate first fires when the encode is practically
    /// over (`deferred-items.md` D-8). Existence means the muxer has opened its
    /// output, which is exactly the claim needed.
    ///
    /// The source is the 240 s fixture, which 58-03 measured at 4872 ms end to
    /// end — the widest cancellation margin available from committed media.
    #[test]
    fn cancel_mid_encode_leaves_no_proxy_and_no_residue() {
        /// 58-03's measured full-encode time for this fixture on this machine.
        /// Named rather than inlined so the bound below is legible.
        const FULL_ENCODE_MS: u64 = 4872;

        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let src = PathBuf::from(fixture("bars_720p30_240s.mp4"));
        let key = job_key(&src);
        register(&ctx, "media-v04", &src);

        let rt = runtime();
        let _enter = rt.enter();

        let t0 = Instant::now();
        spawn_generation(dir.clone(), src.clone());

        // Wait until the encode is PROVABLY in flight.
        let in_flight = wait_until(Duration::from_secs(30), || {
            !names_containing(&dir, ".tmp.").is_empty()
        });
        assert!(
            in_flight,
            "the encode must open its in-flight file before the latch is raised \
             — without this the test could cancel a job that never started"
        );
        let to_in_flight = t0.elapsed();
        assert_eq!(
            state_of(&ctx, "media-v04"),
            STATE_RUNNING,
            "an in-flight encode reports running"
        );

        request_cancel_for_path(&src);
        let cancelled = wait_until(Duration::from_secs(30), || {
            state_of(&ctx, "media-v04") == STATE_CANCELLED
        });
        let total = t0.elapsed();
        assert!(
            cancelled,
            "the job must end as cancelled, stuck at {:?}",
            state_of(&ctx, "media-v04")
        );
        eprintln!("V-04: to_in_flight={to_in_flight:?} total={total:?}");

        // The child was KILLED, not left to finish: a completed encode would
        // have taken FULL_ENCODE_MS and would have left a payload behind.
        assert!(
            total < Duration::from_millis(FULL_ENCODE_MS),
            "cancellation must beat the {FULL_ENCODE_MS} ms full encode, took {total:?}"
        );
        assert!(
            names_containing(&dir, ".proxy.mp4").is_empty(),
            "no proxy payload survives a cancel: {:?}",
            names_containing(&dir, ".proxy.mp4")
        );
        assert!(
            names_containing(&dir, ".proxy.json").is_empty(),
            "no proxy meta survives a cancel: {:?}",
            names_containing(&dir, ".proxy.json")
        );
        assert!(
            names_containing(&dir, ".tmp.").is_empty(),
            "no in-flight residue survives a cancel: {:?}",
            names_containing(&dir, ".tmp.")
        );
        assert!(
            cache::read_fresh(&src, &dir).is_none(),
            "and the cache reports a MISS, so nothing partial can be mistaken \
             for a proxy by a later run"
        );

        forget(&[key]);
    }

    // -----------------------------------------------------------------------
    // The bound (T-58-05-02)
    // -----------------------------------------------------------------------

    /// Two heavy sources queued at once must never put two encodes on the
    /// silicon at the same instant.
    ///
    /// Sampled rather than reasoned about: the registry is read every few
    /// milliseconds and the high-water mark of simultaneously-running rows is
    /// asserted. The non-vacuity control is the same sampling loop — it must
    /// have seen at least one running job, or "never two" would be true of a
    /// pool that never started anything.
    #[test]
    fn two_spawns_never_run_two_encodes_at_the_same_instant() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let a = PathBuf::from(fixture("bars_720p30_75s.mp4"));
        let b = PathBuf::from(fixture("bars_720p30_240s.mp4"));
        let keys = [job_key(&a), job_key(&b)];

        let rt = runtime();
        let _enter = rt.enter();

        spawn_generation(dir.clone(), a.clone());
        spawn_generation(dir.clone(), b.clone());

        let mut max_running = 0usize;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            let running = {
                let jobs = jobs();
                keys.iter()
                    .filter(|k| jobs.get(*k).map(|row| row.state) == Some(STATE_RUNNING))
                    .count()
            };
            max_running = max_running.max(running);
            assert!(
                max_running <= MAX_CONCURRENT_PROXY_JOBS,
                "at most one proxy encode may run at a time, saw {max_running}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            max_running, 1,
            "the sampler must have caught a running job, or this gate is vacuous"
        );

        // Do not make the runtime's drop wait out a second full encode.
        cancel_all();
        wait_until(Duration::from_secs(30), || {
            let jobs = jobs();
            !keys.iter().any(|k| jobs.get(k).is_some_and(JobEntry::is_live))
        });

        forget(&keys);
    }

    // -----------------------------------------------------------------------
    // V-21 — the id is an id (T-58-05-01)
    // -----------------------------------------------------------------------

    /// A traversal payload handed to the status getter is simply an id nothing
    /// matches: `Ok(None)`, no filesystem access, no error to distinguish it
    /// from an ordinary unknown id.
    #[test]
    fn a_traversal_payload_is_just_an_id_that_matches_nothing() {
        let ctx = TestAppCtx::new();
        for hostile in [
            "../../../Windows/System32/config/SAM",
            "..\\..\\..\\Windows\\System32\\config\\SAM",
            "C:/Windows/System32/config/SAM",
            "\\\\?\\C:\\Windows\\win.ini",
            "",
        ] {
            assert_eq!(
                run_get_proxy_status(&ctx, hostile).expect("never an error"),
                None,
                "a traversal payload must resolve to nothing: {hostile:?}"
            );
        }
    }

    /// The non-vacuity control for the gate above: a REGISTERED id answers with
    /// a state, so `None` above means "unknown id" and not "this getter always
    /// returns None".
    #[test]
    fn a_registered_id_with_no_job_and_no_cache_reports_none_the_state() {
        let ctx = TestAppCtx::new();
        let src = PathBuf::from(fixture("bars_720p30_5s.mp4"));
        register(&ctx, "media-known", &src);
        assert_eq!(
            state_of(&ctx, "media-known"),
            STATE_NONE,
            "a known source with no proxy reports a state, not an absence"
        );
        assert_eq!(
            run_get_proxy_status(&ctx, "media-nope").expect("never an error"),
            None,
            "an unknown id still resolves to nothing"
        );
    }

    // -----------------------------------------------------------------------
    // V-05's shape half + the poll-only posture
    // -----------------------------------------------------------------------

    /// The wire shape a C# DTO deserializes: one field, a plain string.
    #[test]
    fn the_status_payload_serializes_state_as_a_plain_string() {
        let json = serde_json::to_string(&ProxyStatusPayload {
            state: STATE_RUNNING.to_string(),
            progress_permille: None,
        })
        .expect("serializes");
        assert_eq!(json, r#"{"state":"running"}"#);

        // Every state this module can produce is a bare lowercase word — no
        // tagging, no nesting, nothing a later state could renumber.
        for state in [
            STATE_QUEUED,
            STATE_RUNNING,
            STATE_READY,
            STATE_NOT_NEEDED,
            STATE_FAILED,
            STATE_CANCELLED,
            STATE_NONE,
        ] {
            let json = serde_json::to_string(&ProxyStatusPayload {
                state: state.to_string(),
                progress_permille: None,
            })
            .expect("serializes");
            assert_eq!(json, format!("{{\"state\":\"{state}\"}}"));
        }
    }

    /// Phase 71 (TRUST-03): `progress_permille` rides ONLY on `running`. Every
    /// other state is serialized with `Some(..)` deliberately set and must still
    /// come out as the bare one-field shape — the skip is on the field, but the
    /// getter's contract is that only `running` ever carries a number, so both
    /// halves are pinned: the serde skip for `None`, and the literal for `running`.
    #[test]
    fn a_running_payload_carries_progress_permille_and_nothing_else_does() {
        let json = serde_json::to_string(&ProxyStatusPayload {
            state: STATE_RUNNING.to_string(),
            progress_permille: Some(420),
        })
        .expect("serializes");
        assert_eq!(json, r#"{"state":"running","progress_permille":420}"#);

        // `None` is absent, not `null` — for every state, `running` included.
        for state in [
            STATE_QUEUED,
            STATE_RUNNING,
            STATE_READY,
            STATE_NOT_NEEDED,
            STATE_FAILED,
            STATE_CANCELLED,
            STATE_NONE,
        ] {
            let json = serde_json::to_string(&ProxyStatusPayload {
                state: state.to_string(),
                progress_permille: None,
            })
            .expect("serializes");
            assert!(!json.contains("progress_permille"), "{state}: {json}");
        }

        // And the GETTER never attaches a number to a non-running row, even
        // when the row's atomic holds one (a finished job's last value).
        let ctx = TestAppCtx::new();
        let src = PathBuf::from(fixture("bars_720p30_5s.mp4"));
        let key = job_key(&src);
        register(&ctx, "media-71-permille-states", &src);
        for state in [
            STATE_QUEUED,
            STATE_RUNNING,
            STATE_NOT_NEEDED,
            STATE_FAILED,
            STATE_CANCELLED,
        ] {
            let epoch = ROW_EPOCH.fetch_add(1, Ordering::Relaxed);
            jobs().insert(
                key.clone(),
                JobEntry {
                    epoch,
                    state,
                    cancel: Arc::new(AtomicBool::new(false)),
                    progress: Arc::new(AtomicU32::new(1234)),
                },
            );
            let payload = run_get_proxy_status(&ctx, "media-71-permille-states")
                .expect("answers")
                .expect("known id");
            assert_eq!(payload.state, state);
            if state == STATE_RUNNING {
                assert_eq!(
                    payload.progress_permille,
                    Some(999),
                    "a running row is clamped to 999 — only `ready` means done"
                );
            } else {
                assert_eq!(payload.progress_permille, None, "{state} carries no number");
            }
        }
        forget(&[key]);
    }

    /// Phase 71 (TRUST-03), through the REAL encoder: while a real 4K proxy is
    /// being built, `run_get_proxy_status` reports `running` with the encoder's
    /// own progress — at least one value above zero, and never going backwards.
    #[test]
    fn the_running_row_reports_the_encoders_own_progress() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let src = PathBuf::from(fixture("longgop_4k30_60s.mp4"));
        let key = job_key(&src);
        forget(std::slice::from_ref(&key));
        register(&ctx, "media-71-progress", &src);

        let rt = runtime();
        let _enter = rt.enter();
        spawn_generation(dir.clone(), src.clone());

        let mut seen: Vec<u32> = Vec::new();
        let mut running_polls = 0usize;
        let deadline = Instant::now() + Duration::from_secs(120);
        let final_state = loop {
            let payload = run_get_proxy_status(&ctx, "media-71-progress")
                .expect("answers")
                .expect("known id");
            if payload.state == STATE_RUNNING {
                running_polls += 1;
                let p = payload
                    .progress_permille
                    .expect("a running payload always carries progress_permille");
                assert!(p <= 999, "running never claims done, saw {p}");
                seen.push(p);
            } else {
                assert_eq!(payload.progress_permille, None, "{}", payload.state);
                if payload.state != STATE_QUEUED {
                    break payload.state;
                }
            }
            if Instant::now() >= deadline {
                break payload.state;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut distinct = seen.clone();
        distinct.dedup();
        println!(
            "TRUST-03 PROXY-PERMILLE: {running_polls} running polls, final={final_state}, distinct={distinct:?}"
        );

        assert_eq!(final_state, STATE_READY, "the real 4K proxy must land");
        assert!(
            seen.iter().any(|&p| p > 0),
            "at least one running answer must carry the encoder's progress above 0, saw {seen:?}"
        );
        assert!(
            seen.windows(2).all(|w| w[0] <= w[1]),
            "progress must be non-decreasing per job, saw {seen:?}"
        );

        forget(&[key]);
    }

    // -----------------------------------------------------------------------
    // The dedupe and the cancel surface
    // -----------------------------------------------------------------------

    /// A second spawn for a live job is a no-op, and the second call must not
    /// replace the latch the first job is watching — otherwise a cancel raised
    /// after a duplicate spawn would be written to an `Arc` nobody reads.
    #[test]
    fn a_second_spawn_for_a_live_job_keeps_the_first_jobs_latch() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let src = PathBuf::from(fixture("bars_720p30_240s.mp4"));
        let key = job_key(&src);

        let rt = runtime();
        let _enter = rt.enter();

        spawn_generation(dir.clone(), src.clone());
        let first = jobs().get(&key).map(|row| Arc::clone(&row.cancel));
        let first = first.expect("the first spawn registered a job");

        spawn_generation(dir.clone(), src.clone());
        let second = jobs()
            .get(&key)
            .map(|row| Arc::clone(&row.cancel))
            .expect("the row is still there");
        assert!(
            Arc::ptr_eq(&first, &second),
            "a duplicate spawn must not replace the live job's cancel latch"
        );
        assert_eq!(
            jobs().keys().filter(|k| *k == &key).count(),
            1,
            "one registry row per source, however many times it is spawned"
        );

        // `cancel_all` reaches the latch the running job actually holds.
        assert!(!first.load(Ordering::Relaxed), "not cancelled yet");
        cancel_all();
        assert!(first.load(Ordering::Relaxed), "cancel_all raises the latch");

        wait_until(Duration::from_secs(30), || {
            !jobs().get(&key).is_some_and(JobEntry::is_live)
        });
        forget(&[key]);
    }

    /// Cancelling a path nothing knows about is a no-op, never a panic — the
    /// dispatch peek calls this for every removed media item, most of which
    /// never had a proxy job.
    #[test]
    fn cancelling_an_unknown_path_is_a_no_op() {
        request_cancel_for_path(Path::new("C:/nope/does/not/exist.mp4"));
        request_cancel_for_path(Path::new("../../../Windows/System32/config/SAM"));
        request_cancel_for_path(Path::new(""));
    }

    // -----------------------------------------------------------------------
    // WR-05 — `ready` is a claim about a FILE, and something re-arms it
    // -----------------------------------------------------------------------

    /// **58-REVIEW WR-05.** A remembered `ready` must be confirmed against the
    /// cache, because `prune_bytes` can evict the pair at any time to keep the
    /// byte budget — any later commit for any other source may take this one's
    /// bytes.
    ///
    /// Playback is never wrong when that happens (D-17's fallback is total), so
    /// this is about the getter telling the truth rather than about correctness
    /// of the pixels. The control half — a `ready` row over a cache that really
    /// does hold the pair still answering `ready` — is what stops this from
    /// passing for a getter that simply never says `ready`.
    #[test]
    fn a_remembered_ready_is_confirmed_against_the_cache() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        std::fs::create_dir_all(&dir).expect("mkdir");

        // A real source file, and a real (fabricated) cache pair for it.
        // `read_fresh` never decodes, so the payload's bytes are irrelevant —
        // only its LENGTH has to agree with the meta.
        let src = dir.join("wr05-ready-probe.mp4");
        std::fs::write(&src, vec![0x5Au8; 4096]).expect("write the source");
        let cache_key = cache::key_for(&src).expect("a real file is keyable");
        let stem = cache::file_stem_for(&cache_key);
        let payload = dir.join(cache::payload_file_name(&stem));
        std::fs::write(&payload, vec![0x11u8; 512]).expect("write the payload");
        assert!(
            cache::write_meta(&dir, &stem, &cache::ProxyMeta::new(cache_key, 960, 540, 512)),
            "the fabricated pair must commit, or this gate tests nothing"
        );

        let key = job_key(&src);
        forget(std::slice::from_ref(&key));
        register(&ctx, "media-wr05", &src);
        jobs().insert(
            key.clone(),
            JobEntry {
                epoch: ROW_EPOCH.fetch_add(1, Ordering::Relaxed),
                state: STATE_READY,
                cancel: Arc::new(AtomicBool::new(false)),
                progress: Arc::new(AtomicU32::new(0)),
            },
        );

        // CONTROL: the pair is on disk, so the remembered `ready` is TRUE.
        assert_eq!(
            state_of(&ctx, "media-wr05"),
            STATE_READY,
            "a remembered `ready` confirmed by the cache must still answer ready"
        );

        // The event under test: the byte budget evicts the pair.
        std::fs::remove_file(&payload).expect("evict the payload");
        std::fs::remove_file(dir.join(cache::meta_file_name(&stem))).expect("evict the meta");

        assert_eq!(
            state_of(&ctx, "media-wr05"),
            STATE_NONE,
            "the registry still remembers `ready`, but the proxy is gone — a \
             remembered `ready` must be confirmed against the cache, not trusted"
        );

        forget(&[key]);
    }

    /// The second trigger. Opening a project re-arms generation for its heavy
    /// media, which is what makes a proxy survive a session boundary or a cache
    /// eviction at all.
    ///
    /// The only permit is held, so nothing encodes: what is asserted is which
    /// sources were ADMITTED, which is the whole of this trigger's policy.
    #[test]
    fn the_project_rearm_admits_heavy_sources_and_only_heavy_sources() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");

        // Synthetic paths — nothing is encoded, and `job_key` falls back to the
        // literal path when `canonicalize` fails.
        let heavy = dir.join("wr05-rearm-4k.mp4");
        let light = dir.join("wr05-rearm-720p.mp4");
        let heavy_key = job_key(&heavy);
        let light_key = job_key(&light);
        forget(&[heavy_key.clone(), light_key.clone()]);

        // Hold the ONLY permit so neither job can reach an encoder.
        let permit = try_take_permit().expect("the idle pool has its permit");

        let rt = runtime();
        {
            let _enter = rt.enter();
            rearm_project_media(
                &ctx,
                &[
                    (heavy.clone(), 3840, 2160),
                    (light.clone(), 1280, 720),
                ],
            );
        }

        assert_eq!(
            jobs().get(&heavy_key).map(|row| row.state),
            Some(STATE_QUEUED),
            "a 4K source in a reopened project must be re-armed — without this \
             trigger `spawn_generation`'s only caller is import, so a project \
             opened from disk never gets proxies at all"
        );
        assert!(
            jobs().get(&light_key).is_none(),
            "a 720p source must cost nothing: the heaviness predicate is the gate, \
             and re-arm must not smuggle work past it. Saw {:?}",
            jobs().get(&light_key).map(|row| row.state)
        );

        drop(rt);
        drop(permit);
        forget(&[heavy_key, light_key]);
    }

    /// An empty project re-arms nothing and touches no filesystem — the
    /// overwhelmingly common case (a new project, or one with no video).
    #[test]
    fn the_project_rearm_is_a_no_op_for_an_empty_media_bin() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let before = jobs().len();
        rearm_project_media(&ctx, &[]);
        assert_eq!(jobs().len(), before, "an empty bin registers nothing");
    }

    /// The WIRING, not the helper: a REAL `open_project` of a REAL `.rud` file
    /// written by a previous "session" must re-arm its heavy media.
    ///
    /// The two tests above would both stay green if the call in
    /// `project::run_open_project` were deleted, which is exactly the gap this
    /// one closes. It is also the scenario the review named — "reopen the
    /// project I made yesterday" — driven end to end through the real
    /// create/autosave/switch/open path rather than simulated.
    ///
    /// The only permit is held throughout, so no encoder is ever reached: what
    /// is under test is admission, not transcoding.
    #[test]
    fn opening_a_project_from_disk_rearms_its_heavy_media() {
        use crate::project::{run_new_project, run_open_project};

        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let heavy = proxy_dir(&ctx)
            .expect("cache dir")
            .join("wr05-e2e-4k.mp4");
        let key = job_key(&heavy);
        forget(std::slice::from_ref(&key));

        // Session 1: a project with one 4K source in its bin.
        run_new_project(&ctx, &serde_json::json!({ "name": "wr05-yesterday" }))
            .expect("create the project");
        ctx.store()
            .lock()
            .expect("store")
            .dispatch(rudis_core::Command::AddMediaBinItem(
                rudis_core::MediaBinItem {
                    id: "wr05-e2e-media".to_string(),
                    path: heavy.to_string_lossy().into_owned(),
                    media_kind: rudis_core::MediaKind::Video,
                    duration_us: 5_000_000,
                    width: 3840,
                    height: 2160,
                    fps: 30.0,
                    is_vfr: false,
                    rotation_degrees: 0,
                    has_audio: false,
                    poster_path: None,
                    folder: String::new(),
                    display_name: None,
                    is_image_sequence: false,
                    reports_alpha: None,
                },
            ))
            .expect("the item registers");

        // Switching away autosaves it — this is the ".rud on disk" half.
        run_new_project(&ctx, &serde_json::json!({ "name": "wr05-today" }))
            .expect("create the second project");
        // No job may exist yet: nothing was imported in this "session".
        forget(std::slice::from_ref(&key));

        // Hold the ONLY permit so the re-arm cannot reach an encoder.
        let permit = try_take_permit().expect("the idle pool has its permit");

        // Session 2: reopen yesterday's project.
        let rt = runtime();
        {
            let _enter = rt.enter();
            run_open_project(&ctx, &serde_json::json!({ "name": "wr05-yesterday" }))
                .expect("reopen the project");
        }

        assert_eq!(
            jobs().get(&key).map(|row| row.state),
            Some(STATE_QUEUED),
            "reopening a project whose media was imported in an EARLIER session \
             must re-arm its proxies. Without the `rearm_project_media` call in \
             `run_open_project`, `spawn_generation`'s only caller is import and \
             this flow never gets a proxy at all."
        );

        drop(rt);
        drop(permit);
        forget(&[key]);
    }

    /// **71-01 / D-63-04-01 — the production shape of the re-arm.** The twin
    /// above wraps `run_open_project` in `rt.enter()`, which is exactly what
    /// the C ABI host does NOT do: `FfiAppCtx` enters no runtime, and 63-04
    /// read `proxy: no tokio runtime; skipping background proxy for …` off the
    /// shipped Open four times while that twin stayed green (WR-05 masking).
    /// This test calls the same entry point with NO ambient runtime.
    #[test]
    fn opening_a_project_from_disk_rearms_with_no_ambient_runtime() {
        use crate::project::{run_new_project, run_open_project};

        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let heavy = proxy_dir(&ctx)
            .expect("cache dir")
            .join("wr05-71-no-runtime-4k.mp4");
        let key = job_key(&heavy);
        forget(std::slice::from_ref(&key));

        // Session 1: a project with one 4K source in its bin.
        run_new_project(&ctx, &serde_json::json!({ "name": "wr05-71-yesterday" }))
            .expect("create the project");
        ctx.store()
            .lock()
            .expect("store")
            .dispatch(rudis_core::Command::AddMediaBinItem(
                rudis_core::MediaBinItem {
                    id: "wr05-71-media".to_string(),
                    path: heavy.to_string_lossy().into_owned(),
                    media_kind: rudis_core::MediaKind::Video,
                    duration_us: 5_000_000,
                    width: 3840,
                    height: 2160,
                    fps: 30.0,
                    is_vfr: false,
                    rotation_degrees: 0,
                    has_audio: false,
                    poster_path: None,
                    folder: String::new(),
                    display_name: None,
                    is_image_sequence: false,
                    reports_alpha: None,
                },
            ))
            .expect("the item registers");

        // Switching away autosaves it — the ".rud on disk" half.
        run_new_project(&ctx, &serde_json::json!({ "name": "wr05-71-today" }))
            .expect("create the second project");
        forget(std::slice::from_ref(&key));

        // Hold the ONLY permit so the re-arm cannot reach an encoder.
        let permit = try_take_permit().expect("the idle pool has its permit");

        // Session 2: reopen yesterday's project exactly as the C ABI does —
        // no runtime entered around the call.
        run_open_project(&ctx, &serde_json::json!({ "name": "wr05-71-yesterday" }))
            .expect("reopen the project");

        let state = jobs().get(&key).map(|row| row.state);
        drop(permit);
        forget(std::slice::from_ref(&key));

        assert_eq!(
            state,
            Some(STATE_QUEUED),
            "D-63-04-01: reopening a project from disk through the production \
             shape (no ambient tokio runtime, as `FfiAppCtx` runs it) must queue a \
             proxy for its heavy media. A `None` here means `rearm_project_media` \
             degraded to a no-op because `Handle::try_current()` failed — the \
             `proxy: no tokio runtime; skipping background proxy for …` line."
        );
    }

    // -----------------------------------------------------------------------
    // WR-04 — a registry row can never outlive the task that owns it
    // -----------------------------------------------------------------------

    /// **58-REVIEW WR-04.** A runtime shutdown with a job still QUEUED must not
    /// strand its row.
    ///
    /// [`JOBS`] is process-global while the runtime is per-ctx, so a stranded
    /// `queued` row is not merely untidy: [`spawn_generation`] dedupes on
    /// [`JobEntry::is_live`], so that source could never get a proxy again for
    /// the life of the PROCESS — including from a completely different ctx — and
    /// no timeout, reaper or caller-facing clear exists to correct it.
    ///
    /// The only permit is held for the whole test, so the spawned task is parked
    /// on `SEM.acquire()` and is provably still QUEUED when the runtime goes
    /// away. That is the mid-await drop the happy path structurally cannot see.
    ///
    /// The source is a SYNTHETIC path, unique to this test, and deliberately not
    /// one of the shared fixtures: [`JOBS`] is keyed by path and process-global,
    /// so a sibling test that happens to use the same fixture would have this
    /// spawn DEDUPED against its row and the gate would then be measuring the
    /// neighbour's job. The file never needs to exist — the task is parked on
    /// the permit and never reaches `proxy::generate`, and `job_key` falls back
    /// to the literal path when `canonicalize` fails.
    #[test]
    fn a_runtime_shutdown_clears_a_queued_row_instead_of_stranding_it() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let src = dir.join("wr04-shutdown-probe-never-encoded.mp4");
        let key = job_key(&src);
        forget(std::slice::from_ref(&key));

        // Hold the ONLY permit, so nothing can progress past the acquire.
        let permit = try_take_permit().expect("the idle pool has its permit");

        let rt = runtime();
        {
            let _enter = rt.enter();
            spawn_generation(dir, src);
        }
        // Give a worker time to poll the task onto its `SEM.acquire()` await, so
        // this really is the MID-AWAIT case and not the never-polled one (which
        // the test below owns).
        std::thread::sleep(Duration::from_millis(150));

        assert_eq!(
            jobs().get(&key).map(|row| row.state),
            Some(STATE_QUEUED),
            "with the permit held the job must be parked in `queued` — otherwise \
             this gate is not testing a mid-await drop"
        );

        // The event under test: the runtime goes away with the task still parked.
        drop(rt);

        let cleared = wait_until(Duration::from_secs(5), || jobs().get(&key).is_none());
        assert!(
            cleared,
            "the queued row survived the runtime shutdown as {:?} — that is WR-04: \
             `spawn_generation` for this source is now a permanent no-op, \
             `run_get_proxy_status` answers `queued` forever, and \
             `evict_finished_if_full` can never reclaim the row because live rows \
             are exempt",
            jobs().get(&key).map(|row| row.state)
        );

        drop(permit);
        forget(&[key]);
    }

    /// The harder half of WR-04, and the one a naive guard MISSES: a task
    /// dropped before it was ever POLLED.
    ///
    /// A guard declared as the async body's first statement does not exist until
    /// the first poll, so a runtime shutting down under load — which drops tasks
    /// that never got a worker — would strand the row exactly as before. This
    /// was not a hypothetical: it is how the first version of the fix failed,
    /// intermittently, when this crate's other proxy tests were saturating the
    /// machine. The guard is therefore constructed BEFORE the spawn and captured
    /// by move.
    ///
    /// A **current-thread** runtime makes "never polled" a fact rather than a
    /// race: it only polls spawned tasks from inside `block_on`, and this test
    /// never calls it. No permit is taken because nothing ever runs.
    #[test]
    fn a_runtime_shutdown_clears_a_row_whose_task_was_never_polled() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let src = dir.join("wr04-never-polled-probe.mp4");
        let key = job_key(&src);
        forget(std::slice::from_ref(&key));

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime builds");
        {
            let _enter = rt.enter();
            spawn_generation(dir, src);
        }

        assert_eq!(
            jobs().get(&key).map(|row| row.state),
            Some(STATE_QUEUED),
            "the spawn must have registered a live row for this gate to mean anything"
        );

        drop(rt);

        assert!(
            jobs().get(&key).is_none(),
            "a task dropped BEFORE its first poll must still clear its row, saw \
             {:?}. Construct the RowGuard before the spawn and capture it by move \
             — a guard declared inside the async body does not exist yet.",
            jobs().get(&key).map(|row| row.state)
        );
        forget(&[key]);
    }

    /// `spawn_generation` called with no runtime at all must not leave a row
    /// behind either — that path used to clear it by hand, and now relies on the
    /// same guard as every other exit.
    #[test]
    fn spawning_outside_a_runtime_leaves_no_row() {
        let _lease = encoder_lease();
        let ctx = TestAppCtx::new();
        let dir = proxy_dir(&ctx).expect("cache dir");
        let src = dir.join("wr04-no-runtime-probe.mp4");
        let key = job_key(&src);
        forget(std::slice::from_ref(&key));

        // No `rt.enter()`: `Handle::try_current()` fails and the function
        // degrades to "no proxy".
        spawn_generation(dir, src);

        assert!(
            jobs().get(&key).is_none(),
            "a spawn that never started a task must not dedupe a later one, saw {:?}",
            jobs().get(&key).map(|row| row.state)
        );
        forget(&[key]);
    }

    /// A row's identity is `(key, epoch)`, not the key alone.
    ///
    /// A key can be re-registered by a LATER spawn once the earlier row is
    /// finished, so a late-arriving write or guard-drop from the earlier task
    /// must not touch the successor's row. Pure registry manipulation — no
    /// encoder, no runtime.
    #[test]
    fn a_finished_task_cannot_write_to_a_later_spawns_row() {
        let _lease = encoder_lease();
        let key = "wr04-epoch-probe".to_string();
        forget(std::slice::from_ref(&key));

        let insert = |epoch: u64, state: &'static str| {
            jobs().insert(
                key.clone(),
                JobEntry {
                    epoch,
                    state,
                    cancel: Arc::new(AtomicBool::new(false)),
                    progress: Arc::new(AtomicU32::new(0)),
                },
            );
        };

        insert(1, STATE_RUNNING); // spawn A
        insert(2, STATE_QUEUED); // A finished, B took the key

        // A publishes its terminal state LATE.
        set_state(&key, 1, STATE_FAILED);
        assert_eq!(
            jobs().get(&key).map(|row| row.state),
            Some(STATE_QUEUED),
            "a stale write must not overwrite the successor's state"
        );

        // A's guard drops LATE.
        drop(RowGuard {
            key: key.clone(),
            epoch: 1,
        });
        assert_eq!(
            jobs().get(&key).map(|row| row.state),
            Some(STATE_QUEUED),
            "a stale guard must not CLEAR the successor's live row"
        );

        // B's own guard does clear it — the control that keeps the two
        // assertions above from passing for a guard that never removes anything.
        drop(RowGuard {
            key: key.clone(),
            epoch: 2,
        });
        assert!(
            jobs().get(&key).is_none(),
            "the OWNING guard must still clear its own live row"
        );

        forget(&[key]);
    }

    /// A FINISHED row is the answer a later poll should read, so the guard must
    /// leave it alone on the normal exit.
    #[test]
    fn the_guard_leaves_a_finished_row_alone() {
        let _lease = encoder_lease();
        let key = "wr04-finished-probe".to_string();
        forget(std::slice::from_ref(&key));
        jobs().insert(
            key.clone(),
            JobEntry {
                epoch: 7,
                state: STATE_READY,
                cancel: Arc::new(AtomicBool::new(false)),
                progress: Arc::new(AtomicU32::new(0)),
            },
        );

        drop(RowGuard {
            key: key.clone(),
            epoch: 7,
        });

        assert_eq!(
            jobs().get(&key).map(|row| row.state),
            Some(STATE_READY),
            "a terminal row was set deliberately and must survive its guard"
        );
        forget(&[key]);
    }

    // -----------------------------------------------------------------------
    // The registry bound
    // -----------------------------------------------------------------------

    /// The eviction sweep forgets FINISHED rows and keeps live ones — the
    /// property that makes the cap safe.
    #[test]
    fn the_registry_sweep_forgets_finished_rows_and_keeps_live_ones() {
        let mut map: HashMap<String, JobEntry> = HashMap::new();
        for i in 0..MAX_TRACKED_PROXY_JOBS {
            let state = if i % 2 == 0 {
                STATE_READY
            } else {
                STATE_RUNNING
            };
            map.insert(
                format!("k{i}"),
                JobEntry {
                    epoch: i as u64,
                    state,
                    cancel: Arc::new(AtomicBool::new(false)),
                    progress: Arc::new(AtomicU32::new(0)),
                },
            );
        }
        evict_finished_if_full(&mut map);
        assert!(
            map.values().all(JobEntry::is_live),
            "every surviving row is live"
        );
        assert_eq!(
            map.len(),
            MAX_TRACKED_PROXY_JOBS / 2,
            "and exactly the finished half was forgotten"
        );

        // Below the cap the sweep does nothing at all.
        let mut small: HashMap<String, JobEntry> = HashMap::new();
        small.insert(
            "only".to_string(),
            JobEntry {
                epoch: 0,
                state: STATE_READY,
                cancel: Arc::new(AtomicBool::new(false)),
                progress: Arc::new(AtomicU32::new(0)),
            },
        );
        evict_finished_if_full(&mut small);
        assert_eq!(small.len(), 1, "an under-full registry is left alone");
    }

    /// The bound is a named constant, and the semaphore is actually built from
    /// it — a pool sized by a stray literal would make the constant decorative.
    #[test]
    fn the_pool_is_sized_by_the_named_bound() {
        assert_eq!(MAX_CONCURRENT_PROXY_JOBS, 1);
        assert!(
            SEM.available_permits() <= MAX_CONCURRENT_PROXY_JOBS,
            "the pool can never hand out more permits than the bound names"
        );
        // Idle, every permit is available. Serialized against the tests that
        // take one, so this is a fact rather than a race.
        let _lease = encoder_lease();
        assert_eq!(
            SEM.available_permits(),
            MAX_CONCURRENT_PROXY_JOBS,
            "an idle pool holds exactly the bound's worth of permits"
        );
    }
}
