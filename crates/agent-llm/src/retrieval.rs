//! MOAT-03 Level-2 in-context retrieval (Phase 14, Plan 14-02).
//!
//! A curated seed library (`seed_library`, DECISIONS.md A3 — plain Rust code,
//! NOT a bundled resource directory) plus a growable runtime library on disk
//! (`append_library_entry`/`load_library`), with pure, deterministic lexical
//! token-overlap selection (`select_top_k`) and few-shot rendering
//! (`render_examples_block`). Explicitly NO vector DB / embedding model —
//! Jaccard token overlap over `std::collections::HashSet` per 14-RESEARCH.md
//! Pattern 4 and the phase's zero-new-dependency constraint.
//!
//! Selection and rendering are pure functions over in-memory data so they are
//! DET-testable with hand-built literals, zero model, zero network. Injection
//! into the actual `MessagesRequest` (a second, UNCACHED system block after
//! the cached rulebook block) is Plan 14-03/14-05's job, not this module's.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// One retrievable past request/edit pair. `tool_calls` entries use the exact
/// `{"tool": <name>, "args": {...}}` wire shape every eval fixture's
/// `tool_calls` array already uses (crates/core/tests/fixtures/agent_evals/
/// README.md), so a retrieved example reads identically to a real recorded
/// interaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryExample {
    pub id: String,
    /// The text retrieval matches against.
    pub request: String,
    /// `{"tool":..,"args":..}` entries, reused verbatim from the fixture wire shape.
    pub tool_calls: Vec<serde_json::Value>,
    /// e.g. "lasso near clip start" — optional extra signal.
    #[serde(default)]
    pub canvas_summary: Option<String>,
}

/// Tiny function-word set dropped from token streams. The `len() > 2` filter
/// below already drops "a"/"to"/"is"-class noise; these are the handful of
/// >2-char function words common enough in editing requests to create
/// spurious overlap ("the" alone would otherwise link EVERY pair of
/// requests). Deliberately minimal — real editing vocabulary ("cut", "gap",
/// "fps") must survive.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "this", "that", "you", "your", "can", "please",
];

/// Case-folded, punctuation-split token set: tokens of length > 2 kept,
/// minus `STOPWORDS`. Order-independent by construction (a set).
fn tokens(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect()
}

/// Jaccard token-overlap similarity: |A ∩ B| / |A ∪ B|, in [0.0, 1.0].
fn similarity(query_tokens: &HashSet<String>, candidate: &str) -> f64 {
    let cand = tokens(candidate);
    if query_tokens.is_empty() || cand.is_empty() {
        return 0.0;
    }
    let inter = query_tokens.intersection(&cand).count() as f64;
    let union = query_tokens.union(&cand).count() as f64;
    inter / union
}

/// Deterministic top-K over the library: scored by Jaccard similarity of
/// `request` texts, sorted `(score DESC, id ASC)` so ties never flap between
/// runs, floored at `> 0.0` (an example sharing ZERO tokens with the query is
/// never injected — DECISIONS.md A6).
pub fn select_top_k<'a>(
    query: &str,
    library: &'a [LibraryExample],
    k: usize,
) -> Vec<&'a LibraryExample> {
    let qt = tokens(query);
    let mut scored: Vec<(f64, &LibraryExample)> = library
        .iter()
        .map(|ex| (similarity(&qt, &ex.request), ex))
        .filter(|(score, _)| *score > 0.0) // never inject a totally-unrelated example
        .collect();
    // Scores are finite by construction (Jaccard of nonempty finite sets), so
    // partial_cmp never sees a NaN; the id fallback makes ties deterministic.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    scored.into_iter().take(k).map(|(_, ex)| ex).collect()
}

/// Every DISTINCT pair of entries whose `request` texts score above
/// `threshold` (Jaccard), sorted deterministically. Pitfall E1's stated
/// prevention (PITFALLS.md): "if an existing entry already scores >0.7
/// similarity, refine that entry instead of adding a new one." O(n^2) is
/// fine at this library's realistic scale (tens of entries), matching
/// `select_top_k`'s own zero-vector-DB, zero-embedding-model constraint.
pub fn near_duplicate_pairs(
    library: &[LibraryExample],
    threshold: f64,
) -> Vec<(String, String, f64)> {
    let mut out = Vec::new();
    for i in 0..library.len() {
        let qt = tokens(&library[i].request);
        for j in (i + 1)..library.len() {
            let score = similarity(&qt, &library[j].request);
            if score > threshold {
                out.push((library[i].id.clone(), library[j].id.clone(), score));
            }
        }
    }
    out
}

/// Render selected examples as the few-shot text block Plan 14-05 splices
/// into the second (uncached) system block. Each example's `request` text and
/// its `tool_calls` JSON appear verbatim.
pub fn render_examples_block(examples: &[&LibraryExample]) -> String {
    let mut out = String::from("# Similar past examples\n");
    for ex in examples {
        out.push_str(&format!("## Request: {}\n", ex.request));
        if let Some(c) = &ex.canvas_summary {
            out.push_str(&format!("Canvas: {c}\n"));
        }
        out.push_str(&format!(
            "Tool calls: {}\n\n",
            serde_json::to_string(&ex.tool_calls).unwrap_or_default()
        ));
    }
    out
}

/// The curated seed library (DECISIONS.md A3): hand-authored, reviewed,
/// compiled into the binary — mirrors how `skills::SKILL_PLAYBOOKS` is a
/// static Rust manifest, not a bundled-resource directory. Clip/media ids are
/// plausible generic placeholders ("clip-1"): these are illustrative few-shot
/// text, never resolved against a real project by this module.
pub fn seed_library() -> Vec<LibraryExample> {
    use serde_json::json;
    vec![
        LibraryExample {
            id: "seed-trim-intro".to_string(),
            request: "cut the boring intro".to_string(),
            tool_calls: vec![json!({
                "tool": "trimClip",
                "args": { "clipId": "clip-1", "edge": "start", "toFrame": 30 }
            })],
            canvas_summary: None,
        },
        LibraryExample {
            id: "seed-tighten-pacing".to_string(),
            request: "tighten the pacing on the middle section".to_string(),
            tool_calls: vec![
                json!({
                    "tool": "removeSection",
                    "args": { "track": "v1", "fromFrame": 30, "toFrame": 60, "fps": 30.0 }
                }),
                json!({
                    "tool": "tightenPacing",
                    "args": { "track": "v1", "fps": 30.0 }
                }),
            ],
            canvas_summary: None,
        },
        LibraryExample {
            id: "seed-lower-music".to_string(),
            request: "lower the background music a bit".to_string(),
            tool_calls: vec![json!({
                "tool": "setClipVolume",
                "args": { "clipId": "clip-1", "gainDb": -6.0 }
            })],
            canvas_summary: None,
        },
        LibraryExample {
            id: "seed-duplicate-outro".to_string(),
            request: "duplicate the ending clip".to_string(),
            tool_calls: vec![json!({
                "tool": "duplicateClip",
                "args": { "clipId": "clip-1" }
            })],
            canvas_summary: None,
        },
        LibraryExample {
            id: "seed-detach-narration".to_string(),
            request: "separate the narration from the video so I can adjust it".to_string(),
            tool_calls: vec![json!({
                "tool": "detachAudio",
                "args": { "clipId": "clip-1" }
            })],
            canvas_summary: None,
        },
        LibraryExample {
            id: "seed-add-captions".to_string(),
            request: "add captions to the interview clip so it's accessible".to_string(),
            tool_calls: vec![json!({
                "tool": "add_captions",
                "args": {
                    "entries": [
                        { "startFrame": 0, "endFrame": 90, "content": "Hello and welcome" },
                        { "startFrame": 90, "endFrame": 180, "content": "to the interview" }
                    ]
                }
            })],
            canvas_summary: None,
        },
        LibraryExample {
            id: "seed-side-by-side-layout".to_string(),
            request: "put these two clips side by side".to_string(),
            tool_calls: vec![json!({
                "tool": "apply_layout",
                "args": {
                    "template": "side_by_side",
                    "assignments": [
                        { "slot": "left", "clipId": "clip-1" },
                        { "slot": "right", "clipId": "clip-2" }
                    ]
                }
            })],
            canvas_summary: None,
        },
        LibraryExample {
            id: "seed-add-logo-overlay".to_string(),
            request: "put our logo overlay in the corner".to_string(),
            tool_calls: vec![json!({
                "tool": "place_overlay",
                "args": {
                    "libraryAssetId": "lib-logo-1",
                    "clipId": "overlay-logo-1",
                    "track": "v2",
                    "startFrame": 0,
                    "transform": { "position": [0.75, 0.05], "scale": [0.2, 0.2], "rotation_deg": 0 }
                }
            })],
            canvas_summary: None,
        },
    ]
}

/// Append one accepted interaction to the growable runtime library
/// (DECISIONS.md A3: real JSON files under an app-writable directory —
/// production passes `{app_data_dir}/agent_library/`, Plan 14-05/14-06).
/// Creates the directory if absent. The filename derives from BOTH the
/// timestamp AND the entry's own id, so two same-millisecond appends of
/// distinct entries never collide by construction. Stdlib only.
pub fn append_library_entry(
    entry: &LibraryExample,
    growable_dir: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(growable_dir)?;
    // A pre-1970 clock is the only failure mode; degrade to 0 rather than
    // panic (the id keeps the filename unique regardless).
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = growable_dir.join(format!("runtime-{millis}-{}.json", entry.id));
    std::fs::write(&path, serde_json::to_string_pretty(entry)?)?;
    Ok(path)
}

/// Load every parseable `*.json` entry from the growable directory. A missing
/// directory yields an empty Vec; a malformed file is SKIPPED (never a panic,
/// never aborts the whole load — one bad growable file must never break
/// retrieval for an entire session, threat T-14-05). Mirrors the eval
/// harness's own `fs::read_dir` + extension-filter loading style; entries are
/// returned in filename order (deterministic).
pub fn load_library(growable_dir: &std::path::Path) -> Vec<LibraryExample> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(growable_dir) else {
        return out; // missing/unreadable directory -> empty, never an error
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
        })
        .collect();
    paths.sort(); // filename order -> deterministic result order
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // unreadable file: skip, never abort the whole load
        };
        let Ok(example) = serde_json::from_str::<LibraryExample>(&text) else {
            continue; // malformed file: skip (T-14-05), never panic
        };
        out.push(example);
    }
    out
}

/// Phase 41 (SKILL-03): the minimal, pure signal a caller can construct to
/// make the harvesting-gate REJECT/ACCEPT decision testable (Pitfall E4,
/// PITFALLS.md). Phase 39 shipped self-check as rulebook prose ONLY
/// (rulebook_v2.md's "# Self-check" section) plus an unrelated
/// GENERATE_RETRY_CAP retry counter -- there is NO structured verdict type
/// anywhere else in this codebase. This enum is scoped ONLY to the
/// harvesting-gate decision below -- it is explicitly NOT wired into
/// the host's production `run_agent_turn` harvesting call site
/// (`crates/app-core/src/agent_turn.rs`)
/// this phase (no real verdict signal reaches that call site yet; see
/// 41-RESEARCH.md's Architecture Pattern 4 and Open Questions). A future
/// reader must not mistake this for a live production signal -- it is
/// exercised here only by tests built from Phase 39's real fixture data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfCheckVerdict {
    /// The self-check inspection confirmed the result matched the request.
    Confirmed,
    /// The self-check inspection flagged a mismatch, regardless of how many
    /// retries followed within the cap.
    FlaggedMismatch,
}

/// The harvesting gate (E4): only a `Confirmed` verdict makes a candidate
/// eligible for the growable library. Pure, total, never panics. NOT called
/// from any production code path this phase -- see the module-level note
/// above and `SelfCheckVerdict`'s own doc comment.
pub fn harvest_confirmed_outcome(
    candidate: LibraryExample,
    verdict: SelfCheckVerdict,
) -> Option<LibraryExample> {
    match verdict {
        SelfCheckVerdict::Confirmed => Some(candidate),
        SelfCheckVerdict::FlaggedMismatch => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(id: &str, request: &str) -> LibraryExample {
        LibraryExample {
            id: id.to_string(),
            request: request.to_string(),
            tool_calls: vec![serde_json::json!({"tool": "trimClip", "args": {"clipId": "clip-1", "edge": "end", "toFrame": 30}})],
            canvas_summary: None,
        }
    }

    // ---- tokens -----------------------------------------------------------

    #[test]
    fn tokens_are_case_and_punctuation_insensitive_and_drop_function_words() {
        let a = tokens("Tighten the pacing!");
        let b = tokens("tighten pacing");
        let expected: HashSet<String> =
            ["tighten", "pacing"].iter().map(|s| s.to_string()).collect();
        assert_eq!(a, expected, "punctuated/cased form must tokenize to the same set");
        assert_eq!(b, expected, "plain form must tokenize to the same set");
        assert_eq!(a, b, "both forms must produce the SAME set");
    }

    // ---- select_top_k -----------------------------------------------------

    #[test]
    fn select_top_k_returns_the_pacing_seed_first_and_never_a_zero_overlap_example() {
        let library = seed_library();
        let selected = select_top_k("tighten the pacing please", &library, 5);
        assert!(
            !selected.is_empty(),
            "a query overlapping the pacing seed must select it"
        );
        assert_eq!(
            selected[0].id, "seed-tighten-pacing",
            "the pacing example must rank FIRST (highest overlap)"
        );
        // Floor > 0.0: nothing sharing zero tokens with the query may appear.
        for s in &selected {
            let q = tokens("tighten the pacing please");
            let c = tokens(&s.request);
            assert!(
                q.intersection(&c).count() > 0,
                "selected example {} shares zero tokens with the query",
                s.id
            );
        }
    }

    #[test]
    fn select_top_k_with_no_token_overlap_returns_empty() {
        let library = seed_library();
        let selected = select_top_k("xyzzy plugh", &library, 5);
        assert!(
            selected.is_empty(),
            "a query sharing zero tokens with every example must select NOTHING, got {:?}",
            selected.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn select_top_k_breaks_score_ties_by_id_ascending() {
        // Both share exactly the token "trim" with the query and have the same
        // token count, so their Jaccard scores are computed identically —
        // ordering must fall back to id ASC, regardless of library order.
        let library = vec![ex("b-ex", "trim something"), ex("a-ex", "trim anything")];
        let selected = select_top_k("trim clip", &library, 5);
        let ids: Vec<&str> = selected.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a-ex", "b-ex"],
            "equal-score examples must be ordered by id ascending (deterministic)"
        );
    }

    // ---- render_examples_block --------------------------------------------

    #[test]
    fn render_examples_block_contains_request_and_tool_calls_verbatim() {
        let mut e = ex("ex-1", "cut the boring intro");
        e.canvas_summary = Some("lasso near clip start".to_string());
        let refs = vec![&e];
        let block = render_examples_block(&refs);
        assert!(block.contains("cut the boring intro"), "request text missing");
        assert!(
            block.contains("lasso near clip start"),
            "canvas_summary missing"
        );
        let tool_calls_json = serde_json::to_string(&e.tool_calls).unwrap();
        assert!(
            block.contains(&tool_calls_json),
            "tool_calls JSON must appear verbatim (round-trippable); block was:\n{block}"
        );
    }

    #[test]
    fn render_examples_block_on_empty_slice_never_panics() {
        let block = render_examples_block(&[]);
        // Header-only (or empty) is acceptable; must contain no example content.
        assert!(
            block.trim() == "" || block.trim() == "# Similar past examples",
            "empty slice must render header-only or empty, got: {block:?}"
        );
    }

    // ---- append_library_entry / load_library (Task 2) ----------------------

    /// A unique-per-test temp directory under the OS temp dir. No cleanup
    /// required (ephemeral CI/dev temp dirs, per the plan).
    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "rudis-retrieval-test-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn append_library_entry_round_trips_byte_identically() {
        let dir = unique_temp_dir("roundtrip");
        let entry = LibraryExample {
            id: "rt-1".to_string(),
            request: "trim the intro clip".to_string(),
            tool_calls: vec![serde_json::json!({
                "tool": "trimClip",
                "args": { "clipId": "clip-1", "edge": "start", "toFrame": 15 }
            })],
            canvas_summary: Some("arrow at clip start".to_string()),
        };
        let path = append_library_entry(&entry, &dir).expect("append succeeds");
        assert!(path.exists(), "appended file must exist at returned path");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("runtime-") && name.ends_with("-rt-1.json"),
            "filename derives from timestamp AND id, got {name}"
        );
        let text = std::fs::read_to_string(&path).expect("read back");
        let reloaded: LibraryExample = serde_json::from_str(&text).expect("parses back");
        assert_eq!(reloaded, entry, "round-trip must be identical (PartialEq)");
    }

    #[test]
    fn load_library_skips_malformed_files_and_keeps_valid_ones() {
        let dir = unique_temp_dir("malformed");
        let a = ex("grow-a", "trim the first clip");
        let b = ex("grow-b", "duplicate the last clip");
        append_library_entry(&a, &dir).expect("append a");
        append_library_entry(&b, &dir).expect("append b");
        std::fs::write(dir.join("runtime-0-broken.json"), "{ not valid json !!")
            .expect("write malformed file");
        let loaded = load_library(&dir);
        assert_eq!(
            loaded.len(),
            2,
            "exactly the 2 valid entries load; malformed is skipped, got {:?}",
            loaded.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        let mut ids: Vec<&str> = loaded.iter().map(|e| e.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["grow-a", "grow-b"]);
    }

    #[test]
    fn load_library_on_nonexistent_dir_returns_empty() {
        let dir = unique_temp_dir("missing").join("does-not-exist");
        let loaded = load_library(&dir);
        assert!(loaded.is_empty(), "missing directory must yield empty Vec, never panic");
    }

    #[test]
    fn two_appends_never_collide_on_filename() {
        let dir = unique_temp_dir("collide");
        let a = ex("uniq-a", "lower the music");
        let b = ex("uniq-b", "mute the outro");
        let pa = append_library_entry(&a, &dir).expect("append a");
        let pb = append_library_entry(&b, &dir).expect("append b");
        assert_ne!(pa, pb, "distinct entries must get distinct paths (id in filename)");
        assert!(pa.exists() && pb.exists(), "both files must exist");
    }

    // ---- seed_library ------------------------------------------------------

    #[test]
    fn seed_library_has_at_least_5_structurally_valid_entries() {
        let library = seed_library();
        assert!(
            library.len() >= 5,
            "seed library must have >= 5 entries, got {}",
            library.len()
        );
        for entry in &library {
            assert!(!entry.id.is_empty(), "entry has empty id");
            assert!(!entry.request.is_empty(), "entry {} has empty request", entry.id);
            assert!(
                !entry.tool_calls.is_empty(),
                "entry {} has no tool_calls",
                entry.id
            );
            for call in &entry.tool_calls {
                assert!(
                    call["tool"].is_string(),
                    "entry {} tool_call missing string `tool`: {call}",
                    entry.id
                );
                assert!(
                    call["args"].is_object(),
                    "entry {} tool_call missing object `args`: {call}",
                    entry.id
                );
            }
        }
        // The distinct editing intents the plan requires signal for.
        for id in [
            "seed-trim-intro",
            "seed-tighten-pacing",
            "seed-lower-music",
            "seed-duplicate-outro",
            "seed-detach-narration",
            "seed-add-captions",
            "seed-side-by-side-layout",
            "seed-add-logo-overlay",
        ] {
            assert!(
                library.iter().any(|e| e.id == id),
                "seed library missing expected entry {id}"
            );
        }
    }

    // ---- near_duplicate_pairs (Phase 41, SKILL-02) --------------------------

    #[test]
    fn near_duplicate_pairs_flags_a_synthetic_near_identical_pair_above_threshold() {
        let library = vec![
            ex("dup-a", "trim the intro clip"),
            ex("dup-b", "trim intro clip please"),
        ];
        let pairs = near_duplicate_pairs(&library, 0.7);
        assert_eq!(
            pairs.len(),
            1,
            "the two near-identical requests must be flagged as one pair, got {pairs:?}"
        );
        let (a, b, score) = &pairs[0];
        assert_eq!((a.as_str(), b.as_str()), ("dup-a", "dup-b"));
        assert!(*score > 0.7, "flagged pair's score must exceed the threshold, got {score}");
    }

    #[test]
    fn near_duplicate_pairs_does_not_flag_a_clearly_distinct_pair() {
        let library = vec![
            ex("distinct-a", "trim the intro clip"),
            ex("distinct-b", "generate a voiceover for the outro"),
        ];
        let pairs = near_duplicate_pairs(&library, 0.7);
        assert!(
            pairs.is_empty(),
            "clearly distinct requests must not be flagged, got {pairs:?}"
        );
    }

    #[test]
    fn near_duplicate_pairs_over_the_grown_seed_library_is_empty_at_threshold_0_7() {
        let library = seed_library();
        assert_eq!(
            library.len(),
            8,
            "5 original + 3 new Phase 41 curated entries (seed-add-captions, \
             seed-side-by-side-layout, seed-add-logo-overlay)"
        );
        let pairs = near_duplicate_pairs(&library, 0.7);
        assert!(
            pairs.is_empty(),
            "the grown 8-entry seed library must have zero near-duplicate pairs \
             at threshold 0.7 (Pitfall E1), got {pairs:?}"
        );
    }

    #[test]
    fn near_duplicate_pairs_also_flags_a_pair_loaded_from_the_on_disk_growable_library() {
        let dir = unique_temp_dir("near-dup-on-disk");
        append_library_entry(&ex("disk-dup-a", "duplicate the ending clip"), &dir)
            .expect("append a");
        append_library_entry(&ex("disk-dup-b", "duplicate ending clip please"), &dir)
            .expect("append b");
        let loaded = load_library(&dir);
        assert_eq!(loaded.len(), 2, "both entries must load back");
        let pairs = near_duplicate_pairs(&loaded, 0.7);
        assert_eq!(
            pairs.len(),
            1,
            "the check must also catch a near-duplicate pair sourced from the \
             on-disk growable library, not just seed_library(), got {pairs:?}"
        );
    }

    // ---- harvest_confirmed_outcome (Phase 41, SKILL-03) ---------------------

    #[test]
    fn harvest_confirmed_outcome_rejects_a_self_check_flagged_mismatch_from_eval_042() {
        // Phase 39 shipped self-check as rulebook prose only -- no structured
        // verdict exists in production. This SelfCheckVerdict is the minimal
        // one Phase 41 (SKILL-03) introduces JUST for this gate; it is NOT
        // read anywhere in production (see retrieval.rs's module doc on
        // SelfCheckVerdict). Candidate content below is transcribed VERBATIM
        // from
        // crates/core/tests/fixtures/agent_evals/eval-042-self-check-regenerate-mismatch.json's
        // round-1 tool call (the mismatch candidate the fixture's own
        // description says self-check flagged before regenerating).
        let candidate = LibraryExample {
            id: "eval-042-round-1-mismatch".to_string(),
            request: "Generate a title-card image for the intro, check it looks right, and place it -- if it's wrong, regenerate and use the better one.".to_string(),
            tool_calls: vec![serde_json::json!({
                "tool": "placeClip",
                "args": { "clipId": "clip-candidate-a", "mediaId": "media-candidate-a", "track": "v1", "startFrame": 0 }
            })],
            canvas_summary: None,
        };
        let result = harvest_confirmed_outcome(candidate, SelfCheckVerdict::FlaggedMismatch);
        assert_eq!(result, None, "a flagged-mismatch outcome must never be harvested");
    }

    #[test]
    fn harvest_confirmed_outcome_accepts_a_self_check_confirmed_outcome_from_eval_042() {
        // Same fixture, FINAL accepted state (round 2's own two tool calls):
        // candidate-a removed, candidate-b placed -- the outcome the fixture's
        // own `expected_final_project` says the turn ends on. Only a Confirmed
        // verdict makes it eligible for the growable library (E4's fix). Not a
        // live production signal -- see SelfCheckVerdict's doc comment.
        let candidate = LibraryExample {
            id: "eval-042-round-2-confirmed".to_string(),
            request: "Generate a title-card image for the intro, check it looks right, and place it -- if it's wrong, regenerate and use the better one.".to_string(),
            tool_calls: vec![
                serde_json::json!({ "tool": "removeClip", "args": { "clipId": "clip-candidate-a" } }),
                serde_json::json!({
                    "tool": "placeClip",
                    "args": { "clipId": "clip-candidate-b", "mediaId": "media-candidate-b", "track": "v1", "startFrame": 0 }
                }),
            ],
            canvas_summary: None,
        };
        let expected = candidate.clone();
        let result = harvest_confirmed_outcome(candidate, SelfCheckVerdict::Confirmed);
        assert_eq!(result, Some(expected), "a confirmed outcome is harvested unchanged");
    }
}
