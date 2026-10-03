using Microsoft.UI.Input;
using Microsoft.UI.Xaml.Controls;

namespace Rudis.Shell.Regions;

/// <summary>
/// <b>A <see cref="Grid"/> that lets its owner set a pointer cursor</b> — the one
/// thing WinUI 3 does not let you do from outside an element (quick task 260828-gb9,
/// deferred-items.md § D-60.2-12).
///
/// <para><c>UIElement.ProtectedCursor</c> is <c>protected</c>, and WinUI 3 has no
/// <c>Cursor</c> XAML attribute (that is WPF). So a plain <c>Grid</c> declared in
/// markup can never be given a cursor by its code-behind: the only legal reach is
/// from INSIDE a subclass. This type is that subclass and nothing else — no
/// behaviour, no handlers, no state.</para>
///
/// <para><b>THIS CLASS COMPILING IS THE PROOF.</b> The WinAppSDK 1.8 shape of
/// <c>ProtectedCursor</c> was not assumed from documentation or memory; it was
/// verified the way Phase 60.1 verified <c>FileOpenPicker.SuggestedStartFolder</c> —
/// by compiling it and reading the compiler's answer instead of shipping a guess. If
/// a future SDK removes or reshapes the member, this file is where the build breaks,
/// which is the correct place for it to break.</para>
///
/// <para><b>Why a LAYER and not a retype of <c>CanvasRoot</c></b> (the trap, recorded
/// so it is not re-derived): WinUI resolves the cursor by walking UP from the
/// hit-test target to the first ancestor with a non-null cursor, and an element's
/// default is "use my parent's", NOT "use the arrow". A cursor on the Canvas region
/// root — or on the <c>Canvas</c> UserControl — would therefore show the crosshair
/// over the tool palette, the region tag and the zoom chip too, since none of those
/// sets a cursor of its own. Z-order solves it structurally instead: see
/// <c>StageCursorLayer</c> in Canvas.xaml, declared BELOW all of that chrome.</para>
///
/// <para><c>internal</c> matches <c>DrawTool</c>'s reasoning (D-12): nothing in this
/// region is a public API for anyone else. XAML markup in this assembly can name an
/// internal type because the generated XAML code lands in the same assembly.</para>
/// </summary>
internal sealed partial class CursorHost : Grid
{
    /// <summary>The pointer cursor shown while this element is the hit-test target,
    /// or <see langword="null"/> to inherit whatever an ancestor says — which, with
    /// no ancestor setting one, is the system default arrow.</summary>
    internal InputCursor? Cursor
    {
        get => ProtectedCursor;
        set => ProtectedCursor = value;
    }
}
