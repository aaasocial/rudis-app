using System.Globalization;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Rudis.Shell.Dialogs;

/// <summary>
/// The remove-track confirmation — v6.0's <c>#remove-track-dialog</c>
/// (frontend/index.html:697-712 + main.ts <c>confirmRemoveTrack</c>) as an in-app
/// WinUI 3 <see cref="ContentDialog"/>.
///
/// <para><b>This is a gate on a DESTRUCTIVE, CASCADING command</b> (T-51-23).
/// <c>Command::RemoveTrack</c> takes the whole lane and every clip on it; the only
/// thing standing between a click and that cascade is this dialog naming what will go.
/// It is undoable — the inverse carries the entire removed track — but "undoable" is
/// not a substitute for "asked".</para>
///
/// <para>Semantics are v6's, exactly: <c>Remove</c> resolves true; Cancel, Esc and
/// dismiss all resolve false and are a no-op.</para>
/// </summary>
internal sealed partial class RemoveTrackDialog : ContentDialog
{
    private bool _confirmed;

    internal RemoveTrackDialog()
    {
        InitializeComponent();
    }

    /// <summary>
    /// Show the confirmation and resolve to the user's choice.
    /// </summary>
    /// <param name="root">The <c>XamlRoot</c> to host the popup in (WinUI 3 requires it).</param>
    /// <param name="trackKind"><c>"video"</c> or <c>"audio"</c> — the mirrored lane kind.</param>
    /// <param name="clipCount">How many clips the cascade will take with the lane.</param>
    internal async Task<bool> ConfirmAsync(XamlRoot root, string trackKind, int clipCount)
    {
        XamlRoot = root;
        _confirmed = false;

        // v6's wording, character-for-character (main.ts `confirmRemoveTrack`):
        //   `This ${kind} track has ${clipCount} clip(s). Remove it and all its clips?`
        // Ported rather than reworded, because the message IS the mitigation: it is the
        // only place the cascade's size is stated.
        MessageText.Text = string.Create(
            CultureInfo.InvariantCulture,
            $"This {trackKind} track has {clipCount} clip(s). Remove it and all its clips?");

        await ShowAsync();
        return _confirmed;
    }

    private void OnCancelClick(object sender, RoutedEventArgs e)
    {
        _confirmed = false;
        Hide();
    }

    private void OnConfirmClick(object sender, RoutedEventArgs e)
    {
        _confirmed = true;
        Hide();
    }
}
