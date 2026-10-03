using System.Text;
using System.Text.Json;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE CHAT REGION'S TWO OUTBOUND PAYLOADS — the only JSON this region builds.
// ============================================================================
//
// WINUI-FREE BY RULE (see `ChatTurnPresenter.cs`'s header and
// `ChatPurityGateTests`), so the exact wire bytes are decidable with no window.
//
// ---------------------------------------------------------------------------
// WHY A WRITER AND NEVER STRING INTERPOLATION (T-54-04)
// ---------------------------------------------------------------------------
//
// Both payloads carry a value that came from OUTSIDE: the composer's message is
// whatever the user typed, and a card id is whatever the model named it. A quote,
// a newline or a backslash in either would ALTER THE JSON STRUCTURE if the payload
// were assembled by concatenation — the injection shape, one trust boundary in
// from the ABI. `Utf8JsonWriter` escapes every value by construction, so the
// structure is fixed before any untrusted character is written into it, and
// `ChatTests` round-trips a deliberately hostile string through `JsonDocument` to
// prove the property rather than assert it.
//
// ---------------------------------------------------------------------------
// THE SHAPES, READ OFF THE PRODUCERS RATHER THAN GUESSED
// ---------------------------------------------------------------------------
//
//   AgentSendMessageArgs { message: String, selection: Vec<String> }
//     crates/ffi/src/commands.rs:568-574
//   ApplyOptionCardArgs  { card_id: String }
//     crates/ffi/src/commands.rs:300-304
//
// Both are plain `serde::Deserialize` structs with NO adjacent tag — unlike
// `rudis_transport`'s `{"cmd": {"type": .., "data": ..}}` envelope, which
// `PreviewMonitorCommand` documents at its own call site. Do not add a wrapper
// here by analogy; serde would refuse it at the boundary with no compile error.

internal static class ChatCommandPayloads
{
    /// <summary>
    /// <c>{"message":"..","selection":["clip-3"]}</c> — the
    /// <c>rudis_agent_send_message</c> request.
    ///
    /// <para>v6.0 parity on the selection (<c>main.ts:2208</c>): a SINGLE-element
    /// array built from the currently selected clip, or an EMPTY array when nothing
    /// is selected. Never <c>null</c> — the Rust field is a bare
    /// <c>Vec&lt;String&gt;</c> with no <c>Option</c> and no
    /// <c>#[serde(default)]</c>, so a missing or null <c>selection</c> fails
    /// deserialization at the boundary.</para>
    /// </summary>
    internal static string SendMessage(string message, string? selectedClipId) =>
        Build(writer =>
        {
            writer.WriteString("message", message);
            writer.WriteStartArray("selection");
            if (!string.IsNullOrEmpty(selectedClipId))
            {
                writer.WriteStringValue(selectedClipId);
            }

            writer.WriteEndArray();
        });

    /// <summary>
    /// <c>{"card_id":".."}</c> and NOTHING ELSE — the
    /// <c>rudis_apply_option_card</c> request.
    ///
    /// <para><b>T-54-01: this builder is the ONLY place an apply payload is
    /// constructed</b>, and the one property it emits is the one the backend reads.
    /// D-07's security property lives half here and half in
    /// <see cref="ChatOptionCard"/>, which has no <c>tool</c>/<c>args</c> members to
    /// smuggle: there is nothing for a caller to pass and nowhere for it to go. The
    /// backend resolves the real edit server-side from
    /// <c>AgentSession.pending_option_choice</c>.</para>
    /// </summary>
    internal static string ApplyOptionCard(string cardId) =>
        Build(writer => writer.WriteString("card_id", cardId));

    /// <summary>
    /// One JSON object, written by <see cref="Utf8JsonWriter"/> and never by
    /// concatenation (see this file's header). The writer is flushed by its own
    /// <c>Dispose</c> before the buffer is read — an unflushed writer yields an
    /// empty or truncated payload, which would surface at the ABI as a
    /// deserialization error about a field the caller did supply.
    /// </summary>
    private static string Build(Action<Utf8JsonWriter> body)
    {
        using var buffer = new MemoryStream();

        using (var writer = new Utf8JsonWriter(buffer))
        {
            writer.WriteStartObject();
            body(writer);
            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(buffer.ToArray());
    }
}
