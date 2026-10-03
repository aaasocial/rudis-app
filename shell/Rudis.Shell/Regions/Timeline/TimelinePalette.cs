namespace Rudis.Shell.Regions;

/// <summary>
/// Token NAMES in, one <see cref="RudisTimelinePalette"/> out.
///
/// <para><b>This file contains no colour values at all, and that is the whole
/// point.</b> CLAUDE.md convention 7 says named design tokens, never raw hex — and a
/// GPU boundary is exactly where that rule would otherwise quietly die, because a
/// shader cannot read a XAML resource dictionary. So the values live in
/// <c>Theme/Tokens.xaml</c> (the one file in <c>shell/</c> where a hex literal is
/// legal), this file names them as string constants, and the WinUI layer supplies a
/// resolver that looks each name up and packs it. The renderer receives numbers it
/// can neither invent nor override.</para>
///
/// <para><b>A missing key is FATAL, immediately, with its name.</b> The resolver the
/// region installs throws rather than substituting a default: a Timeline that renders
/// silently black — or worse, one that renders in a colour nobody chose — is a far
/// more expensive failure than a startup exception naming the key (T-52-29).</para>
///
/// <para>No WinUI types, by rule (D-13): the resolver is a plain
/// <c>Func&lt;string, uint&gt;</c>, so the palette build is unit-testable with no
/// window and no resource dictionary at all.</para>
/// </summary>
internal static class TimelinePalette
{
    // ========================================================================
    // THE TOKEN NAMES — design_handoff_rudis_editor/README.md § Design tokens
    // ========================================================================

    /// <summary>Toolbar / Timeline header band (README:192). Used here for the ruler
    /// band and the sticky gutter.</summary>
    public const string BgBar = "bg-bar";

    /// <summary>Body base (README:189) — the video lane band and the renderer's clear
    /// colour.</summary>
    public const string BgApp = "bg-app";

    /// <summary>Elevated panel (README:190) — the alternating (audio) lane band, so an
    /// audio lane is separable from the video lane above it without a heavy rule.</summary>
    public const string BgPanel = "bg-panel";

    /// <summary>"Timeline track separators" — the handoff's own stated use (README:197).</summary>
    public const string BorderSubtle = "border-subtle";

    /// <summary>Default section borders (README:195) — minor ruler graduations.</summary>
    public const string BorderHairline = "border-hairline";

    /// <summary>Primary text / timecode (README:198).</summary>
    public const string TextPrimary = "text-primary";

    /// <summary>Labels and icons (README:199) — the gutter's lane labels.</summary>
    public const string TextSecondary = "text-secondary";

    /// <summary>"Region tags, neutral clip" (README:201) — ruler tick labels.</summary>
    public const string TextFaint = "text-faint";

    /// <summary>Brand violet (README:202) — the playhead and the selection outline.</summary>
    public const string Accent = "accent";

    /// <summary>Gradients and hovers (README:203) — trim handles and snap guides.</summary>
    public const string AccentBright = "accent-bright";

    /// <summary>The handoff's mint audio-clip fill (README:213).</summary>
    public const string ClipAudioFill = "clip-audio-fill";

    /// <summary>OWNER OVERRIDE (phase 53.1 UAT): the uniform deep-grey video-clip
    /// fill that replaces the cycled posters on video lanes. Not part of the FFI
    /// palette struct (frozen ABI) — resolved separately and handed to the frame
    /// builder via <c>SetVideoClipStyle</c>.</summary>
    public const string ClipVideoFill = "clip-video-fill";

    /// <summary>The label/border ink paired with <see cref="ClipVideoFill"/> —
    /// near-black by owner request ("clearer / dark black", second cut; the first
    /// cut used <c>text-secondary</c>). Same delivery route as the fill.</summary>
    public const string ClipVideoLabelInk = "clip-video-label-ink";

    /// <summary>The translucent waveform line beside the mint fill (README:213).
    /// Consumed by the renderer's waveform module, which plan 52-08 fills.</summary>
    public const string ClipWaveformLine = "clip-waveform-line";

    /// <summary>53.2 D-08: the filmstrip body backdrop — what a clip's body becomes
    /// while it is showing decoded tiles, so <c>crates/filmstrip</c>'s alpha-0
    /// letterbox padding shows through as the "token-colored gaps" D-08 specifies.
    /// Not part of the FFI palette struct (its ABI is frozen at 20 <c>uint</c>s and
    /// both layout canaries are pinned to it) — resolved separately and handed to
    /// the frame builder via <c>SetFilmstripBackdrop</c>, exactly as
    /// <see cref="ClipVideoFill"/> is. Still a NAME (convention 7).</summary>
    public const string ClipFilmstripBackdrop = "clip-filmstrip-backdrop";

    /// <summary>How many entries the handoff's clip poster palette has (README:213).
    /// Assigned per media item and cycled.</summary>
    public const int PosterCount = 8;

    /// <summary>The poster token for a cycle index, e.g. index 0 -&gt;
    /// <c>clip-poster-1</c>. One-based in the token name because the handoff lists
    /// them as a sequence a designer counts, not as an array a program indexes.</summary>
    public static string PosterToken(int index) => PosterTokens[index & (PosterCount - 1)];

    private static readonly string[] PosterTokens =
    [
        "clip-poster-1", "clip-poster-2", "clip-poster-3", "clip-poster-4",
        "clip-poster-5", "clip-poster-6", "clip-poster-7", "clip-poster-8",
    ];

    // ========================================================================
    // Building
    // ========================================================================

    /// <summary>
    /// Resolve every token this region draws with, in one pass, and pack them into the
    /// struct that crosses the ABI.
    ///
    /// <para>Called ONCE per attach (and again only on a theme change), never per
    /// frame: the palette is the one piece of frame state that is genuinely constant
    /// across thousands of redraws, which is why it has an export of its own.</para>
    /// </summary>
    /// <param name="resolve">Token name to <c>0xAARRGGBB</c>. Expected to THROW, with
    /// the key in the message, for a name it cannot resolve.</param>
    public static RudisTimelinePalette BuildPalette(Func<string, uint> resolve)
    {
        ArgumentNullException.ThrowIfNull(resolve);

        var palette = new RudisTimelinePalette
        {
            BgBar = resolve(BgBar),
            BgApp = resolve(BgApp),
            BgPanel = resolve(BgPanel),
            BorderSubtle = resolve(BorderSubtle),
            BorderHairline = resolve(BorderHairline),
            TextPrimary = resolve(TextPrimary),
            TextSecondary = resolve(TextSecondary),
            TextFaint = resolve(TextFaint),
            Accent = resolve(Accent),
            AccentBright = resolve(AccentBright),
            ClipAudioFill = resolve(ClipAudioFill),
            ClipWaveformLine = resolve(ClipWaveformLine),
        };

        unsafe
        {
            for (var i = 0; i < PosterCount; i++)
            {
                palette.ClipPoster[i] = resolve(PosterTokens[i]);
            }
        }

        return palette;
    }

    // ========================================================================
    // Derived colour
    // ========================================================================

    /// <summary>
    /// The clip border: "a ~15% darker shade of the fill" (README:213).
    ///
    /// <para>COMPUTED from the already-resolved fill rather than read from a second
    /// palette, deliberately. A parallel table of border colours would be a second
    /// source of truth for a value the handoff defines as a FUNCTION of the first, and
    /// the two would drift the moment a poster colour changed.</para>
    ///
    /// <para>Alpha is preserved; each of R, G and B is multiplied by 0.85. The
    /// handoff's own worked example is a little darker than that (it works out closer
    /// to 0.76x), and the difference is recorded in this plan's artifact rather than
    /// silently split: 0.85 is what "15% darker" says, and the example is the one
    /// place the handoff paraphrases itself.</para>
    /// </summary>
    public static uint Darken15(uint argb)
    {
        var a = argb & 0xFF000000u;
        var r = (uint)(((argb >> 16) & 0xFFu) * 85 / 100);
        var g = (uint)(((argb >> 8) & 0xFFu) * 85 / 100);
        var b = (uint)((argb & 0xFFu) * 85 / 100);
        return a | (r << 16) | (g << 8) | b;
    }

    /// <summary>
    /// Which of the eight poster colours a media item gets (README:213 — "assign per
    /// media item, cycle").
    ///
    /// <para>Derived from the media id by a hand-rolled FNV-1a, for the reason 52-02
    /// recorded for its cache filenames: the framework's default string hash is
    /// randomised per process, so the same id would land on a different colour every
    /// time the app restarted. A stable function of the id makes the colour a property
    /// of the MEDIA ITEM — the same across rebuilds, across an undo, across a scroll,
    /// and across re-opening a project — which is what lets a user read it as identity
    /// rather than as decoration.</para>
    ///
    /// <para><b>The honest limit, observed rather than assumed:</b> a fresh IMPORT of
    /// the same file mints a NEW media id, so re-importing a clip can give it a
    /// different colour. That is a property of id assignment, not of this function, and
    /// it was visible across two launches while capturing this plan's screenshots.</para>
    ///
    /// <para>Allocation-free: it walks the string's chars and never materialises
    /// anything.</para>
    /// </summary>
    public static int PosterIndexFor(string? mediaId)
    {
        if (string.IsNullOrEmpty(mediaId))
        {
            // The neutral end of the handoff's own palette (README:201 calls its last
            // entry "neutral clip"), so a clip whose media item has not arrived yet
            // reads as unresolved rather than as an arbitrary colour.
            return PosterCount - 1;
        }

        var hash = 2166136261u;
        for (var i = 0; i < mediaId.Length; i++)
        {
            hash = (hash ^ mediaId[i]) * 16777619u;
        }

        return (int)(hash & (PosterCount - 1));
    }
}
