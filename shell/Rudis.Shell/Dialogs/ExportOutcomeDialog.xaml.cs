using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Rudis.Shell.Dialogs;

/// <summary>
/// The Export button's visible outcome — debug session
/// <c>export-no-file-written</c> (2026-08-01).
///
/// <para>Before this dialog, <c>Toolbar.ExportAsync</c> surfaced NOTHING when the
/// encode returned: success and failure both looked like the busy fill clearing,
/// and the only record was <c>App.LogDiagnostic</c>'s in-memory ring (bounded,
/// unpersisted, invisible). The investigation proved the engine's export path was
/// healthy the whole time — the defect was that its outcome was invisible. This
/// dialog shows SUCCESS with the real written path (selectable, plus a
/// Show-in-folder affordance) and FAILURE with the engine's error verbatim, in
/// visibly different states.</para>
///
/// <para><b>Show-in-folder deliberately uses <c>Windows.System.Launcher</c>, never
/// <c>Process.Start</c>:</b> the shipped shell spawns NO subprocess of its own
/// (SC-3 / CONTEXT D-14, enforced mechanically by
/// <c>MechanicalGatesTests.ScanForMediaSubprocess</c>'s Process/ProcessStartInfo
/// patterns). <c>Launcher.LaunchFolderPathAsync</c> is the WinRT shell-activation
/// surface — the OS opens Explorer; this process starts nothing.</para>
/// </summary>
internal sealed partial class ExportOutcomeDialog : ContentDialog
{
    private string? _writtenPath;

    internal ExportOutcomeDialog()
    {
        InitializeComponent();
    }

    /// <summary>
    /// Show the outcome for one completed export. Exactly one of the two
    /// parameters is non-null: <paramref name="writtenPath"/> on success (the
    /// path the engine returned — the file provably exists there),
    /// <paramref name="error"/> on failure (the engine's reason, verbatim —
    /// D-17's errors-are-never-swallowed rule, extended from Chat to the
    /// Toolbar's export).
    /// </summary>
    internal async Task ShowOutcomeAsync(XamlRoot root, string? writtenPath, string? error)
    {
        XamlRoot = root;
        _writtenPath = writtenPath;

        if (writtenPath is not null)
        {
            TitleText.Text = "Export complete";
            PathText.Text = PathDisplay.ToDisplay(writtenPath);
            SuccessPanel.Visibility = Visibility.Visible;
            ErrorSurface.Visibility = Visibility.Collapsed;
            ShowInFolderButton.Visibility = Visibility.Visible;
        }
        else
        {
            TitleText.Text = "Export failed";
            ErrorText.Text = error ?? "unknown error";
            SuccessPanel.Visibility = Visibility.Collapsed;
            ErrorSurface.Visibility = Visibility.Visible;
            ShowInFolderButton.Visibility = Visibility.Collapsed;
        }

        await ShowAsync();
    }

    private void OnCloseClick(object sender, RoutedEventArgs e) => Hide();

    // Sync handler starting an async Task carrying its own total try/catch —
    // the shell's async-void-free event pattern (50-04; the `async void` grep
    // over shell/ must stay at zero).
    private void OnShowInFolderClick(object sender, RoutedEventArgs e) => _ = ShowInFolderAsync();

    /// <summary>
    /// Open Explorer at the exported file, selecting it when the item-select
    /// launch is available; falls back to opening the containing folder. Any
    /// failure is logged and non-fatal — the path is still on screen and
    /// selectable, so the user is never stranded.
    /// </summary>
    private async Task ShowInFolderAsync()
    {
        var path = _writtenPath;
        if (path is null)
        {
            return;
        }

        var folder = System.IO.Path.GetDirectoryName(PathDisplay.ToDisplay(path));
        if (folder is null)
        {
            return;
        }

        try
        {
            var file = await Windows.Storage.StorageFile.GetFileFromPathAsync(
                PathDisplay.ToDisplay(path));
            var options = new Windows.System.FolderLauncherOptions();
            options.ItemsToSelect.Add(file);
            await Windows.System.Launcher.LaunchFolderPathAsync(folder, options);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic(
                $"show-in-folder select failed ({ex.GetType().Name}: {ex.Message}) — " +
                "falling back to a plain folder open");
            try
            {
                await Windows.System.Launcher.LaunchFolderPathAsync(folder);
            }
            catch (Exception inner)
            {
                App.LogDiagnostic(
                    $"show-in-folder open failed: {inner.GetType().Name}: {inner.Message}");
            }
        }
    }
}
