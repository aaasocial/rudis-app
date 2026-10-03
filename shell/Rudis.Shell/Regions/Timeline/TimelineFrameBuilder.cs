using System.Runtime.InteropServices;
using System.Text;

namespace Rudis.Shell.Regions;

/// <summary>
/// Mirror + viewport in, one flat <see cref="RudisTimelineFrame"/> out.
///
/// <para><b>This is where SHELL-05 stops being an architecture diagram.</b> A clip
/// becomes a rectangle in a contiguous array here, and stays one all the way to the
/// GPU: no per-clip object, no per-clip allocation, no per-clip call. The only thing
/// that grows with the project is the CULL, and the cull is O(visible) (52-03).</para>
///
/// <para><b>Every buffer is owned, reused and PINNED.</b> The five arrays and the two
/// UTF-8 arenas are allocated once, grown by doubling, and pinned with a
/// <see cref="GCHandle"/> held in a field for the life of the builder — not pinned
/// per call. That is deliberate on both counts:</para>
/// <list type="bullet">
/// <item>pinning per call would put a handle-table operation on the redraw path for no
///   benefit, since the arrays never move anyway;</item>
/// <item>a long-lived pin is a fragmentation cost, and it is bounded and small: five
///   arrays and two byte buffers for the whole region, sized to the VISIBLE clip count
///   rather than to the project.</item>
/// </list>
///
/// <para><b>Pointers are written LAST, from offsets.</b> During the fill every
/// <c>LabelPtr</c> holds a byte OFFSET into its arena; a final pass turns offsets into
/// absolute addresses. That is what makes it safe for an arena to GROW mid-fill: a
/// grown arena is a different object at a different address, and a pointer captured
/// before the growth would be dangling. Offsets survive; addresses do not.</para>
///
/// <para><b>The frame is a BORROW for exactly one call.</b> The renderer never stores
/// a pointer past the <c>rudis_timeline_render</c> that carried it, so this builder is
/// free to overwrite its buffers on the next frame — but it must not be disposed while
/// a render is in flight, which is why the region disposes it on the same UI thread
/// that calls render, after detaching.</para>
///
/// <para>No WinUI types, by rule (D-13): the whole builder is unit-tested in a plain
/// <c>net9.0-windows</c> host with no window and no GPU device.</para>
/// </summary>
internal sealed class TimelineFrameBuilder : IDisposable
{
    /// <summary>Hard cap on lanes sent in one frame — the renderer's own bound. A
    /// vertically-culled Timeline never sends more than a screenful; this exists so a
    /// corrupted payload costs a bounded loop rather than a rejected frame.</summary>
    private const int MaxLanes = 256;

    /// <summary>Hard cap on ruler graduations, matching the renderer's bound.</summary>
    private const int MaxTicks = 1024;

    /// <summary>How many distinct label strings the arena remembers before it is
    /// dropped wholesale. Bounded so a long session that scrolls past thousands of
    /// media items cannot grow it without limit (T-52-18's shape, C# side).</summary>
    private const int MaxLabelEntries = 2048;

    /// <summary>Ceiling on the label arena, in bytes. A clip label is a filename;
    /// 2,048 of them at a few hundred bytes each is the worst realistic case, and
    /// past this the arena resets rather than growing forever.</summary>
    private const int MaxLabelArenaBytes = 512 * 1024;

    private RudisTimelineClip[] _clips = new RudisTimelineClip[64];
    private RudisTimelineLane[] _lanes = new RudisTimelineLane[8];
    private RudisTimelineTick[] _ticks = new RudisTimelineTick[64];
    private byte[] _labelArena = new byte[4096];
    private byte[] _tickArena = new byte[MaxTicks * TimelineTimecode.RulerLabelMaxBytes];

    // ── the drag/trim overlay (plan 52-07). Fixed size, never grown: a gesture
    //    previews ONE clip, and the renderer bounds the guides at 64. ──
    private readonly RudisTimelineClip[] _ghosts = new RudisTimelineClip[1];
    private readonly float[] _snapGuidesPhysical = new float[TimelineInteraction.MaxSnapGuides];
    private readonly float[] _snapGuidesLogical = new float[TimelineInteraction.MaxSnapGuides];

    private GCHandle _clipsPin;
    private GCHandle _lanesPin;
    private GCHandle _ticksPin;
    private GCHandle _labelPin;
    private GCHandle _tickArenaPin;
    private GCHandle _ghostsPin;
    private GCHandle _snapGuidesPin;

    private bool _hasGhost;
    private GhostRect _ghostLogical;
    private int _snapGuideCount;

    private readonly List<ClipLayout> _cull = new(256);
    private readonly List<long> _tickTimes = new(64);

    /// <summary>Label text to its slice of <see cref="_labelArena"/>. Keyed by the
    /// STRING the model already holds, so a hit is a dictionary lookup and an unchanged
    /// label is never re-encoded — the difference between zero allocations per frame
    /// and one per visible clip per frame.</summary>
    private readonly Dictionary<string, LabelSlice> _labelIndex = new(StringComparer.Ordinal);

    private int _labelUsed;
    private int _tickUsed;

    private readonly uint[] _poster = new uint[TimelinePalette.PosterCount];
    private uint _audioFill;

    // ── OWNER OVERRIDE (phase 53.1 UAT): uniform deep-grey video-clip fill, with a
    //    LIGHT label/border ink — the renderer inks a clip's label with its Border
    //    colour (text.rs, `colour(clip.border)`), and the handoff's Darken15 rule
    //    assumes a pastel fill; 15%-darker deep grey would be unreadable on itself.
    //    0 = not installed → the poster cycle and Darken15 apply, so every test and
    //    caller that predates the override keeps its measured behaviour. ──
    private uint _videoFill;
    private uint _videoLabelInk;

    // ── 53.2 D-08: the filmstrip body backdrop, installed by SetFilmstripBackdrop.
    //    0 = not installed → a strip-bearing clip keeps its own fill. ──
    private uint _filmstripBackdrop;

    // ── change detection. The same discipline TimecodeFormatter uses: the cache
    //    lives IN the unit that would otherwise do the work, so the caller early-
    //    returns instead of asking "has anything changed?" at every call site. ──
    private bool _hasFrame;
    private bool _forceDirty = true;
    private int _lastRevision = -1;
    private double _lastPxPerSecond;
    private double _lastScrollX;
    private double _lastScrollY;
    private double _lastWidth;
    private double _lastHeight;
    private double _lastScale;
    private long _lastPlayheadPx;
    private long _lastPeakGeneration = -1;
    private long _lastFilmstripGeneration = -1;
    private string? _lastSelected;
    private RudisTimelineFrame _lastFrame;

    /// <summary>SHELL-09's client-side peak cache (plan 52-08). The builder READS it
    /// and nothing else — <see cref="PeakCache.TryGetPinnedPeaks"/> is a pure lookup
    /// that cannot start work, which is what keeps the paint path free of the ABI
    /// (52-RESEARCH Pitfall 7). Owned by the region; a default one is constructed
    /// when a caller supplies none, so a headless test never has to.</summary>
    private readonly PeakCache _peaks;

    /// <summary>53.2 D-12's C#-policy half (plan 53.2-06), read on exactly the same
    /// terms as <see cref="_peaks"/>: <c>TryGetPinnedStrip</c> is a pure lookup that
    /// cannot start work, and <c>NoteWanted</c> records intent without touching the
    /// ABI. Owned by the region; a default one is constructed when a caller supplies
    /// none, so a headless test never has to.</summary>
    private readonly FilmstripCache _filmstrips;

    public TimelineFrameBuilder(PeakCache? peaks = null, FilmstripCache? filmstrips = null)
    {
        _peaks = peaks ?? new PeakCache();
        _filmstrips = filmstrips ?? new FilmstripCache();
        _clipsPin = GCHandle.Alloc(_clips, GCHandleType.Pinned);
        _lanesPin = GCHandle.Alloc(_lanes, GCHandleType.Pinned);
        _ticksPin = GCHandle.Alloc(_ticks, GCHandleType.Pinned);
        _labelPin = GCHandle.Alloc(_labelArena, GCHandleType.Pinned);
        _tickArenaPin = GCHandle.Alloc(_tickArena, GCHandleType.Pinned);
        _ghostsPin = GCHandle.Alloc(_ghosts, GCHandleType.Pinned);
        _snapGuidesPin = GCHandle.Alloc(_snapGuidesPhysical, GCHandleType.Pinned);
    }

    /// <summary>
    /// Install the drag/trim overlay for the next frame (plan 52-07): the ghost
    /// rectangle and the snap guides, both in SURFACE-relative LOGICAL px exactly as
    /// <c>TimelineInteraction</c> produced them.
    ///
    /// <para><b>The change check is here rather than at the call site.</b> The region
    /// calls this on every tick whether or not a gesture is in flight, and an overlay
    /// that has not moved must not force a redraw — the same discipline the playhead's
    /// physical-pixel comparison already applies. Conversely a ghost that HAS moved
    /// must, and a builder whose dirty check only looked at model revision and viewport
    /// would drop every frame of a drag.</para>
    ///
    /// <para>Logical→physical conversion happens in <see cref="Build"/>, against the
    /// viewport that frame is being built for — never here, where no viewport is in
    /// hand (D-14: one conversion seam).</para>
    /// </summary>
    public void SetOverlay(bool hasGhost, in GhostRect ghost, ReadOnlySpan<float> snapGuidesLogicalX)
    {
        var count = snapGuidesLogicalX.Length < TimelineInteraction.MaxSnapGuides
            ? snapGuidesLogicalX.Length
            : TimelineInteraction.MaxSnapGuides;

        var changed = hasGhost != _hasGhost || count != _snapGuideCount;
        if (!changed && hasGhost)
        {
            changed = !ghost.Equals(_ghostLogical);
        }

        if (!changed)
        {
            for (var i = 0; i < count; i++)
            {
                if (_snapGuidesLogical[i] != snapGuidesLogicalX[i])
                {
                    changed = true;
                    break;
                }
            }
        }

        if (!changed)
        {
            return;
        }

        _hasGhost = hasGhost;
        _ghostLogical = ghost;
        _snapGuideCount = count;
        for (var i = 0; i < count; i++)
        {
            _snapGuidesLogical[i] = snapGuidesLogicalX[i];
        }

        _forceDirty = true;
    }

    /// <summary>How many times a label has actually been UTF-8 encoded into the arena.
    /// Exposed so <c>label_bytes_are_encoded_once_and_reused</c> can assert the cache
    /// works rather than assume it — a counter is the only way to tell "cached" from
    /// "re-encoded cheaply".</summary>
    public long LabelEncodeCount { get; private set; }

    /// <summary>Clips in the most recently built frame. The cull's own yield, kept for
    /// 52-09's introspection hook.</summary>
    public int LastClipCount { get; private set; }

    /// <summary>
    /// False until <see cref="SetPalette"/> has run. Worth exposing rather than
    /// asserting: the renderer draws NOTHING before its palette upload — deliberately,
    /// because it has no colour of its own — and the symptom is a dead-looking panel
    /// with no error anywhere. The region checks this and says so out loud instead.
    /// </summary>
    public bool HasPalette { get; private set; }

    /// <summary>
    /// Force the next <see cref="Build"/> to do the full work regardless of the change
    /// cache. Needed after a RE-ATTACH: the recovered surface has never been drawn on,
    /// so "nothing changed since the last frame" is true of the model and false of the
    /// pixels.
    /// </summary>
    public void MarkDirty() => _forceDirty = true;

    /// <summary>
    /// Install the palette the WinUI layer resolved from <c>Theme/Tokens.xaml</c>.
    /// Called once per attach and again only on a theme change — never per frame.
    /// Marks the next frame dirty, because every clip's fill is derived from it.
    /// </summary>
    public void SetPalette(in RudisTimelinePalette palette)
    {
        _audioFill = palette.ClipAudioFill;
        unsafe
        {
            fixed (RudisTimelinePalette* p = &palette)
            {
                for (var i = 0; i < TimelinePalette.PosterCount; i++)
                {
                    _poster[i] = p->ClipPoster[i];
                }
            }
        }

        HasPalette = true;
        _forceDirty = true;
    }

    /// <summary>
    /// Install the OWNER-OVERRIDE video-clip style (phase 53.1 UAT): one deep-grey
    /// fill for every video clip in place of the cycled posters, and the light ink
    /// the renderer will use for the clip's border AND label (they share the Border
    /// channel — text.rs inks labels with <c>clip.border</c>). Called beside
    /// <c>SetPalette</c>, from the same token resolution; never per frame. Passing
    /// 0 for <paramref name="fill"/> uninstalls the override and restores the
    /// handoff's poster cycle.
    /// </summary>
    public void SetVideoClipStyle(uint fill, uint labelInk)
    {
        _videoFill = fill;
        _videoLabelInk = labelInk;
        _forceDirty = true;
    }

    /// <summary>
    /// Install 53.2 D-08's filmstrip body backdrop — the near-black a clip's body
    /// takes while it is showing decoded tiles, so <c>crates/filmstrip</c>'s alpha-0
    /// letterbox padding reads as a token-coloured gap rather than as the clip's own
    /// pastel leaking through the picture.
    ///
    /// <para>Delivered here rather than through <see cref="SetPalette"/> for the same
    /// reason <see cref="SetVideoClipStyle"/> is: <see cref="RudisTimelinePalette"/> is
    /// a frozen 20-<c>uint</c> ABI struct with both layout canaries pinned to it, and
    /// this colour never crosses that boundary anyway — it travels per clip in
    /// <see cref="RudisTimelineClip.Fill"/>. Still a NAMED token (convention 7);
    /// <c>TimelinePalette.ClipFilmstripBackdrop</c> is the name and the region resolves
    /// it through the same resolver the palette uses.</para>
    ///
    /// <para><c>0</c> means "not installed", and then a strip-bearing clip keeps its
    /// own fill — the pre-token drawing, not a hole where a colour should be.</para>
    /// </summary>
    public void SetFilmstripBackdrop(uint backdrop)
    {
        _filmstripBackdrop = backdrop;
        _forceDirty = true;
    }

    /// <summary>The fill for a video clip: the owner-override grey when installed,
    /// else the handoff's poster cycle.</summary>
    private uint VideoFillFor(string mediaId) =>
        _videoFill != 0 ? _videoFill : _poster[TimelinePalette.PosterIndexFor(mediaId)];

    /// <summary>The border/label ink to pair with <see cref="VideoFillFor"/> — light
    /// ink over the override grey, Darken15 over a pastel poster.</summary>
    private uint VideoBorderFor(uint fill) =>
        _videoFill != 0 ? _videoLabelInk : TimelinePalette.Darken15(fill);

    /// <summary>
    /// Build one frame.
    /// </summary>
    /// <returns><see langword="true"/> when something changed — i.e. when
    /// <see cref="RudisTimelineFrame.Dirty"/> is 1. A <see langword="false"/> return
    /// still yields a VALID frame describing the previous arrays with
    /// <c>Dirty == 0</c>, and the caller is expected to hand it to the renderer
    /// anyway: the renderer's dirty gate refuses it before touching the GPU and
    /// increments <c>skipped_clean_frames</c>, which is the counter that makes "an
    /// idle Timeline costs nothing" a number instead of a promise.</returns>
    public bool Build(
        TimelineModel model,
        TimelineViewport viewport,
        long playheadUs,
        string? selectedId,
        out RudisTimelineFrame frame)
    {
        ArgumentNullException.ThrowIfNull(model);
        ArgumentNullException.ThrowIfNull(viewport);

        var gutter = TimelineMetrics.TrackHeaderGutterWidth;
        var laneAreaTop = TimelineMetrics.TimelineHeaderHeight + TimelineMetrics.RulerHeight;

        // The playhead is compared in PHYSICAL px, not microseconds: a sub-pixel move
        // redraws a pixel-identical frame, and at 40 px/s that is 24,000 clean frames
        // per second of playback the GPU never has to touch.
        var playheadLogicalX = gutter + viewport.TimeUsToPixel(playheadUs);
        var playheadPx = (long)Math.Round(viewport.LogicalToPhysical(playheadLogicalX));

        var changed =
            _forceDirty
            || !_hasFrame
            || model.Revision != _lastRevision
            || viewport.PxPerSecond != _lastPxPerSecond
            || viewport.ScrollXPx != _lastScrollX
            || viewport.ScrollYPx != _lastScrollY
            || viewport.ViewportWidthPx != _lastWidth
            || viewport.ViewportHeightPx != _lastHeight
            || viewport.RasterizationScale != _lastScale
            || playheadPx != _lastPlayheadPx
            // Peaks arriving must cost EXACTLY ONE redraw. The cache bumps its
            // generation once per media id that transitions from absent to present, so
            // this is one comparison per frame that fires once per arrival — not zero
            // (the surface would keep drawing the pre-peak frame until something else
            // moved) and not once per poll (which would redraw at 10 Hz forever).
            || _peaks.Generation != _lastPeakGeneration
            // Same rule for strips, and it is load-bearing for D-14: the cache bumps
            // its generation once per media id whose DRAWN CONTENT changed — a
            // placeholder arriving, a strip replacing it, each partial ADVANCING —
            // and not at all for a re-poll that returned the same tile count. Without
            // this comparison the surface would keep drawing the pre-strip frame
            // until something else moved; with a per-poll bump it would redraw at
            // 10 Hz forever.
            || _filmstrips.Generation != _lastFilmstripGeneration
            || !string.Equals(selectedId, _lastSelected, StringComparison.Ordinal);

        if (!changed)
        {
            _lastFrame.Dirty = 0;
            frame = _lastFrame;
            return false;
        }

        _forceDirty = false;
        _lastRevision = model.Revision;
        _lastPxPerSecond = viewport.PxPerSecond;
        _lastScrollX = viewport.ScrollXPx;
        _lastScrollY = viewport.ScrollYPx;
        _lastWidth = viewport.ViewportWidthPx;
        _lastHeight = viewport.ViewportHeightPx;
        _lastScale = viewport.RasterizationScale;
        _lastPlayheadPx = playheadPx;
        _lastPeakGeneration = _peaks.Generation;
        _lastFilmstripGeneration = _filmstrips.Generation;
        _lastSelected = selectedId;

        ResetLabelArenaIfExhausted();
        _tickUsed = 0;

        var lanes = viewport.Lanes;
        var laneCount = FillLanes(viewport, lanes, laneAreaTop);
        var clipCount = FillClips(model, viewport, lanes, selectedId, gutter, laneAreaTop);
        var tickCount = FillTicks(viewport, gutter);
        var ghostCount = FillGhost(viewport);
        var guideCount = FillSnapGuides(viewport);

        // ── POINTER FIXUP. Every LabelPtr above holds a byte OFFSET; the arenas may
        //    have moved during the fill (a grown array is a new object), so absolute
        //    addresses are only taken now, once, from the final pinned bases.
        var labelBase = _labelPin.AddrOfPinnedObject();
        var tickBase = _tickArenaPin.AddrOfPinnedObject();
        for (var i = 0; i < laneCount; i++)
        {
            _lanes[i].LabelPtr = _lanes[i].LabelLen == 0 ? nint.Zero : labelBase + _lanes[i].LabelPtr;
        }
        for (var i = 0; i < clipCount; i++)
        {
            _clips[i].LabelPtr = _clips[i].LabelLen == 0 ? nint.Zero : labelBase + _clips[i].LabelPtr;
        }
        for (var i = 0; i < tickCount; i++)
        {
            _ticks[i].LabelPtr = _ticks[i].LabelLen == 0 ? nint.Zero : tickBase + _ticks[i].LabelPtr;
        }

        var surfaceW = viewport.ViewportWidthPx + gutter;
        var surfaceH = viewport.ViewportHeightPx;

        _lastFrame = new RudisTimelineFrame
        {
            SurfaceWPx = (uint)Math.Max(1, Math.Round(viewport.LogicalToPhysical(surfaceW))),
            SurfaceHPx = (uint)Math.Max(1, Math.Round(viewport.LogicalToPhysical(surfaceH))),
            Scale = (float)viewport.RasterizationScale,
            GutterWPx = (float)viewport.LogicalToPhysical(gutter),
            HeaderHPx = (float)viewport.LogicalToPhysical(TimelineMetrics.TimelineHeaderHeight),
            RulerHPx = (float)viewport.LogicalToPhysical(TimelineMetrics.RulerHeight),
            LanesPtr = laneCount == 0 ? nint.Zero : _lanesPin.AddrOfPinnedObject(),
            LanesLen = (uint)laneCount,
            ClipsPtr = clipCount == 0 ? nint.Zero : _clipsPin.AddrOfPinnedObject(),
            ClipsLen = (uint)clipCount,
            TicksPtr = tickCount == 0 ? nint.Zero : _ticksPin.AddrOfPinnedObject(),
            TicksLen = (uint)tickCount,
            PlayheadXPx = (float)playheadLogicalX * (float)viewport.RasterizationScale,

            // The drag/trim overlay (plan 52-07). BOTH halves move together: a stale
            // non-null pointer beside a zero length is exactly the shape the renderer's
            // own bound checker calls out as dangerous, so a count of zero carries a
            // null pointer rather than a pinned-but-unused base.
            GhostPtr = ghostCount == 0 ? nint.Zero : _ghostsPin.AddrOfPinnedObject(),
            GhostLen = (uint)ghostCount,
            SnapGuidesPtr = guideCount == 0 ? nint.Zero : _snapGuidesPin.AddrOfPinnedObject(),
            SnapGuidesLen = (uint)guideCount,

            // 52-08 fills the peaks.
            Dirty = 1,

            // 53.2 D-01's band, converted at THIS seam and nowhere else (the Phase 50
            // DPI discipline): the metric is 14 LOGICAL px and every `_px` field that
            // crosses this boundary is PHYSICAL. The renderer clamps it per clip to
            // that clip's own height, so a lane shorter than the band is all band
            // rather than a body with negative height (D-04).
            BandHPx = (float)(TimelineMetrics.ClipTitleBandHeight * viewport.RasterizationScale),
            BandPad = 0,
        };

        _hasFrame = true;
        LastClipCount = clipCount;
        frame = _lastFrame;
        return true;
    }

    // ========================================================================
    // Lanes
    // ========================================================================

    private int FillLanes(TimelineViewport viewport, IReadOnlyList<Lane> lanes, double laneAreaTop)
    {
        var count = lanes.Count < MaxLanes ? lanes.Count : MaxLanes;
        EnsureLanes(count);

        for (var i = 0; i < count; i++)
        {
            var lane = lanes[i];
            var label = Intern(lane.Label);

            _lanes[i] = new RudisTimelineLane
            {
                YPx = (float)viewport.LogicalToPhysical(laneAreaTop + lane.TopPx - viewport.ScrollYPx),
                HPx = (float)viewport.LogicalToPhysical(lane.HeightPx),
                Kind = lane.Kind == LaneModel.AudioKind ? 1u : 0u,
                LabelPtr = label.Offset,
                LabelLen = (uint)label.Length,
            };
        }

        return count;
    }

    // ========================================================================
    // Clips — the array this whole phase is about
    // ========================================================================

    private int FillClips(
        TimelineModel model,
        TimelineViewport viewport,
        IReadOnlyList<Lane> lanes,
        string? selectedId,
        double gutter,
        double laneAreaTop)
    {
        var count = model.CullTo(viewport, _cull);
        EnsureClips(count);

        // SHELL-09 (plan 52-08): tell the cache which media the VIEWPORT wants
        // peaks for. This records intent and starts nothing — the cold cycle is the
        // only thing that can cause a fetch (D-21 / Pitfall 7). The wanted set is
        // therefore the culled list's, not the project's, which is 52-05 §4.3's
        // fourth point: prefer the ids that are actually on screen.
        _peaks.NoteWanted(_cull);

        // 53.2 D-05/D-07, on exactly the same terms: the VIEWPORT's wanted set, not
        // the project's, recorded on the paint path and starting nothing. The gate is
        // ClipLayout.WantsFilmstrip, resolved once in TimelineModel.Rebuild where the
        // lane kind is in scope.
        _filmstrips.NoteWanted(_cull);

        var scale = viewport.RasterizationScale;
        var pxPerSecond = viewport.PxPerSecond;
        var scrollY = viewport.ScrollYPx;

        for (var i = 0; i < count; i++)
        {
            var clip = _cull[i];

            var laneIndex = clip.LaneIndex;
            var haveLane = (uint)laneIndex < (uint)lanes.Count;
            var lane = haveLane ? lanes[laneIndex] : default;

            var xLogical = gutter + viewport.TimeUsToPixel(clip.StartUs);
            var wLogical = clip.DurationUs / 1_000_000.0 * pxPerSecond;
            var yLogical = laneAreaTop + (haveLane ? lane.TopPx : 0) - scrollY;
            var hLogical = haveLane ? lane.HeightPx : 0;

            var isAudioLane = haveLane && lane.Kind == LaneModel.AudioKind;
            var fill = isAudioLane ? _audioFill : VideoFillFor(clip.MediaId);

            var flags = 0u;
            if (selectedId is not null && string.Equals(clip.Id, selectedId, StringComparison.Ordinal))
            {
                flags |= TimelineClipFlags.Selected;
            }
            if (clip.HasAudio)
            {
                flags |= TimelineClipFlags.HasAudio;
            }
            // TimelineClipFlags.ReservedAgentEdited is NEVER set — see its own remarks.

            var label = Intern(clip.Label);

            // SHELL-09's fill (plan 52-08). A PURE lookup: the cache either has this
            // media's peaks or it does not, and a miss draws a clip with no fill —
            // which is the correct behaviour for an import whose background
            // extraction has not finished, and the correct PERMANENT behaviour for a
            // source whose audio cannot be decoded. Nothing here can start work.
            var peaksPtr = nint.Zero;
            var peaksLen = 0;
            var peaksBlockUs = 0L;
            if (clip.HasAudio)
            {
                _peaks.TryGetPinnedPeaks(clip.MediaId, out peaksPtr, out peaksLen, out peaksBlockUs);
            }

            // ── 53.2's filmstrip, the same PURE lookup shape as the peaks above.
            //
            //    The band keeps the clip's IDENTITY colour whatever happens below it
            //    (D-01/D-04: the band always draws), so bandFill is captured BEFORE
            //    the body is allowed to move to the backdrop. A band that took its
            //    colour from Fill would vanish into the backdrop the moment tiles
            //    landed — which is the one state this whole phase exists to produce.
            //
            //    `fill` itself is NEVER reassigned: it is also the Border's input
            //    (Darken15 on an audio lane, the owner-override ink on a video one),
            //    and a border derived from the backdrop would be a second colour
            //    change nobody asked for. Only the BODY moves.
            var bandFill = fill;
            var bodyFill = fill;

            var stripPtr = nint.Zero;
            var stripLen = 0;
            var stripInfo = default(StripInfo);
            var stripKey = 0uL;

            // D-04's floor, on this side of the boundary. The comparison is stated in
            // PHYSICAL px against a LOGICAL constant scaled the same way, so it is
            // scale-invariant by construction — a clip that degrades at 100% degrades
            // at 125%, and the floor means the same thing to the user on both.
            var wPhysical = wLogical * scale;
            var floorPhysical = TimelineMetrics.FilmstripWidthFloorPx * scale;

            if (clip.WantsFilmstrip
                && wPhysical >= floorPhysical
                && _filmstrips.TryGetPinnedStrip(clip.MediaId, out stripPtr, out stripLen, out stripInfo))
            {
                stripKey = FilmstripCache.StripKeyFor(clip.MediaId, stripInfo.IsPlaceholder);

                if (stripInfo.IsPlaceholder)
                {
                    flags |= TimelineClipFlags.FilmstripPlaceholder;
                }

                // D-08: the letterbox gaps beside a vertical frame are this colour
                // showing through the tiles' own alpha-0 padding. When the token is
                // not installed the clip keeps its own fill — the pre-token drawing,
                // never an invented colour (the renderer has none of its own).
                if (_filmstripBackdrop != 0)
                {
                    bodyFill = _filmstripBackdrop;
                }
            }
            else
            {
                // Every non-strip case is the SAME drawing, deliberately: an audio
                // lane (D-02/D-03 — its body is the waveform's), a clip below the
                // floor (D-04), a media item whose extraction has not finished, one
                // that was evicted, and one that will never have frames at all
                // (D-15 collapses the last three into one indistinguishable state).
                stripPtr = nint.Zero;
                stripLen = 0;
                stripInfo = default;
            }

            _clips[i] = new RudisTimelineClip
            {
                XPx = (float)(xLogical * scale),
                YPx = (float)(yLogical * scale),
                WPx = (float)wPhysical,
                HPx = (float)(hLogical * scale),
                Fill = bodyFill,
                Border = isAudioLane
                    ? TimelinePalette.Darken15(fill)
                    : VideoBorderFor(fill),
                Flags = flags,
                TrimHandlePx = (float)(TimelineHitTester.HandleWidthFor(wLogical) * scale),
                LabelPtr = label.Offset,
                LabelLen = (uint)label.Length,

                // The audio fill's four inputs (plan 52-08). The pointer addresses the
                // cache's own PINNED byte[] — pinned once when the peaks were cached,
                // never per frame — so the redraw path performs no handle-table
                // operation at all. Both halves move together: a zero length always
                // carries a null pointer, because a stale pointer beside a zero length
                // is the shape the renderer's bound checker calls out as dangerous.
                PeaksPtr = peaksPtr,
                PeaksLen = (uint)peaksLen,
                PeaksBlockUs = peaksBlockUs,

                // The SOURCE in-point, not the timeline position: a trimmed clip must
                // show the part of the envelope it actually plays.
                ClipInUs = clip.InUs,
                ClipDurUs = clip.DurationUs,

                // ── 53.2's tail. Both halves of the strip pointer/length pair move
                //    together, like the peaks pair above and for the same reason: a
                //    stale non-null pointer beside a zero length is the shape the
                //    renderer's own bound checker calls out as dangerous.
                BandFill = bandFill,
                StripLen = (uint)stripLen,
                StripTileW = stripInfo.TileW,
                StripTileH = stripInfo.TileH,
                StripTilesPerRow = stripInfo.TilesPerRow,
                StripTotalTiles = stripInfo.TotalTiles,
                StripCompletedTiles = stripInfo.CompletedTiles,

                // ABI padding, always 0 — its whole job is to land StripPtr on an
                // already-8-aligned offset 120, so the append introduces no hole.
                StripPad = 0,
                StripPtr = stripPtr,
                StripIntervalUs = stripInfo.IntervalUs,
                StripKey = stripKey,
            };
        }

        return count;
    }

    // ========================================================================
    // The drag/trim overlay
    // ========================================================================

    /// <summary>
    /// The ghost as a clip-shaped instance the renderer draws at 50% alpha over the
    /// real clips (<c>quads.rs</c> step 9).
    ///
    /// <para>It takes the SAME fill the real clip has — the media item's poster colour,
    /// or the audio fill on an audio lane — because a ghost in a colour of its own
    /// would read as a different object rather than as the clip being moved.</para>
    ///
    /// <para>No label: the ghost sits directly over a clip that already carries one, and
    /// two overlapping strings at 50% alpha is noise. No trim handles either — a ghost
    /// is not grabbable.</para>
    /// </summary>
    private int FillGhost(TimelineViewport viewport)
    {
        if (!_hasGhost || _ghostLogical.WLogicalPx <= 0 || _ghostLogical.HLogicalPx <= 0)
        {
            return 0;
        }

        var scale = viewport.RasterizationScale;
        var fill = _ghostLogical.AudioLane
            ? _audioFill
            : VideoFillFor(_ghostLogical.MediaId);

        _ghosts[0] = new RudisTimelineClip
        {
            XPx = (float)(_ghostLogical.XLogicalPx * scale),
            YPx = (float)(_ghostLogical.YLogicalPx * scale),
            WPx = (float)(_ghostLogical.WLogicalPx * scale),
            HPx = (float)(_ghostLogical.HLogicalPx * scale),
            Fill = fill,
            Border = _ghostLogical.AudioLane
                ? TimelinePalette.Darken15(fill)
                : VideoBorderFor(fill),
            Flags = 0,
            TrimHandlePx = 0,
            LabelPtr = nint.Zero,
            LabelLen = 0,
            PeaksPtr = nint.Zero,
            PeaksLen = 0,
            PeaksBlockUs = 0,
            ClipInUs = 0,
            ClipDurUs = 0,
        };

        return 1;
    }

    /// <summary>Snap guides, logical → physical. The renderer draws each as an
    /// <c>accent-bright</c> hairline spanning the lane stack.</summary>
    private int FillSnapGuides(TimelineViewport viewport)
    {
        var scale = viewport.RasterizationScale;
        for (var i = 0; i < _snapGuideCount; i++)
        {
            _snapGuidesPhysical[i] = (float)(_snapGuidesLogical[i] * scale);
        }

        return _snapGuideCount;
    }

    // ========================================================================
    // Ruler
    // ========================================================================

    private int FillTicks(TimelineViewport viewport, double gutter)
    {
        RulerTicks.Select(viewport, _tickTimes);

        var count = _tickTimes.Count < MaxTicks ? _tickTimes.Count : MaxTicks;
        EnsureTicks(count);

        var scale = viewport.RasterizationScale;

        for (var i = 0; i < count; i++)
        {
            var timeUs = _tickTimes[i];
            var offset = WriteTickLabel(timeUs, out var length);

            _ticks[i] = new RudisTimelineTick
            {
                XPx = (float)((gutter + viewport.TimeUsToPixel(timeUs)) * scale),

                // Every graduation RulerTicks selects is a labelled one: the ladder
                // already picked the coarsest interval whose labels clear the spacing
                // floor, so there is no such thing here as a minor tick that would have
                // been too crowded to label. Sub-graduations between them are a
                // deliberate omission rather than a forgotten feature — the handoff's
                // own example (README:133) shows labelled graduations only.
                Major = 1,
                LabelPtr = offset,
                LabelLen = (uint)length,
            };
        }

        return count;
    }

    /// <summary>Encode one graduation label into the per-frame tick arena and return
    /// its offset. The arena is rewritten from zero every frame, because a tick label
    /// is a function of the SCROLL position and there is nothing stable to key it
    /// by.</summary>
    private nint WriteTickLabel(long timeUs, out int length)
    {
        if (_tickUsed + TimelineTimecode.RulerLabelMaxBytes > _tickArena.Length)
        {
            length = 0;
            return 0;
        }

        var written = TimelineTimecode.WriteRulerLabel(
            timeUs, _tickArena.AsSpan(_tickUsed, TimelineTimecode.RulerLabelMaxBytes));

        var offset = _tickUsed;
        _tickUsed += written;
        length = written;
        return offset;
    }

    // ========================================================================
    // The label arena
    // ========================================================================

    private readonly record struct LabelSlice(nint Offset, int Length);

    /// <summary>
    /// Intern a label's UTF-8 bytes, encoding it at most once.
    ///
    /// <para>The dictionary is keyed by the label STRING the model already holds — the
    /// same instance across rebuilds, because <c>TimelineModel</c> caches its labels by
    /// source path — so the steady state is a hash lookup and nothing else.</para>
    /// </summary>
    private LabelSlice Intern(string? label)
    {
        if (string.IsNullOrEmpty(label))
        {
            return default;
        }

        if (_labelIndex.TryGetValue(label, out var cached))
        {
            return cached;
        }

        var maxBytes = Encoding.UTF8.GetMaxByteCount(label.Length);
        EnsureLabelArena(_labelUsed + maxBytes);

        var written = Encoding.UTF8.GetBytes(label, _labelArena.AsSpan(_labelUsed));
        var slice = new LabelSlice(_labelUsed, written);
        _labelUsed += written;

        _labelIndex[label] = slice;
        LabelEncodeCount++;
        return slice;
    }

    /// <summary>Drop the whole arena when it has grown past either bound. A wholesale
    /// reset is done at the START of a build, never mid-fill: dropping entries while a
    /// half-built frame still indexes them would draw the WRONG label, which is worse
    /// than drawing none.</summary>
    private void ResetLabelArenaIfExhausted()
    {
        if (_labelIndex.Count <= MaxLabelEntries && _labelUsed <= MaxLabelArenaBytes)
        {
            return;
        }

        _labelIndex.Clear();
        _labelUsed = 0;
    }

    // ========================================================================
    // Growth — by doubling, re-pinned, never per frame
    // ========================================================================

    private void EnsureClips(int count)
    {
        if (_clips.Length >= count)
        {
            return;
        }

        var size = _clips.Length;
        while (size < count)
        {
            size *= 2;
        }

        _clipsPin.Free();
        _clips = new RudisTimelineClip[size];
        _clipsPin = GCHandle.Alloc(_clips, GCHandleType.Pinned);
    }

    private void EnsureLanes(int count)
    {
        if (_lanes.Length >= count)
        {
            return;
        }

        var size = _lanes.Length;
        while (size < count)
        {
            size *= 2;
        }

        _lanesPin.Free();
        _lanes = new RudisTimelineLane[size];
        _lanesPin = GCHandle.Alloc(_lanes, GCHandleType.Pinned);
    }

    private void EnsureTicks(int count)
    {
        if (_ticks.Length >= count)
        {
            return;
        }

        var size = _ticks.Length;
        while (size < count)
        {
            size *= 2;
        }

        _ticksPin.Free();
        _ticks = new RudisTimelineTick[size];
        _ticksPin = GCHandle.Alloc(_ticks, GCHandleType.Pinned);
    }

    private void EnsureLabelArena(int bytes)
    {
        if (_labelArena.Length >= bytes)
        {
            return;
        }

        var size = _labelArena.Length;
        while (size < bytes)
        {
            size *= 2;
        }

        var grown = new byte[size];
        Array.Copy(_labelArena, grown, _labelUsed);
        _labelPin.Free();
        _labelArena = grown;
        _labelPin = GCHandle.Alloc(_labelArena, GCHandleType.Pinned);
    }

    public void Dispose()
    {
        FreeIfAllocated(ref _clipsPin);
        FreeIfAllocated(ref _lanesPin);
        FreeIfAllocated(ref _ticksPin);
        FreeIfAllocated(ref _labelPin);
        FreeIfAllocated(ref _tickArenaPin);
        FreeIfAllocated(ref _ghostsPin);
        FreeIfAllocated(ref _snapGuidesPin);
        _hasFrame = false;
    }

    private static void FreeIfAllocated(ref GCHandle handle)
    {
        if (handle.IsAllocated)
        {
            handle.Free();
        }
    }
}
