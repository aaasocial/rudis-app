//! The [`AppCtx`] trait: the synchronous managed-state surface the ~45
//! domain/agent-tool functions need, abstracting the HOST's own handle types
//! away from the logic itself. The live implementor is `crates/ffi`'s
//! `FfiAppCtx`; the trait was designed against the retired Tauri shell's
//! `AppHandle<R>` / `State<'_, T>`, which is why the notes below name them.
//!
//! # Generic-only, never `dyn`
//!
//! Callers take `&impl AppCtx` / `<C: AppCtx>`, matching this codebase's own
//! precedent — `agent_llm::LlmTransport` (`crates/agent-llm/src/transport.rs`)
//! is used only as `fn run_turn<T: LlmTransport>(..)`, never as
//! `dyn LlmTransport`, and therefore needs no `async-trait` crate.
//!
//! The trait's FIRST future-returning method arrived in 45-10, not 45-11 as this
//! doc originally forecast: [`AppCtx::run_blocking`], for the async UI export
//! path. Still no `async-trait` dependency — but NOT the predicted `async fn`
//! under `#[allow(async_fn_in_trait)]`. It returns a
//! `Pin<Box<dyn Future<..> + Send>>`, and that shape was arrived at by
//! elimination against a real compiler error rather than by taste. See the
//! method's own doc for the three forms that were tried and why the first two
//! fail.
//!
//! # `emit_patch` reproduces `emit_changed` EXACTLY — this is load-bearing
//!
//! The Tauri-backed impl (landing in `src-tauri` with the first batch that
//! calls a moved function, 45-05) MUST construct the same **flattened**
//! envelope `src-tauri`'s `emit_changed` constructs today (Phase 43, LAT-02):
//!
//! ```text
//! #[derive(serde::Serialize)]
//! struct ProjectChangedEnvelope<'a> {
//!     #[serde(flatten)]  // <-- NOT nested
//!     patch: &'a Patch,
//!     base_seq: u64,
//!     seq: u64,
//! }
//! ```
//!
//! On the wire that is `{"kind": .., "ids": [..], <"entities": [..],>
//! "base_seq": .., "seq": ..}`. Nesting it as `{"patch": {..}, "base_seq": ..}`
//! would break the two `native_surface.rs` `PROJECT_CHANGED_EVENT` listeners —
//! one of which feeds `edit_affects_single_layer`, the live-preview flush
//! decision. Both parse the payload as a bare `rudis_core::Patch` behind a
//! `let Ok(..) else { return }`, so a nested shape is swallowed **silently**:
//! live preview and the canvas overlay stop updating with no compile error and
//! no runtime log anywhere.

use crate::project_store::ActiveProjectMeta;
use crate::SharedStore;

/// The host capabilities a migrated `run_*` / `handle_*` function needs.
///
/// Deliberately minimal in 45-03. [`AppCtx::active_project_meta`] was added in
/// 45-06, the batch that first needed it (`run_new_project` / `run_open_project`
/// / `run_get_projects`); [`AppCtx::export_progress_sink`] and
/// [`AppCtx::run_blocking`] in 45-10; and [`AppCtx::agent_session`] in 45-12,
/// for [`crate::dispatch::undo_inner`] — the first moved function with no
/// `&Mutex<AgentSession>` parameter of its own to carry it. Each method arrives
/// in the batch that actually needs it, never speculatively.
pub trait AppCtx {
    /// The single backend-owned store (`Mutex<rudis_core::Store>`).
    ///
    /// Callers lock it themselves and must treat a poisoned mutex as a
    /// recoverable `Err`, exactly as `src-tauri`'s `lock()` helper does today —
    /// a poisoned store means a prior handler panicked mid-mutation, and
    /// unwinding again would take the app down.
    fn store(&self) -> &SharedStore;

    /// The per-app persistent data directory (projects, exports, feedback).
    /// `Err` rather than panic when the host cannot resolve it.
    fn app_data_dir(&self) -> Result<std::path::PathBuf, String>;

    /// The per-app cache directory (posters, preview frames, probe cache).
    /// `Err` rather than panic when the host cannot resolve it.
    fn app_cache_dir(&self) -> Result<std::path::PathBuf, String>;

    /// Resolve `path` against the app's READ-ONLY BUNDLED RESOURCE root (plan
    /// 45-08) — a third host directory, distinct from the two above: it ships
    /// inside the installed app rather than being written at runtime.
    ///
    /// Added for [`crate::overlay`]'s `run_get_overlay_library` /
    /// `resolve_overlay_library_asset`, whose bundled overlay catalog lives
    /// there. `TauriAppCtx` implements it as the literal pre-move call,
    /// `app.path().resolve(path, tauri::path::BaseDirectory::Resource)`; the
    /// Phase 47 C ABI / C# host maps it to its own bundled-asset root.
    ///
    /// # Why this one returns the RAW host error, unlike its two siblings
    ///
    /// [`AppCtx::app_data_dir`] and [`AppCtx::app_cache_dir`] each bake in the
    /// exact message their one production consumer used to build inline. That
    /// worked because each has one consumer with one message — and it went
    /// wrong exactly once, when `app_cache_dir` shipped for two batches with a
    /// guessed string nothing consumed (45-07 fixed it as a Rule-1 defect).
    ///
    /// Here the message (`"resolve bundled overlay-library dir: {e}"`) names
    /// the *overlay library*, not "a resource", so baking it into a general
    /// accessor would be wrong for the next consumer by construction. Instead
    /// this returns the host's own error text and the call site re-wraps it —
    /// which yields a byte-identical surfaced string, since `format!("{e}")`
    /// over a `String` is the same text `format!("{e}")` over the host error
    /// produced.
    fn resolve_resource(&self, path: &str) -> Result<std::path::PathBuf, String>;

    /// Emit `project:changed` after a successful mutation.
    ///
    /// The implementation MUST reproduce `emit_changed`'s FLATTENED envelope
    /// exactly (Phase 43, LAT-02) — see this module's doc comment for why a
    /// nested shape fails silently. `base_seq` is `Store::seq` BEFORE the
    /// mutation, `seq` is the value AFTER.
    fn emit_patch(
        &self,
        patch: &rudis_core::Patch,
        base_seq: u64,
        seq: u64,
    ) -> Result<(), String>;

    /// Which `.rud` file the currently-active project autosaves to (plan 45-06).
    ///
    /// `src-tauri` `.manage()`s exactly one
    /// [`ActiveProjectMeta`](crate::project_store::ActiveProjectMeta) at app
    /// setup, so the Tauri impl is `app.state::<ActiveProjectMeta>().inner()` —
    /// which PANICS if that state was never managed, exactly as the
    /// `app.state::<project_store::ActiveProjectMeta>()` calls inside
    /// `run_new_project`/`run_open_project`/`run_get_projects` did before the
    /// move. Behavior on an unmanaged state is therefore unchanged.
    ///
    /// Returned by reference (not cloned) because the interior `Mutex` IS the
    /// synchronization point: two callers must observe the same lock, and
    /// `run_new_project` reads it (hook a) and writes it (the swap) around the
    /// same store mutation.
    fn active_project_meta(&self) -> &ActiveProjectMeta;

    /// The ONE Chat conversation state (`Mutex<AgentSession>`) the host manages
    /// (plan 45-12).
    ///
    /// # Why this arrives now and not at 45-04
    ///
    /// [`crate::AgentSession`] relocated in 45-04, but every function moved
    /// since then took an explicit `&Mutex<AgentSession>` parameter of its own,
    /// so no trait method was warranted — `ctx.rs`'s own doc said as much.
    /// [`crate::dispatch::undo_inner`] is the first that cannot: its
    /// `#[tauri::command]` wrapper `undo` is reduced to a ctx construction, and
    /// a second `&Mutex<AgentSession>` parameter alongside `&C` would be exactly
    /// the host-shaped plumbing this trait exists to absorb. 45-13's
    /// `run_agent_turn` cluster REUSES this accessor rather than adding its own.
    ///
    /// Returned by reference for the same reason
    /// [`AppCtx::active_project_meta`] is: the interior `Mutex` IS the
    /// synchronization point. `undo_inner` takes it strictly AFTER dropping the
    /// store guard (lock order store → session, sequential, never nested), which
    /// is what keeps it deadlock-free against `run_agent_turn`'s session → store
    /// spans.
    ///
    /// `src-tauri` `.manage()`s exactly one `Mutex<AgentSession>` in
    /// `configure()`, so the Tauri impl is
    /// `app.state::<Mutex<AgentSession>>().inner()` — which PANICS if that state
    /// was never managed, exactly as the `State<'_, Mutex<AgentSession>>`
    /// parameter it replaces would have failed the command. Every app, real or
    /// mock, goes through `configure()`.
    fn agent_session(&self) -> &std::sync::Mutex<crate::AgentSession>;

    /// Drive an `async` seam to completion from a SYNCHRONOUS caller (plan
    /// 45-07). Added for [`crate::import::run_import_media`], which stays
    /// synchronous by design (43-06) while the folder walk it shares with the UI
    /// command is `async`.
    ///
    /// # This is a HOST capability, deliberately, and that is the whole point
    ///
    /// It would have been easy to hard-code a block-on primitive inside this
    /// crate. Every candidate is wrong:
    ///
    /// * `pollster::block_on` enters NO Tokio context, so the
    ///   `tokio::task::spawn_blocking` calls nested inside the walk would panic
    ///   with *"must be called from the context of a Tokio 1.x runtime"*.
    /// * `tokio::runtime::Handle::current().block_on(..)` panics when no runtime
    ///   is entered — which is exactly the situation the agent tool path runs in
    ///   (`src-tauri`'s own tests drive `run_agent_turn` through
    ///   `pollster::block_on`).
    /// * A private global `tokio::runtime::Runtime` owned by `app-core` would
    ///   work, but it would be a SECOND runtime with its own blocking pool —
    ///   a real mechanism change, which Phase 45 forbids.
    ///
    /// Handing it to the host keeps the production mechanism byte-identical:
    /// `TauriAppCtx::block_on` is literally `tauri::async_runtime::block_on`,
    /// the call this replaced, so the runtime entered (and therefore the pool
    /// `spawn_blocking` resolves against) is unchanged. Registered threat
    /// T-45-05 is mitigated by construction rather than by testing.
    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output;

    /// An OWNED, `Send + 'static` `export:progress` emitter (plan 45-10).
    ///
    /// Added for [`crate::export`]'s `run_export_blocking`, whose
    /// [`engine::ProgressFn`] closure is handed to the encoder and outlives the
    /// call — a BORROWED `&impl AppCtx` provably cannot produce one, which is
    /// the exact blocker deferred item **D-45-06-01** named when it moved
    /// `run_export_project` from 45-06 to this batch.
    ///
    /// # Why an owned SINK rather than an `emit_export_progress(pct)` method
    ///
    /// The encoder takes ownership of an `FnMut(f64) + Send` and calls it from
    /// the ffmpeg `-progress` reader, on a thread that outlives this call.
    /// Nothing borrowed can satisfy that. Returning the sink (rather than
    /// widening [`AppCtx`] to be `Send + Sync + 'static`, which would force an
    /// owning `TauriAppCtx` on every OTHER call site in the crate) keeps the
    /// requirement exactly where it belongs: on the one capability that needs
    /// it.
    ///
    /// The Tauri impl is literally the closure `run_export_blocking` built
    /// inline before the move — `let app = app.clone(); Box::new(move |pct| {
    /// let _ = app.emit(EXPORT_PROGRESS_EVENT, pct); })` — so the event name,
    /// the payload type and the swallowed-`Result` error handling are
    /// unchanged. The terminal 100% guarantee `run_export`/`run_export_project`
    /// emit after the encode is the SAME sink called once, so it too lands on
    /// the identical event.
    fn export_progress_sink(&self) -> engine::ProgressFn;

    /// Job-lifecycle event emitter (`gen:job` / `gen:progress`), an OWNED
    /// `Send + Sync + 'static` closure the spawned poll task captures — the
    /// [`AppCtx::export_progress_sink`] pattern applied to generation (plan
    /// 54.1-03).
    ///
    /// **NEVER an RPC channel: no caller may read these events back to learn an
    /// outcome** (54.1-RESEARCH Pitfall 1); outcomes ride the task's own return
    /// value. The pattern this replaces — subscribe to `gen:job` before submit,
    /// then scan the captured payloads once the poll task joined — used Tauri's
    /// event bus as an in-process return channel, which has no Tauri-free
    /// equivalent and is the one coupling this phase had to redesign rather than
    /// move. See
    /// [`poll_job_until_terminal`](crate::generation_host::poll_job_until_terminal)
    /// (returns its terminal [`agent_gen::JobStatus`]) and
    /// [`finalize_ready_job`](crate::generation_host::finalize_ready_job) (the
    /// ONE landing + terminal-emit path).
    ///
    /// The name is one of the two frozen wire constants
    /// ([`GEN_JOB_EVENT`](crate::GEN_JOB_EVENT) /
    /// [`GEN_PROGRESS_EVENT`](crate::GEN_PROGRESS_EVENT)) and the payload is the
    /// serialized [`GenJobEventPayload`](crate::GenJobEventPayload) /
    /// [`GenProgressEventPayload`](crate::GenProgressEventPayload) — the C#
    /// shell and the Tauri renderer both read that shape, so it does not move.
    /// Emission failure is swallowed by every implementation, exactly as
    /// `app.emit`'s `Result` was: a closed window must never abort a running
    /// job's bookkeeping.
    ///
    /// Owned (`Arc`), not borrowed, for the same reason
    /// [`AppCtx::export_progress_sink`] is: the background poll task is
    /// `'static` and outlives this call, so nothing borrowed from `&self` can
    /// satisfy it. `Arc` rather than `Box` because BOTH the spawned task and the
    /// awaiting finalizer need one.
    fn gen_event_sink(&self) -> crate::generation_host::GenEventSink;

    /// Run a BLOCKING closure on the host's blocking pool and await its result
    /// (plan 45-10). `Err` carries the host's RAW join-error text.
    ///
    /// Added for [`crate::export`]'s `run_export`, the async UI export path.
    ///
    /// # This is a HOST capability for the same reason [`AppCtx::block_on`] is
    ///
    /// The call being replaced is `tauri::async_runtime::spawn_blocking(..)`,
    /// and substituting `tokio::task::spawn_blocking` here would NOT be
    /// behavior-preserving. Tauri's wrapper owns a process-global runtime and
    /// dispatches onto it unconditionally; `tokio::task::spawn_blocking`
    /// resolves against the AMBIENT runtime and **panics when there is none**.
    /// `src-tauri`'s `export_project_gate` drives `run_export` under
    /// `pollster::block_on`, which enters no runtime at all — so the swap would
    /// turn a green gate red for a reason that has nothing to do with the
    /// export. (This is the one place 45-07's `spawn_blocking` substitution,
    /// deferred item D-45-07-01, would have been a real regression rather than
    /// a changed diagnostic string.)
    ///
    /// Keeping the primitive host-side means `TauriAppCtx::run_blocking` is
    /// literally the pre-move call, so the runtime, the pool and the join
    /// semantics are unchanged.
    ///
    /// # Why the RAW error text, like [`AppCtx::resolve_resource`]
    ///
    /// Its one consumer wraps the failure as `"export task panicked: {e}"`,
    /// which names *the export* — wrong to bake into a general accessor, and
    /// baking a guessed message in is precisely what made `app_cache_dir`
    /// wrong for two batches. `format!("{e}")` over the returned `String` is
    /// the same text `format!("{e}")` over the host's error produced, so the
    /// surfaced message is byte-identical.
    ///
    /// # Why a BOXED `'static` future, not an `async fn` and not RPITIT
    ///
    /// Three forms were tried against the one call site that matters —
    /// `#[tauri::command] async fn export_timeline`, whose future Tauri
    /// requires to be `Send` for the HIGHER-RANKED lifetime its
    /// `State<'_, SharedStore>` parameter introduces:
    ///
    /// 1. `async fn run_blocking(&self, ..)` — compiles here, then fails at
    ///    `export_timeline` with *"implementation of `Send` is not general
    ///    enough: `Send` would have to be implemented for `&AppHandle<R>` … but
    ///    is actually implemented for `&'0 AppHandle<R>`, for some specific
    ///    lifetime"*. An `async fn` in a trait desugars to a future universally
    ///    quantified over the `&self` lifetime, and there is no stable way for a
    ///    generic caller to bound it (return-type notation is unstable).
    /// 2. `-> impl Future<..> + Send` (RPITIT), with and without `+ 'static` —
    ///    the SAME error. RPITIT captures every in-scope lifetime implicitly, so
    ///    the opaque type still carries `&self`'s, and declaring `Send` on it
    ///    does not discharge the higher-ranked obligation.
    /// 3. `-> Pin<Box<dyn Future<..> + Send>>` — compiles. `Box<dyn Trait>`
    ///    defaults to `+ 'static`, so the returned future provably borrows
    ///    NOTHING from `self` and the higher-ranked question never arises.
    ///
    /// The cost is one `Box` allocation per export — against a multi-minute
    /// FFmpeg encode, unmeasurable. Note that
    /// [`AppCtx::block_on`]/[`crate::import::walk_import_roots`] do NOT hit this:
    /// an `async fn` taking `&C` is fine; it is specifically a TRAIT method
    /// returning a future that is not.
    fn run_blocking<T, F>(
        &self,
        f: F,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static;
}

/// What [`SpendPolicy::spend_confirmation_gate`] decided for ONE prospective
/// paid call — the `app-core` mirror of `src-tauri`'s
/// `generation::SpendGateDecision` (Phase 42.3 item (F)).
///
/// Two variants only, on purpose: there is no "warn and proceed" middle ground
/// for money. The variant names and the `question` field mirror the source
/// enum EXACTLY, so the `match` in `src-tauri`'s [`SpendPolicy`] impl is a pure
/// re-tag with no reworded string and no dropped arm — and a new variant added
/// over there becomes a compile error here rather than a silent fall-through
/// that spends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpendGateDecision {
    /// The caller carries evidence of user consent — go ahead and submit.
    Proceed,
    /// Halt and ask. The message names the tool and the resolved model so the
    /// user knows exactly what they are approving.
    NeedsConfirmation {
        /// The verbatim question the halted turn puts to the user.
        question: String,
    },
}

/// The pre-spend consent policy + paid-model disclosure resolver
/// `run_agent_turn`'s meta loop consults before dispatching a `generate_ai_*`
/// tool call (plan 45-12, for 45-13's mover).
///
/// # Why a trait rather than four copied functions
///
/// `45-RESEARCH.md` recommended simply DUPLICATING `generation.rs`'s pure
/// helpers into this crate, on the grounds that they "take no Tauri-specific
/// type at all" — which is true of their signatures and beside the point.
/// All of them are exercised by ~100 lines of `generation.rs`'s OWN
/// `#[cfg(test)]` module, and `resolved_model_for_tool_input` calls two of the
/// others internally. Duplicating the code without duplicating the tests
/// orphans that coverage; relocating the tests means editing the one 10k-line
/// file Phase 45's boundary puts explicitly out of scope. So this is the same
/// answer [`crate::GenSubmission`] gave in 45-11 — put the coupling on a trait
/// and forward, one line per method, from inside the crate `pub(crate)` already
/// scopes to. `src-tauri/src/generation.rs` changes by ZERO lines (threat
/// T-45-07, verified by an empty `git diff --stat`).
///
/// # Sync, unlike [`crate::GenSubmission`]
///
/// Both targets are plain synchronous `serde_json::Value`-in / value-out
/// functions today, so there is no future to shape and 45-10's
/// `Pin<Box<dyn Future + Send>>` question never arises. Generic-only, never
/// `dyn`, matching `agent_llm::LlmTransport` like every other trait here.
///
/// # Why TWO methods and not the four the plan named
///
/// The closure was measured against the tree rather than taken from the plan
/// text, and two of the four named helpers do not belong here:
///
/// * **The shape/stage translation was ALREADY on
///   [`crate::GenSubmission`]** (45-11), with the identical signature. A second
///   copy on this trait would have made the call ambiguous (E0034) for 45-13's
///   `<C: AppCtx + GenSubmission + SpendPolicy>` bound — the exact call it
///   existed to serve.
/// * **The video capability resolver had no `src-tauri/src/lib.rs` PRODUCTION
///   caller.** It was reached transitively, inside `generation.rs`, from
///   `resolved_model_for_tool_input`'s own body; `lib.rs` named it only from
///   three `#[cfg(test)]` sites. Add a method in the batch that first needs it,
///   never speculatively — 45-03's standing rule, and 45-11's precedent for
///   declining an unread fake.
///
/// **Both bullets are HISTORICAL as of Phase 55.1 plan 06**, which deleted the
/// translation, the resolver and the whole capability layer they belonged to.
/// The reasoning is kept because it is why this trait has two methods rather
/// than four, and that shape did not change when the callees died.
pub trait SpendPolicy {
    /// Phase 42.3 (F): the ONE pre-spend confirmation policy. `approved` is the
    /// caller's evidence of user consent — the agent surface passes
    /// [`crate::AgentSession::spend_approved_turn`], set ONLY by the resume of a
    /// spend-confirmation `PendingAskUser` (T-42.3-12). Fail-closed: no
    /// evidence, no spend.
    ///
    /// Every implementor is a one-line forward to
    /// [`crate::spend_confirmation_gate`] — same arguments, same order, same
    /// question string. That is not a convention, it is the point: ONE policy
    /// body and ONE [`SpendGateDecision`] enum in the workspace, so a
    /// "confirmed" spend on one host cannot become an unconfirmed one on
    /// another (ROADMAP Backlog § 999.8's named trap).
    ///
    /// **Phase 55.1 (D-01/D-08/D-12): `resolved_cost` is the cap dimension**
    /// § 999.8 asked for — the advisory roster's own price line for the model,
    /// or the literal [`crate::PRICE_UNKNOWN`] when it has none, or `None` for a
    /// spend with no model-cost concept. Build it with
    /// [`crate::cost_signal_for_tool_input`] from the SAME input the dispatch
    /// bills; never from a second lookup, and never from a guessed figure.
    fn spend_confirmation_gate(
        &self,
        tool_name: &str,
        resolved_model: Option<&str>,
        resolved_cost: Option<&str>,
        approved: bool,
    ) -> SpendGateDecision;

    /// The model a `generate_ai_*` tool call actually ran, resolved from the
    /// tool's OWN input — what the user-facing disclosure reports, and what
    /// [`SpendPolicy::spend_confirmation_gate`] names in its question so the
    /// thing approved is the thing billed (T-42.3-13).
    ///
    /// `None` for a tool with no model concept, or for an input the seam would
    /// have rejected. `TauriAppCtx`'s impl is a one-line forward to
    /// `generation::resolved_model_for_tool_input`, which re-runs the SAME
    /// resolvers the seams do — so the disclosure cannot drift from what ran
    /// (T-42.3-11).
    fn resolved_model_for_tool_input(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<String>;
}

/// The agent-facing VISION surface `run_agent_turn` assembles a fresh turn's
/// image block(s) from (plan 45-13). The phase's FOURTH trait.
///
/// # Why a bridge and not a mover
///
/// Exactly the argument [`crate::GenSubmission::resolve_reference_image`] made
/// in 45-11 — and about the very same code, since that resolver awaits these
/// two builders. `build_vision_snapshot_block` / `build_whiteboard_snapshot_block`
/// sit on a ~600-line `src-tauri` cluster (`resolve_active_snapshot_frame`,
/// `draw_annotations_onto_styled`, `clamp_raster_dims`, `OVERLAY_INK`,
/// `WHITEBOARD_BG`, `blank_whiteboard_frame`, and the `_png`/`_jpeg` byte
/// producers) that is shared with `native_surface.rs`'s live-preview ink
/// overlay and with the GEN-10 reference-image seam, and pinned by ~20
/// byte-identity tests. Relocating all of it inside the phase's largest single
/// batch would have violated Phase 45's own SC-2 rule that a batch which cannot
/// keep the suite green is too large. A later batch can collapse these methods
/// into direct calls; `whiteboard_raster_dims` cannot, because the
/// `Mutex<WhiteboardAspect>` behind it is Tauri managed state that only the
/// windowed app registers.
///
/// # Native `async fn` — TESTED, not assumed
///
/// 45-10 established that a TRAIT method returning a future needs
/// `Pin<Box<dyn Future + Send>>` when the future lives inside a
/// `#[tauri::command] async fn`'s state machine, and 45-11 established that a
/// native `async fn` is fine when it does not. These methods are in 45-10's
/// category on paper — `agent_send_message` (a `#[tauri::command] async fn`
/// with `State<'_, SharedStore>`) awaits `run_agent_turn`, which awaits these.
/// So the native form was NOT assumed: a throwaway probe of exactly that shape
/// was built and compiled before any code moved, and it is accepted. The
/// distinction from `AppCtx::run_blocking` is that the `Send` obligation here is
/// discharged at the CONCRETE instantiation the command creates, rather than
/// higher-ranked over `&self`'s lifetime the way `State<'_, T>` forced it to be
/// for the boxed method. Generic-only, never `dyn`, like every trait here.
#[allow(async_fn_in_trait)]
pub trait AgentVision {
    /// The FRAME vision snapshot for a fresh Chat turn: the active preview
    /// frame with every frame-linked canvas mark drawn on it, JPEG-encoded and
    /// wrapped as an Anthropic `image` content block.
    ///
    /// `None` when the canvas carries no frame-linked marks, when no frame
    /// resolves, or when the decode/encode fails — a failed snapshot degrades
    /// the turn to text-only, it never fails it (T-13-16). `TauriAppCtx`'s impl
    /// is LITERALLY `build_vision_snapshot_block(project).await`.
    async fn vision_snapshot_block(
        &self,
        project: &rudis_core::Project,
    ) -> Option<agent_llm::ContentBlock>;

    /// The WHITEBOARD vision snapshot: every project-global whiteboard mark
    /// rasterized onto a blank `raster_w x raster_h` board, PNG-encoded and
    /// wrapped as an Anthropic `image` content block. Same graceful-`None`
    /// discipline as [`AgentVision::vision_snapshot_block`].
    ///
    /// `TauriAppCtx`'s impl is LITERALLY
    /// `build_whiteboard_snapshot_block(project, raster_w, raster_h).await`.
    async fn whiteboard_snapshot_block(
        &self,
        project: &rudis_core::Project,
        raster_w: u32,
        raster_h: u32,
    ) -> Option<agent_llm::ContentBlock>;

    /// The dimensions [`AgentVision::whiteboard_snapshot_block`] should raster
    /// at (Phase 14.3, D-03): the live Canvas-panel aspect the frontend last
    /// reported, clamped to a 1280px long edge.
    ///
    /// This one is a HOST capability permanently, unlike its two siblings: the
    /// `Mutex<WhiteboardAspect>` mirror is managed by `native_surface::setup`,
    /// i.e. ONLY by the windowed app, so `TauriAppCtx`'s impl keeps the literal
    /// `try_state`-or-default read plus `clamp_raster_dims(.., .., 1280)`. Every
    /// mock-runtime app therefore still gets the retired board's 1280x720
    /// fallback, exactly as before the move.
    fn whiteboard_raster_dims(&self) -> (u32, u32);
}
