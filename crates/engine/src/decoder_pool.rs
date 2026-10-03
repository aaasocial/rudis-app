//! Live-preview twin of the export `SeqLayer` session pool.
//!
//! Phase 18.2 (PERF-01) foundation: a headlessly-testable `LayerDecoderPool`
//! that GENERALIZES the pixel-proven export-side session pool
//! (now `crates/app-core/src/export.rs`, the COMP-02 multi-layer branch) — one persistent
//! [`ExportRunDecoder`] per active clip, resampled to the project fps, advanced
//! one output tick at a time. Bounded spawns (`<= active.len()`); sessions are
//! dropped for clips that leave the active set (T-18-04); a bounded stall HOLDs
//! the last frame; a genuine stream end (EOF) RE-ROOTS the session at the
//! demand with a replacement streaming child (plan 59-15 / Phase 57
//! deferred-items § D-2 — it used to drop the session and cold-decode one
//! frame SYNCHRONOUSLY, measured at 284.3 ms on 720p and 1058.9 ms on 4K
//! Long-GOP, on the producer thread); a real source jump (seek) restarts a
//! session.
//!
//! This module is PACING-agnostic and has NO domain-core, backend-store,
//! app-handle, or surface dependency — pure data in, frames out — so SC-1
//! (spawn count) and SC-4
//! (lifecycle/leak) are fast headless `cargo test -p engine` tests. Compositing
//! stays the untouched `compositor::composite_layers_to_rgba` (this type only
//! SOURCES frames).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::ffmpeg::{frame_step_us, probe, ExportRunDecoder, Frame, FramePull, MediaInfo};

/// How many times the `FramePull::Ended` arm has RE-ROOTED a layer — spawned a
/// replacement streaming session at (or one source-step past) the demand
/// instead of the pre-59-15 synchronous cold single-frame decode.
/// Process-global and `SeqCst`, exactly like `STREAM_SPAWN_COUNT` — tests that
/// read it serialize on a file-level mutex.
///
/// (The removed helper's NAME is deliberately not spelled anywhere in this
/// file. 59-15's acceptance gate is a textual `grep -c` for it against this
/// path, and explanatory prose that mentions it makes that gate unreadable —
/// the same way doc prose has repeatedly tripped D-40's textual seam scan.
/// `tests/pool_ended.rs` names it, where it is a real call: the ground-truth
/// oracle the parity pins compare against.)
pub static POOL_ENDED_REROOTS: AtomicU64 = AtomicU64::new(0);

/// How many times the `FramePull::Ended` arm answered a tick with the ended
/// session's LAST frame (a HOLD) rather than a freshly decoded one. In the
/// nonblocking (ring) mode this fires exactly once per stream end — the
/// documented ≤1-source-step transient; in the default mode it fires only when
/// the replacement could not answer inside the pull timeout.
pub static POOL_ENDED_HOLDS: AtomicU64 = AtomicU64::new(0);

/// How many times the arm GAVE UP on a layer: its replacement session produced
/// nothing at all before ending (a re-root that seeked past the media's real
/// EOF, or unreadable media). Each increment is paired with the loud stderr
/// line and a `FAILURE_BACKOFF` entry, so this counter is the machine-readable
/// half of "a missing layer is never silent" (debug session
/// `multitrack-preview-lag-choppy`) — and its bound is what proves the
/// replacement is not respawned per tick (T-59-15-02).
pub static POOL_ENDED_GIVEUPS: AtomicU64 = AtomicU64::new(0);

/// One layer's frame request for a single output tick — the engine-level,
/// core-free analog of a `core::ClipHit` plus its clip span. The caller builds
/// this from `resolve_multilayer`; `clip_id` is the stable pool key (a clip
/// re-entering after a gap restarts clean, exactly like the export pool).
pub struct LayerFrameSource {
    /// Stable pool key — the clip's id (NOT the media id).
    pub clip_id: String,
    /// Decoded source file path (also the `dims` cache key).
    pub path: PathBuf,
    /// The source timestamp this tick wants a frame at.
    pub source_us: i64,
    /// Upright rotation in degrees (0 / 90 / 180 / 270).
    pub rotation: u32,
    /// Remaining span for the `ExportRunDecoder` `-t` window = `clip.out_us - source_us`.
    pub remaining_dur_us: i64,
    /// The SOURCE advance this output tick represents (debug session
    /// `multitrack-preview-lag-choppy`, 2026-08-01 — the live twin of export
    /// SR-3, quick task 260730-x2t). Un-retimed: exactly the pool's own
    /// `step_us`, keeping every pre-retime behavior byte-identical. A
    /// constant-speed retimed clip: `tempo * step_us` — the pool streams its
    /// session at `fps / tempo` cadence and judges sequential continuation at
    /// `src_step/2` tolerance, instead of reading every retimed tick as a
    /// seek and respawning ffmpeg per output frame (the storm that starved
    /// the layer out of the live composite entirely on real media).
    pub src_step_us: i64,
}

/// One clip's open sequential decoder plus the `source_us` its next pull will
/// land on, and the last frame it successfully produced (the HOLD source).
struct PoolSession {
    decoder: ExportRunDecoder,
    next_source_us: i64,
    /// Last successfully pulled frame — reused on a bounded stall (HOLD).
    last: Option<Frame>,
    /// Whether THIS session (this child process, since ITS spawn) has ever
    /// answered a pull with a frame. The `Ended` arm's storm guard: a session
    /// that ends WITHOUT ever having delivered is a replacement that seeked
    /// past a real EOF (or unreadable media), and re-rooting it again would
    /// spawn one ffmpeg child per tick forever. Such a session backs off
    /// (WR-03) instead (T-59-15-02).
    delivered_since_spawn: bool,
}

/// One path's `probe()` result, kept WHOLE.
///
/// Plan 59-25: the pool used to keep only the rotation-swapped `dims` and drop
/// the rest, so every `ExportRunDecoder` spawn re-discovered the same file's
/// facts with its OWN `ffprobe` child — ~88 ms apiece (59-15's measurement).
/// Keeping the `MediaInfo` beside the dims lets the authorized spawn sites hand
/// it to `ExportRunDecoder::start_with_probed` (plan 59-23) and skip that child
/// entirely.
///
/// The two fields are created TOGETHER from ONE probe of ONE path and are only
/// ever inserted under that path's key, which is exactly the callee's contract
/// (*"`probed` MUST be a `probe(path)` result for this SAME `path`"*). Merges
/// (`seed_dims_from`, `adopt_missing`) copy the WHOLE entry for the same reason:
/// a half-merged entry could pair one file's dims with another file's info, and
/// that failure is silent and visual (a VP9-alpha overlay decoding opaque, a
/// still collapsing to one frame).
#[derive(Clone)]
struct CachedProbe {
    /// Native UPRIGHT dims — the rotation swap applied at FIRST insertion.
    dims: (u32, u32),
    /// The probe that produced them, kept for the spawn sites.
    info: MediaInfo,
}

/// Live-preview session pool (see module docs). One persistent
/// [`ExportRunDecoder`] per active clip, resampled to `fps`, advanced one output
/// tick per `advance()` call.
pub struct LayerDecoderPool {
    sessions: HashMap<String, PoolSession>,
    /// Native UPRIGHT dims per path plus the probe that produced them, probed at
    /// most once per path (see [`CachedProbe`]).
    dims: HashMap<PathBuf, CachedProbe>,
    /// clip_id → the Instant its last probe/start failed. A failing layer is
    /// skipped (no ffprobe/ffmpeg respawn) until FAILURE_BACKOFF elapses,
    /// mirroring native_surface.rs's session_failed discipline (WR-03).
    failed: HashMap<String, Instant>,
    /// clip_id → the last frame a layer produced before its stream ended
    /// UNRECOVERABLY (its replacement seeked past the media's real end). Served
    /// while the clip sits in `FAILURE_BACKOFF` so the layer HOLDS instead of
    /// blinking out of the composite.
    ///
    /// This is the software twin of the coordinator's end-of-stream HOLD
    /// (`preview::layer_sessions`, 57-06): *"there is provably no content at or
    /// after `ended_at`, so a demand that keeps advancing past it cannot be
    /// answered by decoding — only by holding"*. The ordinary way a clip
    /// reaches this state at all is an out-point sitting a few frames past its
    /// media's last decodable frame (a duration probe/estimate mismatch), and
    /// on that clip the alternative is a black flash at every playthrough of
    /// its tail. Bounded at one frame per ACTIVE clip — `advance`'s retain
    /// prunes it exactly like `failed`.
    eof_holds: HashMap<String, Frame>,
    fps: f64,
    step_us: i64,
    timeout: Duration,
    /// Which trade this pool's `FramePull::Ended` arm makes. See
    /// [`LayerDecoderPool::set_ended_nonblocking`]. Default `false` —
    /// blocking-exact — because the render-cache WRITER and every
    /// export-shaped caller must bake the genuinely correct frame.
    ended_nonblocking: bool,
}

/// How long a clip whose probe/start failed is skipped before being retried —
/// stops an unresolvable (moved/deleted) source from re-spawning ffprobe/ffmpeg
/// every tick (WR-03).
const FAILURE_BACKOFF: Duration = Duration::from_secs(2);

impl LayerDecoderPool {
    /// New pool for a `project_fps`-paced timeline. `timeout` bounds each pull
    /// (a stall past it HOLDs the last frame rather than blocking the tick).
    pub fn new(project_fps: f64, timeout: Duration) -> Self {
        Self {
            sessions: HashMap::new(),
            dims: HashMap::new(),
            failed: HashMap::new(),
            eof_holds: HashMap::new(),
            fps: project_fps,
            step_us: frame_step_us(project_fps).max(1),
            timeout,
            ended_nonblocking: false,
        }
    }

    /// Choose which trade this pool's `FramePull::Ended` arm makes when a
    /// layer's streaming decoder reaches the end of its `-t` window
    /// (plan 59-15 / Phase 57 deferred-items § D-2).
    ///
    /// * `false` (**DEFAULT — blocking-exact**): spawn the replacement AT the
    ///   demand and pull it with this pool's own timeout. The delivered pixels
    ///   are byte-identical (MAD 0.0000) to the one-shot precise-decode ground
    ///   truth at the same position — i.e. to what the pre-59-15 arm produced
    ///   and to what EXPORT decodes. This is load-bearing for D-28: the render
    ///   cache WRITER (`render_cache_writer.rs`) bakes segments through this
    ///   pool, and a stream end inside a segment must bake the genuinely
    ///   correct frame, never a hold. **Do not turn this on for the writer.**
    ///
    /// * `true` (**ring opt-in — nonblocking**): HOLD the ended session's last
    ///   frame for THIS tick only and re-root at the NEXT demand, without
    ///   pulling. The single tick of divergence is exactly ground truth at
    ///   `demand - src_step` — the SAME documented ≤1-source-step class the
    ///   frozen `Timedout` lockstep (one frame per miss) and `adopt_missing`'s
    ///   rebase (57 deferred-items § D-5) already accept. Every following tick
    ///   is exact again. The live producer takes this trade because the
    ///   alternative is a synchronous cold seek on the thread that owes the
    ///   presenter a frame every 33 ms.
    pub fn set_ended_nonblocking(&mut self, on: bool) {
        self.ended_nonblocking = on;
    }

    /// Re-arm the per-pull bound [`LayerDecoderPool::new`] was given, for the
    /// pulls that follow this call.
    ///
    /// # Why this exists (debug session
    /// `render-cache-cannot-assemble-six-layers-at-a-seek`, 2026-08-27)
    ///
    /// [`LayerDecoderPool::prewarm`] SPAWNS; it does not make a session READY.
    /// Its own doc says so — *"Still PULLING NOTHING"* — and it is the right
    /// design for its original caller, the live producer, which prewarms
    /// `LOOKAHEAD_US` ahead of the demand so real wall clock elapses before the
    /// first pull. A caller that prewarms and pulls in the SAME breath gets no
    /// such interval, and then the first pull is a COLD one: spawn, seek,
    /// decode-to-position, first frame down the pipe. Measured on six
    /// concurrent 4K sources at a two-second seek that is **2 655–3 159 ms**,
    /// against the 1 000 ms both existing callers construct this pool with.
    ///
    /// The `Timedout` arm then omits every session whose `last` is still `None`
    /// — correct for a live tick (black shows through, the next tick is fine)
    /// and fatal for the render-cache writer, which reads a short composite as
    /// proof that the arrangement can never be cached.
    ///
    /// So the bound is a property of the TICK, not of the pool: a cold first
    /// pull and a warm steady-state pull are different questions and were being
    /// asked with one number. This setter is how a caller says which one it is
    /// asking. It changes nothing on its own — no existing caller invokes it,
    /// and `new()`'s value stands until it does.
    pub fn set_pull_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// The per-pull bound currently in force (the `new()` argument, or the last
    /// [`LayerDecoderPool::set_pull_timeout`]). Exposed so a caller that raises
    /// the bound for one tick can restore its own value rather than re-deriving
    /// it from a constant two files away.
    pub fn pull_timeout(&self) -> Duration {
        self.timeout
    }

    /// Number of live decoder sessions (one long-lived ffmpeg child each).
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Whether a live session exists for `clip_id` (debug session
    /// `multitrack-preview-lag-choppy`, 2026-08-01 — introspection for the
    /// pending-pool adopt handshake; additive, touches no lifecycle rule).
    pub fn has_session(&self, clip_id: &str) -> bool {
        self.sessions.contains_key(clip_id)
    }

    /// Copy `other`'s probed-dims cache entries into this pool (insert-if-
    /// absent). Debug session `multitrack-preview-lag-choppy`, 2026-08-01:
    /// lets a PENDING (prewarm) pool reuse the LIVE pool's dims so prewarming
    /// an incoming layer whose media is already playing costs ZERO ffprobe
    /// spawns on the producer thread. Additive only — never overwrites.
    ///
    /// Plan 59-25: the entry copied is the WHOLE [`CachedProbe`] (dims **and**
    /// the probe result that produced them), which is what takes a seeded
    /// prewarm from one ffprobe per spawn to ZERO — the decoder-internal probe
    /// `pool_adopt.rs`'s differential pin used to record as the irreducible
    /// remainder is now skipped too.
    pub fn seed_dims_from(&mut self, other: &LayerDecoderPool) {
        for (path, entry) in &other.dims {
            self.dims
                .entry(path.clone())
                .or_insert_with(|| entry.clone());
        }
    }

    /// Adopt into this (live) pool every session `other` (a PENDING/prewarmed
    /// pool) holds for a clip in `active` that this pool does NOT yet have,
    /// REBASING each adopted session's `next_source_us` to the demand in
    /// `active` so the immediately-following [`LayerDecoderPool::advance`]
    /// reads it as sequential and serves the already-buffering child's frames
    /// WARM instead of respawning cold.
    ///
    /// Debug session `multitrack-preview-lag-choppy` (2026-08-01): a prewarm
    /// probed at `prod_pos + LOOKAHEAD_US` decodes from up to ~one frame past
    /// the boundary's source position; the rebase accepts that ≤1-frame
    /// content offset — the SAME class the frozen Timedout-lockstep already
    /// accepts (one frame per miss) — instead of failing the `step/2`
    /// sequential check and going cold (which threw the prewarm away ~50% of
    /// the time and ALWAYS for in-section cuts, whose prewarmed session the
    /// next advance's retain reaped from the live pool).
    ///
    /// `other`'s dims cache is merged wholesale (insert-if-absent). Sessions
    /// for clips NOT in `active` are LEFT in `other` — the caller drops the
    /// husk (children reaped by the existing `Drop` impls) when the boundary
    /// latch clears or the pending pool is consumed wholesale. Everything
    /// about `advance`/`prewarm`/retain is byte-untouched.
    pub fn adopt_missing(&mut self, other: &mut LayerDecoderPool, active: &[LayerFrameSource]) {
        for (path, entry) in &other.dims {
            self.dims
                .entry(path.clone())
                .or_insert_with(|| entry.clone());
        }
        for src in active {
            if self.sessions.contains_key(&src.clip_id) {
                continue; // live session wins — never disturb a streaming child
            }
            if let Some(mut s) = other.sessions.remove(&src.clip_id) {
                s.next_source_us = src.source_us; // rebase to the boundary demand
                self.sessions.insert(src.clip_id.clone(), s);
                self.failed.remove(&src.clip_id);
            }
        }
    }

    /// Pre-spawn sessions for `active` WITHOUT pulling (Phase 18.3 Stage 3 —
    /// anticipatory pre-roll, the GStreamer about-to-finish analog). For each
    /// source not already sessioned: cached-probe dims, `ExportRunDecoder::start`,
    /// insert with `next_source_us = source_us` and `last = None`. The first real
    /// `advance()` demanding `source_us` then passes the sequential check and the
    /// already-running children answer at the production timeout. Failures record
    /// the same WR-03 backoff as `advance()`. ADDITIVE ONLY: `advance()`/retain
    /// and all existing behavior are byte-unchanged; prewarm never reaps current
    /// sessions (the CURRENT set is still playing — the union is bounded by
    /// current + incoming, transiently, until the boundary's first `advance`
    /// retains only the new active set).
    /// Plan 59-18: the per-source `ExportRunDecoder::start` calls are issued
    /// CONCURRENTLY, in the same two-phase shape `advance`'s deferred re-roots
    /// already use (see step 3 there). Selection stays serial and byte-identical;
    /// only the spawns move.
    pub fn prewarm(&mut self, active: &[LayerFrameSource]) {
        let step_us = self.step_us;
        let fps = self.fps;

        // PHASE 1 — SELECTION, on the calling thread, byte-identical to the
        // pre-59-18 serial loop: the same filters in the same order, deciding
        // exactly the same set of sources. Nothing here may move into a worker
        // — `probe()` mutates `self.dims` / `self.failed`, so hoisting it would
        // need `&mut self` to cross threads, and that is not what this buys.
        let mut pending: Vec<PendingPrewarm> = Vec::new();
        for src in active {
            // Idempotence: an already-sessioned clip is left alone — its live
            // decoder keeps streaming; disturbing it would trip the sequential
            // check at the boundary.
            if self.sessions.contains_key(&src.clip_id) {
                continue;
            }
            // ...and the same guard against a clip_id appearing TWICE in one
            // `active`. The serial loop got this for free (its first pass
            // inserted the session the second pass then skipped on); with the
            // insertion deferred to phase 2 it has to be explicit, or a
            // duplicated id would spawn two children and drop one.
            if pending.iter().any(|p| p.clip_id == src.clip_id) {
                continue;
            }
            // Failure backoff — the same WR-03 discipline as advance() step 0:
            // a clip whose probe/start recently failed is skipped entirely.
            if let Some(t) = self.failed.get(&src.clip_id) {
                if t.elapsed() < FAILURE_BACKOFF {
                    continue;
                }
            }
            // Native upright dims + the probe that produced them (cached) —
            // mirror of advance() step 2a. The `info` clone is the ONLY new
            // cost here (a handful of small heap strings, once per source that
            // is actually about to be spawned); it buys the worker the probe
            // result so its `ExportRunDecoder` does not spawn an `ffprobe`
            // child of its own.
            let (nw, nh, info) = match self.dims.get(&src.path) {
                Some(c) => (c.dims.0, c.dims.1, c.info.clone()),
                None => {
                    let info = match probe(&src.path) {
                        Ok(i) => i,
                        Err(_) => {
                            self.failed.insert(src.clip_id.clone(), Instant::now());
                            continue; // unresolvable media → nothing to prewarm
                        }
                    };
                    let d = match src.rotation % 360 {
                        90 | 270 => (info.height, info.width),
                        _ => (info.width, info.height),
                    };
                    self.dims.insert(
                        src.path.clone(),
                        CachedProbe {
                            dims: d,
                            info: info.clone(),
                        },
                    );
                    self.failed.remove(&src.clip_id);
                    (d.0, d.1, info)
                }
            };
            // The spawn's arguments — mirror of advance() step 2b. The decode
            // cadence honors the source's own per-tick step exactly as
            // advance() does (export SR-3 parity — a prewarmed retimed layer
            // must stream at `fps / tempo`, or the adopt would hand the
            // boundary a session whose frames land on the wrong cadence).
            let src_step = src.src_step_us.max(1);
            pending.push(PendingPrewarm {
                clip_id: src.clip_id.clone(),
                path: src.path.clone(),
                source_us: src.source_us,
                dur_us: src.remaining_dur_us.max(step_us).max(src_step),
                rotation: src.rotation,
                nw,
                nh,
                decode_fps: fps * (step_us as f64) / (src_step as f64),
                info,
            });
        }
        if pending.is_empty() {
            return;
        }

        // PHASE 2 — THE SPAWNS, CONCURRENTLY (plan 59-18; the same idiom and the
        // same collect-then-insert discipline as `advance`'s step 3, so this
        // file carries ONE concurrency pattern rather than two).
        //
        // Each spawn used to cost one `ffprobe` on top of its child — ~88 ms of
        // the ~102 ms 59-15 measured — because the decoder re-discovered facts
        // this pool had already probed. Plan 59-25 hands the worker the cache's
        // OWN probe result (taken once per path in phase 1 above), so the
        // decoder-internal `ffprobe` child is NOT spawned here at all. What
        // survives is the child spawn itself, plus phase 1's one pool-level
        // probe per path the cache has never seen — and, at the un-wired
        // `!sequential` restart in `advance` (which holds no probe result to
        // pass), the internal probe as before.
        //
        // The concurrency is 59-18's and is unchanged: started inline these
        // spawns SERIALIZE on the caller. 59-15 measured exactly this shape for
        // the Ended arm's re-roots — three starts cost 345.9 ms serialized
        // against 135.5 ms concurrent — and 59-16 measured the same loop HERE,
        // on the ONE exit-discovery tick `prewarm_boundary_stack` runs on, as
        // the residual under its surviving `max_ms = 233.35 ms`.
        //
        // Still PULLING NOTHING: each child's reader thread fills its bounded
        // channel autonomously, so the boundary's first advance() (demanding
        // exactly this source_us → sequential check passes) finds frames
        // already waiting. The scope joins every worker before returning, so
        // nothing outlives this call and no state crosses a tick boundary.
        let started: Vec<(PendingPrewarm, Result<ExportRunDecoder, crate::EngineError>)> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = pending
                    .into_iter()
                    .map(|p| {
                        scope.spawn(move || {
                            // `Some(&p.info)`: the probe of THIS path, taken by
                            // phase 1 (or merged whole from another pool), so
                            // the callee's "same path" contract holds by
                            // construction — the entry is keyed by the path it
                            // was probed from and the two halves are never
                            // separated.
                            let started = ExportRunDecoder::start_with_probed(
                                &p.path,
                                p.source_us,
                                p.dur_us,
                                p.rotation,
                                p.nw,
                                p.nh,
                                p.decode_fps,
                                Some(&p.info),
                            );
                            (p, started)
                        })
                    })
                    .collect();
                // `start` errors (never panics) on locate/spawn failure, so a
                // join failure is unreachable in practice; if one ever happened
                // the clip simply keeps no session and the next `advance()`
                // starts it cold — the pre-prewarm behaviour, never a stuck
                // layer or a half-built pool.
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
        // The session insertion / failure recording the inline version did,
        // unchanged and back on the calling thread.
        for (p, res) in started {
            match res {
                Ok(decoder) => {
                    self.sessions.insert(
                        p.clip_id.clone(),
                        PoolSession {
                            decoder,
                            next_source_us: p.source_us,
                            last: None,
                            delivered_since_spawn: false,
                        },
                    );
                    self.failed.remove(&p.clip_id);
                }
                Err(_) => {
                    self.failed.insert(p.clip_id.clone(), Instant::now());
                }
            }
        }
    }

    /// Reconcile the active set, pull one frame per source, and return
    /// `(clip_id, Frame)` pairs in the SAME order as `active`.
    ///
    /// A layer that produced no frame at all (a first-tick timeout with no HOLD
    /// source, or a probe / decoder-start / fallback-decode failure) is omitted
    /// this tick — black shows through, mirroring the export skip.
    pub fn advance(&mut self, active: &[LayerFrameSource]) -> Vec<(String, Frame)> {
        let step_us = self.step_us;
        let fps = self.fps;
        let timeout = self.timeout;

        // 1. Retain only currently-active clips — dropping a leaver's session
        //    runs its `Drop` (kill()+wait()+drain-then-join), freeing the child.
        let active_ids: HashSet<&str> = active.iter().map(|s| s.clip_id.as_str()).collect();
        self.sessions.retain(|id, _| active_ids.contains(id.as_str()));
        // Prune backoff entries for clips that left the active set so the map
        // cannot grow beyond the active layers (T-18.2-04-01).
        self.failed.retain(|id, _| active_ids.contains(id.as_str()));
        // Same bound for the EOF holds — one frame per ACTIVE clip, never more.
        self.eof_holds.retain(|id, _| active_ids.contains(id.as_str()));

        let mut out: Vec<(String, Frame)> = Vec::with_capacity(active.len());
        // Re-roots decided by the nonblocking `Ended` arm, spawned together
        // after the loop (see step 3).
        let mut reroots: Vec<PendingReroot> = Vec::new();
        for src in active {
            // 0. Failure backoff: a clip whose probe/start recently failed is
            //    skipped entirely (no ffprobe/ffmpeg respawn) until the backoff
            //    elapses — mirrors native_surface.rs's session_failed discipline (WR-03).
            if let Some(t) = self.failed.get(&src.clip_id) {
                if t.elapsed() < FAILURE_BACKOFF {
                    // No respawn this tick (WR-03) — but if the layer reached
                    // an unrecoverable end of stream it HOLDS its last frame
                    // rather than blinking out of the composite, mirroring the
                    // coordinator's EOF hold (see `eof_holds`).
                    if let Some(f) = self.eof_holds.get(&src.clip_id) {
                        out.push((src.clip_id.clone(), f.clone()));
                    }
                    continue;
                }
            }

            // 2a. Native upright dims (cached probe) → identity `scale`.
            //
            //     The cache entry also carries the probe result itself
            //     (plan 59-25); this step reads only the dims, so the steady
            //     per-tick path is byte-unchanged and pays no clone. The two
            //     `Ended`-arm spawns below re-read the entry for its `info`
            //     when — and only when — they are actually about to spawn.
            let (nw, nh) = match self.dims.get(&src.path) {
                Some(c) => c.dims,
                None => {
                    let info = match probe(&src.path) {
                        Ok(i) => i,
                        Err(_) => {
                            // Record the failure so the next tick backs off
                            // instead of re-probing (WR-03).
                            self.failed.insert(src.clip_id.clone(), Instant::now());
                            continue; // unresolvable media → black shows through
                        }
                    };
                    let d = match src.rotation % 360 {
                        90 | 270 => (info.height, info.width),
                        _ => (info.width, info.height),
                    };
                    self.dims
                        .insert(src.path.clone(), CachedProbe { dims: d, info });
                    // Media resolved → clear any prior backoff for this clip.
                    self.failed.remove(&src.clip_id);
                    d
                }
            };

            // 2b. Reuse the clip's decoder when this tick continues it in order;
            //     otherwise (first activation OR a non-sequential jump / seek)
            //     (re)start it at this source_us for the remaining span.
            //
            //     Debug session `multitrack-preview-lag-choppy` (2026-08-01,
            //     round 2 — export SR-3 parity, quick task 260730-x2t): the
            //     continuation test and the session lockstep run in the
            //     SOURCE step this tick represents (`src.src_step_us`), not
            //     the pool's output step. Un-retimed sources carry
            //     `src_step_us == step_us`, keeping every pre-retime behavior
            //     identical; a constant-speed retimed clip advances source by
            //     `tempo * step` per tick, which the old hard-coded `step/2`
            //     tolerance judged non-sequential EVERY tick → a full ffmpeg
            //     respawn per output frame per retimed layer (pinned RED by
            //     tests/pool_retime.rs: 45 spawns / 45 ticks) — and on media
            //     where spawn+seek+first-frame exceeds the pull timeout the
            //     layer never delivered at all (omitted from the composite).
            let src_step = src.src_step_us.max(1);
            let sequential = self
                .sessions
                .get(&src.clip_id)
                .map(|s| (s.next_source_us - src.source_us).abs() <= (src_step / 2).max(1))
                .unwrap_or(false);
            if !sequential {
                let dur = src.remaining_dur_us.max(step_us).max(src_step);
                // Constant speed rides the decoder's own `fps=` filter DIVIDED
                // by the speed (`fps * step / src_step` == `fps / tempo`): the
                // filter samples the SOURCE timeline, so consecutive emitted
                // frames are `src_step` of source apart — one pull per output
                // tick lands exactly on the demanded cadence, the same lever
                // the export pool uses (SR-3). Un-retimed: `fps * 1.0`,
                // bit-identical to before.
                let decode_fps = fps * (step_us as f64) / (src_step as f64);
                match ExportRunDecoder::start(
                    &src.path,
                    src.source_us,
                    dur,
                    src.rotation,
                    nw,
                    nh,
                    decode_fps,
                ) {
                    Ok(decoder) => {
                        self.sessions.insert(
                            src.clip_id.clone(),
                            PoolSession {
                                decoder,
                                next_source_us: src.source_us,
                                last: None,
                                delivered_since_spawn: false,
                            },
                        );
                        // Decoder started → clear any prior backoff for this clip.
                        self.failed.remove(&src.clip_id);
                    }
                    Err(_) => {
                        // Record the failure so the next tick backs off instead
                        // of re-spawning ffmpeg (WR-03).
                        self.failed.insert(src.clip_id.clone(), Instant::now());
                        continue; // spawn failure → skip this layer this tick
                    }
                }
            }

            // 2c. Bounded pull. Ready advances + records the HOLD source;
            //     Timedout HOLDs the last frame (keep session); Ended re-roots
            //     the session at the demand with a replacement streaming child
            //     (59-15 / § D-2 — see the arm's own commentary below).
            let pull = self
                .sessions
                .get(&src.clip_id)
                .map(|s| s.decoder.try_next_frame(timeout));
            match pull {
                Some(FramePull::Ready(f)) => {
                    if let Some(s) = self.sessions.get_mut(&src.clip_id) {
                        // Lockstep in the SOURCE step (export SR-3 parity —
                        // see the sequential check above): the next tick's
                        // demand advances by `src_step`, not the output step.
                        s.next_source_us = src.source_us + src_step;
                        s.last = Some(f.clone());
                        // This child has now answered at least once — the
                        // `Ended` arm's storm guard reads exactly this.
                        s.delivered_since_spawn = true;
                    }
                    // A layer that is decoding again is not at an end of
                    // stream: release any parked EOF hold (the software twin
                    // of the coordinator's reposition release).
                    self.eof_holds.remove(&src.clip_id);
                    out.push((src.clip_id.clone(), f));
                }
                Some(FramePull::Timedout) => {
                    if let Some(s) = self.sessions.get_mut(&src.clip_id) {
                        // Keep the session in lockstep with the self-paced play
                        // clock so a warming/stalled decoder is NOT misread as a
                        // seek next tick and torn down/respawned (WR-01/WR-02).
                        s.next_source_us = src.source_us + src_step;
                        if let Some(f) = s.last.clone() {
                            out.push((src.clip_id.clone(), f)); // HOLD; session genuinely stays alive
                        }
                        // else: no frame yet this tick → skip (black shows through), session retained
                    }
                }
                // Plan 59-15 — Phase 57 deferred-items § D-2, owner-authorized.
                //
                // The old arm cold-decoded HERE: `self.sessions.remove(..)`
                // then a one-shot precise decode of `src.source_us` through
                // `ffmpeg.rs`'s single-frame helper —
                // a fresh ffprobe + accurate seek + full single-frame decode,
                // SYNCHRONOUS, on the producer thread. Measured
                // (`boundary_exit_cost_probe.rs`): 1058.9 ms on 4K Long-GOP,
                // 284.3 ms on a 720p control. Three same-tick stream ends
                // serialize into F5's founding ~1.0 s clip-cut ceiling
                // (`59-CEILING-VERDICT § 3`), reproduced headlessly at
                // 771.5 ms by `tests/pool_ended.rs::ended_same_tick_...`.
                //
                // It is replaced by a streaming REPLACEMENT session re-rooted
                // at the demand — the child performs its accurate seek in its
                // OWN process, exactly the async discipline `prewarm()`
                // documents ("PULL NOTHING: the reader thread fills its
                // bounded channel autonomously").
                //
                // NOT § D-2's own sketched fix ("HOLD `s.last`, as `Timedout`
                // already does"). That is not equivalent: `Timedout` holds
                // because the decoder has not produced YET, so `s.last` is
                // still the right picture; `Ended` means the window is OVER
                // and the old decode fetched a GENUINELY CORRECT frame the
                // dead session cannot supply. An unconditional hold presents a
                // stale frame where EXPORT decodes the correct one — a
                // preview == export parity break (57 D-12, 58 D-21, 59 D-28),
                // the invariant every phase of this milestone pins at MAD
                // 0.0000. `tests/pool_ended.rs` pins the difference on pixels.
                Some(FramePull::Ended) => {
                    // Take the dying session's HOLD source and its storm-guard
                    // flag; dropping it here reaps the (already-exited) child.
                    let (held, had_delivered) = match self.sessions.remove(&src.clip_id) {
                        Some(s) => (s.last, s.delivered_since_spawn),
                        None => (None, false),
                    };

                    if !had_delivered {
                        // This session never answered a single pull since ITS
                        // spawn: a replacement that seeked past the media's
                        // real end, or unreadable media. Re-rooting again
                        // would spawn one ffmpeg child per tick forever
                        // (T-59-15-02 — measured at RED: 14 spawns across 15
                        // post-EOF ticks). Back off (WR-03) and stay loud:
                        // debug session `multitrack-preview-lag-choppy` made
                        // this failure diagnosable from stderr and it must
                        // remain so, held frame or not.
                        self.failed.insert(src.clip_id.clone(), Instant::now());
                        POOL_ENDED_GIVEUPS.fetch_add(1, Ordering::SeqCst);
                        eprintln!(
                            "preview pool: layer {} stream ended having produced nothing \
                             since its spawn (replacement seeked past the media's real end, \
                             or the media is unreadable) at {}us — backing off for {:?}; {}",
                            src.clip_id,
                            src.source_us,
                            FAILURE_BACKOFF,
                            if held.is_some() {
                                "holding its last frame"
                            } else {
                                "the layer is omitted (black shows through)"
                            }
                        );
                        if let Some(f) = held {
                            POOL_ENDED_HOLDS.fetch_add(1, Ordering::SeqCst);
                            // Park it: the backoff ticks that follow serve this
                            // frame instead of dropping the layer.
                            self.eof_holds.insert(src.clip_id.clone(), f.clone());
                            out.push((src.clip_id.clone(), f));
                        }
                        continue;
                    }

                    // The replacement streams at the SOURCE cadence this tick
                    // represents, exactly like the `!sequential` spawn above
                    // (export SR-3 parity: a retimed layer's replacement must
                    // stream at `fps / tempo`).
                    let decode_fps = fps * (step_us as f64) / (src_step as f64);

                    // The pool's OWN probe of THIS path — step 2a above either
                    // found the entry or has just inserted it, so this is a hit
                    // in practice. Cloned here (only on a stream-end tick, never
                    // on a steady one) so the replacement spawn can skip its
                    // decoder-internal `ffprobe`.
                    //
                    // `None` is unreachable today and is deliberately the SAFE
                    // answer if it ever became reachable: the callee then probes
                    // exactly as it always has. The one thing this must never do
                    // is hand a spawn a `MediaInfo` that might describe a
                    // DIFFERENT file — that failure is silent and visual.
                    let cached_info: Option<MediaInfo> =
                        self.dims.get(&src.path).map(|c| c.info.clone());

                    if self.ended_nonblocking {
                        // ---- LIVE PRODUCER (ring opt-in) ----
                        // Unblock THIS tick: hold, and re-root at the NEXT
                        // demand so the following tick's sequential check
                        // (`src_step/2`) passes and pulls the EXACT frame.
                        // The spawn itself is deferred to after this loop so
                        // that N layers ending on one tick spawn CONCURRENTLY
                        // (measured: 3 serialized starts 345.9 ms vs 135.5 ms
                        // concurrent) — the same-tick collapse this plan's
                        // objective is about.
                        if src.remaining_dur_us > src_step {
                            reroots.push(PendingReroot {
                                clip_id: src.clip_id.clone(),
                                path: src.path.clone(),
                                root_us: src.source_us + src_step,
                                dur_us: (src.remaining_dur_us - src_step)
                                    .max(step_us)
                                    .max(src_step),
                                rotation: src.rotation,
                                nw,
                                nh,
                                decode_fps,
                                last: held.clone(),
                                info: cached_info,
                            });
                        }
                        // else: the clip's final tick — there is no next
                        // demand to re-root toward; the clip leaves the active
                        // set and `retain` reaps everything.
                        if let Some(f) = held {
                            POOL_ENDED_HOLDS.fetch_add(1, Ordering::SeqCst);
                            out.push((src.clip_id.clone(), f));
                        }
                    } else {
                        // ---- DEFAULT: BLOCKING-EXACT ----
                        // The render-cache WRITER and every export-shaped
                        // caller. Spawn AT the demand and pull with this
                        // pool's own timeout: the delivered pixels are
                        // byte-identical to the old arm's (MAD 0.0000 vs the
                        // one-shot precise decode, pinned frame- AND
                        // composite-level by `tests/pool_ended.rs`), but the
                        // mechanism is strictly cheaper — no pool-level
                        // ffprobe (dims are cached), no decoder-internal one
                        // either since 59-25 (the cache's probe result is
                        // handed in), and the replacement PERSISTS as a
                        // streaming session, where the old arm's remove-session
                        // made the NEXT tick pay a second cold spawn.
                        let dur = src.remaining_dur_us.max(step_us).max(src_step);
                        match ExportRunDecoder::start_with_probed(
                            &src.path,
                            src.source_us,
                            dur,
                            src.rotation,
                            nw,
                            nh,
                            decode_fps,
                            cached_info.as_ref(),
                        ) {
                            Ok(decoder) => {
                                POOL_ENDED_REROOTS.fetch_add(1, Ordering::SeqCst);
                                let mut s = PoolSession {
                                    decoder,
                                    next_source_us: src.source_us,
                                    last: held,
                                    delivered_since_spawn: false,
                                };
                                match s.decoder.try_next_frame(timeout) {
                                    FramePull::Ready(f) => {
                                        s.delivered_since_spawn = true;
                                        s.next_source_us = src.source_us + src_step;
                                        s.last = Some(f.clone());
                                        self.sessions.insert(src.clip_id.clone(), s);
                                        self.failed.remove(&src.clip_id);
                                        out.push((src.clip_id.clone(), f));
                                    }
                                    FramePull::Timedout => {
                                        // Lockstep (WR-01/WR-02) so the warming
                                        // replacement is not misread as a seek.
                                        s.next_source_us = src.source_us + src_step;
                                        if let Some(f) = s.last.clone() {
                                            POOL_ENDED_HOLDS.fetch_add(1, Ordering::SeqCst);
                                            out.push((src.clip_id.clone(), f));
                                        }
                                        self.sessions.insert(src.clip_id.clone(), s);
                                    }
                                    FramePull::Ended => {
                                        // The replacement ended without a frame
                                        // — a genuine past-EOF demand. Same
                                        // guard, same loud line.
                                        self.failed.insert(src.clip_id.clone(), Instant::now());
                                        POOL_ENDED_GIVEUPS.fetch_add(1, Ordering::SeqCst);
                                        eprintln!(
                                            "preview pool: layer {} stream ended and its \
                                             replacement at {}us produced nothing either — \
                                             backing off for {:?}; {}",
                                            src.clip_id,
                                            src.source_us,
                                            FAILURE_BACKOFF,
                                            if s.last.is_some() {
                                                "holding its last frame"
                                            } else {
                                                "the layer is omitted (black shows through)"
                                            }
                                        );
                                        if let Some(f) = s.last {
                                            POOL_ENDED_HOLDS.fetch_add(1, Ordering::SeqCst);
                                            self.eof_holds
                                                .insert(src.clip_id.clone(), f.clone());
                                            out.push((src.clip_id.clone(), f));
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                self.failed.insert(src.clip_id.clone(), Instant::now());
                                POOL_ENDED_GIVEUPS.fetch_add(1, Ordering::SeqCst);
                                eprintln!(
                                    "preview pool: layer {} stream ended and the replacement \
                                     spawn at {}us failed: {e}",
                                    src.clip_id, src.source_us
                                );
                                if let Some(f) = held {
                                    POOL_ENDED_HOLDS.fetch_add(1, Ordering::SeqCst);
                                    out.push((src.clip_id.clone(), f));
                                }
                            }
                        }
                    }
                }
                None => {} // no session (start skipped) → omit this layer
            }
        }

        // 3. The nonblocking mode's deferred re-roots, spawned CONCURRENTLY.
        //
        //    Each spawn used to cost one `ffprobe` on top of its child (~88 ms
        //    of the ~102 ms measured), because the decoder re-discovered what
        //    the pool had already probed. Since 59-25 these re-roots hand in
        //    the cache's own probe result and that child is not spawned:
        //    the surviving `ffprobe` cost in this file is phase 1's one
        //    pool-level probe per NEW path, plus the `!sequential` restart
        //    above, which holds no probe result to pass and still probes
        //    internally. The spawn itself remains, and it is why these are
        //    still issued together: serialized, three same-tick ends cost
        //    345.9 ms; spawned here in parallel they cost 135.5 ms, and each
        //    child then performs its accurate seek in its own process while the
        //    producer walks on. The scope joins every worker before returning,
        //    so nothing outlives this call and no state crosses a tick
        //    boundary.
        //
        //    (The scoping API's PATH is deliberately spelled only at the two
        //    real call sites — here and in `prewarm` — because plan 59-18's
        //    acceptance gate is a textual `grep -c` for it against this file,
        //    asserting exactly two: one idiom, two sites. Prose that names it
        //    makes that gate unreadable, the same way it did for 59-15's own
        //    removed-helper gate.)
        if !reroots.is_empty() {
            let started: Vec<(PendingReroot, Result<ExportRunDecoder, crate::EngineError>)> =
                std::thread::scope(|scope| {
                    let handles: Vec<_> = reroots
                        .into_iter()
                        .map(|r| {
                            scope.spawn(move || {
                                // The cache's probe of THIS path, carried
                                // across from the tick that decided the re-root
                                // (see `PendingReroot::info`).
                                let started = ExportRunDecoder::start_with_probed(
                                    &r.path,
                                    r.root_us,
                                    r.dur_us,
                                    r.rotation,
                                    r.nw,
                                    r.nh,
                                    r.decode_fps,
                                    r.info.as_ref(),
                                );
                                (r, started)
                            })
                        })
                        .collect();
                    // `start` errors (never panics) on locate/spawn failure, so
                    // a join failure is unreachable in practice; if one ever
                    // happened the clip simply keeps no session and the next
                    // tick's sequential check restarts it cold — the
                    // pre-59-15 behaviour, never a stuck layer.
                    handles.into_iter().filter_map(|h| h.join().ok()).collect()
                });
            for (r, res) in started {
                match res {
                    Ok(decoder) => {
                        POOL_ENDED_REROOTS.fetch_add(1, Ordering::SeqCst);
                        self.sessions.insert(
                            r.clip_id.clone(),
                            PoolSession {
                                decoder,
                                next_source_us: r.root_us,
                                last: r.last,
                                delivered_since_spawn: false,
                            },
                        );
                        self.failed.remove(&r.clip_id);
                    }
                    Err(e) => {
                        self.failed.insert(r.clip_id.clone(), Instant::now());
                        POOL_ENDED_GIVEUPS.fetch_add(1, Ordering::SeqCst);
                        eprintln!(
                            "preview pool: layer {} stream ended and the replacement spawn \
                             at {}us failed: {e}",
                            r.clip_id, r.root_us
                        );
                    }
                }
            }
        }
        out
    }
}

/// A session [`LayerDecoderPool::prewarm`]'s selection phase has decided to
/// open but not yet spawned. Collected on the calling thread with every skip
/// filter already applied, then started CONCURRENTLY, so N incoming layers pay
/// ~one spawn's latency between them instead of N (plan 59-18 — the twin of
/// [`PendingReroot`] below).
struct PendingPrewarm {
    clip_id: String,
    path: PathBuf,
    /// Where the session is rooted — also its `next_source_us`, so the
    /// boundary's first `advance()` at this position reads sequential.
    source_us: i64,
    dur_us: i64,
    rotation: u32,
    nw: u32,
    nh: u32,
    decode_fps: f64,
    /// The pool's cached `probe()` of `path`, cloned at selection time so the
    /// worker owns it (plan 59-25). Not optional here: selection reaches this
    /// point only via a cache hit or a SUCCESSFUL probe of this same `path`, so
    /// the value always exists and always describes `path`.
    info: MediaInfo,
}

/// A re-root the nonblocking (`Ended`) arm has decided on but not yet spawned.
/// Collected during `advance`'s per-source loop and started CONCURRENTLY once
/// the loop ends, so N layers ending on the same tick pay ~one spawn's latency
/// between them instead of N.
struct PendingReroot {
    clip_id: String,
    path: PathBuf,
    /// Where the replacement is rooted — the NEXT demand, so the following
    /// tick reads it as sequential.
    root_us: i64,
    dur_us: i64,
    rotation: u32,
    nw: u32,
    nh: u32,
    decode_fps: f64,
    /// The ended session's HOLD source, carried across so a replacement that
    /// has not answered yet still has something to hold.
    last: Option<Frame>,
    /// The pool's cached `probe()` of `path`, cloned on the tick that decided
    /// this re-root (plan 59-25). `Option` — unlike [`PendingPrewarm::info`] —
    /// because it is read with a map lookup rather than produced by the probe
    /// that fills the cache: `None` means "probe as you always did", which is
    /// the only safe answer to "I am not certain I hold this path's info".
    info: Option<MediaInfo>,
}
