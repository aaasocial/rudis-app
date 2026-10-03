using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using Rudis.Shell.Interop;
using Rudis.Shell.Mirror;

namespace Rudis.Shell.Regions;

/// <summary>
/// The <c>Transport</c> region (design_handoff_rudis_editor/README.md:33,115 — the name
/// is the handoff's, verbatim, per CLAUDE.md rule 7).
///
/// <para><b>This is the phase's load-bearing region (D-14)</b>, because it is the only
/// one that exercises all three paths at once:</para>
/// <list type="number">
/// <item><b>HOT</b> — <c>rudis_get_playback_position()</c> once per composition tick.
///   One scalar; zero managed allocation at steady state (SC-3). Wired in plan 50-06
///   Task 2 through <c>PlayheadTicker</c>.</item>
/// <item><b>COLD</b> — <c>playback:changed</c> / <c>project:changed</c> off the mirror,
///   at the ~100ms cadence: play/pause glyph, loop tint, duration, enablement.</item>
/// <item><b>MUTATION</b> — every control drives a REAL <c>rudis_transport</c> command
///   on the one interop worker (D-13, rule 1).</item>
/// </list>
///
/// <para><b>⚠ THE CLOCK IS THE ENGINE'S (UI-SPEC §7 / D-17) — read
/// <see cref="OnPlayClick"/> before touching playback.</b> The Tauri shell drives
/// playback from a JS loop that ticks the engine's per-frame clock command
/// (frontend/src/main.ts:639). This shell deliberately does NOT port that loop: it opts
/// into the engine's own clock via <c>rudis_init</c>'s <c>self_advance</c> flag
/// (App.xaml.cs's <c>BuildInitConfigJson</c>, wired by plan 50-03) and merely POLLS the
/// position. No per-frame managed clock code exists anywhere in <c>shell/</c>, and a
/// grep-verified acceptance criterion keeps it that way.</para>
/// </summary>
public sealed partial class Transport : UserControl
{
    /// <summary>Below this measured width the right utilities collapse (handoff:118 /
    /// UI-SPEC §2 — "below ~640px the right utilities collapse first, then <c>‹ ● ›</c>").
    /// This number IS the handoff's.</summary>
    private const double RightUtilitiesBreakpoint = 640;

    /// <summary>Below this the clip-edge cluster collapses too. The handoff gives the
    /// ORDER but no second number, so 470 is THIS PHASE'S choice, recorded in
    /// artifacts/50-06-transport.md rather than silently picked.</summary>
    private const double ClipEdgeNavBreakpoint = 470;

    /// <summary>T-50-26: at most ONE seek in flight, with the latest requested value
    /// coalesced behind it. A scrub drag fires continuously; queueing every value would
    /// build an unbounded backlog on the interop worker and make the playhead lag the
    /// pointer by the whole queue.</summary>
    private bool _seekInFlight;

    private long? _pendingSeekUs;

    // ── mirror-derived state (cold path). The UI owns no truth (rule 4). ──
    private bool _playing;
    private bool _looping;
    private bool _ready;
    private bool _hasLoadedMedia;
    private bool _timelineIsPreviewable;
    private long _durationUs;
    private long _frameStepUs;
    private long _mirrorPositionUs;

    /// <summary>Sorted timeline clip boundaries in µs (starts AND ends), rebuilt on
    /// every <c>project:changed</c>. The clip-edge nav seeks between these.</summary>
    private readonly List<long> _clipEdges = [];

    /// <summary>
    /// MDL2 <c>Play</c> / <c>Pause</c>, written as CODEPOINTS rather than as literal
    /// private-use characters.
    ///
    /// <para>⚠ Two rendering traps, both of which have now been met in this codebase.
    /// (1) 50-05's: a PUA codepoint has NO DirectWrite font fallback, so pinning a font
    /// family that is absent renders NOTHING AT ALL — which is why the family here is
    /// <c>{ThemeResource SymbolThemeFontFamily}</c>, measured on this Windows 10 19045
    /// box to resolve to <c>Segoe MDL2 Assets</c> (no <c>Segoe Fluent Icons</c> is
    /// installed). (2) This plan's: a raw PUA character in a source file survives no
    /// tool round trip that is not byte-exact, and when it is lost the compiler is
    /// perfectly happy — the button simply renders empty. Both failure modes are
    /// INVISIBLE rather than loud, so both are pinned by construction: ASCII-only source
    /// here, and numeric character entities (<c>&amp;#xE768;</c>) in the XAML.</para>
    /// </summary>
    private const char GlyphPlay = (char)0xE768;

    private const char GlyphPause = (char)0xE769;

    /// <summary>
    /// The HOT path (D-06's hot half): one scalar <c>rudis_get_playback_position()</c> per
    /// composition tick, change-detected, allocation-free at steady state. Created here
    /// rather than at <c>Loaded</c> so no tick can ever arrive before it exists.
    ///
    /// <para>The position source is a lambda over <c>App.Engine</c> because the engine is
    /// created in <c>App.OnLaunched</c> BEFORE the window, and because
    /// <see cref="RudisNative.GetPlaybackPosition"/> is the wrapper's ONE free-threaded
    /// member — it needs no queue, no marshal and no event mechanism
    /// (v7-ARCHITECTURE:101). A <c>null</c> from it is the fault sentinel, never a
    /// position (T-50-25).</para>
    /// </summary>
    private readonly PlayheadTicker _ticker;

    public Transport()
    {
        InitializeComponent();
        _ticker = new PlayheadTicker(static () => App.Engine?.GetPlaybackPosition(), new Readout(this));
        Scrubber.UserSeekRequested += us => _ = SeekAsync(us);
        Scrubber.StepRequested += delta => _ = StepAsync(delta);
        Scrubber.JumpRequested += toEnd => _ = SeekAsync(toEnd ? _durationUs : 0);
        Scrubber.ScrubbingChanged += OnScrubbingChanged;
        Loaded += OnTransportLoaded;
        ApplyEnablement();
    }

    /// <summary>
    /// The measured answer to UI-SPEC §9's UNVERIFIED item 8 (tabular figures),
    /// rendered in the window's status block so it is readable without a debugger — and
    /// by UIA, which is how this plan's artifact captured it. Same shape as
    /// <c>TitleBar.MechanismNote</c>.
    /// </summary>
    public string TypographyNote { get; private set; } = "(not measured yet)";

    /// <summary>True while the user is dragging <c>Transport.Scrubber</c>. The playhead
    /// readout follows the DRAG, not the engine, while this holds (frontend parity: the
    /// <c>scrubbing</c> guard, main.ts:670).</summary>
    public bool IsScrubbing { get; private set; }

    /// <summary>The last position the hot path read; null before the first tick or after
    /// a fault. Cold-path readers only (the window's status block).</summary>
    public long? LastPlayheadUs => _ticker.LastPositionUs;

    /// <summary>Hot-tick count, so "the rate is live" stays checkable at launch (50-04's
    /// evidence pattern).</summary>
    public long HotTicks => _ticker.TickCount;

    /// <summary>How many of those ticks re-rendered the readout. The GAP between this and
    /// <see cref="HotTicks"/> is the change-detection cache doing its job — and it is the
    /// number that makes SC-3's delta == 0 possible at all (UI-SPEC §4 rule 1).</summary>
    public long HotRenders => _ticker.RenderCount;

    /// <summary>
    /// ⚠ HOT PATH — called once per composition tick from <c>MainWindow.OnRendering</c>.
    /// Everything reachable from here obeys SC-3: no allocation, no interpolation, no
    /// LINQ, no boxing, and no visual-tree touch unless the RENDERED value changed.
    /// Proven numerically by <c>HotPathAllocationTests</c>, not by inspection.
    /// </summary>
    public void OnCompositionTick() => _ticker.Tick();

    // ── cold path: mirror-driven state (UI-SPEC §4) ──────────────────────────

    /// <summary>
    /// Re-derive everything patch-driven from the mirror. Called by
    /// <c>MainWindow</c> on <c>project:changed</c> AND <c>playback:changed</c> — both,
    /// because enablement depends on the project (timeline/bin) while the glyph and loop
    /// tint depend on the playback.
    /// </summary>
    internal void ApplyMirrorState(ShellMirror mirror)
    {
        var project = mirror.Project;
        var playback = mirror.ActivePlayback;

        var isSource = project?.PreviewMode == "source";
        RebuildClipEdges(project);

        _hasLoadedMedia = !string.IsNullOrEmpty(playback?.LoadedMediaId);
        _timelineIsPreviewable = _clipEdges.Count > 0;

        // "Something to preview", exactly as the backend decides it
        // (crates/core/src/transport.rs:118-122) and as the shipping frontend renders it
        // (main.ts:678 — `active = isSource ? loaded : timelineHasClips()`). Deriving
        // this differently here would let the UI enable a control the backend rejects.
        _ready = isSource ? _hasLoadedMedia : _timelineIsPreviewable;
        _playing = playback?.Playing ?? false;
        _looping = playback?.Looping ?? false;
        _durationUs = playback?.DurationUs ?? 0;
        _mirrorPositionUs = playback?.PositionUs ?? 0;
        var fps = playback?.Fps ?? 0;
        // Twin of rudis_core::frame_step_us / main.ts:565 — integer µs per frame.
        _frameStepUs = fps > 0 ? (long)Math.Round(1_000_000 / fps) : 0;

        Scrubber.Minimum = 0;
        Scrubber.Maximum = _durationUs;
        Scrubber.FrameStepUs = _frameStepUs;
        if (!IsScrubbing)
        {
            Scrubber.SetPositionFromMirror(_mirrorPositionUs);
        }

        _ticker.DurationUs = _durationUs;
        // The engine position stays authoritative for the readout, but a COLD change —
        // a new duration, a seek the backend clamped, a media swap — must repaint even
        // when the position itself did not move. Invalidating the change cache is how the
        // cold path asks the hot path for one render, without ever writing the readout
        // from two places.
        _ticker.Invalidate();

        ApplyPlayGlyph();
        ApplyLoopTint();
        ApplyClipEdgeIndicator();
        ApplyEnablement();
        RenderTimecodeFromMirror();
    }

    private void RebuildClipEdges(Project? project)
    {
        _clipEdges.Clear();
        if (project is null)
        {
            return;
        }
        foreach (var track in project.Timeline.Tracks)
        {
            foreach (var clip in track.Clips)
            {
                var end = clip.StartUs + Math.Max(0, clip.OutUs - clip.InUs);
                _clipEdges.Add(clip.StartUs);
                _clipEdges.Add(end);
            }
        }
        _clipEdges.Sort();
    }

    /// <summary>UI-SPEC §5 + §6: the glyph AND the automation name flip with
    /// <c>playback.playing</c>, so UIA can assert the state (plan 50-08 does).</summary>
    private void ApplyPlayGlyph()
    {
        PlayButton.Content = _playing ? GlyphPause : GlyphPlay;
        AutomationProperties.SetName(PlayButton, _playing ? "Pause" : "Play");
        ToolTipService.SetToolTip(PlayButton, _playing ? "Pause" : "Play");
    }

    private void ApplyLoopTint()
    {
        var resources = Application.Current.Resources;
        LoopButton.Foreground = (Brush)resources[_looping ? "accent" : "text-secondary"];
        AutomationProperties.SetName(LoopButton, _looping ? "Loop, on" : "Loop");
    }

    /// <summary>The `●` between `‹` and `›`: real mirror-derived state — is the playhead
    /// inside a timeline clip? Read-only, so it is an indicator rather than a
    /// control.</summary>
    private void ApplyClipEdgeIndicator()
    {
        var inside = false;
        for (var i = 0; i + 1 < _clipEdges.Count; i += 2)
        {
            if (_mirrorPositionUs >= _clipEdges[i] && _mirrorPositionUs < _clipEdges[i + 1])
            {
                inside = true;
                break;
            }
        }
        ClipEdgeIndicator.Fill = (Brush)Application.Current.Resources[inside ? "accent" : "text-tertiary"];
    }

    /// <summary>
    /// UI-SPEC §5's empty state: no media → every control disabled and rendered
    /// <c>text-tertiary</c> (the styles' Disabled visual state does the colour), with the
    /// timecode at <c>00:00:00.000</c>.
    ///
    /// <para>Step needs a LOADED media specifically, not merely something previewable:
    /// the backend takes the frame step from the loaded item's fps and returns
    /// <c>NoPreviewLoaded</c> otherwise (transport.rs:147-151). Enabling it for a
    /// timeline-only project would be a button that always fails.</para>
    /// </summary>
    private void ApplyEnablement()
    {
        PlayButton.IsEnabled = _ready;
        StopButton.IsEnabled = _ready;
        ProjectStartButton.IsEnabled = _ready;
        ProjectEndButton.IsEnabled = _ready && _durationUs > 0;
        LoopButton.IsEnabled = _ready;
        StepBackButton.IsEnabled = _hasLoadedMedia;
        StepForwardButton.IsEnabled = _hasLoadedMedia;
        // Clip-edge nav is a TIMELINE concept: with the Source monitor active there are
        // no timeline boundaries to walk, and seeking to one would move the wrong
        // playhead.
        var clipNav = _timelineIsPreviewable && _ready && _clipEdges.Count > 0;
        PrevClipButton.IsEnabled = clipNav;
        NextClipButton.IsEnabled = clipNav;
        Scrubber.IsEnabled = _ready && _durationUs > 0;
        // Transport.FullscreenButton stays disabled by design (Phase 51), and
        // Transport.FrameMenu is view-state only, so both are left alone here.
    }

    // ── commands: every one a REAL rudis_transport call ──────────────────────

    /// <summary>
    /// <b>UI-SPEC §7 — the single most important implementation note in this
    /// phase.</b>
    ///
    /// <para>This handler sends <c>play</c> (or <c>pause</c>) and then STOPS. It starts
    /// no timer, no render-loop clock, no per-frame command. The Tauri frontend, by
    /// contrast, starts a <c>requestAnimationFrame</c> loop that ticks the engine's
    /// elapsed-time transport command on every frame (frontend/src/main.ts:619-649).
    /// That loop is deliberately NOT ported, for two independent reasons:</para>
    /// <list type="number">
    /// <item>It would put managed code back on the PER-FRAME path that SHELL-06 exists
    ///   to eliminate — the shell is a host-polls-native consumer
    ///   (v7-ARCHITECTURE.md:101-102).</item>
    /// <item>It would FIGHT the engine for the clock. This shell opts into D-17's
    ///   engine clock (<c>rudis_init</c>'s <c>self_advance</c> — App.xaml.cs), so the
    ///   engine's own ~10ms tick thread already moves the playhead
    ///   (crates/ffi/src/self_advance.rs). A managed clock on top of it would advance
    ///   the same <c>Playback</c> twice and play at 2× speed — a bug that reads as
    ///   vague pacing weirdness rather than as a double-drive (T-50-28).</item>
    /// </list>
    ///
    /// <para>So playback progresses here because the ENGINE moves it, and this region
    /// merely polls <c>rudis_get_playback_position()</c> per composition tick.</para>
    /// </summary>
    private void OnPlayClick(object sender, RoutedEventArgs e)
        => _ = SendAsync(Cmd(_playing ? "pause" : "play"), _playing ? "pause" : "play");

    /// <summary>■ stop = <c>pause</c> THEN <c>seek {0}</c> (UI-SPEC §3). Two commands,
    /// in that order, because a seek does not stop the clock — the backend preserves the
    /// playing state across a seek (transport.rs:139-145).</summary>
    private void OnStopClick(object sender, RoutedEventArgs e) => _ = StopAsync();

    private async Task StopAsync()
    {
        if (await SendAsync(Cmd("pause"), "pause"))
        {
            await SendAsync(Seek(0), "seek");
        }
    }

    private void OnStepBackClick(object sender, RoutedEventArgs e) => _ = StepAsync(-1);

    private void OnStepForwardClick(object sender, RoutedEventArgs e) => _ = StepAsync(1);

    private void OnLoopClick(object sender, RoutedEventArgs e)
    {
        var cmd = Cmd("set_looping");
        cmd["data"] = new JsonObject { ["looping"] = !_looping };
        _ = SendAsync(cmd, "set_looping");
    }

    private void OnProjectStartClick(object sender, RoutedEventArgs e) => _ = SeekAsync(0);

    private void OnProjectEndClick(object sender, RoutedEventArgs e) => _ = SeekAsync(_durationUs);

    private void OnPrevClipClick(object sender, RoutedEventArgs e) => _ = SeekAsync(AdjacentEdge(forward: false));

    private void OnNextClipClick(object sender, RoutedEventArgs e) => _ = SeekAsync(AdjacentEdge(forward: true));

    /// <summary><c>Transport.FrameMenu</c> has NO backend by design (UI-SPEC §3): it is
    /// shell view-state. The millisecond-precision toggle is real view state — it
    /// changes what this region renders — and safe areas are recorded as a Preview
    /// concern the menu will drive once Phase 51 owns that surface.</summary>
    private void OnFrameMenuItemClick(object sender, RoutedEventArgs e)
    {
        App.LogDiagnostic(
            $"Transport.FrameMenu: view state only (safe_areas={FrameMenuSafeAreas.IsChecked} " +
            $"timecode_ms={FrameMenuTimecodeMs.IsChecked}) — no backend command (UI-SPEC §3)");
        RenderTimecodeFromMirror();
    }

    private void OnScrubbingChanged(bool scrubbing)
    {
        IsScrubbing = scrubbing;
        // Hand the guard straight to the ticker: while the drag owns the playhead the
        // ticker must not write it back (main.ts:670 parity), and when the drag ENDS the
        // next engine value must land even if it happens to equal the last rendered one.
        _ticker.IsScrubbing = scrubbing;
        if (!scrubbing)
        {
            _ticker.Invalidate();
        }
    }

    /// <summary>Nearest clip boundary in the requested direction; the current position
    /// when there is none, which makes the seek a harmless no-op rather than a
    /// jump.</summary>
    private long AdjacentEdge(bool forward)
    {
        var position = CurrentPositionUs;
        // ±1ms of slack so repeated presses walk the list instead of sticking on the
        // edge the playhead is already sitting exactly on.
        if (forward)
        {
            foreach (var edge in _clipEdges)
            {
                if (edge > position + 1_000)
                {
                    return edge;
                }
            }
            return position;
        }
        for (var i = _clipEdges.Count - 1; i >= 0; i--)
        {
            if (_clipEdges[i] < position - 1_000)
            {
                return _clipEdges[i];
            }
        }
        return 0;
    }

    /// <summary>The playhead this region reasons from: the HOT scalar when it has one (it
    /// is the engine's own live value), falling back to the mirror before the first tick.
    /// Clip-edge navigation in particular must use the live value, or a press during
    /// playback would jump relative to a position up to a poll interval old.</summary>
    private long CurrentPositionUs => _ticker.LastPositionUs ?? _mirrorPositionUs;

    private async Task StepAsync(int deltaFrames)
    {
        var cmd = Cmd("step");
        cmd["data"] = new JsonObject { ["delta_frames"] = deltaFrames };
        await SendAsync(cmd, "step");
    }

    /// <summary>
    /// T-50-26: ONE seek in flight, latest-value-wins. A scrub drag produces a
    /// continuous stream of positions; queueing them all would back up the interop
    /// worker and make the playhead trail the pointer by the whole queue depth, so
    /// intermediate values are COALESCED away and only the newest is ever sent next.
    /// </summary>
    private async Task SeekAsync(long positionUs)
    {
        _pendingSeekUs = Math.Clamp(positionUs, 0, Math.Max(0, _durationUs));
        if (_seekInFlight)
        {
            return;
        }
        _seekInFlight = true;
        try
        {
            while (_pendingSeekUs is { } target)
            {
                _pendingSeekUs = null;
                await SendAsync(Seek(target), "seek");
            }
        }
        finally
        {
            _seekInFlight = false;
        }
    }

    private static JsonObject Cmd(string type) => new() { ["type"] = type };

    private static JsonObject Seek(long positionUs)
    {
        var cmd = Cmd("seek");
        cmd["data"] = new JsonObject { ["position_us"] = positionUs };
        return cmd;
    }

    /// <summary>
    /// The ONE place this region talks to <c>rudis_transport</c>.
    ///
    /// <para>Args shape read from the producer, not guessed:
    /// <c>rudis_transport</c> takes <c>TransportArgs { cmd: TransportCmd }</c>
    /// (commands.rs:274-276) and <c>TransportCmd</c> is ADJACENTLY tagged
    /// (<c>{"type": .., "data": {..}}</c>, transport.rs:11-12,21) — so the command
    /// object must be WRAPPED in a <c>cmd</c> key. Passing the bare command
    /// deserializes to nothing and the call fails at the domain layer with no compile
    /// error and no type mismatch, which is exactly what bit plan 50-05 (its deviation
    /// 1).</para>
    ///
    /// <para>On success the returned <c>Playback</c> envelope is applied IMMEDIATELY
    /// (frontend parity, main.ts:575-591) so a pressed button does not wait a poll
    /// interval — AND the <c>playback:changed</c> event is still listened for, because
    /// the engine also pushes state the shell never asked for (end-of-media auto-pause
    /// comes from the engine's own clock thread, self_advance.rs:174-183).</para>
    ///
    /// <para>The two failure layers stay SEPARATE (RESEARCH §2, UI-SPEC §5): a domain
    /// <c>{"Err": ..}</c> is a user-visible outcome; a <see cref="RudisStatus"/> other
    /// than <c>Ok</c> — <c>PanicCaught</c> (-99), <c>InvalidHandle</c>, … — is a
    /// transport FAULT and a different class of problem. Collapsing them would throw
    /// away the distinction the ABI was designed around.</para>
    /// </summary>
    private async Task<bool> SendAsync(JsonObject cmd, string label)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return false;
        }
        var args = new JsonObject { ["cmd"] = cmd };
        RudisResult<JsonElement> result;
        try
        {
            result = await engine.TransportAsync(args.ToJsonString());
        }
        catch (Exception ex)
        {
            SurfaceTransportFault(label, RudisStatus.PanicCaught, $"{ex.GetType().Name}: {ex.Message}");
            return false;
        }

        switch (result.Kind)
        {
            case RudisResultKind.Ok:
                ClearStatusBanner();
                App.Mirror?.ApplyPlaybackPayload(JsonNode.Parse(result.Value.GetRawText()));
                return true;

            case RudisResultKind.DomainError:
                SurfaceDomainError(label, result.Error);
                ReRenderFromMirror();
                return false;

            default:
                SurfaceTransportFault(label, result.Status, result.Error);
                ReRenderFromMirror();
                return false;
        }
    }

    /// <summary>After a refusal, re-derive from the MIRROR so the UI can never be left
    /// showing a state the backend rejected (UI-SPEC §5).</summary>
    private void ReRenderFromMirror()
    {
        if (App.Mirror is not null)
        {
            ApplyMirrorState(App.Mirror);
        }
    }

    // ── the two DISTINCT error surfaces (UI-SPEC §5, RESEARCH §2) ────────────

    /// <summary>A domain <c>{"Err": ..}</c>: the command was refused by the backend's
    /// own rules (e.g. <c>NoPreviewLoaded</c>). Visible, not swallowed — but not
    /// alarming, because nothing is broken.</summary>
    private void SurfaceDomainError(string label, string? error)
    {
        App.LogDiagnostic($"transport {label} REFUSED: {error}");
        ShowStatusBanner($"{label} refused: {error}", fatal: false);
    }

    /// <summary>A transport FAULT: <see cref="RudisStatus"/> != <c>Ok</c>. Rendered
    /// distinctly (an <c>accent</c>-filled badge with an ENGINE FAULT prefix) because a
    /// caught panic or a dead handle is a different class of problem from a refused
    /// command, and a user who cannot tell them apart cannot report either.</summary>
    private void SurfaceTransportFault(string label, RudisStatus status, string? detail)
    {
        App.LogDiagnostic($"transport {label} FAULT ({status}): {detail}");
        ShowStatusBanner($"ENGINE FAULT · {label} · {status}", fatal: true);
    }

    /// <summary>
    /// Who last wrote to the banner (plan 63-04).
    ///
    /// <para>It exists because a SECOND writer arrived. Until now this surface had one
    /// author — a refusal or a fault — so "is it up?" and "is it mine?" were the same
    /// question. Background cache progress makes them different, and getting that
    /// difference wrong in the obvious direction would erase the only thing telling a
    /// user that a command they issued was refused.</para>
    /// </summary>
    private TransportBannerOwner _bannerOwner = TransportBannerOwner.None;

    /// <summary>True while a render-cache status read is outstanding — the same
    /// single-flight shape every other rider on the 100 ms cold cycle uses.</summary>
    private bool _cachePollInFlight;

    /// <summary>Consecutive quiet polls since the cache line last had something to say.
    /// See <see cref="TransportCacheStatus.QuietPollsBeforeClearing"/> for why the line
    /// is held rather than dropped the instant one segment finishes.</summary>
    private int _cacheQuietPolls;

    /// <summary>
    /// Plan 71-03 (TRUST-03). The engine's process-lifetime <c>committed_total</c> at the
    /// FIRST <c>rendering</c> observation since this project opened, or <c>-1</c> before
    /// one. The line shows <c>committed_total - baseline</c> as "N ready" -- a count about
    /// THIS project this session, never a census (D-63-04-02). The first commit of the
    /// bake lands after the observation that set it, so a cold bake's first reading is 0.
    /// Reset by <see cref="OnProjectSwitched"/>.
    /// </summary>
    private long _cacheBaselineCommitted = -1;

    /// <summary>
    /// 71-REVIEW IN-02. Bumped by <see cref="OnProjectSwitched"/>. A cache-status poll
    /// captures it before its await and discards its result if a switch landed in
    /// between, so the previous project's reading can neither set the new project's
    /// baseline nor raise a banner about it.
    /// </summary>
    private int _projectSwitchGeneration;

    /// <summary>
    /// Plan 71-03. A different project is now open: the session tally starts again from
    /// that project's first render, and a cache line about the previous project comes
    /// down. A refusal/fault message is left alone (it is not ours to clear).
    /// </summary>
    internal void OnProjectSwitched()
    {
        _projectSwitchGeneration++;
        _cacheBaselineCommitted = -1;
        _cacheQuietPolls = 0;

        if (_bannerOwner == TransportBannerOwner.CacheStatus)
        {
            ClearStatusBanner();
        }
    }

    private void ShowStatusBanner(string message, bool fatal)
    {
        var resources = Application.Current.Resources;
        StatusBanner.Background = (Brush)resources[fatal ? "accent" : "bg-elevated"];
        StatusBannerText.Foreground = (Brush)resources[fatal ? "on-accent" : "text-secondary"];
        StatusBannerText.Text = message;
        ToolTipService.SetToolTip(StatusBanner, message);
        AutomationProperties.SetName(StatusBannerText, message);
        StatusBanner.Visibility = Visibility.Visible;

        // Plan 63-04. Claim the surface: from here until it is cleared, the cache poll
        // must not touch it (TransportCacheStatus.Arbitrate).
        _bannerOwner = TransportBannerOwner.Message;
    }

    private void ClearStatusBanner()
    {
        StatusBanner.Visibility = Visibility.Collapsed;
        StatusBannerText.Text = string.Empty;
        AutomationProperties.SetName(StatusBannerText, "Transport status");
        _bannerOwner = TransportBannerOwner.None;
    }

    // ── the render-cache status poll (plan 63-04, TRUST-03 / CACHE-01) ──────

    /// <summary>
    /// The 100 ms cold cycle's entry point into this region (63-CONTEXT D-10).
    ///
    /// <para><b>No new timer and NO seventh event tag.</b> <c>ring::EVENT_NAMES</c>
    /// stays at 6. <c>rudis_get_render_cache_status</c>'s own Rust doc says it is
    /// poll-only BY DECISION and that a push event may follow "when a shell region
    /// actually consumes it"; this is that region, three phases later.</para>
    ///
    /// <para>ONE argument-free read per tick, on the interop worker, against an export
    /// documented as never erroring and never computing — and pinned by
    /// <c>render_cache_job_status_never_starts_a_render</c>, which polls it a hundred
    /// times against a heavy section to prove the second half.</para>
    /// </summary>
    internal Task OnColdPollAsync()
    {
        if (_cachePollInFlight)
        {
            return Task.CompletedTask;
        }

        _cachePollInFlight = true;
        return PollRenderCacheStatusAsync();
    }

    /// <summary>
    /// One render-cache status pass: read, decide, and touch the banner only if the
    /// decision says this poll owns it.
    ///
    /// <para>The whole priority rule lives in
    /// <see cref="TransportCacheStatus.Arbitrate"/>, where it is asserted without a
    /// window. What is left here is the drawing.</para>
    ///
    /// <para>A TOTAL <c>try/catch</c>, and never <c>async void</c>: this runs on the UI
    /// thread from the cold cycle, and an unhandled fault there would take the window
    /// down over a decoration (the 50-04 pattern).</para>
    /// </summary>
    private async Task PollRenderCacheStatusAsync()
    {
        try
        {
            var engine = App.Engine;
            if (engine is null || engine.IsInvalid)
            {
                return;
            }

            var generation = _projectSwitchGeneration;
            var result = await engine.GetRenderCacheStatusAsync();
            if (generation != _projectSwitchGeneration)
            {
                // 71-REVIEW IN-02: a project switch landed during the await; this
                // reading is about the previous project.
                return;
            }

            // Plan 71-03. The session tally: baseline at the first `rendering` reading
            // (71-RESEARCH Pitfall 4), then the engine's own commit counter differenced
            // against it. Never cached_segments / heavy_segments (D-63-04-02).
            var total = TransportCacheStatus.ReadCommittedTotal(result);
            if (_cacheBaselineCommitted < 0
                && total >= 0
                && TransportCacheStatus.LineFor(result).Length > 0)
            {
                _cacheBaselineCommitted = total;
            }

            var line = TransportCacheStatus.LineFor(
                result,
                TransportCacheStatus.ReadyThisSession(_cacheBaselineCommitted, total));

            if (line.Length > 0)
            {
                _cacheQuietPolls = 0;
            }
            else if (_bannerOwner == TransportBannerOwner.CacheStatus
                     && ++_cacheQuietPolls < TransportCacheStatus.QuietPollsBeforeClearing)
            {
                // Held, not dropped: the bake is a sequence of renders with short gaps,
                // and blinking the strip once per segment reads as a fault.
                return;
            }

            switch (TransportCacheStatus.Arbitrate(_bannerOwner, line))
            {
                case TransportBannerAction.Show:
                    ShowCacheStatus(line, TransportCacheStatus.TooltipFor(result));
                    break;

                case TransportBannerAction.Clear:
                    ClearStatusBanner();
                    break;
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic(
                $"transport render-cache poll threw: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            _cachePollInFlight = false;
        }
    }

    /// <summary>
    /// Draw the cache line with the NON-FATAL visual set — <c>text-secondary</c> on
    /// <c>bg-elevated</c>, the same pair a domain refusal uses and deliberately not the
    /// <c>accent</c> fill a fault gets. Nothing is wrong; the app is working.
    ///
    /// <para>The UIA name is set to the line itself, following
    /// <see cref="ShowStatusBanner"/>'s existing discipline, which is what makes the
    /// VALUE readable at <c>Transport.StatusBanner</c> and therefore what makes
    /// 63-CONTEXT D-11 provable at all. The TOOLTIP carries the longer sentence.</para>
    /// </summary>
    private void ShowCacheStatus(string line, string tooltip)
    {
        var resources = Application.Current.Resources;
        StatusBanner.Background = (Brush)resources["bg-elevated"];
        StatusBannerText.Foreground = (Brush)resources["text-secondary"];
        StatusBannerText.Text = line;
        ToolTipService.SetToolTip(StatusBanner, tooltip);
        AutomationProperties.SetName(StatusBannerText, line);
        StatusBanner.Visibility = Visibility.Visible;
        _bannerOwner = TransportBannerOwner.CacheStatus;
    }

    // ── the readout ─────────────────────────────────────────────────────────

    /// <summary>
    /// The EMPTY state (UI-SPEC §5): with nothing previewable the readout reads
    /// <c>00:00:00.000</c> regardless of what any stale playback field says. When there IS
    /// something to preview the readout belongs to the hot path, so this only nudges the
    /// ticker rather than writing text — which keeps
    /// <see cref="SetTimecodeText"/> reachable from exactly one place.
    /// </summary>
    private void RenderTimecodeFromMirror()
    {
        if (!_ready)
        {
            SetTimecodeText(TimecodeFormatter.EmptyState);
            return;
        }
        _ticker.Invalidate();
    }

    /// <summary>
    /// The ONE place the readout's text and its UIA name are written.
    ///
    /// <para>⚠ UI-SPEC §6's trap: this is reached ONLY when the RENDERED value changed —
    /// the hot path's change detector guarantees that, and the empty state above is a
    /// one-shot. A per-tick UIA property-change notification is both an allocation source
    /// and an event storm, and plan 50-08 measures GC-delta-0 with a UIA client
    /// attached — a storm here would fail that gate.</para>
    /// </summary>
    private void SetTimecodeText(string text)
    {
        Timecode.Text = text;
        // UI-SPEC §6: Name = the formatted value, so UIA can read the playhead.
        AutomationProperties.SetName(Timecode, text);
    }

    /// <summary>
    /// The thin WinUI adapter between the UI-free <see cref="PlayheadTicker"/> and the
    /// visual tree. It is the ONLY WinUI-aware part of the hot path, which is what lets
    /// the ticker and formatter be measured headless.
    ///
    /// <para><b>The honest cost, stated where it is paid:</b> WinUI has no
    /// allocation-free <c>TextBlock.Text</c> set (it takes a <c>string</c>) and no unboxed
    /// <c>double</c> dependency-property set. So this adapter allocates ONE small string
    /// per RENDERED CHANGE — never per tick, which is the property SC-3 actually needs and
    /// the one <c>HotPathAllocationTests</c> measures. While paused that is zero
    /// allocations per second; during playback at millisecond precision it is bounded by
    /// the composition rate.</para>
    /// </summary>
    private sealed class Readout(Transport owner) : IPlayheadReadout
    {
        public void RenderTimecode(ReadOnlySpan<char> text) => owner.SetTimecodeText(new string(text));

        public void RenderPlayhead(long positionUs, long durationUs)
            // Moves a TranslateTransform and one Width — never re-created geometry, and
            // itself pixel-change-gated inside Scrubber.Render (UI-SPEC §4 rule 3).
            => owner.Scrubber.SetPositionFromEngine(positionUs);

        /// <summary>i64::MIN or a dead handle (T-50-25). Surfaced ONCE as a transport
        /// FAULT — the same visual class as a PanicCaught command — and the readout stops
        /// rather than rendering a sentinel.</summary>
        public void SurfaceFault()
            => owner.SurfaceTransportFault(
                "playhead poll", RudisStatus.PanicCaught,
                "rudis_get_playback_position returned the i64::MIN fault sentinel " +
                "(null handle or caught panic) - the readout is stopped, never showing a " +
                "sentinel as a position");
    }

    // ── responsive (handoff:118 / UI-SPEC §2) ───────────────────────────────

    private void OnTransportSizeChanged(object sender, SizeChangedEventArgs e)
        => ApplyResponsiveLayout(e.NewSize.Width);

    /// <summary>
    /// ⚠ MEASURED, not declarative. 50-05 implemented the textbook
    /// <c>AdaptiveTrigger</c>/<c>VisualState</c> arrangement in two placements at three
    /// widths and NO state ever applied (artifacts/50-05-regions.md §2.6). A
    /// <c>SizeChanged</c> handler keys off the REGION's own measured width, is a plain
    /// conditional, and is trivially checkable. Do not "restore" AdaptiveTriggers here
    /// without re-measuring.
    ///
    /// <para>Collapse ORDER is the handoff's: right utilities first, then the clip-edge
    /// cluster. <c>AutomationProperties.Name</c> is never touched — a UAT that breaks
    /// when the window narrows is worse than no UAT.</para>
    /// </summary>
    private void ApplyResponsiveLayout(double width)
    {
        RightUtilities.Visibility = width >= RightUtilitiesBreakpoint
            ? Visibility.Visible
            : Visibility.Collapsed;
        var roomy = width >= ClipEdgeNavBreakpoint;
        ClipEdgeNav.Visibility = roomy ? Visibility.Visible : Visibility.Collapsed;

        // Below the second breakpoint the CENTRED main group and the RIGHT-ALIGNED
        // timecode start to collide — measured at a 346px region: the loop glyph
        // overlapped the readout by ~6px. Tightening the main group's gap from the
        // handoff's 16 to its 8 (both values on the handoff's own spacing scale, both
        // tokens) buys back the width. The timecode itself NEVER collapses: it is the
        // region's whole point, while the utilities beside it are conveniences.
        var resources = Application.Current.Resources;
        MainGroup.Spacing = (double)resources[roomy ? "gap-transport-main-group" : "gap-toolbar"];
    }

    // ── startup ─────────────────────────────────────────────────────────────

    private void OnTransportLoaded(object sender, RoutedEventArgs e)
    {
        MeasureTimecodeTypography();
        ApplyResponsiveLayout(TransportRoot.ActualWidth);
    }

    /// <summary>
    /// UI-SPEC §9 UNVERIFIED item 8, answered BY MEASUREMENT rather than by citation:
    /// does the chosen mechanism actually give the timecode TABULAR figures?
    ///
    /// <para>The test is width equality across digit-varying strings at the real
    /// timecode style. A CONTROL measurement (the same strings with the tabular
    /// mechanism switched OFF) runs alongside it, because a pass with no control is
    /// indistinguishable from a measurement that cannot fail — if both come back equal,
    /// the measurement is vacuous and the number proves nothing.</para>
    ///
    /// <para>Runs ONCE, at region load, on the cold path. The result is rendered in the
    /// window's status block (and is therefore UIA-readable), which is how this plan's
    /// artifact captured it.</para>
    /// </summary>
    private void MeasureTimecodeTypography()
    {
        const string wide = "11:11:11.111";
        const string narrow = "00:00:00.000";

        // The shipping style, with the tabular declaration, and the same style with it
        // switched off.
        var shippedWide = MeasureTimecode(Timecode.FontFamily, wide, tabular: true);
        var shippedNarrow = MeasureTimecode(Timecode.FontFamily, narrow, tabular: true);
        var offWide = MeasureTimecode(Timecode.FontFamily, wide, tabular: false);
        var offNarrow = MeasureTimecode(Timecode.FontFamily, narrow, tabular: false);

        // APPARATUS CHECK. Georgia's figures are old-style and genuinely PROPORTIONAL,
        // so its two widths MUST differ. Without this, "the widths are equal" is
        // indistinguishable from "this measurement cannot detect a difference" — a
        // vacuous pass, which is exactly the failure mode a measured verification is
        // supposed to replace.
        var probeFont = new FontFamily("Georgia");
        var apparatusWide = MeasureTimecode(probeFont, wide, tabular: false);
        var apparatusNarrow = MeasureTimecode(probeFont, narrow, tabular: false);

        var shippedEqual = Math.Abs(shippedWide - shippedNarrow) < 0.01;
        var offEqual = Math.Abs(offWide - offNarrow) < 0.01;
        var apparatusDetects = Math.Abs(apparatusWide - apparatusNarrow) >= 0.01;
        TypographyNote = string.Create(
            CultureInfo.InvariantCulture,
            $"timecode font={Timecode.FontFamily.Source} {Timecode.FontSize}/{Timecode.FontWeight.Weight} " +
            $"tabular w({wide})={shippedWide:F3} w({narrow})={shippedNarrow:F3} equal={shippedEqual} · " +
            $"same font, alignment off: {offWide:F3}/{offNarrow:F3} equal={offEqual} · " +
            $"apparatus(Georgia, proportional figures): {apparatusWide:F3}/{apparatusNarrow:F3} " +
            $"detects_difference={apparatusDetects}");
        App.LogDiagnostic(TypographyNote);
    }

    private double MeasureTimecode(FontFamily family, string text, bool tabular)
    {
        var probe = new TextBlock
        {
            Text = text,
            FontFamily = family,
            FontSize = Timecode.FontSize,
            FontWeight = Timecode.FontWeight,
        };
        Typography.SetNumeralAlignment(
            probe, tabular ? FontNumeralAlignment.Tabular : FontNumeralAlignment.Normal);
        probe.Measure(new Windows.Foundation.Size(double.PositiveInfinity, double.PositiveInfinity));
        return probe.DesiredSize.Width;
    }
}
