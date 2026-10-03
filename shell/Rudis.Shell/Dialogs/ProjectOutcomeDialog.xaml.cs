using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Rudis.Shell.Dialogs;

/// <summary>
/// The project lifecycle's VISIBLE refusal — the shape
/// <see cref="ExportOutcomeDialog"/> established after the
/// <c>export-no-file-written</c> debug session (2026-08-01), applied to open, save and
/// save-as.
///
/// <para>That session's finding is the reason this type exists: the engine's export
/// path had been healthy the whole time, and the defect was that its OUTCOME was
/// invisible — <c>App.LogDiagnostic</c>'s ring is in-memory, bounded and unpersisted,
/// so three real exports were reported as "no file written". Open, Save and Save As
/// are the same shape of operation with the same failure mode.</para>
///
/// <para><b>Refusals only.</b> Success is deliberately silent: the backend emits
/// <c>ProjectSwitched</c>, the mirror full-resyncs and the <c>TitleBar</c> picks up the
/// new name, so a modal saying "saved" would put a click between a beginner and their
/// work for information the window already shows.</para>
///
/// <para>The reason arrives already trimmed of serde's wrapper
/// (<c>ToolbarProjectRoutes.RefusalForDisplay</c>, D-60.1-05). The RAW string is
/// logged at the call site — the log gets everything, the screen gets the
/// sentence.</para>
/// </summary>
internal sealed partial class ProjectOutcomeDialog : ContentDialog
{
    internal ProjectOutcomeDialog()
    {
        InitializeComponent();
    }

    /// <summary>Show one refusal and wait for the user to dismiss it.</summary>
    /// <param name="root">The <c>XamlRoot</c> to host the popup in (WinUI 3 requires it).</param>
    /// <param name="title">What was being attempted, in the user's terms.</param>
    /// <param name="reason">The engine's own sentence, display-trimmed.</param>
    internal async Task ShowRefusalAsync(XamlRoot root, string title, string reason)
    {
        XamlRoot = root;
        TitleText.Text = title;
        ReasonText.Text = reason;

        await ShowAsync();
    }

    private void OnCloseClick(object sender, RoutedEventArgs e) => Hide();
}
