//! `rudis_ffi` — the Phase 47 C ABI over the Rudis engine.
//!
//! Charter:
//! - **Additive.** This crate is a NEW workspace member wrapping the `AppCtx`
//!   seam (Phase 45) and, where events cross, the `preview` seam (Phase 46).
//!   The shipping shell build stays buildable and behaviorally identical at
//!   every commit; any observable change to it is a defect of this phase.
//! - **JSON envelopes for the cold path** (S6). Domain results cross as
//!   `{"Ok": ..}` / `{"Err": ".."}` bytes in a [`RudisBuffer`]; only transport
//!   faults (null pointer, bad UTF-8, a caught panic) use [`RudisStatus`].
//! - **Zero desktop-shell-framework dependency** — same gate as
//!   `crates/preview`; the manifest is substring-clean by design.
//! - **Offline core** (CLAUDE.md): no network crate, ever. The standalone
//!   manifest guard test lands in plan 47-07.
//! - **Every export is guarded and registered** via [`ffi_guard!`] (D-08): a
//!   panic maps to [`RudisStatus::PanicCaught`] / a null return, never
//!   unwinding across the `extern "C"` boundary (FFI-02, threat T-47-01).
//!
//! # D-07 ctx-parameter exemptions (recorded per plan 47-02)
//!
//! Every command export takes a `*mut RudisCtx`. Exactly three exports are
//! exempt, each for a structural reason:
//! - [`rudis_init`] — it CREATES the ctx;
//! - [`rudis_free_buffer`] — it frees via the process allocator and must be
//!   callable even after the ctx that produced the buffer is gone;
//! - [`rudis_abi_probe`] — deliberately ctx-free so a host can smoke-test
//!   symbol resolution before ever calling `rudis_init`.

mod buffer;
pub mod commands;
pub mod ctx;
pub mod dispatch;
pub mod export;
/// Phase 51 (SHELL-04): the C# shell's own preview host — the second
/// implementation of the frozen `preview::PreviewHost` / `preview::PresentSink`
/// ports (D-03), plus the `SwapChainPanel` surface layer and the four panel
/// exports.
///
/// The MODULE is private and everything in it is `pub(crate)` or narrower; the
/// only surface reachable from outside is `panel::exports`' four `#[no_mangle]`
/// functions, which are exported by the linker regardless of Rust visibility.
/// Keep it that way — `cbindgen` does not honour module privacy, so a stray
/// `pub` item in here lands in the committed C header (plan 51-03 found and
/// fixed exactly that for the overlay's ink colour).
mod panel;
pub mod ring;
mod self_advance;
/// Phase 54.1 (plan 54.1-02): the agent-vision snapshot builders behind
/// [`ctx::FfiAppCtx`]'s `AgentVision` impl. PRIVATE for the same reason `panel`
/// is — `cbindgen` does not honour module privacy, so nothing in here may be
/// `pub`, or it lands in the committed C header. Every item is `pub(crate)`.
mod vision;

pub use buffer::*;

use std::path::PathBuf;

/// Opaque per-instance context (D-07). NEVER a process-global: SC-4's tier
/// builds and tears down independent instances (the OnceLock-global
/// alternative was rejected in CONTEXT for making tests order-dependent).
///
/// Every field is per-instance — two `RudisCtx`s share NO domain state, no
/// event stream, no directories (the D-07 independence property, asserted by
/// `ctx.rs`'s tests). The one deliberate exception class is process-wide id
/// COUNTERS inside `app-core` (`ID_SEQ` etc., RESEARCH E2): ids stay globally
/// unique across instances, which is a desirable property, not shared state.
pub struct RudisCtx {
    /// The single backend-owned store (`Mutex<rudis_core::Store>`) — a fresh
    /// default per instance, exactly what the shell's `.manage()` seeds.
    /// `Arc`-wrapped since Phase 50 (D-17) so the opt-in self-advance tick
    /// thread can hold a compiler-proven `Send + 'static` clone — no
    /// `unsafe impl`, no raw ctx pointer crosses into the thread (T-50-01).
    /// `FfiAppCtx::store()` still hands out `&SharedStore` via deref.
    store: std::sync::Arc<app_core::SharedStore>,
    /// The three host directories every `AppCtx` impl must answer for.
    /// Read by `ctx::FfiAppCtx` (`app_data_dir`/`app_cache_dir`/
    /// `resolve_resource`).
    data_dir: PathBuf,
    cache_dir: PathBuf,
    resource_dir: PathBuf,
    /// Owned per-instance temp dirs backing whichever of the three the caller
    /// did not configure (the `TestAppCtx` pattern promoted to production).
    /// Held for `Drop`: the directories live exactly as long as the ctx.
    _temp_dirs: Vec<tempfile::TempDir>,
    /// The per-instance bounded event stream (D-10/D-11). `Arc` because
    /// `AppCtx::export_progress_sink` must hand the encoder an OWNED
    /// `Send + 'static` closure that outlives the borrow.
    ring: std::sync::Arc<ring::EventRing>,
    /// The hot-path playback mirror (RESEARCH A5): owned per instance, NOT an
    /// `AppCtx` capability — plain FFI plumbing. Written by `rudis_transport`'s
    /// host half; read lock-free by `rudis_get_playback_position` (47-05).
    mirror: std::sync::Arc<preview::PlaybackMirror>,
    /// Phase 57 (plan 57-08, PLAY-05): the ENGINE→shell diagnostic mirror — the
    /// exact twin of `mirror` above with the arrow reversed. The preview
    /// producer this ctx spawns writes the active dynamic-playback-resolution
    /// level here; `rudis_get_playback_resolution_level` reads it lock-free,
    /// the same way `rudis_get_playback_position` reads `mirror.position_us`.
    ///
    /// Per instance (D-07), reached by the producer through this ctx's
    /// `ShellPreviewHost`, which overrides `PreviewHost::engine_diag`. Every
    /// other host in the tree takes that method's default — which is why no
    /// shell file had to change for PLAY-05 (D-01).
    diag: std::sync::Arc<preview::EngineDiag>,
    /// Phase 51 (SHELL-04): the COMMITTED-INK mirror the C# shell's
    /// `PreviewHost::resolve_overlay` reads every present tick. `Arc` so the
    /// present thread gets a clone rather than a borrow of `ctx` (the T-50-01
    /// discipline `self_advance` already follows); `Mutex` because the DISPATCH
    /// thread writes it and the PRESENT thread `try_lock`s it — never the other
    /// way round, which is what keeps T-51-12's lock cycle unreachable.
    overlay: std::sync::Arc<std::sync::Mutex<panel::overlay::CanvasOverlayMirror>>,
    /// Phase 51: the mid-play edit-flush signal (`preview::PreviewEditSeq`) the
    /// present loop takes as an explicit parameter. Owned per instance and
    /// bumped from [`RudisCtx::observe_preview_patch`]; the Tauri shell bumps
    /// its own from a `project:changed` listener.
    edit_seq: std::sync::Arc<preview::PreviewEditSeq>,
    /// Phase 51 (SHELL-04): the ONE preview surface this instance can host —
    /// the GPU set, the resize atomics and the content-rect atomics. Owned
    /// here (not in a registry) because a `RudisCtx` IS the instance;
    /// `Arc` so the present thread and the two port adapters each hold a
    /// clone rather than a borrow of `ctx`.
    ///
    /// Empty until a panel attaches: every `PresentSink` method degrades to a
    /// documented no-op meanwhile. `rudis_preview_attach_panel` fills the GPU
    /// set, `rudis_preview_resize` publishes into the atomics, and
    /// `rudis_preview_content_rect` reads them back.
    preview_surface: std::sync::Arc<panel::state::PreviewSurfaceState>,
    /// The credential seam behind the `rudis_agent_status`/`rudis_set_api_key`/
    /// `rudis_clear_api_key` exports (the provider-key twins,
    /// `rudis_set_provider_key`/`rudis_clear_provider_key`, write
    /// `provider_key_store` instead) and `rudis_agent_send_message`'s
    /// transport connect. Injected by [`RudisCtx::new_in_process`] so the
    /// contract tier (47-06) can supply `agent_llm::InMemoryKeyStore`;
    /// production (`rudis_init`) supplies the real
    /// `agent_llm::KeyringStore::anthropic_in(&service)` — filed under the
    /// validated [`credential_service`](Self::credential_service) (Phase 69).
    key_store: Box<dyn agent_llm::KeyStore + Send + Sync>,
    /// Phase 69 (OSS-02, D-69-02): the validated credential SERVICE this ctx
    /// was built under — `"rudis"` in production, a `rudis-test-*` value only
    /// when `InitConfig.credential_service` passed the validator (a supplied
    /// value that fails it refuses construction — review 69 WR-01).
    credential_service: agent_llm::CredentialService,
    /// Phase 69 (D-69-04, D-69-13): the ONE provider credential store the
    /// three generation slots resolve through (and Settings writes to).
    /// `rudis_init` builds it with `ManagedProviderKeyStore::production_in`
    /// under the same service as `key_store`; the rlib test tier gets
    /// [`RudisCtx::in_memory_provider_stores`] by default, so `cargo test`
    /// never constructs a `KeyringStore` against `rudis` (D-69-23).
    provider_key_store: app_core::ManagedProviderKeyStore,
    /// Phase 54.1 (plan 04): the generation host-state the Tauri shell holds as
    /// five `.manage()`d singletons — PLAIN STRUCT FIELDS here (54.1-RESEARCH
    /// Pitfall 2: never `tauri::State`, which this crate structurally cannot
    /// name anyway). Read only by [`ctx::FfiAppCtx`]'s `GenSubmission` impl,
    /// which bundles them into an `app_core::GenHost` and forwards to the ONE
    /// shared seam.
    ///
    /// The three provider slots are LAZY, INVALIDATABLE `ProviderSlot`s built
    /// by `production*()` (Phase 69, D-69-13), resolving only through
    /// `provider_key_store`, so constructing a ctx performs **zero** Windows
    /// Credential Manager reads
    /// — the LAT-05 property, preserved verbatim from the Tauri host. The first
    /// real read happens inside the seam, for the ONE modality in use, after the
    /// prompt has been validated. Same discipline as `key_store` above.
    gen_image_provider: app_core::ManagedGenProvider,
    gen_video_provider: app_core::ManagedVideoGenProvider,
    gen_audio_provider: app_core::ManagedAudioGenProvider,
    /// The per-instance job table (D-07: two ctxs share no job ids).
    gen_jobs: app_core::ManagedGenJobs,
    /// The GEN-08 clean-model allow list. `Default` IS
    /// `agent_gen::production_allow_list()` — the ONE vetted list, exactly what
    /// `configure()` manages for the Tauri host. Never a second list.
    gen_allow_list: app_core::ManagedAllowList,
    /// The one active-project pointer the shell `.manage()`s at setup —
    /// per-instance here, mirroring `TestAppCtx`.
    active_project_meta: app_core::project_store::ActiveProjectMeta,
    /// The ONE Chat conversation state, per-instance (`TestAppCtx`'s shape).
    agent_session: std::sync::Mutex<app_core::AgentSession>,
    /// Backs `FfiAppCtx::block_on` (RESEARCH E1): a REAL multi-threaded Tokio
    /// runtime, built lazily — `import_one_path` calls `spawn_blocking`, which
    /// panics under a bare future poller. One runtime per instance keeps D-07
    /// honest.
    runtime: std::sync::OnceLock<tokio::runtime::Runtime>,
    /// D-17 (Phase 50): the OPT-IN engine playback clock — `Some` only when
    /// [`InitConfig::self_advance`] was `true`. Stop flag + `JoinHandle`,
    /// stopped and joined by [`RudisCtx`]'s `Drop` impl BEFORE any owned
    /// field drops (T-50-02), which `rudis_shutdown`'s `Box::from_raw` drop
    /// reaches. `None` (every existing caller) spawns nothing — the default
    /// path is byte-for-byte the pre-Phase-50 host-driven clock.
    self_advance: Option<self_advance::SelfAdvanceHandle>,
}

/// D-17 (T-50-02): stop + join the self-advance tick thread FIRST — Rust
/// runs `Drop::drop` before dropping any field, so the thread is provably
/// gone before the store/mirror/ring it shares are torn down. A ctx without
/// the opt-in clock takes `None` and this is a no-op.
///
/// **Phase 58 D-11 (58-REVIEW WR-01/WR-02): raise every proxy cancel latch here
/// too, for exactly the same "before the fields drop" reason.** The `runtime`
/// field owns a Tokio `Runtime`, and dropping a `Runtime` **waits for
/// already-started `spawn_blocking` tasks** — it does not cancel them.
/// `proxy_job`'s generation runs in `spawn_blocking` and blocks for the whole
/// encode (58-05 measured 0.5–5 s on 720p fixtures; a real 4K source is
/// minutes). Without this line, closing the app straight after importing a heavy
/// file hangs `rudis_shutdown` for the length of that encode — the "the app
/// won't quit" symptom, from a mechanism this phase already ships.
///
/// `cancel_all` is the same call `run_new_project`/`run_open_project` already
/// make; the poll loop in `proxy::generate` reads the latch within one 50 ms
/// interval and kills the sidecar (58-03 measured 64 ms end to end), so teardown
/// is bounded rather than open-ended. It is best-effort and idempotent: a
/// process with no proxy jobs walks an empty map.
impl Drop for RudisCtx {
    fn drop(&mut self) {
        // MUST come before the `runtime` field drops. See the doc above.
        //
        // MUTATION-VERIFIED 2026-08-03 by
        // `crates/ffi/tests/shutdown_proxy_cancel.rs`: with this line commented
        // out, `rudis_shutdown` took **36.44 s** against an in-flight encode of
        // a 2 400 s 720p fixture; with it, **49 ms**.
        app_core::proxy_job::cancel_all();
        // Phase 59 (59-CONTEXT D-24), for the SAME reason and with the same
        // urgency: a background segment render is an `ffmpeg` child on the
        // shared hardware encoder, and this ctx's store is about to drop under
        // it. `clear_render_host` raises every latch and then forgets the host,
        // so a render that outlives this call cannot reach a freed store and
        // cannot hold the admission the proxy worker also uses.
        //
        // MUST come before the `runtime` field drops, exactly as the line above
        // must: the render runs on its own thread rather than in the runtime,
        // but the two share one permit, and a shutdown that waited on a proxy
        // encode queued behind a render would be the same "the app won't quit"
        // symptom by a longer route.
        app_core::render_cache_job::clear_render_host();
        // Dropping the handle sets the stop flag and joins (bounded by the
        // ~10ms tick cadence — never a hang; proven by Test 5).
        self.self_advance.take();
    }
}

impl RudisCtx {
    /// The in-process constructor for the rlib contract tier (NOT an ABI
    /// export). 47-06 injects `agent_llm::InMemoryKeyStore` here: the REAL
    /// `KeyringStore` wraps the machine-global OS Credential Manager, which
    /// parallel tests would race on (RESEARCH E2 — a concrete hazard, not a
    /// style choice). Production (`rudis_init`) calls
    /// [`new_in_process_with`](Self::new_in_process_with) with
    /// `KeyringStore::anthropic_in(&service)` and the OS-backed provider
    /// stores, so the two paths differ ONLY in the injected stores.
    ///
    /// (The plan prose calls the config type `FfiConfig`; it is 47-02's
    /// already-shipped [`InitConfig`], now `pub` — same shape, same fields.)
    ///
    /// Phase 69 (D-69-23): the TEST-TIER convenience — the provider slots get
    /// [`in_memory_provider_stores`](Self::in_memory_provider_stores), so no
    /// test that builds a ctx this way can read or write a real provider
    /// credential. Production goes through [`new_in_process_with`](Self::new_in_process_with).
    pub fn new_in_process(
        config: InitConfig,
        key_store: Box<dyn agent_llm::KeyStore + Send + Sync>,
    ) -> Result<Self, String> {
        Self::new_in_process_with(config, key_store, Self::in_memory_provider_stores())
    }

    /// The in-memory provider stores the rlib TEST tier uses — one `InMemoryKeyStore` per
    /// `PROVIDER_KEY_SLOTS` row, each with its row's validator. Production never calls this.
    pub fn in_memory_provider_stores() -> app_core::ManagedProviderKeyStore {
        app_core::ManagedProviderKeyStore::with_stores(
            app_core::PROVIDER_KEY_SLOTS
                .iter()
                .map(|&(provider, _, validator)| {
                    let store: Box<dyn agent_llm::KeyStore> =
                        Box::new(agent_llm::InMemoryKeyStore::with_validator(validator));
                    (provider.to_string(), store)
                })
                .collect(),
        )
    }

    /// The real constructor: key store AND provider stores injected. `rudis_init` passes the
    /// two OS-backed stores (both under the validated credential service); tests pass
    /// in-memory ones.
    pub fn new_in_process_with(
        config: InitConfig,
        key_store: Box<dyn agent_llm::KeyStore + Send + Sync>,
        provider_key_store: app_core::ManagedProviderKeyStore,
    ) -> Result<Self, String> {
        // Computed BEFORE `config`'s dir fields are moved into `dir_or_temp`.
        // Review 69 WR-01: a SUPPLIED-but-invalid service is REFUSED (never a silent
        // fallback to production); only an ABSENT one means `"rudis"`. The error
        // names the rule, never the value.
        let credential_service =
            agent_llm::CredentialService::try_from_config(config.credential_service.as_deref())
                .ok_or_else(|| {
                    "InitConfig.credential_service rejected (must match rudis-test-[A-Za-z0-9-]{1,48})"
                        .to_string()
                })?;
        let mut temp_dirs = Vec::new();
        let data_dir = dir_or_temp(config.data_dir, &mut temp_dirs)?;
        let cache_dir = dir_or_temp(config.cache_dir, &mut temp_dirs)?;
        let resource_dir = dir_or_temp(config.resource_dir, &mut temp_dirs)?;
        let mut ctx = Self {
            store: std::sync::Arc::new(std::sync::Mutex::new(rudis_core::Store::default())),
            data_dir,
            cache_dir,
            resource_dir,
            _temp_dirs: temp_dirs,
            ring: std::sync::Arc::new(ring::EventRing::new()),
            mirror: std::sync::Arc::new(preview::PlaybackMirror::new()),
            diag: std::sync::Arc::new(preview::EngineDiag::new()),
            overlay: std::sync::Arc::new(std::sync::Mutex::new(
                panel::overlay::CanvasOverlayMirror::default(),
            )),
            edit_seq: std::sync::Arc::new(preview::PreviewEditSeq::new()),
            preview_surface: std::sync::Arc::new(panel::state::PreviewSurfaceState::default()),
            key_store,
            credential_service,
            provider_key_store,
            // Phase 54.1 (plan 04): PRODUCTION defaults — three empty
            // lazy slots (zero credential reads until first real use,
            // LAT-05), a fresh job registry, and the ONE vetted GEN-08 allow
            // list `agent_gen::production_allow_list()` that `ManagedAllowList`'s
            // `Default` resolves to (the same value `configure()` manages for
            // the Tauri host — never a second list, T-54.1-06).
            gen_image_provider: app_core::ManagedGenProvider::production(),
            gen_video_provider: app_core::ManagedVideoGenProvider::production_video(),
            gen_audio_provider: app_core::ManagedAudioGenProvider::production_audio(),
            gen_jobs: app_core::ManagedGenJobs::default(),
            gen_allow_list: app_core::ManagedAllowList::default(),
            active_project_meta: app_core::project_store::ActiveProjectMeta::default(),
            agent_session: std::sync::Mutex::new(app_core::AgentSession::default()),
            runtime: std::sync::OnceLock::new(),
            self_advance: None,
        };
        // D-17 (Phase 50): spawn the opt-in engine clock AFTER construction —
        // the thread receives only `Arc` clones of the store/mirror/ring
        // (compiler-proven Send + Sync), never a borrow of `ctx` itself, so
        // the move into `Box::into_raw`/the caller stays borrow-clean.
        if config.self_advance {
            let handle = self_advance::spawn(
                std::sync::Arc::clone(&ctx.store),
                std::sync::Arc::clone(&ctx.mirror),
                std::sync::Arc::clone(&ctx.ring),
            )
            .map_err(|e| format!("spawn D-17 self-advance tick thread: {e}"))?;
            ctx.self_advance = Some(handle);
        }
        // Phase 59 (plan 59-08, CACHE-01): register the `PreviewHost` a
        // BACKGROUND segment render composites through.
        //
        // `app_core` cannot build one — `AppCtx` deliberately does not carry the
        // shell-SERVICES port (46-CONTEXT D-08: a host that owns a preview
        // surface implements `PreviewHost`, and one that does not should not be
        // forced to) — and a detached render needs a `'static` host, which is
        // why the four fields below are already `Arc`s. This is the SAME
        // `ShellPreviewHost` `rudis_preview_attach_panel` and
        // `resize_reconfigure_and_present` build per call, over the SAME store
        // `FfiAppCtx` reads, so the scheduler's view of the project and the
        // render's view cannot diverge.
        //
        // No shell code and no new region (D-34): this is the cdylib wiring one
        // existing port to one existing consumer. `Drop` clears it.
        app_core::render_cache_job::set_render_host(std::sync::Arc::new(
            panel::host::ShellPreviewHost::new(
                std::sync::Arc::clone(&ctx.store),
                std::sync::Arc::clone(&ctx.mirror),
                std::sync::Arc::clone(&ctx.overlay),
                std::sync::Arc::clone(&ctx.diag),
            ),
        ));
        Ok(ctx)
    }

    /// The ctx's event ring — the rlib-only injection seam the FFI-contract
    /// tier (47-06) pushes through to prove D-02 schema completeness for the
    /// three producer-less tags (`canvas-pointer`/`gen:job`/`gen:progress` —
    /// RESEARCH A4: no window and no WndProc exist in this build, so a
    /// synthetic push is the ONLY honest producer this phase) and D-10's
    /// overflow → resync path through the public poll export.
    ///
    /// NOT an ABI export: this is ordinary rlib API, invisible to the cdylib's
    /// C surface — the locked 23-symbol registry pin is untouched (re-proven
    /// by `export_registry_matches_the_locked_surface` after this addition).
    pub fn ring(&self) -> &ring::EventRing {
        &self.ring
    }

    /// Phase 69: the validated credential service this ctx addresses (rlib
    /// API, not an export). Coordinates only — no secret.
    pub fn credential_service(&self) -> &agent_llm::CredentialService {
        &self.credential_service
    }

    /// Phase 69: the provider credential stores the generation slots resolve
    /// through (rlib API, not an export).
    pub fn provider_key_store(&self) -> &app_core::ManagedProviderKeyStore {
        &self.provider_key_store
    }

    /// Phase 69 (plan 69-02, D-69-13/D-69-23): read-only access to the three
    /// generation provider slots `(image, video, audio)` (rlib API, not an
    /// export), so the contract tier can observe `is_resolved()` and the
    /// same-ctx set → resolve → clear → re-resolve cycle through the REAL
    /// exports. Borrowing a slot reads nothing: only `resolve_via` touches a
    /// credential store (LAT-05).
    pub fn gen_provider_slots(
        &self,
    ) -> (
        &app_core::ManagedGenProvider,
        &app_core::ManagedVideoGenProvider,
        &app_core::ManagedAudioGenProvider,
    ) {
        (
            &self.gen_image_provider,
            &self.gen_video_provider,
            &self.gen_audio_provider,
        )
    }

    /// Test seam (Phase 54.1): fixture slots for the contract tier — the
    /// [`app_core::ManagedGenProvider::preset`] discipline, one host over.
    /// **Production code must never call this**: it PRE-FILLS credential slots,
    /// which is precisely what makes a preset slot structurally incapable of
    /// reaching a real `agent_llm::KeyringStore` (`ProviderSlot::resolve_with`
    /// short-circuits on a resolved slot, so the resolver closure never
    /// runs — the RESEARCH E2 parallel-test hazard, avoided by construction).
    ///
    /// `None` for a modality is a DELIBERATE, DETERMINISTIC absence: a preset
    /// `None` slot resolves to `None` without ever consulting the machine's
    /// credential store, so "no provider configured" is a property of the test
    /// rather than of whatever keys happen to sit on the developer's box. That
    /// distinction is what lets one test prove "refused because unkeyed" and
    /// another prove "refused because unapproved, WITH a provider present"
    /// (54.1 SC-3).
    ///
    /// The job registry is reset too, so a preset ctx starts with an empty job
    /// table. `#[doc(hidden)]` + `pub`: the contract tier lives both in this
    /// crate's `#[cfg(test)]` modules and (from plan 54.1-05) in
    /// `crates/ffi/tests/`, which a `pub(crate)` seam could not reach.
    #[doc(hidden)]
    pub fn preset_generation(
        &mut self,
        image: Option<std::sync::Arc<agent_gen::ConcreteGenProvider>>,
        video: Option<std::sync::Arc<agent_gen::ConcreteGenProvider>>,
        audio: Option<std::sync::Arc<agent_gen::ConcreteGenProvider>>,
        allow_list: agent_gen::AllowList,
    ) {
        self.gen_image_provider = app_core::ManagedGenProvider::preset(image);
        self.gen_video_provider = app_core::ManagedVideoGenProvider::preset(video);
        self.gen_audio_provider = app_core::ManagedAudioGenProvider::preset(audio);
        self.gen_jobs = app_core::ManagedGenJobs::default();
        self.gen_allow_list = app_core::ManagedAllowList(allow_list);
    }

    /// Keep the preview-side ink mirror and the mid-play edit-flush signal
    /// truthful (Phase 51, SHELL-04).
    ///
    /// The Tauri host does this from a `project:changed` listener
    /// (`src-tauri/src/native_surface.rs:1385-1450` for the ink mirror,
    /// `register_edit_seq_listener` for the flush signal). The C ABI has no
    /// event loop to listen ON, so the patch-producing EXPORTS are the hook:
    /// [`crate::dispatch::rudis_dispatch_command`] (the annotation path),
    /// `rudis_undo`/`rudis_redo` (an undone `AddAnnotation` must un-ink) and
    /// `rudis_apply_option_card` (CANV-02 cards mint annotations).
    ///
    /// Deliberately NOT hooked into `ctx::FfiAppCtx::emit_patch`, which is the
    /// tempting single chokepoint: `emit_patch` is called from inside
    /// `app-core`'s `*_inner` bodies at varying lock depths, and this method
    /// takes the store lock — a re-entrant acquisition there would be a
    /// deadlock, not a warning. At an export boundary no domain lock is held.
    ///
    /// Cheap: two SHORT, strictly sequential lock scopes, no I/O, and never on
    /// the present thread. Lock order is store → overlay and the two are NEVER
    /// held simultaneously (T-51-12); the present thread only ever `try_lock`s
    /// the overlay, so no lock cycle exists.
    pub(crate) fn observe_preview_patch(&self, patch: &rudis_core::Patch) {
        // The flush signal filters itself (`patch_touches_preview`): canvas ink
        // deliberately does NOT drain the playback ring.
        self.edit_seq.observe_patch(patch);

        // Skip the store lock entirely for the common non-canvas patch — the
        // same pre-filter the Tauri listener applies before it reaches for the
        // store. `apply_patch_to_overlay` would no-op these anyway.
        if !matches!(
            patch.kind,
            rudis_core::PatchKind::AnnotationAdded
                | rudis_core::PatchKind::AnnotationRemoved
                | rudis_core::PatchKind::CanvasCleared
                | rudis_core::PatchKind::CanvasRestored
        ) {
            return;
        }

        // Scope 1: the AUTHORITATIVE post-patch list, FrameLinked-only
        // (T-51-13 — a whiteboard mark must never composite onto real video).
        // The guard drops at the end of this block, BEFORE the overlay lock.
        let canvas = {
            let Ok(store) = self.store.lock() else {
                return; // poisoned store: skip this refresh, never unwind again
            };
            panel::overlay::frame_linked(store.canvas())
        };

        // Scope 2: the mirror. A poisoned overlay degrades to a stale mirror
        // that self-corrects on the next canvas patch — never a panic.
        if let Ok(mut mirror) = self.overlay.lock() {
            panel::overlay::apply_patch_to_overlay(
                &mut mirror,
                patch,
                &canvas,
                std::time::Instant::now(),
            );
        }
    }

    /// What the present thread's `PreviewHost::resolve_overlay` would hand the
    /// compositor RIGHT NOW: every still-visible committed mark's id paired with
    /// its current fade alpha, evaluated at the injected `now`.
    ///
    /// # Why this is `pub`
    ///
    /// The committed-ink mirror is the far end of the whole Canvas round trip —
    /// "the client dispatched" is only half a claim; "the engine will draw it, and
    /// will stop drawing it on the v6.0 curve" is the other half. `mod panel` is
    /// private, so the contract tier (`tests/contract.rs`, an INTEGRATION test that
    /// links this crate as a library exactly as the C# host links the cdylib) has no
    /// other way to observe it. This one read-only accessor closes that loop without
    /// widening the module or duplicating the fade arithmetic in a test.
    ///
    /// It is a READ. It takes the overlay lock only (never the store's), returns
    /// owned data, and degrades to an empty list on a poisoned lock rather than
    /// unwinding a second time — the same rule every method in `panel::host`
    /// follows.
    ///
    /// Note the alpha-0 convention it inherits from
    /// `panel::overlay::visible_annotations_with_alpha`: a fully-faded mark is
    /// OMITTED ENTIRELY rather than returned at `0.0`. "Not in this list" is what
    /// "the engine draws nothing for it" looks like.
    ///
    /// cbindgen never emits this: it is an inherent method, not a `#[no_mangle]
    /// extern "C"` free function, so the committed header is unchanged (verified by
    /// regenerating it — the drift gate is the check, not the claim).
    pub fn preview_ink_at(&self, now: std::time::Instant) -> Vec<(String, f32)> {
        let Ok(mirror) = self.overlay.lock() else {
            return Vec::new();
        };
        panel::overlay::visible_annotations_with_alpha(&mirror, now)
            .into_iter()
            .map(|(annotation, alpha)| (annotation.id, alpha))
            .collect()
    }
}

/// The SC-5 freshness probe payload. 47-08 edits this string and watches
/// `dotnet build` pick the change up with no manual `cargo build`; 47-07's
/// libloading top-up resolves [`rudis_abi_probe`] as its trivial symbol target.
pub const ABI_PROBE: &str = "rudis-ffi-phase47-probe-v1";

/// Optional UTF-8 JSON config for [`rudis_init`] and the in-process
/// constructor [`RudisCtx::new_in_process`]. Every field is optional; an
/// absent field is backed by a fresh per-instance temp dir. `pub` since 47-04
/// so the rlib contract tier (47-06) can build configs directly.
#[derive(Default, serde::Deserialize)]
pub struct InitConfig {
    pub data_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub resource_dir: Option<PathBuf>,
    /// D-17 (Phase 50): when `true`, the ENGINE owns the playback clock — a
    /// per-instance tick thread drives `TransportCmd::Advance` against wall
    /// time while `playing`, so a host that presses Play needs ZERO per-frame
    /// clock code. Defaults to `false` (host-driven clock, byte-for-byte the
    /// pre-Phase-50 behaviour): the Tauri shell passes no config and keeps
    /// driving `Advance` itself — an unconditional engine clock would double
    /// -drive it to 2× speed. `#[serde(default)]` keeps every existing config
    /// JSON parsing unchanged.
    #[serde(default)]
    pub self_advance: bool,
    /// Phase 69 (D-69-02): an OPTIONAL test-scoped credential service. Accepted only when it
    /// matches `^rudis-test-[A-Za-z0-9-]{1,48}$` (`agent_llm::validate_credential_service`).
    /// ABSENT means the production service `"rudis"`; a SUPPLIED value that fails validation
    /// makes `rudis_init` return null (reported on stderr, key-free) — never a silent fallback
    /// to production (review 69 WR-01). The shell sets it only from a Debug-only env seam
    /// (`RUDIS_TEST_CREDENTIAL_SERVICE`).
    #[serde(default)]
    pub credential_service: Option<String>,
}

/// An explicitly-configured directory, or a fresh per-instance `TempDir`
/// whose guard is pushed into `temp_dirs` (kept alive inside [`RudisCtx`]).
fn dir_or_temp(
    explicit: Option<PathBuf>,
    temp_dirs: &mut Vec<tempfile::TempDir>,
) -> Result<PathBuf, String> {
    match explicit {
        Some(p) => Ok(p),
        None => match tempfile::tempdir() {
            Ok(t) => {
                let p = t.path().to_path_buf();
                temp_dirs.push(t);
                Ok(p)
            }
            Err(e) => Err(format!("create per-instance temp dir: {e}")),
        },
    }
}

/// Create an independent engine instance and hand back its opaque handle.
///
/// `config_json`/`config_len` may describe an optional UTF-8 JSON object
/// `{"data_dir": "...", "cache_dir": "...", "resource_dir": "...",
/// "self_advance": false, "credential_service": null}`; a null pointer or zero length means "no config",
/// and any absent directory is backed by a per-instance temp dir owned by
/// the returned ctx. Returns null on invalid UTF-8/JSON (T-47-05:
/// `str::from_utf8`, never `_unchecked`) or any construction failure — and
/// on a caught panic.
///
/// `self_advance` (D-17, Phase 50, OPT-IN, default `false`): when `true`,
/// the ENGINE owns the playback clock — a per-instance tick thread advances
/// the playhead against wall time while playing, so a host that presses Play
/// sees `rudis_get_playback_position()` progress with ZERO per-frame clock
/// code, and end-of-media auto-pause pushes one `playback:changed` event.
/// A host that drives its own `advance` transport loop must leave this
/// `false` — both clocks running would advance the same playhead twice
/// (2x-speed playback). The retired Tauri shell was such a host; the shipping
/// C# shell is NOT — it passes `self_advance: true` (see
/// `BuildInitConfigJson` in `shell/Rudis.Shell/App.xaml.cs`).
///
/// D-07 note (recorded per plan 47-02): D-07's sketch is zero-arg; the config
/// buffer is discretion-shaped (module layout / export naming are Claude's),
/// and the locked essence — opaque handle, paired shutdown, every export
/// takes ctx, no process-global — is fully preserved. A zero-arg init would
/// hardcode a directory convention Phase 50 could never override.
#[no_mangle]
pub extern "C" fn rudis_init(config_json: *const u8, config_len: usize) -> *mut RudisCtx {
    crate::ffi_guard!("rudis_init", std::ptr::null_mut(), {
        let parsed: Option<InitConfig> = if config_json.is_null() || config_len == 0 {
            Some(InitConfig::default())
        } else {
            // SAFETY: non-null and `config_len` bytes long by the caller's
            // contract; read-only for the duration of this call.
            let bytes = unsafe { std::slice::from_raw_parts(config_json, config_len) };
            match std::str::from_utf8(bytes) {
                Ok(text) => serde_json::from_str::<InitConfig>(text).ok(),
                Err(_) => None,
            }
        };
        let Some(config) = parsed else {
            return std::ptr::null_mut();
        };

        // Production parity with the shell's managed key store: the REAL
        // OS-credential-manager slots. Tests never reach this line — the
        // contract tier goes through `new_in_process` with an
        // `InMemoryKeyStore` instead (RESEARCH E2).
        //
        // Phase 69 (D-69-02): BOTH OS-backed stores are filed under the ONE
        // validated service. Review 69 WR-01: an ABSENT service is production
        // `"rudis"`; a SUPPLIED value that fails validation REFUSES the whole
        // init (null) — it never falls back to production, because the only
        // caller that sets the field is a test harness that must not reach the
        // owner's real entries. The diagnostic names no value (the rejected
        // string could be anything).
        let Some(service) =
            agent_llm::CredentialService::try_from_config(config.credential_service.as_deref())
        else {
            eprintln!("[rudis_ffi] InitConfig.credential_service rejected (must match rudis-test-[A-Za-z0-9-]{{1,48}}); refusing to initialise");
            return std::ptr::null_mut();
        };
        match RudisCtx::new_in_process_with(
            config,
            Box::new(agent_llm::KeyringStore::anthropic_in(&service)),
            app_core::ManagedProviderKeyStore::production_in(&service),
        ) {
            Ok(ctx) => Box::into_raw(Box::new(ctx)),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// Tear down a ctx created by [`rudis_init`]. Null → `InvalidHandle`. The
/// managed side wraps this in a `SafeHandle` (Phase 50) so release runs at
/// most once; a dangling second call here is T-47-04's accepted residual.
#[no_mangle]
pub extern "C" fn rudis_shutdown(ctx: *mut RudisCtx) -> RudisStatus {
    crate::ffi_guard!("rudis_shutdown", RudisStatus::PanicCaught, {
        if ctx.is_null() {
            return RudisStatus::InvalidHandle;
        }
        // SAFETY: non-null, and by the caller's contract this is a pointer
        // obtained from `rudis_init` that has not already been shut down.
        drop(unsafe { Box::from_raw(ctx) });
        RudisStatus::Ok
    })
}

/// Free a [`RudisBuffer`] previously returned by this library. The ONLY legal
/// way to free one (same allocator — a managed-side free is heap corruption,
/// T-47-03). An all-zero/null-ptr struct is a safe no-op (RESEARCH B5).
#[no_mangle]
pub extern "C" fn rudis_free_buffer(buf: RudisBuffer) {
    crate::ffi_guard!("rudis_free_buffer", (), {
        if buf.ptr.is_null() {
            return;
        }
        // SAFETY: by the caller's contract `(ptr, len, cap)` is an untampered
        // triple produced by this library's `vec_to_buffer`, freed only once
        // (T-47-02's managed-side mitigation is the SafeHandle/single-release
        // discipline; the null check above makes the zeroed struct safe).
        drop(unsafe { Vec::from_raw_parts(buf.ptr, buf.len, buf.cap) });
    })
}

/// Write the SC-5 freshness probe envelope `{"Ok": "<ABI_PROBE>"}` into a
/// fresh buffer. Null `out` → `NullPointer`. Deliberately ctx-free (see the
/// crate doc's D-07 exemptions): a host can prove symbol resolution and the
/// buffer round-trip before ever creating an instance.
#[no_mangle]
pub extern "C" fn rudis_abi_probe(out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_abi_probe", RudisStatus::PanicCaught, {
        if out.is_null() {
            return RudisStatus::NullPointer;
        }
        let bytes = serde_json::to_vec(&Ok::<&str, String>(ABI_PROBE))
            .expect("serializing the static probe envelope cannot fail");
        // SAFETY: non-null by the check above; the caller hands us a writable
        // out-slot by contract.
        unsafe { *out = buffer::vec_to_buffer(bytes) };
        RudisStatus::Ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (a) The guard catches a REAL panic and maps it to `$panic_ret` — the
    /// FFI-02 property, proven on an actual unwind, not asserted.
    #[test]
    fn ffi_guard_maps_a_real_panic_to_the_panic_ret() {
        let got = crate::ffi_guard!("test_panic_probe", RudisStatus::PanicCaught, {
            panic!("boom")
        });
        assert_eq!(got, RudisStatus::PanicCaught);
    }

    /// The registry half of the SAME expansion: all four production exports
    /// (and the probe registered by the test above) are in `FFI_EXPORTS`
    /// without any of them having been called. 47-07 builds its export-table
    /// equality on exactly this property.
    #[test]
    fn all_four_exports_are_registered_in_ffi_exports() {
        for name in [
            "rudis_init",
            "rudis_shutdown",
            "rudis_free_buffer",
            "rudis_abi_probe",
        ] {
            assert!(
                export::FFI_EXPORTS.contains(&name),
                "`{name}` must be registered by its ffi_guard! expansion; \
                 registry = {:?}",
                &*export::FFI_EXPORTS
            );
        }
    }

    /// The D-01 "nothing silently dropped, nothing extra" backstop at the
    /// registry level (plan 47-05 Task 3): `FFI_EXPORTS` holds EXACTLY the
    /// locked 30-symbol surface — the 20 command exports + the 2 polls +
    /// 47-02's init/shutdown/free/abi_probe + Phase 51's 4 panel exports.
    /// 47-07 re-proves this at the built-DLL level, which is the ground truth.
    ///
    /// The `rudis_` prefix filter exists because THIS test target compiles
    /// the test cfg, where 47-02's guard-mapping test registers its
    /// `test_panic_probe` name via the same `ffi_guard!` expansion; the
    /// cdylib 47-07 inspects has no test cfg, so its table carries exactly
    /// these 29 and nothing else.
    ///
    /// Phase 51 (plan 51-03) moved this 23 → 27:
    /// `rudis_preview_attach_panel` / `_resize` / `_content_rect` /
    /// `_detach_panel`.
    ///
    /// Phase 52 (plan 52-05, SHELL-09) moved it 27 → 28, adding exactly ONE
    /// symbol: `rudis_get_waveform_peaks`, the pure cache read the Timeline's
    /// waveform fill is fed from. Deliberately an EXPORT and not a seventh
    /// event type — `ring::EVENT_NAMES` stays at 6 and retrieval rides Phase
    /// 50 D-06's existing 100 ms cold-path poll. (The 52-05 plan was written
    /// against a 23-symbol surface; it had grown to 27 under Phase 51 before
    /// this plan ran, so the LOCKED DELTA is +1, not 23 → 24.)
    ///
    /// Phase 54 (plan 54-02, D-12/D-13/D-14) moved this 28 → 29, adding
    /// exactly ONE symbol: `rudis_debug_seed_project` — the DEBUG-GATED (env
    /// `RUDIS_DEBUG_SEED_PROJECT`, off by default, asserted off by a contract
    /// test that proves the store byte-unchanged after a refused call)
    /// whole-project seed the C# live-eval harness drives. It is the ONE
    /// deliberate engine-axis-freeze crossing of the phase: every other Phase
    /// 54 plan leaves `git diff` over `crates/**` and `src-tauri/**` empty, so
    /// an ABI or playback regression in this phase has exactly one plausible
    /// cause (`.planning/phases/54-chat-region/artifacts/54-02-freeze-crossing.md`).
    ///
    /// Phase 53.2 (plan 53.2-04, D-12) moved this +1, adding exactly ONE
    /// symbol: `rudis_get_filmstrip_strip` — the pure cache read the Timeline's
    /// filmstrip fill is fed from. Deliberately an EXPORT and not a seventh
    /// event type; retrieval rides Phase 50 D-06's 100 ms cold-path poll.
    /// Note the arithmetic, and note that it is the SECOND time this exact
    /// note has had to be written: 53.2 was planned against a 29-symbol
    /// surface while Phase 54 was executing CONCURRENTLY on another worktree,
    /// so what a plan owns is a DELTA, never an absolute. Read the array's
    /// current value at execution time and add to it.
    ///
    /// Phase 57 (plan 57-08, PLAY-05) moved this +1, adding exactly ONE symbol:
    /// `rudis_get_playback_resolution_level`. It is the D-01-compliant
    /// observable for automatic dynamic playback resolution — engine state
    /// through the EXISTING scalar-getter envelope, copied verbatim from
    /// `rudis_get_playback_position` (same guard, same null/panic sentinel
    /// discipline, no event type, no envelope). No seventh event type went with
    /// it, and `ring::EVENT_NAMES` stays at 6. Nothing in v8 CALLS it — D-01
    /// freezes both shells — so the export existing and being pinned end-to-end
    /// against a real degradation is the deliverable; rendering it is
    /// post-cutover work. Reading the array's current value at execution time
    /// and adding to it, per the note above.
    ///
    /// Phase 58 (plan 58-05, PROXY-02) moved this +1, adding exactly ONE
    /// symbol: `rudis_get_proxy_status` — the pure poll of one media item's
    /// playback-proxy state. Deliberately an EXPORT and not a seventh event
    /// type (58-CONTEXT D-29): retrieval rides Phase 50 D-06's 100 ms cold-path
    /// poll, exactly as waveform peaks and filmstrip strips already do, and
    /// `ring::EVENT_NAMES` stays at 6. That is the THIRD time this pattern has
    /// been used and the third time the note has had to be written; read the
    /// array's current value at execution time and add to it, per the note
    /// above.
    ///
    /// Phase 59 (plan 59-08, CACHE-01) moved this +1, adding exactly ONE
    /// symbol: `rudis_get_render_cache_status` — the pure poll of the timeline
    /// render cache's state. Deliberately an EXPORT and not a seventh event
    /// type (59-CONTEXT D-30): retrieval rides Phase 50 D-06's 100 ms cold-path
    /// poll, exactly as waveform peaks, filmstrip strips and proxy status
    /// already do, and `ring::EVENT_NAMES` stays at 6. That is the FOURTH time
    /// this pattern has been used and the fourth time the note has had to be
    /// written; read the array's current value at execution time and add to it,
    /// per the note above. Nothing in v8 CALLS it — D-34 freezes both shells —
    /// so the export existing and being pinned end-to-end over the real ABI is
    /// the deliverable; rendering it is post-cutover shell work.
    ///
    /// Phase 60.1 (plan 60.1-03, PROJ-01/02) moved this **+3**, adding the
    /// create/open family: `rudis_new_project`, `rudis_open_project` and
    /// `rudis_open_project_at_path`. This is the first delta in this list that
    /// is not a read: the shipped WinUI app could not save or open a project
    /// at all, because `run_new_project`/`run_open_project` have been
    /// implemented and tested since Phase 26 and had had NO HOST since GATE-07
    /// deleted `src-tauri`. **No event type went with it** — `ProjectSwitched`
    /// already rides `project:changed` and `ShellMirror.ApplyAsync` routes
    /// structural kinds to a full resync, so `ring::EVENT_NAMES` stays at 6 for
    /// the sixth consecutive phase. The delta was read off this array at
    /// execution time and added to, per the note above; a concurrent session
    /// was live in the tree while this plan ran, which is precisely the trap
    /// the 53.2 paragraph records.
    ///
    /// Phase 60.1 (plan 60.1-03, PROJ-01/04/05) moved it a SECOND time, **+4**,
    /// adding the save and read family: `rudis_save_project`,
    /// `rudis_save_project_as`, `rudis_get_projects` and
    /// `rudis_get_missing_media`. Two commits, two honest deltas — which is
    /// exactly what "read the value at execution time and add to it" looks like
    /// when one plan lands in two pieces; the +4 was added to the 36 this array
    /// actually held when the second commit was written, not to any number in
    /// the plan. `rudis_get_projects` is the export the ROADMAP's scope item
    /// names, and it calls `run_get_projects_DETAILED`: the agent's
    /// `run_get_projects` is a minimal-disclosure view (T-26-08) that withholds
    /// the filesystem path on purpose, and a person choosing between two
    /// projects both called "Untitled" needs exactly the path and the mtime
    /// that posture withholds. One export, two contracts, neither widened.
    /// `rudis_get_missing_media` is a POLL and not the seventh event type, for
    /// the fifth time in this list — so `ring::EVENT_NAMES` is still 6 after
    /// both of this plan's deltas.
    ///
    /// Phase 69 (plan 69-02, OSS-01, D-69-12) moved this **+2**, adding the
    /// provider-key pair `rudis_set_provider_key` / `rudis_clear_provider_key`
    /// — ADDITIVE twins of the Anthropic key exports (which stay byte-unchanged)
    /// for the Settings surface's Runway key, allow-listed to `{"runway"}` in
    /// app-core. Read off this array at execution time (43) and added to. No
    /// event type went with it: `ring::EVENT_NAMES` stays at 6.
    ///
    /// The count is deliberately in the ARRAY'S OWN TYPE, so growing the list
    /// without updating the number does not compile. Stated that way rather
    /// than by repeating the literal type: this plan's acceptance gate counts
    /// occurrences of it to prove exactly one exists, and a doc comment
    /// echoing it would make that count meaningless (52-02's recorded rule
    /// about use-detector greps).
    #[test]
    fn export_registry_matches_the_locked_surface() {
        let locked: [&str; 45] = [
            "rudis_abi_probe",
            "rudis_agent_send_message",
            "rudis_agent_status",
            "rudis_apply_option_card",
            "rudis_clear_api_key",
            // Phase 69, plan 69-02 (OSS-01, D-69-12) — the provider-key pair.
            "rudis_clear_provider_key",
            "rudis_debug_mark_interactive",
            "rudis_debug_seed_project",
            "rudis_dispatch_command",
            "rudis_export_timeline",
            "rudis_free_buffer",
            "rudis_get_current_seq",
            "rudis_get_entities",
            "rudis_get_filmstrip_strip",
            "rudis_get_missing_media",
            "rudis_get_playback_position",
            "rudis_get_playback_resolution_level",
            "rudis_get_projects",
            "rudis_get_proxy_status",
            "rudis_get_render_cache_status",
            "rudis_get_snapshot",
            "rudis_get_waveform_peaks",
            "rudis_import_media",
            "rudis_import_media_folder",
            "rudis_init",
            "rudis_new_project",
            "rudis_open_project",
            "rudis_open_project_at_path",
            "rudis_place_clip",
            "rudis_poll_events",
            "rudis_preview_attach_panel",
            "rudis_preview_content_rect",
            "rudis_preview_detach_panel",
            // Phase 63, plan 63-02 (TRUST-01) — the device-lost trio. See
            // `tests/export_table.rs`'s LOCKED_EXPORT_COUNT history for the
            // delta's rationale and for why `EVENT_NAMES` is still 6.
            "rudis_preview_device_status",
            "rudis_preview_recover_device",
            "rudis_preview_resize",
            "rudis_preview_simulate_device_lost",
            "rudis_redo",
            "rudis_save_project",
            "rudis_save_project_as",
            "rudis_set_api_key",
            "rudis_set_provider_key",
            "rudis_shutdown",
            "rudis_transport",
            "rudis_undo",
        ];
        let mut registry: Vec<&str> = export::FFI_EXPORTS
            .iter()
            .copied()
            .filter(|name| name.starts_with("rudis_"))
            .collect();
        registry.sort_unstable();
        assert_eq!(
            registry,
            locked.as_slice(),
            "the ABI surface is locked at these {} symbols (D-01/D-04); a \
             missing name means an export was dropped, an extra one means \
             something grew the surface outside the plan",
            locked.len()
        );
    }

    /// (b) init/shutdown round-trip + the InvalidHandle and bad-input paths
    /// (T-47-05: UTF-8 and JSON are validated at entry, never trusted).
    #[test]
    fn init_shutdown_round_trip_and_invalid_inputs() {
        // Null/empty config → a real ctx backed by per-instance temp dirs.
        let ctx = rudis_init(std::ptr::null(), 0);
        assert!(!ctx.is_null(), "null config must yield a temp-dir-backed ctx");
        {
            // Unit tests share the crate, so the opaque struct is inspectable
            // here (and ONLY here — the ABI side sees an opaque pointer).
            let inner = unsafe { &*ctx };
            assert!(inner.data_dir.is_dir(), "data_dir exists on disk");
            assert!(inner.cache_dir.is_dir(), "cache_dir exists on disk");
            assert!(inner.resource_dir.is_dir(), "resource_dir exists on disk");
            assert_ne!(inner.data_dir, inner.cache_dir, "dirs are per-purpose");
            assert_eq!(inner._temp_dirs.len(), 3, "all three dirs are owned temps");
        }
        assert_eq!(rudis_shutdown(ctx), RudisStatus::Ok);

        // Null handle → InvalidHandle, never a crash.
        assert_eq!(
            rudis_shutdown(std::ptr::null_mut()),
            RudisStatus::InvalidHandle
        );

        // Invalid UTF-8 → null (str::from_utf8 path).
        let bad_utf8 = [0xFFu8, 0xFE, 0xFD];
        assert!(rudis_init(bad_utf8.as_ptr(), bad_utf8.len()).is_null());

        // Valid UTF-8, invalid JSON → null.
        let bad_json = b"not json at all";
        assert!(rudis_init(bad_json.as_ptr(), bad_json.len()).is_null());

        // Explicit config → the given dir is used, the rest are temps.
        let dir = tempfile::tempdir().expect("test dir");
        let cfg = serde_json::json!({ "data_dir": dir.path() }).to_string();
        let ctx = rudis_init(cfg.as_ptr(), cfg.len());
        assert!(!ctx.is_null(), "explicit config must parse");
        {
            let inner = unsafe { &*ctx };
            assert_eq!(inner.data_dir, dir.path());
            assert_eq!(inner._temp_dirs.len(), 2, "only the unconfigured dirs are temps");
        }
        assert_eq!(rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// (c) `rudis_abi_probe` round-trip: status Ok, the buffer parses as
    /// `{"Ok": ABI_PROBE}`, the buffer frees cleanly — and freeing an
    /// all-zero `RudisBuffer` is a no-op.
    #[test]
    fn abi_probe_round_trip_and_zero_buffer_free_is_noop() {
        // Null out-slot → NullPointer.
        assert_eq!(rudis_abi_probe(std::ptr::null_mut()), RudisStatus::NullPointer);

        let mut buf = RudisBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        };
        assert_eq!(rudis_abi_probe(&mut buf), RudisStatus::Ok);
        assert!(!buf.ptr.is_null());
        let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        let value: serde_json::Value =
            serde_json::from_slice(bytes).expect("probe envelope is valid JSON");
        assert_eq!(value, serde_json::json!({ "Ok": ABI_PROBE }));
        rudis_free_buffer(buf);

        // The documented all-zero no-op (what a zeroed C# struct hands us).
        rudis_free_buffer(RudisBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        });
    }
}
