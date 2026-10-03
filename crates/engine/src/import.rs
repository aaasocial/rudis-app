//! Zero-copy D3D11→D3D12 import of hardware-decoded frames onto the
//! compositor's own `wgpu` device (Phase 48, GPU-01 — the `GpuFrame` type every
//! downstream plan consumes).
//!
//! Ported near-verbatim from the first-party, spike-proven
//! `spikes/44-hwaccel/src/import.rs` (SPIKE-02, Phase 44 — MAD 0.0000 in
//! 0 of 921 600 bytes against both `av_hwframe_transfer_data` and the
//! production sidecar, on the exact version pins this crate now uses).
//!
//! ## The audited import path
//!
//! This module plus [`crate::hwdecode`] together ARE the audited import path.
//!
//! > **No CPU pixel transfer of any kind may appear in these two files.** No
//! > buffer mapping, no CPU-side texture upload, no staging/readback resource
//! > creation. Every byte-level inspection of an imported frame lives in the
//! > VERIFICATION-ONLY test helpers (`tests/hwdecode_zero_copy.rs`) and the
//! > live-object evidence example, both excluded from the audit by design.
//!
//! `tests/hwdecode_zero_copy.rs` enforces exactly that by `include_str!`-ing
//! both files and asserting the three forbidden call names are absent AND the
//! four load-bearing import calls are present — a machine check on the real
//! source text, not a comment promising good behaviour.
//!
//! ## What actually happens here (SPIKE-02's proven rung-0 chain)
//!
//! ```text
//!   FFmpeg D3D11VA decode ──▶ ID3D11Texture2D (the WHOLE widened hw frame
//!                             pool array texture, NV12, this frame at slice N)
//!                                    │
//!                     IDXGIResource1::CreateSharedHandle
//!                                    │
//!                                NT HANDLE  (closed promptly after each open)
//!                                    │
//!               ID3D12Device::OpenSharedHandle ← wgpu's OWN raw ID3D12Device
//!                                    │
//!                             ID3D12Resource  (hard-asserted DEFAULT heap)
//!                                    │
//!       wgpu::hal::dx12::Device::texture_from_raw + create_texture_from_hal
//!                                    │
//!                     wgpu::Texture (NV12, depth_or_array_layers = the REAL
//!                     pool ArraySize — never a hardcoded count)
//!                                    │
//!         D2Array plane views: Plane0→R8Unorm luma, Plane1→Rg8Unorm chroma,
//!         base_array_layer = frame->data[1]
//! ```
//!
//! Cross-API ordering is a real D3D11↔D3D12 shared fence, never a sleep: a
//! `D3D12_FENCE_FLAG_SHARED` fence created on wgpu's own `ID3D12Device`,
//! opened on the D3D11 side with `ID3D11Device5::OpenSharedFence`, signalled by
//! `ID3D11DeviceContext4::Signal` once the decode is submitted, and waited on
//! by `ID3D12CommandQueue::Wait` on the exact queue wgpu submits through. The
//! CPU-stall fallback is kept implemented-but-recorded exactly as the spike had
//! it (a stall is not a copy, but it is weaker, so which mechanism ran is never
//! glossed).
//!
//! ## The rung-1 escape hatch (documented, not implemented)
//!
//! Phase 44 also measured a fallback: one GPU-side `CopySubresourceRegion` of
//! the frame's slice into a fresh single-slice shareable NV12 texture, shared
//! and opened the same way (zero CPU copies, one GPU copy) — see
//! `spikes/44-hwaccel/src/import.rs::blit_to_shareable`. Rung 0 was a CLEAN GO
//! on this stack, so production ships rung 0 only; the blit shape stays
//! recorded there as the escape hatch (48-CONTEXT.md § VRAM Budget), alongside
//! `composite_to_rgba` on the CPU side.

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
};
use windows::Win32::Graphics::Direct3D12::{
    ID3D12Fence, ID3D12Resource, D3D12_FENCE_FLAG_SHARED, D3D12_HEAP_FLAGS, D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_TYPE_READBACK, D3D12_HEAP_TYPE_UPLOAD,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::PCWSTR;

use rsmpeg::ffi;

use crate::hwdecode::{HwDecodeSession, HwFrame};
use crate::EngineError;

/// How long the CPU-stall fence fallback waits before giving up (ms).
const FENCE_STALL_TIMEOUT_MS: u32 = 5000;

/// Format an HRESULT the way every Windows tool does.
pub fn hr(code: i32) -> String {
    format!("0x{:08X}", code as u32)
}

fn gpu_err(msg: impl Into<String>) -> EngineError {
    EngineError::Gpu(msg.into())
}

// ---------------------------------------------------------------------------
// Per-frame colour metadata (GPU-02's input — the matrix itself is built by
// the colorspace module, per-frame, never hardcoded).
// ---------------------------------------------------------------------------

/// The frame's colorspace tag, mapped from `AVFrame.colorspace`.
///
/// Only tags inside the SDR matrix this phase ships reach this point — the
/// decoder-open gate ([`crate::hwdecode::HwOpenError::UnsupportedColorspace`])
/// already routed everything else to software decode.
///
/// `Unspecified` is kept DISTINCT so downstream can log honestly; the locked
/// untagged-content rule (48-RESEARCH.md Pattern 2, VERIFIED against
/// libswscale source: no dimension check exists anywhere in the library) maps
/// it to BT.601-family coefficients unconditionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameColorspace {
    /// BT.601 family (`AVCOL_SPC_BT470BG` / `AVCOL_SPC_SMPTE170M` — same
    /// Kr/Kg/Kb: 0.299/0.587/0.114).
    Bt601,
    /// `AVCOL_SPC_BT709` (Kr/Kg/Kb: 0.2126/0.7152/0.0722).
    Bt709,
    /// Untagged. Decodes as BT.601-family (libswscale's unconditional
    /// default — cited, not guessed).
    Unspecified,
}

impl FrameColorspace {
    /// Map a raw `AVColorSpace` tag.
    pub fn from_av(csp: i32) -> Self {
        if csp == ffi::AVCOL_SPC_BT470BG || csp == ffi::AVCOL_SPC_SMPTE170M {
            FrameColorspace::Bt601
        } else if csp == ffi::AVCOL_SPC_BT709 {
            FrameColorspace::Bt709
        } else {
            FrameColorspace::Unspecified
        }
    }
}

/// The frame's sample range, mapped from `AVFrame.color_range`.
///
/// `Unspecified` decodes as limited/"tv" range — libswscale's unconditional
/// default for untagged content (`fmt_encode_range`, VERIFIED at 48-RESEARCH.md
/// Pattern 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameColorRange {
    /// `AVCOL_RANGE_MPEG` — limited/"tv" (Y 16..235, C 16..240 at 8-bit).
    Limited,
    /// `AVCOL_RANGE_JPEG` — full/"pc" (0..255).
    Full,
    /// Untagged. Decodes as limited (libswscale's unconditional default).
    Unspecified,
}

impl FrameColorRange {
    /// Map a raw `AVColorRange` tag.
    pub fn from_av(range: i32) -> Self {
        if range == ffi::AVCOL_RANGE_MPEG {
            FrameColorRange::Limited
        } else if range == ffi::AVCOL_RANGE_JPEG {
            FrameColorRange::Full
        } else {
            FrameColorRange::Unspecified
        }
    }
}

/// VRAM cost of one NV12 frame (luma plane + interleaved half-res chroma
/// plane = w*h*3/2 bytes). Used by the ring's budget math (plan 48-07).
pub fn frame_bytes_nv12(w: u32, h: u32) -> u64 {
    (w as u64 * h as u64 * 3) / 2
}

// ---------------------------------------------------------------------------
// The GpuFrame contract.
// ---------------------------------------------------------------------------

/// A hardware-decoded frame, GPU-resident and addressable by the compositor's
/// own `wgpu` device. THE type every downstream Phase-48 plan consumes.
///
/// **Lifetime discipline (48-RESEARCH.md Pattern 1):** the imported texture and
/// the ref-counted `AVFrame` keeper live in this ONE struct so they live and
/// die together. Dropping the keeper returns the frame's hw-frame-pool slice
/// to the decoder; dropping one without the other would either leak a GPU
/// handle or free a pool slice a view still points at. As long as a `GpuFrame`
/// is held (e.g. pinned in the preview ring), its pool slice is unavailable to
/// the decoder — which is exactly why the pool is widened at decoder-open.
pub struct GpuFrame {
    /// The imported texture — the WHOLE hw frame pool as a `D2` NV12 array
    /// texture (`depth_or_array_layers` = the real, widened pool ArraySize).
    pub texture: wgpu::Texture,
    /// `TextureAspect::Plane0` as `R8Unorm` — the luma plane. `D2Array` view
    /// dimension, bound as `texture_2d_array<f32>`.
    pub luma_view: wgpu::TextureView,
    /// `TextureAspect::Plane1` as `Rg8Unorm` — the interleaved chroma plane.
    pub chroma_view: wgpu::TextureView,
    /// Which array slice holds this frame (`frame->data[1]`). NOT 0 in
    /// general — Phase 44 observed slice 12.
    pub base_array_layer: u32,
    /// Array layers the imported texture was described with (= the widened
    /// pool size, read from the real texture desc).
    pub array_layers: u32,
    /// DISPLAY width in pixels (`AVFrame.width` — the texture itself may be
    /// wider: the hw frame pool is allocated at macroblock-aligned coded size).
    pub width: u32,
    /// DISPLAY height in pixels (`AVFrame.height`).
    pub height: u32,
    /// Per-frame colorspace tag (GPU-02: the shader matrix is CPU-selected
    /// from THIS, never hardcoded).
    pub colorspace: FrameColorspace,
    /// Per-frame sample-range tag.
    pub color_range: FrameColorRange,
    /// Presentation timestamp in microseconds (`None` if the stream carried none).
    pub pts_us: Option<i64>,
    /// Which fence mechanism ordered the cross-API handoff: `"queue_wait"`
    /// (`ID3D12CommandQueue::Wait`, the proven mechanism) or `"cpu_stall"`
    /// (the recorded fallback — a CPU STALL, never a CPU copy).
    pub fence_mechanism: &'static str,

    // --- lifetime anchors, dropped together with the texture ---
    /// The ref-counted `AVFrame` keeper: holds the pool slice.
    hw_frame: HwFrame,
    /// COM refs on the SESSION-lifetime shared fence pair (48-gpu-oom-4k fix:
    /// one fence pair per session, not per frame — these are AddRef'd clones
    /// of the [`PoolImportCache`]'s fences, held so an in-flight frame keeps
    /// the ordering objects alive even if the session drops first).
    _fence12: ID3D12Fence,
    _fence11: ID3D11Fence,
}

impl GpuFrame {
    /// The underlying hardware frame (metadata / evidence access — e.g. the
    /// live-object capture reads the decode device off it). Moves no pixels.
    pub fn hw_frame(&self) -> &HwFrame {
        &self.hw_frame
    }

    /// A handle clone of this GPU-resident frame (plan 48-08, ADDITIVE): the
    /// enabler for the sink's store-what-you-present contract
    /// (`PresentSink::present_gpu` takes `&GpuFrame` and must remember the
    /// frame it showed as "current" for the paused-only readback).
    ///
    /// **Refcount bumps only — zero pixel work**: the `wgpu` texture/views are
    /// internally `Arc`'d handle clones of the SAME imported resource, the two
    /// fences are COM `AddRef`s, and the `AVFrame` keeper is `av_frame_clone`
    /// (a new ref on the SAME hw-frame-pool slice — no new slice is consumed).
    /// Each clone is itself a whole `GpuFrame`, so the drop-together lifetime
    /// contract holds per-instance: the pool slice returns to the decoder only
    /// when EVERY clone (and its texture handle) has dropped.
    ///
    /// Fallible because `av_frame_clone` allocates a frame header; on
    /// allocation failure nothing was cloned and nothing leaks.
    pub fn try_clone(&self) -> Result<GpuFrame, EngineError> {
        Ok(GpuFrame {
            texture: self.texture.clone(),
            luma_view: self.luma_view.clone(),
            chroma_view: self.chroma_view.clone(),
            base_array_layer: self.base_array_layer,
            array_layers: self.array_layers,
            width: self.width,
            height: self.height,
            colorspace: self.colorspace,
            color_range: self.color_range,
            pts_us: self.pts_us,
            fence_mechanism: self.fence_mechanism,
            hw_frame: self.hw_frame.try_clone()?,
            _fence12: self._fence12.clone(),
            _fence11: self._fence11.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// The session import cache (debug session 48-gpu-oom-4k-preview-present-panic).
// ---------------------------------------------------------------------------

/// The ONE whole-pool D3D12 open + wgpu texture + shared fence pair for a
/// decode session, built on the FIRST import and reused for every later frame.
///
/// **Why this exists (measured 2026-07-29,
/// `.planning/debug/48-gpu-oom-4k-preview-present-panic.md`):** Windows
/// charges EVERY `ID3D12Device::OpenSharedHandle` open of the shared pool
/// array SEPARATELY against the process's DXGI VRAM budget — each open is its
/// own resident KM allocation reference, ~the WHOLE pool's bytes (~463 MB at
/// 4K), even though the physical allocation exists once. The pre-fix code
/// opened the pool PER DECODED FRAME, so a ring holding 32 GpuFrames at 4K
/// counted ~15 GB against a ~7.6 GB budget and the OS eventually denied an
/// unrelated small allocation on the present thread. Opening ONCE per session
/// makes a held ring entry cost what the budget math says it costs (a pool
/// slice), not a whole-pool open.
///
/// Keyed by BOTH the source `ID3D11Texture2D` pointer (a session's pool can in
/// principle be re-negotiated mid-stream) and the raw `ID3D12Device` pointer
/// (device-lost recovery recreates the wgpu device; a stale-device cache must
/// re-open on the fresh device, never answer from the dead one). A key
/// mismatch drops the old cache (releasing its open) and rebuilds.
///
/// The shared fence pair is likewise session-lifetime: the D3D11 side signals
/// a MONOTONICALLY INCREASING value per frame and the D3D12 queue waits on
/// that exact value — same ordering guarantee as the old per-frame fence
/// (each frame's wait is satisfied only by its own signal), minus a per-frame
/// `CreateFence`/`CreateSharedHandle`/`OpenSharedFence` syscall round-trip.
pub(crate) struct PoolImportCache {
    /// Identity of the D3D11 pool array texture this cache was opened from.
    src_texture_ptr: usize,
    /// Identity of the raw `ID3D12Device` the open landed on.
    device_ptr: usize,
    /// The one wgpu texture wrapping the whole pool array on that device.
    texture: wgpu::Texture,
    /// The pool ArraySize the texture was described with.
    array_layers: u32,
    /// Session-lifetime shared fence, D3D12 side (owner).
    fence12: ID3D12Fence,
    /// Session-lifetime shared fence, D3D11 side (opened from `fence12`).
    fence11: ID3D11Fence,
    /// The value the NEXT frame's signal/wait handshake uses. Starts at 1,
    /// strictly increasing — a wait on value N completes only once the D3D11
    /// side has signalled N.
    next_fence_value: u64,
}

/// Build the session cache: every SPIKE-02 hard assert runs HERE, once per
/// (pool texture × device) instead of once per frame — the asserts guard the
/// RESOURCE (adapter identity, heap placement), which does not change
/// per frame. Any failure leaves the cache empty; nothing is leaked (the NT
/// handles are closed inside the helpers, and the fence/resource COM refs
/// drop with the locals).
fn build_pool_import_cache(
    compositor_device: &wgpu::Device,
    hw_frame: &HwFrame,
    texture: &ID3D11Texture2D,
    decoder_desc: &crate::hwdecode::TextureDesc,
    src_texture_ptr: usize,
    device_ptr: usize,
) -> Result<PoolImportCache, EngineError> {
    // --- 1. Same-adapter assertion (Phase 44 Pitfall 4 — assert, never assume) --
    let d3d11_luid = d3d11_adapter_luid(hw_frame.device())?;
    let d3d12_luid = {
        let hal_device = unsafe { compositor_device.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or_else(|| gpu_err("compositor device exposes no DX12 hal device — not DX12?"))?;
        let luid = unsafe { hal_device.raw_device().GetAdapterLuid() };
        ((luid.HighPart as i64) << 32) | (luid.LowPart as i64)
    };
    if d3d11_luid != d3d12_luid {
        return Err(gpu_err(format!(
            "cross-adapter mismatch: the decoder is on {} but wgpu's DX12 device is on {} — a \
             shared handle cannot cross adapters (two DX12 adapters enumerate on this machine: \
             the hardware GPU and WARP)",
            luid_hex(d3d11_luid),
            luid_hex(d3d12_luid)
        )));
    }

    // --- 2. The cross-API shared fence ---------------------------------------
    // Created on wgpu's device (the D3D12 side owns it) and opened on FFmpeg's
    // D3D11 device — exactly the direction ID3D11Device5::OpenSharedFence
    // documents. The NT handle is closed as soon as the open consumed it.
    let (fence12, fence11) = create_shared_fence(compositor_device, hw_frame.device())?;

    // --- 3 + 4. Acquire a shared handle, then open it on wgpu's ID3D12Device --
    // ONCE per session: this open is what the OS charges the whole pool's
    // bytes for (see the type doc). It must never run per frame.
    let d3d12_resource = share_and_open(texture, compositor_device)?;

    // --- 5. Heap placement: the zero-copy hard assert -------------------------
    // Fail CLOSED, in both directions. An UNQUERYABLE heap is "unverified",
    // not "fine": this check is the load-bearing evidence for the zero-copy
    // claim, so it must be able to say "unknown" — and refuse — rather than
    // only ever saying "not disproven".
    let (heap_type, heap_query_hresult) = read_heap_type(&d3d12_resource);
    if heap_query_hresult != 0 {
        return Err(gpu_err(format!(
            "ID3D12Resource::GetHeapProperties failed ({}) — the imported resource's heap \
             placement could NOT be verified; refusing to report a zero-copy import on \
             unverified evidence",
            hr(heap_query_hresult)
        )));
    }
    if heap_type != D3D12_HEAP_TYPE_DEFAULT.0 {
        let name = if heap_type == D3D12_HEAP_TYPE_UPLOAD.0 {
            "D3D12_HEAP_TYPE_UPLOAD (CPU-visible!)"
        } else if heap_type == D3D12_HEAP_TYPE_READBACK.0 {
            "D3D12_HEAP_TYPE_READBACK (CPU-visible!)"
        } else {
            "D3D12_HEAP_TYPE_CUSTOM/unknown"
        };
        return Err(gpu_err(format!(
            "the imported resource is NOT in a GPU-local DEFAULT heap ({name}) — a READBACK or \
             UPLOAD heap here would mean the pixels were routed through CPU-visible memory, i.e. \
             a hidden CPU copy. That is not a zero-copy import and must not silently degrade \
             into one (T-48-05-04)"
        )));
    }

    // --- 6. Wrap the raw D3D12 resource as a wgpu texture --------------------
    // The hw frame pool is a whole ARRAY texture; describing it as a single
    // slice would be a lie the driver might not catch. Describe the real shape
    // (the texture desc's own dimensions and ArraySize — never a hardcoded
    // count) and address each frame's slice at view time.
    let array_layers = decoder_desc.array_size;
    let size = wgpu::Extent3d {
        width: decoder_desc.width,
        height: decoder_desc.height,
        depth_or_array_layers: array_layers,
    };
    let hal_texture = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(
            d3d12_resource,
            wgpu::TextureFormat::NV12,
            wgpu::TextureDimension::D2,
            size,
            1, // mip_level_count
            1, // sample_count
        )
    };
    let wgpu_texture = unsafe {
        compositor_device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("imported-d3d11va-frame"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12,
                // TEXTURE_BINDING only: NV12's guaranteed allowed usages in
                // wgpu-types 26.0.0 are exactly `TextureUsages::TEXTURE_BINDING`
                // ("We only support sampling nv12 textures until we implement
                // transfer plane data" — wgpu-types-26.0.0/src/lib.rs:3026).
                // Claiming COPY_SRC here would be describing a capability the
                // format does not have; consumers sample the plane views.
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm],
            },
        )
    };

    Ok(PoolImportCache {
        src_texture_ptr,
        device_ptr,
        texture: wgpu_texture,
        array_layers,
        fence12,
        fence11,
        next_fence_value: 1,
    })
}

// ---------------------------------------------------------------------------
// The import.
// ---------------------------------------------------------------------------

/// Import `hw_frame`'s hardware texture onto the compositor's own DX12 `wgpu`
/// device with **no CPU copy** (SPIKE-02's rung-0 chain).
///
/// `compositor_device`/`compositor_queue` MUST be the device/queue the
/// compositor renders with (DX12-pinned since SPIKE-05) and the device must
/// have `wgpu::Features::TEXTURE_FORMAT_NV12` enabled.
///
/// **Cost discipline (48-gpu-oom-4k fix, 2026-07-29):** the whole-pool
/// `OpenSharedHandle` + wgpu texture wrap + shared-fence creation happen ONCE
/// per session, cached in [`PoolImportCache`] on the session; per frame this
/// function only performs the fence signal/wait handshake (monotone value on
/// the session fence) and creates the two plane VIEWS at this frame's slice.
/// Every open used to be charged the whole pool's bytes against the process
/// VRAM budget — per-frame opens made a 4K ring cost ~37× what the budget
/// math modeled and OOM'd the present thread.
///
/// Every SPIKE-02 hard assert is intact (now once per session, in
/// [`build_pool_import_cache`] — they guard the resource, which does not
/// change per frame):
/// - **same-adapter LUID assert** — two DX12 adapters enumerate on the dev
///   machine (RTX 3070 + WARP), so "it happened to be the same one" is a real
///   failure mode, never assumed away;
/// - **`D3D12_HEAP_TYPE_DEFAULT` hard assert** — fails CLOSED in BOTH
///   directions (a failed heap query is "unverified", not "fine");
/// - **whole-pool import** — `depth_or_array_layers` is the REAL pool
///   ArraySize; the frame is addressed at `base_array_layer = frame->data[1]`;
/// - **`D2Array` plane views only** — wgpu-hal's DX12 backend emits a
///   `TEXTURE2DARRAY` SRV whenever `base_array_layer != 0`, so a `D2` view
///   would leave the binding shape depending on a coincidence;
/// - **handle lifetime discipline** (threat T-48-05-03) — every NT handle is
///   closed promptly after its open and never reused.
pub fn import_frame(
    compositor_device: &wgpu::Device,
    compositor_queue: &wgpu::Queue,
    session: &HwDecodeSession,
    hw_frame: HwFrame,
) -> Result<GpuFrame, EngineError> {
    let texture = hw_frame
        .texture()
        .ok_or_else(|| gpu_err("frame->data[0] is not an ID3D11Texture2D"))?;
    let decoder_desc = hw_frame
        .desc()
        .ok_or_else(|| gpu_err("could not read the decoder texture's D3D11_TEXTURE2D_DESC"))?;
    let array_index = hw_frame.array_index() as u32;

    if !compositor_device
        .features()
        .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
    {
        return Err(gpu_err(
            "the compositor device was created without wgpu::Features::TEXTURE_FORMAT_NV12 — \
             the NV12 import cannot be described to wgpu on this device",
        ));
    }

    // Cross-check against the session's authoritative widened pool size — an
    // inconsistency here means the frame came from a different pool than the
    // session negotiated.
    if session.pool_size() != 0 && decoder_desc.array_size as usize != session.pool_size() {
        return Err(gpu_err(format!(
            "hw frame pool ArraySize ({}) disagrees with the session's recorded pool size ({})",
            decoder_desc.array_size,
            session.pool_size()
        )));
    }

    // --- resolve the session cache (build on first import / key mismatch) ----
    let src_texture_ptr = texture.as_raw() as usize;
    let device_ptr = {
        let hal_device = unsafe { compositor_device.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or_else(|| gpu_err("compositor device exposes no DX12 hal device — not DX12?"))?;
        hal_device.raw_device().as_raw() as usize
    };
    let mut cache_slot = session
        .import_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let cache_valid = matches!(
        &*cache_slot,
        Some(c) if c.src_texture_ptr == src_texture_ptr && c.device_ptr == device_ptr
    );
    if !cache_valid {
        // Drop any stale open FIRST (releases the old whole-pool charge),
        // then build against the current pool texture / device.
        *cache_slot = None;
        *cache_slot = Some(build_pool_import_cache(
            compositor_device,
            &hw_frame,
            &texture,
            &decoder_desc,
            src_texture_ptr,
            device_ptr,
        )?);
    }
    let cache = cache_slot
        .as_mut()
        .expect("import cache was just ensured above");

    // --- per-frame: order the two APIs on the session fence -------------------
    // Signal on D3D11, wait on D3D12, monotone value: this frame's wait is
    // satisfied only by this frame's own signal.
    let fence_value = cache.next_fence_value;
    cache.next_fence_value += 1;
    signal_d3d11_fence(&hw_frame, &cache.fence11, fence_value)?;
    let fence_mechanism = wait_d3d12_fence(compositor_queue, &cache.fence12, fence_value)?;

    // --- per-frame plane views: the SAFE API, D2Array ONLY ---------------------
    // D2Array (not D2) is deliberate: at rung 0 the frame is slice N of the
    // pool array, and wgpu-hal's DX12 backend emits a TEXTURE2DARRAY SRV
    // whenever base_array_layer != 0 (wgpu-hal-26.0.6/src/dx12/view.rs).
    // Declaring the view D2Array makes the shader's binding shape
    // (`texture_2d_array<f32>`) match the SRV that is actually created instead
    // of relying on a Texture2D/Texture2DArray coincidence.
    let luma_view = cache.texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("imported-frame-luma-plane0"),
        format: Some(wgpu::TextureFormat::R8Unorm),
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        aspect: wgpu::TextureAspect::Plane0,
        base_mip_level: 0,
        mip_level_count: Some(1),
        base_array_layer: array_index,
        array_layer_count: Some(1),
        usage: None,
    });
    let chroma_view = cache.texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("imported-frame-chroma-plane1"),
        format: Some(wgpu::TextureFormat::Rg8Unorm),
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        aspect: wgpu::TextureAspect::Plane1,
        base_mip_level: 0,
        mip_level_count: Some(1),
        base_array_layer: array_index,
        array_layer_count: Some(1),
        usage: None,
    });

    Ok(GpuFrame {
        texture: cache.texture.clone(),
        luma_view,
        chroma_view,
        base_array_layer: array_index,
        array_layers: cache.array_layers,
        width: hw_frame.width().max(0) as u32,
        height: hw_frame.height().max(0) as u32,
        colorspace: FrameColorspace::from_av(hw_frame.colorspace()),
        color_range: FrameColorRange::from_av(hw_frame.color_range()),
        pts_us: hw_frame.pts_us(),
        fence_mechanism,
        hw_frame,
        _fence12: cache.fence12.clone(),
        _fence11: cache.fence11.clone(),
    })
}

// ---------------------------------------------------------------------------
// Steps, factored out (each ported from the spike).
// ---------------------------------------------------------------------------

/// Render a LUID as the artifacts record it.
pub fn luid_hex(luid: i64) -> String {
    format!(
        "0x{:08X}{:08X}",
        ((luid >> 32) & 0xFFFF_FFFF) as u32,
        (luid & 0xFFFF_FFFF) as u32
    )
}

/// `texture -> GetDevice -> IDXGIDevice -> GetAdapter -> GetDesc -> AdapterLuid`.
/// `pub(crate)` since plan 48-10: `device_lost::assert_same_adapter_luid`
/// re-runs the same-adapter assertion on the RECREATED devices after a TDR.
pub(crate) fn d3d11_adapter_luid(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
) -> Result<i64, EngineError> {
    unsafe {
        let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device
            .cast()
            .map_err(|e| gpu_err(format!("ID3D11Device -> IDXGIDevice: {}", hr(e.code().0))))?;
        let adapter = dxgi
            .GetAdapter()
            .map_err(|e| gpu_err(format!("IDXGIDevice::GetAdapter: {}", hr(e.code().0))))?;
        let desc = adapter
            .GetDesc()
            .map_err(|e| gpu_err(format!("IDXGIAdapter::GetDesc: {}", hr(e.code().0))))?;
        Ok(((desc.AdapterLuid.HighPart as i64) << 32) | (desc.AdapterLuid.LowPart as i64))
    }
}

/// Create the `D3D12_FENCE_FLAG_SHARED` fence on wgpu's device and open it on
/// D3D11. The handle is closed immediately; the fence objects hold the lifetime.
fn create_shared_fence(
    compositor_device: &wgpu::Device,
    d3d11_device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
) -> Result<(ID3D12Fence, ID3D11Fence), EngineError> {
    let hal_device = unsafe { compositor_device.as_hal::<wgpu::hal::api::Dx12>() }
        .ok_or_else(|| gpu_err("compositor device exposes no DX12 hal device"))?;
    let raw_device = hal_device.raw_device();

    let fence12: ID3D12Fence = unsafe { raw_device.CreateFence(0, D3D12_FENCE_FLAG_SHARED) }
        .map_err(|e| {
            gpu_err(format!(
                "ID3D12Device::CreateFence(D3D12_FENCE_FLAG_SHARED): {}",
                hr(e.code().0)
            ))
        })?;
    let handle =
        unsafe { raw_device.CreateSharedHandle(&fence12, None, GENERIC_ALL.0, PCWSTR::null()) }
            .map_err(|e| {
                gpu_err(format!(
                    "ID3D12Device::CreateSharedHandle(fence): {}",
                    hr(e.code().0)
                ))
            })?;

    let device5: Result<ID3D11Device5, _> = d3d11_device.cast();
    let device5 = match device5 {
        Ok(d) => d,
        Err(e) => {
            // Close the handle before bailing — never leak it (T-48-05-03).
            let _ = unsafe { CloseHandle(handle) };
            return Err(gpu_err(format!(
                "ID3D11Device -> ID3D11Device5 (needed for OpenSharedFence): {}",
                hr(e.code().0)
            )));
        }
    };
    let mut fence11: Option<ID3D11Fence> = None;
    let open = unsafe { device5.OpenSharedFence(handle, &mut fence11) };
    let _ = unsafe { CloseHandle(handle) };
    open.map_err(|e| gpu_err(format!("ID3D11Device5::OpenSharedFence: {}", hr(e.code().0))))?;
    let fence11 =
        fence11.ok_or_else(|| gpu_err("OpenSharedFence returned S_OK but no fence"))?;
    Ok((fence12, fence11))
}

/// Export a shared NT handle for `texture` and open it on wgpu's `ID3D12Device`.
///
/// The handle is closed as soon as D3D12 has consumed it (threat T-48-05-03: a
/// stale shared handle reused later is a driver-level crash risk, so it never
/// outlives this function).
fn share_and_open(
    texture: &ID3D11Texture2D,
    compositor_device: &wgpu::Device,
) -> Result<ID3D12Resource, EngineError> {
    let dxgi: IDXGIResource1 = texture
        .cast()
        .map_err(|e| gpu_err(format!("ID3D11Texture2D -> IDXGIResource1: {}", hr(e.code().0))))?;
    let access = DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0;
    let handle = unsafe { dxgi.CreateSharedHandle(None, access, PCWSTR::null()) }.map_err(|e| {
        gpu_err(format!(
            "IDXGIResource1::CreateSharedHandle on the hw frame pool texture: {}",
            hr(e.code().0)
        ))
    })?;

    let hal_device = match unsafe { compositor_device.as_hal::<wgpu::hal::api::Dx12>() } {
        Some(d) => d,
        None => {
            let _ = unsafe { CloseHandle(handle) };
            return Err(gpu_err("compositor device exposes no DX12 hal device"));
        }
    };
    let mut opened: Option<ID3D12Resource> = None;
    let result = unsafe { hal_device.raw_device().OpenSharedHandle(handle, &mut opened) };
    let _ = unsafe { CloseHandle(handle) };

    match result {
        Ok(()) => opened
            .ok_or_else(|| gpu_err("OpenSharedHandle returned S_OK but no resource")),
        Err(e) => Err(gpu_err(format!(
            "ID3D12Device::OpenSharedHandle<ID3D12Resource>: {} (REJECTED)",
            hr(e.code().0)
        ))),
    }
}

/// D3D11 side signals the shared fence once the work that produced the surface
/// is queued — on the SAME immediate context FFmpeg decodes on, under FFmpeg's
/// documented device lock. `value` is this frame's monotone fence value from
/// the session cache (was a fresh per-frame fence at a constant value pre-fix).
fn signal_d3d11_fence(
    frame: &HwFrame,
    fence11: &ID3D11Fence,
    value: u64,
) -> Result<(), EngineError> {
    let ctx4: ID3D11DeviceContext4 = frame.device_context().cast().map_err(|e| {
        gpu_err(format!(
            "ID3D11DeviceContext -> ID3D11DeviceContext4 (needed for Signal): {}",
            hr(e.code().0)
        ))
    })?;
    let result = frame.locked(|| unsafe {
        let r = ctx4.Signal(fence11, value);
        ctx4.Flush();
        r
    });
    result.map_err(|e| gpu_err(format!("ID3D11DeviceContext4::Signal: {}", hr(e.code().0))))
}

/// D3D12 side waits. Preferred: a GPU-side wait on the exact queue wgpu submits
/// through. Fallback: a CPU stall (`SetEventOnCompletion` + `WaitForSingleObject`)
/// — a stall is not a copy, but it is weaker, so which one ran is recorded on
/// the `GpuFrame` rather than glossed. (On this stack the queue wait is the
/// proven mechanism; the stall is implemented-but-unused, exactly as the spike
/// measured.)
fn wait_d3d12_fence(
    compositor_queue: &wgpu::Queue,
    fence12: &ID3D12Fence,
    value: u64,
) -> Result<&'static str, EngineError> {
    let queue_wait = {
        let hal_queue = unsafe { compositor_queue.as_hal::<wgpu::hal::api::Dx12>() };
        match hal_queue {
            Some(q) => unsafe { q.as_raw().Wait(fence12, value) },
            None => Err(windows::core::Error::from(
                windows::Win32::Foundation::E_NOINTERFACE,
            )),
        }
    };

    match queue_wait {
        Ok(()) => Ok("queue_wait"),
        Err(_) => {
            unsafe {
                let event = CreateEventW(None, false, false, PCWSTR::null()).map_err(|e| {
                    gpu_err(format!("CreateEventW for the fence stall: {}", hr(e.code().0)))
                })?;
                let set = fence12.SetEventOnCompletion(value, event);
                if let Err(e) = set {
                    let _ = CloseHandle(event);
                    return Err(gpu_err(format!(
                        "ID3D12Fence::SetEventOnCompletion: {}",
                        hr(e.code().0)
                    )));
                }
                let waited = WaitForSingleObject(event, FENCE_STALL_TIMEOUT_MS);
                let _ = CloseHandle(event);
                if waited != WAIT_OBJECT_0 {
                    return Err(gpu_err(format!(
                        "the fence CPU stall timed out after {FENCE_STALL_TIMEOUT_MS}ms ({waited:?})"
                    )));
                }
            }
            Ok("cpu_stall")
        }
    }
}

/// Read the opened resource's heap placement (`GetHeapProperties`).
/// Returns `(heap_type, hresult)` — hresult 0 means the query itself succeeded.
fn read_heap_type(resource: &ID3D12Resource) -> (i32, i32) {
    let mut props = D3D12_HEAP_PROPERTIES::default();
    let mut flags = D3D12_HEAP_FLAGS::default();
    match unsafe { resource.GetHeapProperties(Some(&mut props), Some(&mut flags)) } {
        Ok(()) => (props.Type.0, 0),
        Err(e) => (0, e.code().0),
    }
}
