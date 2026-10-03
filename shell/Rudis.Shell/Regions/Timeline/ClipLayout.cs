namespace Rudis.Shell.Regions;

/// <summary>
/// One clip, as the Timeline draws it: a rectangle in an array (SHELL-05).
///
/// <para>There is no per-clip control anywhere in this region — that is the
/// requirement, not an optimisation — so this struct is the ENTIRE representation
/// of a clip on screen. It is deliberately a <c>readonly record struct</c>: the
/// cull path copies thousands of these per second into a caller-owned buffer, and a
/// class would make every visible clip a heap object with a GC lifetime.</para>
///
/// <para>No colours: the poster palette and the agent-edited marker are 52-06's,
/// and they come from <c>Theme/Tokens.xaml</c> by name. No truncation either —
/// <see cref="Label"/> is the whole display name and eliding it to fit the
/// rectangle is the renderer's problem, because only the renderer knows the font
/// metrics.</para>
/// </summary>
/// <param name="Id">The mirrored <c>Clip.id</c> — what a
/// <c>rudis_dispatch_command</c> edit names (D-12).</param>
/// <param name="MediaId">The mirrored <c>Clip.media_id</c> — the key 52-06 uses to
/// look up cached waveform peaks (SHELL-09).</param>
/// <param name="LaneIndex">Index into the lane stack, NOT into
/// <c>Project.Timeline.Tracks</c>; unknown track kinds are skipped, so the two can
/// differ (see <c>LaneModel</c>).</param>
/// <param name="StartUs">Timeline position.</param>
/// <param name="DurationUs">Timeline duration, i.e. <c>OutUs - InUs</c>. NOT the
/// source range: a trimmed clip's timeline length is its trimmed length.</param>
/// <param name="HasAudio">53.2 D-02, REVISING Phase 52's D-20: the media item has
/// audio, this clip has not had it detached, AND the clip sits on an AUDIO lane. D-20
/// drove the waveform fill for video clips too; that body is now the filmstrip's, so a
/// video clip shows frames rather than an envelope and its audio is invisible on the
/// timeline until detached onto its own audio-track clip — which does show one. The
/// revision is recorded in
/// <c>.planning/phases/53.2-*/artifacts/53.2-01-anatomy-records.md</c>.
///
/// <para>This one boolean is the gate for BOTH consumers — <c>TimelineFrameBuilder</c>'s
/// <c>TimelineClipFlags.HasAudio</c> and <c>PeakCache.NoteWanted</c>'s poll intent — so
/// it is resolved once, where the lane kind is in scope, and never re-derived.</para></param>
/// <param name="Label">The media item's display name, or its filename. Whole, not
/// truncated.</param>
/// <param name="InUs">The clip's IN-POINT within its source media (plan 52-08). The
/// one source-side number the DRAW path needs, because the waveform fill maps
/// <c>[InUs, InUs + DurationUs)</c> onto the media's peak array — a trimmed clip must
/// show the part of the envelope it actually plays, not the head of the file.
///
/// <para>It lives here rather than behind <c>TimelineModel.TryGetSource</c>, which
/// carries the rest of the edit-side view, because this one is read per VISIBLE CLIP
/// PER FRAME: a dictionary lookup per clip per redraw is the shape 52-07 deliberately
/// kept OFF this struct, and it is also the shape that would put one here. It is
/// optional so that every existing construction site — and every test that predates
/// the waveform fill — keeps compiling and keeps meaning what it meant.</para></param>
/// <param name="WantsFilmstrip">53.2 D-05/D-07: true only for a clip on a VIDEO lane
/// whose media kind is video. Drives <c>FilmstripCache.NoteWanted</c> exactly as
/// <paramref name="HasAudio"/> drives <c>PeakCache.NoteWanted</c> — which is why it is
/// resolved ONCE, in <c>TimelineModel.Rebuild</c>, rather than re-derived per draw:
/// the lane kind is not otherwise in scope on the render path, and a second derivation
/// site is how the two halves of D-02 drifted apart in the first place.
///
/// <para>Optional and defaulting to FALSE, following <paramref name="InUs"/>'s pattern:
/// every construction site that predates the filmstrip keeps compiling, and a clip
/// nobody asked to carry one does not silently start WANTING one — wanting is what
/// turns into extraction, I/O and GPU residency.</para></param>
/// <param name="PosterPath">53.2 D-13: the mirrored media item's <c>poster_path</c> —
/// the placeholder the frame body shows until (or instead of) a real strip. <c>null</c>
/// when the media has no poster. Carried OPAQUE here: it arrives from untrusted-shaped
/// mirror JSON and the CONSUMER that opens it is the one that must bound the decode and
/// degrade to a token fill on failure (T-53.2-02).</param>
internal readonly record struct ClipLayout(
    string Id,
    string MediaId,
    int LaneIndex,
    long StartUs,
    long DurationUs,
    bool HasAudio,
    string Label,
    long InUs = 0,
    bool WantsFilmstrip = false,
    string? PosterPath = null)
{
    /// <summary>Timeline end position. Saturating, because <c>StartUs</c> and
    /// <c>DurationUs</c> both arrive from untrusted-shaped JSON and a wrapped
    /// <c>long</c> would make a clip's rectangle appear on the wrong side of the
    /// viewport instead of simply off it.</summary>
    public long EndUs
    {
        get
        {
            var end = unchecked(StartUs + DurationUs);
            if (DurationUs > 0 && end < StartUs)
            {
                return long.MaxValue;
            }

            if (DurationUs < 0 && end > StartUs)
            {
                return long.MinValue;
            }

            return end;
        }
    }
}
