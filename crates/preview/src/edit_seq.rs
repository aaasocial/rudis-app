//! Phase 46 (XTRC-02, plan 46-05): the mid-play edit signal — state plus its
//! parse-and-record logic — moved out of `src-tauri/src/native_surface.rs`.
//!
//! # Why this is NOT a `PreviewHost` callback method
//!
//! `46-RESEARCH.md` § "The hard one" settles it: `register_edit_seq_listener`
//! does three separable things today, and only ONE of them is shell-specific.
//! The STATE ([`PreviewEditPending`] / [`PreviewEditSeq`]) and the CLOSURE BODY
//! (check the patch is preview-relevant, record which clips it touched, bump the
//! counter) are plain Rust over already-typed values; only `app.manage(...)` and
//! `app.listen_any(EVENT, ...)` need the desktop shell.
//!
//! So the split is: this crate owns the state and [`PreviewEditSeq::observe_patch`];
//! the shell keeps a small `register_edit_seq_listener` that constructs the
//! `Arc`, manages it, registers the raw listener, parses the payload, and calls
//! `observe_patch`. No callback, no `Box<dyn Fn>`, no `Send + Sync + 'static`
//! closure bound crossing the crate boundary — and no event name in this crate's
//! CODE. (`PROJECT_CHANGED_EVENT` survives below only inside a verbatim-moved
//! doc comment, describing the shell listener that drives this state; nothing
//! here can be compiled against it, the same way `predicates.rs` already
//! documents it without depending on it.)
//!
//! That is the same shape [`crate::PlaybackMirror`] already proves in shipped
//! code: shell code constructs and writes it, the present loop is handed the
//! `Arc` and only ever reads it. Wave 46-10 completes the pattern by taking
//! `edit_seq: Arc<PreviewEditSeq>` as an explicit `present_loop` parameter
//! (mirroring how `spawn_producer` already takes `Arc<Compositor>` directly),
//! rather than looking it up through managed state.
//!
//! # Visibility note (XTRC-04)
//!
//! `seq` and `pending` stay PUBLIC FIELDS, not accessors: the two
//! `AppHandle`-needing tests that stay shell-side per D-04 read
//! `state.seq.load(...)` and `state.pending.lock()` directly, and D-04's whole
//! point is that those tests move to the new crate's API without their own
//! bodies changing. Widening `pub(crate)` to `pub` here is not an XTRC-04
//! public-API event — it is this new crate's own surface, which the pub-item
//! baseline deliberately excludes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Accumulated single-layer-preview invalidation since the presenter last
/// consumed it (18.3-03 Issue-B). `clip_ids` are the timeline clips a mid-play
/// edit touched; `structural` marks edits that change the clip SET / layout /
/// global timebase, where a plain active-clip-id match cannot prove the visible
/// frame is unaffected (a removed clip leaves the active set; a whole-turn
/// revert can restructure). Drained by the presenter on every edit-seq change.
#[derive(Default)]
pub struct PreviewEditPending {
    pub clip_ids: Vec<String>,
    pub structural: bool,
}

/// Mid-play edit signal (18.3 Stage 1b, design §4). `seq` is a low-frequency
/// counter the `PROJECT_CHANGED_EVENT` listener bumps for preview-relevant edits
/// only; `pending` records WHICH clips changed so the presenter flushes the
/// single-layer ring ONLY when the edit affects the frame on screen. Without the
/// `pending` filter, ANY project change (e.g. moving a clip on a non-visible
/// track, or drawing canvas ink) would drain-and-refill the ring → a ~ring-depth
/// (~0.6-1s) repopulate freeze of the current preview (18.3-03 live Issue-B).
pub struct PreviewEditSeq {
    pub seq: AtomicU64,
    pub pending: Mutex<PreviewEditPending>,
}

impl PreviewEditSeq {
    /// A fresh, unarmed signal — what the shell's `register_edit_seq_listener`
    /// manages (today's inline `PreviewEditSeq { seq: AtomicU64::new(0), pending:
    /// Mutex::new(PreviewEditPending::default()) }` struct literal, which the
    /// shell can no longer write now that the fields' owning crate is this one).
    pub fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            pending: Mutex::new(PreviewEditPending::default()),
        }
    }

    /// Today's `register_edit_seq_listener` closure body, minus the shell's
    /// registration mechanics — parse-and-record only. Takes an ALREADY-TYPED
    /// [`rudis_core::Patch`], so it cannot be reached with a malformed payload at
    /// all (T-18.3-03-02's silent-no-op-on-garbage tolerance stays in the shell's
    /// listener, where the `&str` actually is).
    pub fn observe_patch(&self, patch: &rudis_core::Patch) {
        // Issue-B: only edits that can change the single-layer preview
        // frame/mix arm a flush — media imports, canvas ink, and
        // notification-only kinds must NOT drain the playback ring.
        if !crate::patch_touches_preview(patch.kind) {
            return;
        }
        // Record the affected clips BEFORE bumping seq so a presenter that
        // observes the bump (then locks `pending`) always sees these ids (the
        // mutex release→acquire orders the data across threads).
        if let Ok(mut p) = self.pending.lock() {
            p.clip_ids.extend(patch.ids.iter().cloned());
            p.structural |= crate::patch_is_structural(patch.kind);
        }
        self.seq.fetch_add(1, Ordering::Relaxed);
    }

    /// Drain (take) the accumulated single-layer invalidation. Today's
    /// `drain_preview_edit_pending` body, minus the managed-state lookup: returns
    /// the default (empty, non-structural) set during a lock poisoning /
    /// teardown race — a `try_lock`-safe read, never a panic.
    pub fn drain_pending(&self) -> PreviewEditPending {
        self.pending
            .lock()
            .ok()
            .map(|mut p| std::mem::take(&mut *p))
            .unwrap_or_default()
    }
}

impl Default for PreviewEditSeq {
    fn default() -> Self {
        Self::new()
    }
}
