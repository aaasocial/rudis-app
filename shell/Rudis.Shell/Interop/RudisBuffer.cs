using System.Runtime.InteropServices;

namespace Rudis.Shell.Interop;

/// <summary>
/// Header: <c>struct RudisBuffer { uint8_t *ptr; uintptr_t len; uintptr_t cap; }</c>
/// (#[repr(C)], crates/ffi/src/buffer.rs:22-26). Blittable — <c>nint</c>/<c>nuint</c>
/// match <c>uint8_t*</c>/<c>uintptr_t</c> on this x64-only build (layout pinned by
/// the Rust-side compile-time canaries: size 24, offsets 0/8/16).
///
/// Ownership is unforgiving (RESEARCH §2): the triple is <c>(as_mut_ptr, len,
/// capacity)</c> captured from a real <c>Vec</c> before <c>mem::forget</c>, reclaimed
/// EXCLUSIVELY by the native free via <c>Vec::from_raw_parts</c>. On the managed side:
/// <list type="bullet">
/// <item>never free it with any .NET mechanism — Rust's allocator is not this heap
///   (cross-allocator free is heap corruption, T-47-03/T-50-10);</item>
/// <item>never mutate <see cref="Len"/>/<see cref="Cap"/> before handing it back — a
///   changed capacity is UB on the native free;</item>
/// <item>branch on <see cref="Len"/>, never on <see cref="Ptr"/> != 0 — an empty
///   Vec's pointer is dangling-but-non-null BY CONSTRUCTION (V5);</item>
/// <item>an all-zero struct is the documented safe no-op for the free.</item>
/// </list>
/// Every read goes through the ONE chokepoint: <c>RudisNative.NativeMethods.ReadUtf8AndFree</c>
/// (private to <c>RudisNative</c> since CR-01, 50-REVIEW.md — not linkable from here).
/// </summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RudisBuffer
{
    public nint Ptr;
    public nuint Len;
    public nuint Cap;
}
