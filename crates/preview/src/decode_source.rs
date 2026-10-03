//! The `clip → decode-source` resolver seam (Phase 57, CONTEXT **D-05**;
//! extended by Phase 58, CONTEXT **D-16**).
//!
//! **ONE place** maps `(clip identity, media path, source position)` → what a
//! decode session should actually open. Phase 57 shipped it with exactly one
//! answer — the clip's original media — because it is an *interface*
//! deliverable, not an optimization. Phase 58 gave it its second answer, a
//! generated playback proxy, **without moving its signature by one byte and
//! without teaching any other file in this crate that proxies exist**.
//!
//! ## Why it exists in Phase 57, which has only one source kind to return
//!
//! D-05 states it plainly: "The `clip → decode-source` resolver seam is
//! DEFINED IN THIS PHASE. […] Today the source is always the original media;
//! **Phase 58 adds proxies behind this same seam** without touching the decode
//! or composite code. Defining it here is precisely what makes 57 ∥ 58
//! parallel-eligible, so it is a deliverable of this phase even though this
//! phase has only one source kind to return."
//!
//! The consumer contract is `REQUIREMENTS.md` **PROXY-03**: "The timeline
//! transparently relinks clip↔proxy for preview through **one** clip →
//! decode-source resolver seam". *One* seam is the requirement — not one per
//! decode path. So the rule for every caller is:
//!
//! > **Decode and composite code consume [`DecodeSource`], never
//! > `LayerSpec.path` / `Resolved.path` directly.** A proxy swap is then
//! > invisible to them, and Phase 58 changes this file plus a lookup table —
//! > not the ring, not the compositor, not the coordinator.
//!
//! ## What Phase 58 ADDED, and where (D-16/D-17/D-18/D-19, PROXY-03)
//!
//! [`DecodeSourceKind::Proxy`] lives **here**, and nowhere else — and so does
//! every line in this crate that knows a proxy cache exists at all. This file
//! makes exactly ONE proxy-aware call, `proxy::cache::read_fresh`, and it is
//! the only file under `crates/preview/src` that names the `proxy` crate. That
//! is D-16, and it is grep-checkable rather than merely intended.
//!
//! ### Unset means Phase 57 — the compatibility floor
//!
//! The cache directory is process-global and starts **UNSET**. An unconfigured
//! process resolves [`DecodeSourceKind::Original`] for every input, so Phase
//! 57's behaviour is not just preserved, it is the default: every existing test
//! binary, and any host that never calls [`configure_proxy_cache_dir`], sees
//! byte-identical behaviour to the day this seam shipped. The host (`app-core`,
//! plan 58-05) configures it to `AppCtx::app_cache_dir()` joined with
//! `proxy::cache::PROXY_CACHE_DIR_NAME`.
//!
//! ### The fallback is TOTAL, SILENT and PER-CLIP (D-17)
//!
//! | The proxy on disk is… | resolve answers |
//! |---|---|
//! | absent | `Original` |
//! | stale (the source was touched, resized or replaced) | `Original` |
//! | partial / truncated (an encode killed mid-flight) | `Original` |
//! | corrupt (wrong magic, wrong version, garbage JSON) | `Original` |
//! | wrong geometry (odd or absurd dimensions) | `Original` |
//! | written under a different policy or a different encoder | `Original` |
//! | fresh, complete, and this build's | `Proxy { .. }` |
//!
//! No `Result`, no third state, nothing logged on the resolve path — and the
//! decision is made **per resolve**, so one unusable proxy can never degrade a
//! different clip. Playback failing because a *cache* is bad would be strictly
//! worse than playing the original slowly, which is the whole argument.
//!
//! ### Where the staleness check lives, and what it costs (D-18)
//!
//! Inside `proxy::cache::read_fresh`: one `canonicalize`, two `metadata` calls,
//! one length-bounded read of a few-hundred-byte JSON meta, and — on a HIT, at
//! most once per `proxy::cache::LRU_TOUCH_INTERVAL` per entry — one mtime write
//! recording the use for the byte budget. It opens no video and starts no
//! subprocess; that crate pins both at the source level, precisely because this
//! runs on the resolve path.
//!
//! D-18 asks for the cost to be **measured**, not assumed in either direction,
//! so [`PROXY_RESOLVE_NANOS`] accumulates the real wall time spent inside
//! [`resolve_decode_source`], and [`PROXY_RESOLVES`] + [`ORIGINAL_RESOLVES`]
//! count the calls by the answer they gave.
//!
//! **What that measurement found — stated here because this paragraph used to
//! claim the opposite.** Research RQ3 enumerated the seam's call sites and
//! concluded they were all session-open granularity ("a handful of times per
//! playback session, never per frame"), and this file therefore has **no
//! memoization**. The enumeration was wrong for one shape, and the counter is
//! what revealed it, exactly as intended:
//!
//! * `ring.rs`'s multi-layer producer calls `pool_sources` **inside its per-tick
//!   loop**, and `pool_sources` resolves each layer — so the pool arm resolves
//!   once per layer per composited tick, plus again from the parked lookahead
//!   prewarm. MEASURED at ~2.59 resolves per composite and 4–6% of the
//!   producer thread's wall time (`deferred-items.md` § D-15).
//! * That is arm-INDEPENDENT: the hardware coordinator filters `hw_served`
//!   layers on the line AFTER the resolve, so it pays the same cost
//!   (`deferred-items.md` § D-16, 12 045 resolves on a hardware run with no
//!   proxy cache configured at all).
//! * The single-clip CPU arm, the single-clip GPU-delegated arm and the
//!   coordinator's own session-open path are unaffected — RQ3 holds for those
//!   three, and `the_pool_arm_resolve_granularity_is_measured_and_bounded` pins
//!   the difference.
//!
//! **The absence of memoization is still deliberate, and must stay.** 58-07
//! proved by mutation that a `(source_path -> hit)` memo inside this function
//! makes `deleting_the_cache_mid_project_costs_only_speed` (V-15) fail: the
//! reopen after the cache directory is deleted keeps answering `Proxy`. D-14's
//! "deleting the whole directory is always safe" promise lives precisely in
//! this path not being memoized. The correct fix is D-15's preferred one —
//! **hoist** the resolve to where the active clip set changes, in `ring.rs` /
//! `multilayer.rs`, which needs no invalidation at all — and it is recorded
//! there, not smuggled in here. What 58-REVIEW WR-03 *did* remove is the other
//! half: the per-hit filesystem WRITE (see `LRU_TOUCH_INTERVAL`), worth about a
//! quarter of the measured per-resolve cost.
//!
//! ### `source_us` is the IDENTITY for proxies too, deliberately (D-04)
//!
//! [`DecodeSource::source_us`] remains the remap point: a source encoded at a
//! different fps or timebase would answer a *different* number there, in the
//! RETURNED media's clock, and callers must therefore always seek with
//! `DecodeSource::source_us` rather than the position they passed in. Phase 58
//! nevertheless fixes a proxy's fps, timebase and duration to its source's
//! (D-04), which makes the mapping the identity for `Proxy` as well and deletes
//! an entire class of A/V-drift and stamp-desync bugs before it can exist. The
//! capability stays documented and **unused**, and
//! `a_fresh_proxy_is_returned_with_identity_source_us` pins with a literal that
//! it is unused.
//!
//! Export is explicitly NOT a consumer: PROXY-04 requires export to always
//! render from the originals, and D-13 keeps the export path (the FFmpeg CLI
//! sidecar) byte-unchanged. This seam serves the **preview** path only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Instant;

/// What a decode session should actually open for a clip at a position.
///
/// **TWO** variants as of Phase 58. The `#[non_exhaustive]`-like discipline is
/// enforced by the tripwire test below rather than by the attribute, because
/// in-crate matches SHOULD break loudly when a phase extends this — a silent
/// wildcard arm is how a proxy would end up half-wired.
///
/// That tripwire did its job: adding `Proxy { .. }` failed to COMPILE in the
/// test below FIRST, before anything ran, and Phase 58 rewrote it from a
/// one-variant assertion to a two-variant one — planned work, not a workaround
/// (D-16 names the rewrite as part of the phase). The rewrite is still
/// wildcard-free, so **the next phase to add a variant breaks here first too**,
/// and rewriting it again is that phase's planned work.
///
/// (The old test's NAME is deliberately not repeated anywhere in this file: it
/// is spelled `one_variant` and this phase's gate greps for exactly that, which
/// a well-meaning prose reference would defeat. The same footgun bit three
/// earlier plans in this phase, each time on a doc comment.)
#[derive(Debug, Clone, PartialEq)]
pub enum DecodeSourceKind {
    /// The clip's original media — the only variant Phase 57 could answer, and
    /// still the answer whenever a proxy is absent, stale, or in any way
    /// untrustworthy (D-17's total fallback).
    Original,
    /// A generated all-intra playback proxy from `proxy::cache`, fresh against
    /// its source as of this resolve.
    ///
    /// Carries the proxy's GEOMETRY because the cache meta had already been
    /// read and parsed to answer at all: handing it back costs nothing, and it
    /// makes "which media actually played, at what size" answerable by a test
    /// or a counter without a second `stat` and without re-deriving the policy
    /// (D-19 — PROXY-02 hides proxies from the *user*, which is exactly why
    /// they must not be hidden from *verification*).
    ///
    /// It deliberately does NOT carry the cache key: the key is an
    /// implementation detail of `proxy::cache`, and a consumer that could read
    /// it here would be a second place that knows how proxies are identified.
    Proxy {
        /// Pixel width of the proxy at [`DecodeSource::path`].
        proxy_w: u32,
        /// Pixel height of the proxy at [`DecodeSource::path`].
        proxy_h: u32,
    },
}

/// The resolved decode source: what to open, which kind it is, and the
/// position **in the returned media's clock**.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeSource {
    /// The media file a decode session opens: the clip's own media, or — since
    /// Phase 58 — a proxy payload inside PROXY-05's cache directory.
    pub path: PathBuf,
    /// Which kind of source [`Self::path`] is. Observable so a caller (or a
    /// PLAY-05 / BENCH counter) can report *which* media actually played
    /// without re-deriving it — PROXY-02 makes proxies invisible to the user,
    /// which is exactly why they must not be invisible to verification.
    pub kind: DecodeSourceKind,
    /// Source-time position in the RETURNED media's clock. Identity for
    /// [`DecodeSourceKind::Original`] — and, by D-04's construction, identity
    /// for [`DecodeSourceKind::Proxy`] as well, because a proxy keeps its
    /// source's fps, timebase and duration and changes only pixels and GOP. The
    /// remap capability is real and stays available for a future codec that
    /// needs it; Phase 58 deliberately does not use it.
    pub source_us: i64,
}

impl DecodeSource {
    /// `true` when this resolved to the clip's own media — the only outcome in
    /// Phase 57, and still the outcome whenever a proxy is missing or
    /// untrustworthy.
    ///
    /// **D-19 keeps this as THE verification handle.** PROXY-02 makes proxies
    /// invisible to the user; this is what keeps them fully visible to tests
    /// and counters. Phase 58's parity tests read it to assert that export —
    /// and any full-quality re-render — never consumed a proxy (PROXY-04), and
    /// its fallback matrix reads it to assert that a bad proxy answered
    /// `Original` rather than failing.
    pub fn is_original(&self) -> bool {
        matches!(self.kind, DecodeSourceKind::Original)
    }
}

/// The proxy cache directory this process resolves against, or `None` — which
/// is the STARTING state and means "answer [`DecodeSourceKind::Original`] for
/// everything", i.e. exactly Phase 57.
///
/// ## Why an `RwLock` and not a `OnceLock`
///
/// Two independent reasons, both real:
///
/// 1. **The host sets it idempotently, over and over.** Plan 58-05 calls
///    [`configure_proxy_cache_dir`] on transport commands rather than finding a
///    single startup hook to own it, because the app-context that knows the
///    cache root is not available at process start. A `OnceLock` would make
///    every call after the first silently do nothing — which is fine right up
///    until the value legitimately needs to change.
/// 2. **Integration tests reconfigure per fixture.** Each one wants its own
///    temporary cache directory, in one process, and a write-once cell would
///    make the first test to run decide the answer for all the others.
///
/// The cost of the choice is one uncontended read-lock acquisition per resolve.
/// That is charged to [`PROXY_RESOLVE_NANOS`] along with everything else the
/// resolve does, so it is measured rather than argued about (D-18).
static PROXY_CACHE_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Point this process's resolves at a proxy cache directory. Idempotent, and a
/// later call simply overwrites — see the private static this writes to for why
/// overwriting is the REQUIRED behaviour here rather than a concession, and why
/// it is an `RwLock` rather than a write-once cell.
///
/// The directory need not exist and need not contain anything: a configured but
/// empty directory answers `Original` for every clip, exactly like an
/// unconfigured process, just at the cost of one extra `stat`. Nothing here
/// creates, validates or sweeps the directory — the cache owns its own disk.
///
/// The host passes `AppCtx::app_cache_dir()` joined with
/// `proxy::cache::PROXY_CACHE_DIR_NAME` (plan 58-05); nothing in this crate
/// derives a path of its own, which is the direct answer to 58-02's
/// `destructive-sweep` note.
pub fn configure_proxy_cache_dir(dir: PathBuf) {
    let mut slot = match PROXY_CACHE_DIR.write() {
        Ok(slot) => slot,
        // A poisoned lock means some thread panicked while holding this lock.
        // The stored value is still perfectly intact (this module only ever
        // assigns a whole `PathBuf`), so recover rather than panic — a
        // configuration setter that could kill the caller would be a worse
        // failure than any proxy problem it exists to enable.
        Err(poisoned) => poisoned.into_inner(),
    };
    // 59-REVIEW WR-02: pointing this seam somewhere ELSE changes what it
    // answers, with no domain mutation to announce it. Bumped only on a real
    // CHANGE, deliberately — the host re-asserts the same directory on every
    // transport command (58-04), and bumping there would invalidate every
    // consumer's memo on every Play, Pause, Seek and Step.
    let changed = slot.as_deref() != Some(dir.as_path());
    *slot = Some(dir);
    drop(slot);
    if changed {
        note_decode_answers_changed();
    }
}

/// **59-REVIEW WR-02's invalidation axis.** A monotonic counter that moves
/// whenever something OUTSIDE the domain store can have changed what
/// [`resolve_decode_source`] answers.
///
/// The store's own `seq` is bumped only by `dispatch`/`undo`/`redo`, so any
/// consumer that memoizes a resolve keyed on the edit generation alone is
/// keeping a pre-change answer for as long as no edit happens. That is a real
/// consequence rather than a theoretical one: a proxy generation completing
/// dispatches nothing at all, and the proxy landing is precisely the event that
/// changes this seam's answer for every clip over that media.
///
/// Read as an OPAQUE token, never as a count. It is bumped by
/// [`note_decode_answers_changed`], and a spurious bump costs one recomputed
/// answer while a missed one costs a stale answer, so callers should err
/// towards bumping.
static DECODE_ANSWER_GEN: AtomicU64 = AtomicU64::new(0);

/// The current value of the filesystem-side invalidation axis — see
/// [`note_decode_answers_changed`].
///
/// A memo whose key includes this value is invalidated by anything that moves
/// it, without that memo having to know what a proxy is.
pub fn decode_answer_generation() -> u64 {
    DECODE_ANSWER_GEN.load(Ordering::Relaxed)
}

/// Announce that the answers this seam gives may have changed WITHOUT any
/// domain mutation — i.e. a proxy landed, was evicted, or the cache directory's
/// contents moved under the process.
///
/// Best-effort, infallible and cheap (one relaxed `fetch_add`). It caches
/// nothing and invalidates nothing here: this seam is already stateless and
/// re-reads the cache on every call. What it does is give the CONSUMERS that
/// memoize this seam's answer — the program-level render-cache reader is the
/// one that exists today — a signal they can key on.
///
/// The counter cannot see a media file replaced by another application, which is
/// the one filesystem change nothing in this process observes. That case is
/// still bounded (a memo built on this axis is rebuilt at the next edit or the
/// next producer), and it is recorded here rather than left to be rediscovered.
pub fn note_decode_answers_changed() {
    DECODE_ANSWER_GEN.fetch_add(1, Ordering::Relaxed);
}

/// The dimensions a PROXY substitution would decode at for a source of
/// `src_w x src_h`, or `None` when this seam would never proxy that source at
/// all (it is already at or below the target long edge).
///
/// # Why this forwarder exists here rather than at its caller
///
/// It is a pure restatement of `proxy::cache::proxy_dims`, D-05's one geometry
/// policy — no cache read, no disk, no `read_fresh`, no state. It sits in THIS
/// file because this is the only file in the crate that may name the `proxy`
/// crate (D-16, and the manifest's own rule), and because "what shape would the
/// substitute be?" is a fact about the resolver seam, not about its caller.
///
/// # Who needs it, and why a dims-only answer is the right one
///
/// [`crate::occlusion`]'s canvas-coverage rung. That predicate decides in the
/// GATHER — before any decode — whether a layer paints every canvas pixel, and
/// it computes that from the mirror's recorded source dims. A proxy substitution
/// can change the answer: `proxy_dims` rounds and snaps to even, so the
/// substitute is not always the source's exact aspect (measured: a 2704x1520
/// GoPro source proxies to 960x540, which contain-fits into a 2704x1520 canvas
/// with a ~1 px letterbox — and a 1 px band of the layer beneath showing through
/// is exactly the pixel difference OCCL-01 forbids).
///
/// The predicate deliberately asks the DIMS question rather than
/// [`resolve_decode_source`]'s "is there a fresh proxy for this clip right now?"
/// question. It must be conservative against a proxy that lands at ANY later
/// tick, not just against the one on disk at gather time — and unlike a resolve,
/// this costs no `stat`, which matters on a per-layer per-tick path.
pub fn proxy_substitute_dims(src_w: u32, src_h: u32) -> Option<(u32, u32)> {
    proxy::cache::proxy_dims(src_w, src_h)
}

/// Resolves that answered [`DecodeSourceKind::Proxy`] — i.e. a fresh, complete
/// proxy was found and handed to a decode session.
///
/// Purely observational: one relaxed `fetch_add`, no behaviour, following the
/// precedent of `engine::audio::AUDIO_STREAM_OPENS` and
/// `engine::hwdecode::HW_OPEN_COUNT`. Read as a **delta** around a span
/// (snapshot, act, snapshot) — the absolute value carries every resolve since
/// process start, including other tests sharing the binary.
///
/// Consumers: **V-08** (proxy attribution — proving the proxy actually engaged
/// on the single-clip path, which a green pixel test alone cannot show) and
/// **V-19** (D-18's resolve-cost measurement).
pub static PROXY_RESOLVES: AtomicU64 = AtomicU64::new(0);

/// Resolves that answered [`DecodeSourceKind::Original`], for any of D-17's
/// reasons or because no cache directory is configured.
///
/// The counterpart to [`PROXY_RESOLVES`], and the half that makes an
/// attribution claim non-vacuous: "the proxy was used" means nothing without
/// "and the original was not". Same relaxed, delta-read discipline. Consumers:
/// **V-08**, **V-19**.
pub static ORIGINAL_RESOLVES: AtomicU64 = AtomicU64::new(0);

/// Cumulative wall-clock nanoseconds spent INSIDE [`resolve_decode_source`],
/// across every resolve in this process.
///
/// This is **V-19's instrument**, and it exists because D-18 says the
/// resolve-path staleness check must be *measured* rather than assumed cheap or
/// assumed expensive. It covers the whole call: the lock read, the cache's
/// `canonicalize` + two `metadata` calls + bounded meta read, and the JSON
/// parse. Divide by `PROXY_RESOLVES + ORIGINAL_RESOLVES` for a mean.
///
/// Same relaxed, delta-read discipline as the two counters above. It is a
/// diagnostic, never a synchronisation edge, and nothing branches on it.
pub static PROXY_RESOLVE_NANOS: AtomicU64 = AtomicU64::new(0);

/// **THE seam (D-05).** One place maps (clip identity, media path, source
/// position) → the decode source.
///
/// Today: identity — `path` through unchanged, [`DecodeSourceKind::Original`],
/// `source_us` through unchanged. Phase 58: proxy lookup with staleness
/// fallback to `Original`, **behind this signature, verbatim**.
///
/// ## Why `clip_id` is a parameter it STILL does not read
///
/// Deliberate, and the reason has sharpened rather than expired. Phase 57
/// expected Phase 58 to key proxy lookup on clip identity; it does not. Proxies
/// are keyed on the **MEDIA FILE** (`proxy::cache::key_for` — canonical path +
/// mtime + size), because that is what makes a trim, a split, a duplicate and a
/// move cost zero new encoding: two clips over one media share one proxy, and
/// every edit is just a different window into it. Keying on the clip would have
/// meant re-encoding on every split.
///
/// So the parameter remains **reserved**, and reserving it is still the right
/// trade: adding it later would change the signature and therefore touch every
/// call site, which is precisely the churn this seam exists to prevent. Phase
/// 58 extended the body without moving one byte of the signature — the property
/// D-16 asks for — and this unused parameter is part of why that was possible.
///
/// Infallible by design: there is no failure mode a caller could act on that
/// is not better expressed as "fall back to the original", and that fallback
/// is this function's own responsibility, not its callers'.
pub fn resolve_decode_source(clip_id: &str, media_path: &Path, source_us: i64) -> DecodeSource {
    let started = Instant::now();

    let guard = match PROXY_CACHE_DIR.read() {
        Ok(slot) => slot,
        // Poisoned only if a thread panicked holding this lock. The stored
        // value is intact, so read it rather than permanently disabling proxies
        // (or panicking) for the rest of the process.
        Err(poisoned) => poisoned.into_inner(),
    };
    let resolved = resolve_decode_source_in(guard.as_deref(), clip_id, media_path, source_us);
    drop(guard);

    // Measured, not assumed (D-18). The elapsed span covers the lock read AND
    // the cache lookup, because both are what a call site actually pays.
    let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    PROXY_RESOLVE_NANOS.fetch_add(nanos, Ordering::Relaxed);
    if resolved.is_original() {
        ORIGINAL_RESOLVES.fetch_add(1, Ordering::Relaxed);
    } else {
        PROXY_RESOLVES.fetch_add(1, Ordering::Relaxed);
    }

    resolved
}

/// The seam's actual decision, with the process-global configuration passed in
/// as an argument instead of read from a static.
///
/// This split exists for the tests: the fallback matrix needs one temporary
/// cache directory **per case**, and cases run concurrently in one binary. A
/// unit test that had to mutate a process-global to set up would be a test that
/// cannot run beside its siblings — the same hazard 58-01 documented at length
/// for `DEV_ENCODER_OVERRIDE_ENV`, avoided here by construction rather than
/// managed. Everything about the answer is decided here; the public wrapper
/// only reads the configuration and counts.
///
/// `dir` of `None` is the unconfigured process: `Original`, always, for any
/// input — the property that keeps every pre-Phase-58 test byte-unchanged.
pub(crate) fn resolve_decode_source_in(
    dir: Option<&Path>,
    clip_id: &str,
    media_path: &Path,
    source_us: i64,
) -> DecodeSource {
    // Reserved, still unread — see `resolve_decode_source`'s doc for why the
    // proxy is keyed on the MEDIA FILE and not on the clip.
    let _ = clip_id;

    let original = || DecodeSource {
        path: media_path.to_path_buf(),
        kind: DecodeSourceKind::Original,
        source_us,
    };

    let Some(dir) = dir else {
        return original();
    };

    // THE one proxy-aware call in the whole preview crate (D-16). Every one of
    // D-17's failure modes — absent, stale, truncated, corrupt, wrong-geometry,
    // wrong-policy — arrives here as the same `None`, and must: the caller
    // cannot distinguish them and must not try.
    match proxy::cache::read_fresh(media_path, dir) {
        Some(hit) => DecodeSource {
            path: hit.path,
            kind: DecodeSourceKind::Proxy {
                proxy_w: hit.width,
                proxy_h: hit.height,
            },
            // IDENTITY, deliberately (D-04): a proxy carries its source's fps,
            // timebase and duration, so the requested position is already in
            // the returned media's clock. Passing it through unchanged is the
            // whole remap, and a test pins the literal so a future edit that
            // "helpfully" scales it fails loudly.
            source_us,
        },
        None => original(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    use tempfile::TempDir;

    // ---------------------------------------------------------------------
    // The three Phase-57 tests, textually unmodified (comments excepted).
    //
    // They call the PUBLIC seam, which consults the process-global cache
    // directory — and they still assert `Original`, for two independent
    // reasons, either of which alone is sufficient:
    //
    //   1. The directory is UNCONFIGURED for this binary. Only the two tests
    //      at the bottom of this module ever configure it, both hold
    //      `CONFIG_LOCK` while they do, and both point it back at a
    //      nonexistent directory before releasing it.
    //   2. Their media paths do not exist on disk, so `proxy::cache::key_for`
    //      answers `None` before any cache directory is consulted at all.
    //
    // Reason 2 is why these three are immune to test ordering even though
    // reason 1 is the property the phase actually cares about.
    // ---------------------------------------------------------------------

    #[test]
    fn identity_mapping_returns_the_original_media() {
        let p = Path::new(r"C:\media\shot_01.mp4");
        let got = resolve_decode_source("clip-a", p, 1_234_567);
        assert_eq!(got.path, PathBuf::from(r"C:\media\shot_01.mp4"));
        assert_eq!(got.source_us, 1_234_567);
        assert_eq!(got.kind, DecodeSourceKind::Original);
        assert!(got.is_original());
    }

    #[test]
    fn clip_id_does_not_change_the_answer_today() {
        // COMMENT UPDATED BY PHASE 58 (the body is untouched). Phase 57 wrote
        // that the parameter "exists for Phase 58 (PROXY-05 keys the lookup by
        // clip/media identity)". Phase 58 landed and it is keyed on the MEDIA
        // FILE alone, so `clip_id` is STILL unread — and this assertion stopped
        // being a placeholder and became load-bearing: two clips over one media
        // deliberately SHARE one proxy, which is what makes a trim, a split and
        // a duplicate cost zero new encoding. Anything else would mean the seam
        // carries hidden state.
        let p = Path::new("media/a.mp4");
        assert_eq!(
            resolve_decode_source("clip-a", p, 0),
            resolve_decode_source("clip-b", p, 0)
        );
    }

    #[test]
    fn zero_and_negative_positions_pass_through_unclamped() {
        // The seam maps; it does not sanitize. Clamping belongs to the decode
        // session (which already clamps a still to 0), and a seam that
        // silently rewrote a position would desync stamps from pixels.
        let p = Path::new("media/a.mp4");
        assert_eq!(resolve_decode_source("c", p, 0).source_us, 0);
        assert_eq!(resolve_decode_source("c", p, -1).source_us, -1);
    }

    // =====================================================================
    // Phase 58 — V-06 (the fallback matrix), V-07 (the identity pin), D-17's
    // per-clip isolation, and D-19's counters.
    //
    // Every fixture below is REAL FILES ON DISK, fabricated through
    // `proxy::cache`'s own PUBLIC api — no test-only backdoor into the cache,
    // and no mock. The payloads are plain byte files rather than encoded
    // video, which is itself part of what is under test: a resolve that
    // decoded anything would fail on these fixtures instantly.
    //
    // Each negative case first proves its fixture is a HIT, then breaks ONE
    // thing, then proves it is a MISS. Without that baseline a "falls back to
    // the original" assertion passes just as happily on a fixture that was
    // never a valid proxy in the first place.
    // =====================================================================

    /// The bytes of every fabricated payload. Not a video, deliberately.
    const PAYLOAD: &[u8] = b"a proxy payload the resolve path must never open";

    /// Serialises the tests that configure the PROCESS-GLOBAL cache directory,
    /// so one test's configuration is never observed by a sibling running
    /// beside it. It also means `PROXY_RESOLVES` can only be moved by whoever
    /// holds this lock, which is what makes the exact counter deltas below
    /// sound in a parallel test binary.
    static CONFIG_LOCK: Mutex<()> = Mutex::new(());

    /// Both halves of a committed cache entry, so a test can corrupt one.
    struct Planted {
        payload: PathBuf,
        meta: PathBuf,
    }

    /// A real file standing in for a media source. Its CONTENT is irrelevant —
    /// the cache keys on `(canonical path, mtime_ns, size_bytes)` and the
    /// resolve path never opens it.
    fn write_source(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"stand-in for heavy 4K media").expect("write the source fixture");
        path
    }

    /// Commit a genuinely fresh cache entry for `source` into `cache`, through
    /// `proxy::cache`'s own public write path — the same one the 58-03 job
    /// uses. If the cache ever refuses what this builds, the FIXTURE is wrong
    /// and the test says so rather than silently becoming a no-op.
    fn plant_fresh_proxy(cache: &Path, source: &Path, w: u32, h: u32) -> Planted {
        std::fs::create_dir_all(cache).expect("create the cache dir");
        let key = proxy::cache::key_for(source).expect("the source fixture is a real file");
        let stem = proxy::cache::file_stem_for(&key);
        let payload = cache.join(proxy::cache::payload_file_name(&stem));
        std::fs::write(&payload, PAYLOAD).expect("write the payload");
        let meta = proxy::cache::ProxyMeta::new(key, w, h, PAYLOAD.len() as u64);
        assert!(
            proxy::cache::write_meta(cache, &stem, &meta),
            "the cache refused a meta this fixture believes is valid — fix the fixture, not the seam"
        );
        Planted {
            payload,
            meta: cache.join(proxy::cache::meta_file_name(&stem)),
        }
    }

    /// Break exactly ONE field of a committed meta, asserting the edit really
    /// landed. Editing the file's TEXT rather than re-serialising a struct is
    /// deliberate: it corrupts real bytes on disk the way a truncated write or
    /// a foreign build would, and it needs no JSON dependency in this crate.
    fn break_one_meta_field(meta: &Path, from: &str, to: &str) {
        let text = std::fs::read_to_string(meta).expect("the committed meta is readable JSON text");
        assert!(
            text.contains(from),
            "fixture drift: {from:?} is not in the committed meta: {text}"
        );
        std::fs::write(meta, text.replace(from, to)).expect("rewrite the meta");
    }

    /// Return the process to "resolves Original for everything".
    ///
    /// [`configure_proxy_cache_dir`] deliberately has no unset — nothing in
    /// production ever wants one — so this points it at a relative path that
    /// cannot be stat-ed, which is behaviourally identical for every input.
    fn point_at_nothing() {
        configure_proxy_cache_dir(PathBuf::from("rudis-no-such-proxy-cache-6b41f0e2"));
    }

    #[test]
    fn a_fresh_proxy_is_returned_with_identity_source_us() {
        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "shot_01.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        let planted = plant_fresh_proxy(&cache, &src, 960, 540);

        let got = resolve_decode_source_in(Some(&cache), "clip-a", &src, 1_234_567);

        assert_eq!(
            got.kind,
            DecodeSourceKind::Proxy {
                proxy_w: 960,
                proxy_h: 540
            }
        );
        assert_eq!(got.path, planted.payload);
        assert!(!got.is_original(), "D-19's handle must report the proxy");

        // V-07 / D-04: the mapping is the IDENTITY for a proxy too, pinned with
        // a literal on both sides. A proxy keeps its source's fps, timebase and
        // duration, so a future edit that "helpfully" rescales the position by
        // the proxy's frame rate fails right here rather than as A/V drift.
        assert_eq!(got.source_us, 1_234_567);
        assert_eq!(
            resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).source_us,
            0
        );
        assert_eq!(
            resolve_decode_source_in(Some(&cache), "clip-a", &src, -1).source_us,
            -1
        );
    }

    #[test]
    fn a_missing_proxy_falls_back_to_the_original() {
        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "shot_01.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        std::fs::create_dir_all(&cache).expect("an empty but real cache dir");

        let got = resolve_decode_source_in(Some(&cache), "clip-a", &src, 42);

        assert_eq!(got.kind, DecodeSourceKind::Original);
        assert_eq!(got.path, src);
        assert_eq!(got.source_us, 42);
        assert!(got.is_original());
    }

    #[test]
    fn a_stale_proxy_falls_back_to_the_original() {
        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "shot_01.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        plant_fresh_proxy(&cache, &src, 960, 540);

        assert!(
            !resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original(),
            "baseline: this proxy IS a hit before the source is touched"
        );

        // Re-stamp the SOURCE. A fixed, distinctly-different timestamp rather
        // than `now()`: Windows file times advance in ~15.6 ms steps, so
        // "now" can compare equal to a file written microseconds ago.
        let handle = std::fs::File::options()
            .write(true)
            .open(&src)
            .expect("reopen the source");
        handle
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000_000))
            .expect("re-stamp the source");
        drop(handle);

        let got = resolve_decode_source_in(Some(&cache), "clip-a", &src, 0);
        assert_eq!(got.kind, DecodeSourceKind::Original);
        assert_eq!(got.path, src);
    }

    #[test]
    fn a_partial_proxy_falls_back_to_the_original() {
        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "shot_01.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        let planted = plant_fresh_proxy(&cache, &src, 960, 540);

        assert!(
            !resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original(),
            "baseline: this proxy IS a hit at its committed length"
        );

        // SHORT — the killed-mid-encode shape.
        std::fs::write(&planted.payload, &PAYLOAD[..PAYLOAD.len() / 2]).expect("truncate");
        assert!(resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original());

        // LONG — a different file that happened to land on this name. The
        // check is an equality, not a floor, and both directions matter.
        let mut longer = PAYLOAD.to_vec();
        longer.extend_from_slice(b"...and then some");
        std::fs::write(&planted.payload, &longer).expect("extend");
        assert!(resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original());
    }

    #[test]
    fn a_corrupt_proxy_falls_back_to_the_original() {
        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "shot_01.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        let planted = plant_fresh_proxy(&cache, &src, 960, 540);

        assert!(
            !resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original(),
            "baseline: this proxy IS a hit before its meta is corrupted"
        );

        // Garbage — not even UTF-8, let alone JSON.
        std::fs::write(&planted.meta, [0x00u8, 0xff, 0xfe, b'{', b'{', b'{']).expect("garbage");
        assert!(resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original());

        // Re-commit the SAME entry: the miss above was caused by the bytes and
        // nothing else, which is what makes this case non-vacuous.
        plant_fresh_proxy(&cache, &src, 960, 540);
        assert!(!resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original());

        // Well-formed JSON, wrong stamp — a foreign file wearing our name.
        break_one_meta_field(&planted.meta, "\"magic\":\"RPXM\"", "\"magic\":\"XXXX\"");
        assert!(resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original());
    }

    #[test]
    fn a_wrong_geometry_proxy_falls_back_to_the_original() {
        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "shot_01.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        let planted = plant_fresh_proxy(&cache, &src, 960, 540);

        assert!(
            !resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original(),
            "baseline: 960x540 IS a hit"
        );

        // An ODD width. No 4:2:0 encoder produces one, so a meta claiming it
        // is either corrupt or from a policy this build cannot honour — and a
        // decode session handed it would size its buffers wrong.
        break_one_meta_field(&planted.meta, "\"proxy_w\":960", "\"proxy_w\":961");

        let got = resolve_decode_source_in(Some(&cache), "clip-a", &src, 0);
        assert_eq!(got.kind, DecodeSourceKind::Original);
        assert_eq!(got.path, src);
    }

    #[test]
    fn one_bad_proxy_never_degrades_another_clip() {
        // D-17's per-clip half, and the reason the fallback is decided per
        // resolve rather than once per session: a single unreadable entry in a
        // shared cache directory must cost exactly one clip its speed-up.
        let tmp = TempDir::new().expect("tempdir");
        let media_a = write_source(tmp.path(), "broken.mp4");
        let media_b = write_source(tmp.path(), "healthy.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);

        let planted_a = plant_fresh_proxy(&cache, &media_a, 960, 540);
        let planted_b = plant_fresh_proxy(&cache, &media_b, 640, 960);

        assert!(
            !resolve_decode_source_in(Some(&cache), "clip-a", &media_a, 0).is_original(),
            "baseline: BOTH entries are hits before one is broken"
        );
        std::fs::write(&planted_a.meta, b"corrupt").expect("break A only");

        let a = resolve_decode_source_in(Some(&cache), "clip-a", &media_a, 7);
        let b = resolve_decode_source_in(Some(&cache), "clip-b", &media_b, 7);

        assert_eq!(a.kind, DecodeSourceKind::Original);
        assert_eq!(a.path, media_a);
        assert_eq!(
            b.kind,
            DecodeSourceKind::Proxy {
                proxy_w: 640,
                proxy_h: 960
            },
            "B is a portrait proxy and must be untouched by A's corruption"
        );
        assert_eq!(b.path, planted_b.payload);
        assert_eq!(b.source_us, 7);
    }

    #[test]
    fn an_unconfigured_process_resolves_original_for_everything() {
        // THE compatibility floor: this is why every pre-Phase-58 test process
        // is byte-unchanged. Even with a perfectly good proxy sitting on disk,
        // `None` answers Original — the cache is not consulted, not stat-ed,
        // not discovered.
        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "shot_01.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        plant_fresh_proxy(&cache, &src, 960, 540);

        assert!(
            !resolve_decode_source_in(Some(&cache), "clip-a", &src, 0).is_original(),
            "baseline: the same fixture IS a hit when a directory is supplied"
        );

        for (clip, path, pos) in [
            ("clip-a", src.as_path(), 1_234_567_i64),
            ("clip-b", src.as_path(), 0),
            ("clip-c", src.as_path(), -1),
            ("clip-d", Path::new(r"C:\media\does_not_exist.mp4"), 99),
        ] {
            let got = resolve_decode_source_in(None, clip, path, pos);
            assert_eq!(got.kind, DecodeSourceKind::Original);
            assert_eq!(got.path, path);
            assert_eq!(got.source_us, pos);
        }
    }

    #[test]
    fn configure_proxy_cache_dir_makes_the_public_seam_answer_proxy() {
        // The inner fn is where the decision lives, but the PUBLIC seam is what
        // every call site reaches. This is the only test of the plumbing
        // between them: the static, the read lock, and the delegation.
        let _lock = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "shot_01.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        let planted = plant_fresh_proxy(&cache, &src, 960, 540);

        assert!(
            resolve_decode_source("clip-a", &src, 0).is_original(),
            "unconfigured, the public seam answers Original even with a good proxy present"
        );

        configure_proxy_cache_dir(cache.clone());
        let got = resolve_decode_source("clip-a", &src, 1_234_567);
        assert_eq!(
            got.kind,
            DecodeSourceKind::Proxy {
                proxy_w: 960,
                proxy_h: 540
            }
        );
        assert_eq!(got.path, planted.payload);
        assert_eq!(got.source_us, 1_234_567);

        // Idempotent, and a later call overwrites — the property 58-05 relies
        // on when it configures this on every transport command.
        configure_proxy_cache_dir(cache.clone());
        assert!(!resolve_decode_source("clip-a", &src, 0).is_original());

        point_at_nothing();
        assert!(resolve_decode_source("clip-a", &src, 0).is_original());
    }

    #[test]
    fn proxy_and_original_resolves_are_counted_separately() {
        // D-19: proxies are invisible to the USER and must stay fully visible
        // to tests and counters. V-08 reads these to prove a proxy actually
        // engaged; V-19 reads the nanos to answer D-18's "measure it".
        let _lock = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = TempDir::new().expect("tempdir");
        let src = write_source(tmp.path(), "counted.mp4");
        let cache = tmp.path().join(proxy::cache::PROXY_CACHE_DIR_NAME);
        plant_fresh_proxy(&cache, &src, 960, 540);
        configure_proxy_cache_dir(cache.clone());

        // DELTAS, never absolutes: these statics are process-global and carry
        // every resolve since process start, this binary's other tests
        // included. `PROXY_RESOLVES` is nonetheless EXACT here, because a
        // Proxy answer requires a configured directory and every test that
        // configures one holds `CONFIG_LOCK` — which this test is holding.
        // `ORIGINAL_RESOLVES` gets no such guarantee (the three Phase-57 tests
        // bump it from other threads and cannot be locked without editing
        // them), so it is asserted as growth, not as an exact count.
        let p0 = PROXY_RESOLVES.load(Ordering::Relaxed);
        let o0 = ORIGINAL_RESOLVES.load(Ordering::Relaxed);
        let n0 = PROXY_RESOLVE_NANOS.load(Ordering::Relaxed);

        let hit = resolve_decode_source("clip-a", &src, 0);
        assert!(!hit.is_original(), "the fixture must resolve to a proxy");

        assert_eq!(
            PROXY_RESOLVES.load(Ordering::Relaxed) - p0,
            1,
            "one proxy resolve, counted exactly once"
        );
        assert!(
            PROXY_RESOLVE_NANOS.load(Ordering::Relaxed) > n0,
            "a resolve that stat-ed the disk three times cannot cost zero nanoseconds"
        );

        // Now the other side. A miss (no proxy for THIS media) must not touch
        // the proxy counter at all.
        let p1 = PROXY_RESOLVES.load(Ordering::Relaxed);
        let other = write_source(tmp.path(), "unproxied.mp4");
        let miss = resolve_decode_source("clip-b", &other, 0);
        assert!(miss.is_original());
        assert_eq!(
            PROXY_RESOLVES.load(Ordering::Relaxed) - p1,
            0,
            "an Original resolve must never be counted as a proxy"
        );
        assert!(
            ORIGINAL_RESOLVES.load(Ordering::Relaxed) > o0,
            "…and it must be counted as an original"
        );

        point_at_nothing();
    }

    /// TRIPWIRE (D-05 / PROXY-03 / D-16): `DecodeSourceKind` has exactly TWO
    /// variants as of Phase 58. This match is deliberately wildcard-free, and
    /// binds every field of every variant, so adding a THIRD kind — or a field
    /// to `Proxy` — fails to compile HERE first, forcing that phase to
    /// consciously extend the tests it changes rather than discovering an
    /// unhandled kind at runtime in the ring.
    ///
    /// It did exactly that job at this phase's boundary. Phase 57 shipped this
    /// as a ONE-variant assertion and said in as many words that Phase 58 would
    /// have to rewrite it; adding `Proxy { .. }` produced `error[E0004]:
    /// non-exhaustive patterns` right here, before a single test could run, and
    /// rustc obligingly offered a wildcard arm that would have hidden the whole
    /// problem. Declining that offer and rewriting the test is PLANNED WORK
    /// (58-CONTEXT D-16), not a workaround. The next phase to extend the enum
    /// rewrites it again, for the same reason — and must keep it wildcard-free.
    #[test]
    fn kind_has_exactly_two_variants_today() {
        let every_kind = [
            DecodeSourceKind::Original,
            DecodeSourceKind::Proxy {
                proxy_w: 960,
                proxy_h: 540,
            },
        ];

        let names: Vec<&str> = every_kind
            .into_iter()
            .map(|kind| match kind {
                DecodeSourceKind::Original => "Original",
                DecodeSourceKind::Proxy { proxy_w, proxy_h } => {
                    assert_eq!((proxy_w, proxy_h), (960, 540));
                    "Proxy"
                }
            })
            .collect();

        assert_eq!(names, ["Original", "Proxy"]);
    }

    /// D-16, checked rather than intended: the proxy LOGIC lives in this file
    /// and no other file under `crates/preview/src` may consult the cache.
    ///
    /// A source scan, because it is the only form of this claim that keeps
    /// holding after the next person edits the crate. It is the same technique
    /// `crates/proxy`'s own `the_read_path_starts_no_subprocess_and_decodes_nothing`
    /// uses, for the same reason.
    #[test]
    fn this_is_the_only_proxy_aware_file_in_the_preview_crate() {
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();
        let mut scanned = 0usize;

        for entry in std::fs::read_dir(&src_dir).expect("read the crate's src dir").flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("decode_source.rs") {
                continue;
            }
            scanned += 1;
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            // `read_fresh(` — with the paren — and not the bare prefix, since
            // Phase 59 (plan 59-05). That phase added a DIFFERENT cache at a
            // DIFFERENT altitude (it caches a composited PROGRAM RANGE, not one
            // clip's source media), whose own read entry point is spelled
            // `read_fresh_segment` — which the bare prefix matched. Nothing is
            // lost by the paren: `proxy::` is the COMPLETE needle on its own —
            // any file that consults the proxy cache must name it, in a `use`
            // or in a qualified call — and this second needle exists only as
            // belt-and-braces against a direct aliased call, which
            // `read_fresh(` still catches. It is in fact the more robust form:
            // rustfmt never breaks between an identifier and its open paren.
            if text.contains("proxy::") || text.contains("read_fresh(") {
                offenders.push(path.display().to_string());
            }
        }

        // Non-vacuity: the scanner must actually have looked at files, and it
        // must be capable of finding the pattern it is looking for.
        assert!(scanned > 1, "the scan found no sibling modules to check");
        let this_file = std::fs::read_to_string(src_dir.join("decode_source.rs")).expect("read");
        assert!(
            this_file.contains("read_fresh("),
            "the scanner's own pattern no longer matches the one file that SHOULD match"
        );

        assert!(
            offenders.is_empty(),
            "D-16: proxy awareness leaked out of decode_source.rs into {offenders:?}"
        );
    }

    /// **V-09 (D-27): every decode arm consumes THE seam, and none of them still
    /// opens from the clip's own path.**
    ///
    /// Phase 57 shipped this seam with exactly ONE production caller (the
    /// multi-layer hardware coordinator). Research RQ3 found the other three
    /// arms — the single-clip CPU arm, the single-clip GPU-delegated arm and the
    /// multi-layer software-fallback pool — still opening decode sessions from
    /// `Resolved.path` / `LayerSpec.path` directly, which meant a proxy could
    /// only ever engage during a multi-layer overlap. Plan 58-06 wired all four.
    ///
    /// This is a SOURCE SCAN, for the same reason
    /// `this_is_the_only_proxy_aware_file_in_the_preview_crate` is one: the
    /// `grep`s that checked this wiring at execution time proved it that day and
    /// never again. `include_str!` reads the sibling modules at COMPILE time, so
    /// a renamed or deleted file is a build error rather than a silent pass.
    ///
    /// The positive counts are exact MINIMUMS — adding a caller must never fail
    /// this test, removing one must always fail it. The negative asserts are the
    /// half that catches the subtler regression: wiring that is present but
    /// bypassed, and (in `delegate_gpu_clip`) latch bookkeeping that drifts back
    /// onto the ORIGINAL path while the open uses the resolved one — the
    /// desync T-58-06-03 exists to prevent.
    ///
    /// **Both sides are comment-blanked and whitespace-squashed**, and neither
    /// property is cosmetic:
    ///
    /// * blanking comments is `deferred-items.md § D-10`'s recorded fix, applied
    ///   here rather than deferred — the wired sites all EXPLAIN the rule in
    ///   prose right above the code, and a needle that matched prose would make
    ///   documenting the rule blunt the detector that enforces it. Same shape,
    ///   and the same reasoning, as `crates/proxy/tests/cache.rs::code_lines`.
    /// * squashing whitespace is what makes the negative half work at all. The
    ///   first version of this test asserted on the literal one-line call forms
    ///   the plan quoted, and a mutation run proved it VACUOUS: reverting the
    ///   prewarm to `r.path` still passed, because rustfmt had wrapped that call
    ///   across four lines and `"start_with_pts(&r.path"` no longer appeared
    ///   anywhere. A pin that a reformat can silently disarm is not a pin.
    #[test]
    fn seam_is_consumed_by_every_decode_arm() {
        /// Line comments blanked, then every whitespace character dropped, so
        /// the needles below match the CODE regardless of how rustfmt has
        /// wrapped it and regardless of what the prose above it says.
        fn code_only(source: &str) -> String {
            source
                .lines()
                .map(|raw| match raw.find("//") {
                    Some(at) => &raw[..at],
                    None => raw,
                })
                .flat_map(|code| code.chars())
                .filter(|c| !c.is_whitespace())
                .collect()
        }
        // The needles are written the way the code reads; they get the same
        // treatment as the haystack so both sides are compared like with like.
        let n = code_only;

        let ring = code_only(include_str!("ring.rs"));
        let multi = code_only(include_str!("multilayer.rs"));
        let sessions = code_only(include_str!("layer_sessions.rs"));
        let count = |s: &String| s.matches("resolve_decode_source(").count();

        // Non-vacuity: these three files must actually be the ones that carry
        // the arms, or every zero-assert below would pass on the wrong text.
        assert!(
            ring.contains(&n("fn delegate_gpu_clip")) && ring.contains("hw_failed"),
            "ring.rs is no longer the file that owns the single-clip decode arms"
        );
        assert!(
            multi.contains(&n("fn pool_sources")),
            "multilayer.rs is no longer the file that owns the pooled source list"
        );
        assert!(
            sessions.contains(&n("fn open_session")),
            "layer_sessions.rs is no longer the file that owns the hw coordinator"
        );
        // …and the blanking itself works: a needle that appears ONLY in prose in
        // these files must not be found in the code text.
        assert_eq!(
            ring.matches(&n("the source ALWAYS comes")).count(),
            0,
            "comment blanking is not actually blanking comments"
        );

        // ---- the four arms, by count ----
        // ring.rs: the CPU prewarm, the two CPU cold starts, and the top of
        // `delegate_gpu_clip`.
        assert!(
            count(&ring) >= 4,
            "ring.rs lost a seam call: {}",
            count(&ring)
        );
        // multilayer.rs: `pool_sources` — the software-fallback pool's list.
        assert!(count(&multi) >= 1, "pool_sources lost its seam call");
        // layer_sessions.rs: Phase 57's multi-layer hardware coordinator.
        assert!(count(&sessions) >= 1, "the hw coordinator lost its seam call");

        // ---- the inverse pins: no direct-path open survives on a wired arm ----
        // The single-clip CPU prewarm.
        assert_eq!(ring.matches(&n("start_with_pts(&r.path")).count(), 0);
        // The single-clip GPU-delegated cold open.
        assert_eq!(ring.matches(&n("choose_decode_path(&r.path")).count(), 0);
        // The software-fallback pool's `LayerFrameSource` construction — the
        // arm where a resolve can be present and its answer still thrown away.
        assert_eq!(multi.matches(&n("path: spec.path.clone()")).count(), 0);

        // ---- the latch-desync pins (the regression class T-58-06-03 names) ----
        // `delegate_gpu_clip`'s hw_failed bookkeeping — the Software-fallback
        // latch, the Failed exit and the panicked-thread exit.
        assert_eq!(ring.matches(&n("hw_failed.insert(r.path")).count(), 0);
        // …and the short-circuit that READS it. Keyed differently from the
        // inserts, "already routed to software" never fires for a substituted
        // media, and every reopen re-attempts a hardware open that just failed.
        assert_eq!(ring.matches(&n("hw_failed.contains(&r.path")).count(), 0);
        // The warm-reuse half of the same class: the identity compare that
        // decides whether a cached session may be re-seeked…
        assert_eq!(ring.matches(&n("cached_path != r.path")).count(), 0);
        // …and the two writes that fill that cache (Completed + Interrupted).
        // The cache must store the media the session actually decodes, or the
        // compare above can never match it.
        assert_eq!(ring.matches(&n("warm_hw = Some((r.path")).count(), 0);
    }
}
