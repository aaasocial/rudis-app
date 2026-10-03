//! `rudis_dispatch_command` — the ONE fat JSON entry point (D-04).
//!
//! # Why one fat export + 16 named thin ones, and never a universal invoke
//!
//! D-04 mirrors today's Tauri surface 1:1: the shell has exactly one
//! generic-payload command (`dispatch_command`, carrying the whole
//! `rudis_core::Command` enum) and named commands for everything else. A
//! single universal `(name, json)` entry point was rejected because SC-1 —
//! "exposes all 17 live commands" — must stay verifiable by SYMBOL
//! ENUMERATION: 47-07's export-table test asserts the built DLL's export
//! table against the `FFI_EXPORTS` registry set-equal in both directions,
//! which one universal symbol would collapse into an unfalsifiable "exposes
//! one command".
//!
//! # D-03 data-row note (future generation commands)
//!
//! When a later phase adds generation commands to this surface, each is one
//! more data-row-shaped fn in `commands.rs` (Args struct + a
//! [`call_json`] body over its `run_*` symbol) — NOT new variants funneled
//! through this fat entry point. `Command` here stays exactly the store's
//! undoable-mutation enum, as it is on the Tauri side.

use crate::commands::call_json;
use crate::ctx::FfiAppCtx;
use crate::{RudisBuffer, RudisCtx, RudisStatus};

/// Args for [`rudis_dispatch_command`]: `{"cmd": {..Command..}}` — the
/// adjacently-tagged `rudis_core::Command` wire shape
/// (`{"type": "add_clip", "data": {..}}`), unchanged from today's IPC.
#[derive(serde::Deserialize)]
struct DispatchArgs {
    cmd: rudis_core::Command,
}

/// `dispatch_command` — apply one undoable store mutation;
/// `{"Ok": {..Patch..}}`. The `project:changed` push (flattened envelope,
/// seq pair included) happens inside `dispatch_command_inner` via
/// `FfiAppCtx::emit_patch` — zero wiring here.
///
/// # The preview hook (Phase 51, SHELL-04)
///
/// This is the ANNOTATION path: an `AddAnnotation` / `RemoveAnnotation` /
/// `ClearCanvas` command arrives here and must reach the C# shell's
/// committed-ink mirror, or the user draws a mark and no ink appears over the
/// preview. [`RudisCtx::observe_preview_patch`] is that hook — it bumps the
/// mid-play edit-flush signal and runs `panel::overlay::apply_patch_to_overlay`
/// against the store's authoritative FrameLinked-only list. It is called AFTER
/// a successful dispatch (so a rejected command never inks) and BEFORE the
/// envelope is written, on the caller's thread, holding no domain lock.
#[no_mangle]
pub extern "C" fn rudis_dispatch_command(
    ctx: *mut RudisCtx,
    json: *const u8,
    len: usize,
    out: *mut RudisBuffer,
) -> RudisStatus {
    crate::ffi_guard!("rudis_dispatch_command", RudisStatus::PanicCaught, {
        call_json(ctx, json, len, out, |c, args: DispatchArgs| {
            let patch = app_core::dispatch_command_inner(&FfiAppCtx::new(c), args.cmd)?;
            c.observe_preview_patch(&patch);
            Ok(patch)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InitConfig;

    /// The fat entry point drives a REAL store mutation end-to-end: a
    /// dispatched `add_track` returns `{"Ok": {..Patch..}}` and its flattened
    /// `project:changed` envelope lands in the ctx's ring (via
    /// `FfiAppCtx::emit_patch` — the zero-wiring claim, witnessed).
    #[test]
    fn dispatch_command_mutates_the_store_and_pushes_project_changed() {
        let ctx = Box::into_raw(Box::new(
            RudisCtx::new_in_process(
                InitConfig::default(),
                Box::new(agent_llm::InMemoryKeyStore::new()),
            )
            .expect("in-process ctx builds"),
        ));

        // Build the wire bytes from the REAL enum — no hand-guessed tags.
        let cmd = rudis_core::Command::AddTrack {
            kind: rudis_core::TrackKind::Video,
        };
        let args = serde_json::json!({ "cmd": cmd }).to_string();

        let mut buf = crate::RudisBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        };
        assert_eq!(
            rudis_dispatch_command(ctx, args.as_ptr(), args.len(), &mut buf),
            RudisStatus::Ok
        );
        let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        let envelope: serde_json::Value =
            serde_json::from_slice(bytes).expect("envelope is valid JSON");
        crate::rudis_free_buffer(buf);
        let patch: rudis_core::Patch = serde_json::from_value(
            envelope.get("Ok").expect("a real command dispatches Ok").clone(),
        )
        .expect("the Ok payload is a bare Patch");

        // The mutation is real, and its event reached the ring flattened.
        let inner = unsafe { &*ctx };
        let polled = inner.ring.poll(0);
        assert_eq!(polled.events.len(), 1, "one push for the one patch");
        assert_eq!(polled.events[0].event, crate::ring::EVENT_PROJECT_CHANGED);
        let ring_patch: rudis_core::Patch =
            serde_json::from_value(polled.events[0].payload.clone())
                .expect("ring payload parses as a bare Patch — flattened");
        assert_eq!(ring_patch, patch, "command return and event carry the same patch");

        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }

    /// Phase 51 (SHELL-04), the wiring witnessed end to end: a REAL
    /// `AddAnnotation` dispatched through the ABI reaches the C# shell's
    /// committed-ink mirror, a WHITEBOARD mark never does (T-51-13), and
    /// `rudis_undo` un-inks — the case a dispatch-only hook would miss.
    #[test]
    fn annotation_dispatch_inks_the_preview_mirror_and_undo_un_inks_it() {
        let ctx = Box::into_raw(Box::new(
            RudisCtx::new_in_process(
                InitConfig::default(),
                Box::new(agent_llm::InMemoryKeyStore::new()),
            )
            .expect("in-process ctx builds"),
        ));

        fn mark(id: &str, space: rudis_core::AnnotationSpace) -> rudis_core::Annotation {
            rudis_core::Annotation {
                id: id.into(),
                shape: rudis_core::AnnotationShape::Stroke {
                    points: vec![
                        rudis_core::NormPoint { x: 0.1, y: 0.1 },
                        rudis_core::NormPoint { x: 0.4, y: 0.4 },
                        rudis_core::NormPoint { x: 0.8, y: 0.2 },
                    ],
                },
                linked_range_us: None,
                space,
            }
        }

        fn dispatch(ctx: *mut RudisCtx, cmd: rudis_core::Command) {
            let args = serde_json::json!({ "cmd": cmd }).to_string();
            let mut buf = crate::RudisBuffer {
                ptr: std::ptr::null_mut(),
                len: 0,
                cap: 0,
            };
            assert_eq!(
                rudis_dispatch_command(ctx, args.as_ptr(), args.len(), &mut buf),
                RudisStatus::Ok
            );
            let bytes = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
            let envelope: serde_json::Value = serde_json::from_slice(bytes).expect("valid JSON");
            crate::rudis_free_buffer(buf);
            assert!(
                envelope.get("Ok").is_some(),
                "the command must succeed, not report a domain error: {envelope}"
            );
        }

        /// The ids the ink mirror would draw right now.
        fn inked(ctx: *mut RudisCtx) -> Vec<String> {
            let inner = unsafe { &*ctx };
            let mirror = inner.overlay.lock().expect("overlay mirror is healthy");
            crate::panel::overlay::visible_annotations_with_alpha(
                &mirror,
                std::time::Instant::now(),
            )
            .into_iter()
            .map(|(a, _)| a.id)
            .collect()
        }

        assert!(inked(ctx).is_empty(), "a fresh instance has no ink");

        // A frame-linked mark inks the preview.
        dispatch(
            ctx,
            rudis_core::Command::AddAnnotation(mark("fl1", rudis_core::AnnotationSpace::FrameLinked)),
        );
        assert_eq!(inked(ctx), vec!["fl1".to_string()]);

        // T-51-13: a WHITEBOARD mark reaches the store but never the overlay.
        dispatch(
            ctx,
            rudis_core::Command::AddAnnotation(mark("wb1", rudis_core::AnnotationSpace::Whiteboard)),
        );
        assert_eq!(
            inked(ctx),
            vec!["fl1".to_string()],
            "a whiteboard-space mark must never composite onto real video"
        );

        // Undo the whiteboard add, then the frame-linked one: the second undo
        // must un-ink. (`rudis_undo` is in `commands.rs`; the hook is the same.)
        for _ in 0..2 {
            let mut buf = crate::RudisBuffer {
                ptr: std::ptr::null_mut(),
                len: 0,
                cap: 0,
            };
            assert_eq!(crate::commands::rudis_undo(ctx, &mut buf), RudisStatus::Ok);
            crate::rudis_free_buffer(buf);
        }
        assert!(
            inked(ctx).is_empty(),
            "an undone AddAnnotation must remove its ink from the preview"
        );

        assert_eq!(crate::rudis_shutdown(ctx), RudisStatus::Ok);
    }
}
