namespace Rudis.Shell.Regions;

// ============================================================================
// THE D-02 DIVIDER'S PURE MATH — hand-rolled, and that is a RECORDED DEVIATION.
// ============================================================================
//
// D-02 says "a draggable `GridSplitter` between Canvas and Chat, defaulting to
// 50/50". Research verified the load-bearing fact behind that wording is FALSE:
// `GridSplitter` is NOT a built-in WinUI 3 control. It ships only in the
// third-party `CommunityToolkit.WinUI.Controls.Sizers` NuGet package
// (54-RESEARCH § "GridSplitter is NOT a built-in WinUI 3 control"). So D-02 could
// not be satisfied literally without ALSO deciding to take a new UI dependency,
// which is a decision D-02 never made.
//
// SETTLED AS OPTION B — hand-roll the divider in this codebase's own established
// pointer-drag idiom. Three reasons, in the order they weighed:
//
//   1. The shell carries ZERO third-party UI packages today (Rudis.Shell.csproj
//      references only Microsoft.WindowsAppSDK and the SDK build tools). Adding
//      one is a PROVENANCE.md entry, a lock-file change and a permanent supply
//      chain, and CLAUDE.md rule 6 makes that a deliberate act, never a default.
//   2. The whole requirement is two-`*`-row proportion adjustment: D-02 wants a
//      50/50 default and D-03 says the result is never persisted. That is the
//      function below. The package's real value — min/max constraints, a
//      keyboard-accessible thumb, column AND row modes — is mostly requirement
//      this phase does not have.
//   3. SHELL-05 already chose hand-rolled over toolkit for a FAR larger surface
//      (the whole Timeline), and `TimelineInteraction.cs` is this codebase's
//      worked idiom for pointer-driven geometry: pure C# taking surface
//      coordinates as doubles, unit-testable with no window, with the raw pointer
//      events living in the WinUI half.
//
// SAME BEHAVIOUR, DIFFERENT MECHANISM. The deviation is recorded HERE, at the
// code, and in 54-01-SUMMARY.md — and it closes the Wave-0 PROVENANCE.md item for
// the Sizers package as NOT NEEDED rather than leaving it open forever.
//
// WINUI-FREE BY RULE, like the rest of this directory (ChatPurityGateTests). The
// visual half — a thin thumb Border with PointerPressed/PointerMoved/
// PointerReleased and CapturePointer, writing the returned pair into two star
// `GridLength`s — is plan 54-05's, so the MainWindow edit it makes stays minimal.

internal static class ChatSplit
{
    /// <summary>
    /// Move the divider: given the top row's pixel height AT DRAG START, the two
    /// rows' combined pixel height, the drag delta (POSITIVE IS DOWN, matching
    /// pointer coordinates) and the two minimums, return the new
    /// <c>(top, bottom)</c> pixel pair.
    ///
    /// <para>The caller writes the pair back as STAR weights, not as fixed pixel
    /// heights: star weights proportional to the pixel pair reproduce the same
    /// split immediately, and then keep that PROPORTION when the window is
    /// resized — which is what a divider between two <c>*</c> rows means. Writing
    /// pixels would pin the Canvas to a fixed height and hand every resize to the
    /// Chat.</para>
    ///
    /// <para>Total by construction: the result always sums to
    /// <paramref name="totalPx"/> and neither half is ever negative, because a
    /// negative or NaN <c>GridLength</c> is a throw in the WinUI half — and a
    /// layout pass can legitimately hand this function a zero or not-yet-measured
    /// height. Over-constrained input (<paramref name="totalPx"/> smaller than the
    /// two minimums together) refuses to move rather than emitting a split that
    /// cannot exist.</para>
    /// </summary>
    internal static (double Top, double Bottom) Apply(
        double startTopPx, double totalPx, double deltaY, double minTopPx, double minBottomPx)
    {
        // Nothing to divide. A row measured before first layout, or a collapsed
        // window, gets an honest (0, 0) rather than a NaN that surfaces three
        // frames later inside a layout pass.
        if (!double.IsFinite(totalPx) || totalPx <= 0)
        {
            return (0, 0);
        }

        // The split we fall back to when the drag cannot be honoured. Clamped into
        // the row itself, because the caller's remembered drag-start height can
        // outlive a window resize.
        var start = double.IsFinite(startTopPx)
            ? Math.Clamp(startTopPx, 0, totalPx)
            : totalPx / 2;

        // One guard covers three refusals: a non-finite delta, a negative minimum,
        // and the over-constrained case where the two minimums do not both fit.
        var lo = Math.Max(0, minTopPx);
        var hi = totalPx - Math.Max(0, minBottomPx);
        if (!double.IsFinite(deltaY) || !(lo <= hi))
        {
            return (start, totalPx - start);
        }

        var top = Math.Clamp(start + deltaY, lo, hi);
        return (top, totalPx - top);
    }
}
