namespace Rudis.Shell.Regions;

/// <summary>
/// The Timeline's coordinate system: px-per-second, scroll, the lane stack, the
/// visible time window, zoom — and the ONE place logical and physical pixels meet.
///
/// <para><b>EVERYTHING IN THIS DIRECTORY IS LOGICAL PIXELS. Physical pixels exist
/// only past <see cref="LogicalToPhysical"/> and <see cref="PhysicalToLogical"/>,
/// which are the only two methods in it allowed to mention display scale
/// (D-14).</b> This is 50-05/50-06's DPI lesson carried as CODE rather than as a
/// comment: 50-05 lost time to `GetWindowRect`/`SetCursorPos` speaking logical px
/// while `Graphics.CopyFromScreen` and UIA `BoundingRectangle` speak physical;
/// 50-06 re-paid it measuring a 44-logical-px region as 55 physical px at 125%;
/// and 52-01's own screenshot harness clipped a panel off the evidence for exactly
/// the same reason. A model that keeps ONE unit and converts at ONE named seam
/// cannot make that mistake a fourth time —
/// <c>logical_and_physical_convert_at_exactly_one_boundary</c> asserts both the
/// conversion AND the invariance of everything else to scale.</para>
///
/// <para><b>Two horizontal origins, also named once.</b> <c>x</c> in this class is
/// LANE-AREA x — measured from the left edge of the time axis, i.e. past the 46px
/// `TrackHeader` gutter. A pointer event arrives in SURFACE x; put it through
/// <see cref="LaneAreaXFromSurfaceX"/> exactly once. Vertically the same:
/// <see cref="LaneAreaYFromSurfaceY"/> converts a surface y into CONTENT y (below
/// the Timeline header and ruler, with vertical scroll already applied).</para>
///
/// <para>Every arithmetic member is allocation-free — no LINQ, no <c>params</c>, no
/// boxing, no closures — because <c>TimelineHotPathGateTests</c> asserts a
/// <c>GC.GetAllocatedBytesForCurrentThread()</c> delta of exactly 0 across the
/// cull + hit-test path that calls them.</para>
///
/// <para>No WinUI types, by rule (D-13).</para>
/// </summary>
internal sealed class TimelineViewport
{
    private const double UsPerSecond = 1_000_000.0;

    private static readonly Lane[] NoLanes = [];

    private double _pxPerSecond = TimelineMetrics.DefaultPxPerSecond;
    private double _scrollXPx;
    private double _scrollYPx;
    private double _viewportWidthPx;
    private double _viewportHeightPx;
    private double _scale = 1.0;
    private IReadOnlyList<Lane> _lanes = NoLanes;

    /// <summary>Zoom, in logical px per second of timeline. Clamped to
    /// [<see cref="TimelineMetrics.MinPxPerSecond"/>,
    /// <see cref="TimelineMetrics.MaxPxPerSecond"/>] ON THE PROPERTY, so no caller
    /// can route around <see cref="ZoomIn"/>/<see cref="ZoomOut"/> and land the
    /// viewport on 0 (a division by zero) or on NaN.</summary>
    public double PxPerSecond
    {
        get => _pxPerSecond;
        set => _pxPerSecond = ClampPxPerSecond(value);
    }

    /// <summary>Horizontal scroll of the LANE AREA, in logical px. 0 = the time axis
    /// starts at t=0.</summary>
    public double ScrollXPx
    {
        get => _scrollXPx;
        set => _scrollXPx = Finite(value);
    }

    /// <summary>Vertical scroll of the lane stack, in logical px.</summary>
    public double ScrollYPx
    {
        get => _scrollYPx;
        set => _scrollYPx = Finite(value);
    }

    /// <summary>Width of the LANE AREA — the surface width MINUS
    /// <see cref="TimelineMetrics.TrackHeaderGutterWidth"/>. Set it through
    /// <see cref="SetSurfaceSize"/> so the subtraction happens in one place.</summary>
    public double ViewportWidthPx
    {
        get => _viewportWidthPx;
        set => _viewportWidthPx = Math.Max(0, Finite(value));
    }

    /// <summary>Height of the whole Timeline SURFACE (header + ruler + lane area),
    /// in logical px. The drawable lane band is <see cref="LaneAreaHeightPx"/>.</summary>
    public double ViewportHeightPx
    {
        get => _viewportHeightPx;
        set => _viewportHeightPx = Math.Max(0, Finite(value));
    }

    // ========================================================================
    // THE ONE LOGICAL <-> PHYSICAL BOUNDARY (D-14)
    // ========================================================================

    /// <summary>The surface's display scale (1.25 at 125%, 1.5 at 150%, …). The ONLY
    /// state in this directory that knows about physical pixels; nothing but the two
    /// conversion methods below may read it.</summary>
    public double RasterizationScale
    {
        get => _scale;
        set
        {
            var v = Finite(value);
            _scale = v > 0 ? v : 1.0;
        }
    }

    /// <summary>Logical px → physical (raw device) px. Call this at the boundary
    /// where a coordinate leaves the model — a swapchain size, a screen-scrape
    /// rectangle, a synthesized input point — and NEVER inside the model.</summary>
    public double LogicalToPhysical(double logicalPx) => Finite(logicalPx) * _scale;

    /// <summary>Physical (raw device) px → logical px. Call this at the boundary
    /// where a coordinate enters the model.</summary>
    public double PhysicalToLogical(double physicalPx) => Finite(physicalPx) / _scale;

    // ========================================================================
    // Lanes
    // ========================================================================

    /// <summary>The lane stack this viewport scrolls over — built by
    /// <see cref="LaneModel"/> from the mirrored tracks.</summary>
    public IReadOnlyList<Lane> Lanes => _lanes;

    public void SetLanes(IReadOnlyList<Lane>? lanes) => _lanes = lanes ?? NoLanes;

    /// <summary>Total height of the lane stack, in logical px.</summary>
    public double ContentHeightPx => LaneModel.ContentHeightPx(_lanes);

    /// <summary>The drawable lane band's height: the surface minus the sticky
    /// Timeline header and ruler (README.md:174 — both never scroll away).</summary>
    public double LaneAreaHeightPx => Math.Max(
        0, _viewportHeightPx - TimelineMetrics.TimelineHeaderHeight - TimelineMetrics.RulerHeight);

    /// <summary>Lane top in CONTENT y (before scroll), or 0 for an out-of-range
    /// index. Out-of-range is DATA, not an exception: a pointer handler can hold a
    /// lane index that is one mirror patch stale.</summary>
    public double LaneTopPx(int laneIndex) =>
        (uint)laneIndex < (uint)_lanes.Count ? _lanes[laneIndex].TopPx : 0;

    /// <summary>Lane height, or 0 for an out-of-range index (see
    /// <see cref="LaneTopPx"/>).</summary>
    public double LaneHeightPx(int laneIndex) =>
        (uint)laneIndex < (uint)_lanes.Count ? _lanes[laneIndex].HeightPx : 0;

    /// <summary>Does any part of this lane fall inside the scrolled lane band?
    /// Boundary-touching lanes count as visible.</summary>
    public bool IsLaneVisible(int laneIndex)
    {
        if ((uint)laneIndex >= (uint)_lanes.Count)
        {
            return false;
        }

        var lane = _lanes[laneIndex];
        var top = _scrollYPx;
        var bottom = top + LaneAreaHeightPx;
        return lane.BottomPx >= top && lane.TopPx <= bottom;
    }

    /// <summary>CONTENT y → lane index, or <c>null</c> when the point is above the
    /// first lane (e.g. it came from the ruler band) or past the last one.</summary>
    public int? PixelToLane(double contentY)
    {
        if (!double.IsFinite(contentY) || contentY < 0)
        {
            return null;
        }

        for (var i = 0; i < _lanes.Count; i++)
        {
            var lane = _lanes[i];
            if (contentY < lane.TopPx + lane.HeightPx)
            {
                return contentY >= lane.TopPx ? i : null;
            }
        }

        return null;
    }

    // ========================================================================
    // Surface <-> lane-area origins
    // ========================================================================

    /// <summary>Surface x → LANE-AREA x, by subtracting the 46px `TrackHeader`
    /// gutter (README.md:134). The gutter is not part of the time axis: a pointer at
    /// surface x=46 is at t=0, not at 1.15s.</summary>
    public double LaneAreaXFromSurfaceX(double surfaceX) =>
        Finite(surfaceX - TimelineMetrics.TrackHeaderGutterWidth);

    /// <summary>The inverse of <see cref="LaneAreaXFromSurfaceX"/>, so no renderer
    /// open-codes the offset.</summary>
    public double SurfaceXFromLaneAreaX(double laneAreaX) =>
        Finite(laneAreaX + TimelineMetrics.TrackHeaderGutterWidth);

    /// <summary>Surface y → CONTENT y: past the sticky header and ruler, with
    /// vertical scroll applied. Negative means "above the lane area" (the ruler or
    /// the Timeline header).</summary>
    public double LaneAreaYFromSurfaceY(double surfaceY) =>
        Finite(surfaceY - TimelineMetrics.TimelineHeaderHeight - TimelineMetrics.RulerHeight
               + _scrollYPx);

    /// <summary>The inverse of <see cref="LaneAreaYFromSurfaceY"/>, so no overlay
    /// open-codes the header/ruler offset or the vertical scroll — the same pairing
    /// <see cref="SurfaceXFromLaneAreaX"/> provides horizontally (phase 53.1's
    /// drop-preview ghost is the first consumer).</summary>
    public double SurfaceYFromLaneAreaY(double laneAreaY) =>
        Finite(laneAreaY + TimelineMetrics.TimelineHeaderHeight + TimelineMetrics.RulerHeight
               - _scrollYPx);

    /// <summary>Set both surface dimensions at once; the gutter subtraction happens
    /// HERE and nowhere else.</summary>
    public void SetSurfaceSize(double surfaceWidthPx, double surfaceHeightPx)
    {
        ViewportWidthPx = Finite(surfaceWidthPx) - TimelineMetrics.TrackHeaderGutterWidth;
        ViewportHeightPx = surfaceHeightPx;
    }

    // ========================================================================
    // pixel <-> time
    // ========================================================================

    /// <summary>Timeline time → LANE-AREA x, in logical px.</summary>
    public double TimeUsToPixel(long timeUs) => (timeUs / UsPerSecond * _pxPerSecond) - _scrollXPx;

    /// <summary>LANE-AREA x → timeline time. A non-finite x is "no answer", not a
    /// coordinate (T-52-13).</summary>
    public long PixelToTimeUs(double laneAreaX)
    {
        if (!double.IsFinite(laneAreaX))
        {
            return 0;
        }

        var seconds = (laneAreaX + _scrollXPx) / _pxPerSecond;
        var us = Math.Round(seconds * UsPerSecond, MidpointRounding.AwayFromZero);
        if (us >= long.MaxValue)
        {
            return long.MaxValue;
        }

        if (us <= long.MinValue)
        {
            return long.MinValue;
        }

        return (long)us;
    }

    /// <summary>First timeline time visible in the lane area.</summary>
    public long StartUs => PixelToTimeUs(0);

    /// <summary>Last timeline time visible in the lane area.</summary>
    public long EndUs => PixelToTimeUs(_viewportWidthPx);

    // ========================================================================
    // Zoom
    // ========================================================================

    /// <summary>Clamp and set px-per-second, leaving scroll alone.</summary>
    public void SetPxPerSecond(double pxPerSecond) => PxPerSecond = pxPerSecond;

    /// <summary>
    /// Zoom in one <see cref="TimelineMetrics.ZoomStep"/>, keeping the time under
    /// <paramref name="anchorLaneAreaX"/> exactly where it is.
    ///
    /// <para>The anchor is what makes Ctrl+scroll feel right: without it the clip
    /// under the cursor slides away as you zoom, which reads as a bug rather than as
    /// a zoom.</para>
    /// </summary>
    public void ZoomIn(double anchorLaneAreaX) =>
        SetPxPerSecondAnchored(_pxPerSecond * TimelineMetrics.ZoomStep, anchorLaneAreaX);

    /// <summary>Zoom out one step, anchored (see <see cref="ZoomIn"/>).</summary>
    public void ZoomOut(double anchorLaneAreaX) =>
        SetPxPerSecondAnchored(_pxPerSecond / TimelineMetrics.ZoomStep, anchorLaneAreaX);

    /// <summary>Set px-per-second while holding the time under
    /// <paramref name="anchorLaneAreaX"/> fixed.</summary>
    public void SetPxPerSecondAnchored(double pxPerSecond, double anchorLaneAreaX)
    {
        var anchor = double.IsFinite(anchorLaneAreaX) ? anchorLaneAreaX : 0;
        var secondsAtAnchor = (anchor + _scrollXPx) / _pxPerSecond;

        _pxPerSecond = ClampPxPerSecond(pxPerSecond);
        _scrollXPx = Finite((secondsAtAnchor * _pxPerSecond) - anchor);
    }

    private static double ClampPxPerSecond(double value)
    {
        if (!double.IsFinite(value) || value < TimelineMetrics.MinPxPerSecond)
        {
            return TimelineMetrics.MinPxPerSecond;
        }

        return value > TimelineMetrics.MaxPxPerSecond ? TimelineMetrics.MaxPxPerSecond : value;
    }

    /// <summary>
    /// NaN/±Infinity in, 0 out. Every public converter runs its RESULT through this,
    /// so a single bad coordinate cannot poison a whole frame's geometry silently
    /// (T-52-13).
    ///
    /// <para>0 is a real coordinate, so this is a last line of defence, not the
    /// first: <c>TimelineHitTester.Test</c> rejects a non-finite point outright
    /// BEFORE any conversion, which is where a garbage pointer is supposed to
    /// stop.</para>
    /// </summary>
    private static double Finite(double value) => double.IsFinite(value) ? value : 0;
}
