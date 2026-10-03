using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media.Imaging;
using Microsoft.Windows.Storage.Pickers;
using Rudis.Shell.Interop;
using Rudis.Shell.Mirror;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;

namespace Rudis.Shell.Regions;

// ============================================================================
// THE MEDIA RELINK DECISION — recorded HERE, beside the code that implements it.
// ============================================================================
//
// The ROADMAP flags media relink as THE one hard-to-reverse item in an otherwise
// reversible phase, and requires it be "decided, implemented, and recorded with its
// reason — do not discover it later". A decision recorded only in a planning
// document is a decision the next reader of this file does not have.
//
// ---------------------------------------------------------------------------
// SLICE 1 — OPEN WITH OFFLINE PLACEHOLDERS. This is what ships.
// ---------------------------------------------------------------------------
//
// The project OPENS. Always. Missing media is DETECTED and RENDERED — dimmed with a
// warning mark, per the handoff (README:126) — and NOTHING is rewritten.
//
// Premiere Pro, DaVinci Resolve, Final Cut Pro and CapCut were all surveyed
// (60.1-RESEARCH § Finding E). They diverge on almost every detail of how they
// repair a broken link, and they agree completely on one thing: NO EDITOR REFUSES
// TO OPEN BECAUSE MEDIA IS MISSING. Every one of them opens with placeholders and
// offers a repair path. That unanimity across a professional NLE, a colourist's
// NLE, a prosumer NLE and a phone-first beginner app is a stronger signal than any
// individual relink mechanism — and a beginner's first encounter with a broken link
// must be a fixable state inside a working app, never a locked door.
//
// It is strictly better than what came before, which is the test that matters more
// than the size of the change: today a moved file surfaces as a DECODER ERROR AT
// PLAY TIME, with no indication of which file, or why, or that a file is the problem
// at all. Slice 1 turns an unattributed failure deep in playback into a named,
// visible, pre-play state on the exact tile at fault.
//
// Read-only, and NO .rud SCHEMA CHANGE — so the phase's reversibility budget stays
// unspent, and every option below is still open.
//
// ---------------------------------------------------------------------------
// SLICE 2 — FOLDER-LEVEL REPAIR. Deferred, and SPECIFIED rather than gestured at.
// ---------------------------------------------------------------------------
//
// Resolve's "Change Source Folder", which is the repair a beginner can actually
// complete: tell the user "some files are missing — where did you move them?", show
// ONE FolderPicker (the Microsoft.Windows.Storage.Pickers form with a WindowId in the
// constructor, copying ExportDialog.xaml.cs:178 — never the old Windows.Storage
// namespace, which silently reintroduces the InitializeWithWindow requirement),
// match by FILE NAME within that folder and its subdirectories, and rewrite the
// matching MediaBinItem.path values as ONE UNDOABLE COMMAND.
//
// Recorded in this phase's deferred-items.md. It is cheap when it comes, and that is
// a fact about the architecture rather than optimism: the Timeline stores only the
// stable mediaId on each clip and never a path (ARCHITECTURE.md:326), so repairing a
// project with 200 clips drawn from 6 media items is SIX field writes, not 200.
//
// ---------------------------------------------------------------------------
// EXPLICITLY REJECTED — named so a later plan cannot reintroduce them as kindnesses.
// ---------------------------------------------------------------------------
//
// * RELINKING BY SILENTLY REWRITING PATHS ON OPEN. A repair the user did not ask
//   for, applied to data they cannot undo, is the one shape that turns a recoverable
//   state into a LOST project: a wrong guess — the same filename in another folder, a
//   proxy mistaken for a master, an older version — overwrites the only record of
//   where the media actually was, and the user cannot undo it because they were never
//   told it happened. Any future auto-relink must be user-initiated, previewed and
//   undoable, which is exactly how slice 2 above is specified.
//
// * BUNDLING MEDIA INTO AN APP-MANAGED FOLDER (Final Cut's answer). Not this phase's
//   decision to make: STACK.md:161 already settled it, the other way — Rudis does not
//   copy media, "avoids doubling disk usage for a beginner's video files". A 4K-heavy
//   beginner library is the worst case for doubling disk, and it is the case Rudis is
//   for. Reopening that is a stack decision, not a phase decision.
//
// * REFUSING TO OPEN. No surveyed editor does it, and it withholds the very
//   information the user needs to fix the problem — which file — at the moment they
//   are trying to get back into their work.
//
// The fuller record, with citations and confidence markers:
// .planning/phases/60.1-project-save-open/artifacts/60.1-RELINK-DECISION.md

/// <summary>
/// The <c>MediaBin</c> region (design_handoff_rudis_editor/README.md:122 — the name is
/// the handoff's, verbatim, per CLAUDE.md rule 7).
///
/// <para><b>This class holds no authoritative state (rule 4).</b> The library — which
/// folders exist, which items are in them — is the backend's, and reaches here only
/// through <see cref="ShellMirror"/>. The region does not poll the ABI for LIBRARY
/// state; it is pushed by <c>MainWindow</c> on every <c>project:changed</c> (including
/// resyncs), exactly as <c>Toolbar.ApplyProjectState</c> and
/// <c>Transport.ApplyMirrorState</c> are. The one piece of state that IS the region's
/// own is which folder is drilled into, and that is view state by construction — see
/// <see cref="_currentFolder"/>.</para>
///
/// <para><b>⚠ ONE EXCEPTION, ADDED BY PLAN 60.1-07 — named here rather than left to be
/// discovered.</b> WHICH MEDIA FILES ARE MISSING FROM DISK is not library state, and
/// the mirror does not carry it: it is a fact about the FILESYSTEM, which can change
/// while the project is untouched (that is the entire point — restore a moved file and
/// the tile must recover without a reopen). So <see cref="PollOfflineIdsAsync"/> does
/// read the ABI directly, through <c>rudis_get_missing_media</c>, on this region's
/// EXISTING <c>project:changed</c> cadence — no new timer, no seventh event,
/// <c>EVENT_NAMES</c> still exactly 6. It READS; it never mutates. Rule 4 is about who
/// OWNS state, and the owner is still the backend.</para>
///
/// <para><b>What is computed where.</b> Everything decidable without a window lives in
/// the WinUI-free <c>Regions/MediaBin/</c> directory beside this file and is unit-tested
/// there: the level (<c>MediaBinLevel</c>), the trail (<c>MediaBinBreadcrumb</c>), and
/// every string a tile shows (<c>MediaBinTileFactory</c>). This file does layout,
/// events and lifetime, and nothing else. The directory is WinUI-free BY RULE and
/// mechanically gated (<c>MediaBinPurityGateTests</c>); nothing here may move into it.</para>
///
/// <para><b>Cold path, deliberately.</b> <see cref="ApplyMirrorState"/> runs at
/// <c>project:changed</c> cadence — user/agent rate, not per frame — so rebuilding the
/// tile list per call is correct and is what keeps the mirror authoritative (T-53-09).
/// This region takes no per-composition-tick subscription; the shell has exactly one
/// such call site and it is in <c>MainWindow</c>.</para>
///
/// <para><b>═══ TWO SEAMS ARE DELIBERATELY HALF-PROVEN HERE, WITH THE FAR HALF NAMED
/// (D-13) ═══</b></para>
///
/// <para>The failure mode this record exists to prevent is a LATER phase assuming the
/// MediaBin already proved something it structurally could not.</para>
///
/// <list type="bullet">
/// <item><b>The drag SOURCE is real</b> and carries v6.0's own custom format id,
/// spelled ONCE, in <c>MediaBinDragPayload.FormatId</c> — which is the string the
/// Timeline's drop target reads, never re-spells.
/// <b>UPDATED BY PHASE 53.1, because the half-proof recorded here turned out to be a
/// no-proof.</b> When this was written there was no drop target anywhere in the shell,
/// so the seam was declared half-built and deliberately left so. What that framing hid
/// is that the SOURCE half did not work either: plan 53.1-01 measured the items
/// control's built-in gesture pipeline never raising its drag-start event in this
/// unpackaged host, on a real mouse and under injected input alike. Phase 53.1 supplies
/// the drop target (plan 02, <c>Timeline.TryPlaceFromDropAsync</c>) and replaces the
/// trigger here (plan 03, <see cref="OnTilePointerPressed"/>). End-to-end drop is
/// re-checked at the <b>Phase 55</b> gate.</item>
///
/// <item><b>Double-clicking a video tile dispatches the REAL</b>
/// <c>rudis_transport load_preview</c>, and this phase asserts against a real engine
/// that the mirror's <c>source_playback</c> moved to that media id while the PROGRAM
/// monitor did not (<c>MediaBinImportTests</c>). Whether the Source monitor
/// <i>visually</i> shows that frame is <b>Phase 51's</b> to prove — the Preview region
/// has no source/program mode handling yet.</item>
/// </list>
/// </summary>
public sealed partial class MediaBin : UserControl
{
    /// <summary>
    /// PURE VIEW STATE — v6.0's <c>currentMediaFolder</c> (<c>main.ts:238</c>).
    ///
    /// <para>This chooses which level is RENDERED and nothing else. Drilling in and out
    /// sends no command and changes nothing the backend can see, which is why a click on
    /// a folder tile is instant and is not undoable: there is nothing to undo. The
    /// backend owns the folder registry; this owns a cursor into it.</para>
    ///
    /// <para>It is re-validated against reality on EVERY apply, because the folder it
    /// names can be deleted, renamed or undone away between one apply and the next
    /// (T-53-07).</para>
    /// </summary>
    private string _currentFolder = "";

    /// <summary>
    /// The last mirror handed in. Held ONLY so a drill-down can re-render the level
    /// without waiting for the next <c>project:changed</c> — the region reads it, never
    /// writes it, and never polls it.
    /// </summary>
    private ShellMirror? _mirror;

    /// <summary>
    /// v6.0's <c>selectedMediaId</c> (<c>main.ts:236</c>). Deliberately REMEMBERED
    /// across renders even when the item is not on the current level: drilling into a
    /// folder and back out restores the selection, and so does an undo that brings a
    /// deleted item back. v6.0 behaves the same way — it keeps the id and simply renders
    /// nothing as selected while the item is absent (<c>main.ts:347</c>).
    /// </summary>
    private string? _selectedMediaId;

    /// <summary>
    /// True while <see cref="ApplyMirrorState"/> is rebuilding the grid.
    ///
    /// <para>⚠ LOAD-BEARING, and it must exist BEFORE the selection does anything real.
    /// Assigning <c>ItemsSource</c> and re-asserting the selection both raise
    /// <c>SelectionChanged</c>, so without this guard a mirror-driven re-render is
    /// indistinguishable from a user click. Today that would only re-record an id the
    /// region already knows; the moment plan 53-03 dispatches a real source-preview
    /// command from selection, the same re-render would fire that command on every
    /// <c>project:changed</c> — i.e. roughly once per user edit, forever.</para>
    ///
    /// <para>⚠ EVERY set of this field is paired with a <c>finally</c> that clears it
    /// (all three call sites in <see cref="ApplyMirrorState"/>). Without that, a throw
    /// out of an <c>ItemsSource</c>/<c>SelectedItem</c> assignment — a container-generation
    /// fault, an OOM during image binding — would leave the flag stuck <c>true</c> and
    /// <see cref="OnSelectionChanged"/> would then swallow REAL keyboard and pointer
    /// selection for the rest of the session. The guard suppresses one re-render, never a
    /// user.</para>
    /// </summary>
    private bool _rebuilding;

    /// <summary>
    /// What the last COMPLETED render put on screen, or <c>null</c> when nothing has
    /// been rendered yet (or when the last attempt did not finish). Read only by the
    /// IN-01 guard in <see cref="ApplyMirrorState"/>; see
    /// <see cref="MediaBinRender"/> for why the comparison is by value.
    ///
    /// <para>⚠ FAIL-SAFE IN ONE DIRECTION ONLY, and the assignment order below is how
    /// that is bought: it is cleared BEFORE a rebuild and re-assigned only after the
    /// rebuild returns. A stale <c>null</c> costs one extra re-render; a stale SNAPSHOT
    /// would let the region skip a render the grid actually needed, which is the
    /// strictly worse bug of the two.</para>
    /// </summary>
    private MediaBinRender? _lastRender;

    /// <summary>
    /// The media ids whose files are NOT ON DISK right now — the handoff's
    /// offline/missing state (README:126), as last answered by
    /// <c>rudis_get_missing_media</c>.
    ///
    /// <para>Starts EMPTY and returns to empty whenever the poll cannot be read
    /// (T-60.1-20). "Nothing is offline" is the safe answer in both directions that
    /// matter: it is the state Rudis was in before this feature existed, whereas the
    /// opposite failure — painting every tile offline because a payload was malformed —
    /// would tell a beginner with a perfectly healthy project that the app had lost
    /// their work.</para>
    ///
    /// <para>A LIVE answer, never a stored one. Nothing about it is persisted; slice 1
    /// is read-only and adds no <c>.rud</c> field.</para>
    /// </summary>
    private IReadOnlySet<string> _offlineIds = MediaBinOffline.None;

    /// <summary>
    /// True while a missing-media poll is outstanding.
    ///
    /// <para>⚠ It does two jobs, and the second is the load-bearing one.
    /// <see cref="ApplyMirrorState"/> runs on EVERY <c>project:changed</c>, so without
    /// this a burst of edits would queue a stat-per-media-item behind each of them on
    /// the single interop worker. AND: the poll RE-ENTERS
    /// <see cref="ApplyMirrorState"/> when the answer changed, which would start
    /// another poll, which could change the answer again — the flag is cleared in a
    /// <c>finally</c> AFTER that re-render returns, so the nested apply finds it still
    /// set and the recursion terminates at depth one instead of chasing its own
    /// tail.</para>
    /// </summary>
    private bool _offlinePollInFlight;

    /// <summary>
    /// Every media id this region has an answer for, and what that answer was — the
    /// RAW engine state (<c>queued</c> / <c>running</c> / <c>ready</c> / <c>failed</c> /
    /// <c>cancelled</c> / <c>not_needed</c> / <c>none</c>), as last read from
    /// <c>rudis_get_proxy_status</c> (plan 63-04, TRUST-03).
    ///
    /// <para><b>The RAW state, not the displayed one, and that is what makes the fan-out
    /// bounded.</b> The tile stores the narrowed value (two states draw a badge, the rest
    /// draw nothing), but the poll has to know the difference between "nothing to show
    /// because the work finished" and "nothing to show because the work has not started"
    /// — the first is TERMINAL and is never asked about again, the second cannot happen
    /// (a heavy import registers <c>queued</c> synchronously, before the item reaches the
    /// mirror at all). An id absent from this map has never been asked.</para>
    ///
    /// <para>A LIVE answer, never a stored one; nothing here is persisted and no
    /// <c>.rud</c> field exists for it.</para>
    /// </summary>
    private IReadOnlyDictionary<string, string> _proxyStates = MediaBinProxy.None;

    /// <summary>
    /// Plan 71-03 (TRUST-03). The DISPLAY percent per in-flight media id — already
    /// clamped 0..99 by <see cref="MediaBinProxy.ProgressPercentFrom"/> and held
    /// non-decreasing by <see cref="MediaBinProxy.MonotonicPercent"/>. An id is present
    /// only while its job is in flight with a readable number; it is removed when the
    /// state leaves in-flight and the whole map is dropped wherever
    /// <see cref="_proxyStates"/> is, so a later rebuild starts from its own first report.
    /// </summary>
    private IReadOnlyDictionary<string, int> _proxyProgress = new Dictionary<string, int>(StringComparer.Ordinal);

    /// <summary>True while a proxy-status pass is outstanding. Same two jobs as
    /// <see cref="_offlinePollInFlight"/>, and cleared in the same <c>finally</c> AFTER
    /// any re-render, so the nested apply terminates at depth one.</summary>
    private bool _proxyPollInFlight;

    /// <summary>How many status calls the LAST completed pass issued, so the settled-bin
    /// claim is an observation rather than an argument. Logged only when it CHANGES —
    /// a line per 100 ms tick would be its own defect.</summary>
    private int _lastProxyPollCalls = -1;

    /// <summary>
    /// v6.0's <c>draggingMediaId</c> side channel (<c>main.ts:230-233</c>). v6 keeps it
    /// because an HTML5 <c>dataTransfer</c> payload is NOT readable during
    /// <c>dragover</c>, so the drop target's own ghost-preview sizing has no other way
    /// to know which clip is in flight.
    ///
    /// <para>Ported for the same reason plus one more: it is the only observable a
    /// UIA-level drag attempt can assert against, since a <c>DataPackage</c> in flight
    /// is not reachable from outside the process. Plan 53-04's introspection request
    /// publishes <see cref="DraggingMediaId"/> so that attempt has something REAL to
    /// check rather than a screenshot.</para>
    /// </summary>
    private string? _draggingMediaId;

    /// <summary>
    /// WHEN a press-and-move becomes a drag. See <see cref="MediaBinDragGesture"/> for why
    /// the rule is a WinUI-free class with its own assertions rather than four lines inside
    /// <see cref="OnTilePointerMoved"/>.
    /// </summary>
    private readonly MediaBinDragGesture _dragGesture = new();

    /// <summary>
    /// The container the current press landed on — the element <c>StartDragAsync</c> is
    /// called on, and the element <c>DragStarting</c>/<c>DropCompleted</c> are subscribed
    /// on for the life of that press.
    ///
    /// <para>⚠ It is a CONTAINER, and <c>GridView</c> recycles containers. That is why
    /// every subscription made here is torn down in <see cref="ClearDragState"/> and why
    /// <see cref="OnTilePointerPressed"/> tears down before it subscribes: a container
    /// that kept a stale subscription would raise <see cref="OnDragStarting"/> twice for
    /// one gesture, on behalf of a tile it is no longer showing.</para>
    /// </summary>
    private FrameworkElement? _dragSourceElement;

    /// <summary>
    /// The tile resolved at PRESS time, carried to <see cref="OnDragStarting"/>.
    ///
    /// <para>MEASURED (this phase, third instrumented run): the drag source element is
    /// the recycled <c>GridViewItem</c> container, and its <c>DataContext</c> is
    /// <c>null</c> — this shell's item templates bind with <c>x:Bind</c>, which fills the
    /// container's <c>Content</c>, not its <c>DataContext</c>. Re-resolving the tile from
    /// <c>sender.DataContext</c> inside <c>OnDragStarting</c> therefore produced
    /// <c>null</c>, cancelled the drag it was itself servicing, and surfaced as
    /// <c>StartDragAsync</c> throwing <c>TaskCanceledException</c> — no ghost, ever. The
    /// press already resolved and kind-checked the tile from <c>e.OriginalSource</c>
    /// (a template child that DOES carry the DataContext); this field carries that answer
    /// forward instead of asking a second element the same question and getting a
    /// different answer.</para>
    /// </summary>
    private MediaBinTile? _pressTile;

    /// <summary>
    /// How many pointer moves have been LOGGED for the current press — diagnostic only
    /// (plan 53.1-04). Two failure modes of the arm path are identical silence without
    /// it: "moves never delivered to <see cref="OnTilePointerMoved"/> at all" versus
    /// "delivered but <c>TryArm</c> kept answering no" (wrong button state, reset press).
    /// The first three moves of each press are logged with their button state; the
    /// counter caps the noise. The same rule that added the ARM log (53.1-03 run 2):
    /// instrument the decision point, not only the outcome.
    /// </summary>
    private int _movesLoggedThisPress;

    private static readonly List<MediaBinItem> NoItems = [];

    private static readonly List<string> NoFolders = [];

#if DEBUG
    /// <summary>
    /// Plan 53-04. The synthetic level for a <c>App.StartupSyntheticMediaBin</c> launch,
    /// built ONCE and then re-used: <see cref="ApplyMirrorState"/> runs on every
    /// structural <c>project:changed</c>, and rebuilding 5,000 tiles per apply would
    /// make the measurement a measurement of the allocator.
    /// </summary>
    private List<MediaBinTile>? _syntheticTiles;

    /// <summary>
    /// Plan 53-04. Republishes the realized-container count at a fixed cadence, and
    /// STARTED ONLY when a synthetic bin was asked for — a normal Debug session creates
    /// no timer and pays nothing (T-53-15); a Release build has neither the field nor
    /// the flag that would start it.
    ///
    /// <para>It is needed because <c>ContainerContentChanging</c> fires DURING
    /// realization: a count taken inside that handler is mid-batch and would report a
    /// number the control is still in the middle of producing. The timer gives the
    /// measurement a SETTLED value to poll for.</para>
    /// </summary>
    private Microsoft.UI.Dispatching.DispatcherQueueTimer? _introspectionTimer;
#endif

    public MediaBin()
    {
        InitializeComponent();

        // ═══ THE DRAG TRIGGER, wired here and not in XAML ═══
        //
        // Wired in code so each subscription sits beside the remark explaining why it
        // exists, and on `TileGrid` rather than per tile because containers are
        // virtualized and recycled — the grid is the one stable subscriber.
        //
        // ⚠ `handledEventsToo: true` on PointerPressed is LOAD-BEARING. The
        // `GridViewItem`'s own presenter marks PointerPressed Handled for its
        // pressed/selection visuals, so an ordinary `+=` would never see the press at
        // all and every drag would start from a press point that was never recorded.
        TileGrid.AddHandler(PointerPressedEvent, new PointerEventHandler(OnTilePointerPressed), true);

        // The remaining three are not marked Handled on the way up, but they are
        // subscribed the same way anyway: these are the TEARDOWN paths, and a teardown
        // that a control can suppress by claiming an event is not a teardown.
        TileGrid.AddHandler(PointerReleasedEvent, new PointerEventHandler(OnTilePointerReleased), true);
        TileGrid.AddHandler(PointerCaptureLostEvent, new PointerEventHandler(OnTilePointerReleased), true);
        TileGrid.AddHandler(PointerCanceledEvent, new PointerEventHandler(OnTilePointerReleased), true);

        // ⚠ `handledEventsToo: true` is LOAD-BEARING HERE TOO, and its absence is what
        // plan 03's first checkpoint measured: RESULT: NO_GHOST on a real tile, while
        // plan 01's bare `Border` probe showed a ghost from the very same
        // `StartDragAsync` call. The difference is the container. `GridViewItem` sits in
        // a scrolling `ScrollViewer` (VerticalScrollBarVisibility="Auto" below), which
        // marks PointerMoved Handled while it evaluates the gesture for panning — so an
        // ordinary `+=` never sees a move, `MediaBinDragGesture.TryArm` never fires, and
        // `StartDragAsync` is never CALLED at all. No throw, no log, no ghost: the exact
        // silent-failure shape that cost this project a phase.
        TileGrid.AddHandler(PointerMovedEvent, new PointerEventHandler(OnTilePointerMoved), true);

#if DEBUG
        if (App.StartupSyntheticMediaBin > 0)
        {
            _introspectionTimer = DispatcherQueue.CreateTimer();
            _introspectionTimer.Interval = TimeSpan.FromMilliseconds(250);
            _introspectionTimer.IsRepeating = true;
            _introspectionTimer.Tick += (_, _) => PublishIntrospection();
            _introspectionTimer.Start();
        }
#endif
    }

    /// <summary>Raised by <c>MediaBin.ImportMediaButton</c>. <c>MainWindow</c> routes it
    /// into the shell's ONE import routine, which is Toolbar's — this region deliberately
    /// does not open a second path to <c>rudis_import_media</c>.</summary>
    public event Action? ImportMediaRequested;

    // There is deliberately NO `ImportFolderRequested` event beside the one above, and
    // the asymmetry is the point. File import is routed through `MainWindow` because the
    // shell keeps exactly ONE `rudis_import_media` routine and it is Toolbar's — a second
    // path would be the thing to prevent. Folder import has no other owner: `+ Import
    // folder…` is the only affordance in the app that reaches
    // `rudis_import_media_folder`, so it calls it directly (see PickAndImportFolderAsync)
    // and the placeholder event plan 53-02 raised has been retired with the placeholder.

    /// <summary>
    /// Bind this region to its window. Needed by the folder picker, which takes the
    /// window's <c>WindowId</c> — the same reason <c>Toolbar</c> has one.
    /// </summary>
    public void AttachToWindow(Window window) => HostWindow = window;

    /// <summary>The hosting window, or null before <see cref="AttachToWindow"/>.</summary>
    internal Window? HostWindow { get; private set; }

    /// <summary>
    /// Render one level from MIRRORED state. Called on every <c>project:changed</c>
    /// (including resyncs) by <c>MainWindow</c>, and re-entered by this region itself
    /// whenever the drilled path changes.
    ///
    /// <para>This is v6.0's <c>renderMediaBin</c> (<c>main.ts:455-525</c>), reordered
    /// only to hoist the mirror read: guard the drilled path, rebuild the trail, compute
    /// the level, branch on empty, otherwise bind folders-then-items and restore the
    /// selection.</para>
    /// </summary>
    internal void ApplyMirrorState(ShellMirror mirror)
    {
        _mirror = mirror;

#if DEBUG
        // Plan 53-04, SC-2's measurement launch. A synthetic level REPLACES the mirrored
        // one for this launch — it is not merged into it and it never travels back: no
        // command is dispatched, no FFI call is made, and the engine's store is not
        // touched, so nothing here can reach the developer's real project state.
        // Argv-gated even in Debug, and compiled out of Release entirely.
        if (App.StartupSyntheticMediaBin > 0)
        {
            _syntheticTiles ??= Rudis.Shell.Introspection.MediaBinSyntheticBin.Build(
                App.StartupSyntheticMediaBin);

            if (!ReferenceEquals(TileGrid.ItemsSource, _syntheticTiles))
            {
                _rebuilding = true;
                try
                {
                    TileGrid.ItemsSource = _syntheticTiles;
                }
                finally
                {
                    _rebuilding = false;
                }
            }

            EmptyState.Visibility = Visibility.Collapsed;
            TileGrid.Visibility = Visibility.Visible;
            PublishIntrospection();
            return;
        }
#endif

        // Plan 60.1-07. Re-ask the filesystem on the cadence this region already
        // re-renders on — deliberately NOT a new timer. It is placed AFTER the
        // synthetic-bin branch above so a measurement launch never touches the ABI, and
        // BEFORE the empty-level branch below so a bin that is empty right now still
        // learns the answer for when it is not.
        StartOfflinePoll();

        // Plan 63-04. Placed beside the offline poll, for the same reasons in the same
        // order, and with one difference worth stating: this one ALSO rides the 100 ms
        // cold cycle (MainWindow.ApplyBatchAsync), because a proxy finishing is not a
        // project change and would otherwise be noticed only at the next unrelated edit —
        // which is to say, never, for a user who imported some clips and then sat still.
        // Kept here as well so the region re-asks immediately after an import rather than
        // up to a tick later; both entries are single-flighted through the same flag.
        StartProxyPoll();

        var items = mirror.Project?.MediaBin ?? NoItems;
        var folders = mirror.Project?.MediaFolders ?? NoFolders;

        // T-53-07: the folder this region is drilled into may no longer exist. Walk up
        // to one that does before anything is computed against it.
        _currentFolder = MediaBinLevel.ResolveExistingFolder(_currentFolder, items, folders);
        Breadcrumb.ItemsSource = MediaBinBreadcrumb.Build(_currentFolder);

        var level = MediaBinLevel.ListLevel(items, folders, _currentFolder);

        if (level.Folders.Count == 0 && level.Items.Count == 0)
        {
            // TWO strings, because the two situations need two different answers: an
            // empty BIN is asking for an import, an empty FOLDER is not. Both are
            // v6.0's, verbatim (main.ts:509-510).
            var message = _currentFolder.Length == 0
                ? "No media imported yet"
                : "This folder is empty";

            EmptyState.Text = message;

            // The UIA Name is set explicitly rather than left to the text peer's
            // default, because plan 53-02's own UIA proof reads this Name back off the
            // running app — an assertion that depended on a peer's fallback behaviour
            // would be testing WinUI, not the region.
            AutomationProperties.SetName(EmptyState, message);
            EmptyState.Visibility = Visibility.Visible;

            // Nothing is rendered on this branch, so the IN-01 guard has nothing to
            // compare against next time: an empty level must always fall through to a
            // full rebuild when it stops being empty.
            _lastRender = null;

            _rebuilding = true;
            try
            {
                TileGrid.Visibility = Visibility.Collapsed;
                TileGrid.ItemsSource = null;
            }
            finally
            {
                _rebuilding = false;
            }
#if DEBUG
            PublishIntrospection();
#endif
            return;
        }

        EmptyState.Visibility = Visibility.Collapsed;
        TileGrid.Visibility = Visibility.Visible;

        // A FRESH list of FRESH tiles every time. Container identity is by REFERENCE, so
        // reusing tile instances is how a recycled container ends up convinced it is
        // already showing the right thing (MediaBinTile's own remarks).
        var tiles = MediaBinTileFactory.ForLevel(level, _offlineIds, _proxyStates, _proxyProgress);

        // Re-assert the selection if the selected item is still on THIS level; otherwise
        // the control shows nothing selected while the id itself is kept (see
        // _selectedMediaId).
        var selected = MediaBinRender.SelectionFor(tiles, _selectedMediaId);

        // ═══ IN-01: AN UNCHANGED LEVEL IS NOT PUSHED AT THE CONTROL AT ALL ═══
        //
        // This method runs on EVERY project:changed, and most of them have nothing to
        // do with the library — a trim, a split, a place-clip, a detach-audio, an undo.
        // Rebinding ItemsSource for one of those re-realizes every visible container
        // from phase 0, which resets each thumbnail to its placeholder and decodes every
        // poster off disk again: editing a clip blanked the whole bin.
        //
        // The tiles above are still built FRESH every time, because tile freshness is
        // what keeps a recycled container from serving a stale poster (D-06/D-07 — see
        // MediaBinTile's remarks, and MediaBinRender's header for why mutating a
        // collection in place was rejected rather than overlooked). What is skipped is
        // only the PUSH, and only when pushing could not change anything visible:
        // MediaBinRender compares BY VALUE (reference equality on a rebuilt list is
        // never true, so a guard written that way would be dead code) across every
        // field of every tile, the drilled folder, the remembered selection, and — the
        // last mile — the selection the control is actually holding right now.
        if (_lastRender is not null
            && _lastRender.Matches(_currentFolder, _selectedMediaId, tiles)
            && MediaBinRender.SameSelection(TileGrid.SelectedItem as MediaBinTile, selected))
        {
#if DEBUG
            PublishIntrospection();
#endif
            return;
        }

        // Cleared BEFORE the rebuild, re-assigned only after it returns: a throw out of
        // the assignments below must leave the guard DISARMED (one extra re-render next
        // time) rather than describing a level the control may no longer be showing.
        _lastRender = null;

        _rebuilding = true;
        try
        {
            TileGrid.ItemsSource = tiles;
            TileGrid.SelectedItem = selected;
        }
        finally
        {
            _rebuilding = false;
        }

        _lastRender = new MediaBinRender(_currentFolder, _selectedMediaId, tiles);
#if DEBUG
        PublishIntrospection();
#endif
    }

    // ── the missing-media poll (plan 60.1-07, relink slice 1) ────────────────

    /// <summary>
    /// Ask the engine which media files are gone, if nothing is already asking.
    ///
    /// <para>A synchronous starter for an asynchronous <see cref="Task"/> that carries
    /// its own TOTAL <c>try/catch</c> — never <c>async void</c>, which would put a
    /// faulted poll on the dispatcher with no handler and take the window down over a
    /// decoration. The grep over <c>shell/</c> stays at zero; this is the same shape
    /// <see cref="LoadPosterAsync"/> and <see cref="ImportFolderAsync"/> are started
    /// with (the 50-04 pattern).</para>
    /// </summary>
    private void StartOfflinePoll()
    {
        if (_offlinePollInFlight)
        {
            return;
        }

        _offlinePollInFlight = true;
        _ = PollOfflineIdsAsync();
    }

    /// <summary>
    /// One missing-media poll: read, project, and re-render ONLY if the answer moved.
    ///
    /// <para><b>Why a POLL and not an event.</b> 60.1-RESEARCH § Finding E wanted the
    /// missing ids returned in the open envelope. This is the one place the
    /// implementation departs from the research, and it is a deliberate improvement
    /// rather than a shortcut: a poll RE-ANSWERS the question, so a file restored while
    /// the project is open clears its tile without a reopen, which an envelope field
    /// could not do. It also keeps <c>EVENT_NAMES</c> at 6 — where it has stood through
    /// five consecutive one-export phases — and rides the same cold retrieval convention
    /// as waveform peaks, filmstrip strips, proxy status and render-cache status.</para>
    ///
    /// <para><b>The re-render is CONDITIONAL, and that is what makes the poll safe to
    /// run on every apply.</b> An unchanged answer returns without touching the control,
    /// so the IN-01 guard is never even consulted; a changed answer re-enters
    /// <see cref="ApplyMirrorState"/>, where the guard now DOES see a difference because
    /// <c>IsOffline</c> joined <c>MediaBinRender.SameTile</c>. Without that, this method
    /// would set the field correctly and repaint nothing.</para>
    ///
    /// <para><b>Failure leaves nothing offline.</b> A refusal, a transport fault or an
    /// unreadable payload all arrive here as an empty set from
    /// <see cref="MediaBinOffline.IdsFrom"/> and are logged; an engine that is missing or
    /// disposed returns early and leaves the previous answer alone. Neither path can
    /// mark a healthy library broken (T-60.1-20).</para>
    ///
    /// <para>The <c>await</c> resumes on the UI thread — the same contract
    /// <see cref="LoadPosterAsync"/> already depends on when it assigns an image source
    /// after awaiting a decode — which is what lets the re-render happen inline.</para>
    /// </summary>
    private async Task PollOfflineIdsAsync()
    {
        try
        {
            var engine = App.Engine;
            if (engine is null || engine.IsInvalid)
            {
                return;
            }

            var result = await engine.GetMissingMediaAsync();

            if (result.Kind != RudisResultKind.Ok)
            {
                // Not a user-facing refusal: nobody asked for this, it runs on a
                // cadence, and the honest fallback is the state before the feature.
                App.LogDiagnostic(
                    $"MediaBin missing-media poll FAILED ({result.Kind}/{result.Status}): {result.Error}");
            }

            var next = MediaBinOffline.IdsFrom(result);

            if (next.SetEquals(_offlineIds))
            {
                return;
            }

            App.LogDiagnostic(
                $"MediaBin missing media: {_offlineIds.Count} -> {next.Count} offline item(s)");

            _offlineIds = next;

            if (_mirror is not null)
            {
                ApplyMirrorState(_mirror);
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic(
                $"MediaBin missing-media poll threw: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            // ⚠ AFTER the re-render above, not before: see _offlinePollInFlight.
            _offlinePollInFlight = false;
        }
    }

    // ── the proxy-status poll (plan 63-04, TRUST-03 / PROXY-02) ─────────────

    /// <summary>
    /// The 100 ms cold cycle's entry point into this region — the FIFTH rider on Phase 50
    /// D-06's existing poll, after waveform peaks and filmstrip strips (63-CONTEXT D-10).
    ///
    /// <para><b>No new timer and NO seventh event tag.</b> <c>ring::EVENT_NAMES</c> stays
    /// at 6 — the closed set both shells share — and both status exports' own Rust docs
    /// say in so many words that they are poll-only BY DECISION, waiting for a shell
    /// region to consume them. This is that region.</para>
    ///
    /// <para>Returns a <see cref="Task"/> the caller discards, exactly as
    /// <c>Timeline.OnColdPollAsync</c> is discarded, and carries its own TOTAL
    /// <c>try/catch</c> so a faulted poll can never reach the dispatcher unhandled.</para>
    /// </summary>
    internal Task OnColdPollAsync()
    {
        StartProxyPoll();
        return Task.CompletedTask;
    }

    /// <summary>Start one pass if nothing is already asking — the 50-04 shape: a
    /// synchronous starter for an asynchronous <see cref="Task"/>, never
    /// <c>async void</c>.</summary>
    private void StartProxyPoll()
    {
        if (_proxyPollInFlight)
        {
            return;
        }

        _proxyPollInFlight = true;
        _ = PollProxyStatusAsync();
    }

    /// <summary>
    /// One proxy-status pass: ask about the items that can still move, and re-render ONLY
    /// if what a tile would DRAW changed.
    ///
    /// <para><b>Per-item fan-out, bounded at the decision (T-63-13).</b>
    /// <see cref="MediaBinProxy.IdsToPoll"/> returns only the ids nothing is known about
    /// plus the ids last seen <c>queued</c>/<c>running</c>. A bin whose every item has
    /// settled therefore issues ZERO ABI calls per tick, however large it is, and the
    /// calls that are issued are pure reads that "never error, never compute". The count
    /// per pass is logged when it changes so that claim is an observation.</para>
    ///
    /// <para><b>The re-render is CONDITIONAL on the DISPLAY state, not the raw one.</b>
    /// <c>ready -&gt; none</c> and <c>failed -&gt; cancelled</c> both draw nothing before and
    /// nothing after; repainting the level for them would blank every poster on a cadence
    /// for no visible reason. Only a transition into or out of a badge repaints.</para>
    ///
    /// <para><b>Failure leaves the previous answer standing.</b> A refusal, a transport
    /// fault or an unreadable payload all arrive as <see langword="null"/> from
    /// <see cref="MediaBinProxy.StateFrom"/> and are SKIPPED — never written, never
    /// latched terminal. An engine that answers badly for one tick is re-asked on the
    /// next, and a badge cannot disappear permanently on a single hiccup (T-63-14).</para>
    ///
    /// <para>The <c>await</c>s resume on the UI thread, which is what lets the re-render
    /// happen inline — the same contract <see cref="PollOfflineIdsAsync"/> depends on.</para>
    /// </summary>
    private async Task PollProxyStatusAsync()
    {
        try
        {
            var engine = App.Engine;
            if (engine is null || engine.IsInvalid)
            {
                return;
            }

            var items = _mirror?.Project?.MediaBin ?? NoItems;
            var currentIds = new HashSet<string>(StringComparer.Ordinal);
            foreach (var item in items)
            {
                currentIds.Add(item.Id);
            }

            // A remembered id the bin no longer carries means this map is describing a
            // PREVIOUS library — a project switch, or a deleted item. Every remembered
            // answer may be stale (project open RE-ARMS generation for heavy media), so
            // the whole map goes and one pass re-establishes it.
            if (MediaBinProxy.IsStaleFor(_proxyStates, currentIds))
            {
                _proxyStates = MediaBinProxy.None;
                _proxyProgress = new Dictionary<string, int>(StringComparer.Ordinal);
            }

            var due = MediaBinProxy.IdsToPoll(currentIds, _proxyStates);

            if (_lastProxyPollCalls != due.Count)
            {
                _lastProxyPollCalls = due.Count;
                App.LogDiagnostic(
                    $"MediaBin proxy poll: {due.Count} status call(s) per tick over "
                    + $"{currentIds.Count} item(s)");
            }

            if (due.Count == 0)
            {
                return;
            }

            var next = new Dictionary<string, string>(_proxyStates, StringComparer.Ordinal);
            var nextProgress = new Dictionary<string, int>(_proxyProgress, StringComparer.Ordinal);
            var redraw = false;
            var progressMoved = false;

            foreach (var id in due)
            {
                // Plan 71-03: the result is kept, because the SAME read carries both the
                // state and (while running) the engine's progress_permille.
                var result = await engine.GetProxyStatusAsync(id);
                var state = MediaBinProxy.StateFrom(result);
                if (state is null)
                {
                    // Unreadable. Leave whatever was known; do NOT latch it terminal.
                    continue;
                }

                var pct = MediaBinProxy.ProgressPercentFrom(result);
                var previousPct = nextProgress.TryGetValue(id, out var known) ? known : -1;
                // 71-REVIEW WR-04: monotonic only across consecutive `running` reads, so
                // a queued read (a re-armed job) resets the entry.
                next.TryGetValue(id, out var previous);
                var displayPct = MediaBinProxy.DisplayPercentFor(previous, previousPct, state, pct);

                if (displayPct >= 0)
                {
                    nextProgress[id] = displayPct;
                }
                else
                {
                    nextProgress.Remove(id);
                }

                if (displayPct != previousPct)
                {
                    progressMoved = true;
                }

                next[id] = state;

                if (!string.Equals(
                        MediaBinProxy.DisplayStateFor(previous),
                        MediaBinProxy.DisplayStateFor(state),
                        StringComparison.Ordinal))
                {
                    redraw = true;
                    App.LogDiagnostic(
                        $"MediaBin proxy status '{id}': {previous ?? "(unknown)"} -> {state}");
                }
            }

            _proxyStates = next;
            _proxyProgress = nextProgress;

            if (redraw && _mirror is not null)
            {
                ApplyMirrorState(_mirror);
            }
            else if (progressMoved)
            {
                // Plan 71-03. A percent that moved WITHOUT a state change is patched onto
                // the realized containers in place. Routing it through ApplyMirrorState
                // would change a tile field, fail the IN-01 guard and REBIND the whole
                // level (re-decoding every poster) up to ten times a second for the length
                // of an encode, which is the "editing blanked the bin" defect IN-01 exists
                // to prevent. The next real re-render carries the value in the tile field
                // (SameTile compares it), and phase 0 reads the live map, so a recycled
                // container can never show an older number than this.
                RefreshProxyProgressInPlace();
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic(
                $"MediaBin proxy poll threw: {ex.GetType().Name}: {ex.Message}");
        }
        finally
        {
            // ⚠ AFTER the re-render above, not before: see _proxyPollInFlight.
            _proxyPollInFlight = false;
        }
    }

#if DEBUG
    /// <summary>
    /// Plan 53-04. Push this region's own numbers into the Debug-only readout
    /// <see cref="Rudis.Shell.Introspection.IntrospectionHook"/>'s <c>mediabin</c>
    /// request serves. UI-THREAD ONLY, and that is the whole point: asking a
    /// <c>UIElement</c> for its realized children from the pipe's background thread is
    /// <c>RPC_E_WRONG_THREAD</c>, not a race (WR-03, one level up).
    ///
    /// <para><c>ItemsPanelRoot</c> is the <c>ItemsWrapGrid</c> the control builds from
    /// <c>MediaBin.xaml</c>'s <c>ItemsPanelTemplate</c>, and its <c>Children</c> are the
    /// REALIZED containers. That count IS the number SC-2's "must be PROVEN" clause
    /// asks for — measured off the running control, not inferred from the fact that a
    /// virtualizing control was chosen.</para>
    ///
    /// <para><c>controlKind</c> is reported as the control's own type name so the D-16
    /// decision is confirmed AT RUNTIME rather than by reading XAML.</para>
    /// </summary>
    private void PublishIntrospection()
        => Rudis.Shell.Introspection.MediaBinIntrospection.Publish(
            controlKind: nameof(GridView),
            levelItemCount: (TileGrid.ItemsSource as System.Collections.ICollection)?.Count ?? 0,
            realizedContainers: TileGrid.ItemsPanelRoot?.Children.Count ?? 0,
            currentFolder: _currentFolder,
            draggingMediaId: _draggingMediaId);
#endif

    // ── navigation: pure view state, no backend call anywhere below ─────────

    /// <summary>
    /// A folder tile drills IN; a media tile records the primary selection.
    ///
    /// <para>Neither sends a command. v6.0's folder tile does exactly this
    /// (<c>main.ts:442-445</c>): assign the path, re-render. The other half of the media
    /// case — loading the selected item into the Source monitor — is plan 53-03's, and
    /// its far half (the Preview surface itself) is Phase 51's (D-13). It is recorded as
    /// deliberately half-built rather than faked here.</para>
    /// </summary>
    private void OnTileClick(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is not MediaBinTile tile)
        {
            return;
        }

        if (tile.Kind == MediaBinTileKind.Folder)
        {
            _currentFolder = tile.Id;
            if (_mirror is not null)
            {
                ApplyMirrorState(_mirror);
            }

            return;
        }

        _selectedMediaId = tile.Id;
    }

    /// <summary>
    /// Selection can also change by keyboard (arrow keys inside the grid, which is one
    /// of the four grounds for choosing this control), so the id is recorded here rather
    /// than only in <see cref="OnTileClick"/>.
    ///
    /// <para>A folder tile taking selection does not clear the remembered MEDIA
    /// selection — the two are different kinds of thing and the handoff's "one primary
    /// selection" is about media.</para>
    /// </summary>
    private void OnSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_rebuilding)
        {
            return;
        }

        if (TileGrid.SelectedItem is MediaBinTile { Kind: MediaBinTileKind.Media } tile)
        {
            _selectedMediaId = tile.Id;
        }
    }

    // ── D-13, half one: double-click → the REAL source-preview transport ─────

    /// <summary>
    /// v6.0's <c>dblclick</c> on a media tile (<c>main.ts:391-397</c>): load the clip
    /// into the Source monitor WITHOUT placing it on the timeline.
    ///
    /// <para>Resolved from the tapped element's own <c>DataContext</c> rather than from
    /// the grid's selection, because a double-tap that lands in the grid's padding has
    /// no tile under it and must do nothing at all — falling back to
    /// <c>SelectedItem</c> would re-load whatever happened to be selected, which is a
    /// gesture the user did not make.</para>
    /// </summary>
    private void OnTileDoubleTapped(object sender, DoubleTappedRoutedEventArgs e)
    {
        var tile = (e.OriginalSource as FrameworkElement)?.DataContext as MediaBinTile;
        if (tile is null || !MediaBinPreviewCommand.ShouldLoadPreview(tile))
        {
            return;
        }

        // Match v6: the double-click also takes the primary selection (main.ts:393).
        _selectedMediaId = tile.Id;

        // Sync handler starting an async Task that carries its own TOTAL try/catch.
        _ = LoadIntoSourcePreviewAsync(tile.Id);
    }

    /// <summary>
    /// The REAL <c>rudis_transport load_preview</c>, and the immediate-apply that
    /// follows it — frontend parity (<c>main.ts:575-591</c>), the same two lines
    /// <c>Toolbar.ImportPathsAsync</c> already runs.
    ///
    /// <para><c>NotePreviewMode</c> comes FIRST and the order is load-bearing:
    /// <c>ApplyPlaybackPayload</c> routes the payload into the ACTIVE monitor's slot, so
    /// noting the mode second would write source playback into the program slot and
    /// move the Timeline's playhead for a preview that was never placed.</para>
    /// </summary>
    private async Task LoadIntoSourcePreviewAsync(string mediaId)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        try
        {
            var playback = await engine.TransportAsync(MediaBinPreviewCommand.BuildArgs(mediaId));
            if (playback.Kind != RudisResultKind.Ok)
            {
                // UI-SPEC §5: a backend refusal is VISIBLE, never swallowed.
                App.LogDiagnostic(
                    $"MediaBin load_preview FAILED ({playback.Kind}/{playback.Status}): {playback.Error}");
                return;
            }

            App.Mirror?.NotePreviewMode("source");
            App.Mirror?.ApplyPlaybackPayload(JsonNode.Parse(playback.Value.GetRawText()));
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"MediaBin load_preview threw: {ex.GetType().Name}: {ex.Message}");
        }
    }

    // ── D-13, half two: the drag SOURCE (the drop target is now real) ────────

    /// <summary>
    /// The media id currently in flight, or <c>null</c>. Exposed for plan 53-04's
    /// introspection request — see <see cref="_draggingMediaId"/>.
    /// </summary>
    internal string? DraggingMediaId => _draggingMediaId;

    /// <summary>
    /// The drag TRIGGER, replacing the items control's built-in one.
    ///
    /// <para><b>Why manual.</b> Plan 53.1-01 measured that the built-in, gesture-driven
    /// path never raises its drag-start event in this unpackaged host — no ghost on a real
    /// mouse, no event under <c>SendInput</c>, three runs, and an owner UAT before that.
    /// The same plan measured that <c>UIElement.StartDragAsync</c>, called directly from a
    /// custom gesture, DOES render a real ghost and negotiate with the shell to completion
    /// in this very process: <c>RESULT: GHOST_APPEARED</c>, <c>SYNTHETIC:
    /// DRAGSTARTING_FIRED</c> ×3, <c>EXCEPTION: NONE</c>. It does not go through the
    /// built-in gesture recognizer at all, which is the step the fault is confined to.
    /// Full finding:
    /// <c>.planning/phases/53.1-mediabin-to-timeline-drag-and-drop-the-orphaned-seam/artifacts/53.1-01-diagnostic.md</c>.</para>
    ///
    /// <para>The tile is resolved from <c>e.OriginalSource</c>'s <c>DataContext</c> — the
    /// same shape <see cref="OnTileDoubleTapped"/> already uses — so a press landing in the
    /// grid's padding resolves to nothing and arms nothing.</para>
    /// </summary>
    private void OnTilePointerPressed(object sender, PointerRoutedEventArgs e)
    {
        // Tear down whatever the previous press left behind BEFORE resolving a new
        // source. Containers are recycled, so "the previous element" is routinely "this
        // element", and a surviving subscription means two OnDragStarting calls per drag.
        ClearDragState();

        if ((e.OriginalSource as FrameworkElement)?.DataContext is not MediaBinTile tile)
        {
            return;
        }

        // A FOLDER never drags — v6.0 attaches no dragstart to a folder tile at all
        // (main.ts:419-453), and a drag that can only ever be refused is worse than not
        // starting one.
        if (MediaBinDragPayload.MediaIdFor(tile) is null)
        {
            return;
        }

        var container = TileGrid.ContainerFromItem(tile) as FrameworkElement;
        var element = container ?? e.OriginalSource as FrameworkElement;
        if (element is null)
        {
            return;
        }

        _dragSourceElement = element;
        _pressTile = tile;
        _movesLoggedThisPress = 0;
        element.DragStarting += OnDragStarting;
        element.DropCompleted += OnDragCompleted;

        // Which element the DragStarting subscription actually landed on, and what its
        // DataContext is. This run's answer — `GridViewItem`, `DataContext = null` —
        // is why `_pressTile` exists; see that field's remarks.
        App.LogDiagnostic(
            $"mediabin drag: source element = {element.GetType().Name} " +
            $"(container {(container is null ? "NULL — fell back to OriginalSource" : "resolved")}), " +
            $"DataContext = {element.DataContext?.GetType().Name ?? "null"}");

        var p = e.GetCurrentPoint(element).Position;
        _dragGesture.Press(p.X, p.Y);

        // NOT Handled: ItemClick, selection and DoubleTapped must all keep working
        // (T-53.1-12 — a drag trigger that eats selection is a self-inflicted outage of
        // three shipped behaviours).
    }

    /// <summary>
    /// Once past the threshold, hand the gesture to the OS exactly once.
    /// </summary>
    private void OnTilePointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (_dragSourceElement is not { } element)
        {
            return;
        }

        var p = e.GetCurrentPoint(element);

        // Diagnostic only (plan 53.1-04): the first three moves of a press, with the
        // button state TryArm will be given — see _movesLoggedThisPress's remarks.
        if (_movesLoggedThisPress < 3)
        {
            _movesLoggedThisPress++;
            App.LogDiagnostic(
                $"mediabin drag: move #{_movesLoggedThisPress} at " +
                $"({p.Position.X:F1},{p.Position.Y:F1}) left={p.Properties.IsLeftButtonPressed}");
        }

        if (!_dragGesture.TryArm(p.Position.X, p.Position.Y, p.Properties.IsLeftButtonPressed))
        {
            return;
        }

        // Logged at the ARM point, not only at the drag's end. The first checkpoint run
        // produced NO_GHOST with total silence, and silence could not distinguish "armed
        // and the shell drew nothing" from "never armed at all" — two failures with
        // opposite fixes. This line makes the next run answer that question by itself.
        App.LogDiagnostic(
            $"mediabin drag: gesture ARMED at ({p.Position.X:F1},{p.Position.Y:F1}) — calling StartDragAsync");

        // Sync handler starting an async Task that carries its own TOTAL try/catch — the
        // async-void-free way to run work from an event (50-04's pattern, D-08; a grep
        // for `async void` over shell/ must stay at ZERO). The drag-initiation call
        // itself still runs SYNCHRONOUSLY inside this handler, before the first await,
        // which is what the OS drag loop requires.
        _ = BeginTileDragAsync(element, p);
    }

    /// <summary>
    /// The drag-initiation call and its total try/catch.
    ///
    /// <para>T-53.1-13: the one documented constraint on this API (an elevated process)
    /// is already eliminated by owner UAT (F6), so any failure here is unexpected — which
    /// is exactly why it is caught, logged with its full type name and HRESULT, and
    /// cleared rather than left to take the app down or to fail silently. Silence is what
    /// cost this project a whole phase; a logged throw is a finding, and the catch exists
    /// to make it one.</para>
    /// </summary>
    private async Task BeginTileDragAsync(UIElement element, Microsoft.UI.Input.PointerPoint pointerPoint)
    {
        try
        {
            // DragStarting fires from INSIDE this call and populates the DataPackage.
            var op = await element.StartDragAsync(pointerPoint);
            App.LogDiagnostic($"mediabin drag: drag session ended, negotiated operation {op}");
        }
        catch (Exception ex)
        {
            App.LogDiagnostic(
                $"mediabin drag: drag initiation THREW {ex.GetType().FullName}: {ex.Message} " +
                $"(HRESULT 0x{ex.HResult:X8})");
            ClearDragState();
        }
    }

    /// <summary>
    /// The payload, UNCHANGED in content from the v6.0-parity version
    /// (<c>main.ts:404-411</c>): the custom format id, a <c>text/plain</c>-equivalent
    /// fallback, and a Copy operation. Only the EVENT that reaches it changed — the
    /// element's own drag-starting event, raised by the manual initiation above, instead
    /// of the items control's, raised by a gesture recognizer that never ran (plan
    /// 53.1-01).
    ///
    /// <para>The dragged item comes from <c>sender</c>'s own <c>DataContext</c>, because
    /// this event's args carry no items collection (research Pitfall 1). That is the only
    /// line that differs.</para>
    ///
    /// <para>The drop side reads the id with the text channel, not the property bag
    /// (research Pitfall 2), which is why the fallback below is the load-bearing half of
    /// this payload rather than the decorative one.</para>
    /// </summary>
    private void OnDragStarting(UIElement sender, DragStartingEventArgs args)
    {
        // The press-time tile FIRST — the sender is the GridViewItem container, and with
        // x:Bind templates its DataContext is null (measured; see `_pressTile`). The
        // DataContext read stays only as a fallback for a sender that genuinely carries
        // one, and `Content` covers the container shape directly.
        var tile = _pressTile
            ?? (sender as FrameworkElement)?.DataContext as MediaBinTile
            ?? (sender as ContentControl)?.Content as MediaBinTile;
        var mediaId = tile is null ? null : MediaBinDragPayload.MediaIdFor(tile);

        // Proof this handler RAN. Its absence from the diagnostic tail is itself the
        // finding: it means the TaskCanceledException came from outside this handler.
        App.LogDiagnostic(
            $"mediabin drag: OnDragStarting FIRED — sender {sender.GetType().Name}, " +
            $"tile {(tile is null ? "null" : tile.Kind.ToString())}, mediaId {mediaId ?? "null"}");

        if (mediaId is null)
        {
            App.LogDiagnostic("mediabin drag: OnDragStarting CANCELLING — no media id on this sender");
            args.Cancel = true;
            return;
        }

        args.Data.SetText(mediaId);
        args.Data.Properties[MediaBinDragPayload.FormatId] = mediaId;
        args.Data.RequestedOperation = DataPackageOperation.Copy;

        _selectedMediaId = mediaId;
        _draggingMediaId = mediaId;
#if DEBUG
        // ⚠ WITHOUT THIS THE SIDE CHANNEL IS UNOBSERVABLE, and plan 53-06 found that out
        // the hard way. Plan 53-04 published `_draggingMediaId` for a future UIA-level
        // drag attempt, but every publish site it had — the mirror apply and the container
        // lifetime hook — is one that a DRAG does not trigger. So the field moved and the
        // published snapshot did not, and a drag test would have read a stale `null` and
        // reported it as "FlaUI cannot drive a drag" rather than "the instrument never
        // looked". The publish belongs where the value changes.
        PublishIntrospection();
#endif
    }

    /// <summary>v6.0's <c>dragend</c> (<c>main.ts:412-414</c>): clear the side channel
    /// however the drag ended, dropped or abandoned. This is the AUTHORITATIVE end of a
    /// drag — see <see cref="OnTilePointerReleased"/> for why the pointer teardown paths
    /// defer to it.</summary>
    private void OnDragCompleted(UIElement sender, DropCompletedEventArgs args)
    {
        App.LogDiagnostic($"mediabin drag: drop completed with {args.DropResult}");
        ClearDragState();
    }

    /// <summary>
    /// Pointer teardown: release, capture-lost and cancel all land here.
    ///
    /// <para>⚠ IT DEFERS TO A DRAG IN FLIGHT, and that guard is load-bearing rather than
    /// cautious. The OS drag loop TAKES pointer capture as the drag starts, so
    /// <c>PointerCaptureLost</c> fires during a perfectly healthy drag — not only when one
    /// is abandoned. Clearing unconditionally would null <see cref="_draggingMediaId"/> at
    /// the exact moment the SC-4 proof needs to read it, and would unsubscribe
    /// <see cref="OnDragCompleted"/> before it ever fired. A drag that has started owns its
    /// own teardown.</para>
    /// </summary>
    private void OnTilePointerReleased(object sender, PointerRoutedEventArgs e)
    {
        if (_draggingMediaId is not null)
        {
            return;
        }

        // Diagnostic only (plan 53.1-04): a teardown that lands while a press is still
        // armed-in-waiting kills the gesture SILENTLY (capture-lost fires for reasons a
        // release does not), and that silence is indistinguishable from moves never
        // arriving. Logged only when there is actually a press to kill.
        if (_dragSourceElement is not null)
        {
            App.LogDiagnostic(
                "mediabin drag: pointer teardown (release/capture-lost/cancel) before any " +
                "drag started — press state cleared");
        }

        ClearDragState();
    }

    /// <summary>
    /// Return to "no press, no source, nothing in flight". Idempotent by construction, so
    /// the release path, the drop-completed path and the next press can all call it.
    /// </summary>
    private void ClearDragState()
    {
        _dragGesture.Reset();
        _pressTile = null;

        if (_dragSourceElement is { } previous)
        {
            previous.DragStarting -= OnDragStarting;
            previous.DropCompleted -= OnDragCompleted;
            _dragSourceElement = null;
        }

        if (_draggingMediaId is not null)
        {
            _draggingMediaId = null;
#if DEBUG
            // Publish WHERE THE VALUE CHANGES, on the clearing edge as well as the
            // setting one — a side channel that only ever reports non-null is a side
            // channel that cannot show a drag ENDING.
            PublishIntrospection();
#endif
        }
    }

    /// <summary>
    /// Walk back UP the trail. The current crumb is disabled and cannot reach here at
    /// all (v6.0 disables it, <c>main.ts:493</c>), so this never re-renders the level it
    /// is already on.
    /// </summary>
    private void OnCrumbClick(object sender, RoutedEventArgs e)
    {
        if (sender is not FrameworkElement { Tag: string path })
        {
            return;
        }

        _currentFolder = path;
        if (_mirror is not null)
        {
            ApplyMirrorState(_mirror);
        }
    }

    // ── import: this region owns the AFFORDANCE, never a second import path ──

    private void OnImportMediaClick(object sender, RoutedEventArgs e)
        => ImportMediaRequested?.Invoke();

    private void OnImportFolderClick(object sender, RoutedEventArgs e)
        => _ = PickAndImportFolderAsync();

    /// <summary>
    /// D-15. <c>rudis_import_media_folder</c> has existed and been wired in
    /// <c>RudisNative</c> since Phase 50 and has NEVER had a caller — this is its
    /// first. Phase 50's own <c>MainWindow</c> drop handler names this phase as the
    /// owner in its comment ("a dropped FOLDER is deliberately ignored … whose
    /// recursion clamps belong to whichever phase surfaces folder import").
    ///
    /// <para><b>⚠ SECURITY, and it is the whole reason to delegate (T-53-10).</b> The
    /// recursion clamps (500 files, depth 12), the canonicalize-then-walk and the
    /// symlink skip all live in <c>crates/app-core/src/import.rs</c> and are applied by
    /// the FFI export at PRODUCTION values (<c>crates/ffi/src/commands.rs</c> passes the
    /// production constants into the shared UI wrapper). <b>The C# side walks NOTHING
    /// and clamps NOTHING.</b> A second, less careful traversal here would be a new
    /// attack surface with none of those guards, and it would look like a convenience —
    /// so <c>MediaBinImportTests</c> asserts MECHANICALLY that no directory enumeration
    /// exists anywhere under <c>shell/Rudis.Shell/</c>.</para>
    ///
    /// <para><b>MEASURED API DIVERGENCE, not a choice.</b> v6.0 opens its directory
    /// dialog with <c>multiple: true</c> (<c>main.ts:1484-1488</c>), and the wire
    /// contract takes an ARRAY of paths. WinAppSDK 1.8's
    /// <c>Microsoft.Windows.Storage.Pickers.FolderPicker</c> exposes exactly one pick
    /// method — <c>PickSingleFolderAsync</c> — read out of the shipped
    /// <c>Microsoft.Windows.Storage.Pickers.Projection.dll</c>'s own metadata, where the
    /// only <c>Pick*Async</c> members in the assembly are
    /// <c>PickSingleFileAsync</c> / <c>PickMultipleFilesAsync</c> /
    /// <c>PickSaveFileAsync</c> / <c>PickSingleFolderAsync</c>. There is no
    /// multi-folder API to call. The args are still built as an array so the one-folder
    /// limit lives in the PICKER and not in the request shape.</para>
    ///
    /// <para><b>v6.0's folder support is IMPORT-ONLY, deliberately.</b> The backend also
    /// exposes <c>CreateMediaFolder</c> / <c>DeleteMediaFolder</c> /
    /// <c>MoveMediaFolder</c> / <c>MoveMediaItem</c> / <c>RenameMediaItem</c>
    /// (<c>crates/core/src/command.rs:333-354</c>) and v6.0's UI calls <b>none</b> of
    /// them — folders arrive ONLY by mirroring an on-disk tree. Building folder CRUD
    /// here would be new capability, which <c>REQUIREMENTS.md</c> § Out of Scope ("New
    /// features during the port") forbids. A deliberate boundary, not an oversight;
    /// re-decidable at the Phase 55 gate.</para>
    /// </summary>
    private async Task PickAndImportFolderAsync()
    {
        if (HostWindow is null)
        {
            return;
        }

        string pickedPath;
        try
        {
            // WinAppSDK 1.8's picker takes a WindowId directly, so the
            // unpackaged-WinUI-3 owner-window requirement is satisfied with no COM
            // initialisation and no window-handle interop — the same route
            // Toolbar.PickAndImportAsync already uses. Do not invent a second one.
            var picker = new FolderPicker(HostWindow.AppWindow.Id)
            {
                CommitButtonText = "Import folder",
                SuggestedStartLocation = PickerLocationId.VideosLibrary,
            };

            var picked = await picker.PickSingleFolderAsync();
            if (picked is null)
            {
                return;   // cancelled: the engine is not touched at all
            }

            pickedPath = picked.Path;
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"folder picker failed: {ex.GetType().Name}: {ex.Message}");
            return;
        }

        await ImportFolderAsync(pickedPath);
    }

    /// <summary>
    /// The folder-import ROUTINE, with no picker in front of it — split out of
    /// <see cref="PickAndImportFolderAsync"/> by plan 53-06 so a second ENTRY POINT can
    /// exist without a second IMPLEMENTATION.
    ///
    /// <para>The second entry point is <c>App.StartupImportFolderPath</c> — a Debug-only,
    /// argv-gated launch flag that lets a UIA test get a real virtual-folder tree into the
    /// bin so a real click can be proven to drill into it. It is exactly the shape
    /// <c>--import</c> itself already has, and its doc comment already frames: "a THIRD
    /// entry point, not a third implementation". Everything below this line — the args
    /// array, the wrapper call, the refusal log, the count log, and the deliberate absence
    /// of a manual refresh — is shared by both routes, so the flag cannot drift away from
    /// what the button does.</para>
    ///
    /// <para>See <see cref="PickAndImportFolderAsync"/>'s remarks for the whole security
    /// argument (T-53-10): the C# side walks nothing and clamps nothing.</para>
    /// </summary>
    internal async Task ImportFolderAsync(string folderPath)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        try
        {
            var args = new JsonObject { ["paths"] = new JsonArray(folderPath) };
            var imported = await engine.ImportMediaFolderAsync(args.ToJsonString());
            if (imported.Kind != RudisResultKind.Ok)
            {
                // UI-SPEC §5: a backend refusal is VISIBLE, never swallowed.
                App.LogDiagnostic(
                    $"import_media_folder FAILED ({imported.Kind}/{imported.Status}): {imported.Error}");
                return;
            }

            var count = imported.Value.ValueKind == JsonValueKind.Array
                ? imported.Value.GetArrayLength()
                : 0;
            App.LogDiagnostic(
                $"import_media_folder: {count} item(s) from '{folderPath}' " +
                "(non-media entries are skipped by the backend walk)");

            // No manual refresh: the import pushes project:changed, MainWindow's cold
            // poll applies it, and ApplyMirrorState rebuilds this level. Reaching around
            // the mirror to insert tiles here would break rule 4.
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"import_media_folder threw: {ex.GetType().Name}: {ex.Message}");
        }
    }

    // ── container lifetime ───────────────────────────────────────────────────

    /// <summary>
    /// The platform's own phased-loading hook — ground 2 of the D-16 control decision,
    /// and the whole reason a poster can be loaded here safely at all.
    ///
    /// <para><b>Phase 0</b> runs for EVERY realized container before the frame is shown:
    /// reset the thumb to its placeholder, stamp the container with the item's id (the
    /// staleness token, written on the UI thread), and DEFER the decode to the next
    /// phase, so scrolling never waits on file I/O. <b>Phase 1</b> starts the decode.
    /// <b>The recycle-queue flag</b> is the third case and it is the one that
    /// matters most: a container going back to the pool must
    /// drop its image, or the next item to reuse it shows the PREVIOUS item's picture
    /// under the new item's name until the new decode lands
    /// (53-RESEARCH.md, Common Pitfalls #2).</para>
    ///
    /// <para><b>⚠ <c>args.Handled</c> is deliberately NEVER set, and the reason is
    /// load-bearing.</b> Microsoft's phased-loading sample sets it because that sample
    /// builds the entire container content imperatively from an EMPTY template. This
    /// region does not: every string on a tile comes from <c>{x:Bind}</c> in
    /// <c>MediaBin.xaml</c>'s <c>ItemTemplate</c> — including
    /// <c>AutomationProperties.AutomationId</c>, which plan 53-02's live UIA proof reads
    /// back out of the running app. Compiled bindings in a <c>DataTemplate</c> are
    /// evaluated by the control's DEFAULT processing of this event, and
    /// <c>Handled = true</c> is precisely the instruction to skip that. Setting it would
    /// blank every tile. This handler ADDS work; it never claims the event.</para>
    /// </summary>
    private void OnContainerContentChanging(ListViewBase sender, ContainerContentChangingEventArgs args)
    {
        var container = args.ItemContainer;
        if (container is null)
        {
            return;
        }

        if (args.InRecycleQueue)
        {
            ResetThumb(container);
#if DEBUG
            PublishIntrospection();
#endif
            return;
        }

        if (args.Phase == 0)
        {
            ResetThumb(container);
#if DEBUG
            PublishIntrospection();
#endif

            // The staleness token. Written HERE — on the UI thread, at this container's
            // own phase 0 — so that a decode which started for a previous item can tell,
            // after its await, that the container is no longer its own.
            var phaseZeroTile = args.Item as MediaBinTile;
            container.Tag = phaseZeroTile?.Id;

            // Plan 60.1-07. The offline treatment is applied HERE, imperatively, for
            // exactly the reason the thumbnail is: this template's conditional visuals
            // are driven from the container lifecycle rather than from bindings, because
            // a RECYCLED container has to be able to drop state that is no longer its
            // own. A second binding idiom in the same template would be the change that
            // makes the next reader guess which one owns a given pixel.
            if (phaseZeroTile is not null)
            {
                ApplyOfflineState(container, phaseZeroTile);

                // Plan 63-04, immediately beside its twin and for the identical reason:
                // this template's conditional visuals are driven from the container
                // lifecycle, not from bindings, because a RECYCLED container has to be
                // able to drop state that is no longer its own.
                ApplyProxyState(container, phaseZeroTile);
            }

            args.RegisterUpdateCallback(OnContainerContentChanging);
            return;
        }

        // `>= 1` rather than `== 1`: the phase counter belongs to the control, and this
        // handler must not depend on owning a particular number. LoadPosterAsync is
        // idempotent (it returns early once a source is set), so an extra phase costs
        // nothing and a shifted one still loads.
        if (args.Item is MediaBinTile tile)
        {
            // A sync handler starting an async Task that carries its own TOTAL
            // try/catch — never a fire-and-forget void handler, which would put a decode
            // fault on the dispatcher with no handler and take the window down over a
            // thumbnail (the 50-04 pattern; the gate over shell/ stays at zero).
            _ = LoadPosterAsync(container, tile);
        }
    }

    /// <summary>
    /// Decode one poster at TILE SIZE, off the UI thread, and only assign it if the
    /// container still belongs to the item it was started for.
    ///
    /// <para><b>⚠ D-06 — there is NO asset-protocol allowlist here, and adding one would
    /// be cargo cult.</b> v6.0 could load these PNGs only because
    /// <c>tauri.conf.json</c>'s asset-protocol scope allowlists
    /// <c>$APPCACHE/posters/**</c> — a WebView SANDBOX constraint, recorded in
    /// <c>poster_cache_dir</c>'s own doc comment
    /// (<c>crates/app-core/src/import.rs:363-370</c>). WinUI 3 has no such sandbox; the
    /// shell opens the stored path. Nothing about that allowlist carries over.</para>
    ///
    /// <para><b>⚠ No thread-pool hop, on purpose.</b> <c>BitmapImage</c> must be
    /// CONSTRUCTED on the UI thread (microsoft-ui-xaml#2289). What actually moves off it
    /// is the file open and the decode inside <c>SetSourceAsync</c>, both already async
    /// by design — wrapping any of this in a thread-pool hop would buy nothing and would
    /// construct the bitmap on the wrong thread.</para>
    /// </summary>
    private async Task LoadPosterAsync(SelectorItem container, MediaBinTile tile)
    {
        // D-08's NORMAL path, and it is silent: audio never has a poster, a video's
        // poster generation is allowed to fail, and a stored path may no longer resolve.
        // All three keep the placeholder glyph the template already drew.
        if (!MediaBinPoster.ShouldAttemptLoad(tile.PosterPath))
        {
            return;
        }

        // Resolve the template parts BEFORE awaiting anything — they are UI objects and
        // this is the last guaranteed moment on the UI thread.
        if (container.ContentTemplateRoot is not FrameworkElement root
            || root.FindName("Thumb") is not Image image
            || root.FindName("Glyph") is not TextBlock glyph)
        {
            return;
        }

        if (image.Source is not null)
        {
            return;
        }

        // D-07 / T-53-11: bounded to what the tile renders, in physical pixels. A
        // DecodePixelWidth of 0 means FULL RESOLUTION — see MediaBinPoster.
        var px = MediaBinPoster.DecodePixelWidth(
            MediaBinPoster.ThumbWidthDip, XamlRoot?.RasterizationScale ?? 1.0);

        try
        {
            var bitmap = new BitmapImage { DecodePixelWidth = px };
            var file = await StorageFile.GetFileFromPathAsync(tile.PosterPath!);
            using var stream = await file.OpenReadAsync();
            await bitmap.SetSourceAsync(stream);

            // ═══ THE STALENESS GUARD, AND IT IS NOT OPTIONAL ═══
            // The container may have been recycled to a DIFFERENT item while this
            // awaited. `Tag` was rewritten on the UI thread at that container's phase 0,
            // so a mismatch means this decode is stale and assigning it would put the
            // wrong poster on the wrong tile — a scroll-speed-dependent bug that is
            // essentially impossible to catch by hand (53-RESEARCH Pitfall 2).
            if (!string.Equals(container.Tag as string, tile.Id, StringComparison.Ordinal))
            {
                return;
            }

            image.Source = bitmap;
            glyph.Visibility = Visibility.Collapsed;
        }
        catch (Exception ex)
        {
            // A poster is DECORATION. A thrown decode must never take a region down, and
            // the honest fallback is the placeholder that is already on screen.
            App.LogDiagnostic(
                $"MediaBin poster load failed for '{tile.Id}': {ex.GetType().Name}: {ex.Message}");
        }
    }

    /// <summary>
    /// Draw (or undraw) the handoff's offline/missing-file state on one realized
    /// container: <i>"tile dimmed + ⚠ 'relink'"</i> (README:126).
    ///
    /// <para>The DIM is applied to <c>ThumbArea</c> — the poster, its placeholder glyph
    /// and its duration badge together — while the mark itself is a SIBLING of that
    /// group, so the one element that explains the fade is the one element not faded by
    /// it. A dim rather than a blank because a beginner has to see WHICH clip is broken,
    /// and the poster survives the source moving: it is a cached PNG in app data.</para>
    ///
    /// <para><b>ANNOUNCED, not only drawn.</b> The mark carries an
    /// <c>AutomationProperties.Name</c> — the plain-English sentence, not the badge's
    /// two-word shorthand — so a screen-reader user learns the file is missing and what
    /// to do about it, and a UIA caller (plan 60.1-09's gate) can address the mark by its
    /// own id rather than inferring the state from a screenshot. The same sentence is the
    /// tooltip, because "relink" is jargon to the audience this app is for and 140px of
    /// tile has no room for the sentence that is not.</para>
    ///
    /// <para>Every string comes from <see cref="MediaBinOffline"/>, in the WinUI-free
    /// directory where it is asserted, rather than being typed into the XAML where no
    /// test can read it.</para>
    /// </summary>
    private static void ApplyOfflineState(SelectorItem container, MediaBinTile tile)
    {
        if (container.ContentTemplateRoot is not FrameworkElement root)
        {
            return;
        }

        if (root.FindName("ThumbArea") is FrameworkElement area)
        {
            area.Opacity = tile.IsOffline ? MediaBinOffline.DimOpacity : 1.0;
        }

        if (root.FindName("OfflineBadge") is not Border badge)
        {
            return;
        }

        badge.Visibility = tile.IsOffline ? Visibility.Visible : Visibility.Collapsed;

        if (!tile.IsOffline)
        {
            return;
        }

        if (root.FindName("OfflineBadgeText") is TextBlock label)
        {
            label.Text = MediaBinOffline.BadgeText;
        }

        AutomationProperties.SetName(badge, MediaBinOffline.BadgeHelpText);
        AutomationProperties.SetAutomationId(badge, MediaBinOffline.BadgeAutomationId(tile.Id));
        ToolTipService.SetToolTip(badge, MediaBinOffline.BadgeHelpText);
    }

    /// <summary>
    /// Draw (or undraw) "this clip is being got ready" on one realized container — plan
    /// 63-04, TRUST-03.
    ///
    /// <para>A corner mark, NOT a headline, and that placement is the design decision.
    /// <see cref="ApplyOfflineState"/>'s ⚠ is centred because it is the only warning a
    /// beginner gets that a clip will not play at all; this one says the app is busy on
    /// their behalf and that nothing is wrong. It takes the one corner the duration badge
    /// and the ⚠ do not, and it is lighter than both.</para>
    ///
    /// <para><b>ANNOUNCED, not only drawn.</b> The mark carries an
    /// <c>AutomationProperties.Name</c> — a whole sentence, different per state — so a
    /// screen-reader user learns what is happening and that they can keep working, and a
    /// UIA caller can read the VALUE and watch it MOVE. That last part is 63-CONTEXT
    /// D-11 in one line: "a control exists" would have been true of a hard-coded label.</para>
    ///
    /// <para>Every string comes from <see cref="MediaBinProxy"/>, in the WinUI-free
    /// directory where it is asserted.</para>
    /// </summary>
    private void ApplyProxyState(SelectorItem container, MediaBinTile tile)
    {
        if (container.ContentTemplateRoot is not FrameworkElement root)
        {
            return;
        }

        if (root.FindName("ProxyBadge") is not Border badge)
        {
            return;
        }

        var bar = root.FindName("ProxyProgress") as ProgressBar;

        // Plan 71-03. The tile's own percent, or the live map's if that has moved on since
        // the tile was built (see RefreshProxyProgressInPlace) -- never the lower of the
        // two, so a recycled container cannot show an older number (T-71-14).
        var percent = tile.ProxyProgressPercent;
        if (string.Equals(tile.ProxyState, MediaBinProxy.Running, StringComparison.Ordinal)
            && _proxyProgress.TryGetValue(tile.Id, out var live))
        {
            percent = MediaBinProxy.MonotonicPercent(percent, live);
        }

        var text = MediaBinProxy.BadgeTextFor(tile.ProxyState, percent);

        if (text.Length == 0)
        {
            badge.Visibility = Visibility.Collapsed;
            if (bar is not null)
            {
                bar.Visibility = Visibility.Collapsed;
            }

            return;
        }

        if (root.FindName("ProxyBadgeText") is TextBlock label)
        {
            label.Text = text;
        }

        var announcement = MediaBinProxy.AnnouncementFor(tile.ProxyState, percent);
        AutomationProperties.SetName(badge, announcement);
        AutomationProperties.SetAutomationId(badge, MediaBinProxy.BadgeAutomationId(tile.Id));
        ToolTipService.SetToolTip(badge, announcement);
        badge.Visibility = Visibility.Visible;

        if (bar is null)
        {
            return;
        }

        if (string.Equals(tile.ProxyState, MediaBinProxy.Running, StringComparison.Ordinal) && percent >= 0)
        {
            // A DETERMINATE bar: UIA exposes it through IRangeValueProvider, so the number
            // is read as a number, not scraped from a sentence (71-RESEARCH Pattern 4).
            bar.Value = percent;
            AutomationProperties.SetAutomationId(bar, MediaBinProxy.ProgressAutomationId(tile.Id));
            AutomationProperties.SetName(bar, announcement);
            bar.Visibility = Visibility.Visible;
        }
        else
        {
            bar.Visibility = Visibility.Collapsed;
        }
    }

    /// <summary>
    /// Plan 71-03. Re-apply the proxy badge and bar on every REALIZED container, from the
    /// tile it is showing plus the live <see cref="_proxyProgress"/> map, without touching
    /// <c>ItemsSource</c>. Only called when a percent moved and no display state did.
    /// </summary>
    private void RefreshProxyProgressInPlace()
    {
        if (TileGrid.ItemsPanelRoot is not Panel panel)
        {
            return;
        }

        foreach (var child in panel.Children)
        {
            if (child is SelectorItem container && container.Content is MediaBinTile tile)
            {
                ApplyProxyState(container, tile);
            }
        }
    }

    /// <summary>
    /// Put a container back to "placeholder": no image, glyph visible, no staleness
    /// token, NOT offline. Called at phase 0 AND on the way into the recycle queue.
    ///
    /// <para>The thumbnail source is set IMPERATIVELY rather than by a binding precisely
    /// so a container whose item has changed can drop work that is no longer its own —
    /// the consequence being that nothing re-evaluates it on recycle either, which is
    /// what makes this method necessary rather than tidy.</para>
    /// </summary>
    private static void ResetThumb(SelectorItem container)
    {
        container.Tag = null;

        if (container.ContentTemplateRoot is not FrameworkElement root)
        {
            return;
        }

        if (root.FindName("Thumb") is Image thumb)
        {
            thumb.Source = null;
        }

        if (root.FindName("Glyph") is TextBlock glyph)
        {
            glyph.Visibility = Visibility.Visible;
        }

        // Plan 60.1-07. Back to ONLINE, unconditionally. A container heading into the
        // recycle queue carries no item, so this cannot ask a tile — it restores the
        // undimmed default and lets phase 0 re-decide. Skipping it would leave a stale
        // dim and a stale ⚠ on whichever HEALTHY item this container serves next, which
        // is the recycling bug this whole method exists to prevent, wearing a new hat.
        if (root.FindName("ThumbArea") is FrameworkElement area)
        {
            area.Opacity = 1.0;
        }

        if (root.FindName("OfflineBadge") is FrameworkElement badge)
        {
            badge.Visibility = Visibility.Collapsed;
        }

        // Plan 63-04, same rule, same hat: a container heading into the recycle queue
        // carries no item, so this cannot ask a tile — it restores the default and lets
        // phase 0 re-decide. Skipping it would leave a stale "Preparing…" on whichever
        // SETTLED clip this container serves next.
        if (root.FindName("ProxyBadge") is FrameworkElement proxyBadge)
        {
            proxyBadge.Visibility = Visibility.Collapsed;
        }

        // Plan 71-03, the same hat again: a stale bar would claim a percentage for a clip
        // whose preparation finished long ago.
        if (root.FindName("ProxyProgress") is FrameworkElement proxyProgress)
        {
            proxyProgress.Visibility = Visibility.Collapsed;
        }
    }
}
