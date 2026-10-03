using System.Globalization;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.Windows.Storage.Pickers;

namespace Rudis.Shell.Dialogs;

/// <summary>
/// What the user chose in <see cref="ExportDialog"/>.
///
/// <para><b><c>null</c> means "Source", and that is load-bearing.</b> It is the
/// ABSENCE of an override, not a fifth resolution or a seventh frame rate: the caller
/// falls back to <c>Toolbar.ResolveExportFormat()</c>'s existing mirrored-state
/// derivation, so the behaviour Phase 50 shipped and proved is what "Source" MEANS.
/// Nothing about the working path is replaced by adding this dialog in front of it.</para>
/// </summary>
/// <param name="Width">Override width in pixels, or null for Source.</param>
/// <param name="Height">Override height in pixels, or null for Source.</param>
/// <param name="Fps">Override frame rate, or null for Source (match media).</param>
/// <param name="Folder">An EXISTING directory — validated before this record is built.</param>
/// <param name="FileName">Extension-free file stem, already rejected for invalid characters.</param>
/// <param name="Extension">
/// The container extension including the dot (<c>.mp4</c> / <c>.mov</c> / <c>.mkv</c>).
///
/// <para>⚠ NOT in the plan's sketched record, and deliberately added: the C# shell's
/// <c>FileSavePicker</c> offered these same three containers and v6.0's
/// <c>#export-format</c> offers them too. Dropping the choice while porting the dialog
/// would have been a silent capability REGRESSION dressed up as a port. FFmpeg picks
/// the muxer by extension, so this reaches the encode as part of <c>out_path</c> and
/// needs no new argument.</para>
/// </param>
internal sealed record ExportChoice(
    uint? Width, uint? Height, double? Fps, string Folder, string FileName, string Extension)
{
    /// <summary>
    /// The absolute output path. <see cref="System.IO.Path.Combine"/> then
    /// <see cref="System.IO.Path.GetFullPath"/> (T-51-22): no string concatenation of
    /// a path, no separator guessing, and the result is canonicalised before it
    /// crosses the ABI. The encode itself is an ABI call with typed arguments — there
    /// is no shell interpolation anywhere on this path.
    /// </summary>
    internal string OutPath =>
        System.IO.Path.GetFullPath(System.IO.Path.Combine(Folder, FileName + Extension));
}

/// <summary>
/// The <c>Toolbar.ExportButton</c> dialog — v6.0's <c>#export-dialog</c>
/// (frontend/index.html:645-695) as an in-app WinUI 3 <see cref="ContentDialog"/>.
///
/// <para>See ExportDialog.xaml's own comment for WHY this is a ContentDialog rather
/// than a shell picker: it renders in the <c>XamlRoot</c>'s popup layer, i.e. the same
/// visual tree as the <c>Preview</c> region's <c>SwapChainPanel</c>, which is the first
/// of the three cases the retired <c>SetWindowRgn</c> hole-punch never covered
/// (SC-1 / D-14).</para>
/// </summary>
internal sealed partial class ExportDialog : ContentDialog
{
    private ExportChoice? _choice;
    private Microsoft.UI.WindowId _ownerWindowId;

    internal ExportDialog()
    {
        InitializeComponent();
    }

    /// <summary>
    /// Show the dialog and collect the user's choice. Returns <c>null</c> when the
    /// user cancelled (button, Esc or dismiss) — the caller must then touch the engine
    /// not at all.
    /// </summary>
    /// <param name="root">The <c>XamlRoot</c> to host the popup in. Required by WinUI 3.</param>
    /// <param name="ownerWindowId">Owner for the folder picker (it takes a WindowId).</param>
    /// <param name="defaultFolder">
    /// v6 parity (main.ts <c>openExportDialog</c>): the source media's directory, so an
    /// export is easy to find. Ignored when empty — v6 leaves the field blank rather
    /// than inventing a path, because a RELATIVE source path would build an
    /// unfindable output directory (its own comment records os error 123).
    /// </param>
    /// <param name="defaultFileName">v6 parity: <c>&lt;source-stem&gt;-export</c>.</param>
    internal async Task<ExportChoice?> ShowAndCollectAsync(
        XamlRoot root,
        Microsoft.UI.WindowId ownerWindowId,
        string? defaultFolder,
        string? defaultFileName)
    {
        XamlRoot = root;
        _ownerWindowId = ownerWindowId;
        _choice = null;

        if (!string.IsNullOrWhiteSpace(defaultFolder))
        {
            FolderBox.Text = defaultFolder;
        }
        if (!string.IsNullOrWhiteSpace(defaultFileName))
        {
            FileNameBox.Text = defaultFileName;
        }
        ClearError();

        await ShowAsync();
        return _choice;
    }

    private void OnCancelClick(object sender, RoutedEventArgs e)
    {
        _choice = null;
        Hide();
    }

    /// <summary>
    /// Validate, then close. A refusal keeps the dialog OPEN with the reason on screen
    /// — never closes and silently exports nowhere (UI-SPEC §5 / 50-05's rule that a
    /// refusal must be visible).
    /// </summary>
    private void OnConfirmClick(object sender, RoutedEventArgs e)
    {
        var folder = FolderBox.Text.Trim();
        if (folder.Length == 0)
        {
            ShowError("Choose a folder (Browse…) before exporting.");
            return;
        }

        // T-51-22: the folder must EXIST. An unchecked path reaches the encoder as a
        // failed open deep inside the sidecar, where the user learns nothing useful.
        string fullFolder;
        try
        {
            fullFolder = System.IO.Path.GetFullPath(folder);
        }
        catch (Exception ex)
        {
            ShowError($"That folder path is not usable: {ex.Message}");
            return;
        }
        if (!Directory.Exists(fullFolder))
        {
            ShowError($"No such folder: {fullFolder}");
            return;
        }

        // v6 drops any extension the user typed (main.ts `startExport`), so
        // "clip.mp4" + container .mov yields "clip.mov" rather than "clip.mp4.mov".
        var name = FileNameBox.Text.Trim();
        var dot = name.LastIndexOf('.');
        if (dot > 0)
        {
            name = name[..dot];
        }
        if (name.Length == 0)
        {
            ShowError("Enter a file name.");
            return;
        }
        var invalid = name.IndexOfAny(System.IO.Path.GetInvalidFileNameChars());
        if (invalid >= 0)
        {
            ShowError(
                $"'{name[invalid]}' cannot appear in a file name. A path separator here would " +
                "write outside the chosen folder, so it is refused rather than sanitised.");
            return;
        }

        var (width, height) = ParseResolution();
        _choice = new ExportChoice(width, height, ParseFps(), fullFolder, name, ParseExtension());
        Hide();
    }

    // Sync handler starting an async Task that carries its own total try/catch — the
    // shell's async-void-free way to run work from an event (50-04's pattern; a grep
    // for `async void` over shell/ must stay at zero).
    private void OnBrowseClick(object sender, RoutedEventArgs e) => _ = BrowseAsync();

    private async Task BrowseAsync()
    {
        try
        {
            var picker = new FolderPicker(_ownerWindowId)
            {
                CommitButtonText = "Choose folder",
                SuggestedStartLocation = PickerLocationId.VideosLibrary,
            };
            var chosen = await picker.PickSingleFolderAsync();
            if (chosen is not null && !string.IsNullOrEmpty(chosen.Path))
            {
                FolderBox.Text = chosen.Path;
                ClearError();
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"export folder picker failed: {ex.GetType().Name}: {ex.Message}");
            ShowError($"The folder picker failed: {ex.Message}");
        }
    }

    /// <summary>Selected item's <c>Tag</c>, or <c>""</c> when nothing is selected.</summary>
    private static string TagOf(ComboBox box) =>
        (box.SelectedItem as ComboBoxItem)?.Tag as string ?? string.Empty;

    /// <summary>
    /// <c>""</c> → (null, null) = Source. Otherwise <c>WIDTHxHEIGHT</c>, which is the
    /// literal the XAML carries so the parse cannot drift from the label.
    /// </summary>
    private (uint? Width, uint? Height) ParseResolution()
    {
        var tag = TagOf(ResolutionBox);
        var x = tag.IndexOf('x', StringComparison.Ordinal);
        if (x <= 0)
        {
            return (null, null);
        }
        return uint.TryParse(tag[..x], NumberStyles.None, CultureInfo.InvariantCulture, out var w)
            && uint.TryParse(tag[(x + 1)..], NumberStyles.None, CultureInfo.InvariantCulture, out var h)
            ? (w, h)
            : (null, null);
    }

    private double? ParseFps()
    {
        var tag = TagOf(FpsBox);
        return double.TryParse(tag, NumberStyles.Float, CultureInfo.InvariantCulture, out var fps)
            && fps > 0
            ? fps
            : null;
    }

    private string ParseExtension()
    {
        var tag = TagOf(FormatBox);
        return tag.Length > 0 ? tag : ".mp4";
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
