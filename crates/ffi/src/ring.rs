//! The per-[`RudisCtx`](crate::RudisCtx) bounded, ordered, tagged event ring
//! (D-09/D-10/D-11).
//!
//! # Placement (RESEARCH A4 — load-bearing)
//!
//! This ring lives ENTIRELY inside `crates/ffi`. `AppCtx::emit_patch` and its
//! sibling capabilities are TRAIT methods, monomorphized per host, so
//! `FfiAppCtx`'s implementations push here with **zero runtime relationship**
//! to the shipping shell's emit sites — no existing emit call site changes,
//! which is the strongest possible reading of this phase's "additive"
//! constraint. Attempting a shared choke point across those sites instead is
//! the single largest risk to "the shipping app is behaviorally unchanged at
//! every commit" (RESEARCH E3).
//!
//! # ABI note
//!
//! The ring TYPE is host-internal, not ABI. Only the poll EXPORT (plan 47-05)
//! crosses the C boundary, as a JSON envelope; these items are `pub` because
//! the rlib is the contract-test surface (D-16), not because C# ever sees a
//! `VecDeque`.
//!
//! # The contract (D-10/D-11/D-12)
//!
//! - **One ordered, tagged stream** `{ seq, event, payload }` per ctx, never a
//!   queue per event type (D-11): the relative order of `project:changed` and
//!   `playback:changed` is observable — a transport tick that lands after a
//!   structural edit must be applied after it.
//! - **Bounded** (D-10): capacity-limited with oldest-first eviction, so a
//!   slow (or absent) poller can never grow memory without bound (threat
//!   T-47-11).
//! - **Explicit resync, never silent truncation** (D-10, threat T-47-12):
//!   [`EventRing::poll`] answers "did I miss an event" STRUCTURALLY.
//!   `resync_required` is `true` (with EMPTY `events`) iff records the caller
//!   has not yet seen were already evicted — i.e. `local_seq + 1 <
//!   oldest_retained_seq` while newer records exist, or `local_seq <
//!   last_assigned_seq` when the ring is empty after evictions. The caller
//!   then refetches via `rudis_get_snapshot` + `rudis_get_current_seq` — S8's
//!   `base_seq`/full-resync recovery, the same protocol Phase 43 built on the
//!   shell side.
//! - **Non-blocking only** (D-12): polling never parks the caller, and no
//!   blocking variant exists this phase — Phase 50, the first real consumer,
//!   picks its own cadence.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// `project:changed` — a store mutation's flattened `Patch` envelope.
pub const EVENT_PROJECT_CHANGED: &str = "project:changed";
/// `playback:changed` — the authoritative `Playback` after a transport command.
pub const EVENT_PLAYBACK_CHANGED: &str = "playback:changed";
/// `export:progress` — encoder progress percentage (`f64`).
pub const EVENT_EXPORT_PROGRESS: &str = "export:progress";
/// `canvas-pointer` — pointer input reaching the canvas overlay. Carried in
/// the schema per D-02 even though NO live producer exists in the FFI build
/// this phase (no window, no WndProc) — see the frequency-class note on
/// [`EventRing`].
pub const EVENT_CANVAS_POINTER: &str = "canvas-pointer";
/// `gen:job` — a generation job's lifecycle state change (Phase 54's Chat
/// region is the consumer that makes this load-bearing).
pub const EVENT_GEN_JOB: &str = "gen:job";
/// `gen:progress` — a generation job's poll counter tick.
pub const EVENT_GEN_PROGRESS: &str = "gen:progress";

/// D-02's closed set — the poll envelope's `event` field is always one of
/// these six. `canvas-viewport` is deliberately EXCLUDED: Phase 51 deletes the
/// machinery that produces it, and baking a soon-to-be-deleted concept into
/// the ABI would outlive its producer.
pub const EVENT_NAMES: [&str; 6] = [
    EVENT_PROJECT_CHANGED,
    EVENT_PLAYBACK_CHANGED,
    EVENT_EXPORT_PROGRESS,
    EVENT_CANVAS_POINTER,
    EVENT_GEN_JOB,
    EVENT_GEN_PROGRESS,
];

/// One retained event: a ring-global sequence number, the D-02 tag, and the
/// event's JSON payload exactly as the equivalent shell emit would carry it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EventRecord {
    /// Ring-global, 1-based, monotonic (D-11: ONE stream). What the caller
    /// stores as its `local_seq` after processing this record.
    pub seq: u64,
    /// Tag from [`EVENT_NAMES`].
    pub event: &'static str,
    pub payload: serde_json::Value,
}

/// What one non-blocking poll observed. Serialized whole as plan 47-05's poll
/// export envelope.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PollOutcome {
    /// `true` iff records the caller has not seen were already evicted — the
    /// events list is then EMPTY and the caller must full-resync via snapshot
    /// + current-seq (never a silently truncated list, D-10 / T-47-12).
    pub resync_required: bool,
    /// The newest ASSIGNED seq (what the caller stores as its new
    /// `local_seq`). `0` when nothing has ever been pushed.
    pub next_seq: u64,
    /// Every retained record with `seq > local_seq`, in seq order. Empty when
    /// the caller is up to date — or when `resync_required` is set.
    pub events: Vec<EventRecord>,
}

/// One ordered, tagged, BOUNDED stream per `RudisCtx` (D-10/D-11).
///
/// Capacity 1024 (Claude's Discretion): the 5 discrete-action events fire
/// <10 Hz; a C# poller at even 1 Hz cannot overflow it. ⚠ FREQUENCY-CLASS
/// NOTE for Phase 51 (RESEARCH Pitfall 5): `canvas-pointer` is a continuous
/// 60-500+ Hz producer once a live source exists. NO live producer exists in
/// the FFI build this phase (no window, no WndProc) — re-size deliberately
/// (via [`EventRing::with_capacity`]) when one arrives; do not inherit 1024
/// silently.
pub struct EventRing {
    /// The retained window, oldest → newest. Seq assignment happens while
    /// holding this lock, so deque order IS seq order even under concurrent
    /// pushes (the export-progress sink runs on the encoder's thread while a
    /// command runs on the caller's) — D-11's ordering property by
    /// construction.
    entries: Mutex<VecDeque<EventRecord>>,
    /// The newest ASSIGNED seq; `0` = nothing pushed yet. Written only under
    /// the `entries` lock (see above); atomic so the newest seq stays readable
    /// without contending pushes if a future scalar export wants it.
    next_seq: AtomicU64,
    capacity: usize,
}

/// The production capacity — see the frequency-class note on [`EventRing`].
pub const DEFAULT_CAPACITY: usize = 1024;

impl EventRing {
    /// A ring at [`DEFAULT_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A ring at an explicit capacity (min 1 — a zero-capacity ring could
    /// retain nothing and would make every poll a resync). Tests use small
    /// capacities to exercise eviction cheaply; Phase 51 re-sizes here when a
    /// live `canvas-pointer` producer arrives.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
            next_seq: AtomicU64::new(0),
            capacity: capacity.max(1),
        }
    }

    /// Append one tagged event, assigning `seq = next_seq + 1` and evicting
    /// the OLDEST entry when the ring would exceed capacity. Returns the
    /// assigned seq.
    ///
    /// Poisoned-mutex tolerant (`unwrap_or_else(|p| p.into_inner())`, the
    /// `test_support` idiom): a panic caught elsewhere by `ffi_guard!` must
    /// not kill event delivery for the rest of the ctx's life (T-47-01).
    pub fn push(&self, event: &'static str, payload: serde_json::Value) -> u64 {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        // fetch_add under the lock: the ordering guarantee, not a hot path —
        // see the field doc.
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        entries.push_back(EventRecord {
            seq,
            event,
            payload,
        });
        while entries.len() > self.capacity {
            entries.pop_front();
        }
        seq
    }

    /// Non-blocking (D-12): report every retained record newer than
    /// `local_seq`, or an explicit `resync_required` when some such record was
    /// already evicted. See the module doc's contract for the exact rule.
    pub fn poll(&self, local_seq: u64) -> PollOutcome {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        // Read under the same lock the writers hold, so (entries, next_seq)
        // is one consistent view.
        let next_seq = self.next_seq.load(Ordering::Relaxed);
        match entries.front().map(|r| r.seq) {
            // Records (local_seq+1 .. oldest-1) existed, were never seen by
            // this caller, and are gone: the structural resync answer. Note
            // the boundary: a caller at EXACTLY oldest-1 has seen everything
            // that was evicted, misses nothing, and takes the clean branch
            // below — resync fires iff data was actually lost to the caller,
            // never as a heuristic (D-10 / T-47-12). `saturating_add` (WR-01):
            // `local_seq` is caller-supplied across the C ABI with no upper
            // bound, and a bare `+ 1` at `u64::MAX` wraps to 0 in release
            // (no `[profile.*]` overrides anywhere — see export.rs's audit),
            // firing a spurious resync; saturated, `u64::MAX` stays on the
            // clean side, which is the mathematically intended comparison.
            Some(oldest) if local_seq.saturating_add(1) < oldest => PollOutcome {
                resync_required: true,
                next_seq,
                events: Vec::new(),
            },
            Some(_) => PollOutcome {
                resync_required: false,
                next_seq,
                events: entries
                    .iter()
                    .filter(|r| r.seq > local_seq)
                    .cloned()
                    .collect(),
            },
            // Empty ring. With capacity >= 1 this only happens before the
            // first push (`next_seq == 0`, nothing to miss); the general form
            // also answers the contract's empty-after-evictions branch —
            // every record the caller has not seen is gone.
            None => PollOutcome {
                resync_required: local_seq < next_seq,
                next_seq,
                events: Vec::new(),
            },
        }
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Semantics test (1): fewer pushes than capacity → poll(0) returns every
    /// record, in order, no resync.
    #[test]
    fn under_capacity_poll_zero_returns_all_in_order() {
        let ring = EventRing::with_capacity(8);
        for i in 0..5u64 {
            let seq = ring.push(EVENT_PROJECT_CHANGED, json!({ "i": i }));
            assert_eq!(seq, i + 1, "seqs are 1-based and monotonic");
        }

        let got = ring.poll(0);
        assert!(!got.resync_required, "nothing evicted, nothing to resync");
        assert_eq!(got.next_seq, 5);
        assert_eq!(got.events.len(), 5);
        for (i, rec) in got.events.iter().enumerate() {
            assert_eq!(rec.seq, i as u64 + 1, "in seq order");
            assert_eq!(rec.payload, json!({ "i": i as u64 }));
        }
    }

    /// Semantics test (2): overflow evicts oldest-first and a poll that
    /// genuinely missed evicted records gets an explicit resync with EMPTY
    /// events — never a silently truncated list (D-10 / T-47-12).
    ///
    /// Boundary note (the module contract's rule, applied at both edges): the
    /// plan's contract line — resync iff `local_seq + 1 < oldest_retained` —
    /// puts a caller at exactly `oldest_retained - 1` on the CLEAN side (it
    /// saw every evicted record; every unseen record is retained), so that
    /// caller gets the full retained window including the oldest record. The
    /// plan's test-enumeration line placed that same boundary on the resync
    /// side, which contradicts both its own contract formula and its own
    /// no-eviction case (1); the formula is what this module implements —
    /// recorded as a deviation in the 47-04 SUMMARY.
    #[test]
    fn overflow_reports_resync_explicitly_never_a_truncated_list() {
        // Capacity 8, push 11 → seqs 1..=11 assigned, 1..=3 evicted,
        // retained window = 4..=11 (oldest_retained = 4).
        let ring = EventRing::with_capacity(8);
        for i in 0..11u64 {
            ring.push(EVENT_PROJECT_CHANGED, json!({ "i": i }));
        }

        // A fresh caller (local_seq 0) missed seqs 1..=3 → structural resync,
        // EMPTY events.
        let got = ring.poll(0);
        assert!(got.resync_required, "evicted unseen records force a resync");
        assert!(got.events.is_empty(), "resync never carries a partial list");
        assert_eq!(got.next_seq, 11);

        // local_seq 2 (< oldest-1): seq 3 was evicted unseen → resync.
        let got = ring.poll(2);
        assert!(got.resync_required);
        assert!(got.events.is_empty());

        // local_seq 3 (== oldest-1): every evicted record was seen; every
        // unseen record (4..=11) is retained → the full window, clean.
        let got = ring.poll(3);
        assert!(!got.resync_required, "nothing this caller needs was lost");
        assert_eq!(
            got.events.iter().map(|r| r.seq).collect::<Vec<_>>(),
            (4..=11).collect::<Vec<_>>(),
            "the retained window arrives whole, oldest record included"
        );

        // local_seq 4 (== oldest): the retained tail after it.
        let got = ring.poll(4);
        assert!(!got.resync_required);
        assert_eq!(
            got.events.iter().map(|r| r.seq).collect::<Vec<_>>(),
            (5..=11).collect::<Vec<_>>()
        );
    }

    /// Semantics test (3): interleaved tags preserve RELATIVE order — D-11's
    /// whole point. A playback tick pushed after a structural edit must come
    /// back after it, which per-type queues structurally cannot guarantee.
    #[test]
    fn interleaved_tags_preserve_relative_order() {
        let ring = EventRing::with_capacity(16);
        let script: [(&str, serde_json::Value); 5] = [
            (EVENT_PROJECT_CHANGED, json!({ "kind": "edit-1" })),
            (EVENT_PLAYBACK_CHANGED, json!({ "position_us": 100 })),
            (EVENT_PROJECT_CHANGED, json!({ "kind": "edit-2" })),
            (EVENT_EXPORT_PROGRESS, json!(0.5)),
            (EVENT_CANVAS_POINTER, json!({ "phase": "move", "x": 1.0, "y": 2.0 })),
        ];
        for (event, payload) in &script {
            ring.push(event, payload.clone());
        }

        let got = ring.poll(0);
        assert!(!got.resync_required);
        let tags: Vec<&str> = got.events.iter().map(|r| r.event).collect();
        assert_eq!(
            tags,
            script.iter().map(|(e, _)| *e).collect::<Vec<_>>(),
            "one stream, push order preserved across tags"
        );
        for (rec, (_, payload)) in got.events.iter().zip(script.iter()) {
            assert_eq!(&rec.payload, payload, "payloads ride along untouched");
        }
    }

    /// Semantics test (4): a caller that is fully caught up gets an empty,
    /// resync-free poll — and a never-pushed ring answers poll(0) the same way.
    #[test]
    fn caught_up_poll_is_empty_and_clean() {
        let ring = EventRing::with_capacity(8);
        let fresh = ring.poll(0);
        assert!(!fresh.resync_required, "an empty ring has nothing to miss");
        assert_eq!(fresh.next_seq, 0);
        assert!(fresh.events.is_empty());

        for i in 0..3u64 {
            ring.push(EVENT_GEN_JOB, json!({ "i": i }));
        }
        let got = ring.poll(3);
        assert!(!got.resync_required);
        assert_eq!(got.next_seq, 3, "next_seq is what the caller keeps");
        assert!(got.events.is_empty());
    }

    /// WR-01's boundary gate: `local_seq = u64::MAX` must neither wrap nor
    /// resync. A C ABI takes whatever the caller hands it, and the resync
    /// check's `local_seq + 1` would panic under debug overflow checks and
    /// silently wrap to `0` in release (the workspace has no `[profile.*]`
    /// overrides, so release is wrapping arithmetic) — turning "this caller
    /// has seen every representable seq" into a spurious `resync_required`.
    /// `saturating_add` keeps the caller on the clean side: no record it has
    /// not seen can exist, so the poll comes back empty and resync-free.
    #[test]
    fn poll_at_u64_max_local_seq_neither_wraps_nor_resyncs() {
        // Same eviction setup as semantics test (2): capacity 8, 11 pushes,
        // retained window 4..=11 — so the wrapped comparison (`0 < 4`) WOULD
        // fire a resync if the addition ever wrapped.
        let ring = EventRing::with_capacity(8);
        for i in 0..11u64 {
            ring.push(EVENT_PROJECT_CHANGED, json!({ "i": i }));
        }

        let got = ring.poll(u64::MAX);
        assert!(
            !got.resync_required,
            "u64::MAX has by definition seen every assignable seq — never a resync"
        );
        assert!(got.events.is_empty(), "no retained record can exceed u64::MAX");
        assert_eq!(got.next_seq, 11, "next_seq still reports the newest assigned seq");
    }

    /// D-02's closed set is exactly the six tags, no duplicates.
    #[test]
    fn event_names_are_six_and_unique() {
        let mut names = EVENT_NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 6, "six distinct event tags (D-02)");
        assert!(
            !EVENT_NAMES.contains(&"canvas-viewport"),
            "canvas-viewport is excluded by D-02 (Phase 51 deletes its producer)"
        );
    }
}
