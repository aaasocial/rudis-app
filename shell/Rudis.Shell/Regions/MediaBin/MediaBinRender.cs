namespace Rudis.Shell.Regions;

// ============================================================================
// THE MEDIABIN'S RE-RENDER GUARD — code-review finding IN-01.
// ============================================================================
//
// `MainWindow` calls `MediaBin.ApplyMirrorState` on EVERY `project:changed`, not
// only on the ones that touched the library: a split, a trim, a place-clip, a
// detach-audio and every undo/redo of those fires the same notification. Before
// this guard each of them reassigned the tile grid's `ItemsSource` to a brand-new
// list; a plain list carries no change notification, so the control cannot diff the
// new list against the old one by identity and re-realizes every visible container
// from phase 0 — which runs `ResetThumb`, clears each thumbnail back to its
// placeholder glyph, and makes every poster on screen decode off disk again.
// Trimming one clip blanked and reloaded the entire visible bin.
//
// ---------------------------------------------------------------------------
// WHY A GUARD AND NOT AN OBSERVABLE COLLECTION MUTATED IN PLACE
// ---------------------------------------------------------------------------
//
// The obvious alternative — hold one observable collection of tiles and add /
// remove / replace into it — was considered and REJECTED, because it fights a
// deliberate invariant rather than an accident. `ApplyMirrorState` assigns "a FRESH
// list of FRESH tiles every time" ON PURPOSE, and `MediaBinTile` is a plain class
// rather than a `record` for exactly the same reason: the items control identifies
// its realized containers BY REFERENCE, so tile freshness is what stops a recycled
// container from concluding it is already showing the right thing and keeping the
// PREVIOUS item's poster under the new item's name (D-06/D-07, and `MediaBinTile`'s
// own remarks, which `MediaBinTileTests.automation_ids_are_stable_and_unique` pins
// from both sides). Reusing tile instances would trade the single headline
// correctness guard this region was designed around for a frame of paint.
//
// This guard leaves that invariant completely intact. The tiles are still rebuilt
// fresh on every apply; the ONLY thing it decides is whether an unchanged level is
// pushed at the control at all.
//
// ---------------------------------------------------------------------------
// ⚠ THE COMPARISON IS BY VALUE, AND IT HAS TO BE
// ---------------------------------------------------------------------------
//
// Reference equality on the rebuilt list is NEVER true by construction — a guard
// written that way would be dead code that silently never fires, and the flicker
// would still be there with a green test claiming otherwise. So `Matches` compares
// what the level actually RENDERS: every field of every tile, in order, plus the
// drilled folder and the remembered selection.
//
// ORDINAL EVERYWHERE, matching the rest of this directory (`MediaBinLevel`'s trap
// 1). A culture-sensitive comparison here could call two visibly different names
// equal and skip a render that was needed — the same class of silent bug, arriving
// through the same door.
//
// The failure mode to fear is the AGGRESSIVE one. A guard that misses a real change
// leaves the region showing stale data, which is strictly worse than the flicker it
// replaced. Two things hold it honest:
//
//   1. `MediaBinRenderGuardTests` proves BOTH directions — an unrelated timeline
//      edit driven through the real mirror ladder does NOT re-render, and every
//      kind of library change DOES.
//   2. `SameTile` is walked REFLECTIVELY over `MediaBinTile`'s properties by
//      `every_rendered_tile_field_is_compared`, so a field added to the tile later
//      cannot quietly fall out of the comparison and turn this into a stale-data
//      bug months from now.
//
// WINUI-FREE BY RULE, like everything else in this directory (see the contract
// comment in `Rudis.Shell.Tests.csproj`, enforced by `MediaBinPurityGateTests`) —
// which is what lets the guard be unit-tested with no window at all.

/// <summary>
/// A snapshot of what ONE rendered MediaBin level SHOWS, kept so the next apply can
/// ask whether re-rendering would change anything before it blanks the posters.
///
/// <para>Immutable by construction: <see cref="MediaBinTile"/>'s properties are
/// <c>init</c>-only and <c>MediaBinTileFactory.ForLevel</c> hands back a list nobody
/// mutates afterwards, so a captured render cannot drift out from under the
/// comparison.</para>
/// </summary>
internal sealed class MediaBinRender
{
    private readonly string _folder;
    private readonly string? _selectedMediaId;
    private readonly IReadOnlyList<MediaBinTile> _tiles;

    internal MediaBinRender(string folder, string? selectedMediaId, IReadOnlyList<MediaBinTile> tiles)
    {
        _folder = folder;
        _selectedMediaId = selectedMediaId;
        _tiles = tiles;
    }

    /// <summary>
    /// Would rendering <paramref name="tiles"/> for <paramref name="folder"/> with
    /// <paramref name="selectedMediaId"/> selected produce the level this snapshot
    /// already describes?
    ///
    /// <para>All three inputs are compared, and none is redundant:</para>
    /// <list type="bullet">
    /// <item>The <b>tiles</b> are the level itself — an item added, removed,
    /// reordered, renamed, re-badged or re-postered all land here.</item>
    /// <item>The <b>folder</b> is what the breadcrumb and the empty-state copy are
    /// computed from. Two different folders can only produce value-equal tiles in
    /// pathological cases (ids are media ids or full canonical folder paths), but
    /// this is the cheap check that means the argument never has to be made.</item>
    /// <item>The <b>selected id</b> is remembered across renders even while the item
    /// is off-level (see <c>MediaBin._selectedMediaId</c>), so it can change with no
    /// change to the tiles at all — and the render re-asserts the control's selection
    /// from it.</item>
    /// </list>
    /// </summary>
    internal bool Matches(string folder, string? selectedMediaId, IReadOnlyList<MediaBinTile> tiles)
    {
        if (!string.Equals(_folder, folder, StringComparison.Ordinal))
        {
            return false;
        }

        if (!string.Equals(_selectedMediaId, selectedMediaId, StringComparison.Ordinal))
        {
            return false;
        }

        // Order is part of the render: v6.0 puts child folders first and then the
        // level's own items in the bin's ORIGINAL order (`MediaBinLevel` trap 3), so
        // a reordered bin is a changed bin even when the set is identical.
        if (_tiles.Count != tiles.Count)
        {
            return false;
        }

        for (var i = 0; i < tiles.Count; i++)
        {
            if (!SameTile(_tiles[i], tiles[i]))
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>
    /// Do these two tiles render identically? EVERY property of
    /// <see cref="MediaBinTile"/> is compared — including the ones that are derived
    /// from others today (<c>AutomationId</c> from <c>Kind</c> + <c>Id</c>), because
    /// a comparison that assumes a derivation breaks silently the day the derivation
    /// changes, and because <c>AutomationId</c> is a real observable that UIA callers
    /// address the region by.
    ///
    /// <para>⚠ This is deliberately NOT <c>Equals</c>/<c>GetHashCode</c> on
    /// <see cref="MediaBinTile"/> itself. Giving the tile value equality is precisely
    /// what the class refuses to do — see its remarks — because the items control
    /// identifies containers by reference. The comparison lives out here so the tile
    /// stays reference-identified while the GUARD can still ask about values.</para>
    /// </summary>
    internal static bool SameTile(MediaBinTile a, MediaBinTile b)
        => a.Kind == b.Kind
            && a.IsAudio == b.IsAudio
            && a.IsVideo == b.IsVideo

            // ⚠ Plan 60.1-07. `IsOffline` is the one field on this tile that can change
            // WITHOUT the library changing: nothing was added, removed, renamed or
            // re-ordered — a file simply stopped being on disk (or came back). Left out
            // of this comparison the guard would call the level unchanged and skip the
            // push, so the tile would be offline in memory and undimmed on screen, and
            // restoring the file would never clear the ⚠ either. The reflective gate in
            // `MediaBinRenderGuardTests` reddened on precisely this before it was added
            // — which is what that gate is for.
            && a.IsOffline == b.IsOffline

            // ⚠ Plan 63-04. `ProxyState` is the SECOND field that can change with no
            // change to the library at all: nothing was imported, renamed or moved — a
            // background encoder simply took the permit, or gave it back. Left out of
            // this comparison the poll would move the value and the tile would never
            // repaint, which is the same silent failure `IsOffline` had and the same
            // gate (`every_rendered_tile_field_is_compared`) catches it.
            && string.Equals(a.ProxyState, b.ProxyState, StringComparison.Ordinal)

            // ⚠ Plan 71-03. `ProxyProgressPercent` is the THIRD: the encoder advanced a
            // whole percent and nothing else about the library moved. Whole-percent
            // quantisation is what bounds these repaints (T-71-16). The reflective gate
            // reddened on this field before this line existed.
            && a.ProxyProgressPercent == b.ProxyProgressPercent
            && string.Equals(a.Id, b.Id, StringComparison.Ordinal)
            && string.Equals(a.AutomationId, b.AutomationId, StringComparison.Ordinal)
            && string.Equals(a.DisplayText, b.DisplayText, StringComparison.Ordinal)
            && string.Equals(a.FullText, b.FullText, StringComparison.Ordinal)
            && string.Equals(a.BadgeText, b.BadgeText, StringComparison.Ordinal)
            && string.Equals(a.PosterPath, b.PosterPath, StringComparison.Ordinal)
            && string.Equals(a.PlaceholderGlyph, b.PlaceholderGlyph, StringComparison.Ordinal);

    /// <summary>
    /// The tile a render would hand the control as its selection: the MEDIA tile on
    /// this level carrying <paramref name="selectedMediaId"/>, or <c>null</c>.
    ///
    /// <para>Lifted out of <c>ApplyMirrorState</c> so the guard and the render decide
    /// selection with the SAME function rather than two copies of one loop. Folder
    /// tiles are skipped: the remembered selection is a media selection (the
    /// handoff's "one primary selection" is about media), and a folder path could
    /// otherwise collide with a media id.</para>
    /// </summary>
    internal static MediaBinTile? SelectionFor(IReadOnlyList<MediaBinTile> tiles, string? selectedMediaId)
    {
        if (selectedMediaId is null)
        {
            return null;
        }

        foreach (var tile in tiles)
        {
            if (tile.Kind == MediaBinTileKind.Media
                && string.Equals(tile.Id, selectedMediaId, StringComparison.Ordinal))
            {
                return tile;
            }
        }

        return null;
    }

    /// <summary>
    /// Is the control's CURRENT selection already the one a render would set?
    ///
    /// <para>This is the last mile of the guard and it is what makes skipping
    /// PROVABLY invisible rather than merely usually-invisible. The control's
    /// selection can move without <c>_selectedMediaId</c> moving — arrow-keying onto
    /// a FOLDER tile does exactly that — and the old unconditional render would have
    /// yanked it back. Comparing here means the guard fires only when re-rendering
    /// could not have changed anything the user can see, so it preserves the previous
    /// behaviour instead of quietly redefining it.</para>
    ///
    /// <para>Compared by kind and id rather than by reference, because the tiles are
    /// rebuilt fresh on every apply and the control is holding an instance from an
    /// earlier one.</para>
    /// </summary>
    internal static bool SameSelection(MediaBinTile? current, MediaBinTile? next)
    {
        if (current is null || next is null)
        {
            return current is null && next is null;
        }

        return current.Kind == next.Kind
            && string.Equals(current.Id, next.Id, StringComparison.Ordinal);
    }
}
