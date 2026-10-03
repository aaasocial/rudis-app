//! Runtime VRAM budget for the GPU preview ring (Phase 48, GPU-03).
//!
//! Three layers, engine-side substrate only (the wiring into the live
//! producer — eviction on shrink, startup depth logging in the app — lands
//! with the integration in plan 48-09; this module only publishes the number):
//!
//! 1. **PURE** — [`gpu_ring_depth`]: ring depth as the MINIMUM of two
//!    INDEPENDENT ceilings, the VRAM-derived depth and the hw-frame-pool-
//!    derived depth (`pool_size − decoder_headroom`). 48-RESEARCH.md Pitfall 4:
//!    VRAM-only math silently starves the decoder — at rung 0 every frame the
//!    ring pins is a slice of FFmpeg's own decoder pool, so a "budget allows
//!    depth 24" answer is meaningless if the pool only has 20 free slices.
//!    Taking both ceilings is structural here: the function's signature cannot
//!    be called without supplying `pool_size` AND `decoder_headroom`.
//! 2. **QUERY** — [`query_vram_budget`]: the live number, read at runtime from
//!    `IDXGIAdapter3::QueryVideoMemoryInfo` on the compositor's own DX12
//!    adapter (48-RESEARCH.md Pattern 3's verified route via
//!    `wgpu::Adapter::as_hal::<Dx12>()`).
//! 3. **WATCH** — [`VramBudgetWatch`]: a budget-change notification thread
//!    (`RegisterVideoMemoryBudgetChangeNotificationEvent`) re-querying on every
//!    OS wake-up and publishing the fresh `Budget` through an `AtomicU64`.
//!    A startup-only query is stale the moment another application takes VRAM.
//!
//! ## What is deliberately NOT here
//!
//! - **`RING_BUDGET_BYTES` (the CPU ring's 160 MB constant in
//!   `crates/preview/src/ring.rs`) is deliberately NOT ported.** GPU-03
//!   prohibits it by name: it is a CPU-RAM-tuned number sized for system
//!   memory pressure, and reusing it for VRAM would be exactly the ported-
//!   constant failure the requirement exists to prevent. The GPU ring's budget
//!   is queried at runtime, every time, from the adapter that actually holds
//!   the frames.
//! - **Eviction policy.** On a budget shrink the producer (48-09) evicts the
//!   OLDEST ring entries down to the new depth and keeps playing (degraded).
//!   It NEVER drops to software decode for a VRAM event — software fallback is
//!   reserved for decode *capability* failures (GPU-05, plan 48-09). This
//!   module publishes the number; it does not act on it.
//!
//! ## Why `Budget` and not `AvailableForReservation`
//!
//! `DXGI_QUERY_VIDEO_MEMORY_INFO.Budget` is the OS-recommended ceiling this
//! process should target; it tracks system-wide memory pressure live and is
//! exactly the semantic GPU-03 wants (48-CONTEXT.md's locked choice).
//! `AvailableForReservation` was REJECTED per CONTEXT: it answers "how much
//! could I pin via `SetVideoMemoryReservation`" — a reservation API Rudis does
//! not use — not "how much should I be using right now". `CurrentUsage` is
//! carried for headroom diagnostics/logging only, never as the budget itself.
//!
//! Defense in depth: 48-04 already set `MemoryBudgetThresholds` (95%/97%) on
//! the live `wgpu::Instance`, making VRAM-pressure death a catchable
//! `DeviceError::Lost`. That circuit breaker complements — and is NOT a
//! substitute for — this module's explicit budget-tracking, which degrades the
//! ring long before any threshold fires.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter3, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForSingleObject, INFINITE,
};

use crate::EngineError;

/// Fraction of the queried `Budget` the GPU ring may occupy
/// (48-CONTEXT.md: "start ~15%").
pub const VRAM_BUDGET_FRACTION: f64 = 0.15;

/// Explicit ring-depth floor (48-CONTEXT.md: clamped floor + ceiling). Even on
/// a tiny budget the ring keeps a few frames of runway.
pub const GPU_RING_MIN_DEPTH: usize = 4;

/// Explicit ring-depth ceiling (mirrors the CPU ring's `RING_MAX_DEPTH`
/// latency cap of 32 — the *shape* of a cap is shared; the budget feeding it
/// is not).
pub const GPU_RING_MAX_DEPTH: usize = 32;

/// How many times one hw-pool slice's bytes are counted against this
/// process's DXGI budget (48-gpu-oom-4k fix, MEASURED 2026-07-29 —
/// `.planning/debug/48-gpu-oom-4k-preview-present-panic.md`):
/// once for FFmpeg's D3D11 pool allocation itself, and once for the ONE
/// session-lifetime `ID3D12Device::OpenSharedHandle` open the import cache
/// holds (`QueryVideoMemoryInfo.CurrentUsage` grew by exactly pool-bytes at
/// decoder open AND again at the first import; the physical allocation exists
/// once, the OS charges each open separately).
pub const POOL_VRAM_COUNT_FACTOR: u64 = 2;

/// Pool slices the widening formula adds BEYOND `ring_target + headroom`:
/// the decoder's `threads` term (`avctx->thread_count`, observed 1 on this
/// stack — logged as `pool=37` for target 32 + headroom 4 + threads 1). The
/// VRAM half models it so the depth it answers is one the pool the depth
/// FORCES can actually afford.
pub const POOL_THREAD_SLICES: usize = 1;

/// Ring depth = min(VRAM-derived ceiling, hw-frame-pool-derived ceiling).
///
/// ## The VRAM half — a POOL cost model, not a per-frame cost model
///
/// **Corrected by the 48-gpu-oom-4k fix (2026-07-29).** The pre-fix math
/// (`budget × fraction / frame_bytes`) modeled each held ring entry as
/// costing one frame's bytes. That was wrong twice over: (1) ring entries pin
/// PRE-ALLOCATED pool slices — the VRAM is spent at decoder-open when the
/// pool is widened to `depth + headroom + threads` slices, not per held
/// entry; (2) the pool's bytes are charged [`POOL_VRAM_COUNT_FACTOR`] times
/// against the process budget (D3D11 allocation + the session's one D3D12
/// open — measured, see the constant). So the honest question is "what depth
/// can a pool the budget share affords sustain?":
///
/// ```text
/// affordable_slices = (budget × VRAM_BUDGET_FRACTION)
///                     / (POOL_VRAM_COUNT_FACTOR × frame_bytes)
/// vram_depth = affordable_slices − decoder_headroom − POOL_THREAD_SLICES
/// ```
///
/// clamped to `[GPU_RING_MIN_DEPTH, GPU_RING_MAX_DEPTH]`. At 4K on the dev
/// machine (budget ≈ 7.6 GB) this still answers 32 (pool 37 ≈ 921 MB counted,
/// within the 15% share of ~1.14 GB); on smaller cards it now scales the pool
/// down honestly instead of over-promising a depth whose pool cannot fit.
///
/// The pool half is `pool_size − decoder_headroom` (saturating): the slices
/// the decoder can afford to have pinned by the ring while still having
/// `decoder_headroom` slices free for its own in-flight work
/// ([`crate::hwdecode::DECODER_HEADROOM`] is the measured headroom callers
/// pass here — reused, never redefined).
///
/// Only the VRAM half changes at runtime (via [`VramBudgetWatch`]);
/// `pool_size` is fixed for a decode session at decoder-open. NOTE (post-fix
/// semantics): once a session's pool exists, ring depth no longer changes
/// VRAM usage — a budget-shrink eviction returns pool slices to the DECODER,
/// not bytes to the OS; freeing bytes requires ending the session.
///
/// `frame_bytes.max(1)` guards the zero-byte division; a `pool_size` smaller
/// than the headroom saturates to a depth of 0 (a degenerate pool yields no
/// ring, never an underflow panic).
///
/// ## The sibling term (Phase 57, 57-RESEARCH.md Pitfall 2)
///
/// `reserved_by_others` is the bytes CONCURRENT decode sessions have already
/// taken from this adapter's budget, read from a shared [`VramLedger`]. Before
/// Phase 57 the parameter did not exist, so N sessions each provisioned off
/// the FULL queried budget — the 48-gpu-oom-4k over-commit, ×N. It is
/// subtracted from `budget_bytes` **first**; every other line of the formula
/// is byte-identical to the pre-Phase-57 version, so `reserved_by_others == 0`
/// (the N=1 case, and every existing call site) returns exactly what it always
/// did. That equivalence is pinned by `ledger_zero_matches_old_behavior`.
///
/// **What the subtraction does and does not promise.** Because the fraction is
/// applied AFTER the subtraction, a sibling's reservation of `R` bytes removes
/// `VRAM_BUDGET_FRACTION × R` from this session's share — a *graduated*
/// correction that shrinks every later session's ring, not a hard
/// "15% of the budget, split N ways" cap. That is deliberate: 57-01 MEASURED
/// three concurrent 4K sessions at 2838 MB against a 7602 MB budget (37%,
/// flat in ring depth, 0 MB residual after teardown, no OOM —
/// `artifacts/57-A1-VERDICT.md`). A hard 15%-total cap would have starved the
/// third session to [`GPU_RING_MIN_DEPTH`] for a pressure that measurement says
/// is not there, trading a real playback regression for an imaginary safety
/// margin. The ordered shed (CONTEXT D-10) — not this formula — is what refuses
/// the session that genuinely does not fit.
pub fn gpu_ring_depth(
    budget_bytes: u64,
    frame_bytes: u64,
    pool_size: usize,
    decoder_headroom: usize,
    reserved_by_others: u64,
) -> usize {
    // Pitfall 2: what siblings already hold is not ours to provision against.
    // Saturating: a reservation exceeding the (possibly just-shrunk) budget
    // yields 0 → the floor clamp below, never an underflow.
    let budget_bytes = budget_bytes.saturating_sub(reserved_by_others);
    let affordable_slices = ((budget_bytes as f64 * VRAM_BUDGET_FRACTION)
        / (POOL_VRAM_COUNT_FACTOR * frame_bytes.max(1)) as f64) as usize;
    let vram_depth = affordable_slices.saturating_sub(decoder_headroom + POOL_THREAD_SLICES);
    let vram_depth = vram_depth.clamp(GPU_RING_MIN_DEPTH, GPU_RING_MAX_DEPTH);
    let pool_depth = pool_size.saturating_sub(decoder_headroom);
    vram_depth.min(pool_depth)
}

/// **ADMISSION** — can this session fit AT ALL, given what its siblings
/// already hold? A different question from [`gpu_ring_depth`], and the reason
/// D-10's third shed rung was dead for four phases (quick `260821-3qx`,
/// closing deferred item D-59.1-3).
///
/// # The two questions, and why one number could not answer both
///
/// [`gpu_ring_depth`] answers *"what depth should an ADMITTED session's ring
/// be?"*. Its floor is correct for that question: 48-CONTEXT settled a
/// SINGLE-session world where refusal was not on the menu — the choices were a
/// usable ring or no preview at all — so the budget fraction is *"clamped to an
/// explicit floor and ceiling ring depth"* and [`GPU_RING_MIN_DEPTH`] keeps an
/// admitted session a few frames of runway. That same context also ruled
/// *"never drop to software decode for a VRAM event — software fallback is
/// reserved for decode capability failures"*.
///
/// Phase 57 introduced N sessions, where *"does this one fit at all?"* becomes
/// a real question that must be answerable with **no** — and implemented the
/// answer as `gpu_ring_depth(..) == 0`. That reads the depth function for a
/// verdict its own doc explicitly disclaims: *"The ordered shed (CONTEXT D-10)
/// — not this formula — is what refuses the session that genuinely does not
/// fit."* The clamp, correct for the depth question, then made the refusal
/// unreachable for any budget, frame size or sibling reservation
/// (`LAYER-REENTRY-CAP budget-rung hostile_depth=4 floor=4`, D-59.1-3).
///
/// # What this answers
///
/// The UNCLAMPED form of the same arithmetic: after the sibling subtraction,
/// does this session's budget share afford the MINIMUM pool the floor forces
/// it to open — `GPU_RING_MIN_DEPTH + decoder_headroom + POOL_THREAD_SLICES`
/// slices at `frame_bytes`, charged [`POOL_VRAM_COUNT_FACTOR`] times? That is
/// exactly the condition *"the clamp would have had to lift this answer"*, so
/// **the starvation signal Phase 57 threw away is precisely the clamp's own
/// silent action**, now returned as a verdict instead of being absorbed.
///
/// [`gpu_ring_depth`] is byte-untouched, and so are the floor, the ceiling and
/// the clamp: an ADMITTED session gets exactly the depth it has always got.
/// On an unstarved adapter this returns `true` for every call — the dev
/// machine's 7.6 GB budget affords ~45 slices for a 4K session against the 9
/// this requires — so no measured number moves.
///
/// # Why not test the ledger directly
///
/// D-59.1-3 floated `ledger.reserved() + session_pool_vram_bytes(..) > budget`.
/// That compares against the WHOLE budget while every other line of this module
/// provisions against [`VRAM_BUDGET_FRACTION`] of it, so it would admit
/// sessions this module's own cost model says do not fit, and it would be a
/// second, differently-shaped budget policy. This form is the existing policy,
/// asked without the clamp.
pub fn gpu_session_admissible(
    budget_bytes: u64,
    frame_bytes: u64,
    decoder_headroom: usize,
    reserved_by_others: u64,
) -> bool {
    // Identical first two lines to `gpu_ring_depth` — same subtraction, same
    // fraction, same per-slice charge. Only the clamp is absent.
    let budget_bytes = budget_bytes.saturating_sub(reserved_by_others);
    let affordable_slices = ((budget_bytes as f64 * VRAM_BUDGET_FRACTION)
        / (POOL_VRAM_COUNT_FACTOR * frame_bytes.max(1)) as f64) as usize;
    affordable_slices >= GPU_RING_MIN_DEPTH + decoder_headroom + POOL_THREAD_SLICES
}

/// What one open decode session actually costs this process's DXGI budget:
/// its WHOLE hw frame pool, charged [`POOL_VRAM_COUNT_FACTOR`] times.
///
/// This — not `depth × frame_bytes` — is the number a session reserves in the
/// shared [`VramLedger`], for the reason this module's [`gpu_ring_depth`] doc
/// already states: **"once a session's pool exists, ring depth no longer
/// changes VRAM usage — a budget-shrink eviction returns pool slices to the
/// DECODER, not bytes to the OS."** The VRAM is spent at decoder-open, on the
/// whole widened pool, and it is released only when the session ends.
///
/// Cross-checked against measurement: 57-01 logged ~946 MB per 4K session with
/// `pool_size = 37`, and `2 × 37 × frame_bytes_nv12(3840, 2160)` = 921 MB
/// nominal — the same number the 48-gpu-oom-4k fix's own doc cites. A
/// depth-based reservation would have under-reported it by the
/// `decoder_headroom + POOL_THREAD_SLICES` slices the pool carries beyond the
/// ring, and would have had to be re-reserved on every budget shrink for no
/// gain in accuracy.
pub fn session_pool_vram_bytes(pool_size: usize, frame_bytes: u64) -> u64 {
    POOL_VRAM_COUNT_FACTOR
        .saturating_mul(pool_size as u64)
        .saturating_mul(frame_bytes)
}

/// Bytes concurrently reserved by live decode sessions on ONE adapter
/// (57-RESEARCH.md Pitfall 2: N sessions each provisioning off the FULL budget
/// re-creates the 48-gpu-oom-4k over-commit, ×N).
///
/// A shared running total, not a fixed `VRAM_BUDGET_FRACTION / N` split: the
/// research rejected the fixed split because it "either over-provisions when N
/// shrinks (a layer's session ends, siblings don't dynamically reclaim) or
/// under-provisions transiently during an N→N+1 crossover". A ledger degrades
/// gracefully in both directions.
///
/// `Clone` is the point — it is shared across session threads, and every clone
/// is the SAME total (one `Arc<AtomicU64>`, the same atomic-signalling idiom as
/// [`VramBudgetWatch`]'s published budget and the ring's `RingCtl`).
///
/// **RAII, deliberately** (threat T-57-06): [`VramLedger::reserve`] returns a
/// [`LedgerReservation`] that releases in `Drop`. `Drop` runs during unwind, so
/// a session that panics on its own thread — the isolation GPU-06 relies on —
/// cannot leak its share and starve every later session for the life of the
/// process. There is no `release()` to forget to call.
#[derive(Clone, Default)]
pub struct VramLedger(Arc<AtomicU64>);

impl VramLedger {
    /// A fresh, empty ledger. One per adapter/producer; hand `clone()`s to the
    /// sessions.
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    /// Total bytes currently reserved across every live session on this ledger.
    ///
    /// This is what a session about to open passes as
    /// [`gpu_ring_depth`]'s `reserved_by_others` — **before** it reserves its
    /// own share. A session that counted its own reservation against itself
    /// would shrink its own ring every time it re-derived the depth (e.g. on a
    /// budget-change wake); an already-reserved session must use
    /// [`LedgerReservation::others`] instead.
    pub fn reserved(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Add `bytes` to the running total now; subtract them again when the
    /// returned guard drops.
    pub fn reserve(&self, bytes: u64) -> LedgerReservation {
        self.0.fetch_add(bytes, Ordering::AcqRel);
        LedgerReservation {
            ledger: self.clone(),
            bytes,
        }
    }
}

/// One session's live share of a [`VramLedger`]. Releases on `Drop` — hold it
/// for exactly as long as the session's hw frame pool exists.
pub struct LedgerReservation {
    ledger: VramLedger,
    bytes: u64,
}

impl LedgerReservation {
    /// This reservation's own size, in bytes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// What everyone ELSE on the ledger currently holds — the
    /// `reserved_by_others` an already-reserved session must pass when it
    /// re-derives its ring depth (a live budget-shrink reaction). Saturating,
    /// so a concurrent release racing this read can never wrap.
    pub fn others(&self) -> u64 {
        self.ledger.reserved().saturating_sub(self.bytes)
    }
}

impl Drop for LedgerReservation {
    fn drop(&mut self) {
        // Exactly the bytes this guard added; the total can never underflow.
        self.ledger.0.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

// ---------------------------------------------------------------------------
// QUERY — the live number, from the compositor's own adapter.
// ---------------------------------------------------------------------------

/// The runtime VRAM numbers the ring math consumes.
///
/// `budget` is `DXGI_QUERY_VIDEO_MEMORY_INFO.Budget` — the locked choice (see
/// the module doc for why `AvailableForReservation` was rejected).
/// `current_usage` is for headroom diagnostics/logging only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VramInfo {
    /// OS-recommended VRAM ceiling for this process, in bytes. Live-tracking.
    pub budget: u64,
    /// This process's actual current VRAM allocation, in bytes.
    pub current_usage: u64,
}

fn gpu_err(msg: impl Into<String>) -> EngineError {
    EngineError::Gpu(msg.into())
}

/// Clone the `IDXGIAdapter3` out of a `wgpu::Adapter` (48-RESEARCH.md
/// Pattern 3's verified route — `wgpu-hal`'s DX12 `Adapter::as_raw()` already
/// IS an `IDXGIAdapter3`; no LUID lookup is needed for the compositor's own
/// adapter). The clone AddRefs, so the returned interface outlives the hal
/// guard.
fn dxgi_adapter3(adapter: &wgpu::Adapter) -> Result<IDXGIAdapter3, EngineError> {
    let hal_adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Dx12>() }.ok_or_else(|| {
        gpu_err("adapter is not DX12 — SPIKE-05/48-04 pin the backend on Windows")
    })?;
    Ok(hal_adapter.as_raw().clone())
}

/// Query the adapter's LOCAL memory segment budget at runtime.
///
/// Node 0 (single-GPU; multi-node/LDA is Phase 55's problem) and
/// `DXGI_MEMORY_SEGMENT_GROUP_LOCAL` (dedicated VRAM — the segment the
/// decoder pool and the ring textures actually live in).
pub fn query_vram_budget(adapter: &wgpu::Adapter) -> Result<VramInfo, EngineError> {
    let adapter3 = dxgi_adapter3(adapter)?;
    query_on(&adapter3)
}

/// The shared query body — also re-run by the watch thread on every wake.
fn query_on(adapter3: &IDXGIAdapter3) -> Result<VramInfo, EngineError> {
    let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    unsafe { adapter3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }
        .map_err(|e| gpu_err(format!("IDXGIAdapter3::QueryVideoMemoryInfo failed: {e}")))?;
    Ok(VramInfo {
        budget: info.Budget,
        current_usage: info.CurrentUsage,
    })
}

// ---------------------------------------------------------------------------
// WATCH — budget-change notifications, published through an AtomicU64.
// ---------------------------------------------------------------------------

/// A budget-change watch: one manual-reset Win32 event registered via
/// `RegisterVideoMemoryBudgetChangeNotificationEvent`, one small dedicated
/// thread (`"rudis-vram-budget"`) blocking on `WaitForSingleObject(…, INFINITE)`,
/// re-querying on every OS wake-up and publishing the fresh `Budget` through
/// an `AtomicU64` (the same atomic-signaling idiom as the existing `RingCtl`).
///
/// The 48-09 producer holds [`VramBudgetWatch::budget_handle`] and recomputes
/// [`gpu_ring_depth`] with the published value; only the VRAM half of the
/// depth changes at runtime.
///
/// Drop = `UnregisterVideoMemoryBudgetChangeNotification` (no further OS
/// signals) → shutdown flag → `SetEvent` wake → bounded `join` → `CloseHandle`.
/// The join is bounded because the flag is checked after EVERY wake and the
/// manual-reset event stays signaled once set.
pub struct VramBudgetWatch {
    /// The published budget, in bytes. Written with `Ordering::Release` by the
    /// watch thread; read with `Ordering::Acquire` by consumers.
    budget: Arc<AtomicU64>,
    /// The registration cookie `Unregister` needs at Drop.
    cookie: u32,
    /// Checked by the thread after every wake; set at Drop before the wake.
    shutdown: Arc<AtomicBool>,
    /// The manual-reset event handle. Owned; closed at Drop AFTER the join.
    event: HANDLE,
    /// A cloned (AddRef'd) interface for `Unregister` at Drop. `IDXGIAdapter3`
    /// is declared `Send + Sync` by windows-rs itself; DXGI adapter methods
    /// are free-threaded.
    adapter3: IDXGIAdapter3,
    /// `Some` until Drop joins it.
    thread: Option<JoinHandle<()>>,
}

// SAFETY: every field but `event` is Send on its own (`IDXGIAdapter3` carries
// windows-rs's own `unsafe impl Send + Sync`; the Arcs and the JoinHandle are
// Send). `event` is a plain kernel event HANDLE — a process-wide kernel-object
// handle, valid from any thread, with no thread affinity; only the raw-pointer
// representation makes `HANDLE` !Send. Moving the watch (e.g. into the 48-09
// producer's owner) is therefore sound. Same discipline as the documented
// `unsafe impl Send` on `HwDecodeSession`/`HwFrame` in `hwdecode.rs`.
unsafe impl Send for VramBudgetWatch {}

impl VramBudgetWatch {
    /// Register the notification, seed the atomic with a fresh query, and
    /// spawn the watch thread.
    pub fn spawn(adapter: &wgpu::Adapter) -> Result<VramBudgetWatch, EngineError> {
        // The fresh Arc's transient 0 is unobservable: no other clone exists
        // until this returns, and `spawn_into` stores the real query result
        // before registering the notification (the seed-before-register
        // invariant lives THERE and covers both constructors).
        Self::spawn_into(adapter, Arc::new(AtomicU64::new(0)))
    }

    /// [`Self::spawn`], but publishing into the CALLER's existing
    /// `Arc<AtomicU64>` instead of a fresh one — the D-48-10-BUDGETARC fix
    /// (49-02). Device-lost recovery respawns the watch against the recovered
    /// adapter; the long-lived GPU producer captured [`Self::budget_handle`]'s
    /// Arc at setup and never re-fetches it, so a respawn that allocates a
    /// fresh Arc freezes the producer's budget at the last pre-loss value.
    /// Passing the still-live handle here keeps every existing clone fresh:
    /// the returned watch's `budget` field IS `handle`.
    pub fn spawn_into(
        adapter: &wgpu::Adapter,
        handle: Arc<AtomicU64>,
    ) -> Result<VramBudgetWatch, EngineError> {
        let adapter3 = dxgi_adapter3(adapter)?;

        // Seed BEFORE registering (and before CreateEventW): the atomic is
        // never observably zero — and a recovery respawn flips the caller's
        // still-live handle from the stale pre-loss value to a fresh query
        // at the earliest possible moment.
        let initial = query_on(&adapter3)?;
        handle.store(initial.budget, Ordering::Release);
        eprintln!(
            "vram_budget: initial budget {} bytes (current_usage {} bytes)",
            initial.budget, initial.current_usage
        );

        // Manual-reset, initially unsignaled, unnamed.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| gpu_err(format!("CreateEventW for the budget watch: {e}")))?;

        let cookie =
            match unsafe { adapter3.RegisterVideoMemoryBudgetChangeNotificationEvent(event) } {
                Ok(c) => c,
                Err(e) => {
                    let _ = unsafe { CloseHandle(event) };
                    return Err(gpu_err(format!(
                        "RegisterVideoMemoryBudgetChangeNotificationEvent failed: {e}"
                    )));
                }
            };

        // The published-budget Arc IS the caller's handle (already seeded
        // above) — every clone the caller handed out stays live.
        let budget = handle;
        let shutdown = Arc::new(AtomicBool::new(false));

        // HANDLE is !Send only through its raw-pointer representation; the
        // kernel object itself is thread-agnostic. Carry the raw value.
        let raw_event = event.0 as usize;
        let thread_adapter = adapter3.clone();
        let thread_budget = Arc::clone(&budget);
        let thread_shutdown = Arc::clone(&shutdown);

        let thread = std::thread::Builder::new()
            .name("rudis-vram-budget".into())
            .spawn(move || {
                let event = HANDLE(raw_event as *mut core::ffi::c_void);
                loop {
                    let wait = unsafe { WaitForSingleObject(event, INFINITE) };
                    if thread_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if wait != WAIT_OBJECT_0 {
                        // A failed wait would otherwise spin at 100% CPU;
                        // stop watching (the last published budget stands).
                        eprintln!("vram_budget: watch wait failed ({wait:?}); watch stopped");
                        break;
                    }
                    // Manual-reset event: reset BEFORE re-querying, so a
                    // notification landing mid-query re-signals it and the
                    // next iteration picks up the final state (resetting
                    // after the query could silently drop that update).
                    let _ = unsafe { ResetEvent(event) };
                    match query_on(&thread_adapter) {
                        Ok(info) => {
                            thread_budget.store(info.budget, Ordering::Release);
                            eprintln!("vram_budget: budget changed to {} bytes", info.budget);
                        }
                        Err(e) => {
                            eprintln!("vram_budget: re-query after budget-change wake failed: {e}");
                        }
                    }
                }
            })
            .map_err(|e| {
                unsafe { adapter3.UnregisterVideoMemoryBudgetChangeNotification(cookie) };
                let _ = unsafe { CloseHandle(event) };
                gpu_err(format!("spawning the rudis-vram-budget thread: {e}"))
            })?;

        Ok(VramBudgetWatch {
            budget,
            cookie,
            shutdown,
            event,
            adapter3,
            thread: Some(thread),
        })
    }

    /// The live budget handle for the 48-09 producer: an `Arc<AtomicU64>` the
    /// watch thread keeps fresh. Read with `Ordering::Acquire`.
    pub fn budget_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.budget)
    }

    /// Convenience read of the current published budget, in bytes.
    pub fn budget(&self) -> u64 {
        self.budget.load(Ordering::Acquire)
    }
}

impl Drop for VramBudgetWatch {
    fn drop(&mut self) {
        // 1. No further OS signals for this event.
        unsafe {
            self.adapter3
                .UnregisterVideoMemoryBudgetChangeNotification(self.cookie)
        };
        // 2. Flag first, THEN wake — the thread checks the flag after every
        //    wake, and the manual-reset event stays signaled, so the wake
        //    cannot be lost.
        self.shutdown.store(true, Ordering::Release);
        let _ = unsafe { SetEvent(self.event) };
        // 3. Bounded join (tested by budget_watch_starts_and_stops).
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // 4. Only now is the handle safe to close (the thread no longer
        //    waits on it).
        let _ = unsafe { CloseHandle(self.event) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwdecode::DECODER_HEADROOM;

    const GIB_8: u64 = 8 * 1024 * 1024 * 1024;
    const MIB_512: u64 = 512 * 1024 * 1024;
    const MIB_64: u64 = 64 * 1024 * 1024;
    /// 1080p NV12: 1920 × 1080 × 3/2 (= `import::frame_bytes_nv12(1920, 1080)`).
    const FRAME_1080P: u64 = 3_110_400;
    /// 4K NV12: 3840 × 2160 × 3/2.
    const FRAME_4K: u64 = 12_441_600;

    #[test]
    fn pool_ceiling_binds_when_vram_is_plentiful() {
        // 8 GiB × 0.15 / (2 × 3.1 MB) ≈ 207 − 5 → clamps to
        // GPU_RING_MAX_DEPTH (32); pool 27 − headroom 4 = 23 →
        // min(32, 23) = 23: the POOL ceiling binds.
        assert_eq!(gpu_ring_depth(GIB_8, FRAME_1080P, 27, 4, 0), 23);
    }

    #[test]
    fn vram_ceiling_binds_when_budget_is_tight() {
        // 512 MiB × 0.15 / (2 × 3.1 MB) ≈ 12; − (4 + 1) = 7; pool 40 − 4 = 36
        // → min(7, 36) = 7: the VRAM ceiling binds. (Pre-fix this answered
        // 25 — a pool the tight budget could not actually afford twice over.)
        assert_eq!(gpu_ring_depth(MIB_512, FRAME_1080P, 40, 4, 0), 7);
    }

    #[test]
    fn floor_clamps_tiny_budgets() {
        // 64 MiB × 0.15 / (2 × 12.4 MB) < 1 → clamps up to GPU_RING_MIN_DEPTH
        // (4); then min(4, 23) = 4: the FLOOR binds.
        assert_eq!(gpu_ring_depth(MIB_64, FRAME_4K, 27, 4, 0), 4);
    }

    #[test]
    fn zero_frame_bytes_does_not_panic() {
        // The frame_bytes.max(1) guard: no division by zero. The resulting
        // huge VRAM depth clamps to the max, then the pool ceiling binds.
        assert_eq!(gpu_ring_depth(GIB_8, 0, 27, 4, 0), 23);
    }

    #[test]
    fn two_mocked_vram_sizes_produce_two_depths_through_the_same_code_path() {
        // SC-3's "two different VRAM sizes" wording, satisfied with MOCKED
        // budget values: only ONE physical GPU exists on this machine
        // (RTX 3070). The second size is a mock, NOT a second adapter —
        // recorded per 48-VALIDATION.md § Manual-Only Verifications.
        let big = gpu_ring_depth(GIB_8, FRAME_1080P, 40, 4, 0); // VRAM half caps at 32
        let small = gpu_ring_depth(MIB_512, FRAME_1080P, 40, 4, 0); // VRAM half gives 7
        assert_eq!(big, 32);
        assert_eq!(small, 7);
        assert_ne!(big, small);
    }

    #[test]
    fn four_k_at_the_real_dev_budget_still_caps_at_max() {
        // 48-gpu-oom-4k regression pin (frame-size dimension): the EXACT
        // budget the UAT machine logged (7 602 176 000 B). With the corrected
        // pool cost model the answer is still 32 — the resulting pool of 37
        // slices counts 2 × 37 × 12.44 MB ≈ 921 MB, inside the 15% share
        // (~1.14 GB). The depth was never the 4K bug; the per-frame
        // whole-pool OPEN was (fixed in import.rs).
        assert_eq!(gpu_ring_depth(7_602_176_000, FRAME_4K, 37, 4, 0), 32);
    }

    #[test]
    fn four_k_on_a_small_card_scales_the_pool_down() {
        // 48-gpu-oom-4k regression pin (cost-model dimension): 2 GiB budget
        // at 4K → share 322 MB / (2 × 12.44 MB) ≈ 12 affordable slices −
        // (4 + 1) = 7. The PRE-FIX math answered 25 here — a depth whose
        // 30-slice pool would have counted ~747 MB against a 322 MB share.
        const GIB_2: u64 = 2 * 1024 * 1024 * 1024;
        assert_eq!(gpu_ring_depth(GIB_2, FRAME_4K, 37, 4, 0), 7);
    }

    #[test]
    fn measured_48_05_pool_agrees_with_ceiling_math() {
        // 48-05's REAL measured widening: ArraySize 24 = max(baseline 17,
        // ring_target 19 + DECODER_HEADROOM 4 + threads 1). With plentiful
        // VRAM the depth is pool-bound at 24 − DECODER_HEADROOM = 20 ≥ the CPU
        // path's tuned 1080p target of 19 — the widened pool genuinely
        // supports CPU-parity depth without starving the decoder.
        assert_eq!(
            gpu_ring_depth(GIB_8, FRAME_1080P, 24, DECODER_HEADROOM, 0),
            20
        );
    }

    #[test]
    fn pool_smaller_than_headroom_saturates_to_zero() {
        // saturating_sub: a degenerate pool yields depth 0, never underflow.
        assert_eq!(gpu_ring_depth(GIB_8, FRAME_1080P, 3, 4, 0), 0);
    }

    // -----------------------------------------------------------------------
    // Phase 57 (plan 57-03) — the sibling-reservation term, 57-RESEARCH.md
    // Pitfall 2. The eight pinned-literal cases above are ALSO part of this
    // proof: every one of them now calls the 5-arg formula with
    // `reserved_by_others = 0` and expects the SAME number it expected before
    // the parameter existed.
    // -----------------------------------------------------------------------

    #[test]
    fn ledger_zero_matches_old_behavior() {
        // The pre-Phase-57 formula, transcribed verbatim from
        // crates/engine/src/vram_budget.rs:140-152 as it stood on 2026-08-02
        // (git `ee683ea9`). Not a call to the new function with a 0 — an
        // INDEPENDENT copy, so a future edit to the shared body cannot make
        // this test agree with itself vacuously.
        fn old_formula(
            budget_bytes: u64,
            frame_bytes: u64,
            pool_size: usize,
            decoder_headroom: usize,
        ) -> usize {
            let affordable_slices = ((budget_bytes as f64 * VRAM_BUDGET_FRACTION)
                / (POOL_VRAM_COUNT_FACTOR * frame_bytes.max(1)) as f64)
                as usize;
            let vram_depth =
                affordable_slices.saturating_sub(decoder_headroom + POOL_THREAD_SLICES);
            let vram_depth = vram_depth.clamp(GPU_RING_MIN_DEPTH, GPU_RING_MAX_DEPTH);
            let pool_depth = pool_size.saturating_sub(decoder_headroom);
            vram_depth.min(pool_depth)
        }

        // The grid spans every binding ceiling (vram / pool / floor) and both
        // frame sizes, including the real dev-machine budget and the two
        // 48-gpu-oom-4k regression rows.
        const GRID: &[(u64, u64, usize, usize)] = &[
            (GIB_8, FRAME_1080P, 27, 4),
            (MIB_512, FRAME_1080P, 40, 4),
            (MIB_64, FRAME_4K, 27, 4),
            (GIB_8, 0, 27, 4),
            (GIB_8, FRAME_1080P, 40, 4),
            (7_602_176_000, FRAME_4K, 37, 4),
            (2 * 1024 * 1024 * 1024, FRAME_4K, 37, 4),
            (GIB_8, FRAME_1080P, 24, 4),
            (GIB_8, FRAME_1080P, 3, 4),
            (0, FRAME_4K, 37, 4),
        ];
        for &(b, f, p, h) in GRID {
            assert_eq!(
                gpu_ring_depth(b, f, p, h, 0),
                old_formula(b, f, p, h),
                "N=1 (reserved_by_others = 0) must be numerically unchanged \
                 for budget={b} frame={f} pool={p} headroom={h}"
            );
        }

        // And the four literal answers the module has pinned since 48-07,
        // spelled out so a reader sees the numbers, not just the equivalence.
        assert_eq!(gpu_ring_depth(GIB_8, FRAME_1080P, 27, 4, 0), 23);
        assert_eq!(gpu_ring_depth(MIB_512, FRAME_1080P, 40, 4, 0), 7);
        assert_eq!(gpu_ring_depth(MIB_64, FRAME_4K, 27, 4, 0), 4);
        assert_eq!(gpu_ring_depth(7_602_176_000, FRAME_4K, 37, 4, 0), 32);
    }

    #[test]
    fn sibling_reservation_shrinks_depth() {
        // A tight budget where the VRAM half binds, so the sibling term is
        // observable at all (on a plentiful budget the pool ceiling would mask
        // it — which is itself correct, and asserted at the end).
        const BUDGET: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB
        let alone = gpu_ring_depth(BUDGET, FRAME_4K, 37, 4, 0);
        let with_half = gpu_ring_depth(BUDGET, FRAME_4K, 37, 4, BUDGET / 2);
        assert!(
            with_half <= alone,
            "a sibling holding half the budget must never RAISE the depth \
             (alone={alone}, with_half={with_half})"
        );
        assert!(
            with_half < alone,
            "on a VRAM-bound budget a 50% sibling reservation must actually \
             shrink the ring (alone={alone}, with_half={with_half})"
        );

        // Monotone: more reserved by siblings is never a deeper ring.
        let mut prev = usize::MAX;
        for n in 0..=8u64 {
            let d = gpu_ring_depth(BUDGET, FRAME_4K, 37, 4, n * BUDGET / 8);
            assert!(d <= prev, "depth rose at reserved = {n}/8 of the budget");
            prev = d;
        }

        // Saturation: reserved > budget can never underflow into a garbage
        // (huge) depth — it bottoms out at the documented floor.
        assert_eq!(
            gpu_ring_depth(BUDGET, FRAME_4K, 37, 4, BUDGET),
            GPU_RING_MIN_DEPTH
        );
        assert_eq!(
            gpu_ring_depth(BUDGET, FRAME_4K, 37, 4, u64::MAX),
            GPU_RING_MIN_DEPTH
        );

        // The pool ceiling still binds independently of the sibling term: a
        // degenerate pool yields 0 no matter what the ledger says.
        assert_eq!(gpu_ring_depth(GIB_8, FRAME_1080P, 3, 4, 0), 0);
        assert_eq!(gpu_ring_depth(GIB_8, FRAME_1080P, 3, 4, u64::MAX), 0);
    }

    // -----------------------------------------------------------------------
    // Quick 260821-3qx — ADMISSION, separated from DEPTH. Closes D-59.1-3.
    // -----------------------------------------------------------------------

    /// 8K NV12, the most hostile frame `parked_capacity_is_bounded_…` feeds
    /// the depth function.
    const FRAME_8K: u64 = 7680 * 4320 * 3 / 2;

    /// The unclamped form of `gpu_ring_depth`'s VRAM half, transcribed rather
    /// than called — the same discipline `ledger_zero_matches_old_behavior`
    /// uses, so this test cannot agree with the implementation vacuously.
    fn unclamped_vram_depth(
        budget_bytes: u64,
        frame_bytes: u64,
        decoder_headroom: usize,
        reserved_by_others: u64,
    ) -> usize {
        let budget_bytes = budget_bytes.saturating_sub(reserved_by_others);
        let affordable_slices = ((budget_bytes as f64 * VRAM_BUDGET_FRACTION)
            / (POOL_VRAM_COUNT_FACTOR * frame_bytes.max(1)) as f64)
            as usize;
        affordable_slices.saturating_sub(decoder_headroom + POOL_THREAD_SLICES)
    }

    #[test]
    fn admission_answers_exactly_the_question_the_clamp_was_absorbing() {
        // THE DEFINITION, asserted rather than described: a session is
        // admissible iff the clamp would NOT have had to lift its depth. Where
        // admission is false the old rung's condition (`gpu_ring_depth == 0`)
        // is still false — which is the whole of D-59.1-3 in one line.
        const GRID: &[(u64, u64, u64)] = &[
            (GIB_8, FRAME_1080P, 0),
            (GIB_8, FRAME_4K, 0),
            (MIB_512, FRAME_1080P, 0),
            (MIB_512, FRAME_4K, 0),
            (MIB_64, FRAME_4K, 0),
            (MIB_64, FRAME_1080P, 0),
            (7_602_176_000, FRAME_4K, 0),
            (7_602_176_000, FRAME_4K, 3 * 946_000_000),
            (2 * 1024 * 1024 * 1024, FRAME_4K, 1024 * 1024 * 1024),
            (2 * 1024 * 1024 * 1024, FRAME_4K, u64::MAX / 2),
            (1, FRAME_8K, u64::MAX / 2),
            (0, FRAME_4K, 0),
            (GIB_8, 0, 0),
        ];
        for &(budget, frame, reserved) in GRID {
            let admissible = gpu_session_admissible(budget, frame, DECODER_HEADROOM, reserved);
            let unclamped = unclamped_vram_depth(budget, frame, DECODER_HEADROOM, reserved);
            assert_eq!(
                admissible,
                unclamped >= GPU_RING_MIN_DEPTH,
                "admission must be 'the clamp did not have to lift this' for \
                 budget={budget} frame={frame} reserved={reserved} \
                 (unclamped={unclamped}, floor={GPU_RING_MIN_DEPTH})"
            );
            // And the rung it replaces still cannot fire anywhere on this grid.
            let old_rung_would_fire = gpu_ring_depth(
                budget,
                frame,
                GPU_RING_MAX_DEPTH + DECODER_HEADROOM,
                DECODER_HEADROOM,
                reserved,
            ) == 0;
            assert!(
                !old_rung_would_fire,
                "gpu_ring_depth answered 0 for budget={budget} frame={frame} \
                 reserved={reserved} — the clamp is gone and this whole \
                 separation needs re-deriving"
            );
        }
    }

    #[test]
    fn admission_refuses_the_hostile_inputs_the_old_rung_could_not() {
        // The exact arguments `layer_reentry_probe.rs` drives at the depth
        // function to prove the old rung's unreachability: budget 1, an 8K
        // NV12 frame, siblings holding everything the signature allows.
        assert!(
            !gpu_session_admissible(1, FRAME_8K, DECODER_HEADROOM, u64::MAX / 2),
            "maximum pressure must be inadmissible"
        );
        // …while the DEPTH function still answers its floor for the same
        // inputs, unchanged. Both facts together are the separation.
        assert_eq!(
            gpu_ring_depth(
                1,
                FRAME_8K,
                GPU_RING_MAX_DEPTH + DECODER_HEADROOM,
                DECODER_HEADROOM,
                u64::MAX / 2
            ),
            GPU_RING_MIN_DEPTH,
            "the floor is deliberately untouched: an ADMITTED session still \
             gets its runway"
        );
        // Cheaper starvations that are still starvation.
        assert!(!gpu_session_admissible(0, FRAME_1080P, DECODER_HEADROOM, 0));
        assert!(!gpu_session_admissible(
            MIB_512,
            FRAME_4K,
            DECODER_HEADROOM,
            MIB_512
        ));
    }

    #[test]
    fn admission_always_passes_on_this_machines_budget() {
        // THE NO-REGRESSION INVARIANT. Every BENCH number in v8 was taken on a
        // 7602176000-byte DXGI budget with at most three concurrent sessions;
        // if admission could refuse there, this change would have moved the
        // ruler. 57-01 measured ~946 MB per 4K session, 2838 MB for three.
        const DEV_BUDGET: u64 = 7_602_176_000;
        const PER_4K_SESSION: u64 = 946_000_000;
        for siblings in 0..=3u64 {
            for frame in [FRAME_1080P, FRAME_4K] {
                assert!(
                    gpu_session_admissible(
                        DEV_BUDGET,
                        frame,
                        DECODER_HEADROOM,
                        siblings * PER_4K_SESSION
                    ),
                    "the dev machine must admit every session it has ever \
                     opened (frame={frame}, siblings={siblings})"
                );
            }
        }
    }

    #[test]
    fn admission_is_monotone_in_both_directions() {
        // More budget never makes a session less admissible; more held by
        // siblings never makes it more. A predicate that is not monotone here
        // would make the shed's order incoherent.
        const BUDGET: u64 = 2 * 1024 * 1024 * 1024;
        let mut prev = false;
        for n in 0..=16u64 {
            let now = gpu_session_admissible(n * BUDGET / 16, FRAME_4K, DECODER_HEADROOM, 0);
            assert!(!(prev && !now), "admission fell as the budget ROSE at {n}/16");
            prev = now;
        }
        let mut prev = true;
        for n in 0..=16u64 {
            let now =
                gpu_session_admissible(BUDGET, FRAME_4K, DECODER_HEADROOM, n * BUDGET / 16);
            assert!(
                !(!prev && now),
                "admission rose as siblings took MORE at {n}/16"
            );
            prev = now;
        }
    }

    #[test]
    fn ledger_raii_releases_on_drop() {
        let ledger = VramLedger::new();
        assert_eq!(ledger.reserved(), 0);

        let a = ledger.reserve(1_000);
        assert_eq!(ledger.reserved(), 1_000);
        assert_eq!(a.bytes(), 1_000);
        assert_eq!(a.others(), 0, "a session must not count itself");

        {
            let b = ledger.reserve(2_500);
            assert_eq!(ledger.reserved(), 3_500);
            // Each guard sees only the OTHER's bytes as `reserved_by_others`.
            assert_eq!(a.others(), 2_500);
            assert_eq!(b.others(), 1_000);
            // A clone of the ledger is the SAME total, not a fresh one.
            assert_eq!(ledger.clone().reserved(), 3_500);
        }
        assert_eq!(ledger.reserved(), 1_000, "Drop must release b's share");

        drop(a);
        assert_eq!(ledger.reserved(), 0, "Drop must release a's share");
    }

    #[test]
    fn ledger_releases_when_a_session_thread_panics() {
        // Threat T-57-06: a leaked reservation after a session panic would
        // starve every later session for the life of the process. `Drop` runs
        // during unwind, so the RAII guard closes it — proven, not asserted.
        let ledger = VramLedger::new();
        let inner = ledger.clone();
        let joined = std::thread::spawn(move || {
            let _res = inner.reserve(4_096);
            assert_eq!(inner.reserved(), 4_096);
            panic!("simulated decode-thread panic (GPU-06 isolation)");
        })
        .join();
        assert!(joined.is_err(), "the spawned thread must have panicked");
        assert_eq!(
            ledger.reserved(),
            0,
            "the reservation must be released during unwind"
        );
    }

    #[test]
    fn session_pool_bytes_matches_the_measured_per_session_cost() {
        // 57-A1-VERDICT.md measured ~946 MB per 4K session at pool_size 37;
        // the nominal model is 2 × 37 × 12 441 600 B = 920.7 MB — the same
        // number gpu_ring_depth's own doc cites ("pool 37 ≈ 921 MB counted").
        let bytes = session_pool_vram_bytes(37, FRAME_4K);
        assert_eq!(bytes, 2 * 37 * FRAME_4K);
        assert_eq!(bytes / 1_000_000, 920);
        // Degenerate inputs stay finite.
        assert_eq!(session_pool_vram_bytes(0, FRAME_4K), 0);
        assert_eq!(session_pool_vram_bytes(37, 0), 0);
    }

    #[test]
    fn three_4k_siblings_still_fit_the_measured_envelope() {
        // The calibration claim in gpu_ring_depth's doc, made falsifiable:
        // with the REAL dev budget, three 4K sessions reserving their measured
        // pool cost must still leave the third session a USEFUL ring — not the
        // GPU_RING_MIN_DEPTH floor a hard "15% split N ways" cap would force,
        // because 57-01 measured that pressure is not there (37% of budget,
        // 0 MB residual, no OOM).
        const REAL_BUDGET: u64 = 7_602_176_000;
        let per_session = session_pool_vram_bytes(37, FRAME_4K);
        let third = gpu_ring_depth(REAL_BUDGET, FRAME_4K, 37, 4, 2 * per_session);
        assert!(
            third > GPU_RING_MIN_DEPTH,
            "the third concurrent 4K session must keep a real ring, got {third}"
        );
        assert!(
            third <= gpu_ring_depth(REAL_BUDGET, FRAME_4K, 37, 4, 0),
            "…but never MORE than it would get alone"
        );
    }
}
