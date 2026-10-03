VFX -- building the prompt for an effect, and putting it onto footage you
already have.

`generation-prompting` owns prompt length, the triviality check, negation
and the booster ban, and loads alongside this one; add cinematic, epic, VFX
and CGI to its banned boosters. They are layers: that one decides how much
to write, this one what the effect physically does.

**AI VFX fails on INTEGRATION, not on the element.** Models render
convincing fire, water and creatures unasked. What breaks the shot is fire
that does not light the wall behind it, or a creature cleaner and sharper
than the footage. Most of your words go on how the effect ACTS ON the
frame, not on what it looks like by itself.

## TWO routes -- a NEW shot, or the clip's OWN pixels

`generate_ai_video` is IMAGE-to-video. It takes a single reference image,
makes it the shot's FIRST FRAME and animates forward; there is no second
"design reference" slot on it. The plate is a FRAME you choose, the result
is a NEW 4-second asset that starts on it, and everything after that frame
is invented.

To change an EXISTING clip's own pixels -- relight it, swap or replace its
background, clean it up, restyle it -- use `generate_ai_video_edit` on the
clip itself: it sends that clip's current trimmed range (exactly what the
viewer sees) and returns a NEW edited asset in the bin. The original clip
is untouched, and the take's performance, framing and timing survive,
because the model re-renders THOSE frames rather than inventing a shot.

Choose by what has to be true. A change that must be visible from frame
one of a shot nobody has filmed is `generate_ai_video` (the workflows
below). A user pointing at footage already on the timeline and saying
"make THIS different" is `generate_ai_video_edit`.

A preservation block earns its place on BOTH: it makes the model HOLD what
it was given while it changes what you asked for, so the footage governs
the person.

```
Keep the person's face, wardrobe and identity, their performance, gestures
and timing, and the framing, lens and room exactly as in the source -- the
first frame you passed, or the clip you are editing.
Photoreal. [GRADE]. No logos, brand names or readable text.
Only do this: [THE EFFECT]
```

Everything you iterate on lives after `Only do this:`; touch the block
above only where the effect contradicts it, and only that clause.

**Preserve identity, not motion.** "Face unchanged" reads as DO NOT MOVE,
and you get a frozen person under a moving effect. Preserve the likeness,
wardrobe and framing; protect the performance POSITIVELY instead, by saying
what the face and hands DO. Unstated motion is invented or flattened.

## Fix the anchor before writing anything

Every rim, shadow and falloff depends on WHERE the effect sits; get it
wrong and the light comes from the opposite side, and a bare left/right is
ambiguous -- his left is screen right.

**The Canvas is the primary path.** Ask the user to draw on the preview
frame: a circle where the effect belongs, an arrow for its direction.
`referenceSource: "frame"` sends that annotated frame, and you can SEE the
annotation, because the canvas is composited over the frame in your vision
snapshot. The anchor stops being a sentence and becomes pixels.

If nothing is drawn, fall back to WORDS: name the anchor by a unique
visible feature, side second -- "his open cupped hand on the right of
frame, the one with the ring on it" -- then read it back in ONE sentence
before writing. A prompt on the wrong anchor is entirely wasted.

**On a clip edit the drawing cannot be SENT.** `generate_ai_video_edit`
has no usable reference slot today (see "How the shot ships here"), so a
circle on the preview answers the anchor question for YOU and you must
then answer it in WORDS inside the prompt. Still ask for the drawing:
reading "the near side of his face" off a picture you can actually see
beats guessing at it, and the words you write from it are the whole of
what the model receives.

## Writing the change

Aim for 600-1,500 characters after `Only do this:`, as five beats of prose:
what appears and WHERE, on the anchor; how it HOLDS position (the spot, a
slow bob, what it travels with); how it MOVES and why -- name the AXIS,
prefer a tilted one, one continuous speed tied to the design, because
motion with a stated purpose survives where a bare attribute drifts; what
it does to the LIGHT, the workhorse beat below; and a restatement of the
performance, repeated even though the block above says it. Anything
intermittent needs a stated RATE, and a state change a MECHANISM and a
duration, or the model fires it constantly, once, or as a crossfade that
hitches at the join. Four seconds is one beat, so a two-state effect is
usually two generations with a cut.

## Integration craft -- filmed versus composited

- **Interactive light is the single highest-value instruction.** Anything
  self-illuminated must light its surroundings in its OWN colour with
  correct falloff, on NAMED surfaces: "warm orange across the near side of
  his face, his hand and his shirt, everything else falling into shadow."
  An element that glows but lights nothing is an overlay, and the fix is
  more interactive light, never more glow.
- **Specular hits are the cheapest proof of presence.** Two reflections in
  a pair of glasses, or a glint on a ring, beat a paragraph of flame
  description. Use whatever the frame already has.
- **Contact shadow, and deformation at the touch point** -- a ball denting
  the fingertip. A floating object with no contact shadow is the classic
  fake. State the occlusion when something crosses in front of it, and
  match the frame's grain and depth of field.
- **Force is proven by what the world does** -- dust punching outward and
  lingering, NAMED targets -- never by the effect itself.

## How the shot ships here

`generate_ai_video` renders a NEW shot. Three facts to design around:

- **A FIXED 4-second 720p 16:9 clip**, picture ONLY. Trim it on the
  timeline, or retime it (`speed-ramp`).
- **It comes back silent, always.** Rudis's one generated-audio path,
  `generate_ai_audio`, is text-to-speech: it cannot make a roar, a whoosh
  or an impact (see `elevenlabs-audio-prompting`). Effect sound must come
  from media already in the bin. Say that rather than promise a bang.
- **You name the model yourself**, in the call's `model` field -- pick it
  from the cost table in your rulebook, and the result discloses which one
  ran. Cost follows the model you pick, not the words you use: the
  cheapest video model animates forward from a starting image only, so a
  text-only effect ask needs a model that can start from text, at roughly
  double the cheapest price -- the rulebook's table has the numbers.

With ANY reference, **do not re-describe** it -- the frame already carries
the light, the format, the subject and the anchor as pixels. The same rule
governs a clip edit even harder: the footage carries the scene, so spend
the words on the CHANGE.

`generate_ai_video_edit` re-renders an existing clip. Four facts, and the
first two lose the user's work if you skip them:

- **The input WINDOW is enforced, and it refuses BY NAME at BOTH edges.**
  The model takes a bounded range of input -- a clip can be too LONG for
  it and also too SHORT. When the visible range falls outside, the call
  refuses and hands you the real bounds, the clip's measured length, and
  the remedy: `splitClip` or a tighter trim when it is too long, a longer
  clip or an extended trim when it is too short. RELAY that refusal with
  its numbers and offer the split. NEVER pick a sub-range yourself to
  squeeze under the limit -- a guess is paid for at full price, and a
  silently truncated range is not what the user asked for.
- **It comes back PICTURE ONLY, and it lands in the BIN.** Nothing on the
  timeline changes until you place it. Call `detachAudio` on the SOURCE
  FIRST, then place the edited picture above it. Be precise about what
  that step buys, because the obvious story is wrong: COVERING the source
  hides its picture and keeps its sound -- a covered clip is still an
  audio contributor, measured on a real exported file. The sound dies when
  the source clip is REMOVED, which is what "replace it with the edited
  version" and any later tidy-up of that track both do. Detached audio
  lives on its own clip and survives all of them, so detaching first makes
  the take safe whichever way the placement ends up; skip it and one
  ordinary cleanup step silently costs the whole take.
- **References are REFUSED on this path.** The tool offers a `references`
  slot and passing anything in it makes the call refuse with the reason,
  because the endpoint's reference field is unresolved and a guessed one
  can come back looking plausible while having conditioned on nothing at
  all -- which nobody downstream could detect. So do not build a workflow
  on it and never promise "match this photo"; if the user explicitly asks,
  relay the refusal instead of inventing an apology. The PROMPT is the
  whole conditioning surface here: describe the look in words.
- **It is billed by the LENGTH OF INPUT you send**, so a long range costs
  proportionally more than a short one, and this route costs several times
  a short text-to-video generation. Omit `model` and this capability's own
  default runs. Before anything is billed, the spend confirmation names
  the model and a real price for THIS clip's range -- or says plainly that
  the price is unknown, for a model outside the costed table. Let that
  confirmation quote the number; never invent one of your own.

### 1 -- grow the effect out of the footage

Set `referenceSource`: `"frame"` for the annotated preview frame (an
annotation must already exist, so ask them to draw rather than silently
falling back); `"clipEnd"` + `referenceClipId` for the LAST frame of a
timeline clip, trim-respecting, so it is the exact frame the viewer last
sees -- the default for "add an explosion at the end of clip 2"; `"media"`
+ `referenceMediaId` for a bin item's first frame; `"sketch"` for the
drawing itself as the literal opening frame.

Add `destinationSource` (`"clipStart"` + `destinationClipId`, or `"media"`)
and the shot bridges two real frames -- spend the words on the MOVE between
them. For a transition between two clips read `video-transitions` first.

### 2 -- alter the plate first, then animate it

The CHEAPER two-still route, for when the FIRST frame must already show
the change and the shot is going to be re-animated anyway -- a wardrobe
change or a world replacement on a shot you are inventing. It is NOT a
substitute for the direct route: this re-animates and invents new motion,
where `generate_ai_video_edit` keeps the take's own. If the user wants
their own footage kept, edit the clip instead.

`generate_ai_image` with `referenceSource` `"frame"` or `"media"` builds
ONE altered still from the plate -- same preservation block, same
`Only do this:` -- and it lands in the bin; then
`generate_ai_video` with `referenceSource: "media"` on that image animates
forward from it. Two paid calls, and the footage itself is untouched.

### 3 -- composite the element over the footage

Generate the element ISOLATED -- flat empty backdrop, no shadow, no light
spill, locked-off camera -- then lay it over the shot: `add_track` kind
"video" (video tracks insert ON TOP), `placeClip` at the beat,
`set_clip_properties` for `transform.scale`/`transform.position` plus
`opacity`, `set_keyframes` on `opacity` to fade rather than pop, and
`track_object` to pin it to a moving subject -- real on-device tracking
that writes POSITION keyframes, CENTER-tracked, truncated where the subject
is occluded. Offer it with that caveat.

**A generated clip is OPAQUE.** No alpha channel, and no blend modes -- no
screen/add, no way to key black or green out of it -- so a full-frame
overlay HIDES the shot underneath. Inset it, and/or drop `opacity` to
roughly 0.3-0.6 for a ghosted double exposure, or use workflow 1 so the
effect is IN the footage. Real transparency exists only where the ASSET
already carries alpha: `get_overlay_library` + `place_overlay`, and assets
baked by `export_overlay_asset`.

## What this cannot do -- say so plainly

- **content-aware removal / inpainting** -- nothing takes an object out of
  a clip's frames and fills in behind it. A prompt-level cleanup of an
  existing clip is `generate_ai_video_edit`; true object removal with
  fill-behind is still not a thing here -- do not promise it.
- **rotoscoping, masks, segmentation, chroma key** -- no per-object mask,
  no cut-out, no green-screen key. `crop` is four straight-edged insets.
- **editing in place** -- the edit always lands a NEW asset; the timeline
  clip is only changed when you place it. And nothing here does per-object
  masks even inside an edit: the edit is prompt-guided and WHOLE-FRAME, so
  "only his shirt" is a change you describe and hope for, never a
  selection you can make.

Name the limit and offer what is real in the same breath: a clip edit, one
of the three workflows above, or `track_object`. An honest refusal plus a
working alternative is a good turn; a promise the engine cannot keep is
not.
