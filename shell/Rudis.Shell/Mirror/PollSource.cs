using System.Text.Json;
using System.Text.Json.Nodes;
using Rudis.Shell.Interop;

namespace Rudis.Shell.Mirror;

/// <summary>
/// Everything <see cref="ShellMirror"/> is allowed to ask the backend for — and
/// THE D-07 TEST SEAM.
///
/// <para>CONTEXT D-07 is explicit that SC-2's gap must be injected <b>at the C#
/// mirror</b>, never by suppressing a Rust-side emit: the Rust ring's
/// <c>resync_required</c> boundaries were already unit-tested at both ends in
/// Phase 47 (ring.rs:264-302, ring.rs:361-378), so re-testing them would prove
/// nothing AND would mean touching frozen engine code. The untested half is
/// whether <b>C# reacts correctly</b>. This interface is where a test wraps the
/// real source and drops exactly one record, so the mirror genuinely never sees
/// a patch the engine genuinely emitted.</para>
///
/// <para>Every method returns the ABI's two-layer <see cref="RudisResult{T}"/>
/// rather than throwing: a transport fault (<see cref="RudisStatus"/> != Ok, e.g.
/// <c>PanicCaught = -99</c>) and a domain <c>{"Err": ".."}</c> are DIFFERENT
/// failure classes and the mirror must be able to tell them apart (RESEARCH §2;
/// collapsing them throws away the distinction the ABI was designed around).</para>
/// </summary>
internal interface IMirrorSource
{
    /// <summary><c>rudis_poll_events(local_seq)</c> — non-blocking (D-12).</summary>
    Task<RudisResult<PollOutcome>> PollEventsAsync(ulong localSeq);

    /// <summary><c>rudis_get_snapshot</c> — the whole Project as its RAW node.
    /// Raw, not projected: this value becomes the mirror's authoritative state and
    /// must stay byte-faithful to what the engine serialized.</summary>
    Task<RudisResult<JsonObject>> GetSnapshotAsync();

    /// <summary><c>rudis_get_current_seq</c> — the store's mutation counter, the
    /// resync anchor <c>get_snapshot</c> is paired with (queries.rs:70-78).</summary>
    Task<RudisResult<ulong>> GetCurrentSeqAsync();

    /// <summary><c>rudis_get_entities({"ids": [..]})</c> — the named entities ONLY,
    /// in request order; unknown ids are silently OMITTED by the backend
    /// (queries.rs:47-56), which is exactly why the caller must compare counts.</summary>
    Task<RudisResult<List<JsonObject>>> GetEntitiesAsync(IReadOnlyList<string> ids);
}

/// <summary>
/// The production <see cref="IMirrorSource"/>: every call goes through
/// <see cref="RudisNative"/>, so every one of them is serialised onto the ONE
/// interop worker (50-02 §1.5(2)) — including the poll and the snapshot. Only
/// <c>rudis_get_playback_position</c> is free-threaded, and it is not part of this
/// interface because it is the HOT path and never touches the mirror.
///
/// <para>Buffer discipline is inherited, not re-implemented: <c>RudisNative</c>
/// copies-then-frees through the codebase's single <c>ReadUtf8AndFree</c>
/// chokepoint before any JSON is parsed (T-50-15), so nothing here ever sees a
/// <c>RudisBuffer</c>.</para>
/// </summary>
internal sealed class NativeMirrorSource(RudisNative native) : IMirrorSource
{
    private readonly RudisNative _native = native;

    public async Task<RudisResult<PollOutcome>> PollEventsAsync(ulong localSeq)
        => Map(await _native.PollEventsAsync(localSeq).ConfigureAwait(false),
            static ok => ok.Deserialize<PollOutcome>()
                         ?? throw new JsonException("poll envelope deserialized to null"));

    public async Task<RudisResult<JsonObject>> GetSnapshotAsync()
        => Map(await _native.GetSnapshotAsync().ConfigureAwait(false), AsObject);

    public async Task<RudisResult<ulong>> GetCurrentSeqAsync()
        => Map(await _native.GetCurrentSeqAsync().ConfigureAwait(false),
            static ok => ok.GetUInt64());

    public async Task<RudisResult<List<JsonObject>>> GetEntitiesAsync(IReadOnlyList<string> ids)
    {
        var args = JsonSerializer.Serialize(new EntitiesArgs { Ids = [.. ids] });
        return Map(await _native.GetEntitiesAsync(args).ConfigureAwait(false), static ok =>
        {
            var list = new List<JsonObject>();
            foreach (var element in ok.EnumerateArray())
            {
                list.Add(AsObject(element));
            }
            return list;
        });
    }

    /// <summary>Args shape for <c>rudis_get_entities</c>: <c>{"ids": [..]}</c>
    /// (commands.rs:147-151).</summary>
    private sealed class EntitiesArgs
    {
        [System.Text.Json.Serialization.JsonPropertyName("ids")]
        public List<string> Ids { get; init; } = [];
    }

    private static JsonObject AsObject(JsonElement element)
        => JsonNode.Parse(element.GetRawText()) as JsonObject
           ?? throw new JsonException($"expected a JSON object, got {element.ValueKind}");

    /// <summary>
    /// Carry the two-layer result forward, projecting only the <c>Ok</c> payload.
    /// A payload that does not match its documented shape is a CONTRACT violation,
    /// surfaced as a domain error rather than thrown (T-50-19: malformed payloads
    /// are never allowed to take the shell down, and never silently ignored either).
    /// </summary>
    private static RudisResult<T> Map<T>(RudisResult<JsonElement> result, Func<JsonElement, T> project)
    {
        switch (result.Kind)
        {
            case RudisResultKind.TransportFault:
                return RudisResult<T>.Fault(result.Status, result.Error ?? "transport fault");
            case RudisResultKind.DomainError:
                return RudisResult<T>.Domain(result.Error ?? "domain error", result.RawEnvelope);
            default:
                try
                {
                    return RudisResult<T>.Ok(project(result.Value), result.RawEnvelope);
                }
                catch (Exception e) when (e is JsonException or InvalidOperationException or FormatException)
                {
                    return RudisResult<T>.Domain(
                        $"payload-contract violation: {e.Message}", result.RawEnvelope);
                }
        }
    }
}
