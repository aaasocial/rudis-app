//! Thin adapter over the shared `agent_tools` crate (Phase 16, TOOL-06/D-01).
//!
//! The authored 17 tool schemas + the `parse_edit_tool` boundary now live in
//! `crates/agent-tools` (the ONE source both this crate and `crates/agent-mcp`
//! consume — Pattern 8, RESEARCH.md). This module's only job is adapting the
//! shared, transport-agnostic `agent_tools::ToolDef` into this crate's
//! Anthropic-wire `transport::ToolDef` (which additionally carries
//! `cache_control`, an Anthropic-specific concept the shared crate does not
//! know about).

pub use agent_tools::{parse_edit_tool, ToolParseError};

/// Adapt the shared authored tool list into this crate's wire `ToolDef`,
/// injecting `cache_control: None` (unchanged from before this phase — `run_turn`
/// / `run_turn_inner` marks ONLY the last tool for prompt caching, not this
/// authoring layer). Order is preserved verbatim from `agent_tools::tool_defs()`.
pub fn tool_defs() -> Vec<crate::transport::ToolDef> {
    agent_tools::tool_defs()
        .into_iter()
        .map(|d| crate::transport::ToolDef {
            name: d.name,
            description: d.description,
            input_schema: d.input_schema,
            strict: d.strict,
            cache_control: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_defs_adapts_every_shared_def_with_no_cache_control() {
        let defs = tool_defs();
        assert_eq!(defs.len(), agent_tools::tool_defs().len(), "adapts every shared def, 1:1");
        assert!(
            defs.iter().all(|d| d.cache_control.is_none()),
            "authoring layer never sets cache_control — that is run_turn's job"
        );
    }

    #[test]
    fn tool_defs_preserves_shared_crates_fixed_order() {
        let names: Vec<String> = tool_defs().into_iter().map(|d| d.name).collect();
        let shared_names: Vec<String> =
            agent_tools::tool_defs().into_iter().map(|d| d.name).collect();
        assert_eq!(names, shared_names, "order must be byte-identical to the shared source");
    }
}
