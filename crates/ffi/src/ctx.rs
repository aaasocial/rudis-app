//! [`FfiAppCtx`] — the production `app_core::AppCtx` implementation over an
//! opaque [`RudisCtx`](crate::RudisCtx) (plan 47-04).
//!
//! Structurally mirrors `app-core`'s `TestAppCtx` (the proven tauri-free
//! shape: per-instance dirs, lazy `OnceLock` multi-thread runtime for
//! `block_on`, thread-spawn `run_blocking`) — but where `TestAppCtx` records
//! emissions into in-memory `Vec`s for assertions, this host pushes them into
//! the ctx's bounded [`EventRing`](crate::ring::EventRing), which is what the
//! C# side polls (D-09/D-10/D-11).
//!
//! `FfiAppCtx`'s trait impls are monomorphized independently of the shell
//! host's (RESEARCH A4): `dispatch_command_inner(&ffi_ctx, ..)` resolves
//! `ctx.emit_patch(..)` to THIS file's ring push with zero runtime
//! relationship to any existing emit site — which is how this whole plan
//! touches nothing outside `crates/ffi`.
//!
//! # Generation, vision and spend approval (Phase 54.1 — D-03's PLANNED exit)
//!
//! Phase 47's D-03 note here said an FFI-hosted turn is text-only, that
//! generation must fail closed unconditionally, and that reaching
//! `src-tauri/src/generation.rs`'s `pub(crate)` functions would be an
//! **unplanned** extraction. **Phase 54.1 is the PLANNED extraction D-03
//! anticipated**, and it went the other way round: nothing here reaches into
//! `src-tauri` — the host glue MOVED OUT of it into `crates/app-core`
//! (`generation_host.rs`, plans 54.1-01/03), where one definition of the spend
//! gate, the three `generate_*_for_agent` seams, the job lifecycle and the
//! landing bridge now serve BOTH hosts. Every method below is a forward to
//! them (SC-5: one policy, one path). That claim used to be checked by
//! `src-tauri/tests/single_generation_path.rs`, which was deleted together with
//! that shell at Phase 55 (GATE-07) and has no successor — this crate is now the
//! ONLY host, so "one path" is structural rather than scanned.
//!
//! **Fail-closed is preserved exactly where it matters, and nowhere else:**
//!
//! * **No spend without approval.** [`app_core::spend_confirmation_gate`] is
//!   the ONE gate; `approved` (set solely by `build_user_turn`'s resume of a
//!   genuine spend-confirmation `PendingAskUser`, T-42.3-12) is still the only
//!   path to `Proceed`. A configured provider changes nothing about that — the
//!   contract tier proves the refusal in the failing direction WITH a provider
//!   present, because a refusal that only held for lack of a provider would
//!   prove nothing.
//! * **No provider without a key.** An unkeyed slot resolves to `None` and the
//!   seam returns the honest `NO_*_CONFIGURED` message (31-03's
//!   honest-absence pattern) — never a fixture substitute in a shipped build.
//! * **No unbounded reference payload.** The reference-image arms carry the
//!   same DoS caps and the same error strings as the Tauri host's.
//!
//! The vision impl (plan 54.1-02) keeps its own already-tested degradation
//! path: a failed/absent snapshot degrades a turn to text-only, never fails it
//! (T-13-16).

use crate::ring;
use crate::RudisCtx;
use app_core::project_store::ActiveProjectMeta;
use app_core::SharedStore;

/// A borrow-shaped `AppCtx` view over one [`RudisCtx`] — the exact pattern of
/// the shell's own ctx adapter (a cheap wrapper constructed per call, handed
/// to `&impl AppCtx` call sites). 47-05's exports build one from the deref'd
/// handle at the top of every command body.
pub struct FfiAppCtx<'a>(&'a RudisCtx);

impl<'a> FfiAppCtx<'a> {
    pub fn new(ctx: &'a RudisCtx) -> Self {
        Self(ctx)
    }

    /// Bundle this instance's five generation-host fields for the shared
    /// `app_core` seams (Phase 54.1, plan 04) — the exact counterpart of
    /// `src-tauri`'s `generation::gen_host_from_app`, minus the managed-state
    /// lookup: here they are plain struct fields on [`RudisCtx`], so every slot
    /// is always `Some` and the seams' `"… state is not managed"` arms are
    /// structurally unreachable through this host.
    ///
    /// Cheap and LAZY on purpose: this borrows the slots, it never calls
    /// `.resolve_via()`. Firing the credential read for all three modalities on
    /// every seam call — or before the prompt has even been validated — would
    /// undo LAT-05. The seam resolves its OWN slot, after validation.
    ///
    /// Phase 69: carries the ctx-owned provider key store (the ONLY path the
    /// slots resolve through) and is `pub(crate)` so the Settings exports in
    /// `commands.rs` can invalidate slots after a set/clear.
    pub(crate) fn gen_host(&self) -> app_core::GenHost<'_> {
        app_core::GenHost {
            image: Some(&self.0.gen_image_provider),
            video: Some(&self.0.gen_video_provider),
            audio: Some(&self.0.gen_audio_provider),
            jobs: Some(&self.0.gen_jobs),
            allow_list: Some(&self.0.gen_allow_list),
            stores: Some(&self.0.provider_key_store),
        }
    }
}

/// The `project:changed` wire envelope, mirroring the shell's
/// `ProjectChangedEnvelope` EXACTLY — `#[serde(flatten)]`, never nesting.
///
/// ⚠ Nesting this (`{"patch": {..}, "base_seq": ..}`) is the exact
/// silent-parse-failure Phase 43 (43-02) found: both `native_surface.rs`
/// listeners parse the payload as a bare `rudis_core::Patch` behind
/// `let Ok(..) else { return }`, so a nested shape is swallowed with no
/// compile error and no log — live preview and the canvas overlay just stop
/// updating. STATE.md's carry-forward names it Phase 47's #1 hazard; the
/// flattened shape is proven here by `ctx.rs`'s bare-`Patch`-parse unit test.
/// On the wire: `{"kind": .., "ids": [..], <"entities": [..],> "base_seq": ..,
/// "seq": ..}`.
#[derive(serde::Serialize)]
struct ProjectChangedEnvelope<'a> {
    #[serde(flatten)]
    patch: &'a rudis_core::Patch,
    base_seq: u64,
    seq: u64,
}

impl app_core::AppCtx for FfiAppCtx<'_> {
    fn store(&self) -> &SharedStore {
        &self.0.store
    }

    fn app_data_dir(&self) -> Result<std::path::PathBuf, String> {
        Ok(self.0.data_dir.clone())
    }

    fn app_cache_dir(&self) -> Result<std::path::PathBuf, String> {
        Ok(self.0.cache_dir.clone())
    }

    fn resolve_resource(&self, path: &str) -> Result<std::path::PathBuf, String> {
        // TestAppCtx's faithful analogue: resolve against the ctx's bundled-
        // resource root. Phase 50 configures a real directory here; an
        // unconfigured one resolves to a real, absent path rather than
        // erroring — identical to a headless mock app.
        Ok(self.0.resource_dir.join(path))
    }

    fn emit_patch(
        &self,
        patch: &rudis_core::Patch,
        base_seq: u64,
        seq: u64,
    ) -> Result<(), String> {
        let envelope = ProjectChangedEnvelope {
            patch,
            base_seq,
            seq,
        };
        let payload = serde_json::to_value(&envelope)
            .map_err(|e| format!("serialize project:changed envelope: {e}"))?;
        self.0.ring.push(ring::EVENT_PROJECT_CHANGED, payload);
        Ok(())
    }

    fn active_project_meta(&self) -> &ActiveProjectMeta {
        &self.0.active_project_meta
    }

    fn agent_session(&self) -> &std::sync::Mutex<app_core::AgentSession> {
        &self.0.agent_session
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        // TestAppCtx's byte-pattern — a REAL multi-threaded runtime, built
        // lazily, because `import_one_path` calls `tokio::task::spawn_blocking`
        // — a bare future poller panics there (RESEARCH E1). PLUS
        // `.enable_all()` (CR-01), which the test twin deliberately lacks:
        // this host wires `rudis_agent_send_message` to the real
        // `agent_llm::AnthropicTransport`, whose `reqwest` client needs the
        // I/O driver (hyper's connect path) and the timer driver (reqwest's
        // default connection-pool idle timeout arms real timers) — on a
        // driver-less runtime every keyed Chat turn panics the moment it
        // reaches the network. Both drivers are compiled in via this crate's
        // own graph (agent-llm → reqwest → tokio `net`/`time`), so
        // `enable_all()` enables both. `TestAppCtx::block_on`
        // (app-core/src/test_support.rs) keeps the driver-less shape ON
        // PURPOSE — its tests only ever drive `FixtureTransport`, never real
        // I/O — so the twins differ here by design; do not "sync" this back.
        self.0
            .runtime
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("FfiAppCtx tokio runtime builds")
            })
            .block_on(fut)
    }

    fn export_progress_sink(&self) -> engine::ProgressFn {
        // An OWNED Send + 'static closure — the same shape the shell host
        // hands the encoder, which keeps it on a thread that outlives this
        // call. The Arc clone is what makes ownership possible.
        let ring = std::sync::Arc::clone(&self.0.ring);
        Box::new(move |pct| {
            let _ = ring.push(ring::EVENT_EXPORT_PROGRESS, serde_json::json!(pct));
        })
    }

    fn gen_event_sink(&self) -> app_core::GenEventSink {
        // Plan 54.1-03: the `export_progress_sink` template above, one surface
        // over — an OWNED `Send + Sync + 'static` closure the spawned poll task
        // keeps for its whole life. The Arc clone is what makes ownership
        // possible.
        //
        // `ring::EVENT_GEN_JOB` / `EVENT_GEN_PROGRESS` have been RESERVED
        // constants in the D-02 closed `EVENT_NAMES` set since Phase 47, which
        // anticipated exactly this: no `ring.rs` change is needed, only a real
        // push into the already-declared tags.
        //
        // The closed match is the point. The ABI's `event` field is a
        // `&'static str` from that six-name set, so an unknown name must NOT
        // invent a seventh tag on the wire — it is dropped, loudly in a debug
        // build (a new lifecycle event needs a ring tag AND a C# consumer, both
        // deliberate acts) and silently in release, matching every other
        // emitter's "a closed window never aborts a running job's bookkeeping"
        // discipline.
        let ring = std::sync::Arc::clone(&self.0.ring);
        std::sync::Arc::new(move |name: &'static str, payload: serde_json::Value| {
            let tag = match name {
                app_core::GEN_JOB_EVENT => ring::EVENT_GEN_JOB,
                app_core::GEN_PROGRESS_EVENT => ring::EVENT_GEN_PROGRESS,
                other => {
                    debug_assert!(
                        false,
                        "gen_event_sink got an unmapped event name '{other}' — add a \
                         ring::EVENT_* tag for it (and a C# consumer) rather than widening \
                         this match to a catch-all push"
                    );
                    return;
                }
            };
            let _ = ring.push(tag, payload);
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
        // TestAppCtx's thread-spawn+join shape, verbatim: equivalent for a
        // synchronous FFI caller — the export call blocks its own thread
        // either way, and the closure's panic surfaces as an `Err` rather
        // than unwinding the caller.
        Box::pin(async move {
            std::thread::spawn(f)
                .join()
                .map_err(|_| "blocking task panicked".to_string())
        })
    }
}

// Quick 260801-n7q: `MAX_MEDIA_REFERENCE_LONG_EDGE` and
// `decode_item_reference_png` were DELETED from here, and with them plan
// 54.1-02's "reproduces `src-tauri` behaviour-for-behaviour, error strings
// included" doc. Both now live ONCE, in `app_core::media_reference`; the two
// call sites below are unchanged and read through this shim.
//
// The reproduction discipline was not wrong, it was just weaker than it looked.
// Phase 54.1's `deferred-items.md` §1 said so at the time — "a future
// payload-size fix can land on one host only" — and then that happened: an
// oversized STILL was rejected where the identical picture sourced from a video
// was downscaled, burning paid generation attempts. A copy kept in sync by
// discipline is a copy. The shim IS the drift control now — and it is the ONLY
// one: `src-tauri/tests/single_generation_path.rs`, which used to enforce this
// mechanically, was deleted with that shell at Phase 55 (GATE-07).
use app_core::media_reference::decode_item_reference_png;

/// The SOURCE timestamp of the last frame a clip actually puts on screen.
///
/// Reproduces `src-tauri/src/lib.rs:923-938`'s `last_visible_frame_us`
/// byte-for-byte, epsilon included. NOT `out_us - 1`: `out_us` is EXCLUSIVE and
/// the decoder emits the first frame whose PTS is `>=` the requested position,
/// so `out_us - 1` resolves one frame PAST the cut. This indexes the SOURCE
/// media's own frame grid instead — the timestamp of the last frame whose PTS
/// is `< out_us` — because accumulating a rounded frame duration drifts a full
/// frame within a second at 30fps (a real bug the Tauri host's fixture test
/// caught). `fps <= 0` degenerates to `in_us`.
fn last_visible_frame_us(clip: &rudis_core::Clip, fps: f64) -> i64 {
    if !(fps > 0.0) {
        return clip.in_us;
    }
    let frames_before_out = ((clip.out_us as f64) * fps / 1_000_000.0 - 1e-3).ceil();
    let last_index = (frames_before_out as i64 - 1).max(0);
    let position_us = ((last_index as f64) * 1_000_000.0 / fps).round() as i64;
    position_us.max(clip.in_us)
}

/// **Real generation submission through the C ABI host** (Phase 54.1, plan 04).
///
/// Every method forwards to the ONE `app_core` implementation both hosts share
/// (SC-5) — the same function bodies `TauriAppCtx`'s impl calls, reached with a
/// [`app_core::GenHost`] built from this ctx's own plain struct fields rather
/// than from managed state (54.1-RESEARCH Pitfall 2). There is deliberately no
/// second policy, no second submit path and no second error vocabulary here:
/// the prompt caps, the GEN-08 allow-list gate, the honest `NO_*_CONFIGURED`
/// absence messages, the wait caps and the landing bridge all live in
/// `app_core::generation_host`.
impl app_core::GenSubmission for FfiAppCtx<'_> {
    async fn submit_image(
        &self,
        prompt: String,
        model: String,
        background: agent_gen::BackgroundMode,
        reference: Option<agent_gen::ReferenceImage>,
    ) -> Result<app_core::GeneratedAsset, String> {
        let generated = app_core::generate_runway_image_for_agent(
            self,
            &self.gen_host(),
            prompt,
            model,
            background,
            reference,
        )
        .await?;
        // The same two-field copy `TauriAppCtx::submit_image` does.
        Ok(app_core::GeneratedAsset {
            media_item_ids: generated.media_item_ids,
            carries_provenance_watermark: generated.carries_provenance_watermark,
        })
    }

    async fn submit_video(
        &self,
        prompt: String,
        model: String,
        reference: Option<agent_gen::ReferenceImage>,
        destination: Option<agent_gen::ReferenceImage>,
    ) -> Result<app_core::GeneratedAsset, String> {
        let generated = app_core::generate_runway_video_for_agent(
            self,
            &self.gen_host(),
            prompt,
            model,
            reference,
            destination,
        )
        .await?;
        Ok(app_core::GeneratedAsset {
            media_item_ids: generated.media_item_ids,
            carries_provenance_watermark: generated.carries_provenance_watermark,
        })
    }

    /// Phase 56 plan 06 (GEN-11) for this host: turn a store-validated timeline
    /// Clip id into trim-respecting bytes and typed references, ENTIRELY
    /// backend-side, then hand them to the ONE governed submit entry.
    ///
    /// This is the only method on this trait that reads the timeline, because it
    /// is the only one whose input is a CLIP rather than a prompt — and reading
    /// it is what makes D-02 true: the bytes are exactly what the clip currently
    /// SHOWS (`in_us..out_us`, `out_us` exclusive), never the underlying media.
    ///
    /// # The step order IS the contract, and every step's failure is $0.00
    ///
    /// 1. the reference gate — pure, and before the store is even locked;
    /// 2. ONE SHORT store lock: the clip's coordinates AND its media item copied
    ///    out together, the same discipline
    ///    [`Self::resolve_reference_image`]'s `Clip` arm follows, so no guard is
    ///    ever held across decode work;
    /// 3. the D-01 window check, on the VISIBLE length;
    /// 4. the extraction — the first step that costs CPU;
    /// 5. reference resolution through the EXISTING method (zero new raster
    ///    code, so a Canvas-annotated frame would work by construction — D-09);
    /// 6. `app_core::generate_runway_video_edit_for_agent`, the ONE submit entry.
    ///
    /// Step 5 is **unreachable today**: step 1 refuses any non-empty set,
    /// because no probe has named the reference field (F-1b enumerated both
    /// candidates and crowned neither; 56-09 is the remaining route). The call
    /// is written in the order the contract specifies anyway, so unblocking
    /// references is a change at the GATE rather than a re-plumb here.
    ///
    /// `model` appears below ONLY as a pass-through argument. Nothing here
    /// inspects, matches on, validates or rewrites it (55.1 decision 2);
    /// resolution — including the advisory default for a model-less call — is
    /// the governed entry's, so there is exactly one place that decides.
    ///
    /// No clip bytes and no reference bytes cross the C ABI boundary
    /// (T-34.1-09 / CLAUDE.md rule 4): they are produced here and consumed
    /// in-process by the seam.
    async fn submit_video_edit(
        &self,
        prompt: String,
        source_clip_id: String,
        model: Option<String>,
        references: Vec<app_core::ReferenceSource>,
    ) -> Result<app_core::GeneratedAsset, String> {
        // (1) The cheapest structural refusal, before the store lock and before
        //     any raster or decode work can be reached.
        app_core::video_edit_reference_check(references.len())?;

        // (2) ONE short lock: copy out the clip's coordinates AND its media item
        //     so no guard is held across the extraction (slow, blocking work).
        let (media_path, in_us, out_us, fps) = {
            let guard = <Self as app_core::AppCtx>::store(self)
                .lock()
                .map_err(|_| "backend store mutex poisoned".to_string())?;
            let clip = app_core::compose::timeline_clip(guard.timeline(), &source_clip_id)
                .cloned()
                .ok_or_else(|| {
                    format!("no clip with id '{source_clip_id}' on the timeline to edit")
                })?;
            let item = guard.media_item(&clip.media_id).cloned().ok_or_else(|| {
                format!(
                    "clip '{source_clip_id}' references media '{}', which is not in the media bin",
                    clip.media_id
                )
            })?;
            (item.path, clip.in_us, clip.out_us, item.fps)
        };

        // (3) D-01, at the ONE refusal site 56-04 built. Its message reaches the
        //     caller verbatim — it names both bounds and offers a DIFFERENT
        //     remedy per edge, and re-wording it here would be a second story
        //     about the same state.
        app_core::clip_edit_window_check(out_us - in_us)?;

        // (4) D-02: the trim-respecting, out_us-EXCLUSIVE, fps-capped, audio-free
        //     range. `block_in_place` rather than an inline call — this writes a
        //     temp file and spawns an ffmpeg PROCESS, which is the same class of
        //     work `start_generation_job` guards the landing bridge for, and
        //     neither belongs on an async worker. Outside a runtime it simply
        //     runs the closure.
        let path = std::path::PathBuf::from(media_path);
        let source_video = tokio::task::block_in_place(|| {
            app_core::extract_clip_range_mp4(&path, in_us, out_us, fps)
        })?;

        // (5) The EXISTING resolution path, sequentially, first Err aborting —
        //     so `ReferenceSource::Frame` (the Canvas-annotated preview frame,
        //     D-09) would ride it with zero new raster code. Unreachable while
        //     step 1 refuses any non-empty set.
        let mut resolved = Vec::with_capacity(references.len());
        for source in references {
            resolved.push(self.resolve_reference_image(source).await?);
        }

        // (6) The ONE governed entry. `model` passes through UNTOUCHED.
        let generated = app_core::generate_runway_video_edit_for_agent(
            self,
            &self.gen_host(),
            prompt,
            model,
            source_video,
            resolved,
        )
        .await?;
        Ok(app_core::GeneratedAsset {
            media_item_ids: generated.media_item_ids,
            carries_provenance_watermark: generated.carries_provenance_watermark,
        })
    }

    async fn submit_audio(&self, prompt: String) -> Result<app_core::GeneratedAsset, String> {
        let generated =
            app_core::generate_elevenlabs_audio_for_agent(self, &self.gen_host(), prompt).await?;
        Ok(app_core::GeneratedAsset {
            media_item_ids: generated.media_item_ids,
            carries_provenance_watermark: generated.carries_provenance_watermark,
        })
    }

    /// Phase 34.1 (GEN-10) for this host: resolve a chosen
    /// [`app_core::ReferenceSource`] to real PNG bytes, entirely backend-side,
    /// AT TOOL-CALL TIME on a FRESH store snapshot (Pattern 2 — never the
    /// possibly-stale turn-start vision snapshot).
    ///
    /// Arm-for-arm the behaviour of `src-tauri/src/lib.rs:847-897`, with the
    /// SAME error strings: the Sketch/Frame sources reuse the EXACT Pattern-1
    /// byte producers this host's agent vision uses ([`crate::vision`], plan
    /// 54.1-02), so what conditions the generation is byte-identical to what
    /// the agent saw; Media/Clip read an existing library item. A source that
    /// has nothing to give is an HONEST `Err` naming which source was empty —
    /// never a silent unconditioned fallback. No reference bytes ever cross the
    /// C ABI boundary (T-34.1-09 / CLAUDE.md rule 4): they are produced here and
    /// consumed by the seam in-process.
    async fn resolve_reference_image(
        &self,
        source: app_core::ReferenceSource,
    ) -> Result<agent_gen::ReferenceImage, String> {
        let store = <Self as app_core::AppCtx>::store(self);
        match source {
            app_core::ReferenceSource::Sketch => {
                let project = store
                    .lock()
                    .map_err(|_| "backend store mutex poisoned".to_string())?
                    .snapshot();
                // The SAME (rw, rh) recipe `run_agent_turn` uses for THIS host's
                // whiteboard vision snapshot — `AgentVision::whiteboard_raster_dims`,
                // i.e. plan 54.1-02's MEASURED source (the Preview panel's
                // contain-fit content rect) — so the reference raster tracks the
                // box the user actually drew in.
                let (rw, rh) = <Self as app_core::AgentVision>::whiteboard_raster_dims(self);
                let (bytes, w, h) = crate::vision::whiteboard_snapshot_png(&project, rw, rh)
                    .await
                    .ok_or_else(|| {
                        "no canvas sketch to use as a reference -- draw something on the Canvas first"
                            .to_string()
                    })?;
                Ok(agent_gen::ReferenceImage {
                    bytes,
                    width: w,
                    height: h,
                })
            }
            app_core::ReferenceSource::Frame => {
                let project = store
                    .lock()
                    .map_err(|_| "backend store mutex poisoned".to_string())?
                    .snapshot();
                // PNG, lossless — deliberately NOT the JPEG history sibling: this
                // is what a paid provider conditions on, not chat history.
                let (bytes, w, h) = crate::vision::vision_snapshot_png(&project)
                    .await
                    .ok_or_else(|| {
                        "no annotated preview frame to use as a reference -- draw on the current frame first"
                            .to_string()
                    })?;
                Ok(agent_gen::ReferenceImage {
                    bytes,
                    width: w,
                    height: h,
                })
            }
            // Resolved ONLY through the store's own validated ids — never a
            // caller-supplied filesystem path (T-34.1-02: no raw path field
            // exists on the tool schema, so an unknown id is an honest `Err`,
            // never an arbitrary read). Position 0 = the item's first frame,
            // this source's pre-existing behaviour.
            app_core::ReferenceSource::Media(media_id) => {
                let item = {
                    let guard = store
                        .lock()
                        .map_err(|_| "backend store mutex poisoned".to_string())?;
                    guard.media_item(&media_id).cloned().ok_or_else(|| {
                        format!("no media item with id '{media_id}' to use as a reference")
                    })?
                };
                decode_item_reference_png(&item, &media_id, 0)
            }
            // ONE short lock: copy out the clip's coordinates AND its media item,
            // so no guard is held across the decode (slow, blocking work).
            app_core::ReferenceSource::Clip(clip_id, edge) => {
                let (item, position_us) = {
                    let guard = store
                        .lock()
                        .map_err(|_| "backend store mutex poisoned".to_string())?;
                    let clip = app_core::compose::timeline_clip(guard.timeline(), &clip_id)
                        .cloned()
                        .ok_or_else(|| {
                            format!(
                                "no clip with id '{clip_id}' on the timeline to use as a reference"
                            )
                        })?;
                    let item = guard.media_item(&clip.media_id).cloned().ok_or_else(|| {
                        format!(
                            "clip '{clip_id}' references media '{}', which is not in the media bin",
                            clip.media_id
                        )
                    })?;
                    let position_us = match edge {
                        app_core::ClipEdge::Start => clip.in_us,
                        app_core::ClipEdge::End => last_visible_frame_us(&clip, item.fps),
                    };
                    (item, position_us)
                };
                decode_item_reference_png(&item, &clip_id, position_us)
            }
        }
    }

    // The fifth `GenSubmission` method — the shape/stage → capability mapping the
    // dispatch path and the disclosure resolver used to share (T-42.3-11) — was
    // DELETED with the trait method itself in Phase 55.1 plan 06. This impl was a
    // one-line forward onto `app_core`, and the sharing it protected is now
    // structural rather than delegated: both readers take the caller's own
    // `input["model"]`, so there is one string and nothing to keep in sync.
}

/// **The ONE spend policy, reached from the C ABI host** (Phase 54.1, plan 04).
///
/// Both methods are single-line forwards onto `app_core`, byte-for-byte the
/// shape `TauriAppCtx`'s impl has carried since plan 54.1-01. That is SC-5 made
/// structural rather than advisory: a "confirmed" spend on one surface cannot
/// become an unconfirmed one on the other, because there is exactly one policy
/// body and exactly one [`app_core::SpendGateDecision`] enum in the workspace
/// (Backlog § 999.8's named trap).
///
/// **This host can now execute the spend it gates**, which is precisely why the
/// gate must be real: `approved` — set ONLY by `build_user_turn`'s resume of a
/// genuine spend-confirmation `PendingAskUser` (T-42.3-12), never by tool input,
/// rulebook text or model output — is still the only path to `Proceed`.
impl app_core::SpendPolicy for FfiAppCtx<'_> {
    fn spend_confirmation_gate(
        &self,
        tool_name: &str,
        resolved_model: Option<&str>,
        resolved_cost: Option<&str>,
        approved: bool,
    ) -> app_core::SpendGateDecision {
        // Phase 55.1 (D-12): `resolved_cost` is forwarded, never recomputed —
        // this host has no pricing opinion of its own, and a second lookup here
        // is precisely how the question and the bill would drift apart.
        app_core::spend_confirmation_gate(tool_name, resolved_model, resolved_cost, approved)
    }

    fn resolved_model_for_tool_input(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<String> {
        // Re-runs the SAME resolvers the seams do, so the disclosure cannot
        // claim one model while another was billed (T-42.3-11).
        app_core::resolved_model_for_tool_input(tool_name, input)
    }
}

/// **The agent can SEE through this host** (Phase 54.1, plan 54.1-02).
///
/// Phase 47 shipped this impl as an unconditional degrade-to-`None` under D-03
/// — an honest stub for a host that could not yet reach a snapshot builder.
/// It now delegates to [`crate::vision`], the reproduction-exact port of
/// `src-tauri`'s snapshot cluster, so the FFI host degrades EXACTLY where the
/// shell host does and nowhere else: no marks means no image (T-13-15's
/// token discipline), and any decode/encode failure still falls back to a
/// text-only turn rather than failing it (T-13-16).
///
/// Neither snapshot reads a screen pixel: both resolve from `Project` state,
/// decode media from disk and paint from `Project.canvas`. That is why this
/// needed **zero new C ABI exports** — see 54.1-RESEARCH Q5.
impl app_core::AgentVision for FfiAppCtx<'_> {
    async fn vision_snapshot_block(
        &self,
        project: &rudis_core::Project,
    ) -> Option<agent_llm::ContentBlock> {
        crate::vision::vision_snapshot_block(project).await
    }

    async fn whiteboard_snapshot_block(
        &self,
        project: &rudis_core::Project,
        raster_w: u32,
        raster_h: u32,
    ) -> Option<agent_llm::ContentBlock> {
        crate::vision::whiteboard_snapshot_block(project, raster_w, raster_h).await
    }

    fn whiteboard_raster_dims(&self) -> (u32, u32) {
        // MEASURED, not assumed — plan 54.1-02 Task 1's artifact
        // `54.1-02-canvas-surface-measurement.md` (verdict: CONFIRMED).
        //
        // The Tauri host reads a `Mutex<WhiteboardAspect>` fed by the
        // frontend's `canvas-surface-resize`, i.e. the CANVAS PANEL's size,
        // because in that shell the box the user draws in IS that panel. The
        // PREVIEW's ink surface here — `PreviewInkLayer`, a transparent sibling
        // in the Preview region's own Grid cell — instead normalizes every point
        // against the engine's contain-fit CONTENT RECT
        // (`CanvasGesture.Normalize(x, y, scale, rect)`), never against the
        // panel box. Since `draw_annotations_onto_styled` denormalizes with
        // `x * width, y * height`, the raster is undistorted iff its aspect is
        // the aspect of the box the coordinates came from — so for THAT surface
        // the correct source is `content_rect()`, and picking `target_w/target_h`
        // would letterbox the agent's board.
        //
        // ⚠ SINCE PHASE 60.2 THERE ARE TWO INK SURFACES, AND THIS FUNCTION
        // ANSWERS FOR ONLY ONE OF THEM. The text above used to say this shell
        // had exactly ONE; that premise is now FALSE. The Canvas STAGE
        // (`Canvas.xaml`/`Canvas.xaml.cs`, plan 60.2-02) is a second surface,
        // and it normalizes against its OWN panel box —
        // `CanvasStageGesture.NormalizeInBox(x, y, ActualWidth, ActualHeight)`,
        // written deliberately free of the Preview's rect/letterbox machinery
        // and of any engine-readiness gate, because drawing on the stage must
        // work with ZERO video loaded. This function still returns the PREVIEW's
        // content-rect aspect, for both.
        //
        // The consequence, stated so a later reader cannot miss it: stage-drawn
        // marks rasterize into the agent's board POSITIONALLY CORRECT but
        // ANISOTROPICALLY STRETCHED whenever the stage box's aspect differs from
        // the preview's. Normalized coordinates survive (a mark at the middle of
        // the stage is at the middle of the board); shapes do not (a square drawn
        // on a wide stage arrives as a rectangle).
        //
        // This is an ACCEPTED, RECORDED limitation, not an oversight and not a
        // bug to fix in passing. Owner-visible record:
        // `.planning/phases/60.2-canvas-stage-drawing/deferred-items.md` § D-1
        // (written at plan 60.2-05; already named in `.planning/ROADMAP.md`'s
        // Phase 60.2 entry). Correcting it means feeding the STAGE's box size
        // down here, which is a NEW ABI push mirroring `rudis_preview_resize` —
        // a future phase's work, never a drive-by edit of this function.
        //
        // `(0,0,0,0)` until a composite has actually happened. For the PREVIEW
        // that is exactly the state in which its ink layer REFUSES to record a
        // gesture, so the fallback used to fire precisely when no aligned mark
        // could exist. ⚠ THAT IS ALSO NO LONGER TRUE OF THE STAGE: it has no such
        // gate by design, so with no media loaded a real whiteboard mark can
        // exist while this returns the fallback. `(1280, 720)` is the same
        // mock-app fallback the Tauri host has (the retired board's aspect), and
        // it is the aspect every stage mark is rasterized at until a composite
        // publishes a rect — which is the D-1 distortion above, in its most
        // common form rather than an edge case.
        let (_, _, w, h) = self.0.preview_surface.content_rect();
        if w == 0 || h == 0 {
            return (1280, 720);
        }
        crate::vision::clamp_raster_dims(w, h, 1280)
    }
}

/// The compile check that matters (plan 47-04 Task 3): instantiating
/// `run_agent_turn`'s generic — with its full
/// `C: AppCtx + GenSubmission + SpendPolicy + AgentVision` bound — against
/// `FfiAppCtx` proves the 4-trait requirement is satisfied BEFORE 47-05
/// writes the `agent_send_message` export. Never called; compiling IS the
/// assertion.
#[cfg(test)]
#[allow(dead_code)]
fn assert_agent_turn_callable(
    ctx: &FfiAppCtx<'_>,
    session: &std::sync::Mutex<app_core::AgentSession>,
    transport: &agent_llm::FixtureTransport,
    library_dir: &std::path::Path,
) {
    let _future = app_core::run_agent_turn(
        ctx,
        session,
        transport,
        String::new(),
        Vec::new(),
        library_dir,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InitConfig;
    use app_core::AppCtx;
    use app_core::{GenSubmission, SpendPolicy};
    use rudis_core::{Patch, PatchKind};

    fn ctx() -> RudisCtx {
        RudisCtx::new_in_process(
            InitConfig::default(),
            Box::new(agent_llm::InMemoryKeyStore::new()),
        )
        .expect("in-process ctx builds")
    }

    /// The flattened-envelope proof (the 43-02 hazard, asserted): the ring
    /// payload DESERIALIZES AS A BARE `rudis_core::Patch`, with `base_seq`/
    /// `seq` present as SIBLINGS — and no nested `"patch"` key anywhere.
    #[test]
    fn emit_patch_pushes_the_flattened_project_changed_envelope() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);

        let patch = Patch {
            kind: PatchKind::ClipMoved,
            ids: vec!["clip-1".to_string(), "clip-2".to_string()],
            entities: None,
        };
        ffi.emit_patch(&patch, 7, 8).expect("emit_patch ok");

        let polled = ctx.ring.poll(0);
        assert!(!polled.resync_required);
        assert_eq!(polled.events.len(), 1, "exactly one record pushed");
        let record = &polled.events[0];
        assert_eq!(record.event, ring::EVENT_PROJECT_CHANGED);

        // 1. Parses as a BARE Patch — what both native_surface listeners do
        //    behind `let Ok(..) else { return }`.
        let reparsed: Patch = serde_json::from_value(record.payload.clone())
            .expect("the payload parses as a bare Patch — the flattened shape");
        assert_eq!(reparsed, patch, "the patch round-trips unchanged");

        // 2. base_seq/seq are SIBLINGS of the patch fields, not a wrapper.
        assert_eq!(record.payload.get("base_seq"), Some(&serde_json::json!(7)));
        assert_eq!(record.payload.get("seq"), Some(&serde_json::json!(8)));
        assert!(
            record.payload.get("kind").is_some(),
            "the patch's own fields sit at the top level"
        );
        assert!(
            record.payload.get("patch").is_none(),
            "NO nested wrapper key — nesting is the 43-02 silent parse failure"
        );
    }

    /// The export-progress sink is an owned closure that lands ordered
    /// `export:progress` records in the ring — the encoder-thread shape.
    #[test]
    fn export_progress_sink_pushes_ordered_records_into_the_ring() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);

        let mut sink = ffi.export_progress_sink();
        sink(0.25);
        sink(0.5);
        sink(1.0);

        let polled = ctx.ring.poll(0);
        assert_eq!(polled.events.len(), 3);
        for (record, expected) in polled.events.iter().zip([0.25, 0.5, 1.0]) {
            assert_eq!(record.event, ring::EVENT_EXPORT_PROGRESS);
            assert_eq!(record.payload, serde_json::json!(expected));
        }
    }

    /// D-07: two in-process instances share NOTHING — separate stores,
    /// separate rings, separate directories.
    #[test]
    fn two_instances_are_fully_independent() {
        let a = ctx();
        let b = ctx();
        let ffi_a = FfiAppCtx::new(&a);
        let ffi_b = FfiAppCtx::new(&b);

        assert_ne!(
            ffi_a.app_data_dir().expect("a"),
            ffi_b.app_data_dir().expect("b"),
            "per-instance data dirs"
        );

        let patch = Patch {
            kind: PatchKind::ClipMoved,
            ids: vec!["clip-a".to_string()],
            entities: None,
        };
        ffi_a.emit_patch(&patch, 1, 2).expect("emit into a only");

        assert_eq!(a.ring.poll(0).events.len(), 1, "a's ring saw its emit");
        assert!(
            b.ring.poll(0).events.is_empty(),
            "b's ring saw NOTHING — the streams are per-instance (D-07)"
        );

        // Stores are independent too: both fresh, both lockable, seq 0.
        assert_eq!(ffi_a.store().lock().expect("a store").seq(), 0);
        assert_eq!(ffi_b.store().lock().expect("b store").seq(), 0);
    }

    /// `block_on` enters a REAL Tokio runtime (RESEARCH E1): `spawn_blocking`
    /// inside the driven future must not panic — the exact property
    /// `import_one_path` depends on.
    #[test]
    fn block_on_enters_a_real_runtime_and_run_blocking_joins_off_thread() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);

        let via_spawn_blocking = ffi.block_on(async {
            tokio::task::spawn_blocking(|| 21 * 2)
                .await
                .expect("spawn_blocking joins")
        });
        assert_eq!(via_spawn_blocking, 42);

        let joined = ffi.block_on(ffi.run_blocking(|| "off-thread".to_string()));
        assert_eq!(joined.as_deref(), Ok("off-thread"));
    }

    /// CR-01's regression gate: the lazily-built runtime must carry the I/O
    /// AND timer drivers, because `rudis_agent_send_message` drives
    /// `agent_llm::AnthropicTransport`'s real `reqwest` client through this
    /// exact `block_on` — hyper's connect path needs the I/O driver, and
    /// reqwest's default connection pool arms real idle timers. A live-key
    /// round trip is forbidden (offline core, CLAUDE.md rule 5) — which is
    /// exactly how the contract tier missed this — so this drives the DRIVERS
    /// themselves with ZERO network egress: a 1 ms timer future and a
    /// loopback-only TCP bind. On a driver-less runtime BOTH panic (Tokio's
    /// own diagnostics: "..IO is disabled. Call `enable_io`.." / "there is no
    /// timer running.."), so this test is red without `.enable_all()` — a
    /// builder-shape assertion would pass on a broken runtime; driving real
    /// futures cannot.
    #[test]
    fn block_on_runtime_has_io_and_timer_drivers_enabled() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);

        // Timer driver: panics without `.enable_time()`/`.enable_all()`.
        // ⚠ The sleep is CONSTRUCTED inside the async block, not as
        // `block_on`'s argument — `tokio::time::sleep` grabs the runtime
        // handle eagerly at creation, so building it on the bare test thread
        // panics "no reactor running" even on a fully-driven runtime.
        ffi.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        });

        // I/O driver: bind an OS-assigned port on loopback, connect nothing,
        // send nothing. Panics without `.enable_io()`/`.enable_all()`.
        let local = ffi.block_on(async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("loopback bind on an OS-assigned port");
            listener.local_addr().expect("bound listener has a local addr")
        });
        assert!(local.ip().is_loopback(), "bound strictly to loopback");
        assert_ne!(local.port(), 0, "the OS assigned a real port");
    }

    // ------------------------------------------------------------------
    // Task 3: the three fail-closed traits (D-03), asserted — not assumed.
    // ------------------------------------------------------------------

    use app_core::AgentVision;

    fn resp(content: Vec<agent_llm::ContentBlock>, stop_reason: &str) -> agent_llm::MessagesResponse {
        agent_llm::MessagesResponse {
            id: "msg_test".to_string(),
            role: agent_llm::Role::Assistant,
            content,
            stop_reason: Some(stop_reason.to_string()),
            usage: agent_llm::Usage::default(),
        }
    }

    /// A NONEXISTENT growable-library dir (`load_library` on a missing dir is
    /// documented empty) — the same convention as the shell's own gate tests.
    fn lib_dir() -> std::path::PathBuf {
        std::env::temp_dir().join("rudis-ffi-agent-no-library")
    }

    // ------------------------------------------------------------------
    // Plan 54.1-04: the fixture-provider recipe, copied from `src-tauri`'s
    // `generation.rs mod agent_tool` (`admit_image_model` / `build_app`) so the
    // two hosts' contract tiers admit the SAME row for the SAME reason.
    //
    // ⚠ ZERO network, zero spend: `FixtureGenProvider` holds no socket-capable
    // field (31-RESEARCH Pitfall 4). No test in this file may ever construct a
    // real `KeyringStore` or a real provider — the live tier is plan 54.1-05's,
    // and it is checkpoint-gated.
    // ------------------------------------------------------------------

    /// The GEN-08 admit row for the fixture provider.
    ///
    /// **Phase 55.1 (D-10) inverted what this proves.** It used to prove the
    /// SEAM chose the model server-side: the row was derived by resolving the
    /// pinned image capability word through the intent table, so admitting it
    /// could only succeed if the seam — not the caller — had submitted that id
    /// (both the word and the table are deleted as of plan 06). The model
    /// is now a REQUIRED caller field, so the row is derived from the SAME
    /// [`IMAGE_MODEL`] literal the scripted tool inputs name, and a green test
    /// proves the opposite direction: the CALLER's string is what the gate saw.
    /// The two cannot drift because there is one constant.
    ///
    /// The fixture provider is deliberately still GATED — only `runway` is in
    /// `agent_gen::allow_list::UNGATED_PROVIDERS` (55.1-02) — which is what
    /// makes the gate usable here as a reporter of the model id it was handed.
    fn admit_fixture_image_model() -> agent_gen::AllowList {
        agent_gen::AllowList::new(vec![agent_gen::AllowListEntry::new(
            agent_gen::FIXTURE_PROVIDER_ID,
            IMAGE_MODEL,
        )])
    }

    /// The image model these tests drive — a CALLER value since Phase 55.1, so a
    /// literal rather than a lookup into the retiring intent table.
    const IMAGE_MODEL: &str = "gen4_image";

    fn image_model() -> &'static str {
        IMAGE_MODEL
    }

    /// The REAL `test-media/still.png` bytes — the exact fixture `src-tauri`'s
    /// `mod agent_tool` landing tests use. Deliberately not synthetic PNG magic:
    /// `land_generated_asset` PROBES the written file with `ffprobe`, so invalid
    /// bytes never land and a "landing" assertion over them would be vacuous.
    fn fixture_still_bytes() -> Vec<u8> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-media/still.png");
        std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
    }

    /// A zero-network image provider that returns the real still on submit
    /// (`SubmitOutcome::Ready` — the degenerate zero-poll path).
    fn fixture_image_provider() -> std::sync::Arc<agent_gen::ConcreteGenProvider> {
        std::sync::Arc::new(agent_gen::ConcreteGenProvider::Fixture(
            agent_gen::FixtureGenProvider::sync_still(fixture_still_bytes(), "png"),
        ))
    }

    /// A ctx whose IMAGE provider slot IS configured (with the fixture) and
    /// whose allow list admits it. Video/audio are preset to an explicit `None`
    /// so their absence is a property of the test, not of the developer's
    /// keychain.
    fn ctx_with_image_provider() -> RudisCtx {
        let mut ctx = ctx();
        ctx.preset_generation(
            Some(fixture_image_provider()),
            None,
            None,
            admit_fixture_image_model(),
        );
        ctx
    }

    /// Every `gen:job` payload currently in the ring, in order.
    fn gen_job_payloads(ctx: &RudisCtx) -> Vec<serde_json::Value> {
        ctx.ring
            .poll(0)
            .events
            .into_iter()
            .filter(|r| r.event == ring::EVENT_GEN_JOB)
            .map(|r| r.payload)
            .collect()
    }

    /// **SC-3, in the failing direction, with a provider CONFIGURED.**
    ///
    /// This is the Phase-47 fail-closed contract test, RE-AIMED in the same
    /// commit that made the traits real (never deleted — the two impls it pins
    /// changed underneath it, so the assertions had to change WITH them). It
    /// used to prove "this host cannot spend, ever". It now proves the strictly
    /// stronger claim the phase actually needs: **this host cannot spend WITHOUT
    /// APPROVAL — even when a provider is sitting right there, ready to bill.**
    ///
    /// That distinction is the whole point. A refusal asserted on a ctx with no
    /// provider proves nothing: it would stay green on a build whose spend gate
    /// had been deleted outright, because the submit would fail for the
    /// unrelated reason that nothing is configured. So part (a) presets the
    /// FIXTURE image provider and admits its model FIRST, and only then asserts
    /// the refusal.
    ///
    /// The three fail-closed properties that survive, each now for an HONEST
    /// reason rather than a blanket D-03 refusal:
    ///
    /// (a)+(b) the gate refuses an unapproved spend with a provider present, and
    ///         returns `Proceed` only on real approval evidence;
    /// (c)     an UNKEYED slot (preset `None` — deterministic, never a read of
    ///         the developer's keychain) yields the honest per-modality
    ///         `NO_*_CONFIGURED` message, not a fixture substitute;
    /// (d)     the model resolver is real, so the question can name what will
    ///         actually be billed (T-42.3-13).
    ///
    /// ⚠ The AgentVision third of this test MOVED, it was not dropped: plan
    /// 54.1-02 made that impl real, so its assertions now live in
    /// [`vision_degrades_to_text_only_on_an_empty_project_and_falls_back_to_1280x720`]
    /// — where they still hold, because degrading on an EMPTY project is the
    /// T-13-16 contract rather than a stub artifact.
    #[test]
    fn unapproved_spend_refuses_even_with_a_provider_configured_and_unkeyed_submits_fail_closed() {
        // (a) A provider IS configured — the fixture image provider, with the
        //     GEN-08 row admitting exactly the model the seam would submit. A
        //     submit through this ctx would really run (the sibling landing test
        //     proves it does). Nothing below depends on absence.
        let keyed = ctx_with_image_provider();
        let ffi = FfiAppCtx::new(&keyed);

        // (b) Unapproved: the gate still refuses, and the question names the
        //     spend the user is being asked about. Asserted by CONTENT (three
        //     independent substrings), not by rebuilding the expected string
        //     from the function under test.
        let model = image_model();
        // Phase 55.1 (D-12): the cost the PRODUCTION path would hand this gate,
        // built by the same helper the agent loop uses — never a literal typed
        // here, so this test cannot claim a price the roster does not carry.
        let cost = app_core::cost_signal_for_model(model);
        match ffi.spend_confirmation_gate("generate_ai_image", Some(model), Some(cost), false) {
            app_core::SpendGateDecision::NeedsConfirmation { question } => {
                assert!(
                    question.contains("PAID call"),
                    "the question says it is a PAID call: {question}"
                );
                assert!(
                    question.contains("generate_ai_image"),
                    "the question names the tool being gated: {question}"
                );
                assert!(
                    question.contains(model),
                    "T-42.3-13: the question names the model that will actually be \
                     billed ({model}): {question}"
                );
                assert!(
                    question.contains(cost),
                    "T-55.1-08: ...and its PRICE, so the consent is to a specific spend \
                     ({cost}): {question}"
                );
            }
            app_core::SpendGateDecision::Proceed => panic!(
                "SC-3: an UNAPPROVED spend must never Proceed — and this ctx HAS a \
                 provider configured, so the refusal is about approval, not absence"
            ),
        }
        // ...and the gate is REAL, not a constant refusal: genuine approval
        // evidence (which only `build_user_turn`'s resume of a spend-confirmation
        // PendingAskUser can produce, T-42.3-12) does proceed.
        assert!(
            matches!(
                ffi.spend_confirmation_gate("generate_ai_image", Some(model), Some(cost), true),
                app_core::SpendGateDecision::Proceed
            ),
            "the gate is the real one — approval evidence is what unlocks the spend"
        );

        // (c) A SECOND ctx with all three slots preset to an EXPLICIT `None`.
        //     Preset (not production) on purpose: a production slot would read
        //     the machine's Credential Manager, so this assertion would depend on
        //     whether the developer happens to hold a Runway/ElevenLabs key. A
        //     preset `None` short-circuits `get_or_init` and can never reach a
        //     real `KeyringStore` (RESEARCH E2).
        let mut unkeyed = ctx();
        unkeyed.preset_generation(None, None, None, admit_fixture_image_model());
        let ffi = FfiAppCtx::new(&unkeyed);

        // Each modality names ITS OWN missing credential — a real absence
        // refusal from the shared seam, no longer a D-03 blanket. Asserted by
        // equality against the `app_core` consts, so the two hosts cannot tell
        // the user different stories about the same state (T-31-21/T-32-18).
        let err = ffi
            .block_on(ffi.submit_image(
                "p".to_string(),
                IMAGE_MODEL.to_string(),
                agent_gen::BackgroundMode::Auto,
                None,
            ))
            .err()
            .expect("submit_image refuses without a key");
        assert_eq!(err, app_core::NO_PROVIDER_CONFIGURED);
        let err = ffi
            .block_on(ffi.submit_video("p".to_string(), "gen4_turbo".to_string(), None, None))
            .err()
            .expect("submit_video refuses without a key");
        assert_eq!(err, app_core::NO_VIDEO_PROVIDER_CONFIGURED);
        let err = ffi
            .block_on(ffi.submit_audio("p".to_string()))
            .err()
            .expect("submit_audio refuses without a key");
        assert_eq!(err, app_core::NO_AUDIO_PROVIDER_CONFIGURED);

        // The Sketch reference source degrades honestly too: an empty project
        // has no ink, so the answer is the named-source error, never a silent
        // unconditioned fallback (34.1-RESEARCH Pattern 1).
        let err = ffi
            .block_on(ffi.resolve_reference_image(app_core::ReferenceSource::Sketch))
            .err()
            .expect("no ink, no sketch reference");
        assert_eq!(
            err,
            "no canvas sketch to use as a reference -- draw something on the Canvas first"
        );

        // (d) The disclosure resolver is REAL now — its D-03 `None` stub is gone.
        //     Phase 55.1: it reads the caller's own `model` field, so the input
        //     carries one. This used to assert a SECOND resolver beside it (the
        //     shape/stage → capability mapping, whose stub was also retired in
        //     54.1-02); plan 06 deleted that method from the trait, so there is
        //     one resolver here rather than two.
        let resolved = ffi
            .resolved_model_for_tool_input(
                "generate_ai_image",
                &serde_json::json!({ "prompt": "x", "model": IMAGE_MODEL }),
            )
            .expect("the image tool resolves a model for its disclosure");
        assert!(
            resolved.contains(model),
            "the disclosure names the model the seam submits ({model}): {resolved}"
        );
    }

    /// The AgentVision assertions RELOCATED out of the fail-closed test above
    /// (plan 54.1-02), in the same commit that made the impl real — and they
    /// pass against the REAL implementation, unchanged, which is the point.
    ///
    /// On an EMPTY project all three still hold: no frame-linked marks means no
    /// frame snapshot (zero decode), no whiteboard marks means no board, and no
    /// attached panel means no published content rect, so the raster dims are
    /// the mock-app `(1280, 720)` fallback. That is the T-13-16 degradation
    /// CONTRACT — "a failed/absent snapshot degrades the turn to text-only,
    /// never fails it" — not an artifact of the retired stub, and it must keep
    /// holding forever. The tests above it prove the non-degenerate half.
    #[test]
    fn vision_degrades_to_text_only_on_an_empty_project_and_falls_back_to_1280x720() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);

        let project = ffi.store().lock().expect("store").snapshot();
        assert!(ffi.block_on(ffi.vision_snapshot_block(&project)).is_none());
        assert!(ffi
            .block_on(ffi.whiteboard_snapshot_block(&project, 640, 360))
            .is_none());
        assert_eq!(ffi.whiteboard_raster_dims(), (1280, 720));
    }

    /// The 4-trait bound, exercised for real: a text-only Chat turn driven by
    /// `app_core::run_agent_turn` against an `FfiAppCtx` SUCCEEDS.
    ///
    /// Body BYTE-UNCHANGED across Phase 54.1; only this doc moved. At 47-05 it
    /// documented "the honest D-03 scope this export ships with" — text-only was
    /// then the ONLY thing this host could do. It is now ONE of several live
    /// paths (vision landed in plan 54.1-02, generation and spend approval in
    /// 54.1-04), and it must keep working exactly as it did: a turn that asks
    /// for nothing paid and draws nothing must still cost one round trip, carry
    /// no image, and disclose nothing. Widening the host is not allowed to
    /// change the cheap case.
    #[test]
    fn text_only_agent_turn_succeeds_through_ffi_app_ctx() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);
        let session = std::sync::Mutex::new(app_core::AgentSession::default());
        let transport = agent_llm::FixtureTransport::new(vec![resp(
            vec![agent_llm::ContentBlock::Text {
                text: "Nothing to edit — all set.".to_string(),
            }],
            "end_turn",
        )]);

        let outcome = ffi
            .block_on(app_core::run_agent_turn(
                &ffi,
                &session,
                &transport,
                "hello".to_string(),
                vec![],
                &lib_dir(),
            ))
            .expect("a text-only turn succeeds through the FFI host");

        assert!(outcome.narration.is_some(), "the closing text became narration");
        assert!(outcome.clarifying_question.is_none());
        assert!(outcome.generation_disclosures.is_empty());
    }

    /// **SC-3 end-to-end**: a scripted `generate_ai_image` tool call through the
    /// REAL `run_agent_turn` loop halts PRE-DISPATCH on the spend gate — no
    /// handler, no provider work, no file, no spend — and the TURN itself
    /// succeeds rather than failing.
    ///
    /// The Phase-47 original, re-aimed in the same commit as the impls. Two
    /// things changed and nothing else did:
    ///
    /// 1. **The ctx now HAS a provider** ([`ctx_with_image_provider`]). This is
    ///    the load-bearing edit. Before, the halt could have been explained by
    ///    "there was nothing to spend with"; the sibling landing test proves
    ///    THIS EXACT ctx really does generate once approved, so the only thing
    ///    stopping it here is the missing approval. "No spend without approval",
    ///    not "no spend, period".
    /// 2. **The question is the production one.** The hard-coded D-03 refusal
    ///    const this used to compare against is DELETED (`git log -S` on this
    ///    file is the route back to it); the expectation is now built from the
    ///    same `app_core` gate the production path calls, so the test can never
    ///    drift from the text the user actually reads — plus three independent
    ///    substring assertions so it is not purely self-referential.
    ///
    /// Every structural assertion below is untouched: one scripted response, one
    /// request seen, no disclosures, an empty media bin, zero `gen:job`/
    /// `gen:progress` ring records, and a spend-confirmation `PendingAskUser` on
    /// the call's own id with `spend_approved_turn` still false — halting grants
    /// NOTHING.
    #[test]
    fn scripted_generate_ai_image_halts_pending_spend_confirmation_until_approved() {
        let ctx = ctx_with_image_provider();
        let ffi = FfiAppCtx::new(&ctx);
        let session = std::sync::Mutex::new(app_core::AgentSession::default());
        // ONE scripted response. A second `transport.send()` would err
        // ("script exhausted") and fail the turn, so completing at all proves
        // the turn HALTED here — the same discipline as the shell's own
        // spend-gate suite.
        let transport = agent_llm::FixtureTransport::new(vec![resp(
            vec![agent_llm::ContentBlock::ToolUse {
                id: "tu-gen".to_string(),
                name: "generate_ai_image".to_string(),
                input: serde_json::json!({ "prompt": "a red title card", "model": IMAGE_MODEL }),
            }],
            "tool_use",
        )]);

        let outcome = ffi
            .block_on(app_core::run_agent_turn(
                &ffi,
                &session,
                &transport,
                "generate me a red title card".to_string(),
                vec![],
                &lib_dir(),
            ))
            .expect("fail-closed halts the CALL, never errors the TURN");

        let q = outcome
            .clarifying_question
            .expect("the gate halted the turn with a question");
        // The PRODUCTION question, verbatim — built from the one `app_core` gate
        // the turn loop itself consults, over the same tool + the same resolved
        // model, so this can never drift from the text the user reads.
        let expected = match app_core::spend_confirmation_gate(
            "generate_ai_image",
            ffi.resolved_model_for_tool_input(
                "generate_ai_image",
                &serde_json::json!({ "prompt": "a red title card", "model": IMAGE_MODEL }),
            )
            .as_deref(),
            // Phase 55.1: the cost half of the question, from the SAME input the
            // turn loop reads and the dispatch bills (T-55.1-09).
            app_core::cost_signal_for_tool_input(
                "generate_ai_image",
                &serde_json::json!({ "prompt": "a red title card", "model": IMAGE_MODEL }),
            ),
            false,
        ) {
            app_core::SpendGateDecision::NeedsConfirmation { question } => question,
            app_core::SpendGateDecision::Proceed => {
                panic!("the unapproved gate must not Proceed")
            }
        };
        assert_eq!(q, expected, "the real gate question reaches the user verbatim");
        // ...and it is a spend question with real content, not merely equal to
        // whatever the gate happens to return today.
        assert!(q.contains("PAID call"), "the user is told this costs money: {q}");
        assert!(q.contains("generate_ai_image"), "the tool is named: {q}");
        assert!(
            q.contains(image_model()),
            "T-42.3-13: the model that would be billed is named: {q}"
        );
        // Phase 55.1 (T-55.1-08): and its PRICE. This is the question the USER
        // reads, assembled by the real turn loop — so the cost dimension is
        // proven where it matters, not only at the gate function.
        assert!(
            q.contains(app_core::cost_signal_for_model(IMAGE_MODEL)),
            "the roster price for {IMAGE_MODEL} reaches the user: {q}"
        );
        assert_eq!(transport.requests_seen().len(), 1, "halted without a 2nd send");
        assert!(
            outcome.generation_disclosures.is_empty(),
            "nothing was generated, so there is nothing to disclose"
        );

        // ZERO provider work, asserted structurally: no asset landed, and no
        // gen-lifecycle event ever entered the ring.
        assert!(
            ffi.store().lock().expect("store").snapshot().media_bin.is_empty(),
            "no asset landed"
        );
        let polled = ctx.ring.poll(0);
        assert!(
            polled
                .events
                .iter()
                .all(|r| r.event != ring::EVENT_GEN_JOB && r.event != ring::EVENT_GEN_PROGRESS),
            "no gen:job/gen:progress records exist"
        );

        // Captured as a spend-confirmation PendingAskUser on the call's own
        // id — the resume path's entire input; halting grants NOTHING.
        let sess = session.lock().expect("session");
        let pending = sess
            .pending_ask_user
            .as_ref()
            .expect("the gate halt was captured as a PendingAskUser");
        assert_eq!(pending.tool_use_id, "tu-gen");
        assert!(
            pending.spend_confirmation,
            "flagged as the gate's own halt, not as a model askUser"
        );
        assert!(
            !sess.spend_approved_turn,
            "halting grants nothing — only the user's ANSWER does"
        );
    }

    // ------------------------------------------------------------------
    // Plan 54.1-02: `AgentVision` FOR REAL — the agent can SEE through this
    // host. Every test below is RED against the Phase-47 degrade-to-`None`
    // stub and green only once `crate::vision` is wired in.
    // ------------------------------------------------------------------

    /// One Whiteboard-space stroke. Whiteboard marks are the case whose ONLY
    /// channel to the model is the rasterized board IMAGE — the structured
    /// `agent_state` text carries coordinates, never pixels — which is what
    /// makes them the right probe for SC-4.
    fn whiteboard_stroke(id: &str, points: &[(f64, f64)]) -> rudis_core::Annotation {
        rudis_core::Annotation {
            id: id.to_string(),
            shape: rudis_core::AnnotationShape::Stroke {
                points: points
                    .iter()
                    .map(|(x, y)| rudis_core::NormPoint { x: *x, y: *y })
                    .collect(),
            },
            linked_range_us: None,
            space: rudis_core::AnnotationSpace::Whiteboard,
        }
    }

    /// A project carrying exactly the given canvas marks and nothing else.
    fn project_with_marks(marks: Vec<rudis_core::Annotation>) -> rudis_core::Project {
        let mut project = rudis_core::Project::default();
        project.canvas.annotations = marks;
        project
    }

    /// Replace the ctx's whole store, exactly as `rudis_debug_seed_project`
    /// does (`commands.rs`: `Store::from_project`) — minus the env gate, which
    /// guards the ABI export, not the in-process rlib tier.
    fn seed(ctx: &RudisCtx, project: rudis_core::Project) {
        *ctx.store.lock().expect("store") = rudis_core::Store::from_project(project);
    }

    /// The expected whiteboard PNG, reconstructed from FIRST PRINCIPLES —
    /// deliberately NOT by calling `vision::blank_whiteboard_frame`.
    ///
    /// A test that built its expectation by calling the production builder
    /// would assert the code against itself; this one writes the transparent-
    /// white background buffer out longhand, so "the outbound request carries a
    /// board painted from the project's ink" is proven against an independent
    /// construction. The PAINTER is shared on purpose (there must be exactly
    /// one), as is the encoder.
    fn expected_board_png(w: u32, h: u32, marks: &[rudis_core::Annotation]) -> Vec<u8> {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for _ in 0..(w as usize * h as usize) {
            // src-tauri/src/lib.rs:288's WHITEBOARD_BG, written out: transparent
            // white (the `canvas-background-leaks-into-agent-vision` fix).
            rgba.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]);
        }
        let mut frame = engine::Frame {
            width: w,
            height: h,
            rgba,
        };
        crate::panel::overlay::draw_annotations_onto_styled(
            &mut frame,
            marks,
            crate::panel::overlay::OVERLAY_INK,
            false,
        );
        engine::encode_png_bytes(&frame).expect("the control board PNG-encodes")
    }

    /// The `image` content blocks of the LAST user message in a captured
    /// request, as serialized JSON (`ContentBlock` is `Serialize` but not
    /// `PartialEq`, so JSON is how two blocks are compared).
    fn image_blocks_of_last_user_message(
        request: &agent_llm::MessagesRequest,
    ) -> Vec<serde_json::Value> {
        let last = request
            .messages
            .last()
            .expect("the request carries at least the user turn");
        assert_eq!(last.role, agent_llm::Role::User, "the last message is the user turn");
        last.content
            .iter()
            .map(|b| serde_json::to_value(b).expect("a content block serializes"))
            .filter(|v| v["type"] == "image")
            .collect()
    }

    /// The concatenated TEXT of the last user message — the "context the text
    /// already carries", which SC-4 requires the image to be absent from.
    fn text_of_last_user_message(request: &agent_llm::MessagesRequest) -> String {
        request
            .messages
            .last()
            .expect("the request carries at least the user turn")
            .content
            .iter()
            .filter_map(|b| match b {
                agent_llm::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Test A — the whiteboard snapshot is a REAL PNG built from project ink,
    /// and still degrades to `None` with no ink (T-13-16 preserved).
    #[test]
    fn whiteboard_snapshot_block_rasterizes_project_ink_and_degrades_without_it() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);

        let marks = vec![whiteboard_stroke("wb-1", &[(0.1, 0.1), (0.9, 0.9)])];
        let inked = project_with_marks(marks.clone());

        let block = ffi
            .block_on(ffi.whiteboard_snapshot_block(&inked, 640, 360))
            .expect("a whiteboard-annotated project rasterizes a board");
        let json = serde_json::to_value(&block).expect("the block serializes");
        assert_eq!(json["type"], "image");
        assert_eq!(
            json["source"]["media_type"], "image/png",
            "the board stays PNG — JPEG ringing smears flat-colour line art \
             (agent-llm/src/vision.rs's module doc)"
        );
        assert_eq!(
            json["source"]["data"],
            serde_json::to_value(&agent_llm::image_content_block_png(&expected_board_png(
                640, 360, &marks
            )))
            .expect("control serializes")["source"]["data"],
            "the board is the project's ink painted on a 640x360 transparent-white page"
        );
        assert_ne!(
            json["source"]["data"],
            serde_json::to_value(&agent_llm::image_content_block_png(&expected_board_png(
                640,
                360,
                &[]
            )))
            .expect("blank control serializes")["source"]["data"],
            "and it is NOT the blank page — ink changed real pixels"
        );

        // FrameLinked marks belong on the video frame, never on the board
        // (Phase 14.2 SC-2 no-leak), so a frame-linked-only project produces no
        // board at all.
        let mut frame_linked = whiteboard_stroke("fl-1", &[(0.2, 0.2), (0.8, 0.8)]);
        frame_linked.space = rudis_core::AnnotationSpace::FrameLinked;
        assert!(
            ffi.block_on(
                ffi.whiteboard_snapshot_block(&project_with_marks(vec![frame_linked]), 640, 360)
            )
            .is_none(),
            "frame-linked ink never leaks onto the whiteboard"
        );

        assert!(
            ffi.block_on(ffi.whiteboard_snapshot_block(
                &rudis_core::Project::default(),
                640,
                360
            ))
            .is_none(),
            "no marks, no board — the T-13-16 degradation, zero blocking work"
        );
    }

    /// Test B — **SC-4, at the fixture tier.** The outbound request for a
    /// whiteboard-annotated project carries an image block that is a rendering
    /// of the user's ink; the identical run against an ink-free project carries
    /// none. The pixels exist in NO other part of the request: the structured
    /// `agent_state` text carries coordinates, and this asserts the image bytes
    /// appear nowhere in it.
    #[test]
    fn a_whiteboard_annotated_turn_sends_the_board_image_the_text_cannot_carry() {
        // Two independent ctxs in this test, so neither shadows the `ctx()`
        // constructor the way every single-ctx test above can afford to.
        let inked_ctx = ctx();
        let ffi = FfiAppCtx::new(&inked_ctx);
        let marks = vec![whiteboard_stroke("wb-1", &[(0.15, 0.8), (0.5, 0.2), (0.85, 0.8)])];
        seed(&inked_ctx, project_with_marks(marks.clone()));

        let session = std::sync::Mutex::new(app_core::AgentSession::default());
        let transport = agent_llm::FixtureTransport::new(vec![resp(
            vec![agent_llm::ContentBlock::Text {
                text: "I can see the sketch.".to_string(),
            }],
            "end_turn",
        )]);
        ffi.block_on(app_core::run_agent_turn(
            &ffi,
            &session,
            &transport,
            "what did I draw?".to_string(),
            vec![],
            &lib_dir(),
        ))
        .expect("the turn succeeds");

        let requests = transport.requests_seen();
        assert_eq!(requests.len(), 1, "exactly one round trip");
        let images = image_blocks_of_last_user_message(&requests[0]);
        assert_eq!(
            images.len(),
            1,
            "the turn carries exactly the whiteboard board (no frame snapshot: no media loaded)"
        );

        // The (1280, 720) raster is `whiteboard_raster_dims`' fallback — no
        // panel has attached in this test, so the board is the mock-app size.
        let expected = serde_json::to_value(&agent_llm::image_content_block_png(
            &expected_board_png(1280, 720, &marks),
        ))
        .expect("control serializes");
        assert_eq!(
            images[0]["source"]["data"], expected["source"]["data"],
            "the image the model receives IS the user's ink, rasterized"
        );
        assert_ne!(
            images[0]["source"]["data"],
            serde_json::to_value(&agent_llm::image_content_block_png(&expected_board_png(
                1280, 720, &[]
            )))
            .expect("blank control serializes")["source"]["data"],
            "and it is not a blank page"
        );

        // SC-4's "the text context does not contain it": the pixels reach the
        // model ONLY through the image block.
        let text = text_of_last_user_message(&requests[0]);
        let data = images[0]["source"]["data"]
            .as_str()
            .expect("base64 payload is a string");
        assert!(!data.is_empty(), "the image block carries real bytes");
        assert!(
            !text.contains(data),
            "the rasterized ink exists ONLY in the image block, never in the text"
        );

        // The control: same wiring, same transport, NO ink — zero images.
        let clean_ctx = ctx();
        let clean_ffi = FfiAppCtx::new(&clean_ctx);
        let clean_session = std::sync::Mutex::new(app_core::AgentSession::default());
        let clean_transport = agent_llm::FixtureTransport::new(vec![resp(
            vec![agent_llm::ContentBlock::Text {
                text: "Nothing drawn.".to_string(),
            }],
            "end_turn",
        )]);
        clean_ffi
            .block_on(app_core::run_agent_turn(
                &clean_ffi,
                &clean_session,
                &clean_transport,
                "what did I draw?".to_string(),
                vec![],
                &lib_dir(),
            ))
            .expect("the ink-free turn succeeds too");
        assert!(
            image_blocks_of_last_user_message(&clean_transport.requests_seen()[0]).is_empty(),
            "no ink, no image — the difference is the ink, not the wiring"
        );
    }

    /// Test C — the raster dims come from the MEASURED source (the Preview
    /// panel's contain-fit CONTENT RECT, per this plan's Task-1 artifact
    /// `54.1-02-canvas-surface-measurement.md`), clamped, with the mock-app
    /// (1280, 720) fallback whenever no composite has published one yet.
    #[test]
    fn whiteboard_raster_dims_reads_the_content_rect_with_the_1280x720_fallback() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);

        // Fresh ctx: no panel attached, no composite, rect is (0,0,0,0).
        assert_eq!(
            ffi.whiteboard_raster_dims(),
            (1280, 720),
            "the exact fallback every mock-runtime app already gets"
        );

        // A composited landscape rect, under the cap: passed through.
        ctx.preview_surface
            .publish_content_rect([12.0, 8.0, 800.0, 450.0]);
        assert_eq!(
            ffi.whiteboard_raster_dims(),
            (800, 450),
            "never upscaled; the rect's OFFSET is irrelevant to the raster size"
        );

        // A portrait rect: the long edge is the height, capped at 1280.
        ctx.preview_surface
            .publish_content_rect([0.0, 0.0, 1080.0, 1920.0]);
        assert_eq!(ffi.whiteboard_raster_dims(), (720, 1280));

        // Back to a degenerate rect (a detach, or a pre-composite window):
        // fall back rather than raster a 0-pixel board.
        ctx.preview_surface.publish_content_rect([0.0, 0.0, 0.0, 0.0]);
        assert_eq!(ffi.whiteboard_raster_dims(), (1280, 720));
    }

    /// A real `test-media` clip as a Source-mode MediaBin item — the fixture
    /// test D's frame snapshot decodes.
    fn source_project_on(path: &std::path::Path, w: u32, h: u32) -> rudis_core::Project {
        let item = rudis_core::MediaBinItem {
            id: "m1".to_string(),
            path: path.to_string_lossy().into_owned(),
            media_kind: rudis_core::MediaKind::Video,
            duration_us: 5_000_000,
            width: w,
            height: h,
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
        let mut project = rudis_core::Project::default();
        project.media_bin.push(item);
        project.preview_mode = rudis_core::PreviewMode::Source;
        project.source_playback.loaded_media_id = Some("m1".to_string());
        project.source_playback.position_us = 1_000_000;
        project.source_playback.duration_us = 5_000_000;
        let mut mark = whiteboard_stroke("fl-1", &[(0.2, 0.3), (0.8, 0.7)]);
        mark.space = rudis_core::AnnotationSpace::FrameLinked;
        project.canvas.annotations.push(mark);
        project
    }

    /// Test D (FFmpeg tier) — the FRAME snapshot decodes REAL media off disk,
    /// paints the frame-linked ink on it and ships it as the history-tier
    /// JPEG (`bug/agent-history-413`).
    #[test]
    #[ignore = "needs real ffmpeg/ffprobe (test-media fixtures); run with --include-ignored locally and at the phase gate — see 47-06-PLAN scope note"]
    fn vision_snapshot_block_decodes_real_media_and_ships_a_jpeg() {
        let ctx = ctx();
        let ffi = FfiAppCtx::new(&ctx);
        let clip = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-media/bars_720p30_5s.mp4");
        assert!(clip.exists(), "the repo fixture is present: {}", clip.display());

        let project = source_project_on(&clip, 1280, 720);
        let block = ffi
            .block_on(ffi.vision_snapshot_block(&project))
            .expect("a frame-linked mark over real media produces a frame snapshot");
        let json = serde_json::to_value(&block).expect("serializes");
        assert_eq!(json["type"], "image");
        assert_eq!(
            json["source"]["media_type"], "image/jpeg",
            "the CHAT-HISTORY frame snapshot is JPEG (bug/agent-history-413)"
        );
        assert!(
            json["source"]["data"].as_str().expect("base64").len() > 1_000,
            "real encoded bytes, not an empty payload"
        );

        // Same media, NO frame-linked ink: skipped entirely, zero decode
        // (T-13-15 token discipline).
        let mut bare = source_project_on(&clip, 1280, 720);
        bare.canvas.annotations.clear();
        assert!(ffi.block_on(ffi.vision_snapshot_block(&bare)).is_none());
    }

    /// Test E — the ported `clamp_raster_dims` reproduces `src-tauri`'s table.
    ///
    /// The four cases and their assertion messages are COPIED VERBATIM from
    /// `src-tauri/src/lib.rs`'s own `clamp_raster_dims_preserves_aspect_and_bounds`
    /// (Phase 14.3, D-03), which is the whole point: T-54.1-07 is drift between
    /// the two hosts' snapshot clusters, and a re-worded test would not catch
    /// it. If either implementation changes, both suites go red together.
    #[test]
    fn clamp_raster_dims_preserves_aspect_and_bounds() {
        use crate::vision::clamp_raster_dims;

        // Oversized landscape: long edge capped at 1280, aspect preserved.
        let (w, h) = clamp_raster_dims(3840, 2160, 1280);
        assert_eq!(w, 1280, "long edge capped at max");
        assert_eq!(h, 720, "16:9 aspect preserved (2160/3840*1280)");

        // Oversized portrait: HEIGHT is the long edge, capped at 1280.
        let (w, h) = clamp_raster_dims(2160, 3840, 1280);
        assert_eq!(h, 1280, "portrait long edge capped at max");
        assert_eq!(w, 720, "aspect preserved");

        // Small panel: never upscaled (scale capped at 1.0).
        assert_eq!(clamp_raster_dims(640, 360, 1280), (640, 360), "no upscale");

        // Degenerate sliver: short edge floored at 200, long edge <= max.
        let (w, h) = clamp_raster_dims(4000, 50, 1280);
        assert!(w <= 1280, "long edge under cap");
        assert!(h >= 200, "short edge floored at 200 (no degenerate sliver)");

        // The frame-snapshot tier's own long edge (1568, Claude's Standard
        // vision tier) travels through the SAME function — asserted here so the
        // 1568 call sites in `vision.rs` are covered by a case, not just by
        // inspection.
        assert_eq!(clamp_raster_dims(1920, 1080, 1568), (1568, 882));
    }

    // ------------------------------------------------------------------
    // Plan 60.2-03: the Canvas STAGE's wire bytes, end to end.
    //
    // Test A above proves the rasterizer works when a whiteboard-inked
    // `Project` is handed to it. That is one step short of the claim Phase
    // 60.2 actually makes, which is that the SHELL'S OWN BYTES produce such a
    // project. The gap between those two is exactly where the GATE-07 orphan
    // lived for four phases: the domain half was hosted and green the whole
    // time; what was missing was a C# CALLER, and no Rust test could have
    // noticed, because every Rust test built its project by hand.
    //
    // So this one does not build a project. It dispatches the client's literal
    // bytes through the real ABI and reads what the store ended up holding.
    // ------------------------------------------------------------------

    /// BYTE-IDENTICAL to the C# pin in
    /// `shell/Rudis.Shell.Tests/CanvasStageGestureTests.cs::
    /// the_stage_dispatch_envelope_is_byte_pinned_for_the_rust_twin` — the exact
    /// envelope `Canvas.xaml.cs` dispatches for a two-point pen stroke on the
    /// stage (plan 60.2-02), produced there by the SAME
    /// `CanvasGesture.BuildDispatchArgs` the production path calls rather than
    /// hand-written on either side.
    ///
    /// The `contract.rs` `CSHARP_*` family (`CSHARP_PEN`/`LASSO`/`ARROW`/`LABEL`)
    /// pins the `frame_linked` space; this is the WHITEBOARD sibling, kept
    /// in-crate rather than joining them because the assertion target —
    /// `whiteboard_snapshot_block` beside `expected_board_png` — is this
    /// module's test kit and is not reachable from the `tests/` tier.
    ///
    /// If the C# test goes red, this literal is stale. Fix BOTH in one commit.
    const CSHARP_STAGE_PEN: &str = "{\"cmd\":{\"type\":\"add_annotation\",\"data\":{\"id\":\"a-77777777\",\"shape\":{\"kind\":\"stroke\",\"points\":[{\"x\":0.25,\"y\":0.25},{\"x\":0.75,\"y\":0.5}]},\"linked_range_us\":null,\"space\":\"whiteboard\"}}}";

    /// **SC-3, from the stage's own bytes rather than from a hand-built
    /// project** — and the undoable half of STAGE-02 at the wire level.
    ///
    /// The chain asserted here is the whole phase in one test: the bytes
    /// `Canvas.xaml.cs` writes -> the REAL `rudis_dispatch_command` ->
    /// `Command::apply`'s validation -> the store -> the agent's already-shipped
    /// vision path -> a REAL PNG whose pixels are the user's stroke; then
    /// `rudis_undo` -> no annotation -> no board.
    ///
    /// Two controls, because "the agent can see it" needs both halves:
    /// * the board EQUALS `expected_board_png(.., &[the same stroke])`, built
    ///   independently from first principles — that pins WHICH ink;
    /// * and it DIFFERS from `expected_board_png(.., &[])`, the blank page —
    ///   that pins that ink changed real pixels rather than merely being
    ///   present in a field.
    ///
    /// Nothing in this test constructs a `rudis_core::Command`: `dispatch.rs`'s
    /// own annotation test builds one and serializes it, which would prove the
    /// domain round-trips its own type. Here the JSON is the CLIENT's, verbatim,
    /// so `serde` has to accept the C# builder's spelling — the failure mode a
    /// `Value` round-trip would normalise away (`contract.rs`'s `dispatch_raw`
    /// exists for the same reason).
    #[test]
    fn stage_wire_bytes_reach_the_agents_whiteboard_snapshot_and_undo_clears_it() {
        // Heaped, not the by-value `ctx()` helper: the FFI entry points take
        // `*mut RudisCtx` (the `dispatch.rs` shape).
        let ctx = Box::into_raw(Box::new(
            RudisCtx::new_in_process(
                InitConfig::default(),
                Box::new(agent_llm::InMemoryKeyStore::new()),
            )
            .expect("in-process ctx builds"),
        ));

        // The store's current project. Deliberately a NARROW scope: the guard
        // must be gone before any `block_on` below, or a vision path that took
        // the same lock would deadlock against this reader.
        fn project_of(ctx: *mut RudisCtx) -> rudis_core::Project {
            let inner = unsafe { &*ctx };
            inner.store.lock().expect("store healthy").snapshot()
        }

        fn empty_buf() -> crate::RudisBuffer {
            crate::RudisBuffer {
                ptr: std::ptr::null_mut(),
                len: 0,
                cap: 0,
            }
        }

        assert!(
            project_of(ctx).canvas.annotations.is_empty(),
            "a fresh instance has no ink — the control this test's `is_none` half rests on"
        );

        // -- the stage's bytes, through the real ABI ---------------------
        let mut buf = empty_buf();
        assert_eq!(
            crate::dispatch::rudis_dispatch_command(
                ctx,
                CSHARP_STAGE_PEN.as_ptr(),
                CSHARP_STAGE_PEN.len(),
                &mut buf,
            ),
            crate::RudisStatus::Ok,
            "the client's literal bytes are accepted by the transport"
        );
        let envelope: serde_json::Value =
            serde_json::from_slice(unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) })
                .expect("the response is valid JSON");
        crate::rudis_free_buffer(buf);
        assert!(
            envelope.get("Ok").is_some(),
            "and by the DOMAIN — a transport `Ok` carrying an `Err` envelope is the \
             D-06 shape a status-only assertion would miss: {envelope}"
        );

        // -- what the agent now sees -------------------------------------
        let project = project_of(ctx);
        assert_eq!(
            project.canvas.annotations.len(),
            1,
            "exactly the one mark the stage drew"
        );
        assert_eq!(
            project.canvas.annotations[0].space,
            rudis_core::AnnotationSpace::Whiteboard,
            "STAGE-01's subject: the stage writes the WHITEBOARD space, not the Preview's"
        );

        let ffi = FfiAppCtx::new(unsafe { &*ctx });
        let block = ffi
            .block_on(ffi.whiteboard_snapshot_block(&project, 640, 360))
            .expect("the stage's own mark rasterizes a board for the agent");
        let json = serde_json::to_value(&block).expect("the block serializes");
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["media_type"], "image/png");

        // The SAME two points the C# pin carries, as an independently
        // constructed control. If the wire spelling of a point ever changes
        // meaning, these two disagree.
        let control = vec![whiteboard_stroke(
            "a-77777777",
            &[(0.25, 0.25), (0.75, 0.5)],
        )];
        assert_eq!(
            json["source"]["data"],
            serde_json::to_value(&agent_llm::image_content_block_png(&expected_board_png(
                640, 360, &control
            )))
            .expect("control serializes")["source"]["data"],
            "the agent's board IS the stroke the stage dispatched, painted on a 640x360 page"
        );
        assert_ne!(
            json["source"]["data"],
            serde_json::to_value(&agent_llm::image_content_block_png(&expected_board_png(
                640,
                360,
                &[]
            )))
            .expect("blank control serializes")["source"]["data"],
            "and it DIFFERS from the no-ink control — the stage's ink changed real pixels"
        );

        // -- and a REAL TURN carries it ----------------------------------
        //
        // STAGE-03 says "an agent TURN taken with stage ink present carries a
        // whiteboard vision block whose PNG differs from the no-ink control",
        // and the block assertions above stop one hop short of that: they prove
        // `whiteboard_snapshot_block` answers, not that `run_agent_turn` puts
        // the answer in the outbound request. Test B proves that second hop —
        // but from a HAND-SEEDED project, so composing the two would leave the
        // requirement resting on an inference across two tests. It is cheaper
        // to close the hop than to argue it, so this runs the real turn against
        // the store the STAGE's OWN BYTES filled. Nothing is seeded here: the
        // mark under the turn is the one `rudis_dispatch_command` wrote.
        //
        // Zero network: `FixtureTransport` is the same scripted transport every
        // agent test in this module uses, and the raster is the (1280, 720)
        // `whiteboard_raster_dims` fallback because no panel has attached.
        let session = std::sync::Mutex::new(app_core::AgentSession::default());
        let transport = agent_llm::FixtureTransport::new(vec![resp(
            vec![agent_llm::ContentBlock::Text {
                text: "I can see the sketch.".to_string(),
            }],
            "end_turn",
        )]);
        ffi.block_on(app_core::run_agent_turn(
            &ffi,
            &session,
            &transport,
            "what did I draw on the canvas?".to_string(),
            vec![],
            &lib_dir(),
        ))
        .expect("the turn succeeds");

        let requests = transport.requests_seen();
        let images = image_blocks_of_last_user_message(&requests[0]);
        assert_eq!(
            images.len(),
            1,
            "the turn carries exactly the stage's board (no frame snapshot: no media loaded)"
        );
        assert_eq!(
            images[0]["source"]["data"],
            serde_json::to_value(&agent_llm::image_content_block_png(&expected_board_png(
                1280, 720, &control
            )))
            .expect("turn control serializes")["source"]["data"],
            "the image the MODEL receives is the stroke the stage dispatched"
        );
        assert_ne!(
            images[0]["source"]["data"],
            serde_json::to_value(&agent_llm::image_content_block_png(&expected_board_png(
                1280,
                720,
                &[]
            )))
            .expect("blank turn control serializes")["source"]["data"],
            "STAGE-03 verbatim: it DIFFERS from the no-ink control"
        );
        assert!(
            !text_of_last_user_message(&requests[0]).contains(
                images[0]["source"]["data"]
                    .as_str()
                    .expect("base64 payload is a string")
            ),
            "the pixels reach the model ONLY through the image block, never the text"
        );
        drop(ffi);

        // -- and it is UNDOABLE, at the wire level -----------------------
        let mut buf = empty_buf();
        assert_eq!(
            crate::commands::rudis_undo(ctx, &mut buf),
            crate::RudisStatus::Ok
        );
        crate::rudis_free_buffer(buf);

        let undone = project_of(ctx);
        assert!(
            undone.canvas.annotations.is_empty(),
            "one undo removes the stage's mark from project STATE — STAGE-02's wire half, \
             not a UI-side state pop"
        );
        let ffi = FfiAppCtx::new(unsafe { &*ctx });
        assert!(
            ffi.block_on(ffi.whiteboard_snapshot_block(&undone, 640, 360))
                .is_none(),
            "and the agent's board goes back to nothing — the T-13-16 degradation, \
             reached by a real undo rather than by constructing an empty project"
        );
        drop(ffi);

        assert_eq!(crate::rudis_shutdown(ctx), crate::RudisStatus::Ok);
    }

    // ------------------------------------------------------------------
    // Plan 54.1-04: the APPROVAL ROUND TRIP. The halt above is only half a
    // contract — a host that halts forever is indistinguishable from a host
    // that cannot spend at all, which is exactly the claim Phase 47 made and
    // this phase retires. These two prove the other half in both directions:
    // approval really does let money move (and lands a real file), and the
    // absence of a re-issue really does keep it still.
    //
    // Both drive the resume through the SAME `run_agent_turn` entry the C#
    // shell's `rudis_agent_send_message` export calls — `build_user_turn`
    // consumes the `pending_ask_user` internally. NO test performs session
    // surgery to grant approval: a second approval mechanism is Backlog
    // § 999.8's named trap, and a test that invented one would prove the trap
    // instead of the gate.
    // ------------------------------------------------------------------

    /// **SC-6 (Rust half) + the approve→submit→land→disclose pipeline**, proven
    /// zero-network through the FFI host against a REAL file on disk.
    ///
    /// Turn 1 halts on the spend gate. The user answers; turn 2 resumes through
    /// the same entry point, the model re-issues the call (what
    /// `SPEND_GATE_NOT_EXECUTED` tells it to do), the gate now sees genuine
    /// approval evidence and the fixture provider's bytes travel the UNCHANGED
    /// Phase-31 landing bridge into the media bin.
    ///
    /// The assertions that make this real rather than a return-value check:
    /// the landed item's `path` EXISTS on disk under this ctx's own
    /// `app_data_dir()/generated/` and is non-empty (CLAUDE.md rule 3), and a
    /// terminal `gen:job` ring record names that exact id — i.e. the C# shell's
    /// liveness channel genuinely fires through this host, not just the seam's
    /// return value.
    ///
    /// FFmpeg tier: the landing bridge PROBES the written file and generates a
    /// poster, both real `ffprobe`/`ffmpeg` processes.
    #[test]
    #[ignore = "needs real ffmpeg/ffprobe (test-media fixtures); run with --include-ignored locally and at the phase gate — see 47-06-PLAN scope note"]
    fn approved_resume_reissues_the_tool_call_lands_a_fixture_asset_and_discloses() {
        let ctx = ctx_with_image_provider();
        let ffi = FfiAppCtx::new(&ctx);
        let session = std::sync::Mutex::new(app_core::AgentSession::default());

        let gen_call = || {
            resp(
                vec![agent_llm::ContentBlock::ToolUse {
                    id: "tu-gen".to_string(),
                    name: "generate_ai_image".to_string(),
                    input: serde_json::json!({ "prompt": "a red title card", "model": IMAGE_MODEL }),
                }],
                "tool_use",
            )
        };
        // THREE scripted responses across TWO turns: (1) the call that halts,
        // (2) the re-issue after approval, (3) the closing narration.
        let transport = agent_llm::FixtureTransport::new(vec![
            gen_call(),
            gen_call(),
            resp(
                vec![agent_llm::ContentBlock::Text {
                    text: "Added the red title card to your media bin.".to_string(),
                }],
                "end_turn",
            ),
        ]);

        // --- Turn 1: halts, spends nothing. ---
        let halted = ffi
            .block_on(app_core::run_agent_turn(
                &ffi,
                &session,
                &transport,
                "generate me a red title card".to_string(),
                vec![],
                &lib_dir(),
            ))
            .expect("the gate halts the call, not the turn");
        assert!(
            halted.clarifying_question.is_some(),
            "turn 1 halted with the spend question"
        );
        assert!(
            halted.generation_disclosures.is_empty(),
            "nothing ran yet, so nothing is disclosed yet"
        );
        assert!(
            ffi.store().lock().expect("store").snapshot().media_bin.is_empty(),
            "turn 1 landed nothing"
        );

        // --- Turn 2: the user answers. `build_user_turn` grants the one-shot,
        //     the model re-issues, and the spend actually happens. ---
        let outcome = ffi
            .block_on(app_core::run_agent_turn(
                &ffi,
                &session,
                &transport,
                "yes".to_string(),
                vec![],
                &lib_dir(),
            ))
            .expect("the resumed turn succeeds");

        // 1. SC-6's Rust half: the GEN-08 training-license disclosure is
        //    POPULATED on the structural channel (never derived from narration),
        //    which is what the C# ChatTurnPresenter renders.
        assert_eq!(
            outcome.generation_disclosures.len(),
            1,
            "exactly one successful generate_ai_* dispatch is disclosed"
        );
        let disclosure = &outcome.generation_disclosures[0];
        assert_eq!(disclosure.modality, "image");
        assert_eq!(disclosure.prompt, "a red title card");
        let notice = disclosure
            .provider_notice
            .as_ref()
            .expect("GEN-08: the standing provider training-license notice is present");
        assert!(
            notice.contains("train"),
            "the notice says the provider may TRAIN on what was sent: {notice}"
        );
        let model_resolved = disclosure
            .model_resolved
            .as_ref()
            .expect("the disclosure names the model that was billed");
        assert!(
            model_resolved.contains(image_model()),
            "the disclosure names the image model ({}): {model_resolved}",
            image_model()
        );

        // 2. A REAL file, on REAL disk, inside this instance's own data dir.
        let snapshot = ffi.store().lock().expect("store").snapshot();
        assert_eq!(snapshot.media_bin.len(), 1, "exactly one asset landed");
        let item = &snapshot.media_bin[0];
        let landed = std::path::Path::new(&item.path);
        assert!(landed.exists(), "the landed asset exists on disk: {}", item.path);
        let bytes = std::fs::metadata(landed)
            .expect("the landed asset is stat-able")
            .len();
        assert!(bytes > 0, "the landed asset is non-empty ({bytes} bytes)");
        let generated_dir = ffi.app_data_dir().expect("data dir").join("generated");
        assert!(
            landed.starts_with(&generated_dir),
            "confined to {}: {}",
            generated_dir.display(),
            item.path
        );

        // 3. The ring's liveness channel really fired — a terminal `gen:job`
        //    naming the id that actually landed (T-54.1-03: never "ready"
        //    without the assets).
        let jobs = gen_job_payloads(&ctx);
        let ready = jobs
            .iter()
            .find(|p| p["state"] == "ready")
            .unwrap_or_else(|| panic!("a terminal ready gen:job record exists; saw {jobs:?}"));
        assert_eq!(
            ready["media_item_ids"],
            serde_json::json!([item.id]),
            "the ready record names the item that really landed"
        );

        // 4. The resume consumed the pending halt — the one-shot is spent, and a
        //    later turn starts from no approval at all.
        assert!(
            session
                .lock()
                .expect("session")
                .pending_ask_user
                .is_none(),
            "the spend-confirmation halt was consumed by the resume"
        );
        assert_eq!(
            transport.requests_seen().len(),
            3,
            "turn 1's send + turn 2's two rounds — the whole script, nothing more"
        );
    }

    // ------------------------------------------------------------------
    // Plan 56-06 (GEN-11): the clip-edit host impl. Every test below is
    // HERMETIC — no network, no sidecar, no disk — because each of the three
    // refusals fires BEFORE the extraction that would need ffmpeg, which is
    // itself the property under test (T-56-SPEND-06: a local refusal is
    // $0.00 AND cheap).
    // ------------------------------------------------------------------

    /// A `MediaBinItem` pointing at a path that need not exist: every test in
    /// this group refuses before anything opens it, and one of them asserts
    /// exactly that.
    fn edit_media_item(id: &str, path: &str) -> rudis_core::MediaBinItem {
        rudis_core::MediaBinItem {
            id: id.to_string(),
            path: path.to_string(),
            media_kind: rudis_core::MediaKind::Video,
            duration_us: 60_000_000,
            width: 1280,
            height: 720,
            fps: 30.0,
            is_vfr: false,
            rotation_degrees: 0,
            has_audio: true,
            poster_path: None,
            folder: String::new(),
            display_name: None,
            is_image_sequence: false,
            reports_alpha: None,
        }
    }

    /// A `Clip` literal with EVERY field explicit (the 18-03 fixture rule).
    fn edit_clip(id: &str, media_id: &str, in_us: i64, out_us: i64) -> rudis_core::Clip {
        rudis_core::Clip {
            id: id.to_string(),
            media_id: media_id.to_string(),
            start_us: 0,
            in_us,
            out_us,
            volume: 1.0,
            audio_detached: false,
            transform: Default::default(),
            opacity: 1.0,
            crop: Default::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        }
    }

    /// A project holding ONE video track with ONE clip over ONE media item.
    fn project_with_clip(in_us: i64, out_us: i64) -> rudis_core::Project {
        let mut project = rudis_core::Project::default();
        project.media_bin.push(edit_media_item("m-edit", "Z:/no/such/media.mp4"));
        project.timeline.tracks.push(rudis_core::Track {
            kind: rudis_core::TrackKind::Video,
            clips: vec![edit_clip("c-edit", "m-edit", in_us, out_us)],
        });
        project
    }

    /// A ctx whose VIDEO slot is preset to an explicit `None`.
    ///
    /// That is load-bearing rather than lazy: if any refusal below were ever to
    /// stop firing, the call would fall through to the provider slot and return
    /// `NO_VIDEO_PROVIDER_CONFIGURED` — a *different* message — instead of
    /// silently spending. Each test asserts on its own refusal's words, so a
    /// regression shows up as the wrong message rather than as a pass.
    fn ctx_for_edit(project: rudis_core::Project) -> RudisCtx {
        let mut ctx = ctx();
        ctx.preset_generation(None, None, None, agent_gen::AllowList::new(vec![]));
        seed(&ctx, project);
        ctx
    }

    /// **An unknown clip id is a clean `Err` naming it, with zero extraction and
    /// zero provider work.**
    ///
    /// T-56-INJ-04: `source_clip_id` is model-steerable text resolved against
    /// real store state, and the ONLY thing it can resolve to is a clip that is
    /// already on the timeline. There is no path parameter on this seam for it
    /// to become, which is why an unknown id is boring rather than dangerous.
    #[test]
    fn submit_video_edit_refuses_an_unknown_clip_id() {
        let ctx = ctx_for_edit(project_with_clip(0, 5_000_000));
        let ffi = FfiAppCtx::new(&ctx);

        let err = ffi
            .block_on(ffi.submit_video_edit(
                "relight this as golden hour".to_string(),
                "no-such-clip".to_string(),
                None,
                Vec::new(),
            ))
            .err()
            .expect("an unknown clip id is refused");
        assert!(
            err.contains("no-such-clip"),
            "the refusal names the id that was not found: {err}"
        );
        assert!(
            !err.contains("provider"),
            "and it refused BEFORE the provider slot was consulted — reaching \
             that would mean the lookup had been skipped: {err}"
        );
        assert!(
            ffi.store().lock().expect("store").snapshot().media_bin.len() == 1,
            "nothing landed"
        );
    }

    /// **A clip outside the window refuses with `clip_edit_window_check`'s OWN
    /// message, verbatim** — both edges, and with zero decode work.
    ///
    /// Asserted by EQUALITY against the shared function rather than by
    /// re-wording, so the host cannot start telling the user a different story
    /// from the one 56-04 wrote (which names both bounds, the measured length,
    /// and a DIFFERENT offer per edge — `splitClip` over the ceiling, a longer
    /// clip under the floor).
    #[test]
    fn submit_video_edit_refuses_outside_the_window_by_name() {
        let max_s = i64::from(agent_gen::RUNWAY_V2V_INPUT_MAX_SECONDS);
        let min_s = i64::from(agent_gen::RUNWAY_V2V_INPUT_MIN_SECONDS);
        // Over the ceiling, and under the floor — the edge 56-CONTEXT's 2026-08-01
        // correction added and the original D-01 never contemplated.
        let cases: [(i64, &str); 2] = [
            (max_s * 1_000_000 + 1_000_000, "over the ceiling"),
            (min_s * 1_000_000 - 1, "under the floor"),
        ];

        for (visible_us, label) in cases {
            let ctx = ctx_for_edit(project_with_clip(1_000_000, 1_000_000 + visible_us));
            let ffi = FfiAppCtx::new(&ctx);

            let err = ffi
                .block_on(ffi.submit_video_edit(
                    "relight this as golden hour".to_string(),
                    "c-edit".to_string(),
                    None,
                    Vec::new(),
                ))
                .err()
                .unwrap_or_else(|| panic!("{label}: a range outside the window is refused"));

            let expected = app_core::clip_edit_window_check(visible_us)
                .expect_err("the shared check refuses this range");
            assert_eq!(
                err, expected,
                "{label}: the window check's message reaches the caller VERBATIM \
                 — one refusal site, one message (56-04)"
            );
            assert!(
                err.contains(&min_s.to_string()) && err.contains(&max_s.to_string()),
                "{label}: and it names both bounds: {err}"
            );
        }

        // Non-vacuity: the SAME media, trimmed INSIDE the window, gets past the
        // check — so the refusals above are about the range and not about the
        // fixture being unusable. It then fails at the EXTRACTION (the media
        // path does not exist), which is the next step in the contract and
        // proves the ordering: window BEFORE extraction.
        let inside = ctx_for_edit(project_with_clip(0, 4_000_000));
        let ffi = FfiAppCtx::new(&inside);
        let err = ffi
            .block_on(ffi.submit_video_edit(
                "relight".to_string(),
                "c-edit".to_string(),
                None,
                Vec::new(),
            ))
            .err()
            .expect("the missing media file fails at extraction");
        assert!(
            err.contains("clip-edit extraction"),
            "an in-window clip reaches the EXTRACTION stage, which is the step \
             after the window check: {err}"
        );
    }

    /// **Six references refuse before ANY resolution work.**
    ///
    /// `ReferenceSource::Sketch` is used deliberately: on this empty-canvas
    /// project resolving even one of them would raster the whiteboard and then
    /// fail with "no canvas sketch…". Getting the CAP message instead is the
    /// proof that not one of the six was touched — the raster seam is never
    /// entered, so there is no counter to read and none is needed.
    #[test]
    fn submit_video_edit_refuses_six_references_before_resolving_any() {
        let cap = agent_gen::RUNWAY_V2V_MAX_REFERENCES;
        let ctx = ctx_for_edit(project_with_clip(0, 4_000_000));
        let ffi = FfiAppCtx::new(&ctx);

        let refs: Vec<app_core::ReferenceSource> =
            (0..cap + 1).map(|_| app_core::ReferenceSource::Sketch).collect();
        let err = ffi
            .block_on(ffi.submit_video_edit(
                "match this look".to_string(),
                "c-edit".to_string(),
                None,
                refs,
            ))
            .err()
            .expect("over the cap is refused");
        assert_eq!(
            err,
            app_core::video_edit_reference_check(cap + 1).expect_err("the ONE cap message"),
            "the shared refusal reaches the caller verbatim"
        );
        assert!(
            !err.contains("no canvas sketch"),
            "NOT ONE reference was resolved — a resolved Sketch on this empty \
             project would have said so: {err}"
        );
        assert!(
            !err.contains("clip-edit extraction"),
            "and no extraction ran either: the count gate is the FIRST act, \
             before the store lock (T-56-SPEND-06): {err}"
        );
    }

    /// **The reference SLOT is blocked, and the refusal is honest about why.**
    ///
    /// GEN-11 asks for "up to 5 reference images, the Canvas-annotated preview
    /// frame first among them". That clause is UNMET: probe F-1b (2026-08-09,
    /// $0.00) enumerated both candidate fields in full and crowned neither, so
    /// the remaining question is behavioural and belongs to 56-09. The seam
    /// refuses rather than dropping, because 42.1-04 caught a dropped reference
    /// coming back HTTP 200 with a plausible unconditioned result.
    ///
    /// It also refuses the CANVAS frame (`ReferenceSource::Frame`, D-09), which
    /// is the one that would hurt most to lose silently.
    #[test]
    fn submit_video_edit_refuses_a_canvas_frame_reference_and_names_the_paid_run() {
        for (n, source) in [
            (1usize, app_core::ReferenceSource::Frame),
            (1, app_core::ReferenceSource::Sketch),
            (3, app_core::ReferenceSource::Media("m-edit".to_string())),
        ] {
            let ctx = ctx_for_edit(project_with_clip(0, 4_000_000));
            let ffi = FfiAppCtx::new(&ctx);

            let refs: Vec<app_core::ReferenceSource> =
                (0..n).map(|_| source.clone()).collect();
            let err = ffi
                .block_on(ffi.submit_video_edit(
                    "match this look".to_string(),
                    "c-edit".to_string(),
                    None,
                    refs,
                ))
                .err()
                .expect("a blocked reference slot refuses");
            assert_eq!(
                err,
                app_core::video_edit_reference_check(n).expect_err("the ONE blocked message"),
                "{source:?}: the shared refusal, verbatim"
            );
            assert!(
                err.contains("56-09"),
                "{source:?}: and it names the paid run that would unblock it — \
                 F-1b already ran: {err}"
            );
        }
    }

    /// **The whole host path, end to end, on REAL media** — a validated clip id
    /// becomes trim-respecting bytes inside `GenRequest.source_video`, and the
    /// endpoint selector is therefore genuinely set.
    ///
    /// The other four tests in this group prove the REFUSALS. This proves the
    /// success half, which no refusal can: that the lookup, the window check and
    /// the extraction actually compose into bytes that cross the provider
    /// boundary. Asserted on the CAPTURED request — the one production built and
    /// handed to `submit` — and then on the DECODED bytes, never on what was
    /// asked of ffmpeg (CLAUDE.md rule 3).
    ///
    /// **Zero network, zero spend.** `FixtureGenProvider` opens no socket, and
    /// its deliberately DISALLOWED `"bin"` extension makes `land_generated_asset`
    /// refuse before it creates a directory or writes a byte — so the submit is
    /// real and nothing lands.
    ///
    /// FFmpeg tier: the extraction decodes and re-encodes a real range, so this
    /// carries the file's standing `#[ignore]`. Run it with `RUDIS_FFMPEG_DIR`
    /// pointed at the repo's `runtime/binaries` — an unpinned run may resolve a
    /// GPL PATH build, which invalidates any encoder observation (CLAUDE.md
    /// rule 6, and `engine::ffmpeg::locate`'s own DEV-LOOP HAZARD note).
    #[test]
    #[ignore = "needs real ffmpeg/ffprobe (test-media fixtures); run with --include-ignored and RUDIS_FFMPEG_DIR=runtime/binaries — see 47-06-PLAN scope note"]
    fn submit_video_edit_extracts_the_trim_respecting_range_into_the_submitted_request() {
        let media = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-media/bars_720p30_5s.mp4");
        assert!(media.exists(), "the repo fixture is present: {}", media.display());

        // A 3-second VISIBLE range starting 1s into a 5s source — inside the
        // 2..=30s window, and deliberately NOT the whole media, so "trim
        // respecting" is a claim with something to be wrong about.
        const IN_US: i64 = 1_000_000;
        const OUT_US: i64 = 4_000_000;

        let mut project = rudis_core::Project::default();
        let mut item = edit_media_item("m-edit", &media.to_string_lossy());
        item.duration_us = 5_000_000;
        project.media_bin.push(item);
        project.timeline.tracks.push(rudis_core::Track {
            kind: rudis_core::TrackKind::Video,
            clips: vec![edit_clip("c-edit", "m-edit", IN_US, OUT_US)],
        });

        // The model the seam will submit for a model-less call, DERIVED — and
        // the allow list admits exactly it, so `submit_calls() == 1` proves the
        // gate saw the string the seam chose.
        let derived = agent_gen::advisory_video_edit_model()
            .expect("the roster still has its video_to_video row");
        let fx = agent_gen::FixtureGenProvider::sync_still(vec![0u8; 8], "bin");
        let probe = fx.clone();
        let provider = std::sync::Arc::new(agent_gen::ConcreteGenProvider::Fixture(fx));

        let mut ctx = ctx();
        ctx.preset_generation(
            None,
            Some(provider),
            None,
            agent_gen::AllowList::new(vec![agent_gen::AllowListEntry::new(
                agent_gen::FIXTURE_PROVIDER_ID,
                derived,
            )]),
        );
        seed(&ctx, project);
        let ffi = FfiAppCtx::new(&ctx);

        // The landing fails on the disallowed extension; that is expected and
        // irrelevant. The claim is about what was SUBMITTED.
        let _ = ffi.block_on(ffi.submit_video_edit(
            "relight this shot as golden hour".to_string(),
            "c-edit".to_string(),
            None,
            Vec::new(),
        ));

        assert_eq!(
            probe.submit_calls(),
            1,
            "NON-VACUITY — the host must really have reached the provider, or \
             everything below is about a request that was never built"
        );
        let req = probe
            .last_request()
            .expect("a submitted request was captured");
        assert_eq!(req.model_id, derived, "the model-less call submitted the advisory default");

        let bytes = req
            .source_video
            .as_ref()
            .expect("D-02: the extracted range IS the endpoint selector, and it is set");
        assert!(
            bytes.len() > 10_000,
            "a real 3-second re-encode, not a placeholder ({} bytes)",
            bytes.len()
        );

        // …and the bytes are a real, correctly-trimmed, audio-free clip. Written
        // out and PROBED rather than trusted: the whole point of D-02 is that
        // what leaves the machine is what the viewer sees.
        let tmp = std::env::temp_dir().join(format!(
            "rudis-ffi-v2v-{}-{}.mp4",
            std::process::id(),
            OUT_US
        ));
        std::fs::write(&tmp, bytes).expect("write the submitted bytes for probing");
        let info = engine::probe(&tmp).expect("the submitted bytes probe as real media");
        let _ = std::fs::remove_file(&tmp);

        let step_us = (1_000_000.0 / info.avg_frame_rate.max(1.0)) as i64;
        let expected = OUT_US - IN_US;
        assert!(
            (info.duration_us - expected).abs() <= step_us,
            "the submitted range is the VISIBLE length ({expected}us), within one \
             frame step ({step_us}us): got {}us",
            info.duration_us
        );
        assert!(
            !info.has_audio,
            "video-only: a v2v source carries no audio track (D-07 keeps the \
             ORIGINAL clip's audio at placement time, it is not sent)"
        );
        eprintln!(
            "V2V-HOST submitted={}B duration_us={} fps={} has_audio={}",
            bytes.len(),
            info.duration_us,
            info.avg_frame_rate,
            info.has_audio
        );
    }

    /// The DENIAL direction, quick tier: a provider is configured and the user
    /// has already been asked once, yet no money moves.
    ///
    /// What this pins is the MECHANICAL layer, and only that — stated plainly
    /// because an overclaimed green is worse than none. `build_user_turn` grants
    /// the per-turn one-shot on ANY reply (there is deliberately no backend NLP
    /// on free text: the flag means "the user was asked and answered"), so
    /// "no" is not what stops the spend. What stops it is that **nothing spends
    /// unless the MODEL re-issues the tool call** — the rulebook's decline rule
    /// and `SPEND_GATE_NOT_EXECUTED` are what steer it not to. This scripts that
    /// compliant model and asserts the floor holds: the turn completes normally,
    /// the media bin stays EMPTY and not one `gen:job` record exists, on a ctx
    /// that provably COULD have generated.
    #[test]
    fn unapproved_second_tool_call_still_halts_after_a_prior_denial() {
        let ctx = ctx_with_image_provider();
        let ffi = FfiAppCtx::new(&ctx);
        let session = std::sync::Mutex::new(app_core::AgentSession::default());

        let transport = agent_llm::FixtureTransport::new(vec![
            resp(
                vec![agent_llm::ContentBlock::ToolUse {
                    id: "tu-gen".to_string(),
                    name: "generate_ai_image".to_string(),
                    input: serde_json::json!({ "prompt": "a red title card", "model": IMAGE_MODEL }),
                }],
                "tool_use",
            ),
            resp(
                vec![agent_llm::ContentBlock::Text {
                    text: "Understood — I won't generate anything.".to_string(),
                }],
                "end_turn",
            ),
        ]);

        ffi.block_on(app_core::run_agent_turn(
            &ffi,
            &session,
            &transport,
            "generate me a red title card".to_string(),
            vec![],
            &lib_dir(),
        ))
        .expect("turn 1 halts on the gate");

        let outcome = ffi
            .block_on(app_core::run_agent_turn(
                &ffi,
                &session,
                &transport,
                "no, don't".to_string(),
                vec![],
                &lib_dir(),
            ))
            .expect("the declined resume completes normally");
        assert!(
            outcome.narration.is_some(),
            "the turn ends with narration, not an error"
        );
        assert!(
            outcome.generation_disclosures.is_empty(),
            "nothing was generated, so nothing is disclosed"
        );

        // Money did not move, asserted on both channels.
        assert!(
            ffi.store().lock().expect("store").snapshot().media_bin.is_empty(),
            "the media bin is EMPTY — with a provider configured and the one-shot \
             granted, the absence of a re-issue is what kept it that way"
        );
        assert!(
            gen_job_payloads(&ctx).is_empty(),
            "not one gen:job record exists — no job was ever created"
        );
        assert!(
            session
                .lock()
                .expect("session")
                .pending_ask_user
                .is_none(),
            "the halt was consumed; the turn is not left waiting"
        );
    }
}
