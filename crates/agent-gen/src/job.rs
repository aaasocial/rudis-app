//! Async job identity, handles and the in-memory registry (Phase 31, GEN-05).
//!
//! # Scope: in-memory ONLY, by decision
//!
//! [`JobRegistry`] lives for the lifetime of the process. Jobs are **lost on
//! app restart** and must be re-submitted (31-RESEARCH.md Open Question 2).
//! That is a deliberate Phase-31 boundary, not an oversight: no durable
//! job-state precedent exists in this codebase, and inventing one (persisting
//! into the `.rud` project file or a side store) is unjustified complexity for
//! a phase whose only provider is a zero-network fixture. It also caps the
//! T-31-07 DoS surface — nothing here grows across sessions.
//!
//! # Locking
//!
//! A plain `std::sync::Mutex`, matching `SharedStore = Mutex<Store>`
//! (`crates/app-core/src/lib.rs`). Every critical section here is a HashMap
//! lookup or insert — no I/O, no `.await` — so an async-aware mutex would buy
//! nothing and would drag `tokio` into this deliberately runtime-free crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::provider::JobStatus;

/// Monotonic counter behind [`JobId::mint`] — the local twin of
/// the host's `ID_SEQ` (`crates/app-core/src/import.rs`). Reimplemented here
/// rather than imported because `agent-gen` sits BELOW its host in the
/// dependency graph and cannot depend on it.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A backend-minted job identifier, shaped `job-{millis}-{seq}`.
///
/// Same vocabulary as every other id this backend mints (`media-`, `clip-`,
/// `ex-` via `next_id()`), so job ids read like the rest of the system rather
/// than introducing a second id shape (and a `uuid` dependency) for one
/// feature.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct JobId(pub String);

impl JobId {
    /// Mint a fresh id. The `seq` suffix makes ids unique even when two jobs
    /// are submitted inside the same millisecond.
    pub fn mint() -> Self {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        Self(format!("job-{millis}-{seq}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything the poll loop needs to ask a provider about one in-flight job.
///
/// Cheap to clone (the countdown is shared, not copied) so the spawned task can
/// own one without borrowing from the registry.
#[derive(Debug, Clone)]
pub struct JobHandle {
    /// Our id for the job.
    pub id: JobId,
    /// The PROVIDER-side job token — whatever string the remote service handed
    /// back at submit time and expects on a status/cancel call. Opaque to us;
    /// for a fixture provider it is simply a synthetic label.
    pub provider_job_ref: String,
    /// Fixture/test-only deterministic countdown: how many more polls must
    /// return `Pending` before the job goes terminal.
    ///
    /// This rides the HANDLE, not the provider (31-RESEARCH.md Pattern 4): a
    /// `GenProvider` is `Send + Sync` managed state that may drive several
    /// concurrent jobs, so per-job mutable state cannot live in the provider
    /// struct. Real providers leave this `None` — their "am I done yet" state
    /// lives on the remote service.
    pub polls_remaining: Option<Arc<AtomicU32>>,
}

/// Hand-written because `AtomicU32` has no `PartialEq` — two handles are equal
/// when they address the same job with the same countdown VALUE (the atomic is
/// compared by its current reading, not by `Arc` identity). Needed so
/// `SubmitOutcome`/`JobStatus` can keep their plain derives.
impl PartialEq for JobHandle {
    fn eq(&self, other: &Self) -> bool {
        let countdown = |h: &JobHandle| {
            h.polls_remaining
                .as_ref()
                .map(|c| c.load(Ordering::Relaxed))
        };
        self.id == other.id
            && self.provider_job_ref == other.provider_job_ref
            && countdown(self) == countdown(other)
    }
}

impl JobHandle {
    /// A handle for a real provider job: no scripted countdown.
    pub fn new(id: JobId, provider_job_ref: String) -> Self {
        Self {
            id,
            provider_job_ref,
            polls_remaining: None,
        }
    }

    /// A handle with a deterministic scripted countdown — how Wave 3's fixture
    /// provider makes "Ready after N polls" reproducible instead of timing-
    /// dependent.
    pub fn with_countdown(id: JobId, provider_job_ref: String, polls: u32) -> Self {
        Self {
            id,
            provider_job_ref,
            polls_remaining: Some(Arc::new(AtomicU32::new(polls))),
        }
    }

    /// Decrement the scripted countdown, returning `true` when the job should
    /// now be treated as terminal (countdown exhausted, or never set).
    ///
    /// Saturates at zero, so extra polls after the countdown ends keep
    /// answering `true` rather than wrapping around.
    pub fn tick_countdown(&self) -> bool {
        match &self.polls_remaining {
            None => true,
            Some(remaining) => {
                let prev = remaining.load(Ordering::Relaxed);
                if prev == 0 {
                    true
                } else {
                    remaining.store(prev - 1, Ordering::Relaxed);
                    prev - 1 == 0
                }
            }
        }
    }
}

/// The registry's per-job row: the last known status plus the cancel flag the
/// poll loop observes.
#[derive(Debug, Clone)]
pub struct JobRecord {
    pub status: JobStatus,
    /// Set by `cancel_generation_job`; read at the TOP of every poll iteration.
    /// An `AtomicBool` (not a channel) because cancellation is a one-way latch
    /// with no payload, and the loop is already waking on its own timer.
    pub cancel: Arc<AtomicBool>,
}

impl JobRecord {
    /// A freshly-submitted, not-yet-cancelled job.
    pub fn pending() -> Self {
        Self {
            status: JobStatus::Pending,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// In-memory job table. See the module doc for the persistence boundary.
#[derive(Debug, Default)]
pub struct JobRegistry(Mutex<HashMap<String, JobRecord>>);

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a job, returning its cancel flag so the caller can hand the
    /// same `Arc` to the spawned poll task without a second lookup.
    pub fn insert(&self, id: &JobId) -> Arc<AtomicBool> {
        let record = JobRecord::pending();
        let cancel = record.cancel.clone();
        self.lock().insert(id.0.clone(), record);
        cancel
    }

    /// The job's last known status, or `None` for an unknown id (T-31-08: an
    /// arbitrary IPC-supplied id resolves to `None`, never a panic).
    pub fn get_status(&self, id: &JobId) -> Option<JobStatus> {
        self.lock().get(id.as_str()).map(|r| r.status.clone())
    }

    /// Overwrite a job's status. `false` when the id is unknown.
    pub fn set_status(&self, id: &JobId, status: JobStatus) -> bool {
        match self.lock().get_mut(id.as_str()) {
            Some(record) => {
                record.status = status;
                true
            }
            None => false,
        }
    }

    /// The job's cancel latch, or `None` for an unknown id.
    pub fn cancel_flag(&self, id: &JobId) -> Option<Arc<AtomicBool>> {
        self.lock().get(id.as_str()).map(|r| r.cancel.clone())
    }

    /// Raise a job's cancel latch. `false` when the id is unknown — the caller
    /// turns that into a clean error naming the id (T-31-08).
    ///
    /// Only sets the flag; the poll loop owns the actual transition to
    /// `Cancelled`, so cancellation can never race the status the provider is
    /// mid-way through reporting.
    pub fn request_cancel(&self, id: &JobId) -> bool {
        match self.lock().get(id.as_str()) {
            Some(record) => {
                record.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// How many jobs are registered (tests + diagnostics).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Recover from a poisoned lock rather than propagating a panic: a job
    /// table is not worth taking the app down for, and every critical section
    /// here is a single map operation that cannot leave a torn invariant.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, JobRecord>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::AssetRef;

    #[test]
    fn job_ids_follow_the_backend_millis_seq_convention_and_are_unique() {
        let a = JobId::mint();
        let b = JobId::mint();

        assert!(a.as_str().starts_with("job-"), "prefixed like media-/clip-/ex-: {a}");
        let parts: Vec<&str> = a.as_str().split('-').collect();
        assert_eq!(parts.len(), 3, "shaped job-{{millis}}-{{seq}}: {a}");
        assert!(
            parts[1].parse::<u128>().is_ok(),
            "the middle segment is epoch millis: {a}"
        );
        assert!(
            parts[2].parse::<u64>().is_ok(),
            "the last segment is a sequence number: {a}"
        );

        assert_ne!(a, b, "two mints in the same millisecond still differ");
    }

    #[test]
    fn registry_inserts_reads_back_and_round_trips_a_status() {
        let registry = JobRegistry::new();
        let id = JobId::mint();

        assert_eq!(registry.get_status(&id), None, "not yet registered");

        let cancel = registry.insert(&id);
        assert_eq!(
            registry.get_status(&id),
            Some(JobStatus::Pending),
            "a fresh job starts Pending"
        );
        assert!(!cancel.load(Ordering::Relaxed), "and not cancelled");
        assert_eq!(registry.len(), 1);

        assert!(registry.set_status(&id, JobStatus::Cancelled));
        assert_eq!(registry.get_status(&id), Some(JobStatus::Cancelled));

        // A terminal Ready round-trips its real bytes through the registry.
        let asset = AssetRef {
            bytes: vec![1, 2, 3, 4],
            suggested_ext: "png".to_string(),
        };
        assert!(registry.set_status(&id, JobStatus::Ready(vec![asset.clone()])));
        assert_eq!(registry.get_status(&id), Some(JobStatus::Ready(vec![asset])));
    }

    /// T-31-08: an arbitrary id addresses nothing — every accessor answers
    /// "unknown" cleanly instead of panicking or creating a row.
    #[test]
    fn unknown_job_ids_resolve_to_nothing_without_panicking() {
        let registry = JobRegistry::new();
        let ghost = JobId("job-0-999999".to_string());

        assert_eq!(registry.get_status(&ghost), None);
        assert!(registry.cancel_flag(&ghost).is_none());
        assert!(!registry.set_status(&ghost, JobStatus::Cancelled));
        assert!(!registry.request_cancel(&ghost));
        assert!(registry.is_empty(), "no accessor conjured a row into existence");
    }

    #[test]
    fn request_cancel_raises_the_same_flag_the_poll_loop_holds() {
        let registry = JobRegistry::new();
        let id = JobId::mint();
        let loop_side = registry.insert(&id);

        assert!(!loop_side.load(Ordering::Relaxed));
        assert!(registry.request_cancel(&id), "a known id cancels");
        assert!(
            loop_side.load(Ordering::Relaxed),
            "the flag handed to the poll task is the SAME Arc the registry raised"
        );
        assert_eq!(
            registry.get_status(&id),
            Some(JobStatus::Pending),
            "requesting cancel does NOT itself set the terminal status — the poll loop owns that"
        );
    }

    #[test]
    fn scripted_countdown_rides_the_handle_and_saturates() {
        let two_polls = JobHandle::with_countdown(JobId::mint(), "ref".to_string(), 2);
        assert!(!two_polls.tick_countdown(), "poll 1 of 2: still pending");
        assert!(two_polls.tick_countdown(), "poll 2 of 2: terminal");
        assert!(two_polls.tick_countdown(), "further polls saturate at terminal");

        let immediate = JobHandle::with_countdown(JobId::mint(), "ref".to_string(), 0);
        assert!(immediate.tick_countdown(), "a zero countdown is terminal at once");

        let real = JobHandle::new(JobId::mint(), "remote-token".to_string());
        assert!(real.polls_remaining.is_none(), "real providers script nothing");
        assert!(real.tick_countdown(), "no countdown means the provider decides");
    }

    /// A clone of a handle shares the countdown, so the poll loop (which owns
    /// a clone) and the provider observe one countdown, not two.
    #[test]
    fn cloned_handles_share_one_countdown() {
        let original = JobHandle::with_countdown(JobId::mint(), "ref".to_string(), 2);
        let clone = original.clone();
        assert!(!original.tick_countdown(), "1 of 2 via the original");
        assert!(clone.tick_countdown(), "2 of 2 via the clone — shared state");
    }
}
