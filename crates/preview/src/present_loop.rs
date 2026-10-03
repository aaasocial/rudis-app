//! The present loop (plan 46-10) — the thread that decides WHAT pixels to show
//! WHEN, moved out of `src-tauri/src/native_surface.rs`.
//!
//! This is the function the whole of Phase 46 exists to relocate. Everything it
//! calls arrived in this crate first, one wave at a time, precisely so that the
//! move itself would be mechanical: the pure predicates at 46-04, the edit
//! signal at 46-05, the resolvers at 46-06, the present/overlay seam at 46-07,
//! the multi-layer path at 46-08 and the ring + its producer at 46-09. By the
//! time this wave ran, the body's only remaining shell touches were three
//! managed-state reads, and all three had a port answer.
//!
//! # What changed in the move — and nothing else did
//!
//! | Before (`native_surface.rs`) | After |
//! |---|---|
//! | `fn present_loop(app: AppHandle)` | `pub fn present_loop(ctx: PresentContext, edit_seq: Arc<PreviewEditSeq>)` |
//! | `let ctx = present_context(&app);` | the caller builds it; `setup` spawns the thread with it |
//! | `app.try_state::<Mutex<NativePreview>>()…np.compositor.clone()` | [`crate::PresentSink::compositor`] |
//! | `app.try_state::<Arc<PlaybackMirror>>()` + a `None => (false, 0, false, 0)` arm | [`crate::PreviewHost::playback_mirror`], whose fallback mirror IS that tuple |
//! | `app.try_state::<Arc<PreviewEditSeq>>()…unwrap_or(0)` | `edit_seq.seq`, the handle the caller shares with the listener |
//! | `drain_preview_edit_pending(&app)` | `edit_seq.drain_pending()` (that shell function is DELETED) |
//! | `preview::X` | `crate::X` |
//!
//! The pacing policy, the flush handshake, the audio-master clock, the
//! paused/scrub branch, the producer-liveness gate and the Issue-B edit-scoping
//! decision are byte-identical to what the shell held. The `eprintln!` prefix
//! `"native_surface: audio start failed"` is deliberately left alone too: it is
//! a log string, nothing asserts on it, and renaming it would add churn to the
//! one file whose byte-identity is this wave's evidence (46-09's precedent with
//! `"preview_ring: …"`).
//!
//! # Threading
//!
//! One instance per app, spawned once from `setup`, running for the app's
//! lifetime. It owns its [`crate::PresentContext`] (both ports are
//! `Send + Sync`, locked at wave 46-02) and hands the producer thread an OWNED
//! [`crate::PresentContext::host_arc`] plus the `Arc<Compositor>` this module's
//! new sink method returns — so presenter and producer share ONE adapter pair,
//! which is what `app.clone()` amounted to before.
//!
//! `AudioOutput` holds a `!Send` `cpal::Stream` and therefore lives entirely on
//! this thread, as it always has.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

// `Compositor` left this use line at 48-10: the producer's compositor handle
// is now fetched per respawn via `ctx.sink().compositor()` (see the respawn
// branch), so no local names the type any more.
use engine::AudioOutput;

use crate::{
    edit_affects_single_layer, edit_flush_needed, edit_touches_playhead_audio,
    paused_represent_needed, presentation_resync_needed, preview_target_us, resolve_active,
    resolve_audio_mix, resolve_program_audio, reuse_audio, FrameCache, PreviewEditPending,
    PreviewEditSeq,
};

/// The dedicated present thread (Wave 3 Task 3; audio-master pacing added in
/// Wave 4). Follows the frontend's playback clock via the atomic mirror (no
/// `Mutex<Store>` on the hot path) and, while playing, presents streamed frames
/// onto the surface.
///
/// **Pacing.** When the active range HAS audio, the present thread starts an
/// [`AudioOutput`] and paces each video frame off the AUDIO hardware clock
/// (`elapsed_us()` — samples the device has consumed), holding each frame until
/// audio reaches its boundary so video follows audio with no accumulating drift
/// (research §6 / Assumption A1, "audio-master clock"). For video-only ranges
/// (muted / `audio_detached` / no audio stream) it keeps the Wave-3 wall-clock
/// pacing UNCHANGED. Either way it paces to the media's own fps (`frame_step_us`).
///
/// Restarts the single persistent decode session (and its audio) on seek/clip
/// boundary/monitor switch; frees both on pause (the exact paused frame is
/// presented by `present_paused`). Runs for the app's lifetime.
///
/// **Multi-layer (18.3-04, Stage 2).** The playing path has ONE shape for ALL
/// content — single-layer, multi-layer composites, and gaps: the background
/// producer ([`crate::spawn_producer`]) sources every frame (multi ranges
/// composite on the producer thread through the SAME byte-unchanged
/// `composite_layers_to_rgba` the export uses, via a cloned `Arc<Compositor>`)
/// and this loop only pops/presents/HOLDs. Merge/unmerge boundaries are
/// internal to the producer — no presenter branch, no warm-up state machine.
/// The paused/scrub still (`present_still` → `present_multilayer`) keeps its
/// own exact-frame composite path, untouched.
///
/// `AudioOutput` holds a `!Send` `cpal::Stream`; it lives entirely on THIS
/// thread (never sent elsewhere), which is exactly why it is a local here.
pub fn present_loop(ctx: crate::PresentContext, edit_seq: Arc<PreviewEditSeq>) {
    present_loop_with_gpu_budget(ctx, edit_seq, None)
}

/// Phase 48 (plan 48-09): [`present_loop`] with the live VRAM budget handle
/// that arms the producer's per-media GPU decode gate. Only the real app's
/// `native_surface::setup` — the site that spawned the `VramBudgetWatch` —
/// hands `Some`; `None` (and every non-hwdecode build) keeps the CPU-only
/// producer, byte-identical to the pre-48-09 behavior, which is what every
/// existing test route (including 48-01's pacing harness through
/// [`present_loop`]) exercises.
#[cfg_attr(
    not(all(windows, feature = "hwdecode")),
    allow(unused_variables)
)]
pub fn present_loop_with_gpu_budget(
    ctx: crate::PresentContext,
    edit_seq: Arc<PreviewEditSeq>,
    gpu_budget: Option<Arc<std::sync::atomic::AtomicU64>>,
) {
    use std::time::Instant;

    // Phase 46 (plans 46-06, 46-07 and 46-10): the two shell adapters this
    // thread hands to everything that has moved into `crates/preview`. Built
    // ONCE, by the CALLER now (`setup`), and passed as `ctx.host()` wherever
    // `&app` used to go for `resolve_active` / `resolve_audio_mix` /
    // `resolve_program_audio` / `edit_touches_playhead_audio`, and as `&ctx` for
    // `present_frame` / `overlay_signature` / `repaint_overlay`. 46-06 built the
    // host alone; 46-07 promoted it to the full `PresentContext` when the present
    // path itself moved; 46-08 took the multi-layer path with it, so
    // `resolve_multilayer` / `start_multi_audio` / `present_still` are `ctx`-fed
    // now too. 46-10 removed the last `app` touches — the mirror read is
    // `ctx.host().playback_mirror()`, the edit-seq reads go through the
    // `edit_seq` handle the caller shares with the listener, and the producer's
    // compositor clone is `ctx.sink().compositor()` — which is what let this
    // function leave the shell in the same wave.
    let mut audio: Option<AudioOutput> = None; // live preview-audio for this range
    // 18.3-02 (PERF-01): the read-ahead ring seam. `ring` outlives producers
    // across flushes; the producer respawn is gated on the old handle's
    // `is_finished()` (NEVER joined on this hot path — the PoolWarmer precedent).
    // The producer owns ALL single-layer + gap decode; the presenter only pops.
    let ring: Arc<crate::RingCtl> = crate::RingCtl::new();
    let mut producer: Option<std::thread::JoinHandle<()>> = None;
    // 18.3-04 (Stage 2): the producer composites multi-layer frames ITSELF via a
    // clone of the SAME wgpu compositor (design §2 "GPU access": Device/Queue
    // are internally synchronized; offscreen render+readback never touches the
    // presenter's surface lock). `None` only on a mock runtime without
    // `setup()`, where nothing presents anyway.
    //
    // Phase 48 (plan 48-10, GPU-04): the clone is taken PER PRODUCER RESPAWN
    // at the spawn site below, no longer once here at thread start — after a
    // device-lost recovery swaps `NativePreview`'s compositor for one on the
    // rebuilt device, a respawned producer must bind the LIVE compositor, not
    // a captured handle to the dead device (exactly GPU-04's dangling-reference
    // failure class). The fetch runs only on the rare respawn branch (a
    // pause→play boundary), never on the ~60Hz path.
    // Wall-clock pacing origin for video-only single-layer ranges: (timeline_us,
    // Instant) captured at every flush / producer (re)spawn.
    let mut play_origin: (i64, Instant) = (0, Instant::now());
    // Tracks the active monitor (Source vs Program) so a monitor SWITCH re-roots
    // the producer through the same flush handshake as a seek.
    let mut last_is_source = false;
    // Paused/scrub presentation state. `last_paused` coalesces rapid scrubs: the
    // paused branch only decodes when (position, monitor) CHANGES, and always
    // reads the LATEST mirror value — so a fast drag skips stale intermediate
    // positions and only ever renders the newest one. The cache makes scrub-back
    // instant. Both live on THIS thread so paused decoding never touches the UI
    // thread (that was the freeze / "not responding" bug).
    let mut last_paused: (i64, bool) = (i64::MIN, false);
    let mut frame_cache = FrameCache::new(24);
    // Signature of the overlay last composited while paused. When it changes with
    // no playhead movement (a mark drawn/removed/faded/resurfaced, or the live
    // trail moving), the paused branch re-composites the stored frame so the ink
    // actually appears (CANV-01). Reset on every full still-present.
    let mut last_overlay_sig: u64 = 0;
    // 18.3-01 (Stage 0 / Part A): the timeline window `[start, end)` of the
    // CURRENT live audio mix, whichever path started it (multi entry, mid-range
    // restart, or the single restart block) — the input to the `reuse_audio`
    // decision. `None` = no live window known (a fresh play always rebuilds).
    // Replaces the former mix-end sentinel so BOTH boundaries can reuse a
    // content-identical mix instead of dropping + rebuilding it.
    let mut audio_span: Option<(i64, i64)> = None;
    // 18.2-05: latch of the last-seen explicit reposition signal
    // (`PlaybackMirror::seek_seq`) — see the pulse consumption below.
    let mut last_seek_seq: u64 = 0;
    // 18.3-03 (design §4): latch of the last-seen mid-play edit signal
    // (`PreviewEditSeq`, bumped by the `PROJECT_CHANGED_EVENT` listener). Read +
    // latched EVERY tick (paused included, like `last_seek_seq`) so a paused edit
    // — handled by the paused branch's own re-present — never triggers a spurious
    // flush on resume.
    let mut last_edit_seq: u64 = 0;
    // 18.3-03 Issue-A diagnostic: env-gated (OFF by default) once/sec pacing log
    // to distinguish H1 (frame-stamp timebase) from H2 (audio-clock overcount) on
    // a live run. Read once — zero cost when unset.
    let pace_log = std::env::var("RUDIS_PREVIEW_PACE_LOG").is_ok();
    let mut last_pace_log = Instant::now();

    loop {
        // Phase 46 (plan 46-10): read through the port. The old
        // `app.try_state::<Arc<PlaybackMirror>>()` had a `None => (false, 0,
        // false, 0)` arm for the unmanaged (mock-runtime) case; the adapter's
        // own always-available fallback mirror reads as exactly that tuple, so
        // the arm is not lost, it is answered one level down. Still re-read
        // EVERY tick, as before.
        let mirror = ctx.host().playback_mirror();
        let (playing, position, is_source, seek_seq) = (
            mirror.playing.load(Ordering::Relaxed),
            mirror.position_us.load(Ordering::Relaxed),
            mirror.is_source.load(Ordering::Relaxed),
            mirror.seek_seq.load(Ordering::Relaxed),
        );
        // 18.2-05: consume the explicit reposition signal as a ONE-TICK pulse —
        // read + latch every iteration so a seek made while paused or while in
        // the single-layer path is not misread as new later.
        let seek_signaled = seek_seq != last_seek_seq;
        last_seek_seq = seek_seq;

        // 18.3-03 (design §4): consume the mid-play edit signal as a ONE-TICK
        // pulse. Read the SHARED counter, decide with the PRE-latch value, then
        // latch EVERY tick (paused/multi included) so a paused-or-multi edit
        // never fires a spurious flush later. The decision is only ACTED on in
        // the single-layer ring branch below (playing); the pure
        // `edit_flush_needed` guarantees it is false while paused. All bumps
        // since the last tick collapse to ONE flush (T-18.3-03-01: bounded work
        // amplification, no edit-storm blowup).
        //
        // Phase 46 (plan 46-10): this was a managed-state lookup with an
        // `.unwrap_or(0)` for the unmanaged case. The caller now hands this
        // thread the SAME `Arc` it gave the listener, so the lookup — and the
        // silent-no-op-on-a-state-miss failure mode it carried — is gone; an
        // unregistered signal simply reads its own 0, which is what the
        // `unwrap_or` produced.
        let cur_edit_seq = edit_seq.seq.load(Ordering::Relaxed);
        let edit_changed = cur_edit_seq != last_edit_seq;
        let edit_while_playing = edit_flush_needed(cur_edit_seq, last_edit_seq, playing);
        last_edit_seq = cur_edit_seq;
        // 18.3-03 Issue-B: drain the accumulated invalidation on ANY seq change
        // — paused/multi ticks discard it (the paused branch re-presents; the
        // multi branch re-resolves per tick), so it never lingers to pollute a
        // later single-layer flush decision.
        let edit_pending = if edit_changed {
            edit_seq.drain_pending()
        } else {
            PreviewEditPending::default()
        };

        if !playing {
            // Stop audio on pause; the exact paused frame is presented by
            // transport()'s handback, not this loop. Dropping `audio` stops the
            // cpal stream and joins its producer thread.
            audio = None;
            // 18.3-02: reap the ring producer within one bounded op (its session
            // drops -> engine Drop kills+waits+joins every ffmpeg child, SC-R3).
            // The paused/scrub present_still + FrameCache path below is untouched.
            ring.request_stop();
            audio_span = None; // a fresh play always rebuilds the mix
            // Paused-frame / scrub presentation, on THIS background thread (never
            // the UI thread). Decode when the position or monitor changed —
            // reading the latest mirror each tick coalesces a fast drag to its
            // newest position — AND on an explicit seek pulse or a
            // preview-relevant edit pulse (PREVIEW-STALE fix): an agent/UI edit
            // changes the composite at an UNCHANGED playhead, and the frontend's
            // deliberate no-op same-position seek expects a re-present handback.
            // `present_still` re-resolves + re-composites CURRENT state; its
            // FrameCache keys on immutable media identity, so unchanged frames
            // still hit. Polls at ~125Hz so scrubbing feels immediate.
            if paused_represent_needed(position, is_source, last_paused, seek_signaled, edit_changed)
            {
                last_paused = (position, is_source);
                // Phase 49 (plan 49-01, OQ4): a paused re-present IS a landing
                // — scrub-release lands paused — so report the stamp with the
                // CURRENT position on SUCCESS only (a failed decode/composite
                // must not read as "landed at target" to SEEK-01's harness).
                if crate::present_still(&ctx, is_source, position, &mut frame_cache) {
                    ctx.sink().note_presented_stamp(position);
                }
                // The full still-present just composited the current overlay;
                // remember its signature so we only re-composite below on a real
                // overlay change (not immediately again).
                last_overlay_sig = crate::overlay_signature(&ctx);
            } else {
                // Same paused position: re-composite the stored frame ONLY when
                // the visible overlay changed (mark drawn/removed/faded/
                // resurfaced) or the live gesture trail moved. Without this, ink
                // drawn on a paused frame never appears — the present thread
                // otherwise composites the overlay only while playing (CANV-01).
                let sig = crate::overlay_signature(&ctx);
                if sig != last_overlay_sig {
                    last_overlay_sig = sig;
                    crate::repaint_overlay(&ctx);
                }
            }
            std::thread::sleep(Duration::from_millis(8));
            continue;
        }
        // We are PLAYING below. Reset the paused marker so the next pause always
        // re-presents the exact stop frame.
        last_paused = (i64::MIN, false);

        // 18.3-02: the flush handshake is the ONLY reposition mechanism (18.2-05
        // preserved) — `seek_seq` (Seek/Step) OR a monitor switch. NO wall-clock
        // heuristic anywhere (design §7 invariant 7). A genuine reposition flushes
        // the ring (producer rebuilds at `position` under a new gen), re-roots the
        // wall-clock origin, and drops the live audio so it rebuilds at the new
        // playhead (plan 01: a real Seek/Step/monitor switch always rebuilds — the
        // audio hardware clock cannot jump). This runs BEFORE the multi check so
        // both branches see a clean post-flush state.
        let monitor_switched = is_source != last_is_source;
        last_is_source = is_source;
        let repositioned = seek_signaled || monitor_switched;
        if repositioned {
            ring.flush(position);
            play_origin = (position, Instant::now());
            audio = None;
            audio_span = None;
        }

        // ---- 18.3-04 (Stage 2): ONE ring pop path for ALL playing content —
        // single-layer, multi-layer composites, and gaps. The 18.2-05 multi
        // branch (presenter-side pool build, the warm-up HOLD arm, the
        // entry/seek decision, the duplicated AUDIO-MASTER/VIDEO-ONLY pacing
        // blocks, and index-based audio pacing) is RETIRED: its jobs are
        // structural now — no seek heuristic exists to misfire (seek_seq→flush
        // is the only reposition), the underrun answer is HOLD globally, and
        // timestamps pace everything. --------------------------------------
        // Production left the presenter: the background producer owns ALL
        // single-layer + gap decode; the presenter only pops. Ensure the producer
        // is alive — respawn is gated on the OLD handle's `is_finished()` (NEVER
        // joined on this hot path; the PoolWarmer precedent) so a rapid pause→play
        // never stalls here and never leaves two producers racing for long.
        let producer_dead = producer.as_ref().map(|h| h.is_finished()).unwrap_or(true);
        if producer_dead {
            if ring.stop.load(Ordering::Relaxed) {
                // Fresh play after a pause-stop: clear the stop flag, then re-root
                // the producer at the mirror position under a new gen.
                ring.stop.store(false, Ordering::Relaxed);
                ring.flush(position);
                play_origin = (position, Instant::now());
            }
            if let Some(comp) = ctx.sink().compositor() {
                // 46-09: the producer thread takes the port by OWNED `Arc` (it
                // outlives this frame), and it is THIS context's host — one
                // adapter shared by the present thread and its producer, which
                // is what `app.clone()` amounted to before.
                //
                // 48-09: when the live VRAM budget handle is armed (the real
                // app), the producer's per-media GPU decode gate goes with it;
                // `None` spawns the byte-identical CPU-only producer.
                #[cfg(all(windows, feature = "hwdecode"))]
                {
                    producer = Some(match gpu_budget.clone() {
                        // 57-06: hand the producer the sink's texture-present
                        // capability at spawn time. `true` lets the multi arm
                        // keep its composited frame on the GPU all the way to
                        // the swapchain; `false` makes it read the SAME target
                        // back on the producer thread — one composite path
                        // either way, and the readback never lands on this
                        // (latency-critical) thread.
                        Some(budget) => crate::spawn_producer_gpu(
                            ctx.host_arc(),
                            ring.clone(),
                            comp,
                            budget,
                            ctx.sink().supports_composited_present(),
                        ),
                        None => crate::spawn_producer(ctx.host_arc(), ring.clone(), comp),
                    });
                }
                #[cfg(not(all(windows, feature = "hwdecode")))]
                {
                    producer = Some(crate::spawn_producer(ctx.host_arc(), ring.clone(), comp));
                }
            }
        }

        // 18.3-03 (design §4 + live Issue-B): a mid-play timeline edit flushes the
        // ring at the CURRENT position — the producer rebuilds the stream under a
        // new gen — so the edit is visible within one flush (not ~ring-depth
        // late). BUT flush ONLY when the edit changes the frame ON SCREEN: a
        // cross-track / non-visible edit leaves the buffered frames valid — the
        // producer already reflects the change on future frames — so the preview
        // keeps presenting with NO drain-and-refill freeze. When it DOES affect
        // the visible frame we also drop the live audio + span so the reuse gate
        // rebuilds the mix (prompt on-clip volume/trim). 18.3-04: inside a
        // multi-layer range EVERY composited layer is on screen, so the visible
        // set is the resolved stack's clip ids (a lower-layer edit must flush the
        // buffered composites); single ranges keep the exact Issue-B gate.
        if edit_while_playing {
            // VIDEO ring flush: only when the edit changes the frame ON SCREEN
            // (Issue-B — a cross-track / non-visible edit must NOT drain-and-refill
            // the ring). The producer reflects a non-flushing edit on future frames.
            let multi_stack_ids: Option<Vec<String>> = if is_source {
                None
            } else {
                crate::resolve_multilayer(ctx.host(), position)
                    .map(|s| s.layers.iter().map(|l| l.clip_id.clone()).collect())
            };
            let video_flush = match &multi_stack_ids {
                // Multi range: any edited clip that is part of the composited
                // stack changes the on-screen frame (structural always flushes).
                Some(stack_ids) => {
                    edit_pending.structural
                        || edit_pending
                            .clip_ids
                            .iter()
                            .any(|id| stack_ids.contains(id))
                }
                None => {
                    let active_clip =
                        resolve_active(ctx.host(), is_source, position).and_then(|r| r.clip_id);
                    edit_affects_single_layer(
                        &edit_pending.clip_ids,
                        active_clip.as_deref(),
                        is_source,
                        edit_pending.structural,
                    )
                }
            };
            // AUDIO rebuild (regression #3): force a mix rebuild when the edit
            // changes the audible mix at the playhead — visible-clip volume/trim,
            // a detached audio-track clip, etc. — even without a reposition, so
            // the sound applies within a beat instead of only after a pause→play.
            // Decoupled from the video flush so a non-overlapping cross-track edit
            // still touches neither (Issue-B preserved).
            let audio_rebuild = edit_touches_playhead_audio(
                ctx.host(),
                &edit_pending.clip_ids,
                position,
                is_source,
                edit_pending.structural,
            );
            if video_flush {
                ring.flush(position);
            }
            if video_flush || audio_rebuild {
                // Re-root the wall origin (used only for the momentary target
                // until the mix rebuilds) and invalidate reuse so the gate below
                // rebuilds the mix at this playhead with the edited audio.
                play_origin = (position, Instant::now());
                audio = None;
                audio_span = None;
            }
        }

        // AUDIO (presenter-owned, !Send — design §4b): pace video to the audio
        // hardware clock when a live mix covers the target, else the wall clock. A
        // genuine reposition already dropped `audio` + re-rooted `play_origin`
        // above, so on a seek tick the target is the wall clock == position and
        // the mix rebuilds here.
        let cur_gen = ring.gen.load(Ordering::SeqCst);
        // 18.3-03 Issue-A: video is AUDIO-MASTERED (target = mix start + audio
        // hardware clock) whenever a live mix covers the range, else wall-clock.
        // Extracted to the pure `preview_target_us` so the "locked to the audio
        // clock, never wall, while audio plays" invariant is regression-pinned.
        let target_us = preview_target_us(
            audio_span.map(|(s, _)| s),
            audio.as_ref().map(|a| a.elapsed_us()),
            play_origin.0,
            play_origin.1.elapsed().as_micros() as i64,
        );
        // (Re)start the mix only when Part A's gate says the live one no longer
        // covers what is being PRESENTED (span end / never started). reuse_audio's
        // 3rd arg is the PRESENTATION clock (design §4b): span coverage keys off
        // the popped frame's time, and the mix starts AT `target_us`.
        if !reuse_audio(audio_span, audio.is_some(), target_us, false) {
            // Phase 49.1 (SEEK-04): position-driven span anchoring. Each
            // `AudioOutput::start_mix` resets the audio hardware clock to 0
            // and re-anchors `target_us` to the new span's start, so the
            // mix-open cost (~0.27-0.39s per boundary on the owner's real
            // media — measured, artifacts/49.1-01A-RULER-FIX.txt) elapses in
            // wall time while the pacing clock stands still, and the clock
            // carries the loss forward: error COMPOUNDS across boundaries
            // (~0.3s -> ~0.6s -> ~1.0s, the owner's exact symptom). The fix:
            // when the clock has fallen behind the wall-projected transport
            // schedule (`play_origin` — re-rooted only on genuine
            // repositions/edits, never at an ordinary boundary) beyond the
            // pure, unit-tested threshold, anchor the NEW span at the
            // schedule position instead — skipping the deficit's worth of
            // content (SEEK-04's "drop/skip to re-sync"; `pop_for_target`'s
            // catch-up drops the stale video frames within the ring runway).
            // Presentation never rewinds (the predicate refuses a clock
            // ahead of wall), Source mode keeps its own semantics, and
            // within-span pacing is untouched — this runs only at span
            // rebuilds, once per boundary by construction.
            let wall_pos_us = play_origin.0 + play_origin.1.elapsed().as_micros() as i64;
            let anchor_us = if !is_source && presentation_resync_needed(target_us, wall_pos_us) {
                eprintln!(
                    "preview: presentation drift resync — pacing clock {}us behind the \
                     transport schedule; anchoring new span at {}us (was {}us)",
                    wall_pos_us - target_us,
                    wall_pos_us,
                    target_us
                );
                wall_pos_us
            } else {
                target_us
            };
            // 18.3-04 (design §4b — audio unchanged): inside a multi-layer range
            // the rebuild goes through `start_multi_audio` (the SAME
            // all-contributor sum-mix + the top clip's span as the mix WINDOW —
            // exactly what the retired multi branch started), preserving today's
            // mix semantics with `reuse_audio` carrying it across boundaries.
            if !is_source && crate::resolve_multilayer(ctx.host(), anchor_us).is_some() {
                let (a, end) = crate::start_multi_audio(ctx.host(), anchor_us);
                audio_span = (a.is_some() && end != i64::MAX).then_some((anchor_us, end));
                audio = a;
            } else if is_source {
                match resolve_active(ctx.host(), is_source, target_us) {
                    Some(r) => {
                        let tl_start = target_us;
                        let tl_end = target_us + (r.audio_end_us - r.source_us).max(0);
                        let mix = resolve_audio_mix(ctx.host(), is_source, tl_start, tl_end, &r);
                        audio = if !mix.is_empty() && tl_end > tl_start {
                            match AudioOutput::start_mix(mix, tl_start, tl_end) {
                                Ok(a) => Some(a),
                                Err(e) => {
                                    eprintln!("native_surface: audio start failed: {e}");
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        audio_span =
                            (tl_end > tl_start && audio.is_some()).then_some((tl_start, tl_end));
                    }
                    None => {
                        // Gap / nothing loaded: wall-clock pacing, no mix.
                        audio = None;
                        audio_span = None;
                    }
                }
            } else {
                // quick-k0q: Program mode resolves through the ONE seam, so the
                // streaming loop and `start_multi_audio` cannot drift apart.
                match resolve_program_audio(ctx.host(), anchor_us) {
                    Some((mix, tl_start, tl_end)) => {
                        audio = match AudioOutput::start_mix(mix, tl_start, tl_end) {
                            Ok(a) => Some(a),
                            Err(e) => {
                                eprintln!("native_surface: audio start failed: {e}");
                                None
                            }
                        };
                        audio_span = audio.is_some().then_some((tl_start, tl_end));
                    }
                    None => {
                        // Silent gap / nothing loaded: wall-clock pacing, no mix.
                        audio = None;
                        audio_span = None;
                    }
                }
            }
        }

        // Pop the frame due at the target and present it; HOLD the last surface
        // frame on underrun — NEVER black inside media (real gaps arrive as black
        // ENTRIES from the producer, not as a presenter special case). Catch-up in
        // the pop drops stale frames; audio-clock gating (elapsed_us==0 until real
        // samples flow → target sits at span start → frame 0 holds until audio
        // truly begins) preserves Phase-9 startup sync for free.
        let presented_ts = match ring.pop_for_target(cur_gen, target_us) {
            Some(entry) => {
                let ts = entry.timeline_us;
                match entry.payload {
                    // Phase 49 (plan 49-01, OQ4): after each SUCCESSFUL
                    // ring-entry present, report the popped entry's
                    // timeline_us so a harness can tell "landed at target"
                    // from "a present happened". Failed presents report
                    // nothing (both routes below).
                    crate::RingPayload::Cpu(frame) => {
                        if crate::present_frame(&ctx, frame) {
                            ctx.sink().note_presented_stamp(ts);
                        }
                    }
                    // 48-08: a GPU-RESIDENT entry routes to the GPU twin of the
                    // present seam — same reconfigure/viewport-hint shape as
                    // `present_frame`, no CPU collapse anywhere (GPU-06). The
                    // ink path stays CPU-side this phase (a GpuFrame is always
                    // pristine decoded output). Nothing produces these entries
                    // until 48-09 flips the producer.
                    #[cfg(all(windows, feature = "hwdecode"))]
                    crate::RingPayload::Gpu(frame) => {
                        let geometry_changed = ctx.sink().reconfigure_if_dirty().is_some();
                        match ctx.sink().present_gpu(&frame) {
                            Ok(dims_changed) => {
                                ctx.sink().note_presented_stamp(ts);
                                if geometry_changed || dims_changed {
                                    let (cw, ch) = ctx.sink().configured_size();
                                    ctx.host()
                                        .emit_canvas_viewport(cw, ch, frame.width, frame.height);
                                }
                            }
                            Err(e) => eprintln!("preview: present gpu frame failed: {e}"),
                        }
                    }
                    // 57-06 (PLAY-02/D-07): an already-COMPOSITED entry. The
                    // presenter's whole job here is a sample-and-blit through
                    // the sink's own compositor+surface — no readback, no
                    // re-upload, and no knowledge of how many layers (or which
                    // decode families) went into the texture. Pacing, the A/V
                    // clock and the flush handshake are untouched: the entry
                    // carries `timeline_us`/`gen` like every other, and this
                    // arm sits inside the SAME pop.
                    crate::RingPayload::Composited(entry) => {
                        let geometry_changed = ctx.sink().reconfigure_if_dirty().is_some();
                        match ctx.sink().present_composited(&entry.target, entry.w, entry.h) {
                            Ok(dims_changed) => {
                                ctx.sink().note_presented_stamp(ts);
                                if geometry_changed || dims_changed {
                                    let (cw, ch) = ctx.sink().configured_size();
                                    ctx.host().emit_canvas_viewport(cw, ch, entry.w, entry.h);
                                }
                            }
                            Err(e) => {
                                eprintln!("preview: present composited frame failed: {e}")
                            }
                        }
                    }
                }
                Some(ts)
            }
            None => None, // UNDERRUN: HOLD the last presented frame.
        };
        // 18.3-03 Issue-A diagnostic (env-gated, OFF by default → zero behavior
        // change): once/sec log the pacing clocks so a live run distinguishes H1
        // (frame-stamp timebase) from H2 (audio-clock overcount). Compare
        // `audio_elapsed_us` against `wall_us` (real time since play origin): if
        // audio elapsed RACES AHEAD of wall while the presented stamp tracks the
        // target → H2 (the cpal clock counts underrun silence, so video paced by
        // it runs ahead of the audible content). If audio≈wall but the presented
        // stamp races ahead of wall → H1 (a wrong frame_step). Set
        // `RUDIS_PREVIEW_PACE_LOG=1` to enable.
        if pace_log && last_pace_log.elapsed() >= Duration::from_secs(1) {
            last_pace_log = Instant::now();
            let wall_us = play_origin.1.elapsed().as_micros() as i64;
            let audio_elapsed_us = audio.as_ref().map(|a| a.elapsed_us()).unwrap_or(-1);
            eprintln!(
                "[pace] wall_us={wall_us} audio_elapsed_us={audio_elapsed_us} \
                 target_us={target_us} presented_ts={presented_ts:?} \
                 span_start={:?} lead_over_wall_us={}",
                audio_span.map(|(s, _)| s),
                audio_elapsed_us - wall_us,
            );
        }
        // Poll cadence only — the frame TIMESTAMPS (the pop policy) gate fps, not
        // this sleep. Short so pause/seek stay responsive.
        std::thread::sleep(Duration::from_millis(4));
        continue;
    }
}
