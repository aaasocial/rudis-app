using Rudis.Shell.Mirror;

namespace Rudis.Shell.Regions;

/// <summary>
/// The Timeline's drawable state: a lane stack, a flat clip array, one primary
/// selection, and a viewport cull that fills a caller-owned buffer.
///
/// <para><b>This is a PROJECTION, never a source of truth.</b> CLAUDE.md rule 4:
/// the backend owns Project/Clip/Timeline, and the shell is a read-only mirror.
/// <see cref="Rebuild"/> is a pure function of the mirror's typed projection; every
/// mutation goes back out as a <c>rudis_dispatch_command</c> call (D-12), never as
/// a write to anything here.</para>
///
/// <para><b>Culling is O(visible), not O(project).</b> Clips are kept sorted by
/// start time within each lane, with the lane's longest clip recorded, so a redraw
/// binary-searches to the first candidate and stops at the first clip past the
/// window. A per-frame scan of the whole array would make the phase's 1,000-clip
/// criterion a tuning exercise rather than a property (T-52-11), and
/// <c>culling_returns_exactly_what_a_brute_force_filter_would</c> pins the indexed
/// answer to the obvious one over 600 overlapping clips at 119 scroll positions —
/// because "faster" is only worth having when it is indistinguishable.</para>
///
/// <para><b>ONE primary selection (D-08).</b> Structurally, not by convention:
/// there is a single nullable id here and no set to grow into. Since quick
/// 260731-k9b that one selection can be a CLIP or a LANE
/// (<see cref="SelectedTrackIndex"/>) — still one, because each setter clears the
/// other. The design handoff
/// does allow rubber-band and Shift/Ctrl multi-select, but its stated payoff is
/// "the <c>Inspector</c> shows the common-subset behavior" — and the
/// <c>Inspector</c> is Phase 53. Multi-select is DEFERRED to the phase that has a
/// consumer for it, not cancelled.</para>
///
/// <para>No WinUI types and no query operators, by rule (D-13) — the cull path runs
/// per frame and is asserted to allocate exactly zero managed bytes.</para>
/// </summary>
internal sealed class TimelineModel
{
    /// <summary>The clip array cap. A bound, not a prediction: the mirror's payload
    /// is untrusted-shaped and this array is walked by index every frame.</summary>
    public const int MaxClips = 200_000;

    /// <summary>How many distinct label strings are remembered across rebuilds
    /// before the cache is dropped wholesale. Bounded so a long session that touches
    /// thousands of media items cannot grow it without limit.</summary>
    private const int MaxLabelCacheEntries = 4096;

    private static readonly LaneThenStartComparer ClipOrder = new();

    private readonly List<ClipLayout> _clips = [];
    private readonly List<Lane> _lanes = [];
    private readonly List<LaneRange> _laneRanges = [];
    private readonly List<string> _skippedTrackKinds = [];
    private readonly Dictionary<string, MediaFacts> _mediaFacts = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> _labelCache = new(StringComparer.Ordinal);

    /// <summary>Clip id → the values an EDIT needs (plan 52-07). Deliberately NOT
    /// folded into <see cref="ClipLayout"/>: this is read at most once per GESTURE,
    /// where <c>ClipLayout</c> is copied per visible clip per frame. Cleared and
    /// refilled per rebuild against the same string keys, so the steady state reuses
    /// the dictionary's own storage and allocates nothing — the same shape
    /// <see cref="_mediaFacts"/> already relies on.</summary>
    private readonly Dictionary<string, ClipSource> _sources = new(StringComparer.Ordinal);

    /// <summary>Every clip on the timeline, in lane order and, within a lane, in
    /// start order.</summary>
    public IReadOnlyList<ClipLayout> Clips => _clips;

    /// <summary>The lane stack the clips index into.</summary>
    public IReadOnlyList<Lane> Lanes => _lanes;

    /// <summary>Track kinds the last rebuild did not recognise and therefore did not
    /// draw (D-06 / T-52-12). Surfaced rather than swallowed, so 52-09's
    /// introspection hook can report a payload this build cannot fully render.</summary>
    public IReadOnlyList<string> SkippedTrackKinds => _skippedTrackKinds;

    /// <summary>The ONE selected clip (D-08). <c>null</c> when nothing is selected,
    /// and dropped automatically when a rebuild no longer contains it.</summary>
    public string? SelectedClipId { get; private set; }

    /// <summary>
    /// The selected LANE's index into <c>Project.Timeline.Tracks</c>, or
    /// <see cref="NoTrack"/>.
    ///
    /// <para><b>The other half of the ONE primary selection (D-08), not a second
    /// one.</b> Selecting a lane clears <see cref="SelectedClipId"/> and selecting a
    /// clip clears this — the two can never both be set, which
    /// <c>selecting_a_lane_and_selecting_a_clip_are_one_primary_selection</c> pins.
    /// That mutual exclusion is a SAFETY property before it is a UX one: the
    /// remove-track gesture reads whichever is set, and two live selections would let
    /// a destructive cascade name a track the user was not looking at.</para>
    ///
    /// <para><b>A TRACK index, deliberately, not a lane index.</b> The two differ
    /// whenever a track kind this build cannot draw was skipped (D-06 / T-52-12), and
    /// the command this feeds takes a track index (<c>command.rs</c>
    /// <c>RemoveTrack { index }</c>). Storing the drawn position instead would put the
    /// conversion at every read site.</para>
    /// </summary>
    public int SelectedTrackIndex { get; private set; } = NoTrack;

    /// <summary>"No lane is selected". <c>-1</c> rather than a nullable, so reading it
    /// on the redraw path cannot box.</summary>
    public const int NoTrack = -1;

    /// <summary>Is a LANE the current primary selection?</summary>
    public bool HasSelectedTrack => SelectedTrackIndex >= 0;

    /// <summary>
    /// Bumped by every <see cref="Rebuild"/>. The redraw path's change-detection key
    /// (<c>TimelineFrameBuilder</c>): comparing one integer is what lets an idle
    /// Timeline skip a whole frame build, where comparing the clip arrays would cost
    /// more than the rebuild it is trying to avoid.
    ///
    /// <para>It counts REBUILDS, not semantic changes. That is the honest, cheap
    /// reading: the mirror only calls <c>Rebuild</c> when the backend actually pushed a
    /// <c>project:changed</c>, so a spurious bump means one extra drawn frame after a
    /// real backend event — never a MISSED redraw, which is the failure that would
    /// matter.</para>
    /// </summary>
    public int Revision { get; private set; }

    /// <summary>Clips the last <see cref="CullTo"/> handed to the renderer.</summary>
    public int LastDrawnCount { get; private set; }

    /// <summary>Clips the last <see cref="CullTo"/> did NOT hand over — i.e. the
    /// culling's own yield. Read by 52-09 through the introspection hook.</summary>
    public int LastCulledCount { get; private set; }

    /// <summary>How many clip entries the last <see cref="CullTo"/> actually
    /// EXAMINED. The number that makes "O(visible)" checkable instead of claimed:
    /// on a 4,000-clip project with 22 visible it stays under 100.</summary>
    public int LastScannedCount { get; private set; }

    // ========================================================================
    // Rebuild
    // ========================================================================

    /// <summary>
    /// Rebuild lanes and clips from the mirror's typed projection. Buffers are
    /// reused (<c>Clear</c> + <c>Add</c>), so a mirror patch does not allocate a
    /// fresh array every time.
    /// </summary>
    public void Rebuild(Project? project)
    {
        Revision++;
        _clips.Clear();
        _laneRanges.Clear();
        _skippedTrackKinds.Clear();
        _mediaFacts.Clear();
        _sources.Clear();

        var tracks = project?.Timeline?.Tracks;
        LaneModel.BuildInto(tracks, _lanes, _skippedTrackKinds);

        if (project is null || tracks is null || _lanes.Count == 0)
        {
            DropSelectionIfGone();
            return;
        }

        IndexMediaBin(project.MediaBin);

        for (var laneIndex = 0; laneIndex < _lanes.Count; laneIndex++)
        {
            var lane = _lanes[laneIndex];
            var rangeStart = _clips.Count;
            var maxDurationUs = 0L;

            var clips = tracks[lane.TrackIndex]?.Clips;
            if (clips is not null)
            {
                for (var i = 0; i < clips.Count && _clips.Count < MaxClips; i++)
                {
                    var clip = clips[i];
                    if (clip is null)
                    {
                        continue;
                    }

                    var durationUs = clip.OutUs - clip.InUs;
                    if (durationUs > maxDurationUs)
                    {
                        maxDurationUs = durationUs;
                    }

                    var facts = FactsFor(clip.MediaId);
                    var clipId = clip.Id ?? string.Empty;
                    var mediaId = clip.MediaId ?? string.Empty;

                    _clips.Add(new ClipLayout(
                        clipId,
                        mediaId,
                        laneIndex,
                        clip.StartUs,
                        durationUs,

                        // 53.2 D-02 (REVISES Phase 52 D-20): the waveform retreats to
                        // audio-lane clips only — a video clip's body is now the
                        // filmstrip's (see
                        // .planning/phases/53.2-*/artifacts/53.2-01-anatomy-records.md).
                        // This single boolean gates BOTH waveform rendering
                        // (FLAG_HAS_AUDIO via TimelineFrameBuilder) AND peak polling
                        // (PeakCache.NoteWanted), which is why the gate is HERE and not
                        // in the render path (53.2-RESEARCH Pitfall 2).
                        facts.HasAudio && !clip.AudioDetached && lane.Kind == LaneModel.AudioKind,
                        facts.Label,
                        clip.InUs,

                        // 53.2 D-05/D-07: the filmstrip's own want-list, resolved at the
                        // same single site and for the same reason — the LANE KIND is in
                        // scope here and nowhere on the draw path, and a second
                        // derivation site is exactly how D-20's two consumers drifted
                        // apart. An IMAGE is deliberately excluded: its poster already IS
                        // its filmstrip (D-15), so it is a terminal placeholder, not a
                        // pending strip.
                        WantsFilmstrip: lane.Kind == LaneModel.VideoKind
                            && string.Equals(facts.MediaKind, ClipSource.VideoMedia, StringComparison.Ordinal),
                        PosterPath: facts.PosterPath));

                    // The edit-side view of the SAME clip (plan 52-07). It carries the
                    // SOURCE in/out points, which `ClipLayout` deliberately does not:
                    // a trim command names source positions, not a timeline rectangle
                    // (crates/core/src/command.rs:86-100).
                    if (clipId.Length != 0)
                    {
                        _sources[clipId] = new ClipSource(
                            clipId,
                            mediaId,
                            laneIndex,
                            clip.StartUs,
                            clip.InUs,
                            clip.OutUs,
                            facts.DurationUs,
                            facts.MediaKind,
                            facts.HasAudio);
                    }
                }
            }

            var count = _clips.Count - rangeStart;
            SortByStartIfNeeded(rangeStart, count);
            _laneRanges.Add(new LaneRange(rangeStart, count, maxDurationUs));
        }

        DropSelectionIfGone();
    }

    /// <summary>
    /// One pass over the media bin into a reused dictionary, so resolving a clip's
    /// audio and label is a lookup rather than a scan — a per-clip search would make
    /// rebuild O(clips × media), which is exactly the shape that looks fine on a
    /// 3-clip fixture and stalls on a real project.
    /// </summary>
    private void IndexMediaBin(List<MediaBinItem>? bin)
    {
        if (bin is null)
        {
            return;
        }

        if (_labelCache.Count > MaxLabelCacheEntries)
        {
            _labelCache.Clear();
        }

        for (var i = 0; i < bin.Count; i++)
        {
            var item = bin[i];
            if (item is null || string.IsNullOrEmpty(item.Id))
            {
                continue;
            }

            _mediaFacts[item.Id] = new MediaFacts(
                item.HasAudio,
                LabelFor(item),
                item.DurationUs,
                item.MediaKind ?? string.Empty,

                // 53.2 D-13. Normalised HERE, once per media per rebuild, rather than
                // per visible clip per frame: `""` is a path that cannot be opened and
                // would otherwise travel all the way to a decoder to be rejected there.
                string.IsNullOrEmpty(item.PosterPath) ? null : item.PosterPath);
        }
    }

    // ========================================================================
    // The edit-side lookup (plan 52-07)
    // ========================================================================

    /// <summary>
    /// The values an EDIT needs for one clip, as of the last <see cref="Rebuild"/>.
    ///
    /// <para>A miss is DATA, not an error: a pointer handler can legitimately hold an
    /// id that the mirror patch which arrived mid-gesture no longer contains, and the
    /// right answer there is "there is nothing to edit", not an exception on the UI
    /// thread.</para>
    /// </summary>
    public bool TryGetSource(string? clipId, out ClipSource source)
    {
        if (clipId is not null && _sources.TryGetValue(clipId, out source))
        {
            return true;
        }

        source = ClipSource.None;
        return false;
    }

    /// <summary>The selected clip's edit-side view, or
    /// <see cref="ClipSource.None"/>.</summary>
    public bool TryGetSelectedSource(out ClipSource source) =>
        TryGetSource(SelectedClipId, out source);

    /// <summary>The media item's display name, or its filename. Cached by SOURCE
    /// string across rebuilds so a steady-state rebuild allocates nothing: without
    /// the cache, every rebuild would re-cut a substring out of every path.</summary>
    private string LabelFor(MediaBinItem item)
    {
        var displayName = item.DisplayName;
        if (!string.IsNullOrEmpty(displayName))
        {
            return displayName;
        }

        var path = item.Path;
        if (string.IsNullOrEmpty(path))
        {
            return string.Empty;
        }

        if (_labelCache.TryGetValue(path, out var cached))
        {
            return cached;
        }

        var label = Path.GetFileName(path);
        if (string.IsNullOrEmpty(label))
        {
            label = path;
        }

        _labelCache[path] = label;
        return label;
    }

    private MediaFacts FactsFor(string? mediaId)
    {
        // T-52-12: a clip whose media id is absent from the bin is DATA the mirror
        // legitimately produced (an import still in flight, a relink pending), not a
        // fault. It resolves to "no audio, no label" and draws as a plain rectangle.
        if (mediaId is not null && _mediaFacts.TryGetValue(mediaId, out var facts))
        {
            return facts;
        }

        return MediaFacts.Unknown;
    }

    /// <summary>Sort one lane's slice by start time, but only when it is not already
    /// sorted — which it almost always is, because the backend appends clips in
    /// timeline order. The check is O(n) and skips an O(n log n).</summary>
    private void SortByStartIfNeeded(int start, int count)
    {
        for (var i = start + 1; i < start + count; i++)
        {
            if (_clips[i].StartUs < _clips[i - 1].StartUs)
            {
                _clips.Sort(start, count, ClipOrder);
                return;
            }
        }
    }

    // ========================================================================
    // Selection (D-08)
    // ========================================================================

    /// <summary>Make <paramref name="clipId"/> THE selection, replacing whatever was
    /// selected before — a clip, or a lane. <c>null</c> clears it.</summary>
    public void Select(string? clipId)
    {
        SelectedClipId = string.IsNullOrEmpty(clipId) ? null : clipId;

        // Unconditional, including for the null case: "replacing whatever was selected
        // before" is the whole contract of a single primary selection, and a lane that
        // survived a Select(null) would be a second live selection wearing the name of
        // the first one's absence.
        SelectedTrackIndex = NoTrack;
    }

    /// <summary>
    /// Make a LANE the selection — the gutter's gesture (quick 260731-k9b).
    /// <paramref name="trackIndex"/> is an index into <c>Project.Timeline.Tracks</c>;
    /// anything negative clears the selection instead.
    /// </summary>
    public void SelectTrack(int trackIndex)
    {
        SelectedTrackIndex = trackIndex < 0 ? NoTrack : trackIndex;
        SelectedClipId = null;
    }

    public void ClearSelection()
    {
        SelectedClipId = null;
        SelectedTrackIndex = NoTrack;
    }

    /// <summary>
    /// A selection a rebuild invalidated is DROPPED, never left dangling.
    ///
    /// <para>For a CLIP that means one a rebuild removed (deleted, or merged away by a
    /// split's undo): a dangling id would have the Inspector asking Phase 53 about a
    /// clip that no longer exists.</para>
    ///
    /// <para>For a LANE it means a track index no drawn lane claims any more — the
    /// same fix, applied to the same failure. It matters more here, not less: this
    /// index feeds a CASCADING delete, and <c>RemoveTrack</c> shifts every later index
    /// down by one, so a stale one does not merely name nothing — it names somebody
    /// else's track. (The last line of defence against that is not here but at the
    /// gesture, which re-resolves the target after its confirmation and refuses on any
    /// mismatch — T-52-66.)</para>
    /// </summary>
    private void DropSelectionIfGone()
    {
        DropSelectedTrackIfGone();

        var selected = SelectedClipId;
        if (selected is null)
        {
            return;
        }

        for (var i = 0; i < _clips.Count; i++)
        {
            if (string.Equals(_clips[i].Id, selected, StringComparison.Ordinal))
            {
                return;
            }
        }

        SelectedClipId = null;
    }

    private void DropSelectedTrackIfGone()
    {
        var selected = SelectedTrackIndex;
        if (selected < 0)
        {
            return;
        }

        // Against the LANES rather than against the track count: a track this build
        // skipped (an unknown kind — D-06 / T-52-12) draws no lane, so it is not
        // something the user could have selected and must not be something they can
        // still be holding.
        for (var i = 0; i < _lanes.Count; i++)
        {
            if (_lanes[i].TrackIndex == selected)
            {
                return;
            }
        }

        SelectedTrackIndex = NoTrack;
    }

    // ========================================================================
    // Culling
    // ========================================================================

    /// <summary>
    /// Fill a CALLER-OWNED buffer with the clips inside <paramref name="viewport"/>
    /// and return the count. Never returns a fresh collection: the buffer is cleared
    /// and refilled, so a steady-state redraw allocates zero managed bytes
    /// (<c>TimelineHotPathGateTests.cull_and_hit_test_allocate_zero_bytes_at_steady_state</c>).
    /// </summary>
    public int CullTo(TimelineViewport viewport, List<ClipLayout> into)
    {
        ArgumentNullException.ThrowIfNull(into);

        into.Clear();
        LastScannedCount = 0;

        if (viewport is null)
        {
            LastDrawnCount = 0;
            LastCulledCount = _clips.Count;
            return 0;
        }

        var windowStartUs = viewport.StartUs;
        var windowEndUs = viewport.EndUs;

        for (var laneIndex = 0; laneIndex < _laneRanges.Count; laneIndex++)
        {
            if (!viewport.IsLaneVisible(laneIndex))
            {
                continue;
            }

            var range = _laneRanges[laneIndex];
            if (range.Count == 0)
            {
                continue;
            }

            // Any clip overlapping the window must START at or after
            // (windowStart - the lane's longest clip): a clip that begins earlier
            // than that cannot still be running when the window opens. That is what
            // makes the binary search below exact even when clips OVERLAP, where a
            // search on end times would silently drop a long clip.
            var earliestStartUs = SaturatingSubtract(windowStartUs, range.MaxDurationUs);
            var index = LowerBoundByStart(range, earliestStartUs);

            for (; index < range.Start + range.Count; index++)
            {
                LastScannedCount++;
                var clip = _clips[index];
                if (clip.StartUs > windowEndUs)
                {
                    break;
                }

                if (clip.EndUs >= windowStartUs)
                {
                    into.Add(clip);
                }
            }
        }

        LastDrawnCount = into.Count;
        LastCulledCount = _clips.Count - into.Count;
        return into.Count;
    }

    /// <summary>First index in the lane's slice whose <c>StartUs</c> is at least
    /// <paramref name="startUs"/>.</summary>
    private int LowerBoundByStart(LaneRange range, long startUs)
    {
        var low = range.Start;
        var high = range.Start + range.Count;
        while (low < high)
        {
            var mid = low + ((high - low) >> 1);
            if (_clips[mid].StartUs < startUs)
            {
                low = mid + 1;
            }
            else
            {
                high = mid;
            }
        }

        return low;
    }

    private static long SaturatingSubtract(long value, long amount)
    {
        var result = unchecked(value - amount);
        if (amount > 0 && result > value)
        {
            return long.MinValue;
        }

        if (amount < 0 && result < value)
        {
            return long.MaxValue;
        }

        return result;
    }

    // ========================================================================

    /// <summary>What one lane's clips occupy in <see cref="_clips"/>, plus the
    /// longest clip on it — the bound that makes the lower-bound search exact.</summary>
    private readonly record struct LaneRange(int Start, int Count, long MaxDurationUs);

    /// <summary>The per-media facts a clip needs, resolved once per rebuild.
    /// <paramref name="DurationUs"/> and <paramref name="MediaKind"/> are the EDIT
    /// side's (plan 52-07): the first bounds a trim (<c>command.rs:976-988</c>), the
    /// second decides whether a cross-lane drag is legal at all
    /// (<c>model.rs:1310-1318</c>). <paramref name="PosterPath"/> is 53.2 D-13's
    /// placeholder source, already NORMALISED to <c>null</c> for absent-or-empty so
    /// that <see cref="ClipLayout.PosterPath"/> has exactly two states rather than
    /// three.</summary>
    private readonly record struct MediaFacts(
        bool HasAudio, string Label, long DurationUs, string MediaKind, string? PosterPath)
    {
        public static readonly MediaFacts Unknown =
            new(false, string.Empty, 0, string.Empty, null);
    }

    /// <summary>A singleton comparer, so the rare sort does not allocate one.</summary>
    private sealed class LaneThenStartComparer : IComparer<ClipLayout>
    {
        public int Compare(ClipLayout x, ClipLayout y) => x.StartUs.CompareTo(y.StartUs);
    }
}
