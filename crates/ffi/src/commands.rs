//! The named thin command exports (plan 47-05) — 16 of the 17 live commands,
//! each its own `extern "C"` symbol; the 17th (`rudis_dispatch_command`, the
//! one fat JSON entry point) lives in [`crate::dispatch`].
//!
//! D-04: this split mirrors today's Tauri surface EXACTLY — one fat
//! `dispatch_command` plus named thin commands — so SC-1's "exposes all 17
//! commands" stays verifiable by symbol enumeration (47-07's export-table
//! test). Every export body is a single [`ffi_guard!`](crate::ffi_guard)
//! invocation wrapping ONE [`call_out`]/[`call_json`] helper call — zero
//! bespoke logic per export. That is D-03's data-row property: adding a
//! future generation command is one more Args struct + one more fn of this
//! exact shape.
//!
//! # Envelope contract (D-06, two layers — the shape 47-02's serde test proved)
//!
//! - **Transport faults** ride [`RudisStatus`]: null ctx → `InvalidHandle`;
//!   null `out` (or a null `json` with `len > 0`) → `NullPointer`; invalid
//!   UTF-8 → `InvalidUtf8`; envelope serialization failure →
//!   `AllocationFailed`; a caught panic → `PanicCaught` (the guard's job).
//! - **Domain results** ride the JSON envelope in `*out`: `{"Ok": ..}` /
//!   `{"Err": ".."}` — serde's `Result<T, String>`, byte-for-byte today's
//!   Tauri wire shape. Args bytes that decode as UTF-8 but fail serde into
//!   the typed Args struct are a DOMAIN error (`{"Err": ".."}` with status
//!   `Ok`) — mirroring Tauri, where a bad payload errors the *invoke*, never
//!   the transport.
//!
//! # V5 input hygiene (threats T-47-05 / T-47-06)
//!
//! Every raw `ptr`/`len` pair is null-checked BEFORE any dereference and
//! decoded with `str::from_utf8` (never `_unchecked`), centralized in
//! [`call_out`]/[`call_json`] so no export can skip validation. Domain code
//! only ever sees serde-typed Args structs — never raw bytes.

use crate::buffer::vec_to_buffer;
use crate::ctx::FfiAppCtx;
use crate::ring;
use crate::{RudisBuffer, RudisCtx, RudisStatus};
use app_core::AppCtx;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------
// The two shared helpers EVERY command export plumbs through (D-03).
// ---------------------------------------------------------------------------

/// Serialize a domain `Result<T, String>` envelope into `*out`.
///
/// `out` has already been null-checked by the calling helper. A failed
/// `serde_json::to_vec` is a TRANSPORT fault (`AllocationFailed`) — the
/// envelope never reached the wire, so there is nothing for a domain `Err`
/// to ride in.
fn write_envelope<T: serde::Serialize>(
    envelope: &Result<T, String>,
    out: *mut RudisBuffer,
) -> RudisStatus {
    match serde_json::to_vec(envelope) {
        Ok(bytes) => {
            // SAFETY: non-null (checked by the caller); the caller hands us a
            // writable out-slot by the ABI contract.
            unsafe { *out = vec_to_buffer(bytes) };
            RudisStatus::Ok
        }
        Err(_) => RudisStatus::AllocationFailed,
    }
}

/// No-args export plumbing: null-check `ctx`/`out` → run `f` → serialize the
/// `Result<T, String>` envelope → [`vec_to_buffer`] into `*out`.
///
/// Transport-fault rules (D-06, exact): null ctx → `InvalidHandle`; null out
/// → `NullPointer`. Domain errors NEVER touch the status — they are
/// `{"Err": ".."}` with status `Ok`.
pub(crate) fn call_out<T: serde::Serialize>(
    ctx: *mut RudisCtx,
    out: *mut RudisBuffer,
    f: impl FnOnce(&RudisCtx) -> Result<T, String>,
) -> RudisStatus {
    if ctx.is_null() {
        return RudisStatus::InvalidHandle;
    }
    if out.is_null() {
        return RudisStatus::NullPointer;
    }
    // SAFETY: non-null by the check above; by the ABI contract this is a live
    // pointer obtained from `rudis_init` (not yet shut down), and the caller
    // does not mutate the ctx concurrently with this shared borrow.
    let ctx_ref = unsafe { &*ctx };
    write_envelope(&f(ctx_ref), out)
}

/// JSON-args export plumbing: everything [`call_out`] does, plus a null check
/// on the `json` ptr (when `len > 0`), UTF-8 validation (`str::from_utf8`,
/// never `_unchecked` → `InvalidUtf8`), and a serde deserialize into `Args`.
///
/// Bytes that decode as UTF-8 but fail serde into `Args` — malformed JSON or
/// a shape mismatch — are a DOMAIN error (`{"Err": ".."}` envelope, status
/// `Ok`), mirroring Tauri, where a bad payload errors the invoke, not the
/// transport (T-47-06: domain code never sees raw bytes).
pub(crate) fn call_json<Args: serde::de::DeserializeOwned, T: serde::Serialize>(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
    f: impl FnOnce(&RudisCtx, Args) -> Result<T, String>,
) -> RudisStatus {
    call_json_inner(ctx, json, len, out, None, f)
}

/// [`call_json`] for KEY-BEARING exports (review 69 IN-02): a deserialisation
/// failure maps to the FIXED text `invalid arguments: <export> payload rejected`
/// instead of serde's message, which can quote an offending scalar (e.g.
/// ``invalid type: integer `123` ``). The guarantee therefore no longer rests
/// on the caller always sending `key` as a string.
pub(crate) fn call_json_redacted<Args: serde::de::DeserializeOwned, T: serde::Serialize>(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
    export: &'static str,
    f: impl FnOnce(&RudisCtx, Args) -> Result<T, String>,
) -> RudisStatus {
    call_json_inner(ctx, json, len, out, Some(export), f)
}

fn call_json_inner<Args: serde::de::DeserializeOwned, T: serde::Serialize>(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
    redact_as: Option<&'static str>,
    f: impl FnOnce(&RudisCtx, Args) -> Result<T, String>,
) -> RudisStatus {
    if ctx.is_null() {
        return RudisStatus::InvalidHandle;
    }
    if out.is_null() {
        return RudisStatus::NullPointer;
    }
    if json.is_null() && len > 0 {
        return RudisStatus::NullPointer;
    }
    let bytes: &[u8] = if json.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: non-null (checked above) and `len` bytes long by the ABI
        // contract; read-only for the duration of this call.
        unsafe { std::slice::from_raw_parts(json, len) }
    };
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return RudisStatus::InvalidUtf8,
    };
    // SAFETY: same contract as `call_out`'s deref.
    let ctx_ref = unsafe { &*ctx };
    let envelope: Result<T, String> = match serde_json::from_str::<Args>(text) {
        Ok(args) => f(ctx_ref, args),
        Err(e) => Err(match redact_as {
            None => format!("invalid arguments: {e}"),
            Some(export) => format!("invalid arguments: {export} payload rejected"),
        }),
    };
    write_envelope(&envelope, out)
}

// ---------------------------------------------------------------------------
// The sync, store-only exports (plan 47-05 Task 1). Each body is ONE helper
// call over the app-core symbol 47-03's map guarantees exists.
// ---------------------------------------------------------------------------

/// `get_snapshot` — the whole `rudis_core::Project`, `{"Ok": {..Project..}}`.
#[no_mangle]
pub extern "C" fn rudis_get_snapshot(ctx: *mut RudisCtx, out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_get_snapshot", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| app_core::run_get_snapshot(&FfiAppCtx::new(c)))
    })
}

/// Args for [`rudis_get_entities`]: `{"ids": ["clip-1", ..]}`.
#[derive(serde::Deserialize)]
struct GetEntitiesArgs {
    ids: Vec<String>,
}

/// `get_entities` — the named entities only, in request order (the LAT-02
/// bulk-mutation fallback).
#[no_mangle]
pub extern "C" fn rudis_get_entities(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_get_entities", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: GetEntitiesArgs| {
            app_core::run_get_entities(&FfiAppCtx::new(c), &args.ids)
        })
    })
}

/// Args for [`rudis_get_waveform_peaks`]: `{"media_id": "media-3"}`.
#[derive(serde::Deserialize)]
struct GetWaveformPeaksArgs {
    media_id: String,
}

/// `get_waveform_peaks` — a PURE CACHE READ of one media item's peak envelope
/// (Phase 52, SHELL-09 / D-21).
///
/// * HIT:  `{"Ok": {"block_us": 10000, "sample_rate": 48000,
///           "peak_count": 372, "peaks_b64": "AAECAw.."}}`
/// * MISS: `{"Ok": null}` — for cache-miss, not-yet-computed, no-audio AND an
///   unknown `media_id` alike.
///
/// **This export NEVER triggers computation.** Peaks are produced once, by the
/// detached background job `app_core::import` starts at import time; there is
/// no code path from here to a decoder. That is what lets the Timeline call it
/// from its cold-path poll without ever risking the "synchronous call during
/// timeline draw" the roadmap's fifth criterion forbids.
///
/// An `Err` is deliberately NOT one of the four miss cases: the peak cache's
/// own posture is best-effort, and an `Err` would force the caller to
/// distinguish "broken" from "not ready yet" on every 100 ms poll. `Err` is
/// reserved for a genuinely malformed request, which [`call_json`] already
/// produces for free.
///
/// `media_id` is an OPAQUE DOMAIN ID (ASVS V5, T-52-21): `app-core` looks it up
/// in the real `Store` to obtain the path, and never joins the caller's string
/// onto any filesystem path.
///
/// No new event tag went with this — `ring::EVENT_NAMES` stays at 6.
#[no_mangle]
pub extern "C" fn rudis_get_waveform_peaks(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_get_waveform_peaks", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: GetWaveformPeaksArgs| {
            app_core::run_get_waveform_peaks(&FfiAppCtx::new(c), &args.media_id)
        })
    })
}

/// Args for [`rudis_get_filmstrip_strip`]: `{"media_id": "media-3"}`.
#[derive(serde::Deserialize)]
struct GetFilmstripStripArgs {
    media_id: String,
}

/// `get_filmstrip_strip` — a PURE CACHE READ of one media item's packed
/// thumbnail strip (Phase 53.2, D-12/D-14/D-15).
///
/// Mirrors [`rudis_get_waveform_peaks`] exactly — same args shape, same
/// envelope, same "never errors, never computes" promise, same opaque-id
/// treatment of `media_id` — with ONE addition: the payload's
/// `completed_tiles`/`total_tiles` pair distinguishes a PARTIAL strip
/// (progressive fill still in flight, D-14) from a COMPLETE one.
///
/// * HIT: `{"Ok": {"tile_w": 96, "tile_h": 54, "tiles_per_row": 16,
///   "total_tiles": 5, "completed_tiles": 5, "interval_us": 1000000,
///   "sheet_w": 1536, "sheet_h": 54, "strip_b64": "AAECAw.."}}` — `sheet_w`
///   and `sheet_h` describe the bytes ACTUALLY PRESENT (the completed rows),
///   so a consumer can size its upload from the payload alone without
///   assuming anything about a strip that is still being written.
/// * MISS: `{"Ok": null}` — for cache-miss, not-yet-computed, audio-only
///   media, a still image, an offline file, a failed decode AND an unknown
///   `media_id` alike (D-15). The caller cannot distinguish them and must not
///   try.
///
/// **This export NEVER triggers computation.** Strips are produced once, by
/// the detached background job `app_core::import` starts at import time; there
/// is no code path from here to a decoder. That is what lets the Timeline call
/// it from its cold-path poll without ever risking a synchronous decode during
/// paint.
///
/// `media_id` is an OPAQUE DOMAIN ID (ASVS V5, T-53.2-13): `app-core` looks it
/// up in the real `Store` to obtain the path, and never joins the caller's
/// string onto any filesystem path.
///
/// No new event tag went with this either — `ring::EVENT_NAMES` stays at 6.
#[no_mangle]
pub extern "C" fn rudis_get_filmstrip_strip(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_get_filmstrip_strip", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: GetFilmstripStripArgs| {
            app_core::filmstrip_job::run_get_filmstrip_strip(&FfiAppCtx::new(c), &args.media_id)
        })
    })
}

/// Args for [`rudis_get_proxy_status`]: `{"media_id": "media-3"}`.
#[derive(serde::Deserialize)]
struct GetProxyStatusArgs {
    media_id: String,
}

/// `get_proxy_status` — a PURE POLL of one media item's playback-proxy state
/// (Phase 58, PROXY-02 / 58-CONTEXT D-12/D-29).
///
/// Mirrors [`rudis_get_filmstrip_strip`] exactly — same args shape, same
/// envelope, same "never errors, never computes" promise, same opaque-id
/// treatment of `media_id`.
///
/// * KNOWN id: `{"Ok": {"state": "running"}}`. The seven states are `queued`,
///   `running`, `ready`, `not_needed`, `failed`, `cancelled` and `none`. A
///   consumer that only wants "is there a proxy yet?" compares against `ready`
///   and treats everything else as "not yet" — which is what the decode-source
///   resolver already does on its own, so nothing about playback depends on
///   this call being made at all.
/// * UNKNOWN id: `{"Ok": null}` — the only none-case. A known source with no
///   proxy answers `{"Ok": {"state": "none"}}`, because "there is no proxy and
///   none is coming" is information rather than an absence.
///
/// **This export NEVER triggers generation.** Proxies are produced by the
/// detached background job `app_core::import` starts at import time, gated by
/// the heaviness predicate; there is no code path from here to an encoder.
///
/// `media_id` is an OPAQUE DOMAIN ID (ASVS V5, T-58-05-01): `app-core` looks it
/// up in the real `Store` to obtain the path, and never joins the caller's
/// string onto any filesystem path.
///
/// POLL-ONLY BY DECISION (58-CONTEXT D-29), not by omission: progress rides
/// Phase 50 D-06's existing 100 ms cold-path poll, so no seventh event tag went
/// with this either — `ring::EVENT_NAMES` stays at 6, the closed D-02 set shared
/// across this ABI by both shells. A push event may be added by a later phase
/// **when a shell region actually consumes it**; today nothing does, because
/// 58-CONTEXT D-24 freezes shell work while the cutover is owner-blocked.
///
/// Phase 71 (TRUST-03): a `running` payload additionally carries
/// `"progress_permille"` (0..=999, monotonic per job; 999 is the ceiling while
/// running — `ready` is the only 'done'). Every other state serialises exactly
/// as before; a consumer that reads only `state` is unaffected.
#[no_mangle]
pub extern "C" fn rudis_get_proxy_status(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_get_proxy_status", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: GetProxyStatusArgs| {
            app_core::proxy_job::run_get_proxy_status(&FfiAppCtx::new(c), &args.media_id)
        })
    })
}

/// `get_render_cache_status` — a PURE POLL of the timeline render cache's
/// state (Phase 59, CACHE-01 / 59-CONTEXT D-30).
///
/// Mirrors [`rudis_get_proxy_status`] — same envelope, same "never errors,
/// never computes" promise — with **no args at all**, and that difference is
/// the interesting part: a proxy is a property of one MEDIA ITEM, while the
/// render cache is a property of the PROGRAM. There is no id to pass, so this
/// export takes `(ctx, out)` like [`rudis_get_current_seq`] rather than
/// `(ctx, json, len, out)` — and, pleasantly, it has no opaque-id surface to
/// defend at all (T-58-05-01 has no twin here).
///
/// Always `{"Ok": {..}}`, never `{"Ok": null}`:
/// `{"state": "idle", "rendering_segment": -1, "cached_segments": 12,
/// "heavy_segments": 3}`.
///
/// * `state` is exactly one of `none`, `idle`, `rendering`. `none` means this
///   host has no render-cache directory at all — nothing has ever been
///   rendered for it; `idle` means it has one and nothing is rendering right
///   now; `rendering` means a background segment render is in flight.
/// * `rendering_segment` is the segment index being rendered, or **`-1`** when
///   nothing is. `-1` rather than a nullable field, so a C# DTO deserializes it
///   into a plain `long`.
/// * `cached_segments` counts DISTINCT segment indices with a committed meta on
///   disk — a census of what has been rendered, NOT a count of what would be
///   served right now. Re-deriving each segment's identity would mean one
///   key-material walk per entry on a 100 ms poll; the reader answers "would
///   this serve?" per tick, fail-closed, and is the only honest place for it.
/// * `heavy_segments` is the detector's current candidate count — the work the
///   background scheduler still has to do.
///
/// **This export NEVER triggers a render.** Segments are produced by the
/// detached background job `app_core::render_cache_job` starts from the
/// transport funnel; there is no code path from here to an encoder, and
/// `render_cache_job_status_never_starts_a_render` polls it a hundred times
/// against a heavy section to prove it.
///
/// POLL-ONLY BY DECISION (59-CONTEXT D-30), not by omission: retrieval rides
/// Phase 50 D-06's existing 100 ms cold-path poll, exactly as waveform peaks,
/// filmstrip strips and proxy status already do, so no seventh event tag went
/// with this either — `ring::EVENT_NAMES` stays at 6, the closed D-02 set
/// shared across this ABI by both shells. That is the FOURTH time this pattern
/// has been used. A push event may be added by a later phase **when a shell
/// region actually consumes it**; today nothing does, because 59-CONTEXT D-34
/// freezes shell work while the cutover is owner-blocked.
///
/// Phase 71 (TRUST-03): `committed_total` is the SIXTH field, appended LAST — a
/// process-lifetime count of segments actually committed. Difference it against
/// a baseline to get this session's count; `cached_segments` /
/// `heavy_segments` are censuses and must not be shown as progress (D-63-04-02).
#[no_mangle]
pub extern "C" fn rudis_get_render_cache_status(
    ctx: *mut RudisCtx,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_get_render_cache_status", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| {
            app_core::render_cache_job::run_get_render_cache_status(&FfiAppCtx::new(c))
        })
    })
}

/// `get_current_seq` — the store's mutation counter, the resync anchor a
/// poller pairs with [`rudis_get_snapshot`] after a `resync_required` poll.
#[no_mangle]
pub extern "C" fn rudis_get_current_seq(ctx: *mut RudisCtx, out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_get_current_seq", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| {
            app_core::run_get_current_seq(&FfiAppCtx::new(c))
        })
    })
}

/// `debug_mark_interactive` — the LAT-01 cold-launch marker hook (a no-op
/// unless `RUDIS_LAT01_MARKER_PATH` is set). Returns `()` — the envelope is
/// `{"Ok": null}`, keeping the uniform shape across all 17 commands.
#[no_mangle]
pub extern "C" fn rudis_debug_mark_interactive(
    ctx: *mut RudisCtx,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_debug_mark_interactive", RudisStatus::PanicCaught, {
        call_out(ctx, out, |_c| {
            app_core::run_debug_mark_interactive();
            Ok::<(), String>(())
        })
    })
}

/// Args for [`rudis_debug_seed_project`]: `{"project": {..rudis_core::Project..}}`.
#[derive(serde::Deserialize)]
struct DebugSeedProjectArgs {
    project: rudis_core::Project,
}

/// `debug_seed_project` — DEBUG-GATED whole-project seed for the Phase 54 C#
/// live-eval harness (54-CONTEXT D-12/D-13), and for NOTHING else. Envelope
/// `{"Ok": null}`.
///
/// # Gate
///
/// Refuses with a DOMAIN error unless `RUDIS_DEBUG_SEED_PROJECT` is set in the
/// process environment (a PRESENCE check — any value, including empty, opens
/// it). That is [`rudis_debug_mark_interactive`]'s pattern and Phase 50 D-10's
/// rule: off by default, and *asserted* off by a contract test rather than
/// merely documented off. The check runs on EVERY call and latches nothing, so
/// a successful seed does not leave the door open behind it. The shipping shell
/// never sets the variable and no production caller has a reason to.
///
/// # Why this is not a production export (D-13's rejected alternative, recorded)
///
/// A whole-project overwrite silently discards the open project AND its undo
/// stack, and has no defined undo semantics of its own. Shipping that with no
/// production caller would be a hazard, not a feature. A real
/// `open_project`/`new_project` pair was left to Phase 55, when the C# shell
/// took over the project lifecycle from the retiring Tauri shell. It is still
/// NOT exported across this ABI — `app_core::project::run_new_project` /
/// `run_open_project` exist but no C export reaches them yet.
///
/// # Scope — each omission below is deliberate
///
/// - Replaces the store via `rudis_core::Store::from_project`, which starts
///   `seq` at 0 with empty undo/redo stacks — the SAME constructor the Rust
///   live gate uses (`crates/agent-llm/tests/agent_eval_live.rs:572`), so the
///   C# harness and the Rust harness seed identically.
/// - Resets `agent_session` to default. The harness builds a FRESH ctx per
///   fixture, so this is defense-in-depth against misuse on a long-lived ctx,
///   NOT something any caller depends on (54-02's resolution of 54-RESEARCH
///   Open Question 1 — recorded here, at the definition site).
/// - Emits NOTHING into the event ring and calls no `observe_preview_patch`: a
///   LIVE shell mirror calling this would desync until its next full resync.
///   One more reason the gate exists. The harness never polls; it reads the
///   result back through [`rudis_get_snapshot`].
#[no_mangle]
pub extern "C" fn rudis_debug_seed_project(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_debug_seed_project", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: DebugSeedProjectArgs| {
            // THE GATE. Checked before ANY store access, so a refusal cannot
            // half-apply — the contract test asserts the store is byte-unchanged
            // afterwards, not merely that this returned an error (T-54-03).
            if std::env::var_os("RUDIS_DEBUG_SEED_PROJECT").is_none() {
                return Err::<(), String>(
                    "debug project seeding is disabled (set RUDIS_DEBUG_SEED_PROJECT)".to_string(),
                );
            }
            *c.store
                .lock()
                .map_err(|_| "backend store mutex poisoned".to_string())? =
                rudis_core::Store::from_project(args.project);
            *c.agent_session
                .lock()
                .map_err(|_| "agent session mutex poisoned".to_string())? =
                app_core::AgentSession::default();
            Ok::<(), String>(())
        })
    })
}

/// `undo` — `{"Ok": {..Patch..}}`, or `{"Ok": null}` with nothing to undo.
/// The inverse patch's `project:changed` push happens inside `undo_inner` via
/// `FfiAppCtx::emit_patch` — zero wiring here.
///
/// Phase 51 (SHELL-04): the inverse patch ALSO reaches the committed-ink
/// mirror through [`RudisCtx::observe_preview_patch`] — an undone
/// `AddAnnotation` must un-ink the preview, which is exactly the case a
/// dispatch-only hook would miss. Nothing to undo (`None`) observes nothing.
#[no_mangle]
pub extern "C" fn rudis_undo(ctx: *mut RudisCtx, out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_undo", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| {
            let undone = app_core::undo_inner(&FfiAppCtx::new(c))?;
            if let Some(patch) = undone.as_ref() {
                c.observe_preview_patch(patch);
            }
            Ok(undone)
        })
    })
}

/// `redo` — [`rudis_undo`]'s exact mirror, preview hook included.
#[no_mangle]
pub extern "C" fn rudis_redo(ctx: *mut RudisCtx, out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_redo", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| {
            let redone = app_core::redo_inner(&FfiAppCtx::new(c))?;
            if let Some(patch) = redone.as_ref() {
                c.observe_preview_patch(patch);
            }
            Ok(redone)
        })
    })
}

/// Args for [`rudis_place_clip`]:
/// `{"media_id": "..", "track": 0, "start_us": 0}`.
#[derive(serde::Deserialize)]
struct PlaceClipArgs {
    media_id: String,
    track: usize,
    start_us: i64,
}

/// `place_clip` — place a MediaBin item on the timeline; `{"Ok": {..Clip..}}`.
#[no_mangle]
pub extern "C" fn rudis_place_clip(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_place_clip", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: PlaceClipArgs| {
            app_core::run_place_clip(&FfiAppCtx::new(c), args.media_id, args.track, args.start_us)
        })
    })
}

/// Args for [`rudis_apply_option_card`]: `{"card_id": ".."}`.
#[derive(serde::Deserialize)]
struct ApplyOptionCardArgs {
    card_id: String,
}

/// `apply_option_card` — apply one pending option card by id (CANV-02);
/// `{"Ok": [{..Patch..}, ..]}`. The inner's `(Patch, base_seq, seq)` triples
/// are mapped to bare `Patch`es exactly as the Tauri wrapper does: the seq
/// pair is an EVENT-envelope concern (Phase 43 D-06), already emitted through
/// `FfiAppCtx::emit_patch` by the time this returns.
///
/// Phase 51 (SHELL-04): an option card can mint annotations, so EVERY patch it
/// produced is observed by the committed-ink mirror, in order.
#[no_mangle]
pub extern "C" fn rudis_apply_option_card(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_apply_option_card", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: ApplyOptionCardArgs| {
            let fctx = FfiAppCtx::new(c);
            let patches: Vec<rudis_core::Patch> =
                app_core::apply_option_card_inner(&fctx, fctx.agent_session(), &args.card_id)
                    .map(|patches| patches.into_iter().map(|(p, _, _)| p).collect())?;
            for patch in &patches {
                c.observe_preview_patch(patch);
            }
            Ok(patches)
        })
    })
}

// ---------------------------------------------------------------------------
// transport + the auth trio + the two polls (plan 47-05 Task 2).
// ---------------------------------------------------------------------------

/// Args for [`rudis_transport`]: `{"cmd": {..TransportCmd..}}` — the
/// adjacently-tagged `rudis_core::TransportCmd` wire shape
/// (`{"type": "seek", "data": {"position_us": ..}}`), unchanged from today's
/// IPC.
#[derive(serde::Deserialize)]
struct TransportArgs {
    cmd: rudis_core::TransportCmd,
}

/// `transport` — apply one playback command; `{"Ok": {..Playback..}}`.
///
/// The store half is `app_core::run_transport`; everything after it is this
/// host's CONSEQUENCE half, twinning the Tauri wrapper line-for-line
/// (`app_core::TransportOutcome`'s documented split): publish to the
/// lock-free playback mirror, bump `seek_seq` on a reposition (Seek/Step
/// only, 18.2-05), then push `playback:changed` into the ring — the ring push
/// IS this host's `app.emit(PLAYBACK_CHANGED_EVENT, ..)`, same payload type
/// (`Playback`). On a domain `Err` the host half never runs, exactly as
/// Tauri's `?` skips it.
#[no_mangle]
pub extern "C" fn rudis_transport(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_transport", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: TransportArgs| {
            let outcome = app_core::run_transport(&FfiAppCtx::new(c), args.cmd)?;
            c.mirror.update(&outcome.playback, outcome.is_source);
            if outcome.reposition {
                c.mirror.seek_seq.fetch_add(1, Ordering::Relaxed);
            }
            let payload = serde_json::to_value(&outcome.playback)
                .map_err(|e| format!("failed to emit {}: {e}", ring::EVENT_PLAYBACK_CHANGED))?;
            c.ring.push(ring::EVENT_PLAYBACK_CHANGED, payload);
            Ok(outcome.playback)
        })
    })
}

/// `agent_status` — `{"Ok": {"key_configured": bool, "source": "credential_manager"|
/// "environment"|"none", "providers": {"runway": {"key_configured": bool, "source": ..}}}}`
/// (Phase 69, D-69-14: `source`/`providers` are ADDITIVE). Only booleans and a
/// source enum — key material, its length or prefix never cross back over the
/// ABI (T-47-13 / T-12-11).
///
/// `run_agent_status` returns a bare `AgentStatusView` (it cannot fail);
/// wrapped as `Ok::<_, String>` DELIBERATELY so the envelope shape stays
/// uniform across all 17 commands — the C# side parses one shape, not two.
#[no_mangle]
pub extern "C" fn rudis_agent_status(ctx: *mut RudisCtx, out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_agent_status", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| {
            Ok::<_, String>(app_core::run_agent_status(c.key_store.as_ref(), &c.provider_key_store))
        })
    })
}

/// Args for [`rudis_set_api_key`]: `{"key": ".."}`. Inbound only — the key
/// goes INTO the ctx's key store and is never echoed back (T-47-13).
#[derive(serde::Deserialize)]
struct SetApiKeyArgs {
    key: String,
}

/// `set_api_key` — persist the user's own Anthropic key (BYO-key, AUTH-02);
/// `{"Ok": null}`, or `{"Err": ".."}` for a malformed key (never stored).
#[no_mangle]
pub extern "C" fn rudis_set_api_key(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_set_api_key", RudisStatus::PanicCaught, {
        call_json_redacted(ctx, json, len, out, "set_api_key", |c, args: SetApiKeyArgs| {
            app_core::run_set_api_key(c.key_store.as_ref(), &args.key)
        })
    })
}

/// `clear_api_key` — remove the stored key; idempotent, `{"Ok": null}`.
#[no_mangle]
pub extern "C" fn rudis_clear_api_key(ctx: *mut RudisCtx, out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_clear_api_key", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| app_core::run_clear_api_key(c.key_store.as_ref()))
    })
}

/// Args for [`rudis_set_provider_key`]: `{"provider": "runway", "key": ".."}`.
/// Inbound only — the key goes INTO the provider's credential slot and is
/// never echoed back (T-47-13). Phase 69 (D-69-12): ADDITIVE — the Anthropic
/// path stays `rudis_set_api_key`, byte-unchanged.
#[derive(serde::Deserialize)]
struct SetProviderKeyArgs {
    provider: String,
    key: String,
}

/// Args for [`rudis_clear_provider_key`]: `{"provider": "runway"}`.
#[derive(serde::Deserialize)]
struct ClearProviderKeyArgs {
    provider: String,
}

/// `set_provider_key` — persist a generation provider's key in the OS
/// credential store under its `PROVIDER_KEY_SLOTS` account, then INVALIDATE
/// the provider slots so the key takes effect on next use, no restart
/// (D-69-13). Provider ids resolve ONLY through
/// `app_core::SETTINGS_PROVIDER_ALLOW_LIST` (`{"runway"}`) +
/// `PROVIDER_KEY_SLOTS` (T-31-04 / T-69-08): anything else is
/// `{"Err": "provider '..' is not managed by Settings (allowed: runway)"}` —
/// the error names the provider, never the key. A malformed key is refused
/// with the validator's rule text and never stored. `{"Ok": null}` on success.
#[no_mangle]
pub extern "C" fn rudis_set_provider_key(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_set_provider_key", RudisStatus::PanicCaught, {
        call_json_redacted(ctx, json, len, out, "set_provider_key", |c, args: SetProviderKeyArgs| {
            let fctx = FfiAppCtx::new(c);
            app_core::run_set_provider_key(
                &c.provider_key_store,
                &fctx.gen_host(),
                &args.provider,
                &args.key,
            )
        })
    })
}

/// `clear_provider_key` — remove the provider slot's stored key; idempotent,
/// `{"Ok": null}`. Same allow-list as [`rudis_set_provider_key`], and the same
/// invalidation: the next generation submit re-resolves (and, with no
/// environment key either, refuses with the Settings-pointing text).
#[no_mangle]
pub extern "C" fn rudis_clear_provider_key(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_clear_provider_key", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: ClearProviderKeyArgs| {
            let fctx = FfiAppCtx::new(c);
            app_core::run_clear_provider_key(&c.provider_key_store, &fctx.gen_host(), &args.provider)
        })
    })
}

/// `rudis_poll_events` — drain the ctx's event ring, non-blocking (the cold
/// path's ONLY delivery mechanism). Envelope: `{"Ok": {"resync_required":
/// bool, "next_seq": u64, "events": [{"seq": .., "event": "..",
/// "payload": ..}, ..]}}` — `ring::PollOutcome` serialized whole.
///
/// D-09: host-polls-native. There is NO callback from Rust into C#, so the
/// reverse-P/Invoke + DispatcherQueue re-marshal hazard class does not exist
/// at this boundary at all. D-12: non-blocking ONLY — polling never parks the
/// caller, and no blocking variant exists this phase; Phase 50, the first
/// real consumer, picks its own cadence. On `resync_required` the caller
/// refetches via [`rudis_get_snapshot`] + [`rudis_get_current_seq`] (S8's
/// recovery protocol, unchanged from the shell side).
#[no_mangle]
pub extern "C" fn rudis_poll_events(
    ctx: *mut RudisCtx,
    local_seq: u64,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_poll_events", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| Ok::<_, String>(c.ring.poll(local_seq)))
    })
}

/// `rudis_get_playback_position` — the HOT path: a lock-free scalar read of
/// the playback mirror's `position_us` atomic, called at poll rate by the
/// host's transport UI. `research/v7-ARCHITECTURE.md:101` is explicit that
/// this path needs NO event mechanism at all — no envelope, no allocation,
/// no JSON, just the `i64`.
///
/// `i64::MIN` is the out-of-band sentinel for BOTH a null handle and a caught
/// panic (a real position is clamped to `[0, duration]` and can never be
/// `i64::MIN`). Deliberately not over-built: no playing/duration scalar twins
/// until a consumer exists (Phase 50 adds what it measures a need for).
#[no_mangle]
pub extern "C" fn rudis_get_playback_position(ctx: *mut RudisCtx) -> i64 {
    crate::ffi_guard!("rudis_get_playback_position", i64::MIN, {
        if ctx.is_null() {
            return i64::MIN;
        }
        // SAFETY: non-null by the check above; same live-handle contract as
        // every other export's deref.
        let c = unsafe { &*ctx };
        c.mirror.position_us.load(Ordering::Relaxed)
    })
}

/// `rudis_get_playback_resolution_level` — PLAY-05's observable (Phase 57, plan
/// 57-08). A lock-free scalar read of the engine's active dynamic-playback-
/// resolution level: **`0` = full, `1` = half, `2` = quarter**.
///
/// The pattern is [`rudis_get_playback_position`]'s, deliberately copied rather
/// than reinvented — same shape, same `ffi_guard!`, same out-of-band sentinel
/// discipline, one poll-rate scalar with no envelope, no allocation and no JSON.
/// CONTEXT D-01 asks for "engine state through the existing envelope"; this is
/// that envelope.
///
/// `i32::MIN` is the sentinel for BOTH a null handle and a caught panic. A real
/// level is one of `{0, 1, 2}` (and an unknown byte from a future writer
/// saturates to `0`, "nothing is degraded"), so it can never collide.
///
/// # Direction, and why it is a different struct
///
/// The position getter reads a mirror the SHELL writes. This reads one the
/// ENGINE writes — the preview producer's multi-layer arm, which is the only
/// writer. That is why it hangs off `RudisCtx::diag` rather than becoming a
/// sixth atomic on `PlaybackMirror`: one struct, one writer.
///
/// # No consumer in v8, on purpose
///
/// D-01 says v8 adds nothing to either shell, so nothing calls this yet.
/// Rendering a "1/2" indicator in the Transport region is post-cutover work.
/// The export existing, crossing the ABI, and being pinned end-to-end against a
/// real degradation IS the deliverable — the same standard `rudis_poll_events`
/// was held to before Phase 50 had a consumer for it.
///
/// # What it can never report
///
/// Anything about export. Playback resolution is transient engine state that
/// lives as a `!Send` stack local on one producer thread (D-11/D-12); the
/// degraded level is structurally unreachable from the export path, which runs
/// in crates that cannot even name the type.
#[no_mangle]
pub extern "C" fn rudis_get_playback_resolution_level(ctx: *mut RudisCtx) -> i32 {
    crate::ffi_guard!("rudis_get_playback_resolution_level", i32::MIN, {
        if ctx.is_null() {
            return i32::MIN;
        }
        // SAFETY: non-null by the check above; same live-handle contract as
        // every other export's deref.
        let c = unsafe { &*ctx };
        c.diag.playback_res_level.load(Ordering::Relaxed) as i32
    })
}

// ---------------------------------------------------------------------------
// The 4 async commands (plan 47-05 Task 3). Every one crosses the boundary
// SYNCHRONOUSLY (RESEARCH E1: never an `async` item at the ABI): the body is
// the same call_json plumbing, with `fctx.block_on(..)` driving the future on
// the ctx-owned multi-thread Tokio runtime (spawn_blocking-safe — proven by
// 47-04's runtime test).
// ---------------------------------------------------------------------------

/// Args for [`rudis_import_media`] AND [`rudis_import_media_folder`] — both
/// take exactly `{"paths": [".."]}`, as their Tauri twins do.
#[derive(serde::Deserialize)]
struct ImportArgs {
    paths: Vec<String>,
}

/// `import_media` — probe + poster + register each path;
/// `{"Ok": [{..MediaBinItem..}, ..]}`.
///
/// Per-file `project:changed` pushes happen automatically via
/// `FfiAppCtx::emit_patch` inside the moved per-call loop — zero extra wiring
/// here (RESEARCH A4's monomorphization dividend: the SAME `run_*` body
/// emits into the ring on this host and into the WebView on the shell host).
#[no_mangle]
pub extern "C" fn rudis_import_media(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_import_media", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: ImportArgs| {
            let fctx = FfiAppCtx::new(c);
            fctx.block_on(app_core::run_import_media_ui(&fctx, args.paths))
        })
    })
}

/// `import_media_folder` — recursive folder import as ONE undo turn, at the
/// SAME production clamps the Tauri wrapper uses (500 files / depth 12, the
/// T-47-09-class DoS bounds); `{"Ok": [{..MediaBinItem..}, ..]}`.
#[no_mangle]
pub extern "C" fn rudis_import_media_folder(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_import_media_folder", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: ImportArgs| {
            let fctx = FfiAppCtx::new(c);
            fctx.block_on(app_core::run_import_media_folder_ui(
                &fctx,
                args.paths,
                app_core::MAX_FOLDER_IMPORT_FILES,
                app_core::MAX_FOLDER_IMPORT_DEPTH,
            ))
        })
    })
}

/// Args for [`rudis_export_timeline`]:
/// `{"out_path": "..", "width": 1280, "height": 720, "fps": 30.0}`.
#[derive(serde::Deserialize)]
struct ExportTimelineArgs {
    out_path: String,
    width: u32,
    height: u32,
    fps: f64,
}

/// `export_timeline` — render + encode the timeline to `out_path`;
/// `{"Ok": "<written path>"}`. Blocks the calling thread for the whole
/// encode (the C# host runs it off its UI thread — Phase 50's concern).
/// `export:progress` records land in the ring via 47-04's
/// `export_progress_sink` — zero extra wiring here.
#[no_mangle]
pub extern "C" fn rudis_export_timeline(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_export_timeline", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: ExportTimelineArgs| {
            let fctx = FfiAppCtx::new(c);
            fctx.block_on(app_core::run_export(
                &fctx,
                std::path::PathBuf::from(args.out_path),
                Some(args.width),
                Some(args.height),
                Some(args.fps),
            ))
        })
    })
}

/// Args for [`rudis_agent_send_message`]:
/// `{"message": "..", "selection": ["clip-1", ..]}`.
#[derive(serde::Deserialize)]
struct AgentSendMessageArgs {
    message: String,
    selection: Vec<String>,
}

/// `agent_send_message` — run one real Chat turn;
/// `{"Ok": {..AgentTurnOutcome..}}`. Mirrors the Tauri body exactly:
/// key-gated `AnthropicTransport::connect` (with NO key configured this
/// errors CLEANLY with `agent_llm::NO_API_KEY_MESSAGE` — the refusal that
/// points at Settings, Phase 69 D-69-09 — pointing at the surface that
/// can fix it; 47-06's offline contract test asserts the const), then the backend-resolved
/// `agent_library` dir (never caller-supplied, T-14-15), then the SAME
/// `run_agent_turn` every host drives.
///
/// Honest scope (D-03): a turn through this host is TEXT-ONLY — 47-04's
/// fail-closed `GenSubmission`/`SpendPolicy` impls halt any generation tool
/// call pre-dispatch with the D-03 question, and `AgentVision` degrades to
/// text-only. No paid call can fire through this export (T-47-10).
#[no_mangle]
pub extern "C" fn rudis_agent_send_message(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_agent_send_message", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: AgentSendMessageArgs| {
            let fctx = FfiAppCtx::new(c);
            fctx.block_on(async {
                let transport = agent_llm::AnthropicTransport::connect(c.key_store.as_ref())
                    .ok_or_else(|| agent_llm::NO_API_KEY_MESSAGE.to_string())?;
                let library_dir = fctx.app_data_dir()?.join("agent_library");
                app_core::run_agent_turn(
                    &fctx,
                    fctx.agent_session(),
                    &transport,
                    args.message,
                    args.selection,
                    &library_dir,
                )
                .await
            })
        })
    })
}

// ---------------------------------------------------------------------------
// Phase 60.1 (plan 60.1-03): the project lifecycle across the ABI.
//
// `run_new_project` / `run_open_project` have been implemented and tested in
// `app-core` since Phase 26 and have had NO HOST since GATE-07 deleted
// `src-tauri`; their save/open siblings landed in plan 60.1-02. Every arrow on
// the path from a menu item to `write_project_atomic` already existed and was
// proven — except `run_x` <- `rudis_x`. These exports are that arrow, and
// until they landed the shipped app could not save or open a project at all.
//
// Same two shapes as everything above: one `ffi_guard!` wrapping ONE
// `call_out`/`call_json` call, zero bespoke logic per export.
// ---------------------------------------------------------------------------

/// Args for [`rudis_new_project`] and [`rudis_open_project`] — both take
/// exactly `{"name": ".."}`, as their `app-core` functions do.
#[derive(serde::Deserialize)]
struct ProjectNameArgs {
    name: String,
}

/// Turn an `app-core` reader's SERIALIZED JSON string into a real JSON value,
/// so the envelope carries `{"Ok": {..}}` rather than `{"Ok": "{..}"}`.
///
/// Several of the project functions return `Result<String, String>` whose `Ok`
/// is already JSON text (`run_get_projects` set that precedent for host-facing
/// reads). Passing that `String` straight to [`call_out`] would double-encode
/// it, and the C# `RunOut`/`RunJson` pair — which hands the caller a
/// `JsonElement` — would deliver a `JsonValueKind.String` that every region
/// then has to `JsonDocument.Parse` by hand. One parse here, in the one place,
/// instead of one parse per consumer that can be forgotten.
///
/// A parse failure is an `app-core` bug rather than caller input, so it rides
/// the domain envelope like any other backend error; it cannot be provoked
/// across the ABI.
fn json_payload(produced: Result<String, String>) -> Result<serde_json::Value, String> {
    serde_json::from_str(&produced?)
        .map_err(|e| format!("backend produced a non-JSON payload: {e}"))
}

/// `new_project` — create a brand-new, empty, NAMED project and switch to it.
///
/// Args `{"name": ".."}`; envelope `{"Ok": "<prose>"}` (agent-facing prose,
/// unchanged since Phase 26 — this export is a new HOST for it, not a new
/// contract). The name goes through `sanitize_project_name` BEFORE any path
/// join (T-26-01), so a reserved Windows device name, a path separator or an
/// over-long name comes back as a DOMAIN error with transport status `Ok`:
/// text a dialog can print, never a transport fault.
///
/// Side effects the caller must expect, because they are the point: the
/// OUTGOING project is autosaved first (hook a), the new one is written to
/// `<app-data>/projects/<name>.rud` immediately (hook b), undo/redo is reset,
/// and one `project:changed` record lands in the ring carrying the structural
/// `ProjectSwitched` patch — the shell full-resyncs on it. No seventh event
/// type exists for any of this.
#[no_mangle]
pub extern "C" fn rudis_new_project(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_new_project", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: ProjectNameArgs| {
            let fctx = FfiAppCtx::new(c);
            // `run_new_project` takes a `&Value`, so the Value is BUILT from
            // the typed field rather than the caller's bytes being passed
            // through: the ABI's argument shape stays a real typed contract
            // and an extra key in the caller's JSON reaches nothing.
            app_core::project::run_new_project(&fctx, &serde_json::json!({ "name": args.name }))
        })
    })
}

/// `open_project` — switch to a previously created project **by name**.
///
/// Args `{"name": ".."}`; envelope `{"Ok": "<prose>"}`, or
/// `{"Err": "no known project named \"..\" — call get_projects to see what
/// exists"}` with status `Ok`.
///
/// ⚠ The name is a LOOKUP KEY, never a path component: `app-core` resolves it
/// exclusively through the real on-disk registry (`scan_known_projects`) and
/// never joins caller text into a path. That is T-26-03, and it is deliberately
/// stricter than accepting any path the caller names, because this `name` may
/// have come from the LLM. A user who picked a file from a dialog wants
/// [`rudis_open_project_at_path`] instead — a separate export over a separate
/// function taking a separate type, which is what keeps this route narrow.
#[no_mangle]
pub extern "C" fn rudis_open_project(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_open_project", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: ProjectNameArgs| {
            let fctx = FfiAppCtx::new(c);
            app_core::project::run_open_project(&fctx, &serde_json::json!({ "name": args.name }))
        })
    })
}

/// Args for [`rudis_open_project_at_path`]: `{"path": "..\\Foo.rud"}`.
///
/// ⚠ The field's TYPE is the validation. `RudProjectPath`'s hand-written
/// `Deserialize` is its only constructor and it canonicalises, requires an
/// existing regular file, requires the `.rud` extension and caps the file's
/// size — so this struct structurally cannot hold an unvalidated path and the
/// export body below has, deliberately, no check of its own to forget.
#[derive(serde::Deserialize)]
struct OpenProjectAtPathArgs {
    path: app_core::project_store::RudProjectPath,
}

/// `open_project_at_path` — open a `.rud` the USER picked, from ANYWHERE on
/// disk.
///
/// Args `{"path": ".."}`; envelope `{"Ok": {"name": "..", "path": ".."}}`, and
/// the `name` is read from the LOADED document rather than the file stem (a
/// user who renamed the file in Explorer has not renamed the project inside
/// it).
///
/// **The path is USER-chosen, and the validation is a property of the
/// argument's TYPE.** Rudis has accepted user-picked absolute paths across this
/// same ABI since Phase 47 (`rudis_import_media`); what T-26-03 defends against
/// is an LLM-chosen path, and the distinction is *who chose it*. Every refusal
/// — a directory, a non-`.rud`, a file that is not there, a file above the size
/// cap — therefore arrives from the deserialize as `{"Err": "invalid
/// arguments: .."}` with transport status `Ok`, which is exactly what a file
/// dialog needs in order to say something useful.
///
/// Same side effects as [`rudis_open_project`]: the outgoing project is
/// autosaved, in-flight proxy and render-cache work is cancelled and both
/// registries are forgotten, undo/redo resets, and ONE `project:changed`
/// record carries the structural switch.
#[no_mangle]
pub extern "C" fn rudis_open_project_at_path(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_open_project_at_path", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: OpenProjectAtPathArgs| {
            let fctx = FfiAppCtx::new(c);
            json_payload(app_core::project::run_open_project_at_path(&fctx, args.path))
        })
    })
}


/// `save_project` — persist the LIVE store to the active project's file.
///
/// No args; envelope `{"Ok": {"path": "..", "seq": N}}`. `seq` is the store's
/// mutation counter captured in the SAME guard as the bytes (LAT-02's rule), so
/// a host can tell whether the store has moved since — there is deliberately no
/// separate "is dirty" export, and after any open or new the seq is 0 with the
/// project already on disk, so "nothing unsaved" is true by construction rather
/// than by a flag someone has to remember to clear.
///
/// # It MINTS rather than refusing when nothing is active
///
/// With no active project this does **not** fail: it creates
/// `<app-data>/projects/Untitled.rud` (then `Untitled 2`, `Untitled 3`, … —
/// there is no `Untitled 1`), renames the live document to match, points the
/// active-project pointer at it and writes. A modal asking a beginner to name a
/// file **before** their work is safe is precisely the moment the work gets
/// lost, and this is what lets a close path have no dialog and no branch at
/// all: close always saves. The minting save — and ONLY it — also pushes one
/// `project:changed` record, because it renamed the live document; an ordinary
/// Ctrl+S emits nothing, since a full resync on every save would make the
/// commonest operation in the app the most expensive one.
///
/// Deliberately host-only: there is no agent tool behind this, because an LLM
/// that could silently overwrite the user's file on disk is not a capability
/// this ABI wants.
#[no_mangle]
pub extern "C" fn rudis_save_project(ctx: *mut RudisCtx, out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_save_project", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| {
            json_payload(app_core::project::run_save_project(&FfiAppCtx::new(c)))
        })
    })
}

/// Args for [`rudis_save_project_as`]: `{"path": "..\\Chosen Name.rud"}`.
///
/// ⚠ The field's TYPE is the validation, and it is a DIFFERENT type from
/// [`OpenProjectAtPathArgs`]'s on purpose: an open target must already exist, a
/// save target must not have to. `RudSaveTargetPath` canonicalises the PARENT
/// folder only, requires the `.rud` extension, and puts the final component
/// through `sanitize_project_name` — the same T-26-01 owner, returning its
/// message verbatim rather than a drifting second copy of those rules. That is
/// what keeps the `NUL.rud` sink closed on this door too: Windows resolves it
/// to the null device, a writer that reports success and discards every byte.
#[derive(serde::Deserialize)]
struct SaveProjectAsArgs {
    path: app_core::project_store::RudSaveTargetPath,
}

/// `save_project_as` — persist to a user-chosen path **and re-point the active
/// document there**.
///
/// Args `{"path": ".."}`; envelope
/// `{"Ok": {"name": "..", "path": "..", "seq": N}}`. The universal NLE
/// convention: after Save As you are editing the copy, and every later save —
/// including the one on close — lands at the new path. A Save As that wrote a
/// copy and left you editing the original is the shape that loses the next
/// hour of work. The live project takes the target's file stem as its name, so
/// the TitleBar and the next registry scan agree with the filename the user
/// just chose, and ONE `project:changed` record carries that rename.
///
/// ⚠ The returned `path` is the CANONICALISED target (on Windows that is the
/// extended-length `\\?\C:\..` form, because the parent folder went through
/// `std::fs::canonicalize`). A host that wants to display it should shorten it
/// for presentation; a host that wants to compare it must canonicalise the
/// other side rather than compare the strings.
///
/// A folder that does not exist, a name that is not a legal project name, or a
/// missing `.rud` extension is refused by the argument's TYPE, before any
/// write, as `{"Err": "invalid arguments: .."}` with transport status `Ok`.
#[no_mangle]
pub extern "C" fn rudis_save_project_as(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_save_project_as", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: SaveProjectAsArgs| {
            let fctx = FfiAppCtx::new(c);
            json_payload(app_core::project::run_save_project_as(&fctx, args.path))
        })
    })
}

/// `get_projects` — the HUMAN's project list.
///
/// No args; envelope
/// `{"Ok": [{"isActive": bool, "modifiedUnixMs": N, "name": "..", "path": ".."}, ..]}`
/// (keys serialize alphabetically — `serde_json`'s map is a `BTreeMap` here, so
/// a host must not assume insertion order).
///
/// ⚠ **This export is named `get_projects` and it calls
/// `run_get_projects_DETAILED`, on purpose.** `app-core`'s `run_get_projects`
/// is the AGENT tool's view and withholds the filesystem path deliberately
/// (T-26-08, minimal disclosure) — and a person choosing between two projects
/// both called "Untitled" needs exactly the path and the mtime that posture
/// withholds. One export, two contracts, neither widened: nothing here can
/// widen what the LLM sees, and nothing the LLM sees constrains what a file
/// list can show its owner.
#[no_mangle]
pub extern "C" fn rudis_get_projects(ctx: *mut RudisCtx, out: *mut RudisBuffer) -> RudisStatus {
    crate::ffi_guard!("rudis_get_projects", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| {
            json_payload(app_core::project::run_get_projects_detailed(&FfiAppCtx::new(c)))
        })
    })
}

/// `get_missing_media` — which of the ACTIVE project's media files are not on
/// disk right now, by **id**.
///
/// No args; envelope `{"Ok": ["<media id>", ..]}`, and `{"Ok": []}` when no
/// project is active (an absent project has no missing media, and that is not
/// an error). Ids rather than paths: T-26-08's posture, and the host already
/// holds each item's path in its own mirror while the MediaBin keys its tiles
/// by id.
///
/// A **poll**, deliberately not a seventh event type — `ring::EVENT_NAMES` has
/// stayed at 6 through five consecutive phases that each added exactly one
/// export, and retrieval rides the existing 100 ms cold-path poll exactly as
/// waveform peaks, filmstrip strips, proxy status and render-cache status
/// already do. Cheap by construction: it snapshots the store and DROPS the
/// guard before it stats anything, so the interop worker that also services
/// `rudis_poll_events` is never blocked across a cold directory walk.
#[no_mangle]
pub extern "C" fn rudis_get_missing_media(
    ctx: *mut RudisCtx,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_get_missing_media", RudisStatus::PanicCaught, {
        call_out(ctx, out, |c| {
            json_payload(app_core::project::run_get_missing_media(&FfiAppCtx::new(c)))
        })
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::InitConfig;

    /// A raw ctx exactly as the ABI hands one out (the exports deref it the
    /// same way they will a C#-held handle).
    fn raw_ctx() -> *mut RudisCtx {
        Box::into_raw(Box::new(
            RudisCtx::new_in_process(
                InitConfig::default(),
                Box::new(agent_llm::InMemoryKeyStore::new()),
            )
            .expect("in-process ctx builds"),
        ))
    }

    fn free_ctx(ctx: *mut RudisCtx) {
        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    fn out_buf() -> RudisBuffer {
        RudisBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }

    /// Read the envelope out of a filled buffer and free it.
    fn take_json(buf: RudisBuffer) -> serde_json::Value {
        assert!(!buf.ptr.is_null(), "a filled buffer has a real allocation");
        let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        let value = serde_json::from_slice(bytes).expect("envelope is valid JSON");
        crate::rudis_free_buffer(buf);
        value
    }

    /// D-06's transport-fault classes, through REAL exports (the helpers are
    /// the only path, so one export per class covers all of them): null ctx →
    /// InvalidHandle, null out → NullPointer, null json with len > 0 →
    /// NullPointer, invalid UTF-8 → InvalidUtf8 (T-47-05).
    #[test]
    fn transport_faults_map_to_the_d06_statuses() {
        let mut buf = out_buf();
        assert_eq!(
            rudis_get_snapshot(std::ptr::null_mut(), &mut buf),
            RudisStatus::InvalidHandle
        );
        assert!(buf.ptr.is_null(), "a faulted call never touches out");

        let ctx = raw_ctx();
        assert_eq!(
            rudis_get_snapshot(ctx, std::ptr::null_mut()),
            RudisStatus::NullPointer
        );
        assert_eq!(
            rudis_get_entities(ctx, std::ptr::null(), 5, &mut buf),
            RudisStatus::NullPointer,
            "a null args ptr with a nonzero len is a transport fault"
        );
        let bad_utf8 = [0xFFu8, 0xFE, 0xFD];
        assert_eq!(
            rudis_get_entities(ctx, bad_utf8.as_ptr(), bad_utf8.len(), &mut buf),
            RudisStatus::InvalidUtf8
        );
        assert!(buf.ptr.is_null(), "no fault above wrote an envelope");
        free_ctx(ctx);
    }

    /// The DOMAIN side of D-06: args that decode as UTF-8 but fail serde —
    /// malformed JSON or a shape mismatch — come back as `{"Err": ".."}` with
    /// status Ok, exactly as a bad Tauri payload errors the invoke.
    #[test]
    fn bad_args_are_a_domain_error_never_a_transport_fault() {
        let ctx = raw_ctx();
        for bad in [&b"not json at all"[..], br#"{"ids": 42}"#] {
            let mut buf = out_buf();
            assert_eq!(
                rudis_get_entities(ctx, bad.as_ptr(), bad.len(), &mut buf),
                RudisStatus::Ok,
                "bad args error the invoke, not the transport"
            );
            let envelope = take_json(buf);
            assert!(
                envelope.get("Err").is_some(),
                "domain error envelope, got {envelope}"
            );
        }
        free_ctx(ctx);
    }

    /// A good no-args call and a good JSON-args call round-trip `{"Ok": ..}`
    /// envelopes — the shape 47-02's serde test pinned, through real exports.
    #[test]
    fn good_calls_round_trip_ok_envelopes() {
        let ctx = raw_ctx();

        let mut buf = out_buf();
        assert_eq!(rudis_get_snapshot(ctx, &mut buf), RudisStatus::Ok);
        let snapshot = take_json(buf);
        assert!(
            snapshot.get("Ok").map(|p| p.get("timeline").is_some()) == Some(true),
            "the envelope carries a real Project, got {snapshot}"
        );

        let args = br#"{"ids": []}"#;
        let mut buf = out_buf();
        assert_eq!(
            rudis_get_entities(ctx, args.as_ptr(), args.len(), &mut buf),
            RudisStatus::Ok
        );
        assert_eq!(take_json(buf), serde_json::json!({ "Ok": [] }));

        // `Result<Option<_>, _>`: nothing to undo is `{"Ok": null}`, not an Err.
        let mut buf = out_buf();
        assert_eq!(rudis_undo(ctx, &mut buf), RudisStatus::Ok);
        assert_eq!(take_json(buf), serde_json::json!({ "Ok": null }));

        free_ctx(ctx);
    }

    /// The transport host half twins Tauri's: a successful command publishes
    /// to the mirror AND pushes ONE `playback:changed` ring record (same
    /// `Playback` payload as the envelope); a domain-failed command runs NO
    /// part of the host half — exactly as the Tauri wrapper's `?` skips it.
    /// A non-reposition command leaves `seek_seq` untouched (the Seek/Step
    /// `matches!` twin, 18.2-05).
    #[test]
    fn transport_host_half_publishes_mirror_and_pushes_playback_changed() {
        let ctx = raw_ctx();
        let inner = unsafe { &*ctx };

        let cmd = serde_json::json!({ "cmd": rudis_core::TransportCmd::Pause }).to_string();
        let mut buf = out_buf();
        assert_eq!(
            rudis_transport(ctx, cmd.as_ptr(), cmd.len(), &mut buf),
            RudisStatus::Ok
        );
        let envelope = take_json(buf);
        let playback: rudis_core::Playback =
            serde_json::from_value(envelope.get("Ok").expect("pause succeeds").clone())
                .expect("the Ok payload is a Playback");
        assert!(!playback.playing);

        // Host half, both organs: mirror published, ring pushed.
        assert!(!inner.mirror.playing.load(Ordering::Relaxed));
        assert_eq!(
            inner.mirror.seek_seq.load(Ordering::Relaxed),
            0,
            "Pause is not a reposition — no seek_seq bump (18.2-05)"
        );
        let polled = inner.ring.poll(0);
        assert_eq!(polled.events.len(), 1, "one push for the one command");
        assert_eq!(polled.events[0].event, crate::ring::EVENT_PLAYBACK_CHANGED);
        let ring_playback: rudis_core::Playback =
            serde_json::from_value(polled.events[0].payload.clone())
                .expect("the ring payload is the same Playback type");
        assert_eq!(ring_playback, playback, "envelope and event carry the same state");

        // A Seek with nothing loaded is a DOMAIN error — and the host half
        // must not half-run on it (no new ring record, seq untouched).
        let cmd = serde_json::json!({
            "cmd": rudis_core::TransportCmd::Seek { position_us: 500 }
        })
        .to_string();
        let mut buf = out_buf();
        assert_eq!(
            rudis_transport(ctx, cmd.as_ptr(), cmd.len(), &mut buf),
            RudisStatus::Ok
        );
        assert!(take_json(buf).get("Err").is_some(), "no preview loaded");
        assert_eq!(inner.ring.poll(0).events.len(), 1, "failed command pushed nothing");
        assert_eq!(inner.mirror.seek_seq.load(Ordering::Relaxed), 0);

        free_ctx(ctx);
    }

    /// `rudis_poll_events` serializes `PollOutcome` whole, and the hot-path
    /// scalar reads the mirror without any envelope. Null-handle → `i64::MIN`
    /// (the out-of-band sentinel).
    #[test]
    fn poll_events_and_playback_position_round_trip() {
        let ctx = raw_ctx();
        let inner = unsafe { &*ctx };

        // Empty ring: clean, empty, next_seq 0.
        let mut buf = out_buf();
        assert_eq!(rudis_poll_events(ctx, 0, &mut buf), RudisStatus::Ok);
        assert_eq!(
            take_json(buf),
            serde_json::json!({
                "Ok": { "resync_required": false, "next_seq": 0, "events": [] }
            })
        );

        // One synthetic push comes back tagged, seq'd, payload intact.
        inner
            .ring
            .push(crate::ring::EVENT_GEN_PROGRESS, serde_json::json!(7));
        let mut buf = out_buf();
        assert_eq!(rudis_poll_events(ctx, 0, &mut buf), RudisStatus::Ok);
        assert_eq!(
            take_json(buf),
            serde_json::json!({
                "Ok": {
                    "resync_required": false,
                    "next_seq": 1,
                    "events": [{ "seq": 1, "event": "gen:progress", "payload": 7 }]
                }
            })
        );

        // The hot-path scalar: fresh mirror reads 0; a published position
        // reads back; a null handle reads the sentinel.
        assert_eq!(rudis_get_playback_position(ctx), 0);
        inner.mirror.position_us.store(1_234_567, Ordering::Relaxed);
        assert_eq!(rudis_get_playback_position(ctx), 1_234_567);
        assert_eq!(rudis_get_playback_position(std::ptr::null_mut()), i64::MIN);

        // PLAY-05's twin (Phase 57, plan 57-08): a fresh ctx reads 0 (Full);
        // each level the producer can publish reads back as its documented
        // discriminant; a null handle reads the sentinel.
        assert_eq!(rudis_get_playback_resolution_level(ctx), 0);
        for (level, expect) in [
            (preview::ResLevel::Half, 1),
            (preview::ResLevel::Quarter, 2),
            (preview::ResLevel::Full, 0),
        ] {
            inner.diag.set_playback_res_level(level);
            assert_eq!(
                rudis_get_playback_resolution_level(ctx),
                expect,
                "{level:?} must cross the ABI as {expect}"
            );
        }
        assert_eq!(
            rudis_get_playback_resolution_level(std::ptr::null_mut()),
            i32::MIN,
            "a null handle reads the out-of-band sentinel, never a level"
        );

        free_ctx(ctx);
    }
}
