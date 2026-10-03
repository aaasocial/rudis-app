namespace Rudis.Shell.Regions;

/// <summary>
/// The visual-tree side of the hot path, kept behind an interface so the ticker itself is
/// WinUI-free and the allocation tests can run headless (plan 50-06 Task 2 requirement 4).
/// </summary>
internal interface IPlayheadReadout
{
    /// <summary>Render the timecode. The span is a view over the formatter's reusable
    /// buffer — copy it if you must keep it, and expect to be called ONLY when the
    /// rendered value actually changed.</summary>
    void RenderTimecode(ReadOnlySpan<char> text);

    /// <summary>Move the playhead. Implementations move a transform, never re-create
    /// geometry (UI-SPEC §4 rule 3).</summary>
    void RenderPlayhead(long positionUs, long durationUs);

    /// <summary>The position source reported a fault. Called AT MOST ONCE per
    /// ticker.</summary>
    void SurfaceFault();
}

/// <summary>
/// The composition-tick consumer: the ONLY per-tick ABI call in the shell.
///
/// <para>Fills the consumer slot 50-04 left at <c>MainWindow.OnRendering</c>. Per tick it
/// does exactly one scalar read of <c>rudis_get_playback_position()</c> — a lock-free
/// Relaxed atomic load, the one free-threaded member of the interop wrapper — feeds the
/// formatter's change detector, and touches the visual tree ONLY when the RENDERED value
/// changed (UI-SPEC §4 rule 1). Everything here is SC-3 code: no allocation, no
/// interpolation, no boxing, no LINQ, no event dispatch.</para>
///
/// <para><b>The fault sentinel is not a position (T-50-25).</b>
/// <c>rudis_get_playback_position</c> answers <c>i64::MIN</c> for a null handle or a
/// caught panic, and the interop wrapper maps that — plus every disposed/invalid-handle
/// case — to <c>null</c>. A null is therefore a FAULT SIGNAL: it is surfaced exactly once
/// and the readout stops, rather than rendering a nonsense playhead or spinning a log.
/// A real position is clamped to <c>[0, duration]</c> by the engine and can never be the
/// sentinel, so the two can never be confused.</para>
///
/// <para><b>There is no clock here.</b> The position MOVES because the engine's own tick
/// thread moves it (D-17 / UI-SPEC §7 — <c>self_advance</c>, opted into in
/// <c>App.BuildInitConfigJson</c>). This type only reads. If it ever gains a timer, the
/// shell has re-acquired the per-frame clock SHELL-06 exists to remove.</para>
/// </summary>
internal sealed class PlayheadTicker(Func<long?> positionSource, IPlayheadReadout readout)
{
    private readonly Func<long?> _positionSource = positionSource;
    private readonly IPlayheadReadout _readout = readout;
    private readonly TimecodeFormatter _formatter = new();

    private long _tickCount;
    private long _renderCount;
    private bool _faulted;
    private long? _lastPositionUs;

    /// <summary>Duration in µs, from the mirror (cold path).</summary>
    public long DurationUs { get; set; }

    /// <summary>While true the user owns the playhead and the ticker does nothing.</summary>
    public bool IsScrubbing { get; set; }

    public long TickCount => _tickCount;

    /// <summary>How many ticks actually re-rendered. The gap between this and
    /// <see cref="TickCount"/> is the change-detection cache's whole value.</summary>
    public long RenderCount => _renderCount;

    /// <summary>Set once, permanently, when the position source reports a fault.</summary>
    public bool Faulted => _faulted;

    /// <summary>The last position actually read; null before the first tick.</summary>
    public long? LastPositionUs => _lastPositionUs;

    /// <summary>
    /// ⚠ HOT PATH. One scalar read, one change check, and work ONLY on a rendered change.
    ///
    /// <para>Ordering is deliberate: the fault branch comes FIRST (a sentinel must never
    /// reach any arithmetic), the scrub guard second (the user's drag wins over the
    /// engine — frontend parity, main.ts:670), and only then the change detector.</para>
    /// </summary>
    public void Tick()
    {
        _tickCount++;
        if (_faulted)
        {
            // One-shot: the readout has already been told, and re-reading a dead handle
            // every frame would be a spin, not a diagnostic.
            return;
        }

        var position = _positionSource();
        if (position is null)
        {
            _faulted = true;
            _readout.SurfaceFault();
            return;
        }

        if (IsScrubbing)
        {
            return;
        }

        var positionUs = position.GetValueOrDefault();
        _lastPositionUs = positionUs;
        if (!_formatter.TryUpdate(positionUs))
        {
            // The rendered value is unchanged, so the correct amount of work is NONE:
            // no text set, no transform move, and NO UIA property-change notification
            // (UI-SPEC §6 — a per-tick notification is both an allocation source and an
            // event storm, and plan 50-08 measures GC-delta-0 with a UIA client attached).
            return;
        }

        _renderCount++;
        _readout.RenderTimecode(_formatter.Rendered);
        _readout.RenderPlayhead(positionUs, DurationUs);
    }

    /// <summary>Force the next tick to re-render (cold-path state changed).</summary>
    public void Invalidate() => _formatter.Invalidate();
}
