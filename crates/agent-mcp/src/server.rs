//! The real Rudis agent MCP server (Phase 11, Plan 06 — Wave 3 integration;
//! tool surface rebuilt programmatically in Phase 16, Plan 04).
//!
//! This is the WIRING layer AUTH-01 requires: the point where a real MCP client
//! (Plan 07's deterministic stub, and live Claude via Phase 12+) can drive the
//! v1 timeline. It exposes the SHARED agent tool surface over the `rmcp`
//! transport Plan 01 DECIDED (`DECISIONS.md` Decision 1):
//!
//! - the 14 intent-shaped edit tools named by [`agent_tools::EDIT_TOOL_NAMES`]
//!   (`placeClip` through `clearCanvas`) plus the `get_timeline` state read —
//!   ALL dynamically routed from `agent_tools::tool_defs()` (Phase 16,
//!   TOOL-06). Every tool advertises its EXACT authored field-typed schema
//!   (never a generic `{"type":"object"}` placeholder), and a tool added to
//!   `agent_tools` alone appears here automatically — the construction that
//!   makes the two-surface schema drift which once let the Phase-14.1
//!   canvas-delete tools go missing structurally hard to repeat.
//! - two MCP-only control tools: `beginTurn` / `endTurn` (opt-in multi-call
//!   batching), hand-authored in their own `control_tool_router` block —
//!   they are session/turn-boundary controls with no `rudis_core::tools::Tool`
//!   equivalent, deliberately NOT part of the shared schema crate.
//!
//! **Zero new business logic.** Every edit call is parsed by
//! `agent_tools::parse_edit_tool` into the matching `rudis_core::tools::Tool`
//! variant, `resolve()`d against the CURRENT project snapshot, and
//! `dispatch()`ed through the SAME server-held `Store`. All domain arithmetic
//! (frames/dB/trim deltas), validation, and undo semantics already live in
//! `crates/core` — this layer only adds a transport.
//!
//! ## Turn-boundary design (a genuine MCP-protocol gap resolved here)
//!
//! Vanilla MCP has no native "my multi-tool-call turn ends here" signal —
//! `tools/call` is inherently per-tool. To avoid regressing v1's "one action =
//! one undo step" UX, EVERY edit-tool call auto-wraps itself in
//! `begin_turn()` + dispatch(es) + `end_turn()` (for a single command this is
//! byte-identical to a bare `dispatch()` — one undo step, unchanged). The two
//! control tools `beginTurn`/`endTurn` let a caller OPT INTO batching several
//! tool calls into ONE undo step: `beginTurn` sets a SERVER-side flag
//! ([`RudisMcpServer::explicit_turn_open`], deliberately NOT `Store` state) that
//! suppresses the auto-wrap until `endTurn`. `Store::begin_turn()`'s documented
//! defensive idempotency (Plan 02) keeps both paths safe even if they overlap.
//!
//! ## Scope-exit turn guard (threat T-11-11 / carried-forward T-11-03)
//!
//! Each auto-wrapped edit call closes its own turn via [`TurnGuard`], an RAII
//! Drop guard: even if a handler returns early on a resolve/dispatch error, the
//! auto-opened turn is ALWAYS closed, so a dropped connection mid-handler can
//! never leave `open_turn` open. The EXPLICIT `beginTurn`..`endTurn` path is a
//! documented caller-discipline requirement: a caller that calls `beginTurn`
//! and never `endTurn` (dropped connection mid-batch) leaves `explicit_turn_open`
//! stuck `true`; `Store::begin_turn()`'s idempotency bounds the worst case to
//! "the next call's undo step is larger than intended," never data corruption
//! (a connection-drop handler resetting the flag is a Phase-12 hardening item,
//! out of this phase's scope).
//!
//! DEV-ONLY: this crate is NEVER a dependency of the shipped `rudis-app` /
//! `src-tauri` build (Pitfall 4, verified structurally by this plan's
//! acceptance criteria). No HTTP listener, no network socket — a local test
//! harness until AUTH-02 (Phase 15).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{object, CallToolResult, ContentBlock, Tool as McpTool};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use rudis_core::tools::Tool;
use rudis_core::Store;

/// Arguments for `get_timeline`. The `selection` is the frontend's opaque
/// clip-id selection passed through verbatim (Plan 04 semantics — no backend
/// selection state); it defaults to `[]` when omitted.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetStateArgs {
    /// Opaque clip ids the caller currently has selected. Passed through
    /// verbatim to `rudis_core::agent_state::view`. Defaults to empty.
    #[serde(default)]
    pub selection: Vec<String>,
}

/// The real agent MCP server: wraps the SAME `rudis_core::Store` the v1 app
/// owns, behind an async mutex so concurrent tool calls serialize through the
/// one backend-owned state (Convention 4: backend owns state).
#[derive(Clone)]
pub struct RudisMcpServer {
    /// The one live backend-owned `Store`. Held behind a `tokio::sync::Mutex`
    /// so each `tools/call` handler locks it for the duration of its
    /// resolve+dispatch, exactly like the Tauri app's single mutexed `Store`.
    store: Arc<Mutex<Store>>,
    /// SERVER-layer turn flag, deliberately NOT inside `Store`. An explicit
    /// `beginTurn`/`endTurn` pair toggles it; each edit-tool call consults it to
    /// decide whether to auto-wrap its own `begin_turn()`/`end_turn()`.
    explicit_turn_open: Arc<AtomicBool>,
    /// The tool router, built ONCE in [`RudisMcpServer::new`] via
    /// [`RudisMcpServer::tool_router`] and cached here (16-review fix 3).
    /// `#[tool_handler(router = self.tool_router)]` points the macro at this
    /// field — previously it used the macro's default `Self::tool_router()`
    /// expression, which is spliced verbatim into `call_tool`/`list_tools`/
    /// `get_tool`, rebuilding all 17 authored JSON schemas on EVERY tool RPC.
    /// This is rmcp's own documented field-plus-builder pattern; the field and
    /// the associated fn deliberately share the name (distinct namespaces).
    /// Cloning the server clones only Arc'd handlers + schema `Value`s.
    tool_router: ToolRouter<Self>,
}

/// RAII scope-exit guard for the AUTO-wrapped turn boundary (T-11-11 mitigation
/// + Plan 02's carried-forward T-11-03 note). Constructed only when a handler
/// auto-opened a turn (`!explicit`); its `Drop` calls `store.end_turn()`, so
/// the turn is closed on EVERY exit path — normal return, early error return,
/// or an unwind — and a dropped connection mid-handler can never strand an open
/// turn. `Store::end_turn()` is a no-op / empty-group-safe close, so guarding an
/// already-closed or never-populated turn is harmless.
struct TurnGuard<'a> {
    store: &'a mut Store,
    active: bool,
}

impl<'a> TurnGuard<'a> {
    /// Open an auto-turn on `store` and return a guard that will close it on
    /// drop. When `explicit` is true a turn is already caller-managed, so this
    /// is inert (opens nothing, closes nothing).
    fn open(store: &'a mut Store, explicit: bool) -> Self {
        if !explicit {
            store.begin_turn();
        }
        TurnGuard {
            store,
            active: !explicit,
        }
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.store.end_turn();
        }
    }
}

/// Dynamic dispatch for every edit tool (all 14 `EDIT_TOOL_NAMES` entries): the
/// tool's NAME is read from `ctx.name()` at call time (never captured per-route),
/// so ONE function handles all 14 — this is what makes adding a 15th edit tool
/// in `agent_tools::tool_defs()` alone (zero new hand-written methods here)
/// automatically wire up correctly. A parse failure is surfaced as a tool-level
/// error result (T-16-09: malformed JSON never reaches `Tool::resolve`).
fn dispatch_edit_dynamic(
    ctx: ToolCallContext<'_, RudisMcpServer>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_>> {
    Box::pin(async move {
        let value = Value::Object(ctx.arguments.clone().unwrap_or_default());
        let result = match agent_tools::parse_edit_tool(ctx.name(), value) {
            Ok(tool) => ctx.service.run_edit(tool).await,
            Err(err) => CallToolResult::error(vec![ContentBlock::text(format!(
                "invalid tool args: {err}"
            ))]),
        };
        Ok(result)
    })
}

/// Dynamic route for `get_timeline` (formerly the hand-written `get_state`
/// method) — same full-state-view logic, unchanged, just relocated so it can
/// be registered from the SAME `agent_tools::tool_defs()` loop as the edit
/// tools (Open Question 2 in 16-RESEARCH.md: route get_timeline through the
/// shared list too, since it is genuinely shared between both surfaces).
fn get_timeline_dynamic(
    ctx: ToolCallContext<'_, RudisMcpServer>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_>> {
    Box::pin(async move {
        // Absent arguments mean "default/empty selection"; PRESENT-but-malformed
        // args (e.g. `selection` as a string instead of an array) are surfaced
        // as a tool-level error result, exactly like `dispatch_edit_dynamic`'s
        // parse failures (16-review fix 1 — never silently swallow into a
        // default and return a successful full-state view).
        let args: GetStateArgs = match ctx.arguments.clone() {
            None => GetStateArgs::default(),
            Some(m) => match serde_json::from_value(Value::Object(m)) {
                Ok(args) => args,
                Err(err) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "invalid tool args: {err}"
                    ))]));
                }
            },
        };
        let project = ctx.service.store.lock().await.snapshot();
        let view = rudis_core::agent_state::view(&project, &args.selection);
        let compact = rudis_core::agent_state::render_compact(&view);
        let result = match serde_json::to_value(&view) {
            Ok(view_json) => {
                CallToolResult::structured(json!({ "compact": compact, "view": view_json }))
            }
            Err(err) => CallToolResult::error(vec![ContentBlock::text(format!(
                "internal: failed to serialize view: {err}"
            ))]),
        };
        Ok(result)
    })
}

/// Dynamic route for `get_media` (Phase 26, TOOL-04): the SAME
/// no-app_data_dir-dependency reasoning as get_timeline_dynamic --
/// media_view() is a pure Store read, so this works identically on the
/// MCP surface as it does in-app.
fn get_media_dynamic(
    ctx: ToolCallContext<'_, RudisMcpServer>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_>> {
    Box::pin(async move {
        let project = ctx.service.store.lock().await.snapshot();
        let view = rudis_core::agent_state::media_view(&project);
        let result = match serde_json::to_value(&view) {
            Ok(view_json) => CallToolResult::structured(json!({ "view": view_json })),
            Err(err) => CallToolResult::error(vec![ContentBlock::text(format!(
                "internal: failed to serialize view: {err}"
            ))]),
        };
        Ok(result)
    })
}

/// Dynamic route for `undo` (Phase 17-03, D-09 / TOOL-07): the ONE meta tool
/// exposed on the MCP surface (cheap, Store-only — no engine/session/
/// filesystem dependency). Mirrors `get_timeline_dynamic`'s shape: lock the
/// server-held `Store`, pop the last COMMITTED edit step via `Store::undo()`,
/// and report whether anything was reverted as structured JSON. An empty undo
/// stack is a benign `{ "undone": false }`, never a tool error. The other 3
/// meta tools (`read_skill`/`send_feedback`/`export_project`) stay in-app-only
/// by design — the router loop below keeps skipping them.
fn undo_dynamic(
    ctx: ToolCallContext<'_, RudisMcpServer>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_>> {
    Box::pin(async move {
        let undone = ctx.service.store.lock().await.undo().is_some();
        Ok(CallToolResult::structured(json!({ "undone": undone })))
    })
}

impl RudisMcpServer {
    /// Wrap an existing backend-owned `Store` (shared, so tests and the Tauri
    /// app can hold their own handle to the same live state).
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            explicit_turn_open: Arc::new(AtomicBool::new(false)),
            tool_router: Self::tool_router(),
        }
    }

    /// A handle to the same live `Store` this server drives — used by
    /// deterministic tests to inspect state / call `undo()` directly, and by a
    /// future app embedding to share one `Store` across the UI and the agent.
    pub fn store_handle(&self) -> Arc<Mutex<Store>> {
        Arc::clone(&self.store)
    }

    // ---- the shared edit path (all 14 edit tools funnel through here) -------

    /// Resolve one `Tool` against the live project and dispatch its command(s),
    /// honoring the auto-wrap-vs-explicit-turn flag via [`TurnGuard`]. Returns
    /// the [`rudis_core::TimelineDelta`] between the pre- and post-dispatch
    /// snapshots as structured JSON on success (TOOL-01: the changed clips'
    /// NEW field values, so the caller patches its world-model without a full
    /// re-fetch — mirroring agent-llm's `dispatch_edit`, the reference
    /// before/dispatch/after/delta ordering, T-16-07), or a tool-level error
    /// result (never a panic / protocol error) on a `ToolError` or a mid-batch
    /// `dispatch` rejection. Per research's flagged partial-failure semantics,
    /// a dispatch failure partway through a composite tool's command list is
    /// surfaced as an error WITHOUT rolling back already-applied commands in
    /// this same call.
    async fn run_edit(&self, tool: Tool) -> CallToolResult {
        let explicit = self.explicit_turn_open.load(Ordering::SeqCst);
        let mut store = self.store.lock().await;

        // resolve() sees the PRE-dispatch snapshot, exactly as before; the
        // same snapshot doubles as delta()'s `before` input.
        let before = store.snapshot();
        let commands = match tool.resolve(&before) {
            Ok(cmds) => cmds,
            Err(err) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "tool rejected: {err}"
                ))]);
            }
        };

        // Auto-open a turn (unless the caller has an explicit turn open). The
        // guard closes it on EVERY exit path below.
        let guard = TurnGuard::open(&mut store, explicit);

        for cmd in commands {
            if let Err(err) = guard.store.dispatch(cmd) {
                // Partial failure: the guard's Drop still closes the turn,
                // grouping whatever succeeded — no in-handler rollback.
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "command rejected: {err}"
                ))]);
            }
        }
        let after = guard.store.snapshot();
        drop(guard); // close the auto-turn before serializing the result

        let delta = rudis_core::agent_state::delta(&before, &after);
        match serde_json::to_value(&delta) {
            Ok(value) => CallToolResult::structured(json!({ "delta": value })),
            Err(err) => CallToolResult::error(vec![ContentBlock::text(format!(
                "internal: failed to serialize delta: {err}"
            ))]),
        }
    }

    /// Built PROGRAMMATICALLY from `agent_tools::tool_defs()` (Phase 16,
    /// TOOL-06/D-01/Pattern 8) — every edit tool + `get_timeline` gets its
    /// EXACT authored field-typed schema advertised over MCP (never the old
    /// generic `{"type":"object"}` RawArgs placeholder). Adding a 15th tool to
    /// `agent_tools::EDIT_TOOL_NAMES` alone makes it appear here automatically
    /// — no hand-written method to remember (the exact gap that let the
    /// canvas-delete tools go missing before this phase).
    ///
    /// `#[tool_handler]` (on `impl ServerHandler for RudisMcpServer`) calls
    /// this associated function by NAME — it does not care that it is
    /// hand-written rather than macro-generated.
    pub fn tool_router() -> ToolRouter<Self> {
        let mut router = ToolRouter::new();
        for def in agent_tools::tool_defs() {
            let is_edit = agent_tools::EDIT_TOOL_NAMES.contains(&def.name.as_str());
            let is_get_timeline = def.name == "get_timeline";
            let is_undo = def.name == "undo";
            let is_get_media = def.name == "get_media";
            if !is_edit && !is_get_timeline && !is_undo && !is_get_media {
                // proposeOptions/askUser are Claude(agent-llm)-only; read_skill/
                // send_feedback/export_project and the Phase-26
                // get_projects/new_project/open_project tools are in-app-only
                // (the latter 3 need app_data_dir, a Tauri-only concept
                // agent-mcp structurally lacks) — never advertised over MCP.
                continue;
            }
            let attr = McpTool::new(
                def.name.clone(),
                def.description.clone(),
                object(def.input_schema.clone()),
            );
            let route = if is_get_timeline {
                ToolRoute::new_dyn(attr, get_timeline_dynamic)
            } else if is_get_media {
                ToolRoute::new_dyn(attr, get_media_dynamic)
            } else if is_undo {
                ToolRoute::new_dyn(attr, undo_dynamic)
            } else {
                ToolRoute::new_dyn(attr, dispatch_edit_dynamic)
            };
            router.add_route(route);
        }
        router + Self::control_tool_router()
    }
}

// ---- MCP-only control tools ------------------------------------------------
//
// `beginTurn`/`endTurn` stay hand-authored in their own macro'd router block:
// they are session/turn-boundary controls with no `rudis_core::tools::Tool`
// equivalent (Open Question 2's recommendation, threat T-16-10 accepted) — they
// carry no domain mutation capability themselves, only toggle the auto-wrap
// flag, so excluding them from the shared schema crate creates no
// under-advertised capability gap.
#[tool_router(router = control_tool_router)]
impl RudisMcpServer {
    /// Open an explicit multi-call turn: subsequent edit-tool calls are NOT
    /// auto-wrapped, so a whole `beginTurn`..edits..`endTurn` batch collapses to
    /// ONE undo step. Idempotent at the `Store` level (Plan 02).
    #[tool(name = "beginTurn", description = "Begin an explicit multi-tool-call turn (one undo step for the whole batch)")]
    async fn begin_turn(&self) -> CallToolResult {
        self.store.lock().await.begin_turn();
        self.explicit_turn_open.store(true, Ordering::SeqCst);
        CallToolResult::structured(json!({ "turn": "open" }))
    }

    /// Close the explicit turn opened by `beginTurn`, pushing the whole batch as
    /// one undo entry and restoring the default auto-wrap behavior.
    #[tool(name = "endTurn", description = "End the explicit turn opened by beginTurn")]
    async fn end_turn(&self) -> CallToolResult {
        self.store.lock().await.end_turn();
        self.explicit_turn_open.store(false, Ordering::SeqCst);
        CallToolResult::structured(json!({ "turn": "closed" }))
    }
}

// `#[tool_handler(router = self.tool_router)]` wires `call_tool`/`list_tools`/
// `get_info` to the CACHED router field built once in `new()` (16-review
// fix 3). The macro splices the router expression verbatim into each generated
// method, so the previous default (`Self::tool_router()`) rebuilt the entire
// router — all 17 authored JSON schemas — from scratch on every tool RPC.
// `ToolRouter::{call, list_all, get}` all take `&self`, so the bare field
// expression borrows without cloning.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for RudisMcpServer {}
