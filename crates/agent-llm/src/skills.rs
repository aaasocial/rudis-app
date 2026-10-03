//! MOAT-04 skill playbooks (Phase 14, Plan 14-02): a hand-rolled, static
//! manifest of task-specific playbooks plus a host-side keyword matcher.
//!
//! This is deliberately NOT Anthropic's official Agent Skills feature (that
//! requires the code-execution/container tool — infrastructure Rudis's plain
//! Messages-API integration does not use; 14-RESEARCH.md State of the Art).
//! It is the rudis-agent-rulebook-guide.md Principle-9 analog: keep the
//! rulebook core small and cache-stable; load a playbook's body into the
//! prompt ONLY when the user's request matches its triggers — or on demand
//! via the `read_skill` tool (Phase 17-03, TOOL-05), which looks a body up
//! by id through [`skill_body`]. Matching runs against the chat request text
//! only (DECISIONS.md A5) and is a pure, deterministic function — provable
//! with hand-built string literals, zero model, zero network.
//!
//! Per DECISIONS.md A4, the ONE shipped playbook gives advice EXECUTABLE with
//! the existing v1 edit tools: audio-ducking (detachAudio + setClipVolume,
//! with setClipMuted explicitly forbidden) — never research's illustrative
//! "split-screen" (v1's compositor has no multi-layer blend, so that advice
//! would be un-executable). The old pacing playbook was retired in Phase
//! 17-03 (D-04): `ripple_delete_ranges` performs the cut-then-close-up it
//! taught as one atomic tool call, so the two-step recipe is obsolete (the
//! `tightenPacing` TOOL itself is untouched and still works).
//!
//! Phase 41 added `elevenlabs-audio-prompting`. Phase 42.3 (SKILL-01) added
//! `video-transitions` and `generation-prompting` — the two bodies EXTRACTED
//! out of the always-loaded rulebook when `rulebook_v3.md` was cut from v2,
//! so guidance that was dead weight on a "mute this clip" turn is now paid
//! for only when the request is actually about generating something.
//!
//! Quick task 260730-x2t added `speed-ramp`. It clears A4 (every step resolves
//! to a SHIPPED tool: `set_clip_properties`, `set_keyframes`, `detachAudio`,
//! `move_clips`, `ripple_delete_ranges`) and D-04 (it does NOT narrate what one
//! tool already does atomically — the Phase-17 pacing playbook was retired for
//! exactly that; this teaches four things NO schema field can encode: the
//! slow-motion frame floor, the detached-audio desync trap, the non-rippling
//! gap/overlap consequence, and ramp craft).
//!
//! Quick task 260801-1gn added `vfx` — VFX prompt-construction guidance
//! ADAPTED from a third-party skill document (see PROVENANCE.md Entry 34; the
//! verbatim source is frozen at
//! `.planning/research/vfx-shot-prompt-builder-SKILL-source.md`). The body was
//! REBUILT from a REPLACEMENT source later the same day — the owner swapped
//! the upstream document for `vfx-shot-prompt-builder`, and the first source
//! (`seedance-vfx`, PROVENANCE Entry 33) plus everything derived from it was
//! removed from the repo rather than left to diverge.
//!
//! It clears A4 (every step resolves to a SHIPPED tool: `generate_ai_video`,
//! `generate_ai_image`, `generate_ai_audio`, `add_track`, `placeClip`,
//! `set_clip_properties`, `set_keyframes`, `track_object`, `place_overlay`)
//! and D-04 (it narrates NOTHING a single tool already does atomically — it
//! teaches integration craft, the anchor problem the Canvas exists to solve,
//! the three REAL ways to put an effect onto footage that already exists, and
//! the accurate refusal line for what Rudis genuinely cannot do).
//!
//! TWO adaptations are load-bearing and are pinned by tests below:
//!   * **The source's ADDRESSING SYNTAX stays banned — but the mechanism it
//!     addresses is no longer absent.** The source is written for edit models
//!     and hands a whole clip in as `Match the source footage @[Video 1]
//!     (video_1)`. Rudis has no such addressing and never will, so that
//!     syntax stays out of the body. What CHANGED at Phase 56 (GEN-11) is the
//!     CAPABILITY behind it: `aleph2` is no longer GEN-08-rejected, and the
//!     body now teaches BOTH routes — `generate_ai_video`'s ONE reference
//!     image that becomes the shot's FIRST FRAME, and
//!     `generate_ai_video_edit`, which re-renders an EXISTING clip's own
//!     pixels. The pin is
//!     `the_vfx_playbook_teaches_the_clip_edit_route_and_keeps_the_hard_refusals`,
//!     which carries the old syntax ban forward unchanged and adds the new
//!     claim in both directions.
//!   * **Model identity is DELETED, not translated.** The source names six
//!     models in its own frontmatter. Phase 54's D-18 bans agent-tier model
//!     identity from the interface, and `no_model_identity_in_the_vfx_playbook`
//!     enforces that at the skill layer.
//!
//! KNOWN NAME/SCOPE NOTE, recorded rather than renamed: the id `speed-ramp`
//! honours the parked v3 seed (`.planning/seeds/v3-skill-marketplace.md`, and
//! `V3-AGENT-TOOL-SKILL-PATTERNS.md` B1's worked example is literally
//! `name: Speed ramp`) even though the BODY also covers CONSTANT speed. The
//! triggers cover both vocabularies, and renaming away from the seed would
//! contradict the parked plan.

/// One loadable skill playbook: a stable id, lowercase trigger
/// keywords/phrases, a one-line summary for the always-present skill index
/// (D-07), and the full markdown body compiled in via `include_str!`
/// (mirrors the rulebook's own pattern).
#[derive(Debug)]
pub struct SkillPlaybook {
    pub id: &'static str,
    /// Lowercase keyword/phrase list, matched as case-insensitive substrings
    /// of the request text.
    pub triggers: &'static [&'static str],
    /// One-line summary rendered into the per-turn skill index by
    /// [`render_skill_index`], so the agent knows what `read_skill(id)`
    /// would load without paying for the full body.
    pub summary: &'static str,
    /// The playbook markdown body.
    pub body: &'static str,
}

/// The static, versioned skill manifest. Declared order is the order
/// multi-matching requests receive playbooks in.
pub const SKILL_PLAYBOOKS: &[SkillPlaybook] = &[
    SkillPlaybook {
        id: "audio-ducking",
        triggers: &[
            "duck",
            "ducking",
            "lower the music",
            "background music",
            "under the dialogue",
            "under the narration",
        ],
        summary: "Lower background music under narration by detaching audio and reducing its gain.",
        body: include_str!("skills/audio_ducking.md"),
    },
    SkillPlaybook {
        id: "elevenlabs-audio-prompting",
        triggers: &[
            "voiceover",
            "text-to-speech",
            "tts",
            "elevenlabs",
            "audio tag",
            "bracket tag",
            "voice design",
            "sound effect",
            "generate a voice",
        ],
        summary: "Author ElevenLabs-ready prompts for narration/TTS (bracket audio tags), and reference Voice Design / Music / SFX structure.",
        body: include_str!("skills/elevenlabs_audio_prompting.md"),
    },
    // Phase 42.3 (SKILL-01): the two bodies EXTRACTED out of the always-loaded
    // rulebook (v2's "# Transitions between two clips" and "# Prompt
    // expansion") now live here, reachable by trigger match or `read_skill`.
    // Trigger provenance is REAL recorded user phrasing from STATE.md's
    // quick-task history, not guessed vocabulary -- `matching_playbooks` is a
    // plain substring matcher with no semantic fallback, so an under-covered
    // trigger list would silently drop the guidance.
    SkillPlaybook {
        id: "video-transitions",
        triggers: &[
            "transition",
            "connect these",
            "bridge",
            "fly from",
            "morph into",
            "dissolve to",
            "blend into",
            "from one clip to",
        ],
        summary: "A-to-B transition between two clips: both endpoint frames as real references, shape \"transition\", prompt spent on the move -- read BEFORE generating any transition.",
        body: include_str!("skills/video_transitions.md"),
    },
    SkillPlaybook {
        id: "generation-prompting",
        triggers: &[
            "generate",
            "make me a video",
            "make me an image",
            "make me a picture",
            "create a video",
            "create an image",
            "ai video",
            "ai image",
            "photoreal",
        ],
        summary: "Per-model prompt depth for generate_ai_image/video/audio: Runway per-tier video structure and length, full-sentence image detail, negation-to-positive conversion.",
        body: include_str!("skills/generation_prompting.md"),
    },
    // Quick task 260730-x2t: retime. Multi-word triggers first -- and NOTE the
    // deliberate absence of bare "speed"/"slow": `matching_playbooks` is a
    // plain case-insensitive SUBSTRING matcher, so "speed" alone would fire on
    // "speed up the export" and "slow" on "the render is slow".
    //
    // IN-04: the same argument retires bare "2x"/"0.5x". A SUBSTRING matcher
    // fires them on "scale it 2x", "make the title 2x bigger" and "a 2x2 grid"
    // -- none of which are retime asks -- and the body is ~155 lines of context
    // paid for on every such turn. The multi-word forms below cover every real
    // phrasing ("put this at 2x", "run it at 2x speed"), and "speed up" /
    // "double speed" / "half speed" already carry the rest.
    SkillPlaybook {
        id: "speed-ramp",
        triggers: &[
            "speed up",
            "speed ramp",
            "slow motion",
            "slow-mo",
            "slowmo",
            "half speed",
            "double speed",
            "at 2x",
            "2x speed",
            "at 0.5x",
            "0.5x speed",
            "fast forward",
            "time lapse",
            "timelapse",
            "ramp into",
            "retime",
            "play it faster",
            "make it faster",
            "slow this",
            "slow it down",
        ],
        summary: "Retime a clip: constant speed vs a speed ramp, the slow-motion frame floor, keeping detached audio in sync, and closing the gap retiming leaves.",
        body: include_str!("skills/speed_ramp.md"),
    },
    // Quick task 260801-1gn: VFX. Adapted from a third-party skill doc
    // (PROVENANCE Entry 34, which SUPERSEDES the withdrawn Entry 33 — the
    // owner replaced the upstream source and the first one is gone from the
    // repo). Model identity scrubbed per Phase 54 D-18.
    //
    // D-04 clear: this body teaches things NO schema field encodes -- that AI
    // VFX fails on INTEGRATION rather than on the element, how to resolve the
    // effect's ANCHOR (the Canvas path, which is Rudis's own answer to the
    // source's hardest step), the three REAL apply-to-existing-footage
    // workflows, and the accurate line between what Rudis can and cannot do to
    // an existing clip's pixels. It narrates no tool's atomic behaviour.
    //
    // TRIGGERS, and the IN-04 substring reasoning behind every ABSENCE. The
    // matcher (`matching_playbooks`) is a plain case-insensitive SUBSTRING
    // test with no word boundaries and no semantic fallback, so a bare token
    // that hides inside a common editing word injects ~140 lines of context
    // into a turn that is not about effects at all. Bare tokens REFUSED here,
    // each with the word that eats it -- every one is pinned by a negative
    // case in `non_vfx_asks_do_not_load_the_vfx_playbook`:
    //   "rain"      inside "film grain", "constraint"   -> multi-word forms only
    //   "raining"   inside "training" (the training video) -> dropped entirely
    //   "dust"      inside "industry standard"          -> "dust cloud" only
    //   "mist"      inside "I made a mistake"           -> "misty"/"add mist"
    //   "storm"     inside "brainstorm the edit"        -> "thunderstorm"/"storm cloud"
    //   "transform" = a SCALE/fit verb in every editor  -> "transformation" only
    //   "weather"   inside "weathered skin"             -> "weather effect" only
    //   "fog"/"foggy" inside "a foggy harbor"           -> "add fog"/"fog effect"/"fog rolling"
    //   "plate"     inside "the title template"         -> dropped entirely
    //   "fire"/"snow" = ordinary FOOTAGE SUBJECTS ("the campfire clip", "the
    //                  snowboarding clip")              -> "add fire"/"on fire"/"add snow"/...
    // "weathered skin" and "a foggy harbor" are MEASURED, not imagined: both
    // are literal text of the promoted live-eval fixture
    // eval-047-prompt-expansion-detailed-passthrough, whose whole subject is
    // how much prompt surface a generation turn carries -- the last place a
    // stray body should land. "lightning" is kept bare BECAUSE "lighting" does
    // not contain it (also pinned negatively).
    //
    // The 2026-08-01 REPLACEMENT source carries its own trigger guidance in
    // its frontmatter `description`, and those phrasings are honoured here as
    // literal multi-word triggers: "VFX prompt" (covered by bare "vfx"), "add
    // an effect to this shot", "make this explode" (bare "explode"), "put me
    // in a X", "turn my room into", "video-to-video prompt", "how do I prompt
    // this effect", plus the effect classes it names (creatures, holograms,
    // levitation, world replacement, destruction, transformations). The
    // source's remaining trigger -- "the user names a video edit model" --
    // is deliberately NOT expressible here: naming a model is exactly what
    // D-18 keeps out of this interface, and the effect vocabulary below
    // catches the same asks by what they want rather than by which model the
    // user happened to have heard of.
    //
    // Two source phrasings are NOT expressible as substrings and are accepted
    // as misses: "put me in a X"/"turn my X into Y" generalise over a slot the
    // matcher has no wildcard for, so only the source's own literal examples
    // are pinned. That is acceptable BECAUSE a missed trigger is RECOVERABLE
    // and a false positive is not: the summary below is rendered into the
    // skill index EVERY turn (D-07), so the agent can always
    // `read_skill("vfx")` on judgment. That asymmetry is why this list stays
    // conservative.
    //
    // "green screen", "chroma key" and "rotoscope" are triggers ON PURPOSE
    // even though Rudis cannot do any of them: those asks are precisely the
    // ones that need this body's honest-refusal section plus its real
    // alternatives, and a silent wrong promise is the worst outcome available.
    //
    // Most VFX asks contain "generate", so `generation-prompting` (index 3)
    // and `vfx` (index 5) deliberately load TOGETHER, in that declared order
    // -- pinned by `a_generate_vfx_ask_loads_generation_prompting_and_vfx`.
    // They are layers: that one governs prompt LENGTH and negation handling,
    // this one governs the effect's physics and how it reaches the timeline.
    SkillPlaybook {
        id: "vfx",
        triggers: &[
            // --- the replacement source's own trigger phrasings ---
            "vfx",
            "add an effect",
            "this effect",
            "video-to-video",
            "put me in a",
            "turn my room into",
            // --- effect classes ---
            "visual effect",
            "special effect",
            "weather effect",
            "rain effect",
            "fire effect",
            "fog effect",
            "make it rain",
            "add rain",
            "heavy rain",
            "rain falling",
            "fog rolling",
            "add fog",
            "add mist",
            "add snow",
            "make it snow",
            "snow falling",
            "thunderstorm",
            "storm cloud",
            "dust cloud",
            "add fire",
            "on fire",
            "particle",
            "explosion",
            "explode",
            "smoke",
            "flame",
            "spark",
            "shockwave",
            "debris",
            "lightning",
            "energy",
            "magic",
            "glow",
            "misty",
            "disintegrate",
            "shatter",
            "destruction",
            "transformation",
            "creature",
            "hologram",
            "levitate",
            "levitation",
            // --- world replacement ---
            "replace the background",
            "sky replacement",
            // --- asks that must reach the honest refusal, not a silent no ---
            "green screen",
            "chroma key",
            "rotoscope",
        ],
        summary: "Build a VFX prompt for a shot: integration craft (interactive light, contact, occlusion, format match), anchoring the effect on the frame, and the three real ways to put an effect onto footage you already have -- first-frame generation, altering the plate first, and overlay compositing.",
        body: include_str!("skills/vfx.md"),
    },
];

/// Every playbook with at least one trigger appearing (case-insensitive
/// substring) in the request text, in `SKILL_PLAYBOOKS` declared order.
/// Pure and deterministic; an empty/non-matching request yields an empty Vec.
pub fn matching_playbooks(request: &str) -> Vec<&'static SkillPlaybook> {
    let lower = request.to_lowercase();
    SKILL_PLAYBOOKS
        .iter()
        .filter(|p| p.triggers.iter().any(|t| lower.contains(t)))
        .collect()
}

/// Render matched playbooks as the text block Plan 14-05 splices into the
/// second (uncached) system block: `# Skill: {id}` headers with each full
/// body verbatim. An empty slice renders an empty string, never panics.
pub fn render_playbooks_block(playbooks: &[&SkillPlaybook]) -> String {
    playbooks
        .iter()
        .map(|p| format!("# Skill: {}\n{}\n", p.id, p.body))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Look a playbook's full markdown body up by exact id — the `read_skill`
/// tool's resolution step (Phase 17-03, TOOL-05). Purely an in-memory match
/// over the compiled-in [`SKILL_PLAYBOOKS`] list: the id NEVER maps to a
/// filesystem path, so path traversal is structurally impossible (T-17-07).
/// An unknown id is a benign `None`, never a panic.
pub fn skill_body(id: &str) -> Option<&'static str> {
    SKILL_PLAYBOOKS.iter().find(|p| p.id == id).map(|p| p.body)
}

/// Render the one-line-per-playbook skill index (`- {id}: {summary}`) that
/// the host's `run_agent_turn` (`crates/app-core/src/agent_turn.rs`) appends to
/// the UNCACHED dynamic system block
/// ON EVERY TURN (Phase 17-03, D-07) — so the agent always knows which skill
/// ids exist for `read_skill`, even when no retrieval example or playbook
/// trigger matches the request.
pub fn render_skill_index() -> String {
    SKILL_PLAYBOOKS
        .iter()
        .map(|p| format!("- {}: {}", p.id, p.summary))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playbook_bodies_are_nonempty_and_v1_executable_topics() {
        assert_eq!(
            SKILL_PLAYBOOKS.len(),
            6,
            "6 shipped playbooks after quick task 260801-1gn added vfx"
        );
        for p in SKILL_PLAYBOOKS {
            assert!(!p.body.trim().is_empty(), "playbook {} has an empty body", p.id);
            assert!(!p.triggers.is_empty(), "playbook {} has no triggers", p.id);
            assert!(!p.summary.trim().is_empty(), "playbook {} has no summary", p.id);
        }
        assert_eq!(SKILL_PLAYBOOKS[0].id, "audio-ducking");
        // A4: advice must reference only v1-executable tools.
        assert!(SKILL_PLAYBOOKS[0].body.contains("detachAudio"));
        assert!(SKILL_PLAYBOOKS[0].body.contains("setClipVolume"));
        assert!(
            SKILL_PLAYBOOKS[0].body.contains("setClipMuted"),
            "ducking playbook must explicitly forbid setClipMuted"
        );
        assert_eq!(SKILL_PLAYBOOKS[1].id, "elevenlabs-audio-prompting");
        // Phase 41 SKILL-01: the TTS section must reference the real,
        // dispatchable tool and a real bracket-tag example.
        assert!(SKILL_PLAYBOOKS[1].body.contains("generate_ai_audio"));
        assert!(SKILL_PLAYBOOKS[1].body.contains("[whispers]"));
        // Content-scope guard (41-RESEARCH.md's Content-Scope Tension): Voice
        // Design/Music/SFX must be framed as not-yet-available reference
        // material, never an executable recipe -- Rudis has no tool to
        // design a voice / generate music / generate SFX.
        assert!(
            SKILL_PLAYBOOKS[1].body.contains("NOT available in Rudis yet"),
            "voice design/music/sfx section must be explicitly labeled unavailable"
        );

        // Phase 42.3 SKILL-01: the two bodies extracted OUT of the always-loaded
        // rulebook. Their substance must survive the move -- these needles are
        // the extraction's proof that nothing was lost on the way across.
        //
        // Needles are matched against a WHITESPACE-SQUASHED copy of the body.
        // These files are hard-wrapped at ~72 columns, so a phrase the guard
        // exists to protect can straddle a line break ("a single person /
        // walking alone" does). Guarding raw text would make the assertion a
        // function of where the wrap happens to fall rather than of whether
        // the sentence is still there -- and a re-wrap is exactly the edit
        // this guard must survive.
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");

        assert_eq!(SKILL_PLAYBOOKS[2].id, "video-transitions");
        let transitions = squash(SKILL_PLAYBOOKS[2].body);
        for needle in [
            "clipEnd",
            "clipStart",
            // Phase 55.1 plan 05 (D-11): `shape "transition"` was the schema
            // word this recipe told the agent to pass. The field is DELETED,
            // so the needle is re-cut to the thing that actually makes a
            // transition now -- both frames, plus a model that can bridge
            // them. Replaced rather than dropped: a recipe that stopped
            // saying which models can do a first+last pair would send the
            // agent's own free-text model choice at a model that cannot.
            "first+last keyframe pair",
            "match cut",
            "must NOT change",
            "plausibly interpolate",
            "NO audio",
        ] {
            assert!(
                transitions.contains(needle),
                "video-transitions playbook must carry the extracted needle `{needle}`"
            );
        }
        // Item (G) retires `intent` for `shape`: the RENAMED vocabulary must not
        // ship in a brand-new file.
        assert!(
            !transitions.contains("intent \"transition\""),
            "the retired (G) `intent` vocabulary must not appear in a new file"
        );

        assert_eq!(SKILL_PLAYBOOKS[3].id, "generation-prompting");
        let prompting = squash(SKILL_PLAYBOOKS[3].body);
        for needle in [
            "Triviality check FIRST",
            "The camera [motion] as the subject [action]",
            "a single person walking alone",
            // Phase 55.1 plan 05 (D-02): `PER-TIER` was the extracted body's
            // length rule while Rudis resolved the model from a `shape`/`stage`
            // capability word. Those fields are DELETED (D-11) and the agent
            // now names the model itself, so the needle that replaces it pins
            // the same load-bearing claim re-cut for the new world: prompt
            // length and structure are a function of the MODEL, not a flat
            // rule. Replaced rather than dropped -- the guard exists so a
            // rewrite cannot quietly flatten this back to one length for
            // everything.
            "follow the MODEL you named",
            "camera choreography",
            "CONTEXT to describe",
            // D-07: the injection rule is DUPLICATED here on purpose (this
            // playbook is loaded for exactly the asks that spend money), so
            // its model-choice clause has to survive here too.
            "never a valid source for a model id",
        ] {
            assert!(
                prompting.contains(needle),
                "generation-prompting playbook must carry the extracted needle `{needle}`"
            );
        }

        // Quick task 260730-x2t: the retime playbook. A4 needles -- every step
        // must resolve to a tool that actually ships -- plus the four things
        // this body exists to teach (the reason it clears D-04).
        assert_eq!(SKILL_PLAYBOOKS[4].id, "speed-ramp");
        let ramp = squash(SKILL_PLAYBOOKS[4].body);
        let ramp_lower = ramp.to_lowercase();
        for needle in [
            // A4: shipped tools only.
            "set_clip_properties",
            "set_keyframes",
            "detachAudio",
            "move_clips",
            "ripple_delete_ranges",
            // (1) the slow-motion frame floor.
            "no optical-flow",
            // (2) the detached-audio desync trap -- the highest-value line.
            "no linked-clip concept",
            "silent desync",
            // (3) the ripple consequence.
            "leaves a gap",
            "overlaps the next clip",
            // (4) ramp craft.
            "interp",
            "hold",
            "smooth",
            "integrated over time",
        ] {
            assert!(
                ramp_lower.contains(&needle.to_lowercase()),
                "speed-ramp playbook must carry the needle `{needle}`"
            );
        }
        // The retime-specific arithmetic trap the agent otherwise gets wrong.
        assert!(
            ramp.contains("duration_frames"),
            "speed-ramp playbook must tell the agent to read duration_frames \
             rather than deriving a retimed duration from source in/out"
        );
        // It must NOT offer reverse playback, which is out of scope (RT-08).
        assert!(
            ramp.contains("Reverse playback (negative speed) does not exist"),
            "speed-ramp playbook must state that reverse playback is unavailable"
        );
        // WR-07: the ENFORCED speed-key cap is `rudis_core::MAX_RETIME_KEYS`
        // (64), not `set_keyframes`' general 1000. The playbook and the tool
        // schema must both advertise 64 or the agent authors a legal-per-schema
        // call the backend hard-rejects -- and `set_keyframes` is
        // full-track-replace and all-or-none, so that is a dead end the model
        // has no documented way out of.
        assert!(
            ramp.contains("64"),
            "speed-ramp playbook must state the 64-key cap on a speed track, \
             not leave the agent with set_keyframes' general 1000"
        );
        assert!(
            ramp.contains("not 1000"),
            "the playbook must call out that the speed cap DIFFERS from the \
             general keyframe cap -- stating 64 alone still leaves the schema's \
             1000 as the more prominent number"
        );

        // Quick task 260801-1gn: the VFX playbook, REBUILT 2026-08-01 from the
        // replacement source (PROVENANCE Entry 34). Same discipline as the
        // retime body above -- A4 needles (every referenced tool SHIPS), plus
        // the substance this adaptation exists to carry: the craft that
        // survived from the third-party source, the hard facts of the
        // generation surface, and the refusal line that must stay honest.
        assert_eq!(SKILL_PLAYBOOKS[5].id, "vfx");
        let vfx = squash(SKILL_PLAYBOOKS[5].body).to_lowercase();
        for needle in [
            // A4: shipped tools only, spelled exactly as the schemas spell them.
            "generate_ai_video",
            // Phase 56 plan 08 (GEN-11): the 59th tool. Listed separately from
            // `generate_ai_video` even though it CONTAINS it as a substring --
            // the sibling needle would go on passing if every mention of the
            // edit tool were deleted, which is exactly the regression this
            // phase's body exists to prevent.
            "generate_ai_video_edit",
            "generate_ai_image",
            "generate_ai_audio",
            "add_track",
            "placeclip",
            "set_clip_properties",
            "set_keyframes",
            "track_object",
            "place_overlay",
            // the conditioning vocabulary the three workflows turn on.
            "referencesource",
            "clipend",
            "destinationsource",
            // the craft core adapted from the source (PROVENANCE Entry 34).
            // These are the source's own load-bearing claims, kept because
            // they are what make a generated element read as filmed.
            "fails on integration",
            "interactive light",
            "specular",
            "contact shadow",
            "occlusion",
            "only do this:",
            // the ANCHOR problem, and Rudis's own answer to it. The source
            // solves it with a read-back sentence; Rudis can solve it with
            // pixels, so the Canvas path is stated as PRIMARY and the
            // read-back survives as the fallback.
            "unique visible feature",
            "annotated",
            "canvas",
            // hard facts about the generation surface a wrong promise breaks.
            "4-second",
            "picture only",
            // Phase 56 plan 08 (D-07): the clip edit comes back picture-only
            // and lands in the BIN, so the source clip's audio only survives
            // if the agent detaches it BEFORE covering the clip. It is the one
            // step in this body whose omission destroys work the user already
            // had, which is why it is a needle here as well as a whole
            // assertion in the clip-edit test below.
            "detachaudio",
            // Phase 55.1 plan 05 (D-02): was "the tier follows the frames you
            // pass". There are no tiers to follow any more -- the agent names
            // the model itself in a required free-text field -- so the needle
            // is re-cut to the claim that replaced it. Both halves are pinned
            // because a vfx body that mentioned the field without the cost
            // consequence would let the agent name the most expensive model
            // for an ordinary effect ask and never know it had.
            "you name the model yourself",
            "cost follows the model you pick",
            "do not re-describe",
            // the fake capability this body must never teach: the source has
            // a "Sound:" section, and generate_ai_audio is text-to-SPEECH.
            "text-to-speech",
            // the honest refusals (.planning/seeds/v3-advanced-vfx.md).
            "content-aware removal",
            "rotoscoping",
            "chroma key",
        ] {
            assert!(
                vfx.contains(needle),
                "vfx playbook must carry the needle `{needle}`"
            );
        }
    }

    // RETIRED 2026-08-09 by Phase 56 (GEN-11), plan 56-08:
    // `the_vfx_playbook_teaches_image_to_video_not_video_to_video`.
    //
    // That test existed to ban the source document's video-to-video reference
    // syntax at a time when the edit MECHANISM was GEN-08-rejected — its own
    // failure message said so: "the edit model that syntax addresses is GEN-08
    // REJECTED". The owner signed GEN-08 on 2026-08-09 and that mechanism
    // shipped under the signature as `generate_ai_video_edit` (56-07), so the
    // test's stated reason is now false and a guard whose reason is false
    // invites the next reader to delete it for the wrong reason — the same
    // hazard 55.1-05 recorded one test below.
    //
    // It is RETIRED WHOLE rather than edited down, because a weakened copy of
    // assertions written for the opposite world is worse than none. The
    // successor is
    // `the_vfx_playbook_teaches_the_clip_edit_route_and_keeps_the_hard_refusals`,
    // which carries BOTH of the retired test's assertions forward unchanged
    // (the five source-syntax substrings, and "first frame") and adds the new
    // claim's own two directions. Nothing this test checked is unchecked now.

    /// **The clip-edit route, pinned in BOTH directions — SC-6.**
    ///
    /// Phase 56 inverts part of this body's refusal list, and the phase's own
    /// CONTEXT names the worst available outcome: *"a sloppy rewrite here
    /// converts an honest refusal into a false promise"*, with the asymmetry
    /// stated outright — a missed capability is recoverable, a silently wrong
    /// promise is not. So this test asserts the inversion is PRECISE, not
    /// merely present:
    ///
    /// 1. the NEW capability is taught by the tool's REAL name, with the three
    ///    facts that make it usable rather than dangerous (it lands a NEW
    ///    ASSET, the source's audio needs `detachAudio` FIRST, references are
    ///    REFUSED, and the price is confirmed or declared unknown);
    /// 2. the retired ABSOLUTES are gone (`nothing re-renders an existing
    ///    clip`, `no recolour`) — a body that kept them would be refusing the
    ///    thing this phase shipped;
    /// 3. the KEPT refusals survive **in the body's own words**, asserted as
    ///    whole sentences and not merely as keywords, because a keyword can
    ///    survive inside a sentence that has quietly started promising the
    ///    opposite;
    /// 4. the source document's addressing syntax is STILL absent — the one
    ///    thing the retired test guarded that no phase changes. Rudis's tool
    ///    takes a `clipId`, never `@[Video 1](video_1)`.
    #[test]
    fn the_vfx_playbook_teaches_the_clip_edit_route_and_keeps_the_hard_refusals() {
        let body = skill_body("vfx").expect("the vfx playbook is registered");
        let lower = body.to_lowercase();
        let squashed = lower.split_whitespace().collect::<Vec<_>>().join(" ");

        // (1) The new claim. Each of these is a fact a user LOSES money or
        // work over if the body stops carrying it.
        for (needle, why) in [
            (
                "generate_ai_video_edit",
                "the clip-edit tool by its real name -- the body's whole \
                 vocabulary for the capability (56-07)",
            ),
            (
                "new asset",
                "D-06: the edit lands a NEW MediaBin asset and the source clip \
                 survives; an agent that thinks it edits in place will not place \
                 the result at all",
            ),
            (
                "detachaudio",
                "D-07: the result is picture-only, so covering the source clip \
                 without detaching its audio FIRST silently loses the take's sound",
            ),
            (
                "splitclip",
                "D-01: when the input window refuses, the body must relay the \
                 real remedy rather than let the agent pick a sub-range",
            ),
            (
                "references are refused",
                "F-1b left the endpoint's reference field uncrowned, so the tool \
                 REFUSES any reference; a body that taught a reference workflow \
                 would be promising a capability the endpoint does not have",
            ),
            (
                "price is unknown",
                "the spend posture: a real price for this clip's range, or the \
                 honest absence -- never a figure the agent invented (D-56-05-01)",
            ),
            (
                "first frame",
                "CARRIED FORWARD from the retired test: `generate_ai_video` is \
                 still image-to-video, and the edit route is an ADDITION to that \
                 teaching rather than a replacement for it",
            ),
        ] {
            assert!(
                squashed.contains(needle),
                "the vfx playbook must teach `{needle}` -- {why}"
            );
        }

        // (2) The absolutes this phase falsified. Keeping either would have the
        // agent refuse a capability that now ships.
        for (retired, why) in [
            (
                "nothing re-renders an existing clip",
                "generate_ai_video_edit re-renders exactly that",
            ),
            (
                "no recolour",
                "recolour-in-place is one of the three asks this phase made real",
            ),
            (
                "no video-to-video edit pass",
                "there is one now, and it is the point of Phase 56",
            ),
        ] {
            assert!(
                !squashed.contains(retired),
                "the vfx playbook still carries the retired absolute \
                 `{retired}` -- {why}. Phase 56 (GEN-11) shipped the mechanism; \
                 a body that refuses it is refusing a capability the user paid \
                 for"
            );
        }

        // (3) SC-6's other half, and the half a sloppy rewrite loses. These
        // stay refused, and they are asserted as the body's OWN SENTENCES
        // rather than as bare keywords: `rotoscoping` can survive as a word in
        // a paragraph that has started offering it.
        for kept in [
            "**rotoscoping, masks, segmentation, chroma key** -- no per-object \
             mask, no cut-out, no green-screen key. `crop` is four \
             straight-edged insets.",
            "nothing takes an object out of a clip's frames and fills in behind it",
        ] {
            let kept = kept.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                squashed.contains(&kept),
                "the vfx playbook must keep this refusal VERBATIM -- Phase 56 \
                 inverts relight / recolour-in-place / background swap and \
                 NOTHING else (56-CONTEXT, Deferred Ideas). Missing: {kept:?}"
            );
        }
        // Belt and braces on the same claim: each refused word is still in the
        // body somewhere, so a rewrap of the sentence above cannot silently
        // take one out with it.
        for word in [
            "rotoscoping",
            "masks",
            "segmentation",
            "chroma key",
            "content-aware removal",
        ] {
            assert!(
                lower.contains(word),
                "the vfx playbook must still refuse `{word}` by name"
            );
        }

        // (4) The retired test's own ban, carried forward BYTE-UNCHANGED. The
        // capability arrived; the third-party addressing syntax did not, and
        // never will -- `generate_ai_video_edit` takes a `clipId`.
        for syntax in ["@[video", "video_1", "@[image", "image_1", "image_2"] {
            assert!(
                !lower.contains(syntax),
                "the vfx playbook carries the source document's edit-model \
                 reference syntax `{syntax}` -- Rudis addresses a clip by its \
                 `clipId`, and a body that taught that syntax would be pointing \
                 the agent at a mechanism this product does not have"
            );
        }
    }

    /// Phase 54's D-18 model-identity ban, enforced at the SKILL layer.
    ///
    /// **Its ORIGINAL justification is gone; the ban is not.** D-18 rested on
    /// `generate_ai_video`'s own schema promising "you choose the capability,
    /// never the model, and there is no way to name one" — a promise Phase
    /// 55.1 deliberately retired (D-01/D-10: `model` is now a REQUIRED free-
    /// text field, and the rulebook's cost table names every roster model
    /// outright). What survives is the OTHER reason this test exists, and it
    /// is the reason it was written: the `vfx` body is ADAPTED from a
    /// third-party document written FOR named models (PROVENANCE Entry 34) —
    /// its own frontmatter names SIX of them — and this is what keeps the
    /// adaptation from leaking any of them back in. A model name appearing
    /// here would be evidence the source text bled through, not guidance
    /// Rudis authored.
    ///
    /// It is therefore kept, unweakened, with its ban list and both matching
    /// modes byte-unchanged through 55.1 — the vfx rewrite that would revisit
    /// it belongs to Phase 56 (D-04). Only the rationale above and the failure
    /// message below were corrected, because a guard whose stated reason is
    /// false invites the next reader to delete it for the wrong reason.
    ///
    /// It is proven to FAIL against the unadapted source text, and it reports
    /// EVERY leaked identity in one run rather than stopping at the first, so
    /// a RED is a complete scrub list rather than one name at a time.
    #[test]
    fn no_model_identity_in_the_vfx_playbook() {
        let body = skill_body("vfx").expect("the vfx playbook is registered");
        let lower = body.to_lowercase();

        // Checked as SUBSTRINGS: none of these hides inside an ordinary
        // English or editing word, so a substring test is free and catches
        // "Seedance-VFX", "Runway Aleph", "Veo 3.1" and "Gen-4" alike.
        const SUBSTRING_BANNED: &[&str] = &[
            "seedance",
            "runway",
            "aleph",
            "higgsfield",
            "veo",
            "gen-4",
            "gen4",
        ];
        // Checked as WHOLE WORDS, deliberately, because these two DO hide
        // inside words a VFX body legitimately wants: "kling" sits inside
        // "crackling" and "sparkling", and "luma" is also the video term for
        // the brightness plane. Banning them as substrings would be a booby
        // trap for the next author rather than a guard.
        const WORD_BANNED: &[&str] = &["kling", "luma"];

        let words: Vec<&str> = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();

        let mut leaked: Vec<&str> = SUBSTRING_BANNED
            .iter()
            .copied()
            .filter(|b| lower.contains(b))
            .collect();
        leaked.extend(
            WORD_BANNED
                .iter()
                .copied()
                .filter(|b| words.iter().any(|w| w == b)),
        );

        assert!(
            leaked.is_empty(),
            "the vfx playbook names the model/provider identities {leaked:?} -- \
             this body is ADAPTED from a third-party document written FOR named \
             models (PROVENANCE Entry 34), so a name here is source text bleeding \
             through, not Rudis guidance. Describe the capability and point at the \
             rulebook's cost table, which names models where Rudis authored them"
        );
    }

    /// Trigger coverage for the VFX playbook against phrasings a user actually
    /// types when they want an effect.
    ///
    /// The second group is the REPLACEMENT source's own trigger guidance,
    /// taken from its frontmatter `description` and pinned literally — that
    /// document is where these phrasings come from, so a retune that dropped
    /// one would be drifting away from the skill it adapts.
    #[test]
    fn vfx_asks_match_the_vfx_playbook() {
        for request in [
            "add an explosion at the end of clip 2",
            "generate smoke drifting over the logo",
            "make sparks fly when the swords clash",
            "can you add fire to the torch",
            "make it rain in this shot",
            "add some vfx to the intro",
            "particles rising from the crowd",
            "a magic glow around her hands",
            // --- the replacement source's own listed triggers ---
            "write me a vfx prompt for this shot",
            "add an effect to this shot",
            "make this explode",
            "put me in a spaceship cockpit",
            "turn my room into a jungle",
            "write a video-to-video prompt for my clip",
            "how do i prompt this effect",
            // --- the effect classes it names ---
            "a creature looming behind me in the storm",
            "a hologram over his open hand",
            "make the lamp levitate off the desk",
            // --- asks whose whole value is reaching the honest refusal ---
            "can you green screen me out of this clip",
            "rotoscope the subject and put them on a beach",
            "chroma key the background out",
        ] {
            let ids: Vec<&str> = matching_playbooks(request).iter().map(|p| p.id).collect();
            assert!(
                ids.contains(&"vfx"),
                "`{request}` must load the vfx playbook, got {ids:?}"
            );
        }
    }

    /// The IN-04 half, and the reason the trigger list above reads the way it
    /// does. `matching_playbooks` is a bare case-insensitive SUBSTRING matcher,
    /// so each of these would fire a ~100-line body into a turn that has
    /// nothing to do with effects if the obvious bare token had been used.
    ///
    /// The last two cases are the REAL request text of promoted live-eval
    /// fixtures (eval-047, eval-048) -- the trigger list was measured against
    /// the fixture corpus, not only against invented negatives. eval-047 in
    /// particular is the fixture whose entire subject is how much prompt
    /// surface a generation turn carries
    /// (.planning/debug/resolved/eval-047-prompt-expansion-passthrough-drift.md);
    /// it says "weathered skin" and "a foggy harbor", which is exactly why bare
    /// "weather", "fog" and "foggy" are not triggers.
    #[test]
    fn non_vfx_asks_do_not_load_the_vfx_playbook() {
        for request in [
            "add film grain to this clip",             // "grain" contains "rain"
            "cut the training video intro",            // "training" contains "raining"
            "use industry standard export settings",   // "industry" contains "dust"
            "I made a mistake, undo that",             // "mistake" contains "mist"
            "let's brainstorm the edit",               // "brainstorm" contains "storm"
            "transform the clip to fit the frame",     // "transform" is a scale word
            "fix the lighting on this clip",           // "lighting" is NOT "lightning"
            "trim the campfire clip",                  // fire as a footage SUBJECT
            "trim the snowboarding clip",              // snow as a footage SUBJECT
            "use the title template on the intro",     // "template" contains "plate"
            "collapse the inspector panel",
            "trim the intro",
            "Generate a photorealistic close-up portrait of an elderly fisherman \
             with weathered skin and a grey beard, wearing a yellow raincoat, shot \
             with an 85mm lens at golden hour, soft rim lighting from the left, \
             shallow depth of field, background of a foggy harbor with silhouetted \
             boats",
            "make me a video of a lighthouse in a storm",
        ] {
            let ids: Vec<&str> = matching_playbooks(request).iter().map(|p| p.id).collect();
            assert!(
                !ids.contains(&"vfx"),
                "`{request}` is not a VFX ask, got {ids:?}"
            );
        }
    }

    /// Multi-skill coherence: a "generate ... effect" ask loads BOTH bodies, in
    /// declared order (generation-prompting at index 3, vfx last at index 5),
    /// and `render_playbooks_block` carries both verbatim. The two are designed
    /// to stack -- prompt length/negation rules first, effect physics second.
    #[test]
    fn a_generate_vfx_ask_loads_generation_prompting_and_vfx() {
        let matched = matching_playbooks("generate an explosion effect for the finale");
        let ids: Vec<&str> = matched.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["generation-prompting", "vfx"]);

        let block = render_playbooks_block(&matched);
        assert!(block.contains("# Skill: generation-prompting"));
        assert!(block.contains("# Skill: vfx"));
        assert!(
            block.contains(SKILL_PLAYBOOKS[3].body) && block.contains(SKILL_PLAYBOOKS[5].body),
            "both bodies must appear verbatim in the rendered block"
        );
    }

    /// Trigger coverage for the retime playbook, against the phrasings a user
    /// actually types. Includes the NEGATIVE case that motivated dropping bare
    /// "speed"/"slow" from the trigger list.
    #[test]
    fn retime_asks_match_the_speed_ramp_playbook() {
        for request in [
            "make this slow motion",
            "can you speed up this clip",
            "ramp into slow-mo on the jump",
            "put this at 2x",
            "retime the second clip to half speed",
            "slow it down a bit",
            "make it a timelapse",
            // IN-04: the real "2x"/"0.5x" phrasings must still match after the
            // bare tokens were retired.
            "run this at 2x speed",
            "play the b-roll at 0.5x",
            "drop it to 0.5x speed",
            "can you make it faster",
        ] {
            let ids: Vec<&str> = matching_playbooks(request)
                .iter()
                .map(|p| p.id)
                .collect();
            assert!(
                ids.contains(&"speed-ramp"),
                "`{request}` must load the retime recipe, got {ids:?}"
            );
        }
        // NEGATIVE: bare "speed"/"slow" in an unrelated sentence must NOT
        // fire -- the matcher has no semantic fallback, so an over-broad
        // trigger would inject a whole playbook into every export complaint.
        //
        // IN-04 adds the "2x"/"0.5x" half: a bare "2x" is a SCALE/size word far
        // more often than a speed word, and the playbook body is ~155 lines of
        // context paid for on every false positive.
        for request in [
            "the export is slow",
            "what is the render speed",
            "scale it 2x",
            "make the title 2x bigger",
            "lay them out in a 2x2 grid",
            "resize the logo to 0.5x its size",
            "the preview is faster now",
        ] {
            let ids: Vec<&str> = matching_playbooks(request)
                .iter()
                .map(|p| p.id)
                .collect();
            assert!(
                !ids.contains(&"speed-ramp"),
                "`{request}` is not a retime ask, got {ids:?}"
            );
        }
    }

    /// Trigger-coverage proof against a REAL recorded user phrasing (STATE.md
    /// quick-task history), not guessed vocabulary: `matching_playbooks` is a
    /// plain substring matcher with no semantic fallback, so a trigger list
    /// that misses the way users actually ask silently drops the guidance.
    #[test]
    fn real_recorded_transition_ask_matches_the_video_transitions_playbook() {
        let matched = matching_playbooks(
            "generate using veo a transition between end of clip 10 to the start of shibuya top down",
        );
        let ids: Vec<&str> = matched.iter().map(|p| p.id).collect();
        assert!(
            ids.contains(&"video-transitions"),
            "the real recorded transition ask must load the transition recipe, got {ids:?}"
        );
    }

    /// The other REAL recorded shape ("generate me a picture of ...") must load
    /// the prompt-depth playbook -- and ONLY it, since nothing in the sentence
    /// is about bridging two clips.
    #[test]
    fn terse_generate_ask_matches_the_generation_prompting_playbook() {
        let matched = matching_playbooks("generate me a picture of a dog on a beach");
        let ids: Vec<&str> = matched.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["generation-prompting"]);
    }

    // (removed) the pacing skill is retired this phase — D-04;
    // exact-single-match is now covered by
    // ducking_request_matches_exactly_the_audio_ducking_playbook below.

    #[test]
    fn ducking_request_matches_exactly_the_audio_ducking_playbook() {
        let matched = matching_playbooks("can you duck the music under the narration");
        let ids: Vec<&str> = matched.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["audio-ducking"]);
    }

    #[test]
    fn voiceover_request_matches_exactly_the_elevenlabs_audio_prompting_playbook() {
        let matched = matching_playbooks(
            "write a voiceover script using bracket audio tags for elevenlabs",
        );
        let ids: Vec<&str> = matched.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["elevenlabs-audio-prompting"]);
    }

    #[test]
    fn request_with_no_triggers_matches_none() {
        let matched = matching_playbooks("hello, add a title card");
        assert!(
            matched.is_empty(),
            "no trigger words -> no playbooks, got {:?}",
            matched.iter().map(|p| p.id).collect::<Vec<_>>()
        );
    }

    // Rewritten from the retired two-skill declared-order case (D-04: update
    // fixtures, don't delete coverage): with one surviving skill, the
    // no-double-count half of that coverage is what still matters — a request
    // hitting SEVERAL of one playbook's triggers must yield the playbook
    // exactly ONCE, never duplicated per trigger.
    #[test]
    fn request_with_multiple_ducking_triggers_returns_one_playbook_no_duplicates() {
        let matched = matching_playbooks("duck the background music under the narration");
        let ids: Vec<&str> = matched.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            vec!["audio-ducking"],
            "several matching triggers must still yield the playbook exactly once"
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        let matched = matching_playbooks("DUCK THE MUSIC");
        let ids: Vec<&str> = matched.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["audio-ducking"]);
    }

    #[test]
    fn render_playbooks_block_contains_id_and_full_body_verbatim() {
        let matched = matching_playbooks("duck the music");
        let block = render_playbooks_block(&matched);
        assert!(block.contains("# Skill: audio-ducking"), "id header missing");
        assert!(
            block.contains(SKILL_PLAYBOOKS[0].body),
            "full body must appear verbatim"
        );
    }

    #[test]
    fn render_playbooks_block_on_empty_slice_is_empty_and_never_panics() {
        let block = render_playbooks_block(&[]);
        assert!(
            block.trim().is_empty(),
            "empty slice must render an empty/near-empty string, got {block:?}"
        );
    }
}
