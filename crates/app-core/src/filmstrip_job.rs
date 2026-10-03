//! Phase 53.2's DELIVERY half (plan 53.2-04): the bounded background strip
//! extraction job, and the PURE cache read behind the C ABI.
//!
//! `crates/filmstrip` (plan 53.2-02) produces packed thumbnail sheets. This
//! module decides *when* that happens and *how* the shell is allowed to ask for
//! the result — and both answers are lifted, deliberately unchanged, from
//! [`crate::waveform_job`], the module that settled the same two questions for
//! audio peaks one phase ago:
//!
//! * **Production is a DETACHED, semaphore-bounded background job** started from
//!   `import::import_one_path` beside the poster job and the peaks job. Nothing
//!   on the import path ever awaits it, so `project:changed` — and therefore the
//!   item's appearance in the MediaBin and on the Timeline — never waits on a
//!   decode that costs seconds and hundreds of megabytes (D-09, Phase 52's D-19
//!   reused verbatim).
//! * **Consumption is [`read_strip`], which cannot start work.** There is no code
//!   path from the read to `filmstrip::extract_strip`; a miss is a miss. That is
//!   the forbidden "compute it during paint" shape made UNREACHABLE rather than
//!   avoided: the Timeline's paint can only ever reach a function that reads a
//!   file or returns `None`.
//!
//! # The `{"Ok": null}` cases (D-15)
//!
//! Cache miss, not-yet-computed, audio-only media, a still image, an
//! offline/relinked file, a failed decode and an unknown `media_id` all resolve
//! to `Ok(None)`, never `Err`. The cache's own posture is best-effort
//! (`filmstrip::cache::read` returns `Option`, never `Result`), and an `Err`
//! would force every 100 ms poll to distinguish "broken" from "not ready yet".
//! An `Err` is reserved for a genuinely malformed request, which `crates/ffi`'s
//! `call_json` helper already produces for free.
//!
//! # The one thing that is NOT the peaks pipeline: D-14's third state
//!
//! Peaks were written once, at the end, so retrieval was flat: `null` or a
//! payload. A filmstrip fills PROGRESSIVELY, so retrieval has THREE outcomes —
//! absent, partial, complete — and the extra one has to cross the ABI. It does
//! so as a PAIR OF NUMBERS on the existing envelope ([`StripPayload::completed_tiles`]
//! and [`StripPayload::total_tiles`]), not as a new envelope shape and not as a
//! new event: `ring::EVENT_NAMES` stays at 6 and retrieval rides Phase 50 D-06's
//! existing 100 ms cold-path poll.
//!
//! The same pair is what makes re-import correct. [`extract_and_cache`]
//! short-circuits on a COMPLETE cached strip and deliberately does NOT
//! short-circuit on a partial one: a partial entry means the previous job died
//! mid-fill, and re-running from tile 0 overwrites it with a superset. Treating
//! partial as a hit would freeze a half-drawn filmstrip in the cache forever.
//!
//! # Threat model
//!
//! * **T-53.2-13 (path traversal / info disclosure, ASVS V5)** — [`read_strip`]
//!   treats `media_id` as an OPAQUE DOMAIN ID: it is looked up in the real
//!   `Store` to obtain a path, and is never joined onto the cache dir or any
//!   other filesystem path. A traversal payload is simply an id nothing matches.
//!   Asserted by `read_strip_for_an_unknown_media_id_is_none_not_an_error` and,
//!   at the ABI, by `filmstrip_media_id_is_never_treated_as_a_path`.
//! * **T-53.2-14 (import-time resource exhaustion)** — [`MAX_CONCURRENT_FILMSTRIP_JOBS`]
//!   on top of 53.2-02's per-job BYTE-derived chunk cap. Asserted, not assumed,
//!   by `concurrent_extraction_is_capped`, and its VALUE is measured by
//!   `crates/ffi/tests/contract_media.rs::measure_filmstrip_end_to_end`.
//! * **T-53.2-15 (blocking the import/IPC path)** — [`spawn_extraction`] is
//!   detached and takes OWNED values only: no `ctx`, no store, no borrow, so no
//!   `SharedStore` guard can be alive across it and the call adds no suspension
//!   point to `import_one_path`.
//! * **T-53.2-06/07 (a corrupt or torn cache surfacing as an error or a crash)**
//!   — 53.2-02's fail-closed `cache::read` returns `None` for every hostile
//!   input, and this module maps every `None` to `Ok(None)`.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::Engine as _;

use crate::{AppCtx, SharedStore};
use filmstrip::cache;
use filmstrip::CachedStrip;

/// 53.2-02's per-job BYTE budget, re-exported so the measurement that VALIDATES
/// it can read the real number instead of transcribing a copy that can drift.
/// The other half of this plan's DoS cap ([`MAX_CONCURRENT_FILMSTRIP_JOBS`])
/// bounds how many of these may be live at once; the two multiply, and neither
/// is meaningful without the other in the same transcript.
pub use filmstrip::FILMSTRIP_CHUNK_BUDGET_BYTES;

/// How many filmstrip extractions may be decoding at the same instant.
///
/// **This is NOT [`crate::waveform_job::MAX_CONCURRENT_WAVEFORM_JOBS`]'s `2`.**
/// That value was ablated against MONO PCM, where one in-flight job holds ~124
/// MiB. One filmstrip job holds up to `filmstrip::FILMSTRIP_CHUNK_BUDGET_BYTES`
/// (256 MiB) of native-resolution RGBA — an order of magnitude more — so the
/// knee is expected at 1, and copying the audio number would have been exactly
/// the mistake 53.2-02 already had to correct once for the chunk size itself
/// (the five-minute figure that would have attempted ~69 GiB per 1080p chunk).
///
/// The discipline is inherited; the number is measured. `1` entered this file
/// as a STARTING value pending plan 53.2-04's Task 3, and Task 3 then ABLATED
/// it on REAL media — one folder-import gesture over 12 real video sources
/// (720p, 4K, VFR, long-GOP), sampling the process working set, run once per
/// candidate cap:
///
/// | cap | peak working set (MiB) | growth over baseline (MiB) | time to all strips (s) |
/// |-----|------------------------|----------------------------|------------------------|
/// | 1   | 528.1                  | 481.8                      | 23.93                  |
/// | 2   | 780.8                  | 724.0                      | 15.56                  |
///
/// Marginal cost of one more permit: **+242 MiB**, for **1.54x** the
/// throughput. Set that beside the audio precedent's own ablation, which is
/// the whole reason its `2` must not be copied here: there, 1 → 2 bought
/// **1.74x** for **+24 MiB**. This pipeline pays **ten times the memory for
/// less speed**. One is the knee, and it is the knee by an order of magnitude
/// rather than by a hair.
///
/// The instrument is
/// `crates/ffi/tests/contract_media.rs::measure_filmstrip_end_to_end` and the
/// full numbers are in
/// `.planning/phases/53.2-*/artifacts/53.2-04-extraction-measurement.md`.
///
/// Same DoS-cap SHAPE the rest of this codebase already uses — `audio_sync`'s
/// five-minute sync window, `whisper`'s window cap, `waveform_job`'s permit
/// count: a bound that is a named constant with a test on it, rather than a
/// property of whatever the runtime happens to do.
pub const MAX_CONCURRENT_FILMSTRIP_JOBS: usize = 1;

/// The permits. One process-wide semaphore, deliberately: the resource being
/// bounded (RAM, and the FFmpeg sidecar processes the decode spawns) is
/// process-wide too, so a per-ctx bound would multiply with the number of
/// contexts and stop being a bound.
static SEM: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_FILMSTRIP_JOBS));

/// What a cache HIT looks like on the wire.
///
/// Every field name here is a WIRE CONTRACT — plan 53.2-06's C# DTO
/// deserializes them by name. Renaming one is an ABI break with no compiler on
/// the other side to catch it.
///
/// `strip_b64` is standard (padded) base64 of the RAW RGBA rows the cache read
/// returned — what C#'s `Convert.FromBase64String` reads with no options, and
/// exactly the same envelope shape `PeaksPayload::peaks_b64` already crosses on
/// (D-12's instruction was "mirror `rudis_get_waveform_peaks` exactly", and this
/// is what mirroring it means). 53.2-RESEARCH's Pitfall 3 flagged the cost:
/// base64 inflates a full 5.06 MiB sheet to ~6.7 MiB, and a raw `RudisBuffer`
/// variant is the recorded fallback if that ever shows real cost. It does not
/// today — the payload crosses ONCE per media item, on the cold-path poll, and
/// is then cached client-side; Task 3's measurement records the real number.
///
/// `completed_tiles` / `total_tiles` is D-14's three-state distinction on the
/// wire, and `sheet_w` / `sheet_h` describe the bytes actually present (the
/// COMPLETED rows), never the finished grid — a consumer must be able to size
/// its upload from this payload alone without assuming anything about a strip
/// still being written.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StripPayload {
    pub tile_w: u32,
    pub tile_h: u32,
    pub tiles_per_row: u32,
    pub total_tiles: u32,
    pub completed_tiles: u32,
    pub interval_us: i64,
    pub sheet_w: u32,
    pub sheet_h: u32,
    pub strip_b64: String,
}

/// Everything `filmstrip::extract_strip` needs beyond the path, in one value.
///
/// A struct rather than five more parameters threaded through three functions:
/// the extraction step is behind a test seam, and a seam whose signature is a
/// positional list of four `u32`s and two `i64`s is a seam that silently accepts
/// a transposed call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripSpec {
    pub duration_us: i64,
    pub src_w: u32,
    pub src_h: u32,
    pub rotation_degrees: u32,
    pub frame_step_us: i64,
}

/// Start a DETACHED, semaphore-bounded background extraction for `path`, writing
/// revisions into the strip cache at `dir`.
///
/// Returns `()` IMMEDIATELY and can never propagate a failure into the import
/// path: every error path logs to stderr and returns, exactly like the poster
/// block and the peaks block it sits beside. Losing a filmstrip costs a clip
/// drawn with its poster placeholder (D-13), never an import.
///
/// Takes OWNED values only — no `ctx`, no `SharedStore`, no borrow — so
/// `import_one_path`'s lock-discipline invariant (no store guard alive across a
/// spawn) holds by construction rather than by review.
pub fn spawn_extraction(
    dir: PathBuf,
    path: PathBuf,
    duration_us: i64,
    src_w: u32,
    src_h: u32,
    rotation_degrees: u32,
    frame_step_us: i64,
) {
    let spec = StripSpec {
        duration_us,
        src_w,
        src_h,
        rotation_degrees,
        frame_step_us,
    };

    // `tokio::spawn` PANICS outside a runtime. Every real call site is inside one
    // (`import_one_path` is `async`, and is polled by the host's runtime on all
    // three import surfaces), but this function is `pub` and a panic here would
    // be a panic in the import path — the one thing this module promises never to
    // do. Degrade to "no filmstrip" instead.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        eprintln!(
            "filmstrip: no tokio runtime; skipping background strip for {}",
            path.display()
        );
        return;
    };

    handle.spawn(async move {
        // T-53.2-14: take a permit BEFORE reaching the blocking pool, so N queued
        // jobs cost N cheap futures rather than N parked blocking threads each
        // waiting for a quarter-gigabyte of memory to free up.
        let permit = match SEM.acquire().await {
            Ok(p) => p,
            // Unreachable: nothing ever closes this semaphore. Handled anyway
            // because the alternative is an `unwrap` in a detached task.
            Err(_) => return,
        };

        let for_log = path.clone();
        let joined =
            tokio::task::spawn_blocking(move || extract_and_cache(&dir, &path, spec)).await;
        if let Err(e) = joined {
            // Mirrors the poster block's own panic arm (`import.rs`): a panicking
            // decode is logged and dropped, never re-raised into a caller that
            // has long since returned.
            eprintln!(
                "filmstrip: extraction task panicked for {}: {e}",
                for_log.display()
            );
        }
        // The permit drops HERE, on every path including the panicking one —
        // `spawn_blocking` catches the unwind and hands it back as a `JoinError`,
        // so this task itself never unwinds and the permit is never leaked. With
        // a cap of ONE that is not a nicety: a single leaked permit would zero
        // the pool for the rest of the process's life.
        // `an_extraction_panic_does_not_poison_the_import_path` is the gate.
        drop(permit);
    });
}

/// The blocking body: key → complete hit? → extract → publish revisions.
///
/// Never returns an error and never panics on its own account: every failure is
/// a stderr note and an early return.
fn extract_and_cache(dir: &Path, path: &Path, spec: StripSpec) {
    // `None` = "not cacheable" (an unreadable stat/timestamp), never a failure.
    // Without a key there is nowhere to put the result, so extracting would burn
    // a decode for nothing.
    let Some(key) = cache::key_for(path) else {
        return;
    };

    // Idempotent re-import: the same unchanged file yields the same
    // `(canonical path, mtime_ns, size_bytes)` key, so a second import of it
    // costs one `stat` and one file read instead of a full decode.
    //
    // The `is_complete()` qualifier is D-14's resume rule, and it is the one
    // place this module diverges from `waveform_job::extract_and_cache`'s
    // unqualified `if cache::read(..).is_some() { return; }`. A PARTIAL entry
    // means the previous job died mid-fill; short-circuiting on it would pin a
    // half-drawn strip in the cache until the file's mtime changed. Re-running
    // from tile 0 is correct rather than merely acceptable: the new pass
    // produces a SUPERSET of the tiles the partial revision holds, at the same
    // grid, so every rewrite strictly advances `completed_tiles`.
    if let Some(existing) = cache::read(dir, &key) {
        if existing.is_complete() {
            return;
        }
    }

    // The grid is computed HERE as well as inside `extract_strip`, from the same
    // `duration_us`, because the header needs `total_tiles`/`interval_us` and the
    // extractor reports neither — it reports COMPLETED counts against a grid the
    // caller is expected to know. Both derive from `filmstrip::plan_tiles`, so
    // they cannot disagree.
    let (total, interval) = filmstrip::plan_tiles(spec.duration_us);
    if total == 0 || interval <= 0 {
        // A non-positive or absurd duration is "nothing to draw", not an error —
        // and writing an empty strip would cache a grid no consumer can slice.
        return;
    }

    // One published revision per call (D-14). `cache::write` is temp-then-rename
    // per revision by construction (53.2-02), so a concurrent reader observes the
    // previous whole revision or the new whole revision, never a mixture.
    // Returning `false` ABORTS the extraction cleanly: a cache that cannot be
    // written costs one re-extraction, never an import.
    let mut publish = |rows: &[u8], completed: u32| -> bool {
        let header = cache::StripHeader {
            tile_w: filmstrip::TILE_W,
            tile_h: filmstrip::TILE_H,
            tiles_per_row: filmstrip::TILES_PER_ROW,
            total_tiles: total,
            completed_tiles: completed,
            interval_us: interval,
            src_w: spec.src_w,
            src_h: spec.src_h,
        };
        cache::write(dir, &key, &header, rows)
    };

    if let Err(e) = extract(dir, path, spec, &mut publish) {
        eprintln!("filmstrip: extraction failed for {}: {e}", path.display());
    }
}

/// The extraction step, behind one indirection so tests can COUNT it, BOUND it
/// and HOLD IT OPEN. In every non-test build this is a direct call to
/// `filmstrip::extract_strip`.
fn extract(
    dir: &Path,
    path: &Path,
    spec: StripSpec,
    publish: &mut dyn FnMut(&[u8], u32) -> bool,
) -> Result<(), String> {
    #[cfg(test)]
    if let Some(hook) = test_hook::get(dir) {
        return hook(path, spec, publish);
    }
    let _ = dir;
    filmstrip::extract_strip(
        path,
        spec.duration_us,
        spec.src_w,
        spec.src_h,
        spec.rotation_degrees,
        spec.frame_step_us,
        publish,
    )
    .map_err(|e| e.to_string())
}

/// THE PURE READ: whatever is already cached for `media_id`, or `None`.
///
/// **It is not possible for this function to start an extraction.** It reaches
/// exactly three things — the store (to resolve an id to a path), `key_for` (one
/// `stat`) and `cache::read` (one bounded file read) — and none of them can
/// reach a decoder. That is deliberate and load-bearing: it is the only route the
/// Timeline has to frames, so "never a synchronous call during timeline draw"
/// holds even if a future caller does exactly that. Do not add a "compute it if
/// missing" fallback here; put it on the write path, which is
/// [`spawn_extraction`].
///
/// A `Some` whose [`CachedStrip::is_complete`] is `false` is D-14's third state:
/// a real, drawable, still-growing strip — NOT a failure.
///
/// # `media_id` is an ID, not a path (ASVS V5, T-53.2-13)
///
/// The caller's string is looked up in the real `Store`; the path, and through it
/// the mtime and size that form the cache key, come from the domain model. The id
/// itself is NEVER joined onto `dir` or any other path, so
/// `"../../../Windows/System32/config/SAM"` is just an id that matches nothing.
pub fn read_strip(store: &SharedStore, dir: &Path, media_id: &str) -> Option<CachedStrip> {
    // Scope the guard: the lock is held for one lookup and a `String` clone, and
    // is released BEFORE any filesystem work. A poisoned store degrades to a miss
    // rather than unwinding again.
    let path = {
        let store = store.lock().ok()?;
        store.media_item(media_id)?.path.clone()
    };

    let key = cache::key_for(Path::new(&path))?;
    cache::read(dir, &key)
}

/// The app-core entry point `crates/ffi`'s `rudis_get_filmstrip_strip` calls.
///
/// ALL of D-15's cases — cache miss, not-yet-computed, audio-only, a still, an
/// offline file, a failed decode and an unknown id — are `Ok(None)`, which
/// serializes as exactly `{"Ok":null}`. The only `Err` this can produce is a host
/// that cannot resolve its own cache directory, which is a genuine host fault
/// rather than one of those, and is unreachable on the C ABI host (`FfiAppCtx`'s
/// `app_cache_dir` is an infallible clone).
pub fn run_get_filmstrip_strip<C: AppCtx>(
    ctx: &C,
    media_id: &str,
) -> Result<Option<StripPayload>, String> {
    let dir = filmstrip_dir(ctx)?;
    Ok(
        read_strip(ctx.store(), &dir, media_id).map(|cached| StripPayload {
            tile_w: cached.tile_w,
            tile_h: cached.tile_h,
            tiles_per_row: cached.tiles_per_row,
            total_tiles: cached.total_tiles,
            completed_tiles: cached.completed_tiles,
            interval_us: cached.interval_us,
            // Both describe the bytes PRESENT, not the finished grid: `sheet_rows`
            // is computed from `completed_tiles`, so a partial revision reports
            // the smaller sheet it actually carries.
            sheet_w: cached.sheet_w(),
            sheet_h: cached.sheet_rows().saturating_mul(cached.tile_h),
            strip_b64: base64::engine::general_purpose::STANDARD.encode(&cached.rgba),
        }),
    )
}

/// The strip cache directory, WITHOUT creating it.
///
/// The read path must have no side effects — it runs on a 100 ms poll, once per
/// newly-seen media item, and a read that mints directories is a read that can
/// fail for reasons a read has no business having. `import::filmstrip_cache_dir`
/// is the write-path twin: same join, plus the `create_dir_all`. The join itself
/// lives HERE, once, so the two cannot drift.
pub(crate) fn filmstrip_dir<C: AppCtx>(ctx: &C) -> Result<PathBuf, String> {
    Ok(ctx.app_cache_dir()?.join(cache::FILMSTRIP_CACHE_DIR_NAME))
}

/// The test-only injection seam for the extraction step.
#[cfg(test)]
pub(crate) mod test_hook {
    use super::StripSpec;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};

    /// `(source path, spec, publish) -> ()`. The `publish` parameter is what
    /// makes this seam able to stand in for a PROGRESSIVE producer: a hook can
    /// publish zero revisions, one, or drive the real `extract_strip` and let it
    /// publish as many as the file needs.
    pub(crate) type Hook = Arc<
        dyn Fn(&Path, StripSpec, &mut dyn FnMut(&[u8], u32) -> bool) -> Result<(), String>
            + Send
            + Sync
            + 'static,
    >;

    /// Keyed by the FILMSTRIP CACHE DIR the job was spawned against, never
    /// global: `cargo test` runs this binary's tests in parallel threads, and
    /// several of them (plus every unrelated import test in `import.rs`) drive
    /// real imports concurrently. A single global hook would leak across them.
    /// Each `TestAppCtx` owns a fresh temp cache dir, so keying on it makes the
    /// seam per-test by construction.
    ///
    /// The lock is held ONLY to clone the `Arc` out — never across the call — so
    /// a panicking hook (which one gate deliberately installs) cannot poison the
    /// map for the tests running beside it.
    static HOOKS: LazyLock<Mutex<HashMap<PathBuf, Hook>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    pub(crate) fn install(dir: &Path, hook: Hook) {
        HOOKS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(dir.to_path_buf(), hook);
    }

    pub(crate) fn get(dir: &Path) -> Option<Hook> {
        HOOKS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(dir)
            .cloned()
    }
}

#[cfg(test)]
mod tests;
