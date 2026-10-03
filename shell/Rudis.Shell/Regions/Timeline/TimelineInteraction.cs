using System.Diagnostics;

namespace Rudis.Shell.Regions;

/// <summary>Where the machine is. Everything except <see cref="Idle"/> is a gesture in
/// flight, and a gesture in flight is 100% LOCAL state (D-11).</summary>
internal enum InteractionState
{
    Idle,
    Dragging,
    Trimming,
    Scrubbing,
}

/// <summary>Modifier keys held at pointer-press time. Carried so the region does not
/// have to invent a second event shape later: D-08 defers rubber-band and Shift/Ctrl
/// multi-select to Phase 53, which is where its stated consumer — the
/// <c>Inspector</c>'s common-subset view — first exists.</summary>
[Flags]
internal enum PointerModifiers
{
    None = 0,
    Control = 1,
    Shift = 2,
    Alt = 4,
}

/// <summary>The keyboard verbs this region answers (README:140).</summary>
internal enum TimelineKey
{
    None,
    Split,
    Delete,
    Duplicate,
    Escape,
}

/// <summary>Which edit a <see cref="PendingCommand"/> carries. The JSON is what
/// actually travels; this is for the caller's own diagnostics and for the evidence
/// record a launch produces.</summary>
internal enum PendingKind
{
    Move,
    MoveToTrack,
    Trim,
    Split,
    Duplicate,
    Remove,

    // ── plan 52-14's two, and they are NOT built by this state machine ──
    //
    // Every kind above is produced by a POINTER or KEY gesture on the drawn surface,
    // which is what this machine is for. Adding and removing a TRACK is a toolbar
    // gesture with no surface geometry in it at all, so `Timeline.xaml.cs` builds those
    // two directly from `TimelineCommands`. They are named here anyway because
    // `PendingCommand.Kind` is what the dispatch path reports in its diagnostics and in
    // its two failure messages, and an edit that travelled as `Remove` when it removed a
    // whole TRACK would make a real fault report read as the wrong bug.
    AddTrack,
    RemoveTrack,
}

/// <summary>One edit, built and ready for the layer above to send. The machine BUILDS
/// these and never sends them — see <see cref="TimelineInteraction"/>'s remarks for
/// why that separation is load-bearing.</summary>
internal readonly record struct PendingCommand(PendingKind Kind, string Json);

/// <summary>
/// The drag/trim preview rectangle, in LOGICAL px measured from the Timeline surface's
/// top-left — the same origin every pointer event arrives in and the same origin the
/// frame builder converts from (D-14: one coordinate space, one conversion seam).
/// </summary>
/// <param name="MediaId">So the frame builder colours the ghost with the SAME poster
/// entry the real clip uses, rather than inventing a colour for it.</param>
/// <param name="AudioLane">The destination lane is an audio lane, so the ghost takes
/// the audio fill instead of a poster colour.</param>
internal readonly record struct GhostRect(
    double XLogicalPx,
    double YLogicalPx,
    double WLogicalPx,
    double HLogicalPx,
    string MediaId,
    bool AudioLane);

/// <summary>
/// The Timeline's pointer and keyboard state machine: Idle → Dragging / Trimming /
/// Scrubbing → Idle, with the edit built on RELEASE and never before.
///
/// <para><b>D-11 is the whole design, and it is a structural claim rather than a
/// tuning one.</b> While a drag or a trim is in flight this class produces a ghost
/// rectangle, a set of snap guides and a live duration readout — all local, all
/// recomputed from the pointer position, all costing zero managed bytes. Not one
/// backend call happens per pixel of movement. That is why criterion 2's ≤100ms
/// feedback target is a property of the architecture: the visual answer is a local
/// redraw, and the only round trip in the gesture happens once, after the pointer is
/// already up. <c>a_hundred_moves_while_dragging_emit_zero_commands</c> is that
/// paragraph as a test, including the assertion that <see cref="OnPointerMoved"/>
/// returns <see langword="void"/> so a future change cannot quietly reintroduce the
/// per-pixel round trip.</para>
///
/// <para><b>This class is WinUI-free AND transport-free.</b> It takes logical
/// coordinates and a clock; it hands BUILT JSON upward and lets the region send it.
/// The second absence is deliberate and is what makes every behaviour here testable
/// with no window, no GPU device and no engine — and it is also what plan 52-08's
/// "nothing on the paint path can reach the boundary" gate depends on.</para>
///
/// <para><b>Snap targets are resolved ONCE, at press.</b> The culled clip array is
/// captured when the gesture starts and reused for its duration, so a hundred moves
/// cost a hundred passes over a screenful rather than a hundred culls — and so the
/// neighbours a gesture snaps to cannot change under the user mid-drag.</para>
///
/// <para>No WinUI types, by rule (D-13).</para>
/// </summary>
internal sealed class TimelineInteraction
{
    /// <summary>The renderer's own bound on the guides one frame may carry
    /// (<c>crates/timeline-render/src/abi.rs:63</c>). Mirrored here so the machine
    /// never produces more than the frame can describe.</summary>
    public const int MaxSnapGuides = 64;

    /// <summary>Widest <see cref="LiveDurationReadout"/>: <c>HHH:MM:SS.mmm</c> with
    /// slack.</summary>
    private const int ReadoutMaxChars = 24;

    private const double UsPerSecond = 1_000_000.0;

    private static readonly long EpochUnixMsAtStart = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

    /// <summary>Monotonic by construction: a unix-ms EPOCH captured once plus elapsed
    /// time, so a clock adjustment mid-session cannot make an input look older than the
    /// one before it — which is exactly the shape that would corrupt plan 52-09's
    /// input→redraw measurement.</summary>
    private static readonly Stopwatch Monotonic = Stopwatch.StartNew();

    private readonly TimelineModel _model;
    private readonly TimelineViewport _viewport;
    private readonly Func<long> _nowUnixMs;

    /// <summary>The press-time cull, reused for the whole gesture and across
    /// gestures.</summary>
    private readonly List<ClipLayout> _candidates = new(256);

    private readonly float[] _snapGuides = new float[MaxSnapGuides];
    private readonly char[] _readout = new char[ReadoutMaxChars];

    private InteractionState _state;
    private ClipSource _subject = ClipSource.None;
    private TrimEdge _edge = TrimEdge.None;

    /// <summary>Pointer time minus the REFERENCE the gesture grabbed: the clip's start
    /// for a drag, and the moving timeline edge for a trim. Keeping the offset is what
    /// stops a clip jumping so its start lands under the cursor.</summary>
    private long _grabOffsetUs;

    private double _pressXLogical;
    private double _pressYLogical;
    private bool _movedPastSlop;

    private bool _hasGhost;
    private GhostRect _ghost;
    private long _ghostStartUs;
    private long _ghostInUs;
    private long _ghostOutUs;
    private int _ghostLaneIndex = -1;
    private int _snapCount;
    private int _readoutLength;

    private double _bestSnapDistPx;
    private long _bestSnapValueUs;
    private bool _hasSnap;

    private long _seekUs;
    private bool _hasSeek;

    public TimelineInteraction(TimelineModel model, TimelineViewport viewport, Func<long>? nowUnixMs = null)
    {
        ArgumentNullException.ThrowIfNull(model);
        ArgumentNullException.ThrowIfNull(viewport);

        _model = model;
        _viewport = viewport;
        _nowUnixMs = nowUnixMs ?? NowUnixMs;
    }

    /// <summary>
    /// The SAME monotonic unix-ms clock <see cref="LastInputUnixMs"/> is stamped from,
    /// exposed so the region can time a REDRAW against an INPUT on one clock rather
    /// than two (plan 52-09, criterion 2).
    ///
    /// <para>Two clocks would be the classic way to manufacture a number: a
    /// <c>DateTimeOffset.UtcNow</c> redraw stamp compared against a monotonic input
    /// stamp can differ by whatever a clock adjustment did in between, and at the
    /// 100ms scale criterion 2 is about that is not a rounding error. Exposing the
    /// one clock is cheaper than documenting the hazard.</para>
    /// </summary>
    internal static long NowUnixMs() => EpochUnixMsAtStart + Monotonic.ElapsedMilliseconds;

    // ========================================================================
    // State the region reads
    // ========================================================================

    public InteractionState State => _state;

    /// <summary>The playhead as of this tick, pushed in by the region from the value it
    /// already read. The machine never asks for it itself — that would be a per-frame
    /// call from a class whose entire point is not making any.</summary>
    public long PlayheadUs { get; set; }

    /// <summary>Monotonic unix-ms of the most recent ACCEPTED input. Plan 52-09's
    /// ≤100ms measurement reads this against the frame that followed it; a rejected
    /// (non-finite) coordinate deliberately does not stamp it, because no input
    /// happened.</summary>
    public long LastInputUnixMs { get; private set; }

    /// <summary>
    /// The dragging/trimming tooltip text (README:140), as a span over a REUSED buffer.
    ///
    /// <para>Deliberately not a <c>string</c>: this is rewritten on every pointer move,
    /// and a string property would allocate one small object per move — which would
    /// contradict this class's own zero-allocation gate. The
    /// caller-owned-buffer discipline is <c>TimelineTimecode</c>'s, for the same
    /// reason.</para>
    /// </summary>
    public ReadOnlySpan<char> LiveDurationReadout => _readout.AsSpan(0, _readoutLength);

    /// <summary>Is a CLIP selected? Drives `Split`/`Duplicate`/`Delete`, all three of
    /// which edit a clip and have no meaning for a lane (D-08: ONE primary
    /// selection).</summary>
    public bool HasSelection => !string.IsNullOrEmpty(_model.SelectedClipId);

    /// <summary>Is a LANE selected? The other subject the remove-track gesture can
    /// name — and the only one an EMPTY track can ever have (quick 260731-k9b).
    /// Mutually exclusive with <see cref="HasSelection"/> by construction.</summary>
    public bool HasTrackSelection => _model.HasSelectedTrack;

    /// <summary>Would a split at the current playhead be accepted? False when nothing
    /// is selected or the playhead is not STRICTLY inside the selected clip — the
    /// domain's own rule (<c>command.rs:1055-1067</c>), so the tile is disabled rather
    /// than offering an operation that can only fail.</summary>
    public bool CanSplitAtPlayhead =>
        _model.TryGetSelectedSource(out var clip)
        && PlayheadUs > clip.StartUs
        && PlayheadUs < clip.EndUs;

    // ========================================================================
    // Pointer
    // ========================================================================

    /// <summary>
    /// Begin a gesture. <paramref name="xSurface"/>/<paramref name="ySurface"/> are
    /// LOGICAL px relative to the Timeline surface's top-left — the space
    /// <c>GetCurrentPoint(TimelineSurface).Position</c> already speaks (D-14).
    /// </summary>
    public void OnPointerPressed(double xSurface, double ySurface, PointerModifiers modifiers)
    {
        // T-52-13, one layer up from where 52-03 mitigated it: a NaN is not a
        // coordinate, and a machine that transitioned on one would be in a state the
        // user never asked for with nothing in any log.
        if (!double.IsFinite(xSurface) || !double.IsFinite(ySurface))
        {
            return;
        }

        Stamp();
        _pressXLogical = xSurface;
        _pressYLogical = ySurface;
        _movedPastSlop = false;
        ClearGhost();

        _model.CullTo(_viewport, _candidates);
        var hit = TimelineHitTester.Test(_viewport, _candidates, PlayheadUs, xSurface, ySurface);
        var timeUs = TimeAt(xSurface);

        switch (hit.Kind)
        {
            // Scrubbing is PLAYBACK, not an edit: it travels through the transport
            // channel (see TryTakeSeek), never through a command.
            case HitKind.Ruler:
            case HitKind.Playhead:
                _state = InteractionState.Scrubbing;
                RequestSeek(timeUs);
                return;

            case HitKind.TrimHandle:
                if (!_model.TryGetSource(hit.ClipId, out _subject))
                {
                    ResetToIdle();
                    return;
                }

                _model.Select(hit.ClipId);
                _edge = hit.Edge;
                _grabOffsetUs = timeUs - (hit.Edge == TrimEdge.Left ? _subject.StartUs : _subject.EndUs);
                _ghostInUs = _subject.InUs;
                _ghostOutUs = _subject.OutUs;
                _ghostStartUs = _subject.StartUs;
                _ghostLaneIndex = _subject.LaneIndex;
                _state = InteractionState.Trimming;
                return;

            // The `TrackHeader` gutter selects the LANE (quick 260731-k9b). No drag
            // starts from here — the gutter has no time axis to drag along — so the
            // machine stays Idle and the gesture is complete on the press.
            //
            // This is the ONLY route by which an EMPTY track can be named: 52-14
            // mapped the remove gesture onto the selected CLIP's track, which left a
            // track with no clips in it unreachable, and the owner hit that dead end.
            case HitKind.TrackHeader:
                var gutterLanes = _model.Lanes;
                if ((uint)hit.LaneIndex >= (uint)gutterLanes.Count)
                {
                    // One mirror patch stale — data, not an error (the LaneTopPx rule).
                    ResetToIdle();
                    return;
                }

                // SelectTrack clears the clip selection: one primary selection (D-08).
                _model.SelectTrack(gutterLanes[hit.LaneIndex].TrackIndex);
                ResetToIdle();
                return;

            case HitKind.ClipBody:
                if (!_model.TryGetSource(hit.ClipId, out _subject))
                {
                    ResetToIdle();
                    return;
                }

                // Selection happens on PRESS, not on release: it is the one piece of
                // feedback a click owes immediately, and a drag that follows keeps it.
                _model.Select(hit.ClipId);
                _edge = TrimEdge.None;
                _grabOffsetUs = timeUs - _subject.StartUs;
                _ghostStartUs = _subject.StartUs;
                _ghostLaneIndex = _subject.LaneIndex;
                _state = InteractionState.Dragging;
                return;

            default:
                // An empty spot on a lane, the toolbar band above the surface, or the
                // gutter's corner cell under the ruler. NOT the gutter itself any more
                // — that has its own arm above and SELECTS rather than clears.
                _model.ClearSelection();
                ResetToIdle();
                return;
        }
    }

    /// <summary>
    /// Advance the gesture. <b>Returns void, and that is a contract</b>: no edit may
    /// leave this method (D-11). The only thing that crosses a boundary from here is a
    /// scrub seek, which is playback and is coalesced to one in flight by the caller.
    /// </summary>
    public void OnPointerMoved(double xSurface, double ySurface)
    {
        if (!double.IsFinite(xSurface) || !double.IsFinite(ySurface))
        {
            return;
        }

        Stamp();
        UpdateFromPointer(xSurface, ySurface);
    }

    /// <summary>
    /// End the gesture and, only now, build the edit it asked for.
    ///
    /// <para>The machine returns to <see cref="InteractionState.Idle"/> and discards
    /// the ghost BEFORE the command is built, so the surface goes back to showing
    /// mirror truth and waits for the patch rather than pre-rendering an edit the
    /// backend has not accepted yet (CLAUDE.md rule 4).</para>
    /// </summary>
    public PendingCommand? OnPointerReleased(double xSurface, double ySurface)
    {
        if (double.IsFinite(xSurface) && double.IsFinite(ySurface))
        {
            Stamp();
            UpdateFromPointer(xSurface, ySurface);
        }

        var state = _state;
        var moved = _movedPastSlop;
        var subject = _subject;
        var ghostStartUs = _ghostStartUs;
        var ghostLaneIndex = _ghostLaneIndex;
        var ghostInUs = _ghostInUs;
        var ghostOutUs = _ghostOutUs;

        ResetToIdle();

        // A press+release inside the slop radius is a CLICK: it selected (on press) and
        // asks for nothing else. Sending a move_clip to the position it already has
        // would push a no-op onto the undo stack for every click.
        if (!moved || subject.IsEmpty)
        {
            return null;
        }

        switch (state)
        {
            case InteractionState.Dragging:
                if (ghostLaneIndex == subject.LaneIndex)
                {
                    return ghostStartUs == subject.StartUs
                        ? null
                        : new PendingCommand(
                            PendingKind.Move,
                            TimelineCommands.MoveClip(subject.ClipId, ghostStartUs));
                }

                var trackIndex = TrackIndexForLane(ghostLaneIndex);
                return trackIndex < 0
                    ? null
                    : new PendingCommand(
                        PendingKind.MoveToTrack,
                        TimelineCommands.MoveClipToTrack(subject.ClipId, trackIndex, ghostStartUs));

            case InteractionState.Trimming:
                return ghostInUs == subject.InUs && ghostOutUs == subject.OutUs
                    ? null
                    : new PendingCommand(
                        PendingKind.Trim,
                        TimelineCommands.TrimClip(subject.ClipId, ghostInUs, ghostOutUs));

            default:
                return null;
        }
    }

    /// <summary>Abandon the gesture — pointer capture lost, or Escape. The ghost is
    /// discarded and NOTHING is dispatched. Deliberately not a deselect: the clip the
    /// user grabbed stays selected, because losing capture is not a statement about
    /// selection.</summary>
    public void OnCancel() => ResetToIdle();

    // ========================================================================
    // Keyboard and the tool tiles — the same three edits, two ways in
    // ========================================================================

    public PendingCommand? OnKey(TimelineKey key)
    {
        Stamp();
        switch (key)
        {
            case TimelineKey.Split:
                return RequestSplitAtPlayhead();

            case TimelineKey.Delete:
                return RequestRemove();

            case TimelineKey.Duplicate:
                return RequestDuplicate();

            case TimelineKey.Escape:
                ResetToIdle();
                _model.ClearSelection();
                return null;

            default:
                return null;
        }
    }

    /// <summary>Split the selected clip at the CURRENT playhead. Null when nothing is
    /// selected or the playhead is not strictly inside it.</summary>
    ///
    /// <remarks>
    /// ⚠ The three <c>Request*</c> verbs below <see cref="Stamp"/> on their ACCEPTED
    /// path only — added by plan 52-09, because criterion 2 asks for the ≤100ms
    /// interaction feedback of all FIVE edits and three of them (split / duplicate /
    /// delete) arrive through a `Timeline › Toolbar` tile or an accelerator rather than
    /// through a pointer gesture. Without a stamp there is no <c>t_input</c> for those
    /// three and the measurement would silently have nothing to measure.
    ///
    /// <para>Stamping only on ACCEPTANCE is <see cref="LastInputUnixMs"/>'s existing
    /// rule, kept: a rejected non-finite coordinate does not stamp, and neither does a
    /// tile press with no selection — no input the app acted on happened, so recording
    /// one would put a phantom into the measurement's denominator.</para>
    ///
    /// <para><see cref="OnKey"/> already stamps unconditionally (its own line), so the
    /// keyboard route was never the gap.</para>
    /// </remarks>
    public PendingCommand? RequestSplitAtPlayhead()
    {
        if (!_model.TryGetSelectedSource(out var clip))
        {
            return null;
        }

        var json = TimelineCommands.Split(clip, PlayheadUs);
        if (json is null)
        {
            return null;
        }

        Stamp();
        return new PendingCommand(PendingKind.Split, json);
    }

    public PendingCommand? RequestDuplicate()
    {
        if (!_model.TryGetSelectedSource(out var clip))
        {
            return null;
        }

        Stamp();
        return new PendingCommand(PendingKind.Duplicate, TimelineCommands.DuplicateClip(clip.ClipId));
    }

    public PendingCommand? RequestRemove()
    {
        if (!_model.TryGetSelectedSource(out var clip))
        {
            return null;
        }

        Stamp();
        return new PendingCommand(PendingKind.Remove, TimelineCommands.RemoveClip(clip.ClipId));
    }

    // ========================================================================
    // Wheel — zoom and pan are pure VIEW state and reach no boundary at all
    // ========================================================================

    /// <summary>
    /// Ctrl+wheel zooms about the pointer; a plain wheel pans.
    ///
    /// <para>The pan axis follows the content: vertical when the lane stack is taller
    /// than the band that shows it, horizontal otherwise. A timeline that is not tall
    /// enough to scroll has nothing to gain from a vertical wheel, and the horizontal
    /// axis is the one a Timeline is actually about.</para>
    /// </summary>
    public void OnWheel(double deltaLogicalPx, bool ctrl, double anchorXSurface)
    {
        if (!double.IsFinite(deltaLogicalPx) || !double.IsFinite(anchorXSurface))
        {
            return;
        }

        Stamp();

        if (ctrl)
        {
            var anchor = _viewport.LaneAreaXFromSurfaceX(anchorXSurface);
            if (deltaLogicalPx > 0)
            {
                _viewport.ZoomIn(anchor);
            }
            else if (deltaLogicalPx < 0)
            {
                _viewport.ZoomOut(anchor);
            }

            ClampScroll();
            return;
        }

        if (_viewport.ContentHeightPx > _viewport.LaneAreaHeightPx)
        {
            _viewport.ScrollYPx -= deltaLogicalPx;
        }
        else
        {
            _viewport.ScrollXPx -= deltaLogicalPx;
        }

        ClampScroll();
    }

    /// <summary>Zoom one step about the middle of the visible lane area — what the
    /// toolbar's <c>−</c>/<c>+</c> tiles do, since they have no pointer to anchor
    /// to.</summary>
    public void ZoomIn()
    {
        Stamp();
        _viewport.ZoomIn(_viewport.ViewportWidthPx / 2);
        ClampScroll();
    }

    public void ZoomOut()
    {
        Stamp();
        _viewport.ZoomOut(_viewport.ViewportWidthPx / 2);
        ClampScroll();
    }

    /// <summary>Fit <paramref name="projectDurationUs"/> into the visible lane width.
    /// A zero-length project falls back to the default zoom rather than dividing by
    /// zero into an infinite scale.</summary>
    public void ZoomToFit(long projectDurationUs)
    {
        Stamp();

        var width = _viewport.ViewportWidthPx;
        if (projectDurationUs <= 0 || width <= 0)
        {
            _viewport.SetPxPerSecond(TimelineMetrics.DefaultPxPerSecond);
        }
        else
        {
            _viewport.SetPxPerSecond(width / (projectDurationUs / UsPerSecond));
        }

        _viewport.ScrollXPx = 0;
        ClampScroll();
    }

    // ========================================================================
    // Frame-facing output — caller-owned storage, so the frame builder copies
    // these in with no allocation of its own
    // ========================================================================

    public bool TryGetGhost(out GhostRect ghost)
    {
        ghost = _ghost;
        return _hasGhost;
    }

    /// <summary>Copy the surviving snap targets, as SURFACE-relative logical x, into
    /// caller-owned storage; returns how many were written.</summary>
    public int GetSnapGuides(Span<float> into)
    {
        var count = _snapCount < into.Length ? _snapCount : into.Length;
        for (var i = 0; i < count; i++)
        {
            into[i] = _snapGuides[i];
        }

        return count;
    }

    /// <summary>Take the latest requested scrub position, if any. DESTRUCTIVE by
    /// design: the region keeps ONE seek in flight and coalesces to the newest value
    /// (T-50-26's rule), which only works if a taken value cannot be taken twice.</summary>
    public bool TryTakeSeek(out long positionUs)
    {
        positionUs = _seekUs;
        var had = _hasSeek;
        _hasSeek = false;
        return had;
    }

    // ========================================================================
    // The gesture's own arithmetic
    // ========================================================================

    private void UpdateFromPointer(double xSurface, double ySurface)
    {
        if (_state == InteractionState.Idle)
        {
            return;
        }

        var dx = xSurface - _pressXLogical;
        var dy = ySurface - _pressYLogical;
        if ((dx * dx) + (dy * dy) > TimelineMetrics.SnapThresholdPx * TimelineMetrics.SnapThresholdPx)
        {
            _movedPastSlop = true;
        }

        var timeUs = TimeAt(xSurface);

        switch (_state)
        {
            case InteractionState.Scrubbing:
                RequestSeek(timeUs);
                return;

            case InteractionState.Dragging:
                UpdateDragGhost(timeUs, ySurface);
                return;

            case InteractionState.Trimming:
                UpdateTrimGhost(timeUs);
                return;
        }
    }

    private void UpdateDragGhost(long timeUs, double ySurface)
    {
        var durationUs = _subject.DurationUs;

        var rawStartUs = timeUs - _grabOffsetUs;
        if (rawStartUs < 0)
        {
            rawStartUs = 0;
        }

        var startUs = SnapDragStart(rawStartUs, durationUs);
        if (startUs < 0)
        {
            startUs = 0;
        }

        _ghostStartUs = startUs;
        _ghostInUs = _subject.InUs;
        _ghostOutUs = _subject.OutUs;

        // The destination lane, when the domain would accept it there.
        var lane = _viewport.PixelToLane(_viewport.LaneAreaYFromSurfaceY(ySurface));
        _ghostLaneIndex = lane is int laneIndex && LaneAcceptsSubject(laneIndex)
            ? laneIndex
            : _subject.LaneIndex;

        SetGhost(startUs, durationUs, _ghostLaneIndex);
        WriteReadout(durationUs);
    }

    private void UpdateTrimGhost(long timeUs)
    {
        var anchorUs = _edge == TrimEdge.Left ? _subject.StartUs : _subject.EndUs;
        var rawEdgeUs = timeUs - _grabOffsetUs;
        var edgeUs = SnapSingleEdge(rawEdgeUs);

        // A left-edge trim moves start_us by the SAME delta as in_us, so the timeline
        // delta and the source delta are one number (command.rs:82-100). A right-edge
        // trim moves out_us only.
        TimelineCommands.TryTrim(_subject, _edge, edgeUs - anchorUs, out _ghostInUs, out _ghostOutUs);

        var startUs = _edge == TrimEdge.Left
            ? _subject.StartUs + (_ghostInUs - _subject.InUs)
            : _subject.StartUs;
        var durationUs = _ghostOutUs - _ghostInUs;

        _ghostStartUs = startUs;
        _ghostLaneIndex = _subject.LaneIndex;

        SetGhost(startUs, durationUs, _subject.LaneIndex);
        WriteReadout(durationUs);
    }

    private void SetGhost(long startUs, long durationUs, int laneIndex)
    {
        var lanes = _viewport.Lanes;
        var haveLane = (uint)laneIndex < (uint)lanes.Count;
        var lane = haveLane ? lanes[laneIndex] : default;

        var x = TimelineMetrics.TrackHeaderGutterWidth + _viewport.TimeUsToPixel(startUs);
        var w = durationUs / UsPerSecond * _viewport.PxPerSecond;
        var y = TimelineMetrics.TimelineHeaderHeight + TimelineMetrics.RulerHeight
                + (haveLane ? lane.TopPx : 0) - _viewport.ScrollYPx;
        var h = haveLane ? lane.HeightPx : 0;

        _ghost = new GhostRect(x, y, w, h, _subject.MediaId, haveLane && lane.Kind == LaneModel.AudioKind);
        _hasGhost = true;
    }

    // ── snapping (README:140) ────────────────────────────────────────────────

    /// <summary>
    /// Snap the dragged clip's LEADING or TRAILING edge to the playhead or to an
    /// adjacent clip edge within <see cref="TimelineMetrics.SnapThresholdPx"/>, and
    /// record every target that survived the filter as a guide.
    ///
    /// <para>The trailing edge matters as much as the leading one: "butt this clip up
    /// against the next" is the gesture, and only the trailing edge can express it.</para>
    /// </summary>
    private long SnapDragStart(long rawStartUs, long durationUs)
    {
        BeginSnapPass();

        var pxPerSecond = _viewport.PxPerSecond;
        if (pxPerSecond <= 0)
        {
            return rawStartUs;
        }

        ConsiderDragTarget(PlayheadUs, rawStartUs, durationUs, pxPerSecond);

        for (var i = 0; i < _candidates.Count; i++)
        {
            var candidate = _candidates[i];
            if (string.Equals(candidate.Id, _subject.ClipId, StringComparison.Ordinal))
            {
                continue;
            }

            ConsiderDragTarget(candidate.StartUs, rawStartUs, durationUs, pxPerSecond);
            ConsiderDragTarget(candidate.EndUs, rawStartUs, durationUs, pxPerSecond);
        }

        return _hasSnap ? _bestSnapValueUs : rawStartUs;
    }

    private void ConsiderDragTarget(long targetUs, long rawStartUs, long durationUs, double pxPerSecond)
    {
        var within = false;

        var leadingPx = DistancePx(targetUs, rawStartUs, pxPerSecond);
        if (leadingPx <= TimelineMetrics.SnapThresholdPx)
        {
            within = true;
            OfferSnap(leadingPx, targetUs);
        }

        var trailingPx = DistancePx(targetUs, rawStartUs + durationUs, pxPerSecond);
        if (trailingPx <= TimelineMetrics.SnapThresholdPx)
        {
            within = true;
            var start = targetUs - durationUs;
            if (start >= 0)
            {
                OfferSnap(trailingPx, start);
            }
        }

        if (within)
        {
            AddGuide(targetUs);
        }
    }

    /// <summary>The trim case: one moving edge, so one distance per target.</summary>
    private long SnapSingleEdge(long rawEdgeUs)
    {
        BeginSnapPass();

        var pxPerSecond = _viewport.PxPerSecond;
        if (pxPerSecond <= 0)
        {
            return rawEdgeUs;
        }

        ConsiderEdgeTarget(PlayheadUs, rawEdgeUs, pxPerSecond);

        for (var i = 0; i < _candidates.Count; i++)
        {
            var candidate = _candidates[i];
            if (string.Equals(candidate.Id, _subject.ClipId, StringComparison.Ordinal))
            {
                continue;
            }

            ConsiderEdgeTarget(candidate.StartUs, rawEdgeUs, pxPerSecond);
            ConsiderEdgeTarget(candidate.EndUs, rawEdgeUs, pxPerSecond);
        }

        return _hasSnap ? _bestSnapValueUs : rawEdgeUs;
    }

    private void ConsiderEdgeTarget(long targetUs, long rawEdgeUs, double pxPerSecond)
    {
        var distancePx = DistancePx(targetUs, rawEdgeUs, pxPerSecond);
        if (distancePx > TimelineMetrics.SnapThresholdPx)
        {
            return;
        }

        OfferSnap(distancePx, targetUs);
        AddGuide(targetUs);
    }

    private void BeginSnapPass()
    {
        _snapCount = 0;
        _hasSnap = false;
        _bestSnapDistPx = double.MaxValue;
        _bestSnapValueUs = 0;
    }

    private void OfferSnap(double distancePx, long valueUs)
    {
        if (distancePx >= _bestSnapDistPx)
        {
            return;
        }

        _bestSnapDistPx = distancePx;
        _bestSnapValueUs = valueUs;
        _hasSnap = true;
    }

    /// <summary>Record a surviving snap target as a surface-relative guide x, deduped —
    /// two clips that touch share an edge, and drawing the same hairline twice is a
    /// double-strength line the user reads as a different thing.</summary>
    private void AddGuide(long targetUs)
    {
        if (_snapCount >= MaxSnapGuides)
        {
            return;
        }

        var x = (float)(TimelineMetrics.TrackHeaderGutterWidth + _viewport.TimeUsToPixel(targetUs));
        for (var i = 0; i < _snapCount; i++)
        {
            if (_snapGuides[i] == x)
            {
                return;
            }
        }

        _snapGuides[_snapCount++] = x;
    }

    private static double DistancePx(long aUs, long bUs, double pxPerSecond)
    {
        var delta = (aUs - bUs) / UsPerSecond * pxPerSecond;
        return delta < 0 ? -delta : delta;
    }

    // ── lanes ────────────────────────────────────────────────────────────────

    /// <summary>
    /// <c>crates/core/src/model.rs:1310-1318</c>'s <c>track_accepts_media</c>,
    /// reproduced so an impossible drop is refused as a GESTURE.
    ///
    /// <para>The alternative — dispatch and let the backend bounce it — shows the user
    /// an error banner for something the UI should never have let them ask for. This
    /// is a mirror of a domain rule, not a second source of truth: if the two ever
    /// disagree the backend still wins, because it is the one that applies the
    /// edit.</para>
    /// </summary>
    private bool LaneAcceptsSubject(int laneIndex)
    {
        var lanes = _viewport.Lanes;
        if ((uint)laneIndex >= (uint)lanes.Count)
        {
            return false;
        }

        var laneKind = lanes[laneIndex].Kind;

        if (string.Equals(laneKind, LaneModel.VideoKind, StringComparison.Ordinal))
        {
            return string.Equals(_subject.MediaKind, ClipSource.VideoMedia, StringComparison.Ordinal)
                || string.Equals(_subject.MediaKind, ClipSource.ImageMedia, StringComparison.Ordinal);
        }

        if (string.Equals(laneKind, LaneModel.AudioKind, StringComparison.Ordinal))
        {
            return string.Equals(_subject.MediaKind, ClipSource.AudioMedia, StringComparison.Ordinal)
                || (string.Equals(_subject.MediaKind, ClipSource.VideoMedia, StringComparison.Ordinal)
                    && _subject.MediaHasAudio);
        }

        return false;
    }

    /// <summary>Lane index → <c>Project.Timeline.Tracks</c> index. The two DIFFER as
    /// soon as the payload carries a track kind this build does not draw (D-06), and
    /// the cross-track command names the TRACK.</summary>
    private int TrackIndexForLane(int laneIndex)
    {
        var lanes = _viewport.Lanes;
        return (uint)laneIndex < (uint)lanes.Count ? lanes[laneIndex].TrackIndex : -1;
    }

    // ── small plumbing ───────────────────────────────────────────────────────

    private long TimeAt(double xSurface) =>
        _viewport.PixelToTimeUs(_viewport.LaneAreaXFromSurfaceX(xSurface));

    private void RequestSeek(long positionUs)
    {
        _seekUs = positionUs < 0 ? 0 : positionUs;
        _hasSeek = true;
    }

    private void ClampScroll()
    {
        if (_viewport.ScrollXPx < 0)
        {
            _viewport.ScrollXPx = 0;
        }

        var overflow = _viewport.ContentHeightPx - _viewport.LaneAreaHeightPx;
        if (overflow < 0)
        {
            overflow = 0;
        }

        if (_viewport.ScrollYPx < 0)
        {
            _viewport.ScrollYPx = 0;
        }
        else if (_viewport.ScrollYPx > overflow)
        {
            _viewport.ScrollYPx = overflow;
        }
    }

    private void ResetToIdle()
    {
        _state = InteractionState.Idle;
        _subject = ClipSource.None;
        _edge = TrimEdge.None;
        _grabOffsetUs = 0;
        _movedPastSlop = false;
        ClearGhost();
    }

    private void ClearGhost()
    {
        _hasGhost = false;
        _ghost = default;
        _ghostStartUs = 0;
        _ghostInUs = 0;
        _ghostOutUs = 0;
        _ghostLaneIndex = -1;
        _snapCount = 0;
        _readoutLength = 0;
    }

    private void Stamp() => LastInputUnixMs = _nowUnixMs();

    // ── the readout's digits ─────────────────────────────────────────────────
    //
    // Hand-rolled for the same reason TimelineTimecode's are: this runs per pointer
    // move, and the number formatter's culture lookup on a `long` is a cost with
    // nothing to show for it at three fixed field widths.

    private void WriteReadout(long durationUs)
    {
        if (durationUs < 0)
        {
            durationUs = 0;
        }

        // SYMMETRIC WITH THE NEGATIVE CLAMP, and for the same reason (52-REVIEW CR-01).
        // `durationUs` arrives here as `_subject.DurationUs` — `OutUs - InUs` straight
        // off the mirrored clip, with no upper bound applied anywhere between the JSON
        // and this line — on the FIRST OnPointerMoved of every drag. A one-sided clamp
        // bounds only half the ways that value can be wrong.
        if (durationUs > TimelineTimecode.MaxDisplayableUs)
        {
            durationUs = TimelineTimecode.MaxDisplayableUs;
        }

        var totalSeconds = durationUs / 1_000_000L;
        var millis = (durationUs % 1_000_000L) / 1_000L;
        var hours = totalSeconds / 3600;
        var minutes = (totalSeconds / 60) % 60;
        var seconds = totalSeconds % 60;

        var dest = _readout.AsSpan();
        var at = 0;
        var ok = true;

        if (hours > 0)
        {
            ok = TryWriteUnpadded(dest, ref at, hours)
                && TryWriteChar(dest, ref at, ':')
                && TryWriteTwo(dest, ref at, minutes);
        }
        else
        {
            ok = TryWriteUnpadded(dest, ref at, minutes);
        }

        ok = ok
            && TryWriteChar(dest, ref at, ':')
            && TryWriteTwo(dest, ref at, seconds)
            && TryWriteChar(dest, ref at, '.')
            && TryWriteThree(dest, ref at, millis);

        // A readout that will not fit is an EMPTY readout, never a throw: this runs on
        // the UI thread inside a pointer handler, and App.xaml.cs installs no
        // Application.UnhandledException handler, so an exception here is a process exit
        // on the most common gesture in the region.
        _readoutLength = ok ? at : 0;
    }

    // Bounds-checked for the reason recorded on TimelineTimecode's own writers: the
    // previous shape trusted the 24-char buffer to be wide enough for whatever `hours`
    // divided down to, which is an assumption about the DATA rather than a property of
    // the code.

    private static bool TryWriteChar(Span<char> dest, ref int at, char value)
    {
        if ((uint)at >= (uint)dest.Length)
        {
            return false;
        }

        dest[at++] = value;
        return true;
    }

    private static bool TryWriteUnpadded(Span<char> dest, ref int at, long value)
    {
        if (value < 0)
        {
            value = 0;
        }

        var digits = 1;
        for (var probe = value; probe >= 10; probe /= 10)
        {
            digits++;
        }

        if (at + digits > dest.Length)
        {
            return false;
        }

        for (var i = digits - 1; i >= 0; i--)
        {
            dest[at + i] = (char)('0' + (int)(value % 10));
            value /= 10;
        }

        at += digits;
        return true;
    }

    private static bool TryWriteTwo(Span<char> dest, ref int at, long value)
    {
        if (value >= 100)
        {
            return TryWriteUnpadded(dest, ref at, value);
        }

        if (value < 0 || at + 2 > dest.Length)
        {
            return false;
        }

        dest[at++] = (char)('0' + (int)(value / 10));
        dest[at++] = (char)('0' + (int)(value % 10));
        return true;
    }

    private static bool TryWriteThree(Span<char> dest, ref int at, long value)
    {
        if (value >= 1000)
        {
            return TryWriteUnpadded(dest, ref at, value);
        }

        if (value < 0 || at + 3 > dest.Length)
        {
            return false;
        }

        dest[at++] = (char)('0' + (int)(value / 100));
        dest[at++] = (char)('0' + (int)((value / 10) % 10));
        dest[at++] = (char)('0' + (int)(value % 10));
        return true;
    }
}
