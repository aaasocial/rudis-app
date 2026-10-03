//! The C ABI — six exports, and the ONE place a raw pointer becomes a slice.
//!
//! # V5 input hygiene (T-52-15, T-52-16, T-52-17)
//!
//! This module mirrors the doctrine `crates/ffi/src/commands.rs` established and states it
//! the same way: **every raw `ptr`/`len` pair is null-checked and length-bounded BEFORE any
//! dereference, and every label is decoded with the CHECKED `str::from_utf8` rather than
//! its unchecked sibling, centralised in [`slice_checked`] and [`label_checked`] so no
//! export can skip validation.**
//!
//! The unchecked decoder's name is deliberately not written anywhere in this file. The
//! acceptance gate for this module is a grep pinning it at zero, and that grep is a
//! USE-detector: prose saying "we do not call it" trips it exactly as a call would, and a
//! gate that reddens on its own rationale gets the rationale deleted rather than the gate
//! fixed (52-02 recorded this twice; this plan hit it twice more).
//!
//! Here that centralisation is enforced by the compiler rather than by convention.
//! [`RudisTimeline::render`](crate::RudisTimeline::render) takes a [`FrameView`], a
//! `FrameView` holds only real Rust slices, and the sole constructor that builds one from
//! raw pointers is [`frame_view`] below. There is no path from a `*const T` to a rendered
//! frame that does not pass through the bound checks — not because everyone remembers to
//! call them, but because nothing else compiles.
//!
//! # WHICH LAYER OWNS WHICH CHECK
//!
//! Stated in both places, so neither is skipped on the assumption the other did it (the
//! same statement appears on `quads::rect_is_drawable`):
//!
//! | Check | Owner | Why there |
//! |---|---|---|
//! | null pointer, length bound, UTF-8 validity | **this module** | memory safety; there is no safe way to observe a bad pointer after the fact, so it must be caught before a slice exists |
//! | non-finite / degenerate `f32` geometry | **`quads.rs`** | a `NaN` width is a drawing problem, not a safety problem; the right answer is a skipped quad in an otherwise complete frame, and rejecting the frame would turn one bad clip into a blank Timeline |
//!
//! # Why every bound is per-ARRAY
//!
//! A Timeline can plausibly show a hundred thousand clips and can never plausibly show a
//! hundred thousand drag ghosts. One shared ceiling would make the loose case's bound the
//! tight case's bound, and a bound that no plausible input approaches is not a bound — it
//! is a comment with a number in it.

use std::os::raw::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::frame::{RudisTimelineFrame, RudisTimelinePalette, RudisTimelineStats};
use crate::{
    FrameView, RudisTimeline, RC_BAD_BUFFER, RC_NULL_HANDLE, RC_OK, RC_PANIC, RC_SURFACE_LOST,
};

/// Maximum clips in one frame. The C# side has already viewport-culled (52-03 §3.6), so a
/// real frame carries tens; this is a ceiling on a hostile or corrupted one, sized to match
/// `TimelineModel.MaxClips`'s own 200,000 order of magnitude without being it.
pub const MAX_CLIPS: u32 = 100_000;
/// Maximum lanes. `LaneModel.MaxLanes` is 512 on the C# side; a Timeline that culls
/// vertically never sends more than a screenful.
pub const MAX_LANES: u32 = 256;
/// Maximum ruler graduations. `RulerTicks.MaxTicks` is 1,024 on the C# side; four times
/// that is comfortable slack and still four orders of magnitude below a DoS.
pub const MAX_TICKS: u32 = 4_096;
/// Maximum drag/trim ghosts. A drag previews ONE clip (D-08 is single primary selection);
/// eight is slack for a future multi-select, not a design allowance.
pub const MAX_GHOSTS: u32 = 8;
/// Maximum snap guides. 52-07 snaps to the playhead and to adjacent clip edges — a handful.
pub const MAX_SNAP_GUIDES: u32 = 64;
/// Maximum label bytes. A clip label is a filename or a display name; `MAX_PATH` is 260 and
/// a long UTF-8 name is a few hundred bytes, so 4 KiB is an order of magnitude of slack
/// over anything real.
pub const MAX_LABEL_BYTES: u32 = 4_096;

/// Turn one `ptr`/`len` pair into a slice, or into a named error code.
///
/// The rules, in the order they are applied — the ORDER is the point:
///
/// 1. `len == 0` ⇒ an empty slice, whatever the pointer is. The LENGTH is authoritative. A
///    non-null pointer beside a zero length is exactly what a partially-updated frame looks
///    like, and reading it would be an out-of-bounds read that happens to succeed.
/// 2. `len > max` ⇒ [`RC_BAD_BUFFER`], **before** anything is built from `len`. This is the
///    check that must come first: `u32::MAX` clips is 8.8 GB of addresses, and a
///    `from_raw_parts` on it is undefined behaviour immediately, not on first use.
/// 3. `ptr.is_null()` with a non-zero length ⇒ [`RC_BAD_BUFFER`].
/// 4. Only then, `from_raw_parts`.
///
/// # Safety
/// The caller promises that a non-null `ptr` addresses at least `len` initialised `T`s that
/// stay valid for the returned lifetime. That is the ABI contract; everything this function
/// can check without trusting the caller, it checks.
pub(crate) unsafe fn slice_checked<'a, T>(
    ptr: *const T,
    len: u32,
    max: u32,
) -> Result<&'a [T], i32> {
    if len == 0 {
        return Ok(&[]);
    }
    if len > max {
        return Err(RC_BAD_BUFFER);
    }
    if ptr.is_null() {
        return Err(RC_BAD_BUFFER);
    }
    Ok(std::slice::from_raw_parts(ptr, len as usize))
}

/// Maximum peak bytes per clip. One byte per 10 ms block, so this is **22.7 hours** of
/// audio — the same ceiling `crates/waveform`'s own cache file cap enforces on the producing
/// side, restated here because a bound that only one side of a boundary knows is a bound one
/// edit away from being absent.
pub const MAX_PEAK_BYTES: u32 = 8 * 1024 * 1024;

/// One clip's peak bytes as a BOUNDED slice, or `None`.
///
/// The peaks reach this crate as a raw pointer into a `byte[]` the C# side pinned, whose
/// CONTENTS came out of a cache file — so they are untrusted twice over, and the same
/// ordered check every other array gets applies here (T-52-15's shape, T-52-37's threat).
/// Unlike a malformed frame array, a malformed peak array does NOT fail the frame: the clip
/// simply draws with no fill, exactly as an unextracted clip does.
///
/// # Safety
/// Same contract as [`slice_checked`]: a non-null `peaks_ptr` addresses at least `peaks_len`
/// bytes that stay valid for the duration of the render call that carried them.
pub fn peaks_checked(clip: &crate::frame::RudisTimelineClip) -> Option<&[u8]> {
    unsafe { slice_checked::<u8>(clip.peaks_ptr, clip.peaks_len, MAX_PEAK_BYTES).ok() }
}

/// Decode a label, or yield `""`.
///
/// A label NEVER fails a frame. Every failure — null, over-long, invalid UTF-8 — renders as
/// an empty label while the clip, lane or tick it belongs to still draws. Losing a user's
/// clip because its filename has a broken byte in it would be a far worse outcome than
/// losing the filename.
///
/// The decode is the CHECKED `str::from_utf8`: the bytes come from another language's heap,
/// and "the C# side always sends valid UTF-8" is an assumption, not a guarantee. (The
/// unchecked variant is not named here — see the module docs for why.)
///
/// # Safety
/// Same contract as [`slice_checked`].
pub(crate) unsafe fn label_checked<'a>(ptr: *const u8, len: u32) -> &'a str {
    match slice_checked::<u8>(ptr, len, MAX_LABEL_BYTES) {
        Ok(bytes) => std::str::from_utf8(bytes).unwrap_or(""),
        Err(_) => "",
    }
}

/// Build a validated [`FrameView`] from a raw frame pointer.
///
/// The ONLY constructor. Every array goes through [`slice_checked`] with its own bound; a
/// single failure fails the whole frame, because a frame whose clip array is unreadable is
/// not a frame with a missing clip — it is a frame whose description cannot be trusted.
///
/// # Safety
/// `f` must be null or a valid `RudisTimelineFrame`.
unsafe fn frame_view<'a>(f: *const RudisTimelineFrame) -> Result<FrameView<'a>, i32> {
    if f.is_null() {
        return Err(RC_BAD_BUFFER);
    }
    let geom = &*f;
    Ok(FrameView {
        geom,
        lanes: slice_checked(geom.lanes_ptr, geom.lanes_len, MAX_LANES)?,
        clips: slice_checked(geom.clips_ptr, geom.clips_len, MAX_CLIPS)?,
        ticks: slice_checked(geom.ticks_ptr, geom.ticks_len, MAX_TICKS)?,
        ghosts: slice_checked(geom.ghost_ptr, geom.ghost_len, MAX_GHOSTS)?,
        snap_guides: slice_checked(geom.snap_guides_ptr, geom.snap_guides_len, MAX_SNAP_GUIDES)?,
    })
}

// ---------------------------------------------------------------------------
// THE SIX EXPORTS. Every body is wrapped in catch_unwind (T-52-17): unwinding
// across an `extern "C"` frame into the CLR is undefined behaviour, not an
// exception the managed side can catch.
// ---------------------------------------------------------------------------

/// Attach to a `SwapChainPanel` and return an opaque handle, or null on any failure.
///
/// **Must be called on the panel's UI thread** — see `surface`'s THREAD RULE.
///
/// Null is the only failure signal here, because the return type is a pointer rather than
/// a code; `rudis_timeline_stats` on a null handle then yields [`RC_NULL_HANDLE`], so the
/// managed side never has to guess which step failed.
///
/// # `scale` IS LOAD-BEARING (plan 52-12)
///
/// It was previously bound to the discard pattern here, under a comment calling it
/// "carried for signature symmetry". It is not symmetry: `w_px`/`h_px` are PHYSICAL
/// pixels, and a `SwapChainPanel` composites one buffer pixel per DIP, so the swapchain
/// needs the matching inverse-scale matrix transform or the whole Timeline draws `scale`x
/// too large while the pointer path stays correct. That was D1, D2 and D3 of the Phase-52
/// UAT. See `surface::apply_composition_scale`.
///
/// # Safety
/// `panel` must be null or a valid `Microsoft.UI.Xaml.Controls.SwapChainPanel` COM pointer.
#[no_mangle]
pub unsafe extern "C" fn rudis_timeline_attach(
    panel: *mut c_void,
    w_px: u32,
    h_px: u32,
    scale: f32,
) -> *mut RudisTimeline {
    catch_unwind(AssertUnwindSafe(|| {
        match RudisTimeline::attach(panel, w_px, h_px, scale) {
            Ok(t) => Box::into_raw(Box::new(t)),
            Err(e) => {
                crate::trace(&format!("attach:failed err={e:?}"));
                std::ptr::null_mut()
            }
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Reconfigure for a new panel size. **UI thread.**
///
/// `scale` is applied, not discarded (plan 52-12) — and this is the half the D1/D2/D3
/// diagnosis specifically flagged: a DPI change (a monitor move, or the user changing
/// the display scale) reaches this crate as a RESIZE carrying a NEW composition scale,
/// never as a re-attach. A fix on the attach path alone would leave the bug alive on
/// every one of them.
///
/// # Safety
/// `t` must be null or a handle from [`rudis_timeline_attach`] that has not been detached.
#[no_mangle]
pub unsafe extern "C" fn rudis_timeline_resize(
    t: *mut RudisTimeline,
    w_px: u32,
    h_px: u32,
    scale: f32,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(t) = t.as_mut() else {
            return RC_NULL_HANDLE;
        };
        t.resize(w_px, h_px, scale);
        RC_OK
    }))
    .unwrap_or(RC_PANIC)
}

/// Upload the palette the C# side resolved from `Theme/Tokens.xaml`.
///
/// # Safety
/// `t` must be null or a live handle; `p` must be null or a valid palette.
#[no_mangle]
pub unsafe extern "C" fn rudis_timeline_set_palette(
    t: *mut RudisTimeline,
    p: *const RudisTimelinePalette,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(t) = t.as_mut() else {
            return RC_NULL_HANDLE;
        };
        let Some(p) = p.as_ref() else {
            return RC_BAD_BUFFER;
        };
        t.set_palette(p);
        RC_OK
    }))
    .unwrap_or(RC_PANIC)
}

/// Render one frame from one flat description.
///
/// # Ordering, which is behaviour and not an optimisation
///
/// The dirty gate runs **before** validation, not only before presentation. An idle
/// Timeline ticking at display rate should cost a comparison, not a walk of five arrays —
/// and `a_clean_frame_is_refused_before_its_arrays_are_even_looked_at` pins that ordering
/// by handing in a frame that is both clean and malformed and requiring [`RC_OK`].
///
/// # Safety
/// `t` must be null or a live handle; `f` must be null or a valid frame whose pointers
/// address at least their stated lengths.
#[no_mangle]
pub unsafe extern "C" fn rudis_timeline_render(
    t: *mut RudisTimeline,
    f: *const RudisTimelineFrame,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(t) = t.as_mut() else {
            return RC_NULL_HANDLE;
        };
        if f.is_null() {
            return RC_BAD_BUFFER;
        }
        // THE DIRTY GATE, first. See the doc block above.
        if (*f).dirty == 0 {
            t.note_clean_frame();
            return RC_OK;
        }
        let view = match frame_view(f) {
            Ok(v) => v,
            Err(code) => return code,
        };
        t.render(&view)
    }))
    .unwrap_or(RC_PANIC)
}

/// Copy the counters out.
///
/// # Safety
/// `t` must be null or a live handle; `out` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn rudis_timeline_stats(
    t: *mut RudisTimeline,
    out: *mut RudisTimelineStats,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(t) = t.as_ref() else {
            return RC_NULL_HANDLE;
        };
        if out.is_null() {
            return RC_BAD_BUFFER;
        }
        *out = t.stats();
        RC_OK
    }))
    .unwrap_or(RC_PANIC)
}

/// Destroy the handle **and unbind the panel** — the whole teardown sequence, in order.
///
/// # THE TEARDOWN ORDER — LEARNED BY CRASHING, TWICE (52-01)
///
/// Detaching a panel-backed surface is a four-step ordered sequence and two of the three
/// plausible orderings crash on real hardware:
///
/// ```text
/// 1. stop and JOIN the present thread            (the CALLER's job — nothing may be
///                                                 presenting when this is called)
/// 2. ISwapChainPanelNative::SetSwapChain(null)   (here, ON THE CALLING THREAD, WHILE the
///                                                 wgpu objects are still alive)
/// 3. drop the Surface/Device/Queue/Instance      (here)
/// 4. release the QueryInterface reference        (here)
/// ```
///
/// Step 2 must precede step 3: with the final `wgpu::Instance` already dropped, the DXGI
/// factory the panel's unbind path depends on is gone with it, and `SetSwapChain`
/// access-violates. SKIPPING step 2 is worse and quieter — `wgpu-hal` calls
/// `SetSwapChain` in exactly one place (`configure`) and never with null, so the XAML
/// panel keeps a swapchain whose device is dead, and the process dies with `0xC0000005`
/// inside `D3D12Core.dll` at exit, long after everything appeared to work.
///
/// # Why steps 2 and 4 are HERE and not the caller's
///
/// They were the caller's when this export was written (plan 52-04), matching
/// `unbind_swap_chain_panel`'s own documentation — but the ABI is deliberately SIX
/// exports and none of them hands the managed side either the `ISwapChainPanelNative`
/// pointer or a way to call through it. So "the caller's job" was a job the caller could
/// not do, and the only alternative would have been a seventh export plus a second COM
/// vtable declaration in C#. Plan 52-06 moved both steps in here instead. The one thing
/// that changes is the THREAD requirement, and it is stated below rather than implied.
///
/// This is also exactly what `smoke::SmokeHandle`'s own `Drop` already does in this
/// crate, so the shipping path and the tripwire path now tear down the same way.
///
/// # Threading
///
/// **Call on the panel's UI thread.** Step 2 reaches a COM call with thread affinity;
/// off-thread it returns `RPC_E_WRONG_THREAD`, the panel keeps its dead swapchain, and
/// the failure is the quiet one above. The managed side calls this from
/// `AppWindow.Closing` — **never** `Window.Closed`, which fires while the window is
/// already being destroyed. A finalizer-driven release is a LEAK BACKSTOP only.
///
/// A headless renderer has no panel; both COM steps are null no-ops for it.
///
/// # Safety
/// `t` must be null or a live handle, and must not be used again afterwards. Calling twice
/// with the same non-null pointer is a double free; a `SafeHandle` that nulls on release
/// (which is what the managed side uses) makes the second call a clean [`RC_NULL_HANDLE`].
#[no_mangle]
pub unsafe extern "C" fn rudis_timeline_detach(t: *mut RudisTimeline) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if t.is_null() {
            return RC_NULL_HANDLE;
        }

        let renderer = Box::from_raw(t);

        // The pointer is COPIED out before the box is dropped, so step 4 still has it
        // after step 3 has taken the surface away. `TimelineSurface` deliberately has no
        // `Drop` for it, so this is the only release and it happens exactly once.
        let panel_native = renderer
            .surface()
            .map_or(std::ptr::null_mut(), |s| s.panel_native());

        crate::trace("detach:unbind-panel");
        crate::surface::unbind_swap_chain_panel(panel_native);

        crate::trace("detach:drop-renderer");
        drop(renderer);

        crate::surface::release_panel_native(panel_native);
        crate::trace("detach:done");
        RC_OK
    }))
    .unwrap_or(RC_PANIC)
}

/// Keeps [`RC_SURFACE_LOST`] referenced from this module's own documentation surface.
///
/// `-3` is produced inside `RudisTimeline::render`, which classifies the wgpu outcome, and
/// is listed here so the six-code table in `lib.rs` has a definition site in the module
/// that owns the codes rather than only a mention.
pub const SURFACE_LOST: i32 = RC_SURFACE_LOST;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_length_yields_an_empty_slice_whatever_the_pointer_is() {
        let data = [1u32, 2, 3];
        // The length is authoritative in BOTH directions: a real pointer with a zero
        // length reads nothing, and a null pointer with a zero length is not an error.
        let s = unsafe { slice_checked(data.as_ptr(), 0, 10) }.unwrap();
        assert!(s.is_empty());
        let s = unsafe { slice_checked(std::ptr::null::<u32>(), 0, 10) }.unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn the_bound_is_checked_before_the_null_check() {
        // Both are errors, so the ORDER is invisible in the return value — which is
        // exactly why it needs asserting some other way. An absurd length paired with a
        // NON-null pointer is the dangerous combination: if the null check ran first it
        // would pass, and from_raw_parts would then be reached with u32::MAX.
        let data = [1u32];
        assert_eq!(
            unsafe { slice_checked(data.as_ptr(), u32::MAX, 10) },
            Err(RC_BAD_BUFFER)
        );
    }

    #[test]
    fn exactly_the_bound_is_allowed_and_one_more_is_not() {
        let data = [0u8; 4];
        assert!(unsafe { slice_checked(data.as_ptr(), 4, 4) }.is_ok());
        assert_eq!(
            unsafe { slice_checked(data.as_ptr(), 5, 4) },
            Err(RC_BAD_BUFFER)
        );
    }

    #[test]
    fn a_null_pointer_with_a_real_length_is_rejected() {
        assert_eq!(
            unsafe { slice_checked(std::ptr::null::<u32>(), 1, 10) },
            Err(RC_BAD_BUFFER)
        );
    }

    #[test]
    fn every_label_failure_is_the_empty_string_and_never_a_panic() {
        assert_eq!(unsafe { label_checked(std::ptr::null(), 0) }, "");
        assert_eq!(unsafe { label_checked(std::ptr::null(), 12) }, "");

        let good = b"Beach_01.mp4";
        assert_eq!(
            unsafe { label_checked(good.as_ptr(), good.len() as u32) },
            "Beach_01.mp4"
        );

        // A lone continuation byte — not valid UTF-8 anywhere.
        let bad = [0x80u8, 0x81, 0x82];
        assert_eq!(unsafe { label_checked(bad.as_ptr(), 3) }, "");

        // A length past the crate's own cap.
        assert_eq!(
            unsafe { label_checked(good.as_ptr(), MAX_LABEL_BYTES + 1) },
            ""
        );
    }

    #[test]
    fn a_truncated_multibyte_sequence_is_empty_not_partially_decoded() {
        // The realistic corruption: a length computed in UTF-16 code units against UTF-8
        // bytes cuts a multi-byte character in half. Decoding "as much as parses" would
        // silently truncate a user's filename mid-character.
        let text = "Sønderborg_øst.mov".as_bytes();
        let cut = 2; // splits the two-byte 'ø'
        assert!(std::str::from_utf8(&text[..cut]).is_err());
        assert_eq!(unsafe { label_checked(text.as_ptr(), cut as u32) }, "");
    }

    #[test]
    fn the_export_surface_is_exactly_six_and_the_test_only_constructors_are_not_in_it() {
        // A MECHANICAL check on this module's own source, in the shape 50-07's
        // `ShellSourceScan` gates use on the C# side. Two claims are made in prose
        // elsewhere in this crate and both need to be more than prose:
        //
        //   1. The ABI is SIX exports. `crates/ffi`'s own export-table test exists because
        //      "the surface did not widen" is exactly the kind of thing that quietly stops
        //      being true, and the Timeline's surface is a second front door into the same
        //      process.
        //   2. The headless constructor and the panic hook on `RudisTimeline` are
        //      TEST-ONLY. They are `pub` because integration tests can only reach the
        //      public API, and the thing that actually keeps them out of the shell is that
        //      no export wraps them. That is a property of THIS file, so this file asserts
        //      it.
        //
        // Every needle below is spelled in two pieces, and the prose above avoids the
        // identifiers entirely. This test scans its own source, so a needle written whole
        // would match itself and the gate would report a violation it had manufactured —
        // which is the same use-detector trap from the other side, and it caught this very
        // test on its first run.
        let src = include_str!("abi.rs");
        assert_eq!(
            src.matches(concat!("#[no_", "mangle]")).count(),
            6,
            "the Timeline C ABI is six exports; widening it is a decision, not a diff"
        );
        assert!(
            !src.contains(concat!("attach_", "headless")),
            "the headless constructor must not be reachable from the C ABI"
        );
        assert!(
            !src.contains(concat!("inject_panic_", "for_test")),
            "the panic hook must not be reachable from the C ABI"
        );
    }

    #[test]
    fn the_bounds_are_ordered_by_how_many_of_each_thing_can_plausibly_exist() {
        assert!(MAX_GHOSTS < MAX_SNAP_GUIDES);
        assert!(MAX_SNAP_GUIDES < MAX_LANES);
        assert!(MAX_LANES < MAX_TICKS);
        assert!(MAX_TICKS < MAX_CLIPS);
        // And every one of them is a real ceiling rather than a formality.
        assert!(MAX_CLIPS < u32::MAX / 2);
    }
}
