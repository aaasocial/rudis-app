using System.Buffers;
using System.Text;
using System.Text.Json;

namespace Rudis.Shell.Regions;

/// <summary>
/// The Timeline's edits, as the exact wire JSON the backend already deserialises.
///
/// <para><b>D-12, stated plainly: this phase adds NO FFI export for edits, and it
/// modifies no <c>Command</c> variant.</b> Trim, split, rearrange, delete,
/// duplicate — and, since plan 52-14, add-track and remove-track — all already
/// exist in <c>crates/core::Command</c> — a crate the
/// <c>engine-axis-freeze</c> tag makes CALLABLE and unmodifiable — and they all
/// travel through the ONE generic entry point the shell has had since Phase 47:
/// <c>rudis_dispatch_command</c>, whose args are
/// <c>{"cmd": {..Command..}}</c> (<c>crates/ffi/src/dispatch.rs:29-33</c>). The
/// Timeline is a new CONSUMER of a proven pipeline, never a new pipeline.</para>
///
/// <para><b>This file is pure SERIALISATION plus the clamping rules, and nothing
/// else.</b> Every signature takes DOMAIN values — ids and microseconds — never
/// pixels. Pixel→µs conversion happened one layer up, in the interaction state
/// machine, against <c>TimelineViewport</c>'s single coordinate system (D-14).</para>
///
/// <para><b>Built with a JSON writer, never string interpolation</b> (T-52-34). A
/// clip id is domain data that ultimately came from JSON, and interpolating one that
/// contained a quote would let it restructure the payload around it. The writer
/// escapes; an interpolation cannot.</para>
///
/// <para><b>The clamping rules live HERE, once, beside the reason they exist.</b>
/// Each cites the arm of <c>crates/core/src/command.rs</c> that would otherwise
/// refuse the edit, so the clamp and its justification travel together and neither
/// can be "simplified" without the other being read.</para>
///
/// <para><b>The INVERSE-ONLY field names are deliberately ABSENT from this file,
/// including from its comments.</b> Several <c>Command</c> variants carry payloads a
/// forward caller must never send — the exact-restore carriers on the trim command
/// (<c>command.rs:90-99</c> and its twins: apply derives the content-preserving remap
/// itself, and a hand-sent one would inject state into an edit that is supposed to
/// compute it), and the within-track position field on the cross-track move
/// (<c>command.rs:48-57</c> — it exists ONLY so the inverse can restore an exact
/// index, and sending one would pin a user's drag to a position the user did not
/// choose). Their acceptance check is a literal grep over this file pinned at zero,
/// so naming any of them even to explain the absence would blunt the gate into
/// indistinguishability between "used" and "mentioned" — the trap plans 52-02, 52-04,
/// 52-05 and 52-06 each recorded. <c>TimelineCommandTests</c> names them, asserts each
/// absent from every builder's output, and carries the same reasoning in full.</para>
///
/// <para>No WinUI types, by rule (D-13) — the whole file is unit-tested in a plain
/// <c>net9.0-windows</c> host with no window.</para>
/// </summary>
internal static class TimelineCommands
{
    /// <summary>
    /// The shortest source range a clip may be trimmed to.
    ///
    /// <para>One microsecond, because that is exactly what the domain requires:
    /// <c>command.rs:959-965</c> refuses <c>new_out_us &lt;= new_in_us</c> and says
    /// nothing about a minimum beyond it. A larger, friendlier floor (one frame, say)
    /// would be inventing a rule the backend does not have, and the two would then
    /// disagree about what a legal clip is.</para>
    /// </summary>
    public const long MinSourceLengthUs = 1;

    // ── the reused writer ───────────────────────────────────────────────────
    //
    // [ThreadStatic] rather than a lock: these are built on the UI thread, once per
    // gesture (on pointer RELEASE — D-11), never per frame, so contention is not the
    // concern. What IS worth avoiding is a fresh writer plus a fresh buffer per edit
    // for no reason, and a thread-local pair costs nothing and cannot be raced.

    [ThreadStatic]
    private static ArrayBufferWriter<byte>? _buffer;

    [ThreadStatic]
    private static Utf8JsonWriter? _writer;

    // ========================================================================
    // The CLIP wire shapes — six of them. One method each, so the payload is
    // readable AT the boundary rather than assembled from a table. The two TRACK
    // shapes plan 52-14 added follow further down, in their own section.
    // ========================================================================

    /// <summary>
    /// <c>{"cmd":{"type":"trim_clip","data":{"id":..,"new_in_us":..,"new_out_us":..}}}</c>
    ///
    /// <para>SOURCE in/out points, never a timeline rectangle.
    /// <c>command.rs:82-100</c>: a changed in-point moves <c>start_us</c> by the SAME
    /// delta, so the frames already on the timeline stay put (a non-ripple trim). A
    /// caller that ALSO sent a move would apply the shift twice.</para>
    /// </summary>
    public static string TrimClip(string clipId, long newInUs, long newOutUs)
    {
        ClampSourceRange(ref newInUs, ref newOutUs, long.MaxValue);

        var w = Begin("trim_clip");
        w.WriteString("id", clipId);
        w.WriteNumber("new_in_us", newInUs);
        w.WriteNumber("new_out_us", newOutUs);
        return End(w);
    }

    /// <summary>
    /// <c>{"cmd":{"type":"split_clip","data":{"id":..,"at_position_us":..}}}</c>
    ///
    /// <para><c>at_position_us</c> is a TIMELINE position (<c>command.rs:101-107</c>),
    /// not a source offset. The two differ by <c>start_us - in_us</c>, and a builder
    /// that confused them would cut at a plausible-looking but wrong frame — the kind
    /// of wrong that looks right until someone counts.</para>
    /// </summary>
    public static string SplitClip(string clipId, long atPositionUs)
    {
        var w = Begin("split_clip");
        w.WriteString("id", clipId);
        w.WriteNumber("at_position_us", atPositionUs);
        return End(w);
    }

    /// <summary>
    /// <c>{"cmd":{"type":"duplicate_clip","data":{"id":..}}}</c>
    ///
    /// <para>The id and nothing else. <c>command.rs:123-127</c> places the copy at the
    /// ORIGINAL's timeline end on the same track and mints its own deterministic id —
    /// the UI does not choose either, so it must not pretend to by sending one.</para>
    /// </summary>
    public static string DuplicateClip(string clipId)
    {
        var w = Begin("duplicate_clip");
        w.WriteString("id", clipId);
        return End(w);
    }

    /// <summary><c>{"cmd":{"type":"remove_clip","data":{"id":..}}}</c> — the id and
    /// nothing else (<c>command.rs:65-66</c>). Its inverse restores the clip at its
    /// EXACT prior index, which is why a delete is losslessly undoable.</summary>
    public static string RemoveClip(string clipId)
    {
        var w = Begin("remove_clip");
        w.WriteString("id", clipId);
        return End(w);
    }

    /// <summary>
    /// <c>{"cmd":{"type":"move_clip","data":{"id":..,"new_start_us":..}}}</c>
    ///
    /// <para>SAME-TRACK reposition only (<c>command.rs:41</c>). A cross-lane drag is a
    /// different command — see <see cref="MoveClipToTrack"/>.</para>
    ///
    /// <para>The start clamps to 0. <c>command.rs:850-852</c> clamps it too (a drag
    /// past the origin pins the clip at 0), so this is not compensating for a missing
    /// backend rule — it is keeping the LOCAL ghost and the COMMITTED state in
    /// agreement, so the clip does not visibly jump on release.</para>
    /// </summary>
    public static string MoveClip(string clipId, long newStartUs)
    {
        var w = Begin("move_clip");
        w.WriteString("id", clipId);
        w.WriteNumber("new_start_us", NonNegative(newStartUs));
        return End(w);
    }

    /// <summary>
    /// <c>{"cmd":{"type":"move_clip_to_track","data":{"id":..,"target_track":..,"new_start_us":..}}}</c>
    ///
    /// <para>The cross-lane drag. <paramref name="targetTrack"/> is an index into
    /// <c>Project.Timeline.Tracks</c> — a TRACK index, not a lane index; the two differ
    /// whenever a track kind this build cannot draw is skipped (see
    /// <c>LaneModel</c>), so the caller resolves it through <c>Lane.TrackIndex</c>.</para>
    ///
    /// <para>The optional inverse-only field this command also accepts is never
    /// written; see this class's own remarks for what it is and why naming it here
    /// would blunt its gate.</para>
    /// </summary>
    public static string MoveClipToTrack(string clipId, int targetTrack, long newStartUs)
    {
        var w = Begin("move_clip_to_track");
        w.WriteString("id", clipId);
        w.WriteNumber("target_track", targetTrack < 0 ? 0 : targetTrack);
        w.WriteNumber("new_start_us", NonNegative(newStartUs));
        return End(w);
    }

    // ========================================================================
    // Track management (plan 52-14) — two more commands the backend already had
    // ========================================================================

    /// <summary>
    /// <c>{"cmd":{"type":"add_track","data":{"kind":..}}}</c>
    ///
    /// <para><b>THE SEMANTIC THAT SURPRISES</b> (<c>command.rs:390-393</c>): a VIDEO
    /// track is INSERTED AT INDEX 0 — lowest index is the top compositing layer, so the
    /// new lane appears at the TOP of the stack — while an AUDIO track is APPENDED at the
    /// bottom. "Add a track" is therefore not "append a track", and UI copy or a test
    /// that assumed otherwise would be describing a different command. The new track is
    /// always EMPTY, and the inverse is <c>RemoveTrack</c> at the resulting index, so the
    /// gesture is losslessly undoable without the caller arranging anything.</para>
    ///
    /// <para><paramref name="kind"/> must be one of <see cref="LaneModel.VideoKind"/> /
    /// <see cref="LaneModel.AudioKind"/> — the SAME two constants the lane builder
    /// compares a mirrored track against, never a third copy of the literals. Anything
    /// else throws rather than being sent: <c>TrackKind</c> has exactly two variants
    /// (<c>crates/core/src/model.rs:399-409</c>), so a third value could only ever come
    /// back as a domain error that no user caused and no user can read.</para>
    /// </summary>
    public static string AddTrack(string kind)
    {
        ArgumentNullException.ThrowIfNull(kind);

        if (kind != LaneModel.VideoKind && kind != LaneModel.AudioKind)
        {
            throw new ArgumentOutOfRangeException(
                nameof(kind),
                kind,
                "the domain has exactly two track kinds (crates/core/src/model.rs:399-409); " +
                "pass LaneModel.VideoKind or LaneModel.AudioKind");
        }

        var w = Begin("add_track");
        w.WriteString("kind", kind);
        return End(w);
    }

    /// <summary>
    /// <c>{"cmd":{"type":"remove_track","data":{"index":..}}}</c>
    ///
    /// <para><b>THIS ONE CASCADES</b> (<c>command.rs:394-396</c>): it takes every clip
    /// the track holds with it, and every track AFTER <paramref name="index"/> shifts
    /// down by one. That shift is why the index must be resolved at the MOMENT of the
    /// gesture and never cached across a mirror update — an index read before an earlier
    /// removal names a different track afterwards. It is also why the caller asks first
    /// when the track holds clips (v6's rule, <c>main.ts</c> <c>removeTrackAt</c>).</para>
    ///
    /// <para>Undo re-inserts the WHOLE track — kind plus its entire clip list — at the
    /// exact original index, so the cascade is reversible; the confirmation exists
    /// because "undoable" is not the same as "asked".</para>
    ///
    /// <para><paramref name="index"/> indexes <c>Project.Timeline.Tracks</c> — a TRACK
    /// index, not a lane index (the two differ whenever a track kind this build cannot
    /// draw is skipped), so the caller resolves it through <c>Lane.TrackIndex</c>. A
    /// negative one is not an index and throws rather than being sent.</para>
    /// </summary>
    public static string RemoveTrack(int index)
    {
        if (index < 0)
        {
            throw new ArgumentOutOfRangeException(
                nameof(index),
                index,
                "a track index is a usize on the domain side; a negative one can only ever " +
                "come back as a deserialisation error");
        }

        var w = Begin("remove_track");
        w.WriteNumber("index", index);
        return End(w);
    }

    // ========================================================================
    // The domain-aware entry points: clamp, then serialise
    // ========================================================================

    /// <summary>
    /// Resolve a trim gesture into the SOURCE range it asks for, clamped to what the
    /// domain will accept. Returns <see langword="false"/> only when there is nothing
    /// to ask for at all (no clip, or no edge).
    ///
    /// <para>THE FOUR RULES, each with the arm that would otherwise refuse it:</para>
    /// <list type="number">
    /// <item><b>in ≥ 0</b> — a negative source position is not a position
    ///   (<c>command.rs:959-965</c>).</item>
    /// <item><b>out &gt; in</b>, by at least <see cref="MinSourceLengthUs"/> — an
    ///   inverted or empty range is refused outright (<c>command.rs:959-965</c>). A
    ///   drag past the far edge therefore produces the shortest legal clip rather
    ///   than an error the user did not ask for.</item>
    /// <item><b>out ≤ media duration</b>, when the bin knows one
    ///   (<c>command.rs:976-988</c>). A duration of 0 — a still image, or a clip whose
    ///   media is not in the bin — is UNBOUNDED, exactly as the domain's own check
    ///   is: it skips the comparison for a zero duration rather than treating it as
    ///   "zero long".</item>
    /// <item><b>the left-edge shift keeps start ≥ 0</b> (<c>command.rs:993-999</c>).
    ///   This is the rule that is invisible from the trim payload itself: a left trim
    ///   moves <c>start_us</c> by the same delta, so a clip near the timeline origin
    ///   can only be trimmed left as far as its own start allows.</item>
    /// </list>
    /// </summary>
    public static bool TryTrim(
        in ClipSource clip, TrimEdge edge, long deltaUs, out long newInUs, out long newOutUs)
    {
        newInUs = clip.InUs;
        newOutUs = clip.OutUs;

        if (clip.IsEmpty || edge == TrimEdge.None)
        {
            return false;
        }

        if (edge == TrimEdge.Left)
        {
            var candidate = SaturatingAdd(clip.InUs, deltaUs);

            // Rule 4 first, because it is the tightest lower bound near the origin:
            // start + (candidate - in) >= 0  <=>  candidate >= in - start.
            var floor = SaturatingSubtract(clip.InUs, clip.StartUs);
            if (floor < 0)
            {
                floor = 0;
            }

            if (candidate < floor)
            {
                candidate = floor;
            }

            newInUs = candidate;
        }
        else
        {
            newOutUs = SaturatingAdd(clip.OutUs, deltaUs);
        }

        ClampSourceRange(ref newInUs, ref newOutUs, clip.MediaDurationUs);
        return true;
    }

    /// <summary>The trim gesture as wire bytes, or <see langword="null"/> when there
    /// is nothing to send.</summary>
    public static string? Trim(in ClipSource clip, TrimEdge edge, long deltaUs) =>
        TryTrim(clip, edge, deltaUs, out var newInUs, out var newOutUs)
            ? TrimClip(clip.ClipId, newInUs, newOutUs)
            : null;

    /// <summary>
    /// The split gesture as wire bytes, or <see langword="null"/> when the position is
    /// not STRICTLY inside the clip's timeline occupancy.
    ///
    /// <para><c>command.rs:1055-1067</c> refuses an edge split — it would create a
    /// zero-length clip. Refusing it HERE rather than round-tripping is the better
    /// answer for the same reason the disabled tool tiles are: a control (or a
    /// gesture) that can only fail should not appear to be offering something.</para>
    /// </summary>
    public static string? Split(in ClipSource clip, long atPositionUs)
    {
        if (clip.IsEmpty || atPositionUs <= clip.StartUs || atPositionUs >= clip.EndUs)
        {
            return null;
        }

        return SplitClip(clip.ClipId, atPositionUs);
    }

    // ========================================================================
    // Plumbing
    // ========================================================================

    /// <summary>The ONE place the source-range rules are applied — see
    /// <see cref="TryTrim"/>'s remarks for each rule and its citation.</summary>
    private static void ClampSourceRange(ref long inUs, ref long outUs, long mediaDurationUs)
    {
        if (inUs < 0)
        {
            inUs = 0;
        }

        // A duration of 0 means "the bin does not bound this", matching the domain's
        // own `media_duration > 0 &&` guard.
        if (mediaDurationUs > 0)
        {
            if (outUs > mediaDurationUs)
            {
                outUs = mediaDurationUs;
            }

            var maxIn = mediaDurationUs - MinSourceLengthUs;
            if (inUs > maxIn)
            {
                inUs = maxIn < 0 ? 0 : maxIn;
            }
        }

        if (outUs < SaturatingAdd(inUs, MinSourceLengthUs))
        {
            // The far edge was dragged past the near one. Prefer moving the edge the
            // gesture did NOT grab as little as possible: pull `in` back when `out`
            // cannot legally rise, otherwise push `out` up.
            if (outUs > MinSourceLengthUs)
            {
                inUs = outUs - MinSourceLengthUs;
            }
            else
            {
                outUs = SaturatingAdd(inUs, MinSourceLengthUs);
            }
        }
    }

    private static long NonNegative(long value) => value < 0 ? 0 : value;

    private static long SaturatingAdd(long value, long amount)
    {
        var result = unchecked(value + amount);
        if (amount > 0 && result < value)
        {
            return long.MaxValue;
        }

        if (amount < 0 && result > value)
        {
            return long.MinValue;
        }

        return result;
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

    /// <summary>Open <c>{"cmd":{"type":"&lt;type&gt;","data":{</c> into the reused
    /// writer. The type is always a literal from this file, never caller data.</summary>
    private static Utf8JsonWriter Begin(string type)
    {
        var buffer = _buffer ??= new ArrayBufferWriter<byte>(256);
        buffer.ResetWrittenCount();

        var writer = _writer ??= new Utf8JsonWriter(buffer, new JsonWriterOptions { Indented = false });
        writer.Reset(buffer);

        writer.WriteStartObject();
        writer.WritePropertyName("cmd");
        writer.WriteStartObject();
        writer.WriteString("type", type);
        writer.WritePropertyName("data");
        writer.WriteStartObject();
        return writer;
    }

    /// <summary>Close the three objects and take the bytes as a string.</summary>
    private static string End(Utf8JsonWriter writer)
    {
        writer.WriteEndObject();
        writer.WriteEndObject();
        writer.WriteEndObject();
        writer.Flush();
        return Encoding.UTF8.GetString(_buffer!.WrittenSpan);
    }
}
