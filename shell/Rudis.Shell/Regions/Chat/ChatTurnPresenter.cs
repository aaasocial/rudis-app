using System.Text.Json;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE CHAT TRANSCRIPT'S DECISION SURFACE — a line-for-line port of v6.0's
// `sendChatMessage` post-await body (`frontend/src/main.ts:2214-2302`).
// ============================================================================
//
// WINUI-FREE BY RULE. This directory holds the `Chat` region's PURE half; see the
// contract comment in `Rudis.Shell.Tests.csproj` and the mechanical enforcement in
// `ChatPurityGateTests`. The WinUI-bearing half (`Regions/Chat.xaml{,.cs}`, plan
// 54-03) lives BESIDE this directory, never inside it — which is what lets every
// rule below be pinned by `ChatTests` in a plain net9.0-windows host with no
// window, no XAML compilation and no dispatcher.
//
// The wire shape parsed here is the RUST struct, not the TypeScript interface:
// `app_core::AgentTurnOutcome` (`crates/app-core/src/agent_turn.rs:253-264`),
// serialised by serde and handed back through `rudis_agent_send_message`'s
// `{"Ok": {..}}`. The TS names happen to coincide; the Rust ones are authority.
//
//   narration:               Option<String>            -> string | null
//   clarifying_question:     Option<String>            -> string | null
//   options:                 Option<Vec<OptionCard>>   -> null OR a (possibly empty) array
//   truncated:               bool                      -> never null
//   generation_disclosures:  Vec<GenerationDisclosure> -> always an array, never null
//   export_disclosures:      Vec<ExportDisclosure>     -> always an array, never null
//
// ---------------------------------------------------------------------------
// THE THREE D-17 NARRATION RULES THIS PORTS, AND THE ONE THING IT DELETES
// ---------------------------------------------------------------------------
//
//  * A clarifying question is surfaced ALONE, never merged with narration
//    (AGENT-06) — the turn PAUSED, and pasting narration onto the question reads
//    as though it continued.
//  * Truncation is NAMED. A truncated turn that produced text still gets the
//    caveat appended, or a half-written answer reads as a complete one; a
//    truncated turn with no text at all gets v6.0's replacement copy, never the
//    old dead-end string `(no response)` which could not be acted on.
//  * Errors surface VERBATIM in their own error-styled bubble (`PresentError`),
//    never swallowed.
//
// ...and D-08's deletion, documented at its exact site in `Present` below.
//
// D-18 IS TWO DIFFERENT MODEL AXES AND CONFLATING THEM IS THE TRAP. The
// generation disclosures built here (including `Model that ran (..): ..`) are the
// KEEP side and are REQUIRED. See the comment above the disclosure loop.

/// <summary>Which visual channel a transcript bubble belongs to. Mirrors v6.0's
/// four CSS classes (<c>chat-user</c> / plain agent / <c>chat-disclosure</c> /
/// <c>chat-error</c>, <c>frontend/index.html:368-490</c>). The styling itself is
/// plan 54-03's; this half only decides WHICH channel a line belongs to.</summary>
internal enum ChatBubbleKind
{
    /// <summary>What the user typed, echoed into the transcript on send.</summary>
    User,

    /// <summary>The turn's single reply line — narration, a clarifying question,
    /// or one of the three fallbacks.</summary>
    Agent,

    /// <summary>A STRUCTURAL generation disclosure. Never derived from narration
    /// text (T-p3q-01: nothing forces Claude to relay tool text verbatim, and live
    /// UAT proved it paraphrases), so it rides its own channel.</summary>
    Disclosure,

    /// <summary>A backend error, surfaced verbatim (D-17).</summary>
    Error,
}

/// <summary>One rendered transcript line. Model-authored text is DATA here and
/// stays data: plan 54-03 binds it to <c>TextBlock.Text</c> only, the WinUI
/// equivalent of v6.0's <c>textContent</c>-never-<c>innerHTML</c> rule
/// (<c>main.ts:2167</c>, T-14-20).</summary>
internal sealed record ChatBubble(ChatBubbleKind Kind, string Text);

/// <summary>
/// One agent-proposed option, as the transcript renders it (D-06: in Chat, not
/// Canvas).
///
/// <para><b>D-07 IS A TYPE-LEVEL PROPERTY HERE, not a convention.</b> The backing
/// <c>agent_llm::OptionCard</c> (<c>crates/agent-llm/src/cards.rs:22-34</c>) also
/// carries a <c>tool</c> name and an <c>args</c> object. Those are DISPLAY-ONLY and
/// the frontend must never re-interpret them (<c>main.ts:2093-2096</c>): applying a
/// card sends only its id, and the backend resolves the payload server-side from
/// <c>AgentSession.pending_option_choice</c> (T-14-13 / T-14-21), so a card can only
/// ever do what a normal <c>tool_use</c> could already do — no new privilege
/// surface. v6.0 never RENDERED them either (<c>main.ts:2489-2541</c> draws
/// ✦ + label + rationale + Apply and nothing else).</para>
///
/// <para>This record therefore has NO members for them and this port does not even
/// DESERIALIZE them: there is no code path that could re-interpret a value that was
/// never read. Adding a field here must re-justify itself against T-54-01 first —
/// <c>ChatTests.the_card_type_structurally_cannot_carry_a_tool_or_args</c> is the
/// case that fails if someone tries.</para>
/// </summary>
internal sealed record ChatOptionCard(string Id, string Label, string Rationale);

/// <summary>Everything one agent turn adds to the transcript.</summary>
/// <param name="Bubbles">The reply first, then this turn's disclosures in array
/// order.</param>
/// <param name="Cards">Empty unless <c>options</c> was a non-null, non-empty
/// array.</param>
/// <param name="ClearsCards">D-09: true on every NON-card turn, so stale cards
/// from a prior <c>proposeOptions</c> offer the user never acted on come off
/// screen. The backend has already replaced <c>pending_option_choice</c>, so those
/// ids would be rejected anyway — leaving them up offers a button that cannot
/// work.</param>
internal sealed record ChatTurnView(
    IReadOnlyList<ChatBubble> Bubbles,
    IReadOnlyList<ChatOptionCard> Cards,
    bool ClearsCards);

internal static class ChatTurnPresenter
{
    /// <summary>v6.0's neutral lead-in when a <c>proposeOptions</c> turn carries no
    /// text at all (<c>main.ts:2230</c>). KEPT — D-08 deletes only the pointer line
    /// that followed it.</summary>
    internal const string OptionsLeadIn = "Here are a couple of options:";

    /// <summary>The truncated-and-no-text copy (<c>main.ts:2232</c>), verbatim,
    /// double hyphens included.</summary>
    internal const string TruncatedNoTextReply =
        "I ran out of room mid-answer, so nothing came back as text. Anything I finished " +
        "before that still stands -- check the Timeline and MediaBin. Ask again, more " +
        "narrowly, and I'll pick it up.";

    /// <summary>The plain no-text copy (<c>main.ts:2233</c>), verbatim. It says what
    /// to do next, which is the whole reason it replaced the old dead-end
    /// string.</summary>
    internal const string NoTextReply =
        "The turn finished without saying anything. Nothing was changed -- try rephrasing " +
        "your request.";

    /// <summary>Appended to a truncated turn that DID produce narration
    /// (<c>main.ts:2240-2241</c>), verbatim. Note the leading space.</summary>
    internal const string TruncationCaveat =
        " (Cut off there -- I hit my output limit, so this answer is incomplete.)";

    /// <summary>
    /// Turn one <c>AgentTurnOutcome</c> — the INNER payload of
    /// <c>rudis_agent_send_message</c>'s <c>{"Ok": {..}}</c> — into the transcript
    /// lines and cards it produces.
    /// </summary>
    internal static ChatTurnView Present(JsonElement outcome)
    {
        var narration = OptionalString(outcome, "narration");
        var question = OptionalString(outcome, "clarifying_question");
        var truncated = Flag(outcome, "truncated");

        // 1. `Array.isArray(result.options) && result.options.length > 0`
        //    (main.ts:2218-2219). A PRESENT-but-EMPTY array is a NON-card turn, and
        //    the length check is the only thing that says so.
        var hasOptions =
            outcome.TryGetProperty("options", out var options)
            && options.ValueKind == JsonValueKind.Array
            && options.GetArrayLength() > 0;

        // 2. The reply ladder, at v6.0's exact precedence (main.ts:2226-2233). A
        //    clarifying question means the turn PAUSED, so it wins outright and is
        //    surfaced ALONE — narration is never merged into it (AGENT-06).
        var reply =
            question
            ?? narration
            ?? (hasOptions
                ? OptionsLeadIn
                : truncated
                    ? TruncatedNoTextReply
                    : NoTextReply);

        // 3. A truncated turn that DID produce text still needs the caveat, or a
        //    half-written answer reads as a complete one (main.ts:2239-2242). The
        //    `reply == narration` guard is what keeps it off the other three
        //    branches: a clarifying question is surfaced alone, and the two no-text
        //    fallbacks already say it themselves, so neither collects a second copy.
        if (truncated && narration is not null && reply == narration)
        {
            reply += TruncationCaveat;
        }

        // 4. D-08 DELETION, recorded at its exact site. v6.0 appended a pointer line
        //    here — "Check the Canvas panel to review and apply them." — to every
        //    card-bearing reply (main.ts:2247-2249), because CANV-02 rendered the
        //    cards in the CANVAS region and a reader watching only the Chat panel
        //    could miss them. D-06 moves the cards INTO this transcript, directly
        //    below this reply, so the line would now point at a panel that holds no
        //    cards: deleting it is required for CORRECTNESS, not a preference. This
        //    comment exists so a later parity diff reads a decision here rather than
        //    an omission — and `ChatTests.
        //    no_output_string_ever_points_the_user_at_the_canvas_panel` is what fails
        //    if a "parity fix" restores it.

        var bubbles = new List<ChatBubble> { new(ChatBubbleKind.Agent, reply) };

        // 5. The generation disclosures (main.ts:2257-2292), one STRUCTURAL bubble
        //    per line, in array order.
        //
        //    ⚠ THESE ARE D-18's KEEP SIDE. An executor reading SC-1's "no model
        //    badge, selector, or per-turn model identity anywhere in the UI" as
        //    covering `Model that ran (..)` has misread D-18: that clause is
        //    ROUTE-03, and it is about `agent-llm`'s Haiku-vs-Opus ROUTING TIER
        //    (crates/agent-llm/src/routing.rs) — a different model axis entirely,
        //    banned mechanically by `MechanicalGatesTests.
        //    no_agent_tier_identity_in_shell_sources`. The GENERATION model is the
        //    one the user is BILLED for, and Phase 42.1-03's rationale is exactly
        //    that: "the user is billed per model, so which one ran is not a detail,
        //    it is the price". `provider_notice` is additionally a GEN-08 sign-off
        //    condition. Deleting any of these four lines is a REGRESSION against
        //    v6.0, not parity.
        //
        //    Each rides its own structural channel rather than the narration,
        //    because nothing forces Claude to relay tool text verbatim and live UAT
        //    proved it paraphrases (T-p3q-01). The three optional fields render
        //    NOTHING when null, rather than a misleading empty or "text only" line.
        if (outcome.TryGetProperty("generation_disclosures", out var disclosures)
            && disclosures.ValueKind == JsonValueKind.Array)
        {
            foreach (var disclosure in disclosures.EnumerateArray())
            {
                var modality = OptionalString(disclosure, "modality") ?? string.Empty;

                bubbles.Add(new ChatBubble(
                    ChatBubbleKind.Disclosure,
                    $"Prompt sent to the provider ({modality}): " +
                    $"\"{OptionalString(disclosure, "prompt")}\""));

                if (OptionalString(disclosure, "frames") is { } frames)
                {
                    bubbles.Add(new ChatBubble(
                        ChatBubbleKind.Disclosure,
                        $"Frames conditioning it ({modality}): {frames}"));
                }

                if (OptionalString(disclosure, "model_resolved") is { } model)
                {
                    bubbles.Add(new ChatBubble(
                        ChatBubbleKind.Disclosure,
                        $"Model that ran ({modality}): {model}"));
                }

                if (OptionalString(disclosure, "provider_notice") is { } notice)
                {
                    bubbles.Add(new ChatBubble(ChatBubbleKind.Disclosure, notice));
                }
            }
        }

        // 5b. Export outcomes (debug session `export-no-file-written`,
        //     2026-08-01). The agent's `export_project` writes to a
        //     server-derived `app_data_dir/exports` path the user cannot see
        //     anywhere else in the UI, and nothing forces the model to narrate
        //     the destination (T-p3q-01 — the same reason the generation
        //     disclosures above ride a structural channel): three real,
        //     successful exports were reported as "export completes but no file
        //     appears" because the path was never surfaced. A SUCCESS carries
        //     the written path on the Disclosure channel; a FAILURE rides the
        //     ERROR channel — different channels on purpose, because success
        //     and failure being visually indistinguishable was the reported
        //     defect itself.
        if (outcome.TryGetProperty("export_disclosures", out var exports)
            && exports.ValueKind == JsonValueKind.Array)
        {
            foreach (var export in exports.EnumerateArray())
            {
                if (OptionalString(export, "path") is { } exportedPath)
                {
                    bubbles.Add(new ChatBubble(
                        ChatBubbleKind.Disclosure, $"Exported to: {exportedPath}"));
                }
                else if (OptionalString(export, "error") is { } exportError)
                {
                    bubbles.Add(new ChatBubble(
                        ChatBubbleKind.Error, $"Export failed: {exportError}"));
                }
            }
        }

        // 6. The cards, and D-09's clear (main.ts:2298-2302). ONLY id/label/rationale
        //    are read; `tool` and `args` are present on the wire and are deliberately
        //    never touched — see ChatOptionCard's own summary for why that is a
        //    security property and not a style choice.
        if (!hasOptions)
        {
            return new ChatTurnView(bubbles, [], ClearsCards: true);
        }

        var cards = new List<ChatOptionCard>(options.GetArrayLength());
        foreach (var option in options.EnumerateArray())
        {
            cards.Add(new ChatOptionCard(
                OptionalString(option, "id") ?? string.Empty,
                OptionalString(option, "label") ?? string.Empty,
                OptionalString(option, "rationale") ?? string.Empty));
        }

        return new ChatTurnView(bubbles, cards, ClearsCards: false);
    }

    /// <summary>
    /// A serde <c>Option&lt;String&gt;</c>: the property's string value, or
    /// <c>null</c> for JSON <c>null</c>, a missing property, or any non-string kind.
    ///
    /// <para>Total by design. This runs one trust boundary in from the ABI on
    /// model-influenced JSON, on a path whose whole job is to SHOW the user what
    /// happened — a throwing read here would replace a rendered turn with an
    /// exception dialog over a field the transcript did not even need.</para>
    /// </summary>
    private static string? OptionalString(JsonElement element, string name) =>
        element.ValueKind == JsonValueKind.Object
        && element.TryGetProperty(name, out var value)
        && value.ValueKind == JsonValueKind.String
            ? value.GetString()
            : null;

    /// <summary>
    /// A serde <c>bool</c>. The backend guarantees <c>truncated</c> is never null
    /// (agent_turn.rs:262), so this only ever takes the true branch on a real
    /// payload; it is written as a <c>ValueKind</c> test rather than
    /// <c>GetBoolean()</c> for the same reason <see cref="OptionalString"/> is total.
    /// </summary>
    private static bool Flag(JsonElement element, string name) =>
        element.ValueKind == JsonValueKind.Object
        && element.TryGetProperty(name, out var value)
        && value.ValueKind == JsonValueKind.True;

    /// <summary>
    /// A failed turn: one <see cref="ChatBubbleKind.Error"/> bubble carrying the
    /// backend's own string, verbatim (D-17 — errors are never swallowed, and never
    /// reworded into something friendlier that hides which of the twenty-odd real
    /// causes fired).
    ///
    /// <para><c>ClearsCards</c> is FALSE: v6.0's <c>catch</c> branch
    /// (<c>main.ts:2303-2307</c>) never reached <c>clearOptionCards()</c>, and that
    /// is right — a turn that FAILED did not replace
    /// <c>pending_option_choice</c>, so cards already on screen are still
    /// applicable.</para>
    /// </summary>
    internal static ChatTurnView PresentError(string error) =>
        new([new ChatBubble(ChatBubbleKind.Error, error)], [], ClearsCards: false);
}
