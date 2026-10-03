namespace Rudis.Shell.Regions;

/// <summary>
/// The Timeline's two time-to-text formats, both written into caller-owned buffers so
/// neither allocates on the redraw path.
///
/// <para><b>Why the Timeline's timecode differs from Transport's, deliberately.</b>
/// <c>Transport.Timecode</c> (50-06) renders <c>hh:mm:ss.mmm</c> — millisecond
/// precision, because the transport is where a user reads a PLAYBACK position and the
/// engine's clock is microsecond-accurate. The design handoff gives the Timeline
/// <c>HH:MM:SS:FF</c> (<c>00:00:12:08</c>, README:132 and :265), i.e. FRAMES, because
/// the Timeline is where a user reasons about EDITS and an edit lands on a frame
/// boundary. Two different questions, two different formats — recorded here so the
/// difference reads as a decision rather than as an inconsistency.</para>
///
/// <para>No WinUI types, by rule (D-13).</para>
/// </summary>
internal static class TimelineTimecode
{
    private const long UsPerSecond = 1_000_000L;

    /// <summary>
    /// The largest magnitude either formatter will DISPLAY: 999 hours (41.6 days).
    ///
    /// <para><b>Why a clamp exists at all (52-REVIEW CR-01).</b> Both formatters write
    /// into a FIXED-SIZE caller buffer, and the hour field is the only one whose width
    /// is not bounded by its own arithmetic — minutes and seconds are always two digits
    /// and the frame index is bounded by the validated fps, but <c>hours</c> is whatever
    /// the incoming microsecond count divides down to. At <c>long.MaxValue</c> that is
    /// TEN digits, which is seven wider than the buffers were sized for, and the value
    /// arriving out of range needs no user gesture: a unit-conversion bug elsewhere, a
    /// hand-edited <c>.rud</c>, or any future writer of <c>Clip.OutUs</c> is enough.
    /// Clamping here bounds the WIDTH of every field before a single character is
    /// written, which is what makes <see cref="FrameTimecodeMaxChars"/> a real bound
    /// rather than an assumption. The writers below are independently bounds-checked as
    /// well, so removing this clamp would degrade the display, never corrupt or
    /// crash — belt and braces, deliberately, because this sits on the composition
    /// tick and on the first pointer-move of every drag.</para>
    ///
    /// <para>Saturating rather than throwing is the posture the rest of this codebase
    /// already takes for out-of-range domain values (<c>ClipLayout.EndUs</c>,
    /// <c>crates/waveform</c>'s cache bounds): an honest, obviously-pegged readout beats
    /// an unhandled <c>IndexOutOfRangeException</c> on the UI thread.</para>
    /// </summary>
    public const long MaxDisplayableUs = 999L * 3600L * UsPerSecond;

    /// <summary>
    /// Widest output of <see cref="TryWriteFrameTimecode"/>, and therefore also the
    /// SMALLEST buffer it will write into: <c>HHH:MM:SS:FFF</c>.
    ///
    /// <para>Derived, not guessed: three hour digits (<see cref="MaxDisplayableUs"/>
    /// caps hours at 999), two each for minutes and seconds, three frame digits (fps is
    /// validated to <c>&lt;= 1000</c>, so the frame index inside a second cannot exceed
    /// 999), plus three separators = 13. <c>TimelineTimecodeBoundsTests</c> sweeps the
    /// input space and asserts this number is never exceeded and never short.</para>
    ///
    /// <para>The precheck compares against THIS, not against the 11 an
    /// <c>HH:MM:SS:FF</c> happy path needs. A minimum that is narrower than the
    /// worst case is not a bound — it is the bug CR-01 named.</para>
    /// </summary>
    public const int FrameTimecodeMaxChars = 13;

    /// <summary>Widest output of <see cref="WriteRulerLabel"/> in UTF-8 bytes.
    ///
    /// <para>The true worst case after the <see cref="MaxDisplayableUs"/> clamp is 10
    /// (<c>-HHH:MM:SS</c>); 16 is kept because it is also the per-tick stride of
    /// <c>TimelineFrameBuilder</c>'s label arena, and the slack costs one array
    /// sizing rather than anything per frame.</para></summary>
    public const int RulerLabelMaxBytes = 16;

    /// <summary>
    /// Frames-per-second used when the mirror carries none.
    ///
    /// <para>Stated as a named constant rather than buried in an expression because it
    /// is a GUESS, and a guess that shows on screen should be findable. It bites only
    /// before any media is loaded, when the readout reads <c>00:00:00:00</c> and the
    /// frame field is therefore correct at any rate.</para>
    /// </summary>
    public const double FallbackFps = 30.0;

    /// <summary>
    /// <c>HH:MM:SS:FF</c> into <paramref name="dest"/> — the handoff's Timeline
    /// timecode. Returns false (writing nothing) if the buffer is too small.
    /// </summary>
    /// <param name="timeUs">Timeline position. Negative clamps to zero: a negative
    /// position is a fault signal elsewhere and must never render as a time.</param>
    /// <param name="fps">Project frame rate. Non-finite or out of range falls back to
    /// <see cref="FallbackFps"/>.</param>
    public static bool TryWriteFrameTimecode(
        long timeUs, double fps, Span<char> dest, out int written)
    {
        written = 0;
        if (dest.Length < FrameTimecodeMaxChars)
        {
            return false;
        }

        if (timeUs < 0)
        {
            timeUs = 0;
        }

        if (timeUs > MaxDisplayableUs)
        {
            timeUs = MaxDisplayableUs;
        }

        var rate = double.IsFinite(fps) && fps >= 1.0 && fps <= 1000.0 ? fps : FallbackFps;

        var totalSeconds = timeUs / UsPerSecond;
        var subSecondUs = timeUs % UsPerSecond;

        var hours = totalSeconds / 3600;
        var minutes = (totalSeconds / 60) % 60;
        var seconds = totalSeconds % 60;

        // The frame INDEX inside the current second, floored — the same convention a
        // frame counter uses, so the last frame of a second is rate-1 and never rate.
        var frames = (long)(subSecondUs * rate / UsPerSecond);
        var lastFrame = (long)Math.Ceiling(rate) - 1;
        if (frames > lastFrame)
        {
            frames = lastFrame;
        }
        if (frames < 0)
        {
            frames = 0;
        }

        var at = 0;
        var ok = TryWriteTwo(dest, ref at, hours)
            && TryWriteChar(dest, ref at, ':')
            && TryWriteTwo(dest, ref at, minutes)
            && TryWriteChar(dest, ref at, ':')
            && TryWriteTwo(dest, ref at, seconds)
            && TryWriteChar(dest, ref at, ':')
            && TryWriteTwo(dest, ref at, frames);

        // Unreachable given the clamp above and the precheck (the sweep in
        // TimelineTimecodeBoundsTests proves it), and kept anyway: the SECOND half of
        // CR-01's fix is that no arithmetic mistake upstream can turn this function
        // into a throw. A refused format renders as "no change this tick"; a throw on
        // the composition tick is a process exit.
        if (!ok)
        {
            written = 0;
            return false;
        }

        written = at;
        return true;
    }

    /// <summary>
    /// A ruler graduation label as UTF-8 ASCII: <c>M:SS</c> below an hour (the
    /// handoff's own <c>0:00 0:05 0:10</c> form, README:133), <c>H:MM:SS</c> above it.
    /// Returns the byte count written, or 0 if the buffer is too small.
    ///
    /// <para>Written as BYTES rather than as a string because these labels are
    /// re-derived on every redraw as the viewport scrolls, so there is nothing stable
    /// to cache them by — unlike a clip label, which is keyed by its own filename.
    /// Encoding straight into the frame's arena keeps the redraw path at zero
    /// allocations either way.</para>
    /// </summary>
    public static int WriteRulerLabel(long timeUs, Span<byte> dest)
    {
        if (dest.Length < RulerLabelMaxBytes)
        {
            return 0;
        }

        // Clamp BEFORE negating: `-long.MinValue` is still `long.MinValue`, so negating
        // first would push a negative straight through the digit writers and print a
        // string of punctuation. Both bounds are inside `long`, so the negation below
        // cannot overflow.
        if (timeUs > MaxDisplayableUs)
        {
            timeUs = MaxDisplayableUs;
        }
        else if (timeUs < -MaxDisplayableUs)
        {
            timeUs = -MaxDisplayableUs;
        }

        var negative = timeUs < 0;
        if (negative)
        {
            timeUs = -timeUs;
        }

        var totalSeconds = timeUs / UsPerSecond;
        var hours = totalSeconds / 3600;
        var minutes = (totalSeconds / 60) % 60;
        var seconds = totalSeconds % 60;

        var at = 0;
        var ok = true;
        if (negative)
        {
            ok = TryWriteByte(dest, ref at, (byte)'-');
        }

        if (hours > 0)
        {
            ok = ok
                && TryWriteUnpaddedBytes(dest, ref at, hours)
                && TryWriteByte(dest, ref at, (byte)':')
                && TryWriteTwoBytes(dest, ref at, minutes);
        }
        else
        {
            ok = ok && TryWriteUnpaddedBytes(dest, ref at, minutes);
        }

        ok = ok
            && TryWriteByte(dest, ref at, (byte)':')
            && TryWriteTwoBytes(dest, ref at, seconds);

        // Same posture as TryWriteFrameTimecode: an unwritable label is a label with no
        // glyphs, never a throw inside Build().
        return ok ? at : 0;
    }

    // ------------------------------------------------------------------ digits
    //
    // Hand-rolled rather than TryFormat, for one reason: these run per frame and
    // TryFormat on a `long` with a "00" format goes through the number formatter's
    // culture lookup. Two digits is four lines.
    //
    // EVERY ONE OF THEM IS BOUNDS-CHECKED (52-REVIEW CR-01). The previous shape trusted
    // each caller to have already guaranteed the room, and `WriteUnpadded` in particular
    // wrote exactly as many characters as its value needed with no reference to
    // `dest.Length` at all — so a caller's length precheck that stated a MINIMUM (11)
    // rather than the worst case silently became a buffer overrun, i.e. an
    // IndexOutOfRangeException on the UI thread with no handler above it. The check
    // costs one comparison per field on a path that already does a division per field.

    /// <summary>Decimal digits in a non-negative value. Shared so the char and byte
    /// writers cannot disagree about how wide a number is.</summary>
    private static int DigitCount(long value)
    {
        var digits = 1;
        for (var probe = value; probe >= 10; probe /= 10)
        {
            digits++;
        }

        return digits;
    }

    private static bool TryWriteChar(Span<char> dest, ref int at, char value)
    {
        if ((uint)at >= (uint)dest.Length)
        {
            return false;
        }

        dest[at++] = value;
        return true;
    }

    private static bool TryWriteTwo(Span<char> dest, ref int at, long value)
    {
        if (value < 0)
        {
            value = 0;
        }

        if (value >= 100)
        {
            return TryWriteUnpadded(dest, ref at, value);
        }

        if (at + 2 > dest.Length)
        {
            return false;
        }

        dest[at++] = (char)('0' + (int)(value / 10));
        dest[at++] = (char)('0' + (int)(value % 10));
        return true;
    }

    private static bool TryWriteUnpadded(Span<char> dest, ref int at, long value)
    {
        if (value < 0)
        {
            value = 0;
        }

        var digits = DigitCount(value);
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

    private static bool TryWriteByte(Span<byte> dest, ref int at, byte value)
    {
        if ((uint)at >= (uint)dest.Length)
        {
            return false;
        }

        dest[at++] = value;
        return true;
    }

    private static bool TryWriteTwoBytes(Span<byte> dest, ref int at, long value)
    {
        if (value < 0)
        {
            value = 0;
        }

        if (value >= 100)
        {
            return TryWriteUnpaddedBytes(dest, ref at, value);
        }

        if (at + 2 > dest.Length)
        {
            return false;
        }

        dest[at++] = (byte)('0' + (int)(value / 10));
        dest[at++] = (byte)('0' + (int)(value % 10));
        return true;
    }

    private static bool TryWriteUnpaddedBytes(Span<byte> dest, ref int at, long value)
    {
        if (value < 0)
        {
            value = 0;
        }

        var digits = DigitCount(value);
        if (at + digits > dest.Length)
        {
            return false;
        }

        for (var i = digits - 1; i >= 0; i--)
        {
            dest[at + i] = (byte)('0' + (int)(value % 10));
            value /= 10;
        }

        at += digits;
        return true;
    }
}
