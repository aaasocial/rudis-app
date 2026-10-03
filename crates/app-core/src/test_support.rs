//! A non-`tauri` fake [`AppCtx`] for tests.
//!
//! `src-tauri/src/lib.rs` currently carries ~13 near-duplicated
//! `build_app()` / `build_app_isolated(identifier)` helpers across its
//! `#[cfg(test)]` modules, each one spinning up a `tauri::test::MockRuntime`
//! app purely to get a managed `Store` plus a private `app_data_dir`. **This
//! crate must never depend on `tauri`** — that is the whole point of the
//! extraction — so every test migrating in with its function needs a
//! tauri-free equivalent. This is that ONE helper.
//!
//! It mirrors `build_app_isolated`'s actual reason for existing: the harness
//! runs tests in parallel, so any test asserting on files under `app_data_dir`
//! must not share that directory with a sibling. Here the isolation is
//! per-INSTANCE (a fresh `TempDir` per `TestAppCtx`) rather than per-identifier
//! string, which is strictly stronger and needs no unique name to be invented
//! at each call site.

use crate::project_store::ActiveProjectMeta;
use crate::{AppCtx, SharedStore};
use std::sync::Mutex;

/// A tauri-free [`AppCtx`] whose directories are per-instance temp dirs and
/// whose `project:changed` emissions land in an in-memory `Vec` instead of a
/// Tauri event bus.
pub struct TestAppCtx {
    store: SharedStore,
    /// Held for its `Drop` (the temp dir is removed when this ctx dies) AND
    /// read by `app_data_dir()`. Underscore-prefixed to signal that the
    /// binding's lifetime, not just its value, is load-bearing.
    _data_dir: tempfile::TempDir,
    _cache_dir: tempfile::TempDir,
    /// Plan 45-08: the stand-in for the app's READ-ONLY BUNDLED RESOURCE root
    /// ([`AppCtx::resolve_resource`]). Per-instance and EMPTY, which is the
    /// faithful analogue of a headless `tauri::test::MockRuntime` app: nothing
    /// bundles resources into a test binary, so a resource lookup resolves to a
    /// real, absent path rather than erroring. A test that wants real bundled
    /// assets reads them directly (see `overlay::overlay_library`'s
    /// `real_bundled_dir`), exactly as it did before the move.
    _resource_dir: tempfile::TempDir,
    /// In-memory sink standing in for `app.emit(PROJECT_CHANGED_EVENT, ..)`.
    emitted: Mutex<Vec<(rudis_core::Patch, u64, u64)>>,
    /// Plan 45-10: the in-memory twin of the above for
    /// `app.emit(EXPORT_PROGRESS_EVENT, pct)`. `Arc` rather than a plain field
    /// because [`AppCtx::export_progress_sink`] hands out an OWNED
    /// `Send + 'static` closure the encoder keeps — it cannot borrow `self`.
    progress: std::sync::Arc<Mutex<Vec<f64>>>,
    /// Plan 54.1-03: the in-memory twin of the above for the job-lifecycle
    /// surface (`gen:job` / `gen:progress`). `Arc` for the same reason
    /// [`TestAppCtx::export_progress`]'s is — [`AppCtx::gen_event_sink`] hands
    /// out an OWNED `Send + Sync + 'static` closure the spawned poll task keeps,
    /// so it cannot borrow `self`.
    gen_events: std::sync::Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    /// Plan 45-06: the tauri-free stand-in for the ONE
    /// `.manage(ActiveProjectMeta::default())` `src-tauri` registers at setup.
    /// Owned per-instance, matching `_data_dir`'s per-instance isolation — two
    /// `TestAppCtx`es never share an active-project pointer any more than they
    /// share a `projects/` directory.
    active_project_meta: ActiveProjectMeta,
    /// Plan 45-12: the tauri-free stand-in for the ONE
    /// `.manage(Mutex::new(AgentSession::default()))` `src-tauri` registers at
    /// setup. Per-instance, matching `_data_dir`'s isolation — two
    /// `TestAppCtx`es never share a conversation any more than they share a
    /// `projects/` directory.
    agent_session: Mutex<crate::AgentSession>,
    /// Plan 45-07: [`AppCtx::block_on`]'s tauri-free backing. A REAL
    /// multi-threaded Tokio runtime, not a bare future poller, because the code
    /// under test (`import_one_path`) calls `tokio::task::spawn_blocking`, which
    /// panics without an entered runtime — the same property
    /// `tauri::async_runtime::block_on` provides in production. Built lazily so
    /// the many ctxs that never block on anything pay nothing for it.
    runtime: std::sync::OnceLock<tokio::runtime::Runtime>,
}

impl TestAppCtx {
    /// A ctx over an empty `rudis_core::Store::default()`.
    pub fn new() -> Self {
        Self::with_store(Mutex::new(rudis_core::Store::default()))
    }

    /// A ctx over a caller-seeded store — callers build a `Project` the same
    /// way the existing `#[cfg(test)]` modules do (`serde_json::from_value(
    /// json!({..}))`) and load it before handing the store over.
    pub fn with_store(store: SharedStore) -> Self {
        Self {
            store,
            _data_dir: tempfile::tempdir().expect("tempdir for TestAppCtx app_data_dir"),
            _cache_dir: tempfile::tempdir().expect("tempdir for TestAppCtx app_cache_dir"),
            _resource_dir: tempfile::tempdir().expect("tempdir for TestAppCtx resource root"),
            emitted: Mutex::new(Vec::new()),
            progress: std::sync::Arc::new(Mutex::new(Vec::new())),
            gen_events: std::sync::Arc::new(Mutex::new(Vec::new())),
            active_project_meta: ActiveProjectMeta::default(),
            agent_session: Mutex::new(crate::AgentSession::default()),
            runtime: std::sync::OnceLock::new(),
        }
    }

    /// Every `(patch, base_seq, seq)` passed to [`AppCtx::emit_patch`], in
    /// order. Migrated tests that used to assert on the `project:changed`
    /// payload via `app.listen_any(PROJECT_CHANGED_EVENT, ..)` read this
    /// instead.
    pub fn emitted_patches(&self) -> Vec<(rudis_core::Patch, u64, u64)> {
        self.emitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Every percentage pushed through a sink handed out by
    /// [`AppCtx::export_progress_sink`], in order — the twin of
    /// [`TestAppCtx::emitted_patches`] for the `export:progress` surface
    /// (plan 45-10).
    pub fn export_progress(&self) -> Vec<f64> {
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Every `(event name, payload)` pushed through a sink handed out by
    /// [`AppCtx::gen_event_sink`], in order — the job-lifecycle twin of
    /// [`TestAppCtx::export_progress`] (plan 54.1-03).
    ///
    /// Read by tests that assert on what the HOST was told about a job. It is
    /// **not** a return channel: nothing in `app-core` reads this to learn an
    /// outcome (54.1-RESEARCH Pitfall 1) — outcomes ride
    /// `poll_job_until_terminal`'s return value.
    pub fn gen_events(&self) -> Vec<(String, serde_json::Value)> {
        self.gen_events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl Default for TestAppCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// Absolute path to a shared REAL media fixture in the repo's `test-media/`
/// directory. The twin of `rudis_app_lib`'s `test_support::fixture`, carried in
/// by plan 45-05 with the four inspect/transcript gates that call it.
///
/// The one difference is the relative hop: `src-tauri`'s copy joins
/// `../test-media` off `src-tauri/`; this one joins `../../test-media` off
/// `crates/app-core/`. Both resolve to the SAME directory — the fixtures did
/// not move, only the caller did.
pub fn fixture(name: &str) -> String {
    prefer_bundled_ffmpeg();
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-media")
        .join(name)
        .canonicalize()
        .unwrap_or_else(|e| panic!("fixture {name} must exist: {e}"))
        .to_string_lossy()
        .into_owned()
}

/// Point [`engine::locate`] at the repo's BUNDLED sidecar — `runtime/binaries`
/// first, then the CI dev build — the same order and the same hook point every
/// other ffmpeg-touching test file in this tree uses
/// (`crates/ffi/tests/contract_media.rs` hangs it off its own `fixture()`;
/// `crates/engine/tests/{audio_render,audio_retime}.rs` and
/// `crates/waveform/tests/extract.rs` do the equivalent).
///
/// # Why this is here, and what it MEASURED (2026-08-08)
///
/// `engine::locate()` resolves `RUDIS_FFMPEG_DIR` -> a sidecar beside the current
/// exe -> **PATH**, and a cargo test binary lives in `target/debug/deps/` where
/// nothing stages an `ffmpeg.exe`. So with no pin, every real-media test in this
/// crate silently scored whatever ffmpeg the developer happened to have installed
/// — on the machine this was found on, a 2023 GPL `6.1-full_build-www.gyan.dev`,
/// which CLAUDE.md rule 6 forbids scoring anything against.
///
/// It is not theoretical, in either direction:
///
/// * **A gate was VACUOUS without it.** `export::export_gate::an_untrimmed_aac_
///   clip_exports_its_audio_in_sync_not_40ms_early` PASSES with its defect
///   deliberately reintroduced when unpinned, and FAILS (onset 850 ms vs 890 ms)
///   when pinned — because the PATH build does not exhibit the AAC input-seek head
///   loss that the shipped one does. Debug session
///   `waveform-aac-priming-trim-short`.
/// * **Two tests were FLAKY because of it.** `cargo test -p app-core --lib --
///   --test-threads=1` measured **149 passed / 2 failed** at HEAD and **152 passed
///   / 0 failed** pinned — `render_cache_job_shared_admission` and
///   `render_cache_job_a_commit_clears_the_budgets` both stop failing. That is the
///   same PATH-`h264_nvenc` teardown crash already diagnosed in
///   `.planning/debug/rendercache-nvenc-parity-crash.md`, reaching one crate
///   further than that session knew.
///
/// It PREFERS rather than asserts, and it does not panic when nothing is found: a
/// checkout with no fetched sidecar must still run this crate's suite, where PATH
/// is the only option. It prints the resolved binary once, so every run's
/// evidence names its instrument.
///
/// Hung off `fixture()` deliberately: resolving a `test-media` path is the one
/// thing every real-media test in this crate does before it spawns anything, and
/// a pin a test can forget is a pin some test will forget.
fn prefer_bundled_ffmpeg() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("RUDIS_FFMPEG_DIR").is_none() {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf());
            if let Some(root) = root {
                for cand in [
                    root.join("runtime/binaries"),
                    root.join("crates/engine/ffmpeg-dev/bin"),
                ] {
                    if cand.is_dir() {
                        std::env::set_var("RUDIS_FFMPEG_DIR", &cand);
                        break;
                    }
                }
            }
        }
        if let Ok(bins) = engine::locate() {
            println!("APPCORE-SIDECAR resolved={}", bins.ffmpeg.display());
        }
    });
}

/// Mean absolute difference per channel between equal-length RGBA buffers.
/// Carried verbatim from `rudis_app_lib`'s `test_support` by plan 45-05 — the
/// migrated gates frame-diff against independently-built composites with it, and
/// the numeric tolerances they assert are only comparable if the metric is
/// byte-identical to the one they used before the move.
pub fn mad(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "buffers must be the same size to diff");
    let total: u64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i64 - *y as i64).unsigned_abs())
        .sum();
    total as f64 / a.len() as f64
}

impl AppCtx for TestAppCtx {
    fn store(&self) -> &SharedStore {
        &self.store
    }

    fn app_data_dir(&self) -> Result<std::path::PathBuf, String> {
        Ok(self._data_dir.path().to_path_buf())
    }

    fn app_cache_dir(&self) -> Result<std::path::PathBuf, String> {
        Ok(self._cache_dir.path().to_path_buf())
    }

    fn resolve_resource(&self, path: &str) -> Result<std::path::PathBuf, String> {
        Ok(self._resource_dir.path().join(path))
    }

    fn emit_patch(
        &self,
        patch: &rudis_core::Patch,
        base_seq: u64,
        seq: u64,
    ) -> Result<(), String> {
        self.emitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((patch.clone(), base_seq, seq));
        Ok(())
    }

    fn active_project_meta(&self) -> &ActiveProjectMeta {
        &self.active_project_meta
    }

    fn agent_session(&self) -> &Mutex<crate::AgentSession> {
        &self.agent_session
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.runtime
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .build()
                    .expect("TestAppCtx tokio runtime builds")
            })
            .block_on(fut)
    }

    fn export_progress_sink(&self) -> engine::ProgressFn {
        // The tauri-free twin of `TauriAppCtx`'s `app.clone()` + `app.emit(
        // EXPORT_PROGRESS_EVENT, pct)` closure: an owned handle to the same
        // in-memory recorder `export_progress()` reads back.
        let sink = std::sync::Arc::clone(&self.progress);
        Box::new(move |pct| {
            sink.lock().unwrap_or_else(|p| p.into_inner()).push(pct);
        })
    }

    fn gen_event_sink(&self) -> crate::generation_host::GenEventSink {
        // The tauri-free twin of `TauriAppCtx`'s `app.clone()` +
        // `app.emit(name, payload)` closure, and of `FfiAppCtx`'s ring push:
        // an owned handle to the same in-memory recorder `gen_events()` reads
        // back.
        let sink = std::sync::Arc::clone(&self.gen_events);
        std::sync::Arc::new(move |name: &'static str, payload: serde_json::Value| {
            sink.lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((name.to_string(), payload));
        })
    }

    fn run_blocking<T, F>(
        &self,
        f: F,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // A bare `#[test]` enters NO async runtime, so `tokio::task::
        // spawn_blocking` would panic here — the same asymmetry
        // `AppCtx::run_blocking`'s doc records as the reason the primitive
        // stays host-side at all. A plain OS thread + join reproduces the
        // property that matters (the closure runs OFF the caller's thread and
        // its panic surfaces as an `Err` rather than unwinding the caller)
        // with no runtime requirement. Captures nothing borrowed, so the
        // future is trivially `Send + 'static`.
        Box::pin(async move {
            std::thread::spawn(f)
                .join()
                .map_err(|_| "blocking task panicked".to_string())
        })
    }
}

// ---------------------------------------------------------------------------
// ONE GPU device at a time, across this whole crate's lib test binary.
// ---------------------------------------------------------------------------

/// Serializes every test in this crate that causes a wgpu device to be created.
///
/// This module is `#[cfg(test)]` (`lib.rs:173`), so none of this exists in a
/// production build.
///
/// # Why, measured
///
/// `cargo test -p app-core --lib` aborted mid-run with `0xc0000374
/// STATUS_HEAP_CORRUPTION` while `--test-threads=1` was 97/97 green — logged as
/// Phase 57 `deferred-items.md § D-1a`. It is the same defect that was fixed in
/// `crates/timeline-render/tests/abi_validation.rs`
/// (`.planning/debug/resolved/abi-validation-heap-corruption.md`): a lib harness
/// running device-creating tests on as many threads as the machine has cores,
/// so N D3D12 devices are created AND destroyed concurrently, and the
/// driver/debug-layer teardown path faults after the harness has already printed
/// `test result: ok`. Peak GPU memory for this binary, measured with `nvidia-smi`
/// against a settled 1619 MiB idle baseline on the RTX 3070 this project targets:
///
/// | `--test-threads` | peak above baseline |
/// |---|---|
/// | 1 | 597 MiB |
/// | default (16) | **2297 MiB** |
///
/// The residual 597 MiB is one device's honest working set and is not something
/// a guard can remove; the other ~1700 MiB was purely the harness's thread count.
///
/// # Why this is a plain `let _gpu = gpu_lease();` and NOT a value-returning fixture
///
/// `abi_validation.rs` hands the lease back FROM its `headless()` fixture, so a
/// device cannot be obtained without one. That shape does not work here, and the
/// reason is worth writing down rather than rediscovering:
///
/// **These tests create devices through MORE THAN ONE path each.** A parity twin
/// such as `export::parity_twins::multilayer_export_matches_offscreen_composite`
/// runs the production `export(..)` path (which builds a compositor inside
/// `run_export_blocking`) AND then builds a second, independent offscreen
/// `engine::Compositor` to compare against — that is the entire point of a parity
/// test. `inspect_wiring_gate::inspect_timeline_returns_text_then_image_within_bounded_lossy_tolerance`
/// does the same through `handle_inspect_timeline` plus `ground_truth_rgba`.
/// [`GPU`] is a plain non-reentrant `Mutex`, so a fixture that took the lock at
/// each construction site would DEADLOCK the moment a test used two of them.
/// Exactly one lease, taken at the top of the test, is the only correct shape.
///
/// # How to tell whether a test is missing its lease
///
/// Do not read the call graph — measure. Run the binary at `--test-threads=1` and
/// at the default, sampling `nvidia-smi --query-gpu=memory.used`. If every
/// device-creating test holds a lease the two peaks are the SAME, because only
/// one device is ever live. A default-threads peak materially above the
/// single-thread peak means some test reaches a device without a lease.
static GPU: Mutex<()> = Mutex::new(());

/// Held for the whole of a test that will cause a wgpu device to be created.
///
/// Poison is recovered rather than propagated: a test that fails while holding
/// this must report ITS OWN assertion, not a poison panic in the next twenty.
pub struct GpuLease(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

/// Take the crate-wide GPU lease. Call ONCE, at the top of the test — see the
/// deadlock note on [`GPU`].
pub fn gpu_lease() -> GpuLease {
    GpuLease(GPU.lock().unwrap_or_else(|p| p.into_inner()))
}

/// Apply ONE structural edit through the real `dispatch_command_inner` funnel —
/// for tests that need "a genuine edit happened", not any particular edit.
///
/// Appending an empty video track needs no media, applies to any store, and is
/// admitted by `preview::patch_touches_preview`, so it is the cheapest thing
/// that is unambiguously an edit.
///
/// It lives HERE rather than in the module that wants it because
/// `render_cache_job`'s D-26 guard
/// (`render_cache_job_never_names_a_mutation_type`) scans that whole file — its
/// test half included, deliberately, since a text scan cannot tell a test from a
/// shipped line. Spelling a mutation type there to prove a wiring would blunt
/// the rule it is proving. One shared helper is a much cheaper price than a
/// weakened scan.
pub fn dispatch_a_structural_edit<C: AppCtx>(ctx: &C) -> Result<rudis_core::Patch, String> {
    crate::dispatch::dispatch_command_inner(
        ctx,
        rudis_core::Command::AddTrack {
            kind: rudis_core::TrackKind::Video,
        },
    )
}

/// Seed `store` with a REAL, **multi-layer**, cacheable video arrangement: one
/// clip per entry of `sources`, each placed WHOLE at timeline zero on its own
/// video lane and running `span_us` long, over a `MediaBinItem` PROBED from the
/// actual file.
///
/// `sources` must hold **at least two** paths, and that is load-bearing rather
/// than fussy. `preview::resolve_multilayer` answers `None` for a single-clip
/// arrangement — the live path serves that one without compositing at all — and
/// 59-07's segment writer turns that answer into `NotCacheable`, a PERMANENT
/// refusal the render-cache scheduler is right never to re-enqueue. A one-clip
/// fixture therefore proves nothing about a scheduler except that its fixture
/// was wrong. Fix the fixture, never the funnel.
///
/// Pass paths from [`fixture`], which pins `RUDIS_FFMPEG_DIR` at the BUNDLED
/// LGPL sidecar as its first act (CLAUDE.md rule 6) — probing and later
/// decoding a fixture through whatever `ffmpeg` happens to be on `PATH` has
/// MEASURABLY falsified this crate's real-media gates before.
///
/// Identity transform, full opacity, no crop, no retime: the callers of this
/// helper measure schedulers and pipelines, not the effect stack.
///
/// It lives HERE, beside [`dispatch_a_structural_edit`], for that function's
/// own recorded reason: `render_cache_job`'s D-26 guard
/// (`render_cache_job_never_names_a_mutation_type`) scans that whole file — its
/// test half included, deliberately, since a text scan cannot tell a test line
/// from a shipped one. Spelling a mutation type there to build a fixture would
/// blunt the rule instead of passing it.
pub fn seed_layered_video_arrangement(
    store: &mut rudis_core::Store,
    sources: &[String],
    span_us: i64,
) {
    assert!(
        sources.len() >= 2,
        "a single-clip arrangement is not a multi-layer one, and the segment \
         writer refuses it PERMANENTLY — see this function's own doc"
    );
    // `rudis_core::Store::default()` opens with `[Video, Audio]`, and each
    // `AddTrack` INSERTS its video lane at index 0, so N sources need N-1 more
    // and lane 0 ends up the TOP compositing layer.
    for _ in 1..sources.len() {
        store
            .dispatch(rudis_core::Command::AddTrack {
                kind: rudis_core::TrackKind::Video,
            })
            .expect("add a video track");
    }
    for (lane, path) in sources.iter().enumerate() {
        let info = engine::probe(std::path::Path::new(path))
            .unwrap_or_else(|e| panic!("ffprobe must read the fixture {path}: {e}"));
        let media_id = format!("m-layer-{lane}");
        store
            .dispatch(rudis_core::Command::AddMediaBinItem(
                rudis_core::MediaBinItem {
                    id: media_id.clone(),
                    path: path.clone(),
                    media_kind: rudis_core::MediaKind::Video,
                    duration_us: info.duration_us,
                    width: info.width,
                    height: info.height,
                    fps: info.avg_frame_rate,
                    is_vfr: info.is_vfr,
                    rotation_degrees: info.rotation_degrees,
                    has_audio: info.has_audio,
                    poster_path: None,
                    folder: String::new(),
                    display_name: None,
                    is_image_sequence: false,
                    reports_alpha: None,
                },
            ))
            .expect("add the media bin item");
        store
            .dispatch(rudis_core::Command::AddClip {
                track: lane,
                clip: rudis_core::Clip {
                    id: format!("c-layer-{lane}"),
                    media_id,
                    start_us: 0,
                    in_us: 0,
                    out_us: span_us,
                    volume: 1.0,
                    audio_detached: false,
                    transform: rudis_core::ClipTransform::default(),
                    opacity: 1.0,
                    crop: rudis_core::ClipCrop::default(),
                    keyframes: Default::default(),
                    text: None,
                    alpha_mode: Default::default(),
                    retime: None,
                },
            })
            .expect("place the clip on its lane");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudis_core::{Patch, PatchKind};

    #[test]
    fn test_app_ctx_builds_and_exposes_an_empty_store() {
        let ctx = TestAppCtx::new();
        // Constructing it must not panic, and the store must be lockable.
        let store = ctx.store().lock().expect("store lock");
        assert_eq!(store.seq(), 0, "a fresh Store starts at seq 0");
    }

    #[test]
    fn data_and_cache_dirs_are_two_different_existing_directories() {
        let ctx = TestAppCtx::new();
        let data = ctx.app_data_dir().expect("app_data_dir");
        let cache = ctx.app_cache_dir().expect("app_cache_dir");

        assert!(data.is_dir(), "app_data_dir must exist on disk: {data:?}");
        assert!(cache.is_dir(), "app_cache_dir must exist on disk: {cache:?}");
        assert_ne!(
            data, cache,
            "data and cache dirs must be DISTINCT — a shared dir is exactly the \
             parallel-test race build_app_isolated exists to avoid"
        );
    }

    #[test]
    fn two_instances_do_not_share_a_data_dir() {
        // Per-INSTANCE isolation is the property migrated tests rely on; the
        // src-tauri helpers had to be handed a unique identifier string to get
        // it, which is a step a caller can forget.
        let a = TestAppCtx::new();
        let b = TestAppCtx::new();
        assert_ne!(
            a.app_data_dir().expect("a"),
            b.app_data_dir().expect("b"),
            "each TestAppCtx must get its own app_data_dir"
        );
    }

    #[test]
    fn emit_patch_round_trips_the_exact_patch_base_seq_and_seq() {
        let ctx = TestAppCtx::new();
        assert!(
            ctx.emitted_patches().is_empty(),
            "nothing emitted before the first emit_patch call"
        );

        let patch = Patch {
            kind: PatchKind::ClipMoved,
            ids: vec!["clip-1".to_string(), "clip-2".to_string()],
            entities: None,
        };
        ctx.emit_patch(&patch, 7, 8).expect("emit_patch");

        let emitted = ctx.emitted_patches();
        assert_eq!(emitted.len(), 1, "exactly one emission recorded");
        assert_eq!(emitted[0].0, patch, "the patch round-trips unchanged");
        assert_eq!(emitted[0].1, 7, "base_seq round-trips");
        assert_eq!(emitted[0].2, 8, "seq round-trips");
    }
}
