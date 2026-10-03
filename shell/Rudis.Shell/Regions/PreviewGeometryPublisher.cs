using Rudis.Shell.Interop;

namespace Rudis.Shell.Regions;

/// <summary>
/// D-09's publisher: the allocation-relevant core of <c>Preview.PublishSize()</c>,
/// extracted out of the XAML partial so it can be MEASURED rather than reviewed.
///
/// <para><b>⚠ THIS METHOD MUST ALLOCATE ZERO BYTES</b> (SHELL-06 / D-15 part 2). It
/// is gated at exactly 0 by
/// <c>Rudis.Shell.Tests/PreviewAllocationTests.publish_size_with_unchanged_dimensions_allocates_zero_bytes</c>
/// and its changing-dimensions twin, on the REAL shipped code — this file is
/// compiled into that test assembly, not copied into it.</para>
///
/// <para><b>Why a separate non-XAML file at all.</b> <c>Rudis.Shell.Tests</c> is a
/// plain <c>net9.0-windows</c> host with no window, no dispatcher and no XAML
/// compilation (see its csproj's own remarks), so a <c>SwapChainPanel</c> cannot be
/// instantiated there. The alternative — re-typing this arithmetic into the test —
/// would measure a COPY, and a copy that drifts is worse than no gate. So the
/// handler keeps only what genuinely needs the panel (three dependency-property
/// reads and one scalar push into the ink layer) and forwards everything else here.
/// <c>PreviewAllocationTests.publish_size_handler_body_carries_nothing_that_allocates</c>
/// scans the handler's own source to keep that division from eroding.</para>
///
/// <para><b>The idempotence guard is the point of the <c>ref</c> parameters.</b> A
/// WinUI layout pass fires <c>SizeChanged</c> far more often than the size actually
/// changes, and <c>CompositionScaleChanged</c> / <c>XamlRoot.Changed</c> add two more
/// sources onto the same publisher. Holding the last-published triple in the CALLER's
/// fields (rather than in a static here) keeps this type stateless and keeps the
/// region the single owner of its own geometry.</para>
///
/// <para>Both layers clamp and neither trusts the other (T-51-06): width/height are
/// floored at 1 here, and Rust clamps to <c>1..=16_384</c> and rejects a non-finite
/// scale on the far side of the ABI.</para>
/// </summary>
internal static class PreviewGeometryPublisher
{
    /// <summary>
    /// Convert the panel's DIP size and composition scale into the PHYSICAL pixels
    /// D-10 sends across the ABI, and publish them via <c>rudis_preview_resize</c> —
    /// but only if they actually changed.
    /// </summary>
    /// <returns><see langword="true"/> if the geometry changed and was published;
    /// <see langword="false"/> if the idempotence guard returned early. Returned
    /// rather than logged so the measurement can assert WHICH branch it measured
    /// (a "zero bytes" reading over 10,000 early returns would say nothing about
    /// the branch that crosses the ABI).</returns>
    internal static bool Publish(
        RudisNative? engine,
        double actualWidthDips,
        double actualHeightDips,
        float compositionScale,
        ref uint lastWidthPx,
        ref uint lastHeightPx,
        ref float lastScale)
    {
        var w = (uint)Math.Max(1, Math.Round(actualWidthDips * compositionScale));
        var h = (uint)Math.Max(1, Math.Round(actualHeightDips * compositionScale));

        if (w == lastWidthPx && h == lastHeightPx && compositionScale == lastScale)
        {
            return false;
        }

        lastWidthPx = w;
        lastHeightPx = h;
        lastScale = compositionScale;

        // Lock-free on the Rust side — three relaxed atomic stores plus a
        // release-ordered dirty flag the present thread consumes on its own next
        // tick. Scalars only: nothing is boxed, formatted or logged on this path.
        engine?.ResizePreview(w, h, compositionScale);
        return true;
    }
}
