//! `FixtureGenProvider` — a scripted, zero-network [`GenProvider`] implementation.
//!
//! # Why this is ALWAYS compiled (not `#[cfg(test)]`-gated)
//!
//! Same reasoning as [`agent_llm::FixtureTransport`] and
//! `agent_llm::InMemoryKeyStore`: the HOST crates' own integration tests
//! (`crates/app-core`, `crates/ffi`) must be able to CONSTRUCT this type, and a
//! `#[cfg(test)]` item in *this* crate is
//! invisible to a *different* crate's test build. It is also Phase 31's ONLY
//! concrete `GenProvider` — the one thing that makes `submit_generation_job` a
//! real, IPC-reachable command rather than a signature with no implementor.
//!
//! # Structural offline proof (31-RESEARCH.md Pitfall 4)
//!
//! This struct holds NO `reqwest::Client` and no other socket-capable field. It
//! cannot perform network I/O — that is a property of its *shape*, checkable by
//! reading it, not a runtime harness that has to be trusted to be installed.
//! The bytes it returns are handed in by the caller (real committed media from
//! `test-media/`), so "real data only" holds without this library code ever
//! hardcoding a test-fixture path.
//!
//! # Determinism
//!
//! Status transitions consult ONLY a per-job countdown integer. No wall clock,
//! no randomness, no timing dependence — two identical runs produce identical
//! poll sequences. (`JobId::mint()` reads the clock, but that is identity
//! minting, not a status transition.)
//!
//! # Where the mutable countdown lives
//!
//! On the [`JobHandle`], never on the provider (31-RESEARCH.md Pattern 4). A
//! `GenProvider` is `Send + Sync` managed state that may drive several
//! concurrent jobs, so per-job mutable state in the provider struct would be
//! both wrong (jobs would share a countdown) and un-`Sync` (the `RefCell` trap
//! `FixtureTransport` can afford only because it is single-threaded test-only).
//!
//! [`FixtureGenProvider::submit_calls`] and
//! [`FixtureGenProvider::last_request`] are the only two pieces of
//! provider-level mutable state, and neither is an exception to that rule: both
//! are PER-PROVIDER by intent (shared deliberately across every job and every
//! clone), both are `Sync` behind an `Arc` and need no `RefCell`, and neither is
//! ever read by a status transition, so determinism is untouched.
//!
//! Phase 55.1 (plan 04) added the counter because the WR-04 claim "the spend
//! gate refuses BEFORE any provider work" cannot be distinguished from
//! "submitted, then the result was thrown away" by an error return alone. Only a
//! call count can — the same discipline `allow_list_rejects_before_submit` has
//! used since Phase 31.
//!
//! Phase 56 (plan 05) added the request capture for the adjacent question. Since
//! [`GenRequest::source_video`](crate::GenRequest::source_video) SELECTS the
//! `/v1/video_to_video` endpoint, "the legacy video seam cannot reach the
//! clip-edit endpoint" is a claim about one field at the moment of dispatch —
//! and a count cannot see fields.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::job::{JobHandle, JobId};
use crate::provider::{
    AssetRef, GenError, GenProvider, GenRequest, JobStatus, ModelInfo, ProviderId, SubmitOutcome,
};

/// This provider's stable registered id — a fixed server-side string. It is
/// what the host's landing bridge (`app_core::generation_host`) interpolates
/// into the generated filename, so it must never be free text.
pub const FIXTURE_PROVIDER_ID: &str = "fixture";

/// Which of GEN-05's two submit shapes this provider exercises.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FixtureMode {
    /// Synchronous: `submit` returns [`SubmitOutcome::Ready`] immediately. The
    /// degenerate ZERO-poll case (image/TTS providers behave this way).
    Sync,
    /// Asynchronous: `submit` returns [`SubmitOutcome::Pending`] and the caller
    /// polls. `ready_after_n_polls` polls answer `Pending`; poll number
    /// `n + 1` answers `Ready`.
    Async { ready_after_n_polls: u32 },
}

/// A deterministic, zero-network `GenProvider` returning REAL caller-supplied
/// media bytes.
///
/// Construct with [`sync_still`](Self::sync_still) or
/// [`async_clip`](Self::async_clip).
#[derive(Debug, Clone)]
pub struct FixtureGenProvider {
    models: Vec<ModelInfo>,
    mode: FixtureMode,
    asset_bytes: Vec<u8>,
    asset_ext: String,
    /// How many times [`GenProvider::submit`] has been ENTERED — read back with
    /// [`submit_calls`](Self::submit_calls). SHARED by every clone (that is the
    /// point: a test can keep a cheap probe handle after moving the provider
    /// into a `ConcreteGenProvider` behind an `Arc`). Never consulted by a
    /// status transition, so determinism is untouched.
    submit_calls: Arc<AtomicU32>,
    /// The LAST [`GenRequest`] handed to [`GenProvider::submit`] — read back
    /// with [`last_request`](Self::last_request). Shared across clones for the
    /// same reason the counter is, and equally never consulted by a status
    /// transition. Phase 56 (plan 05); see that method's doc for why a count was
    /// not enough this time.
    last_request: Arc<Mutex<Option<GenRequest>>>,
}

impl FixtureGenProvider {
    /// The canned catalog both constructors carry (GEN-07).
    ///
    /// Deliberately shaped to make later waves' proofs possible against a
    /// GENUINELY-offered model rather than an invented one:
    /// - `fixture-image` — watermark **false**
    /// - `fixture-video` — watermark **true**  (both GEN-09 disclosure paths)
    /// - `fixture-nonclean` — offered here, but Wave 4's GEN-08 allow-list
    ///   EXCLUDES it, so SC-4's "rejected before submit" is proven against a
    ///   model the provider really advertises.
    fn canned_models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "fixture-image".to_string(),
                label: "Fixture Still Image".to_string(),
                modality: "image".to_string(),
                carries_provenance_watermark: false,
                rough_cost_signal: Some("free (fixture)".to_string()),
            },
            ModelInfo {
                id: "fixture-video".to_string(),
                label: "Fixture Video Clip".to_string(),
                modality: "video".to_string(),
                carries_provenance_watermark: true,
                rough_cost_signal: Some("free (fixture)".to_string()),
            },
            ModelInfo {
                id: "fixture-nonclean".to_string(),
                label: "Fixture Non-Clean-Licensed Model".to_string(),
                modality: "video".to_string(),
                carries_provenance_watermark: false,
                rough_cost_signal: None,
            },
        ]
    }

    /// A SYNCHRONOUS provider: `submit` hands back the bytes at once, the poll
    /// loop is never entered, and zero `gen:progress` events are emitted.
    ///
    /// `bytes` are REAL encoded media supplied by the caller (e.g.
    /// `std::fs::read("../../test-media/still.png")`); `ext` is the extension
    /// WITHOUT a dot.
    pub fn sync_still(bytes: Vec<u8>, ext: &str) -> Self {
        Self {
            models: Self::canned_models(),
            mode: FixtureMode::Sync,
            asset_bytes: bytes,
            asset_ext: ext.to_string(),
            submit_calls: Arc::new(AtomicU32::new(0)),
            last_request: Arc::new(Mutex::new(None)),
        }
    }

    /// An ASYNCHRONOUS provider: `submit` returns a pollable handle, exactly
    /// `ready_after_n_polls` polls answer `Pending`, and the next one answers
    /// `Ready` with the real bytes.
    ///
    /// `ready_after_n_polls == 0` still exercises the poll loop once — the
    /// FIRST poll returns `Ready`.
    ///
    /// **Note the off-by-one difference from [`JobHandle::tick_countdown`]**:
    /// that helper treats the Nth poll as terminal (N-1 pending polls). This
    /// provider deliberately does NOT use it, decrementing the same counter with
    /// the more obvious "N means N pending polls" semantics instead, so a test
    /// scripting "2 pending polls" writes `2`.
    pub fn async_clip(bytes: Vec<u8>, ext: &str, ready_after_n_polls: u32) -> Self {
        Self {
            models: Self::canned_models(),
            mode: FixtureMode::Async { ready_after_n_polls },
            asset_bytes: bytes,
            asset_ext: ext.to_string(),
            submit_calls: Arc::new(AtomicU32::new(0)),
            last_request: Arc::new(Mutex::new(None)),
        }
    }

    /// How many bytes of real media this provider will hand back (diagnostics
    /// and test assertions — never used by a status transition).
    pub fn asset_len(&self) -> usize {
        self.asset_bytes.len()
    }

    /// How many times [`GenProvider::submit`] has been ENTERED on this provider
    /// **or on any clone of it** (Phase 55.1, plan 04).
    ///
    /// The only honest way to prove a refusal happened BEFORE the provider was
    /// reached: an `Err` return alone cannot distinguish "never submitted" from
    /// "submitted, then the result was discarded", and on a PAID provider those
    /// two are the difference between $0 and a real charge. `crates/app-core`'s
    /// WR-04 suite reads this after an unapproved
    /// `submit_generation_job_inner`; `crates/agent-gen`'s own
    /// `allow_list_rejects_before_submit` established the pattern with a local
    /// counting double.
    pub fn submit_calls(&self) -> u32 {
        self.submit_calls.load(Ordering::SeqCst)
    }

    /// The LAST [`GenRequest`] this provider (or any clone of it) was asked to
    /// submit — Phase 56 (plan 05).
    ///
    /// [`submit_calls`](Self::submit_calls) answers *whether* the wire was
    /// reached; this answers *with what*, which is a different and sometimes
    /// sharper question. Phase 56 needs it because
    /// [`GenRequest::source_video`](crate::GenRequest::source_video) is an
    /// ENDPOINT SELECTOR: `RunwayProvider::submit` routes to
    /// `/v1/video_to_video` on its presence and on nothing else. "The legacy
    /// text/image->video seam cannot reach the clip-edit endpoint" is therefore
    /// a claim about a FIELD at the moment of dispatch, and the only
    /// non-vacuous way to check it is to read the request that actually crossed
    /// that boundary — a request built by production code, not one a test
    /// wrote out and then asserted about.
    ///
    /// Captured at `submit`'s entry, before any branch, so it records what was
    /// asked regardless of what came back. Returns a clone; `None` before the
    /// first submit.
    pub fn last_request(&self) -> Option<GenRequest> {
        self.last_request
            .lock()
            .expect("fixture last_request mutex poisoned")
            .clone()
    }

    /// One `AssetRef` carrying a fresh clone of the real bytes.
    fn asset(&self) -> AssetRef {
        AssetRef {
            bytes: self.asset_bytes.clone(),
            suggested_ext: self.asset_ext.clone(),
        }
    }
}

impl GenProvider for FixtureGenProvider {
    fn id(&self) -> ProviderId {
        ProviderId(FIXTURE_PROVIDER_ID.to_string())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, GenError> {
        Ok(self.models.clone())
    }

    async fn submit(&self, req: GenRequest) -> Result<SubmitOutcome, GenError> {
        // Counted at ENTRY, before any branch, so a caller that reaches this
        // function at all is recorded no matter what it gets back. The request
        // itself is captured on the same terms (Phase 56 plan 05) — what
        // crossed the boundary, not what a test hoped crossed it.
        self.submit_calls.fetch_add(1, Ordering::SeqCst);
        *self
            .last_request
            .lock()
            .expect("fixture last_request mutex poisoned") = Some(req);
        match self.mode {
            FixtureMode::Sync => Ok(SubmitOutcome::Ready(vec![self.asset()])),
            FixtureMode::Async { ready_after_n_polls } => {
                let id = JobId::mint();
                // Each submit mints its OWN countdown Arc, so two concurrent
                // jobs from ONE provider never share poll state.
                Ok(SubmitOutcome::Pending(JobHandle {
                    provider_job_ref: format!("fixture-job-{}", id.0),
                    id,
                    polls_remaining: Some(Arc::new(AtomicU32::new(ready_after_n_polls))),
                }))
            }
        }
    }

    async fn poll(&self, job: &JobHandle) -> Result<JobStatus, GenError> {
        let Some(counter) = job.polls_remaining.as_ref() else {
            // A handle with no countdown was not minted by this provider — a
            // caller error, not a provider failure.
            return Err(GenError::InvalidRequest(format!(
                "job '{}' was not submitted to the fixture provider (no scripted countdown)",
                job.id
            )));
        };
        // Saturating decrement. `prev == 0` means every scripted pending poll
        // has been consumed, so THIS poll is the terminal one. Purely
        // arithmetic: no clock, no randomness.
        let prev = counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .unwrap_or(0);
        if prev == 0 {
            Ok(JobStatus::Ready(vec![self.asset()]))
        } else {
            Ok(JobStatus::Pending)
        }
    }

    async fn cancel(&self, _job: &JobHandle) -> Result<(), GenError> {
        // Nothing remote to abandon — the caller stops polling regardless.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Read a REAL committed fixture. `CARGO_MANIFEST_DIR` here is
    /// `crates/agent-gen`, hence `../../test-media` — two levels up to the repo
    /// root.
    fn fixture_bytes(name: &str) -> Vec<u8> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-media")
            .join(name);
        std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
    }

    /// Drive an `async fn` from a plain `#[test]` without a runtime dependency
    /// — the `pollster` precedent from `agent-llm`.
    fn run<T>(fut: impl std::future::Future<Output = T>) -> T {
        pollster::block_on(fut)
    }

    fn png_bytes() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3]
    }

    fn request(model: &str) -> GenRequest {
        GenRequest {
            provider: ProviderId(FIXTURE_PROVIDER_ID.to_string()),
            model_id: model.to_string(),
            // Derived from the fixture id rather than hardcoded, so a
            // `fixture-video` case cannot silently claim to be an image.
            modality: if model.contains("video") {
                crate::provider::RequestModality::Video
            } else {
                crate::provider::RequestModality::Image
            },
            prompt: "a red square".to_string(),
            background: crate::provider::BackgroundMode::Auto,
            reference_image: None,
            destination_image: None,
            // Phase 56 (GEN-11): the v2v slots. `None` keeps this helper on the
            // pre-56 path byte-for-byte — `source_video` is the endpoint selector.
            source_video: None,
            reference_images: None,
        }
    }

    /// GEN-05's degenerate ZERO-poll path: a sync provider is already terminal
    /// at submit, and its bytes survive byte-identically.
    #[test]
    fn fixture_sync_still_is_ready_at_submit_with_identical_bytes() {
        let input = png_bytes();
        let provider = FixtureGenProvider::sync_still(input.clone(), "png");

        let outcome = run(provider.submit(request("fixture-image"))).expect("submit succeeds");
        let SubmitOutcome::Ready(assets) = outcome else {
            panic!("a sync fixture provider returns Ready at submit (zero-poll)");
        };
        assert_eq!(assets.len(), 1, "one asset per sync generation");
        assert_eq!(
            assets[0].bytes, input,
            "the sync path carries the caller's bytes BYTE-IDENTICALLY"
        );
        assert_eq!(assets[0].suggested_ext, "png");
    }

    /// The scripted async path: exactly N `Pending` polls, then `Ready`.
    #[test]
    fn fixture_async_clip_returns_pending_for_the_scripted_count_then_ready() {
        let input = png_bytes();
        let provider = FixtureGenProvider::async_clip(input.clone(), "mp4", 2);

        let outcome = run(provider.submit(request("fixture-video"))).expect("submit succeeds");
        let SubmitOutcome::Pending(handle) = outcome else {
            panic!("an async fixture provider returns Pending with a pollable handle");
        };

        assert_eq!(
            run(provider.poll(&handle)).expect("poll 1"),
            JobStatus::Pending,
            "poll 1 of 2 scripted pending polls"
        );
        assert_eq!(
            run(provider.poll(&handle)).expect("poll 2"),
            JobStatus::Pending,
            "poll 2 of 2 scripted pending polls"
        );
        let JobStatus::Ready(assets) = run(provider.poll(&handle)).expect("poll 3") else {
            panic!("the 3rd poll is terminal Ready");
        };
        assert_eq!(assets[0].bytes, input, "Ready carries the real bytes");
        assert_eq!(assets[0].suggested_ext, "mp4");

        // Saturates: extra polls keep answering Ready, never wrapping around.
        assert!(matches!(
            run(provider.poll(&handle)).expect("poll 4"),
            JobStatus::Ready(_)
        ));
    }

    /// `ready_after_n_polls == 0` still exercises the poll loop once.
    #[test]
    fn fixture_async_clip_with_zero_countdown_is_ready_on_the_first_poll() {
        let provider = FixtureGenProvider::async_clip(png_bytes(), "mp4", 0);
        let SubmitOutcome::Pending(handle) =
            run(provider.submit(request("fixture-video"))).expect("submit")
        else {
            panic!("still the async shape");
        };
        assert!(
            matches!(run(provider.poll(&handle)).expect("poll 1"), JobStatus::Ready(_)),
            "a zero countdown is Ready on the FIRST poll (the loop still runs once)"
        );
    }

    /// Per-job state rides the HANDLE: a second submit gets its own countdown,
    /// unaffected by how far the first job has been polled.
    #[test]
    fn fixture_countdowns_are_independent_per_submitted_job() {
        let provider = FixtureGenProvider::async_clip(png_bytes(), "mp4", 2);

        let SubmitOutcome::Pending(first) = run(provider.submit(request("fixture-video"))).unwrap()
        else {
            panic!("pending");
        };
        // Drive the first job all the way to terminal.
        assert_eq!(run(provider.poll(&first)).unwrap(), JobStatus::Pending);
        assert_eq!(run(provider.poll(&first)).unwrap(), JobStatus::Pending);
        assert!(matches!(
            run(provider.poll(&first)).unwrap(),
            JobStatus::Ready(_)
        ));

        let SubmitOutcome::Pending(second) = run(provider.submit(request("fixture-video"))).unwrap()
        else {
            panic!("pending");
        };
        assert_ne!(first.id, second.id, "two submits mint distinct job ids");
        assert_eq!(
            run(provider.poll(&second)).unwrap(),
            JobStatus::Pending,
            "the SECOND job starts its countdown fresh — state rides the handle, \
             not the Send+Sync provider"
        );
    }

    /// "Real data only": the provider carries genuinely decodable committed
    /// media, not synthetic pixels — the PNG magic number survives the round
    /// trip and the length matches the file on disk.
    #[test]
    fn fixture_round_trips_real_committed_png_bytes() {
        let real = fixture_bytes("still.png");
        assert_eq!(real.len(), 54_110, "the committed still.png fixture");

        let provider = FixtureGenProvider::sync_still(real.clone(), "png");
        let SubmitOutcome::Ready(assets) = run(provider.submit(request("fixture-image"))).unwrap()
        else {
            panic!("sync Ready");
        };
        assert_eq!(
            &assets[0].bytes[..4],
            &[0x89, b'P', b'N', b'G'],
            "the PNG magic bytes survive submit -> Ready"
        );
        assert_eq!(
            assets[0].bytes, real,
            "every byte of the real fixture round-trips"
        );
        assert_eq!(provider.asset_len(), 54_110);
    }

    /// Determinism: two identical runs produce identical poll sequences. There
    /// is no clock or RNG in any status transition, so this cannot flake.
    #[test]
    fn fixture_poll_sequences_are_deterministic_across_runs() {
        let sequence = || {
            let provider = FixtureGenProvider::async_clip(png_bytes(), "mp4", 3);
            let SubmitOutcome::Pending(handle) =
                run(provider.submit(request("fixture-video"))).unwrap()
            else {
                panic!("pending");
            };
            (0..5)
                .map(|_| run(provider.poll(&handle)).unwrap().wire_state().to_string())
                .collect::<Vec<_>>()
        };
        let a = sequence();
        let b = sequence();
        assert_eq!(a, b, "identical scripts produce identical poll sequences");
        assert_eq!(
            a,
            vec!["pending", "pending", "pending", "ready", "ready"],
            "3 scripted pending polls, then terminal (saturating)"
        );
    }

    /// A handle this provider never minted is a clean `InvalidRequest`, not a
    /// panic and not a silent `Pending` (which would hang a poll loop forever).
    #[test]
    fn fixture_poll_rejects_a_handle_with_no_scripted_countdown() {
        let provider = FixtureGenProvider::async_clip(png_bytes(), "mp4", 1);
        let foreign = JobHandle::new(JobId::mint(), "someone-elses-token".to_string());
        let err = run(provider.poll(&foreign)).expect_err("a foreign handle is rejected");
        assert!(
            matches!(err, GenError::InvalidRequest(_)),
            "an unknown handle is an InvalidRequest: {err}"
        );
    }

    #[test]
    fn fixture_cancel_is_ok_and_needs_no_remote_call() {
        let provider = FixtureGenProvider::async_clip(png_bytes(), "mp4", 5);
        let SubmitOutcome::Pending(handle) = run(provider.submit(request("fixture-video"))).unwrap()
        else {
            panic!("pending");
        };
        assert_eq!(run(provider.cancel(&handle)), Ok(()));
    }

    /// GEN-07 + the setup Wave 4 needs: the catalog is stable, offers BOTH
    /// watermark values (GEN-09's two disclosure paths), and deliberately
    /// includes a model the allow-list will reject (SC-4).
    #[test]
    fn fixture_list_models_returns_canned_catalog() {
        let provider = FixtureGenProvider::sync_still(png_bytes(), "png");
        let models = run(provider.list_models()).expect("list_models succeeds");

        assert_eq!(models.len(), 3, "exactly three canned models");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["fixture-image", "fixture-video", "fixture-nonclean"]);

        assert!(
            models.iter().any(|m| m.carries_provenance_watermark),
            "at least one watermarked model (GEN-09 disclosure path A)"
        );
        assert!(
            models.iter().any(|m| !m.carries_provenance_watermark),
            "at least one un-watermarked model (GEN-09 disclosure path B)"
        );
        assert!(
            models.iter().any(|m| m.id == "fixture-nonclean"),
            "the catalog OFFERS the model Wave 4's allow-list must reject, so \
             SC-4 is proven against a genuinely-offered model"
        );
    }

    #[test]
    fn fixture_provider_id_is_the_fixed_registered_string() {
        let provider = FixtureGenProvider::sync_still(png_bytes(), "png");
        assert_eq!(provider.id().as_str(), "fixture");
        assert_eq!(FIXTURE_PROVIDER_ID, "fixture");
    }

    /// The provider is `Send + Sync` — a hard requirement of `GenProvider` and
    /// of being host-managed state driving spawned poll tasks. This fails to
    /// COMPILE if interior mutability (a `RefCell`) is ever added to the struct.
    #[test]
    fn fixture_provider_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FixtureGenProvider>();
    }
}
