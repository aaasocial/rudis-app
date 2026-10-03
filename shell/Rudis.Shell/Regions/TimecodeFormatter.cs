namespace Rudis.Shell.Regions;

/// <summary>
/// Allocation-free <c>hh:mm:ss.mmm</c> formatting with a change-detection cache — the
/// hot path's text producer (UI-SPEC §4 rules 1 and 2).
///
/// <para><b>Two rules, both load-bearing for SC-3:</b></para>
/// <list type="number">
/// <item><b>No string interpolation, ever.</b> An interpolated timecode allocates a
///   string on EVERY tick. Digits go straight into a caller-supplied
///   <see cref="Span{T}"/> through <c>TryFormat</c>, which allocates nothing. (The
///   anti-pattern is deliberately not spelled out even as an example here: a grep for
///   interpolation over this file and <c>PlayheadTicker</c> must return ZERO, and a gate
///   that reddens on its own documentation teaches people to delete the documentation —
///   50-07 recorded that lesson from the other side.)</item>
/// <item><b>Change detection lives here, not at the call site.</b> The formatter
///   remembers the text it last rendered, so the caller can early-return and never touch
///   the visual tree for an unchanged value. At millisecond precision a PAUSED shell
///   changes zero times per second, which is what makes a strict <c>delta == 0</c>
///   assertion achievable rather than aspirational.</item>
/// </list>
///
/// <para>Deliberately free of WinUI types: <c>HotPathAllocationTests</c> and
/// <c>TimecodeFormatterTests</c> exercise it in a plain <c>net9.0-windows</c> host with no
/// window, no XAML and no composition tick.</para>
/// </summary>
internal sealed class TimecodeFormatter
{
    /// <summary>The shortest destination that can hold a rendered timecode:
    /// <c>hh:mm:ss.mmm</c> is 12 characters.</summary>
    public const int MinimumLength = 12;

    /// <summary>The internal buffer size, with room for hour counts past 99 so a very
    /// long project cannot overflow the format.</summary>
    public const int BufferLength = 20;

    /// <summary>UI-SPEC §5's empty state.</summary>
    public const string EmptyState = "00:00:00.000";

    private const long UsPerMs = 1_000;
    private const long MsPerSecond = 1_000;
    private const long MsPerMinute = 60 * MsPerSecond;
    private const long MsPerHour = 60 * MsPerMinute;

    /// <summary>The hot path's render target — allocated ONCE, reused forever.</summary>
    private readonly char[] _scratch = new char[BufferLength];

    /// <summary>The change-detection cache: the last text actually reported as a change.
    /// Kept SEPARATE from <see cref="_scratch"/> so <see cref="TryUpdate"/> can compare a
    /// fresh render against the previous one without a third copy.</summary>
    private readonly char[] _cache = new char[BufferLength];

    private int _scratchLength;
    private int _cacheLength;
    private bool _primed;

    /// <summary>The most recently rendered text, as a span over the formatter's OWN
    /// reusable buffer — never a new string.</summary>
    public ReadOnlySpan<char> Rendered => _scratch.AsSpan(0, _scratchLength);

    /// <summary>
    /// Format <paramref name="positionUs"/> into <paramref name="destination"/>.
    ///
    /// <para>Negatives clamp to zero, which covers the <c>i64::MIN</c> sentinel as a last
    /// line of defence: the ticker branches on the fault BEFORE any math (T-50-25), but a
    /// formatter that happily rendered a negative timecode would hide that bug instead of
    /// making it visible.</para>
    /// </summary>
    /// <returns><c>true</c> iff the rendered text DIFFERS from the last text this
    /// formatter rendered — i.e. iff the caller must touch the visual tree.</returns>
    public bool TryFormat(long positionUs, Span<char> destination, out int charsWritten)
    {
        charsWritten = Write(positionUs, destination);
        if (charsWritten == 0)
        {
            // Too small a destination: nothing written, and the cache is deliberately
            // left untouched so a failed call cannot suppress the next real change.
            return false;
        }

        var rendered = destination[..charsWritten];
        if (_primed
            && _cacheLength == charsWritten
            && rendered.SequenceEqual(_cache.AsSpan(0, _cacheLength)))
        {
            return false;
        }

        rendered.CopyTo(_cache);
        _cacheLength = charsWritten;
        _primed = true;
        return true;
    }

    /// <summary>The hot path's route: format into the internal buffer and report whether
    /// the rendered text changed. Read the result through <see cref="Rendered"/>.</summary>
    public bool TryUpdate(long positionUs)
    {
        var changed = TryFormat(positionUs, _scratch, out var written);
        if (written > 0)
        {
            _scratchLength = written;
        }
        return changed;
    }

    /// <summary>Drop the change cache so the next call reports a change even for an
    /// unchanged position (the cold path needs this after a duration or seek change).
    /// </summary>
    public void Invalidate() => _primed = false;

    /// <summary>
    /// The digits, and nothing else. <c>int.TryFormat</c> writes straight into the
    /// destination span with a constant standard format — no boxing, no intermediate
    /// string, no culture lookup that could produce non-ASCII digits (the format is
    /// invariant by construction because <c>D2</c>/<c>D3</c> on a non-negative int cannot
    /// vary by culture).
    /// </summary>
    /// <returns>Characters written, or 0 when <paramref name="destination"/> is too
    /// small.</returns>
    private static int Write(long positionUs, Span<char> destination)
    {
        if (destination.Length < MinimumLength)
        {
            return 0;
        }

        var totalMs = (positionUs > 0 ? positionUs : 0) / UsPerMs;
        var hours = (int)(totalMs / MsPerHour);
        var minutes = (int)(totalMs % MsPerHour / MsPerMinute);
        var seconds = (int)(totalMs % MsPerMinute / MsPerSecond);
        var milliseconds = (int)(totalMs % MsPerSecond);

        var written = 0;
        if (!hours.TryFormat(destination, out var n, "D2"))
        {
            return 0;
        }
        written += n;
        destination[written++] = ':';
        if (!minutes.TryFormat(destination[written..], out n, "D2"))
        {
            return 0;
        }
        written += n;
        destination[written++] = ':';
        if (!seconds.TryFormat(destination[written..], out n, "D2"))
        {
            return 0;
        }
        written += n;
        destination[written++] = '.';
        if (!milliseconds.TryFormat(destination[written..], out n, "D3"))
        {
            return 0;
        }
        return written + n;
    }
}
