//! The hosting core: a `Microsoft.UI.Xaml.Controls.SwapChainPanel` COM pointer in,
//! an independent `wgpu` device presenting into it out.
//!
//! Everything Phase 52 renders stands on this file, so the two facts that make it
//! correct are stated here rather than left to be rediscovered.
//!
//! # THREAD RULE (a hard requirement, not a style preference)
//!
//! `wgpu`'s [`wgpu::Surface::configure`] calls `ISwapChainPanelNative::SetSwapChain`
//! internally (verified in `wgpu-hal`'s dx12 backend: `Surface::configure` →
//! `IDXGIFactory2::CreateSwapChainForComposition` → `ISwapChainPanelNative::SetSwapChain`).
//! `SetSwapChain` returns `RPC_E_WRONG_THREAD` when it is called from any thread other
//! than the UI thread that owns the panel. Therefore:
//!
//! * [`TimelineSurface::attach`] MUST be called on the panel's own UI thread — in
//!   practice, from its `Loaded` handler or through its `DispatcherQueue`.
//! * [`TimelineSurface::resize`] MUST likewise, because it reconfigures.
//! * [`TimelineSurface::present_solid`] is the ONLY method that is safe off that
//!   thread. `get_current_texture` / `submit` / `present` do not touch
//!   `ISwapChainPanelNative`, which is what makes a dedicated present thread legal
//!   (and is exactly the shape Microsoft's own SwapChainPanel guidance describes).
//!
//! # QI BEFORE wgpu, ALWAYS
//!
//! `wgpu` does **not** `QueryInterface` the pointer it is handed for
//! [`wgpu::SurfaceTargetUnsafe::SwapChainPanel`] — it transmutes the reference. Handing
//! it a bare `IInspectable*` would therefore call `IInspectable::GetIids` (vtable slot 3)
//! as if it were `SetSwapChain`: undefined behaviour, not an error return. So this module
//! does the `QueryInterface` itself, first, and turns a wrong pointer into a clean
//! [`TimelineError::NotASwapChainPanel`] instead of a crash. Established by
//! `spikes/44-hwaccel/src/swapchain_host.rs`; not re-derived here.
//!
//! # ONE DEVICE PER SURFACE, NEVER A SHARED ONE
//!
//! Each [`TimelineSurface`] owns its own `Instance`, `Adapter`, `Device`, `Queue` and
//! `Surface`. Nothing is shared, nothing is static. That is 52-RESEARCH Architecture
//! Pattern 2, and the alternative is not merely worse — a shared-device design is
//! DISQUALIFIED by the engine-axis freeze, because sharing the video device would
//! require editing a frozen crate to hand its handle out.

use std::ffi::c_void;

use windows::core::{Interface, GUID};

/// `ISwapChainPanelNative`.
///
/// Byte-identical between the Windows App SDK 1.8 header
/// `microsoft.ui.xaml.media.dxinterop.h` (`MIDL_INTERFACE("63aad0b8-7c24-40ff-85a8-640d944cc325")`)
/// and `wgpu-hal`'s own private binding — checked, not assumed
/// (`spikes/44-hwaccel/src/swapchain_host.rs:72-82`).
pub const IID_ISWAPCHAINPANELNATIVE: GUID =
    GUID::from_u128(0x63aad0b8_7c24_40ff_85a8_640d944cc325);

/// The surface format preferred when the adapter offers it. Non-sRGB on purpose: the
/// clear colour written by [`TimelineSurface::present_solid`] is already display-referred,
/// and an `*_Srgb` target would encode it a second time.
const PREFERRED_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// Every way attaching or presenting can fail, as a code that can cross an FFI boundary.
///
/// Distinct variants, not one catch-all: the tripwire's whole value is being able to say
/// WHICH link broke if it breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineError {
    /// The caller passed a null panel pointer.
    NullPanel,
    /// `QueryInterface(ISwapChainPanelNative)` failed — the pointer is not a
    /// `SwapChainPanel`. Never forwarded to wgpu; see the module docs.
    NotASwapChainPanel,
    /// `Instance::create_surface_unsafe` failed.
    SurfaceCreation,
    /// No DX12 adapter could be found for this surface.
    NoAdapter,
    /// `Adapter::request_device` failed.
    NoDevice,
    /// The adapter reports no supported formats for this surface.
    NoSurfaceFormat,
    /// `get_current_texture` returned a recoverable error (timeout / validation).
    Present,
    /// `get_current_texture` reported the device or surface as lost or outdated.
    DeviceLost,
    /// The window is minimised or fully covered. NOT an error — wgpu's own guidance is
    /// to skip the frame and retry — but it is not a presented frame either, so it is a
    /// distinct outcome rather than being folded into either bucket.
    Occluded,
}

impl TimelineError {
    /// The negative code this error crosses the FFI boundary as.
    pub fn code(self) -> i32 {
        match self {
            TimelineError::NullPanel => -1,
            TimelineError::NotASwapChainPanel => -2,
            TimelineError::SurfaceCreation => -3,
            TimelineError::NoAdapter => -4,
            TimelineError::NoDevice => -5,
            TimelineError::NoSurfaceFormat => -6,
            TimelineError::Present => -7,
            TimelineError::DeviceLost => -8,
            TimelineError::Occluded => -9,
        }
    }
}

/// `QueryInterface` the supplied COM pointer for `ISwapChainPanelNative`, failing fast.
///
/// This is the fail-fast that keeps a wrong pointer from becoming undefined behaviour
/// inside wgpu (threat T-52-01). The returned pointer carries a reference this process
/// owns for the life of the surface; the caller's own reference stays the caller's.
///
/// # Safety
/// `panel` must be either null or a valid COM interface pointer.
pub fn query_swap_chain_panel_native(panel: *mut c_void) -> Result<*mut c_void, TimelineError> {
    if panel.is_null() {
        return Err(TimelineError::NullPanel);
    }

    // from_raw_borrowed does not take ownership — the caller keeps its reference.
    let unknown = unsafe { windows::core::IUnknown::from_raw_borrowed(&panel) }
        .ok_or(TimelineError::NullPanel)?;

    let mut native: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { unknown.query(&IID_ISWAPCHAINPANELNATIVE, &mut native) };
    if hr.is_err() || native.is_null() {
        return Err(TimelineError::NotASwapChainPanel);
    }
    Ok(native)
}

/// Apply the composition-scale compensation to the swapchain `wgpu` created for
/// `surface`. **Call after EVERY successful `configure`.**
///
/// # What goes wrong without it (D1 + D2 + D3, measured)
///
/// A `SwapChainPanel` composites its swapchain at ONE BUFFER PIXEL PER DIP. This
/// crate is handed PHYSICAL pixels by `rudis_timeline_attach` so the Timeline
/// renders at native resolution rather than being upscaled by DWM — so on a
/// display at scale `s` the buffer is `s`x larger than the panel's DIP box and
/// everything drawn is magnified by `s` about the panel's top-left. The POINTER
/// path is unaffected (`GetCurrentPoint(TimelineSurface).Position` is already
/// logical), so hit test and pixels disagree MULTIPLICATIVELY: at 125% the
/// playhead lands `0.25 * x` DIP right of the pointer, the 6px trim zones sit ~58
/// logical px left of where the pointer arrives (so a body drag of the NEXT clip
/// fires instead of a trim), and only the top ~57% of the drawn V1 lane resolves
/// to its own lane.
///
/// The whole correction is one inverse-scale matrix, and it lives in
/// [`composition_scale`] — the repository's ONLY copy, shared with the Preview
/// surface in `crates/ffi`, which had this fix a phase earlier (plan 51-04) and
/// from which it was not inherited.
///
/// # Why reaching through `as_hal` is legitimate rather than a hack
///
/// `wgpu_hal::dx12::Surface::swap_chain()` is a PUBLIC accessor returning the
/// live `IDXGISwapChain3` (`wgpu-hal-29.0.4/src/dx12/mod.rs:633`) — wgpu exposes
/// it deliberately for exactly this class of platform integration. The
/// alternative, sizing the buffer in DIPs, is geometrically correct and renders
/// the Timeline at logical resolution for DWM to upscale: a permanent sharpness
/// regression. 51-04 evaluated and rejected it for the Preview for that reason.
///
/// Returns `Err` with a describable reason rather than panicking; the callers
/// LOG and CONTINUE. A missing transform is a wrongly-framed picture, never a
/// dead surface.
fn apply_composition_scale(surface: &wgpu::Surface<'static>, scale: f32) -> Result<(), String> {
    // ⚠ IMPORTED INSIDE THE FUNCTION BODY, NEVER AT MODULE SCOPE.
    //
    // This file already has `use windows::core::{Interface, GUID};` at module
    // scope, at windows 0.58 — the version the `ISwapChainPanelNative`
    // QueryInterface must use. `windows_dxgi` is windows 0.62 (wgpu-hal 29's own
    // universe, and the only one in which the hal's `IDXGISwapChain3` is a
    // nameable type). Bringing its `Interface` into MODULE scope too would make
    // both traits candidates at every call site in this file. Anonymous (`as _`)
    // and function-local, it is a candidate only here — and only the 0.62 trait
    // has an impl for the 0.62 type, so `as_raw` resolves without ambiguity.
    use windows_dxgi::core::Interface as _;

    // SAFETY: the guard borrows wgpu's own hal surface. It is held for the whole
    // of the call below and dropped before this function returns, so nothing is
    // destroyed and no resource outlives the borrow — which is what `as_hal`'s
    // contract asks for. Same contract `crates/ffi` documents on its twin.
    let hal_surface = unsafe { surface.as_hal::<wgpu_hal::api::Dx12>() }
        .ok_or_else(|| "surface is not a DX12 hal surface".to_string())?;
    let swap_chain = hal_surface
        .swap_chain()
        .ok_or_else(|| "surface has no swapchain yet (configure first)".to_string())?;

    // `swap_chain` is an OWNED clone of the hal's interface, so it holds a
    // reference for the whole call; `apply_composition_scale_raw` BORROWS the
    // pointer and releases nothing.
    let result = unsafe { composition_scale::apply_composition_scale_raw(swap_chain.as_raw(), scale) };
    drop(swap_chain);
    drop(hal_surface);
    result
}

/// `ISwapChainPanelNative`'s vtable: `IUnknown`'s three slots, then `SetSwapChain`.
///
/// Declared by hand rather than pulled from a projection because exactly one method is
/// needed and the shape is pinned by two independent sources: the Windows App SDK 1.8
/// header, and `wgpu-hal`'s own binding (`wgpu-hal-29.0.4/src/dx12/types.rs`, an
/// `IUnknown_Vtbl` base followed by `SetSwapChain`).
#[repr(C)]
struct ISwapChainPanelNativeVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    set_swap_chain: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
}

/// Unbind the panel from whatever swapchain it is showing (`SetSwapChain(nullptr)`).
/// Returns the HRESULT.
///
/// # THE TEARDOWN ORDER THIS FUNCTION IS PART OF — LEARNED BY CRASHING, TWICE
///
/// Detaching a Timeline surface is a THREE-STEP sequence and the order is load-bearing:
///
/// 1. stop and join the present thread (nothing may be presenting);
/// 2. **this function**, on the panel's UI thread, **while the `wgpu` objects are still
///    alive**;
/// 3. only then drop the `wgpu` `Surface`/`Device`/`Instance`, and finally
///    [`release_panel_native`].
///
/// Both wrong orders were measured, in this crate, on real hardware:
///
/// * **Skipping step 2 entirely** (the obvious implementation — just drop the wgpu
///   objects) leaves the XAML `SwapChainPanel` still holding the DXGI swapchain, because
///   `wgpu-hal` calls `SetSwapChain` in exactly ONE place (`configure`) and never with
///   null. The orphaned swapchain keeps a dead D3D12 device alive into process teardown:
///   the run measured perfectly, wrote its transcript, and then died with `0xC0000005`
///   inside `D3D12Core.dll` at exit.
/// * **Doing step 2 AFTER step 3** works for every panel except the LAST one, and then
///   access-violates inside `SetSwapChain` itself. With the final `wgpu::Instance` gone,
///   so is the DXGI factory the panel's unbind path relies on. The stage trace makes the
///   asymmetry unmistakable: the first panel logs `hr=0x00000000` and a healthy refcount,
///   the second never returns from the same call.
///
/// **Call on the panel's UI thread** — `SetSwapChain` returns `RPC_E_WRONG_THREAD`
/// anywhere else, exactly as it does when configuring. That is why unbinding is a free
/// function rather than `TimelineSurface`'s `Drop`: the surface may be dropped by whatever
/// thread owns it (for the smoke, a dedicated present thread), while the panel must be
/// unbound by the UI thread specifically.
///
/// # Safety
/// `native` must be a pointer obtained from [`query_swap_chain_panel_native`], or null.
pub unsafe fn unbind_swap_chain_panel(native: *mut c_void) -> i32 {
    if native.is_null() {
        return 0;
    }
    let vtbl = *(native as *const *const ISwapChainPanelNativeVtbl);
    crate::trace("panel:set-swap-chain-null:begin");
    let hr = ((*vtbl).set_swap_chain)(native, std::ptr::null_mut());
    crate::trace(&format!("panel:set-swap-chain-null:hr=0x{hr:08x}"));
    hr
}

/// Drop this crate's `QueryInterface` reference to the panel. Returns the refcount the
/// panel reports afterwards (XAML holds plenty of its own; this is never expected to be
/// the last one).
///
/// Step 4 of the sequence documented on [`unbind_swap_chain_panel`].
///
/// # Safety
/// `native` must be a pointer obtained from [`query_swap_chain_panel_native`] that has not
/// already been passed to this function, or null.
pub unsafe fn release_panel_native(native: *mut c_void) -> u32 {
    if native.is_null() {
        return 0;
    }
    let vtbl = *(native as *const *const ISwapChainPanelNativeVtbl);
    let remaining = ((*vtbl).release)(native);
    crate::trace(&format!("panel:released refcount_after={remaining}"));
    remaining
}

/// An OWNED `ISwapChainPanelNative*` reference: released on drop unless it is
/// explicitly handed on with [`PanelNativeRef::into_raw`].
///
/// # WHY THIS TYPE EXISTS (52-REVIEW WR-01)
///
/// [`query_swap_chain_panel_native`] performs a `QueryInterface`, and a successful QI
/// ADDREFS — this crate now owns one reference to the panel. [`TimelineSurface::attach`]
/// then runs four more fallible steps (surface creation, adapter, device, format
/// capabilities), every one of which can legitimately fail on real hardware for reasons
/// [`TimelineError`]'s own variants exist to name: a device loss during a reattach cycle,
/// a transient adapter enumeration failure. Before this type, each of those returned `Err`
/// with the reference still held and `Self` never constructed — so the ONLY code that
/// releases it (`rudis_timeline_detach`, reading the pointer back out of
/// [`TimelineSurface::panel_native`]) never ran, and never could: there was no handle for
/// the C# side to detach. Every failed attach pinned the panel's refcount one higher for
/// the life of the process.
///
/// Hand-threading a `release_panel_native` call into each of those four branches would fix
/// today's four; it would not stop the fifth from being added without one, which is
/// precisely how the first four came to be missing. So the fix is a shape rather than a
/// patch: the reference has an owner from the instant it is acquired, and `?` gives it
/// back automatically.
///
/// Note the asymmetry with [`unbind_swap_chain_panel`], which is deliberately NOT part of
/// this type: unbinding must happen on the panel's UI thread while the wgpu objects are
/// still alive, whereas merely giving back a refcount is thread-agnostic. A guard that
/// unbound on drop would reintroduce the exact crash the teardown order documented on
/// `unbind_swap_chain_panel` was learned by. This guard's `Drop` runs only when `Self` was
/// never built, i.e. when nothing was ever bound.
struct PanelNativeRef(*mut c_void);

impl PanelNativeRef {
    /// `QueryInterface` the caller's panel pointer and TAKE OWNERSHIP of the resulting
    /// reference.
    fn acquire(panel: *mut c_void) -> Result<Self, TimelineError> {
        query_swap_chain_panel_native(panel).map(Self)
    }

    /// The pointer, borrowed. The reference stays this guard's.
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Hand the reference on to a longer-lived owner — in practice
    /// [`TimelineSurface::panel_native`], which `rudis_timeline_detach` releases as step 4
    /// of the documented teardown. Consumes the guard WITHOUT releasing.
    fn into_raw(self) -> *mut c_void {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }
}

impl Drop for PanelNativeRef {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `query_swap_chain_panel_native` (the only
        // constructor), this type is the sole owner of that reference, and `into_raw` —
        // the only way a reference leaves — forgets rather than drops, so no pointer can
        // reach here twice.
        unsafe { release_panel_native(self.0) };
    }
}

/// One `SwapChainPanel`, one independent DX12 `wgpu` device presenting into it.
///
/// Field order is drop order: the surface is released before the device that configured
/// it, and the instance outlives both.
pub struct TimelineSurface {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter: wgpu::Adapter,
    instance: wgpu::Instance,
    /// The QI'd `ISwapChainPanelNative*`. A RAW pointer with no `Drop`, deliberately:
    /// unbinding the panel is the UI thread's job (see [`release_swap_chain_panel`]) and
    /// this struct may be dropped anywhere.
    panel_native: *mut c_void,
}

// SAFETY: `panel_native` is an opaque COM pointer this struct never dereferences — every
// use of it goes through `unbind_swap_chain_panel` / `release_panel_native`, whose own
// contracts carry the UI-thread requirement. Everything else in this struct is `wgpu`'s,
// which is Send + Sync on Windows. Without these the surface could neither be moved onto
// a present thread nor shared with the owner that has to outlive it (see the teardown
// order on `unbind_swap_chain_panel`).
unsafe impl Send for TimelineSurface {}
unsafe impl Sync for TimelineSurface {}

impl TimelineSurface {
    /// Create an independent device/swapchain pair bound to `panel`.
    ///
    /// **Call on the panel's UI thread** (see the module's THREAD RULE) — `configure`
    /// at the end of this function is the call that reaches `SetSwapChain`.
    ///
    /// `scale` is the panel's own `CompositionScaleX`, and it is LOAD-BEARING, not
    /// decoration: `width_px`/`height_px` are PHYSICAL pixels, and without the
    /// matching inverse-scale transform on the swapchain the whole Timeline draws
    /// `scale`x too large. See [`apply_composition_scale`].
    ///
    /// # Safety
    /// `panel` must be a valid COM pointer to a `Microsoft.UI.Xaml.Controls.SwapChainPanel`
    /// (an `IInspectable*` is fine — this function QIs it), or null.
    pub fn attach(
        panel: *mut c_void,
        width_px: u32,
        height_px: u32,
        scale: f32,
    ) -> Result<Self, TimelineError> {
        // QI FIRST. Nothing below may run against an unverified pointer.
        //
        // Held as an OWNING GUARD, not as a bare pointer: every `?` between here and the
        // `Ok(...)` at the bottom is a path on which this reference has to be given back,
        // and hand-threading a release into each of them is the shape that lost all four
        // to begin with (52-REVIEW WR-01). `panel_ref.into_raw()` at the very end is the
        // single point where ownership transfers to `Self`.
        let panel_ref = PanelNativeRef::acquire(panel)?;
        let native = panel_ref.as_ptr();

        // TEST-ONLY fault injection, compiled out of every shipping build: it gives
        // `attach_releases_the_qi_reference_on_every_post_qi_failure` a deterministic
        // post-QI failure to observe, on the REAL function, without needing a genuine
        // XAML panel or a DX12 adapter (both of which would have to be real for the
        // wgpu steps below to be reachable at all, and neither of which a unit test on
        // a headless agent has). Placed here — before the first wgpu call — because
        // that is the earliest post-QI return point and therefore the one that proves
        // the guard rather than the surrounding code.
        #[cfg(test)]
        {
            if let Some(injected) = tests::take_injected_post_qi_failure() {
                return Err(injected);
            }
        }

        // DX12 is PINNED explicitly, for the same stated reason the shipping
        // compositor pins it (crates/engine/src/compositor.rs:504-517): unpinned, wgpu
        // resolves to Vulkan on Windows, and SwapChainPanel hosting is a DX12 mechanism.
        //
        // wgpu 29's InstanceDescriptor deliberately has NO Default (its `display` field
        // is a boxed trait object), so every field is named here rather than defaulted —
        // which is the better shape anyway: the backend pin is impossible to lose in a
        // `..Default::default()`.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });

        // SAFETY: `native` is an ISwapChainPanelNative*, proven by the QI above — which
        // is precisely the invariant wgpu does not check for itself.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::SwapChainPanel(native))
        }
        .map_err(|_| TimelineError::SurfaceCreation)?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|_| TimelineError::NoAdapter)?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rudis-timeline-device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|_| TimelineError::NoDevice)?;

        let caps = surface.get_capabilities(&adapter);
        if caps.formats.is_empty() || caps.alpha_modes.is_empty() {
            return Err(TimelineError::NoSurfaceFormat);
        }
        let format = if caps.formats.contains(&PREFERRED_FORMAT) {
            PREFERRED_FORMAT
        } else {
            caps.formats[0]
        };

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width_px.max(1),
            height: height_px.max(1),
            // Fifo, deliberately: it is the only present mode guaranteed available, AND
            // it is what makes the tripwire's inter-present deltas MEAN something. Under
            // Fifo each swapchain paces itself to the display's refresh, so "did the
            // second panel steal frames from the first" is answerable by comparing the
            // first panel's delta percentiles with the second panel off and on.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        // THE call that runs CreateSwapChainForComposition + SetSwapChain inside
        // wgpu-hal. The UI-thread requirement on this whole function is about this line.
        surface.configure(&device, &config);

        // ...and the compensation, the moment the swapchain exists (there is
        // nothing to transform before the FIRST configure creates it).
        //
        // NOT FATAL: a missing transform is a wrongly-framed picture, never a dead
        // surface, and failing the attach over it would trade a visible,
        // diagnosable geometry bug for no Timeline at all. Same disposition
        // `crates/ffi` took at 51-04 (T-52-58).
        if let Err(e) = apply_composition_scale(&surface, scale) {
            crate::trace(&format!("attach:composition-scale-not-applied err={e}"));
            eprintln!("[rudis_timeline] composition-scale transform not applied: {e}");
        }

        // OWNERSHIP TRANSFERS HERE, and nowhere else. Past this line the reference
        // belongs to `Self::panel_native` and is released by step 4 of
        // `rudis_timeline_detach`; before it, every exit released it automatically.
        Ok(Self {
            surface,
            config,
            device,
            queue,
            adapter,
            instance,
            panel_native: panel_ref.into_raw(),
        })
    }

    /// The QI'd `ISwapChainPanelNative*` this surface is bound to.
    ///
    /// Callers keep a copy so they can [`release_swap_chain_panel`] it on the UI thread
    /// AFTER this struct has been dropped — see that function for why both halves are
    /// required and what happens if the second one is skipped.
    pub fn panel_native(&self) -> *mut c_void {
        self.panel_native
    }

    /// The negotiated swapchain format — recorded rather than assumed.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// This surface's own device. Every pipeline, buffer and atlas the renderer owns is
    /// created from it, and from no other — one device per surface is Architecture Pattern
    /// 2, and mixing two would fail at the first `set_pipeline` rather than at creation.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// This surface's own queue.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Acquire the next swapchain texture, classifying every wgpu 29 outcome.
    ///
    /// Factored out of [`Self::present_solid`] by plan 52-04 so the real renderer and the
    /// tripwire's clear-and-present share ONE classification of `CurrentSurfaceTexture`.
    /// Two copies of this match would be two places for `Occluded` to be quietly folded
    /// into either the success or the error bucket — and an occluded window masquerading as
    /// a clean measurement is precisely what 52-01's ablation had to rule out.
    ///
    /// Safe to call from a dedicated present thread — see the module's THREAD RULE.
    pub fn acquire(&self) -> Result<wgpu::SurfaceTexture, TimelineError> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => Ok(frame),
            // Suboptimal still yields a usable texture (a resize wants a reconfigure);
            // presenting it is correct and it is a real presented frame.
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => Ok(frame),
            wgpu::CurrentSurfaceTexture::Occluded => Err(TimelineError::Occluded),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                Err(TimelineError::DeviceLost)
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Validation => {
                Err(TimelineError::Present)
            }
        }
    }

    /// The adapter actually chosen, for the record.
    pub fn adapter_name(&self) -> String {
        self.adapter.get_info().name
    }

    /// Reconfigure for a new panel size.
    ///
    /// **Call on the panel's UI thread** — reconfiguring reaches `SetSwapChain` too.
    ///
    /// `scale` is threaded here for a reason the attach path cannot cover: a DPI
    /// change (a monitor move, or the user changing the display scale) arrives as
    /// a RESIZE carrying a NEW composition scale, never as a re-attach. A fix
    /// applied only on attach leaves the bug alive on every one of those.
    /// `rudis_timeline_resize` has always received this value from C# and used to
    /// bind it straight to the discard pattern.
    pub fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) {
        self.config.width = width_px.max(1);
        self.config.height = height_px.max(1);
        self.surface.configure(&self.device, &self.config);
        // Re-applied after EVERY reconfiguration, with the scale THIS resize
        // carries. Setting it twice with the same value is harmless; not setting
        // it after a DPI change is D1/D2/D3 all over again.
        if let Err(e) = apply_composition_scale(&self.surface, scale) {
            crate::trace(&format!("resize:composition-scale-not-applied err={e}"));
            eprintln!("[rudis_timeline] composition-scale transform not re-applied: {e}");
        }
    }

    /// Clear to `rgba` and present exactly one frame.
    ///
    /// The minimal draw on purpose: the instanced-quad renderer belongs to plan 52-04.
    /// What this proves is the hosting link, and a clear-and-present exercises the whole
    /// of it (acquire → encode → submit → present).
    ///
    /// Safe to call from a dedicated present thread — see the module's THREAD RULE.
    pub fn present_solid(&self, rgba: [f64; 4]) -> Result<(), TimelineError> {
        // wgpu 29 replaced the old `Result<SurfaceTexture, SurfaceError>` with an enum
        // that separates "acquired, but reconfigure soon" from the real failures. The
        // classification lives in `acquire` so the tripwire and the real renderer cannot
        // drift apart on what counts as a presented frame.
        let frame = self.acquire()?;

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rudis-timeline-clear"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rudis-timeline-clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: rgba[0],
                            g: rgba[1],
                            b: rgba[2],
                            a: rgba[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// The instance this surface was created from — kept accessible so a future caller
    /// cannot accidentally mix instances (wgpu panics with "Surface does not exist" if a
    /// surface and its adapter come from different instances).
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }
}

/// **THE STRUCTURAL GATE for D1/D2/D3.** Walk `src` for every swapchain
/// reconfiguration and report the ones that are NOT followed by a
/// composition-scale compensation.
///
/// Returns `(how many configure sites were found, the 1-based line numbers of
/// the ones with no transform within [`TRANSFORM_WINDOW_LINES`] lines after)`.
///
/// # Why a SOURCE SCAN and not a behavioural test
///
/// Every real failure path here lives inside `wgpu` and needs a live XAML panel
/// plus a DX12 adapter to reach — a rig, not a test (the same wall
/// `attach_releases_the_qi_reference_on_every_post_qi_failure` documents). The
/// transform's RUNTIME correctness is proven by 52-11's pixel-vs-model gate
/// (`shell/Rudis.Shell.UiTests/SurfaceGeometryTests.cs`); what is proven HERE is
/// its STRUCTURAL correctness — that no path reconfigures this swapchain without
/// compensating it. Phase 52 shipped ten plans with exactly that omission.
///
/// Both needles are spelled in two pieces because this scan reads its own file:
/// a needle written whole would match its own definition and the gate would
/// report a site it had manufactured (the use-detector trap `abi.rs` records,
/// which caught its own gate on the first run).
#[cfg(test)]
fn configure_sites_missing_the_transform(src: &str) -> (usize, Vec<usize>) {
    let configure_needle = concat!(".config", "ure(");
    let transform_needle = concat!("composition_", "scale");

    let lines: Vec<&str> = src.lines().collect();
    let mut found = 0usize;
    let mut violations = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if !line.contains(configure_needle) {
            continue;
        }
        found += 1;
        let end = (idx + 1 + TRANSFORM_WINDOW_LINES).min(lines.len());
        let compensated = lines[idx + 1..end]
            .iter()
            .any(|later| later.contains(transform_needle));
        if !compensated {
            violations.push(idx + 1);
        }
    }
    (found, violations)
}

/// How far after a reconfiguration the compensation is allowed to be. Wide
/// enough for a comment block explaining WHY the call is there (which every one
/// of them carries), narrow enough that "somewhere later in the function" does
/// not count.
#[cfg(test)]
const TRANSFORM_WINDOW_LINES: usize = 12;

/// Does the function whose signature starts at `needle` declare a `scale`?
///
/// Cheap, and it is the thing that silently regressed once already: `abi.rs`
/// RECEIVED the composition scale from C# on both the attach and the resize path
/// and bound it straight to the discard pattern, so the ABI looked complete while
/// the renderer had never been told what scale it was drawing at.
///
/// (The discard pattern is spelled out nowhere in this crate any more. The
/// acceptance gate for plan 52-12 counts its occurrences and expects zero, and a
/// gate that reddens on its own rationale gets the rationale deleted rather than
/// the gate fixed — 52-02 and 52-11 both recorded that, so this comment describes
/// the token instead of quoting it.)
#[cfg(test)]
fn signature_declares_a_scale(src: &str, needle: &str) -> bool {
    let Some(at) = src.find(needle) else {
        return false;
    };
    src[at..]
        .lines()
        .take(8)
        .any(|line| line.contains(concat!("scale: ", "f32")))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, Ordering};

    // =====================================================================
    // 52-12 — every path that configures this swapchain also compensates it
    // =====================================================================

    /// **THE GATE.** A `configure` without the composition-scale transform is a
    /// `scale`x magnified surface on which every existing assertion still
    /// passes — measured twice in this repository (51-04's Preview, 52-04's
    /// Timeline) and found by eye both times.
    #[test]
    fn every_configure_is_followed_by_a_composition_scale_call() {
        let src = include_str!("surface.rs");
        let (found, violations) = configure_sites_missing_the_transform(src);

        assert_eq!(
            found, 2,
            "expected exactly 2 swapchain reconfiguration sites in this file (attach and \
             resize); found {found}. If a third was added deliberately, it needs the \
             transform too — that is what this gate is for."
        );
        assert!(
            violations.is_empty(),
            "swapchain reconfiguration at line(s) {violations:?} of surface.rs is NOT followed \
             within {TRANSFORM_WINDOW_LINES} lines by a composition-scale call.\n\
             \n\
             A SwapChainPanel composites ONE BUFFER PIXEL PER DIP. This crate sizes its buffer \
             in PHYSICAL pixels, so without `IDXGISwapChain2::SetMatrixTransform` carrying the \
             inverse scale, everything drawn is magnified by `scale` about the panel's \
             top-left while the pointer path stays correct — hit test and pixels then disagree \
             MULTIPLICATIVELY. That is exactly D1/D2/D3 of the Phase-52 UAT, and it passed 5/5 \
             machine criteria and two GREEN interaction suites. Call \
             `apply_composition_scale` after this line."
        );
    }

    /// **THE CAN-FAIL COMPANION.** Without it, "0 violations" and "the parser
    /// matched nothing" are the same reading — and a parser that matches nothing
    /// is precisely how a magnified renderer passes its own gate.
    ///
    /// Runs the SAME function over a fixture that is the pre-fix shape: a
    /// reconfiguration with nothing after it.
    #[test]
    fn the_configure_gate_can_fail() {
        // Assembled from pieces for the same reason the parser's needles are:
        // written whole, this fixture would be a real, uncompensated site in
        // this file and the gate above would redden on its own companion.
        let fixture = format!(
            "fn attach_without_the_fix() {{\n    let config = todo!();\n    \
             surface{}&device, &config);\n    Ok(())\n}}\n",
            concat!(".config", "ure(")
        );

        let (found, violations) = configure_sites_missing_the_transform(&fixture);
        assert_eq!(found, 1, "the parser must SEE the reconfiguration");
        assert_eq!(
            violations,
            vec![3],
            "and must report it as uncompensated, naming its line"
        );
    }

    /// Both thread the scale. A fix on only the attach path leaves the bug alive
    /// on every window resize and on every monitor move (a DPI change arrives as
    /// a resize carrying a NEW scale).
    #[test]
    fn attach_and_resize_both_take_a_scale() {
        let src = include_str!("surface.rs");
        assert!(
            signature_declares_a_scale(src, concat!("pub fn at", "tach(")),
            "TimelineSurface::attach must take the panel's composition scale"
        );
        assert!(
            signature_declares_a_scale(src, concat!("pub fn re", "size(")),
            "TimelineSurface::resize must take the panel's composition scale — \
             rudis_timeline_resize already RECEIVES it from C# and used to discard it"
        );
    }

    // =====================================================================
    // 52-REVIEW WR-01 — the QI'd reference is given back on every failure
    // =====================================================================
    //
    // WHAT MADE THIS UNTESTED FOR A PHASE. `TimelineSurface::attach`'s four post-QI
    // failure points are all INSIDE wgpu, so reaching any of them from a unit test
    // would need a real `SwapChainPanel` (a XAML object, so a window, so a UI thread)
    // AND a DX12 adapter that then fails on cue. That is not a test, it is a rig.
    //
    // So this file tests the two halves separately and honestly:
    //
    //   * the REFERENCE ARITHMETIC, against a real COM object with a real vtable —
    //     `query_swap_chain_panel_native`'s actual `QueryInterface` runs against it and
    //     the refcount it moves is one these tests can read; and
    //   * the REAL `attach`, with a `#[cfg(test)]`-only injected failure at the first
    //     post-QI return point, so what is under test is the shipping function's
    //     ownership handling and not a look-alike written for the occasion.
    //
    // ...plus `the_refcount_instrument_can_observe_a_leak`, because "releases == 1" and
    // "the counter is not wired" are otherwise the same reading.

    thread_local! {
        static INJECTED_POST_QI_FAILURE: Cell<Option<TimelineError>> = Cell::new(None);
    }

    /// Read and clear the injected failure. Called by `attach` under `#[cfg(test)]`.
    pub(super) fn take_injected_post_qi_failure() -> Option<TimelineError> {
        INJECTED_POST_QI_FAILURE.with(|slot| slot.take())
    }

    fn inject_post_qi_failure(err: TimelineError) {
        INJECTED_POST_QI_FAILURE.with(|slot| slot.set(Some(err)));
    }

    /// A hand-rolled COM object with a REAL `ISwapChainPanelNative` vtable whose slots
    /// count what is done to them.
    ///
    /// Not a mock of anything in this crate: the point is that the production
    /// `QueryInterface` / `Release` calls run against a genuine vtable, so a refcount
    /// assertion below is an assertion about the code's actual COM behaviour rather than
    /// about a stand-in that was written to agree with it.
    #[repr(C)]
    struct FakePanel {
        /// MUST be first: `release_panel_native` reads the vtable pointer from offset 0.
        vtbl: *const ISwapChainPanelNativeVtbl,
        refcount: AtomicU32,
        add_refs: AtomicU32,
        releases: AtomicU32,
        qi_succeeds: bool,
    }

    /// `E_NOINTERFACE`.
    const E_NOINTERFACE: i32 = -2147467262; // 0x8000_4002

    unsafe extern "system" fn fake_query_interface(
        this: *mut c_void,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> i32 {
        let me = &*(this as *const FakePanel);
        if !me.qi_succeeds || *iid != IID_ISWAPCHAINPANELNATIVE {
            *out = std::ptr::null_mut();
            return E_NOINTERFACE;
        }

        // A successful QI addrefs — that is the whole reason WR-01 was a leak.
        me.refcount.fetch_add(1, Ordering::SeqCst);
        me.add_refs.fetch_add(1, Ordering::SeqCst);
        *out = this;
        0
    }

    unsafe extern "system" fn fake_add_ref(this: *mut c_void) -> u32 {
        let me = &*(this as *const FakePanel);
        me.add_refs.fetch_add(1, Ordering::SeqCst);
        me.refcount.fetch_add(1, Ordering::SeqCst) + 1
    }

    unsafe extern "system" fn fake_release(this: *mut c_void) -> u32 {
        let me = &*(this as *const FakePanel);
        me.releases.fetch_add(1, Ordering::SeqCst);
        me.refcount.fetch_sub(1, Ordering::SeqCst) - 1
    }

    unsafe extern "system" fn fake_set_swap_chain(_this: *mut c_void, _chain: *mut c_void) -> i32 {
        0
    }

    static FAKE_VTBL: ISwapChainPanelNativeVtbl = ISwapChainPanelNativeVtbl {
        query_interface: fake_query_interface,
        add_ref: fake_add_ref,
        release: fake_release,
        set_swap_chain: fake_set_swap_chain,
    };

    impl FakePanel {
        /// Refcount 1 on creation, like any COM object handed to a caller.
        fn new(qi_succeeds: bool) -> Box<Self> {
            Box::new(Self {
                vtbl: &FAKE_VTBL,
                refcount: AtomicU32::new(1),
                add_refs: AtomicU32::new(0),
                releases: AtomicU32::new(0),
                qi_succeeds,
            })
        }

        fn as_com_ptr(&self) -> *mut c_void {
            self as *const FakePanel as *mut c_void
        }
    }

    /// THE FINDING. Every post-QI failure must give the reference back — not three of
    /// four, not "usually".
    #[test]
    fn attach_releases_the_qi_reference_on_every_post_qi_failure() {
        for err in [
            TimelineError::SurfaceCreation,
            TimelineError::NoAdapter,
            TimelineError::NoDevice,
            TimelineError::NoSurfaceFormat,
        ] {
            let panel = FakePanel::new(true);
            let ptr = panel.as_com_ptr();

            inject_post_qi_failure(err);
            let result = TimelineSurface::attach(ptr, 8, 8, 1.0);

            assert_eq!(result.err(), Some(err), "the injected failure must surface");
            assert_eq!(
                panel.add_refs.load(Ordering::SeqCst),
                1,
                "the QI must have taken exactly one reference ({err:?})"
            );
            assert_eq!(
                panel.releases.load(Ordering::SeqCst),
                1,
                "...and the failure path must give exactly that one back ({err:?}). \
                 Pre-fix this read 0 — the WR-01 leak."
            );
            assert_eq!(
                panel.refcount.load(Ordering::SeqCst),
                1,
                "the panel is left at the refcount it arrived with ({err:?})"
            );
        }
    }

    /// A pointer that is not a `SwapChainPanel` takes NOTHING, so there is nothing to
    /// give back — the symmetric case, asserted so a future "release on every error
    /// path" edit cannot over-release a reference it never held.
    #[test]
    fn a_failed_qi_neither_takes_nor_releases_a_reference() {
        let panel = FakePanel::new(false);
        let ptr = panel.as_com_ptr();

        assert_eq!(
            TimelineSurface::attach(ptr, 8, 8, 1.0).err(),
            Some(TimelineError::NotASwapChainPanel)
        );
        assert_eq!(panel.add_refs.load(Ordering::SeqCst), 0);
        assert_eq!(panel.releases.load(Ordering::SeqCst), 0);
        assert_eq!(panel.refcount.load(Ordering::SeqCst), 1);
    }

    /// A null panel is rejected before the guard exists at all.
    #[test]
    fn attach_rejects_null_before_acquiring_anything() {
        assert_eq!(
            TimelineSurface::attach(std::ptr::null_mut(), 8, 8, 1.0).err(),
            Some(TimelineError::NullPanel)
        );
    }

    /// The guard's two exits, directly: dropping releases, `into_raw` hands on.
    ///
    /// The second half is the one that would turn a leak fix into a use-after-free if it
    /// were wrong — a guard that released on the SUCCESS path too would hand
    /// `TimelineSurface` a pointer whose reference had already been given back, and
    /// `rudis_timeline_detach` would then release it a second time.
    #[test]
    fn the_guard_releases_on_drop_and_hands_on_via_into_raw() {
        let panel = FakePanel::new(true);
        let ptr = panel.as_com_ptr();

        {
            let guard = PanelNativeRef::acquire(ptr).expect("the fake QIs successfully");
            assert_eq!(guard.as_ptr(), ptr);
            assert_eq!(panel.refcount.load(Ordering::SeqCst), 2, "the QI took one");
            assert_eq!(panel.releases.load(Ordering::SeqCst), 0, "still held");
        }

        assert_eq!(panel.releases.load(Ordering::SeqCst), 1, "drop gave it back");
        assert_eq!(panel.refcount.load(Ordering::SeqCst), 1);

        let guard = PanelNativeRef::acquire(ptr).expect("the fake QIs successfully");
        let raw = guard.into_raw();

        assert_eq!(raw, ptr);
        assert_eq!(
            panel.releases.load(Ordering::SeqCst),
            1,
            "into_raw hands the reference ON — releasing here would double-free at detach"
        );
        assert_eq!(panel.refcount.load(Ordering::SeqCst), 2);

        // ...and whoever took it now owns it, exactly as `rudis_timeline_detach` does.
        unsafe { release_panel_native(raw) };
        assert_eq!(panel.refcount.load(Ordering::SeqCst), 1);
    }

    /// THE CAN-FAIL COMPANION. The pre-fix shape, in two lines, measured by the same
    /// instrument: QI and return. Without this, every `releases == 1` above is
    /// indistinguishable from a counter that never moves.
    #[test]
    fn the_refcount_instrument_can_observe_a_leak() {
        let panel = FakePanel::new(true);
        let ptr = panel.as_com_ptr();

        let leaked = query_swap_chain_panel_native(ptr).expect("the fake QIs successfully");

        assert_eq!(panel.add_refs.load(Ordering::SeqCst), 1);
        assert_eq!(
            panel.releases.load(Ordering::SeqCst),
            0,
            "nothing released — this is what the pre-fix error paths did"
        );
        assert_eq!(
            panel.refcount.load(Ordering::SeqCst),
            2,
            "the panel is pinned one higher than it arrived: the WR-01 leak, observed"
        );

        unsafe { release_panel_native(leaked) };
        assert_eq!(panel.refcount.load(Ordering::SeqCst), 1);
    }

    /// If the IID ever drifts from the one both WinUI 3 and wgpu-hal use, this constant
    /// is where the drift must be noticed — the same pin the 44 spike keeps.
    #[test]
    fn swapchain_panel_native_iid_is_the_winui3_one() {
        assert_eq!(
            format!("{:?}", IID_ISWAPCHAINPANELNATIVE).to_lowercase(),
            "63aad0b8-7c24-40ff-85a8-640d944cc325"
        );
    }

    /// A null pointer is a CODE, never a dereference.
    #[test]
    fn null_panel_is_rejected_before_any_wgpu_call() {
        assert_eq!(
            query_swap_chain_panel_native(std::ptr::null_mut()),
            Err(TimelineError::NullPanel)
        );
    }

    /// Every error maps to a distinct negative code — a collision would make a tripwire
    /// failure ambiguous at exactly the moment precision matters.
    #[test]
    fn error_codes_are_distinct_and_negative() {
        let all = [
            TimelineError::NullPanel,
            TimelineError::NotASwapChainPanel,
            TimelineError::SurfaceCreation,
            TimelineError::NoAdapter,
            TimelineError::NoDevice,
            TimelineError::NoSurfaceFormat,
            TimelineError::Present,
            TimelineError::DeviceLost,
            TimelineError::Occluded,
        ];
        let mut codes: Vec<i32> = all.iter().map(|e| e.code()).collect();
        assert!(codes.iter().all(|c| *c < 0));
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
    }
}
