//! The versioned MOAT-01 rulebook. Frozen text lives in `rulebook_v1.md`;
//! bump the file name (`rulebook_v2.md`) and this const together when the
//! eval loop's maintenance cycle (rudis-agent-rulebook-guide.md Part 3)
//! produces a revision -- never edit v1 in place once shipped.
pub const RULEBOOK_V1: &str = include_str!("rulebook_v1.md");

/// v2: v1's text plus the Canvas world-model bullet and the
/// "# Interpreting sketches" section (CANV-03). RULEBOOK_V1 stays
/// UNCHANGED and its own tests keep passing -- never edit a shipped
/// rulebook file in place; bump filename + const together instead.
pub const RULEBOOK_V2: &str = include_str!("rulebook_v2.md");

/// v3 (Phase 42.3): v2 with the "# Prompt expansion" and "# Transitions
/// between two clips" bodies EXTRACTED to trigger-loaded skill playbooks
/// (`generation-prompting`, `video-transitions` -- skills.rs) and a compact
/// "# Generation" section added (images-first, capability fence, the
/// reference-frame rule, the pre-spend confirmation pause, skill pointers).
/// RULEBOOK_V1/V2 stay UNCHANGED and their tests keep passing -- never edit
/// a shipped rulebook file in place; bump filename + const together instead.
pub const RULEBOOK_V3: &str = include_str!("rulebook_v3.md");
