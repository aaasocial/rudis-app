//! `ConcreteGenProvider` — the non-`dyn` enum dispatch that lets ONE concrete
//! type hold either provider.
//!
//! # Why enum dispatch, not boxing
//!
//! `provider.rs:158-162` anticipated this exact moment:
//!
//! > "Phase 32, when a second real provider exists, picks the dispatch
//! > mechanism it actually needs (enum dispatch or boxing) against a real
//! > requirement rather than a speculative one."
//!
//! [`GenProvider`](crate::GenProvider) is deliberately NOT object-safe — its
//! methods are RPITIT (`-> impl Future<..> + Send`), so `Box<dyn GenProvider>`
//! will not compile without `async-trait`-style boxing gymnastics the codebase
//! avoids everywhere else. The host's command layer is monomorphic — the C ABI
//! exports in `crates/ffi` cannot be generic, and the retired Tauri shell's
//! `#[tauri::command]` could not either — so the host-managed provider slot must
//! name exactly ONE concrete type at compile time (see
//! `app_core::generation_host::ManagedGenProvider`). An enum whose
//! `GenProvider` impl delegates each method to
//! the active arm is the clean answer: the fixture double keeps working for
//! every existing test, and the real providers ship in production behind the
//! same managed slot.
//!
//! Phases 32/33/34 grew this enum to four arms — `OpenAi`, `Veo`, `ElevenLabs`
//! beside `Fixture` — one per direct provider. **Phase 42.1 shrinks it back.**
//! The `OpenAi` and `Veo` arms are DELETED with their providers (image and video
//! both route through `Runway` now), leaving three:
//!
//! - `Fixture` — the always-compiled, zero-network test double;
//! - `ElevenLabs` — audio TTS, synchronous (it inherits the 250ms
//!   `poll_interval` DEFAULT, but its delegation arm must still exist by hand
//!   for the match to compile exhaustively);
//! - `Runway` — image AND video, genuinely async, with its own >=5s jittered
//!   poll floor.
//!
//! The one non-obvious rule any future arm must follow: `poll_interval` is a
//! DEFAULTED trait method, and a defaulted method is NOT auto-forwarded by an
//! enum's own trait impl. A missing arm there silently pins that provider to
//! 250ms — which, on a paid async job, is a 20x-too-fast poll of someone's
//! billed endpoint. Delegate it by hand; never add a wildcard arm.

use crate::job::JobHandle;
use crate::provider::{
    GenError, GenProvider, GenRequest, JobStatus, ModelInfo, ProviderId, SubmitOutcome,
};

/// One concrete type nameable by the host's monomorphic command layer,
/// dispatching to
/// the always-compiled test double, the real Runway provider (image + video),
/// or the real ElevenLabs provider (audio).
pub enum ConcreteGenProvider {
    Fixture(crate::FixtureGenProvider),
    /// Phase 34: ElevenLabs TTS — the AUDIO provider, synchronous (zero-poll).
    ElevenLabs(crate::ElevenLabsProvider),
    /// Phase 42.1: Runway — the single provider serving BOTH image and video
    /// generation, and the one the retired OpenAI/Veo arms folded into.
    Runway(crate::RunwayProvider),
}

impl GenProvider for ConcreteGenProvider {
    fn id(&self) -> ProviderId {
        match self {
            Self::Fixture(p) => p.id(),
            Self::ElevenLabs(p) => p.id(),
            Self::Runway(p) => p.id(),
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, GenError> {
        match self {
            Self::Fixture(p) => p.list_models().await,
            Self::ElevenLabs(p) => p.list_models().await,
            Self::Runway(p) => p.list_models().await,
        }
    }

    async fn submit(&self, req: GenRequest) -> Result<SubmitOutcome, GenError> {
        match self {
            Self::Fixture(p) => p.submit(req).await,
            Self::ElevenLabs(p) => p.submit(req).await,
            Self::Runway(p) => p.submit(req).await,
        }
    }

    async fn poll(&self, job: &JobHandle) -> Result<JobStatus, GenError> {
        match self {
            Self::Fixture(p) => p.poll(job).await,
            Self::ElevenLabs(p) => p.poll(job).await,
            Self::Runway(p) => p.poll(job).await,
        }
    }

    async fn cancel(&self, job: &JobHandle) -> Result<(), GenError> {
        match self {
            Self::Fixture(p) => p.cancel(job).await,
            Self::ElevenLabs(p) => p.cancel(job).await,
            Self::Runway(p) => p.cancel(job).await,
        }
    }

    /// MUST be explicitly delegated: `poll_interval` is a DEFAULTED trait method,
    /// and a defaulted method is NOT auto-forwarded by an enum's own trait impl.
    /// Omitting an arm would silently pin that variant to the 250ms default —
    /// Runway would poll a PAID job ~20× too fast. The dispatch enum therefore
    /// forwards it to the active arm exactly like the five async methods above.
    fn poll_interval(&self) -> std::time::Duration {
        match self {
            Self::Fixture(p) => p.poll_interval(),
            // ElevenLabs inherits the 250ms DEFAULT (synchronous — the loop is
            // never entered), but the arm must exist by hand: a defaulted trait
            // method is not auto-forwarded, and a wildcard arm would silently
            // mask a future override.
            Self::ElevenLabs(p) => p.poll_interval(),
            // Runway overrides with its documented >=5s JITTERED floor. Without
            // this arm the enum would silently poll a paid job 20x too fast.
            Self::Runway(p) => p.poll_interval(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ElevenLabsProvider, FixtureGenProvider};

    fn run<T>(fut: impl std::future::Future<Output = T>) -> T {
        pollster::block_on(fut)
    }

    fn png_bytes() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3]
    }

    fn request(provider: &str, model: &str) -> GenRequest {
        GenRequest {
            provider: ProviderId(provider.to_string()),
            model_id: model.to_string(),
            // The dispatch fixtures are still-image ids; the enum arms forward
            // the whole request untouched, so modality is inert here.
            modality: crate::provider::RequestModality::Image,
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

    /// The Fixture arm delegates fully: `id` and a real sync `submit` that
    /// carries the same bytes the bare fixture returns.
    #[test]
    fn concrete_provider_dispatches_to_the_fixture_variant() {
        let input = png_bytes();
        let concrete =
            ConcreteGenProvider::Fixture(FixtureGenProvider::sync_still(input.clone(), "png"));

        assert_eq!(concrete.id().as_str(), "fixture");

        let SubmitOutcome::Ready(assets) =
            run(concrete.submit(request("fixture", "fixture-image"))).expect("submit succeeds")
        else {
            panic!("the fixture sync path returns Ready through the enum");
        };
        assert_eq!(
            assets[0].bytes, input,
            "the Fixture arm passes the bytes through byte-identically"
        );
    }

    /// The ElevenLabs arm delegates fully: `id` and the 1-row audio catalog
    /// (proving real delegation, not a stub arm).
    #[test]
    fn concrete_provider_dispatches_to_the_elevenlabs_variant() {
        let concrete =
            ConcreteGenProvider::ElevenLabs(ElevenLabsProvider::with_key("k".to_string()));
        assert_eq!(concrete.id().as_str(), "elevenlabs");

        let models = run(concrete.list_models()).expect("list_models delegates");
        assert_eq!(models.len(), 1, "the 1-row ElevenLabs TTS catalog delegates through");
        assert_eq!(models[0].id, crate::ELEVENLABS_TTS_MODEL);
        assert_eq!(models[0].modality, "audio");
    }

    /// The ElevenLabs arm EXISTS for `poll_interval` and returns the inherited
    /// 250ms default (synchronous — the poll loop is never entered). The
    /// delegation is by-hand even though the value equals the default.
    #[test]
    fn concrete_provider_delegates_elevenlabs_poll_interval() {
        let el = ConcreteGenProvider::ElevenLabs(ElevenLabsProvider::with_key("k".to_string()));
        assert_eq!(
            el.poll_interval(),
            crate::DEFAULT_POLL_INTERVAL,
            "the ElevenLabs arm inherits the 250ms default through the enum"
        );
    }

    /// 42.1-02: the Runway arm delegates fully — `id` is the registered
    /// `"runway"` provider id, and `list_models` returns Runway's OWN catalog,
    /// proving a real delegation rather than a stub arm.
    ///
    /// **55.1:** the expected size is derived from `RUNWAY_MODELS` rather than
    /// hardcoded (it was `5`, the count of the since-deleted intent map). This
    /// is a DELEGATION test — the roster's size is Runway's business, not this
    /// enum's, and pinning the number here made a roster edit look like a
    /// dispatch bug.
    #[test]
    fn concrete_provider_dispatches_to_the_runway_variant() {
        let concrete = ConcreteGenProvider::Runway(crate::RunwayProvider::with_key("k".to_string()));
        assert_eq!(concrete.id().as_str(), crate::RUNWAY_PROVIDER_ID);

        let live_roster = crate::RUNWAY_MODELS
            .iter()
            .filter(|(_, _, caps)| !caps.deprecated)
            .count();
        assert!(live_roster > 0, "the roster is non-empty, so this is non-vacuous");
        let models = run(concrete.list_models()).expect("list_models delegates");
        assert_eq!(
            models.len(),
            live_roster,
            "Runway's own catalog delegates through the enum"
        );
        // The ONE variant serving TWO modalities — the property that lets 42.1-02
        // collapse the separate image and video managed slots onto one provider.
        assert!(models.iter().any(|m| m.modality == "video"));
        assert!(models.iter().any(|m| m.modality == "image"));
    }

    /// 42.1-02: the Runway arm's JITTERED >=5s poll floor must reach through the
    /// enum. Without its explicit `poll_interval` arm the enum would silently
    /// inherit the 250ms default and poll a PAID job ~20x too fast — the exact
    /// bug the Veo arm below already pins, now for the provider that replaces it.
    #[test]
    fn concrete_provider_delegates_runway_poll_interval_not_the_default() {
        let runway = ConcreteGenProvider::Runway(crate::RunwayProvider::with_key("k".to_string()));
        let interval = runway.poll_interval();
        assert!(
            interval >= crate::RUNWAY_POLL_INTERVAL_FLOOR,
            "the delegated interval honours Runway's documented 5s floor: {interval:?}"
        );
        assert_ne!(
            interval,
            crate::DEFAULT_POLL_INTERVAL,
            "and is NOT the inherited 250ms default"
        );
    }

    /// The Fixture arm still inherits the historic 250ms default through the
    /// enum — the control case that makes
    /// `concrete_provider_delegates_runway_poll_interval_not_the_default`'s
    /// `assert_ne!` non-vacuous. (This pinned the Veo arm's 10s cadence before
    /// 42.1 deleted that provider; the SHAPE of the bug it guards — a defaulted
    /// trait method silently not forwarded — is unchanged, so the pin MOVED to
    /// Runway rather than being dropped.)
    #[test]
    fn concrete_provider_delegates_poll_interval_not_the_default() {
        let fixture = ConcreteGenProvider::Fixture(FixtureGenProvider::sync_still(
            vec![1, 2, 3],
            "png",
        ));
        assert_eq!(
            fixture.poll_interval(),
            crate::DEFAULT_POLL_INTERVAL,
            "the Fixture arm still inherits the historic 250ms default"
        );

        // ... and the Runway arm genuinely differs from it, so "delegated" is a
        // claim with observable content rather than a coincidence.
        let runway = ConcreteGenProvider::Runway(crate::RunwayProvider::with_key("k".to_string()));
        assert_ne!(runway.poll_interval(), fixture.poll_interval());
    }
}
