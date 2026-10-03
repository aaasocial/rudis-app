// D-05's tripwire interop (Phase 52, plan 52-01, Task 2).
//
// ⚠ THE ENTIRE FILE IS COMPILED OUT OF RELEASE. Everything below — including the
// namespace declaration — lives inside one #if DEBUG block, so a Release build of
// Rudis.Shell carries NO type named TimelineSmokeNative or SmokeStats at all: not
// disabled, ABSENT. That absence is asserted MECHANICALLY by
// shell/Rudis.Shell.Tests/ReleaseHookAbsenceTests.cs (the same collectible
// AssemblyLoadContext proof that already polices IntrospectionHook — extended, never
// duplicated into a parallel gate).
#if DEBUG
using System.Runtime.InteropServices;
using Microsoft.UI.Xaml.Controls;

// ⚠ NAMED `Introspection`, not `Debug`. The FOLDER is Debug/ (the plan's path), but a C#
// namespace literally named `Rudis.Shell.Debug` shadows `System.Diagnostics.Debug` for
// every unqualified `Debug.WriteLine` already written inside the `Rudis.Shell` namespace
// (App.xaml.cs's LogDiagnostic) — nested-namespace lookup beats a `using`. Plan 50-08
// already paid for that lesson once; it is not re-learned here.
namespace Rudis.Shell.Introspection;

/// <summary>
/// The seven counters <c>rudis_timeline_smoke_stats</c> writes, mirroring the Rust
/// <c>#[repr(C)] SmokeStats</c> field order EXACTLY: five <c>u64</c> then two <c>u32</c>
/// (crates/timeline-render/src/smoke.rs). A Rust-side test pins
/// <c>size_of::&lt;SmokeStats&gt;() == 5*8 + 2*4</c>, so a field added on that side without
/// one added here becomes a red test rather than silently misread bytes.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
internal struct SmokeStats
{
    internal ulong FramesPresented;
    internal ulong MinDeltaUs;
    internal ulong P50DeltaUs;
    internal ulong P99DeltaUs;
    internal ulong MaxDeltaUs;
    internal uint PresentErrors;
    internal uint DeviceLost;
}

/// <summary>
/// P/Invoke surface for <c>rudis_timeline.dll</c> — the SECOND native artifact beside
/// <c>rudis_ffi.dll</c>, staged by <c>crates/timeline-render/Rudis.Timeline.targets</c>.
///
/// <para><b>Why this does not go through <c>RudisNative</c>'s serialized worker.</b> That
/// worker exists to serialize the ENGINE ABI (<c>rudis_ffi.dll</c>, one <c>RudisCtx</c>,
/// one logical flow). <c>rudis_timeline.dll</c> is a different library with no shared
/// state, and its one thread-affine call — <c>attach</c> — must run on the PANEL'S UI
/// THREAD by construction (<c>ISwapChainPanelNative::SetSwapChain</c> returns
/// <c>RPC_E_WRONG_THREAD</c> anywhere else). Posting it to a background worker would be
/// exactly the bug. This whole file is Debug-only regardless.</para>
/// </summary>
internal static partial class TimelineSmokeNative
{
    private const string Library = "rudis_timeline";

    /// <summary>
    /// Bind an independent DX12 <c>wgpu</c> device to <paramref name="panel"/> and start a
    /// dedicated present thread clearing it to <c>(r, g, b)</c>.
    ///
    /// <para><b>MUST be called on the panel's own UI thread</b> — see the class remarks.
    /// Returns <see cref="IntPtr.Zero"/> on any failure (bad pointer, no DX12 adapter,
    /// surface creation refused); the Rust side never dereferences an unverified pointer,
    /// it <c>QueryInterface</c>s for <c>ISwapChainPanelNative</c> first.</para>
    ///
    /// <para><paramref name="scale"/> is the panel's own <c>CompositionScaleX</c>, added by
    /// plan 52-12. <paramref name="widthPx"/>/<paramref name="heightPx"/> are PHYSICAL
    /// pixels and a <c>SwapChainPanel</c> composites one buffer pixel per DIP, so the
    /// swapchain needs the matching inverse-scale matrix transform. A flat clear colour
    /// would hide its absence perfectly — which is exactly why the tripwire carries it
    /// rather than passing 1.0.</para>
    /// </summary>
    [LibraryImport(Library)]
    internal static partial IntPtr rudis_timeline_smoke_attach(
        IntPtr panel, uint widthPx, uint heightPx, float scale, float r, float g, float b);

    /// <summary>Copy the live counters out. Safe from any thread.</summary>
    [LibraryImport(Library)]
    internal static partial int rudis_timeline_smoke_stats(IntPtr handle, out SmokeStats stats);

    /// <summary>Zero the counters so one ablation phase measures only its own window.</summary>
    [LibraryImport(Library)]
    internal static partial int rudis_timeline_smoke_reset_stats(IntPtr handle);

    /// <summary>Stop the present thread, drop the device and swapchain, free the handle.</summary>
    [LibraryImport(Library)]
    internal static partial int rudis_timeline_smoke_detach(IntPtr handle);

    /// <summary>Adapter / format / size / occlusion-skip description, UTF-8, NUL-terminated.</summary>
    [LibraryImport(Library)]
    internal static partial int rudis_timeline_smoke_describe(IntPtr handle, Span<byte> outUtf8, uint cap);

    /// <summary>Managed wrapper over <see cref="rudis_timeline_smoke_describe"/>.</summary>
    internal static string Describe(IntPtr handle)
    {
        if (handle == IntPtr.Zero)
        {
            return "(not attached)";
        }

        Span<byte> buffer = stackalloc byte[512];
        var written = rudis_timeline_smoke_describe(handle, buffer, (uint)buffer.Length);
        return written > 0
            ? System.Text.Encoding.UTF8.GetString(buffer[..written])
            : $"(describe failed, rc={written})";
    }
}

/// <summary>
/// The C# half of the pointer hand-off, isolated so the ONE idiom that works is written
/// down once.
/// </summary>
internal static class TimelineSmokeInterop
{
    /// <summary>
    /// Take a COM pointer to <paramref name="panel"/> for the native side.
    ///
    /// <para><b>The idiom:</b> <c>WinRT.MarshalInspectable&lt;object&gt;.FromManaged</c>
    /// yields an AddRef'd <c>IInspectable*</c>. The Rust side takes its OWN reference (it
    /// <c>QueryInterface</c>s for <c>ISwapChainPanelNative</c>), so the reference returned
    /// here belongs to the caller and MUST be handed to
    /// <see cref="ReleasePanelPointer"/> when the panel is detached — see
    /// <c>TimelineSmokeWindow</c>'s Closed handler.</para>
    ///
    /// <para><b>The documented trap (44-RESEARCH Pattern 4):</b> the naive WinRT-projection
    /// cast <c>panel.As&lt;ISwapChainPanelNative&gt;()</c> throws
    /// <c>InvalidCastException</c> for exactly this interop. It is named here so nobody
    /// re-derives it, and so the record says what actually worked rather than what was
    /// expected to.</para>
    /// </summary>
    internal static IntPtr PanelPointer(SwapChainPanel panel) =>
        WinRT.MarshalInspectable<object>.FromManaged(panel);

    /// <summary>Drop the reference <see cref="PanelPointer"/> took.</summary>
    internal static void ReleasePanelPointer(IntPtr pointer)
    {
        if (pointer != IntPtr.Zero)
        {
            Marshal.Release(pointer);
        }
    }
}
#endif
