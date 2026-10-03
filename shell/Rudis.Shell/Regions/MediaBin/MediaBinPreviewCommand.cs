using System.Text.Json.Nodes;

namespace Rudis.Shell.Regions;

/// <summary>
/// The exact <c>rudis_transport load_preview</c> request a double-clicked video tile
/// sends, and the rule for which tiles send one at all.
///
/// <para>Both are decidable with no window, so both live here and are asserted by
/// <c>MediaBinDragTests</c> — and, critically, <c>MediaBinImportTests</c> drives the
/// REAL engine through <see cref="BuildArgs"/> rather than through a second copy of
/// the args. A test that rebuilt the JSON itself would prove the wire shape and
/// nothing whatever about the region.</para>
///
/// <para>WinUI-free by rule (<c>MediaBinPurityGateTests</c>).</para>
/// </summary>
internal static class MediaBinPreviewCommand
{
    /// <summary>
    /// <c>{"cmd":{"type":"load_preview","data":{"media_id":"&lt;id&gt;"}}}</c>.
    ///
    /// <para><b>The <c>"cmd"</c> wrapper is not decoration.</b> Args shape read from
    /// the producer, not guessed: <c>rudis_transport</c> takes
    /// <c>TransportArgs { cmd: TransportCmd }</c> and <c>TransportCmd</c> is
    /// ADJACENTLY tagged (<c>{"type": .., "data": {..}}</c>), so passing the bare
    /// command object deserializes to nothing and the call fails.
    /// <c>Toolbar.xaml.cs</c> records this exact trap at its own <c>load_preview</c>
    /// call site; this is the same shape, factored out so the MediaBin's copy cannot
    /// drift from it.</para>
    /// </summary>
    internal static string BuildArgs(string mediaId) =>
        new JsonObject
        {
            ["cmd"] = new JsonObject
            {
                ["type"] = "load_preview",
                ["data"] = new JsonObject { ["media_id"] = mediaId },
            },
        }.ToJsonString();

    /// <summary>
    /// v6 parity: the <c>dblclick</c> → preview handler is attached ONLY when
    /// <c>media_kind === "video"</c> (<c>main.ts:391-397</c>). Audio and images stay
    /// selectable and draggable but do not move the Source monitor, and a folder tile
    /// is navigation rather than playback.
    ///
    /// <para>This is a PORT of that condition, not an improvement on it. Loading an
    /// audio item into a monitor whose far half (Phase 51's Preview) has no
    /// audio-only presentation would be inventing behaviour v6.0 does not have, which
    /// <c>REQUIREMENTS.md</c> § Out of Scope forbids during the port.</para>
    /// </summary>
    internal static bool ShouldLoadPreview(MediaBinTile tile) =>
        tile.Kind == MediaBinTileKind.Media && tile.IsVideo;
}
