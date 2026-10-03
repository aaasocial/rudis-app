using System.Diagnostics;
using Rudis.Shell.Mirror;

namespace Rudis.Shell.Regions;

/// <summary>One drawn track lane: where it sits, how tall it is, what it is called,
/// and which mirrored track it came from.</summary>
/// <param name="Index">Position in the LANE stack (0-based, contiguous). Skipped
/// tracks leave no gap here — see <see cref="LaneModel"/>.</param>
/// <param name="Kind"><c>"video"</c> or <c>"audio"</c>, the snake_case
/// <c>TrackKind</c> the mirror carries.</param>
/// <param name="Label">The `TrackHeader` gutter's text: <c>V1</c>, <c>V2</c>,
/// <c>A1</c>, … numbered per KIND (README.md:134). Video counts UPWARD FROM THE
/// BOTTOM lane, audio downward from the top — see <see cref="LaneModel"/>.</param>
/// <param name="TopPx">Lane top in LOGICAL px, measured from the top of the lane
/// area (i.e. below the Timeline header and ruler), before scroll.</param>
/// <param name="HeightPx">48 for video, 42 for audio (README.md:239).</param>
/// <param name="TrackIndex">Index into <c>Project.Timeline.Tracks</c> — the way
/// back to the mirrored track this lane draws.</param>
internal readonly record struct Lane(
    int Index,
    string Kind,
    string Label,
    double TopPx,
    double HeightPx,
    int TrackIndex)
{
    public double BottomPx => TopPx + HeightPx;
}

/// <summary>
/// Mirrored tracks → drawable lanes.
///
/// <para><b>D-06: there are exactly TWO lane kinds, and that is a domain fact, not
/// a simplification.</b> <c>crates/core::TrackKind</c> is
/// <c>Video | Audio</c> — see <c>crates/core/src/model.rs:399-409</c>, where
/// <c>Track { kind: TrackKind, clips: Vec&lt;Clip&gt; }</c> sits directly above the
/// two-variant enum. The design handoff's <c>T</c> (text/captions, 28px, the yellow
/// caption token — README.md:134-135) therefore has NOTHING to port: building it
/// would be inventing new BACKEND scope under the banner of a UI port, inside a
/// milestone whose core crates are frozen by the <c>engine-axis-freeze</c> tag.
/// The gap is recorded as a deferred item by plan 52-10 rather than silently
/// dropped. <b>Do not "fix" the omission here.</b></para>
///
/// <para><b>VIDEO LANES ARE NUMBERED FROM THE BOTTOM UP, and that inversion
/// relative to Vec order is the whole point.</b> Track index 0 is BOTH the top lane
/// AND the top compositing layer: <c>crates/engine/src/compositor.rs:1118</c> walks
/// <c>layers.iter().rev()</c>, so index 0 is painted LAST — on top (stated outright
/// at <c>crates/core/src/model.rs:326-327</c>, and <c>command.rs:2206</c> inserts a
/// new video track at index 0 precisely to make it the new top layer). The
/// BOTTOM-most video lane is therefore the BASE layer, and the base layer is what
/// Premiere and Resolve call V1. Numbering the first video track met <c>V1</c>
/// would have named the TOP layer V1 and — worse — let <c>AddTrack</c> steal the
/// name V1 from the lane that already had it. Counting down from the video total
/// fixes the labels without touching lane ORDER, which was already right.</para>
///
/// <para><b>Audio is NOT inverted:</b> <c>A1</c> is the first audio lane and audio
/// numbers grow downward. <c>command.rs:2210</c> APPENDS audio, so the first audio
/// lane keeps its name when a track is added — the convention already holds.</para>
///
/// <para>An unrecognised kind is SKIPPED with a diagnostic and never rendered
/// (T-52-12): the mirror carries JSON produced by a backend that may be newer than
/// this build, and a lane model that threw on an unknown string would turn a
/// forward-compatible payload into a crash.</para>
///
/// <para>No WinUI types, by rule — see the directory's contract in
/// <c>Rudis.Shell.Tests.csproj</c> and the gate that enforces it.</para>
/// </summary>
internal static class LaneModel
{
    /// <summary>The snake_case <c>TrackKind::Video</c> discriminant.</summary>
    public const string VideoKind = "video";

    /// <summary>The snake_case <c>TrackKind::Audio</c> discriminant.</summary>
    public const string AudioKind = "audio";

    /// <summary>How many lanes may be built from one payload. A bound, not a
    /// prediction: the track list arrives as untrusted-shaped JSON, and a lane stack
    /// is walked per frame.</summary>
    public const int MaxLanes = 512;

    private static readonly Lane[] NoLanes = [];

    /// <summary>Pre-rendered <c>V1</c>…/<c>A1</c>… labels so a rebuild allocates no
    /// strings for a realistic track count.</summary>
    private static readonly string[] VideoLabels = BuildLabels('V');
    private static readonly string[] AudioLabels = BuildLabels('A');

    private const int CachedLabelCount = 64;

    /// <summary>Convenience form: allocates a fresh list. The cold path (tests,
    /// one-off queries) uses this; <see cref="BuildInto"/> is what a rebuild calls.</summary>
    public static IReadOnlyList<Lane> Build(
        IReadOnlyList<Track>? tracks, ICollection<string>? skippedKinds = null)
    {
        if (tracks is null || tracks.Count == 0)
        {
            return NoLanes;
        }

        var lanes = new List<Lane>(tracks.Count);
        BuildInto(tracks, lanes, skippedKinds);
        return lanes;
    }

    /// <summary>
    /// Fill a CALLER-OWNED buffer and return the lane count. <paramref name="into"/>
    /// is cleared first and reused, so a mirror patch does not allocate a fresh lane
    /// array every time a track's clips change.
    /// </summary>
    public static int BuildInto(
        IReadOnlyList<Track>? tracks, List<Lane> into, ICollection<string>? skippedKinds = null)
    {
        ArgumentNullException.ThrowIfNull(into);

        into.Clear();
        if (tracks is null)
        {
            return 0;
        }

        // Counted over ALL tracks, DELIBERATELY — not over the ones that fit. The loop
        // below stops at MaxLanes, and counting only the drawn tracks would rename the
        // bottom-most DRAWN lane the moment a stack crossed the cap, which is exactly
        // the "the label moved under me" failure this numbering exists to prevent.
        var videoTotal = 0;
        for (var i = 0; i < tracks.Count; i++)
        {
            if (tracks[i]?.Kind == VideoKind)
            {
                videoTotal++;
            }
        }

        var videoSeen = 0;
        var audioCount = 0;
        var top = 0.0;

        for (var i = 0; i < tracks.Count && into.Count < MaxLanes; i++)
        {
            var kind = tracks[i]?.Kind;

            string label;
            double height;
            if (kind == VideoKind)
            {
                // Counting DOWN: the first video met (topmost lane, top compositing
                // layer) is the HIGHEST number, the last one met (bottom lane, base
                // layer) is V1. Numbering is by video-ordinal, not by lane position,
                // so an interleaved [V, A, V, A] project reads V2, A1, V1, A2.
                label = LabelFor(VideoLabels, 'V', videoTotal - videoSeen++);
                height = TimelineMetrics.VideoLaneHeight;
            }
            else if (kind == AudioKind)
            {
                label = LabelFor(AudioLabels, 'A', ++audioCount);
                height = TimelineMetrics.AudioLaneHeight;
            }
            else
            {
                // D-06 / T-52-12: skipped, diagnosed, never fatal. The `T` lane the
                // design handoff draws lands here BY DESIGN — it has no domain
                // backing, and plan 52-10 files the gap.
                skippedKinds?.Add(kind ?? string.Empty);
                Trace.WriteLine(
                    $"[Timeline] skipping track {i}: unknown TrackKind '{kind}' " +
                    "(the domain has exactly video|audio — crates/core/src/model.rs:399-409)");
                continue;
            }

            into.Add(new Lane(into.Count, kind, label, top, height, i));
            top += height;
        }

        return into.Count;
    }

    /// <summary>Total drawn height of a lane stack, in logical px.</summary>
    public static double ContentHeightPx(IReadOnlyList<Lane>? lanes)
    {
        if (lanes is null || lanes.Count == 0)
        {
            return 0;
        }

        var last = lanes[^1];
        return last.TopPx + last.HeightPx;
    }

    private static string LabelFor(string[] cache, char prefix, int oneBased) =>
        oneBased < cache.Length ? cache[oneBased] : prefix + oneBased.ToString();

    private static string[] BuildLabels(char prefix)
    {
        var labels = new string[CachedLabelCount];
        labels[0] = string.Empty;
        for (var i = 1; i < labels.Length; i++)
        {
            labels[i] = prefix + i.ToString();
        }

        return labels;
    }
}
