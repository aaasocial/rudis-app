//! Phase 54.1 (plan 05, SC-2's paid tier) — **THIS BINARY SPENDS REAL MONEY.**
//!
//! `#[ignore]` + env-guard + its own binary = three independent locks. Run:
//! `cargo test -p ffi --test contract_generation_live -- --ignored --test-threads=1`.
//! Exactly once, at the phase gate, after the owner's explicit go-ahead. NEVER
//! `--include-ignored` on this binary from a script.
//!
//! # The FOURTH lock, and why it exists (plan 54.1-05, executor addition)
//!
//! The three locks above are the plan's. They are not sufficient on their own,
//! and the gap is worth stating plainly because it is a money bug:
//!
//! > `cargo test -p ffi -- --include-ignored` — the command plan 54.1-04's
//! > SUMMARY records as a green phase-gate sweep, and which a future plan or a
//! > CI step will run again — passes `--include-ignored` to **every** test
//! > binary in the package, including this one. `#[ignore]` does not stop it.
//! > Being in a separate binary does not stop it. On a keyed machine that sweep
//! > would bill the owner, silently, as a side effect of "running the tests".
//!
//! So this target is declared in `crates/ffi/Cargo.toml` as
//! `[[test]] name = "contract_generation_live" … test = false`: cargo does not
//! include it in `cargo test -p ffi` at all, while `--test
//! contract_generation_live` still selects it explicitly. The package-wide sweep
//! structurally cannot reach these two tests; only someone who NAMES this binary
//! and passes `--ignored` can. Do not remove that line without replacing the
//! property it provides.
//!
//! # What the guard does, and what it must never do
//!
//! Each test loads the untracked repo-root `.env` via `dotenvy` and then
//! resolves the SAME production credential slot production uses (the OS
//! Credential Manager entry, with the `*_API_KEY` env var as `connect`'s own
//! documented fallback). Resolution is a keychain/env READ — it opens no socket.
//! If nothing resolves, the test **panics loudly before any network attempt**,
//! and the panic names WHAT TO SET, never a value. No key is read into a local,
//! printed, asserted on, logged, or written to any artifact: it never leaves
//! `connect` except as an opaque field inside the returned provider (T-32-07 /
//! T-34-18).
//!
//! # Scope: the provider WIRE, not the spend gate
//!
//! The spend gate, the halt, the approval resume, the landing bridge and the
//! SC-6 disclosure are all proven FREE in `contract_generation.rs`, and again
//! live by the owner in the C# shell session. What only a real call can prove is
//! the half that the fixture provider cannot fake: that a real provider round
//! trip, through THIS host's `GenSubmission` impl, lands real bytes that
//! `ffprobe` accepts. That is deliberately all these two tests do.
//!
//! # Evidence that outlives the process (plan 54.1-05 Task 3, executor addition)
//!
//! This run happens ONCE. Everything it proves has to still be inspectable
//! afterwards, and as first written this file lost two thirds of that:
//!
//! * `InitConfig::default()` gives each `RudisCtx` a per-instance
//!   `tempfile::TempDir` for `app_data_dir()`. The paid bytes land inside it and
//!   are **deleted when the ctx drops** — so `assert!(path.exists())` was true
//!   only in-process, nothing survived for the owner to open in Task 4, and the
//!   plan's "keep the produced media files on disk" could not hold.
//! * libtest swallows a passing test's stdout, so the `println!` evidence lines
//!   existed only on failure.
//!
//! Both are fixed here, additively, WITHOUT touching the guard, the provider
//! slots, the prompts or the assertions: each test copies its landed asset into
//! the gitignored `target/live-gen-evidence/` before the temp dir is reclaimed,
//! and every evidence line is appended to `evidence.txt` there as well as
//! printed. The terminal `gen:job` ring records are recorded the same way —
//! *recorded*, deliberately not asserted, because a missing record is a finding
//! to write down, never a reason to turn a generation the owner already paid for
//! into a red run.

use app_core::{AppCtx, GenSubmission};
use rudis_ffi::{ctx::FfiAppCtx, ring, InitConfig, RudisCtx};

/// A ctx with PRODUCTION generation wiring — the three lazy provider slots and
/// the ONE vetted GEN-08 allow list that `RudisCtx::new_in_process` already
/// installs by default. `preset_generation` is NEVER called here: presetting is
/// what the fixture tier does, and this tier's entire purpose is the real slot.
///
/// The Anthropic key store is still an `InMemoryKeyStore`: no Chat turn runs in
/// this file (the turn loop is the fixture tier's subject), so nothing needs it,
/// and injecting the real one would put a second machine-global credential read
/// on a path that does not use it.
///
/// Phase 69 (D-69-23): `new_in_process` now defaults the PROVIDER stores to
/// in-memory doubles, so this tier — whose whole purpose is the real slot —
/// injects `ManagedProviderKeyStore::production()` explicitly via
/// `new_in_process_with`.
fn production_ctx() -> RudisCtx {
    RudisCtx::new_in_process_with(
        InitConfig::default(),
        Box::new(agent_llm::InMemoryKeyStore::new()),
        app_core::ManagedProviderKeyStore::production(),
    )
    .expect("in-process ctx builds")
}

/// Load the untracked repo-root `.env`, then fail LOUDLY — never a blind network
/// attempt — if the named modality's production slot resolves to nothing.
///
/// `resolved` is a BOOLEAN the caller computed from a `.resolve().is_some()`;
/// this function never sees, and could not print, key material. The panic text
/// names the keychain coordinates and the env var to set, and nothing else.
fn require_provider(resolved: bool, modality: &str, env_var: &str, account: &str, where_to_get: &str) {
    if resolved {
        return;
    }
    panic!(
        "no {modality} provider resolves — this live test needs a real key, and it is \
         #[ignore]-gated on purpose. Set ONE of:\n  \
         * the OS Credential Manager entry (service '{service}', account '{account}') — what \
           the shell's Settings writes, or\n  \
         * {env_var} in the untracked repo-root .env ({where_to_get}).\n\
         Refusing to attempt a blind network call.",
        service = app_core::KEYCHAIN_SERVICE,
    );
}

/// The landed asset for one returned id: the real `MediaBinItem` from the store,
/// asserted to exist on disk, be non-empty, and sit under this instance's own
/// `app_data_dir()/generated/` (CLAUDE.md rule 3 — never "the call returned Ok").
fn landed_item(
    ffi: &FfiAppCtx<'_>,
    asset: &app_core::GeneratedAsset,
) -> (rudis_core::MediaBinItem, u64) {
    assert_eq!(
        asset.media_item_ids.len(),
        1,
        "exactly one landed asset: {:?}",
        asset.media_item_ids
    );
    let id = &asset.media_item_ids[0];
    let snapshot = ffi.store().lock().expect("store").snapshot();
    let item = snapshot
        .media_bin
        .iter()
        .find(|i| &i.id == id)
        .unwrap_or_else(|| panic!("the returned id {id} is in the MediaBin"))
        .clone();

    let path = std::path::Path::new(&item.path);
    assert!(path.exists(), "the landed asset exists on disk: {}", item.path);
    let bytes = std::fs::metadata(path)
        .expect("the landed asset is stat-able")
        .len();
    assert!(bytes > 0, "the landed asset is non-empty ({bytes} bytes)");
    let generated_dir = ffi.app_data_dir().expect("data dir").join("generated");
    assert!(
        path.starts_with(&generated_dir),
        "confined to {}: {}",
        generated_dir.display(),
        item.path
    );
    (item, bytes)
}

/// The gitignored `target/live-gen-evidence/` directory: where this ONE run's
/// paid bytes and their evidence lines are preserved. Deliberately not the
/// tracked phase `artifacts/` dir — the run record is a document, the media is
/// working evidence for the owner's Task 4 inspection.
fn evidence_dir() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/live-gen-evidence");
    std::fs::create_dir_all(&dir).expect("the evidence dir is creatable");
    dir
}

/// Print an evidence line AND append it to `evidence.txt`, so it survives both
/// libtest's stdout capture and the end of the process.
///
/// Only paths, byte counts, probed measurements and `gen:job` payloads (job id /
/// provider / model / state / media ids — no request, no response body, no
/// header) ever reach this function. Nothing key-shaped, by construction.
fn record(line: String) {
    println!("{line}");
    let path = evidence_dir().join("evidence.txt");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("the evidence log opens");
    use std::io::Write;
    writeln!(f, "{line}").expect("the evidence log writes");
}

/// Copy the landed asset OUT of the per-instance temp `app_data_dir` before
/// `RudisCtx`'s `TempDir` guard reclaims it at process exit. Returns the durable
/// path and its byte length — re-`stat`ed at the destination, so the number is a
/// measurement of the file that still exists, not of the one that is about to
/// vanish.
fn preserve(tag: &str, item: &rudis_core::MediaBinItem) -> (std::path::PathBuf, u64) {
    let src = std::path::Path::new(&item.path);
    let name = src.file_name().expect("the landed path has a file name");
    let dest = evidence_dir().join(format!("{tag}-{}", name.to_string_lossy()));
    std::fs::copy(src, &dest).expect("the paid bytes are preserved");
    let bytes = std::fs::metadata(&dest)
        .expect("the preserved copy is stat-able")
        .len();
    (dest, bytes)
}

/// Every `gen:job` payload currently in the ring, in order — the harness twin of
/// `contract_generation.rs`'s helper. The terminal record is what the C# shell's
/// liveness channel delivers, and the media ids it names are the ones that
/// really landed (T-54.1-03).
fn gen_job_payloads(ctx: &RudisCtx) -> Vec<serde_json::Value> {
    ctx.ring()
        .poll(0)
        .events
        .into_iter()
        .filter(|r| r.event == ring::EVENT_GEN_JOB)
        .map(|r| r.payload)
        .collect()
}

// ---------------------------------------------------------------------------
// Sidecar pin — carried from `export_rendercache_isolation.rs:241-342`
// (commit 2dc630db) and MIRRORED rather than hoisted: integration tests are
// separate crates, and a shared test-support module would move a helper out of
// each file's own audit surface (the stance `export_parity.rs`,
// `export_proxy_isolation.rs` and `export_rendercache_isolation.rs` each state).
//
// Both live tests call this as their FIRST line, BEFORE `require_provider`, so
// the pin has taken before any gate, any spend and any probe. It is the one
// piece of this file that CANNOT be exercised here: running these tests bills
// the owner's own provider keys, and `require_provider` panics without them. Its
// placement is therefore proven by reading, and its MECHANISM by the RED control
// the sibling files ran (quick 260807-nul); it will first FIRE at the next keyed
// phase-gate run.
// ---------------------------------------------------------------------------

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

/// Where a bundled LGPL sidecar lives in this repo, in preference order — the
/// same two paths every other ffmpeg-dependent gate in the tree looks in
/// (`crates/engine/tests/proxy_encode.rs`, `crates/rendercache/tests/generate.rs`,
/// `crates/preview/tests/render_cache_invalidation.rs`, and this crate's own
/// `shutdown_proxy_cancel.rs` / `export_rendercache_isolation.rs`).
fn bundled_ffmpeg_candidates() -> [std::path::PathBuf; 2] {
    let root = workspace_root();
    [
        root.join("runtime/binaries"),
        root.join("crates/engine/ffmpeg-dev/bin"),
    ]
}

static SIDECAR_PINNED: std::sync::Once = std::sync::Once::new();

/// Point [`engine::locate`] at the repo's BUNDLED LGPL build, then PROVE it took.
///
/// # The measurement that put this here (2026-08-07)
///
/// `engine::locate()` resolves in three steps: `RUDIS_FFMPEG_DIR`, then a
/// sidecar beside the current exe, then **PATH**. A cargo test binary lives in
/// `target/debug/deps/`, where nothing stages an `ffmpeg.exe` — so with the env
/// var unset, step 3 wins and this file measured whatever ffmpeg the developer
/// happened to have installed. `locate`'s own doc says that "INVALIDATES any
/// license, codec or encoder-availability observation made against it", and this
/// file's whole point is an observation: the `engine::probe` that re-measures a
/// PAID, live-generated asset and cross-checks the MediaBinItem's dimensions
/// against it. That evidence is written to a run artifact and cited at a phase
/// gate — it has to name the instrument that produced it.
///
/// It was not a theoretical hazard. On the machine this was found on, PATH
/// resolved `ffmpeg 6.1-full_build-www.gyan.dev` (2023, libavcodec 60,
/// `--enable-gpl --enable-libx264 --enable-libx265`), whose `h264_nvenc`
/// segfaults in its post-encode session teardown — 5 crashes in 10 runs on a
/// synthetic 640x360 gradient against 0 in 20 for the bundled LGPL build, on
/// byte-identical argv and input. It cost `export_rendercache_isolation.rs`
/// three of its four tests before the pin landed there
/// (`.planning/debug/rendercache-nvenc-parity-crash.md`). A probe failure here
/// would be worse than a crash there: the money is already spent.
///
/// # Why this ASSERTS rather than skipping
///
/// A live run costs real money and happens once, at a gate. There is no second
/// chance to notice that the probe was taken against the wrong binary, so the
/// instrument must fail loudly BEFORE the spend rather than quietly after it —
/// which is exactly why the call sites put this above `require_provider`. Both
/// candidate directories are payloads this repo fetches, so absence is an
/// unprepared checkout, not a supported configuration.
///
/// A caller who has already chosen a `RUDIS_FFMPEG_DIR` keeps it (that is the
/// documented override), but it is still printed and still checked.
fn pin_the_sidecar_to_the_bundled_build() {
    SIDECAR_PINNED.call_once(|| {
        if std::env::var_os("RUDIS_FFMPEG_DIR").is_some() {
            return;
        }
        for cand in bundled_ffmpeg_candidates() {
            if cand.is_dir() {
                std::env::set_var("RUDIS_FFMPEG_DIR", &cand);
                return;
            }
        }
    });

    let chosen = std::env::var_os("RUDIS_FFMPEG_DIR").unwrap_or_else(|| {
        panic!(
            "no bundled LGPL ffmpeg in this checkout (looked in {:?}) and no RUDIS_FFMPEG_DIR \
             set. This file re-probes a PAID live asset and publishes the number; taken \
             against a PATH build that is an observation about someone's machine, not about \
             Rudis — and the spend has already happened",
            bundled_ffmpeg_candidates()
        )
    });
    let chosen = std::path::PathBuf::from(&chosen);
    let bins = engine::locate().expect("locate the pinned ffmpeg sidecar");
    assert_eq!(
        bins.ffmpeg.parent(),
        Some(chosen.as_path()),
        "engine::locate() resolved {} instead of a binary inside the pinned {}. A sidecar \
         from PATH is a DIFFERENT ffmpeg — the one measured on this machine (6.1, GPL) \
         crashes h264_nvenc outright — and CLAUDE.md rule 6 forbids scoring licence or \
         encoder behaviour against it",
        bins.ffmpeg.display(),
        chosen.display()
    );
    println!("GENLIVE-SIDECAR pinned={}", bins.ffmpeg.display());
}

/// GEN-01 live: one REAL Runway image generation through the C ABI host's
/// `GenSubmission::submit_image`, landing a real file this process can probe.
///
/// Approximate spend: one `gen4_image` call (~$0.05-0.08).
#[test]
#[ignore = "live paid call -- real spend against the owner's own provider keys; run explicitly ONCE at the phase gate, never in CI"]
fn gen01_live_runway_image_lands_through_the_ffi_host() {
    // The instrument BEFORE the gate and before the spend: this test's evidence
    // is an `engine::probe` measurement, and a probe taken from a PATH ffmpeg is
    // a fact about the developer's machine (CLAUDE.md rule 6).
    pin_the_sidecar_to_the_bundled_build();
    // --- Layered gate, ON TOP of #[ignore] and the `test = false` exclusion:
    //     load the untracked repo-root .env, then resolve the SAME production
    //     slot production uses. A keychain/env read; no socket is opened. ---
    let _ = dotenvy::dotenv();
    require_provider(
        app_core::ManagedGenProvider::production()
            .resolve_via(&app_core::ManagedProviderKeyStore::production())
            .is_some(),
        "Runway image",
        "RUNWAY_API_KEY",
        "gen-runway-api-key",
        "Runway Dashboard -> API keys",
    );

    let ctx = production_ctx();
    let ffi = FfiAppCtx::new(&ctx);
    assert!(
        ffi.store().lock().expect("store").snapshot().media_bin.is_empty(),
        "precondition: empty media bin"
    );

    // A deliberately cheap, machine-checkable prompt — flat pixels, one image.
    //
    // ⚠ **Rot repaired here, plan 56-09 (Rule 3 — blocking).** Phase 55.1 (D-10)
    // made `model` a REQUIRED parameter of `GenSubmission::submit_image`, and
    // this call site was never updated — because the `test = false` exclusion
    // that keeps a package sweep from BILLING this binary also keeps `cargo
    // test -p ffi` (and `cargo build --workspace`, and CI) from ever COMPILING
    // it. The file had not built since 55.1-03 and nothing said so. The literal
    // below is the same `gen4_image` `contract_generation.rs` pins as
    // `IMAGE_MODEL`, so both tiers drive the one model the allow list admits.
    //
    // Worth stating as a standing hazard rather than a one-off fix: the fourth
    // lock's cost is that this file rots silently. Whoever next touches a
    // `GenSubmission` signature must compile this target BY NAME
    // (`cargo test -p ffi --test contract_generation_live --no-run`) — it is
    // free, it opens no socket, and it is the only thing that will notice.
    let asset = ffi
        .block_on(ffi.submit_image(
            "a solid bright red circle perfectly centered on a plain white background, \
             flat minimal vector style, no text"
                .to_string(),
            "gen4_image".to_string(),
            agent_gen::BackgroundMode::Auto,
            None,
        ))
        .expect("the live Runway submit + landing succeeds");

    let (item, bytes) = landed_item(&ffi, &asset);
    assert!(
        item.width > 0 && item.height > 0,
        "REAL probed dimensions ({}x{})",
        item.width,
        item.height
    );
    let reprobe = engine::probe(std::path::Path::new(&item.path)).expect("the landed asset probes");
    assert_eq!(
        (item.width, item.height),
        (reprobe.width, reprobe.height),
        "the MediaBinItem's dims ARE the measurement of the landed file"
    );

    // Machine-written evidence for the run artifact. Paths, sizes and dims only
    // — no request, no response body, no header, nothing key-shaped.
    let (preserved, preserved_bytes) = preserve("gen01", &item);
    record(format!(
        "LIVE-GEN01 landed path={} bytes={bytes} dims={}x{} kind={:?} provenance_watermark={} \
         media_item_id={} preserved={} preserved_bytes={preserved_bytes}",
        item.path,
        item.width,
        item.height,
        item.media_kind,
        asset.carries_provenance_watermark,
        item.id,
        preserved.display()
    ));
    for (i, payload) in gen_job_payloads(&ctx).iter().enumerate() {
        record(format!("LIVE-GEN01 gen:job[{i}] {payload}"));
    }
}

/// GEN-03 live: one REAL ElevenLabs TTS generation through the C ABI host's
/// `GenSubmission::submit_audio`, landing a real audio file this process can
/// probe.
///
/// Approximate spend: 18 characters of speech (under $0.01).
#[test]
#[ignore = "live paid call -- real spend against the owner's own provider keys; run explicitly ONCE at the phase gate, never in CI"]
fn gen03_live_elevenlabs_audio_lands_through_the_ffi_host() {
    // Same order as GEN-01 above, for the same reason: instrument, then gate,
    // then spend.
    pin_the_sidecar_to_the_bundled_build();
    let _ = dotenvy::dotenv();
    require_provider(
        app_core::ManagedAudioGenProvider::production_audio()
            .resolve_via(&app_core::ManagedProviderKeyStore::production())
            .is_some(),
        "ElevenLabs audio",
        "ELEVENLABS_API_KEY",
        "gen-elevenlabs-api-key",
        "ElevenLabs profile -> API key",
    );

    let ctx = production_ctx();
    let ffi = FfiAppCtx::new(&ctx);
    assert!(
        ffi.store().lock().expect("store").snapshot().media_bin.is_empty(),
        "precondition: empty media bin"
    );

    // The cheapest possible live proof: 18 characters, seconds not minutes.
    // `prompt` IS the literal text to speak (T-34-11).
    let asset = ffi
        .block_on(ffi.submit_audio("Hello from Rudis.".to_string()))
        .expect("the live ElevenLabs submit + landing succeeds");

    let (item, bytes) = landed_item(&ffi, &asset);
    assert!(
        item.has_audio || item.duration_us > 0,
        "REAL probed audio ({} us, has_audio={})",
        item.duration_us,
        item.has_audio
    );
    let reprobe = engine::probe(std::path::Path::new(&item.path)).expect("the landed asset probes");
    assert!(
        reprobe.has_audio,
        "an independent re-probe of the landed file finds a real audio stream"
    );

    let (preserved, preserved_bytes) = preserve("gen03", &item);
    record(format!(
        "LIVE-GEN03 landed path={} bytes={bytes} duration_us={} has_audio={} \
         provenance_watermark={} media_item_id={} preserved={} \
         preserved_bytes={preserved_bytes}",
        item.path,
        item.duration_us,
        item.has_audio,
        asset.carries_provenance_watermark,
        item.id,
        preserved.display()
    ));
    for (i, payload) in gen_job_payloads(&ctx).iter().enumerate() {
        record(format!("LIVE-GEN03 gen:job[{i}] {payload}"));
    }
}

// ===========================================================================
// GEN-11 — Phase 56 plan 09: the ONE owner-approved paid clip edit.
//
// Three tests below, and only the LAST of them can spend anything:
//
//   1. `gen11_dry_the_extraction_fits_the_inline_transport`  -- $0.00, offline
//   2. `gen11_dry_the_whole_turn_reaches_the_seam_and_stops` -- $0.00, offline
//   3. `gen11_live_runway_video_edit_lands_through_the_ffi_host` -- **PAID**
//
// The two rehearsals exist because this run happens ONCE and every part of the
// path except the HTTP call itself can be proven for free first. 1 proves the
// D-02 extraction really produces inlinable bytes (a clip that overflowed to
// `/v1/uploads` would put the run on A3's unprobed transport). 2 drives the
// COMPLETE two-turn scripted flow -- gate halt, legitimate approval, dispatch,
// bridge, seam -- against a clip deliberately UNDER the D-01 window, so the
// path is exercised end to end and refuses locally before the provider slot is
// even resolved. Anything that would have broken the harness breaks there, at
// no cost.
//
// Test 3 carries a FIFTH lock on top of this binary's existing four: the env
// var `RUDIS_V2V_LIVE_ARMED` must hold the exact literal
// `arm-the-one-paid-run`. `#[ignore]` + `test = false` + `--ignored` +
// `require_provider` all guard against a sweep; this one guards against
// someone running THIS binary's ignored tier for the GEN-01/GEN-03 tests above
// and billing a clip edit as a side effect.
// ===========================================================================

/// The source clip's media. A REAL project fixture (T-56-FOOTAGE-08: project-
/// owned test media, never personal footage), 5 s of 1280x720@30 h264.
const V2V_FIXTURE: &str = "bars_720p30_5s.mp4";

/// The clip's TRIM. `in_us` is deliberately non-zero and `out_us` short of the
/// media's own 5 s end, so what leaves the machine is provably the clip's
/// VISIBLE range (D-02) and not the file: 3.0 s out of a 5 s source.
///
/// 3.0 s sits inside the 2-30 s window at both edges and prices at
/// `ceil(3) x 28 = 84` credits = **$0.84** -- inside the band the owner
/// approved, and above the 56-credit minimum so the figure is the per-second
/// rate's and not the floor's.
const V2V_CLIP_IN_US: i64 = 1_000_000;
const V2V_CLIP_OUT_US: i64 = 4_000_000;

/// The edit. A GLOBAL relight -- `aleph2`'s headline capability, and the one
/// whose effect is machine-measurable as a channel-mean shift rather than by
/// eye. Deliberately contains no person, no brand and no named work, so the
/// call has nothing for a moderation filter to catch.
const V2V_PROMPT: &str = "Relight this footage as a night scene lit only by deep blue moonlight: \
                          cool blue tones everywhere, heavy shadows, no warm colours.";

/// The clip id the scripted tool call names.
const V2V_CLIP_ID: &str = "c-live-edit";

/// The fifth lock's required value.
const V2V_ARM_VALUE: &str = "arm-the-one-paid-run";

fn v2v_fixture_path() -> std::path::PathBuf {
    workspace_root().join("test-media").join(V2V_FIXTURE)
}

/// A NONEXISTENT growable-library dir -- `load_library` on a missing dir is
/// documented empty, the same convention `contract_generation.rs` uses.
fn lib_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("rudis-ffi-contract-generation-live-no-library")
}

fn resp(content: Vec<agent_llm::ContentBlock>, stop_reason: &str) -> agent_llm::MessagesResponse {
    agent_llm::MessagesResponse {
        id: "msg_live".to_string(),
        role: agent_llm::Role::Assistant,
        content,
        stop_reason: Some(stop_reason.to_string()),
        usage: agent_llm::Usage::default(),
    }
}

/// One scripted `generate_ai_video_edit` tool call.
///
/// **`model` is deliberately ABSENT.** On this tool the field is OPTIONAL
/// (56-07 deviation 1) precisely because the capability has a real DERIVED,
/// PRICED default, and omitting it here is what makes that default's whole
/// chain -- `advisory_video_edit_model` -> the quoted price -> the disclosure
/// -> the submitted `model` id -- reachable and observable on a real call.
/// A retyped `"aleph2"` would prove none of it.
///
/// `references` is likewise absent: `video_edit_reference_check` refuses ANY
/// non-empty set today, so including one would be a $0.00 local refusal that
/// consumed the run.
fn v2v_edit_call() -> agent_llm::MessagesResponse {
    resp(
        vec![agent_llm::ContentBlock::ToolUse {
            id: "tu-v2v-live".to_string(),
            name: "generate_ai_video_edit".to_string(),
            input: serde_json::json!({
                "prompt": V2V_PROMPT,
                "clipId": V2V_CLIP_ID,
            }),
        }],
        "tool_use",
    )
}

/// Import the real fixture through the REAL import path and put ONE trimmed
/// clip on the timeline.
///
/// The import is genuine (`run_import_media_ui` -> ffprobe -> poster), so the
/// `MediaBinItem` the seam reads carries MEASURED dims and fps rather than a
/// hand-written literal -- which matters because `extract_clip_range_mp4` caps
/// its output cadence against that fps.
fn ctx_with_a_real_trimmed_clip(ctx: &RudisCtx, out_us: i64) -> rudis_core::MediaBinItem {
    let ffi = FfiAppCtx::new(ctx);
    let imported = ffi
        .block_on(app_core::run_import_media_ui(
            &ffi,
            vec![v2v_fixture_path().to_string_lossy().into_owned()],
        ))
        .expect("the real fixture imports");
    assert_eq!(imported.len(), 1, "one fixture imported");
    let item = imported[0].clone();

    let clip: rudis_core::Clip = serde_json::from_value(serde_json::json!({
        "id": V2V_CLIP_ID,
        "media_id": item.id.clone(),
        "start_us": 0,
        "in_us": V2V_CLIP_IN_US,
        "out_us": out_us,
    }))
    .expect("serde defaults fill the optional clip fields");
    ffi.store()
        .lock()
        .expect("store")
        .dispatch(rudis_core::Command::AddClip { track: 0, clip })
        .expect("the clip joins track v1");
    item
}

/// Every tool_result TEXT the model was shown, across the whole script — the
/// same reader `contract_generation.rs` uses to observe a refusal.
fn tool_result_texts(transport: &agent_llm::FixtureTransport) -> Vec<String> {
    transport
        .requests_seen()
        .iter()
        .flat_map(|req| req.messages.iter())
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            agent_llm::ContentBlock::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            agent_llm::ToolResultBlock::Text { text } => Some(text.clone()),
            agent_llm::ToolResultBlock::Image { .. } => None,
        })
        .collect()
}

/// Mean per-channel value of an RGBA buffer — the cheapest honest "did the
/// look change" number, and the one the run record publishes beside the frames.
fn channel_means(rgba: &[u8]) -> (f64, f64, f64) {
    let px = rgba.len() / 4;
    assert!(px > 0, "a frame has pixels");
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    for c in rgba.chunks_exact(4) {
        r += c[0] as u64;
        g += c[1] as u64;
        b += c[2] as u64;
    }
    (
        r as f64 / px as f64,
        g as f64 / px as f64,
        b as f64 / px as f64,
    )
}

/// Mean absolute difference per channel between equal-length RGBA buffers —
/// byte-identical to `app_core::test_support::mad`, so the MAD numbers this run
/// publishes are comparable to every other export MAD in the tree.
fn mad(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "buffers must be the same size to diff");
    let total: u64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i64 - *y as i64).unsigned_abs())
        .sum();
    total as f64 / a.len() as f64
}

// ---------------------------------------------------------------------------
// REHEARSAL 1 — $0.00, offline: the extraction fits the INLINE transport
// ---------------------------------------------------------------------------

/// **Free.** Run the exact D-02 extraction the paid call will run, measure its
/// bytes, and prove the shipped transport router chooses `DataUri`.
///
/// Why this is worth a test of its own rather than a note: over 16 MB of
/// projected data URI the router switches to `/v1/uploads`, which
/// 56-01-PROBE-RESULTS lists as **F-4, entirely unprobed** and on which A3's
/// SSRF argument still rests. Discovering that at the moment of a paid call
/// would put the one budgeted run on the untested transport. This decides it
/// beforehand, for nothing.
///
/// It also re-probes the extracted range, which is where the "the upload
/// carries no audio" property (`clip_range.rs` property 3) becomes a
/// measurement rather than a claim — and that measurement is exactly what
/// bounds what the A4 verdict can honestly say.
#[test]
#[ignore = "spawns the ffmpeg sidecar on real test media; zero-spend, no network -- run beside the live tier"]
fn gen11_dry_the_extraction_fits_the_inline_transport() {
    pin_the_sidecar_to_the_bundled_build();
    let probe = engine::probe(&v2v_fixture_path()).expect("the fixture probes");
    let bytes = app_core::extract_clip_range_mp4(
        &v2v_fixture_path(),
        V2V_CLIP_IN_US,
        V2V_CLIP_OUT_US,
        probe.avg_frame_rate,
    )
    .expect("the D-02 extraction succeeds");

    let projected = agent_gen::projected_video_data_uri_len(bytes.len());
    let transport = agent_gen::video_transport_for_len(projected, bytes.len())
        .expect("the extracted range fits a transport");

    // Re-probe the extracted bytes themselves: this is what actually leaves the
    // machine, so its duration / fps / audio inventory is the honest statement
    // of what the provider is given.
    let scratch = evidence_dir().join("gen11-source-range.mp4");
    std::fs::write(&scratch, &bytes).expect("preserve the extracted range");
    let ex = engine::probe(&scratch).expect("the extracted range probes");

    record(format!(
        "LIVE-GEN11-DRY1 extraction bytes={} projected_data_uri={} cap={} \
         transport={:?} extracted_duration_us={} extracted_fps={:.3} \
         extracted_dims={}x{} extracted_has_audio={} source_visible_us={} \
         preserved={}",
        bytes.len(),
        projected,
        agent_gen::RUNWAY_MAX_VIDEO_DATA_URI_BYTES,
        transport,
        ex.duration_us,
        ex.avg_frame_rate,
        ex.width,
        ex.height,
        ex.has_audio,
        V2V_CLIP_OUT_US - V2V_CLIP_IN_US,
        scratch.display()
    ));

    assert_eq!(
        transport,
        agent_gen::VideoTransport::DataUri,
        "the paid run must ride the PROBE-CONFIRMED inline data-URI transport, \
         not the unprobed /v1/uploads overflow (F-4)"
    );
    assert!(
        !ex.has_audio,
        "clip_range.rs property 3: what leaves the machine is audio-free by \
         construction -- and that is the limit on what the A4 verdict can claim"
    );
}

// ---------------------------------------------------------------------------
// REHEARSAL 2 — $0.00, offline: the whole turn reaches the seam and stops
// ---------------------------------------------------------------------------

/// **Free.** The COMPLETE two-turn scripted flow the paid run uses, against a
/// clip deliberately trimmed to 1.0 s — under the D-01 floor.
///
/// Every stage of the paid path runs: the spend gate halts turn 1 with a costed
/// question; the resume grants the one-shot through the EXISTING
/// `pending_ask_user` -> `spend_approved_turn` mechanism (no session surgery);
/// the model re-issues; the dispatch reaches `handle_generate_ai_video_edit`;
/// the bridge parses; `FfiAppCtx::submit_video_edit` passes the reference gate,
/// takes the store lock, resolves the clip — and then `clip_edit_window_check`
/// refuses, **before the extraction and before the provider slot is resolved**.
///
/// So a green run here means the only untested thing left is the HTTP call, and
/// a red one costs nothing to fix.
#[test]
#[ignore = "spawns the ffmpeg sidecar on real test media; zero-spend, no network -- the rehearsal for the paid run"]
fn gen11_dry_the_whole_turn_reaches_the_seam_and_stops() {
    pin_the_sidecar_to_the_bundled_build();
    let ctx = production_ctx();
    // 1.0 s visible: a real clip, under the 2 s floor.
    let _item = ctx_with_a_real_trimmed_clip(&ctx, V2V_CLIP_IN_US + 1_000_000);
    let ffi = FfiAppCtx::new(&ctx);
    let session = std::sync::Mutex::new(app_core::AgentSession::default());

    let transport = agent_llm::FixtureTransport::new(vec![
        v2v_edit_call(),
        v2v_edit_call(),
        resp(
            vec![agent_llm::ContentBlock::Text {
                text: "That clip is too short to edit.".to_string(),
            }],
            "end_turn",
        ),
    ]);

    let halted = ffi
        .block_on(app_core::run_agent_turn(
            &ffi,
            &session,
            &transport,
            "make this shot look like night".to_string(),
            vec![],
            &lib_dir(),
        ))
        .expect("the gate halts the CALL, never errors the TURN");
    let q = halted
        .clarifying_question
        .expect("turn 1 halted on the spend gate");
    record(format!("LIVE-GEN11-DRY2 spend_question={q}"));

    let outcome = ffi
        .block_on(app_core::run_agent_turn(
            &ffi,
            &session,
            &transport,
            "yes".to_string(),
            vec![],
            &lib_dir(),
        ))
        .expect("the resumed turn completes even though the call is refused");

    let results = tool_result_texts(&transport);
    let refusal = results
        .iter()
        .find(|t| t.contains("generate_ai_video_edit failed"))
        .unwrap_or_else(|| panic!("the seam refused and said so; saw {results:?}"));
    record(format!("LIVE-GEN11-DRY2 seam_refusal={refusal}"));

    let expected = app_core::clip_edit_window_check(1_000_000)
        .expect_err("1.0s is under the floor")
        .to_string();
    assert!(
        refusal.contains(&expected),
        "the refusal must be `clip_edit_window_check`'s OWN message -- proof the \
         turn reached the seam rather than failing earlier:\n  got: {refusal}\n  \
         want substring: {expected}"
    );
    assert!(
        outcome.generation_disclosures.is_empty(),
        "a refused call generates nothing, so it discloses nothing"
    );
    assert!(
        ffi.store()
            .lock()
            .expect("store")
            .snapshot()
            .media_bin
            .iter()
            .all(|m| !m.path.contains("generated")),
        "nothing was landed"
    );
    assert!(
        session.lock().expect("session").pending_ask_user.is_none(),
        "the halt was consumed by the legitimate resume"
    );
    assert_eq!(
        transport.requests_seen().len(),
        3,
        "turn 1's send + turn 2's two rounds -- exactly ONE dispatch was possible"
    );
}

// ---------------------------------------------------------------------------
// THE ONE PAID RUN
// ---------------------------------------------------------------------------

/// **GEN-11 live — THIS TEST SPENDS THE OWNER'S MONEY, ONCE.**
///
/// ```text
/// RUDIS_V2V_LIVE_ARMED=arm-the-one-paid-run \
///   cargo test -p ffi --test contract_generation_live \
///   gen11_live_runway_video_edit_lands_through_the_ffi_host \
///   -- --ignored --exact --nocapture --test-threads=1
/// ```
///
/// Approximate spend: 3.0 s of input at 28 credits/s = 84 credits = **$0.84**
/// (`RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND`, verified at plan 05 against the
/// vendor's own pricing page). The run reconciles that estimate against what
/// the account is actually billed, and a mismatch corrects the constant.
///
/// # Exactly one submit, structurally
///
/// The script holds three responses: the call that halts, the re-issue after
/// approval, and a closing narration. A fourth `transport.send()` would err, so
/// the model cannot retry — `GENERATE_RETRY_CAP` allows a second paid dispatch
/// per turn and the SCRIPT is what forbids one here. If the call fails, the
/// closing response is consumed and the test reports the failure. **There is no
/// retry, by the 32-03 rule and by construction.**
///
/// # What only this run can settle, and what it deliberately cannot
///
/// * **Output duration vs input** (56-RESEARCH Q3, recorded UNKNOWN): both
///   numbers are probed and printed.
/// * **Picture-only output** (A4, D-07's premise): `engine::probe` on the landed
///   file. ⚠ Bounded honestly — Rudis's extraction is audio-free by
///   construction, so a silent output settles *what lands in the bin* and NOT
///   whether `aleph2` would pass audio through if it were given some.
/// * **SC-1's export verification**: the landed asset is placed on the timeline,
///   the project is exported through the real export path, and a frame decoded
///   from inside the placed range of the EXPORTED file is MAD-compared against
///   the same-timestamp frame of the LANDED file (match) and against the
///   ORIGINAL source frame (control). The GEN-01/GEN-02 proof shape.
/// * **NOT settled: the reference field.** `video_edit_reference_check` refuses
///   any non-empty set, so a reference cannot ride the shipped path; sending one
///   would have spent the run on a $0.00 local refusal. F-1 stays open.
#[test]
#[ignore = "live paid call -- real spend against the owner's own provider keys; run explicitly ONCE at the phase gate, never in CI"]
fn gen11_live_runway_video_edit_lands_through_the_ffi_host() {
    // --- Lock 5: the arming literal, checked before anything else. ---
    let armed = std::env::var("RUDIS_V2V_LIVE_ARMED").unwrap_or_default();
    assert_eq!(
        armed, V2V_ARM_VALUE,
        "this test BILLS the owner for a clip edit. Set \
         RUDIS_V2V_LIVE_ARMED={V2V_ARM_VALUE} to arm it. Refusing to spend \
         because someone ran this binary's ignored tier."
    );

    // The instrument, then the gate, then the spend — the order the two live
    // tests above established.
    pin_the_sidecar_to_the_bundled_build();
    let _ = dotenvy::dotenv();
    require_provider(
        app_core::ManagedVideoGenProvider::production_video()
            .resolve_via(&app_core::ManagedProviderKeyStore::production())
            .is_some(),
        "Runway video",
        "RUNWAY_API_KEY",
        "gen-runway-api-key",
        "Runway Dashboard -> API keys",
    );

    let started_at = std::time::Instant::now();
    let ctx = production_ctx();
    let source_item = ctx_with_a_real_trimmed_clip(&ctx, V2V_CLIP_OUT_US);
    let ffi = FfiAppCtx::new(&ctx);
    let session = std::sync::Mutex::new(app_core::AgentSession::default());

    let visible_us = V2V_CLIP_OUT_US - V2V_CLIP_IN_US;
    let advisory = agent_gen::advisory_video_edit_model()
        .expect("the roster carries a video_to_video row");
    let estimate_cents = app_core::estimated_video_edit_cost_cents(advisory, visible_us)
        .expect("the advisory model is priceable");
    record(format!(
        "LIVE-GEN11 plan clip={V2V_CLIP_ID} media={} source_dims={}x{} \
         source_fps={:.3} source_duration_us={} clip_in_us={V2V_CLIP_IN_US} \
         clip_out_us={V2V_CLIP_OUT_US} visible_us={visible_us} \
         advisory_model={advisory} estimate_cents={estimate_cents} \
         rate_cents_per_input_second={} minimum_cents={}",
        source_item.id,
        source_item.width,
        source_item.height,
        source_item.fps,
        source_item.duration_us,
        app_core::RUNWAY_ALEPH2_CENTS_PER_INPUT_SECOND,
        app_core::RUNWAY_ALEPH2_MINIMUM_CENTS,
    ));

    // --- The script: halt, re-issue, close. Exactly one dispatch is possible. -
    let transport = agent_llm::FixtureTransport::new(vec![
        v2v_edit_call(),
        v2v_edit_call(),
        resp(
            vec![agent_llm::ContentBlock::Text {
                text: "Your clip has been relit as a night scene.".to_string(),
            }],
            "end_turn",
        ),
    ]);

    // --- Turn 1: the spend gate halts. NOTHING has been submitted. ---
    let halted = ffi
        .block_on(app_core::run_agent_turn(
            &ffi,
            &session,
            &transport,
            "make this shot look like night".to_string(),
            vec![],
            &lib_dir(),
        ))
        .expect("the gate halts the CALL, never errors the TURN");
    let question = halted
        .clarifying_question
        .expect("turn 1 halted on the spend gate");
    record(format!("LIVE-GEN11 spend_question={question}"));
    assert!(
        ffi.store()
            .lock()
            .expect("store")
            .snapshot()
            .media_bin
            .len()
            == 1,
        "turn 1 landed nothing (only the imported source is in the bin)"
    );

    let can_undo_before = ffi.store().lock().expect("store").can_undo();
    let seq_before = ffi.store().lock().expect("store").seq();

    // --- Turn 2: the owner's approval resumes the halt. THE SUBMIT HAPPENS. ---
    let outcome = ffi
        .block_on(app_core::run_agent_turn(
            &ffi,
            &session,
            &transport,
            "yes".to_string(),
            vec![],
            &lib_dir(),
        ))
        .expect("the resumed turn completes");
    let wall_s = started_at.elapsed().as_secs_f64();

    // Everything the model was shown, recorded BEFORE any assertion can abort:
    // a failed run must still leave a full record (the plan's step 3).
    for (i, t) in tool_result_texts(&transport).iter().enumerate() {
        record(format!("LIVE-GEN11 tool_result[{i}] {t}"));
    }
    for (i, payload) in gen_job_payloads(&ctx).iter().enumerate() {
        record(format!("LIVE-GEN11 gen:job[{i}] {payload}"));
    }
    for (i, d) in outcome.generation_disclosures.iter().enumerate() {
        record(format!(
            "LIVE-GEN11 disclosure[{i}] modality={} model_resolved={:?} \
             frames={:?} provider_notice_present={}",
            d.modality,
            d.model_resolved,
            d.frames,
            d.provider_notice.is_some()
        ));
    }
    record(format!(
        "LIVE-GEN11 wall_seconds={wall_s:.1} requests_seen={} \
         media_bin_len={}",
        transport.requests_seen().len(),
        ffi.store().lock().expect("store").snapshot().media_bin.len()
    ));

    // --- Did it land? -------------------------------------------------------
    let snapshot = ffi.store().lock().expect("store").snapshot();
    let landed = snapshot
        .media_bin
        .iter()
        .find(|m| m.id != source_item.id)
        .unwrap_or_else(|| {
            panic!(
                "THE PAID RUN DID NOT LAND AN ASSET. Everything above is the \
                 record; do NOT retry (32-03). media_bin={:?}",
                snapshot.media_bin.iter().map(|m| &m.id).collect::<Vec<_>>()
            )
        })
        .clone();

    let landed_path = std::path::PathBuf::from(&landed.path);
    assert!(landed_path.exists(), "the landed asset exists: {}", landed.path);
    let landed_bytes = std::fs::metadata(&landed_path).expect("stat").len();
    assert!(landed_bytes > 0, "the landed asset is non-empty");
    let generated_dir = ffi.app_data_dir().expect("data dir").join("generated");
    assert!(
        landed_path.starts_with(&generated_dir),
        "confined to {}: {}",
        generated_dir.display(),
        landed.path
    );

    // --- THE TWO FACTS ONLY MONEY COULD BUY --------------------------------
    let out = engine::probe(&landed_path).expect("the landed asset probes");
    let (preserved, preserved_bytes) = preserve("gen11", &landed);
    record(format!(
        "LIVE-GEN11 landed media_item_id={} path={} bytes={landed_bytes} \
         preserved={} preserved_bytes={preserved_bytes} \
         provenance_watermark_flag=see_disclosure",
        landed.id,
        landed.path,
        preserved.display()
    ));
    record(format!(
        "LIVE-GEN11 Q3-DURATION input_visible_us={visible_us} \
         output_duration_us={} delta_us={} ratio={:.4} \
         (bin_item_duration_us={})",
        out.duration_us,
        out.duration_us - visible_us,
        out.duration_us as f64 / visible_us as f64,
        landed.duration_us
    ));
    record(format!(
        "LIVE-GEN11 A4-AUDIO output_has_audio={} output_acodec={:?} \
         bin_item_has_audio={} (BOUND: the uploaded range was audio-free by \
         construction, so this settles what LANDS, not what aleph2 would do \
         with an audio-bearing input)",
        out.has_audio, out.acodec, landed.has_audio
    ));
    record(format!(
        "LIVE-GEN11 OUTPUT-SHAPE dims={}x{} fps={:.3} vcodec={:?} \
         input_dims={}x{} input_fps={:.3}",
        out.width,
        out.height,
        out.avg_frame_rate,
        out.vcodec,
        source_item.width,
        source_item.height,
        source_item.fps
    ));

    // --- Did the PROMPT take? A channel-mean shift, input vs output. --------
    let mid_out_us = out.duration_us / 2;
    let mid_src_us = V2V_CLIP_IN_US + visible_us / 2;
    let out_frame = engine::decode_frame_rgba_at_scaled(
        &landed_path,
        mid_out_us,
        out.rotation_degrees,
        out.width,
        out.height,
    )
    .expect("decode the landed output mid-frame");
    let src_frame = engine::decode_frame_rgba_at_scaled(
        &v2v_fixture_path(),
        mid_src_us,
        0,
        out.width,
        out.height,
    )
    .expect("decode the source mid-frame at the output's geometry");
    let (or, og, ob) = channel_means(&out_frame.rgba);
    let (sr, sg, sb) = channel_means(&src_frame.rgba);
    let d_out_vs_src = mad(&out_frame.rgba, &src_frame.rgba);
    record(format!(
        "LIVE-GEN11 PROMPT-EFFECT source_mean_rgb=({sr:.1},{sg:.1},{sb:.1}) \
         output_mean_rgb=({or:.1},{og:.1},{ob:.1}) \
         delta_rgb=({:.1},{:.1},{:.1}) MAD_output_vs_source={d_out_vs_src:.4} \
         (sampled source@{mid_src_us}us vs output@{mid_out_us}us)",
        or - sr,
        og - sg,
        ob - sb
    ));

    // --- The undoable-landing evidence, BEFORE the export ------------------
    // Recorded here rather than at the end on purpose: the generation is already
    // paid for, and no later step is allowed to cost the record.
    let (can_undo_after, seq_after) = {
        let store = ffi.store().lock().expect("store");
        (store.can_undo(), store.seq())
    };
    record(format!(
        "LIVE-GEN11 UNDO bin_item_id={} can_undo_before_submit={can_undo_before} \
         can_undo_after={can_undo_after} store_seq_before={seq_before} \
         store_seq_after={seq_after}",
        landed.id
    ));

    // --- SC-1: EXPORT VERIFICATION (the GEN-01/GEN-02 proof shape) ----------
    // Clear the source clip, place the LANDED asset at t=0, export through the
    // real export path, and read the exported file's pixels back.
    //
    // Every step here is FALLIBLE-BUT-RECORDED rather than `expect`ed: a panic
    // between the spend and the record would destroy facts the owner has
    // already paid for. Failures become `None` + a recorded line, and the
    // verdict is asserted at the very end.
    let sc1: Option<(f64, f64)> = (|| {
        {
            let mut store = ffi.store().lock().expect("store");
            if let Err(e) = store.dispatch(rudis_core::Command::RemoveClip {
                id: V2V_CLIP_ID.to_string(),
            }) {
                record(format!("LIVE-GEN11 SC1-EXPORT FAILED at remove_clip: {e}"));
                return None;
            }
        }
        let placed = match app_core::run_place_clip(&ffi, landed.id.clone(), 0, 0) {
            Ok(c) => c,
            Err(e) => {
                record(format!("LIVE-GEN11 SC1-EXPORT FAILED at place_clip: {e}"));
                return None;
            }
        };
        record(format!(
            "LIVE-GEN11 SC1-PLACED clip={} range_us=[{},{}) start_us={}",
            placed.id, placed.in_us, placed.out_us, placed.start_us
        ));

        let export_path = evidence_dir().join("gen11-export.mp4");
        let exported = match ffi.block_on(app_core::run_export(
            &ffi,
            export_path.clone(),
            Some(out.width),
            Some(out.height),
            Some(if out.avg_frame_rate > 0.0 {
                out.avg_frame_rate
            } else {
                source_item.fps
            }),
        )) {
            Ok(p) => p,
            Err(e) => {
                record(format!("LIVE-GEN11 SC1-EXPORT FAILED at run_export: {e}"));
                return None;
            }
        };

        let exported_probe = match engine::probe(&export_path) {
            Ok(p) => p,
            Err(e) => {
                record(format!("LIVE-GEN11 SC1-EXPORT FAILED at probe: {e}"));
                return None;
            }
        };
        let sample_us = (placed.out_us - placed.in_us) / 2;
        let exported_frame = match engine::decode_frame_rgba_at(&export_path, sample_us, 0) {
            Ok(f) => f,
            Err(e) => {
                record(format!("LIVE-GEN11 SC1-EXPORT FAILED decoding the export: {e}"));
                return None;
            }
        };
        let landed_frame = match engine::decode_frame_rgba_at_scaled(
            &landed_path,
            sample_us,
            out.rotation_degrees,
            exported_frame.width,
            exported_frame.height,
        ) {
            Ok(f) => f,
            Err(e) => {
                record(format!("LIVE-GEN11 SC1-EXPORT FAILED decoding the landed asset: {e}"));
                return None;
            }
        };
        let control_frame = match engine::decode_frame_rgba_at_scaled(
            &v2v_fixture_path(),
            V2V_CLIP_IN_US + sample_us,
            0,
            exported_frame.width,
            exported_frame.height,
        ) {
            Ok(f) => f,
            Err(e) => {
                record(format!("LIVE-GEN11 SC1-EXPORT FAILED decoding the original: {e}"));
                return None;
            }
        };

        let mad_match = mad(&exported_frame.rgba, &landed_frame.rgba);
        let mad_control = mad(&exported_frame.rgba, &control_frame.rgba);
        record(format!(
            "LIVE-GEN11 SC1-EXPORT exported={exported} exported_dims={}x{} \
             exported_duration_us={} sample_us={sample_us} \
             MAD_export_vs_landed={mad_match:.4} (match threshold <= 12.0) \
             MAD_export_vs_original={mad_control:.4} (control threshold > 40.0)",
            exported_probe.width, exported_probe.height, exported_probe.duration_us
        ));
        Some((mad_match, mad_control))
    })();

    // --- Assertions LAST, so the record is complete either way. -------------
    assert_eq!(
        transport.requests_seen().len(),
        3,
        "EXACTLY ONE dispatch was possible: turn 1's send + turn 2's two rounds"
    );
    assert!(
        out.duration_us > 0 && out.width > 0 && out.height > 0,
        "the landed asset is real decodable video ({}x{}, {}us)",
        out.width,
        out.height,
        out.duration_us
    );
    let (mad_match, mad_control) = sc1.expect(
        "SC-1's export verification must have RUN; the recorded \
         `SC1-EXPORT FAILED at ...` line above says where it stopped",
    );
    assert!(
        mad_match <= 12.0,
        "SC-1: the EXPORTED file's pixels must BE the landed edited asset's \
         (MAD {mad_match:.4} > 12.0) -- a 200 proves nothing, this does"
    );
    assert!(
        mad_control > 40.0,
        "SC-1's control must discriminate: the exported frame must clearly \
         differ from the ORIGINAL source frame (MAD {mad_control:.4} <= 40.0). \
         If this is the only red, record it: it means the edit was too subtle \
         to separate, not that the export failed"
    );
    assert!(
        can_undo_after,
        "the landing is UNDOABLE -- it went through the store's command bracket"
    );
    assert_eq!(
        outcome.generation_disclosures.len(),
        1,
        "exactly one successful generate_ai_* dispatch is disclosed"
    );
}

// ===========================================================================
// GEN-12 — Phase 55.1 plan 07: open model selection, proven against the vendor.
//
// Two tests, and only the FIRST of them can spend anything:
//
//   1. `gen12_live_off_roster_model_reaches_runway_through_the_agent_dispatch_path`
//                                                                    -- **PAID**
//   2. `gen12_live_invalid_model_id_surfaces_runways_own_400`         -- $0.00
//
// # Why a live test at all, when plans 01-06 were entirely hermetic
//
// Phase 55.1 retired the local closed roster: `build_submission` stopped
// consulting `RUNWAY_MODELS` (55.1-01), the allow list stopped consulting the
// intent table and gained `UNGATED_PROVIDERS = ["runway"]` (55.1-02), both
// tools took a REQUIRED free-text `model` field (55.1-03), and 55.1-06 deleted
// the twelve identifiers of the indirection outright, so the Rudis half of
// GEN-12 is provable by ABSENCE: `rg INTENT_MODELS crates/` returns nothing,
// and there is no code left that could intercept a model id.
//
// **Absence of a local refusal is not presence of a remote acceptance.** The
// half no fixture can fake is Runway's: that a model id THIS BINARY HAS NEVER
// HEARD OF is accepted by the vendor's own server-side enum and produces real
// bytes. That is a claim about `api.dev.runwayml.com`, and it is settled once,
// under sign-off, or not at all.
//
// # The division of labour between the two, which is also the cost split
//
// Test 2 is the D-05 half and it is **free by construction**: a genuinely
// unknown id fails Runway's request VALIDATION, before any generation is
// scheduled, so nothing is billed. What it proves is the DIRECTION 55.1 chose
// — the vendor's own `{error, docUrl, issues}` body reaches the caller
// verbatim, rather than a Rudis-authored sentence about a roster Rudis no
// longer keeps. Both retired sentences are asserted ABSENT by their exact
// historical text (see the two consts below).
//
// Test 1 is the expensive half and the only one that can succeed: a real,
// off-roster, Gen-4-family id driven through the FULL dispatch path.
//
// # Non-vacuity: both tests check that their model really IS off-roster
//
// A proof about an "off-roster" id is worthless if the id quietly joins the
// roster. Both tests therefore assert `app_core::cost_signal_for_model(id) ==
// app_core::PRICE_UNKNOWN` BEFORE spending anything — the roster's own answer
// to "do I know this model?". Adding a `gen4` row to `RUNWAY_MODELS` reddens
// this immediately instead of silently turning the proof into a tautology.
// ===========================================================================

/// The off-roster-but-REAL video model this phase's central claim is proven on.
///
/// `gen4` is in **Runway's** live `model` enum and absent from **Rudis's**
/// `RUNWAY_MODELS` — exactly the gap GEN-12 exists to close. 55.1-RESEARCH § F
/// enumerated the live enum on `POST /v1/image_to_video` (probed 2026-07-27) at
/// **20** ids against the roster's ~12: `gen3a_turbo, gen4_turbo, gen4, gen4.5,
/// kling2.5_turbo_pro, kling3.0_pro, kling3.0_4k, kling3.0_standard,
/// klingO3_pro, klingO3_standard, klingO3_4k, veo3, veo3.1, veo3.1_fast,
/// robotics_v1, seedance2, seedance2_fast, seedance2_mini, happyhorse_1_0,
/// gemini_omni_flash`. `gen4` is the cheapest-plausible of those (its
/// roster-listed siblings `gen4_turbo` and `gen4.5` price at 5 and 12 credits/s
/// respectively) while still being a first-class Runway model rather than an
/// exotic third-party tier.
///
/// A LITERAL, deliberately — not a new const in `agent-gen`. 55.1-06's closing
/// note to this plan is explicit: *"Plan 07 should NOT re-add a model constant
/// of any kind for its fixture; use a literal at the test site"*, which is the
/// pattern that plan established when it converted the two former
/// `TRANSITION_FALLBACK_MODEL` call sites. A const in the shipped crate would
/// be a roster in miniature, in the phase that deleted the roster.
///
/// **If this 400s for a NON-gate reason** (a server-side retirement), that is a
/// roster fact to REPORT, not to code around: read the recorded error, pick a
/// current off-roster id out of Runway's own enumerated rejection, edit this
/// literal, re-run, and write the drift into the SUMMARY. `seedance2_fast` and
/// `kling2.5_turbo_pro` are the plan's named alternates.
///
/// # ⚠ MEASURED 2026-08-21, and the paragraph above is only half right
///
/// The one owner-approved run fired against this literal and came back
/// **HTTP 403 in 2.6 s** — `{"error":"Model variant gen4 is not available",
/// "docUrl":"…"}` — with **nothing generated and $0.00 billed**. The literal is
/// deliberately LEFT AS `gen4` rather than swapped for an alternate, because
/// the run is capped at one paid submit and the id here must keep naming what
/// was actually run.
///
/// What that 403 exposed is a distinction no earlier probe could see, and it
/// contradicts the assumption above that a live-enum id is a runnable id:
///
/// | Layer | Question it answers | `gen4`'s answer |
/// | --- | --- | --- |
/// | request validation (`400`) | is this id in the endpoint's schema enum? | **YES** |
/// | account entitlement (`403`) | may THIS key run that id? | **NO** |
///
/// Both were measured the same day. `gen12_live_image_to_video_enum_is_enumerated_by_runways_own_400`
/// listed `gen4` second in a 21-id `image_to_video` enum minutes before the
/// paid call was refused for it. **Runway's validation enum is a superset of
/// what an account may actually run**, so enumerating the enum — the technique
/// the plan's how-to-verify step 4 relies on, and the one this file now
/// implements — cannot by itself predict whether a call will be entitled.
///
/// A future run that wants sub-claim (a) must therefore pick its id on
/// ENTITLEMENT evidence, not enum membership: the cheapest source is a model
/// this account has already been billed for. Do NOT simply retry another enum
/// entry and hope (32-03).
const GEN12_OFF_ROSTER_MODEL: &str = "gen4";

/// A model id no vendor will ever ship — the $0.00 case.
///
/// Prefixed `rudis-` so that if it somehow DID resolve, the fact would be
/// unmistakable rather than plausible.
const GEN12_INVALID_MODEL: &str = "rudis-definitely-not-a-model";

/// The RETIRED Rudis-authored refusal that `gen_submit_error_message` produces
/// for `GenError::ModelNotAllowed` — still live code, and correctly so: it is
/// what an **ElevenLabs** id outside the one signed-off row still gets.
///
/// Spelled here as documentation of what the assertions look for. The
/// assertions themselves repeat the literal AT EACH SITE rather than reading
/// this const, and that is deliberate: these are strings that must never
/// reappear on the Runway path, so a test that tracked a const would silently
/// follow a rename instead of catching it. (`crates/app-core/src/generation_host.rs`,
/// `gen_submit_error_message`.)
const RETIRED_RUNWAY_ALLOW_LIST_REFUSAL: &str = "not on the clean-model allow-list";

/// The RETIRED roster refusal, quoted from the commit that deleted it.
///
/// `build_submission` raised `"'{model_id}' is not a known Runway model — see
/// RUNWAY_MODELS"` from 42.1-01 (`da7622d1`) until 55.1-01 (`bd47be5f`) removed
/// it. Unlike the allow-list sentence above it has **no** remaining definition
/// anywhere — `rg "is not a known Runway model" crates/` is empty — so an
/// occurrence in a live error would mean a local roster gate had been rebuilt.
const RETIRED_RUNWAY_ROSTER_REFUSAL: &str = "'<id>' is not a known Runway model — see RUNWAY_MODELS";

/// **The FIFTH lock on the PAID gen12 test — an executor addition (plan 07,
/// deviation Rule 2), not something the plan asked for.**
///
/// The plan specifies three locks (`#[ignore]` + `require_provider` + the
/// `test = false` binary exclusion) and this file already carries a fourth (the
/// manifest exclusion is what makes lock 3 structural). None of the four
/// separates the tests INSIDE this binary from each other, and this module's
/// own doc — line 4 — tells a reader to run
/// `cargo test -p ffi --test contract_generation_live -- --ignored
/// --test-threads=1`, which after this plan would bill a Gen-4-class video
/// generation as a side effect of confirming GEN-01/GEN-03. That is precisely
/// the hazard 56-09 named when it added `RUDIS_V2V_LIVE_ARMED` for the clip
/// edit; the argument transfers verbatim, so the mechanism does too.
///
/// A DIFFERENT env var and a DIFFERENT literal from the v2v one on purpose:
/// arming one paid run must never arm the other.
///
/// The $0.00 invalid-id test deliberately does NOT carry this lock — it cannot
/// spend, and making it harder to run would discourage the one check in this
/// pair that is free.
const GEN12_ARM_ENV: &str = "RUDIS_GEN12_LIVE_ARMED";
const GEN12_ARM_VALUE: &str = "arm-the-off-roster-run";

/// The prompt for the paid run: a plain camera move over a test pattern.
///
/// No person, no brand, no named work and no style-of-a-living-artist, so there
/// is nothing for a moderation filter to catch — the one budgeted run must not
/// be consumed by a content refusal. Deliberately undemanding: this test scores
/// *"did an off-roster model produce real decodable bytes"*, never *"is the
/// result good"*, which is a judgement no assertion should pretend to make.
const GEN12_PROMPT: &str = "A slow, steady push-in on a colour test pattern. Locked-off tripod \
                            framing, even lighting, no text and no people.";

/// A REAL PNG conditioning frame, produced the way production produces one.
///
/// `GenSubmission::resolve_reference_image` yields PNG bytes from
/// `engine::encode_png_bytes` over a decoded frame, so this builds its
/// reference through the SAME two functions rather than hand-rolling a byte
/// vector: a synthetic `vec![1, 2, 3]` (what the offline builder tests use, and
/// correctly — they assert the SHAPE of the request) would reach Runway as a
/// malformed image and buy a 400 about the PNG instead of an answer about the
/// model.
///
/// The source is `bars_720p30_5s.mp4`, a project-owned fixture
/// (T-56-FOOTAGE-08: never personal footage), and it decodes at **1280x720** —
/// which is [`agent_gen::RUNWAY_RATIO`]'s pinned `1280:720`, the one ratio
/// legal on every model and endpoint Rudis has probed. A mismatched reference
/// aspect is another way to buy a 400 that says nothing about the roster.
///
/// Requires the sidecar, so every caller pins it first.
fn gen12_reference_frame() -> agent_gen::ReferenceImage {
    let frame = engine::decode_frame_rgba_at(&v2v_fixture_path(), 1_000_000, 0)
        .expect("the project-owned fixture decodes a conditioning frame");
    let bytes = engine::encode_png_bytes(&frame).expect("the conditioning frame PNG-encodes");
    agent_gen::ReferenceImage {
        bytes,
        width: frame.width,
        height: frame.height,
    }
}

/// Assert the id under test is genuinely absent from the ADVISORY roster, and
/// record the roster's own answer.
///
/// This is the non-vacuity guard for both tests, and it is free. It also
/// records a real GEN-12 property on the way past: an off-roster id prices as
/// [`app_core::PRICE_UNKNOWN`], which is the literal the spend confirmation
/// shows the user instead of inventing a figure (55.1-04).
fn gen12_assert_off_roster(tag: &str, model: &str) {
    let signal = app_core::cost_signal_for_model(model);
    record(format!(
        "LIVE-GEN12 {tag} model={model} roster_cost_signal={signal:?} \
         (off-roster <=> {:?})",
        app_core::PRICE_UNKNOWN
    ));
    assert_eq!(
        signal,
        app_core::PRICE_UNKNOWN,
        "{tag}: '{model}' must be OFF the advisory roster for this proof to mean \
         anything -- RUNWAY_MODELS now prices it, so this test is measuring an \
         on-roster model and proving nothing about open selection. Pick another \
         id from Runway's live enum (55.1-RESEARCH F) rather than deleting this \
         assertion"
    );
}

/// **GEN-12 live — THIS TEST SPENDS THE OWNER'S MONEY, ONCE.**
///
/// ```text
/// RUDIS_GEN12_LIVE_ARMED=arm-the-off-roster-run \
///   cargo test -p ffi --test contract_generation_live \
///   gen12_live_off_roster_model_reaches_runway_through_the_agent_dispatch_path \
///   -- --ignored --exact --nocapture --test-threads=1
/// ```
///
/// # What only this run can settle
///
/// That a model id **absent from every table in this repository** is accepted
/// by Runway and lands real, probe-able bytes through Rudis's own governed
/// submit path. Everything upstream of the socket is already green offline;
/// everything downstream of it belongs to the vendor. The seam between them is
/// the only thing here that money buys.
///
/// Two sub-claims, and they fail differently on purpose:
///
/// * **(a) it reaches the wire and lands** — a `MediaBinItem` under this
///   instance's own `generated/` dir that `engine::probe` accepts as real
///   video. Asserted through the shared [`landed_item`], so this run is scored
///   by exactly the ruler GEN-01's was.
/// * **(b) no LOCAL gate fired** — on ANY error, the three retired
///   Rudis-authored refusals are asserted absent BEFORE the error is re-raised.
///   That distinction is the whole point: *"Runway said no"* is a roster fact
///   worth writing down, while *"Rudis said no"* would mean this phase did not
///   ship.
///
/// # The dispatch path is the REAL one
///
/// `FfiAppCtx::submit_video` — the trait method the C# shell's agent turn
/// reaches — not a hand-built `GenRequest`. A `GenRequest` assembled here would
/// skip `generate_runway_video_for_agent`'s prompt caps, its modality-scoped
/// provider resolution, `start_generation_job`'s `submit_checked` gate and the
/// landing bridge, i.e. every layer that could still have refused. Passing
/// `Some(reference), None` selects `image_to_video` **by the frames supplied**
/// (55.1-01's total function over `(modality, frames)`), never by the model id
/// — which is itself part of what is being proven.
#[test]
#[ignore = "live paid call -- real spend against the owner's own provider keys; run explicitly ONCE at the phase gate, never in CI"]
fn gen12_live_off_roster_model_reaches_runway_through_the_agent_dispatch_path() {
    // --- Lock 5: the arming literal, checked before anything else. ---
    let armed = std::env::var(GEN12_ARM_ENV).unwrap_or_default();
    assert_eq!(
        armed, GEN12_ARM_VALUE,
        "this test BILLS the owner for a video generation. Set \
         {GEN12_ARM_ENV}={GEN12_ARM_VALUE} to arm it. Refusing to spend because \
         someone ran this binary's ignored tier for the other live tests."
    );

    // The instrument, then the gate, then the spend -- the order every live
    // test in this file establishes. Needed twice over here: the conditioning
    // frame is DECODED by the sidecar, and the landed asset is re-probed by it.
    pin_the_sidecar_to_the_bundled_build();
    let _ = dotenvy::dotenv();
    require_provider(
        app_core::ManagedVideoGenProvider::production_video()
            .resolve_via(&app_core::ManagedProviderKeyStore::production())
            .is_some(),
        "Runway video",
        "RUNWAY_API_KEY",
        "gen-runway-api-key",
        "Runway Dashboard -> API keys",
    );

    // Free, and it must hold or the rest is theatre.
    gen12_assert_off_roster("off_roster", GEN12_OFF_ROSTER_MODEL);

    let started_at = std::time::Instant::now();
    let reference = gen12_reference_frame();
    let ctx = production_ctx();
    let ffi = FfiAppCtx::new(&ctx);
    assert!(
        ffi.store().lock().expect("store").snapshot().media_bin.is_empty(),
        "precondition: empty media bin"
    );

    // Recorded BEFORE the spend, so a crash mid-call still leaves a record of
    // what was attempted. Sizes and dims only -- never the bytes, never a key.
    record(format!(
        "LIVE-GEN12 plan model={GEN12_OFF_ROSTER_MODEL} endpoint=image_to_video \
         (frames decide, not the model) reference_bytes={} reference_dims={}x{} \
         pinned_ratio={} prompt_chars={}",
        reference.bytes.len(),
        reference.width,
        reference.height,
        agent_gen::RUNWAY_RATIO,
        GEN12_PROMPT.chars().count()
    ));

    // --- THE SUBMIT. The real trait method, the real dispatch path. ---
    let result = ffi.block_on(ffi.submit_video(
        GEN12_PROMPT.to_string(),
        GEN12_OFF_ROSTER_MODEL.to_string(),
        Some(reference),
        None,
    ));
    let wall_s = started_at.elapsed().as_secs_f64();

    // Everything the host observed, recorded BEFORE any assertion can abort --
    // a failed run must still leave a complete record (the 56-09 discipline).
    for (i, payload) in gen_job_payloads(&ctx).iter().enumerate() {
        record(format!("LIVE-GEN12 gen:job[{i}] {payload}"));
    }
    record(format!(
        "LIVE-GEN12 wall_seconds={wall_s:.1} outcome={} media_bin_len={}",
        if result.is_ok() { "ok" } else { "err" },
        ffi.store().lock().expect("store").snapshot().media_bin.len()
    ));

    let asset = match result {
        Ok(asset) => asset,
        Err(err) => {
            record(format!(
                "LIVE-GEN12 off_roster FAILED model={GEN12_OFF_ROSTER_MODEL} error={err}"
            ));

            // ---- Sub-claim (b), asserted on the error path, where it is the
            //      ONLY thing that can still be proven. Each literal is spelled
            //      HERE rather than read from its const: these are strings that
            //      must never come back, so tracking a const would follow a
            //      rename instead of catching one. ----
            assert!(
                !err.contains("not on the clean-model allow-list"),
                "THE LOCAL ALLOW-LIST GATE FIRED. Phase 55.1 D-01 put 'runway' in \
                 UNGATED_PROVIDERS so no Runway model id can be refused locally; \
                 this error is `gen_submit_error_message`'s ModelNotAllowed \
                 wording, which means it was. GEN-12 is not shipped.\n  error: {err}"
            );
            assert!(
                !err.contains("clean-model allow list"),
                "THE LOCAL ALLOW-LIST GATE FIRED, surfaced through \
                 `GenError::ModelNotAllowed`'s own Display (the un-mapped, \
                 space-spelled twin of the sentence above) rather than through \
                 `gen_submit_error_message`. Same defect, different surface.\n  \
                 error: {err}"
            );
            assert!(
                !err.contains("is not a known Runway model"),
                "A LOCAL ROSTER GATE HAS BEEN REBUILT. 55.1-01 (bd47be5f) deleted \
                 build_submission's {RETIRED_RUNWAY_ROSTER_REFUSAL:?} refusal and \
                 nothing in crates/ defines that sentence any more, so seeing it \
                 means a roster lookup was re-added upstream of the wire.\n  \
                 error: {err}"
            );

            // Not a gate, then. Surface it VERBATIM: a moved or retired `gen4`
            // is a roster fact for the owner to read and the SUMMARY to record,
            // never something to silently retry against another id (32-03: no
            // retries, and the money for this attempt is already gone).
            panic!(
                "the off-roster run reached Runway and Runway refused it. No Rudis \
                 gate fired -- sub-claim (b) HOLDS and is the finding -- but \
                 sub-claim (a) is unproven. Read the error, and if it names a \
                 model enum, pick a current off-roster id out of it, edit \
                 GEN12_OFF_ROSTER_MODEL, and record the drift. Do NOT re-run \
                 without a fresh go-ahead.\n  model: {GEN12_OFF_ROSTER_MODEL}\n  \
                 error: {err}"
            );
        }
    };

    // ---- Sub-claim (a): real bytes, measured by the shared ruler. ----
    let (item, bytes) = landed_item(&ffi, &asset);
    let out = engine::probe(std::path::Path::new(&item.path)).expect("the landed asset probes");
    let (preserved, preserved_bytes) = preserve("gen12", &item);
    record(format!(
        "LIVE-GEN12 landed model={GEN12_OFF_ROSTER_MODEL} path={} bytes={bytes} \
         probed_dims={}x{} probed_duration_us={} probed_fps={:.3} vcodec={:?} \
         has_audio={} kind={:?} provenance_watermark={} media_item_id={} \
         preserved={} preserved_bytes={preserved_bytes}",
        item.path,
        out.width,
        out.height,
        out.duration_us,
        out.avg_frame_rate,
        out.vcodec,
        out.has_audio,
        item.media_kind,
        asset.carries_provenance_watermark,
        item.id,
        preserved.display()
    ));
    assert!(
        out.duration_us > 0 && out.width > 0 && out.height > 0,
        "an OFF-ROSTER model produced real decodable video ({}x{}, {}us)",
        out.width,
        out.height,
        out.duration_us
    );
    assert_eq!(
        (item.width, item.height),
        (out.width, out.height),
        "the MediaBinItem's dims ARE the measurement of the landed file"
    );
}

/// **GEN-12 live, $0.00 — a deliberately-invalid id surfaces RUNWAY'S own 400.**
///
/// ```text
/// cargo test -p ffi --test contract_generation_live \
///   gen12_live_invalid_model_id_surfaces_runways_own_400 \
///   -- --ignored --exact --nocapture --test-threads=1
/// ```
///
/// # Why this costs nothing, and why it still needs the funded key
///
/// Runway validates the `model` field against its own server-side enum during
/// REQUEST parsing — before any generation is scheduled — so an unknown id is
/// rejected with a `400` and nothing is billed. It still needs a resolvable
/// credential, because an unauthenticated call would be refused at a
/// completely different layer and would prove nothing about the model enum.
/// Hence: no arming lock (it cannot spend), but the same `require_provider`
/// gate as its paid sibling.
///
/// # What it proves: D-05, the direction of the refusal
///
/// Phase 55.1's bargain was to give up local capability refusals in exchange
/// for never lying about a roster this binary cannot keep current. The
/// obligation that came with it is that the vendor's rejection is **surfaced,
/// not re-authored** — `non_success_to_gen_error` wraps the status and passes
/// the body through, which is safe precisely because it never receives the API
/// key (T-31-01).
///
/// So the assertion has two halves, and both are needed:
///
/// * **positive** — Runway's own `{error, docUrl, issues}` vocabulary is
///   visible in the string the caller gets;
/// * **negative** — neither retired Rudis-authored sentence is.
///
/// Either alone is satisfiable by an accident: a wrapper that swallowed the
/// body would pass the negative, and a local gate that happened to quote the
/// word "issues" would pass the positive.
///
/// No frames are supplied, so this is `text_to_video` — the endpoint follows
/// the frames (55.1-01), and a frameless call is the cheapest shape to be
/// refused in.
#[test]
#[ignore = "live call -- $0.00 (server-side validation rejection) but requires the owner's real key; run explicitly at the phase gate, never in CI"]
fn gen12_live_invalid_model_id_surfaces_runways_own_400() {
    // No arming lock: this test cannot spend. The instrument is still pinned
    // first -- `probe`/`locate` are not reached on this path, but the pin
    // asserts the checkout is the prepared one before a network call is made.
    pin_the_sidecar_to_the_bundled_build();
    let _ = dotenvy::dotenv();
    require_provider(
        app_core::ManagedVideoGenProvider::production_video()
            .resolve_via(&app_core::ManagedProviderKeyStore::production())
            .is_some(),
        "Runway video",
        "RUNWAY_API_KEY",
        "gen-runway-api-key",
        "Runway Dashboard -> API keys",
    );

    gen12_assert_off_roster("invalid_id", GEN12_INVALID_MODEL);

    let ctx = production_ctx();
    let ffi = FfiAppCtx::new(&ctx);

    // Frameless => text_to_video, decided by the ABSENCE of frames.
    let err = ffi
        .block_on(ffi.submit_video(
            "a short establishing shot of an empty room".to_string(),
            GEN12_INVALID_MODEL.to_string(),
            None,
            None,
        ))
        .err()
        .unwrap_or_else(|| {
            panic!(
                "an id no vendor ships was ACCEPTED. Either '{GEN12_INVALID_MODEL}' \
                 resolved to something real (and the owner has just been billed for \
                 it -- check the account), or the submit path is not reaching the \
                 network at all"
            )
        });
    record(format!(
        "LIVE-GEN12 invalid_id model={GEN12_INVALID_MODEL} error={err}"
    ));

    for (i, payload) in gen_job_payloads(&ctx).iter().enumerate() {
        record(format!("LIVE-GEN12 invalid_id gen:job[{i}] {payload}"));
    }

    // --- POSITIVE: the vendor's own 400 vocabulary reached the caller. ---
    //
    // Four needles rather than one: `issues` and `docUrl` are the body KEYS
    // 55.1-RESEARCH names, while the two enum phrasings are what the `issues[]`
    // entry itself carries. Any one is enough -- requiring all four would pin
    // Runway's error prose, which is theirs to change.
    let vendor_needles = ["issues", "docUrl", "invalid_enum_value", "Invalid enum value"];
    let matched: Vec<&str> = vendor_needles
        .iter()
        .copied()
        .filter(|n| err.contains(n))
        .collect();
    record(format!(
        "LIVE-GEN12 invalid_id vendor_needles_matched={matched:?} of {vendor_needles:?}"
    ));
    assert!(
        !matched.is_empty(),
        "the refusal must be RUNWAY'S, surfaced as-is (D-05). None of \
         {vendor_needles:?} appears, so either the body was swallowed on the way \
         out or the call never reached Runway's validator.\n  error: {err}"
    );

    // --- NEGATIVE: neither retired Rudis-authored sentence came back. ---
    //
    // Spelled at the site, not read from the consts, for the reason given on
    // `RETIRED_RUNWAY_ALLOW_LIST_REFUSAL`: a const-tracking assertion follows a
    // rename instead of catching one.
    assert!(
        !err.contains("not on the clean-model allow-list"),
        "the refusal is RUDIS-AUTHORED ({RETIRED_RUNWAY_ALLOW_LIST_REFUSAL:?}), not \
         Runway's. D-05 says the vendor's error is surfaced, never re-authored, and \
         'runway' is in UNGATED_PROVIDERS precisely so this sentence is unreachable \
         on this path -- it survives only for ElevenLabs.\n  error: {err}"
    );
    assert!(
        !err.contains("clean-model allow list"),
        "the refusal is RUDIS-AUTHORED, via `GenError::ModelNotAllowed`'s own \
         Display rather than `gen_submit_error_message`'s wording.\n  error: {err}"
    );
    assert!(
        !err.contains("is not a known Runway model"),
        "a LOCAL ROSTER GATE has been rebuilt: {RETIRED_RUNWAY_ROSTER_REFUSAL:?} was \
         deleted at 55.1-01 (bd47be5f) and has no definition left in crates/.\n  \
         error: {err}"
    );

    // Nothing was generated, so nothing landed and nothing is billable.
    assert!(
        ffi.store().lock().expect("store").snapshot().media_bin.is_empty(),
        "a validation rejection lands no asset"
    );
}

/// **GEN-12 live, $0.00 — read Runway's CURRENT `image_to_video` model enum out
/// of its own rejection, before spending anything.**
///
/// ```text
/// cargo test -p ffi --test contract_generation_live \
///   gen12_live_image_to_video_enum_is_enumerated_by_runways_own_400 \
///   -- --ignored --exact --nocapture --test-threads=1
/// ```
///
/// # Why this exists (executor addition, plan 07 — deviation Rule 3)
///
/// The plan's how-to-verify step 4 tells a human that if the off-roster id
/// errors for a non-gate reason they should *"read the recorded error, pick a
/// current off-roster id from Runway's own error/enum, edit the test's id
/// literal, and re-run"*. It assumed that enum would be learned from the PAID
/// test's failure. It does not have to be: a validation rejection enumerates
/// the enum at **$0.00**, so the enum can be read BEFORE the one budgeted run
/// rather than by consuming it.
///
/// That stopped being hypothetical the moment its sibling ran.
/// `gen12_live_invalid_model_id_surfaces_runways_own_400` came back on
/// 2026-08-21 with a `text_to_video` enum that no longer contains `gen4`,
/// `gen4_turbo`, `gen3a_turbo`, `veo3` or `robotics_v1`, and DOES contain three
/// ids no Rudis probe has ever seen (`seedance2_5`, `hailuo3`,
/// `grok_imagine_1_5`). Most of those absences are explainable as endpoint
/// asymmetry — the Gen-4 base family is image-to-video — but `veo3` is
/// `text_to_video: true` in `RUNWAY_MODELS`, so at least some of that drift is
/// genuine retirement. Firing the paid run blind against a 3.5-week-old
/// `image_to_video` enumeration would have been a coin flip.
///
/// # Why it is FREE, and why it is not a second paid attempt
///
/// Identical construction to its sibling: an unknown `model` fails Runway's
/// request validation before any generation is scheduled. The only difference
/// is that a conditioning frame is supplied, which routes the call to
/// `image_to_video` (55.1-01: the endpoint follows the FRAMES) and therefore
/// makes Runway enumerate THAT endpoint's enum instead of `text_to_video`'s.
///
/// It asserts only the vendor-shape property, and deliberately does NOT assert
/// that any particular id is present: which models Runway offers is Runway's
/// business and a test that pinned the list would be red every time they ship.
/// The list is RECORDED — that is the whole output.
#[test]
#[ignore = "live call -- $0.00 (server-side validation rejection) but requires the owner's real key; run explicitly at the phase gate, never in CI"]
fn gen12_live_image_to_video_enum_is_enumerated_by_runways_own_400() {
    pin_the_sidecar_to_the_bundled_build();
    let _ = dotenvy::dotenv();
    require_provider(
        app_core::ManagedVideoGenProvider::production_video()
            .resolve_via(&app_core::ManagedProviderKeyStore::production())
            .is_some(),
        "Runway video",
        "RUNWAY_API_KEY",
        "gen-runway-api-key",
        "Runway Dashboard -> API keys",
    );

    let ctx = production_ctx();
    let ffi = FfiAppCtx::new(&ctx);

    // A frame IS supplied, so this routes to image_to_video -- the endpoint the
    // paid run uses, and therefore the enum the paid run is judged against.
    let err = ffi
        .block_on(ffi.submit_video(
            "a short establishing shot of an empty room".to_string(),
            GEN12_INVALID_MODEL.to_string(),
            Some(gen12_reference_frame()),
            None,
        ))
        .err()
        .unwrap_or_else(|| {
            panic!("an id no vendor ships was ACCEPTED on image_to_video -- check the account")
        });
    record(format!("LIVE-GEN12 i2v_enum_probe error={err}"));

    let vendor_needles = ["issues", "docUrl", "invalid_value", "Invalid option"];
    let matched: Vec<&str> = vendor_needles
        .iter()
        .copied()
        .filter(|n| err.contains(n))
        .collect();
    record(format!(
        "LIVE-GEN12 i2v_enum_probe vendor_needles_matched={matched:?}"
    ));
    assert!(
        !matched.is_empty(),
        "the enumeration must be RUNWAY'S own rejection body.\n  error: {err}"
    );
    assert!(
        ffi.store().lock().expect("store").snapshot().media_bin.is_empty(),
        "a validation rejection lands no asset"
    );
}
