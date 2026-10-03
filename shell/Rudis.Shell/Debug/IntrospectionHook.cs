// D-10's secondary UAT assertion channel (Phase 50, plan 50-08, Task 3).
//
// ⚠ THE ENTIRE FILE IS COMPILED OUT OF RELEASE. Everything below — including this
// namespace declaration — lives inside one #if DEBUG block, so a Release build of
// Rudis.Shell carries NO type named IntrospectionHook at all: not disabled, not
// dead-code-eliminated, ABSENT from the assembly. That absence is asserted
// MECHANICALLY, not promised, by shell/Rudis.Shell.Tests/ReleaseHookAbsenceTests.cs,
// which reflection-loads a real Release build and asserts the type does not exist (and,
// for the other half of the claim, that a Debug build DOES carry it).
//
// Even in a Debug build this is OFF BY DEFAULT: it starts only when
// RUDIS_SHELL_INTROSPECT=1 is set in THIS process's environment — mirroring
// debug_mark_interactive's env-gating shape (crates/app-core/src/queries.rs:82-103,
// RUDIS_LAT01_MARKER_PATH), which is a zero-cost no-op unless a test harness explicitly
// opts in. A normal `dotnet run`/F5 developer session never sets this variable.
#if DEBUG
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Rudis.Shell.Mirror;

// ⚠ NAMED `Introspection`, not `Debug` — the FILE lives under `Debug/` (the plan's exact
// path), but a C# namespace literally named `Rudis.Shell.Debug` would shadow
// `System.Diagnostics.Debug` for every unqualified `Debug.WriteLine` call already made
// from within the `Rudis.Shell` namespace (App.xaml.cs's LogDiagnostic — nested-namespace
// lookup wins over a `using`), breaking an unrelated file the instant this one is added.
// Found by the compiler on the first build of App.xaml.cs's registration call, not by
// inspection — an unplanned Rule 3 (blocking) fix, not a deviation from the file's
// required PATH.
namespace Rudis.Shell.Introspection;

/// <summary>
/// A local-only, per-process named pipe answering a closed set of READ-ONLY requests. This
/// is an ASSERTION CHANNEL, not a control channel (T-50-35): there is no branch anywhere
/// in <see cref="HandleRequest"/> that mutates the mirror, the engine, or anything else
/// reachable from this process — every request either reads and serializes state that
/// already exists, or returns an error string naming an unsupported request. No request
/// is ever parsed as a command and dispatched.
///
/// <para>Pipe name: <c>rudis-shell-introspect-{processId}</c> — process-scoped so
/// multiple concurrently-launched instances (exactly what the UIA smoke suite runs) can
/// never collide on the same pipe name.</para>
///
/// <para>Several requests, all plain-text single-line request/response over the pipe:</para>
/// <list type="bullet">
/// <item><c>"state"</c> -> <c>{"ringSeq":.., "storeSeq":.., "mediaCount":.., "clipCount":..}</c>
///   — the mirror's own cursors and two cheap counts, never the raw project JSON (no
///   reason to hand a test harness more surface than the numbers it needs to assert).
///   <para><b>WIDENED by plan 53-06, additively, by three fields and no branch.</b> D-13's
///   second half-proof is "a double-click loads the clip into the SOURCE monitor WITHOUT
///   placing it", and a test that only checked the source half would still pass if the
///   PROGRAM monitor had been hijacked — so the answer now also carries the active
///   monitor's mode and BOTH monitors' loaded media ids. All three are projections of
///   state <c>ShellMirror</c> already holds and this branch already reads
///   (<c>Project.PreviewMode</c>, <c>Project.SourcePlayback</c>, <c>Project.Playback</c>):
///   no new read path, no new channel, and the T-50-35 assertion-channel contract and the
///   WR-03 broad catch below are both untouched.</para></item>
/// <item><c>"gc"</c> -> <c>{"gen0":.., "gen1":.., "gen2":.., "totalAllocatedBytes":..}</c>
///   — <c>GC.CollectionCount(0..2)</c> + <c>GC.GetTotalAllocatedBytes()</c>, the §8
///   "GC-delta-0 holds ... with a UIA client attached" measurement's read side.</item>
/// <item><c>"timeline"</c> -> the mirror's own tracks and clips (plan 52-07), PLUS the
///   surface's own viewport / cull / redraw / latency snapshot (plan 52-09, D-22).</item>
/// <item><c>"timeline-view"</c> -> the same answer WITHOUT the clip list (plan 52-09):
///   constant-size, so a millisecond-scale latency poll does not put 120 KB of JSON
///   inside the number it is measuring.</item>
/// <item><c>"diagnostics"</c> -> the bounded diagnostic tail <c>App.LogDiagnostic</c>
///   already keeps.</item>
/// <item><c>"waveform"</c> -> SHELL-09's gate numbers (plan 52-08).</item>
/// <item><c>"preview"</c> -> <c>{"attached":.., "contentX/Y/W/H":.., "lastResizeW/H":..}</c>
///   — plan 51-07 / D-15, so a "zero garbage collections" reading can be bracketed by
///   evidence that the Preview panel was attached and had actually composited during
///   the measurement window.</item>
/// <item><c>ink</c> (spelled unquoted here, for the same reason as mediabin below)
///   -> the last finished gesture's RAW recorded pointer-sample stream plus the
///   committed normalized point list (plan 51-10) — the measurement that tells a
///   stale current position from a bogus coalesced history from a driver that never
///   moved the pointer. PUBLISHED by the ink layer from the UI thread as one
///   immutable string; this branch reads that string and nothing else.</item>
/// <item><c>mediabin</c> (spelled unquoted HERE, and only here, so the quoted form
///   occurs exactly ONCE in this file — at the switch arm that answers it; the
///   plan's acceptance grep is what keeps a second answering branch from appearing)
///   -> <c>{"controlKind":.., "levelItemCount":..,
///   "realizedContainers":.., "currentFolder":.., "draggingMediaId":..}</c> — SC-2's
///   realized-container measurement (plan 53-04), PUBLISHED by the MediaBin region from
///   the UI thread and merely read here.</item>
/// <item><c>canvas</c> (spelled unquoted here, for the same reason as mediabin and
///   ink above — the double-quoted form occurs exactly ONCE in this file, at the
///   switch arm that answers it)
///   -> <c>{"annotationCount":.., "annotations":[{"id":.., "space":.., "kind":..}]}</c>
///   — plan 60.2-03, the READ side of the Canvas stage phase's SC-1. A stage drag
///   must be asserted on PROJECT STATE (an entry with <c>space == "whiteboard"</c>),
///   never on the gesture object and never on a screenshot (CLAUDE.md rule 3), and no
///   other request here projects <c>canvas</c> at all. Ids, spaces and shape KINDS
///   only — never the point geometry and never a label's text, mirroring the
///   counts-not-project posture the <c>state</c> answer takes.</item>
/// </list>
///
/// <para><b>Wire I/O is RAW bytes, deliberately, not <see cref="StreamReader"/>/
/// <see cref="StreamWriter"/>.</b> Isolated during this plan's own Task 3 with a
/// minimal repro (a throwaway console probe, not committed): constructing a
/// <c>StreamWriter</c>/<c>StreamReader</c> pair directly over a connected
/// <see cref="PipeStream"/> hung non-deterministically on THIS machine's .NET 9 runtime,
/// while raw <see cref="Stream.Read(byte[],int,int)"/>/<see cref="Stream.Write(byte[],int,int)"/>
/// over the identical connected pipe round-tripped reliably across repeated trials. The
/// protocol here is therefore hand-rolled and trivial on purpose: one UTF-8 line in, one
/// UTF-8 line out, newline-delimited, read/written a byte at a time with no buffered
/// text-encoding layer in between.</para>
/// </summary>
internal sealed class IntrospectionHook : IDisposable
{
    /// <summary>Off by default; a normal launch never sets this.</summary>
    internal const string EnvVarName = "RUDIS_SHELL_INTROSPECT";

    private readonly ShellMirror? _mirror;
    private readonly Thread _serverThread;
    private volatile bool _stopRequested;
    private NamedPipeServerStream? _currentServer;
    private readonly object _currentServerLock = new();

    private IntrospectionHook(ShellMirror? mirror, int processId)
    {
        _mirror = mirror;
        PipeName = PipeNameFor(processId);
        _serverThread = new Thread(RunServerLoop) { IsBackground = true, Name = "IntrospectionHook" };
        _serverThread.Start();
    }

    internal string PipeName { get; }

    internal static string PipeNameFor(int processId) => $"rudis-shell-introspect-{processId}";

    /// <summary>
    /// The one entry point. Returns null (does nothing, starts no pipe, no background
    /// thread) unless <see cref="EnvVarName"/> is exactly <c>"1"</c> in THIS process's
    /// environment — called once from App.xaml.cs, itself inside its own #if DEBUG.
    /// </summary>
    internal static IntrospectionHook? StartIfRequested(ShellMirror? mirror)
    {
        if (Environment.GetEnvironmentVariable(EnvVarName) != "1")
        {
            return null;
        }
        return new IntrospectionHook(mirror, Environment.ProcessId);
    }

    private void RunServerLoop()
    {
        while (!_stopRequested)
        {
            NamedPipeServerStream server;
            try
            {
                server = new NamedPipeServerStream(
                    PipeName,
                    PipeDirection.InOut,
                    maxNumberOfServerInstances: 1,
                    PipeTransmissionMode.Byte,
                    PipeOptions.None);
            }
            catch (IOException)
            {
                // All instances busy (should not happen with the client's short-lived
                // connect-request-disconnect pattern, but never spin hot on it).
                Thread.Sleep(50);
                continue;
            }

            lock (_currentServerLock)
            {
                _currentServer = server;
            }

            try
            {
                server.WaitForConnection();
                var request = ReadLine(server);
                var response = HandleRequest(request);
                WriteLine(server, response);
                server.WaitForPipeDrain();
            }
            catch (IOException)
            {
                // A client disconnected mid-request, or Dispose() closed this instance
                // to unblock WaitForConnection — either way, serve the next connection
                // (or exit, if _stopRequested is now true).
            }
            catch (ObjectDisposedException)
            {
                // Dispose() closed this instance from another thread.
            }
            finally
            {
                lock (_currentServerLock)
                {
                    if (ReferenceEquals(_currentServer, server))
                    {
                        _currentServer = null;
                    }
                }
                server.Dispose();
            }
        }
    }

    /// <summary>Read one newline-delimited UTF-8 line, byte at a time, with no
    /// intermediate <see cref="StreamReader"/> (see the class remarks). Bounded so a
    /// misbehaving client cannot make this grow unbounded.</summary>
    private static string? ReadLine(Stream stream)
    {
        const int maxLineBytes = 4096;
        var buffer = new List<byte>(64);
        int b;
        while (buffer.Count < maxLineBytes && (b = stream.ReadByte()) != -1)
        {
            if (b == '\n')
            {
                break;
            }
            if (b != '\r')
            {
                buffer.Add((byte)b);
            }
        }
        return buffer.Count == 0 ? null : Encoding.UTF8.GetString(buffer.ToArray());
    }

    private static void WriteLine(Stream stream, string text)
    {
        var bytes = Encoding.UTF8.GetBytes(text + "\n");
        stream.Write(bytes, 0, bytes.Length);
        stream.Flush();
    }

    /// <summary>
    /// READ-ONLY dispatch — the whole surface. Every branch reads and serializes
    /// state that already exists; nothing here constructs or executes a command, and
    /// there is no branch that could be extended into one without changing this method's
    /// shape (T-50-35's grep-verified claim: no dispatch, no command execution).
    ///
    /// <para><b>WR-03 (50-REVIEW.md):</b> the <c>"state"</c> branch reads
    /// <see cref="ShellMirror"/> from THIS background pipe thread, racing the UI
    /// thread's mutations of a type documented as "affinity, not thread-safety...
    /// touched from ONE logical flow" (<c>ShellMirror.cs:37-40</c>). A query landing
    /// mid-mutation can legitimately throw (e.g. a collection-modified exception
    /// while <c>WireJson.FromNode</c> serializes <c>_raw</c>). This whole file is
    /// compiled out of Release (<c>ReleaseHookAbsenceTests</c> proves it) and off by
    /// default even in Debug, so the blast radius is dev-tooling only — but an
    /// unhandled exception on a plain background <see cref="Thread"/> terminates the
    /// WHOLE PROCESS by default in .NET, which would crash the very app someone is
    /// trying to debug. Caught broadly and reported as data, never rethrown: this
    /// keeps the read-only-assertion-channel contract (T-50-35) intact — a caught
    /// race is still just information, never a command.</para>
    /// </summary>
    private string HandleRequest(string? request)
    {
        try
        {
            // ⚠ ONE GATE, NEVER A PARALLEL ONE (D-22). Any future query — Timeline,
            // MediaBin, Chat, whatever a later phase wants to assert on — belongs in
            // THIS switch, inside THIS file's single `#if DEBUG`, behind THIS env var,
            // and answered READ-ONLY. A second inspection channel would have to
            // re-derive the compile-out, the env gate, the raw-byte protocol and the
            // Release-absence proof, and the first one of those it got wrong would be
            // a debug surface in a shipped build (T-52-42). Adding a branch here costs
            // nothing and inherits all four.
            return request switch
            {
                "state" => JsonSerializer.Serialize(new
                {
                    ringSeq = _mirror?.RingSeq ?? 0UL,
                    storeSeq = _mirror?.LastAppliedStoreSeq ?? 0UL,
                    mediaCount = _mirror?.Project?.MediaBin.Count ?? 0,
                    clipCount = _mirror?.Project?.Timeline.Tracks.Sum(t => t.Clips.Count) ?? 0,

                    // Plan 53-06's three additions — see the class remarks. Same object,
                    // same read, three more projections of what the mirror already holds.
                    previewMode = _mirror?.Project?.PreviewMode ?? "",
                    sourceLoadedMediaId = _mirror?.Project?.SourcePlayback.LoadedMediaId,
                    programLoadedMediaId = _mirror?.Project?.Playback.LoadedMediaId,
                }),
                "gc" => JsonSerializer.Serialize(new
                {
                    gen0 = GC.CollectionCount(0),
                    gen1 = GC.CollectionCount(1),
                    gen2 = GC.CollectionCount(2),
                    totalAllocatedBytes = GC.GetTotalAllocatedBytes(),
                }),

                // Plan 52-07. The Timeline's own clips, as the MIRROR holds them —
                // which is the only authority worth reading (CLAUDE.md rule 4). A UAT
                // run performs a gesture and needs to compare the project before and
                // after; asserting on a screenshot instead would prove the surface
                // redrew, not that the backend state changed.
                //
                // Still read-only, still one `JsonSerializer.Serialize` over state that
                // already exists, still no branch that could grow into a mutation
                // without changing this method's shape (T-50-35).
                //
                // ── EXTENDED by plan 52-09 (D-22), ADDITIVELY. ────────────────────
                // The `tracks` array is 52-07's and is UNCHANGED: `TimelineEditTests`
                // reads it, and re-shaping a channel another plan's evidence already
                // rests on would have made this plan's first act a regression. What is
                // ADDED is the SURFACE's own state — the viewport, the cull's yield,
                // the redraw clock, the two latency measurements criterion 2 is about,
                // and the hydrate costs criterion 3 is about.
                //
                // Field naming is mixed and that is deliberate: `tracks`' inner fields
                // stay camelCase because they are 52-07's published shape, and the
                // snapshot's stay snake_case because that is this plan's own specified
                // contract. Renaming either breaks a consumer to gain a convention.
                //
                // The snapshot half is a SNAPSHOT the region publishes at the end of
                // each render tick, never a live read: the viewport, the model and the
                // peak cache all belong to the UI thread, and this method runs on the
                // pipe's background thread. Answering a query must not be able to race
                // a frame — and must never be able to STALL one.
                "timeline" => JsonSerializer.Serialize(TimelinePayload(includeTracks: true)),

                // The SAME payload minus the clip list — the SAME gate, the SAME
                // switch, the SAME read-only posture, and a seventh word rather than a
                // second channel (D-22's rule is one gate, not one word).
                //
                // It exists because criterion 2 polls this pipe every few milliseconds
                // looking for the first redraw after an input, and at 1,000 clips the
                // `tracks` array is ~120 KB. Serialising, piping and parsing that a
                // couple of hundred times per measurement would add real milliseconds
                // to the very number being measured — the instrument would be part of
                // the reading. This answer is a few hundred bytes and constant in the
                // clip count.
                "timeline-view" => JsonSerializer.Serialize(TimelinePayload(includeTracks: false)),

                // The bounded diagnostic tail App.LogDiagnostic already keeps. It is
                // where the exact Command JSON of every dispatched edit lands, so a UAT
                // run can record what actually travelled rather than what it believes
                // it asked for.
                "diagnostics" => JsonSerializer.Serialize(new { lines = App.DiagnosticTail(200) }),

                // Plan 52-08. SHELL-09's gate numbers, and criterion 5's in
                // particular: `abiInPaint` must read 0 after any session, however
                // long and however busy. It is a SNAPSHOT the Timeline's own 1 Hz
                // publisher refreshes on the UI thread, not a live read of the peak
                // cache — that cache is single-threaded by contract, and a background
                // reader walking its dictionary mid-mutation is exactly the race its
                // doc comment refuses. Read-only like every other branch here.
                "waveform" => JsonSerializer.Serialize(Regions.Timeline.LastWaveformDiagnostics),

                // Plan 51-07. SHELL-06's measurement window has to be BRACKETED by
                // evidence that the thing being measured was actually running: a
                // "zero garbage collections during sustained playback" reading taken
                // from a run where the Preview never came up would be exactly the
                // vacuity D-15 exists to refuse. This answers `attached` plus the
                // engine's own content rect, so the gate can assert the panel was
                // live and had composited BEFORE it starts counting.
                //
                // Read-only like every other branch: three scalars the Preview region
                // already holds, plus four relaxed atomic loads through
                // `rudis_preview_content_rect`. No branch here mutates the mirror, the
                // engine or the panel, and none touches the visual tree from this
                // background thread (T-51-25; T-50-35 preserved, not widened).
                "preview" => JsonSerializer.Serialize(Regions.Preview.DescribeForIntrospection()),

                // Plan 53-04. SC-2 says the MediaBin must use a virtualized standard
                // control; D-03 sharpens it into the clause that costs something —
                // "virtualization must be PROVEN, not assumed: realized-container count
                // against a synthetic large bin, not 'we used the virtualizing
                // control.'" This is the read side of that measurement: how many
                // containers the control actually realized, beside how many items the
                // level actually holds, so the count can be shown to be bounded
                // RELATIVE to a large bin rather than merely small in isolation.
                //
                // ⚠ WHY THIS READS A PUBLISHED SNAPSHOT AND NOT THE CONTROL. WR-03
                // (50-REVIEW) notes that the `state` branch reads ShellMirror from this
                // background pipe thread and merely RACES the UI thread. The visual tree
                // is not like that: asking a live XAML element for its realized children
                // off the thread that owns it is RPC_E_WRONG_THREAD — a hard failure,
                // not a torn read. So the MediaBin region publishes plain ints and
                // strings from the UI thread and this branch reads plain ints and
                // strings. No visual-tree object is reachable from here, by construction
                // — and the plan's acceptance grep over this file for the visual-tree
                // type names is what keeps it that way, which is also why they are
                // described here rather than named.
                //
                // Still an ASSERTION channel, verbatim (T-50-35): one read of an
                // already-existing snapshot, one Serialize. It constructs no command,
                // dispatches nothing, takes no argument the caller could steer, and
                // mutates neither the mirror, the engine, nor the region.
                "mediabin" => JsonSerializer.Serialize(MediaBinIntrospection.Describe()),

                // Plan 51-10. The RAW pointer-sample stream for the last gesture —
                // the measurement that discriminates a stale current position (H1)
                // from a bogus coalesced history (H2) from a driver that never moved
                // the pointer (H3). Read-only, one snapshot the UI thread published,
                // no visual-tree object reachable from here.
                //
                // Already a JSON string when it arrives, so it is answered verbatim
                // rather than re-serialized: the shape is the ink layer's to state,
                // beside the values it describes, exactly as MediaBinIntrospection
                // and Preview.DescribeForIntrospection do.
                "ink" => Regions.PreviewInkLayer.LastInkSamplesJson,

                // Plan 60.2-03. SC-1 of the Canvas stage phase says a stage drag must
                // be asserted ON PROJECT STATE — an entry in Project.canvas.annotations
                // carrying space == "whiteboard" — never on the gesture object and
                // never on a screenshot (CLAUDE.md rule 3). No existing branch projects
                // `canvas` at all, and Mirror/Models.cs deliberately has no typed canvas
                // projection to fall back on ("under-projecting is free; a wrong
                // projection is not", PreviewInkLayer.xaml.cs's MirrorHasAnnotation
                // remarks), so this is the read side of that assertion. It projects the
                // MINIMUM the tests need — id, space, shape kind, and the count — never
                // the raw shape geometry and never a label's text, for the same
                // minimal-disclosure reason the `state` answer reports counts rather
                // than the project JSON (T-60.2-05).
                //
                // Read-only like every branch here (T-50-35): one walk over the raw node
                // the mirror already holds, one Serialize. It constructs no command,
                // takes no argument a caller could steer, sends nothing to the backend,
                // and mutates neither the mirror nor the engine nor any region. The
                // RawProject read from this background thread RACES the UI thread's
                // mirror mutations exactly as the `state` branch does (WR-03) — the
                // method-wide catch reports a torn read as data, never a crash — and no
                // visual-tree object is reachable from here, so the RPC_E_WRONG_THREAD
                // hazard the `mediabin` branch documents does not apply.
                "canvas" => JsonSerializer.Serialize(
                    DescribeCanvas(Child(Child(_mirror?.RawProject, "canvas"), "annotations") as JsonArray)),

                _ => JsonSerializer.Serialize(new
                {
                    error = "unsupported request; only 'state', 'gc', 'timeline', 'timeline-view', " +
                            "'diagnostics', 'waveform', 'preview', 'mediabin', 'ink' and 'canvas' " +
                            "are read",
                }),
            };
        }
        catch (Exception e)
        {
            // Never let a mirror race (or any other read fault) take the process
            // down — see the remarks above. The caller (RunServerLoop) still gets a
            // response line to write back, so the client sees a diagnosable error
            // instead of a dropped connection.
            return JsonSerializer.Serialize(new
            {
                error = $"introspection query threw: {e.GetType().Name}: {e.Message}",
            });
        }
    }

    /// <summary>
    /// The <c>timeline</c> response, built in one place so the switch stays a switch.
    ///
    /// <para>READ-ONLY like every branch that calls it: two projections over state that
    /// already exists (the mirror's tracks, and the render tick's published snapshot).
    /// Nothing here constructs a command, and there is no parameter a caller could
    /// supply — the request word carries no arguments at all, which is the cheapest
    /// possible guarantee that this cannot become a control channel (T-52-41).</para>
    /// </summary>
    private object TimelinePayload(bool includeTracks)
    {
        var snapshot = Regions.Timeline.ReadIntrospectionSnapshot();
        var drop = TimelineDropIntrospection.Read();

        return new
        {
            // ── 52-07's shape, unchanged ──
            tracks = includeTracks
                ? _mirror?.Project?.Timeline.Tracks
                    .Select((track, index) => new
                    {
                        index,
                        kind = track.Kind,
                        clips = track.Clips.Select(c => new
                        {
                            id = c.Id,
                            mediaId = c.MediaId,
                            startUs = c.StartUs,
                            inUs = c.InUs,
                            outUs = c.OutUs,
                        }).ToArray(),
                    })
                    .ToArray()
                : null,

            // ── 52-09's addition ──
            //
            // `rendered` is FALSE until the Timeline has drawn at least once. A UAT
            // run should wait on it rather than on a control merely existing in the
            // UIA tree — 50-08 paid for that lesson and wrote it down; this is the
            // Timeline's version of the readiness signal.
            rendered = snapshot is not null,
            rasterization_scale = snapshot?.RasterizationScale ?? 0,
            viewport_width_px = snapshot?.ViewportWidthPx ?? 0,
            viewport_height_px = snapshot?.ViewportHeightPx ?? 0,
            px_per_second = snapshot?.PxPerSecond ?? 0,
            scroll_x_px = snapshot?.ScrollXPx ?? 0,
            scroll_y_px = snapshot?.ScrollYPx ?? 0,
            viewport_start_us = snapshot?.ViewportStartUs ?? 0,
            viewport_end_us = snapshot?.ViewportEndUs ?? 0,
            visible_lane_first = snapshot?.VisibleLaneFirst ?? -1,
            visible_lane_last = snapshot?.VisibleLaneLast ?? -1,
            lane_count = snapshot?.LaneCount ?? 0,
            total_clip_count = snapshot?.TotalClipCount ?? 0,
            culled_clip_count = snapshot?.CulledClipCount ?? 0,
            drawn_clip_count = snapshot?.DrawnClipCount ?? 0,
            scanned_clip_count = snapshot?.ScannedClipCount ?? 0,
            selected_clip_id = snapshot?.SelectedClipId,

            // The LANE selection (quick 260731-k9b) — the other thing the ONE primary
            // selection can be, and the ONLY subject an EMPTY track can ever have.
            // `-1` is "no lane is selected" and is a real published value, not an
            // absence: it is what makes "the two selections never coexist" checkable
            // from outside the process, beside `selected_clip_id` above.
            //
            // Read from the region's own published static rather than from `snapshot`
            // because the snapshot class lives in `Timeline.xaml.cs`, which a
            // concurrent session held when this landed. Same UI-thread-writes /
            // pipe-thread-reads discipline either way.
            selected_track_index = Regions.Timeline.LastSelectedTrackIndex,

            playhead_us = snapshot?.PlayheadUs ?? 0,

            // Criterion 2's own numbers. `-1` means "not measured yet" and is NOT a
            // latency: a test that treated it as one would report a 1ms edit.
            last_input_unix_ms = snapshot?.LastInputUnixMs ?? 0,
            last_redraw_unix_ms = snapshot?.LastRedrawUnixMs ?? 0,
            last_feedback_latency_ms = snapshot?.LastFeedbackLatencyMs ?? -1,
            last_feedback_input_unix_ms = snapshot?.LastFeedbackInputUnixMs ?? 0,

            last_render_us = snapshot?.LastRenderUs ?? 0,
            frames_rendered = snapshot?.FramesRendered ?? 0,
            skipped_clean_frames = snapshot?.SkippedCleanFrames ?? 0,
            abi_calls_inside_paint_scope = snapshot?.AbiCallsInsidePaintScope ?? 0,
            peaks_cached_media_ids = snapshot?.PeaksCachedMediaIds ?? 0,
            waveform_quads_drawn = snapshot?.WaveformQuadsDrawn ?? 0,
            model_revision = snapshot?.ModelRevision ?? 0,

            // The enum's NAME is produced here, on the pipe thread — never on the
            // render tick, where `Enum.ToString()` would allocate once per frame.
            interaction_state = snapshot?.InteractionState.ToString(),

            // Criterion 3's project-open half, decomposed.
            apply_mirror_state_us = snapshot?.ApplyMirrorStateUs ?? 0,
            mirror_projection_us = snapshot?.MirrorProjectionUs ?? 0,
            model_rebuild_us = snapshot?.ModelRebuildUs ?? 0,
            mirror_apply_count = snapshot?.MirrorApplyCount ?? 0,

            snapshot_unix_ms = snapshot?.SnapshotUnixMs ?? 0,
            now_unix_ms = Regions.TimelineInteraction.NowUnixMs(),

            // ── 53.1-02's addition, ADDITIVELY. ───────────────────────────────
            // The last DROP's outcome, published from the UI thread at the moment
            // TryPlaceFromDropAsync learns it — NOT from the render-tick snapshot,
            // which a drop does not necessarily trigger (the 53-06 lesson, in code).
            //
            // No branch is added to HandleRequest for these: D-22 keeps ONE gate, and
            // re-shaping a channel other evidence already rests on is a regression
            // (52-09's rule). They join the EXISTING read-only payload.
            //
            // `tracks` above remains the authoritative proof that a REAL clip landed;
            // these six fields are how a UAT run tells "refused by the backend" apart
            // from "nothing happened", which SC-3 needs and a clip count cannot answer.
            last_drop_media_id = drop.MediaId,
            last_drop_track = drop.Track,
            last_drop_start_us = drop.StartUs,
            last_drop_result = drop.Outcome,
            last_drop_error = drop.Error,
            last_drop_seq = drop.Seq,
        };
    }

    /// <summary>
    /// The <c>canvas</c> response, built in one place so the switch arm stays a single
    /// expression like every sibling.
    ///
    /// <para>READ-ONLY and ARGUMENT-FREE, like every branch that calls into here: one
    /// projection over the raw snapshot node the mirror already holds. There is no
    /// parameter the pipe's caller could supply — the request word carries no arguments
    /// at all, which is the cheapest possible guarantee that this cannot become a
    /// control channel (T-52-41; T-60.2-04 for this branch specifically).</para>
    ///
    /// <para><b>Deliberately per-field defensive, and NOT via the obvious
    /// <c>node?["id"]</c> chain.</b> MEASURED with a throwaway probe during this task,
    /// not assumed: on .NET 9, <c>JsonNode</c>'s STRING INDEXER itself throws
    /// <see cref="InvalidOperationException"/> ("The node must be of type 'JsonObject'")
    /// when the receiver is a <see cref="JsonValue"/> or a <see cref="JsonArray"/> —
    /// so a node shaped <c>{"shape":"not-an-object"}</c> takes down the read at
    /// <c>node?["shape"]?["kind"]</c>, BEFORE any <c>GetValue&lt;string&gt;()</c> is
    /// reached. <c>GetValue&lt;string&gt;()</c> throws on a wrong-kind value too. Under
    /// the method-wide catch neither crashes the process, but either would cost the
    /// WHOLE answer for ONE odd node — the abort-the-render-pass failure
    /// <c>CanvasStageGesture.ParseWhiteboardMarks</c> refuses per-node (T-60.2-02).
    /// Hence <see cref="Child"/>: every hop is gated on the receiver actually being a
    /// <see cref="JsonObject"/>. A missing, wrongly-shaped or wrongly-typed field reads
    /// <see langword="null"/> and the row still appears, so the COUNT stays truthful.</para>
    /// </summary>
    private static object DescribeCanvas(JsonArray? annotations)
    {
        IEnumerable<JsonNode?> nodes = annotations ?? Enumerable.Empty<JsonNode?>();

        var projected = nodes
            .Select(node => new
            {
                id = ReadString(node, "id"),
                space = ReadString(node, "space"),
                kind = ReadString(Child(node, "shape"), "kind"),
            })
            .ToArray();

        return new
        {
            // The count is STATED rather than left to the reader's `.length`: a caller
            // polling for "the drag landed" wants one number, and `annotations` is the
            // evidence for WHICH mark landed. Both come out of the same single walk.
            annotationCount = projected.Length,
            annotations = projected,
        };
    }

    /// <summary>A child node, or <see langword="null"/> if the receiver is absent or is
    /// not a <see cref="JsonObject"/>. The non-throwing replacement for <c>node?[name]</c>
    /// — see <see cref="DescribeCanvas"/> for the measurement that made it necessary.
    /// </summary>
    private static JsonNode? Child(JsonNode? owner, string property)
        => owner is JsonObject obj && obj.TryGetPropertyValue(property, out var child)
            ? child
            : null;

    /// <summary>A JSON string PROPERTY of <paramref name="owner"/>, or
    /// <see langword="null"/> if the owner, the property or its string-ness is missing.
    /// Never throws — see <see cref="DescribeCanvas"/>.</summary>
    private static string? ReadString(JsonNode? owner, string property)
        => Child(owner, property) is JsonValue value && value.TryGetValue<string>(out var text)
            ? text
            : null;

    public void Dispose()
    {
        _stopRequested = true;
        // Close whatever server instance is currently active so a thread blocked in
        // WaitForConnection() unblocks immediately (it throws ObjectDisposedException/
        // IOException, both handled in the loop) rather than waiting for a client that
        // may never come.
        lock (_currentServerLock)
        {
            _currentServer?.Dispose();
        }
        _serverThread.Join(TimeSpan.FromSeconds(2));
    }
}
#endif
