//! Proxy generation: the orchestration that turns a heavy source into a
//! committed cache entry (Phase 58 — PROXY-01/PROXY-02, decisions D-09, D-10,
//! D-11, D-23).
//!
//! This module owns nothing new. It drives two things that already exist and
//! were each proven on their own:
//!
//! * the **encode mechanism** — the engine's proxy-encode entry point (plan
//!   58-01): all-intra, video-only, below-normal priority, no encoder parameter
//!   at all, and *killable*. It is called from exactly ONE place in this crate
//!   (the line in [`generate`] below), and a `grep -c` for its name in this file
//!   returning anything but 1 means a second encode door has been opened;
//! * the **commit discipline** — [`crate::cache`] (plan 58-02): a hashed stem,
//!   a `.tmp.`-named payload that only reaches its final name on a clean exit,
//!   and a meta file written LAST as the commit marker.
//!
//! What is assembled here, and nowhere else, is the end-to-end shape:
//!
//! ```text
//! key_for -> read_fresh? -> probe -> proxy_dims? -> spawn -> poll ---------> rename -> write_meta -> prune
//!                |             |          |                   |  cancel?              (commit marker)
//!             AlreadyFresh  Failed   NotProxiable              +--> KILL the child, delete the tmp -> Cancelled
//! ```
//!
//! # D-11: cancellation is a KILL, not a flag (research Pitfall 5)
//!
//! The codebase's existing cancel precedent (`agent-gen`'s `Arc<AtomicBool>`
//! latch) cancels a *remote, polled* job, where "stop polling" is the whole
//! mechanism. A local sidecar is a different animal: **a single `ffmpeg` CLI
//! invocation has no loop in which to observe a flag** — once spawned it runs to
//! completion, so a flag-only cancel would return `Cancelled` promptly while the
//! encode kept burning the CPU that D-10 says playback is entitled to. The flag
//! is only how the *decision* reaches this loop; [`engine::ProxyEncodeChild::kill_and_reap`]
//! is the mechanism that carries it out.
//!
//! And because a killed encode is by definition a half-written file, the second
//! half of D-11 ("no partial file a later run could mistake for a complete
//! proxy") is structural: the encode only ever writes to a `.tmp.`-named path,
//! nothing is renamed except after a zero exit, and no meta — the cache's commit
//! marker — is ever written for a cancelled run. The cancel arm additionally
//! deletes the tmp, so not even a swept-later orphan is left behind.
//!
//! # The directory is the CALLER's (58-02's threat flag, answered)
//!
//! [`crate::cache::prune_bytes`] deletes files inside whatever directory it is
//! handed, and 58-02 flagged that as a hazard worth naming here. [`generate`]'s
//! answer: it **never invents a directory**. It writes into, and prunes, exactly
//! the `dir` it was given — the same one it just committed a payload and a meta
//! into — and only on the success path. It does not walk upward, it does not
//! derive a sibling, and it does not prune on any early return. Choosing that
//! directory (`AppCtx::app_cache_dir()/proxies`, D-14) is the job layer's
//! decision (plan 58-05), made once, from one named constant
//! ([`crate::cache::PROXY_CACHE_DIR_NAME`]).
//!
//! # Blocking, by design (D-09)
//!
//! [`generate`] blocks its calling thread for the whole encode. It is meant to
//! run on a blocking pool (`spawn_blocking`), one at a time — the Semaphore(1)
//! worker cap is plan 58-05's, not this module's. What D-09 forbids is blocking
//! *import, editing or playback*, and a function that owns no lock and no store
//! guard cannot: it takes a path, a directory and a latch, and returns a value.
//!
//! Nothing here panics and nothing returns `Err`: every failure is a
//! [`GenerateOutcome`] variant, because a proxy that cannot be built costs speed
//! and never correctness.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::cache;

/// How long the poll loop sleeps between checks of the cancel latch and the
/// child's exit status.
///
/// 50 ms is the whole latency budget of a cancellation: the flag is observed at
/// most one interval after it is raised. Short enough that "cancel" feels
/// immediate against an encode measured in seconds, long enough that a
/// multi-second encode costs a few dozen wakeups rather than thousands.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Marker embedded in the in-flight payload's name.
///
/// It MUST match the marker [`crate::cache::prune_bytes`] sweeps, or an
/// abandoned tmp would live forever. That coupling is pinned behaviourally by
/// `an_abandoned_tmp_payload_is_swept_by_the_cache` rather than by a shared
/// constant, so the test fails if either side drifts.
const TMP_MARKER: &str = ".tmp.";

/// Windows only releases a killed process's file handle *shortly* after
/// `TerminateProcess` + `WaitForSingleObject` return — 58-01 MEASURED
/// `ERROR_SHARING_VIOLATION` (os error 32) on the instruction after the reap,
/// clearing after 0 ms single-threaded and bounded at 2 s under parallel load.
/// A single best-effort `remove_file` would therefore sometimes leave the very
/// tmp file D-11 promises is gone.
const HANDLE_RETRY_INTERVAL: Duration = Duration::from_millis(10);
/// See [`HANDLE_RETRY_INTERVAL`]. 2 s, matching 58-01's measured bound.
const HANDLE_RETRY_BUDGET: Duration = Duration::from_secs(2);

/// Monotonic per-call nonce, so two concurrent generations of the SAME source
/// (which resolve to the same stem) cannot share one tmp path and
/// interleave-corrupt it. Mirrors `cache::TMP_COUNTER` and its stated reason.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Every way a generation attempt can end. Deliberately NOT a `Result`: four of
/// the five are ordinary, expected answers, and only [`Failed`](Self::Failed) is
/// a problem — and even that one is a cache miss, not a playback failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerateOutcome {
    /// A real, playable, all-intra proxy is now committed in the cache.
    ///
    /// `wall_ms` is D-23's measurement: generation cost is MEASURED and
    /// reported, never hidden. `bytes` is the payload's exact length on disk.
    Created { wall_ms: u64, bytes: u64 },
    /// A current proxy for this exact source already exists. Nothing was
    /// spawned — not even an `ffprobe` — because the freshness check comes
    /// first and costs stats plus one bounded read.
    AlreadyFresh,
    /// This source will never be proxied: it is already at or below the target
    /// long edge (D-05 — there is nothing to win), or it is not a cacheable
    /// file, or it carries no video dimensions at all. A policy answer, not an
    /// error.
    NotProxiable,
    /// The cancel latch was raised while the encode was in flight. The child was
    /// KILLED (not merely abandoned) and the in-flight payload deleted, so the
    /// cache directory is exactly as it was before the call.
    Cancelled,
    /// Something went wrong; the string is for logs and tests. The cache is
    /// left with nothing half-committed.
    Failed(String),
}

/// The in-flight payload's file name for `stem`, unique per call:
/// `{stem}.proxy.tmp.{pid}-{nonce}.mp4`.
///
/// Three constraints meet in this one string, and MEASURED evidence forced the
/// shape:
///
/// 1. **It must still end in `.mp4`.** ffmpeg chooses its muxer from the output
///    file's EXTENSION. The obvious temp shape — appending the marker, exactly
///    as [`crate::cache::write_meta`] does for its JSON — yields
///    `{stem}.proxy.mp4.tmp.1234-5`, whose extension is `.1234-5`, and every
///    encode dies with *"Unable to choose an output format ... use a standard
///    extension for the filename or specify the format manually"* before writing
///    a byte. (The cache's meta temp is unaffected: `std::fs` sniffs nothing.)
///    So the marker goes in the MIDDLE and the extension stays last.
/// 2. **It must carry `.proxy.`**, or [`crate::cache::prune_bytes`] ignores it
///    entirely and an abandoned encode's output leaks forever.
/// 3. **It must carry the `.tmp.` marker**, or that same sweep — which checks
///    for the marker BEFORE it classifies payloads — would see a file that is
///    not a `.proxy.mp4` and not a `.proxy.json`, or worse a payload with no
///    meta, and act on a LIVE writer's output.
///
/// It must also NOT end in `.proxy.mp4`, or a reader could mistake an in-flight
/// write for a committed proxy. All four properties are pinned by
/// `the_tmp_payload_name_is_recognisable_to_the_cache_sweep`, and the extension
/// is derived from [`crate::cache::PROXY_PAYLOAD_SUFFIX`] rather than retyped,
/// so a change to the payload suffix carries through here automatically.
pub fn tmp_payload_name(stem: &str) -> String {
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    // ".proxy.mp4" -> (".proxy", "mp4")
    let (infix, ext) = cache::PROXY_PAYLOAD_SUFFIX
        .rsplit_once('.')
        .unwrap_or((".proxy", "mp4"));
    format!(
        "{stem}{infix}{TMP_MARKER}{}-{nonce:x}.{ext}",
        std::process::id()
    )
}

/// Delete `path`, retrying briefly while Windows still holds the killed child's
/// handle. Best-effort: a file that still cannot be removed is left for
/// [`crate::cache::prune_bytes`]'s stale-temp sweep, which is exactly why that
/// sweep exists.
fn remove_with_retry(path: &Path) {
    let deadline = Instant::now() + HANDLE_RETRY_BUDGET;
    loop {
        if !path.exists() {
            return;
        }
        if std::fs::remove_file(path).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "proxy generate: could not delete {} — leaving it for the cache's \
                 stale-temp sweep",
                path.display()
            );
            return;
        }
        std::thread::sleep(HANDLE_RETRY_INTERVAL);
    }
}

/// Rename `from` over `to`, retrying briefly for the same Windows reason
/// [`remove_with_retry`] documents: a just-exited child's handle can outlive its
/// exit status by a few tens of milliseconds.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let deadline = Instant::now() + HANDLE_RETRY_BUDGET;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(HANDLE_RETRY_INTERVAL);
            }
        }
    }
}

/// Generate a playback proxy for `source` into the cache directory `dir`,
/// abandoning the work — for real — if `cancel` is raised.
///
/// Blocking (see the module doc), never panicking, never `Err`. The full
/// pipeline is: identity, freshness, geometry, spawn, poll/kill, atomic rename,
/// meta LAST, prune.
///
/// # Order of the early exits, and why it is this order
///
/// 1. **Identity** ([`cache::key_for`]) — an unstattable or non-file source is
///    [`NotProxiable`](GenerateOutcome::NotProxiable) before anything is spawned.
/// 2. **Freshness** ([`cache::read_fresh`]) — an unchanged source with a current
///    proxy is [`AlreadyFresh`](GenerateOutcome::AlreadyFresh) for the cost of
///    stats plus one bounded read. This is checked BEFORE the probe on purpose:
///    re-importing the same file is the common case, and it must not cost an
///    `ffprobe` subprocess (`filmstrip_job`'s rule — the cheap check goes first).
/// 3. **Geometry** ([`cache::proxy_dims`]) — a source already at or below the
///    target long edge is [`NotProxiable`](GenerateOutcome::NotProxiable) with
///    no encode spawned at all (D-05).
///
/// Only after all three does anything expensive happen.
///
/// A delegate of [`generate_with_progress`] with a throwaway progress cell —
/// byte-equivalent behaviour for every caller that does not want the number.
pub fn generate(source: &Path, dir: &Path, cancel: &AtomicBool) -> GenerateOutcome {
    generate_with_progress(source, dir, cancel, &AtomicU32::new(0))
}

/// [`generate`], plus a live progress value (71-01, TRUST-03).
///
/// While the encode runs, every [`POLL_INTERVAL`] this reads the child's own
/// `out_time_us` ([`engine::ProxyEncodeChild::progress_out_time_us`], parsed
/// off ffmpeg's `-progress` stream) and publishes
/// `out_time_us * 1000 / duration_us` — the SOURCE's probed duration, which a
/// proxy shares (same timebase, same duration) — into `progress_permille` via
/// `fetch_max`, clamped to `0..=999`:
///
/// * **monotonic at the source** — `fetch_max` means a reader can never see
///   the value go backwards, whatever the child reports;
/// * **never 1000** — "done" is the caller's `ready` state (the commit below),
///   not a number; a bar at 100 % over an encode that then fails to commit
///   would be a lie;
/// * **no estimate** — with no probed duration, or before the child's first
///   report, nothing is written. Every early exit ([`GenerateOutcome::NotProxiable`],
///   [`GenerateOutcome::AlreadyFresh`], ...) leaves the cell untouched.
///
/// An `&AtomicU32` is the whole contract: no callback, no `Send` bound — the
/// job layer hands one into `spawn_blocking` and polls it from its status getter.
pub fn generate_with_progress(
    source: &Path,
    dir: &Path,
    cancel: &AtomicBool,
    progress_permille: &AtomicU32,
) -> GenerateOutcome {
    // (1) Identity. `None` means "not a cacheable file"; the caller simply
    //     keeps playing the original.
    let Some(key) = cache::key_for(source) else {
        return GenerateOutcome::NotProxiable;
    };

    // (2) Freshness, before the probe. See the doc comment above.
    if cache::read_fresh(source, dir).is_some() {
        return GenerateOutcome::AlreadyFresh;
    }

    // (3) Geometry, from the source's real dimensions.
    let info = match engine::probe(source) {
        Ok(info) => info,
        Err(e) => {
            return GenerateOutcome::Failed(format!(
                "proxy generate: probe failed for {}: {e}",
                source.display()
            ))
        }
    };
    let Some((out_w, out_h)) = cache::proxy_dims(info.width, info.height) else {
        return GenerateOutcome::NotProxiable;
    };

    // A latch already raised before the work starts is honoured without paying
    // for a spawn — cancellation is cheapest when it is earliest.
    if cancel.load(Ordering::Relaxed) {
        return GenerateOutcome::Cancelled;
    }

    if let Err(e) = std::fs::create_dir_all(dir) {
        return GenerateOutcome::Failed(format!(
            "proxy generate: cannot create the cache directory {}: {e}",
            dir.display()
        ));
    }

    let stem = cache::file_stem_for(&key);
    let final_path = dir.join(cache::payload_file_name(&stem));
    // The encode writes HERE, never to `final_path`. That single fact is what
    // makes a killed, crashed or failed run unable to leave anything a later
    // read could mistake for a proxy.
    let tmp_path = dir.join(tmp_payload_name(&stem));

    // D-23: generation cost is measured, starting from before the spawn so the
    // encoder-availability probe is counted honestly as part of the cost.
    let started = Instant::now();

    // The NON-MF FALLBACK bitrate (debug `proxy-bitrate-starved-all-intra`):
    // bits-per-pixel x proxy pixels x source fps. On the shipped `h264_mf`
    // path the engine encodes QUALITY-TARGETED (`engine::PROXY_ENCODE_QUALITY`)
    // and ignores this value entirely; it is only emitted as `-b:v` when the
    // loud DEV-only encoder override selects a non-MF encoder that cannot take
    // the MF-private quality options. Pixel-rate-derived, never flat: the flat
    // 6 Mbps constant this replaced starved all-intra encodes of dense
    // high-fps content to 0.19 bpp and macroblocked playback. fps guards:
    // avg_frame_rate is the authoritative working rate; a source reporting no
    // usable rate is priced at 30 fps, and a floor of 1 Mbps keeps a
    // degenerate tiny geometry from producing a bitrate the encoder refuses.
    let fps = if info.avg_frame_rate > 0.0 {
        info.avg_frame_rate
    } else if info.r_frame_rate > 0.0 {
        info.r_frame_rate
    } else {
        30.0
    };
    let fallback_bitrate_bps = (((cache::PROXY_FALLBACK_BPP_MILLI as f64) / 1000.0)
        * (out_w as f64)
        * (out_h as f64)
        * fps)
        .round()
        .max(1_000_000.0) as u64;

    // The ONLY encode entry point in this crate. It deliberately takes no
    // encoder parameter — that omission IS the licensing control (D-03/D-32),
    // and an encoder that is unavailable arrives here LOUD, never silent.
    let mut child = match engine::spawn_proxy_encode(
        source,
        &tmp_path,
        out_w,
        out_h,
        fallback_bitrate_bps,
    ) {
        Ok(child) => child,
        Err(e) => {
            return GenerateOutcome::Failed(format!(
                "proxy generate: encode could not start for {}: {e}",
                source.display()
            ))
        }
    };

    // ---- the poll loop -------------------------------------------------
    //
    // The cancel latch is checked FIRST on every iteration, before the exit
    // status, so a raised flag is never overtaken by an encode that finished in
    // the same instant.
    //
    // RESEARCH PITFALL 5, in one line: the flag alone is INSUFFICIENT. There is
    // no loop inside a CLI encode in which to observe it — one `ffmpeg`
    // invocation runs to completion once spawned. `kill_and_reap()` is the
    // mechanism; the flag is only how the decision gets here.
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            child.kill_and_reap();
            remove_with_retry(&tmp_path);
            return GenerateOutcome::Cancelled;
        }
        if info.duration_us > 0 {
            let out_time_us = child.progress_out_time_us();
            if out_time_us >= 0 {
                let permille = ((out_time_us as i128 * 1000) / (info.duration_us as i128))
                    .clamp(0, 999) as u32;
                progress_permille.fetch_max(permille, Ordering::Relaxed);
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => {
                child.kill_and_reap();
                remove_with_retry(&tmp_path);
                return GenerateOutcome::Failed(format!(
                    "proxy generate: could not poll the encode of {}: {e}",
                    source.display()
                ));
            }
        }
    };

    if !status.success() {
        // `wait()` joins the engine's stderr PUMP thread — the pipe has been
        // drained concurrently since the spawn, which is what makes the poll
        // loop above safe on a chatty encode — so this is where the encode's
        // real diagnostics come from. (The child is already reaped by
        // `try_wait`; `wait` returns the stored status.)
        let detail = match child.wait() {
            Err(e) => e.to_string(),
            Ok(()) => format!("exited with {status:?} but reported no error"),
        };
        remove_with_retry(&tmp_path);
        return GenerateOutcome::Failed(format!(
            "proxy generate: encode failed for {}: {detail}",
            source.display()
        ));
    }

    // ---- commit --------------------------------------------------------
    //
    // Payload FIRST (an atomic rename into the final name), meta LAST. Between
    // those two lines the entry is an orphan payload, which `read_fresh` treats
    // as a MISS and `prune_bytes` sweeps.
    if let Err(e) = rename_with_retry(&tmp_path, &final_path) {
        remove_with_retry(&tmp_path);
        return GenerateOutcome::Failed(format!(
            "proxy generate: could not publish {} as {}: {e}",
            tmp_path.display(),
            final_path.display()
        ));
    }

    let payload_bytes = match std::fs::metadata(&final_path) {
        Ok(stat) => stat.len(),
        Err(e) => {
            remove_with_retry(&final_path);
            return GenerateOutcome::Failed(format!(
                "proxy generate: cannot stat the encoded payload {}: {e}",
                final_path.display()
            ));
        }
    };

    let meta = cache::ProxyMeta::new(key, out_w, out_h, payload_bytes);
    if !cache::write_meta(dir, &stem, &meta) {
        // A payload with no meta is an orphan the sweep would eventually eat.
        // Leaving one DELIBERATELY would be worse than useless: it burns disk to
        // buy a permanent MISS. Remove it now.
        remove_with_retry(&final_path);
        return GenerateOutcome::Failed(format!(
            "proxy generate: the cache refused to commit a proxy for {}",
            source.display()
        ));
    }

    // The bound belongs to the cache, enforced after every successful write
    // (filmstrip's rule). `write_meta` already prunes on a successful commit;
    // repeating it here is deliberate and cheap — it keeps the byte budget
    // legible at the layer that just added bytes, and a second pass over an
    // already-bounded directory evicts nothing. `dir` is the directory this
    // call was handed and just wrote into, never one this function derived.
    cache::prune_bytes(dir, cache::MAX_PROXY_CACHE_BYTES);

    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    GenerateOutcome::Created {
        wall_ms,
        bytes: payload_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tmp name must satisfy all four constraints [`tmp_payload_name`]
    /// documents. Three of them are about the cache sweep; the fourth is about
    /// ffmpeg, and it is the one that was learned the hard way.
    #[test]
    fn the_tmp_payload_name_is_recognisable_to_the_cache_sweep() {
        let name = tmp_payload_name("deadbeefdeadbeef-0000000000000010");
        assert!(
            name.contains(".proxy."),
            "the sweep ignores any name without .proxy. — {name} would leak forever"
        );
        assert!(
            name.contains(TMP_MARKER),
            "without {TMP_MARKER} the sweep would classify {name} as an orphan payload \
             and delete it while the encoder still holds it"
        );
        assert!(
            !name.ends_with(cache::PROXY_PAYLOAD_SUFFIX),
            "an in-flight payload must NOT be readable as a final payload: {name}"
        );
        // MEASURED: ffmpeg picks its muxer from the EXTENSION. With the marker
        // appended instead of infixed, the extension became `.1234-5` and every
        // encode failed with "Unable to choose an output format" before writing
        // a byte. This assertion is that failure, made permanent.
        assert!(
            name.ends_with(".mp4"),
            "ffmpeg chooses its muxer from the extension — {name} would make every \
             proxy encode fail to even open its output"
        );
    }

    fn workspace_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf()
    }

    /// Point the engine at the repo's BUNDLED LGPL build unless the caller
    /// already chose one; `false` (skip, loudly) when none exists.
    fn bundled_ffmpeg_ready() -> bool {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            if std::env::var_os("RUDIS_FFMPEG_DIR").is_none() {
                let cand = workspace_root().join("runtime").join("binaries");
                if cand.is_dir() {
                    std::env::set_var("RUDIS_FFMPEG_DIR", &cand);
                }
            }
        });
        if std::env::var_os("RUDIS_FFMPEG_DIR").is_some() {
            return true;
        }
        eprintln!("SKIPPING: no bundled LGPL ffmpeg (runtime/binaries) and RUDIS_FFMPEG_DIR unset");
        false
    }

    /// Copy a `test-media` fixture to a UNIQUE temp path: the cache key is
    /// path-derived, so a unique path is a genuinely cold proxy.
    fn unique_copy(fixture: &str, tmp: &Path) -> Option<std::path::PathBuf> {
        let src = workspace_root().join("test-media").join(fixture);
        if !src.is_file() {
            eprintln!("SKIPPING: fixture {} not present", src.display());
            return None;
        }
        let dst = tmp.join(format!("src-{}-{fixture}", std::process::id()));
        std::fs::copy(&src, &dst).expect("copy the fixture");
        Some(dst)
    }

    /// 71-01 (TRUST-03): during a REAL 4K proxy encode, a caller holding the
    /// `AtomicU32` sees a monotonic permille in 1..=999 that took at least two
    /// distinct values — derived from the child's own `out_time_us` and the
    /// source's probed duration.
    #[test]
    fn generate_with_progress_publishes_a_monotonic_permille_during_a_real_encode() {
        if !bundled_ffmpeg_ready() {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let Some(source) = unique_copy("bars_4k30_5s.mp4", tmp.path()) else {
            return;
        };
        let dir = tmp.path().join("proxies");

        let progress = std::sync::Arc::new(AtomicU32::new(0));
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let worker = {
            let progress = std::sync::Arc::clone(&progress);
            let done = std::sync::Arc::clone(&done);
            let source = source.clone();
            let dir = dir.clone();
            std::thread::spawn(move || {
                let cancel = AtomicBool::new(false);
                let outcome = generate_with_progress(&source, &dir, &cancel, &progress);
                done.store(true, Ordering::SeqCst);
                outcome
            })
        };

        let mut samples: Vec<u32> = Vec::new();
        while !done.load(Ordering::SeqCst) {
            samples.push(progress.load(Ordering::Relaxed));
            std::thread::sleep(Duration::from_millis(20));
        }
        samples.push(progress.load(Ordering::Relaxed));
        let outcome = worker.join().expect("the worker must not panic");

        let mut distinct = samples.clone();
        distinct.dedup();
        println!("permille samples: {} read, distinct {distinct:?}", samples.len());

        assert!(
            matches!(outcome, GenerateOutcome::Created { .. }),
            "expected Created, got {outcome:?}"
        );
        assert!(distinct.len() >= 2, "the permille never moved: {distinct:?}");
        assert!(
            samples.windows(2).all(|w| w[1] >= w[0]),
            "the permille went backwards: {samples:?}"
        );
        let max = *samples.iter().max().unwrap();
        assert!((1..=999).contains(&max), "max permille {max} outside 1..=999");
    }

    /// 71-01: an early exit (a source already at or under the 960 px proxy long
    /// edge is NotProxiable) never touches the
    /// progress cell.
    #[test]
    fn an_early_exit_leaves_the_progress_at_zero() {
        if !bundled_ffmpeg_ready() {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let Some(source) = unique_copy("bars_640x480_30p_2s_untagged.mp4", tmp.path()) else {
            return;
        };
        let progress = AtomicU32::new(0);
        let outcome = generate_with_progress(
            &source,
            &tmp.path().join("proxies"),
            &AtomicBool::new(false),
            &progress,
        );
        assert!(
            matches!(outcome, GenerateOutcome::NotProxiable),
            "a 640x480 source is under the proxy long edge: {outcome:?}"
        );
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }

    /// Two calls must never collide, even for the same stem — concurrent
    /// generations of one source would otherwise share a handle.
    #[test]
    fn tmp_payload_names_are_unique_per_call() {
        let a = tmp_payload_name("same-stem");
        let b = tmp_payload_name("same-stem");
        assert_ne!(a, b, "the per-call nonce must make tmp names unique");
    }
}
