using System.Runtime.InteropServices;

namespace Rudis.Shell.Interop;

/// <summary>
/// Header: <c>struct RudisPreviewRect { int32_t x; int32_t y; uint32_t width;
/// uint32_t height; }</c> (#[repr(C)], crates/ffi/src/buffer.rs). Blittable —
/// layout pinned by the Rust-side compile-time canaries (size 16, align 4,
/// offsets 0/4/8/12) AND re-proven from managed code by
/// <c>InteropTests.preview_rect_layout_matches_the_rust_canary</c>.
///
/// <para>The frame-content sub-rect inside the attached <c>SwapChainPanel</c> —
/// contain-fit, EXCLUDING the letterbox bars — in PHYSICAL pixels relative to
/// the panel's own origin. Filled by <c>rudis_preview_content_rect</c>, which
/// is four relaxed atomic loads: no allocation, no lock, callable from any
/// thread (the same hot-path shape as <c>rudis_get_playback_position</c>).</para>
///
/// <para>This is the ONE source of truth for the Canvas region's pointer
/// normalization. The engine computes it via <c>engine::contain_fit_viewport</c>
/// on every composite — the same formula the compositor letterboxes with — so
/// the ink overlay's geometry and the drawn picture cannot drift apart. Do NOT
/// re-derive contain-fit math in C# (D-12: two models of one truth).</para>
///
/// <para><see cref="X"/>/<see cref="Y"/> are SIGNED and
/// <see cref="Width"/>/<see cref="Height"/> unsigned, exactly as the header
/// declares them; the width/height pair is <c>0</c> until the first composite
/// has actually happened, which is the caller's cue that there is nothing to
/// align to yet.</para>
/// </summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RudisPreviewRect
{
    public int X;
    public int Y;
    public uint Width;
    public uint Height;
}
