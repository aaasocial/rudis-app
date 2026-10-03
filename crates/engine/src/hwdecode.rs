//! In-process D3D11VA hardware decode for the PREVIEW path (Phase 48, GPU-01 /
//! GPU-05 — XTRC-04 clause 2: "`crates/engine` gains hardware decode additively").
//!
//! Ported near-verbatim from the first-party, spike-proven
//! `spikes/44-hwaccel/src/hwdecode.rs` (SPIKE-01, Phase 44 — CLEAN GO at rung 0),
//! with the three production hardenings that phase deferred to this one:
//!
//! 1. **Hw frame pool widening** (48-CONTEXT.md RESOLVED 2026-07-29): FFmpeg's
//!    unconditional `1 + 16` H.264/HEVC pool (`ff_dxva2_common_frame_params`) is
//!    smaller than the CPU ring's tuned depth of 19 for 1080p, so at the default
//!    pool the preview ring could not reach parity without starving the decoder.
//!    `initial_pool_size` is widened in the same window the spike already used
//!    for `BindFlags`/`MiscFlags` — after `avcodec_get_hw_frames_parameters()`,
//!    before `av_hwframe_ctx_init()` (48-RESEARCH.md Pattern 4, route 1).
//! 2. **Typed capability gates that FAIL CLOSED** (GPU-05's decision point; the
//!    routing POLICY is plan 48-09's): non-NV12 `sw_format` (10-bit P010 — wgpu
//!    26 has no P010 texture format), out-of-matrix colorspace tags, and any
//!    libav open/init failure each yield a distinct [`HwOpenError`] variant the
//!    producer routes to software decode.
//! 3. **The runtime kill-switch** (`RUDIS_DISABLE_HWDECODE`): SPIKE-06's named
//!    re-isolation path. SPIKE-06 accepted a real crash-isolation regression —
//!    malformed media now parses inside the app's own address space — on the
//!    condition that a way back exists. The Cargo feature `hwdecode` is the
//!    compile-time half (plan 48-03); this env var is the runtime half: a user
//!    in the field flips an environment variable and every subsequent
//!    hardware-decode open fails closed into the CLI-sidecar software path,
//!    **no rebuild required**.
//!
//! The CLI-sidecar decode path (`crates/engine/src/ffmpeg.rs`) is NOT modified
//! or replaced by this module — it stands beside it and still owns export
//! byte-unchanged (GPU-07).
//!
//! ## Terminology (48-RESEARCH.md Pitfall 1)
//!
//! This module's array texture is the **hw frame pool** (FFmpeg's
//! `AVHWFramesContext`). It is entirely unrelated to `LayerDecoderPool` in
//! `decoder_pool.rs` (the existing CPU-side, sidecar-backed multi-layer
//! sessions), which this phase does not touch.
//!
//! ## The A1 finding, carried over (mirrors + runtime validation)
//!
//! `AVD3D11VADeviceContext` / `AVD3D11VAFramesContext` do not exist in rsmpeg
//! 0.18's generated bindings (rusty_ffmpeg's bindgen wrapper omits
//! `hwcontext_d3d11va.h` because it `#include`s `<d3d11.h>`). This module
//! therefore declares its own `#[repr(C)]` mirrors and **validates them at
//! runtime**: the `ID3D11Device` read out of the mirrored struct must equal
//! `texture->GetDevice()` (threat T-48-05-02 — a wrong layout must become a
//! failed open, never silent pointer corruption).
//!
//! ## Audited zero-copy discipline
//!
//! This file and `import.rs` together ARE the audited import path. **No CPU
//! pixel transfer of any kind may appear in either file** — no buffer mapping,
//! no CPU-side texture upload, no staging/readback resource creation. The
//! static audit in `tests/hwdecode_zero_copy.rs` checks the real source text.

use std::ffi::{c_int, c_void, CString};
use std::fmt;
use std::path::Path;
use std::ptr;

use rsmpeg::ffi;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Debug, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_DECODER,
    D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

use crate::EngineError;

// ---------------------------------------------------------------------------
// Public constants.
// ---------------------------------------------------------------------------

/// The runtime kill-switch environment variable (SPIKE-06's re-isolation path,
/// runtime half; CONTEXT D-02). Checked at the top of [`open_hw_decoder`]:
/// when set (to anything), hardware-decode open fails closed with
/// [`HwOpenError::Disabled`] and the caller routes to software decode.
pub const KILL_SWITCH_ENV: &str = "RUDIS_DISABLE_HWDECODE";

/// How many hardware-decode OPEN ATTEMPTS this process has made since start
/// (Phase 57, plan 57-07).
///
/// Incremented once inside [`open_hw_decoder_with`], immediately **after** the
/// kill-switch check and **before** any libav/D3D11 work — so it counts every
/// attempt that genuinely reached the driver, including the ones that then fail
/// closed into a typed [`HwOpenError`]. It deliberately does NOT count opens
/// suppressed by the kill-switch: those never touched hardware.
///
/// Purely observational (one relaxed `fetch_add`, no behaviour). Two pins read
/// it, and they read it for opposite reasons:
///
/// * **A floor** — a multi-layer pixel pin asserts the delta is `>= 2` across a
///   3-layer overlap, so the pin cannot pass green while every layer silently
///   fell back to software decode (which would make it a test of the CPU path
///   wearing the hardware path's name).
/// * **A ceiling** — the retimed-layer pin asserts the delta across a retimed
///   overlap stays at the layer count, which is the anti-storm bound: the
///   pre-fix software pool respawned its decoder *per output frame* for a
///   constant-retimed layer (45 spawns / 45 ticks, `tests/pool_retime.rs`), and
///   a hardware coordinator that rediscovered that bug independently would show
///   up here as a number that will not stop climbing.
pub static HW_OPEN_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many REAL `av_hwdevice_ctx_create(D3D11VA)` calls this process has made
/// (Phase 59.1, plan 59.1-03 — the device pool's reuse proof).
///
/// [`HW_OPEN_COUNT`] counts hardware-decode OPEN ATTEMPTS; this counts the
/// subset of them that actually built a new D3D11 device. **The difference is
/// the reuse count** — `HW_OPEN_COUNT − HW_DEVICE_CREATE_COUNT` is how many
/// opens were served by an already-existing device.
///
/// Why a SEPARATE counter rather than an inference: the pooled-vs-fresh pixel
/// pins would pass VACUOUSLY if the pool silently never engaged (every open
/// creating its own device produces byte-identical frames too — that is the
/// pre-pool behaviour). Threat T-59.1-03-02. Every pixel pin therefore carries
/// a delta clause on THIS counter inside the same test.
///
/// Incremented at exactly the create arms (relaxed `fetch_add`, purely
/// observational — nothing branches on it). A failed `av_hwdevice_ctx_create`
/// does NOT count: no device was built.
pub static HW_DEVICE_CREATE_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Per-stage wall times of the most recent [`open_inner`] run (Phase 59.1,
/// plan 59.1-01 — the decomposition 49-06 *estimated* but never measured).
///
/// 49-06 recorded `session_open_ms` as ONE opaque 64–72 ms bucket and then
/// asserted, in prose, that `av_hwdevice_ctx_create` is "the largest slice" of
/// it. Nothing in this tree had ever timed that call separately from demux,
/// probe, stream-select, decoder-open and pool-init — and Phase 59.1's entire
/// ROI premise rests on it. These fields turn that estimate into a number.
///
/// **DIAGNOSTIC ONLY.** Five [`std::time::Instant`] reads (~ns each) bracketing
/// stage boundaries that already existed; no call is reordered, no error path
/// gains an early return, nothing decides anything on these values. Written
/// unconditionally at the SUCCESS exit of `open_inner` and read only by
/// `tests/hwopen_stage_decomposition.rs`. **Never read by production code** —
/// if that ever changes, this doc comment is the thing to delete first.
#[derive(Debug, Clone, Copy)]
pub struct HwOpenStageTimes {
    /// `av_hwdevice_ctx_create(D3D11VA)` — the whole D3D11 device + context +
    /// video-device creation. **The stage a device pool would eliminate.**
    pub device_create_ms: f64,
    /// `avformat_open_input` + `avformat_find_stream_info` — per-file demux
    /// open and stream probe (in-process libavformat, NOT the CLI `ffprobe`).
    pub demux_probe_ms: f64,
    /// `av_find_best_stream` + the colorspace gate — in-memory, expected ~0.
    pub stream_select_ms: f64,
    /// `avcodec_alloc_context3` → `avcodec_open2` → `av_packet_alloc`.
    pub decoder_open_ms: f64,
    /// The probe decode: `get_format_d3d11` fires, the hw frame pool widens,
    /// `av_hwframe_ctx_init` allocates the array texture, first frame decodes.
    pub probe_decode_ms: f64,
    /// Whole-function wall — what the harness prints as `session_open_ms`.
    pub total_ms: f64,
}

/// The most recent [`HwOpenStageTimes`], or `None` before the first successful
/// open. Written at exactly ONE site (`open_inner`'s success exit); see
/// [`HwOpenStageTimes`] for why this exists and why nothing may read it outside
/// tests.
pub static HW_OPEN_LAST_STAGES: std::sync::Mutex<Option<HwOpenStageTimes>> =
    std::sync::Mutex::new(None);

/// Decoder-internal in-flight headroom added on top of the ring target when
/// widening the hw frame pool. Start value +4 matches FFmpeg's own internal
/// "guarantee 4 base work surfaces" margin (`libavcodec/decode.c`), per
/// 48-RESEARCH.md Pattern 4 / Open Question 2.
pub const DECODER_HEADROOM: usize = 4;

/// `D3D11_RESOURCE_MISC_SHARED` (0x2).
pub const MISC_SHARED: u32 = D3D11_RESOURCE_MISC_SHARED.0 as u32;
/// `D3D11_RESOURCE_MISC_SHARED_NTHANDLE` (0x800).
pub const MISC_SHARED_NTHANDLE: u32 = D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32;
/// The combination the interop chain wants: `SHARED | SHARED_NTHANDLE` (0x802).
/// NTHANDLE is only legal alongside SHARED (or SHARED_KEYEDMUTEX) per the D3D11 docs.
pub const MISC_SHARED_NTHANDLE_COMBO: u32 = MISC_SHARED | MISC_SHARED_NTHANDLE;
/// `D3D11_BIND_SHADER_RESOURCE` (0x8) — required for sampling the imported texture.
pub const BIND_SHADER_RESOURCE: u32 = D3D11_BIND_SHADER_RESOURCE.0 as u32;
/// `D3D11_BIND_DECODER` (0x200) — set by FFmpeg itself; recorded, never removed.
pub const BIND_DECODER: u32 = D3D11_BIND_DECODER.0 as u32;

/// `DXGI_FORMAT_NV12` (103) — the format a D3D11VA 8-bit decode pool uses.
pub const EXPECTED_DECODE_FORMAT: i32 = DXGI_FORMAT_NV12.0;

// ---------------------------------------------------------------------------
// Typed open errors — GPU-05's decision point.
// ---------------------------------------------------------------------------

/// Why a hardware-decode open failed CLOSED. Every variant is a per-media
/// fallback signal: the producer (plan 48-09) routes the clip to the existing
/// CLI-sidecar software decode path and logs the reason. None of these is a
/// crash; all of them keep playback correct (if slower).
#[derive(Debug, thiserror::Error)]
pub enum HwOpenError {
    /// The `RUDIS_DISABLE_HWDECODE` kill-switch is set (SPIKE-06 re-isolation,
    /// runtime half).
    #[error("hardware decode disabled by the RUDIS_DISABLE_HWDECODE kill-switch")]
    Disabled,
    /// Any libav open/init failure (device creation, demux, decoder open,
    /// hw frame pool init), with the av error string.
    #[error("hardware decoder init failed: {0}")]
    InitFailed(String),
    /// The hw frame pool's `sw_format` is not NV12 — e.g. P010 for any 10-bit
    /// source (HEVC Main10 / AV1 Main10 / VP9 Profile 2). `wgpu` 26 exposes no
    /// P010 texture format, so the proven Plane0→R8Unorm / Plane1→Rg8Unorm
    /// plane-view route has nothing to substitute (48-CONTEXT.md EXTENDED
    /// 2026-07-29 callout; 48-RESEARCH.md Pitfall 3). Carries the raw
    /// `AVPixelFormat` value.
    #[error("hw frame pool sw_format {0} is not NV12 (10-bit P010 class) — no wgpu plane-view route exists; use software decode for this media")]
    UnsupportedSwFormat(i32),
    /// The stream's colorspace tag is outside the SDR 601/709/untagged matrix
    /// this phase ships. Anything else must keep going through libswscale on
    /// the CPU path (which already handles it) — silent wrong color would
    /// violate matches-what-shipped-yesterday. Carries the raw `AVColorSpace`.
    #[error("stream colorspace tag {0} is outside the SDR 601/709/untagged matrix — use software decode for this media")]
    UnsupportedColorspace(i32),
}

// ---------------------------------------------------------------------------
// The session-wide failure latch (GPU-05, plan 48-09; CONTEXT D-15).
// ---------------------------------------------------------------------------

/// How many counted hardware-init failures engage the session-wide latch.
/// CONTEXT D-15: "a session-wide latch engages only after N repeated
/// failures" — N = 3, across DIFFERENT media (the caller de-duplicates
/// per-media before recording).
pub const LATCH_THRESHOLD: usize = 3;

/// Grace window, in milliseconds, after a noted device reset during which
/// `InitFailed` opens do NOT count toward the session latch
/// (D-48-10-LATCHWINDOW fix, 49-02). The deferred item measured the
/// adapter-reset window at ~2s; 5s covers it with headroom for slow
/// recoveries. The window is bounded and one-shot per reset mark: outside it
/// the conservative latch policy is byte-identical to the pre-fix behavior
/// (a spurious non-count is recoverable next open; a spurious PERMANENT
/// software demotion is not).
pub const HW_LATCH_RESET_GRACE_MS: u64 = 5_000;

/// Lazily-initialized process epoch for the latch's monotonic clock
/// ([`latch_now_ms`]). `Instant` is monotonic, so the reset mark can never
/// jump backward with wall-clock changes.
static LATCH_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Base offset added to the epoch-relative elapsed time. Two jobs:
/// (1) [`latch_now_ms`] is never 0, so `0` stays the never-armed sentinel in
/// `reset_mark_ms` structurally; (2) an EXPIRED mark is representable from
/// the very first millisecond of the process — without the base, a process
/// younger than [`HW_LATCH_RESET_GRACE_MS`] (exactly what a fresh test
/// process is) could not hold any mark that reads as expired, making the
/// out-of-window unit tests timing-dependent.
const LATCH_EPOCH_BASE_MS: u64 = 1 << 20;

/// Milliseconds since the (lazily initialized) process epoch, plus
/// [`LATCH_EPOCH_BASE_MS`]. Monotonic, nonzero, u64 — no overflow within any
/// plausible process lifetime.
fn latch_now_ms() -> u64 {
    LATCH_EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + LATCH_EPOCH_BASE_MS
}

/// The session-wide hardware-failure latch (GPU-05's escalation policy,
/// plan 48-09). Pure counting — the routing that consults it lives at the
/// producer's per-clip decode-path gate (`crates/preview/src/ring.rs`).
///
/// **Only [`HwOpenError::InitFailed`] counts.** `Disabled` is the user's own
/// kill-switch and `UnsupportedSwFormat` / `UnsupportedColorspace` are
/// expected per-media capability ROUTING (10-bit phone footage is ordinary,
/// not hardware trouble) — none of them is evidence the hardware/driver is
/// misbehaving, so none of them may cost the session its hardware path.
///
/// Once engaged the latch never disengages for the process lifetime (a
/// "session" — restart the app to re-attempt hardware decode).
pub struct HwFailureLatch {
    failures: std::sync::atomic::AtomicUsize,
    /// Milliseconds ([`latch_now_ms`] clock) of the most recent noted device
    /// reset; `0` = never armed (the sentinel — [`latch_now_ms`] itself is
    /// structurally nonzero). Opens the [`HW_LATCH_RESET_GRACE_MS`] window
    /// (D-48-10-LATCHWINDOW fix, 49-02).
    reset_mark_ms: std::sync::atomic::AtomicU64,
}

impl HwFailureLatch {
    /// A fresh, disengaged latch. `const` so it can live in a `static`.
    pub const fn new() -> Self {
        Self {
            failures: std::sync::atomic::AtomicUsize::new(0),
            reset_mark_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Open the device-reset grace window: for the next
    /// [`HW_LATCH_RESET_GRACE_MS`] ms, `InitFailed` opens are logged but NOT
    /// counted toward the latch (D-48-10-LATCHWINDOW fix, 49-02 — a real TDR
    /// window can fail D3D11 device creation for every clip opened inside it,
    /// and three different media in one ~2s reset would otherwise latch the
    /// session to software until restart). Call the moment device loss is
    /// detected; a second reset simply moves the mark forward.
    pub fn note_device_reset(&self) {
        self.reset_mark_ms
            .store(latch_now_ms().max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// Test seam: place the reset mark at an arbitrary [`latch_now_ms`]-clock
    /// value, so the window can be expired without sleeping
    /// (`set_reset_mark_ms_for_test(1)` reads as ~[`LATCH_EPOCH_BASE_MS`] in
    /// the past). Not part of the public policy surface.
    #[doc(hidden)]
    pub fn set_reset_mark_ms_for_test(&self, ms: u64) {
        self.reset_mark_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record an open failure. Returns `true` when it COUNTED toward the
    /// latch (`InitFailed` only — see the type doc). The caller must call
    /// this at most once per distinct media, or one bad file could engage
    /// the latch alone.
    ///
    /// An `InitFailed` inside the device-reset grace window
    /// ([`Self::note_device_reset`]) is logged but NOT counted — a clip
    /// opened against a resetting adapter fails spuriously, and spurious
    /// failures must not cost the session its hardware path
    /// (D-48-10-LATCHWINDOW fix, 49-02).
    pub fn record(&self, e: &HwOpenError) -> bool {
        match e {
            HwOpenError::InitFailed(_) => {
                let mark = self
                    .reset_mark_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                if mark != 0 {
                    let since_reset_ms = latch_now_ms().saturating_sub(mark);
                    if since_reset_ms < HW_LATCH_RESET_GRACE_MS {
                        eprintln!(
                            "hwdecode: InitFailed within {since_reset_ms}ms of a device reset \
                             — NOT counted toward the session latch (grace window \
                             {HW_LATCH_RESET_GRACE_MS}ms)"
                        );
                        return false;
                    }
                }
                self.failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            HwOpenError::Disabled
            | HwOpenError::UnsupportedSwFormat(_)
            | HwOpenError::UnsupportedColorspace(_) => false,
        }
    }

    /// Engage the latch DIRECTLY, bypassing the per-open counting — the
    /// containment route for failures that prove hardware/VRAM trouble
    /// without going through a decoder open at all (48-gpu-oom-4k fix: an
    /// uncaptured `OutOfMemory` on the live preview device engages this so
    /// every later clip routes to the proven software path instead of
    /// re-attempting hardware against an exhausted adapter). Idempotent;
    /// like every engagement, permanent for the process lifetime.
    pub fn force_engage(&self) {
        self.failures
            .fetch_max(LATCH_THRESHOLD, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether hardware-decode attempts should be skipped for the rest of
    /// the session.
    pub fn engaged(&self) -> bool {
        self.failures() >= LATCH_THRESHOLD
    }

    /// Counted failures so far (diagnostics / the engage log).
    pub fn failures(&self) -> usize {
        self.failures.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for HwFailureLatch {
    fn default() -> Self {
        Self::new()
    }
}

/// **THE** session-wide hardware-failure latch (GPU-05 / CONTEXT D-15): after
/// [`LATCH_THRESHOLD`] counted `InitFailed` results across DIFFERENT media,
/// hardware attempts are skipped for the rest of the session. A process-wide
/// static because "session" means the app's lifetime; the per-media
/// de-duplication lives in the producer's `hw_failed` set (each media records
/// at most once per producer lifetime).
///
/// **Why it lives in `engine` and not next to its readers** (debug session
/// `gpu-oom-guard-lost-in-the-cutover`, 2026-08-22). It used to be a private
/// `static HW_LATCH` in `crates/preview/src/ring.rs`, which meant the ONE
/// mechanism that can arm it from outside a decoder open — the live preview
/// device's uncaptured-error handler — had to reach it through a `pub fn` in
/// `preview`, called from whatever crate happened to own the device. That call
/// site was `src-tauri/src/native_surface.rs`; GATE-07 deleted the file and the
/// containment path silently had no caller for four phases. Here, the crate
/// that CREATES the device ([`crate::Compositor::build`]) can arm the latch
/// directly, so the coupling has no registration step and no call site for a
/// future host rewrite to forget. `preview` adopts this exact instance
/// (`use engine::PROCESS_HW_LATCH as HW_LATCH;`) — there is still precisely ONE
/// latch process-wide, and every reader is unchanged.
///
/// Tests must NEVER engage this: engagement is permanent for the process
/// lifetime by design, so it would poison every later test in the same binary.
/// Unit tests construct their own [`HwFailureLatch::new`]; the one proof that
/// must engage it (`crates/preview/tests/gpu_oom_guard.rs`) is an `#[ignore]`d
/// test in a binary of its own.
pub static PROCESS_HW_LATCH: HwFailureLatch = HwFailureLatch::new();

/// Force-engage [`PROCESS_HW_LATCH`] from OUTSIDE the decoder-open gate
/// (48-gpu-oom-4k fix, panic-containment half): the live preview device's
/// uncaptured-error handler calls this on an `OutOfMemory` so every LATER clip
/// delegation routes to the proven CPU sidecar path instead of re-attempting
/// hardware decode against an exhausted adapter. Idempotent; permanent for the
/// process lifetime (the latch's own semantic). Logs once, on the
/// disengaged->engaged edge.
///
/// Moved here VERBATIM in behaviour from `preview::force_hw_latch_engage`
/// (`ring.rs:1895-1911`), which had zero callers between GATE-07 and
/// 2026-08-22 — see [`PROCESS_HW_LATCH`] for why the move is the fix and not
/// merely a relocation.
pub fn force_hw_latch_engage(reason: &str) {
    if !PROCESS_HW_LATCH.engaged() {
        eprintln!(
            "hwdecode: session latch FORCE-ENGAGED ({reason}) — hardware-decode attempts are \
             skipped for the rest of this session; the software path serves preview"
        );
    }
    PROCESS_HW_LATCH.force_engage();
}

/// Report a GPU device reset / loss to the decode side — the ONE reset seam,
/// arming [`HwFailureLatch::note_device_reset`]'s grace window AND releasing
/// the pooled D3D11VA device in the same breath.
///
/// For the next [`HW_LATCH_RESET_GRACE_MS`] ms, `InitFailed` opens are logged
/// but NOT counted toward the session-wide fallback latch (D-48-10-LATCHWINDOW
/// fix, 49-02): a clip opened against a resetting adapter fails spuriously, and
/// three different media inside one ~2s reset window would otherwise demote the
/// session to software decode until restart. The pooled `av_hwdevice_ctx`
/// master ref goes with it (Phase 59.1, plan 59.1-03) — it outlives every
/// session by design, so nothing a session drops can reach it, and whoever
/// reports the reset must drop it here or every REOPENED session inherits the
/// dead device. Live sessions keep their own refs and are unaffected.
///
/// **The production caller is the device-lost response armed at DEVICE BIRTH**
/// in [`crate::Compositor`]'s `build` (quick-260829-n96) — not a registration a
/// host has to remember. Moved here VERBATIM in behaviour from
/// `preview::note_hw_device_reset` (`ring.rs`), whose imagined caller ("the
/// shell calls this the moment device loss is detected") never materialized
/// after GATE-07: it had ZERO callers repo-wide, which is exactly how
/// `force_hw_latch_engage` was lost on 2026-08-22. Same fix, same reason — the
/// response must be reachable from the crate that CREATES the device.
pub fn note_hw_device_reset() {
    PROCESS_HW_LATCH.note_device_reset();
    clear_pooled_device();
}

// ---------------------------------------------------------------------------
// #[repr(C)] mirrors of libavutil/hwcontext_d3d11va.h (FFmpeg 8.0.x).
// Field order/type copied from the header this crate links (the same pinned
// n8.0.1 dev headers build.rs guards); validated at RUNTIME below.
// ---------------------------------------------------------------------------

/// Mirror of `AVD3D11VADeviceContext` — allocated as `AVHWDeviceContext.hwctx`.
#[repr(C)]
#[allow(non_snake_case)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,         // ID3D11Device *
    device_context: *mut c_void, // ID3D11DeviceContext *
    video_device: *mut c_void,   // ID3D11VideoDevice *
    video_context: *mut c_void,  // ID3D11VideoContext *
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
}

/// Mirror of `AVD3D11FrameDescriptor`.
#[repr(C)]
#[allow(dead_code)]
struct AVD3D11FrameDescriptor {
    texture: *mut c_void, // ID3D11Texture2D *
    index: isize,         // intptr_t
}

/// Mirror of `AVD3D11VAFramesContext` — allocated as `AVHWFramesContext.hwctx`.
/// **This is where `BindFlags`/`MiscFlags` actually live** (Phase 44's A1
/// finding: the flags are a hw-frame-pool property, not a device property).
#[repr(C)]
#[allow(non_snake_case)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void, // ID3D11Texture2D *
    BindFlags: u32,       // UINT
    MiscFlags: u32,       // UINT
    texture_infos: *mut AVD3D11FrameDescriptor,
}

// Compile-time sanity on the mirrors' shape (x64 MSVC: 8-byte pointers, 4-byte UINT).
const _: () = assert!(std::mem::size_of::<AVD3D11VAFramesContext>() == 24);
const _: () = assert!(std::mem::size_of::<AVD3D11VADeviceContext>() == 56);

// ---------------------------------------------------------------------------
// libavutil error helpers (macros, so bindgen does not emit them).
// ---------------------------------------------------------------------------

/// `AVERROR_EOF` = `-MKTAG('E','O','F',' ')`.
const AVERROR_EOF: c_int = -0x2046_4F45;
/// `AVERROR(EAGAIN)`.
const AVERROR_EAGAIN: c_int = -(ffi::EAGAIN as c_int);
/// `AV_NOPTS_VALUE` (`INT64_MIN` — a macro, so bindgen does not emit it).
const AV_NOPTS_VALUE: i64 = i64::MIN;

/// Render an FFmpeg error code the way `av_strerror` does, with the raw code kept.
pub fn av_err(code: c_int) -> String {
    let mut buf = [0i8; 256];
    let text = unsafe {
        if ffi::av_strerror(code, buf.as_mut_ptr(), buf.len()) == 0 {
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        } else {
            "unknown error".to_owned()
        }
    };
    format!("{code} ({text})")
}

fn gpu_err(msg: impl Into<String>) -> EngineError {
    EngineError::Gpu(msg.into())
}

// ---------------------------------------------------------------------------
// RAII guards for the raw FFmpeg objects (ported unchanged).
// ---------------------------------------------------------------------------

struct FormatCtx(*mut ffi::AVFormatContext);
impl Drop for FormatCtx {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::avformat_close_input(&mut self.0) };
        }
    }
}

struct CodecCtx(*mut ffi::AVCodecContext);
impl Drop for CodecCtx {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::avcodec_free_context(&mut self.0) };
        }
    }
}

struct BufferRef(*mut ffi::AVBufferRef);
impl Drop for BufferRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_buffer_unref(&mut self.0) };
        }
    }
}

struct Packet(*mut ffi::AVPacket);
impl Drop for Packet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_packet_free(&mut self.0) };
        }
    }
}

struct Frame(*mut ffi::AVFrame);
impl Drop for Frame {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_frame_free(&mut self.0) };
        }
    }
}

// ---------------------------------------------------------------------------
// Texture desc evidence type (ported unchanged).
// ---------------------------------------------------------------------------

/// A read-back `D3D11_TEXTURE2D_DESC`. Real field values, not inferred
/// (CLAUDE.md rule 3 — inspect the object).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureDesc {
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub array_size: u32,
    pub format: i32,
    pub sample_count: u32,
    pub sample_quality: u32,
    pub usage: i32,
    pub bind_flags: u32,
    pub cpu_access_flags: u32,
    pub misc_flags: u32,
}

impl From<D3D11_TEXTURE2D_DESC> for TextureDesc {
    fn from(d: D3D11_TEXTURE2D_DESC) -> Self {
        Self {
            width: d.Width,
            height: d.Height,
            mip_levels: d.MipLevels,
            array_size: d.ArraySize,
            format: d.Format.0,
            sample_count: d.SampleDesc.Count,
            sample_quality: d.SampleDesc.Quality,
            usage: d.Usage.0,
            bind_flags: d.BindFlags,
            cpu_access_flags: d.CPUAccessFlags,
            misc_flags: d.MiscFlags,
        }
    }
}

impl fmt::Display for TextureDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Width={} Height={} MipLevels={} ArraySize={} Format={} SampleDesc={{Count={},Quality={}}} \
             Usage={} BindFlags=0x{:x} CPUAccessFlags=0x{:x} MiscFlags=0x{:x}",
            self.width,
            self.height,
            self.mip_levels,
            self.array_size,
            self.format,
            self.sample_count,
            self.sample_quality,
            self.usage,
            self.bind_flags,
            self.cpu_access_flags,
            self.misc_flags,
        )
    }
}

// ---------------------------------------------------------------------------
// Device handles read out of the mirrored structs (ported unchanged).
// ---------------------------------------------------------------------------

/// The D3D11 objects that own the decoded texture, borrowed out of FFmpeg's
/// `AVD3D11VADeviceContext` mirror and AddRef'd so they outlive any FFmpeg
/// teardown ordering.
///
/// `Clone` (plan 48-08, for [`HwFrame::try_clone`]) is COM `AddRef` on the two
/// interfaces plus plain copies of the lock callbacks — no pixel work, no new
/// D3D objects.
#[derive(Clone)]
struct DeviceHandles {
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
}

/// `ID3D11Device::GetDeviceRemovedReason` on the device inside an
/// `av_hwdevice_ctx` `AVBufferRef` — `None` = healthy.
///
/// THE ONE mirror walk for device health (`AVBufferRef` → `AVHWDeviceContext`
/// → `hwctx` (`AVD3D11VADeviceContext` mirror) → `ID3D11Device`). Two callers
/// share it deliberately rather than duplicating the unsafe pointer chase:
/// [`HwDecodeSession::device_removed_reason`] (the decode thread's
/// media-problem-vs-adapter-loss classifier, plan 48-10) and
/// [`acquire_pooled_device`]'s health check (plan 59.1-03 — a pooled master
/// that died in a TDR must never be handed to a new session).
///
/// Contains no pixel access of any kind.
unsafe fn hwdevice_removed_reason(device_ref: *mut ffi::AVBufferRef) -> Option<String> {
    if device_ref.is_null() {
        return None;
    }
    let device_ctx = (*device_ref).data as *mut ffi::AVHWDeviceContext;
    if device_ctx.is_null() {
        return None;
    }
    let hwctx = (*device_ctx).hwctx as *const AVD3D11VADeviceContext;
    if hwctx.is_null() {
        return None;
    }
    let device = ID3D11Device::from_raw_borrowed(&(*hwctx).device)?;
    match device.GetDeviceRemovedReason() {
        Ok(()) => None,
        Err(e) => Some(format!("0x{:08X}", e.code().0 as u32)),
    }
}

impl DeviceHandles {
    /// Read the device handles out of a live `AVHWFramesContext`.
    ///
    /// This is the first half of the **runtime validation of the `#[repr(C)]`
    /// mirrors**: if the layout were wrong, `device`/`device_context` would not
    /// QI as D3D11 interfaces here. The second half — the caller cross-checking
    /// `device` against `texture->GetDevice()` — happens in
    /// [`HwDecodeSession::wrap_frame`] and fails the decode, not the process.
    unsafe fn from_frames_ctx(frames_ctx: *mut ffi::AVHWFramesContext) -> Result<Self, EngineError> {
        let device_ctx = (*frames_ctx).device_ctx;
        if device_ctx.is_null() {
            return Err(gpu_err("AVHWFramesContext.device_ctx is null"));
        }
        let hwctx = (*device_ctx).hwctx as *const AVD3D11VADeviceContext;
        if hwctx.is_null() {
            return Err(gpu_err("AVHWDeviceContext.hwctx is null"));
        }
        let device = ID3D11Device::from_raw_borrowed(&(*hwctx).device)
            .ok_or_else(|| gpu_err("AVD3D11VADeviceContext.device is null — mirror layout wrong?"))?
            .clone();
        let device_context = ID3D11DeviceContext::from_raw_borrowed(&(*hwctx).device_context)
            .ok_or_else(|| {
                gpu_err("AVD3D11VADeviceContext.device_context is null — mirror layout wrong?")
            })?
            .clone();
        Ok(Self {
            device,
            device_context,
            lock: (*hwctx).lock,
            unlock: (*hwctx).unlock,
            lock_ctx: (*hwctx).lock_ctx,
        })
    }

    /// Honour FFmpeg's documented locking contract around `device_context` use.
    fn locked<T>(&self, f: impl FnOnce() -> T) -> T {
        unsafe {
            if let Some(lock) = self.lock {
                lock(self.lock_ctx);
            }
            let out = f();
            if let Some(unlock) = self.unlock {
                unlock(self.lock_ctx);
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// The decoded hardware frame.
// ---------------------------------------------------------------------------

/// A decoded hardware frame, keeping everything the texture depends on alive.
///
/// The `AVFrame` inside is REF-COUNTED into the hw frame pool: as long as this
/// value lives, its pool slice cannot be reused by the decoder. Dropping it
/// returns the slice. `import.rs` therefore keeps the `HwFrame` and the
/// imported `wgpu::Texture` together in ONE struct (`GpuFrame`) so they live
/// and die together (48-RESEARCH.md Pattern 1).
pub struct HwFrame {
    frame: Frame,
    handles: DeviceHandles,
    /// Kept only so its `Drop` runs after `frame`'s — belt and braces on
    /// teardown order (the frame's own refs already keep the device alive).
    _device_ref: BufferRef,
    /// Which received frame (0-based, decoder output order) this is — the same
    /// convention as the FFmpeg CLI's `select=eq(n\,N)` filter, so frame-diff
    /// baselines line up without fudging.
    pub frame_index: usize,
    /// Cross-check: FFmpeg's `AVD3D11VADeviceContext.device` ==
    /// `texture->GetDevice()`. Always `true` on a returned frame — a mismatch
    /// fails the decode instead (threat T-48-05-02).
    pub device_pointer_matches_texture_device: bool,
    /// Was `D3D11_CREATE_DEVICE_DEBUG` actually honoured on FFmpeg's decode
    /// device? Determined by CONSUMING the claim (QI for `ID3D11Debug`), not by
    /// trusting the option string. The live-object evidence capture
    /// (`examples/hwdecode_live_objects.rs`) depends on this being `true`.
    pub d3d11_debug_layer_active: bool,
    /// The video stream's time base, for PTS→µs conversion.
    time_base: ffi::AVRational,
}

// SAFETY: the raw AVFrame and the COM interfaces are used from one thread at a
// time (the frame is moved decode-thread → ring → present-thread, never shared
// mutably), COM interface pointers are AddRef'd owned references, and every
// use of the shared D3D11 immediate context goes through `locked()` honouring
// FFmpeg's documented locking contract. This is what lets the dedicated decode
// thread hand frames to the present thread (48-CONTEXT.md § Decode Integration).
unsafe impl Send for HwFrame {}

impl HwFrame {
    /// A refcounted clone of this hardware frame (plan 48-08, ADDITIVE — the
    /// enabler for `GpuFrame::try_clone`'s store-what-you-present contract).
    ///
    /// `av_frame_clone` bumps the refcount on the SAME hw-frame-pool slice —
    /// **no pixels move and no new slice is consumed**; the slice returns to
    /// the decoder only when EVERY clone has dropped. The device keep-alive
    /// ref is re-referenced the same way `wrap_frame` created it, and the COM
    /// handles are `AddRef`'d — so each clone independently upholds the
    /// drop-together lifetime contract (48-RESEARCH.md Pattern 1).
    ///
    /// Contains no CPU pixel transfer of any kind (the static zero-copy audit
    /// in `tests/hwdecode_zero_copy.rs` covers this file's real source text).
    pub fn try_clone(&self) -> Result<HwFrame, EngineError> {
        let raw = unsafe { ffi::av_frame_clone(self.frame.0) };
        if raw.is_null() {
            return Err(gpu_err("av_frame_clone returned null"));
        }
        let frame = Frame(raw);
        let device_ref_clone = unsafe { ffi::av_buffer_ref(self._device_ref.0) };
        if device_ref_clone.is_null() {
            return Err(gpu_err("av_buffer_ref(device) returned null"));
        }
        Ok(HwFrame {
            frame,
            handles: self.handles.clone(),
            _device_ref: BufferRef(device_ref_clone),
            frame_index: self.frame_index,
            device_pointer_matches_texture_device: self.device_pointer_matches_texture_device,
            d3d11_debug_layer_active: self.d3d11_debug_layer_active,
            time_base: self.time_base,
        })
    }

    /// `AVFrame.format` — must be `AV_PIX_FMT_D3D11` for this to be a real GPU frame.
    pub fn pix_fmt(&self) -> c_int {
        unsafe { (*self.frame.0).format }
    }

    pub fn is_d3d11(&self) -> bool {
        self.pix_fmt() == ffi::AV_PIX_FMT_D3D11
    }

    pub fn width(&self) -> i32 {
        unsafe { (*self.frame.0).width }
    }

    pub fn height(&self) -> i32 {
        unsafe { (*self.frame.0).height }
    }

    pub fn pts(&self) -> i64 {
        unsafe { (*self.frame.0).pts }
    }

    /// Presentation timestamp in microseconds (`None` when the stream carries
    /// no PTS for this frame).
    pub fn pts_us(&self) -> Option<i64> {
        let pts = self.pts();
        if pts == AV_NOPTS_VALUE {
            return None;
        }
        let us = unsafe {
            ffi::av_rescale_q(pts, self.time_base, ffi::AVRational { num: 1, den: 1_000_000 })
        };
        Some(us)
    }

    /// `AVFrame.colorspace` — the raw tag, mapped to a typed enum in `import.rs`.
    pub fn colorspace(&self) -> c_int {
        unsafe { (*self.frame.0).colorspace }
    }

    /// `AVFrame.color_range` — the raw tag, mapped to a typed enum in `import.rs`.
    pub fn color_range(&self) -> c_int {
        unsafe { (*self.frame.0).color_range }
    }

    /// `frame->data[0]` — the raw `ID3D11Texture2D*` (the WHOLE hw frame pool
    /// array texture, not a single slice).
    pub fn texture_ptr(&self) -> *mut c_void {
        unsafe { (*self.frame.0).data[0] as *mut c_void }
    }

    /// `frame->data[1]` — the array-texture slice this frame occupies. The live
    /// frame is NOT at slice 0 in general (Phase 44 observed slice 12); a
    /// single-slice import assumption is wrong.
    pub fn array_index(&self) -> isize {
        unsafe { (*self.frame.0).data[1] as isize }
    }

    /// The texture as a real COM interface. `None` if `data[0]` is null or does not QI.
    pub fn texture(&self) -> Option<ID3D11Texture2D> {
        let raw = self.texture_ptr();
        unsafe { ID3D11Texture2D::from_raw_borrowed(&raw).cloned() }
    }

    /// Read back the real `D3D11_TEXTURE2D_DESC` (CLAUDE.md rule 3 — inspect the object).
    pub fn desc(&self) -> Option<TextureDesc> {
        let texture = self.texture()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        Some(desc.into())
    }

    /// The `ID3D11Device` that owns the texture (from FFmpeg's device context).
    pub fn device(&self) -> &ID3D11Device {
        &self.handles.device
    }

    /// The `ID3D11DeviceContext` that owns the texture.
    pub fn device_context(&self) -> &ID3D11DeviceContext {
        &self.handles.device_context
    }

    /// Run `f` while holding FFmpeg's documented `AVD3D11VADeviceContext` lock.
    ///
    /// Exposed for the import layer, which must issue an
    /// `ID3D11DeviceContext4::Signal` on the SAME immediate context FFmpeg
    /// decodes on. Contains no pixel access of any kind — it only brackets a
    /// caller's closure.
    pub fn locked<T>(&self, f: impl FnOnce() -> T) -> T {
        self.handles.locked(f)
    }

    /// The raw `AVFrame*`.
    ///
    /// This accessor itself moves no pixels; it exists so the import layer can
    /// read frame metadata and so the VERIFICATION layer (test-only, excluded
    /// from the static audit by design) can produce its baselines.
    pub fn raw_frame(&self) -> *mut ffi::AVFrame {
        self.frame.0
    }
}

// ---------------------------------------------------------------------------
// get_format negotiation, with pool widening and the sw_format gate.
// ---------------------------------------------------------------------------

/// State handed to the `get_format` callback through `AVCodecContext.opaque`.
/// Boxed and owned by the session so the pointer stays valid if `get_format`
/// re-fires mid-stream (e.g. on a parameter change).
struct GetFormatState {
    device_ref: *mut ffi::AVBufferRef,
    ring_target: usize,
    // outputs
    called: bool,
    offered_d3d11: bool,
    ffmpeg_bind_flags: u32,
    ffmpeg_misc_flags: u32,
    applied_bind: u32,
    applied_misc: u32,
    /// FFmpeg's own codec-derived pool baseline (read from the struct — never
    /// hardcoded here; H.264/HEVC's is the unconditional `1 + 16`).
    baseline_pool_size: i32,
    /// What we widened `initial_pool_size` to.
    requested_pool_size: i32,
    /// `avctx->thread_count` at negotiation time (recorded, used in the formula).
    effective_thread_count: i32,
    /// The `sw_format` FFmpeg selected for the pool (the P010 gate reads this).
    sw_format_seen: i32,
    /// True when `get_format` refused the pool because `sw_format` != NV12.
    sw_format_rejected: bool,
    error: c_int,
    stage: &'static str,
}

/// `AVCodecContext.get_format` — select `AV_PIX_FMT_D3D11` and build the
/// hw frame pool with the proven flags OR'd in and the pool WIDENED.
///
/// Panic-free by construction (unwinding across an `extern "C"` boundary is UB).
unsafe extern "C" fn get_format_d3d11(
    avctx: *mut ffi::AVCodecContext,
    fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let state = (*avctx).opaque as *mut GetFormatState;
    if state.is_null() || fmts.is_null() {
        return ffi::AV_PIX_FMT_NONE;
    }
    (*state).called = true;

    // Is the hardware format even on offer?
    let mut p = fmts;
    let mut offered = false;
    while *p != ffi::AV_PIX_FMT_NONE {
        if *p == ffi::AV_PIX_FMT_D3D11 {
            offered = true;
            break;
        }
        p = p.add(1);
    }
    (*state).offered_d3d11 = offered;
    if !offered {
        (*state).stage = "get_format(no AV_PIX_FMT_D3D11 offered)";
        return ffi::AV_PIX_FMT_NONE;
    }

    // Ask the decoder to fill in an UNINITIALIZED frames context we can still edit.
    let mut frames_ref: *mut ffi::AVBufferRef = ptr::null_mut();
    let err = ffi::avcodec_get_hw_frames_parameters(
        avctx,
        (*state).device_ref,
        ffi::AV_PIX_FMT_D3D11,
        &mut frames_ref,
    );
    if err < 0 || frames_ref.is_null() {
        (*state).error = if err < 0 { err } else { -1 };
        (*state).stage = "avcodec_get_hw_frames_parameters";
        return ffi::AV_PIX_FMT_NONE;
    }

    let frames_ctx = (*frames_ref).data as *mut ffi::AVHWFramesContext;

    // THE P010 GATE (GPU-05 trigger; 48-CONTEXT.md EXTENDED callout): FFmpeg's
    // `ff_dxva2_common_frame_params` sets sw_format by bit depth
    // (YUV420P10→P010, YUV420P12→P012, default→NV12). Anything but NV12 has no
    // wgpu plane-view route at all, so refuse the hardware pool HERE — the open
    // then fails closed into `HwOpenError::UnsupportedSwFormat`.
    let sw_format = (*frames_ctx).sw_format;
    (*state).sw_format_seen = sw_format;
    if sw_format != ffi::AV_PIX_FMT_NV12 {
        (*state).sw_format_rejected = true;
        (*state).stage = "sw_format gate (pool sw_format is not NV12)";
        ffi::av_buffer_unref(&mut frames_ref);
        return ffi::AV_PIX_FMT_NONE;
    }

    // THE A1 FIELD PATH: flags live on AVHWFramesContext.hwctx (AVD3D11VAFramesContext).
    let hwctx = (*frames_ctx).hwctx as *mut AVD3D11VAFramesContext;
    if hwctx.is_null() {
        (*state).error = -1;
        (*state).stage = "AVHWFramesContext.hwctx was null";
        ffi::av_buffer_unref(&mut frames_ref);
        return ffi::AV_PIX_FMT_NONE;
    }
    (*state).ffmpeg_bind_flags = (*hwctx).BindFlags;
    (*state).ffmpeg_misc_flags = (*hwctx).MiscFlags;
    (*hwctx).BindFlags |= BIND_SHADER_RESOURCE;
    (*hwctx).MiscFlags |= MISC_SHARED_NTHANDLE_COMBO;
    (*state).applied_bind = (*hwctx).BindFlags;
    (*state).applied_misc = (*hwctx).MiscFlags;

    // POOL WIDENING (48-CONTEXT.md RESOLVED callout, 48-RESEARCH.md Pattern 4
    // route 1) — same window, before av_hwframe_ctx_init. The baseline is READ
    // from the struct (FFmpeg's own codec-derived number, which already
    // accounts for the codec's reference-frame needs) and never shrunk: the
    // widened pool is max(baseline, ring_target + DECODER_HEADROOM + threads).
    let baseline = (*frames_ctx).initial_pool_size;
    let threads = (*avctx).thread_count.max(1);
    let wanted = (*state).ring_target as i32 + DECODER_HEADROOM as i32 + threads;
    let widened = baseline.max(wanted);
    (*frames_ctx).initial_pool_size = widened;
    (*state).baseline_pool_size = baseline;
    (*state).requested_pool_size = widened;
    (*state).effective_thread_count = threads;

    // This is where the driver gets to say no.
    let err = ffi::av_hwframe_ctx_init(frames_ref);
    if err < 0 {
        (*state).error = err;
        (*state).stage = "av_hwframe_ctx_init";
        ffi::av_buffer_unref(&mut frames_ref);
        return ffi::AV_PIX_FMT_NONE;
    }

    (*avctx).hw_frames_ctx = frames_ref; // ownership moves to the codec context
    ffi::AV_PIX_FMT_D3D11
}

// ---------------------------------------------------------------------------
// The session.
// ---------------------------------------------------------------------------

/// A persistent in-process D3D11VA decode session for ONE media file's video
/// stream. Additive beside — never replacing — the CLI sidecar in `ffmpeg.rs`.
///
/// Created on (and used from) the dedicated decode thread; the present thread
/// only consumes the [`HwFrame`]s/`GpuFrame`s it hands over.
pub struct HwDecodeSession {
    // Field order IS drop order: the D3D12-side import cache FIRST (its wgpu
    // texture / fence refs release the whole-pool OpenSharedHandle charge
    // before FFmpeg tears the pool itself down), then the codec context (it
    // unrefs the hw frame pool + device refs it owns), then the demuxer, then
    // this session's own device ref, then the boxed get_format state the
    // codec's `opaque` pointer referenced while alive.
    /// Session-scoped D3D12 import cache (48-gpu-oom-4k fix): the ONE
    /// whole-pool `OpenSharedHandle` + wgpu texture + shared fence pair,
    /// built by `import_frame` on first use and reused for every later frame
    /// of this session. Interior mutability because `import_frame` takes
    /// `&HwDecodeSession`; only the decode thread ever touches it.
    import_cache: std::sync::Mutex<Option<crate::import::PoolImportCache>>,
    codec_ctx: CodecCtx,
    fmt_ctx: FormatCtx,
    device_ref: BufferRef,
    packet: Packet,
    state: Box<GetFormatState>,
    stream_index: i32,
    time_base: ffi::AVRational,
    /// Authoritative hw frame pool size: `desc.ArraySize` read back off the
    /// real pool texture of the first decoded frame — never a hardcoded number.
    pool_size: usize,
    received: usize,
    eof_sent: bool,
    /// The frame decoded while priming/validating at open, handed to the first
    /// `decode_next` call.
    pending: Option<HwFrame>,
}

// SAFETY: same argument as `HwFrame` — the session is used from one thread at
// a time (the dedicated decode thread owns it after creation); FFmpeg contexts
// are not thread-affine, only non-reentrant.
unsafe impl Send for HwDecodeSession {}

impl HwDecodeSession {
    /// The session's D3D12 import cache slot (48-gpu-oom-4k fix) — owned here
    /// so the whole-pool open lives and dies with the session; populated and
    /// keyed by `crate::import::import_frame`.
    pub(crate) fn import_cache(
        &self,
    ) -> &std::sync::Mutex<Option<crate::import::PoolImportCache>> {
        &self.import_cache
    }

    /// The WIDENED hw frame pool size actually created, read back from the
    /// real pool texture's `ArraySize` (authoritative — never the requested
    /// number restated).
    pub fn pool_size(&self) -> usize {
        self.pool_size
    }

    /// What `initial_pool_size` was set to before `av_hwframe_ctx_init`
    /// (= max(FFmpeg's codec-derived baseline, ring_target + headroom + threads)).
    pub fn requested_pool_size(&self) -> usize {
        self.state.requested_pool_size.max(0) as usize
    }

    /// FFmpeg's own codec-derived pool baseline, read from the frames context
    /// before widening (H.264/HEVC: the unconditional `1 + 16`).
    pub fn baseline_pool_size(&self) -> usize {
        self.state.baseline_pool_size.max(0) as usize
    }

    /// `avctx->thread_count` at pool-negotiation time (the `threads` term of
    /// the widening formula).
    pub fn effective_thread_count(&self) -> usize {
        self.state.effective_thread_count.max(0) as usize
    }

    /// Final `BindFlags` handed to `av_hwframe_ctx_init` (expected 0x208:
    /// DECODER | SHADER_RESOURCE).
    pub fn accepted_bind_flags(&self) -> u32 {
        self.state.applied_bind
    }

    /// Final `MiscFlags` handed to `av_hwframe_ctx_init` (expected 0x802:
    /// SHARED | SHARED_NTHANDLE).
    pub fn accepted_misc_flags(&self) -> u32 {
        self.state.applied_misc
    }

    /// The video stream's time base.
    pub fn time_base(&self) -> ffi::AVRational {
        self.time_base
    }

    /// D3D11-side device-loss detection (plan 48-10, 48-RESEARCH.md Pattern 5):
    /// poll `ID3D11Device::GetDeviceRemovedReason` on this session's decode
    /// device.
    ///
    /// `None` = the device is healthy (a decode failure is a media/codec
    /// problem — GPU-05's per-media fallback owns it). `Some(hresult)` = the
    /// WHOLE adapter reset (the TDR class): expect the wgpu device to be lost
    /// too, and route the observation into the ONE recovery funnel
    /// ([`crate::device_lost::DeviceLostSignal`]) — never a second competing
    /// recovery from the decode thread.
    ///
    /// Reads the device pointer off the same runtime-validated `#[repr(C)]`
    /// mirror the decode path uses; contains no pixel access of any kind.
    pub fn device_removed_reason(&self) -> Option<String> {
        unsafe { hwdevice_removed_reason(self.device_ref.0) }
    }

    /// Decode and return the next frame in decoder output order.
    /// `Ok(None)` = end of stream (not an error).
    pub fn decode_next(&mut self) -> Result<Option<HwFrame>, EngineError> {
        if let Some(frame) = self.pending.take() {
            return Ok(Some(frame));
        }
        unsafe { self.decode_next_inner() }
    }

    /// Seek the demuxer to (at or before) `us` microseconds and flush the
    /// decoder. The next [`decode_next`](Self::decode_next) resumes from the
    /// preceding keyframe; `frame_index` numbering restarts at 0 (it counts
    /// frames since the last open/seek, mirroring a fresh decode).
    pub fn seek_to_us(&mut self, us: i64) -> Result<(), EngineError> {
        self.pending = None;
        let ts = unsafe {
            ffi::av_rescale_q(us, ffi::AVRational { num: 1, den: 1_000_000 }, self.time_base)
        };
        let err = unsafe {
            ffi::av_seek_frame(
                self.fmt_ctx.0,
                self.stream_index,
                ts,
                ffi::AVSEEK_FLAG_BACKWARD as c_int,
            )
        };
        if err < 0 {
            return Err(gpu_err(format!("av_seek_frame failed: {}", av_err(err))));
        }
        self.flush();
        Ok(())
    }

    /// Flush the decoder's internal state (after a seek, or to restart the
    /// stream position without reopening).
    pub fn flush(&mut self) {
        self.pending = None;
        unsafe { ffi::avcodec_flush_buffers(self.codec_ctx.0) };
        self.eof_sent = false;
        self.received = 0;
    }

    unsafe fn decode_next_inner(&mut self) -> Result<Option<HwFrame>, EngineError> {
        loop {
            // Pull first: drain what the decoder is willing to give before
            // feeding it more (the spike's proven loop shape).
            let raw = ffi::av_frame_alloc();
            if raw.is_null() {
                return Err(gpu_err("av_frame_alloc returned null"));
            }
            let frame = Frame(raw);
            let err = ffi::avcodec_receive_frame(self.codec_ctx.0, frame.0);
            if err >= 0 {
                return self.wrap_frame(frame).map(Some);
            }
            if err == AVERROR_EOF {
                return Ok(None);
            }
            if err != AVERROR_EAGAIN {
                return Err(gpu_err(format!(
                    "avcodec_receive_frame failed: {} (get_format called={}, offered_d3d11={}, \
                     stage={}, get_format error={})",
                    av_err(err),
                    self.state.called,
                    self.state.offered_d3d11,
                    self.state.stage,
                    av_err(self.state.error)
                )));
            }
            drop(frame);

            if self.eof_sent {
                // The decoder was flushed and fully drained; EAGAIN here would
                // spin forever, so report it as the logic error it is.
                return Err(gpu_err("decoder returned EAGAIN after the flush packet"));
            }

            // Feed the next video packet (or the flush packet at EOF).
            loop {
                let err = ffi::av_read_frame(self.fmt_ctx.0, self.packet.0);
                if err < 0 {
                    // Includes AVERROR_EOF: flush the decoder and drain what is left.
                    let err = ffi::avcodec_send_packet(self.codec_ctx.0, ptr::null());
                    if err < 0 && err != AVERROR_EOF {
                        return Err(gpu_err(format!(
                            "avcodec_send_packet(flush) failed: {}",
                            av_err(err)
                        )));
                    }
                    self.eof_sent = true;
                    break;
                }
                if (*self.packet.0).stream_index != self.stream_index {
                    ffi::av_packet_unref(self.packet.0);
                    continue;
                }
                let err = ffi::avcodec_send_packet(self.codec_ctx.0, self.packet.0);
                ffi::av_packet_unref(self.packet.0);
                if err < 0 {
                    return Err(gpu_err(format!(
                        "avcodec_send_packet failed: {} (get_format called={}, offered_d3d11={}, \
                         stage={}, get_format error={})",
                        av_err(err),
                        self.state.called,
                        self.state.offered_d3d11,
                        self.state.stage,
                        av_err(self.state.error)
                    )));
                }
                break;
            }
        }
    }

    /// Validate a received frame and wrap it as a [`HwFrame`].
    ///
    /// Carries the ported runtime mirror validation (threat T-48-05-02): the
    /// `ID3D11Device` read out of the `#[repr(C)]` mirror MUST equal
    /// `texture->GetDevice()`. A wrong mirror layout becomes a failed decode —
    /// a test failure and a software-fallback signal — never silent pointer
    /// corruption.
    unsafe fn wrap_frame(&mut self, frame: Frame) -> Result<HwFrame, EngineError> {
        if (*frame.0).format != ffi::AV_PIX_FMT_D3D11 {
            return Err(gpu_err(format!(
                "frame {} came back as pix_fmt {} — NOT AV_PIX_FMT_D3D11 ({}). A CPU frame is \
                 not evidence of hardware decode.",
                self.received,
                (*frame.0).format,
                ffi::AV_PIX_FMT_D3D11
            )));
        }
        if (*frame.0).hw_frames_ctx.is_null() {
            return Err(gpu_err("D3D11 frame carries no hw_frames_ctx"));
        }
        let frames_ctx = (*(*frame.0).hw_frames_ctx).data as *mut ffi::AVHWFramesContext;
        let handles = DeviceHandles::from_frames_ctx(frames_ctx)?;

        // The mirror validation: mirrored device pointer == texture->GetDevice().
        let matches = {
            let raw = (*frame.0).data[0] as *mut c_void;
            match ID3D11Texture2D::from_raw_borrowed(&raw) {
                Some(tex) => match tex.GetDevice() {
                    Ok(d) => d.as_raw() == handles.device.as_raw(),
                    Err(_) => false,
                },
                None => false,
            }
        };
        if !matches {
            return Err(gpu_err(
                "repr(C) mirror validation FAILED: the ID3D11Device read out of the \
                 AVD3D11VADeviceContext mirror is not the device that owns the texture \
                 (texture->GetDevice() disagrees). The mirror layout is wrong for this \
                 FFmpeg build — refusing to touch its pointers (T-48-05-02).",
            ));
        }

        // Consume the debug-layer claim instead of trusting the option string.
        let debug_active = handles.device.cast::<ID3D11Debug>().is_ok();

        let device_ref_clone = ffi::av_buffer_ref(self.device_ref.0);
        if device_ref_clone.is_null() {
            return Err(gpu_err("av_buffer_ref(device) returned null"));
        }

        let index = self.received;
        self.received += 1;

        let hw = HwFrame {
            frame,
            handles,
            _device_ref: BufferRef(device_ref_clone),
            frame_index: index,
            device_pointer_matches_texture_device: matches,
            d3d11_debug_layer_active: debug_active,
            time_base: self.time_base,
        };

        // Authoritative pool size: the real texture's ArraySize, not our request.
        if let Some(desc) = hw.desc() {
            self.pool_size = desc.array_size as usize;
        }
        Ok(hw)
    }
}

// ---------------------------------------------------------------------------
// Open.
// ---------------------------------------------------------------------------

/// Open an in-process D3D11VA hardware-decode session for `path`'s best video
/// stream, widening the hw frame pool for a preview ring of `ring_target`
/// entries.
///
/// FAILS CLOSED into a typed [`HwOpenError`] on: the runtime kill-switch, any
/// libav init failure, a non-NV12 pool `sw_format` (10-bit P010 class), or an
/// out-of-matrix colorspace tag. The caller (plan 48-09's producer) routes
/// every variant to the existing CLI-sidecar software decode path.
pub fn open_hw_decoder(path: &Path, ring_target: usize) -> Result<HwDecodeSession, HwOpenError> {
    open_hw_decoder_with(path, ring_target, false)
}

// ---------------------------------------------------------------------------
// The process-wide D3D11VA DEVICE pool (Phase 59.1, plan 59.1-03).
//
// 49-06's costed option 1, verbatim: "pre-create and retain the
// `av_hwdevice_ctx` across sessions". `av_hwdevice_ctx` IS an `AVBufferRef`,
// i.e. already refcounted by libav, so "pooling" here is not a new cache
// abstraction — it is one retained master reference plus `av_buffer_ref`.
//
// ## What is PER-DEVICE (pooled) vs PER-SESSION (untouched)
//
// Medians measured 2026-08-09 in the `+comp` regime (a live wgpu compositor on
// the adapter — the running app's regime and the harness's), fixture
// `g30-cold-shape`, reps 1..9. Full record:
// `.planning/phases/59.1-decoder-device-session-pooling-close-seek-01-s-accepted-shor/
//  artifacts/59.1-DECOMPOSITION.md`.
//
// | Stage of `open_inner`                                   | median   | share  | pooled?                                                                                                   |
// |---------------------------------------------------------|----------|--------|------------------------------------------------------------------------------------------------------------|
// | `probe_decode` — `av_hwframe_ctx_init` + pool widening + first frame | 37.39 ms | 57.7 % | **NO — PER SESSION.** The allocation is bound to this codec context's dims + `ring_target`; the decode is real work on this file's bytes. |
// | `device_create` — `av_hwdevice_ctx_create`              | 23.20 ms | 35.8 % | **YES — PER DEVICE.** A D3D11VA device is generic to the adapter and carries no media identity (RESEARCH RQ2a). **The only stage this pool removes.** |
// | `demux_probe` — `avformat_open_input` + `find_stream_info` | 3.71 ms | 5.7 % | NO — PER FILE.                                                                                             |
// | `decoder_open` — `avcodec_alloc_context3` + `avcodec_open2` | 0.07 ms | 0.1 % | NO — PER SESSION, and free.                                                                                |
// | `stream_select` — `av_find_best_stream` + colorspace gate | 0.01 ms | 0.0 % | NO — PER FILE, and free.                                                                                   |
//
// **The ceiling is 23.20 ms**, and only if the stage vanishes outright.
// 49-06's prose called device creation "the largest slice" of `session_open`;
// the measurement REFUTED that — `probe_decode` leads. Nothing above ~23 ms may
// be attributed to this pool.
//
// ## Terminology (the standing note at the top of this module)
//
// The **hw FRAME pool** (`AVHWFramesContext`, the widened NV12 array texture)
// keeps its own name and its PER-SESSION life. This is the **DEVICE** pool.
// They are not the same object and the two must not be blurred.
// ---------------------------------------------------------------------------

/// Options that affect DEVICE CREATION — the pooled key.
///
/// Destructured **without a wildcard** at the pool gate (the D-37 tripwire):
/// adding a field here fails [`acquire_pooled_device`]'s compile until someone
/// decides, consciously, whether the new option may share a device.
#[derive(Debug, Clone, Copy)]
struct DeviceAcquireOpts {
    /// `D3D11_CREATE_DEVICE_DEBUG` (`av_dict` key `debug=1`). A debug device is
    /// a DIFFERENT creation, used only by the live-objects evidence capture, so
    /// it bypasses the pool in BOTH directions: it never reads the master and
    /// never becomes one.
    request_debug_layer: bool,
}

/// The process's one retained `av_hwdevice_ctx` reference.
struct PooledMaster(*mut ffi::AVBufferRef);

// SAFETY: the pointer is an `AVBufferRef`, whose refcount operations libav
// documents as thread-safe, and every read/write of this value happens under
// `POOLED_DEVICE`'s mutex regardless.
unsafe impl Send for PooledMaster {}

impl Drop for PooledMaster {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_buffer_unref(&mut self.0) };
        }
    }
}

/// The process's ONE shared D3D11VA device master ref. Lazy; lifetime = the
/// process, which is the HW_LATCH's already-documented "session = app lifetime"
/// semantic (`ring.rs`).
///
/// **Not charged to the `VramLedger`**: one device + immediate context is a
/// small, N-independent cost the ledger's 0.15 budget fraction absorbs, unlike
/// the per-session hw FRAME pools it does account for (RESEARCH RQ4).
///
/// **Not a fourth session manager.** It sits BELOW `warm_hw`, `LayerSessionSet`
/// and the software `LayerDecoderPool` — invisible to all three, keyed by
/// nothing, holding no media state.
static POOLED_DEVICE: std::sync::Mutex<Option<PooledMaster>> = std::sync::Mutex::new(None);

/// Really call `av_hwdevice_ctx_create(D3D11VA)`. Does NOT count — the two
/// arms of [`acquire_pooled_device`] own [`HW_DEVICE_CREATE_COUNT`], so the
/// counter has exactly two write sites and both are at the pool gate.
///
/// The debug arm keeps its established best-effort shape: if the debug layer is
/// unavailable, creation falls back to a plain device, and whether the layer is
/// really on is established afterwards by `QueryInterface`
/// ([`HwFrame::d3d11_debug_layer_active`]) rather than by trusting the option.
unsafe fn create_hw_device(request_debug_layer: bool) -> Result<BufferRef, HwOpenError> {
    let mut device_raw: *mut ffi::AVBufferRef = ptr::null_mut();
    let mut err = -1;
    if request_debug_layer {
        let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
        let key = CString::new("debug").unwrap();
        let val = CString::new("1").unwrap();
        ffi::av_dict_set(&mut opts, key.as_ptr(), val.as_ptr(), 0);
        err = ffi::av_hwdevice_ctx_create(
            &mut device_raw,
            ffi::AV_HWDEVICE_TYPE_D3D11VA,
            ptr::null(),
            opts,
            0,
        );
        ffi::av_dict_free(&mut opts);
    }
    if err < 0 {
        device_raw = ptr::null_mut();
        err = ffi::av_hwdevice_ctx_create(
            &mut device_raw,
            ffi::AV_HWDEVICE_TYPE_D3D11VA,
            ptr::null(),
            ptr::null_mut(),
            0,
        );
    }
    if err < 0 {
        return Err(HwOpenError::InitFailed(format!(
            "av_hwdevice_ctx_create(D3D11VA) failed: {}",
            av_err(err)
        )));
    }
    Ok(BufferRef(device_raw))
}

/// Hand this open its D3D11VA device: a new reference to the POOLED one when
/// there is a healthy master, otherwise a freshly created device that becomes
/// the master.
///
/// The returned `BufferRef` is the session's own reference and behaves exactly
/// as before — only its ORIGIN changed. Every other stage of `open_inner` is
/// per-session and byte-ordered as it was.
///
/// **Health is checked at every acquire**, not assumed: a TDR that killed the
/// adapter while no one was looking would otherwise hand the dead device to
/// every subsequent session (the "half-dead device" class, threat
/// T-59.1-03-04). The explicit clears at the device-lost funnel
/// ([`clear_pooled_device`]) are the primary mechanism; this is the backstop
/// for a loss nobody funneled.
unsafe fn acquire_pooled_device(opts: DeviceAcquireOpts) -> Result<BufferRef, HwOpenError> {
    // THE D-37 TRIPWIRE: wildcard-free. A new field breaks this line.
    let DeviceAcquireOpts { request_debug_layer } = opts;

    // ARM 1 — the debug-layer bypass. Never reads the pool, never writes it.
    if request_debug_layer {
        let fresh = create_hw_device(true)?;
        HW_DEVICE_CREATE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(fresh);
    }

    let mut master = POOLED_DEVICE.lock().unwrap_or_else(|p| p.into_inner());

    if let Some(existing) = master.as_ref() {
        match hwdevice_removed_reason(existing.0) {
            None => {
                let reused = ffi::av_buffer_ref(existing.0);
                if !reused.is_null() {
                    return Ok(BufferRef(reused));
                }
                eprintln!(
                    "hwdecode: av_buffer_ref on the pooled device returned null — dropping the \
                     master and creating fresh"
                );
            }
            Some(hr) => {
                eprintln!(
                    "hwdecode: pooled device dropped — GetDeviceRemovedReason={hr}; creating fresh"
                );
            }
        }
        // Drop runs `av_buffer_unref`; live sessions keep their own refs.
        *master = None;
    }

    // ARM 2 — the pool miss: create, count, retain one extra ref as the master.
    let fresh = create_hw_device(false)?;
    HW_DEVICE_CREATE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let retained = ffi::av_buffer_ref(fresh.0);
    if retained.is_null() {
        return Err(HwOpenError::InitFailed(
            "av_buffer_ref(device) for the pool master returned null".to_owned(),
        ));
    }
    *master = Some(PooledMaster(retained));
    Ok(fresh)
}

/// Release the process's pooled D3D11VA device master reference (Phase 59.1,
/// plan 59.1-03), so the NEXT hardware open builds a fresh device.
///
/// **Idempotent.** Live sessions are unaffected: each holds its own
/// `av_buffer_ref` on the device, and libav's refcounting means dropping the
/// pool's master reference never invalidates them — they die with their
/// sessions exactly as before.
///
/// Called from the TWO device-lost touchpoints that already exist, never from a
/// third path of its own (threat T-59.1-03-05, T-48-10-01's standing rule):
///
/// 1. [`note_hw_device_reset`] — the one production reset seam, the same call
///    that arms the latch's grace window. It lived in `preview` and had no
///    caller at all until quick-260829-n96 armed the device-lost response at
///    device birth in `Compositor::build`.
/// 2. The `RecoveryHooks::teardown_hw_pool` step-2 hook, beside the session
///    drop it already performs.
pub fn clear_pooled_device() {
    let mut master = POOLED_DEVICE.lock().unwrap_or_else(|p| p.into_inner());
    if master.take().is_some() {
        eprintln!(
            "hwdecode: pooled D3D11VA device master ref released — the next hardware open \
             creates a fresh device (live sessions keep their own refs and are unaffected)"
        );
    }
}

/// Acquire the process's shared D3D11VA device and hand back its
/// `ID3D11Device` — with NO media, NO demuxer and NO decoder open (Phase 63,
/// plan 63-01).
///
/// Device-lost recovery's step 4 must recreate, in order, "a new D3D11VA
/// device → a new `AVHWFramesContext` → a new wgpu device"
/// ([`crate::device_lost::RecoveryHooks::recreate`]). At the ENGINE tier only
/// the FIRST of those is the engine's to rebuild: hw FRAME pools live on
/// [`HwDecodeSession`]s, and sessions belong to the ring/preview callers, not
/// to the compositor. This is the seam that lets the recovery coordinator
/// rebuild the decode DEVICE — and hand the coordinator the `ID3D11Device` its
/// same-adapter LUID re-assert needs — without inventing a session it does not
/// own.
///
/// **Not a second creation path.** It goes through [`acquire_pooled_device`],
/// the one gate every hardware open already runs through (T-59.1-03-05,
/// T-48-10-01's standing rule), so it runs the same removed-device health
/// check, counts into [`HW_DEVICE_CREATE_COUNT`] the same way, and the device
/// recovery rebuilds IS the device the next [`open_hw_decoder`] reuses. Step 2
/// having just run [`clear_pooled_device`] is what makes this a genuinely fresh
/// device rather than the dead one.
///
/// The returned interface is AddRef'd, so it outlives the `AVBufferRef` it is
/// borrowed out of; the pool master reference keeps the device itself alive.
pub fn acquire_pooled_d3d11_device() -> Result<ID3D11Device, HwOpenError> {
    // SAFETY: the pool gate returns a live `av_hwdevice_ctx` `AVBufferRef`, and
    // the walk below is the SAME `AVBufferRef -> AVHWDeviceContext -> hwctx ->
    // ID3D11Device` chain `hwdevice_removed_reason` and
    // `DeviceHandles::from_frames_ctx` perform, through the same `#[repr(C)]`
    // mirror this module validates at runtime (a wrong layout fails the QI
    // below rather than corrupting anything).
    unsafe {
        let device_ref = acquire_pooled_device(DeviceAcquireOpts {
            request_debug_layer: false,
        })?;
        let device_ctx = (*device_ref.0).data as *mut ffi::AVHWDeviceContext;
        if device_ctx.is_null() {
            return Err(HwOpenError::InitFailed(
                "AVBufferRef.data (AVHWDeviceContext) is null".to_owned(),
            ));
        }
        let hwctx = (*device_ctx).hwctx as *const AVD3D11VADeviceContext;
        if hwctx.is_null() {
            return Err(HwOpenError::InitFailed(
                "AVHWDeviceContext.hwctx is null".to_owned(),
            ));
        }
        ID3D11Device::from_raw_borrowed(&(*hwctx).device)
            .cloned()
            .ok_or_else(|| {
                HwOpenError::InitFailed(
                    "AVD3D11VADeviceContext.device is null — mirror layout wrong?".to_owned(),
                )
            })
    }
}

/// As [`open_hw_decoder`], but optionally asks FFmpeg to create its D3D11
/// device with `D3D11_CREATE_DEVICE_DEBUG` (`av_hwdevice_ctx_create` option
/// `debug=1`, the key `hwcontext_d3d11va.c` reads).
///
/// Used by the live-object evidence capture
/// (`examples/hwdecode_live_objects.rs`). Best-effort by design: if the debug
/// layer is unavailable, device creation falls back to a non-debug device;
/// whether the layer is really on is then established by QueryInterface
/// ([`HwFrame::d3d11_debug_layer_active`]), not by assuming the option took.
pub fn open_hw_decoder_with(
    path: &Path,
    ring_target: usize,
    request_debug_layer: bool,
) -> Result<HwDecodeSession, HwOpenError> {
    // THE RUNTIME KILL-SWITCH — checked before any libav/D3D11 work happens.
    // SPIKE-06's named re-isolation path (runtime half; the Cargo feature is
    // the compile-time half, plan 48-03): flip an env var in the field, no
    // rebuild, and preview decode returns to the crash-isolated sidecar.
    if std::env::var_os(KILL_SWITCH_ENV).is_some() {
        return Err(HwOpenError::Disabled);
    }

    // The ONE increment site (see [`HW_OPEN_COUNT`]). Deliberately here — past
    // the kill-switch, before any driver work — so the counter means "attempts
    // that reached the hardware", success or typed failure alike.
    HW_OPEN_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    unsafe { open_inner(path, ring_target, request_debug_layer) }
}

/// The SDR colorspace tags this phase's shader matrix ships (GPU-02's fixture
/// matrix: BT.601-family / BT.709 / untagged). Everything else keeps going
/// through libswscale on the CPU path, which already handles it.
fn colorspace_supported(csp: c_int) -> bool {
    csp == ffi::AVCOL_SPC_UNSPECIFIED
        || csp == ffi::AVCOL_SPC_BT470BG
        || csp == ffi::AVCOL_SPC_SMPTE170M
        || csp == ffi::AVCOL_SPC_BT709
}

unsafe fn open_inner(
    path: &Path,
    ring_target: usize,
    request_debug_layer: bool,
) -> Result<HwDecodeSession, HwOpenError> {
    let init = |msg: String| HwOpenError::InitFailed(msg);

    // Phase 59.1 stage timers (see `HW_OPEN_LAST_STAGES`). DIAGNOSTIC ONLY:
    // these read the clock at boundaries that already existed and are consumed
    // once, at the success exit. Nothing below branches on them.
    let t_stage_start = std::time::Instant::now();

    // --- D3D11VA device -----------------------------------------------------
    // THE ONE call-path change of plan 59.1-03: the device comes from the
    // process-wide pool (`av_buffer_ref` on a healthy master) instead of a
    // per-open `av_hwdevice_ctx_create`. Every stage below is per-session and
    // runs in exactly the order it always did.
    let device_ref = acquire_pooled_device(DeviceAcquireOpts { request_debug_layer })?;
    let t_after_device = std::time::Instant::now();

    // --- demux --------------------------------------------------------------
    let path_str = path
        .to_str()
        .ok_or_else(|| init(format!("media path is not valid UTF-8: {}", path.display())))?;
    let c_path = CString::new(path_str)
        .map_err(|_| init("media path contains an interior NUL".to_owned()))?;

    let mut fmt_raw: *mut ffi::AVFormatContext = ptr::null_mut();
    let err = ffi::avformat_open_input(&mut fmt_raw, c_path.as_ptr(), ptr::null(), ptr::null_mut());
    if err < 0 {
        return Err(init(format!(
            "avformat_open_input({path_str}) failed: {}",
            av_err(err)
        )));
    }
    let fmt_ctx = FormatCtx(fmt_raw);

    let err = ffi::avformat_find_stream_info(fmt_ctx.0, ptr::null_mut());
    if err < 0 {
        return Err(init(format!(
            "avformat_find_stream_info failed: {}",
            av_err(err)
        )));
    }
    let t_after_demux = std::time::Instant::now();

    let mut decoder: *const ffi::AVCodec = ptr::null();
    let stream_index =
        ffi::av_find_best_stream(fmt_ctx.0, ffi::AVMEDIA_TYPE_VIDEO, -1, -1, &mut decoder, 0);
    if stream_index < 0 {
        return Err(init(format!(
            "av_find_best_stream(video) failed: {}",
            av_err(stream_index)
        )));
    }
    if decoder.is_null() {
        return Err(init("av_find_best_stream returned no decoder".to_owned()));
    }

    let stream = *(*fmt_ctx.0).streams.add(stream_index as usize);
    let time_base = (*stream).time_base;

    // --- colorspace gate (GPU-05; this phase ships SDR 601/709/untagged ONLY) --
    let stream_csp = (*(*stream).codecpar).color_space;
    if !colorspace_supported(stream_csp) {
        return Err(HwOpenError::UnsupportedColorspace(stream_csp));
    }
    let t_after_select = std::time::Instant::now();

    // --- decoder ------------------------------------------------------------
    let codec_raw = ffi::avcodec_alloc_context3(decoder);
    if codec_raw.is_null() {
        return Err(init("avcodec_alloc_context3 returned null".to_owned()));
    }
    let codec_ctx = CodecCtx(codec_raw);

    let err = ffi::avcodec_parameters_to_context(codec_ctx.0, (*stream).codecpar);
    if err < 0 {
        return Err(init(format!(
            "avcodec_parameters_to_context failed: {}",
            av_err(err)
        )));
    }

    // Boxed so the address the codec's `opaque` holds stays stable for the
    // session's whole life (get_format can re-fire mid-stream).
    let mut state = Box::new(GetFormatState {
        device_ref: device_ref.0,
        ring_target,
        called: false,
        offered_d3d11: false,
        ffmpeg_bind_flags: 0,
        ffmpeg_misc_flags: 0,
        applied_bind: 0,
        applied_misc: 0,
        baseline_pool_size: 0,
        requested_pool_size: 0,
        effective_thread_count: 0,
        sw_format_seen: ffi::AV_PIX_FMT_NONE,
        sw_format_rejected: false,
        error: 0,
        stage: "",
    });

    (*codec_ctx.0).hw_device_ctx = ffi::av_buffer_ref(device_ref.0);
    if (*codec_ctx.0).hw_device_ctx.is_null() {
        return Err(init("av_buffer_ref(device) returned null".to_owned()));
    }
    (*codec_ctx.0).opaque = state.as_mut() as *mut GetFormatState as *mut c_void;
    (*codec_ctx.0).get_format = Some(get_format_d3d11);

    let err = ffi::avcodec_open2(codec_ctx.0, decoder, ptr::null_mut());
    if err < 0 {
        return Err(init(format!("avcodec_open2 failed: {}", av_err(err))));
    }

    let packet = Packet(ffi::av_packet_alloc());
    if packet.0.is_null() {
        return Err(init("av_packet_alloc returned null".to_owned()));
    }
    let t_after_decoder = std::time::Instant::now();

    let mut session = HwDecodeSession {
        import_cache: std::sync::Mutex::new(None),
        codec_ctx,
        fmt_ctx,
        device_ref,
        packet,
        state,
        stream_index,
        time_base,
        pool_size: 0,
        received: 0,
        eof_sent: false,
        pending: None,
    };

    // --- probe decode (48-RESEARCH.md Pattern 1: decide fallback AT OPEN) ----
    // Decode until the first frame arrives. This forces get_format (and with it
    // the sw_format gate + pool widening) to actually run, validates the
    // repr(C) mirrors on a real texture, and reads the authoritative widened
    // pool size — so every capability gap surfaces HERE as a typed error, not
    // later as a mid-playback surprise.
    match session.decode_next() {
        Ok(Some(first)) => {
            session.pending = Some(first);
        }
        Ok(None) => {
            return Err(init(
                "stream ended before producing a single video frame".to_owned(),
            ));
        }
        Err(e) => {
            if session.state.sw_format_rejected {
                return Err(HwOpenError::UnsupportedSwFormat(session.state.sw_format_seen));
            }
            return Err(init(format!("probe decode failed: {e}")));
        }
    }
    let t_after_probe = std::time::Instant::now();

    // The ONE write site of `HW_OPEN_LAST_STAGES` (see its doc). Success path
    // only: a failed open has no meaningful decomposition and must not
    // overwrite the last good one. Poison-tolerant — a diagnostic must never
    // be the reason an open fails.
    {
        let stages = HwOpenStageTimes {
            device_create_ms: (t_after_device - t_stage_start).as_secs_f64() * 1e3,
            demux_probe_ms: (t_after_demux - t_after_device).as_secs_f64() * 1e3,
            stream_select_ms: (t_after_select - t_after_demux).as_secs_f64() * 1e3,
            decoder_open_ms: (t_after_decoder - t_after_select).as_secs_f64() * 1e3,
            probe_decode_ms: (t_after_probe - t_after_decoder).as_secs_f64() * 1e3,
            total_ms: (t_after_probe - t_stage_start).as_secs_f64() * 1e3,
        };
        let mut slot = HW_OPEN_LAST_STAGES
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *slot = Some(stages);
    }

    Ok(session)
}
