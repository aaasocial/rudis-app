using Rudis.Shell.Mirror;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE MEDIABIN'S LEVEL COMPUTATION — a statement-for-statement port of v6.0's
// `frontend/src/mediabin.ts` (53-CONTEXT D-05).
// ============================================================================
//
// The backend (crates/core, Phase 25 LIB-01) models the library as:
//   - `Project.media_folders`: the canonical set of EXISTING non-root virtual
//     folder paths — segments joined by "/", with NO leading/trailing slash
//     (e.g. "broll/city"). Root is "" and is implicit (never in the list). An
//     EMPTY folder exists purely as an entry here.
//   - `MediaBinItem.folder`: the path of the folder the item sits directly in
//     ("" = root, the default).
//
// The panel renders ONE LEVEL AT A TIME (D-04): the current folder's immediate
// child folders as clickable tiles, then the items sitting directly in it, with a
// breadcrumb to walk back up. Hierarchy is entirely implicit in path-string
// prefixes, so a child of `a` is any path starting with `"a/"` — and the trailing
// slash is what keeps a name-prefix SIBLING (`ab`) from being mistaken for a child.
//
// WINUI-FREE BY RULE. This directory holds the MediaBin's PURE half; see the
// contract comment in `Rudis.Shell.Tests.csproj` and the mechanical enforcement in
// `MediaBinPurityGateTests`. Nothing here touches the filesystem either: folder
// path segments are OPAQUE DISPLAY STRINGS and are never joined to a real path
// (T-53-01). Real path walking stays backend-side, in `import.rs`'s
// canonicalize-then-walk with its symlink skip and depth/file clamps.
//
// ---------------------------------------------------------------------------
// THE THREE TRANSLATION TRAPS, all of which change behaviour SILENTLY
// ---------------------------------------------------------------------------
//
//  1. ORDINALITY. A JS `Set` dedupes by SameValueZero (code-unit equality) and JS
//     `<`/`>` on strings compare UTF-16 code units. C#'s defaults — `HashSet<string>`
//     with the default comparer is ordinal, but `OrderBy(x => x)` / `List<string>.Sort()`
//     are CULTURE-SENSITIVE — do not match. Every set, every sort and every
//     comparison below is therefore explicitly ordinal. `MediaBinLevelTests.
//     sort_is_ORDINAL_not_culture_sensitive` is the case that catches a regression
//     here; no twinned v6.0 case can, because every v6.0 fixture is same-case ASCII.
//  2. THE EMPTY-SEGMENT GUARD. `rest.Split('/')[0]` can be "" for a path like
//     "a//b"; the guard is what stops that producing an empty child tile.
//  3. ITEM ORDER. v6.0's `items.filter(...)` preserves the bin's ORIGINAL order.
//     The items half of the result is never sorted.

/// <summary>One child-folder tile at the level being rendered.</summary>
/// <param name="Path">Canonical full path, e.g. <c>"broll/city"</c> (no leading/trailing slash).</param>
/// <param name="Name">The segment beyond the current level — the display label.</param>
/// <param name="ItemCount">Items in this folder's WHOLE SUBTREE (itself + every
/// descendant), so a folder tile tells the user how much is inside without
/// drilling in.</param>
internal sealed record LevelFolder(string Path, string Name, int ItemCount);

/// <summary>One rendered MediaBin level: child folders, then this level's own items.</summary>
/// <param name="Folders">Immediate child folders, sorted ORDINALLY by name.</param>
/// <param name="Items">Items sitting DIRECTLY in the current folder (never
/// descendants), in the bin's original order.</param>
internal sealed record MediaBinLevelResult(
    IReadOnlyList<LevelFolder> Folders,
    IReadOnlyList<MediaBinItem> Items);

internal static class MediaBinLevel
{
    /// <summary>
    /// List ONE level of the library: the immediate child folders of
    /// <paramref name="currentPath"/> plus the items sitting directly in it.
    /// <paramref name="currentPath"/> is <c>""</c> for the root.
    ///
    /// <para>Child derivation runs over the UNION of <paramref name="mediaFolders"/>
    /// and every item's folder: an item whose folder is somehow NOT registered still
    /// surfaces its folder. That is deliberate and defensive — media must never
    /// silently disappear from the panel — and it is twinned by two of v6.0's own
    /// cases (the `orphan` and `ghost/deep` fixtures).</para>
    /// </summary>
    internal static MediaBinLevelResult ListLevel(
        IReadOnlyList<MediaBinItem> items,
        IReadOnlyList<string> mediaFolders,
        string currentPath)
    {
        // Root ("") matches every path; a non-root level matches only "<path>/…".
        var prefix = currentPath.Length == 0 ? string.Empty : currentPath + "/";

        var allPaths = new HashSet<string>(StringComparer.Ordinal);
        foreach (var path in mediaFolders)
        {
            if (path.Length != 0)
            {
                allPaths.Add(path);
            }
        }

        foreach (var item in items)
        {
            if (item.Folder.Length != 0)
            {
                allPaths.Add(item.Folder);
            }
        }

        // Immediate children, deduped by the FIRST segment beyond currentPath: both
        // "a/b" and "a/b/c" contribute exactly the one child "a/b".
        var childPaths = new HashSet<string>(StringComparer.Ordinal);
        foreach (var path in allPaths)
        {
            if (!path.StartsWith(prefix, StringComparison.Ordinal))
            {
                continue;
            }

            var rest = path[prefix.Length..];
            if (rest.Length == 0)
            {
                continue;                       // the current level itself, not a child
            }

            var segment = rest.Split('/')[0];
            if (segment.Length == 0)
            {
                continue;                       // trap 2: "a//b" must not yield an empty tile
            }

            childPaths.Add(prefix + segment);
        }

        var folders = new List<LevelFolder>(childPaths.Count);
        foreach (var path in childPaths)
        {
            var subtree = path + "/";
            var itemCount = 0;
            foreach (var item in items)
            {
                if (string.Equals(item.Folder, path, StringComparison.Ordinal)
                    || item.Folder.StartsWith(subtree, StringComparison.Ordinal))
                {
                    itemCount += 1;
                }
            }

            folders.Add(new LevelFolder(path, path[prefix.Length..], itemCount));
        }

        // Trap 1. Names are unique within a level (childPaths is a set keyed by
        // prefix+segment, and Name IS that segment), so the sort has no ties and
        // List.Sort's instability cannot show.
        folders.Sort(static (a, b) => StringComparer.Ordinal.Compare(a.Name, b.Name));

        // Trap 3: original order, never sorted.
        var levelItems = new List<MediaBinItem>();
        foreach (var item in items)
        {
            if (string.Equals(item.Folder, currentPath, StringComparison.Ordinal))
            {
                levelItems.Add(item);
            }
        }

        return new MediaBinLevelResult(folders, levelItems);
    }

    /// <summary>
    /// v6.0's STALE-PATH GUARD (<c>renderMediaBin</c>, <c>main.ts:464-472</c>): walk UP
    /// from <paramref name="currentPath"/> to the nearest level that still exists —
    /// ultimately the root <c>""</c>, which always does — so a render can never target a
    /// ghost folder.
    ///
    /// <para><b>Why this is not paranoia (T-53-07).</b> The drilled path is view state
    /// held by the region; the folder registry is owned by the backend and can change
    /// under it at any moment: a folder-delete command, a rename, an UNDO of the import
    /// that created it, or a full resync after a sequence gap. Every one of those arrives
    /// as an ordinary <c>project:changed</c>, and without this the next render would
    /// compute a level for a folder that is gone — an empty panel with a breadcrumb
    /// pointing at nothing, which reads as "your media vanished".</para>
    ///
    /// <para>EXISTS is the same two-part, ORDINAL test v6.0 uses: the path is registered
    /// in <paramref name="mediaFolders"/>, OR some item still lives in it or under it. The
    /// second half is what keeps a folder that was never registered — but demonstrably has
    /// media in it — from being walked away from (the same defensive union
    /// <see cref="ListLevel"/> derives its children from).</para>
    /// </summary>
    /// <returns>The deepest ancestor-or-self of <paramref name="currentPath"/> that
    /// exists; <c>""</c> when none does. Returns <paramref name="currentPath"/> unchanged
    /// when it is still real, which is the overwhelmingly common case.</returns>
    internal static string ResolveExistingFolder(
        string currentPath,
        IReadOnlyList<MediaBinItem> items,
        IReadOnlyList<string> mediaFolders)
    {
        var path = currentPath;
        while (path.Length != 0 && !FolderExists(path, items, mediaFolders))
        {
            // v6.0's `lastIndexOf("/")` then `slice(0, cut)`; -1 means the last segment,
            // so the next level up is the root. `LastIndexOf(char)` is ordinal by
            // definition — the string overload would not have been.
            var cut = path.LastIndexOf('/');
            path = cut < 0 ? string.Empty : path[..cut];
        }

        return path;
    }

    private static bool FolderExists(
        string path,
        IReadOnlyList<MediaBinItem> items,
        IReadOnlyList<string> mediaFolders)
    {
        foreach (var folder in mediaFolders)
        {
            if (string.Equals(folder, path, StringComparison.Ordinal))
            {
                return true;
            }
        }

        var subtree = path + "/";
        foreach (var item in items)
        {
            if (string.Equals(item.Folder, path, StringComparison.Ordinal)
                || item.Folder.StartsWith(subtree, StringComparison.Ordinal))
            {
                return true;
            }
        }

        return false;
    }
}
