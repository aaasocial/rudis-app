namespace Rudis.Shell.Regions;

/// <summary>
/// `Timeline › Ruler`'s graduations (README.md:133): pick an interval, then list the
/// tick times inside the visible window and no others.
///
/// <para><b>The rule:</b> the SMALLEST entry of
/// <see cref="TimelineMetrics.RulerTickLadderSeconds"/> whose labels would sit at
/// least <see cref="TimelineMetrics.MinTickLabelSpacingPx"/> apart wins. That makes
/// the graduation density a function of zoom alone, and it is why the handoff's
/// `0:00 0:05 0:10` example is reproducible instead of eyeballed.</para>
///
/// <para><b>The buffer overload is the real one.</b> <c>Select(v, into)</c> fills a
/// caller-owned <see cref="List{T}"/> so a redraw allocates nothing;
/// <c>Select(v)</c> exists for cold-path callers and tests and DOES allocate. The
/// zero-allocation claim is a measured number, not an intention — see
/// <c>TimelineHotPathGateTests.ruler_tick_selection_allocates_zero_when_given_a_buffer</c>.</para>
///
/// <para>No WinUI types, by rule (D-13).</para>
/// </summary>
internal static class RulerTicks
{
    /// <summary>
    /// A hard cap on ticks per selection. With a 60px label floor a 4K-wide lane area
    /// yields ~64 ticks, so this is a BOUND, not a budget: it exists so that a
    /// nonsense viewport width (or a future caller that forgets to clamp one) costs a
    /// bounded loop instead of an unbounded one on the redraw path — the same
    /// DoS-cap discipline `audio_sync.rs::SYNC_MAX_WINDOW_US` established backend-side.
    /// </summary>
    public const int MaxTicks = 1024;

    private const long UsPerSecond = 1_000_000L;

    /// <summary>The ladder rule, on its own, so it is testable without a viewport.</summary>
    public static long SelectIntervalUs(double pxPerSecond)
    {
        var pps = double.IsFinite(pxPerSecond) && pxPerSecond > 0
            ? pxPerSecond
            : TimelineMetrics.MinPxPerSecond;

        var ladder = TimelineMetrics.RulerTickLadderSeconds;
        for (var i = 0; i < ladder.Length; i++)
        {
            if (ladder[i] * pps >= TimelineMetrics.MinTickLabelSpacingPx)
            {
                return ladder[i] * UsPerSecond;
            }
        }

        // Unreachable while MinPxPerSecond * the ladder's last entry clears the floor
        // (1 px/s × 600s = 600px), but the fallback is stated rather than assumed.
        return ladder[ladder.Length - 1] * UsPerSecond;
    }

    /// <summary>
    /// Fill <paramref name="into"/> with every tick time inside
    /// <c>[v.StartUs, v.EndUs]</c> and return the chosen interval. The buffer is
    /// cleared and reused; nothing is allocated once it has grown once.
    /// </summary>
    public static long Select(TimelineViewport viewport, List<long> into)
    {
        ArgumentNullException.ThrowIfNull(into);
        into.Clear();

        if (viewport is null)
        {
            return SelectIntervalUs(TimelineMetrics.DefaultPxPerSecond);
        }

        var intervalUs = SelectIntervalUs(viewport.PxPerSecond);
        var startUs = viewport.StartUs;
        var endUs = viewport.EndUs;
        if (endUs < startUs)
        {
            return intervalUs;
        }

        var tick = FloorDiv(startUs, intervalUs) * intervalUs;
        if (tick < startUs)
        {
            tick += intervalUs;
        }

        while (tick <= endUs && into.Count < MaxTicks)
        {
            into.Add(tick);
            if (tick > long.MaxValue - intervalUs)
            {
                break;
            }

            tick += intervalUs;
        }

        return intervalUs;
    }

    /// <summary>The allocating convenience form — cold path and tests only.</summary>
    public static (long IntervalUs, IReadOnlyList<long> TickUs) Select(TimelineViewport viewport)
    {
        var ticks = new List<long>();
        var intervalUs = Select(viewport, ticks);
        return (intervalUs, ticks);
    }

    /// <summary>Floor division for signed values — <c>/</c> truncates toward zero,
    /// which would snap a NEGATIVE window start to the wrong side of its interval
    /// (scrolling left of t=0 is legal).</summary>
    private static long FloorDiv(long a, long b)
    {
        var quotient = a / b;
        if (a % b != 0 && (a < 0) != (b < 0))
        {
            quotient--;
        }

        return quotient;
    }
}
