namespace Rudis.Shell.Regions;

/// <summary>What a point on the Timeline surface turned out to be.</summary>
internal enum HitKind
{
    /// <summary>Nothing addressable: the region's own header band, the `TrackHeader`
    /// gutter's corner cell under the ruler, or empty space below the lane
    /// stack.</summary>
    None,

    /// <summary>`Timeline › Ruler` — click/drag scrubs (README.md:133).</summary>
    Ruler,

    /// <summary>Within grabbing distance of `Timeline.Playhead`.</summary>
    Playhead,

    /// <summary>The interior of a clip: a drag from here MOVES it.</summary>
    ClipBody,

    /// <summary>A `Clip.TrimHandle`: a drag from here changes in/out.</summary>
    TrimHandle,

    /// <summary>A lane, but no clip at that time — a click clears the selection.</summary>
    EmptyLane,

    /// <summary>The 46px `TrackHeader` gutter — selects the LANE, not a clip.
    ///
    /// <para>The route by which an EMPTY track can be named at all: the remove-track
    /// gesture reads a selection, and a track with no clips in it has nothing else
    /// that could carry one (quick 260731-k9b, closing 52-14's recorded boundary).
    /// <c>LaneIndex</c> is the lane; <c>ClipId</c> is always <c>null</c>.</para></summary>
    TrackHeader,
}

/// <summary>Which edge of a clip a <see cref="HitKind.TrimHandle"/> belongs to.</summary>
internal enum TrimEdge
{
    None,
    Left,
    Right,
}

/// <summary>One resolved point. A struct: the pointer-move path produces one of
/// these per event and must allocate nothing.</summary>
internal readonly record struct HitResult(HitKind Kind, string? ClipId, TrimEdge Edge, int LaneIndex)
{
    /// <summary>Nothing addressable, on no lane.</summary>
    public static readonly HitResult Miss = new(HitKind.None, null, TrimEdge.None, -1);
}

/// <summary>
/// Point → data resolution for a surface that has no per-clip controls (D-10).
///
/// <para>SHELL-05 makes the Timeline custom-drawn, so <c>PointerPressed</c> arrives
/// once, on the whole surface, with a single <c>(x, y)</c>. There is nothing to hit
/// test against: this class IS the hit test, resolving that point against the
/// CULLED clip array — never against the whole project (T-52-11).</para>
///
/// <para><b>THE ORDER IS THE DESIGN. Read it before changing it:</b></para>
/// <list type="number">
/// <item>a non-finite coordinate is no coordinate at all (T-52-13);</item>
/// <item><b>the 46px `TrackHeader` gutter resolves to its LANE</b>
///   (<see cref="HitKind.TrackHeader"/>) — never to a clip and never to a time,
///   because the gutter is not part of the time axis. It is checked FIRST-but-one
///   for exactly that reason: a gutter point can never be a clip point, so nothing
///   below can have a better claim to it and no tie-break arises. Its corner cell
///   under the ruler, and any gutter point past the last lane, are still
///   <see cref="HitKind.None"/>;</item>
/// <item>the region's own header band belongs to `Timeline › Toolbar`, a XAML
///   control, not to this surface;</item>
/// <item>the ruler is sticky and scrubbing beats anything drawn below it;</item>
/// <item>the playhead's grab zone beats a clip body underneath it — otherwise the
///   playhead becomes ungrabbable exactly where clips are, which is everywhere the
///   user wants it;</item>
/// <item><b>RIGHT trim handles, then LEFT trim handles, then bodies.</b></item>
/// <item>a lane with no clip at that point.</item>
/// </list>
///
/// <para><b>Why handles are checked before the body (D-10):</b> the two zones
/// overlap by construction — a handle IS the outer 6px of the clip's rectangle — so
/// whichever is tested first wins. Reverse them and every edge grab becomes a body
/// drag: trimming never starts, and the bug presents as "the trim handles don't
/// work" with nothing in any log.</para>
///
/// <para><b>Why RIGHT handles come before LEFT ones:</b> two clips that touch share
/// a boundary pixel that belongs to BOTH the left clip's right handle and the right
/// clip's left handle. Testing them in one interleaved pass would let the mirror's
/// array order decide — and that order changes whenever a clip is added, split or
/// moved, so the same seam would answer differently depending on the project's edit
/// history. Splitting the passes makes the tie-break a stated rule instead:
/// <b>at a shared boundary the LEFT clip's RIGHT handle wins.</b></para>
///
/// <para>Zero allocations: struct result, indexed <c>for</c> over the caller's
/// buffer, no query operators, no closures. No WinUI types, by rule (D-13).</para>
/// </summary>
internal static class TimelineHitTester
{
    /// <summary>Extra grab distance either side of the 2px playhead line, so a
    /// 2-pixel target is actually hittable with a mouse.</summary>
    public const double PlayheadGrabSlopPx = 3;

    /// <summary>
    /// The trim-handle width for a clip <paramref name="clipWidthPx"/> wide.
    ///
    /// <para>Handles shrink to a third of the clip on anything narrower than
    /// <c>3 × TrimHandleWidth</c>. Without that, two 6px handles on a 12px clip meet
    /// in the middle and leave no body zone — the clip could be trimmed but never
    /// MOVED, and a zoomed-out timeline is full of such clips.</para>
    /// </summary>
    public static double HandleWidthFor(double clipWidthPx)
    {
        if (!double.IsFinite(clipWidthPx) || clipWidthPx <= 0)
        {
            return 0;
        }

        var third = clipWidthPx / 3.0;
        return third < TimelineMetrics.TrimHandleWidth ? third : TimelineMetrics.TrimHandleWidth;
    }

    /// <summary>
    /// Resolve a SURFACE point (origin at the Timeline region's top-left, LOGICAL
    /// px) against the culled clip array.
    /// </summary>
    public static HitResult Test(
        TimelineViewport? viewport,
        IReadOnlyList<ClipLayout>? culled,
        long playheadUs,
        double xSurface,
        double ySurface)
    {
        // 1. A NaN or an infinity is not a coordinate (T-52-13). Letting one through
        //    would make every comparison below false and report "empty" from deep
        //    inside the clip loop: the right answer for the wrong reason, which would
        //    mask the upstream fault that produced it.
        if (viewport is null || !double.IsFinite(xSurface) || !double.IsFinite(ySurface))
        {
            return HitResult.Miss;
        }

        // 2. The `TrackHeader` gutter labels lanes; it is not part of the time axis,
        //    and its corner cell under the ruler is not the ruler either. A point in
        //    it names the LANE it sits beside — the only handle a user has on a track
        //    that holds no clips (quick 260731-k9b).
        if (xSurface < TimelineMetrics.TrackHeaderGutterWidth)
        {
            // The band check is made HERE and not left to PixelToLane's own
            // "above the first lane" answer, because vertical scroll would defeat
            // that: at ScrollYPx = 100 a point in the ruler band converts to a
            // POSITIVE content y and would resolve to a real lane. The gutter's
            // corner cell must stay a Miss at every scroll position.
            if (ySurface < TimelineMetrics.TimelineHeaderHeight + TimelineMetrics.RulerHeight)
            {
                return HitResult.Miss;
            }

            var gutterLane = viewport.PixelToLane(viewport.LaneAreaYFromSurfaceY(ySurface));
            return gutterLane is int gutterLaneIndex
                ? new HitResult(HitKind.TrackHeader, null, TrimEdge.None, gutterLaneIndex)
                : HitResult.Miss;
        }

        // 3. `Timeline › Toolbar` is an ordinary XAML control sitting above this
        //    surface.
        if (ySurface < TimelineMetrics.TimelineHeaderHeight)
        {
            return HitResult.Miss;
        }

        // 4. The ruler is sticky (README.md:174) and scrubbing beats everything under
        //    it.
        if (ySurface < TimelineMetrics.TimelineHeaderHeight + TimelineMetrics.RulerHeight)
        {
            return new HitResult(HitKind.Ruler, null, TrimEdge.None, -1);
        }

        var x = viewport.LaneAreaXFromSurfaceX(xSurface);
        var contentY = viewport.LaneAreaYFromSurfaceY(ySurface);

        // 5. The playhead spans every lane, so it is resolved before any clip — but
        //    only inside the lane stack, where it is actually drawn.
        if (contentY >= 0 && contentY < viewport.ContentHeightPx)
        {
            var playheadX = viewport.TimeUsToPixel(playheadUs);
            var distance = x - playheadX;
            if (distance < 0)
            {
                distance = -distance;
            }

            if (distance <= TimelineMetrics.PlayheadLineWidth + PlayheadGrabSlopPx)
            {
                return new HitResult(HitKind.Playhead, null, TrimEdge.None, -1);
            }
        }

        var lane = viewport.PixelToLane(contentY);
        if (lane is not int laneIndex)
        {
            return HitResult.Miss;
        }

        if (culled is null)
        {
            return new HitResult(HitKind.EmptyLane, null, TrimEdge.None, laneIndex);
        }

        var count = culled.Count;

        // 6a. RIGHT handles first — see the class summary for why this pass exists
        //     separately from 6b rather than being folded into it.
        for (var i = 0; i < count; i++)
        {
            var clip = culled[i];
            if (clip.LaneIndex != laneIndex)
            {
                continue;
            }

            var left = viewport.TimeUsToPixel(clip.StartUs);
            var right = viewport.TimeUsToPixel(clip.EndUs);
            var handle = HandleWidthFor(right - left);
            if (handle > 0 && x > right - handle && x <= right)
            {
                return new HitResult(HitKind.TrimHandle, clip.Id, TrimEdge.Right, laneIndex);
            }
        }

        // 6b. LEFT handles.
        for (var i = 0; i < count; i++)
        {
            var clip = culled[i];
            if (clip.LaneIndex != laneIndex)
            {
                continue;
            }

            var left = viewport.TimeUsToPixel(clip.StartUs);
            var right = viewport.TimeUsToPixel(clip.EndUs);
            var handle = HandleWidthFor(right - left);
            if (handle > 0 && x >= left && x < left + handle)
            {
                return new HitResult(HitKind.TrimHandle, clip.Id, TrimEdge.Left, laneIndex);
            }
        }

        // 6c. Bodies, LAST — never before the two handle passes above (D-10).
        for (var i = 0; i < count; i++)
        {
            var clip = culled[i];
            if (clip.LaneIndex != laneIndex)
            {
                continue;
            }

            var left = viewport.TimeUsToPixel(clip.StartUs);
            var right = viewport.TimeUsToPixel(clip.EndUs);
            if (right > left && x >= left && x <= right)
            {
                return new HitResult(HitKind.ClipBody, clip.Id, TrimEdge.None, laneIndex);
            }
        }

        // 7. A lane, but nothing on it here.
        return new HitResult(HitKind.EmptyLane, null, TrimEdge.None, laneIndex);
    }
}
