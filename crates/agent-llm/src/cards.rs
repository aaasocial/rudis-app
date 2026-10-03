//! Agent-proposed option cards (CANV-02): the data model Claude's
//! `proposeOptions` control tool produces, plus the deterministic apply path.
//!
//! Two hard design commitments (mirroring 14-RESEARCH.md Pattern 3):
//!
//! 1. **Applying a chosen card is NOT another Claude round-trip.** The user's
//!    click is a deterministic choice — `apply_option_card` is a synchronous
//!    resolve→dispatch through the EXACT `dispatch_edit` path every edit
//!    `tool_use` already uses (T-14-09: a card can only ever do what a normal
//!    tool_use call could already do; no new privilege surface).
//!
//! 2. **One card = one undo entry** (AGENT-03 precedent): the whole apply is
//!    bracketed in its own `begin_turn()`/`end_turn()` pair, and a FAILED
//!    dispatch pushes nothing (`Store::end_turn` on an empty group is a
//!    documented no-op), so an invalid card leaves the `Store` byte-identical.

/// One concrete, user-choosable way to satisfy an open-ended request. Each
/// card carries exactly ONE existing edit-tool invocation — the same
/// `{"tool": name, "args": {...}}` wire shape every eval fixture's
/// `tool_calls` entries already use.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OptionCard {
    /// Unique within this proposal, Claude-chosen (e.g. "opt-a").
    pub id: String,
    /// Short UI label, e.g. "Speed ramp".
    pub label: String,
    /// One-line "why", per CANV-02's design intent.
    pub rationale: String,
    /// An existing edit tool's wire name (e.g. "trimClip") — resolved through
    /// `parse_edit_tool`'s exact boundary, never trusted as pre-validated.
    pub tool: String,
    /// The tool's args object, exactly as a live tool_use would carry.
    pub args: serde_json::Value,
}

/// Apply a chosen card's underlying edit against the live `Store` as ONE
/// undoable step. Returns the dispatch summary PLUS the real patches the
/// command produced (so a caller — Plan 14-05's Tauri command — can emit
/// `project:changed` without a second lookup).
///
/// On any failure (unknown tool name, args that fail to resolve against the
/// current project) returns `Err(..)` and leaves the `Store` UNCHANGED — no
/// partial mutation, no undo entry pushed.
pub fn apply_option_card(
    store: &mut rudis_core::Store,
    card: &OptionCard,
) -> Result<(String, Vec<(rudis_core::Patch, u64, u64)>), String> {
    store.begin_turn();
    let mut patches = Vec::new();
    let result = crate::turn::dispatch_edit(store, &card.tool, &card.args, &mut patches);
    // end_turn runs regardless of the dispatch outcome (mirrors
    // run_agent_turn's "always close the undo group" discipline). A FAILED
    // dispatch pushed nothing to the open group (its Err path returns before
    // store.dispatch and before extending `patches`), and `Store::end_turn`
    // on an empty group is a documented no-op — so the Err path leaves the
    // Store unchanged with no extra handling.
    store.end_turn();
    result.map(|summary| (summary, patches))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudis_core::Store;
    use serde_json::json;

    /// A minimal one-clip, 30fps project (same fixture conventions as
    /// `crate::turn`'s inline tests — a real-looking path, no decoded file).
    fn seed_store() -> Store {
        let project: rudis_core::Project = serde_json::from_value(json!({
            "media_bin": [
                { "id": "media-1", "path": "test-media/bars_720p30_5s.mp4", "media_kind": "video", "duration_us": 5_000_000, "width": 1280, "height": 720, "fps": 30.0, "is_vfr": false, "rotation_degrees": 0, "has_audio": true, "poster_path": null }
            ],
            "timeline": { "tracks": [
                { "kind": "video", "clips": [
                    { "id": "clip-1", "media_id": "media-1", "start_us": 0, "in_us": 0, "out_us": 2_000_000, "volume": 1.0, "audio_detached": false }
                ] },
                { "kind": "audio", "clips": [] }
            ] }
        }))
        .expect("seed project deserializes");
        Store::from_project(project)
    }

    fn trim_card() -> OptionCard {
        OptionCard {
            id: "opt-a".to_string(),
            label: "Trim the end".to_string(),
            rationale: "Tightening the outro makes the cut land harder.".to_string(),
            tool: "trimClip".to_string(),
            args: json!({ "clipId": "clip-1", "edge": "end", "toFrame": 45 }),
        }
    }

    #[test]
    fn applying_a_card_dispatches_one_undoable_edit_and_returns_patches() {
        let mut store = seed_store();
        let (summary, patches) =
            apply_option_card(&mut store, &trim_card()).expect("card applies");

        // The EXACT real mutation trimClip would make: 45 frames at 30fps =
        // 45 * frame_step_us(30.0) = 45 * 33_333 us (traced expected value).
        assert_eq!(
            store.snapshot().timeline.tracks[0].clips[0].out_us,
            1_499_985,
            "the card's underlying trimClip really trimmed the clip"
        );
        assert!(!summary.is_empty(), "summary carries the dispatch result");
        assert!(
            !patches.is_empty(),
            "the returned patches are the real dispatched patches (Plan 14-05 \
             emits project:changed from these without a second lookup)"
        );

        // ONE card = ONE undo entry, no leftover steps (AGENT-03 precedent).
        store.undo().expect("one undo reverts the card");
        assert_eq!(
            store.snapshot(),
            seed_store().snapshot(),
            "undo restores the pre-card project exactly"
        );
        assert!(!store.can_undo(), "no leftover undo steps after the one revert");
    }

    #[test]
    fn unknown_tool_card_is_err_and_leaves_store_unchanged() {
        let mut store = seed_store();
        let card = OptionCard {
            tool: "frobnicate".to_string(),
            ..trim_card()
        };
        let result = apply_option_card(&mut store, &card);
        assert!(result.is_err(), "an unknown tool name must be a clean Err");
        assert!(
            !store.can_undo(),
            "a failed card pushes NO undo entry (empty turn group is a no-op)"
        );
        assert_eq!(
            store.snapshot(),
            seed_store().snapshot(),
            "the Store is byte-identical to before the call"
        );
    }

    #[test]
    fn unresolvable_args_card_is_err_and_leaves_store_unchanged() {
        let mut store = seed_store();
        let card = OptionCard {
            args: json!({ "clipId": "no-such-clip", "edge": "end", "toFrame": 45 }),
            ..trim_card()
        };
        let result = apply_option_card(&mut store, &card);
        assert!(
            result.is_err(),
            "args that fail to resolve against the current project must be Err"
        );
        assert!(!store.can_undo(), "no undo entry pushed on a failed resolve");
        assert_eq!(store.snapshot(), seed_store().snapshot());
    }
}
