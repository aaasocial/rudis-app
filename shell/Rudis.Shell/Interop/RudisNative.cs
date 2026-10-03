using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Rudis.Shell.Interop;

/// <summary>
/// The threading-policy-enforcing facade over the ABI exports this shell
/// declares. The count is stated as a RATIO in exactly ONE place -- see
/// <see cref="NativeMethods"/> -- so a gap between the two sides of the ABI
/// stays visible rather than being repeated in four places that drift apart.
/// The policy is
/// 50-02's, implemented STRUCTURALLY (artifacts/50-02-unknowns.md §1.5/§1.6):
///
/// <list type="bullet">
/// <item><b>The two thread-free members</b> — <see cref="GetPlaybackPosition"/>
///   (a pure Relaxed atomic load, commands.rs:398) and, since Phase 51,
///   <see cref="ResizePreview"/> / <see cref="TryGetPreviewContentRect"/> (three
///   relaxed stores plus a release-ordered dirty flag, and four relaxed loads,
///   respectively). Callable from ANY thread: no lock, no allocation, no
///   JSON.</item>
/// <item><b>The panel-affine pair</b> — <see cref="AttachPreviewPanel"/> and
///   <see cref="DetachPreviewPanel"/> run SYNCHRONOUSLY on the caller's own
///   thread, and MUST be the panel's UI thread. Queueing them would fail every
///   attach; see their own remarks for the specific COM failure that makes this a
///   correctness requirement rather than an optimisation (Phase 51, D-07).</item>
/// <item><b>Everything else</b> — EVERY remaining ctx export (plus the ctx-free
///   probe, for uniformity) run ONLY on ONE dedicated interop worker thread via a
///   single-consumer queue, so no two ABI calls ever run concurrently. The deciding
///   evidence: <c>rudis_transport</c>'s host half publishes mirror/ring OUTSIDE the
///   store lock (commands.rs:296-305) — concurrent callers could invert apply order
///   vs publish order (T-50-12).</item>
/// <item><b>Export blocks the worker, never the UI thread</b> (50-02 §1.5(3)):
///   while an encode is in flight the worker cannot service the event poll; the
///   bounded ring + resync protocol absorbs that by design.</item>
/// <item><b>Lifecycle</b> — <c>rudis_init</c> runs on the constructing thread
///   BEFORE the worker starts (thread-start is a happens-before edge, so the
///   no-concurrent-calls invariant holds trivially); the shutdown release runs only
///   after the worker has drained and joined (<see cref="Dispose"/>), so it can
///   never race an in-flight command (50-02 §1.5(5)).</item>
/// <item><b>D-08</b> — callers <c>await</c> the returned Tasks; no Task is ever
///   blocked on synchronously in this codebase. The teardown join is a
///   <see cref="Thread.Join()"/> on a drained worker, bounded by design.</item>
/// </list>
///
/// The raw ctx pointer never leaves <see cref="RudisCtxHandle"/> (T-47-04).
///
/// <para><b>CR-01 (50-REVIEW.md): the serialization policy is enforced by the
/// compiler, not by a doc comment.</b> <see cref="NativeMethods"/> is a `private`
/// class nested INSIDE this one — not a top-level `internal` type — so no other
/// file in the <c>Rudis.Shell</c> assembly can name it, let alone call
/// <c>NativeMethods.rudis_transport(...)</c> or any other serialized export
/// directly on the UI thread. The three reaches around this class's own async
/// surface are all narrow and explicit: <see cref="Shutdown"/> (SafeHandle's
/// guaranteed-at-most-once release, which must run OUTSIDE the queue — see
/// <see cref="RudisCtxHandle.ReleaseHandle"/>), <see cref="TestSupport"/> (the
/// three primitives that were never subject to the serialization policy in the
/// first place, exposed for `Rudis.Shell.Tests`' white-box interop tests), and the
/// four panel members below. Each has its own remarks naming the reason it exists;
/// none of them touches any of the 23 serialized command exports.</para>
/// </summary>
internal sealed partial class RudisNative : IDisposable
{
    private readonly RudisCtxHandle _handle;
    private readonly BlockingCollection<Action>? _queue;
    private readonly Thread? _worker;
    private int _disposed;

    /// <summary>True when <c>rudis_init</c> returned null — a DESIGNED outcome for
    /// malformed config JSON (lib.rs:191-201). Check it; never assume (T-50-13).</summary>
    public bool IsInvalid => _handle.IsInvalid;

    /// <summary>
    /// Create an engine instance. Null/empty config = engine defaults (per-instance
    /// temp dirs). Never throws on a bad config — inspect <see cref="IsInvalid"/>.
    /// Runs <c>rudis_init</c> on the calling thread: the worker does not exist yet,
    /// so no concurrent ABI call is possible, and the worker's later start
    /// establishes the happens-before edge that hands the ctx over.
    /// </summary>
    public static RudisNative Create(string? initConfigJson)
    {
        RudisCtxHandle handle;
        if (string.IsNullOrEmpty(initConfigJson))
        {
            handle = NativeMethods.rudis_init(nint.Zero, 0);
        }
        else
        {
            var bytes = Encoding.UTF8.GetBytes(initConfigJson);
            unsafe
            {
                fixed (byte* p = bytes)
                {
                    handle = NativeMethods.rudis_init((nint)p, (nuint)bytes.Length);
                }
            }
        }
        return new RudisNative(handle);
    }

    private RudisNative(RudisCtxHandle handle)
    {
        _handle = handle;
        if (!handle.IsInvalid)
        {
            _queue = new BlockingCollection<Action>();
            _worker = new Thread(WorkerLoop)
            {
                IsBackground = true,
                Name = "rudis-interop",
            };
            _worker.Start();
        }
    }

    private void WorkerLoop()
    {
        foreach (var work in _queue!.GetConsumingEnumerable())
        {
            work();
        }
    }

    // ── HOT PATH ────────────────────────────────────────────────────────────

    /// <summary>
    /// The one thread-free member (50-02 §1.5(1)): callable per composition tick
    /// from any thread, zero allocation on the call itself. Returns null — never a
    /// position — for the <c>i64::MIN</c> sentinel (null handle / caught panic), an
    /// invalid handle, or a disposed wrapper.
    /// </summary>
    public long? GetPlaybackPosition()
    {
        if (Volatile.Read(ref _disposed) != 0 || _handle.IsClosed || _handle.IsInvalid)
        {
            return null;
        }
        long v;
        try
        {
            v = NativeMethods.rudis_get_playback_position(_handle);
        }
        catch (ObjectDisposedException)
        {
            // Teardown race: Dispose closed the handle between the guard above and
            // the marshal — the marshaller's AddRef refused the closed handle.
            // A fault signal, never a position.
            return null;
        }
        return v == long.MinValue ? null : v;
    }

    // ── cold path: every other export, serialised on the ONE worker ─────────

    /// <summary>Ctx-free symbol/buffer smoke test; <c>{"Ok": "rudis-ffi-phase47-probe-v1"}</c>.</summary>
    public Task<RudisResult<JsonElement>> AbiProbeAsync()
        => RunOut(static (RudisCtxHandle _, out RudisBuffer b) => NativeMethods.rudis_abi_probe(out b));

    public Task<RudisResult<JsonElement>> GetSnapshotAsync()
        => RunOut(NativeMethods.rudis_get_snapshot);

    public Task<RudisResult<JsonElement>> GetEntitiesAsync(string argsJson)
        => RunJson(NativeMethods.rudis_get_entities, argsJson);

    public Task<RudisResult<JsonElement>> GetCurrentSeqAsync()
        => RunOut(NativeMethods.rudis_get_current_seq);

    /// <summary>
    /// Args <c>{"media_id": ".."}</c>; <c>{"Ok": {"block_us", "sample_rate",
    /// "peak_count", "peaks_b64"}}</c> on a hit and <c>{"Ok": null}</c> for every
    /// miss — cache miss, not-yet-extracted, no audio and unknown id are all the
    /// SAME answer by design (52-CONTEXT D-21), because distinguishing them would
    /// require the read to know something only a decode could tell it.
    ///
    /// <para><b>A pure cache READ. It never triggers computation</b>
    /// (<c>app_core::read_peaks</c> reaches the store, the cache key and the cache
    /// file, and has no path to a decoder at all). Peaks are produced by a
    /// background job at IMPORT time.</para>
    ///
    /// <para><b>Never call this from anywhere reachable from a paint or a
    /// present.</b> That is 52-RESEARCH Pitfall 7, and it is the one shape
    /// criterion 5 forbids in so many words. Its single caller is
    /// <c>Timeline.OnColdPollAsync</c>, on Phase 50 D-06's existing 100ms cold
    /// cycle, feeding a client-side cache the render loop only ever reads. The
    /// paint-scope guard in <see cref="Enqueue"/> counts any violation of that
    /// rule for whoever comes next.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> GetWaveformPeaksAsync(string argsJson)
        => RunJson(NativeMethods.rudis_get_waveform_peaks, argsJson);

    /// <summary>
    /// 53.2 D-12 — a PURE cache read; <c>{"Ok":null}</c> covers miss / not-yet /
    /// no-video-frames / unknown-id alike (D-15); the payload's
    /// <c>completed_tiles</c> vs <c>total_tiles</c> pair is D-14's partial/complete
    /// distinction. NEVER call from the render tick — <c>TimelinePaintScopeTests</c>
    /// bans this name from the per-frame path (extended in 53.2-07).
    ///
    /// <para>Args <c>{"media_id": ".."}</c>, written by <c>System.Text.Json</c>'s
    /// writer rather than by interpolation (T-52-34's rule, which
    /// <c>Timeline.BuildPeaksArgs</c> already applies to the peaks payload). This
    /// wrapper takes the media id ITSELF rather than a pre-serialised args string —
    /// the ONE deliberate difference from <see cref="GetWaveformPeaksAsync"/> — so
    /// that <c>FilmstripCache.PumpAsync</c>'s injected fetch delegate, whose
    /// signature is <c>Func&lt;string, Task&lt;RudisResult&lt;JsonElement&gt;&gt;&gt;</c>,
    /// binds to it directly and no second JSON-writing site can drift from this one.</para>
    ///
    /// <para><b>A pure cache READ. It never triggers computation</b>
    /// (<c>app_core::filmstrip_job::read_strip</c> reaches the store, the cache key and
    /// the cache file, and has no path to a decoder at all — 53.2-04). Strips are
    /// produced by a detached background job at IMPORT time, gated on
    /// <c>MediaKind::Video</c>.</para>
    ///
    /// <para><b>Never call this from anywhere reachable from a paint or a
    /// present.</b> Its single intended caller is the Timeline's cold poll on Phase 50
    /// D-06's existing 100ms cycle, feeding <c>FilmstripCache</c> — a client-side cache
    /// the render loop only ever READS. The paint-scope guard in <see cref="Enqueue"/>
    /// counts any violation; the source scan in <c>TimelinePaintScopeTests</c> is the
    /// complementary half, and plan 53.2-07 adds this method's name to its
    /// <c>AbiSymbols</c> list so the two agree.</para>
    ///
    /// <para>The payload's nine fields are a WIRE CONTRACT, mirrored field for field by
    /// <see cref="FilmstripStripPayload"/>; <c>crates/app-core/src/filmstrip_job.rs</c>'s
    /// <c>StripPayload</c> is the other side and renaming one without the other is an
    /// ABI break with no compiler to catch it.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> GetFilmstripStripAsync(string mediaId)
        => RunJson(NativeMethods.rudis_get_filmstrip_strip, BuildMediaIdArgs(mediaId));

    /// <summary>
    /// One media item's proxy state (Phase 58 / PROXY-02; the CALLER is plan 63-04's,
    /// TRUST-03). <c>{"Ok":{"state":".."}}</c> where state is one of <c>queued</c>,
    /// <c>running</c>, <c>ready</c>, <c>failed</c>, <c>cancelled</c>, <c>not_needed</c>
    /// or <c>none</c>; <c>{"Ok":null}</c> for a miss or an unknown id.
    ///
    /// <para><b>A pure READ. It never triggers computation</b> — the export reaches the
    /// job registry, the cache key and the cache file, and has no path to an encoder at
    /// all. Proxies are produced by a detached background job at IMPORT time
    /// (<c>import.rs</c> then <c>proxy_job::spawn_generation</c>) and re-armed at project
    /// open.</para>
    ///
    /// <para>Takes the media id ITSELF rather than a pre-serialised args string, like
    /// <see cref="GetFilmstripStripAsync"/> and for the same reason: the JSON is written
    /// by <c>System.Text.Json</c>'s writer (T-52-34) at ONE site, so no second
    /// JSON-writing call site can drift from this one.</para>
    ///
    /// <para><b>Never call this from anywhere reachable from a paint or a present.</b>
    /// Its single intended caller is <c>MediaBin.OnColdPollAsync</c>, on Phase 50 D-06's
    /// existing 100 ms cycle, and only for items whose last known state is NON-TERMINAL —
    /// a settled bin costs ZERO calls per tick (T-63-13). The paint-scope guard in
    /// <see cref="Enqueue"/> counts any violation.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> GetProxyStatusAsync(string mediaId)
        => RunJson(NativeMethods.rudis_get_proxy_status, BuildMediaIdArgs(mediaId));

    /// <summary>
    /// The timeline render cache's PROGRAM-level state (Phase 59 / CACHE-01; the CALLER is
    /// plan 63-04's). Always <c>{"Ok": {"state":"none|idle|rendering",
    /// "rendering_segment": i64, "cached_segments": u64, "heavy_segments": u64,
    /// "idle_spawned": u64}}</c> — never <c>{"Ok":null}</c>, and <c>rendering_segment</c>
    /// is <c>-1</c> rather than null so it deserialises into a plain <c>long</c>.
    ///
    /// <para><b>It NEVER triggers a render.</b> Segments are produced by the detached
    /// <c>app_core::render_cache_job</c> that the transport and project-open funnels
    /// start; there is no code path from this read to an encoder, and a Rust test polls it
    /// a hundred times against a heavy section to keep it that way.</para>
    ///
    /// <para>No args, deliberately: the render cache is a property of the PROGRAM, not of
    /// a media item, so there is no id to pass and no opaque-id surface to defend.</para>
    ///
    /// <para><b>Never call this from a paint or a present.</b> Its single intended caller
    /// is <c>Transport.OnColdPollAsync</c>, on the same 100 ms cold cycle, single-flighted
    /// like every other rider.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> GetRenderCacheStatusAsync()
        => RunOut(NativeMethods.rudis_get_render_cache_status);

    /// <summary>
    /// <c>rudis_preview_device_status</c> -- the preview surface's device health and its
    /// present counter (Phase 63 / TRUST-01). Always
    /// <c>{"Ok": {"lost":bool, "recovering":bool, "recovered":u64, "presented":u64,
    /// "attach_epoch":u64}}</c>, never <c>{"Ok":null}</c> and never <c>{"Err": ..}</c>: an
    /// unattached ctx honestly answers all-zeroes, because "no panel has ever been
    /// attached" is a state and not a fault.
    ///
    /// <para><b><c>presented</c> is the field that matters.</b> It is a monotonic count of
    /// SUCCESSFUL surface presents, so it FREEZES the instant a device loss stops the
    /// preview and RESUMES climbing the instant recovery lands -- which is what makes
    /// "the preview came back" a VALUE a test can read rather than an impression
    /// (63-CONTEXT D-11).</para>
    ///
    /// <para>Five relaxed atomic loads on the Rust side; it never computes and never takes
    /// the GPU lock, so it cannot queue behind an in-flight composite (threat T-63-06).
    /// Its intended caller is <c>Preview</c>'s own single-flight cold poll -- the SIXTH
    /// rider on the poll-don't-push pattern, and the reason <c>ring::EVENT_NAMES</c> is
    /// still 6.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> GetPreviewDeviceStatusAsync()
        => RunOut(NativeMethods.rudis_preview_device_status);

    /// <summary>
    /// <c>rudis_preview_simulate_device_lost</c> -- <b>really calls
    /// <c>ID3D12Device5::RemoveDevice</c> on the live preview device.</b> This is not a
    /// simulation of a device loss; it is a device loss.
    ///
    /// <para><b>Gated twice, on purpose.</b> The Rust export refuses with a DOMAIN error
    /// unless <c>RUDIS_DEBUG_DEVICE_LOSS=1</c> was in the environment when the process
    /// started (threat T-63-04, fail-closed and latched at first attach), and every C#
    /// call site is <c>#if DEBUG</c>, so a Release build does not contain one. A paid
    /// build must not ship a reachable "remove my GPU" lever.</para>
    ///
    /// <para>Its only purpose is to make <c>TRUST-01</c> provable on demand: without it,
    /// proving that a real TDR recovers means waiting for a real TDR.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> SimulatePreviewDeviceLostAsync()
        => RunOut(NativeMethods.rudis_preview_simulate_device_lost);

    /// <summary><c>{"media_id": ".."}</c> through the JSON WRITER, never through
    /// interpolation (T-52-34). A media id is backend-minted today; the rule exists so
    /// that the day one is not, the payload cannot be steered by its content.</summary>
    private static string BuildMediaIdArgs(string mediaId)
    {
        var buffer = new System.Buffers.ArrayBufferWriter<byte>(64);
        using (var writer = new Utf8JsonWriter(buffer))
        {
            writer.WriteStartObject();
            writer.WriteString("media_id", mediaId ?? string.Empty);
            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(buffer.WrittenSpan);
    }

    public Task<RudisResult<JsonElement>> PollEventsAsync(ulong localSeq)
        => RunOut((RudisCtxHandle c, out RudisBuffer b) => NativeMethods.rudis_poll_events(c, localSeq, out b));

    public Task<RudisResult<JsonElement>> DebugMarkInteractiveAsync()
        => RunOut(NativeMethods.rudis_debug_mark_interactive);

    /// <summary>DEBUG-GATED (env <c>RUDIS_DEBUG_SEED_PROJECT</c>) whole-project seed —
    /// Phase 54's live-eval harness is the ONLY intended caller (54-CONTEXT D-12/D-13).
    /// Args <c>{"project": {..}}</c>; <c>{"Ok": null}</c>, or the disabled-refusal domain
    /// error <c>"debug project seeding is disabled (set RUDIS_DEBUG_SEED_PROJECT)"</c>
    /// when ungated (a DOMAIN error — transport status stays Ok, per D-06).
    ///
    /// <para><b>The shell itself NEVER calls this.</b> A mirror-attached caller would
    /// desync until its next full resync, because the export emits nothing into the
    /// event ring and calls no <c>observe_preview_patch</c>, BY DESIGN. It also replaces
    /// the store wholesale (<c>Store::from_project</c>), discarding the open project and
    /// its undo stack — which is precisely why the gate exists and why no production
    /// <c>open_project</c>/<c>new_project</c> pair was shipped in its place (Phase 55's
    /// problem). 54-02's contract test 16 proves a refused call leaves the store
    /// BYTE-unchanged, not merely that it errored.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> DebugSeedProjectAsync(string argsJson)
        => RunJson(NativeMethods.rudis_debug_seed_project, argsJson);

    public Task<RudisResult<JsonElement>> UndoAsync()
        => RunOut(NativeMethods.rudis_undo);

    public Task<RudisResult<JsonElement>> RedoAsync()
        => RunOut(NativeMethods.rudis_redo);

    public Task<RudisResult<JsonElement>> PlaceClipAsync(string argsJson)
        => RunJson(NativeMethods.rudis_place_clip, argsJson);

    public Task<RudisResult<JsonElement>> ApplyOptionCardAsync(string argsJson)
        => RunJson(NativeMethods.rudis_apply_option_card, argsJson);

    public Task<RudisResult<JsonElement>> TransportAsync(string argsJson)
        => RunJson(NativeMethods.rudis_transport, argsJson);

    public Task<RudisResult<JsonElement>> AgentStatusAsync()
        => RunOut(NativeMethods.rudis_agent_status);

    public Task<RudisResult<JsonElement>> SetApiKeyAsync(string argsJson)
        => RunJson(NativeMethods.rudis_set_api_key, argsJson);

    public Task<RudisResult<JsonElement>> ClearApiKeyAsync()
        => RunOut(NativeMethods.rudis_clear_api_key);

    /// <summary>Settings' non-Anthropic key path (Phase 69, D-69-12). Args
    /// <c>{"provider":"runway","key":".."}</c>; <c>{"Ok": null}</c> or <c>{"Err": "&lt;rule text&gt;"}</c>.
    /// Inbound only — the key never comes back.</summary>
    public Task<RudisResult<JsonElement>> SetProviderKeyAsync(string argsJson)
        => RunJson(NativeMethods.rudis_set_provider_key, argsJson);

    /// <summary>Args <c>{"provider":"runway"}</c>; idempotent, <c>{"Ok": null}</c> (D-69-12).</summary>
    public Task<RudisResult<JsonElement>> ClearProviderKeyAsync(string argsJson)
        => RunJson(NativeMethods.rudis_clear_provider_key, argsJson);

    public Task<RudisResult<JsonElement>> ImportMediaAsync(string argsJson)
        => RunJson(NativeMethods.rudis_import_media, argsJson);

    public Task<RudisResult<JsonElement>> ImportMediaFolderAsync(string argsJson)
        => RunJson(NativeMethods.rudis_import_media_folder, argsJson);

    /// <summary>Blocks the interop worker (never the UI thread) for the whole
    /// encode; event polls queue behind it and drain afterwards (50-02 §1.5(3)).</summary>
    public Task<RudisResult<JsonElement>> ExportTimelineAsync(string argsJson)
        => RunJson(NativeMethods.rudis_export_timeline, argsJson);

    public Task<RudisResult<JsonElement>> AgentSendMessageAsync(string argsJson)
        => RunJson(NativeMethods.rudis_agent_send_message, argsJson);

    public Task<RudisResult<JsonElement>> DispatchCommandAsync(string argsJson)
        => RunJson(NativeMethods.rudis_dispatch_command, argsJson);

    // ── the project lifecycle (Phase 60.1, plan 60.1-04) ────────────────────
    //
    // The seven wrappers wave 5's region plans call. Every one is a plain
    // `RunJson`/`RunOut` over the ONE interop worker — no bespoke marshalling is
    // added anywhere in this block, so `ReadUtf8AndFree` still pairs every
    // returned buffer with its `rudis_free_buffer` structurally (T-60.1-13).
    //
    // ⚠ THE ONE ASYMMETRY IN THE SET, and the thing to know before writing a
    // caller: `NewProjectAsync` and `OpenProjectAsync` hand back a
    // `JsonValueKind.String` — agent-facing PROSE, unchanged since Phase 26,
    // because `run_new_project`/`run_open_project` are also agent tools and plan
    // 60.1-03 gave them a host rather than a new contract. The other five hand
    // back an Object or an Array. A region that calls `.GetProperty("name")` on
    // the first two gets an `InvalidOperationException`, not a null.
    //
    // ⚠ AND THE ONE THAT BITES LATER: the `path` these return is NOT one uniform
    // form. `OpenProjectAtPathAsync` and `SaveProjectAsAsync` go through
    // validated newtypes that CANONICALISE, which on Windows is the
    // extended-length `\\?\C:\...` form, while `GetProjectsAsync` always reports
    // the plain form its directory scan produced. The two forms name the same
    // file and are NOT equal as strings. Shorten for display; canonicalise BOTH
    // SIDES before comparing. Never compare them raw — that mistake is already
    // logged as D-60.1-04, and `ProjectPersistenceTests` pins both halves.

    // ── THE PERSISTED-SEQ CHOKEPOINT (plan 60.1-06) ─────────────────────
    //
    // WHY IT LIVES HERE rather than in the window that renders the dot. Every save
    // in this shell reaches `rudis_save_project` / `rudis_save_project_as` through
    // the two wrappers below, and it can reach them NO OTHER WAY: `NativeMethods` is
    // a private nested class and `MechanicalGatesTests.native_methods_is_unreachable_
    // outside_rudis_native` fails the build on any attempt to name it elsewhere. So
    // this is a proven chokepoint, not a convention - a save site added later cannot
    // forget to report, because there is nowhere else for it to go.
    //
    // The alternative considered and rejected: a static callback the window installs
    // and each region invokes (the `App.RequestMirrorPollNow` shape). It works, and a
    // third save site would silently skip it. The dot is exactly the surface that
    // must not quietly stop being true.

    private long _lastPersistedStoreSeq;

    /// <summary>
    /// The store's mutation counter at the last successful persist through THIS
    /// handle, and 0 when the active document was just opened or created.
    ///
    /// <para>Consumed by <c>UnsavedIndicator.HasUnpersistedEdits</c>, which compares it
    /// against the mirror's <c>LastAppliedStoreSeq</c>. It is an OBSERVATION of the
    /// engine, recorded where the observation is available, and carries no policy of
    /// its own.</para>
    ///
    /// <para><c>0</c> after an open or a create is correct by construction rather than
    /// by convention: <c>Store::from_project</c> resets the counter to 0, and both
    /// routes leave the in-memory project equal to what is on disk (open loaded it;
    /// create wrote it). <c>run_save_project</c>'s own doc says the same thing - "after
    /// any open or new the store's seq is 0 with the project already on disk, so
    /// nothing unsaved is true by construction rather than by a flag somebody has to
    /// remember to clear".</para>
    ///
    /// <para>Written from the interop continuation and read from the UI thread, so both
    /// sides go through <see cref="Volatile"/>. A <c>long</c> field rather than a
    /// <c>ulong</c> one because <c>Volatile</c> has no unsigned 64-bit overload; the
    /// cast is lossless in both directions.</para>
    /// </summary>
    internal ulong LastPersistedStoreSeq => (ulong)Volatile.Read(ref _lastPersistedStoreSeq);

    /// <summary>
    /// Record the seq the save's own envelope reported. A refusal or a transport fault
    /// changes nothing: the last thing actually written is still the last thing
    /// actually written, and moving the number on a failed save would blank the dot
    /// over work that never reached the disk.
    /// </summary>
    private async Task<RudisResult<JsonElement>> NotePersistedAsync(
        Task<RudisResult<JsonElement>> pending)
    {
        var result = await pending.ConfigureAwait(false);

        if (result.Kind == RudisResultKind.Ok &&
            result.Value.ValueKind == JsonValueKind.Object &&
            result.Value.TryGetProperty("seq", out var seq) &&
            seq.TryGetUInt64(out var persisted))
        {
            Volatile.Write(ref _lastPersistedStoreSeq, (long)persisted);
        }

        return result;
    }

    /// <summary>
    /// Reset on a successful project switch. See <see cref="LastPersistedStoreSeq"/>
    /// for why 0 is the truth here and not a guess.
    /// </summary>
    private async Task<RudisResult<JsonElement>> NoteSwitchedAsync(
        Task<RudisResult<JsonElement>> pending)
    {
        var result = await pending.ConfigureAwait(false);

        if (result.Kind == RudisResultKind.Ok)
        {
            Volatile.Write(ref _lastPersistedStoreSeq, 0L);
        }

        return result;
    }

    /// <summary>
    /// <c>rudis_new_project</c> — create a brand-new, empty, NAMED project and
    /// switch to it. Args <c>{"name": ".."}</c>; envelope
    /// <c>{"Ok": "&lt;prose&gt;"}</c> — ⚠ a STRING, not an object.
    ///
    /// <para>A reserved Windows device name, a path separator or an over-long
    /// name comes back as a DOMAIN error (transport status stays <c>Ok</c>) whose
    /// message is text a dialog can print: the name goes through
    /// <c>sanitize_project_name</c> before any path join (T-26-01).</para>
    ///
    /// <para>Side effects to expect, because they are the point: the OUTGOING
    /// project is autosaved first, the new one is written to
    /// <c>&lt;app-data&gt;/projects/&lt;name&gt;.rud</c> immediately, undo/redo
    /// resets, and ONE <c>project:changed</c> record lands in the ring carrying
    /// the structural <c>ProjectSwitched</c> patch — which
    /// <c>ShellMirror.ApplyAsync</c> already routes to a full resync. There is no
    /// seventh event type for any of this.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> NewProjectAsync(string argsJson)
        => NoteSwitchedAsync(RunJson(NativeMethods.rudis_new_project, argsJson));

    /// <summary>
    /// <c>rudis_open_project</c> — switch to a previously created project **by
    /// name**. Args <c>{"name": ".."}</c>; envelope
    /// <c>{"Ok": "&lt;prose&gt;"}</c> — ⚠ a STRING, not an object — or the domain
    /// refusal <c>"no known project named \"..\" — call get_projects to see what
    /// exists"</c>.
    ///
    /// <para>⚠ The name is a LOOKUP KEY, never a path component: the backend
    /// resolves it exclusively through the on-disk registry and never joins
    /// caller text into a path (T-26-03). A user who picked a FILE from a dialog
    /// wants <see cref="OpenProjectAtPathAsync"/> instead — a separate export
    /// over a separate function taking a separate type, which is what keeps this
    /// route narrow.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> OpenProjectAsync(string argsJson)
        => NoteSwitchedAsync(RunJson(NativeMethods.rudis_open_project, argsJson));

    /// <summary>
    /// <c>rudis_open_project_at_path</c> — open a <c>.rud</c> the USER picked,
    /// from anywhere on disk. Args <c>{"path": ".."}</c>; envelope
    /// <c>{"Ok": {"name": "..", "path": ".."}}</c>, where <c>name</c> comes from
    /// the LOADED document rather than the file stem (a user who renamed the file
    /// in Explorer has not renamed the project inside it).
    ///
    /// <para><b>⚠ BUILD THE ARGUMENT WITH <c>System.Text.Json</c>
    /// (<c>JsonObject</c>), NEVER WITH STRING INTERPOLATION (T-52-34).</b> This
    /// is one of the two wrappers whose argument is a PATH, and a Windows path is
    /// full of backslashes — it is precisely the value that turns a hand-built
    /// <c>$"{{\"path\":\"{p}\"}}"</c> into either invalid JSON or a different path
    /// than the user chose. Call <c>Path.GetFullPath</c> on the value before it
    /// crosses the ABI (T-51-22).</para>
    ///
    /// <para>Every refusal — a directory, a non-<c>.rud</c>, a file that is not
    /// there, a file over the size cap — arrives as a DOMAIN error with transport
    /// status <c>Ok</c>, because the validation is a property of the argument's
    /// TYPE on the Rust side. That is exactly what a file dialog needs in order
    /// to say something useful. ⚠ The raw string carries serde's machine noise
    /// (<c>"invalid arguments: "</c> ... <c>" at line 1 column N"</c>) around the
    /// human sentence — log it verbatim, trim it for display (D-60.1-05).</para>
    ///
    /// <para>⚠ The returned <c>path</c> is CANONICALISED — on Windows the
    /// extended-length <c>\\?\C:\..</c> form. See this region's header.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> OpenProjectAtPathAsync(string argsJson)
        => NoteSwitchedAsync(RunJson(NativeMethods.rudis_open_project_at_path, argsJson));

    /// <summary>
    /// <c>rudis_save_project</c> — persist the LIVE store to the active project's
    /// file. No args; envelope <c>{"Ok": {"path": "..", "seq": N}}</c>.
    ///
    /// <para><b>It MINTS rather than refusing when nothing is active.</b> With no
    /// active project this does not fail: it creates
    /// <c>&lt;app-data&gt;/projects/Untitled.rud</c> (then <c>Untitled 2</c>,
    /// <c>Untitled 3</c>, … — there is no <c>Untitled 1</c>), renames the live
    /// document to match and writes. A modal asking a beginner to name a file
    /// BEFORE their work is safe is the moment the work gets lost. This is what
    /// lets a close path have no dialog and no branch at all: close always
    /// saves.</para>
    ///
    /// <para><c>seq</c> is the store's mutation counter captured in the SAME
    /// guard as the bytes (LAT-02), so a host can tell whether the store has
    /// moved since. There is deliberately no separate "is dirty" export: after
    /// any open or new the seq is 0 with the project already on disk, so
    /// "nothing unsaved" is true by construction rather than by a flag someone
    /// has to remember to clear.</para>
    ///
    /// <para>Safe on the close path: <see cref="Dispose"/> completes the queue
    /// and JOINS the worker before the SafeHandle closes, so work ENQUEUED first
    /// drains before shutdown. Enqueue, <c>await</c>, then close — never the
    /// other way round.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> SaveProjectAsync()
        => NotePersistedAsync(RunOut(NativeMethods.rudis_save_project));

    /// <summary>
    /// <c>rudis_save_project_as</c> — persist to a user-chosen path AND re-point
    /// the active document there. Args <c>{"path": ".."}</c>; envelope
    /// <c>{"Ok": {"name": "..", "path": "..", "seq": N}}</c>, where <c>name</c>
    /// is the target's file stem: after Save As the TitleBar and the next
    /// registry scan agree with the filename the user just typed.
    ///
    /// <para><b>⚠ BUILD THE ARGUMENT WITH <c>System.Text.Json</c>
    /// (<c>JsonObject</c>), NEVER WITH STRING INTERPOLATION (T-52-34).</b> The
    /// other of the two path-taking wrappers; see
    /// <see cref="OpenProjectAtPathAsync"/> for why a backslash-laden path is the
    /// exact value that breaks a hand-built JSON string.
    /// <c>Path.GetFullPath</c> first (T-51-22).</para>
    ///
    /// <para><b>The universal NLE convention, and it is load-bearing:</b> after
    /// Save As you are editing the COPY, and every later save — including the one
    /// on close — lands at the new path. A Save As that wrote a copy and left you
    /// editing the original is the shape that loses the next hour of work.</para>
    ///
    /// <para>The parent folder must EXIST; a folder that does not, a name that is
    /// not a legal project name, or a missing <c>.rud</c> extension is refused by
    /// the argument's TYPE, before any write, as a domain error.</para>
    ///
    /// <para>⚠ The returned <c>path</c> is CANONICALISED. See this region's
    /// header.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> SaveProjectAsAsync(string argsJson)
        => NotePersistedAsync(RunJson(NativeMethods.rudis_save_project_as, argsJson));

    /// <summary>
    /// <c>rudis_get_projects</c> — the HUMAN's project list. No args; envelope
    /// <c>{"Ok": [{"isActive": bool, "modifiedUnixMs": N, "name": "..",
    /// "path": ".."}, ..]}</c>.
    ///
    /// <para>⚠ <b>Keys serialize ALPHABETICALLY</b> (<c>serde_json</c>'s map is a
    /// <c>BTreeMap</c> here), never in the Rust struct's declaration order. Read
    /// every field by NAME.</para>
    ///
    /// <para>⚠ This export is named <c>get_projects</c> and it calls
    /// <c>run_get_projects_DETAILED</c>, on purpose. The AGENT's
    /// <c>get_projects</c> withholds the filesystem path deliberately (T-26-08,
    /// minimal disclosure) — and a person choosing between two projects both
    /// called "Untitled" needs exactly the path and the mtime that posture
    /// withholds. One export, two contracts, neither widened.</para>
    ///
    /// <para>⚠ It lists the MANAGED <c>projects/</c> directory only. A project
    /// saved to an arbitrary path with <see cref="SaveProjectAsAsync"/> will NOT
    /// appear here — that is the honest behaviour a user meets, pinned by
    /// <c>ProjectPersistenceTests</c>, and a recent-files surface (not this one)
    /// is where it belongs. And see D-60.1-04: <c>isActive</c> currently reads
    /// <c>false</c> for a project opened BY PATH, because the meta holds the
    /// canonical form and this scan yields the plain one.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> GetProjectsAsync()
        => RunOut(NativeMethods.rudis_get_projects);

    /// <summary>
    /// <c>rudis_get_missing_media</c> — which of the ACTIVE project's media files
    /// are not on disk right now, by ID. No args; envelope
    /// <c>{"Ok": ["&lt;media id&gt;", ..]}</c>, and <c>{"Ok": []}</c> when no
    /// project is active (an absent project has no missing media, and that is not
    /// an error).
    ///
    /// <para>A <b>POLL</b>, deliberately not a seventh event type —
    /// <c>ring::EVENT_NAMES</c> has stayed at 6 through six consecutive phases.
    /// Drive it from the MediaBin's existing 100 ms cold cycle, exactly as
    /// waveform peaks, filmstrip strips, proxy status and render-cache status are
    /// already driven. <b>Never from a paint</b>; the guard in
    /// <see cref="Enqueue"/> counts any violation.</para>
    ///
    /// <para>A LIVE stat, not a value frozen at load: restoring a moved file and
    /// polling again returns an empty list, which is what makes it usable as a
    /// poll at all. Cheap by construction — the backend snapshots the store and
    /// DROPS the guard before it stats anything, so the interop worker that also
    /// services <c>rudis_poll_events</c> is never blocked across a cold directory
    /// walk (T-60.1-09).</para>
    ///
    /// <para>Ids rather than paths (T-26-08): the shell already holds each item's
    /// path in its own mirror, and the MediaBin keys its tiles by id.</para>
    /// </summary>
    public Task<RudisResult<JsonElement>> GetMissingMediaAsync()
        => RunOut(NativeMethods.rudis_get_missing_media);

    // ── plumbing ────────────────────────────────────────────────────────────

    private delegate RudisStatus OutCall(RudisCtxHandle ctx, out RudisBuffer buf);

    private delegate RudisStatus JsonCall(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

    private Task<RudisResult<JsonElement>> RunOut(OutCall fn) => Enqueue(() =>
    {
        var status = fn(_handle, out var buf);
        var envelope = NativeMethods.ReadUtf8AndFree(ref buf);
        return Envelope.Parse(status, envelope);
    });

    private Task<RudisResult<JsonElement>> RunJson(JsonCall fn, string argsJson) => Enqueue(() =>
    {
        var bytes = Encoding.UTF8.GetBytes(argsJson);
        RudisStatus status;
        RudisBuffer buf;
        try
        {
            unsafe
            {
                fixed (byte* p = bytes)
                {
                    status = fn(_handle, (nint)p, (nuint)bytes.Length, out buf);
                }
            }
        }
        finally
        {
            // Review 69 IN-03: this array carries the key for SetApiKeyAsync /
            // SetProviderKeyAsync. The managed string copies cannot be zeroed; this can.
            System.Security.Cryptography.CryptographicOperations.ZeroMemory(bytes);
        }
        var envelope = NativeMethods.ReadUtf8AndFree(ref buf);
        return Envelope.Parse(status, envelope);
    });

    // ── THE PAINT-SCOPE GUARD (plan 52-08 / criterion 5 / 52-RESEARCH Pitfall 7) ──
    //
    // Criterion 5 forbids "a synchronous call during timeline draw" in so many words,
    // and Pitfall 7 explains why the forbidden version is so hard to notice: it looks
    // completely correct on a warm cache, and only shows up as stutter on a cold one or
    // a large media item. A visual check cannot find that. So the check is a COUNTER.
    //
    // ⚠ THE CHECK LIVES AT THE TOP OF `Enqueue` BECAUSE `Enqueue` IS THE SINGLE
    // CHOKEPOINT. Plan 50-03's CR-01 fix made `NativeMethods` a `private` type nested
    // inside this class, so every one of the serialized exports — present and future
    // — necessarily funnels through this one method. One `if` therefore covers the whole
    // surface, and a new export cannot be added that bypasses it without also being
    // written outside this class, which does not compile. That is the property that
    // makes a single guard sufficient rather than merely convenient.
    //
    // WHAT IT DOES NOT COVER, stated rather than implied: the four unqueued members
    // (`GetPlaybackPosition` and the three preview-panel ones) do not pass through here.
    // That is correct rather than a hole — they are lock-free scalar/COM calls with
    // their own documented any-thread policy, and `GetPlaybackPosition` is DELIBERATELY
    // a per-composition-tick read. The shape Pitfall 7 is about — a JSON round trip on
    // the interop worker, taken synchronously from a draw — is exactly the shape that
    // funnels through here. The complementary check is the SOURCE SCAN in
    // `TimelinePaintScopeTests`, which asserts that no file on the Timeline's per-frame
    // path references the interop wrapper at all.
    //
    // NOT `#if DEBUG`: plan 52-09 reads this counter out of a RELEASE-shaped run through
    // the introspection hook, and a gate that exists only in Debug proves nothing about
    // what ships. The cost is one thread-static read per COLD-path ABI call, on a path
    // that is already doing JSON.

    [ThreadStatic]
    private static int _paintDepth;

    /// <summary>
    /// ABI calls that were issued from inside a paint scope, ever, on any thread.
    /// **Expected to be ZERO forever.** A non-zero reading is criterion 5's named
    /// failure and must be ATTRIBUTED by ablation before it is accepted or dismissed
    /// (the 50-08 rule), never explained away.
    ///
    /// <para>Read it with <see cref="Interlocked.Read(ref long)"/>; it is written with
    /// <see cref="Interlocked.Increment(ref long)"/> so a violation from a background
    /// thread that happens to be inside its own paint scope is still counted exactly
    /// once.</para>
    /// </summary>
    internal static long AbiCallsInsidePaintScope;

    /// <summary>
    /// Mark the calling thread as being inside a paint/present callstack until the
    /// returned scope is disposed. Nestable; unwinds correctly on an exception, because
    /// <c>using</c> disposes on the throwing path too.
    ///
    /// <para>Returns a STRUCT, so a per-tick <c>using</c> costs no allocation — the
    /// composition tick runs 60 times a second and a boxed disposable there would show
    /// up in the very GC gate this codebase already measures.</para>
    /// </summary>
    internal static PaintScope EnterPaintScope()
    {
        _paintDepth++;
        return default;
    }

    /// <summary>True while the calling thread is inside a paint scope.</summary>
    internal static bool InPaintScope => _paintDepth > 0;

    /// <summary>The scope token. A struct with no fields: the depth it manages is the
    /// thread-static above, so this carries nothing and copies for free.</summary>
    internal readonly struct PaintScope : IDisposable
    {
        public void Dispose() => _paintDepth--;
    }

    private Task<RudisResult<JsonElement>> Enqueue(Func<RudisResult<JsonElement>> work)
    {
        // BEFORE any work, so a call is counted even if it is refused a line later for
        // a disposed wrapper — the question this answers is "was it ATTEMPTED from a
        // paint", and a violation that happens to hit a closed handle is still a
        // violation.
        if (_paintDepth > 0)
        {
            Interlocked.Increment(ref AbiCallsInsidePaintScope);
        }

        var tcs = new TaskCompletionSource<RudisResult<JsonElement>>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        if (_queue is null)
        {
            tcs.SetResult(RudisResult<JsonElement>.Fault(
                RudisStatus.InvalidHandle,
                "rudis_init returned null — no engine instance exists"));
            return tcs.Task;
        }
        if (Volatile.Read(ref _disposed) != 0)
        {
            tcs.SetResult(RudisResult<JsonElement>.Fault(
                RudisStatus.InvalidHandle, "wrapper disposed"));
            return tcs.Task;
        }
        try
        {
            _queue.Add(() =>
            {
                try
                {
                    tcs.SetResult(work());
                }
                catch (Exception e)
                {
                    tcs.SetException(e);
                }
            });
        }
        catch (Exception e) when (e is InvalidOperationException or ObjectDisposedException)
        {
            // Dispose completed/disposed the queue between the guard and the Add:
            // surfaced as a fault, never an unhandled throw.
            tcs.SetResult(RudisResult<JsonElement>.Fault(
                RudisStatus.InvalidHandle, "wrapper disposed"));
        }
        return tcs.Task;
    }

    /// <summary>
    /// Drain-then-release (50-02 §1.5(5)): complete the queue, join the worker
    /// (bounded — it exits once drained), THEN dispose the SafeHandle, whose
    /// release (the shutdown call) the runtime runs at most once. A second Dispose
    /// is a structural no-op. Close-path parity (50-02 §2.3): nothing else happens
    /// at close — no save call exists at close in either host; persistence is the
    /// three hooks (switch/create/end-of-agent-turn) in both.
    /// </summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }
        _queue?.CompleteAdding();
        _worker?.Join();
        _queue?.Dispose();
        _handle.Dispose();
    }

    // ── documented reach around the queue #1: teardown ───────────────────────

    /// <summary>
    /// <c>rudis_shutdown</c>, forwarded for exactly one caller:
    /// <see cref="RudisCtxHandle.ReleaseHandle"/>. This is NOT a bypass of CR-01's
    /// fix — <c>rudis_shutdown</c> was never one of the 21 exports the single-
    /// worker policy protects (see this class's own remarks: "the shutdown release
    /// runs only after the worker has drained and joined"). SafeHandle's runtime
    /// guarantees <c>ReleaseHandle</c> fires AT MOST ONCE, and <see cref="Dispose"/>
    /// already orders it strictly after the worker is drained — so calling the raw
    /// export here, outside the queue, is the documented, correct shape, not an
    /// accident of visibility. No other export is forwarded this way.
    /// </summary>
    internal static RudisStatus Shutdown(nint ctx) => NativeMethods.rudis_shutdown(ctx);

    // ── the panel-affine reaches around the queue (Phase 51, D-07) ───────────

    /// <summary>
    /// <c>rudis_preview_attach_panel</c>, called DIRECTLY and SYNCHRONOUSLY on the
    /// caller's own thread — never posted to the interop worker.
    ///
    /// <para><b>This is a correctness requirement, not an optimisation.</b> Inside
    /// the DLL, the first <c>Surface::configure</c> calls
    /// <c>ISwapChainPanelNative::SetSwapChain</c>, which returns
    /// <c>RPC_E_WRONG_THREAD</c> off the UI thread that owns the panel. Routing this
    /// through <see cref="RunOut"/> like every other export would run it on the
    /// <c>"rudis-interop"</c> background thread and fail EVERY attach — the exact
    /// failure mode D-07 names, and the failure mode a copy-paste of the
    /// <c>RunOut</c> pattern would reintroduce. The caller
    /// (<c>Regions/Preview.xaml.cs</c>) satisfies the rule STRUCTURALLY by calling
    /// from the panel's <c>Loaded</c> handler, which WinUI runs on exactly that
    /// thread.</para>
    ///
    /// <para>Consistent with CR-01, not a hole in it: the two existing reaches
    /// (<see cref="Shutdown"/>, <see cref="TestSupport"/>) exist for the same kind
    /// of reason — an export the single-worker policy was never the right home for.
    /// Neither this method nor those touch any serialized command export.</para>
    ///
    /// <para><paramref name="panel"/> is an AddRef'd <c>IInspectable*</c> the CALLER
    /// still owns and must release (T-51-15); Rust takes its own reference through
    /// its own <c>QueryInterface</c>. <paramref name="widthPx"/>/
    /// <paramref name="heightPx"/> are PHYSICAL pixels and <paramref name="scale"/>
    /// the panel's composition scale (D-10) — Rust clamps all three, and so does the
    /// caller, because neither layer trusts the other (T-51-06).</para>
    ///
    /// <para>Statuses to SURFACE rather than swallow: <c>NotASwapChainPanel</c>
    /// (wrong pointer), <c>WrongThread</c>, <c>SurfaceCreateFailed</c>,
    /// <c>AlreadyAttached</c>.</para>
    /// </summary>
    internal RudisStatus AttachPreviewPanel(nint panel, uint widthPx, uint heightPx, float scale)
    {
        if (Volatile.Read(ref _disposed) != 0 || _handle.IsClosed || _handle.IsInvalid)
        {
            return RudisStatus.InvalidHandle;
        }
        try
        {
            return NativeMethods.rudis_preview_attach_panel(_handle, panel, widthPx, heightPx, scale);
        }
        catch (ObjectDisposedException)
        {
            // Teardown race: Dispose closed the handle between the guard above and
            // the marshal — the marshaller's AddRef refused the closed handle. The
            // same arm GetPlaybackPosition already carries.
            return RudisStatus.InvalidHandle;
        }
    }

    /// <summary>
    /// <c>rudis_preview_detach_panel</c> — the other half of the panel-affine pair,
    /// and subject to the same rule for a different reason: Rust records the
    /// attaching thread id and answers <c>WrongThread</c> if teardown arrives from
    /// anywhere else, so a queued detach could never release the surface. Call it
    /// from the panel's <c>Unloaded</c> handler, which WinUI runs on the same UI
    /// thread as <c>Loaded</c>.
    ///
    /// <para>Idempotent-safe by contract: a second call answers
    /// <c>NotAttached</c> rather than faulting.</para>
    /// </summary>
    internal RudisStatus DetachPreviewPanel()
    {
        if (Volatile.Read(ref _disposed) != 0 || _handle.IsClosed || _handle.IsInvalid)
        {
            return RudisStatus.InvalidHandle;
        }
        try
        {
            return NativeMethods.rudis_preview_detach_panel(_handle);
        }
        catch (ObjectDisposedException)
        {
            return RudisStatus.InvalidHandle;
        }
    }

    /// <summary>
    /// <c>rudis_preview_recover_device</c> — the THIRD panel-affine reach-around, and
    /// subject to the same rule as <see cref="AttachPreviewPanel"/> for exactly the same
    /// reason: recovery step 4 recreates the surface, which means a FIRST
    /// <c>Surface::configure</c>, which means <c>ISwapChainPanelNative::SetSwapChain</c>,
    /// which returns <c>RPC_E_WRONG_THREAD</c> off the panel's UI thread.
    ///
    /// <para><b>Synchronous, and it can take seconds.</b> The whole six-step
    /// <c>engine::RecoveryPlan</c> sequence runs inside this one call: stop the producer
    /// (bounded wait), tear down the FFmpeg hw pool, drop the dead wgpu device, recreate
    /// the D3D11VA device and the whole GPU set against <paramref name="panel"/>, restore
    /// the playhead, re-arm the present path. It blocks the UI thread for the duration —
    /// deliberately, because the alternatives are a torn-down device recreated from the
    /// wrong thread, or a preview that stays dead until restart.</para>
    ///
    /// <para><b>PAUSE THE TRANSPORT FIRST.</b> Not a nicety: the ring producer holds an
    /// owned <c>Arc</c> clone of the compositor for its whole life, and while ANY handle
    /// to a removed D3D12 device lives, DXGI hides the hardware adapter from fresh
    /// enumeration in this process — step 4 would then recreate on WARP and the
    /// same-adapter LUID re-assert would refuse it. The producer exits when playback
    /// stops; step 1 waits for that and REFUSES (before tearing anything down) if it does
    /// not happen.</para>
    ///
    /// <para><paramref name="panel"/> is a FRESH AddRef'd <c>IInspectable*</c> for the SAME
    /// <c>SwapChainPanel</c> — the reference Rust held was released with the dead surface
    /// in step 3, so the caller marshals a new one and releases its own afterwards
    /// (T-51-15), exactly as at attach.</para>
    ///
    /// <para>Fail-closed: any step failing latches the sequence shut for the rest of the
    /// session (<c>recovering</c> stays <see langword="true"/> in the status poll), so the
    /// caller's trigger can never become a teardown/attach retry storm (threat
    /// T-63-05).</para>
    /// </summary>
    internal RudisStatus RecoverPreviewDevice(nint panel, uint widthPx, uint heightPx, float scale)
    {
        if (Volatile.Read(ref _disposed) != 0 || _handle.IsClosed || _handle.IsInvalid)
        {
            return RudisStatus.InvalidHandle;
        }
        try
        {
            return NativeMethods.rudis_preview_recover_device(_handle, panel, widthPx, heightPx, scale);
        }
        catch (ObjectDisposedException)
        {
            return RudisStatus.InvalidHandle;
        }
    }

    /// <summary>
    /// <c>rudis_preview_resize</c> — the SECOND any-thread member, alongside
    /// <see cref="GetPlaybackPosition"/>. Lock-free by construction on the Rust
    /// side: three relaxed atomic stores plus a release-ordered dirty flag the
    /// present thread consumes on its own next tick. It must NOT be queued — a DPI
    /// or monitor change that waited behind an in-flight export (an encode blocks
    /// the worker for the WHOLE encode, 50-02 §1.5(3)) would present at the wrong
    /// scale for the entire duration.
    ///
    /// <para>Verified, not assumed: the second and later <c>configure()</c> calls
    /// take <c>wgpu-hal</c>'s <c>ResizeBuffers</c> branch and never reach
    /// <c>SetSwapChain</c> (<c>wgpu-hal-26.0.6/src/dx12/mod.rs:1254-1272,
    /// 1348-1355</c>), so resize carries no COM thread rule at all — over-constraining
    /// it to the UI thread would add latency and buy nothing (Pitfall 2's other
    /// half).</para>
    /// </summary>
    internal RudisStatus ResizePreview(uint widthPx, uint heightPx, float scale)
    {
        if (Volatile.Read(ref _disposed) != 0 || _handle.IsClosed || _handle.IsInvalid)
        {
            return RudisStatus.InvalidHandle;
        }
        try
        {
            return NativeMethods.rudis_preview_resize(_handle, widthPx, heightPx, scale);
        }
        catch (ObjectDisposedException)
        {
            return RudisStatus.InvalidHandle;
        }
    }

    /// <summary>
    /// <c>rudis_preview_content_rect</c> — four relaxed atomic loads, zero
    /// allocation, any thread. Returns <see langword="false"/> for any
    /// non-<c>Ok</c> status, with <paramref name="rect"/> left at
    /// <see langword="default"/>, so a caller can never mistake an unattached
    /// panel's zeroes for a real rect.
    ///
    /// <para>This is the ONE source of truth for the Canvas region's pointer
    /// normalization: the engine fills it from the same
    /// <c>engine::contain_fit_viewport</c> call the compositor letterboxes with. Do
    /// NOT re-derive contain-fit math in C# (D-12).</para>
    /// </summary>
    internal bool TryGetPreviewContentRect(out RudisPreviewRect rect)
    {
        rect = default;
        if (Volatile.Read(ref _disposed) != 0 || _handle.IsClosed || _handle.IsInvalid)
        {
            return false;
        }
        try
        {
            if (NativeMethods.rudis_preview_content_rect(_handle, out var native) != RudisStatus.Ok)
            {
                return false;
            }
            rect = native;
            return true;
        }
        catch (ObjectDisposedException)
        {
            return false;
        }
    }

    // ── white-box test support (Rudis.Shell.Tests/InteropTests.cs ONLY) ─────

    /// <summary>
    /// The three primitives <c>InteropTests.cs</c> exercises directly, forwarded
    /// here rather than left unreachable, because none of the three were ever
    /// subject to CR-01's single-worker policy in the first place:
    /// <list type="bullet">
    /// <item><c>rudis_abi_probe</c> is ctx-free — no shared engine state to race
    ///   (see <see cref="AbiProbeAsync"/>'s own "ctx-free by design" note).</item>
    /// <item><c>rudis_get_playback_position</c> IS <see cref="GetPlaybackPosition"/>'s
    ///   hot path — already documented as "callable from any thread" by design, so
    ///   a direct call here proves nothing the wrapper's own contract forbids.</item>
    /// <item><c>ReadUtf8AndFree</c> is the shared buffer-free chokepoint (T-50-10),
    ///   not an ABI call that mutates engine state — it is a marshalling utility
    ///   the tests exercise for its own double-free/dangling-pointer safety.</item>
    /// </list>
    /// None of the serialized command exports (<c>rudis_transport</c>,
    /// <c>rudis_export_timeline</c>, <c>rudis_undo</c>, ...) are reachable through
    /// this type or anywhere else outside <see cref="RudisNative"/>'s own async
    /// surface — this is a deliberately narrow exception, not a reopening of the
    /// gap CR-01 closed.
    /// </summary>
    internal static class TestSupport
    {
        internal static RudisStatus AbiProbeRaw(out RudisBuffer buf) => NativeMethods.rudis_abi_probe(out buf);

        internal static string ReadUtf8AndFreeRaw(ref RudisBuffer buf) => NativeMethods.ReadUtf8AndFree(ref buf);

        internal static long GetPlaybackPositionRaw(RudisCtxHandle ctx) => NativeMethods.rudis_get_playback_position(ctx);
    }

    // ── the P/Invoke surface — PRIVATE to this class (CR-01, 50-REVIEW.md) ──

    /// <summary>
    /// The production P/Invoke surface over <c>rudis_ffi.dll</c> — every signature
    /// transcribed from the committed C ABI contract
    /// (<c>crates/ffi/include/rudis_ffi.h</c>; the header IS the contract, FFI-03).
    /// Every export this shell calls is declared here and nowhere else, source-generated
    /// marshalling only (v7-STACK Q3 — the legacy runtime-marshalled attribute is banned
    /// in this codebase). <b>That is 42 of the ABI's 43</b>, and the count is stated as a
    /// RATIO on purpose, so a gap between the two sides stays VISIBLE rather than implied.
    ///
    /// <para>How the gap got to 3, what closed two of them, and what is left. Plans
    /// 52-05/52-08, 54-02/54-04 and 53.2-04/53.2-06 each added one export on the Rust side
    /// and closed the ratio again on the C# side, ending at 30 of 30. Phases 58 and 59 then
    /// added <c>rudis_get_proxy_status</c>, <c>rudis_get_render_cache_status</c> and
    /// <c>rudis_get_playback_resolution_level</c> WITHOUT a C# half, and plan 60.1-03 added
    /// the seven project-lifecycle exports below, which plan 60.1-04 declares here.
    /// <b>This ratio is what caught that</b>, and v8's closing audit un-ticked
    /// <c>PROXY-02</c> on it: a feature the engine had fully built, tested and shipped was
    /// invisible to every user because nobody had written the caller.</para>
    ///
    /// <para><b>Plan 63-04 (TRUST-03) closed two of the three.</b> The two status reads are
    /// declared below and CONSUMED — proxy status by <c>MediaBin</c>'s per-tile badge,
    /// render-cache status by <c>Transport</c>'s banner — both riding the existing 100 ms
    /// cold poll, with no change to any Rust signature and no seventh event tag.
    /// <c>rudis_get_playback_resolution_level</c> is the ONE still missing: PLAY-05's
    /// observable has no shell consumer yet, and it is left STATED rather than quietly
    /// declared, because <b>a declared-but-uncalled export and an undeclared one are the
    /// same defect class this ratio exists to keep loud</b> -- the whole of Phase 60.1
    /// exists because <c>run_save_project</c>'s ancestors sat in that category for five
    /// phases.</para>
    ///
    /// <para><b>Plan 63-02 (TRUST-01) added three exports and declared all three in the
    /// same commit</b> — <c>rudis_preview_device_status</c>,
    /// <c>rudis_preview_simulate_device_lost</c> and <c>rudis_preview_recover_device</c> —
    /// which is what this ratio is FOR: "40 of 40" would have been the wrong number the
    /// moment the surface grew, and "42 of 43" is the right one, because the export still
    /// missing a caller has not changed. That gap is still
    /// <c>rudis_get_playback_resolution_level</c>, still PLAY-05's, and still stated
    /// rather than quietly closed.</para>
    ///
    /// Marshalling: <c>nint</c> = pointers, <c>nuint</c> = <c>uintptr_t</c> (x64-only
    /// build); <see cref="RudisCtxHandle"/> = <c>struct RudisCtx*</c> under SafeHandle
    /// discipline. JSON parameters cross as raw UTF-8 bytes (callers pin and pass
    /// <c>nint</c>), so no string marshalling is configured anywhere.
    ///
    /// <para><b>Nested and `private`, deliberately (CR-01):</b> this type used to be
    /// a top-level `internal` class, which meant ANY file in the assembly could call
    /// a serialized export directly — including from the UI thread — while
    /// <c>RudisNative</c>'s own docs claimed the policy was "structural, not a
    /// convention". Nesting it here, `private`, makes that claim literally true:
    /// only <see cref="RudisNative"/>'s own members (its async surface,
    /// <see cref="RudisNative.Shutdown"/>, and <see cref="RudisNative.TestSupport"/>)
    /// can name this type at all.</para>
    /// </summary>
    private static partial class NativeMethods
    {
        private const string Dll = "rudis_ffi";

        // ── lifecycle + memory ──────────────────────────────────────────────────

        /// <summary>Header: <c>struct RudisCtx *rudis_init(const uint8_t *config_json,
        /// uintptr_t config_len)</c>. Null/zero config = engine defaults (per-instance
        /// temp dirs). Returns null (IsInvalid) on malformed UTF-8/JSON or any
        /// construction failure — BY DESIGN (lib.rs:191-201).</summary>
        [LibraryImport(Dll)]
        internal static partial RudisCtxHandle rudis_init(nint configJson, nuint configLen);

        /// <summary>Header: <c>RudisStatus rudis_shutdown(struct RudisCtx *ctx)</c>.
        /// ONLY via <see cref="RudisNative.Shutdown"/>, itself reachable ONLY from
        /// <see cref="RudisCtxHandle.ReleaseHandle"/>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_shutdown(nint ctx);

        /// <summary>Header: <c>void rudis_free_buffer(struct RudisBuffer buf)</c> — the
        /// ONLY legal free for a buffer returned by this library (same allocator).
        /// All-zero struct is a safe no-op. Callable ONLY from
        /// <see cref="ReadUtf8AndFree"/> — the one-chokepoint discipline (T-50-10).</summary>
        [LibraryImport(Dll)]
        internal static partial void rudis_free_buffer(RudisBuffer buf);

        /// <summary>Ctx-free by design: proves symbol resolution + the buffer
        /// round-trip before any instance exists. Envelope:
        /// <c>{"Ok": "rudis-ffi-phase47-probe-v1"}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_abi_probe(out RudisBuffer buf);

        // ── reads ───────────────────────────────────────────────────────────────

        /// <summary><c>{"Ok": {..Project..}}</c> — the whole snapshot (resync path).</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_snapshot(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Args <c>{"ids": [..]}</c>; named entities in request order.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_entities(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>The store's mutation counter — the resync anchor.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_current_seq(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Drain the event ring, non-blocking. Envelope:
        /// <c>{"Ok": {"resync_required": bool, "next_seq": u64, "events": [..]}}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_poll_events(RudisCtxHandle ctx, ulong localSeq, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_get_waveform_peaks(struct RudisCtx *ctx,
        /// const uint8_t *json, uintptr_t len, struct RudisBuffer *out)</c>. Args
        /// <c>{"media_id": ".."}</c>. A PURE cache read that cannot start a decode
        /// (52-05); reachable only through <see cref="RudisNative.GetWaveformPeaksAsync"/>,
        /// which is called only from the cold poll.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_waveform_peaks(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_get_filmstrip_strip(struct RudisCtx *ctx,
        /// const uint8_t *json, uintptr_t len, struct RudisBuffer *out)</c> — the 30th
        /// export (Phase 53.2, plan 53.2-04). Args <c>{"media_id": ".."}</c>. A PURE
        /// cache read that cannot start a decode, exactly like
        /// <see cref="rudis_get_waveform_peaks"/> above; reachable only through
        /// <see cref="RudisNative.GetFilmstripStripAsync"/>, which is called only from
        /// the cold poll.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_filmstrip_strip(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_get_proxy_status(struct RudisCtx *ctx,
        /// const uint8_t *json, uintptr_t len, struct RudisBuffer *out)</c> — Phase 58
        /// (PROXY-02). Args <c>{"media_id": ".."}</c>. A PURE registry/cache read that
        /// "never errors, never computes": it cannot start a generation, and
        /// <c>{"Ok":null}</c> covers a miss and an unknown id alike. Reachable only through
        /// <see cref="RudisNative.GetProxyStatusAsync"/>, which is called only from the
        /// cold poll — the FIFTH rider on Phase 50 D-06's 100 ms cycle after peaks and
        /// filmstrips, and the reason <c>ring::EVENT_NAMES</c> is still 6.
        ///
        /// <para><b>Declared by plan 63-04, five phases after the export shipped.</b> That
        /// gap IS <c>PROXY-02</c>'s un-tick, and it is why the ratio in this type's summary
        /// is written as a ratio.</para></summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_proxy_status(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_get_render_cache_status(struct RudisCtx
        /// *ctx, struct RudisBuffer *out)</c> — Phase 59 (CACHE-01). <b>NO args at all</b>,
        /// and that is the interesting difference from
        /// <see cref="rudis_get_proxy_status"/> above: a proxy is a property of one MEDIA
        /// ITEM, the render cache is a property of the PROGRAM, so there is no id to pass
        /// and this takes <c>(ctx, out)</c> like <see cref="rudis_get_current_seq"/>.
        /// Always <c>{"Ok": {..}}</c>, never null. It NEVER triggers a render
        /// (<c>render_cache_job_status_never_starts_a_render</c> polls it a hundred times
        /// against a heavy section to prove it). Reachable only through
        /// <see cref="RudisNative.GetRenderCacheStatusAsync"/>, on the same cold
        /// cycle.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_render_cache_status(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>THE HOT PATH: a lock-free scalar read of the playback mirror —
        /// no envelope, no allocation, no JSON. <c>i64::MIN</c> is the out-of-band
        /// sentinel for BOTH a null handle and a caught panic; a real position is
        /// clamped to [0, duration] and can never be the sentinel.</summary>
        [LibraryImport(Dll)]
        internal static partial long rudis_get_playback_position(RudisCtxHandle ctx);

        // ── commands (JSON in, envelope out) ────────────────────────────────────

        /// <summary>LAT-01 marker hook; <c>{"Ok": null}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_debug_mark_interactive(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_debug_seed_project(struct RudisCtx *ctx,
        /// const uint8_t *json, uintptr_t len, struct RudisBuffer *out)</c> — the 29th
        /// export (Phase 54, plan 54-02: the phase's ONE engine-axis freeze crossing).
        /// Args <c>{"project": {..Project..}}</c>. DEBUG-GATED on
        /// <c>RUDIS_DEBUG_SEED_PROJECT</c>, checked per call and latching nothing;
        /// reachable only through <see cref="RudisNative.DebugSeedProjectAsync"/>, whose
        /// only caller is the never-shipped <c>Rudis.Shell.EvalHarness</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_debug_seed_project(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary><c>{"Ok": {..Patch..}}</c>, or <c>{"Ok": null}</c> with nothing to undo.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_undo(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>rudis_undo's exact mirror.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_redo(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Place a MediaBin item on the timeline; <c>{"Ok": {..Clip..}}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_place_clip(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Apply one pending option card (CANV-02); <c>{"Ok": [{..Patch..}, ..]}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_apply_option_card(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Args <c>{"cmd": {"type": "..", "data": {..}}}</c> (adjacently-tagged
        /// TransportCmd); <c>{"Ok": {..Playback..}}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_transport(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary><c>{"Ok": {"key_configured": bool, "source": "credential_manager"|"environment"|"none",
        /// "providers": {"runway": {..same..}}}}</c> — key material never
        /// crosses back over the ABI (T-47-13).</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_agent_status(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Args <c>{"key": ".."}</c>; inbound only.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_set_api_key(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Idempotent; <c>{"Ok": null}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_clear_api_key(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Args <c>{"provider": "runway", "key": ".."}</c> — provider ids resolve only
        /// through the backend's Settings allow-list (<c>{"runway"}</c>, D-69-12); anything
        /// else is refused. Inbound only (T-47-13).</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_set_provider_key(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Args <c>{"provider": "runway"}</c>; idempotent, <c>{"Ok": null}</c> (D-69-12).</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_clear_provider_key(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Probe + poster + register each path; <c>{"Ok": [{..MediaBinItem..}, ..]}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_import_media(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Recursive folder import as ONE undo turn (500 files / depth 12 clamps).</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_import_media_folder(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>BLOCKS the calling thread for the WHOLE encode — reachable only via
        /// the interop worker, never the UI thread (50-02 §1.5(3)).</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_export_timeline(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>One real Chat turn; text-only through this host (D-03).</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_agent_send_message(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Apply one undoable store mutation; <c>{"Ok": {..Patch..}}</c>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_dispatch_command(RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        // ── the project lifecycle (Phase 60.1, plan 60.1-03's +7) ───────────────
        //
        // Transcribed from the committed header, `crates/ffi/include/rudis_ffi.h`
        // lines 650-797 — the header IS the contract (FFI-03). Four take JSON,
        // three take none; all seven return the D-06 two-layer envelope, so a
        // refusal ("no such project", "that is a folder, not a .rud") arrives as
        // `{"Err": ".."}` with transport status `Ok` and is NEVER a transport
        // fault. Reachable only through the seven public wrappers above.

        /// <summary>Header: <c>RudisStatus rudis_new_project(struct RudisCtx *ctx,
        /// const uint8_t *json, uintptr_t len, struct RudisBuffer *out)</c>. Args
        /// <c>{"name": ".."}</c>; envelope <c>{"Ok": "&lt;prose&gt;"}</c> — a STRING.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_new_project(
            RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_open_project(struct RudisCtx *ctx,
        /// const uint8_t *json, uintptr_t len, struct RudisBuffer *out)</c>. Args
        /// <c>{"name": ".."}</c> — a registry LOOKUP KEY, never a path component
        /// (T-26-03); envelope <c>{"Ok": "&lt;prose&gt;"}</c> — a STRING.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_open_project(
            RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_open_project_at_path(struct RudisCtx *ctx,
        /// const uint8_t *json, uintptr_t len, struct RudisBuffer *out)</c>. Args
        /// <c>{"path": ".."}</c>, whose Rust field type (<c>RudProjectPath</c>) IS the
        /// validation; envelope <c>{"Ok": {"name": .., "path": ..}}</c> with the path
        /// CANONICALISED.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_open_project_at_path(
            RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_save_project(struct RudisCtx *ctx,
        /// struct RudisBuffer *out)</c>. No args; envelope
        /// <c>{"Ok": {"path": .., "seq": N}}</c>. MINTS <c>Untitled.rud</c> rather than
        /// refusing when nothing is active.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_save_project(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_save_project_as(struct RudisCtx *ctx,
        /// const uint8_t *json, uintptr_t len, struct RudisBuffer *out)</c>. Args
        /// <c>{"path": ".."}</c>, whose Rust field type (<c>RudSaveTargetPath</c> — a
        /// DIFFERENT type from the open target's, because a save target need not
        /// already exist) IS the validation; envelope
        /// <c>{"Ok": {"name": .., "path": .., "seq": N}}</c>. Re-points the active
        /// document at the new path.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_save_project_as(
            RudisCtxHandle ctx, nint json, nuint len, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_get_projects(struct RudisCtx *ctx,
        /// struct RudisBuffer *out)</c>. No args; envelope
        /// <c>{"Ok": [{"isActive", "modifiedUnixMs", "name", "path"}, ..]}</c> over the
        /// MANAGED <c>projects/</c> directory, keys ALPHABETICAL.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_projects(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_get_missing_media(struct RudisCtx *ctx,
        /// struct RudisBuffer *out)</c>. No args; envelope
        /// <c>{"Ok": ["&lt;media id&gt;", ..]}</c>. A live stat and a POLL — reachable
        /// only through <see cref="RudisNative.GetMissingMediaAsync"/>, which belongs on
        /// the cold cycle and never on a paint.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_get_missing_media(RudisCtxHandle ctx, out RudisBuffer buf);

        // ── the preview surface (Phase 51, SHELL-04) ────────────────────────────
        //
        // These four carry NO envelope — a bare RudisStatus is their only channel,
        // which is why the -5..=-9 block exists in RudisStatus. Their threading
        // requirements genuinely DIFFER (attach/detach are UI-thread-affine, resize
        // and content-rect are lock-free any-thread), which is why the ABI keeps
        // them as four symbols instead of one nullable-pointer `configure`.

        /// <summary>Header: <c>RudisStatus rudis_preview_attach_panel(struct RudisCtx *ctx,
        /// void *panel, uint32_t width_px, uint32_t height_px, float scale)</c>.
        /// ⚠ UI-THREAD-AFFINE — reachable ONLY via
        /// <see cref="RudisNative.AttachPreviewPanel"/>, which is deliberately NOT
        /// queued. See that method's remarks.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_preview_attach_panel(
            RudisCtxHandle ctx, nint panel, uint widthPx, uint heightPx, float scale);

        /// <summary>Header: <c>RudisStatus rudis_preview_resize(struct RudisCtx *ctx,
        /// uint32_t width_px, uint32_t height_px, float scale)</c>. Lock-free,
        /// any-thread, allocation-free.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_preview_resize(
            RudisCtxHandle ctx, uint widthPx, uint heightPx, float scale);

        /// <summary>Header: <c>RudisStatus rudis_preview_device_status(struct RudisCtx *ctx,
        /// struct RudisBuffer *out)</c> — Phase 63 (TRUST-01). Five relaxed atomic loads;
        /// callable from any thread, and routed through <c>RunOut</c> like every other
        /// envelope-carrying read. See <see cref="RudisNative.GetPreviewDeviceStatusAsync"/>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_preview_device_status(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_preview_simulate_device_lost(struct RudisCtx
        /// *ctx, struct RudisBuffer *out)</c> — Phase 63 (TRUST-01). DEBUG-GATED FAIL-CLOSED
        /// on the Rust side (<c>RUDIS_DEBUG_DEVICE_LOSS=1</c>, read once at process start) and
        /// <c>#if DEBUG</c> at every call site. See
        /// <see cref="RudisNative.SimulatePreviewDeviceLostAsync"/>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_preview_simulate_device_lost(RudisCtxHandle ctx, out RudisBuffer buf);

        /// <summary>Header: <c>RudisStatus rudis_preview_recover_device(struct RudisCtx *ctx,
        /// void *panel, uint32_t width_px, uint32_t height_px, float scale)</c> — Phase 63
        /// (TRUST-01). PANEL-AFFINE and synchronous, like
        /// <see cref="rudis_preview_attach_panel"/> and for the same
        /// <c>SetSwapChain</c> reason; deliberately NOT routed through <c>RunOut</c>. See
        /// <see cref="RudisNative.RecoverPreviewDevice"/>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_preview_recover_device(
            RudisCtxHandle ctx, nint panel, uint widthPx, uint heightPx, float scale);

        /// <summary>Header: <c>RudisStatus rudis_preview_detach_panel(struct RudisCtx *ctx)</c>.
        /// ⚠ MUST run on the thread that attached.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_preview_detach_panel(RudisCtxHandle ctx);

        /// <summary>Header: <c>RudisStatus rudis_preview_content_rect(struct RudisCtx *ctx,
        /// struct RudisPreviewRect *out)</c>. <c>*out</c> is written ONLY on
        /// <see cref="RudisStatus.Ok"/> — see
        /// <see cref="RudisNative.TryGetPreviewContentRect"/>.</summary>
        [LibraryImport(Dll)]
        internal static partial RudisStatus rudis_preview_content_rect(
            RudisCtxHandle ctx, out RudisPreviewRect rect);

        // ── the ONE buffer reader ───────────────────────────────────────────────

        /// <summary>
        /// Read a returned buffer as UTF-8 and free it EXACTLY ONCE via the native free
        /// (the paired-free discipline, structural: this is the codebase's ONLY buffer
        /// reader — T-50-10's mitigation, promoted from the Phase 47 harness). Branches
        /// on <c>Len</c>, never <c>Ptr != 0</c>: an empty Vec's pointer is
        /// dangling-but-non-null by construction (V5). <c>Len</c>/<c>Cap</c> are never
        /// mutated before the free (a tampered triple is UB). The struct is zeroed
        /// behind the free, so a second call is the documented all-zero safe no-op.
        /// </summary>
        internal static string ReadUtf8AndFree(ref RudisBuffer buf)
        {
            try
            {
                if (buf.Len == 0)
                {
                    return string.Empty;
                }
                var bytes = new byte[checked((int)buf.Len)];
                Marshal.Copy(buf.Ptr, bytes, 0, bytes.Length);
                return System.Text.Encoding.UTF8.GetString(bytes);
            }
            finally
            {
                rudis_free_buffer(buf);
                buf = default;
            }
        }
    }
}

/// <summary>
/// The <c>rudis_get_filmstrip_strip</c> payload, field for field.
///
/// <para><b>These nine names are a WIRE CONTRACT.</b> The other side is
/// <c>crates/app-core/src/filmstrip_job.rs</c>'s <c>StripPayload</c>, whose own doc
/// comment says the same thing from the other direction: serde serializes by name and
/// <c>System.Text.Json</c> deserializes by name, so renaming one without the other is
/// an ABI break with no compiler anywhere to catch it. Every property therefore carries
/// an explicit <see cref="JsonPropertyName"/> rather than relying on a naming policy —
/// the wire spelling is written down, greppable, and diffable against the Rust struct.</para>
///
/// <para><b>Shape only; no policy.</b> A successful parse means the nine fields were
/// present and of the right JSON kind. It does NOT mean the geometry is self-consistent,
/// that the base64 decodes, or that the byte count matches the grid — those are
/// <c>FilmstripCache</c>'s fail-closed checks, made there because there they can be
/// answered against the DECODED bytes and because the cache is where a violation has to
/// turn into a miss. This split mirrors <c>PeakCache.TryDecode</c>'s discipline
/// (52-05 §4.2 / T-52-40): a corrupt payload is a MISS, never an exception on the poll
/// path. It also means this type is safe to hand a hostile payload — it can lie, and
/// lying gets it rejected one layer up.</para>
///
/// <para><b>D-14 lives in two of these fields.</b> <c>completed_tiles &lt; total_tiles</c>
/// is a PARTIAL strip — drawable now, more coming, keep polling.
/// <c>completed_tiles == total_tiles</c> is complete: stop polling that media id forever.
/// <c>sheet_w</c>/<c>sheet_h</c> describe the bytes actually PRESENT (the completed rows),
/// never the finished grid, so a consumer can size its upload from this payload alone
/// without assuming anything about a strip still being written.</para>
///
/// <para>A record CLASS, not a record struct, deliberately: a record struct has both a
/// primary constructor and an implicit parameterless one, which makes
/// <c>System.Text.Json</c>'s constructor selection a thing to reason about rather than a
/// thing that is obvious. This allocates once per media item, ever, on the cold path.</para>
/// </summary>
internal sealed record FilmstripStripPayload(
    [property: JsonPropertyName("tile_w")] uint TileW,
    [property: JsonPropertyName("tile_h")] uint TileH,
    [property: JsonPropertyName("tiles_per_row")] uint TilesPerRow,
    [property: JsonPropertyName("total_tiles")] uint TotalTiles,
    [property: JsonPropertyName("completed_tiles")] uint CompletedTiles,
    [property: JsonPropertyName("interval_us")] long IntervalUs,
    [property: JsonPropertyName("sheet_w")] uint SheetW,
    [property: JsonPropertyName("sheet_h")] uint SheetH,
    [property: JsonPropertyName("strip_b64")] string StripB64)
{
    /// <summary>
    /// Parse the <c>{"Ok": {..}}</c> body into this shape, or answer
    /// <see langword="false"/>.
    ///
    /// <para><b>Never throws.</b> <c>{"Ok": null}</c> (D-15's flat null — miss, not-yet,
    /// audio-only, still, offline, decode-failed, unknown id, all indistinguishable by
    /// design), a non-object body, a missing field, a field of the wrong JSON kind, and a
    /// number outside its target range are all the same answer: <see langword="false"/>.
    /// The caller's next move is identical in every case, which is precisely why the ABI
    /// does not distinguish them.</para>
    /// </summary>
    internal static bool TryParse(JsonElement body, out FilmstripStripPayload payload)
    {
        payload = default!;

        if (body.ValueKind != JsonValueKind.Object)
        {
            return false;
        }

        try
        {
            var parsed = body.Deserialize<FilmstripStripPayload>();
            if (parsed is null || parsed.StripB64 is null)
            {
                return false;
            }

            payload = parsed;
            return true;
        }
        catch (JsonException)
        {
            // A field of the wrong kind, or a number that does not fit its target
            // type. Both are corruption signals, and both are a MISS.
            return false;
        }
        catch (NotSupportedException)
        {
            return false;
        }
    }
}
