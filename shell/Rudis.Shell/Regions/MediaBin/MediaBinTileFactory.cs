using System.Globalization;
using Rudis.Shell.Mirror;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE MEDIABIN'S TILE PRESENTATION — a port of v6.0's `frontend/src/main.ts`
// :318-450 (`basename`, `middleEllipsis`, `formatDuration`, `buildMediaTile`,
// `buildFolderTile`), minus the DOM.
// ============================================================================
//
// Pure statics, ordinal everywhere, no I/O and no WinUI types. The whole point is
// that every string a tile shows is decided HERE, where it can be asserted without
// a window — so the region's parity claim is a measurement, not a screenshot.
//
// ---------------------------------------------------------------------------
// EXPLICIT NON-GOAL: nothing here compares one path to another.
// ---------------------------------------------------------------------------
//
// D-09 ("never compare a MediaBin path to a caller-supplied path by string
// equality") is a real finding — the backend canonicalises stored paths and on
// Windows that yields the extended-length form `\\?\C:\...`, so an ordinary `==`
// against the string the caller passed silently mis-compares. But it applies to
// DEDUPE / "already imported?" / RELINK / SELECT-BY-PATH, and **Phase 53 builds
// none of those.** The only places this phase consumes a stored path are
// `Basename` for display (below) and, in plan 53-02, `File.Exists` on the poster —
// both of which are correct on the extended-length form as-is.
//
// Saying that plainly IS the disposition. Inventing a canonicalisation helper with
// no caller would be worse than saying so: it would look like the problem had been
// solved, and the first real caller would find an untested helper rather than a
// recorded finding.
//
// ---------------------------------------------------------------------------
// THE GLYPHS ARE PLAIN UNICODE PICTOGRAPHS, NOT ICON-FONT PUA CODEPOINTS.
// ---------------------------------------------------------------------------
//
// Phase 50 MEASURED that a Private Use Area codepoint with a pinned-but-absent font
// family renders NOTHING on Windows 10 19045 — that machine ships no Segoe Fluent
// Icons (50-05-SUMMARY's third recorded trap). `\u266A` (MUSICAL NOTE) and
// `\U0001F5C0` (FOLDER) are v6.0's own choices, are NOT in the PUA, and fall back
// through the system font stack on both Windows 10 and 11.
//
// ACCEPTED COST, recorded rather than discovered later: they may render as COLOUR
// emoji rather than monochrome glyphs, depending on the resolved font. That is the
// same trade Phase 50 already recorded and accepted for the Toolbar's pencil.

internal static class MediaBinTileFactory
{
    /// <summary>v6.0's <c>middleEllipsis</c> default (<c>main.ts:324</c>).</summary>
    internal const int MaxNameChars = 22;

    /// <summary>U+266A MUSICAL NOTE — v6.0's audio placeholder (<c>main.ts:364</c>).</summary>
    internal const string AudioGlyph = "\u266A";

    /// <summary>U+1F5C0 FOLDER — v6.0's folder-tile thumb (<c>main.ts:428</c>).</summary>
    internal const string FolderGlyph = "\U0001F5C0";

    /// <summary>U+2026 HORIZONTAL ELLIPSIS — ONE character, as v6.0 uses. Three
    /// dots would make every truncated name two characters too long.</summary>
    private const string Ellipsis = "\u2026";

    /// <summary>
    /// File name from a path, for BOTH separators — a port of v6.0's
    /// <c>path.split(/[\\/]/)</c> then <c>parts[parts.length - 1] || path</c>.
    ///
    /// <para>Deliberately NOT the BCL's file-name helper: its behaviour on
    /// <c>\\?\</c>-prefixed input and on trailing separators is not v6.0's, and it
    /// is shaped by the running platform rather than by the string. The split is
    /// ported so the answer depends only on the characters.</para>
    /// </summary>
    internal static string Basename(string path)
    {
        if (path.Length == 0)
        {
            return path;
        }

        var parts = path.Split('\\', '/');
        var last = parts[^1];

        // v6.0's `|| path`: an empty last segment (a trailing separator) falls back
        // to the whole string rather than rendering a blank tile.
        return last.Length == 0 ? path : last;
    }

    /// <summary>
    /// Middle-ellipsize: <c>"a_really_long_clip_name.mp4"</c> →
    /// <c>"a_really_l…name.mp4"</c>.
    ///
    /// <para>v6.0's arithmetic exactly: <c>head = ceil((max - 1) / 2)</c>,
    /// <c>tail = max - 1 - head</c>, and the comparison is <c>&lt;=</c> so a name
    /// exactly AT the ceiling is left alone.</para>
    /// </summary>
    internal static string MiddleEllipsis(string name, int max = MaxNameChars)
    {
        if (name.Length <= max)
        {
            return name;
        }

        var head = (int)Math.Ceiling((max - 1) / 2.0);
        var tail = max - 1 - head;

        return string.Concat(name.AsSpan(0, head), Ellipsis, name.AsSpan(name.Length - tail, tail));
    }

    /// <summary>
    /// Microseconds → <c>"M:SS"</c>, or <c>"H:MM:SS"</c> for long media.
    ///
    /// <para>The midpoint rule is STATED below, never defaulted: JS
    /// <c>Math.round</c> rounds a .5 UP, while .NET's default is BANKER'S
    /// rounding, which would turn 2.5 s into <c>"0:02"</c>. Formatting goes through
    /// <see cref="CultureInfo.InvariantCulture"/> so a comma-decimal locale can
    /// never reach a timecode.</para>
    /// </summary>
    internal static string FormatDuration(long us)
    {
        var totalSeconds = (long)Math.Round(us / 1_000_000.0, MidpointRounding.AwayFromZero);

        var s = totalSeconds % 60;
        var m = totalSeconds / 60 % 60;
        var h = totalSeconds / 3600;

        return h > 0
            ? string.Format(CultureInfo.InvariantCulture, "{0}:{1:00}:{2:00}", h, m, s)
            : string.Format(CultureInfo.InvariantCulture, "{0}:{1:00}", m, s);
    }

    /// <summary>
    /// One media tile. D-12: <c>DisplayName ?? Basename(Path)</c>.
    ///
    /// <para><paramref name="offlineIds"/> is the live set of media ids whose files are
    /// not on disk right now (plan 60.1-07, relink slice 1). It is OPTIONAL and defaults
    /// to <c>null</c> on purpose: this factory has many call sites older than the
    /// feature, and widening it additively means none of them had to change to
    /// accommodate a state they do not exercise. <c>null</c> and an empty set are the
    /// same answer — nothing is offline — which is also what a FAILED poll must degrade
    /// to (T-60.1-20).</para>
    ///
    /// <para>Membership is ORDINAL, like everything else in this directory: media ids
    /// are opaque backend strings, not paths, so there is no case-folding question to
    /// get wrong. Whatever comparer the caller's set carries is the comparer used.</para>
    /// </summary>
    internal static MediaBinTile ForItem(
        MediaBinItem item,
        IReadOnlySet<string>? offlineIds = null,
        IReadOnlyDictionary<string, string>? proxyStates = null,
        IReadOnlyDictionary<string, int>? proxyProgress = null)
    {
        var isAudio = string.Equals(item.MediaKind, "audio", StringComparison.Ordinal);
        var isVideo = string.Equals(item.MediaKind, "video", StringComparison.Ordinal);
        var isImage = string.Equals(item.MediaKind, "image", StringComparison.Ordinal);

        // The display-name override never touches the file at `path`
        // (crates/core/src/model.rs:1361) — it is a library-level label only.
        var full = item.DisplayName ?? Basename(item.Path);

        return new MediaBinTile
        {
            Kind = MediaBinTileKind.Media,
            Id = item.Id,
            AutomationId = "MediaBin.Tile." + item.Id,
            DisplayText = MiddleEllipsis(full),
            FullText = full,
            BadgeText = isImage ? "IMG" : FormatDuration(item.DurationUs),
            PosterPath = item.PosterPath,

            // D-08. A null poster is NORMAL. Audio gets v6.0's note; a VIDEO whose
            // poster generation failed (import.rs:338 logs and continues) gets an
            // EMPTY glyph so the XAML can draw a neutral fill — v6.0's single
            // else-branch would put a musical note on it, which is a v6.0 bug and is
            // deliberately not ported (see MediaBinTileTests).
            PlaceholderGlyph = isAudio ? AudioGlyph : "",

            IsAudio = isAudio,
            IsVideo = isVideo,

            // The handoff's dimmed + ⚠ "relink" state (README:126). A pure set lookup —
            // this directory is WinUI-free and does no I/O, so the STAT that produced the
            // set happened in the engine and arrived through the region.
            IsOffline = offlineIds?.Contains(item.Id) == true,

            // Plan 63-04. A pure map lookup, NARROWED here so the tile only ever holds a
            // value the render guard should react to. Optional and null-defaulting for
            // exactly the reason `offlineIds` above is: this factory has call sites older
            // than the feature, and a null map is the same answer as an empty one.
            ProxyState = proxyStates is null
                ? ""
                : MediaBinProxy.DisplayStateFor(
                    proxyStates.TryGetValue(item.Id, out var proxyState) ? proxyState : null),

            // Plan 71-03. The region's per-id DISPLAY percent (already clamped 0..99 and
            // held monotonic by `MediaBinProxy`); -1 = no number. Optional and
            // null-defaulting like the two maps above.
            ProxyProgressPercent = proxyProgress is not null
                && proxyProgress.TryGetValue(item.Id, out var percent)
                && percent >= 0
                    ? Math.Min(percent, 99)
                    : -1,
        };
    }

    /// <summary>One child-folder tile (v6.0's <c>buildFolderTile</c>).</summary>
    internal static MediaBinTile ForFolder(LevelFolder folder)
    {
        return new MediaBinTile
        {
            Kind = MediaBinTileKind.Folder,
            Id = folder.Path,
            AutomationId = "MediaBin.Folder." + folder.Path,
            DisplayText = MiddleEllipsis(folder.Name),

            // v6.0's `tile.title = /${folder.path}` — the full canonical path, which
            // is the more useful of that tile's two tooltips.
            FullText = "/" + folder.Path,

            BadgeText = folder.ItemCount.ToString(CultureInfo.InvariantCulture),
            PosterPath = null,
            PlaceholderGlyph = FolderGlyph,
            IsAudio = false,
            IsVideo = false,
        };
    }

    /// <summary>
    /// Every tile for one level, in v6.0's order: child folders FIRST, then this
    /// level's own items (<c>renderMediaBin</c>, <c>main.ts:521-526</c>).
    ///
    /// <para>Always a FRESH list of FRESH tiles — see the note on
    /// <see cref="MediaBinTile"/> about container identity.</para>
    ///
    /// <para><paramref name="offlineIds"/> is forwarded to <see cref="ForItem"/> and is
    /// optional for the same reason it is optional there. FOLDER tiles never consult it:
    /// a folder is not a file, and a folder tile's <c>Id</c> is its canonical PATH, so a
    /// folder that happened to be named after a media id must not inherit that item's
    /// offline state (<c>MediaBinTileTests.folder_tiles_are_never_offline</c>).</para>
    /// </summary>
    internal static List<MediaBinTile> ForLevel(
        MediaBinLevelResult level,
        IReadOnlySet<string>? offlineIds = null,
        IReadOnlyDictionary<string, string>? proxyStates = null,
        IReadOnlyDictionary<string, int>? proxyProgress = null)
    {
        var tiles = new List<MediaBinTile>(level.Folders.Count + level.Items.Count);

        foreach (var folder in level.Folders)
        {
            tiles.Add(ForFolder(folder));
        }

        foreach (var item in level.Items)
        {
            tiles.Add(ForItem(item, offlineIds, proxyStates, proxyProgress));
        }

        return tiles;
    }
}
