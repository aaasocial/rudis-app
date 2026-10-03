namespace Rudis.Shell.Regions;

// ============================================================================
// THE MEDIABIN'S BREADCRUMB — a port of v6.0's `renderMediaBin` breadcrumb block
// (`frontend/src/main.ts:474-502`), minus the DOM.
// ============================================================================
//
// v6.0 builds the trail in three statements and this is the same three:
//
//   const segments = currentMediaFolder === "" ? [] : currentMediaFolder.split("/");
//   const crumbs = [{ label: "Media", path: "" }];
//   segments.forEach((segment, i) => crumbs.push({ label: segment,
//                                                  path: segments.slice(0, i + 1).join("/") }));
//
// ...then renders every crumb as a button, disabling the LAST one because it is the
// level already on screen (main.ts:488-494). `IsCurrent` carries that here, so the
// XAML never has to know about indices.
//
// The root crumb is ALWAYS present and ALWAYS reads "Media" — that word is v6.0's,
// not this port's, and it is the reason an empty bin still shows a trail rather than
// an empty strip.
//
// WINUI-FREE BY RULE (the directory contract, enforced by MediaBinPurityGateTests).
// Path segments are OPAQUE DISPLAY STRINGS: nothing here touches the filesystem, and
// nothing joins a segment to a real path (T-53-01).
//
// ORDINALITY. `Split('/')` is a character split, so it matches JS exactly. No
// comparison, no sort and no set lives in this file, which is why — unlike
// `MediaBinLevel` — it needs no explicit ordinal comparer anywhere.

/// <summary>One crumb in the MediaBin's breadcrumb trail.</summary>
/// <param name="Label">The word shown: <c>"Media"</c> for the root, else the segment.</param>
/// <param name="Path">The canonical folder path this crumb navigates to
/// (<c>""</c> = root). Clicking a crumb sets the region's drilled path to exactly this.</param>
/// <param name="IsCurrent">True for the LAST crumb only — the level already rendered.
/// v6.0 disables it (main.ts:493) so the trail can never navigate to where it already
/// is.</param>
internal sealed record Crumb(string Label, string Path, bool IsCurrent)
{
    /// <summary>
    /// <c>"MediaBin.Crumb.&lt;{path}&gt;"</c>, with the root crumb using the literal
    /// <c>"MediaBin.Crumb.root"</c> because its path is the empty string and an
    /// automation id ending in a dot is not addressable.
    ///
    /// <para>⚠ THE ANGLE BRACKETS ARE LOAD-BEARING, not decoration. The naive
    /// <c>"MediaBin.Crumb." + Path</c> COLLIDES with the root sentinel the moment a user
    /// imports a top-level folder literally named <c>root</c> — a perfectly legal Windows
    /// directory name, reachable through the real <c>+ Import folder…</c> affordance this
    /// region ships. Drilled in, the trail would hold TWO crumbs advertising
    /// <c>"MediaBin.Crumb.root"</c> at once, and a <c>FindElement</c>-by-id caller would
    /// get whichever the tree walk reached first. An ambiguously-named control is as
    /// untestable as an unnamed one.</para>
    ///
    /// <para>Wrapping closes it TWICE OVER. Structurally: every non-root id now begins
    /// <c>"MediaBin.Crumb.&lt;"</c> and the sentinel does not, so root-vs-non-root can
    /// never collide no matter WHAT the path contains — the guarantee does not rest on
    /// the character choice. And by character: <c>&lt;</c> and <c>&gt;</c> are among the
    /// characters Windows forbids in a path segment (<c>&lt; &gt; : " / \ | ? *</c>), so
    /// the brackets can never be mistaken for part of a real folder name. <c>/</c> stays
    /// legal INSIDE the wrapper because it is this model's own segment separator.</para>
    ///
    /// <para>The CLOSING bracket earns its keep separately: it keeps prefix matching
    /// unambiguous, so a search for <c>"MediaBin.Crumb.&lt;broll&gt;"</c> cannot also
    /// match <c>"MediaBin.Crumb.&lt;broll/city&gt;"</c> the way a bare suffix would.</para>
    ///
    /// <para>Computed HERE rather than in the XAML so it is unit-testable without a
    /// window — the same bargain every other string in this region makes.</para>
    /// </summary>
    internal string AutomationId =>
        Path.Length == 0 ? "MediaBin.Crumb.root" : "MediaBin.Crumb.<" + Path + ">";

    /// <summary>
    /// U+203A SINGLE RIGHT-POINTING ANGLE QUOTATION MARK, or the empty string for the
    /// root crumb — v6.0's separator (main.ts:485), emitted BEFORE every crumb except
    /// the first.
    ///
    /// <para>"Is this the first crumb?" is expressible from the crumb alone: the root
    /// is the only crumb whose path is empty, and it is always index 0. Carrying it as
    /// a plain string keeps the item template free of a visibility converter — a
    /// converter would be a second place for the rule to live, and it would need a
    /// WinUI type in a file that is not allowed one.</para>
    /// </summary>
    internal string SeparatorText => Path.Length == 0 ? "" : "›";

    /// <summary>
    /// The inverse of <see cref="IsCurrent"/>, as a property rather than as a negation
    /// in the markup.
    ///
    /// <para>The binding language does have a negation operator, but expressing the
    /// rule HERE keeps it unit-testable and keeps the item template a straight
    /// property-to-property map — the same reason <see cref="AutomationId"/> and
    /// <see cref="SeparatorText"/> are computed here rather than in XAML.</para>
    /// </summary>
    internal bool CanNavigate => !IsCurrent;
}

internal static class MediaBinBreadcrumb
{
    /// <summary>The root crumb's label. v6.0's word, verbatim (main.ts:477).</summary>
    internal const string RootLabel = "Media";

    /// <summary>
    /// The full trail for a drilled path: the root crumb, then one crumb per segment,
    /// with the last marked current.
    ///
    /// <para><c>Build("")</c> is a ONE-element list, never an empty one — an empty bin
    /// still shows "Media", exactly as v6.0 does.</para>
    /// </summary>
    /// <param name="currentPath">The drilled folder path (<c>""</c> = root). Already
    /// resolved against reality by <see cref="MediaBinLevel.ResolveExistingFolder"/>
    /// before it reaches here.</param>
    internal static IReadOnlyList<Crumb> Build(string currentPath)
    {
        if (currentPath.Length == 0)
        {
            return [new Crumb(RootLabel, "", true)];
        }

        var segments = currentPath.Split('/');
        var crumbs = new List<Crumb>(segments.Length + 1)
        {
            new(RootLabel, "", false),
        };

        // v6.0's `segments.slice(0, i + 1).join("/")` expressed without allocating a
        // slice per segment: the prefix of `currentPath` up to the end of segment i is
        // the same string, and the running offset is what the join would have produced.
        var offset = 0;
        for (var i = 0; i < segments.Length; i++)
        {
            offset += segments[i].Length;
            var path = currentPath[..offset];
            offset += 1;                      // step past the '/' for the next segment

            crumbs.Add(new Crumb(segments[i], path, i == segments.Length - 1));
        }

        return crumbs;
    }
}
