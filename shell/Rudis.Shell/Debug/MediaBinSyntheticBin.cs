// Plan 53-04, Task 1 — SC-2's measurement INSTRUMENT.
//
// D-03 is the clause that actually costs something: "Virtualization must be PROVEN,
// not assumed — realized-container count against a synthetic large bin, not 'we used
// the virtualizing control.'" A realized-container count means nothing unless the bin
// is large enough that a virtualizing control must leave most of it unrealized. This
// file builds that bin.
//
// ⚠ THE ENTIRE FILE IS COMPILED OUT OF RELEASE — everything below, including the
// namespace declaration, sits inside the single DEBUG conditional-compilation block
// that opens on the next line and closes on the last. A Release build of Rudis.Shell
// carries NO type of this name at all: not disabled, not merely unreferenced, ABSENT
// from the assembly. That absence is asserted MECHANICALLY against the REAL built DLL,
// in BOTH directions, by shell/Rudis.Shell.Tests/ReleaseHookAbsenceTests.cs
// (`mediabin_synthetic_surface_absent_from_release_and_present_in_debug`). The Debug
// half is what stops a deleted feature from making the Release half vacuously green —
// 50-08's rule, extended rather than re-derived (T-53-05).
//
// Even in a Debug build it is ARGV-GATED: nothing here runs unless the synthetic-bin
// flag was on the command line (see App.StartupSyntheticMediaBin), following the
// --timeline-smoke / --place-on-timeline precedent. A normal F5 session never sees it.
#if DEBUG
using Rudis.Shell.Regions;

// ⚠ NAMED `Introspection`, not `Debug` — the FILE lives under `Debug/` (the plan's
// exact path), but a C# namespace literally named `Rudis.Shell.Debug` would shadow
// `System.Diagnostics.Debug` for every unqualified `Debug.WriteLine` already made from
// within the `Rudis.Shell` namespace. IntrospectionHook.cs records the compiler error
// that established this; it is not re-derived here.
namespace Rudis.Shell.Introspection;

/// <summary>
/// N synthetic <see cref="MediaBinTile"/>s for SC-2's virtualization measurement.
///
/// <para><b>Why a TEST INSTRUMENT is allowed to exist at all under CLAUDE.md rule 1.</b>
/// Rule 1 forbids building UI for an editing feature that has not been proven on real
/// video. This is not UI and it is not a feature: it never reaches a Release build (a
/// mechanical assertion against the real built DLL, not a promise), it is argv-gated
/// even in Debug, it dispatches no command and mutates no project, and it drives the
/// REAL <c>GridView</c> through the REAL <c>DataTemplate</c> — which is exactly what
/// SC-2's virtualization claim is about. The alternative — importing 5,000 real files,
/// or dispatching 5,000 <c>AddMediaBinItem</c> commands each emitting its own
/// structural <c>project:changed</c> — would not be a better proof of container
/// virtualization, only a slower one, and it would write 5,000 items into the
/// developer's real <c>%APPDATA%\app.rudis.desktop</c> store to do it.</para>
///
/// <para><b>Nothing here can leak into the user's project state.</b> The tiles are
/// handed straight to the control's <c>ItemsSource</c> and never travel the other way:
/// there is no command, no FFI call and no file write anywhere in this file or in the
/// region path that consumes it. The engine's store is not touched by this flag at all
/// — which is a stronger guarantee than pointing the launch at a temp store would be,
/// because it removes the write rather than redirecting it.</para>
///
/// <para><b><see cref="MediaBinTile.PosterPath"/> is deliberately null on every
/// tile.</b> This measures CONTAINER realization. 5,000 real poster decodes would
/// measure the disk instead, and D-08 already establishes that a null poster is the
/// NORMAL path rather than an error one — so the placeholder branch these tiles take is
/// the region's real behaviour, not a shortcut around it.</para>
/// </summary>
internal static class MediaBinSyntheticBin
{
    /// <summary>
    /// A hard harness ceiling. 5,000 is the number this plan measures at and 100,000 is
    /// roughly where building the list stops being a test and starts being a wait. The
    /// clamp lives here as well as at the argv parse so a future caller cannot bypass it.
    /// </summary>
    internal const int MaxTiles = 100_000;

    /// <summary>
    /// Build <paramref name="count"/> synthetic media tiles, clamped to
    /// <c>[0, <see cref="MaxTiles"/>]</c>. Every tile is a FRESH instance, because the
    /// items control identifies its realized containers BY REFERENCE
    /// (<see cref="MediaBinTile"/>'s own remarks) — a shared instance is how a recycled
    /// container ends up convinced it is already showing the right thing.
    /// </summary>
    internal static List<MediaBinTile> Build(int count)
    {
        var clamped = count < 0 ? 0 : count > MaxTiles ? MaxTiles : count;

        var tiles = new List<MediaBinTile>(clamped);
        for (var i = 0; i < clamped; i++)
        {
            var id = $"synthetic-{i}";
            var name = $"synthetic_{i:D5}.mp4";
            tiles.Add(new MediaBinTile
            {
                Kind = MediaBinTileKind.Media,

                // The SAME id/AutomationId shapes MediaBinTileFactory produces for a
                // real item, so the template binds and the automation peers are built
                // exactly as they are for real media. A tile shape that differed here
                // would make the realized-container count a measurement of a different
                // control than the one that ships.
                Id = id,
                AutomationId = $"MediaBin.Tile.{id}",
                DisplayText = name,
                FullText = name,
                BadgeText = "0:05",
                PosterPath = null,
                PlaceholderGlyph = "",
                IsVideo = true,
            });
        }

        return tiles;
    }
}
#endif
