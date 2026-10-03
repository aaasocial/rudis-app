//! D-17 (Phase 50): the OPT-IN engine playback clock.
//!
//! When [`crate::InitConfig::self_advance`] is `true`, a per-instance tick
//! thread drives the SAME `TransportCmd::Advance` apply path the hosts drive
//! (`rudis_core::Store::transport` → `TransportCmd::apply` →
//! `Playback::advance_with_duration` — the clamp/loop/pause math is NEVER
//! re-derived here), so a C# shell that presses Play sees
//! `rudis_get_playback_position()` progress against wall time with ZERO
//! managed per-frame clock code.
//!
//! **Default OFF is load-bearing (the 2× trap):** the Tauri frontend drives
//! its own `Advance` loop (`frontend/src/main.ts`). An unconditional engine
//! clock would advance the same `Playback` twice — playback at double speed,
//! a D-15 violation reading as a vague pacing bug. Tauri passes no config →
//! `InitConfig::default()` → no thread spawns → byte-unchanged behaviour,
//! asserted by `default_config_never_self_advances` below with EXACT position
//! equality (no tolerance).
//!
//! # Contract (from 50-01-PLAN Task 2)
//!
//! - Tick ~10ms. Gate on `mirror.playing` (Relaxed): while paused, sleep and
//!   RESET the wall anchor — paused wall time is never added; on a
//!   false→true observation, anchor = now.
//! - Apply through the core path under the store lock; publish to the mirror
//!   (`mirror.update`) — the hot scalar path `rudis_get_playback_position`
//!   reads (`position_us` rides atomics, per `v7-ARCHITECTURE.md:101`).
//! - NEVER bump `seek_seq` (Advance is not a reposition, 18.2-05).
//! - NO per-tick ring push. `EVENT_PLAYBACK_CHANGED` is pushed EXACTLY ONCE
//!   when the tick's OWN apply flips `playing` true→false (end-of-media
//!   auto-pause) — cold-path hosts learn the stop without polling position.
//!   Loop wraps are position-only and push nothing.
//! - A domain `Err` from the apply (e.g. preview unloaded mid-play) pauses
//!   the tick loop's advancing (reset anchor, keep polling the gate) — never
//!   a panic, never a log spin.
//! - Lifecycle: stop flag + `JoinHandle`, joined in [`crate::RudisCtx`]'s
//!   drop path (reached by `rudis_shutdown`'s `Box::from_raw` drop) BEFORE
//!   any owned field drops. The thread holds ONLY compiler-proven Send+Sync
//!   state (`Arc<Mutex<Store>>`, `Arc<PlaybackMirror>`, `Arc<EventRing>`) —
//!   hand-written (unsafe) `Send`/`Sync` impls and raw ctx pointers are
//!   forbidden (T-50-01).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Tick cadence (Claude's discretion, 5-20ms per CONTEXT): 10ms keeps the
/// playhead smooth at any preview rate while costing one short store lock
/// per tick — far below the contention any host caller can observe.
const TICK_INTERVAL: Duration = Duration::from_millis(10);

/// Owning handle for one ctx's tick thread: a stop flag the loop polls every
/// tick, plus the `JoinHandle`. Dropping the handle stops AND joins — the
/// `RudisCtx` drop path takes it first, so the thread is provably gone
/// before the store/mirror/ring it shares are torn down (T-50-02).
pub(crate) struct SelfAdvanceHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for SelfAdvanceHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Bounded by the tick cadence (~10ms): the loop re-checks the
            // stop flag every iteration and never blocks unboundedly. A
            // panicked thread (impossible by construction — the loop has no
            // panic path — but never trust that at a join site) is absorbed:
            // teardown must not unwind (FFI-02 discipline).
            let _ = thread.join();
        }
    }
}

/// Spawn the per-instance engine clock. Called by
/// [`crate::RudisCtx::new_in_process`] ONLY when `InitConfig::self_advance`
/// is `true` — every existing caller passes `false` (the derive default) and
/// spawns nothing.
///
/// The thread captures ONLY compiler-proven `Send + Sync + 'static` state:
/// `Arc<Mutex<Store>>`, `Arc<PlaybackMirror>`, `Arc<EventRing>`, and the
/// stop flag. No hand-written (unsafe) `Send`/`Sync` impls, no raw ctx
/// pointer (T-50-01) — if this signature ever needs one, the design is wrong.
pub(crate) fn spawn(
    store: Arc<app_core::SharedStore>,
    mirror: Arc<preview::PlaybackMirror>,
    ring: Arc<crate::ring::EventRing>,
) -> std::io::Result<SelfAdvanceHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("rudis-self-advance".to_string())
        .spawn(move || tick_loop(&store, &mirror, &ring, &thread_stop))?;
    Ok(SelfAdvanceHandle {
        stop,
        thread: Some(thread),
    })
}

/// The engine clock body. See the module doc for the full contract; the
/// summary: gate on `mirror.playing`, advance the store by ELAPSED WALL TIME
/// through `Store::transport(Advance)` (the same apply `rudis_transport`
/// routes through — never re-derived math), publish to the mirror, and push
/// `playback:changed` exactly once on the tick's own true→false auto-pause.
fn tick_loop(
    store: &app_core::SharedStore,
    mirror: &preview::PlaybackMirror,
    ring: &crate::ring::EventRing,
    stop: &AtomicBool,
) {
    // Wall anchor of the previous tick while playing; `None` whenever the
    // clock is not accumulating (paused, fresh play, or a failed apply) so
    // paused wall time is NEVER added to the playhead.
    let mut anchor: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        if !mirror.playing.load(Ordering::Relaxed) {
            anchor = None;
            std::thread::sleep(TICK_INTERVAL);
            continue;
        }

        let now = Instant::now();
        let Some(prev) = anchor.replace(now) else {
            // false→true observation: fresh anchor; dt starts NEXT tick, so
            // no pre-Play wall time can leak into the playhead.
            std::thread::sleep(TICK_INTERVAL);
            continue;
        };
        let dt_us = now.duration_since(prev).as_micros().min(i64::MAX as u128) as i64;
        if dt_us <= 0 {
            std::thread::sleep(TICK_INTERVAL);
            continue;
        }

        // Apply through the SAME core path as `rudis_transport`'s store half
        // (`app_core::run_transport`'s exact two-line body, under one lock):
        // `Store::transport` → `TransportCmd::apply` →
        // `Playback::advance_with_duration` — the ONLY legal clamp/loop/pause
        // math. `was_playing` is read under the SAME lock so a host Pause
        // racing this tick can never be mistaken for an end-of-media
        // auto-pause (the ring push below is for the tick's OWN flip only).
        let auto_paused = match store.lock() {
            Ok(mut guard) => {
                let was_playing = guard.active_playback().playing;
                match guard.transport(rudis_core::TransportCmd::Advance { dt_us }) {
                    Ok(playback) => {
                        let is_source = guard.preview_mode() == rudis_core::PreviewMode::Source;
                        // Publish the hot scalars while still holding the
                        // store lock, so any host command applied AFTER this
                        // tick also mirror-publishes after it. NEVER bump
                        // `seek_seq` — Advance is not a reposition (18.2-05).
                        mirror.update(&playback, is_source);
                        let flipped = was_playing && !playback.playing;
                        flipped.then_some(playback)
                    }
                    // Domain err (e.g. preview unloaded mid-play): pause the
                    // clock's accumulation — reset the anchor, keep polling
                    // the gate. Never panic, never spin-log.
                    Err(_) => {
                        anchor = None;
                        None
                    }
                }
            }
            // A poisoned store means a caller panicked mid-apply; the guard
            // macro already mapped that caller's fault. The clock just stops
            // accumulating — same posture as the domain-err arm.
            Err(_) => {
                anchor = None;
                None
            }
        };

        // End-of-media auto-pause: the ONE cold-path push this thread ever
        // makes (same `Playback` payload shape as `rudis_transport`'s host
        // half), OUTSIDE the store lock. Wraps and ordinary ticks push
        // nothing — position rides the hot scalar path.
        if let Some(playback) = auto_paused {
            anchor = None;
            if let Ok(payload) = serde_json::to_value(&playback) {
                ring.push(crate::ring::EVENT_PLAYBACK_CHANGED, payload);
            }
        }

        std::thread::sleep(TICK_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use crate::{InitConfig, RudisBuffer, RudisCtx, RudisStatus};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    // -----------------------------------------------------------------
    // Helpers (mirroring commands.rs's test conventions: raw ABI ctx,
    // envelope round-trip, direct store/mirror fabrication — no real
    // media, no FFmpeg).
    // -----------------------------------------------------------------

    fn out_buf() -> RudisBuffer {
        RudisBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }

    fn take_json(buf: RudisBuffer) -> serde_json::Value {
        assert!(!buf.ptr.is_null(), "a filled buffer has a real allocation");
        let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        let value = serde_json::from_slice(bytes).expect("envelope is valid JSON");
        crate::rudis_free_buffer(buf);
        value
    }

    /// An opt-in ctx through the REAL ABI entry point — config bytes + len,
    /// exactly as a C# host would call it.
    fn opt_in_ctx() -> *mut RudisCtx {
        let cfg = br#"{"self_advance": true}"#;
        let ctx = crate::rudis_init(cfg.as_ptr(), cfg.len());
        assert!(!ctx.is_null(), "opt-in config must parse and build a ctx");
        ctx
    }

    /// Fabricate playable SOURCE state directly in the store — the
    /// commands.rs:761 convention (no real media, no FFmpeg): a Playback
    /// with a loaded media id + duration, preview mode Source.
    fn make_playable(ctx: *mut RudisCtx, duration_us: i64, looping: bool) {
        let inner = unsafe { &*ctx };
        let mut project = rudis_core::Project::default();
        project.preview_mode = rudis_core::PreviewMode::Source;
        project.source_playback = rudis_core::Playback {
            loaded_media_id: Some("m-self-advance-test".to_string()),
            playing: false,
            position_us: 0,
            duration_us,
            fps: 30.0,
            looping,
        };
        *inner.store.lock().expect("store lock") = rudis_core::Store::from_project(project);
    }

    /// Send one transport command through the REAL export and assert the
    /// domain envelope is `{"Ok": ..}`; returns the Ok payload.
    fn transport_ok(ctx: *mut RudisCtx, cmd: &rudis_core::TransportCmd) -> serde_json::Value {
        let json = serde_json::json!({ "cmd": cmd }).to_string();
        let mut buf = out_buf();
        assert_eq!(
            crate::commands::rudis_transport(ctx, json.as_ptr(), json.len(), &mut buf),
            RudisStatus::Ok
        );
        let envelope = take_json(buf);
        envelope
            .get("Ok")
            .unwrap_or_else(|| panic!("transport succeeded, got {envelope}"))
            .clone()
    }

    /// Every `playback:changed` record currently retained in the ctx's ring.
    fn playback_changed_records(ctx: *mut RudisCtx) -> Vec<serde_json::Value> {
        let inner = unsafe { &*ctx };
        inner
            .ring
            .poll(0)
            .events
            .iter()
            .filter(|r| r.event == crate::ring::EVENT_PLAYBACK_CHANGED)
            .map(|r| r.payload.clone())
            .collect()
    }

    fn shutdown(ctx: *mut RudisCtx) {
        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    // -----------------------------------------------------------------
    // Test 1 — THE MEASURED 2× CHECK. With InitConfig::default() (and with
    // a null config through rudis_init), position moves ONLY on explicit
    // host Advance: exactly 0 after Play + a wall-clock sleep with zero
    // Advance calls; exactly 320_000 after 20×16ms explicit ticks (a
    // double-driving engine would read roughly double). EXACT equality —
    // deliberately NO tolerance on the default path.
    // -----------------------------------------------------------------
    #[test]
    fn default_config_never_self_advances() {
        // Ctx A: the in-process constructor with InitConfig::default().
        let ctx_a = Box::into_raw(Box::new(
            RudisCtx::new_in_process(
                InitConfig::default(),
                Box::new(agent_llm::InMemoryKeyStore::new()),
            )
            .expect("in-process ctx builds"),
        ));
        // Ctx B: a null config through the REAL rudis_init (the exact call
        // shape the Tauri-equivalent host makes — no config at all).
        let ctx_b = crate::rudis_init(std::ptr::null(), 0);
        assert!(!ctx_b.is_null());

        for &ctx in &[ctx_a, ctx_b] {
            make_playable(ctx, 5_000_000, false);
            transport_ok(ctx, &rudis_core::TransportCmd::Play);
        }

        // Wall time passes; ZERO Advance calls are made.
        std::thread::sleep(Duration::from_millis(320));

        for &ctx in &[ctx_a, ctx_b] {
            assert_eq!(
                crate::commands::rudis_get_playback_position(ctx),
                0,
                "the default path is HOST-driven: with no Advance calls the \
                 position must be EXACTLY 0 — any drift means an engine \
                 clock is running (the 2× trap)"
            );
        }

        // Simulate the Tauri frontend's loop (frontend/src/main.ts:639):
        // N=20 explicit Advance ticks of 16ms each.
        for &ctx in &[ctx_a, ctx_b] {
            for _ in 0..20 {
                transport_ok(ctx, &rudis_core::TransportCmd::Advance { dt_us: 16_000 });
            }
            assert_eq!(
                crate::commands::rudis_get_playback_position(ctx),
                320_000,
                "20 × 16_000µs host ticks land at EXACTLY 320_000µs — a \
                 self-advancing engine underneath would roughly DOUBLE this"
            );
        }

        shutdown(ctx_a);
        shutdown(ctx_b);
    }

    // -----------------------------------------------------------------
    // Test 2 — the opt-in ctx advances against wall time with ZERO Advance
    // calls from the host (what the C# shell relies on).
    // -----------------------------------------------------------------
    #[test]
    fn opt_in_tracks_wall_clock_with_zero_advance_calls() {
        let ctx = opt_in_ctx();
        make_playable(ctx, 10_000_000, false);

        transport_ok(ctx, &rudis_core::TransportCmd::Play);
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(500));
        let elapsed_us = t0.elapsed().as_micros() as i64;

        let position = crate::commands::rudis_get_playback_position(ctx);
        assert!(
            position > 0,
            "the engine clock must have moved the playhead with ZERO host \
             Advance calls; position = {position}"
        );
        assert!(
            (position - elapsed_us).abs() <= 150_000,
            "opt-in position tracks wall time (±150ms for CI machine load, \
             D-47-01-01): position = {position}µs, wall = {elapsed_us}µs"
        );

        shutdown(ctx);
    }

    // -----------------------------------------------------------------
    // Test 3 — end-of-media auto-pause: playing flips false, position
    // clamps to duration, and the tick thread pushes EXACTLY ONE
    // playback:changed (beyond the one Play itself pushed) with
    // playing: false — cold-path hosts learn the stop without polling.
    // -----------------------------------------------------------------
    #[test]
    fn auto_pause_at_end_pushes_exactly_one_playback_changed() {
        let ctx = opt_in_ctx();
        let inner = unsafe { &*ctx };
        make_playable(ctx, 200_000, false);

        transport_ok(ctx, &rudis_core::TransportCmd::Play);
        std::thread::sleep(Duration::from_millis(500));

        assert!(
            !inner.mirror.playing.load(Ordering::Relaxed),
            "playback auto-paused at end of media"
        );
        assert_eq!(
            crate::commands::rudis_get_playback_position(ctx),
            200_000,
            "position clamps to duration on auto-pause"
        );

        let records = playback_changed_records(ctx);
        assert_eq!(
            records.len(),
            2,
            "exactly TWO playback:changed records: Play's own host-half push \
             + the tick thread's single auto-pause push (never per-tick spam); \
             got {records:?}"
        );
        assert_eq!(
            records[0].get("playing"),
            Some(&serde_json::json!(true)),
            "the first record is Play's (playing: true)"
        );
        assert_eq!(
            records[1].get("playing"),
            Some(&serde_json::json!(false)),
            "the tick thread's auto-pause record carries playing: false"
        );
        assert_eq!(
            records[1].get("position_us"),
            Some(&serde_json::json!(200_000)),
            "the auto-pause record carries the clamped end position"
        );

        shutdown(ctx);
    }

    // -----------------------------------------------------------------
    // Test 4 — looping wraps and stays playing, with NO tick-thread ring
    // records: wraps are position-only, riding the hot scalar path
    // (v7-ARCHITECTURE.md:101), never the cold event path.
    // -----------------------------------------------------------------
    #[test]
    fn looping_wraps_and_stays_playing_with_no_ring_spam() {
        let ctx = opt_in_ctx();
        let inner = unsafe { &*ctx };
        make_playable(ctx, 100_000, true);

        transport_ok(ctx, &rudis_core::TransportCmd::Play);
        std::thread::sleep(Duration::from_millis(400));

        assert!(
            inner.mirror.playing.load(Ordering::Relaxed),
            "looping playback never auto-pauses"
        );
        let position = crate::commands::rudis_get_playback_position(ctx);
        assert!(
            (0..100_000).contains(&position),
            "a wrapped playhead stays inside [0, duration); position = {position}"
        );

        let records = playback_changed_records(ctx);
        assert_eq!(
            records.len(),
            1,
            "ONLY Play's own host-half record — the tick thread pushes \
             NOTHING for wraps (position-only); got {records:?}"
        );

        shutdown(ctx);
    }

    // -----------------------------------------------------------------
    // Test 5 — rudis_shutdown joins the tick thread: returns Ok, does not
    // hang (a deadlock fails on the test timeout), no panic.
    // -----------------------------------------------------------------
    #[test]
    fn shutdown_joins_the_tick_thread() {
        let ctx = opt_in_ctx();
        make_playable(ctx, 10_000_000, false);
        transport_ok(ctx, &rudis_core::TransportCmd::Play);

        // Let the thread actually run a few ticks mid-play, then tear down.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            crate::rudis_shutdown(ctx),
            RudisStatus::Ok,
            "shutdown stops + joins the tick thread and tears down cleanly"
        );
    }
}
