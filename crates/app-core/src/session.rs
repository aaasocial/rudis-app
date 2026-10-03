//! The Chat agent's conversation state (Phase 12+), relocated out of
//! `src-tauri/src/lib.rs` in plan 45-04 as a PURE TYPE RELOCATION.
//!
//! # Why this type moved BEFORE any function that touches it
//!
//! `app-core` cannot depend back on `src-tauri`, so every function that names
//! [`AgentSession`] and migrates into this crate needs the TYPE to already live
//! here. 45-RESEARCH.md grouped `run_inspect_timeline` / `run_transcribe` /
//! `run_get_transcript` / `run_search_media` as the "safest first batch, zero
//! `AppCtx` dependency" — true of `AppHandle`, false of `AgentSession`: each of
//! them takes `&Mutex<AgentSession>` and reads/writes its fields directly, as do
//! `handle_send_feedback` (45-10's batch) and `undo`/`undo_inner` (45-12's).
//! `AgentSession` is genuine connective tissue across several otherwise
//! unrelated batches, so it moves once, early, on its own.
//!
//! # T-45-02 — 45-04's `pub` widening, PARTIALLY closed at 45-14
//!
//! In `src-tauri` these six types had module-private fields (visible crate-wide,
//! since they sat at the crate root). Crossing a crate boundary mid-migration
//! meant code in BOTH crates had to reach them, so 45-04 widened **all 26 fields
//! plus the five companion structs** to `pub` and recorded the ledger by name in
//! `45-04-SUMMARY.md`, for 45-14 to close.
//!
//! **What 45-14 actually measured, and what it therefore could and could not
//! close.** After 45-13 landed `run_agent_turn` / `build_user_turn` /
//! `apply_option_card_inner`, **no `src-tauri` PRODUCTION code touches any field
//! of any of these six types — 0 sites.** But `#[cfg(test)]` code still does, at
//! 43 sites across `src-tauri/src/lib.rs` and `src-tauri/src/generation.rs`
//! (`confirmed_session`), and those tests build a `tauri::test::MockRuntime` app,
//! so they cannot migrate into this crate (45-05's standing rule, applied for the
//! tenth time). Per 45-14's own conditional — *"if ANY remaining touch is found,
//! do NOT re-privatize that specific field; note it as a genuine follow-up"* —
//! the close is per-field and measured:
//!
//! | Item | 45-14 | Why |
//! |---|---|---|
//! | [`AgentSession::recent_tool_calls`] | **`pub(crate)`** | 0 `src-tauri` references |
//! | [`AgentSession::inspect_timeline_cache`] | **`pub(crate)`** | 0 |
//! | [`AgentSession::transcript_cache`] | **`pub(crate)`** | 0 |
//! | `InspectTimelineCache` + its 4 fields | **`pub(crate)`** | the type name has 0 `src-tauri` references |
//! | `TranscriptCache` + its 4 fields | **`pub(crate)`** | ditto |
//! | `AgentSession::history` | stays `pub` | 4 `#[cfg(test)]` sites (seed / take / read) |
//! | `AgentSession::pending_ask_user` | stays `pub` | 11 |
//! | `AgentSession::pending_option_choice` | stays `pub` | 10 |
//! | `AgentSession::pending_growth` | stays `pub` | 11 |
//! | `AgentSession::last_error` | stays `pub` | 1 (a `send_feedback` gate seeds it) |
//! | `AgentSession::spend_approved_turn` | stays `pub` | 6, **all READS** — see D-45-14-01 |
//! | `PendingGrowth` + 2 fields | stays `pub` | `growth.path` / `growth.ids` read by `library_growth_gate` |
//! | `PendingAskUser` + 3 fields | stays `pub` | built by STRUCT LITERAL at 4 `#[cfg(test)]` sites |
//! | `PendingOptionChoice` + 4 fields | stays `pub` | built by STRUCT LITERAL at 3 `#[cfg(test)]` sites |
//!
//! **11 of 26 fields and 2 of 5 structs closed; 15 fields and 3 structs remain
//! `pub` with their exact consumers named.** The residue is logged as
//! **D-45-14-01** with the two candidate closure paths, and it is NOT dead debt:
//! `spend_approved_turn` in particular is read-only from outside this crate, so a
//! read-only accessor would close the field that matters most (T-42.3-12's
//! "nothing but a spend-confirmation resume can set this") at a cost of 6
//! call-site edits.
//!
//! **The re-privatized visibility is `pub(crate)`, not bare-private, and that is
//! forced.** 45-04's ledger describes the pre-move state as "module-private", but
//! that was module-private **at the `src-tauri` crate ROOT**, i.e. crate-visible.
//! These types now live in a non-root module, so a bare field would be reachable
//! only from `session.rs` itself — and `inspect.rs`, `transcribe.rs`,
//! `feedback.rs`, `dispatch.rs` and `agent_turn.rs` all touch them. `pub(crate)`
//! is the exact restoration of the pre-45-04 semantics.
//!
//! Nothing external can reach the residue meanwhile: `app-core` and `rudis-app`
//! are both first-party, same-repo, `publish = false` crates (T-45-02).

/// The Chat conversation state: the running message history plus the pending
/// `askUser` bookkeeping (Pitfall 5 — when a turn pauses on a clarifying
/// question, ALL already-applied tool_results for that round are persisted
/// alongside the `askUser` id, so the NEXT user message resumes with every
/// pending `tool_use` id resolved). Managed by Tauri, NOT part of the domain
/// `Store` — the timeline is backend-owned; the conversation is UI-adjacent.
#[derive(Default)]
pub struct AgentSession {
    pub history: Vec<agent_llm::MessageParam>,
    pub pending_ask_user: Option<PendingAskUser>,
    /// Phase 14 (CANV-02): server-held bookkeeping for a turn halted on
    /// `proposeOptions` — a SIBLING field beside `pending_ask_user`, per
    /// DECISIONS.md A2 (Option a), NOT a unified enum: `apply_response`
    /// breaks on the FIRST control block seen, so both pending at once is
    /// unreachable by construction (proven in agent-llm's own T-14-10 test).
    pub pending_option_choice: Option<PendingOptionChoice>,
    /// Phase 14 (MOAT-03, DECISIONS.md A1): the ONE just-appended growable
    /// library entry still inside its retraction window. Set when a completed
    /// turn that dispatched >=1 real edit is appended to the growable library;
    /// the IMMEDIATELY-next `undo` that pops that exact turn deletes the file
    /// (retract); ANY other next action — a different undo, or a new
    /// `run_agent_turn` — clears this field WITHOUT deleting (finalize).
    pub pending_growth: Option<PendingGrowth>,
    /// Phase 17-04 (D-08): a bounded rolling buffer of this session's recent
    /// successfully-dispatched `{tool,args}` calls (cap 15 — palmier Pattern 9),
    /// populated every round in the `apply_round` closure and read by the
    /// `send_feedback` meta tool when it writes a local diagnostic report. Never
    /// leaves the device (offline-core); never carries a key or secret (E-01).
    pub(crate) recent_tool_calls: std::collections::VecDeque<serde_json::Value>,
    /// Phase 17-04 (D-08): the text of the last real (`is_error: true`)
    /// tool_result this session produced, folded into the same `send_feedback`
    /// diagnostic report. `None` until an errored tool_result occurs.
    pub last_error: Option<String>,
    /// Phase 21 (EYES-01, Pitfall 3): the ONE-SLOT last-inspected-frame cache
    /// for `inspect_timeline`. An `Option` always has a `Default` regardless of
    /// its inner type, so `#[derive(Default)]` above needs no change.
    pub(crate) inspect_timeline_cache: Option<InspectTimelineCache>,
    /// Phase 42.3 (F): the per-turn ONE-SHOT spend approval for the paid
    /// `generate_ai_*` tools.
    ///
    /// Set in exactly ONE place — `build_user_turn`'s resume of a
    /// [`PendingAskUser`] whose `spend_confirmation` flag is true, i.e. the user
    /// answering the gate's own question. NOTHING else can set it: not tool
    /// input, not rulebook text, not model output, not IPC (T-42.3-12 —
    /// prompt-injected "the user already approved" is structurally inert).
    /// Cleared at the start of EVERY turn (see `run_agent_turn`), so it can only
    /// ever be true for the single turn it was granted for, and read by the
    /// dispatcher as the `approved` evidence
    /// [`generation::spend_confirmation_gate`] fails closed without.
    ///
    /// Scoped to the whole resumed TURN (not to one call) on purpose: CHECK-02/03
    /// allow exactly one automatic, silent self-check retry of a paid generation,
    /// and re-prompting for that sanctioned retry would turn a designed behavior
    /// into confirmation fatigue. `GENERATE_RETRY_CAP` still bounds the turn at 2
    /// paid calls per tool name.
    pub spend_approved_turn: bool,
    /// Phase 22 (TEXT-02/EYES-03): a small bounded LRU of transcripts keyed by
    /// MEDIA IDENTITY (`media_id` + the exact `[in_us, out_us)` window), NEVER by
    /// mutable timeline state. Transcription is seconds-expensive and its input
    /// is the media BYTES — an unrelated timeline edit does not change those, so
    /// (unlike `inspect_timeline_cache`'s `state_hash`) this deliberately does
    /// NOT invalidate on a timeline mutation (22-RESEARCH.md Don't-Hand-Roll /
    /// Anti-Patterns). A `Vec` has a `Default`, so `#[derive(Default)]` stands.
    pub(crate) transcript_cache: Vec<TranscriptCache>,
}

/// Phase 21 (EYES-01, Pitfall 3): the ONE-SLOT last-inspected-frame cache.
/// A hit requires BOTH `state_hash` (a hash of the whole Timeline +
/// project width/height/fps -- ANY edit anywhere changes it, deliberately
/// over-invalidating rather than risking a stale hit, SC-3) and the exact
/// same requested `position_us` to match.
pub(crate) struct InspectTimelineCache {
    pub(crate) state_hash: u64,
    pub(crate) position_us: i64,
    pub(crate) text: String,
    pub(crate) jpeg: Vec<u8>,
}

/// Phase 22 (TEXT-02/EYES-03): one cached transcript entry. The cache key is
/// MEDIA IDENTITY — `media_id` plus the exact requested `[in_us, out_us)`
/// window — and DELIBERATELY carries NO timeline `state_hash` (contrast
/// [`InspectTimelineCache`]): the transcript's input is the immutable media
/// bytes, so an unrelated timeline edit must never force a re-transcription
/// (22-RESEARCH.md Don't-Hand-Roll). A different window (e.g. after a real trim
/// that changed the clip's source range) is a distinct key and misses, which is
/// correct — the words no longer line up.
pub(crate) struct TranscriptCache {
    pub(crate) media_id: String,
    pub(crate) in_us: i64,
    pub(crate) out_us: i64,
    pub(crate) words: Vec<engine::whisper::Word>,
}

/// One appended-but-not-yet-finalized growable library entry (DECISIONS.md
/// A1): the file `append_library_entry` just wrote, plus every entity id the
/// turn's own dispatched patches touched — the overlap test the `undo`
/// command runs against the popped `Patch.ids` to decide retract vs finalize.
pub struct PendingGrowth {
    pub path: std::path::PathBuf,
    pub ids: Vec<String>,
}

/// A paused `askUser` exchange: the id of the `askUser` tool_use plus every
/// tool_result already computed in that round (edits applied BEFORE the
/// question), replayed tool_results-FIRST when the user answers.
pub struct PendingAskUser {
    pub tool_use_id: String,
    pub prior_tool_results: Vec<agent_llm::ContentBlock>,
    /// Phase 42.3 (F): `true` marks a halt SYNTHESIZED by the pre-spend
    /// confirmation gate rather than emitted by the model as a real `askUser`.
    /// The resume of such a halt grants the turn's [`AgentSession::spend_approved_turn`]
    /// one-shot.
    ///
    /// It ALSO selects the resume SHAPE, which is the whole point of the flag
    /// existing (gap closure plan 42.3-06; 42.3-HUMAN-UAT.md tests 1b/2, live
    /// 2026-07-28). A real `askUser` resume relays the user's answer verbatim as
    /// the halting call's tool_result, because there the answer genuinely IS
    /// that tool's result. Doing the same HERE was the defect: the halting id
    /// belongs to a `generate_ai_*` call that never executed, so a successful
    /// tool_result carrying "yes" told the model its paid call had already run —
    /// it reported an image that did not exist and never re-issued the call. A
    /// gate halt therefore resumes with [`SPEND_GATE_NOT_EXECUTED`]
    /// (`is_error: Some(true)`, a FIXED const — no user bytes, T-42.3-15) plus
    /// the verbatim reply as a trailing user-role Text block. See
    /// `build_user_turn`.
    ///
    /// The halt is otherwise INDISTINGUISHABLE from a model `askUser` and rides
    /// the identical machinery — `round.asked` -> this pending state ->
    /// `build_user_turn`'s resume, with `agent_llm::HALT_SKIP_ACK` covering the
    /// blocks skipped alongside it and `agent_llm::reconcile_tool_results`
    /// guarding the send. That is deliberate: debug session
    /// `agent-turn-dangling-tool-result-on-halt` established that a bespoke halt
    /// path is how a `tool_use` id goes unanswered forever (HTTP 400 at a fixed
    /// `messages.N` on every later request, since the assistant message is
    /// replayed verbatim). One difference matters and is handled at the CR-01 ack
    /// loop: the halting id HERE is a real `meta` entry, so it must be excluded
    /// from that loop or it would be answered twice.
    pub spend_confirmation: bool,
}

/// A paused `proposeOptions` exchange (CANV-02): the halted tool_use's id
/// plus the OFFERED cards, held SERVER-SIDE so `apply_option_card` can only
/// ever look a card up by id from what Claude actually proposed — the
/// frontend has NO path to pass a `{tool,args}` pair directly (T-14-13).
pub struct PendingOptionChoice {
    pub tool_use_id: String,
    pub cards: Vec<agent_llm::OptionCard>,
    /// `None` until a card is applied; `Some(summary)` after, threaded as
    /// the `proposeOptions` call's tool_result content on the NEXT turn.
    /// When still `None` at the next turn, `build_user_turn` synthesizes a
    /// fallback string so the API's "every tool_use needs a tool_result"
    /// invariant can never be violated by this mechanism (T-14-16).
    pub resolved_summary: Option<String>,
    /// Every OTHER tool_result computed in the halting round — the SAME field
    /// `PendingAskUser` has always carried, and for the same reason.
    ///
    /// Debug session `agent-turn-dangling-tool-result-on-halt`: this field did
    /// not exist, and a `proposeOptions` halt returns from `run_turn_inner`
    /// BEFORE `round.tool_results` is pushed into history — so on this path
    /// alone, results for blocks handled BEFORE the control block (a real edit,
    /// a `get_timeline` read) plus the halt acks for the blocks after it were
    /// dropped on the floor, leaving those `tool_use` ids permanently
    /// unanswered in the recorded assistant message. Replayed
    /// tool_results-FIRST on the next turn, exactly like the askUser branch.
    pub prior_tool_results: Vec<agent_llm::ContentBlock>,
}
