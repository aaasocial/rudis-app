//! The central mutation plumbing: dispatch, undo (with the library-growth
//! retraction) and redo — relocated from `src-tauri/src/lib.rs` by plan 45-12
//! (Phase 45, XTRC-01 / XTRC-03).
//!
//! # Why the command wrappers themselves stay in the HOST
//!
//! `dispatch_command`, `undo` and `redo` are IPC surface. A host's command
//! layer resolves each parameter from its own transport and managed state and
//! cannot resolve a `&impl AppCtx` — so those three functions can never fully
//! relocate. Only their LOGIC can, which is exactly the split `run_export` /
//! `run_export_blocking` established in 45-10: the host keeps a thin wrapper
//! that builds its `AppCtx` and calls straight through to one of the three
//! `*_inner` functions here. Today that wrapper is a C export in
//! `crates/ffi/src/commands.rs` building an `FfiAppCtx`; when this was written
//! it was a `#[tauri::command]` building a `TauriAppCtx`.
//!
//! # What changed, and what did not
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `lock(&store)?` | `ctx.store().lock().map_err(..)?` — the SAME `"backend store mutex poisoned"` string that helper baked in |
//! | `emit_changed(&app, &patch, base_seq, seq)?` | `ctx.emit_patch(&patch, base_seq, seq)?` — whose Tauri impl delegates to the unchanged `emit_changed`, so the `#[serde(flatten)]`ed LAT-02 envelope is byte-identical |
//! | `session: &Mutex<AgentSession>` (a parameter) | [`AppCtx::agent_session`] (added by this plan) |
//! | `redo`'s body, inline in the command | [`redo_inner`] — a NEW name for the SAME statements, added purely for symmetry with its two siblings |
//!
//! Nothing else. The store-then-session lock ORDER, the `take()`-before-compare
//! retraction, the swallowed `remove_file` failure, the "emit only when
//! something actually changed" condition and all three return shapes are
//! carried verbatim.
//!
//! # The lock order is load-bearing (T-45-03, carried)
//!
//! [`undo_inner`] locks the store, pops, and lets that guard drop as a temporary
//! at the end of the statement BEFORE it ever touches the session. Strictly
//! sequential, never nested — which is what keeps it deadlock-free against
//! `run_agent_turn`'s session → store spans. Moving the `ctx.agent_session()`
//! lock inside the store guard's scope would be a deadlock, not a refactor.

use crate::{AppCtx, Command, Patch};

/// Apply one typed [`Command`] to the backend store (validate → apply → push
/// inverse). Returns the [`Patch`] and emits it as `project:changed`.
///
/// The `#[tauri::command] fn dispatch_command` in `src-tauri` is now exactly
/// this call plus a ctx construction.
pub fn dispatch_command_inner<C: AppCtx>(ctx: &C, cmd: Command) -> Result<Patch, String> {
    // Phase 58 (PROXY-02, 58-CONTEXT D-11): removing a media item cancels any
    // proxy generation still running for its file. Placed HERE — one peek at the
    // ONE dispatch funnel every host and every agent tool passes through —
    // rather than inside `Store::dispatch`, because the domain core owns no
    // background jobs and must not learn about them.
    //
    // BEFORE the dispatch, because after it the item is gone from the store and
    // its path is unrecoverable. Strictly best-effort: an unknown id, a poisoned
    // store or a path with no job in flight all skip silently and NEVER fail the
    // command. The guard is scoped to the lookup and released before the latch
    // is raised, so this cannot extend the lifetime of any store borrow across
    // the dispatch below.
    if let Command::RemoveMediaBinItem { id } = &cmd {
        let path = ctx
            .store()
            .lock()
            .ok()
            .and_then(|store| store.media_item(id).map(|item| item.path.clone()));
        if let Some(path) = path {
            crate::proxy_job::request_cancel_for_path(std::path::Path::new(&path));
        }
        // Phase 59 (59-CONTEXT D-24 names media removal in its caller list):
        // every in-flight segment render, not just this media's. A segment is a
        // composite of whatever was visible over a range, so the cache has no
        // per-media handle to raise — and a render whose media is being removed
        // is about to fail its own identity check anyway. Cancelling is the
        // cheap, correct answer, and it hands the shared encoder admission back
        // to the proxy worker immediately.
        crate::render_cache_job::request_cancel_all();
    }

    // Phase 43 (LAT-02): the RETURN type is deliberately unchanged — every
    // agent tool call relies on the direct synchronous `Patch`. Only the
    // emitted EVENT gains the seq envelope.
    //
    // A poisoned mutex means a prior handler panicked mid-mutation; surface a
    // recoverable error to the renderer instead of unwinding again (the
    // verbatim contract of `src-tauri`'s `lock()` helper, whose error string
    // this reproduces).
    let (patch, base_seq, seq) = ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .dispatch(cmd)
        .map_err(|e| e.to_string())?;
    ctx.emit_patch(&patch, base_seq, seq)?;
    note_edit_for_render_cache(ctx, patch.kind);
    Ok(patch)
}

/// Phase 59 D-38's HYGIENE sweep, triggered where an edit is already known
/// (59-REVIEW **WR-05**).
///
/// D-38 shipped `on_structural_edit` fully implemented with no shipped caller,
/// so the orphaned bytes it documents were never actually reclaimed — only
/// D-25's LRU reclaimed them, at the 8 GiB budget. This is the wiring, placed
/// here for the reason the media-removal peek above is here: this is the ONE
/// funnel every host and every agent tool passes through, and it is the only
/// altitude that knows both that an edit happened and where the playhead is.
///
/// `preview::patch_touches_preview` is the filter, reused rather than
/// reinvented: it already answers "can this patch change what the preview
/// shows", it defaults to `true` for kinds it has not been taught, and it
/// excludes exactly the media-bin and annotation kinds that cannot move a
/// segment's pixels. An import or an ink stroke therefore pays nothing.
///
/// Everything past the filter is best-effort, throttled and infallible — see
/// `render_cache_job::on_structural_edit_at_playhead`. It cannot fail the edit
/// that triggered it, and D-18 refuses a stale segment reactively whether this
/// runs or not.
fn note_edit_for_render_cache<C: AppCtx>(ctx: &C, kind: rudis_core::PatchKind) {
    if preview::patch_touches_preview(kind) {
        crate::render_cache_job::on_structural_edit_at_playhead(ctx);
    }
}

/// Undo the most recent command. `None` when the undo stack is empty (not an
/// error). Emits `project:changed` when state actually changed.
///
/// The command-level helper BOTH the real `undo` command and the deterministic
/// `mod library_growth_gate` tests drive (the exact `apply_option_card_inner`
/// pattern).
///
/// After popping a patch, the ONE pending library-growth record (if any) is
/// resolved (Phase 14, MOAT-03, DECISIONS.md A1): when the popped `Patch.ids`
/// OVERLAP the tracked ids, the just-appended file is deleted (retract — the
/// interaction was NOT accepted); when they don't, the tracking is simply
/// cleared (finalize — only the immediately-next undo gets a retraction
/// chance). A failed deletion logs and never breaks the undo itself. When
/// nothing was popped, neither the file nor the tracking is touched.
///
/// Lock order is store → session, strictly SEQUENTIAL (the store guard is a
/// dropped temporary before the session lock) — no nesting, no deadlock
/// against `run_agent_turn`'s session → store spans.
pub fn undo_inner<C: AppCtx>(ctx: &C) -> Result<Option<Patch>, String> {
    let undone = ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .undo();
    if let Some((p, base_seq, seq)) = &undone {
        {
            let mut sess = ctx
                .agent_session()
                .lock()
                .map_err(|_| "agent session poisoned".to_string())?;
            if let Some(growth) = sess.pending_growth.take() {
                if p.ids.iter().any(|id| growth.ids.contains(id)) {
                    // The VERY NEXT undo reverted the appended turn itself:
                    // retract the just-written library file.
                    if let Err(e) = std::fs::remove_file(&growth.path) {
                        eprintln!("library-growth retraction failed (undo unaffected): {e}");
                    }
                }
                // Non-overlap: finalize — `take()` already cleared the
                // tracking, the file stays on disk.
            }
        }
        ctx.emit_patch(p, *base_seq, *seq)?;
        // An undo is an edit: it moves `Store::seq` and it re-arranges the
        // timeline, so the segments around the playhead are as orphaned by it as
        // by the command it reverts (59-REVIEW WR-05).
        note_edit_for_render_cache(ctx, p.kind);
    }
    // The COMMAND's return type is deliberately unchanged (`Option<Patch>`) —
    // only the emitted event gained the seq envelope (Phase 43, LAT-02).
    Ok(undone.map(|(p, _, _)| p))
}

/// Redo the most recently undone command. `None` when the redo stack is empty.
/// Emits `project:changed` when state actually changed.
///
/// This function is NEW as a name only: before 45-12 these four statements sat
/// inline in the `#[tauri::command] fn redo`, which (unlike `undo`) had no
/// `_inner` split. Extracting it is a mechanical symmetry addition — identical
/// logic, now relocated and callable from a non-Tauri host.
pub fn redo_inner<C: AppCtx>(ctx: &C) -> Result<Option<Patch>, String> {
    let redone = ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .redo();
    if let Some((p, base_seq, seq)) = &redone {
        ctx.emit_patch(p, *base_seq, *seq)?;
        // …and so is a redo, for `undo_inner`'s reason one direction over.
        note_edit_for_render_cache(ctx, p.kind);
    }
    Ok(redone.map(|(p, _, _)| p))
}
