using System.Runtime.InteropServices;
using System.Text.Json;
using Rudis.Shell.Interop;
using Windows.Graphics.Imaging;
using Windows.Storage.Streams;

namespace Rudis.Shell.Regions;

/// <summary>
/// One media id's strip geometry, as the frame builder needs it. A
/// <c>readonly record struct</c> so the per-visible-clip lookup on the render path
/// hands it back without allocating (<c>PeakCache</c>'s own rule, applied to a
/// payload that carries seven numbers instead of one).
/// </summary>
/// <param name="TileW">Tile cell width in pixels (<c>crates/filmstrip</c>'s
/// <c>TILE_W</c> = 96 in every strip this build produces; read from the payload
/// rather than assumed, because the grid is the payload's to state).</param>
/// <param name="TileH">Tile cell height in pixels (<c>TILE_H</c> = 54).</param>
/// <param name="TilesPerRow">Tiles side by side in one sheet row
/// (<c>TILES_PER_ROW</c> = 16), so <c>SheetW == TilesPerRow * TileW</c>.</param>
/// <param name="TotalTiles">Tiles the finished grid holds.</param>
/// <param name="CompletedTiles">Tiles actually present in the pinned bytes. D-14's
/// three-state distinction is this number against <paramref name="TotalTiles"/>:
/// fewer means PARTIAL (drawable now, more coming), equal means COMPLETE.</param>
/// <param name="IntervalUs">Source microseconds between consecutive tiles — the
/// divisor a consumer maps a trimmed clip window onto tile indices with, which is
/// why a non-positive value is rejected before anything is pinned.</param>
/// <param name="IsPlaceholder">D-13: these bytes are the media's IMPORT-TIME POSTER
/// stretched across the body, not a strip. A 1x1 grid by construction, so a consumer
/// that draws tiles needs no second code path — it draws one tile across the whole
/// clip body instead of N.</param>
internal readonly record struct StripInfo(
    uint TileW,
    uint TileH,
    uint TilesPerRow,
    uint TotalTiles,
    uint CompletedTiles,
    long IntervalUs,
    bool IsPlaceholder)
{
    /// <summary>Sheet width in pixels — <c>TilesPerRow * TileW</c>, validated against
    /// the payload's own <c>sheet_w</c> before the bytes were accepted.</summary>
    public uint SheetW => TilesPerRow * TileW;

    /// <summary>Whole tile-rows the present bytes cover.</summary>
    public uint SheetRows =>
        TilesPerRow == 0 ? 0 : (CompletedTiles + TilesPerRow - 1) / TilesPerRow;

    /// <summary>Sheet height in pixels.</summary>
    public uint SheetH => SheetRows * TileH;

    /// <summary>D-14: every tile the grid holds is present. A consumer that sees this
    /// can stop expecting the picture to change.</summary>
    public bool IsComplete => !IsPlaceholder && CompletedTiles >= TotalTiles;
}

/// <summary>A decoded poster, bounded and in RGBA8 rows.</summary>
/// <param name="Rgba">Exactly <c>Width * 4 * Height</c> bytes.</param>
/// <param name="Width">Decoded width in pixels.</param>
/// <param name="Height">Decoded height in pixels.</param>
internal readonly record struct PosterBitmap(byte[] Rgba, int Width, int Height);

/// <summary>
/// D-13's placeholder decode, as a SEAM.
///
/// <para>Injected rather than reached for, for exactly the reason
/// <see cref="FilmstripCache.PumpAsync"/>'s fetch delegate is: the failure paths that
/// matter here — a poster that no longer exists, a truncated PNG, a decoder that
/// throws — are the ones a real file cannot be relied on to produce on demand, and a
/// unit test must be able to produce all three in microseconds. The production
/// implementation is <see cref="WicPosterDecoder"/> below.</para>
///
/// <para><b>ASYNCHRONOUS, and that is a correction to this plan's stated signature.</b>
/// 53.2-06 specified <c>bool TryDecode(..., out byte[] rgba, ...)</c> AND
/// <c>Windows.Graphics.Imaging.BitmapDecoder</c> as the implementation. Those two are
/// not compatible in this codebase: every WinRT imaging entry point is an
/// <c>IAsyncOperation</c>, and turning one into a synchronous <c>bool</c> requires
/// <c>.GetResult()</c> / <c>.Result</c> / <c>.Wait()</c> — all three of which
/// <c>MechanicalGatesTests.no_blocking_waits_in_shell_sources</c> makes a BUILD
/// FAILURE across <c>shell/Rudis.Shell/**</c>. The seam is therefore async, which also
/// delivers the "off the UI thread" half of the requirement structurally rather than by
/// convention: the decode never runs on the caller's thread at all.</para>
/// </summary>
internal interface IPosterDecoder
{
    /// <summary>
    /// Decode <paramref name="path"/> to RGBA8, downscaled to fit
    /// <paramref name="maxW"/> x <paramref name="maxH"/> with the source aspect
    /// preserved (D-08's letterbox rule is the RENDERER's; this only bounds the
    /// decode). Answers <see langword="null"/> for every failure — absent file,
    /// unreadable file, unsupported codec, corrupt bytes — because
    /// <see cref="FilmstripCache"/>'s response to all of them is identical.
    /// </summary>
    Task<PosterBitmap?> DecodeAsync(string path, int maxW, int maxH);
}

/// <summary>
/// 53.2 D-12's C#-policy half: WHICH media ids want a filmstrip, which of their strips
/// stay resident, and when each is asked for again.
///
/// <para>Shaped on <see cref="PeakCache"/> deliberately and almost line for line —
/// bounded least-recently-DRAWN LRU, a per-cycle request cap, exponential back-off with
/// a give-up rule, GCHandle-pinned buffers handed to the frame contract by pointer, and
/// an INJECTED fetch delegate so this class never names the ABI. What differs is the
/// payload's WEIGHT and D-14's third state, and both differences are numbers rather
/// than structure. See each constant's own derivation.</para>
///
/// <para><b>This class knows nothing about the ABI, deliberately.</b> The fetch is a
/// delegate the caller supplies (plan 53.2-07 wires it to the cold poll). Two things
/// follow, and both are the point: the whole poll policy is testable against a recording
/// double that can answer <c>null</c> a hundred times in microseconds, and the source
/// scan in <c>TimelinePaintScopeTests</c> can assert ZERO whole-word ABI references in
/// this file and mean it. 53.2-07 adds <c>GetFilmstripStripAsync</c> to that scan's
/// pattern list; the delegate injection exists precisely so adding it changes nothing
/// here.</para>
///
/// <para><b>The three states (D-14), which is the whole reason this is not just
/// <see cref="PeakCache"/> with a different payload):</b></para>
/// <list type="number">
/// <item><b>null</b> — miss, not-yet-extracted, audio-only, a still, an offline file, a
///   decode failure, an unknown id. All SEVEN are the same answer at the ABI by design
///   (D-15), because distinguishing them would require the read to know something only a
///   decode could tell it. The id backs off from <see cref="FirstBackoffMs"/>, doubling
///   to <see cref="MaxBackoffMs"/>, and is dropped after
///   <see cref="GiveUpAfterConsecutiveNulls"/> consecutive nulls.</item>
/// <item><b>partial</b> — <c>completed_tiles &lt; total_tiles</c>. The bytes are KEPT and
///   DRAWN (that is the entire point of D-14's progressive fill: 53.2-04 measured the
///   first 16 tiles drawable at 7.24 s against a 34.83 s completion, frames visible 4.8x
///   sooner), the null streak RESETS because progress is not a miss, and the id re-polls
///   at a flat <see cref="PartialRepollMs"/> — never on the null back-off.</item>
/// <item><b>complete</b> — <c>completed_tiles == total_tiles</c>. Cached and the id STOPS
///   being asked, forever. The cache key on the Rust side is
///   <c>(canonical path, mtime_ns, size_bytes)</c>, so for a given media id a complete
///   answer cannot change.</item>
/// </list>
///
/// <para><b>Every cached array is PINNED for as long as it is cached</b> and the pinned
/// address is handed to the frame builder by <see cref="TryGetPinnedStrip"/> — 52-06's
/// rule ("the redraw path must not perform a handle-table operation per visible clip per
/// frame") applied to a third kind of buffer. The old pin is released BEFORE a partial
/// upgrade replaces it, which is the one leak this shape can have and the one
/// <c>eviction_unpins_and_a_stale_pin_is_never_returned</c> exists to catch.</para>
///
/// <para><b>Single-threaded by contract</b>, exactly as <see cref="PeakCache"/> is:
/// every member runs on the UI thread — <see cref="NoteWanted"/> and
/// <see cref="TryGetPinnedStrip"/> from the frame build, <see cref="PumpAsync"/> from
/// the cold cycle's UI-thread half. The two things that genuinely must not run there
/// (the ABI round trip and the poster decode) both leave the thread by construction:
/// the fetch delegate hands its call to the interop worker and
/// <see cref="IPosterDecoder"/> is asynchronous. A pump running on the pool while the
/// render loop read the same <see cref="Dictionary{TKey,TValue}"/> would be a genuine
/// data race, and no gate in this plan would have caught it.</para>
///
/// <para>No WinUI types, by rule (52 D-13) — this file lives in the WinUI-free
/// <c>Regions/Timeline/</c> directory and every one of its behaviours is unit-tested
/// with no window. <c>Windows.Graphics.Imaging</c> is WinRT IMAGING, not WinUI: it needs
/// no visual tree, no dispatcher and no XAML, which is why
/// <c>TimelineHotPathGateTests</c>' forbidden vocabulary (<c>Microsoft.UI</c>,
/// <c>Windows.UI</c>, <c>Microsoft.Graphics</c>, <c>DependencyObject</c>,
/// <c>FrameworkElement</c>, <c>SwapChainPanel</c>, <c>DispatcherQueue</c>) does not name
/// it and must not be extended to.</para>
/// </summary>
internal sealed class FilmstripCache : IDisposable
{
    // ========================================================================
    // The constants. Each one states its derivation AGAINST PeakCache's value,
    // because "mirror PeakCache" is the instruction and a silently different
    // number is how a mirrored design stops being one.
    // ========================================================================

    /// <summary>Requests allowed to leave in ONE cold cycle. <b>ONE, where
    /// <see cref="PeakCache.MaxRequestsPerCycle"/> is two.</b> A peaks payload is
    /// kilobytes; 53.2-04 measured a strip payload at <b>5,308,416 bytes worst case</b>
    /// (256 tiles of 96x54 RGBA), crossing the ABI as base64 at a measured 1.333x —
    /// ~6.75 MiB of JSON. The cost that has to be bounded is not the round trip, it is
    /// the base64 decode and the parse, and both happen on the cold cycle's UI-thread
    /// half. One per 100 ms cycle bounds that; two would double the worst-case stall for
    /// no benefit a user could see, because the second strip is drawn 100 ms later
    /// either way.</summary>
    public const int MaxRequestsPerCycle = 1;

    /// <summary>Slots retained, least-recently-DRAWN evicted first.
    /// <b>24, where <see cref="PeakCache.MaxEntries"/> is 256.</b> The SHAPE transfers;
    /// the NUMBER cannot, and D-16 says so in as many words ("<c>PeakCache.MaxEntries =
    /// 256</c> is the shape but not the number"). At 53.2-04's measured 5,308,416-byte
    /// worst case per entry, 24 slots is ~121.5 MiB of pinned bytes at the absolute
    /// ceiling and tens of MiB in practice (a 5 s clip's strip is 331,776 B, a 75 s
    /// clip's 1,658,880 B). PeakCache's 256 at the same weight would be <b>1.27 GiB</b>
    /// — pinned, on the LOH, for the lifetime of a scroll.
    ///
    /// <para>24 rather than a rounder number because it is the count that keeps a
    /// full-height timeline's worth of DISTINCT media resident: the viewport shows on the
    /// order of a dozen clips at a usable zoom, several of which typically share a media
    /// id, and 24 leaves headroom for a scroll to overshoot and come back without
    /// re-fetching. D-16's own fallback covers the miss — an evicted clip renders D-13's
    /// poster until its strip returns.</para></summary>
    public const int MaxEntries = 24;

    /// <summary>The first retry delay after a null. <b>D-15: PeakCache verbatim.</b>
    /// Deliberately far below <see cref="MaxBackoffMs"/> — extraction is a detached
    /// import-time job, so the first non-null answer can arrive within a second of the
    /// item appearing in the bin, and a flat 2 s back-off would leave a freshly imported
    /// clip showing its poster for up to two seconds longer than it has to.</summary>
    public const long FirstBackoffMs = 200;

    /// <summary>The back-off ceiling — "no more often than once per 2 s per id".
    /// <b>D-15: PeakCache verbatim.</b> 53.2-04 measured a 75 s source completing in
    /// 34.83 s with <c>MAX_CONCURRENT_FILMSTRIP_JOBS = 1</c>; a long import legitimately
    /// takes a while and asking faster changes nothing.</summary>
    public const long MaxBackoffMs = 2_000;

    /// <summary>Consecutive nulls before an id is dropped. <b>D-15: PeakCache
    /// verbatim.</b> With the back-off above that is roughly 3.3 minutes of asking, which
    /// outlasts a legitimate long extraction and still bounds a permanently-failing
    /// source (audio-only media, a still, an offline file, a codec the decoder refuses)
    /// at 100 wasted calls total. This rule has to live HERE and nowhere else: at the ABI
    /// a permanently-failing source and a not-ready-yet source are the same answer BY
    /// DESIGN, so only the client can ever stop.</summary>
    public const int GiveUpAfterConsecutiveNulls = 100;

    /// <summary>How often a PARTIAL strip is asked again. <b>D-14, and it has no
    /// PeakCache counterpart at all</b> — peaks were written once, at the end, so
    /// retrieval was flat and a non-null answer was always final.
    ///
    /// <para>A FIXED 1 s cadence, deliberately NOT the null back-off: progress is not a
    /// miss. A strip that just gained 16 tiles is a strip whose producer is alive and
    /// working, and backing off exponentially from it would make the LAST tiles of a long
    /// file arrive minutes after they were written. 1 s against 53.2-04's measured 7.24 s
    /// first-publish / 34.83 s completion means roughly 27 polls across a 75 s source's
    /// fill — each one a ~1.6 MiB parse on the cold path, which is why it is a second and
    /// not 100 ms.</para></summary>
    public const long PartialRepollMs = 1_000;

    /// <summary>Poster decodes allowed to start in ONE cold cycle. Four, not one: a
    /// bounded 256x144 decode is sub-millisecond of CPU plus a small file read, and the
    /// placeholder is the FIRST thing the user sees (D-13's whole argument is
    /// "recognizable instantly at zero new decode cost"). Four per 100 ms cycle fills the
    /// entire <see cref="MaxEntries"/>-slot cache inside 600 ms while never spending more
    /// than a few percent of one cycle.</summary>
    public const int MaxPosterDecodesPerCycle = 4;

    /// <summary>D-13 / T-53.2-26: the placeholder decode's hard ceiling in pixels. A
    /// poster is an arbitrary PNG on disk and a full-resolution decode of one is
    /// unbounded work with no visible symptom until a real library is imported — the same
    /// threat <c>MediaBinPoster.DecodePixelWidth</c> bounds for the bin's tiles.
    /// 256x144 is 16:9 at roughly 2.7x a filmstrip tile, which is enough to survive being
    /// stretched across a wide clip body at 150% DPI and is 147,456 B of RGBA.</summary>
    public const int PlaceholderMaxW = 256;

    /// <summary>See <see cref="PlaceholderMaxW"/>.</summary>
    public const int PlaceholderMaxH = 144;

    // ── the payload's fail-closed clamps ────────────────────────────────────
    //
    // These MIRROR crates/filmstrip/src/cache.rs's header sanity clamps
    // (MIN/MAX_TILE_DIM, MIN/MAX_TILES_PER_ROW, MIN/MAX_TOTAL_TILES,
    // MAX_STRIP_FILE_BYTES) rather than trusting that the Rust reader already
    // applied them. T-53.2-23: the JSON envelope and its base64 sheet are
    // untrusted-SHAPED input on this side of the boundary, and geometry that
    // disagrees with the byte count is exactly how a consumer that sizes a GPU
    // upload from the header reads past the end of the buffer. A violation is a
    // NULL — the same fail-closed answer the Rust reader gives.

    /// <summary>See <c>crates/filmstrip/src/cache.rs</c>'s <c>MIN_TILE_DIM</c>.</summary>
    public const uint MinTileDim = 1;

    /// <summary>See <c>MAX_TILE_DIM</c>.</summary>
    public const uint MaxTileDim = 512;

    /// <summary>See <c>MIN_TILES_PER_ROW</c>.</summary>
    public const uint MinTilesPerRow = 1;

    /// <summary>See <c>MAX_TILES_PER_ROW</c>.</summary>
    public const uint MaxTilesPerRow = 64;

    /// <summary>See <c>MIN_TOTAL_TILES</c>.</summary>
    public const uint MinTotalTiles = 1;

    /// <summary>See <c>MAX_TOTAL_TILES</c>.</summary>
    public const uint MaxTotalTiles = 4096;

    /// <summary>The byte ceiling for ANY pinned buffer, strip or placeholder — the
    /// managed twin of <c>crates/filmstrip/src/cache.rs</c>'s <c>MAX_STRIP_FILE_BYTES</c>.
    /// Checked against the base64 string's LENGTH before a single byte is decoded, so a
    /// hostile payload cannot make this process allocate its way out of memory before the
    /// geometry check gets a chance to reject it (T-53.2-23 / T-53.2-24).</summary>
    public const int MaxStripBytes = 8 * 1024 * 1024;

    private readonly Dictionary<string, Slot> _slots = new(StringComparer.Ordinal);

    /// <summary>Reused across cycles so selecting the due ids allocates nothing.</summary>
    private readonly List<Slot> _due = new(MaxRequestsPerCycle);

    /// <summary>Reused across cycles, same reason.</summary>
    private readonly List<Slot> _posterDue = new(MaxPosterDecodesPerCycle);

    private readonly IPosterDecoder _posters;

    /// <summary>
    /// Where a miss or a give-up is reported. Injected rather than reached for, for the
    /// same reason the fetch is: this file must stay free of every type that lives
    /// outside the WinUI-free directory, and the shell's own diagnostic sink is a member
    /// of the WinUI <c>Application</c>. The region passes it in; a unit test passes
    /// nothing.
    /// </summary>
    private readonly Action<string>? _log;

    private long _drawTick;
    private long _seq;
    private bool _disposed;

    /// <param name="posterDecoder">D-13's placeholder source. Defaults to the production
    /// WinRT decoder so a caller that forgets does not silently lose every placeholder;
    /// tests inject a fake. A media id with no <c>PosterPath</c> never reaches it at
    /// all, which is how a test that does not care about placeholders opts out.</param>
    /// <param name="log">Diagnostics sink; see the field's remarks.</param>
    public FilmstripCache(IPosterDecoder? posterDecoder = null, Action<string>? log = null)
    {
        _posters = posterDecoder ?? WicPosterDecoder.Shared;
        _log = log;
    }

    /// <summary>
    /// Bumped ONCE per media id whose DRAWN CONTENT changes: a placeholder arriving, a
    /// strip replacing it, and each partial advancing. The frame builder folds it into
    /// its dirty check, which is what makes a strip arriving cost exactly one redraw —
    /// not zero (the surface would keep drawing the pre-strip frame forever) and not one
    /// per poll.
    ///
    /// <para>A partial re-poll that returns the SAME completed count does NOT bump it.
    /// The Rust side guarantees every rewrite strictly advances <c>completed_tiles</c>
    /// (53.2-04's resume rule), so an unchanged count means nothing new was written and
    /// redrawing would be a second of wasted work per second per partial clip.</para>
    /// </summary>
    public long Generation { get; private set; }

    /// <summary>Requests this cache has issued through the injected fetch, ever. Read by
    /// the tests and by any later introspection query; a policy whose traffic nobody can
    /// count is a policy nobody can check.</summary>
    public long RequestsIssued { get; private set; }

    /// <summary>Poster decodes STARTED, ever — success and failure alike. The number
    /// <c>decode_failures_degrade_to_nothing</c> asserts stays at one per media id.</summary>
    public long PosterDecodesAttempted { get; private set; }

    /// <summary>Slots held, resolved or not — the number <see cref="MaxEntries"/>
    /// bounds.</summary>
    public int SlotCount => _slots.Count;

    /// <summary>Ids still being polled.</summary>
    public int WantedCount => Count(static s => s.Wanted);

    /// <summary>Ids holding REAL strip bytes, partial or complete.</summary>
    public int ResolvedCount => Count(static s => s.Rgba is not null && !s.Info.IsPlaceholder);

    /// <summary>Ids holding a decoded poster instead of a strip (D-13).</summary>
    public int PlaceholderCount => Count(static s => s.Rgba is not null && s.Info.IsPlaceholder);

    /// <summary>Ids whose strip is complete — D-14's terminal state.</summary>
    public int CompleteCount => Count(static s => s.Complete);

    /// <summary>Ids dropped by the give-up rule.</summary>
    public int GivenUpCount => Count(static s => s.GivenUp);

    public bool IsWanted(string mediaId) =>
        mediaId is not null && _slots.TryGetValue(mediaId, out var slot) && slot.Wanted;

    private int Count(Func<Slot, bool> predicate)
    {
        var n = 0;
        foreach (var slot in _slots.Values)
        {
            if (predicate(slot))
            {
                n++;
            }
        }

        return n;
    }

    // ========================================================================
    // The stable key — 53.2-07's renderer-side handle for one resident strip
    // ========================================================================

    /// <summary>
    /// FNV-1a 64 over the media id's UTF-8 bytes, with the low bit flipped for the
    /// placeholder variant.
    ///
    /// <para>Why a hash at all: the renderer's per-frame struct carries fixed-width
    /// numbers, not strings, so the strip a clip wants has to be nameable as a
    /// <c>u64</c>. Why FNV-1a specifically: it is four lines, has no dependency, is
    /// byte-order-independent, and is <b>reproducible on the Rust side from the same
    /// spec</b> — which matters, because 53.2-07's atlas will key on this number and the
    /// two languages must agree without sharing code.</para>
    ///
    /// <para><b>Not a security boundary.</b> A media id is backend-minted and the map it
    /// keys is process-local and bounded at <see cref="MaxEntries"/>; a collision costs
    /// one wrong thumbnail, not a memory-safety failure. Nothing here is a substitute for
    /// the id itself, which stays the dictionary's key.</para>
    ///
    /// <para>The placeholder XOR is <c>0x1</c> rather than a separate hash so that a
    /// media id's two possible pictures — its poster and its strip — are one bit apart
    /// and obviously related in a log. Pure and static, so
    /// <c>the_strip_key_is_a_pinned_fnv1a_vector</c> can pin it against a literal
    /// computed independently.</para>
    /// </summary>
    public static ulong StripKeyFor(string mediaId, bool placeholder)
    {
        // FNV-1a 64, the canonical parameters.
        const ulong offset = 14695981039346656037UL;
        const ulong prime = 1099511628211UL;

        var hash = offset;
        if (mediaId is not null)
        {
            // Encoding.UTF8.GetBytes would allocate per call; this walks the UTF-8 the
            // string already implies without materialising it, for ids that are ASCII in
            // practice and correct for any id that is not.
            Span<byte> buffer = stackalloc byte[256];
            var utf8 = System.Text.Encoding.UTF8;
            var needed = utf8.GetByteCount(mediaId);
            var bytes = needed <= buffer.Length ? buffer[..needed] : new byte[needed];
            utf8.GetBytes(mediaId, bytes);

            for (var i = 0; i < bytes.Length; i++)
            {
                hash ^= bytes[i];
                hash *= prime;
            }
        }

        return placeholder ? hash ^ 0x1UL : hash;
    }

    // ========================================================================
    // The frame build's side — cheap, allocation-free in the steady state, and
    // structurally incapable of starting work
    // ========================================================================

    /// <summary>
    /// Note every visible filmstrip-bearing clip's media id as WANTED, and record the
    /// poster path D-13's placeholder comes from.
    ///
    /// <para>Called from the frame build with the CULLED clip list, so the wanted set is
    /// the viewport's and not the project's. The gate is <c>ClipLayout.WantsFilmstrip</c>
    /// — 53.2 D-05/D-07's "a clip on a VIDEO lane whose media kind is video", resolved
    /// ONCE in <c>TimelineModel.Rebuild</c> where the lane kind is in scope, exactly as
    /// <c>HasAudio</c> is for <see cref="PeakCache"/>. Re-deriving it here is how the two
    /// halves of D-02 drifted apart in the first place.</para>
    ///
    /// <para>This starts NOTHING. It records intent; <see cref="PumpAsync"/> is the only
    /// member that can cause a fetch or a decode, and it is never reachable from a
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

            if (!clip.WantsFilmstrip || string.IsNullOrEmpty(clip.MediaId))
            {
                continue;
            }

            if (_slots.TryGetValue(clip.MediaId, out var slot))
            {
                slot.LastDrawnTick = ++_drawTick;

                // A poster path that arrives late (the mirror learned it after the clip
                // first became visible) still gets its placeholder. A path that arrives
                // DIFFERENT is ignored while one is already recorded: the poster is
                // per-media and immutable for a given file, and swapping it would mean
                // decoding twice for the same picture.
                if (string.IsNullOrEmpty(slot.PosterPath) && !string.IsNullOrEmpty(clip.PosterPath))
                {
                    slot.PosterPath = clip.PosterPath;
                }

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
                PosterPath = clip.PosterPath,
            };
        }
    }

    /// <summary>
    /// The render path's ONLY door into the pixels, and it CANNOT OPEN A CONNECTION.
    ///
    /// <para><b>Do not ever give this method a fetch.</b> That is 52-RESEARCH Pitfall 7's
    /// forbidden shape in one line — "get this clip's filmstrip, and go and get it if we
    /// haven't got it" — and it will look completely correct on a warm cache. The bug
    /// only appears as stutter on a cold cache or a long media item, which is exactly why
    /// the check is mechanical rather than visual. If a clip's strip is not here, the
    /// correct behaviour is to draw the clip's body flat and let the cold poll do its
    /// job.</para>
    ///
    /// <para>Allocates nothing, stamps the draw tick that drives eviction, and answers
    /// <see langword="false"/> for every miss: unknown id, nothing decoded yet, given up
    /// with no poster, evicted. The returned <paramref name="info"/> tells a caller which
    /// of the two pictures it got — <see cref="StripInfo.IsPlaceholder"/> — and, for a
    /// strip, how much of it is real (<see cref="StripInfo.CompletedTiles"/> against
    /// <see cref="StripInfo.TotalTiles"/>).</para>
    ///
    /// <para>The pointer is valid for as long as the id stays cached, which is at least
    /// until the next <see cref="NoteWanted"/> can evict it — and eviction runs on the
    /// same UI thread as the frame build, never between a builder's fill and the render
    /// call that consumes it.</para>
    /// </summary>
    public bool TryGetPinnedStrip(string mediaId, out nint ptr, out int length, out StripInfo info)
    {
        if (mediaId is not null && _slots.TryGetValue(mediaId, out var slot) && slot.Rgba is not null)
        {
            slot.LastDrawnTick = ++_drawTick;
            ptr = slot.Ptr;
            length = slot.Rgba.Length;
            info = slot.Info;
            return true;
        }

        ptr = nint.Zero;
        length = 0;
        info = default;
        return false;
    }

    // ========================================================================
    // The cold cycle's side
    // ========================================================================

    /// <summary>
    /// One cold cycle: start up to <see cref="MaxPosterDecodesPerCycle"/> placeholder
    /// decodes, then ask for at most <see cref="MaxRequestsPerCycle"/> due strips through
    /// the injected fetch, and fold the answers in.
    ///
    /// <para><paramref name="nowMs"/> is a caller-supplied monotonic millisecond clock,
    /// so both schedules are pure functions of their inputs and a test can drive ten
    /// simulated seconds without waiting ten real ones.</para>
    ///
    /// <para><b>Never throws.</b> A throwing fetch, a transport fault, a domain error, a
    /// corrupt payload, a poster decoder that throws and a poster that no longer exists
    /// are all the same thing here — nothing to show — because this runs on a
    /// fire-and-forget cold cycle where an escaping exception would be invisible.</para>
    ///
    /// <para>Placeholders are pumped FIRST, and deliberately: D-13's argument is that the
    /// clip is recognizable INSTANTLY, so on the very first cycle after a clip becomes
    /// visible the poster should already be on its way while the strip request is still
    /// in flight.</para>
    /// </summary>
    public async Task PumpAsync(Func<string, Task<RudisResult<JsonElement>>> fetch, long nowMs)
    {
        ArgumentNullException.ThrowIfNull(fetch);

        if (_disposed)
        {
            return;
        }

        await PumpPostersAsync();

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
                _log?.Invoke($"filmstrip strip fetch faulted for {slot.MediaId}: {e.GetType().Name}: {e.Message}");
                result = RudisResult<JsonElement>.Fault(RudisStatus.PanicCaught, e.Message);
            }

            slot.InFlight = false;

            if (_disposed || !_slots.TryGetValue(slot.MediaId, out var current) || !ReferenceEquals(current, slot))
            {
                // Evicted, or the cache was torn down, while the answer was in flight.
                // Dropping it is correct: the id is no longer resident, and D-16's
                // fallback (the placeholder, until a refetch) is the right picture.
                continue;
            }

            Apply(slot, result, nowMs);
        }
    }

    /// <summary>Up to <see cref="MaxRequestsPerCycle"/> wanted, not-in-flight ids whose
    /// back-off (or partial re-poll) has expired, oldest deadline first with insertion
    /// order as the tie-break — so a large wanted set drains fairly rather than starving
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

    /// <summary>Fold one strip answer in: cache it, advance it, or schedule the next
    /// attempt.</summary>
    private void Apply(Slot slot, in RudisResult<JsonElement> result, long nowMs)
    {
        if (TryDecode(result, out var rgba, out var info))
        {
            // A partial that has not advanced is not a redraw. Everything else is: a
            // strip replacing a placeholder, a first partial, a partial gaining tiles,
            // and a re-extraction at a different grid.
            var changed =
                slot.Rgba is null ||
                slot.Info.IsPlaceholder ||
                slot.Info.CompletedTiles != info.CompletedTiles ||
                slot.Info.TotalTiles != info.TotalTiles ||
                slot.Info.TilesPerRow != info.TilesPerRow ||
                slot.Info.TileW != info.TileW ||
                slot.Info.TileH != info.TileH;

            if (changed)
            {
                // Release the OLD pin BEFORE replacing it. A partial upgrade runs this
                // path repeatedly for the same id, so leaking one handle here would leak
                // one per second per partially-filled clip.
                slot.Adopt(rgba, info);
                Generation++;
            }

            slot.ConsecutiveNulls = 0;

            if (info.IsComplete)
            {
                // D-14's terminal state. The id STOPS being asked, forever.
                slot.Wanted = false;
                slot.Complete = true;
                return;
            }

            // D-14: progress is not a miss, so a FIXED cadence rather than the null
            // back-off.
            slot.NextAttemptMs = nowMs + PartialRepollMs;
            return;
        }

        slot.ConsecutiveNulls++;
        if (slot.ConsecutiveNulls >= GiveUpAfterConsecutiveNulls)
        {
            // D-15's give-up rule. The clip keeps whatever it has — a placeholder, if the
            // media had a poster — from here on, which is also the correct PERMANENT
            // state for a still image (whose poster already IS its filmstrip) and for an
            // audio-only or offline source.
            slot.Wanted = false;
            slot.GivenUp = true;
            _log?.Invoke(
                $"filmstrip: giving up on {slot.MediaId} after " +
                $"{GiveUpAfterConsecutiveNulls} consecutive nulls (53.2 D-15)");
            return;
        }

        // Exponential, capped. The shift is bounded before it is taken: shifting a long
        // by 64 or more masks to 0..63 in C# and would silently produce a SHORTER delay
        // than the previous attempt.
        var steps = Math.Min(slot.ConsecutiveNulls - 1, 20);
        var delay = Math.Min(MaxBackoffMs, FirstBackoffMs << steps);
        slot.NextAttemptMs = nowMs + delay;
    }

    /// <summary>
    /// The payload, exactly as <c>crates/app-core/src/filmstrip_job.rs</c>'s
    /// <c>StripPayload</c> writes it, then CHECKED rather than trusted.
    ///
    /// <para>Every failure is a MISS, never an exception (T-53.2-23): a domain error, a
    /// transport fault, a <c>{"Ok": null}</c> body, a missing or wrong-typed field, a
    /// malformed base64 string, a geometry that disagrees with itself, a byte count that
    /// disagrees with the geometry, a dimension outside the Rust reader's own clamps, a
    /// non-positive <c>interval_us</c>, or a payload past
    /// <see cref="MaxStripBytes"/>.</para>
    ///
    /// <para>Three of those are worth naming individually. <c>sheet_w</c> is redundant
    /// with <c>tiles_per_row * tile_w</c> ON PURPOSE, so a mismatch is a corruption
    /// signal. The byte-length equality is what stands between a lying header and a
    /// consumer that sizes a GPU upload from it. And <c>interval_us</c> is the divisor a
    /// consumer maps a trimmed clip window onto tile indices with — a zero would be a
    /// divide-by-zero inside the paint path, the one place this codebase cannot afford
    /// one (<c>PeakCache</c>'s identical rule for <c>block_us</c>).</para>
    ///
    /// <para><c>completed_tiles == 0</c> is treated as a NULL rather than as an empty
    /// partial: there is nothing to draw, nothing to pin, and a zero-length pinned array
    /// yields a pointer that is legal to hold and illegal to dereference. Counting it as
    /// a null also keeps the back-off doing its job while the first chunk is still being
    /// decoded.</para>
    /// </summary>
    private static bool TryDecode(in RudisResult<JsonElement> result, out byte[] rgba, out StripInfo info)
    {
        rgba = [];
        info = default;

        if (result.Kind != RudisResultKind.Ok)
        {
            return false;
        }

        if (!FilmstripStripPayload.TryParse(result.Value, out var payload))
        {
            // `{"Ok": null}` and every malformed shape alike — D-15's flat null.
            return false;
        }

        if (payload.TileW < MinTileDim || payload.TileW > MaxTileDim ||
            payload.TileH < MinTileDim || payload.TileH > MaxTileDim ||
            payload.TilesPerRow < MinTilesPerRow || payload.TilesPerRow > MaxTilesPerRow ||
            payload.TotalTiles < MinTotalTiles || payload.TotalTiles > MaxTotalTiles ||
            payload.CompletedTiles > payload.TotalTiles ||
            payload.CompletedTiles == 0 ||
            payload.IntervalUs <= 0)
        {
            return false;
        }

        var candidate = new StripInfo(
            payload.TileW,
            payload.TileH,
            payload.TilesPerRow,
            payload.TotalTiles,
            payload.CompletedTiles,
            payload.IntervalUs,
            IsPlaceholder: false);

        if (payload.SheetW != candidate.SheetW || payload.SheetH != candidate.SheetH)
        {
            return false;
        }

        var expected = (long)candidate.SheetW * 4L * candidate.SheetH;
        if (expected <= 0 || expected > MaxStripBytes)
        {
            return false;
        }

        var b64 = payload.StripB64;

        // Bound the DECODE before it happens: standard padded base64 is 4 characters per
        // 3 bytes, so anything longer than this cannot decode to something we would
        // accept, and refusing it here means a hostile payload never gets to allocate.
        if (b64.Length > ((MaxStripBytes / 3) + 1) * 4)
        {
            return false;
        }

        byte[] decoded;
        try
        {
            // Standard padded base64 (RFC 4648 §4), no options — the same encoding
            // `PeaksPayload::peaks_b64` already crosses on.
            decoded = Convert.FromBase64String(b64);
        }
        catch (FormatException)
        {
            return false;
        }

        if (decoded.LongLength != expected)
        {
            return false;
        }

        rgba = decoded;
        info = candidate;
        return true;
    }

    // ========================================================================
    // D-13 — the placeholder, riding the SAME pinned-buffer shape
    // ========================================================================

    /// <summary>
    /// Start (and await) up to <see cref="MaxPosterDecodesPerCycle"/> poster decodes.
    ///
    /// <para>A media id is decoded <b>at most once, ever</b> — success or failure. That
    /// is T-53.2-26's mitigation and it is also the difference between "the poster is
    /// missing" and "a retry storm on a file that will never be readable": every
    /// exception, every null, every unusable result sets the same
    /// <c>PosterAttempted</c> flag the success path sets.</para>
    ///
    /// <para>A decoded poster is adopted ONLY if the slot still holds nothing. A real
    /// strip that arrived while the decode was in flight always wins — a placeholder
    /// overwriting the picture it was standing in for would be a visible regression, and
    /// on a busy import it would be a frequent one.</para>
    /// </summary>
    private async Task PumpPostersAsync()
    {
        var count = SelectPosterDue();
        for (var i = 0; i < count; i++)
        {
            var slot = _posterDue[i];
            PosterDecodesAttempted++;

            PosterBitmap? bitmap = null;
            try
            {
                bitmap = await _posters.DecodeAsync(slot.PosterPath!, PlaceholderMaxW, PlaceholderMaxH);
            }
            catch (Exception e)
            {
                // T-53.2-26: a hostile or corrupt PNG must degrade to NO PLACEHOLDER, and
                // must do so exactly once. Never a crash on a fire-and-forget cycle.
                _log?.Invoke(
                    $"filmstrip placeholder decode faulted for {slot.MediaId}: " +
                    $"{e.GetType().Name}: {e.Message}");
            }

            slot.PosterInFlight = false;
            slot.PosterAttempted = true;

            if (_disposed || !_slots.TryGetValue(slot.MediaId, out var current) || !ReferenceEquals(current, slot))
            {
                continue;
            }

            if (slot.Rgba is not null)
            {
                // A strip landed first. It wins, always.
                continue;
            }

            if (bitmap is not { } bmp || !IsUsablePoster(bmp))
            {
                continue;
            }

            slot.Adopt(
                bmp.Rgba,
                new StripInfo(
                    (uint)bmp.Width,
                    (uint)bmp.Height,
                    TilesPerRow: 1,
                    TotalTiles: 1,
                    CompletedTiles: 1,
                    IntervalUs: 0,
                    IsPlaceholder: true));
            Generation++;
        }
    }

    /// <summary>Most-recently-DRAWN first: the placeholders that matter are the ones on
    /// screen right now, and on a fast scroll the wanted set turns over faster than the
    /// decode budget can drain it.</summary>
    private int SelectPosterDue()
    {
        _posterDue.Clear();

        foreach (var slot in _slots.Values)
        {
            if (slot.PosterAttempted || slot.PosterInFlight ||
                slot.Rgba is not null || string.IsNullOrEmpty(slot.PosterPath))
            {
                continue;
            }

            var at = _posterDue.Count;
            while (at > 0 && slot.LastDrawnTick > _posterDue[at - 1].LastDrawnTick)
            {
                at--;
            }

            if (at < MaxPosterDecodesPerCycle)
            {
                _posterDue.Insert(at, slot);
                if (_posterDue.Count > MaxPosterDecodesPerCycle)
                {
                    _posterDue.RemoveAt(_posterDue.Count - 1);
                }
            }
        }

        for (var i = 0; i < _posterDue.Count; i++)
        {
            _posterDue[i].PosterInFlight = true;
        }

        return _posterDue.Count;
    }

    /// <summary>The decoder is a seam, so its OUTPUT is untrusted too (T-53.2-26): the
    /// byte count has to agree with the dimensions before anything is pinned and handed
    /// to a renderer as a pointer.</summary>
    private static bool IsUsablePoster(in PosterBitmap bmp)
    {
        if (bmp.Rgba is null || bmp.Width <= 0 || bmp.Height <= 0)
        {
            return false;
        }

        if (bmp.Width > MaxTileDim || bmp.Height > MaxTileDim)
        {
            // The placeholder rides the strip's own geometry fields, so it inherits the
            // strip's dimension clamps. PlaceholderMaxW/H are well inside them; a decoder
            // that ignored its bounds gets rejected rather than trusted.
            return false;
        }

        var expected = (long)bmp.Width * 4L * bmp.Height;
        return expected > 0 && expected <= MaxStripBytes && bmp.Rgba.LongLength == expected;
    }

    // ========================================================================
    // The bound
    // ========================================================================

    /// <summary>Drop the least-recently-drawn slot when a new id would take the dictionary
    /// past <see cref="MaxEntries"/>, freeing its pin. O(MaxEntries) and it fires only
    /// when a genuinely NEW media id becomes visible — i.e. on a project change or a
    /// scroll onto unseen media, never on a redraw of an unchanged viewport.
    ///
    /// <para>D-16's C#-side mirror: an evicted clip falls back to whatever it can draw
    /// (the placeholder, once re-decoded) until a later <see cref="NoteWanted"/> starts a
    /// fresh fetch cycle for it.</para></summary>
    private void EvictIfFull()
    {
        while (_slots.Count >= MaxEntries)
        {
            Slot? victim = null;
            foreach (var slot in _slots.Values)
            {
                if (slot.InFlight || slot.PosterInFlight)
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
                // Every slot has work in flight. Growing past the bound for one cycle is
                // better than evicting a slot whose answer is about to arrive against a
                // freed pin.
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
        _posterDue.Clear();
    }

    /// <summary>One media id's state: the pinned bytes it can draw right now (a strip, a
    /// partial strip, or D-13's poster) plus the poll bookkeeping that is still trying to
    /// improve on them.</summary>
    private sealed class Slot
    {
        public required string MediaId { get; init; }

        /// <summary>Insertion order, the tie-break that makes selection fair.</summary>
        public required long Seq { get; init; }

        public byte[]? Rgba;
        public GCHandle Pin;
        public nint Ptr;
        public StripInfo Info;

        /// <summary>D-13's source, mirrored from <c>ClipLayout.PosterPath</c>. Carried
        /// OPAQUE: it arrives from untrusted-shaped mirror JSON and the decoder is the one
        /// that must bound what it does with it (T-53.2-02 / T-53.2-26).</summary>
        public string? PosterPath;

        /// <summary>Set on the FIRST decode outcome of any kind. The whole of
        /// "attempted once per media".</summary>
        public bool PosterAttempted;

        public bool PosterInFlight;

        public long LastDrawnTick;

        public bool Wanted;
        public bool GivenUp;
        public bool Complete;
        public bool InFlight;
        public int ConsecutiveNulls;
        public long NextAttemptMs;

        /// <summary>Take ownership of a new buffer, <b>releasing the previous pin
        /// first</b>. Every replacement in this class goes through here: placeholder
        /// adoption, a strip replacing a placeholder, and each partial upgrade. A partial
        /// upgrade runs it repeatedly for one id, which is what makes the ordering a
        /// correctness requirement rather than tidiness.</summary>
        public void Adopt(byte[] bytes, in StripInfo info)
        {
            if (Pin.IsAllocated)
            {
                Pin.Free();
            }

            Rgba = bytes;
            Pin = GCHandle.Alloc(bytes, GCHandleType.Pinned);
            Ptr = Pin.AddrOfPinnedObject();
            Info = info;
        }

        public void Release()
        {
            if (Pin.IsAllocated)
            {
                Pin.Free();
            }

            Rgba = null;
            Ptr = nint.Zero;
        }
    }
}

/// <summary>
/// The production <see cref="IPosterDecoder"/>: WinRT imaging (WIC underneath), bounded,
/// off the calling thread, and silent about every failure.
///
/// <para><b>Not WinUI.</b> <c>Windows.Graphics.Imaging</c> is the OS's codec surface — no
/// visual tree, no dispatcher, no XAML, no window — which is why this file still belongs
/// in the WinUI-free <c>Regions/Timeline/</c> directory and why
/// <c>TimelineHotPathGateTests</c>' forbidden vocabulary does not name it. The MediaBin's
/// poster path was checked first for something reusable (<c>MediaBinPoster</c>) and it is
/// POLICY ONLY — <c>ShouldAttemptLoad</c> and <c>DecodePixelWidth</c>; the actual decode
/// over there is a WinUI <c>BitmapImage</c> inside <c>MediaBin.xaml.cs</c>, which needs a
/// visual tree and cannot be reused here. <c>ShouldAttemptLoad</c>'s existence check IS
/// reused, in spirit and in the same three cases it names.</para>
///
/// <para><b>Bounded twice.</b> The file's own length is checked against
/// <see cref="FilmstripCache.MaxStripBytes"/> from its metadata before a byte is read,
/// and the decode itself is scaled to fit the caller's box with the source aspect
/// preserved — so an 8K poster costs one scaled decode, not a full-resolution one.
/// <c>MediaBinPosterTests</c> records why the second bound is not a nicety: an unbounded
/// poster decode has no visible symptom until a real library is imported.</para>
/// </summary>
internal sealed class WicPosterDecoder : IPosterDecoder
{
    /// <summary>Stateless, so one instance serves every cache.</summary>
    internal static readonly WicPosterDecoder Shared = new();

    public Task<PosterBitmap?> DecodeAsync(string path, int maxW, int maxH) =>
        DecodeCoreAsync(path, maxW, maxH);

    private static async Task<PosterBitmap?> DecodeCoreAsync(string path, int maxW, int maxH)
    {
        if (string.IsNullOrWhiteSpace(path) || maxW <= 0 || maxH <= 0)
        {
            return null;
        }

        byte[] fileBytes;
        try
        {
            // The same three cases MediaBinPoster.ShouldAttemptLoad names, and they are
            // all NORMAL rather than errors (53 D-08): audio never gets a poster, a
            // video's poster generation is logged-and-continued rather than fatal, and a
            // relink or a cleared cache leaves a stored path pointing at nothing.
            var info = new FileInfo(path);
            if (!info.Exists || info.Length <= 0 || info.Length > FilmstripCache.MaxStripBytes)
            {
                return null;
            }

            fileBytes = await File.ReadAllBytesAsync(path).ConfigureAwait(false);
        }
        catch (Exception)
        {
            // ArgumentException / PathTooLongException / IOException /
            // UnauthorizedAccessException / SecurityException, and anything a future
            // filesystem shim adds. Every one of them means "no placeholder", and the
            // caller records the attempt either way so there is no retry storm.
            return null;
        }

        try
        {
            using var stream = new InMemoryRandomAccessStream();
            using (var writer = new DataWriter(stream))
            {
                writer.WriteBytes(fileBytes);
                await writer.StoreAsync();
                writer.DetachStream();
            }

            stream.Seek(0);

            var decoder = await BitmapDecoder.CreateAsync(stream);

            var srcW = (int)decoder.PixelWidth;
            var srcH = (int)decoder.PixelHeight;
            if (srcW <= 0 || srcH <= 0)
            {
                return null;
            }

            var (dstW, dstH) = Fit(srcW, srcH, maxW, maxH);

            var transform = new BitmapTransform
            {
                ScaledWidth = (uint)dstW,
                ScaledHeight = (uint)dstH,
                InterpolationMode = BitmapInterpolationMode.Fant,
            };

            var pixels = await decoder.GetPixelDataAsync(
                BitmapPixelFormat.Rgba8,
                BitmapAlphaMode.Straight,
                transform,
                ExifOrientationMode.RespectExifOrientation,
                ColorManagementMode.DoNotColorManage);

            var rgba = pixels.DetachPixelData();
            if (rgba is null || rgba.LongLength != (long)dstW * 4L * dstH)
            {
                // The transform is a REQUEST; a codec is free to honour it approximately.
                // Rather than guess which dimension moved, refuse — the caller's answer to
                // a null is the same as its answer to a corrupt file.
                return null;
            }

            return new PosterBitmap(rgba, dstW, dstH);
        }
        catch (Exception)
        {
            // T-53.2-26. Every WinRT failure — unsupported codec, truncated file, a
            // corrupt PNG crafted to fault the decoder — arrives here as an exception
            // carrying an HRESULT, and every one of them means "no placeholder".
            return null;
        }
    }

    /// <summary>Largest box inside <paramref name="maxW"/> x <paramref name="maxH"/> that
    /// preserves the source aspect, never upscaling and never returning a zero dimension
    /// (a 4000x1 panorama scaled to fit 256x144 rounds its height to zero, and a
    /// zero-dimension decode request is a failure rather than a small picture).</summary>
    internal static (int Width, int Height) Fit(int srcW, int srcH, int maxW, int maxH)
    {
        if (srcW <= maxW && srcH <= maxH)
        {
            return (srcW, srcH);
        }

        var scale = Math.Min((double)maxW / srcW, (double)maxH / srcH);
        var w = (int)Math.Round(srcW * scale, MidpointRounding.AwayFromZero);
        var h = (int)Math.Round(srcH * scale, MidpointRounding.AwayFromZero);

        return (Math.Clamp(w, 1, maxW), Math.Clamp(h, 1, maxH));
    }
}
