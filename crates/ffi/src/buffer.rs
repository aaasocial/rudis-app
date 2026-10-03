//! The ABI's fixed-layout types, plus the owned-`Vec<u8>` construct idiom.
//!
//! RESEARCH B7 confirmed there is **zero** `#[repr(C)]` anywhere in shipped
//! crates today — S6's JSON-envelope decision deliberately leaves almost no
//! fixed-layout surface. [`RudisBuffer`], [`RudisStatus`] and (since Phase 51)
//! [`RudisPreviewRect`] are the whole of it, which is why the SC-3 layout
//! canary (compile-time `const` asserts here, golden-byte integration test in
//! `tests/layout_canary.rs`) can be exhaustive.
//!
//! The construct/free idiom is RESEARCH B5's stable-Rust shape:
//! `mem::forget` after capturing `(ptr, len, cap)` — `Vec::into_raw_parts` is
//! nightly-only, and `into_boxed_slice` would force `cap == len` at the cost of
//! a potential reallocation. The paired free (`rudis_free_buffer`) lives in
//! `lib.rs` with the other exports, guarded and registered like every export.

/// Owned byte buffer handed across the ABI. Contract (carried into the
/// cbindgen header via these doc comments):
/// - freed ONLY by `rudis_free_buffer` (same allocator; a .NET free is heap corruption)
/// - treat as opaque/read-only on the managed side; a tampered len/cap is UB on free
/// - `rudis_free_buffer` on an all-zero struct is a safe no-op
#[repr(C)]
pub struct RudisBuffer {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

/// Transport-fault status (D-06). Domain errors NEVER appear here — they stay
/// inside the JSON envelope as {"Err": "..."} exactly as today's Result<T, String>.
///
/// The `-5..=-9` block (Phase 51, D-05/D-06/D-07) exists because the four
/// panel exports carry NO envelope: `rudis_preview_attach_panel` and friends
/// return a bare status, so a QueryInterface failure, a wrong-thread call or a
/// failed surface creation has nowhere else to be reported. They are genuine
/// TRANSPORT/precondition faults — environment and caller-contract problems,
/// not user-data validation — so widening this enum is the right home rather
/// than overloading `InvalidHandle`/`AllocationFailed` with unrelated meanings.
///
/// ⚠ APPEND ONLY. A shipped C# host compares raw `i32`s off the wire
/// (`shell/Rudis.Shell/Interop/RudisStatus.cs`); renumbering an existing
/// variant would still compile on both sides and silently mis-diagnose every
/// fault. The per-variant compile-time asserts below make a reorder a BUILD
/// error, and `tests/layout_canary.rs` + `InteropTests.status_vocabulary_
/// mirrors_the_rust_enum` re-prove it at runtime in both languages.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RudisStatus {
    Ok = 0,
    NullPointer = -1,
    InvalidUtf8 = -2,
    AllocationFailed = -3,
    InvalidHandle = -4,
    /// D-06: the COM pointer handed to `rudis_preview_attach_panel` is not an
    /// `ISwapChainPanelNative` (`QueryInterface` returned `E_NOINTERFACE`, or a
    /// null interface). `wgpu-hal` TRANSMUTES this pointer rather than QI-ing
    /// it (`wgpu-hal-26.0.6/src/dx12/mod.rs:511`), so passing it through
    /// unchecked would be undefined behaviour rather than an error return.
    /// This is the check that turns a caller mistake into a diagnosis.
    NotASwapChainPanel = -5,
    /// D-07: a panel-affine call arrived from a thread other than the one that
    /// attached. `ISwapChainPanelNative::SetSwapChain` returns
    /// `RPC_E_WRONG_THREAD` off the panel's own UI thread; this is the named
    /// failure, never a hang.
    WrongThread = -6,
    /// Surface/adapter/device creation or the first `configure` failed. The
    /// full HRESULT / wgpu error text is written to stderr; this status says
    /// WHICH stage failed.
    SurfaceCreateFailed = -7,
    /// A resize/detach/content-rect call arrived with no panel attached.
    NotAttached = -8,
    /// `rudis_preview_attach_panel` called while a panel is already attached.
    /// Detach first; re-attaching over a live surface is not a supported
    /// transition.
    AlreadyAttached = -9,
    PanicCaught = -99, // matches the Phase-44 spike's RC_PANIC convention
}

/// The frame-content sub-rect inside the attached panel — contain-fit,
/// EXCLUDING the letterbox bars — in PHYSICAL pixels relative to the panel's
/// own origin.
///
/// This is the capability the retired `emit_canvas_viewport` pushed as an
/// event; here it is PULLED on demand (`rudis_preview_content_rect`), which is
/// the hot-scalar-poll idiom this ABI already uses for
/// `rudis_get_playback_position`. It is computed in exactly ONE place —
/// `ShellPresentSink`'s composite paths, via `engine::contain_fit_viewport` —
/// so the pointer-normalization the C# Canvas does and the letterbox the
/// compositor draws can never drift apart.
///
/// `x`/`y` are SIGNED: the rect is panel-relative and a future non-origin
/// content placement (or a rounding step) may legitimately produce a negative
/// offset. `width`/`height` are unsigned and are `0` before the first
/// composite.
#[repr(C)]
pub struct RudisPreviewRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Hand ownership of a `Vec<u8>` to the caller. The returned triple is exactly
/// `(as_mut_ptr, len, capacity)` captured from a real `Vec` before
/// `mem::forget`, so an empty `Vec`'s dangling-but-non-null pointer is handled
/// correctly by construction. Reclaimed exclusively by `rudis_free_buffer`'s
/// `Vec::from_raw_parts`.
pub(crate) fn vec_to_buffer(mut v: Vec<u8>) -> RudisBuffer {
    let buf = RudisBuffer {
        ptr: v.as_mut_ptr(),
        len: v.len(),
        cap: v.capacity(),
    };
    std::mem::forget(v);
    buf
}

// ---------------------------------------------------------------------------
// Compile-time layout canaries (SC-3's first half — these run before any test
// does; the golden-byte runtime half lives in tests/layout_canary.rs).
// Same pattern as the Phase-44 spike's FFmpeg-struct mirrors, reimplemented —
// spike code never ships. `std::mem::offset_of!` is stable on the pinned
// rustc 1.96.1 (RESEARCH B7); no memoffset crate.
// ---------------------------------------------------------------------------
const _: () = assert!(std::mem::size_of::<RudisBuffer>() == 24);
const _: () = assert!(std::mem::align_of::<RudisBuffer>() == 8);
const _: () = assert!(std::mem::offset_of!(RudisBuffer, ptr) == 0);
const _: () = assert!(std::mem::offset_of!(RudisBuffer, len) == 8);
const _: () = assert!(std::mem::offset_of!(RudisBuffer, cap) == 16);
const _: () = assert!(std::mem::size_of::<RudisStatus>() == 4);

// Every discriminant pinned individually (Phase 51). A future reorder or an
// accidental renumbering is then a COMPILE error on this crate, not a
// runtime mis-diagnosis in a shipped managed host. `Ok`'s zero is included
// deliberately: `Ok = 0` is the one value C# code branches on by identity.
const _: () = assert!(RudisStatus::Ok as i32 == 0);
const _: () = assert!(RudisStatus::NullPointer as i32 == -1);
const _: () = assert!(RudisStatus::InvalidUtf8 as i32 == -2);
const _: () = assert!(RudisStatus::AllocationFailed as i32 == -3);
const _: () = assert!(RudisStatus::InvalidHandle as i32 == -4);
const _: () = assert!(RudisStatus::NotASwapChainPanel as i32 == -5);
const _: () = assert!(RudisStatus::WrongThread as i32 == -6);
const _: () = assert!(RudisStatus::SurfaceCreateFailed as i32 == -7);
const _: () = assert!(RudisStatus::NotAttached as i32 == -8);
const _: () = assert!(RudisStatus::AlreadyAttached as i32 == -9);
const _: () = assert!(RudisStatus::PanicCaught as i32 == -99);

// The second fixed-layout type (Phase 51, SHELL-04). Same exhaustive shape as
// `RudisBuffer` above: size, align and EVERY field offset, so a field reorder
// or a type widening cannot reach the managed mirror unnoticed.
const _: () = assert!(std::mem::size_of::<RudisPreviewRect>() == 16);
const _: () = assert!(std::mem::align_of::<RudisPreviewRect>() == 4);
const _: () = assert!(std::mem::offset_of!(RudisPreviewRect, x) == 0);
const _: () = assert!(std::mem::offset_of!(RudisPreviewRect, y) == 4);
const _: () = assert!(std::mem::offset_of!(RudisPreviewRect, width) == 8);
const _: () = assert!(std::mem::offset_of!(RudisPreviewRect, height) == 12);
