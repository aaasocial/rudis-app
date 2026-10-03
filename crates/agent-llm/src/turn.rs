//! The orchestration loop (AGENT-01/03/05/06): `run_turn` brackets an ENTIRE
//! multi-round Chat turn in exactly one `Store::begin_turn()`/`end_turn()` pair
//! (the caller owns those brackets — today that caller is
//! `crates/app-core/src/agent_turn.rs`; Plan 04 first established the shape in
//! the since-retired `src-tauri`) regardless of how
//! many Claude round-trips or tool calls occur inside it, and `apply_response`
//! is the synchronous, Store-mutating half that resolves+dispatches every
//! `tool_use` block against a REAL `rudis_core::Store`, stopping at (never past)
//! the first `askUser` block.
//!
//! Two hard design commitments, both proven by the inline tests below:
//!
//! 1. **One turn, N round-trips.** `run_turn` NEVER touches a `Store` — it only
//!    drives the transport and threads `tool_result`s back. The one-turn-one-undo
//!    grouping is keyed to the whole `run_turn` invocation via the caller's
//!    `begin_turn`/`end_turn`, not to any single HTTP round (AGENT-03/05).
//!
//! 2. **`apply_response` is purely synchronous — no `.await` anywhere.** So a
//!    caller may lock a `std::sync::Mutex<Store>` around the WHOLE call without
//!    ever holding it across an async boundary (Pitfall 3). The ONE `.await` in
//!    this module lives in `run_turn`, on `transport.send()`, and never touches
//!    a `Store` lock.

use crate::tools::parse_edit_tool;
use crate::transport::{
    CacheControl, ContentBlock, LlmError, LlmTransport, MessageParam, MessagesRequest,
    MessagesResponse, Role, SystemBlock, ToolDef, ToolResultBlock,
};

/// A pending clarifying question raised by an `askUser` tool_use block, carrying
/// its `tool_use_id` so the caller (Plan 04) can thread a `tool_result` for it
/// (alongside any sibling edit `tool_result`s already applied this round) when
/// the user answers — per Anthropic's "every tool_use id needs a tool_result"
/// rule (Pitfall 5).
#[derive(Debug, Clone)]
pub struct AskUser {
    pub tool_use_id: String,
    pub question: String,
}

/// The outcome of applying ONE Claude response against the `Store`.
pub struct RoundOutcome {
    /// `ToolResult` blocks (one per processed edit/get_timeline `tool_use`), in
    /// order — the caller sends these back as the NEXT user turn's content
    /// (tool_results FIRST, per Anthropic's ordering rule).
    pub tool_results: Vec<ContentBlock>,
    /// Accumulated `Text` block content from this response (newline-joined), or
    /// `None` if the response carried no text.
    pub narration: Option<String>,
    /// `Some` iff an `askUser` block was seen — the loop halts on it.
    pub asked: Option<AskUser>,
    /// Every `Patch` dispatched this round (T-12-10: carried even when the round
    /// also asked a question, so the caller can narrate/emit exactly what changed
    /// before the pause).
    ///
    /// Phase 43 (LAT-02): each entry is `(patch, base_seq, seq)` — the pair
    /// `Store::dispatch` captured atomically alongside that specific
    /// mutation. The app layer's AGENT-05 mid-turn emission replays these
    /// one-for-one, so a live per-tool-call emission is exactly as
    /// seq-correct as every other emission site (no re-locking the store to
    /// read a seq that has since moved on).
    pub patches: Vec<(rudis_core::Patch, u64, u64)>,
    /// `Some` iff a `proposeOptions` block was seen — `(tool_use_id, cards)`.
    /// The loop halts on it exactly like `asked` (first-block-wins: at most
    /// ONE of `asked`/`options` is ever `Some` per round, T-14-10).
    pub options: Option<(String, Vec<crate::cards::OptionCard>)>,
    /// One `{"tool": name, "args": input}` JSON value per SUCCESSFULLY
    /// dispatched edit `tool_use`, in order — a failed call is NOT a
    /// "dispatched call" for library-growth purposes (Plan 14-06).
    pub dispatched_calls: Vec<serde_json::Value>,
}

/// The final result of a whole turn, surfaced to the caller/UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnOutcome {
    pub narration: Option<String>,
    pub clarifying_question: Option<String>,
    /// `Some` iff the turn halted on a `proposeOptions` block — the ≥2 cards
    /// the user must choose between (the `tool_use_id` stays in the caller's
    /// pending bookkeeping, mirroring how `AskUser.tool_use_id` never reaches
    /// `TurnOutcome`).
    pub options: Option<Vec<crate::cards::OptionCard>>,
    /// [bug/agent-turn-no-response-max-tokens] `true` iff the turn ended because
    /// a response hit the output-token ceiling ([`STOP_REASON_MAX_TOKENS`])
    /// rather than finishing what it was writing.
    ///
    /// This is the HONEST FACT, not UI copy: the model's last content block was
    /// cut off mid-write, so whatever `narration` accompanies this flag is
    /// partial (and was `None` entirely in the reported bug — the truncation
    /// landed inside a tool-input block, so no complete `Text` block existed).
    /// Work the turn had ALREADY completed still stands: rounds are applied to
    /// the `Store` as they arrive, so a truncated turn may well have made real
    /// edits or produced real assets. Callers must surface this rather than
    /// present an incomplete turn as a finished one.
    pub truncated: bool,
}

/// Safety cap against a runaway tool loop (T-12-08). A single turn will make at
/// most this many `transport.send()` round-trips regardless of model behavior.
pub const MAX_ROUNDS: u32 = 20;

/// [bug/agent-turn-no-response-max-tokens] the per-round OUTPUT token budget.
///
/// Was a hardcoded `4096` since the loop was written, which is genuinely tight
/// for the tools this app now ships: `generate_ai_video`/`generate_ai_image`
/// instruct the model to author a five-part Veo/Imagen prompt of up to 4000
/// CHARACTERS (~1000 tokens of tool input alone), and a vision turn narrates
/// more than a text-only one, so one round could legitimately want several
/// thousand output tokens and get cut off mid-block. A truncated round is
/// strictly worse than a slightly more expensive one: it wastes the ENTIRE
/// round's input cost (the whole re-sent history, images included) and returns
/// nothing usable.
///
/// This is a CEILING, not a target — the model stops when it is done, so raising
/// it costs nothing on ordinary turns and only pays out on the long ones that
/// were previously truncated. Kept well below the model's real maximum so a
/// pathological run still terminates: it bounds a single round, while
/// [`MAX_ROUNDS`] bounds the turn.
const MAX_OUTPUT_TOKENS: u32 = 16_384;

/// The Anthropic `stop_reason` meaning "I ran out of output budget mid-message".
/// A response bearing it is INCOMPLETE — its last content block was cut off
/// mid-write — so it must never be mistaken for a clean end of turn.
const STOP_REASON_MAX_TOKENS: &str = "max_tokens";

/// [bug/agent-history-413] the LAST-RESORT budget guard `run_turn_inner` checks
/// BEFORE every `transport.send()`. The Anthropic Messages API hard-caps the
/// serialized HTTP request BODY at 32MB (33_554_432 bytes) — independent of any
/// token/context budget — and returns an opaque, unrecoverable
/// `413 request_too_large` with NO server-side hint of what to trim; because the
/// caller persists+resends the SAME history forever, a request that crosses that
/// cap once stays broken on every future turn until something intervenes. `28`
/// MiB leaves ~4MiB of margin below the real 32MiB cap for what this raw
/// `serde_json::to_vec` count doesn't separately account for (JSON string
/// escaping expansion, the system/tools blocks — the whole rulebook + every tool
/// schema, re-sent every round). This is deliberately a BACKSTOP, not the
/// primary defense: the host's turn-start `prune_stale_images` call
/// (`crates/app-core/src/agent_turn.rs`, keeping
/// only the current turn's own fresh snapshot(s)) and the JPEG-not-PNG
/// chat-history vision snapshot are what keep ordinary sessions well under this
/// number in the first place.
const MAX_REQUEST_BYTES: usize = 28 * 1024 * 1024;

/// The `tool_result` content minted for a `tool_use` block that was recorded in
/// history but never executed — because the round halted on a control block
/// (`askUser`/`proposeOptions`) emitted alongside it, or because a message was
/// reconciled after the fact by `reconcile_tool_results`.
///
/// `is_error: Some(true)` on purpose: the call genuinely did NOT happen, so the
/// model must be told to retry it rather than believing it succeeded. The
/// wording is shared by `apply_response`, `reconcile_tool_results` and
/// the host's intercepted-meta-tool back-fill (`crates/app-core/src/agent_turn.rs`)
/// so all three surfaces speak with one voice.
pub const HALT_SKIP_ACK: &str =
    "skipped: the turn halted on a pending question; call this again after the user answers";

/// The model id used for every request, overridable via `RUDIS_AGENT_MODEL`.
///
/// E-01 / D-10 note: `RUDIS_AGENT_MODEL` is the ZERO-CODE model-tier latency
/// lever, and it still is — but the prose that used to live here (the default
/// being "Anthropic's slowest/highest-latency tier") is no longer true and was
/// removed rather than left to silently contradict the code.
///
/// The default is `claude-sonnet-5` as of 2026-08-02 (owner decision, quick
/// 260802-wvj) — a mid-tier model, not the slow path the old note described.
/// This literal is deliberately byte-identical to `routing::DEFAULT_TIER_MODEL`
/// (and, since that same decision, to `routing::CHEAP_TIER_MODEL` too); the two
/// are kept as separate literals so this function stays dependency-free.
///
/// Overriding via this env var remains a product QUALITY/latency tradeoff the
/// human can measure live. Any latency or cost figure recorded against the OLD
/// default is stale twice over: different model, and Sonnet 5's new tokenizer
/// emits ~30% more tokens than Sonnet 4.6 for the same text.
fn model_name() -> String {
    std::env::var("RUDIS_AGENT_MODEL").unwrap_or_else(|_| "claude-sonnet-5".to_string())
}

/// Drive one whole Chat turn to completion: repeatedly ask the transport for the
/// next step, apply it (via `apply_round`, the caller-supplied SYNCHRONOUS
/// Store-mutating closure), and thread the resulting `tool_result`s back, until
/// the model ends its turn, asks a clarifying question, or the `MAX_ROUNDS`
/// safety cap is hit.
///
/// The caller wraps this whole call in ONE `Store::begin_turn()`/`end_turn()`
/// pair, so the entire multi-round turn reverses as a single `undo()` step
/// (AGENT-03/05), and supplies `apply_round` closing over the (locked) `Store`.
///
/// `system` is ONE cache-marked `SystemBlock` (the rulebook); `tools` are sent
/// verbatim with `cache_control` set on ONLY the LAST tool (the exact placement
/// Plan 02's request-shape tests proved). The ONE `.await` is on
/// `transport.send()`.
pub async fn run_turn<T: LlmTransport>(
    transport: &T,
    rulebook: &str,
    tools: &[ToolDef],
    history: &mut Vec<MessageParam>,
    apply_round: impl FnMut(&MessagesResponse) -> RoundOutcome,
) -> Result<TurnOutcome, LlmError> {
    // The existing single-cached-block system vec, exactly as before the
    // Phase-14 run_turn_inner extraction — signature + behavior UNCHANGED.
    let system = vec![SystemBlock {
        kind: "text",
        text: rulebook.to_string(),
        cache_control: Some(CacheControl { kind: "ephemeral" }),
    }];
    run_turn_inner(transport, system, |_round| tools.to_vec(), history, |_round| model_name(), apply_round).await
}

/// The shared loop body `run_turn`/`run_turn_with_context` both delegate to,
/// parameterized on the FULLY ASSEMBLED `system` vec (otherwise byte-identical
/// to the pre-extraction `run_turn` loop).
async fn run_turn_inner<T: LlmTransport>(
    transport: &T,
    system: Vec<SystemBlock>,
    mut tools_for_round: impl FnMut(u32) -> Vec<ToolDef>,
    history: &mut Vec<MessageParam>,
    mut model_for_round: impl FnMut(u32) -> String,
    mut apply_round: impl FnMut(&MessagesResponse) -> RoundOutcome,
) -> Result<TurnOutcome, LlmError> {
    let mut narration: Option<String> = None;
    for round_num in 0..MAX_ROUNDS {
        // Phase 42 (WR-01): the tool schema is re-resolved EVERY round, exactly
        // like `model_for_round` — so a mid-turn escalation (ROUTE-02) can WIDEN
        // the offered tools (cheap subset -> full `tool_defs()`) starting the very
        // next round, not merely swap the model. `cache_control` is (re)applied to
        // the LAST tool each round; for a FIXED per-round tool set (every caller
        // except a routed+escalating turn) this is byte-identical to the pre-fix
        // single application before the loop, so the existing request-byte-shape
        // tests stay green.
        let mut tools_with_cache = tools_for_round(round_num);
        if let Some(last) = tools_with_cache.last_mut() {
            last.cache_control = Some(CacheControl { kind: "ephemeral" });
        }
        // The last line of defence before the request leaves: no `tool_use`
        // block anywhere in the history may go unanswered, or the API rejects
        // the WHOLE conversation (and keeps rejecting it forever, since the
        // caller persists this same history). A no-op on a valid history, so
        // the request byte-shape is unchanged for every well-formed turn.
        // Deliberately mutates the CALLER's history, not the request's clone,
        // so a repair to an already-broken backlog is persisted, not re-derived
        // on every future turn.
        let repaired = reconcile_tool_results(history);
        if repaired > 0 {
            eprintln!(
                "[E-01] round {round_num}: reconciled {repaired} dangling tool_use block(s) \
                 with a skipped ack — history was invalid before this request"
            );
        }
        let mut request = MessagesRequest {
            model: model_for_round(round_num),
            max_tokens: MAX_OUTPUT_TOKENS,
            system: system.clone(),
            tools: tools_with_cache,
            messages: history.clone(),
        };
        // [E-01] the per-round network+model round-trip time, plus the request's
        // serialized byte size — reveals whether a multi-round tool loop is
        // re-transmitting the same base64 image every round (D-10 / RESEARCH §5.3)
        // and how many of MAX_ROUNDS a slow turn actually spent. Diagnostic-only:
        // logs a duration + byte count + stop_reason, never any message content
        // or key (threat T-14.3-05).
        let mut approx_bytes = serde_json::to_vec(&request).map(|v| v.len()).unwrap_or(0);
        // [bug/agent-history-413] the LAST-RESORT budget guard (see
        // MAX_REQUEST_BYTES's doc comment): a request THIS large is dominated by
        // base64 image data (nothing else in a Rudis request gets remotely this
        // big), so the one lever that can plausibly bring it back under the cap
        // is stripping every remaining image block. Mutates the CALLER's
        // `history` in place — same "the repair persists" discipline
        // `reconcile_tool_results` established — so a session that ever crosses
        // this guard stays stripped (and therefore small) on every future turn,
        // instead of re-inflating and re-tripping the guard again next message.
        if approx_bytes > MAX_REQUEST_BYTES {
            let stripped = prune_stale_images(history, 0);
            request.messages = history.clone();
            approx_bytes = serde_json::to_vec(&request).map(|v| v.len()).unwrap_or(0);
            eprintln!(
                "[E-01] round {round_num}: request exceeded the {MAX_REQUEST_BYTES}B budget guard \
                 -- stripped {stripped} image block(s) as a last resort, now request~{approx_bytes}B"
            );
            if approx_bytes > MAX_REQUEST_BYTES {
                // Stripping every image didn't help — the bloat isn't images.
                // Fail closed with an actionable message rather than let the
                // Anthropic API return its opaque, unrecoverable 413.
                return Err(LlmError::RequestTooLarge { bytes: approx_bytes });
            }
        }
        let t0 = std::time::Instant::now();
        let response = transport.send(&request).await?;
        eprintln!(
            "[E-01] round {round_num}: {:?} request~{approx_bytes}B stop={:?}",
            t0.elapsed(),
            response.stop_reason
        );

        // Record the assistant turn verbatim BEFORE acting on it (so the next
        // request's history contains this response and its tool_result reply).
        history.push(MessageParam {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        let round = apply_round(&response);
        narration = round.narration.or(narration);

        // [bug/agent-turn-no-response-max-tokens] Did THIS response get cut off
        // mid-write? Computed once here, before any of the three return paths, so
        // no exit can accidentally drop the fact on the floor.
        let truncated = response.stop_reason.as_deref() == Some(STOP_REASON_MAX_TOKENS);
        if truncated {
            eprintln!(
                "[E-01] round {round_num}: response TRUNCATED at the {MAX_OUTPUT_TOKENS}-token \
                 output ceiling -- the turn ends here with partial output (narration \
                 present={})",
                narration.is_some()
            );
        }

        // Check the halt BEFORE stop_reason: an askUser block ends the turn
        // immediately (no further transport.send()), regardless of stop_reason.
        if let Some(ask) = round.asked {
            return Ok(TurnOutcome {
                narration,
                clarifying_question: Some(ask.question),
                options: None,
                truncated,
            });
        }

        // A proposeOptions block ends the turn immediately too (checked in the
        // same pass — mutually exclusive with `asked` by construction, since
        // apply_response breaks on the FIRST control block it sees, T-14-10).
        if let Some((_, cards)) = round.options {
            return Ok(TurnOutcome {
                narration,
                clarifying_question: None,
                options: Some(cards),
                truncated,
            });
        }

        // Any stop_reason other than `tool_use` ends the turn. NOT all of them
        // are clean: `max_tokens` means the model was still writing, so the
        // outcome carries `truncated` and the caller/UI must say so instead of
        // presenting an empty turn as a finished one (the whole bug).
        if response.stop_reason.as_deref() != Some("tool_use") {
            return Ok(TurnOutcome {
                narration,
                clarifying_question: None,
                options: None,
                truncated,
            });
        }

        // Feed the applied tool_results back as the next user turn and loop.
        history.push(MessageParam {
            role: Role::User,
            content: round.tool_results,
        });
    }

    // Hit MAX_ROUNDS — terminate cleanly, surfacing whatever narration accrued.
    // `truncated: false` is correct here: every round that got this far ended on
    // `tool_use` (a COMPLETE response), so nothing was cut off mid-write. The
    // turn is unfinished for a different reason — the round budget — which the
    // loop has always reported by simply returning what it had.
    Ok(TurnOutcome {
        narration,
        clarifying_question: None,
        options: None,
        truncated: false,
    })
}

/// NEW, Phase-14-only sibling of `run_turn` (whose signature/behavior stay
/// byte-for-byte untouched). `dynamic_context` (retrieval examples + matched
/// skill playbooks, already rendered to text by the caller) becomes a SECOND
/// system block placed AFTER the cached rulebook block, deliberately carrying
/// NO `cache_control`: Anthropic's cache lookback is per-breakpoint and
/// backward-only, so content placed AFTER the rulebook's breakpoint never
/// invalidates its cache hit (14-RESEARCH.md's CITED finding).
///
/// `None` produces a request byte-shape-identical to `run_turn`'s.
pub async fn run_turn_with_context<T: LlmTransport>(
    transport: &T,
    rulebook: &str,
    dynamic_context: Option<&str>,
    tools: &[ToolDef],
    history: &mut Vec<MessageParam>,
    apply_round: impl FnMut(&MessagesResponse) -> RoundOutcome,
) -> Result<TurnOutcome, LlmError> {
    let mut system = vec![SystemBlock {
        kind: "text",
        text: rulebook.to_string(),
        cache_control: Some(CacheControl { kind: "ephemeral" }),
    }];
    if let Some(ctx) = dynamic_context {
        system.push(SystemBlock {
            kind: "text",
            text: ctx.to_string(),
            cache_control: None, // NEVER cached — changes every turn
        });
    }
    run_turn_inner(transport, system, |_round| tools.to_vec(), history, |_round| model_name(), apply_round).await
}

/// Phase 42 (ROUTE-01/02): sibling of `run_turn_with_context` that
/// additionally accepts a per-ROUND model-selector closure, called at the
/// SAME point `run_turn`/`run_turn_with_context` call `model_name()`
/// (`run_turn_inner`'s loop) -- so a routing decision that changes mid-turn
/// (an escalation flag mutated by the caller's `apply_round` closure,
/// ROUTE-02) can change the model used starting the VERY NEXT round, not
/// just once at turn start. Feeding `|_round| model_name()` reproduces
/// `run_turn_with_context`'s exact byte-shape (proven by
/// `with_model_closure_producing_model_names_matches_run_turn_with_context_request`
/// below).
///
/// Phase 42 (WR-01): also takes a per-ROUND `tools_for_round` selector (mirroring
/// `model_for_round`), so an escalated round can WIDEN the tool schema from the
/// cheap subset to the full `tool_defs()` set — not just switch the model.
/// Feeding `|_round| tools.to_vec()` reproduces the old fixed-tools behavior.
pub async fn run_turn_with_context_and_model<T: LlmTransport>(
    transport: &T,
    rulebook: &str,
    dynamic_context: Option<&str>,
    tools_for_round: impl FnMut(u32) -> Vec<ToolDef>,
    history: &mut Vec<MessageParam>,
    model_for_round: impl FnMut(u32) -> String,
    apply_round: impl FnMut(&MessagesResponse) -> RoundOutcome,
) -> Result<TurnOutcome, LlmError> {
    let mut system = vec![SystemBlock {
        kind: "text",
        text: rulebook.to_string(),
        cache_control: Some(CacheControl { kind: "ephemeral" }),
    }];
    if let Some(ctx) = dynamic_context {
        system.push(SystemBlock {
            kind: "text",
            text: ctx.to_string(),
            cache_control: None,
        });
    }
    run_turn_inner(transport, system, tools_for_round, history, model_for_round, apply_round).await
}

/// Resolve+dispatch every `tool_use` block in `response` against `store`, IN
/// ORDER, stopping at (not processing past) the first CONTROL block
/// (`askUser` OR `proposeOptions`) seen.
///
/// - `Text` blocks accumulate into `narration` (newline-joined).
/// - `askUser` sets `asked` (with the block's `tool_use_id`) and `break`s — no
///   later block is EXECUTED, and NO `tool_result` is minted for the `askUser`
///   block itself (the caller mints that when the user answers, Pitfall 5).
/// - `proposeOptions` sets `options` (tool_use_id + deserialized cards;
///   malformed input degrades to zero cards, T-14-08) and `break`s exactly the
///   same way — first-control-block-wins means at most ONE of
///   `asked`/`options` is ever `Some` per round (T-14-10).
/// - EVERY `tool_use` block positioned AFTER that control block is ABANDONED
///   (never executed) but STILL ACKED with a benign `is_error` "skipped"
///   `tool_result` — see `HALT_SKIP_ACK`. This is not cosmetic: the caller
///   records the assistant message in history VERBATIM, so the Anthropic API
///   requires a matching `tool_result` for every `tool_use` id it carries. A
///   missing one makes the conversation permanently invalid (every subsequent
///   request 400s with "`tool_use` ids were found without `tool_result`
///   blocks"), which no later turn can repair on its own. Each id therefore
///   gets EXACTLY ONE result: executed blocks get their real one, abandoned
///   blocks get the ack, the control block itself gets neither.
/// - `get_timeline` is handled inline: its `tool_result` is a fresh compact view
///   (using the block's own `selection` input if present, else `selection`).
/// - every other name goes through `parse_edit_tool` -> `resolve()` against the
///   CURRENT snapshot (so a later call in the SAME round sees an earlier call's
///   minted child id) -> `dispatch()` per resulting `Command`. A resolve/dispatch
///   error yields an `is_error: Some(true)` tool_result and does NOT abort the
///   rest of the round (matches `crates/agent-mcp`'s partial-failure precedent,
///   T-12-09).
///
/// Purely synchronous — no `.await` anywhere, so a caller may hold a
/// `std::sync::Mutex<Store>` lock around the whole call safely.
pub fn apply_response(
    store: &mut rudis_core::Store,
    response: &MessagesResponse,
    selection: &[String],
) -> RoundOutcome {
    let mut tool_results: Vec<ContentBlock> = Vec::new();
    let mut narration_parts: Vec<String> = Vec::new();
    let mut asked: Option<AskUser> = None;
    let mut patches: Vec<(rudis_core::Patch, u64, u64)> = Vec::new();
    let mut options: Option<(String, Vec<crate::cards::OptionCard>)> = None;
    let mut dispatched_calls: Vec<serde_json::Value> = Vec::new();
    // Index of the control block the round halted on, if any — everything
    // strictly after it is abandoned and gets acked below the loop.
    let mut halted_at: Option<usize> = None;

    for (block_index, block) in response.content.iter().enumerate() {
        match block {
            ContentBlock::Text { text } => narration_parts.push(text.clone()),
            ContentBlock::ToolUse { id, name, input } => {
                if name == "askUser" {
                    let question = input
                        .get("question")
                        .and_then(|q| q.as_str())
                        .unwrap_or("(missing question)")
                        .to_string();
                    asked = Some(AskUser {
                        tool_use_id: id.clone(),
                        question,
                    });
                    halted_at = Some(block_index);
                    break; // do NOT process anything after askUser this round
                }
                if name == "proposeOptions" {
                    // Malformed input degrades to zero cards, never panics
                    // (T-14-08, the askUser unwrap_or precedent).
                    let cards: Vec<crate::cards::OptionCard> =
                        serde_json::from_value(input["options"].clone()).unwrap_or_default();
                    options = Some((id.clone(), cards));
                    halted_at = Some(block_index);
                    break; // halts the turn exactly like askUser — nothing
                           // after this block is processed (T-14-10)
                }
                if name == "get_timeline" {
                    let sel: Vec<String> = input
                        .get("selection")
                        .and_then(|s| s.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_else(|| selection.to_vec());
                    let view = rudis_core::agent_state::view(&store.snapshot(), &sel);
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: crate::vision::text_tool_result(
                            rudis_core::agent_state::render_compact(&view),
                        ),
                        is_error: None,
                    });
                    continue;
                }
                // get_media (Phase 26, TOOL-04): pure read of Project.media_bin/
                // media_folders, no app_data_dir/engine dependency -- special-cased
                // inline exactly like get_timeline (NOT a src-tauri Pattern-C
                // interception), so it works identically on BOTH the in-app surface
                // and agent-mcp's dev harness.
                if name == "get_media" {
                    let view = rudis_core::agent_state::media_view(&store.snapshot());
                    let json = serde_json::to_string(&view).unwrap_or_else(|_| "{}".to_string());
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: crate::vision::text_tool_result(json),
                        is_error: None,
                    });
                    continue;
                }
                // read_skill (Phase 17-03, TOOL-05): progressive-disclosure
                // skill loading, special-cased like get_timeline (no engine/
                // session/filesystem dep — skill bodies are compiled in). A
                // missing/malformed skillId degrades to the not-found text via
                // unwrap_or (T-17-09: benign tool_result, never a panic); the
                // round continues, is_error stays None.
                if name == "read_skill" {
                    let skill_id = input.get("skillId").and_then(|v| v.as_str()).unwrap_or("");
                    let body = crate::skills::skill_body(skill_id)
                        .unwrap_or("(no skill with that id)");
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: crate::vision::text_tool_result(body),
                        is_error: None,
                    });
                    continue;
                }
                // undo (Phase 17-03, TOOL-07): a thin wrapper over
                // Store::undo(). SUBTLE: inside an open turn this pops the
                // last COMMITTED entry, NOT the in-progress turn's own edits
                // (documented in the tool's schema description). An empty
                // stack reports "nothing to undo" — never an error. It is
                // deliberately NOT recorded as a dispatched_call (undo is not
                // a library-growth edit).
                //
                // Its PATCH, however, MUST ride `patches`. `Store::undo`
                // mutates the project and bumps `Store::seq` exactly like a
                // dispatch, and `patches` is the only channel through which
                // `app_core::agent_turn` emits `project:changed`. Discarding
                // the triple (`store.undo().is_some()`) made an agent-driven
                // undo a SILENT mutation — live-UAT bug
                // `retime-live-uat-frontend-mirror-undo-audio`: the renderer
                // mirror never applied the revert, `PreviewEditSeq` never
                // flushed the live preview, and the renderer's `lastAppliedSeq`
                // fell one behind the store's, so the next unrelated mutation
                // tripped main.ts's `base_seq !== lastAppliedSeq` full resync
                // and the true state snapped in at once (the "ghost clip").
                // The UI's own undo (`app_core::dispatch::undo_inner`) always
                // emitted; only this path did not.
                if name == "undo" {
                    let undone = match store.undo() {
                        Some(triple) => {
                            patches.push(triple);
                            true
                        }
                        // Empty stack: nothing changed and no seq was bumped,
                        // so emitting anything here would desynchronise the
                        // renderer's chain in the OTHER direction.
                        None => false,
                    };
                    let msg = if undone {
                        "Reverted the last committed edit."
                    } else {
                        "Nothing to undo."
                    };
                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: crate::vision::text_tool_result(msg),
                        is_error: None,
                    });
                    continue;
                }
                // An edit tool: parse -> resolve -> dispatch.
                let (content, is_error) = match dispatch_edit(store, name, input, &mut patches) {
                    Ok(summary) => {
                        // Record the SUCCESSFUL call for library growth (Plan
                        // 14-06) — a failed call is not a "dispatched call".
                        dispatched_calls
                            .push(serde_json::json!({ "tool": name, "args": input }));
                        (summary, None)
                    }
                    Err(err) => (err, Some(true)),
                };
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: crate::vision::text_tool_result(content),
                    is_error,
                });
            }
            // An assistant response never carries tool_result blocks; ignore.
            ContentBlock::ToolResult { .. } => {}
            // Image blocks are USER-side content (the Phase 13 vision
            // snapshot, attached by the caller via
            // vision::image_content_block_png/_jpeg); an assistant response
            // never carries one. Ignore defensively.
            ContentBlock::Image { .. } => {}
            // Reasoning blocks (extended/interleaved thinking): not actionable for
            // the Store. run_turn echoes them back verbatim (signature preserved)
            // so multi-round tool turns stay valid; nothing to do here.
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {}
        }
    }

    // The halt ack (see this function's doc comment). Every `tool_use` block
    // AFTER the control block was abandoned unexecuted; each still needs
    // EXACTLY ONE `tool_result` or the recorded assistant message is
    // unanswerable and the whole conversation becomes permanently invalid.
    // `halted_at + 1` deliberately skips the control block itself (the caller
    // mints that one on resume) and every earlier block already has its real
    // result, so no id is ever acked twice.
    if let Some(halt_index) = halted_at {
        for block in &response.content[halt_index + 1..] {
            if let ContentBlock::ToolUse { id, .. } = block {
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: crate::vision::text_tool_result(HALT_SKIP_ACK),
                    is_error: Some(true),
                });
            }
        }
    }

    let narration = if narration_parts.is_empty() {
        None
    } else {
        Some(narration_parts.join("\n"))
    };

    RoundOutcome {
        tool_results,
        narration,
        asked,
        patches,
        options,
        dispatched_calls,
    }
}

/// Make `history` structurally valid for the Anthropic Messages API by
/// guaranteeing the one invariant a conversation can never recover from on its
/// own: **every `tool_use` block in an assistant message has exactly one
/// matching `tool_result` in the immediately following user message.**
///
/// Pure and total — no I/O, no `.await`, no panics — so it is directly unit
/// testable and safe to call under a lock. Returns how many `tool_result`
/// blocks it had to synthesize (0 on an already-valid history, which it leaves
/// BYTE-IDENTICAL: the existing request-shape tests depend on that no-op).
///
/// This is the second, defence-in-depth layer under `apply_response`'s halt ack.
/// `apply_response` fixes the known CAUSE; this makes the whole class of bug
/// unrepresentable at the point of no return (the outgoing request), no matter
/// which surface — src-tauri's Pattern-C interception, a resumed
/// `askUser`/`proposeOptions` answer, an aborted turn, a future caller —
/// dropped a result.
///
/// Crucially it also REPAIRS a conversation that is ALREADY broken. A session
/// that has recorded a dangling `tool_use` fails EVERY subsequent request with
/// `messages.N: tool_use ids were found without tool_result blocks` at a fixed,
/// historical index N; because `run_turn_inner` calls this on the caller's
/// `&mut` history, the repair is both sent AND persisted by the caller, so the
/// session un-bricks itself on the next message instead of staying dead.
///
/// It therefore scans EVERY assistant message, not just the last one — the
/// offending pair is typically deep in the backlog (the observed failure was at
/// `messages.86`), so a last-message-only check would send the same invalid
/// request forever.
///
/// Three shapes are repaired:
/// - next message is a User message missing some ids -> the missing results are
///   inserted after the last existing `tool_result` (Anthropic requires
///   `tool_result` blocks to lead the message, ahead of any text/image).
/// - the assistant message is LAST, or is followed by another Assistant message
///   -> a fresh User message carrying the results is inserted right after it.
/// - ids already answered are left alone, so no id is ever answered twice.
pub fn reconcile_tool_results(history: &mut Vec<MessageParam>) -> usize {
    let mut synthesized = 0usize;
    let mut index = 0usize;
    while index < history.len() {
        if !matches!(history[index].role, Role::Assistant) {
            index += 1;
            continue;
        }
        let pending: Vec<String> = history[index]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        if pending.is_empty() {
            index += 1;
            continue;
        }

        // Does a User message follow that can carry the answers?
        let reply_index = index + 1;
        let has_user_reply = history
            .get(reply_index)
            .is_some_and(|m| matches!(m.role, Role::User));
        if !has_user_reply {
            history.insert(
                reply_index,
                MessageParam {
                    role: Role::User,
                    content: Vec::new(),
                },
            );
        }
        let reply = &mut history[reply_index];
        let answered: std::collections::HashSet<&str> = reply
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<String> = pending
            .iter()
            .filter(|id| !answered.contains(id.as_str()))
            .cloned()
            .collect();
        // `answered` borrows `reply`; drop it before mutating.
        drop(answered);

        if missing.is_empty() {
            // Nothing to add. If an empty User message was just inserted (an
            // assistant message whose every id was somehow already answered
            // cannot happen here, but stay total), remove it again so a valid
            // history is left byte-identical.
            if !has_user_reply {
                history.remove(reply_index);
            }
            index = reply_index + 1;
            continue;
        }

        // Anthropic requires `tool_result` blocks to LEAD the user message, so
        // the synthesized ones slot in right after the last existing result
        // (index 0 when there is none) — never after a text/image block.
        let base = reply
            .content
            .iter()
            .rposition(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .map_or(0, |p| p + 1);
        synthesized += missing.len();
        for (offset, id) in missing.into_iter().enumerate() {
            reply.content.insert(
                base + offset,
                ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: crate::vision::text_tool_result(HALT_SKIP_ACK),
                    is_error: Some(true),
                },
            );
        }
        index = reply_index + 1;
    }
    synthesized
}

/// [bug/agent-history-413] the short, honest text a pruned image is replaced
/// with — never silently vanishes (the model should know something WAS there
/// and is now stale, not that this turn never had one), never pretends to be
/// real image content the model can still reason from.
const STALE_IMAGE_STUB: &str =
    "[earlier canvas/frame snapshot omitted -- stale state, no longer current]";

/// [bug/agent-history-413] Replace every image block in `history` OLDER than
/// the newest `keep` real images with [`STALE_IMAGE_STUB`], covering BOTH
/// shapes an image can take on the wire: a top-level `ContentBlock::Image` (a
/// user-turn vision/whiteboard snapshot, `vision::image_content_block_png`/
/// `_jpeg`) and a nested `ToolResultBlock::Image` (an earlier
/// `inspect_timeline`/`inspect_media` result's JPEG, `vision::image_tool_result`/
/// `images_tool_result`). Returns how many were stubbed — `0` on a history
/// already at or under `keep` images, which it leaves BYTE-IDENTICAL (mirrors
/// `reconcile_tool_results`'s own no-op contract).
///
/// WHY: `AgentSession.history` (src-tauri) is never pruned by turn count, and
/// every request re-sends the WHOLE history verbatim (this module's own
/// `messages: history.clone()` above) — so an uncapped run of canvas/vision
/// turns grows the persisted, always-resent history without bound, until the
/// Anthropic Messages API's 32MB HTTP request-body cap rejects EVERY future
/// request with an unrecoverable `413 request_too_large` (independent of any
/// token/context budget — base64 images are the only thing large enough in a
/// Rudis request to reach it). A stale snapshot is also stale STATE (the
/// canvas/frame may have changed since it was captured), so a model still
/// reasoning from it is a correctness risk, not just a size one — pruning old
/// images is a quality improvement as much as a size one.
///
/// Mirrors `reconcile_tool_results`'s shape exactly: pure and total (no I/O,
/// no panics, directly unit-testable), mutates the CALLER's `history` in
/// place so the repair PERSISTS (the caller writes this same `history` back
/// into `AgentSession`) instead of being re-derived every future turn. NEVER
/// removes or reorders a `ToolUse`/`ToolResult` BLOCK, and never touches a
/// `ToolResult`'s `tool_use_id`/`is_error` — only an `Image` block INSIDE one
/// is replaced with a `Text` block at the SAME position, so the
/// tool_use/tool_result pairing invariant `reconcile_tool_results` guards is
/// untouched (proven by this module's own tests: `reconcile_tool_results`
/// finds nothing new to repair after this runs).
///
/// "Newest" is GLOBAL document order across the whole history, not per
/// message — a turn that attaches BOTH a frame AND a whiteboard snapshot
/// (Phase 13/14.2, src-tauri's `image_blocks`) pushes 2 images at once, so a
/// caller wanting to guarantee the CURRENT turn's own fresh snapshot(s)
/// survive must pass `keep >= 2`.
pub fn prune_stale_images(history: &mut [MessageParam], keep: usize) -> usize {
    let total: usize = history
        .iter()
        .flat_map(|m| m.content.iter())
        .map(image_blocks_in)
        .sum();
    if total <= keep {
        return 0;
    }
    // The first `prune_before` occurrences, in document order, are the
    // OLDEST — stubbing those and leaving the rest keeps the newest `keep`.
    let prune_before = total - keep;
    let mut seen = 0usize;
    let mut pruned = 0usize;
    for message in history.iter_mut() {
        for block in message.content.iter_mut() {
            match block {
                ContentBlock::Image { .. } => {
                    if seen < prune_before {
                        *block = ContentBlock::Text {
                            text: STALE_IMAGE_STUB.to_string(),
                        };
                        pruned += 1;
                    }
                    seen += 1;
                }
                ContentBlock::ToolResult { content, .. } => {
                    for inner in content.iter_mut() {
                        if matches!(inner, ToolResultBlock::Image { .. }) {
                            if seen < prune_before {
                                *inner = ToolResultBlock::Text {
                                    text: STALE_IMAGE_STUB.to_string(),
                                };
                                pruned += 1;
                            }
                            seen += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    pruned
}

/// How many image blocks a single `ContentBlock` carries — `0` or `1` for
/// every variant except `ToolResult`, which may carry several
/// (`images_tool_result`'s storyboard mode, Phase 21/EYES-02).
fn image_blocks_in(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Image { .. } => 1,
        ContentBlock::ToolResult { content, .. } => content
            .iter()
            .filter(|b| matches!(b, ToolResultBlock::Image { .. }))
            .count(),
        _ => 0,
    }
}

/// Parse+resolve+dispatch a single edit `tool_use`, appending every dispatched
/// `Patch` to `patches`. Returns a `TimelineDelta`-shaped JSON summary (the
/// changed clips' NEW field values, computed via
/// `rudis_core::agent_state::delta(&before, &after)` over the pre/post-dispatch
/// snapshots — TOOL-01: the agent patches its own world-model from this
/// `tool_result` without re-reading) on success, or the error's `Display` text
/// on any failure (parse/resolve/dispatch) — never panics, never aborts the
/// caller's loop.
///
/// The `patches` OUTPUT parameter (RoundOutcome.patches, consumed by
/// library-growth/UI code) is UNRELATED to the returned content string and is
/// populated exactly as before.
///
/// `pub(crate)` (not private) so `crate::cards::apply_option_card` can reuse
/// the EXACT SAME resolve→dispatch→collect-patches path every edit `tool_use`
/// already goes through (T-14-09: a card grants no new privilege surface).
pub(crate) fn dispatch_edit(
    store: &mut rudis_core::Store,
    name: &str,
    input: &serde_json::Value,
    patches: &mut Vec<(rudis_core::Patch, u64, u64)>,
) -> Result<String, String> {
    let tool = parse_edit_tool(name, input.clone()).map_err(|e| e.to_string())?;
    // T-16-07: `before` is captured BEFORE resolve/dispatch, `after` AFTER the
    // full per-call command loop — the delta the model reads reflects the whole
    // call's effect (resolve still runs against the PRE-dispatch snapshot,
    // exactly as before).
    let before = store.snapshot();
    let commands = tool.resolve(&before).map_err(|e| e.to_string())?;
    let mut block_patches: Vec<(rudis_core::Patch, u64, u64)> = Vec::new();
    for cmd in commands {
        // Phase 43 (LAT-02): the (base_seq, seq) pair rides along with the
        // patch it describes, captured inside `dispatch`'s own body.
        let (patch, base_seq, seq) = store.dispatch(cmd).map_err(|e| e.to_string())?;
        block_patches.push((patch, base_seq, seq));
    }
    let after = store.snapshot();
    let delta = rudis_core::agent_state::delta(&before, &after);
    let summary = serde_json::to_string(&delta)
        .unwrap_or_else(|_| format!("{name} applied ({} patches)", block_patches.len()));
    patches.extend(block_patches);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::FixtureTransport;
    use crate::transport::{ToolResultBlock, Usage};
    use rudis_core::Store;
    use serde_json::json;

    /// A minimal one-clip, 30fps project (mirrors the fixture conventions used
    /// across `crates/core/tests` — a real-looking path, no real decoded file).
    fn seed_store() -> Store {
        let project: rudis_core::Project = serde_json::from_value(json!({
            "media_bin": [
                { "id": "media-1", "path": "test-media/bars_720p30_5s.mp4", "media_kind": "video", "duration_us": 5_000_000, "width": 1280, "height": 720, "fps": 30.0, "is_vfr": false, "rotation_degrees": 0, "has_audio": true, "poster_path": null }
            ],
            "timeline": { "tracks": [
                { "kind": "video", "clips": [
                    { "id": "clip-1", "media_id": "media-1", "start_us": 0, "in_us": 0, "out_us": 2_000_000, "volume": 1.0, "audio_detached": false }
                ] },
                { "kind": "audio", "clips": [] }
            ] }
        }))
        .expect("seed project deserializes");
        Store::from_project(project)
    }

    fn resp(content: Vec<ContentBlock>, stop_reason: &str) -> MessagesResponse {
        MessagesResponse {
            id: "msg_test".to_string(),
            role: Role::Assistant,
            content,
            stop_reason: Some(stop_reason.to_string()),
            usage: Usage::default(),
        }
    }

    fn tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text { text: t.to_string() }
    }

    /// Seed history with the opening user message a real turn would carry.
    fn seed_history() -> Vec<MessageParam> {
        vec![MessageParam {
            role: Role::User,
            content: vec![text("do the thing")],
        }]
    }

    #[test]
    fn single_round_tool_use_dispatches_real_command_and_ends_turn() {
        let mut store = seed_store();
        let transport = FixtureTransport::new(vec![
            resp(
                vec![tool_use(
                    "tu-1",
                    "placeClip",
                    json!({ "clipId": "clip-2", "mediaId": "media-1", "track": "v1", "startFrame": 90 }),
                )],
                "tool_use",
            ),
            resp(vec![text("Placed a second clip on the video track.")], "end_turn"),
        ]);
        let mut history = seed_history();

        let outcome = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok");

        // One real Command dispatched: a new clip exists in the live store.
        let snap = store.snapshot();
        let video_clips = &snap.timeline.tracks[0].clips;
        assert_eq!(video_clips.len(), 2, "placeClip added a real clip");
        assert!(video_clips.iter().any(|c| c.id == "clip-2"));

        assert!(outcome.narration.is_some(), "final text became narration");
        assert!(outcome.clarifying_question.is_none());

        // Exactly two round-trips; the SECOND request carries round-1's tool_result.
        let seen = transport.requests_seen();
        assert_eq!(seen.len(), 2, "one tool round + one closing round");
        let second_has_tool_result = seen[1]
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
        assert!(second_has_tool_result, "round-2 request replays round-1 tool_result");
    }

    #[test]
    fn ask_user_halts_on_first_round_with_zero_edits() {
        let mut store = seed_store();
        // Only ONE response scripted — a second transport.send() would panic
        // "script exhausted", proving the loop did NOT continue past askUser.
        let transport = FixtureTransport::new(vec![resp(
            vec![tool_use("tu-ask", "askUser", json!({ "question": "Which clip?" }))],
            "tool_use",
        )]);
        let mut history = seed_history();

        let outcome = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok");

        assert_eq!(outcome.clarifying_question.as_deref(), Some("Which clip?"));
        assert_eq!(transport.requests_seen().len(), 1, "halted without a 2nd send");

        // apply_response mints ZERO tool_results for an askUser-only response.
        let mut store2 = seed_store();
        let round = apply_response(
            &mut store2,
            &resp(
                vec![tool_use("tu-ask", "askUser", json!({ "question": "Which clip?" }))],
                "tool_use",
            ),
            &[],
        );
        assert!(round.tool_results.is_empty(), "no edit tool ran");
        assert!(round.patches.is_empty(), "no command dispatched");
        assert!(round.asked.is_some());
    }

    #[test]
    fn ask_user_after_an_edit_in_the_same_response_still_dispatches_the_edit() {
        let mut store = seed_store();
        let round = apply_response(
            &mut store,
            &resp(
                vec![
                    tool_use(
                        "tu-edit",
                        "setClipVolume",
                        json!({ "clipId": "clip-1", "gainDb": -6.0206 }),
                    ),
                    tool_use("tu-ask", "askUser", json!({ "question": "Anything else?" })),
                ],
                "tool_use",
            ),
            &[],
        );

        // The edit BEFORE askUser was dispatched (real patch), but nothing after.
        assert_eq!(round.patches.len(), 1, "the pre-askUser edit was applied");
        assert_eq!(round.tool_results.len(), 1, "only the edit's tool_result");
        assert!(round.asked.is_some(), "the question was captured");
        assert_eq!(round.asked.as_ref().unwrap().tool_use_id, "tu-ask");

        // The volume change is real in the live store.
        let snap = store.snapshot();
        assert!((snap.timeline.tracks[0].clips[0].volume - 0.5).abs() < 1e-3);
    }

    /// LIVE-UAT REGRESSION (`retime-live-uat-frontend-mirror-undo-audio`).
    ///
    /// The `undo` tool MUTATES the project and BUMPS `Store::seq`, so it is a
    /// mutation like any other and its `(patch, base_seq, seq)` triple MUST
    /// reach `RoundOutcome.patches` — that vec is the ONLY channel by which
    /// `app_core::agent_turn` emits `project:changed`
    /// (`for (patch, base_seq, seq) in &round.patches { ctx.emit_patch(..) }`).
    ///
    /// Dropping it was a THREE-way silent failure, all observed live:
    ///   1. the renderer's mirror never applied the revert, so the UI kept
    ///      showing the pre-undo state ("undo didn't restore the speed");
    ///   2. `PreviewEditSeq::observe_patch` never fired, so the live preview
    ///      kept its pre-undo decode session and audio mix;
    ///   3. the renderer's `lastAppliedSeq` fell one behind the store's, so the
    ///      NEXT unrelated mutation tripped main.ts's `base_seq !== lastAppliedSeq`
    ///      resync and the true state snapped in all at once — the "ghost clip
    ///      that appeared when I deleted a different clip".
    ///
    /// The UI's own undo path (`app_core::dispatch::undo_inner`) has always
    /// emitted; only the agent's tool did not.
    #[test]
    fn the_undo_tool_surfaces_its_patch_and_seq_pair_so_the_mutation_is_observable() {
        let mut store = seed_store();
        // A real committed edit to revert. (Not inside begin_turn/end_turn:
        // a bare dispatch pushes its own 1-member undo group, exactly like the
        // committed turn the agent's undo pops in production.)
        let (_, _, seq_after_edit) = store
            .dispatch(rudis_core::Command::SetClipVolume {
                id: "clip-1".to_string(),
                volume: 0.25,
            })
            .expect("seed edit dispatches");

        let round = apply_response(
            &mut store,
            &resp(vec![tool_use("tu-undo", "undo", json!({}))], "tool_use"),
            &[],
        );

        // The revert really happened.
        let snap = store.snapshot();
        assert!(
            (snap.timeline.tracks[0].clips[0].volume - 1.0).abs() < 1e-6,
            "undo restored the original volume"
        );
        // The model still gets exactly one tool_result, and it is not an error.
        assert_eq!(round.tool_results.len(), 1, "one ack for the undo tool_use");

        // ...AND the mutation is OBSERVABLE by the app layer.
        assert_eq!(
            round.patches.len(),
            1,
            "the undo's patch must ride RoundOutcome.patches — it is the only \
             channel app_core::agent_turn emits project:changed through"
        );
        let (patch, base_seq, seq) = &round.patches[0];
        assert_eq!(
            patch.ids,
            vec!["clip-1".to_string()],
            "the patch names the reverted clip"
        );
        // The seq chain is UNBROKEN: this patch's base_seq is where the store
        // was after the seeded edit, and its seq is the store's value now. A
        // renderer applying these in order never sees a gap.
        assert_eq!(*base_seq, seq_after_edit, "base_seq continues the chain");
        assert_eq!(*seq, store.seq(), "seq is the store's post-undo value");
        assert_eq!(*seq, *base_seq + 1, "exactly one mutation was emitted");
    }

    /// The other half of the contract: an EMPTY undo stack changes nothing and
    /// bumps no seq, so it must emit NOTHING. An unconditional push here would
    /// desynchronise the renderer's chain in the opposite direction.
    #[test]
    fn an_empty_stack_undo_emits_no_patch_and_bumps_no_seq() {
        let mut store = seed_store();
        let before = store.seq();
        let round = apply_response(
            &mut store,
            &resp(vec![tool_use("tu-undo", "undo", json!({}))], "tool_use"),
            &[],
        );
        assert!(round.patches.is_empty(), "nothing to undo emits no patch");
        assert_eq!(store.seq(), before, "an empty-stack undo bumps no seq");
        assert_eq!(round.tool_results.len(), 1, "the model is still acked");
    }

    #[test]
    fn max_rounds_safety_cap_terminates_without_panicking() {
        let mut store = seed_store();
        // MAX_ROUNDS responses, EVERY one a trivial edit with stop_reason=tool_use.
        let script: Vec<MessagesResponse> = (0..MAX_ROUNDS)
            .map(|_| {
                resp(
                    vec![tool_use(
                        "tu-loop",
                        "setClipVolume",
                        json!({ "clipId": "clip-1", "gainDb": 0.0 }),
                    )],
                    "tool_use",
                )
            })
            .collect();
        let transport = FixtureTransport::new(script);
        let mut history = seed_history();

        let outcome = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn terminates cleanly at the cap");

        assert!(outcome.clarifying_question.is_none());
        assert_eq!(
            transport.requests_seen().len(),
            MAX_ROUNDS as usize,
            "exactly MAX_ROUNDS round-trips, no 21st send, no panic/infinite loop"
        );
    }

    /// Drive `run_turn_with_context` once with the given dynamic context over a
    /// fresh single-end_turn-response transport; return the ONE request built.
    fn one_context_request(dynamic_context: Option<&str>) -> MessagesRequest {
        let mut store = seed_store();
        let transport =
            FixtureTransport::new(vec![resp(vec![text("Done.")], "end_turn")]);
        let mut history = seed_history();
        pollster::block_on(run_turn_with_context(
            &transport,
            "RULEBOOK",
            dynamic_context,
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn_with_context ok");
        let seen = transport.requests_seen();
        assert_eq!(seen.len(), 1, "single end_turn response = one request");
        seen.into_iter().next().unwrap()
    }

    /// A well-formed `proposeOptions` input carrying 2 hand-authored cards.
    fn two_cards_input() -> serde_json::Value {
        json!({
            "options": [
                { "id": "opt-a", "label": "Speed ramp",
                  "rationale": "A quick speed ramp emphasizes the cut.",
                  "tool": "trimClip",
                  "args": { "clipId": "clip-1", "edge": "end", "toFrame": 45 } },
                { "id": "opt-b", "label": "Louder intro",
                  "rationale": "Boosting the intro's volume adds punch.",
                  "tool": "setClipVolume",
                  "args": { "clipId": "clip-1", "gainDb": 3.0 } }
            ]
        })
    }

    #[test]
    fn propose_options_halts_the_round_with_zero_edits() {
        // Mirrors ask_user_halts_on_first_round_with_zero_edits byte-for-byte
        // in style: a proposeOptions-only response mints NOTHING and captures
        // the cards.
        let mut store = seed_store();
        let round = apply_response(
            &mut store,
            &resp(
                vec![tool_use("tu-opt", "proposeOptions", two_cards_input())],
                "tool_use",
            ),
            &[],
        );
        assert!(round.tool_results.is_empty(), "no edit tool ran");
        assert!(round.patches.is_empty(), "no command dispatched");
        assert!(round.dispatched_calls.is_empty(), "an all-proposeOptions round records nothing");
        let (id, cards) = round.options.expect("options captured");
        assert_eq!(id, "tu-opt", "the block's tool_use_id is carried");
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].id, "opt-a");
        assert_eq!(cards[0].tool, "trimClip");
        assert_eq!(cards[1].tool, "setClipVolume");
    }

    #[test]
    fn propose_options_halts_run_turn_after_exactly_one_request() {
        let mut store = seed_store();
        // Only ONE response scripted — a second transport.send() would fail
        // "script exhausted", proving the loop did NOT continue past the halt.
        let transport = FixtureTransport::new(vec![resp(
            vec![tool_use("tu-opt", "proposeOptions", two_cards_input())],
            "tool_use",
        )]);
        let mut history = seed_history();

        let outcome = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok");

        assert!(outcome.clarifying_question.is_none());
        let cards = outcome.options.expect("cards surfaced to the caller");
        assert_eq!(cards.len(), 2);
        assert_eq!(transport.requests_seen().len(), 1, "halted without a 2nd send");
        assert_eq!(
            store.snapshot(),
            seed_store().snapshot(),
            "proposeOptions dispatches nothing"
        );
    }

    #[test]
    fn propose_options_after_an_edit_in_the_same_response_still_dispatches_the_edit() {
        let mut store = seed_store();
        let round = apply_response(
            &mut store,
            &resp(
                vec![
                    tool_use(
                        "tu-edit",
                        "setClipVolume",
                        json!({ "clipId": "clip-1", "gainDb": -6.0206 }),
                    ),
                    tool_use("tu-opt", "proposeOptions", two_cards_input()),
                ],
                "tool_use",
            ),
            &[],
        );

        assert_eq!(round.patches.len(), 1, "the pre-proposeOptions edit was applied");
        assert_eq!(round.tool_results.len(), 1, "only the edit's tool_result");
        assert!(round.options.is_some(), "the cards were captured");
        assert_eq!(round.dispatched_calls.len(), 1, "the successful edit was recorded");

        // The volume change is real in the live store.
        let snap = store.snapshot();
        assert!((snap.timeline.tracks[0].clips[0].volume - 0.5).abs() < 1e-3);
    }

    #[test]
    fn first_control_block_wins_for_both_orderings() {
        // T-14-10 / Pitfall 3: exactly one of asked/options is ever Some.
        // askUser FIRST, proposeOptions SECOND -> only asked.
        let mut store = seed_store();
        let round = apply_response(
            &mut store,
            &resp(
                vec![
                    tool_use("tu-ask", "askUser", json!({ "question": "Which clip?" })),
                    tool_use("tu-opt", "proposeOptions", two_cards_input()),
                ],
                "tool_use",
            ),
            &[],
        );
        assert!(round.asked.is_some(), "the first-seen control block wins");
        assert!(round.options.is_none(), "nothing after askUser is processed");
        assert!(round.dispatched_calls.is_empty(), "an all-control round records nothing");

        // proposeOptions FIRST, askUser SECOND -> only options.
        let mut store2 = seed_store();
        let round2 = apply_response(
            &mut store2,
            &resp(
                vec![
                    tool_use("tu-opt", "proposeOptions", two_cards_input()),
                    tool_use("tu-ask", "askUser", json!({ "question": "Which clip?" })),
                ],
                "tool_use",
            ),
            &[],
        );
        assert!(round2.options.is_some(), "the first-seen control block wins");
        assert!(round2.asked.is_none(), "nothing after proposeOptions is processed");
    }

    /// Regression for the debug session `agent-turn-dangling-tool-result-on-halt`.
    ///
    /// A model may emit a CONTROL block (`askUser`/`proposeOptions`) ALONGSIDE
    /// further tool calls in the SAME assistant message. That whole assistant
    /// message is recorded in history verbatim, so the Anthropic API requires a
    /// matching `tool_result` for EVERY `tool_use` id it carries. Halting must
    /// therefore still ACK the abandoned blocks (without executing them) —
    /// otherwise the conversation is permanently invalid and every later request
    /// 400s with "`tool_use` ids were found without `tool_result` blocks".
    #[test]
    fn halted_round_acks_every_abandoned_tool_use_block() {
        for (control_id, control) in [
            (
                "tu-opt",
                tool_use("tu-opt", "proposeOptions", two_cards_input()),
            ),
            (
                "tu-ask",
                tool_use("tu-ask", "askUser", json!({ "question": "Which region?" })),
            ),
        ] {
            let mut store = seed_store();
            let round = apply_response(
                &mut store,
                &resp(
                    vec![
                        control,
                        tool_use(
                            "tu-edit-1",
                            "setClipVolume",
                            json!({ "clipId": "clip-1", "gainDb": -6.0206 }),
                        ),
                        tool_use("tu-edit-2", "moveClip", json!({ "clipId": "clip-1", "toFrame": 30 })),
                    ],
                    "tool_use",
                ),
                &[],
            );

            // The halt invariant is UNCHANGED: nothing after the control block runs.
            assert!(
                round.patches.is_empty(),
                "{control_id}: nothing after the control block may dispatch"
            );
            assert!(
                round.dispatched_calls.is_empty(),
                "{control_id}: an abandoned call is never a dispatched call"
            );
            assert_eq!(
                store.snapshot(),
                seed_store().snapshot(),
                "{control_id}: the store is untouched by abandoned blocks"
            );

            let acked: Vec<&str> = round
                .tool_results
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    _ => None,
                })
                .collect();
            for id in ["tu-edit-1", "tu-edit-2"] {
                assert_eq!(
                    acked.iter().filter(|a| **a == id).count(),
                    1,
                    "{control_id}: abandoned block {id} needs EXACTLY one tool_result, got {acked:?}"
                );
            }
            // The control block's OWN result is minted by the caller when the
            // user answers (Pitfall 5) — acking it here would double-ack it.
            assert_eq!(
                acked.iter().filter(|a| **a == control_id).count(),
                0,
                "{control_id}: the control block is acked on resume, not here"
            );
            // Every ack is a benign is_error result, so the model retries the
            // call after answering instead of believing it succeeded.
            for tr in &round.tool_results {
                if let ContentBlock::ToolResult { is_error, .. } = tr {
                    assert_eq!(*is_error, Some(true), "{control_id}: an ack is an is_error result");
                }
            }
        }
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: crate::vision::text_tool_result("ok"),
            is_error: None,
        }
    }

    fn result_ids(msg: &MessageParam) -> Vec<&str> {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Layer 2 of the `agent-turn-dangling-tool-result-on-halt` fix: the
    /// reconciliation invariant REPAIRS a history that is ALREADY invalid —
    /// the state the user's live session is stuck in. 3 `tool_use` blocks
    /// answered by only 1 `tool_result` becomes 3 answered by 3.
    #[test]
    fn reconcile_repairs_an_already_invalid_history() {
        let mut history = vec![
            MessageParam {
                role: Role::User,
                content: vec![text("crop the clip into the lassoed region")],
            },
            MessageParam {
                role: Role::Assistant,
                content: vec![
                    text("Here are two ways to read that lasso."),
                    tool_use("tu-opt", "proposeOptions", two_cards_input()),
                    tool_use("tu-edit-1", "setClipVolume", json!({})),
                    tool_use("tu-edit-2", "moveClip", json!({})),
                ],
            },
            MessageParam {
                role: Role::User,
                content: vec![tool_result("tu-opt"), text("the first one")],
            },
        ];

        let repaired = reconcile_tool_results(&mut history);

        assert_eq!(repaired, 2, "exactly the two dangling ids were synthesized");
        assert_eq!(history.len(), 3, "no message was added — the reply already existed");
        assert_eq!(
            result_ids(&history[2]),
            vec!["tu-opt", "tu-edit-1", "tu-edit-2"],
            "every tool_use id is now answered, tool_results leading the message"
        );
        // The user's own text still trails the results (Anthropic's ordering rule).
        assert!(
            matches!(history[2].content.last(), Some(ContentBlock::Text { .. })),
            "text stays after the tool_results"
        );
        // Synthesized acks are is_error so the model retries rather than
        // assuming the abandoned calls landed.
        for block in &history[2].content {
            if let ContentBlock::ToolResult { tool_use_id, is_error, .. } = block {
                if tool_use_id != "tu-opt" {
                    assert_eq!(*is_error, Some(true), "{tool_use_id} ack must be is_error");
                }
            }
        }

        // Idempotent: a second pass changes nothing.
        let before = serde_json::to_string(&history).unwrap();
        assert_eq!(reconcile_tool_results(&mut history), 0, "second pass is a no-op");
        assert_eq!(serde_json::to_string(&history).unwrap(), before);
    }

    /// The invariant that matters is on the WIRE, not in a helper: a session
    /// already carrying the invalid pair (the live bug — Anthropic rejects it
    /// with `messages.N: tool_use ids were found without tool_result blocks`,
    /// forever, because the caller replays the same history every turn) must
    /// produce a VALID outgoing request on its very next message, and the
    /// caller's persisted history must come back repaired.
    #[test]
    fn a_broken_history_sends_a_valid_request_and_is_persisted_repaired() {
        let mut store = seed_store();
        let transport = FixtureTransport::new(vec![resp(vec![text("Sure.")], "end_turn")]);
        // The exact recorded shape of the reported failure: an assistant
        // message with a control block + 2 trailing edits, answered by only the
        // control block's result. Buried mid-history, NOT last (the report was
        // `messages.86`), so a last-message-only check would never see it.
        let mut history = vec![
            MessageParam {
                role: Role::User,
                content: vec![text("crop the clip into the lassoed region")],
            },
            MessageParam {
                role: Role::Assistant,
                content: vec![
                    tool_use("tu-opt", "proposeOptions", two_cards_input()),
                    tool_use("tu-dangle-1", "setClipVolume", json!({})),
                    tool_use("tu-dangle-2", "moveClip", json!({})),
                ],
            },
            MessageParam {
                role: Role::User,
                content: vec![tool_result("tu-opt"), text("try again")],
            },
        ];

        pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok");

        // Assert on what the transport ACTUALLY received.
        let sent = transport.requests_seen();
        let messages = &sent.first().expect("one request was sent").messages;
        for (i, msg) in messages.iter().enumerate() {
            let used: Vec<&str> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect();
            if used.is_empty() {
                continue;
            }
            let answered = messages
                .get(i + 1)
                .map(result_ids)
                .expect("a tool_use message is always followed by its answers");
            for id in used {
                assert!(
                    answered.contains(&id),
                    "request message {i}: tool_use {id} went out unanswered — \
                     this is the exact shape the API rejects"
                );
            }
        }

        // And the repair is in the CALLER's history, so it is persisted rather
        // than re-derived (and re-logged) on every future turn.
        assert_eq!(
            reconcile_tool_results(&mut history),
            0,
            "the caller's history came back already repaired"
        );
        assert!(
            result_ids(&history[2]).contains(&"tu-dangle-1"),
            "the synthesized acks landed in the persisted history"
        );
    }

    /// The no-op half of the invariant: a VALID history must come out
    /// byte-identical, or every request-shape assertion in this file (and the
    /// prompt-cache hit rate in production) would break.
    #[test]
    fn reconcile_is_a_byte_identical_no_op_on_a_valid_history() {
        let mut history = vec![
            MessageParam {
                role: Role::User,
                content: vec![text("trim it")],
            },
            MessageParam {
                role: Role::Assistant,
                content: vec![tool_use("tu-1", "trimClip", json!({}))],
            },
            MessageParam {
                role: Role::User,
                content: vec![tool_result("tu-1")],
            },
            MessageParam {
                role: Role::Assistant,
                content: vec![text("Done.")],
            },
        ];
        let before = serde_json::to_string(&history).unwrap();

        assert_eq!(reconcile_tool_results(&mut history), 0, "nothing to repair");
        assert_eq!(
            serde_json::to_string(&history).unwrap(),
            before,
            "a valid history is left byte-identical"
        );
    }

    /// A turn that died right after the assistant message was recorded leaves
    /// history ENDING on unanswered `tool_use` blocks. Reconciliation must
    /// insert the missing user reply rather than give up.
    #[test]
    fn reconcile_inserts_a_user_reply_when_the_assistant_message_is_last() {
        let mut history = vec![
            MessageParam {
                role: Role::User,
                content: vec![text("export it")],
            },
            MessageParam {
                role: Role::Assistant,
                content: vec![tool_use("tu-x", "export_project", json!({}))],
            },
        ];

        assert_eq!(reconcile_tool_results(&mut history), 1);
        assert_eq!(history.len(), 3, "a user reply was inserted");
        assert!(matches!(history[2].role, Role::User));
        assert_eq!(result_ids(&history[2]), vec!["tu-x"]);
    }

    /// End-to-end proof that the two layers compose: a halted round's acks ride
    /// through `run_turn` into the RESUMED request. Simulates what src-tauri
    /// does on resume — replay `round.tool_results`, append the control block's
    /// own answer — and asserts the outgoing request is API-valid.
    #[test]
    fn a_halted_round_then_resumed_produces_an_api_valid_request() {
        let mut store = seed_store();
        let halting = resp(
            vec![
                tool_use("tu-ask", "askUser", json!({ "question": "Which region?" })),
                tool_use("tu-edit-1", "setClipVolume", json!({ "clipId": "clip-1", "gainDb": -6.0206 })),
                tool_use("tu-edit-2", "moveClip", json!({ "clipId": "clip-1", "toFrame": 30 })),
            ],
            "tool_use",
        );
        let transport = FixtureTransport::new(vec![halting.clone()]);
        let mut history = seed_history();
        let mut captured: Vec<ContentBlock> = Vec::new();

        let outcome = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| {
                let round = apply_response(&mut store, response, &[]);
                captured = round.tool_results.clone();
                round
            },
        ))
        .expect("run_turn ok");
        assert!(outcome.clarifying_question.is_some(), "the turn halted");

        // src-tauri's resume shape: prior_tool_results, then the answer.
        captured.push(ContentBlock::ToolResult {
            tool_use_id: "tu-ask".to_string(),
            content: crate::vision::text_tool_result("the lassoed one"),
            is_error: None,
        });
        history.push(MessageParam {
            role: Role::User,
            content: captured,
        });

        // The resumed history must already be valid WITHOUT reconciliation —
        // proving layer 1 alone fixes the cause.
        let mut copy = history.clone();
        assert_eq!(
            reconcile_tool_results(&mut copy),
            0,
            "layer 1 already answered every tool_use id; layer 2 found nothing to repair"
        );

        let assistant = history
            .iter()
            .find(|m| matches!(m.role, Role::Assistant))
            .expect("the assistant message was recorded");
        let used: Vec<&str> = assistant
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let mut answered = result_ids(history.last().unwrap());
        answered.sort_unstable();
        let mut used_sorted = used.clone();
        used_sorted.sort_unstable();
        assert_eq!(used_sorted, answered, "every recorded tool_use id is answered exactly once");
    }

    #[test]
    fn dispatched_calls_records_only_successful_edits_in_order() {
        let mut store = seed_store();
        let round = apply_response(
            &mut store,
            &resp(
                vec![
                    tool_use("tu-1", "setClipVolume", json!({ "clipId": "clip-1", "gainDb": 0.0 })),
                    tool_use("tu-2", "removeClip", json!({ "clipId": "no-such-clip" })),
                    tool_use("tu-3", "moveClip", json!({ "clipId": "clip-1", "toFrame": 30 })),
                ],
                "tool_use",
            ),
            &[],
        );

        assert_eq!(round.tool_results.len(), 3, "every edit got a tool_result");
        assert_eq!(
            round.dispatched_calls.len(),
            2,
            "the failed removeClip is NOT a dispatched call (Plan 14-06 library growth)"
        );
        assert_eq!(
            round.dispatched_calls[0],
            json!({ "tool": "setClipVolume", "args": { "clipId": "clip-1", "gainDb": 0.0 } })
        );
        assert_eq!(
            round.dispatched_calls[1],
            json!({ "tool": "moveClip", "args": { "clipId": "clip-1", "toFrame": 30 } })
        );
    }

    #[test]
    fn dispatch_edit_tool_result_carries_the_changed_clips_new_value() {
        let mut store = seed_store();
        let mut patches = Vec::new();
        let content = dispatch_edit(
            &mut store,
            "moveClip",
            &json!({ "clipId": "clip-1", "toFrame": 30 }),
            &mut patches,
        )
        .expect("moveClip dispatches");

        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        let changed = parsed["changed_clips"].as_array().expect("changed_clips array");
        assert_eq!(changed.len(), 1, "exactly one clip changed: {parsed}");
        assert_eq!(changed[0]["id"], "clip-1");
        // 30fps clip, frame 30 -> start_us == 30 * frame_step_us(30) == 999_990.
        assert_eq!(
            changed[0]["start_us"],
            30 * rudis_core::frame_step_us(30.0),
            "delta must carry the NEW start_us, not the pre-move value: {parsed}"
        );
        assert!(parsed["removed_ids"].as_array().unwrap().is_empty());
        assert!(!patches.is_empty(), "the RoundOutcome.patches side-channel is still populated");
    }

    #[test]
    fn get_timeline_returns_full_real_backend_state() {
        let mut store = seed_store();
        let round = apply_response(
            &mut store,
            &resp(
                vec![tool_use("tu-gt", "get_timeline", json!({}))],
                "tool_use",
            ),
            &[],
        );
        assert_eq!(round.tool_results.len(), 1);
        match &round.tool_results[0] {
            ContentBlock::ToolResult { content, .. } => {
                let text = match &content[0] {
                    ToolResultBlock::Text { text } => text,
                    other => panic!("expected the first block to be Text, got {other:?}"),
                };
                assert!(text.contains("clip-1"), "full state must name the seeded clip: {text}");
                // Label-based track addressing: the state names tracks by
                // their UI gutter label (v1/a1), never a raw backend index.
                assert!(text.contains("Track v1"), "full state must show the track label: {text}");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn get_media_returns_real_media_bin_and_folders() {
        // get_media (Phase 26, TOOL-04) is handled inline in apply_response
        // exactly like get_timeline (pure Store read, no engine/app_data_dir
        // dependency). Prove it returns REAL, live Store state — a second media
        // item filed under a "broll" folder, both built through the undoable
        // command path — not a stub view.
        let mut store = seed_store();
        let broll: rudis_core::model::MediaBinItem = serde_json::from_value(json!({
            "id": "media-2",
            "path": "test-media/broll_1080p24_3s.mp4",
            "media_kind": "video",
            "duration_us": 3_000_000,
            "width": 1920,
            "height": 1080,
            "fps": 24.0,
            "is_vfr": false,
            "rotation_degrees": 0,
            "has_audio": false,
            "poster_path": null,
            "folder": "broll"
        }))
        .expect("broll media deserializes");
        store
            .dispatch(rudis_core::Command::CreateMediaFolder { path: "broll".into() })
            .expect("create broll folder");
        store
            .dispatch(rudis_core::Command::AddMediaBinItem(broll))
            .expect("add broll media");

        let round = apply_response(
            &mut store,
            &resp(vec![tool_use("tu-gm", "get_media", json!({}))], "tool_use"),
            &[],
        );
        assert_eq!(round.tool_results.len(), 1, "exactly one tool_result");
        let text = match &round.tool_results[0] {
            ContentBlock::ToolResult { content, is_error, .. } => {
                assert_ne!(*is_error, Some(true), "get_media must not be an error result");
                match &content[0] {
                    ToolResultBlock::Text { text } => text.clone(),
                    other => panic!("expected the first block to be Text, got {other:?}"),
                }
            }
            other => panic!("expected ToolResult, got {other:?}"),
        };
        let view: rudis_core::MediaLibraryView =
            serde_json::from_str(&text).expect("get_media result deserializes to a MediaLibraryView");
        assert!(
            view.items.iter().any(|i| i.id == "media-1"),
            "media library must contain the seeded media id: {view:?}"
        );
        assert!(
            view.items.iter().any(|i| i.id == "media-2"),
            "media library must contain the broll media id: {view:?}"
        );
        assert_eq!(
            view.folders,
            vec!["broll".to_string()],
            "folders must reflect the created virtual folder"
        );
    }

    #[test]
    fn with_context_rulebook_block_and_tools_are_byte_identical_across_contexts() {
        let req_a = one_context_request(Some("EXAMPLE A"));
        let req_b = one_context_request(Some("EXAMPLE B"));

        let sys_a = serde_json::to_value(&req_a.system).unwrap();
        let sys_b = serde_json::to_value(&req_b.system).unwrap();
        assert_eq!(
            sys_a[0], sys_b[0],
            "the CACHED rulebook block's serialized bytes are identical across \
             two different dynamic contexts (cache-safety by construction)"
        );
        assert!(
            sys_a[0].get("cache_control").is_some(),
            "the rulebook block still carries its cache breakpoint"
        );

        let tools_a = serde_json::to_value(&req_a.tools).unwrap();
        let tools_b = serde_json::to_value(&req_b.tools).unwrap();
        assert_eq!(tools_a, tools_b, "tools identical across contexts");
        let arr = tools_a.as_array().unwrap();
        assert!(
            arr.last().unwrap().get("cache_control").is_some(),
            "cache_control still on the LAST tool"
        );
        assert!(
            arr[..arr.len() - 1].iter().all(|t| t.get("cache_control").is_none()),
            "no non-last tool carries a cache_control key"
        );
    }

    #[test]
    fn with_context_dynamic_block_differs_and_carries_no_cache_control() {
        let req_a = one_context_request(Some("EXAMPLE A"));
        let req_b = one_context_request(Some("EXAMPLE B"));

        let sys_a = serde_json::to_value(&req_a.system).unwrap();
        let sys_b = serde_json::to_value(&req_b.system).unwrap();
        assert_eq!(sys_a.as_array().unwrap().len(), 2, "rulebook + dynamic block");
        assert_eq!(sys_b.as_array().unwrap().len(), 2);
        assert!(sys_a[1]["text"].as_str().unwrap().contains("EXAMPLE A"));
        assert!(sys_b[1]["text"].as_str().unwrap().contains("EXAMPLE B"));
        assert_ne!(sys_a[1], sys_b[1], "the dynamic blocks DIFFER per turn");
        // The exact assertion style Plan 12-01 established: the key must be
        // ABSENT from the serialized JSON, not present-as-null.
        assert!(
            sys_a[1].get("cache_control").is_none(),
            "the dynamic block carries NO cache_control key at all"
        );
        assert!(sys_b[1].get("cache_control").is_none());
    }

    #[test]
    fn with_context_none_produces_exactly_run_turns_single_block_shape() {
        // Baseline: what run_turn itself builds for the same rulebook/tools.
        let mut store = seed_store();
        let plain = FixtureTransport::new(vec![resp(vec![text("Done.")], "end_turn")]);
        let mut history = seed_history();
        pollster::block_on(run_turn(
            &plain,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok");
        let req_plain = plain.requests_seen().into_iter().next().unwrap();

        let req_none = one_context_request(None);
        assert_eq!(req_none.system.len(), 1, "no dynamic context = ONE system block");
        assert_eq!(
            serde_json::to_value(&req_none.system).unwrap(),
            serde_json::to_value(&req_plain.system).unwrap(),
            "byte-shape-identical to run_turn's own system vec"
        );
        assert_eq!(
            serde_json::to_value(&req_none.tools).unwrap(),
            serde_json::to_value(&req_plain.tools).unwrap()
        );
    }

    #[test]
    fn with_model_closure_producing_model_names_matches_run_turn_with_context_request() {
        let mut store = seed_store();
        let transport = FixtureTransport::new(vec![resp(vec![text("Done.")], "end_turn")]);
        let mut history = seed_history();
        pollster::block_on(run_turn_with_context_and_model(
            &transport,
            "RULEBOOK",
            None,
            |_round| crate::tools::tool_defs(),
            &mut history,
            |_round| model_name(),
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn_with_context_and_model ok");
        let seen = transport.requests_seen();
        let req_new = seen.into_iter().next().unwrap();

        let req_ctx = one_context_request(None);
        assert_eq!(
            serde_json::to_value(&req_new).unwrap(),
            serde_json::to_value(&req_ctx).unwrap(),
            "run_turn_with_context_and_model with |_round| model_name() must be byte-identical \
             to run_turn_with_context's own request"
        );
    }

    #[test]
    fn model_for_round_is_called_once_per_round_with_the_correct_round_number_and_its_result_lands_in_the_request() {
        let mut store = seed_store();
        let transport = FixtureTransport::new(vec![
            resp(
                vec![tool_use(
                    "tu-1",
                    "setClipVolume",
                    json!({ "clipId": "clip-1", "gainDb": 0.0 }),
                )],
                "tool_use",
            ),
            resp(vec![text("Done.")], "end_turn"),
        ]);
        let mut history = seed_history();
        let seen_rounds = std::cell::RefCell::new(Vec::<u32>::new());
        pollster::block_on(run_turn_with_context_and_model(
            &transport,
            "RULEBOOK",
            None,
            |_round| crate::tools::tool_defs(),
            &mut history,
            |round| {
                seen_rounds.borrow_mut().push(round);
                if round == 0 { "MODEL-A".to_string() } else { "MODEL-B".to_string() }
            },
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("ok");

        assert_eq!(*seen_rounds.borrow(), vec![0, 1], "round numbers must be 0-indexed and sequential");
        let seen = transport.requests_seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].model, "MODEL-A");
        assert_eq!(seen[1].model, "MODEL-B");
    }

    // -----------------------------------------------------------------------
    // [bug/agent-turn-no-response-max-tokens] a response cut off at the output
    // ceiling must NEVER read as a clean, empty turn.
    // -----------------------------------------------------------------------

    /// The EXACT reported bug, reproduced structurally: the model spends its
    /// whole output budget writing one giant tool-input block (the five-part Veo
    /// prompt) and gets cut off, so the response carries NO complete `Text`
    /// block and `stop_reason` is `max_tokens`. Pre-fix this returned
    /// `narration: None` with nothing to distinguish it from a finished turn,
    /// and the frontend printed the dead-end "(no response)".
    #[test]
    fn max_tokens_stop_reason_reports_truncated_not_a_clean_empty_turn() {
        let mut store = seed_store();
        // A single response: no Text block at all, truncated mid-write.
        let transport = FixtureTransport::new(vec![resp(
            vec![tool_use(
                "tu-1",
                "generate_ai_video",
                json!({ "prompt": "Cinematography: a drone rises over the crossin" }),
            )],
            STOP_REASON_MAX_TOKENS,
        )]);
        let mut history = seed_history();

        let outcome = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok");

        assert!(
            outcome.truncated,
            "a max_tokens stop_reason MUST surface as truncated -- otherwise the \
             caller cannot tell an incomplete turn from a finished one"
        );
        assert!(
            outcome.narration.is_none(),
            "this fixture deliberately carries no Text block -- the point is that \
             `truncated` is what makes the empty narration explicable"
        );
        assert!(outcome.clarifying_question.is_none());
        assert!(outcome.options.is_none());
    }

    /// The control case: an ordinary finished turn must NOT be flagged, or the
    /// UI would warn about truncation on every normal message.
    #[test]
    fn end_turn_stop_reason_is_never_reported_as_truncated() {
        let mut store = seed_store();
        let transport =
            FixtureTransport::new(vec![resp(vec![text("Trimmed the clip.")], "end_turn")]);
        let mut history = seed_history();

        let outcome = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok");

        assert!(!outcome.truncated, "a clean end_turn is not a truncation");
        assert!(outcome.narration.is_some());
    }

    /// The output ceiling actually reaches the wire — a raise that never leaves
    /// the constant would not fix anything.
    #[test]
    fn every_request_carries_the_raised_output_ceiling() {
        let mut store = seed_store();
        let transport =
            FixtureTransport::new(vec![resp(vec![text("done")], "end_turn")]);
        let mut history = seed_history();

        let _ = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok");

        let seen = transport.requests_seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].max_tokens, MAX_OUTPUT_TOKENS,
            "the request must carry MAX_OUTPUT_TOKENS, not a stale hardcoded 4096"
        );
        assert!(
            MAX_OUTPUT_TOKENS > 4096,
            "the ceiling must actually be higher than the value that truncated"
        );
    }

    #[test]
    fn turn_outcome_has_no_model_field() {
        let outcome = TurnOutcome {
            narration: Some("done".to_string()),
            clarifying_question: None,
            options: None,
            truncated: false,
        };
        let v = serde_json::to_value(&outcome).unwrap();
        let obj = v.as_object().expect("TurnOutcome serializes to a JSON object");
        for key in obj.keys() {
            assert!(
                !key.to_ascii_lowercase().contains("model"),
                "TurnOutcome must never carry a model-identity field (ROUTE-03) -- found key `{key}`"
            );
        }
        let expected: std::collections::BTreeSet<String> =
            ["narration", "clarifying_question", "options", "truncated"]
                .into_iter()
                .map(String::from)
                .collect();
        let actual: std::collections::BTreeSet<String> = obj.keys().cloned().collect();
        assert_eq!(
            actual, expected,
            "TurnOutcome's field set changed -- re-verify no model-identity field was added"
        );
    }

    // -----------------------------------------------------------------------
    // [bug/agent-history-413] `prune_stale_images` + the `run_turn_inner`
    // budget guard.
    // -----------------------------------------------------------------------

    #[test]
    fn prune_stale_images_is_a_byte_identical_no_op_when_at_or_under_keep() {
        let mut history = vec![
            MessageParam {
                role: Role::User,
                content: vec![crate::vision::image_content_block_png(b"one")],
            },
            MessageParam {
                role: Role::User,
                content: vec![text("no image here")],
            },
        ];
        let before = serde_json::to_string(&history).unwrap();
        assert_eq!(
            prune_stale_images(&mut history, 1),
            0,
            "exactly at the keep budget -- nothing to prune"
        );
        assert_eq!(
            serde_json::to_string(&history).unwrap(),
            before,
            "a history at/under keep is left byte-identical"
        );

        // Comfortably under keep, too.
        assert_eq!(prune_stale_images(&mut history, 5), 0);
    }

    #[test]
    fn prune_stale_images_stubs_the_oldest_top_level_images_keeping_the_newest_n() {
        let mut history = vec![
            MessageParam {
                role: Role::User,
                content: vec![crate::vision::image_content_block_png(b"oldest")],
            },
            MessageParam {
                role: Role::User,
                content: vec![crate::vision::image_content_block_png(b"middle")],
            },
            MessageParam {
                role: Role::User,
                content: vec![crate::vision::image_content_block_jpeg(b"newest")],
            },
        ];

        let pruned = prune_stale_images(&mut history, 1);
        assert_eq!(pruned, 2, "the 2 oldest of 3 images were stubbed, keeping the newest 1");

        for (i, label) in [(0, "oldest"), (1, "middle")] {
            match &history[i].content[0] {
                ContentBlock::Text { text } => {
                    assert_eq!(text, STALE_IMAGE_STUB, "{label} image must be replaced with the stub text")
                }
                other => panic!("expected {label} image stubbed to Text, got {other:?}"),
            }
        }
        assert!(
            matches!(history[2].content[0], ContentBlock::Image { .. }),
            "the newest image survives untouched"
        );
    }

    #[test]
    fn prune_stale_images_stubs_nested_tool_result_images_and_preserves_the_pairing_invariant() {
        let mut history = vec![
            MessageParam {
                role: Role::User,
                content: vec![text("look at this")],
            },
            MessageParam {
                role: Role::Assistant,
                content: vec![tool_use("tu-1", "inspect_timeline", json!({}))],
            },
            MessageParam {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tu-1".to_string(),
                    content: crate::vision::image_tool_result("visible clips: []", b"jpeg-bytes-old"),
                    is_error: None,
                }],
            },
            MessageParam {
                role: Role::User,
                content: vec![crate::vision::image_content_block_jpeg(b"fresh-snapshot")],
            },
        ];

        let pruned = prune_stale_images(&mut history, 1);
        assert_eq!(
            pruned, 1,
            "the older NESTED tool_result image was stubbed, the newer top-level image kept"
        );

        // The ToolResult block itself, its tool_use_id and is_error, are
        // untouched -- only the Image INSIDE its content array was replaced.
        match &history[2].content[0] {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                assert_eq!(tool_use_id, "tu-1");
                assert_eq!(*is_error, None);
                assert_eq!(content.len(), 2, "text summary untouched, image replaced -- same slot count");
                assert!(matches!(content[0], ToolResultBlock::Text { .. }), "the original text summary survives");
                match &content[1] {
                    ToolResultBlock::Text { text } => assert_eq!(text, STALE_IMAGE_STUB),
                    other => panic!("expected the nested image replaced with a Text stub, got {other:?}"),
                }
            }
            other => panic!("expected a ToolResult block, got {other:?}"),
        }
        assert!(
            matches!(history[3].content[0], ContentBlock::Image { .. }),
            "the newest top-level image survives"
        );

        // The pairing invariant `reconcile_tool_results` guards is untouched:
        // the tool_use/tool_result pair is still intact (nothing new to repair).
        assert_eq!(
            reconcile_tool_results(&mut history),
            0,
            "stubbing an image INSIDE a tool_result must never break tool_use/tool_result pairing"
        );
    }

    #[test]
    fn run_turn_strips_every_image_before_send_when_the_request_exceeds_the_budget_guard() {
        let mut store = seed_store();
        let transport = FixtureTransport::new(vec![resp(vec![text("Done.")], "end_turn")]);
        // Simulate a caller that never pruned at turn-start (e.g.
        // crates/agent-mcp's dev harness, which never calls
        // `prune_stale_images`) accumulating several oversized image blocks --
        // exactly what src-tauri's Layer 1 (JPEG, not PNG) + Layer 2
        // (turn-start `prune_stale_images`) exist to prevent in the first
        // place. Proves the LAST-RESORT backstop still holds even when
        // neither of those ran.
        let big = "A".repeat(6 * 1024 * 1024); // ~6MB of base64 payload per image
        let mut history = seed_history();
        for i in 0..6 {
            history.push(MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Image {
                    source: crate::transport::ImageSource {
                        kind: "base64",
                        media_type: "image/png",
                        data: big.clone(),
                    },
                }],
            });
            history.push(MessageParam {
                role: Role::Assistant,
                content: vec![text(&format!("ack {i}"))],
            });
        }
        // ~36MB of raw base64 alone -- comfortably over MAX_REQUEST_BYTES (28MiB).

        let outcome = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect("run_turn ok -- stripping brings the request back under budget");
        assert!(outcome.clarifying_question.is_none());

        let sent = transport.requests_seen();
        assert_eq!(sent.len(), 1, "the guard fixes the request in place -- no retry round needed");
        let bytes = serde_json::to_vec(&sent[0]).unwrap().len();
        assert!(
            bytes < MAX_REQUEST_BYTES,
            "the request actually SENT to the transport must be back under budget: {bytes}B"
        );

        // The caller's history came back stripped too (persisted, not
        // re-derived -- same discipline `reconcile_tool_results` established).
        let remaining_images = history
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert_eq!(
            remaining_images, 0,
            "the last-resort strip removed every image from the persisted history"
        );
    }

    #[test]
    fn run_turn_fails_with_request_too_large_when_a_fully_stripped_request_still_exceeds_the_budget() {
        let mut store = seed_store();
        // No responses scripted: transport.send() must NEVER be called.
        let transport = FixtureTransport::new(vec![]);
        let mut history = seed_history();
        // A single oversized TEXT block -- `prune_stale_images` only strips
        // IMAGES, so this bloat is untouched by the last-resort strip and the
        // guard must still fail closed rather than let an opaque 413 reach
        // the wire.
        history.push(MessageParam {
            role: Role::User,
            content: vec![text(&"A".repeat(30 * 1024 * 1024))],
        });

        let err = pollster::block_on(run_turn(
            &transport,
            "RULEBOOK",
            &crate::tools::tool_defs(),
            &mut history,
            |response| apply_response(&mut store, response, &[]),
        ))
        .expect_err("an oversized non-image request must fail closed, never reach transport.send()");
        assert!(
            matches!(err, LlmError::RequestTooLarge { .. }),
            "expected RequestTooLarge, got {err:?}"
        );
        assert!(
            transport.requests_seen().is_empty(),
            "the oversized request must never actually be sent"
        );
    }
}
