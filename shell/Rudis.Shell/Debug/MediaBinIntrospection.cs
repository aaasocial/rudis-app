// Plan 53-04, Task 1 — the publish/read box between the MediaBin region (UI thread)
// and the introspection pipe (background thread).
//
// ⚠ THE ENTIRE FILE IS COMPILED OUT OF RELEASE — everything below, including the
// namespace declaration, sits inside the single DEBUG conditional-compilation block
// that opens on the next line and closes on the last. Asserted MECHANICALLY against
// the real built DLL, in BOTH directions, by ReleaseHookAbsenceTests
// (`mediabin_synthetic_surface_absent_from_release_and_present_in_debug`).
#if DEBUG

// ⚠ `Introspection`, not `Debug` — see MediaBinSyntheticBin.cs / IntrospectionHook.cs.
namespace Rudis.Shell.Introspection;

/// <summary>
/// The MediaBin's own numbers, PUBLISHED as plain values from the UI thread and read
/// back by <see cref="IntrospectionHook"/>'s pipe thread.
///
/// <para><b>⚠ WR-03's boundary, applied one level up, and it is the reason this type
/// exists at all.</b> <c>HandleRequest</c> runs on a background pipe thread. Reading
/// <c>ShellMirror</c> from there already RACES the UI thread and is guarded by a broad
/// catch. Reading the VISUAL TREE from there would not merely race — asking a
/// <c>UIElement</c> for its realized children off the thread that owns it is
/// <c>RPC_E_WRONG_THREAD</c>, a hard failure rather than a torn read. So the region
/// pushes ints and strings HERE, on the UI thread, and the pipe reads ints and strings.
/// No <c>UIElement</c> is reachable from this file, deliberately.</para>
///
/// <para><b>Still an ASSERTION channel, never a control channel (T-50-35).</b> Nothing
/// here is settable from outside the process, nothing here is read by product code, and
/// no value published here feeds back into a command. This is a one-way readout.</para>
///
/// <para>No locks. The two counters are <see cref="int"/>s written and read through
/// <see cref="Volatile"/>, which cannot tear; the three strings are ordinary volatile
/// reference fields. A reader can observe a publish half-applied — a container count
/// from one tick beside a folder path from the next — and that is acceptable because
/// the consumer polls for a value that has STOPPED CHANGING rather than trusting a
/// single sample. A lock here would put the pipe thread and the UI thread in contention
/// over a measurement, which is the one thing a measurement must not do.</para>
/// </summary>
internal static class MediaBinIntrospection
{
    private static volatile string _controlKind = "";
    private static volatile string _currentFolder = "";
    private static volatile string? _draggingMediaId;

    // The two counters go through Volatile.Read/Write at every access rather than
    // carrying the field modifier: the modifier would be equally correct here, but it
    // is invisible at the call site, and these two fields are the ONLY values in this
    // process read from a thread other than the one that writes them. Spelling the
    // barrier where it happens is what stops a later edit from quietly adding a
    // non-volatile read.
    private static int _levelItemCount;
    private static int _realizedContainers;

    /// <summary>
    /// Called from the MediaBin region ON THE UI THREAD — from the end of its mirror
    /// apply, from its container-lifetime hook, and from the settle timer a synthetic
    /// launch starts. Cheap by construction: five stores, no allocation, no formatting.
    /// </summary>
    internal static void Publish(
        string controlKind,
        int levelItemCount,
        int realizedContainers,
        string currentFolder,
        string? draggingMediaId)
    {
        _controlKind = controlKind;
        _currentFolder = currentFolder;
        _draggingMediaId = draggingMediaId;
        Volatile.Write(ref _levelItemCount, levelItemCount);
        Volatile.Write(ref _realizedContainers, realizedContainers);
    }

    /// <summary>The last published sample, as plain values.</summary>
    internal static (string ControlKind, int LevelItemCount, int RealizedContainers,
                     string CurrentFolder, string? DraggingMediaId) Read()
        => (_controlKind,
            Volatile.Read(ref _levelItemCount),
            Volatile.Read(ref _realizedContainers),
            _currentFolder,
            _draggingMediaId);

    /// <summary>
    /// The <c>mediabin</c> response's payload, projected in ONE place — the same shape
    /// <c>Regions.Preview.DescribeForIntrospection()</c> established, so
    /// <see cref="IntrospectionHook.HandleRequest"/> stays a switch of one-liners and
    /// the wire contract lives beside the values it describes.
    ///
    /// <para><c>controlKind</c> is the D-16 decision CONFIRMED AT RUNTIME: the app names
    /// the control it actually built, rather than a reader inferring it from XAML.</para>
    /// </summary>
    internal static object Describe()
    {
        var sample = Read();
        return new
        {
            controlKind = sample.ControlKind,
            levelItemCount = sample.LevelItemCount,
            realizedContainers = sample.RealizedContainers,
            currentFolder = sample.CurrentFolder,
            draggingMediaId = sample.DraggingMediaId,
        };
    }
}
#endif
