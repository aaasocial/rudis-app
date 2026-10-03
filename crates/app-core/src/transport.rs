//! The `transport` command's STORE half ([`run_transport`]), relocated from
//! `src-tauri/src/lib.rs` by plan 47-03 (Phase 47, FFI-01) — one of the last 3
//! of RESEARCH A1's 11 under-extracted commands, alongside the two UI import
//! movers in [`crate::import`].
//!
//! The host-specific consequences deliberately did NOT move — see
//! [`TransportOutcome`] for why they stay with each host.

use crate::AppCtx;

/// What one transport command did to the store — everything host-agnostic.
/// Host-specific consequences (Tauri: the lock-free playback-mirror publish +
/// `app.emit`; FFI: its own mirror + ring push) stay with each host on
/// purpose: the playback mirror is windowed-app managed state on the Tauri
/// side and an owned `RudisCtx` field on the FFI side — an [`AppCtx`] method
/// would force a capability onto hosts that resolve it differently (the exact
/// "just widen the trait" anti-pattern 46-CONTEXT D-08 warns against).
pub struct TransportOutcome {
    /// The new authoritative playback state — the command's return value and
    /// the `playback:changed` payload, on every host.
    pub playback: rudis_core::Playback,
    /// Active preview monitor at the time of the command, read under the SAME
    /// lock as the transport apply: `true` = Source (a MediaBin clip), `false`
    /// = Program (the timeline).
    pub is_source: bool,
    /// Seek/Step only — an explicit reposition signal for the present thread
    /// (18.2-05; Advance is normal playback, never a reposition).
    pub reposition: bool,
}

/// Apply one [`rudis_core::TransportCmd`] (load_preview / play / pause / seek /
/// step / advance / set_looping) to the backend playback state and report what
/// it did, host-agnostically.
///
/// Transport is DELIBERATELY not undoable (see `rudis_core::transport`):
/// scrubbing must never eat the user's edit undo history.
///
/// The `#[tauri::command] fn transport` in `src-tauri` is now exactly this
/// call plus its unchanged HOST half (mirror publish + `playback:changed`
/// emit), driven by the returned [`TransportOutcome`].
pub fn run_transport<C: AppCtx>(
    ctx: &C,
    cmd: rudis_core::TransportCmd,
) -> Result<TransportOutcome, String> {
    // Phase 58 (PROXY-03): configure the decode-source resolver's proxy cache
    // dir on the ONE host-agnostic funnel every playback command passes through,
    // so a fresh process resolves proxies from its first Play — including for
    // media imported by a PREVIOUS run, which this process has spawned no job
    // for and would otherwise never look up.
    //
    // Here rather than in each host's startup for the reason 46-CONTEXT D-08
    // gives about widening the trait: the resolver's configuration is a property
    // of "there is an app cache dir", which is exactly what `AppCtx` already
    // provides, and putting it on the shared funnel means neither shell has to
    // learn the word "proxy" (58-CONTEXT D-24's no-shell-code rule).
    //
    // Idempotent and best-effort by design (58-04): the setter overwrites a
    // process-global, and a host that cannot resolve a cache dir simply never
    // serves proxies — D-17's degrade-to-original posture, one level up. Cost is
    // one uncontended write-lock acquisition per transport command, on a path
    // that then takes the store mutex anyway.
    if let Ok(dir) = ctx.app_cache_dir() {
        preview::decode_source::configure_proxy_cache_dir(
            dir.join(proxy::cache::PROXY_CACHE_DIR_NAME),
        );
    }

    // Phase 59 (CACHE-01, 59-CONTEXT D-24/D-39): the render cache's own funnel,
    // and for the identical reason one line up — 59-06 and 59-07 both closed
    // naming this as "the ONE production wiring point still missing".
    //
    // `poll_and_spawn` does two things, both cheap and both best-effort. It
    // points the segment reader and writer at THIS host's cache directory (one
    // uncontended write lock), which is what lets a fresh process serve segments
    // rendered by a previous run; and it starts at most ONE background render if
    // the detector has a heavy section, nothing is already rendering, the
    // playback guard allows it and the SHARED encoder permit is free. Every one
    // of those is a synchronous check; the render itself is a detached thread.
    //
    // On the transport funnel rather than in either host's startup for the
    // reason 46-CONTEXT D-08 gives about widening the trait, and because a
    // trigger that only fires at startup would never see the heat a session
    // accumulates. Playback is also exactly when a heavy section announces
    // itself — the detector's only inlet is a live tick.
    crate::render_cache_job::poll_and_spawn(ctx);

    // 18.2-05: an EXPLICIT reposition signal for the present thread's
    // multi-layer branch. Seek/Step only — a per-frame Advance is normal
    // playback, never a reposition (bumping on Advance would resurrect the
    // multi-layer teardown doom loop this signal exists to kill).
    let reposition = matches!(
        cmd,
        rudis_core::TransportCmd::Seek { .. } | rudis_core::TransportCmd::Step { .. }
    );
    // Apply, then read the active mode under the SAME lock.
    let (playback, is_source, was_playing, now_playing, program_position_us) = {
        let mut guard = ctx
            .store()
            .lock()
            .map_err(|_| "backend store mutex poisoned".to_string())?;
        // Phase 61 (WARM-04, D-07/D-08): the OLD value, read under the SAME
        // lock ONE statement before the mutation that replaces it. Deliberately
        // `playback()` and never `active_playback()`: the render cache is cut
        // on the PROGRAM (timeline) grid, and this is the identical accessor
        // the scheduler's own playback guard reads, so the two can never
        // disagree about what "playing" meant.
        let was_playing = guard.playback().playing;
        let pb = guard.transport(cmd).map_err(|e| e.to_string())?;
        let is_source = guard.preview_mode() == rudis_core::PreviewMode::Source;
        // The NEW program playback, same lock. NOT `pb`: the returned value is
        // the ACTIVE tab's playback, which in Source mode is a MediaBin clip's
        // own clock. Pressing Play on a source clip must not be read as the
        // timeline starting, and that clip's position is not a program-grid
        // position at all. Two `Copy` scalars, so the tuple carries no borrow
        // out of the block.
        let program = guard.playback();
        (
            pb,
            is_source,
            was_playing,
            program.playing,
            program.position_us,
        )
    };

    // Phase 61 (WARM-04, 61-CONTEXT D-07/D-08): THE YIELD - resume-from-pause
    // must not contend.
    //
    // POST-APPLY, and that placement is the entire point. The `poll_and_spawn`
    // call above runs BEFORE the store apply, when the program playback is
    // still the OLD value and the transition is invisible; a hook riding it
    // could never see one. This is modelled instead on
    // `render_cache_job::on_structural_edit_at_playhead`'s shape - lock, read,
    // DROP, then call - so nothing here holds the store lock into a function
    // that takes it again (`AppCtx::store()` and `PreviewHost::store()` are the
    // same non-reentrant mutex).
    //
    // EDGE-triggered, never level-triggered: Play-while-playing, Pause,
    // Seek-while-paused and every per-frame Advance leave an in-flight bake
    // exactly as they found it. Only a genuine `false -> true` costs anything,
    // and then only for a bake inside the playback guard band - one outside it
    // is paid-for work that is not in the user's way and runs to completion.
    //
    // Phase 61's idle clock makes a bake strictly MORE likely to be in flight
    // at the moment of Play, which is the new risk this guards.
    if !was_playing && now_playing {
        let new_seg = preview::render_cache_lookup::segment_index_for(program_position_us);
        crate::render_cache_job::on_play_transition(new_seg);
    }

    Ok(TransportOutcome {
        playback,
        is_source,
        reposition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixture, TestAppCtx};
    use std::path::{Path, PathBuf};

    /// Write a VALID proxy pair for `source` into `dir`, through the cache's own
    /// public write path — no test-only backdoor, so a fixture that this builds
    /// is exactly what a real generation commits.
    ///
    /// The payload's bytes are arbitrary (nothing in this gate decodes it); its
    /// LENGTH is not, because `read_fresh` cross-checks it against the meta.
    fn plant_fresh_proxy(dir: &Path, source: &Path) -> (u32, u32) {
        std::fs::create_dir_all(dir).expect("cache dir");
        let key = proxy::cache::key_for(source).expect("a committed fixture is cacheable");
        let stem = proxy::cache::file_stem_for(&key);
        let payload = dir.join(proxy::cache::payload_file_name(&stem));
        std::fs::write(&payload, vec![0xABu8; 4096]).expect("write payload");
        let bytes = std::fs::metadata(&payload).expect("stat payload").len();
        let (w, h) = proxy::cache::proxy_dims(1280, 720).expect("720p has a proxy geometry");
        let meta = proxy::cache::ProxyMeta::new(key, w, h, bytes);
        assert!(
            proxy::cache::write_meta(dir, &stem, &meta),
            "fix the fixture, not the funnel: the cache refused to commit it"
        );
        (w, h)
    }

    /// PROXY-03's host-wiring half: after ONE transport command, the
    /// decode-source resolver answers `Proxy` for a source whose proxy sits in
    /// this ctx's cache — on any host, because this function is the only funnel
    /// both of them share.
    ///
    /// The "before" reading is a real MISS against a real (empty) directory
    /// rather than the unset default, so the gate cannot pass merely because the
    /// resolver started out answering `Original` for everything.
    #[test]
    fn run_transport_configures_the_decode_source_resolver() {
        // Serializes against the other tests in this binary that touch the
        // resolver's process-global configuration.
        let _lease = crate::proxy_job::encoder_lease();
        let ctx = TestAppCtx::new();
        let src = PathBuf::from(fixture("bars_720p30_5s.mp4"));
        let dir = ctx
            .app_cache_dir()
            .expect("cache dir")
            .join(proxy::cache::PROXY_CACHE_DIR_NAME);
        let (w, h) = plant_fresh_proxy(&dir, &src);

        // Point the resolver at a real directory that holds no proxy for this
        // source: the control has to be a MISS, not an absence of configuration.
        let empty = ctx.app_data_dir().expect("data dir").join("no-proxies-here");
        std::fs::create_dir_all(&empty).expect("empty dir");
        preview::decode_source::configure_proxy_cache_dir(empty);
        let before = preview::decode_source::resolve_decode_source("clip-1", &src, 1_234_567);
        assert!(
            before.is_original(),
            "control: with the resolver pointed elsewhere the source plays from \
             its original"
        );

        // The funnel. Pause is the cheapest transport command and touches no
        // media; what matters is that EVERY transport command passes here.
        run_transport(&ctx, rudis_core::TransportCmd::Pause).expect("pause applies");

        let after = preview::decode_source::resolve_decode_source("clip-1", &src, 1_234_567);
        assert!(
            !after.is_original(),
            "after one transport command the planted proxy resolves — this is \
             the whole of PROXY-03's host wiring"
        );
        assert_eq!(
            after.path,
            dir.join(proxy::cache::payload_file_name(&proxy::cache::file_stem_for(
                &proxy::cache::key_for(&src).expect("cacheable")
            ))),
            "and it is the payload in THIS ctx's cache dir"
        );
        assert_eq!(
            after.kind,
            preview::decode_source::DecodeSourceKind::Proxy {
                proxy_w: w,
                proxy_h: h
            },
            "carrying the geometry the cache committed"
        );
        assert_eq!(
            after.source_us, 1_234_567,
            "D-04: a proxy's source clock is the IDENTITY of the requested one"
        );

        // Leave the process-global as this test found it in spirit: unset is the
        // compatibility floor, and a stale temp dir would outlive the ctx.
        preview::decode_source::configure_proxy_cache_dir(dir);
    }
}
