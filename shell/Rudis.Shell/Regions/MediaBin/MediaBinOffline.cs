using System.Text.Json;
using Rudis.Shell.Interop;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE MEDIABIN'S OFFLINE / MISSING-FILE STATE — plan 60.1-07, relink slice 1.
// ============================================================================
//
// The design handoff has specified this MediaBin state since before any of this
// existed (`design_handoff_rudis_editor/README.md:126`):
//
//     States — ... *offline/missing file* (tile dimmed + ⚠ "relink").
//
// and `ARCHITECTURE.md:366` has named the status value behind it ('missing' =
// relink needed). Nothing rendered it because nothing could COMPUTE it. Plan
// 60.1-02's `missing_media`, 60.1-03's `rudis_get_missing_media` and 60.1-04's
// `GetMissingMediaAsync` closed that; this is the projection between the wire and
// the tile.
//
// ---------------------------------------------------------------------------
// WHY THIS IS A SEPARATE FILE IN THE PURE DIRECTORY, NOT A STATIC ON THE REGION
// ---------------------------------------------------------------------------
//
// `Rudis.Shell.Tests` does NOT project-reference the WinExe app — it LINKS source
// files, and `Regions/MediaBin/**/*.cs` is one of the globs. A member on
// `MediaBin.xaml.cs` is therefore unreachable from a test no matter what its
// accessibility says, because that file is never compiled into the test host at
// all. This is the same move plan 60.1-05 made for `ToolbarProjectRoutes` and plan
// 52-13 made for `PreviewMonitorCommand`, for the same reason, and it is why the
// projection is asserted directly rather than inferred from a rendered tile.
//
// WinUI-free by rule, like everything else in this directory: `System.Text.Json`
// and the interop envelope type, nothing more. The gate
// (`MediaBinPurityGateTests`) scans this file with the rest.
//
// ---------------------------------------------------------------------------
// ⚠ THE FAILURE DIRECTION IS ASYMMETRIC, AND THAT IS THE DESIGN (T-60.1-20)
// ---------------------------------------------------------------------------
//
// This projection sits on a trust boundary: a list of ids from the engine decides
// how tiles RENDER. There are three things it could do with a payload it cannot
// read, and only one of them is acceptable.
//
//   * THROW — unacceptable. The caller is a poll continuation on the UI thread; an
//     exception there is a fault on the dispatcher over a decoration.
//   * Report EVERYTHING offline — the quiet disaster. A beginner opening a
//     perfectly healthy project and finding every clip marked broken would conclude
//     the app had lost their work. There is no payload shape that justifies it.
//   * Report NOTHING offline — correct. It is exactly the state Rudis was in before
//     this feature existed: a moved file still fails later, at play time, which is
//     no worse than yesterday. Under-reporting is recoverable; over-reporting is a
//     false accusation about the user's own files.
//
// So every unreadable shape yields an EMPTY set, and `MediaBinTileTests` enumerates
// the shapes rather than asserting the intention.

/// <summary>
/// The MediaBin's offline-media projection and the copy that goes with it — the pure
/// half of relink slice 1.
/// </summary>
internal static class MediaBinOffline
{
    /// <summary>
    /// "Nothing is offline" — the value the region starts at, the value a failed poll
    /// degrades to, and the value passed when no answer has arrived yet.
    ///
    /// <para>Ordinal, like every other comparison in this directory: media ids are
    /// opaque backend strings, not paths, so there is no case-folding question to get
    /// wrong and folding it would let one id mark a DIFFERENT item's tile.</para>
    /// </summary>
    internal static readonly IReadOnlySet<string> None =
        new HashSet<string>(StringComparer.Ordinal);

    /// <summary>
    /// What the TILE says. U+26A0 WARNING SIGN plus the handoff's own word
    /// (<c>README:126</c>), which CLAUDE.md rule 7 makes the contract rather than a
    /// suggestion.
    ///
    /// <para>⚠ A plain Unicode pictograph, NOT an icon-font Private Use Area codepoint.
    /// Phase 50 MEASURED that a PUA glyph with a pinned-but-absent font family renders
    /// NOTHING on Windows 10 19045 — the machine this ships to. Same accepted cost as
    /// this region's other two glyphs: it may resolve as a colour emoji rather than a
    /// monochrome mark, depending on the font stack (<c>MediaBinTileFactory</c>'s
    /// recorded trade, applied again).</para>
    ///
    /// <para>The word "relink" is jargon to the beginner Rudis is for, and it is kept
    /// anyway because 140px of tile has no room for a sentence and the handoff named it.
    /// <see cref="BadgeHelpText"/> is how that debt is paid.</para>
    /// </summary>
    internal const string BadgeText = "⚠ relink";

    /// <summary>
    /// What the TOOLTIP and the UIA name say — the same fact in a beginner's language,
    /// carried where there is room for it so the badge is never the only explanation.
    ///
    /// <para>It promises only what slice 1 can keep. There is no in-app repair yet
    /// (slice 2 — Resolve-style folder-level relink — is deferred and specified in this
    /// phase's <c>deferred-items.md</c>), so the action it names is the one that
    /// actually works today: put the file back. A tooltip offering a Locate… button that
    /// does not exist would be worse than the jargon it replaced.</para>
    /// </summary>
    internal const string BadgeHelpText =
        "File missing. Rudis cannot find this file on your computer — it was moved, "
        + "renamed or deleted. Put it back where it was to use this clip again.";

    /// <summary>
    /// How far the poster/placeholder area is dimmed.
    ///
    /// <para>Deliberately a DIM and not a blank: a beginner has to be able to see WHICH
    /// clip is broken, and the poster survives the file moving (it is a cached PNG in app
    /// data, which is not where the source lives — <c>import.rs</c>'s
    /// <c>poster_cache_dir</c>). The value is the shell's existing dim, borrowed from the
    /// Timeline's drop ghost (<c>Timeline.xaml:289</c>) so the app has ONE dim level
    /// rather than two that differ for no reason.</para>
    ///
    /// <para>An opacity, not a colour: rule 7's raw-hex prohibition is about the token
    /// dictionary owning every COLOUR, and the honest way to dim under that rule is to
    /// reduce the element rather than to invent a greyed twin of six brushes. The
    /// handoff's own colour table defines no "disabled" or "warning" ramp — see the
    /// note in <c>Theme/Tokens.xaml</c> where plan 54-03 declined to invent an amber
    /// for the same reason.</para>
    /// </summary>
    internal const double DimOpacity = 0.35;

    /// <summary>
    /// The badge's UIA id, so plan 60.1-09's gate can ask "is THIS clip marked offline?"
    /// rather than only "is something on screen dimmed".
    ///
    /// <para>A distinct prefix from the tile's own <c>MediaBin.Tile.{id}</c>, for the
    /// same reason the folder tiles have one: two different observables about the same
    /// item must not answer to the same name.</para>
    /// </summary>
    internal static string BadgeAutomationId(string mediaId) => "MediaBin.Offline." + mediaId;

    /// <summary>
    /// A <c>rudis_get_missing_media</c> result → the set of media ids whose files are not
    /// on disk right now.
    ///
    /// <para>The happy payload is <c>{"Ok": ["&lt;media id&gt;", ..]}</c>. EVERY other
    /// shape — a domain error, a transport fault, an object where an array belongs,
    /// unparseable bytes — yields an EMPTY set and never throws. See this file's header
    /// for why that direction is the only acceptable one.</para>
    ///
    /// <para>Non-string elements inside a well-formed array are SKIPPED rather than
    /// fatal: the ids that are ids remain usable, and discarding them because a sibling
    /// was a number would under-report a real breakage for no gain. An empty string is
    /// dropped too — it can match no tile, and admitting it would make the set's count
    /// lie about how many files are missing.</para>
    /// </summary>
    internal static IReadOnlySet<string> IdsFrom(RudisResult<JsonElement> result)
    {
        if (result.Kind != RudisResultKind.Ok)
        {
            return None;
        }

        var payload = result.Value;
        if (payload.ValueKind != JsonValueKind.Array)
        {
            return None;
        }

        var ids = new HashSet<string>(StringComparer.Ordinal);

        foreach (var element in payload.EnumerateArray())
        {
            if (element.ValueKind != JsonValueKind.String)
            {
                continue;
            }

            var id = element.GetString();
            if (!string.IsNullOrEmpty(id))
            {
                ids.Add(id);
            }
        }

        return ids;
    }
}
