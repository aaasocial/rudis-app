//! SHELL-09's DELIVERY half (Phase 52, plan 52-05): the bounded background
//! extraction job, and the PURE cache read behind the C ABI.
//!
//! `crates/waveform` (plan 52-02) produces peaks. This module decides *when*
//! that happens and *how* the shell is allowed to ask for the result — and the
//! two answers are what make the roadmap's fifth success criterion structural
//! instead of a rule people have to remember:
//!
//! * **Production is a DETACHED, semaphore-bounded background job** started
//!   from `import::import_one_path` beside the poster job. Nothing on the
//!   import path ever awaits it, so `project:changed` — and therefore the
//!   item's appearance in the MediaBin and on the Timeline — never waits on a
//!   decode that costs ~11 s per hour of audio (D-19).
//! * **Consumption is [`read_peaks`], which cannot start work.** There is no
//!   code path from the read to `waveform::extract_peaks`; a miss is a miss.
//!   That is Pitfall 7's forbidden shape made unreachable rather than avoided:
//!   the Timeline's paint can only ever reach a function that reads a file or
//!   returns `None`.
//!
//! # The four `{"Ok": null}` cases (D-21)
//!
//! Cache miss, not-yet-computed, no-audio and an unknown `media_id` all
//! resolve to `Ok(None)`, never `Err`. The cache's own posture is best-effort
//! (`waveform::cache::read` returns `Option`, never `Result`), and an `Err`
//! would force every 100 ms poll to distinguish "broken" from "not ready yet".
//! An `Err` is reserved for a genuinely malformed request, which
//! `crates/ffi`'s `call_json` helper already produces for free.
//!
//! # Threat model
//!
//! * **T-52-21 (path traversal / info disclosure, ASVS V5)** — [`read_peaks`]
//!   treats `media_id` as an OPAQUE DOMAIN ID: it is looked up in the real
//!   `Store` to obtain a path, and is never joined onto the cache dir or any
//!   other filesystem path. A traversal payload is simply an id nothing
//!   matches. Asserted by `read_peaks_for_an_unknown_media_id_is_none_not_an_error`
//!   and, at the ABI, by `media_id_is_never_treated_as_a_path`.
//! * **T-52-22 (import-time resource exhaustion)** — [`MAX_CONCURRENT_WAVEFORM_JOBS`]
//!   on top of 52-02's per-job 5-minute chunk cap. Asserted, not assumed, by
//!   `concurrent_extraction_is_capped`.
//! * **T-52-23 (blocking the import/IPC path)** — [`spawn_extraction`] is
//!   detached and takes OWNED values only: no `ctx`, no store, no borrow, so
//!   no `SharedStore` guard can be alive across it and the call adds no
//!   `.await` to `import_one_path`.
//! * **T-52-24 (a corrupt cache surfacing as an error or a crash)** — 52-02's
//!   fail-closed `cache::read` returns `None` for every hostile input, and
//!   this module maps every `None` to `Ok(None)`.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::Engine as _;

use crate::{AppCtx, SharedStore};
use waveform::cache;
use waveform::CachedPeaks;

/// How many waveform extractions may be decoding at the same instant.
///
/// What this prevents, concretely: a 200-file batch import starting 200
/// decoders, each holding one 5-minute chunk of `f32` PCM — which 52-02
/// MEASURED at ~124 MB apiece (not the 57.6 MB the arithmetic suggests: the
/// frozen `engine::render_audio_pcm` holds the sidecar's stdout `Vec<u8>` and
/// the converted `Vec<f32>` simultaneously).
///
/// **The value 2 is a MEASUREMENT, not a guess.** Plan 52-05 ablated it over a
/// real one-gesture folder import of four 30-minute audio sources (122 minutes
/// of audio), sampling the process working set:
///
/// | cap | peak working set (MiB) | time to all peaks (s) |
/// |-----|-----------------------|-----------------------|
/// | 1   | 132.9                 | 12.03                 |
/// | 2   | 164.8, 148.8          |  6.75,  7.12          |
/// | 4   | 228.7, 229.0          |  4.37,  4.49          |
///
/// Two samples where there are two, so the ~16 MiB of run-to-run noise is
/// visible rather than averaged away: the SEPARATION between caps (133 → 157
/// → 229) is several times that noise, which is what makes the ordering a
/// result rather than a coincidence.
///
/// Marginal cost of one more permit: **~32 MiB**. Going 1 → 2 buys 1.74x the
/// throughput for +24 MiB; 2 → 4 buys only 1.55x more for +72 MiB. Two is the
/// knee. The uncapped 200-file case extrapolates from the same slope to
/// roughly **6.5 GB** — the arithmetic-only projection this comment used to
/// carry said 25 GB, which the measurement corrects; either number is fatal,
/// but only one of them is true. The instrument is
/// `crates/ffi/tests/contract_media.rs::measure_shell09_end_to_end` and the
/// numbers are in `.planning/phases/52-timeline-region/artifacts/52-05-waveform-abi.md`.
///
/// The same DoS-cap shape `engine::audio_sync`'s five-minute sync window and
/// `engine::whisper`'s window cap already set in this codebase — a bound that
/// is a named constant with a test on it, rather than a property of whatever
/// the runtime happens to do.
pub const MAX_CONCURRENT_WAVEFORM_JOBS: usize = 2;

/// The permits. One process-wide semaphore, deliberately: the resource being
/// bounded (RAM, and the FFmpeg sidecar processes the decode spawns) is
/// process-wide too, so a per-ctx bound would multiply with the number of
/// contexts and stop being a bound.
static SEM: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_WAVEFORM_JOBS));

/// What a cache HIT looks like on the wire.
///
/// `peaks_b64` is standard (padded) base64 of the raw `u8` peak bytes — what
/// C#'s `Convert.FromBase64String` reads with no options. A JSON number array
/// would be ~4x larger for a multi-hour source; this payload crosses the C ABI
/// once per media item, on the existing 100 ms cold-path poll, and is then
/// cached client-side.
///
/// `block_us` and `sample_rate` travel WITH the peaks rather than being
/// assumed by the consumer: they are stored per file precisely so a
/// `CACHE_VERSION` bump cannot silently mis-scale older data. `peak_count` is
/// authoritative for mapping the array across a clip — NOT
/// `duration_us / block_us`, which can differ by ~1% when a container
/// overstates its decodable audio (52-02 §2's fixture note).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PeaksPayload {
    pub block_us: i64,
    pub sample_rate: u32,
    pub peak_count: u32,
    pub peaks_b64: String,
}

/// Start a DETACHED, semaphore-bounded background extraction for `path`,
/// writing the result into the peak cache at `dir`.
///
/// Returns `()` IMMEDIATELY and can never propagate a failure into the import
/// path: every error path logs to stderr and returns, exactly like the poster
/// block this parallels. Losing a waveform costs a redraw with no fill, never
/// an import.
///
/// Takes OWNED values only — no `ctx`, no `SharedStore`, no borrow — so
/// `import_one_path`'s lock-discipline invariant (no store guard alive across
/// a spawn) holds by construction rather than by review.
pub fn spawn_extraction(dir: PathBuf, path: PathBuf, duration_us: i64) {
    // `tokio::spawn` PANICS outside a runtime. Every real call site is inside
    // one (`import_one_path` is `async`, and is polled by the host's runtime on
    // all three import surfaces), but this function is `pub` and a panic here
    // would be a panic in the import path — the one thing this module promises
    // never to do. Degrade to "no peaks" instead.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        eprintln!(
            "waveform: no tokio runtime; skipping background peaks for {}",
            path.display()
        );
        return;
    };

    handle.spawn(async move {
        // T-52-22: take a permit BEFORE reaching the blocking pool, so N queued
        // jobs cost N cheap futures rather than N parked blocking threads each
        // waiting for memory to free up.
        let permit = match SEM.acquire().await {
            Ok(p) => p,
            // Unreachable: nothing ever closes this semaphore. Handled anyway
            // because the alternative is an `unwrap` in a detached task.
            Err(_) => return,
        };

        let for_log = path.clone();
        let joined = tokio::task::spawn_blocking(move || extract_and_cache(&dir, &path, duration_us))
            .await;
        if let Err(e) = joined {
            // Mirrors the poster block's own panic arm (`import.rs`): a
            // panicking decode is logged and dropped, never re-raised into a
            // caller that has long since returned.
            eprintln!(
                "waveform: extraction task panicked for {}: {e}",
                for_log.display()
            );
        }
        // The permit drops HERE, on every path including the panicking one —
        // `spawn_blocking` catches the unwind and hands it back as a
        // `JoinError`, so this task itself never unwinds and the permit is
        // never leaked. `an_extraction_panic_does_not_poison_the_import_path`
        // is the gate on that.
        drop(permit);
    });
}

/// The blocking body: key → hit? → extract → cache.
///
/// Never returns an error and never panics on its own account: every failure
/// is a stderr note and an early return.
fn extract_and_cache(dir: &Path, path: &Path, duration_us: i64) {
    // `None` = "not cacheable" (an unreadable stat/timestamp), never a failure.
    // Without a key there is nowhere to put the result, so extracting would
    // burn a decode for nothing.
    let Some(key) = cache::key_for(path) else {
        return;
    };

    // Idempotent re-import: the same unchanged file yields the same
    // `(canonical path, mtime_ns, size_bytes)` key, so a second import of it
    // costs one `stat` and one file read instead of a full decode.
    if cache::read(dir, &key).is_some() {
        return;
    }

    let peaks = match extract(dir, path, duration_us) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("waveform: extraction failed for {}: {e}", path.display());
            return;
        }
    };

    // Deliberately cached even when EMPTY. An empty envelope means "we decoded
    // it, and there was nothing", which is a different answer from "not ready
    // yet" — and caching it is what lets a client stop polling. (A source with
    // no audio stream at all never gets here: the import gate is `has_audio`.)
    //
    // `cache::write` prunes internally (52-02's decision: the entry bound is
    // the cache's property, not the caller's), so there is deliberately no
    // second `cache::prune` call here — one would just re-walk the directory.
    cache::write(
        dir,
        &key,
        &peaks,
        waveform::PEAK_BLOCK_US,
        waveform::AUDIO_SAMPLE_RATE,
    );
}

/// The extraction step, behind one indirection so tests can COUNT it and BOUND
/// it. In every non-test build this is a direct call to
/// `waveform::extract_peaks`.
fn extract(dir: &Path, path: &Path, duration_us: i64) -> Result<Vec<u8>, String> {
    #[cfg(test)]
    if let Some(hook) = test_hook::get(dir) {
        return hook(path, duration_us);
    }
    let _ = dir;
    // `has_audio = true`: the caller has already gated on the probe result
    // (D-20 — any clip WITH audio, video or audio-kind alike), and this crate
    // deliberately never re-probes.
    waveform::extract_peaks(path, duration_us, true).map_err(|e| e.to_string())
}

/// THE PURE READ: whatever is already cached for `media_id`, or `None`.
///
/// **It is not possible for this function to start an extraction.** It reaches
/// exactly three things — the store (to resolve an id to a path), `key_for`
/// (one `stat`) and `cache::read` (one bounded file read) — and none of them
/// can reach a decoder. That is deliberate and load-bearing: it is the only
/// route the Timeline has to peaks, so the roadmap's "never a synchronous call
/// during timeline draw" holds even if a future caller does exactly that.
/// Do not add a "compute it if missing" fallback here; put it on the write
/// path, which is [`spawn_extraction`].
///
/// # `media_id` is an ID, not a path (ASVS V5, T-52-21)
///
/// The caller's string is looked up in the real `Store`; the path, and through
/// it the mtime and size that form the cache key, come from the domain model.
/// The id itself is NEVER joined onto `dir` or any other path, so
/// `"../../../Windows/System32/config/SAM"` is just an id that matches
/// nothing.
pub fn read_peaks(store: &SharedStore, dir: &Path, media_id: &str) -> Option<CachedPeaks> {
    // Scope the guard: the lock is held for one `HashMap`-style lookup and a
    // `String` clone, and is released BEFORE any filesystem work. A poisoned
    // store degrades to a miss rather than unwinding again.
    let path = {
        let store = store.lock().ok()?;
        store.media_item(media_id)?.path.clone()
    };

    let key = cache::key_for(Path::new(&path))?;
    cache::read(dir, &key)
}

/// The app-core entry point `crates/ffi`'s `rudis_get_waveform_peaks` calls.
///
/// All four of D-21's miss cases — cache miss, not-yet-computed, no-audio and
/// an unknown id — are `Ok(None)`, which serializes as exactly
/// `{"Ok":null}`. The only `Err` this can produce is a host that cannot
/// resolve its own cache directory, which is a genuine host fault rather than
/// one of the four, and is unreachable on the C ABI host (`FfiAppCtx`'s
/// `app_cache_dir` is an infallible clone).
pub fn run_get_waveform_peaks<C: AppCtx>(
    ctx: &C,
    media_id: &str,
) -> Result<Option<PeaksPayload>, String> {
    let dir = waveform_dir(ctx)?;
    Ok(
        read_peaks(ctx.store(), &dir, media_id).map(|cached| PeaksPayload {
            block_us: cached.block_us,
            sample_rate: cached.sample_rate,
            // `peaks.len()` is authoritative for the consumer, NOT
            // `duration_us / block_us` (52-02's handed-forward rule).
            peak_count: cached.peaks.len() as u32,
            peaks_b64: base64::engine::general_purpose::STANDARD.encode(&cached.peaks),
        }),
    )
}

/// The peak cache directory, WITHOUT creating it.
///
/// The read path must have no side effects — it runs on a 100 ms poll, once
/// per newly-seen media item, and a read that mints directories is a read that
/// can fail for reasons a read has no business having. `import::waveform_cache_dir`
/// is the write-path twin: same join, plus the `create_dir_all`. The join
/// itself lives HERE, once, so the two cannot drift.
pub(crate) fn waveform_dir<C: AppCtx>(ctx: &C) -> Result<PathBuf, String> {
    Ok(ctx.app_cache_dir()?.join(cache::WAVEFORM_CACHE_DIR_NAME))
}

/// The test-only injection seam for the extraction step.
#[cfg(test)]
pub(crate) mod test_hook {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};

    /// `(source path, duration_us) -> peak bytes`.
    pub(crate) type Hook =
        Arc<dyn Fn(&Path, i64) -> Result<Vec<u8>, String> + Send + Sync + 'static>;

    /// Keyed by the WAVEFORM CACHE DIR the job was spawned against, never
    /// global: `cargo test` runs this binary's tests in parallel threads, and
    /// several of them (plus every unrelated import test in `import.rs`) drive
    /// real imports concurrently. A single global hook would leak across them.
    /// Each `TestAppCtx` owns a fresh temp cache dir, so keying on it makes the
    /// seam per-test by construction.
    ///
    /// The lock is held ONLY to clone the `Arc` out — never across the call —
    /// so a panicking hook (which one gate deliberately installs) cannot poison
    /// the map for the tests running beside it.
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
