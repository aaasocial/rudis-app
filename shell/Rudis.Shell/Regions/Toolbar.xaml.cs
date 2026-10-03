using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.Windows.Storage.Pickers;
using Rudis.Shell.Dialogs;
using Rudis.Shell.Interop;
using Rudis.Shell.Mirror;
using VirtualKey = Windows.System.VirtualKey;

namespace Rudis.Shell.Regions;

/// <summary>
/// The <c>Toolbar</c> region (design_handoff_rudis_editor/README.md:88 — the name is
/// the handoff's, verbatim, per CLAUDE.md rule 7).
///
/// <para><b>Every control drives a REAL backend command (D-13, rule 1).</b> The
/// project lifecycle, import, undo, redo and export all reach the engine through
/// <see cref="RudisNative"/>, so
/// all of them are serialised onto the ONE interop worker and none of them can run on
/// the UI thread (50-02 §1.5(2)). The two exceptions are honest ones:
/// <c>Toolbar.ModeTabs</c> has no backend by design (UI-SPEC §3) and its
/// <c>Color</c> segment reveals an explicitly-LABELLED placeholder, because
/// <c>Inspector</c> is a deferred surface.</para>
///
/// <para><b>No visible import destination this phase.</b> <c>MediaBin</c> is Phase 53.
/// A successful import is observed through the mirror (<c>project:changed</c> →
/// media_bin), which is what
/// <c>ImportMirrorTests.import_lands_in_the_mirror_with_no_visible_destination</c>
/// asserts. Faking a MediaBin here would violate rule 1 AND create work Phase 53
/// would have to delete (UI-SPEC §9 item 12).</para>
/// </summary>
public sealed partial class Toolbar : UserControl
{
    /// <summary>The extensions offered by the import picker. Deliberately a
    /// convenience filter only — the authoritative gate is the backend's own probe,
    /// which rejects anything it cannot decode (the T-47-09 clamp class). The picker
    /// filter is not a security boundary and is not treated as one.</summary>
    private static readonly string[] MediaExtensions =
    [
        ".mp4", ".mov", ".mkv", ".webm", ".avi", ".m4v",
        ".m4a", ".mp3", ".wav", ".flac", ".aac",
        ".png", ".jpg", ".jpeg", ".bmp", ".gif",
    ];

    private Window? _window;
    private bool _exportInFlight;
    private bool _timelineHasContent;

    public Toolbar()
    {
        InitializeComponent();
        // XAML starts Toolbar.ExportButton disabled (an empty timeline cannot export);
        // dim its label to match, since the label is Content, not template (see
        // SetExportEnabled).
        SetExportEnabled(false);

        // Ctrl+, (Settings, Phase 69 D-69-10). Registered in code, not XAML:
        // Windows.System.VirtualKey has no member for the comma key (VK_OEM_COMMA = 0xBC),
        // so the XAML enum converter cannot spell it.
        var settingsAccelerator = new KeyboardAccelerator
        {
            Modifiers = Windows.System.VirtualKeyModifiers.Control,
            Key = (VirtualKey)0xBC,
        };
        settingsAccelerator.Invoked += OnSettingsAccelerator;
        ToolbarRoot.KeyboardAccelerators.Add(settingsAccelerator);
    }

    /// <summary>Raised when <c>Toolbar.ModeTabs</c> changes. Pure shell view-state —
    /// <c>"edit"</c> or <c>"color"</c> — with NO backend call (UI-SPEC §3).</summary>
    public event Action<string>? ModeChanged;

    /// <summary>Bind this region to its window. Needed only for the file pickers,
    /// which take the window's <c>WindowId</c>.</summary>
    public void AttachToWindow(Window window) => _window = window;

    // ── the ONE import routine: picker, window-wide drop and --import all land here ──

    /// <summary>
    /// THE shared import routine. All three entry points converge here —
    /// <c>Toolbar.ImportButton</c>'s picker, the window-wide file drop
    /// (<c>MainWindow</c>), and <c>App</c>'s <c>--import</c> startup argument — so
    /// there is exactly one place that talks to <c>rudis_import_media</c> and exactly
    /// one place to change if its contract moves.
    ///
    /// <para>Args shape read from the producer, not guessed:
    /// <c>rudis_import_media</c> takes <c>{"paths": [".."]}</c>
    /// (<c>crates/ffi/src/commands.rs:413-416</c>) and answers
    /// <c>{"Ok": [{..MediaBinItem..}, ..]}</c>, pushing one <c>project:changed</c> per
    /// file from inside the shared <c>run_import_media_ui</c> body.</para>
    ///
    /// <para>T-50-20: paths reaching here have already been reduced to
    /// existing files by their caller; the backend's probe is the real gate.</para>
    /// </summary>
    /// <param name="paths">Absolute file paths.</param>
    /// <param name="loadPreviewOnFirst">
    /// When true, the first imported item is also loaded into the SOURCE monitor via
    /// <c>rudis_transport load_preview</c> so a later Play has a target. Used by the
    /// <c>--import</c> route (open-with-file behaviour, and plan 50-08's UIA route);
    /// the picker and drop routes leave the monitor alone, matching v6, where
    /// importing does not move the preview.
    /// </param>
    public async Task ImportPathsAsync(IReadOnlyList<string> paths, bool loadPreviewOnFirst)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid || paths.Count == 0)
        {
            return;
        }

        var args = new JsonObject { ["paths"] = new JsonArray([.. paths.Select(p => (JsonNode)p!)]) };
        RudisResult<JsonElement> imported;
        try
        {
            imported = await engine.ImportMediaAsync(args.ToJsonString());
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"import_media threw: {ex.GetType().Name}: {ex.Message}");
            return;
        }
        if (imported.Kind != RudisResultKind.Ok)
        {
            // UI-SPEC §5: a backend refusal must be VISIBLE, never swallowed.
            App.LogDiagnostic($"import_media FAILED ({imported.Kind}/{imported.Status}): {imported.Error}");
            return;
        }

        var count = imported.Value.ValueKind == JsonValueKind.Array ? imported.Value.GetArrayLength() : 0;
        App.LogDiagnostic(
            $"import_media: {count}/{paths.Count} imported (no visible destination this phase - " +
            "MediaBin is Phase 53; success is observed through the mirror)");

        if (!loadPreviewOnFirst || count == 0)
        {
            return;
        }

        var firstId = imported.Value[0].TryGetProperty("id", out var idProp) ? idProp.GetString() : null;
        if (firstId is null)
        {
            return;
        }

        // Args shape read from the producer, not guessed: rudis_transport takes
        // TransportArgs { cmd: TransportCmd } (commands.rs:274-276), and TransportCmd
        // is ADJACENTLY tagged (`{"type": .., "data": {..}}`, transport.rs:11-12,21).
        // So the command object must be WRAPPED in a "cmd" key - passing the bare
        // TransportCmd deserializes to nothing and the call fails.
        var transportArgs = new JsonObject
        {
            ["cmd"] = new JsonObject
            {
                ["type"] = "load_preview",
                ["data"] = new JsonObject { ["media_id"] = firstId },
            },
        };
        var playback = await engine.TransportAsync(transportArgs.ToJsonString());
        if (playback.Kind != RudisResultKind.Ok)
        {
            App.LogDiagnostic($"transport load_preview FAILED ({playback.Kind}): {playback.Error}");
            return;
        }

        // Frontend parity (main.ts:575-591): apply the returned Playback IMMEDIATELY
        // rather than waiting a poll interval, and tell the mirror which monitor the
        // payload belongs to — load_preview flips preview_mode to Source but
        // playback:changed carries no monitor tag (50-04's D-15 FINDING).
        App.Mirror?.NotePreviewMode("source");
        App.Mirror?.ApplyPlaybackPayload(JsonNode.Parse(playback.Value.GetRawText()));
    }

    // Sync handler starting an async Task that carries its own total try/catch -
    // the async-void-free way to run work from an event (50-04's pattern; a grep for
    // `async void` over shell/ must stay at zero).
    private void OnImportClick(object sender, RoutedEventArgs e) => _ = PickAndImportAsync();

    /// <summary>
    /// The picker route. Uses WinAppSDK 1.8's <c>Microsoft.Windows.Storage.Pickers</c>,
    /// which takes a <c>WindowId</c> directly — so the unpackaged-WinUI-3 owner-window
    /// requirement is satisfied without any COM initialisation or window-handle
    /// interop, and this region stays free of Win32 plumbing (the Phase 51 rule).
    /// </summary>
    /// <remarks>Widened from <c>private</c> to <c>internal</c> by plan 53-02 so
    /// <c>MediaBin.ImportMediaButton</c> can reach the SAME routine through
    /// <c>MainWindow</c>, instead of the MediaBin opening a second path to
    /// <c>rudis_import_media</c>. There is still exactly one import routine in this
    /// shell and it is still this one.</remarks>
    internal async Task PickAndImportAsync()
    {
        if (_window is null)
        {
            return;
        }
        try
        {
            var picker = new FileOpenPicker(_window.AppWindow.Id)
            {
                CommitButtonText = "Import",
                SuggestedStartLocation = PickerLocationId.VideosLibrary,
            };
            foreach (var ext in MediaExtensions)
            {
                picker.FileTypeFilter.Add(ext);
            }

            var picked = await picker.PickMultipleFilesAsync();
            if (picked is null || picked.Count == 0)
            {
                return;
            }
            await ImportPathsAsync([.. picked.Select(f => f.Path)], loadPreviewOnFirst: false);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"import picker failed: {ex.GetType().Name}: {ex.Message}");
        }
    }

    // ── project lifecycle: New / Open… / Open Recent ────────────────────────
    //
    // THE PHASE THIS CODE CLOSES. Plans 60.1-02, -03 and -04 each declined to tick a
    // PROJ requirement with the same sentence — the engine behaviour is proven, the
    // wrappers are proven, and a USER can still do none of it. There was no menu item,
    // no Ctrl+O and no picker; the owner could not open a .rud sitting in the very
    // folder the app manages. These are the routes that make that sentence false.
    //
    // ⚠ TWO ROUTES INTO A PROJECT, DELIBERATELY, AND NEITHER IS DEAD CODE:
    //
    //   Open…       -> rudis_open_project_at_path   The user chose a FILE, from
    //                                               anywhere on disk. This is the one
    //                                               that makes "from anywhere" true;
    //                                               the name route cannot reach a .rud
    //                                               outside the managed directory at
    //                                               all, and plan 60.1-04 pinned that
    //                                               as an assertion.
    //
    //   Open Recent -> rudis_open_project           The app ENUMERATED it, and the name
    //                                               is how the registry and the agent
    //                                               both address a project. Sending a
    //                                               path here would work and would also
    //                                               throw away the one identifier the
    //                                               rest of the system uses.
    //
    // ⚠ AND THE ENVELOPES ARE NOT THE SAME SHAPE (plan 60.1-04's measured table).
    // rudis_new_project and rudis_open_project answer a JsonValueKind.String — PROSE,
    // unchanged since Phase 26, because both are also agent tools. The other five
    // answer an Object or an Array. Calling .GetProperty("name") on either of the two
    // prose ones raises InvalidOperationException, not a null, so nothing below reads
    // into their payload; the mirror is what tells the UI what happened.

    private void OnProjectNewClick(object sender, RoutedEventArgs e) => _ = NewProjectAsync();

    private void OnProjectOpenClick(object sender, RoutedEventArgs e) => _ = OpenProjectAsync();

    private void OnProjectFlyoutOpening(object sender, object e) => _ = PopulateRecentProjectsAsync();

    /// <summary>
    /// <c>rudis_new_project</c>, behind <see cref="Dialogs.ProjectNameDialog"/>.
    ///
    /// <para><b>The shell does NOT pre-validate the name</b> (T-26-01).
    /// <c>sanitize_project_name</c> owns those rules, including the Windows
    /// reserved-device-name refusal a from-scratch validator misses, so a duplicate or
    /// a "NUL" comes back as a domain <c>Err</c> with a readable sentence and the
    /// dialog stays open showing it. A second rule set here is exactly the drift the
    /// don't-hand-roll rule exists to prevent.</para>
    ///
    /// <para>Also NOT undoable, and the engine says so in its own prose answer — this
    /// switches the active document, which no undo stack spans.</para>
    /// </summary>
    internal async Task NewProjectAsync()
    {
        var engine = App.Engine;
        var root = XamlRoot;
        if (engine is null || engine.IsInvalid || root is null)
        {
            App.LogDiagnostic("new project: no engine or no XamlRoot yet — nothing dispatched");
            return;
        }

        try
        {
            var dialog = new Dialogs.ProjectNameDialog();
            await dialog.CollectAsync(root, async typed =>
            {
                var created = await engine.NewProjectAsync(ToolbarProjectRoutes.BuildNameArgs(typed));
                if (created.Kind == RudisResultKind.Ok)
                {
                    App.LogDiagnostic($"new_project OK -> {created.RawEnvelope}");
                    return null;
                }

                // The RAW string to the log, the trimmed one to the screen (D-60.1-05).
                App.LogDiagnostic(
                    $"new_project REFUSED ({created.Kind}/{created.Status}): {created.Error}");
                return ToolbarProjectRoutes.RefusalForDisplay(
                    created.Error ?? $"{created.Kind}/{created.Status}");
            });
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"new project failed: {ex.GetType().Name}: {ex.Message}");
            await ShowProjectRefusalAsync("Could not create that project", ex);
        }
    }

    /// <summary>
    /// <c>rudis_open_project_at_path</c> — the ROADMAP's scope item 3, and the route
    /// that makes "from anywhere on disk" true rather than aspirational.
    ///
    /// <para>Uses WinAppSDK 1.8's <c>Microsoft.Windows.Storage.Pickers</c>, which takes
    /// the <c>WindowId</c> in the constructor. ⚠ The owner-window requirement has NOT
    /// gone away — an unpackaged WinUI 3 picker without an owner shows nothing at all,
    /// with no exception a user sees. What changed is how it is satisfied. Importing
    /// the older, one-word-different <c>Windows.Storage.Pickers</c> silently
    /// reintroduces the need for <c>InitializeWithWindow</c>, which is why
    /// <c>ToolbarProjectRoutesTests</c> pins the namespace by source scan: this repo
    /// has already paid for that mistake twice, and the second time compiled fine.</para>
    ///
    /// <para>A cancelled picker is ORDINARY, not a refusal: <c>null</c> returns without
    /// touching the engine and without raising anything. A refusal from
    /// <c>RudProjectPath</c>'s validation is different — it arrives as a domain
    /// <c>Err</c> naming what was wrong (a folder, a non-.rud, a missing file) and is
    /// shown.</para>
    ///
    /// <para>The path is canonicalised ONCE, here, with <c>Path.GetFullPath</c>
    /// (T-51-22), and marshalled with <c>JsonObject</c> (T-52-34). Never concatenated:
    /// every separator in a Windows path is a JSON escape character.</para>
    /// </summary>
    internal async Task OpenProjectAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid || _window is null)
        {
            return;
        }

        string chosen;
        try
        {
            var picker = new FileOpenPicker(_window.AppWindow.Id)
            {
                CommitButtonText = "Open",
                SuggestedStartLocation = PickerLocationId.DocumentsLibrary,
            };
            picker.FileTypeFilter.Add(ToolbarProjectRoutes.RudExtension);

            var file = await picker.PickSingleFileAsync();
            if (file is null || string.IsNullOrEmpty(file.Path))
            {
                // Cancel is ordinary. Nothing is dispatched and nothing is reported.
                return;
            }
            chosen = Path.GetFullPath(file.Path);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"open picker failed: {ex.GetType().Name}: {ex.Message}");
            await ShowProjectRefusalAsync("Could not open that project", ex);
            return;
        }

        try
        {
            var opened = await engine.OpenProjectAtPathAsync(
                ToolbarProjectRoutes.BuildPathArgs(chosen));
            if (opened.Kind == RudisResultKind.Ok)
            {
                App.LogDiagnostic($"open_project_at_path OK -> {opened.RawEnvelope}");
                return;
            }

            App.LogDiagnostic(
                $"open_project_at_path REFUSED ({opened.Kind}/{opened.Status}): {opened.Error}");
            await ShowProjectRefusalAsync("Could not open that project", opened);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"open_project_at_path threw: {ex.GetType().Name}: {ex.Message}");
            await ShowProjectRefusalAsync("Could not open that project", ex);
        }
    }

    /// <summary>
    /// Fill <c>Toolbar.Project.Recent</c> from a REAL directory scan, every time the
    /// parent flyout opens.
    ///
    /// <para><b>Never from a cache and never from a stub</b> (CLAUDE.md rule 1).
    /// <c>run_get_projects_detailed</c> walks the managed <c>projects/</c> directory
    /// and reads each file, so a list built here at open time is the only one that can
    /// be true — a project saved, renamed or deleted since the last open would
    /// otherwise still be offered.</para>
    ///
    /// <para><b>Total by construction.</b> This runs inside <c>MenuFlyout.Opening</c>,
    /// where an unhandled exception takes the whole menu with it, so every branch ends
    /// in rows or a notice and none of them throws.</para>
    ///
    /// <para>⚠ <b><c>rudis_get_projects</c> lists the MANAGED directory only</b>, which
    /// plan 60.1-04 pinned as an assertion rather than a surprise: a project saved to
    /// an arbitrary path with Save As will NOT appear here. That is the export's
    /// contract, not a defect, and a genuine recent-files list (one that remembered
    /// arbitrary paths) would be a new persisted surface, not a change to this
    /// call.</para>
    /// </summary>
    private async Task PopulateRecentProjectsAsync()
    {
        var items = ProjectRecentItem.Items;
        items.Clear();

        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            items.Add(RecentNoticeRow("The engine is not running"));
            return;
        }

        try
        {
            var listed = await engine.GetProjectsAsync();
            items.Clear();

            if (listed.Kind != RudisResultKind.Ok)
            {
                App.LogDiagnostic(
                    $"get_projects FAILED ({listed.Kind}/{listed.Status}): {listed.Error}");
                items.Add(RecentNoticeRow(ToolbarProjectRoutes.RefusalForDisplay(
                    listed.Error ?? $"{listed.Kind}/{listed.Status}")));
                return;
            }

            var rows = ToolbarProjectRoutes.ProjectRows(listed.Value);
            if (rows.Count == 0)
            {
                // An EMPTY STATE, not a placeholder: the scan really did run and really
                // did find nothing, which is what a fresh install looks like.
                items.Add(RecentNoticeRow("No saved projects yet"));
                return;
            }

            foreach (var row in rows)
            {
                items.Add(BuildRecentRow(row));
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"get_projects threw: {ex.GetType().Name}: {ex.Message}");
            items.Clear();
            items.Add(RecentNoticeRow("Could not read the projects folder"));
        }
    }

    /// <summary>
    /// One scanned project as a menu row.
    ///
    /// <para>⚠ The name is rendered as TEXT ONLY and its displayed length is capped
    /// (T-60.1-06). <c>scan_known_projects</c> reads <c>Project.name</c> out of each
    /// file, so anything with write access to the projects directory chooses that
    /// string. <c>MenuFlyoutItem.Text</c> is inert, which is why this is a LAYOUT
    /// concern rather than an injection one — but an unbounded name turns the only
    /// surface a user has for reaching their other projects into an unusable one.
    /// <c>RecentProjectRow.Name</c> stays untouched, because that is the string the
    /// name route addresses the project by; only the DISPLAY is capped.</para>
    ///
    /// <para><c>ToggleMenuFlyoutItem</c> rather than an invented glyph: the platform's
    /// own check mark is a template part, so "this is the open project" costs no
    /// colour, no codepoint and no token the handoff does not define.</para>
    ///
    /// <para>⚠ <b><c>D-60.1-04</c> is visible right here, and is left visible.</b> The
    /// check comes straight from the engine's <c>isActive</c>, and
    /// <c>run_get_projects_detailed</c> compares a canonical extended-length path
    /// against a plain scan-form one — so a project opened by PATH reads
    /// <c>false</c> even when its file is inside the managed directory, and no row is
    /// marked. Reconciling that in C# would hide a Rust defect from the Rust tests that
    /// own it and would change this mark's meaning from "what the engine knows" to
    /// "what the shell guessed". The fix belongs in <c>run_get_projects_detailed</c>,
    /// canonicalising both sides.</para>
    /// </summary>
    private MenuFlyoutItemBase BuildRecentRow(RecentProjectRow row)
    {
        var item = new ToggleMenuFlyoutItem
        {
            Text = row.DisplayName,
            IsChecked = row.IsActive,
        };

        if (row.DisplayModified.Length > 0)
        {
            // The menu's own right-aligned secondary slot. No accelerator exists for a
            // recent entry, so the slot is free and is where a date belongs.
            item.KeyboardAcceleratorTextOverride = row.DisplayModified;
        }

        AutomationProperties.SetAutomationId(item, "Toolbar.Project.Recent.Item");
        AutomationProperties.SetName(item, row.DisplayName);
        if (row.Path.Length > 0)
        {
            // The plain form, never the \\?\ one — PathDisplay.ToDisplay is the shell's
            // one place that strip happens (debug session export-no-file-written).
            ToolTipService.SetToolTip(item, Dialogs.PathDisplay.ToDisplay(row.Path));
        }

        item.Click += (_, _) => _ = OpenProjectByNameAsync(row.Name);
        return item;
    }

    /// <summary>A disabled, non-actionable row explaining why the submenu is empty.
    /// Disabled on purpose: a clickable row that does nothing is the fake-functional
    /// control rule 1 forbids.</summary>
    private static MenuFlyoutItemBase RecentNoticeRow(string text)
    {
        var item = new MenuFlyoutItem { Text = text, IsEnabled = false };
        AutomationProperties.SetAutomationId(item, "Toolbar.Project.Recent.Notice");
        AutomationProperties.SetName(item, text);
        return item;
    }

    /// <summary>
    /// <c>rudis_open_project</c> — the NAME route, reached only from Open Recent, where
    /// the app itself enumerated the project and the name is the identifier the
    /// registry and the agent both use.
    ///
    /// <para>Its <c>Ok</c> payload is PROSE (a <c>JsonValueKind.String</c>), unchanged
    /// since Phase 26 because this is also an agent tool. Nothing here reads into
    /// it.</para>
    /// </summary>
    private async Task OpenProjectByNameAsync(string name)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        try
        {
            var opened = await engine.OpenProjectAsync(ToolbarProjectRoutes.BuildNameArgs(name));
            if (opened.Kind == RudisResultKind.Ok)
            {
                App.LogDiagnostic($"open_project OK -> {opened.RawEnvelope}");
                return;
            }

            App.LogDiagnostic($"open_project REFUSED ({opened.Kind}/{opened.Status}): {opened.Error}");
            await ShowProjectRefusalAsync("Could not open that project", opened);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"open_project threw: {ex.GetType().Name}: {ex.Message}");
            await ShowProjectRefusalAsync("Could not open that project", ex);
        }
    }

    /// <summary>
    /// Put a domain refusal on the SCREEN (UI-SPEC section 5; 50-05's rule that a
    /// refusal must be visible, never swallowed).
    ///
    /// <para>The precedent is a measured one, not a preference: before
    /// <c>ExportOutcomeDialog</c>, a completed export surfaced nothing at all and its
    /// only record was <c>App.LogDiagnostic</c>'s in-memory ring — which is how three
    /// real, successful exports came to be reported as "no file written" (debug session
    /// <c>export-no-file-written</c>). The log keeps the raw string; the user gets the
    /// sentence.</para>
    /// </summary>
    private Task ShowProjectRefusalAsync(string title, RudisResult<JsonElement> result) =>
        ShowProjectRefusalAsync(
            title,
            ToolbarProjectRoutes.RefusalForDisplay(result.Error ?? $"{result.Kind}/{result.Status}"));

    /// <summary>
    /// The THROWN half of the same contract (plan 63-03, TRUST-02 / D-60.1-14).
    ///
    /// <para>Every <c>catch (Exception)</c> in this file's project family used to end at
    /// <see cref="App.LogDiagnostic"/> and nowhere else, which on a RELEASE build meant
    /// the user got nothing at all: the ring is in-memory and process-private, and
    /// <c>Debug.WriteLine</c> is compiled out. A refusal envelope was visible and a thrown
    /// exception was not — the same operation, the same user, two different products
    /// depending on which way it failed.</para>
    ///
    /// <para><b>⚠ <c>GetType().Name</c> and <c>Message</c> ONLY — never
    /// <c>ex.ToString()</c> (T-63-09).</b> <c>ToString()</c> carries the stack trace, which
    /// names this repository's own source paths and, for a nested exception, arbitrary
    /// inner state. The type name is what makes a report actionable ("FileNotFoundException"
    /// versus "COMException"); the frames are for the log, and the log already has them
    /// via the call site's own line.</para>
    /// </summary>
    private Task ShowProjectRefusalAsync(string title, Exception ex) =>
        ShowProjectRefusalAsync(title, $"{ex.GetType().Name}: {ex.Message}");

    private async Task ShowProjectRefusalAsync(string title, string reason)
    {
        var root = XamlRoot;
        if (root is null)
        {
            // ⚠ THE REASON GOES IN THE LINE, not just the title. Before plan 63-03 this
            // said "the reason is in the diagnostic log only" — and then did not put the
            // reason in the diagnostic log. `App.LastDiagnostic` is the ONE surface that
            // survives here (it renders into `Shell.StatusText`, which is UIA-readable on a
            // Release build), so a fallback that drops the sentence leaves nothing anywhere.
            App.LogDiagnostic($"{title} (no XamlRoot — log only): {reason}");
            return;
        }

        try
        {
            var dialog = new Dialogs.ProjectOutcomeDialog();
            await dialog.ShowRefusalAsync(root, title, reason);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"project refusal dialog failed: {ex.GetType().Name}: {ex.Message}");
        }
    }


    // ── save / save as ──────────────────────────────────────────────────────

    private void OnProjectSaveClick(object sender, RoutedEventArgs e) => _ = SaveAsync();

    private void OnProjectSaveAsClick(object sender, RoutedEventArgs e) => _ = SaveAsAsync();

    /// <summary>
    /// <c>rudis_save_project</c>. No picker and no dialog, deliberately: plan 60.1-02's
    /// backend MINTS an <c>Untitled</c> project when none is active, so Save always has
    /// somewhere to go and a beginner never meets a modal between their work and
    /// safety.
    ///
    /// <para>The <c>Ok</c> payload is an OBJECT carrying <c>path</c> and <c>seq</c>
    /// (plan 60.1-04's envelope table) — unlike the two prose-returning exports in this
    /// same family. Nothing here reads into it; the log keeps the raw envelope, and the
    /// TitleBar learns the name from the mirror.</para>
    ///
    /// <para>⚠ The returned <c>path</c> may be EITHER the plain form or the canonical
    /// extended-length one, depending on how the document was opened
    /// (<c>active_project_meta</c> holds whatever the route that set it produced). A
    /// host that compares it to anything must canonicalise BOTH sides; this one only
    /// logs it, which is why it can afford not to.</para>
    /// </summary>
    internal async Task SaveAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        try
        {
            var saved = await engine.SaveProjectAsync();
            if (saved.Kind == RudisResultKind.Ok)
            {
                App.LogDiagnostic($"save_project OK -> {saved.RawEnvelope}");
                return;
            }

            App.LogDiagnostic($"save_project REFUSED ({saved.Kind}/{saved.Status}): {saved.Error}");
            await ShowProjectRefusalAsync("Could not save the project", saved);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"save_project threw: {ex.GetType().Name}: {ex.Message}");
            await ShowProjectRefusalAsync("Could not save the project", ex);
        }
    }

    /// <summary>
    /// <c>rudis_save_project_as</c> — write the project to a file the user chooses, and
    /// re-point the active document at it.
    /// </summary>
    /// <returns><c>true</c> when a save actually happened; <c>false</c> for a cancelled
    /// picker, a refused target, or no engine.</returns>
    /// <remarks>
    /// <para><b><c>internal</c>, and returning a <c>bool</c>, on purpose.</b> The
    /// precedent is in this very file: <see cref="PickAndImportAsync"/> was widened from
    /// <c>private</c> to <c>internal</c> by plan 53-02 so <c>MediaBin</c> could reach the
    /// SAME routine instead of opening a second path to <c>rudis_import_media</c>, and
    /// there is still exactly one import routine in this shell. Save As gets the same
    /// treatment: any later region that needs it calls THIS, and the <c>bool</c> is what
    /// lets a caller distinguish "saved" from "the user backed out" without inspecting
    /// the engine.</para>
    ///
    /// <para>⚠ Plan 60.1-06 deliberately does NOT need this. Its close-time save calls
    /// <see cref="SaveAsync"/> directly, because the backend mints an <c>Untitled</c>
    /// project when none is active — a close path that opened a picker would be a modal
    /// between the user and quitting.</para>
    ///
    /// <para><b>Why a plain <c>FileSavePicker</c> here, when <c>ExportDialog</c> uses a
    /// FolderPicker plus a name box inside a ContentDialog.</b> That choice was about
    /// LAYERING: the export dialog renders in the <c>XamlRoot</c>'s popup layer, the
    /// same visual tree as the <c>Preview</c> region's <c>SwapChainPanel</c>, and it is
    /// the first of the three cases the retired <c>SetWindowRgn</c> hole-punch never
    /// covered (SC-1 / D-14) — so it had to be an in-app modal to prove anything. Save
    /// As has no <c>SwapChainPanel</c> interaction and no such constraint, so the
    /// simpler picker is legitimate. Saying which constraint does NOT apply is what
    /// stops a later reader "fixing" this back.</para>
    ///
    /// <para>⚠ <b>Nothing here parses or writes a <c>.rud</c></b> (CLAUDE.md rule 4).
    /// The shell sends a path; Rust owns the file. No default project is constructed, no
    /// existence check is made, and no canonicalisation happens beyond
    /// <c>Path.GetFullPath</c> — <c>RudSaveTargetPath</c> owns all of that and a second
    /// copy here would drift.</para>
    /// </remarks>
    internal async Task<bool> SaveAsAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid || _window is null)
        {
            return false;
        }

        string picked;
        try
        {
            var picker = new FileSavePicker(_window.AppWindow.Id)
            {
                CommitButtonText = "Save",
                SuggestedStartLocation = PickerLocationId.DocumentsLibrary,
                SuggestedFileName = SuggestedProjectFileName(),
            };
            picker.FileTypeChoices.Add("Rudis project", [ToolbarProjectRoutes.RudExtension]);

            var chosen = await picker.PickSaveFileAsync();
            if (chosen is null || string.IsNullOrEmpty(chosen.Path))
            {
                // Cancel is ordinary, not a refusal. Nothing is dispatched.
                return false;
            }
            picked = chosen.Path;
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"save-as picker failed: {ex.GetType().Name}: {ex.Message}");
            await ShowProjectRefusalAsync("Could not save the project there", ex);
            return false;
        }

        var folder = Path.GetDirectoryName(picked) ?? string.Empty;
        var fileName = Path.GetFileName(picked);

        // T-51-22, and ExportChoice.OutPath's recorded rule one surface over: an invalid
        // file-name character is REFUSED WITH A REASON, never sanitised. A path
        // separator smuggled into a file name writes outside the folder the user chose,
        // and a silently-rewritten name puts the project somewhere they did not ask for
        // and then reports success.
        var refusal = ToolbarProjectRoutes.RefuseSaveTargetName(fileName);
        if (refusal is not null)
        {
            App.LogDiagnostic($"save_project_as refused before dispatch: {refusal}");
            await ShowProjectRefusalAsync("Could not save the project there", refusal);
            return false;
        }

        // GetFullPath once, here, and Combine rather than concatenation (T-51-22).
        var target = Path.GetFullPath(
            Path.Combine(folder, ToolbarProjectRoutes.EnsureRudExtension(fileName)));

        try
        {
            var saved = await engine.SaveProjectAsAsync(ToolbarProjectRoutes.BuildPathArgs(target));
            if (saved.Kind == RudisResultKind.Ok)
            {
                // Nothing else to do on success: the backend emits ProjectSwitched, the
                // mirror full-resyncs, and the TitleBar picks up the new name for free.
                App.LogDiagnostic($"save_project_as OK -> {saved.RawEnvelope}");
                return true;
            }

            App.LogDiagnostic(
                $"save_project_as REFUSED ({saved.Kind}/{saved.Status}): {saved.Error}");
            await ShowProjectRefusalAsync("Could not save the project there", saved);
            return false;
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"save_project_as threw: {ex.GetType().Name}: {ex.Message}");
            await ShowProjectRefusalAsync("Could not save the project there", ex);
            return false;
        }
    }

    /// <summary>
    /// The Save As picker's pre-filled name, seeded from MIRRORED state — the active
    /// project's own name — never invented.
    ///
    /// <para>Falls back to <c>Untitled</c>, which is the same word the backend mints
    /// when no project is active (plan 60.1-02), so the two halves agree instead of
    /// offering the user two different defaults for the same situation.</para>
    /// </summary>
    private static string SuggestedProjectFileName()
    {
        var name = App.Mirror?.Project?.Name;
        return string.IsNullOrWhiteSpace(name) ? "Untitled" : name;
    }

    // ── undo / redo ─────────────────────────────────────────────────────────

    private void OnUndoClick(object sender, RoutedEventArgs e) => _ = UndoAsync();

    private void OnRedoClick(object sender, RoutedEventArgs e) => _ = RedoAsync();

    /// <summary>
    /// <c>rudis_undo</c>. Answers <c>{"Ok": null}</c> when there is nothing to undo,
    /// and that answer is surfaced rather than hidden.
    ///
    /// <para><b>v6 PARITY FINDING (read main.ts before changing this).</b> UI-SPEC §4
    /// says undo/redo enablement derives from <c>project:changed</c> "(history
    /// depth)". <b>There is no history depth to derive it from.</b>
    /// <c>Store::can_undo</c>/<c>can_redo</c> exist (<c>crates/core/src/store.rs:317-323</c>)
    /// but are exposed by NO ABI export, are absent from the <c>Project</c> snapshot,
    /// and are referenced only by Rust tests. The shipping frontend therefore leaves
    /// <c>#undo</c>/<c>#redo</c> <b>permanently enabled</b> — nothing in
    /// <c>frontend/src/main.ts</c> ever assigns <c>disabled</c> to either — and simply
    /// logs "nothing to undo" when the backend says so (main.ts:1518-1525).</para>
    ///
    /// <para>So this is a PORT: both buttons stay enabled, and the real backend answer
    /// is what the user learns. A greyed-out button derived from a guess would be the
    /// dishonest option. The disabled visual (<c>text-tertiary</c>) IS defined in the
    /// style so it is ready the day the ABI grows a history signal — which would be a
    /// Rust-side widening and its own planned change (D-15), not something to smuggle
    /// in here.</para>
    /// </summary>
    public async Task UndoAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }
        try
        {
            var undone = await engine.UndoAsync();
            App.LogDiagnostic(undone.Kind == RudisResultKind.Ok
                ? $"undo -> {undone.RawEnvelope}"
                : $"undo FAILED ({undone.Kind}/{undone.Status}): {undone.Error}");
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"undo threw: {ex.GetType().Name}: {ex.Message}");
        }
    }

    /// <summary><c>rudis_redo</c> — <see cref="UndoAsync"/>'s exact mirror, same
    /// enablement parity.</summary>
    public async Task RedoAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }
        try
        {
            var redone = await engine.RedoAsync();
            App.LogDiagnostic(redone.Kind == RudisResultKind.Ok
                ? $"redo -> {redone.RawEnvelope}"
                : $"redo FAILED ({redone.Kind}/{redone.Status}): {redone.Error}");
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"redo threw: {ex.GetType().Name}: {ex.Message}");
        }
    }

    // ── mode tabs: shell view-state only, no backend ─────────────────────────

    private void OnModeTabChecked(object sender, RoutedEventArgs e)
        => ModeChanged?.Invoke(ReferenceEquals(sender, ColorTab) ? "color" : "edit");

    // ── export ──────────────────────────────────────────────────────────────

    private void OnExportClick(object sender, RoutedEventArgs e) => _ = ExportAsync();

    /// <summary>
    /// <c>rudis_export_timeline</c> — the one command in this region that proves
    /// CLAUDE.md rule 3 ("verify on output"): it writes a real file to a real path the
    /// user chose, and the envelope carries the written path back.
    ///
    /// <para><b>Never on the UI thread</b> (T-50-21): the call is only reachable
    /// through <see cref="RudisNative.ExportTimelineAsync"/>, which posts to the one
    /// interop worker. The export blocks THAT thread for the complete encode, by design
    /// (<c>commands.rs:473-474</c>). This is structural, not a convention —
    /// <c>NativeMethods</c> is private to the wrapper.</para>
    ///
    /// <para>⚠ <b>KNOWN CONSEQUENCE, recorded rather than papered over</b>
    /// (50-02 §1.5(3)): while the encode holds the worker, <c>rudis_poll_events</c>
    /// cannot run, so <c>export:progress</c> records accumulate in the bounded ring
    /// and drain only after the export returns. The determinate fill is therefore
    /// correct but LATE — it does not animate smoothly during the encode. Live
    /// per-percent progress would need a second sanctioned poll-only thread, which
    /// 50-02 explicitly assigns to a later phase rather than to this one.</para>
    ///
    /// <para>Args shape read from <c>commands.rs:466-472</c>:
    /// <c>{"out_path": "..", "width": u32, "height": u32, "fps": f64}</c>.</para>
    ///
    /// <para><b>⚠ PLAN 51-06 REPLACED THE SHELL PICKER WITH AN IN-APP MODAL, and the
    /// reason is the phase's whole point.</b> Phase 50 used a
    /// <c>Microsoft.Windows.Storage.Pickers</c> save picker and derived the geometry
    /// from mirrored state without asking — a documented divergence from v6.0, which
    /// asks (resolution / frame rate / folder / file name) in its own
    /// <c>#export-dialog</c> modal. A shell picker lives in its OWN top-level window,
    /// so it can never overlap the <c>Preview</c> swapchain and proves nothing about
    /// layering; <see cref="Dialogs.ExportDialog"/> is a WinUI 3 <c>ContentDialog</c>
    /// in the <c>XamlRoot</c>'s popup layer — the same visual tree as the panel — which
    /// is the first of SC-1's three previously-broken cases.</para>
    ///
    /// <para>The encode itself is UNCHANGED: same <see cref="RudisNative.ExportTimelineAsync"/>,
    /// same args, same busy state, same <c>_exportInFlight</c> guard. "Source" in the
    /// dialog means <see cref="ResolveExportFormat"/> — the working Phase-50 behaviour
    /// is the DEFAULT, not something the dialog replaced.</para>
    /// </summary>
    public async Task ExportAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid || _window is null || _exportInFlight || !_timelineHasContent)
        {
            return;
        }

        var root = XamlRoot;
        if (root is null)
        {
            App.LogDiagnostic("export: no XamlRoot yet — the dialog cannot be shown");
            return;
        }

        ExportChoice? choice;
        try
        {
            var dialog = new ExportDialog();
            var (defaultFolder, defaultName) = ResolveExportDefaults();
            choice = await dialog.ShowAndCollectAsync(
                root, _window.AppWindow.Id, defaultFolder, defaultName);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"export dialog failed: {ex.GetType().Name}: {ex.Message}");
            return;
        }

        if (choice is null)
        {
            // Cancel / Esc / dismiss: the engine is not touched at all.
            App.LogDiagnostic("export: cancelled in the dialog, nothing dispatched");
            return;
        }

        var outPath = choice.OutPath;
        var (sourceWidth, sourceHeight, sourceFps) = ResolveExportFormat();
        var width = choice.Width ?? sourceWidth;
        var height = choice.Height ?? sourceHeight;
        var fps = choice.Fps ?? sourceFps;

        BeginExportBusy();
        App.LogDiagnostic($"export_timeline -> {outPath} ({width}x{height} @ {fps}fps)");

        var args = new JsonObject
        {
            ["out_path"] = outPath,
            ["width"] = width,
            ["height"] = height,
            ["fps"] = fps,
        };
        // Debug session `export-no-file-written` (2026-08-01): the outcome is
        // captured here and SURFACED below — before this, success and failure
        // were both invisible (the diagnostic ring is in-memory only), which is
        // how three real exports got reported as "no file written".
        string? writtenPath = null;
        string? failure = null;
        try
        {
            var exported = await engine.ExportTimelineAsync(args.ToJsonString());
            if (exported.Kind == RudisResultKind.Ok)
            {
                // The Ok payload is the engine's written path (a JSON string);
                // fall back to the requested out_path on an unexpected shape.
                writtenPath = exported.Value.ValueKind == JsonValueKind.String
                    ? exported.Value.GetString() ?? outPath
                    : outPath;
                App.LogDiagnostic($"export_timeline OK -> {exported.RawEnvelope}");
            }
            else
            {
                failure = exported.Error ?? $"{exported.Kind}/{exported.Status}";
                App.LogDiagnostic(
                    $"export_timeline FAILED ({exported.Kind}/{exported.Status}): {exported.Error}");
            }
        }
        catch (Exception ex)
        {
            failure = $"{ex.GetType().Name}: {ex.Message}";
            App.LogDiagnostic($"export_timeline threw: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            // The busy state must clear even on a fault, or the button stays disabled
            // for the rest of the session.
            EndExportBusy();
        }

        // The visible outcome (UI-SPEC §5's refusal-must-be-visible rule,
        // extended to completion): success names the REAL written path with a
        // Show-in-folder affordance; failure shows the engine's reason
        // verbatim. A dialog failure here is logged, never thrown — the export
        // itself already finished, and its result is in the diagnostic ring.
        try
        {
            var outcome = new Dialogs.ExportOutcomeDialog();
            await outcome.ShowOutcomeAsync(root, writtenPath, failure);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"export outcome dialog failed: {ex.GetType().Name}: {ex.Message}");
        }
    }

    /// <summary>
    /// Export geometry from MIRRORED state, never invented: the first video item the
    /// backend probed. Falls back to 1280x720@30 only when the bin carries no probed
    /// video dimensions, which is the same shape of default v6 uses
    /// (<c>main.ts</c>'s <c>resolveExportDimensions</c> plus its
    /// <c>playback.fps &gt; 0 ? … : 30</c>).
    /// </summary>
    private static (uint Width, uint Height, double Fps) ResolveExportFormat()
    {
        var project = App.Mirror?.Project;
        if (project is not null)
        {
            foreach (var item in project.MediaBin)
            {
                if (item.MediaKind == "video" && item.Width > 0 && item.Height > 0)
                {
                    return (item.Width, item.Height, item.Fps > 0 ? item.Fps : 30.0);
                }
            }
        }
        return (1280, 720, 30.0);
    }

    /// <summary>
    /// The dialog's pre-filled folder and file name, ported from v6's
    /// <c>openExportDialog</c> (main.ts) rather than invented.
    ///
    /// <para>Folder = the FIRST bin item's directory, and ONLY when its path is
    /// ABSOLUTE — v6's own comment records why: a relative source path (a bundled test
    /// clip) builds an invalid output directory and the export fails with os error 123.
    /// Blank is the honest answer there, and Browse… is one click away.</para>
    ///
    /// <para>Name = <c>&lt;source-stem&gt;-export</c>, falling back to
    /// <c>rudis-export</c> — the same string the Phase-50 save picker suggested.</para>
    /// </summary>
    private static (string? Folder, string? FileName) ResolveExportDefaults()
    {
        var source = App.Mirror?.Project?.MediaBin.FirstOrDefault()?.Path;
        if (string.IsNullOrWhiteSpace(source))
        {
            return (null, "rudis-export");
        }

        string? folder = null;
        try
        {
            // Debug session `export-no-file-written` (2026-08-01): the backend
            // canonicalizes imported media paths, which on Windows yields the
            // `\\?\C:\...` verbatim form — correct internally, unreadable in the
            // dialog's Folder box. Strip it for DISPLAY here, at the derivation
            // site, so everything downstream (the dialog, the confirmed
            // out_path, the outcome dialog) carries the plain form.
            var display = Dialogs.PathDisplay.ToDisplay(source);
            if (Path.IsPathRooted(display))
            {
                var dir = Path.GetDirectoryName(display);
                if (!string.IsNullOrEmpty(dir) && Directory.Exists(dir))
                {
                    folder = dir;
                }
            }
        }
        catch (ArgumentException)
        {
            // A path the framework refuses to parse is simply not a default.
            folder = null;
        }

        var stem = Path.GetFileNameWithoutExtension(source);
        var name = string.IsNullOrWhiteSpace(stem) ? "rudis-export" : stem + "-export";
        return (folder, name);
    }

    private void BeginExportBusy()
    {
        _exportInFlight = true;
        ExportButton.IsEnabled = false;
        SetExportProgress(0);
    }

    private void EndExportBusy()
    {
        _exportInFlight = false;
        ExportBusyFill.Width = 0;
        ExportLabel.Text = "Export ▾";
        AutomationProperties.SetName(ExportButton, "Export");
        SetExportEnabled(_timelineHasContent);
    }

    /// <summary>
    /// Enable/disable Toolbar.ExportButton and dim its label with it.
    ///
    /// <para>The label lives in the button's CONTENT rather than its template (so the
    /// busy fill can span the button), which means the template's Disabled visual state
    /// cannot reach it — the fill greys out but white-on-grey text would stay. UI-SPEC
    /// GAP 4 (the handoff gives Export a busy state but no disabled fill) is resolved
    /// as <c>bg-elevated</c> + <c>text-tertiary</c>, matching every other disabled
    /// control in the bar; this is the half of that resolution the template cannot
    /// express.</para>
    /// </summary>
    private void SetExportEnabled(bool enabled)
    {
        ExportButton.IsEnabled = enabled;
        ExportLabel.Foreground = (Brush)Application.Current.Resources[enabled ? "on-accent" : "text-tertiary"];
    }

    /// <summary>
    /// The busy state (UI-SPEC §5 "busy" row): determinate fill + "Exporting… N%",
    /// self-disabled, and the UIA <c>Name</c> becomes "Exporting, N percent" so
    /// SHELL-07 can assert progress without reading pixels (UI-SPEC §6).
    /// </summary>
    public void SetExportProgress(double percent)
    {
        if (!_exportInFlight)
        {
            return;
        }
        var clamped = Math.Clamp(percent, 0, 100);
        var rounded = (int)Math.Round(clamped);
        ExportBusyFill.Width = ExportButton.ActualWidth * clamped / 100.0;
        ExportLabel.Text = string.Create(
            CultureInfo.InvariantCulture, $"Exporting… {rounded}%");
        AutomationProperties.SetName(
            ExportButton, string.Create(CultureInfo.InvariantCulture, $"Exporting, {rounded} percent"));
    }

    // ── mirror-driven enablement (cold path, UI-SPEC §4) ────────────────────

    /// <summary>
    /// Re-derive enablement from mirrored state. Called on every
    /// <c>project:changed</c> (including resyncs) by <c>MainWindow</c>.
    ///
    /// <para><c>Toolbar.ExportButton</c> is enabled only when the TIMELINE holds at
    /// least one clip — derivable, honestly, from the mirror. This is a DELIBERATE
    /// divergence from v6, where the export button is never disabled: exporting an
    /// empty timeline cannot succeed, so a disabled button is more truthful than a
    /// button that fails. Recorded in artifacts/50-05-regions.md.</para>
    ///
    /// <para>Import/undo/redo stay enabled — see <see cref="UndoAsync"/>'s parity
    /// finding.</para>
    /// </summary>
    internal void ApplyProjectState(ShellMirror mirror)
    {
        var clips = 0;
        var project = mirror.Project;
        if (project is not null)
        {
            foreach (var track in project.Timeline.Tracks)
            {
                clips += track.Clips.Count;
            }
        }
        _timelineHasContent = clips > 0;
        if (!_exportInFlight)
        {
            SetExportEnabled(_timelineHasContent);
        }
    }

    // ── responsive layout (handoff:91 / UI-SPEC §2) ──────────────────

    /// <summary>At or above this width undo/redo sit inline; below it they collapse
    /// into the overflow. The handoff says undo/redo collapse "at narrow widths"
    /// without giving a number, so 760 is THIS PHASE'S choice, recorded in
    /// artifacts/50-05-regions.md rather than silently picked.</summary>
    private const double InlineUndoRedoBreakpoint = 760;

    /// <summary>Below this width the mode-tab labels become icons. This number IS the
    /// handoff's ("mode tabs keep labels until &lt; ~520px then become icons").</summary>
    private const double ModeTabLabelBreakpoint = 520;

    /// <summary>Icon-mode glyphs, rendered in <c>SymbolThemeFontFamily</c>:
    /// <c>E70F</c> (Edit) and <c>E790</c> (Color).
    ///
    /// <para>⚠ Two rendering traps, both MEASURED (artifacts/50-05-regions.md), which
    /// is why these are icon-font codepoints paired with an explicit family swap rather
    /// than plain Unicode:</para>
    /// <list type="bullet">
    /// <item>A PUA codepoint has NO DirectWrite font fallback, so pinning a family that
    ///   is absent renders NOTHING at all. <c>SymbolThemeFontFamily</c> is used because
    ///   it was proven to resolve to a Chrome*/icon-carrying face on Windows 10 19045,
    ///   which ships no Segoe Fluent Icons.</item>
    /// <item>Plain BMP pictographs were tried first (U+270E pencil, U+25D0 half
    ///   circle). They render — but the pencil falls back to Segoe UI Emoji and comes
    ///   out as a COLOUR emoji, which is wrong for this UI. Icon fonts are monochrome
    ///   by construction.</item>
    /// </list>
    /// The handoff says "become icons" without naming them, so the two codepoints are
    /// this phase's choice.</summary>
    private const string EditIconGlyph = "";

    private const string ColorIconGlyph = "";

    private static FontFamily IconFont =>
        (FontFamily)Application.Current.Resources["SymbolThemeFontFamily"];

    private static FontFamily LabelFont => new("Calibri");

    private void OnToolbarSizeChanged(object sender, SizeChangedEventArgs e)
        => ApplyResponsiveLayout(e.NewSize.Width);

    /// <summary>
    /// The two named breakpoints, applied from a MEASURED width.
    ///
    /// <para>⚠ <c>AutomationProperties.Name</c> is NOT touched here. Both segments keep
    /// "Edit" / "Color" at every width, so SHELL-07's UIA harness (plan 50-08) can
    /// identify them regardless of how wide the window happens to be — a UAT that
    /// breaks when the window narrows would be worse than no UAT.</para>
    /// </summary>
    private void ApplyResponsiveLayout(double width)
    {
        var inlineUndoRedo = width >= InlineUndoRedoBreakpoint;
        UndoRedoInline.Visibility = inlineUndoRedo ? Visibility.Visible : Visibility.Collapsed;
        OverflowButton.Visibility = inlineUndoRedo ? Visibility.Collapsed : Visibility.Visible;

        var showLabels = width >= ModeTabLabelBreakpoint;
        var font = showLabels ? LabelFont : IconFont;
        EditTab.Content = showLabels ? "Edit" : EditIconGlyph;
        EditTab.FontFamily = font;
        ColorTab.Content = showLabels ? "Color" : ColorIconGlyph;
        ColorTab.FontFamily = font;
    }

    // ── keyboard accelerators (UI-SPEC §3) ──────────────────────────────────

    private void OnImportAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        _ = PickAndImportAsync();
    }

    private void OnUndoAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        _ = UndoAsync();
    }

    private void OnRedoAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        _ = RedoAsync();
    }

    /// <summary>
    /// Asks <c>MainWindow</c> to open Settings (Phase 69, D-69-10) — <c>Ctrl+,</c>. Routes to
    /// <c>MainWindow.ShowSettingsAsync</c>, the same guarded entry as the TitleBar app menu
    /// and <c>Chat.KeyButton</c>. Null until installed; the accelerator then does nothing.
    /// </summary>
    public Action? RequestSettings { get; set; }

    private void OnSettingsAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        RequestSettings?.Invoke();
    }

    private void OnExportAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        _ = ExportAsync();
    }

    private void OnProjectNewAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        _ = NewProjectAsync();
    }

    private void OnProjectOpenAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        _ = OpenProjectAsync();
    }

    private void OnProjectSaveAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        _ = SaveAsync();
    }
}
