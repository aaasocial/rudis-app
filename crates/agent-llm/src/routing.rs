//! Phase 42 (ROUTE-01..04) — per-turn/per-round LLM model-tier routing.
//!
//! Pure, dependency-free, unit-testable (mirrors `retrieval.rs`'s
//! `select_top_k` style -- deterministic, explainable, zero external crate).
//!
//! Two layers (42-RESEARCH.md §2 -- the phase's central finding):
//! 1. **Pre-call classification** (`classify`): a conservative, ENUMERATED
//!    allowlist over the INCOMING user message only -- never turn length
//!    alone (Pitfall D1). Decides the STARTING tier for round 0.
//! 2. **Mid-turn escalation** (`should_escalate`): the shipped Phase-39
//!    Self-check rule ("a visual or audible edit OR a generation") is
//!    broader than "generation only" (Finding T1), so a pre-call classifier
//!    alone cannot guarantee a "trivial" turn never triggers
//!    `inspect_timeline`/`inspect_media`. The caller
//!    (`app_core::agent_turn::run_agent_turn`) latches a sticky escalation flag the instant ANY
//!    round's raw response contains an escalation-trigger tool_use, forcing
//!    the Default tier for every SUBSEQUENT round.

use crate::transport::ToolDef;

/// The two routing tiers (ROUTE-01). `Cheap` is ONLY ever chosen by
/// `classify()`'s enumerated allowlist; every other path fails closed to
/// `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteTier {
    Cheap,
    Default,
}

/// The cheap-tier model id (ROUTE-01).
///
/// OWNER DECISION 2026-08-02 (quick 260802-wvj): both tiers now resolve to
/// `claude-sonnet-5`. The previous `claude-haiku-4-5-20251001` was measured
/// failing two Phase-55 GATE-05 eval rows on this tier (eval-016 misreading
/// "full volume" as a +6 dB boost, eval-022 acting instead of halting on an
/// ambiguous reference).
///
/// The ROUTING LAYER IS DELIBERATELY RETAINED even though both constants are
/// now equal: the owner changed the MODEL, not the architecture. The tiers
/// still differ in the thing this module's `cheap_tier_tools()` controls --
/// the Cheap route offers the reduced `CHEAP_TIER_TOOL_NAMES` schema while
/// Default offers the full `tool_defs()` set. That schema split is the
/// remaining live difference between the tiers, and it is untouched.
///
/// NOTE for anyone re-baselining cost/latency: Sonnet 5 uses a NEW TOKENIZER
/// that emits ~30% more tokens than Sonnet 4.6 for identical text, so every
/// token-budget, `max_tokens` and cost figure previously measured against
/// haiku-4-5 or opus-4-8 is STALE. Re-measure with `count_tokens` before
/// trusting any of them.
pub const CHEAP_TIER_MODEL: &str = "claude-sonnet-5";

/// The default-tier model id. Byte-identical to `turn.rs`'s own
/// `model_name()` fallback literal -- kept as a SEPARATE constant (not
/// re-derived) so this module stays pure/dependency-free.
///
/// Equal to `CHEAP_TIER_MODEL` as of 2026-08-02 (see above). Kept separate on
/// purpose: the two tiers remain independently addressable, so re-splitting
/// them later is a one-line change here rather than an architectural undo.
pub const DEFAULT_TIER_MODEL: &str = "claude-sonnet-5";

/// Case-insensitive substrings whose presence ANYWHERE in the incoming
/// message hard-excludes Cheap tier, regardless of any positive match below.
/// Checked FIRST. Never a turn-length/complexity score (Pitfall D1) -- an
/// explicit, reviewable list.
const HARD_EXCLUDE_KEYWORDS: &[&str] = &[
    "generate", "create a", "create an", "make a", "make an",
    "check", "look", "see if", "does it", "match", "verify", "confirm",
    "inspect",
    "canvas", "sketch", "annotation", "annotate", "whiteboard", "drawing",
    "all of", "all the", "every clip", "each clip", "the clips",
    "layout", "split-screen", "split screen", "grid", "picture-in-picture",
    "pip", "track object", "tracking",
];

/// Case-insensitive substrings that, absent any hard-exclude hit, indicate a
/// single-target mechanical edit -- tied to `EDIT_TOOL_NAMES`'s simplest
/// single-parameter members (trimClip/setClipVolume/setClipMuted/moveClip/
/// duplicateClip/splitClip/removeClip), calibrated against the exact request
/// phrasing of eval-002/eval-008/eval-009.
const MECHANICAL_EDIT_KEYWORDS: &[&str] = &[
    "trim", "cut the first", "cut the last", "shorten",
    "volume", "mute", "unmute",
    "move the clip", "move clip",
    "duplicate",
    "split the clip", "split clip",
    "remove the clip", "remove clip", "delete the clip", "delete clip",
];

/// Conservative, enumerated allowlist classification (ROUTE-01). NEVER a
/// length/keyword-count score (Pitfall D1) -- fails CLOSED (`Default`) on any
/// ambiguous, empty, or unrecognized input.
pub fn classify(user_message: &str) -> RouteTier {
    let lower = user_message.to_ascii_lowercase();
    if HARD_EXCLUDE_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return RouteTier::Default;
    }
    if MECHANICAL_EDIT_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        return RouteTier::Cheap;
    }
    RouteTier::Default
}

/// Resolve the model id for ONE round (ROUTE-01/02). The caller MUST pass
/// `env_override` (typically `std::env::var("RUDIS_AGENT_MODEL").ok()`) to
/// preserve the existing E-01/D-10 escape hatch -- this function is
/// deliberately pure (no internal env I/O) so it stays trivially,
/// deterministically unit-testable with zero risk of cross-test env-var
/// races under cargo's default parallel test execution. `escalated=true`
/// forces `DEFAULT_TIER_MODEL` regardless of `tier` -- the Layer-2 safety
/// net (ROUTE-02).
pub fn resolve_model(tier: RouteTier, escalated: bool, env_override: Option<&str>) -> String {
    if let Some(pinned) = env_override {
        return pinned.to_string();
    }
    if escalated {
        return DEFAULT_TIER_MODEL.to_string();
    }
    match tier {
        RouteTier::Cheap => CHEAP_TIER_MODEL.to_string(),
        RouteTier::Default => DEFAULT_TIER_MODEL.to_string(),
    }
}

/// Tool names whose appearance in ANY round's raw tool_use blocks forces
/// escalation to the Default tier starting the FOLLOWING round (ROUTE-02).
/// The exact set the host's Pattern-C `is_intercepted_meta_tool` pre-scan
/// (`crates/app-core/src/agent_turn.rs`) already isolates before
/// `apply_response` runs -- the agent-eyes vision tools plus the three PAID
/// external-provider generation tools. Deliberately does NOT include
/// `generate_image`/`generate_video` (Rudis's own free, local, declarative
/// renderer, Phase 24) -- those are non-vision, and their own self-check
/// still routes through `inspect_timeline`/`inspect_media`, which ARE
/// covered.
pub const ESCALATION_TRIGGER_TOOL_NAMES: &[&str] = &[
    "inspect_timeline",
    "inspect_media",
    "generate_ai_image",
    "generate_ai_video",
    "generate_ai_audio",
];

/// True iff any of `round_tool_names` is an escalation-trigger name
/// (ROUTE-02). Pure, no I/O -- the caller
/// (`app_core::agent_turn::run_agent_turn`)
/// calls this once per round against that round's raw ToolUse names and
/// latches the result for the rest of the turn (escalation is sticky, never
/// un-set mid-turn).
pub fn should_escalate<'a>(round_tool_names: impl IntoIterator<Item = &'a str>) -> bool {
    round_tool_names
        .into_iter()
        .any(|name| ESCALATION_TRIGGER_TOOL_NAMES.contains(&name))
}

/// The reduced tool-name subset offered to a Cheap-tier turn (ROUTE-04).
/// Small and reviewable, mirroring `agent_tools::lib.rs`'s existing
/// `EDIT_TOOL_NAMES`/`NON_EDIT_TOOL_NAMES` const-array idiom. Deliberately
/// KEEPS `inspect_timeline`/`inspect_media` reachable so a mechanical edit's
/// own self-check (CHECK-01) stays possible even on a Cheap-tier turn --
/// Layer 2 (`should_escalate`) still promotes the round AFTER either is
/// called to the Default tier, so the round that actually PROCESSES the
/// returned image is never Cheap. Deliberately EXCLUDES generation/layout/
/// canvas tools: `classify()`'s own hard-exclude keywords already guarantee
/// a Cheap-tier turn's INITIAL request never asked for one of these, so
/// omitting them from the schema is a harmless extra layer of structural
/// safety, not a functionality cut.
pub const CHEAP_TIER_TOOL_NAMES: &[&str] = &[
    "trimClip",
    "setClipVolume",
    "setClipMuted",
    "moveClip",
    "duplicateClip",
    "splitClip",
    "removeClip",
    "get_timeline",
    "askUser",
    "proposeOptions",
    "inspect_timeline",
    "inspect_media",
];

/// A byte-identical FILTERED SLICE of `crate::tools::tool_defs()` (ROUTE-04)
/// -- never a second, independently-authored schema. `tools::tool_defs()` is
/// itself already a thin 1:1 adapter over `agent_tools::tool_defs()`, so this
/// is two `.filter()` hops from the single authored source, never a
/// duplicate.
pub fn cheap_tier_tool_defs() -> Vec<ToolDef> {
    crate::tools::tool_defs()
        .into_iter()
        .filter(|d| CHEAP_TIER_TOOL_NAMES.contains(&d.name.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_routes_the_calibration_fixtures_cheap() {
        assert_eq!(classify("Cut the first half-second off the start of the clip."), RouteTier::Cheap);
        assert_eq!(classify("Set the clip volume to 0 dB (unity gain)."), RouteTier::Cheap);
        assert_eq!(classify("Mute this clip."), RouteTier::Cheap);
    }

    #[test]
    fn classify_hard_excludes_generation_and_vision_language() {
        assert_eq!(
            classify("Generate a 16:9 title-card image for the intro, check it before you finish, and tell me exactly what you checked."),
            RouteTier::Default
        );
        assert_eq!(classify("Check if the export matches what I asked for."), RouteTier::Default);
        assert_eq!(classify("Does it match the sketch on the canvas?"), RouteTier::Default);
        assert_eq!(
            classify("Trim the intro but make sure it still looks right."),
            RouteTier::Default,
            "a mechanical keyword alongside a hard-exclude keyword must still fail closed"
        );
    }

    #[test]
    fn classify_fails_closed_on_unrecognized_or_ambiguous_input() {
        assert_eq!(classify(""), RouteTier::Default);
        assert_eq!(classify("asdfasdf gibberish"), RouteTier::Default);
        assert_eq!(classify("Make the video better."), RouteTier::Default);
    }

    #[test]
    fn classify_never_routes_based_on_message_length_alone() {
        assert_eq!(classify("fix it"), RouteTier::Default, "short is not automatically trivial");
        assert_eq!(
            classify("Could you please just trim the very start of the clip back a tiny little bit, nothing dramatic, just a small touch, thanks so much for doing this for me today"),
            RouteTier::Cheap,
            "an unambiguously mechanical ask must route cheap regardless of length"
        );
    }

    #[test]
    fn resolve_model_picks_the_tier_model_when_not_escalated_and_no_override() {
        assert_eq!(resolve_model(RouteTier::Cheap, false, None), CHEAP_TIER_MODEL);
        assert_eq!(resolve_model(RouteTier::Default, false, None), DEFAULT_TIER_MODEL);
    }

    /// HONESTY NOTE (2026-08-02): since `CHEAP_TIER_MODEL == DEFAULT_TIER_MODEL`,
    /// these two assertions can no longer FAIL by picking the wrong tier -- the
    /// model string stopped being a discriminator the moment both tiers were
    /// pointed at `claude-sonnet-5`. They are retained (not deleted) because they
    /// pin `resolve_model`'s CONTRACT and regain their teeth the instant the tiers
    /// are ever re-split.
    ///
    /// The observable that DOES still discriminate is the tool schema (cheap
    /// subset -> full `tool_defs()`). ⚠ The test that proved it,
    /// `escalation_widens_the_tool_schema_starting_the_round_after_the_trigger`,
    /// lived in `src-tauri` and was DELETED with that shell at Phase 55
    /// (GATE-07); it has no successor in `crates/app-core`. So escalation's
    /// schema-widening behaviour is currently unproven by any live test — do not
    /// read this note as a claim that it is covered.
    #[test]
    fn resolve_model_escalation_forces_default_regardless_of_tier() {
        assert_eq!(resolve_model(RouteTier::Cheap, true, None), DEFAULT_TIER_MODEL);
        assert_eq!(resolve_model(RouteTier::Default, true, None), DEFAULT_TIER_MODEL);
    }

    #[test]
    fn resolve_model_env_override_wins_over_tier_and_escalation() {
        assert_eq!(resolve_model(RouteTier::Cheap, false, Some("pinned")), "pinned");
        assert_eq!(resolve_model(RouteTier::Default, true, Some("pinned")), "pinned");
    }

    #[test]
    fn should_escalate_triggers_on_each_escalation_trigger_name() {
        for name in ESCALATION_TRIGGER_TOOL_NAMES {
            assert!(should_escalate([*name]), "{name} must trigger escalation");
        }
    }

    #[test]
    fn should_escalate_does_not_trigger_on_mechanical_edit_names() {
        assert!(!should_escalate(["trimClip", "setClipVolume", "moveClip"]));
        assert!(!should_escalate(Vec::<&str>::new()));
    }

    #[test]
    fn cheap_tier_tool_names_keeps_self_check_reachable() {
        assert!(CHEAP_TIER_TOOL_NAMES.contains(&"inspect_timeline"));
        assert!(CHEAP_TIER_TOOL_NAMES.contains(&"inspect_media"));
    }

    #[test]
    fn cheap_tier_tool_names_excludes_generation_and_batch_tools() {
        const MUST_BE_ABSENT: &[&str] = &[
            "generate_ai_image", "generate_ai_video", "generate_ai_audio",
            "generate_image", "generate_video", "apply_layout",
        ];
        for name in MUST_BE_ABSENT {
            assert!(!CHEAP_TIER_TOOL_NAMES.contains(name), "{name} must NOT be in the cheap-tier schema");
        }
    }

    #[test]
    fn cheap_tier_tool_defs_is_a_proper_nonempty_reduction() {
        let full = crate::tools::tool_defs();
        let cheap = cheap_tier_tool_defs();
        assert!(!cheap.is_empty());
        assert!(cheap.len() < full.len(), "must actually be a reduction");
        assert_eq!(cheap.len(), CHEAP_TIER_TOOL_NAMES.len());
    }
}
