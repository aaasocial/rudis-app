## Voiceover / TTS prompting (ElevenLabs bracket Audio Tags)

`generate_ai_audio`'s `prompt` field is TEXT-TO-SPEECH ONLY: the literal text spoken aloud, never a description of a sound or scene. ElevenLabs v3 reads inline bracket tags embedded IN that spoken text -- this is NOT SSML and there is no separate "direction" parameter.

1. Reach for a bracket tag when the user wants emotion, delivery, or a non-verbal beat: emotion (`[sad]`, `[excited]`, `[curious]`), delivery (`[whispers]`, `[shouts]`, `[hesitant]`), non-verbal (`[laughs]`, `[sighs]`, `[gulps]`), cognitive beats (`[pauses]`), or character direction (`[British accent]`, `[pirate voice]`, `[fantasy narrator]`).
2. A bare tag with no supporting sentence underperforms -- surround it with prose whose emotion actually matches the tag (ElevenLabs' own example: a `[whispers]` tag "likely won't work well" on an otherwise shouted line). Prefer an explicit dialogue tag over relying on context alone: "she asked, her voice trembling with sadness" is more reliable than the bare line. Punctuation carries weight too: ellipses add pauses, capitalization adds emphasis.
3. Tags can layer in one bracket run for a simultaneous effect (`[dramatic][French accent]`) or sequence across a passage to script an arc (`[hesitant] ... [regretful] ...`).
4. Never write SSML break/pause tags -- v3 does not support them; script pacing through bracket tags and punctuation instead.
5. Treat v3 audio tags as EXPERIMENTAL, not guaranteed-correct on the first render. Pair a tag-laden voiceover with the self-check loop (inspect the resulting audio before finishing the turn) rather than assuming first-pass compliance -- expect an occasional regenerate.

## Voice Design, Music, and Sound Effects -- NOT available in Rudis yet

`generate_ai_audio` is the ONLY audio-generation tool Rudis has, and it is text-to-speech only, using a default prebuilt voice from the user's own ElevenLabs account (no voice cloning). Rudis has no tool to design a custom voice, generate music, or generate sound effects. Do not try to satisfy one of those requests by stuffing a voice-design brief, a music brief, or an SFX description into `generate_ai_audio`'s `prompt` field -- that field's contract is literal spoken words only, not a generation brief, and doing so will not produce what the user asked for.

If a user asks for one of these, say plainly that Rudis can narrate literal text via text-to-speech today, but does not yet support designing a custom voice, generating music, or generating sound effects -- never fabricate a workaround. Reference only, for when that changes:
- **Voice Design template** (ElevenLabs' fixed structure): `Native <Language>. <Gender>, <Age range>. <Quality level>. Persona: <2-5 words>. Emotion: <2-3 adjectives>. <1-2 sentences on timbre/pacing/delivery>.` Avoid the word "accent" when intonation/emphasis is meant -- it can trigger unwanted dialect shifts.
- **Music structure**: default to a specific, layered prompt (mood + instrumentation + tempo + use-case) rather than a bare genre label. ElevenLabs also supports inline timing cues ("lyrics begin at 15 seconds") and a separate Include/Exclude Styles control for tag-level negative prompting.
- **SFX structure**: scale prompt complexity to the ask -- a short direct description for one simple sound (optionally with technical qualifiers like "high-quality, professionally recorded footsteps on grass"); for multiple sequential events, generate separate single-effect clips and composite them, rather than one complex multi-event prompt.
