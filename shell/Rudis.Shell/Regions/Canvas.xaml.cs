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
/// The <c>Canvas</c> region (design_handoff_rudis_editor/README.md:33,96 — the name
/// is the handoff's, verbatim, per CLAUDE.md rule 7 / D-19).
///
/// <para><b>TWO real ink surfaces, driven by ONE tool palette.</b></para>
/// <list type="bullet">
/// <item>The palette's selected tool drives <see cref="PreviewInkLayer"/>, the
///   transparent XAML sibling above the Preview region's <c>SwapChainPanel</c>,
///   where a drag becomes a genuine undoable <c>AddAnnotation</c> in the backend
///   (D-11/D-12). Those marks carry the frame-linked space and the ENGINE
///   composites them onto the video.</item>
/// <item>The dotted-grid STAGE in this region is now the SECOND ink surface
///   (plan 60.2-02). A drag on it becomes an equally real, equally undoable
///   <c>AddAnnotation</c> carrying <see cref="CanvasStageGesture.WhiteboardSpace"/>,
///   built by the SAME <see cref="CanvasGesture.BuildAnnotation"/> /
///   <see cref="CanvasGesture.BuildDispatchArgs"/> pair the Preview uses — which
///   has taken <c>space</c> as a parameter since Phase 51 and needed no edit.</item>
/// </list>
/// <para>Selecting <see cref="DrawTool.Pointer"/> makes BOTH surfaces inert.</para>
///
/// <para><b>⚠ THIS CLASS DOC RETIRES THE PHASE 51 SCOPE NOTE IT REPLACES.</b> The
/// stage was previously an honest EMPTY state, deliberately not a drawing surface,
/// because this region's ink was scoped to the Preview swapchain. That boundary is
/// now lifted, not patched around: the stage draws, commits and re-renders from
/// state, with ZERO video loaded and ZERO composite ever having happened.</para>
///
/// <para><b>Committed whiteboard marks are drawn HERE, from the MIRROR.</b> Plan
/// 51-02's engine overlay is refreshed exclusively from the frame-linked marks
/// (T-51-13), so nothing else in the product would ever draw a whiteboard one —
/// which is why <see cref="ApplyMirrorState"/> exists and why D-13's trail hand-off
/// is coherent on this surface at all. It is also what CLAUDE.md rule 4 prescribes:
/// the renderer is a read-only mirror of backend-owned state, so an Undo visibly
/// removes a mark and a Redo restores it with no stage-local retained state.</para>
///
/// <para><b>The two boundaries that remain, recorded rather than smuggled</b> (the
/// same pair Canvas.xaml's header states, and plan 60.2-02's SUMMARY):</para>
/// <list type="number">
/// <item><b>The zoom chip stays an explicitly-labelled INERT placeholder.</b> Canvas
///   pan/zoom was offered to the owner on 2026-08-27 and DECLINED; there is no
///   viewport transform anywhere in this region and the stage box is simply the
///   region's own bounds. CLAUDE.md rule 1 permits a labelled placeholder where a
///   fake operation is not — the standard <c>Inspector.Placeholder</c> already
///   meets.</item>
/// <item><b>The agent's whiteboard raster is sized from the PREVIEW's aspect</b>
///   (<c>whiteboard_raster_dims</c>, crates/ffi/src/ctx.rs), not from this stage's,
///   so stage marks reach the agent positionally correct in normalized space but
///   anisotropically stretched wherever the two aspects differ. An ACCEPTED,
///   RECORDED limitation of Phase 60.2 (deferred-items D-1).</item>
/// </list>
/// </summary>
public sealed partial class Canvas : UserControl
{
    /// <summary>Dot pitch of the stage grid, in DIPs.</summary>
    private const double DotPitch = 24.0;

    /// <summary>Dot radius, in DIPs. Small enough that the grid reads as texture
    /// rather than as content.</summary>
    private const double DotRadius = 1.0;

    /// <summary>
    /// Ceiling on the number of dots built for one layout. The stage lives in the
    /// handoff's FIXED 486px column, so the real count is in the hundreds — this is a
    /// guard against a pathological layout pass (a transient enormous ActualHeight
    /// during window restore) turning a decorative backdrop into a hang.
    /// </summary>
    private const int MaxDots = 4096;

    private int _dotColumns = -1;
    private int _dotRows = -1;

    public Canvas()
    {
        InitializeComponent();
        _dispatcher = DispatcherQueue.GetForCurrentThread();

        // ONE code path to the stage's cursor, even though `Pointer` is the launch
        // default and a null cursor is already what a fresh layer has. Stating the
        // start state costs nothing and means a later change to the default tool
        // cannot leave the pointer disagreeing with the palette.
        ApplyStageCursor();
    }

    /// <summary>The tool the palette currently has selected. <c>Pointer</c> at
    /// startup, matching the handoff's default and v6.0's.
    ///
    /// <para><c>internal</c> because <see cref="DrawTool"/> is — the annotation
    /// client is shell-internal by design (D-12: the wire contract lives in
    /// <c>rudis_core</c>, and nothing here is a public API for anyone else).</para>
    /// </summary>
    internal DrawTool SelectedTool { get; private set; } = DrawTool.Pointer;

    /// <summary>Raised when the palette selection changes. MainWindow forwards it to
    /// the Preview region's ink layer — the Canvas region never reaches across to
    /// another region itself.</summary>
    internal event Action<DrawTool>? ToolChanged;

    private void OnToolChecked(object sender, RoutedEventArgs e)
    {
        if (sender is not RadioButton button)
        {
            return;
        }

        var tool = button.Name switch
        {
            nameof(PenTool) => DrawTool.Pen,
            nameof(LassoTool) => DrawTool.Lasso,
            nameof(TextTool) => DrawTool.Text,
            nameof(ShapeTool) => DrawTool.Shape,
            _ => DrawTool.Pointer,
        };

        if (tool == SelectedTool)
        {
            return;
        }

        SelectedTool = tool;
        ApplyStageCursor();

        // A half-drawn pen stroke must not commit under lasso rules — the same
        // rule PreviewInkLayer.SelectedTool documents. Abandoned BEFORE the event
        // is raised, so no listener can observe a tool change while this region
        // still believes a gesture of the OLD tool is in flight.
        if (_gestureActive)
        {
            AbandonStageGesture("tool changed mid-gesture");
        }

        App.LogDiagnostic($"canvas: tool -> {tool}");
        ToolChanged?.Invoke(tool);
    }

    // ── the stage's pointer affordance ─────────────────────────────────────────
    //
    // D-60.2-12 / 60.2-HUMAN-UAT gap G-1. Phase 60.2 made this stage genuinely
    // drawable and PROVED it on real project state; the owner then ran check 1 on
    // that same build and answered "No — still looks inert", reproducing his
    // original "I can't draw on the canvas section" report against a surface whose
    // stroke fidelity he had just passed in the same session. Nothing was wrong with
    // the state: with no cursor cue, the first feedback that the surface is drawable
    // arrived only after he had already committed to a drag.
    //
    // ⚠ THE CURSOR GOES ON `StageCursorLayer`, NEVER ON THIS CLASS OR ON
    // `CanvasRoot`. WinUI resolves the cursor by walking UP from the hit-test target
    // to the first ancestor with a non-null one, and an element's default is "use my
    // parent's", not "use the arrow" — so a cursor set here would show the crosshair
    // over the tool palette and every other piece of chrome as well, none of which
    // sets a cursor of its own. The layer's z-order BELOW that chrome (Canvas.xaml)
    // is what keeps the arrow there, structurally. `InkCursorWiringGateTests` pins
    // that rule by scanning this file for the protected UIElement cursor member and
    // requiring ZERO hits — which is also why the member is not spelled out anywhere
    // in this file, comments included; the only reach to a cursor here is through
    // CursorHost's `Cursor` wrapper. `CanvasCursorUiaTests` measures the real OS
    // cursor handle on the running shell.
    //
    // ⚠ AND Z-ORDER IS ONLY HALF THE CONTRACT (review WR-01) — see
    // OnChromePointerPressed below. Z-order decides which cursor is SHOWN; it does
    // NOT decide what a press DOES, and this fix first shipped with two elements where
    // those two disagreed: over the region tag and over the palette's own frame,
    // padding and inter-tile gaps the ARROW showed while the press bubbled on to
    // CanvasRoot and drew a real annotation anyway. (The zoom chip was suspected of
    // the opposite failure and MEASURED CLEAN — a disabled Control still takes the hit
    // and still consumes the press on WinAppSDK 1.8; Canvas.xaml records the reading.)

    /// <summary>The stage's ink cursor, created ONCE. <c>InputSystemCursor.Create</c>
    /// allocates, and the tool can change as often as the user clicks the palette —
    /// there is no reason to mint a new cursor per click. <c>Cross</c> because there
    /// is no system pen cursor and a custom cursor resource is out of proportion to
    /// this fix; it is the standard drawing crosshair, and it is what
    /// <see cref="PreviewInkLayer"/> uses too, so the two ink surfaces read as one
    /// feature.</summary>
    private static readonly InputCursor InkCursor =
        InputSystemCursor.Create(InputSystemCursorShape.Cross);

    /// <summary>Point the stage's cursor at whatever the palette currently says.
    /// <see langword="null"/> means "inherit", and with no ancestor setting a cursor
    /// that is the system default arrow — the honest look for a surface that, with
    /// <see cref="DrawTool.Pointer"/> selected, will refuse the press before any
    /// allocation.</summary>
    private void ApplyStageCursor()
        => StageCursorLayer.Cursor = InkCursorGate.WantsInkCursor(SelectedTool) ? InkCursor : null;

    // ── chrome that shows the arrow must SWALLOW the press ─────────────────────
    //
    // THE PRINCIPLE, stated once because it is the whole of review WR-01: THE CURSOR
    // AND THE PRESS MUST AGREE, EVERYWHERE. A pointer shape is a promise about what a
    // click will do, and the two are decided by DIFFERENT mechanisms here — the cursor
    // by z-order (which element is the hit-test target, and what its ancestor chain
    // says), the press by whether that element marks PointerPressed handled. Getting
    // one right and leaving the other to chance is what shipped, and it produced the
    // exact defect InkCursorGate's own doc names as the mirror image of the bug this
    // whole task exists to fix: an ARROW over the region tag and over the palette's
    // frame, padding and inter-tile gaps, while the press bubbled on to CanvasRoot and
    // started a real, undoable AddAnnotation.
    //
    // So the decision is made PER ELEMENT, and each element carries its own one-line
    // justification in Canvas.xaml:
    //
    //   · DECORATION that must not interrupt a stroke crossing it — `Canvas.RegionTag`
    //     — is made TRANSPARENT TO INPUT (IsHitTestVisible="False"). The hit falls
    //     through to StageCursorLayer, so it shows the CROSSHAIR and it DRAWS. Agreed.
    //   · REAL CHROME the user aims at whose container does not already consume the
    //     press — `ToolPalette` — keeps the ARROW and SWALLOWS the press through this
    //     handler. Agreed.
    //   · Two elements need NOTHING, and that was measured rather than assumed: the
    //     five RadioButton tiles (ButtonBase already marks the press handled — which is
    //     why the ONE chrome reading CanvasCursorUiaTests used to take was the single
    //     place the "no leak" claim cost nothing) and the DISABLED zoom chip, which
    //     reads as the arrow and swallows the press exactly like them.
    //
    // No element is left undecided, and no decision here rests on doctrine —
    // `CanvasCursorUiaTests` now reads BOTH HALVES (the OS cursor handle AND whether
    // Project.canvas.annotations changed) at the region tag, at the palette's own
    // padding and at the zoom chip, because a cursor assertion alone is what let this
    // through in the first place.

    /// <summary>Swallow a press on Canvas chrome that shows the default arrow, so the
    /// pointer's promise and the region's behaviour cannot disagree. Wired in markup
    /// from the CONTAINER (the palette Border) rather than from each child, because it
    /// is the container's own pixels — frame, padding, inter-child gaps — that
    /// hit-test to it and that leaked; a bubbling handler there also covers anything
    /// added inside it later.
    ///
    /// <para>Marking it handled is the entire body: <c>CanvasRoot</c>'s
    /// <c>PointerPressed</c> is wired in markup, and WinUI does not deliver a handled
    /// routed event to a markup-wired handler, so no gesture starts, no pointer is
    /// captured and nothing allocates.</para></summary>
    private void OnChromePointerPressed(object sender, PointerRoutedEventArgs e)
        => e.Handled = true;

    // ── the stage's ink surface ────────────────────────────────────────────────
    //
    // Canvas.xaml wires the five handlers below in MARKUP, on `CanvasRoot` itself
    // (research Pattern 1). The ink host above it is IsHitTestVisible="False" from
    // top to bottom, so nothing the stage draws can eat the pointer that is drawing
    // it — and, unlike the Preview's layer, no transparent-sibling or
    // background-toggling machinery is needed, because nothing BENEATH the stage
    // wants the input in the first place.
    //
    // ⚠ EVERY DECISION LIVES IN THE WinUI-FREE HALF. This file sequences events,
    // renders and dispatches; `CanvasStageGesture` and `CanvasGesture` decide. That
    // split is not taste: `Rudis.Shell.Tests.csproj` compiles an explicit allow-list
    // and never a WinUI-bearing region file, so a rule written HERE would be
    // permanently unreachable from the unit tier whatever its access modifier said.

    /// <summary>How long the finished trail may stay on screen waiting for the
    /// mirror to confirm the mark landed, before it is cleared REGARDLESS. The
    /// Preview ink layer's budget, for the Preview ink layer's reason: a stuck trail
    /// is worse than a flicker, and an unbounded wait on a visual element is not
    /// acceptable at any latency. When it fires it is LOGGED, never swallowed.</summary>
    private const int MirrorConfirmTimeoutMs = 750;

    /// <summary>Confirmation poll cadence, deliberately finer than the shell's 100 ms
    /// cold poll so the observed latency is the mirror's rather than this timer's.</summary>
    private const int MirrorConfirmPollMs = 25;

    private readonly DispatcherQueue _dispatcher;

    /// <summary>The gesture's NORMALIZED points — the authoritative list. The trail
    /// is rendered FROM this (never the other way round), so what is drawn and what
    /// is dispatched cannot diverge and a resize can re-place the trail exactly.</summary>
    private readonly List<NormPoint> _points = new(CanvasGesture.MaxPoints);

    private bool _gestureActive;

    /// <summary>Which gesture owns <see cref="_points"/>, the trail and
    /// <see cref="_confirmTimer"/> right now. Every commit captures the token it was
    /// launched under and re-checks it before touching any of the three — see
    /// <see cref="StageGestureSequence"/> for the interleaving that made it necessary
    /// (review WR-01), and <see cref="ClearStageTrailIfCurrent"/> for the check.</summary>
    private readonly StageGestureSequence _gestures = new();

    /// <summary>True between "the trail was frozen at pointer-up" and "the trail was
    /// cleared". While set, <see cref="RenderStageTrail"/> keeps drawing the finished
    /// stroke — that is the whole hand-off design.</summary>
    private bool _trailFrozen;

    /// <summary>The stage box the CURRENT gesture is normalized against, latched at
    /// pointer-down from <c>CanvasRoot</c>'s own <c>ActualWidth</c>/
    /// <c>ActualHeight</c> — DIPs, from the same layout pass the pointer event comes
    /// from. There is no scale factor, no offset and no engine call anywhere in this
    /// path: a whiteboard mark has nothing to align to and must be drawable with no
    /// media loaded at all.</summary>
    private double _boxWidth;

    private double _boxHeight;

    private DispatcherQueueTimer? _confirmTimer;

    // ── pointer ────────────────────────────────────────────────────────────────

    private void OnStagePointerPressed(object sender, PointerRoutedEventArgs e)
    {
        // The ENTIRE Pointer gate, and all it needs to be: an early return before
        // any allocation. No hit-test toggling, because nothing beneath the stage
        // needs the pass-through the Preview's layer has to arrange.
        //
        // It stays FIRST — ahead of the button gate below — so the inert tool keeps
        // costing nothing at all: `GetCurrentPoint` allocates a PointerPoint, and the
        // Pointer tool must not pay for it on every press across the region.
        if (SelectedTool == DrawTool.Pointer)
        {
            return;
        }

        // ⚠ AND THE BUTTON (review WR-02). WinUI raises PointerPressed for ANY
        // button, so before this gate a RIGHT-click or a MIDDLE-click on the Canvas
        // region with Pen selected began a real gesture and committed a real,
        // undoable annotation on release. `IsLeftButtonPressed` is the PRIMARY-contact
        // question, not a mouse-only one: WinUI reports it true for a touch contact
        // and a pen tip too, so pen and touch drawing are unaffected. The whole rule
        // lives in the WinUI-free half, where the unit tier can reach it.
        var down = e.GetCurrentPoint(CanvasRoot);
        if (!CanvasStageGesture.ShouldStartGesture(SelectedTool, down.Properties.IsLeftButtonPressed))
        {
            return;
        }

        _boxWidth = CanvasRoot.ActualWidth;
        _boxHeight = CanvasRoot.ActualHeight;
        if (_boxWidth <= 0 || _boxHeight <= 0)
        {
            // A never-measured or collapsed stage: every point would normalize to
            // the origin, and a stroke of 500 identical points is a REAL annotation
            // the user never drew. Refusing is the honest answer.
            App.LogDiagnostic("canvas stage: gesture refused — zero-size stage box");
            return;
        }

        CanvasRoot.CapturePointer(e.Pointer);

        // ⚠ The PREVIOUS commit's confirm timer must die HERE, before the new
        // gesture's points start accumulating. Its tick calls ClearStageTrail(),
        // which clears `_points` — so a second stroke begun inside the 750 ms
        // confirmation window would have its own accumulated points wiped mid-drag
        // and would commit a TRUNCATED shape (or, on a wedged backend, none at all).
        // Stopping it costs nothing: the mark it was waiting on is drawn by
        // ApplyMirrorState from mirror state, not by the trail it was holding.
        StopStageConfirmTimer();

        // ⚠ AND THE TOKEN, for the HALF THE STOP ABOVE CANNOT REACH (review WR-01).
        // A commit that is still IN FLIGHT has no timer to stop yet: it is parked on
        // `await DispatchCommandAsync` on the shared interop worker, and when it
        // resumes it will clear the trail — and therefore `_points` — out from under
        // the gesture starting right here. Minting a new token now makes every clear
        // site in that continuation a no-op for it. See StageGestureSequence.
        _gestures.Begin();

        _points.Clear();
        _gestureActive = true;
        _trailFrozen = false;

        AppendFromStagePoint(down.Position);
        RenderStageTrail();
        e.Handled = true;
    }

    /// <summary>
    /// ⚠ The early return comes FIRST, before the coalesced-history read below,
    /// which allocates a list on every call. `CanvasRoot` is hit-testable for all of
    /// the region's life, so this handler fires on every hover sample across the
    /// Canvas — the same ordering SHELL-06 pins for the Preview's ink layer, written
    /// right here the first time even though no allocation scanner covers this file.
    /// </summary>
    private void OnStagePointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (CanvasGesture.ShouldIgnoreMove(_gestureActive))
        {
            return;
        }

        // The FULL coalesced history since the last event — points the OS batched
        // that a per-event read would silently drop. The collection is documented
        // most-recent-FIRST, so it is walked BACKWARDS to append chronologically;
        // getting that backwards draws every stroke as a zig-zag.
        var coalesced = e.GetIntermediatePoints(CanvasRoot);
        if (coalesced is { Count: > 0 })
        {
            for (var i = coalesced.Count - 1; i >= 0; i--)
            {
                AppendFromStagePoint(coalesced[i].Position);
            }
        }

        // ⚠ AND THEN THE CURRENT POSITION, ALWAYS — not as an `else`. Plan 51-10's
        // MEASURED fix: with synthetic input the coalesced collection came back
        // non-empty on every move with every entry carrying the pointer-DOWN
        // position, i.e. a zero-length polyline, i.e. a trail that renders nothing
        // while every assertion stays green. `AccumulateDistinctPoint` bounds the
        // cost of doing both and de-duplicates the overlap.
        AppendFromStagePoint(e.GetCurrentPoint(CanvasRoot).Position);

        RenderStageTrail();
        e.Handled = true;
    }

    private void OnStagePointerReleased(object sender, PointerRoutedEventArgs e)
    {
        if (!_gestureActive)
        {
            return;
        }

        // The release point is part of the stroke, so the FROZEN trail must include
        // it — otherwise the held trail is one sample short of what was dispatched.
        AppendFromStagePoint(e.GetCurrentPoint(CanvasRoot).Position);
        RenderStageTrail();
        CanvasRoot.ReleasePointerCapture(e.Pointer);
        FinishStageGesture();
        e.Handled = true;
    }

    /// <summary>Capture lost / cancelled (another element took the pointer, the
    /// window lost activation, a touch was cancelled). The gesture is FINISHED, not
    /// abandoned: the user drew a real mark, and lifting the pointer outside the
    /// element is a legitimate way to end it.</summary>
    private void OnStagePointerCaptureLost(object sender, PointerRoutedEventArgs e)
    {
        if (!_gestureActive)
        {
            return;
        }

        FinishStageGesture();
    }

    // ── the commit ─────────────────────────────────────────────────────────────

    private void FinishStageGesture()
    {
        // ⚠ IDEMPOTENT, AND THAT IS A MEASURED FIX, NOT A DEFENSIVE HABIT
        // (plan 60.2-04, on the running app). `OnStagePointerReleased` calls
        // `CanvasRoot.ReleasePointerCapture(e.Pointer)` and THEN calls this method
        // unconditionally — and WinUI raises `PointerCaptureLost` from inside that
        // release call, synchronously, while `_gestureActive` is still true. So the
        // capture-lost handler finished the gesture first and this call finished it
        // AGAIN: one real drag on the stage dispatched TWO `add_annotation` commands
        // carrying the SAME accumulated points, and the running shell answered
        // `{"annotationCount":2}` with two whiteboard strokes for one stroke drawn.
        //
        // That is not a cosmetic double-draw. It puts a phantom annotation in the
        // user's project, it makes ONE Undo look like it did nothing (it removes the
        // duplicate, and the identical mark underneath stays on screen), and the
        // duplicate reaches the agent's whiteboard raster too. It was invisible to
        // every pixel-based test by construction, because two identical dashed
        // strokes in the same place look exactly like one — which is the same
        // property `CommitStageAsync`'s remarks rely on for the hand-off.
        //
        // The guard lives HERE rather than at the call site because this method is
        // the single funnel both endings go through, and "the gesture ends now" is
        // an event that can legitimately arrive twice.
        if (!_gestureActive)
        {
            return;
        }

        _gestureActive = false;

        var tool = SelectedTool;

        // ⚠ A TAP IS NOT A STROKE (review WR-02), and this is the gate that says so.
        //
        // A single click with Pen selected accumulates exactly ONE point — the down
        // and up positions are identical, so AccumulateDistinctPoint de-duplicates
        // them — and `CanvasGesture.BuildAnnotation`'s Pen arm accepts one point, so
        // the backend was handed a real one-point `stroke` and correctly stored it.
        // Nothing on this surface then DREW it: RenderStageTrail needs two points and
        // AddPolylineFigure needs two points. The user saw nothing while the project
        // gained an undoable annotation that the `canvas` channel counts, the agent's
        // whiteboard raster receives and every saved `.rud` carries.
        //
        // BEFORE `_trailFrozen`, deliberately: there is no hand-off to sequence for a
        // mark that is never committed, and freezing a trail that is about to be
        // cleared would leave `_trailFrozen` briefly lying about what is on screen.
        if (!CanvasStageGesture.IsCommittableGesture(tool, _points.Count))
        {
            ClearStageTrail();
            return;
        }

        // ⚠ THE TRAIL IS *NOT* CLEARED HERE. It is FROZEN — it stops growing and
        // stays visible. See CommitStageAsync.
        _trailFrozen = true;

        var points = _points.ToArray();

        // The token this commit is launched under. Captured HERE — synchronously,
        // while this gesture is still the current one — because everything after the
        // first `await` inside CommitStageAsync may run when it no longer is.
        _ = CommitStageAsync(tool, points, _gestures.Current);
    }

    /// <summary>
    /// <b>THE HAND-OFF SEQUENCE (D-13), STAGE EDITION.</b>
    /// <code>
    /// pointer-up
    ///   -> freeze the trail (stop appending; KEEP IT VISIBLE)
    ///   -> dispatch add_annotation with the whiteboard space
    ///   -> await the dispatch's own outcome
    ///   -> wait for the mirror to report the annotation present in
    ///      Project.canvas.annotations
    ///   -> THEN clear the trail — by which time ApplyMirrorState has already
    ///      drawn the COMMITTED mark from that same mirror state
    /// </code>
    ///
    /// <para><b>Why this order, stated so a later "simplification" is recognisably a
    /// regression.</b> The naive sequence clears the trail at pointer-up. Between
    /// that (immediate) and the committed layer being redrawn, NEITHER draws the
    /// stroke — a visible gap. Holding the trail until the mirror confirms converts
    /// that gap into a brief double-draw of identical geometry: same points, same
    /// box, same dashed <c>accent</c> ink, because Canvas.xaml declares the two Paths
    /// identically on purpose. Drawing the same dashed stroke twice in the same place
    /// is indistinguishable from drawing it once; drawing it ZERO times is a flicker
    /// the eye catches immediately.</para>
    ///
    /// <para><b>On the Preview the second drawer is the ENGINE. Here it cannot
    /// be:</b> whiteboard marks never composite into the preview (T-51-13). Without
    /// this region's own committed layer every stroke would VANISH about a second
    /// after being drawn — exactly the surface-that-silently-discards-strokes the
    /// Phase 51 scope note refused to ship.</para>
    ///
    /// <para>Bounded at <see cref="MirrorConfirmTimeoutMs"/> ms by a
    /// <see cref="DispatcherQueueTimer"/> — NEVER a blocking wait, which on the UI
    /// thread would deadlock against the very continuation it is waiting for.</para>
    ///
    /// <para><b>⚠ EVERY LINE BELOW THE FIRST <c>await</c> MAY RUN UNDER A DIFFERENT
    /// GESTURE.</b> The dispatch rides the shared interop worker, which an export, an
    /// import or an agent turn can already occupy, so this task can resume long after
    /// the user has begun the NEXT stroke. That is why <paramref name="token"/> exists
    /// and why every clear site here goes through
    /// <see cref="ClearStageTrailIfCurrent"/> rather than
    /// <see cref="ClearStageTrail"/>: the trail and <c>_points</c> are SHARED, and
    /// clearing them from a superseded commit truncates a stroke in progress
    /// (review WR-01, <see cref="StageGestureSequence"/>).</para>
    /// </summary>
    private async Task CommitStageAsync(DrawTool tool, NormPoint[] points, int token)
    {
        try
        {
            // ⚠ THE CHEAPEST REFUSAL COMES FIRST (review IN-04). This check used to
            // sit BELOW the label dialog, so with no engine the user typed a label
            // into a modal, pressed Add, and watched the result go to a diagnostic
            // log. Refusing before any UI is shown is the same ordering the Rust half
            // this file keeps citing applies ("step 1 … before the store is even
            // locked", ctx.rs) — and the only ordering that cannot waste the user's
            // input.
            var engine = App.Engine;
            if (engine is null || engine.IsInvalid)
            {
                App.LogDiagnostic("canvas stage: no engine instance — the mark was NOT committed");
                ClearStageTrailIfCurrent(token);
                return;
            }

            string? text = null;
            if (tool == DrawTool.Text)
            {
                text = await PromptForStageLabelAsync();
            }

            var shape = CanvasGesture.BuildAnnotation(tool, points, text);
            if (shape is null)
            {
                // A structurally invalid gesture (a 2-point lasso, an empty stroke,
                // a cancelled label). The backend would refuse it; the client
                // refuses first so a DOOMED command is never dispatched.
                ClearStageTrailIfCurrent(token);
                return;
            }

            // v6.0's id shape, character for character: `a-${crypto.randomUUID().slice(0, 8)}`.
            var id = "a-" + Guid.NewGuid().ToString("N")[..8];

            // ⚠ `linked` is null UNCONDITIONALLY, and that is not an omission. The
            // server force-nulls the linked range for every whiteboard mark
            // (command.rs:1693-1697), so computing one here would be dead client
            // work, and adding a client-side branch "to match the domain rule" would
            // duplicate a rule that is already enforced exactly once, server-side.
            var args = CanvasGesture.BuildDispatchArgs(id, shape, null, CanvasStageGesture.WhiteboardSpace);

            var outcome = await engine.DispatchCommandAsync(args.ToJsonString());
            if (outcome.Kind != RudisResultKind.Ok)
            {
                // A backend refusal must be VISIBLE, never swallowed. The trail goes
                // because nothing was committed and holding it would imply otherwise.
                App.LogDiagnostic(
                    $"canvas stage: commit {id} REFUSED: {outcome.Kind}/{outcome.Status} — {outcome.Error}");
                ClearStageTrailIfCurrent(token);
                return;
            }

            AwaitStageMirrorThenClear(id, token);
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"canvas stage: commit faulted: {ex.GetType().Name}: {ex.Message}");
            ClearStageTrailIfCurrent(token);
        }
    }

    /// <summary>
    /// The bounded half of the hand-off: hold the frozen trail until the mirror has
    /// the annotation, then clear. Polls the mirror rather than subscribing to
    /// <c>ProjectChanged</c> because the mirror IS what the patch-apply updates —
    /// polling observes the same fact one dispatcher turn later at worst, with no
    /// subscription to unhook on every teardown path.
    /// </summary>
    private void AwaitStageMirrorThenClear(string id, int token)
    {
        if (!_gestures.IsCurrent(token))
        {
            // ⚠ BEFORE the stop below, not after. A LATER gesture owns the trail,
            // `_points` AND `_confirmTimer` now, and this superseded commit must
            // touch none of the three — stopping "the" confirm timer here would kill
            // the NEWER commit's hold, and clearing the trail would wipe a stroke
            // that is still being drawn (review WR-01). Standing down costs this
            // mark nothing: ApplyMirrorState draws it from MIRROR state, never from
            // the trail it was holding.
            return;
        }

        StopStageConfirmTimer();

        if (MirrorHasAnnotation(id))
        {
            ClearStageTrail();
            return;
        }

        var started = Environment.TickCount64;
        var timer = _dispatcher.CreateTimer();
        _confirmTimer = timer;
        timer.Interval = TimeSpan.FromMilliseconds(MirrorConfirmPollMs);
        timer.IsRepeating = true;
        timer.Tick += (_, _) =>
        {
            // ⚠ THE SAME QUESTION THE CONTINUATION ASKS, ASKED AGAIN PER TICK. A tick
            // already QUEUED on the dispatcher still runs after `Stop()` — so
            // pointer-down's StopStageConfirmTimer() does not, on its own, guarantee
            // this body never executes under a newer gesture.
            if (!_gestures.IsCurrent(token))
            {
                StopStageConfirmTimer(timer);
                return;
            }

            var elapsed = Environment.TickCount64 - started;
            if (MirrorHasAnnotation(id))
            {
                StopStageConfirmTimer(timer);
                ClearStageTrail();
                return;
            }

            if (elapsed >= MirrorConfirmTimeoutMs)
            {
                StopStageConfirmTimer(timer);
                App.LogDiagnostic(
                    $"canvas stage: commit {id} NOT mirror-confirmed within {MirrorConfirmTimeoutMs}ms " +
                    "— trail cleared anyway");
                ClearStageTrail();
            }
        };
        timer.Start();
    }

    /// <summary>Stop whatever confirm timer is CURRENT. Called from the synchronous
    /// gesture-lifetime sites (pointer-down, abandon, the start of a new wait), where
    /// "current" is exactly what is meant.</summary>
    private void StopStageConfirmTimer()
    {
        _confirmTimer?.Stop();
        _confirmTimer = null;
    }

    /// <summary>
    /// Stop <b>THIS</b> timer — the one whose tick is running — and release the field
    /// only if it still points at it.
    ///
    /// <para>A tick must never call the parameterless overload above: by the time a
    /// queued tick runs, <see cref="_confirmTimer"/> may already hold a NEWER commit's
    /// timer, and stopping that one would end the newer mark's hold while leaving this
    /// (stopped-but-still-referenced) one in the field (review WR-01, second
    /// site).</para>
    /// </summary>
    private void StopStageConfirmTimer(DispatcherQueueTimer timer)
    {
        timer.Stop();
        if (ReferenceEquals(_confirmTimer, timer))
        {
            _confirmTimer = null;
        }
    }

    /// <summary>
    /// Is the committed mark visible in the shell's mirror of
    /// <c>Project.canvas.annotations</c>?
    ///
    /// <para>Read from the mirror's RAW node rather than a typed projection:
    /// <c>Mirror/Models.cs</c>'s <c>Project</c> record deliberately does not project
    /// <c>canvas</c> ("under-projecting is free; a wrong projection is not"), and
    /// this is a presence check by id. Inventing a typed canvas projection to satisfy
    /// one boolean would be a SECOND managed model of a backend-owned type
    /// (CLAUDE.md rule 4).</para>
    ///
    /// <para><b>⚠ THE WALK ITSELF IS IN THE WinUI-FREE HALF, and not for tidiness.</b>
    /// The obvious <c>RawProject?["canvas"]?["annotations"]</c> chain THROWS on a
    /// non-object receiver — <c>JsonNode</c>'s string indexer raises
    /// <c>InvalidOperationException</c>, measured on .NET 9 during this phase and
    /// recorded in <c>IntrospectionHook.DescribeCanvas</c>'s remarks — and this method
    /// runs on the UI thread inside the <c>ProjectChanged</c> callback, where that is
    /// an unhandled exception rather than a bad answer (review IN-01).</para>
    /// </summary>
    private static bool MirrorHasAnnotation(string id)
        => CanvasStageGesture.ContainsAnnotationId(
            CanvasStageGesture.AnnotationsNode(App.Mirror?.RawProject), id);

    /// <summary>
    /// The WinUI replacement for v6.0's <c>window.prompt("Label text:")</c>. An empty
    /// or cancelled result yields <see langword="null"/>, which makes
    /// <see cref="CanvasGesture.BuildAnnotation"/> return null, which means NO
    /// dispatch.
    ///
    /// <para>Its two AutomationIds are DISTINCT from the Preview dialog's
    /// (<c>Canvas.TextDialog</c>): both surfaces can be driven in the same UIA run,
    /// and an id that matched two dialogs would let a test drive the wrong one.</para>
    /// </summary>
    private async Task<string?> PromptForStageLabelAsync()
    {
        var input = new TextBox
        {
            PlaceholderText = "Label text",
            AcceptsReturn = false,
        };
        AutomationProperties.SetAutomationId(input, "Canvas.Stage.TextDialog.Input");
        AutomationProperties.SetName(input, "Label text");

        var dialog = new ContentDialog
        {
            Title = "Add a label",
            Content = input,
            PrimaryButtonText = "Add",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = CanvasRoot.XamlRoot,
        };
        AutomationProperties.SetAutomationId(dialog, "Canvas.Stage.TextDialog");
        AutomationProperties.SetName(dialog, "Add a label");

        var chosen = await dialog.ShowAsync();
        return chosen == ContentDialogResult.Primary && input.Text.Length > 0 ? input.Text : null;
    }

    // ── geometry + rendering ───────────────────────────────────────────────────

    private void AppendFromStagePoint(Windows.Foundation.Point p)
    {
        var normalized = CanvasStageGesture.NormalizeInBox(p.X, p.Y, _boxWidth, _boxHeight);

        // ⚠ DISTINCT, not merely bounded (plan 51-10): OnStagePointerMoved appends
        // the coalesced entry AND the authoritative current position on every event,
        // and on a real injected-input drive those two are frequently the SAME point.
        CanvasGesture.AccumulateDistinctPoint(_points, normalized);
    }

    /// <summary>Stage-DIP coordinates for a normalized point, through the SAME box
    /// the point was normalized with. Round-tripping through normalized space —
    /// rather than keeping the raw pointer coordinates — is what lets a resize
    /// re-place the trail instead of smearing it.</summary>
    private Windows.Foundation.Point ToStagePoint(NormPoint n)
        => new(n.X * _boxWidth, n.Y * _boxHeight);

    /// <summary>
    /// Rebuild the live trail's geometry from the normalized list and ASSIGN it.
    ///
    /// <para><b>⚠ MEASURED, both halves (plan 51-05).</b> A fresh
    /// <see cref="Geometry"/> assigned to <c>Path.Data</c> is a plain
    /// DependencyProperty set with a new value, which invalidates unambiguously;
    /// mutating a <c>Polyline</c>'s <c>Points</c> collection does NOT re-render. And
    /// a Shape's size is derived from its geometry, so the FIRST assignment grows it
    /// from 0x0 — without the forced layout pass the trail paints nothing until
    /// something else happens to invalidate.</para>
    ///
    /// <para>Bounded and off any per-frame path: once per pointer EVENT while a
    /// gesture is in progress — a human-timescale interaction — and never at rest,
    /// because <see cref="OnStagePointerMoved"/> returns before reaching it.</para>
    ///
    /// <para>A single point draws nothing: a zero-length dashed stroke has no pixels,
    /// which is correct — one sample is a tap, not a trail.</para>
    /// </summary>
    private void RenderStageTrail()
    {
        if (_points.Count < 2)
        {
            StageLiveTrail.Data = null;
            return;
        }

        var segment = new PolyLineSegment();
        for (var i = 1; i < _points.Count; i++)
        {
            segment.Points.Add(ToStagePoint(_points[i]));
        }

        var figure = new PathFigure
        {
            StartPoint = ToStagePoint(_points[0]),
            IsClosed = false,
            IsFilled = false,
        };
        figure.Segments.Add(segment);

        var geometry = new PathGeometry();
        geometry.Figures.Add(figure);
        StageLiveTrail.Data = geometry;
        StageLiveTrail.UpdateLayout();
    }

    private void ClearStageTrail()
    {
        _trailFrozen = false;
        _points.Clear();
        StageLiveTrail.Data = null;
    }

    /// <summary>
    /// The ONLY way the ASYNC half of a commit is allowed to clear the trail.
    ///
    /// <para><c>_points</c> is a single SHARED list and the trail is a single shared
    /// element, so a commit that resumes after the next pointer-down would otherwise
    /// wipe a stroke that is still being drawn — committing a truncated shape, or none
    /// at all once <c>BuildAnnotation</c> refuses the emptied list (review WR-01; the
    /// same failure D-60.2-02 records for <c>PreviewInkLayer</c>, arriving here through
    /// the continuation rather than through the timer).</para>
    ///
    /// <para>Standing down loses this mark NOTHING: <see cref="ApplyMirrorState"/>
    /// draws it from mirror state, not from the trail this commit was holding, and by
    /// the time a later gesture exists the hand-off window this trail existed for has
    /// long closed.</para>
    /// </summary>
    private void ClearStageTrailIfCurrent(int token)
    {
        if (!_gestures.IsCurrent(token))
        {
            // Logged rather than silent: "my stroke came out short" is precisely the
            // report this guard exists to prevent, and a run that hits the race often
            // should say so.
            App.LogDiagnostic(
                $"canvas stage: commit for gesture {token} finished after gesture " +
                $"{_gestures.Current} began — trail left ALONE (it is not this commit's)");
            return;
        }

        ClearStageTrail();
    }

    // ── committed marks, rendered FROM THE MIRROR ──────────────────────────────

    /// <summary>Length of an arrow head's arms, in stage DIPs.</summary>
    private const double ArrowHeadLength = 12.0;

    /// <summary>Half-angle between an arrow head's arms and the shaft, in radians
    /// (30 degrees each side).</summary>
    private const double ArrowHeadSpread = Math.PI / 6.0;

    /// <summary>The last parsed set of committed whiteboard marks, cached so a
    /// resize can re-place them WITHOUT re-reading the mirror. The mirror is the
    /// source of truth; this is a render input derived from it, never a second
    /// model of it (CLAUDE.md rule 4).</summary>
    private IReadOnlyList<StageMark> _committed = [];

    /// <summary>
    /// Draw every COMMITTED whiteboard mark from mirrored project state.
    ///
    /// <para><b>Why this exists at all.</b> On the Preview, the second drawer at the
    /// D-13 hand-off is the ENGINE. Whiteboard marks never composite there
    /// (T-51-13), so if this region cleared its live trail with no committed layer
    /// behind it, every stroke would VANISH about a second after being drawn. It is
    /// also the plain reading of CLAUDE.md rule 4 — the renderer is a read-only
    /// mirror updated by events — and it is what makes undo/redo VISIBLE here: an
    /// Undo removes the annotation from project state, the next project:changed
    /// arrives with a shorter array, and the mark stops being drawn. No stage-local
    /// retained state participates.</para>
    ///
    /// <para><b>Raw node, never a typed projection.</b> <c>Mirror/Models.cs</c>'s
    /// <c>Project</c> record deliberately does not project <c>canvas</c> —
    /// "under-projecting is free; a wrong projection is not". The whiteboard filter
    /// and all the malformed-node handling live in the PURE, unit-tested
    /// <see cref="CanvasStageGesture.ParseWhiteboardMarks"/>; this method only
    /// renders.</para>
    ///
    /// <para>A COLD path: once per project:changed and once per resize, never per
    /// frame. Every stroke, lasso and arrow lands in ONE
    /// <see cref="PathGeometry"/> on ONE element with ONE brush.</para>
    /// </summary>
    internal void ApplyMirrorState(Rudis.Shell.Mirror.ShellMirror mirror)
    {
        // The same non-throwing walk MirrorHasAnnotation uses, for the same measured
        // reason (review IN-01): the null-conditional chain this replaced defended
        // against a MISSING node and against nothing else, and a throw on this path
        // is an unhandled exception on the UI thread.
        var annotations = CanvasStageGesture.AnnotationsNode(mirror.RawProject);
        _committed = CanvasStageGesture.ParseWhiteboardMarks(annotations, out var skipped);

        // ⚠ A DROPPED MARK IS SAID OUT LOUD (review IN-02). Skipping a node the render
        // pass cannot understand is correct — one bad entry must not abort the marks
        // after it — but the commit hand-off confirms by ID ALONE, so a mark that
        // lands in state and cannot be parsed here confirms the commit, clears the
        // trail and then draws NOTHING. That is the silently-discarded stroke this
        // whole surface exists to end, and it must not be discoverable only by
        // noticing the absence.
        if (skipped > 0)
        {
            App.LogDiagnostic(
                $"canvas stage: {skipped} whiteboard mark(s) in project state could NOT be parsed " +
                "and are NOT drawn");
        }

        RenderCommittedMarks();
    }

    /// <summary>
    /// Denormalize the cached marks into the CURRENT stage box and draw them.
    ///
    /// <para>The box is <c>CanvasRoot</c>'s own <c>ActualWidth</c>/
    /// <c>ActualHeight</c> — the same box the gesture normalized against, which is
    /// what makes the live trail and the committed mark land on the same pixels at
    /// the hand-off instant. A not-yet-measured stage skips the render; the
    /// SizeChanged that gives it a size re-runs it.</para>
    /// </summary>
    private void RenderCommittedMarks()
    {
        var width = CanvasRoot.ActualWidth;
        var height = CanvasRoot.ActualHeight;
        if (width <= 0 || height <= 0)
        {
            return;
        }

        StageLabels.Children.Clear();

        if (_committed.Count == 0)
        {
            // The half that makes an UNDO visible: nothing in state, nothing drawn.
            CommittedInk.Data = null;
            return;
        }

        var geometry = new PathGeometry();

        foreach (var mark in _committed)
        {
            switch (mark.Kind)
            {
                case StageMarkKind.Stroke:
                    AddPolylineFigure(geometry, mark.Points, width, height, closed: false);
                    break;

                case StageMarkKind.Lasso:
                    AddPolylineFigure(geometry, mark.Points, width, height, closed: true);
                    break;

                case StageMarkKind.Arrow:
                    AddArrowFigures(geometry, mark.Points, width, height);
                    break;

                case StageMarkKind.Label:
                    AddLabel(mark, width, height);
                    break;
            }
        }

        CommittedInk.Data = geometry;

        // Same measured requirement as the live trail: a Shape's arranged bounds are
        // still empty until a layout pass runs, so the FIRST assignment paints
        // nothing without this. Cold path — per project:changed, never per frame.
        CommittedInk.UpdateLayout();
    }

    private static void AddPolylineFigure(
        PathGeometry geometry, IReadOnlyList<NormPoint> points, double width, double height, bool closed)
    {
        if (points.Count < 2)
        {
            // One point is a tap: a zero-length dashed stroke has no pixels, and a
            // one-point figure would only cost a geometry node to draw nothing.
            return;
        }

        var segment = new PolyLineSegment();
        for (var i = 1; i < points.Count; i++)
        {
            segment.Points.Add(Denormalize(points[i], width, height));
        }

        var figure = new PathFigure
        {
            StartPoint = Denormalize(points[0], width, height),
            IsClosed = closed,
            IsFilled = false,
        };
        figure.Segments.Add(segment);
        geometry.Figures.Add(figure);
    }

    /// <summary>
    /// The shaft, plus two head arms swept back from the tip at
    /// <see cref="ArrowHeadSpread"/> off the shaft direction. Pure trigonometry on
    /// already-denormalized points — a DEGENERATE (zero-length) shaft draws the
    /// shaft figure and no head, because there is no direction to point one in and
    /// <c>Atan2(0, 0)</c> would invent one.
    /// </summary>
    private static void AddArrowFigures(
        PathGeometry geometry, IReadOnlyList<NormPoint> points, double width, double height)
    {
        if (points.Count < 2)
        {
            return;
        }

        var start = Denormalize(points[0], width, height);
        var end = Denormalize(points[1], width, height);

        var shaft = new PathFigure { StartPoint = start, IsClosed = false, IsFilled = false };
        var shaftSegment = new PolyLineSegment();
        shaftSegment.Points.Add(end);
        shaft.Segments.Add(shaftSegment);
        geometry.Figures.Add(shaft);

        var dx = end.X - start.X;
        var dy = end.Y - start.Y;
        if ((dx * dx) + (dy * dy) <= 0)
        {
            return;
        }

        // The arms point BACK down the shaft (hence the half turn), one to each side.
        var back = Math.Atan2(dy, dx) + Math.PI;
        foreach (var side in new[] { 1.0, -1.0 })
        {
            var angle = back + (side * ArrowHeadSpread);
            var arm = new Windows.Foundation.Point(
                end.X + (ArrowHeadLength * Math.Cos(angle)),
                end.Y + (ArrowHeadLength * Math.Sin(angle)));

            var figure = new PathFigure { StartPoint = end, IsClosed = false, IsFilled = false };
            var segment = new PolyLineSegment();
            segment.Points.Add(arm);
            figure.Segments.Add(segment);
            geometry.Figures.Add(figure);
        }
    }

    /// <summary>
    /// One committed label, positioned by <c>Margin</c> inside
    /// <c>StageLabels</c>.
    ///
    /// <para><b>Margin placement in a Grid, NOT a XAML <c>Canvas</c> panel</b> —
    /// <c>Canvas</c> in this namespace is THIS class, so the panel's own name is
    /// unavailable here without a fully-qualified alias that would read as a
    /// mistake. The brush comes from the token dictionary by KEY, the same
    /// <c>Token(key)</c> move <c>Chat</c> and <c>TitleBar</c> already make; a colour
    /// constructed from channel literals in code would be a raw colour and a build
    /// failure (CLAUDE.md rule 7, gated by the raw-hex scan).</para>
    /// </summary>
    private void AddLabel(StageMark mark, double width, double height)
    {
        if (mark.Points.Count < 1 || string.IsNullOrEmpty(mark.Text))
        {
            // An empty label is legal domain data (the backend truncates label text
            // and never refuses it) that simply draws nothing.
            return;
        }

        var at = Denormalize(mark.Points[0], width, height);
        StageLabels.Children.Add(new TextBlock
        {
            Text = mark.Text,
            Foreground = (Brush)Application.Current.Resources["accent"],
            FontSize = 12,
            HorizontalAlignment = HorizontalAlignment.Left,
            VerticalAlignment = VerticalAlignment.Top,
            Margin = new Thickness(at.X, at.Y, 0, 0),
            IsHitTestVisible = false,
        });
    }

    private static Windows.Foundation.Point Denormalize(NormPoint n, double width, double height)
        => new(n.X * width, n.Y * height);

    private void AbandonStageGesture(string why)
    {
        StopStageConfirmTimer();
        _gestureActive = false;
        ClearStageTrail();
        App.LogDiagnostic("canvas stage: gesture abandoned — " + why);
    }

    /// <summary>
    /// Rebuild the dotted grid for the new size — a COLD path: it runs on layout
    /// changes only, never per frame, and it no-ops unless the dot COUNT actually
    /// changed (a WinUI layout pass fires SizeChanged far more often than the
    /// rounded row/column counts move).
    ///
    /// <para>WinUI 3 has no tiling brush (WPF's <c>TileMode</c> has no WinUI
    /// counterpart and <c>ImageBrush</c> does not tile), so a repeating dot field is
    /// either an image asset or geometry. Geometry keeps the colour a NAMED TOKEN,
    /// which an asset could not (rule 7), and keeps the whole grid one visual.</para>
    /// </summary>
    private void OnCanvasSizeChanged(object sender, SizeChangedEventArgs e)
    {
        var width = e.NewSize.Width;
        var height = e.NewSize.Height;
        if (width <= 0 || height <= 0)
        {
            return;
        }

        // A layout change moves the box the normalized points map into. Re-latch and
        // re-draw from the SAME normalized list — which is exactly why the trail is
        // rendered from that list rather than from raw pointer coordinates: a resize
        // mid-gesture re-places the trail instead of smearing it.
        if (_gestureActive || _trailFrozen)
        {
            _boxWidth = width;
            _boxHeight = height;
            RenderStageTrail();
        }

        // Committed marks track the box too, exactly as the dot grid below does —
        // they are stored normalized, so a resize re-places them rather than
        // stretching a stale geometry. Re-rendered from the CACHED parse, so a
        // layout storm never re-reads the mirror.
        RenderCommittedMarks();

        var columns = (int)Math.Floor(width / DotPitch) + 1;
        var rows = (int)Math.Floor(height / DotPitch) + 1;
        if (columns == _dotColumns && rows == _dotRows)
        {
            return;
        }

        _dotColumns = columns;
        _dotRows = rows;

        var group = new GeometryGroup();
        var drawn = 0;
        for (var row = 0; row < rows && drawn < MaxDots; row++)
        {
            for (var column = 0; column < columns && drawn < MaxDots; column++)
            {
                group.Children.Add(new EllipseGeometry
                {
                    Center = new Windows.Foundation.Point(column * DotPitch, row * DotPitch),
                    RadiusX = DotRadius,
                    RadiusY = DotRadius,
                });
                drawn++;
            }
        }

        DotGrid.Data = group;
    }
}
