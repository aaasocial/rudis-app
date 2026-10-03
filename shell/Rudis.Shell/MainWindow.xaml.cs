using System.Globalization;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;
using Rudis.Shell.Interop;
using Rudis.Shell.Mirror;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;

namespace Rudis.Shell;

/// <summary>
/// The production shell's main window. Regions (TitleBar/Toolbar/Transport) land in
/// plans 50-05..50-07; plan 50-03 proved the hosting pattern and plan 50-04 makes it
/// carry real, sequenced backend state through the two-rate polling of D-06.
///
/// <para><b>TWO RATES, matching the ABI's own two-path design (D-06):</b></para>
/// <list type="number">
/// <item><b>HOT</b> — a <see cref="DispatcherQueueTimer"/> at frame cadence
///   (<see cref="HotTickIntervalMs"/>ms) reads <c>rudis_get_playback_position()</c>,
///   one scalar per tick. That export is a pure Relaxed atomic load and is the ONE
///   free-threaded member of the wrapper (50-02 §1.5(1)), so this needs no queue and
///   no event mechanism at all (v7-ARCHITECTURE:101). Deliberately NOT over-built.
///   (Originally a <c>CompositionTarget.Rendering</c> subscription; changed by the
///   GATE-02 debug session 2026-08-02 because that event's per-raise args marshaling
///   was itself a ~60/s finalizable-wrapper churn — see <c>StartHotPoll</c>.)</item>
/// <item><b>COLD</b> — a <see cref="DispatcherQueueTimer"/> at
///   <see cref="ColdPollIntervalMs"/>ms drives <c>rudis_poll_events</c>. Structural
///   changes move at user/agent cadence, not per-frame; polling this per frame would
///   burn CPU for no responsiveness gain, and polling the playhead at 100ms would
///   visibly stutter the transport readout.</item>
/// </list>
///
/// <para><b>Thread discipline (50-02 §1.5, T-50-16):</b> the cold cycle is split in
/// two halves on purpose. <see cref="ShellMirror.FetchPollAsync"/> is awaited with
/// <c>ConfigureAwait(false)</c>, so the ABI call, the buffer copy, the single native
/// free and BOTH JSON parses happen on the interop worker / thread pool — never on
/// the UI thread. Only the already-parsed batch is marshalled back via
/// <c>DispatcherQueue.TryEnqueue</c>, and the mirror is mutated exclusively there, so
/// regions can bind to it without locks.</para>
/// </summary>
public sealed partial class MainWindow : Window
{
    /// <summary>CONTEXT D-06 leaves the interval to discretion and names 100ms as the
    /// default; that is what this ships.</summary>
    private const int ColdPollIntervalMs = 100;

    /// <summary>The hot tick's cadence: nominal 16ms ≈ the 60Hz frame rate the old
    /// <c>CompositionTarget.Rendering</c> subscription delivered (Windows timer
    /// resolution makes the effective period ~15.6–16ms). See
    /// <see cref="StartHotPoll"/> for why this is a timer at all.</summary>
    private const int HotTickIntervalMs = 16;

    private readonly DispatcherQueue _dispatcher;

    private DispatcherQueueTimer? _coldTimer;

    /// <summary>The HOT tick's driver (debug session
    /// <c>gate02-present-thread-gc-collection-count</c>, 2026-08-02). A
    /// <see cref="DispatcherQueueTimer"/> at frame cadence, deliberately NOT a
    /// <c>CompositionTarget.Rendering</c> subscription — see <see cref="StartHotPoll"/>
    /// for the measured reason.</summary>
    private DispatcherQueueTimer? _hotTimer;

    /// <summary>T-50-18: at most ONE poll cycle outstanding. Touched ONLY on the UI
    /// thread (the timer tick sets it, the marshalled apply clears it), so no
    /// interlocking is needed — and stacked polls under a slow parse are impossible
    /// by construction rather than by hope.</summary>
    private bool _pollInFlight;

    /// <summary>A poll was requested while one was already running — see
    /// <see cref="RequestImmediatePoll"/>. UI-thread-only state.</summary>
    private bool _pollAgainRequested;

    private bool _closed;

    /// <summary>The close funnel's re-entrancy latch — see
    /// <see cref="RequestCloseAsync"/>. Set on entry and never cleared: a close, once
    /// asked for, is not cancellable in this shell. UI-thread-only state.</summary>
    private bool _closing;

    // ── HOT PATH state. A plain counter, incremented once per tick; the position
    //    itself lives in Transport's PlayheadTicker (plan 50-06). ──
    private long _renderTicks;

    // ── D-01's Canvas/Chat divider. UI-thread-only state; NOTHING here is persisted
    //    (D-03 — every launch is 50/50, and a splitter position is not project data).
    //    The pure math lives in Regions/Chat/ChatSplit.cs and is unit-pinned by 18
    //    cases with no window; what lives HERE is only the raw pointer plumbing, the
    //    TimelineInteraction split this codebase already uses for pointer geometry. ──
    private bool _splitDragging;
    private double _splitStartY;
    private double _splitStartTopPx;
    private double _splitTotalPx;

    public MainWindow()
    {
        InitializeComponent();
        Title = "Rudis";
        _dispatcher = DispatcherQueue.GetForCurrentThread();

        // TitleBar owns the window's custom chrome (plan 50-05 Task 1). Attached
        // BEFORE Activate() so the system title bar never flashes.
        TitleBarRegion.AttachToWindow(this);
        Activated += OnWindowActivated;

        // Toolbar needs the window only for its file pickers (WindowId).
        ToolbarRegion.AttachToWindow(this);
        ToolbarRegion.ModeChanged += OnWorkspaceModeChanged;

        // Settings (Phase 69, D-69-10): THREE entry points, ONE guarded entry. The TitleBar
        // app menu (Settings...), Ctrl+, (registered on the Toolbar root, window-scoped) and
        // Chat.KeyButton all ask; ShowSettingsAsync is the only thing that opens it.
        TitleBarRegion.RequestSettings = () => _ = ShowSettingsAsync();
        ToolbarRegion.RequestSettings = () => _ = ShowSettingsAsync();
        ChatRegion.RequestSettings = () => _ = ShowSettingsAsync();

        // Canvas → Preview, wired HERE rather than region-to-region: the Canvas owns
        // the tool palette, the Preview owns the ink layer over its own swapchain,
        // and the window is the one place that knows about both. Selecting a drawing
        // tool is what makes that layer hit-testable at all (D-11).
        CanvasRegion.ToolChanged += OnCanvasToolChanged;

        // MediaBin needs the window only for plan 53-03's folder picker (WindowId).
        MediaBinRegion.AttachToWindow(this);

        // Chat → Timeline, wired HERE for the same reason Canvas → Preview is: the
        // window is the one place that knows about both regions. This is the WHOLE of
        // what the Chat reads from the Timeline — one read-only clip id, which
        // `ChatCommandPayloads.SendMessage` turns into the turn's `selection` argument
        // (a one-element array, or empty — v6.0's main.ts:2208 parity). The Chat never
        // reaches into Timeline internals beyond this, and it holds no mirrored state of
        // its own, so it takes no ProjectChanged subscription (CLAUDE.md rule 4).
        ChatRegion.SelectionProvider = () => TimelineRegion.SelectedClipId;

        // Preview device-lost recovery → Timeline (Phase 71, TRUST-01), wired HERE for the
        // same reason: the window is the one place that knows both regions. D3D12 devices
        // are singletons per adapter, so the Timeline's surface must be released for the
        // preview's recovery to find the hardware adapter, and re-attached afterwards —
        // on failure too, so the Timeline is never left blank. Both run on the UI thread.
        PreviewRegion.BeforeDeviceRecovery = () => TimelineRegion.SuspendSurface();
        PreviewRegion.AfterDeviceRecovery = _ => TimelineRegion.ResumeSurface();

        // The region owns the AFFORDANCE; the shell keeps exactly ONE import routine and
        // it stays Toolbar's. Routing the event here rather than letting the MediaBin
        // call rudis_import_media itself is what stops a second import path existing.
        MediaBinRegion.ImportMediaRequested += () => _ = ToolbarRegion.PickAndImportAsync();

        // D-15's other half needs NO line here any more, and that asymmetry with the
        // import route above is deliberate. `MediaBin.ImportFolderButton` now calls
        // `rudis_import_media_folder` itself (plan 53-03), because unlike file import —
        // which has exactly one routine and it is Toolbar's — folder import has no other
        // owner in the shell: the MediaBin IS its owner. Routing it through the window
        // would be ceremony around a single caller. The placeholder subscription plan
        // 53-02 left here was explicitly "the line 53-03 replaces"; it is replaced by
        // the real call, one region over.

        ApplyWindowIcon();

        // ONE close path, TWO entry points into it (plan 60.1-06), wired together HERE
        // so the pair is visible in one place rather than one line per region.
        //
        //   * `TitleBar.RequestClose` - the close button this shell DRAWS. It is not a
        //     system caption button, so it never raises `AppWindow.Closing`: the old
        //     body called `AppWindow.Destroy()`, and Microsoft Learn is explicit that
        //     Destroy does not raise Closing. A save hung only on Closing would have
        //     worked for Alt+F4 and silently skipped the button every user clicks.
        //   * `AppWindow.Closing` - Alt+F4, the system menu, a taskbar close.
        //
        // Both reach `RequestCloseAsync`, which saves, releases the GPU surface (plan
        // 52-01's rule, found by crashing twice on real hardware: a swapchain bound to
        // a XAML panel must be unbound while the window is still ALIVE) and only then
        // calls `Close()`. NOT `AppWindow.Destroy()`: measured on the pre-plan build,
        // Destroy makes the window vanish without ever raising `Window.Closed`, so the
        // teardown never runs and the process is orphaned. See RequestCloseAsync.
        // `Closed` stays TEARDOWN-only - it fires while the window is already being
        // destroyed, which is too late to save and too late to unbind.
        TitleBarRegion.RequestClose = () => _ = RequestCloseAsync();
        AppWindow.Closing += OnAppWindowClosing;
        Closed += OnWindowClosed;
        _ = StartAsync();
    }

    /// <summary>Re-entrancy guard for <see cref="ShowSettingsAsync"/>.</summary>
    private bool _settingsOpen;

    /// <summary>
    /// The ONE way Settings opens (TitleBar menu, <c>Ctrl+,</c> and <c>Chat.KeyButton</c> all
    /// route here). Re-entrancy-guarded: WinUI throws if a second <c>ContentDialog</c> is
    /// shown while one is open (T-69-25), so a repeat request is dropped and any failure is
    /// logged. After the dialog closes the Chat status is re-read so the pill and
    /// <c>Chat.AgentStatus</c> reflect any key change.
    /// </summary>
    internal async Task ShowSettingsAsync()
    {
        if (_settingsOpen)
        {
            return;
        }

        var root = Content?.XamlRoot;
        if (root is null)
        {
            App.LogDiagnostic("settings: no XamlRoot — the dialog cannot be shown");
            return;
        }

        _settingsOpen = true;
        try
        {
            await new Dialogs.SettingsDialog().ShowSettingsAsync(root);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"settings dialog failed: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            _settingsOpen = false;
        }

        _ = ChatRegion.RefreshStatusAsync();
    }

    /// <summary>The exe's icon, kept alive for the process lifetime — see
    /// <see cref="ApplyWindowIcon"/>.</summary>
    private static IntPtr _windowIcon = IntPtr.Zero;

    [System.Runtime.InteropServices.DllImport("shell32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
    private static extern uint ExtractIconExW(string file, int index, out IntPtr large, out IntPtr small, uint count);

    /// <summary>
    /// Point the window at the exe's own icon resource, so the taskbar button, Alt-Tab and
    /// the window's system menu draw the Rudis mark.
    ///
    /// <para><b>Why this is not automatic.</b> <c>&lt;ApplicationIcon&gt;</c> gives the exe an
    /// <c>RT_GROUP_ICON</c>, which is what Explorer and a Desktop shortcut read — but an
    /// unpackaged WinUI 3 window ships with no icon of its own and no MSIX manifest to
    /// declare one, so it answers <c>WM_GETICON</c> with 0 and its class icon is 0 too. The
    /// shell then falls back to a generic placeholder while the app is RUNNING, even though
    /// the file on disk shows the right icon. Measured on this window before this call
    /// existed: <c>WM_GETICON big=0 small=0 classIcon=0</c>.</para>
    ///
    /// <para>Sourced from the running exe rather than a shipped <c>.ico</c> file: the icon is
    /// already embedded, so there is no second copy to stage, go missing, or drift out of
    /// sync with the one the shortcut draws.</para>
    ///
    /// <para>Best-effort by design. A window with the generic icon is a cosmetic defect; a
    /// window that fails to open is not, so nothing here is allowed to throw.</para>
    /// </summary>
    private void ApplyWindowIcon()
    {
        try
        {
            var exePath = Environment.ProcessPath;
            if (string.IsNullOrEmpty(exePath))
            {
                return;
            }

            if (_windowIcon == IntPtr.Zero)
            {
                // The HICON is deliberately never destroyed: AppWindow keeps using it for as
                // long as the window lives, and the window lives as long as the process. One
                // handle held for the process lifetime is the intended trade, not a leak to
                // be "fixed" with a DestroyIcon that would blank the taskbar button.
                _ = ExtractIconExW(exePath, 0, out var large, out var small, 1);
                _windowIcon = large != IntPtr.Zero ? large : small;
            }

            if (_windowIcon != IntPtr.Zero)
            {
                AppWindow.SetIcon(Microsoft.UI.Win32Interop.GetIconIdFromIcon(_windowIcon));
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"window icon: not applied ({ex.GetType().Name}) - cosmetic only");
        }
    }

    /// <summary>Inactive-window dimming (UI-SPEC §5 row 2). The state enum's
    /// <c>Deactivated</c> member is the only "blurred" value; both activated values
    /// (code-activated and pointer-activated) mean active.</summary>
    private void OnWindowActivated(object sender, WindowActivatedEventArgs args)
        => TitleBarRegion.SetWindowActive(
            args.WindowActivationState != WindowActivationState.Deactivated);

    /// <summary>
    /// Startup order, and the order matters: engine (created in
    /// <see cref="App.OnLaunched"/>) → the ABI probe render (50-03's launch proof,
    /// kept) → ONE full baseline learn → both rates. The baseline is learned BEFORE
    /// either rate starts so no poll can ever apply a patch against an unlearned
    /// baseline and take a spurious first resync.
    /// </summary>
    private async Task StartAsync()
    {
        var engine = App.Engine;
        var mirror = App.Mirror;
        if (engine is null || engine.IsInvalid || mirror is null)
        {
            StatusText.Text =
                "engine init failed: rudis_init returned null (a designed outcome " +
                "for malformed InitConfig JSON — lib.rs:191-201). No engine instance.";
            return;
        }

        var probe = await engine.AbiProbeAsync();
        var probeLine = probe.Kind switch
        {
            RudisResultKind.Ok => $"rudis_abi_probe envelope: {probe.RawEnvelope}",
            RudisResultKind.DomainError => $"probe domain error: {probe.Error}",
            _ => $"probe transport fault: {probe.Status} — {probe.Error}",
        };
        App.LogDiagnostic(probeLine);
        StatusText.Text = probeLine;

        // SHELL-02's baseline: rudis_get_snapshot + rudis_get_current_seq, adopting
        // the ring's next_seq as the poll cursor.
        await mirror.AttachAsync();

        // Regions bind to these (plan 50-04's pattern: a region NEVER polls the ABI
        // itself). They also keep the status readout honest, which is what makes
        // "both rates are live" checkable at launch.
        //
        // Plan 71-03. A `project_switched` patch routes to a full resync, and the resync's
        // ProjectChanged no longer knows why it happened — so the mirror announces the
        // switch first, and the Transport's session cache tally restarts BEFORE the region
        // ApplyMirrorState calls below re-render from the new document's snapshot.
        mirror.ProjectSwitched += () =>
        {
            TransportRegion.OnProjectSwitched();
        };
        mirror.ProjectChanged += _ =>
        {
            RefreshTitleBar(mirror);
            ToolbarRegion.ApplyProjectState(mirror);
            // Transport needs BOTH events: enablement and the clip-edge list come from
            // the project, the glyph/loop/duration from the playback (UI-SPEC §4).
            TransportRegion.ApplyMirrorState(mirror);
            // Timeline needs only the PROJECT: its lanes, clips and empty state are a
            // pure function of the mirrored timeline (plan 52-06). The playhead is
            // NOT pushed here - it rides the hot tick below, from the value
            // PlayheadTicker has already read.
            TimelineRegion.ApplyMirrorState(mirror);
            // MediaBin needs only the PROJECT too: the level on screen is a pure function
            // of media_bin + media_folders + the region's own drilled path (plan 53-02).
            MediaBinRegion.ApplyMirrorState(mirror);
            // The Canvas STAGE renders committed whiteboard marks as a pure function of
            // the mirrored canvas.annotations (plan 60.2-02) - which is what makes an
            // undo visibly remove one. frame_linked marks stay the ENGINE's to
            // composite (T-51-13) and are filtered out in the pure parser.
            CanvasRegion.ApplyMirrorState(mirror);
            // Preview needs only the two monitor facts - which one is active, and
            // whether a Source clip is loaded (plan 52-13). It is NOT on the frame path:
            // the swapchain is Rust's and no mirror event ever touches it.
            PreviewRegion.ApplyMirrorState(mirror);
            RefreshStatus(probeLine, mirror);
        };
        mirror.PlaybackChanged += _ =>
        {
            TransportRegion.ApplyMirrorState(mirror);
            // BOTH events for Preview, and neither is redundant: `preview_mode` and the
            // loaded source id are project state, but a `load_preview` issued by another
            // region applies its Playback immediately and announces only
            // `playback:changed` (ShellMirror.NotePreviewMode's recorded FINDING).
            PreviewRegion.ApplyMirrorState(mirror);
            RefreshStatus(probeLine, mirror);
        };
        mirror.ExportProgressChanged += pct =>
        {
            ToolbarRegion.SetExportProgress(pct);
            RefreshStatus(probeLine, mirror);
        };

        RefreshTitleBar(mirror);
        ToolbarRegion.ApplyProjectState(mirror);
        TransportRegion.ApplyMirrorState(mirror);
        TimelineRegion.ApplyMirrorState(mirror);
        // ⚠ BOTH call sites, not one. The subscription above keeps the region correct
        // AFTER the first poll; this initial push is what makes it correct AT LAUNCH,
        // before any poll has run. Phase 50's own regions do both, and a region wired
        // only to the subscription renders nothing until the first project:changed.
        MediaBinRegion.ApplyMirrorState(mirror);
        CanvasRegion.ApplyMirrorState(mirror);
        PreviewRegion.ApplyMirrorState(mirror);

        // v6.0's startup `refreshChatStatus()` (main.ts:2470). Fire-and-forget on
        // purpose — the region carries its own total try/catch and treats ANY failure as
        // disconnected, so nothing here can fault startup, and awaiting a keychain read
        // would delay both poll rates behind it. WITHOUT this line the pill and the key
        // button keep their markup defaults (`disconnected` / `Reconnect`) forever, no
        // matter what key is actually configured.
        _ = ChatRegion.RefreshStatusAsync();

        StartColdPoll(mirror, probeLine);
        StartHotPoll();
        RefreshStatus(probeLine, mirror);

        // Open-with-file / CLI import (plan 50-08's UIA route too). Runs AFTER attach
        // so the resulting project:changed lands on a learned baseline instead of
        // forcing a first-poll resync.
        if (App.StartupImportPaths.Count > 0)
        {
            App.LogDiagnostic($"--import: {App.StartupImportPaths.Count} path(s) from the command line");
            await ToolbarRegion.ImportPathsAsync(App.StartupImportPaths, loadPreviewOnFirst: true);
        }

#if DEBUG
        // Plan 54-05. The Debug-only synthetic-cards hook — see App.SynthChatCards for
        // why it exists (rendering a REAL option card needs a paid model turn, and a UIA
        // test must never spend money) and Chat.RenderSyntheticCards for what it renders.
        // The flag's own literal is deliberately NOT repeated here: it is defined once,
        // in App.xaml.cs, which is the single place the Release-absence proof points at.
        if (App.SynthChatCards)
        {
            ChatRegion.RenderSyntheticCards();
        }

        // Plan 53-06. The Debug-only folder-import entry point — see
        // App.StartupImportFolderPath for why it exists and why it is not a second
        // implementation. It calls the SAME routine `+ Import folder…` runs once its
        // picker has returned a path, so this launch route and the button cannot diverge.
        //
        // The flag's own literal is deliberately NOT repeated here: it is defined once, in
        // App.xaml.cs, which is the single place the Release-absence proof points at.
        if (App.StartupImportFolderPath is { Length: > 0 } startupFolder)
        {
            App.LogDiagnostic($"startup folder import: '{startupFolder}' from the command line");
            await MediaBinRegion.ImportFolderAsync(startupFolder);
        }

        if (App.StartupPlaceOnTimeline)
        {
            await PlaceImportedOnTimelineAsync(engine, mirror);
        }

        if (App.StartupDetachAudio)
        {
            await DetachFirstAudioBearingClipAsync(engine, mirror);
        }

        if (App.ShowRemoveTrackDialogOnLaunch)
        {
            await ShowRemoveTrackDialogOnLaunchAsync(mirror);
        }

        if (App.StartupSynthesizeClipCount > 0)
        {
            await SynthesizeClipsAsync(engine, mirror, App.StartupSynthesizeClipCount);
        }
#endif
    }

#if DEBUG
    /// <summary>
    /// <c>--synth-clips N</c>'s implementation (plan 52-09): place N clips across the
    /// project's two lanes through the REAL <c>rudis_place_clip</c>, so criterion 3's
    /// 1,000-clip measurement has a 1,000-clip project.
    ///
    /// <para><b>Real commands, not a hand-written store file.</b> Every clip here is
    /// created by the same backend command a MediaBin drop will use, so what gets
    /// measured is a project the domain itself produced — not a JSON file whose shape a
    /// test author guessed and which would drift silently the first time the schema
    /// moved. It costs a generation loop; it buys the measurement being about the real
    /// thing (CLAUDE.md rule 1).</para>
    ///
    /// <para><b>Lane assignment follows the domain's rule, not a guess.</b>
    /// <c>run_place_clip</c> refuses a media/track-kind mismatch outright, so the video
    /// item goes on the first VIDEO track and the audio item on the first AUDIO track,
    /// both resolved from the mirror. If only one kind was imported, every clip lands
    /// on that kind's lane and the run is still valid — it is just single-laned, which
    /// the log line says.</para>
    ///
    /// <para>Clips are laid end to end with a 1ms gap so no two ever overlap: an
    /// overlapping place is a legitimate domain refusal and would silently produce a
    /// smaller project than the artifact claims.</para>
    ///
    /// <para>Debug-only, argv-gated, and pointed at an isolated store — see
    /// <see cref="App.StartupSynthesizeClipCount"/>.</para>
    /// </summary>
    private static async Task SynthesizeClipsAsync(RudisNative engine, ShellMirror mirror, int count)
    {
        // The mirror is a poll behind the import (D-06) — the same wait
        // PlaceImportedOnTimelineAsync already performs, for the same reason.
        List<MediaBinItem>? bin = null;
        for (var attempt = 0; attempt < 60; attempt++)
        {
            bin = mirror.Project?.MediaBin;
            if (bin is { Count: > 0 })
            {
                break;
            }

            await Task.Delay(100);
        }

        if (bin is null || bin.Count == 0)
        {
            App.LogDiagnostic("--synth-clips: nothing in the media bin to place");
            return;
        }

        var videoTrack = FindTrackIndex(mirror, "video");
        var audioTrack = FindTrackIndex(mirror, "audio");

        MediaBinItem? videoItem = null;
        MediaBinItem? audioItem = null;
        foreach (var item in bin)
        {
            var isAudioOnly = string.Equals(item.MediaKind, "audio", StringComparison.OrdinalIgnoreCase);
            if (isAudioOnly)
            {
                audioItem ??= item;
            }
            else
            {
                videoItem ??= item;
            }
        }

        var lanes = new List<(int Track, MediaBinItem Item)>(2);
        if (videoItem is not null && videoTrack >= 0)
        {
            lanes.Add((videoTrack, videoItem));
        }

        if (audioItem is not null && audioTrack >= 0)
        {
            lanes.Add((audioTrack, audioItem));
        }

        if (lanes.Count == 0)
        {
            App.LogDiagnostic(
                "--synth-clips: no (media kind, track kind) pair the domain would accept — " +
                "nothing placed");
            return;
        }

        var nextStartUs = new long[lanes.Count];
        var placed = 0;
        var refused = 0;
        var started = System.Diagnostics.Stopwatch.StartNew();

        for (var i = 0; i < count; i++)
        {
            var lane = i % lanes.Count;
            var (track, item) = lanes[lane];
            var startUs = nextStartUs[lane];

            var args = new System.Text.Json.Nodes.JsonObject
            {
                ["media_id"] = item.Id,
                ["track"] = track,
                ["start_us"] = startUs,
            };

            var result = await engine.PlaceClipAsync(args.ToJsonString());
            if (result.Kind != RudisResultKind.Ok)
            {
                refused++;
                if (refused <= 3)
                {
                    App.LogDiagnostic(
                        $"--synth-clips: place_clip refused '{item.Id}' on track {track} at " +
                        $"{startUs}us ({result.Kind}/{result.Status}): {result.Error}");
                }

                continue;
            }

            // +1ms so consecutive clips touch but never overlap.
            nextStartUs[lane] = startUs + Math.Max(1_000, item.DurationUs) + 1_000;
            placed++;
        }

        started.Stop();
        App.LogDiagnostic(
            $"--synth-clips: placed {placed} of {count} clip(s) across {lanes.Count} lane(s) " +
            $"in {started.ElapsedMilliseconds}ms ({refused} refused)");
    }

    /// <summary>The first mirrored track of a given kind, or -1. Resolved from the
    /// mirror rather than assumed to be 0/1, because a project the harness did not
    /// create may not be laid out that way.</summary>
    private static int FindTrackIndex(ShellMirror mirror, string kind)
    {
        var tracks = mirror.Project?.Timeline?.Tracks;
        if (tracks is null)
        {
            return -1;
        }

        for (var i = 0; i < tracks.Count; i++)
        {
            if (string.Equals(tracks[i].Kind, kind, StringComparison.OrdinalIgnoreCase))
            {
                return i;
            }
        }

        return -1;
    }
#endif

#if DEBUG
    /// <summary>
    /// Dispatch the REAL <c>DetachAudio</c> for the first video-track clip whose media
    /// has audio, so 52-CONTEXT D-20's rule is visible on screen (plan 52-08).
    ///
    /// <para>The rule under test: a waveform fill follows
    /// <c>has_audio &amp;&amp; !audio_detached</c>, so after this the VIDEO clip must
    /// show no fill and the audio-track clip the detach created (<c>{id}~a1</c>, same
    /// media/in/out) must show one. Nothing in the shell can perform that detach yet —
    /// see <see cref="App.StartupDetachAudio"/> for why this route exists and why it is
    /// argv-gated, Debug-only, and goes through the same command a real audio panel
    /// will.</para>
    ///
    /// <para>The domain owns the rules and keeps them: <c>command.rs:1529</c> refuses a
    /// clip that is not on a video track, one that is already detached, media with no
    /// audio, and a project with no audio track. Every refusal is LOGGED, not
    /// swallowed — a silently skipped detach would make the screenshot beside it a lie.</para>
    /// </summary>
    private static async Task DetachFirstAudioBearingClipAsync(RudisNative engine, ShellMirror mirror)
    {
        // The place dispatch returned, but the MIRROR is a poll behind it (D-06), and
        // the clip id this needs only exists in the mirror. Wait for it rather than
        // reaching around — the same rule PlaceImportedOnTimelineAsync already follows.
        string? clipId = null;
        for (var attempt = 0; attempt < 40 && clipId is null; attempt++)
        {
            var project = mirror.Project;
            var tracks = project?.Timeline.Tracks;
            if (tracks is not null)
            {
                for (var t = 0; t < tracks.Count && clipId is null; t++)
                {
                    if (!string.Equals(tracks[t].Kind, "video", StringComparison.OrdinalIgnoreCase))
                    {
                        continue;
                    }

                    foreach (var clip in tracks[t].Clips)
                    {
                        var media = project!.MediaBin.Find(m => m.Id == clip.MediaId);
                        if (media is { HasAudio: true } && !clip.AudioDetached)
                        {
                            clipId = clip.Id;
                            break;
                        }
                    }
                }
            }

            if (clipId is null)
            {
                await Task.Delay(100);
            }
        }

        if (clipId is null)
        {
            App.LogDiagnostic("--detach-audio: no audio-bearing video clip on the timeline to detach");
            return;
        }

        var args = new System.Text.Json.Nodes.JsonObject
        {
            ["cmd"] = new System.Text.Json.Nodes.JsonObject
            {
                ["type"] = "detach_audio",
                ["data"] = new System.Text.Json.Nodes.JsonObject { ["clip_id"] = clipId },
            },
        };

        var result = await engine.DispatchCommandAsync(args.ToJsonString());
        if (result.Kind != RudisResultKind.Ok)
        {
            App.LogDiagnostic(
                $"--detach-audio: REFUSED for '{clipId}' ({result.Kind}/{result.Status}): {result.Error}");
            return;
        }

        App.LogDiagnostic($"--detach-audio: detached '{clipId}' — its audio now lives on the audio track");
    }
#endif

    // ── remove track: the real, cascading, undoable dispatch ────────────────

    /// <summary>
    /// The real remove-track flow: confirm, then dispatch the REAL undoable
    /// <c>remove_track</c> command.
    ///
    /// <para><b>This method is production code and the whole of the behaviour.</b> The
    /// Timeline's track-gutter button is the PRODUCTION trigger and it arrives with the
    /// rest of that region's interactions (Phase 52); what is temporary is only the way
    /// plan 51-06 reaches this method, not the method. Nothing here is Debug-gated,
    /// nothing here is a stub.</para>
    ///
    /// <para>Semantics are v6's, read from <c>main.ts</c>'s <c>removeTrackAt</c> rather
    /// than invented: an EMPTY lane is removed without a prompt (there is nothing to
    /// warn about and the removal is undoable either way); a lane holding clips is
    /// gated by <see cref="Dialogs.RemoveTrackDialog"/>, whose message names the kind
    /// and the clip count — because <c>Command::RemoveTrack</c> CASCADES the whole lane
    /// (crates/core/src/command.rs:2177-2196) and the count is the part of that cascade
    /// the user cannot see from the control they pressed (T-51-23).</para>
    ///
    /// <para>Wire shape read from the producer, not guessed:
    /// <c>{"type":"remove_track","data":{"index":n}}</c> through
    /// <c>rudis_dispatch_command</c> — the same envelope the frontend sends
    /// (main.ts:205). A non-<c>Ok</c> result is LOGGED and surfaced through
    /// <c>Shell.StatusText</c>, never swallowed (UI-SPEC §5).</para>
    /// </summary>
    /// <param name="index">The lane's index in the mirrored timeline.</param>
    internal async Task RemoveTrackAsync(int index)
    {
        var engine = App.Engine;
        var mirror = App.Mirror;
        if (engine is null || engine.IsInvalid || mirror is null)
        {
            return;
        }

        var tracks = mirror.Project?.Timeline.Tracks;
        if (tracks is null || index < 0 || index >= tracks.Count)
        {
            App.LogDiagnostic(
                $"remove_track: index {index} is out of range (mirror holds " +
                $"{tracks?.Count ?? 0} track(s)) — nothing dispatched");
            return;
        }

        var track = tracks[index];
        var clipCount = track.Clips.Count;

        if (clipCount > 0)
        {
            var root = Content?.XamlRoot;
            if (root is null)
            {
                App.LogDiagnostic("remove_track: no XamlRoot — the confirmation cannot be shown");
                return;
            }

            var dialog = new Dialogs.RemoveTrackDialog();
            var confirmed = await dialog.ConfirmAsync(root, track.Kind, clipCount);
            if (!confirmed)
            {
                App.LogDiagnostic(
                    $"remove_track: cancelled at the confirmation for track {index} " +
                    $"({track.Kind}, {clipCount} clip(s)) — nothing dispatched");
                return;
            }
        }

        // ⚠ FIXED BY PLAN 52-14 — this payload was UNSENDABLE as first written.
        //
        // It was built here by hand as a BARE `Command` object,
        // `{"type":"remove_track","data":{"index":n}}`. That is the shape the v6 frontend
        // passes as the ARGUMENT to its own `dispatch()` helper (main.ts:205), but it is
        // NOT what the export takes: `rudis_dispatch_command`'s args are
        // `{"cmd": {..Command..}}` (crates/ffi/src/dispatch.rs:29-33), and `DispatchArgs`
        // has no `serde(default)`, so a missing wrapper is a deserialisation refusal
        // before any domain code runs. The failure was ONE LINE IN THE DIAGNOSTIC TAIL
        // and nothing else — the user would have confirmed a destructive cascade and seen
        // the track stay exactly where it was.
        //
        // The fix is not a hand-added wrapper but the BUILDER, which is where the envelope
        // now lives once for every Timeline command; `TimelineCommandTests`'
        // `a_command_sent_without_the_cmd_envelope_is_refused` is the standing gate that
        // this stays true, and it fails against the old payload.
        var args = Regions.TimelineCommands.RemoveTrack(index);

        try
        {
            var result = await engine.DispatchCommandAsync(args);
            App.LogDiagnostic(result.Kind == RudisResultKind.Ok
                ? $"remove_track: track {index} ({track.Kind}, {clipCount} clip(s)) removed -> " +
                  result.RawEnvelope
                : $"remove_track FAILED ({result.Kind}/{result.Status}): {result.Error}");
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"remove_track threw: {ex.GetType().Name}: {ex.Message}");
        }
    }

#if DEBUG
    /// <summary>
    /// The Debug-only launch hook behind <see cref="App.ShowRemoveTrackDialogOnLaunch"/>:
    /// open the REAL confirmation against track 0 once the mirror reports it holds clips.
    ///
    /// <para><b>It waits for a NON-EMPTY lane and does nothing if none appears, and that
    /// is a safety property, not politeness.</b> <see cref="RemoveTrackAsync"/> keeps
    /// v6's semantics, in which an EMPTY lane is removed with no prompt — so a launch
    /// that reached it before any clip existed would silently delete track 0 instead of
    /// opening the dialog this flag exists to photograph. Failing visibly (a diagnostic
    /// line and no dialog) is the correct outcome there; destroying a lane is not.</para>
    ///
    /// <para>Pair it with <c>--place-on-timeline</c>, which is what puts a real clip on
    /// track 0 through <c>rudis_place_clip</c>.</para>
    /// </summary>
    private async Task ShowRemoveTrackDialogOnLaunchAsync(ShellMirror mirror)
    {
        for (var attempt = 0; attempt < 60; attempt++)
        {
            var tracks = mirror.Project?.Timeline.Tracks;
            if (tracks is { Count: > 0 } && tracks[0].Clips.Count > 0)
            {
                App.LogDiagnostic(
                    $"remove-track launch flag: track 0 holds {tracks[0].Clips.Count} clip(s) — " +
                    "opening the real confirmation");
                await RemoveTrackAsync(0);
                return;
            }
            await Task.Delay(100);
        }

        App.LogDiagnostic(
            "remove-track launch flag: track 0 never reported a clip within 6s, so the " +
            "confirmation was NOT opened. An empty lane is removed without a prompt (v6 " +
            "parity), so opening the flow here would have deleted it silently instead of " +
            "showing the dialog.");
    }
#endif

#if DEBUG
    /// <summary>
    /// <c>--place-on-timeline</c>'s implementation: walk the mirrored MediaBin and place
    /// each item end to end through <c>rudis_place_clip</c> — the SAME backend command
    /// Phase 53's MediaBin drag will use, with no shortcut around it.
    ///
    /// <para>Track choice follows the domain's own rule rather than guessing: a fresh
    /// project has track 0 Video and track 1 Audio (<c>crates/core/src/model.rs:184</c>),
    /// and <c>run_place_clip</c> refuses a mismatch outright, so audio-only media goes to
    /// track 1 and everything else to track 0. A refusal is LOGGED, never swallowed.</para>
    ///
    /// <para>Debug-only. See <see cref="App.StartupPlaceOnTimeline"/> for why this is a
    /// developer route and not product behaviour.</para>
    /// </summary>
    private static async Task PlaceImportedOnTimelineAsync(RudisNative engine, ShellMirror mirror)
    {
        // The import returned, but the MIRROR is a poll behind it: `project:changed`
        // arrives on the 100ms cold cadence (D-06), so reading the bin immediately would
        // reliably find it empty. Wait for the mirror rather than reaching around it —
        // reaching around would make this route stop being a faithful stand-in for the
        // real MediaBin drag it is standing in for.
        List<MediaBinItem>? bin = null;
        for (var attempt = 0; attempt < 30; attempt++)
        {
            bin = mirror.Project?.MediaBin;
            if (bin is { Count: > 0 })
            {
                break;
            }
            await Task.Delay(100);
        }

        if (bin is null || bin.Count == 0)
        {
            App.LogDiagnostic("--place-on-timeline: nothing in the media bin to place");
            return;
        }

        var videoStartUs = 0L;
        var audioStartUs = 0L;

        foreach (var item in bin)
        {
            var audioOnly = string.Equals(item.MediaKind, "audio", StringComparison.OrdinalIgnoreCase);
            var track = audioOnly ? 1 : 0;
            var startUs = audioOnly ? audioStartUs : videoStartUs;

            var args = new System.Text.Json.Nodes.JsonObject
            {
                ["media_id"] = item.Id,
                ["track"] = track,
                ["start_us"] = startUs,
            };

            var result = await engine.PlaceClipAsync(args.ToJsonString());
            if (result.Kind != RudisResultKind.Ok)
            {
                App.LogDiagnostic(
                    $"--place-on-timeline: place_clip refused '{item.Id}' on track {track} " +
                    $"({result.Kind}/{result.Status}): {result.Error}");
                continue;
            }

            if (audioOnly)
            {
                audioStartUs += item.DurationUs;
            }
            else
            {
                videoStartUs += item.DurationUs;
            }

            App.LogDiagnostic(
                $"--place-on-timeline: placed '{item.Id}' on track {track} at {startUs}us " +
                $"({item.DurationUs}us long)");
        }
    }
#endif

    /// <summary>
    /// <c>Toolbar.ModeTabs</c> is pure shell view-state (UI-SPEC §3 — no backend), and
    /// this is all it does: reveal the LABELLED Inspector placeholder. Phase 53 puts
    /// the real Inspector there.
    /// </summary>
    private void OnWorkspaceModeChanged(string mode)
        => InspectorPlaceholder.Visibility =
            mode == "color" ? Visibility.Visible : Visibility.Collapsed;

    /// <summary>
    /// The <c>Canvas › ToolPalette</c> selection reaches the ink layer over the
    /// Preview swapchain. With <c>Pointer</c> selected the layer is INERT and every
    /// pointer event falls through to whatever is beneath — the same gate v6.0's
    /// <c>DRAW_MODE</c> flag performed, expressed as ordinary WinUI hit-testing
    /// instead of a Win32 WndProc subclass (D-11).
    /// </summary>
    private void OnCanvasToolChanged(Rudis.Shell.Regions.DrawTool tool)
        => PreviewRegion.Ink.SelectedTool = tool;

    // ── D-01's Canvas/Chat divider (D-02: hand-rolled, settled as Option B in 54-01) ──

    /// <summary>
    /// Start a divider drag: capture the pointer so the gesture survives the cursor
    /// leaving the 6px thumb, and remember the split as it was AT DRAG START.
    ///
    /// <para>The start height is remembered rather than re-read per move because the
    /// rows are being rewritten by the drag itself — reading <c>ActualHeight</c> each
    /// move would integrate the delta against a moving base and accelerate away from the
    /// cursor. This is the <c>TimelineInteraction</c> idiom, unchanged.</para>
    /// </summary>
    private void OnChatSplitterPressed(object sender, Microsoft.UI.Xaml.Input.PointerRoutedEventArgs e)
    {
        _splitStartY = e.GetCurrentPoint(CanvasChatSplit).Position.Y;
        _splitStartTopPx = CanvasSplitRow.ActualHeight;
        _splitTotalPx = CanvasSplitRow.ActualHeight + ChatSplitRow.ActualHeight;

        // A refused capture means no PointerReleased is guaranteed to arrive, so the
        // drag is simply not started rather than started and left un-endable.
        _splitDragging = ChatSplitter.CapturePointer(e.Pointer);
        e.Handled = true;
    }

    /// <summary>
    /// Rebalance the two rows.
    ///
    /// <para><b>The returned pixel pair is written back as STAR weights, and that is part
    /// of the split function's own contract, not a shortcut here (54-01).</b> Star weights
    /// proportional to the pixel pair reproduce the split immediately and then keep that
    /// PROPORTION across a window resize, which is what a divider between two <c>*</c>
    /// rows means. Writing pixel heights would pin the Canvas and hand every resize to
    /// the Chat.</para>
    ///
    /// <para>The two minimums are READ OFF THE ROWS rather than re-typed here: the Grid's
    /// own clamp and this clamp are then one number each, and cannot drift into a divider
    /// that stops somewhere the layout will not honour.</para>
    ///
    /// <para><b>The Chat's floor is 96, not 176.</b> This paragraph used to name 176 as
    /// the Chat's floor and credit it to 54-03 — a claim that never matched the shipped
    /// RowDefinition. 54-03 did recommend 160-180 for a COMFORTABLE transcript, but 54-05
    /// measured that a 176 floor does not FIT column 0 at the shell's default window
    /// height: the two minimums together exceeded the row's budget, so this method's
    /// <see cref="Regions.ChatSplit.Apply"/> hit its over-constrained guard and refused
    /// every drag (a 60px pull moved the divider 0px), and WinUI clipped
    /// <c>Chat.Composer</c> and <c>Chat.SendButton</c> fully offscreen. The shipped floors
    /// are therefore 80 (Canvas) / 96 (Chat) — the smallest rows that keep every control
    /// on screen — and COMFORT is delivered by the 50/50 default instead. See 54-05's
    /// SUMMARY § "The row minimums are 80/96" and MainWindow.xaml's own split comment.</para>
    ///
    /// <para>Corrected by quick-260731-w45 (2026-07-31), which also re-measured the
    /// budget after removing the <c>Shell.StatusText</c> panel from view: the default now
    /// puts ~193.6 / ~192.0 logical px per side (was ~114 / ~112), and a 60-physical-px
    /// pull moves the divider the full 60px instead of clamping at ~41. The floors were
    /// re-examined against those numbers and deliberately KEPT at 80/96 — raising them
    /// would only widen the window-height range where the two failure modes above return.
    /// Readings: that task's <c>artifacts/w45-uia-probe.md</c>.</para>
    /// </summary>
    private void OnChatSplitterMoved(object sender, Microsoft.UI.Xaml.Input.PointerRoutedEventArgs e)
    {
        if (!_splitDragging)
        {
            return;
        }

        var y = e.GetCurrentPoint(CanvasChatSplit).Position.Y;
        var (top, bottom) = Regions.ChatSplit.Apply(
            _splitStartTopPx,
            _splitTotalPx,
            y - _splitStartY,
            CanvasSplitRow.MinHeight,
            ChatSplitRow.MinHeight);

        CanvasSplitRow.Height = new GridLength(top, GridUnitType.Star);
        ChatSplitRow.Height = new GridLength(bottom, GridUnitType.Star);
        e.Handled = true;
    }

    /// <summary>
    /// End the drag. Wired to <c>PointerReleased</c>, <c>PointerCanceled</c> AND
    /// <c>PointerCaptureLost</c> — the third is not ceremony: capture can be taken away
    /// (a system gesture, another element capturing, the window deactivating) without a
    /// release ever arriving, and a drag left flagged live would then follow the pointer
    /// around the window with no button held. Nothing is persisted here (D-03).
    /// </summary>
    private void OnChatSplitterReleased(object sender, Microsoft.UI.Xaml.Input.PointerRoutedEventArgs e)
    {
        if (!_splitDragging)
        {
            return;
        }

        _splitDragging = false;
        ChatSplitter.ReleasePointerCapture(e.Pointer);
        e.Handled = true;
    }

    /// <summary>
    /// T-50-20 (malicious/irrelevant drop payload): accept the drop ONLY when the
    /// package actually carries storage items. Anything else — text, bitmaps, HTML,
    /// a link — is left as <c>DataPackageOperation.None</c>, so no path ever reaches
    /// the backend from a non-file drop.
    /// </summary>
    private void OnWindowDragOver(object sender, DragEventArgs e)
    {
        if (e.DataView.Contains(StandardDataFormats.StorageItems))
        {
            e.AcceptedOperation = DataPackageOperation.Copy;
            e.DragUIOverride.Caption = "Import into Rudis";
            e.DragUIOverride.IsGlyphVisible = true;
        }
        else
        {
            e.AcceptedOperation = DataPackageOperation.None;
        }
    }

    /// <summary>
    /// The window-wide drop route into the Toolbar's ONE shared import routine
    /// (handoff README:92 — "Import also accepts file drop anywhere on the window").
    ///
    /// <para>T-50-20 continued: only <see cref="StorageFile"/> items are taken (a
    /// dropped FOLDER is deliberately ignored rather than silently routed to
    /// <c>rudis_import_media_folder</c>, whose recursion clamps belong to whichever
    /// phase surfaces folder import), and each path must still resolve to a file that
    /// EXISTS. The backend's own probe remains the authoritative gate.</para>
    /// </summary>
    private void OnWindowDrop(object sender, DragEventArgs e)
    {
        var deferral = e.GetDeferral();
        _ = HandleDropAsync(e, deferral);
    }

    private async Task HandleDropAsync(DragEventArgs e, DragOperationDeferral deferral)
    {
        try
        {
            if (!e.DataView.Contains(StandardDataFormats.StorageItems))
            {
                return;
            }
            var items = await e.DataView.GetStorageItemsAsync();
            var paths = new List<string>();
            foreach (var item in items)
            {
                if (item is StorageFile file && !string.IsNullOrEmpty(file.Path) && File.Exists(file.Path))
                {
                    paths.Add(file.Path);
                }
            }
            if (paths.Count == 0)
            {
                App.LogDiagnostic("drop: no importable files in the payload (folders and non-file items are ignored)");
                return;
            }
            await ToolbarRegion.ImportPathsAsync(paths, loadPreviewOnFirst: false);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"drop failed: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            deferral.Complete();
        }
    }

    /// <summary>The last project name pushed into the TitleBar, and the last dot
    /// state — see <see cref="RefreshTitleBar"/> for why they are cached.</summary>
    private string? _titleBarName;

    private bool? _titleBarUnsaved;

    /// <summary>
    /// Push mirrored project state into the TitleBar. Read-only in the rule-4 sense:
    /// both values are derived from the mirror and nothing here is authoritative.
    ///
    /// <para><b>Idempotent, and that is what lets it ride the cold cycle.</b> Before
    /// plan 60.1-06 the dot could only change on a <c>project:changed</c> event, which
    /// was enough while the only things that moved it were mutations. A save moves it
    /// too — and an ordinary <c>Ctrl+S</c> emits NO patch, deliberately
    /// (<c>run_save_project</c> emits only when it MINTED a name, because
    /// <c>ProjectSwitched</c> routes to a full resync and saving would otherwise be the
    /// most expensive operation in the app). So the dot is re-derived every cold cycle,
    /// beside <c>RefreshStatus</c>, and the cached pair means a tick that changes
    /// nothing touches no visual at all. That is one string compare and one bool
    /// compare per 100ms, on the COLD path; the hot path is not involved.</para>
    ///
    /// <para>The alternative — a second refresh mechanism each save site invokes — was
    /// rejected for the reason the persisted seq lives in <c>RudisNative</c>: a save
    /// site added later would forget to call it, and the dot is exactly the surface
    /// that must not quietly stop being true.</para>
    /// </summary>
    private void RefreshTitleBar(ShellMirror mirror)
    {
        var name = mirror.Project?.Name;
        var unsaved = HasUnpersistedEdits(mirror);

        if (_titleBarUnsaved == unsaved && string.Equals(_titleBarName, name, StringComparison.Ordinal))
        {
            return;
        }

        _titleBarName = name;
        _titleBarUnsaved = unsaved;
        TitleBarRegion.SetProject(name, unsaved);
    }

    /// <summary>
    /// The unsaved <c>•</c> (handoff README:84), derived HONESTLY from two real
    /// numbers. The rule itself lives in <see cref="UnsavedIndicator"/> — pure, pinned
    /// by nine facts, and reachable from the test tier, which a member of this
    /// WinUI-bearing file is not (the 60.1-05 structural finding).
    ///
    /// <para>The retired rule was <c>mirror.LastAppliedStoreSeq &gt; 0</c>, and its own
    /// doc comment here predicted this change: <i>"this rule stops being exact and needs
    /// a real 'last persisted seq' signal — which today does not exist in the ABI (a
    /// D-15 FINDING … not a licence to widen it here)."</i> Phase 60.1 IS that widening,
    /// so the forward note is now due and has been paid: the signal rides
    /// <c>rudis_save_project</c>'s own envelope and <c>RudisNative</c> records it at the
    /// one chokepoint every save passes through. <c>ring::EVENT_NAMES</c> stays at 6.</para>
    /// </summary>
    private static bool HasUnpersistedEdits(ShellMirror mirror)
        => UnsavedIndicator.HasUnpersistedEdits(
            mirror.LastAppliedStoreSeq, App.Engine?.LastPersistedStoreSeq ?? 0UL);

    private void StartColdPoll(ShellMirror mirror, string probeLine)
    {
        _coldTimer = _dispatcher.CreateTimer();
        _coldTimer.Interval = TimeSpan.FromMilliseconds(ColdPollIntervalMs);
        _coldTimer.IsRepeating = true;
        _coldTimer.Tick += (_, _) => OnColdTick(mirror, probeLine);
        _coldTimer.Start();

        // Plan 52-09. See App.RequestMirrorPollNow for why. Wired HERE, beside the
        // timer it short-circuits, so the two schedules are visible together.
        App.RequestMirrorPollNow = () => RequestImmediatePoll(mirror, probeLine);
    }

    /// <summary>
    /// Run a cold cycle NOW, or arrange for one the moment the current cycle ends.
    ///
    /// <para>The deferral half is the part that matters: a cycle already in flight may
    /// have read the event ring BEFORE the caller's patch reached it, so simply
    /// returning would put the caller back on the 100ms tick it was trying to avoid.
    /// One flag, cleared as it is consumed, so a burst of dispatches during one cycle
    /// costs exactly one extra cycle rather than one each (T-50-18's single-cycle
    /// invariant is preserved, not widened).</para>
    /// </summary>
    private void RequestImmediatePoll(ShellMirror mirror, string probeLine)
    {
        if (_closed)
        {
            return;
        }

        if (_pollInFlight)
        {
            _pollAgainRequested = true;
            return;
        }

        OnColdTick(mirror, probeLine);
    }

    /// <summary>
    /// ⚠ THE HOT TICK IS A <see cref="DispatcherQueueTimer"/>, NOT a
    /// <c>CompositionTarget.Rendering</c> SUBSCRIPTION — MEASURED, NOT STYLISTIC
    /// (debug session <c>.planning/debug/gate02-present-thread-gc-collection-count.md</c>,
    /// 2026-08-02).
    ///
    /// <para>This method DID subscribe <c>CompositionTarget.Rendering</c> from plan 50-04
    /// until Phase 55's GATE-02 red was traced to it. <c>Rendering</c> is a WinRT
    /// <c>EventHandler&lt;object&gt;</c> event, and CsWinRT's generated
    /// <c>Do_Abi_Invoke</c> marshals the args on EVERY native raise:
    /// <c>MarshalInspectable&lt;object&gt;.FromAbi</c> creates a brand-new
    /// <c>RenderingEventArgs</c> RCW plus finalizable
    /// <c>WinRT.ObjectReference`1[IUnknownVftbl]</c> wrappers, per frame, ~40–60/s, from
    /// startup, whether or not anything here uses the args (this handler never did). A
    /// verbose GC trace (<c>Microsoft-Windows-DotNETRuntime:0x1080001:5</c>) over a
    /// gate-shaped session measured 6,161 of 6,315 finalized objects (97.6%) as exactly
    /// that wrapper type, all allocated at
    /// <c>Windows_Foundation_EventHandler_1_object.Do_Abi_Invoke</c>. While the app
    /// idles no GC runs, so thousands of dead-but-unfinalized reference-tracker objects
    /// pile up — and WinUI's ReferenceTrackerManager then requests a FULL BLOCKING GC
    /// (reason=Induced, native, on the UI thread) at the next burst of UI activity,
    /// which in the GATE-02 session is always the Play press. That induced collection
    /// was GATE-02's 5/5-reproducible marginal-collection red.</para>
    ///
    /// <para>The two consumers reached from <see cref="OnHotTick"/> need a REGULAR
    /// UI-thread tick, not a vsync-phase-aligned one: both are change-gated (the
    /// ticker's timecode cache; the Timeline renderer's dirty gate), and the actual
    /// pixel presentation is the Rust present thread's swapchain, which never depended
    /// on XAML's render clock. A repeating <see cref="DispatcherQueueTimer"/> raises
    /// <c>Tick</c> with a CACHED sender RCW and null args — the same verbose trace shows
    /// the shell's existing timers produced zero finalizable wrappers over the whole
    /// session — so the hot tick keeps its cadence and the steady-state interop churn
    /// drops to ~nothing.</para>
    /// </summary>
    private void StartHotPoll()
    {
        _hotTimer = _dispatcher.CreateTimer();
        _hotTimer.Interval = TimeSpan.FromMilliseconds(HotTickIntervalMs);
        _hotTimer.IsRepeating = true;
        _hotTimer.Tick += (_, _) => OnHotTick();
        _hotTimer.Start();
    }

    /// <summary>Runs on the UI thread. Starts at most one cycle (T-50-18) and never
    /// blocks: the returned Task is deliberately not awaited here, and it is an
    /// <c>async Task</c> with a total try/catch — never <c>async void</c> (D-08).</summary>
    private void OnColdTick(ShellMirror mirror, string probeLine)
    {
        if (_pollInFlight || _closed)
        {
            return;
        }
        _pollInFlight = true;
        _ = PollCycleAsync(mirror, probeLine);
    }

    private async Task PollCycleAsync(ShellMirror mirror, string probeLine)
    {
        try
        {
            // OFF the UI thread from here: ConfigureAwait(false) keeps the
            // continuation off it, and the parse already happened on the worker /
            // pool. The mirror is NOT touched on this side of the marshal.
            var poll = await mirror.FetchPollAsync().ConfigureAwait(false);

            if (_closed || !_dispatcher.TryEnqueue(() => _ = ApplyBatchAsync(mirror, poll, probeLine)))
            {
                // The window is going away (or its queue already shut down): drop the
                // batch and release the gate. The next attach/poll re-reads from the
                // same cursor, so nothing is lost.
                _pollInFlight = false;
            }
        }
        catch (Exception e)
        {
            App.LogDiagnostic($"cold poll fetch faulted: {e.GetType().Name}: {e.Message}");
            _pollInFlight = false;
        }
    }

    /// <summary>Runs on the UI thread (marshalled): the ONLY place the mirror is
    /// mutated, which is what makes the mirror's lock-free affinity sound.</summary>
    private async Task ApplyBatchAsync(ShellMirror mirror, RudisResult<PollOutcome> poll, string probeLine)
    {
        try
        {
            if (poll.Kind == RudisResultKind.Ok)
            {
                await mirror.ApplyPollOutcomeAsync(poll.Value!);
            }
            else
            {
                // Two-layer discipline preserved: the KIND distinguishes a transport
                // fault (PanicCaught = -99 and friends) from a domain error. Either
                // way the cursor is held and the next tick re-asks.
                App.LogDiagnostic(
                    $"cold poll failed ({poll.Kind}/{poll.Status}: {poll.Error}) — cursor held, retrying next tick");
            }
        }
        catch (Exception e)
        {
            App.LogDiagnostic($"cold poll apply faulted: {e.GetType().Name}: {e.Message}");
        }
        finally
        {
            _pollInFlight = false;
            RefreshStatus(probeLine, mirror);

            // Plan 60.1-06. A save emits no patch, so the dot cannot ride
            // `project:changed`; it is re-derived here instead, and the call is a no-op
            // unless the answer actually changed. See RefreshTitleBar.
            RefreshTitleBar(mirror);

            // Plan 52-08 — SHELL-09's retrieval half rides THIS cycle (D-21): no
            // new timer, no seventh event type. Deliberately NOT awaited inside the
            // poll gate: it carries its own in-flight gate, and holding
            // `_pollInFlight` for the duration of a peak fetch would let a slow
            // answer stall event polling. The ABI call it makes runs on the interop
            // worker like every other cold-path call; what runs HERE is the decode
            // and the cache write, on the same UI thread the frame build reads
            // that cache from.
            _ = TimelineRegion.OnColdPollAsync();

            // Plan 63-04 (TRUST-03) — the SECOND and THIRD region on this same cycle, and
            // deliberately not a timer of their own. `rudis_get_proxy_status` and
            // `rudis_get_render_cache_status` are both documented POLL-ONLY BY DECISION
            // (58-CONTEXT D-29 / 59-CONTEXT D-30): each says, in its own Rust doc, that a
            // push event may be added "when a shell region actually consumes it" and that
            // until then `ring::EVENT_NAMES` stays at 6. These two lines are that
            // consumption, and the event set is still 6.
            //
            // Not awaited, for the reason the line above is not: each carries its own
            // in-flight gate, and holding `_pollInFlight` across a status read would let a
            // slow answer stall event polling. MediaBin's pass is bounded to the items that
            // can still move (a settled bin costs zero ABI calls); Transport's is a single
            // argument-free read.
            _ = MediaBinRegion.OnColdPollAsync();
            _ = TransportRegion.OnColdPollAsync();

            // Plan 52-09: a dispatch that landed while this cycle was in flight asked
            // for another look. Consume the request here, after the gate is released,
            // so the next cycle starts immediately instead of on the 100ms tick.
            if (_pollAgainRequested && !_closed)
            {
                _pollAgainRequested = false;
                OnColdTick(mirror, probeLine);
            }
        }
    }

    /// <summary>
    /// ⚠ HOT PATH — SC-3 zero-allocation rules apply to EVERYTHING reachable from
    /// here (UI-SPEC §4, v7-SUMMARY:249's <c>GC.GetAllocatedBytesForCurrentThread()</c>
    /// delta == 0 gate). No string interpolation, no boxing, no LINQ, no event
    /// dispatch, no visual-tree touch. The body is one scalar ABI read into a
    /// <c>long?</c> field plus a counter increment.
    ///
    /// <para>Plan 50-06 FILLED the consumer slot 50-04 left here: the body is now one
    /// call into <c>Transport</c>'s <c>PlayheadTicker</c>, which performs the single scalar
    /// ABI read and touches the visual tree ONLY when the rendered timecode changes. The
    /// ABI read moved INTO the ticker rather than being duplicated here, so there is
    /// exactly one per-tick call site in the whole shell — and
    /// <c>HotPathAllocationTests</c> measures that one.</para>
    ///
    /// <para>Driven by <c>_hotTimer</c> since the GATE-02 debug session (2026-08-02);
    /// previously the <c>CompositionTarget.Rendering</c> handler. The body is unchanged
    /// — what changed is the DRIVER, because the Rendering event's own args marshaling
    /// was the finalizable-wrapper churn behind GATE-02's induced-GC red. See
    /// <see cref="StartHotPoll"/>.</para>
    /// </summary>
    private void OnHotTick()
    {
        _renderTicks++;
        TransportRegion.OnCompositionTick();

        // Plan 52-06 is the SECOND consumer of this ONE slot, and it deliberately does
        // not open a second subscription of its own. It is handed the position the
        // ticker just read rather than making an ABI call of its own - the Timeline's
        // whole per-frame cost is one frame build (measured at zero managed bytes) plus
        // one native call, and when nothing has moved that native call is refused by the
        // renderer's dirty gate before it touches the GPU (T-52-30).
        TimelineRegion.OnRenderingTick(TransportRegion.LastPlayheadUs);
    }

    /// <summary>
    /// COLD-cadence status readout — real mirrored state, never a placeholder: the
    /// project the backend actually holds, both sequence cursors, the resync count
    /// with its last cause, and evidence that the hot rate is ticking. Called from
    /// mirror notifications and after each poll apply, NEVER from
    /// <see cref="OnHotTick"/> (that would put string formatting on the hot path).
    /// </summary>
    private void RefreshStatus(string probeLine, ShellMirror mirror)
    {
        if (_closed)
        {
            return;
        }
        var project = mirror.Project;
        var clips = 0;
        if (project is not null)
        {
            foreach (var track in project.Timeline.Tracks)
            {
                clips += track.Clips.Count;
            }
        }
        var playhead = TransportRegion.LastPlayheadUs?.ToString(CultureInfo.InvariantCulture) ?? "—";
        var full = string.Join(
            Environment.NewLine,
            probeLine,
            $"mirror: project=\"{project?.Name}\" media={project?.MediaBin.Count ?? 0} clips={clips} preview_mode={project?.PreviewMode}",
            $"cursors: ring_seq={mirror.RingSeq} store_seq={mirror.LastAppliedStoreSeq} resyncs={mirror.ResyncCount}",
            $"cold poll: every {ColdPollIntervalMs}ms · hot ticks={_renderTicks} · playhead_us={playhead} " +
            $"· readout renders={TransportRegion.HotRenders}/{TransportRegion.HotTicks} (the gap IS the change-detection cache)",
            $"playback: playing={mirror.ActivePlayback?.Playing} duration_us={mirror.ActivePlayback?.DurationUs}",
            $"titlebar: {TitleBarRegion.MechanismNote}",
            $"transport: {TransportRegion.TypographyNote}",
            // ⚠ SHELL-04's attach measurement is deliberately NOT here.
            //
            // It was, for one run, and Phase 50's
            // `no_gen0_collections_during_sustained_playback_with_uia_attached` went
            // RED at delta 2: this method already builds nine interpolated strings and
            // a Join per call, it runs on EVERY mirror notification (which playback
            // makes frequent), and a tenth was enough to push the cold path over the
            // gen0 threshold inside the gate's 10s window. The note never changes after
            // attach, so rebuilding it 100x/second to display a constant was pure
            // waste. It now lives on `Preview.Surface`'s AutomationProperties.HelpText,
            // written ONCE at attach — same UIA readability, zero per-tick cost.
            $"last diagnostic: {App.LastDiagnostic ?? "(none)"}");

        // ⚠ MEASURED, and recorded because the obvious "optimisation" here is WRONG.
        //
        // Plan 51-04 moved this readout out of the (now real) Preview surface into the
        // handoff's 486px left column — a status block painted over live video would
        // corrupt exactly the pixel evidence 51-04/51-06 capture through that surface's
        // own bounding rect. Laying ~700 characters out in a 486px box, on every mirror
        // notification (~100 during a 10s playback window), costs a little more than the
        // same text in a full-width box: allocation over the GC gate's play window went
        // 1,002,816 B -> 1,073,416 B (+7.0%). Measured against a rebuilt pre-51-04
        // shell, not inferred.
        //
        // The tempting fix — render a compact line and put the full diagnostic on
        // AutomationProperties.Name so UIA readers still get everything — was tried and
        // MEASURED WORSE: 1,105,688 B (+10.3% over baseline), because raising a UIA
        // Name-changed property notification ~100 times costs more than the layout it
        // saves. It is not in the code for that reason; do not re-derive it.
        //
        // The residual +7% is COLD-path only. It is not per-frame managed code — the
        // Preview region has no per-tick code at all once its startup latch stops, and
        // the present loop is a Rust thread (SHELL-06/D-08). See plan 51-04's SUMMARY
        // and the GC gate's own amended remarks for the full numbers and the finding
        // handed to plan 51-07.
        //
        // ── quick-260731-w45 (2026-07-31) ──────────────────────────────────────────
        // The readout's ROW IS NOW ZERO-HEIGHT (MainWindow.xaml), so the 486px-column
        // layout cost measured above no longer applies per notification: there is no
        // ~700-character line to lay out in a 486px box any more. Layout work only
        // SHRANK; nothing here was made more expensive.
        //
        // The assignment below stays, unchanged and ungated, because `.Name` is fed by
        // `Text` — that is the whole reason the element is still in the tree. Do not
        // "optimise" it away now that nobody can see it: three suites read
        // `Shell.StatusText.Name` into committed evidence artifacts, and two of those
        // reads are non-assertive (they would silently record "(not found)" rather than
        // fail). The Name-notification alternative described above remains REJECTED on
        // its own measurement (+10.3% vs +7.0%) and is not made attractive by this
        // change — it never was about the layout being visible.
        //
        // NO CODE IN THIS METHOD WAS CHANGED BY THAT TASK. Comment only.
        StatusText.Text = full;
    }

    /// <summary>
    /// <b>THE ONE CLOSE PATH.</b> Both routes reach it:
    /// <list type="bullet">
    /// <item>the close button this shell DRAWS — <c>TitleBar</c> owns this window's
    ///   chrome, so its button is not a system caption button and closes nothing by
    ///   itself; it invokes <c>TitleBar.RequestClose</c>, which the constructor points
    ///   here;</item>
    /// <item>Alt+F4, the system menu, a taskbar close and every other OS-driven close,
    ///   via <see cref="OnAppWindowClosing"/>.</item>
    /// </list>
    ///
    /// <para><b>It exists because <c>AppWindow.Destroy()</c> does NOT raise
    /// <c>AppWindow.Closing</c></b> — Microsoft Learn, explicit: <i>"The Closing event
    /// does not occur when the AppWindow.Destroy method is called."</i> The drawn
    /// button used to call <c>Destroy()</c> directly, so a save-on-close installed only
    /// on <c>Closing</c> would work for Alt+F4 and never fire for the button the user
    /// actually clicks: the one shape of this bug that tests green and ships broken.
    /// <c>MechanicalGatesTests.exactly_one_close_path</c> fails the build if a
    /// <c>Destroy</c> call ever reappears anywhere in the shell.</para>
    ///
    /// <para><b>⚠ AND IT ENDS IN <c>Window.Close()</c>, NOT <c>AppWindow.Destroy()</c>,
    /// BECAUSE DESTROY DOES NOT ACTUALLY CLOSE THIS APP.</b> Measured on a real running
    /// window on 2026-08-27, against the PRE-PLAN build so the finding is about the
    /// shipped shell and not about this change: a synthetic UIA invoke of
    /// <c>TitleBar.Close</c> made the window vanish, <c>Window.Closed</c> NEVER fired,
    /// <c>OnWindowClosed</c> never ran, <c>rudis_shutdown</c> never ran, and the process
    /// was still alive 20s later answering its introspection pipe. The same build closed
    /// cleanly on <c>WM_CLOSE</c>. So the shipped close button has been orphaning the
    /// process, and a close-time save placed before a <c>Destroy()</c> would have written
    /// the file and then left the app hanging behind it. <c>Close()</c> raises
    /// <c>AppWindow.Closing</c> a second time, which the <c>_closing</c> latch lets
    /// through un-cancelled, and the OS close then runs to completion — which is exactly
    /// what 60.1-RESEARCH pitfall 2 recommends: <i>"route the custom X through the same
    /// path as the system close (Window.Close() / let Closing run) … Prefer the former —
    /// one close path, not two."</i></para>
    ///
    /// <para><b>The order inside is the whole point:</b></para>
    /// <list type="number">
    /// <item>latch, so the two entry points cannot re-enter each other — the
    ///   <c>Closing</c> arm cancels the OS close and calls back in here, which without
    ///   the latch is an infinite loop;</item>
    /// <item><b>SAVE, awaited</b> — enqueued while the engine still accepts work.
    ///   <c>RudisNative.Dispose()</c> does <c>CompleteAdding()</c> then <c>Join()</c>,
    ///   so work enqueued BEFORE it drains, while work enqueued after is refused with
    ///   <c>InvalidHandle</c> (50-02 §1.5(5), 60.1-RESEARCH G-5). Enqueue, await,
    ///   THEN close — never the other way round;</item>
    /// <item>release the GPU surface, while the window is still ALIVE (plan 52-01,
    ///   found by crashing twice on real hardware). This used to live in the
    ///   <c>Closing</c> handler, which is precisely why the drawn button skipped it;</item>
    /// <item><c>Close()</c>, which re-enters <see cref="OnAppWindowClosing"/> with the
    ///   latch already set — so it is NOT cancelled the second time — and the OS close
    ///   runs on to <c>Window.Closed</c>, where <see cref="OnWindowClosed"/> stops both
    ///   poll rates and disposes the engine (SafeHandle release →
    ///   <c>rudis_shutdown</c>).</item>
    /// </list>
    ///
    /// <para><b>A failed save does not block the close.</b> A user who cannot quit
    /// their editor is worse off than one who lost a single save, and the failure is
    /// recorded in the diagnostic log rather than swallowed. <c>Close()</c> is
    /// therefore unconditional and last.</para>
    /// </summary>
    private async Task RequestCloseAsync()
    {
        if (_closing)
        {
            return;
        }

        _closing = true;
        App.LogDiagnostic("close: funnel entered");

        await SaveOnCloseAsync();

        try
        {
            // Idempotent, so the region's own Unloaded backstop running afterwards is a
            // no-op rather than a double free.
            TimelineRegion.DetachSurface();
        }
        catch (Exception e)
        {
            App.LogDiagnostic($"timeline detach at close faulted: {e.GetType().Name}: {e.Message}");
        }

        App.LogDiagnostic("close: closing the window");
        Close();
    }

    /// <summary>
    /// The close-time save — PROJ-01's missing clause, and the reason this phase
    /// exists. Everything before it built the machinery; this is where a user stops
    /// losing work.
    ///
    /// <para><b>No dialog, no picker and no branch, by construction.</b>
    /// <c>rudis_save_project</c> MINTS <c>Untitled.rud</c> when no project is active
    /// rather than refusing (<c>crates/app-core/src/project.rs</c>, whose own doc says
    /// this is "what lets the close path have no dialog and no branch at all"). A
    /// modal between a beginner and quitting is the moment the work gets lost.</para>
    ///
    /// <para><b>Guarded by <see cref="HasUnpersistedEdits"/> on purpose.</b> An
    /// unguarded save would mint a fresh <c>Untitled</c> project every time the app was
    /// launched and closed without an edit — that is litter, not durability.</para>
    ///
    /// <para>Never throws, and never prompts. The elapsed time is logged because
    /// 60.1-RESEARCH assumption A3 — that serialising a large project is fast enough to
    /// run on the close path without a perceptible hang — is documented as UNMEASURED;
    /// this is where the measurement accumulates.</para>
    /// </summary>
    private async Task SaveOnCloseAsync()
    {
        var engine = App.Engine;
        var mirror = App.Mirror;
        if (engine is null || engine.IsInvalid || mirror is null || !HasUnpersistedEdits(mirror))
        {
            return;
        }

        var started = Environment.TickCount64;
        try
        {
            var saved = await engine.SaveProjectAsync();
            var elapsedMs = Environment.TickCount64 - started;
            if (saved.Kind == RudisResultKind.Ok)
            {
                App.LogDiagnostic($"save at close OK in {elapsedMs}ms -> {saved.RawEnvelope}");
                return;
            }

            App.LogDiagnostic(
                $"save at close REFUSED after {elapsedMs}ms ({saved.Kind}/{saved.Status}): {saved.Error} " +
                "- closing anyway, because a user who cannot quit is worse off than one who lost one save");
        }
        catch (Exception e)
        {
            App.LogDiagnostic($"save at close threw: {e.GetType().Name}: {e.Message} - closing anyway");
        }
    }

    /// <summary>
    /// The SYSTEM close arm — Alt+F4, the system menu, a taskbar close. It does none
    /// of the work itself; it hands over to <see cref="RequestCloseAsync"/>, so there
    /// is one close path rather than two.
    ///
    /// <para><c>Closing</c> is <b>synchronous</b> and the save is not, so the only
    /// correct shape is: cancel the OS close, POST the funnel, return. ⚠ A
    /// <c>.Result</c> or a <c>.Wait()</c> here would deadlock the UI thread against the
    /// single interop worker it is waiting on, and
    /// <c>MechanicalGatesTests.no_blocking_waits_in_shell_sources</c> fails the build on
    /// either.</para>
    ///
    /// <para><b>⚠ THE POST IS LOAD-BEARING, AND IT WAS FOUND BY LAUNCHING, NOT BY
    /// REVIEW.</b> <c>RequestCloseAsync</c> only suspends if there is something to save;
    /// with nothing unsaved it runs to completion SYNCHRONOUSLY, so a bare
    /// <c>_ = RequestCloseAsync();</c> reaches <c>AppWindow.Destroy()</c> while this
    /// handler is still on the stack and the OS is still inside its own
    /// <c>WM_CLOSE</c>. Measured on a real window on 2026-08-27: the window then
    /// NEVER CLOSES - <c>Process.CloseMainWindow()</c> returns true and the process is
    /// still alive 60s later. Worse, it fails only on the launch-and-quit case (nothing
    /// unsaved), which is the one a smoke test is least likely to cover. Posting the
    /// funnel means <c>Destroy()</c> can never run inside the OS close it is finishing.</para>
    ///
    /// <para>The drawn button needs no such post: destroying the window from a Click
    /// handler is what this shell shipped for five phases, and no OS close is in
    /// progress there.</para>
    ///
    /// <para>Never throws: an unhandled exception out of a closing handler is a crash
    /// dialog on exit, in the one path nobody exercises interactively.</para>
    /// </summary>
    private void OnAppWindowClosing(
        Microsoft.UI.Windowing.AppWindow sender,
        Microsoft.UI.Windowing.AppWindowClosingEventArgs args)
    {
        if (_closing)
        {
            // The funnel's own Close(), and THIS BRANCH IS THE EXIT. Returning without
            // cancelling is what lets the OS close run through to Window.Closed and the
            // teardown - the funnel has already saved and released the GPU surface by
            // the time control arrives here.
            App.LogDiagnostic("close: the funnel finished; letting the OS close proceed");
            return;
        }

        args.Cancel = true;
        App.LogDiagnostic("close: system arm (AppWindow.Closing) cancelled the OS close and posted the funnel");

        if (!_dispatcher.TryEnqueue(() => _ = RequestCloseAsync()))
        {
            // The queue is already shutting down, so there is no later to defer to and
            // nothing left that could save. Let the close proceed rather than trapping
            // the user in a window that refuses to shut.
            App.LogDiagnostic("close: the dispatcher queue is gone; closing without a save");
            args.Cancel = false;
        }
    }

    /// <summary>
    /// TEARDOWN ONLY, and it is too late to be anything else: <c>Closed</c> fires while
    /// the window is already being destroyed. Stop both rates, then SafeHandle release
    /// → <c>rudis_shutdown</c>, nothing more.
    ///
    /// <para><b>The retired contract, kept so nobody restores it:</b> this used to read
    /// "parity — no extra call (50-02 §2.3). The Tauri app persisted nothing at close
    /// … this shell does exactly the same." That was true while no export could persist
    /// anything on demand. Phase 60.1 added <c>rudis_save_project</c> and the save now
    /// happens in <see cref="RequestCloseAsync"/>, one event EARLIER, because by the
    /// time this handler runs the window is going away and an awaited save has nowhere
    /// to finish. Parity with the retired Tauri shell is no longer the contract.</para>
    ///
    /// <para>Order is load-bearing: both rates stop BEFORE Dispose, so no tick can post
    /// work against a handle that is being released. A poll already in flight is
    /// harmless — the wrapper drains its worker and joins before releasing
    /// (50-02 §1.5(5)), and <c>_closed</c> makes the marshal back a no-op.</para>
    /// </summary>
    private void OnWindowClosed(object sender, WindowEventArgs args)
    {
        App.LogDiagnostic("closed: teardown entered");
        _closed = true;
        _coldTimer?.Stop();
        _coldTimer = null;
        _hotTimer?.Stop();
        _hotTimer = null;
        App.LogDiagnostic("closed: rates stopped, disposing the engine");
        App.Engine?.Dispose();
        App.LogDiagnostic("closed: engine disposed");
    }
}
