//! The `send_feedback` agent tool (Phase 17-04, D-08 / TOOL-05) — relocated here
//! from `src-tauri/src/lib.rs` by plan 45-10.
//!
//! # What moved, and the closure that says exactly this set
//!
//! `handle_send_feedback` + its exclusively-owned private `write_feedback_file`,
//! and nothing else. The both-directions grep, measured before the cut:
//!
//! * **Outward** — `handle_send_feedback` is reached only from `run_agent_turn`'s
//!   `"send_feedback"` dispatch arm; `write_feedback_file` only from that handler.
//!   Zero hits anywhere else in `src-tauri`, `crates/` or `frontend/` (the two
//!   hits in [`crate::session`]'s module doc are prose naming this very batch).
//! * **Inward** — everything it names was already resident:
//!   [`crate::AgentSession`] and its `recent_tool_calls` / `last_error` fields
//!   (45-04), `agent_llm::ContentBlock` / `agent_llm::vision::text_tool_result`
//!   (a crate dependency since 45-04). **No new [`crate::AppCtx`] method was
//!   needed** — the region was grepped for `app.path().` in full and the single
//!   hit is `app_data_dir()`, which the trait has had since 45-06, and whose
//!   baked-in message (`"resolve app data dir: {e}"`) is EXACTLY the string this
//!   call site built inline, so the surfaced error text is byte-identical.
//!
//! # The path-confinement guard moved VERBATIM (threat T-17-10 / T-45-06)
//!
//! `write_feedback_file`'s filename is built ENTIRELY server-side from a
//! millisecond timestamp; no component of the LLM `input` ever reaches the path.
//! The defensive `filename.contains('/') || .. ` check is a real `return Err`
//! that also holds in `--release` (LO-01), NOT a `debug_assert!` — it was moved
//! byte-for-byte rather than reimplemented, so it is still active in the shipped
//! profile. It is pinned by `src-tauri`'s
//! `agent_gate::fixture_scripted_send_feedback_writes_path_confined_local_diagnostic`,
//! which drives the whole thing through the REAL `run_agent_turn` and asserts the
//! written file canonicalizes inside `app_data_dir/feedback`.
//!
//! # Why NO test moved with it
//!
//! Both of this pair's tests (`fixture_scripted_send_feedback_writes_path_confined_local_diagnostic`
//! and `a_failed_export_projects_error_is_captured_into_last_error_for_send_feedback`)
//! live in `src-tauri`'s `agent_gate` and drive `run_agent_turn` with a scripted
//! `FixtureTransport`. `run_agent_turn` has not moved, so 45-05's rule keeps them
//! — they reach this code through the re-export shim, unchanged.
//!
//! # The conversion — nothing else in these bodies changed
//!
//! | Before (`src-tauri`) | After (here) |
//! |---|---|
//! | `fn f<R: tauri::Runtime>(app: &AppHandle<R>, ..)` | `fn f<C: AppCtx>(ctx: &C, ..)` |
//! | `app.path().app_data_dir().map_err(..)` | `ctx.app_data_dir()?` |
//!
//! The `session: &Mutex<AgentSession>` parameter is untouched: `AgentSession` is
//! an intra-crate type here, so it needs no shim and no trait method.

use std::sync::Mutex;

use crate::{AgentSession, AppCtx};

/// Phase 17-04 (D-08, TOOL-05): the `send_feedback` interception — write a
/// LOCAL diagnostic JSON report (offline-core: never a network call) and mint
/// its tool_result. Never panics: any failure becomes an `is_error` tool_result
/// so a broken diagnostic write never aborts the agent turn.
pub fn handle_send_feedback<C: AppCtx>(
    ctx: &C,
    session: &Mutex<AgentSession>,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content: String, is_error: Option<bool>| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: agent_llm::vision::text_tool_result(content),
        is_error,
    };
    match write_feedback_file(ctx, session, input) {
        Ok(_path) => result("Feedback saved locally.".to_string(), None),
        Err(e) => result(format!("Failed to save feedback: {e}"), Some(true)),
    }
}

/// Write the `send_feedback` diagnostic report to a PATH-CONFINED local file and
/// return its path. The directory is a FIXED `app_data_dir/feedback` and the
/// filename is BUILT ENTIRELY SERVER-SIDE from a millisecond timestamp — NO
/// component of the LLM `input` ever reaches the path (threat T-17-10; the
/// `send_feedback` schema carries no path field, so separators / `..` are
/// structurally impossible here — asserted below and by the confinement test).
/// The payload is the user message + the session's recent `{tool,args}` calls +
/// the last error text; it NEVER includes an API key or secret (E-01), and it is
/// written to disk ONLY — no network endpoint (threat T-17-12 / offline-core).
fn write_feedback_file<C: AppCtx>(
    ctx: &C,
    session: &Mutex<AgentSession>,
    input: &serde_json::Value,
) -> Result<std::path::PathBuf, String> {
    let dir = ctx.app_data_dir()?.join("feedback");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create feedback dir: {e}"))?;

    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // SERVER-BUILT filename ONLY. Even though nothing here derives from `input`
    // today, enforce the confinement invariant defensively AS A REAL CHECK — a
    // `return Err` that also holds in `--release` (the shipped profile), not a
    // `debug_assert!` the compiler strips from release builds (LO-01). If a future
    // change ever threads untrusted input into the filename, this rejects the
    // write in production instead of silently doing nothing.
    let filename = format!("feedback-{millis}.json");
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("feedback filename must be a server-built, path-free component".to_string());
    }
    let path = dir.join(filename);

    let message = input.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let category = input.get("category").and_then(|v| v.as_str());
    let (recent_tool_calls, last_error) = {
        let sess = session
            .lock()
            .map_err(|_| "agent session poisoned".to_string())?;
        (
            sess.recent_tool_calls.iter().cloned().collect::<Vec<_>>(),
            sess.last_error.clone(),
        )
    };

    let payload = serde_json::json!({
        "timestamp": millis,
        "user_message": message,
        "category": category,
        "recent_tool_calls": recent_tool_calls,
        "last_error": last_error,
    });
    let json = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("write feedback file: {e}"))?;
    Ok(path)
}
