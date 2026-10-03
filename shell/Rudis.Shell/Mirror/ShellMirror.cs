using System.Text.Json.Nodes;
using Rudis.Shell.Interop;

namespace Rudis.Shell.Mirror;

/// <summary>What one applied <c>project:changed</c> (or one full resync) changed.
/// Regions bind to this instead of re-reading the whole project.</summary>
/// <param name="Kind">The <c>PatchKind</c> that caused it, or
/// <see cref="ShellMirror.ResyncKind"/> for a full resync.</param>
/// <param name="Ids">The entity ids the patch named (empty for a resync).</param>
/// <param name="WasResync">True when the mirror was rebuilt wholesale.</param>
internal sealed record MirrorProjectChange(string Kind, IReadOnlyList<string> Ids, bool WasResync);

/// <summary>
/// SHELL-02 — the C# read-only mirror of backend truth (CLAUDE.md rule 4), and
/// the single most pattern-load-bearing type in the shell: Phases 51-54 inherit
/// this apply/resync machinery rather than inventing their own.
///
/// <para><b>Two cursors, and they are NOT the same number.</b> Conflating them is
/// the easiest way to get this wrong:</para>
/// <list type="bullet">
/// <item><see cref="RingSeq"/> — the EVENT RING cursor (<c>local_seq</c>). Ring-
///   global, assigned by <c>EventRing::push</c> (ring.rs:157-171), advanced by
///   EVERY record regardless of whether this shell renders it. Its only job is
///   "which records have I already been handed".</item>
/// <item><see cref="LastAppliedStoreSeq"/> — the STORE mutation counter, the
///   patch chain's baseline. Advanced only by an applied <c>project:changed</c>,
///   and re-learned from <c>rudis_get_current_seq</c> on resync. Its only job is
///   "is this patch the next one, or did I miss one".</item>
/// </list>
///
/// <para><b>The apply ladder is a PORT.</b> Its semantics are
/// <c>frontend/src/main.ts:1732-1780</c>'s, verbatim, because that ladder is
/// shipping and proven (Phase 43 LAT-03). The five steps are annotated at their
/// implementation sites in <see cref="ApplyProjectChangedAsync"/>.</para>
///
/// <para><b>Affinity, not thread-safety (T-50-16).</b> This type takes no locks
/// and must be touched from ONE logical flow. In the app that flow is the UI
/// thread: the interop worker owns the raw ABI calls, parses off-UI, and marshals
/// the parsed batch back via <c>DispatcherQueue.TryEnqueue</c> before any mutation
/// here. In tests the flow is the test method. Deliberately UI-FRAMEWORK-FREE —
/// this file references no WinUI type at all (the acceptance grep for the WinUI
/// root namespace over it returns ZERO), which is what makes the unit tier
/// possible and what Phases 51-54 inherit.</para>
/// </summary>
internal sealed class ShellMirror(IMirrorSource source, Action<string>? log = null)
{
    /// <summary>
    /// PatchKinds whose ENTIRE effect is "fields of entities that already exist in
    /// the mirror changed" — the only kinds <c>get_entities(ids)</c> can express.
    /// Transcribed verbatim from <c>main.ts:1669-1686</c>; the reasoning there is
    /// load-bearing and is restated because it is easy to "simplify" wrongly:
    ///
    /// <para><c>EntitySnapshot</c> is <c>Clip | MediaBinItem</c> and
    /// <c>Store::get_entities</c> looks ids up in the CURRENT project, silently
    /// omitting unknown ones (queries.rs:47-56). It therefore carries no track
    /// index, cannot represent an insertion, cannot report a deletion, and returns
    /// nothing at all for a track index, a folder path, an annotation id or the
    /// <c>"project"</c> singleton. Routing <c>clip_added</c> / <c>clip_removed</c>
    /// / <c>clip_split</c> / <c>track_added</c> / <c>annotation_added</c> /
    /// <c>project_switched</c> through it would leave the shell silently ignoring
    /// imports, drops, deletes, splits, undo of any of those, canvas ink and
    /// project switches. An ALLOW-LIST, not a deny-list: a kind this set does not
    /// know falls into the same safe full-resync branch.</para>
    /// </summary>
    private static readonly HashSet<string> InPlacePatchKinds =
    [
        // ⚠ `clip_moved` is deliberately absent — see NotEntityApplicableKinds below.
        "clip_trimmed",
        "clip_volume_changed",
        "clip_transform_changed",
        "clip_opacity_changed",
        "clip_crop_changed",
        // Single-clip field updates that never attach entities.
        "clip_alpha_mode_changed",
        "clip_keyframes_changed",
        "clip_text_changed",
        // Single media-bin-item field updates (folder / display name).
        "media_item_moved",
        "media_item_renamed",
    ];

    /// <summary>
    /// PatchKinds that CANNOT be applied from entities at all, whether the envelope
    /// carries them or the thin fallback fetches them. Checked BEFORE the entity merge,
    /// because the entity merge is what gets them wrong.
    ///
    /// <para><b>Why <c>clip_moved</c> is here (plan 52-07).</b> It used to be in
    /// <see cref="InPlacePatchKinds"/>, and it was wrong, and nothing could show that
    /// until the Timeline gained a cross-lane drag.
    /// <c>Command::MoveClipToTrack</c> emits <c>PatchKind::ClipMoved</c> — the very same
    /// kind the same-track <c>Command::MoveClip</c> emits
    /// (<c>crates/core/src/command.rs:855</c> and <c>:906-907</c>) — and a <c>Clip</c>
    /// entity <b>carries no track index</b>, as this type's own remarks above already
    /// say. So an in-place merge updates the clip's fields INSIDE THE TRACK THE MIRROR
    /// ALREADY HOLDS IT IN: a cross-track move lands on the backend and never reaches
    /// the shell. The clip stays drawn on its old lane, silently, until something else
    /// forces a resync.</para>
    ///
    /// <para><b>Undo is the worse half</b>, and the reason a producer-side fix would not
    /// have been enough: the inverse of a cross-track move is another cross-track move
    /// (<c>command.rs:909-925</c>), it arrives through <c>rudis_undo</c> rather than
    /// through whichever region sent the original, and <c>Store::undo</c> attaches
    /// <c>entities: None</c> (<c>store.rs:171-176</c>) — so the thin fallback would
    /// merge it onto the wrong lane with nothing anywhere to notice.</para>
    ///
    /// <para><b>The cost, and why it is the right trade.</b> One snapshot per clip MOVE.
    /// A move is a GESTURE — exactly what branch 5's own comment says a snapshot is
    /// worth paying for. The per-frame drag traffic this ladder exists to keep cheap is
    /// LOCAL by construction (D-11): a drag dispatches nothing until the pointer lifts,
    /// so what reaches here is one commit per gesture, not one per pixel.</para>
    ///
    /// <para><b>The structurally right repair</b> is for the patch to carry its
    /// destination track, and that is a <c>crates/core</c> change the engine-axis freeze
    /// forbids. Written up rather than worked around, so a later plan replaces this with
    /// the real fix instead of rediscovering the bug. <c>MirrorCrossTrackMoveTests</c>
    /// is the regression.</para>
    /// </summary>
    private static readonly HashSet<string> NotEntityApplicableKinds = ["clip_moved"];

    /// <summary>The synthetic <see cref="MirrorProjectChange.Kind"/> a full resync
    /// reports — a resync has no single PatchKind.</summary>
    public const string ResyncKind = "__resync__";

    private readonly IMirrorSource _source = source;
    private readonly Action<string>? _log = log;

    private JsonObject? _raw;
    private Project? _projection;
    private ulong _ringSeq;
    private ulong _lastAppliedStoreSeq;

    /// <summary>
    /// THE AUTHORITATIVE mirror: the project exactly as the engine serialized it,
    /// with in-place merges applied to the same node graph.
    ///
    /// <para>Raw on purpose (see the fidelity note in Models.cs): it is lossless
    /// where a nominal C# record would silently drop <c>rudis_core::Clip</c>'s six
    /// unprojected fields, and it never round-trips a number through a C# numeric
    /// type — which is what makes <c>JsonNode.DeepEquals</c> against a freshly
    /// fetched snapshot an EXACT test rather than a float-tolerance one.</para>
    /// </summary>
    public JsonObject? RawProject => _raw;

    /// <summary>The typed read projection over <see cref="RawProject"/>, derived
    /// lazily and cached until the next mutation. Regions bind to this; nothing
    /// ever writes it back, so the two representations cannot drift.</summary>
    public Project? Project => _projection ??= WireJson.FromNode<Project>(_raw);

    /// <summary>The ACTIVE monitor's playback (program or source per
    /// <c>preview_mode</c>) — frontend parity with <c>applyActivePlayback</c>
    /// (main.ts:251-257). Null until the first resync populates the mirror.</summary>
    public Playback? ActivePlayback { get; private set; }

    /// <summary>Latest <c>export:progress</c> percentage, or null when no export
    /// has reported. Consumed by the Toolbar's busy state (UI-SPEC §4).</summary>
    public double? ExportProgressPercent { get; private set; }

    /// <summary>The EVENT RING cursor — see the class remarks' two-cursor note.</summary>
    public ulong RingSeq => _ringSeq;

    /// <summary>The STORE baseline — see the class remarks' two-cursor note.</summary>
    public ulong LastAppliedStoreSeq => _lastAppliedStoreSeq;

    /// <summary>How many full resyncs have run. Observable so tests can assert
    /// "exactly ONE" and "no resync loop" numerically instead of by inspection.</summary>
    public int ResyncCount { get; private set; }

    /// <summary>True once a snapshot has been adopted.</summary>
    public bool IsAttached => _raw is not null;

    public event Action<MirrorProjectChange>? ProjectChanged;

    /// <summary>
    /// Plan 71-03. A <c>project:changed</c> of kind <see cref="ProjectSwitchedKind"/>
    /// arrived: a DIFFERENT document is now open (open-from-disk, new, or a save that
    /// minted a name). Raised BEFORE the full resync that kind always takes, so a
    /// subscriber can drop per-project session state before any region re-renders from
    /// the new snapshot. The resync itself still arrives as <see cref="ProjectChanged"/>
    /// with <see cref="ResyncKind"/>, which carries no memory of why it happened.
    /// </summary>
    public event Action? ProjectSwitched;

    /// <summary>The engine's <c>PatchKind::ProjectSwitched</c> on the wire (serde
    /// <c>snake_case</c>).</summary>
    public const string ProjectSwitchedKind = "project_switched";

    public event Action<Playback>? PlaybackChanged;

    public event Action<double>? ExportProgressChanged;

    /// <summary>
    /// Learn the baseline: one poll to discover the ring's newest assigned seq
    /// (whose records the snapshot below supersedes, so they are deliberately
    /// discarded), then the full recovery protocol — <c>rudis_get_snapshot</c> +
    /// <c>rudis_get_current_seq</c>, adopting the poll's <c>next_seq</c> as the ring
    /// cursor (RESEARCH §5; the header's documented recovery order).
    ///
    /// <para>Mechanically this is the same rebuild a resync performs, but it is
    /// deliberately NOT counted in <see cref="ResyncCount"/>: attach is the FIRST
    /// learning of the baseline, not a RECOVERY from a lost patch. Keeping the
    /// counter to recoveries only is what lets a test assert "a missed patch buys
    /// exactly ONE resync" as a bare number instead of an off-by-attach delta.</para>
    /// </summary>
    public async Task AttachAsync()
    {
        var probe = await _source.PollEventsAsync(0);
        var cursor = probe.Kind == RudisResultKind.Ok ? probe.Value!.NextSeq : 0UL;
        await RebuildAsync("initial attach (baseline learn, not a recovery)", cursor);
    }

    /// <summary>
    /// FETCH ONLY — the half of a cold cycle that is safe to await from OFF the UI
    /// thread, because it mutates nothing here. It reads the cursor synchronously on
    /// the caller's thread and then hands off to <c>RudisNative</c>, which does the
    /// ABI call, the length-checked buffer copy, the single native free and the JSON
    /// parse on the interop worker / thread pool — so no JSON of any size is ever
    /// parsed on the UI thread (50-02 §1.5(4)).
    ///
    /// <para>Pair it with <see cref="ApplyPollOutcomeAsync"/> marshalled BACK to the
    /// mirror's thread. <see cref="PollOnceAsync"/> is the single-threaded
    /// convenience form used by tests.</para>
    /// </summary>
    public Task<RudisResult<PollOutcome>> FetchPollAsync() => _source.PollEventsAsync(_ringSeq);

    /// <summary>One whole cold-path cycle on the CALLING flow: fetch then apply.
    /// Faults are logged and skipped — a failed poll is not a reason to resync,
    /// because the next poll re-asks from the same cursor and loses nothing.</summary>
    public async Task PollOnceAsync()
    {
        var poll = await FetchPollAsync();
        if (poll.Kind != RudisResultKind.Ok)
        {
            Log($"poll failed ({poll.Kind}: {poll.Error}) — cursor held at {_ringSeq}, retrying next tick");
            return;
        }
        await ApplyPollOutcomeAsync(poll.Value!);
    }

    /// <summary>
    /// Apply one <c>rudis_poll_events</c> outcome in ring order.
    ///
    /// <para><c>resync_required</c> short-circuits to a full resync: the ring
    /// guarantees the events list is EMPTY in that case (ring.rs:32-39) — never a
    /// silently truncated list — so there is nothing to apply first.</para>
    ///
    /// <para>Otherwise every record advances <see cref="RingSeq"/> whether or not
    /// this shell renders it. <c>canvas-pointer</c> (Phase 51) and
    /// <c>gen:job</c>/<c>gen:progress</c> (Phase 54) are SEQUENCED-BUT-UNCONSUMED
    /// for exactly that reason (UI-SPEC §4): dropping them would leave the cursor
    /// behind, re-deliver them forever, and — worse — desynchronise this cursor
    /// from the store baseline into spurious resyncs.</para>
    /// </summary>
    public async Task ApplyPollOutcomeAsync(PollOutcome outcome)
    {
        if (outcome.ResyncRequired)
        {
            await FullResyncAsync(
                $"ring reported resync_required (cursor {_ringSeq}, ring at {outcome.NextSeq}) — retained records were evicted",
                outcome.NextSeq);
            return;
        }

        foreach (var record in outcome.Events)
        {
            _ringSeq = Math.Max(_ringSeq, record.Seq);
            switch (record.Event)
            {
                case EventNames.ProjectChanged:
                    await ApplyProjectChangedAsync(record, outcome.NextSeq);
                    break;
                case EventNames.PlaybackChanged:
                    ApplyPlaybackChanged(record);
                    break;
                case EventNames.ExportProgress:
                    ApplyExportProgress(record);
                    break;
                case EventNames.CanvasPointer:
                case EventNames.GenJob:
                case EventNames.GenProgress:
                    // Sequenced, deliberately not rendered: canvas-pointer is Phase
                    // 51's and gen:* are Phase 54's. The cursor advance above IS the
                    // handling — see the method remarks (UI-SPEC §4).
                    break;
                default:
                    // T-50-19: an event name outside the ring's closed six is a
                    // contract drift. Sequenced (the cursor already moved) and
                    // logged, but NEVER executed and never silently swallowed.
                    Log($"unknown event name '{record.Event}' at ring seq {record.Seq} — sequenced, not executed");
                    break;
            }
        }

        // The ring's own contract: next_seq is "what the caller stores as its new
        // local_seq" (ring.rs:96-99). Adopting it also covers the caught-up case
        // (empty events) and — deliberately — the D-07 case where a record was lost
        // on THIS side: the ring has legitimately delivered it, so the cursor must
        // move on and the gap must be caught by the store baseline instead.
        _ringSeq = Math.Max(_ringSeq, outcome.NextSeq);
    }

    /// <summary>
    /// THE LADDER (main.ts:1732-1780, twinned step for step). Each numbered
    /// comment below is one step of the shipping algorithm.
    /// </summary>
    private async Task ApplyProjectChangedAsync(EventRecord record, ulong ringCursor)
    {
        var envelope = WireJson.FromNode<ProjectChangedEnvelope>(record.Payload);
        if (envelope is null)
        {
            // A payload that does not parse is a contract violation, not a missed
            // patch — resyncing is the only safe reconciliation (T-50-19).
            await FullResyncAsync(
                $"project:changed payload at ring seq {record.Seq} did not parse as a flattened envelope",
                ringCursor);
            return;
        }

        Log($"project:changed {envelope.Kind} [{string.Join(", ", envelope.Ids)}] base_seq={envelope.BaseSeq} seq={envelope.Seq}");

        // Plan 71-03. Announce a document switch BEFORE whichever resync branch below
        // handles it (its (0, 0) seq pair usually lands in the mismatch branch).
        if (string.Equals(envelope.Kind, ProjectSwitchedKind, StringComparison.Ordinal))
        {
            // 71-REVIEW IN-04: each subscriber is isolated, so a throwing handler can
            // neither skip the other subscribers nor the resync below.
            foreach (var handler in ProjectSwitched?.GetInvocationList() ?? Array.Empty<Delegate>())
            {
                try
                {
                    ((Action)handler)();
                }
                catch (Exception e)
                {
                    Log($"ProjectSwitched subscriber threw ({e.GetType().Name}: {e.Message}); the resync still runs");
                }
            }
        }

        // 1. base_seq mismatch -> exactly ONE full resync, logged (D-09). It CANNOT
        //    loop: the resync re-learns the baseline from get_current_seq, so the
        //    next envelope is compared against fresh truth rather than a stale one.
        if (envelope.BaseSeq != _lastAppliedStoreSeq)
        {
            await FullResyncAsync(
                $"project:changed seq mismatch (had {_lastAppliedStoreSeq}, patch expects {envelope.BaseSeq})",
                ringCursor);
            return;
        }

        // 2. Advance the baseline SYNCHRONOUSLY, before any await below. A second
        //    envelope arriving while THIS one's get_entities() is in flight then
        //    compares against the FRESH baseline instead of a stale one, so it
        //    cannot mis-detect a mismatch and take a spurious snapshot (the
        //    per-tool-call resync storm — main.ts:1747-1753, T-50-17).
        //    DO NOT move this line below the branches.
        _lastAppliedStoreSeq = envelope.Seq;

        if (NotEntityApplicableKinds.Contains(envelope.Kind))
        {
            // 2b. A kind whose effect an ENTITY cannot express, whoever supplies the
            //     entity. Checked BEFORE the merge below, because the merge is exactly
            //     what gets it wrong — see NotEntityApplicableKinds for the whole
            //     reasoning and for why a producer-side fix would not have covered undo.
            await FullResyncAsync(
                $"{envelope.Kind} cannot be applied from entities (a Clip entity carries no "
                + "track index, so a cross-track move would merge onto the lane it left)",
                ringCursor);
            return;
        }

        if (envelope.Entities is not null)
        {
            // 3. Post-mutation values rode along: merge in place, ZERO IPC. Any
            //    entity that cannot be placed means the two views disagree about
            //    what EXISTS — reconcile, do not guess.
            if (!ApplyEntities(envelope.Entities))
            {
                await FullResyncAsync(
                    $"{envelope.Kind} carried an entity the mirror does not hold", ringCursor);
                return;
            }
        }
        else if (InPlacePatchKinds.Contains(envelope.Kind))
        {
            // 4. The thin fallback (D-10): ONLY the named entities, never a
            //    snapshot. A wrong count means the backend could not resolve an id.
            var fetched = await _source.GetEntitiesAsync(envelope.Ids);
            if (fetched.Kind != RudisResultKind.Ok
                || fetched.Value!.Count != envelope.Ids.Count
                || !ApplyEntities(fetched.Value))
            {
                await FullResyncAsync(
                    $"get_entities fallback for {envelope.Kind} could not be applied", ringCursor);
                return;
            }
        }
        else
        {
            // 5. Structural / non-entity kind (import, drop, delete, split, undo,
            //    track add, folder op, canvas ink, project switch...). These are
            //    GESTURES, not the per-frame drag/trim/scrub traffic the
            //    incremental path exists to make cheap, so paying one snapshot for
            //    guaranteed correctness is the right trade — the same one the
            //    shipping frontend makes.
            await FullResyncAsync($"structural patch kind {envelope.Kind}", ringCursor);
            return;
        }

        InvalidateProjection();
        RefreshActivePlayback();
        ProjectChanged?.Invoke(new MirrorProjectChange(envelope.Kind, envelope.Ids, false));
    }

    /// <summary>
    /// Merge post-mutation entity values into the AUTHORITATIVE raw node, in place,
    /// with no backend call. Returns false if ANY entity named a target the mirror
    /// does not hold — the caller then full-resyncs (main.ts:1688-1716).
    /// </summary>
    private bool ApplyEntities(IReadOnlyList<JsonObject> entities)
    {
        if (_raw is null)
        {
            return false;
        }

        var allPlaced = true;
        foreach (var entity in entities)
        {
            var type = entity["type"]?.GetValue<string>();
            var id = entity["id"]?.GetValue<string>();
            if (id is null)
            {
                allPlaced = false;
                continue;
            }

            // The internal tag is a SIBLING of the entity's own fields
            // (command.rs:2055-2060), so stripping it yields exactly the entity's
            // own serialization — which is what keeps the merged node byte-faithful
            // to a snapshot. Cloned first because a JsonNode may have only one
            // parent; assigning a parented node throws.
            var value = (JsonObject)entity.DeepClone();
            value.Remove("type");

            var placed = type switch
            {
                EntityTypes.Clip => ReplaceClip(id, value),
                EntityTypes.MediaBinItem => ReplaceMediaItem(id, value),
                _ => false,
            };
            if (!placed)
            {
                allPlaced = false;
            }
        }
        return allPlaced;
    }

    /// <summary>Locate a clip by id ACROSS tracks (a clip's track is not carried on
    /// the patch) and replace it wholesale.</summary>
    private bool ReplaceClip(string id, JsonObject value)
    {
        if (_raw?["timeline"]?["tracks"] is not JsonArray tracks)
        {
            return false;
        }
        foreach (var track in tracks)
        {
            if (track?["clips"] is not JsonArray clips)
            {
                continue;
            }
            for (var i = 0; i < clips.Count; i++)
            {
                if (clips[i]?["id"]?.GetValue<string>() == id)
                {
                    clips[i] = value;
                    return true;
                }
            }
        }
        return false;
    }

    private bool ReplaceMediaItem(string id, JsonObject value)
    {
        if (_raw?["media_bin"] is not JsonArray bin)
        {
            return false;
        }
        for (var i = 0; i < bin.Count; i++)
        {
            if (bin[i]?["id"]?.GetValue<string>() == id)
            {
                bin[i] = value;
                return true;
            }
        }
        return false;
    }

    /// <summary>
    /// The authoritative <c>Playback</c> after a transport command
    /// (commands.rs:300-304). Applied to the ACTIVE monitor's slot on the raw node
    /// — frontend parity with <c>applyActivePlayback</c> (main.ts:251-257), which
    /// tracks both playbacks locally because <c>load_preview</c> is a transport and
    /// does not refetch the snapshot.
    ///
    /// <para>Playback is SESSION state mutated through <c>Store::transport</c>, NOT
    /// the undoable command path (model.rs:1211-1215), so this must never touch
    /// <see cref="LastAppliedStoreSeq"/>.</para>
    /// </summary>
    private void ApplyPlaybackChanged(EventRecord record)
    {
        if (!ApplyPlaybackPayload(record.Payload))
        {
            Log($"playback:changed at ring seq {record.Seq} did not parse as a Playback — ignored");
        }
    }

    /// <summary>
    /// Route ONE raw <c>Playback</c> payload into the active monitor's slot. Also
    /// the IMMEDIATE-apply surface for a transport call site: <c>rudis_transport</c>
    /// returns the authoritative <c>Playback</c> in its own envelope AND pushes
    /// <c>playback:changed</c>, and the shipping frontend applies BOTH
    /// (main.ts:575-591 plus the listener at main.ts:1636-1640) so the UI does not
    /// wait a poll interval to reflect a button the user just pressed. Applying the
    /// same value twice is idempotent.
    /// </summary>
    /// <returns>False when the payload was not a parseable Playback.</returns>
    public bool ApplyPlaybackPayload(JsonNode? payload)
    {
        var playback = WireJson.FromNode<Playback>(payload);
        if (playback is null)
        {
            return false;
        }

        if (_raw is not null && payload is JsonObject obj)
        {
            var slot = IsSourceMode() ? "source_playback" : "playback";
            _raw[slot] = obj.DeepClone();
            InvalidateProjection();
        }

        ActivePlayback = playback;
        PlaybackChanged?.Invoke(playback);
        return true;
    }

    /// <summary>
    /// Tell the mirror which monitor is active. <b>This is not the mirror inventing
    /// state — it is mirroring a backend change the ABI does not announce.</b>
    ///
    /// <para>FINDING (recorded per D-15 — a finding, not a licence to widen the Rust
    /// side): <c>TransportCmd::LoadPreview</c> writes <c>source_playback</c> AND flips
    /// <c>project.preview_mode</c> to Source (transport.rs:76-87), and
    /// <c>SetPreviewMode</c> obviously changes it too — but the ONLY event
    /// <c>rudis_transport</c> pushes is <c>playback:changed</c>, whose payload is a
    /// bare <c>Playback</c> carrying no monitor tag (commands.rs:300-304). A host
    /// therefore cannot route the payload to the right slot from the event alone.</para>
    ///
    /// <para>The shipping frontend solves it exactly this way, at the transport call
    /// site: <c>if (cmd.type === "load_preview") previewMode = "source"; else if
    /// (cmd.type === "set_preview_mode") previewMode = cmd.data.mode;</c>
    /// (main.ts:581-582). This method is that line's twin, and it is self-healing:
    /// <c>preview_mode</c> IS carried by every snapshot, so any missed note is
    /// corrected by the next resync.</para>
    /// </summary>
    /// <param name="mode"><c>"program"</c> or <c>"source"</c>.</param>
    public void NotePreviewMode(string mode)
    {
        if (_raw is null)
        {
            return;
        }
        _raw["preview_mode"] = mode;
        InvalidateProjection();
        RefreshActivePlayback();
    }

    /// <summary><c>export:progress</c> carries a BARE f64 percentage
    /// (ctx.rs:145-150) — not an object.</summary>
    private void ApplyExportProgress(EventRecord record)
    {
        double pct;
        try
        {
            pct = record.Payload?.GetValue<double>() ?? 0;
        }
        catch (Exception e) when (e is FormatException or InvalidOperationException)
        {
            Log($"export:progress at ring seq {record.Seq} was not a number — ignored");
            return;
        }
        ExportProgressPercent = pct;
        ExportProgressChanged?.Invoke(pct);
    }

    /// <summary>
    /// The recovery protocol, one place only (RESEARCH §5): whole snapshot, then
    /// adopt <c>rudis_get_current_seq</c> as the new store baseline, then adopt the
    /// poll's <c>next_seq</c> as the ring cursor.
    ///
    /// <para><b>Snapshot BEFORE seq, deliberately</b> (the 43-02 order): a mutation
    /// landing between the two reads leaves the NEXT envelope's <c>base_seq</c>
    /// behind this baseline, which trips the mismatch path and self-heals with one
    /// more resync — whereas seq-then-snapshot would leave the baseline AHEAD of the
    /// data and silently drop a patch forever.</para>
    ///
    /// <para><b>Cannot loop</b> (D-09): the baseline is re-learned from the backend,
    /// so the next envelope is compared against fresh truth. Every resync is logged
    /// with its cause.</para>
    /// </summary>
    public async Task FullResyncAsync(string cause, ulong? ringCursor = null)
    {
        // Counted as a RECOVERY (see AttachAsync's note): every increment here is a
        // resync the mirror was forced into, which is the number D-09's
        // "exactly ONE, and it is logged" property is asserted against.
        if (await RebuildAsync(cause, ringCursor))
        {
            ResyncCount++;
        }
    }

    /// <summary>The rebuild itself, shared by attach and recovery. Returns true iff
    /// the mirror was actually replaced.
    ///
    /// <para><b>WR-02 (50-REVIEW.md) fix:</b> <c>_raw</c> is committed the moment the
    /// snapshot itself resolves, BEFORE the second await
    /// (<c>_source.GetCurrentSeqAsync()</c>), not after it. <c>await</c> is a
    /// reentrancy point on the UI thread: while this method was suspended waiting on
    /// the SECOND call, the UI thread was free to run another already-in-flight
    /// continuation — e.g. <c>Transport.SendAsync</c>'s, which applies a fresh
    /// <c>Playback</c> via <see cref="ApplyPlaybackPayload"/> SYNCHRONOUSLY, in place,
    /// on whatever <c>_raw</c> happens to be at that moment (<c>ShellMirror.cs</c>'s
    /// own affinity-not-thread-safety note: this is safe because it is all one
    /// logical flow, but "one flow" still reenters at every <c>await</c>). The OLD
    /// code assigned <c>_raw = snapshot.Value</c> only after BOTH awaits, so a mutation
    /// landing in that window was silently discarded when the wholesale replacement
    /// ran — a real, if transient (self-healing within ~100ms via the next poll),
    /// lost-update. Committing right after the snapshot fetch does not change the
    /// documented snapshot-before-seq READ ordering (still correct for the
    /// <c>base_seq</c> self-heal reasoning below) — it only removes the window during
    /// the SECOND await where a concurrent direct mutator could be clobbered.</para>
    /// </summary>
    private async Task<bool> RebuildAsync(string cause, ulong? ringCursor)
    {
        Log($"full resync: {cause}");

        var snapshot = await _source.GetSnapshotAsync();
        if (snapshot.Kind != RudisResultKind.Ok)
        {
            // Leave the mirror as it was and do NOT move either cursor: a failed
            // resync must not be mistaken for a completed one. The next mismatch
            // (or poll) retries.
            Log($"full resync FAILED at get_snapshot ({snapshot.Kind}: {snapshot.Error}) — mirror unchanged");
            return false;
        }

        // Commit immediately — no further awaits before this. See the method
        // remarks (WR-02): this closes the lost-update window for anything that
        // reaches _raw between here and the get_current_seq await below.
        _raw = snapshot.Value;
        InvalidateProjection();

        var seq = await _source.GetCurrentSeqAsync();
        if (seq.Kind != RudisResultKind.Ok)
        {
            // NOTE (WR-02): unlike the get_snapshot failure above, _raw was ALREADY
            // committed to the fresh snapshot a few lines up — that is the whole
            // point of committing early. The cursors (_lastAppliedStoreSeq,
            // _ringSeq) are NOT advanced, so the next base_seq comparison almost
            // certainly mismatches and self-heals with one more resync (D-09: this
            // cannot loop, the baseline is always re-learned from the backend). Left
            // stale-but-honest rather than rolled back — a rollback here would need
            // to keep a copy of the PRE-resync _raw around for exactly this rare
            // edge case, which is more machinery than a self-healing gap warrants.
            Log($"full resync PARTIAL at get_current_seq ({seq.Kind}: {seq.Error}) — " +
                "_raw already advanced to the fetched snapshot, cursors NOT moved; next mismatch retries");
            return false;
        }

        _lastAppliedStoreSeq = seq.Value;
        if (ringCursor is { } cursor)
        {
            _ringSeq = Math.Max(_ringSeq, cursor);
        }
        InvalidateProjection();
        RefreshActivePlayback();
        Log($"full resync complete: store baseline {_lastAppliedStoreSeq}, ring cursor {_ringSeq}");
        ProjectChanged?.Invoke(new MirrorProjectChange(ResyncKind, [], true));
        return true;
    }

    private void InvalidateProjection() => _projection = null;

    private bool IsSourceMode()
        => _raw?["preview_mode"]?.GetValue<string>() == "source";

    /// <summary>Re-derive the active monitor's playback from the raw node after a
    /// snapshot or an in-place merge, so a resync cannot leave a stale playhead.</summary>
    private void RefreshActivePlayback()
    {
        var project = Project;
        if (project is null)
        {
            return;
        }
        var next = IsSourceMode() ? project.SourcePlayback : project.Playback;
        var changed = ActivePlayback != next;
        ActivePlayback = next;
        if (changed)
        {
            PlaybackChanged?.Invoke(next);
        }
    }

    private void Log(string line) => _log?.Invoke(line);
}
