// D-05, the phase TRIPWIRE (Phase 52, plan 52-01, Task 2) — the dual-SwapChainPanel
// smoke surface, inside the REAL shell process.
//
// ⚠ THE ENTIRE FILE IS COMPILED OUT OF RELEASE. Everything below — including the
// namespace declaration — lives inside one #if DEBUG block, and there is deliberately NO
// .xaml file for this window: XAML pages compile unconditionally, so a `Timeline
// Smoke.xaml` would put a debug surface in the shipped assembly no matter what the
// code-behind said. Building the visual tree in code is what makes `#if DEBUG` total.
// Absence from Release is asserted MECHANICALLY by
// shell/Rudis.Shell.Tests/ReleaseHookAbsenceTests.cs — the same collectible
// AssemblyLoadContext proof that already polices IntrospectionHook, EXTENDED rather than
// duplicated into a parallel gate (T-52-02).
//
// Even in Debug it is unreachable without `--timeline-smoke` on the command line, and in
// that mode it REPLACES MainWindow rather than being opened beside it.
#if DEBUG
using System.Diagnostics;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Rudis.Shell.Interop;

// ⚠ `Introspection`, not `Debug` — see TimelineSmokeInterop.cs's note; a namespace named
// `Debug` shadows System.Diagnostics.Debug for the whole Rudis.Shell namespace.
namespace Rudis.Shell.Introspection;

/// <summary>
/// Two <see cref="SwapChainPanel"/>s, each bound to its OWN independent <c>wgpu</c> device
/// and swapchain, presenting continuously in one real WinUI 3 window of the real shell
/// process.
///
/// <para><b>What this exists to answer.</b> 52-RESEARCH assumption A1 — "two independent
/// SwapChainPanel-backed wgpu devices can coexist in one WinUI 3 window/process" — is the
/// phase's highest unverified risk, and the answer to it must be a MEASUREMENT, not an
/// extrapolation from "one panel worked" (52-RESEARCH Pitfall 5; Phase 50-03's "prove the
/// mechanism by launching, not by inspection"). So this window exposes a three-phase
/// ablation: panel A alone, panel A with panel B live, panel A alone again after B is
/// detached — with per-panel present counters and inter-present delta percentiles sampled
/// in each phase. Degradation that appears when B starts and reverses when B stops is
/// ATTRIBUTABLE to B; degradation that does not reverse is something else, and the
/// difference is the whole point of running the third phase (plan 50-08's rule).</para>
/// </summary>
internal sealed partial class TimelineSmokeWindow : Window
{
    /// <summary>The argv gate. Debug-only, and it REPLACES MainWindow when present.</summary>
    private const string SmokeFlag = "--timeline-smoke";

    /// <summary>Run the three-phase ablation automatically and write a transcript.</summary>
    private const string AblationFlag = "--timeline-smoke-ablation";

    /// <summary>Where the ablation transcript goes (defaults under the phase artifacts).</summary>
    private const string AblationOutFlag = "--timeline-smoke-out";

    /// <summary>Seconds per ablation phase. 10 by default, per the plan.</summary>
    private const string AblationSecondsFlag = "--timeline-smoke-seconds";

    /// <summary>
    /// Distinct tints so the two surfaces are TELLABLE APART in a screenshot — the visual
    /// half of the evidence, beside the numeric half.
    ///
    /// DECIMAL components, never hex: <c>MechanicalGatesTests.no_raw_hex_outside_the_token_dictionary</c>
    /// scans every .cs and .xaml under shell/Rudis.Shell/, and a debug rectangle is not a
    /// reason to punch a hole in CLAUDE.md rule 7. These are not design tokens and must
    /// not become any — they are a diagnostic, and Theme/Tokens.xaml stays the single
    /// source of truth for anything a user sees.
    /// </summary>
    private static readonly (float R, float G, float B) TintA = (220f / 255f, 40f / 255f, 40f / 255f);

    private static readonly (float R, float G, float B) TintB = (40f / 255f, 120f / 255f, 220f / 255f);

    private readonly SwapChainPanel _panelA = new();
    private readonly SwapChainPanel _panelB = new();
    private readonly TextBlock _readoutA = new();
    private readonly TextBlock _readoutB = new();
    private readonly TextBlock _status = new();
    private readonly DispatcherQueueTimer _pollTimer;
    private readonly List<string> _transcript = [];

    private IntPtr _handleA;
    private IntPtr _handleB;
    private IntPtr _panelPointerA;
    private IntPtr _panelPointerB;
    private bool _panelBReady;
    private bool _ablationStarted;
    private bool _panelsTornDown;
    private bool _closed;

    internal TimelineSmokeWindow()
    {
        Title = "Rudis 52-01 tripwire — two SwapChainPanels, two independent wgpu devices";

        var root = new Grid
        {
            Background = Application.Current.Resources["bg-window"] as Brush,
        };
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });

        _status.Text = "starting…";
        _status.Margin = new Thickness(12, 8, 12, 8);
        _status.TextWrapping = TextWrapping.Wrap;
        _status.Foreground = Application.Current.Resources["text-primary"] as Brush;
        Grid.SetRow(_status, 0);
        root.Children.Add(_status);

        var panels = new Grid();
        panels.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        panels.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        Grid.SetRow(panels, 1);
        root.Children.Add(panels);

        panels.Children.Add(BuildCell(_panelA, _readoutA, "SmokePanelA", 0));
        panels.Children.Add(BuildCell(_panelB, _readoutB, "SmokePanelB", 1));

        Content = root;

        // Attach on EACH panel's OWN Loaded handler. That is the THREAD RULE, satisfied
        // structurally rather than by hoping: wgpu's Surface::configure reaches
        // ISwapChainPanelNative::SetSwapChain, which returns RPC_E_WRONG_THREAD off the
        // thread that owns the panel.
        _panelA.Loaded += OnPanelALoaded;
        _panelB.Loaded += OnPanelBLoaded;

        _pollTimer = DispatcherQueue.CreateTimer();
        _pollTimer.Interval = TimeSpan.FromMilliseconds(500);
        _pollTimer.Tick += (_, _) => RefreshReadouts();
        _pollTimer.Start();

        // AppWindow.Closing fires while the window is STILL ALIVE; Window.Closed fires
        // while it is being destroyed, which is too late to unbind a SwapChainPanel
        // safely (see TeardownPanels). A user closing the window manually therefore goes
        // through Closing; Closed remains only as an idempotent backstop.
        AppWindow.Closing += (_, _) => TeardownPanels();
        Closed += OnWindowClosed;
        SizeWindow();
    }

    // -----------------------------------------------------------------------
    // The argv gate
    // -----------------------------------------------------------------------

    /// <summary>True when this launch asked for the smoke surface instead of MainWindow.</summary>
    internal static bool IsRequested() => HasFlag(SmokeFlag);

    /// <summary>True when the three-phase ablation should run itself and write a transcript.</summary>
    private static bool IsAblationRequested() => HasFlag(AblationFlag);

    private static bool HasFlag(string flag) =>
        Environment.GetCommandLineArgs()
            .Any(a => string.Equals(a, flag, StringComparison.OrdinalIgnoreCase));

    private static string? FlagValue(string flag)
    {
        var args = Environment.GetCommandLineArgs();
        for (var i = 1; i < args.Length - 1; i++)
        {
            if (string.Equals(args[i], flag, StringComparison.OrdinalIgnoreCase))
            {
                return args[i + 1];
            }
        }

        return null;
    }

    /// <summary>
    /// The engine config for a smoke launch: the SAME shape App.BuildInitConfigJson
    /// produces, pointed at a THROWAWAY directory.
    ///
    /// <para>The wgpu-26 coexistence proof performs a REAL import + place + export
    /// (CLAUDE.md rule 1 — nothing about this run is mocked), and a real mutation belongs
    /// nowhere near the developer's actual project store. One temp root per launch, so
    /// two smoke runs cannot collide either.</para>
    /// </summary>
    internal static string IsolatedInitConfigJson()
    {
        var root = Path.Combine(
            Path.GetTempPath(), "rudis-52-01-smoke", Environment.ProcessId.ToString());
        Directory.CreateDirectory(root);
        return JsonSerializer.Serialize(new Dictionary<string, object>
        {
            ["data_dir"] = Path.Combine(root, "data"),
            ["cache_dir"] = Path.Combine(root, "cache"),
            ["resource_dir"] = AppContext.BaseDirectory,
            ["self_advance"] = true,
        });
    }

    // -----------------------------------------------------------------------
    // Visual tree
    // -----------------------------------------------------------------------

    private static Grid BuildCell(SwapChainPanel panel, TextBlock readout, string automationId, int column)
    {
        var cell = new Grid { Margin = new Thickness(8) };
        Grid.SetColumn(cell, column);

        panel.Name = automationId;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(panel, automationId);
        cell.Children.Add(panel);

        readout.Text = $"{automationId}: not attached";
        readout.Margin = new Thickness(8);
        readout.VerticalAlignment = VerticalAlignment.Top;
        readout.HorizontalAlignment = HorizontalAlignment.Left;
        readout.Foreground = Application.Current.Resources["text-primary"] as Brush;
        cell.Children.Add(readout);

        return cell;
    }

    private void SizeWindow()
    {
        try
        {
            var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
            var id = Microsoft.UI.Win32Interop.GetWindowIdFromWindow(hwnd);
            var appWindow = Microsoft.UI.Windowing.AppWindow.GetFromWindowId(id);
            var area = Microsoft.UI.Windowing.DisplayArea.GetFromWindowId(
                id, Microsoft.UI.Windowing.DisplayAreaFallback.Primary);

            var w = Math.Min(1280, area.WorkArea.Width);
            var h = Math.Min(640, area.WorkArea.Height);
            appWindow.MoveAndResize(new Windows.Graphics.RectInt32(
                area.WorkArea.X + ((area.WorkArea.Width - w) / 2),
                area.WorkArea.Y + ((area.WorkArea.Height - h) / 2),
                w, h));
        }
        catch (Exception e)
        {
            Log($"window sizing failed (cosmetic only): {e.GetType().Name}: {e.Message}");
        }
    }

    // -----------------------------------------------------------------------
    // Attach / detach — always on the panel's UI thread
    // -----------------------------------------------------------------------

    private void OnPanelALoaded(object sender, RoutedEventArgs e)
    {
        if (_handleA != IntPtr.Zero)
        {
            return;
        }

        _handleA = Attach(_panelA, TintA, "A", ref _panelPointerA);
        Log($"panel A attach -> {(_handleA == IntPtr.Zero ? "FAILED (null handle)" : TimelineSmokeNative.Describe(_handleA))}");

        if (IsAblationRequested() && !_ablationStarted)
        {
            _ablationStarted = true;
            _ = RunAblationAsync();
        }
    }

    private void OnPanelBLoaded(object sender, RoutedEventArgs e)
    {
        _panelBReady = true;

        // In ablation mode panel B stays UNATTACHED until phase 2 asks for it: phase 1's
        // baseline must be panel A with no second device in the process at all, not panel
        // A beside a paused one.
        if (IsAblationRequested())
        {
            Log("panel B loaded; deliberately NOT attached yet (ablation phase 1 is the A-alone baseline)");
            return;
        }

        StartPanelB();
    }

    private IntPtr Attach(SwapChainPanel panel, (float R, float G, float B) tint, string label, ref IntPtr panelPointer)
    {
        // Physical pixels: the swapchain is sized in device pixels, and CompositionScale
        // is the panel's own logical->physical factor. Carrying Phase 50-05/06's DPI
        // lesson forward — convert explicitly at the boundary, never assume 1.0.
        var scaleX = panel.CompositionScaleX <= 0 ? 1f : panel.CompositionScaleX;
        var scaleY = panel.CompositionScaleY <= 0 ? 1f : panel.CompositionScaleY;
        var width = (uint)Math.Max(16, Math.Round(panel.ActualWidth * scaleX));
        var height = (uint)Math.Max(16, Math.Round(panel.ActualHeight * scaleY));

        panelPointer = TimelineSmokeInterop.PanelPointer(panel);
        if (panelPointer == IntPtr.Zero)
        {
            Log($"panel {label}: MarshalInspectable.FromManaged returned a null pointer");
            return IntPtr.Zero;
        }

        // The scale goes ACROSS the boundary now, not just into the log line below
        // (plan 52-12): the swapchain is sized in physical pixels, so Rust needs it to
        // set the panel's inverse-scale matrix transform.
        var handle = TimelineSmokeNative.rudis_timeline_smoke_attach(
            panelPointer, width, height, scaleX, tint.R, tint.G, tint.B);

        Log($"panel {label}: attach({width}x{height} px, scale {scaleX:0.##}x{scaleY:0.##}) -> " +
            (handle == IntPtr.Zero ? "NULL HANDLE" : "ok"));
        return handle;
    }

    /// <summary>Attach panel B (phase 2 of the ablation, or immediately in manual mode).</summary>
    internal void StartPanelB()
    {
        if (_handleB != IntPtr.Zero || !_panelBReady)
        {
            return;
        }

        _handleB = Attach(_panelB, TintB, "B", ref _panelPointerB);
        Log($"panel B attach -> {(_handleB == IntPtr.Zero ? "FAILED (null handle)" : TimelineSmokeNative.Describe(_handleB))}");
    }

    /// <summary>
    /// Detach panel B FOR REAL — the present thread stops, the device and swapchain drop.
    /// The ablation's "B off" phases are genuinely device-free, not merely paused, which
    /// is what makes phase 3 evidence that any phase-2 degradation was attributable to B.
    /// </summary>
    internal void StopPanelB()
    {
        if (_handleB == IntPtr.Zero)
        {
            return;
        }

        var rc = TimelineSmokeNative.rudis_timeline_smoke_detach(_handleB);
        _handleB = IntPtr.Zero;
        TimelineSmokeInterop.ReleasePanelPointer(_panelPointerB);
        _panelPointerB = IntPtr.Zero;
        _readoutB.Text = "SmokePanelB: detached";
        Log($"panel B detach -> rc={rc}");
    }

    /// <summary>Zero both panels' counters so a phase measures only its own window.</summary>
    internal void ResetStats()
    {
        if (_handleA != IntPtr.Zero)
        {
            TimelineSmokeNative.rudis_timeline_smoke_reset_stats(_handleA);
        }

        if (_handleB != IntPtr.Zero)
        {
            TimelineSmokeNative.rudis_timeline_smoke_reset_stats(_handleB);
        }
    }

    /// <summary>Sample both panels. A detached panel reports all-zero, flagged as such.</summary>
    internal (SmokeStats A, SmokeStats B, bool BAttached) ReadStats()
    {
        var a = default(SmokeStats);
        var b = default(SmokeStats);
        if (_handleA != IntPtr.Zero)
        {
            TimelineSmokeNative.rudis_timeline_smoke_stats(_handleA, out a);
        }

        var bAttached = _handleB != IntPtr.Zero;
        if (bAttached)
        {
            TimelineSmokeNative.rudis_timeline_smoke_stats(_handleB, out b);
        }

        return (a, b, bAttached);
    }

    private void RefreshReadouts()
    {
        var (a, b, bAttached) = ReadStats();
        _readoutA.Text = Format("SmokePanelA", a, _handleA != IntPtr.Zero);
        _readoutB.Text = Format("SmokePanelB", b, bAttached);
    }

    private static string Format(string label, SmokeStats s, bool attached) =>
        attached
            ? $"{label}\nframes {s.FramesPresented}\np50 {s.P50DeltaUs}us  p99 {s.P99DeltaUs}us\n" +
              $"min {s.MinDeltaUs}us  max {s.MaxDeltaUs}us\nerrors {s.PresentErrors}  device_lost {s.DeviceLost}"
            : $"{label}: not attached";

    // -----------------------------------------------------------------------
    // The ablation
    // -----------------------------------------------------------------------

    private async Task RunAblationAsync()
    {
        var seconds = int.TryParse(FlagValue(AblationSecondsFlag), out var parsed) && parsed > 0 ? parsed : 10;
        var settle = TimeSpan.FromSeconds(2);
        var window = TimeSpan.FromSeconds(seconds);

        try
        {
            Log($"ablation: {seconds}s per phase, {settle.TotalSeconds}s settle between changes");
            Log($"timeline lib: {TimelineSmokeNative.Describe(_handleA)}");

            if (_handleA == IntPtr.Zero)
            {
                Log("ABLATION ABORTED: panel A never attached — there is nothing to measure.");
                await FinishAsync();
                return;
            }

            // ---- Phase 1: A alone. B is not attached; no second device exists. --------
            SetStatus("phase 1/3 — panel A alone");
            await Task.Delay(settle);
            ResetStats();
            await Task.Delay(window);
            var (a1, _, _) = ReadStats();
            Record("PHASE1_A", a1);
            Log($"PHASE1_B  (panel B not attached — no second wgpu device in the process)");

            // ---- Phase 2: A and B both presenting. ------------------------------------
            SetStatus("phase 2/3 — panel A + panel B");
            StartPanelB();
            await Task.Delay(settle);
            ResetStats();
            await Task.Delay(window);
            var (a2, b2, b2Attached) = ReadStats();
            Record("PHASE2_A", a2);
            Record("PHASE2_B", b2);
            Log($"PHASE2_B_attached={b2Attached}");
            Log($"panel A: {TimelineSmokeNative.Describe(_handleA)}");
            Log($"panel B: {TimelineSmokeNative.Describe(_handleB)}");

            // ---- Coexistence with the frozen wgpu-26 device (assumption A2, runtime) ---
            await RunExportCoexistenceAsync();

            // ---- Phase 3: A alone again, after B is genuinely detached. ---------------
            SetStatus("phase 3/4 — panel A alone again (B detached)");
            StopPanelB();
            await Task.Delay(settle);
            ResetStats();
            await Task.Delay(window);
            var (a3, _, _) = ReadStats();
            Record("PHASE3_A", a3);
            Log("PHASE3_B  (panel B detached — device and swapchain dropped)");

            // ---- Phase 4: RE-ATTACH panel B. -----------------------------------------
            //
            // ADDED BY PLAN 52-04, to close the open question 52-01 flagged for it
            // (52-01-tripwire.md §5.4, first bullet): "Attach -> detach -> re-attach was
            // not exercised. Whether a panel can be re-bound to a NEW wgpu surface after
            // being unbound is UNPROVEN and is a real question for a Timeline that may
            // need to rebuild its device (e.g. after device_lost)."
            //
            // It is a real question because of what teardown actually does. Step 2 of the
            // detach sequence calls ISwapChainPanelNative::SetSwapChain(null), leaving the
            // XAML panel bound to NOTHING; re-attaching then asks wgpu to create a second
            // swapchain for the same panel and call SetSwapChain on it again, through a
            // DIFFERENT DXGI factory belonging to a DIFFERENT wgpu::Instance. Nothing in
            // Phase 44's spike or 52-01's ablation ever did that, and "it should work" is
            // the exact reasoning 52-01's own crash disproved twice.
            //
            // The methods needed no change — StopPanelB already nulls both the handle and
            // the QI pointer, so StartPanelB re-QIs and re-attaches cleanly. What was
            // missing was a caller. This is that caller.
            SetStatus("phase 4/4 — panel B RE-ATTACHED after a full detach");
            StartPanelB();
            var reattached = _handleB != IntPtr.Zero;
            Log($"PHASE4_reattach_handle={(reattached ? "ok" : "NULL — re-attach FAILED")}");
            await Task.Delay(settle);
            ResetStats();
            await Task.Delay(window);
            var (a4, b4, b4Attached) = ReadStats();
            Record("PHASE4_A", a4);
            Record("PHASE4_B", b4);
            Log($"PHASE4_B_attached={b4Attached}");
            if (reattached)
            {
                Log($"panel B (re-attached): {TimelineSmokeNative.Describe(_handleB)}");
            }

            EvaluateThresholds(a1, a2, a3, b2);
            EvaluateReattach(reattached, b4, a1, a4);
            SetStatus("ablation complete — transcript written");
        }
        catch (Exception e)
        {
            Log($"ABLATION THREW: {e}");
            SetStatus("ablation FAILED — see transcript");
        }

        await FinishAsync();
    }

    /// <summary>
    /// Assumption A2's RUNTIME half: drive a REAL export through the already-shipping ABI
    /// so <c>crates/engine</c>'s wgpu-26 DX12 Compositor is constructed and used in THIS
    /// process while both wgpu-29 swapchains are presenting.
    ///
    /// <para>Runs only when a media path was supplied with <c>--import</c>. The store is a
    /// throwaway temp directory for this launch (see <see cref="IsolatedInitConfigJson"/>),
    /// so a real mutation never touches real project data.</para>
    /// </summary>
    private async Task RunExportCoexistenceAsync()
    {
        var media = App.StartupImportPaths.FirstOrDefault();
        if (media is null)
        {
            Log("COEXIST: skipped — no --import path supplied, so no real export was driven");
            return;
        }

        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            Log("COEXIST: skipped — the engine failed to initialise for this launch");
            return;
        }

        SetStatus("coexistence — real export running while both panels present");
        ResetStats();
        var started = Stopwatch.StartNew();

        try
        {
            var importArgs = new JsonObject { ["paths"] = new JsonArray(media) };
            var imported = await engine.ImportMediaAsync(importArgs.ToJsonString());
            if (imported.Kind != RudisResultKind.Ok)
            {
                Log($"COEXIST: import FAILED ({imported.Kind}) — {imported.RawEnvelope}");
                return;
            }

            var mediaId = imported.Value[0].GetProperty("id").GetString();
            Log($"COEXIST: imported {Path.GetFileName(media)} as {mediaId}");

            var placeArgs = new JsonObject
            {
                ["media_id"] = mediaId,
                ["track"] = 0,
                ["start_us"] = 0,
            };
            var placed = await engine.PlaceClipAsync(placeArgs.ToJsonString());
            if (placed.Kind != RudisResultKind.Ok)
            {
                Log($"COEXIST: place_clip FAILED ({placed.Kind}) — {placed.RawEnvelope}");
                return;
            }

            var outPath = Path.Combine(
                Path.GetTempPath(), $"rudis-52-01-coexist-{Environment.ProcessId}.mp4");
            var exportArgs = new JsonObject
            {
                ["out_path"] = outPath,
                ["width"] = 640,
                ["height"] = 360,
                ["fps"] = 15.0,
            };

            // THE call that builds crates/engine's wgpu-26 DX12 Compositor in this
            // process, while two wgpu-29 devices are presenting.
            var exported = await engine.ExportTimelineAsync(exportArgs.ToJsonString());
            started.Stop();

            if (exported.Kind != RudisResultKind.Ok)
            {
                Log($"COEXIST: export FAILED ({exported.Kind}) after {started.ElapsedMilliseconds}ms — {exported.RawEnvelope}");
                return;
            }

            var writtenPath = exported.Value.GetString();
            var exists = writtenPath is not null && File.Exists(writtenPath);
            var size = exists ? new FileInfo(writtenPath!).Length : 0;
            Log($"COEXIST: export OK in {started.ElapsedMilliseconds}ms -> {writtenPath} exists={exists} bytes={size}");
        }
        catch (Exception e)
        {
            Log($"COEXIST THREW: {e}");
        }
        finally
        {
            var (ax, bx, bAttached) = ReadStats();
            Record("COEXIST_A", ax);
            if (bAttached)
            {
                Record("COEXIST_B", bx);
            }

            Log($"COEXIST: window covered {started.ElapsedMilliseconds}ms of wall clock");
        }
    }

    /// <summary>
    /// The PASS conditions, evaluated here rather than left to prose: zero present errors
    /// and zero device-lost on both panels in every phase, panel A's phase-2 p50 within
    /// 1.5x of phase 1's, and phase 3's p50 back within 1.2x of phase 1's.
    /// </summary>
    private void EvaluateThresholds(SmokeStats a1, SmokeStats a2, SmokeStats a3, SmokeStats b2)
    {
        var errorsClean =
            a1.PresentErrors == 0 && a1.DeviceLost == 0 &&
            a2.PresentErrors == 0 && a2.DeviceLost == 0 &&
            a3.PresentErrors == 0 && a3.DeviceLost == 0 &&
            b2.PresentErrors == 0 && b2.DeviceLost == 0;

        var baseline = a1.P50DeltaUs == 0 ? 1 : a1.P50DeltaUs;
        var phase2Ratio = (double)a2.P50DeltaUs / baseline;
        var phase3Ratio = (double)a3.P50DeltaUs / baseline;
        var framesPlausible = a1.FramesPresented > 0 && a2.FramesPresented > 0 &&
                              a3.FramesPresented > 0 && b2.FramesPresented > 0;

        Log($"GATE errors_all_zero={errorsClean}");
        Log($"GATE phase2_p50_ratio={phase2Ratio:0.###} threshold=1.5 pass={phase2Ratio <= 1.5}");
        Log($"GATE phase3_p50_ratio={phase3Ratio:0.###} threshold=1.2 pass={phase3Ratio <= 1.2}");
        Log($"GATE frames_nonzero_every_phase={framesPlausible}");
        Log($"GATE overall={(errorsClean && phase2Ratio <= 1.5 && phase3Ratio <= 1.2 && framesPlausible ? "PASS" : "FAIL")}");
    }

    /// <summary>
    /// Grade phase 4 — the re-attach cycle plan 52-04 added to answer 52-01 §5.4's first
    /// open question.
    ///
    /// <para>Graded separately from <see cref="EvaluateThresholds"/> on purpose. That
    /// method's verdict is 52-01's, it is quoted in a shipped artifact, and folding a new
    /// condition into it would silently change what a historical PASS meant. A new
    /// question gets a new gate line.</para>
    ///
    /// <para>Three things have to hold, and the third is the interesting one: a panel that
    /// re-binds but presents at a degraded cadence would be a re-attach that "works" and is
    /// useless for the <c>device_lost</c> recovery path this question exists to serve.</para>
    /// </summary>
    private void EvaluateReattach(bool reattached, SmokeStats b4, SmokeStats a1, SmokeStats a4)
    {
        var bClean = b4.PresentErrors == 0 && b4.DeviceLost == 0 && b4.FramesPresented > 0;
        var aClean = a4.PresentErrors == 0 && a4.DeviceLost == 0 && a4.FramesPresented > 0;
        var baseline = a1.P50DeltaUs == 0 ? 1 : a1.P50DeltaUs;
        var ratio = (double)a4.P50DeltaUs / baseline;

        Log($"GATE reattach_handle_nonnull={reattached}");
        Log($"GATE reattach_panelB_presents_cleanly={bClean}");
        Log($"GATE reattach_panelA_undisturbed={aClean}");
        Log($"GATE reattach_phase4_p50_ratio={ratio:0.###} threshold=1.5 pass={ratio <= 1.5}");
        Log($"GATE reattach_overall={(reattached && bClean && aClean && ratio <= 1.5 ? "PASS" : "FAIL")}");
    }

    private void Record(string label, SmokeStats s) =>
        Log($"{label} frames={s.FramesPresented} min_us={s.MinDeltaUs} p50_us={s.P50DeltaUs} " +
            $"p99_us={s.P99DeltaUs} max_us={s.MaxDeltaUs} present_errors={s.PresentErrors} " +
            $"device_lost={s.DeviceLost}");

    private void SetStatus(string text)
    {
        _status.Text = text;
        Log($"STATUS {text}");
    }

    private void Log(string line)
    {
        _transcript.Add($"{DateTime.UtcNow:HH:mm:ss.fff} {line}");
        App.LogDiagnostic($"[52-01] {line}");
    }

    private async Task FinishAsync()
    {
        WriteTranscript();
        await Task.Delay(TimeSpan.FromMilliseconds(500));

        // TEAR THE PANELS DOWN BEFORE Close(), NOT INSIDE Closed.
        //
        // Found by crashing, twice, in this plan's own Task 2 (and this is exactly the
        // class of thing the tripwire exists to surface): unbinding a SwapChainPanel from
        // inside the Closed handler faults with 0xC0000005. By the time Closed fires the
        // XAML window is already being destroyed, and ISwapChainPanelNative::SetSwapChain
        // on a panel in that state is not a supported call. Doing it while the window is
        // still alive is clean — the phase-3 detach of panel B, which happens mid-run,
        // always was.
        TeardownPanels();
        Close();
    }

    /// <summary>
    /// Release both panels' GPU devices and COM references. Idempotent, and safe to call
    /// only while the window is still alive — see <see cref="FinishAsync"/>.
    /// </summary>
    private void TeardownPanels()
    {
        if (_panelsTornDown)
        {
            return;
        }

        _panelsTornDown = true;
        _pollTimer.Stop();
        Log("TEARDOWN begin");

        StopPanelB();
        Log("TEARDOWN panel B released");

        if (_handleA != IntPtr.Zero)
        {
            var rc = TimelineSmokeNative.rudis_timeline_smoke_detach(_handleA);
            _handleA = IntPtr.Zero;
            Log($"TEARDOWN panel A detach -> rc={rc}");
        }

        TimelineSmokeInterop.ReleasePanelPointer(_panelPointerA);
        _panelPointerA = IntPtr.Zero;
        Log("TEARDOWN panel A COM reference released");
    }

    private void WriteTranscript()
    {
        try
        {
            var path = FlagValue(AblationOutFlag)
                ?? Path.Combine(Path.GetTempPath(), $"rudis-52-01-ablation-{Environment.ProcessId}.txt");
            Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(path))!);

            var sb = new StringBuilder();
            sb.AppendLine("# 52-01 tripwire — dual SwapChainPanel ablation transcript");
            sb.AppendLine("# Written by shell/Rudis.Shell/Debug/TimelineSmokeWindow.cs (Debug-only).");
            sb.AppendLine($"# utc={DateTime.UtcNow:O} pid={Environment.ProcessId}");
            sb.AppendLine();
            foreach (var line in _transcript)
            {
                sb.AppendLine(line);
            }

            File.WriteAllText(path, sb.ToString());
        }
        catch (Exception e)
        {
            App.LogDiagnostic($"[52-01] transcript write failed: {e.Message}");
        }
    }

    private void OnWindowClosed(object sender, WindowEventArgs args)
    {
        if (_closed)
        {
            return;
        }

        _closed = true;

        // Normally already done by AppWindow.Closing (or by FinishAsync). This is the
        // belt-and-braces path for a teardown route neither of those covers — and it is
        // exactly the path that must NOT be the primary one, per TeardownPanels' note.
        TeardownPanels();

        // The SAME close contract MainWindow honours (MainWindow.xaml.cs:420-427):
        // SafeHandle release -> rudis_shutdown. Omitting it left the engine ctx (and its
        // runtime threads) live into process teardown, which crashed this window's first
        // run at exit — after the transcript was written, so the measurement was intact
        // but the shutdown was not. Order is load-bearing: GPU devices and COM references
        // go first, the engine last.
        App.Engine?.Dispose();
        Log("TEARDOWN engine disposed — complete");
        WriteTranscript();
    }
}
#endif
