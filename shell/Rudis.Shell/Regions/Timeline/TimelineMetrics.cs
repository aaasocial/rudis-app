namespace Rudis.Shell.Regions;

/// <summary>
/// Every fixed number the Timeline's geometry needs, in LOGICAL pixels, in one
/// place, each with its source named.
///
/// <para><b>Two tables, deliberately distinguishable.</b> The first is transcribed
/// from <c>design_handoff_rudis_editor/README.md</c> and is NOT ours to change —
/// CLAUDE.md convention 7 makes that file the canonical UI spec. The second is
/// this plan's own discretion, recorded in
/// <c>.planning/phases/52-timeline-region/artifacts/52-03-hotpath.md</c> with the
/// reason for each value, so plans 52-06 and 52-07 consume these numbers instead
/// of re-deriving them under UI pressure.</para>
///
/// <para><b>No behaviour and no colours live here.</b> Colours are 52-06's and
/// they come from <c>Theme/Tokens.xaml</c> by NAME — CLAUDE.md convention 7
/// again, and <c>MechanicalGatesTests.no_raw_hex_outside_the_token_dictionary</c>
/// enforces it. A "handy" colour constant in this file would be a second source
/// of truth for a value the token dictionary already owns.</para>
/// </summary>
internal static class TimelineMetrics
{
    // ========================================================================
    // THE DESIGN HANDOFF'S NUMBERS — transcribed, not chosen
    // ========================================================================

    /// <summary>36 — README.md:239 "Timeline header 36" (the `Timeline › Toolbar`
    /// band: region tag, timecode, ✂ ⧉ ⌫, the zoom control).</summary>
    public const double TimelineHeaderHeight = 36;

    /// <summary>20 — README.md:239 "Ruler 20" / :133 "height ~20px".</summary>
    public const double RulerHeight = 20;

    /// <summary>48 — README.md:239 "Track lanes V1 48" / :135 "V1 (video, 48px)".</summary>
    public const double VideoLaneHeight = 48;

    /// <summary>42 — README.md:239 "A1 42" / :135 "A1 (audio, 42px)".</summary>
    public const double AudioLaneHeight = 42;

    /// <summary>46 — README.md:239 "TrackHeader gutter 46" / :134 "fixed 46px left
    /// gutter". This band is NOT part of the time axis; see
    /// <c>TimelineViewport.LaneAreaXFromSurfaceX</c>.</summary>
    public const double TrackHeaderGutterWidth = 46;

    /// <summary>140 — README.md:173 "`Timeline` has a min height (~140px) so at
    /// least ruler + 1–2 lanes show".</summary>
    public const double MinTimelineHeight = 140;

    /// <summary>2 — README.md:136 "2px `accent` vertical line + diamond cap".</summary>
    public const double PlayheadLineWidth = 2;

    /// <summary>2 — README.md:160 (§ Focus &amp; selection) "2px `accent`
    /// outline".</summary>
    public const double SelectionOutlineWidth = 2;

    // ========================================================================
    // CLAUDE'S DISCRETION, 52-03 — recorded in artifacts/52-03-hotpath.md
    // ========================================================================

    /// <summary>6 per edge (`Clip.TrimHandle`, README.md:137). A clip narrower than
    /// <c>3 * TrimHandleWidth</c> shrinks its handles to <c>clipWidth / 3</c> so a
    /// tiny clip keeps a body zone and stays draggable — see
    /// <c>TimelineHitTester.HandleWidthFor</c>.</summary>
    public const double TrimHandleWidth = 6;

    /// <summary>8 — drag/trim snapping to the playhead and to adjacent clip edges
    /// (README.md:140 "snapping to playhead/clip edges"). Consumed by 52-07's drag
    /// state machine; declared here so both plans use one number.</summary>
    public const double SnapThresholdPx = 8;

    /// <summary>40 — 5 seconds ≈ 200px, which is the ruler density the handoff's own
    /// `0:00 0:05 0:10` graduation example implies.</summary>
    public const double DefaultPxPerSecond = 40;

    /// <summary>1 — whole-project overview: a one-hour timeline is 3,600px wide.</summary>
    public const double MinPxPerSecond = 1;

    /// <summary>600 — sub-frame precision: one 30fps frame (33,333µs) is ~20px
    /// wide, so a frame boundary is individually grabbable.</summary>
    public const double MaxPxPerSecond = 600;

    /// <summary>1.25 — one `−`/`+` click (README.md:132's zoom control) or one
    /// Ctrl+scroll notch multiplies/divides px-per-second by this.</summary>
    public const double ZoomStep = 1.25;

    /// <summary>60 — the ruler's graduation selector picks the smallest ladder entry
    /// whose labels sit at least this far apart, so a `00:00:12.000` label never
    /// collides with its neighbour.</summary>
    public const double MinTickLabelSpacingPx = 60;

    /// <summary>
    /// The ruler's graduation ladder, in seconds: the smallest entry whose label
    /// spacing clears <see cref="MinTickLabelSpacingPx"/> wins.
    ///
    /// <para>Exposed as a span over a private array so it cannot be mutated by a
    /// caller AND so reading it allocates nothing — <c>RulerTicks</c> walks it on
    /// every redraw.</para>
    /// </summary>
    public static ReadOnlySpan<long> RulerTickLadderSeconds => RulerTickLadderSecondsData;

    private static readonly long[] RulerTickLadderSecondsData =
        [1, 2, 5, 10, 15, 30, 60, 120, 300, 600];

    /// <summary>The lane height for a <c>TrackKind</c>, or 0 for a kind this build
    /// does not know (D-06 — there are exactly two).</summary>
    public static double LaneHeightForKind(string? kind) => kind switch
    {
        LaneModel.VideoKind => VideoLaneHeight,
        LaneModel.AudioKind => AudioLaneHeight,
        _ => 0,
    };

    // ========================================================================
    // CLAUDE'S DISCRETION, 53.2-01 — recorded in
    // artifacts/53.2-01-anatomy-records.md
    // ========================================================================

    /// <summary>14 — 53.2 D-01: the clip title band. NOT in
    /// <c>design_handoff_rudis_editor/README.md</c> (which specifies a label on a
    /// colored block, not a header strip) — a RECORDED deviation, see
    /// <c>.planning/phases/53.2-*/artifacts/53.2-01-anatomy-records.md</c>. Leaves
    /// 34px of frame body on a video lane (48-14) and 28px on an audio lane
    /// (42-14).</summary>
    public const double ClipTitleBandHeight = 14;

    /// <summary>24 — 53.2 D-04: below this clip width (logical px) the frame body
    /// degrades to a solid token fill and no thumbnails draw. 24 = 4 *
    /// <see cref="TrimHandleWidth"/>: below it a tile would be a sliver narrower
    /// than the two trim handles beside it. The band always draws regardless
    /// (D-04).</summary>
    public const double FilmstripWidthFloorPx = 24;

    /// <summary>60 — 53.2 D-05: the logical draw width of one filmstrip tile on a
    /// video lane: the 34px body height at a 16:9 cell (34 * 16 / 9 ≈ 60.4,
    /// floored). Tile COUNT per clip is <c>floor(clipWidth / this)</c>, capped
    /// below.</summary>
    public const double FilmstripTileDrawWidthPx = 60;

    /// <summary>64 — 53.2 D-05's mandatory per-clip tile cap: at
    /// <see cref="MaxPxPerSecond"/> = 600 a 10-minute clip is 360,000px wide and an
    /// uncapped <c>floor(w / 60)</c> would request 6,000 tiles in one draw. When the
    /// cap binds, tiles widen to <c>clipWidth / 64</c> rather than
    /// multiplying.</summary>
    public const int MaxFilmstripTilesPerClip = 64;

    /// <summary>The frame-body height below the D-01 band for a lane kind, or 0 for
    /// an unknown kind (mirrors <see cref="LaneHeightForKind"/>'s D-06
    /// posture).</summary>
    public static double FilmstripBodyHeight(string? kind) => kind switch
    {
        LaneModel.VideoKind => VideoLaneHeight - ClipTitleBandHeight,
        LaneModel.AudioKind => AudioLaneHeight - ClipTitleBandHeight,
        _ => 0,
    };
}
