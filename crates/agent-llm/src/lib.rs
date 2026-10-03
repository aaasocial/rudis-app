//! Rudis real-Claude agent crate (Phase 12): the ONE sanctioned network
//! dependency in the whole workspace (`reqwest`, confined here) plus the
//! orchestration loop that drives real, undoable `rudis_core` timeline
//! mutations from a live (or fixture-scripted) Claude conversation.
//!
//! NEVER a dependency of `crates/agent-mcp`/`rmcp` and vice versa (Pitfall
//! 2, 12-RESEARCH.md) -- Chat is a sibling front door onto the SAME
//! `rudis_core` spine as the dev-only MCP transport, not a consumer of it.

pub mod cards;
pub mod fixture;
pub mod key_store;
pub mod retrieval;
pub mod routing;
pub mod rulebook;
pub mod skills;
pub mod tools;
pub mod transport;
pub mod turn;
pub mod vision;

// `apply_option_card` is re-exported under the `cards_` alias so Plan 14-05's
// own, differently-shaped Tauri-command-level `apply_option_card` function can
// share the crate without a name collision.
pub use cards::{apply_option_card as cards_apply_option_card, OptionCard};
pub use fixture::FixtureTransport;
pub use key_store::{
    validate_credential_service, validate_key_format, CredentialService, InMemoryKeyStore,
    KeyStore, KeyStoreError, KeyValidator, KeyringStore, TEST_SERVICE_PREFIX,
    TEST_SERVICE_SUFFIX_MAX,
};
pub use retrieval::{
    append_library_entry, harvest_confirmed_outcome, load_library, near_duplicate_pairs,
    render_examples_block, seed_library, select_top_k, LibraryExample, SelfCheckVerdict,
};
pub use rulebook::{RULEBOOK_V1, RULEBOOK_V2, RULEBOOK_V3};
pub use skills::{
    matching_playbooks, render_playbooks_block, render_skill_index, skill_body, SkillPlaybook,
    SKILL_PLAYBOOKS,
};
pub use tools::{parse_edit_tool, tool_defs, ToolParseError};
pub use turn::{
    apply_response, prune_stale_images, reconcile_tool_results, run_turn, run_turn_with_context,
    run_turn_with_context_and_model, AskUser, RoundOutcome, TurnOutcome, HALT_SKIP_ACK, MAX_ROUNDS,
};
pub use transport::{
    api_key_configured, key_configured, AnthropicTransport, CacheControl, ContentBlock,
    ImageSource, LlmError, LlmTransport, MessageParam, MessagesRequest, MessagesResponse, Role,
    SystemBlock, ToolDef, ToolResultBlock, Usage, NO_API_KEY_MESSAGE,
};
pub use vision::{
    image_content_block_jpeg, image_content_block_png, image_tool_result, images_tool_result,
    text_tool_result,
};
