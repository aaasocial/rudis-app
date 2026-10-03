//! The program-level render-cache **WRITER** (Phase 59, plan 59-07; decisions
//! D-06, D-14, D-18, D-39, and the writer half of D-40).
//!
//! One call, one segment: [`render_segment`] turns a segment index into one
//! committed cache file — or into a clean abort that leaves nothing behind.
//! Scheduling is deliberately absent. WHEN this runs, how many run at once, what
//! priority they take and who re-enqueues an abort are all `app-core`'s (plan
//! 59-08), which calls INTO this function; a writer that also decided when to
//! write would be two policies in one file and neither of them testable.
//!
//! # Why this lives in `crates/preview` (D-39, resolving OQ-3)
//!
//! The render loop needs the resolver-aware compose path, and two of its
//! helpers — [`crate::multilayer::pool_sources`] and
//! `crate::multilayer::cpu_layer_for_spec` (reached here through the `pub`
//! wrapper [`crate::compose_multilayer_from_pool`], which is nothing but a loop
//! over the latter) — are `pub(crate)`. Widening them to `pub` to buy one caller
//! would push the multilayer resolver's internals across a crate boundary, which
//! is the opposite of what Phases 57 and 58 spent effort building. So the LOOP
//! moved here instead, and **nothing was widened to `pub`**: the two items this
//! module needed from the reader (`configured_dir`, `current_edit_gen`) became
//! `pub(crate)`, which is in-crate access, not a new public surface.
//!
//! # THE TWO STRUCTURAL ABSENCES
//!
//! Both are enforced by source scans over this file
//! (`tests/render_cache_full_res_only.rs`), not by intent:
//!
//! 1. **Full canvas, unconditionally (D-06).** This file names no part of the
//!    playback-degradation machinery at all — not the controller, not the level
//!    type, not the module they live in. Full resolution here is not a parameter
//!    with a default; it is the ABSENCE of any way to ask for anything else.
//!    That matters more than it looks: playback degradation is a *transient*
//!    property that snaps back at the next pause, so a degraded composite baked
//!    into a cache file would make it **permanent and invisible** — the exact
//!    inverse of the contract. The geometry recorded in the segment's meta is
//!    the full canvas, so such a file would also be a *visible* wrong-size
//!    defect rather than a silent one, which is the second half of the pin
//!    (row 18 probes the written payload's real geometry).
//! 2. **No reach into `app-core`'s encode-to-file pipeline.** That loop is the
//!    closest existing walk-and-composite shape in the tree and it is
//!    deliberately NOT reused: it decodes ORIGINALS by design (its whole job is
//!    to produce final-quality output), it mixes audio, and it drives a
//!    different encoder. Composing a cache segment through it would produce
//!    pixels that differ in sharpness from the live playback surrounding the
//!    cached range — precisely what CACHE-02 forbids. `crates/preview` cannot
//!    name `crates/app-core` anyway (the dependency arrow points the other way),
//!    so this absence is structural as well as scanned.
//!
//! # D-14 — the writer decodes what the LIVE path would have decoded
//!
//! Every media layer is sourced through [`crate::multilayer::pool_sources`],
//! which routes each layer through `decode_source::resolve_decode_source`. So a
//! clip with a warm playback proxy is rendered FROM that proxy, exactly as live
//! playback would have rendered it — and the resolved answer is already part of
//! the segment's identity (59-05's D-14 term), so a proxy landing or being
//! evicted re-keys the segment rather than silently changing what a committed
//! file contains. Rendering from originals here would be the *better-looking*
//! choice and the wrong one: entering a cached range would sharpen and leaving
//! it would soften, which is a boundary the user can see.
//!
//! # D-18's write half — a mid-render edit ABORTS, it never commits
//!
//! The segment identity is computed once, from the store state at
//! `gen0`, through the SAME material builder the reader checks against
//! ([`SegmentLookup::segment_hash_memo`]) — so writer and reader cannot disagree
//! about what a segment IS. The generation is then re-read on every tick, and
//! the first time it moves the session is CANCELLED. A segment committed under
//! an identity the world has already moved past would be a file that can never
//! be served and can only be swept; worse, if the edit moved and moved BACK, it
//! would be a file whose identity matches an arrangement it was not rendered
//! from.
//!
//! # Software decode only, and below-normal encode
//!
//! Nothing here opens a hardware decode session. Three hardware slots is the
//! exact resource the six-layer ceiling saturates, so background work that took
//! one would be paying for its own win; `pool_sources` feeds
//! `engine::LayerDecoderPool`, whose sessions are CLI sidecar children. The
//! encoder child is already below-normal priority (59-03), so the one shared
//! hardware encoder is yielded to anything the user is waiting on.
//!
//! # Cost, stated rather than hidden (D-32)
//!
//! Each call builds a FRESH decode pool, so every layer pays a cold child spawn
//! once per segment. That is accepted: this is background work with no presenter
//! waiting on it, and hiding the cost by keeping pools alive between segments
//! would mean holding `ffmpeg` children open against media the user may be
//! editing. [`render_segment`] prints one `RENDER-CACHE-SEGMENT` line per call
//! carrying the split, and `SegmentEncodeSession::finish` prints its own
//! `RENDER-CACHE-GENCOST` line beside it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::render_cache_lookup::{configured_dir, current_edit_gen, SegmentLookup};
use crate::PreviewHost;

/// Frames this process has composited and pushed into a segment encode, across
/// every [`render_segment`] call — including calls that later aborted.
///
/// **The progress observable.** A background render is otherwise completely
/// silent between `begin` and the commit, which leaves two things impossible:
/// 59-08 cannot report progress in the status D-30 asks for, and no gate can
/// establish that a render was genuinely IN FLIGHT before it interfered with it.
/// The second one is not theoretical — the first version of this plan's cancel
/// gate used a wall-clock sleep, and a whole 640x360 segment renders in about
/// 1.3 s here, so the "mid-render" latch landed after the commit and the gate
/// asserted about a render that had already finished. Watching the in-flight
/// file's LENGTH does not answer it either: on Windows a file with a live write
/// handle reports a stale size, so the payload appears to jump from nothing to
/// its final length at the moment the child exits.
///
/// Relaxed, read as a DELTA around a span, never a synchronisation edge, and
/// nothing branches on it — the same discipline as the reader's six counters
/// and `decode_source::PROXY_RESOLVE_NANOS`.
pub static RENDER_CACHE_FRAMES_RENDERED: AtomicU64 = AtomicU64::new(0);

/// Segments this process has COMMITTED. The denominator for
/// [`RENDER_CACHE_FRAMES_RENDERED`] that says how much of that work survived:
/// frames rendered without segments committed is a background worker doing
/// nothing but burn.
pub static RENDER_CACHE_SEGMENTS_COMMITTED: AtomicU64 = AtomicU64::new(0);

/// Segments this process abandoned — cancelled, stale, uncacheable, incomplete
/// or failed, without distinction, because the useful question at a glance is
/// "how much of the background budget produced nothing".
pub static RENDER_CACHE_SEGMENTS_ABORTED: AtomicU64 = AtomicU64::new(0);

/// How long ONE layer's frame pull may block before the pool gives up on it for
/// that tick.
///
/// The same 1 s the producer's live pool uses (`ring.rs`'s `POOL_PULL_TIMEOUT`),
/// and generous for the same reason: a pull returns the instant a frame arrives,
/// so a long bound never slows a healthy decode — it only caps how long a
/// genuinely stalled child can hold this loop. Nothing is waiting on this thread,
/// so the bound is here to keep a stall BOUNDED, not to keep it short.
///
/// Deliberately a second constant rather than a widening of the ring's: that one
/// governs a loop with a presenter downstream of it, and the two are free to
/// diverge the day either has a reason to.
const SEG_POOL_PULL_TIMEOUT: Duration = Duration::from_millis(1_000);

/// How long the **first** pull of a segment may block — the one that runs
/// against sessions [`engine::LayerDecoderPool::prewarm`] spawned microseconds
/// earlier.
///
/// # The number this replaces was measuring the wrong thing
///
/// Debug session `render-cache-cannot-assemble-six-layers-at-a-seek`
/// (2026-08-27). [`SEG_POOL_PULL_TIMEOUT`] above bounds a pull against a
/// STREAMING child: the frame is already decoded and sitting in the reader
/// thread's channel, so a healthy pull returns in microseconds and 1 s is
/// enormous. The first pull of a segment is not that pull. Its child was
/// spawned by the `prewarm` two statements below and has since had to open the
/// file, seek to the segment's start, decode forward to it, scale, and push a
/// full-resolution RGBA frame down a pipe — none of which had begun when the
/// bound started counting, because `prewarm` returns as soon as the SPAWNS are
/// issued (*"Still PULLING NOTHING"*, its own doc).
///
/// Measured on the owner's six-layer minimized 4K stack (fixture F6M), six
/// sessions concurrent, sidecar pinned to `runtime/binaries`:
///
/// | segment start | first frame, per layer | over 1 000 ms? |
/// |---|---|---|
/// | `0 µs` (no seek) | 313 · 409 · 1 756 · 1 758 · 1 778 · 1 798 ms | four of six |
/// | `2 000 000 µs` | 638 · 702 · 2 655 · 3 052 · 3 096 · 3 159 ms | four of six |
/// | `4 000 000 µs` | 745 · 891 · 2 162 · 2 765 · 2 830 · 2 946 ms | four of six |
///
/// Every layer produced — nothing in that arrangement is unmakeable — but under
/// the old bound the segments that need a seek lost their two lowest track
/// indices every time and were declared permanently uncacheable. Segment 0 only
/// escaped because `advance` pulls SERIALLY, so layer *i* inherits the slack of
/// the layers before it, and at `-ss 0` that was just enough.
///
/// # Why ten seconds, and why that is still fast-fail
///
/// ~3× the worst first frame measured idle and ~1.75× the worst measured under
/// six-way concurrent load, which is the margin a background render can afford
/// when a busier machine is exactly when the cache is most wanted. Nothing waits
/// on this thread (the module doc's standing premise), so the bound exists to
/// keep a stall BOUNDED, not short.
///
/// **It does not slow a genuinely uncacheable segment down.** The failures that
/// SHOULD abort instantly still do: a layer whose media cannot be probed or
/// whose child cannot spawn is recorded in the pool's `FAILURE_BACKOFF` map by
/// `prewarm` itself, and `advance` step 0 then skips it **without pulling at
/// all** — so a corrupt or missing source still returns
/// [`RenderOutcome::Incomplete`] in milliseconds, exactly as before. This bound
/// is only ever paid by a session that spawned cleanly and is genuinely
/// decoding, and only on the first tick.
const SEG_POOL_FIRST_TICK_TIMEOUT: Duration = Duration::from_millis(10_000);

/// What one [`render_segment`] call did.
///
/// Every non-`Committed` arm means **no file exists under a final name** — the
/// session was cancelled, which kills and reaps the encoder child and deletes
/// the in-flight payload. The arms are distinguished because 59-08's registry
/// answers them differently: a cancel or a stale abort is worth re-enqueueing,
/// and [`RenderOutcome::NotCacheable`] never is.
#[derive(Debug)]
pub enum RenderOutcome {
    /// One complete, full-canvas segment is on disk under the identity the
    /// reader computes for it, with the meta written LAST.
    Committed {
        /// The committed payload's length on disk (D-32).
        payload_bytes: u64,
        /// Wall-clock milliseconds from `begin` to the completed commit —
        /// resolve, decode, composite and encode, all of it (D-32).
        wall_ms: u64,
        /// Frames the session accepted. D-07's identity, as a number: N frames
        /// pushed must be N frames in the segment.
        frames: u64,
    },
    /// The caller's latch was raised. Nothing was committed.
    Cancelled,
    /// The edit generation moved while the segment was rendering, so the
    /// identity it was being rendered under is no longer the one the material
    /// hashes to (D-18's write half). Nothing was committed.
    StaleAborted,
    /// A tick inside the segment is one the live path serves through the
    /// SINGLE-CLIP streaming route rather than by compositing — so this writer
    /// has no canvas-space render of it, and committing anything for it would be
    /// committing a guess. Nothing was committed.
    ///
    /// This is a property of the ARRANGEMENT, not a transient failure: re-running
    /// the same segment against the same arrangement will answer the same way.
    NotCacheable {
        /// The program time that could not be composited.
        at_us: i64,
    },
    /// A layer that the resolved stack contains produced no frame for a tick, so
    /// the composite would have been missing a layer that live playback shows
    /// once its decoder is warm. Transient by nature (a cold child, a stalled
    /// pull). Nothing was committed.
    Incomplete {
        /// The program time whose composite was short.
        at_us: i64,
        /// Layers that produced a frame.
        got: usize,
        /// Layers the resolved stack contains.
        want: usize,
    },
    /// Something the writer cannot proceed past: no store, no configured cache
    /// directory, an unhashable segment, no GPU compositor, a dead encoder
    /// child, a refused commit. Nothing was committed.
    Failed(engine::EngineError),
}

impl RenderOutcome {
    /// `true` only for [`RenderOutcome::Committed`]. Kept as a method so call
    /// sites and gates read the same way and neither invents a second spelling.
    pub fn is_committed(&self) -> bool {
        matches!(self, RenderOutcome::Committed { .. })
    }
}

/// The offscreen compositor background renders use.
///
/// Built at most once per process and reused by every segment. It is
/// deliberately NOT the presenter's compositor: this function takes only a
/// [`PreviewHost`], and the compositor lives behind the OTHER port
/// (`PresentSink`) precisely because the object that owns the GPU device owns
/// the surface too. A background render must never need the surface, and
/// borrowing the presenter's device would put a whole segment's worth of
/// offscreen work on the same device the user's playback is running on.
///
/// A build failure is not cached as permanent: the slot stays `None` and the
/// next call tries again, because a transient device loss must not disable the
/// cache for the life of the process.
fn background_compositor() -> Result<Arc<engine::Compositor>, engine::EngineError> {
    let slot = BACKGROUND_COMPOSITOR.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(existing) = guard.as_ref() {
        return Ok(existing.clone());
    }
    let built = Arc::new(engine::Compositor::new()?);
    *guard = Some(built.clone());
    Ok(built)
}

/// The slot behind [`background_compositor`], at module scope so
/// [`release_background_compositor`] can empty it.
static BACKGROUND_COMPOSITOR: OnceLock<Mutex<Option<Arc<engine::Compositor>>>> = OnceLock::new();

/// **Phase 71 (TRUST-01): drop the process-lifetime background compositor.**
///
/// D3D12 devices are singletons per adapter (71-05, measured). This slot's
/// compositor is the SAME removed device the preview lost, and while any
/// reference to it lives DXGI will not hand the hardware adapter back, so the
/// preview's recovery would recreate on WARP. Device-lost recovery calls this
/// after it has cancelled and waited out every running bake.
///
/// Returns `Some(strong_count)` as it was just before the slot's reference was
/// dropped (1 = the slot was the only owner, so the device reference is really
/// gone), or `None` when the slot was already empty. A count above 1 means a
/// render still holds a clone, and that clone keeps the device alive until the
/// render returns.
///
/// Nothing else changes: the next [`render_segment`] rebuilds lazily, which is
/// the same path a failed build already takes.
pub fn release_background_compositor() -> Option<usize> {
    let slot = BACKGROUND_COMPOSITOR.get()?;
    let taken = slot.lock().unwrap_or_else(|p| p.into_inner()).take()?;
    let count = Arc::strong_count(&taken);
    // No `poll(Wait)` first: this runs on the UI thread against a REMOVED
    // device, where a blocking wait is an unbounded stall, and dropping the last
    // owner releases the device, adapter and instance with it (71-05 census:
    // the count reached 1 once the owners dropped).
    drop(taken);
    Some(count)
}

/// A typed error for the writer's own refusals, so every one of them carries a
/// sentence naming what was missing rather than a bare `None` the caller has to
/// guess about.
fn refused(detail: String) -> engine::EngineError {
    engine::EngineError::SidecarFailed {
        tool: "render-cache segment writer".to_string(),
        status: -1,
        stderr: detail,
    }
}

/// Are there any active video layers at `at_us`?
///
/// The question separates the two reasons the multi-layer resolver answers
/// `None`: a genuine timeline GAP (nothing is there, and the live path shows
/// black) from the DEGENERATE single-layer case (one identity clip, which the
/// live path shows through the streaming route without compositing). The two
/// must not be treated alike — see [`RenderOutcome::NotCacheable`].
///
/// `None` here means the store could not be read at all.
fn has_active_layers(host: &dyn PreviewHost, at_us: i64) -> Option<bool> {
    let guard = host.store()?;
    Some(!guard.timeline().active_layers_at(at_us).is_empty())
}

/// **Render segment `seg_index` and commit it.**
///
/// Walks every tick the segment covers — `[seg_start, seg_end)` stepping one
/// project frame — resolves the multi-layer stack at each, composites it at the
/// FULL project canvas through the same resolver-aware path live playback uses,
/// pushes the frame into a [`rendercache::SegmentEncodeSession`], and commits.
///
/// `cancel` is polled before every tick. `abort` conditions and their outcomes
/// are enumerated on [`RenderOutcome`]; in every one of them the session is
/// cancelled, which means the encoder child is killed and reaped and the
/// in-flight payload is deleted, so no path through this function can leave a
/// file under a final name that is not a complete segment.
///
/// The cache directory is the one the READER probes
/// (`render_cache_lookup::configure_render_cache_dir`), read here rather than
/// passed in so writer and reader can never be pointed at two different places.
/// An unconfigured process cannot render a segment at all, which is the same
/// compatibility floor the reader has.
pub fn render_segment(
    host: &dyn PreviewHost,
    seg_index: i64,
    cancel: &AtomicBool,
) -> RenderOutcome {
    let outcome = render_segment_inner(host, seg_index, cancel);
    // Counted HERE rather than at each return, so no exit path can forget and
    // no future arm can be added without being counted.
    if outcome.is_committed() {
        RENDER_CACHE_SEGMENTS_COMMITTED.fetch_add(1, Ordering::Relaxed);
    } else {
        RENDER_CACHE_SEGMENTS_ABORTED.fetch_add(1, Ordering::Relaxed);
    }
    outcome
}

/// [`render_segment`]'s decision, with the counting lifted out — the same shape,
/// and for the same reason, as the reader's `probe` / `probe_inner` split.
fn render_segment_inner(
    host: &dyn PreviewHost,
    seg_index: i64,
    cancel: &AtomicBool,
) -> RenderOutcome {
    let started = Instant::now();

    // ---- (a) the canvas, and the generation everything below belongs to ----
    //
    // Read through the same borrow-only accessor `resolve_multilayer` uses, so
    // the geometry the session is opened with is the geometry the composites
    // will carry.
    let Some((canvas_w, canvas_h, fps)) = host.store().map(|guard| guard.project_canvas()) else {
        return RenderOutcome::Failed(refused(
            "no project store: a background render cannot identify, let alone \
             composite, a segment"
                .to_string(),
        ));
    };
    let canvas = (canvas_w, canvas_h, fps);
    let gen0 = current_edit_gen(host);
    let step = engine::frame_step_us(fps).max(1);
    let (seg_start, seg_end) = rendercache::segment_bounds(seg_index);

    // ---- (b) the identity, from the READER's own material builder ----
    //
    // `None` means the segment cannot be identified — a clip whose media cannot
    // be stat-ed, a store that vanished. Fail closed and encode nothing: a
    // caller that cannot prove what it is rendering must not name the result.
    let mut lookup = SegmentLookup::new();
    let Some(hash) = lookup.segment_hash_memo(host, seg_index, gen0, canvas) else {
        return RenderOutcome::Failed(refused(format!(
            "segment {seg_index} is unhashable under the current store state, so \
             a render of it could not be named — refusing to guess"
        )));
    };

    let Some(dir) = configured_dir() else {
        return RenderOutcome::Failed(refused(
            "no render-cache directory is configured for this process".to_string(),
        ));
    };

    let compositor = match background_compositor() {
        Ok(c) => c,
        Err(e) => return RenderOutcome::Failed(e),
    };

    // ---- (c) open the session ----
    //
    // The geometry handed over is the FULL project canvas. There is no other
    // value available at this call site, by construction — see the module doc's
    // first structural absence.
    let mut session = match rendercache::SegmentEncodeSession::begin(
        &dir, seg_index, hash, canvas_w, canvas_h, fps,
    ) {
        Ok(s) => s,
        Err(e) => return RenderOutcome::Failed(e),
    };

    // ---- (d) the frame loop ----
    let mut pool = engine::LayerDecoderPool::new(fps, SEG_POOL_PULL_TIMEOUT);
    let mut primed = false;
    let mut composite_us: u128 = 0;
    let mut t = seg_start;
    while t < seg_end {
        // The latch first: a caller that has decided to stop should not pay for
        // one more tick to find out.
        if cancel.load(Ordering::SeqCst) {
            session.cancel();
            return RenderOutcome::Cancelled;
        }
        // D-18's write half, re-asked EVERY tick rather than once at the end: an
        // edit that lands halfway through means every frame after it is being
        // composited from different material than the frames before it, so the
        // file would not be a render of ANY single arrangement.
        if current_edit_gen(host) != gen0 {
            session.cancel();
            return RenderOutcome::StaleAborted;
        }
        // A child that died mid-segment is noticed here rather than sixty frames
        // later at the commit.
        if let Ok(Some(status)) = session.try_wait() {
            session.cancel();
            return RenderOutcome::Failed(refused(format!(
                "the segment encoder exited early ({status}) at {t}us"
            )));
        }

        let rgba = match crate::resolve_multilayer(host, t) {
            Some(stack) => {
                // The canvas cannot move under a fixed generation (a settings
                // change is a dispatch, which bumps the counter checked above),
                // so a mismatch here means the two reads disagree and the
                // honest answer is to stop rather than to encode a frame the
                // meta would misdescribe.
                if (stack.width, stack.height) != (canvas_w, canvas_h)
                    || stack.fps.to_bits() != fps.to_bits()
                {
                    session.cancel();
                    return RenderOutcome::StaleAborted;
                }
                // Spawn every layer's child ONCE, before the first pull, so the
                // first tick is not the one that pays the whole cold start — and
                // so a cold start cannot make the first frames of a segment
                // short a layer. `pool_sources` is the `pub(crate)` helper this
                // module exists in this crate to reach (D-39), and it is also
                // where D-14 comes from for free: every source in it has been
                // through the decode-source resolver.
                //
                // Debug session
                // `render-cache-cannot-assemble-six-layers-at-a-seek`: the
                // second half of that sentence was never true, and the first
                // half is why. `prewarm` SPAWNS and returns; it does not wait
                // for a frame. So the tick immediately below IS the one that
                // pays the whole cold start — spawn, seek, decode-to-position,
                // first frame — and it was being asked to do it inside the
                // STREAMING bound. On six 4K sources at a seek that cost
                // 2 655–3 159 ms against 1 000 ms, and the two layers `advance`
                // pulls FIRST (which inherit no slack from earlier layers) were
                // dropped from the composite every single time. The stack was
                // then declared permanently uncacheable — see
                // [`SEG_POOL_FIRST_TICK_TIMEOUT`] for the measurement.
                //
                // The bound is raised for exactly this tick and lowered again
                // the moment it returns. That is deliberately NOT a retry: a
                // short composite is still terminal one line below, and a layer
                // that cannot be produced at all never reaches the pull (the
                // pool's own failure backoff skips it), so the uncacheable
                // arrangements this arm exists to catch still fail in
                // milliseconds.
                let priming = !primed;
                if priming {
                    pool.set_pull_timeout(SEG_POOL_FIRST_TICK_TIMEOUT);
                    pool.prewarm(&crate::multilayer::pool_sources(&stack));
                    primed = true;
                }
                let layers = crate::compose_multilayer_from_pool(host, &mut pool, &stack);
                if priming {
                    // Every later tick pulls a frame the reader thread has
                    // already buffered, so it is back under the streaming bound.
                    pool.set_pull_timeout(SEG_POOL_PULL_TIMEOUT);
                }
                // A short composite is a composite MISSING A LAYER. Live
                // playback tolerates that for a tick (black shows through and
                // the next tick is fine); a cache file would make one warming
                // decoder's bad luck permanent for as long as the segment is
                // served.
                if layers.len() != stack.layers.len() {
                    let (got, want) = (layers.len(), stack.layers.len());
                    session.cancel();
                    return RenderOutcome::Incomplete {
                        at_us: t,
                        got,
                        want,
                    };
                }
                let at = Instant::now();
                let out = compositor.composite_layers_to_rgba(&layers, canvas_w, canvas_h);
                composite_us += at.elapsed().as_micros();
                match out {
                    Ok(rgba) => rgba,
                    Err(e) => {
                        session.cancel();
                        return RenderOutcome::Failed(e);
                    }
                }
            }
            None => {
                match has_active_layers(host, t) {
                    None => {
                        session.cancel();
                        return RenderOutcome::Failed(refused(format!(
                            "the project store became unreadable at {t}us"
                        )));
                    }
                    // Something IS there, and the resolver declined it — the
                    // degenerate single-layer case, which the live path serves
                    // without compositing at all. There is no canvas-space
                    // render of that tick to commit.
                    Some(true) => {
                        session.cancel();
                        return RenderOutcome::NotCacheable { at_us: t };
                    }
                    // A genuine gap. The live path shows black here, and the
                    // canvas-black frame is produced by the SAME composite call
                    // every other tick uses (an empty layer slice clears to
                    // opaque black), so it cannot drift from it.
                    Some(false) => {
                        let at = Instant::now();
                        let out = compositor.composite_layers_to_rgba(&[], canvas_w, canvas_h);
                        composite_us += at.elapsed().as_micros();
                        match out {
                            Ok(rgba) => rgba,
                            Err(e) => {
                                session.cancel();
                                return RenderOutcome::Failed(e);
                            }
                        }
                    }
                }
            }
        };

        if let Err(e) = session.push_frame(&rgba) {
            session.cancel();
            return RenderOutcome::Failed(e);
        }
        // Published per FRAME, not per segment: this is the only signal that a
        // render is in flight at all (see the static's own doc).
        RENDER_CACHE_FRAMES_RENDERED.fetch_add(1, Ordering::Relaxed);
        t += step;
    }

    // ---- (e) commit, but only if the world still agrees ----
    //
    // The last frame was pushed a moment ago and the commit is about to name
    // this file after `hash`. Asking once more here closes the window between
    // the final tick's check and the rename.
    if cancel.load(Ordering::SeqCst) {
        session.cancel();
        return RenderOutcome::Cancelled;
    }
    if current_edit_gen(host) != gen0 {
        session.cancel();
        return RenderOutcome::StaleAborted;
    }
    let frames = session.frames_pushed();
    match session.finish() {
        Ok(commit) => {
            // D-32: the split is PRINTED, because "the background render is
            // cheap" and "the background render is ruinous" are both things
            // people assume, and 59-10 has to report one of them.
            eprintln!(
                "RENDER-CACHE-SEGMENT seg_index={seg_index} frames={} \
                 canvas={canvas_w}x{canvas_h}@{fps} composite_ms={} wall_ms={} \
                 total_ms={} bytes={}",
                commit.frames,
                composite_us / 1_000,
                commit.wall_ms,
                started.elapsed().as_millis(),
                commit.payload_bytes,
            );
            RenderOutcome::Committed {
                payload_bytes: commit.payload_bytes,
                wall_ms: commit.wall_ms,
                frames: commit.frames,
            }
        }
        Err(e) => {
            eprintln!(
                "RENDER-CACHE-SEGMENT seg_index={seg_index} frames={frames} \
                 outcome=failed err={e}"
            );
            RenderOutcome::Failed(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Phase 71 (TRUST-01).** Releasing the background compositor really
    /// empties the slot: the count it reports includes the caller's own clone,
    /// and the next request builds a NEW compositor rather than handing the old
    /// (possibly removed) device back. Needs a GPU adapter; without one the
    /// build fails and there is nothing to release, which is reported, not
    /// faked.
    #[test]
    fn releasing_the_background_compositor_empties_the_slot_and_the_next_build_is_fresh() {
        let held = match background_compositor() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no GPU adapter for the background compositor ({e}); nothing to release");
                return;
            }
        };
        let count = release_background_compositor()
            .expect("the slot held the compositor this test just built");
        assert!(
            count >= 2,
            "the count must include this test's own clone as well as the slot's ({count})"
        );
        let rebuilt = background_compositor().expect("a rebuild after release");
        assert!(
            !Arc::ptr_eq(&held, &rebuilt),
            "after a release the next request must build a NEW compositor"
        );
    }

    /// The outcome type must keep distinguishing the arms 59-08 branches on.
    /// A wildcard here — or a collapse of two arms into one — would silently
    /// turn "this arrangement can never be cached" into "try again in a second",
    /// which is a background worker spinning forever on a segment it cannot make.
    #[test]
    fn every_outcome_but_committed_means_nothing_was_written() {
        let outcomes = [
            RenderOutcome::Committed {
                payload_bytes: 1,
                wall_ms: 2,
                frames: 3,
            },
            RenderOutcome::Cancelled,
            RenderOutcome::StaleAborted,
            RenderOutcome::NotCacheable { at_us: 0 },
            RenderOutcome::Incomplete {
                at_us: 0,
                got: 1,
                want: 2,
            },
            RenderOutcome::Failed(refused("probe".to_string())),
        ];
        let committed: Vec<bool> = outcomes.iter().map(|o| o.is_committed()).collect();
        assert_eq!(
            committed,
            [true, false, false, false, false, false],
            "exactly one arm may mean a file exists"
        );
    }

    /// The frames this writer produces and the frame indices the SERVE path
    /// demands are the same grid, walked from the same anchor.
    ///
    /// Three properties, and the third is an honest limit rather than a claim:
    ///
    /// 1. the writer holds exactly the frames whose program time falls INSIDE
    ///    `[seg_start, seg_end)` — the same count 59-06's fixtures commit;
    /// 2. a producer walking the segment's own tick grid demands index `i` for
    ///    the frame written at index `i`, exactly, at every cadence;
    /// 3. a producer whose tick grid is OFFSET from the segment grid (playback
    ///    started somewhere else) can, at cadences whose step does not divide
    ///    `SEG_US`, round the final sub-frame sliver UP to one index past the
    ///    last written frame. That tick composites live (D-21) — it is never a
    ///    wrong frame — and the bound is exactly one, never more.
    #[test]
    fn the_written_grid_is_the_grid_the_serve_demands() {
        // The serve's own mapping, copied as the arithmetic it is: NEAREST, not
        // floor (`render_cache_lookup::serve_inner`).
        fn demand(prod_pos: i64, seg_start: i64, step: i64) -> i64 {
            (prod_pos - seg_start + step / 2).div_euclid(step)
        }

        for fps in [24.0_f64, 25.0, 30.0, 59.94, 60.0] {
            let step = engine::frame_step_us(fps).max(1);
            let (seg_start, seg_end) = rendercache::segment_bounds(3);

            let mut written = 0i64;
            let mut t = seg_start;
            while t < seg_end {
                written += 1;
                t += step;
            }

            // (1) the same count 59-06's warm helper commits.
            assert_eq!(
                written,
                (rendercache::SEG_US - 1).div_euclid(step) + 1,
                "at {fps} fps the writer's tick walk and the fixtures' frame \
                 count disagree about how many frames a segment holds"
            );

            // (2) on the segment's own grid the mapping is exact.
            for i in 0..written {
                assert_eq!(
                    demand(seg_start + i * step, seg_start, step),
                    i,
                    "at {fps} fps a tick on the segment grid demanded the wrong \
                     frame index"
                );
            }

            // (3) the offset-grid overshoot is bounded at exactly one.
            let highest = demand(seg_end - 1, seg_start, step);
            assert!(
                highest <= written,
                "at {fps} fps a tick inside the segment can demand index \
                 {highest}, which is more than one past the last written frame \
                 ({})",
                written - 1
            );
        }
    }
}
