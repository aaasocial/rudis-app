using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Rudis.Shell.Interop;
using Rudis.Shell.Mirror;
using Windows.ApplicationModel.DataTransfer;
using Windows.System;

namespace Rudis.Shell.Regions;

/// <summary>
/// The <c>Timeline</c> region (design_handoff_rudis_editor/README.md:35,129 — the name
/// is the handoff's, verbatim, per CLAUDE.md rule 7).
///
/// <para><b>This file is the WinUI half of SHELL-05, and it is deliberately thin.</b>
/// Everything that decides WHAT the Timeline looks like lives one directory down, in
/// <c>Regions/Timeline/</c>, which is WinUI-free by rule (D-13) and unit-tested without
/// a window: the coordinate system, the lane model, the viewport cull, the hit-test
/// classification, and the frame builder. What is left here is the four things that
/// genuinely need a UI thread:</para>
/// <list type="number">
/// <item><b><see cref="OnSurfaceLoaded"/> — attach.</b> Hands
///   <c>rudis_timeline.dll</c> a COM pointer to the panel and uploads the palette.
///   Runs on the panel's own UI thread, which is a CORRECTNESS requirement, not a
///   style: the COM call reached inside the first surface configure refuses every
///   other thread.</item>
/// <item><b><see cref="PublishSize"/> — forward the geometry.</b> C# forwards, RUST
///   owns the resize. Two owners of one swapchain is the dual-DPI bug class the
///   milestone research named, and Phase 51 applies the same single-owner rule to
///   Preview.</item>
/// <item><b><see cref="ApplyMirrorState"/> — the mirrored-state sink.</b> The region
///   owns NO truth (CLAUDE.md rule 4): the backend owns the project, the mirror
///   projects it, and this rebuilds a drawable model from that projection. Every
///   mutation will go back out as a <c>rudis_dispatch_command</c> call (D-12, plan
///   52-07), never as a write to anything here.</item>
/// <item><b><see cref="OnRenderingTick"/> — one frame.</b> Called from MainWindow's
///   EXISTING composition-tick consumer (plan 50-04's single slot). It does NOT open a
///   second per-frame subscription of its own, and it makes NO ABI call for the
///   playhead — it is handed the value <c>PlayheadTicker</c> already read this tick
///   (T-52-30). The framework event's name is deliberately not written anywhere in
///   this file: the acceptance check for "there is only one such subscriber in the
///   shell" is a literal grep over this file pinned at zero, and a gate its own
///   rationale can trip is a gate that gets the rationale deleted.</item>
/// </list>
///
/// <para><b>Teardown is an ordered sequence, not a Dispose.</b>
/// <see cref="DetachSurface"/> must run on this UI thread and BEFORE the window is
/// destroyed — from <c>AppWindow.Closing</c>, never <c>Window.Closed</c>. Plan 52-01
/// found the two wrong orderings by crashing, on real hardware, and the export now
/// performs the whole sequence internally; what this side owes is the THREAD and the
/// TIMING.</para>
/// </summary>
public sealed partial class Timeline : UserControl
{
    /// <summary>
    /// Below this REGION width the zoom group collapses to a single overflow chevron
    /// (README:176 — "Timeline tool labels stay, zoom control may collapse").
    ///
    /// <para>Applied by a MEASURED <see cref="FrameworkElement.SizeChanged"/> handler
    /// and never by an <c>AdaptiveTrigger</c>: plan 50-05 implemented the textbook
    /// VisualState arrangement, in two placements, at three widths, and it NEVER
    /// FIRED. A plain conditional on the region's own measured width cannot be
    /// defeated by whatever the trigger evaluation was doing.</para>
    /// </summary>
    private const double ZoomCollapseWidthPx = 520;

    private readonly TimelineModel _model = new();
    private readonly TimelineViewport _viewport = new();

    /// <summary>SHELL-09's client-side peak cache (plan 52-08). The render loop only
    /// ever READS it; the only thing that can fill it is
    /// <see cref="OnColdPollAsync"/>, on the cold cycle.</summary>
    private readonly PeakCache _peaks = new(App.LogDiagnostic);

    /// <summary>53.2 D-12's client-side strip cache (plan 53.2-06), on exactly the
    /// same terms as <see cref="_peaks"/>: the render loop only ever READS it and the
    /// only thing that can fill it is <see cref="OnColdPollAsync"/>. Its production
    /// poster decoder is the constructor's own default, so a placeholder is never
    /// lost to a forgotten injection.</summary>
    private readonly FilmstripCache _filmstrips = new(log: App.LogDiagnostic);

    private readonly TimelineFrameBuilder _builder;

    /// <summary>The pointer/keyboard state machine (plan 52-07). It is WinUI-free and
    /// transport-free by rule: this file owns the events and the sending, and it owns
    /// NOTHING of the gesture logic.</summary>
    private readonly TimelineInteraction _interaction;

    /// <summary>Reused so draining the snap guides onto the frame allocates nothing on
    /// the composition tick.</summary>
    private readonly float[] _snapGuides = new float[TimelineInteraction.MaxSnapGuides];

    /// <summary>Reused so the per-tick timecode format allocates nothing; only the
    /// final <c>TextBlock.Text</c> assignment does, and only on a CHANGE. That is the
    /// same honest limit plan 50-06 recorded for Transport: WinUI has no
    /// allocation-free text setter, so the bound is "one small string per rendered
    /// change", never "per tick".</summary>
    private readonly char[] _timecodeChars = new char[TimelineTimecode.FrameTimecodeMaxChars];

    private TimelineHandle? _handle;
    private XamlRoot? _observedXamlRoot;
    private bool _attached;
    private bool _detached;

    /// <summary>The single re-attach the error contract allows for a lost surface
    /// (T-52-19). One, not a loop: a permanently failing surface must degrade to a
    /// visible, diagnosable message rather than a spin.</summary>
    private bool _reattachSpent;

    /// <summary>Set between <see cref="SuspendSurface"/> and <see cref="ResumeSurface"/>
    /// (Phase 71, TRUST-01): the surface handle is released for a preview device-lost
    /// recovery and the region is waiting to re-attach. Distinct from
    /// <see cref="_detached"/>, which is permanent.</summary>
    private bool _suspended;

    private int _lastTimecodeLength = -1;
    private double _fps = TimelineTimecode.FallbackFps;

    // ── the last PUBLISHED geometry, in PHYSICAL px. A WinUI layout pass fires
    //    SizeChanged far more often than the size actually changes, so the publisher
    //    is idempotent against these. ──
    private uint _lastW;
    private uint _lastH;
    private float _lastScale;

    private bool _zoomCollapsed;

    // ── the tool tiles' enabled state, cached. Recomputed on the composition tick
    //    (Split's availability depends on the PLAYHEAD, which moves), but WRITTEN only
    //    on a change: an IsEnabled assignment raises a UIA property-change event, and
    //    50-06 recorded what happens when one of those lands per tick. ──
    private bool _splitEnabled;
    private bool _selectionEnabled;

    /// <summary>The furthest clip end in the mirrored project, for `Zoom to fit`.</summary>
    private long _projectDurationUs;

    // ── scrubbing: ONE seek in flight, latest value wins (T-50-26 / the 50-06 rule).
    //    A scrub drag produces a continuous stream of positions; queueing all of them
    //    would back up the one interop worker and make the playhead trail the pointer
    //    by the whole queue depth. ──
    private bool _seekInFlight;
    private long? _pendingSeekUs;

    /// <summary>ONE waveform-peak poll in flight at a time (plan 52-08). Without it
    /// a slow answer would let cold cycles stack, and the ≤2-requests-per-cycle cap
    /// would stop meaning ≤2 requests per 100ms.</summary>
    private bool _peakPollInFlight;

    // ── the drag/trim duration label's own change gates (see RenderDragReadout) ──
    private bool _readoutVisible;
    private int _lastReadoutLength = -1;
    private double _lastReadoutLeft = double.NaN;
    private double _lastReadoutTop = double.NaN;

#if DEBUG
    /// <summary>See <see cref="StartStatsPublisher"/>. Debug-only, 1 Hz, UAT channel.</summary>
    private Microsoft.UI.Dispatching.DispatcherQueueTimer? _statsTimer;

    // ── plan 52-09 / criterion 2: the input→redraw measurement's own state ──
    //
    // The whole instrument is three longs and one bool compared once per tick. It is
    // Debug-only for the same reason the hook that reads it is: it exists to be
    // MEASURED, not to ship.
    //
    // ⚠ IT MEASURES EXACTLY ONE THING: t_feedback — an accepted input to the next frame
    // this region actually DREW. For a drag or a trim, that frame carries the moved
    // ghost, which is the feedback the user sees and the thing D-11 traded the backend
    // round trip for.
    //
    // The OTHER question — t_commit, input to the first drawn frame reflecting the
    // backend's committed answer — is deliberately NOT instrumented here, and the
    // reason is a correction rather than a preference. A first draft armed it the same
    // way, on "the first dirty frame at a different model revision", and it raced: the
    // arming happens on a render tick, the model revision moves on the mirror's own
    // 100ms cadence, and a rebuild landing between an input and the next tick makes the
    // armed revision ALREADY the post-commit one, at which point the measurement waits
    // forever for a change that has happened. It cost two red runs.
    //
    // t_commit needs no in-app state at all: `last_redraw_unix_ms`, `last_input_unix_ms`
    // and `model_revision` are all published here on ONE clock, so the harness
    // subtracts two numbers it can already see and there is no arming to race. Fewer
    // moving parts in the instrument is worth more than symmetry with t_feedback.
    private long _pendingInputUnixMs;
    private bool _feedbackArmed;
    private long _lastRedrawUnixMs;
    private long _lastFeedbackLatencyMs = -1;
    private long _lastFeedbackInputUnixMs;

    // ── the cost of hydrating the Timeline from a mirrored project, in µs ──
    //    Criterion 3's "project open" half: at 1,000 clips this is what the C# shell
    //    pays to turn the mirror's raw JSON into a drawable model.
    private long _lastMirrorProjectionUs;
    private long _lastModelRebuildUs;
    private long _lastApplyMirrorStateUs;
    private int _mirrorApplyCount;
#endif

    public Timeline()
    {
        InitializeComponent();
        ApplyHandoffMetrics();

        _builder = new TimelineFrameBuilder(_peaks, _filmstrips);
        _interaction = new TimelineInteraction(_model, _viewport);

        // Wired here rather than in XAML so every subscription sits beside the remark
        // that says why it exists.
        TimelineSurface.Loaded += OnSurfaceLoaded;
        TimelineSurface.Unloaded += OnSurfaceUnloaded;
        TimelineSurface.SizeChanged += OnSurfaceSizeChanged;
        TimelineSurface.CompositionScaleChanged += OnSurfaceCompositionScaleChanged;
        SizeChanged += OnRegionSizeChanged;

        // ── the pointer surface (plan 52-07) ──
        // A `SwapChainPanel` is an ordinary `Grid` subclass, so these are ordinary
        // routed pointer events; the surface itself is the only hit-test target there
        // is, and D-10's point -> data resolution happens inside the state machine.
        TimelineSurface.PointerPressed += OnSurfacePointerPressed;
        TimelineSurface.PointerMoved += OnSurfacePointerMoved;
        TimelineSurface.PointerReleased += OnSurfacePointerReleased;
        TimelineSurface.PointerCaptureLost += OnSurfacePointerCaptureLost;
        TimelineSurface.PointerCanceled += OnSurfacePointerCaptureLost;
        TimelineSurface.PointerWheelChanged += OnSurfacePointerWheelChanged;

        // ── the drop target (plan 53.1-02) ──
        // TimelineSurface carries its OWN AllowDrop in XAML: an ancestor's does not
        // extend drop-target-hood downward, it only lets that ancestor receive the
        // bubbled event. Without it these two never fire at all.
        TimelineSurface.DragOver += OnSurfaceDragOver;
        TimelineSurface.Drop += OnSurfaceDrop;

        // ── the drop-preview ghost (phase 53.1, owner request) ──
        // Enter caches the dragged media id (the only async read of the session, done
        // ONCE, not per DragOver); Leave hides the ghost so it never outlives the
        // pointer that summoned it.
        TimelineSurface.DragEnter += OnSurfaceDragEnter;
        TimelineSurface.DragLeave += OnSurfaceDragLeave;

        // The region's accelerators are SCOPED to the region (see Timeline.xaml's own
        // note): they are bare keys, and an unscoped `S` would fire while the user was
        // typing anywhere else in the window. Set here rather than in XAML because a
        // markup binding to the very element that hosts the collection is an ordering
        // question nobody should have to think about twice.
        foreach (var accelerator in TimelineRoot.KeyboardAccelerators)
        {
            accelerator.ScopeOwner = TimelineRoot;
        }

        // ── the selected-lane indicator (quick 260731-k9b) ──
        // Built in code rather than in markup because the element is meaningless
        // without the code that positions it; both live in Timeline.SelectedLane.cs,
        // which also records why this cannot be a renderer quad (the gutter is drawn
        // LAST, over everything, and no lane flag crosses the ABI).
        InstallSelectedLaneIndicator();
    }

    /// <summary>The renderer's own counters, or <see langword="null"/> when nothing is
    /// attached. <c>FramesRendered</c> flat while <c>SkippedCleanFrames</c> climbs IS
    /// the evidence that an idle Timeline does no GPU work; plan 52-09's introspection
    /// hook reads the same numbers.</summary>
    internal RudisTimelineStats? Stats
    {
        get
        {
            if (_handle is null || _handle.IsInvalid)
            {
                return null;
            }

            return TimelineNative.rudis_timeline_stats(_handle, out var stats) == TimelineStatus.Ok
                ? stats
                : null;
        }
    }

    /// <summary>A one-line, UIA-readable description of what the region actually did
    /// at attach — published ONCE onto the surface's own HelpText, the shape plan
    /// 51-04 settled on after measuring that folding such a note into the window's
    /// per-tick status readout reddened Phase 50's GC gate.</summary>
    public string AttachNote { get; private set; } = "(not attached yet)";

    /// <summary>True once the renderer holds this panel.</summary>
    public bool IsAttached => _attached;

    /// <summary>
    /// The id of the clip the user currently has selected, or <see langword="null"/>.
    ///
    /// <para>Plan 54-05's ONE additive member: the whole of what the <c>Chat</c> region
    /// reads from this one, wired window-side so the two regions never reference each
    /// other. It is a read-only projection of the region's own view model — this
    /// property grants no way to CHANGE the selection, which is deliberate: selection
    /// changes belong to this region's gestures and to the backend's commands.</para>
    /// </summary>
    internal string? SelectedClipId => _model.SelectedClipId;

    // ========================================================================
    // The handoff's numbers, applied from ONE place
    // ========================================================================

    /// <summary>
    /// The two sizing constants this region's XAML would otherwise have to re-type:
    /// the `Timeline › Toolbar` band's 36px and the region's ~140px minimum
    /// (README:239 and :173).
    ///
    /// <para>Applied from <see cref="TimelineMetrics"/> in code rather than written as
    /// XAML literals, because the RENDERER is driven by those same two numbers through
    /// the frame contract. A literal here and a constant there would be two sources of
    /// truth for one measurement, and the symptom of them drifting is a ruler drawn
    /// under the toolbar — visible, but not obviously a units bug.</para>
    /// </summary>
    private void ApplyHandoffMetrics()
    {
        TimelineToolbar.Height = TimelineMetrics.TimelineHeaderHeight;
        MinHeight = TimelineMetrics.MinTimelineHeight;
    }

    // ========================================================================
    // Attach
    // ========================================================================

    private void OnSurfaceLoaded(object sender, RoutedEventArgs e)
    {
        // `Loaded` fires again if the panel is ever re-parented. Re-attaching over a
        // live surface is not a supported transition, so guard rather than rely on a
        // refusal.
        if (_attached || _detached || _suspended)
        {
            return;
        }

        AttachSurface();
    }

    private void AttachSurface()
    {
        // The panel's own composition scale, queried INSIDE the handler rather than
        // cached: it is the scale the SWAPCHAIN is measured in, and the change event
        // that carries it fires asynchronously with respect to the change itself.
        // (XamlRoot.RasterizationScale is the WINDOW's; the two agree today and would
        // diverge under a render transform, and it is the panel that is being sized.)
        var scale = TimelineSurface.CompositionScaleX;
        _viewport.RasterizationScale = scale;
        _viewport.SetSurfaceSize(TimelineSurface.ActualWidth, TimelineSurface.ActualHeight);

        var w = (uint)Math.Max(1, Math.Round(_viewport.LogicalToPhysical(TimelineSurface.ActualWidth)));
        var h = (uint)Math.Max(1, Math.Round(_viewport.LogicalToPhysical(TimelineSurface.ActualHeight)));

        // The COM pointer, obtained with the idiom the Phase-44 spike PROVED on this
        // machine.
        //
        // DO NOT "simplify" this to a WinRT-projection cast to the native panel
        // interface: that throws InvalidCastException for exactly this interop
        // (44-RESEARCH Pattern 4, re-confirmed by the spike's own transcript). Rust
        // does the QueryInterface itself, from an IInspectable*, which is why a wrong
        // pointer is a named failure instead of undefined behaviour.
        nint panelPtr;
        try
        {
            panelPtr = WinRT.MarshalInspectable<object>.FromManaged(TimelineSurface);
        }
        catch (Exception ex)
        {
            ShowStatus($"Could not obtain a COM pointer for the Timeline surface: {ex.GetType().Name}: {ex.Message}");
            return;
        }

        if (panelPtr == nint.Zero)
        {
            ShowStatus("The Timeline surface handed back a null COM pointer; nothing was attached.");
            return;
        }

        TimelineHandle handle;
        try
        {
            handle = TimelineNative.rudis_timeline_attach(panelPtr, w, h, (float)scale);
        }
        catch (Exception ex)
        {
            Marshal.Release(panelPtr);
            ShowStatus($"rudis_timeline_attach threw: {ex.GetType().Name}: {ex.Message}");
            return;
        }

        // T-52-27: `FromManaged` handed back an AddRef'd pointer, and Rust took its OWN
        // reference through its QueryInterface. This one is ours to drop, on every path
        // including the throwing one above.
        Marshal.Release(panelPtr);

        if (handle.IsInvalid)
        {
            handle.Dispose();
            ShowStatus(
                "Couldn't start the Timeline surface: rudis_timeline_attach returned null. " +
                "The failing stage and its adapter/HRESULT are on the engine's stderr — the " +
                "realistic causes are no DX12 adapter and a panel pointer that is not this panel.");
            return;
        }

        _handle = handle;
        _attached = true;

        // Attach already published this geometry; seed the idempotence cache so the
        // first layout-driven SizeChanged does not re-publish identical numbers.
        _lastW = w;
        _lastH = h;
        _lastScale = (float)scale;

        if (!UploadPalette())
        {
            return;
        }

        // A monitor move can change the scale WITHOUT firing SizeChanged, so this is a
        // genuine third notification source rather than a duplicate of the other two.
        _observedXamlRoot = TimelineSurface.XamlRoot;
        if (_observedXamlRoot is not null)
        {
            _observedXamlRoot.Changed += OnXamlRootChanged;
        }

        HideStatus();
        _builder.MarkDirty();

        AttachNote = $"attached at {w}x{h}px scale {scale:0.###}";
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetHelpText(TimelineSurface, AttachNote);
        App.LogDiagnostic($"timeline: {AttachNote}");

#if DEBUG
        StartStatsPublisher();
#endif
    }

#if DEBUG
    /// <summary>
    /// Republish <c>rudis_timeline_stats</c> onto the surface's own
    /// <c>AutomationProperties.HelpText</c> once a second, so a UIA probe can READ the
    /// renderer's counters out of a running shell.
    ///
    /// <para>This is what turns "an idle Timeline costs nothing" from a promise into a
    /// number: sample twice, ten seconds apart, and <c>frames</c> must be flat while
    /// <c>clean</c> climbs by roughly the display rate x 10. It is the same published
    /// channel plan 51-04 chose for Preview's attach measurement, for the same reason —
    /// folding a per-tick value into the window's status readout measurably reddened
    /// Phase 50's GC gate.</para>
    ///
    /// <para><b>1 Hz, and Debug-only.</b> The frequency is a UAT convenience, not a
    /// product feature, and a Release build carries neither the timer nor the call. Plan
    /// 52-09 replaces this with the proper introspection-hook query (D-22).</para>
    /// </summary>
    private void StartStatsPublisher()
    {
        if (_statsTimer is not null)
        {
            return;
        }

        _statsTimer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
        _statsTimer.Interval = TimeSpan.FromSeconds(1);
        _statsTimer.IsRepeating = true;
        _statsTimer.Tick += (_, _) =>
        {
            if (Stats is not { } s)
            {
                return;
            }

            // pps/scrollX/sel are plan 52-07's additions: a coordinate-driven UAT run
            // (D-23) has to turn a TIMELINE POSITION into a screen point, and doing
            // that from assumed defaults rather than from the viewport the shell is
            // actually using is how a gesture lands on the wrong clip and the test
            // reports a bug that is its own.
            // Plan 52-08's gate numbers, refreshed on the SAME 1 Hz tick and read by
            // the introspection hook's `waveform` query. Published from HERE rather
            // than read live from the pipe thread because every field below belongs
            // to the UI thread — the peak cache is explicitly single-threaded, and a
            // background reader walking its dictionary mid-mutation is the exact race
            // its own doc comment refuses.
            LastWaveformDiagnostics = new WaveformDiagnostics(
                Interlocked.Read(ref RudisNative.AbiCallsInsidePaintScope),
                _peaks.RequestsIssued,
                _peaks.SlotCount,
                _peaks.ResolvedCount,
                _peaks.WantedCount,
                _peaks.GivenUpCount,
                s.WaveformQuadsDrawn,
                s.WaveformTruncatedClips,
                s.FramesRendered,
                s.SkippedCleanFrames,
                s.QuadsDrawn,
                _builder.LastClipCount);

            Microsoft.UI.Xaml.Automation.AutomationProperties.SetHelpText(
                TimelineSurface,
                $"{AttachNote} · frames={s.FramesRendered} clean={s.SkippedCleanFrames} " +
                $"quads={s.QuadsDrawn} glyphs={s.GlyphsDrawn} " +
                $"wf_quads={s.WaveformQuadsDrawn} wf_trunc={s.WaveformTruncatedClips} " +
                $"peaks={_peaks.ResolvedCount}/{_peaks.SlotCount} " +
                $"peak_reqs={_peaks.RequestsIssued} " +
                $"abi_in_paint={Interlocked.Read(ref RudisNative.AbiCallsInsidePaintScope)} " +
                $"present_errors={s.PresentErrors} device_lost={s.DeviceLost} " +
                $"clips={_builder.LastClipCount} " +
                $"pps={_viewport.PxPerSecond:0.###} scrollX={_viewport.ScrollXPx:0.###} " +
                $"state={_interaction.State} sel={_model.SelectedClipId ?? "-"}");
        };
        _statsTimer.Start();
    }

    /// <summary>
    /// Plan 52-08's gate numbers, as of the last 1 Hz publish. Read by
    /// <c>IntrospectionHook</c>'s <c>waveform</c> query.
    ///
    /// <para>A snapshot rather than a live read, deliberately: the hook's server runs
    /// on its own background thread, and every source below belongs to the UI thread.
    /// Copying a struct once a second is the cheap way to make the query safe without
    /// giving the cache a lock it otherwise does not need.</para>
    /// </summary>
    internal static WaveformDiagnostics LastWaveformDiagnostics;

    /// <summary>The numbers this plan's artifact records. <c>AbiInPaint</c> is the
    /// one that must read ZERO.</summary>
    internal readonly record struct WaveformDiagnostics(
        long AbiInPaint,
        long PeakRequestsIssued,
        int PeakSlots,
        int PeakResolved,
        int PeakWanted,
        int PeakGivenUp,
        uint WaveformQuadsDrawn,
        uint WaveformTruncatedClips,
        ulong FramesRendered,
        ulong SkippedCleanFrames,
        uint QuadsDrawn,
        int ClipsInFrame);

    // ========================================================================
    // Plan 52-09 / D-22 — the Timeline's read-only introspection snapshot
    // ========================================================================

    /// <summary>
    /// Everything <c>IntrospectionHook</c>'s <c>timeline</c> query answers about the
    /// SURFACE, refreshed at the end of every render tick.
    ///
    /// <para><b>A mutable class, deliberately, and there are exactly two of them.</b>
    /// This is written on the UI thread inside the paint scope, so it must allocate
    /// nothing; and it is read from the hook's background pipe thread, so a partially
    /// written object must never be observable. Two preallocated instances with a
    /// published index (<see cref="PublishIntrospectionSnapshot"/>) give both: the
    /// writer fills the slot nobody is reading and then swaps the index with a
    /// <see cref="Volatile.Write"/>, which is one store and zero bytes.</para>
    ///
    /// <para><b>The honest limit:</b> a reader preempted for longer than two full
    /// render ticks could see the slot it is reading recycled underneath it. The
    /// consequence is a snapshot whose fields come from two adjacent frames — never a
    /// torn primitive, never a null dereference, and never a wrong ANSWER to the
    /// questions this channel exists for (all of which are compared with slack). A
    /// lock would remove that window and would also let a stalled pipe client stall a
    /// frame, which is a strictly worse trade for a debug-only assertion channel.</para>
    /// </summary>
    internal sealed class IntrospectionSnapshot
    {
        /// <summary>The surface's display scale. Published so a coordinate-driven UAT
        /// reads D-14's conversion factor OUT OF THE RUNNING SHELL instead of assuming
        /// 1.0 — this machine runs at 125%, and that assumption is exactly how 52-07's
        /// first trim landed 61px past the clip it was aiming at.</summary>
        internal double RasterizationScale;

        /// <summary>Lane-area width and whole-surface height, LOGICAL px.</summary>
        internal double ViewportWidthPx;
        internal double ViewportHeightPx;

        internal double PxPerSecond;
        internal double ScrollXPx;
        internal double ScrollYPx;
        internal long ViewportStartUs;
        internal long ViewportEndUs;
        internal int VisibleLaneFirst;
        internal int VisibleLaneLast;
        internal int LaneCount;
        internal int TotalClipCount;
        internal int CulledClipCount;
        internal int DrawnClipCount;
        internal int ScannedClipCount;
        internal string? SelectedClipId;
        internal long PlayheadUs;
        internal long LastInputUnixMs;
        internal long LastRedrawUnixMs;
        internal long LastFeedbackLatencyMs;
        internal long LastFeedbackInputUnixMs;
        internal ulong LastRenderUs;
        internal ulong FramesRendered;
        internal ulong SkippedCleanFrames;
        internal long AbiCallsInsidePaintScope;
        internal int PeaksCachedMediaIds;
        internal uint WaveformQuadsDrawn;
        internal int ModelRevision;
        internal InteractionState InteractionState;
        internal long ApplyMirrorStateUs;
        internal long MirrorProjectionUs;
        internal long ModelRebuildUs;
        internal int MirrorApplyCount;
        internal long SnapshotUnixMs;
    }

    private static readonly IntrospectionSnapshot[] IntrospectionSlots = [new(), new()];

    /// <summary>Index of the slot a reader may take, or -1 before the first publish.
    /// Written ONLY by the UI thread, and only with <see cref="Volatile.Write"/>.</summary>
    private static int _publishedIntrospectionSlot = -1;

    /// <summary>The slot the UI thread is free to overwrite. UI-thread-only state.</summary>
    private static int _writableIntrospectionSlot;

    /// <summary>The hook's read side. <see langword="null"/> until the Timeline has
    /// rendered at least once — which is itself the readiness signal a UAT run should
    /// wait on rather than assuming a launched window has composited.</summary>
    internal static IntrospectionSnapshot? ReadIntrospectionSnapshot()
    {
        var slot = Volatile.Read(ref _publishedIntrospectionSlot);
        return slot < 0 ? null : IntrospectionSlots[slot];
    }

    /// <summary>
    /// Fill the writable slot from state this tick already computed, then publish it.
    ///
    /// <para>Allocation-free by construction: every field is a primitive, an enum or a
    /// reference this region already holds (<see cref="TimelineModel.SelectedClipId"/>
    /// is an interned model string, not a formatted one). The only work that is not a
    /// field copy is the visible-lane scan, which walks at most
    /// <c>Lanes.Count</c> entries — two on a fresh project, and bounded by the
    /// project's track count in every case.</para>
    /// </summary>
    private void PublishIntrospectionSnapshot(long playheadUs)
    {
        var slot = IntrospectionSlots[_writableIntrospectionSlot];
        var stats = Stats;

        slot.RasterizationScale = _viewport.RasterizationScale;
        slot.ViewportWidthPx = _viewport.ViewportWidthPx;
        slot.ViewportHeightPx = _viewport.ViewportHeightPx;
        slot.PxPerSecond = _viewport.PxPerSecond;
        slot.ScrollXPx = _viewport.ScrollXPx;
        slot.ScrollYPx = _viewport.ScrollYPx;
        slot.ViewportStartUs = _viewport.StartUs;
        slot.ViewportEndUs = _viewport.EndUs;

        var first = -1;
        var last = -1;
        var lanes = _viewport.Lanes;
        for (var i = 0; i < lanes.Count; i++)
        {
            if (!_viewport.IsLaneVisible(i))
            {
                continue;
            }

            if (first < 0)
            {
                first = i;
            }

            last = i;
        }

        slot.VisibleLaneFirst = first;
        slot.VisibleLaneLast = last;
        slot.LaneCount = lanes.Count;

        slot.TotalClipCount = _model.Clips.Count;
        slot.CulledClipCount = _model.LastCulledCount;
        slot.DrawnClipCount = _model.LastDrawnCount;
        slot.ScannedClipCount = _model.LastScannedCount;
        slot.SelectedClipId = _model.SelectedClipId;
        slot.PlayheadUs = playheadUs;

        slot.LastInputUnixMs = _interaction.LastInputUnixMs;
        slot.LastRedrawUnixMs = _lastRedrawUnixMs;
        slot.LastFeedbackLatencyMs = _lastFeedbackLatencyMs;
        slot.LastFeedbackInputUnixMs = _lastFeedbackInputUnixMs;

        slot.LastRenderUs = stats?.LastRenderUs ?? 0;
        slot.FramesRendered = stats?.FramesRendered ?? 0;
        slot.SkippedCleanFrames = stats?.SkippedCleanFrames ?? 0;
        slot.WaveformQuadsDrawn = stats?.WaveformQuadsDrawn ?? 0;

        slot.AbiCallsInsidePaintScope = Interlocked.Read(ref RudisNative.AbiCallsInsidePaintScope);
        slot.PeaksCachedMediaIds = _peaks.ResolvedCount;
        slot.ModelRevision = _model.Revision;
        slot.InteractionState = _interaction.State;

        slot.ApplyMirrorStateUs = _lastApplyMirrorStateUs;
        slot.MirrorProjectionUs = _lastMirrorProjectionUs;
        slot.ModelRebuildUs = _lastModelRebuildUs;
        slot.MirrorApplyCount = _mirrorApplyCount;
        slot.SnapshotUnixMs = TimelineInteraction.NowUnixMs();

        var published = _writableIntrospectionSlot;
        _writableIntrospectionSlot = 1 - published;
        Volatile.Write(ref _publishedIntrospectionSlot, published);
    }
#endif

    /// <summary>
    /// Resolve every token the renderer draws with and upload it once.
    ///
    /// <para><b>A missing key is FATAL and says which key</b> (T-52-29). The renderer
    /// has no colour of its own and draws NOTHING until this has run, so a resolver
    /// that quietly substituted a default would turn a typo in a token name into a
    /// Timeline rendered in a colour nobody chose — or, if the whole upload were
    /// skipped, into a panel that looks dead with no error anywhere. A startup
    /// exception naming the key is a much cheaper failure.</para>
    /// </summary>
    private bool UploadPalette()
    {
        RudisTimelinePalette palette;
        try
        {
            palette = TimelinePalette.BuildPalette(ResolveTokenColour);
        }
        catch (Exception ex)
        {
            ShowStatus(
                $"The Timeline could not resolve its design tokens: {ex.Message}. " +
                "Every colour it draws comes from Theme/Tokens.xaml BY NAME (CLAUDE.md rule 7); " +
                "the renderer has none of its own and will draw nothing without them.");
            return false;
        }

        var status = TimelineNative.rudis_timeline_set_palette(_handle!, palette);
        if (status != TimelineStatus.Ok)
        {
            SurfaceRendererFault(status, "uploading the palette");
            return false;
        }

        _builder.SetPalette(palette);

        // OWNER OVERRIDE (phase 53.1 UAT): video clips draw one deep grey, not the
        // cycled posters. Resolved HERE, beside the palette, from the same token
        // dictionary — but handed to the builder separately because the FFI palette
        // struct's ABI is frozen and this colour never crosses it: fills travel
        // per-clip. The ink is the owner's second cut — near-black over the grey
        // ("clearer / dark black"); the first cut's `text-secondary` read washed-out
        // to the owner on the live app. Both halves are tokens BY NAME (rule 7).
        _builder.SetVideoClipStyle(
            ResolveTokenColour(TimelinePalette.ClipVideoFill),
            ResolveTokenColour(TimelinePalette.ClipVideoLabelInk));

        // 53.2 D-08's filmstrip body backdrop, delivered the same way and for the
        // same reason: the palette struct is a frozen 20-uint ABI and this colour
        // travels per clip. Resolved through the SAME resolver, so a typo in the
        // token name is the same fatal-with-its-name failure the palette's own keys
        // get rather than a silently wrong colour behind the frames.
        _builder.SetFilmstripBackdrop(ResolveTokenColour(TimelinePalette.ClipFilmstripBackdrop));

        return true;
    }

    /// <summary>
    /// One named token to one <c>0xAARRGGBB</c>. The dictionary's colour entries are
    /// keyed <c>{token}-color</c>, which is Theme/Tokens.xaml's own convention
    /// (one Color, one SolidColorBrush twin, established by plan 50-03).
    /// </summary>
    private static uint ResolveTokenColour(string token)
    {
        var key = token + "-color";
        var resources = Application.Current?.Resources;
        if (resources is null || !resources.TryGetValue(key, out var value) || value is not Windows.UI.Color colour)
        {
            throw new KeyNotFoundException(
                $"Theme/Tokens.xaml has no Color resource `{key}` for design token `{token}`");
        }

        return ((uint)colour.A << 24) | ((uint)colour.R << 16) | ((uint)colour.G << 8) | colour.B;
    }

    // ========================================================================
    // Geometry — C# forwards, Rust owns
    // ========================================================================

    private void OnSurfaceSizeChanged(object sender, SizeChangedEventArgs e) => PublishSize();

    private void OnSurfaceCompositionScaleChanged(SwapChainPanel sender, object args) => PublishSize();

    private void OnXamlRootChanged(XamlRoot sender, XamlRootChangedEventArgs args) => PublishSize();

    /// <summary>
    /// The ONE publisher, fed by all three notifications.
    ///
    /// <para>The logical-to-physical conversion happens at exactly one seam —
    /// <see cref="TimelineViewport.LogicalToPhysical"/>, D-14's boundary — and the
    /// viewport keeps LOGICAL px for everything else. This trap has cost the project
    /// three times already (50-05's screen-scrape, 50-06's 44-vs-55px measurement,
    /// 52-01's clipped screenshot); a region that converts in one place cannot make it
    /// a fourth.</para>
    /// </summary>
    private void PublishSize()
    {
        if (!_attached || _handle is null)
        {
            return;
        }

        var scale = TimelineSurface.CompositionScaleX;
        _viewport.RasterizationScale = scale;
        _viewport.SetSurfaceSize(TimelineSurface.ActualWidth, TimelineSurface.ActualHeight);

        var w = (uint)Math.Max(1, Math.Round(_viewport.LogicalToPhysical(TimelineSurface.ActualWidth)));
        var h = (uint)Math.Max(1, Math.Round(_viewport.LogicalToPhysical(TimelineSurface.ActualHeight)));

        if (w == _lastW && h == _lastH && (float)scale == _lastScale)
        {
            return;
        }

        _lastW = w;
        _lastH = h;
        _lastScale = (float)scale;

        var status = TimelineNative.rudis_timeline_resize(_handle, w, h, (float)scale);
        if (status != TimelineStatus.Ok)
        {
            SurfaceRendererFault(status, "resizing the surface");
        }
    }

    /// <summary>The measured responsive rule (README:176). Keyed off the REGION's own
    /// width rather than the window's, so it is correct however the columns are
    /// split.</summary>
    private void OnRegionSizeChanged(object sender, SizeChangedEventArgs e)
    {
        var collapse = e.NewSize.Width < ZoomCollapseWidthPx;
        if (collapse == _zoomCollapsed)
        {
            return;
        }

        _zoomCollapsed = collapse;
        ZoomGroup.Visibility = collapse ? Visibility.Collapsed : Visibility.Visible;
        ZoomOverflowButton.Visibility = collapse ? Visibility.Visible : Visibility.Collapsed;
    }

    // ========================================================================
    // The mirrored-state sink (CLAUDE.md rule 4)
    // ========================================================================

    /// <summary>
    /// Rebuild the drawable model from the mirror's typed projection. Called by
    /// MainWindow on <c>ProjectChanged</c>, the same push shape every region since plan
    /// 50-05 uses — a region NEVER polls the ABI itself.
    /// </summary>
    internal void ApplyMirrorState(ShellMirror mirror)
    {
        ArgumentNullException.ThrowIfNull(mirror);

#if DEBUG
        var t0 = System.Diagnostics.Stopwatch.GetTimestamp();
#endif

        // Hoisted out of the Rebuild call, deliberately: `ShellMirror.Project` is a
        // LAZY projection (`WireJson.FromNode<Project>(_raw)`), so the first read after
        // an invalidation is where the mirror's raw JSON becomes a typed object graph —
        // the closest thing this shell has to the Phase-43 baseline's `load_project`
        // JSON-parse cost, and the number plan 52-09's criterion-3 spot check needs
        // separated from the model rebuild that follows it. Every later read in this
        // method hits the cached projection.
        var project = mirror.Project;

#if DEBUG
        var t1 = System.Diagnostics.Stopwatch.GetTimestamp();
#endif

        _model.Rebuild(project);
        _viewport.SetLanes(_model.Lanes);
        _fps = ResolveFps(mirror);
        _projectDurationUs = ResolveProjectDuration(mirror);

        // The drop-preview ghost's width source (phase 53.1): media id → duration, a
        // projection of the SAME cached `project` read above — no second mirror read,
        // rebuilt whole on each apply because media count is tile-scale, not clip-scale.
        //
        // GUARDED (plan 53.2-07, closing `deferred-items.md` § 12's CS8602). `project`
        // is a NULLABLE lazy projection: `_model.Rebuild` above accepts null and the
        // two reads under it did not, so "no project loaded" was a latent NRE on the
        // mirror-apply path. The empty map is deliberate rather than "leave the last
        // one": a null project IS the no-project state, and a stale duration table
        // would size a drop ghost from media that is no longer in the bin.
        if (project is null)
        {
            _mediaDurationsUs = EmptyMediaDurations;
        }
        else
        {
            var ghostDurations = new Dictionary<string, long>(project.MediaBin.Count);
            foreach (var media in project.MediaBin)
            {
                ghostDurations[media.Id] = media.DurationUs;
            }
            _mediaDurationsUs = ghostDurations;
        }

#if DEBUG
        var t2 = System.Diagnostics.Stopwatch.GetTimestamp();
        _lastMirrorProjectionUs = Microseconds(t0, t1);
        _lastModelRebuildUs = Microseconds(t1, t2);
        _lastApplyMirrorStateUs = Microseconds(t0, t2);
        _mirrorApplyCount++;
#endif

        // The mirror is authoritative: a rebuild may have dropped the selected clip
        // (an undone split, a delete), so the tiles are re-derived rather than left
        // pointing at a clip that no longer exists.
        UpdateToolAvailability();

        // The handoff's *Empty* state (README:139). The lanes and the ruler are still
        // DRAWN underneath it — the label is what changes, not the surface.
        EmptyState.Visibility = _model.Clips.Count == 0 ? Visibility.Visible : Visibility.Collapsed;

        if (_model.SkippedTrackKinds.Count > 0)
        {
            App.LogDiagnostic(
                $"timeline: {_model.SkippedTrackKinds.Count} track(s) skipped — this build renders " +
                "video and audio lanes only (crates/core::TrackKind, D-06)");
        }
    }

    // ========================================================================
    // SHELL-09 — peaks ride the EXISTING cold cycle (D-21). No new timer.
    // ========================================================================

    /// <summary>
    /// One cold cycle's worth of waveform-peak AND filmstrip-strip retrieval (53.2
    /// D-12 rides the peaks' cycle rather than opening a second one).
    ///
    /// <para><b>Called from MainWindow's EXISTING 100ms cold cycle</b> (Phase 50
    /// D-06's own repeating dispatcher tick, declared over there), from its
    /// UI-thread apply half, and never from anything reachable from a paint. This
    /// region opens NO periodic source of its own and the ABI grew no seventh event
    /// type for it — D-21's whole point is that the cadence already exists. (The
    /// type name of that tick is deliberately NOT written here: this plan's
    /// acceptance check for "52-08 added no timer" is a literal count over this
    /// file, and a gate its own rationale can trip is a gate that gets the
    /// rationale deleted — 52-02's recorded trap, now on its fifth outing.)</para>
    ///
    /// <para><b>Why the UI-thread half rather than the fetch half.</b> The ABI call
    /// itself never runs on this thread: <c>GetWaveformPeaksAsync</c> hands it to
    /// the interop worker and this method only awaits the answer. What DOES run
    /// here is the base64 decode and the cache mutation — and the cache is read by
    /// the frame build on this same thread, every tick. Mutating it from the pool
    /// would be a genuine <see cref="Dictionary{TKey,TValue}"/> data race that no
    /// gate in this plan would have caught, and it would buy nothing: the decode is
    /// microseconds, once per media item, ever.</para>
    ///
    /// <para>Its own in-flight gate, so a slow answer cannot stack cycles, and a
    /// total try/catch so nothing escapes into a fire-and-forget Task (D-08 —
    /// <c>async Task</c>, never <c>async void</c>).</para>
    /// </summary>
    internal async Task OnColdPollAsync()
    {
        if (_peakPollInFlight || _detached)
        {
            return;
        }

        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        _peakPollInFlight = true;
        try
        {
            var nowMs = Environment.TickCount64;

            await _peaks.PumpAsync(
                mediaId => engine.GetWaveformPeaksAsync(BuildPeaksArgs(mediaId)),
                nowMs);

            // 53.2 D-12, on the SAME existing cycle — no new timer, no seventh event
            // type. The strip wrapper takes the media id ITSELF and serialises the
            // args, so this is one line and there is no second JSON-writing site to
            // drift from the one beside the export. This is also its ONLY call site
            // in the shell, deliberately: this file is the WinUI half, OUTSIDE
            // `Regions/Timeline/`, so the paint-scope scan over that directory keeps
            // meaning "the per-frame classes cannot reach the boundary at all" —
            // exactly where the peaks pump above already sits, for the same reason.
            await _filmstrips.PumpAsync(engine.GetFilmstripStripAsync, nowMs);
        }
        catch (Exception e)
        {
            App.LogDiagnostic($"timeline cold poll faulted: {e.GetType().Name}: {e.Message}");
        }
        finally
        {
            _peakPollInFlight = false;
        }
    }

    /// <summary>
    /// <c>{"media_id": ".."}</c>, written by <c>System.Text.Json</c>'s writer rather
    /// than by interpolation — T-52-34's rule, applied to the one payload this plan
    /// sends. A media id is backend-minted today, but the rule exists precisely so
    /// that the day one is not, the payload cannot be steered by its content.
    /// </summary>
    private static string BuildPeaksArgs(string mediaId)
    {
        var buffer = new System.Buffers.ArrayBufferWriter<byte>(64);
        using (var writer = new Utf8JsonWriter(buffer))
        {
            writer.WriteStartObject();
            writer.WriteString("media_id", mediaId);
            writer.WriteEndObject();
        }

        return System.Text.Encoding.UTF8.GetString(buffer.WrittenSpan);
    }

    /// <summary>
    /// The frame rate the <c>HH:MM:SS:FF</c> readout counts in, taken from real
    /// mirrored state and never invented: the active playback's rate first, then the
    /// first media item that reports one, then
    /// <see cref="TimelineTimecode.FallbackFps"/>. The fallback only bites before any
    /// media exists, when the readout is all zeros and the frame field is correct at
    /// any rate.
    /// </summary>
    private static double ResolveFps(ShellMirror mirror)
    {
        var playbackFps = mirror.ActivePlayback?.Fps ?? 0;
        if (playbackFps >= 1.0)
        {
            return playbackFps;
        }

        var bin = mirror.Project?.MediaBin;
        if (bin is not null)
        {
            for (var i = 0; i < bin.Count; i++)
            {
                if (bin[i]?.Fps >= 1.0)
                {
                    return bin[i].Fps;
                }
            }
        }

        return TimelineTimecode.FallbackFps;
    }

    /// <summary>The furthest clip end in the project, which is what `Zoom to fit` fits.
    /// Cold-path only (once per mirror patch), so a walk of every clip is the right
    /// shape here even though nothing else in this region may do one.</summary>
    private static long ResolveProjectDuration(ShellMirror mirror)
    {
        var tracks = mirror.Project?.Timeline?.Tracks;
        if (tracks is null)
        {
            return 0;
        }

        var durationUs = 0L;
        for (var t = 0; t < tracks.Count; t++)
        {
            var clips = tracks[t]?.Clips;
            if (clips is null)
            {
                continue;
            }

            for (var c = 0; c < clips.Count; c++)
            {
                var clip = clips[c];
                if (clip is null)
                {
                    continue;
                }

                var end = clip.StartUs + (clip.OutUs - clip.InUs);
                if (end > durationUs)
                {
                    durationUs = end;
                }
            }
        }

        return durationUs;
    }

    // ========================================================================
    // The render tick — MainWindow's ONE composition-tick slot
    // ========================================================================

    /// <summary>
    /// Build one frame and hand it to the renderer.
    ///
    /// <para><b>Called from MainWindow's EXISTING composition-tick consumer</b> (plan
    /// 50-04's single slot) — this region opens no second per-frame subscription, and
    /// it makes no ABI call of its own for the playhead: the value arrives already read
    /// by <c>PlayheadTicker</c> this same tick (T-52-30).</para>
    ///
    /// <para><b>A clean frame is still SENT.</b> When the builder reports nothing
    /// changed it emits the same frame with <c>Dirty == 0</c>, and the renderer's dirty
    /// gate refuses it before acquiring a surface texture, incrementing
    /// <c>skipped_clean_frames</c>. Skipping the call here instead would be marginally
    /// cheaper and would destroy the only evidence that an idle Timeline does no GPU
    /// work.</para>
    /// </summary>
    public void OnRenderingTick(long? playheadUs)
    {
        if (!_attached || _handle is null || _handle.IsInvalid)
        {
            return;
        }

        // ⚠ THE PAINT SCOPE (plan 52-08 / criterion 5). Everything below is the
        // paint/present callstack, and an ABI call issued from inside it increments
        // `RudisNative.AbiCallsInsidePaintScope` — a counter proven able to report a
        // violation by its own companion test, so a reading of zero is EVIDENCE rather
        // than an absence of instrumentation. The scope is a struct, so this costs no
        // allocation on a 60Hz tick.
        using var paintScope = RudisNative.EnterPaintScope();

        var positionUs = playheadUs ?? 0;
        RenderTimecode(positionUs);

#if DEBUG
        // ARM before the build, so a frame drawn on THIS tick can answer an input that
        // landed between the previous tick and this one. Arming after the build would
        // systematically add one frame period (~16.7ms at 60Hz) to every measurement —
        // a sixth of criterion 2's whole budget, manufactured by the instrument.
        ArmLatencyMeasurement();
#endif

        // The state machine is handed the playhead this tick already read — it never
        // asks for one itself (T-52-30). `Split`'s availability depends on it, so the
        // tiles are re-derived here; the write is change-gated inside.
        _interaction.PlayheadUs = positionUs;
        UpdateToolAvailability();

        // The drag/trim overlay, in LOGICAL px, into caller-owned storage. Both are
        // no-ops with no gesture in flight, and the builder refuses a frame whose
        // overlay has not moved — so an idle Timeline still costs nothing.
        var hasGhost = _interaction.TryGetGhost(out var ghost);
        var guideCount = _interaction.GetSnapGuides(_snapGuides);
        _builder.SetOverlay(hasGhost, ghost, _snapGuides.AsSpan(0, guideCount));
        RenderDragReadout(hasGhost, ghost);

        // The selected lane's gutter mark (quick 260731-k9b). Change-gated inside, the
        // same discipline as the readout above: an idle Timeline with a lane selected
        // touches the visual tree zero times.
        RenderSelectedLaneIndicator();

        _builder.Build(_model, _viewport, positionUs, _model.SelectedClipId, out var frame);

        var status = TimelineNative.rudis_timeline_render(_handle, frame);
        if (status != TimelineStatus.Ok)
        {
            SurfaceRendererFault(status, "rendering a frame");
        }

#if DEBUG
        // A CLEAN frame is still SENT (see this method's remarks) and the renderer
        // refuses it before acquiring a surface texture — so `Dirty` is the only honest
        // reading of "this Timeline actually redrew". Stamping the redraw clock on
        // every tick instead would make `last_redraw_unix_ms` a 60Hz heartbeat, and
        // criterion 2's poll ("the first redraw AFTER my input") would then always
        // succeed on the very next tick regardless of whether anything was drawn.
        if (frame.Dirty != 0)
        {
            SettleLatencyMeasurement();
        }

        PublishIntrospectionSnapshot(positionUs);
#endif
    }

#if DEBUG
    /// <summary>Micro-seconds between two <see cref="System.Diagnostics.Stopwatch"/>
    /// timestamps, in one place so no call site re-derives the frequency scaling.</summary>
    private static long Microseconds(long fromTicks, long toTicks) =>
        (long)((toTicks - fromTicks) * 1_000_000.0 / System.Diagnostics.Stopwatch.Frequency);

    /// <summary>
    /// A new ACCEPTED input since the last one arms the feedback measurement.
    ///
    /// <para>Re-arming per input is deliberate for a DRAG: every pointer move stamps,
    /// so <c>t_feedback</c> reads "the latest move → the next drawn frame", which is
    /// the latency a user actually feels while dragging. Measuring only the first move
    /// of a gesture would report the cheapest sample in the whole drag.</para>
    /// </summary>
    private void ArmLatencyMeasurement()
    {
        var inputMs = _interaction.LastInputUnixMs;
        if (inputMs == 0 || inputMs == _pendingInputUnixMs)
        {
            return;
        }

        _pendingInputUnixMs = inputMs;
        _feedbackArmed = true;
    }

    /// <summary>Called only from a tick that DREW.</summary>
    private void SettleLatencyMeasurement()
    {
        var nowMs = TimelineInteraction.NowUnixMs();
        _lastRedrawUnixMs = nowMs;

        if (_feedbackArmed)
        {
            _feedbackArmed = false;
            _lastFeedbackInputUnixMs = _pendingInputUnixMs;
            _lastFeedbackLatencyMs = nowMs - _pendingInputUnixMs;
        }
    }
#endif

    /// <summary>
    /// The dragging/trimming duration label (README:140), parked under the ghost.
    ///
    /// <para>Three separate change gates, all for the 50-06 reason: a
    /// <c>Text</c> write allocates a string and raises a UIA property change, a
    /// <c>Margin</c> write forces a layout pass, and a <c>Visibility</c> write does
    /// both. None of the three happens on a tick where its own input did not move — so
    /// an idle Timeline touches the visual tree exactly zero times, and a drag touches
    /// it only for the parts that actually changed.</para>
    /// </summary>
    private void RenderDragReadout(bool hasGhost, in GhostRect ghost)
    {
        var text = _interaction.LiveDurationReadout;

        if (!hasGhost || text.IsEmpty)
        {
            if (_readoutVisible)
            {
                _readoutVisible = false;
                DragReadout.Visibility = Visibility.Collapsed;
            }

            return;
        }

        if (text.Length != _lastReadoutLength || !text.SequenceEqual(DragReadoutText.Text.AsSpan()))
        {
            _lastReadoutLength = text.Length;
            DragReadoutText.Text = new string(text);
        }

        // The ghost's y is measured from the SURFACE top, which spans both grid rows;
        // this Border lives in row 1, so the toolbar band comes off once, here.
        var left = ghost.XLogicalPx;
        var top = ghost.YLogicalPx - TimelineMetrics.TimelineHeaderHeight + ghost.HLogicalPx + 4;
        if (left != _lastReadoutLeft || top != _lastReadoutTop)
        {
            _lastReadoutLeft = left;
            _lastReadoutTop = top;
            DragReadout.Margin = new Thickness(left, top, 0, 0);
        }

        if (!_readoutVisible)
        {
            _readoutVisible = true;
            DragReadout.Visibility = Visibility.Visible;
        }
    }

    /// <summary>Format into the reused buffer and touch the visual tree ONLY when the
    /// rendered text actually changed — the change-detection-lives-in-the-unit
    /// discipline plan 50-06 established for Transport's readout.</summary>
    private void RenderTimecode(long positionUs)
    {
        if (!TimelineTimecode.TryWriteFrameTimecode(positionUs, _fps, _timecodeChars, out var written))
        {
            return;
        }

        var span = _timecodeChars.AsSpan(0, written);
        if (written == _lastTimecodeLength && span.SequenceEqual(Timecode.Text.AsSpan()))
        {
            return;
        }

        _lastTimecodeLength = written;
        Timecode.Text = new string(span);
    }

    // ========================================================================
    // Pointer — the ONE DPI boundary, and nothing else (D-14)
    // ========================================================================

    /// <summary>
    /// Every pointer handler below reads
    /// <c>e.GetCurrentPoint(TimelineSurface).Position</c> and passes it STRAIGHT
    /// through.
    ///
    /// <para>That position is already LOGICAL px, panel-relative — exactly the space
    /// <c>Regions/Timeline/</c> speaks, and exactly the space the hit tester, the
    /// viewport and the frame builder agree on. There is no conversion here, and that
    /// is the point: this trap has cost the project three times (50-05's screen-scrape,
    /// 50-06's 44-vs-55px measurement, 52-01's clipped screenshot), and every one of
    /// them came from a coordinate that had been through a DIFFERENT space on its way
    /// in. A synthesized screen coordinate, a window-relative rect or a physical pixel
    /// would each need a conversion; the panel-relative point needs none.</para>
    /// </summary>
    private void OnSurfacePointerPressed(object sender, PointerRoutedEventArgs e)
    {
        var point = e.GetCurrentPoint(TimelineSurface).Position;

        // Focus the surface so the region's SCOPED accelerators (S / Delete / Back /
        // Escape) fire. Plan 52-06 left the panel a tab stop that nothing focused; this
        // is the other half of that.
        TimelineSurface.Focus(FocusState.Pointer);

        _interaction.OnPointerPressed(point.X, point.Y, ModifiersOf(e));
        TimelineSurface.CapturePointer(e.Pointer);
        e.Handled = true;

        UpdateToolAvailability();
        DrainSeek();
    }

    private void OnSurfacePointerMoved(object sender, PointerRoutedEventArgs e)
    {
        // A hover does nothing yet — cursor feedback over trim handles belongs to
        // whichever plan owns cursors, and answering a hover here would put the hit
        // tester on every mouse move for no visible result.
        if (_interaction.State == InteractionState.Idle)
        {
            return;
        }

        var point = e.GetCurrentPoint(TimelineSurface).Position;
        _interaction.OnPointerMoved(point.X, point.Y);
        e.Handled = true;

        // NOTE: no dispatch here, ever (D-11). The only thing a move can produce is a
        // scrub seek, which is playback rather than an edit and is capped at one in
        // flight.
        DrainSeek();
    }

    private void OnSurfacePointerReleased(object sender, PointerRoutedEventArgs e)
    {
        var point = e.GetCurrentPoint(TimelineSurface).Position;
        var pending = _interaction.OnPointerReleased(point.X, point.Y);

        TimelineSurface.ReleasePointerCapture(e.Pointer);
        e.Handled = true;

        DrainSeek();
        Submit(pending);
    }

    /// <summary>Capture lost (or cancelled): the gesture is abandoned, the ghost is
    /// discarded and NOTHING is dispatched.</summary>
    private void OnSurfacePointerCaptureLost(object sender, PointerRoutedEventArgs e)
    {
        _interaction.OnCancel();
        UpdateToolAvailability();
    }

    private void OnSurfacePointerWheelChanged(object sender, PointerRoutedEventArgs e)
    {
        var point = e.GetCurrentPoint(TimelineSurface);
        _interaction.OnWheel(
            point.Properties.MouseWheelDelta,
            ModifiersOf(e).HasFlag(PointerModifiers.Control),
            point.Position.X);
        e.Handled = true;
    }

    private static PointerModifiers ModifiersOf(PointerRoutedEventArgs e)
    {
        var modifiers = PointerModifiers.None;
        var held = e.KeyModifiers;

        if (held.HasFlag(VirtualKeyModifiers.Control))
        {
            modifiers |= PointerModifiers.Control;
        }

        if (held.HasFlag(VirtualKeyModifiers.Shift))
        {
            modifiers |= PointerModifiers.Shift;
        }

        if (held.HasFlag(VirtualKeyModifiers.Menu))
        {
            modifiers |= PointerModifiers.Alt;
        }

        return modifiers;
    }

    // ========================================================================
    // `Timeline › Toolbar` and the keyboard — the same three edits, two ways in
    // ========================================================================

    private void OnSplitClick(object sender, RoutedEventArgs e) =>
        Submit(_interaction.RequestSplitAtPlayhead());

    private void OnDuplicateClick(object sender, RoutedEventArgs e) =>
        Submit(_interaction.RequestDuplicate());

    private void OnDeleteClick(object sender, RoutedEventArgs e) =>
        Submit(_interaction.RequestRemove());

    // ========================================================================
    // Track management (plan 52-14) — the owner's one scope addition
    // ========================================================================
    //
    // A PORT, not an invention. v6.0's timeline toolbar carried `#tl-add-video-track`
    // and `#tl-add-audio-track` (frontend/src/main.ts:1478-1479) and a per-lane `✕` in
    // the gutter, and `removeTrackAt` (main.ts:1259-1270) gated the destructive one
    // behind a confirm ONLY when the track held clips — Cancel/Esc/dismiss being a
    // no-op. That rule is ported by BEHAVIOUR; no v6 source was copied.
    //
    // Both commands already existed in the frozen `crates/core` and both are undoable
    // there (`AddTrack` at command.rs:393 with `RemoveTrack` as its inverse; `RemoveTrack`
    // at :396 with `RestoreTrack`, which carries the whole removed track back). Only the
    // gesture was missing. Nothing here adds an FFI export, a Command variant or an
    // event type — this is a UI affordance over a backend that was already complete.

    private void OnAddVideoTrackClick(object sender, RoutedEventArgs e) =>
        RequestAddTrack(LaneModel.VideoKind);

    private void OnAddAudioTrackClick(object sender, RoutedEventArgs e) =>
        RequestAddTrack(LaneModel.AudioKind);

    /// <summary>
    /// Ask the backend for one more empty track, unless the lane stack is already at the
    /// model's ceiling.
    ///
    /// <para><b>The refusal is VISIBLE, never a silent no-op</b> (T-52-68).
    /// <see cref="LaneModel.MaxLanes"/> bounds how many lanes a payload may be turned
    /// into, so a track added beyond it would exist in the project and never be drawn —
    /// a user pressing a live-looking control and getting nothing, with the reason
    /// visible only to a debugger. Refusing it here, out loud, is the honest answer;
    /// dispatching it would be the "the UI shows X" failure CLAUDE.md rule 3
    /// forbids.</para>
    ///
    /// <para>A note on where the new lane appears, because it surprises people:
    /// <c>command.rs:390-392</c> INSERTS a video track at index 0 (lowest index is the
    /// top compositing layer) and APPENDS an audio track at the bottom. "Add video
    /// track" therefore puts the new lane at the TOP of the stack.</para>
    /// </summary>
    private void RequestAddTrack(string kind)
    {
        if (_model.Lanes.Count >= LaneModel.MaxLanes)
        {
            ShowStatus(
                $"This timeline already has {_model.Lanes.Count} lanes, which is the most this " +
                "build draws. No track was added.",
                fatal: false);
            return;
        }

        Submit(new PendingCommand(PendingKind.AddTrack, TimelineCommands.AddTrack(kind)));
    }

    private void OnRemoveTrackClick(object sender, RoutedEventArgs e) =>
        _ = RemoveSelectedTrackAsync();

    /// <summary>
    /// Remove the SELECTED track — the selected LANE's when there is one, otherwise the
    /// selected CLIP's — asking first when that track holds clips.
    ///
    /// <para><b>The index is resolved at the MOMENT of the gesture and re-resolved after
    /// the dialog</b> (T-52-66). <c>RemoveTrack</c> shifts every later track down by one,
    /// so an index cached across a mirror update names a DIFFERENT track by the time it
    /// is sent — and this command cascades, so naming the wrong track is data loss rather
    /// than a wrong-looking screen. The confirmation is an <c>await</c>, which is exactly
    /// such a window: another session's edit, an agent action or an undo can all land
    /// while it is open. So the selection is read again afterwards and the gesture is
    /// REFUSED if anything about it moved. Refusing is the right answer for a destructive
    /// cascade; silently retargeting is not.</para>
    ///
    /// <para><b>The confirm rule is v6's, ported verbatim as behaviour</b>
    /// (<c>main.ts</c> <c>removeTrackAt</c>): a track with ZERO clips is removed with no
    /// prompt — there is nothing to warn about and the removal is undoable either way —
    /// and a track with clips gets <see cref="Dialogs.RemoveTrackDialog"/>, whose message
    /// names the kind and the count because the count is the part of the cascade the user
    /// cannot see from the control they pressed. Cancel, Esc and dismiss are all a NO-OP.
    /// Adding a prompt v6 does not have would be inventing UX under the banner of a
    /// port.</para>
    ///
    /// <para>The dialog itself is plan 51-06's — the SAME <c>ContentDialog</c>, with the
    /// SAME message string, reached at last by the production trigger its own doc comment
    /// says was waiting on this region.</para>
    /// </summary>
    private async Task RemoveSelectedTrackAsync()
    {
        if (!TryResolveTargetTrack(out var trackIndex, out var kind, out var clipCount))
        {
            return;
        }

        if (clipCount > 0)
        {
            var root = XamlRoot;
            if (root is null)
            {
                ShowStatus(
                    "The remove-track confirmation could not be shown, so nothing was removed.",
                    fatal: true);
                return;
            }

            var dialog = new Rudis.Shell.Dialogs.RemoveTrackDialog();
            var confirmed = await dialog.ConfirmAsync(root, kind, clipCount);
            if (!confirmed)
            {
                App.LogDiagnostic(
                    $"remove_track: cancelled at the confirmation for track {trackIndex} " +
                    $"({kind}, {clipCount} clip(s)) — nothing dispatched");
                return;
            }

            // The await above is a window in which the mirror can move underneath us.
            // All THREE facts are re-checked, not just the index: a track that kept its
            // index while its kind or its clip count changed is a different subject
            // from the one the user was shown, and the message they agreed to named
            // exactly those two numbers.
            if (!TryResolveTargetTrack(out var recheckIndex, out var recheckKind, out var recheckCount)
                || recheckIndex != trackIndex
                || !string.Equals(recheckKind, kind, StringComparison.Ordinal)
                || recheckCount != clipCount)
            {
                App.LogDiagnostic(
                    $"remove_track: the selection moved from track {trackIndex} ({kind}, " +
                    $"{clipCount} clip(s)) while the confirmation was open — nothing dispatched");
                ShowStatus(
                    "The timeline changed while the confirmation was open, so nothing was removed.",
                    fatal: false);
                return;
            }
        }

        // The track this names is about to cease to exist, and `RemoveTrack` shifts
        // every LATER index down by one — so a surviving lane selection would silently
        // re-point at whichever track slid into its place. Dropped HERE rather than
        // left to the rebuild, because `DropSelectedTrackIfGone` keeps an index that
        // still names SOMETHING and after this removal that something is somebody
        // else's track. If the domain refuses the command the cost is a cleared
        // selection, which is cheap; the alternative cost is the wrong track.
        _model.ClearSelection();

        Submit(new PendingCommand(PendingKind.RemoveTrack, TimelineCommands.RemoveTrack(trackIndex)));
    }

    /// <summary>
    /// The selected clip's TRACK index, that track's kind, and how many clips it holds —
    /// all read from the model as it is RIGHT NOW.
    ///
    /// <para><c>ClipSource.LaneIndex</c> is a LANE index; the command wants a TRACK index
    /// and the two differ whenever a track kind this build cannot draw was skipped, so
    /// the answer goes through <c>Lane.TrackIndex</c> exactly as the cross-lane drag
    /// does. Returns <see langword="false"/> — never throws — when there is nothing
    /// selected or the selection is one mirror patch stale.</para>
    /// </summary>
    private bool TryResolveSelectedTrack(out int trackIndex, out string kind, out int clipCount)
    {
        trackIndex = -1;
        kind = string.Empty;
        clipCount = 0;

        if (!_model.TryGetSelectedSource(out var selected) || selected.IsEmpty)
        {
            return false;
        }

        var lanes = _model.Lanes;
        var laneIndex = selected.LaneIndex;
        if (laneIndex < 0 || laneIndex >= lanes.Count)
        {
            return false;
        }

        var lane = lanes[laneIndex];
        trackIndex = lane.TrackIndex;
        kind = lane.Kind;

        // Counted rather than cached: this runs once per gesture, and a stored per-lane
        // count would be one more thing to keep in step with every mirror patch.
        var clips = _model.Clips;
        for (var i = 0; i < clips.Count; i++)
        {
            if (clips[i].LaneIndex == laneIndex)
            {
                clipCount++;
            }
        }

        return true;
    }

    // Zoom is pure VIEW state — no backend call, no mirror patch, nothing to undo.
    private void OnZoomInClick(object sender, RoutedEventArgs e) => _interaction.ZoomIn();

    private void OnZoomOutClick(object sender, RoutedEventArgs e) => _interaction.ZoomOut();

    private void OnZoomFitClick(object sender, RoutedEventArgs e) =>
        _interaction.ZoomToFit(_projectDurationUs);

    private void OnSplitAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        Submit(_interaction.OnKey(TimelineKey.Split));
    }

    private void OnDeleteAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        Submit(_interaction.OnKey(TimelineKey.Delete));
    }

    private void OnEscapeAccelerator(KeyboardAccelerator sender, KeyboardAcceleratorInvokedEventArgs args)
    {
        args.Handled = true;
        Submit(_interaction.OnKey(TimelineKey.Escape));
    }

    /// <summary>Re-derive the tiles, then send whatever the gesture asked for. Both
    /// halves in one place so no route can do one and forget the other.</summary>
    private void Submit(PendingCommand? pending)
    {
        UpdateToolAvailability();

        if (pending is { } command)
        {
            _ = DispatchAsync(command);
        }
    }

    /// <summary>
    /// The tiles follow the SELECTION, and `Split` additionally follows the PLAYHEAD.
    ///
    /// <para>Written only on a change. This runs on the composition tick because
    /// <c>CanSplitAtPlayhead</c> moves with the playhead, and an unconditional
    /// <c>IsEnabled</c> assignment raises a UIA property-change event — 50-06 measured
    /// what a per-tick one of those does.</para>
    /// </summary>
    private void UpdateToolAvailability()
    {
        var canSplit = _interaction.CanSplitAtPlayhead;
        if (canSplit != _splitEnabled)
        {
            _splitEnabled = canSplit;
            SplitButton.IsEnabled = canSplit;
        }

        var hasSelection = _interaction.HasSelection;
        if (hasSelection != _selectionEnabled)
        {
            _selectionEnabled = hasSelection;
            DuplicateButton.IsEnabled = hasSelection;
            DeleteButton.IsEnabled = hasSelection;
        }

        // Plan 52-14. The toolbar is where the track gesture lives because the drawn
        // gutter has no per-lane control (SHELL-05), so the only thing that can name a
        // track is the selection — and with nothing selected there is no track to name.
        // A greyed tile here is correct, not a bug.
        //
        // Quick 260731-k9b gave it a SECOND subject and therefore its own change gate:
        // the remove tile follows a selected LANE as well as a selected clip, and the
        // two move independently. Riding on `hasSelection`'s gate would have made the
        // tile answer a question it is no longer asking — and would have left the
        // owner's case (an EMPTY track, which has no clip at all) unreachable.
        UpdateRemoveTrackAvailability(hasSelection);
    }

    // ========================================================================
    // Sending — one generic path for every edit (D-12)
    // ========================================================================

    /// <summary>
    /// Hand one built edit to the backend through the EXISTING generic dispatch
    /// export, on the interop worker.
    ///
    /// <para><b>The two failure layers stay SEPARATE</b> (the 50-06 rule, and this
    /// region's own six-code renderer contract already follows it): a domain
    /// <c>{"Err": ..}</c> means the backend REFUSED the edit by its own rules — visible,
    /// not alarming, and the surface re-renders from the mirror so it can never be left
    /// showing a state the backend rejected. A <c>RudisStatus</c> other than
    /// <c>Ok</c> is a transport FAULT and gets the distinct, accent-filled treatment,
    /// because a caught panic and a refused command have different causes and different
    /// fixes and a user who cannot tell them apart cannot report either.</para>
    ///
    /// <para><b>Nothing is pre-rendered.</b> The new state arrives as a
    /// <c>project:changed</c> patch on the mirror's own cadence and the surface redraws
    /// when <see cref="ApplyMirrorState"/> lands — never before. CLAUDE.md rule 4: the
    /// backend owns the project, and this region is a read-only mirror of it.</para>
    /// </summary>
    private async Task DispatchAsync(PendingCommand command)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            ShowStatus(
                $"The Timeline could not apply the {command.Kind} edit: no engine instance exists.",
                fatal: true);
            return;
        }

        RudisResult<JsonElement> result;
        try
        {
            result = await engine.DispatchCommandAsync(command.Json);
        }
        catch (Exception ex)
        {
            SurfaceEditFault(command, RudisStatus.PanicCaught, $"{ex.GetType().Name}: {ex.Message}");
            return;
        }

        switch (result.Kind)
        {
            case RudisResultKind.Ok:
                HideStatus();
                App.LogDiagnostic($"timeline edit {command.Kind} APPLIED: {command.Json}");

                // The backend has already applied it, so there is a patch waiting on
                // the ring RIGHT NOW. Ask the mirror to look instead of letting the
                // 100ms cold tick decide when the user sees their own edit (plan
                // 52-09 / criterion 2 — measured as the dominant term). Nothing is
                // pre-rendered and nothing is assumed: the surface still redraws only
                // when ApplyMirrorState lands, exactly as the remarks above say.
                App.RequestMirrorPollNow?.Invoke();
                return;

            case RudisResultKind.DomainError:
                App.LogDiagnostic($"timeline edit {command.Kind} REFUSED: {result.Error} — {command.Json}");
                ShowStatus($"{command.Kind} refused: {result.Error}", fatal: false);
                _builder.MarkDirty();
                return;

            default:
                SurfaceEditFault(command, result.Status, result.Error);
                return;
        }
    }

    private void SurfaceEditFault(PendingCommand command, RudisStatus status, string? detail)
    {
        App.LogDiagnostic($"timeline edit {command.Kind} FAULT ({status}): {detail} — {command.Json}");
        ShowStatus($"ENGINE FAULT · {command.Kind} · {status}", fatal: true);
        _builder.MarkDirty();
    }

    // ========================================================================
    // The drop target — MediaBin → Timeline (plan 53.1-02)
    // ========================================================================

    /// <summary>
    /// Hover feedback, and the ROUTED-EVENT OWNERSHIP BOUNDARY (SC-5).
    ///
    /// <para><b><c>Handled</c> is set ONLY for a Text-bearing drag.</b> The window
    /// root's import path accepts the storage-item (file) format and nothing
    /// else, so an Explorer file drag must travel PAST the Timeline and reach it. Two
    /// mechanisms keep those apart and both are deliberate: the format discrimination
    /// below, and the <c>Handled</c> discipline that states the ownership boundary in
    /// code rather than leaving it to an accident of format-checking.</para>
    ///
    /// <para><b>No kind check here.</b> Whether this media may live on this track is
    /// the BACKEND's rule. Resolving it during hover would need the media id decoded
    /// against the mirror and would create a second copy of the backend's compatibility
    /// rule to drift out of sync with it. A lane accepts <c>Copy</c>; the refusal, if
    /// any, happens at Drop, from the backend, in its own words.</para>
    /// </summary>
    private void OnSurfaceDragOver(object sender, DragEventArgs e)
    {
        if (!e.DataView.Contains(StandardDataFormats.Text))
        {
            // Not ours. Leave AcceptedOperation alone and DO NOT set Handled — the
            // window root's file-drop handler is upstream and must still see this.
            return;
        }

        var pt = e.GetPosition(TimelineSurface);
        var drop = TimelineDropTarget.Resolve(_viewport, pt.X, pt.Y);

        e.AcceptedOperation = drop.OnLane ? DataPackageOperation.Copy : DataPackageOperation.None;
        e.DragUIOverride.Caption = drop.OnLane ? "Place on Timeline" : "";
        e.DragUIOverride.IsGlyphVisible = drop.OnLane;
        e.Handled = true;

        UpdateDropGhost(drop);
    }

    // ========================================================================
    // The drop-preview ghost (phase 53.1, owner request mid-UAT): a translucent
    // clip-sized rectangle at the exact (track, time) the drop would resolve to,
    // sized by the dragged media's real duration out of the mirror.
    // ========================================================================

    /// <summary>Dragged media's duration lookup, rebuilt on every mirror apply.
    /// Read on the UI thread only (DragOver), so a plain swap is enough.</summary>
    private IReadOnlyDictionary<string, long> _mediaDurationsUs = EmptyMediaDurations;

    /// <summary>The no-project state's table, allocated once. A shared EMPTY map
    /// rather than a null field, so every reader stays a plain lookup and the
    /// "no project loaded" case needs no second branch anywhere.</summary>
    private static readonly IReadOnlyDictionary<string, long> EmptyMediaDurations =
        new Dictionary<string, long>();

    /// <summary>The media id cached by <see cref="OnSurfaceDragEnter"/>'s one async
    /// read, so the per-frame DragOver never awaits. Null until the read lands —
    /// the ghost falls back to a nominal width until it does (typically one frame).</summary>
    private string? _hoverDragMediaId;

    /// <summary>Ghost width while the payload text is still in flight: 3s of
    /// timeline, scaled by the live zoom like everything else.</summary>
    private const long GhostFallbackDurationUs = 3_000_000;

    /// <summary>What the backend actually PLACES for a still image (53.1-REVIEW WR-01):
    /// a still probes `duration_us = 0`, and `run_place_clip` substitutes
    /// `DEFAULT_STILL_DURATION_US = 5_000_000` (`crates/core/src/tools.rs:57`,
    /// applied at `crates/app-core/src/place.rs:79-90`). A ghost computed from the
    /// RAW mirror duration would be 0px wide — collapsed — for exactly the media
    /// kind whose placement succeeds anyway. The preview must predict the drop, so
    /// it mirrors the backend's substitution rather than the raw probe.</summary>
    private const long BackendStillDurationUs = 5_000_000;

    private void OnSurfaceDragEnter(object sender, DragEventArgs e)
    {
        if (!e.DataView.Contains(StandardDataFormats.Text))
        {
            return;   // same ownership rule as DragOver — not ours, let it bubble
        }

        var deferral = e.GetDeferral();
        _ = CacheHoverMediaIdAsync(e, deferral);
    }

    private async Task CacheHoverMediaIdAsync(DragEventArgs e, DragOperationDeferral deferral)
    {
        try
        {
            _hoverDragMediaId = await e.DataView.GetTextAsync();
        }
        catch (Exception ex)
        {
            // A failed read only costs the ghost its true width — the fallback keeps
            // the affordance alive, and Drop re-reads the text itself regardless.
            App.LogDiagnostic(
                $"timeline drag-enter: text read threw {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            deferral.Complete();
        }
    }

    private void OnSurfaceDragLeave(object sender, DragEventArgs e)
    {
        _hoverDragMediaId = null;
        DropGhost.Visibility = Visibility.Collapsed;
    }

    /// <summary>Position the ghost with the SAME viewport conversions the drop itself
    /// resolves through — <c>TimeUsToPixel</c>/<c>SurfaceXFromLaneAreaX</c> for x and
    /// width, <c>LaneTopPx</c>/<c>SurfaceYFromLaneAreaY</c> for y, <c>LaneHeightPx</c>
    /// for height — so what the preview shows IS what the drop does, by construction
    /// rather than by parallel arithmetic.</summary>
    private void UpdateDropGhost(TimelineDropTarget.DropPoint drop)
    {
        if (!drop.OnLane)
        {
            DropGhost.Visibility = Visibility.Collapsed;
            return;
        }

        var durationUs =
            _hoverDragMediaId is { } id && _mediaDurationsUs.TryGetValue(id, out var known)
                ? (known > 0 ? known : BackendStillDurationUs)   // 0 = a still; predict what the backend places
                : GhostFallbackDurationUs;

        // The lane band starts past the TrackHeader gutter; a ghost for a clip whose
        // start is scrolled off to the left is CLIPPED at the band's edge, not drawn
        // over the headers.
        var bandLeft = _viewport.SurfaceXFromLaneAreaX(0);
        var xStart = _viewport.SurfaceXFromLaneAreaX(_viewport.TimeUsToPixel(drop.StartUs));
        var xEnd = _viewport.SurfaceXFromLaneAreaX(_viewport.TimeUsToPixel(drop.StartUs + durationUs));
        var left = Math.Max(bandLeft, xStart);
        var width = xEnd - left;

        var top = _viewport.SurfaceYFromLaneAreaY(_viewport.LaneTopPx(drop.LaneIndex));
        var height = _viewport.LaneHeightPx(drop.LaneIndex);

        if (width < 1 || height < 1)
        {
            DropGhost.Visibility = Visibility.Collapsed;
            return;
        }

        DropGhost.Margin = new Thickness(left, top, 0, 0);
        DropGhost.Width = width;
        DropGhost.Height = height;
        DropGhost.Visibility = Visibility.Visible;
    }

    /// <summary>
    /// The real placement. Reads the media id off the package's TEXT format, and
    /// deliberately NOT off its descriptive property set: that property set is the
    /// metadata bag (Title/Thumbnail-style), not the queryable custom-format channel
    /// <c>Contains</c>/<c>GetDataAsync(formatId)</c> reads from, and whether an
    /// arbitrary custom key marshals across a real cross-process drag is unverified in
    /// this repo. The text channel is also exactly what v6.0's own fallback models.
    /// </summary>
    private void OnSurfaceDrop(object sender, DragEventArgs e)
    {
        if (!e.DataView.Contains(StandardDataFormats.Text))
        {
            return;   // same ownership rule as DragOver — let it bubble
        }

        // The preview's work is done the moment the real placement takes over.
        _hoverDragMediaId = null;
        DropGhost.Visibility = Visibility.Collapsed;

        e.Handled = true;
        var deferral = e.GetDeferral();
        _ = CompleteDropAsync(e, deferral);
    }

    private async Task CompleteDropAsync(DragEventArgs e, DragOperationDeferral deferral)
    {
        try
        {
            // The point is read BEFORE the first await. `GetPosition` answers from the
            // live drag session, and after an await that session may already have
            // ended — a drop resolved from a stale or defaulted point would place a
            // real clip at the wrong track and time, which is worse than not placing
            // one at all.
            var pt = e.GetPosition(TimelineSurface);
            var drop = TimelineDropTarget.Resolve(_viewport, pt.X, pt.Y);

            string? mediaId = null;
            try
            {
                mediaId = await e.DataView.GetTextAsync();
            }
            catch (Exception ex)
            {
                App.LogDiagnostic($"timeline drop: reading the drag text threw {ex.GetType().Name}: {ex.Message}");
            }

            if (!drop.OnLane || !TimelineDropTarget.IsPlaceableMediaId(mediaId))
            {
                // No lane under the pointer, or a payload that is not shaped like an id
                // (T-53.1-06): nothing is dispatched and nothing is drawn. A phantom
                // clip is exactly what SC-3 forbids.
                e.AcceptedOperation = DataPackageOperation.None;
                App.LogDiagnostic(
                    $"timeline drop: ignored (onLane={drop.OnLane}, idLen={mediaId?.Length ?? -1})");
                return;
            }

            e.AcceptedOperation = DataPackageOperation.Copy;
            await TryPlaceFromDropAsync(mediaId!, drop.TrackIndex, drop.StartUs);
        }
        catch (Exception ex)
        {
            // A throw out of here would escape into a fire-and-forget task and take the
            // process down, during an OS drag session, with no status shown.
            App.LogDiagnostic($"timeline drop: faulted {ex.GetType().Name}: {ex.Message}");
            ShowStatus($"ENGINE FAULT · drop · {ex.GetType().Name}", fatal: true);
        }
        finally
        {
            deferral.Complete();
        }
    }

    /// <summary>
    /// THE ONE PLACEMENT ENTRY POINT, <see langword="internal"/> so both drag
    /// mechanisms reach the same code: the OLE <c>Drop</c> path above, and MediaBin's
    /// own drag source (plan 53.1-03). One entry point, never two implementations.
    /// </summary>
    internal async Task<bool> TryPlaceFromDropAsync(string mediaId, int track, long startUs)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            ShowStatus("The Timeline could not place the clip: no engine instance exists.", fatal: true);
            PublishDropOutcome(mediaId, track, startUs, "fault", "no engine instance");
            return false;
        }

        var args = new JsonObject
        {
            ["media_id"] = mediaId,
            ["track"] = track,
            ["start_us"] = startUs,
        };
        var json = args.ToJsonString();

        RudisResult<JsonElement> result;
        try
        {
            result = await engine.PlaceClipAsync(json);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"timeline drop place_clip FAULT (throw): {ex.GetType().Name}: {ex.Message} — {json}");
            ShowStatus($"ENGINE FAULT · place_clip · {ex.GetType().Name}", fatal: true);
            _builder.MarkDirty();
            PublishDropOutcome(mediaId, track, startUs, "fault", ex.GetType().Name);
            return false;
        }

        switch (result.Kind)
        {
            case RudisResultKind.Ok:
                HideStatus();
                App.LogDiagnostic($"timeline drop place_clip APPLIED: {json}");

                // The backend has already applied it, so there is a patch waiting on the
                // ring right now — the same fast path DispatchAsync takes (52-09).
                // Nothing is pre-rendered: the surface redraws when ApplyMirrorState lands.
                App.RequestMirrorPollNow?.Invoke();
                PublishDropOutcome(mediaId, track, startUs, "ok", null);
                return true;

            case RudisResultKind.DomainError:
                // SC-3: this is the BACKEND's refusal (run_place_clip's own compatibility
                // check, crates/app-core/src/place.rs:70-74) in the backend's own words.
                // No client-side pre-check produced it and none could have.
                App.LogDiagnostic($"timeline drop place_clip REFUSED: {result.Error} — {json}");
                ShowStatus(TimelineDropTarget.RefusalMessage(result.Error), fatal: false);
                _builder.MarkDirty();
                PublishDropOutcome(mediaId, track, startUs, "refused", result.Error);
                return false;

            default:
                App.LogDiagnostic($"timeline drop place_clip FAULT ({result.Status}): {result.Error} — {json}");
                ShowStatus($"ENGINE FAULT · place_clip · {result.Status}", fatal: true);
                _builder.MarkDirty();
                PublishDropOutcome(mediaId, track, startUs, "fault", result.Error);
                return false;
        }
    }

    /// <summary>Publish the drop's outcome to the Debug-only introspection box the
    /// SC-4 proof polls. Compiled out of Release entirely; see
    /// <c>Debug/TimelineDropIntrospection.cs</c>.</summary>
    private static void PublishDropOutcome(string mediaId, int track, long startUs, string outcome, string? error)
    {
#if DEBUG
        Rudis.Shell.Introspection.TimelineDropIntrospection.Publish(mediaId, track, startUs, outcome, error);
#endif
    }

    // ========================================================================
    // Scrubbing — playback, not an edit
    // ========================================================================

    /// <summary>Take whatever position the ruler drag last asked for and keep exactly
    /// ONE seek in flight, coalescing to the newest value.</summary>
    private void DrainSeek()
    {
        if (!_interaction.TryTakeSeek(out var positionUs))
        {
            return;
        }

        _pendingSeekUs = positionUs;
        if (_seekInFlight)
        {
            return;
        }

        _ = PumpSeekAsync();
    }

    private async Task PumpSeekAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            _pendingSeekUs = null;
            return;
        }

        _seekInFlight = true;
        try
        {
            while (_pendingSeekUs is { } target)
            {
                _pendingSeekUs = null;

                var args = new JsonObject
                {
                    ["cmd"] = new JsonObject
                    {
                        ["type"] = "seek",
                        ["data"] = new JsonObject { ["position_us"] = target },
                    },
                };

                try
                {
                    await engine.TransportAsync(args.ToJsonString());
                }
                catch (Exception ex)
                {
                    // A scrub is not worth a banner: the next drag re-asks, and the
                    // playhead is authoritative from the engine either way.
                    App.LogDiagnostic($"timeline scrub seek faulted: {ex.GetType().Name}: {ex.Message}");
                    return;
                }
            }
        }
        finally
        {
            _seekInFlight = false;
        }
    }

    // ========================================================================
    // Errors — every negative code on a NAMED branch, never swallowed
    // ========================================================================

    /// <summary>
    /// The renderer's six-code contract, handled one branch at a time (T-52-28).
    ///
    /// <para>The 50-06 rule applies: a recoverable transport condition and a
    /// programming fault stay on two DISTINGUISHABLE paths. Collapsing them into
    /// "render failed" would throw away the only diagnostic the ABI gives, and each of
    /// these has a different cause and a different fix.</para>
    /// </summary>
    private void SurfaceRendererFault(TimelineStatus status, string what)
    {
        switch (status)
        {
            case TimelineStatus.SurfaceLost:
                // Recoverable, and PROVEN recoverable: plan 52-04 §6 re-bound a panel to
                // a new surface after a full detach, 2,399 frames at full cadence with
                // zero present errors. Exactly ONE attempt — a loop would turn a dead
                // adapter into a spin.
                if (_reattachSpent)
                {
                    ShowStatus(
                        "The Timeline's GPU surface was lost and could not be re-created. " +
                        "Editing still works; the surface will come back on the next launch.");
                    return;
                }

                _reattachSpent = true;
                App.LogDiagnostic($"timeline: surface lost while {what} — re-attaching once");
                ReleaseHandle();
                AttachSurface();
                return;

            case TimelineStatus.PanicCaught:
                ShowStatus(
                    $"The Timeline renderer caught an internal fault while {what} (PanicCaught). " +
                    "It was contained at the boundary and the renderer is still usable; the panic " +
                    "message is on the engine's stderr.");
                return;

            case TimelineStatus.WrongThread:
                ShowStatus(
                    $"The Timeline renderer was called from the wrong thread while {what} " +
                    "(WrongThread). Attach, resize and detach must run on the UI thread that owns " +
                    "the panel — this is a bug in the shell, not a GPU condition.");
                return;

            case TimelineStatus.BadBuffer:
                // A frame-builder bug. Logged rather than surfaced, and NOT retried with
                // the same frame: the next tick builds a fresh one.
                App.LogDiagnostic($"timeline: malformed frame rejected while {what} (BadBuffer)");
                return;

            case TimelineStatus.NullHandle:
                App.LogDiagnostic($"timeline: call on a released handle while {what} (NullHandle)");
                return;

            default:
                App.LogDiagnostic($"timeline: renderer answered {status} while {what}");
                return;
        }
    }

    /// <summary>
    /// The region's one visible message surface.
    ///
    /// <para><paramref name="fatal"/> switches it to the <c>accent</c>-filled treatment
    /// <c>Transport</c> already uses for the same distinction (50-06): a refused edit
    /// and a transport fault must not look alike, because the first is a normal outcome
    /// of a rule the user tripped and the second is a broken engine. Both colours come
    /// from <c>Theme/Tokens.xaml</c> BY NAME — CLAUDE.md rule 7 — and there is still no
    /// danger token anywhere in the handoff to reach for instead.</para>
    /// </summary>
    private void ShowStatus(string message, bool fatal = false)
    {
        var resources = Application.Current.Resources;
        StatusMessage.Background = (Brush)resources[fatal ? "accent" : "bg-elevated"];
        StatusMessageText.Foreground = (Brush)resources[fatal ? "on-accent" : "text-tertiary"];
        StatusMessageText.Text = message;
        StatusMessage.Visibility = Visibility.Visible;
        EmptyState.Visibility = Visibility.Collapsed;
        App.LogDiagnostic("timeline: " + message);
    }

    private void HideStatus() => StatusMessage.Visibility = Visibility.Collapsed;

    // ========================================================================
    // Teardown
    // ========================================================================

    /// <summary>
    /// Release the surface, on this UI thread, while the window is still alive.
    ///
    /// <para><b>Call this from <c>AppWindow.Closing</c>, never <c>Window.Closed</c></b>
    /// — <c>Closed</c> fires while the window is already being destroyed, which is one
    /// of the two orderings plan 52-01 found by crashing.</para>
    ///
    /// <para>Step 1 of the teardown sequence ("nothing may be presenting") is satisfied
    /// by construction here rather than by joining a thread: this renderer has no
    /// present thread at all — every frame is drawn synchronously from the composition
    /// tick — so clearing <see cref="_attached"/> first is what makes the next tick a
    /// no-op. Steps 2 to 4 happen inside <c>rudis_timeline_detach</c>.</para>
    /// </summary>
    public void DetachSurface()
    {
        if (_detached)
        {
            return;
        }

        _detached = true;
        _attached = false;

#if DEBUG
        _statsTimer?.Stop();
        _statsTimer = null;
#endif

        if (_observedXamlRoot is not null)
        {
            _observedXamlRoot.Changed -= OnXamlRootChanged;
            _observedXamlRoot = null;
        }

        ReleaseHandle();
        _builder.Dispose();

        // The peak cache holds a GCHandle per cached array (plan 52-08). Freeing
        // them here, on the same UI thread and after the surface is gone, is the
        // same discipline the builder's own pins already follow.
        _peaks.Dispose();

        // The strip cache holds one GCHandle per resident sheet (plan 53.2-06), and
        // a sheet is ~5 MiB against a peak array's few KiB — so leaking these is a
        // pinned-memory leak with a visible size, not a bookkeeping one.
        _filmstrips.Dispose();
    }

    private void ReleaseHandle()
    {
        _attached = false;
        var handle = _handle;
        _handle = null;
        handle?.Dispose();
    }

    // ========================================================================
    // Device-lost recovery: reversible suspend/resume (Phase 71, TRUST-01)
    // ========================================================================

    /// <summary>
    /// Release this region's GPU surface for the duration of a preview device-lost
    /// recovery, keeping everything else (model, builder, peaks, filmstrips, palette).
    ///
    /// <para><b>Why this exists.</b> D3D12 devices are singletons per adapter. This
    /// region's wgpu-29 device is the SAME COM object as the preview's wgpu-26 device,
    /// so when the preview's device is removed this one is removed too, and while any
    /// reference to it lives DXGI will not hand the hardware adapter back: the
    /// preview's step 4 would then only ever see WARP, which its LUID re-assert
    /// refuses. 71-05 measured it: releasing preview + background compositor + this
    /// Timeline brought the RTX back 10/10; releasing fewer never did.</para>
    ///
    /// <para><b>UI thread only.</b> <c>rudis_timeline_detach</c> ends in
    /// <c>SetSwapChain(null)</c> on this panel, which is <c>RPC_E_WRONG_THREAD</c>
    /// anywhere else. The preview calls this from its own recovery, on the same
    /// thread.</para>
    ///
    /// <para>A TDR also lands on this region's own device, so its SurfaceLost one-shot
    /// (<see cref="SurfaceRendererFault"/>) may already have fired and re-attached, or
    /// failed to. Either way there is a handle to release or none, and the region is
    /// suspended so <see cref="ResumeSurface"/> re-attaches it. Unlike
    /// <see cref="DetachSurface"/>, nothing is disposed here.</para>
    /// </summary>
    internal void SuspendSurface()
    {
        if (_detached)
        {
            return;
        }

        _suspended = true;

        // AttachSurface subscribes again on resume; unsubscribe so it is never doubled.
        if (_observedXamlRoot is not null)
        {
            _observedXamlRoot.Changed -= OnXamlRootChanged;
            _observedXamlRoot = null;
        }

        var hadHandle = _handle is not null;
        ReleaseHandle();
        LogDeviceRecovery(
            $"timeline: surface SUSPENDED for the preview's device-lost recovery " +
            $"(released a live handle: {hadHandle}); every D3D12 reference must go for the " +
            "hardware adapter to come back");
    }

    /// <summary>
    /// Re-attach the surface after a preview device-lost recovery, on the same panel.
    ///
    /// <para>Runs whether or not the preview recovered: the Timeline is never left
    /// blank. After a success it lands on the recovered hardware adapter; after a
    /// failure it may land on WARP or fail and show its own status line, which is the
    /// detect-and-degrade state.</para>
    ///
    /// <para><see cref="AttachSurface"/> re-uploads the palette, marks the builder
    /// dirty (so the next tick re-uploads the model) and republishes geometry. The
    /// SurfaceLost one-shot is re-armed because the device behind the new handle is a
    /// new one.</para>
    /// </summary>
    internal void ResumeSurface()
    {
        if (!_suspended || _detached)
        {
            return;
        }

        _suspended = false;
        _reattachSpent = false;
        AttachSurface();
        LogDeviceRecovery($"timeline: surface RESUMED after the preview's recovery (attached: {_attached})");
    }

    /// <summary>Recovery transitions go to the diagnostic ring and, when the device-loss
    /// instrumentation is armed (<c>RUDIS_DEBUG_DEVICE_LOSS=1</c>), to stderr too, so a
    /// harness that captures stderr sees the Timeline's half of the sequence next to the
    /// engine's. 71-REVIEW IN-05: the SAME gate <c>Preview.LogRecovery</c> uses, so a
    /// Release build under the harness shows both halves, not only the Preview's.</summary>
    private static void LogDeviceRecovery(string line)
    {
        App.LogDiagnostic(line);
        if (EchoDeviceRecoveryToStderr)
        {
            Console.Error.WriteLine("[rudis] " + line);
            Console.Error.Flush();
        }
    }

    /// <summary>The Preview's <c>PublishDeviceCounters</c> gate, read the same way.</summary>
    private static readonly bool EchoDeviceRecoveryToStderr =
        string.Equals(
            Environment.GetEnvironmentVariable("RUDIS_DEBUG_DEVICE_LOSS"),
            "1",
            StringComparison.Ordinal);

    /// <summary>
    /// A BACKSTOP, not the intended route. <c>Unloaded</c> can fire during window close
    /// — i.e. after the engine has already been disposed — so it must never throw: an
    /// unhandled exception out of an Unloaded handler is a crash dialog on exit, in the
    /// one path nobody exercises interactively.
    /// </summary>
    private void OnSurfaceUnloaded(object sender, RoutedEventArgs e)
    {
        try
        {
            DetachSurface();
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"timeline: detach during unload faulted: {ex.GetType().Name}: {ex.Message}");
        }
    }
}
