using System.Runtime.InteropServices;
using System.Text.Json;
using Rudis.Shell.Interop;

namespace Rudis.Shell.Regions;

/// <summary>
/// SHELL-09's client-side half: one <c>byte[]</c> of `u8` RMS peaks per media id,
/// fetched on Phase 50 D-06's EXISTING 100ms cold-path poll and never, ever
/// fetched from the paint path.
///
/// <para><b>This class knows nothing about the ABI, deliberately.</b> The fetch is
/// a delegate the caller supplies. Two things follow, and both are the point:
/// the whole poll policy is testable against a recording double that can answer
/// "null" a hundred times in microseconds, and the source scan in
/// <c>TimelinePaintScopeTests</c> can assert ZERO whole-word ABI references in
/// this file and mean it. A cache that named the interop wrapper would be a cache
/// one edit away from calling it.</para>
///
/// <para><b>The policy (52-CONTEXT D-21, written out in
/// <c>artifacts/52-05-waveform-abi.md</c> §4.3):</b></para>
/// <list type="number">
/// <item>An id becomes WANTED when a visible clip references it and that clip has
///   undetached audio (D-20 — video clips included).</item>
/// <item>At most <see cref="MaxRequestsPerCycle"/> requests leave per cold cycle,
///   however many ids are wanted. A 50-clip project must not turn one 100ms tick
///   into 50 ABI round trips (T-52-38).</item>
/// <item>A non-null answer is cached and the id STOPS being asked, forever. The
///   cache key on the Rust side is <c>(canonical path, mtime_ns, size_bytes)</c>,
///   so for a given media id the answer cannot change.</item>
/// <item>A null answer backs off exponentially from
///   <see cref="FirstBackoffMs"/>, capped at <see cref="MaxBackoffMs"/>.</item>
/// <item>After <see cref="GiveUpAfterConsecutiveNulls"/> consecutive nulls the id
///   is dropped and the clip simply draws with no fill. 52-05 named this rule and
///   named why it has to live HERE: at the ABI a permanently-failing source and a
///   not-ready-yet source are the same answer by design, so only the client can
///   ever stop.</item>
/// </list>
///
/// <para><b>Every cached array is PINNED for as long as it is cached</b>, and the
/// pinned address is handed to the frame builder by
/// <see cref="TryGetPinnedPeaks"/>. Pinning once per array rather than once per
/// frame is 52-06's own rule applied to a second kind of buffer: the redraw path
/// must not perform a handle-table operation per visible clip per frame. The cost
/// is bounded by construction — at most <see cref="MaxEntries"/> arrays, and the
/// large ones live on the LOH, which is not compacted anyway.</para>
///
/// <para><b>Single-threaded by contract.</b> Every member runs on the UI thread:
/// <see cref="NoteWanted"/> and the two lookups from the frame build,
/// <see cref="PumpAsync"/> from the cold cycle's UI-thread half. The ABI call
/// itself never runs there — the injected delegate hands it to the interop worker
/// and this class only awaits the answer. A pump running on the pool while the
/// render loop read the same <see cref="Dictionary{TKey,TValue}"/> would be a
/// genuine data race, and no gate in this plan would have caught it.</para>
///
/// <para>No WinUI types, by rule (D-13) — this file lives in the WinUI-free
/// <c>Regions/Timeline/</c> directory and is unit-tested with no window.</para>
/// </summary>
internal sealed class PeakCache : IDisposable
{
    /// <summary>Requests allowed to leave in ONE cold cycle. Two, not one, so a
    /// two-clip import resolves in one tick; not fifty, because that is the
    /// amplification T-52-38 is about.</summary>
    public const int MaxRequestsPerCycle = 2;

    /// <summary>Slots retained. Least-recently-DRAWN evicted first, so the ids the
    /// viewport is actually showing survive a scroll through a large bin.</summary>
    public const int MaxEntries = 256;

    /// <summary>The first retry delay after a null. Deliberately far below
    /// <see cref="MaxBackoffMs"/>: 52-05 measured the first non-null answer
    /// arriving 197.5ms after import returned, and a flat 2s back-off would leave
    /// a freshly imported clip's waveform blank for up to two seconds every
    /// time.</summary>
    public const long FirstBackoffMs = 200;

    /// <summary>The back-off ceiling — the "no more often than once per 2s per id"
    /// half of D-21's rule. Extraction of a long file legitimately takes a while
    /// (52-02 measured ~11s per hour of audio); asking faster changes
    /// nothing.</summary>
    public const long MaxBackoffMs = 2_000;

    /// <summary>Consecutive nulls before an id is dropped. With the back-off above
    /// that is roughly 3.3 minutes of asking, which comfortably outlasts a
    /// legitimate multi-hour extraction and still bounds a permanently-failing
    /// source at 100 wasted calls total.</summary>
    public const int GiveUpAfterConsecutiveNulls = 100;

    private readonly Dictionary<string, Slot> _slots = new(StringComparer.Ordinal);

    /// <summary>Reused across cycles so selecting the due ids allocates nothing.</summary>
    private readonly List<Slot> _due = new(MaxRequestsPerCycle);

    private long _drawTick;
    private long _seq;
    private bool _disposed;

    /// <summary>
    /// Where a miss or a give-up is reported. Injected rather than reached for,
    /// for the same reason the fetch is: this file must stay free of every type
    /// that lives outside the WinUI-free directory, and the shell's own diagnostic
    /// sink is a member of the WinUI <c>Application</c>. The region passes it in;
    /// a unit test passes nothing.
    /// </summary>
    private readonly Action<string>? _log;

    public PeakCache(Action<string>? log = null) => _log = log;

    /// <summary>
    /// Bumped ONCE per media id whose peaks transition from absent to present.
    /// The frame builder folds it into its dirty check, which is what makes peaks
    /// arriving cost exactly one redraw — not zero (the surface would keep drawing
    /// the pre-peak frame forever) and not one per poll.
    /// </summary>
    public long Generation { get; private set; }

    /// <summary>Ids still being polled.</summary>
    public int WantedCount
    {
        get
        {
            var n = 0;
            foreach (var slot in _slots.Values)
            {
                if (slot.Wanted)
                {
                    n++;
                }
            }

            return n;
        }
    }

    /// <summary>Slots held, resolved or not — the number <see cref="MaxEntries"/>
    /// bounds.</summary>
    public int SlotCount => _slots.Count;

    /// <summary>Ids resolved with real peak bytes.</summary>
    public int ResolvedCount
    {
        get
        {
            var n = 0;
            foreach (var slot in _slots.Values)
            {
                if (slot.Peaks is not null)
                {
                    n++;
                }
            }

            return n;
        }
    }

    /// <summary>Requests this cache has issued through the injected fetch, ever.
    /// Read by the artifact and by 52-09's introspection query; a policy whose
    /// traffic nobody can count is a policy nobody can check.</summary>
    public long RequestsIssued { get; private set; }

    /// <summary>Ids dropped by the give-up rule.</summary>
    public int GivenUpCount
    {
        get
        {
            var n = 0;
            foreach (var slot in _slots.Values)
            {
                if (slot.GivenUp)
                {
                    n++;
                }
            }

            return n;
        }
    }

    public bool IsWanted(string mediaId) =>
        mediaId is not null && _slots.TryGetValue(mediaId, out var slot) && slot.Wanted;

    // ========================================================================
    // The frame build's side — cheap, allocation-free in the steady state, and
    // structurally incapable of starting work
    // ========================================================================

    /// <summary>
    /// Note every visible audio-bearing clip's media id as WANTED.
    ///
    /// <para>Called from the frame build with the CULLED clip list, so the wanted
    /// set is the viewport's, not the project's — 52-05 §4.3 point 4. The steady
    /// state is one dictionary lookup per audio-bearing visible clip and nothing
    /// else: a slot is created only for a genuinely new id, which happens on a
    /// project change, not on a redraw.</para>
    ///
    /// <para>This starts NOTHING. It records intent; <see cref="PumpAsync"/> is the
    /// only member that can cause a fetch, and it is never reachable from a
    /// paint.</para>
    /// </summary>
    public void NoteWanted(IReadOnlyList<ClipLayout> visible)
    {
        if (visible is null)
        {
            return;
        }

        for (var i = 0; i < visible.Count; i++)
        {
            var clip = visible[i];

            // D-20, already resolved by TimelineModel.Rebuild into this one flag:
            // `has_audio && !audio_detached`. A video clip with audio IS wanted; a
            // video clip whose audio was detached is not, because its audio now
            // lives on its own audio-track clip, which is separately visible.
            if (!clip.HasAudio || string.IsNullOrEmpty(clip.MediaId))
            {
                continue;
            }

            if (_slots.TryGetValue(clip.MediaId, out var slot))
            {
                slot.LastDrawnTick = ++_drawTick;
                continue;
            }

            EvictIfFull();
            _slots[clip.MediaId] = new Slot
            {
                MediaId = clip.MediaId,
                Seq = ++_seq,
                Wanted = true,
                NextAttemptMs = long.MinValue,
                LastDrawnTick = ++_drawTick,
            };
        }
    }

    /// <summary>
    /// The render path's ONLY door into the peaks, and it CANNOT OPEN A
    /// CONNECTION.
    ///
    /// <para><b>Do not ever give this method a fetch.</b> That is Pitfall 7's
    /// forbidden shape in one line — "get this clip's waveform, and go and get it
    /// if we haven't got it" — and it will look completely correct on a warm
    /// cache. The bug only appears as stutter on a cold cache or a large item,
    /// which is exactly why criterion 5 asks for a mechanical check rather than a
    /// visual one. If a clip's peaks are not here, the correct behaviour is to
    /// draw the clip with no fill and let the cold poll do its job.</para>
    ///
    /// <para>Allocates nothing, stamps the draw tick that drives eviction, and
    /// returns <see langword="false"/> for every miss: unknown id, not yet
    /// extracted, no audio, given up.</para>
    /// </summary>
    public bool TryGetPeaks(string mediaId, out byte[] peaks, out long blockUs)
    {
        if (mediaId is not null && _slots.TryGetValue(mediaId, out var slot) && slot.Peaks is not null)
        {
            slot.LastDrawnTick = ++_drawTick;
            peaks = slot.Peaks;
            blockUs = slot.BlockUs;
            return true;
        }

        peaks = [];
        blockUs = 0;
        return false;
    }

    /// <summary>
    /// The same lookup, handing back the PINNED address the frame contract wants.
    /// Same contract as <see cref="TryGetPeaks"/> in every respect that matters:
    /// pure, allocation-free, and unable to start anything.
    ///
    /// <para>The pointer is valid for as long as the id stays cached, which is at
    /// least until the next <see cref="NoteWanted"/> can evict it — and eviction
    /// runs on the same UI thread as the frame build, never between a builder's
    /// fill and the render call that consumes it.</para>
    /// </summary>
    public bool TryGetPinnedPeaks(string mediaId, out nint ptr, out int length, out long blockUs)
    {
        if (mediaId is not null && _slots.TryGetValue(mediaId, out var slot) && slot.Peaks is not null)
        {
            slot.LastDrawnTick = ++_drawTick;
            ptr = slot.Ptr;
            length = slot.Peaks.Length;
            blockUs = slot.BlockUs;
            return true;
        }

        ptr = nint.Zero;
        length = 0;
        blockUs = 0;
        return false;
    }

    // ========================================================================
    // The cold cycle's side
    // ========================================================================

    /// <summary>
    /// One cold cycle: select at most <see cref="MaxRequestsPerCycle"/> due ids,
    /// ask for each through the injected fetch, and fold the answers in.
    ///
    /// <para><paramref name="nowMs"/> is a caller-supplied monotonic millisecond
    /// clock, so the back-off schedule is a pure function of its inputs and a test
    /// can drive ten simulated seconds without waiting ten real ones.</para>
    ///
    /// <para>Never throws. A throwing fetch, a transport fault and a corrupt
    /// payload are all the same thing here — a MISS — because this runs on a
    /// fire-and-forget cold cycle where an escaping exception would be invisible.</para>
    /// </summary>
    public async Task PumpAsync(Func<string, Task<RudisResult<JsonElement>>> fetch, long nowMs)
    {
        ArgumentNullException.ThrowIfNull(fetch);

        if (_disposed)
        {
            return;
        }

        var count = SelectDue(nowMs);
        for (var i = 0; i < count; i++)
        {
            var slot = _due[i];
            RequestsIssued++;

            RudisResult<JsonElement> result;
            try
            {
                result = await fetch(slot.MediaId);
            }
            catch (Exception e)
            {
                // A fetch that throws is a miss, recorded rather than swallowed.
                _log?.Invoke($"waveform peaks fetch faulted for {slot.MediaId}: {e.GetType().Name}: {e.Message}");
                result = RudisResult<JsonElement>.Fault(RudisStatus.PanicCaught, e.Message);
            }

            slot.InFlight = false;

            if (_disposed || !_slots.TryGetValue(slot.MediaId, out var current) || !ReferenceEquals(current, slot))
            {
                // Evicted, or the cache was torn down, while the answer was in
                // flight. Dropping it is correct: the id is no longer visible.
                continue;
            }

            Apply(slot, result, nowMs);
        }
    }

    /// <summary>Up to <see cref="MaxRequestsPerCycle"/> wanted, not-in-flight ids
    /// whose back-off has expired, oldest deadline first and insertion order as
    /// the tie-break — so a large wanted set drains fairly rather than starving
    /// whatever the dictionary happens to enumerate last.</summary>
    private int SelectDue(long nowMs)
    {
        _due.Clear();

        foreach (var slot in _slots.Values)
        {
            if (!slot.Wanted || slot.InFlight || slot.NextAttemptMs > nowMs)
            {
                continue;
            }

            var at = _due.Count;
            while (at > 0 && IsBefore(slot, _due[at - 1]))
            {
                at--;
            }

            if (at < MaxRequestsPerCycle)
            {
                _due.Insert(at, slot);
                if (_due.Count > MaxRequestsPerCycle)
                {
                    _due.RemoveAt(_due.Count - 1);
                }
            }
        }

        for (var i = 0; i < _due.Count; i++)
        {
            _due[i].InFlight = true;
        }

        return _due.Count;
    }

    private static bool IsBefore(Slot a, Slot b) =>
        a.NextAttemptMs != b.NextAttemptMs ? a.NextAttemptMs < b.NextAttemptMs : a.Seq < b.Seq;

    /// <summary>Fold one answer in: cache it, or schedule the next attempt.</summary>
    private void Apply(Slot slot, in RudisResult<JsonElement> result, long nowMs)
    {
        if (TryDecode(result, out var peaks, out var blockUs, out var sampleRate))
        {
            slot.Peaks = peaks;
            slot.BlockUs = blockUs;
            slot.SampleRate = sampleRate;
            slot.Pin = GCHandle.Alloc(peaks, GCHandleType.Pinned);
            slot.Ptr = slot.Pin.AddrOfPinnedObject();
            slot.Wanted = false;
            slot.ConsecutiveNulls = 0;
            Generation++;
            return;
        }

        slot.ConsecutiveNulls++;
        if (slot.ConsecutiveNulls >= GiveUpAfterConsecutiveNulls)
        {
            // 52-05's give-up rule. The clip draws with no fill from here on, which
            // is also the correct PERMANENT behaviour for a source whose audio
            // cannot be decoded at all.
            slot.Wanted = false;
            slot.GivenUp = true;
            _log?.Invoke(
                $"waveform peaks: giving up on {slot.MediaId} after " +
                $"{GiveUpAfterConsecutiveNulls} consecutive nulls (52-05 §4.3)");
            return;
        }

        // Exponential, capped. The shift is bounded before it is taken: shifting a
        // long by 64 or more is undefined-shaped in C# (it masks to 0..63) and
        // would silently produce a SHORTER delay than the previous attempt.
        var steps = Math.Min(slot.ConsecutiveNulls - 1, 20);
        var delay = Math.Min(MaxBackoffMs, FirstBackoffMs << steps);
        slot.NextAttemptMs = nowMs + delay;
    }

    /// <summary>
    /// The payload, exactly as 52-05 §4.2 wrote it:
    /// <c>{"block_us":10000,"sample_rate":48000,"peak_count":372,"peaks_b64":".."}</c>
    /// or <c>null</c>.
    ///
    /// <para>Every failure is a MISS, never an exception (T-52-40): a domain error,
    /// a transport fault, a null body, a non-object body, a missing field, a
    /// malformed base64 string, a decoded length that disagrees with
    /// <c>peak_count</c>, or a non-positive <c>block_us</c>. The last two are worth
    /// naming: <c>peak_count</c> is redundant with the decoded length ON PURPOSE so
    /// a mismatch is a corruption signal, and a zero <c>block_us</c> would be a
    /// divide-by-zero inside the renderer's mapping of a clip onto the array.</para>
    /// </summary>
    private static bool TryDecode(
        in RudisResult<JsonElement> result, out byte[] peaks, out long blockUs, out uint sampleRate)
    {
        peaks = [];
        blockUs = 0;
        sampleRate = 0;

        if (result.Kind != RudisResultKind.Ok)
        {
            return false;
        }

        var body = result.Value;
        if (body.ValueKind != JsonValueKind.Object)
        {
            // `{"Ok": null}` — cache miss, not yet computed, no audio, or an
            // unknown id. All four are the same answer by D-21's design.
            return false;
        }

        if (!body.TryGetProperty("peaks_b64", out var b64Element) ||
            b64Element.ValueKind != JsonValueKind.String ||
            !body.TryGetProperty("block_us", out var blockElement) ||
            !blockElement.TryGetInt64(out var block) ||
            !body.TryGetProperty("peak_count", out var countElement) ||
            !countElement.TryGetInt32(out var declaredCount))
        {
            return false;
        }

        if (block <= 0 || declaredCount < 0)
        {
            return false;
        }

        byte[] decoded;
        try
        {
            // Standard padded base64 (RFC 4648 §4), no options — 52-05 §4.2.
            decoded = Convert.FromBase64String(b64Element.GetString() ?? string.Empty);
        }
        catch (FormatException)
        {
            return false;
        }

        if (decoded.Length != declaredCount)
        {
            return false;
        }

        var rate = 0u;
        if (body.TryGetProperty("sample_rate", out var rateElement))
        {
            rateElement.TryGetUInt32(out rate);
        }

        peaks = decoded;
        blockUs = block;
        sampleRate = rate;
        return true;
    }

    // ========================================================================
    // The bound
    // ========================================================================

    /// <summary>Drop the least-recently-drawn slot when a new id would take the
    /// dictionary past <see cref="MaxEntries"/>. O(MaxEntries) and it fires only
    /// when a genuinely NEW media id becomes visible — i.e. on a project change or
    /// a scroll onto unseen media, never on a redraw of an unchanged
    /// viewport.</summary>
    private void EvictIfFull()
    {
        while (_slots.Count >= MaxEntries)
        {
            Slot? victim = null;
            foreach (var slot in _slots.Values)
            {
                if (slot.InFlight)
                {
                    continue;
                }

                if (victim is null || slot.LastDrawnTick < victim.LastDrawnTick)
                {
                    victim = slot;
                }
            }

            if (victim is null)
            {
                // Every slot has a request in flight. Growing past the bound for
                // one cycle is better than evicting a slot whose answer is about
                // to arrive against a freed pin.
                return;
            }

            victim.Release();
            _slots.Remove(victim.MediaId);
        }
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        foreach (var slot in _slots.Values)
        {
            slot.Release();
        }

        _slots.Clear();
        _due.Clear();
    }

    /// <summary>One media id's state: resolved bytes and their pin, or the poll
    /// bookkeeping that is still trying to get them.</summary>
    private sealed class Slot
    {
        public required string MediaId { get; init; }

        /// <summary>Insertion order, the tie-break that makes selection fair.</summary>
        public required long Seq { get; init; }

        public byte[]? Peaks;
        public GCHandle Pin;
        public nint Ptr;
        public long BlockUs;
        public uint SampleRate;

        public long LastDrawnTick;

        public bool Wanted;
        public bool GivenUp;
        public bool InFlight;
        public int ConsecutiveNulls;
        public long NextAttemptMs;

        public void Release()
        {
            if (Pin.IsAllocated)
            {
                Pin.Free();
            }

            Peaks = null;
            Ptr = nint.Zero;
        }
    }
}
