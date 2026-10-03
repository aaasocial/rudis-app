// Plan 53.1-02, Task 3 — the publish/read box for the Timeline's LAST DROP OUTCOME.
//
// ⚠ THE ENTIRE FILE IS COMPILED OUT OF RELEASE — everything below, including the
// namespace declaration, sits inside the single DEBUG conditional-compilation block
// that opens on the next line and closes on the last, asserted mechanically in BOTH
// directions by ReleaseHookAbsenceTests.
#if DEBUG

// ⚠ `Introspection`, not `Debug` — see MediaBinIntrospection.cs / IntrospectionHook.cs.
namespace Rudis.Shell.Introspection;

/// <summary>
/// What the last Timeline drop did, PUBLISHED as plain values from the UI thread and
/// read back by <see cref="IntrospectionHook"/>'s pipe thread.
///
/// <para><b>⚠ PUBLISHED WHERE THE VALUE CHANGES, and that is the entire reason this
/// type exists rather than a field on the render-tick snapshot.</b> Plan 53-06 lost a
/// whole verdict to exactly that mistake: <c>_draggingMediaId</c> was set by the drag
/// handler, but every publish site it had was one a drag does not trigger, so the pipe
/// served a stale null and the finding would have been about the instrument rather than
/// about the product. A drop completes off the render tick. It publishes here, from
/// <c>TryPlaceFromDropAsync</c>, at the moment the outcome is known.</para>
///
/// <para><b>Still an ASSERTION channel, never a control channel (T-50-35).</b> Nothing
/// here is settable from outside the process, nothing here is read by product code, and
/// no value published here feeds back into a command. One-way readout, primitives and
/// strings only, and no visual-tree object is reachable from this file — deliberately,
/// because asking a live XAML element for state off the thread that owns it is
/// <c>RPC_E_WRONG_THREAD</c>, a hard failure rather than a torn read.</para>
///
/// <para>No locks, for the reason <c>MediaBinIntrospection</c> gives: the consumer polls
/// for a value that has STOPPED CHANGING rather than trusting a single sample, and a
/// lock here would put the pipe thread and the UI thread in contention over a
/// measurement.</para>
/// </summary>
internal static class TimelineDropIntrospection
{
    private static volatile string _mediaId = "";

    // ⚠ `_outcome`, and the tuple element below is `Outcome`, NOT `Result` — deliberately.
    // `MechanicalGatesTests.no_blocking_waits_in_shell_sources` forbids `.Result` anywhere
    // in the shipped shell because sync-over-async on the UI thread deadlocks against its
    // own continuation (D-08 / T-50-32). A tuple element called `Result` reads as exactly
    // that at the call site, and the gate is right not to try to tell them apart. The wire
    // field is still `last_drop_result`; only the C# name changed.
    private static volatile string _outcome = "";     // "" | "ok" | "refused" | "fault"
    private static volatile string? _error;

    // Volatile.Read/Write at every access rather than the field modifier — the same
    // rule MediaBinIntrospection states: spelling the barrier where it happens is what
    // stops a later edit from quietly adding a non-volatile read.
    private static int _track = -1;
    private static long _startUs = -1;
    private static int _seq;

    /// <summary>Called from the UI thread the instant a drop's outcome is known.
    /// <c>seq</c> increments on every publish so a poller can tell "no drop yet" from
    /// "the same drop again" without comparing every field.</summary>
    internal static void Publish(string mediaId, int track, long startUs, string outcome, string? error)
    {
        _mediaId = mediaId;
        _outcome = outcome;
        _error = error;
        Volatile.Write(ref _track, track);
        Volatile.Write(ref _startUs, startUs);

        // Written LAST, so a reader that sees a new seq is looking at fields that have
        // already landed rather than at a publish caught halfway.
        Volatile.Write(ref _seq, Volatile.Read(ref _seq) + 1);
    }

    /// <summary>The last published outcome, as plain values.</summary>
    internal static (string MediaId, int Track, long StartUs, string Outcome, string? Error, int Seq) Read()
        => (_mediaId,
            Volatile.Read(ref _track),
            Volatile.Read(ref _startUs),
            _outcome,
            _error,
            Volatile.Read(ref _seq));
}
#endif
