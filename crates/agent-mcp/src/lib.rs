//! Rudis DEV-ONLY MCP transport spike (Phase 11, Plan 01 — Wave 1).
//!
//! This crate exists to answer ONE genuinely uncertain technical question the
//! phase research flagged at MEDIUM confidence (Assumptions Log A1): does
//! `rmcp` 2.1.0's macro API (`#[tool_router]` / `#[tool]` / `#[tool_handler]`,
//! `Parameters<T>`, `ServiceExt::serve`, the `(AsyncRead, AsyncWrite)`-tuple
//! transport) actually compile and round-trip a trivial tool call the way the
//! research's WebFetch-reconstructed sketch claimed?
//!
//! The answer (proven by `tests/spike_tool_roundtrip.rs`): YES — the researched
//! macro shape compiles verbatim against the real crate, so later plans (06/07)
//! build the full 12-tool agent surface on `rmcp`, not a hand-rolled JSON-RPC
//! fallback. See `.planning/phases/11-agent-tool-spine-eval-harness/DECISIONS.md`.
//!
//! DEV-ONLY: this crate is NEVER a dependency of the shipped `rudis-app` /
//! `src-tauri` build. Beginners never see MCP.

use rmcp::{ServerHandler, handler::server::wrapper::Parameters, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// The real agent MCP server (Plan 06; tool surface rebuilt programmatically
/// from `agent_tools::tool_defs()` in Phase 16 Plan 04 — 14 edit tools +
/// `get_timeline`, plus the MCP-only `beginTurn`/`endTurn` controls). The
/// Plan 01 `EchoServer` spike below is retained as a permanent transport
/// regression canary (per Plan 01's design) — it is NOT the production surface.
pub mod server;
pub use server::RudisMcpServer;

/// Arguments for the single spike `echo` tool. `#[derive(JsonSchema)]` is what
/// `rmcp`'s `#[tool]` macro turns into the tool's MCP `inputSchema` — exactly
/// how the real 12 tools will declare their parameter shapes in later plans.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EchoRequest {
    /// The message to echo back, unchanged.
    pub msg: String,
}

/// The trivial spike server: exposes exactly one MCP tool, `echo`, which returns
/// its input string unchanged. This is the smallest possible proof that a real
/// `tools/call` crosses a real MCP wire and comes back correct.
#[derive(Debug, Clone, Default)]
pub struct EchoServer;

#[tool_router]
impl EchoServer {
    /// Construct a fresh spike server.
    pub fn new() -> Self {
        Self
    }

    /// Echo a message back unchanged.
    #[tool(description = "Echo a message back unchanged")]
    async fn echo(&self, Parameters(req): Parameters<EchoRequest>) -> String {
        req.msg
    }
}

// `#[tool_handler]` generates `call_tool` / `list_tools` / `get_info` against the
// `ToolRouter` that `#[tool_router]` emitted as `Self::tool_router()`.
#[tool_handler]
impl ServerHandler for EchoServer {}
