using System.Collections.Concurrent;
using System.Diagnostics;
using System.Text.Json;
using Microsoft.UI.Xaml;
using Rudis.Shell.Interop;
using Rudis.Shell.Mirror;

namespace Rudis.Shell;

/// <summary>
/// The production shell application (Phase 50, plan 50-03). Hosting shape promoted
/// from the proven spike (spikes/44-hwaccel/csharp-host — technique reference only;
/// spike code never ships). Startup creates the ONE engine instance for the app's
/// lifetime; MainWindow renders the ABI probe through it (R4's launch proof) and
/// disposes it at close (parity close contract, 50-02 §2.3).
/// </summary>
public partial class App : Application
{
    /// <summary>Most recent diagnostic lines, newest last. Bounded so a chatty
    /// mirror cannot grow memory; the last line is surfaced in the window because
    /// UI-SPEC §5 requires backend errors to be VISIBLE rather than swallowed.</summary>
    private const int DiagnosticCapacity = 200;

    private static readonly ConcurrentQueue<string> Diagnostics = new();

    private Window? _window;

    /// <summary>The app's single engine instance. May be IsInvalid — a DESIGNED
    /// outcome for malformed config (lib.rs:191-201); consumers null-check via
    /// <see cref="RudisNative.IsInvalid"/>, never assume.</summary>
    internal static RudisNative? Engine { get; private set; }

    /// <summary>
    /// The app's single state mirror (SHELL-02) — created here so every region from
    /// plan 50-05 onward binds to the SAME one. Null when the engine failed to
    /// initialise; there is deliberately no mirror over a dead ctx.
    /// </summary>
    internal static ShellMirror? Mirror { get; private set; }

#if DEBUG
    /// <summary>D-10's secondary UAT assertion channel. Null in every launch except a
    /// Debug build with <c>RUDIS_SHELL_INTROSPECT=1</c> set — see
    /// <see cref="Rudis.Shell.Introspection.IntrospectionHook.StartIfRequested"/>. Held here
    /// (rather than discarded) so the running server loop stays unambiguously rooted
    /// for the app's lifetime and nothing has to guess whether it is still alive.</summary>
    internal static Rudis.Shell.Introspection.IntrospectionHook? IntrospectionHook { get; private set; }
#endif

    public App()
    {
        InitializeComponent();
    }

    /// <summary>The newest diagnostic line, or null when nothing has been logged.</summary>
    internal static string? LastDiagnostic { get; private set; }

    /// <summary>
    /// Ask the mirror to poll NOW rather than at the next cold tick. Set once by
    /// <c>MainWindow</c>; null before the window exists, so every caller null-conditions.
    ///
    /// <para><b>Why this exists (plan 52-09, criterion 2).</b> A dispatched edit's
    /// result exists on the backend the instant <c>rudis_dispatch_command</c> returns
    /// <c>Ok</c>. Without this, the shell does not go LOOKING for it until the next
    /// 100ms cold tick (Phase 50 D-06) — a wait uniformly distributed over 0..100ms
    /// that buys nothing at all, because the caller already knows a patch is waiting.
    /// Measured: it was the dominant term in every one of criterion 2's committed
    /// edits, and it is a scheduling constant rather than a cost of doing the work.</para>
    ///
    /// <para><b>It changes no ownership and adds no pipeline.</b> The backend still
    /// owns the state, the mirror is still the only reader, the patch ladder is
    /// unchanged and the poll's own in-flight gate still admits exactly one cycle at a
    /// time (T-50-18). All this does is choose WHEN to look.</para>
    /// </summary>
    internal static Action? RequestMirrorPollNow { get; set; }

#if DEBUG
    /// <summary>
    /// The newest <paramref name="max"/> diagnostic lines, oldest first — a SNAPSHOT of
    /// the bounded tail <see cref="LogDiagnostic"/> already keeps.
    ///
    /// <para>Its one caller is the Debug-only introspection hook (plan 52-07): a UAT run
    /// needs to read back the exact <c>Command</c> JSON an edit dispatched, and the
    /// alternative — a bespoke edit log — would have added a second, narrower channel
    /// beside the general one that already records it. Read-only, allocates a fresh list
    /// per call (a UAT query, not a hot path), and wrapped in <c>#if DEBUG</c> so the
    /// Release surface is unchanged.</para>
    /// </summary>
    internal static IReadOnlyList<string> DiagnosticTail(int max)
    {
        var all = Diagnostics.ToArray();
        var skip = all.Length > max ? all.Length - max : 0;
        var tail = new List<string>(all.Length - skip);
        for (var i = skip; i < all.Length; i++)
        {
            tail.Add(all[i]);
        }

        return tail;
    }
#endif

    /// <summary>
    /// Media paths given on the command line as <c>--import &lt;path&gt; [&lt;path&gt; ..]</c>.
    ///
    /// <para>This is real open-with-file behaviour (a shell association hands the app
    /// its file on argv), and it is ALSO plan 50-08's route for driving an import from
    /// a UIA test without a file-picker dialog to defeat. It is a THIRD entry point,
    /// not a third implementation: <c>MainWindow</c> feeds these paths to
    /// <c>Toolbar.ImportPathsAsync</c> — the one routine that calls
    /// <c>rudis_import_media</c> — exactly as the button and the window-wide drop do.
    /// The CLI route additionally asks for <c>load_preview</c> on the first item so
    /// "open this file" leaves something loaded to play.</para>
    ///
    /// <para>Only paths that EXIST are kept (T-50-20's boundary is the same for argv as
    /// for a drop); the backend's probe is still the authoritative gate.</para>
    /// </summary>
    internal static IReadOnlyList<string> StartupImportPaths { get; private set; } = [];

#if DEBUG
    /// <summary>Phase 69 (D-69-03). Debug-only env seams the UIA harness uses to keep a
    /// test off the owner's real credentials and real project store. Values are read ONLY
    /// inside BuildInitConfigJson's #if DEBUG block; ReleaseHookAbsenceTests asserts the
    /// literals are absent from a Release build. <c>internal</c> (not private) only so the
    /// Debug-only plaintext-key fault in SettingsDialog can gate on the SAME seams without a
    /// second copy of the literals (review 69 WR-03).</summary>
    internal const string TestCredentialServiceEnvName = "RUDIS_TEST_CREDENTIAL_SERVICE";
    internal const string TestDataRootEnvName = "RUDIS_TEST_DATA_ROOT";

    /// <summary>Phase 69 fault injection (D-69-20 / D-69-22). <c>require-key-at-startup</c> makes
    /// OnLaunched refuse to open the editor without a key — the fault the SC-4 arm must go
    /// red under. (<c>plaintext-key</c> is read by SettingsDialog.) Debug-only.</summary>
    private const string Fault69EnvName = "RUDIS_FAULT_69";
    private const string Fault69RequireKeyAtStartup = "require-key-at-startup";

    /// <summary>
    /// <c>--place-on-timeline</c>: after a <c>--import</c>, place each imported item on
    /// the timeline end to end, so a developer or a UAT run has real clips to look at.
    ///
    /// <para><b>Why this exists at all, stated rather than assumed.</b> As of plan
    /// 52-06 the shell has NO user gesture that puts a clip on the timeline: dragging
    /// from the <c>MediaBin</c> is the handoff's own route and Phase 53 owns that
    /// region (D-09), and the `Timeline › Toolbar` tools are wired by plan 52-07. So
    /// "the Timeline draws REAL imported media" — this plan's whole claim, and CLAUDE.md
    /// rule 3's standard for making it — was not reachable by any route in the app.
    /// This is the same shape as <c>--import</c> itself (plan 50-05 added that so a UIA
    /// test could import without a file dialog to defeat), and it goes through the SAME
    /// backend command a real drop will: <c>rudis_place_clip</c>, unchanged.</para>
    ///
    /// <para><b>Debug-only, and argv-gated even in Debug</b>, following the
    /// <c>--timeline-smoke</c> precedent (plan 52-01): auto-placing on import is a
    /// developer affordance, not product behaviour, and open-with-file must keep
    /// meaning "import this", not "import this and edit my timeline". A Release build
    /// carries neither the flag nor the code path.</para>
    /// </summary>
    internal static bool StartupPlaceOnTimeline { get; private set; }

    /// <summary>
    /// Plan 53-04. The argv flag behind <see cref="StartupSyntheticMediaBin"/>. The
    /// literal lives HERE and nowhere else in the shipped shell — one definition, no
    /// scattered copies, and one place for the mechanical Release-absence proof to
    /// point at (the <see cref="RemoveTrackDialogFlagName"/> discipline, followed rather
    /// than re-invented).
    /// </summary>
    private const string SyntheticMediaBinFlagName = "--mediabin-synthetic";

    /// <summary>
    /// Replace the <c>MediaBin</c>'s items with N synthetic tiles for SC-2's
    /// realized-container measurement — D-03's clause that "virtualization must be
    /// PROVEN, not assumed". ZERO is the default and means the region behaves exactly as
    /// it always does; <see cref="SyntheticMediaBinFlagName"/> is the flag that sets it.
    ///
    /// <para><b>Debug-only, and argv-gated even in Debug</b>, following the
    /// <see cref="StartupPlaceOnTimeline"/> / <c>--timeline-smoke</c> precedent. A
    /// Release build carries neither the flag, the parsing, nor the code path, and that
    /// is asserted against the REAL built DLL — both by type absence and by a byte
    /// search for the flag literal itself (<c>ReleaseHookAbsenceTests</c>).</para>
    ///
    /// <para><b>It needs no isolated store, and the reason is a stronger guarantee than
    /// one.</b> <see cref="StartupSynthesizeClipCount"/> points its launch at a throwaway
    /// temp root because it dispatches a thousand REAL <c>rudis_place_clip</c> commands.
    /// This flag dispatches nothing at all: the tiles are handed to the control's
    /// <c>ItemsSource</c> and never travel back, so there is no write to redirect away
    /// from <c>%APPDATA%\app.rudis.desktop</c>.</para>
    /// </summary>
    internal static int StartupSyntheticMediaBin { get; private set; }

    /// <summary>The harness ceiling, taken FROM the builder rather than restated beside
    /// it — two ceilings that could drift apart would be a clamp that does not clamp.</summary>
    private const int MaxSyntheticMediaBinTiles =
        Rudis.Shell.Introspection.MediaBinSyntheticBin.MaxTiles;

    /// <summary>Parse the synthetic-bin flag's count. A missing, malformed or
    /// non-positive N is ZERO (the flag does nothing) and is LOGGED — a typo that
    /// silently measured a different bin size than the artifact claims would be worse
    /// than a no-op.</summary>
    private static int ParseSyntheticMediaBinCount()
    {
        var args = Environment.GetCommandLineArgs();
        for (var i = 1; i < args.Length - 1; i++)
        {
            if (!string.Equals(args[i], SyntheticMediaBinFlagName, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            if (!int.TryParse(args[i + 1], out var count) || count <= 0)
            {
                LogDiagnostic(
                    $"{SyntheticMediaBinFlagName}: '{args[i + 1]}' is not a positive tile count; ignored");
                return 0;
            }

            if (count > MaxSyntheticMediaBinTiles)
            {
                LogDiagnostic(
                    $"{SyntheticMediaBinFlagName}: {count} exceeds the " +
                    $"{MaxSyntheticMediaBinTiles} harness ceiling; clamped");
                return MaxSyntheticMediaBinTiles;
            }

            return count;
        }

        return 0;
    }

    /// <summary>
    /// Plan 54-05. The argv flag behind <see cref="SynthChatCards"/>. The literal lives
    /// HERE and nowhere else in the shipped shell — the <see cref="RemoveTrackDialogFlagName"/>
    /// / <see cref="SyntheticMediaBinFlagName"/> discipline, followed rather than
    /// re-invented, and that count is itself one of the assertions.
    /// </summary>
    private const string SynthChatCardsFlagName = "--synth-chat-cards";

    /// <summary>
    /// Render two SYNTHETIC option cards into the <c>Chat</c> transcript at launch, so a
    /// UIA test can drive the REAL apply-card pipe with a REAL mouse click.
    ///
    /// <para><b>Why it exists, and why the alternative was unacceptable.</b> An option
    /// card only appears when the agent proposes options — which needs a live, PAID model
    /// turn. A UIA suite that spent money every run is not a suite anyone can run, and
    /// a suite that invoked the click handler directly instead would be exactly the
    /// handler-level substitute plan 53.1-04 banned after MEDIABIN-53 showed what that
    /// class of shortcut hides. This flag renders the cards through the region's own
    /// <c>RenderCards</c> — no forked pipeline — so the click that follows exercises OS
    /// input → button → payload builder → ABI → backend → error bubble, whole.</para>
    ///
    /// <para><b>Spend-safe BY CONSTRUCTION, not by convention (T-54-13).</b> The
    /// synthetic ids were never offered by any turn, so the backend has no
    /// <c>pending_option_choice</c> for them and <c>rudis_apply_option_card</c> can only
    /// REFUSE. There is no code path from this flag to a billable call.</para>
    ///
    /// <para><b>Debug-only, and argv-gated even in Debug</b>, following the
    /// <see cref="StartupPlaceOnTimeline"/> / <see cref="StartupSyntheticMediaBin"/>
    /// precedent for the same reason (T-54-06): fabricated cards in a shipped build would
    /// be the fake-functional surface CLAUDE.md rule 1 forbids. A Release build carries
    /// neither the flag literal, the parsing, the member, nor the call site — asserted
    /// mechanically against the REAL built DLL in both directions plus a source-level
    /// <c>#if DEBUG</c> region scan (<c>ReleaseHookAbsenceTests</c>).</para>
    ///
    /// <para>It needs no isolated store: it dispatches nothing that mutates a project.
    /// The only ABI call it can lead to is an apply the backend refuses.</para>
    /// </summary>
    internal static bool SynthChatCards { get; private set; }

    /// <summary>
    /// The argv flag behind <see cref="ShowRemoveTrackDialogOnLaunch"/>. The literal
    /// lives HERE and nowhere else in the shipped shell — one definition, no scattered
    /// copies, and one place for the mechanical Release-absence proof to point at.
    /// </summary>
    private const string RemoveTrackDialogFlagName = "--show-remove-track-dialog";

    /// <summary>
    /// Opens the REAL remove-track confirmation against track 0 once the window is
    /// ready, so plan 51-06's layering proof can photograph the SECOND of SC-1's three
    /// cases before the Timeline has a track-gutter button to trigger it (Phase 52).
    ///
    /// <para><b>Why argv and not the introspection pipe.</b>
    /// <c>IntrospectionHook</c> is an ASSERTION channel, not a control channel
    /// (T-50-35) — no request there may cause anything to HAPPEN. A command-line flag
    /// is the precedent this codebase already set for driving a real code path from a
    /// test (<see cref="StartupImportPaths"/>, <see cref="StartupPlaceOnTimeline"/>),
    /// and it is compiled OUT of Release entirely — not disabled, ABSENT — which
    /// <c>ReleaseHookAbsenceTests</c> asserts against a real Release build AND against
    /// this file's own <c>#if DEBUG</c> region (T-51-21).</para>
    ///
    /// <para><b>Phase 52 DELETES this flag</b> and replaces it with the Timeline's
    /// track-gutter button. The DIALOG and its dispatch
    /// (<c>MainWindow.RemoveTrackAsync</c>) are production code and stay.</para>
    ///
    /// <para>Pair it with <see cref="StartupPlaceOnTimeline"/>: an EMPTY lane is removed
    /// with no prompt (v6 parity), so the launch hook waits for a lane that actually
    /// holds clips and otherwise does nothing at all.</para>
    /// </summary>
    internal static bool ShowRemoveTrackDialogOnLaunch { get; private set; }

    /// <summary>
    /// <c>--detach-audio</c>: after <c>--place-on-timeline</c>, dispatch the REAL
    /// <c>DetachAudio</c> command for the first placed video clip that has audio.
    ///
    /// <para><b>Why it exists.</b> 52-CONTEXT D-20's rule is that a waveform fill
    /// follows <c>has_audio &amp;&amp; !audio_detached</c> — so a video clip whose
    /// audio has been DETACHED must show no fill, while the audio-track clip the
    /// detach created must show one. That is a claim about pixels, and CLAUDE.md rule
    /// 3 says to prove it on output. As of plan 52-08 no gesture in the shell can
    /// detach audio: the Timeline's tool tiles are split/duplicate/delete (52-07), the
    /// <c>Inspector</c> that would host an audio panel is Phase 53's, and nothing else
    /// dispatches <c>DetachAudio</c>. So the rule was unverifiable by any route in the
    /// app.</para>
    ///
    /// <para>Exactly the <see cref="StartupPlaceOnTimeline"/> shape, for exactly the
    /// same reason and with the same limits: Debug-only, argv-gated even in Debug, and
    /// it goes through the SAME <c>rudis_dispatch_command</c> a real audio panel will,
    /// with no shortcut around the domain's own rules (which refuse a non-video track,
    /// an already-detached clip, and media with no audio). A refusal is LOGGED, never
    /// swallowed. A Release build carries neither the flag nor the code path.</para>
    /// </summary>
    internal static bool StartupDetachAudio { get; private set; }

    /// <summary>
    /// <c>--synth-clips N</c>: after a <c>--import</c>, place N synthetic clips across
    /// the project's two lanes through the REAL <c>rudis_place_clip</c>, so plan
    /// 52-09's criterion-3 measurement has a 1,000-clip project to measure.
    ///
    /// <para><b>Why a generator and not a fixture.</b> The obvious alternative is a
    /// committed 1,000-clip <c>.rud</c> under <c>resources/</c> or <c>test-media/</c>.
    /// That would be a SHIPPED artifact whose only purpose is a performance test —
    /// dead weight in the installer forever, and a file that silently rots the first
    /// time the project schema moves. Generating it at test-run time costs seconds and
    /// keeps <c>git status</c> clean (T-52-43), which is one of this plan's own
    /// acceptance criteria.</para>
    ///
    /// <para><b>It runs against an ISOLATED store.</b> Every other UAT launch shares
    /// <c>%APPDATA%\app.rudis.desktop</c> with the developer's real projects — an
    /// inherited risk 50-08 recorded and accepted. Dropping a THOUSAND synthetic clips
    /// into that store is a different order of nuisance, and the seam to avoid it
    /// already exists (<c>TimelineSmokeWindow.IsolatedInitConfigJson</c>, plan 52-01),
    /// so this route takes it. See <see cref="SyntheticClipsInitConfigJson"/>.</para>
    ///
    /// <para>Debug-only and argv-gated even in Debug, exactly like
    /// <see cref="StartupPlaceOnTimeline"/>. A Release build carries neither the flag,
    /// the parsing, nor the code path.</para>
    /// </summary>
    internal static int StartupSynthesizeClipCount { get; private set; }

    /// <summary>
    /// Plan 63-02. The argv flag behind <see cref="SimulateDeviceLostAfterMs"/>. The
    /// literal lives HERE and nowhere else in the shipped shell -- the
    /// <see cref="RemoveTrackDialogFlagName"/> / <see cref="SyntheticMediaBinFlagName"/>
    /// discipline, followed rather than re-invented, so there is exactly one place for
    /// the mechanical Release-absence proof to point at.
    /// </summary>
    private const string SimulateDeviceLostFlagName = "--simulate-device-lost-after-ms";

    /// <summary>
    /// <c>--simulate-device-lost-after-ms N</c>: N milliseconds after the preview panel
    /// attaches, call <c>rudis_preview_simulate_device_lost</c>, which really calls
    /// <c>ID3D12Device5::RemoveDevice</c> on the live preview device.
    ///
    /// <para><b>Why it exists.</b> <c>TRUST-01</c> is "a real TDR is followed, with no
    /// restart, by preview presenting again". Proving that without this flag means
    /// waiting for a real driver TDR on the machine running the test -- which is not a
    /// test, it is a hope. This is the same argument <see cref="StartupDetachAudio"/>
    /// makes for its own existence: the rule was unverifiable by any route in the app,
    /// so the plan built the route.</para>
    ///
    /// <para><b>Gated three times, and the layers are not redundant.</b> (1) This member,
    /// the parsing and the literal are all inside <c>#if DEBUG</c>, so a Release build
    /// does not contain them (<c>ReleaseHookAbsenceTests</c>' region scan). (2) Even in
    /// Debug it does nothing unless the flag is on the command line. (3) The Rust export
    /// it calls refuses fail-closed unless <c>RUDIS_DEBUG_DEVICE_LOSS=1</c> was in the
    /// environment when the process STARTED (threat T-63-04) -- so even a Debug build
    /// launched with the flag cannot remove the device unless the environment armed it
    /// too.</para>
    ///
    /// <para><c>0</c> = absent, which is also the "never fire" value: a zero-delay
    /// injection would race the first attach.</para>
    /// </summary>
    internal static int SimulateDeviceLostAfterMs { get; private set; }

    /// <summary>
    /// The parse behind <see cref="SimulateDeviceLostAfterMs"/> -- byte-for-byte the shape
    /// <see cref="ParseSynthesizeClipCount"/> established, including the "log and ignore"
    /// arm for a malformed value (a silently-dropped harness flag is how a proof passes
    /// while measuring nothing).
    /// </summary>
    private static int ParseSimulateDeviceLostAfterMs()
    {
        var args = Environment.GetCommandLineArgs();
        for (var i = 1; i < args.Length - 1; i++)
        {
            if (!string.Equals(args[i], SimulateDeviceLostFlagName, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            if (!int.TryParse(args[i + 1], out var ms) || ms <= 0)
            {
                LogDiagnostic(
                    $"{SimulateDeviceLostFlagName}: '{args[i + 1]}' is not a positive millisecond " +
                    "delay; ignored");
                return 0;
            }

            return ms;
        }

        return 0;
    }

    /// <summary>A hard ceiling on <c>--synth-clips</c>. 1,000 is the number criterion 3
    /// names and 20,000 is roughly where the generation loop stops being a test and
    /// starts being a wait; the model's own <c>MaxClips</c> is far higher, so this is a
    /// harness bound rather than a domain one.</summary>
    private const int MaxSynthesizedClips = 20_000;

    /// <summary>
    /// A throwaway per-process store for a <c>--synth-clips</c> launch — the
    /// <c>--timeline-smoke</c> shape (plan 52-01), reused rather than re-derived.
    /// </summary>
    private static string SyntheticClipsInitConfigJson()
    {
        var root = Path.Combine(
            Path.GetTempPath(), "rudis-52-09-synth", Environment.ProcessId.ToString());
        Directory.CreateDirectory(root);
        return JsonSerializer.Serialize(new Dictionary<string, object>
        {
            ["data_dir"] = Path.Combine(root, "data"),
            ["cache_dir"] = Path.Combine(root, "cache"),
            ["resource_dir"] = AppContext.BaseDirectory,
            ["self_advance"] = true,
        });
    }

    /// <summary>Parse <c>--synth-clips N</c>. A missing, malformed or non-positive N is
    /// ZERO (the flag does nothing) and is LOGGED — a typo that silently generated a
    /// different number of clips than the artifact claims would be worse than a
    /// no-op.</summary>
    private static int ParseSynthesizeClipCount()
    {
        var args = Environment.GetCommandLineArgs();
        for (var i = 1; i < args.Length - 1; i++)
        {
            if (!string.Equals(args[i], "--synth-clips", StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            if (!int.TryParse(args[i + 1], out var count) || count <= 0)
            {
                LogDiagnostic($"--synth-clips: '{args[i + 1]}' is not a positive clip count; ignored");
                return 0;
            }

            if (count > MaxSynthesizedClips)
            {
                LogDiagnostic(
                    $"--synth-clips: {count} exceeds the {MaxSynthesizedClips} harness ceiling; clamped");
                return MaxSynthesizedClips;
            }

            return count;
        }

        return 0;
    }

    /// <summary>
    /// Plan 53-06. The argv flag behind <see cref="StartupImportFolderPath"/>. The literal
    /// lives HERE and nowhere else in the shipped shell — the
    /// <see cref="RemoveTrackDialogFlagName"/> / <see cref="SyntheticMediaBinFlagName"/>
    /// discipline, followed rather than re-invented, so there is exactly one place for the
    /// mechanical Release-absence proof to point at.
    /// </summary>
    private const string ImportFolderFlagName = "--import-folder";

    /// <summary>
    /// A directory to hand to <c>rudis_import_media_folder</c> at launch, or null.
    ///
    /// <para><b>Why this route exists.</b> Plan 53-06 has to prove — through REAL OS
    /// input — that clicking a folder tile drills the MediaBin into that folder and that a
    /// breadcrumb click walks back out. That needs a real virtual-folder tree in the bin,
    /// and folders arrive by exactly ONE route: a folder import. The affordance that
    /// starts one (<c>MediaBin.ImportFolderButton</c>) opens the OS folder dialog, and
    /// plan 53-03 MEASURED that WinAppSDK 1.8 hosts that dialog OUTSIDE the calling
    /// process — a process-scoped UIA search finds nothing while it is plainly on screen —
    /// and that its name box needs two <c>Enter</c>s (the first navigates, the second
    /// commits). A UIA test built on that is a flaky test, and a flaky test is worse than
    /// none.</para>
    ///
    /// <para><b>A third ENTRY POINT, never a third implementation</b> — the framing
    /// <see cref="StartupImportPaths"/> already uses for <c>--import</c>. This flag routes
    /// into <c>MediaBin.ImportFolderAsync</c>, which is the SAME routine the button runs
    /// once its picker has returned a path: same args array, same
    /// <c>ImportMediaFolderAsync</c> wrapper, same refusal log. It cannot drift away from
    /// what the button does, because there is nothing for it to drift from.</para>
    ///
    /// <para><b>Debug-only, and argv-gated even in Debug.</b> A Release build carries
    /// neither the flag literal, the parsing, the member, nor the call site, and that is
    /// asserted against the REAL built DLL in both directions plus a source-level
    /// <c>#if DEBUG</c> region scan (<c>ReleaseHookAbsenceTests</c>).</para>
    ///
    /// <para><b>It needs no isolated store</b> for the same reason every other UAT launch
    /// does not have one: it imports into <c>%APPDATA%\app.rudis.desktop</c>, the shared
    /// risk <c>AppLauncher</c>'s own remarks record and 53-03 re-recorded. What it does NOT
    /// do is walk anything — the entire recursion, its clamps and its symlink skip stay in
    /// <c>crates/app-core/src/import.rs</c> (T-53-10).</para>
    /// </summary>
    internal static string? StartupImportFolderPath { get; private set; }

    /// <summary>
    /// A throwaway per-process store for a folder-import launch — the
    /// <see cref="SyntheticClipsInitConfigJson"/> shape, reused rather than re-derived.
    /// </summary>
    private static string ImportFolderInitConfigJson()
    {
        var root = Path.Combine(
            Path.GetTempPath(), "rudis-53-06-folder", Environment.ProcessId.ToString());
        Directory.CreateDirectory(root);
        return JsonSerializer.Serialize(new Dictionary<string, object>
        {
            ["data_dir"] = Path.Combine(root, "data"),
            ["cache_dir"] = Path.Combine(root, "cache"),
            ["resource_dir"] = AppContext.BaseDirectory,
            ["self_advance"] = true,
        });
    }

    /// <summary>Parse the folder-import flag's path. A missing or non-existent directory
    /// is null (the flag does nothing) and is LOGGED — a typo that silently measured an
    /// empty bin would be worse than a no-op. Existence is checked with a plain
    /// <c>Exists</c> probe; nothing here enumerates a directory (T-53-10).</summary>
    private static string? ParseImportFolderPath()
    {
        var args = Environment.GetCommandLineArgs();
        for (var i = 1; i < args.Length - 1; i++)
        {
            if (!string.Equals(args[i], ImportFolderFlagName, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            var candidate = args[i + 1];
            if (!Directory.Exists(candidate))
            {
                LogDiagnostic($"{ImportFolderFlagName}: '{candidate}' is not an existing directory; ignored");
                return null;
            }

            return Path.GetFullPath(candidate);
        }

        return null;
    }
#endif

    /// <summary>
    /// Parse <c>--import</c> from argv. Consumes every following non-flag token, so
    /// <c>--import a.mp4 b.mp4</c> imports both, and stops at the next <c>-</c>-prefixed
    /// argument.
    /// </summary>
    private static IReadOnlyList<string> ParseImportArgs()
    {
        var args = Environment.GetCommandLineArgs();
        var paths = new List<string>();
        for (var i = 1; i < args.Length; i++)
        {
            if (!string.Equals(args[i], "--import", StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }
            for (var j = i + 1; j < args.Length && !args[j].StartsWith('-'); j++)
            {
                if (File.Exists(args[j]))
                {
                    paths.Add(Path.GetFullPath(args[j]));
                }
                else
                {
                    LogDiagnostic($"--import: skipping '{args[j]}' (no such file)");
                }
            }
        }
        return paths;
    }

    /// <summary>
    /// The shell's diagnostic sink. Every mirror resync, every poll fault and every
    /// unknown event name lands here with its cause (D-09: "a missed patch buys
    /// exactly ONE full resync, and it is logged"). Debug output for a debugger plus
    /// a bounded in-memory tail that survives a Release build, because a resync that
    /// nobody can see is a resync nobody will diagnose.
    ///
    /// <para><b>⚠ <see cref="Trace"/> AND <see cref="Debug"/>, and the pair is the point
    /// (plan 63-03, D-06 / D-60.1-14).</b> <c>Debug.WriteLine</c> is compiled OUT of a
    /// Release build. For the whole of v8 that made this method's third line a no-op on the
    /// only build a customer ever runs, so the shell wrote every diagnosis it had into a
    /// process-private ring and told nobody — which is how "it doesn't open the file and
    /// its blank" cost an hour to attribute. <c>Trace.WriteLine</c> survives Release and
    /// reaches the OS debug channel, so a support session (or a UIA gate) can attach a
    /// listener and read the app's own sentences back.</para>
    ///
    /// <para><b>What this deliberately is NOT (T-63-10):</b> no file sink and no listener
    /// is registered here. .NET's <c>DefaultTraceListener</c> writes to
    /// <c>OutputDebugString</c> and nowhere else, so a shipped Release still writes nothing
    /// to disk and leaks nothing to anyone who is not deliberately watching. Diagnosability
    /// costs an attached listener, which is the correct price.</para>
    ///
    /// <para><b>Log-safe by construction, and it must stay that way.</b> Callers name
    /// variables, never values (see <c>EnvBootstrap.Describe</c>); nothing that reaches
    /// this method may carry key material, because reaching Trace widens the audience from
    /// "a debugger" to "any attached listener".</para>
    /// </summary>
    internal static void LogDiagnostic(string line)
    {
        LastDiagnostic = line;
        Diagnostics.Enqueue(line);
        while (Diagnostics.Count > DiagnosticCapacity && Diagnostics.TryDequeue(out _))
        {
            // Trim oldest-first; the loop condition does the work.
        }
        Trace.WriteLine($"[rudis] {line}");

        // Kept beside Trace rather than replaced by it. On a DEBUG build this does emit
        // the line twice (both default listeners end at OutputDebugString) — accepted,
        // because the two are not the same guarantee: `Trace`'s listener collection is
        // configurable and removable, `Debug`'s route to an attached debugger is not, and
        // a developer stepping through must never lose the diagnostic to someone else's
        // listener configuration. RELEASE, the build this plan is about, emits it once.
        Debug.WriteLine($"[rudis] {line}");
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        // FIRST, before BuildInitConfigJson() below hands a ctx to rudis_init: install the
        // .env variables so the BYO-key fallbacks inside agent-llm / agent-gen can see them.
        // A Desktop-shortcut launch inherits neither a dev shell's exports nor the retired
        // Tauri shell's dotenvy call, so without this the Chat pill reads disconnected on a
        // machine whose keys are sitting right there in the repo. Log-safe by construction:
        // Describe() names variables, never values.
        LogDiagnostic(EnvBootstrap.Load().Describe());

        StartupImportPaths = ParseImportArgs();
#if DEBUG
        StartupPlaceOnTimeline = Array.Exists(
            Environment.GetCommandLineArgs(),
            a => string.Equals(a, "--place-on-timeline", StringComparison.OrdinalIgnoreCase));

        // Parsed in the SAME pass as --import / --place-on-timeline, and inside the same
        // #if DEBUG: a Release build carries neither this line nor the member it writes.
        ShowRemoveTrackDialogOnLaunch = Array.Exists(
            Environment.GetCommandLineArgs(),
            a => string.Equals(a, RemoveTrackDialogFlagName, StringComparison.OrdinalIgnoreCase));

        // Plan 52-08, same pass, same #if DEBUG.
        StartupDetachAudio = Array.Exists(
            Environment.GetCommandLineArgs(),
            a => string.Equals(a, "--detach-audio", StringComparison.OrdinalIgnoreCase));

        // Plan 52-09, same pass, same #if DEBUG.
        StartupSynthesizeClipCount = ParseSynthesizeClipCount();

        // Plan 53-04, same pass, same #if DEBUG: a Release build carries neither this
        // line, nor the member it writes, nor the flag string it looks for.
        StartupSyntheticMediaBin = ParseSyntheticMediaBinCount();

        // Plan 63-02, same pass, same #if DEBUG, same guarantee. See
        // SimulateDeviceLostAfterMs for the three independent gates this is the second
        // of -- the third is the Rust export's own fail-closed environment latch.
        SimulateDeviceLostAfterMs = ParseSimulateDeviceLostAfterMs();

        // Plan 53-06, same pass, same #if DEBUG, same guarantee.
        StartupImportFolderPath = ParseImportFolderPath();

        // Plan 54-05, same pass, same #if DEBUG, same guarantee: a Release build carries
        // neither this line, nor the member it writes, nor the flag string it looks for.
        SynthChatCards = Array.Exists(
            Environment.GetCommandLineArgs(),
            a => string.Equals(a, SynthChatCardsFlagName, StringComparison.OrdinalIgnoreCase));
#endif

        var initConfig = BuildInitConfigJson();
#if DEBUG
        // Phase 52 plan 52-01's D-05 tripwire (--timeline-smoke) drives a REAL import +
        // place + export to prove the frozen wgpu-26 device coexists in-process with the
        // Timeline's wgpu-29 ones. A real mutation must never land in the developer's
        // actual project store, so that launch — and ONLY that launch — is pointed at a
        // throwaway temp root. No Release code path is affected: both the call and its
        // target type are compiled out.
        if (Rudis.Shell.Introspection.TimelineSmokeWindow.IsRequested())
        {
            initConfig = Rudis.Shell.Introspection.TimelineSmokeWindow.IsolatedInitConfigJson();
            LogDiagnostic("timeline-smoke: engine pointed at an isolated temp store for this launch");
        }
        else if (StartupSynthesizeClipCount > 0)
        {
            // Plan 52-09. A thousand synthetic clips are a MEASUREMENT, not a project,
            // and they must not land in the store the developer's real work lives in.
            initConfig = SyntheticClipsInitConfigJson();
            LogDiagnostic(
                $"--synth-clips {StartupSynthesizeClipCount}: engine pointed at an isolated " +
                "temp store for this launch");
        }
        else if (StartupImportFolderPath is { Length: > 0 })
        {
            // Plan 53-06, following 52-09's precedent for the same two reasons and one
            // more. (1) A folder import writes a whole TREE of items and virtual folders
            // into the store — a different order of nuisance from one file, and exactly
            // the case 52-09 pointed away from %APPDATA%. (2) 53-03 recorded that the
            // eyes-on UAT runs polluted the developer's real store; this route does not
            // repeat that. (3) It is also what makes the proof it exists for POSSIBLE: a
            // fresh store means the MediaBin's root level holds EXACTLY the tiles this
            // launch created, so "one folder tile at root, two tiles inside it" is a
            // deterministic claim rather than a claim about whatever the developer had
            // imported that week.
            initConfig = ImportFolderInitConfigJson();
            LogDiagnostic(
                "startup folder import: engine pointed at an isolated temp store for this launch");
        }
#endif

        Engine = RudisNative.Create(initConfig);
        if (!Engine.IsInvalid)
        {
            // The mirror's ONLY route to the backend is NativeMirrorSource, so every
            // read it performs is serialised onto the one interop worker by
            // construction (50-02 §1.5(2)) — the policy cannot be bypassed here.
            Mirror = new ShellMirror(new NativeMirrorSource(Engine), LogDiagnostic);
        }

#if DEBUG
        // Phase 69 SC-4 red leg (D-69-22): an INJECTED fault that makes startup require a key —
        // the exact behaviour the editor must never have (CLAUDE.md rule 5). The window is
        // NOT opened here; RunRequireKeyAtStartupFaultAsync reads the status first and opens it
        // only when a key is configured. Awaited, not blocked on: MechanicalGatesTests forbids a
        // sync-over-async wait anywhere in the shell, and the fault only needs "no window before
        // the credential read", which the continuation gives exactly.
        if (string.Equals(Environment.GetEnvironmentVariable(Fault69EnvName), Fault69RequireKeyAtStartup, StringComparison.Ordinal)
            && Engine is { IsInvalid: false })
        {
            _ = RunRequireKeyAtStartupFaultAsync(Engine);
            return;
        }
#endif

        OpenMainWindow();
    }

#if DEBUG
    /// <summary>FAULT-69 <c>require-key-at-startup</c> (Debug only, D-69-22). Reads the agent
    /// status BEFORE any window exists; with no key it writes ONE stderr line and exits 69
    /// (the harness asserts both, so a red for any other reason is not mistaken for this
    /// fault). With a key configured, startup continues into the normal window.
    ///
    /// <para>Review 69 IN-04: the task is fire-and-forget from OnLaunched, so an exception
    /// here would otherwise go unobserved and leave a windowless process (the red-leg test
    /// would then time out with a misleading "the window opened"). Any exception is logged
    /// (type name only) and the process exits with a DISTINCT code, 70.</para></summary>
    private async Task RunRequireKeyAtStartupFaultAsync(RudisNative engine)
    {
        try
        {
            var status = await engine.AgentStatusAsync();
            var configured = status.Kind == RudisResultKind.Ok && status.Value.ValueKind == JsonValueKind.Object
                && status.Value.TryGetProperty("key_configured", out var kc) && kc.ValueKind == JsonValueKind.True;
            if (!configured)
            {
                const string line = "[rudis] FAULT-69 require-key-at-startup: no API key configured — refusing to open the editor (injected fault, Debug only)";
                LogDiagnostic(line);
                Console.Error.WriteLine(line);
                Console.Error.Flush();
                Environment.Exit(69);
                return;
            }

            LogDiagnostic("FAULT-69 require-key-at-startup: a key is configured — startup continues (fault armed but not tripped)");
            OpenMainWindow();
        }
        catch (Exception ex)
        {
            var line = $"[rudis] FAULT-69 require-key-at-startup: the fault path itself failed ({ex.GetType().Name}) — exiting 70";
            LogDiagnostic(line);
            Console.Error.WriteLine(line);
            Console.Error.Flush();
            Environment.Exit(70);
        }
    }
#endif

    /// <summary>The tail of <see cref="OnLaunched"/>: the Debug introspection hook, the main
    /// window, and the background update check — in that order. Split out (Phase 69) only so
    /// the Debug-only FAULT-69 startup fault can defer it past an awaited status read.</summary>
    private void OpenMainWindow()
    {
        // D-10's secondary UAT assertion channel. The call and its target type are
        // BOTH inside #if DEBUG: a Release build has neither — not merely a no-op call,
        // an absent one (ReleaseHookAbsenceTests.cs asserts this mechanically). Even
        // here it starts nothing unless RUDIS_SHELL_INTROSPECT=1 is set (off by
        // default — StartIfRequested's own null-return path).
#if DEBUG
        IntrospectionHook = Rudis.Shell.Introspection.IntrospectionHook.StartIfRequested(Mirror);
#endif

#if DEBUG
        // The D-05 tripwire REPLACES MainWindow rather than opening beside it: the point
        // is to measure two SwapChainPanels in the real shell process with the real WinUI 3
        // ABI initialised, not to add a surface to the running app. Debug-only, argv-gated,
        // and asserted ABSENT from Release by ReleaseHookAbsenceTests.
        _window = Rudis.Shell.Introspection.TimelineSmokeWindow.IsRequested()
            ? new Rudis.Shell.Introspection.TimelineSmokeWindow()
            : new MainWindow();
#else
        _window = new MainWindow();
#endif
        _window.Activate();

        // AFTER Activate(), deliberately and measurably: the update check must never be
        // between the user and their window (T-62-07). Returns immediately — see
        // UpdateService.StartBackgroundCheck, which does nothing at all unless a feed URL
        // was compiled into THIS binary and this binary is a Velopack install.
        Rudis.Shell.Updates.UpdateService.StartBackgroundCheck();
    }

    /// <summary>
    /// The InitConfig this shell passes to <c>rudis_init</c>
    /// (crates/ffi/include/rudis_ffi.h — the documented config keys).
    ///
    /// <para><b>Clock ownership (D-17 / SHELL-06):</b> <c>"self_advance": true</c> —
    /// the ENGINE owns the playback clock. This shell NEVER ticks a clock: it calls
    /// only play/pause/seek/step and polls the scalar position. No per-frame managed
    /// clock code exists anywhere in shell/ — that is the point.</para>
    ///
    /// <para><b>Why these exact directories (RESEARCH §2 ACTION):</b> the identifier
    /// <c>app.rudis.desktop</c> is INHERITED, not invented here — it was the Tauri
    /// shell's identifier, and Phase 50 matched it so the C# shell read the SAME
    /// project store rather than starting an empty one. That parity requirement ended
    /// with Phase 55's GATE-07 cutover (this is now the only shell), but the paths
    /// stay: real user projects live under <c>%APPDATA%\app.rudis.desktop</c>
    /// (roaming data — the project store) and <c>%LOCALAPPDATA%\app.rudis.desktop</c>
    /// (cache — posters/preview), and changing the identifier would orphan them.
    /// <c>resource_dir</c> = the exe directory, where the build stages native
    /// resources.</para>
    ///
    /// <para><b>Debug seams (Phase 69, D-69-03).</b> In a Debug build only, the
    /// <c>TestDataRootEnvName</c> variable points <c>data_dir</c>/<c>cache_dir</c> at
    /// <c>&lt;root&gt;\data</c> / <c>&lt;root&gt;\cache</c> (so a harness never touches the
    /// owner's real project store), and the <c>TestCredentialServiceEnvName</c> variable is
    /// passed as
    /// <c>credential_service</c> (the Rust side accepts only <c>rudis-test-*</c> and REFUSES
    /// to initialise on any other supplied value — never a fallback to production, review 69
    /// WR-01 — so the owner's real Credential Manager entries are unreachable from a test). Neither the
    /// reads nor the literals exist in a Release build (ReleaseHookAbsenceTests).</para>
    /// </summary>
    private static string BuildInitConfigJson()
    {
        var roaming = Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData);
        var local = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        var config = new Dictionary<string, object>
        {
            ["data_dir"] = Path.Combine(roaming, "app.rudis.desktop"),
            ["cache_dir"] = Path.Combine(local, "app.rudis.desktop"),
            ["resource_dir"] = AppContext.BaseDirectory,
            ["self_advance"] = true,
        };
#if DEBUG
        var testRoot = Environment.GetEnvironmentVariable(TestDataRootEnvName);
        if (!string.IsNullOrWhiteSpace(testRoot))
        {
            var dataDir = Path.Combine(testRoot, "data");
            var cacheDir = Path.Combine(testRoot, "cache");
            Directory.CreateDirectory(dataDir);
            Directory.CreateDirectory(cacheDir);
            config["data_dir"] = dataDir;
            config["cache_dir"] = cacheDir;
            LogDiagnostic("engine pointed at an isolated test data root (RUDIS_TEST_DATA_ROOT; Debug seam)");
        }

        var testService = Environment.GetEnvironmentVariable(TestCredentialServiceEnvName);
        if (!string.IsNullOrWhiteSpace(testService))
        {
            config["credential_service"] = testService;
            LogDiagnostic("credential_service override requested (RUDIS_TEST_CREDENTIAL_SERVICE; Debug seam — the Rust side accepts only rudis-test-* and refuses init otherwise)");
        }
#endif
        return JsonSerializer.Serialize(config);
    }
}
