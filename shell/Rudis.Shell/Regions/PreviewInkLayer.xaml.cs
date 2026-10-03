using System.Diagnostics;
using System.Globalization;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Rudis.Shell.Interop;

namespace Rudis.Shell.Regions;

/// <summary>
/// The Canvas region's ink layer over the Preview swapchain — pointer capture, the
/// live trail, and the commit that turns a finished gesture into a REAL undoable
/// backend annotation.
///
/// <para><b>Every capability of the retired WndProc subclass, and three it never
/// had.</b> src-tauri/src/canvas_input.rs intercepted raw <c>WM_LBUTTONDOWN</c>/
/// <c>WM_MOUSEMOVE</c>/<c>WM_LBUTTONUP</c> on the video HWND. It never called
/// <c>SetCapture</c>, never read pressure, and could not recover a point the message
/// queue coalesced away. This layer uses ordinary WinUI hit-testing plus
/// <see cref="UIElement.CapturePointer"/> (a strict upgrade: delivery is guaranteed
/// when the drag leaves the element's bounds) and
/// <see cref="PointerRoutedEventArgs.GetIntermediatePoints"/> (the full coalesced
/// history since the last event, which legacy mouse messages structurally cannot
/// provide). There is no capability gap here — only an implementation swap.</para>
///
/// <para><b>⚠ THE COMMIT SEQUENCE IS THE POINT OF THIS FILE.</b> See
/// <see cref="CommitAsync"/>. D-13 splits one drawing path into two for the first
/// time, and the ordering there is what keeps the seam invisible. It is not
/// arbitrary and it is not simplifiable — the remarks say why in full.</para>
///
/// <para><b>Zero allocation at rest</b> (SHELL-06 / D-15 part 2, gated by plan
/// 51-07). <see cref="OnPointerMoved"/> returns BEFORE
/// <c>GetIntermediatePoints</c> when no gesture is in progress, because that call
/// allocates a list on every invocation and hovering over the video is the common
/// case. Nothing here runs per frame at all.</para>
/// </summary>
public sealed partial class PreviewInkLayer : UserControl
{
    /// <summary>
    /// How long the finished trail may stay on screen waiting for the mirror to
    /// confirm the annotation landed, before it is cleared REGARDLESS.
    ///
    /// <para>A stuck trail is worse than a flicker, and an unbounded wait on a
    /// visual element is not acceptable at any latency. 750 ms is generous against
    /// the cold poll's own 100 ms cadence (MainWindow.ColdPollIntervalMs) — it is a
    /// backstop for a wedged backend, not a budget anything normal should approach.
    /// When it fires, the timeout is logged, not swallowed.</para>
    /// </summary>
    private const int MirrorConfirmTimeoutMs = 750;

    /// <summary>Confirmation poll cadence. Deliberately FINER than the shell's
    /// 100 ms cold poll so the observed latency is the mirror's, not this timer's,
    /// and so the trail is held for the shortest interval that is actually
    /// safe.</summary>
    private const int MirrorConfirmPollMs = 25;

    private readonly DispatcherQueue _dispatcher;

    /// <summary>The gesture's NORMALIZED points — the authoritative list. The trail
    /// is rendered FROM this (never the other way round) so what is drawn and what is
    /// dispatched cannot diverge, and so a resize can re-place the trail exactly.</summary>
    private readonly List<NormPoint> _points = new(CanvasGesture.MaxPoints);

    private DrawTool _tool = DrawTool.Pointer;

    private bool _gestureActive;

    /// <summary>True between "the trail was frozen at pointer-up" and "the trail was
    /// cleared". While set, <see cref="RenderTrail"/> keeps drawing the finished
    /// stroke — that is the whole hand-off design.</summary>
    private bool _trailFrozen;

    /// <summary>The content rect the CURRENT gesture is normalized against, latched
    /// at pointer-down. Re-read on resize so a mid-gesture layout change re-places
    /// the trail rather than smearing it.</summary>
    private RudisPreviewRect _rect;

    /// <summary>DIP→physical factor, pushed by the Preview region from the SAME
    /// <c>CompositionScaleX</c> it sizes the swapchain buffer with (D-10). One source
    /// of truth; <see cref="EnsureScale"/> only falls back when nothing has pushed
    /// yet.</summary>
    private double _scale;

    /// <summary>The active monitor's playhead at gesture START (v6.0 captures it at
    /// "down" and reads the other end at "up").</summary>
    private long? _gestureStartUs;

    private DispatcherQueueTimer? _confirmTimer;

    /// <summary>The geometry the last committed gesture was normalized against,
    /// appended to the commit note so the measurement carries its own inputs.</summary>
    private string _geometryNote = string.Empty;

    /// <summary>The XAML-declared hit-testable brush, captured once so
    /// <see cref="ApplyToolGate"/> can put it back. Declared in markup (not built
    /// here) because that is where a reader looks for it, and where the raw-hex gate
    /// can see it.</summary>
    private readonly Brush? _hitTestBrush;

    public PreviewInkLayer()
    {
        InitializeComponent();
        _hitTestBrush = InkRoot.Background;
        _dispatcher = DispatcherQueue.GetForCurrentThread();

        InkRoot.PointerPressed += OnPointerPressed;
        InkRoot.PointerMoved += OnPointerMoved;
        InkRoot.PointerReleased += OnPointerReleased;
        InkRoot.PointerCaptureLost += OnPointerCaptureLost;
        InkRoot.PointerCanceled += OnPointerCaptureLost;
        SizeChanged += OnLayerSizeChanged;

        ApplyToolGate();
    }

    /// <summary>
    /// The selected <c>Canvas › ToolPalette</c> tool. Setting it is the ONLY thing
    /// that makes this layer hit-testable: with <see cref="DrawTool.Pointer"/> the
    /// whole layer is inert and every pointer event reaches whatever is beneath.
    ///
    /// <para>Changing tools mid-gesture ABANDONS the gesture rather than committing
    /// a half-drawn shape under the new tool's rules — which would produce, say, a
    /// lasso from points the user drew as a pen stroke.</para>
    /// </summary>
    internal DrawTool SelectedTool
    {
        get => _tool;
        set
        {
            if (_tool == value)
            {
                return;
            }

            _tool = value;
            if (_gestureActive)
            {
                AbandonGesture("tool changed mid-gesture");
            }
            ApplyToolGate();
        }
    }

    /// <summary>The last commit's measurement, published onto
    /// <c>Canvas.PreviewInk</c>'s <c>AutomationProperties.HelpText</c> so plan
    /// 51-05's ink-hand-off observation can READ the real dispatch→mirror latency
    /// through UIA instead of estimating it from screenshots. Written once per
    /// commit, on the element it describes — the shape plan 51-04 established for the
    /// attach measurement after the per-tick alternative measurably reddened the GC
    /// gate.</summary>
    internal string CommitNote { get; private set; } = "(no gesture committed yet)";

    /// <summary>
    /// Pushed by the Preview region from its own <c>CompositionScaleX</c> — the same
    /// scalar it sends across the ABI as <c>rudis_preview_resize</c>'s scale (D-10).
    /// A field write, no allocation: it is called from <c>PublishSize</c>, which is
    /// gated to zero bytes.
    /// </summary>
    internal void SetCompositionScale(double scale)
    {
        if (double.IsFinite(scale) && scale > 0)
        {
            _scale = scale;
        }
    }

    // ── pointer ────────────────────────────────────────────────────────────────

    private void OnPointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (_tool == DrawTool.Pointer)
        {
            return;
        }

        if (!TryLatchGeometry())
        {
            // `rudis_preview_content_rect` publishes 0x0 until a composite has
            // actually happened. Refusing the gesture is the honest answer: every
            // point would normalize to the origin, and a stroke of 500 identical
            // points is a REAL annotation the user never drew.
            App.LogDiagnostic(
                "canvas ink: gesture refused — the engine reports no preview content rect yet " +
                "(nothing has been composited, so there is nothing to align a mark to)");
            return;
        }

        // A strict upgrade over the retiring Win32 code, which never called
        // SetCapture at all: delivery is guaranteed once the drag leaves this
        // element's bounds.
        //
        // ⚠ MEASURED, and recorded because it was a real suspect: capture is NOT
        // what suppresses the live trail's composition over the SwapChainPanel.
        // Plan 51-05's observation was re-run with this call removed and the
        // mid-drag reading was unchanged (paused: exactly the ink-free baseline;
        // playing: delta -13 px against a playing control). The capability is kept
        // because it is genuinely wanted; the finding is independent of it.
        InkRoot.CapturePointer(e.Pointer);

        _gestureActive = true;
        _trailFrozen = false;
        _points.Clear();
        _gestureStartUs = App.Mirror?.ActivePlayback?.PositionUs;

#if DEBUG
        // Plan 51-10's MEASUREMENT, and nothing else — no sampling behaviour is
        // changed by it. The recorded stream for THIS gesture starts here, cleared
        // at pointer-down so one run reads exactly one gesture's worth of rows.
        ResetInkSamples();
        BeginSamplePhase("down", 0, default);
#endif
        AppendFromLayerPoint(e.GetCurrentPoint(InkRoot).Position);
        RenderTrail();
        e.Handled = true;
    }

    /// <summary>
    /// ⚠ The early return is a MEASURED requirement, not a micro-optimisation: with
    /// a draw tool selected the layer is hit-testable for the whole tool selection,
    /// so this handler fires on every hover sample across the video — and
    /// <c>GetIntermediatePoints</c> allocates a list on each call. Plan 51-07 gates
    /// pointer-events-at-rest at zero bytes; this is written right the first time.
    /// </summary>
    private void OnPointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (CanvasGesture.ShouldIgnoreMove(_gestureActive))
        {
            return;
        }

        // The FULL coalesced history since the last event — points the OS batched
        // that a per-event read would silently drop. The legacy mouse messages the
        // retired WndProc subclass read could not recover these at all, so this is a
        // capability GAIN over what is being replaced.
        //
        // Ordering: the collection is documented most-recent-FIRST, so it is walked
        // backwards to append in chronological order. Getting this backwards would
        // draw every stroke as a zig-zag, which is loud rather than subtle.
        var coalesced = e.GetIntermediatePoints(InkRoot);
        if (coalesced is { Count: > 0 })
        {
#if DEBUG
            // H2's discriminator (plan 51-10): how many entries the coalesced read
            // offered on THIS event, and where the chronologically-FIRST of them sat.
            BeginSamplePhase("coalesced", coalesced.Count, coalesced[^1].Position);
#endif
            for (var i = coalesced.Count - 1; i >= 0; i--)
            {
                AppendFromLayerPoint(coalesced[i].Position);
            }
        }

        // ⚠ AND THEN THE CURRENT POSITION, ALWAYS — not as an `else`.
        //
        // MEASURED during plan 51-05's own hand-off observation: with synthetic input
        // (and, per the same reports, with some real high-frequency devices) the
        // coalesced collection came back NON-EMPTY on every move and every entry
        // carried the POINTER-DOWN position. A nine-sample drag across 300 physical
        // pixels produced nine identical points, i.e. a zero-length polyline, i.e. a
        // trail that renders nothing at all — which is exactly what the first
        // observation run captured, frame after frame, while every assertion in the
        // C# tier stayed green. The authoritative current position is therefore
        // always appended on top of whatever history the coalesced read offered, and
        // the ceiling in `AccumulatePoint` bounds the cost of doing both.
#if DEBUG
        // H1's discriminator (plan 51-10): the AUTHORITATIVE current position, tagged
        // as its own row so it can be compared, event by event, against both the
        // coalesced history above and the OS cursor the recorder reads independently.
        BeginSamplePhase(
            "current",
            coalesced?.Count ?? 0,
            coalesced is { Count: > 0 } ? coalesced[^1].Position : default);
#endif
        AppendFromLayerPoint(e.GetCurrentPoint(InkRoot).Position);

        RenderTrail();
        e.Handled = true;
    }

    private void OnPointerReleased(object sender, PointerRoutedEventArgs e)
    {
        if (!_gestureActive)
        {
            return;
        }

#if DEBUG
        BeginSamplePhase("up", 0, default);
#endif
        AppendFromLayerPoint(e.GetCurrentPoint(InkRoot).Position);
        // The release point is part of the stroke, so the FROZEN trail must include
        // it. Without this the trail held across the hand-off is one sample short of
        // what was dispatched — a small, permanent disagreement between the two
        // layers at exactly the instant they are supposed to be indistinguishable.
        RenderTrail();
        InkRoot.ReleasePointerCapture(e.Pointer);
        FinishGesture();
        e.Handled = true;
    }

    /// <summary>Capture lost / cancelled (another element took the pointer, the
    /// window lost activation, a touch was cancelled). The gesture is FINISHED, not
    /// abandoned: the user drew a real mark and lifting the pointer outside the
    /// element is a legitimate way to end it.</summary>
    private void OnPointerCaptureLost(object sender, PointerRoutedEventArgs e)
    {
        if (!_gestureActive)
        {
            return;
        }

        FinishGesture();
    }

    private void OnLayerSizeChanged(object sender, SizeChangedEventArgs e)
    {
        // A layout change moves the contain-fit content rect, so the normalized
        // points now map somewhere else. Re-latch and re-draw from the SAME
        // normalized list, which is why the trail is rendered from it rather than
        // from raw pointer coordinates: a resize mid-gesture re-places the trail
        // instead of smearing it.
        if (!_gestureActive && !_trailFrozen)
        {
            return;
        }

        if (TryLatchGeometry())
        {
            RenderTrail();
        }
    }

    // ── the commit ─────────────────────────────────────────────────────────────

    private void FinishGesture()
    {
        _gestureActive = false;

        // ⚠ THE TRAIL IS *NOT* CLEARED HERE. It is frozen — it stops growing and
        // stays visible. See CommitAsync's remarks: clearing at pointer-up is the
        // naive sequence and it is the one that produces a visible gap.
        _trailFrozen = true;

        var startUs = _gestureStartUs;
        _gestureStartUs = null;

        var tool = _tool;
        var points = _points.ToArray();

#if DEBUG
        // Published ON THE UI THREAD as ONE immutable string (T-51-36). The
        // introspection pipe's background thread reads that string and nothing else —
        // no XAML object is reachable from it, which is the difference between a torn
        // read and RPC_E_WRONG_THREAD.
        PublishInkSampleSnapshot(points);
#endif
        _ = CommitAsync(tool, points, startUs);
    }

    /// <summary>
    /// <b>THE HAND-OFF SEQUENCE (D-13, plan 51-05 Task 1).</b>
    ///
    /// <para>Today — and in every shipped version until this file — ONE engine
    /// function draws both the in-progress trail and the committed ink, in the same
    /// pass, every present tick (crates/preview/src/overlay.rs's
    /// <c>annotated_clone</c> reads <c>live_gesture()</c> and <c>resolve_overlay()</c>
    /// together). D-13 splits that for the first time: XAML draws the live trail, the
    /// engine composites the committed mark. The split opens a seam at exactly one
    /// moment — the instant a gesture commits — and this ordering is what closes
    /// it:</para>
    /// <code>
    /// pointer-up
    ///   -> freeze the trail (stop appending; KEEP IT VISIBLE)
    ///   -> dispatch add_annotation
    ///   -> await the dispatch's own outcome (the backend has committed; the fade
    ///      anchor is set at apply_patch_to_overlay time, i.e. now)
    ///   -> wait for the mirror to report the annotation present in
    ///      Project.canvas.annotations
    ///   -> THEN clear the trail
    /// </code>
    ///
    /// <para><b>Why this order, stated so a later "simplification" is recognisably a
    /// regression.</b> The naive sequence clears the trail at pointer-up. Between
    /// that (immediate, sub-millisecond) and the present thread's next composite of
    /// the now-committed annotation (bounded by the present tick), NEITHER layer
    /// draws the stroke — a visible GAP. Holding the trail until the mirror confirms
    /// converts that gap into a brief double-draw of identical geometry: the XAML
    /// trail and the engine's committed ink are the same points, at the same
    /// letterboxed position, in the same `accent` ink, both dashed. Drawing the same
    /// dashed stroke twice in the same place is visually indistinguishable from
    /// drawing it once; drawing it ZERO times is a flicker the eye catches
    /// immediately. That asymmetry — gap is visible, double-draw is not — is the
    /// entire reason for the ordering, and it is written down here so that
    /// "simplifying" this back to clear-at-pointer-up is recognisably a
    /// REGRESSION rather than a tidy-up.</para>
    ///
    /// <para>Bounded at <see cref="MirrorConfirmTimeoutMs"/> ms by a
    /// <see cref="DispatcherQueueTimer"/> — never a blocking wait, which on the UI
    /// thread would deadlock against the very continuation it is waiting for (and is
    /// mechanically forbidden by <c>no_blocking_waits_in_shell_sources</c>).</para>
    /// </summary>
    private async Task CommitAsync(DrawTool tool, NormPoint[] points, long? startUs)
    {
        try
        {
            string? text = null;
            if (tool == DrawTool.Text)
            {
                text = await PromptForLabelAsync();
            }

            var shape = CanvasGesture.BuildAnnotation(tool, points, text);
            if (shape is null)
            {
                // A structurally invalid gesture (a 2-point lasso, an empty stroke,
                // a cancelled label). The backend would refuse it; the client
                // refuses it first so a DOOMED command is never dispatched. Nothing
                // was committed, so there is nothing for the engine to draw and the
                // trail goes immediately.
                ClearTrail();
                return;
            }

            var engine = App.Engine;
            if (engine is null || engine.IsInvalid)
            {
                App.LogDiagnostic("canvas ink: no engine instance — the mark was NOT committed");
                ClearTrail();
                return;
            }

            // v6.0's id shape, character for character: `a-${crypto.randomUUID().slice(0, 8)}`.
            var id = "a-" + Guid.NewGuid().ToString("N")[..8];
            var linked = ResolveLinkedRange(startUs);
            var args = CanvasGesture.BuildDispatchArgs(id, shape, linked, space: "frame_linked");

            var clock = Stopwatch.StartNew();
            var result = await engine.DispatchCommandAsync(args.ToJsonString());
            var dispatchMs = clock.ElapsedMilliseconds;

            if (result.Kind != RudisResultKind.Ok)
            {
                // A backend refusal must be VISIBLE, never swallowed — the same rule
                // plan 50-05 applies to import. The trail goes because nothing was
                // committed and holding it would imply otherwise.
                Publish(
                    $"commit {id} REFUSED after {dispatchMs}ms: {result.Kind}/{result.Status} — {result.Error}");
                ClearTrail();
                return;
            }

            // The geometry the gesture was normalized against, carried into the
            // commit note. Plan 51-05's hand-off observation reads this note back out
            // through UIA, and a note that says only "committed" cannot tell a trail
            // that was never drawn from one drawn in the wrong place — which is the
            // exact ambiguity that observation had to resolve by hand.
            var norm = new System.Text.StringBuilder();
            for (var i = 0; i < points.Length && i < 6; i++)
            {
                norm.Append($" n{i}=({points[i].X:0.###},{points[i].Y:0.###})");
            }
            var first = points.Length > 0 ? ToLayerPoint(points[0]) : default;
            var last = points.Length > 0 ? ToLayerPoint(points[^1]) : default;
            _geometryNote =
                $"rect {_rect.Width}x{_rect.Height}@({_rect.X},{_rect.Y}) scale {_scale:0.###} " +
                $"pts {points.Length} drawn={LiveTrail.Data is not null} " +
                $"layer ({first.X:0},{first.Y:0})→({last.X:0},{last.Y:0}) ·" + norm;

            AwaitMirrorThenClear(id, clock, dispatchMs);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"canvas ink: commit faulted: {ex.GetType().Name}: {ex.Message}");
            ClearTrail();
        }
    }

    /// <summary>
    /// The bounded half of the hand-off: hold the frozen trail until the mirror has
    /// the annotation, then clear. Polls the mirror rather than subscribing to
    /// <c>ProjectChanged</c> because the mirror IS what the patch-apply updates —
    /// polling it observes the same fact one dispatcher turn later at worst, with no
    /// subscription to unhook on every teardown path.
    /// </summary>
    private void AwaitMirrorThenClear(string id, Stopwatch clock, long dispatchMs)
    {
        StopConfirmTimer();

        if (MirrorHasAnnotation(id))
        {
            Publish($"commit {id}: dispatch {dispatchMs}ms · mirror already current (0ms)");
            ClearTrail();
            return;
        }

        var timer = _dispatcher.CreateTimer();
        _confirmTimer = timer;
        timer.Interval = TimeSpan.FromMilliseconds(MirrorConfirmPollMs);
        timer.IsRepeating = true;
        timer.Tick += (_, _) =>
        {
            var elapsed = clock.ElapsedMilliseconds;
            if (MirrorHasAnnotation(id))
            {
                StopConfirmTimer();
                Publish(
                    $"commit {id}: dispatch {dispatchMs}ms · mirror-confirmed {elapsed}ms · trail cleared");
                ClearTrail();
                return;
            }

            if (elapsed >= MirrorConfirmTimeoutMs)
            {
                StopConfirmTimer();
                Publish(
                    $"commit {id}: dispatch {dispatchMs}ms · mirror NOT confirmed within " +
                    MirrorConfirmTimeoutMs.ToString(CultureInfo.InvariantCulture) +
                    "ms — trail cleared anyway");
                ClearTrail();
            }
        };
        timer.Start();
    }

    private void StopConfirmTimer()
    {
        _confirmTimer?.Stop();
        _confirmTimer = null;
    }

    /// <summary>
    /// Is the committed mark visible in the shell's mirror of
    /// <c>Project.canvas.annotations</c>?
    ///
    /// <para>Read from the mirror's RAW node rather than a typed projection:
    /// <c>Mirror/Models.cs</c>'s <c>Project</c> record deliberately does not project
    /// <c>canvas</c> ("under-projecting is free; a wrong projection is not"), and
    /// this is a presence check by id — the only thing the hand-off needs to know.
    /// Adding a typed canvas projection to satisfy one boolean would be the wrong
    /// trade, and it would be a SECOND managed model of a backend-owned type
    /// (CLAUDE.md rule 4).</para>
    /// </summary>
    private static bool MirrorHasAnnotation(string id)
    {
        var annotations = App.Mirror?.RawProject?["canvas"]?["annotations"] as JsonArray;
        if (annotations is null)
        {
            return false;
        }

        foreach (var node in annotations)
        {
            if (node?["id"] is JsonValue value &&
                value.TryGetValue<string>(out var found) &&
                found == id)
            {
                return true;
            }
        }

        return false;
    }

    /// <summary>
    /// v6.0's rule (main.ts:2591-2597), unchanged: the ACTIVE monitor's playhead at
    /// gesture start → at gesture end, SORTED so a drag whose playhead moved
    /// backwards still yields <c>start &lt;= end</c>. Falls back to a point range
    /// when no start was captured, and to <see langword="null"/> when there is no
    /// playback state at all.
    /// </summary>
    private static (long Start, long End)? ResolveLinkedRange(long? startUs)
    {
        var endUs = App.Mirror?.ActivePlayback?.PositionUs;
        if (endUs is not { } end)
        {
            return null;
        }

        return startUs is { } start ? CanvasGesture.SortedRange(start, end) : (end, end);
    }

    /// <summary>
    /// The WinUI replacement for v6.0's <c>window.prompt("Label text:")</c>. An empty
    /// or cancelled result yields <see langword="null"/>, which makes
    /// <c>BuildAnnotation</c> return null, which means NO dispatch — the same
    /// three-step refusal the frontend has.
    /// </summary>
    private async Task<string?> PromptForLabelAsync()
    {
        var input = new TextBox
        {
            PlaceholderText = "Label text",
            AcceptsReturn = false,
        };
        AutomationProperties.SetAutomationId(input, "Canvas.TextDialog.Input");
        AutomationProperties.SetName(input, "Label text");

        var dialog = new ContentDialog
        {
            Title = "Add a label",
            Content = input,
            PrimaryButtonText = "Add",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = InkRoot.XamlRoot,
        };
        AutomationProperties.SetAutomationId(dialog, "Canvas.TextDialog");
        AutomationProperties.SetName(dialog, "Add a label");

        var outcome = await dialog.ShowAsync();
        return outcome == ContentDialogResult.Primary && input.Text.Length > 0 ? input.Text : null;
    }

    // ── geometry + rendering ───────────────────────────────────────────────────

    /// <summary>
    /// Latch the ENGINE's contain-fit content rect and the composition scale for the
    /// current gesture. The rect is the one source of truth for where the picture
    /// actually is inside the panel; nothing here re-derives contain-fit (D-12).
    /// </summary>
    private bool TryLatchGeometry()
    {
        EnsureScale();

        var engine = App.Engine;
        if (engine is null || !engine.TryGetPreviewContentRect(out var rect))
        {
            return false;
        }

        if (rect.Width == 0 || rect.Height == 0)
        {
            return false;
        }

        _rect = rect;
        return true;
    }

    /// <summary>The Preview region pushes the authoritative value (D-10). This only
    /// covers the window before the first push — a fallback, not a second source of
    /// truth.</summary>
    private void EnsureScale()
    {
        if (_scale > 0)
        {
            return;
        }

        var fromRoot = InkRoot.XamlRoot?.RasterizationScale ?? 0;
        _scale = fromRoot > 0 ? fromRoot : 1.0;
    }

    private void AppendFromLayerPoint(Windows.Foundation.Point p)
    {
        var normalized = CanvasGesture.Normalize(p.X, p.Y, _scale, _rect);
#if DEBUG
        RecordInkSample(p, normalized);
#endif
        // ⚠ DISTINCT, not merely bounded (plan 51-10). `OnPointerMoved` appends the
        // coalesced entry AND the authoritative current position on every event, and
        // the recorded stream shows those two are the SAME point on every event of a
        // real injected-input drive — so half of every gesture's 500-point budget was
        // being spent re-recording positions the list already had. The recorder above
        // still sees EVERY sample; only what is committed is de-duplicated.
        CanvasGesture.AccumulateDistinctPoint(_points, normalized);
    }

    /// <summary>
    /// Layer-DIP coordinates for a normalized point, through the SAME rect the point
    /// was normalized with. Round-tripping through normalized space — rather than
    /// keeping the raw pointer coordinates — is what guarantees the trail sits
    /// EXACTLY where the engine will draw the committed mark, which is the property
    /// the hand-off depends on.
    /// </summary>
    private Windows.Foundation.Point ToLayerPoint(NormPoint n)
    {
        // Total by construction: `_scale` is latched > 0 before any point is taken,
        // and the local guard keeps this a pure function of its inputs rather than
        // one that trusts call ordering for its divisor.
        var scale = _scale > 0 ? _scale : 1.0;
        return new Windows.Foundation.Point(
            (_rect.X + (n.X * _rect.Width)) / scale,
            (_rect.Y + (n.Y * _rect.Height)) / scale);
    }

    /// <summary>
    /// Rebuild the trail's geometry from the normalized list and ASSIGN it.
    ///
    /// <para><b>⚠ MEASURED — this is the second shape this method has had, and the
    /// first two did not draw.</b> Plan 51-05's own hand-off observation is what
    /// caught it: frame after captured frame showed no live trail while every
    /// assertion in the C# tier was green. Both <c>Polyline</c> shapes failed —
    /// assigning a fresh <see cref="PointCollection"/> to <c>Points</c> did not
    /// re-render, and mutating the collection the Shape was handed re-rendered on
    /// <c>Clear</c> (the XAML-set value visibly vanished) but not on the subsequent
    /// <c>Add</c>s. Replacing <c>Path.Data</c> with a NEW <see cref="Geometry"/> is a
    /// plain DependencyProperty set with a new value, which invalidates
    /// unambiguously.</para>
    ///
    /// <para>The cost is bounded and off the per-frame path: one small geometry per
    /// pointer EVENT (not per coalesced point), only while a gesture is in progress.
    /// A gesture is a human-timescale interaction; the per-frame path has no managed
    /// code on it at all (SHELL-06).</para>
    ///
    /// <para>A single point draws nothing — a zero-length dashed stroke has no
    /// pixels — which is correct: one sample is a tap, not a trail. The engine draws
    /// the same geometry the same way once it commits.</para>
    /// </summary>
    private void RenderTrail()
    {
        if (_points.Count < 2)
        {
            LiveTrail.Data = null;
            return;
        }

        var segment = new PolyLineSegment();
        for (var i = 1; i < _points.Count; i++)
        {
            segment.Points.Add(ToLayerPoint(_points[i]));
        }

        var figure = new PathFigure
        {
            StartPoint = ToLayerPoint(_points[0]),
            IsClosed = false,
            IsFilled = false,
        };
        figure.Segments.Add(segment);

        var geometry = new PathGeometry();
        geometry.Figures.Add(figure);
        LiveTrail.Data = geometry;

        // ⚠ MEASURED, and the reason this is here rather than left to the framework.
        //
        // A Shape's size is derived from its geometry, so the very first assignment
        // grows it from 0x0 — and until a layout pass runs, the arranged bounds a
        // Shape paints into are still empty. Plan 51-05's hand-off observation
        // captured exactly that: mid-drag, with the pointer held down and 250ms of
        // idle time, the crop was byte-identical to the ink-free baseline, while the
        // SAME geometry became visible the moment the gesture ended and an ordinary
        // layout pass ran. The trail must repaint at POINTER cadence, not at
        // whenever-something-else-invalidates cadence, so the pass is forced here.
        //
        // Bounded and off the per-frame path (SHELL-06): this runs once per pointer
        // event while a gesture is in progress — a human-timescale interaction —
        // and never at rest. `OnPointerMoved` returns before reaching this when no
        // gesture is running.
        LiveTrail.UpdateLayout();
    }

    private void ClearTrail()
    {
        _trailFrozen = false;
        _points.Clear();
        LiveTrail.Data = null;
    }

    private void AbandonGesture(string why)
    {
        StopConfirmTimer();
        _gestureActive = false;
        _gestureStartUs = null;
        ClearTrail();
        App.LogDiagnostic("canvas ink: gesture abandoned — " + why);
    }

    /// <summary>
    /// Pointer = FULLY inert: not hit-testable AND painting nothing. Every event
    /// falls through to whatever is beneath, exactly as v6.0's <c>DRAW_MODE</c> gate
    /// behaved.
    ///
    /// <para><b>The BACKGROUND is toggled too.</b> A `Transparent` brush is a real
    /// brush that covers the whole region — it is what makes the layer hit-testable
    /// at all (a null brush is not), and it is load-bearing while drawing. Painting
    /// nothing when there is nothing to draw makes the inert state genuinely inert
    /// rather than merely non-hit-testable, and costs nothing.</para>
    ///
    /// <para>⚠ RECORDED so it is not re-derived: this was FIRST written as a fix for
    /// plan 51-04's <c>disabled_and_empty_states_are_visible_to_uia</c>, on the
    /// hypothesis that a covering sibling declared after <c>Preview.EmptyState</c>
    /// removed that element from what UIA reports. That hypothesis is WRONG. It was
    /// tested the way plan 51-04's own GC red was attributed — by measuring instead
    /// of reasoning: the whole ink layer was removed from <c>Preview.xaml</c>, the
    /// shell rebuilt, and the test STILL failed. Whatever collapses that empty state
    /// is not this layer. The toggle is kept on its own merits.</para>
    ///
    /// <para><b>AND THE CURSOR</b> (D-60.2-12, quick task 260828-gb9). The same
    /// question — "is a drawing tool selected?" — decided in the same place, through
    /// <see cref="InkCursorGate.WantsInkCursor"/>, which the Canvas stage also asks:
    /// one predicate, two surfaces, so they cannot drift into disagreeing about what
    /// the pointer looks like. The stage needed a dedicated cursor LAYER to keep the
    /// crosshair off its tool palette; this UserControl needs none, because its
    /// content is <c>InkRoot</c> + <c>LiveTrail</c> and there is no chrome inside it
    /// to leak onto. Defence in depth for free: with <c>Pointer</c> selected the two
    /// lines above make the layer non-hit-testable, so it cannot be the hit-test
    /// target at all and could not contribute a cursor even if one were set.</para>
    ///
    /// <para><b>⚠ THE QUIET PART, SAID (review IN-02): the crosshair DOES cover
    /// chrome here — <c>Preview.MonitorTabs</c> — and that is correct for the OPPOSITE
    /// reason to the stage's.</b> The sentence above is true of this control's own
    /// content, but this control is declared as the LAST child of <c>PreviewRoot</c>
    /// (Preview.xaml), ABOVE the Timeline/Source tab buttons. With a draw tool
    /// selected <c>InkRoot</c> is full-bleed and hit-testable, so the crosshair reaches
    /// those tabs. It does NOT lie: the same two lines that make it hit-testable also
    /// make it SWALLOW the press, so a click on the tabs while drawing does nothing —
    /// which is exactly what Preview.xaml already records ("the tabs are still
    /// clickable through it: <c>InkRoot.IsHitTestVisible</c> is false until a drawing
    /// tool is chosen"). The stage needs a chrome exclusion because its chrome stays
    /// LIVE while drawing; the Preview needs none because its chrome is already inert
    /// while drawing. Same rule — the cursor and the press must agree — reached from
    /// opposite ends, and the asymmetry is written down here so a later reader does not
    /// "fix" this surface to match the other one.</para>
    /// </summary>
    // ⚠ A PIN WAS RELEASED TO WRITE THE LINE BELOW — READ THIS BEFORE CONCLUDING A
    // REQUIREMENT IS BROKEN. This file was pinned BYTE-UNCHANGED by requirement
    // STAGE-04 for the duration of Phase 60.2 ONLY: the construction half of that
    // requirement is that the `frame_linked` path was provably untouched by the phase
    // that added the second (whiteboard) ink space. That phase is CLOSED and its
    // non-regression evidence was taken over the commit range `016fa8ee..HEAD` AT
    // CLOSE TIME, so this edit — quick task 260828-gb9, the first deliberate change
    // after the release — does NOT falsify STAGE-04. A future reader re-running that
    // range check WILL see a diff here and must read it as a released pin, not a
    // broken requirement. The release is recorded in three places: here,
    // deferred-items.md § D-60.2-12's resolution, and the quick task's SUMMARY.
    //
    // ⚠ AND IT IS A NARROW RELEASE. D-60.2-02 (the stale confirm timer truncating the
    // NEXT stroke) and D-60.2-03 (every stroke committing TWICE since Phase 51) are
    // both real, both confirmed live at HEAD, and both still OPEN in this file. They
    // are deliberately NOT fixed here: this task changes what the pointer LOOKS like
    // and nothing that reaches project state. Do not read the release as permission
    // to have skipped them.
    private void ApplyToolGate()
    {
        // ⚠ ONE ROUTE TO THE PREDICATE, AND THE GATE IS THE ROUTE (review WR-02). This
        // method used to answer "is a drawing tool selected?" TWICE, three lines apart,
        // by two different routes: an inline comparison against the inert tool for the
        // hit-test and background toggle, and InkCursorGate for the cursor.
        //
        // That is the exact shape InkCursorWiringGateTests names as the rot mechanism in
        // its own comment — "a site that re-derived the comparison inline would be
        // correct on the day it was written and is exactly how a shared rule rots" — and
        // the gate enforced the rule on the cursor line while leaving the identical
        // inline derivation untouched immediately above it. DrawTool is explicitly
        // expected to GROW (InkCursorGateTests has a theory row per tool to force that
        // decision), and the first non-drawing tool that is not the inert one — a
        // Hand/Pan tool is the obvious candidate — would have made the two diverge: this
        // layer hit-testable and SWALLOWING presses while showing a plain arrow. One
        // assignment, no test failure. The gate now bans the re-derivation from this
        // method's body outright.
        var drawing = InkCursorGate.WantsInkCursor(_tool);
        InkRoot.IsHitTestVisible = drawing;
        InkRoot.Background = drawing ? _hitTestBrush : null;
        ProtectedCursor = drawing ? InkCursor : null;
    }

    /// <summary>The ink cursor, created ONCE — <c>InputSystemCursor.Create</c>
    /// allocates and the tool changes as often as the user clicks the palette.
    /// <c>Cross</c> byte-matches the Canvas stage's choice for the same reason the
    /// dashed <c>accent</c> trail styling does: the two ink surfaces are ONE product
    /// feature, not two implementations. There is no system pen cursor, and a custom
    /// cursor resource is out of proportion to this fix.</summary>
    private static readonly InputCursor InkCursor =
        InputSystemCursor.Create(InputSystemCursorShape.Cross);

    // ── the recorded pointer-sample stream (plan 51-10, DEBUG ONLY) ────────────

#if DEBUG
    /// <summary>
    /// One recorded pointer sample — plan 51-10's INSTRUMENT, not a behaviour.
    ///
    /// <para>A <c>readonly record struct</c> in a pre-sized array, because the
    /// recorder runs inside the handler <c>PreviewAllocationTests</c> gates at zero
    /// bytes (SHELL-06 / D-15 part 2, T-51-35). No allocation per sample: the struct
    /// is stored by value into a slot that already exists, and <see cref="Phase"/> is
    /// always one of four interned literals.</para>
    ///
    /// <para><b>The three columns exist to DISCRIMINATE, not to describe.</b>
    /// <see cref="ScreenX"/>/<see cref="ScreenY"/> is the OS cursor (H3: did the
    /// pointer move at all?); <see cref="LayerX"/>/<see cref="LayerY"/> is what the
    /// WinUI pointer stack handed this layer (H1: is the current position stale?);
    /// <see cref="CoalescedCount"/> plus the first coalesced entry is the history the
    /// event offered (H2: is that history bogus?).</para>
    /// </summary>
    internal readonly record struct InkSample(
        int Seq, string Phase, double ScreenX, double ScreenY,
        double LayerX, double LayerY, double NormX, double NormY,
        int CoalescedCount, double FirstCoalescedX, double FirstCoalescedY,
        long TimestampUs);

    /// <summary>Ring-BOUNDED at 512 rows: full means DROP, never grow. A wedged or
    /// very long gesture must not be able to turn a debug instrument into an
    /// allocation source.</summary>
    private readonly InkSample[] _samples = new InkSample[512];

    private int _sampleCount;

    private int _samplesDropped;

    private string _samplePhase = "idle";

    private int _sampleCoalescedCount;

    private Windows.Foundation.Point _sampleFirstCoalesced;

    private static readonly long SampleClockStart = Stopwatch.GetTimestamp();

    /// <summary>
    /// The LAST finished gesture's recorded stream, as ONE immutable JSON string
    /// published from the UI thread at <see cref="FinishGesture"/> and read verbatim
    /// by <c>IntrospectionHook</c>'s <c>"ink"</c> branch on its background pipe
    /// thread (T-51-36).
    ///
    /// <para>The shape is <c>MediaBinIntrospection.Describe()</c>'s exactly: the UI
    /// thread publishes plain values, the pipe thread reads plain values, and no XAML
    /// object is reachable from the reader. Asking a live element for its state off
    /// its own thread is <c>RPC_E_WRONG_THREAD</c> — a hard failure, not a torn
    /// read.</para>
    /// </summary>
    internal static string LastInkSamplesJson { get; private set; } =
        "{\"gestures\":0,\"count\":0,\"dropped\":0,\"samples\":[],\"points\":[]}";

    private void ResetInkSamples()
    {
        _sampleCount = 0;
        _samplesDropped = 0;
    }

    /// <summary>Tag the rows the NEXT <see cref="AppendFromLayerPoint"/> calls will
    /// write. Four field stores, no allocation.</summary>
    private void BeginSamplePhase(string phase, int coalescedCount, Windows.Foundation.Point firstCoalesced)
    {
        _samplePhase = phase;
        _sampleCoalescedCount = coalescedCount;
        _sampleFirstCoalesced = firstCoalesced;
    }

    /// <summary>
    /// One row. Reads the OS cursor DIRECTLY rather than through the WinUI pointer
    /// stack, deliberately: that is the only column in the table whose value does not
    /// come from the code path under suspicion, so it is the one that can say whether
    /// the input arrived at all (H3).
    /// </summary>
    private void RecordInkSample(Windows.Foundation.Point layer, NormPoint normalized)
    {
        if (_sampleCount >= _samples.Length)
        {
            _samplesDropped++;
            return;
        }

        double screenX = 0;
        double screenY = 0;
        if (GetCursorPos(out var cursor))
        {
            screenX = cursor.X;
            screenY = cursor.Y;
        }

        var elapsed = Stopwatch.GetTimestamp() - SampleClockStart;
        var micros = (long)(elapsed * (1_000_000.0 / Stopwatch.Frequency));

        _samples[_sampleCount] = new InkSample(
            _sampleCount,
            _samplePhase,
            screenX,
            screenY,
            layer.X,
            layer.Y,
            normalized.X,
            normalized.Y,
            _sampleCoalescedCount,
            _sampleFirstCoalesced.X,
            _sampleFirstCoalesced.Y,
            micros);
        _sampleCount++;
    }

    private static int _inkGestureCount;

    /// <summary>Serialize the recorded stream ONCE per gesture, on the UI thread, at
    /// the moment the gesture finishes. A cold path by construction.</summary>
    private void PublishInkSampleSnapshot(NormPoint[] committed)
    {
        var rows = new JsonArray();
        for (var i = 0; i < _sampleCount; i++)
        {
            var s = _samples[i];
            rows.Add(new JsonObject
            {
                ["seq"] = s.Seq,
                ["phase"] = s.Phase,
                ["screenX"] = s.ScreenX,
                ["screenY"] = s.ScreenY,
                ["layerX"] = s.LayerX,
                ["layerY"] = s.LayerY,
                ["normX"] = s.NormX,
                ["normY"] = s.NormY,
                ["coalescedCount"] = s.CoalescedCount,
                ["firstCoalescedX"] = s.FirstCoalescedX,
                ["firstCoalescedY"] = s.FirstCoalescedY,
                ["timestampUs"] = s.TimestampUs,
            });
        }

        var points = new JsonArray();
        double minY = 1.0;
        double maxY = 0.0;
        double minX = 1.0;
        double maxX = 0.0;
        foreach (var p in committed)
        {
            points.Add(new JsonObject { ["x"] = p.X, ["y"] = p.Y });
            if (p.Y < minY) { minY = p.Y; }
            if (p.Y > maxY) { maxY = p.Y; }
            if (p.X < minX) { minX = p.X; }
            if (p.X > maxX) { maxX = p.X; }
        }

        var spreadY = committed.Length == 0 ? 0.0 : maxY - minY;
        var spreadX = committed.Length == 0 ? 0.0 : maxX - minX;

        LastInkSamplesJson = new JsonObject
        {
            ["gestures"] = ++_inkGestureCount,
            ["count"] = _sampleCount,
            ["dropped"] = _samplesDropped,
            ["cursorSource"] = "user32!GetCursorPos",
            ["rectX"] = _rect.X,
            ["rectY"] = _rect.Y,
            ["rectW"] = _rect.Width,
            ["rectH"] = _rect.Height,
            ["scale"] = _scale,
            ["committedCount"] = committed.Length,
            ["ySpread"] = spreadY,
            ["xSpread"] = spreadX,
            ["samples"] = rows,
            ["points"] = points,
        }.ToJsonString();
    }

    /// <summary>The OS cursor position in PHYSICAL screen pixels (this process is
    /// per-monitor DPI aware). Blittable out-parameter, no allocation.</summary>
    [System.Runtime.InteropServices.StructLayout(
        System.Runtime.InteropServices.LayoutKind.Sequential)]
    private struct CursorPoint
    {
        public int X;
        public int Y;
    }

    [System.Runtime.InteropServices.LibraryImport("user32.dll", SetLastError = true)]
    [return: System.Runtime.InteropServices.MarshalAs(
        System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static partial bool GetCursorPos(out CursorPoint lpPoint);
#endif

    /// <summary>
    /// Record one commit's measurement on the element it describes, and log it.
    ///
    /// <para>TWO read paths on purpose, because plan 51-05's ink-hand-off
    /// observation depends on reading this number back out of the running app
    /// rather than estimating it from screenshots: <c>HelpText</c> on
    /// <c>Canvas.PreviewInk</c> (UIA), and the diagnostic tail that
    /// <c>Shell.StatusText</c> renders as its "last diagnostic" line. One write per
    /// COMMIT — a cold path by construction, nothing like the per-tick readout that
    /// measurably reddened the GC gate in plan 51-04.</para>
    /// </summary>
    private void Publish(string note)
    {
        var full = _geometryNote.Length > 0 ? note + " · " + _geometryNote : note;
        CommitNote = full;
        AutomationProperties.SetHelpText(InkRoot, full);
        App.LogDiagnostic("canvas ink: " + full);
    }
}
