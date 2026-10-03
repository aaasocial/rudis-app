//! The four store-query / instrumentation one-liners — relocated from
//! `src-tauri/src/lib.rs` by plan 47-01 (Phase 47, FFI-01; RESEARCH A1 rows
//! 1-4).
//!
//! # Why these four move now
//!
//! Phase 47's C ABI (`crates/ffi`) must be a pure transport layer: every
//! exported command wraps an `app-core` `run_*`, never a re-implementation.
//! RESEARCH A1 found these four commands still had their logic inline in the
//! Tauri command bodies — one-liners, but one-liners the C# shell cannot reach
//! without duplicating them. Two of them (`run_get_entities`,
//! `run_get_current_seq`) are exactly what decision D-01 exists to preserve:
//! Phase 43's LAT-02 apply-instead-of-refetch path, which the shell would
//! otherwise have to re-create by refetching whole snapshots.
//!
//! # What changed, and what did not
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `lock(&store)?` | `ctx.store().lock().map_err(..)?` — the SAME poisoned-mutex string that helper baked in (the `dispatch_command_inner` idiom; byte-identical, per 45-07's `app_cache_dir` guessed-string lesson) |
//!
//! Nothing else. The three store queries are byte-copies of the command bodies
//! with only that named substitution; `run_debug_mark_interactive` is a
//! verbatim copy (it touches no store and takes no ctx). The
//! `#[tauri::command]`s stay in `src-tauri` as thin wrappers, exactly like
//! `dispatch_command`'s split in 45-12 — the macro cannot resolve a
//! `&impl AppCtx` parameter.

use crate::AppCtx;

/// Full snapshot of the backend-owned project. The renderer (re)builds its
/// read-only mirror from this — on load and whenever it chooses to rehydrate.
///
/// The `#[tauri::command] fn get_snapshot` in `src-tauri` is now exactly this
/// call plus a ctx construction (plan 47-01; origin: that command's inline
/// `lock(&store)?.snapshot()`).
pub fn run_get_snapshot<C: AppCtx>(ctx: &C) -> Result<rudis_core::Project, String> {
    Ok(ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .snapshot())
}

/// Phase 43 (LAT-02, D-08): the bulk-mutation / unrecognized-kind fallback —
/// the renderer calls this instead of `get_snapshot` whenever a `Patch`'s
/// `entities` is `None`. Returns ONLY the named entities (request order);
/// unknown ids are silently omitted rather than erroring, so a stale id in
/// the renderer's list degrades to "resolve the rest" instead of failing the
/// whole mirror update.
///
/// The `#[tauri::command] fn get_entities` in `src-tauri` is now exactly this
/// call plus a ctx construction (plan 47-01; origin: that command's inline
/// `lock(&store)?.get_entities(&ids)`).
pub fn run_get_entities<C: AppCtx>(
    ctx: &C,
    ids: &[String],
) -> Result<Vec<rudis_core::EntitySnapshot>, String> {
    Ok(ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .get_entities(ids))
}

/// Phase 43 (LAT-02): lets the renderer learn the CURRENT seq once, at
/// mirror-rehydration time (`get_snapshot` itself carries no seq — this is
/// the minimal companion call, matching the LAT-01 marker's "one extra cheap
/// IPC call at startup" precedent).
///
/// The `#[tauri::command] fn get_current_seq` in `src-tauri` is now exactly
/// this call plus a ctx construction (plan 47-01; origin: that command's
/// inline `lock(&store)?.seq()`).
pub fn run_get_current_seq<C: AppCtx>(ctx: &C) -> Result<u64, String> {
    Ok(ctx
        .store()
        .lock()
        .map_err(|_| "backend store mutex poisoned".to_string())?
        .seq())
}

/// Phase 43 (LAT-01, D-04): writes an empty marker file the instant this is
/// invoked, IF AND ONLY IF `RUDIS_LAT01_MARKER_PATH` is set in the process
/// environment. A normal end-user launch never sets this env var, so this
/// is a zero-cost no-op in production. It exists purely so the LAT-01
/// baseline harness can measure wall-clock from process spawn to
/// "first interactive" — defined here as: the frontend's FIRST
/// `get_snapshot` mirror hydration has resolved. Never panics: a write
/// failure is logged to stderr and swallowed, since a broken marker must
/// never crash the app.
///
/// Body moved VERBATIM from `src-tauri`'s `#[tauri::command] fn
/// debug_mark_interactive` (plan 47-01) — it touches no store and takes no
/// ctx, so it is the one function in this module with no `AppCtx` bound.
/// `src-tauri/tests/lat01_baseline.rs` keys its cold-launch measurement on
/// exactly this marker write.
pub fn run_debug_mark_interactive() {
    if let Ok(path) = std::env::var("RUDIS_LAT01_MARKER_PATH") {
        if let Err(e) = std::fs::write(&path, b"interactive") {
            eprintln!("LAT-01 cold-launch marker: failed to write {path}: {e}");
        }
    }
}
