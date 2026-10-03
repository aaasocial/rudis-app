
using System.Text.Json;
using Rudis.Shell.Interop;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE TRANSPORT BANNER'S RENDER-CACHE LINE — plan 63-04, TRUST-03.
// ============================================================================
//
// `MediaBinProxy`'s program-level counterpart. Proxy status is a property of one
// MEDIA ITEM and belongs on its tile; the render cache is a property of the
// PROGRAM, so it belongs on `Transport.StatusBanner` — an AutomationId that has
// existed since Phase 50 (63-CONTEXT D-09).
//
// `rudis_get_render_cache_status` has been a real, tested ABI export since Phase
// 59, and its own Rust doc says it is poll-only BY DECISION, waiting for "a shell
// region to actually consume it". Nothing ever did. So a heavy timeline baked
// segments for a minute and the app said nothing at all.
//
// WinUI-free by rule, and in `Regions/` beside `ToolbarProjectRoutes` for the
// same reason that file is: `Rudis.Shell.Tests` LINKS shell sources rather than
// project-referencing the WinExe, and `Transport.xaml.cs` is never compiled into
// the test host. A decision written there could not be asserted at all.
//
// ---------------------------------------------------------------------------
// ⚠ WHICH NUMBER THE LINE CARRIES, AND WHY ONLY THAT ONE
// ---------------------------------------------------------------------------
//
// (Plan 71-03, TRUST-03. Until then this section was "WHY THE LINE CARRIES NO
// NUMBER AT ALL"; the two rejections below are kept VERBATIM from it, because
// they are still true and still measured.)
//
// The payload has three counts and NONE of them can honestly be shown to a user
// as progress on THIS timeline. Both rejections are recorded because both are
// non-obvious and the second was found by MEASUREMENT, after the first draft of
// this file had already shipped a number to screen.
//
// `heavy_segments` — a countdown, "3 done, 5 to go" — is the obvious copy and is a
// LIE. It is the detector's candidate count and a heat mark is STICKY by design.
// `note_live_tick`'s own doc: "once a segment has earned SEGMENT_MISS_STREAK
// consecutive misses, later in-budget ticks reset the run but never clear the
// mark." `prearm_stack`'s structural marks never clear either. The candidate count
// does NOT fall as work completes, so a "to go" built from it would sit at 5
// forever while five segments finished one by one.
//
// `cached_segments` looked safe — it only grows, once per finished segment — and it
// is not, for a reason its own Rust doc states plainly: it "counts DISTINCT segment
// indices with a committed meta on disk — a census of what has been rendered, NOT a
// count of what would be served right now." The directory is per-INSTALL, not
// per-project, and a segment INDEX is a coordinate on a global program-time grid,
// so indices baked for a completely different timeline are counted too.
//
// MEASURED, on plan 63-04's own GREEN run (2026-08-30): a six-layer project was
// opened against a machine whose cache already held indices 0..3 from Phase 61.
// The banner read "(4 ready)" from the FIRST poll — before this timeline had
// rendered a single frame — and then stayed at 4 through four real segment renders
// (8876, 20092, 17562 and 17591 ms of h264_nvenc work). A number that is correct
// only on a machine with an empty cache is a spoofed progress claim on every other
// one, which is exactly what T-63-14 forbids.
//
// THE ACCEPTED SOURCE (plan 71-03). 71-02 added a sixth field, `committed_total`:
// a PROCESS-LIFETIME counter the engine bumps once per segment render that really
// COMMITTED, on the same code path, immediately before the `render-cache: committed
// segment` log line. On its own it is a lifetime census too (it counts whatever
// this process committed before the current project was open), so the line never
// shows it raw. The region captures a BASELINE — `committed_total` at the FIRST
// `rendering` observation of the session — and resets that baseline on every
// project switch; the line then shows `committed_total − baseline` as "N ready".
// That is D-63-04-02's "shell-side tally", made exact by the engine's own commit
// counter rather than by counting `rendering_segment` transitions this shell
// watched — which would over-count, because a render that is cancelled (a seek,
// an edit, a play) also leaves the `rendering` state without committing anything.
// Because the baseline is taken at a `rendering` observation and the first commit
// of that bake lands after it, a cold bake's first reading is 0 and the line shows
// the bare headline; the tally can never exceed the number of `committed segment`
// lines the engine logged.
//
// COUNT-UP WITHOUT DENOMINATOR. The indicator has NO denominator — "N ready", never
// "N of M" — because no honest engine total exists (71-RESEARCH, Open Questions
// (RESOLVED) item 2): `heavy_segments` is a sticky candidate census that includes
// segments already fresh, and `cached_segments` is a per-install directory census;
// both were measured to lie (D-63-04-02). A fraction is a follow-up that needs an
// engine-vouched total; it is deliberately not attempted here.
//
// ---------------------------------------------------------------------------
// SILENCE IS THE HEALTHY STATE
// ---------------------------------------------------------------------------
//
// The banner is not a status ticker. `none` (no cache directory at all) and
// `idle` (a directory, nothing rendering) show NOTHING, because there is nothing
// happening and a permanent "everything is fine" strip is noise a beginner has to
// learn to ignore. Only `rendering` — a background segment render genuinely in
// flight — earns the surface.
//
// ---------------------------------------------------------------------------
// AND IT YIELDS
// ---------------------------------------------------------------------------
//
// `Transport.StatusBanner` already belongs to errors: a domain refusal and a
// transport FAULT are drawn there, distinctly, and they are the only thing on
// screen telling a user that a command they issued did not happen. Cache progress
// is nobody's request and nobody's problem. So the arbitration below is
// asymmetric on purpose — a message ALWAYS outranks cache status, cache status
// never overwrites one, and it clears only what it put there itself.

/// <summary>Who last wrote to <c>Transport.StatusBanner</c>.</summary>
internal enum TransportBannerOwner
{
    /// <summary>Nothing is on screen.</summary>
    None,

    /// <summary>A refusal or a fault — a message about something the USER asked for.</summary>
    Message,

    /// <summary>Background cache progress — nobody asked, and it yields to everything.</summary>
    CacheStatus,
}

/// <summary>What one poll pass should do to the banner.</summary>
internal enum TransportBannerAction
{
    /// <summary>Touch nothing.</summary>
    Leave,

    /// <summary>Draw the cache line.</summary>
    Show,

    /// <summary>Take the cache line down (and only the cache line).</summary>
    Clear,
}

/// <summary>
/// The render-cache status projection and the banner's priority rule — the pure half
/// of TRUST-03's program-level indicator.
/// </summary>
internal static class TransportCacheStatus
{
    /// <summary>The engine's <c>state</c> value that means a background segment render is
    /// in flight right now.</summary>
    internal const string Rendering = "rendering";

    /// <summary>
    /// The head of every line this can produce, and the whole of the copy when nothing
    /// has finished yet.
    ///
    /// <para>Beginner language on purpose. The payload's vocabulary is "segments",
    /// "heavy", "cache" — every word of which is an implementation detail of a feature
    /// the user never asked for and cannot configure. What they can understand is that
    /// the app is doing something so their timeline will play smoothly, and that they
    /// need not wait for it.</para>
    /// </summary>
    internal const string Headline = "Preparing smooth playback";

    /// <summary>
    /// How many consecutive quiet polls the line is HELD for before it comes down.
    ///
    /// <para>At the 100 ms cold cadence this is two seconds, and it exists because the
    /// bake is a SEQUENCE of segment renders with short gaps between them: without the
    /// hold, the banner would blink on and off once per segment, which reads as a fault
    /// rather than as progress. It is a presentation decision and it is stated here rather
    /// than buried in the region, so the number can be argued with.</para>
    /// </summary>
    internal const int QuietPollsBeforeClearing = 20;

    /// <summary>
    /// The line to show, or <c>""</c> for "show nothing".
    ///
    /// <para>Every unreadable shape — a refusal, a transport fault, a payload that is not
    /// an object, a missing or non-numeric field — answers <c>""</c>. It cannot answer
    /// anything else: there is no state to fall back on and inventing progress is the one
    /// failure direction T-63-14 forbids outright.</para>
    /// </summary>
    internal static string LineFor(RudisResult<JsonElement> result) => LineFor(result, 0);

    /// <summary>
    /// Plan 71-03. The line with the SESSION tally folded in: <c>"{Headline}… N ready"</c>
    /// while rendering with <paramref name="readyThisSession"/> &gt; 0, the bare headline
    /// while rendering with nothing finished yet, and <c>""</c> for everything the
    /// one-argument overload already silences. The tally is supplied by the caller (see
    /// <see cref="ReadyThisSession"/>) — no census field on the payload is ever quoted.
    /// </summary>
    internal static string LineFor(RudisResult<JsonElement> result, long readyThisSession)
    {
        if (result.Kind != RudisResultKind.Ok)
        {
            return "";
        }

        var payload = result.Value;
        if (payload.ValueKind != JsonValueKind.Object)
        {
            return "";
        }

        if (!payload.TryGetProperty("state", out var state)
            || state.ValueKind != JsonValueKind.String
            || !string.Equals(state.GetString(), Rendering, StringComparison.Ordinal))
        {
            // `none` and `idle` are healthy and silent; so is anything this build cannot
            // recognise.
            return "";
        }

        // Only the session tally. See this file's header for the two census counts that
        // were rejected, the measurement that rejected the second, and why the tally has
        // no denominator.
        return readyThisSession > 0
            ? Headline + "… " + readyThisSession.ToString(System.Globalization.CultureInfo.InvariantCulture) + " ready"
            : Headline + "…";
    }

    /// <summary>
    /// Plan 71-03. <c>committed_total</c> — the engine's process-lifetime count of segment
    /// renders that really committed (71-02) — or <c>-1</c> for "not readable" (not Ok,
    /// not an object, missing, not a non-negative integer). Readable in EVERY state, so the
    /// region can take its baseline from whichever reading comes first.
    /// </summary>
    internal static long ReadCommittedTotal(RudisResult<JsonElement> result)
    {
        if (result.Kind != RudisResultKind.Ok || result.Value.ValueKind != JsonValueKind.Object)
        {
            return -1;
        }

        var total = ReadCount(result.Value, "committed_total");
        return total >= 0 ? total : -1;
    }

    /// <summary>
    /// Plan 71-03. How many segments this session has finished: <c>committedTotal −
    /// baseline</c>, where the baseline is <c>committed_total</c> at the first
    /// <c>rendering</c> observation since the project opened. Either side unknown → 0 (say
    /// nothing rather than guess), and never negative.
    /// </summary>
    internal static long ReadyThisSession(long baseline, long committedTotal)
        => baseline < 0 || committedTotal < 0 ? 0 : Math.Max(0, committedTotal - baseline);

    /// <summary>
    /// The longer sentence for the tooltip and, through it, for anyone who stops to ask
    /// what the strip means.
    ///
    /// <para><c>idle_spawned</c> is consumed HERE and nowhere else. Phase 61 added that
    /// field "for exactly this consumer", and it answers the one question the visible line
    /// cannot: whether this work started because the user pressed play, or because the app
    /// took the initiative while they were doing nothing. Saying so is the difference
    /// between an app that looks busy and one that explains itself.</para>
    /// </summary>
    internal static string TooltipFor(RudisResult<JsonElement> result)
    {
        const string body =
            "Rudis is getting parts of your timeline ready so they play back smoothly. "
            + "You can keep editing while it works.";

        if (result.Kind != RudisResultKind.Ok || result.Value.ValueKind != JsonValueKind.Object)
        {
            return body;
        }

        return ReadCount(result.Value, "idle_spawned") > 0
            ? body + " It started this on its own while you were not editing."
            : body;
    }

    /// <summary>
    /// ⚠ THE PRIORITY RULE, as one pure function.
    ///
    /// <para>A <see cref="TransportBannerOwner.Message"/> — a refusal or an engine fault —
    /// ALWAYS keeps the surface. It is the only thing telling a user that something they
    /// asked for did not happen, and background progress overwriting it would be a
    /// regression of UI-SPEC §5 dressed up as a feature. Cache status draws only onto an
    /// unowned banner or over its own previous line, and clears only what it put there
    /// itself — so a <c>ClearStatusBanner</c> from a successful transport command hands the
    /// surface back, and the next poll re-draws.</para>
    /// </summary>
    internal static TransportBannerAction Arbitrate(TransportBannerOwner owner, string line)
    {
        if (owner == TransportBannerOwner.Message)
        {
            return TransportBannerAction.Leave;
        }

        if (line.Length > 0)
        {
            return TransportBannerAction.Show;
        }

        return owner == TransportBannerOwner.CacheStatus
            ? TransportBannerAction.Clear
            : TransportBannerAction.Leave;
    }

    /// <summary>A non-negative count, or <c>-1</c> for "not readable". The payload's
    /// counts are <c>u64</c> on the Rust side; anything that does not arrive as a number
    /// in range is treated as absent rather than guessed at.</summary>
    private static long ReadCount(JsonElement payload, string name)
        => payload.TryGetProperty(name, out var value)
           && value.ValueKind == JsonValueKind.Number
           && value.TryGetInt64(out var count)
            ? count
            : -1;
}
