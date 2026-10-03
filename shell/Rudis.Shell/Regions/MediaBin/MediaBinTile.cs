namespace Rudis.Shell.Regions;

/// <summary>Which of the two things a level can contain this tile is.</summary>
internal enum MediaBinTileKind
{
    Media,
    Folder,
}

/// <summary>
/// One MediaBin tile's fully-resolved PRESENTATION — every string the XAML will
/// show, decided before any control exists.
///
/// <para>Deliberately a plain CLASS, not a <c>record</c>: a record's value equality
/// would make two tiles built from the same item indistinguishable, and the items
/// control identifies its realized containers BY REFERENCE. A rebuilt level must
/// therefore produce FRESH instances, or a recycled container can keep showing a
/// stale poster while believing it is already up to date.
/// (<c>MediaBinTileTests.automation_ids_are_stable_and_unique</c> pins both halves:
/// the ids are stable, the objects are not the same objects.)</para>
///
/// <para>WinUI-free by rule — see the contract comment in
/// <c>Rudis.Shell.Tests.csproj</c>, enforced by <c>MediaBinPurityGateTests</c>.</para>
/// </summary>
internal sealed class MediaBinTile
{
    internal MediaBinTileKind Kind { get; init; }

    /// <summary>The media item's id, or the folder's canonical path.</summary>
    internal string Id { get; init; } = "";

    /// <summary><c>"MediaBin.Tile.{id}"</c> | <c>"MediaBin.Folder.{path}"</c>. The
    /// two prefixes are what keep a folder named <c>m1</c> from colliding with a
    /// media item whose id is <c>m1</c>.</summary>
    internal string AutomationId { get; init; } = "";

    /// <summary>Middle-ellipsized to <see cref="MediaBinTileFactory.MaxNameChars"/>.</summary>
    internal string DisplayText { get; init; } = "";

    /// <summary>Untruncated — the tooltip, and the UIA Name a screen reader or a
    /// FlaUI test reads.</summary>
    internal string FullText { get; init; } = "";

    /// <summary><c>"0:14"</c> | <c>"1:02:05"</c> | <c>"IMG"</c> | a folder's item count.</summary>
    internal string BadgeText { get; init; } = "";

    /// <summary>Absolute path to the poster PNG on disk. NULL IS NORMAL (D-08):
    /// audio never has one, and video poster generation is allowed to fail.</summary>
    internal string? PosterPath { get; init; }

    /// <summary><c>"♪"</c> for audio · <c>"\U0001F5C0"</c> for a folder ·
    /// <c>""</c> otherwise (the XAML draws a neutral fill).</summary>
    internal string PlaceholderGlyph { get; init; } = "";

    internal bool IsAudio { get; init; }

    internal bool IsVideo { get; init; }

    /// <summary>
    /// The file this item points at IS NOT ON DISK RIGHT NOW — the handoff's
    /// <c>README:126</c> state, verbatim: <i>"offline/missing file (tile dimmed + ⚠
    /// 'relink')"</i>, and <c>ARCHITECTURE.md:366</c>'s status value
    /// (<c>'missing' = relink needed</c>).
    ///
    /// <para>An OVERLAY on the tile, never a different tile: every other property is
    /// byte-identical to the same item's online tile
    /// (<c>MediaBinTileTests.an_id_in_the_offline_set_flips_is_offline_and_nothing_else</c>).
    /// A missing file changes whether the tile is dimmed, never what it says.</para>
    ///
    /// <para><b>A LIVE ANSWER, not a stored one.</b> It is computed from the id set
    /// <c>rudis_get_missing_media</c> reports — a POLL, so a file restored while the
    /// project is open clears the state without reopening it. Nothing about it is
    /// persisted: relink slice 1 is strictly read-only and adds no <c>.rud</c> field
    /// (see the decision recorded at the top of <c>Regions/MediaBin.xaml.cs</c>).</para>
    ///
    /// <para>⚠ Adding this property obliged <c>MediaBinRender.SameTile</c> to compare it.
    /// The IN-01 render guard skips a level whose tiles compare equal BY VALUE, so a
    /// field outside that comparison would set correctly in memory and never repaint.
    /// <c>MediaBinRenderGuardTests.every_rendered_tile_field_is_compared</c> reddened on
    /// exactly that before it was added.</para>
    /// </summary>
    internal bool IsOffline { get; init; }

    /// <summary>
    /// This clip is being got ready to play smoothly RIGHT NOW — plan 63-04, TRUST-03.
    ///
    /// <para>One of <c>MediaBinProxy.Queued</c>, <c>MediaBinProxy.Running</c>, or
    /// <c>""</c> for every other answer the engine can give. It is the NARROWED state,
    /// not the raw one: <c>ready</c>, <c>failed</c>, <c>cancelled</c>, <c>not_needed</c>
    /// and <c>none</c> all draw nothing, so storing the raw string would make the render
    /// guard repaint the level for a transition that changes no pixels.</para>
    ///
    /// <para>Like <see cref="IsOffline"/> this is an OVERLAY and a LIVE ANSWER: it comes
    /// from a poll of <c>rudis_get_proxy_status</c> (the export that had no caller at all
    /// until this plan), nothing about it is persisted, and it changes WHAT THE TILE
    /// SHOWS without changing what the tile SAYS. Folder tiles never carry it — a folder
    /// is not a file and has no proxy.</para>
    ///
    /// <para>⚠ Adding this property obliged <c>MediaBinRender.SameTile</c> to compare it,
    /// for the reason spelled out on <see cref="IsOffline"/> above: the IN-01 guard skips
    /// a level whose tiles compare equal by value, so a field outside that comparison
    /// would move in memory and never repaint. The reflective gate in
    /// <c>MediaBinRenderGuardTests</c> is what makes that impossible to forget.</para>
    /// </summary>
    internal string ProxyState { get; init; } = "";

    /// <summary>
    /// HOW FAR the running preparation has got, as a WHOLE percent 0..99 — or <c>-1</c>
    /// for "no number" — plan 71-03, TRUST-03.
    ///
    /// <para>From <c>rudis_get_proxy_status</c>'s <c>progress_permille</c> (71-02), which
    /// the engine attaches ONLY while <c>running</c>; projected by
    /// <c>MediaBinProxy.ProgressPercentFrom</c> and held non-decreasing by
    /// <c>MediaBinProxy.MonotonicPercent</c>. A missing number is <c>-1</c>, never 0: a
    /// bar at 0% for a job that has not reported is a fake claim (T-71-13). 99 is the
    /// ceiling; completion is the bar going away when the state leaves in-flight.</para>
    ///
    /// <para>Like <see cref="ProxyState"/> this is an OVERLAY and a LIVE ANSWER, never
    /// persisted, and folder tiles always carry <c>-1</c>.</para>
    ///
    /// <para>⚠ Adding this property obliged <c>MediaBinRender.SameTile</c> to compare it —
    /// the reflective gate <c>every_rendered_tile_field_is_compared</c> reddened on exactly
    /// that before the compare was added (plan 71-03 SUMMARY records the ordering).</para>
    /// </summary>
    internal int ProxyProgressPercent { get; init; } = -1;
}
