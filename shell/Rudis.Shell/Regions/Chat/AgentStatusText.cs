using System.Text.Json;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE ONE PLACE A KEY STATUS BECOMES WORDS (Phase 69, D-69-15).
// ============================================================================
//
// WINUI-FREE BY RULE (this directory is the Chat's pure half — ChatPurityGateTests).
// `Chat.AgentStatus`, `Settings.Anthropic.Status` and `Settings.Runway.Status` all read
// their text from `Describe`, so the three can never phrase the same fact differently.
//
// The proofs (plans 06/07) key on the two PREFIXES: every output starts with either
// `connected` or `unavailable — `. The `source` enum is the only key-related fact that
// crosses the ABI (T-12-11 / T-47-13): no key material, no length, no prefix.
//
// Wire shape (`rudis_agent_status`, plan 69-01/02):
//   {"key_configured": bool, "source": "credential_manager"|"environment"|"none",
//    "providers": {"runway": {"key_configured": bool, "source": ..}}}
// A pre-Phase-69 payload carries `key_configured` only — that reads as Source=Unknown.
// ============================================================================

/// <summary>Where a provider's key came from, as <c>agent_status</c> reports it.</summary>
internal enum KeySource
{
    CredentialManager,
    Environment,
    None,
    /// <summary>The field was absent or unrecognised (legacy payload, or a read failure).</summary>
    Unknown,
}

/// <summary>One provider's key state: configured or not, and from where.</summary>
internal readonly record struct AgentKeyStatus(bool Configured, KeySource Source);

/// <summary>
/// Parses <c>agent_status</c>'s <c>Ok</c> value and turns it into the exact status text
/// D-69-15 fixes. Pure: no WinUI, no I/O.
/// </summary>
internal static class AgentStatusText
{
    public const string Connected = "connected";
    public const string ConnectedFromEnvironment =
        "connected (from environment variable — developer setup)";
    public const string UnavailablePrefix = "unavailable — ";
    public const string UnavailableNoKey = "unavailable — no API key — add one in Settings";
    public const string UnavailableUnreadable = "unavailable — status could not be read";

    /// <summary>The top-level (Anthropic) fields of the <c>Ok</c> value.</summary>
    public static AgentKeyStatus ParseAnthropic(JsonElement ok) => ParseEntry(ok);

    /// <summary><c>providers.&lt;provider&gt;</c> of the <c>Ok</c> value; missing → not configured, Unknown.</summary>
    public static AgentKeyStatus ParseProvider(JsonElement ok, string provider)
    {
        if (ok.ValueKind == JsonValueKind.Object
            && ok.TryGetProperty("providers", out var providers)
            && providers.ValueKind == JsonValueKind.Object
            && providers.TryGetProperty(provider, out var entry))
        {
            return ParseEntry(entry);
        }
        return new AgentKeyStatus(false, KeySource.Unknown);
    }

    /// <summary>The status text. Always starts with <c>connected</c> or <c>unavailable — </c>.</summary>
    public static string Describe(AgentKeyStatus status)
    {
        if (!status.Configured)
        {
            return UnavailableNoKey;
        }
        return status.Source == KeySource.Environment ? ConnectedFromEnvironment : Connected;
    }

    /// <summary>What every status element shows when <c>agent_status</c> itself failed.</summary>
    public static string DescribeUnreadable() => UnavailableUnreadable;

    private static AgentKeyStatus ParseEntry(JsonElement entry)
    {
        if (entry.ValueKind != JsonValueKind.Object)
        {
            return new AgentKeyStatus(false, KeySource.Unknown);
        }

        var configured = entry.TryGetProperty("key_configured", out var kc)
            && kc.ValueKind == JsonValueKind.True;

        var source = KeySource.Unknown;
        if (entry.TryGetProperty("source", out var src) && src.ValueKind == JsonValueKind.String)
        {
            source = src.GetString() switch
            {
                "credential_manager" => KeySource.CredentialManager,
                "environment" => KeySource.Environment,
                "none" => KeySource.None,
                _ => KeySource.Unknown,
            };
        }

        return new AgentKeyStatus(configured, source);
    }
}
