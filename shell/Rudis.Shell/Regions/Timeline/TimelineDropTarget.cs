namespace Rudis.Shell.Regions;

/// <summary>
/// A drop point on the Timeline surface, resolved to the (track, time) pair
/// <c>rudis_place_clip</c> needs.
///
/// <para><b>This class COMPOSES <see cref="TimelineViewport"/>; it computes nothing
/// of its own.</b> That is the whole point. <c>OnSurfacePointerPressed</c> already
/// turns a surface point into a lane and a time through exactly these four calls,
/// and a drop needs the identical conversion — only the event source differs. A
/// second pixel arithmetic here would be a second place for the D-14
/// logical-vs-physical lesson to be re-broken, which this project has already paid
/// for four times (50-05, 50-06, 52-01, and the remark at the top of
/// <see cref="TimelineViewport"/> itself).</para>
///
/// <para>No WinUI types, by rule (D-13) — this file lives in the hot-path directory
/// and <c>TimelineHotPathGateTests</c> scans it. The drag event's args are unwrapped
/// by the caller in <c>Regions/Timeline.xaml.cs</c> (a FILE beside this directory)
/// and only two <see langword="double"/>s cross into here, which is also what makes
/// every case unit-testable with no window.</para>
/// </summary>
internal static class TimelineDropTarget
{
    /// <summary>A media id longer than this is not an id — it is a foreign drag
    /// payload (T-53.1-06). Refuse it before it crosses the FFI boundary rather
    /// than after.</summary>
    internal const int MaxMediaIdLength = 256;

    /// <summary>One resolved drop point. <see cref="OnLane"/> false means there is
    /// nothing under the pointer to place onto, and the other fields are not
    /// answers. <see cref="LaneIndex"/> is the VIEWPORT lane (geometry — feeds the
    /// drop-preview ghost's y/height), distinct from <see cref="TrackIndex"/> (the
    /// engine's track — feeds <c>rudis_place_clip</c>); conflating the two is exactly
    /// the off-by-a-collapsed-lane class of bug the lane model exists to prevent.</summary>
    internal readonly record struct DropPoint(bool OnLane, int TrackIndex, long StartUs, int LaneIndex);

    /// <summary>
    /// Surface-relative logical px → the lane's track index and the timeline time
    /// under the point. <see cref="DropPoint.OnLane"/> false means the point is over
    /// the ruler, the Timeline header, or past the last lane — a drop there places
    /// nothing.
    /// </summary>
    internal static DropPoint Resolve(TimelineViewport viewport, double surfaceX, double surfaceY)
    {
        // A garbage coordinate stops HERE, before any conversion — TimelineHitTester.Test's
        // own posture, matched deliberately (T-52-13). Letting one through would make every
        // comparison below false and report "no lane" for the wrong reason.
        if (viewport is null || !double.IsFinite(surfaceX) || !double.IsFinite(surfaceY))
        {
            return default;
        }

        // The region's own toolbar band and the sticky ruler are NOT droppable, and
        // that is checked HERE, on surface y, rather than being left to PixelToLane's
        // negative-contentY rejection below.
        //
        // The difference is only visible once the lane stack is scrolled, which is
        // exactly why it is easy to miss: LaneAreaYFromSurfaceY ADDS ScrollYPx, so at
        // any ScrollYPx > 0 a ruler-band point converts to a POSITIVE content y and
        // lands on a real lane. Relying on the sign alone would make the scrub bar a
        // live drop target on every scrolled timeline — a clip placed where the user
        // meant to scrub. TimelineHitTester.Test rejects these two bands before
        // converting for the same reason (its steps 3 and 4); this is that rule, not
        // a second one.
        if (surfaceY < TimelineMetrics.TimelineHeaderHeight + TimelineMetrics.RulerHeight)
        {
            return default;
        }

        var contentY = viewport.LaneAreaYFromSurfaceY(surfaceY);
        if (viewport.PixelToLane(contentY) is not int laneIndex)
        {
            return default;
        }

        if ((uint)laneIndex >= (uint)viewport.Lanes.Count)
        {
            return default;
        }

        var laneAreaX = viewport.LaneAreaXFromSurfaceX(surfaceX);
        var startUs = viewport.PixelToTimeUs(laneAreaX);

        // A drop in the `TrackHeader` gutter is a drop at t=0, never at a negative
        // time: `rudis_place_clip` would refuse a negative start, and the user's
        // intent over the gutter ("the very beginning") is unambiguous. The gutter's
        // WIDTH is not restated here — LaneAreaXFromSurfaceX owns it.
        return new DropPoint(true, viewport.Lanes[laneIndex].TrackIndex, Math.Max(0, startUs), laneIndex);
    }

    /// <summary>
    /// Is this string shaped like a media id at all? The ONLY client-side check on
    /// the payload, and it is deliberately NOT a compatibility check: whether the
    /// media may live on that track is the BACKEND's rule
    /// (<c>crates/app-core/src/place.rs</c>), and duplicating it here is the exact
    /// anti-pattern CLAUDE.md rule 4 exists to prevent.
    ///
    /// <para>What this DOES refuse is a foreign drag payload — text dragged from
    /// another application arrives as <c>StandardDataFormats.Text</c> exactly like a
    /// MediaBin drag does, because an OLE drag carries no sender identity
    /// (T-53.1-06). An oversized or blank string is not an id and must not cross the
    /// FFI boundary; a plausible-but-wrong one is refused by the backend's own
    /// "media bin item not found" lookup, which is where that decision belongs.</para>
    /// </summary>
    internal static bool IsPlaceableMediaId(string? mediaId) =>
        !string.IsNullOrWhiteSpace(mediaId) && mediaId!.Length <= MaxMediaIdLength;

    /// <summary>The refusal text, formatted in ONE place so the UIA proof can assert
    /// the exact string the user sees. Mirrors <c>DispatchAsync</c>'s
    /// <c>$"{command.Kind} refused: {result.Error}"</c> shape, and passes the
    /// backend's own words through verbatim rather than rewording them.</summary>
    internal static string RefusalMessage(string? backendError) =>
        $"place_clip refused: {backendError}";
}
