namespace Rudis.Shell.Regions;

/// <summary>
/// WHEN a press-and-move becomes a drag — the one decision that was missing.
///
/// <para><b>Why this is a class and not four lines inside a pointer handler.</b> Plan
/// 53.1-01 measured that the items control's BUILT-IN gesture recognizer never reaches
/// its drag-start event in this unpackaged host, so the MediaBin now recognizes the drag
/// gesture itself. That recognition is exactly the kind of thing that is trivially wrong
/// in a way no screenshot shows — a threshold on max-axis instead of euclidean distance,
/// an arm that fires twice per press, a stale press point surviving a button release.
/// Here it is eleven assertions in a plain net9.0-windows test host with no window
/// (<c>MediaBinDragTests</c>).</para>
///
/// <para>No WinUI types, by rule — the directory contract set by 53-01 and gated by
/// <c>MediaBinPurityGateTests</c>. The caller in <c>MediaBin.xaml.cs</c> unwraps the
/// pointer's position and button state and passes plain doubles and a bool. That is what
/// lets the arming rule be asserted without a window, which is the whole reason it is
/// here and not there.</para>
///
/// <para><b>Not thread-safe, deliberately.</b> Every caller is a pointer event handler,
/// and pointer events are delivered on the UI thread only. Locking would buy nothing and
/// would imply a concurrency this type never sees.</para>
/// </summary>
internal sealed class MediaBinDragGesture
{
    /// <summary>8 logical px, squared — compared against SQUARED displacement so that no
    /// square root is ever evaluated on the pointer path. The one statement of the
    /// threshold in this codebase; a second copy is how the tile and a test end up
    /// disagreeing about what a drag is, which is unfalsifiable from the outside because
    /// both halves stay internally consistent.</summary>
    internal const double ThresholdPxSquared = 64.0;

    private double _pressX;
    private double _pressY;
    private bool _pressed;
    private bool _armedThisPress;

    /// <summary>
    /// Record where a press landed. Any prior press is discarded, latch included.
    ///
    /// <para>A non-finite coordinate is treated as no press at all rather than stored:
    /// every later comparison against it would be <c>NaN</c>, and a stored garbage origin
    /// is worse than none.</para>
    /// </summary>
    internal void Press(double x, double y)
    {
        if (!double.IsFinite(x) || !double.IsFinite(y))
        {
            Reset();
            return;
        }

        _pressX = x;
        _pressY = y;
        _pressed = true;
        _armedThisPress = false;
    }

    /// <summary>
    /// Has this move crossed the threshold, with the button still down, for the FIRST
    /// time since the press? Returns <c>true</c> exactly once per press.
    ///
    /// <para>⚠ The finite check is NOT defensive noise. <c>NaN</c> makes EVERY comparison
    /// false, including <c>&lt; ThresholdPxSquared</c> — so an implementation that only
    /// early-returns on "under threshold" would ARM on a <c>NaN</c> coordinate rather
    /// than refuse it.</para>
    /// </summary>
    internal bool TryArm(double x, double y, bool leftButtonPressed)
    {
        if (!_pressed || _armedThisPress)
        {
            return false;
        }

        if (!leftButtonPressed)
        {
            // The button came up without a drag: this was a click, and the press point
            // must not survive to arm a later, unrelated move.
            Reset();
            return false;
        }

        if (!double.IsFinite(x) || !double.IsFinite(y))
        {
            return false;
        }

        var dx = x - _pressX;
        var dy = y - _pressY;
        if ((dx * dx) + (dy * dy) < ThresholdPxSquared)
        {
            return false;
        }

        _armedThisPress = true;
        return true;
    }

    /// <summary>No press is recorded; the next move cannot arm. Call from
    /// <c>PointerReleased</c>, <c>PointerCaptureLost</c> and <c>PointerCanceled</c> — an
    /// interrupted gesture that stayed armed would drag on the next unrelated hover.
    /// </summary>
    internal void Reset()
    {
        _pressed = false;
        _armedThisPress = false;
        _pressX = 0;
        _pressY = 0;
    }
}
