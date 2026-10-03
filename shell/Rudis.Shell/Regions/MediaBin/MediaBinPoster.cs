namespace Rudis.Shell.Regions;

/// <summary>
/// The MediaBin poster POLICY: may this path be opened at all, and how many pixels
/// wide should the decode be.
///
/// <para>Both questions are decidable with no window, so both are decided here and
/// asserted by <c>MediaBinPosterTests</c> in a plain test host. What is left in
/// <c>MediaBin.xaml.cs</c> is only the part that genuinely needs a visual tree —
/// the phased-loading hook, the template parts and the staleness token.</para>
///
/// <para><b>⚠ D-06 — there is NO allowlist here, and there must not be one.</b>
/// v6.0 could load these PNGs only because <c>tauri.conf.json</c>'s asset-protocol
/// scope allowlists <c>$APPCACHE/posters/**</c>;
/// <c>poster_cache_dir</c>'s own doc comment (<c>crates/app-core/src/import.rs:363-370</c>)
/// records that constraint and why every poster must be written through that one
/// directory. It is a WebView SANDBOX rule. <b>WinUI 3 has no such sandbox</b> — the
/// shell opens the stored path directly — so the allowlist requirement evaporates and
/// must not be cargo-culted in as "the thing v6 did".
/// <see cref="ShouldAttemptLoad"/> is an EXISTENCE check, not a permission check.</para>
///
/// <para>WinUI-free by rule — see the contract comment in
/// <c>Rudis.Shell.Tests.csproj</c>, enforced by <c>MediaBinPurityGateTests</c>.</para>
/// </summary>
internal static class MediaBinPoster
{
    /// <summary>
    /// The tile thumb's width in DIPs. MUST match the <c>Width</c> of the tile
    /// <c>StackPanel</c> in <c>MediaBin.xaml</c>'s <c>ItemTemplate</c> — decoding to
    /// anything else is either a soft image or wasted memory, which is the whole
    /// point of D-07.
    /// </summary>
    internal const double ThumbWidthDip = 140;

    /// <summary>
    /// D-08 + V5 / T-53-12. A null, empty or whitespace path is <b>NORMAL</b>: audio
    /// items never get a poster (<c>import.rs:535</c> returns <c>None</c> for
    /// <c>MediaKind::Audio</c>) and a video's poster generation is logged-and-continued
    /// rather than fatal (<c>import.rs:549</c>). A path that no longer resolves is a
    /// relink / cleared-cache case, not a fault. Both answer <c>false</c> and the tile
    /// keeps its placeholder glyph.
    ///
    /// <para><c>File.Exists</c> already swallows most malformed-path exceptions, but it
    /// is not documented to swallow all of them and the caller here has no handler of its
    /// own worth relying on — so the guard is explicit. A stored path this method cannot
    /// even parse is, definitionally, a path with no poster behind it.</para>
    ///
    /// <para><b>D-09:</b> this is an EXISTENCE check and never a comparison. The backend
    /// canonicalises stored paths and on Windows that yields the extended-length
    /// <c>\\?\C:\...</c> form; comparing such a path to a caller-supplied one by ordinary
    /// equality is the trap Phase 50 recorded for this phase. Nothing here compares
    /// anything.</para>
    /// </summary>
    internal static bool ShouldAttemptLoad(string? posterPath)
    {
        if (string.IsNullOrWhiteSpace(posterPath))
        {
            return false;
        }

        try
        {
            return File.Exists(posterPath);
        }
        catch (ArgumentException)
        {
            return false;
        }
        catch (PathTooLongException)
        {
            return false;
        }
        catch (IOException)
        {
            return false;
        }
        catch (UnauthorizedAccessException)
        {
            return false;
        }
    }

    /// <summary>
    /// D-07 / T-53-11: the decoded footprint, bounded to what the tile actually
    /// renders, in PHYSICAL pixels.
    ///
    /// <para><b><c>BitmapImage.DecodePixelWidth = 0</c> means FULL RESOLUTION</b>, not
    /// "no scaling" — so every degenerate input has to be clamped rather than passed
    /// through. <c>RasterizationScale</c> is 0 before a control is in a visual tree,
    /// which makes the zero case a real one and not a defensive nicety.</para>
    ///
    /// <para>The scale factor is the display's, not a guess: a 140-DIP thumb is 210
    /// real pixels at 150%, and decoding 140 there would be visibly soft.</para>
    /// </summary>
    internal static int DecodePixelWidth(double thumbWidthDip, double rasterizationScale)
    {
        var width = double.IsFinite(thumbWidthDip) && thumbWidthDip > 0 ? thumbWidthDip : ThumbWidthDip;
        var scale = double.IsFinite(rasterizationScale) && rasterizationScale > 0 ? rasterizationScale : 1.0;

        var px = Math.Round(width * scale, MidpointRounding.AwayFromZero);

        // Belt and braces against an absurd scale producing something outside int:
        // a clamped-but-large decode is a slow tile, an overflowed one is a crash.
        if (!double.IsFinite(px) || px < 1)
        {
            return 1;
        }

        return px > int.MaxValue ? int.MaxValue : (int)px;
    }
}
