//! **The one composition-scale compensation in this repository.**
//!
//! A `Microsoft.UI.Xaml.Controls.SwapChainPanel` composites its swapchain at
//! **one buffer pixel per DIP**, anchored at the panel's top-left. Both of
//! Rudis's panel-backed surfaces deliberately size their buffer in PHYSICAL
//! pixels, so the renderer draws at the display's native resolution instead of
//! being upscaled by DWM. On a display at scale `s` that makes the buffer `s`x
//! larger than the panel's DIP box — so everything drawn appears magnified by
//! `s` about the top-left, with the right/bottom `(1 - 1/s)` of it clipped away.
//!
//! The platform's fix is a single inverse-scale matrix on the swapchain, and
//! there is no XAML-side equivalent: it must be
//! `IDXGISwapChain2::SetMatrixTransform`. `wgpu-hal` does not set one — it calls
//! `ISwapChainPanelNative::SetSwapChain` and nothing else — so the host must.
//!
//! # THIS MECHANISM WAS WRITTEN TWICE AND MISSED ONCE
//!
//! **If you are adding a third `SwapChainPanel` surface, call this crate.**
//!
//! Plan 51-04 wrote it for the Preview (`crates/ffi/src/panel/surface.rs`) after
//! finding the consequence of its absence by opening a committed screenshot:
//! *"the video ran 1.25x oversized and cropped WHILE EVERY ASSERTION PASSED"*
//! (`PROVENANCE.md` entry 29b). Phase 52 then built a second panel surface
//! (`crates/timeline-render`) without it, and the identical bug shipped ten
//! plans deep — surfacing as three separate user-visible defects (D1 selection,
//! D2 trim, D3 playhead) because the pointer path stayed correct while the
//! pixels moved, so hit test and pixels disagreed by a MULTIPLICATIVE 1.25.
//! Plan 52-12 promoted the mechanism here so a third surface cannot repeat it.
//!
//! # The measurement, so the failure is recognisable rather than abstract
//!
//! 51-04, on this machine at 125%: a 3214x838 buffer in a 2571x670 DIP panel put
//! the contain-fit frame at physical `x=1077..2939` instead of `x=862..2351`,
//! with the bottom of the frame off-panel entirely.
//!
//! 52-11, on the Timeline at the same scale: the drawn `TrackHeader` gutter
//! measured 70 physical px where the model says `round(46 x 1.25) = 58`
//! (`46 x s^2 = 71.9`, not `46 x s = 57.5`), and the drawn ruler band started
//! 11 px below the XAML toolbar it must be flush with (`(s-1) x 45 = 11.25`).
//!
//! # THE REJECTED ALTERNATIVE — do not re-open it
//!
//! Sizing the swapchain buffer in DIPs instead is geometrically correct and
//! **wrong**: it renders at logical resolution and lets DWM upscale, which is a
//! permanent sharpness regression on the product's core surfaces. Plan 51-04
//! evaluated and rejected exactly this for the Preview; plan 52-12 restates the
//! rejection here so the next reader does not have to re-derive it.
//!
//! # WHEN TO CALL IT
//!
//! **After EVERY successful `configure`.** The FIRST `configure` is what creates
//! the swapchain, so there is nothing to transform before it; and a DPI change
//! arrives as a *resize* carrying a new scale, so a fix applied only on the
//! attach path leaves the bug alive on every window move between monitors.
//! Setting it twice with the same value is harmless.
//!
//! # WHY THE API TAKES A RAW POINTER
//!
//! `crates/ffi` builds on wgpu 26 / windows-core 0.58; `crates/timeline-render`
//! builds on wgpu 29 / windows 0.62. A shared function taking `&wgpu::Surface`
//! could serve only one of them. A COM interface pointer is a COM interface
//! pointer: `IUnknown::from_raw_borrowed` + `QueryInterface(IDXGISwapChain2)` +
//! `SetMatrixTransform` is correct whichever Rust projection produced it,
//! because the vtable layout is fixed by DXGI and not by the binding. Each
//! caller reaches its own hal swapchain and hands over `.as_raw()`.
//!
//! # FAILURE POSTURE
//!
//! Every entry point returns `Err(String)` rather than panicking, and both
//! callers LOG and CONTINUE. A missing transform is a wrongly-framed picture,
//! never a dead surface — failing an attach over it would trade a visible,
//! diagnosable geometry bug for no surface at all.

use std::ffi::c_void;

/// The composition-scale compensation matrix, as a pure, total function over
/// `scale`: `[_11, _12, _21, _22, _31, _32]` in `DXGI_MATRIX_3X2_F` order.
///
/// Pure on purpose — the interesting rules (reject a scale the driver would
/// choke on; invert rather than apply; translate by nothing, because the panel
/// and the buffer share an origin) are all decidable without a GPU, so they are
/// unit-testable without one.
///
/// A non-finite or non-positive scale is REFUSED rather than clamped to 1.0:
/// silently substituting identity would render the whole picture `scale`x too
/// large with no error anywhere, which is exactly the failure this crate exists
/// to prevent — twice measured, and both times found by eye rather than by a
/// test.
pub fn inverse_scale_matrix(scale: f32) -> Result<[f32; 6], String> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err(format!(
            "refusing a non-finite or non-positive composition scale {scale}"
        ));
    }
    Ok([1.0 / scale, 0.0, 0.0, 1.0 / scale, 0.0, 0.0])
}

/// Apply the inverse-scale matrix to a live DXGI swapchain reached as a raw COM
/// interface pointer.
///
/// # Safety
///
/// `swap_chain` must be null, or a live `IDXGISwapChain*` interface pointer that
/// outlives this call. The pointer is **BORROWED**: this function never releases
/// it and never takes ownership of it, so the caller's own reference is
/// unchanged by a call.
#[cfg(windows)]
pub unsafe fn apply_composition_scale_raw(
    swap_chain: *mut c_void,
    scale: f32,
) -> Result<(), String> {
    use windows::core::Interface as _;
    use windows::Win32::Graphics::Dxgi::{DXGI_MATRIX_3X2_F, IDXGISwapChain2};

    // Validated FIRST, by a pure function, so the rule is unit-testable without
    // a GPU, a panel or a window — and so a bad scale can never reach a driver.
    let m = inverse_scale_matrix(scale)?;

    if swap_chain.is_null() {
        return Err(
            "refusing a null swapchain pointer — there is nothing to transform (configure the \
             surface first; the FIRST configure is what creates the swapchain)"
                .to_string(),
        );
    }

    // SAFETY: non-null by the check above, and live for the duration of the call
    // by this function's safety contract. `from_raw_borrowed` BORROWS — it takes
    // no reference and releases none, so the caller's own reference is untouched.
    let Some(unknown) = (unsafe { windows::core::IUnknown::from_raw_borrowed(&swap_chain) }) else {
        return Err("refusing a null swapchain pointer".to_string());
    };

    // A real `QueryInterface`, never a transmute: the caller reached this pointer
    // through its own wgpu-hal accessor, and a wrong type here would otherwise be
    // a call through the wrong vtable slot — undefined behaviour rather than an
    // error return. `cast` gives `E_NOINTERFACE` (0x80004002) instead.
    let swap_chain2 = unknown.cast::<IDXGISwapChain2>().map_err(|e| {
        format!(
            "cast to IDXGISwapChain2 failed: HRESULT 0x{:08x} ({e}) — the pointer is not a \
             DXGI swapchain",
            e.code().0 as u32
        )
    })?;

    let inverse = DXGI_MATRIX_3X2_F {
        _11: m[0],
        _12: m[1],
        _21: m[2],
        _22: m[3],
        _31: m[4],
        _32: m[5],
    };

    // SAFETY: `inverse` outlives the call; the callee copies it.
    unsafe { swap_chain2.SetMatrixTransform(&inverse) }
        .map_err(|e| format!("SetMatrixTransform({scale}) failed: {e}"))
}

/// Non-Windows twin. `SwapChainPanel` hosting genuinely does not exist off
/// Windows (wgpu's `SurfaceTargetUnsafe::SwapChainPanel` variant is
/// `#[cfg(dx12)]`), so there is no composition scale to compensate and saying so
/// is the honest no-op — the same shape `crates/ffi` uses, so the workspace
/// still builds for the Phase-8 macOS dev target.
#[cfg(not(windows))]
pub unsafe fn apply_composition_scale_raw(
    swap_chain: *mut c_void,
    scale: f32,
) -> Result<(), String> {
    let _ = (swap_chain, scale);
    Ok(())
}

/// Every `wgpu`-naming line inside a `[…dependencies]` table of this crate's own
/// manifest — which is the mechanical form of "this crate is wgpu-version-
/// agnostic BY CONSTRUCTION".
///
/// A bare `grep -c wgpu` over the manifest cannot express that property, because
/// the manifest EXPLAINS at length that `crates/ffi` is on wgpu 26 and
/// `crates/timeline-render` on wgpu 29 — which is the entire reason the API takes
/// a raw pointer. Deleting a rationale to satisfy a substring count is the
/// anti-pattern this repository has already recorded twice (52-02, 52-11); so the
/// check parses the tables instead and the prose stays.
///
/// Exposed as a function rather than inlined into its test so the can-fail
/// companion runs the SAME parser over a fixture.
#[cfg(test)]
fn wgpu_lines_in_dependency_tables(manifest: &str) -> Vec<String> {
    let mut in_dependencies = false;
    let mut offenders = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed.contains("dependencies");
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if in_dependencies && trimmed.contains("wgpu") {
            offenders.push(trimmed.to_string());
        }
    }
    offenders
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The reason this crate can be shared at all, as a build property.**
    ///
    /// Its two consumers are on different `wgpu` majors. The instant a `wgpu`
    /// dependency appears here, the crate can serve exactly one of them and the
    /// duplication that produced D1-D3 comes straight back. The API's own shape
    /// is already enforced by the compiler — with no `wgpu` in the graph, a
    /// `wgpu` type could not be named in a signature even by accident — and this
    /// test pins the manifest half.
    #[test]
    fn the_manifest_declares_no_wgpu_dependency() {
        let offenders = wgpu_lines_in_dependency_tables(include_str!("../Cargo.toml"));
        assert!(
            offenders.is_empty(),
            "composition-scale must depend on NO wgpu major — it is consumed by crates/ffi \
             (wgpu 26) AND crates/timeline-render (wgpu 29), and a dependency on either would \
             make it unshareable. Offending dependency line(s): {offenders:?}"
        );
    }

    /// The companion. Without it, "no offenders" and "the parser matched nothing"
    /// are the same reading — the exact ambiguity 52-11 recorded three times in
    /// one plan.
    #[test]
    fn the_wgpu_dependency_scan_can_observe_a_violation() {
        const FIXTURE: &str = "\
[package]\n\
name = \"composition-scale\"\n\
description = \"mentions wgpu in prose, which must NOT count\"\n\
\n\
[target.'cfg(windows)'.dependencies]\n\
# a comment naming wgpu-hal, which must NOT count either\n\
windows = { version = \"=0.62.2\" }\n\
wgpu = \"=29.0.1\"\n";
        let offenders = wgpu_lines_in_dependency_tables(FIXTURE);
        assert_eq!(
            offenders.len(),
            1,
            "the scan must flag the real dependency line and ignore both the description and \
             the comment, got: {offenders:?}"
        );
        assert!(offenders[0].starts_with("wgpu ="));
    }

    /// The compensation is an INVERSE, and getting that backwards is the
    /// difference between a picture 1.25x too large and one 1.25x too small —
    /// both plausible-looking, neither correct. Pinned at the 125% this machine
    /// actually runs, plus identity and a doubled scale.
    ///
    /// Moved VERBATIM from `crates/ffi/src/panel/surface.rs` (plan 51-04) when
    /// the mechanism was promoted here; the assertions are unchanged so the
    /// promotion is provably behaviour-preserving.
    #[test]
    fn inverse_scale_matrix_inverts_and_never_translates() {
        let m = inverse_scale_matrix(1.25).expect("1.25 is a real composition scale");
        assert_eq!(m[0], 0.8, "_11 must be 1/scale, not scale");
        assert_eq!(m[3], 0.8, "_22 must be 1/scale, not scale");
        // No shear, no translation: the panel and the swapchain buffer share an
        // origin, so anything non-zero here would OFFSET the picture.
        assert_eq!([m[1], m[2], m[4], m[5]], [0.0, 0.0, 0.0, 0.0]);

        let identity = inverse_scale_matrix(1.0).expect("1.0 is a real composition scale");
        assert_eq!(identity, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

        let two = inverse_scale_matrix(2.0).expect("200% is a real composition scale");
        assert_eq!([two[0], two[3]], [0.5, 0.5]);
    }

    /// REFUSED, never clamped to identity. A silent identity substitution would
    /// render the whole picture `scale`x too large with no error anywhere —
    /// which is precisely the failure plan 51-04 measured before this function
    /// existed, and it took a screenshot to find. Twice.
    #[test]
    fn inverse_scale_matrix_refuses_a_scale_the_driver_could_not_use() {
        for bad in [0.0f32, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let err = inverse_scale_matrix(bad)
                .expect_err("a non-finite or non-positive scale must be refused");
            assert!(
                err.contains("refusing"),
                "the refusal must SAY it refused, not look like a shrug: {err}"
            );
        }
    }

    /// A null pointer is an early `Err` naming itself, never a dereference.
    #[test]
    fn a_null_swap_chain_pointer_is_rejected() {
        let err = unsafe { apply_composition_scale_raw(std::ptr::null_mut(), 1.25) }
            .expect_err("a null swapchain pointer must be an error, never a dereference");
        assert!(
            err.contains("null"),
            "the rejection must NAME the null pointer so the failure is diagnosable: {err}"
        );
    }

    /// **The QueryInterface, PROVEN against a real vtable rather than asserted.**
    ///
    /// `CreateStreamOnHGlobal` yields a genuine live `IStream*` — a real COM
    /// object with a real vtable that is emphatically not a swapchain. Handing
    /// it in is exactly the shape of caller mistake that a transmute-based
    /// implementation would turn into a call through the wrong vtable slot.
    /// With a real QI the answer is `E_NOINTERFACE` (0x80004002) and a diagnosis.
    ///
    /// Mirrors `crates/ffi`'s `a_real_com_object_that_is_not_a_panel_is_rejected`
    /// so both COM boundaries in this repository are proven the same way.
    #[cfg(windows)]
    #[test]
    fn a_real_com_object_that_is_not_a_swapchain_is_rejected() {
        use windows_sys::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;

        let mut stream: *mut c_void = std::ptr::null_mut();
        // SAFETY: a null HGLOBAL asks the OS to allocate one; `TRUE` makes the
        // stream free it on release; `stream` is a valid out-slot.
        let hr = unsafe { CreateStreamOnHGlobal(std::ptr::null_mut(), 1, &mut stream) };
        assert_eq!(hr, 0, "CreateStreamOnHGlobal must succeed (HRESULT {hr:#x})");
        assert!(!stream.is_null(), "a successful call yields a live IStream*");

        let result = unsafe { apply_composition_scale_raw(stream, 1.25) };

        // Release OUR reference before asserting, so a failed assertion still
        // leaves no leaked COM object behind.
        {
            use windows::core::Interface as _;
            // SAFETY: `stream` is the live interface pointer we just created and
            // have not yet released.
            let unknown = unsafe { windows::core::IUnknown::from_raw(stream) };
            drop(unknown); // IUnknown::Drop == Release
        }

        let err = result.expect_err(
            "an IStream* is not an IDXGISwapChain2 — QueryInterface must reject it rather \
             than letting a transmute call through the wrong vtable slot",
        );
        assert!(
            err.contains("80004002"),
            "the rejection must name E_NOINTERFACE so the failure is diagnosable, got: {err}"
        );
        assert!(
            err.contains("IDXGISwapChain2"),
            "the rejection must name the interface that was missing, got: {err}"
        );
    }

    /// The BORROW contract, stated as a test: a call must not move the caller's
    /// refcount in either direction. An over-release here would be a
    /// use-after-free in the caller (which still holds its own reference and
    /// will release it), and an AddRef that is never given back would pin the
    /// swapchain for the life of the process.
    ///
    /// Measured on a real COM object by reading its refcount through
    /// `AddRef`/`Release` around the call.
    #[cfg(windows)]
    #[test]
    fn the_pointer_is_borrowed_and_the_refcount_is_left_exactly_as_it_arrived() {
        use windows::core::Interface as _;
        use windows_sys::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;

        let mut stream: *mut c_void = std::ptr::null_mut();
        // SAFETY: as above.
        let hr = unsafe { CreateStreamOnHGlobal(std::ptr::null_mut(), 1, &mut stream) };
        assert_eq!(hr, 0, "CreateStreamOnHGlobal must succeed (HRESULT {hr:#x})");

        // SAFETY: `stream` is live and is released exactly once, at the end.
        let owner = unsafe { windows::core::IUnknown::from_raw(stream) };

        // AddRef then Release reads the refcount without changing it.
        let before = {
            let clone = owner.clone();
            let n = unsafe { count_refs(&clone) };
            drop(clone);
            n
        };

        let _ = unsafe { apply_composition_scale_raw(stream, 1.25) };

        let after = {
            let clone = owner.clone();
            let n = unsafe { count_refs(&clone) };
            drop(clone);
            n
        };

        drop(owner);

        assert_eq!(
            before, after,
            "apply_composition_scale_raw BORROWS: a failed QI must not leave a reference \
             behind, and a successful one must give back what it took"
        );
    }

    /// Read a COM object's refcount without changing it: `AddRef` returns the new
    /// count, `Release` gives it straight back.
    ///
    /// # Safety
    /// `obj` must be a live COM interface.
    #[cfg(windows)]
    unsafe fn count_refs(obj: &windows::core::IUnknown) -> u32 {
        use windows::core::Interface as _;
        #[repr(C)]
        struct UnknownVtbl {
            query_interface:
                unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> i32,
            add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
            release: unsafe extern "system" fn(*mut c_void) -> u32,
        }
        let raw = obj.as_raw();
        let vtbl = *(raw as *const *const UnknownVtbl);
        let n = ((*vtbl).add_ref)(raw);
        ((*vtbl).release)(raw);
        n
    }
}
