using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;

namespace Rudis.Shell.Dialogs;

/// <summary>
/// <c>Toolbar.Project.New</c>'s name prompt.
///
/// <para><b>It carries no naming rules and must never grow any.</b>
/// <c>sanitize_project_name</c> owns them (T-26-01) — including the Windows
/// reserved-device-name refusal a from-scratch validator misses — so this dialog hands
/// over exactly what was typed and renders whatever the engine says back. The text is
/// not trimmed, not filtered and not length-checked here, because every one of those
/// would be a second copy of a rule that already exists in Rust.</para>
///
/// <para><b>A refusal keeps the dialog OPEN,</b> with the reason on screen and the name
/// still in the box —
/// <see cref="ExportDialog"/>'s shape, deliberately reused (UI-SPEC section 5 /
/// 50-05's rule that a refusal must be visible). Closing on failure would make the
/// user retype the name in order to find out what was wrong with it.</para>
/// </summary>
internal sealed partial class ProjectNameDialog : ContentDialog
{
    /// <summary>
    /// The caller's engine call. Returns <c>null</c> when the project was created (the
    /// dialog closes), or a display-ready refusal (the dialog stays open and shows it).
    ///
    /// <para>The ENGINE CALL stays at the call site, in <c>Toolbar</c>, rather than
    /// moving in here: this shell has exactly one routine per export and this dialog is
    /// not it. Passing the submit step in keeps the "stay open on refusal" behaviour
    /// where the dialog lives without opening a second path to
    /// <c>rudis_new_project</c>.</para>
    /// </summary>
    private Func<string, Task<string?>>? _submit;

    /// <summary>Guards a double-Enter, which would otherwise dispatch two creates.</summary>
    private bool _inFlight;

    internal ProjectNameDialog()
    {
        InitializeComponent();
    }

    /// <summary>
    /// Show the prompt and run <paramref name="submit"/> for each attempt until it
    /// succeeds or the user cancels.
    /// </summary>
    /// <param name="root">The <c>XamlRoot</c> to host the popup in (WinUI 3 requires it).</param>
    /// <param name="submit">The engine call; see <see cref="_submit"/>.</param>
    internal async Task CollectAsync(XamlRoot root, Func<string, Task<string?>> submit)
    {
        XamlRoot = root;
        _submit = submit;
        _inFlight = false;
        NameBox.Text = string.Empty;
        ClearError();

        await ShowAsync();
    }

    private void OnCancelClick(object sender, RoutedEventArgs e) => Hide();

    // Sync handler starting an async Task that carries its own total try/catch — the
    // shell's async-void-free event pattern (50-04; the `async void` scan over shell/
    // must stay at zero).
    private void OnConfirmClick(object sender, RoutedEventArgs e) => _ = ConfirmAsync();

    /// <summary>Enter commits, because this dialog's own buttons replace
    /// <c>ContentDialog</c>'s primary/close pair and take its default Enter handling
    /// with them.</summary>
    private void OnNameKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key != Windows.System.VirtualKey.Enter)
        {
            return;
        }
        e.Handled = true;
        _ = ConfirmAsync();
    }

    /// <summary>A stale refusal above a name the user has since edited is worse than
    /// no refusal — it describes a string that is no longer there.</summary>
    private void OnNameChanged(object sender, TextChangedEventArgs e) => ClearError();

    private async Task ConfirmAsync()
    {
        if (_inFlight || _submit is null)
        {
            return;
        }

        _inFlight = true;
        ConfirmButton.IsEnabled = false;
        CancelButton.IsEnabled = false;
        try
        {
            // NOT trimmed, NOT validated: see the class remarks. Whatever is in the box
            // is what `sanitize_project_name` gets to rule on.
            var refusal = await _submit(NameBox.Text);
            if (refusal is null)
            {
                Hide();
                return;
            }
            ShowError(refusal);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"new project submit threw: {ex.GetType().Name}: {ex.Message}");
            ShowError($"That did not work: {ex.Message}");
        }
        finally
        {
            _inFlight = false;
            ConfirmButton.IsEnabled = true;
            CancelButton.IsEnabled = true;
        }
    }

    private void ShowError(string message)
    {
        ErrorText.Text = message;
        ErrorSurface.Visibility = Visibility.Visible;
    }

    private void ClearError()
    {
        ErrorText.Text = string.Empty;
        ErrorSurface.Visibility = Visibility.Collapsed;
    }
}
