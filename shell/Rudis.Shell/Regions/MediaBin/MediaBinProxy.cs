using System.Text.Json;
using Rudis.Shell.Interop;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE MEDIABIN'S PROXY-PROGRESS STATE — plan 63-04, TRUST-03 / PROXY-02.
// ============================================================================
//
// `MediaBinOffline`'s twin, and deliberately its twin: a per-MEDIA-ITEM fact the
// engine already knows, projected onto the tile it is about (63-CONTEXT D-09).
//
// The fact here is `rudis_get_proxy_status` — a real, tested ABI export since
// Phase 58 that, until this plan, had NO `[LibraryImport]` declaration anywhere
// in `shell/`. So a beginner dropped a folder of 4K footage in, an ffmpeg child
// burned CPU for a minute, and the app looked idle. v8's closing audit un-ticked
// `PROXY-02` for exactly that.
//
// ---------------------------------------------------------------------------
// WHY THE WORD "PROXY" NEVER APPEARS ON SCREEN
// ---------------------------------------------------------------------------
//
// `import.rs`'s D-07 says it in so many words: the proxy is "AUTOMATIC and
// PREDICATE-GATED. There is no prompt, no codec question, no quality ladder and
// no place for the word 'proxy' to reach a beginner." That decision is the
// engine's, it is recorded at the spawn site, and the badge does not get to
// overturn it because the badge is convenient. So the copy below says what is
// HAPPENING FOR THE USER — this clip is being got ready to play smoothly — and
// the jargon stays in the log where it belongs.
//
// ---------------------------------------------------------------------------
// WHY THIS IS A SEPARATE FILE IN THE PURE DIRECTORY
// ---------------------------------------------------------------------------
//
// Identical to `MediaBinOffline`'s reason: `Rudis.Shell.Tests` does NOT
// project-reference the WinExe app — it LINKS source files, and
// `Regions/MediaBin/**/*.cs` is one of the globs, while `MediaBin.xaml.cs` is
// never compiled into the test host at all. Every string, every state predicate
// and the poll's fan-out decision therefore live HERE, where they are asserted
// directly, rather than inside the region where no test could read them.
//
// WinUI-free by rule, like everything else in this directory (the gate is
// `MediaBinPurityGateTests`): `System.Text.Json` and the interop envelope type,
// nothing more.
//
// ---------------------------------------------------------------------------
// ⚠ THE FAILURE DIRECTION, AND WHY IT IS "SHOW NOTHING" (T-63-14)
// ---------------------------------------------------------------------------
//
// This projection sits on a trust boundary: an engine-authored state string
// decides whether a tile claims work is happening. Exactly one direction is
// acceptable.
//
//   * THROW — no. The caller is a poll continuation on the UI thread; an
//     exception there is a fault on the dispatcher over a decoration.
//   * Show progress for a shape it could not read — the dangerous one. A badge
//     saying "Preparing…" forever, for a job that failed or never existed, is a
//     FAKE progress claim: the user waits for something that will never finish.
//   * Show NOTHING — correct, and it is exactly the state Rudis was in before
//     this feature existed. A refusal, a transport fault, `{"Ok":null}` and any
//     state string this file does not recognise all render an empty tile.
//
// So `StateFrom` answers `null` for "I could not read that" — which the poll
// treats as "leave the previous answer alone", never as "clear it" and never as
// "invent one" — and `DisplayStateFor` answers `""` for everything except the
// two states that genuinely mean work is in flight.

/// <summary>
/// The MediaBin's proxy-progress projection and the copy that goes with it — the
/// pure half of TRUST-03's per-item indicator.
/// </summary>
internal static class MediaBinProxy
{
    /// <summary>"Nothing is known yet" — the value the region starts at and the value a
    /// project switch degrades to. Ordinal for the same reason
    /// <see cref="MediaBinOffline.None"/> is: media ids are opaque backend strings, and
    /// folding them would let one id answer for a DIFFERENT item's tile.</summary>
    internal static readonly IReadOnlyDictionary<string, string> None =
        new Dictionary<string, string>(StringComparer.Ordinal);

    // ── the engine's state vocabulary (crates/app-core/src/proxy_job.rs) ─────

    /// <summary>A job exists and is waiting for the single generation permit
    /// (<c>MAX_CONCURRENT_PROXY_JOBS = 1</c>).</summary>
    internal const string Queued = "queued";

    /// <summary>A job holds the permit and an encoder is running right now.</summary>
    internal const string Running = "running";

    /// <summary>
    /// The state the tile RENDERS, which is a narrowing of the state the engine reports:
    /// <see cref="Queued"/>, <see cref="Running"/>, or <c>""</c> for everything else.
    ///
    /// <para><b>Why the tile stores the narrowed value.</b> It is what
    /// <c>MediaBinRender.SameTile</c> compares, and the guard's job is to skip renders
    /// that would change no pixels. <c>ready</c> and <c>none</c> both draw nothing, so
    /// storing the raw string would repaint the whole level for a transition the user
    /// cannot see.</para>
    ///
    /// <para><b>`failed` deliberately renders NOTHING.</b> A failed proxy is not a failed
    /// import and not a broken clip: playback silently falls back to the original source
    /// (<c>resolve_decode_source</c>'s total fallback), so nothing the user can see is
    /// worse and there is no action for them to take. Telling a beginner that something
    /// they never asked for has failed would be alarming, unactionable, and — since the
    /// handoff names no warning token (plan 54-03) — undrawable without inventing hex.
    /// It stays in the log.</para>
    /// </summary>
    internal static string DisplayStateFor(string? engineState) => engineState switch
    {
        Queued => Queued,
        Running => Running,
        _ => "",
    };

    /// <summary>Is there still an answer worth re-asking for? Only the two in-flight
    /// states; everything else — including a state string this build does not know — is
    /// TERMINAL and is never polled again until the library changes. That is the whole of
    /// T-63-13: a settled bin costs ZERO status calls per tick, however many items it
    /// holds.</summary>
    internal static bool IsInFlight(string? engineState)
        => engineState is Queued or Running;

    // ── the copy ─────────────────────────────────────────────────────────────

    /// <summary>What the TILE shows while a job waits its turn. Short because the badge
    /// is a corner mark on a 140px tile and must stay subordinate to the offline
    /// headline.</summary>
    internal const string BadgeTextQueued = "Queued";

    /// <summary>What the TILE shows while an encoder is actually running. U+2026, ONE
    /// character, for the same reason <c>MediaBinTileFactory</c>'s ellipsis is.</summary>
    internal const string BadgeTextRunning = "Preparing…";

    /// <summary>
    /// What the TOOLTIP and the UIA name say — the same fact with room to explain it,
    /// so the two-word badge is never the only account a user gets.
    ///
    /// <para>Each state's sentence is DIFFERENT on purpose, and not only for readability:
    /// the UIA name IS the value plan 63-04's gate reads, and D-11 requires that value to
    /// be seen CHANGING. Two states that announced the same string would be
    /// indistinguishable to a screen reader and to the proof alike.</para>
    ///
    /// <para>It promises only what is true. There is no cancel and no ETA — the engine
    /// publishes neither — so the sentence says what is happening and that the user need
    /// not wait for it. Since plan 71-03 the engine DOES publish a running percent; the
    /// two-argument overload below carries it, and this one is its "no number" case.</para>
    /// </summary>
    internal static string AnnouncementFor(string? engineState) => AnnouncementFor(engineState, -1);

    /// <summary>
    /// Plan 71-03 (TRUST-03). The same sentences, with the percent folded into the
    /// RUNNING one when there is a number to fold in (<paramref name="percent"/> &gt;= 0).
    ///
    /// <para>The <c>"Preparing"</c> prefix is kept on purpose: 63-04's UIA gate asserts it
    /// and a screen reader user hears the same opening word before and after the number
    /// arrives. <see cref="Queued"/> NEVER carries a percent — a queued job has not
    /// started, whatever number a caller hands in.</para>
    /// </summary>
    internal static string AnnouncementFor(string? engineState, int percent) => DisplayStateFor(engineState) switch
    {
        Running when percent >= 0 =>
            $"Preparing {Math.Min(percent, 99)}% — Rudis is getting this clip ready to play smoothly. "
            + "It is working on it now, and you can keep working.",
        Queued =>
            "Queued — Rudis will get this clip ready to play smoothly. It is waiting its "
            + "turn, and you can keep working.",
        Running =>
            "Preparing — Rudis is getting this clip ready to play smoothly. It is working "
            + "on it now, and you can keep working.",
        _ => "",
    };

    /// <summary>The badge's own text, or <c>""</c> when the tile shows no badge.</summary>
    internal static string BadgeTextFor(string? engineState) => BadgeTextFor(engineState, -1);

    /// <summary>Plan 71-03. The badge text with the running percent when there is one
    /// (<c>"Preparing 42%"</c>); queued and the terminal states are unchanged.</summary>
    internal static string BadgeTextFor(string? engineState, int percent) => DisplayStateFor(engineState) switch
    {
        Running when percent >= 0 => $"Preparing {Math.Min(percent, 99)}%",
        Queued => BadgeTextQueued,
        Running => BadgeTextRunning,
        _ => "",
    };

    /// <summary>
    /// The badge's UIA id, so a gate can ask "is THIS clip being prepared?" rather than
    /// only "is something on screen".
    ///
    /// <para>A distinct prefix from the tile's own <c>MediaBin.Tile.{id}</c> and from
    /// <c>MediaBin.Offline.{id}</c>, because three different observables about the same
    /// item must not answer to the same name.</para>
    /// </summary>
    internal static string BadgeAutomationId(string mediaId) => "MediaBin.ProxyBadge." + mediaId;

    /// <summary>
    /// Plan 71-03. The determinate progress bar's UIA id — a FOURTH observable about the
    /// same item, so a fourth namespace: <c>MediaBin.Tile.</c>, <c>MediaBin.Offline.</c>,
    /// <c>MediaBin.ProxyBadge.</c> and this must never answer to one another's name. The
    /// bar is a determinate <c>ProgressBar</c>, so UIA reads its value as a NUMBER through
    /// <c>RangeValue</c> rather than scraping one out of a sentence.
    /// </summary>
    internal static string ProgressAutomationId(string mediaId) => "MediaBin.ProxyProgress." + mediaId;

    // ── the number (plan 71-03, TRUST-03) ────────────────────────────────────

    /// <summary>
    /// A <c>rudis_get_proxy_status</c> result → a WHOLE display percent 0..99, or
    /// <c>-1</c> for <b>"no number"</b>.
    ///
    /// <para>71-02's wire: <c>{"Ok":{"state":"running","progress_permille":420}}</c>, the
    /// field present ONLY while running, 0..=999, monotonic at the source. Anything else —
    /// a missing field, a non-integer, a queued/terminal row carrying one anyway, a miss,
    /// a refusal, a fault — is <c>-1</c>, which draws no bar (T-71-13, and the T-63-14
    /// "show nothing" direction this file already takes). -1 is NEVER 0: a bar at 0% for
    /// a job with no report is a fake claim.</para>
    ///
    /// <para>99 is the ceiling while running; completion is not a number, it is the bar
    /// going away when the state becomes <c>ready</c>.</para>
    /// </summary>
    internal static int ProgressPercentFrom(RudisResult<JsonElement> result)
    {
        if (result.Kind != RudisResultKind.Ok)
        {
            return -1;
        }

        var payload = result.Value;
        if (payload.ValueKind != JsonValueKind.Object
            || !payload.TryGetProperty("state", out var state)
            || state.ValueKind != JsonValueKind.String
            || !string.Equals(state.GetString(), Running, StringComparison.Ordinal)
            || !payload.TryGetProperty("progress_permille", out var permille)
            || permille.ValueKind != JsonValueKind.Number
            || !permille.TryGetInt64(out var value))
        {
            return -1;
        }

        return (int)Math.Clamp(value / 10, 0, 99);
    }

    /// <summary>
    /// ⚠ T-71-14. The DISPLAYED percent for one id never goes backwards while its job is in
    /// flight: an unreadable read (<c>-1</c>) leaves the previous value standing, and a
    /// lower one is ignored. The region resets the entry whenever the state is not
    /// <see cref="Running"/> (see <see cref="DisplayPercentFor"/>), so a later rebuild of
    /// the same item starts from its own first report.
    /// </summary>
    internal static int MonotonicPercent(int previous, int next)
        => next < 0 ? previous : Math.Max(previous, next);

    /// <summary>
    /// 71-REVIEW WR-04. The percent to DISPLAY for one id after a status read. Only
    /// <see cref="Running"/> is monotonic, and only across consecutive running reads: any
    /// other state (including <see cref="Queued"/>) answers <c>-1</c>, and a running read
    /// that follows a non-running one starts from its own report. A job cancelled at 60 %
    /// and re-armed for the same id therefore cannot carry 60 % into the new job once a
    /// poll sees it queued.
    /// </summary>
    internal static int DisplayPercentFor(string? previousState, int previousPct, string? state, int pct)
        => string.Equals(state, Running, StringComparison.Ordinal)
            ? MonotonicPercent(
                string.Equals(previousState, Running, StringComparison.Ordinal) ? previousPct : -1,
                pct)
            : -1;

    // ── the wire ─────────────────────────────────────────────────────────────

    /// <summary>
    /// A <c>rudis_get_proxy_status</c> result → the engine's state string, or
    /// <see langword="null"/> for <b>"that could not be read"</b>.
    ///
    /// <para>The happy payload is <c>{"Ok":{"state":".."}}</c>. <c>{"Ok":null}</c> — a
    /// cache miss or an unknown id — is a real, readable answer meaning "no job, no
    /// proxy", and comes back as <c>"none"</c>. Everything else (a domain refusal, a
    /// transport fault, an object with no <c>state</c>, a <c>state</c> that is not a
    /// string) answers <see langword="null"/>, and the caller leaves the previous answer
    /// standing rather than inventing or clearing one.</para>
    ///
    /// <para>The distinction is load-bearing rather than tidy: <c>null</c> must NOT latch
    /// an item as terminal. An engine that answers unreadably for one tick has to be
    /// re-asked on the next, or a badge would vanish permanently on a single hiccup.</para>
    /// </summary>
    internal static string? StateFrom(RudisResult<JsonElement> result)
    {
        if (result.Kind != RudisResultKind.Ok)
        {
            return null;
        }

        var payload = result.Value;

        if (payload.ValueKind == JsonValueKind.Null || payload.ValueKind == JsonValueKind.Undefined)
        {
            // A MISS is an answer: nothing is queued, nothing is running, nothing is
            // cached. Same shape the engine uses for an unknown id, deliberately
            // (58-CONTEXT: distinguishing them would need a decode to tell them apart).
            return "none";
        }

        if (payload.ValueKind != JsonValueKind.Object
            || !payload.TryGetProperty("state", out var state)
            || state.ValueKind != JsonValueKind.String)
        {
            return null;
        }

        var text = state.GetString();
        return string.IsNullOrEmpty(text) ? null : text;
    }

    /// <summary>
    /// Which media ids one poll pass should ask about: the ones nothing is known about
    /// yet, plus the ones whose last known state was still IN FLIGHT.
    ///
    /// <para><b>This function is T-63-13.</b> The status export is a pure read, but it is
    /// a read PER ITEM on a 100 ms cadence, and a 200-item bin polled indiscriminately
    /// would issue 2,000 ABI calls a second on the interop worker for a decoration. So a
    /// settled bin returns an EMPTY list — no call, no allocation beyond this list, no
    /// worker traffic — and the only items that cost anything are the handful genuinely
    /// being worked on.</para>
    ///
    /// <para>Order follows <paramref name="mediaIds"/>, which is the bin's own order, so
    /// the first item a user imported is the first one asked about.</para>
    /// </summary>
    internal static List<string> IdsToPoll(
        IEnumerable<string> mediaIds,
        IReadOnlyDictionary<string, string> known)
    {
        var due = new List<string>();

        foreach (var id in mediaIds)
        {
            if (string.IsNullOrEmpty(id))
            {
                continue;
            }

            if (!known.TryGetValue(id, out var state))
            {
                due.Add(id);
                continue;
            }

            if (IsInFlight(state))
            {
                due.Add(id);
            }
        }

        return due;
    }

    /// <summary>
    /// Does the known-state map describe a library that is no longer the one on screen?
    ///
    /// <para>A media id is minted per project, so opening a different project produces a
    /// different id set and every id is simply unknown — which needs no help. The case
    /// that DOES need help is the same project re-opened, or an item deleted: an id the
    /// map remembers as terminal that the bin no longer carries means the map is
    /// describing a previous library, and re-arming (<c>proxy_job::rearm_project_media</c>
    /// runs at every project open) may have made every remembered answer stale.</para>
    ///
    /// <para>The answer is deliberately coarse — drop the WHOLE map — because the cost is
    /// one extra poll pass and the alternative is a tile that never shows a badge again
    /// for work that really is running.</para>
    /// </summary>
    internal static bool IsStaleFor(
        IReadOnlyDictionary<string, string> known,
        IReadOnlySet<string> currentIds)
    {
        foreach (var id in known.Keys)
        {
            if (!currentIds.Contains(id))
            {
                return true;
            }
        }

        return false;
    }
}
