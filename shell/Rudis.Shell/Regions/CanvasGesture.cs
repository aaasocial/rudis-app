using System.Text.Json.Nodes;
using Rudis.Shell.Interop;

namespace Rudis.Shell.Regions;

/// <summary>
/// The <c>Canvas › ToolPalette</c> tools, named exactly as the design handoff names
/// them (<c>Canvas.ToolPalette.Pointer|Pen|Lasso|Text|Shape</c>,
/// design_handoff_rudis_editor/README.md:41) and wire-identical to
/// <c>frontend/src/canvas.ts</c>'s <c>DrawTool</c> union.
///
/// <para>⚠ These are TOOL names. They are NOT the annotation wire's <c>kind</c>
/// tags — see <see cref="CanvasGesture.BuildAnnotation"/>.</para>
/// </summary>
internal enum DrawTool
{
    /// <summary>The non-drawing tool. The ink layer is INERT while it is selected
    /// (every pointer event falls through), and it never produces an annotation —
    /// the same gate v6.0's <c>DRAW_MODE</c> flag performed.</summary>
    Pointer,

    Pen,
    Lasso,
    Text,
    Shape,
}

/// <summary>
/// A normalized <c>[0,1]</c> point relative to the video FRAME CONTENT — never the
/// panel, never the letterbox bars. The managed mirror of
/// <c>rudis_core::canvas::NormPoint</c> (and of <c>frontend/src/canvas.ts</c>'s
/// <c>NormPoint</c>), carried as a <c>readonly record struct</c> so a gesture's
/// point list allocates one array, not N objects.
/// </summary>
internal readonly record struct NormPoint(double X, double Y);

/// <summary>
/// The PURE client half of the annotation contract (plan 51-05, D-12) — the tool
/// rules, the point ceiling, the coordinate normalization and the exact wire bytes,
/// with no XAML type anywhere in the file.
///
/// <para><b>Why a client half exists at all, when the backend is authoritative.</b>
/// <c>Command::apply</c> (crates/core/src/command.rs) is the REAL guarantee: it
/// clamps every coordinate, rejects an empty stroke, rejects a lasso under 3 points
/// and rejects a stroke over 500. This class mirrors those structural rejections so
/// a DOOMED command is never dispatched in the first place — the same reason
/// <c>frontend/src/canvas.ts</c> has them (its own header says so). It is a
/// courtesy layer, not a second source of truth (CLAUDE.md rule 4): whenever the
/// two disagree, the backend wins and its refusal is surfaced, never swallowed.</para>
///
/// <para><b>⚠ THE TOOL VOCABULARY IS NOT THE WIRE VOCABULARY.</b> The five tools are
/// <c>pointer/pen/lasso/text/shape</c>; the four wire <c>kind</c> tags are
/// <c>stroke/lasso/arrow/label</c>. <c>shape</c> emits <c>arrow</c>, <c>text</c>
/// emits <c>label</c>, <c>pen</c> emits <c>stroke</c>, and only <c>lasso</c> shares
/// a spelling with its tool. Getting one of these four wrong is the most likely
/// SILENT parity break in the whole port — the dispatch would fail deserialization
/// (or land as the wrong kind) for a reason nobody reading the tool name would
/// guess. <c>CanvasGestureTests.the_wire_kind_tags_are_not_the_tool_names</c> pins
/// all four.</para>
///
/// <para><b>The letterbox formula is NOT re-derived here — deliberately, and this
/// file contains no trace of it.</b> <see cref="Normalize"/> takes the engine's own
/// <see cref="RudisPreviewRect"/>, filled by <c>rudis_preview_content_rect</c> from
/// the SAME call the compositor letterboxes the picture with. One formula, one
/// language, no drift (D-12); re-deriving it in C# is exactly the "two models of one
/// truth" CLAUDE.md rule 4 forbids, and the absence of the formula's own vocabulary
/// from this file is a mechanical check in plan 51-05's acceptance criteria.</para>
/// </summary>
internal static class CanvasGesture
{
    /// <summary>
    /// The client-side point ceiling — mirrors <c>frontend/src/canvas.ts</c>'s
    /// <c>MAX_POINTS</c> and the backend's own <c>MAX_ANNOTATION_POINTS</c>
    /// (crates/core/src/command.rs:359), which REJECTS a shape carrying more.
    ///
    /// <para>T-51-07: at the ceiling <see cref="AccumulatePoint"/> returns the list
    /// UNCHANGED, so a long drag stops growing rather than accumulating
    /// unboundedly. Client and server enforce this independently — the contract
    /// tier proves the server still refuses a 501-point stroke even though the
    /// client can no longer produce one.</para>
    /// </summary>
    internal const int MaxPoints = 500;

    /// <summary>
    /// <c>PreviewInkLayer.OnPointerMoved</c>'s at-rest guard, as a named predicate
    /// on this WinUI-free type so the per-tick cost of a pointer move with NO
    /// gesture in progress can be measured in bytes (SHELL-06 / D-15 part 2,
    /// <c>Rudis.Shell.Tests/PreviewAllocationTests</c>).
    ///
    /// <para><b>The predicate is trivial and its VALUE is trivial; the ORDERING is
    /// what matters.</b> With a draw tool selected the ink layer is hit-testable for
    /// the whole tool selection, so the handler fires on every hover sample across
    /// the video — and <c>GetIntermediatePoints</c> allocates a list on every call.
    /// So the real claim is "the handler returns BEFORE the coalesced read", which
    /// no byte measurement of this method can establish. That claim is asserted
    /// mechanically instead, against the handler's own source, by
    /// <c>PreviewAllocationTests.the_pointer_move_guard_precedes_the_coalesced_read</c>
    /// — with the scan proven to fail on a reordered fixture. Naming the guard here
    /// is what gives that scan a stable token to anchor on.</para>
    /// </summary>
    internal static bool ShouldIgnoreMove(bool gestureInProgress) => !gestureInProgress;

    /// <summary>The one space this path ever dispatches (D-07, unchanged from v6.0):
    /// a mark drawn over the Preview is FRAME-LINKED. Whiteboard-space marks are the
    /// Canvas STAGE's, and their consumer is Phase 54 — see <c>Canvas.xaml.cs</c>'s
    /// recorded scope note.</summary>
    internal const string FrameLinkedSpace = "frame_linked";

    /// <summary>
    /// Map a pointer position in the ink layer's own DIP coordinates onto the
    /// engine's frame-content rect, normalized into <c>[0,1]</c> and CLAMPED.
    ///
    /// <para><paramref name="rect"/> is in PHYSICAL pixels relative to the panel's
    /// origin (plan 51-03), and the ink layer shares that origin — it is a sibling
    /// filling the same Grid cell as the <c>SwapChainPanel</c> — so the only
    /// conversion needed is the DIP→physical multiply by
    /// <paramref name="scale"/>.</para>
    ///
    /// <para><b>Clamp, never reject</b> (T-51-18): a press inside a letterbox bar is
    /// a legitimate gesture point and becomes <c>0.0</c> or <c>1.0</c>, matching
    /// <c>NormPoint::clamped</c>'s contract and v6.0's
    /// <c>client_px_to_normalized</c>. The backend clamps independently — two
    /// clamps, neither trusting the other.</para>
    ///
    /// <para>A degenerate rect (the <c>0x0</c> the ABI publishes until a composite
    /// has actually happened) or a non-finite scale answers the ORIGIN rather than
    /// dividing by zero: a NaN coordinate would serialize as JSON <c>null</c> and
    /// fail deserialization on the far side for a reason no reader could trace back
    /// to here.</para>
    /// </summary>
    internal static NormPoint Normalize(double x, double y, double scale, RudisPreviewRect rect)
    {
        if (rect.Width == 0 || rect.Height == 0 || !double.IsFinite(scale))
        {
            return new NormPoint(0, 0);
        }

        var nx = ((x * scale) - rect.X) / rect.Width;
        var ny = ((y * scale) - rect.Y) / rect.Height;
        return new NormPoint(Clamp01(nx), Clamp01(ny));
    }

    /// <summary>
    /// Clamp into <c>[0,1]</c>, mapping NaN to <c>0</c>.
    ///
    /// <para>Written as two comparisons rather than <c>Math.Clamp</c> deliberately:
    /// <c>Math.Clamp(double.NaN, 0, 1)</c> RETURNS NaN (it does not throw and it
    /// does not clamp), which is exactly the value this guard exists to stop. Every
    /// comparison against NaN is false, so both branches fall through to
    /// <c>0</c>.</para>
    /// </summary>
    private static double Clamp01(double v) => v > 1.0 ? 1.0 : v > 0.0 ? v : 0.0;

    /// <summary>
    /// Append <paramref name="p"/> unless <see cref="MaxPoints"/> is already
    /// reached, in which case the list is left UNCHANGED and <see langword="false"/>
    /// is returned (mirrors <c>canvas.ts</c>'s <c>accumulatePoint</c>, which returns
    /// the array unchanged).
    /// </summary>
    /// <returns><see langword="true"/> when the point was appended.</returns>
    internal static bool AccumulatePoint(List<NormPoint> points, NormPoint p)
    {
        if (points.Count >= MaxPoints)
        {
            return false;
        }

        points.Add(p);
        return true;
    }

    /// <summary>
    /// Append <paramref name="p"/> unless it is the SAME point as the one already at
    /// the end of the list, or the ceiling is reached.
    ///
    /// <para>⚠ MEASURED, plan 51-10. Every committed curve in this phase rendered as a
    /// straight line, and the in-app commit notes showed the first six normalized
    /// points IDENTICAL (<c>n0..n5 = (0.266,0.499)</c>) while the layer-space extent
    /// was real. The recorded pointer-sample stream
    /// (<c>artifacts/51-10-ink-sampling.md</c>) named the CAUSE of that particular
    /// reading as the test harness — <c>SetCursorPos</c> relocates the cursor without
    /// injecting an input event, so WinUI's pointer stack kept answering the
    /// pointer-DOWN position — and the same measurement, taken again through real
    /// injected input, showed the SECOND half of the problem, which IS this file's:
    /// 28 recorded samples committing 28 points of which only 14 are distinct,
    /// because <c>OnPointerMoved</c> appends the coalesced entry AND the identical
    /// authoritative current position on every event.</para>
    ///
    /// <para>Repeated samples do not merely waste the 500-point ceiling — they
    /// CONSUME it, so a long drag spends its whole budget on positions it already has
    /// and the far end of the stroke the user drew is never recorded. Dropping an
    /// exact repeat is lossless: a stroke through a point twice draws the same pixels
    /// as a stroke through it once.</para>
    ///
    /// <para>Only the IMMEDIATELY preceding point is compared, never the whole list.
    /// A gesture that legitimately revisits a position — a loop, a zig-zag, a lasso
    /// closing on itself — must keep every one of those vertices, and de-duplicating
    /// against the whole list would flatten exactly the shapes this exists to
    /// preserve. <c>NormPoint</c> is a <c>readonly record struct</c>, so <c>==</c> is
    /// the value comparison: no epsilon, no <c>Math.Abs</c>, and no second definition
    /// of "the same point".</para>
    /// </summary>
    /// <returns><see langword="true"/> when the point was appended.</returns>
    internal static bool AccumulateDistinctPoint(List<NormPoint> points, NormPoint p)
    {
        if (points.Count > 0 && points[^1] == p)
        {
            return false;
        }

        return AccumulatePoint(points, p);
    }

    /// <summary>
    /// Sort a <c>(start, end)</c> pair so <c>linked_range_us</c> is always
    /// <c>start &lt;= end</c> regardless of drag direction — v6.0's
    /// <c>sortedRange</c> (frontend/src/main.ts:2552). The backend passes ranges
    /// through verbatim without validating order, so ordering is the client's job.
    /// </summary>
    internal static (long Start, long End) SortedRange(long a, long b)
        => a <= b ? (a, b) : (b, a);

    /// <summary>
    /// The <c>shape</c> JSON object for a finished gesture, or <see langword="null"/>
    /// when the gesture cannot form a valid annotation. Mirrors
    /// <c>frontend/src/canvas.ts</c>'s <c>buildAnnotation</c> EXACTLY, including
    /// which gestures are structurally refused client-side:
    /// <list type="bullet">
    /// <item><c>Pointer</c> never produces an annotation;</item>
    /// <item><c>Pen</c> needs &gt;= 1 point (the backend rejects an empty stroke);</item>
    /// <item><c>Lasso</c> needs &gt;= 3 points (a polygon);</item>
    /// <item><c>Shape</c> needs &gt;= 2 points and uses the FIRST as the arrow's
    ///   start and the LAST as its end — every intermediate drag sample is
    ///   discarded;</item>
    /// <item><c>Text</c> needs &gt;= 1 point AND non-empty text (a cancelled or
    ///   empty label dialog produces nothing).</item>
    /// </list>
    ///
    /// <para>⚠ The emitted <c>kind</c> is the WIRE tag, not the tool name — see the
    /// class remarks.</para>
    /// </summary>
    internal static JsonObject? BuildAnnotation(
        DrawTool tool, IReadOnlyList<NormPoint> points, string? text = null)
    {
        switch (tool)
        {
            case DrawTool.Pointer:
                return null;

            case DrawTool.Pen:
                // `pen` -> WIRE kind `stroke`. The point list is carried through
                // unchanged.
                return points.Count >= 1 ? Points("stroke", points) : null;

            case DrawTool.Lasso:
                // `lasso` -> WIRE kind `lasso` — the ONE tool whose name and wire
                // tag coincide.
                return points.Count >= 3 ? Points("lasso", points) : null;

            case DrawTool.Shape:
                // `shape` -> WIRE kind `arrow`. A drag: first point is the start,
                // last is the end; the samples in between are irrelevant.
                return points.Count >= 2
                    ? new JsonObject
                    {
                        ["kind"] = "arrow",
                        ["start"] = Point(points[0]),
                        ["end"] = Point(points[^1]),
                    }
                    : null;

            case DrawTool.Text:
                // `text` -> WIRE kind `label`, anchored at the gesture's FIRST
                // point. Never dispatch an empty label (covers a cancelled dialog,
                // the WinUI equivalent of v6.0's cancelled `window.prompt`).
                return points.Count >= 1 && !string.IsNullOrEmpty(text)
                    ? new JsonObject
                    {
                        ["kind"] = "label",
                        ["position"] = Point(points[0]),
                        ["text"] = text,
                    }
                    : null;

            default:
                return null;
        }
    }

    /// <summary>
    /// The full <c>rudis_dispatch_command</c> args envelope for one committed
    /// gesture:
    /// <c>{"cmd":{"type":"add_annotation","data":{id, shape, linked_range_us, space}}}</c>.
    ///
    /// <para>The outer <c>cmd</c> key is <c>crates/ffi/src/dispatch.rs</c>'s
    /// <c>DispatchArgs</c>; the inner <c>type</c>/<c>data</c> pair is
    /// <c>rudis_core::Command</c>'s adjacent tagging. Both are pinned by tests
    /// rather than remembered.</para>
    ///
    /// <para>Lives HERE, not in <c>PreviewInkLayer</c>, for one reason: the exact
    /// strings this produces are the literals
    /// <c>crates/ffi/tests/contract.rs</c> dispatches, and a builder shared between
    /// the production caller and the test that pins those strings is the only shape
    /// in which the Rust tier is testing the CLIENT rather than testing itself.</para>
    /// </summary>
    internal static JsonObject BuildDispatchArgs(
        string id, JsonObject shape, (long Start, long End)? linkedRangeUs, string space)
    {
        JsonNode? linked = null;
        if (linkedRangeUs is { } range)
        {
            linked = new JsonArray(JsonValue.Create(range.Start), JsonValue.Create(range.End));
        }

        return new JsonObject
        {
            ["cmd"] = new JsonObject
            {
                ["type"] = "add_annotation",
                ["data"] = new JsonObject
                {
                    ["id"] = id,
                    ["shape"] = shape,
                    ["linked_range_us"] = linked,
                    ["space"] = space,
                },
            },
        };
    }

    private static JsonObject Points(string kind, IReadOnlyList<NormPoint> points)
    {
        var array = new JsonArray();
        for (var i = 0; i < points.Count; i++)
        {
            array.Add(Point(points[i]));
        }

        return new JsonObject { ["kind"] = kind, ["points"] = array };
    }

    private static JsonObject Point(NormPoint p)
        => new() { ["x"] = JsonValue.Create(p.X), ["y"] = JsonValue.Create(p.Y) };
}
