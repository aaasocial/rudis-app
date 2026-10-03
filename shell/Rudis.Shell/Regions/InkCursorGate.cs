namespace Rudis.Shell.Regions;

/// <summary>
/// <b>Which tools want an ink cursor — the CURSOR half of the tool gate</b>
/// (deferred-items.md § D-60.2-12, quick task 260828-gb9).
///
/// <para>This is the hover-time sibling of the press-time <c>tool != Pointer</c>
/// clause inside <see cref="CanvasStageGesture.ShouldStartGesture"/>: the same
/// single question — "is a drawing tool selected?" — asked at the moment the
/// pointer merely ARRIVES rather than the moment it is pressed. One predicate, so
/// the two ink surfaces (the Canvas stage and <c>PreviewInkLayer</c>) cannot drift
/// into disagreeing about what the pointer should look like.</para>
///
/// <para><b>Why this defect was worth its own file.</b> Phase 60.2 made the stage
/// genuinely drawable and proved it on real project state — and the owner then ran
/// <c>60.2-HUMAN-UAT.md</c> check 1 on that build and answered <i>"No — still looks
/// inert"</i>, reproducing his ORIGINAL bug report (<i>"I can't draw on the canvas
/// section"</i>) against a surface whose stroke fidelity he had just passed in the
/// same session. The state was right and the surface looked dead: with no cursor
/// affordance the first feedback that anything is drawable arrives only after the
/// user has already committed to a drag.</para>
///
/// <para><b>Why it is a separate file at all</b> — the 51-05 / 52-13 / 60.1-05 /
/// 60.2-01 move, made again. <c>Rudis.Shell.Tests.csproj</c> compiles an explicit
/// ALLOW-LIST and never a WinUI-bearing region file, so this rule written beside the
/// pointer handlers in <c>Canvas.xaml.cs</c> or inside <c>PreviewInkLayer.xaml.cs</c>
/// would be permanently unreachable from the unit tier whatever its accessibility
/// said. It is not written into <c>CanvasStageGesture.cs</c> or
/// <c>CanvasGesture.cs</c> because both are explicitly out of this task's scope.</para>
///
/// <para><b>WinUI-FREE BY CONSTRUCTION.</b> No <c>Microsoft.UI.*</c> using, and no
/// mention of a cursor TYPE: this file decides <i>whether</i>, and the two WinUI
/// halves decide <i>which</i> (both currently
/// <c>InputSystemCursorShape.Cross</c>). Adding an <c>InputCursor</c> here would drag
/// WinUI into the unit tier and undo the whole point of the split.</para>
/// </summary>
internal static class InkCursorGate
{
    /// <summary>
    /// <see langword="true"/> when <paramref name="tool"/> draws, so the pointer
    /// should show the ink cursor; <see langword="false"/> for
    /// <see cref="DrawTool.Pointer"/>, the tool that makes both ink surfaces inert
    /// and must therefore keep the default arrow — a crosshair over a surface that
    /// will swallow nothing is a lie about what a click does, which is the mirror
    /// image of the defect this gate fixes.
    /// </summary>
    internal static bool WantsInkCursor(DrawTool tool) => tool != DrawTool.Pointer;
}
