using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Rudis.Shell.Interop;
using Rudis.Shell.Regions;

namespace Rudis.Shell.Dialogs;

/// <summary>
/// The Settings surface (Phase 69, OSS-01, D-69-10/11/12) — the ONE place a user enters,
/// replaces or clears a provider key. Two sections of identical shape: Anthropic (the
/// agent; <c>rudis_set_api_key</c> / <c>rudis_clear_api_key</c>, unchanged) and Runway
/// (generation; <c>rudis_set_provider_key</c> / <c>rudis_clear_provider_key</c> with
/// <c>{"provider":"runway"}</c>).
///
/// <para><b>No client-side key regex.</b> Validation lives exactly once, backend-side,
/// where the storage is — a second copy here could only drift and start rejecting keys the
/// backend would have accepted. A refusal is shown to the user VERBATIM.</para>
///
/// <para><b>T-54-02 / T-69-22: the key is never logged, never retained in a field and never
/// read back.</b> It exists as one local and inside the outbound payload, both of which go
/// out of scope on return. Nothing in this file passes it to <c>App.LogDiagnostic</c>. The
/// boxes are only ever emptied — never populated from stored data (the stored key never
/// crosses back over the ABI at all, T-47-13); "Replace" means "type a new one".</para>
/// </summary>
internal sealed partial class SettingsDialog : ContentDialog
{
#if DEBUG
    /// <summary>D-69-20's planted SC-2 red-leg fault. DEBUG-ONLY: plan 05's
    /// ReleaseHookAbsenceTests assert this literal and the fault file name are absent from
    /// the Release build.</summary>
    private const string Fault69EnvName = "RUDIS_FAULT_69";
#endif

    private enum Provider
    {
        Anthropic,
        Runway,
    }

    internal SettingsDialog()
    {
        InitializeComponent();
    }

    /// <summary>Show the dialog with both boxes empty and both statuses freshly read.</summary>
    internal async Task ShowSettingsAsync(XamlRoot root)
    {
        XamlRoot = root;
        ClearBoxes();
        ClearError(Provider.Anthropic);
        ClearError(Provider.Runway);
        await RefreshAsync();
        await ShowAsync();
    }

    private void ClearBoxes()
    {
        AnthropicKeyInput.Password = string.Empty;
        RunwayKeyInput.Password = string.Empty;
    }

    /// <summary>One <c>agent_status</c> read feeds both sections (D-69-15). Any failure
    /// shows the unreadable text on both — a key cannot be assumed.</summary>
    private async Task RefreshAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            ApplyUnreadable();
            return;
        }

        try
        {
            var status = await engine.AgentStatusAsync();
            if (status.Kind != RudisResultKind.Ok)
            {
                App.LogDiagnostic($"settings: agent_status failed ({status.Kind}/{status.Status})");
                ApplyUnreadable();
                return;
            }

            ApplyProvider(Provider.Anthropic, AgentStatusText.ParseAnthropic(status.Value));
            ApplyProvider(Provider.Runway, AgentStatusText.ParseProvider(status.Value, "runway"));
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"settings: agent_status threw: {ex.GetType().Name}: {ex.Message}");
            ApplyUnreadable();
        }
    }

    private void ApplyUnreadable()
    {
        foreach (var provider in new[] { Provider.Anthropic, Provider.Runway })
        {
            var (status, _, save, clear, _, _) = Parts(provider);
            SetStatusText(status, AgentStatusText.DescribeUnreadable());
            SetSaveLabel(save, provider, replace: false);
            clear.Visibility = Visibility.Collapsed;
        }
    }

    /// <summary>Status text + Name from the one helper; Save reads "Replace" only when a
    /// key is stored in Credential Manager; Clear is offered only then too (an environment
    /// key cannot be cleared from here).</summary>
    private void ApplyProvider(Provider provider, AgentKeyStatus s)
    {
        var (status, _, save, clear, _, _) = Parts(provider);
        SetStatusText(status, AgentStatusText.Describe(s));
        var stored = s.Source == KeySource.CredentialManager;
        SetSaveLabel(save, provider, replace: s.Configured && stored);
        clear.Visibility = stored ? Visibility.Visible : Visibility.Collapsed;
    }

    private static void SetStatusText(TextBlock status, string text)
    {
        status.Text = text;
        AutomationProperties.SetName(status, text);
    }

    private static void SetSaveLabel(Button save, Provider provider, bool replace)
    {
        var verb = replace ? "Replace" : "Save";
        save.Content = verb;
        AutomationProperties.SetName(save, $"{verb} {provider} key");
    }

    private (TextBlock Status, PasswordBox Box, Button Save, Button Clear, Border ErrorSurface, TextBlock Error)
        Parts(Provider provider) => provider switch
        {
            Provider.Anthropic => (AnthropicStatus, AnthropicKeyInput, AnthropicSave, AnthropicClear,
                AnthropicErrorSurface, AnthropicError),
            _ => (RunwayStatus, RunwayKeyInput, RunwaySave, RunwayClear, RunwayErrorSurface, RunwayError),
        };

    private void OnAnthropicSaveClick(object sender, RoutedEventArgs e) => _ = SaveAsync(Provider.Anthropic);

    private void OnRunwaySaveClick(object sender, RoutedEventArgs e) => _ = SaveAsync(Provider.Runway);

    private void OnAnthropicClearClick(object sender, RoutedEventArgs e) => _ = ClearAsync(Provider.Anthropic);

    private void OnRunwayClearClick(object sender, RoutedEventArgs e) => _ = ClearAsync(Provider.Runway);

    private void OnCloseClick(object sender, RoutedEventArgs e) => Hide();

    /// <summary>Anthropic → <c>rudis_set_api_key({"key"})</c>; Runway →
    /// <c>rudis_set_provider_key({"provider":"runway","key"})</c> — the only provider id this
    /// dialog ever serialises (T-69-23). A refusal is shown verbatim and the box is left as
    /// typed so the user can correct it.</summary>
    private async Task SaveAsync(Provider provider)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        var (_, box, _, _, _, _) = Parts(provider);
        var key = box.Password?.Trim();
        if (string.IsNullOrEmpty(key))
        {
            return;
        }

        try
        {
            var saved = provider == Provider.Anthropic
                ? await engine.SetApiKeyAsync(JsonSerializer.Serialize(new { key }))
                : await engine.SetProviderKeyAsync(JsonSerializer.Serialize(new { provider = "runway", key }));

            if (saved.Kind == RudisResultKind.Ok)
            {
                box.Password = string.Empty;
                ClearError(provider);
#if DEBUG
                InjectPlaintextKeyFault(key);
#endif
                await RefreshAsync();
                return;
            }

            var op = provider == Provider.Anthropic ? "set_api_key" : "set_provider_key";
            ShowError(provider, saved.Error ?? $"{op} failed ({saved.Kind}/{saved.Status})");
        }
        catch (Exception ex)
        {
            ShowError(provider, $"{ex.GetType().Name}: {ex.Message}");
        }
    }

    /// <summary>Idempotent and BEST-EFFORT (as Chat's retired ClearKeyAsync was): a failure
    /// is not user-actionable, so it is logged (never with a key — there is none here) and
    /// the refresh afterwards tells the user the truth either way.</summary>
    private async Task ClearAsync(Provider provider)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        var (_, box, _, _, _, _) = Parts(provider);
        try
        {
            var cleared = provider == Provider.Anthropic
                ? await engine.ClearApiKeyAsync()
                : await engine.ClearProviderKeyAsync(JsonSerializer.Serialize(new { provider = "runway" }));
            if (cleared.Kind != RudisResultKind.Ok)
            {
                App.LogDiagnostic($"settings: clear {provider} key FAILED ({cleared.Kind}/{cleared.Status})");
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"settings: clear {provider} key threw: {ex.GetType().Name}: {ex.Message}");
        }

        box.Password = string.Empty;
        ClearError(provider);
        await RefreshAsync();
    }

    private void ShowError(Provider provider, string message)
    {
        var (_, _, _, _, surface, error) = Parts(provider);
        error.Text = message;
        AutomationProperties.SetName(error, message);
        surface.Visibility = Visibility.Visible;
    }

    private void ClearError(Provider provider)
    {
        var (_, _, _, _, surface, error) = Parts(provider);
        error.Text = string.Empty;
        AutomationProperties.SetName(error, $"{provider} key error");
        surface.Visibility = Visibility.Collapsed;
    }

#if DEBUG
    /// <summary>
    /// D-69-20's SC-2 RED LEG, planted on purpose: with <c>RUDIS_FAULT_69=plaintext-key</c>
    /// a successful Save ALSO writes the key as UTF-8 to
    /// <c>&lt;RUDIS_TEST_DATA_ROOT&gt;\cache\rudis-fault-69-plaintext-key.txt</c>, so the
    /// sentinel scan has something real to find and is shown able to go red. DEBUG-ONLY —
    /// absent from Release (plan 05's ReleaseHookAbsenceTests). The diagnostic line names
    /// the fault, never the key.
    ///
    /// <para><b>Honoured ONLY inside a harness scope (review 69 WR-03).</b> The fault is a
    /// no-op unless <c>RUDIS_TEST_CREDENTIAL_SERVICE</c> is a <c>rudis-test-*</c> value AND
    /// <c>RUDIS_TEST_DATA_ROOT</c> is set, and it writes under that isolated root — never
    /// the owner's real <c>%LOCALAPPDATA%\app.rudis.desktop</c>. A <c>RUDIS_FAULT_69</c>
    /// left in a developer's environment therefore cannot put a real key on disk.</para>
    /// </summary>
    private static void InjectPlaintextKeyFault(string key)
    {
        if (!string.Equals(Environment.GetEnvironmentVariable(Fault69EnvName), "plaintext-key", StringComparison.Ordinal))
        {
            return;
        }

        var service = Environment.GetEnvironmentVariable(App.TestCredentialServiceEnvName);
        var root = Environment.GetEnvironmentVariable(App.TestDataRootEnvName);
        if (string.IsNullOrWhiteSpace(service)
            || !service.StartsWith("rudis-test-", StringComparison.Ordinal)
            || string.IsNullOrWhiteSpace(root))
        {
            // Never plant outside a harness scope (the owner's real dirs, a real key).
            App.LogDiagnostic("FAULT-69 plaintext-key: ignored — not a test-scoped process (needs a rudis-test-* service and a test data root)");
            return;
        }

        try
        {
            var dir = Path.Combine(root, "cache");
            Directory.CreateDirectory(dir);
            File.WriteAllText(Path.Combine(dir, "rudis-fault-69-plaintext-key.txt"), key, new System.Text.UTF8Encoding(false));
            App.LogDiagnostic("FAULT-69 plaintext-key: wrote the key to a plaintext file (injected fault, Debug only)");
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"FAULT-69 plaintext-key: write failed: {ex.GetType().Name}");
        }
    }
#endif
}
