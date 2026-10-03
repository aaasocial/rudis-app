namespace Rudis.Shell.Regions;

/// <summary>
/// What a dragged MediaBin tile carries, and under what format id.
///
/// <para><b>This is a CONTRACT BETWEEN TWO REGIONS, and only one of them exists.</b>
/// The MediaBin is the drag SOURCE; the Timeline lane that receives the drop is
/// Phase 52's and, as re-verified at this plan's execution time, has no
/// <c>AllowDrop</c>/<c>DragOver</c>/<c>Drop</c> anywhere under
/// <c>Regions/Timeline*</c>. When that half is built it must read
/// <see cref="FormatId"/> from HERE. A second spelling on the far side is a silent
/// no-drop that presents as a drag bug, which is exactly why the string lives in one
/// place and is pinned by a test.</para>
///
/// <para>WinUI-free by rule — the <c>DataPackage</c> that carries this payload is
/// assembled in <c>MediaBin.xaml.cs</c>; what goes IN it is decided here, where it
/// can be asserted without a window (<c>MediaBinPurityGateTests</c>).</para>
/// </summary>
internal static class MediaBinDragPayload
{
    /// <summary>
    /// v6.0's own custom format — the exact string its <c>dragstart</c> hands to
    /// <c>dataTransfer.setData</c> (<c>frontend/src/main.ts:406</c>).
    /// Ported verbatim rather than re-invented: the wire name is part of the parity
    /// surface, and a WinUI-flavoured rename would make the two implementations
    /// gratuitously different for no gain.
    /// </summary>
    internal const string FormatId = "application/x-rudis-media-id";

    /// <summary>
    /// The media id a tile drags, or <c>null</c> when the tile is a FOLDER.
    ///
    /// <para>v6.0 never makes a folder tile draggable — <c>buildFolderTile</c>
    /// (<c>main.ts:419-453</c>) sets no <c>draggable</c> flag and attaches no
    /// <c>dragstart</c> handler at all. A folder is not a clip; there is nothing for
    /// a lane to receive, and starting a drag that can only ever be refused is worse
    /// than not starting one. The caller cancels the drag on <c>null</c>.</para>
    /// </summary>
    internal static string? MediaIdFor(MediaBinTile tile) =>
        tile.Kind == MediaBinTileKind.Media ? tile.Id : null;
}
