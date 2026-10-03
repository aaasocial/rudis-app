namespace Rudis.Shell.Regions;

/// <summary>
/// Everything an EDIT needs to know about one clip, resolved once per gesture.
///
/// <para><b>Why this is not <see cref="ClipLayout"/>.</b> <c>ClipLayout</c> is the
/// per-frame drawing struct: it is copied thousands of times a second into the cull
/// buffer, so every field it carries is paid for on the redraw path. The values below
/// are read at most ONCE PER GESTURE — on the pointer press that starts a drag or a
/// trim, and on the key that splits or deletes — so they live in a lookup the redraw
/// path never touches.</para>
///
/// <para><b>The source range is the load-bearing part.</b> <c>crates/core</c>'s trim
/// command takes SOURCE in/out points, not a timeline rectangle
/// (<c>crates/core/src/command.rs:86-100</c>), and a left-edge trim moves
/// <c>start_us</c> by the SAME delta all by itself. A UI that sent a timeline rect —
/// or that sent a trim AND a move — would double-apply the shift and slide the clip.
/// <c>ClipLayout</c> carries only <c>StartUs</c>/<c>DurationUs</c> and therefore
/// cannot express the trim at all; this can.</para>
///
/// <para><see cref="MediaKind"/> and <see cref="MediaHasAudio"/> are here for the
/// cross-lane drag: <c>crates/core/src/model.rs:1310-1318</c>'s
/// <c>track_accepts_media</c> is the domain's own compatibility rule, and knowing it
/// locally is what lets a drag onto an incompatible lane be REFUSED as a gesture
/// rather than dispatched and bounced back as an error banner.</para>
///
/// <para>No WinUI types, by rule (D-13).</para>
/// </summary>
/// <param name="ClipId">The mirrored <c>Clip.id</c> — what every edit names.</param>
/// <param name="MediaId">The mirrored <c>Clip.media_id</c>.</param>
/// <param name="LaneIndex">Index into the LANE stack (not into
/// <c>Project.Timeline.Tracks</c> — see <c>LaneModel</c>).</param>
/// <param name="StartUs">Timeline position.</param>
/// <param name="InUs">Source in-point.</param>
/// <param name="OutUs">Source out-point, exclusive.</param>
/// <param name="MediaDurationUs">The media item's container duration, or 0 when the
/// bin does not know it (a still image, or a clip whose media id is not in the bin
/// yet). 0 means UNBOUNDED, matching the domain's own check, which skips the bound
/// for a zero duration.</param>
/// <param name="MediaKind"><c>"video"</c> | <c>"audio"</c> | <c>"image"</c>.</param>
/// <param name="MediaHasAudio">The media item's own <c>has_audio</c> — NOT the clip's
/// effective audio (that is <c>ClipLayout.HasAudio</c>, which also accounts for a
/// detached track). The domain's compatibility rule asks about the MEDIA.</param>
internal readonly record struct ClipSource(
    string ClipId,
    string MediaId,
    int LaneIndex,
    long StartUs,
    long InUs,
    long OutUs,
    long MediaDurationUs,
    string MediaKind,
    bool MediaHasAudio)
{
    /// <summary>The snake_case <c>MediaKind::Video</c> discriminant
    /// (<c>crates/core/src/model.rs:1297-1303</c>). Named here rather than open-coded
    /// at the one comparison site, so the MEDIA kinds and <c>LaneModel</c>'s TRACK
    /// kinds cannot be confused by a reader — they share two spellings and mean
    /// different things.</summary>
    public const string VideoMedia = "video";

    /// <summary>The snake_case <c>MediaKind::Audio</c> discriminant.</summary>
    public const string AudioMedia = "audio";

    /// <summary>The snake_case <c>MediaKind::Image</c> discriminant. There is no image
    /// TRACK kind — an image goes on a video track.</summary>
    public const string ImageMedia = "image";

    /// <summary>Nothing resolved. <see cref="IsEmpty"/> is true.</summary>
    public static readonly ClipSource None =
        new(string.Empty, string.Empty, -1, 0, 0, 0, 0, string.Empty, false);

    /// <summary>True when no clip was resolved — never an exception, because a
    /// pointer handler can legitimately hold an id one mirror patch stale.</summary>
    public bool IsEmpty => string.IsNullOrEmpty(ClipId);

    /// <summary>Timeline length = the trimmed source length
    /// (<c>crates/core/src/model.rs:1159-1161</c>).</summary>
    public long DurationUs => OutUs - InUs;

    /// <summary>Exclusive timeline end.</summary>
    public long EndUs => StartUs + DurationUs;
}
