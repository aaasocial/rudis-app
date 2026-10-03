using System.Text.Json;

namespace Rudis.Shell.Interop;

/// <summary>Which of the ABI's two error layers (or neither) a call landed in.</summary>
internal enum RudisResultKind
{
    /// <summary>Status Ok and an <c>{"Ok": ..}</c> envelope — the command succeeded.</summary>
    Ok,

    /// <summary>Status Ok but an <c>{"Err": ".."}</c> envelope — the COMMAND failed
    /// (validation, domain rules like "nothing loaded"). A user-visible outcome,
    /// NOT a transport fault; the two must never be collapsed (RESEARCH §2).</summary>
    DomainError,

    /// <summary><see cref="RudisStatus"/> != Ok (null pointer, invalid UTF-8,
    /// invalid handle, PanicCaught = -99) — the CALL failed to cross the boundary
    /// cleanly. An engine/host bug class, distinct from any domain outcome.</summary>
    TransportFault,
}

/// <summary>
/// The two-layer result every cold-path call returns: branch FIRST on
/// <see cref="Status"/> (transport fault), THEN on the envelope's Ok/Err tag
/// (domain outcome). This type makes the branching structural so no caller can
/// collapse the layers by accident.
/// </summary>
internal readonly struct RudisResult<T>
{
    public RudisResultKind Kind { get; init; }
    public RudisStatus Status { get; init; }
    public T? Value { get; init; }
    public string? Error { get; init; }

    /// <summary>The envelope exactly as it crossed the ABI (empty for transport
    /// faults, which carry no buffer) — the diagnostic surface, e.g. this plan's
    /// launch-probe render.</summary>
    public string RawEnvelope { get; init; }

    public static RudisResult<T> Ok(T value, string raw) => new()
    {
        Kind = RudisResultKind.Ok,
        Status = RudisStatus.Ok,
        Value = value,
        RawEnvelope = raw,
    };

    public static RudisResult<T> Domain(string error, string raw) => new()
    {
        Kind = RudisResultKind.DomainError,
        Status = RudisStatus.Ok,
        Error = error,
        RawEnvelope = raw,
    };

    public static RudisResult<T> Fault(RudisStatus status, string detail) => new()
    {
        Kind = RudisResultKind.TransportFault,
        Status = status,
        Error = detail,
        RawEnvelope = string.Empty,
    };
}

/// <summary>
/// Envelope parsing for the cold path. Plain System.Text.Json is deliberate here:
/// source-generated serializer contexts are RESEARCH-§4-UNVERIFIED, and nothing in
/// this wrapper runs per-tick — the hot path is a scalar and never touches JSON.
/// </summary>
internal static class Envelope
{
    public static RudisResult<JsonElement> Parse(RudisStatus status, string envelopeJson)
    {
        if (status != RudisStatus.Ok)
        {
            return RudisResult<JsonElement>.Fault(status, $"transport fault: {status}");
        }
        try
        {
            using var doc = JsonDocument.Parse(envelopeJson);
            if (doc.RootElement.ValueKind == JsonValueKind.Object)
            {
                if (doc.RootElement.TryGetProperty("Ok", out var ok))
                {
                    return RudisResult<JsonElement>.Ok(ok.Clone(), envelopeJson);
                }
                if (doc.RootElement.TryGetProperty("Err", out var err))
                {
                    var message = err.ValueKind == JsonValueKind.String
                        ? err.GetString() ?? string.Empty
                        : err.GetRawText();
                    return RudisResult<JsonElement>.Domain(message, envelopeJson);
                }
            }
            // Contract violation: the native side serializes Result<T, String>, so an
            // envelope missing both tags cannot happen. Surfaced with a distinctive
            // prefix rather than crashing — never take the shell down over a
            // diagnostic surface.
            return RudisResult<JsonElement>.Domain(
                $"envelope-contract violation: {envelopeJson}", envelopeJson);
        }
        catch (JsonException e)
        {
            return RudisResult<JsonElement>.Domain(
                $"envelope-contract violation (unparseable): {e.Message}", envelopeJson);
        }
    }
}
