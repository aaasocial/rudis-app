using System.Text.Json.Nodes;

namespace Rudis.Shell.Regions;

/// <summary>
/// The exact <c>rudis_transport set_preview_mode</c> request the <c>Preview</c>
/// region's monitor tabs send, and the two mode strings the domain will accept.
///
/// <para><b>Why this had to be written at all.</b> <c>TransportCmd::SetPreviewMode</c>
/// (<c>crates/core/src/transport.rs:89-97</c>) has existed since Phase 4 and had
/// <b>zero</b> call sites anywhere under <c>shell/</c> until plan 52-13: the only
/// textual match in the whole tree was inside a doc comment. Meanwhile every route
/// into the shell that loads a clip — the <c>--import</c> CLI route through
/// <c>ToolbarRegion.ImportPathsAsync(.., loadPreviewOnFirst: true)</c>, and a MediaBin
/// tile double-click — issues <c>load_preview</c>, which sets
/// <c>project.preview_mode = PreviewMode::Source</c> (<c>transport.rs:86</c>). With no
/// way back, the C# shell was pinned in the SOURCE monitor permanently and the Preview
/// could never show the timeline composite at all. That is the root cause of the
/// owner's D4 (<c>.planning/debug/timeline-52-uat-d1-d4.md</c> § Evidence 09:48Z).</para>
///
/// <para><b>What this is a port OF.</b> v6.0's web frontend had the pair of tabs this
/// restores — <c>frontend/src/main.ts:717-742</c>, <c>#tab-program</c> /
/// <c>#tab-source</c>, each dispatching exactly this command. This is v6 PARITY, not a
/// design-handoff element: the handoff's <c>Preview</c> contents list no monitor tabs.
/// Said here as well as beside the XAML, because a control invented without a stated
/// source is how an unrecorded deviation is born.</para>
///
/// <para><b>The <c>"cmd"</c> wrapper is not decoration.</b> Shape read from the
/// producer, not guessed: <c>rudis_transport</c> takes
/// <c>TransportArgs { cmd: TransportCmd }</c> (<c>commands.rs:274-276</c>) and
/// <c>TransportCmd</c> is ADJACENTLY tagged (<c>{"type": .., "data": {..}}</c>,
/// <c>transport.rs:11-12,21</c>). Passing the bare command object deserializes to
/// nothing and the call fails at the domain layer with no compile error and no type
/// mismatch — the trap plan 50-05 recorded as its deviation 1, and the reason
/// <c>Transport.SendAsync</c> and <c>MediaBinPreviewCommand</c> both spell it out at
/// their own call sites.</para>
///
/// <para>WinUI-free by construction (D-13), so the whole thing is decidable with no
/// window and <c>PreviewMonitorTests</c> can round-trip its output through a REAL
/// <c>rudis_transport</c> headlessly.</para>
/// </summary>
internal static class PreviewMonitorCommand
{
    /// <summary>The Timeline monitor. <c>PreviewMode</c> serialises
    /// <c>rename_all = "snake_case"</c>, so the wire spelling is lower-case.</summary>
    internal const string Program = "program";

    /// <summary>The MediaBin-clip monitor, which <c>load_preview</c> selects.</summary>
    internal const string Source = "source";

    /// <summary>
    /// True only for the two variants <c>PreviewMode</c> actually has.
    ///
    /// <para>T-52-61: a builder that can emit a third mode is a builder that can emit a
    /// domain error nobody reads — serde would refuse it, the transport call would come
    /// back as a domain <c>Err</c>, and the region would surface a message about JSON
    /// to a user who pressed a tab. Refusal belongs here, before a call is made.</para>
    /// </summary>
    internal static bool IsKnownMode(string? mode) => mode is Program or Source;

    /// <summary>
    /// <c>{"cmd":{"type":"set_preview_mode","data":{"mode":"&lt;mode&gt;"}}}</c>.
    ///
    /// <para>On success <c>rudis_transport</c> answers the ACTIVE tab's <c>Playback</c>
    /// envelope — for Program that is <c>project.playback</c> with <c>duration_us</c>
    /// freshly synced to <c>timeline.duration_us()</c> (<c>transport.rs:89-97</c>) — so
    /// the caller can apply it immediately rather than wait a poll interval.</para>
    /// </summary>
    /// <exception cref="ArgumentOutOfRangeException">The mode is not one of the two
    /// <c>PreviewMode</c> variants.</exception>
    internal static string SetMode(string mode)
    {
        if (!IsKnownMode(mode))
        {
            throw new ArgumentOutOfRangeException(
                nameof(mode),
                mode,
                "PreviewMode has exactly two variants, `program` and `source` " +
                "(crates/core/src/transport.rs:89-97). Sending anything else would reach the " +
                "domain only to be refused by serde, and the refusal would surface to a user " +
                "who pressed a tab.");
        }

        return new JsonObject
        {
            ["cmd"] = new JsonObject
            {
                ["type"] = "set_preview_mode",
                ["data"] = new JsonObject { ["mode"] = mode },
            },
        }.ToJsonString();
    }
}
