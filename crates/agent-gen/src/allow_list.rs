//! The GEN-08 clean-model allow-list gate.
//!
//! # The two-layer rule this enforces
//!
//! GEN-08 requires **both** (a) provider-ToS clearance — the provider's terms
//! permit a closed-source paid product using BYO keys — **and** (b) a per-model
//! weight/output-license check. A per-provider check alone is NOT sufficient:
//! **a hosted API does NOT launder a non-commercial model-weight license.**
//! Upscaling served through SUPIR/CodeFormer weights, or generation through
//! FLUX.1-[dev], is non-commercial and taints the output *even through a paid
//! hosted endpoint*. That is why this list is **model-granular AND
//! provider-granular**: a clean model id offered by an unvetted provider is
//! still rejected, and a vetted provider's non-clean model is still rejected.
//!
//! # Phase 55.1 (owner decision D-01): the granularity is now per-provider
//!
//! GEN-08 is **model-granular for ElevenLabs, and provider-granular —
//! deliberately UNGATED — for Runway.** See [`UNGATED_PROVIDERS`] for the
//! recorded decision.
//!
//! For every provider NOT named there, everything above holds unchanged, an
//! unknown id defaults to REJECTED, and so does the product rule it implies:
//! **the agent cannot select outside this list.** Model choice is not a
//! capability we hand to a prompt-steerable caller and then hope it behaves —
//! the list is enforced server-side, here, and never expressed as a tool-schema
//! enum a model could be talked out of.
//!
//! For Runway that product rule no longer holds, on purpose: the agent — or the
//! user, naming a model in chat — reaches any id Runway's own server-side model
//! enum accepts. The weight/output-license warning above is **not retracted** by
//! that. It is the residual risk the owner accepted knowingly, and enforcing it
//! became a human/process obligation (the PROVENANCE Entry 17 amendment)
//! instead of a compiler-enforced one.
//!
//! # [`submit_checked`] is the ONE gate every submit path goes through
//!
//! Defense-in-depth over any provider self-check: a provider implementation may
//! also validate, but the gate does not *depend* on it — a newly-merged
//! provider that forgets to self-check is still governed. The check runs BEFORE
//! `provider.submit(..)` is ever called, mirroring `run_generate_image`'s
//! "resolve() BEFORE any GPU/file work" precedent (T-24-11): a rejected request
//! performs no network call, registers no job, emits no event, and writes no
//! file. `crates/agent-gen`'s tests prove that by CALL COUNT, not merely by an
//! error being returned.
//!
//! **The D-01 ungating did NOT fork a second submit path.** Runway still goes
//! through `submit_checked` like everything else; the gate simply answers
//! "allowed" for it by the policy recorded in [`UNGATED_PROVIDERS`] rather than
//! by a row. There is still exactly one place to read to know what is
//! reachable, and exactly one place to change to make it closed again.

use crate::provider::{GenError, GenProvider, GenRequest, ProviderId, SubmitOutcome};

/// **Providers whose model roster is deliberately UNGATED** (Phase 55.1, owner
/// decision D-01, locked 2026-08-01 in `ROADMAP.md` § Phase 55.1).
///
/// Every model id the provider's own API accepts is reachable, agent- or
/// user-chosen, with no local roster validation of any kind. Runway's
/// server-side model enum is the sole remaining existence check (an unknown id
/// comes back as its own `{error, docUrl, issues}` 400, which Rudis surfaces
/// verbatim rather than re-authoring), and the per-model weight/output-license
/// review GEN-08 required is now a **human/process obligation** — recorded in
/// the `PROVENANCE.md` Entry 17 amendment — no longer code-enforced.
///
/// [`submit_checked`] remains the ONE gate every submit path goes through: for
/// these providers it answers "allowed" by this recorded policy rather than by
/// a row, which is a narrower change than letting one provider skip the gate.
/// One consequence worth stating plainly — closing Runway again is a one-line
/// edit here, not an archaeology exercise across call sites.
///
/// **ElevenLabs is deliberately NOT here (D-03).** Its single signed-off TTS
/// row, and the fail-closed rejection of `sound-generation` / `music` / the
/// un-reviewed generations, are untouched by this phase. Neither are the
/// retired `openai` / `google` ids, which `tests/retired_providers_stay_rejected.rs`
/// still pins as permanently rejected.
///
/// Adding a provider here is the same class of act as adding a row to
/// [`production_allow_list`], in the opposite direction — an OWNER decision
/// with a recorded reason, never a convenience.
pub const UNGATED_PROVIDERS: &[&str] = &["runway"];

/// One vetted `(provider, model)` pair — the unit of GEN-08 clearance.
///
/// Both halves matter: clearance is granted to a model *as served by a specific
/// provider*, because the ToS layer and the weight-license layer are properties
/// of the provider and the model respectively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowListEntry {
    pub provider: ProviderId,
    pub model_id: String,
}

impl AllowListEntry {
    /// Convenience constructor for the literal `(provider, model)` pairs the
    /// vetted sets are written as.
    pub fn new(provider: &str, model_id: &str) -> Self {
        Self {
            provider: ProviderId(provider.to_string()),
            model_id: model_id.to_string(),
        }
    }
}

/// The set of `(provider, model)` pairs that have cleared the GEN-08 gate.
///
/// Deliberately a closed, explicitly-enumerated list rather than a pattern or a
/// deny-list: an unknown model must default to REJECTED. A deny-list would mean
/// every model a provider adds after our last review is silently permitted —
/// exactly the licensing exposure GEN-08 exists to prevent.
///
/// That reasoning applies to every provider EXCEPT those in
/// [`UNGATED_PROVIDERS`], for which this list is not consulted at all and the
/// exposure above is knowingly accepted (D-01). The exception is a named
/// policy in one place, not a hole in the data structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowList(Vec<AllowListEntry>);

impl AllowList {
    pub fn new(entries: Vec<AllowListEntry>) -> Self {
        Self(entries)
    }

    /// Is this exact `(provider, model)` pair vetted?
    ///
    /// Exact match on both halves. No prefix matching, no case folding, no
    /// normalization — an approximate match here would be an approximate
    /// licensing claim.
    ///
    /// **One exception, checked FIRST:** a provider named in
    /// [`UNGATED_PROVIDERS`] answers `true` for every model id, including ids
    /// with no row at all. That short-circuit IS the D-01 decision in code —
    /// without it, deleting Runway's rows would reject every Runway model,
    /// the exact inversion of the decision.
    pub fn is_allowed(&self, provider: &ProviderId, model_id: &str) -> bool {
        if UNGATED_PROVIDERS.contains(&provider.as_str()) {
            return true;
        }
        self.0
            .iter()
            .any(|e| &e.provider == provider && e.model_id == model_id)
    }

    /// The vetted entries (diagnostics and tests).
    pub fn entries(&self) -> &[AllowListEntry] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Phase 31's clean set: the fixture provider's two vetted models.
///
/// `fixture-nonclean` is deliberately EXCLUDED even though
/// [`FixtureGenProvider`](crate::FixtureGenProvider) genuinely offers it in
/// `list_models` — it is the standing SC-4 rejection case, so "a non-clean model
/// cannot reach submit" is proven against a model the provider really
/// advertises rather than an invented strawman.
///
/// **Extending this list is the GEN-08 human legal gate.** Phase 32+ adds a row
/// only after a human has confirmed that provider's live ToS AND that model's
/// weight/output license. No code path may append to it at runtime.
pub fn phase31_allow_list() -> AllowList {
    AllowList::new(vec![
        AllowListEntry::new(crate::FIXTURE_PROVIDER_ID, "fixture-image"),
        AllowListEntry::new(crate::FIXTURE_PROVIDER_ID, "fixture-video"),
        // ("fixture", "fixture-nonclean") is NOT here, on purpose. See the doc.
    ])
}

/// The PRODUCTION clean set: the phase-31 fixture entries PLUS the one
/// human-signed-off ElevenLabs TTS row. **No Runway row — see below.**
///
/// # The five `("runway", …)` rows are GONE (Phase 55.1, D-01)
///
/// Phase 42.1 added five rows here — `gen4_turbo`, `veo3.1_fast`, `gen4.5`,
/// `gen4_image`, `seedance2` — derived from the intent table under a dated
/// human GEN-08 sign-off recorded in
/// `.planning/phases/42.1-runway-single-provider-generation-with-intent-based-model-se/GEN-08-SIGNOFF.md`.
/// Everything else Runway offers (`veo3`, `veo3.1`, `seedance2_mini`, `aleph2`,
/// `gen4_image_turbo`, both retired ids) was REJECTED, on the rule that Runway
/// offering a model is not Rudis clearing it.
///
/// **The owner retired that rule for Runway on 2026-08-01 (D-01).** Rows are no
/// longer the mechanism, so keeping five of them would be theatre: the ungating
/// lives in [`UNGATED_PROVIDERS`], and this function simply no longer mentions
/// Runway. The sign-off document stays on record as history — it records what
/// WAS reviewed (those five models genuinely were), which is exactly the
/// information the amendment needs in order to say what is not.
///
/// ⚠️ **The sign-off's binding condition SURVIVES the rows.** Runway retains a
/// license to use inputs AND outputs for model training on every non-Enterprise
/// tier. That is a user-facing fact, not an internal note, and it is disclosed
/// in-app in the same manner the provenance watermark is — see
/// `handle_generate_ai_image` / `handle_generate_ai_video` in
/// `crates/app-core/src/generation_bridge.rs` (re-exported through
/// `agent_turn`), where the disclosure is appended to the tool result beside
/// the watermark note. Removing the clearance rows did not remove that
/// obligation; if anything an open roster widens what it covers.
///
/// # The retired OpenAI and Google rows (Phase 42.1)
///
/// This list used to carry one `("openai", "gpt-image-1.5")` row and three
/// `("google", "veo-3.1-*-generate-preview")` rows under their own 2026-07-22
/// sign-offs. Phase 42.1 collapsed image and video generation onto a single
/// provider and DELETED both providers, so those rows are gone with them — a
/// clearance for a provider the app can no longer reach is not protection, it is
/// an out-of-date claim. Their sign-off documents
/// (`.planning/phases/32-image-generation/GEN-08-SIGNOFF.md`,
/// `.planning/phases/33-video-generation/GEN-08-SIGNOFF.md`) remain on record as
/// history, and `tests/retired_providers_stay_rejected.rs` pins that neither
/// provider's ids can ever be admitted again by accident.
///
/// The one `("elevenlabs", "eleven_multilingual_v2")` row is added under the
/// GEN-08 HUMAN sign-off recorded in
/// `.planning/phases/34-audio-generation/GEN-08-SIGNOFF.md` (project owner, 2026-07-23):
/// paid ElevenLabs users retain all rights in their Output with commercial use
/// and no attribution, the reseller/sublicense restriction is not triggered by
/// BYO-key (the end user is ElevenLabs' own contracting customer), voice CLONING
/// is not exposed by Rudis so the informed-consent obligation is satisfied by
/// construction, and the SynthID/C2PA-style inaudible provenance mark is
/// acknowledged (the basis for the seam's `carries_provenance_watermark: true`).
/// **Scope is TTS-ONLY.** `sound-generation` (SFX) and Music rows are
/// DELIBERATELY absent: SFX may not be sold/used commercially on a standalone
/// basis and Music is not cleared for film/TV/large-studio games without an
/// Enterprise plan, so neither ships until its product guardrail exists. The
/// un-reviewed `eleven_flash_v2_5` / `eleven_v3` generations are likewise absent
/// — an unknown id defaults to REJECTED (34-01's everlasting rejection pin). The
/// id is sourced from [`ELEVENLABS_TTS_MODEL`](crate::ELEVENLABS_TTS_MODEL) —
/// never a retyped string literal.
///
/// The fixture entries are kept so every existing test that resolves the
/// production list still sees them. **Extending this list is ALWAYS a human
/// legal gate** — no code path may append to it at runtime (same standing rule
/// as [`phase31_allow_list`]).
pub fn production_allow_list() -> AllowList {
    let mut entries = phase31_allow_list().entries().to_vec();

    // =====================================================================
    // The five ("runway", …) rows that stood here from 42.1-02 to 55.1-02 were
    // REMOVED by owner decision D-01 (2026-08-01), not lost. They were derived
    // from the intent table + the transition fallback (both of which Plan 06
    // deletes outright — deliberately not NAMED here, so this record does not
    // become a dangling reference) under the dated 2026-07-27
    // GEN-08 sign-off (the project owner), which cleared exactly:
    //   gen4_turbo, veo3.1_fast, gen4.5, gen4_image, seedance2
    // on the findings that Runway claims no ownership of inputs or outputs and
    // permits commercial use on every tier — CONDITIONAL on the training-license
    // caveat being disclosed in-app. That condition is NOT retired with the rows
    // (see this function's doc); only the row-shaped enforcement is.
    //
    // Runway clearance is now provider-level and open: UNGATED_PROVIDERS.
    // Adding rows back here would have NO effect on what Runway admits — the
    // is_allowed short-circuit runs first — so a future reviewer who wants the
    // gate back must remove "runway" from that const, not add rows here.
    // =====================================================================

    // The ONE ElevenLabs TTS row, gated by the human GEN-08 review recorded in
    // .planning/phases/34-audio-generation/GEN-08-SIGNOFF.md. TTS ONLY — no
    // SFX/Music row ships without its own review AND product guardrail.
    entries.push(AllowListEntry::new(
        crate::ELEVENLABS_PROVIDER_ID,
        crate::ELEVENLABS_TTS_MODEL,
    ));
    AllowList::new(entries)
}

/// Submit a generation request **through** the GEN-08 gate.
///
/// Every submit path in the app calls this instead of `provider.submit(..)`
/// directly. The allow-list check happens first and returns early, so a
/// non-vetted model never reaches the provider at all — no network call, and
/// (at the app layer) no job row, no event, no file.
pub async fn submit_checked<P: GenProvider>(
    provider: &P,
    allow_list: &AllowList,
    req: GenRequest,
) -> Result<SubmitOutcome, GenError> {
    if !allow_list.is_allowed(&provider.id(), &req.model_id) {
        // Rejected BEFORE any submit / network call.
        return Err(GenError::ModelNotAllowed(req.model_id));
    }
    provider.submit(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobHandle;
    use crate::provider::{AssetRef, JobStatus, ModelInfo};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn run<T>(fut: impl std::future::Future<Output = T>) -> T {
        pollster::block_on(fut)
    }

    /// A `GenProvider` that COUNTS how many times `submit` was entered.
    ///
    /// The whole SC-4 claim is "rejected *before* submit", which an error return
    /// alone cannot distinguish from "submitted, then the result was thrown
    /// away". Only a call count can.
    struct CountingProvider {
        id: ProviderId,
        submit_calls: AtomicU32,
    }

    impl CountingProvider {
        fn new(id: &str) -> Self {
            Self {
                id: ProviderId(id.to_string()),
                submit_calls: AtomicU32::new(0),
            }
        }

        fn calls(&self) -> u32 {
            self.submit_calls.load(Ordering::SeqCst)
        }
    }

    impl GenProvider for CountingProvider {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, GenError> {
            Ok(Vec::new())
        }

        async fn submit(&self, _req: GenRequest) -> Result<SubmitOutcome, GenError> {
            self.submit_calls.fetch_add(1, Ordering::SeqCst);
            Ok(SubmitOutcome::Ready(vec![AssetRef {
                bytes: vec![0x89, b'P', b'N', b'G'],
                suggested_ext: "png".to_string(),
            }]))
        }

        async fn poll(&self, _job: &JobHandle) -> Result<JobStatus, GenError> {
            Ok(JobStatus::Pending)
        }

        async fn cancel(&self, _job: &JobHandle) -> Result<(), GenError> {
            Ok(())
        }
    }

    fn request(provider: &str, model: &str) -> GenRequest {
        GenRequest {
            provider: ProviderId(provider.to_string()),
            model_id: model.to_string(),
            // These fixtures are all still-image ids (`fixture-image`,
            // `gen4_image`, …); the allow list itself never reads modality.
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

    /// **SC-4's core.** A non-allow-listed model is rejected BEFORE any submit
    /// call — proven by the provider's submit counter still reading ZERO, not
    /// merely by an `Err` coming back.
    #[test]
    fn allow_list_rejects_before_submit() {
        let provider = CountingProvider::new(crate::FIXTURE_PROVIDER_ID);
        let list = phase31_allow_list();

        let err = run(submit_checked(
            &provider,
            &list,
            request(crate::FIXTURE_PROVIDER_ID, "fixture-nonclean"),
        ))
        .expect_err("a non-clean model is rejected");

        assert!(
            matches!(err, GenError::ModelNotAllowed(_)),
            "the licensing gate rejects with ModelNotAllowed: {err}"
        );
        assert!(
            err.to_string().contains("fixture-nonclean"),
            "the rejection NAMES the model: {err}"
        );
        assert_eq!(
            provider.calls(),
            0,
            "SC-4: the provider's submit was NEVER invoked — the rejection \
             happens BEFORE the call, not after it"
        );
    }

    /// The gate is not a blanket denier: a vetted model passes through exactly
    /// once, and the provider's outcome is returned unchanged.
    #[test]
    fn allow_listed_model_reaches_submit_exactly_once_and_passes_the_outcome_through() {
        let provider = CountingProvider::new(crate::FIXTURE_PROVIDER_ID);
        let list = phase31_allow_list();

        let outcome = run(submit_checked(
            &provider,
            &list,
            request(crate::FIXTURE_PROVIDER_ID, "fixture-video"),
        ))
        .expect("a vetted model is admitted");

        assert_eq!(provider.calls(), 1, "submit ran exactly once");
        let SubmitOutcome::Ready(assets) = outcome else {
            panic!("the gate passes the provider's outcome through UNCHANGED");
        };
        assert_eq!(assets[0].suggested_ext, "png");
        assert_eq!(&assets[0].bytes[..4], &[0x89, b'P', b'N', b'G']);
    }

    /// The two-layer GEN-08 rule: clearance is `(provider, model)`, not either
    /// alone. A clean model id served by an UNVETTED provider is still rejected
    /// — that provider's ToS has not been reviewed, and a hosted endpoint does
    /// not launder anything.
    #[test]
    fn is_allowed_is_both_model_granular_and_provider_granular() {
        let list = phase31_allow_list();
        let fixture = ProviderId(crate::FIXTURE_PROVIDER_ID.to_string());
        let other = ProviderId("other-provider".to_string());

        assert!(
            list.is_allowed(&fixture, "fixture-video"),
            "a vetted pair is allowed"
        );
        assert!(
            !list.is_allowed(&fixture, "fixture-nonclean"),
            "model-granular: a vetted PROVIDER does not clear its non-clean model"
        );
        assert!(
            !list.is_allowed(&other, "fixture-video"),
            "provider-granular: a clean MODEL ID under an unvetted provider is \
             still rejected (a hosted API does not launder a weight license)"
        );
        assert!(
            !list.is_allowed(&fixture, "fixture-video-v2"),
            "unknown models default to REJECTED — this is an allow-list, never \
             a deny-list"
        );
    }

    /// The phase-31 clean set is exactly the two vetted fixture models.
    #[test]
    fn phase31_allow_list_is_the_two_vetted_fixture_models() {
        let list = phase31_allow_list();
        assert_eq!(list.len(), 2, "exactly two vetted pairs");

        let pairs: Vec<(String, String)> = list
            .entries()
            .iter()
            .map(|e| (e.provider.as_str().to_string(), e.model_id.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("fixture".to_string(), "fixture-image".to_string()),
                ("fixture".to_string(), "fixture-video".to_string()),
            ]
        );
        assert!(
            !pairs.iter().any(|(_, m)| m == "fixture-nonclean"),
            "fixture-nonclean is OFFERED by the provider but NOT vetted — the \
             standing SC-4 rejection case"
        );
    }

    /// **D-01 (Phase 55.1): the Runway roster is UNGATED, and this is the
    /// anti-regression pin for "silently rejected because it now has ZERO
    /// rows".**
    ///
    /// Deleting the five Runway rows without teaching the gate about the
    /// decision would invert D-01 exactly: an allow-list with no Runway row
    /// rejects *every* Runway model. So the claim worth pinning is the
    /// conjunction — the provider holds no rows AND every id clears anyway.
    #[test]
    fn runway_is_ungated_every_model_id_clears_the_gate() {
        let list = production_allow_list();
        let runway = ProviderId(crate::RUNWAY_PROVIDER_ID.to_string());

        for model in [
            // Was one of the five signed-off rows.
            "gen4_turbo",
            // In RUNWAY_MODELS, and REJECTED BY NAME until this phase.
            "aleph2",
            // A real Runway model that has never appeared in any Rudis table.
            "kling3.0_pro",
            // Nonsense. There is no local gate left to catch it; Runway's own
            // server-side model enum is the existence check (a clean 400).
            "totally-made-up-model",
        ] {
            assert!(
                list.is_allowed(&runway, model),
                "(runway, {model}) clears the gate by the recorded D-01 ungating"
            );
        }

        assert_eq!(
            list.entries().iter().filter(|e| e.provider == runway).count(),
            0,
            "...and it clears with ZERO runway rows present — by recorded \
             POLICY, not by a row nobody reviewed"
        );

        // Non-vacuity: this is not "the list admits everything now". The very
        // same list still refuses a non-clean model under a gated provider.
        let fixture = ProviderId(crate::FIXTURE_PROVIDER_ID.to_string());
        assert!(
            !list.is_allowed(&fixture, "fixture-nonclean"),
            "the ungating is Runway-ONLY — the standing SC-4 rejection survives"
        );
    }

    /// The ungating reaches the far side of the ONE gate, not just `is_allowed`.
    ///
    /// `submit_checked` is still the only submit path (no second fork was
    /// created for Runway), so the property that matters is that the provider's
    /// `submit` really RUNS for an off-roster id — proven by call count, the
    /// same way `allow_list_rejects_before_submit` proves the inverse.
    #[test]
    fn runway_ungating_reaches_submit_not_just_is_allowed() {
        assert!(
            crate::model_caps("kling3.0_pro").is_none(),
            "precondition: 'kling3.0_pro' really is off-roster — this is not a \
             known-model test in disguise"
        );

        let provider = CountingProvider::new(crate::RUNWAY_PROVIDER_ID);
        let list = production_allow_list();

        let outcome = run(submit_checked(
            &provider,
            &list,
            request(crate::RUNWAY_PROVIDER_ID, "kling3.0_pro"),
        ))
        .expect("an off-roster runway id is admitted, not ModelNotAllowed");

        assert_eq!(
            provider.calls(),
            1,
            "the provider's submit genuinely ran — the ungating is not an \
             is_allowed-only fiction"
        );
        assert!(
            matches!(outcome, SubmitOutcome::Ready(_)),
            "and the gate passes the provider's outcome through unchanged"
        );
    }

    /// **D-03: the ungating is Runway-ONLY.** ElevenLabs and the fixture
    /// provider keep exact-row gating, byte-unchanged.
    #[test]
    fn elevenlabs_and_fixture_gating_are_unchanged() {
        let list = production_allow_list();
        let elevenlabs = ProviderId(crate::ELEVENLABS_PROVIDER_ID.to_string());
        let fixture = ProviderId(crate::FIXTURE_PROVIDER_ID.to_string());

        assert!(
            list.is_allowed(&elevenlabs, crate::ELEVENLABS_TTS_MODEL),
            "the one signed-off TTS pair still admits"
        );
        assert!(
            !list.is_allowed(&elevenlabs, "eleven_v3"),
            "an un-reviewed elevenlabs model is STILL rejected — D-03"
        );
        assert!(
            !list.is_allowed(&fixture, "fixture-nonclean"),
            "the standing SC-4 rejection case is untouched"
        );
        assert!(
            list.is_allowed(&fixture, "fixture-image"),
            "and the vetted fixture rows still admit"
        );
    }

    // =====================================================================
    // TWO TESTS WERE DELETED HERE by 55.1-02 (owner decision D-01). Recorded
    // rather than dropped, because both were protecting something real:
    //
    // 1. `production_allow_list_clears_exactly_the_signed_off_runway_models`
    //    pinned the five ids as LITERALS and asserted the Runway row count was
    //    EXACTLY five, so that repointing an intent at, say, `seedance2_mini`
    //    could not silently inherit a clearance no human granted. It also
    //    asserted `veo3`, `veo3.1`, `seedance2_mini`, `aleph2` and
    //    `gen4_image_turbo` were REJECTED. Every one of those claims is now
    //    false by decision: there are no rows, and all five ids are reachable.
    //    Its surviving half — "the Runway provider holds zero rows" — moved
    //    into `runway_is_ungated_every_model_id_clears_the_gate`, where it is
    //    asserted TOGETHER with "and every id clears anyway", which is the
    //    conjunction that makes the removal correct instead of an inversion.
    //    WHAT IS GENUINELY LOST: the drift alarm. A future Runway model with a
    //    non-commercial weight license is now a human/process catch only
    //    (PROVENANCE Entry 17 amendment), which is precisely the accepted risk
    //    D-01 records — not an oversight this test could have kept covering.
    //
    // 2. `every_intent_reachable_model_is_cleared` asserted every destination
    //    in the intent table was cleared, guarding against "a live probe moves
    //    one intent row and every default generation is gate-rejected at
    //    runtime".
    //    That runtime rejection can no longer happen for Runway at all, so the
    //    test could only ever assert `true == true`. A vacuously-green
    //    licensing test is worse than no test: it reads like coverage. (The
    //    intent table itself is Plan 06's to delete; nothing here touches it.)
    // =====================================================================

    /// The ungating is EXACTLY one provider — the pin that stops
    /// [`UNGATED_PROVIDERS`] growing a second entry without a decision.
    ///
    /// D-03 makes ElevenLabs' absence a hard requirement, and the retired
    /// `openai`/`google` ids must stay permanently rejected, so this asserts on
    /// content rather than only on length.
    #[test]
    fn the_ungated_set_is_exactly_runway() {
        assert_eq!(
            UNGATED_PROVIDERS,
            &[crate::RUNWAY_PROVIDER_ID],
            "exactly one provider is ungated; adding another is an OWNER \
             decision with a recorded reason, never a convenience"
        );
        assert!(
            !UNGATED_PROVIDERS.contains(&crate::ELEVENLABS_PROVIDER_ID),
            "D-03: ElevenLabs keeps its row gate"
        );
        for gated in ["openai", "google", crate::FIXTURE_PROVIDER_ID] {
            assert!(
                !UNGATED_PROVIDERS.contains(&gated),
                "'{gated}' is NOT ungated"
            );
        }
    }

    /// **Phase 42.1: the retired providers' clearances are GONE, not dormant.**
    ///
    /// This test is the rewritten survivor of
    /// `production_allow_list_adds_exactly_the_openai_row` (32-02) and
    /// `production_allow_list_adds_exactly_the_three_veo_rows` (33-02). Their
    /// original claim — "exactly these signed-off pairs are vetted" — inverted
    /// when 42.1 deleted both providers: the claim worth pinning now is that
    /// NOTHING under either provider id is admitted, so a copy-pasted id or a
    /// half-reverted migration cannot quietly re-open a clearance for a provider
    /// the app can no longer even construct.
    #[test]
    fn production_allow_list_admits_nothing_under_the_retired_providers() {
        let list = production_allow_list();
        let openai = ProviderId("openai".to_string());
        let google = ProviderId("google".to_string());

        for retired in [&openai, &google] {
            assert_eq!(
                list.entries().iter().filter(|e| &e.provider == retired).count(),
                0,
                "no row survives for the retired provider '{}'",
                retired.as_str()
            );
        }

        // The exact ids that WERE vetted, each now rejected by name — a pin with
        // real content rather than a count that could pass vacuously.
        for id in ["gpt-image-1.5", "gpt-image-1", "dall-e-3"] {
            assert!(!list.is_allowed(&openai, id), "(openai, {id}) is rejected");
        }
        for id in [
            "veo-3.1-generate-preview",
            "veo-3.1-fast-generate-preview",
            "veo-3.1-lite-generate-preview",
        ] {
            assert!(!list.is_allowed(&google, id), "(google, {id}) is rejected");
        }

        // ... and the phase-31 meanings are untouched by the removal.
        let fixture = ProviderId(crate::FIXTURE_PROVIDER_ID.to_string());
        assert!(list.is_allowed(&fixture, "fixture-image"));
        assert!(list.is_allowed(&fixture, "fixture-video"));
        assert!(
            !list.is_allowed(&fixture, "fixture-nonclean"),
            "the standing SC-4 rejection case survives unchanged"
        );
    }

    /// 34-03: `production_allow_list()` carries EXACTLY ONE elevenlabs row — the
    /// signed-off `(elevenlabs, eleven_multilingual_v2)` TTS pair — sourced from
    /// the `elevenlabs` consts, with flash/v3 and the SFX/Music ids provably
    /// rejected and every prior row intact.
    #[test]
    fn production_allow_list_adds_exactly_the_elevenlabs_tts_row() {
        let list = production_allow_list();
        // Pinned-value UPDATE, third step: 7 → 3 when the openai + three veo
        // rows were DELETED with their providers, 3 → 8 when the five
        // human-signed-off runway rows landed (42.1-02), and 8 → 3 again when
        // D-01 removed those five (55.1-02). ONLY this length literal moved in
        // 55.1-02 — every ElevenLabs assertion below is byte-identical, because
        // this test is the D-03 regression guard.
        assert_eq!(
            list.len(),
            3,
            "two fixture + one elevenlabs tts row (runway is ungated, not listed)"
        );

        let elevenlabs = ProviderId(crate::ELEVENLABS_PROVIDER_ID.to_string());
        let fixture = ProviderId(crate::FIXTURE_PROVIDER_ID.to_string());

        assert!(
            list.is_allowed(&elevenlabs, crate::ELEVENLABS_TTS_MODEL),
            "the signed-off (elevenlabs, eleven_multilingual_v2) pair is vetted"
        );

        // TTS-ONLY scope. Neither the un-reviewed TTS generations nor the
        // SFX/Music endpoints ship a row — they stay fail-closed until their own
        // human review AND their product guardrails exist (Pitfall 3).
        assert!(
            !list.is_allowed(&elevenlabs, "eleven_flash_v2_5"),
            "flash was not reviewed — REJECTED"
        );
        assert!(
            !list.is_allowed(&elevenlabs, "eleven_v3"),
            "v3 was not reviewed — REJECTED"
        );
        assert!(
            !list.is_allowed(&elevenlabs, "sound-generation"),
            "SFX has a standalone-commercial-use constraint — deliberately absent"
        );
        assert!(
            !list.is_allowed(&elevenlabs, "music"),
            "Music needs Enterprise for film/TV/large-studio games — absent"
        );

        // Exactly ONE elevenlabs row, no more.
        assert_eq!(
            list.entries()
                .iter()
                .filter(|e| e.provider == elevenlabs)
                .count(),
            1,
            "exactly one elevenlabs row ships under the GEN-08 sign-off"
        );

        // Every phase-31 row survives unchanged.
        assert!(list.is_allowed(&fixture, "fixture-image"));
        assert!(list.is_allowed(&fixture, "fixture-video"));
        assert!(
            !list.is_allowed(&fixture, "fixture-nonclean"),
            "the standing SC-4 rejection case survives unchanged"
        );
    }


    /// **The gate keys on `provider.id()` — what the server KNOWS it is talking
    /// to — never on the caller-supplied `GenRequest::provider`.**
    ///
    /// A request cannot vote on its own clearance: here a request LABELLED with
    /// the vetted `"fixture"` provider is submitted to a provider that is
    /// actually something else, and it is rejected. Trusting `req.provider`
    /// would make the whole allow list bypassable by editing one JSON string
    /// (T-31-17).
    #[test]
    fn the_gate_keys_on_the_real_provider_not_the_requests_self_declared_one() {
        let impostor = CountingProvider::new("unvetted-provider");
        let list = phase31_allow_list();

        // The request claims to be for "fixture" — a genuinely vetted provider.
        let claiming_to_be_fixture = request(crate::FIXTURE_PROVIDER_ID, "fixture-video");
        assert!(
            list.is_allowed(&claiming_to_be_fixture.provider, "fixture-video"),
            "precondition: the pair the request CLAIMS to be is genuinely vetted"
        );

        let err = run(submit_checked(&impostor, &list, claiming_to_be_fixture))
            .expect_err("clearance follows the real provider, not the request's label");
        assert!(matches!(err, GenError::ModelNotAllowed(_)));
        assert_eq!(
            impostor.calls(),
            0,
            "and the impostor's submit was never reached"
        );
    }

    /// An EMPTY allow-list rejects everything, including a model the provider
    /// offers. The gate fails CLOSED — a mis-wired or not-yet-populated list is
    /// never a silent pass-through.
    #[test]
    fn an_empty_allow_list_fails_closed() {
        let provider = CountingProvider::new(crate::FIXTURE_PROVIDER_ID);
        let empty = AllowList::new(Vec::new());
        assert!(empty.is_empty());

        let err = run(submit_checked(
            &provider,
            &empty,
            request(crate::FIXTURE_PROVIDER_ID, "fixture-image"),
        ))
        .expect_err("an empty list admits nothing");
        assert!(matches!(err, GenError::ModelNotAllowed(_)));
        assert_eq!(provider.calls(), 0, "still never reached submit");
    }
}
