using Velopack;

namespace Rudis.Shell;

/// <summary>
/// The shell's hand-written entry point (Phase 62, plan 62-02 — SHIP-02/SHIP-03).
///
/// <para><b>Why this file exists at all.</b> Until this plan the entry point was the XAML
/// compiler's GENERATED one — <c>obj/x64/{Config}/net9.0-windows10.0.26100.0/win-x64/App.g.i.cs</c>,
/// a <c>Program.Main</c> emitted inside <c>#if !DISABLE_XAML_GENERATED_MAIN</c>. Velopack's
/// hooks (<c>--veloapp-install</c>, <c>--veloapp-updated</c>, <c>--veloapp-obsolete</c>,
/// <c>--veloapp-uninstall</c>) run inside THIS process and are allowed to exit it, so
/// <c>VelopackApp.Build().Run()</c> must execute before ANY app initialisation — before WinRT
/// COM init, before <c>Application.Start</c>. The generated Main offers no seam in front of
/// itself, so the .csproj defines <c>DISABLE_XAML_GENERATED_MAIN</c> and this file takes over.</para>
///
/// <para><b>The body below is a VERBATIM copy of the generated one</b>, read out of
/// App.g.i.cs at the time this file was written rather than recalled — the WinRT/COM
/// initialisation order is load-bearing and a paraphrase is a bug waiting for a
/// toolchain bump. Measured on this tree (WindowsAppSDK 1.8.*, XAML compiler 3.0.0.2607,
/// net9.0-windows10.0.26100.0): the generated Main is exactly
/// <c>InitializeComWrappers()</c> then <c>Application.Start(…)</c> — note in particular that
/// it does NOT call <c>XamlCheckProcessRequirements()</c>, because this project is
/// <c>WindowsAppSDKSelfContained</c> and needs no bootstrapper. Debug and Release generate
/// byte-identical Mains; both were compared before this replaced them.</para>
///
/// <para><b>If a future SDK bump changes the generated shape</b>, diff this against the
/// then-current App.g.i.cs (it is still emitted, just compiled out by the constant) and
/// port the difference. Deleting the constant would silently reinstate the generated Main
/// and, with it, an app whose first statement is no longer Velopack's — an installer that
/// hangs at first run with no error.</para>
/// </summary>
public static class Program
{
    [STAThread]
    private static void Main(string[] args)
    {
        // ── MUST BE FIRST. Velopack's install/update/uninstall hooks may exit the
        //    process here; anything above this line runs during an install and is,
        //    at best, wasted work and, at worst, an engine created inside a hook.
        //    This is a no-op for an ordinary launch of an ordinary build.
        VelopackApp.Build().Run();

        // ── Everything below: the generated Main, verbatim. Do not "tidy" it.
        global::WinRT.ComWrappersSupport.InitializeComWrappers();
        global::Microsoft.UI.Xaml.Application.Start((p) =>
        {
            var context = new global::Microsoft.UI.Dispatching.DispatcherQueueSynchronizationContext(
                global::Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread());
            global::System.Threading.SynchronizationContext.SetSynchronizationContext(context);
            new App();
        });
    }
}
