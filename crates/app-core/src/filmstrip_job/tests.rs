//! Plan 53.2-04, Task 1's gates.
//!
//! # Why these are IN-CRATE tests and not `crates/app-core/tests/filmstrip_job.rs`
//!
//! The identical reason `waveform_job/tests.rs` records one module over: three
//! of the gates below drive a REAL import, which needs an
//! [`AppCtx`](crate::AppCtx), and this crate's only one is
//! `crate::test_support::TestAppCtx` — `#[cfg(test)]` in `lib.rs` and therefore
//! invisible to an integration test, which compiles as a separate crate against
//! the ordinary library. That is why `crates/app-core` has NO `tests/` directory
//! at all. Run these with `cargo test -p app-core filmstrip`.
//!
//! # The one stubbed step, and why it is not a mocked feature
//!
//! CLAUDE.md rule 1 forbids mocking a core editing operation, and nothing here
//! does. `spawn_returns_immediately_and_never_blocks_import` runs the REAL
//! `filmstrip::extract_strip` against a REAL 720p fixture and asserts on the
//! REAL decoded sheet that lands in the cache. What the seam in
//! [`super::test_hook`] replaces elsewhere is only the *scheduling
//! observability* of that call — a counter, a barrier, a panic — because "the
//! import returned BEFORE the frames were ready", "at most
//! [`MAX_CONCURRENT_FILMSTRIP_JOBS`] decoders ran at once" and "this file was
//! never decoded a second time" are claims about ORDER, CONCURRENCY and ABSENCE
//! that a real decode's timing cannot assert without being flaky.

use super::*;
use crate::test_support::{fixture, TestAppCtx};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Harness (the `waveform_job::tests` shapes, carried across by subject)
// ---------------------------------------------------------------------------

/// Multi-thread, not current-thread: [`super::spawn_extraction`] detaches a task
/// that then `spawn_blocking`s, and `concurrent_extraction_is_capped` is about
/// two of those overlapping.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .build()
        .expect("test runtime builds")
}

/// A REAL file on disk with `bytes` in it.
///
/// Not a convenience: `filmstrip::cache::key_for` canonicalizes and `stat`s its
/// argument and returns `None` for anything that is not a real file, so a job
/// spawned against an imaginary path would return before ever reaching the
/// extraction step — and every counting gate below would pass vacuously.
fn real_file(dir: &Path, name: &str, bytes: usize) -> PathBuf {
    std::fs::create_dir_all(dir).expect("scratch dir");
    let path = dir.join(name);
    std::fs::write(&path, vec![b'x'; bytes]).expect("write scratch file");
    path
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
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Records every extraction the job path performs, the high-water mark of how
/// many were inside it at the same instant, and which files they were for.
#[derive(Default)]
struct Recorder {
    calls: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

impl Recorder {
    fn enter(&self, path: &Path) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(path.file_name().unwrap_or_default().to_string_lossy().into_owned());
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

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// The filmstrip cache dir a `TestAppCtx`'s imports actually use — the same
/// value `import_one_path` resolves through `filmstrip_cache_dir(ctx)`.
fn ctx_dir(ctx: &TestAppCtx) -> PathBuf {
    filmstrip_dir(ctx).expect("test ctx resolves a cache dir")
}

/// Import one real path through the REAL UI import path.
fn import(ctx: &TestAppCtx, path: &str) -> Vec<rudis_core::MediaBinItem> {
    ctx.block_on(crate::import::run_import_media_ui(
        ctx,
        vec![path.to_string()],
    ))
    .expect("the real import path succeeds on a real fixture")
}

/// A `MediaBinItem` in the store pointing at a REAL file, registered WITHOUT an
/// import — the store knows the id and the strip cache has never heard of it.
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

/// Write one strip revision straight into the cache, bypassing extraction — the
/// gates below are about the READ and the short-circuit, not the decode.
fn seed_strip(dir: &Path, path: &Path, total: u32, completed: u32, fill: u8) -> Vec<u8> {
    let key = cache::key_for(path).expect("the seeded source is cacheable");
    let rows: Vec<u8> = (0..filmstrip::sheet_bytes_for(completed))
        .map(|i| fill.wrapping_add(i as u8))
        .collect();
    let header = cache::StripHeader {
        tile_w: filmstrip::TILE_W,
        tile_h: filmstrip::TILE_H,
        tiles_per_row: filmstrip::TILES_PER_ROW,
        total_tiles: total,
        completed_tiles: completed,
        interval_us: filmstrip::FILMSTRIP_MIN_INTERVAL_US,
        src_w: 1280,
        src_h: 720,
    };
    assert!(
        cache::write(dir, &key, &header, &rows),
        "the seed revision landed"
    );
    rows
}

/// One published revision's worth of plausible bytes for a synthetic source.
fn publish_one_tile(publish: &mut dyn FnMut(&[u8], u32) -> bool) {
    let rows = vec![0u8; filmstrip::sheet_bytes_for(1)];
    publish(&rows, 1);
}

// ---------------------------------------------------------------------------
// D-09 — the import path never waits for frames
// ---------------------------------------------------------------------------

/// The load-bearing ordering gate: the `MediaBinItem` is in the store and the
/// import call has RETURNED while the extraction is still running.
///
/// Deterministic by construction rather than by timing: the extraction is held
/// open on a gate the test releases, so "still `None` when import returned" is a
/// fact, not a race that usually wins. Once released, the REAL
/// `filmstrip::extract_strip` runs against the REAL 720p fixture and the
/// assertions are against its real decoded sheet.
#[test]
fn spawn_returns_immediately_and_never_blocks_import() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let released = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));

    {
        let released = Arc::clone(&released);
        let entered = Arc::clone(&entered);
        test_hook::install(
            &dir,
            Arc::new(
                move |path: &Path,
                      spec: StripSpec,
                      publish: &mut dyn FnMut(&[u8], u32) -> bool| {
                    entered.store(true, Ordering::SeqCst);
                    while !released.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    // REAL extraction, real decode, real pixels.
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
                },
            ),
        );
    }

    let items = import(&ctx, &fixture("bars_720p30_5s.mp4"));
    assert_eq!(items.len(), 1, "the video fixture imported");
    let media_id = items[0].id.clone();
    assert_eq!(items[0].media_kind, rudis_core::MediaKind::Video);

    // (a) The item is ALREADY in the backend-owned store...
    assert!(
        ctx.store()
            .lock()
            .expect("store")
            .media_item(&media_id)
            .is_some(),
        "the MediaBinItem lands in the bin as part of the import call"
    );
    // (b) ...while the frames are provably NOT ready.
    assert!(
        read_strip(ctx.store(), &dir, &media_id).is_none(),
        "import must NOT block on extraction (D-09)"
    );
    assert!(
        wait_until(Duration::from_secs(20), || entered.load(Ordering::SeqCst)),
        "the background job really did start"
    );
    assert!(
        read_strip(ctx.store(), &dir, &media_id).is_none(),
        "still not ready while the decode is held open — the ORDER is the claim"
    );

    // Now let it finish, and prove the real sheet arrives.
    released.store(true, Ordering::SeqCst);
    assert!(
        wait_until(Duration::from_secs(300), || read_strip(
            ctx.store(),
            &dir,
            &media_id
        )
        .is_some()),
        "frames arrive asynchronously after the import returned"
    );

    let cached = read_strip(ctx.store(), &dir, &media_id).expect("a hit");
    let (total, interval) = filmstrip::plan_tiles(items[0].duration_us);
    assert_eq!(cached.total_tiles, total, "the grid matches plan_tiles");
    assert_eq!(cached.interval_us, interval);
    assert_eq!(cached.tile_w, filmstrip::TILE_W);
    assert_eq!(cached.tile_h, filmstrip::TILE_H);
    assert_eq!(cached.tiles_per_row, filmstrip::TILES_PER_ROW);
    assert!(
        cached.is_complete(),
        "the finished job publishes a COMPLETE strip ({}/{})",
        cached.completed_tiles,
        cached.total_tiles
    );
    assert_eq!(
        cached.rgba.len(),
        filmstrip::sheet_bytes_for(cached.completed_tiles),
        "the payload is whole sheet rows"
    );
    assert!(
        cached.rgba.iter().any(|&b| b != 0),
        "real colour bars are not an all-zero sheet"
    );
}

// ---------------------------------------------------------------------------
// T-53.2-14 — the DoS cap is asserted, not assumed
// ---------------------------------------------------------------------------

/// [`MAX_CONCURRENT_FILMSTRIP_JOBS`] + 5 jobs, never more than the cap alive at
/// once. Compared against the CONSTANT rather than a literal, so Task 3's
/// measurement re-tunes the gate instead of breaking it.
#[test]
fn concurrent_extraction_is_capped() {
    let jobs = MAX_CONCURRENT_FILMSTRIP_JOBS + 5;
    let scratch = tempfile::tempdir().expect("scratch");
    let dir = scratch.path().join("filmstrips");
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(
                move |path: &Path, _spec: StripSpec, _p: &mut dyn FnMut(&[u8], u32) -> bool| {
                    rec.enter(path);
                    // Held long enough that any second permit-holder provably
                    // overlaps this one.
                    std::thread::sleep(Duration::from_millis(200));
                    rec.leave();
                    Ok(())
                },
            ),
        );
    }

    let sources: Vec<PathBuf> = (0..jobs)
        .map(|i| real_file(scratch.path(), &format!("src{i}.bin"), 32 + i))
        .collect();

    let rt = runtime();
    rt.block_on(async {
        for src in &sources {
            spawn_extraction(dir.clone(), src.clone(), 1_000_000, 1280, 720, 0, 33_333);
        }
    });

    assert!(
        wait_until(Duration::from_secs(120), || rec.calls() >= jobs),
        "all {jobs} jobs ran (saw {})",
        rec.calls()
    );
    assert_eq!(rec.calls(), jobs, "every job ran exactly once");
    assert_eq!(
        rec.max_in_flight(),
        MAX_CONCURRENT_FILMSTRIP_JOBS,
        "a batch import must never start more than MAX_CONCURRENT_FILMSTRIP_JOBS \
         decoders at once (T-53.2-14); observed {}",
        rec.max_in_flight()
    );
}

/// A panicking job is caught, and the permit it held is released — the very next
/// job still runs. A leaked permit would silently zero the pool (the cap is 1)
/// for the rest of the process's life.
#[test]
fn an_extraction_panic_does_not_poison_the_import_path() {
    let scratch = tempfile::tempdir().expect("scratch");
    let dir = scratch.path().join("filmstrips");
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(
                move |path: &Path,
                      _spec: StripSpec,
                      publish: &mut dyn FnMut(&[u8], u32) -> bool| {
                    rec.enter(path);
                    let boom = path
                        .file_name()
                        .map(|n| n.to_string_lossy().contains("boom"))
                        .unwrap_or(false);
                    rec.leave();
                    if boom {
                        panic!("simulated decoder panic");
                    }
                    publish_one_tile(publish);
                    Ok(())
                },
            ),
        );
    }

    // Enough panicking jobs to exhaust the pool many times over if permits leaked.
    let mut sources: Vec<PathBuf> = (0..MAX_CONCURRENT_FILMSTRIP_JOBS * 2 + 2)
        .map(|i| real_file(scratch.path(), &format!("boom{i}.bin"), 8 + i))
        .collect();
    let survivor = real_file(scratch.path(), "survivor.bin", 99);
    sources.push(survivor.clone());

    let rt = runtime();
    rt.block_on(async {
        for src in &sources {
            spawn_extraction(dir.clone(), src.clone(), 500_000, 1280, 720, 0, 33_333);
        }
    });

    let key = cache::key_for(&survivor).expect("the survivor is cacheable");
    assert!(
        wait_until(Duration::from_secs(120), || cache::read(&dir, &key).is_some()),
        "a job following {} panics still ran and cached its strip",
        sources.len() - 1
    );
    assert_eq!(
        rec.calls(),
        sources.len(),
        "every job entered the extraction step; none was swallowed by a poisoned permit"
    );
}

// ---------------------------------------------------------------------------
// D-14's resume rule: complete short-circuits, PARTIAL does not
// ---------------------------------------------------------------------------

/// A COMPLETE cache hit short-circuits before any decode. A PARTIAL entry does
/// NOT — the previous job died mid-fill, so the strip is re-extracted from tile
/// 0 (which overwrites correctly, because a partial revision carries no tiles
/// the new pass will not also produce).
#[test]
fn an_unchanged_file_is_not_re_extracted() {
    let scratch = tempfile::tempdir().expect("scratch");
    let dir = scratch.path().join("filmstrips");
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(
                move |path: &Path,
                      _spec: StripSpec,
                      publish: &mut dyn FnMut(&[u8], u32) -> bool| {
                    rec.enter(path);
                    publish_one_tile(publish);
                    rec.leave();
                    Ok(())
                },
            ),
        );
    }

    let done = real_file(scratch.path(), "already-done.bin", 41);
    let partial = real_file(scratch.path(), "half-done.bin", 42);
    let fresh = real_file(scratch.path(), "never-seen.bin", 43);

    // (a) COMPLETE: 1 of 1 tiles valid.
    seed_strip(&dir, &done, 1, 1, 0x10);
    // (b) PARTIAL: 1 of 5 tiles valid — a job that died mid-fill.
    seed_strip(&dir, &partial, 5, 1, 0x20);

    let rt = runtime();
    rt.block_on(async {
        spawn_extraction(dir.clone(), done.clone(), 500_000, 1280, 720, 0, 33_333);
        spawn_extraction(dir.clone(), partial.clone(), 500_000, 1280, 720, 0, 33_333);
        spawn_extraction(dir.clone(), fresh.clone(), 500_000, 1280, 720, 0, 33_333);
    });

    // The un-cached control is the liveness proof: once IT has run, the pool has
    // drained and any extraction the other two were going to do has happened.
    assert!(
        wait_until(Duration::from_secs(120), || rec
            .seen()
            .iter()
            .any(|n| n == "never-seen.bin")),
        "the un-cached control extracted (saw {:?})",
        rec.seen()
    );
    std::thread::sleep(Duration::from_millis(300));

    let seen = rec.seen();
    assert!(
        !seen.iter().any(|n| n == "already-done.bin"),
        "a COMPLETE cache hit short-circuits BEFORE any decode (saw {seen:?})"
    );
    assert!(
        seen.iter().any(|n| n == "half-done.bin"),
        "a PARTIAL entry must NOT short-circuit — the D-14 resume rule (saw {seen:?})"
    );
}

// ---------------------------------------------------------------------------
// D-15 / T-53.2-13 — the read path
// ---------------------------------------------------------------------------

/// An id that is not in the media bin is a MISS, not an error — and a traversal
/// payload is just another unknown id, because `media_id` is looked up in the
/// Store and never joined onto a path (T-53.2-13, the T-52-21 discipline).
#[test]
fn read_strip_for_an_unknown_media_id_is_none_not_an_error() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);

    assert!(read_strip(ctx.store(), &dir, "media-does-not-exist").is_none());
    assert_eq!(run_get_filmstrip_strip(&ctx, "media-does-not-exist"), Ok(None));

    for hostile in [
        "../../../Windows/System32/config/SAM",
        "C:\\Windows\\win.ini",
        "..",
        "",
    ] {
        assert_eq!(
            run_get_filmstrip_strip(&ctx, hostile),
            Ok(None),
            "`{hostile}` is an ID that does not exist, not a path to read"
        );
    }
}

/// The read path can NEVER start work. 50 reads against an empty cache, zero
/// extractions — the forbidden shape is unreachable, not merely avoided.
#[test]
fn read_strip_never_triggers_computation() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(
                move |path: &Path, _spec: StripSpec, _p: &mut dyn FnMut(&[u8], u32) -> bool| {
                    rec.enter(path);
                    rec.leave();
                    Ok(())
                },
            ),
        );
    }

    let scratch = tempfile::tempdir().expect("scratch");
    let src = real_file(scratch.path(), "cold.bin", 64);
    register(&ctx, "media-read-only", &src);

    for _ in 0..50 {
        assert!(
            read_strip(ctx.store(), &dir, "media-read-only").is_none(),
            "an empty cache is a MISS, never a computation"
        );
    }
    assert_eq!(
        rec.calls(),
        0,
        "50 reads, ZERO extractions — the read path cannot reach the decoder"
    );
    assert_eq!(
        run_get_filmstrip_strip(&ctx, "media-read-only"),
        Ok(None),
        "a not-yet-computed item is Ok(None), never Err"
    );
    assert_eq!(rec.calls(), 0, "still zero after the export-level read");
}

/// D-14 crosses the ABI: the payload distinguishes a PARTIAL strip from a
/// COMPLETE one through `completed_tiles` / `total_tiles`, and the bytes survive
/// the base64 boundary unchanged.
#[test]
fn run_get_filmstrip_strip_carries_the_three_state_pair() {
    use base64::Engine as _;

    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let scratch = tempfile::tempdir().expect("scratch");
    let src = real_file(scratch.path(), "payload.bin", 21);
    register(&ctx, "media-payload", &src);

    // PARTIAL: 1 of 5 tiles.
    let rows = seed_strip(&dir, &src, 5, 1, 0x37);

    let payload = run_get_filmstrip_strip(&ctx, "media-payload")
        .expect("a hit is never an Err")
        .expect("a hit is Some");
    assert_eq!(payload.total_tiles, 5);
    assert_eq!(payload.completed_tiles, 1);
    assert!(
        payload.completed_tiles < payload.total_tiles,
        "this is D-14's PARTIAL state, and it is visible on the wire"
    );
    assert_eq!(payload.tile_w, filmstrip::TILE_W);
    assert_eq!(payload.tile_h, filmstrip::TILE_H);
    assert_eq!(payload.tiles_per_row, filmstrip::TILES_PER_ROW);
    assert_eq!(payload.interval_us, filmstrip::FILMSTRIP_MIN_INTERVAL_US);
    assert_eq!(
        payload.sheet_w,
        filmstrip::TILES_PER_ROW * filmstrip::TILE_W
    );
    assert_eq!(payload.sheet_h, filmstrip::TILE_H, "one whole sheet row");

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload.strip_b64.as_bytes())
        .expect("standard base64 with padding — what C#'s Convert.FromBase64String reads");
    assert_eq!(decoded, rows, "the bytes survive the boundary unchanged");
    assert_eq!(
        decoded.len(),
        payload.sheet_w as usize * payload.sheet_h as usize * 4,
        "the payload is exactly the declared sheet"
    );

    // The serialized envelope shape the C ABI writes.
    let envelope: Result<Option<StripPayload>, String> = Ok(Some(payload));
    let json = serde_json::to_value(&envelope).expect("serializes");
    assert!(json["Ok"]["strip_b64"].is_string());
    assert_eq!(json["Ok"]["completed_tiles"], serde_json::json!(1));
    assert_eq!(json["Ok"]["total_tiles"], serde_json::json!(5));

    let miss: Result<Option<StripPayload>, String> = Ok(None);
    assert_eq!(
        serde_json::to_string(&miss).expect("serializes"),
        r#"{"Ok":null}"#,
        "every miss is exactly this envelope (D-15)"
    );
}

// ---------------------------------------------------------------------------
// D-15's gate at the import site
// ---------------------------------------------------------------------------

/// Audio-only media and still images spawn NO filmstrip job — their placeholder
/// is TERMINAL (a still's poster already IS its filmstrip). A video does.
#[test]
fn only_video_media_spawns_extraction() {
    let ctx = TestAppCtx::new();
    let dir = ctx_dir(&ctx);
    let rec = Arc::new(Recorder::default());

    {
        let rec = Arc::clone(&rec);
        test_hook::install(
            &dir,
            Arc::new(
                move |path: &Path, _spec: StripSpec, _p: &mut dyn FnMut(&[u8], u32) -> bool| {
                    rec.enter(path);
                    rec.leave();
                    Ok(())
                },
            ),
        );
    }

    for (name, kind) in [
        ("tone.m4a", rudis_core::MediaKind::Audio),
        ("still.png", rudis_core::MediaKind::Image),
    ] {
        let items = import(&ctx, &fixture(name));
        assert_eq!(items.len(), 1, "{name} imported");
        assert_eq!(items[0].media_kind, kind, "{name} is really {kind:?}");
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        rec.calls(),
        0,
        "audio-only and still-image imports must not spawn an extraction (D-15); saw {:?}",
        rec.seen()
    );

    // The control: a VIDEO import on the SAME ctx does spawn one, so the zero
    // above is the GATE and not an inert hook.
    let items = import(&ctx, &fixture("bars_720p30_5s.mp4"));
    assert_eq!(items[0].media_kind, rudis_core::MediaKind::Video);
    assert!(
        wait_until(Duration::from_secs(60), || rec.calls() == 1),
        "video media DOES spawn extraction (saw {:?})",
        rec.seen()
    );
    assert!(
        read_strip(ctx.store(), &dir, &items[0].id).is_none(),
        "a hook that publishes nothing leaves the cache a miss — which the export \
         reports as {{\"Ok\": null}}"
    );
}
