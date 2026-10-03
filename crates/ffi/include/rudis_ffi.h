/* rudis_ffi.h — the Rudis C ABI contract (Phase 47, FFI-03).
 *
 * COMMITTED AND GENERATED — never hand-edit. Regenerate with:
 *     & ".\scripts\windows\regen-ffi-header.ps1"
 * Drift gate (D-15): CI regenerates this header and fails on
 *     git diff --exit-code -- crates/ffi/include/rudis_ffi.h
 * so the committed header IS the ABI; any Rust-side signature change must
 * land together with its regenerated header in the same commit.
 *
 * Memory contract: every RudisBuffer returned by this library is freed ONLY
 * by rudis_free_buffer (same allocator). Envelopes are UTF-8 JSON:
 * {"Ok": ..} / {"Err": ".."}; transport faults ride RudisStatus. */

#ifndef RUDIS_FFI_H
#define RUDIS_FFI_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * Transport-fault status (D-06). Domain errors NEVER appear here — they stay
 * inside the JSON envelope as {"Err": "..."} exactly as today's Result<T, String>.
 *
 * The `-5..=-9` block (Phase 51, D-05/D-06/D-07) exists because the four
 * panel exports carry NO envelope: `rudis_preview_attach_panel` and friends
 * return a bare status, so a QueryInterface failure, a wrong-thread call or a
 * failed surface creation has nowhere else to be reported. They are genuine
 * TRANSPORT/precondition faults — environment and caller-contract problems,
 * not user-data validation — so widening this enum is the right home rather
 * than overloading `InvalidHandle`/`AllocationFailed` with unrelated meanings.
 *
 * ⚠ APPEND ONLY. A shipped C# host compares raw `i32`s off the wire
 * (`shell/Rudis.Shell/Interop/RudisStatus.cs`); renumbering an existing
 * variant would still compile on both sides and silently mis-diagnose every
 * fault. The per-variant compile-time asserts below make a reorder a BUILD
 * error, and `tests/layout_canary.rs` + `InteropTests.status_vocabulary_
 * mirrors_the_rust_enum` re-prove it at runtime in both languages.
 */
enum RudisStatus
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
  Ok = 0,
  NullPointer = -1,
  InvalidUtf8 = -2,
  AllocationFailed = -3,
  InvalidHandle = -4,
  /**
   * D-06: the COM pointer handed to `rudis_preview_attach_panel` is not an
   * `ISwapChainPanelNative` (`QueryInterface` returned `E_NOINTERFACE`, or a
   * null interface). `wgpu-hal` TRANSMUTES this pointer rather than QI-ing
   * it (`wgpu-hal-26.0.6/src/dx12/mod.rs:511`), so passing it through
   * unchecked would be undefined behaviour rather than an error return.
   * This is the check that turns a caller mistake into a diagnosis.
   */
  NotASwapChainPanel = -5,
  /**
   * D-07: a panel-affine call arrived from a thread other than the one that
   * attached. `ISwapChainPanelNative::SetSwapChain` returns
   * `RPC_E_WRONG_THREAD` off the panel's own UI thread; this is the named
   * failure, never a hang.
   */
  WrongThread = -6,
  /**
   * Surface/adapter/device creation or the first `configure` failed. The
   * full HRESULT / wgpu error text is written to stderr; this status says
   * WHICH stage failed.
   */
  SurfaceCreateFailed = -7,
  /**
   * A resize/detach/content-rect call arrived with no panel attached.
   */
  NotAttached = -8,
  /**
   * `rudis_preview_attach_panel` called while a panel is already attached.
   * Detach first; re-attaching over a live surface is not a supported
   * transition.
   */
  AlreadyAttached = -9,
  PanicCaught = -99,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum RudisStatus RudisStatus;
#else
typedef int32_t RudisStatus;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/**
 * Opaque per-instance context (D-07). NEVER a process-global: SC-4's tier
 * builds and tears down independent instances (the OnceLock-global
 * alternative was rejected in CONTEXT for making tests order-dependent).
 *
 * Every field is per-instance — two `RudisCtx`s share NO domain state, no
 * event stream, no directories (the D-07 independence property, asserted by
 * `ctx.rs`'s tests). The one deliberate exception class is process-wide id
 * COUNTERS inside `app-core` (`ID_SEQ` etc., RESEARCH E2): ids stay globally
 * unique across instances, which is a desirable property, not shared state.
 */
typedef struct RudisCtx RudisCtx;

/**
 * Owned byte buffer handed across the ABI. Contract (carried into the
 * cbindgen header via these doc comments):
 * - freed ONLY by `rudis_free_buffer` (same allocator; a .NET free is heap corruption)
 * - treat as opaque/read-only on the managed side; a tampered len/cap is UB on free
 * - `rudis_free_buffer` on an all-zero struct is a safe no-op
 */
typedef struct RudisBuffer {
  uint8_t *ptr;
  uintptr_t len;
  uintptr_t cap;
} RudisBuffer;

/**
 * The frame-content sub-rect inside the attached panel — contain-fit,
 * EXCLUDING the letterbox bars — in PHYSICAL pixels relative to the panel's
 * own origin.
 *
 * This is the capability the retired `emit_canvas_viewport` pushed as an
 * event; here it is PULLED on demand (`rudis_preview_content_rect`), which is
 * the hot-scalar-poll idiom this ABI already uses for
 * `rudis_get_playback_position`. It is computed in exactly ONE place —
 * `ShellPresentSink`'s composite paths, via `engine::contain_fit_viewport` —
 * so the pointer-normalization the C# Canvas does and the letterbox the
 * compositor draws can never drift apart.
 *
 * `x`/`y` are SIGNED: the rect is panel-relative and a future non-origin
 * content placement (or a rounding step) may legitimately produce a negative
 * offset. `width`/`height` are unsigned and are `0` before the first
 * composite.
 */
typedef struct RudisPreviewRect {
  int32_t x;
  int32_t y;
  uint32_t width;
  uint32_t height;
} RudisPreviewRect;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Create an independent engine instance and hand back its opaque handle.
 *
 * `config_json`/`config_len` may describe an optional UTF-8 JSON object
 * `{"data_dir": "...", "cache_dir": "...", "resource_dir": "...",
 * "self_advance": false, "credential_service": null}`; a null pointer or zero length means "no config",
 * and any absent directory is backed by a per-instance temp dir owned by
 * the returned ctx. Returns null on invalid UTF-8/JSON (T-47-05:
 * `str::from_utf8`, never `_unchecked`) or any construction failure — and
 * on a caught panic.
 *
 * `self_advance` (D-17, Phase 50, OPT-IN, default `false`): when `true`,
 * the ENGINE owns the playback clock — a per-instance tick thread advances
 * the playhead against wall time while playing, so a host that presses Play
 * sees `rudis_get_playback_position()` progress with ZERO per-frame clock
 * code, and end-of-media auto-pause pushes one `playback:changed` event.
 * A host that drives its own `advance` transport loop must leave this
 * `false` — both clocks running would advance the same playhead twice
 * (2x-speed playback). The retired Tauri shell was such a host; the shipping
 * C# shell is NOT — it passes `self_advance: true` (see
 * `BuildInitConfigJson` in `shell/Rudis.Shell/App.xaml.cs`).
 *
 * D-07 note (recorded per plan 47-02): D-07's sketch is zero-arg; the config
 * buffer is discretion-shaped (module layout / export naming are Claude's),
 * and the locked essence — opaque handle, paired shutdown, every export
 * takes ctx, no process-global — is fully preserved. A zero-arg init would
 * hardcode a directory convention Phase 50 could never override.
 */
struct RudisCtx *rudis_init(const uint8_t *config_json,
                            uintptr_t config_len);

/**
 * Tear down a ctx created by [`rudis_init`]. Null → `InvalidHandle`. The
 * managed side wraps this in a `SafeHandle` (Phase 50) so release runs at
 * most once; a dangling second call here is T-47-04's accepted residual.
 */
RudisStatus rudis_shutdown(struct RudisCtx *ctx);

/**
 * Free a [`RudisBuffer`] previously returned by this library. The ONLY legal
 * way to free one (same allocator — a managed-side free is heap corruption,
 * T-47-03). An all-zero/null-ptr struct is a safe no-op (RESEARCH B5).
 */
void rudis_free_buffer(struct RudisBuffer buf);

/**
 * Write the SC-5 freshness probe envelope `{"Ok": "<ABI_PROBE>"}` into a
 * fresh buffer. Null `out` → `NullPointer`. Deliberately ctx-free (see the
 * crate doc's D-07 exemptions): a host can prove symbol resolution and the
 * buffer round-trip before ever creating an instance.
 */
RudisStatus rudis_abi_probe(struct RudisBuffer *out);

/**
 * `get_snapshot` — the whole `rudis_core::Project`, `{"Ok": {..Project..}}`.
 */
RudisStatus rudis_get_snapshot(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `get_entities` — the named entities only, in request order (the LAT-02
 * bulk-mutation fallback).
 */
RudisStatus rudis_get_entities(struct RudisCtx *ctx,
                               const uint8_t *json,
                               uintptr_t len,
                               struct RudisBuffer *out);

/**
 * `get_waveform_peaks` — a PURE CACHE READ of one media item's peak envelope
 * (Phase 52, SHELL-09 / D-21).
 *
 * * HIT:  `{"Ok": {"block_us": 10000, "sample_rate": 48000,
 *           "peak_count": 372, "peaks_b64": "AAECAw.."}}`
 * * MISS: `{"Ok": null}` — for cache-miss, not-yet-computed, no-audio AND an
 *   unknown `media_id` alike.
 *
 * **This export NEVER triggers computation.** Peaks are produced once, by the
 * detached background job `app_core::import` starts at import time; there is
 * no code path from here to a decoder. That is what lets the Timeline call it
 * from its cold-path poll without ever risking the "synchronous call during
 * timeline draw" the roadmap's fifth criterion forbids.
 *
 * An `Err` is deliberately NOT one of the four miss cases: the peak cache's
 * own posture is best-effort, and an `Err` would force the caller to
 * distinguish "broken" from "not ready yet" on every 100 ms poll. `Err` is
 * reserved for a genuinely malformed request, which [`call_json`] already
 * produces for free.
 *
 * `media_id` is an OPAQUE DOMAIN ID (ASVS V5, T-52-21): `app-core` looks it up
 * in the real `Store` to obtain the path, and never joins the caller's string
 * onto any filesystem path.
 *
 * No new event tag went with this — `ring::EVENT_NAMES` stays at 6.
 */
RudisStatus rudis_get_waveform_peaks(struct RudisCtx *ctx,
                                     const uint8_t *json,
                                     uintptr_t len,
                                     struct RudisBuffer *out);

/**
 * `get_filmstrip_strip` — a PURE CACHE READ of one media item's packed
 * thumbnail strip (Phase 53.2, D-12/D-14/D-15).
 *
 * Mirrors [`rudis_get_waveform_peaks`] exactly — same args shape, same
 * envelope, same "never errors, never computes" promise, same opaque-id
 * treatment of `media_id` — with ONE addition: the payload's
 * `completed_tiles`/`total_tiles` pair distinguishes a PARTIAL strip
 * (progressive fill still in flight, D-14) from a COMPLETE one.
 *
 * * HIT: `{"Ok": {"tile_w": 96, "tile_h": 54, "tiles_per_row": 16,
 *   "total_tiles": 5, "completed_tiles": 5, "interval_us": 1000000,
 *   "sheet_w": 1536, "sheet_h": 54, "strip_b64": "AAECAw.."}}` — `sheet_w`
 *   and `sheet_h` describe the bytes ACTUALLY PRESENT (the completed rows),
 *   so a consumer can size its upload from the payload alone without
 *   assuming anything about a strip that is still being written.
 * * MISS: `{"Ok": null}` — for cache-miss, not-yet-computed, audio-only
 *   media, a still image, an offline file, a failed decode AND an unknown
 *   `media_id` alike (D-15). The caller cannot distinguish them and must not
 *   try.
 *
 * **This export NEVER triggers computation.** Strips are produced once, by
 * the detached background job `app_core::import` starts at import time; there
 * is no code path from here to a decoder. That is what lets the Timeline call
 * it from its cold-path poll without ever risking a synchronous decode during
 * paint.
 *
 * `media_id` is an OPAQUE DOMAIN ID (ASVS V5, T-53.2-13): `app-core` looks it
 * up in the real `Store` to obtain the path, and never joins the caller's
 * string onto any filesystem path.
 *
 * No new event tag went with this either — `ring::EVENT_NAMES` stays at 6.
 */
RudisStatus rudis_get_filmstrip_strip(struct RudisCtx *ctx,
                                      const uint8_t *json,
                                      uintptr_t len,
                                      struct RudisBuffer *out);

/**
 * `get_proxy_status` — a PURE POLL of one media item's playback-proxy state
 * (Phase 58, PROXY-02 / 58-CONTEXT D-12/D-29).
 *
 * Mirrors [`rudis_get_filmstrip_strip`] exactly — same args shape, same
 * envelope, same "never errors, never computes" promise, same opaque-id
 * treatment of `media_id`.
 *
 * * KNOWN id: `{"Ok": {"state": "running"}}`. The seven states are `queued`,
 *   `running`, `ready`, `not_needed`, `failed`, `cancelled` and `none`. A
 *   consumer that only wants "is there a proxy yet?" compares against `ready`
 *   and treats everything else as "not yet" — which is what the decode-source
 *   resolver already does on its own, so nothing about playback depends on
 *   this call being made at all.
 * * UNKNOWN id: `{"Ok": null}` — the only none-case. A known source with no
 *   proxy answers `{"Ok": {"state": "none"}}`, because "there is no proxy and
 *   none is coming" is information rather than an absence.
 *
 * **This export NEVER triggers generation.** Proxies are produced by the
 * detached background job `app_core::import` starts at import time, gated by
 * the heaviness predicate; there is no code path from here to an encoder.
 *
 * `media_id` is an OPAQUE DOMAIN ID (ASVS V5, T-58-05-01): `app-core` looks it
 * up in the real `Store` to obtain the path, and never joins the caller's
 * string onto any filesystem path.
 *
 * POLL-ONLY BY DECISION (58-CONTEXT D-29), not by omission: progress rides
 * Phase 50 D-06's existing 100 ms cold-path poll, so no seventh event tag went
 * with this either — `ring::EVENT_NAMES` stays at 6, the closed D-02 set shared
 * across this ABI by both shells. A push event may be added by a later phase
 * **when a shell region actually consumes it**; today nothing does, because
 * 58-CONTEXT D-24 freezes shell work while the cutover is owner-blocked.
 *
 * Phase 71 (TRUST-03): a `running` payload additionally carries
 * `"progress_permille"` (0..=999, monotonic per job; 999 is the ceiling while
 * running — `ready` is the only 'done'). Every other state serialises exactly
 * as before; a consumer that reads only `state` is unaffected.
 */
RudisStatus rudis_get_proxy_status(struct RudisCtx *ctx,
                                   const uint8_t *json,
                                   uintptr_t len,
                                   struct RudisBuffer *out);

/**
 * `get_render_cache_status` — a PURE POLL of the timeline render cache's
 * state (Phase 59, CACHE-01 / 59-CONTEXT D-30).
 *
 * Mirrors [`rudis_get_proxy_status`] — same envelope, same "never errors,
 * never computes" promise — with **no args at all**, and that difference is
 * the interesting part: a proxy is a property of one MEDIA ITEM, while the
 * render cache is a property of the PROGRAM. There is no id to pass, so this
 * export takes `(ctx, out)` like [`rudis_get_current_seq`] rather than
 * `(ctx, json, len, out)` — and, pleasantly, it has no opaque-id surface to
 * defend at all (T-58-05-01 has no twin here).
 *
 * Always `{"Ok": {..}}`, never `{"Ok": null}`:
 * `{"state": "idle", "rendering_segment": -1, "cached_segments": 12,
 * "heavy_segments": 3}`.
 *
 * * `state` is exactly one of `none`, `idle`, `rendering`. `none` means this
 *   host has no render-cache directory at all — nothing has ever been
 *   rendered for it; `idle` means it has one and nothing is rendering right
 *   now; `rendering` means a background segment render is in flight.
 * * `rendering_segment` is the segment index being rendered, or **`-1`** when
 *   nothing is. `-1` rather than a nullable field, so a C# DTO deserializes it
 *   into a plain `long`.
 * * `cached_segments` counts DISTINCT segment indices with a committed meta on
 *   disk — a census of what has been rendered, NOT a count of what would be
 *   served right now. Re-deriving each segment's identity would mean one
 *   key-material walk per entry on a 100 ms poll; the reader answers "would
 *   this serve?" per tick, fail-closed, and is the only honest place for it.
 * * `heavy_segments` is the detector's current candidate count — the work the
 *   background scheduler still has to do.
 *
 * **This export NEVER triggers a render.** Segments are produced by the
 * detached background job `app_core::render_cache_job` starts from the
 * transport funnel; there is no code path from here to an encoder, and
 * `render_cache_job_status_never_starts_a_render` polls it a hundred times
 * against a heavy section to prove it.
 *
 * POLL-ONLY BY DECISION (59-CONTEXT D-30), not by omission: retrieval rides
 * Phase 50 D-06's existing 100 ms cold-path poll, exactly as waveform peaks,
 * filmstrip strips and proxy status already do, so no seventh event tag went
 * with this either — `ring::EVENT_NAMES` stays at 6, the closed D-02 set
 * shared across this ABI by both shells. That is the FOURTH time this pattern
 * has been used. A push event may be added by a later phase **when a shell
 * region actually consumes it**; today nothing does, because 59-CONTEXT D-34
 * freezes shell work while the cutover is owner-blocked.
 *
 * Phase 71 (TRUST-03): `committed_total` is the SIXTH field, appended LAST — a
 * process-lifetime count of segments actually committed. Difference it against
 * a baseline to get this session's count; `cached_segments` /
 * `heavy_segments` are censuses and must not be shown as progress (D-63-04-02).
 */
RudisStatus rudis_get_render_cache_status(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `get_current_seq` — the store's mutation counter, the resync anchor a
 * poller pairs with [`rudis_get_snapshot`] after a `resync_required` poll.
 */
RudisStatus rudis_get_current_seq(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `debug_mark_interactive` — the LAT-01 cold-launch marker hook (a no-op
 * unless `RUDIS_LAT01_MARKER_PATH` is set). Returns `()` — the envelope is
 * `{"Ok": null}`, keeping the uniform shape across all 17 commands.
 */
RudisStatus rudis_debug_mark_interactive(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `debug_seed_project` — DEBUG-GATED whole-project seed for the Phase 54 C#
 * live-eval harness (54-CONTEXT D-12/D-13), and for NOTHING else. Envelope
 * `{"Ok": null}`.
 *
 * # Gate
 *
 * Refuses with a DOMAIN error unless `RUDIS_DEBUG_SEED_PROJECT` is set in the
 * process environment (a PRESENCE check — any value, including empty, opens
 * it). That is [`rudis_debug_mark_interactive`]'s pattern and Phase 50 D-10's
 * rule: off by default, and *asserted* off by a contract test rather than
 * merely documented off. The check runs on EVERY call and latches nothing, so
 * a successful seed does not leave the door open behind it. The shipping shell
 * never sets the variable and no production caller has a reason to.
 *
 * # Why this is not a production export (D-13's rejected alternative, recorded)
 *
 * A whole-project overwrite silently discards the open project AND its undo
 * stack, and has no defined undo semantics of its own. Shipping that with no
 * production caller would be a hazard, not a feature. A real
 * `open_project`/`new_project` pair was left to Phase 55, when the C# shell
 * took over the project lifecycle from the retiring Tauri shell. It is still
 * NOT exported across this ABI — `app_core::project::run_new_project` /
 * `run_open_project` exist but no C export reaches them yet.
 *
 * # Scope — each omission below is deliberate
 *
 * - Replaces the store via `rudis_core::Store::from_project`, which starts
 *   `seq` at 0 with empty undo/redo stacks — the SAME constructor the Rust
 *   live gate uses (`crates/agent-llm/tests/agent_eval_live.rs:572`), so the
 *   C# harness and the Rust harness seed identically.
 * - Resets `agent_session` to default. The harness builds a FRESH ctx per
 *   fixture, so this is defense-in-depth against misuse on a long-lived ctx,
 *   NOT something any caller depends on (54-02's resolution of 54-RESEARCH
 *   Open Question 1 — recorded here, at the definition site).
 * - Emits NOTHING into the event ring and calls no `observe_preview_patch`: a
 *   LIVE shell mirror calling this would desync until its next full resync.
 *   One more reason the gate exists. The harness never polls; it reads the
 *   result back through [`rudis_get_snapshot`].
 */
RudisStatus rudis_debug_seed_project(struct RudisCtx *ctx,
                                     const uint8_t *json,
                                     uintptr_t len,
                                     struct RudisBuffer *out);

/**
 * `undo` — `{"Ok": {..Patch..}}`, or `{"Ok": null}` with nothing to undo.
 * The inverse patch's `project:changed` push happens inside `undo_inner` via
 * `FfiAppCtx::emit_patch` — zero wiring here.
 *
 * Phase 51 (SHELL-04): the inverse patch ALSO reaches the committed-ink
 * mirror through [`RudisCtx::observe_preview_patch`] — an undone
 * `AddAnnotation` must un-ink the preview, which is exactly the case a
 * dispatch-only hook would miss. Nothing to undo (`None`) observes nothing.
 */
RudisStatus rudis_undo(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `redo` — [`rudis_undo`]'s exact mirror, preview hook included.
 */
RudisStatus rudis_redo(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `place_clip` — place a MediaBin item on the timeline; `{"Ok": {..Clip..}}`.
 */
RudisStatus rudis_place_clip(struct RudisCtx *ctx,
                             const uint8_t *json,
                             uintptr_t len,
                             struct RudisBuffer *out);

/**
 * `apply_option_card` — apply one pending option card by id (CANV-02);
 * `{"Ok": [{..Patch..}, ..]}`. The inner's `(Patch, base_seq, seq)` triples
 * are mapped to bare `Patch`es exactly as the Tauri wrapper does: the seq
 * pair is an EVENT-envelope concern (Phase 43 D-06), already emitted through
 * `FfiAppCtx::emit_patch` by the time this returns.
 *
 * Phase 51 (SHELL-04): an option card can mint annotations, so EVERY patch it
 * produced is observed by the committed-ink mirror, in order.
 */
RudisStatus rudis_apply_option_card(struct RudisCtx *ctx,
                                    const uint8_t *json,
                                    uintptr_t len,
                                    struct RudisBuffer *out);

/**
 * `transport` — apply one playback command; `{"Ok": {..Playback..}}`.
 *
 * The store half is `app_core::run_transport`; everything after it is this
 * host's CONSEQUENCE half, twinning the Tauri wrapper line-for-line
 * (`app_core::TransportOutcome`'s documented split): publish to the
 * lock-free playback mirror, bump `seek_seq` on a reposition (Seek/Step
 * only, 18.2-05), then push `playback:changed` into the ring — the ring push
 * IS this host's `app.emit(PLAYBACK_CHANGED_EVENT, ..)`, same payload type
 * (`Playback`). On a domain `Err` the host half never runs, exactly as
 * Tauri's `?` skips it.
 */
RudisStatus rudis_transport(struct RudisCtx *ctx,
                            const uint8_t *json,
                            uintptr_t len,
                            struct RudisBuffer *out);

/**
 * `agent_status` — `{"Ok": {"key_configured": bool, "source": "credential_manager"|
 * "environment"|"none", "providers": {"runway": {"key_configured": bool, "source": ..}}}}`
 * (Phase 69, D-69-14: `source`/`providers` are ADDITIVE). Only booleans and a
 * source enum — key material, its length or prefix never cross back over the
 * ABI (T-47-13 / T-12-11).
 *
 * `run_agent_status` returns a bare `AgentStatusView` (it cannot fail);
 * wrapped as `Ok::<_, String>` DELIBERATELY so the envelope shape stays
 * uniform across all 17 commands — the C# side parses one shape, not two.
 */
RudisStatus rudis_agent_status(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `set_api_key` — persist the user's own Anthropic key (BYO-key, AUTH-02);
 * `{"Ok": null}`, or `{"Err": ".."}` for a malformed key (never stored).
 */
RudisStatus rudis_set_api_key(struct RudisCtx *ctx,
                              const uint8_t *json,
                              uintptr_t len,
                              struct RudisBuffer *out);

/**
 * `clear_api_key` — remove the stored key; idempotent, `{"Ok": null}`.
 */
RudisStatus rudis_clear_api_key(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `set_provider_key` — persist a generation provider's key in the OS
 * credential store under its `PROVIDER_KEY_SLOTS` account, then INVALIDATE
 * the provider slots so the key takes effect on next use, no restart
 * (D-69-13). Provider ids resolve ONLY through
 * `app_core::SETTINGS_PROVIDER_ALLOW_LIST` (`{"runway"}`) +
 * `PROVIDER_KEY_SLOTS` (T-31-04 / T-69-08): anything else is
 * `{"Err": "provider '..' is not managed by Settings (allowed: runway)"}` —
 * the error names the provider, never the key. A malformed key is refused
 * with the validator's rule text and never stored. `{"Ok": null}` on success.
 */
RudisStatus rudis_set_provider_key(struct RudisCtx *ctx,
                                   const uint8_t *json,
                                   uintptr_t len,
                                   struct RudisBuffer *out);

/**
 * `clear_provider_key` — remove the provider slot's stored key; idempotent,
 * `{"Ok": null}`. Same allow-list as [`rudis_set_provider_key`], and the same
 * invalidation: the next generation submit re-resolves (and, with no
 * environment key either, refuses with the Settings-pointing text).
 */
RudisStatus rudis_clear_provider_key(struct RudisCtx *ctx,
                                     const uint8_t *json,
                                     uintptr_t len,
                                     struct RudisBuffer *out);

/**
 * `rudis_poll_events` — drain the ctx's event ring, non-blocking (the cold
 * path's ONLY delivery mechanism). Envelope: `{"Ok": {"resync_required":
 * bool, "next_seq": u64, "events": [{"seq": .., "event": "..",
 * "payload": ..}, ..]}}` — `ring::PollOutcome` serialized whole.
 *
 * D-09: host-polls-native. There is NO callback from Rust into C#, so the
 * reverse-P/Invoke + DispatcherQueue re-marshal hazard class does not exist
 * at this boundary at all. D-12: non-blocking ONLY — polling never parks the
 * caller, and no blocking variant exists this phase; Phase 50, the first
 * real consumer, picks its own cadence. On `resync_required` the caller
 * refetches via [`rudis_get_snapshot`] + [`rudis_get_current_seq`] (S8's
 * recovery protocol, unchanged from the shell side).
 */
RudisStatus rudis_poll_events(struct RudisCtx *ctx, uint64_t local_seq, struct RudisBuffer *out);

/**
 * `rudis_get_playback_position` — the HOT path: a lock-free scalar read of
 * the playback mirror's `position_us` atomic, called at poll rate by the
 * host's transport UI. `research/v7-ARCHITECTURE.md:101` is explicit that
 * this path needs NO event mechanism at all — no envelope, no allocation,
 * no JSON, just the `i64`.
 *
 * `i64::MIN` is the out-of-band sentinel for BOTH a null handle and a caught
 * panic (a real position is clamped to `[0, duration]` and can never be
 * `i64::MIN`). Deliberately not over-built: no playing/duration scalar twins
 * until a consumer exists (Phase 50 adds what it measures a need for).
 */
int64_t rudis_get_playback_position(struct RudisCtx *ctx);

/**
 * `rudis_get_playback_resolution_level` — PLAY-05's observable (Phase 57, plan
 * 57-08). A lock-free scalar read of the engine's active dynamic-playback-
 * resolution level: **`0` = full, `1` = half, `2` = quarter**.
 *
 * The pattern is [`rudis_get_playback_position`]'s, deliberately copied rather
 * than reinvented — same shape, same `ffi_guard!`, same out-of-band sentinel
 * discipline, one poll-rate scalar with no envelope, no allocation and no JSON.
 * CONTEXT D-01 asks for "engine state through the existing envelope"; this is
 * that envelope.
 *
 * `i32::MIN` is the sentinel for BOTH a null handle and a caught panic. A real
 * level is one of `{0, 1, 2}` (and an unknown byte from a future writer
 * saturates to `0`, "nothing is degraded"), so it can never collide.
 *
 * # Direction, and why it is a different struct
 *
 * The position getter reads a mirror the SHELL writes. This reads one the
 * ENGINE writes — the preview producer's multi-layer arm, which is the only
 * writer. That is why it hangs off `RudisCtx::diag` rather than becoming a
 * sixth atomic on `PlaybackMirror`: one struct, one writer.
 *
 * # No consumer in v8, on purpose
 *
 * D-01 says v8 adds nothing to either shell, so nothing calls this yet.
 * Rendering a "1/2" indicator in the Transport region is post-cutover work.
 * The export existing, crossing the ABI, and being pinned end-to-end against a
 * real degradation IS the deliverable — the same standard `rudis_poll_events`
 * was held to before Phase 50 had a consumer for it.
 *
 * # What it can never report
 *
 * Anything about export. Playback resolution is transient engine state that
 * lives as a `!Send` stack local on one producer thread (D-11/D-12); the
 * degraded level is structurally unreachable from the export path, which runs
 * in crates that cannot even name the type.
 */
int32_t rudis_get_playback_resolution_level(struct RudisCtx *ctx);

/**
 * `import_media` — probe + poster + register each path;
 * `{"Ok": [{..MediaBinItem..}, ..]}`.
 *
 * Per-file `project:changed` pushes happen automatically via
 * `FfiAppCtx::emit_patch` inside the moved per-call loop — zero extra wiring
 * here (RESEARCH A4's monomorphization dividend: the SAME `run_*` body
 * emits into the ring on this host and into the WebView on the shell host).
 */
RudisStatus rudis_import_media(struct RudisCtx *ctx,
                               const uint8_t *json,
                               uintptr_t len,
                               struct RudisBuffer *out);

/**
 * `import_media_folder` — recursive folder import as ONE undo turn, at the
 * SAME production clamps the Tauri wrapper uses (500 files / depth 12, the
 * T-47-09-class DoS bounds); `{"Ok": [{..MediaBinItem..}, ..]}`.
 */
RudisStatus rudis_import_media_folder(struct RudisCtx *ctx,
                                      const uint8_t *json,
                                      uintptr_t len,
                                      struct RudisBuffer *out);

/**
 * `export_timeline` — render + encode the timeline to `out_path`;
 * `{"Ok": "<written path>"}`. Blocks the calling thread for the whole
 * encode (the C# host runs it off its UI thread — Phase 50's concern).
 * `export:progress` records land in the ring via 47-04's
 * `export_progress_sink` — zero extra wiring here.
 */
RudisStatus rudis_export_timeline(struct RudisCtx *ctx,
                                  const uint8_t *json,
                                  uintptr_t len,
                                  struct RudisBuffer *out);

/**
 * `agent_send_message` — run one real Chat turn;
 * `{"Ok": {..AgentTurnOutcome..}}`. Mirrors the Tauri body exactly:
 * key-gated `AnthropicTransport::connect` (with NO key configured this
 * errors CLEANLY with `agent_llm::NO_API_KEY_MESSAGE` — the refusal that
 * points at Settings, Phase 69 D-69-09 — pointing at the surface that
 * can fix it; 47-06's offline contract test asserts the const), then the backend-resolved
 * `agent_library` dir (never caller-supplied, T-14-15), then the SAME
 * `run_agent_turn` every host drives.
 *
 * Honest scope (D-03): a turn through this host is TEXT-ONLY — 47-04's
 * fail-closed `GenSubmission`/`SpendPolicy` impls halt any generation tool
 * call pre-dispatch with the D-03 question, and `AgentVision` degrades to
 * text-only. No paid call can fire through this export (T-47-10).
 */
RudisStatus rudis_agent_send_message(struct RudisCtx *ctx,
                                     const uint8_t *json,
                                     uintptr_t len,
                                     struct RudisBuffer *out);

/**
 * `new_project` — create a brand-new, empty, NAMED project and switch to it.
 *
 * Args `{"name": ".."}`; envelope `{"Ok": "<prose>"}` (agent-facing prose,
 * unchanged since Phase 26 — this export is a new HOST for it, not a new
 * contract). The name goes through `sanitize_project_name` BEFORE any path
 * join (T-26-01), so a reserved Windows device name, a path separator or an
 * over-long name comes back as a DOMAIN error with transport status `Ok`:
 * text a dialog can print, never a transport fault.
 *
 * Side effects the caller must expect, because they are the point: the
 * OUTGOING project is autosaved first (hook a), the new one is written to
 * `<app-data>/projects/<name>.rud` immediately (hook b), undo/redo is reset,
 * and one `project:changed` record lands in the ring carrying the structural
 * `ProjectSwitched` patch — the shell full-resyncs on it. No seventh event
 * type exists for any of this.
 */
RudisStatus rudis_new_project(struct RudisCtx *ctx,
                              const uint8_t *json,
                              uintptr_t len,
                              struct RudisBuffer *out);

/**
 * `open_project` — switch to a previously created project **by name**.
 *
 * Args `{"name": ".."}`; envelope `{"Ok": "<prose>"}`, or
 * `{"Err": "no known project named \"..\" — call get_projects to see what
 * exists"}` with status `Ok`.
 *
 * ⚠ The name is a LOOKUP KEY, never a path component: `app-core` resolves it
 * exclusively through the real on-disk registry (`scan_known_projects`) and
 * never joins caller text into a path. That is T-26-03, and it is deliberately
 * stricter than accepting any path the caller names, because this `name` may
 * have come from the LLM. A user who picked a file from a dialog wants
 * [`rudis_open_project_at_path`] instead — a separate export over a separate
 * function taking a separate type, which is what keeps this route narrow.
 */
RudisStatus rudis_open_project(struct RudisCtx *ctx,
                               const uint8_t *json,
                               uintptr_t len,
                               struct RudisBuffer *out);

/**
 * `open_project_at_path` — open a `.rud` the USER picked, from ANYWHERE on
 * disk.
 *
 * Args `{"path": ".."}`; envelope `{"Ok": {"name": "..", "path": ".."}}`, and
 * the `name` is read from the LOADED document rather than the file stem (a
 * user who renamed the file in Explorer has not renamed the project inside
 * it).
 *
 * **The path is USER-chosen, and the validation is a property of the
 * argument's TYPE.** Rudis has accepted user-picked absolute paths across this
 * same ABI since Phase 47 (`rudis_import_media`); what T-26-03 defends against
 * is an LLM-chosen path, and the distinction is *who chose it*. Every refusal
 * — a directory, a non-`.rud`, a file that is not there, a file above the size
 * cap — therefore arrives from the deserialize as `{"Err": "invalid
 * arguments: .."}` with transport status `Ok`, which is exactly what a file
 * dialog needs in order to say something useful.
 *
 * Same side effects as [`rudis_open_project`]: the outgoing project is
 * autosaved, in-flight proxy and render-cache work is cancelled and both
 * registries are forgotten, undo/redo resets, and ONE `project:changed`
 * record carries the structural switch.
 */
RudisStatus rudis_open_project_at_path(struct RudisCtx *ctx,
                                       const uint8_t *json,
                                       uintptr_t len,
                                       struct RudisBuffer *out);

/**
 * `save_project` — persist the LIVE store to the active project's file.
 *
 * No args; envelope `{"Ok": {"path": "..", "seq": N}}`. `seq` is the store's
 * mutation counter captured in the SAME guard as the bytes (LAT-02's rule), so
 * a host can tell whether the store has moved since — there is deliberately no
 * separate "is dirty" export, and after any open or new the seq is 0 with the
 * project already on disk, so "nothing unsaved" is true by construction rather
 * than by a flag someone has to remember to clear.
 *
 * # It MINTS rather than refusing when nothing is active
 *
 * With no active project this does **not** fail: it creates
 * `<app-data>/projects/Untitled.rud` (then `Untitled 2`, `Untitled 3`, … —
 * there is no `Untitled 1`), renames the live document to match, points the
 * active-project pointer at it and writes. A modal asking a beginner to name a
 * file **before** their work is safe is precisely the moment the work gets
 * lost, and this is what lets a close path have no dialog and no branch at
 * all: close always saves. The minting save — and ONLY it — also pushes one
 * `project:changed` record, because it renamed the live document; an ordinary
 * Ctrl+S emits nothing, since a full resync on every save would make the
 * commonest operation in the app the most expensive one.
 *
 * Deliberately host-only: there is no agent tool behind this, because an LLM
 * that could silently overwrite the user's file on disk is not a capability
 * this ABI wants.
 */
RudisStatus rudis_save_project(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `save_project_as` — persist to a user-chosen path **and re-point the active
 * document there**.
 *
 * Args `{"path": ".."}`; envelope
 * `{"Ok": {"name": "..", "path": "..", "seq": N}}`. The universal NLE
 * convention: after Save As you are editing the copy, and every later save —
 * including the one on close — lands at the new path. A Save As that wrote a
 * copy and left you editing the original is the shape that loses the next
 * hour of work. The live project takes the target's file stem as its name, so
 * the TitleBar and the next registry scan agree with the filename the user
 * just chose, and ONE `project:changed` record carries that rename.
 *
 * ⚠ The returned `path` is the CANONICALISED target (on Windows that is the
 * extended-length `\\?\C:\..` form, because the parent folder went through
 * `std::fs::canonicalize`). A host that wants to display it should shorten it
 * for presentation; a host that wants to compare it must canonicalise the
 * other side rather than compare the strings.
 *
 * A folder that does not exist, a name that is not a legal project name, or a
 * missing `.rud` extension is refused by the argument's TYPE, before any
 * write, as `{"Err": "invalid arguments: .."}` with transport status `Ok`.
 */
RudisStatus rudis_save_project_as(struct RudisCtx *ctx,
                                  const uint8_t *json,
                                  uintptr_t len,
                                  struct RudisBuffer *out);

/**
 * `get_projects` — the HUMAN's project list.
 *
 * No args; envelope
 * `{"Ok": [{"isActive": bool, "modifiedUnixMs": N, "name": "..", "path": ".."}, ..]}`
 * (keys serialize alphabetically — `serde_json`'s map is a `BTreeMap` here, so
 * a host must not assume insertion order).
 *
 * ⚠ **This export is named `get_projects` and it calls
 * `run_get_projects_DETAILED`, on purpose.** `app-core`'s `run_get_projects`
 * is the AGENT tool's view and withholds the filesystem path deliberately
 * (T-26-08, minimal disclosure) — and a person choosing between two projects
 * both called "Untitled" needs exactly the path and the mtime that posture
 * withholds. One export, two contracts, neither widened: nothing here can
 * widen what the LLM sees, and nothing the LLM sees constrains what a file
 * list can show its owner.
 */
RudisStatus rudis_get_projects(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `get_missing_media` — which of the ACTIVE project's media files are not on
 * disk right now, by **id**.
 *
 * No args; envelope `{"Ok": ["<media id>", ..]}`, and `{"Ok": []}` when no
 * project is active (an absent project has no missing media, and that is not
 * an error). Ids rather than paths: T-26-08's posture, and the host already
 * holds each item's path in its own mirror while the MediaBin keys its tiles
 * by id.
 *
 * A **poll**, deliberately not a seventh event type — `ring::EVENT_NAMES` has
 * stayed at 6 through five consecutive phases that each added exactly one
 * export, and retrieval rides the existing 100 ms cold-path poll exactly as
 * waveform peaks, filmstrip strips, proxy status and render-cache status
 * already do. Cheap by construction: it snapshots the store and DROPS the
 * guard before it stats anything, so the interop worker that also services
 * `rudis_poll_events` is never blocked across a cold directory walk.
 */
RudisStatus rudis_get_missing_media(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * `dispatch_command` — apply one undoable store mutation;
 * `{"Ok": {..Patch..}}`. The `project:changed` push (flattened envelope,
 * seq pair included) happens inside `dispatch_command_inner` via
 * `FfiAppCtx::emit_patch` — zero wiring here.
 *
 * # The preview hook (Phase 51, SHELL-04)
 *
 * This is the ANNOTATION path: an `AddAnnotation` / `RemoveAnnotation` /
 * `ClearCanvas` command arrives here and must reach the C# shell's
 * committed-ink mirror, or the user draws a mark and no ink appears over the
 * preview. [`RudisCtx::observe_preview_patch`] is that hook — it bumps the
 * mid-play edit-flush signal and runs `panel::overlay::apply_patch_to_overlay`
 * against the store's authoritative FrameLinked-only list. It is called AFTER
 * a successful dispatch (so a rejected command never inks) and BEFORE the
 * envelope is written, on the caller's thread, holding no domain lock.
 */
RudisStatus rudis_dispatch_command(struct RudisCtx *ctx,
                                   const uint8_t *json,
                                   uintptr_t len,
                                   struct RudisBuffer *out);

/**
 * Attach a WinUI 3 `SwapChainPanel` and start presenting into it.
 *
 * `panel` is a COM pointer to the panel. An `IInspectable*` — what
 * `WinRT.MarshalInspectable<object>.FromManaged` yields — is fine: this
 * function `QueryInterface`s it for `ISwapChainPanelNative` itself, so a
 * wrong pointer is `NotASwapChainPanel`, never undefined behaviour. The
 * CALLER keeps ownership of its own reference and must release it; this
 * function takes its own.
 *
 * `width_px`/`height_px` are the panel's size in PHYSICAL pixels and `scale`
 * its composition scale — WinUI-native signals Rust cannot query, so the
 * caller sends both and Rust keeps one source of truth. All three are clamped
 * (`1..=16384`, finite positive scale) before any GPU call: a 0-px or absurd
 * swapchain is a driver allocation failure or a hard OOM, not a graceful
 * error.
 *
 * A panel must be detached before another is attached; re-attaching over a
 * live surface answers `AlreadyAttached`.
 *
 * ⚠ MUST be called on the UI thread that owns `panel`. `wgpu`'s
 * `Surface::configure` calls `ISwapChainPanelNative::SetSwapChain`, which
 * returns `RPC_E_WRONG_THREAD` off that thread. Call it from the panel's
 * `Loaded` handler, synchronously — never from a background interop worker
 * (a serialized interop queue is the WRONG thread by construction).
 *
 * Returns `Ok`, or: `InvalidHandle` (null ctx) · `NullPointer` (null panel) ·
 * `AlreadyAttached` · `NotASwapChainPanel` · `SurfaceCreateFailed` ·
 * `PanicCaught`.
 */
RudisStatus rudis_preview_attach_panel(struct RudisCtx *ctx,
                                       void *panel,
                                       uint32_t width_px,
                                       uint32_t height_px,
                                       float scale);

/**
 * Publish a new panel size — and, best-effort, consume it immediately.
 *
 * Callable from ANY thread. The publish half is lock-free: it stores three
 * scalars and sets a dirty flag the present paths' `reconfigure_if_dirty`
 * consumes. Rust owns the resize; the caller only forwards the notification.
 * The second and later `Surface::configure` calls take `ResizeBuffers`, which
 * has no COM apartment rule.
 *
 * The consume half ([`resize_reconfigure_and_present`]) is a try_lock PROBE
 * that NEVER blocks the caller: during playback the present thread owns the
 * GPU set and its own inline reconfigure already covers the resize, so a held
 * lock is a skip — the pre-fix behaviour, verbatim. It exists because a PAUSED
 * transport presents nothing, so before it the dirty flag had NO consumer
 * until the next scrub/edit/play and the swapchain kept its old dimensions
 * (debug session `preview-swapchain-not-reconfigured-on-window-resize`,
 * measured live 2026-07-31).
 *
 * `width_px`/`height_px` are PHYSICAL pixels; `scale` is the panel's
 * composition scale. All three are clamped exactly as at attach.
 *
 * Returns `Ok`, or: `InvalidHandle` (null ctx) · `NotAttached` ·
 * `PanicCaught`.
 */
RudisStatus rudis_preview_resize(struct RudisCtx *ctx,
                                 uint32_t width_px,
                                 uint32_t height_px,
                                 float scale);

/**
 * Read the frame-content sub-rect inside the panel — contain-fit, EXCLUDING
 * the letterbox bars — in physical pixels, panel-relative.
 *
 * Four relaxed atomic loads; callable from any thread, zero allocation, no
 * lock — the same hot-path shape as `rudis_get_playback_position`. Written by
 * the present path on every composite, from the SAME
 * `engine::contain_fit_viewport` call the compositor letterboxes with, so the
 * ink overlay's geometry and the drawn picture cannot drift apart. Do not
 * re-derive contain-fit math in the host.
 *
 * This is the capability the retired viewport push provided, re-expressed as
 * a PULL: under `SwapChainPanel` the ink overlay is an ordinary sibling in the
 * same visual tree, so it can simply ask.
 *
 * `width`/`height` are `0` until a composite has actually happened — the
 * caller's cue that there is nothing to align to yet.
 *
 * Returns `Ok`, or: `InvalidHandle` (null ctx) · `NullPointer` (null `out`) ·
 * `NotAttached` · `PanicCaught`. `*out` is written only on `Ok`.
 */
RudisStatus rudis_preview_content_rect(struct RudisCtx *ctx, struct RudisPreviewRect *out);

/**
 * Detach the panel: stop presenting into it and release the GPU set.
 *
 * ⚠ MUST be called on the SAME thread that attached. Call it from the panel's
 * `Unloaded` handler, which WinUI runs on the same UI thread as `Loaded`.
 *
 * The present thread is NOT joined: `preview::present_loop_with_gpu_budget`
 * has no stop signal and is frozen, so it keeps ticking against a
 * surface-less sink, and every `PresentSink` method already degrades to a
 * documented no-op there. That is the same degradation the Tauri host
 * performs under its mock runtime, not a leak of live work — and it is what
 * makes a later re-attach cheap.
 *
 * Idempotent-safe: a second detach answers `NotAttached` rather than faulting.
 *
 * Returns `Ok`, or: `InvalidHandle` (null ctx) · `NotAttached` ·
 * `WrongThread` · `PanicCaught`.
 */
RudisStatus rudis_preview_detach_panel(struct RudisCtx *ctx);

/**
 * Poll the preview device's health and the present counter.
 *
 * `(ctx, out)` — the shape `rudis_get_render_cache_status` and
 * `rudis_get_current_seq` already use. Always `{"Ok": {..}}`, never
 * `{"Ok": null}` and never `{"Err": ..}`: an unattached ctx honestly answers
 * all-zeroes, because "no panel has ever been attached" is a state, not a
 * fault.
 *
 * **NEVER COMPUTES AND NEVER TAKES THE GPU LOCK** (threat T-63-06). Five
 * relaxed atomic loads. The present thread owns the GPU lock for the whole of
 * a composite, and a cold-cadence UI-thread poll that queued behind one would
 * stall the shell on exactly the tick a TDR is being handled — so this reads
 * the atomics [`super::sink::ShellPresentSink`] publishes and nothing else.
 *
 * POLL-ONLY BY DECISION (63-CONTEXT D-10), not by omission: `ring::EVENT_NAMES`
 * stays at 6.
 */
RudisStatus rudis_preview_device_status(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * **TEST/PROOF ONLY — force a REAL device removal on the LIVE preview device.**
 *
 * Drives [`engine::inject_forced_device_loss`], which is the body factored
 * verbatim out of `RecoveryPlan::simulate_device_lost` (quick `260829-n96`), so
 * this export reuses the 48-03 probe-measured discipline rather than
 * re-implementing `RemoveDevice` here: every encoder is created BEFORE the
 * trigger (a post-remove `create_command_encoder` is a native, uncatchable
 * `STATUS_ACCESS_VIOLATION`) and only submit + poll — both probe-measured safe
 * — touch the device afterwards. There is exactly ONE injection body in this
 * repository and it is the engine's.
 *
 * ⚠ **FAIL-CLOSED DEBUG GATE (threat T-63-04).** Refuses with a DOMAIN error
 * unless `RUDIS_DEBUG_DEVICE_LOSS=1` was in the environment when
 * [`debug_device_loss_enabled`] first latched. A shipped, paid build must not
 * carry an ungated lever that kills the user's GPU device on request; the C#
 * caller is additionally `#if DEBUG`-only, so a Release shell does not even
 * contain the call.
 *
 * Callable from any thread. Takes the GPU lock for the duration — deliberately,
 * so the present thread cannot be mid-composite into the device being removed.
 */
RudisStatus rudis_preview_simulate_device_lost(struct RudisCtx *ctx, struct RudisBuffer *out);

/**
 * **Run the coordinated six-step recovery against the LIVE `SwapChainPanel`**
 * (Phase 63, plan 63-02 — TRUST-01, CONTEXT D-02/D-03).
 *
 * This is the production surface half of what plan 63-01 proved headlessly. The
 * shell's Preview region polls [`rudis_preview_device_status`], sees
 * `lost && !recovering`, pauses the transport, and calls this — **on the UI
 * thread that owns the panel**, handing back a FRESH COM pointer to the same
 * `SwapChainPanel`.
 *
 * # Why the whole sequence runs here, on the caller's thread
 *
 * Two constraints meet, and exactly one placement satisfies both:
 *
 * * **Step 4 must run on the panel's UI thread.** Recreating the surface means
 *   a FIRST `Surface::configure`, which reaches
 *   `ISwapChainPanelNative::SetSwapChain` and returns `RPC_E_WRONG_THREAD`
 *   anywhere else (see [`super::surface::attach_gpu`] step 5).
 * * **Recovery must NOT run on wgpu's device-lost callback thread**, because
 *   step 3 destroys the very device that callback belongs to (plan 63-01's
 *   request/run split).
 *
 * The panel's UI thread is neither the callback thread nor the present thread,
 * so it is a legal place to tear the device down AND the only legal place to
 * stand a new surface up. That is why this is a synchronous export rather than
 * something the present loop drives.
 *
 * # Honest scope note: this drives `RecoveryPlan`, it does not fork it
 *
 * The sequence IS `engine::RecoveryPlan::recover` — the same coordinator, the
 * same six-step order, the same single-entry fail-closed guard, the same
 * step-4 same-adapter LUID re-assert. What differs from plan 63-01's
 * `engine::PreviewRecovery` is only WHO owns the six hooks. `PreviewRecovery`
 * owns the compositor itself and is the right owner at the ENGINE tier; here
 * the compositor lives in [`super::state::PreviewGpu`], where the present
 * thread and the ring producer reach it through `Arc` clones, and
 * `PreviewRecovery`'s scoped-borrow ownership model cannot express that
 * without changing the frozen `preview::PresentSink::compositor` port.
 * `engine::RecoveryHooks`' own doc anticipates exactly this split — *"the
 * headless test wires them to a real session/compositor pair; **the shell
 * wires them to its managed state**"* — as does
 * `RecoveryPlan::recovering_flag`'s (*"the shell keeps its own managed
 * twin"*). So this is the second HOST of one coordinator, not a second
 * recovery discipline.
 *
 * # The `Arc` discipline plan 63-01 warned about
 *
 * While ANY handle to the removed D3D12 device lives, DXGI hides the hardware
 * adapter from fresh enumeration in this process and step 4 silently recreates
 * on WARP — where the LUID re-assert refuses it, turning a recoverable TDR into
 * a hard failure. `PreviewGpu::compositor` is an `Arc` and the ring producer
 * holds a clone, so **step 1 waits for that clone to be released** (the
 * producer exits on `ring.request_stop()`, which `present_loop` issues the
 * moment the transport is not playing — which is why the shell pauses first)
 * and refuses to proceed if it is not. Failing THERE is safe: nothing has been
 * torn down yet.
 *
 * Returns `Ok`, or: `InvalidHandle` (null ctx) · `NullPointer` (null panel) ·
 * `WrongThread` / `NotAttached` (affinity) · `NotASwapChainPanel` ·
 * `SurfaceCreateFailed` (any step failed — the sequence is then latched
 * closed) · `PanicCaught`.
 */
RudisStatus rudis_preview_recover_device(struct RudisCtx *ctx,
                                         void *panel,
                                         uint32_t width_px,
                                         uint32_t height_px,
                                         float scale);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* RUDIS_FFI_H */
