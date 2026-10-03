//! Plan 52-05, Task 1's gates.
//!
//! # Why these are IN-CRATE tests and not `crates/app-core/tests/waveform_job.rs`
//!
//! The plan names an integration-test file. `app-core` cannot have one for this
//! subject: three of the seven gates below drive a REAL import, which needs an
//! [`AppCtx`](crate::AppCtx), and this crate's only one is
//! `crate::test_support::TestAppCtx` — declared `#[cfg(test)]` in `lib.rs:159`
//! and therefore invisible to an integration test, which compiles as a separate
//! crate against the ordinary library. That is why `crates/app-core` has NO
//! `tests/` directory at all today: every test it owns is in-crate, for exactly
//! this reason. Making `test_support` reachable would mean a cargo feature, a
//! self-dev-dependency and promoting `tempfile` to a non-optional runtime
//! dependency of a SHIPPED crate — a manifest restructure of shared code, for a
//! test's file location, while Phase 51 is live on this tree. The gates, their
//! names and what they prove are unchanged; only the file they live in moved.
//! Run them with `cargo test -p app-core waveform_job::`.
//!
//! # The one stubbed step, and why it is not a mocked feature
//!
//! CLAUDE.md rule 1 forbids mocking a core editing operation. Nothing here
//! mocks one: every gate imports a REAL fixture through the REAL import path,
//! against real `ffprobe` metadata, and the peaks that land in the cache are
//! produced by the REAL `waveform::extract_peaks`. What the seam in
//! [`super::test_hook`] replaces is only the *scheduling observability* of that
//! call — a counter, a barrier, or a gate that holds the decode open — because
//! "the import returned BEFORE the peaks were ready" and "at most two decoders
//! ran at once" are claims about ORDER and CONCURRENCY that a real decode's
//! timing cannot assert without being flaky. `read_peaks_never_triggers_
//! computation` is the same idea: a counter is the only way to prove a call did
//! NOT happen.

use super::*;
use crate::test_support::{fixture, TestAppCtx};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A multi-threaded runtime for the gates that do not need a full `AppCtx`.
///
/// Multi-thread, not current-thread: [`super::spawn_extraction`] detaches a
/// task that then `spawn_blocking`s, and the whole point of
/// `concurrent_extraction_is_capped` is that two of those overlap.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .build()
        .expect("test runtime builds")
}

/// A REAL file on disk with `bytes` in it.
///
/// Not a convenience: `waveform::cache::key_for` canonicalizes and `stat`s its
/// argument and returns `None` for anything that is not a real file, so a job
/// spawned against an imaginary path would return before ever reaching the
/// extraction step — and every counting gate below would pass vacuously.
fn real_file(dir: &Path, name: &str, bytes: usize) -> PathBuf {
    std::fs::create_dir_all(dir).expect("scratch dir");
    let path = dir.join(name);
    std::fs::write(&path, vec![b'x'; bytes]).expect("write scratch file");
    path
}

/// Poll `pred` until it holds or `timeout` elapses; returns whether it held.
fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Records every extraction the job path performs, and the high-water mark of
/// how many were inside it at the same instant.
#[derive(Default)]
struct Recorder {
    calls: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl Recorder {
    fn enter(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
    }

    fn leave(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

/// The waveform cache dir a `TestAppCtx`'s imports will actually use — the same
/// value `import_one_path` resolves through `waveform_cache_dir(ctx)`.
fn ctx_dir(ctx: &TestAppCtx) -> PathBuf {
    waveform_dir(ctx).expect("test ctx resolves a cache dir")
}

/// Import one real path through the REAL UI import path.
fn import(ctx: &TestAppCtx, path: &str) -> Vec<rudis_core::MediaBinItem> {
    ctx.block_on(crate::import::run_import_media_ui(
        ctx,
        vec![path.to_string()],
    ))
    .expect("the real import path succeeds on a real fixture")
}

// ---------------------------------------------------------------------------
// D-19 — the import path never waits for peaks
// ---------------------------------------------------------------------------

/// The load-bearing ordering gate: the `MediaBinItem` is in the store and the
/// import call has RETURNED while the extraction is still running.
///
/// Deterministic by construction rather than by timing: the extraction is held
/// open on a gate the test releases, so "still `None` when import returned" is
/// a fact, not a race that usually wins. Once released, the REAL
/// `waveform::extract_peaks` runs against the REAL fixture and the assertions
/// are against its real output (372 peaks of 10 ms each for a 3.72 s source).
#[test]
fn import_returns_before_peaks_are_ready() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));

    {
        let released = Arc::clone(&released);
        let entered = Arc::clone(&entered);
        test_hook::install(
            &dir,
            Arc::new(move |path: &Path, duration_us: i64| {
                entered.store(true, Ordering::SeqCst);
                while !released.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // REAL extraction, real decode, real bytes.
                waveform::extract_peaks(path, duration_us, true).map_err(|e| e.to_string())
            }),
        );
    }

    let items = import(&ctx, &fixture("speech_en.mp4"));
    assert_eq!(items.len(), 1, "the audio fixture imported");
    let media_id = items[0].id.clone();
    assert!(items[0].has_audio, "speech_en.mp4 really does carry audio");

    // (a) The item is ALREADY in the backend-owned store...
    assert!(
        ctx.store()
            .lock()
            .expect("store")
            .media_item(&media_id)
            .is_some(),
        "the MediaBinItem lands in the bin as part of the import call"
    );
    // (b) ...while the peaks are provably NOT ready.
    assert!(
        read_peaks(ctx.store(), &dir, &media_id).is_none(),
        "import must NOT block on extraction (D-19)"
    );
    assert!(
        wait_until(Duration::from_secs(10), || entered.load(Ordering::SeqCst)),
        "the background job really did start"
    );
    assert!(
        read_peaks(ctx.store(), &dir, &media_id).is_none(),
        "still not ready while the decode is held open — the ORDER is the claim"
    );

    // Now let it finish, and prove the real envelope arrives.
    released.store(true, Ordering::SeqCst);
    assert!(
        wait_until(Duration::from_secs(120), || read_peaks(
            ctx.store(),
            &dir,
            &media_id
        )
        .is_some()),
        "peaks arrive asynchronously after the import returned"
    );

    let cached = read_peaks(ctx.store(), &dir, &media_id).expect("a hit");
    assert_eq!(cached.block_us, waveform::PEAK_BLOCK_US);
    assert_eq!(cached.sample_rate, waveform::AUDIO_SAMPLE_RATE);
    let expected = (items[0].duration_us / waveform::PEAK_BLOCK_US) as usize;
    assert!(
        cached.peaks.len().abs_diff(expected) <= 2,
        "real envelope sized to the real duration: got {} peaks, expected ~{expected}",
        cached.peaks.len()
    );
    assert!(
        cached.peaks.iter().any(|&p| p > 0),
        "real speech is not silence"
    );
}

// ---------------------------------------------------------------------------
// T-52-22 — the DoS cap is asserted, not assumed
// ---------------------------------------------------------------------------

/// 12 jobs, at most [`MAX_CONCURRENT_WAVEFORM_JOBS`] decoders alive at once.
///
/// The number that matters is the HIGH-WATER MARK, and it is compared against
/// the constant rather than a literal `2`, so re-tuning the cap re-tunes the
/// gate instead of breaking it.
#[test]
fn concurrent_extraction_is_capped() {
    const JOBS: usize = 12;
    let scratch = tempfile::tempdir().expect("scratch");
    let dir = scratch.path().join("waveforms");
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(move |_path: &Path, _duration_us: i64| {
                rec.enter();
                // Held long enough that any second permit-holder provably
                // overlaps this one; short enough that 12 jobs at a cap of 2
                // finish in ~1.2 s.
                std::thread::sleep(Duration::from_millis(200));
                rec.leave();
                Ok(vec![7u8; 16])
            }),
        );
    }

    let sources: Vec<PathBuf> = (0..JOBS)
        .map(|i| real_file(scratch.path(), &format!("src{i}.bin"), 32 + i))
        .collect();

    let rt = runtime();
    rt.block_on(async {
        for src in &sources {
            spawn_extraction(dir.clone(), src.clone(), 1_000_000);
        }
    });

    assert!(
        wait_until(Duration::from_secs(60), || rec.calls() >= JOBS),
        "all {JOBS} jobs ran (saw {})",
        rec.calls()
    );
    assert_eq!(rec.calls(), JOBS, "every job ran exactly once");
    assert_eq!(
        rec.max_in_flight(),
        MAX_CONCURRENT_WAVEFORM_JOBS,
        "a batch import must never start more than MAX_CONCURRENT_WAVEFORM_JOBS \
         decoders at once (T-52-22); observed {}",
        rec.max_in_flight()
    );
}

/// A panicking job is caught, and the permit it held is released — the very
/// next job still runs. A leaked permit would silently halve (then zero) the
/// pool for the rest of the process's life.
#[test]
fn an_extraction_panic_does_not_poison_the_import_path() {
    let scratch = tempfile::tempdir().expect("scratch");
    let dir = scratch.path().join("waveforms");
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(move |path: &Path, _duration_us: i64| {
                rec.enter();
                let boom = path
                    .file_name()
                    .map(|n| n.to_string_lossy().contains("boom"))
                    .unwrap_or(false);
                rec.leave();
                if boom {
                    panic!("simulated decoder panic");
                }
                Ok(vec![3u8; 8])
            }),
        );
    }

    // Enough panicking jobs to exhaust the pool twice over if permits leaked.
    let mut sources: Vec<PathBuf> = (0..MAX_CONCURRENT_WAVEFORM_JOBS * 2)
        .map(|i| real_file(scratch.path(), &format!("boom{i}.bin"), 8 + i))
        .collect();
    let survivor = real_file(scratch.path(), "survivor.bin", 99);
    sources.push(survivor.clone());

    let rt = runtime();
    rt.block_on(async {
        for src in &sources {
            spawn_extraction(dir.clone(), src.clone(), 500_000);
        }
    });

    let key = waveform::cache::key_for(&survivor).expect("the survivor is cacheable");
    assert!(
        wait_until(Duration::from_secs(60), || waveform::cache::read(&dir, &key)
            .is_some()),
        "a job following {} panics still ran and cached its peaks",
        MAX_CONCURRENT_WAVEFORM_JOBS * 2
    );
    assert_eq!(
        rec.calls(),
        sources.len(),
        "every job entered the extraction step; none was swallowed by a poisoned permit"
    );
}

// ---------------------------------------------------------------------------
// Idempotence and gating
// ---------------------------------------------------------------------------

/// Re-importing the SAME unchanged file is a cache HIT, so the decode does not
/// run twice. Instrumented with a counter because "it was fast" is not a proof.
#[test]
fn a_second_import_of_the_same_unchanged_file_does_not_recompute() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(move |path: &Path, duration_us: i64| {
                rec.enter();
                let out = waveform::extract_peaks(path, duration_us, true).map_err(|e| e.to_string());
                rec.leave();
                out
            }),
        );
    }

    let path = fixture("speech_en.mp4");
    let first = import(&ctx, &path);
    let first_id = first[0].id.clone();
    assert!(
        wait_until(Duration::from_secs(120), || read_peaks(
            ctx.store(),
            &dir,
            &first_id
        )
        .is_some()),
        "the first import computed and cached peaks"
    );
    assert_eq!(rec.calls(), 1, "exactly one extraction for the first import");

    let second = import(&ctx, &path);
    let second_id = second[0].id.clone();
    assert_ne!(second_id, first_id, "a re-import mints a new item id");
    assert!(
        wait_until(Duration::from_secs(30), || read_peaks(
            ctx.store(),
            &dir,
            &second_id
        )
        .is_some()),
        "the second item resolves to the SAME cached envelope (same file, same key)"
    );
    // Give a stray second extraction every chance to show up before asserting.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        rec.calls(),
        1,
        "the second import hit the cache and never decoded again"
    );
}

/// A source with no audio stream never starts a job at all — the gate is
/// `has_audio` on the probe result, checked before the spawn.
#[test]
fn no_audio_media_never_spawns_a_job() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(move |_path: &Path, _duration_us: i64| {
                rec.enter();
                rec.leave();
                Ok(vec![1u8; 4])
            }),
        );
    }

    let items = import(&ctx, &fixture("testsrc_720p30_5s.mp4"));
    assert_eq!(items.len(), 1);
    assert!(!items[0].has_audio, "the control fixture really has no audio");

    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        rec.calls(),
        0,
        "an audio-less import must not spawn an extraction"
    );
    assert!(
        read_peaks(ctx.store(), &dir, &items[0].id).is_none(),
        "and it stays a miss forever, which the export reports as {{\"Ok\": null}}"
    );
}

/// D-20: a VIDEO clip that carries audio gets a waveform too. This is the
/// clause that makes the gate `has_audio` rather than `media_kind == Audio`.
#[test]
fn an_audio_bearing_video_clip_gets_a_job_too() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(move |path: &Path, duration_us: i64| {
                rec.enter();
                let out = waveform::extract_peaks(path, duration_us, true).map_err(|e| e.to_string());
                rec.leave();
                out
            }),
        );
    }

    let items = import(&ctx, &fixture("bars_720p30_5s.mp4"));
    assert_eq!(items[0].media_kind, rudis_core::MediaKind::Video);
    assert!(items[0].has_audio, "the fixture is a video WITH an audio track");

    assert!(
        wait_until(Duration::from_secs(120), || read_peaks(
            ctx.store(),
            &dir,
            &items[0].id
        )
        .is_some()),
        "an audio-bearing VIDEO clip gets peaks (D-20)"
    );
    assert_eq!(rec.calls(), 1);
}

// ---------------------------------------------------------------------------
// The property criterion 5 rests on
// ---------------------------------------------------------------------------

/// The read path can NEVER start work. 50 reads against an empty cache, zero
/// extractions. Pitfall 7's forbidden shape is unreachable, not merely avoided.
#[test]
fn read_peaks_never_triggers_computation() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(move |_path: &Path, _duration_us: i64| {
                rec.enter();
                rec.leave();
                Ok(vec![9u8; 4])
            }),
        );
    }

    // A REAL item in the store with a REAL path, and NOTHING in the cache: the
    // one situation in which a "helpful" read would be tempted to compute.
    let scratch = tempfile::tempdir().expect("scratch");
    let src = real_file(scratch.path(), "silent.bin", 64);
    let item = rudis_core::MediaBinItem {
        id: "media-read-only".to_string(),
        path: src.to_string_lossy().into_owned(),
        media_kind: rudis_core::MediaKind::Audio,
        duration_us: 1_000_000,
        width: 0,
        height: 0,
        fps: 0.0,
        is_vfr: false,
        rotation_degrees: 0,
        has_audio: true,
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

    for _ in 0..50 {
        assert!(
            read_peaks(ctx.store(), &dir, "media-read-only").is_none(),
            "an empty cache is a MISS, never a computation"
        );
    }
    assert_eq!(
        rec.calls(),
        0,
        "50 reads, ZERO extractions — the read path cannot reach the decoder"
    );

    // And the app-core entry the export calls agrees, without erroring.
    assert_eq!(
        run_get_waveform_peaks(&ctx, "media-read-only"),
        Ok(None),
        "a not-yet-computed item is Ok(None), never Err"
    );
    assert_eq!(rec.calls(), 0, "still zero after the export-level read");
}

/// An id that is not in the media bin is a MISS, not an error — the fourth of
/// D-21's four `{"Ok": null}` cases.
#[test]
fn read_peaks_for_an_unknown_media_id_is_none_not_an_error() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);

    assert!(read_peaks(ctx.store(), &dir, "media-does-not-exist").is_none());
    assert_eq!(run_get_waveform_peaks(&ctx, "media-does-not-exist"), Ok(None));
    // V5 / T-52-21: the id is looked up in the Store; it is never joined onto a
    // path, so a traversal payload is just another unknown id.
    for hostile in [
        "../../../Windows/System32/config/SAM",
        "C:\\Windows\\win.ini",
        "..",
        "",
    ] {
        assert_eq!(
            run_get_waveform_peaks(&ctx, hostile),
            Ok(None),
            "`{hostile}` is an ID that does not exist, not a path to read"
        );
    }
}

/// The payload the FFI export serializes: base64 of exactly `peak_count` raw
/// bytes, with the block size and sample rate the file was WRITTEN at.
#[test]
fn run_get_waveform_peaks_encodes_the_cached_bytes_as_base64() {
    use base64::Engine as _;

    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let scratch = tempfile::tempdir().expect("scratch");
    let src = real_file(scratch.path(), "payload.bin", 21);

    let item = rudis_core::MediaBinItem {
        id: "media-payload".to_string(),
        path: src.to_string_lossy().into_owned(),
        media_kind: rudis_core::MediaKind::Audio,
        duration_us: 1_000_000,
        width: 0,
        height: 0,
        fps: 0.0,
        is_vfr: false,
        rotation_degrees: 0,
        has_audio: true,
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

    let peaks: Vec<u8> = (0..=255u8).cycle().take(700).collect();
    let key = waveform::cache::key_for(&src).expect("cacheable");
    assert!(
        waveform::cache::write(&dir, &key, &peaks, waveform::PEAK_BLOCK_US, 48_000),
        "seed the cache directly — this gate is about the READ, not the job"
    );

    let payload = run_get_waveform_peaks(&ctx, "media-payload")
        .expect("a hit is never an Err")
        .expect("a hit is Some");
    assert_eq!(payload.block_us, waveform::PEAK_BLOCK_US);
    assert_eq!(payload.sample_rate, 48_000);
    assert_eq!(payload.peak_count as usize, peaks.len());

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload.peaks_b64.as_bytes())
        .expect("standard base64 with padding — what C#'s Convert.FromBase64String reads");
    assert_eq!(decoded, peaks, "the bytes survive the boundary unchanged");

    // The serialized envelope shape the C ABI writes.
    let envelope: Result<Option<PeaksPayload>, String> = Ok(Some(payload));
    let json = serde_json::to_value(&envelope).expect("serializes");
    assert!(json["Ok"]["peaks_b64"].is_string());
    assert_eq!(json["Ok"]["peak_count"], serde_json::json!(700));

    let miss: Result<Option<PeaksPayload>, String> = Ok(None);
    assert_eq!(
        serde_json::to_string(&miss).expect("serializes"),
        r#"{"Ok":null}"#,
        "every miss is exactly this envelope (D-21)"
    );
}
