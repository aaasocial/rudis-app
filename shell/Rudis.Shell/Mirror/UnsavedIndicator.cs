namespace Rudis.Shell.Mirror;

/// <summary>
/// The unsaved <c>•</c> the <c>TitleBar</c> renders (design handoff README:84,
/// 50-UI-SPEC §5 row 3), as a PURE function of two numbers.
///
/// <para>It lives here, beside the mirror rather than inside
/// <c>MainWindow.xaml.cs</c>, for the reason plan 60.1-05 recorded as a structural
/// finding: <c>shell/Rudis.Shell.Tests</c> does not project-reference the shell. It
/// compiles a hand-picked set of WinUI-free production sources, and
/// <c>Mirror/**/*.cs</c> is one of its directory globs — so a rule that lives in a
/// region's <c>.xaml.cs</c> is unreachable from a test no matter what its
/// accessibility says, and a rule that lives HERE needs no csproj edit at all.</para>
/// </summary>
internal static class UnsavedIndicator
{
    /// <summary>
    /// Does the backend hold mutations that no write has captured?
    ///
    /// <para><b>OLD RULE, and why it is now wrong.</b>
    /// <c>mirror.LastAppliedStoreSeq &gt; 0</c> was EXACT while it held. 50-02 §2
    /// recorded all three of its premises, and the third was <i>"no export reachable
    /// from these regions can persist anything"</i> — the only persistence hooks were
    /// project-switch, project-create and end-of-agent-turn, none of which the
    /// <c>TitleBar</c> or the <c>Toolbar</c> could reach. Phase 60.1 added
    /// <c>rudis_save_project</c> and put it on the Toolbar and on the close path, so
    /// that premise is FALSE and the dot would light on the first edit and stay lit
    /// forever, through every save, for the rest of the session. A dot that is always
    /// on says nothing at all.</para>
    ///
    /// <para><b>NEW RULE: the store has moved, and not to the number we last wrote.</b>
    /// Two facts, both real: <paramref name="storeSeq"/> is
    /// <c>Store::seq</c>, a monotonic mutation counter that moves forward even across
    /// undo/redo (<c>store.rs</c>); <paramref name="lastPersistedSeq"/> is the value
    /// that rode the save's OWN envelope (<c>{"path":.., "seq":N}</c>), captured in the
    /// same store guard as the bytes (LAT-02) so it describes exactly what was written.
    /// There is deliberately NO separate "is dirty" export and
    /// <c>ring::EVENT_NAMES</c> stays at 6.</para>
    ///
    /// <para><b>Why not the plain <c>storeSeq &gt; lastPersistedSeq</c>.</b> The store
    /// can move BACKWARDS: <c>Store::from_project</c> resets the counter to 0 on every
    /// open and create. A persisted number left over from the OUTGOING document then
    /// sits above the incoming one, and the plain comparison answers "saved" for edits
    /// that have never been written anywhere — the one direction this indicator must
    /// not err in. A lower store seq means a different document, so the persisted
    /// number does not describe it and only <c>storeSeq &gt; 0</c> is meaningful.</para>
    ///
    /// <para><b>⚠ ONE RESIDUAL, recorded rather than hidden.</b> The end-of-agent-turn
    /// hook (<c>agent_turn.rs</c>) persists the project without telling the shell, so
    /// after an agent turn the dot may stay lit although the file on disk is current.
    /// It errs in the SAFE direction — it says "unsaved" when saved, never the reverse
    /// — and closing it needs a last-persisted-seq signal that does not exist in the
    /// ABI. Recorded as <c>D-60.1-10</c> in the phase's deferred-items ledger rather
    /// than left as a silent gap. (Plan 60.1-08's orphan sweep, which the 60.1-06 plan
    /// document named as its carrier, had already shipped by then; the ledger is where
    /// it actually lives.)</para>
    /// </summary>
    /// <param name="storeSeq">
    /// <c>ShellMirror.LastAppliedStoreSeq</c> — the backend's mutation counter as the
    /// mirror last learned it.
    /// </param>
    /// <param name="lastPersistedSeq">
    /// <c>RudisNative.LastPersistedStoreSeq</c> — the counter at the last successful
    /// save, or 0 after an open or a create (the document then equals what is on disk).
    /// </param>
    internal static bool HasUnpersistedEdits(ulong storeSeq, ulong lastPersistedSeq)
        => storeSeq > 0 && storeSeq != lastPersistedSeq;
}
