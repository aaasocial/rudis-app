using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Rudis.Shell.Interop;
using Rudis.Shell.Mirror;

namespace Rudis.Shell.Regions;

/// <summary>
/// The <c>Preview</c> region (design_handoff_rudis_editor/README.md:33,108 — the name
/// is the handoff's, verbatim, per CLAUDE.md rule 7 / D-19).
///
/// <para><b>This file is the whole C# half of SHELL-04.</b> Everything it does fits in
/// four handlers, and the division of labour is the point:</para>
/// <list type="number">
/// <item><b><see cref="OnPanelLoaded"/> → attach.</b> Hands the engine a COM pointer to
///   <c>PreviewPanel</c>; Rust <c>QueryInterface</c>s it for
///   <c>ISwapChainPanelNative</c>, builds the GPU device, creates the surface, and
///   spawns its own present thread. Runs on the panel's own UI thread, which is a
///   CORRECTNESS requirement — see the remarks on
///   <see cref="RudisNative.AttachPreviewPanel"/>.</item>
/// <item><b><see cref="OnPanelSizeChanged"/> / <see cref="OnPanelCompositionScaleChanged"/>
///   / <see cref="OnXamlRootChanged"/> → forward scalars.</b> C# forwards the
///   notification; RUST owns the resize (D-09). Two owners of one swapchain is exactly
///   the dual-DPI bug class the milestone research named.</item>
/// <item><b><see cref="OnPanelUnloaded"/> → detach.</b> Same UI thread as
///   <c>Loaded</c>, satisfying the affinity guard from the other side.</item>
/// </list>
///
/// <para><b>What this region deliberately does NOT do.</b> It never sees a frame, never
/// runs a clock, never composites, and has no per-frame code of any kind. The present
/// loop lives on Rust's own <c>"rudis-preview-present"</c> thread with Phase 48's
/// <c>"rudis-gpu-decode"</c> thread upstream of it, and the playhead advances because
/// <c>rudis_init</c> was given <c>self_advance</c> (App.xaml.cs). Any managed code on
/// the per-frame path would defeat SHELL-06 by construction (D-08), so there is
/// none — and <see cref="PublishSize"/> is written to allocate zero bytes because it is
/// the one handler a resize storm can call repeatedly.</para>
///
/// <para><b>Letterboxing and contain-fit are the compositor's</b>, not this file's
/// (D-12). The content sub-rect is available as a PULL through
/// <see cref="RudisNative.TryGetPreviewContentRect"/> for the Canvas region's pointer
/// normalization; nothing here re-derives it.</para>
/// </summary>
public sealed partial class Preview : UserControl
{
    /// <summary>
    /// Cadence of the frame-arrival latch (see <see cref="StartFrameArrivalLatch"/>).
    /// Cold path by construction: the timer STOPS the first time a real frame is
    /// observed, so this is a startup poll, not a per-frame one.
    /// </summary>
    private const int FrameArrivalPollMs = 250;

    /// <summary>
    /// Above this, the synchronous attach is worth recording as a FINDING for Phase 55
    /// (a splash / deferred-attach decision), NOT as a defect to fix here. The whole
    /// GPU device build — adapter request, device, compositor, surface, first
    /// configure, placeholder composite — happens inside one call on the UI thread,
    /// and the Phase-44 spike measured that as "a second or two" on this machine.
    /// </summary>
    private const long AttachLatencyFindingThresholdMs = 1500;

    /// <summary>
    /// Cadence of the device-status poll (see <see cref="StartDeviceStatusPoll"/>). COLD
    /// by construction: the export behind it is five relaxed atomic loads that never take
    /// the GPU lock (threat T-63-06), and the same 250 ms the frame-arrival latch above
    /// already established in this region.
    ///
    /// <para>Unlike that latch this one never stops, and it must not: a TDR can arrive at
    /// any instant for the whole life of the process, which is exactly why the defect it
    /// watches for went unnoticed for four phases.</para>
    /// </summary>
    private const int DeviceStatusPollMs = 250;

    /// <summary>
    /// The pause the recovery choreography issues before it touches the surface.
    ///
    /// <para>Shape read from the producer, not guessed, exactly as
    /// <c>Transport.SendAsync</c> records it: <c>rudis_transport</c> takes
    /// <c>TransportArgs { cmd: TransportCmd }</c> and <c>TransportCmd</c> is
    /// ADJACENTLY tagged, so a bare command object is refused. A literal rather than a
    /// built <c>JsonObject</c> because it is CONSTANT -- there is nothing to steer and
    /// nothing to allocate on a path that runs while the GPU is on fire.</para>
    /// </summary>
    private const string PauseCommandJson = "{\"cmd\":{\"type\":\"pause\"}}";

    private readonly DispatcherQueue _dispatcher;

    private DispatcherQueueTimer? _frameLatch;

    // ── plan 63-02 (TRUST-01): the device-lost poll and its recovery choreography ──

    private DispatcherQueueTimer? _deviceStatusPoll;

#if DEBUG
    /// <summary>
    /// The one-shot forced-loss timer, held in a FIELD rather than a local.
    ///
    /// <para>⚠ MEASURED, plan 63-02's first RED run. As a local it fired reliably in a
    /// hand-launched session and NOT ONCE in the UIA harness across a 75 s window — the
    /// harness attaches an automation provider and adds a sampling loop, i.e. exactly the
    /// GC pressure a hand run does not have, and a <c>DispatcherQueueTimer</c> that
    /// nothing managed roots can be collected between <c>Start()</c> and its tick. The
    /// symptom was a test failing on "the loss never happened" while the mechanism it was
    /// testing was fine, which is the worst kind of red. <see cref="_frameLatch"/> and
    /// <see cref="_deviceStatusPoll"/> are fields for the same reason; this one now
    /// matches them.</para>
    /// </summary>
    private DispatcherQueueTimer? _deviceLossInjector;
#endif

    /// <summary>The single-flight guard on the poll itself -- <c>MediaBin.PollOfflineIdsAsync</c>'s
    /// shape verbatim: one call in flight at a time, and a tick that lands while one is
    /// running is DROPPED rather than queued.</summary>
    private bool _deviceStatusInFlight;

    /// <summary>The single-flight guard on the RECOVERY, which is a different guard from
    /// the one above and not redundant with it. The recovery choreography <c>await</c>s a
    /// transport pause before it calls the panel-affine export, and during that await the
    /// UI thread is free and the poll timer CAN tick again -- so without this a second
    /// sequence would start while the first was still deciding (threat T-63-05).</summary>
    private bool _recoveryInFlight;

    /// <summary>Latched when a recovery FAILS, so the shell never retries. The engine
    /// latches its own half (a failed <c>RecoveryPlan</c> leaves <c>recovering</c> engaged
    /// for the session, so the trigger below can never fire again either) -- this is the
    /// managed twin, kept so the region can say so on screen exactly once.</summary>
    private bool _recoveryFailed;

    /// <summary>
    /// Phase 71 (TRUST-01): runs on the UI thread immediately before
    /// <c>rudis_preview_recover_device</c>. MainWindow wires it to
    /// <c>Timeline.SuspendSurface</c>: D3D12 devices are singletons per adapter, so the
    /// Timeline's device is one of the references that must be released before the
    /// preview's step 4 can see the hardware adapter again (71-05, measured).
    /// </summary>
    internal Action? BeforeDeviceRecovery { get; set; }

    /// <summary>
    /// Phase 71 (TRUST-01): runs on the UI thread after the recovery attempt, with
    /// whether it succeeded, whenever <see cref="BeforeDeviceRecovery"/> ran (success,
    /// refusal or throw). MainWindow wires it to <c>Timeline.ResumeSurface</c> so no
    /// region is ever left blank.
    /// </summary>
    internal Action<bool>? AfterDeviceRecovery { get; set; }

    /// <summary>The last device-status line rendered onto the panel's UIA HelpText, so a
    /// poll whose answer did not move rewrites nothing (UI-SPEC §4 rule 1, the same
    /// change-detection discipline <see cref="ApplyMirrorState"/> already applies).</summary>
    private string _renderedDeviceNote = string.Empty;

    /// <summary>
    /// Whether the device counters are published onto UIA at all.
    ///
    /// <para>Read ONCE, from the same environment variable that arms the Rust-side
    /// forced-loss gate, and <see langword="false"/> in every normal run. The counters are
    /// TEST INSTRUMENTATION: <c>presented</c> moves at frame rate, so publishing it on
    /// every poll would put an interpolated string and an automation-property write on the
    /// cold path of a region whose allocation budget is measured
    /// (<see cref="AttachNote"/>'s remarks record the GC regression the last one caused).
    /// The POLL itself is unconditional -- only the publishing is gated.</para>
    /// </summary>
    private static readonly bool PublishDeviceCounters =
        string.Equals(
            Environment.GetEnvironmentVariable("RUDIS_DEBUG_DEVICE_LOSS"),
            "1",
            StringComparison.Ordinal);

    private XamlRoot? _observedXamlRoot;

    private bool _attached;

    private bool _frameSeen;

    /// <summary>
    /// True once the MIRROR has shown this session any media at all. The second half of
    /// <see cref="TryCollapseEmptyState"/>'s condition — see its remarks for the measured
    /// defect (plan 54-08) that made a second half necessary.
    ///
    /// <para>One-way, like <see cref="_frameSeen"/>: it never goes back to
    /// <see langword="false"/> when media is unloaded. That is not an oversight, it is
    /// the SAME recorded limitation the frame-arrival latch already carries (see
    /// <see cref="StartFrameArrivalLatch"/>), kept deliberately identical so the two
    /// signals cannot drift into disagreeing about what "once" means.</para>
    /// </summary>
    private bool _mediaEverPresent;

    /// <summary>True while <see cref="PreviewErrorState"/> is showing a MONITOR-SWITCH
    /// refusal rather than an ATTACH failure. The border is shared, and an attach
    /// failure is permanent and diagnostic — a later successful tab press must never
    /// erase it, or the region would go from "the surface could not start, here is why"
    /// to a blank picture with no explanation.</summary>
    private bool _monitorSwitchErrorShown;

    /// <summary>The last monitor mode this region rendered its tabs for, so a mirror
    /// push that changed nothing relevant does not rewrite two brushes and a string on
    /// every <c>project:changed</c>. Same change-detection discipline
    /// <c>Transport</c>'s cold path already applies (UI-SPEC §4 rule 1).</summary>
    private string _renderedMode = string.Empty;

    /// <summary>The last Source-tab label rendered, for the same reason.</summary>
    private string _renderedSourceLabel = string.Empty;

    // ── the last PUBLISHED geometry. Scalars only: PublishSize compares against
    //    these to stay idempotent, and a WinUI layout pass fires SizeChanged far
    //    more often than the size actually changes. ──
    private uint _lastW;
    private uint _lastH;
    private float _lastScale;

    // ── plan 51-07 / D-15: the READ-ONLY introspection snapshot ─────────────────
    //
    // STATIC, and scalars only, for two reasons that are both about the thing being
    // measured:
    //   (1) `IntrospectionHook` answers from its own background pipe thread and must
    //       never touch the visual tree, so it cannot reach an instance member.
    //   (2) `PublishSize` is gated at ZERO allocated bytes, so whatever it publishes
    //       has to be plain scalar stores — not a boxed snapshot object.
    //
    // Same shape, and the same accepted race, as `Timeline.LastWaveformDiagnostics`:
    // a torn read across these three fields yields a diagnostic that is one tick
    // stale, never a correctness fault, and the whole channel is Debug-only and
    // env-gated OFF by default.
    private static volatile bool _introspectAttached;
    private static uint _introspectLastResizeW;
    private static uint _introspectLastResizeH;

    public Preview()
    {
        InitializeComponent();
        _dispatcher = DispatcherQueue.GetForCurrentThread();

        // Wired here rather than in XAML so all four subscriptions sit next to the
        // remarks that explain why each one exists.
        PreviewPanel.Loaded += OnPanelLoaded;
        PreviewPanel.Unloaded += OnPanelUnloaded;
        PreviewPanel.SizeChanged += OnPanelSizeChanged;
        PreviewPanel.CompositionScaleChanged += OnPanelCompositionScaleChanged;

        // Plan 52-13's monitor switch. Sync handlers starting an async Task that
        // carries its own TOTAL try/catch — the async-void-free way to run work from an
        // event (50-04's pattern; a grep for `async void` over shell/ must stay at zero).
        ProgramTab.Click += (_, _) => _ = SwitchMonitorAsync(PreviewMonitorCommand.Program);
        SourceTab.Click += (_, _) => _ = SwitchMonitorAsync(PreviewMonitorCommand.Source);
    }

    /// <summary>
    /// The measured attach outcome, published ONCE onto
    /// <c>Preview.Surface</c>'s <c>AutomationProperties.HelpText</c> so the real number
    /// is readable through UIA without a debugger — which is how this plan's artifact
    /// records it rather than asserting it.
    ///
    /// <para>⚠ It is deliberately NOT folded into the window's per-tick status readout,
    /// the way <c>Transport.TypographyNote</c> and <c>TitleBar.MechanismNote</c> are.
    /// Doing that reddened Phase 50's
    /// <c>no_gen0_collections_during_sustained_playback_with_uia_attached</c> at delta 2:
    /// that readout rebuilds ten interpolated strings on every mirror notification, and
    /// this value is a CONSTANT after attach. See MainWindow.RefreshStatus's own note.</para>
    /// </summary>
    public string AttachNote { get; private set; } = "(not attached yet)";

    /// <summary>Wall-clock milliseconds <c>rudis_preview_attach_panel</c> took, or
    /// <see langword="null"/> if it has not run. Read by the status block and by plan
    /// 51-04's artifact.</summary>
    public long? AttachLatencyMs { get; private set; }

    /// <summary>True once the engine has confirmed the panel is attached.</summary>
    public bool IsAttached => _attached;

    /// <summary>True once the engine has reported that a composite has actually
    /// happened on the panel — derived from its own content rect, never assumed from
    /// "attach returned Ok". See <see cref="StartFrameArrivalLatch"/> for what this
    /// does and does not claim.</summary>
    public bool HasPresentedFrame => _frameSeen;

    /// <summary>
    /// The <c>Canvas</c> region's ink layer, hosted here as a transparent XAML
    /// sibling above the swapchain (plan 51-05, D-11). Exposed because the XAML
    /// compiler emits <c>x:Name</c> fields as <c>private</c>, and MainWindow — not
    /// this region — owns the Canvas→Preview wiring, so no region reaches across into
    /// another region's tree.
    /// </summary>
    internal PreviewInkLayer Ink => InkLayer;

    /// <summary>
    /// Runs on the panel's own UI thread. That is the structural satisfaction of D-07:
    /// <c>ISwapChainPanelNative::SetSwapChain</c> — reached from inside the FIRST
    /// <c>Surface::configure</c> — returns <c>RPC_E_WRONG_THREAD</c> anywhere else, so
    /// the attach is issued from here and NOT posted to the interop worker queue.
    /// </summary>
    private void OnPanelLoaded(object sender, RoutedEventArgs e)
    {
        // `Loaded` can fire again if the panel is ever re-parented; re-attaching over
        // a live surface is not a supported transition (the ABI answers
        // AlreadyAttached), so guard rather than rely on that refusal.
        if (_attached)
        {
            return;
        }

        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            ShowError(
                "No engine instance: rudis_init returned null, which is a DESIGNED outcome for " +
                "malformed InitConfig JSON. There is nothing to attach the preview surface to.");
            return;
        }

        // Queried INSIDE the handler, never from a cached value: CompositionScaleX/Y
        // are the source of truth and the change event that carries them fires
        // asynchronously with respect to the change itself.
        var scale = PreviewPanel.CompositionScaleX;
        var w = (uint)Math.Max(1, Math.Round(PreviewPanel.ActualWidth * scale));
        var h = (uint)Math.Max(1, Math.Round(PreviewPanel.ActualHeight * scale));

        // The panel's COM pointer, obtained with the idiom the Phase-44 spike PROVED
        // on this machine.
        //
        // ⚠ DO NOT "simplify" this to a `.As<ISwapChainPanelNative>()` WinRT-projection
        // cast: that throws InvalidCastException for exactly this interop (recorded in
        // 44-RESEARCH Pattern 4 and re-confirmed by the spike's own transcript). Rust
        // does the QueryInterface itself, from an IInspectable*, which is why a wrong
        // pointer is a named NotASwapChainPanel status instead of undefined behaviour.
        nint panelPtr;
        try
        {
            panelPtr = WinRT.MarshalInspectable<object>.FromManaged(PreviewPanel);
        }
        catch (Exception ex)
        {
            ShowError($"Could not obtain a COM pointer for the preview panel: {ex.GetType().Name}: {ex.Message}");
            return;
        }

        if (panelPtr == nint.Zero)
        {
            ShowError("MarshalInspectable.FromManaged returned a null pointer for the preview panel.");
            return;
        }

        var stopwatch = Stopwatch.StartNew();
        RudisStatus status;
        try
        {
            status = engine.AttachPreviewPanel(panelPtr, w, h, scale);
        }
        catch (Exception ex)
        {
            stopwatch.Stop();
            Marshal.Release(panelPtr);
            ShowError($"rudis_preview_attach_panel threw: {ex.GetType().Name}: {ex.Message}");
            return;
        }
        stopwatch.Stop();

        // T-51-15: `FromManaged` handed back an AddRef'd pointer and Rust took its OWN
        // reference through its QueryInterface, so this one is ours to drop — on every
        // path, including the throwing one above.
        Marshal.Release(panelPtr);

        AttachLatencyMs = stopwatch.ElapsedMilliseconds;

        if (status != RudisStatus.Ok)
        {
            // NO RETRY LOOP (T-51-16): a permanently-failing attach must degrade to a
            // visible, diagnosable error rather than a spin.
            ShowError(DescribeAttachFailure(status));
            return;
        }

        _attached = true;
        // Attach already published this geometry, so seed the idempotence cache with
        // it — otherwise the first layout-driven SizeChanged would re-publish the
        // identical numbers and set the dirty flag for nothing.
        _lastW = w;
        _lastH = h;
        _lastScale = scale;
        _introspectAttached = true;
        _introspectLastResizeW = w;
        _introspectLastResizeH = h;
        InkLayer.SetCompositionScale(scale);

        // A monitor move can change RasterizationScale WITHOUT firing SizeChanged, so
        // the XamlRoot signal is a genuine third source, not a duplicate of the other
        // two. XamlRoot is non-null by Loaded; it is guarded anyway because a
        // Loaded-before-XamlRoot ordering would otherwise be an unhandled exception on
        // the UI thread.
        _observedXamlRoot = PreviewPanel.XamlRoot;
        if (_observedXamlRoot is not null)
        {
            _observedXamlRoot.Changed += OnXamlRootChanged;
        }

        AttachNote =
            $"attached in {AttachLatencyMs}ms at {w}x{h}px scale {scale:0.###}" +
            (AttachLatencyMs > AttachLatencyFindingThresholdMs
                ? " — FINDING: over 1500ms synchronously on the UI thread (Phase 55: splash / deferred attach)"
                : string.Empty);
        PublishAttachNote();
        App.LogDiagnostic($"preview: {AttachNote}");

        StartFrameArrivalLatch();
        StartDeviceStatusPoll();
#if DEBUG
        StartSimulatedDeviceLossIfRequested();
#endif
    }

    /// <summary>
    /// The layout-driven half of D-09. WinUI fires this on every layout pass that
    /// touches the panel, which is why the work lands in the idempotent
    /// <see cref="PublishSize"/> rather than here.
    /// </summary>
    private void OnPanelSizeChanged(object sender, SizeChangedEventArgs e) => PublishSize();

    /// <summary>
    /// The DPI/monitor-driven half of D-09. <c>CompositionScaleChanged</c> is the
    /// WinUI-native signal for a scale change; the scale is READ from the panel inside
    /// <see cref="PublishSize"/> rather than taken from the sender, because the event
    /// fires asynchronously with respect to the change.
    /// </summary>
    private void OnPanelCompositionScaleChanged(SwapChainPanel sender, object args) => PublishSize();

    /// <summary>A monitor move can change <c>RasterizationScale</c> without changing
    /// the panel's size at all — so this is a real third notification source, routed
    /// to the same one publisher.</summary>
    private void OnXamlRootChanged(XamlRoot sender, XamlRootChangedEventArgs args) => PublishSize();

    /// <summary>
    /// The ONE publisher, fed by all three notifications.
    ///
    /// <para><b>⚠ THIS METHOD MUST ALLOCATE ZERO BYTES</b> (SHELL-06 / D-15 part 2,
    /// gated in plan 51-07). Scalars only: no boxing, no LINQ, no interpolated string,
    /// no logging, no visual-tree touch. Physical pixels and the scale cross the ABI
    /// (D-10) and the call is lock-free on the Rust side — three relaxed stores plus a
    /// release-ordered dirty flag the present thread consumes on its own next tick.</para>
    ///
    /// <para><b>The arithmetic, the idempotence guard and the ABI call live in
    /// <see cref="PreviewGeometryPublisher"/></b>, a non-XAML type, so plan 51-07 can
    /// measure the REAL shipped code instead of a re-typed copy of it — this region's
    /// test host cannot instantiate a <c>SwapChainPanel</c>. What stays here is only
    /// what genuinely needs the panel: three dependency-property reads and one scalar
    /// push into the ink layer. <c>PreviewAllocationTests</c>' source scan fails the
    /// build if anything that allocates is ever added back into this body, where no
    /// measurement could see it — the exact way plan 51-04's measured regression
    /// arrived.</para>
    ///
    /// <para>Both layers clamp, and neither trusts the other (T-51-06): C# floors
    /// width/height at 1 (in the publisher), Rust clamps to <c>1..=16_384</c> and
    /// rejects a non-finite scale.</para>
    /// </summary>
    private void PublishSize()
    {
        if (!_attached)
        {
            return;
        }

        var scale = PreviewPanel.CompositionScaleX;

        // ONE source of truth for the DIP→physical factor (D-10): the ink layer's
        // pointer normalization uses the SAME scalar the publisher sends across the
        // ABI, so a DPI change can never leave the two disagreeing about where the
        // picture is. A field write on a struct-free scalar — no allocation, so the
        // zero-bytes contract above is unaffected.
        InkLayer.SetCompositionScale(scale);

        PreviewGeometryPublisher.Publish(
            App.Engine,
            PreviewPanel.ActualWidth,
            PreviewPanel.ActualHeight,
            scale,
            ref _lastW,
            ref _lastH,
            ref _lastScale);

        _introspectLastResizeW = _lastW;
        _introspectLastResizeH = _lastH;
    }

    /// <summary>
    /// The <c>"preview"</c> introspection request's payload (plan 51-07, D-15).
    /// READ-ONLY by construction: it serialises three scalars this region already
    /// holds plus four relaxed atomic loads through
    /// <see cref="RudisNative.TryGetPreviewContentRect"/>. Nothing here mutates the
    /// mirror, the engine or the panel, and nothing here touches the visual tree —
    /// which matters because <c>IntrospectionHook</c> calls it from its own
    /// background pipe thread (T-51-25; T-50-35's assertion-channel boundary is
    /// preserved, not widened).
    ///
    /// <para><b>Why it exists at all.</b> A "zero garbage collections during
    /// sustained playback" reading taken from a run where the Preview never came up
    /// would be exactly the vacuity D-15 is about — the measurement would be of a
    /// process with no GPU surface, no present thread and nothing to regress. The
    /// GC gate asserts <c>attached</c> and <c>contentW &gt; 0</c> BEFORE its
    /// measurement window opens, so the reading is bracketed by evidence that the
    /// thing being measured was actually running.</para>
    ///
    /// <para><c>contentW</c>/<c>contentH</c> stay <c>0</c> until a composite has
    /// actually happened, which is the ABI's own documented cue and a stronger
    /// signal than "attach returned Ok".</para>
    /// </summary>
    internal static object DescribeForIntrospection()
    {
        var engine = App.Engine;
        var rect = default(RudisPreviewRect);
        var haveRect = engine is not null && engine.TryGetPreviewContentRect(out rect);

        return new
        {
            attached = _introspectAttached,
            contentX = haveRect ? rect.X : 0,
            contentY = haveRect ? rect.Y : 0,
            contentW = haveRect ? rect.Width : 0u,
            contentH = haveRect ? rect.Height : 0u,
            lastResizeW = _introspectLastResizeW,
            lastResizeH = _introspectLastResizeH,
        };
    }

    // ══ plan 52-13: THE MONITOR SWITCH — Program (Timeline) vs Source ══════════
    //
    // v6 PARITY, not a handoff element. See Preview.xaml's own paragraph beside the
    // strip for what this ports (main.ts:717-742) and why it had to be written at all
    // (`set_preview_mode` had zero call sites under shell/ — D4's root cause).

    /// <summary>
    /// The mirror push, mirroring the shape every region since 50-05 uses: the region
    /// NEVER polls the ABI, it is TOLD. Called by <c>MainWindow</c> on both
    /// <c>project:changed</c> and <c>playback:changed</c>, plus once at launch before
    /// any poll has run — both call sites, because the subscription keeps the region
    /// correct AFTER the first poll and the initial push is what makes it correct AT
    /// launch (MainWindow's own recorded note).
    ///
    /// <para>Both events matter here and neither is redundant: which monitor is ACTIVE
    /// and whether a Source clip is loaded are `project` state, while a
    /// <c>load_preview</c> issued by another region lands as an immediate
    /// <c>ApplyPlaybackPayload</c> + <c>NotePreviewMode</c> pair whose only
    /// notification is <c>playback:changed</c>.</para>
    /// </summary>
    internal void ApplyMirrorState(ShellMirror mirror)
    {
        var project = mirror.Project;

        // v6 hides `#tab-source` until a clip is loaded (main.ts:730): a tab that
        // switches to an empty monitor is worse than no tab.
        var sourceMediaId = project?.SourcePlayback.LoadedMediaId;
        var sourceLoaded = !string.IsNullOrEmpty(sourceMediaId);

        var label = "Source";
        if (sourceLoaded && project is not null)
        {
            // `Source · <basename>` — main.ts:731-733, which resolves the id through
            // the media bin and falls back to the bare word when it cannot.
            foreach (var item in project.MediaBin)
            {
                if (item.Id == sourceMediaId)
                {
                    label = "Source · " + System.IO.Path.GetFileName(item.Path);
                    break;
                }
            }
        }

        SourceTab.Visibility = sourceLoaded ? Visibility.Visible : Visibility.Collapsed;
        if (!string.Equals(label, _renderedSourceLabel, StringComparison.Ordinal))
        {
            _renderedSourceLabel = label;
            SourceTab.Content = label;
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(SourceTab, label + " monitor");
        }

        var mode = project?.PreviewMode ?? PreviewMonitorCommand.Program;
        if (!string.Equals(mode, _renderedMode, StringComparison.Ordinal))
        {
            _renderedMode = mode;
            ApplyTabTint(ProgramTab, mode == PreviewMonitorCommand.Program);
            ApplyTabTint(SourceTab, mode == PreviewMonitorCommand.Source);
        }

        // ── plan 54-08: the mirror half of the empty-state condition ──────────────
        //
        // Latched off the state the region has ALREADY read three lines up, so this
        // costs one bool test per mirror change once it is true, and nothing is
        // allocated on either branch (`Count` is O(1); the `foreach` over `List<T>`
        // uses its struct enumerator). The whole guard is skipped once latched.
        if (!_mediaEverPresent && project is not null && HasAnyMedia(project, sourceLoaded))
        {
            _mediaEverPresent = true;
        }

        // The SECOND of two call sites, and the reason is a race — see
        // TryCollapseEmptyState's remarks before considering this redundant.
        TryCollapseEmptyState();
    }

    /// <summary>
    /// Does the mirrored project contain media of any kind?
    ///
    /// <para>Three clauses because the Preview can be fed three ways and any one of
    /// them means the handoff's "Open or import media to preview" is no longer true:
    /// a clip loaded into the SOURCE monitor, an item sitting in the MediaBin, or a
    /// clip on any timeline track feeding the PROGRAM monitor. The union is the
    /// conservative direction here: every extra clause makes the empty state EASIER
    /// to collapse, never harder, so no clause can resurrect the 54-08 red — which
    /// occurs only when all three are false, i.e. on a genuinely empty project.</para>
    /// </summary>
    private static bool HasAnyMedia(Project project, bool sourceLoaded)
    {
        if (sourceLoaded || project.MediaBin.Count > 0)
        {
            return true;
        }

        foreach (var track in project.Timeline.Tracks)
        {
            if (track.Clips.Count > 0)
            {
                return true;
            }
        }

        return false;
    }

    /// <summary>
    /// Collapse <see cref="PreviewEmptyState"/> iff BOTH a composite has happened
    /// (<see cref="_frameSeen"/>) AND the project actually contains media
    /// (<see cref="_mediaEverPresent"/>).
    ///
    /// <para><b>⚠ MEASURED — plan 54-08, and the two conditions are not redundant.</b>
    /// Until 54-08 this collapse was unconditional on the frame signal alone, and
    /// <c>UiaSmokeTests.disabled_and_empty_states_are_visible_to_uia</c> failed
    /// <b>3 out of 3</b> runs with <i>"`Preview.EmptyState` is absent with NO media
    /// loaded"</i>. The recorded diagnostic launch (no arguments at all) read
    /// <c>mirror: media=0 clips=0</c> and, on the same launch,
    /// <c>preview: first composite observed — content rect <b>435x435px</b></c> — a
    /// 1:1 square, which is exactly the placeholder signature
    /// <see cref="StartFrameArrivalLatch"/> already documents from plan 51-04.
    /// <c>Preview.ErrorState</c> was absent and the attach took 207 ms, so nothing had
    /// failed: the engine simply composites its placeholder into an attached panel
    /// whether or not there is media, and the latch pinned on it.</para>
    ///
    /// <para><b>The defect was a conflated predicate.</b> The latch answers "has a
    /// composite happened?"; the collapse needs "is there something to show?". Those
    /// agree on every <c>--import</c> launch Phase 51 tested and come apart on the
    /// empty launch — which is the app's FIRST-RUN state, so the one user guaranteed
    /// to see it was the one who most needed the instruction. With no media the
    /// placeholder is the ONLY composite that will ever arrive, so the empty state
    /// stayed collapsed for the whole session.</para>
    ///
    /// <para><b>⚠ DO NOT "simplify" this to a single call site.</b> It is called from
    /// <see cref="OnFrameArrivalTick"/> AND from <see cref="ApplyMirrorState"/> because
    /// the two signals RACE: with <c>--import</c>, media can reach the mirror either
    /// before or after the first composite, and whichever arrives SECOND is the one
    /// that must perform the collapse. Deleting either call reintroduces the bug in
    /// one of the two orderings — and the ordering it would break is the one
    /// <c>PreviewSurfaceTests</c> exercises, so it would land as a NEW red rather than
    /// as the old one.</para>
    ///
    /// <para>Idempotent, and cheap enough to call on every mirror change: the
    /// <see cref="UIElement.Visibility"/> test short-circuits before any assignment.</para>
    /// </summary>
    private void TryCollapseEmptyState()
    {
        if (!_frameSeen || !_mediaEverPresent)
        {
            return;
        }

        if (PreviewEmptyState.Visibility != Visibility.Collapsed)
        {
            PreviewEmptyState.Visibility = Visibility.Collapsed;
        }
    }

    /// <summary>
    /// The active/inactive treatment, from tokens only. Same shape as
    /// <c>Transport.ApplyLoopTint</c>: a mirror-derived tint cannot be a XAML visual
    /// state, because "which monitor is active" is backend state rather than pointer
    /// state.
    /// </summary>
    private static void ApplyTabTint(Button tab, bool active)
    {
        var resources = Application.Current.Resources;
        tab.Foreground = (Brush)resources[active ? "accent" : "text-secondary"];
        tab.Background = active
            ? (Brush)resources["bg-segment-active"]
            : new SolidColorBrush(Microsoft.UI.Colors.Transparent);
    }

    /// <summary>
    /// The REAL <c>rudis_transport set_preview_mode</c>, and the immediate-apply that
    /// follows it — frontend parity (<c>main.ts:575-591</c>), the same two lines
    /// <c>MediaBin.LoadIntoSourcePreviewAsync</c> and <c>Toolbar.ImportPathsAsync</c>
    /// already run.
    ///
    /// <para><c>NotePreviewMode</c> comes BEFORE <c>ApplyPlaybackPayload</c> and the
    /// order is load-bearing — the rationale is written once, at
    /// <c>MediaBin.xaml.cs:481</c>, and is not restated here: applying second would
    /// file the Program playback into the Source slot.</para>
    ///
    /// <para>⚠ Both are issued only AFTER the transport call has come back <c>Ok</c>,
    /// which is <c>MediaBin.LoadIntoSourcePreviewAsync</c>'s shape verbatim rather
    /// than a note-then-send. Noting the mode first would leave the mirror claiming a
    /// monitor the backend refused — self-healing on the next full resync, but in the
    /// meantime routing every playback payload into the wrong slot and moving the
    /// wrong playhead. The pair is atomic on the UI thread: there is no <c>await</c>
    /// between them, so no poll can interleave.</para>
    ///
    /// <para>The two failure layers stay SEPARATE, exactly as
    /// <c>Transport.SendAsync</c> documents: a domain <c>{"Err": ..}</c> is a
    /// user-visible OUTCOME, and a <see cref="RudisStatus"/> other than <c>Ok</c> is a
    /// transport FAULT. Collapsing them would throw away the distinction the ABI was
    /// designed around. Neither is swallowed — a backend refusal must be VISIBLE
    /// (UI-SPEC §5, the rule this region already applies to attach failures).</para>
    /// </summary>
    private async Task SwitchMonitorAsync(string mode)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        RudisResult<System.Text.Json.JsonElement> result;
        try
        {
            result = await engine.TransportAsync(PreviewMonitorCommand.SetMode(mode));
        }
        catch (Exception ex)
        {
            ShowMonitorSwitchError(
                $"Couldn't switch the preview monitor: rudis_transport threw " +
                $"{ex.GetType().Name}: {ex.Message}");
            return;
        }

        switch (result.Kind)
        {
            case RudisResultKind.Ok:
                App.Mirror?.NotePreviewMode(mode);
                App.Mirror?.ApplyPlaybackPayload(JsonNode.Parse(result.Value.GetRawText()));
                ClearMonitorSwitchError();
                App.LogDiagnostic($"preview: set_preview_mode -> {mode}");
                if (App.Mirror is not null)
                {
                    ApplyMirrorState(App.Mirror);
                }

                break;

            case RudisResultKind.DomainError:
                // A user-visible OUTCOME: the backend's own rules refused it.
                ShowMonitorSwitchError(
                    $"The engine refused the {mode} monitor: {result.Error}");
                break;

            default:
                // A transport FAULT: PanicCaught (-99), InvalidHandle, … — a different
                // class of problem entirely, and reported as one.
                ShowMonitorSwitchError(
                    $"Couldn't switch to the {mode} monitor — transport fault " +
                    $"{result.Status}: {result.Error}");
                break;
        }
    }

    /// <summary>
    /// A monitor-switch refusal, made visible in the region's own error border.
    ///
    /// <para>Deliberately NOT <see cref="ShowError"/>: that one rewrites
    /// <see cref="AttachNote"/> to "attach FAILED — …" and collapses the empty state,
    /// both of which would be false claims about a surface that attached perfectly
    /// well and merely declined a tab press.</para>
    /// </summary>
    private void ShowMonitorSwitchError(string message)
    {
        _monitorSwitchErrorShown = true;
        PublishErrorText(message);
        PreviewErrorState.Visibility = Visibility.Visible;
        App.LogDiagnostic("preview: " + message);
    }

    /// <summary>
    /// Put the failure text where BOTH a sighted user and an assistive technology can
    /// reach it.
    ///
    /// <para>⚠ MEASURED, plan 63-02. <c>Preview.ErrorState</c> carries a STATIC
    /// <c>AutomationProperties.Name</c> of <c>"Preview error state"</c> in the XAML, which
    /// overrides a <c>TextBlock</c>'s normal "the Name is the text" behaviour — so every
    /// failure this region has ever shown has been announced to a screen reader as the
    /// three words "Preview error state" and nothing else. The device-loss proof found it
    /// by reading the element and getting the placeholder back where the message should
    /// have been.</para>
    ///
    /// <para>UI-SPEC §5's rule is that a backend refusal must be VISIBLE; a message that
    /// only sighted users can read is half of that. Setting the Name alongside the Text is
    /// the whole fix, and it belongs in the one place both error paths already funnel
    /// through.</para>
    /// </summary>
    private void PublishErrorText(string message)
    {
        PreviewErrorStateText.Text = message;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(PreviewErrorStateText, message);
    }

    /// <summary>Clear ONLY a monitor-switch message. An attach failure stays on screen
    /// forever, because it is still true.</summary>
    private void ClearMonitorSwitchError()
    {
        if (!_monitorSwitchErrorShown)
        {
            return;
        }

        _monitorSwitchErrorShown = false;
        PreviewErrorStateText.Text = string.Empty;
        // Back to the XAML placeholder, so a cleared border does not keep announcing a
        // failure that is no longer true.
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(
            PreviewErrorStateText, "Preview error state");
        PreviewErrorState.Visibility = Visibility.Collapsed;
    }

    /// <summary>
    /// Same UI thread as <see cref="OnPanelLoaded"/>, which is what satisfies the
    /// affinity guard from the other side (Rust records the attaching thread id and
    /// answers <c>WrongThread</c> to anyone else).
    ///
    /// <para>NEVER throws: <c>Unloaded</c> can fire during window close, i.e. AFTER
    /// <c>App.Engine.Dispose()</c> has run (MainWindow.OnWindowClosed), and an
    /// unhandled exception out of an <c>Unloaded</c> handler is a crash dialog on exit
    /// in the one path nobody exercises interactively. The wrapper's own
    /// teardown-race guard covers the disposed case and returns
    /// <c>InvalidHandle</c>.</para>
    /// </summary>
    private void OnPanelUnloaded(object sender, RoutedEventArgs e)
    {
        _frameLatch?.Stop();
        _frameLatch = null;
        _deviceStatusPoll?.Stop();
        _deviceStatusPoll = null;

        if (_observedXamlRoot is not null)
        {
            _observedXamlRoot.Changed -= OnXamlRootChanged;
            _observedXamlRoot = null;
        }

        if (!_attached)
        {
            return;
        }
        _attached = false;
        _introspectAttached = false;

        var status = App.Engine?.DetachPreviewPanel() ?? RudisStatus.InvalidHandle;
        if (status != RudisStatus.Ok)
        {
            App.LogDiagnostic($"preview: rudis_preview_detach_panel returned {status}");
        }
    }

    /// <summary>
    /// Observe, once, that the engine reports a COMPOSITE HAS ACTUALLY HAPPENED on the
    /// panel — not that attach merely returned <c>Ok</c>.
    ///
    /// <para>⚠ This latch is ONE of the two conditions on the empty state, not the
    /// whole of it: as of plan 54-08 the collapse also requires that the project
    /// actually contains media. See <see cref="TryCollapseEmptyState"/> for the
    /// measured 3/3 red that made the second condition necessary — the paragraph below
    /// about the placeholder is exactly what that red turned out to be.</para>
    ///
    /// <para>The signal is the engine's own content rect, which the present path
    /// publishes from the SAME <c>engine::contain_fit_viewport</c> call it letterboxes
    /// with, and which the ABI documents as staying <c>0x0</c> "until a composite has
    /// actually happened — the caller's cue that there is nothing to align to yet".
    /// That is a stronger signal than an API return value and a weaker one than "real
    /// decoded video", and this says so rather than overclaiming.</para>
    ///
    /// <para>⚠ MEASURED, and stated because the difference matters: the FIRST
    /// composite observed after attach is typically the engine's own placeholder
    /// (plan 51-04's runs saw a 1x1-aspect content rect on the first tick), not the
    /// first decoded frame. So the empty state can collapse a beat before real video
    /// appears. That is the right trade for a region whose alternative is showing
    /// "Open or import media to preview" ON TOP of a live picture — and the claim
    /// that a REAL frame reaches the panel is proven where it can be proven, in
    /// pixels, by <c>PreviewSurfaceTests.preview_panel_shows_a_real_decoded_frame</c>.
    /// Distinguishing a placeholder from a genuinely square video is not something
    /// the content rect can express.</para>
    ///
    /// <para>The timer STOPS on the first observation: this is a startup latch, not a
    /// per-frame poll, so it adds nothing to the steady-state path. ⚠ RECORDED
    /// LIMITATION: the latch does not re-show the empty state if media is later
    /// unloaded, because nothing resets the content rect once published — claiming
    /// otherwise would be claiming behaviour the ABI does not provide.
    /// <b>Plan 54-08 did NOT change this</b>, and deliberately: <c>_mediaEverPresent</c>
    /// is one-way for the same reason <c>_frameSeen</c> is, so the fix makes the
    /// collapse harder to trigger and never easier. Unload-then-re-show remains
    /// unimplemented and is still stated here rather than quietly upgraded.</para>
    /// </summary>
    private void StartFrameArrivalLatch()
    {
        if (_frameSeen || _frameLatch is not null)
        {
            return;
        }

        _frameLatch = _dispatcher.CreateTimer();
        _frameLatch.Interval = TimeSpan.FromMilliseconds(FrameArrivalPollMs);
        _frameLatch.IsRepeating = true;
        _frameLatch.Tick += (_, _) => OnFrameArrivalTick();
        _frameLatch.Start();
    }

    private void OnFrameArrivalTick()
    {
        if (_frameSeen)
        {
            return;
        }

        var engine = App.Engine;
        if (engine is null || !engine.TryGetPreviewContentRect(out var rect))
        {
            return;
        }
        if (rect.Width == 0 || rect.Height == 0)
        {
            return;
        }

        _frameSeen = true;
        _frameLatch?.Stop();
        _frameLatch = null;

        // The FIRST of two call sites. ⚠ NOT an unconditional collapse any more —
        // a composite is not by itself evidence that there is anything to show
        // (plan 54-08's 3/3 red; the reasoning is written once, at
        // TryCollapseEmptyState, and is not restated here).
        TryCollapseEmptyState();

        // Once, on the cold path, onto the element it describes.
        AttachNote += $" · first composite {rect.Width}x{rect.Height}px at ({rect.X},{rect.Y})";
        PublishAttachNote();

        App.LogDiagnostic(
            $"preview: first composite observed — content rect {rect.Width}x{rect.Height}px " +
            $"at ({rect.X},{rect.Y}) inside the panel (may be the engine's placeholder; " +
            "the first DECODED frame follows)");
    }

    // ══ plan 63-02: TRUST-01, the shell half ═══════════════════════════════════
    //
    // A real GPU driver TDR removes the D3D12 device the preview surface is built on.
    // Detection was restored at the device-birth site by quick 260829-n96 and the
    // coordinated six-step recreation got its first production caller in plan 63-01 --
    // but only at the engine tier, through the debug-surface twin. In the RUNNING APP
    // nothing consumed either: `git grep device_lost shell/` was EMPTY, so a TDR left the
    // preview dead until the user restarted Rudis. This block is the consumer.
    //
    // The shape is POLL, not push: `ring::EVENT_NAMES` stays at 6 (63-CONTEXT D-10), and
    // this is the sixth rider on that pattern.

    /// <summary>
    /// Start the device-status poll. Cold, single-flight, and it never stops while the
    /// panel is attached -- see <see cref="DeviceStatusPollMs"/>.
    /// </summary>
    private void StartDeviceStatusPoll()
    {
        if (_deviceStatusPoll is not null)
        {
            return;
        }

        _deviceStatusPoll = _dispatcher.CreateTimer();
        _deviceStatusPoll.Interval = TimeSpan.FromMilliseconds(DeviceStatusPollMs);
        _deviceStatusPoll.IsRepeating = true;
        _deviceStatusPoll.Tick += (_, _) => _ = PollDeviceStatusAsync();
        _deviceStatusPoll.Start();
    }

    /// <summary>
    /// One poll of <c>rudis_preview_device_status</c>, and the ONE place the recovery
    /// trigger lives.
    ///
    /// <para><b>Single-flight, conditional re-act, failure leaves previous state</b> --
    /// <c>MediaBin.PollOfflineIdsAsync</c>'s three properties, followed rather than
    /// re-derived. A tick that lands while a call is in flight is dropped; a status that
    /// did not move rewrites nothing; a refusal or a transport fault is LOGGED and
    /// changes nothing, because a poll that could not read the device's health has not
    /// learned that the device is healthy.</para>
    ///
    /// <para><b>The trigger is <c>lost &amp;&amp; !recovering</c>, and both halves are
    /// load-bearing.</b> <c>recovering</c> is <see langword="true"/> while a sequence is
    /// running AND for the rest of the session after one has FAILED (the engine's own
    /// fail-closed convention), so this can never become a teardown/attach retry storm
    /// against half-recreated state (threat T-63-05).</para>
    ///
    /// <para>Total try/catch, and an <c>async Task</c> started from a sync handler rather
    /// than an <c>async void</c> -- 50-04's pattern; a grep for <c>async void</c> over
    /// <c>shell/</c> must stay at zero.</para>
    /// </summary>
    private async Task PollDeviceStatusAsync()
    {
        if (_deviceStatusInFlight || _recoveryInFlight)
        {
            return;
        }

        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        _deviceStatusInFlight = true;
        try
        {
            var result = await engine.GetPreviewDeviceStatusAsync();
            if (result.Kind != RudisResultKind.Ok)
            {
                // Fail to SILENCE, never to a fabricated verdict: a fault here means the
                // health of the device is UNKNOWN, which is not the same as healthy and
                // is certainly not a reason to tear a live surface down.
                return;
            }

            var status = result.Value;
            var lost = ReadBool(status, "lost");
            var recovering = ReadBool(status, "recovering");
            var recovered = ReadUInt64(status, "recovered");
            var presented = ReadUInt64(status, "presented");
            var epoch = ReadUInt64(status, "attach_epoch");

            if (PublishDeviceCounters)
            {
                var note =
                    $"device lost={lost} recovering={recovering} recovered={recovered} " +
                    $"presented={presented} epoch={epoch}";
                if (!string.Equals(note, _renderedDeviceNote, StringComparison.Ordinal))
                {
                    _renderedDeviceNote = note;
                    Microsoft.UI.Xaml.Automation.AutomationProperties.SetHelpText(
                        PreviewPanel, AttachNote + " · " + note);
                }
            }

            if (lost && !recovering && !_recoveryFailed)
            {
                await RecoverFromDeviceLossAsync();
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"preview: device-status poll threw {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            _deviceStatusInFlight = false;
        }
    }

    /// <summary>
    /// The recovery choreography, on the UI thread that owns the panel.
    ///
    /// <para><b>Three steps, and the order is the whole design.</b></para>
    /// <list type="number">
    /// <item><b>Pause the transport.</b> Not politeness -- correctness. The ring producer
    ///   holds an owned <c>Arc</c> clone of the compositor for its whole life, and while
    ///   ANY handle to a removed D3D12 device lives, DXGI hides the hardware adapter from
    ///   fresh enumeration in this process: recovery step 4 would then recreate on WARP,
    ///   and its own same-adapter LUID re-assert would refuse that -- turning a
    ///   recoverable TDR into a hard failure AFTER the old device had been torn down. The
    ///   producer exits when playback stops, and the engine's step 1 waits for exactly
    ///   that (and refuses, before releasing anything, if it does not happen).</item>
    /// <item><b>Marshal a FRESH COM pointer for the same panel.</b> The reference Rust
    ///   held died with the surface in step 3, so this is a new one, obtained with the
    ///   same idiom <see cref="OnPanelLoaded"/> uses and released the same way
    ///   (T-51-15).</item>
    /// <item><b>2.5 — release every other D3D12 reference in the process (Timeline
    ///   surface) before the engine's pre-flight enumerates</b> (Phase 71, TRUST-01).
    ///   <see cref="BeforeDeviceRecovery"/> suspends the Timeline's surface on this
    ///   thread; the export itself pauses/cancels the render cache and releases its
    ///   background compositor. D3D12 devices are singletons per adapter, and the
    ///   adapter only comes back once the LAST reference is gone (71-05, measured).
    ///   <see cref="AfterDeviceRecovery"/> resumes the Timeline afterwards, on success or
    ///   failure.</item>
    /// <item><b>Call the panel-affine export, synchronously, from here.</b> Step 4 ends in
    ///   a FIRST <c>Surface::configure</c>, i.e. in
    ///   <c>ISwapChainPanelNative::SetSwapChain</c>, which returns
    ///   <c>RPC_E_WRONG_THREAD</c> anywhere but this thread. It blocks for the duration
    ///   -- a preview that is already dead is not made worse by a second of stall, and
    ///   there is no other thread that may legally do this work.</item>
    /// </list>
    ///
    /// <para><b>Resumes PAUSED, deliberately.</b> The engine's coordinator has no
    /// play path at all, and this method does not add one: a preview that silently
    /// resumed playing after a GPU fault would be presenting frames the user did not ask
    /// for, from a position they did not choose.</para>
    /// </summary>
    private async Task RecoverFromDeviceLossAsync()
    {
        if (_recoveryInFlight || _recoveryFailed)
        {
            return;
        }

        var engine = App.Engine;
        if (engine is null || engine.IsInvalid || !_attached)
        {
            return;
        }

        _recoveryInFlight = true;
        var stopwatch = Stopwatch.StartNew();
        var beforeRan = false;
        var recoveredOk = false;
        try
        {
            LogRecovery(
                "preview: device loss observed on the live surface — pausing the transport and " +
                "running the coordinated recovery (TRUST-01)");

            // 1. Pause. A refusal is logged, never swallowed, and never fatal to the
            //    attempt: the engine's step 1 is the real gate, and it will refuse if the
            //    producer is still holding the dead compositor.
            try
            {
                var paused = await engine.TransportAsync(PauseCommandJson);
                if (paused.Kind != RudisResultKind.Ok)
                {
                    App.LogDiagnostic(
                        $"preview: pause before recovery answered {paused.Kind} ({paused.Status}): " +
                        $"{paused.Error}");
                }
            }
            catch (Exception ex)
            {
                App.LogDiagnostic($"preview: pause before recovery threw {ex.GetType().Name}: {ex.Message}");
            }

            // 2. A fresh pointer for the SAME panel, at its CURRENT geometry (a monitor or
            //    DPI change during the dead window would otherwise rebuild at a stale size).
            var scale = PreviewPanel.CompositionScaleX;
            var w = (uint)Math.Max(1, Math.Round(PreviewPanel.ActualWidth * scale));
            var h = (uint)Math.Max(1, Math.Round(PreviewPanel.ActualHeight * scale));

            nint panelPtr;
            try
            {
                panelPtr = WinRT.MarshalInspectable<object>.FromManaged(PreviewPanel);
            }
            catch (Exception ex)
            {
                FailRecovery($"could not obtain a COM pointer for the panel: {ex.GetType().Name}: {ex.Message}");
                return;
            }

            if (panelPtr == nint.Zero)
            {
                FailRecovery("MarshalInspectable.FromManaged returned a null pointer for the panel.");
                return;
            }

            // 2.5 Release every other D3D12 reference the SHELL holds (Phase 71, TRUST-01):
            //     the Timeline's surface. D3D12 devices are singletons per adapter, so its
            //     wgpu-29 device is the same removed object, and the hardware adapter only
            //     comes back once every reference is gone (71-05, measured). Same UI
            //     thread: its detach ends in SetSwapChain(null) on its own panel. The
            //     engine releases its own holders (render cache) inside the export.
            beforeRan = true;
            try
            {
                BeforeDeviceRecovery?.Invoke();
            }
            catch (Exception ex)
            {
                LogRecovery($"preview: BeforeDeviceRecovery threw {ex.GetType().Name}: {ex.Message} (continuing)");
            }

            // 3. The sequence itself.
            RudisStatus status;
            try
            {
                status = engine.RecoverPreviewDevice(panelPtr, w, h, scale);
            }
            catch (Exception ex)
            {
                Marshal.Release(panelPtr);
                FailRecovery($"rudis_preview_recover_device threw: {ex.GetType().Name}: {ex.Message}");
                return;
            }

            Marshal.Release(panelPtr);
            stopwatch.Stop();

            if (status != RudisStatus.Ok)
            {
                FailRecovery(
                    $"the engine answered {status} after {stopwatch.ElapsedMilliseconds}ms. " +
                    DescribeAttachFailure(status));
                return;
            }

            recoveredOk = true;
            ClearMonitorSwitchError();
            LogRecovery(
                $"preview: RECOVERED from a device loss in {stopwatch.ElapsedMilliseconds}ms — the " +
                "surface was rebuilt on the same panel and the transport stays paused. No restart.");
#if DEBUG
            RearmSimulatedDeviceLossAfterRecovery();
#endif
        }
        catch (Exception ex)
        {
            FailRecovery($"unexpected {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            // Resume whatever 2.5 released, on success AND on failure, so no region is
            // left blank (on failure the resume shows its own status line if it cannot
            // attach — the detect-and-degrade state).
            if (beforeRan)
            {
                try
                {
                    AfterDeviceRecovery?.Invoke(recoveredOk);
                }
                catch (Exception ex)
                {
                    LogRecovery($"preview: AfterDeviceRecovery threw {ex.GetType().Name}: {ex.Message}");
                }
            }

            _recoveryInFlight = false;
        }
    }

    /// <summary>Recovery transitions go to the diagnostic ring and, when the device-loss
    /// instrumentation is armed, to stderr too, so a harness that captures stderr reads
    /// the shell's half of the sequence beside the engine's.</summary>
    private static void LogRecovery(string line)
    {
        App.LogDiagnostic(line);
        if (PublishDeviceCounters)
        {
            Console.Error.WriteLine("[rudis] " + line);
            Console.Error.Flush();
        }
    }

    /// <summary>
    /// A failed recovery, made visible and LATCHED.
    ///
    /// <para>No retry (T-51-16's rule, and threat T-63-05's mitigation): a recovery that
    /// failed left the surface torn down, and repeating it would be a teardown/attach
    /// storm against half-recreated state. The engine latches its own guard for the same
    /// reason; this is the half the user can see.</para>
    /// </summary>
    private void FailRecovery(string detail)
    {
        _recoveryFailed = true;
        LogRecovery("preview: RECOVERY FAILED (latched): " + detail);
        ShowError(
            "The graphics device was lost and the preview could not be restarted. " +
            "Saving and reopening the project, or restarting Rudis, will restore it. (" +
            detail + ")");
    }

    private static bool ReadBool(System.Text.Json.JsonElement obj, string name) =>
        obj.ValueKind == System.Text.Json.JsonValueKind.Object
        && obj.TryGetProperty(name, out var v)
        && v.ValueKind == System.Text.Json.JsonValueKind.True;

    private static ulong ReadUInt64(System.Text.Json.JsonElement obj, string name) =>
        obj.ValueKind == System.Text.Json.JsonValueKind.Object
        && obj.TryGetProperty(name, out var v)
        && v.ValueKind == System.Text.Json.JsonValueKind.Number
        && v.TryGetUInt64(out var n)
            ? n
            : 0UL;

#if DEBUG
    /// <summary>
    /// <c>--simulate-device-lost-after-ms N</c>: N ms after attach, really remove the live
    /// D3D12 device (see <c>App.SimulateDeviceLostAfterMs</c> for the three independent
    /// gates on this, and why a harness that waits for a real TDR is not a harness).
    ///
    /// <para>One-shot: the timer stops itself on its first tick. The injection is issued
    /// through the ordinary async wrapper, so a refusal comes back as a DOMAIN error and
    /// is logged rather than thrown -- which is what a run with the flag set but
    /// <c>RUDIS_DEBUG_DEVICE_LOSS</c> unset must look like.</para>
    /// </summary>
    private void StartSimulatedDeviceLossIfRequested()
    {
        var delayMs = App.SimulateDeviceLostAfterMs;
        if (delayMs <= 0)
        {
            return;
        }

        _deviceLossInjector = _dispatcher.CreateTimer();
        _deviceLossInjector.Interval = TimeSpan.FromMilliseconds(delayMs);
        _deviceLossInjector.IsRepeating = false;
        _deviceLossInjector.Tick += (t, _) =>
        {
            t.Stop();
            _ = InjectDeviceLossAsync();
        };
        _deviceLossInjector.Start();
        _simulatedLossesRemaining = Math.Max(0, SimulatedLossCount() - 1);
        App.LogDiagnostic($"preview: --simulate-device-lost-after-ms {delayMs} armed for this launch");
    }

    /// <summary>Further forced losses still to inject after successful recoveries
    /// (Phase 71). 0 = one-shot, the 63-02 behaviour.</summary>
    private int _simulatedLossesRemaining;

    /// <summary>
    /// <c>RUDIS_DEBUG_DEVICE_LOSS_COUNT=N</c> (Debug only, Phase 71): how many forced
    /// losses one process sees in total. Read only when the injector is armed, which
    /// already needs <c>--simulate-device-lost-after-ms</c> AND
    /// <c>RUDIS_DEBUG_DEVICE_LOSS=1</c>. Clamped to 1..10; anything unparseable is 1.
    /// A reference that leaks across one recovery only shows on the NEXT loss, so the
    /// UIA proof runs several in one process (71-05 hand-off).
    /// </summary>
    private static int SimulatedLossCount() =>
        int.TryParse(Environment.GetEnvironmentVariable("RUDIS_DEBUG_DEVICE_LOSS_COUNT"), out var n)
            ? Math.Clamp(n, 1, 10)
            : 1;

    /// <summary>After a SUCCESSFUL recovery, re-arm the forced-loss timer if more losses
    /// were requested. The same field-held timer (the GC finding above), the same delay
    /// measured from the recovery.</summary>
    private void RearmSimulatedDeviceLossAfterRecovery()
    {
        if (_deviceLossInjector is null || _simulatedLossesRemaining <= 0)
        {
            return;
        }

        _simulatedLossesRemaining--;
        _deviceLossInjector.Start();
        LogRecovery(
            $"preview: forced device loss re-armed ({App.SimulateDeviceLostAfterMs} ms; " +
            $"{_simulatedLossesRemaining} more after this one)");
    }

    private static async Task InjectDeviceLossAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        try
        {
            var result = await engine.SimulatePreviewDeviceLostAsync();
            App.LogDiagnostic(
                result.Kind == RudisResultKind.Ok
                    ? "preview: forced device removal INJECTED on the live preview device"
                    : $"preview: forced device removal refused ({result.Kind}/{result.Status}): {result.Error}");
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"preview: forced device removal threw {ex.GetType().Name}: {ex.Message}");
        }
    }
#endif

    /// <summary>
    /// A named status becomes a human sentence. The three that can realistically
    /// appear are enumerated explicitly because each has a DIFFERENT cause and a
    /// different fix, and collapsing them into "attach failed" would throw away the
    /// only diagnostic the ABI gives (these exports carry no envelope).
    /// </summary>
    private static string DescribeAttachFailure(RudisStatus status) => status switch
    {
        RudisStatus.NotASwapChainPanel =>
            "Couldn't start the preview: the engine rejected the panel pointer " +
            "(NotASwapChainPanel — QueryInterface for ISwapChainPanelNative returned E_NOINTERFACE). " +
            "The pointer handed over is not this panel.",
        RudisStatus.WrongThread =>
            "Couldn't start the preview: the attach reached the engine on the wrong thread " +
            "(WrongThread). It must run on the UI thread that owns the panel — from Loaded, " +
            "synchronously, never through the interop worker queue.",
        RudisStatus.SurfaceCreateFailed =>
            "Couldn't start the preview: the GPU surface could not be created " +
            "(SurfaceCreateFailed). The failing stage and its HRESULT are on the engine's stderr.",
        RudisStatus.AlreadyAttached =>
            "Couldn't start the preview: a panel is already attached (AlreadyAttached). " +
            "Detach before attaching another.",
        RudisStatus.InvalidHandle =>
            "Couldn't start the preview: no live engine instance (InvalidHandle).",
        RudisStatus.NullPointer =>
            "Couldn't start the preview: a null panel pointer reached the engine (NullPointer).",
        RudisStatus.PanicCaught =>
            "Couldn't start the preview: the engine caught a panic during attach (PanicCaught). " +
            "The panic message is on the engine's stderr.",
        _ => "Couldn't start the preview: the engine answered " + status + ".",
    };

    /// <summary>
    /// Make a failure VISIBLE and diagnosable, in the region itself, and log it. The
    /// empty state collapses because "open or import media" would be misleading advice
    /// for a surface that cannot start at all.
    /// </summary>
    private void ShowError(string message)
    {
        AttachNote = "attach FAILED — " + message;
        PublishAttachNote();
        App.LogDiagnostic("preview: " + AttachNote);
        PreviewEmptyState.Visibility = Visibility.Collapsed;
        PublishErrorText(message);
        PreviewErrorState.Visibility = Visibility.Visible;
    }

    /// <summary>
    /// Publish <see cref="AttachNote"/> onto the surface's own
    /// <c>AutomationProperties.HelpText</c>: read by
    /// <c>PreviewSurfaceTests</c>, which writes the number into this plan's artifact.
    ///
    /// <para>Written ONCE per attach (or failure), on the element the note is ABOUT —
    /// not appended to a string the window rebuilds a hundred times a second. That is
    /// not a stylistic preference: see <see cref="AttachNote"/>'s remarks for the
    /// measured GC regression the other shape caused.</para>
    /// </summary>
    private void PublishAttachNote()
    {
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetHelpText(PreviewPanel, AttachNote);
        // The frame-arrival latch appends to this later, so the accessible name stays
        // the region's while the measurement rides HelpText.
    }
}
