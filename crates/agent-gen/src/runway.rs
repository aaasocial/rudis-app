//! `RunwayProvider` — Runway ML's generation surface (Phase 42.1), the
//! single provider v5's multi-provider fan-out collapses onto.
//!
//! # Wave 1 is OFFLINE AND HERMETIC
//!
//! This module is built and proven with **zero network, zero API key, zero
//! spend**: every wire shape below is exercised through pure `serde` and pure
//! builder functions, exactly as `veo.rs`'s `build_predict_request` is. The one
//! live probe this phase needs (which models accept the first/last keyframe
//! PAIR — 42.1-RESEARCH gate (b)) belongs to Plan 04 and changes ONE table row
//! here, never this module's shape.
//!
//! # Licensing hygiene (CLAUDE.md rule 6 / PROVENANCE.md)
//!
//! Hand-rolled `reqwest` client written against Runway's PUBLIC HTTP API
//! documentation (request/response shapes only). There is **no vendor SDK** and
//! **no third-party source copied** — the same "thin, fully-audited typed client
//! we control" stance `AnthropicTransport` and `ElevenLabsProvider` take (and
//! that the now-retired `OpenAiProvider`/`VeoProvider` took before 42.1-02
//! deleted them).
//!
//! # The GEN-08 human gate — RETIRED for this provider (Phase 55.1, D-01)
//!
//! History, because the change is a deliberate reduction in a safety mechanism
//! and deserves to be readable: 42.1 Wave 1 shipped with **no `("runway", *)`
//! allow-list row at all**, so every Runway submit was fail-closed rejected by
//! [`submit_checked`](crate::submit_checked); 42.1 Plan 02 then added rows for
//! exactly five models under a dated human sign-off, and any other model in
//! [`RUNWAY_MODELS`] — `veo3`, `veo3.1`, `seedance2_mini`, `aleph2`,
//! `gen4_image_turbo` and both retired ids — was rejected before submit.
//!
//! **None of that is true any more.** The owner retired the Runway half of the
//! gate on 2026-08-01 (D-01): `"runway"` is in
//! [`UNGATED_PROVIDERS`](crate::allow_list::UNGATED_PROVIDERS), so
//! `submit_checked` — still the ONE gate, still called — admits **every** model
//! id, including ids absent from [`RUNWAY_MODELS`] entirely. Runway's own
//! server-side model enum is the sole existence check, and its 400 is surfaced
//! verbatim rather than re-authored. `RUNWAY_MODELS` survives as an ADVISORY
//! cost/label/caps table with no refusal power; see `production_allow_list` for
//! what the removed rows recorded and which sign-off condition outlived them.
//!
//! # SSRF discipline (T-31-09 / T-33-02, extended)
//!
//! Runway accepts THREE forms of `promptImage`: an HTTPS URL, a `data:` URI, and
//! a `runway://` URI. **Rudis can only ever express the `data:` form.** That is
//! enforced at the type level: the only inhabitant of a `promptImage` slot is a
//! [`RunwayDataUri`], whose inner `String` is private and whose ONLY constructor
//! is [`data_uri`] — which prepends a FIXED `data:image/png;base64,` literal to
//! backend-resolved bytes. There is structurally no caller path (and therefore
//! no prompt-injection path) that can put an attacker-chosen host into a
//! request.

use base64::Engine as _;

use crate::job::{JobHandle, JobId};
use crate::provider::{
    AssetRef, GenError, GenProvider, GenRequest, JobStatus, ModelInfo, ProviderId, ReferenceImage,
    RequestModality, SubmitOutcome,
};

// ---------------------------------------------------------------------------
// Identity, endpoints and headers. Every URL-shaped value here is a `const` —
// NEVER request data (T-31-09 / T-33-03 SSRF discipline, identical to
// `VEO_BASE_URL`): the ONLY things ever concatenated onto the base are the
// `&'static str` endpoint names below and an opaque task id Runway itself
// handed us, so no prompt-injected value can steer a request at another host.
// ---------------------------------------------------------------------------

/// This provider's stable registered id — what the GEN-08 gate keys on (since
/// 55.1-02 that means matching `UNGATED_PROVIDERS`, not matching a row) and
/// what the landing bridge interpolates into a generated filename, so it must
/// never be free text.
pub const RUNWAY_PROVIDER_ID: &str = "runway";

/// Runway's API base.
pub const RUNWAY_BASE_URL: &str = "https://api.dev.runwayml.com/v1";

/// Runway's REQUIRED API-version header name. Omitting it is an immediate
/// rejection, so it is applied in ONE place ([`RunwayProvider::headers`]) that
/// every request goes through — a new endpoint cannot forget it.
pub const RUNWAY_VERSION_HEADER: &str = "X-Runway-Version";

/// The pinned API version. A dated contract: Runway's request/response shapes
/// are versioned by this header, so pinning it is what stops a server-side
/// revision silently changing the bodies this module builds.
pub const RUNWAY_API_VERSION: &str = "2024-11-06";

pub const RUNWAY_ENDPOINT_TEXT_TO_IMAGE: &str = "text_to_image";
pub const RUNWAY_ENDPOINT_TEXT_TO_VIDEO: &str = "text_to_video";
pub const RUNWAY_ENDPOINT_IMAGE_TO_VIDEO: &str = "image_to_video";
/// Phase 56 (GEN-11): the endpoint that EDITS an existing clip's own pixels.
/// Reached only when the call carries source-video frames — never chosen by a
/// model id (55.1's central design consequence, made structural in
/// [`build_video_edit_submission`]).
pub const RUNWAY_ENDPOINT_VIDEO_TO_VIDEO: &str = "video_to_video";
/// The step-1 resource of D-03's two-step presigned upload — the overflow
/// transport for a clip too large to inline. See [`RunwayUploadsResponse`].
pub const RUNWAY_ENDPOINT_UPLOADS: &str = "uploads";
/// The task-status resource polled after submit; the opaque task id is appended.
pub const RUNWAY_ENDPOINT_TASKS: &str = "tasks";

/// Runway's FLOOR for an uploaded asset (56-RESEARCH § Q2) — **CORROBORATED**.
///
/// Only reachable in a degenerate case — a payload under 512 raw bytes could
/// never exceed the inline cap — but [`video_transport_for_len`] refuses on
/// it EXPLICITLY rather than falling through to a transport, so a future change
/// to either cap cannot open a silent hole.
///
/// See [`RUNWAY_MAX_UPLOAD_BYTES`] for the shared epistemic note: probe F-4 read
/// this number out of Runway's own SIGNED S3 POST policy, so it is no longer
/// documentation taken on trust.
pub const RUNWAY_MIN_UPLOAD_BYTES: usize = 512;

/// Runway's CEILING for an uploaded asset — 200 MB, with a **24 h server-side
/// expiry** on the stored object (56-RESEARCH § Q2).
///
/// # Epistemic status: DOCUMENTED, then CORROBORATED, and the transport is now LIVE-EXERCISED
///
/// **This line used to read "`/v1/uploads` is entirely UNPROBED — 56-01 tracked
/// it as follow-up F-4".** That is no longer true in any part, and the
/// correction is worth stating precisely because the previous wording is what
/// made this endpoint the prime suspect for a bug it did not cause.
///
/// **F-4 is CLOSED** (2026-08-13, $0.00 measured by a `creditBalance` read either
/// side; `scripts/runway-capability-probe.mjs --uploads`, artifact
/// `.planning/phases/56-…/artifacts/56-F4-F5-PROBE-RESULTS.md`):
///
/// * **The window is CORROBORATED, not merely documented.** `POST /v1/uploads`
///   returns an S3 POST policy whose conditions include
///   `["content-length-range", 512, 209715200]` — this constant and
///   [`RUNWAY_MIN_UPLOAD_BYTES`] byte for byte, read out of a document the
///   vendor SIGNED rather than off a docs page.
/// * **The response shape is CONFIRMED.** `uploadUrl` / `fields` / `runwayUri`
///   are the real spellings [`RunwayUploadsResponse`] deserializes, and
///   `runwayUri` is issued by step 1 *before any bytes exist* — research
///   assumption A3, and with it D-03's SSRF argument, holds on evidence.
/// * **Step 2 works.** A real 30 162 998 B MP4 through the presigned multipart
///   POST: **HTTP 204**.
/// * **The `runway://` handoff works** — the one step no free probe could reach,
///   because only a real submit can show that `video_to_video` ACCEPTS an
///   uploaded-asset handle in `videoUri`. Owner-verified end to end in the
///   shipped app on 2026-08-13, on the same 2160x4096 clip that had failed.
///
/// So [`upload_video_to_runway`](RunwayProvider::upload_video_to_runway) is no
/// longer dead code carrying an untested contract: **all three hops — step 1,
/// the presigned transfer, and the submit that consumes the handle — have now
/// executed for real.** Before that day none of them ever had; 56-09
/// deliberately kept its one paid run on the inline arm *because* F-4 was open,
/// which is exactly why the first clip big enough to need this transport was
/// also the first to exercise it.
///
/// **Still NOT established, and not guessed:** whether the 24 h expiry deletes
/// the bytes or only stops the URI resolving.
pub const RUNWAY_MAX_UPLOAD_BYTES: usize = 200 * 1024 * 1024;

/// The version header pair every Runway request carries — pure, key-free and
/// public, so the hermetic suite can assert it without ever touching a
/// credential.
pub const fn version_header() -> (&'static str, &'static str) {
    (RUNWAY_VERSION_HEADER, RUNWAY_API_VERSION)
}

/// Bound a hostile/oversized download BEFORE it reaches the landing bridge —
/// mirrors both the bridge's own `MAX_GEN_ASSET_BYTES` and
/// `VEO_MAX_DOWNLOAD_BYTES` (T-33-05 DoS).
pub const RUNWAY_MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Runway's documented poll FLOOR: "≥5s intervals with jitter and exponential
/// backoff", not fixed-interval polling. Today's Veo path is a flat 10s, which
/// satisfies the floor but not the jitter clause — this provider carries the
/// improvement over.
pub const RUNWAY_POLL_INTERVAL_FLOOR: std::time::Duration = std::time::Duration::from_secs(5);

/// The width of the random window added on top of the floor, so N concurrent
/// jobs submitted in the same instant do not stay phase-locked and hammer the
/// status endpoint in lockstep for their whole lifetime.
pub const RUNWAY_POLL_JITTER_SPAN: std::time::Duration = std::time::Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Pinned request constants. Every one is a `const` — NEVER request data
// (T-31-09 SSRF discipline, the same posture `VEO_ASPECT_RATIO`/
// `VEO_RESOLUTION`/`VEO_DURATION_SECONDS` take).
// ---------------------------------------------------------------------------

/// The pinned output aspect ratio. Rudis pins landscape 720p for the same
/// cost-conscious reason `VEO_RESOLUTION` pinned `"720p"` — `GenRequest` is
/// deliberately NOT extended with a ratio knob.
///
/// **42.1-04 correction (live-verified 2026-07-27).** 42.1-RESEARCH (b) gave a
/// single documented set (`1280:720 | 720:1280 | 1104:832 | 832:1104 |
/// 960:960`) for all video. That is WRONG: the legal set is **per model AND per
/// endpoint**, and it is enumerated verbatim in Runway's 400 body. Observed:
///
/// | model / endpoint | legal `ratio` |
/// | --- | --- |
/// | `veo3` · `veo3.1` · `veo3.1_fast` (image_to_video) | `1280:720`, `720:1280`, `1080:1920`, `1920:1080` |
/// | `gen4_turbo` · `gen4.5` (image_to_video) | `1280:720`, `720:1280`, `1104:832`, `832:1104`, `960:960`, `1584:672` |
/// | `gen4.5` (text_to_video) | `1280:720`, `720:1280` — only two |
/// | `seedance2` · `seedance2_mini` (image_to_video) | not required at all |
/// | `gen4_image` (text_to_image) | 16 values, including `1280:720` |
///
/// `1280:720` is the ONE value legal on every cleared model and every endpoint,
/// which is why a single pinned const works at all. That is now evidence, not
/// luck — and [`RUNWAY_RATIO_IS_UNIVERSAL`] is the guard that says so.
pub const RUNWAY_RATIO: &str = "1280:720";

/// Recorded so a future model addition has to confront the fact above rather
/// than assume it: the pin is only safe because `1280:720` appeared in EVERY
/// legal-ratio enumeration the 42.1-04 probe collected. A model whose set
/// omits it needs its own ratio, not this const.
pub const RUNWAY_RATIO_IS_UNIVERSAL: bool = true;

/// The pinned clip length, in seconds. A JSON NUMBER on the wire (`u32`), never
/// a string — the 33-03 live-run lesson (`durationSeconds` as `"4"` earned an
/// HTTP 400) carried across to a new provider rather than re-learned.
///
/// 4s matches today's Veo spend baseline exactly (~$0.20/clip), which is what
/// makes the cost column of the intent table comparable to what ships today.
///
/// **42.1-04 correction (live-verified 2026-07-27).** 42.1-RESEARCH (b) said
/// "integer 2–10". Also wrong, and per-model:
///
/// | model | legal `duration` |
/// | --- | --- |
/// | `veo3.1` · `veo3.1_fast` | **exactly 4, 6 or 8** — a discrete union, not a range |
/// | `veo3` | **exactly 8** |
/// | `gen4_turbo` · `gen4.5` | a number `<= 10` |
/// | `seedance2` · `seedance2_mini` | a number `<= 15` |
///
/// `4` is legal on every model except `veo3` (which no intent routes to). Had
/// the pin been 5, or 2, the transition tier would have 400'd on submit.
pub const RUNWAY_DURATION_SECONDS: u32 = 4;

/// Runway's documented cap on an inlined `data:` URI for an IMAGE (16 MB for
/// video/audio, which this wave never inlines). Conditioning frames reaching
/// here are already bounded by the caller's `MAX_MEDIA_REFERENCE_LONG_EDGE`
/// (1568px) clamp, which puts a PNG comfortably under this — so
/// [`validate_data_uri_sizes`] is a belt-and-braces pre-egress bound, not the
/// primary control (42.1-RESEARCH gate (c)).
pub const RUNWAY_MAX_DATA_URI_BYTES: usize = 5 * 1024 * 1024;

/// The FIXED data-URI prefix. `image/png` is a LITERAL, never derived from a
/// caller, a response, or a sniffed header (T-31-12 / T-32-03 "fixed literal,
/// never derived") — Rudis's conditioning bytes are ALWAYS PNG, produced
/// upstream by `engine::encode_png_bytes`.
const RUNWAY_DATA_URI_PREFIX: &str = "data:image/png;base64,";

/// The FIXED data-URI prefix for the `video_to_video` INPUT CLIP — a DIFFERENT
/// literal from `RUNWAY_DATA_URI_PREFIX` (private, the image twin), under the
/// same never-derived rule
/// (T-31-12 / T-32-03 "fixed literal, never derived").
///
/// `video/mp4` is a literal, never a sniffed container type and never a
/// caller-supplied mime: the bytes reaching here are produced by Rudis's own
/// trim-respecting extraction (Plan 04), which always writes MP4.
///
/// **Live-probed 2026-08-01 (56-01, `v2v-2-data-uri-decode-stage`).** A
/// malformed `data:video/mp4;base64,…` on `videoUri` failed at the CONTENT
/// stage (`"Failed to fetch video metadata"`), not the shape stage — i.e. the
/// string satisfied a union branch and only the bytes were rejected. That is
/// the confirmation that this transport exists at all; see
/// [`RUNWAY_ENDPOINT_UPLOADS`] for the overflow path it makes optional rather
/// than mandatory.
pub const RUNWAY_VIDEO_DATA_URI_PREFIX: &str = "data:video/mp4;base64,";

/// Runway's DOCUMENTED cap on an inlined `data:` URI for VIDEO/AUDIO — 16 MB,
/// three times the 5 MB still-image cap in [`RUNWAY_MAX_DATA_URI_BYTES`]
/// (`docs.dev.runwayml.com/assets/inputs`, recorded by 42.1-RESEARCH § (c) and
/// re-confirmed as this endpoint's real transport by 56-01).
///
/// **PER-ASSET, not per-request** (56-RESEARCH Pitfall 2).
///
/// # Recorded as EVIDENCE, and deliberately NOT the operative bound
///
/// This number is true about the *asset* and unreachable in *practice*: the API
/// host refuses the whole request at [`RUNWAY_MAX_REQUEST_BODY_BYTES`], which
/// probe F-5 MEASURED at 10 MiB — 60% BELOW this figure. So a data URI legal by
/// this cap can still be refused before Runway's validator ever sees it. That is
/// exactly the bug this constant caused (debug session
/// `v2v-413-on-4k-source-unprobed-upload-window`), and the constant survives as
/// the documented fact rather than as a gate — the same posture
/// [`RUNWAY_OBSERVED_OUTPUT_HOST`] takes.
///
/// [`RUNWAY_MAX_VIDEO_DATA_URI_BYTES`] is what the router and
/// [`check_video_data_uri_size`] actually enforce.
pub const RUNWAY_DOCUMENTED_VIDEO_ASSET_CAP_BYTES: usize = 16 * 1024 * 1024;

/// The API host's REAL ceiling on a whole request body — **10 MiB, MEASURED**.
///
/// # This number used to be invented, and the invention cost a live failure
///
/// Until 2026-08-13 this was `48 * 1024 * 1024`, chosen as "three times the
/// per-asset video cap: generous enough that no legitimate request can hit it".
/// It was labelled honestly as Rudis's own number — and it was **4.6x too
/// loose**, so it never fired, and the thing it was supposed to backstop reached
/// the wire instead. The owner's first live 4K clip edit met
/// `HTTP 413 {"message":"Request Entity Too Large"}` twice.
///
/// # What was measured, and how
///
/// Probe F-5 (`scripts/runway-capability-probe.mjs --bodysize --fine`,
/// 2026-08-13, **$0.00 measured by a `creditBalance` read either side**) walked
/// escalating bodies at `POST /v1/video_to_video`, each carrying 56-01 V-2's
/// proven-safe undecodable `videoUri`:
///
/// | serialized body | result |
/// | --- | --- |
/// | 10 485 760 B (exactly 10 MiB) | **HTTP 400** — `{"code":"custom","message":"Failed to fetch video metadata","path":["videoUri"]}`, i.e. the body was ACCEPTED and only the bytes were rejected |
/// | 10 747 904 B | **HTTP 413** — `{"message":"Request Entity Too Large"}` |
/// | 11 010 048 / 11 534 336 / 12 582 912 / 16 / 20 MB | **HTTP 413**, identically |
///
/// So the ceiling is 10 MiB inclusive. That is AWS API Gateway's documented
/// hard, non-configurable request-payload limit, and the 413 arrives from an
/// EDGE PROXY: no `issues` array, no field path, no `docUrl`. Nothing downstream
/// can turn it into advice, which is why it has to be caught here.
///
/// **Confidence: MEASURED, not documented** — the inverse of every other size
/// constant in this module, and stated that way round on purpose. Runway
/// documents no combined per-request cap anywhere (56-RESEARCH Open Question 1);
/// this came off the wire.
pub const RUNWAY_MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;

/// How much of [`RUNWAY_MAX_REQUEST_BODY_BYTES`] is held back for everything in
/// the body that is NOT the video data URI — the model id, the prompt, and the
/// JSON punctuation around all three.
///
/// 64 KiB, which is 0.6% of the budget and comfortably more than the worst case
/// can need: the prompt is bounded at 4 000 CHARS by
/// `app_core::MAX_AGENT_VIDEO_PROMPT_CHARS`, and even if every one of them
/// JSON-escapes to a 6-byte `\uXXXX` that is 24 KB, leaving ~40 KB for a
/// free-text model id and the scaffolding.
///
/// It is a RESERVE rather than an exact computation because
/// [`video_transport_for_len`] is a pure function of two lengths — it never sees
/// the prompt — and widening its signature to thread one through would put the
/// transport decision downstream of request content for no gain. The exactness
/// lives one layer later instead: [`build_video_edit_submission`] measures the
/// REAL serialized body against [`RUNWAY_MAX_REQUEST_BODY_BYTES`], so a
/// pathological prompt or model id is caught before egress even though the
/// router could not have predicted it.
pub const RUNWAY_BODY_SCAFFOLD_RESERVE_BYTES: usize = 64 * 1024;

/// The OPERATIVE ceiling on an inlined source clip: what is left of the MEASURED
/// request-body budget once [`RUNWAY_BODY_SCAFFOLD_RESERVE_BYTES`] is held back.
///
/// **DERIVED, so there is exactly one number to correct.** The name is unchanged
/// from when this was the documented 16 MB per-asset figure, because every call
/// site and test in the workspace already reads it symbolically — but the VALUE
/// is now the bound that actually decides whether a request survives the edge.
/// The documented per-asset figure is kept as evidence at
/// [`RUNWAY_DOCUMENTED_VIDEO_ASSET_CAP_BYTES`].
///
/// # Why lowering this is a FIX and not a new refusal
///
/// A clip pushed off the inline arm does not fail — it takes the `/v1/uploads`
/// overflow arm, whose window probe F-4 MEASURED as
/// `content-length-range 512 .. 209715200` (the vendor's own signed S3 policy,
/// agreeing exactly with [`RUNWAY_MIN_UPLOAD_BYTES`]/[`RUNWAY_MAX_UPLOAD_BYTES`])
/// and whose step-2 transfer the same probe completed live at 30 162 998 B for
/// **HTTP 204**. Both arms of [`VideoTransport`] are now live-proven, which is
/// what makes moving the boundary safe rather than merely narrower.
pub const RUNWAY_MAX_VIDEO_DATA_URI_BYTES: usize =
    RUNWAY_MAX_REQUEST_BODY_BYTES - RUNWAY_BODY_SCAFFOLD_RESERVE_BYTES;

// ---------------------------------------------------------------------------
// Phase 56 (GEN-11 / D-01) — the `video_to_video` INPUT window.
//
// These are the numbers Plan 04's named refusal reads. They live here, as
// consts with their epistemic status in the doc, precisely so that the refusal
// mechanism is decoupled from the number's confidence — which is what D-01's
// 2026-08-01 correction actually fixed after the phase was discussed believing
// the ceiling was 10s.
// ---------------------------------------------------------------------------

/// The MINIMUM input length `video_to_video` accepts, in whole seconds.
///
/// **DOCUMENTED (3 official sources: Runway's API changelog, the
/// `/assets/inputs` limits page and the `aleph-2` product page), NOT
/// live-verified — V-6 outstanding** (56-01 constant 11, follow-up F-3).
///
/// 56-01 could not reach a duration check and said so rather than rounding up:
/// `duration` is not a request field (so nothing declared was range-checked)
/// and the content path fails at *"Failed to fetch video metadata"* BEFORE any
/// length validation. Settling it needs a real, decodable MP4 longer than the
/// ceiling.
///
/// Corroborating evidence that 2 s is real: Runway's pricing page carries
/// `minimumCredits: {'aleph2': 56}`, which is exactly 2 s at the published
/// 28 credits/s — the floor is priced, which is a different source agreeing.
///
/// A sub-2 s clip must refuse too; the original D-01 discussion never
/// contemplated a minimum at all.
pub const RUNWAY_V2V_INPUT_MIN_SECONDS: u32 = 2;

/// The MAXIMUM input length `video_to_video` accepts, in whole seconds.
///
/// **DOCUMENTED (3 official sources), NOT live-verified — V-6 outstanding**
/// (56-01 constant 11, follow-up F-3). Same reason as
/// [`RUNWAY_V2V_INPUT_MIN_SECONDS`].
///
/// **Explicitly NOT `10`.** The phase was discussed believing the ceiling was
/// ≤10 s, inherited from 42.1-RESEARCH's UPDATE section; 56-RESEARCH § Q3
/// retired that number against three official-domain sources. 42.1 has the
/// cautionary precedent in the other direction too — its own DOCUMENTED
/// `duration` range was found wrong by live probing, and a pin of `5` would
/// have 400'd an entire tier on submit. So this number is carried at MEDIUM
/// confidence and labelled, not promoted.
///
/// At the published $0.28/s this bounds a single call at **$8.40** — the
/// figure the GEN-08 sign-off was given, and 21x the transition tier.
pub const RUNWAY_V2V_INPUT_MAX_SECONDS: u32 = 30;

/// The input FRAME-RATE ceiling: `docs.dev.runwayml.com/assets/inputs` states
/// "30 FPS or lower".
///
/// **DOCUMENTED, NOT PROBED** — it was outside the authorised V-0..V-6 set
/// (56-01 constant 12). Undocumented until 56-RESEARCH § Q3 surfaced it, which
/// is itself the reason to carry it as a const: a 60 fps phone clip is an
/// entirely ordinary input and nothing else in the pipeline would notice.
pub const RUNWAY_V2V_INPUT_MAX_FPS: u32 = 30;

/// How many conditioning images the endpoint's contract allows alongside the
/// source clip (D-08; 42.1-RESEARCH (a) UPDATE, re-stated by Runway's Aleph 2.0
/// changelog as "up to 5 keyframe images").
///
/// **An ENDPOINT-contract cap on the request SHAPE, not a per-model capability
/// gate** — which is why it survives 55.1's no-caps rule while
/// `caps.video_to_video` does not: it bounds what the body may contain, never
/// which model may be named.
///
/// **LIVE-PROBED (56-F1b, 2026-08-09) — and it is REAL.** This line read
/// "DOCUMENTED, NOT PROBED" until plan 06 corrected it, which was accurate only
/// of 56-F1: that sweep never reached an array length, because a scalar cannot
/// reach an array's item schema. **F-1b sent six items and the endpoint named
/// its own bound:**
///
/// ```json
/// {"origin":"array","code":"too_big","maximum":5,"inclusive":true,"path":["keyframes"],"message":"Too big: expected array to have <=5 items"}
/// ```
///
/// …and the byte-equivalent response at `["promptImage"]`. So `5` is now
/// MEASURED rather than read off a changelog, **inclusive**, and a genuine
/// upper bound rather than a fixed arity (a one-item body raised an item-type
/// issue and no length issue at all). The **minimum**-items bound and the
/// validity of `[]` remain unprobed and are not guessed here — an empty array
/// cannot carry F-1b's guaranteed-invalid rider, so it was unreachable under
/// that run's $0.00 discipline.
///
/// ⚠ **The ceiling belongs to BOTH candidate fields, which is exactly why it
/// crowns neither.** See [`build_video_edit_submission`] for why references
/// still refuse entirely rather than ride a guessed key, and why the remaining
/// question is now a PAID one (56-09) rather than another free probe.
pub const RUNWAY_V2V_MAX_REFERENCES: usize = 5;

/// The ONE tag Rudis ever attaches to a still-image reference, and therefore
/// the only `@…` token it ever writes into `promptText`.
///
/// A fixed literal for the same reason every other Runway constant here is one:
/// the tag round-trips through the prompt, so a caller-chosen tag would be
/// caller text reaching a structured field AND reaching the prompt — two
/// injection surfaces for zero capability. Alphanumeric and letter-initial,
/// which is the conservative intersection of every tag-naming rule Runway
/// documents.
pub const RUNWAY_REFERENCE_TAG: &str = "Reference";

/// The sentence [`build_text_to_image_request`] appends to `promptText` to CITE
/// [`RUNWAY_REFERENCE_TAG`]. Runway's tagged-reference mechanism works by the
/// prompt naming `@Tag`; an attached-but-unnamed reference is the one shape
/// 42.1-01 refused to guess at, so Rudis simply never produces it.
///
/// **42.1-04 verified live (2026-07-27) that this was the right call, and that
/// the alternative would have failed SILENTLY.** Probe `image-reference-shape`
/// confirmed `referenceImages: [{uri, tag}]` parses on `gen4_image`. Probe
/// `image-reference-uncited` then sent the SAME array with a prompt that never
/// names `@Reference` — and Runway accepted it without complaint. So an uncited
/// tag is not a 400; it is a reference the model is simply never told to use.
/// Had 42.1-03 attached the array without the citation, the user's sketch would
/// have been dropped server-side with a 200 and a plausible unconditioned
/// image coming back — undetectable by any caller. Structurally coupling the
/// attachment to the citation is what makes that unreachable.
///
/// Appended, not prepended: the agent-authored prompt keeps first position (the
/// 40-prompt-expansion structure puts the subject early), and this reads as the
/// constraint it is.
const RUNWAY_REFERENCE_CITATION: &str =
    " Match the appearance, composition and subject of @Reference.";

// ---------------------------------------------------------------------------
// Wire types — request side (Serialize).
// ---------------------------------------------------------------------------

/// A `data:image/png;base64,…` URI built from BACKEND-RESOLVED bytes — the only
/// form of `promptImage` Rudis can express.
///
/// The inner `String` is **private** and the ONLY constructor is [`data_uri`].
/// That is the whole point: Runway also accepts `https://…` and `runway://…`
/// for this field, and an HTTPS form reachable from model output would be an
/// SSRF surface of exactly the kind [`GenRequest`](crate::GenRequest)'s "no URL
/// field" tripwire exists to prevent (42.1-RESEARCH gate (c), security note).
/// A caller cannot hand-build one from a string, so there is no such path.
///
/// `#[serde(transparent)]` — on the wire this is a bare JSON string, exactly
/// what Runway's `promptImage` expects.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct RunwayDataUri(String);

impl RunwayDataUri {
    /// Borrow the URI. Read-only: there is deliberately no `from_str`/`new`
    /// taking arbitrary text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Byte length of the URI as it will appear on the wire — what
    /// [`validate_data_uri_sizes`] bounds against Runway's 5 MB cap.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Never true in practice (the fixed prefix alone is non-empty); present
    /// because clippy pairs it with [`len`](Self::len).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Phase 56 (GEN-11 / SC-3 / D-03) — the TWO forms of a `video_to_video` input
// clip, and why there are exactly two.
//
// `videoUri` is a union of exactly THREE string branches (56-01 constant 2:
// every `invalid_union` error carries exactly 3 sub-error groups). Documented
// and consistent with an HTTPS URL / a `data:` URI / a `runway://` handle.
// **Rudis can express only the second and third**, and neither is a host a
// caller or a prompt can choose:
//
//   * `RunwayVideoDataUri` — private inner `String`, ONE constructor
//     (`video_data_uri`) which prepends a fixed literal to backend-produced
//     bytes. Identical discipline to `RunwayDataUri`.
//   * `RunwayUploadedAsset` — private inner `String`, and its ONLY way into
//     existence is deserializing Runway's OWN `/v1/uploads` response through a
//     custom `Deserialize` that REFUSES anything not beginning `runway://`.
//
// D-03 reopens an asset transport 42.1 deliberately closed, so the type system
// has to say the SSRF property, not a comment (56-CONTEXT D-03: "the
// mitigating fact to verify, not assume ... only if the type system says so").
// There is structurally no caller path — and therefore no prompt-injection
// path — that can put an attacker-chosen host into a request field
// (T-31-09 / T-33-02, extended to video).
// ---------------------------------------------------------------------------

/// A `data:video/mp4;base64,…` URI built from BACKEND-PRODUCED clip bytes — the
/// inline form of `videoUri`.
///
/// The inner `String` is **private** and the ONLY constructor is
/// [`video_data_uri`]. `#[serde(transparent)]` — on the wire this is a bare
/// JSON string, which is what `videoUri`'s string union expects.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct RunwayVideoDataUri(String);

impl RunwayVideoDataUri {
    /// Borrow the URI. Read-only: there is deliberately no `from_str`/`new`
    /// taking arbitrary text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Byte length of the URI as it will appear on the wire — what
    /// [`check_video_data_uri_size`] bounds against the 16 MB per-asset cap.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Never true in practice (the fixed prefix alone is non-empty); present
    /// because clippy pairs it with [`len`](Self::len).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Inline real clip bytes as a `data:video/mp4;base64,…` URI — the ONLY
/// constructor of a [`RunwayVideoDataUri`].
///
/// The prefix is a fixed literal ([`RUNWAY_VIDEO_DATA_URI_PREFIX`]); the base64
/// engine is the crate-standard `STANDARD` (padded) alphabet, the same one
/// [`data_uri`] uses.
///
/// **Encoding a 16 MB clip allocates ~21 MB of `String`.** Call
/// [`projected_video_data_uri_len`] FIRST when all you need is the transport
/// decision — see [`video_transport_for_len`]. This function is for the branch
/// that has already decided to inline.
pub fn video_data_uri(bytes: &[u8]) -> RunwayVideoDataUri {
    RunwayVideoDataUri(format!(
        "{RUNWAY_VIDEO_DATA_URI_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Bound one inlined clip against [`RUNWAY_MAX_VIDEO_DATA_URI_BYTES`] BEFORE
/// any egress.
///
/// The message names the LENGTH and the CAP — never the URI itself, which is
/// megabytes of base64 and would flood any log it reached (the same rule
/// `check_data_uri_size` (private, the 5 MB image twin) follows).
///
/// Since 2026-08-13 the cap it names is the MEASURED body-budgeted one rather
/// than the documented per-asset one, so this check is now the last thing between
/// an over-budget inline body and the edge proxy's contentless 413. Reaching it
/// at all means [`video_transport_for_len`] was bypassed, because that router
/// sends anything this large to `/v1/uploads` instead.
pub fn check_video_data_uri_size(uri: &RunwayVideoDataUri) -> Result<(), GenError> {
    if uri.len() > RUNWAY_MAX_VIDEO_DATA_URI_BYTES {
        return Err(GenError::InvalidRequest(format!(
            "the inlined source clip is {} bytes, over the \
             {RUNWAY_MAX_VIDEO_DATA_URI_BYTES}-byte per-asset inline budget that \
             Runway's measured {RUNWAY_MAX_REQUEST_BODY_BYTES}-byte request-body \
             ceiling leaves; a clip this size belongs on the upload transport",
            uri.len()
        )));
    }
    Ok(())
}

/// A `runway://…` handle Runway's OWN `/v1/uploads` response issued — the
/// overflow form of `videoUri` (D-03).
///
/// # Why this type has no constructor
///
/// A `runway://` URI is **server-issued**, never caller-chosen. That is the
/// entire SSRF argument for reopening this transport, and 56-CONTEXT D-03 is
/// explicit that it must be verified by the type system rather than asserted in
/// a comment. So: the inner `String` is private; there is no `new`, no
/// `From<String>`, no `FromStr`, no public field and no `Default`. The ONLY way
/// a value of this type comes into existence anywhere in the workspace is the
/// custom [`Deserialize`](serde::Deserialize) below, reached through
/// [`RunwayUploadsResponse`].
///
/// The custom impl also refuses any value that does not begin with the literal
/// `runway://`, so **even a forged or compromised response body cannot smuggle
/// an `https://`/`file://` egress target into a request field** — the response
/// arrives over TLS from a pinned host, but it is still deserialized bytes from
/// the network (T-56-SSRF-01).
///
/// `#[serde(transparent)]` on the Serialize side: on the wire this is a bare
/// JSON string, the same union branch shape [`RunwayVideoDataUri`] occupies.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct RunwayUploadedAsset(String);

impl RunwayUploadedAsset {
    /// The required scheme prefix, as a fixed literal. A handle that does not
    /// start with this is not a Runway asset handle and is refused at the only
    /// door into this type.
    const REQUIRED_PREFIX: &'static str = "runway://";

    /// Borrow the handle. Read-only, and the only accessor.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for RunwayUploadedAsset {
    /// Hand-written rather than derived, because the derive would accept any
    /// string and this type's whole value is that it does not. Deliberately
    /// case-SENSITIVE and prefix-exact: `RUNWAY://` and `x-runway://` are both
    /// refused, because a scheme comparison that is loose in either direction
    /// is how a look-alike gets through.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        if !raw.starts_with(Self::REQUIRED_PREFIX) {
            // The error names the required scheme, never the rejected value —
            // a hostile URL echoed into an error string is an injection vector
            // into every log that error reaches.
            return Err(serde::de::Error::custom(
                "a Runway uploaded-asset handle must begin with runway://",
            ));
        }
        Ok(RunwayUploadedAsset(raw))
    }
}

/// The `POST /v1/uploads` step-1 response — **the ONE place a
/// [`RunwayUploadedAsset`] comes into existence.**
///
/// Deserialize-only (there is no reason for Rudis to ever construct one), and
/// the key spellings are `{uploadUrl, fields, runwayUri}`.
///
/// **Epistemic status, stated because this module's discipline is that
/// documented and probed facts are labelled differently: these three key names
/// are now LIVE-CONFIRMED, not documentation.** This paragraph read *"`/v1/uploads`
/// is entirely unprobed — 56-01 tracked it as follow-up F-4"* until 2026-08-13,
/// when probe F-4 called the endpoint for real at $0.00 and got back exactly
/// `[uploadUrl, fields, runwayUri]` — the spellings 56-RESEARCH § Q2 had
/// predicted, now observed. See [`RUNWAY_MAX_UPLOAD_BYTES`] for the full close-out.
///
/// The old safety argument still holds and is worth keeping: a wrong spelling
/// fails loudly and at $0.00, because serde raises a missing-field error before
/// any second request exists. Being wrong here was always the cheap direction.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RunwayUploadsResponse {
    /// The presigned target for the step-2 multipart POST. A `String` because
    /// it is a full https URL rather than a Rudis-controlled form — which is
    /// exactly why [`validate_upload_url`] gates it before any request is sent
    /// at it, and why it is NEVER stored in a request body.
    #[serde(rename = "uploadUrl")]
    pub upload_url: String,
    /// The presigned form fields that must be replayed verbatim as multipart
    /// parts. Opaque to Rudis: copied through, never parsed, never logged.
    #[serde(default)]
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// The server-issued handle — the ONLY value from this response that ever
    /// reaches a request body.
    #[serde(rename = "runwayUri")]
    pub runway_uri: RunwayUploadedAsset,
}

/// The `videoUri` slot, modelled as a CLOSED enum over the two forms Rudis can
/// express — so the inline and uploaded cases are distinct at the type level
/// and neither can be confused for a free string.
///
/// `#[serde(untagged)]`: both arms serialize as a bare JSON string, which is
/// what `videoUri`'s union expects. **Both arms carry a private-inner,
/// one-constructor type**, so no caller path (and therefore no prompt-injection
/// path) can put an attacker-chosen host into a request — the T-31-09 property
/// [`RunwayDataUri`] established for images, carried across to video without
/// weakening it, which is what SC-3 asks for and what D-03 could otherwise have
/// quietly cost.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum RunwayVideoInput {
    /// Inline, under the 16 MB per-asset cap. The default path.
    DataUri(RunwayVideoDataUri),
    /// The `/v1/uploads` overflow handle (D-03), for clips the inline cap
    /// cannot carry.
    Uploaded(RunwayUploadedAsset),
}

/// Which transport a source clip of a given size must take.
///
/// A closed enum rather than a `bool` so the `match` at the one call site is
/// exhaustive, and so the two arms read as the two genuinely different
/// data-handling paths they are (one round trip vs. a presigned upload Runway
/// stores for 24 h — a distinction the GEN-08 pack names explicitly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoTransport {
    /// Inline as a `data:video/mp4;base64,…` URI on the request itself.
    DataUri,
    /// Two-step: `POST /v1/uploads`, then a multipart POST to the presigned
    /// URL, then send the server-issued `runway://` handle.
    Upload,
}

/// The exact byte length [`video_data_uri`] WOULD produce for `raw_len` input
/// bytes, computed arithmetically instead of by encoding.
///
/// This exists so the transport decision never has to encode: a 200 MB clip
/// base64-expands to ~266 MB of `String`, and allocating that only to discover
/// it must go to `/v1/uploads` instead would be a self-inflicted memory spike
/// on the exact path that decided not to inline it.
///
/// `STANDARD` is the padded alphabet, so the encoded length is
/// `4 * ceil(n / 3)` exactly — pinned against the real encoder by
/// `projected_data_uri_len_agrees_with_the_real_encoder`, which is what stops
/// this arithmetic drifting if the engine ever changes.
pub fn projected_video_data_uri_len(raw_len: usize) -> usize {
    RUNWAY_VIDEO_DATA_URI_PREFIX.len() + raw_len.div_ceil(3) * 4
}

/// Decide the transport for a source clip — the whole of D-03's routing, as a
/// PURE function of two lengths so it is provable with no client and no bytes.
///
/// * `data_uri_len` — the SERIALIZED length the inline form would have (use
///   [`projected_video_data_uri_len`], not a raw byte count: the inline cap is a
///   bound on the *URI*, and base64 inflates by 4/3).
/// * `raw_len` — the clip's real byte length, which is what `/v1/uploads`
///   bounds.
///
/// Inline wins at the boundary (`== cap` still inlines) because both bounds are
/// maxima rather than exclusive bounds, and because the inline path is the one
/// that leaves nothing on Runway's storage for 24 h. Over the inline cap the
/// upload window applies; outside BOTH, this is a clean local `InvalidRequest`
/// naming every bound, so the caller (Plan 04's refusal, then the agent) can say
/// something true about what would fit.
///
/// # 2026-08-13: what moved, and why the boundary is now trustworthy
///
/// The routing logic is BYTE-UNCHANGED. What changed is
/// [`RUNWAY_MAX_VIDEO_DATA_URI_BYTES`], which used to be the documented 16 MB
/// per-asset figure and is now derived from the MEASURED 10 MiB request-body
/// ceiling. Under the old value this function inlined every clip in the
/// 10 485 761 .. 16 777 216 B data-URI band, and an edge proxy refused each one
/// with a contentless `HTTP 413` — a band a ~3 s range of an ordinary 4K phone
/// clip lands squarely inside. Both arms are now live-proven (F-4 completed a
/// real 30 162 998 B step-2 transfer for HTTP 204; F-5 measured the inline
/// ceiling), so the boundary between them is measurement rather than
/// documentation.
pub fn video_transport_for_len(
    data_uri_len: usize,
    raw_len: usize,
) -> Result<VideoTransport, GenError> {
    if data_uri_len <= RUNWAY_MAX_VIDEO_DATA_URI_BYTES {
        return Ok(VideoTransport::DataUri);
    }
    if (RUNWAY_MIN_UPLOAD_BYTES..=RUNWAY_MAX_UPLOAD_BYTES).contains(&raw_len) {
        return Ok(VideoTransport::Upload);
    }
    // Outside BOTH transports. The two edges are NOT the same failure and must
    // not share a remedy — the D-01 window refusal's discipline
    // (`app_core::clip_edit_window_check`), applied to bytes instead of seconds.
    if raw_len > RUNWAY_MAX_UPLOAD_BYTES {
        // TOO BIG. A shorter range or a smaller frame is the remedy; a SPLIT is
        // not, because splitting leaves both halves on the same timeline and the
        // user still has to pick one — and the D-01 floor means a half that is
        // too short is refused for the opposite reason.
        return Err(GenError::InvalidRequest(format!(
            "this clip's extracted range is {raw_len} bytes ({:.1} MB), over the \
             {RUNWAY_MAX_UPLOAD_BYTES}-byte ({} MB) maximum Runway's upload \
             endpoint accepts — and far over the {RUNWAY_MAX_VIDEO_DATA_URI_BYTES}-byte \
             inline budget too, so neither transport can carry it. Trim it to a \
             shorter range, or use a lower-resolution version of the footage — the \
             clip-edit model normalizes resolution anyway, so a smaller frame \
             usually costs nothing. Sending a truncated range instead is not an \
             option.",
            raw_len as f64 / (1024.0 * 1024.0),
            RUNWAY_MAX_UPLOAD_BYTES / (1024 * 1024)
        )));
    }
    // TOO SMALL: over the inline budget yet under the upload floor. Only
    // reachable if the two windows ever stop overlapping, which is precisely the
    // hole `RUNWAY_MIN_UPLOAD_BYTES`'s doc says must never open silently.
    Err(GenError::InvalidRequest(format!(
        "the source clip is {raw_len} bytes, which fits neither transport: \
         inlining needs a data URI at or under {RUNWAY_MAX_VIDEO_DATA_URI_BYTES} \
         bytes (this one would be {data_uri_len}), and Runway's upload endpoint \
         accepts {RUNWAY_MIN_UPLOAD_BYTES}-{RUNWAY_MAX_UPLOAD_BYTES} bytes"
    )))
}

/// The `POST /v1/uploads` step-1 body — **two fixed literals, and nothing
/// caller-derived** (T-56-INJ-01).
///
/// `filename` is a `&'static str` and not a `String` for exactly the reason
/// [`RunwayReferenceImage`]'s `tag` is: the only value Rudis ever sends is
/// [`RunwayUploadRequest::EPHEMERAL_CLIP`], so there is no caller path that can
/// inject text into a request field. Passing the user's real clip name through
/// here would put filesystem-derived (and therefore prompt-reachable) text into
/// a request for zero capability — Runway does not key anything Rudis needs off
/// this name.
///
/// `type: "ephemeral"` is the 24 h-expiry class (56-RESEARCH § Q2) — the
/// shortest-lived option, chosen deliberately: a source clip is a
/// single-generation input and Rudis has no reason to leave a user's own
/// footage on a third party's storage any longer than the call needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RunwayUploadRequest {
    pub filename: &'static str,
    /// `r#type` because `type` is a Rust keyword; serde strips the raw
    /// identifier prefix, so the wire key is `"type"`.
    pub r#type: &'static str,
}

impl RunwayUploadRequest {
    /// The ONE step-1 body this module ever sends.
    pub const EPHEMERAL_CLIP: Self = Self {
        filename: "rudis-clip.mp4",
        r#type: "ephemeral",
    };
}

/// Gate the presigned `uploadUrl` before Rudis POSTs a user's own footage at it
/// (T-56-SSRF-02).
///
/// The value is server-issued and arrives inside an authenticated, TLS-verified
/// response from a pinned host, so it is in the same trust class as the output
/// download URL [`validate_output_uri`] guards — and this mirrors that
/// function's posture deliberately, including the reasoning that the marginal
/// case is Runway's own API being compromised or buggy rather than an
/// attacker-chosen URL. The check is cheap and it closes the `file://`-shaped
/// hole, which matters more here than it does for a download: this request
/// carries the user's footage OUT.
///
/// Shape-based only, for the same reason `validate_output_uri` is: a
/// one-observation host pin would break on a CDN/bucket rotation and would
/// admit every other AWS customer's bucket anyway.
pub fn validate_upload_url(url: &str) -> Result<(), GenError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| GenError::Provider("Runway upload URL is not a valid URL".to_string()))?;
    if parsed.scheme() != "https" {
        return Err(GenError::Provider(format!(
            "Runway upload URL is not HTTPS: scheme '{}'",
            parsed.scheme()
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(GenError::Provider(
            "Runway upload URL embeds credentials".to_string(),
        ));
    }
    // The error names the HOST only — never echoes a full untrusted URL, whose
    // query string is capability-bearing on a presigned target.
    let host = parsed
        .host_str()
        .ok_or_else(|| GenError::Provider("Runway upload URL has no host".to_string()))?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return Err(GenError::Provider(format!(
            "Runway upload URL host is a bare IP literal: {host}"
        )));
    }
    if !bare.contains('.') {
        return Err(GenError::Provider(format!(
            "Runway upload URL host is not a dotted domain: {host}"
        )));
    }
    Ok(())
}

/// Where a keyframe sits in the generated clip.
///
/// A CLOSED enum, never a free string: `position` is the field that decides
/// whether an A→B transition plays forwards or backwards, and a typo'd
/// `"frist"` would be silently accepted by a `String` and rejected (or worse,
/// ignored) server-side. Serializes lowercase — `"first"` / `"last"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunwayKeyframePosition {
    First,
    Last,
}

/// One entry of the ARRAY form of `promptImage` — `{uri, position}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunwayKeyframe {
    pub uri: RunwayDataUri,
    pub position: RunwayKeyframePosition,
}

/// Runway's polymorphic `promptImage` field, modelled as a CLOSED enum so the
/// single-frame and first/last forms are **distinct at the type level and
/// cannot be confused**.
///
/// `#[serde(untagged)]` reproduces Runway's own polymorphism: `Single`
/// serializes as a bare JSON string, `Keyframes` as a JSON array of objects.
/// Both arms carry [`RunwayDataUri`], so neither can smuggle a caller-supplied
/// host.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum RunwayPromptImage {
    /// A single first frame (image→video conditioning).
    Single(RunwayDataUri),
    /// Positioned keyframes — the first/last PAIR that carries the A→B
    /// transition across from Veo's `instances[].lastFrame` (quick 260726-t5z).
    Keyframes(Vec<RunwayKeyframe>),
}

/// The `POST /v1/image_to_video` (and, with `prompt_image: None`, the
/// `POST /v1/text_to_video`) request body.
///
/// **Structurally carries no URL/endpoint field** — the base URL is a `const`
/// on the module, and the only string that could resemble one is a
/// [`RunwayDataUri`] this crate built itself. `promptImage` is skip-serialized
/// when absent, never sent as `null` — the same discipline `VeoInstance::image`
/// carries, and the reason a text-only request is byte-minimal.
///
/// **There is deliberately NO `references` field this wave.** On `seedance2`
/// the first/last keyframes and the `references` array are MUTUALLY EXCLUSIVE
/// (42.1-RESEARCH (b) update), so adding one is a decision that must be taken
/// together with the intent table, not incidentally. The exhaustive-destructure
/// tripwire in this module's tests fails to compile if a field is added,
/// forcing that review.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunwayImageToVideoRequest {
    /// The provider-side model id (`"gen4_turbo"`, `"veo3.1_fast"`, …). Travels
    /// in the BODY, not the URL — so unlike Veo's path-interpolated model id it
    /// cannot influence the request target at all.
    pub model: String,
    #[serde(rename = "promptImage", skip_serializing_if = "Option::is_none")]
    pub prompt_image: Option<RunwayPromptImage>,
    /// Optional for most models, **REQUIRED for `gen4_turbo`**
    /// (42.1-RESEARCH (b)). Rudis always has a prompt, so it is always sent.
    #[serde(rename = "promptText")]
    pub prompt_text: String,
    pub ratio: &'static str,
    pub duration: u32,
}

// ---------------------------------------------------------------------------
// Pure helpers — no `Client`, no network. The SAME code the HTTP path calls,
// which is what makes every test here hermetic without a mock HTTP server.
// ---------------------------------------------------------------------------

/// Inline real PNG bytes as a `data:image/png;base64,…` URI — the ONLY
/// constructor of a [`RunwayDataUri`], and therefore the only way any
/// `promptImage` value can come into existence.
///
/// The prefix is a fixed literal (see [`RUNWAY_DATA_URI_PREFIX`]); the base64
/// engine is the crate-standard `STANDARD` (padded) alphabet, the same one
/// `veo::build_predict_request` uses for `bytesBase64Encoded`.
pub fn data_uri(bytes: &[u8]) -> RunwayDataUri {
    RunwayDataUri(format!(
        "{RUNWAY_DATA_URI_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Bound every inlined frame against Runway's documented 5 MB data-URI cap
/// BEFORE any egress, so an oversized reference fails locally and cheaply
/// instead of burning a round trip on a server-side 400.
///
/// The error names only the LENGTH — never the URI itself, which is megabytes
/// of base64 and would flood any log it reached.
pub fn validate_data_uri_sizes(req: &RunwayImageToVideoRequest) -> Result<(), GenError> {
    match &req.prompt_image {
        None => Ok(()),
        Some(RunwayPromptImage::Single(uri)) => check_data_uri_size(uri),
        Some(RunwayPromptImage::Keyframes(frames)) => {
            frames.iter().try_for_each(|k| check_data_uri_size(&k.uri))
        }
    }
}

/// The same 5 MB bound for the STILL-IMAGE body's tagged `referenceImages`.
///
/// A separate entry point rather than one generic function because the two
/// bodies have no shared trait and inventing one would be more machinery than
/// the check; both delegate to the ONE [`check_data_uri_size`] below, so the
/// bound itself exists exactly once and the image path cannot end up with a
/// looser cap than the video path.
pub fn validate_image_data_uri_sizes(req: &RunwayTextToImageRequest) -> Result<(), GenError> {
    match &req.reference_images {
        None => Ok(()),
        Some(refs) => refs.iter().try_for_each(|r| check_data_uri_size(&r.uri)),
    }
}

/// The single 5 MB bound, shared by both bodies.
fn check_data_uri_size(uri: &RunwayDataUri) -> Result<(), GenError> {
    if uri.len() > RUNWAY_MAX_DATA_URI_BYTES {
        return Err(GenError::InvalidRequest(format!(
            "an inlined conditioning frame is {} bytes, over Runway's \
             {RUNWAY_MAX_DATA_URI_BYTES}-byte data-URI cap",
            uri.len()
        )));
    }
    Ok(())
}

/// Build the `image_to_video` / `text_to_video` body — the twin of
/// `veo::build_predict_request` was (retired in 42.1-02), and the
/// ONE place a [`ReferenceImage`] becomes a `promptImage`.
///
/// Four request shapes, decided ENTIRELY by which frames the caller supplied —
/// exactly the four `build_predict_request` already produces, re-expressed in
/// Runway's vocabulary:
///
/// | `reference` | `destination` | `promptImage` |
/// | --- | --- | --- |
/// | `None` | `None` | omitted (a text→video request) |
/// | `Some` | `None` | [`Single`](RunwayPromptImage::Single) — a first frame |
/// | `Some` | `Some` | [`Keyframes`](RunwayPromptImage::Keyframes)`[first, last]` |
/// | `None` | `Some` | `Keyframes[last]` — interpolate INTO a target |
///
/// **Order is fixed here and nowhere else.** `reference` is ALWAYS the `First`
/// keyframe and `destination` ALWAYS the `Last`, so a caller cannot silently
/// invert a transition — the same guard `build_predict_request` carries for
/// `image`/`lastFrame`, and the reason this module's tests assert the pair is
/// not swapped rather than merely present. An inverted transition would
/// generate the move backwards and would look entirely plausible in review.
///
/// Pure: no `Client`, no network, no key. `req.background` has no analogue on
/// Runway's video surface and is ignored, exactly as `VeoProvider` ignores it.
pub fn build_image_to_video_request(
    model: &str,
    prompt: String,
    reference: Option<&ReferenceImage>,
    destination: Option<&ReferenceImage>,
) -> RunwayImageToVideoRequest {
    // ONE encoder for both slots — a second hand-rolled copy is how the two
    // frames would drift apart (different prefix, different base64 engine).
    let encode = |r: &ReferenceImage| data_uri(&r.bytes);
    let prompt_image = match (reference, destination) {
        (None, None) => None,
        (Some(first), None) => Some(RunwayPromptImage::Single(encode(first))),
        (first, last) => {
            let mut frames = Vec::with_capacity(2);
            // Pushed in this order deliberately: `first` before `last`, so the
            // array reads the way the clip plays.
            if let Some(f) = first {
                frames.push(RunwayKeyframe {
                    uri: encode(f),
                    position: RunwayKeyframePosition::First,
                });
            }
            if let Some(l) = last {
                frames.push(RunwayKeyframe {
                    uri: encode(l),
                    position: RunwayKeyframePosition::Last,
                });
            }
            Some(RunwayPromptImage::Keyframes(frames))
        }
    };
    RunwayImageToVideoRequest {
        model: model.to_string(),
        prompt_image,
        prompt_text: prompt,
        ratio: RUNWAY_RATIO,
        duration: RUNWAY_DURATION_SECONDS,
    }
}

// ---------------------------------------------------------------------------
// The ADVISORY model roster.
//
// HISTORICAL NOTE (Phase 55.1, plan 06 — the deletion this comment replaces).
// Until 55.1 this header introduced an intent -> model map, and that table WAS
// the cost policy: a caller named a capability ("transition", "cheap-draft", …)
// and the table chose the model, so a default could never silently land on the
// 7.2x tier. Open model selection (D-01/D-02) retired the whole indirection —
// the caller now names a Runway model id as free text and it reaches the wire
// verbatim — so the capability enum, the map, its lookup function and the
// expensive-transition fallback const were deleted here rather than left beside
// the free-text path as a second source of truth.
//
// Their exact identifiers are DELIBERATELY not spelled anywhere in `crates/*`
// (the discipline 55.1-02 established for this same record): a grep for a name
// that no longer exists should come back empty, or the next reader concludes it
// is still there. They are written out once, in
// `.planning/phases/55.1-…/55.1-06-SUMMARY.md`, and the risk accepted by
// deleting them is recorded in PROVENANCE.md Entry 17's 55.1 amendment.
//
// What replaced each job, so the loss is legible rather than implied:
//   * choosing the model            -> the caller's `model` field (agent-tools)
//   * keeping a default cheap       -> `spend_confirmation_gate`'s per-call price
//                                      (`cost_signal_for_model`, app-core), plus
//                                      the rulebook's cost table
//   * refusing an incapable model   -> Runway's own 400 (D-05)
// What is genuinely LOST is the compile-time guarantee that no default could
// reach an uncosted or expensive model. That is D-01's accepted risk, recorded
// in PROVENANCE.md Entry 17's 55.1 amendment, not an oversight a test can cover.
//
// `RUNWAY_MODELS` below SURVIVES, demoted: it gates nothing and refuses nothing.
// Its only production readers are advisory — the spend prompt's price
// (`model_caps`), the disclosure label (`model_label`), `catalog()` and the
// provenance-watermark flag.
// ---------------------------------------------------------------------------

/// 42.1-RESEARCH gate (a), answered **NO**: no current Runway model exposes
/// structured 6-axis camera control (pan/tilt/roll/zoom) as API parameters.
/// The request bodies are `model` + `promptImage`/`promptText` + `ratio` +
/// `duration` and nothing else; Camera Control was a **Gen-3 Alpha Turbo**
/// product feature and `gen3a_turbo` is deprecated (see
/// [`RUNWAY_DEPRECATION_SUNSET`]), while Gen-4's "Director Mode" is a
/// node-based UI, not an API surface.
///
/// **Consequence, and it must be told to the user rather than implied away:** a
/// deliberate camera move is delivered by PROMPT LANGUAGE (the five-part
/// Cinematography line), which is strictly less precise than numeric axes. Phase
/// 42.1 said that through a `camera-move` capability word; since Phase 55.1
/// (D-11) there is no such word — the guidance lives in the agent's prose
/// instead (`rulebook_v3.md` and the `video_transitions` / `vfx` playbooks),
/// which is where it always did the work, because this flag never gated any
/// behaviour.
///
/// It is kept for the same reason [`RUNWAY_RATIO_IS_UNIVERSAL`] is: it is a
/// 42.1-04 probe ANSWER, pinned by `live_probe_2026_07_27_answers_are_pinned` so
/// that a later edit which "corrects" it back to a guess has to argue with a
/// failing test.
///
/// ## VERIFIED LIVE 2026-07-27 (42.1-04) — the gate is CLOSED, and the naive
/// ## version of this probe would have got the WRONG answer
///
/// Plan 04 expected "a 400 on an unknown key is itself the answer". **It is
/// not.** Runway's Zod validator is NOT strict: an unrecognised key is silently
/// **stripped**, never reported. Sending `cameraControl` and getting no error
/// therefore proves nothing on its own — it is exactly what a *working* field
/// would also look like.
///
/// The discriminating probe is a **wrong-typed** field. A field that exists
/// raises `invalid_type` at its own `path`; a field that does not exist is
/// stripped and stays silent. With `promptText: 12345` as the positive control
/// (which duly raised `{code: "invalid_type", path: ["promptText"]}`, so the
/// method is non-vacuous), all five plausible spellings —
/// `cameraControl`, `motion`, `cameraMotion`, `camera`, `cameraMovement` —
/// were sent wrong-typed to `gen4.5` and **every one was silently dropped**.
///
/// So: no camera field exists under any of those names, `false` is correct, and
/// `camera-move` remains prompt language. The *general* lesson is worth more
/// than the answer: against this API, a speculative field never fails loudly,
/// so any future "does Runway support X?" question must be asked by type, not
/// by presence.
pub const RUNWAY_HAS_STRUCTURED_CAMERA_CONTROL: bool = false;

/// The date Runway retires the models flagged
/// [`deprecated`](ModelCaps::deprecated) below. Present so the guard test can
/// name it: after this date those ids stop existing server-side, and an intent
/// row pointing at one is a silent total outage, not a degraded result.
pub const RUNWAY_DEPRECATION_SUNSET: &str = "2026-07-30";

// The expensive-transition fallback const (= "seedance2") was DELETED by Phase
// 55.1 plan 06. It named the EXPENSIVE floor the transition capability could
// fall back to if no cheap model carried the first/last `promptImage` pair.
// 42.1-04 proved the $0.40 `veo3.1_fast` carries it, so the fallback was never
// used; and with the intent table gone there is no routing decision left for a
// fallback to serve. The capability fact it guarded survives, per-model and
// pinned by literal, in `live_probe_2026_07_27_answers_are_pinned` and in each
// row's `keyframe_pair: KeyframePairSupport` — which is what the rulebook's
// "pair YES" column is written from.

/// How well a model is known to accept the first/last `promptImage` PAIR — the
/// single fact the A→B transition depends on, and the one 42.1-RESEARCH could
/// not fully settle documentarily.
///
/// Three states rather than a `bool`, because "we have not checked" and "the
/// docs say no" are different risks and must not collapse into one another.
/// **Every video row below is now LIVE-VERIFIED** (42.1-04, 2026-07-27) by the
/// zero-spend probe in `scripts/runway-capability-probe.mjs`. See that script's
/// header for the poison-pill technique that makes an acceptance test cost $0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyframePairSupport {
    /// The pair is accepted. Originally "Runway's docs say so"
    /// (42.1-RESEARCH (b), `seedance2` only); since 42.1-04 this also means
    /// **observed live**: a `promptImage: [{first},{last}]` submitted with a
    /// poisoned `ratio`/`duration` came back with ONLY the poison issues and
    /// no `promptImage` issue, which is Runway's Zod validator saying the
    /// shape parsed.
    Documented,
    /// Plausible but UNVERIFIED. **This state currently has no video
    /// inhabitants** — 42.1-04 probed every roster video model and moved them
    /// all to `Documented` or `Unsupported`. It is retained because it is the
    /// correct state for any model added later and not yet probed: the whole
    /// point of the three-state flag is that a new row cannot default into
    /// looking checked. An intent row MAY sit here; it may never sit on
    /// `Unsupported`.
    Presumed,
    /// The pair is REJECTED. Verified live for `gen4_turbo`, `gen4.5` and
    /// `veo3`: `path: ["promptImage"]`, whose array branch is
    /// `too_big: {maximum: 1, exact: true}` with `position` pinned to
    /// `"first"` — i.e. those models accept at most ONE keyframe and it must
    /// be the first. A last frame is structurally inexpressible, not merely
    /// undocumented. Also the correct state for a structurally inapplicable
    /// model (still-image, or video-to-video).
    Unsupported,
}

/// The per-model facts the advisory roster carries. Every field traces to a
/// 42.1-RESEARCH line (or a 42.1-04 live probe), not to intuition.
///
/// **Phase 55.1 (D-01): advisory, never gating.** These flags used to be read by
/// `build_submission` to REFUSE a mismatched request and by the intent table to
/// choose a model; both readers are gone. What is left reads them to *describe*
/// — the spend prompt's price, the disclosure's label, `catalog()`, the
/// provenance-watermark flag — and a capability mismatch is now Runway's own 400
/// (D-05). A new reader that refuses on one of these fields would rebuild the
/// closed roster this phase retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCaps {
    /// The [`ModelInfo::modality`](crate::ModelInfo::modality) word this model
    /// surfaces as — `"video"` or `"image"`.
    pub modality: &'static str,
    /// `POST /v1/text_to_image` accepts this model.
    pub text_to_image: bool,
    /// `POST /v1/text_to_video` accepts this model.
    pub text_to_video: bool,
    /// `POST /v1/image_to_video` accepts this model.
    pub image_to_video: bool,
    /// `POST /v1/video_to_video` accepts this model (edits an EXISTING clip —
    /// never the A→B bridge between two frames).
    pub video_to_video: bool,
    /// Whether the first/last keyframe pair works here. See
    /// [`KeyframePairSupport`].
    pub keyframe_pair: KeyframePairSupport,
    /// `promptText` is REQUIRED, not optional (true for `gen4_turbo` and the
    /// image models). Checked locally by [`validate_request`] so an empty
    /// prompt fails here rather than burning a round trip on a 400.
    pub prompt_text_required: bool,
    /// **The constraint to encode, not discover later** (plan 42.1-01's Risk
    /// section): on this model the first/last keyframes and the `references`
    /// array are MUTUALLY EXCLUSIVE — never both in one call. A future "match
    /// this look while bridging" ask needs two calls or a different model. This
    /// wave models no `references` field at all, and the request tripwire test
    /// keeps it that way, which is the whole enforcement now that no local table
    /// picks the model (Phase 55.1).
    pub pair_excludes_references: bool,
    /// GEN-09: Runway publishes C2PA Content Credentials on generated output,
    /// and the Veo-family models additionally carry SynthID — so the honest
    /// answer is `true` across the roster. Rides the job-completion event to a
    /// user-facing disclosure, exactly as the Veo tiers' SynthID flag does.
    pub carries_provenance_watermark: bool,
    /// Whether this model accepts the TAGGED `referenceImages: [{uri, tag}]`
    /// array on `POST /v1/text_to_image`, cited as `@Tag` inside `promptText`
    /// (42.1-RESEARCH (b) update — documented for `gen4_image` specifically).
    ///
    /// A per-model fact, not a modality-wide one: RESEARCH names `gen4_image`
    /// and says nothing about `gen4_image_turbo`, so the turbo row stays
    /// `false`. Unknown is treated as NO — [`build_submission`] refuses a
    /// reference rather than posting a field the model may not read, which
    /// would be the silent-degradation failure this module exists to avoid.
    pub tagged_reference_images: bool,
    /// Retired, or retiring at [`RUNWAY_DEPRECATION_SUNSET`]. Present in the
    /// roster ONLY so the guard below is non-vacuous.
    pub deprecated: bool,
    /// Cost in **US cents** for the pinned [`RUNWAY_DURATION_SECONDS`] clip
    /// (per image, for the image models). Integer cents, never a float — this
    /// number is compared, not displayed. `None` = 42.1-RESEARCH (e) did not
    /// cost this model, which is itself a reason not to route a default to it.
    pub cost_cents_per_4s: Option<u32>,
    /// The free-form affordability hint [`ModelInfo::rough_cost_signal`](crate::ModelInfo::rough_cost_signal)
    /// carries. Never parsed.
    pub rough_cost_signal: Option<&'static str>,
}

// Named caps consts, written ONCE and shared by both tables below, so the
// roster and the intent map cannot drift apart (a test asserts they are equal
// for every intent row).

const CAPS_GEN4_TURBO: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: false,
    image_to_video: true,
    video_to_video: false,
    // Runway's Gen-4 Video guide: Gen-4 "does not currently support keyframes".
    // VERIFIED LIVE 2026-07-27 (42.1-04 probe `pair-detail-gen4_turbo`): the
    // `promptImage` array branch is `too_big {maximum: 1, exact: true}` with
    // `position` pinned to `"first"`. A last frame cannot be expressed at all.
    keyframe_pair: KeyframePairSupport::Unsupported,
    prompt_text_required: true,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    cost_cents_per_4s: Some(20),
    rough_cost_signal: Some("~$0.20 / 4s clip (5 credits/s)"),
};

const CAPS_GEN4_5: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: true,
    image_to_video: true,
    video_to_video: false,
    // VERIFIED LIVE 2026-07-27 (42.1-04 probe `pair-detail-gen4.5`): identical
    // rejection to `gen4_turbo` — one keyframe, `position: "first"` only.
    keyframe_pair: KeyframePairSupport::Unsupported,
    // NOTE (42.1-04): `promptText` came back REQUIRED on the live
    // `image_to_video` branch for this model ("expected string, received
    // undefined"), though NOT on `text_to_video`. This flag is read by
    // `build_submission` for the text-required check; it stays `false` because
    // every Rudis caller path supplies a prompt (the tool schema makes it
    // mandatory), so tightening it would reject nothing real while making the
    // per-endpoint asymmetry inexpressible in a per-MODEL flag. Recorded here
    // rather than silently absorbed.
    prompt_text_required: false,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    cost_cents_per_4s: Some(48),
    rough_cost_signal: Some("~$0.48 / 4s clip (12 credits/s)"),
};

const CAPS_VEO3: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: true,
    image_to_video: true,
    video_to_video: false,
    // VERIFIED LIVE 2026-07-27 (42.1-04 probe `pair-veo3`): REJECTED, with the
    // same one-keyframe-`first`-only `promptImage` shape as the Gen-4 family.
    // The Veo family is NOT uniform: veo3 refuses the pair while veo3.1 and
    // veo3.1_fast accept it. (`duration` is also pinned to exactly 8 here.)
    keyframe_pair: KeyframePairSupport::Unsupported,
    prompt_text_required: false,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    cost_cents_per_4s: None,
    rough_cost_signal: None,
};

const CAPS_VEO3_1: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: true,
    image_to_video: true,
    video_to_video: false,
    // Rudis had already proven `lastFrame` on Google's own Veo 3.1 surface
    // (quick 260726-t5z), but "Veo via Runway" is a DIFFERENT wire contract.
    // VERIFIED LIVE 2026-07-27 (42.1-04 probe `pair-veo3.1`): ACCEPTED — the
    // pair produced no `promptImage` issue while the poisoned ratio/duration
    // both fired, so the array branch parsed. The capability really does
    // survive the migration on the Veo 3.1 tier specifically.
    keyframe_pair: KeyframePairSupport::Documented,
    prompt_text_required: false,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    cost_cents_per_4s: None,
    rough_cost_signal: None,
};

const CAPS_VEO3_1_FAST: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: true,
    image_to_video: true,
    video_to_video: false,
    // **THE ROW THE WHOLE PHASE HUNG ON.** VERIFIED LIVE 2026-07-27 (42.1-04
    // probe `pair-veo3.1_fast`): ACCEPTED. The A→B transition shipped in quick
    // 260726-t5z survives the migration off direct Veo at the $0.40 tier — the
    // $1.44 seedance2 fallback was not needed. (Both the fallback const and the
    // intent row that would have used it were deleted by Phase 55.1 plan 06;
    // this fact now reaches the agent as the rulebook's "pair YES" column.)
    keyframe_pair: KeyframePairSupport::Documented,
    prompt_text_required: false,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    cost_cents_per_4s: Some(40),
    rough_cost_signal: Some("~$0.40 / 4s clip (10 credits/s, no audio)"),
};

const CAPS_SEEDANCE2: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: true,
    image_to_video: true,
    video_to_video: false,
    keyframe_pair: KeyframePairSupport::Documented,
    prompt_text_required: false,
    // The mutual exclusion is documented on THIS model specifically.
    pair_excludes_references: true,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    cost_cents_per_4s: Some(144),
    rough_cost_signal: Some("~$1.44 / 4s clip (36 credits/s) — 7.2x the baseline"),
};

const CAPS_SEEDANCE2_MINI: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: true,
    image_to_video: true,
    video_to_video: false,
    // VERIFIED LIVE 2026-07-27 (42.1-04 probe `pair-seedance2_mini`): ACCEPTED,
    // like its full-size sibling. It WAS "recorded, not reachable" — un-cleared
    // by GEN-08, and named by no intent row — until 55.1-02 ungated the provider
    // (D-01) and 55.1-03 gave the caller the `model` field. It is reachable now,
    // and its $0.64 (3.2x the baseline) is the spend gate's problem, not the
    // allow list's.
    keyframe_pair: KeyframePairSupport::Documented,
    prompt_text_required: false,
    pair_excludes_references: true,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    cost_cents_per_4s: Some(64),
    rough_cost_signal: Some("~$0.64 / 4s clip (16 credits/s)"),
};

const CAPS_ALEPH2: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: false,
    image_to_video: false,
    // video_to_video ONLY: aleph2 EDITS an existing clip (relight, background
    // swap, cleanup) with up to 5 guiding images. That is a genuinely useful
    // capability Rudis lacks, but it is a SEPARATE future intent operating on
    // ONE existing clip — never the A→B bridge between two frames.
    video_to_video: true,
    keyframe_pair: KeyframePairSupport::Unsupported,
    prompt_text_required: false,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    // CORRECTED 2026-08-01 (quick 260801-r7s): shipped as `Some(56)` / "~$0.56 /
    // 4s" — HALF the real price. Root cause: Runway's pricing page carries both
    // `'aleph2': 28` (credits/s) and `minimumCredits: {'aleph2': 56}`, and the 56
    // — the 2s-minimum charge, in CREDITS — was transplanted into this CENTS
    // field. At the published 28 credits/s and 1 credit = $0.01
    // (docs.dev.runwayml.com/guides/pricing, re-confirmed 2026-08-01;
    // 56-01-PROBE-RESULTS.md § Pricing facts): 4s = 112 credits = $1.12. This
    // field feeds spend_confirmation_gate — the halving under-quoted the most
    // expensive per-second model on the roster.
    //
    // WHAT THIS NUMBER IS, AND IS NOT — Phase 56 plan 05, and it matters here
    // more than on any other row. Every OTHER video row is quoted for the one
    // clip length those endpoints produce (RUNWAY_DURATION_SECONDS), so "the
    // price of a 4s clip" is very nearly "the price". `aleph2` is the exception:
    // `video_to_video` edits a clip the USER already has, so the billable length
    // is an INPUT the caller chooses anywhere inside
    // RUNWAY_V2V_INPUT_MIN_SECONDS..=RUNWAY_V2V_INPUT_MAX_SECONDS. The real bill
    // is `max(28 x input_seconds, 56)` cents — 2s costs the 56c minimum, 4s costs
    // the 112c below, and 30s costs 840c ($8.40), which is 7.5x this figure.
    //
    // So 112 is a 4-SECOND SAMPLE of a per-second rate, not a flat clip price,
    // and anything quoting it as the cost of an edit is under-quoting every
    // range longer than 4s. The per-call estimate is
    // `app_core::estimated_video_edit_cost_cents`, which derives from
    // `app_core::RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND` (28) and
    // `RUNWAY_ALEPH2_MINIMUM_CENTS` (56); an app-core test pins both against
    // this row so the two cannot drift, and
    // `every_costed_video_row_satisfies_cents_equals_credits_per_second_times_4`
    // pins THIS row against the published rate. Keeping 112 here is deliberate:
    // it is what makes that invariant, the rulebook's 5.6x ratio and the
    // cross-model comparison all read on one consistent 4-second basis.
    cost_cents_per_4s: Some(112),
    rough_cost_signal: Some("~$1.12 / 4s clip (28 credits/s) — video-to-video only"),
};

const CAPS_GEN4_IMAGE: ModelCaps = ModelCaps {
    modality: "image",
    text_to_image: true,
    text_to_video: false,
    image_to_video: false,
    video_to_video: false,
    keyframe_pair: KeyframePairSupport::Unsupported,
    prompt_text_required: true,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: true,
    deprecated: false,
    cost_cents_per_4s: Some(8),
    rough_cost_signal: Some("~$0.05-0.08 / image (5-8 credits)"),
};

const CAPS_GEN4_IMAGE_TURBO: ModelCaps = ModelCaps {
    modality: "image",
    text_to_image: true,
    text_to_video: false,
    image_to_video: false,
    video_to_video: false,
    keyframe_pair: KeyframePairSupport::Unsupported,
    prompt_text_required: true,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: false,
    cost_cents_per_4s: Some(2),
    rough_cost_signal: Some("~$0.02 / image (2 credits)"),
};

/// Shared shape for the two retired ids. They exist in the roster for exactly
/// one reason: so the exclusion guards have something real to reject, instead of
/// passing vacuously. Since Phase 55.1 plan 06 those guards are
/// `catalog_lists_every_non_deprecated_roster_model` and
/// `the_roster_is_internally_consistent` (the intent-table guard that used to be
/// the primary one, `no_intent_row_names_a_deprecated_model`, died with the
/// table it checked).
const CAPS_DEPRECATED: ModelCaps = ModelCaps {
    modality: "video",
    text_to_image: false,
    text_to_video: false,
    image_to_video: false,
    video_to_video: false,
    keyframe_pair: KeyframePairSupport::Unsupported,
    prompt_text_required: false,
    pair_excludes_references: false,
    carries_provenance_watermark: true,
    tagged_reference_images: false,
    deprecated: true,
    cost_cents_per_4s: None,
    rough_cost_signal: None,
};

/// Every model id 42.1-RESEARCH's roster names — `(id, human label, caps)`.
///
/// The two `deprecated` rows are DELIBERATELY present: a guard that can only
/// reject ids nobody listed is a guard that never fires.
///
/// # ADVISORY ONLY since Phase 55.1 (D-01) — this table refuses nothing
///
/// Being absent from this list is **not** an error and never blocks a call. Any
/// Runway model id the caller names is submitted verbatim; 55.1-01 stopped
/// `build_submission` consulting the table, 55.1-02 stopped the allow list
/// consulting it, and plan 06 deleted the intent map that used to *choose* from
/// it. Its four remaining jobs are all descriptive:
///
/// | Reader | Uses it for |
/// | --- | --- |
/// | `app_core::cost_signal_for_model` | the price the spend confirmation quotes |
/// | [`model_label`] | the human name in the "Model that ran" disclosure |
/// | [`RunwayProvider::list_models`] via `catalog()` | GEN-07's model list |
/// | `ModelCaps::carries_provenance_watermark` | the GEN-09 disclosure flag |
///
/// An off-roster id is therefore a normal outcome, not a failure: it prices as
/// `app_core::PRICE_UNKNOWN` and discloses as its bare id.
pub const RUNWAY_MODELS: &[(&str, &str, ModelCaps)] = &[
    ("gen4_turbo", "Runway Gen-4 Turbo", CAPS_GEN4_TURBO),
    ("gen4.5", "Runway Gen-4.5", CAPS_GEN4_5),
    ("veo3", "Google Veo 3 (via Runway)", CAPS_VEO3),
    ("veo3.1", "Google Veo 3.1 (via Runway)", CAPS_VEO3_1),
    ("veo3.1_fast", "Google Veo 3.1 Fast (via Runway)", CAPS_VEO3_1_FAST),
    ("seedance2", "ByteDance Seedance 2 (via Runway)", CAPS_SEEDANCE2),
    ("seedance2_mini", "ByteDance Seedance 2 Mini (via Runway)", CAPS_SEEDANCE2_MINI),
    ("aleph2", "Runway Aleph 2 (video-to-video)", CAPS_ALEPH2),
    ("gen4_image", "Runway Gen-4 Image", CAPS_GEN4_IMAGE),
    ("gen4_image_turbo", "Runway Gen-4 Image Turbo", CAPS_GEN4_IMAGE_TURBO),
    // --- retired; sunset RUNWAY_DEPRECATION_SUNSET ---
    ("gen3a_turbo", "Runway Gen-3 Alpha Turbo (RETIRED)", CAPS_DEPRECATED),
    ("gen4_aleph", "Runway Gen-4 Aleph (RETIRED)", CAPS_DEPRECATED),
];

// The five-variant capability enum (transition / camera-move / cheap-draft /
// photoreal / image), its wire-word impl, the intent -> model map and the map's
// lookup function all stood here until Phase 55.1 plan 06 DELETED them. See the
// roster header above for what took over each of their jobs, and for why their
// identifiers are not spelled out. Nothing in `crates/*` resolves a capability
// word to a model any more: the caller names the model.

/// Look up a model id in the roster. An id absent here is simply UNKNOWN TO
/// RUDIS — since Phase 55.1 (D-01) that is an ordinary, submittable case, and
/// callers must treat `None` as "no advisory information", never as a refusal.
pub fn model_caps(model_id: &str) -> Option<ModelCaps> {
    RUNWAY_MODELS
        .iter()
        .find(|(id, _, _)| *id == model_id)
        .map(|(_, _, caps)| *caps)
}

/// The roster entry whose price the CLIP-EDIT (`video_to_video`) path can quote
/// — Phase 56 (GEN-11), post-55.1.
///
/// # ADVISORY DEFAULT ONLY. This does NOT gate anything.
///
/// Any free-text model id submits to the `video_to_video` endpoint regardless of
/// what this returns (55.1 owner decision 1, and [`RUNWAY_MODELS`]' own header):
/// this function refuses nothing, clears nothing and is consulted by no submit
/// path. It names the one roster row whose price the spend confirmation is able
/// to quote, for use when a tool call names no model of its own — and an id it
/// does not name is priced `app_core::PRICE_UNKNOWN` and submitted anyway.
///
/// **Endpoint selection follows the FRAMES the call carries, never the model
/// id.** [`RunwayProvider::submit`] dispatches on `GenRequest::source_video`
/// being present, and [`build_video_edit_submission`] is reachable only by a
/// caller already holding a [`RunwayVideoInput`], i.e. only with source-clip
/// bytes. Naming this model does not route anything here, and NOT naming it does
/// not route anything away.
///
/// Derived from the roster's `video_to_video` caps flag rather than written as a
/// literal, so the id lives in exactly one place. The paired test pins the
/// literal from the other direction, and a second test asserts the roster
/// currently has exactly ONE such row — because "the first match" only names a
/// decision while there is nothing else to match.
pub fn advisory_video_edit_model() -> Option<&'static str> {
    RUNWAY_MODELS
        .iter()
        .find(|(_, _, caps)| caps.video_to_video)
        .map(|(id, _, _)| *id)
}

/// The human label for a roster model — [`ModelInfo::label`](crate::ModelInfo::label).
pub fn model_label(model_id: &str) -> Option<&'static str> {
    RUNWAY_MODELS
        .iter()
        .find(|(id, _, _)| *id == model_id)
        .map(|(_, label, _)| *label)
}

// `suggested_ext_for_model` was DELETED by Phase 55.1 (D-01/D-05). It resolved
// the output extension by looking the model up in `RUNWAY_MODELS` and reading
// `caps.modality` — precisely the model-derived reasoning this phase removes,
// and it returned `None` (then panicked at its one call site's `.expect`) for
// any off-roster id. `build_submission` was its only reader; the extension now
// comes from the caller's `RequestModality`, which is still a FIXED literal
// chosen before the request goes out and never derived from a response body, a
// `Content-Type` or an output URL's path (T-31-12 intact).

// ---------------------------------------------------------------------------
// The still-image request, and the two-arm body every submit posts.
// ---------------------------------------------------------------------------

/// One entry of `gen4_image`'s tagged `referenceImages` array — `{uri, tag}`.
///
/// `tag` is a `&'static str` and not a `String` **on purpose**: the only value
/// Rudis ever sends is [`RUNWAY_REFERENCE_TAG`], so there is no caller path that
/// can inject a tag, and therefore no caller path that can inject the matching
/// `@…` token into `promptText` either. Same structural discipline as
/// [`RunwayDataUri`]: make the dangerous form unrepresentable rather than
/// merely untested.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunwayReferenceImage {
    pub uri: RunwayDataUri,
    pub tag: &'static str,
}

/// The `POST /v1/text_to_image` body.
///
/// # 42.1-03: `referenceImages` is now built (the Wave-2 hand-off, closed)
///
/// 42.1-01 deliberately did not build this field and 42.1-02 therefore shipped
/// a sketch-conditioned `generate_ai_image` that FAILED — loudly, but a shipped
/// v5 capability all the same. The blocker Plan 01 recorded was narrow and
/// precise: *"whether an UNCITED tag is honoured is exactly the kind of question
/// only a live call answers."*
///
/// That question is dissolved rather than answered, and offline: Rudis never
/// sends an uncited tag. [`build_text_to_image_request`] appends the `@…`
/// citation to `promptText` in the SAME expression that pushes the reference
/// entry, so the cited and uncited cases cannot come apart. What remains for
/// Plan 04's live probe is only how WELL the reference is honoured, not whether
/// the request is well-formed.
///
/// `referenceImages` is skip-serialized when absent, never sent as `null` —
/// the same discipline `promptImage` carries, so an unconditioned still image
/// is byte-identical to what 42.1-01 shipped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunwayTextToImageRequest {
    pub model: String,
    #[serde(rename = "promptText")]
    pub prompt_text: String,
    pub ratio: &'static str,
    #[serde(rename = "referenceImages", skip_serializing_if = "Option::is_none")]
    pub reference_images: Option<Vec<RunwayReferenceImage>>,
}

/// Build the `text_to_image` body — the ONE place a [`ReferenceImage`] becomes
/// a tagged `referenceImages` entry, and the ONE place the matching `@…`
/// citation is written into `promptText`.
///
/// The two happen in the same `match` arm on purpose. Runway's tagged-reference
/// mechanism is a two-part contract (attach the tag AND name it in the prompt),
/// and splitting it across two call sites is exactly how a later edit would
/// leave a reference attached but uncited — the shape 42.1-01 refused to guess
/// at. Here they are structurally inseparable.
///
/// Pure: no `Client`, no network, no key. `req.background` has no analogue on
/// Runway's still-image surface and is ignored (Runway has no
/// transparent-background parameter; the retired OpenAI path's `background` was
/// provider-specific).
pub fn build_text_to_image_request(
    model: &str,
    prompt: String,
    reference: Option<&ReferenceImage>,
) -> RunwayTextToImageRequest {
    match reference {
        None => RunwayTextToImageRequest {
            model: model.to_string(),
            prompt_text: prompt,
            ratio: RUNWAY_RATIO,
            reference_images: None,
        },
        Some(r) => RunwayTextToImageRequest {
            model: model.to_string(),
            // Cited HERE, in the same expression that attaches the tag.
            prompt_text: format!("{prompt}{RUNWAY_REFERENCE_CITATION}"),
            ratio: RUNWAY_RATIO,
            reference_images: Some(vec![RunwayReferenceImage {
                uri: data_uri(&r.bytes),
                tag: RUNWAY_REFERENCE_TAG,
            }]),
        },
    }
}

/// The `POST /v1/video_to_video` body — Phase 56 (GEN-11).
///
/// # Every key here was LIVE-PROBED, and the absences are findings
///
/// This struct's field set is the direct output of `56-01`'s 13 zero-spend
/// probes (`artifacts/56-01-PROBE-RESULTS.md`), written after that artifact
/// existed. It is deliberately SMALLER than 56-RESEARCH predicted, because the
/// probe falsified three of the research's claims:
///
/// | key | status | evidence |
/// | --- | --- | --- |
/// | `videoUri` | **present, REQUIRED** | CONFIRMED twice independently: a body of only `{model}` makes the endpoint NAME the field, and a wrong-typed value raises `invalid_union` at its own path. Four alternate spellings (`video_uri`, `videoUrl`, `video`, `inputVideo`) were each rejected. |
/// | `promptText` | **present, optional** | no issue raised for its absence, against a validator that demonstrably reports several issues at once |
/// | `duration` | **ABSENT** | `unrecognized_keys` — the endpoint edits IN PLACE and bills against the input's own length |
/// | `references` | **ABSENT** | `unrecognized_keys` — this FALSIFIES 56-RESEARCH Q4 outright |
///
/// # Two probe-confirmed keys deliberately NOT modelled
///
/// - **`ratio`** — the KEY exists (a byte-identical A/B control against the
///   `duration` probe raised no `unrecognized_keys`), but **not one legal value
///   was enumerated** (follow-up F-2), and [`RUNWAY_RATIO`]'s own doc records
///   that the legal set is per-model AND per-endpoint, with `video_to_video`
///   never among the endpoints 42.1-04 collected. Sending the pinned
///   `1280:720` here would either 400 or — worse, because it is silent —
///   reframe a vertical clip the user shot themselves. This endpoint edits an
///   existing clip; omitting the key is what preserves it. Adding `ratio` is
///   an F-2-shaped change, not a guess.
/// - **`seed`** — exists and is number-typed, but `GenRequest` has no seed
///   concept to put in it. An unused field is a wider struct for no capability.
///
/// **Structurally carries no URL/endpoint field:** the base URL and the
/// endpoint are `&'static str` consts on this module, and the only string that
/// could resemble a target is a [`RunwayVideoInput`], whose both arms are
/// private-inner one-constructor types. `model` is free text by 55.1 design and
/// reaches ONLY the `model` JSON key — the tripwire test pins that.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunwayVideoToVideoRequest {
    /// The provider-side model id, free text since Phase 55.1. Travels in the
    /// BODY; it selects WHICH model and never WHERE the request goes.
    pub model: String,
    /// The source clip. Probe-confirmed key name, and a typed slot rather than
    /// a `String` so no caller-chosen host is expressible (SC-3).
    #[serde(rename = "videoUri")]
    pub video_uri: RunwayVideoInput,
    /// Optional on the endpoint (56-01 constant 5), but Rudis always has an
    /// instruction for an edit — "relight", "swap the background" — so it is
    /// always sent, exactly as [`RunwayImageToVideoRequest::prompt_text`] is.
    #[serde(rename = "promptText")]
    pub prompt_text: String,
}

/// The body of a submit, one arm per modality. `#[serde(untagged)]` — the wire
/// sees the inner object exactly as documented, with no discriminant key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum RunwayRequestBody {
    TextToImage(RunwayTextToImageRequest),
    Video(RunwayImageToVideoRequest),
    /// Phase 56: the source-clip edit. Reachable ONLY from
    /// [`build_video_edit_submission`], i.e. only when the call carries frames.
    VideoToVideo(RunwayVideoToVideoRequest),
}

impl RunwayRequestBody {
    /// The model id this body names — the same string the gate upstream saw.
    /// Since 55.1-02 that gate ADMITS it by the D-01 ungating rather than by
    /// vetting it, so this is "what was sent", never "what was cleared".
    pub fn model(&self) -> &str {
        match self {
            RunwayRequestBody::TextToImage(r) => &r.model,
            RunwayRequestBody::Video(r) => &r.model,
            RunwayRequestBody::VideoToVideo(r) => &r.model,
        }
    }
}

/// Everything the HTTP layer needs, decided ENTIRELY by pure code: the body,
/// the endpoint it must be POSTed to, and the fixed extension its output lands
/// under. Bundled into one value so [`RunwayProvider::submit`] makes no
/// decisions of its own — which is what lets every decision be tested offline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunwaySubmission {
    pub body: RunwayRequestBody,
    /// One of the `RUNWAY_ENDPOINT_*` consts — a `&'static str`, never data.
    pub endpoint: &'static str,
    /// `"mp4"` or `"png"`, decided by the request's [`RequestModality`].
    pub suggested_ext: &'static str,
}

/// Turn a model id + modality + prompt + optional frames into a
/// [`RunwaySubmission`] — a **total function** over `(modality, frames)`, with
/// no arm that depends on the model id at all.
///
/// **Phase 55.1 (D-01/D-05) rewrote this.** It used to open with
/// `model_caps(model_id)` and refuse anything absent from [`RUNWAY_MODELS`],
/// then gate seven further branches on that row's capability flags. That is
/// exactly the gate open model selection removes: `model_caps` returns `None`
/// for every legitimate model Runway ships after this binary was compiled, so
/// a known-model lookup is a guarantee that new models never work. The id is
/// now free text and travels to the wire verbatim.
///
/// **What that costs, stated plainly:** the capability-mismatch refusals this
/// function used to make locally, at $0 — a keyframe pair on a model that
/// cannot bridge, a text-only ask to an image-to-video-only model, an empty
/// prompt where `promptText` is required — are now Runway's server-side 400s.
/// Still fail-closed on spend (Runway charges nothing for a validation
/// rejection), but remote, and worded by the vendor. Surface that body as the
/// vendor's; never re-author it as a Rudis-style rejection.
///
/// **What still refuses locally, and why neither is model-derived:**
///
/// - a DESTINATION frame on an [`Image`](RequestModality::Image) request — a
///   category error (a single image has no last frame), keyed to the caller's
///   modality, never to a caps row;
/// - an inlined frame over Runway's 5 MB data-URI cap — an API-wide constant;
/// - an [`Audio`](RequestModality::Audio) request, refused explicitly so a
///   future mis-wiring is loud rather than a video built from a voice prompt.
pub fn build_submission(
    model_id: &str,
    modality: RequestModality,
    prompt: String,
    reference: Option<&ReferenceImage>,
    destination: Option<&ReferenceImage>,
) -> Result<RunwaySubmission, GenError> {
    // The ONE exhaustive match on modality. Adding a variant to
    // `RequestModality` breaks this at compile time rather than falling
    // through to an arbitrary arm.
    let suggested_ext = match modality {
        RequestModality::Audio => {
            return Err(GenError::InvalidRequest(
                "an audio request cannot route to the Runway provider".to_string(),
            ))
        }
        RequestModality::Image => "png",
        RequestModality::Video => "mp4",
    };

    if modality == RequestModality::Image {
        // A still image has no "last frame" to arrive at. A CATEGORY error,
        // not a missing capability — no Runway field could ever express it —
        // so it survives 55.1 unchanged in substance, re-keyed from
        // `caps.modality == "image"` to the caller's own modality. The wording
        // deliberately no longer implies anything about the model EXISTING.
        if destination.is_some() {
            return Err(GenError::InvalidRequest(format!(
                "a still-image request has no destination (last) frame to \
                 interpolate towards — pass only a reference (model \
                 '{model_id}')"
            )));
        }
        // 55.1: the reference is ALWAYS attached now. The old gate refused
        // when `caps.tagged_reference_images` was false, to avoid a frame
        // being silently ignored server-side; with free-text ids that flag is
        // unknowable, and Runway 400s a model that does not read the field.
        let image = build_text_to_image_request(model_id, prompt, reference);
        validate_image_data_uri_sizes(&image)?;
        return Ok(RunwaySubmission {
            body: RunwayRequestBody::TextToImage(image),
            endpoint: RUNWAY_ENDPOINT_TEXT_TO_IMAGE,
            suggested_ext,
        });
    }

    // Video. `build_image_to_video_request` is ALREADY a total function over
    // the frames supplied (its own doc table) and needed no change: no frames
    // => `promptImage` omitted; a lone reference => `Single(first)`; both =>
    // `Keyframes[first, last]`; a lone destination => `Keyframes[last]`,
    // "interpolate INTO a target". `prompt_image` is therefore the single
    // discriminant, and each of the four cases is resolved EXPLICITLY.
    let video = build_image_to_video_request(model_id, prompt, reference, destination);
    let endpoint = match &video.prompt_image {
        // No frames were supplied, so this is text→video — unconditionally,
        // regardless of what any roster row claims the model serves.
        None => RUNWAY_ENDPOINT_TEXT_TO_VIDEO,
        Some(_) => RUNWAY_ENDPOINT_IMAGE_TO_VIDEO,
    };
    validate_data_uri_sizes(&video)?;
    Ok(RunwaySubmission {
        body: RunwayRequestBody::Video(video),
        endpoint,
        suggested_ext,
    })
}

/// Build the `POST /v1/video_to_video` submission — Phase 56 (GEN-11), the arm
/// that EDITS an existing clip's own pixels.
///
/// # The endpoint follows the FRAMES, never the model id
///
/// This is a separate entry point from [`build_submission`] rather than a
/// fourth arm inside it, and the split IS the design: reaching this function
/// requires already holding a [`RunwayVideoInput`], which requires source-clip
/// bytes. There is no model id — not `aleph2`, not anything — that routes a
/// frameless call here, and no model id that routes a source-clip call
/// anywhere else. That makes 55.1's central consequence structural instead of
/// conventional, and it is what
/// `submit_dispatches_on_source_video` pins in both directions.
///
/// # The POST-55.1 refusal set, and what it deliberately does NOT contain
///
/// Refuses locally, at $0:
///
/// - an **empty/whitespace `model_id`** — a MALFORMED call, not a roster
///   judgment (the same distinction both agent seams draw since 55.1-04);
/// - **more than [`RUNWAY_V2V_MAX_REFERENCES`] references** — an
///   ENDPOINT-contract bound on the request SHAPE;
/// - **any reference at all**, for now — see the section below;
/// - an **inlined clip over the 16 MB per-asset cap**, an API-wide constant;
/// - a **serialized body over [`RUNWAY_MAX_REQUEST_BODY_BYTES`]** — Rudis's own
///   defensive backstop, honestly labelled as such.
///
/// Does NOT refuse, on purpose (55.1 decision 1): there is **no roster lookup,
/// no deprecated-model check and no `caps.video_to_video` gate.** An off-roster
/// id submits. **The recorded LOSS:** a capability mismatch — naming a model
/// that cannot edit video on this path — used to be a local $0 refusal and is
/// now Runway's **400 over the network**, worded by the vendor. Still
/// fail-closed on spend (Runway bills nothing for a validation rejection), but
/// no longer local and no longer in Rudis's words. That is 55.1's locked design
/// trade (T-56-SPEND-12), accepted rather than mitigated, and this comment is
/// where it is recorded.
///
/// # Why a reference currently REFUSES rather than shipping
///
/// `56-01` proved the `references` key does not exist. `56-F1` then swept six
/// documented candidate spellings at $0.00 and returned **AMBIGUOUS**: four do
/// not exist, **two do** (`keyframes` and `promptImage`, both array-typed) and
/// **neither is crowned**.
///
/// **56-F1b then ran (2026-08-09, 11 requests, 11 x HTTP 400, $0.00) and the
/// crown is STILL uncrowned — but the blocker has changed kind.** It closed the
/// per-item shape for both fields (a `keyframes` item is `{uri, seconds}` or
/// `{uri, at}`; a `promptImage` item is `{uri, position}`; both `uri`s are the
/// same 3-branch string union `videoUri` uses) and closed the ceiling for both
/// (`"maximum": 5, "inclusive": true` — see
/// [`RUNWAY_V2V_MAX_REFERENCES`]). **Closing those two questions is precisely
/// what destroyed the discriminator:** two five-capacity arrays of image
/// references, neither excludable, so "which one does Aleph 2.0 actually
/// consult" is a question about **BEHAVIOUR** — and a validation error proves a
/// schema, never a behaviour. **No further free probe can settle it. 56-09, the
/// owner-approved paid run, is the only remaining route** (F-1c, which would
/// enumerate `position`'s two object branches, is recorded in the artifact as
/// explicitly NOT a crown).
///
/// So there is still no field name a reference could be serialized into that
/// would not be a guess. The two available failure modes are: guess and be
/// silently wrong, or refuse and say why. `RUNWAY_REFERENCE_CITATION`'s doc
/// (private, in this module) records
/// 42.1-04 catching the first mode live — an uncited reference came back
/// **HTTP 200** with a plausible, unconditioned result that no caller could
/// detect. Refusing is the only option that cannot lie about what was applied.
/// **D-08, D-09 and GEN-11's reference clause remain BLOCKED**, now behind
/// 56-09 rather than behind F-1b.
pub fn build_video_edit_submission(
    model_id: &str,
    prompt: String,
    video: RunwayVideoInput,
    references: &[ReferenceImage],
) -> Result<RunwaySubmission, GenError> {
    if model_id.trim().is_empty() {
        return Err(GenError::InvalidRequest(
            "a video edit requires a model id — the call named none".to_string(),
        ));
    }

    // Request-SHAPE bound first: it is the cheapest structural refusal and the
    // one whose number is a contract rather than a probe gap.
    if references.len() > RUNWAY_V2V_MAX_REFERENCES {
        return Err(GenError::InvalidRequest(format!(
            "a video edit accepts at most {RUNWAY_V2V_MAX_REFERENCES} reference \
             images; {} were supplied",
            references.len()
        )));
    }
    if !references.is_empty() {
        // The citation is CURRENT, not historical: F-1b ran on 2026-08-09 and did
        // not crown a field, so a message still sending the reader after it would
        // point at a probe that has already been spent — which looks actionable
        // and is not. The blocker is now behavioural and paid (56-09).
        return Err(GenError::InvalidRequest(
            "reference images cannot be sent to video_to_video yet: the field's \
             real name is UNRESOLVED. 56-01 proved `references` does not exist \
             on this endpoint, and probe F-1b enumerated the two surviving \
             array-typed candidates in full — `keyframes` ({uri, seconds}|{uri, \
             at}) and `promptImage` ({uri, position}) — finding BOTH cap at 5 \
             image references, which is exactly why neither could be crowned. \
             Which one the model consults is a question about BEHAVIOUR, so no \
             validation error at any price can answer it; 56-09, the \
             owner-approved paid run, is the one that would. Rudis refuses \
             rather than guessing a key, because a wrongly-named reference is \
             accepted server-side and silently ignored, which is \
             indistinguishable from a bad generation"
                .to_string(),
        ));
    }

    // Per-asset bound on the inline arm. The uploaded arm has no size to check
    // here: `upload_video_to_runway` already bounded the bytes it sent, and the
    // handle itself is a few dozen characters.
    if let RunwayVideoInput::DataUri(uri) = &video {
        check_video_data_uri_size(uri)?;
    }

    let body = RunwayVideoToVideoRequest {
        model: model_id.to_string(),
        video_uri: video,
        prompt_text: prompt,
    };

    // The defensive whole-body backstop. Serializing to measure costs one extra
    // pass over a body that is already in memory, which is a fair price for a
    // bound that catches a future concatenation bug before it reaches a PAID
    // endpoint rather than after.
    let measured = serde_json::to_vec(&body)
        .map_err(|e| GenError::InvalidRequest(format!("the video-edit body is unserializable: {e}")))?
        .len();
    if measured > RUNWAY_MAX_REQUEST_BODY_BYTES {
        return Err(GenError::InvalidRequest(format!(
            "the serialized video-edit body is {measured} bytes, over Rudis's \
             defensive {RUNWAY_MAX_REQUEST_BODY_BYTES}-byte request-body bound"
        )));
    }

    Ok(RunwaySubmission {
        body: RunwayRequestBody::VideoToVideo(body),
        endpoint: RUNWAY_ENDPOINT_VIDEO_TO_VIDEO,
        // A fixed literal chosen BEFORE egress, never sniffed from the response
        // or the output URL's path (T-31-12) — this endpoint returns a clip.
        suggested_ext: "mp4",
    })
}

// ---------------------------------------------------------------------------
// Wire types — response side (Deserialize). serde ignores unknown fields by
// default, so a Runway-side additive change cannot break parsing.
// ---------------------------------------------------------------------------

/// What a `POST` returns: an id, immediately, always. The bytes are NEVER
/// inlined, so [`RunwayProvider::submit`] only ever returns
/// [`SubmitOutcome::Pending`] — this provider is structurally async, exactly
/// like the retired `VeoProvider` was.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RunwaySubmitResponse {
    pub id: String,
}

/// What `GET /v1/tasks/{id}` returns.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RunwayTaskEnvelope {
    pub id: String,
    /// `PENDING | THROTTLED | RUNNING | SUCCEEDED | FAILED | CANCELLED`. Kept a
    /// `String` and mapped by [`task_to_poll_decision`] so an UNRECOGNISED
    /// status is a clean, visible failure rather than a deserialize panic.
    pub status: String,
    #[serde(default)]
    pub output: Option<Vec<String>>,
    #[serde(default)]
    pub failure: Option<String>,
    #[serde(default, rename = "failureCode")]
    pub failure_code: Option<String>,
}

/// Parse a submit body. A parse failure is a clean [`GenError::Provider`],
/// never a panic.
pub fn parse_submit_response(body: &str) -> Result<RunwaySubmitResponse, GenError> {
    serde_json::from_str(body)
        .map_err(|e| GenError::Provider(format!("bad Runway submit response: {e}")))
}

/// Parse a task-status body. A parse failure is a clean [`GenError::Provider`].
pub fn parse_task_envelope(body: &str) -> Result<RunwayTaskEnvelope, GenError> {
    serde_json::from_str(body)
        .map_err(|e| GenError::Provider(format!("bad Runway task response: {e}")))
}

/// The four-way decision a poll resolves to — the crux of the async mapping,
/// entirely testable without HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunwayPollDecision {
    StillPending,
    Failed(String),
    Cancelled,
    /// A terminal success — the response-supplied output URL to fetch. Passes
    /// through [`validate_output_uri`] before any GET.
    Download(String),
}

/// Map a parsed task envelope to a [`RunwayPollDecision`].
///
/// A `SUCCEEDED` task carrying zero outputs is a `Failed`, never a panic and
/// never a fake `Ready` — returning success with nothing on disk is the lie the
/// landing bridge forbids. An UNRECOGNISED status is likewise a `Failed`:
/// treating it as still-pending would poll a dead job forever.
pub fn task_to_poll_decision(env: &RunwayTaskEnvelope) -> RunwayPollDecision {
    match env.status.to_ascii_uppercase().as_str() {
        "PENDING" | "THROTTLED" | "RUNNING" => RunwayPollDecision::StillPending,
        "SUCCEEDED" => match env.output.as_ref().and_then(|o| o.first()) {
            Some(uri) => RunwayPollDecision::Download(uri.clone()),
            None => RunwayPollDecision::Failed(
                "Runway task SUCCEEDED but carried zero outputs".to_string(),
            ),
        },
        "FAILED" => {
            let code = env.failure_code.as_deref().unwrap_or("unknown");
            let msg = env.failure.as_deref().unwrap_or("no failure detail");
            RunwayPollDecision::Failed(format!("Runway generation failed ({code}): {msg}"))
        }
        "CANCELLED" | "CANCELED" => RunwayPollDecision::Cancelled,
        other => RunwayPollDecision::Failed(format!(
            "unrecognised Runway task status '{other}' — treating as terminal \
             rather than polling a dead job forever"
        )),
    }
}

/// Gate the ONE URL in this integration that comes from a response body rather
/// than a `&'static` const (T-33-02 SSRF), before any GET is issued.
///
/// # Why this is not a host-suffix pin like Veo's
///
/// `validate_download_uri` in `veo.rs` can demand `*.googleapis.com` because
/// Google serves its outputs from its own API host. Runway serves generated
/// assets from a **CDN distribution**, and 42.1-RESEARCH could not settle which
/// one documentarily — a guessed suffix would either be wrong (breaking every
/// real download) or so loose it proves nothing. So this gate blocks the
/// SHAPES that make SSRF useful instead:
///
/// - non-HTTPS (`http:`, `file:`, `ftp:`, a relative path);
/// - embedded credentials (`https://user:pass@host/…`);
/// - a bare IP literal for a host — the form that reaches `169.254.169.254`
///   (cloud metadata), `127.0.0.1` and RFC-1918 ranges. Runway's own input
///   rules already say "domain names only, no IP addresses";
/// - a single-label host (`https://localhost/…`, intranet names).
///
/// ## 42.1-04: the host was OBSERVED, and a hard pin was deliberately NOT added
///
/// The real host is [`RUNWAY_OBSERVED_OUTPUT_HOST`] — a CloudFront
/// distribution. That closes 42.1-01's open item, but it does not by itself
/// justify turning the gate into a suffix pin, and the reasoning matters more
/// than the hostname:
///
/// - **The failure modes are wildly asymmetric.** A wrong pin rejects every
///   download and takes generation down completely. What a right pin buys is
///   defence-in-depth on a URL that already arrives inside an authenticated,
///   TLS-verified response from a pinned host — the marginal case is Runway's
///   own API being compromised or buggy, not an attacker-chosen URL.
/// - **One observation is not the set.** A single distribution id seen on one
///   account, one region and one modality is weak evidence that every output
///   link forever shares that suffix; CDNs get rotated and multi-CDN'd, and
///   `cloudfront.net` is not Runway-specific anyway, so pinning it would admit
///   every other AWS customer's distribution regardless.
///
/// So the hard gate stays SHAPE-based (which is what actually blocks the useful
/// SSRF targets), and an unexpected host is logged rather than refused —
/// gathering the evidence a real pin would need instead of guessing at it now.
/// If the observed set turns out to be stable across accounts and modalities,
/// promoting this to a refusal is a one-line change.
pub fn validate_output_uri(uri: &str) -> Result<(), GenError> {
    let parsed = reqwest::Url::parse(uri)
        .map_err(|_| GenError::Provider("Runway output URI is not a valid URL".to_string()))?;
    if parsed.scheme() != "https" {
        return Err(GenError::Provider(format!(
            "Runway output URI is not HTTPS: scheme '{}'",
            parsed.scheme()
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(GenError::Provider(
            "Runway output URI embeds credentials".to_string(),
        ));
    }
    // The error names the HOST only — never echoes a full untrusted URI.
    let host = parsed
        .host_str()
        .ok_or_else(|| GenError::Provider("Runway output URI has no host".to_string()))?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return Err(GenError::Provider(format!(
            "Runway output URI host is a bare IP literal: {host}"
        )));
    }
    if !bare.contains('.') {
        return Err(GenError::Provider(format!(
            "Runway output URI host is not a dotted domain: {host}"
        )));
    }
    Ok(())
}

/// The output CDN host OBSERVED live on 2026-07-27 (42.1-04), closing the open
/// item 42.1-01 left behind: `dnznrvs05pmza.cloudfront.net`.
///
/// Recorded as evidence, NOT enforced — see [`validate_output_uri`] for why a
/// one-observation suffix pin is the wrong trade. [`output_host_is_expected`]
/// is what reads it, and only to decide whether to log.
pub const RUNWAY_OBSERVED_OUTPUT_HOST: &str = "dnznrvs05pmza.cloudfront.net";

/// The suffix the observed host belongs to. Kept separate from the full host
/// because the distribution id is per-account/per-region and will differ for
/// other users, whereas the suffix is the part that could ever become a pin.
pub const RUNWAY_OBSERVED_OUTPUT_SUFFIX: &str = ".cloudfront.net";

/// Whether an output host matches what 42.1-04 observed. Used ONLY to decide
/// whether the download logs a "this is new" line — never to accept or reject,
/// so a CDN rotation degrades to a log entry rather than an outage.
pub fn output_host_is_expected(host: &str) -> bool {
    host.ends_with(RUNWAY_OBSERVED_OUTPUT_SUFFIX)
}

/// The HOST of an output URI, and nothing else — no scheme, no path, no query.
///
/// Exists so the observed CDN host can be RECORDED (see the call site in
/// `poll`) without any caller being handed the pre-signed path+query, which is
/// capability-bearing: anyone holding it can fetch the asset. `None` for a URI
/// that does not parse or has no host, which [`validate_output_uri`] has
/// already rejected by the time this runs.
pub fn output_uri_host(uri: &str) -> Option<String> {
    reqwest::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
}

/// The sentence [`non_success_to_gen_error`] appends to a DETERMINISTIC status,
/// and the needle `app_core`'s retry accounting matches on.
///
/// A fixed literal, in one place, for the same reason every other user-visible
/// string in this module is: it round-trips into a tool result the agent reads,
/// and two wordings would mean two behaviours.
pub const RUNWAY_TERMINAL_STATUS_NOTICE: &str =
    " This status is DETERMINISTIC — the identical request will fail the identical \
     way, so do NOT retry it. Change the request or tell the user what you cannot do.";

/// Whether an HTTP status can EVER succeed by repeating the identical request.
///
/// # Why this exists
///
/// The owner's first live 4K clip edit failed **twice** with the same
/// `HTTP 413 {"message":"Request Entity Too Large"}` (debug session
/// `v2v-413-on-4k-source-unprobed-upload-window`). The second attempt was
/// guaranteed to fail: the body was byte-identical and the refusal happened at an
/// edge proxy on size alone. It burned the second of the turn's two
/// `GENERATE_RETRY_CAP` slots and showed the user the same vendor error again.
///
/// Nothing in the code retried it — `app_core`'s `dispatch_generate_with_cap`
/// caps, it does not retry. The MODEL retried, because the surfaced error said
/// nothing about whether retrying could help. So the fix is the message, and this
/// predicate is what decides whether it carries
/// [`RUNWAY_TERMINAL_STATUS_NOTICE`].
///
/// # The set, and why each member is in it
///
/// | status | why a retry cannot help |
/// | --- | --- |
/// | 400 | the body failed validation; the same body fails identically |
/// | 401 | the credential is wrong, and this provider never rotates one mid-turn |
/// | 403 | the account is not permitted this operation |
/// | 404 | the endpoint or task does not exist |
/// | 413 | the body is too large; size does not vary between attempts |
/// | 422 | semantically invalid content, same input, same verdict |
///
/// **Deliberately EXCLUDED:** `408`, `425`, `429` and every `5xx`. Those are
/// transient by definition and a retry is exactly the right response — a
/// classifier that swept them in would turn a recoverable rate-limit into a dead
/// end, which is the opposite failure and a worse one.
pub fn runway_status_is_terminal(status: u16) -> bool {
    matches!(status, 400 | 401 | 403 | 404 | 413 | 422)
}

/// Map a non-2xx HTTP status + body to a [`GenError::Provider`].
///
/// The `body` is Runway's own error JSON — it never contains the
/// `Authorization` header, so surfacing it verbatim is safe (T-31-01).
/// Crucially this helper does NOT receive the API key, so there is structurally
/// no interpolation path by which the key could reach the error string.
///
/// Since 2026-08-13 a status [`runway_status_is_terminal`] recognises also
/// carries [`RUNWAY_TERMINAL_STATUS_NOTICE`]. The vendor's own text is still
/// surfaced VERBATIM and FIRST — the notice is appended, never substituted, so
/// nothing Runway said is hidden or re-authored (D-05's posture).
pub fn non_success_to_gen_error(status: u16, body: &str) -> GenError {
    let terminal = if runway_status_is_terminal(status) {
        RUNWAY_TERMINAL_STATUS_NOTICE
    } else {
        ""
    };
    GenError::Provider(format!("Runway API error (HTTP {status}): {body}{terminal}"))
}

/// Map an arbitrary entropy word onto the `[floor, floor + span)` poll window —
/// pure, so the jitter's BOUNDS are provable without sampling a clock.
pub fn jittered_poll_interval(entropy: u64) -> std::time::Duration {
    let span_ms = RUNWAY_POLL_JITTER_SPAN.as_millis() as u64;
    debug_assert!(span_ms > 0);
    RUNWAY_POLL_INTERVAL_FLOOR + std::time::Duration::from_millis(entropy % span_ms)
}

/// A dependency-free entropy word. `RandomState` is seeded per-thread and
/// bumped on every construction, so successive calls differ even inside one
/// clock tick — enough to de-phase concurrent pollers, and deliberately NOT a
/// cryptographic source (nothing here is a secret).
fn poll_entropy() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.finish()
}

// ---------------------------------------------------------------------------
// The provider.
// ---------------------------------------------------------------------------

/// The REAL, key-gated Runway generation provider — genuinely async, covering
/// both video and still images.
///
/// Mirrors the retired `VeoProvider`'s `{reqwest::Client, api_key}`
/// shape and key hygiene. Deliberately NOT `Debug`/`Serialize`-derived —
/// nothing here can leak the key into a log line (T-31-01/T-33-01).
pub struct RunwayProvider {
    client: reqwest::Client,
    api_key: String,
    models: Vec<ModelInfo>,
    /// `task id -> suggested extension`, recorded at submit and consumed at
    /// poll.
    ///
    /// This exists because one provider serves TWO modalities, so the output
    /// extension cannot be a single fixed literal the way `veo.rs`'s `"mp4"`
    /// is — and it must NOT be sniffed from the response or the output URL's
    /// path (T-31-12). Recording the choice we already made at submit keeps the
    /// literal fixed AND correct. Safe because `JobRegistry` is explicitly
    /// in-memory for the process lifetime (see `job.rs`), and the provider is
    /// process-lifetime managed state — a handle can never outlive the instance
    /// that minted it.
    in_flight: std::sync::Mutex<std::collections::HashMap<String, &'static str>>,
}

impl RunwayProvider {
    /// The advisory catalog GEN-07 surfaces: every [`RUNWAY_MODELS`] row that
    /// is not `deprecated`, in table order.
    ///
    /// **Phase 55.1 re-derived this.** It used to enumerate the intent map's
    /// five models plus the expensive-transition fallback, on the reasoning that
    /// listing the whole roster would advertise models no caller could drive.
    /// Open model selection makes that reasoning false in both directions: every
    /// roster model is now drivable (`aleph2` included — this phase makes it
    /// REACHABLE; its `video_to_video` endpoint is Phase 56's), and plan 06
    /// deleted the map, which would have left this empty. `veo3`/`veo3.1` are
    /// listed despite carrying no cost signal — an honest `None` is better than
    /// hiding a real model.
    ///
    /// **This is advisory, not a gate.** An off-roster id is absent from here
    /// and submits perfectly well (D-01). The one consequence worth naming:
    /// `app_core::resolve_provenance_flag` reads this catalog and defaults a
    /// model it cannot find to `carries_provenance_watermark: true`. For an
    /// off-roster id that is the T-31-19 conservative over-disclosure — telling
    /// a user their footage may carry an invisible provenance signal when it
    /// does not is harmless; the reverse is a product lie — so it is unchanged
    /// and correct here.
    fn catalog() -> Vec<ModelInfo> {
        RUNWAY_MODELS
            .iter()
            .filter(|(_, _, caps)| !caps.deprecated)
            .map(|(id, label, caps)| ModelInfo {
                id: (*id).to_string(),
                label: (*label).to_string(),
                modality: caps.modality.to_string(),
                carries_provenance_watermark: caps.carries_provenance_watermark,
                rough_cost_signal: caps.rough_cost_signal.map(str::to_string),
            })
            .collect()
    }

    /// Construct directly from a resolved key — the constructor the tests and
    /// [`connect`](Self::connect) both funnel through. Building a
    /// `reqwest::Client` needs no runtime; only `.send()` does.
    pub fn with_key(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            models: Self::catalog(),
            in_flight: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// BYO-key resolution mirroring the retired `VeoProvider::connect` (and the
    /// surviving [`ElevenLabsProvider::connect`](crate::ElevenLabsProvider::connect)):
    /// the STORE's key
    /// WINS (the OS-keychain `"runway"` slot, registered in the human-gated
    /// Plan 02), then `RUNWAY_API_KEY`, then `RUNWAYML_API_SECRET` — Runway's
    /// OWN documented env-var name, so a developer who already exported it for
    /// the vendor CLI is picked up without a second copy of the credential.
    /// Absent ⇒ `None`, never a panic.
    ///
    /// **Env assumption:** `cargo test` does not load `.env` (dotenvy runs only
    /// in `run()`), so with an empty store and neither var set in the process
    /// env this returns `None` deterministically in the offline suite.
    pub fn connect(store: &dyn agent_llm::KeyStore) -> Option<Self> {
        let api_key = store
            .get()
            .ok()
            .flatten()
            .or_else(|| std::env::var("RUNWAY_API_KEY").ok())
            .or_else(|| std::env::var("RUNWAYML_API_SECRET").ok())?;
        Some(Self::with_key(api_key))
    }

    /// The EXACT header pairs every Runway request carries — the ONE site the
    /// key is interpolated, and the reason a newly-added endpoint cannot forget
    /// [`RUNWAY_VERSION_HEADER`]. Deliberately private: the return value
    /// contains the credential.
    fn headers(&self) -> [(&'static str, String); 2] {
        let (version_name, version_value) = version_header();
        [
            ("Authorization", format!("Bearer {}", self.api_key)),
            (version_name, version_value.to_string()),
        ]
    }

    /// Apply [`headers`](Self::headers) to a request builder. Every authorized
    /// call in this module goes through here.
    fn authorized(&self, mut rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in self.headers() {
            rb = rb.header(name, value);
        }
        rb
    }

    fn remember_task(&self, task_id: &str, ext: &'static str) {
        let mut map = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(task_id.to_string(), ext);
    }

    fn task_ext(&self, task_id: &str) -> Option<&'static str> {
        let map = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(task_id).copied()
    }

    fn forget_task(&self, task_id: &str) {
        let mut map = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.remove(task_id);
    }

    /// D-03's overflow transport: put a source clip through Runway's two-step
    /// presigned upload and return the **server-issued handle**, which is the
    /// only value that escapes.
    ///
    /// # Why both HTTP calls live in here
    ///
    /// The intermediate presigned URL is capability-bearing (anyone holding it
    /// can write to that object) and it is a caller-uncontrolled host. If this
    /// returned it, or took it as a parameter, there would be a `String`-typed
    /// URL in the flow and the SC-3 property would be a convention rather than
    /// a type. Instead: bytes in, [`RunwayUploadedAsset`] out, both requests
    /// inside the provider, and the URL never crosses this function's boundary
    /// in either direction (56-RESEARCH § the size-gated transport pattern).
    ///
    /// # Coverage posture
    ///
    /// No hermetic test drives the HTTP here — the same stance `submit`/`poll`
    /// take. What IS tested hermetically is everything this function DECIDES
    /// with: [`video_transport_for_len`], [`RunwayUploadRequest::EPHEMERAL_CLIP`],
    /// [`validate_upload_url`] and [`RunwayUploadedAsset`]'s deserialize gate.
    ///
    /// **This paragraph used to end "the live exercise is the ONE owner-approved
    /// paid run in Plan 09". That run never came here** — 56-09 sized its fixture
    /// to stay on the inline arm precisely BECAUSE `/v1/uploads` was unprobed, so
    /// this method shipped having never executed, and the first user clip big
    /// enough to need it was also the first to run it. That is the shape worth
    /// remembering: *a fallback nobody has exercised is not a fallback.*
    ///
    /// **All three hops are now live-proven** (2026-08-13): step 1 and the
    /// presigned transfer by probe F-4 at $0.00 (HTTP 200, then HTTP 204 for a
    /// real 30 162 998 B MP4), and the `runway://` handoff into `video_to_video`
    /// by the owner's verified end-to-end run in the shipped app. See
    /// [`RUNWAY_MAX_UPLOAD_BYTES`].
    ///
    /// Errors name the STEP that failed and never echo the clip bytes, the
    /// presigned query string, or the credential (which this method cannot
    /// reach except through [`authorized`](Self::authorized)).
    async fn upload_video_to_runway(&self, bytes: &[u8]) -> Result<RunwayUploadedAsset, GenError> {
        // Step 1 — ask for a presigned target. The URL is a `&'static str`
        // const joined to the base const; nothing request-derived reaches it.
        let step1 = self
            .authorized(self.client.post(format!(
                "{RUNWAY_BASE_URL}/{RUNWAY_ENDPOINT_UPLOADS}"
            )))
            .json(&RunwayUploadRequest::EPHEMERAL_CLIP)
            .send()
            .await
            .map_err(|e| GenError::Provider(format!("Runway upload step 1 (create): {e}")))?;
        if !step1.status().is_success() {
            let status = step1.status().as_u16();
            let text = step1.text().await.unwrap_or_default();
            return Err(GenError::Provider(format!(
                "Runway upload step 1 (create) failed: {}",
                non_success_to_gen_error(status, &text)
            )));
        }
        let text = step1
            .text()
            .await
            .map_err(|e| GenError::Provider(format!("Runway upload step 1 (create): {e}")))?;
        // The ONE place a RunwayUploadedAsset is minted: a non-`runway://`
        // value fails HERE, before any second request exists.
        let created: RunwayUploadsResponse = serde_json::from_str(&text).map_err(|e| {
            GenError::Provider(format!(
                "Runway upload step 1 (create) response was unusable: {e}"
            ))
        })?;
        validate_upload_url(&created.upload_url)?;

        // Step 2 — the presigned form POST. Every field Runway issued is
        // replayed verbatim as a text part; only non-string values are
        // stringified (a presigned policy is always a string in practice, and
        // dropping one silently would fail the upload with a confusing S3
        // error rather than here).
        let mut form = reqwest::multipart::Form::new();
        for (name, value) in &created.fields {
            let rendered = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            form = form.text(name.clone(), rendered);
        }
        // The file part LAST — presigned policies require the fields to precede
        // the content, and the part name is the documented fixed literal.
        form = form.part(
            "file",
            reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(
                RunwayUploadRequest::EPHEMERAL_CLIP.filename,
            ),
        );
        // NOT `authorized`: the presigned URL carries its own authorization in
        // the form fields, and attaching Rudis's Runway bearer token to a
        // request aimed at a storage host would leak the credential off the API
        // host for no benefit (T-31-01).
        let step2 = self
            .client
            .post(&created.upload_url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| GenError::Provider(format!("Runway upload step 2 (transfer): {e}")))?;
        if !step2.status().is_success() {
            // The storage host's body is NOT surfaced: unlike Runway's own API
            // error JSON it is not a shape this module has vetted, and it can
            // echo the presigned query string back. The STATUS still gets the
            // deterministic-status notice, because the reason a retry is futile
            // does not depend on reading the body.
            let status = step2.status().as_u16();
            return Err(GenError::Provider(format!(
                "Runway upload step 2 (transfer) failed: HTTP {status}{}",
                if runway_status_is_terminal(status) {
                    RUNWAY_TERMINAL_STATUS_NOTICE
                } else {
                    ""
                }
            )));
        }
        Ok(created.runway_uri)
    }

    /// Resolve raw clip bytes into the ONE video-input form that fits, taking
    /// the `/v1/uploads` detour only when the inline cap cannot carry them.
    ///
    /// The projection is computed BEFORE any encoding, so an over-cap clip is
    /// never base64-expanded just to be discarded.
    async fn resolve_video_input(&self, bytes: &[u8]) -> Result<RunwayVideoInput, GenError> {
        match video_transport_for_len(projected_video_data_uri_len(bytes.len()), bytes.len())? {
            VideoTransport::DataUri => Ok(RunwayVideoInput::DataUri(video_data_uri(bytes))),
            VideoTransport::Upload => Ok(RunwayVideoInput::Uploaded(
                self.upload_video_to_runway(bytes).await?,
            )),
        }
    }

    /// Test-only accessor proving [`connect`](Self::connect) held the STORE's
    /// key. Not part of the public API — the key is otherwise unreachable.
    #[cfg(test)]
    pub(crate) fn api_key_for_test(&self) -> &str {
        &self.api_key
    }
}

impl GenProvider for RunwayProvider {
    fn id(&self) -> ProviderId {
        ProviderId(RUNWAY_PROVIDER_ID.to_string())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, GenError> {
        // Zero network — Rudis's own curated, intent-table-aligned catalog.
        Ok(self.models.clone())
    }

    async fn submit(&self, req: GenRequest) -> Result<SubmitOutcome, GenError> {
        // Every decision is made by PURE code first; this method only moves
        // bytes. `req.background` is IGNORED — Runway's surface has no
        // background parameter (mirrors VeoProvider's ignore posture).
        //
        // Phase 56 (GEN-11): the ONE dispatch, and it fires on what the call
        // CARRIES. `source_video` present => the clip is being re-rendered, so
        // the endpoint is `video_to_video`; absent => every pre-56 path,
        // byte-unchanged. The model id is not consulted here and must not be:
        // it selects WHICH model, never WHERE the request goes (55.1 D-01/D-05,
        // pinned by `submit_dispatches_on_source_video`).
        let submission = match &req.source_video {
            Some(bytes) => {
                // The only genuinely non-pure step in the whole path, and it is
                // here rather than in the builder because resolving the
                // transport may need Runway's own upload endpoint (D-03).
                let video = self.resolve_video_input(bytes).await?;
                build_video_edit_submission(
                    &req.model_id,
                    req.prompt.clone(),
                    video,
                    req.reference_images.as_deref().unwrap_or(&[]),
                )?
            }
            None => build_submission(
                &req.model_id,
                // 55.1 (D-05): the routing truth rides the REQUEST, from the
                // tool that produced it — it is no longer looked up from the
                // model.
                req.modality,
                req.prompt.clone(),
                req.reference_image.as_ref(),
                req.destination_image.as_ref(),
            )?,
        };
        // The endpoint is a `&'static str` const and the model rides the BODY,
        // so nothing caller-supplied reaches this URL at all.
        let url = format!("{RUNWAY_BASE_URL}/{}", submission.endpoint);
        let resp = self
            .authorized(self.client.post(url))
            .json(&submission.body)
            .send()
            .await
            .map_err(|e| GenError::Provider(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(non_success_to_gen_error(status, &text));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| GenError::Provider(e.to_string()))?;
        let parsed = parse_submit_response(&text)?;
        self.remember_task(&parsed.id, submission.suggested_ext);
        // ALWAYS Pending — a Runway POST returns a task id immediately, always.
        // The id is OPAQUE: stored verbatim, never parsed or reconstructed.
        Ok(SubmitOutcome::Pending(JobHandle::new(
            JobId::mint(),
            parsed.id,
        )))
    }

    async fn poll(&self, job: &JobHandle) -> Result<JobStatus, GenError> {
        let task_id = &job.provider_job_ref;
        let ext = self.task_ext(task_id).ok_or_else(|| {
            GenError::Provider(format!(
                "Runway task '{task_id}' was not submitted through this provider \
                 instance, so its output type is unknown"
            ))
        })?;

        let resp = self
            .authorized(
                self.client
                    .get(format!("{RUNWAY_BASE_URL}/{RUNWAY_ENDPOINT_TASKS}/{task_id}")),
            )
            .send()
            .await
            .map_err(|e| GenError::Provider(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(non_success_to_gen_error(status, &text));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| GenError::Provider(e.to_string()))?;
        let env = parse_task_envelope(&text)?;

        match task_to_poll_decision(&env) {
            RunwayPollDecision::StillPending => Ok(JobStatus::Pending),
            RunwayPollDecision::Failed(msg) => {
                self.forget_task(task_id);
                Ok(JobStatus::Failed(msg))
            }
            RunwayPollDecision::Cancelled => {
                self.forget_task(task_id);
                Ok(JobStatus::Cancelled)
            }
            RunwayPollDecision::Download(uri) => {
                // (1) SSRF-gate the ONE response-supplied URL BEFORE any GET.
                validate_output_uri(&uri)?;
                // (1b) Record the HOST — and ONLY the host. 42.1-01 could not
                //      pin a suffix because RESEARCH never learned which CDN
                //      Runway serves outputs from, and a guessed pin would
                //      either break every download or prove nothing. This is
                //      the evidence channel for that pin. Deliberately NOT the
                //      full URI: the path and query of a pre-signed link are
                //      capability-bearing and do not belong in a log.
                if let Some(host) = output_uri_host(&uri) {
                    if output_host_is_expected(&host) {
                        eprintln!("runway: output download host = {host}");
                    } else {
                        eprintln!(
                            "runway: output download host = {host} — OUTSIDE the suffix \
                             observed in 42.1-04 ({RUNWAY_OBSERVED_OUTPUT_SUFFIX}). Not an \
                             error: the gate is shape-based on purpose. Worth recording if \
                             it recurs, since a stable observed set is what a real suffix \
                             pin would need."
                        );
                    }
                }
                // (2) Download WITHOUT our headers. This is the one place this
                //     module deliberately diverges from `veo.rs`, which DOES
                //     send its key on the download: Veo's download host is
                //     pinned to `*.googleapis.com`, Runway's is an unpinned CDN
                //     distribution. Attaching `Authorization: Bearer <key>` to
                //     a host named by the RESPONSE would hand the credential to
                //     whatever host that response chose — the exact inverse of
                //     the gate above. Runway's output links are pre-signed and
                //     need no auth.
                let dl = self
                    .client
                    .get(&uri)
                    .send()
                    .await
                    .map_err(|e| GenError::Provider(e.to_string()))?;
                let dl_status = dl.status().as_u16();
                if !dl.status().is_success() {
                    let text = dl.text().await.unwrap_or_default();
                    return Err(non_success_to_gen_error(dl_status, &text));
                }
                // Bound a hostile body by declared content-length first...
                if let Some(len) = dl.content_length() {
                    if len > RUNWAY_MAX_DOWNLOAD_BYTES {
                        return Err(GenError::Provider(format!(
                            "Runway download exceeds {RUNWAY_MAX_DOWNLOAD_BYTES} bytes \
                             (content-length {len})"
                        )));
                    }
                }
                let bytes = dl
                    .bytes()
                    .await
                    .map_err(|e| GenError::Provider(e.to_string()))?;
                // ...then by the ACTUAL received length (content-length may lie
                // or be absent).
                if bytes.len() as u64 > RUNWAY_MAX_DOWNLOAD_BYTES {
                    return Err(GenError::Provider(format!(
                        "Runway download exceeds {RUNWAY_MAX_DOWNLOAD_BYTES} bytes \
                         (received {})",
                        bytes.len()
                    )));
                }
                self.forget_task(task_id);
                Ok(JobStatus::Ready(vec![AssetRef {
                    bytes: bytes.to_vec(),
                    // The extension we CHOSE at submit from the model table —
                    // never sniffed from this response (T-31-12).
                    suggested_ext: ext.to_string(),
                }]))
            }
        }
    }

    async fn cancel(&self, job: &JobHandle) -> Result<(), GenError> {
        // Best-effort: the poll loop stops locally regardless of the outcome.
        // Swallow everything — mirrors `VeoProvider::cancel`'s posture.
        let _ = self
            .authorized(self.client.delete(format!(
                "{RUNWAY_BASE_URL}/{RUNWAY_ENDPOINT_TASKS}/{}",
                job.provider_job_ref
            )))
            .send()
            .await;
        self.forget_task(&job.provider_job_ref);
        Ok(())
    }

    fn poll_interval(&self) -> std::time::Duration {
        jittered_poll_interval(poll_entropy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PNG magic — the shape of the real bytes `engine::encode_png_bytes`
    /// produces upstream.
    const PNG_MAGIC: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

    fn req(prompt_image: Option<RunwayPromptImage>) -> RunwayImageToVideoRequest {
        RunwayImageToVideoRequest {
            model: "gen4_turbo".to_string(),
            prompt_image,
            prompt_text: "a red bicycle".to_string(),
            ratio: RUNWAY_RATIO,
            duration: RUNWAY_DURATION_SECONDS,
        }
    }

    /// The data URI carries the FIXED prefix and round-trips the exact bytes.
    #[test]
    fn data_uri_uses_the_fixed_png_prefix_and_round_trips_the_bytes() {
        let uri = data_uri(PNG_MAGIC);
        assert!(
            uri.as_str().starts_with("data:image/png;base64,"),
            "the mime is a fixed literal, never derived: {}",
            uri.as_str()
        );
        let b64 = uri.as_str().strip_prefix(RUNWAY_DATA_URI_PREFIX).unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(b64).unwrap(),
            PNG_MAGIC,
            "base64 round-trips to the EXACT input bytes"
        );

        // Even a degenerate input keeps the prefix — there is no code path that
        // produces a bare/unprefixed uri.
        assert_eq!(data_uri(&[]).as_str(), RUNWAY_DATA_URI_PREFIX);
    }

    /// `Single` is a bare JSON string; `Keyframes` is a JSON array of
    /// `{uri, position}` objects with LOWERCASE positions.
    #[test]
    fn prompt_image_serializes_single_as_a_string_and_keyframes_as_an_array() {
        let single = RunwayPromptImage::Single(data_uri(PNG_MAGIC));
        let v = serde_json::to_value(&single).unwrap();
        assert!(
            v.as_str().unwrap().starts_with("data:image/png;base64,"),
            "the single form is a BARE string, not an object: {v}"
        );

        let pair = RunwayPromptImage::Keyframes(vec![
            RunwayKeyframe {
                uri: data_uri(&[0xAA]),
                position: RunwayKeyframePosition::First,
            },
            RunwayKeyframe {
                uri: data_uri(&[0xBB]),
                position: RunwayKeyframePosition::Last,
            },
        ]);
        let v = serde_json::to_value(&pair).unwrap();
        let arr = v.as_array().expect("the keyframe form is a JSON ARRAY");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["position"], "first", "positions serialize lowercase");
        assert_eq!(arr[1]["position"], "last");
        assert!(arr[0]["uri"].as_str().unwrap().starts_with("data:image/png;base64,"));
    }

    /// A text-only request OMITS `promptImage` entirely — never `"promptImage":
    /// null`. Same `skip_serializing_if` discipline as `VeoInstance::image`.
    #[test]
    fn text_only_request_omits_prompt_image_entirely() {
        let json = serde_json::to_string(&req(None)).unwrap();
        assert!(
            !json.contains("promptImage"),
            "a text-only body must OMIT the key, never send null: {json}"
        );
        assert!(json.contains("\"promptText\":\"a red bicycle\""), "{json}");
        assert!(json.contains("\"ratio\":\"1280:720\""), "{json}");
        assert!(
            json.contains("\"duration\":4"),
            "duration is a JSON NUMBER, not a string (the 33-03 lesson): {json}"
        );
    }

    /// TRIPWIRE (mirrors `gen_request_carries_no_endpoint_field`): the request
    /// body has exactly these five fields. Adding a URL-ish field — or a
    /// `references` array, which is MUTUALLY EXCLUSIVE with keyframes on
    /// `seedance2` — fails to compile here and forces a deliberate review.
    #[test]
    fn image_to_video_request_carries_no_url_or_references_field() {
        let r = req(Some(RunwayPromptImage::Single(data_uri(PNG_MAGIC))));
        let RunwayImageToVideoRequest {
            model,
            prompt_image,
            prompt_text,
            ratio,
            duration,
        } = &r;
        assert_eq!(model, "gen4_turbo");
        assert!(prompt_image.is_some());
        assert_eq!(prompt_text, "a red bicycle");
        assert_eq!(*ratio, RUNWAY_RATIO);
        assert_eq!(*duration, RUNWAY_DURATION_SECONDS);

        // And nothing url/reference-shaped reaches the wire.
        let v = serde_json::to_value(&r).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.get("references").is_none(), "no references array: {obj:?}");
        assert!(obj.get("url").is_none() && obj.get("uri").is_none());
        assert_eq!(obj.len(), 5, "exactly the five documented keys: {obj:?}");
    }

    /// The pre-egress size bound: an in-range frame passes, an oversized one is
    /// a clean `InvalidRequest` whose message carries the LENGTH but not the
    /// (megabytes-of-base64) URI.
    #[test]
    fn data_uri_size_bound_rejects_over_the_five_megabyte_cap() {
        assert!(validate_data_uri_sizes(&req(None)).is_ok(), "text-only is trivially fine");
        assert!(
            validate_data_uri_sizes(&req(Some(RunwayPromptImage::Single(data_uri(PNG_MAGIC)))))
                .is_ok(),
            "a real conditioning frame is far under the cap"
        );

        // 4 MiB of raw bytes base64-expands 4/3 to ~5.33 MB — over the cap.
        let oversized = data_uri(&vec![0u8; 4 * 1024 * 1024]);
        assert!(oversized.len() > RUNWAY_MAX_DATA_URI_BYTES);
        let err = validate_data_uri_sizes(&req(Some(RunwayPromptImage::Keyframes(vec![
            RunwayKeyframe {
                uri: oversized,
                position: RunwayKeyframePosition::First,
            },
        ]))))
        .expect_err("an oversized frame is rejected BEFORE egress");
        assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("data-URI cap"), "{msg}");
        assert!(
            !msg.contains("data:image/png;base64,"),
            "the message must not echo the URI itself: {} chars",
            msg.len()
        );
    }

    // -----------------------------------------------------------------------
    // Task 4 — the pure builder. Four request shapes, decided only by which
    // frames the caller supplied.
    // -----------------------------------------------------------------------

    fn frame(bytes: Vec<u8>) -> ReferenceImage {
        ReferenceImage {
            bytes,
            width: 1280,
            height: 720,
        }
    }

    /// Decode a `data:image/png;base64,…` back to its raw bytes — the assertion
    /// primitive for "the RIGHT frame landed in the RIGHT slot".
    fn decode(uri: &RunwayDataUri) -> Vec<u8> {
        let b64 = uri
            .as_str()
            .strip_prefix(RUNWAY_DATA_URI_PREFIX)
            .expect("every promptImage value carries the fixed data-URI prefix");
        base64::engine::general_purpose::STANDARD.decode(b64).unwrap()
    }

    #[test]
    fn build_no_frames_is_a_text_only_body() {
        let body = build_image_to_video_request("gen4_turbo", "a red bicycle".into(), None, None);
        assert!(body.prompt_image.is_none());
        let json = serde_json::to_string(&body).unwrap();
        assert_eq!(
            json,
            r#"{"model":"gen4_turbo","promptText":"a red bicycle","ratio":"1280:720","duration":4}"#,
            "the text-only body is exactly the four documented keys — no \
             promptImage, and duration a JSON number: {json}"
        );
    }

    #[test]
    fn build_reference_only_is_a_bare_single_data_uri() {
        let first = frame(vec![1, 2, 3, 4]);
        let body =
            build_image_to_video_request("veo3.1_fast", "pan left".into(), Some(&first), None);
        match body.prompt_image.as_ref().expect("a reference populates promptImage") {
            RunwayPromptImage::Single(uri) => {
                assert!(uri.as_str().starts_with("data:image/png;base64,"));
                assert_eq!(decode(uri), vec![1, 2, 3, 4], "the EXACT reference bytes");
            }
            other => panic!("a lone reference must be the Single form, got {other:?}"),
        }
        // And on the wire it is a bare STRING, not a one-element array.
        let v = serde_json::to_value(&body).unwrap();
        assert!(v["promptImage"].is_string(), "{v}");
    }

    /// THE headline capability, and the assertion that matters most: both
    /// frames present, in the RIGHT slots, **not swapped**. This mirrors
    /// `veo.rs`'s own guard — an inverted transition generates the move
    /// backwards and looks entirely plausible in code review.
    #[test]
    fn build_reference_and_destination_is_an_ordered_first_last_pair_not_swapped() {
        let first = frame(vec![0xAA, 0xAA, 0xAA]);
        let last = frame(vec![0xBB, 0xBB]);
        let body = build_image_to_video_request(
            "veo3.1_fast",
            "drone rises from street level to a top-down bird's-eye view".into(),
            Some(&first),
            Some(&last),
        );

        let frames = match body.prompt_image.as_ref().expect("both frames populate promptImage") {
            RunwayPromptImage::Keyframes(f) => f,
            other => panic!("a first+last pair must be the Keyframes form, got {other:?}"),
        };
        assert_eq!(frames.len(), 2, "exactly two keyframes: {frames:?}");

        assert_eq!(frames[0].position, RunwayKeyframePosition::First);
        assert_eq!(frames[1].position, RunwayKeyframePosition::Last);
        assert_eq!(
            decode(&frames[0].uri),
            vec![0xAA, 0xAA, 0xAA],
            "position \"first\" MUST carry the REFERENCE bytes"
        );
        assert_eq!(
            decode(&frames[1].uri),
            vec![0xBB, 0xBB],
            "position \"last\" MUST carry the DESTINATION bytes"
        );

        // --- the explicit NOT-SWAPPED assertion ---
        assert_ne!(
            decode(&frames[0].uri),
            decode(&frames[1].uri),
            "the two keyframes must be distinct for this guard to mean anything"
        );
        assert_ne!(
            decode(&frames[0].uri),
            vec![0xBB, 0xBB],
            "NOT SWAPPED: the destination must never land on position \"first\" \
             — an inverted pair generates the transition backwards"
        );
        assert_ne!(
            decode(&frames[1].uri),
            vec![0xAA, 0xAA, 0xAA],
            "NOT SWAPPED: the reference must never land on position \"last\""
        );

        // And the same guarantee once serialized, where a caller would see it.
        let v = serde_json::to_value(&body).unwrap();
        let arr = v["promptImage"].as_array().expect("an ARRAY of exactly two objects");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["position"], "first");
        assert_eq!(arr[1]["position"], "last");
        assert_ne!(
            arr[0]["uri"], arr[1]["uri"],
            "the serialized pair is not two copies of one frame: {v}"
        );
    }

    /// A destination WITHOUT a first frame is a legal shape too (interpolate
    /// INTO a target) — and it must NOT silently become a `Single`, which would
    /// send the destination as the FIRST frame.
    #[test]
    fn build_destination_only_is_a_lone_last_keyframe_never_a_single() {
        let last = frame(vec![7, 7, 7]);
        let body =
            build_image_to_video_request("veo3.1_fast", "end on the rooftop".into(), None, Some(&last));
        match body.prompt_image.as_ref().expect("a destination populates promptImage") {
            RunwayPromptImage::Keyframes(frames) => {
                assert_eq!(frames.len(), 1);
                assert_eq!(
                    frames[0].position,
                    RunwayKeyframePosition::Last,
                    "a lone destination is positioned LAST, never first"
                );
                assert_eq!(decode(&frames[0].uri), vec![7, 7, 7]);
            }
            RunwayPromptImage::Single(_) => panic!(
                "a destination-only request must NOT collapse to Single — that \
                 would send the target frame as the clip's FIRST frame"
            ),
        }
        let v = serde_json::to_value(&body).unwrap();
        assert!(v["promptImage"].is_array(), "{v}");
    }

    /// The builder never invents a model, and always applies the pinned
    /// ratio/duration constants.
    #[test]
    fn build_carries_the_caller_model_and_the_pinned_constants() {
        let body = build_image_to_video_request("gen4.5", "x".into(), None, None);
        assert_eq!(body.model, "gen4.5");
        assert_eq!(body.ratio, RUNWAY_RATIO);
        assert_eq!(body.duration, RUNWAY_DURATION_SECONDS);
    }

    /// The pinned constants are the phase's cost contract — pin them literally
    /// so a silent edit (e.g. `duration: 10`, a 2.5× spend) fails a test.
    #[test]
    fn pinned_request_constants() {
        assert_eq!(RUNWAY_RATIO, "1280:720");
        assert_eq!(RUNWAY_DURATION_SECONDS, 4);
        assert_eq!(RUNWAY_MAX_DATA_URI_BYTES, 5 * 1024 * 1024);
        assert_eq!(RUNWAY_DATA_URI_PREFIX, "data:image/png;base64,");
    }

    // -----------------------------------------------------------------------
    // DELETED by Phase 55.1 plan 06: the seven 42.1 "Task 5" tests that were the
    // intent map's enforcement. They asserted, over a table that no longer
    // exists, that every capability word resolved to exactly one row, that wire
    // words round-tripped, that no row named an off-roster or deprecated id,
    // that the two tables' caps could not drift, and — the big one — that no
    // DEFAULT could exceed the $0.48 tier or land on the 7.2x model.
    //
    // WHAT IS GENUINELY LOST, stated rather than implied: the compile-time-plus-
    // test guarantee that a default generation could never be expensive or
    // uncosted. Nothing replaces it in code, because with free-text models there
    // is no "default" left to bound — the caller names the model. The remaining
    // controls are the spend confirmation (which now quotes a real price or the
    // literal "price unknown", 55.1-04) and the rulebook's cost table (55.1-05).
    // That trade is D-01, recorded in PROVENANCE.md Entry 17's amendment.
    //
    // WHAT DID NOT NEED REPLACING, because a surviving test already pins it:
    //   * per-model keyframe-pair facts   -> live_probe_2026_07_27_answers_are_pinned
    //   * every published price + rate    -> every_costed_video_row_satisfies_
    //                                        cents_equals_credits_per_second_times_4
    //   * the retired ids are flagged and excluded
    //                                     -> the_roster_is_internally_consistent,
    //                                        catalog_lists_every_non_deprecated_roster_model
    //   * the prices the AGENT is shown   -> app-core's
    //                                        rulebook_cost_table_agrees_with_the_spend_gate
    // -----------------------------------------------------------------------

    /// **The arithmetic invariant the `aleph2` halving defect broke.** Runway
    /// prices per-second video models in CREDITS PER SECOND and sells credits
    /// at $0.01 each, so a [`RUNWAY_DURATION_SECONDS`] clip must cost exactly
    /// `credits_per_second × 4` cents. Every costed video row obeyed that
    /// except `aleph2`, which shipped `cost_cents_per_4s: Some(56)` against a
    /// published 28 credits/s: `56` is Runway's `minimumCredits` figure — the
    /// 2-second input floor, in CREDITS — transplanted into a CENTS-per-4s
    /// field, i.e. exactly HALF the real $1.12. That field feeds
    /// `spend_confirmation_gate`, so the defect would have quoted a user half
    /// the price of the most expensive per-second model on the roster.
    /// Corrected 2026-08-01 (quick 260801-r7s); this test is what would have
    /// caught it, and what stops it coming back.
    ///
    /// The rates are pinned as LITERALS, in the same house style as
    /// `live_probe_2026_07_27_answers_are_pinned`: re-confirmed 2026-08-01 from
    /// `docs.dev.runwayml.com/guides/pricing` (see 56-01-PROBE-RESULTS.md
    /// § "Pricing facts"). They are deliberately NOT parsed out of
    /// [`ModelCaps::rough_cost_signal`], whose doc says "Never parsed" — that
    /// holds for tests too, and its formats are heterogeneous anyway (the image
    /// rows carry per-image ranges like "5-8 credits"). Parsing it would test
    /// the prose against itself and agree with any future typo.
    #[test]
    fn every_costed_video_row_satisfies_cents_equals_credits_per_second_times_4() {
        // Runway's published per-second rates, in CREDITS (1 credit = $0.01).
        const CREDITS_PER_SECOND: &[(&str, u32)] = &[
            ("gen4_turbo", 5),
            ("gen4.5", 12),
            ("veo3.1_fast", 10),
            ("seedance2", 36),
            ("seedance2_mini", 16),
            ("aleph2", 28),
        ];

        // The ONLY exemption, and it is a real pricing difference rather than a
        // convenience: the image models are billed PER IMAGE (`gen4_image` 5-8
        // credits, `gen4_image_turbo` 2), so no credits-per-SECOND rate exists
        // to multiply. Their `cost_cents_per_4s` reuses the field as a
        // per-image price, as its own doc says.
        //
        // Uncosted rows (`cost_cents_per_4s: None` — veo3, veo3.1 and the two
        // retired ids) are NOT exemptions: they carry no figure to check, so
        // they fall outside this invariant's domain by construction.
        const EXEMPT_PER_IMAGE: &[&str] = &["gen4_image", "gen4_image_turbo"];

        for (id, credits) in CREDITS_PER_SECOND {
            let caps = model_caps(id).unwrap_or_else(|| panic!("'{id}' is a roster model"));
            assert_eq!(
                caps.cost_cents_per_4s,
                Some(credits * 4),
                "'{id}' is published at {credits} credits/s and 1 credit = $0.01, so \
                 {RUNWAY_DURATION_SECONDS}s costs {} cents. A row disagreeing with its \
                 own rate is how the aleph2 half-price defect shipped — re-check \
                 docs.dev.runwayml.com/guides/pricing before changing either side, and \
                 never write a minimumCredits figure into this field",
                credits * 4
            );
        }

        // Exhaustiveness: a NEW costed model must land in one of the two lists
        // above or fail loudly here. Without this sweep the table would only
        // ever check the rows someone remembered to add — a silent skip.
        for (id, _, caps) in RUNWAY_MODELS {
            if caps.cost_cents_per_4s.is_none() {
                continue;
            }
            assert!(
                CREDITS_PER_SECOND.iter().any(|(rated, _)| rated == id)
                    || EXEMPT_PER_IMAGE.contains(id),
                "'{id}' carries a cost but is neither rate-pinned in CREDITS_PER_SECOND \
                 nor listed in EXEMPT_PER_IMAGE with a reason — price it against Runway's \
                 pricing page or state why the per-second invariant does not apply"
            );
        }
    }

    /// **The A→B bridge still has a floor, stated about the ROSTER rather than
    /// about a routing decision.**
    ///
    /// This is what survives of 42.1's
    /// `transition_routes_to_a_keyframe_pair_capable_model_with_a_documented_fallback`
    /// (Phase 55.1 plan 06). The half that asked "which model does the
    /// `Transition` intent resolve to, and is it pair-capable?" died with the
    /// intent map — no local table routes anything now. The half that still
    /// means something is the *capability floor*: the rulebook tells the agent
    /// which models can bridge a first+last pair (55.1-05), and that column is
    /// written from these `keyframe_pair` values, so at least one live,
    /// `image_to_video`-capable model must genuinely carry `Documented` or the
    /// guidance is pointing at nothing.
    #[test]
    fn the_roster_keeps_at_least_one_live_documented_keyframe_pair_model() {
        let pair_capable: Vec<&str> = RUNWAY_MODELS
            .iter()
            .filter(|(_, _, c)| {
                c.keyframe_pair == KeyframePairSupport::Documented
                    && c.image_to_video
                    && !c.deprecated
            })
            .map(|(id, _, _)| *id)
            .collect();
        assert!(
            !pair_capable.is_empty(),
            "no live roster model documents the first/last pair — the rulebook's \
             \"pair YES\" column and every transition recipe would be describing a \
             capability Rudis cannot name a model for"
        );
        // Named, so a roster edit that quietly drops the cheap one is visible:
        // 42.1-04 proved `veo3.1_fast` ($0.40) carries the pair, which is why the
        // $1.44 fallback const this test used to check was never needed.
        for expected in ["veo3.1_fast", "seedance2"] {
            assert!(
                pair_capable.contains(&expected),
                "{expected} carries the pair per the 2026-07-27 live probe: {pair_capable:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Phase 56 plan 05 — the ADVISORY default model for the edit path.
    //
    // RED SCAFFOLD (deleted at GREEN): this local stub shadowed the production
    // name and answered `None`, so both failures below were ASSERTION failures
    // on a tree that still BUILT — the 55.1-04 "signature-change TDD" discipline
    // — rather than a compile error, which proves nothing about behaviour.
    // -----------------------------------------------------------------------

    /// **The default is pinned in the TEST and DERIVED in the code.**
    ///
    /// That two-direction redundancy is the same shape the retired allow-list
    /// literal had: the production function reads the roster's
    /// `video_to_video` flag and never names a model, while this test names
    /// `aleph2` and never reads the flag. A roster edit that removes `aleph2`
    /// or re-caps another row therefore has to argue with a failing test before
    /// the edit path's priced default can silently change under it.
    #[test]
    fn advisory_video_edit_model_is_the_rosters_v2v_capable_entry() {
        assert_eq!(
            advisory_video_edit_model(),
            Some("aleph2"),
            "the edit path's advisory default is the roster's video_to_video row. \
             The code DERIVES this from the caps flag and the test PINS the literal \
             — if they disagree, decide deliberately which one moved"
        );
        // Non-vacuity: the id this test names is genuinely on the roster and
        // genuinely priced, so the spend confirmation really can quote it.
        let caps = model_caps("aleph2").expect("aleph2 is a roster row");
        assert!(caps.video_to_video, "and its caps really do carry the flag");
        assert!(
            caps.cost_cents_per_4s.is_some() && caps.rough_cost_signal.is_some(),
            "the whole point of an advisory default is that it has a price to quote"
        );
    }

    /// **"First match" is only honest while there IS exactly one match.**
    ///
    /// `advisory_video_edit_model` returns the FIRST `video_to_video` row. If a
    /// second one ever lands, that derivation silently keeps returning whichever
    /// sits higher in the table — a default chosen by list order rather than by
    /// anyone's decision, and possibly the more expensive of the two. Asserted
    /// by iterating the whole roster rather than by trusting the lookup, so this
    /// test cannot be satisfied by the very function it is guarding.
    #[test]
    fn advisory_video_edit_model_is_currently_unambiguous() {
        let v2v: Vec<&str> = RUNWAY_MODELS
            .iter()
            .filter(|(_, _, caps)| caps.video_to_video)
            .map(|(id, _, _)| *id)
            .collect();
        assert_eq!(
            v2v.len(),
            1,
            "exactly one roster row is video_to_video-capable today, so returning \
             the first match names a decision rather than a table position. Found \
             {v2v:?} — if a second model genuinely gained the capability, choose the \
             default deliberately (price is the axis: see CAPS_ALEPH2's doc) and \
             update advisory_video_edit_model to say so"
        );
    }

    /// `output_uri_host` is the EVIDENCE channel for the SSRF suffix pin
    /// 42.1-01 could not invent — and it must never widen into a full-URI leak,
    /// because a Runway output link is pre-signed and therefore
    /// capability-bearing.
    #[test]
    fn output_uri_host_yields_the_host_and_never_the_signed_path() {
        let signed = "https://dnznrvs05pmza.cloudfront.net/abc123.mp4\
                      ?_jwt=SECRET-CAPABILITY-TOKEN&Expires=123";
        let host = output_uri_host(signed).expect("a host");
        assert_eq!(host, "dnznrvs05pmza.cloudfront.net");
        assert!(!host.contains("_jwt"), "the signed query must not ride along");
        assert!(!host.contains("SECRET"), "the capability token must not ride along");
        assert!(!host.contains('/'), "no path component: {host}");
        // Garbage in, honest None out — never a panic and never a guess.
        assert_eq!(output_uri_host("not a url"), None);
    }

    /// 42.1-04 recorded the real CDN host. This pins BOTH halves of the
    /// decision: the observation itself, and the fact that it is deliberately
    /// not enforced — an unexpected host must still pass `validate_output_uri`,
    /// because a CDN rotation has to degrade to a log line and never to a total
    /// generation outage.
    #[test]
    fn the_observed_output_host_is_recorded_but_not_enforced() {
        assert!(output_host_is_expected(RUNWAY_OBSERVED_OUTPUT_HOST));
        assert!(RUNWAY_OBSERVED_OUTPUT_HOST.ends_with(RUNWAY_OBSERVED_OUTPUT_SUFFIX));
        assert!(!output_host_is_expected("assets.runwayml.com"));

        // The decisive half: a host outside the observed suffix is still
        // ACCEPTED by the real gate. If this ever fails, someone promoted the
        // record into a pin — which may be right, but is a deliberate
        // availability trade that belongs in a plan, not a refactor.
        validate_output_uri("https://assets.runwayml.com/out.mp4")
            .expect("an unexpected-but-well-shaped host must still download");
        // ...while the shapes that make SSRF useful stay refused.
        validate_output_uri("https://169.254.169.254/latest/meta-data/")
            .expect_err("the shape-based gate is what actually does the work");
    }

    /// **42.1-04: the live probe's answers, pinned.**
    ///
    /// Every assertion below is a fact `scripts/runway-capability-probe.mjs`
    /// OBSERVED against `api.dev.runwayml.com` on 2026-07-27, at $0.00 (each
    /// answer arrived as a validation error before a task was created). They
    /// are pinned rather than left in prose so that a later edit which
    /// "corrects" the table back to a guess has to argue with a failing test.
    ///
    /// Re-run the probe before changing any of these — the answers are Runway's
    /// to change, and a stale pin is worse than no pin.
    #[test]
    fn live_probe_2026_07_27_answers_are_pinned() {
        // --- The keyframe PAIR, per model. The Veo family is NOT uniform, which
        // is the single most surprising result and the one a reader is most
        // likely to "simplify" away.
        for (model, expected) in [
            ("veo3.1_fast", KeyframePairSupport::Documented),
            ("veo3.1", KeyframePairSupport::Documented),
            ("seedance2", KeyframePairSupport::Documented),
            ("seedance2_mini", KeyframePairSupport::Documented),
            ("veo3", KeyframePairSupport::Unsupported),
            ("gen4.5", KeyframePairSupport::Unsupported),
            ("gen4_turbo", KeyframePairSupport::Unsupported),
        ] {
            assert_eq!(
                model_caps(model).expect("a roster model").keyframe_pair,
                expected,
                "{model}'s keyframe_pair was VERIFIED LIVE on 2026-07-27; re-run \
                 scripts/runway-capability-probe.mjs before changing it"
            );
        }

        // --- No video model may be left on `Presumed`. The probe settled every
        // one, so a `Presumed` video row now means a model was added without
        // being probed — exactly the "looks checked but isn't" state the
        // three-state flag exists to make impossible.
        let unprobed: Vec<&str> = RUNWAY_MODELS
            .iter()
            .filter(|(_, _, c)| {
                c.modality == "video"
                    && !c.deprecated
                    && c.keyframe_pair == KeyframePairSupport::Presumed
            })
            .map(|(id, _, _)| *id)
            .collect();
        assert!(
            unprobed.is_empty(),
            "these live video models have never been pair-probed: {unprobed:?} — run \
             scripts/runway-capability-probe.mjs and record the answer"
        );

        // --- The transition-routing assertion that stood here was DELETED with
        // the intent map (Phase 55.1 plan 06): it read "the `Transition` intent
        // still resolves to veo3.1_fast, not the $1.44 fallback", and there is
        // no routing left to assert. The probe FACT it depended on is already
        // pinned above (`veo3.1_fast` => Documented), and the capability floor
        // moved to `the_roster_keeps_at_least_one_live_documented_keyframe_pair_model`.

        // --- RESEARCH gate (a), closed. See the const's doc for why a
        // wrong-TYPED probe was needed: unknown keys are silently stripped, so
        // presence-probing this API cannot answer the question.
        assert!(
            !RUNWAY_HAS_STRUCTURED_CAMERA_CONTROL,
            "verified live: cameraControl / motion / cameraMotion / camera / \
             cameraMovement are all stripped by gen4.5's schema"
        );

        // --- The two pinned request constants, against the observed legal sets.
        assert!(RUNWAY_RATIO_IS_UNIVERSAL);
        assert_eq!(
            RUNWAY_RATIO, "1280:720",
            "the only ratio present in EVERY observed per-model enumeration"
        );
        assert_eq!(
            RUNWAY_DURATION_SECONDS, 4,
            "veo3.1_fast accepts EXACTLY 4|6|8 — not a 2..=10 range as RESEARCH (b) \
             claimed. 4 is legal everywhere an intent routes; 5 would 400."
        );
        // Non-vacuity: the duration pin must be one of veo3.1_fast's three legal
        // values, so this test would really fail if someone re-pinned it to 5.
        assert!(
            [4u32, 6, 8].contains(&RUNWAY_DURATION_SECONDS),
            "the pinned duration must be one of the transition model's legal values"
        );
    }

    /// **The constraint to encode, not discover later:** on `seedance2` the
    /// first/last keyframes and the `references` array are mutually exclusive.
    /// This wave models no `references` field at all — which is the enforcement
    /// — and the flag records WHY, so a later wave adding one has to confront
    /// it. (The compile-time half lives in
    /// `image_to_video_request_carries_no_url_or_references_field`.)
    ///
    /// **Phase 55.1 plan 06.** The second half of this test swept the intent map
    /// for a row whose model set `pair_excludes_references`, so no capability
    /// word could promise a transition that ALSO carried style references. With
    /// the map deleted there is no such promise to make: nothing local picks a
    /// model, and the request builder still has no `references` field, which is
    /// what actually made the promise impossible. The exclusivity flag is kept
    /// and asserted, so a wave that adds the field has to confront it.
    #[test]
    fn keyframe_pair_and_style_references_are_never_promised_together() {
        assert!(
            model_caps("seedance2").unwrap().pair_excludes_references,
            "the exclusivity is recorded on the model RESEARCH documents it for"
        );
    }

    // `camera_move_is_prompt_language_not_structured_axes` was DELETED by Phase
    // 55.1 plan 06: its subject was the `camera-move` capability word, and its
    // second half asserted which model that word resolved to. Its FIRST half —
    // that Runway exposes no structured camera axes — is asserted verbatim in
    // `live_probe_2026_07_27_answers_are_pinned` above, so the recorded gate (a)
    // answer is still pinned by a test and `RUNWAY_HAS_STRUCTURED_CAMERA_CONTROL`
    // is not left as a const nothing reads. The guidance it protected (a camera
    // move is prompt LANGUAGE, never numeric axes) now lives in the rulebook and
    // the `video_transitions` playbook, where 55.1-05 put it.

    /// Roster hygiene: ids and labels are unique, and every live entry is
    /// reachable through at least one real endpoint.
    #[test]
    fn the_roster_is_internally_consistent() {
        let mut ids: Vec<&str> = RUNWAY_MODELS.iter().map(|(id, _, _)| *id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "roster ids are unique");

        for (id, label, caps) in RUNWAY_MODELS {
            assert_eq!(model_label(id), Some(*label));
            assert!(
                matches!(caps.modality, "video" | "image"),
                "'{id}' has modality '{}', which is not a ModelInfo word",
                caps.modality
            );
            let reachable = caps.text_to_image
                || caps.text_to_video
                || caps.image_to_video
                || caps.video_to_video;
            assert_eq!(
                reachable, !caps.deprecated,
                "'{id}': every LIVE model is reachable through an endpoint, and \
                 every retired one through none"
            );
            if caps.modality == "image" {
                assert_eq!(
                    caps.keyframe_pair,
                    KeyframePairSupport::Unsupported,
                    "'{id}' is a still — it has no last frame"
                );
            }
        }
        assert_eq!(model_caps("not-a-runway-model"), None);
        assert_eq!(model_label("not-a-runway-model"), None);
    }

    // -----------------------------------------------------------------------
    // Tasks 1 + 6 — the provider: identity, key hygiene, connect, the pure
    // half of the async lifecycle. ZERO network: no test below reaches a
    // `.send()`.
    // -----------------------------------------------------------------------

    /// A well-formed but FAKE sentinel key — never a real credential. Threaded
    /// through the error-mapping context to PROVE no interpolation path lets it
    /// reach a surfaced error string.
    const SENTINEL_KEY: &str = "key_SENTINEL-not-a-real-runway-secret";

    /// A permissive validator so a test key is accepted by the in-memory store
    /// double. NOTHING about it hits the network.
    fn accept_any(_key: &str) -> Result<(), agent_llm::KeyStoreError> {
        Ok(())
    }

    /// Drive an `async fn` from a plain `#[test]` without a runtime — the
    /// `pollster` precedent. Constructing a `reqwest::Client` needs no runtime;
    /// only `.send()` does, and no unit test here reaches a `.send()`.
    fn run<T>(fut: impl std::future::Future<Output = T>) -> T {
        pollster::block_on(fut)
    }

    #[test]
    fn runway_provider_id_and_pinned_endpoints() {
        let p = RunwayProvider::with_key("k".to_string());
        assert_eq!(p.id().as_str(), "runway");
        assert_eq!(RUNWAY_PROVIDER_ID, "runway");
        assert_eq!(RUNWAY_BASE_URL, "https://api.dev.runwayml.com/v1");
        assert!(
            RUNWAY_BASE_URL.starts_with("https://"),
            "the pinned base is HTTPS and is a const — never request data"
        );
        assert_eq!(RUNWAY_ENDPOINT_TEXT_TO_IMAGE, "text_to_image");
        assert_eq!(RUNWAY_ENDPOINT_TEXT_TO_VIDEO, "text_to_video");
        assert_eq!(RUNWAY_ENDPOINT_IMAGE_TO_VIDEO, "image_to_video");
        assert_eq!(RUNWAY_ENDPOINT_TASKS, "tasks");
    }

    /// **The Prove bullet:** every request carries
    /// `X-Runway-Version: 2024-11-06`. `headers()` is the ONE place headers are
    /// built and the ONE place the key is interpolated, and `authorized()` — the
    /// only way any request in this module is issued — applies all of them.
    #[test]
    fn every_request_carries_the_version_header_and_exactly_one_key_site() {
        assert_eq!(version_header(), ("X-Runway-Version", "2024-11-06"));
        assert_eq!(RUNWAY_API_VERSION, "2024-11-06");

        let p = RunwayProvider::with_key(SENTINEL_KEY.to_string());
        let headers = p.headers();
        assert_eq!(headers.len(), 2, "exactly Authorization + the version header");
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(
            headers[0].1,
            format!("Bearer {SENTINEL_KEY}"),
            "the key is a Bearer token, interpolated ONLY here"
        );
        assert_eq!(headers[1].0, "X-Runway-Version");
        assert_eq!(headers[1].1, "2024-11-06");
        assert_eq!(
            headers.iter().filter(|(_, v)| v.contains(SENTINEL_KEY)).count(),
            1,
            "the credential appears in exactly ONE header value"
        );
    }

    /// `connect` prefers the STORE's key over ambient env; an empty store with
    /// no env key resolves to `None` (honest absence, never a panic).
    #[test]
    fn runway_connect_prefers_the_store_then_runway_api_key_then_runwayml_api_secret() {
        use agent_llm::{InMemoryKeyStore, KeyStore as _};

        let store = InMemoryKeyStore::with_validator(accept_any);
        store.set("store-key-123").expect("seed the store double");
        let p = RunwayProvider::connect(&store).expect("a seeded store builds a provider");
        assert_eq!(
            p.api_key_for_test(),
            "store-key-123",
            "the STORE's key wins over any ambient RUNWAY_API_KEY/RUNWAYML_API_SECRET"
        );

        // Empty store + (documented assumption) neither env var set → None.
        if std::env::var("RUNWAY_API_KEY").is_ok() || std::env::var("RUNWAYML_API_SECRET").is_ok() {
            eprintln!("skipping absence check: a Runway key env var is set in the process env");
            return;
        }
        let empty = InMemoryKeyStore::with_validator(accept_any);
        assert!(
            RunwayProvider::connect(&empty).is_none(),
            "no store key + no env key → None, never a panic"
        );
    }

    /// The catalog is exactly the LIVE roster — every `RUNWAY_MODELS` row with
    /// `deprecated == false`, in table order.
    ///
    /// **Phase 55.1.** It used to be derived from the intent map plus the
    /// expensive-transition fallback (five ids), on the reasoning that listing
    /// the whole roster would advertise models no caller could drive. With open
    /// model selection every roster model IS drivable, and plan 06 deleted the
    /// map — a catalog derived from it would have ended up empty.
    #[test]
    fn catalog_lists_every_non_deprecated_roster_model() {
        let p = RunwayProvider::with_key("k".to_string());
        let models = run(p.list_models()).expect("list_models succeeds offline");

        let expected: Vec<&str> = RUNWAY_MODELS
            .iter()
            .filter(|(_, _, caps)| !caps.deprecated)
            .map(|(id, _, _)| *id)
            .collect();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, expected, "the live roster, in table order");
        assert_eq!(ids.len(), 10, "12 roster rows minus the 2 retired ones");
        assert!(!models.is_empty(), "never empty — GEN-07 would surface nothing");

        // Named rows, so a roster edit that quietly drops one is caught.
        // `aleph2` becomes REACHABLE this phase (its endpoint is Phase 56's);
        // `gen4_image_turbo` was never intent-reachable at all.
        for present in ["aleph2", "gen4_image_turbo"] {
            assert!(ids.contains(&present), "{present} is listed: {ids:?}");
        }
        for retired in ["gen3a_turbo", "gen4_aleph"] {
            assert!(
                model_caps(retired).expect("still an advisory row").deprecated,
                "the exclusion below is non-vacuous"
            );
            assert!(!ids.contains(&retired), "{retired} is excluded: {ids:?}");
        }

        for m in &models {
            let caps = model_caps(&m.id).expect("catalog ids come from the roster");
            assert!(!caps.deprecated, "{} is live", m.id);
            assert_eq!(m.modality, caps.modality, "{} modality matches the table", m.id);
            assert_eq!(
                m.carries_provenance_watermark, caps.carries_provenance_watermark,
                "{} watermark flag matches the table",
                m.id
            );
            // Pinned to the TABLE, not to a hardcoded expectation: `veo3` and
            // `veo3.1` are honestly uncosted (`rough_cost_signal: None`) and
            // are now listed rather than hidden, so an is_some() assertion here
            // would be false. Catalog and table cannot drift.
            assert_eq!(
                m.rough_cost_signal.as_deref(),
                caps.rough_cost_signal,
                "{} cost signal matches the table",
                m.id
            );
            assert!(
                m.carries_provenance_watermark,
                "Runway publishes C2PA content credentials on output (GEN-09 \
                 disclosure source): {}",
                m.id
            );
        }
    }

    // --- build_submission: every rejection is a real 400 (or a real silent
    //     degradation) avoided, offline and for free. ---

    fn ref_frame() -> ReferenceImage {
        ReferenceImage {
            bytes: vec![1, 2, 3],
            width: 1280,
            height: 720,
        }
    }

    // --- Phase 55.1 (D-01/D-05): the open-model-selection contract. -------
    //
    // These three tests are the phase's central claim, and each of them is
    // RED against the pre-55.1 `build_submission`: today it looks the model up
    // in `RUNWAY_MODELS` and refuses anything absent, deprecated, or whose
    // `ModelCaps` row does not advertise the capability the frames imply. That
    // lookup returns `None` for every legitimate model Runway ships after this
    // binary was compiled, which is exactly the gate D-01 removes.

    /// **The phase's headline claim.** A model id absent from `RUNWAY_MODELS`
    /// reaches `RunwaySubmission` construction with NO local refusal.
    ///
    /// Rudis cannot know whether Runway's server-side model enum admits an id
    /// it has never heard of — a real one submits fine, and a genuinely
    /// invalid one comes back as the vendor's OWN `{error, docUrl, issues}`
    /// 400 (D-05: still fail-closed on spend, just remotely, and worded by the
    /// vendor rather than re-authored by Rudis).
    #[test]
    fn off_roster_model_id_reaches_submission_with_no_local_gate() {
        for off_roster in ["kling3.0_pro", "totally-made-up-model"] {
            assert!(
                model_caps(off_roster).is_none(),
                "the fixture is genuinely off-roster, so this test is non-vacuous"
            );
            let s = build_submission(
                off_roster,
                RequestModality::Video,
                "a prompt".into(),
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("'{off_roster}' must reach submission: {e}"));
            assert_eq!(s.endpoint, RUNWAY_ENDPOINT_TEXT_TO_VIDEO);
            assert_eq!(s.suggested_ext, "mp4");
            match &s.body {
                RunwayRequestBody::Video(v) => {
                    assert_eq!(v.model, off_roster, "the id travels to the wire VERBATIM")
                }
                other => panic!("expected a video body, got {other:?}"),
            }
        }
    }

    /// Endpoint selection is a TOTAL function over (modality, frames supplied)
    /// — one assertion per row of B.9's table, and no fallthrough arm exists.
    ///
    /// The model id is deliberately off-roster on every video row: if any row
    /// still consulted `model_caps`, an unknown id could not satisfy it.
    #[test]
    fn endpoint_selection_is_total_over_modality_and_frames() {
        let first = ref_frame();
        let last = ReferenceImage {
            bytes: vec![9, 9, 9, 9],
            width: 1280,
            height: 720,
        };
        let unknown = "some-model-runway-ships-next-year";

        // (Video, None, None) -> text_to_video
        let s = build_submission(unknown, RequestModality::Video, "p".into(), None, None)
            .expect("text->video");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_TEXT_TO_VIDEO);
        assert!(matches!(
            &s.body,
            RunwayRequestBody::Video(v) if v.prompt_image.is_none()
        ));

        // (Video, Some, None) -> image_to_video / Single
        let s = build_submission(
            unknown,
            RequestModality::Video,
            "p".into(),
            Some(&first),
            None,
        )
        .expect("image->video");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_IMAGE_TO_VIDEO);
        assert!(matches!(
            &s.body,
            RunwayRequestBody::Video(v) if matches!(v.prompt_image, Some(RunwayPromptImage::Single(_)))
        ));

        // (Video, Some, Some) -> image_to_video / Keyframes[first, last]
        let s = build_submission(
            unknown,
            RequestModality::Video,
            "p".into(),
            Some(&first),
            Some(&last),
        )
        .expect("keyframe pair");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_IMAGE_TO_VIDEO);
        match &s.body {
            RunwayRequestBody::Video(v) => match &v.prompt_image {
                Some(RunwayPromptImage::Keyframes(frames)) => {
                    assert_eq!(frames.len(), 2);
                    assert_eq!(frames[0].position, RunwayKeyframePosition::First);
                    assert_eq!(frames[1].position, RunwayKeyframePosition::Last);
                }
                other => panic!("expected a keyframe pair, got {other:?}"),
            },
            other => panic!("expected a video body, got {other:?}"),
        }

        // (Video, None, Some) -> image_to_video / Keyframes[last]
        //   "interpolate INTO a target" — resolved EXPLICITLY, never by
        //   fallthrough (build_image_to_video_request:392).
        let s = build_submission(
            unknown,
            RequestModality::Video,
            "p".into(),
            None,
            Some(&last),
        )
        .expect("lone last frame");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_IMAGE_TO_VIDEO);
        match &s.body {
            RunwayRequestBody::Video(v) => match &v.prompt_image {
                Some(RunwayPromptImage::Keyframes(frames)) => {
                    assert_eq!(frames.len(), 1);
                    assert_eq!(frames[0].position, RunwayKeyframePosition::Last);
                }
                other => panic!("expected a lone last keyframe, got {other:?}"),
            },
            other => panic!("expected a video body, got {other:?}"),
        }

        // (Image, Some reference, None) -> text_to_image, reference attached.
        //   Off-roster on purpose: the image arm consults no caps row either.
        let s = build_submission(
            unknown,
            RequestModality::Image,
            "p".into(),
            Some(&first),
            None,
        )
        .expect("text->image");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_TEXT_TO_IMAGE);
        assert_eq!(s.suggested_ext, "png");
        match &s.body {
            RunwayRequestBody::TextToImage(r) => assert_eq!(
                r.reference_images.as_ref().map(Vec::len),
                Some(1),
                "a supplied reference is ALWAYS attached now — Runway 400s if \
                 the model does not read it"
            ),
            other => panic!("expected a still-image body, got {other:?}"),
        }

        // (Image, _, Some destination) -> InvalidRequest. A CATEGORY error (a
        // single image has no last frame), not a capability gate — the one
        // check that survives, re-keyed to modality.
        let err = build_submission(
            unknown,
            RequestModality::Image,
            "p".into(),
            Some(&first),
            Some(&last),
        )
        .expect_err("a destination frame is meaningless for a still image");
        assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
        assert!(err.to_string().contains("destination"), "{err}");

        // (Audio, _, _) -> InvalidRequest, explicitly. There is no fallthrough
        // arm: `build_submission`'s `match modality` is exhaustive, so a
        // fourth modality would break the build rather than pick an endpoint.
        for frames in [(None, None), (Some(&first), None), (Some(&first), Some(&last))] {
            let err = build_submission(unknown, RequestModality::Audio, "p".into(), frames.0, frames.1)
                .expect_err("audio never routes to Runway");
            assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
            assert!(err.to_string().contains("audio request"), "{err}");
        }
    }

    /// The seven capability gates B.10 deletes no longer refuse anything
    /// locally. Each row names the gate it retires and what it used to say.
    #[test]
    fn retired_capability_gates_no_longer_refuse_locally() {
        // #1 unknown-model: covered by the off-roster test above.

        // #2 `caps.deprecated`. Both retired ids stay in RUNWAY_MODELS as
        //    advisory rows, but the sunset (2026-07-30) has already passed, so
        //    Runway's own 400 covers them; a literal reading of D-01 ("no
        //    roster validation") wins over a discretionary local advisory.
        for retired in ["gen3a_turbo", "gen4_aleph"] {
            assert!(
                model_caps(retired).expect("still listed as advisory").deprecated,
                "the fixture really is flagged, so this row is non-vacuous"
            );
            assert!(
                build_submission(retired, RequestModality::Video, "x".into(), None, None).is_ok(),
                "'{retired}' is no longer refused locally"
            );
        }

        // #7 `!caps.text_to_video`. gen4_turbo's row says text_to_video:false;
        //    with no frames supplied the endpoint is text_to_video anyway.
        assert!(!model_caps("gen4_turbo").unwrap().text_to_video);
        let s = build_submission(
            "gen4_turbo",
            RequestModality::Video,
            "a red bicycle".into(),
            None,
            None,
        )
        .expect("no frames => text_to_video, unconditionally");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_TEXT_TO_VIDEO);

        // #9 keyframe_pair == Unsupported. Accepted by D-05: this refusal
        //    becomes a real Runway 400 rather than a local $0 one.
        assert_eq!(
            model_caps("gen4_turbo").unwrap().keyframe_pair,
            KeyframePairSupport::Unsupported
        );
        let s = build_submission(
            "gen4_turbo",
            RequestModality::Video,
            "bridge the shots".into(),
            Some(&ref_frame()),
            Some(&ref_frame()),
        )
        .expect("a keyframe pair is no longer refused locally");
        assert!(matches!(
            &s.body,
            RunwayRequestBody::Video(v) if matches!(v.prompt_image, Some(RunwayPromptImage::Keyframes(_)))
        ));

        // #3 `caps.prompt_text_required`. The agent seams already refuse an
        //    empty prompt before any provider work; other callers get the
        //    vendor 400.
        assert!(
            build_submission(
                "some-brand-new-model",
                RequestModality::Video,
                "   ".into(),
                None,
                None
            )
            .is_ok(),
            "the per-model empty-prompt gate is gone, with no replacement"
        );

        // #5 `!caps.tagged_reference_images`. gen4_image_turbo's row says the
        //    field is undocumented; the reference is attached regardless.
        assert!(!model_caps("gen4_image_turbo").unwrap().tagged_reference_images);
        let s = build_submission(
            "gen4_image_turbo",
            RequestModality::Image,
            "a logo".into(),
            Some(&ref_frame()),
            None,
        )
        .expect("a reference is always attached now");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_TEXT_TO_IMAGE);
        match &s.body {
            RunwayRequestBody::TextToImage(r) => assert_eq!(
                r.reference_images.as_ref().map(Vec::len),
                Some(1),
                "attached, not dropped"
            ),
            other => panic!("expected a still-image body, got {other:?}"),
        }

        // #6 `!caps.text_to_image`. A video-only roster row now serves the
        //    image arm if that is what the CALLER asked for.
        assert!(!model_caps("gen4_turbo").unwrap().text_to_image);
        assert!(build_submission(
            "gen4_turbo",
            RequestModality::Image,
            "a logo".into(),
            None,
            None
        )
        .is_ok());

        // #8 `!caps.image_to_video`. gen4_image's row says image_to_video is
        //    false; with a frame supplied to the VIDEO tool it routes anyway.
        assert!(!model_caps("gen4_image").unwrap().image_to_video);
        let s = build_submission(
            "gen4_image",
            RequestModality::Video,
            "pan left".into(),
            Some(&ref_frame()),
            None,
        )
        .expect("frames supplied => image_to_video, unconditionally");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_IMAGE_TO_VIDEO);
    }

    #[test]
    fn submission_routes_each_shape_to_the_right_endpoint_and_extension() {
        // text -> video
        let s = build_submission(
            "veo3.1_fast",
            RequestModality::Video,
            "a red bicycle".into(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_TEXT_TO_VIDEO);
        assert_eq!(s.suggested_ext, "mp4");
        assert!(matches!(s.body, RunwayRequestBody::Video(_)));

        // image -> video (a single first frame)
        let s = build_submission(
            "gen4_turbo",
            RequestModality::Video,
            "pan left".into(),
            Some(&ref_frame()),
            None,
        )
        .unwrap();
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_IMAGE_TO_VIDEO);
        assert_eq!(s.suggested_ext, "mp4");

        // text -> image
        let s = build_submission(
            "gen4_image",
            RequestModality::Image,
            "a red bicycle".into(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_TEXT_TO_IMAGE);
        assert_eq!(s.suggested_ext, "png", "a still lands as PNG, never .mp4");
        match &s.body {
            RunwayRequestBody::TextToImage(r) => {
                let v = serde_json::to_value(r).unwrap();
                let obj = v.as_object().unwrap();
                assert_eq!(obj.len(), 3, "an image body has no duration: {obj:?}");
                assert!(obj.contains_key("promptText") && obj.contains_key("ratio"));
            }
            other => panic!("expected a TextToImage body, got {other:?}"),
        }

        // The untagged enum puts the inner object straight on the wire.
        let s = build_submission(
            "gen4_turbo",
            RequestModality::Video,
            "pan left".into(),
            Some(&ref_frame()),
            None,
        )
        .unwrap();
        let v = serde_json::to_value(&s.body).unwrap();
        assert_eq!(v["model"], "gen4_turbo", "no discriminant key wraps it: {v}");
    }

    // `submission_rejects_unknown_and_deprecated_models`,
    // `submission_rejects_an_empty_prompt_where_the_model_requires_one` and
    // `submission_refuses_keyframes_on_a_model_that_does_not_support_them`
    // were DELETED by Phase 55.1. Each pinned one of the seven capability
    // gates B.10 retires (verdicts #1/#2, #3 and #9), so each asserted the
    // exact behavior D-01/D-05 remove — they are not weakened, they are
    // superseded by `off_roster_model_id_reaches_submission_with_no_local_gate`
    // and `retired_capability_gates_no_longer_refuse_locally` above, which
    // assert the OPPOSITE on the same fixtures.
    //
    // The keyframe-pair refusal in particular is recorded here rather than
    // just removed: 42.1 called it "the failure mode this phase most needs to
    // avoid", because Runway would generate *something* from the first frame
    // alone and an A→B transition would silently become an ordinary
    // image→video clip. D-05 accepts that risk knowingly — the refusal moves
    // to Runway's server-side 400, which costs $0 but is no longer local and
    // no longer catches a model whose keyframe support Rudis cannot look up.

    /// A first+last pair still builds the ORDERED `Keyframes` body — the one
    /// property the deleted keyframe gate was protecting that does NOT depend
    /// on knowing the model. An inverted pair generates the transition
    /// backwards and looks entirely plausible in review.
    #[test]
    fn a_first_and_last_pair_still_builds_an_ordered_keyframe_body() {
        // `seedance2` was reached through a named fallback const until Phase 55.1
        // plan 06 deleted it; the id is a literal here for the same reason
        // "not-on-the-roster" is — the point is that the builder does not care.
        for model in ["gen4_turbo", "seedance2", "not-on-the-roster"] {
            let s = build_submission(
                model,
                RequestModality::Video,
                "bridge the shots".into(),
                Some(&ref_frame()),
                Some(&ReferenceImage {
                    bytes: vec![4, 5, 6, 7],
                    width: 1280,
                    height: 720,
                }),
            )
            .unwrap_or_else(|e| panic!("'{model}' builds a pair: {e}"));
            assert_eq!(s.endpoint, RUNWAY_ENDPOINT_IMAGE_TO_VIDEO);
            match &s.body {
                RunwayRequestBody::Video(v) => match &v.prompt_image {
                    Some(RunwayPromptImage::Keyframes(frames)) => {
                        assert_eq!(frames.len(), 2, "{model}");
                        assert_eq!(frames[0].position, RunwayKeyframePosition::First);
                        assert_eq!(frames[1].position, RunwayKeyframePosition::Last);
                        assert_ne!(
                            frames[0].uri.as_str(),
                            frames[1].uri.as_str(),
                            "the two frames are distinct, so not-swapped is non-vacuous"
                        );
                        assert_eq!(frames[0].uri.as_str(), data_uri(&ref_frame().bytes).as_str());
                    }
                    other => panic!("expected a keyframe pair, got {other:?}"),
                },
                other => panic!("expected a video body, got {other:?}"),
            }
        }
    }

    /// **42.1-03 — this test's claim INVERTED, and why.**
    ///
    /// 42.1-01/02 asserted that a conditioning frame on a still-image model was
    /// REFUSED, because `referenceImages` was not built. 42.1-03 builds it, so
    /// the same input must now SUCCEED and carry both halves of Runway's
    /// two-part tagged-reference contract. The refusal is not merely dropped —
    /// it is replaced by the stronger claim that the capability really works,
    /// which is what the v5-shipped sketch conditioning needs.
    #[test]
    fn a_reference_frame_on_the_image_model_builds_a_cited_tagged_reference() {
        let s = build_submission(
            "gen4_image",
            RequestModality::Image,
            "a logo".into(),
            Some(&ref_frame()),
            None,
        )
        .expect("gen4_image documents the tagged referenceImages field");
        assert_eq!(s.endpoint, RUNWAY_ENDPOINT_TEXT_TO_IMAGE);
        assert_eq!(s.suggested_ext, "png");
        let image = match &s.body {
            RunwayRequestBody::TextToImage(r) => r,
            other => panic!("expected a still-image body, got {other:?}"),
        };
        let refs = image
            .reference_images
            .as_ref()
            .expect("the reference is attached, not dropped");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].tag, RUNWAY_REFERENCE_TAG);
        // The bytes really travel — as OUR data URI, never a caller URL.
        assert_eq!(refs[0].uri.as_str(), data_uri(&ref_frame().bytes).as_str());
        // ...and the tag is CITED. An attached-but-uncited tag is the one shape
        // 42.1-01 refused to guess at; this is what makes the guess unnecessary.
        assert!(
            image.prompt_text.contains(&format!("@{RUNWAY_REFERENCE_TAG}")),
            "the tag must be cited inside promptText: {}",
            image.prompt_text
        );
        // The agent's own prompt survives verbatim at the front.
        assert!(image.prompt_text.starts_with("a logo"), "{}", image.prompt_text);

        // An UNCONDITIONED still image is byte-identical to what 42.1-01
        // shipped: the key is omitted entirely, never sent as `null`.
        let plain = build_submission(
            "gen4_image",
            RequestModality::Image,
            "a logo".into(),
            None,
            None,
        )
        .unwrap();
        let plain_json = serde_json::to_value(&plain.body).unwrap();
        assert!(
            plain_json.get("referenceImages").is_none(),
            "an unconditioned image must not carry the key at all: {plain_json}"
        );
        assert_eq!(plain_json["promptText"], "a logo");
    }

    /// The citation const and the tag const cannot drift apart — the citation
    /// is the ONLY place the `@…` token is written, so if it stopped naming the
    /// tag Rudis would send exactly the uncited shape 42.1-01 refused to build.
    #[test]
    fn the_reference_citation_names_the_reference_tag() {
        assert!(
            RUNWAY_REFERENCE_CITATION.contains(&format!("@{RUNWAY_REFERENCE_TAG}")),
            "citation `{RUNWAY_REFERENCE_CITATION}` must name @{RUNWAY_REFERENCE_TAG}"
        );
        // Conservative tag shape: alphanumeric, letter-initial. Runway's tag
        // rules are documented loosely; this is the intersection that is safe
        // under every reading of them.
        assert!(RUNWAY_REFERENCE_TAG.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(RUNWAY_REFERENCE_TAG
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic()));
    }

    /// The ONE refusal that survives Phase 55.1, and the reason it is not a
    /// capability gate: a DESTINATION frame on a still-image request is a
    /// CATEGORY error — no Runway field could ever express "the last frame of
    /// a single image" — so it holds for a model nobody has ever heard of just
    /// as firmly as for `gen4_image`. Its trigger is now the caller's
    /// `RequestModality`, never `caps.modality`.
    ///
    /// (The other half of this test — refusing a reference on a model whose
    /// caps did not document the tagged `referenceImages` field — was verdict
    /// #5 and is DELETED; `retired_capability_gates_no_longer_refuse_locally`
    /// pins its replacement on the same `gen4_image_turbo` fixture.)
    #[test]
    fn the_image_path_still_refuses_a_destination_frame_on_any_model() {
        for model in ["gen4_image", "a-model-shipping-next-year"] {
            let err = build_submission(
                model,
                RequestModality::Image,
                "a logo".into(),
                Some(&ref_frame()),
                Some(&ref_frame()),
            )
            .expect_err("a still image has no last frame to interpolate towards")
            .to_string();
            assert!(err.contains("still-image request"), "{err}");
            assert!(err.contains("destination"), "{err}");
            assert!(
                !err.contains("not a known Runway model"),
                "the refusal must not imply anything about the model EXISTING: {err}"
            );
        }
    }

    /// Audio is refused EXPLICITLY, not by fallthrough — the arm that stops a
    /// future mis-wiring turning a voice prompt into a video request.
    #[test]
    fn an_audio_request_cannot_route_to_runway() {
        let err = build_submission(
            agent_llm_free_model_id(),
            RequestModality::Audio,
            "speak this".into(),
            None,
            None,
        )
        .expect_err("audio never routes to Runway");
        assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
        assert!(err.to_string().contains("audio request"), "{err}");
    }

    /// A free-text id used only to prove the audio arm fires before ANY
    /// model-shaped reasoning could.
    fn agent_llm_free_model_id() -> &'static str {
        "eleven_multilingual_v2"
    }

    // --- the async lifecycle, proven without a socket ---

    #[test]
    fn runway_submit_response_parses_to_an_opaque_task_id() {
        let parsed = parse_submit_response(r#"{"id":"17f2e0b1-abc"}"#).unwrap();
        let handle = JobHandle::new(JobId::mint(), parsed.id);
        assert_eq!(
            handle.provider_job_ref, "17f2e0b1-abc",
            "the task id is stored verbatim, never parsed/reconstructed"
        );
        assert!(parse_submit_response("not json").is_err());
    }

    /// All six lifecycle statuses map, an empty-output success is an honest
    /// failure, and an UNRECOGNISED status is terminal rather than an infinite
    /// poll.
    #[test]
    fn runway_poll_decision_maps_every_status() {
        let decide = |json: &str| task_to_poll_decision(&parse_task_envelope(json).unwrap());

        for pending in ["PENDING", "THROTTLED", "RUNNING", "running"] {
            assert_eq!(
                decide(&format!(r#"{{"id":"t","status":"{pending}"}}"#)),
                RunwayPollDecision::StillPending,
                "{pending} keeps polling"
            );
        }

        assert_eq!(
            decide(r#"{"id":"t","status":"SUCCEEDED","output":["https://cdn.example.com/a.mp4"]}"#),
            RunwayPollDecision::Download("https://cdn.example.com/a.mp4".to_string())
        );

        match decide(r#"{"id":"t","status":"SUCCEEDED","output":[]}"#) {
            RunwayPollDecision::Failed(msg) => assert!(
                msg.contains("zero outputs"),
                "a success with nothing to fetch is an honest failure: {msg}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
        match decide(r#"{"id":"t","status":"SUCCEEDED"}"#) {
            RunwayPollDecision::Failed(msg) => assert!(msg.contains("zero outputs"), "{msg}"),
            other => panic!("expected Failed for a missing output key, got {other:?}"),
        }

        match decide(
            r#"{"id":"t","status":"FAILED","failure":"content moderation","failureCode":"SAFETY.INPUT.TEXT"}"#,
        ) {
            RunwayPollDecision::Failed(msg) => {
                assert!(msg.contains("content moderation"), "{msg}");
                assert!(msg.contains("SAFETY.INPUT.TEXT"), "{msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        assert_eq!(
            decide(r#"{"id":"t","status":"CANCELLED"}"#),
            RunwayPollDecision::Cancelled
        );

        match decide(r#"{"id":"t","status":"ASCENDED"}"#) {
            RunwayPollDecision::Failed(msg) => assert!(
                msg.contains("unrecognised Runway task status"),
                "an unknown status is terminal, never an infinite poll: {msg}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }

        assert!(parse_task_envelope("{}").is_err(), "a bodyless task is a clean Err");
    }

    /// The SSRF gate on the ONE response-supplied URL. Runway serves outputs
    /// from an unpinned CDN, so this blocks the SHAPES that make SSRF useful
    /// rather than pinning a guessed host suffix.
    #[test]
    fn runway_output_uri_gate_blocks_the_ssrf_shapes() {
        for ok in [
            "https://dnznrvs05pmza.cloudfront.net/abc.mp4",
            "https://api.dev.runwayml.com/v1/x",
            "https://storage.example.co.uk/a/b.png?sig=1",
        ] {
            validate_output_uri(ok).unwrap_or_else(|e| panic!("must accept {ok}: {e}"));
        }

        for (bad, why) in [
            ("http://cdn.example.com/a.mp4", "scheme"),
            ("ftp://cdn.example.com/a.mp4", "scheme"),
            ("file:///c:/windows/system32/config/sam", "scheme"),
            ("https://user:pass@cdn.example.com/a", "embedded credentials"),
            ("https://169.254.169.254/latest/meta-data/", "cloud metadata IP"),
            ("https://127.0.0.1/a", "loopback"),
            ("https://10.0.0.5/a", "RFC-1918"),
            ("https://[::1]/a", "IPv6 loopback"),
            ("https://localhost/a", "single-label host"),
            ("not-a-url", "garbage"),
            ("/relative/path", "relative"),
        ] {
            let err = validate_output_uri(bad).expect_err(&format!("must reject {bad} ({why})"));
            assert!(
                matches!(err, GenError::Provider(_)),
                "rejection is a clean Provider error for {bad}: {err}"
            );
        }
    }

    /// The error-mapping helper surfaces Runway's body verbatim but CANNOT
    /// contain the API key — it never even receives it (T-31-01).
    #[test]
    fn runway_non_success_maps_to_provider_error_without_the_key() {
        let body = r#"{"error":"You do not have enough credits to run this task."}"#;
        let _key_in_scope = SENTINEL_KEY; // present in context, NOT an argument
        let err = non_success_to_gen_error(402, body);
        let text = err.to_string();
        assert!(text.contains("enough credits"), "{text}");
        assert!(text.contains("402"), "{text}");
        assert!(
            !text.contains(SENTINEL_KEY),
            "the API key must NEVER appear in an error string: {text}"
        );
    }

    /// Poll cadence: at or above Runway's documented 5s floor, ALWAYS, with
    /// real jitter on top. Bounds are proven purely; the live source is then
    /// sampled to prove it actually varies.
    #[test]
    fn poll_interval_respects_the_five_second_floor_and_really_jitters() {
        assert_eq!(RUNWAY_POLL_INTERVAL_FLOOR, std::time::Duration::from_secs(5));
        assert_eq!(RUNWAY_POLL_JITTER_SPAN, std::time::Duration::from_secs(3));
        let ceiling = RUNWAY_POLL_INTERVAL_FLOOR + RUNWAY_POLL_JITTER_SPAN;

        // Pure bounds, across the whole entropy range including the edges.
        for entropy in [0u64, 1, 2_999, 3_000, 3_001, u64::MAX / 2, u64::MAX] {
            let d = jittered_poll_interval(entropy);
            assert!(
                d >= RUNWAY_POLL_INTERVAL_FLOOR && d < ceiling,
                "entropy {entropy} produced {d:?}, outside [5s, 8s)"
            );
        }
        assert_eq!(jittered_poll_interval(0), RUNWAY_POLL_INTERVAL_FLOOR);
        assert_ne!(
            jittered_poll_interval(0),
            jittered_poll_interval(1_500),
            "distinct entropy really does move the interval"
        );

        // The live source: bounded, and not a constant.
        let p = RunwayProvider::with_key("k".to_string());
        let samples: Vec<std::time::Duration> = (0..200).map(|_| p.poll_interval()).collect();
        for d in &samples {
            assert!(
                *d >= RUNWAY_POLL_INTERVAL_FLOOR && *d < ceiling,
                "sampled {d:?}, outside [5s, 8s)"
            );
        }
        let distinct = samples
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(
            distinct > 1,
            "200 samples produced one value — the jitter source is not varying"
        );
        assert_ne!(
            p.poll_interval(),
            crate::provider::DEFAULT_POLL_INTERVAL,
            "Runway overrides the historic 250ms default"
        );
    }

    /// The download cap mirrors the landing bridge's own bound (256 MiB), same
    /// as Veo's.
    #[test]
    fn runway_download_cap_is_256_mib() {
        // Deliberately re-stated rather than imported from `veo.rs` — that
        // const is private, and this wave is ADDITIVE ONLY (no existing
        // provider file is touched; that is Plan 02's scope).
        assert_eq!(RUNWAY_MAX_DOWNLOAD_BYTES, 256 * 1024 * 1024);
    }

    /// The submit-time extension record is what keeps a still from landing as
    /// `.mp4` WITHOUT sniffing the response (T-31-12), and a poll for a task
    /// this instance never submitted is a clean Err, never a guess.
    ///
    /// **55.1:** the extension's SOURCE moved from a model-table lookup to the
    /// caller's `RequestModality` (see `build_submission`). It is still a fixed
    /// literal decided before egress — which is the whole of T-31-12 — and it
    /// now resolves for an off-roster id too, where the table returned `None`.
    #[test]
    fn output_extension_comes_from_the_request_not_the_response() {
        assert_eq!(
            build_submission("gen4_image", RequestModality::Image, "p".into(), None, None)
                .unwrap()
                .suggested_ext,
            "png"
        );
        assert_eq!(
            build_submission("veo3.1_fast", RequestModality::Video, "p".into(), None, None)
                .unwrap()
                .suggested_ext,
            "mp4"
        );
        assert_eq!(
            build_submission("nope", RequestModality::Video, "p".into(), None, None)
                .unwrap()
                .suggested_ext,
            "mp4",
            "an off-roster id resolves an extension instead of returning None"
        );

        let p = RunwayProvider::with_key("k".to_string());
        assert_eq!(p.task_ext("task-1"), None);
        p.remember_task("task-1", "png");
        assert_eq!(p.task_ext("task-1"), Some("png"));
        p.forget_task("task-1");
        assert_eq!(
            p.task_ext("task-1"),
            None,
            "a terminal task is forgotten — the map cannot grow across a session"
        );
    }

    /// `cancel` is best-effort and infallible-`Ok`.
    ///
    /// **Deliberately NOT awaited** in the offline suite: awaiting `.send()`
    /// would attempt real DNS/egress, violating this wave's hard zero-network
    /// rule. The posture is instead proven structurally (a single swallowed
    /// `let _ = …send().await;` followed by `Ok(())`); this test pins the RETURN
    /// TYPE that guarantees a caller can never observe a cancel error, without
    /// opening a socket.
    #[test]
    fn runway_cancel_is_best_effort_ok_by_type() {
        fn assert_infallible_shape<'a>(
            p: &'a RunwayProvider,
            j: &'a JobHandle,
        ) -> impl std::future::Future<Output = Result<(), GenError>> + 'a {
            p.cancel(j)
        }
        let p = RunwayProvider::with_key("k".to_string());
        let j = JobHandle::new(JobId::mint(), "task-x".to_string());
        // Construct the future (no poll/await → no send → no network) and drop it.
        let _fut = assert_infallible_shape(&p, &j);
    }

    /// The providers this migration was NOT allowed to disturb are undisturbed.
    ///
    /// Plan 01 wrote this as "Fixture / OpenAI / Veo are untouched" — its
    /// additive-change proof. Plan 02 then deleted OpenAI and Veo *on purpose*,
    /// which is why the test is REWRITTEN rather than dropped: the scope boundary
    /// it guards is still live, it just has different subjects now. The surviving
    /// pair is the always-compiled fixture double and — locked decision 2 — the
    /// ENTIRE ElevenLabs audio path, which the single-provider collapse must not
    /// touch. If a future edit gave either of them Runway's poll cadence, this
    /// fails.
    #[test]
    fn the_untouched_providers_are_untouched() {
        assert_eq!(
            crate::FixtureGenProvider::sync_still(vec![1], "png").poll_interval(),
            crate::provider::DEFAULT_POLL_INTERVAL,
            "the fixture double keeps the historic 250ms default"
        );
        assert_eq!(
            crate::ElevenLabsProvider::with_key("k".to_string()).poll_interval(),
            crate::provider::DEFAULT_POLL_INTERVAL,
            "the audio provider is synchronous and keeps the 250ms default — \
             the 42.1 collapse did not reach it"
        );
        // Non-vacuity: Runway's cadence really is different, so "untouched" is a
        // claim that could have failed.
        assert!(
            RunwayProvider::with_key("k".to_string()).poll_interval()
                > crate::provider::DEFAULT_POLL_INTERVAL
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 56 (GEN-11) — `POST /v1/video_to_video`.
//
// A SEPARATE `#[cfg(test)]` module rather than more tests appended to the one
// above, because these tests have a different evidential standing and the
// separation is the record of it: every field name and every bound below was
// written AFTER `artifacts/56-01-PROBE-RESULTS.md` and
// `artifacts/56-F1-PROBE-RESULTS.md` existed, and each asserts a LIVE-PROBED
// fact rather than a documented one. Mixing them into the 42.1 suite would
// erase which is which.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod v2v_tests {
    use super::*;

    /// Not a real MP4 — the transport types never parse the bytes, they only
    /// wrap them. The one property that matters is that whatever goes in comes
    /// back out byte-identically.
    const FAKE_MP4: &[u8] = b"\x00\x00\x00\x18ftypmp42";

    // -----------------------------------------------------------------------
    // Task 1 — the two SSRF-safe video-input forms (SC-3 / D-03 / T-56-SSRF-01).
    // -----------------------------------------------------------------------

    /// The video prefix is its OWN fixed literal — `video/mp4`, not the image
    /// module's `image/png`, and never derived from a sniffed header or a
    /// caller-supplied mime.
    #[test]
    fn video_data_uri_carries_the_fixed_video_prefix() {
        let uri = video_data_uri(b"xx");
        let json = serde_json::to_value(&uri).expect("a video data URI serializes");
        let s = json
            .as_str()
            .expect("`#[serde(transparent)]` puts a BARE JSON string on the wire, not an object");
        assert!(
            s.starts_with("data:video/mp4;base64,"),
            "the mime is a fixed literal, never derived: {s}"
        );
        assert_ne!(
            RUNWAY_VIDEO_DATA_URI_PREFIX, RUNWAY_DATA_URI_PREFIX,
            "the video prefix is a DIFFERENT literal from the image one — a \
             shared const would put PNG bytes behind a video mime"
        );

        // Round-trips the exact bytes through the crate-standard engine.
        let b64 = s.strip_prefix(RUNWAY_VIDEO_DATA_URI_PREFIX).unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(b64).unwrap(),
            b"xx",
            "base64 round-trips to the EXACT input bytes"
        );

        // There is no code path that produces a bare/unprefixed value.
        assert_eq!(video_data_uri(&[]).as_str(), RUNWAY_VIDEO_DATA_URI_PREFIX);
        assert!(!video_data_uri(&[]).is_empty(), "the prefix alone is non-empty");
    }

    /// The 16 MB per-asset bound refuses BEFORE egress, and the message names
    /// the LENGTH and the cap — never the URI, which is megabytes of base64 and
    /// would flood any log it reached.
    #[test]
    fn video_data_uri_size_bound_rejects_over_sixteen_megabytes() {
        assert!(
            check_video_data_uri_size(&video_data_uri(FAKE_MP4)).is_ok(),
            "a small clip is far under the cap"
        );

        // 13 MiB of raw bytes base64-expands 4/3 to ~18.2 MB — over the cap.
        let oversized = video_data_uri(&vec![0u8; 13 * 1024 * 1024]);
        assert!(
            oversized.len() > RUNWAY_MAX_VIDEO_DATA_URI_BYTES,
            "the fixture really is over the cap, so this test is non-vacuous"
        );
        let err =
            check_video_data_uri_size(&oversized).expect_err("an oversized clip is refused locally");
        assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains(&oversized.len().to_string()),
            "the message names the LENGTH: {msg}"
        );
        assert!(
            msg.contains(&RUNWAY_MAX_VIDEO_DATA_URI_BYTES.to_string()),
            "the message names the CAP: {msg}"
        );
        assert!(
            !msg.contains("data:video/mp4;base64,"),
            "the message must never echo the URI itself: {} chars",
            msg.len()
        );
    }

    /// T-56-SSRF-01: a forged or compromised `/v1/uploads` response cannot
    /// smuggle an `https://` egress target into a request field, because the
    /// ONLY way a [`RunwayUploadedAsset`] comes into existence rejects anything
    /// that is not a `runway://` handle.
    #[test]
    fn runway_uploaded_asset_rejects_non_runway_scheme_at_deserialize() {
        let ok: RunwayUploadedAsset =
            serde_json::from_str(r#""runway://tasks/abc""#).expect("a real handle deserializes");
        assert_eq!(ok.as_str(), "runway://tasks/abc");

        for hostile in [
            r#""https://attacker.example/x""#,
            r#""http://169.254.169.254/latest/meta-data/""#,
            r#""file:///C:/Windows/System32/config/SAM""#,
            r#""//attacker.example/x""#,
            r#""RUNWAY://tasks/abc""#,
            r#""x-runway://tasks/abc""#,
            r#""""#,
        ] {
            let err = match serde_json::from_str::<RunwayUploadedAsset>(hostile) {
                Ok(smuggled) => panic!(
                    "a non-runway:// value must be REJECTED at deserialize, but \
                     {hostile} produced {smuggled:?}"
                ),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("runway://"),
                "the rejection names the required scheme: {err}"
            );
            assert!(
                !err.to_string().contains("attacker.example"),
                "the rejection must NOT echo the hostile value into a log: {err}"
            );
        }
    }

    /// TRIPWIRE, born in the SAME commit as the types it guards
    /// (56-VALIDATION Wave-0 requirement).
    ///
    /// A [`RunwayUploadedAsset`] is constructed here the ONLY way it can be:
    /// by deserializing a real `/v1/uploads` response body. There is
    /// deliberately no `new`, no `From<String>`, no `FromStr` and no public
    /// field — **and there must never be one.** Adding any of those would make
    /// a caller- or prompt-chosen host expressible in a request field, which is
    /// exactly the SSRF surface `RunwayDataUri`'s private-inner discipline
    /// (T-31-09) exists to make unrepresentable.
    ///
    /// The exhaustive destructure of [`RunwayUploadsResponse`] is the second
    /// half: an added field fails to compile here and forces a review of what
    /// new server-controlled value just became reachable.
    #[test]
    fn runway_uploaded_asset_has_no_caller_reachable_constructor() {
        let fixture = r#"{
            "uploadUrl": "https://uploads.runwayml.com/presigned/abc",
            "fields": { "key": "uploads/abc", "policy": "eyJ..." },
            "runwayUri": "runway://uploads/abc"
        }"#;
        let resp: RunwayUploadsResponse =
            serde_json::from_str(fixture).expect("the documented response body deserializes");

        let RunwayUploadsResponse {
            upload_url,
            fields,
            runway_uri,
        } = &resp;
        assert_eq!(upload_url, "https://uploads.runwayml.com/presigned/abc");
        assert_eq!(fields.len(), 2, "every presigned form field survives verbatim");
        assert_eq!(
            runway_uri.as_str(),
            "runway://uploads/abc",
            "the handle round-trips read-only through the ONLY accessor"
        );

        // And it serializes back as a bare string, so the request field carries
        // exactly the server's own handle and nothing Rudis invented.
        assert_eq!(
            serde_json::to_value(runway_uri).unwrap(),
            serde_json::json!("runway://uploads/abc")
        );

        // Both arms of the video-input union are private-inner one-constructor
        // types, so neither can carry a caller-chosen host.
        let inputs = [
            RunwayVideoInput::DataUri(video_data_uri(FAKE_MP4)),
            RunwayVideoInput::Uploaded(runway_uri.clone()),
        ];
        for input in &inputs {
            let v = serde_json::to_value(input).unwrap();
            let s = v.as_str().expect("both arms are BARE strings on the wire");
            assert!(
                s.starts_with(RUNWAY_VIDEO_DATA_URI_PREFIX) || s.starts_with("runway://"),
                "a video input is ALWAYS one of the two Rudis-controlled forms: {s}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Task 2 — the size-gated transport dispatch and the /v1/uploads two-step
    // (D-03 / T-56-SSRF-02 / T-56-INJ-01).
    // -----------------------------------------------------------------------

    /// The transport decision is a PURE function of two lengths, so the whole
    /// of D-03's routing is testable with no client, no socket and no bytes.
    ///
    /// The boundary matters and is asserted exactly: `== cap` still inlines.
    /// An off-by-one here is a 400 on every clip that happens to land on the
    /// cap, which is precisely the class of bug 42.1-04 found in Runway's
    /// documented `duration` range.
    #[test]
    fn video_transport_for_len_dispatches_at_the_data_uri_cap() {
        // Under and exactly AT the cap: inline.
        for len in [0, 1, RUNWAY_MAX_VIDEO_DATA_URI_BYTES] {
            assert_eq!(
                video_transport_for_len(len, 1024).unwrap(),
                VideoTransport::DataUri,
                "a serialized data URI of {len} bytes is at or under the cap"
            );
        }

        // One byte over: the /v1/uploads overflow path.
        assert_eq!(
            video_transport_for_len(RUNWAY_MAX_VIDEO_DATA_URI_BYTES + 1, 13 * 1024 * 1024).unwrap(),
            VideoTransport::Upload,
            "one byte over the inline cap routes to the upload endpoint"
        );
        assert_eq!(
            video_transport_for_len(usize::MAX, RUNWAY_MAX_UPLOAD_BYTES).unwrap(),
            VideoTransport::Upload,
            "exactly at the 200 MB upload ceiling is still accepted"
        );

        // Over the upload ceiling: refused locally, naming BOTH caps.
        let err = video_transport_for_len(usize::MAX, RUNWAY_MAX_UPLOAD_BYTES + 1)
            .expect_err("over 200 MB there is no transport at all");
        assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains(&RUNWAY_MAX_VIDEO_DATA_URI_BYTES.to_string())
                && msg.contains(&RUNWAY_MAX_UPLOAD_BYTES.to_string()),
            "the refusal names BOTH caps so the caller can act on it: {msg}"
        );

        // Under Runway's 512-byte upload FLOOR, with a data URI over the inline
        // cap, is unreachable in practice but must still refuse cleanly rather
        // than fall through to a transport.
        let err = video_transport_for_len(RUNWAY_MAX_VIDEO_DATA_URI_BYTES + 1, 100)
            .expect_err("under the 512-byte upload floor there is no transport");
        assert!(
            err.to_string().contains(&RUNWAY_MIN_UPLOAD_BYTES.to_string()),
            "the refusal names the floor: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Debug session `v2v-413-on-4k-source-unprobed-upload-window` (2026-08-13).
    //
    // The owner's first live 4K clip edit met HTTP 413 twice. The cause was not
    // the upload window (probe F-4 read Runway's own signed S3 policy and it says
    // `content-length-range 512 .. 209715200`, agreeing with both shipped upload
    // constants exactly) but the INLINE arm: `RUNWAY_MAX_VIDEO_DATA_URI_BYTES`
    // was the DOCUMENTED 16 MB per-asset figure, 60% above the API host's real
    // request-body ceiling, which probe F-5 MEASURED at 10 MiB.
    // -----------------------------------------------------------------------

    /// The DEAD ZONE, closed — and named by the measurement that found it.
    ///
    /// Every data-URI length in the band that used to be inlined and refused at
    /// the edge must now route to `/v1/uploads`. The band's own endpoints are
    /// asserted, and so is the exact measured value from the owner's clip, so a
    /// future re-raise of the inline cap fails HERE rather than in a UAT.
    #[test]
    fn the_413_dead_zone_routes_to_upload_instead_of_inlining() {
        // What the owner's 2160x4096 clip produced at a 3.0 s range, measured on
        // the bundled LGPL sidecar: 8 631 057 raw bytes -> this data URI.
        const OWNER_CLIP_3S_DATA_URI_LEN: usize = 11_508_098;
        const OWNER_CLIP_3S_RAW_LEN: usize = 8_631_057;
        // What probe F-5 measured, to the kilobyte: 10 485 760 B was answered with
        // an HTTP 400 naming `videoUri`; 10 747 904 B was answered with
        // HTTP 413 `{"message":"Request Entity Too Large"}`.
        const MEASURED_GATEWAY_CEILING: usize = 10 * 1024 * 1024;
        // The old, documented, WRONG inline cap.
        const RETIRED_INLINE_CAP: usize = 16 * 1024 * 1024;

        assert_eq!(
            RUNWAY_MAX_REQUEST_BODY_BYTES, MEASURED_GATEWAY_CEILING,
            "the body bound must be the MEASURED ceiling, not an invented number"
        );
        assert!(
            RUNWAY_MAX_VIDEO_DATA_URI_BYTES < MEASURED_GATEWAY_CEILING,
            "the inline budget must sit strictly under the ceiling it is carved out of"
        );
        assert_eq!(
            RUNWAY_MAX_VIDEO_DATA_URI_BYTES + RUNWAY_BODY_SCAFFOLD_RESERVE_BYTES,
            RUNWAY_MAX_REQUEST_BODY_BYTES,
            "the inline budget is DERIVED, so there is exactly one number to correct"
        );

        // The owner's reproduction, in both coordinates.
        assert_eq!(
            video_transport_for_len(OWNER_CLIP_3S_DATA_URI_LEN, OWNER_CLIP_3S_RAW_LEN).unwrap(),
            VideoTransport::Upload,
            "the exact payload that earned two 413s must now take the upload arm"
        );

        // The whole band, at both ends and in the middle.
        for data_uri_len in [
            RUNWAY_MAX_VIDEO_DATA_URI_BYTES + 1,
            MEASURED_GATEWAY_CEILING,
            MEASURED_GATEWAY_CEILING + 1,
            OWNER_CLIP_3S_DATA_URI_LEN,
            13_128_134, // the same clip at 3.5 s
            14_777_978, // and at 4.0 s
            RETIRED_INLINE_CAP,
        ] {
            assert_eq!(
                video_transport_for_len(data_uri_len, 12 * 1024 * 1024).unwrap(),
                VideoTransport::Upload,
                "a {data_uri_len}-byte data URI was INLINED under the retired \
                 {RETIRED_INLINE_CAP}-byte cap and refused at the edge with a \
                 contentless 413; it must take the upload arm"
            );
        }

        // And the documented per-asset figure survives as EVIDENCE, unenforced.
        assert_eq!(RUNWAY_DOCUMENTED_VIDEO_ASSET_CAP_BYTES, RETIRED_INLINE_CAP);
        assert!(
            RUNWAY_DOCUMENTED_VIDEO_ASSET_CAP_BYTES > RUNWAY_MAX_VIDEO_DATA_URI_BYTES,
            "the documented asset cap is real but unreachable — the body ceiling binds first"
        );
    }

    /// The scaffold reserve is PROVEN sufficient rather than asserted generous:
    /// a maximal legal inline URI, a maximal-length prompt and a long model id
    /// still serialize under the MEASURED ceiling.
    ///
    /// The prompt bound is `app_core::MAX_AGENT_VIDEO_PROMPT_CHARS` = 4 000
    /// CHARS; this uses a 4-byte-UTF-8 char throughout, which is the worst case
    /// serde_json emits for a non-escaped code point, so the body built here is
    /// bigger than any real one.
    #[test]
    fn the_scaffold_reserve_covers_a_maximal_body() {
        let uri = RunwayVideoDataUri(
            // Exactly at the inline budget — the largest a caller can get past
            // `check_video_data_uri_size`.
            "x".repeat(RUNWAY_MAX_VIDEO_DATA_URI_BYTES),
        );
        assert!(
            check_video_data_uri_size(&uri).is_ok(),
            "exactly at the budget is INSIDE it"
        );
        let body = RunwayVideoToVideoRequest {
            model: "m".repeat(1024),
            video_uri: RunwayVideoInput::DataUri(uri),
            prompt_text: "\u{10348}".repeat(4_000),
        };
        let measured = serde_json::to_vec(&body).expect("serializes").len();
        assert!(
            measured <= RUNWAY_MAX_REQUEST_BODY_BYTES,
            "a maximal body must still fit the MEASURED {RUNWAY_MAX_REQUEST_BODY_BYTES}-byte \
             ceiling; this one is {measured} bytes, so the \
             {RUNWAY_BODY_SCAFFOLD_RESERVE_BYTES}-byte reserve is too small"
        );
    }

    /// The too-big refusal is D-01-shaped: the MEASURED value, the named bound,
    /// and a remedy that is RIGHT FOR THIS EDGE — never a split.
    #[test]
    fn the_over_upload_ceiling_refusal_names_the_bound_and_a_per_edge_remedy() {
        let too_big = RUNWAY_MAX_UPLOAD_BYTES + 1;
        let msg = video_transport_for_len(usize::MAX, too_big)
            .expect_err("over the upload ceiling there is no transport")
            .to_string();

        assert!(
            msg.contains(&too_big.to_string()),
            "the refusal must quote the clip's MEASURED size, not just say it is too big: {msg}"
        );
        assert!(
            msg.contains(&RUNWAY_MAX_UPLOAD_BYTES.to_string()),
            "and name the bound it broke: {msg}"
        );
        assert!(
            msg.contains("Trim it to a shorter range"),
            "the remedy for a TOO BIG clip is a shorter range: {msg}"
        );
        assert!(
            msg.contains("lower-resolution"),
            "…or a smaller frame, which is the other real lever: {msg}"
        );
        // The wrong remedy, borrowed from the D-01 floor test's discipline: a
        // split leaves both halves on the timeline and the user still has to
        // pick one, and a half short enough to fit may then be under the D-01
        // floor. Offering it would make the refusal dishonest.
        assert!(
            !msg.contains("split"),
            "offering a SPLIT for a byte-oversized clip is the wrong remedy: {msg}"
        );
        assert!(
            msg.contains("truncated"),
            "and it must say a silently truncated range is not what happens: {msg}"
        );
    }

    /// The three transport refusals carry the literals
    /// `app_core::PRE_SPEND_VALIDATION_SUBSTRINGS` matches on, so a $0.00
    /// pre-egress refusal never burns one of the turn's two paid retry slots
    /// (the WR-01 defect class). Pinned HERE because the strings live here.
    #[test]
    fn the_transport_refusals_carry_the_pre_spend_needles() {
        let over_ceiling = video_transport_for_len(usize::MAX, RUNWAY_MAX_UPLOAD_BYTES + 1)
            .expect_err("over the ceiling")
            .to_string();
        assert!(
            over_ceiling.contains("maximum Runway's upload endpoint accepts"),
            "{over_ceiling}"
        );

        let under_floor = video_transport_for_len(RUNWAY_MAX_VIDEO_DATA_URI_BYTES + 1, 100)
            .expect_err("under the floor")
            .to_string();
        assert!(under_floor.contains("fits neither transport"), "{under_floor}");

        let over_inline = check_video_data_uri_size(&RunwayVideoDataUri(
            "x".repeat(RUNWAY_MAX_VIDEO_DATA_URI_BYTES + 1),
        ))
        .expect_err("over the inline budget")
        .to_string();
        assert!(
            over_inline.contains("belongs on the upload transport"),
            "{over_inline}"
        );
    }

    /// Defect 2 of the same debug session: a DETERMINISTIC status must tell the
    /// agent not to retry, and a TRANSIENT one must not.
    ///
    /// Nothing in the code retried the owner's 413 — `dispatch_generate_with_cap`
    /// caps, it does not retry. The MODEL retried, because the surfaced error
    /// said nothing about whether a retry could help. So the fix is the message.
    #[test]
    fn deterministic_statuses_say_do_not_retry_and_transient_ones_do_not() {
        for status in [400u16, 401, 403, 404, 413, 422] {
            assert!(
                runway_status_is_terminal(status),
                "HTTP {status} cannot succeed on an identical retry"
            );
            let msg = non_success_to_gen_error(status, r#"{"message":"Request Entity Too Large"}"#)
                .to_string();
            assert!(
                msg.contains(RUNWAY_TERMINAL_STATUS_NOTICE),
                "HTTP {status} must carry the do-not-retry notice: {msg}"
            );
            // The vendor's own text is still surfaced VERBATIM and FIRST — the
            // notice is appended, never substituted (D-05's posture).
            assert!(
                msg.contains(r#"{"message":"Request Entity Too Large"}"#),
                "and must not hide or re-author what Runway said: {msg}"
            );
            assert!(msg.contains(&status.to_string()), "{msg}");
        }

        // Transient statuses are the opposite failure and a worse one: sweeping
        // them in would turn a recoverable rate-limit into a dead end.
        for status in [408u16, 425, 429, 500, 502, 503, 504] {
            assert!(
                !runway_status_is_terminal(status),
                "HTTP {status} is transient — a retry is exactly the right response"
            );
            let msg = non_success_to_gen_error(status, "{}").to_string();
            assert!(
                !msg.contains(RUNWAY_TERMINAL_STATUS_NOTICE),
                "HTTP {status} must NOT be marked terminal: {msg}"
            );
        }
    }

    /// The projected length must agree with the real encoder EXACTLY, because
    /// [`video_transport_for_len`] is fed the projection (a 200 MB clip must
    /// never be base64-encoded just to discover it is too big — that is a
    /// ~266 MB allocation on the path that already decided not to inline).
    #[test]
    fn projected_data_uri_len_agrees_with_the_real_encoder() {
        for n in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 63, 64, 65, 1000, 4096] {
            let bytes = vec![0xABu8; n];
            assert_eq!(
                projected_video_data_uri_len(n),
                video_data_uri(&bytes).len(),
                "the projection must equal the encoded length for n = {n}"
            );
        }
    }

    /// T-56-INJ-01: the step-1 body is TWO fixed literals and nothing else.
    ///
    /// A caller-derived filename would be caller text reaching a request field
    /// — a clip named `../../etc/passwd.mp4` or one carrying a prompt-injected
    /// string is exactly the surface `RUNWAY_REFERENCE_TAG` is a `&'static str`
    /// to avoid. Rudis refuses the whole idea rather than sanitising it.
    #[test]
    fn upload_request_body_is_the_fixed_two_field_shape() {
        let json = serde_json::to_string(&RunwayUploadRequest::EPHEMERAL_CLIP).unwrap();
        assert_eq!(
            json, r#"{"filename":"rudis-clip.mp4","type":"ephemeral"}"#,
            "exactly two keys, both fixed literals: {json}"
        );

        // The type-level half of the claim: neither field can hold caller text.
        let RunwayUploadRequest { filename, r#type } = RunwayUploadRequest::EPHEMERAL_CLIP;
        let _: &'static str = filename;
        let _: &'static str = r#type;
        assert_eq!(filename, "rudis-clip.mp4");
        assert_eq!(r#type, "ephemeral");
    }

    /// T-56-SSRF-02: the presigned target is server-issued, but it is still a
    /// URL from the network that Rudis is about to POST a user's own footage
    /// at. The scheme gate closes the `file://`-shaped hole before that
    /// happens, mirroring [`validate_output_uri`]'s posture on the equally
    /// server-issued download URL.
    #[test]
    fn upload_url_must_be_https() {
        assert!(validate_upload_url("https://uploads.runwayml.com/presigned/abc").is_ok());

        for hostile in [
            "http://uploads.runwayml.com/presigned/abc",
            "file:///C:/Windows/System32/config/SAM",
            "ftp://uploads.runwayml.com/x",
            "not a url at all",
            "https://user:pass@uploads.runwayml.com/x",
            "https://169.254.169.254/latest/meta-data/",
            "https://localhost/x",
        ] {
            let err = match validate_upload_url(hostile) {
                Ok(()) => panic!("{hostile} must be refused before any POST"),
                Err(e) => e,
            };
            assert!(matches!(err, GenError::Provider(_)), "{err}");
        }

        // And the gate really is reached from a parsed response body, not only
        // from a hand-written string.
        let bad: RunwayUploadsResponse = serde_json::from_str(
            r#"{"uploadUrl":"file:///etc/passwd","fields":{},"runwayUri":"runway://uploads/abc"}"#,
        )
        .expect("the body itself parses — the URL gate is a SEPARATE check");
        assert!(
            validate_upload_url(&bad.upload_url).is_err(),
            "a file:// uploadUrl is refused by the shape gate, not by deserialize"
        );
    }

    // -----------------------------------------------------------------------
    // Task 3 — the request body, the build arm, and the probe-derived consts.
    //
    // EVERY key literal below was copied from `artifacts/56-01-PROBE-RESULTS.md`
    // AFTER that artifact existed. None is a guess, and none is a name the
    // probe falsified — `references` in particular is absent from this suite on
    // purpose, because 56-01 proved it does not exist and pinning it would be
    // the exact defect the probe-first sequencing was designed to prevent
    // (T-56-SPEND-03).
    // -----------------------------------------------------------------------

    fn v2v_video() -> RunwayVideoInput {
        RunwayVideoInput::DataUri(video_data_uri(FAKE_MP4))
    }

    fn v2v_reference(bytes: Vec<u8>) -> ReferenceImage {
        ReferenceImage {
            bytes,
            width: 1280,
            height: 720,
        }
    }

    /// The body speaks ONLY probe-confirmed field names, and the exact set of
    /// them.
    ///
    /// | key | 56-01 verdict |
    /// | --- | --- |
    /// | `videoUri` | CONFIRMED twice, independently (constant 1) — REQUIRED |
    /// | `promptText` | CONFIRMED present, OPTIONAL (constant 5) |
    /// | `duration` | CONFIRMED **absent** (constant 8) |
    /// | `references` | CONFIRMED **absent** (constant 6) |
    /// | `ratio` | key CONFIRMED present (constant 9), **legal values NOT enumerated (F-2)** |
    /// | `seed` | CONFIRMED present, number-typed (constant 10) |
    #[test]
    fn video_to_video_request_serializes_the_probe_confirmed_field_names() {
        let body = RunwayVideoToVideoRequest {
            model: "aleph2".to_string(),
            video_uri: v2v_video(),
            prompt_text: "relight the subject as golden hour".to_string(),
        };
        let v = serde_json::to_value(&body).unwrap();
        let obj = v.as_object().unwrap();

        // Present, and spelled exactly as the endpoint named them.
        assert_eq!(obj["model"], "aleph2");
        assert!(
            obj["videoUri"]
                .as_str()
                .unwrap()
                .starts_with("data:video/mp4;base64,"),
            "videoUri is a BARE string, and 56-01 v2v-2 proved the data-URI \
             branch reaches the content stage: {obj:?}"
        );
        assert_eq!(obj["promptText"], "relight the subject as golden hour");

        // Absent — each for its own recorded reason.
        assert!(
            obj.get("duration").is_none(),
            "56-01 constant 8: `duration` is NOT a request field here — the \
             endpoint edits in place and bills against the input's own length"
        );
        assert!(
            obj.get("references").is_none(),
            "56-01 constant 6: the key is REJECTED with unrecognized_keys, \
             which FALSIFIES 56-RESEARCH Q4"
        );
        assert!(
            obj.get("ratio").is_none(),
            "the KEY exists (56-01 constant 9) but not one legal VALUE was \
             enumerated (F-2), and this endpoint edits an existing clip — a \
             guessed ratio would either 400 or silently reframe the user's own \
             footage. Omitted until F-2 runs; see the struct's doc comment."
        );
        assert!(obj.get("seed").is_none(), "Rudis has no seed concept to send");
        assert!(
            obj.get("url").is_none() && obj.get("uri").is_none(),
            "no url/uri-named field of any kind: {obj:?}"
        );

        assert_eq!(
            obj.len(),
            3,
            "exactly the three probe-confirmed keys this body needs: {obj:?}"
        );
    }

    /// TRIPWIRE (the twin of `image_to_video_request_carries_no_url_or_references_field`).
    ///
    /// Adding a field to [`RunwayVideoToVideoRequest`] fails to compile here and
    /// forces a deliberate review — which for THIS endpoint means asking
    /// whether the new key was probed or guessed.
    ///
    /// **`model` is a free-text `String` BY DESIGN (Phase 55.1 decision 1), and
    /// that is not a hole in the tripwire:** it selects WHICH model, never WHERE
    /// the request goes. The endpoint is a `&'static str` const and the base URL
    /// is a `&'static str` const; the video slot is a
    /// [`RunwayVideoInput`], whose both arms are private-inner
    /// one-constructor types. There is no `String`-typed field on this body that
    /// any URL is ever built from.
    #[test]
    fn video_to_video_request_carries_no_url_or_free_string_field() {
        let body = RunwayVideoToVideoRequest {
            model: "aleph2".to_string(),
            video_uri: v2v_video(),
            prompt_text: "p".to_string(),
        };
        let RunwayVideoToVideoRequest {
            model,
            video_uri,
            prompt_text,
        } = &body;
        assert_eq!(model, "aleph2");
        assert_eq!(prompt_text, "p");

        // The one non-`model` string on the wire is a Rudis-built data URI or a
        // server-issued handle — never free text.
        let rendered = serde_json::to_value(video_uri).unwrap();
        let s = rendered.as_str().expect("a bare string on the wire");
        assert!(
            s.starts_with(RUNWAY_VIDEO_DATA_URI_PREFIX) || s.starts_with("runway://"),
            "the video slot cannot hold a caller-chosen host: {s}"
        );

        // The model field reaches ONLY the `model` JSON key. Proven by naming a
        // model that LOOKS like a host and checking nothing else moved.
        let hostile = RunwayVideoToVideoRequest {
            model: "https://attacker.example/x".to_string(),
            video_uri: v2v_video(),
            prompt_text: "p".to_string(),
        };
        let v = serde_json::to_value(&hostile).unwrap();
        assert_eq!(v["model"], "https://attacker.example/x");
        assert_eq!(v.as_object().unwrap().len(), 3, "no extra key appeared");
        assert_eq!(
            RUNWAY_ENDPOINT_VIDEO_TO_VIDEO, "video_to_video",
            "the endpoint is a fixed literal the model cannot influence"
        );
        assert!(
            RUNWAY_BASE_URL.starts_with("https://api.dev.runwayml.com"),
            "and so is the host"
        );
    }

    /// 55.1 decision 1: **no roster validation and no capability gate.** An
    /// off-roster id builds fine; a capability mismatch is Runway's 400 over
    /// the network. Only a MALFORMED call (an empty/whitespace id) refuses
    /// locally, and that is not a roster judgment.
    #[test]
    fn build_video_edit_submission_accepts_any_free_text_model_id() {
        for id in ["brand-new-model-2027", "aleph2", "gen4_image"] {
            let s = build_video_edit_submission(id, "p".into(), v2v_video(), &[])
                .unwrap_or_else(|e| panic!("'{id}' must reach submission: {e}"));
            assert_eq!(
                s.endpoint, RUNWAY_ENDPOINT_VIDEO_TO_VIDEO,
                "the ENDPOINT follows the FRAMES the call carries, never the \
                 model id — '{id}' proves it, including a still-image id"
            );
            assert_eq!(s.suggested_ext, "mp4");
            match &s.body {
                RunwayRequestBody::VideoToVideo(b) => {
                    assert_eq!(b.model, id, "the id travels to the wire VERBATIM")
                }
                other => panic!("expected a video_to_video body, got {other:?}"),
            }
            assert_eq!(s.body.model(), id);
        }
        assert!(
            model_caps("brand-new-model-2027").is_none(),
            "the off-roster fixture is genuinely off-roster, so this is non-vacuous"
        );

        for malformed in ["", "   ", "\t\n"] {
            let err = build_video_edit_submission(malformed, "p".into(), v2v_video(), &[])
                .expect_err("an empty model id is a MALFORMED call, not a roster judgment");
            assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
        }
    }

    /// The reference cap is an ENDPOINT-contract bound on the request SHAPE,
    /// not a per-model capability gate, so it survives 55.1's no-caps rule.
    #[test]
    fn build_video_edit_submission_refuses_more_than_five_references() {
        let six: Vec<ReferenceImage> = (0..6).map(|i| v2v_reference(vec![i as u8; 4])).collect();
        assert!(six.len() > RUNWAY_V2V_MAX_REFERENCES);
        let err = build_video_edit_submission("aleph2", "p".into(), v2v_video(), &six)
            .expect_err("six references exceed the endpoint's documented ceiling");
        assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains(&RUNWAY_V2V_MAX_REFERENCES.to_string()) && msg.contains('6'),
            "the refusal names the CAP and the count: {msg}"
        );
    }

    /// **The probe-honesty refusal, and the whole reason this plan deviates
    /// from its own written struct.**
    ///
    /// 56-01 proved `references` does not exist. 56-F1 swept six documented
    /// spellings at $0.00 and returned **AMBIGUOUS**: four do not exist, two do
    /// (`keyframes`, `promptImage`, both array-typed), **neither is crowned**,
    /// and the per-item shape was never reached. So there is no field name to
    /// serialize a reference into that is not a guess.
    ///
    /// Rudis therefore REFUSES a reference rather than silently dropping it.
    /// Dropping is the failure mode this module was built to avoid —
    /// [`RUNWAY_REFERENCE_CITATION`]'s doc records 42.1-04 catching exactly
    /// that: an uncited reference came back HTTP 200 with a plausible,
    /// unconditioned result, undetectable by any caller.
    #[test]
    fn build_video_edit_submission_refuses_references_until_the_key_is_probed() {
        for n in 1..=RUNWAY_V2V_MAX_REFERENCES {
            let refs: Vec<ReferenceImage> = (0..n).map(|i| v2v_reference(vec![i as u8; 4])).collect();
            let err = build_video_edit_submission("aleph2", "p".into(), v2v_video(), &refs)
                .expect_err("a reference has no probe-confirmed key to ride");
            assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
            let msg = err.to_string();
            assert!(
                msg.contains("F-1b"),
                "the refusal names the probe that enumerated the candidates: {msg}"
            );
            // Phase 56 plan 06: F-1b RAN (2026-08-09) and did not crown a field,
            // so the refusal must point at what WOULD settle it now — the paid,
            // owner-gated run — rather than at a probe already spent. A message
            // naming a completed probe as the unblocker looks actionable and is
            // not; this is the assertion that keeps the citation current.
            assert!(
                msg.contains("56-09"),
                "and names the PAID run that is now the only remaining route — \
                 F-1b already ran and could not settle it: {msg}"
            );
            assert!(
                msg.contains("BEHAVIOUR"),
                "and says WHY no further free probe helps: a validation error \
                 proves a schema, and the open question is behavioural: {msg}"
            );
            assert!(
                msg.contains("keyframes") && msg.contains("promptImage"),
                "and names the two surviving candidates, so the refusal is \
                 actionable rather than a shrug: {msg}"
            );
        }
        // Zero references is the shipping path and must NOT refuse.
        assert!(build_video_edit_submission("aleph2", "p".into(), v2v_video(), &[]).is_ok());
    }

    /// The consts are sourced from the artifact, with their epistemic status in
    /// the doc — and the retired "10s" number appears nowhere.
    #[test]
    fn the_v2v_window_consts_carry_the_documented_numbers_not_the_retired_ten() {
        assert_eq!(RUNWAY_V2V_INPUT_MIN_SECONDS, 2);
        assert_eq!(RUNWAY_V2V_INPUT_MAX_SECONDS, 30);
        assert_eq!(RUNWAY_V2V_INPUT_MAX_FPS, 30);
        assert_eq!(RUNWAY_V2V_MAX_REFERENCES, 5);
        assert_ne!(
            RUNWAY_V2V_INPUT_MAX_SECONDS, 10,
            "56-RESEARCH § Q3 retired the inherited '<=10s' number outright"
        );
        assert!(
            RUNWAY_V2V_INPUT_MIN_SECONDS < RUNWAY_V2V_INPUT_MAX_SECONDS,
            "a window, and one with a MINIMUM the original decision never contemplated"
        );
        // The defensive combined-body bound is larger than either per-asset cap
        // and smaller than the upload ceiling — i.e. it bounds a body, not a
        // transport, which is what its doc claims.
        assert!(RUNWAY_MAX_REQUEST_BODY_BYTES > RUNWAY_MAX_VIDEO_DATA_URI_BYTES);
        assert!(RUNWAY_MAX_REQUEST_BODY_BYTES < RUNWAY_MAX_UPLOAD_BYTES);
    }

    /// **The endpoint-follows-the-frames pin, in both directions.**
    ///
    /// 1. [`build_submission`] — the no-source-video path — can NEVER produce
    ///    the `video_to_video` endpoint, for any modality/frame combination and
    ///    for any model id, INCLUDING `aleph2`, the one roster model whose only
    ///    real endpoint this is.
    /// 2. [`build_video_edit_submission`] — the source-video path — ALWAYS
    ///    produces it, even for a still-image model id.
    ///
    /// Together those are the structural form of 55.1's central consequence:
    /// what a call CARRIES decides where it goes; the model id decides nothing.
    /// And the golden body below is the regression proof that adding this arm
    /// changed no existing model's wire bytes.
    #[test]
    fn submit_dispatches_on_source_video() {
        // (1) The legacy path is byte-unchanged. This literal is the exact
        // body the tree produced before Phase 56 existed.
        let legacy = build_submission(
            "gen4_turbo",
            RequestModality::Video,
            "a red bicycle".into(),
            None,
            None,
        )
        .expect("the pre-existing text->video path");
        assert_eq!(
            serde_json::to_string(&legacy.body).unwrap(),
            r#"{"model":"gen4_turbo","promptText":"a red bicycle","ratio":"1280:720","duration":4}"#,
            "a request with no source video serializes BYTE-IDENTICALLY to the \
             pre-plan body"
        );

        // ...and no arm of it can reach the v2v endpoint, whatever the model.
        for model in ["gen4_turbo", "aleph2", "some-model-runway-ships-next-year"] {
            for (modality, first, last) in [
                (RequestModality::Video, None, None),
                (RequestModality::Video, Some(v2v_reference(vec![1])), None),
                (
                    RequestModality::Video,
                    Some(v2v_reference(vec![1])),
                    Some(v2v_reference(vec![2])),
                ),
                (RequestModality::Video, None, Some(v2v_reference(vec![2]))),
                (RequestModality::Image, None, None),
                (RequestModality::Image, Some(v2v_reference(vec![1])), None),
            ] {
                let s = build_submission(model, modality, "p".into(), first.as_ref(), last.as_ref())
                    .expect("every legacy row still builds");
                assert_ne!(
                    s.endpoint, RUNWAY_ENDPOINT_VIDEO_TO_VIDEO,
                    "no frameless path may reach video_to_video — not even for \
                     '{model}', whose only real endpoint it is"
                );
                assert!(
                    !matches!(s.body, RunwayRequestBody::VideoToVideo(_)),
                    "and no legacy path may build the v2v body"
                );
            }
        }

        // (2) The source-video path always goes there, and only there.
        let edit = build_video_edit_submission("gen4_image", "p".into(), v2v_video(), &[])
            .expect("source video present");
        assert_eq!(edit.endpoint, RUNWAY_ENDPOINT_VIDEO_TO_VIDEO);
    }

    /// The whole-body bound. **Since 2026-08-13 this number is MEASURED** — probe
    /// F-5 walked escalating bodies and found the API host's ceiling at 10 MiB
    /// (see [`RUNWAY_MAX_REQUEST_BODY_BYTES`]); it was an invented 48 MB before,
    /// and being 4.6x too loose is what let the owner's 4K clip edit reach the
    /// wire and earn a contentless HTTP 413.
    #[test]
    fn build_video_edit_submission_bounds_the_whole_serialized_body() {
        // A clip whose inline URI is legal per-asset cannot on its own breach the
        // body bound — the per-asset budget is DERIVED from it, minus the
        // scaffold reserve — so the guard is proven by the per-asset check firing
        // first, which is the ordering that matters.
        let oversized = RunwayVideoInput::DataUri(video_data_uri(&vec![0u8; 13 * 1024 * 1024]));
        let err = build_video_edit_submission("aleph2", "p".into(), oversized, &[])
            .expect_err("an over-cap inline clip is refused BEFORE egress");
        let msg = err.to_string();
        assert!(
            msg.contains("per-asset"),
            "the per-asset cap fires first and says so: {msg}"
        );
        assert!(
            !msg.contains("data:video/mp4;base64,"),
            "and never echoes the URI: {} chars",
            msg.len()
        );

        // The body bound itself is reachable and clean: an uploaded handle plus a
        // huge prompt is the only way to breach the body ceiling without breaching
        // a per-asset cap first.
        let handle: RunwayUploadedAsset =
            serde_json::from_str(r#""runway://uploads/abc""#).unwrap();
        let huge_prompt = "x".repeat(RUNWAY_MAX_REQUEST_BODY_BYTES + 1);
        let err = build_video_edit_submission(
            "aleph2",
            huge_prompt,
            RunwayVideoInput::Uploaded(handle),
            &[],
        )
        .expect_err("the whole serialized body is bounded too");
        assert!(matches!(err, GenError::InvalidRequest(_)), "{err}");
        assert!(
            err.to_string()
                .contains(&RUNWAY_MAX_REQUEST_BODY_BYTES.to_string()),
            "the refusal names the bound: {err}"
        );
    }
}
