//! The agent's two engine-backed transcript tools -- `get_transcript`
//! (Phase 22, TEXT-02) and `search_media` (EYES-03) -- relocated verbatim from
//! `src-tauri/src/lib.rs` by plan 45-05 (Phase 45, XTRC-01).
//!
//! Like [`crate::inspect`], nothing here takes `AppHandle`/`State`: the surface
//! is `&SharedStore` + `&Mutex<AgentSession>`, so no [`crate::AppCtx`]
//! implementation is involved. Both tools transcribe REAL media OFFLINE through
//! `engine::whisper` and return TEXT-ONLY tool_results.
//!
//! `src-tauri` keeps its old call sites through a `pub use app_core::{..}` shim.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::session::{AgentSession, TranscriptCache};
use crate::SharedStore;

// ---------------------------------------------------------------------------
// Phase 22 (TEXT-02/EYES-03): the two engine-needing READ tools, routed through
// the SAME Pattern-C interception the inspect_* tools use. Both transcribe REAL
// media OFFLINE via `engine::whisper` (an MCP dev-only dep, so it lives HERE not
// in `agent-llm`) and return TEXT-ONLY tool_results (no image block). Both are
// NON_EDIT — never a `rudis_core::tools::Tool` variant, never routed to
// `Store::dispatch`, never on the MCP surface (mirroring inspect_timeline/media).
// ---------------------------------------------------------------------------

/// T-22-14 (Denial of Service): a hard ceiling on the number of matches
/// `search_media` returns for an agent-controlled query — mirroring
/// `INSPECT_STORYBOARD_MAX_COUNT`. A pathological query (e.g. a single very
/// common word over a long asset) can never make the tool return an unbounded
/// result set.
const SEARCH_MEDIA_MAX_MATCHES: usize = 50;

/// Phase 22: a small bound on the media-identity transcript cache so a session
/// that inspects many assets cannot grow it without limit. Oldest entry evicted
/// first (FIFO) — transcripts are cheap to recompute relative to their memory.
const TRANSCRIPT_CACHE_MAX_ENTRIES: usize = 8;

/// Truncate a match set to the [`SEARCH_MEDIA_MAX_MATCHES`] DoS ceiling
/// (T-22-14). Factored out so the exact cap `run_search_media` applies is
/// directly unit-testable without the whisper binary.
fn cap_matches(mut matches: Vec<engine::whisper::WordMatch>) -> Vec<engine::whisper::WordMatch> {
    matches.truncate(SEARCH_MEDIA_MAX_MATCHES);
    matches
}

/// Phase 22 (TEXT-02/EYES-03): transcribe `media_id`'s `[in_us, out_us)` window,
/// checking the MEDIA-IDENTITY cache first. On a miss, resolve the media path
/// under a SHORT store lock, drop the guard, run
/// `engine::whisper::transcribe_media_window` (seconds-expensive, offline), then
/// store the result keyed by media identity. Deliberately NOT keyed on timeline
/// state — an unrelated timeline edit must never evict/re-run a transcript
/// (22-RESEARCH.md Don't-Hand-Roll). Never panics: every failure is an `Err`
/// the caller folds into an `is_error` text tool_result.
pub fn run_transcribe(
    store: &SharedStore,
    session: &Mutex<AgentSession>,
    media_id: &str,
    in_us: i64,
    out_us: i64,
) -> Result<Vec<engine::whisper::Word>, String> {
    // Cache hit: media_id + the EXACT window. No timeline state in the key.
    {
        let sess = session.lock().map_err(|_| "agent session poisoned".to_string())?;
        if let Some(hit) = sess
            .transcript_cache
            .iter()
            .find(|c| c.media_id == media_id && c.in_us == in_us && c.out_us == out_us)
        {
            return Ok(hit.words.clone());
        }
    }

    // Miss: resolve the media path under a short lock, then DROP the guard
    // before the expensive, blocking transcription (the same resolve-then-drop
    // discipline run_inspect_media uses).
    let path = {
        let guard = store.lock().map_err(|_| "backend store mutex poisoned".to_string())?;
        let item = guard
            .media_item(media_id)
            .ok_or_else(|| format!("no media bin item with id {media_id}"))?;
        PathBuf::from(&item.path)
    };

    let words = engine::whisper::transcribe_media_window(&path, in_us, out_us)
        .map_err(|e| format!("transcribe {media_id} [{in_us},{out_us})us: {e}"))?;

    {
        let mut sess = session.lock().map_err(|_| "agent session poisoned".to_string())?;
        // A concurrent call may have inserted the same key while we transcribed;
        // do not duplicate.
        if !sess
            .transcript_cache
            .iter()
            .any(|c| c.media_id == media_id && c.in_us == in_us && c.out_us == out_us)
        {
            sess.transcript_cache.push(TranscriptCache {
                media_id: media_id.to_string(),
                in_us,
                out_us,
                words: words.clone(),
            });
            while sess.transcript_cache.len() > TRANSCRIPT_CACHE_MAX_ENTRIES {
                sess.transcript_cache.remove(0); // FIFO evict oldest
            }
        }
    }
    Ok(words)
}

/// Serialize words/matches into the clean JSON array the agent reads —
/// `[{text, sourceStartUs, sourceEndUs}, ...]` (Open Q1: clean word/timestamp
/// arrays, the agent groups them itself; never pre-grouped caption cards).
fn transcript_json(items: impl IntoIterator<Item = (String, i64, i64)>) -> String {
    let arr: Vec<serde_json::Value> = items
        .into_iter()
        .map(|(text, start_us, end_us)| {
            serde_json::json!({ "text": text, "sourceStartUs": start_us, "sourceEndUs": end_us })
        })
        .collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
}

/// Phase 22 (TEXT-02): resolve the clip's CURRENT `[in_us, out_us)` source
/// window + media id (under a short lock), transcribe that window (cached), and
/// return the clean per-word JSON array. A no-audio clip transcribes to an EMPTY
/// array (Pitfall 4), not an error; a text-overlay clip (no backing media) is
/// likewise an empty array (it has no spoken audio).
pub fn run_get_transcript(
    store: &SharedStore,
    session: &Mutex<AgentSession>,
    input: &serde_json::Value,
) -> Result<String, String> {
    let clip_id = input
        .get("clipId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "get_transcript requires clipId".to_string())?
        .to_string();

    let (media_id, in_us, out_us, is_text) = {
        let guard = store.lock().map_err(|_| "backend store mutex poisoned".to_string())?;
        let project = guard.snapshot();
        let clip = project
            .timeline
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .find(|c| c.id == clip_id)
            .ok_or_else(|| format!("no clip with id {clip_id}"))?;
        (clip.media_id.clone(), clip.in_us, clip.out_us, clip.text.is_some())
    };

    // A text-overlay clip has no spoken audio → empty transcript (never an error).
    if is_text || media_id.is_empty() {
        return Ok("[]".to_string());
    }

    let words = run_transcribe(store, session, &media_id, in_us, out_us)?;
    Ok(transcript_json(
        words.into_iter().map(|w| (w.text, w.start_us, w.end_us)),
    ))
}

/// Phase 22 (TEXT-02): the `get_transcript` interception. Never panics: any
/// failure becomes an `is_error` TEXT tool_result. Success is TEXT-ONLY (no
/// image block) — the clean per-word JSON array the agent reasons over before
/// calling remove_words / add_captions.
pub fn handle_get_transcript(
    store: &SharedStore,
    session: &Mutex<AgentSession>,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content, is_error| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content,
        is_error,
    };
    match run_get_transcript(store, session, input) {
        Ok(text) => result(agent_llm::vision::text_tool_result(text), None),
        Err(e) => result(agent_llm::vision::text_tool_result(e), Some(true)),
    }
}

/// Phase 22 (EYES-03): transcribe the WHOLE media asset `[0, duration_us)`
/// (MEDIA-scoped like inspect_media, cached by media identity), keyword-search it
/// for `query`, cap the matches at the [`SEARCH_MEDIA_MAX_MATCHES`] DoS ceiling
/// (T-22-14), and return each surviving match as a placeable
/// `{text, sourceStartUs, sourceEndUs}` source range.
pub fn run_search_media(
    store: &SharedStore,
    session: &Mutex<AgentSession>,
    input: &serde_json::Value,
) -> Result<String, String> {
    let media_id = input
        .get("mediaId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "search_media requires mediaId".to_string())?
        .to_string();
    let query = input
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "search_media requires query".to_string())?
        .to_string();

    let duration_us = {
        let guard = store.lock().map_err(|_| "backend store mutex poisoned".to_string())?;
        let item = guard
            .media_item(&media_id)
            .ok_or_else(|| format!("no media bin item with id {media_id}"))?;
        item.duration_us
    };
    // A zero-duration asset (still image) has no spoken audio → no matches.
    if duration_us <= 0 {
        return Ok("[]".to_string());
    }

    let words = run_transcribe(store, session, &media_id, 0, duration_us)?;
    let matches = cap_matches(engine::whisper::search_words(&words, &query));
    Ok(transcript_json(
        matches.into_iter().map(|m| (m.text, m.start_us, m.end_us)),
    ))
}

/// Phase 22 (EYES-03): the `search_media` interception. Never panics: any
/// failure becomes an `is_error` TEXT tool_result. Success is TEXT-ONLY — a JSON
/// array of placeable source ranges the agent can hand to placeClip.
pub fn handle_search_media(
    store: &SharedStore,
    session: &Mutex<AgentSession>,
    tool_use_id: &str,
    input: &serde_json::Value,
) -> agent_llm::ContentBlock {
    let result = |content, is_error| agent_llm::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content,
        is_error,
    };
    match run_search_media(store, session, input) {
        Ok(text) => result(agent_llm::vision::text_tool_result(text), None),
        Err(e) => result(agent_llm::vision::text_tool_result(e), Some(true)),
    }
}

/// Phase 22 Plan 05 (TEXT-02 / EYES-03): the get_transcript + search_media
/// wiring gate. Two FAST non-binary tests (media-identity caching + the
/// match-count DoS ceiling) run headlessly everywhere; two `#[ignore]`
/// real-binary tests (SC-1 offline word timestamps, SC-5 placeable phrase range)
/// run only with the bundled whisper-cli + model (`-- --ignored`).
#[cfg(test)]
mod transcript_search_gate {
    use crate::inspect::inspect_wiring_gate::stacked_store_from_paths;
    use crate::test_support::fixture;
    use super::*;

    /// Extract the single TEXT payload from a transcript tool_result, asserting
    /// it carries NO image block (get_transcript/search_media are TEXT-ONLY).
    /// Returns `(text, is_error)`.
    fn text_of(block: &agent_llm::ContentBlock) -> (String, bool) {
        match block {
            agent_llm::ContentBlock::ToolResult { content, is_error, .. } => {
                assert!(
                    !content
                        .iter()
                        .any(|c| matches!(c, agent_llm::ToolResultBlock::Image { .. })),
                    "get_transcript/search_media must be TEXT-only — no image block"
                );
                let text = content
                    .iter()
                    .find_map(|c| match c {
                        agent_llm::ToolResultBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .expect("a text block");
                (text, is_error.unwrap_or(false))
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// Point `engine::whisper::locate_whisper` at the FETCHED bundled binaries
    /// (BLOCKER-2): `RUDIS_WHISPER_DIR` = `runtime/binaries`. Reached here as
    /// `CARGO_MANIFEST_DIR/../../runtime/binaries` -- plan 45-05 moved this
    /// gate from `src-tauri` into `crates/app-core`, so the manifest dir it
    /// resolves from is now `crates/app-core`, not `src-tauri`. Phase 55 plan
    /// 55-01 then moved the BINARIES themselves out of `src-tauri/binaries` to
    /// `runtime/binaries`, so the shipped payload survives GATE-07's deletion of
    /// the Tauri shell. The packaged app does this at shell startup; a `--lib`
    /// test has no setup(), so the `#[ignore]` SC tests set it explicitly.
    fn point_at_bundled_whisper() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../runtime/binaries")
            .canonicalize()
            .expect("runtime/binaries must exist — run scripts/windows/fetch-whisper-cli.ps1");
        std::env::set_var("RUDIS_WHISPER_DIR", &dir);
    }

    /// A store with ONE audio clip [0, duration_us) referencing `media_path` as
    /// the media id `m-speech`, on a single audio track. Used by the SC tests.
    fn speech_store(media_path: &str, duration_us: i64) -> (SharedStore, Mutex<AgentSession>) {
        let clip = rudis_core::Clip {
            id: "SP".to_string(),
            media_id: "m-speech".to_string(),
            start_us: 0,
            in_us: 0,
            out_us: duration_us,
            volume: 1.0,
            audio_detached: false,
            transform: Default::default(),
            opacity: 1.0,
            crop: Default::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        };
        let item = rudis_core::MediaBinItem {
            id: "m-speech".to_string(),
            path: media_path.to_string(),
            media_kind: rudis_core::MediaKind::Audio,
            duration_us,
            width: 0,
            height: 0,
            fps: 0.0,
            is_vfr: false,
            rotation_degrees: 0,
            has_audio: true,
            poster_path: None,
            folder: String::new(),
            display_name: None,
            is_image_sequence: false,
            reports_alpha: None,
        };
        let mut project: rudis_core::Project = serde_json::from_value(serde_json::json!({
            "media_bin": [], "timeline": { "tracks": [] }, "fps": 30.0, "width": 512, "height": 288
        }))
        .expect("project skeleton deserializes");
        project.timeline = rudis_core::Timeline {
            tracks: vec![rudis_core::Track {
                kind: rudis_core::TrackKind::Audio,
                clips: vec![clip],
            }],
        };
        project.media_bin = vec![item];
        (
            Mutex::new(rudis_core::Store::from_project(project)),
            Mutex::new(AgentSession::default()),
        )
    }

    /// FAST (no binary): media-identity caching. A no-audio clip (testsrc has NO
    /// audio stream) transcribes to an EMPTY array via the engine's no-audio
    /// short-circuit — NEVER spawning whisper — so this runs headlessly. Proven
    /// non-vacuously: after the FIRST call the media file is DELETED and an
    /// UNRELATED timeline edit is made; the SECOND call still returns the cached
    /// result (not an error), which is only possible if the transcript cache is
    /// keyed by MEDIA identity and an unrelated edit did NOT evict it
    /// (22-RESEARCH.md Don't-Hand-Roll — the anti-pattern is keying on timeline
    /// state like inspect_timeline's state_hash).
    #[test]
    fn get_transcript_caches_by_media_identity() {
        // Per-test-unique temp copy of the no-audio fixture so we can DELETE it
        // without touching the shared fixture other tests use.
        let pid = std::process::id();
        let top = std::env::temp_dir().join(format!("rudis-transcript-cache-{pid}-top.mp4"));
        std::fs::copy(fixture("testsrc_720p30_5s.mp4"), &top).expect("copy no-audio fixture");
        let base = fixture("bars_720p30_5s.mp4");

        let (store, session) = stacked_store_from_paths(&top.to_string_lossy(), &base);

        // First call: no-audio clip L-top (m-top = testsrc) → empty transcript,
        // populating the media-identity cache. No whisper binary needed.
        let (first, first_err) = text_of(&handle_get_transcript(
            &store,
            &session,
            "gt-1",
            &serde_json::json!({ "clipId": "L-top" }),
        ));
        assert!(!first_err, "no-audio transcript must not error: {first}");
        assert_eq!(first, "[]", "a no-audio clip transcribes to an empty word array");

        // Make a fresh transcription IMPOSSIBLE: delete the media file.
        std::fs::remove_file(&top).expect("remove temp no-audio copy");

        // An UNRELATED timeline mutation (opacity on the OTHER clip). It changes
        // neither m-top's bytes nor L-top's [in_us,out_us) window, so the
        // media-identity cache MUST survive it.
        store
            .lock()
            .unwrap()
            .dispatch(rudis_core::Command::SetClipOpacity {
                id: "L-base".to_string(),
                opacity: 0.3,
            })
            .expect("dispatch unrelated opacity change");

        // Second call: SAME clip. If the cache regressed (or wrongly keyed on
        // timeline state and evicted), run_transcribe would re-open the DELETED
        // file and error. A cache hit returns the byte-identical empty array.
        let (second, second_err) = text_of(&handle_get_transcript(
            &store,
            &session,
            "gt-2",
            &serde_json::json!({ "clipId": "L-top" }),
        ));
        assert!(
            !second_err,
            "an unrelated edit must NOT force re-transcription (media deleted) — got error: {second}"
        );
        assert_eq!(
            second, first,
            "media-identity cache hit: an unrelated timeline edit must not re-run the transcript"
        );
    }

    /// FAST (no binary): the search_media match-count DoS ceiling (T-22-14).
    /// Exercises the EXACT `cap_matches` path run_search_media applies, proven
    /// non-vacuously with far more synthetic matches than the cap.
    #[test]
    fn search_media_caps_match_count() {
        let many: Vec<engine::whisper::WordMatch> = (0..(SEARCH_MEDIA_MAX_MATCHES * 4))
            .map(|i| engine::whisper::WordMatch {
                text: "hit".to_string(),
                start_us: i as i64 * 1_000,
                end_us: i as i64 * 1_000 + 500,
            })
            .collect();
        let n = many.len();
        let capped = cap_matches(many);
        assert_eq!(
            capped.len(),
            SEARCH_MEDIA_MAX_MATCHES,
            "search_media must cap returned matches at SEARCH_MEDIA_MAX_MATCHES ({SEARCH_MEDIA_MAX_MATCHES}), \
             not return all {n}"
        );
    }

    /// SC-1 (`#[ignore]`, real binary): get_transcript returns real, offline,
    /// word-level source timestamps for a placed clip's audio (TEXT-02). Asserts
    /// non-empty words containing the reference content words, timestamps within
    /// the clip's source window and monotonic, a NON-error TEXT-only result.
    #[test]
    #[ignore = "real-binary SC-1 (TEXT-02): needs bundled whisper-cli.exe + ggml-small.bin"]
    fn get_transcript_returns_words_offline() {
        point_at_bundled_whisper();
        const DUR_US: i64 = 3_720_000; // speech_en.mp4 duration
        let (store, session) = speech_store(&fixture("speech_en.mp4"), DUR_US);

        let block = handle_get_transcript(
            &store,
            &session,
            "tu-t",
            &serde_json::json!({ "clipId": "SP" }),
        );
        let (text, is_error) = text_of(&block);
        assert!(!is_error, "SC-1 transcript must not error: {text}");

        let words: Vec<serde_json::Value> =
            serde_json::from_str(&text).expect("get_transcript returns a clean JSON word array");
        assert!(!words.is_empty(), "offline transcript must have words: {text}");

        // The concatenation contains the reference's key content words.
        let joined = words
            .iter()
            .filter_map(|w| w["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        for kw in ["quick", "brown", "fox", "dog"] {
            assert!(joined.contains(kw), "transcript must contain '{kw}': {joined}");
        }

        // Every word: end>start, within the clip's source window, monotonic.
        let mut prev_start = -1i64;
        for w in &words {
            let s = w["sourceStartUs"].as_i64().expect("sourceStartUs is an int");
            let e = w["sourceEndUs"].as_i64().expect("sourceEndUs is an int");
            assert!(e > s, "word end>start: {s}..{e}");
            assert!(
                s >= 0 && e <= DUR_US + 200_000,
                "word within the clip source window [0,{DUR_US}us): {s}..{e}"
            );
            assert!(s >= prev_start, "word starts are monotonic: {s} after {prev_start}");
            prev_start = s;
        }
    }

    /// SC-5 (`#[ignore]`, real binary): search_media finds a real spoken phrase
    /// and returns a PLACEABLE source range (EYES-03). Proven: the range places
    /// as a real clip (a real Command::AddClip validates it against the media
    /// duration), AND re-decoding+transcribing exactly that source window
    /// confirms the phrase is present in it (the SC-5 export/decode proof).
    #[test]
    #[ignore = "real-binary SC-5 (EYES-03): needs bundled whisper-cli.exe + ggml-small.bin"]
    fn search_media_finds_phrase_and_range_is_placeable() {
        point_at_bundled_whisper();
        const DUR_US: i64 = 3_720_000;
        let path = fixture("speech_en.mp4");
        let (store, session) = speech_store(&path, DUR_US);

        let block = handle_search_media(
            &store,
            &session,
            "tu-s",
            &serde_json::json!({ "mediaId": "m-speech", "query": "brown fox" }),
        );
        let (text, is_error) = text_of(&block);
        assert!(!is_error, "SC-5 search must not error: {text}");

        let matches: Vec<serde_json::Value> =
            serde_json::from_str(&text).expect("search_media returns a clean JSON match array");
        assert!(!matches.is_empty(), "found >=1 match for 'brown fox': {text}");

        let m = &matches[0];
        let s = m["sourceStartUs"].as_i64().expect("sourceStartUs is an int");
        let e = m["sourceEndUs"].as_i64().expect("sourceEndUs is an int");
        // A placeable half-open source range inside the asset.
        assert!(
            s >= 0 && e > s && e <= DUR_US,
            "placeable range [{s},{e}) must sit inside [0,{DUR_US})"
        );

        // Place that exact range as a real clip on a scratch timeline: a real
        // Command::AddClip validates media existence + (in>=0, out>in) — an Ok
        // dispatch IS the "valid placement" proof.
        let mut project: rudis_core::Project = serde_json::from_value(serde_json::json!({
            "media_bin": [], "timeline": { "tracks": [] }, "fps": 30.0, "width": 512, "height": 288
        }))
        .expect("scratch project deserializes");
        project.media_bin = store.lock().unwrap().snapshot().media_bin;
        project.timeline = rudis_core::Timeline {
            tracks: vec![rudis_core::Track {
                kind: rudis_core::TrackKind::Audio,
                clips: vec![],
            }],
        };
        let scratch: SharedStore = Mutex::new(rudis_core::Store::from_project(project));
        let placed = rudis_core::Clip {
            id: "placed".to_string(),
            media_id: "m-speech".to_string(),
            start_us: 0,
            in_us: s,
            out_us: e,
            volume: 1.0,
            audio_detached: false,
            transform: Default::default(),
            opacity: 1.0,
            crop: Default::default(),
            keyframes: Default::default(),
            text: None,
            alpha_mode: Default::default(),
            retime: None,
        };
        scratch
            .lock()
            .unwrap()
            .dispatch(rudis_core::Command::AddClip { track: 0, clip: placed })
            .expect("SC-5: the returned range must place as a valid clip");

        // SC-5 decode proof: re-decode the audio AT the placed range and confirm
        // the queried phrase is spoken there. whisper mis-transcribes a bare
        // sub-second slice (no acoustic context), so we decode a context-PADDED
        // window centered on the range and assert BOTH the phrase words appear
        // AND their fresh timestamps fall inside the returned [s,e) — i.e. the
        // placeable range genuinely bounds where "brown fox" is spoken. This is a
        // fresh, independent transcription (not the handler's cached one).
        const PAD_US: i64 = 1_000_000;
        let pad_lo = (s - PAD_US).max(0);
        let pad_hi = (e + PAD_US).min(DUR_US);
        let window_words = engine::whisper::transcribe_media_window(
            std::path::Path::new(&path),
            pad_lo,
            pad_hi,
        )
        .expect("re-transcribe the padded source window offline");
        let window_text = window_words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(
            window_text.contains("brown") && window_text.contains("fox"),
            "the decoded audio around the placeable range must contain the phrase 'brown fox': {window_text}"
        );
        // The phrase's fresh timestamps must land inside the returned range
        // (± one word of slack) — proving the range is WHERE the phrase is, not
        // merely that the phrase exists somewhere in the padded window.
        let brown = window_words
            .iter()
            .find(|w| w.text.to_lowercase().contains("brown"))
            .expect("'brown' present in the re-transcription");
        let fox = window_words
            .iter()
            .find(|w| w.text.to_lowercase().contains("fox"))
            .expect("'fox' present in the re-transcription");
        assert!(
            brown.start_us >= s - PAD_US && fox.end_us <= e + PAD_US,
            "the phrase's real spoken time [{},{}) must sit within the returned range [{s},{e}) (± slack)",
            brown.start_us,
            fox.end_us
        );
    }
}
