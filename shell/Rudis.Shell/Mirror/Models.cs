using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;

namespace Rudis.Shell.Mirror;

/// <summary>
/// Node → typed-projection deserialization, in one place.
///
/// <para>Plain <see cref="JsonSerializer"/>, not a source-generated context: 50-03
/// recorded that choice and its reason (RESEARCH §4 left source-gen's .NET 9
/// analyzer requirements UNVERIFIED) and nothing here runs per-tick — the HOT path
/// is a scalar that never touches JSON, and this runs at the ~100ms cold cadence.</para>
///
/// <para>Returns null instead of throwing on a shape mismatch: every caller must
/// treat a malformed payload as a contract violation to log and reconcile, never as
/// a crash (T-50-19).</para>
/// </summary>
internal static class WireJson
{
    public static T? FromNode<T>(JsonNode? node)
        where T : class
    {
        if (node is null)
        {
            return null;
        }
        try
        {
            return JsonSerializer.Deserialize<T>(node.ToJsonString());
        }
        catch (JsonException)
        {
            return null;
        }
    }
}

// ---------------------------------------------------------------------------
// The wire shapes the C# mirror consumes, transcribed from the ONLY two
// authoritative sources: the Rust producers (crates/core/src/model.rs,
// crates/core/src/command.rs, crates/ffi/src/ring.rs) and the SHIPPING
// TypeScript mirror that already consumes them (frontend/src/main.ts:44-218).
// This is a PORT of a proven consumer, not a redesign.
//
// ⚠ FIDELITY NOTE (load-bearing — read before adding a field).
// The TypeScript interfaces in main.ts are STRUCTURAL and compile-time only:
// at runtime the frontend's mirror IS the parsed JSON object, so every field
// the interface does not name still rides along untouched. `Clip` is the proof —
// main.ts declares 7 fields while `rudis_core::Clip` has 13 (transform, opacity,
// crop, keyframes, text, alpha_mode). A naive transcription into C# NOMINAL
// records would therefore make the C# mirror LOSSY exactly where the TS one is
// not, because System.Text.Json drops unknown members on deserialize.
//
// So the mirror keeps TWO representations, with ONE source of truth:
//   * AUTHORITATIVE: the raw `JsonObject` snapshot inside ShellMirror. Lossless
//     by construction, never round-tripped through a C# numeric type, and
//     therefore the ONLY thing state-equality assertions compare (a re-serialized
//     `float`/`double` can differ in text from serde's output; the raw node
//     cannot).
//   * PROJECTION: the records below, derived FROM the raw node for regions to
//     read. Lossy on purpose, and never written back.
// ---------------------------------------------------------------------------

// ── the event ring's poll envelope (crates/ffi/src/ring.rs:92-104) ──────────

/// One retained ring record: `{ seq, event, payload }` (ring.rs:81-88). The
/// payload stays a `JsonNode` because each of the six event names carries a
/// DIFFERENT shape (a flattened Patch, a Playback, a bare f64, …).
internal sealed class EventRecord
{
    [JsonPropertyName("seq")]
    public ulong Seq { get; init; }

    /// <summary>One of <see cref="EventNames.All"/>. Never trusted blindly:
    /// an unrecognised name is logged and SEQUENCED, never executed (T-50-19).</summary>
    [JsonPropertyName("event")]
    public string Event { get; init; } = string.Empty;

    [JsonPropertyName("payload")]
    public JsonNode? Payload { get; init; }
}

/// What one non-blocking <c>rudis_poll_events</c> observed (ring.rs:92-104).
internal sealed class PollOutcome
{
    /// <summary><c>true</c> iff records this caller had not seen were already
    /// evicted — <see cref="Events"/> is then EMPTY and the caller must
    /// full-resync (ring.rs:32-39, D-10 / T-47-12). Never a truncated list.</summary>
    [JsonPropertyName("resync_required")]
    public bool ResyncRequired { get; init; }

    /// <summary>The newest ASSIGNED ring seq — what the caller stores as its new
    /// <c>local_seq</c>. <c>0</c> when nothing has ever been pushed.</summary>
    [JsonPropertyName("next_seq")]
    public ulong NextSeq { get; init; }

    [JsonPropertyName("events")]
    public List<EventRecord> Events { get; init; } = [];
}

/// The closed set of six event tags (<c>ring.rs:69-76</c>, D-02).
///
/// ⚠ There is NO <c>canvas-viewport</c> event: <c>ring.rs:388</c> asserts its
/// absence because Phase 51 deletes the machinery that produced it. Do not add
/// one here — a grep for it over <c>shell/</c> must return zero hits.
internal static class EventNames
{
    public const string ProjectChanged = "project:changed";
    public const string PlaybackChanged = "playback:changed";
    public const string ExportProgress = "export:progress";

    /// <summary>Phase 51's. Sequenced-but-unconsumed here (UI-SPEC §4).</summary>
    public const string CanvasPointer = "canvas-pointer";

    /// <summary>Phase 54's (Chat). Sequenced-but-unconsumed here (UI-SPEC §4).</summary>
    public const string GenJob = "gen:job";

    /// <summary>Phase 54's (Chat). Sequenced-but-unconsumed here (UI-SPEC §4).</summary>
    public const string GenProgress = "gen:progress";

    public static readonly string[] All =
    [
        ProjectChanged, PlaybackChanged, ExportProgress, CanvasPointer, GenJob, GenProgress,
    ];
}

// ── the project:changed payload ─────────────────────────────────────────────

/// The <c>project:changed</c> wire payload.
///
/// ⚠ The <c>Patch</c> is FLATTENED into this envelope, NOT nested under a
/// <c>"patch"</c> key — <c>base_seq</c>/<c>seq</c> are SIBLINGS of
/// <c>kind</c>/<c>ids</c>/<c>entities</c>. Producer:
/// <c>crates/ffi/src/ctx.rs:61-66</c> (<c>#[serde(flatten)]</c>); consumer
/// parity: <c>frontend/src/main.ts:66-73</c>, whose warning is quoted here
/// because it cost Phase 43 (43-02) a silent parse failure: both
/// <c>native_surface.rs</c> listeners parse the payload as a bare
/// <c>rudis_core::Patch</c> behind <c>let Ok(..) else { return }</c>, so a
/// nested shape is swallowed with NO compile error and NO log. It is pinned
/// backend-side by a test asserting the payload has no <c>patch</c> key.
/// <b>Do not "restore" the nesting.</b>
internal sealed class ProjectChangedEnvelope
{
    /// <summary><c>rudis_core::PatchKind</c>, snake_case
    /// (<c>command.rs:2112-2114</c>) — e.g. <c>clip_moved</c>, <c>track_added</c>.</summary>
    [JsonPropertyName("kind")]
    public string Kind { get; init; } = string.Empty;

    [JsonPropertyName("ids")]
    public List<string> Ids { get; init; } = [];

    /// <summary>Post-mutation entity values for the ids named above (LAT-02).
    /// ABSENT — not null — for every kind that does not carry them
    /// (<c>skip_serializing_if</c>, command.rs:2086-2088). Each element is an
    /// internally-tagged <c>EntitySnapshot</c>:
    /// <c>{"type": "clip"|"media_bin_item", ...entity fields}</c>
    /// (command.rs:2055-2060), so the tag is a SIBLING of the entity's own
    /// fields, not a wrapper. Kept as raw <c>JsonObject</c> for the same
    /// lossless reason the snapshot is (see the fidelity note above).</summary>
    [JsonPropertyName("entities")]
    public List<JsonObject>? Entities { get; init; }

    /// <summary>The store seq this patch expects the consumer to already hold.
    /// A mismatch means a patch was missed — the ONLY thing that buys a full
    /// resync during editing (D-09).</summary>
    [JsonPropertyName("base_seq")]
    public ulong BaseSeq { get; init; }

    /// <summary>The store seq AFTER this patch.</summary>
    [JsonPropertyName("seq")]
    public ulong Seq { get; init; }
}

/// The internally-tagged <c>EntitySnapshot</c> discriminators
/// (<c>command.rs:2056-2060</c>, snake_case).
internal static class EntityTypes
{
    public const string Clip = "clip";
    public const string MediaBinItem = "media_bin_item";
}

// ── typed PROJECTIONS over the raw mirror (never written back) ──────────────

/// <summary>Transport/preview state (<c>model.rs:1217-1229</c>). Six scalars —
/// small enough that the Transport region binds this record directly.</summary>
internal sealed record Playback
{
    [JsonPropertyName("loaded_media_id")]
    public string? LoadedMediaId { get; init; }

    [JsonPropertyName("playing")]
    public bool Playing { get; init; }

    [JsonPropertyName("position_us")]
    public long PositionUs { get; init; }

    [JsonPropertyName("duration_us")]
    public long DurationUs { get; init; }

    [JsonPropertyName("fps")]
    public double Fps { get; init; }

    [JsonPropertyName("looping")]
    public bool Looping { get; init; }
}

/// <summary>A timeline clip (<c>model.rs:1087-1154</c>). The seven fields
/// main.ts:79-90 names; the other six (transform / opacity / crop / keyframes /
/// text / alpha_mode) live on the AUTHORITATIVE raw node and are read from
/// there by whichever later phase needs them — see the fidelity note.</summary>
internal sealed record Clip
{
    [JsonPropertyName("id")]
    public string Id { get; init; } = string.Empty;

    [JsonPropertyName("media_id")]
    public string MediaId { get; init; } = string.Empty;

    [JsonPropertyName("start_us")]
    public long StartUs { get; init; }

    [JsonPropertyName("in_us")]
    public long InUs { get; init; }

    [JsonPropertyName("out_us")]
    public long OutUs { get; init; }

    [JsonPropertyName("volume")]
    public double Volume { get; init; } = 1.0;

    [JsonPropertyName("audio_detached")]
    public bool AudioDetached { get; init; }
}

/// <summary>A timeline lane (<c>model.rs:399-402</c>); <c>kind</c> is
/// <c>"video"</c> or <c>"audio"</c> (snake_case <c>TrackKind</c>).</summary>
internal sealed record Track
{
    [JsonPropertyName("kind")]
    public string Kind { get; init; } = string.Empty;

    [JsonPropertyName("clips")]
    public List<Clip> Clips { get; init; } = [];
}

internal sealed record Timeline
{
    [JsonPropertyName("tracks")]
    public List<Track> Tracks { get; init; } = [];
}

/// <summary>A media file registered in the bin (<c>model.rs:1331-1377</c>).</summary>
internal sealed record MediaBinItem
{
    [JsonPropertyName("id")]
    public string Id { get; init; } = string.Empty;

    [JsonPropertyName("path")]
    public string Path { get; init; } = string.Empty;

    /// <summary><c>"video"</c> | <c>"audio"</c> | <c>"image"</c>.</summary>
    [JsonPropertyName("media_kind")]
    public string MediaKind { get; init; } = string.Empty;

    [JsonPropertyName("duration_us")]
    public long DurationUs { get; init; }

    [JsonPropertyName("width")]
    public uint Width { get; init; }

    [JsonPropertyName("height")]
    public uint Height { get; init; }

    [JsonPropertyName("fps")]
    public double Fps { get; init; }

    [JsonPropertyName("is_vfr")]
    public bool IsVfr { get; init; }

    [JsonPropertyName("rotation_degrees")]
    public uint RotationDegrees { get; init; }

    [JsonPropertyName("has_audio")]
    public bool HasAudio { get; init; }

    [JsonPropertyName("poster_path")]
    public string? PosterPath { get; init; }

    /// <summary>Virtual library folder; <c>""</c> = root (LIB-01).</summary>
    [JsonPropertyName("folder")]
    public string Folder { get; init; } = string.Empty;

    /// <summary>Display-name override; <c>null</c> = derive from the basename.</summary>
    [JsonPropertyName("display_name")]
    public string? DisplayName { get; init; }
}

/// <summary>
/// The backend-owned project, PROJECTED for reading (<c>model.rs:18-82</c>).
/// The regions bind to this; nothing ever writes it back. Fields the three
/// Phase-50 regions plus Phases 51-53 need are named; <c>canvas</c>, <c>fps</c>,
/// <c>width</c> and <c>height</c> are deliberately NOT projected yet — they are
/// present and lossless on the raw node, and the phase that needs typed access
/// declares them then (Phase 51 for canvas). Under-projecting is free; a wrong
/// projection is not.
/// </summary>
internal sealed record Project
{
    [JsonPropertyName("media_bin")]
    public List<MediaBinItem> MediaBin { get; init; } = [];

    [JsonPropertyName("timeline")]
    public Timeline Timeline { get; init; } = new();

    /// <summary>PROGRAM (timeline) playback — the Timeline playhead tracks this.</summary>
    [JsonPropertyName("playback")]
    public Playback Playback { get; init; } = new();

    /// <summary>SOURCE (MediaBin clip) playback — independent playhead.</summary>
    [JsonPropertyName("source_playback")]
    public Playback SourcePlayback { get; init; } = new();

    /// <summary><c>"program"</c> | <c>"source"</c> — which monitor is active.</summary>
    [JsonPropertyName("preview_mode")]
    public string PreviewMode { get; init; } = "program";

    [JsonPropertyName("media_folders")]
    public List<string> MediaFolders { get; init; } = [];

    /// <summary>The project's display name; <c>""</c> when never named
    /// (LIB-02) — the TitleBar's "Rudis — No project" case.</summary>
    [JsonPropertyName("name")]
    public string Name { get; init; } = string.Empty;
}
