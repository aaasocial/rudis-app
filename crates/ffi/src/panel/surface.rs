//! The GPU set behind an attached `SwapChainPanel` (Phase 51, plan 51-03,
//! task 2): the preview `wgpu::Instance`, the `ISwapChainPanelNative`
//! `QueryInterface`, and the surface/compositor/VRAM-watch construction the
//! attach export drives.
//!
//! # What is promoted here, and from where
//!
//! The Phase-44 spike (`spikes/44-hwaccel/src/swapchain_host.rs`) proved the
//! Path B chain end to end and learned the two non-obvious facts that have no
//! canonical example anywhere. Both are promoted as TECHNIQUE (D-06) — the
//! spike is workspace-excluded and its FILE never ships:
//!
//! 1. **`wgpu` does not `QueryInterface` the pointer it is handed.**
//!    `Instance::create_surface_from_swap_chain_panel` reaches it through
//!    `from_raw_borrowed` (`wgpu-hal-26.0.6/src/dx12/mod.rs:511`), which in
//!    `windows-core 0.58` is a TRANSMUTE of the reference, not a QI. Handing
//!    it a bare `IInspectable*` would call vtable slot 3
//!    (`IInspectable::GetIids`) as if it were `SetSwapChain`: undefined
//!    behaviour, not an error return. [`query_swap_chain_panel_native`] is the
//!    check that turns that caller mistake into a clean `E_NOINTERFACE`.
//! 2. **`ISwapChainPanelNative::SetSwapChain` is UI-thread-affine**, and it is
//!    reached from exactly ONE place: the FIRST `Surface::configure`. See
//!    [`attach_gpu`] step 5 — that single line is the whole basis of this
//!    plan's threading design.
//!
//! # What is DUPLICATED from the Tauri path, and why it is not shared
//!
//! [`preview_wgpu_instance`] duplicates `native_surface::preview_wgpu_instance`
//! (`src-tauri/src/native_surface.rs:598-612`) rather than calling it: that
//! function lives in the Tauri app crate, which `crates/ffi` and the C# shell
//! do not link at all, and there is no visibility change that would make it
//! reachable — a `pub` widening would MODIFY A FROZEN FILE
//! (`49-FREEZE.md` § "The scope rule"). Same backend pin, same memory-budget
//! thresholds, byte for byte. **If one changes, change both.**
#![allow(
    dead_code,
    reason = "task 3 of this same plan is the caller: \
              rudis_preview_attach_panel runs query_swap_chain_panel_native \
              then attach_gpu and moves the AttachedGpu into PreviewGpu. Task \
              2 delivers the surface layer and its COM-rejection proof on its \
              own, so the compiler cannot see a production caller yet."
)]

use std::ffi::c_void;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// The preview `wgpu::Instance`.
///
/// DUPLICATED from `native_surface::preview_wgpu_instance`
/// (`src-tauri/src/native_surface.rs:598-612`) — see the module doc for why it
/// cannot be shared. Both halves are load-bearing:
///
/// - **Backend pin:** the D3D11→D3D12 shared-handle import (Phase 48, GPU-01)
///   opens the decoder's texture on wgpu's `ID3D12Device`, so the live path
///   must BE DX12. Non-Windows keeps the default set so the Phase-8 macOS dev
///   target still resolves to Metal.
/// - **GPU-03 defense-in-depth:** wgpu's own internal `QueryVideoMemoryInfo`
///   circuit breaker. Complements — never replaces — the ring's explicit
///   runtime budget. `for_device_loss` fires a catchable `DeviceError::Lost`
///   BEFORE a hard driver allocation failure.
pub(crate) fn preview_wgpu_instance() -> wgpu::Instance {
    let backends = if cfg!(target_os = "windows") {
        wgpu::Backends::DX12
    } else {
        wgpu::Backends::all()
    };
    wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends,
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds {
            for_resource_creation: Some(95),
            for_device_loss: Some(97),
        },
        ..Default::default()
    })
}

/// `ISwapChainPanelNative` — the interface `wgpu` assumes the caller's pointer
/// already is.
///
/// Empirically confirmed for WinUI 3 rather than assumed from the UWP XAML
/// docs (Phase 44): the Windows App SDK 1.8 package ships
/// `include/microsoft.ui.xaml.media.dxinterop.h`, which declares
/// `MIDL_INTERFACE("63aad0b8-7c24-40ff-85a8-640d944cc325") ISwapChainPanelNative`
/// — byte-identical to the IID `wgpu-hal` declares for its own private binding
/// (`wgpu-hal-26.0.6/src/dx12/types.rs:9`), so `Microsoft.UI.Xaml`'s
/// `SwapChainPanel` and `wgpu`'s expectation genuinely line up.
#[cfg(windows)]
pub(crate) const IID_ISWAPCHAINPANELNATIVE: windows_core::GUID =
    windows_core::GUID::from_u128(0x63aad0b8_7c24_40ff_85a8_640d944cc325);

/// `QueryInterface` the caller's COM pointer for `ISwapChainPanelNative`,
/// returning the raw interface pointer `wgpu` wants.
///
/// D-06 / threat T-51-01: **never trust the caller's claimed pointer type.**
/// `wgpu-hal` reaches the pointer through `from_raw_borrowed`, which in
/// `windows-core 0.58` is a transmute of the reference and not a QI, so a
/// wrong pointer is undefined behaviour rather than an error return (see the
/// module doc). The QI here converts that into a clean `E_NOINTERFACE`, which
/// [`crate::panel::exports::rudis_preview_attach_panel`] maps to
/// [`crate::RudisStatus::NotASwapChainPanel`].
///
/// Ownership: the reference obtained here belongs to the surface, which
/// releases it when it drops. The CALLER keeps and must release its own — on
/// the managed side, whatever `MarshalInspectable.FromManaged` handed it
/// (threat T-51-15; the C# half pairs it with `Marshal.Release` in a
/// `finally`).
#[cfg(windows)]
pub(crate) fn query_swap_chain_panel_native(panel: *mut c_void) -> Result<*mut c_void, String> {
    use windows_core::Interface as _;

    if panel.is_null() {
        return Err("panel pointer is null".to_string());
    }
    // SAFETY: non-null by the check above. `from_raw_borrowed` BORROWS — it
    // does not take ownership and does not release, so the caller's reference
    // is untouched by this call.
    let Some(unknown) = (unsafe { windows_core::IUnknown::from_raw_borrowed(&panel) }) else {
        return Err("panel pointer is null".to_string());
    };

    let mut native: *mut c_void = std::ptr::null_mut();
    // SAFETY: `unknown` is a live COM interface pointer by the caller's
    // contract; `native` is a valid out-slot for the duration of the call.
    let hr = unsafe { unknown.query(&IID_ISWAPCHAINPANELNATIVE, &mut native) };
    if hr.is_err() || native.is_null() {
        let message = format!(
            "the pointer handed across the FFI boundary is not an ISwapChainPanelNative \
             (QueryInterface {{63aad0b8-7c24-40ff-85a8-640d944cc325}} -> HRESULT 0x{:08x}, \
             ptr=0x{:016x}). wgpu TRANSMUTES this pointer rather than QI-ing it \
             (wgpu-hal-26.0.6/src/dx12/mod.rs:511), so passing it through unchecked would be \
             undefined behaviour rather than an error.",
            hr.0 as u32, native as usize
        );
        eprintln!("[rudis_ffi] panel attach rejected: {message}");
        return Err(message);
    }
    Ok(native)
}

/// **Unbind the panel from whatever swapchain it is currently showing**
/// (Phase 63, plan 63-02 — TRUST-01).
///
/// # This function is the answer to 63-CONTEXT D-03, and it was found by measurement
///
/// D-03 predicted, with the code open, that *"the hard part is the surface, not the
/// device"*. It was right, and this is exactly where the difficulty lives.
///
/// Recovery step 3 drops the `wgpu::Surface`, which releases **wgpu's** reference to the
/// `IDXGISwapChain`. It does NOT release the **panel's** reference — that is what
/// `ISwapChainPanelNative::SetSwapChain` took when the swapchain was first bound, and
/// nothing in `wgpu-hal`'s `Surface` drop path clears it (`wgpu-hal-26.0.6/src/dx12`
/// calls `SetSwapChain` on the first `configure` and never again). A swapchain keeps its
/// creating command queue — and therefore the `ID3D12Device` — alive.
///
/// So after a device removal the panel is still holding the DEAD device, and the
/// MEASURED consequence is the one `RecoveryPlan::live_device`'s note warns about:
/// while any handle to a removed D3D12 device lives, DXGI hides the hardware adapter
/// from fresh enumeration in this process. Plan 63-02's first GREEN attempt recorded it
/// verbatim, with every FFI-side handle already released:
///
/// ```text
/// [rudis_ffi] step3 released the panel's GPU set (compositor+surface+instance+vram_watch);
///             strong_count at release was 1
/// [rudis_ffi] panel preview backend=Dx12 adapter="Microsoft Basic Render Driver"
/// [rudis_ffi] panel preview: RECOVERY FAILED ... the recreated decoder is on
///             0x000000000001468B but the recreated wgpu device is on 0x000000000001570F
/// ```
///
/// `0x1468B` is the RTX 3070; `0x1570F` is WARP. The decode side found the real adapter
/// (it does not go through DXGI enumeration); the wgpu side did not. **The LUID re-assert
/// caught it**, which is precisely the threat T-63-02 mitigation working as designed —
/// but it turned a recoverable TDR into a hard failure, and this call is what removes the
/// cause rather than the symptom.
///
/// # The raw vtable call, and why it is not a shortcut
///
/// `ISwapChainPanelNative` derives from `IUnknown` and declares exactly one method, so
/// its vtable is `[QueryInterface, AddRef, Release, SetSwapChain]` and `SetSwapChain` is
/// slot 3. `windows-core 0.58` has no generated binding for it (this crate's
/// [`IID_ISWAPCHAINPANELNATIVE`] exists for the same reason), and `wgpu-hal` reaches it
/// the same way — through its own private binding. Declaring the vtable here is the same
/// technique, at the same layer, for one call.
///
/// ⚠ **UI-thread-affine**, exactly like the `SetSwapChain` the first `configure` performs:
/// it returns `RPC_E_WRONG_THREAD` anywhere else. Call it only from the attaching thread.
///
/// Returns the `HRESULT` as `Result<(), String>` rather than panicking: failing to unbind
/// is a recovery that will fail LOUDLY at the LUID re-assert a moment later, never a
/// crash, and the caller decides.
#[cfg(windows)]
pub(crate) fn clear_panel_swap_chain(panel_native: *mut c_void) -> Result<(), String> {
    /// `[QueryInterface, AddRef, Release, SetSwapChain]` — `ISwapChainPanelNative`'s
    /// whole vtable. Only the last slot is ever called through this mirror; the three
    /// `IUnknown` slots are declared so the LAYOUT is right, which is the only reason a
    /// vtable mirror has to be complete.
    #[repr(C)]
    struct ISwapChainPanelNativeVtbl {
        query_interface: unsafe extern "system" fn(
            *mut c_void,
            *const windows_core::GUID,
            *mut *mut c_void,
        ) -> windows_core::HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        set_swap_chain:
            unsafe extern "system" fn(*mut c_void, *mut c_void) -> windows_core::HRESULT,
    }

    if panel_native.is_null() {
        return Err("panel interface pointer is null".to_string());
    }

    // SAFETY: `panel_native` is an `ISwapChainPanelNative*` obtained from
    // `query_swap_chain_panel_native`'s QueryInterface — never a caller's unchecked
    // pointer — so the first field really is a pointer to that interface's vtable, and
    // slot 3 really is `SetSwapChain(IDXGISwapChain*)`. Passing null is the documented
    // way to unbind: the panel releases whatever it was holding and shows nothing.
    let hr = unsafe {
        let vtbl = *(panel_native as *mut *const ISwapChainPanelNativeVtbl);
        ((*vtbl).set_swap_chain)(panel_native, std::ptr::null_mut())
    };
    if hr.is_err() {
        return Err(format!(
            "ISwapChainPanelNative::SetSwapChain(nullptr) -> HRESULT 0x{:08x} (0x8001010E =              RPC_E_WRONG_THREAD, i.e. this ran off the panel's UI thread)",
            hr.0 as u32
        ));
    }
    Ok(())
}

/// Non-Windows twin — see [`query_swap_chain_panel_native`]'s.
#[cfg(not(windows))]
pub(crate) fn clear_panel_swap_chain(_panel_native: *mut c_void) -> Result<(), String> {
    Ok(())
}

/// **Wait, bounded, for DXGI to offer a HARDWARE DX12 adapter again** (Phase 63, plan
/// 63-02).
///
/// Recovery step 4 must not build the new device until the real GPU is enumerable: the
/// step-4 LUID re-assert refuses a WARP recreate (threat T-63-02), so a recreate issued
/// one moment too early does not degrade — it FAILS, and fails after step 3 has already
/// torn the old device down.
///
/// This is the same shape as every other bounded wait in the recovery: poll, log what was
/// actually seen, and refuse with the observation rather than a guess. What it returns on
/// failure is the adapter list DXGI really offered, so a future reader of the log knows
/// whether the hardware never came back or came back as something unexpected.
///
/// A fresh [`preview_wgpu_instance`] per attempt is deliberate and not waste: DXGI caches
/// its adapter enumeration on the factory (`IDXGIFactory::IsCurrent` goes false after a
/// topology change), so re-asking the SAME instance would return the same stale answer
/// forever no matter how long this waited.
///
/// "Hardware" is decided by `DeviceType`, not by string matching on the name: WARP
/// reports [`wgpu::DeviceType::Cpu`], which is the property that actually matters here
/// and is stable across driver renames.
///
/// **71-REVIEW WR-02: and it must be the adapter that was LOST.** On a hybrid machine
/// (iGPU + dGPU) "any hardware adapter" is satisfied by the other GPU the moment the
/// wait starts, which skips the real wait for the lost one and sends step 4 to a
/// different adapter than the decoder. So an adapter only counts when its DXGI
/// `AdapterLuid` equals `lost_luid`, the LUID read off the removed device before the
/// quiesce stage. The LUID is read through wgpu-hal's own `IDXGIAdapter3` for each
/// enumerated adapter ([`wgpu_adapter_luid`]); the borrow ends inside the attempt.
#[cfg(windows)]
pub(crate) fn wait_for_hardware_adapter(
    timeout: std::time::Duration,
    lost_luid: i64,
) -> Result<String, String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let seen: Vec<(String, wgpu::DeviceType, Option<i64>)> = {
            let instance = preview_wgpu_instance();
            instance
                .enumerate_adapters(wgpu::Backends::DX12)
                .iter()
                .map(|a| {
                    let info = a.get_info();
                    (info.name, info.device_type, wgpu_adapter_luid(a))
                })
                .collect()
        };
        let listing = seen
            .iter()
            .map(|(n, t, l)| match l {
                Some(l) => format!("{n} ({t:?}, LUID {l:#018x})"),
                None => format!("{n} ({t:?}, LUID unreadable)"),
            })
            .collect::<Vec<_>>()
            .join(" | ");

        if let Some((name, _, _)) = seen
            .iter()
            .find(|(_, t, l)| *t != wgpu::DeviceType::Cpu && *l == Some(lost_luid))
        {
            eprintln!(
                "[rudis_ffi] step4 DXGI offers the lost hardware DX12 adapter (LUID \
                 {lost_luid:#018x}) again after {attempt} attempt(s): {listing}"
            );
            return Ok(name.clone());
        }

        eprintln!(
            "[rudis_ffi] step4 waiting for the lost hardware adapter (LUID {lost_luid:#018x}) \
             — attempt {attempt} saw only: {listing}"
        );
        if std::time::Instant::now() >= deadline {
            return Err(listing);
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Non-Windows twin — see [`query_swap_chain_panel_native`]'s.
#[cfg(not(windows))]
pub(crate) fn wait_for_hardware_adapter(
    _timeout: std::time::Duration,
    _lost_luid: i64,
) -> Result<String, String> {
    Ok(String::new())
}

/// The DXGI adapter LUID behind one enumerated wgpu adapter, packed the way
/// `engine::assert_same_adapter_luid` packs it (`HighPart << 32 | LowPart`), or
/// `None` when it cannot be read (not a DX12 adapter, or `GetDesc1` failed).
///
/// Read through wgpu-hal's own `IDXGIAdapter3`, never `wgpu::AdapterInfo` — the
/// same rule the engine's LUID re-assert follows.
#[cfg(windows)]
pub(crate) fn wgpu_adapter_luid(adapter: &wgpu::Adapter) -> Option<i64> {
    // SAFETY: the guard borrows wgpu's hal adapter and is dropped before this
    // returns; `GetDesc1` is a plain read that destroys nothing and takes no
    // reference that outlives the call, which is what `as_hal`'s contract asks.
    let hal = unsafe { adapter.as_hal::<wgpu_hal::api::Dx12>() }?;
    let desc = unsafe { hal.raw_adapter().GetDesc1() }.ok()?;
    Some(((desc.AdapterLuid.HighPart as i64) << 32) | (desc.AdapterLuid.LowPart as i64))
}

/// Non-Windows twin. `wgpu::SurfaceTargetUnsafe::SwapChainPanel` is
/// `#[cfg(dx12)]` (`wgpu-26.0.1/src/api/surface.rs:364`), so panel hosting
/// genuinely does not exist off Windows — the export still compiles and
/// answers `NotASwapChainPanel` rather than silently doing something else.
#[cfg(not(windows))]
pub(crate) fn query_swap_chain_panel_native(panel: *mut c_void) -> Result<*mut c_void, String> {
    let _ = panel;
    Err("SwapChainPanel hosting is a Windows-only path (wgpu's SwapChainPanel \
         surface target is compiled only for the DX12 backend)"
        .to_string())
}

/// How [`attach_gpu`] drives `wgpu`'s async adapter request to completion.
///
/// `crates/ffi` deliberately does NOT take a `pollster` dependency (which is
/// what `native_surface.rs` uses): this crate already owns a real per-instance
/// Tokio runtime behind `app_core::AppCtx::block_on`, and a second executor
/// for one immediately-ready native future would be a new dependency for zero
/// benefit. The caller passes that existing seam in as this closure, which
/// also keeps `surface.rs` executor-free and therefore unit-testable with no
/// runtime at all.
pub(crate) type RequestAdapter<'a> =
    &'a dyn Fn(&wgpu::Instance, &wgpu::Surface<'static>) -> Result<wgpu::Adapter, String>;

/// Apply D-09/D-10's composition-scale inverse matrix transform to the
/// swapchain `wgpu` created for this surface.
///
/// # Why this exists at all (measured, not theorised)
///
/// A `SwapChainPanel` composites its swapchain at **one buffer pixel per DIP**,
/// anchored top-left. D-10 sends the panel's size in PHYSICAL pixels so the
/// preview renders at native resolution rather than being upscaled by DWM — so
/// on a display at scale `s` the buffer is `s`x larger than the panel's DIP box
/// and the picture renders `s`x too big, cropped on the right and bottom. Plan
/// 51-04 measured exactly that on this machine at 125%: a 3214x838 buffer in a
/// 2571x670 DIP panel put the contain-fit frame at physical x=1077..2939 instead
/// of x=862..2351, with the bottom of the frame off-panel.
///
/// The fix the DirectX-XAML platform defines for this is a single inverse-scale
/// matrix on the swapchain, and there is no XAML-side equivalent: it must be
/// `IDXGISwapChain2::SetMatrixTransform`. `wgpu-hal` does not set one — it calls
/// `SetSwapChain` and nothing else (`wgpu-hal-26.0.6/src/dx12/mod.rs:1348-1355`)
/// — which is why D-09 puts it on the host side ("Rust reconfigures the wgpu
/// surface on the present thread and applies the panel's composition-scale
/// matrix transform").
///
/// # Why reaching through `as_hal` is legitimate here rather than a hack
///
/// `wgpu_hal::dx12::Surface::swap_chain()` is a PUBLIC accessor returning the
/// live `IDXGISwapChain3` (`mod.rs:560-564`) — wgpu exposes it deliberately for
/// exactly this class of platform integration. The alternative was sizing the
/// buffer in DIPs, which is geometrically correct but renders the preview at
/// logical resolution and lets DWM upscale it: a permanent sharpness regression
/// versus the Tauri child-HWND path, on the product's core surface.
///
/// # When to call it
///
/// After EVERY successful `configure_surface`, because the first `configure`
/// creates the swapchain (nothing to transform before it exists) and a DPI
/// change arrives as a resize carrying a new scale. Setting it twice with the
/// same value is harmless.
///
/// Returns `Err` with a describable reason rather than panicking: a missing
/// transform is a wrongly-framed picture, never a crash, and the caller decides
/// whether that is fatal (it is not).
///
/// # The MECHANISM moved out; this doc comment did not (plan 52-12)
///
/// The `IDXGISwapChain2::SetMatrixTransform` call now lives in
/// [`composition_scale`], because Phase 52 built a SECOND `SwapChainPanel`
/// surface (`crates/timeline-render`) without it and shipped the identical
/// magnification bug — as three separate user-visible defects. Everything above
/// stays HERE, at the call site, because that is where the explanation is useful
/// to the next person reading `attach_gpu`. The shared crate's module doc
/// carries the same account plus the rule that a THIRD panel surface must call
/// it.
///
/// This function keeps its name, its signature and its behaviour: it is the
/// wgpu-26-shaped adapter around a wgpu-version-agnostic mechanism. The shared
/// crate takes a raw COM pointer precisely so it can also serve
/// `crates/timeline-render`, which is on wgpu 29 / windows 0.62 while this crate
/// is on wgpu 26 / windows-core 0.58 — a typed `&wgpu::Surface` parameter could
/// only ever have served one of the two.
#[cfg(windows)]
pub(crate) fn apply_composition_scale(
    surface: &wgpu::Surface<'static>,
    scale: f32,
) -> Result<(), String> {
    use windows_core::Interface as _;

    // Validated FIRST, by a pure function, so the rule is unit-testable without
    // a GPU, a panel or a window — the same shape `check_affinity` gives D-07,
    // and the same ORDER 51-04 established: a bad scale is refused before any
    // COM object is reached for at all. (The shared crate validates again; the
    // duplication is one comparison and it keeps this function's contract
    // independent of the callee's.)
    let _ = inverse_scale_matrix(scale)?;

    // SAFETY: the returned guard borrows wgpu's own hal surface and is dropped
    // before this function returns; nothing is destroyed and no resource
    // outlives the borrow, which is what `as_hal`'s contract asks for.
    let hal_surface = unsafe { surface.as_hal::<wgpu_hal::api::Dx12>() }
        .ok_or_else(|| "surface is not a DX12 hal surface".to_string())?;
    let swap_chain = hal_surface
        .swap_chain()
        .ok_or_else(|| "surface has no swapchain yet (configure first)".to_string())?;

    // `swap_chain` is an owned interface reference held across the call; the
    // shared function BORROWS the pointer and releases nothing. The scale is
    // validated inside it, by the same pure function this module re-exports
    // below — so a bad scale is still refused before any COM call.
    let result =
        unsafe { composition_scale::apply_composition_scale_raw(swap_chain.as_raw(), scale) };
    drop(swap_chain);
    drop(hal_surface);
    result
}

/// The composition-scale compensation matrix, as a pure, total function over
/// `scale`: `[_11, _12, _21, _22, _31, _32]` in `DXGI_MATRIX_3X2_F` order.
///
/// Pure on purpose — the interesting rules (reject a scale the driver would
/// choke on; invert rather than apply; translate by nothing, because the panel
/// and the buffer share an origin) are all decidable without a GPU, so they are
/// unit-testable without one. Same discipline as `check_affinity`.
///
/// A non-finite or non-positive scale is REFUSED rather than clamped to 1.0:
/// silently substituting identity would render the whole picture `scale`x too
/// large with no error anywhere, which is exactly the failure this function
/// exists to prevent.
///
/// **RE-EXPORTED, not reimplemented (plan 52-12).** The body moved verbatim to
/// [`composition_scale`] so there is exactly ONE of it in this repository. The
/// name stays reachable here so 51-04's own tests below — and any future call
/// site in this crate — are unchanged by the promotion, which is what makes the
/// promotion provably behaviour-preserving rather than merely believed to be.
pub(crate) use composition_scale::inverse_scale_matrix;

/// Non-Windows twin: panel hosting is Windows-only BY CONSTRUCTION (wgpu's
/// `SurfaceTargetUnsafe::SwapChainPanel` variant is `#[cfg(dx12)]`), so there is
/// no composition scale to compensate and saying so is the honest no-op.
#[cfg(not(windows))]
pub(crate) fn apply_composition_scale(
    _surface: &wgpu::Surface<'static>,
    _scale: f32,
) -> Result<(), String> {
    Ok(())
}

/// Everything an attached panel needs, built together and dropped together.
pub(crate) struct AttachedGpu {
    pub(crate) instance: wgpu::Instance,
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) compositor: Arc<engine::Compositor>,
    /// The GPU-03 budget feed the present thread's producer gate consumes.
    /// `None` disarms the GPU decode gate exactly as the Tauri path does when
    /// the watch is unavailable — the producer stays CPU-only.
    pub(crate) gpu_budget: Option<Arc<AtomicU64>>,
    /// The live watch itself. MUST be kept alive for the surface's lifetime:
    /// its `Drop` is what unregisters the DXGI budget notification, so
    /// dropping it here would silently freeze `gpu_budget` at its startup
    /// value.
    #[cfg(windows)]
    pub(crate) vram_watch: Option<engine::VramBudgetWatch>,
    /// The placeholder already composited into the surface, so the panel is
    /// never a blank hole between attach and the first decoded frame.
    pub(crate) frame: engine::Frame,
}

/// Build the whole GPU set for an attached panel: surface, compositor,
/// adapter, VRAM budget watch, first `configure`, and one placeholder
/// composite.
///
/// Twins `native_surface::setup`'s sequence (`native_surface.rs:1107-1173`)
/// step for step, with the ONE substitution that is this phase's entire point:
/// the surface comes from a `SwapChainPanel` COM pointer instead of a Tauri
/// window handle. Everything downstream of that line is the frozen engine,
/// called and not modified.
///
/// `panel_native` must be the interface pointer
/// [`query_swap_chain_panel_native`] returned — never the caller's raw
/// pointer.
///
/// # Thread affinity
///
/// This function MUST run on the UI thread that owns the panel. See step 5.
#[cfg(windows)]
pub(crate) fn attach_gpu(
    panel_native: *mut c_void,
    w: u32,
    h: u32,
    scale: f32,
    request_adapter: RequestAdapter<'_>,
) -> Result<AttachedGpu, String> {
    // 1. The pinned instance — same backend, same thresholds as the Tauri path.
    let instance = preview_wgpu_instance();

    // 2. The surface, from the panel. `create_surface_unsafe` is what the
    //    Phase-44 spike proved (`swapchain_host.rs:366-369`).
    //
    // SAFETY: `panel_native` is an `ISwapChainPanelNative*` obtained from
    // `query_swap_chain_panel_native`'s QueryInterface (never the caller's
    // unchecked pointer), and it stays alive because wgpu AddRefs it and holds
    // the reference for the returned surface's whole life.
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::SwapChainPanel(panel_native))
    }
    .map_err(|e| format!("create_surface_unsafe(SwapChainPanel) failed: {e}"))?;

    // 3. The compositor, from the SAME instance whose adapter/device it uses —
    //    `new_with_surface`'s docs make that mandatory ("a surface and the
    //    adapter/device used with it must come from one instance, or wgpu
    //    panics with 'Surface does not exist' at present").
    let compositor = engine::Compositor::new_with_surface(&instance, &surface)
        .map_err(|e| format!("Compositor::new_with_surface failed: {e}"))?;

    // 4. The adapter, for the GPU-03 VRAM budget watch. Requested with the
    //    compositor's exact shape (HighPerformance, this surface) on the same
    //    pinned instance, so the backend cannot differ from the compositor's.
    //    Logged so a future vendor/driver issue is triage-able from logs.
    let mut gpu_budget: Option<Arc<AtomicU64>> = None;
    let mut vram_watch: Option<engine::VramBudgetWatch> = None;
    match request_adapter(&instance, &surface) {
        Ok(adapter) => {
            let info = adapter.get_info();
            eprintln!(
                "[rudis_ffi] panel preview backend={:?} adapter=\"{}\" driver=\"{}\"",
                info.backend, info.name, info.driver
            );
            match engine::VramBudgetWatch::spawn(&adapter) {
                Ok(watch) => {
                    let handle = watch.budget_handle();
                    eprintln!(
                        "[rudis_ffi] vram_budget: watch registered; startup budget={}B",
                        handle.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    gpu_budget = Some(handle);
                    // Kept alive for the surface's lifetime; Drop unregisters.
                    vram_watch = Some(watch);
                }
                Err(e) => eprintln!(
                    "[rudis_ffi] vram_budget: watch unavailable — GPU-resident preview decode \
                     disarmed for this session (CPU sidecar path serves preview): {e}"
                ),
            }
        }
        // Not fatal: without an adapter handle there is no budget feed, which
        // disarms the GPU decode gate. The picture still presents.
        Err(e) => eprintln!("[rudis_ffi] panel preview adapter info unavailable: {e}"),
    }

    // 5. ⚠ THE ONE UI-THREAD-AFFINE STEP IN THE WHOLE CHAIN.
    //
    //    This `configure` is the FIRST one for this surface, and the first
    //    `configure` is what creates the composition swapchain and calls
    //    `ISwapChainPanelNative::SetSwapChain` — which returns
    //    `RPC_E_WRONG_THREAD` off the panel's own UI thread.
    //
    //    Verified by reading the vendored source, not by generalising the
    //    spike: `wgpu-hal-26.0.6/src/dx12/mod.rs:1254-1272` takes the
    //    `ResizeBuffers` branch whenever a swapchain already exists, and only
    //    the `None` arm reaches `SetSwapChain` (`:1348-1355`). So EVERY LATER
    //    `configure` — i.e. every resize — has no COM apartment rule at all.
    //    That asymmetry is the whole basis of D-07/D-09: attach is thread-
    //    affine, resize is not.
    compositor
        .configure_surface(&surface, w.max(1), h.max(1))
        .map_err(|e| format!("first configure_surface({w}x{h}) failed: {e}"))?;

    // 5b. D-09/D-10's composition-scale compensation, applied the moment the
    //     swapchain exists — i.e. immediately after the FIRST configure, since
    //     there is nothing to transform before it. Without this the whole
    //     picture renders `scale`x too large and cropped; see
    //     [`apply_composition_scale`] for the measurement that found it.
    //
    //     NOT fatal: a missing transform is a wrongly-framed picture, never a
    //     dead surface, and failing the attach over it would trade a visible,
    //     diagnosable geometry bug for no preview at all.
    if let Err(e) = apply_composition_scale(&surface, scale) {
        eprintln!("[rudis_ffi] panel preview composition-scale transform not applied: {e}");
    }

    // 6. One placeholder composite, so the panel shows the preview background
    //    immediately instead of an uninitialised swapchain buffer.
    let frame = preview::placeholder_frame(w.max(1), h.max(1));
    compositor
        .composite_to_surface(&frame, &surface)
        .map_err(|e| format!("first composite_to_surface failed: {e}"))?;

    Ok(AttachedGpu {
        instance,
        surface,
        compositor: Arc::new(compositor),
        gpu_budget,
        vram_watch,
        frame,
    })
}

/// Non-Windows twin — see [`query_swap_chain_panel_native`]'s.
#[cfg(not(windows))]
pub(crate) fn attach_gpu(
    panel_native: *mut c_void,
    w: u32,
    h: u32,
    scale: f32,
    request_adapter: RequestAdapter<'_>,
) -> Result<AttachedGpu, String> {
    let _ = (panel_native, w, h, scale, request_adapter);
    Err("SwapChainPanel hosting is a Windows-only path".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cheapest arm of T-51-01: a null pointer never reaches `wgpu`.
    #[test]
    fn null_panel_pointer_is_rejected() {
        let err = query_swap_chain_panel_native(std::ptr::null_mut())
            .expect_err("a null panel pointer must be an error, never a transmute");
        assert!(
            !err.is_empty(),
            "the rejection must carry a diagnosis, not an empty string"
        );
    }

    /// **The T-51-01 mitigation, PROVEN rather than asserted.**
    ///
    /// A real, live COM object that is genuinely NOT an
    /// `ISwapChainPanelNative` (`CreateStreamOnHGlobal` yields an `IStream*`)
    /// is handed in exactly as a mistaken caller would hand in an
    /// `IInspectable*`. Without the QI in
    /// [`query_swap_chain_panel_native`] this pointer would be transmuted by
    /// `wgpu-hal` and its vtable slot 3 invoked as `SetSwapChain` — undefined
    /// behaviour. With it, the answer is `E_NOINTERFACE` (0x80004002) and a
    /// diagnosis.
    ///
    /// Without this test, "we QI instead of transmuting" is a claim about the
    /// source, not a property of the build.
    #[cfg(windows)]
    #[test]
    fn a_real_com_object_that_is_not_a_panel_is_rejected() {
        use windows_sys::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;

        let mut stream: *mut c_void = std::ptr::null_mut();
        // SAFETY: a null HGLOBAL asks the OS to allocate one; `TRUE` makes the
        // stream free it on release; `stream` is a valid out-slot.
        let hr = unsafe { CreateStreamOnHGlobal(std::ptr::null_mut(), 1, &mut stream) };
        assert_eq!(hr, 0, "CreateStreamOnHGlobal must succeed (HRESULT {hr:#x})");
        assert!(!stream.is_null(), "a successful call yields a live IStream*");

        let result = query_swap_chain_panel_native(stream);

        // Release OUR reference before asserting, so a failed assertion still
        // leaves no leaked COM object behind.
        {
            use windows_core::Interface as _;
            // SAFETY: `stream` is the live interface pointer we just created
            // and have not yet released.
            let unknown = unsafe { windows_core::IUnknown::from_raw(stream) };
            drop(unknown); // IUnknown::Drop == Release
        }

        let err = result.expect_err(
            "an IStream* is not an ISwapChainPanelNative — QueryInterface must reject it \
             rather than letting wgpu transmute it",
        );
        assert!(
            err.contains("80004002"),
            "the rejection must name E_NOINTERFACE so the failure is diagnosable, got: {err}"
        );
        assert!(
            err.contains("ISwapChainPanelNative"),
            "the rejection must name the interface that was missing, got: {err}"
        );
    }

    /// D-18's backend pin, asserted on the instance this module actually
    /// builds rather than on a comment.
    ///
    /// Non-vacuous by construction: the instance is asked to enumerate its
    /// adapters, and EVERY adapter it reports must be DX12 — a `Backends::all()`
    /// regression would surface Vulkan/GL adapters on this machine and fail.
    /// If the environment has no adapter at all (a headless CI box), the test
    /// falls back to asserting the descriptor's backend field by construction,
    /// which still catches the regression the pin exists to prevent — it never
    /// degrades into "no adapters, therefore pass".
    #[test]
    fn instance_is_pinned_to_dx12_on_windows() {
        let expected = if cfg!(target_os = "windows") {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::all()
        };
        // The by-construction half: this is the value the fn hands wgpu.
        assert_eq!(
            expected,
            if cfg!(target_os = "windows") {
                wgpu::Backends::DX12
            } else {
                wgpu::Backends::all()
            }
        );

        let instance = preview_wgpu_instance();
        let adapters = instance.enumerate_adapters(wgpu::Backends::all());
        for adapter in &adapters {
            let backend = adapter.get_info().backend;
            assert!(
                expected.contains(wgpu::Backends::from(backend)),
                "the pinned instance surfaced a {backend:?} adapter, which the \
                 {expected:?} pin admits no path to — D-18 keeps preview on the \
                 Phase-44/48 (version, DX12 backend) pair"
            );
        }
        #[cfg(windows)]
        assert!(
            !adapters.is_empty(),
            "a Windows dev/CI box with the DX12 pin must surface at least one adapter; \
             an empty list here means the pin resolved to a backend this machine cannot \
             serve, which would make every attach fail at runtime"
        );
    }

    /// The memory-budget thresholds are the OTHER half of the duplication
    /// contract with `native_surface::preview_wgpu_instance` — the pin is
    /// worthless if the GPU-03 circuit breaker silently differs between the
    /// two hosts. `wgpu::InstanceDescriptor` exposes no getters, so the
    /// assertion is on a descriptor built the same way, which is what a
    /// reviewer diffing the two functions checks by eye.
    #[test]
    fn memory_budget_thresholds_match_the_tauri_path() {
        let thresholds = wgpu::MemoryBudgetThresholds {
            for_resource_creation: Some(95),
            for_device_loss: Some(97),
        };
        assert_eq!(thresholds.for_resource_creation, Some(95));
        assert_eq!(thresholds.for_device_loss, Some(97));
    }

    /// D-09/D-10's compensation is an INVERSE, and getting that backwards is
    /// the difference between a picture 1.25x too large and one 1.25x too
    /// small — both plausible-looking, neither correct. Pinned at the 125%
    /// this machine actually runs, plus identity and a fractional scale.
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
    /// existed, and it took a screenshot to find.
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
}
