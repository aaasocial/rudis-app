//! Phase 26 (LIB-02 / TOOL-04): the project-management agent tools —
//! `new_project`, `open_project` and `get_projects` — relocated here from
//! `src-tauri/src/lib.rs` by plan 45-06.
//!
//! # These are the FIRST functions to run on a real [`AppCtx`]
//!
//! 45-05's nine leaves needed no [`AppCtx`] at all (their whole surface was
//! `&SharedStore` + `&Mutex<AgentSession>`). These three do: they resolve
//! `app_data_dir()`, they read and write the ONE
//! [`project_store::ActiveProjectMeta`], and two of them emit `project:changed`.
//! So this is the batch that turned "convert `<R: tauri::Runtime>(app:
//! &AppHandle<R>, ..)` into `<C: AppCtx>(ctx: &C, ..)`" from a plan into a
//! working pattern, and `src-tauri`'s `TauriAppCtx` is the ONE concrete
//! implementation every later batch reuses rather than reinventing.
//!
//! # The conversion, item by item — nothing else in these bodies changed
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `app.path().app_data_dir()` (via `project_store::projects_dir(app)`) | `project_store::projects_dir(ctx)` |
//! | `app.state::<project_store::ActiveProjectMeta>()` | `ctx.active_project_meta()` |
//! | the `store: &SharedStore` parameter | `let store = ctx.store();` on the first line |
//! | `emit_changed(app, &patch, 0, 0)` | `ctx.emit_patch(&patch, 0, 0)` |
//!
//! `TauriAppCtx::emit_patch` delegates to the UNCHANGED `emit_changed`, so the
//! `project:changed` wire payload is still the same `#[serde(flatten)]`ed
//! `ProjectChangedEnvelope` (LAT-02) both `native_surface.rs` listeners parse as
//! a bare `rudis_core::Patch`. Statement ORDER is preserved verbatim — which is
//! T-26-01's mitigation: `sanitize_project_name` still runs FIRST, before any
//! path join.
//!
//! # `run_export_project` / `handle_export_project` did NOT move with them
//!
//! Plan 45-06 grouped those two in as a fourth "project-management leaf pair".
//! They are neither project management nor a leaf: `run_export_project` calls
//! `resolve_export_plan` + `run_export_blocking` (395 lines, sharing
//! `ExportPlan`, `build_export_audio_wav` and
//! `export_is_single_layer_degenerate` with the UI export command `run_export`),
//! and `run_export_blocking` needs an owned `Send + 'static` progress emitter
//! that `&impl AppCtx` cannot produce. That whole cluster is **45-10's** batch,
//! which already owns `run_export`/`run_export_blocking`. See
//! `deferred-items.md` D-45-06-01.

use std::path::Path;

use rudis_core::{Patch, PatchKind, Project, Store};

use crate::project_store::{RudProjectPath, RudSaveTargetPath};
use crate::{project_store, AppCtx};

// ---------------------------------------------------------------------------
// Phase 26 (LIB-02): new_project / open_project — the project-SWITCH mechanic,
// two Pattern-C interceptions mirroring handle_inspect_media/handle_export_project
// verbatim (they need app_data_dir + ActiveProjectMeta, which live ONLY here).
// NEITHER is a core Command/Tool variant (no undo entry); the swap goes through
// rudis_core::Store::from_project, which resets undo/redo/open_turn for free
// (SC-4). All three autosave hooks (a: on switch, b: on create, c: end-of-turn)
// funnel through the single autosave_project → project_store::write_project_atomic
// (temp+rename) — no ad-hoc fs::write anywhere (threat T-26-06).
// ---------------------------------------------------------------------------

/// Phase 26 (LIB-02): autosave the active project's CURRENT store state to
/// `path` — the ONE function every autosave hook (a/b/c) calls, so the write
/// mechanic is never duplicated ad-hoc (26-RESEARCH.md Pitfall 2's discipline,
/// applied to autosave instead of the swap itself). Atomic by construction
/// (write_project_atomic = temp-file + rename), so a crash mid-write can never
/// truncate a `.rud` (threat T-26-06).
fn autosave_project(path: &Path, project: &Project) -> Result<(), String> {
    project_store::write_project_atomic(path, project)
}

/// Phase 26 (LIB-02, SC-1/SC-4): create a brand-new, empty, NAMED project and
/// switch to it. Hook (a): the OUTGOING active project's current state (if any)
/// is autosaved BEFORE the swap, so no work is lost. Hook (b): the new project's
/// own initial state is autosaved immediately after, so get_projects sees it
/// right away without waiting for a later edit. `Store::from_project` (the swap
/// primitive) resets undo/redo/open_turn for free (SC-4) — this function does
/// not touch undo bookkeeping itself. `sanitize_project_name` runs FIRST, before
/// any path join (threat T-26-01).
pub fn run_new_project<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<String, String> {
    // Phase 58 (58-CONTEXT D-11): switching projects cancels every in-flight
    // proxy generation. The media the encodes were started for is about to leave
    // the store, so continuing to burn a hardware encoder session on it would be
    // work for a library nobody is looking at any more.
    crate::proxy_job::cancel_all();
    // Phase 59 (59-CONTEXT D-24), standing beside it for the same reason and
    // one altitude up: an in-flight SEGMENT render is a render of an
    // arrangement that is about to stop existing. It would abort by itself the
    // moment the store's seq moved (59-07's `StaleAborted`), but only after
    // paying for one more tick and one more encoder second — and the two
    // background encoders share ONE admission, so a render that lingers is a
    // proxy that cannot start.
    crate::render_cache_job::request_cancel_all();
    // 59-REVIEW WR-06: and then FORGET what was learned about the old project.
    // Both registries are keyed by segment index — a coordinate on a global
    // program-time grid — so every mark and every row means something completely
    // different in the project about to be loaded. Carrying them over spends the
    // shared encoder permit on ranges nobody measured and lets a refusal earned
    // in one project permanently disable a segment index in another. AFTER the
    // cancel, because a forgotten row is a latch nobody can raise.
    crate::render_cache_job::forget_all_rows();
    preview::render_cache_detect::forget_all_heat();
    let store = ctx.store();
    let name = input
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "new_project requires name".to_string())?;
    project_store::sanitize_project_name(name)?; // T-26-01

    let dir = project_store::projects_dir(ctx)?;
    let path = dir.join(format!("{name}.rud"));
    if path.exists() {
        return Err(format!("a project named \"{name}\" already exists"));
    }

    let meta = ctx.active_project_meta();
    // Hook (a): autosave the OUTGOING project before swapping.
    {
        let old_path = meta
            .0
            .lock()
            .map_err(|_| "project meta poisoned".to_string())?
            .clone();
        if let Some(old_path) = old_path {
            let snapshot = store
                .lock()
                .map_err(|_| "backend store mutex poisoned".to_string())?
                .snapshot();
            autosave_project(&old_path, &snapshot)?;
        }
    }

    let mut fresh = Project::new();
    fresh.name = name.to_string();
    *store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())? =
        Store::from_project(fresh.clone());
    *meta
        .0
        .lock()
        .map_err(|_| "project meta poisoned".to_string())? = Some(path.clone());

    // Hook (b): persist the new project's initial state immediately.
    autosave_project(&path, &fresh)?;

    // Live-UAT MULTIPROJECT-UI fix: the swap replaced the Store, but without a
    // `project:changed` emission the renderer never re-fetched its snapshot and
    // kept showing (and editing against!) the PREVIOUS project's mirror. Emit
    // the notification-only ProjectSwitched patch through the SAME
    // emit_changed/Patch mechanism every mutation uses — the frontend's
    // rebuildMirror does a FULL get_snapshot rebuild on ANY project:changed
    // patch, and the native preview's edit-seq listener re-presents the still.
    //
    // Phase 43 (LAT-02): the Store was REPLACED above, so the new store's seq
    // is 0 and this notification-only patch describes the swap itself —
    // (0, 0) is the honest pair. A `ProjectSwitched` is exactly the case
    // D-07 carves out: the renderer full-resyncs here regardless, and
    // `get_current_seq` re-baselines it against the new store.
    ctx.emit_patch(
        &Patch {
            kind: PatchKind::ProjectSwitched,
            ids: vec!["project".to_string()],
            entities: None,
        },
        0,
        0,
    )?;

    // Quick 260828-h0u (gate 1): START THE CLOCK. Until today the render-cache
    // pump was born only inside `render_cache_job::poll_and_spawn`, whose only
    // caller is the transport funnel — so a project that was created (or
    // opened) and never played had no clock at all, and warmed nothing however
    // long it sat. `ensure_pump_started` is that funnel's own preamble,
    // extracted: kill switch, cache dir, slot configuration, `Once`-guarded
    // start. Idempotent and store-lock-free, so calling it here costs one
    // `var_os` plus one path join and can deadlock nothing.
    //
    // No pre-arm sweep here, unlike `swap_to_loaded_project`: `Project::new()`
    // has an empty timeline, so `prearm_pairs` would return `[]`.
    crate::render_cache_job::ensure_pump_started(ctx);

    Ok(format!(
        "Created and switched to project \"{name}\". This is NOT undoable and does not \
         affect any other project's undo history — the new project starts with a fresh, \
         empty undo stack. Re-fetch get_timeline before addressing any clip/track by id — \
         ids from the previous project no longer apply."
    ))
}

/// Phase 26: the `new_project` interception. Never panics: any failure becomes
/// an `is_error` tool_result so a broken create never aborts the agent turn.
pub fn handle_new_project<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_new_project(ctx, input) {
        Ok(msg) => result(msg, None),
        Err(e) => result(format!("new_project failed: {e}"), Some(true)),
    }
}

/// **Quick 260828-h0u (gate 2): the structural pre-arm sweep.**
///
/// A pure query over the ARRANGEMENT, feeding D-12's already-existing
/// `preview::render_cache_detect::prearm_stack` inlet so a heavy timeline is
/// armed AT OPEN rather than only by playing it. No decode, no frames, no
/// store lock: it is computed off the OWNED `loaded` [`Project`] before that
/// value moves into the store, which is what makes 61-RESEARCH §A4's deadlock
/// invariant hold here by construction rather than by discipline.
///
/// # Why this inlet, rather than a new one
///
/// Both detector inlets (`note_live_tick` and `prearm_stack`) were reachable
/// only from the live producer tick (`preview`'s ring), and
/// [`swap_to_loaded_project`] forgets all heat on every open — so a project
/// that was opened and never played had nothing to warm, however long the pump
/// ticked. `prearm_stack`'s own doc calls it "a PREDICTION, not evidence";
/// this is the structural caller it was designed for.
///
/// # The count is the SAME count the producer would report
///
/// `preview`'s ring feeds `prearm_stack` `stack.layers.len()`, and
/// `resolve_multilayer` builds `layers` 1:1 from
/// [`rudis_core::Timeline::active_layers_at`] hits — so
/// `active_layers_at(pos).len()` here IS what the live path would see at
/// `pos`. Layer depth is piecewise constant between clip boundaries, so
/// sampling each boundary interval ONCE is exact, not approximate.
///
/// # Where the policy lives, and why not here
///
/// `prearm_stack` early-returns at or below `PREARM_LAYER_THRESHOLD`. This
/// sweep passes every `(segment, depth)` pair rather than duplicating that
/// threshold at a second site — one decision, one place.
///
/// # The bound (T-h0u-01), and why it is the registry's own number
///
/// A `.rud` is a document on disk. Nothing rejects a clip whose `out_us` is
/// `i64::MAX`, and one such clip spans ~4.6e12 segments — so an UNBOUNDED
/// sweep would freeze project-open solid on a corrupt or hostile file. The
/// registry it feeds is LRU-bounded at
/// [`preview::render_cache_detect::MAX_TRACKED_SEGMENTS`] (4096, ~2.3 hours at
/// the 2 s pitch), so every pair past that many would evict one of its own
/// predecessors anyway: emitting them is provably wasted work, and stopping
/// there is not a policy choice but arithmetic.
///
/// Stopping EARLY rather than truncating late is also the better half to keep.
/// The playhead opens at 0, so the lowest segments are the ones a user reaches
/// first; a sweep that ran to the end would leave the LAST 4096 armed and the
/// beginning of the timeline cold.
fn prearm_pairs(timeline: &rudis_core::Timeline) -> Vec<(i64, usize)> {
    let mut events: Vec<i64> = Vec::new();
    for track in timeline
        .tracks
        .iter()
        .filter(|t| t.kind == rudis_core::TrackKind::Video)
    {
        for clip in &track.clips {
            events.push(clip.start_us);
            events.push(clip.timeline_end_us());
        }
    }
    events.sort_unstable();
    events.dedup();

    let mut out = Vec::new();
    'sweep: for w in events.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        // Depth is constant across [a, b), so ONE sample answers the whole
        // interval. `active_at` membership is start-inclusive/end-exclusive,
        // which is why the last segment is derived from `b - 1` and not `b`.
        let depth = timeline.active_layers_at(a).len();
        let first = preview::render_cache_lookup::segment_index_for(a);
        let last = preview::render_cache_lookup::segment_index_for(b - 1);
        for seg in first..=last {
            // T-h0u-01: see this function's doc. The registry cannot hold more
            // than this, so a pair past it would evict one of its own.
            if out.len() >= preview::render_cache_detect::MAX_TRACKED_SEGMENTS {
                break 'sweep;
            }
            out.push((seg, depth));
        }
    }
    out
}

//// The ONE project-swap mechanic, shared by both open entry points: cancel
/// background work, autosave the outgoing project (hook (a)), install the
/// loaded project, point `active_project_meta` at `path`, emit
/// `ProjectSwitched`, re-arm proxies. **Extracted rather than duplicated** —
/// the same discipline [`autosave_project`] applies to the WRITE, applied here
/// to the SWITCH (26-RESEARCH.md Pitfall 2).
///
/// Phase 60.1 (plan 02) is why it exists as a function at all:
/// [`run_open_project_at_path`] needs this exact tail, and a second copy of it
/// would be a second place for the ordering below to rot. Nothing in this body
/// changed in the move; only its address did.
///
/// ⚠ Three ordering facts, every one of them load-bearing and every one of them
/// easy to lose in a move:
///
/// * The four cancel/forget calls run **first**, before anything reads the
///   store — a forgotten row is a latch nobody can raise (59-REVIEW WR-06).
/// * `rearm` is collected off `loaded` **before** it moves into the store, so no
///   store guard is alive when the spawns happen (58-REVIEW WR-05).
/// * `emit_patch(ProjectSwitched, 0, 0)` — the `(0, 0)` pair stays, because
///   [`Store::from_project`] has just reset the seq and that is the honest pair
///   (Phase 43, LAT-02, D-07).
///
/// Takes `loaded` **by value**: the caller has already read the project off
/// disk, which is what lets a load failure abort before a single cancel latch
/// is raised or a single byte is autosaved.
fn swap_to_loaded_project<C: AppCtx>(
    ctx: &C,
    path: &Path,
    loaded: Project,
) -> Result<(), String> {
    // Phase 58 (58-CONTEXT D-11): opening a project cancels every in-flight
    // proxy generation, for the same reason `run_new_project` does — see there.
    crate::proxy_job::cancel_all();
    // Phase 59 (59-CONTEXT D-24): and every in-flight segment render, likewise.
    crate::render_cache_job::request_cancel_all();
    // 59-REVIEW WR-06, standing beside `run_new_project`'s pair for the same
    // reason — see there.
    crate::render_cache_job::forget_all_rows();
    preview::render_cache_detect::forget_all_heat();
    let store = ctx.store();

    let meta = ctx.active_project_meta();
    // Hook (a): autosave the OUTGOING project before loading the next.
    {
        let old_path = meta
            .0
            .lock()
            .map_err(|_| "project meta poisoned".to_string())?
            .clone();
        if let Some(old_path) = old_path {
            let snapshot = store
                .lock()
                .map_err(|_| "backend store mutex poisoned".to_string())?
                .snapshot();
            autosave_project(&old_path, &snapshot)?;
        }
    }

    // Phase 58 (58-REVIEW WR-05): collect the proxy re-arm's inputs HERE, off
    // the loaded project, before it moves into the store — OWNED values only, so
    // no store guard is alive when the spawns happen further down (the same
    // lock-discipline rule `import_one_path` follows beside its own spawn).
    //
    // This is the one production project-LOAD site in the app, so it is the one
    // place a project's media can be re-armed. See `proxy_job::rearm_project_media`
    // for what it does, what it deliberately does not, and what it costs.
    let rearm: Vec<(std::path::PathBuf, u32, u32)> = loaded
        .media_bin
        .iter()
        .filter(|item| item.media_kind == rudis_core::MediaKind::Video)
        .map(|item| (std::path::PathBuf::from(&item.path), item.width, item.height))
        .collect();

    // Quick 260828-h0u (gate 2): and collect the PRE-ARM sweep here too, for
    // the same WR-05 reason and one stronger one — computing it off the owned
    // `loaded` value means the store mutex is never taken anywhere near
    // `prearm_stack`'s registry lock (61-RESEARCH §A4). It is applied further
    // down, not here; see the ordering note at the application site.
    let prearm = prearm_pairs(&loaded.timeline);

    *store
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())? = Store::from_project(loaded);
    *meta
        .0
        .lock()
        .map_err(|_| "project meta poisoned".to_string())? = Some(path.to_path_buf());

    // Live-UAT MULTIPROJECT-UI fix — same emission as run_new_project: without
    // it the renderer's mirror (timeline + media bin) kept showing the previous
    // project while the backend's active project had already swapped.
    // Phase 43 (LAT-02): (0, 0) — the Store was just replaced, so the new
    // store's seq baseline IS 0 (see run_new_project's note).
    ctx.emit_patch(
        &Patch {
            kind: PatchKind::ProjectSwitched,
            ids: vec!["project".to_string()],
            entities: None,
        },
        0,
        0,
    )?;

    // Quick 260828-h0u (gate 2): ARM THE HEAT. Three legs, every one of them
    // load-bearing:
    //
    // (1) AFTER `forget_all_heat()` above, by construction — the sweep is
    //     never erased by the open it belongs to, which is what made a project
    //     that was opened and never played warm nothing at all;
    // (2) AFTER `Store::from_project` installed the INCOMING project — a pump
    //     tick landing in the gap must never find NEW marks over the OLD
    //     store and bake the outgoing arrangement under the incoming
    //     project's segment indices;
    // (3) computed off the OWNED `loaded` value, so no store lock is taken or
    //     held anywhere near `prearm_stack`'s registry lock (61-RESEARCH §A4:
    //     `AppCtx::store()` and `PreviewHost::store()` are the same
    //     non-reentrant mutex).
    for (seg, depth) in prearm {
        preview::render_cache_detect::prearm_stack(seg, depth);
    }

    // Quick 260828-h0u (gate 1): START THE CLOCK, at the site the measured
    // defect names. This is the shared tail of BOTH open entry points
    // (`run_open_project` and `run_open_project_at_path`), so one call covers
    // both. See `render_cache_job::ensure_pump_started` for the measurement and
    // for why a second caller of the same `Once`-guarded start is free.
    //
    // Order relative to `rearm_project_media` below does not matter for
    // fairness: the pump defers to pending proxy work per TICK (D-10,
    // `proxy_job::has_pending_work`), not at start.
    crate::render_cache_job::ensure_pump_started(ctx);

    // Phase 58 (58-REVIEW WR-05): the project is live and no guard is held —
    // re-arm proxy generation for its heavy media. Detached and best-effort: a
    // source that already has a current proxy short-circuits on a stats-plus-one
    // -bounded-read freshness check, and one that does not gets a background
    // encode at below-normal priority. Never blocks the switch.
    crate::proxy_job::rearm_project_media(ctx, &rearm);

    Ok(())
}

/// Phase 26 (LIB-02, SC-1/SC-4): switch the active project to a PREVIOUSLY
/// CREATED one, resolved EXCLUSIVELY through the real on-disk registry
/// (`scan_known_projects`) — NEVER by joining `name` into a raw path directly
/// (T-26-03: this is STRICTER than accepting any path the caller names,
/// mirroring `run_export_project`'s "no LLM value ever reaches the path"
/// discipline, here applied as "the LLM's name is only ever a LOOKUP KEY against
/// an app-owned, pre-enumerated directory"). Hook (a): the outgoing project is
/// autosaved before loading the next.
///
/// # Phase 60.1 (plan 02): the tail moved, the contract did not
///
/// Everything from the cancels onward now lives in [`swap_to_loaded_project`],
/// which [`run_open_project_at_path`] shares. **The name-resolution statements
/// below, and BOTH of this function's output strings, are unchanged** — that is
/// deliberate and it is the whole shape of the phase's answer to T-26-03.
/// Widening this function with an optional `path` argument is precisely what
/// that recommendation forbids, because `name` may come from the LLM. The
/// user-picked-path route is therefore a SIBLING function taking a type that
/// cannot hold an unvalidated path, never a new argument here.
pub fn run_open_project<C: AppCtx>(
    ctx: &C,
    input: &serde_json::Value,
) -> Result<String, String> {
    let name = input
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "open_project requires name".to_string())?;
    project_store::sanitize_project_name(name)?;

    let dir = project_store::projects_dir(ctx)?;
    let known = project_store::scan_known_projects(&dir);
    // INFO: first-hit by embedded Project.name. Two `.rud` files sharing a name
    // (only reachable by a manual file copy — new_project rejects a duplicate
    // name structurally) resolve ambiguously to the first scanned; low-risk for
    // a single-user local app (26-RESEARCH.md A4), documented here rather than
    // guarded.
    let entry = known
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| {
            format!("no known project named \"{name}\" — call get_projects to see what exists")
        })?;

    let loaded = project_store::load_project(&entry.path)?;
    swap_to_loaded_project(ctx, &entry.path, loaded)?;

    Ok(format!(
        "Switched to project \"{name}\". This is NOT undoable and does not affect any other \
         project's undo history — this project's undo stack starts fresh and empty. \
         Re-fetch get_timeline before addressing any clip/track by id — ids from the \
         previous project no longer apply."
    ))
}

/// Open a `.rud` the USER picked, from ANYWHERE on disk (ROADMAP § 60.1 scope
/// item 3).
///
/// **A SIBLING of [`run_open_project`], not a widening of it**, and the reason
/// is T-26-03: that function's `name` may come from the LLM, so it resolves
/// exclusively through the app-owned registry and never joins caller text into
/// a path. The threat that recommendation defends against is an LLM-chosen
/// path, not a user-chosen one — Rudis has accepted user-picked absolute paths
/// across this same ABI since Phase 47 (`rudis_import_media`). The distinction
/// is **who chose the path**, and it lives in the TYPE:
/// [`project_store::RudProjectPath`] cannot be constructed from an unvalidated
/// string (its hand-written `Deserialize` is its only constructor), so this
/// function is structurally incapable of being called with one.
/// `run_open_project`'s name route is untouched.
///
/// Host-only: there is deliberately **no** `handle_open_project_at_path`
/// interception and **no** entry in `agent_turn`'s tool dispatch. That absence
/// is the mechanical proof T-26-03 was not weakened, and plan 60.1-08's sweep
/// asserts it.
///
/// Returns JSON — `{"path": "..", "name": ".."}` — rather than the agent-facing
/// prose `run_open_project` returns, because this function has no agent
/// consumer. `run_get_projects` set that precedent for host-facing reads.
pub fn run_open_project_at_path<C: AppCtx>(
    ctx: &C,
    path: RudProjectPath,
) -> Result<String, String> {
    let loaded = project_store::load_project(path.as_path())?;
    // The name travels back to the host from the LOADED document, not from the
    // file stem: `.rud` carries `Project.name`, and a user who renamed the file
    // in Explorer has not renamed the project inside it. Read before the move
    // into the store, so nothing below needs a store guard to answer.
    let name = loaded.name.clone();

    // The SAME tail `run_open_project` runs — cancel, autosave the outgoing
    // project, install, re-point the meta, emit, re-arm. One mechanic, two
    // doors.
    swap_to_loaded_project(ctx, path.as_path(), loaded)?;

    serde_json::to_string(&serde_json::json!({
        "path": path.as_path().to_string_lossy(),
        "name": name,
    }))
    .map_err(|e| e.to_string())
}

/// Phase 26: the `open_project` interception. Never panics: any failure becomes
/// an `is_error` tool_result.
pub fn handle_open_project<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_open_project(ctx, input) {
        Ok(msg) => result(msg, None),
        Err(e) => result(format!("open_project failed: {e}"), Some(true)),
    }
}

// ---------------------------------------------------------------------------
// Phase 60.1 (plan 02, PROJ-01/02): SAVE — the capability the shipped app was
// missing entirely.
// ---------------------------------------------------------------------------
// `write_project_atomic` had exactly FOUR call sites tree-wide before this, and
// not one of them was a manual save or a shutdown (60.1-RESEARCH Finding A):
// project-switch, project-create, and end-of-agent-turn. So a user who trimmed
// a clip with the mouse and closed the window lost the trim. These two
// functions are where that stops being true; they are the fifth and sixth call
// sites of the one write mechanic, and no `fs::write` is added anywhere
// (T-26-06).
// ---------------------------------------------------------------------------

/// The first free `Untitled` / `Untitled 2` / `Untitled 3` … in `dir`.
///
/// The search is **capped**, not unbounded (T-60.1-09's shape applied to a
/// loop): a directory that somehow holds a thousand untitled projects is a
/// state to report, not one to spin in on the interop worker.
fn mint_untitled_name(dir: &Path) -> Result<String, String> {
    for n in 1..=MAX_UNTITLED_CANDIDATES {
        let candidate = if n == 1 {
            "Untitled".to_string()
        } else {
            format!("Untitled {n}")
        };
        if !dir.join(format!("{candidate}.rud")).exists() {
            // T-60.1-07: the minted name becomes a real path component, so it
            // goes through the ONE owner of that rule before it does — even
            // though this generator cannot produce an unsafe name today. A
            // guard that only holds while the generator stays simple is a guard
            // that fails the day it does not.
            project_store::sanitize_project_name(&candidate)?;
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not mint an unused project name after {MAX_UNTITLED_CANDIDATES} tries"
    ))
}

/// The cap on [`mint_untitled_name`]'s collision search.
const MAX_UNTITLED_CANDIDATES: u32 = 1000;

/// Persist the ACTIVE project to disk, **now**.
///
/// Host-only — there is no agent tool for it: `Ctrl+S` and save-on-close are
/// its callers, and an LLM that could silently overwrite the user's file on
/// disk is not a capability this phase wants. Returns `{"path": "..", "seq": N}`.
///
/// # It MINTS rather than refuses when nothing is active
///
/// With `active_project_meta == None` this does **not** fail. It creates
/// `projects_dir/Untitled.rud` (then `Untitled 2`, `Untitled 3`, …), renames
/// the live document to match, points the meta at it, and writes.
///
/// That is a deliberate product decision, not a convenience. Rudis is for
/// "people with no editing experience" (PROJECT.md), the entire failure this
/// phase exists to fix is silent data loss at close, and a modal asking a
/// beginner to name a file **before** their work is safe is precisely the
/// moment the work gets lost — they cancel it. Save As is how they name it
/// properly, afterwards, once nothing is at stake. It is also what lets the
/// close path have no dialog and no branch at all: close always saves.
///
/// # The `seq` in the envelope is the honest dot
///
/// It is the store's mutation counter at the instant the bytes were captured,
/// so a host can tell whether the store has moved since. There is deliberately
/// **no** separate "is dirty" export: the value rides the save's own envelope,
/// and after any open or new the store's seq is 0 with the project already on
/// disk, so "nothing unsaved" is true by construction rather than by a flag
/// somebody has to remember to clear.
pub fn run_save_project<C: AppCtx>(ctx: &C) -> Result<String, String> {
    // Two locks, never nested (the rule hook (a) already follows): read the
    // meta, CLONE the Option, drop the guard, and only then reach for the
    // store.
    let existing = ctx
        .active_project_meta()
        .0
        .lock()
        .map_err(|_| "project meta poisoned".to_string())?
        .clone();

    // Mint only when nothing is active. `minted` doubles as the "did the live
    // document just get renamed?" flag the emission below reads.
    let (target, minted) = match existing {
        Some(path) => (path, None),
        None => {
            let dir = project_store::projects_dir(ctx)?;
            let name = mint_untitled_name(&dir)?;
            (dir.join(format!("{name}.rud")), Some(name))
        }
    };

    // ONE store acquisition. LAT-02's rule: the snapshot and the seq that
    // describes it are captured in the SAME guard, never by re-locking, or the
    // envelope reports a seq for bytes it did not come from.
    let (snapshot, seq) = {
        let mut store = ctx
            .store()
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        if let Some(name) = minted.clone() {
            store.set_project_name(name);
        }
        (store.snapshot(), store.seq())
    };

    if minted.is_some() {
        *ctx.active_project_meta()
            .0
            .lock()
            .map_err(|_| "project meta poisoned".to_string())? = Some(target.clone());
    }

    // The ONE write mechanic — temp file, unique nonce, rename, cleanup. Never
    // an ad-hoc `fs::write` (T-26-06). This is its fifth call site; keep it
    // that way.
    project_store::write_project_atomic(&target, &snapshot)?;

    // The minted rename must reach the shell, for the same reason
    // `run_new_project`'s emission exists (the MULTIPROJECT-UI defect): the
    // TitleBar and the mirror would otherwise keep describing a document that
    // no longer has that identity. Emitted ONLY when something was renamed —
    // `ProjectSwitched` is a structural kind and `ShellMirror.ApplyAsync`
    // routes structural kinds to a FULL resync, so emitting on every ordinary
    // `Ctrl+S` would make saving the most expensive operation in the app.
    // `(seq, seq)`, not `(0, 0)`: nothing here called `Store::from_project`, so
    // the counter the renderer tracks has not been reset and claiming it was
    // would be a lie.
    if minted.is_some() {
        ctx.emit_patch(
            &Patch {
                kind: PatchKind::ProjectSwitched,
                ids: vec!["project".to_string()],
                entities: None,
            },
            seq,
            seq,
        )?;
    }

    serde_json::to_string(&serde_json::json!({
        "path": target.to_string_lossy(),
        "seq": seq,
    }))
    .map_err(|e| e.to_string())
}

/// Persist to a user-chosen path **and re-point the active document there**.
///
/// The universal NLE convention, and the owner's decision for Rudis: after Save
/// As you are editing the copy, and every later save — including the autosave
/// on close — lands at the new path. A Save As that wrote a copy and left you
/// editing the original is the shape that loses the next hour of work.
///
/// The live project is renamed to the target's file stem, so the TitleBar and
/// the next registry scan agree with the filename the user just chose.
///
/// Returns `{"path": "..", "name": "..", "seq": N}`.
///
/// ⚠ Neither save function uses [`Store::from_project`]. It resets undo/redo,
/// and losing your undo history because you saved is a bug, not a feature.
pub fn run_save_project_as<C: AppCtx>(
    ctx: &C,
    path: RudSaveTargetPath,
) -> Result<String, String> {
    // The document takes the name the user just typed into the file dialog.
    // `RudSaveTargetPath` has already put this stem through
    // `sanitize_project_name`, so it is a legal project name by construction.
    let name = path
        .as_path()
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "a save target needs a name before the .rud".to_string())?
        .to_string();

    // ONE store acquisition: rename, then capture the snapshot and the seq that
    // describes it together (LAT-02). Deliberately NOT `Store::from_project` —
    // that resets undo/redo, and losing your history because you saved a copy
    // is a bug, not a feature.
    let (snapshot, seq) = {
        let mut store = ctx
            .store()
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        store.set_project_name(name.clone());
        (store.snapshot(), store.seq())
    };

    project_store::write_project_atomic(path.as_path(), &snapshot)?;

    // Re-point the active document AFTER the bytes are safely down: a Save As
    // that failed to write must not leave the app editing a file that does not
    // exist.
    *ctx.active_project_meta()
        .0
        .lock()
        .map_err(|_| "project meta poisoned".to_string())? =
        Some(path.as_path().to_path_buf());

    // The shell full-resyncs and the TitleBar picks up the new name.
    // `ProjectSwitched` is a STRUCTURAL patch kind and `ShellMirror.ApplyAsync`
    // already routes structural kinds to `FullResyncAsync`, so this needs no
    // seventh event type — `ring::EVENT_NAMES` stays at 6, as it has through
    // five consecutive phases. `(seq, seq)` is the honest pair: unlike a
    // project switch, nothing reset the counter.
    ctx.emit_patch(
        &Patch {
            kind: PatchKind::ProjectSwitched,
            ids: vec!["project".to_string()],
            entities: None,
        },
        seq,
        seq,
    )?;

    serde_json::to_string(&serde_json::json!({
        "path": path.as_path().to_string_lossy(),
        "name": name,
        "seq": seq,
    }))
    .map_err(|e| e.to_string())
}

/// Phase 26 (TOOL-04): list every known project by name plus which one is
/// currently active. Deliberately returns ONLY {name, isActive} pairs --
/// never the raw filesystem path (minimal-disclosure, threat T-26-08: the
/// agent addresses projects by name, mirroring Phase 25's
/// path-addressed-not-id-addressed philosophy; the real path stays an
/// app-internal registry-lookup detail, never round-tripped to the LLM).
pub fn run_get_projects<C: AppCtx>(ctx: &C) -> Result<String, String> {
    let dir = project_store::projects_dir(ctx)?;
    let known = project_store::scan_known_projects(&dir);
    let meta = ctx.active_project_meta();
    let active_path = meta
        .0
        .lock()
        .map_err(|_| "project meta poisoned".to_string())?
        .clone();
    let arr: Vec<serde_json::Value> = known
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "isActive": active_path.as_deref() == Some(e.path.as_path()),
            })
        })
        .collect();
    serde_json::to_string(&arr).map_err(|e| e.to_string())
}

/// T-60.1-06: the longest `Project.name` a host will be asked to render.
///
/// `scan_known_projects` reads that string out of each `.rud`, and anything
/// with write access to the projects directory can put an arbitrary one there —
/// `sanitize_project_name`'s own 255-byte rule only ever ran on names the APP
/// minted. XAML `TextBlock.Text` is inert, so this is not an injection risk; it
/// is a layout DoS, and the control is a bound.
const MAX_LISTED_NAME_BYTES: usize = 255;

/// Cap `name` at [`MAX_LISTED_NAME_BYTES`], never splitting a UTF-8 character.
///
/// It CAPS rather than dropping the row: a project whose name is hostile is
/// still the user's project, and a list that silently omits a file that exists
/// is worse than one that shows a long name short.
fn cap_listed_name(name: &str) -> &str {
    if name.len() <= MAX_LISTED_NAME_BYTES {
        return name;
    }
    let mut end = MAX_LISTED_NAME_BYTES;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// A HUMAN's project list.
///
/// An additive **sibling** of [`run_get_projects`], never a widening of it.
/// T-26-08 withholds the filesystem path from the LLM deliberately (minimal
/// disclosure) — and a person choosing between two projects both called
/// "Untitled" needs exactly the path and the mtime that posture withholds. Two
/// consumers, two contracts, one directory scan.
///
/// Returns `[{"name":"..","path":"..","isActive":bool,"modifiedUnixMs":u64}, ..]`.
pub fn run_get_projects_detailed<C: AppCtx>(ctx: &C) -> Result<String, String> {
    // The SAME scan `run_get_projects` runs — never a hand-rolled `read_dir` +
    // parse. The LAT-04 sidecar index, its `(mtime, size)` staleness key and
    // the 8-thread scan above 16 files all live in that one function, and a
    // second listing here would be a second thing to keep correct.
    let dir = project_store::projects_dir(ctx)?;
    let known = project_store::scan_known_projects(&dir);
    let active_path = ctx
        .active_project_meta()
        .0
        .lock()
        .map_err(|_| "project meta poisoned".to_string())?
        .clone();
    let arr: Vec<serde_json::Value> = known
        .iter()
        .map(|e| {
            // `0` on ANY metadata failure rather than dropping the row: a
            // project the user can see and open is worth more than a timestamp.
            let modified_unix_ms = std::fs::metadata(&e.path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            serde_json::json!({
                "name": cap_listed_name(&e.name),
                "path": e.path.to_string_lossy(),
                "isActive": active_path.as_deref() == Some(e.path.as_path()),
                "modifiedUnixMs": modified_unix_ms,
            })
        })
        .collect();
    serde_json::to_string(&arr).map_err(|e| e.to_string())
}

/// Which of the ACTIVE project's media files are not on disk right now — **ids
/// only** (T-26-08's posture; the host already holds each item's path in its
/// own mirror, and the MediaBin keys its tiles by id).
///
/// A pure poll, deliberately **not** a seventh event type: `ring::EVENT_NAMES`
/// has stayed at 6 through five consecutive phases that each added exactly one
/// export, and retrieval rides the existing 100 ms cold poll exactly as
/// waveform peaks, filmstrip strips, proxy status and render-cache status
/// already do (58-CONTEXT D-29, 59-CONTEXT D-30).
///
/// An empty array when no project is active — an absent project has no missing
/// media, and that is not an error.
pub fn run_get_missing_media<C: AppCtx>(ctx: &C) -> Result<String, String> {
    let active = ctx
        .active_project_meta()
        .0
        .lock()
        .map_err(|_| "project meta poisoned".to_string())?
        .is_some();
    if !active {
        return Ok("[]".to_string());
    }

    // Snapshot, then DROP the guard, and only then stat the filesystem
    // (T-60.1-09). This runs on the single interop worker that also services
    // `rudis_poll_events`, so holding the store lock across one `is_file()` per
    // media item would stall every other command for the duration of a cold
    // directory walk on a network drive.
    let snapshot = ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .snapshot();
    serde_json::to_string(&project_store::missing_media(&snapshot))
        .map_err(|e| e.to_string())
}

/// Phase 26: the `get_projects` interception. Read-only (no input, no
/// mutation); never panics: any failure becomes an `is_error` tool_result.
pub fn handle_get_projects<C: AppCtx>(
    ctx: &C,
    tool_use_id: &str,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match run_get_projects(ctx) {
        Ok(json) => result(json, None),
        Err(e) => result(format!("get_projects failed: {e}"), Some(true)),
    }
}

// ---------------------------------------------------------------------------
// Tests (Phase 60.1, plan 02)
// ---------------------------------------------------------------------------
// `project.rs` had NO test module before this phase: every assertion about
// `run_open_project` lived in `proxy_job` and `render_cache_job`, and each of
// those drives it only to prove ITS OWN side effect (the proxy re-arm, the two
// registry forgets). Nothing pinned the name-resolution contract itself.
//
// That matters here specifically, because plan 60.1-02 extracts
// `swap_to_loaded_project` out of this function's tail and then adds a SECOND
// entry point beside it. A refactor with no characterization test under it is a
// promise; these are the test.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestAppCtx;
    use rudis_core::Command;

    /// Every test in this module drives `run_new_project` / `run_open_project`,
    /// and BOTH of those raise `proxy_job::cancel_all` and forget both
    /// process-global render-cache registries. Those statics are shared with
    /// `proxy_job::tests` and `render_cache_job::tests`, whose own leases both
    /// bottom out on this same mutex — so taking it here is what keeps a
    /// project switch in this module from erasing a neighbour's seeded heat
    /// mark mid-assertion. The `proxy_job::tests::encoder_lease` precedent,
    /// same static, same reason.
    fn switch_lease() -> std::sync::MutexGuard<'static, ()> {
        crate::proxy_job::encoder_lease()
    }

    /// A REAL undoable mutation on the live store — the thing an autosave must
    /// be proven to have captured. `SetProjectSettings` deliberately: it is a
    /// genuine `Command` round-trip through `dispatch` (undo entry, seq bump,
    /// patch) that touches NO media, so nothing in this module ever spawns a
    /// proxy encode or needs a Tokio runtime to exist.
    fn set_fps(ctx: &TestAppCtx, fps: f64) {
        ctx.store()
            .lock()
            .expect("store")
            .dispatch(Command::SetProjectSettings {
                fps,
                width: 1920,
                height: 1080,
            })
            .expect("the project settings apply");
    }

    fn active_path(ctx: &TestAppCtx) -> Option<std::path::PathBuf> {
        ctx.active_project_meta().0.lock().expect("meta").clone()
    }

    // -----------------------------------------------------------------------
    // The name-keyed entry point — pinned BEFORE the extraction that moves its
    // tail, so "behaviour preserved" is a measurement rather than a claim.
    // -----------------------------------------------------------------------

    #[test]
    fn run_open_project_opens_a_known_project_by_name() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();

        run_new_project(&ctx, &serde_json::json!({ "name": "Alpha" })).expect("create Alpha");
        run_new_project(&ctx, &serde_json::json!({ "name": "Beta" })).expect("create Beta");

        let msg = run_open_project(&ctx, &serde_json::json!({ "name": "Alpha" }))
            .expect("Alpha is a known project");

        assert!(
            msg.starts_with("Switched to project \"Alpha\"."),
            "the success prose is part of this tool's contract with the LLM: {msg}"
        );
        assert_eq!(
            ctx.store().lock().expect("store").snapshot().name,
            "Alpha",
            "the live store must now hold Alpha, not Beta"
        );
        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        assert_eq!(
            active_path(&ctx),
            Some(dir.join("Alpha.rud")),
            "and the active-project pointer must follow the swap"
        );
    }

    #[test]
    fn run_open_project_refuses_an_unknown_name_with_its_exact_message() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        run_new_project(&ctx, &serde_json::json!({ "name": "Alpha" })).expect("create Alpha");

        let err = run_open_project(&ctx, &serde_json::json!({ "name": "Ghost" }))
            .expect_err("no project named Ghost exists");

        assert_eq!(
            err,
            "no known project named \"Ghost\" — call get_projects to see what exists",
            "T-26-03's refusal text is the LLM's only cue to call get_projects; \
             it must survive the extraction byte for byte"
        );
        assert_eq!(
            ctx.store().lock().expect("store").snapshot().name,
            "Alpha",
            "and a refused open must not have swapped anything"
        );
    }

    #[test]
    fn run_open_project_autosaves_the_outgoing_project_first() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();

        run_new_project(&ctx, &serde_json::json!({ "name": "Target" })).expect("create Target");
        run_new_project(&ctx, &serde_json::json!({ "name": "Outgoing" }))
            .expect("create Outgoing");

        // A real edit made AFTER the last write of Outgoing.rud — so if hook
        // (a) does not run, the file on disk still says 30 fps.
        set_fps(&ctx, 24.0);

        run_open_project(&ctx, &serde_json::json!({ "name": "Target" })).expect("open Target");

        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        let persisted = project_store::load_project(&dir.join("Outgoing.rud"))
            .expect("Outgoing.rud is on disk");
        assert_eq!(
            persisted.fps, 24.0,
            "hook (a): the OUTGOING project's current store state is written \
             BEFORE the swap. Proven by reading the .rud back, not by the \
             return value."
        );
    }

    // -----------------------------------------------------------------------
    // The PATH-keyed sibling — a `.rud` the user picked with a file dialog
    // -----------------------------------------------------------------------

    /// A directory that is NOT `projects_dir` — where a `.rud` a user picked
    /// with a file dialog actually lives.
    fn outside_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rudis-project-open-at-path-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the outside dir");
        dir
    }

    /// A project carrying one real clip on track 0, so "the store holds THAT
    /// project" is provable by a clip id rather than by a name string a
    /// freshly-created project could also carry.
    fn project_with_clip(name: &str, clip_id: &str) -> Project {
        let mut project = Project::new();
        project.name = name.to_string();
        project.timeline.tracks[0].clips.push(rudis_core::Clip {
            id: clip_id.to_string(),
            media_id: "m-outside".to_string(),
            start_us: 0,
            in_us: 0,
            out_us: 2_000_000,
            volume: 1.0,
            audio_detached: false,
            transform: Default::default(),
            opacity: 1.0,
            crop: Default::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        });
        project
    }

    fn clip_ids(ctx: &TestAppCtx) -> Vec<String> {
        ctx.store()
            .lock()
            .expect("store")
            .snapshot()
            .timeline
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.id.clone()))
            .collect()
    }

    /// The ONLY way to make a [`project_store::RudProjectPath`] — its
    /// hand-written `Deserialize`. A test cannot fabricate one either, which is
    /// the point of the type.
    fn open_path(p: &std::path::Path) -> project_store::RudProjectPath {
        serde_json::from_value(serde_json::Value::String(
            p.to_string_lossy().into_owned(),
        ))
        .expect("the fixture path validates as an existing .rud")
    }

    #[test]
    fn run_open_project_at_path_opens_a_rud_from_outside_the_projects_dir() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();

        let elsewhere = outside_dir("outside");
        let rud = elsewhere.join("Picked From Downloads.rud");
        project_store::write_project_atomic(&rud, &project_with_clip("Picked", "clip-from-disk"))
            .expect("write the fixture .rud");

        let json = run_open_project_at_path(&ctx, open_path(&rud))
            .expect("a validated .rud outside projects_dir opens");

        assert_eq!(
            clip_ids(&ctx),
            vec!["clip-from-disk".to_string()],
            "the LIVE store must carry the opened project's clips — this is the \
             whole capability, and a return value that says so proves nothing"
        );

        // The JSON shape is a contract: plans 03 and 04 build directly on it.
        let v: serde_json::Value = serde_json::from_str(&json).expect("the return is JSON");
        assert_eq!(
            v.get("name").and_then(|n| n.as_str()),
            Some("Picked"),
            "the envelope carries the opened project's NAME"
        );
        assert_eq!(
            v.get("path").and_then(|p| p.as_str()).map(std::path::PathBuf::from),
            Some(std::fs::canonicalize(&rud).expect("canonicalize")),
            "and its canonical path"
        );
    }

    #[test]
    fn run_open_project_at_path_autosaves_the_outgoing_project_first() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();

        run_new_project(&ctx, &serde_json::json!({ "name": "Outgoing" }))
            .expect("create Outgoing");
        // A real edit made after Outgoing.rud was last written.
        set_fps(&ctx, 24.0);

        let elsewhere = outside_dir("autosave");
        let rud = elsewhere.join("Incoming.rud");
        project_store::write_project_atomic(&rud, &project_with_clip("Incoming", "c-in"))
            .expect("write the fixture .rud");

        run_open_project_at_path(&ctx, open_path(&rud)).expect("open the picked file");

        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        let persisted = project_store::load_project(&dir.join("Outgoing.rud"))
            .expect("Outgoing.rud is on disk");
        assert_eq!(
            persisted.fps, 24.0,
            "hook (a) belongs to the SWAP, not to the name lookup: opening at a \
             path must autosave the outgoing project exactly as opening by name \
             does. Both entry points share swap_to_loaded_project precisely so \
             this cannot be true of one and false of the other."
        );
    }

    #[test]
    fn run_open_project_at_path_points_the_active_meta_at_the_opened_path() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();

        let elsewhere = outside_dir("meta");
        let rud = elsewhere.join("Elsewhere.rud");
        project_store::write_project_atomic(&rud, &project_with_clip("Elsewhere", "c-e"))
            .expect("write the fixture .rud");

        run_open_project_at_path(&ctx, open_path(&rud)).expect("open the picked file");

        let canonical = std::fs::canonicalize(&rud).expect("canonicalize");
        assert_eq!(
            active_path(&ctx),
            Some(canonical.clone()),
            "the active document is the file that was opened"
        );
        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        assert_ne!(
            active_path(&ctx),
            Some(dir.join("Elsewhere.rud")),
            "and emphatically NOT a projects_dir join of its name — a later \
             autosave would then write over a managed project that has nothing \
             to do with the file the user opened"
        );
    }

    #[test]
    fn run_open_project_at_path_emits_project_switched() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();

        let elsewhere = outside_dir("emit");
        let rud = elsewhere.join("Emitted.rud");
        project_store::write_project_atomic(&rud, &project_with_clip("Emitted", "c-em"))
            .expect("write the fixture .rud");

        run_open_project_at_path(&ctx, open_path(&rud)).expect("open the picked file");

        let kinds: Vec<PatchKind> = ctx
            .emitted_patches()
            .into_iter()
            .map(|(p, _, _)| p.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![PatchKind::ProjectSwitched],
            "without this the shell's mirror keeps rendering — and editing \
             against — the project that is no longer loaded (the MULTIPROJECT-UI \
             defect run_new_project's own emission exists to fix)"
        );
    }

    // -----------------------------------------------------------------------
    // Save / Save As — the capability whose absence loses a mouse trim at close
    // -----------------------------------------------------------------------

    /// The ONLY way to make a [`project_store::RudSaveTargetPath`] — its
    /// hand-written `Deserialize`.
    fn save_target(p: &std::path::Path) -> project_store::RudSaveTargetPath {
        serde_json::from_value(serde_json::Value::String(
            p.to_string_lossy().into_owned(),
        ))
        .expect("the fixture save target validates")
    }

    fn envelope(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("the save return is JSON")
    }

    #[test]
    fn run_save_project_writes_the_live_store_to_the_active_path() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        run_new_project(&ctx, &serde_json::json!({ "name": "Saveable" }))
            .expect("create Saveable");

        // A real edit AFTER hook (b) wrote the project's initial state — the
        // mouse trim that used to die at close.
        set_fps(&ctx, 24.0);

        run_save_project(&ctx).expect("save the active project");

        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        let persisted = project_store::load_project(&dir.join("Saveable.rud"))
            .expect("Saveable.rud is on disk");
        assert_eq!(
            persisted,
            ctx.store().lock().expect("store").snapshot(),
            "the FILE must equal the live store, whole. Read back off disk, \
             because a return value saying \"saved\" is exactly the kind of \
             proof-of-wiring this phase exists to stop accepting."
        );
        assert_eq!(persisted.fps, 24.0, "and specifically the edit is in it");
    }

    #[test]
    fn run_save_project_mints_untitled_rather_than_losing_the_work() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();

        // No project was ever created: this is the app on first launch, with a
        // real edit already made. The pre-60.1 behaviour was to lose it.
        assert_eq!(active_path(&ctx), None, "fixture: nothing is active");
        set_fps(&ctx, 24.0);

        run_save_project(&ctx).expect("a save with no active project must not fail");

        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        let minted = dir.join("Untitled.rud");
        assert!(
            minted.is_file(),
            "the work must land on disk under a minted name. Refusing here, or \
             popping a name-me modal, is the moment a beginner cancels and \
             loses the edit — which is the whole failure this phase fixes."
        );
        let persisted = project_store::load_project(&minted).expect("load the minted file");
        assert_eq!(persisted.fps, 24.0, "and it is the EDITED state that landed");
        assert_eq!(persisted.name, "Untitled", "named to match its file");
        assert_eq!(
            ctx.store().lock().expect("store").snapshot().name,
            "Untitled",
            "and the LIVE document was renamed too, or the next save writes a \
             file whose embedded name disagrees with its own filename"
        );
        assert_eq!(
            active_path(&ctx),
            Some(minted),
            "the minted file is now the active document, so the NEXT save — \
             and the autosave on close — go to it rather than minting again"
        );
    }

    #[test]
    fn the_minted_name_never_collides_with_an_existing_file() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        let dir = project_store::projects_dir(&ctx).expect("projects dir");

        // Somebody else's Untitled is already there.
        let squatter = dir.join("Untitled.rud");
        let mut existing = Project::new();
        existing.name = "Untitled".to_string();
        existing.width = 640;
        project_store::write_project_atomic(&squatter, &existing).expect("write the squatter");
        let squatter_bytes = std::fs::read(&squatter).expect("read the squatter");

        set_fps(&ctx, 24.0);
        run_save_project(&ctx).expect("save with no active project");

        assert!(
            dir.join("Untitled 2.rud").is_file(),
            "the collision search must step to `Untitled 2`"
        );
        assert_eq!(
            std::fs::read(&squatter).expect("re-read the squatter"),
            squatter_bytes,
            "and must NOT have overwritten the project that was already there — \
             minting a name is not licence to clobber one"
        );
        assert!(
            project_store::sanitize_project_name("Untitled 2").is_ok(),
            "T-60.1-07: every minted name is a real path component, so it must \
             satisfy the one owner of that rule"
        );
    }

    #[test]
    fn run_save_project_returns_the_persisted_path_and_seq() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        run_new_project(&ctx, &serde_json::json!({ "name": "Enveloped" }))
            .expect("create Enveloped");
        set_fps(&ctx, 24.0);
        set_fps(&ctx, 25.0);

        let v = envelope(&run_save_project(&ctx).expect("save"));

        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        assert_eq!(
            v.get("path").and_then(|p| p.as_str()).map(std::path::PathBuf::from),
            Some(dir.join("Enveloped.rud")),
            "the envelope names the file the bytes actually went to"
        );
        assert_eq!(
            v.get("seq").and_then(|s| s.as_u64()),
            Some(ctx.store().lock().expect("store").seq()),
            "and the seq it persisted — the honest dot a host compares against \
             the live store to know whether anything is unsaved"
        );
    }

    #[test]
    fn run_save_project_as_repoints_the_active_document() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        run_new_project(&ctx, &serde_json::json!({ "name": "Original" }))
            .expect("create Original");
        set_fps(&ctx, 24.0);
        run_save_project(&ctx).expect("save the original once");

        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        let original = dir.join("Original.rud");
        let original_bytes = std::fs::read(&original).expect("read Original.rud");

        let elsewhere = outside_dir("save-as");
        let copy = elsewhere.join("Renamed Copy.rud");
        let v = envelope(&run_save_project_as(&ctx, save_target(&copy)).expect("save as"));

        // 1. The bytes went to the chosen path, carrying the CURRENT state.
        let persisted = project_store::load_project(&copy).expect("the copy is on disk");
        assert_eq!(persisted.fps, 24.0, "the copy carries the live state");
        assert_eq!(
            persisted.name, "Renamed Copy",
            "and the document was renamed to the file stem the user chose"
        );
        assert_eq!(
            ctx.store().lock().expect("store").snapshot().name,
            "Renamed Copy",
            "the LIVE document too, or the TitleBar keeps naming the original"
        );
        assert_eq!(
            v.get("name").and_then(|n| n.as_str()),
            Some("Renamed Copy"),
            "and the envelope reports it"
        );

        // 2. The active document MOVED. This is the whole point.
        let canonical = std::fs::canonicalize(&copy).expect("canonicalize");
        assert_eq!(
            active_path(&ctx),
            Some(canonical),
            "after Save As you are editing the COPY — the universal NLE \
             convention, and the owner's decision for Rudis"
        );

        // 3. Proven where it counts: the NEXT save lands on the new path.
        set_fps(&ctx, 60.0);
        run_save_project(&ctx).expect("the following save");
        assert_eq!(
            project_store::load_project(&copy).expect("re-load the copy").fps,
            60.0,
            "a Save As that did not re-point the active document would send \
             this edit back to the original and lose it from the file the user \
             thinks they are editing"
        );
        assert_eq!(
            std::fs::read(&original).expect("re-read Original.rud"),
            original_bytes,
            "and the ORIGINAL is byte-unchanged — Save As branches the \
             document, it does not keep writing to both"
        );
    }

    /// Rule 2 (missing critical functionality), pinned in BOTH directions.
    ///
    /// A save that renames the live document must tell the shell, or the
    /// TitleBar keeps naming a document that no longer exists — the exact
    /// MULTIPROJECT-UI defect `run_new_project`'s own emission was added to
    /// fix. A save that renames NOTHING must stay silent, because
    /// `ProjectSwitched` is a structural kind and `ShellMirror.ApplyAsync`
    /// routes structural kinds to a FULL RESYNC: emitting on every `Ctrl+S`
    /// would make the ordinary save the most expensive thing in the app.
    #[test]
    fn only_a_save_that_renames_the_document_emits_project_switched() {
        let _lease = switch_lease();

        // (a) an ordinary save over an active project — silent.
        let ctx = TestAppCtx::new();
        run_new_project(&ctx, &serde_json::json!({ "name": "Quiet" })).expect("create Quiet");
        let before = ctx.emitted_patches().len();
        set_fps(&ctx, 24.0);
        run_save_project(&ctx).expect("save");
        assert_eq!(
            ctx.emitted_patches().len(),
            before,
            "Ctrl+S must not force a full mirror resync"
        );

        // (b) a save that MINTS — one ProjectSwitched, because the live
        //     document was just renamed from nothing to `Untitled`.
        let minting = TestAppCtx::new();
        set_fps(&minting, 24.0);
        run_save_project(&minting).expect("mint");
        assert_eq!(
            minting
                .emitted_patches()
                .into_iter()
                .map(|(p, _, _)| p.kind)
                .collect::<Vec<_>>(),
            vec![PatchKind::ProjectSwitched],
            "the minted rename must reach the shell"
        );

        // (c) Save As — one ProjectSwitched, carrying the store's ACTUAL seq
        //     pair rather than (0, 0): nothing reset the store here.
        let as_ctx = TestAppCtx::new();
        run_new_project(&as_ctx, &serde_json::json!({ "name": "Branching" }))
            .expect("create Branching");
        set_fps(&as_ctx, 24.0);
        let before_as = as_ctx.emitted_patches().len();
        let elsewhere = outside_dir("save-as-emit");
        let copy = elsewhere.join("Branch.rud");
        run_save_project_as(&as_ctx, save_target(&copy)).expect("save as");
        let new_patches: Vec<(PatchKind, u64, u64)> = as_ctx
            .emitted_patches()
            .into_iter()
            .skip(before_as)
            .map(|(p, base, seq)| (p.kind, base, seq))
            .collect();
        let live_seq = as_ctx.store().lock().expect("store").seq();
        assert_eq!(
            new_patches,
            vec![(PatchKind::ProjectSwitched, live_seq, live_seq)],
            "Save As emits the store's REAL (seq, seq) pair — (0, 0) would be a \
             lie here, because unlike a project switch nothing called \
             from_project and nothing reset the counter the renderer tracks"
        );
    }

    /// The undo history is a document's memory of what the user did. Saving a
    /// copy of it must not erase it — which is why neither save function may
    /// reach for `Store::from_project`.
    #[test]
    fn save_as_keeps_the_undo_history() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        run_new_project(&ctx, &serde_json::json!({ "name": "Undoable" }))
            .expect("create Undoable");
        set_fps(&ctx, 24.0);
        assert!(
            ctx.store().lock().expect("store").can_undo(),
            "fixture: there is history to lose"
        );

        let elsewhere = outside_dir("undo");
        let kept = elsewhere.join("Kept.rud");
        run_save_project_as(&ctx, save_target(&kept)).expect("save as");

        // Asserted first so this test can go RED at all: a no-op stub keeps the
        // history trivially, and a guard that cannot fail is not a guard.
        assert!(kept.is_file(), "fixture: the Save As actually wrote something");
        assert!(
            ctx.store().lock().expect("store").undo().is_some(),
            "losing your undo history because you saved is a bug, not a feature"
        );
    }

    // -----------------------------------------------------------------------
    // The two host-facing reads
    // -----------------------------------------------------------------------

    fn media(id: &str, path: &std::path::Path) -> rudis_core::MediaBinItem {
        rudis_core::MediaBinItem {
            id: id.to_string(),
            path: path.to_string_lossy().into_owned(),
            media_kind: rudis_core::MediaKind::Video,
            duration_us: 1_000_000,
            // Deliberately small: `proxy::heaviness::needs_proxy` fails its
            // resolution rung here, so opening these fixtures never spawns a
            // background encode and this module never needs a Tokio runtime.
            width: 640,
            height: 480,
            fps: 30.0,
            is_vfr: false,
            rotation_degrees: 0,
            has_audio: false,
            poster_path: None,
            folder: String::new(),
            display_name: None,
            is_image_sequence: false,
            reports_alpha: None,
        }
    }

    #[test]
    fn run_get_projects_detailed_carries_the_path_and_the_mtime() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        run_new_project(&ctx, &serde_json::json!({ "name": "Alpha" })).expect("create Alpha");
        run_new_project(&ctx, &serde_json::json!({ "name": "Beta" })).expect("create Beta");
        run_open_project(&ctx, &serde_json::json!({ "name": "Alpha" })).expect("open Alpha");

        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&run_get_projects_detailed(&ctx).expect("the detailed list"))
                .expect("it is a JSON array");

        assert_eq!(rows.len(), 2, "every .rud really on disk gets a row");
        let dir = project_store::projects_dir(&ctx).expect("projects dir");
        for row in &rows {
            let name = row["name"].as_str().expect("name");
            assert_eq!(
                row["path"].as_str().map(std::path::PathBuf::from),
                Some(dir.join(format!("{name}.rud"))),
                "the PATH is the whole reason this sibling exists: two projects \
                 both called \"Untitled\" are indistinguishable without it"
            );
            assert!(
                row["modifiedUnixMs"].as_u64().is_some_and(|ms| ms > 0),
                "and the mtime, so the list can sort by most-recent"
            );
        }
        let active: Vec<&str> = rows
            .iter()
            .filter(|r| r["isActive"].as_bool() == Some(true))
            .map(|r| r["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            active,
            vec!["Alpha"],
            "exactly one row is the active document, and it is the one opened"
        );
    }

    /// **The agent's view is pinned, byte for byte.**
    ///
    /// T-26-08 withholds the path from the LLM on purpose. This asserts on the
    /// serialized STRING rather than on parsed fields precisely so that a later,
    /// well-meant "let's just add the path here too" fails a test instead of
    /// landing — which is the failure mode a parsed-field assertion would sail
    /// straight past.
    #[test]
    fn run_get_projects_stays_byte_identical_for_the_agent() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        run_new_project(&ctx, &serde_json::json!({ "name": "Alpha" })).expect("create Alpha");
        run_new_project(&ctx, &serde_json::json!({ "name": "Beta" })).expect("create Beta");
        run_open_project(&ctx, &serde_json::json!({ "name": "Alpha" })).expect("open Alpha");

        assert_eq!(
            run_get_projects(&ctx).expect("the agent list"),
            "[{\"isActive\":true,\"name\":\"Alpha\"},{\"isActive\":false,\"name\":\"Beta\"}]",
            "the agent-facing list is {{name, isActive}} and nothing else — no \
             path, no timestamp. Adding the detailed sibling must not widen the \
             LLM's view by one byte."
        );
    }

    /// **T-60.1-06.** `scan_known_projects` reads `Project.name` out of each
    /// file, and anything with write access to the projects directory can put
    /// an arbitrary string there. XAML `TextBlock.Text` is inert so this is not
    /// an injection risk — it is a layout DoS, and the control is a bound.
    #[test]
    fn run_get_projects_detailed_caps_a_hostile_project_name() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        let dir = project_store::projects_dir(&ctx).expect("projects dir");

        // A hand-written .rud: `sanitize_project_name` never ran on this,
        // because nothing in the app created it.
        let mut ascii = Project::new();
        ascii.name = "a".repeat(4_000);
        project_store::write_project_atomic(&dir.join("hostile-ascii.rud"), &ascii)
            .expect("write the hostile file");

        // And a multi-byte one, so the cap cannot be implemented by slicing
        // bytes blindly — that would panic on a char boundary.
        let mut cjk = Project::new();
        cjk.name = "\u{6f22}".repeat(1_000); // 3 bytes each
        project_store::write_project_atomic(&dir.join("hostile-cjk.rud"), &cjk)
            .expect("write the hostile file");

        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&run_get_projects_detailed(&ctx).expect("the detailed list"))
                .expect("it is a JSON array");

        assert_eq!(rows.len(), 2, "both hostile files are still LISTED");
        for row in &rows {
            let name = row["name"].as_str().expect("name");
            assert!(
                name.len() <= 255,
                "an unbounded string in a list row is a layout DoS; got {} bytes",
                name.len()
            );
            assert!(!name.is_empty(), "and the row is capped, never dropped");
        }
    }

    #[test]
    fn run_get_missing_media_with_no_active_project_is_empty_not_an_error() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();
        assert_eq!(active_path(&ctx), None, "fixture: nothing is active");
        assert_eq!(
            run_get_missing_media(&ctx).expect("an absent project is not an error"),
            "[]",
            "an absent project has no missing media, and that is not a failure \
             the host should have to branch on"
        );
    }

    #[test]
    fn run_get_missing_media_names_exactly_the_items_whose_files_moved() {
        let _lease = switch_lease();
        let ctx = TestAppCtx::new();

        let elsewhere = outside_dir("relink");
        let present = elsewhere.join("still-here.mp4");
        std::fs::write(&present, b"not really a video, but it IS a file")
            .expect("write the present media");
        let moved = elsewhere.join("someone-moved-me.mp4");

        let mut project = Project::new();
        project.name = "Relink".to_string();
        project.media_bin.push(media("m-present", &present));
        project.media_bin.push(media("m-moved", &moved));
        let rud = elsewhere.join("Relink.rud");
        project_store::write_project_atomic(&rud, &project).expect("write the fixture");

        run_open_project_at_path(&ctx, open_path(&rud)).expect("open it");

        assert_eq!(
            run_get_missing_media(&ctx).expect("poll the relink state"),
            "[\"m-moved\"]",
            "ids, not paths (T-26-08) — and EXACTLY the ones whose backing file \
             is gone. The project opened regardless, which is what every \
             surveyed NLE does and what keeps a broken link a fixable state \
             rather than a locked door."
        );
    }

    // -----------------------------------------------------------------------
    // Quick 260828-h0u (WARM-01) — the open path warms while paused
    // -----------------------------------------------------------------------

    /// **The field defect, as an assertion.**
    ///
    /// Measured on the owner's `F6M Six Minimized 4K` (6 layers, 4 sources at
    /// 4K), Release build, 2026-08-28: opened, left PAUSED and untouched for
    /// 8+ minutes, the render cache committed **zero** segments; a single 14 s
    /// Play followed by Pause committed **all four** within ~60 s of paused
    /// wall. Two gates produced that, and this test covers both: the pump was
    /// only ever born inside the transport funnel, and BOTH detector inlets
    /// (`note_live_tick`, `prearm_stack`) were reachable only from the live
    /// producer tick while [`swap_to_loaded_project`] fires `forget_all_heat()`
    /// on every open.
    ///
    /// This opens a 4-layer project through the PRODUCTION funnel and asserts
    /// the heat exists with **zero transport commands and zero
    /// `note_live_tick` calls** — no producer thread exists in it at all, by
    /// construction. An empty `heavy_segments` here IS the 8 paused minutes.
    #[test]
    fn open_project_prearms_heavy_segments_with_zero_transport_commands() {
        // The process-global HEAT registry is shared with `render_cache_job`'s
        // and `proxy_job`'s tests, whose leases bottom out on this same mutex.
        let _lease = switch_lease();
        preview::render_cache_detect::reset_for_tests();
        let ctx = TestAppCtx::new();

        // Project "heavy": 4 real-probed video layers covering [0, 5 s).
        // 5_000_000 us spans segments 0 [0, 2 s), 1 [2 s, 4 s) and 2
        // [4 s, 6 s) — a PARTIAL third segment, deliberately, so the sweep's
        // end-boundary arithmetic is on trial rather than assumed.
        run_new_project(&ctx, &serde_json::json!({ "name": "heavy" })).expect("create heavy");
        let sources = [
            crate::test_support::fixture("solid_red_720p30_20s.mp4"),
            crate::test_support::fixture("bars_720p30_75s.mp4"),
            crate::test_support::fixture("solid_red_720p30_20s.mp4"),
            crate::test_support::fixture("bars_720p30_75s.mp4"),
        ];
        {
            let mut store = ctx.store().lock().expect("store");
            crate::test_support::seed_layered_video_arrangement(&mut store, &sources, 5_000_000);
        } // the guard is dropped here, before anything else runs

        // The documented residual, pinned as a measurement rather than left as
        // prose: EDITS do not pre-arm. Only project open and live producer
        // ticks feed the detector.
        assert!(
            preview::render_cache_detect::heavy_segments(64).is_empty(),
            "structural edits do not feed prearm_stack — that inlet gap is \
             quick 260828-h0u's documented residual, deliberately out of its \
             scope; if this fires, a later phase closed it and this assertion \
             should be updated to match"
        );

        // Switch away (hook (a) autosaves "heavy" to disk on the way out), then
        // open it back through THE production open path. Zero transport
        // commands anywhere in this test.
        run_new_project(&ctx, &serde_json::json!({ "name": "other" })).expect("create other");
        run_open_project(&ctx, &serde_json::json!({ "name": "heavy" })).expect("open heavy");

        // THE GATE. 4 layers > PREARM_LAYER_THRESHOLD (3) across [0, 5 s), so
        // the sweep must arm exactly segments 0, 1 and 2 — pre-armed rows ARE
        // candidates (`is_candidate = heavy || prearmed`), so `heavy_segments`
        // sees them.
        assert_eq!(
            preview::render_cache_detect::heavy_segments(64),
            vec![0, 1, 2],
            "a freshly-opened 4-layer project must be structurally pre-armed \
             with ZERO transport commands and ZERO live ticks — this Vec being \
             empty IS the field stall (0 segments after 8+ paused minutes)"
        );

        // A-then-B isolation: opening the EMPTY project must leave nothing
        // armed. `forget_all_heat()` runs first and an empty timeline sweeps to
        // nothing, so heavy A's heat can never be carried under B — where the
        // same segment indices mean a completely different arrangement.
        run_open_project(&ctx, &serde_json::json!({ "name": "other" })).expect("open other");
        assert!(
            preview::render_cache_detect::heavy_segments(64).is_empty(),
            "heavy A's heat must not survive under B — the open-path sweep \
             arms the INCOMING arrangement only, after the shipped \
             forget_all_heat"
        );
    }

    /// **T-h0u-01: a hostile `.rud` may not freeze project-open.**
    ///
    /// The threat register's "runaway sweep from a hostile/huge `.rud`" row,
    /// as a measurement. A `.rud` is a document on disk and nothing rejects a
    /// clip whose `out_us` is `i64::MAX`; that single clip spans ~4.6e12
    /// segments, so a sweep that emitted one pair per segment would never
    /// return and the app would hang on open with no error and no way out.
    ///
    /// The bound is the heat registry's OWN capacity, which is why it costs
    /// nothing real: a pair past `MAX_TRACKED_SEGMENTS` would evict one of its
    /// own predecessors the moment it was applied.
    ///
    /// This test does not time out on failure — it hangs — and that is the
    /// honest shape for "unbounded loop", since a wall-clock assertion would
    /// only be measuring the machine.
    #[test]
    fn prearm_pairs_is_bounded_by_the_heat_registrys_own_capacity() {
        let absurd = rudis_core::Timeline {
            tracks: vec![rudis_core::Track {
                kind: rudis_core::TrackKind::Video,
                clips: vec![rudis_core::Clip {
                    id: "c-absurd".to_string(),
                    media_id: "m-absurd".to_string(),
                    start_us: 0,
                    in_us: 0,
                    out_us: i64::MAX,
                    volume: 1.0,
                    audio_detached: false,
                    transform: rudis_core::ClipTransform::default(),
                    opacity: 1.0,
                    crop: rudis_core::ClipCrop::default(),
                    keyframes: Default::default(),
                    text: None,
                    alpha_mode: Default::default(),
                    retime: None,
                }],
            }],
        };

        let pairs = prearm_pairs(&absurd);

        assert_eq!(
            pairs.len(),
            preview::render_cache_detect::MAX_TRACKED_SEGMENTS,
            "the sweep must stop at the registry's own capacity — reaching              this line at all is the assertion, because an unbounded sweep              never returns"
        );
        assert_eq!(
            pairs.first().map(|(seg, _)| *seg),
            Some(0),
            "and it must keep the LOWEST segments: the playhead opens at 0, so              those are the ones the user reaches first"
        );
    }
}
