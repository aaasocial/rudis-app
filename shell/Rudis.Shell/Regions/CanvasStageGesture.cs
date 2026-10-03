using System.Text.Json.Nodes;

namespace Rudis.Shell.Regions;

/// <summary>
/// Which wire <c>kind</c> a committed whiteboard mark carries — the managed mirror
/// of <c>rudis_core::canvas::AnnotationShape</c>'s four tags
/// (<c>stroke/lasso/arrow/label</c>, crates/core/src/canvas.rs:35-41).
///
/// <para>⚠ These are WIRE tags, NOT the <see cref="DrawTool"/> names. The mapping
/// between the two is <see cref="CanvasGesture.BuildAnnotation"/>'s job and is not
/// re-derived here — see that method's remarks for why getting it wrong is the
/// likeliest silent parity break in the port.</para>
/// </summary>
internal enum StageMarkKind
{
    Stroke,
    Lasso,
    Arrow,
    Label,
}

/// <summary>
/// One COMMITTED whiteboard mark, flattened for drawing: an id, a kind, and one
/// point list every kind is expressed in — a stroke's or lasso's own points, an
/// arrow's <c>[start, end]</c>, a label's <c>[position]</c> — so a render pass walks
/// ONE representation instead of branching four ways over JSON.
///
/// <para>Coordinates stay NORMALIZED. Denormalizing is the caller's job
/// (<c>p.X * width</c>, <c>p.Y * height</c>), which is exactly how the Rust half's
/// <c>draw_annotations_onto_styled</c> paints the same marks into the agent's
/// whiteboard raster — one convention, two renderers.</para>
///
/// <para><see cref="Text"/> is non-null for <see cref="StageMarkKind.Label"/> and
/// null for every other kind.</para>
/// </summary>
internal sealed record StageMark(
    string Id, StageMarkKind Kind, IReadOnlyList<NormPoint> Points, string? Text);

/// <summary>
/// WHICH GESTURE OWNS THE TRAIL RIGHT NOW — a monotonic token minted at pointer-down
/// and carried by that gesture's commit for as long as the commit is in flight.
///
/// <para><b>The defect this exists to close (review WR-01), stated as the ordering
/// that produced it.</b> <c>Canvas.xaml.cs</c>'s <c>FinishStageGesture</c> launches
/// <c>CommitStageAsync</c> fire-and-forget, and that task awaits a dispatch on the
/// SHARED interop worker — a serialized queue that an export, an import or an agent
/// turn can already be sitting in. Every finish/failure path in the continuation then
/// clears the trail, and clearing the trail clears the gesture's point list, which is
/// a single SHARED list. So:</para>
/// <code>
/// gesture 1 pointer-up   -> CommitStageAsync -> await dispatch   (yields the UI thread)
/// gesture 2 pointer-down -> StopStageConfirmTimer()              (nothing to stop yet)
/// gesture 2 drag         -> _points accumulating
/// gesture 1 continuation -> ClearStageTrail() -> _points.Clear()  // gesture 2 is WIPED
/// </code>
/// <para>Gesture 2 then commits a TRUNCATED stroke, or — if the wipe lands after its
/// last move sample — none at all, because <c>CanvasGesture.BuildAnnotation</c>
/// refuses an empty point list and the client returns before dispatching. Rapid
/// consecutive sketching is the ordinary way to hit it. The stage's pointer-down
/// <c>StopStageConfirmTimer()</c> (plan 60.2-02) closes only the TIMER half of this:
/// it cannot stop a continuation that has not resumed yet.</para>
///
/// <para><b>Why a token rather than a flag.</b> The question the async half has to ask
/// is not "is a gesture active" (gesture 2 may already be finished too) but "is the
/// trail still MINE" — and only an identity can answer that. A commit whose token is
/// no longer current has no bookkeeping left to do: its mark is drawn by
/// <c>ApplyMirrorState</c> from MIRROR state, never by the trail it was holding, so
/// standing down costs nothing visual (D-13's hand-off is between the trail and the
/// COMMITTED layer, and the committed layer is already drawing by then).</para>
///
/// <para>This is the shape <c>D-60.2-02</c> names as the right one for the same root
/// cause on <c>PreviewInkLayer</c> ("the tick checks that the gesture it belongs to is
/// still the current one"). It is NOT transplanted there by this fix: that file is
/// pinned byte-unchanged by STAGE-04, and its second drawer is the ENGINE rather than
/// a mirror-driven committed layer, so the cost of standing down is not the same
/// question. That entry stays open.</para>
///
/// <para>Lives HERE, in the WinUI-free half, for the reason this whole file exists:
/// <c>Rudis.Shell.Tests.csproj</c> compiles an explicit allow-list that never includes
/// a WinUI-bearing region file, so an <c>int</c> field beside the pointer handlers
/// would be permanently unreachable from the unit tier.</para>
/// </summary>
internal sealed class StageGestureSequence
{
    /// <summary>The token of the gesture that currently owns the trail. Zero before
    /// the first gesture — a value no commit is ever handed, because
    /// <see cref="Begin"/> increments BEFORE it answers.</summary>
    internal int Current { get; private set; }

    /// <summary>Mint the next token and make it the current one. Called at
    /// pointer-down, before a single point is accumulated.</summary>
    internal int Begin()
    {
        Current++;
        return Current;
    }

    /// <summary>Does <paramref name="token"/> still own the trail? A commit asks this
    /// before touching the trail, the point list or the confirm timer.</summary>
    internal bool IsCurrent(int token) => token == Current;
}

/// <summary>
/// The PURE client half of the Canvas STAGE's annotation path (plan 60.2-01) —
/// the whiteboard wire constant, the stage's own coordinate normalizer, and the
/// committed-mark parser the mirror-driven render consumes. No XAML type anywhere
/// in the file.
///
/// <para><b>Courtesy client, backend authoritative</b> — the same framing
/// <see cref="CanvasGesture"/> states at length and this file does not restate:
/// <c>Command::apply</c> (crates/core/src/command.rs) clamps every coordinate,
/// rejects an empty stroke and rejects a lasso under three points. Whatever this
/// file decides, the backend decides again and wins.</para>
///
/// <para><b>Why this is a separate file rather than members on
/// <c>Canvas.xaml.cs</c>.</b> <c>Rudis.Shell.Tests.csproj</c> compiles an explicit
/// ALLOW-LIST of production sources and never compiles a WinUI-bearing region file,
/// so logic placed beside the pointer handlers would be permanently unreachable from
/// the unit tier no matter what its access modifier said. The established answer is
/// to extract the decision logic into its own WinUI-free file and add one named
/// <c>Compile Include</c> line: <c>CanvasGesture.cs</c> (51-05),
/// <c>PreviewMonitorCommand.cs</c> (52-13) and <c>ToolbarProjectRoutes.cs</c>
/// (60.1-05) are the three precedents, and this is the fourth. Same file, not a
/// copy.</para>
///
/// <para><b>What is deliberately NOT here.</b> The tool-to-shape rules, the point
/// ceiling, the exact-repeat de-duplication and the dispatch envelope all live on
/// <see cref="CanvasGesture"/> and were written generically in Phase 51 — including
/// <see cref="CanvasGesture.BuildDispatchArgs"/>, which has always taken
/// <c>space</c> as a parameter and needs ZERO changes to serve this surface. The
/// stage CALLS them; it does not re-derive them. A second copy of any of it would be
/// the silent parity break those files' own remarks warn about.</para>
/// </summary>
internal static class CanvasStageGesture
{
    /// <summary>
    /// The second value of the SAME <c>AnnotationSpace</c> wire enum
    /// <see cref="CanvasGesture.FrameLinkedSpace"/> names — serde renames the
    /// variants to snake_case, so <c>AnnotationSpace::Whiteboard</c> travels as this
    /// string (crates/core/src/canvas.rs:61-72).
    ///
    /// <para><b>The absence of this string from the shell WAS this phase's
    /// orphan.</b> The domain half shipped in Phases 13/14 — the enum, the undoable
    /// <c>AddAnnotation</c>, the agent's whiteboard vision block, all of it tested —
    /// and then the GATE-07 cutover deleted the only client that ever wrote the
    /// value. Until plan 60.2-01, a comment-filtered search for it across
    /// <c>shell/Rudis.Shell/**</c> returned nothing at all, which is precisely the
    /// shape STAGE-06's sweep extension exists to catch mechanically next time.</para>
    ///
    /// <para>Whiteboard marks are project-global and carry no
    /// <c>linked_range_us</c>: the server forces it to <c>None</c> for this space
    /// regardless of what the client sends (command.rs:1693-1697), so the stage
    /// passes <c>null</c> and adds no client-side branch to "match the domain
    /// rule" — that rule is already enforced exactly once, server-side.</para>
    /// </summary>
    internal const string WhiteboardSpace = "whiteboard";

    /// <summary>
    /// Does a pointer-down START a stage gesture?
    ///
    /// <para><b>TWO conditions, and the second one is the fix (review WR-02).</b>
    /// <c>OnStagePointerPressed</c> used to gate on the TOOL alone. WinUI raises
    /// <c>PointerPressed</c> for ANY button, so a right-click or a middle-click
    /// anywhere in the Canvas region with Pen selected began a real gesture and — one
    /// release later — committed a real, undoable annotation. A drawing gesture is a
    /// drag with the PRIMARY button; every other press belongs to whatever context
    /// menu or paste affordance this region grows later, and must not silently draw
    /// in the meantime.</para>
    ///
    /// <para><b><paramref name="primaryButtonPressed"/> is
    /// <c>PointerPointProperties.IsLeftButtonPressed</c>, which is NOT a mouse-only
    /// question.</b> WinUI reports it <see langword="true"/> for a touch contact and
    /// for a pen tip in contact as well, so gating on it keeps pen and touch drawing
    /// exactly as they were — the reason this is a primary-button test rather than a
    /// <c>PointerDeviceType.Mouse</c> one.</para>
    /// </summary>
    internal static bool ShouldStartGesture(DrawTool tool, bool primaryButtonPressed)
        => tool != DrawTool.Pointer && primaryButtonPressed;

    /// <summary>
    /// Is a FINISHED gesture worth dispatching, or was it a tap?
    ///
    /// <para><b>The defect (review WR-02).</b> Nothing downstream required a stage
    /// gesture to have MOVED. A single click with Pen selected accumulated one point
    /// (the down and the up positions are identical, so
    /// <see cref="CanvasGesture.AccumulateDistinctPoint"/> de-duplicates them into
    /// one), and <see cref="CanvasGesture.BuildAnnotation"/>'s Pen arm accepts
    /// <c>points.Count &gt;= 1</c> — so the backend was handed a real one-point
    /// <c>stroke</c>, which it correctly accepted.</para>
    ///
    /// <para><b>And then NOTHING drew it.</b> Both renderers on this surface need two
    /// points — the live trail because a zero-length dashed stroke has no pixels, and
    /// the committed polyline for the same reason. So a stray click minted state the
    /// user could not see: an undoable annotation counted by the <c>canvas</c>
    /// introspection channel, carried into the agent's whiteboard raster, and
    /// persisted into every <c>.rud</c> the project is saved as. Invisible state the
    /// user cannot see to undo is the worst of the three options; the other two are
    /// "draw a dot" (which would need a render change, and a dot is not what a stroke
    /// tool promises) and this one.</para>
    ///
    /// <para><b>Text is EXEMPT, and that is not an inconsistency.</b> A label is
    /// anchored at a POINT — one click IS the whole gesture, and the mark it produces
    /// is drawn (<c>AddLabel</c> needs one point, not two). The rule is therefore
    /// "a stroke must be a stroke", not "every gesture must move".</para>
    ///
    /// <para><b>Where this rule does NOT go.</b> Not into
    /// <see cref="CanvasGesture.BuildAnnotation"/>: that builder is SHARED with
    /// <c>PreviewInkLayer</c>'s <c>frame_linked</c> path, which Phase 60.2 pins
    /// byte-unchanged (STAGE-04) and which has had this behaviour since Phase 51.
    /// Widening the fix to both surfaces is a real question with a real answer, but it
    /// is a change to the Preview's commit semantics and belongs to a plan that is
    /// allowed to touch that file.</para>
    /// </summary>
    internal static bool IsCommittableGesture(DrawTool tool, int distinctPointCount)
        => tool switch
        {
            // The non-drawing tool never produces anything. Stated rather than left to
            // the shared builder, because this gate runs BEFORE it.
            DrawTool.Pointer => false,

            // A label's single point IS its anchor.
            DrawTool.Text => distinctPointCount >= 1,

            // Pen, Lasso and Shape all draw a LINE between points. One point is a tap.
            // (The shared builder's own minimums are stricter still for Lasso and
            // Shape — 3 and 2 — and it applies them afterwards regardless; this gate
            // does not restate them, it only refuses the tap that the builder would
            // have accepted.)
            _ => distinctPointCount >= 2,
        };

    /// <summary>
    /// Map a pointer position in the stage's own DIP coordinates onto its own box,
    /// normalized into <c>[0,1]</c> and CLAMPED.
    ///
    /// <para><b>This function is simpler than <see cref="CanvasGesture.Normalize"/>
    /// on purpose, and the simplicity is the finding.</b> The Preview's normalizer
    /// solves three problems the stage does not have, and none of the three
    /// machinery for them may be imported here:
    /// <list type="number">
    /// <item><b>No engine rect.</b> The Preview normalizes against the engine's
    ///   frame-content rect; the stage's box is the drawing element's own
    ///   <c>ActualWidth</c>/<c>ActualHeight</c>. This method must never grow a
    ///   <c>RudisPreviewRect</c> parameter.</item>
    /// <item><b>No letterbox.</b> There is no contain-fit picture inside the stage,
    ///   so there is no offset to subtract and no bar to fall inside.</item>
    /// <item><b>No DIP-to-physical conversion.</b> The Preview's rect is published in
    ///   physical pixels, so it needs the display factor multiplied in. Here BOTH the
    ///   pointer position and the box are DIPs produced by the SAME element's layout
    ///   pass, so there is no unit mismatch to correct and no such parameter.</item>
    /// </list>
    /// </para>
    ///
    /// <para><b>And it must never grow an engine-readiness gate.</b>
    /// <c>PreviewInkLayer.TryLatchGeometry</c> refuses a gesture until a composite
    /// has happened, because a frame-linked mark is meaningless without a frame to
    /// anchor it to. A whiteboard mark has no such dependency: drawing on the stage
    /// must work with ZERO video loaded, on a project that has never composited
    /// anything, and gating it the same way would silently break exactly that.</para>
    ///
    /// <para><b>Clamp, never reject.</b> A drag that leaves the stage saturates at
    /// the edge rather than being dropped — <c>NormPoint::clamped</c>'s contract
    /// (canvas.rs:26-32). The backend clamps independently: two clamps, neither
    /// trusting the other.</para>
    ///
    /// <para>A degenerate box (zero, or the non-finite values a never-measured or
    /// collapsed element can report) answers the ORIGIN rather than dividing by
    /// zero. A NaN or infinite coordinate would serialize as JSON <c>null</c> and
    /// fail deserialization on the far side for a reason no reader could trace back
    /// to here.</para>
    /// </summary>
    internal static NormPoint NormalizeInBox(double x, double y, double width, double height)
    {
        if (width <= 0 || height <= 0 || !double.IsFinite(width) || !double.IsFinite(height))
        {
            return new NormPoint(0, 0);
        }

        return new NormPoint(Clamp01(x / width), Clamp01(y / height));
    }

    /// <summary>
    /// Clamp into <c>[0,1]</c>, mapping NaN to <c>0</c> — the same two-comparison
    /// form <c>CanvasGesture.Clamp01</c> uses, and for the same reason:
    /// <c>Math.Clamp(double.NaN, 0, 1)</c> RETURNS NaN. It neither throws nor
    /// clamps, which is exactly the value this guard exists to stop. Every
    /// comparison against NaN is false, so both branches fall through to
    /// <c>0</c>.
    ///
    /// <para>It is duplicated rather than shared because the alternative is widening
    /// a private member of another type into public surface to save four
    /// characters — and the constant that matters (the CONTRACT: NaN becomes zero)
    /// is asserted independently on both, so the two cannot drift silently.</para>
    /// </summary>
    private static double Clamp01(double v) => v > 1.0 ? 1.0 : v > 0.0 ? v : 0.0;

    /// <summary>
    /// The mirror's <c>canvas.annotations</c> array, or <see langword="null"/> when
    /// there is no canvas state, no annotations, or something that is not an array
    /// where one is expected.
    ///
    /// <para><b>⚠ THIS EXISTS BECAUSE <c>node?["canvas"]?["annotations"]</c> THROWS</b>
    /// — measured on .NET 9 during this same phase, not assumed. <c>JsonNode</c>'s
    /// STRING INDEXER itself raises <see cref="InvalidOperationException"/> ("The node
    /// must be of type 'JsonObject'") when the receiver is a <see cref="JsonValue"/> or
    /// a <see cref="JsonArray"/>, BEFORE any <c>GetValue&lt;T&gt;</c> is reached. The
    /// null-conditional operators defend against a MISSING node and against nothing
    /// else. <c>IntrospectionHook.DescribeCanvas</c>'s remarks record the probe and its
    /// <c>Child</c> helper is the same shape; this is that measurement applied to the
    /// stage's own two readers (review IN-01), which run on the UI thread inside the
    /// <c>ProjectChanged</c> callback where a throw is an UNHANDLED exception rather
    /// than a bad answer.</para>
    ///
    /// <para>Not reachable from a well-formed backend snapshot — which is exactly why
    /// it is worth closing cheaply rather than relying on: the cost of being wrong is
    /// the whole shell, and the cost of the guard is two type tests per project
    /// change on a cold path.</para>
    /// </summary>
    internal static JsonArray? AnnotationsNode(JsonNode? rawProject)
        => Child(Child(rawProject, "canvas"), "annotations") as JsonArray;

    /// <summary>
    /// Is a mark with this id present in the mirror's annotation array? The
    /// presence check the stage's commit hand-off waits on (D-13): the trail is held
    /// until this answers <see langword="true"/>, at which point
    /// <c>ApplyMirrorState</c> has already drawn the committed mark.
    ///
    /// <para>Reads by id ALONE and deliberately so — this asks "did the backend take
    /// it", not "can this build draw it". The second question is
    /// <see cref="ParseWhiteboardMarks"/>'s, and the gap between the two answers is
    /// recorded rather than papered over (review IN-02): a mark that lands in state
    /// but cannot be parsed here confirms the commit and then draws nothing, so
    /// <see cref="ParseWhiteboardMarks"/> reports its skip count and the caller
    /// LOGS it.</para>
    ///
    /// <para>Every hop is non-throwing for <see cref="AnnotationsNode"/>'s reason: an
    /// annotation node that is a bare value rather than an object would take the read
    /// down at <c>node?["id"]</c>.</para>
    /// </summary>
    internal static bool ContainsAnnotationId(JsonArray? annotations, string id)
    {
        if (annotations is null)
        {
            return false;
        }

        foreach (var node in annotations)
        {
            if (AsString(Child(node, "id")) == id)
            {
                return true;
            }
        }

        return false;
    }

    /// <summary>A child node, or <see langword="null"/> if the receiver is absent or is
    /// not a <see cref="JsonObject"/>. The non-throwing replacement for
    /// <c>node?[name]</c> — see <see cref="AnnotationsNode"/> for the measurement that
    /// made it necessary. Twin of <c>IntrospectionHook.Child</c>, which cannot be
    /// shared with this file: that one lives inside a <c>#if DEBUG</c> block in a
    /// WinUI-bearing assembly, and this one has to compile into the unit tier.</summary>
    private static JsonNode? Child(JsonNode? owner, string property)
        => owner is JsonObject obj && obj.TryGetPropertyValue(property, out var child)
            ? child
            : null;

    /// <summary>
    /// Project the mirror's raw <c>canvas.annotations</c> array down to the
    /// WHITEBOARD marks, flattened for drawing and in input order.
    ///
    /// <para><b>Reads the raw node, deliberately.</b> The source is
    /// <c>App.Mirror?.RawProject?["canvas"]?["annotations"]</c> — the same node
    /// <c>PreviewInkLayer.MirrorHasAnnotation</c> already reads. No typed
    /// <c>Annotation</c>/<c>CanvasState</c> model is added to the mirror's projected
    /// <c>Project</c> record: that file's own rule is "under-projecting is free; a
    /// wrong projection is not", and a render pass needs four fields, not a
    /// schema.</para>
    ///
    /// <para><b>Frame-linked marks are DROPPED</b>, and the drop is the point: a
    /// stage that drew Preview marks would put ink on the whiteboard that the user
    /// drew somewhere else entirely.</para>
    ///
    /// <para><b>Every malformed or unrecognized node is SKIPPED, never thrown</b>
    /// (T-60.2-02). The producer is the backend's own snapshot, so skip-and-continue
    /// is proportionate — but a render pass must not take the shell down over one
    /// node it does not understand, and one bad entry must not abort the marks after
    /// it. A node is skipped when it is not an object, carries no id, carries no
    /// shape object, names a kind this build does not know, or is missing any
    /// coordinate a kind requires. Skipping the WHOLE node on a bad point rather
    /// than the point alone is deliberate: a stroke silently missing a vertex draws
    /// a different shape than the user drew, which is worse than drawing
    /// nothing.</para>
    ///
    /// <para>The structural minimums mirror the backend's own
    /// (<c>normalize_and_validate_shape</c>): a stroke needs at least one point and a
    /// lasso at least three, so a node that could never have been accepted is not
    /// rendered as though it had been. An empty label text is NOT a rejection —
    /// the backend truncates label text and never refuses it, so an empty label is
    /// legal domain data that simply draws nothing.</para>
    /// </summary>
    internal static List<StageMark> ParseWhiteboardMarks(JsonArray? annotations)
        => ParseWhiteboardMarks(annotations, out _);

    /// <summary>
    /// <inheritdoc cref="ParseWhiteboardMarks(JsonArray?)"/>
    ///
    /// <para><b><paramref name="skipped"/> — HOW MANY WHITEBOARD MARKS THIS BUILD
    /// COULD NOT DRAW.</b> Skipping is right (a render pass must not die on one node)
    /// but skipping SILENTLY is not, and the difference matters here more than it
    /// usually would: the commit hand-off confirms by ID ALONE
    /// (<see cref="ContainsAnnotationId"/>), so a mark that lands in state and cannot
    /// be parsed here confirms the commit, clears the trail, and then draws
    /// nothing — the "surface that silently discards strokes" this whole phase exists
    /// to end, arriving through a narrow failure mode instead of a wide one
    /// (review IN-02).</para>
    ///
    /// <para>REPORTED rather than logged, because this file is compiled into the
    /// WinUI-free unit tier and cannot reach <c>App.LogDiagnostic</c>. The caller
    /// logs; see <c>Canvas.xaml.cs</c>'s <c>ApplyMirrorState</c>.</para>
    ///
    /// <para><b>A frame-linked mark is NOT counted.</b> Dropping those is the correct,
    /// deliberate behaviour this method exists for — counting them would make the
    /// signal fire on every ordinary Preview stroke and be ignored within a day.</para>
    /// </summary>
    internal static List<StageMark> ParseWhiteboardMarks(JsonArray? annotations, out int skipped)
    {
        skipped = 0;
        var marks = new List<StageMark>();
        if (annotations is null)
        {
            return marks;
        }

        foreach (var node in annotations)
        {
            var whiteboard = false;
            StageMark? mark;
            try
            {
                mark = ParseMark(node, out whiteboard);
            }
            catch (Exception)
            {
                // The per-node boundary. `GetValue<T>` throws on a type it did not
                // expect, and the whole reason this catch is here is that the next
                // node is still worth drawing.
                //
                // A node that THREW is counted whatever space it claimed: the throw
                // may well have happened before the space was even read, and a node
                // violent enough to raise is exactly the one worth reporting.
                mark = null;
                whiteboard = true;
            }

            if (mark is not null)
            {
                marks.Add(mark);
            }
            else if (whiteboard)
            {
                skipped++;
            }
        }

        return marks;
    }

    /// <summary>One node, or <see langword="null"/> when it is not a whiteboard mark
    /// this build can draw.
    ///
    /// <para><paramref name="whiteboard"/> separates the two reasons for a null: "not
    /// mine" (a frame-linked mark, or not an annotation object at all — correct, and
    /// silent) from "mine, and I could not draw it" (worth reporting, review
    /// IN-02).</para></summary>
    private static StageMark? ParseMark(JsonNode? node, out bool whiteboard)
    {
        whiteboard = false;

        if (node is not JsonObject obj)
        {
            return null;
        }

        if (AsString(obj["space"]) != WhiteboardSpace)
        {
            return null;
        }

        // Past this line every null return is a whiteboard mark that WILL NOT BE
        // DRAWN, and the caller is told how many there were.
        whiteboard = true;

        var id = AsString(obj["id"]);
        if (string.IsNullOrEmpty(id))
        {
            return null;
        }

        if (obj["shape"] is not JsonObject shape)
        {
            return null;
        }

        switch (AsString(shape["kind"]))
        {
            case "stroke":
                return PointListMark(id, StageMarkKind.Stroke, shape, minimumPoints: 1);

            case "lasso":
                return PointListMark(id, StageMarkKind.Lasso, shape, minimumPoints: 3);

            case "arrow":
            {
                if (AsPoint(shape["start"]) is not { } start ||
                    AsPoint(shape["end"]) is not { } end)
                {
                    return null;
                }

                return new StageMark(id, StageMarkKind.Arrow, new[] { start, end }, null);
            }

            case "label":
            {
                if (AsPoint(shape["position"]) is not { } position)
                {
                    return null;
                }

                var text = AsString(shape["text"]);
                return text is null
                    ? null
                    : new StageMark(id, StageMarkKind.Label, new[] { position }, text);
            }

            default:
                // An unknown kind is a newer backend talking to an older shell. Draw
                // nothing rather than guessing.
                return null;
        }
    }

    private static StageMark? PointListMark(
        string id, StageMarkKind kind, JsonObject shape, int minimumPoints)
    {
        if (shape["points"] is not JsonArray raw || raw.Count < minimumPoints)
        {
            return null;
        }

        var points = new List<NormPoint>(raw.Count);
        foreach (var entry in raw)
        {
            if (AsPoint(entry) is not { } point)
            {
                return null;
            }

            points.Add(point);
        }

        return new StageMark(id, kind, points, null);
    }

    /// <summary>A <c>{"x":…,"y":…}</c> node, or <see langword="null"/> when either
    /// coordinate is absent or not a finite number.</summary>
    private static NormPoint? AsPoint(JsonNode? node)
    {
        if (node is not JsonObject obj)
        {
            return null;
        }

        if (AsDouble(obj["x"]) is not { } x || AsDouble(obj["y"]) is not { } y)
        {
            return null;
        }

        return new NormPoint(x, y);
    }

    private static double? AsDouble(JsonNode? node)
        => node is JsonValue value && value.TryGetValue<double>(out var d) && double.IsFinite(d)
            ? d
            : null;

    private static string? AsString(JsonNode? node)
        => node is JsonValue value && value.TryGetValue<string>(out var s) ? s : null;
}
